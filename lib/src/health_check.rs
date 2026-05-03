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

use crate::{
    backends::BackendMap,
    protocol::mux::{
        parser::{
            FLAG_END_HEADERS, FLAG_PADDED, FLAG_PRIORITY, FRAME_HEADER_SIZE, FrameType,
            frame_header,
        },
        serializer::H2_PRI,
    },
    server::push_event,
};

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
/// will actually use to reach those backends.
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
                gauge!("backend.available", 1, Some(cluster_id), Some(backend_id));
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
                gauge!("backend.available", 0, Some(cluster_id), Some(backend_id));
                push_event(Event {
                    kind: EventKind::HealthCheckUnhealthy as i32,
                    cluster_id: Some(cluster_id.to_owned()),
                    backend_id: Some(backend_id.to_owned()),
                    address: Some(address.into()),
                });
            }
            count!("health_check.failure", 1);
        }

        // Emit the healthy-backend gauge on every result update for clusters
        // with at least one configured backend, including `healthy == 0`. The
        // gauge is the only documented signal that lets dashboards detect
        // universal-outage / fail-open. Previously it was gated on
        // `healthy > 0 && healthy * 2 <= total`, so when all backends went
        // unhealthy the gauge retained its last non-zero value and operators
        // could not see fail-open in dashboards.
        //
        // The "critically low" warning (≤50% healthy) keeps its original
        // condition — only the gauge emission is unconditional.
        drop(backend);
        let total = backend_list.backends.len();
        let healthy = backend_list
            .backends
            .iter()
            .filter(|b| b.borrow().health.is_healthy())
            .count();
        if total > 0 {
            gauge!(
                "health_check.healthy_backends",
                healthy,
                Some(cluster_id),
                None
            );
            if healthy > 0 && healthy * 2 <= total {
                warn!(
                    "{} cluster {} has only {}/{} healthy backends",
                    log_context!(),
                    cluster_id,
                    healthy,
                    total
                );
            }
        }
        // Re-evaluate per-cluster availability so an `Available -> AllDown`
        // or `AllDown -> Available` transition driven purely by health-check
        // results (no failed routing call to observe it) still emits the
        // log + counter + Event. This is the only path that catches
        // passive-only recoveries — a cluster whose backends came back via
        // successful HC probes but whose retry policies haven't been
        // touched by any session.
        // `backend_list` is a `&BackendList` reborrowed from
        // `backend_map.backends.get(cluster_id)` above. The `&backend_map`
        // borrow it depends on lasts until the next use of `backend_map`,
        // so we just stop using `backend_list` and call the helper, which
        // re-takes its own `&self` borrow internally.
        backend_map.record_cluster_availability(cluster_id);
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

/// Compose a bare-minimum h2c (HTTP/2 over cleartext, prior-knowledge)
/// probe: the 24-byte connection preface, an empty client SETTINGS
/// frame (acknowledging the spec-mandated handshake), and a single
/// HEADERS frame on stream 1 carrying `GET <path>` with
/// END_STREAM | END_HEADERS.
///
/// HPACK encoding is delegated to `loona_hpack::Encoder` — the same
/// encoder the H2 mux uses for live traffic (`lib/src/protocol/mux/converter.rs`).
/// The probe inherits whatever static/dynamic-table behaviour the
/// encoder picks, including any future Huffman support. The connection
/// preface comes from `serializer::H2_PRI` so the probe and the live
/// mux stay in lockstep.
fn build_h2c_probe_bytes(uri: &str, address: SocketAddr) -> Vec<u8> {
    let authority = address.to_string();

    // Build the HPACK header block first so we know its length for the
    // HEADERS frame header. A fresh encoder per probe keeps things
    // stateless — we never reuse the dynamic table across probes.
    let mut encoder = loona_hpack::Encoder::new();
    let mut hpack: Vec<u8> = Vec::new();
    let headers: [(&[u8], &[u8]); 4] = [
        (b":method", b"GET"),
        (b":scheme", b"http"),
        (b":path", uri.as_bytes()),
        (b":authority", authority.as_bytes()),
    ];
    // Encoder::encode_into writes to an io::Write. Vec<u8> implements
    // it infallibly, so the ? cannot fire in practice.
    if encoder.encode_into(headers, &mut hpack).is_err() {
        // Defensive: return an empty buffer so the probe records as
        // failed via the read path instead of panicking.
        return Vec::new();
    }

    // Pre-allocate: preface + SETTINGS (9) + HEADERS header (9) + block.
    let mut out = Vec::with_capacity(H2_PRI.len() + FRAME_HEADER_SIZE * 2 + hpack.len());
    out.extend_from_slice(H2_PRI.as_bytes());

    // Empty client SETTINGS frame: length=0, type=Settings(0x04), flags=0, sid=0.
    out.extend_from_slice(&[0, 0, 0, 0x04, 0, 0, 0, 0, 0]);

    // HEADERS frame: length=hpack.len(), type=Headers(0x01),
    // flags=END_STREAM|END_HEADERS=0x05, stream=1.
    let len = hpack.len() as u32;
    out.push(((len >> 16) & 0xFF) as u8);
    out.push(((len >> 8) & 0xFF) as u8);
    out.push((len & 0xFF) as u8);
    out.push(0x01); // HEADERS
    out.push(0x05); // END_STREAM | END_HEADERS
    out.extend_from_slice(&[0, 0, 0, 1]); // stream id = 1
    out.extend_from_slice(&hpack);
    out
}

