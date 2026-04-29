//! Non-blocking HTTP health checks for backends
//!
//! Health checks run within the single-threaded mio event loop using non-blocking TCP
//! connections. Each check cycle is triggered by a timer. For each backend with a
//! configured health check, we open a non-blocking TCP connection, send a minimal
//! HTTP/1.1 GET request, parse the status line from the response, and update the
//! backend's health state accordingly.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    hash::{Hash, Hasher},
    io::{Read, Write},
    net::SocketAddr,
    rc::Rc,
    time::{Duration, Instant},
};

use mio::{Interest, Registry, Token, net::TcpStream};
use sozu_command::{
    proto::command::{Event, EventKind, HealthCheckConfig},
    state::ClusterId,
};

use crate::{backends::BackendMap, server::push_event};

/// Canonical log envelope tag for the health-checker. Mirrors the
/// hyphenated, all-caps `MUX-H2`/`PROXY-RELAY`/`TLS-RESOLVER` convention.
/// The `log_context!()` macro is the single contact point — every
/// `info!`/`warn!`/`error!`/`debug!`/`trace!` in this module prefixes its
/// format string with `"{} ", log_context!()` so the regression guard at
/// `lib/tests/log_layout.rs` recognises the tag.
macro_rules! log_context {
    () => {
        "HEALTH-CHECK"
    };
    ($cluster:expr) => {
        concat!("HEALTH-CHECK cluster=", $cluster)
    };
}

/// Base of the dedicated mio token namespace used for health-check
/// sockets. Tokens are allocated in the range
/// `[HEALTH_CHECK_TOKEN_BASE, HEALTH_CHECK_TOKEN_BASE + HEALTH_CHECK_TOKEN_CAPACITY)`
/// so they never collide with slab-allocated session tokens (capped well
/// below `1 << 24` by `Server::sessions::max_connections`) nor with the
/// mux GOAWAY sentinel `Token(usize::MAX)`.
const HEALTH_CHECK_TOKEN_BASE: usize = 1 << 24;
/// Maximum number of concurrent in-flight health-check sockets. The
/// allocator wraps modulo this capacity and skips slots that are still
/// in flight (see `HealthChecker::allocate_token`); exceeding it is a
/// programmer error and emits an `error!` rather than silently colliding.
const HEALTH_CHECK_TOKEN_CAPACITY: usize = 1 << 16;

/// Each pending entry is `(cluster_id, config, h2c, backends_to_check)`.
/// `h2c` mirrors `cluster.http2` (the same backend-capability hint the
/// mux router uses) so the probe wire format matches what the proxy
/// will actually use to reach those backends — no per-cluster
/// `is_h2c` knob, no risk of probe / data-plane divergence.
type PendingChecks = Vec<(
    ClusterId,
    HealthCheckConfig,
    bool,
    Vec<(String, SocketAddr)>,
)>;

/// Tracks an in-flight health check connection
#[derive(Debug)]
struct InFlightCheck {
    stream: TcpStream,
    token: Token,
    cluster_id: ClusterId,
    backend_id: String,
    address: SocketAddr,
    started_at: Instant,
    timeout: Duration,
    request_bytes: Option<Vec<u8>>,
    write_offset: usize,
    response_buf: Vec<u8>,
    config: HealthCheckConfig,
    /// Captured at probe-creation time from `BackendMap.cluster_http2`.
    /// The response parser needs to know whether to walk H2 frames or
    /// parse an HTTP/1.1 status line; storing it on the in-flight
    /// record avoids racing the cluster's `http2` flag if the
    /// operator flips it mid-probe.
    h2c: bool,
}

/// Manages health checks across all clusters and backends
#[derive(Debug)]
pub struct HealthChecker {
    in_flight: Vec<InFlightCheck>,
    last_check_time: HashMap<ClusterId, Instant>,
    next_token_id: usize,
    ready_tokens: HashSet<Token>,
}

impl Default for HealthChecker {
    fn default() -> Self {
        Self::new()
    }
}

impl HealthChecker {
    pub fn new() -> Self {
        HealthChecker {
            in_flight: Vec::new(),
            last_check_time: HashMap::new(),
            next_token_id: 0,
            ready_tokens: HashSet::new(),
        }
    }