/// Walk the buffered H2 frames looking for a HEADERS frame (plus any
/// CONTINUATION frames until END_HEADERS) on stream 1, then HPACK-decode
/// the assembled block via `loona_hpack::Decoder` and pull `:status`.
///
/// Returns:
///
/// * `Some(true)` — `:status` decoded and matches
///   `config.expected_status` (or any 2xx when `expected_status == 0`).
/// * `Some(false)` — `:status` decoded but does not match, the HPACK
///   block was malformed, or a GOAWAY frame arrived.
/// * `None` — buffer truncated mid-frame; caller should keep reading.
///
/// Frames with PADDED / PRIORITY flags are normalised correctly via the
/// `mux::parser` flag constants (`FLAG_PADDED` / `FLAG_PRIORITY`). Unknown
/// HPACK encodings, Huffman-coded values, and CONTINUATION fragmentation
/// all fall through to the decoder rather than the hand-rolled byte walk
/// the previous implementation used.
fn try_parse_h2c_status(buf: &[u8], config: &HealthCheckConfig) -> Option<bool> {
    // RFC 9113 §4.2: the absolute upper bound for SETTINGS_MAX_FRAME_SIZE
    // is 2^24 - 1. The probe never advertises a custom limit, so accept
    // anything within the spec ceiling.
    const MAX_FRAME_SIZE: u32 = (1 << 24) - 1;

    let mut remaining: &[u8] = buf;
    // Buffer accumulating HEADERS + CONTINUATION block fragments for
    // stream 1 until END_HEADERS lands. RFC 9113 §6.10: until the
    // continuation chain ends, no other frames may arrive on the
    // connection, but we still tolerate interleaved control frames
    // for robustness — the decoder only fires on END_HEADERS.
    let mut headers_block: Option<Vec<u8>> = None;

    while !remaining.is_empty() {
        // The `mux::parser::frame_header` is built from `nom::number::complete`
        // primitives which emit `Err::Error` (not `Err::Incomplete`) on short
        // input, so distinguish "header still arriving" from "header arrived
        // but is malformed" by an explicit length check before parsing.
        if remaining.len() < FRAME_HEADER_SIZE {
            return None;
        }
        let (rest, header) = match frame_header(remaining, MAX_FRAME_SIZE) {
            Ok(parsed) => parsed,
            // Header bytes are present but parser rejected them: malformed
            // framing (oversized payload, invalid stream-id parity, etc.).
            // → probe is unhealthy.
            Err(_) => return Some(false),
        };

        let payload_len = header.payload_len as usize;
        if rest.len() < payload_len {
            // Body of the frame has not arrived yet.
            return None;
        }
        let (payload, after) = rest.split_at(payload_len);

        match header.frame_type {
            FrameType::Headers if header.stream_id == 1 => {
                let block = strip_padded_priority(payload, header.flags)?;
                let mut accumulator = headers_block.take().unwrap_or_default();
                accumulator.extend_from_slice(block);
                if header.flags & FLAG_END_HEADERS != 0 {
                    return Some(decode_status_from_block(&accumulator, config));
                }
                headers_block = Some(accumulator);
            }
            FrameType::Continuation if header.stream_id == 1 => {
                // CONTINUATION carries no padding/priority flags; the
                // payload is a raw block fragment.
                let Some(mut accumulator) = headers_block.take() else {
                    // CONTINUATION without a preceding HEADERS is a
                    // protocol error per RFC 9113 §6.10.
                    return Some(false);
                };
                accumulator.extend_from_slice(payload);
                if header.flags & FLAG_END_HEADERS != 0 {
                    return Some(decode_status_from_block(&accumulator, config));
                }
                headers_block = Some(accumulator);
            }
            FrameType::GoAway => return Some(false),
            // SETTINGS, SETTINGS-ACK, DATA, PING, etc. — keep walking
            // until we find HEADERS on stream 1.
            _ => {}
        }

        remaining = after;
    }
    None
}

/// Trim the optional 1-byte pad-length prefix and the 5-byte priority
/// dependency (RFC 9113 §6.2). Returns `None` when the flags claim
/// padding/priority but the payload is too short to satisfy them — the
/// caller turns that into `Some(false)` (probe unhealthy).
fn strip_padded_priority(payload: &[u8], flags: u8) -> Option<&[u8]> {
    let mut start = 0usize;
    let mut end = payload.len();

    if flags & FLAG_PADDED != 0 {
        let &pad_len = payload.first()?;
        start = 1;
        let pad = pad_len as usize;
        // Trailing padding bytes must fit within the remaining payload
        // (after dropping the 1-byte pad-length prefix) — otherwise the
        // frame is malformed.
        let available = end.checked_sub(start)?;
        if pad > available {
            return None;
        }
        end -= pad;
    }
    if flags & FLAG_PRIORITY != 0 {
        let new_start = start.checked_add(5)?;
        if new_start > end {
            return None;
        }
        start = new_start;
    }
    payload.get(start..end)
}

/// Run `loona_hpack::Decoder` over the assembled HEADERS block and
/// return whether `:status` matches `config.expected_status`. Unknown
/// HPACK encodings, malformed integers, Huffman fallbacks, and
/// `:status` values that fail UTF-8 / numeric parsing all collapse to
/// `false` — the probe is recorded as unhealthy, never as a panic.
fn decode_status_from_block(block: &[u8], config: &HealthCheckConfig) -> bool {
    let mut decoder = loona_hpack::Decoder::new();
    let mut status: Option<u32> = None;
    let decode_result = decoder.decode_with_cb(block, |name, value| {
        if status.is_some() {
            return;
        }
        if name.as_ref() == b":status"
            && let Ok(s) = std::str::from_utf8(value.as_ref())
            && let Ok(parsed) = s.parse::<u32>()
        {
            status = Some(parsed);
        }
    });
    if decode_result.is_err() {
        return false;
    }
    match status {
        Some(code) => is_status_healthy(code, config.expected_status),
        None => false,
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

    /// Wrap an HPACK-encoded header block in an H2 frame header with the
    /// given type, flags and stream id. Frame body bytes live in
    /// `payload`.
    fn frame_with_header(frame_type: u8, flags: u8, sid: u32, payload: &[u8]) -> Vec<u8> {
        let payload_len = payload.len();
        let mut out = Vec::with_capacity(FRAME_HEADER_SIZE + payload_len);
        out.push(((payload_len >> 16) & 0xFF) as u8);
        out.push(((payload_len >> 8) & 0xFF) as u8);
        out.push((payload_len & 0xFF) as u8);
        out.push(frame_type);
        out.push(flags);
        out.extend_from_slice(&sid.to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    /// Encode `:status <code>` (plus optional extra response headers) into
    /// a fresh HPACK block via the same encoder the live mux uses. This
    /// keeps the tests aligned with whatever loona_hpack picks for static
    /// vs. literal vs. (future) Huffman encoding instead of pinning bytes.
    fn encode_response_headers(headers: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut encoder = loona_hpack::Encoder::new();
        let mut out = Vec::new();
        encoder
            .encode_into(headers.iter().copied(), &mut out)
            .unwrap();
        out
    }

    #[test]
    fn build_h2c_probe_starts_with_preface_and_frames() {
        let bytes = build_h2c_probe_bytes("/health", "127.0.0.1:8080".parse().unwrap());

        // Connection preface (24 bytes).
        assert!(bytes.starts_with(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"));

        // SETTINGS frame after the preface: empty payload, type=0x04, flags=0, sid=0.
        let settings_start = 24;
        assert_eq!(&bytes[settings_start..settings_start + 3], &[0u8, 0, 0]); // length = 0
        assert_eq!(bytes[settings_start + 3], 0x04); // SETTINGS
        assert_eq!(bytes[settings_start + 4], 0); // flags
        assert_eq!(
            &bytes[settings_start + 5..settings_start + 9],
            &[0u8, 0, 0, 0]
        );

        // HEADERS frame on stream 1, END_STREAM | END_HEADERS = 0x05.
        let headers_start = settings_start + 9;
        assert_eq!(bytes[headers_start + 3], 0x01); // HEADERS
        assert_eq!(bytes[headers_start + 4], 0x05);
        assert_eq!(
            &bytes[headers_start + 5..headers_start + 9],
            &[0u8, 0, 0, 1]
        );

        // The HEADERS payload must HPACK-decode back to the four pseudo-headers.
        let payload_start = headers_start + 9;
        let mut decoder = loona_hpack::Decoder::new();
        let mut method = None;
        let mut scheme = None;
        let mut path = None;
        let mut authority = None;
        decoder
            .decode_with_cb(&bytes[payload_start..], |name, value| match name.as_ref() {
                b":method" => method = Some(value.to_vec()),
                b":scheme" => scheme = Some(value.to_vec()),
                b":path" => path = Some(value.to_vec()),
                b":authority" => authority = Some(value.to_vec()),
                _ => {}
            })
            .expect("loona_hpack decodes a freshly-encoded probe");
        assert_eq!(method.as_deref(), Some(b"GET" as &[u8]));
        assert_eq!(scheme.as_deref(), Some(b"http" as &[u8]));
        assert_eq!(path.as_deref(), Some(b"/health" as &[u8]));
        assert_eq!(authority.as_deref(), Some(b"127.0.0.1:8080" as &[u8]));
    }

    #[test]
    fn h2c_response_with_status_200_decodes_healthy() {
        let block = encode_response_headers(&[(b":status", b"200")]);
        let buf = frame_with_header(0x01, FLAG_END_HEADERS, 1, &block);
        let cfg = h2c_config(0);
        assert_eq!(try_parse_h2c_status(&buf, &cfg), Some(true));
    }

    #[test]
    fn h2c_response_with_status_500_fails_default_2xx_check() {
        let block = encode_response_headers(&[(b":status", b"500")]);
        let buf = frame_with_header(0x01, FLAG_END_HEADERS, 1, &block);
        let cfg = h2c_config(0);
        assert_eq!(try_parse_h2c_status(&buf, &cfg), Some(false));
    }

    #[test]
    fn h2c_response_with_status_503_matches_expected_503() {
        let block =
            encode_response_headers(&[(b":status", b"503"), (b"content-type", b"text/plain")]);
        let buf = frame_with_header(0x01, FLAG_END_HEADERS, 1, &block);
        let cfg = h2c_config(503);
        assert_eq!(try_parse_h2c_status(&buf, &cfg), Some(true));
    }

    #[test]
    fn h2c_response_with_continuation_decodes_status_200_healthy() {
        // Build one HPACK block, then split it across HEADERS (no
        // END_HEADERS) + CONTINUATION (END_HEADERS). The hand-rolled
        // walker that this commit replaces would have refused to assemble
        // the block and reported the probe as unhealthy.
        let block = encode_response_headers(&[
            (b":status", b"200"),
            (b"x-trace-id", b"abc-123"),
            (b"server", b"sozu-test"),
        ]);
        assert!(block.len() >= 4, "HPACK block needs to be splittable");
        let split = block.len() / 2;
        let (head, tail) = block.split_at(split);

        // HEADERS with no END_HEADERS (flags = 0) — block continues.
        let mut buf = frame_with_header(0x01, 0, 1, head);
        // CONTINUATION (frame type 0x09) carrying the rest, END_HEADERS set.
        buf.extend_from_slice(&frame_with_header(0x09, FLAG_END_HEADERS, 1, tail));

        let cfg = h2c_config(0);
        assert_eq!(try_parse_h2c_status(&buf, &cfg), Some(true));
    }

    #[test]
    fn h2c_response_with_padded_priority_headers_decodes_status_200() {
        // Build a HEADERS frame with both PADDED and PRIORITY flags so the
        // parser must strip 1 + 5 = 6 bytes before handing the block to
        // loona_hpack.
        let block = encode_response_headers(&[(b":status", b"200")]);
        let pad_len: u8 = 3;

        let mut payload = Vec::new();
        payload.push(pad_len); // PADDED: 1-byte pad length
        payload.extend_from_slice(&[0u8, 0, 0, 0, 16]); // PRIORITY: stream-dep + weight
        payload.extend_from_slice(&block);
        payload.extend_from_slice(&[0u8; 3]); // padding bytes

        let flags = FLAG_PADDED | FLAG_PRIORITY | FLAG_END_HEADERS;
        let buf = frame_with_header(0x01, flags, 1, &payload);
        let cfg = h2c_config(0);
        assert_eq!(try_parse_h2c_status(&buf, &cfg), Some(true));
    }

    #[test]
    fn h2c_response_after_unrelated_settings_frame_decodes_healthy() {
        // The probe parser must skip over the server's SETTINGS frame
        // and any other connection-scoped frames before locating the
        // HEADERS on stream 1.
        let mut buf = frame_with_header(0x04, 0, 0, &[]); // SETTINGS, empty
        buf.extend_from_slice(&frame_with_header(0x04, 0x01, 0, &[])); // SETTINGS-ACK
        let block = encode_response_headers(&[(b":status", b"200")]);
        buf.extend_from_slice(&frame_with_header(0x01, FLAG_END_HEADERS, 1, &block));

        let cfg = h2c_config(0);
        assert_eq!(try_parse_h2c_status(&buf, &cfg), Some(true));
    }

    #[test]
    fn h2c_goaway_returns_unhealthy() {
        // GOAWAY: 8-byte payload (last-stream-id + error code).
        let buf = frame_with_header(0x07, 0, 0, &[0u8; 8]);
        let cfg = h2c_config(0);
        assert_eq!(try_parse_h2c_status(&buf, &cfg), Some(false));
    }

    #[test]
    fn h2c_truncated_frame_returns_none() {
        // Frame header claims a 10-byte payload but only 5 are present.
        let mut buf: Vec<u8> = vec![
            0,                // length high
            0,                // length mid
            10,               // length low = 10
            0x01,             // HEADERS
            FLAG_END_HEADERS, // flags
        ];
        buf.extend_from_slice(&1u32.to_be_bytes()); // stream id = 1
        buf.extend_from_slice(&[0u8; 5]); // partial payload
        let cfg = h2c_config(0);
        assert_eq!(try_parse_h2c_status(&buf, &cfg), None);
    }

    #[test]
    fn h2c_partial_frame_header_returns_none() {
        // Fewer than 9 bytes: the frame header has not even fully arrived
        // yet. The probe loop should report `None` so the caller keeps
        // reading rather than recording the backend as unhealthy.
        let cfg = h2c_config(0);
        for partial_len in 0usize..FRAME_HEADER_SIZE {
            let buf = vec![0u8; partial_len];
            assert_eq!(
                try_parse_h2c_status(&buf, &cfg),
                None,
                "partial buffer of {partial_len} byte(s) should be 'keep reading'"
            );
        }
    }

    #[test]
    fn h2c_continuation_without_preceding_headers_returns_unhealthy() {
        // CONTINUATION without a HEADERS predecessor is a protocol
        // error per RFC 9113 §6.10.
        let block = encode_response_headers(&[(b":status", b"200")]);
        let buf = frame_with_header(0x09, FLAG_END_HEADERS, 1, &block);
        let cfg = h2c_config(0);
        assert_eq!(try_parse_h2c_status(&buf, &cfg), Some(false));
    }
}