    /// Pick the next free slot offset modulo `HEALTH_CHECK_TOKEN_CAPACITY`,
    /// skipping offsets that match an in-flight check. Returns `None` when
    /// the entire capacity is occupied — caller must surface the error
    /// rather than silently colliding with another in-flight stream's
    /// readiness events.
    fn allocate_token(&mut self) -> Option<Token> {
        let in_flight: HashSet<usize> = self
            .in_flight
            .iter()
            .map(|c| c.token.0 - HEALTH_CHECK_TOKEN_BASE)
            .collect();

        for _ in 0..HEALTH_CHECK_TOKEN_CAPACITY {
            let offset = self.next_token_id % HEALTH_CHECK_TOKEN_CAPACITY;
            self.next_token_id = self.next_token_id.wrapping_add(1);
            if !in_flight.contains(&offset) {
                return Some(Token(HEALTH_CHECK_TOKEN_BASE + offset));
            }
        }
        error!(
            "{} token-table full ({} in-flight checks); refusing to allocate a new probe slot",
            log_context!(),
            in_flight.len()
        );
        None
    }

    /// Returns true iff `token` falls in the bounded range reserved for
    /// health-check sockets. Critically, the upper bound prevents this
    /// helper from falsely claiming the mux GOAWAY sentinel
    /// `Token(usize::MAX)` (CVE-class regression caught during the
    /// post-1209 rebase cross-check).
    pub fn owns_token(&self, token: Token) -> bool {
        token.0 >= HEALTH_CHECK_TOKEN_BASE
            && token.0 < HEALTH_CHECK_TOKEN_BASE + HEALTH_CHECK_TOKEN_CAPACITY
    }

    /// Called by the server event loop when mio reports readiness for a health check socket.
    pub fn ready(&mut self, token: Token) {
        self.ready_tokens.insert(token);
    }

    /// Called on each event loop iteration. Initiates new health checks when intervals
    /// have elapsed, and progresses in-flight checks.
    pub fn poll(&mut self, backends: &Rc<RefCell<BackendMap>>, registry: &Registry) {
        if self.in_flight.is_empty() && backends.borrow().health_check_configs.is_empty() {
            return;
        }
        self.initiate_checks(backends, registry);
        self.progress_checks(backends, registry);
    }

    fn initiate_checks(&mut self, backends: &Rc<RefCell<BackendMap>>, registry: &Registry) {
        let backend_map = backends.borrow();
        let now = Instant::now();

        let mut to_check: PendingChecks = Vec::new();

        for (cluster_id, config) in &backend_map.health_check_configs {
            let interval = Duration::from_secs(u64::from(config.interval));

            // Add jitter based on cluster_id hash to prevent synchronized health check storms
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            cluster_id.hash(&mut hasher);
            let jitter_ms = hasher.finish() % (interval.as_millis() as u64 / 5).max(1);
            let jittered_interval = interval + Duration::from_millis(jitter_ms);

            let should_check = match self.last_check_time.get(cluster_id) {
                Some(last) => now.duration_since(*last) >= jittered_interval,
                None => true,
            };

            if !should_check {
                continue;
            }

            if let Some(backend_list) = backend_map.backends.get(cluster_id) {
                let backends_to_check: Vec<(String, SocketAddr)> = backend_list
                    .backends
                    .iter()
                    .filter(|b| {
                        let b = b.borrow();
                        b.status == crate::backends::BackendStatus::Normal
                            && !self.in_flight.iter().any(|f| {
                                f.cluster_id == *cluster_id && f.backend_id == b.backend_id
                            })
                    })
                    .map(|b| {
                        let b = b.borrow();
                        (b.backend_id.to_owned(), b.address)
                    })
                    .collect();

                if !backends_to_check.is_empty() {
                    let h2c = backend_map
                        .cluster_http2
                        .get(cluster_id)
                        .copied()
                        .unwrap_or(false);
                    to_check.push((
                        cluster_id.to_owned(),
                        config.to_owned(),
                        h2c,
                        backends_to_check,
                    ));
                }
            }
        }

        drop(backend_map);

        for (cluster_id, config, h2c, backends_to_check) in to_check {
            self.last_check_time.insert(cluster_id.to_owned(), now);

            // The URI was validated at the worker `SetHealthCheck`
            // boundary by `sozu_command::config::validate_health_check_config`
            // (CR/LF/NUL/C0 rejected, leading `/` enforced). The probe
            // runtime trusts that contract — no second silent strip
            // here, no defense-in-depth divergence between what the
            // operator typed and what hits the wire.
            let probe_uri = config.uri.as_str();

            for (backend_id, address) in backends_to_check {
                match TcpStream::connect(address) {
                    Ok(mut stream) => {
                        let Some(token) = self.allocate_token() else {
                            // Token table exhausted — record the probe as failed
                            // so threshold logic can still fire, then move on.
                            // The allocator already logged at error level.
                            Self::record_check_result(
                                backends,
                                &cluster_id,
                                &backend_id,
                                address,
                                false,
                                &config,
                            );
                            continue;
                        };
                        if let Err(e) = registry.register(
                            &mut stream,
                            token,
                            Interest::READABLE | Interest::WRITABLE,
                        ) {
                            debug!(
                                "{} failed to register socket for {} ({}) in cluster {}: {}",
                                log_context!(),
                                backend_id,
                                address,
                                cluster_id,
                                e
                            );
                            Self::record_check_result(
                                backends,
                                &cluster_id,
                                &backend_id,
                                address,
                                false,
                                &config,
                            );
                            continue;
                        }
                        trace!(
                            "{} initiated connection to {} ({}) for cluster {}",
                            log_context!(),
                            backend_id,
                            address,
                            cluster_id
                        );
                        let request_bytes = if h2c {
                            build_h2c_probe_bytes(probe_uri, address)
                        } else {
                            // RFC 9110 §7.2: `Host` MUST carry the
                            // authority component, including the port
                            // when it differs from the URI scheme's
                            // default. SocketAddr's Display impl emits
                            // `ip:port` (with brackets for IPv6), which
                            // is unambiguous against any non-default
                            // backend port. Backends that demand a
                            // specific virtual-host name should expose
                            // a non-vhost health endpoint (the same
                            // pattern nginx/apache document) — adding
                            // a per-cluster `host` field on
                            // `HealthCheckConfig` is tracked as a
                            // follow-up.
                            format!(
                                "GET {probe_uri} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
                            )
                            .into_bytes()
                        };
                        self.in_flight.push(InFlightCheck {
                            stream,
                            token,
                            cluster_id: cluster_id.to_owned(),
                            backend_id,
                            address,
                            started_at: now,
                            timeout: Duration::from_secs(u64::from(config.timeout)),
                            request_bytes: Some(request_bytes),
                            write_offset: 0,
                            response_buf: Vec::with_capacity(256),
                            config: config.to_owned(),
                            h2c,
                        });
                    }
                    Err(e) => {
                        debug!(
                            "{} failed to connect to {} ({}) for cluster {}: {}",
                            log_context!(),
                            backend_id,
                            address,
                            cluster_id,
                            e
                        );
                        Self::record_check_result(
                            backends,
                            &cluster_id,
                            &backend_id,
                            address,
                            false,
                            &config,
                        );
                    }
                }
            }
        }
    }

    fn progress_checks(&mut self, backends: &Rc<RefCell<BackendMap>>, registry: &Registry) {
        const MAX_HEALTH_RESPONSE_SIZE: usize = 4096;

        let now = Instant::now();
        let mut completed = Vec::new();
        let ready = std::mem::take(&mut self.ready_tokens);

        for (idx, check) in self.in_flight.iter_mut().enumerate() {
            // Always check timeouts regardless of readiness
            if now.duration_since(check.started_at) > check.timeout {
                debug!(
                    "{} timeout for {} ({}) in cluster {}",
                    log_context!(),
                    check.backend_id,
                    check.address,
                    check.cluster_id
                );
                completed.push((idx, false));
                continue;
            }

            // Skip I/O if the socket has not been reported ready by mio
            if !ready.contains(&check.token) {
                continue;
            }

            if let Some(ref request_bytes) = check.request_bytes {
                match check.stream.write(&request_bytes[check.write_offset..]) {
                    Ok(n) => {
                        check.write_offset += n;
                        if check.write_offset >= request_bytes.len() {
                            check.request_bytes = None;
                        } else {
                            continue;
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        continue;
                    }
                    Err(_e) => {
                        completed.push((idx, false));
                        continue;
                    }
                }
            }

            let mut buf = [0u8; 256];
            match check.stream.read(&mut buf) {
                Ok(0) => {
                    let success =
                        parse_probe_response(&check.response_buf, &check.config, check.h2c)
                            .unwrap_or(false);
                    completed.push((idx, success));
                }
                Ok(n) => {
                    if check.response_buf.len() + n > MAX_HEALTH_RESPONSE_SIZE {
                        completed.push((idx, false));
                        continue;
                    }
                    check.response_buf.extend_from_slice(&buf[..n]);
                    if let Some(success) =
                        parse_probe_response(&check.response_buf, &check.config, check.h2c)
                    {
                        completed.push((idx, success));
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_e) => {
                    completed.push((idx, false));
                }
            }
        }

        // Sort indices in descending order so swap_remove doesn't invalidate
        // later indices — it moves the last element to the removed position,
        // which has already been processed or is beyond our range.
        completed.sort_by(|a, b| b.0.cmp(&a.0));
        for (idx, success) in completed {
            let mut check = self.in_flight.swap_remove(idx);
            let _ = registry.deregister(&mut check.stream);
            Self::record_check_result(
                backends,
                &check.cluster_id,
                &check.backend_id,
                check.address,
                success,
                &check.config,
            );
        }
    }

    fn record_check_result(
        backends: &Rc<RefCell<BackendMap>>,
        cluster_id: &str,
        backend_id: &str,
        address: SocketAddr,
        success: bool,
        config: &HealthCheckConfig,
    ) {
        let mut backend_map = backends.borrow_mut();
        let Some(backend_list) = backend_map.backends.get_mut(cluster_id) else {
            return;
        };

        let Some(backend_ref) = backend_list.find_backend(&address) else {
            return;
        };

        let mut backend = backend_ref.borrow_mut();

        if success {
            let transitioned = backend.health.record_success(config.healthy_threshold);
            if transitioned {
                info!(
                    "{} backend {} at {} marked UP (health check passed {} consecutive times) for cluster {}",
                    log_context!(),
                    backend_id,
                    address,
                    config.healthy_threshold,
                    cluster_id
                );
                incr!("health_check.up");
                push_event(Event {
                    kind: EventKind::HealthCheckHealthy as i32,
                    cluster_id: Some(cluster_id.to_owned()),
                    backend_id: Some(backend_id.to_owned()),
                    address: Some(address.into()),
                });
            }
            count!("health_check.success", 1);
        } else {
            let transitioned = backend.health.record_failure(config.unhealthy_threshold);
            if transitioned {
                warn!(
                    "{} backend {} at {} marked DOWN (health check failed {} consecutive times) for cluster {}",
                    log_context!(),
                    backend_id,
                    address,
                    config.unhealthy_threshold,
                    cluster_id
                );
                incr!("health_check.down");
                push_event(Event {
                    kind: EventKind::HealthCheckUnhealthy as i32,
                    cluster_id: Some(cluster_id.to_owned()),
                    backend_id: Some(backend_id.to_owned()),
                    address: Some(address.into()),
                });
            }
            count!("health_check.failure", 1);
        }

        // Emit a warning when the healthy backend count is critically low
        drop(backend);
        let total = backend_list.backends.len();
        let healthy = backend_list
            .backends
            .iter()
            .filter(|b| b.borrow().health.is_healthy())
            .count();
        if total > 0 && healthy > 0 && healthy * 2 <= total {
            warn!(
                "{} cluster {} has only {}/{} healthy backends",
                log_context!(),
                cluster_id,
                healthy,
                total
            );
            gauge!("health_check.healthy_backends", healthy);
        }
    }

    pub fn remove_cluster(&mut self, cluster_id: &str) {
        self.last_check_time.remove(cluster_id);
        self.in_flight
            .retain(|check| check.cluster_id != cluster_id);
    }
}

/// Top-level dispatch: pick the HTTP/1.1 status-line parser or the
/// HTTP/2 frame walker depending on whether the probe was sent as h2c.
/// `h2c` is captured from `BackendMap.cluster_http2` on the
/// `InFlightCheck` at probe-creation time.
fn parse_probe_response(buf: &[u8], config: &HealthCheckConfig, h2c: bool) -> Option<bool> {
    if h2c {
        try_parse_h2c_status(buf, config)
    } else {
        try_parse_status_line(buf, config)
    }
}

fn try_parse_status_line(buf: &[u8], config: &HealthCheckConfig) -> Option<bool> {
    let response = std::str::from_utf8(buf).ok()?;
    let first_line_end = response.find("\r\n")?;
    let status_line = &response[..first_line_end];

    let (_, rest) = status_line.split_once(' ')?;
    let status_str = rest.split(' ').next()?;
    let status_code: u32 = status_str.parse().unwrap_or(0);
    Some(is_status_healthy(status_code, config.expected_status))
}

fn is_status_healthy(actual: u32, expected: u32) -> bool {
    if expected == 0 {
        (200..300).contains(&actual)
    } else {
        actual == expected
    }
}

/// HTTP/2 frame type constants — RFC 9113 §6.
const H2_FRAME_TYPE_HEADERS: u8 = 0x01;
const H2_FRAME_TYPE_SETTINGS: u8 = 0x04;
const H2_FRAME_TYPE_GOAWAY: u8 = 0x07;

/// Flags used in the parser. END_STREAM = 0x01, END_HEADERS = 0x04,
/// PADDED = 0x08, PRIORITY = 0x20 (HEADERS only).
const H2_FLAG_PADDED: u8 = 0x08;
const H2_FLAG_PRIORITY: u8 = 0x20;
const H2_FRAME_HEADER_LEN: usize = 9;

/// Compose a bare-minimum h2c (HTTP/2 over cleartext, prior-knowledge)
/// probe: the 24-byte connection preface, an empty client SETTINGS
/// frame (acknowledging the spec-mandated handshake), and a single
/// HEADERS frame on stream 1 carrying `GET <path>` with
/// END_STREAM | END_HEADERS.
///
/// HPACK encoding is hand-rolled (no dynamic table use, no Huffman):
///
/// * `:method GET` — static table index 2 → indexed (`0x82`).
/// * `:scheme http` — static index 6 → indexed (`0x86`).
/// * `:path <uri>` — literal-without-indexing, name index 4 →
///   byte `0x04`, then 7-bit length + ASCII bytes.
/// * `:authority host` — literal-without-indexing, name index 1 →
///   byte `0x01`, then 7-bit length + ASCII bytes.
///
/// Length octets use the HPACK 7-bit-prefix integer form, which fits
/// in one byte for any sane health-check URI or `host:port` (≤127
/// bytes). For longer values the encoder falls back to RFC 7541 §5.1
/// multi-byte form.
fn build_h2c_probe_bytes(uri: &str, address: SocketAddr) -> Vec<u8> {
    const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

    let authority = address.to_string();

    // Build the HPACK header block first so we know its length for the
    // HEADERS frame header.
    let mut hpack = Vec::with_capacity(2 + 2 + uri.len() + authority.len() + 4);
    hpack.push(0x82); // :method GET
    hpack.push(0x86); // :scheme http
    hpack.push(0x04); // :path  — literal w/o indexing, name idx 4
    encode_hpack_length(uri.len(), &mut hpack);
    hpack.extend_from_slice(uri.as_bytes());
    hpack.push(0x01); // :authority — literal w/o indexing, name idx 1
    encode_hpack_length(authority.len(), &mut hpack);
    hpack.extend_from_slice(authority.as_bytes());

    let mut out = Vec::with_capacity(PREFACE.len() + 9 + 9 + hpack.len());
    out.extend_from_slice(PREFACE);

    // Empty client SETTINGS frame: length=0, type=0x04, flags=0, sid=0.
    out.extend_from_slice(&[0, 0, 0, H2_FRAME_TYPE_SETTINGS, 0, 0, 0, 0, 0]);

    // HEADERS frame: length=hpack.len(), type=0x01,
    // flags=END_STREAM|END_HEADERS=0x05, stream=1.
    let len = hpack.len() as u32;
    out.push(((len >> 16) & 0xFF) as u8);
    out.push(((len >> 8) & 0xFF) as u8);
    out.push((len & 0xFF) as u8);
    out.push(H2_FRAME_TYPE_HEADERS);
    out.push(0x05); // END_STREAM | END_HEADERS
    out.extend_from_slice(&[0, 0, 0, 1]); // stream id = 1
    out.extend_from_slice(&hpack);
    out
}

/// HPACK 7-bit-prefix integer (RFC 7541 §5.1) for length octets.
/// Single-byte form when value < 127; otherwise emits the continuation
/// chain.
fn encode_hpack_length(value: usize, out: &mut Vec<u8>) {
    if value < 127 {
        out.push(value as u8);
        return;
    }
    out.push(0x7F);
    let mut v = value - 127;
    while v >= 128 {
        out.push(((v & 0x7F) as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Walk the buffered H2 frames looking for a HEADERS frame on stream 1
/// and extract `:status`. Returns:
///
/// * `Some(true)` — `:status` found and matches
///   `config.expected_status` (or any 2xx when `expected_status == 0`).
/// * `Some(false)` — `:status` found but does not match, or a GOAWAY
///   frame was received.
/// * `None` — buffer truncated mid-frame; caller should keep reading.
///
/// The decoder handles only the subset of HPACK used by typical
/// servers replying to a probe: the indexed-status static-table forms
/// (200/204/206/304/400/404/500), and the literal-with-indexed-name
/// form for `:status` (name index 8). Frames with PADDED/PRIORITY flags
/// are skipped past correctly. Unknown HPACK encodings are treated as
/// "header block continues" and the decoder advances byte-by-byte
/// until it finds a recognisable representation or end-of-block.
fn try_parse_h2c_status(buf: &[u8], config: &HealthCheckConfig) -> Option<bool> {
    let mut cursor = 0usize;

    while cursor + H2_FRAME_HEADER_LEN <= buf.len() {
        let length = ((buf[cursor] as usize) << 16)
            | ((buf[cursor + 1] as usize) << 8)
            | (buf[cursor + 2] as usize);
        let frame_type = buf[cursor + 3];
        let flags = buf[cursor + 4];
        let stream_id = (((buf[cursor + 5] as u32) & 0x7F) << 24)
            | ((buf[cursor + 6] as u32) << 16)
            | ((buf[cursor + 7] as u32) << 8)
            | (buf[cursor + 8] as u32);

        let frame_end = cursor + H2_FRAME_HEADER_LEN + length;
        if frame_end > buf.len() {
            return None;
        }

        match frame_type {
            H2_FRAME_TYPE_HEADERS if stream_id == 1 => {
                let mut payload_start = cursor + H2_FRAME_HEADER_LEN;
                let mut payload_end = frame_end;

                if flags & H2_FLAG_PADDED != 0 {
                    if payload_start >= payload_end {
                        return Some(false);
                    }
                    let pad = buf[payload_start] as usize;
                    payload_start += 1;
                    if payload_end < pad || payload_end - pad < payload_start {
                        return Some(false);
                    }
                    payload_end -= pad;
                }
                if flags & H2_FLAG_PRIORITY != 0 {
                    if payload_end < payload_start + 5 {
                        return Some(false);
                    }
                    payload_start += 5;
                }

                let block = &buf[payload_start..payload_end];
                if let Some(status) = decode_h2c_status(block) {
                    return Some(is_status_healthy(status, config.expected_status));
                }
                return Some(false);
            }
            H2_FRAME_TYPE_GOAWAY => return Some(false),
            // SETTINGS, SETTINGS-ACK, DATA, etc. are ignored — keep
            // walking until we find HEADERS on stream 1.
            _ => {}
        }
        cursor = frame_end;
    }
    None
}

/// Recover the numeric `:status` from an HPACK header block. Handles:
///
/// * Static-table indexed forms 0x88..=0x8E (`:status` 200/204/206/
///   304/400/404/500).
/// * Literal-with-incremental-indexing using static-table name index
///   8 — prefix `01000000` + 6-bit index 8 = `0x48` per RFC 7541 §6.2.1.
/// * Literal-without-indexing and literal-never-indexed using static
///   name index 8 — `0x08` and `0x18` respectively (4-bit prefix).
///
/// Other HPACK forms in the block (e.g. dynamic-table-size updates,
/// indexed headers for other pseudo-headers) advance the cursor by
/// one byte and the loop keeps walking. That's lossy (wrong jump on
/// multi-byte literals) but bounded — at worst the decoder returns
/// `None` and the probe is recorded as failed, which only matters
/// after `unhealthy_threshold` consecutive misses.
fn decode_h2c_status(block: &[u8]) -> Option<u32> {
    let mut i = 0usize;
    while i < block.len() {
        let b = block[i];
        match b {
            0x88 => return Some(200),
            0x89 => return Some(204),
            0x8A => return Some(206),
            0x8B => return Some(304),
            0x8C => return Some(400),
            0x8D => return Some(404),
            0x8E => return Some(500),
            // Literal with incremental indexing, name idx 8
            // (`:status`). 6-bit-prefix integer = 8 fits in the
            // single-byte form, so the byte is exactly 0x48.
            0x48 |
            // Literal w/o indexing OR literal never indexed, name in
            // static table at index 8. 4-bit-prefix integer = 8 fits
            // in single-byte form (max 14 inline).
            0x08 | 0x18 => {
                let (value_len, len_consumed) = decode_hpack_length(&block[i + 1..])?;
                let val_start = i + 1 + len_consumed;
                let val_end = val_start.checked_add(value_len)?;
                if val_end > block.len() {
                    return None;
                }
                let s = std::str::from_utf8(&block[val_start..val_end]).ok()?;
                return s.parse::<u32>().ok();
            }
            _ => {
                i += 1;
                continue;
            }
        }
    }
    None
}

/// HPACK 7-bit-prefix integer decode. Returns `(value, bytes_read)`
/// or `None` on buffer underrun / overflow. Mirror of
/// `encode_hpack_length` above.
fn decode_hpack_length(buf: &[u8]) -> Option<(usize, usize)> {
    if buf.is_empty() {
        return None;
    }
    let first = (buf[0] & 0x7F) as usize;
    if first < 127 {
        return Some((first, 1));
    }
    let mut value = 127usize;
    let mut shift = 0u32;
    let mut consumed = 1usize;
    loop {
        if consumed >= buf.len() {
            return None;
        }
        let b = buf[consumed];
        value = value.checked_add(((b & 0x7F) as usize).checked_shl(shift)?)?;
        consumed += 1;
        if b & 0x80 == 0 {
            return Some((value, consumed));
        }
        shift = shift.checked_add(7)?;
        if shift > usize::BITS {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::HealthState;

    #[test]
    fn test_is_status_healthy_any_2xx() {
        assert!(is_status_healthy(200, 0));
        assert!(is_status_healthy(204, 0));
        assert!(is_status_healthy(299, 0));
        assert!(!is_status_healthy(301, 0));
        assert!(!is_status_healthy(500, 0));
        assert!(!is_status_healthy(0, 0));
    }

    #[test]
    fn test_is_status_healthy_specific() {
        assert!(is_status_healthy(200, 200));
        assert!(!is_status_healthy(204, 200));
        assert!(!is_status_healthy(500, 200));
    }

    #[test]
    fn test_try_parse_status_line() {
        let config = HealthCheckConfig {
            uri: "/health".to_owned(),
            interval: 10,
            timeout: 5,
            healthy_threshold: 3,
            unhealthy_threshold: 3,
            expected_status: 0,
        };

        let buf = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(try_parse_status_line(buf, &config), Some(true));

        let buf = b"HTTP/1.1 500 Internal Server Error\r\n\r\n";
        assert_eq!(try_parse_status_line(buf, &config), Some(false));

        let buf = b"HTTP/1.1 200";
        assert_eq!(try_parse_status_line(buf, &config), None);
    }

    #[test]
    fn test_health_state_transitions() {
        let mut state = HealthState::default();
        assert!(state.is_healthy());

        assert!(!state.record_failure(3));
        assert!(!state.record_failure(3));
        assert!(state.is_healthy());

        assert!(state.record_failure(3));
        assert!(!state.is_healthy());

        assert!(!state.record_success(3));
        assert!(!state.record_success(3));
        assert!(!state.is_healthy());

        assert!(state.record_success(3));
        assert!(state.is_healthy());
    }

    fn h2c_config(expected: u32) -> HealthCheckConfig {
        HealthCheckConfig {
            uri: "/health".to_owned(),
            interval: 10,
            timeout: 5,
            healthy_threshold: 3,
            unhealthy_threshold: 3,
            expected_status: expected,
        }
    }

    fn h2_frame_header(payload_len: usize, frame_type: u8, flags: u8, sid: u32) -> Vec<u8> {
        let mut h = Vec::with_capacity(9);
        h.push(((payload_len >> 16) & 0xFF) as u8);
        h.push(((payload_len >> 8) & 0xFF) as u8);
        h.push((payload_len & 0xFF) as u8);
        h.push(frame_type);
        h.push(flags);
        h.extend_from_slice(&sid.to_be_bytes());
        h
    }

    #[test]
    fn build_h2c_probe_starts_with_preface_and_settings() {
        let bytes = build_h2c_probe_bytes("/health", "127.0.0.1:8080".parse().unwrap());
        // Preface
        assert!(bytes.starts_with(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"));
        // SETTINGS frame: 9 bytes after preface, type=0x04, flags=0, sid=0
        let settings_start = 24;
        assert_eq!(bytes[settings_start + 3], H2_FRAME_TYPE_SETTINGS);
        assert_eq!(bytes[settings_start + 4], 0);
        assert_eq!(
            &bytes[settings_start + 5..settings_start + 9],
            &[0u8, 0, 0, 0]
        );
        // HEADERS frame on stream 1, END_STREAM | END_HEADERS = 0x05
        let headers_start = settings_start + 9;
        assert_eq!(bytes[headers_start + 3], H2_FRAME_TYPE_HEADERS);
        assert_eq!(bytes[headers_start + 4], 0x05);
        assert_eq!(
            &bytes[headers_start + 5..headers_start + 9],
            &[0u8, 0, 0, 1]
        );
        // First HPACK byte must be :method GET indexed (0x82)
        assert_eq!(bytes[headers_start + 9], 0x82);
    }

    #[test]
    fn h2c_indexed_status_200_is_healthy_for_any_2xx() {
        // SETTINGS ack (server's empty SETTINGS) + HEADERS on stream 1 with `0x88` (=200).
        let mut buf = Vec::new();
        buf.extend(h2_frame_header(0, H2_FRAME_TYPE_SETTINGS, 0, 0));
        buf.extend(h2_frame_header(1, H2_FRAME_TYPE_HEADERS, 0x05, 1));
        buf.push(0x88);
        let cfg = h2c_config(0);
        assert_eq!(try_parse_h2c_status(&buf, &cfg), Some(true));
    }

    #[test]
    fn h2c_indexed_status_500_fails_default_2xx_check() {
        let mut buf = Vec::new();
        buf.extend(h2_frame_header(1, H2_FRAME_TYPE_HEADERS, 0x05, 1));
        buf.push(0x8E); // :status 500
        let cfg = h2c_config(0);
        assert_eq!(try_parse_h2c_status(&buf, &cfg), Some(false));
    }

    #[test]
    fn h2c_literal_status_503_matches_expected_503() {
        // Literal w/o indexing, name idx 8, value "503"
        let mut block = Vec::new();
        block.extend_from_slice(&[0x08, 0x03, b'5', b'0', b'3']);
        let mut buf = Vec::new();
        buf.extend(h2_frame_header(block.len(), H2_FRAME_TYPE_HEADERS, 0x05, 1));
        buf.extend_from_slice(&block);
        let cfg = h2c_config(503);
        assert_eq!(try_parse_h2c_status(&buf, &cfg), Some(true));
    }

    #[test]
    fn h2c_goaway_marks_unhealthy() {
        let mut buf = Vec::new();
        buf.extend(h2_frame_header(8, H2_FRAME_TYPE_GOAWAY, 0, 0));
        buf.extend_from_slice(&[0u8; 8]); // last-stream-id + error code
        let cfg = h2c_config(0);
        assert_eq!(try_parse_h2c_status(&buf, &cfg), Some(false));
    }

    #[test]
    fn h2c_truncated_buffer_returns_none() {
        let mut buf = Vec::new();
        // Frame header claims 10 bytes payload but we only deliver 5.
        buf.extend(h2_frame_header(10, H2_FRAME_TYPE_HEADERS, 0x05, 1));
        buf.extend_from_slice(&[0u8; 5]);
        let cfg = h2c_config(0);
        assert_eq!(try_parse_h2c_status(&buf, &cfg), None);
    }

    #[test]
    fn h2c_literal_with_incremental_indexing_status_201_matches() {
        // 0x48 = literal w/ incremental indexing, name idx 8 (:status)
        let mut block = Vec::new();
        block.extend_from_slice(&[0x48, 0x03, b'2', b'0', b'1']);
        let mut buf = Vec::new();
        buf.extend(h2_frame_header(block.len(), H2_FRAME_TYPE_HEADERS, 0x05, 1));
        buf.extend_from_slice(&block);
        let cfg = h2c_config(0); // any 2xx
        assert_eq!(try_parse_h2c_status(&buf, &cfg), Some(true));
    }

    #[test]
    fn h2c_padded_headers_strips_pad_length_octet() {
        // PADDED flag (0x08) + END_HEADERS (0x04) = 0x0C. Payload =
        // 1-byte pad-length + indexed :status (0x88) + 2 padding bytes.
        let mut buf = Vec::new();
        buf.extend(h2_frame_header(4, H2_FRAME_TYPE_HEADERS, 0x0C, 1));
        buf.extend_from_slice(&[2, 0x88, 0, 0]);
        let cfg = h2c_config(0);
        assert_eq!(try_parse_h2c_status(&buf, &cfg), Some(true));
    }
}
