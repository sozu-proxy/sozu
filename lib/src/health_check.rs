//! Non-blocking health checks for backends
//!
//! Health checks run within the single-threaded mio event loop using non-blocking TCP
//! connections. Each check cycle is triggered by a timer. For each backend with a
//! configured health check, we open a non-blocking TCP connection, then either:
//!
//! - `HealthCheckMode::Http`: send a minimal HTTP/1.1 (or h2c) GET request, parse the
//!   response status and judge it against the accepted statuses;
//! - `HealthCheckMode::Tcp`: send nothing and count the probe as passed once the
//!   connection is established, then close it.
//!
//! The result updates the backend's health state accordingly.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    hash::{Hash, Hasher},
    io::{Read, Write},
    net::SocketAddr,
    rc::{Rc, Weak},
    time::{Duration, Instant},
};

use mio::{Interest, Registry, Token, net::TcpStream};
use sozu_command::{
    proto::command::{Event, EventKind, HealthCheckConfig, HealthCheckMode},
    state::ClusterId,
};

use crate::metrics::names;
use crate::{
    backends::{Backend, BackendMap},
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
/// will actually use to reach those backends. Each backend to check is
/// `(backend_id, address, incarnation)`.
type PendingChecks = Vec<(
    ClusterId,
    HealthCheckConfig,
    bool,
    Vec<(String, SocketAddr, Weak<RefCell<Backend>>)>,
)>;

/// Tracks an in-flight health check connection
#[derive(Debug)]
struct InFlightCheck {
    stream: TcpStream,
    token: Token,
    cluster_id: ClusterId,
    backend_id: String,
    address: SocketAddr,
    /// The backend incarnation this probe was launched for. The result
    /// applies to it only while it is still in the cluster's list, so a
    /// probe that outlives a `RemoveBackend` (and a re-add of the same id
    /// and address) never mutates the replacement (#1821).
    backend: Weak<RefCell<Backend>>,
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
    /// `HealthCheckMode::Tcp`: the probe passes once the connection is
    /// established; nothing is written or read. Captured at probe-creation
    /// time for the same reason as `h2c`.
    tcp_connect_only: bool,
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
        // Invariant: every in-flight token sits inside the reserved namespace,
        // so subtracting the base above never underflows and every offset is a
        // valid slot index. Mirrors the bound enforced by `owns_token`.
        debug_assert!(
            in_flight.iter().all(|&o| o < HEALTH_CHECK_TOKEN_CAPACITY),
            "every in-flight token offset must fall within the slot capacity"
        );
        debug_assert!(
            in_flight.len() <= HEALTH_CHECK_TOKEN_CAPACITY,
            "cannot have more in-flight checks than the token slot capacity"
        );

        for _ in 0..HEALTH_CHECK_TOKEN_CAPACITY {
            let offset = self.next_token_id % HEALTH_CHECK_TOKEN_CAPACITY;
            self.next_token_id = self.next_token_id.wrapping_add(1);
            if !in_flight.contains(&offset) {
                let token = Token(HEALTH_CHECK_TOKEN_BASE + offset);
                // Post-condition: a freshly allocated token is owned by this
                // namespace and does not collide with any in-flight stream.
                debug_assert!(
                    self.owns_token(token),
                    "allocated token must fall inside the health-check namespace"
                );
                debug_assert!(
                    !in_flight.contains(&offset),
                    "allocated offset must not already be in flight"
                );
                return Some(token);
            }
        }
        // Exhaustion: the slot table is full. This is graceful (returns None),
        // never a panic — the caller records the probe as failed. The table is
        // only full when every slot is occupied.
        debug_assert_eq!(
            in_flight.len(),
            HEALTH_CHECK_TOKEN_CAPACITY,
            "allocation only fails when every slot is occupied"
        );
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
        let owned = token.0 >= HEALTH_CHECK_TOKEN_BASE
            && token.0 < HEALTH_CHECK_TOKEN_BASE + HEALTH_CHECK_TOKEN_CAPACITY;
        // The upper bound is load-bearing: it must reject the mux GOAWAY
        // sentinel `Token(usize::MAX)`, which would otherwise be misclaimed as
        // a health-check socket (CVE-class regression). Assert the relationship
        // the comparison encodes rather than restating it.
        debug_assert!(
            !owned || token.0 - HEALTH_CHECK_TOKEN_BASE < HEALTH_CHECK_TOKEN_CAPACITY,
            "an owned token must map to a valid bounded slot offset"
        );
        debug_assert!(
            owned || token != Token(HEALTH_CHECK_TOKEN_BASE),
            "the base token itself must always be classified as owned"
        );
        owned
    }

    /// Called by the server event loop when mio reports readiness for a health check socket.
    pub fn ready(&mut self, token: Token) {
        self.ready_tokens.insert(token);
        // Post-condition: the readiness set now records this token. The set is
        // drained wholesale in `progress_checks`, so a missed insert here would
        // strand the socket until timeout.
        debug_assert!(
            self.ready_tokens.contains(&token),
            "ready() must record the token in the readiness set"
        );
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
                // One probe in flight per backend incarnation: a probe still
                // draining for a removed incarnation does not delay the
                // first probe of its replacement.
                let backends_to_check: Vec<(String, SocketAddr, Weak<RefCell<Backend>>)> =
                    backend_list
                        .backends
                        .iter()
                        .filter(|b| {
                            b.borrow().status == crate::backends::BackendStatus::Normal
                                && !self
                                    .in_flight
                                    .iter()
                                    .any(|f| std::ptr::eq(f.backend.as_ptr(), Rc::as_ptr(b)))
                        })
                        .map(|b| {
                            let backend = b.borrow();
                            (
                                backend.backend_id.to_owned(),
                                backend.address,
                                Rc::downgrade(b),
                            )
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
            let tcp_connect_only = config.mode == HealthCheckMode::Tcp as i32;
            // A connect-only probe never writes or reads: it waits for the
            // writability that signals the end of the handshake.
            let interest = if tcp_connect_only {
                Interest::WRITABLE
            } else {
                Interest::READABLE | Interest::WRITABLE
            };

            for (backend_id, address, incarnation) in backends_to_check {
                match TcpStream::connect(address) {
                    Ok(mut stream) => {
                        let Some(token) = self.allocate_token() else {
                            // Token table exhausted — record the probe as failed
                            // so threshold logic can still fire, then move on.
                            // The allocator already logged at error level.
                            Self::record_check_result(
                                backends,
                                &cluster_id,
                                &incarnation,
                                false,
                                &config,
                            );
                            continue;
                        };
                        if let Err(e) = registry.register(&mut stream, token, interest) {
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
                                &incarnation,
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
                        let request_bytes = if tcp_connect_only {
                            None
                        } else if h2c {
                            Some(build_h2c_probe_bytes(probe_uri, address))
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
                            Some(
                                format!(
                                    "GET {probe_uri} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
                                )
                                .into_bytes(),
                            )
                        };
                        // A connect-only probe carries no request; every
                        // HTTP probe carries one.
                        debug_assert_eq!(
                            request_bytes.is_none(),
                            tcp_connect_only,
                            "only a TCP-mode probe may skip the HTTP request"
                        );
                        self.in_flight.push(InFlightCheck {
                            stream,
                            token,
                            cluster_id: cluster_id.to_owned(),
                            backend_id,
                            address,
                            backend: incarnation,
                            started_at: now,
                            timeout: Duration::from_secs(u64::from(config.timeout)),
                            request_bytes,
                            write_offset: 0,
                            response_buf: Vec::with_capacity(256),
                            config: config.to_owned(),
                            h2c,
                            tcp_connect_only,
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
                            &incarnation,
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
        // Post-condition of the wholesale take: the live readiness set is now
        // empty, so a token re-armed during this pass is handled next loop.
        debug_assert!(
            self.ready_tokens.is_empty(),
            "readiness set must be drained before processing in-flight checks"
        );
        let in_flight_before = self.in_flight.len();

        for (idx, check) in self.in_flight.iter_mut().enumerate() {
            // Index must address a live in-flight slot — the basis for the
            // descending-sort swap_remove below.
            debug_assert!(
                idx < in_flight_before,
                "in-flight index ({idx}) must be within the live slot range ({in_flight_before})"
            );
            // The write cursor never runs past the request it is draining.
            debug_assert!(
                check
                    .request_bytes
                    .as_ref()
                    .is_none_or(|r| check.write_offset <= r.len()),
                "write_offset must never exceed the request length"
            );

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

            if check.tcp_connect_only {
                // Nothing to send: the probe only waits for the handshake.
                // Dropping the stream after `deregister` closes it: a FIN
                // while the receive queue is empty, which is the usual case
                // since nothing was asked for. A backend that greets first
                // (an SMTP or SSH banner) and whose bytes already arrived
                // makes the kernel answer the close with an RST instead;
                // the probe never reads them, and the verdict is the same.
                if let Some(success) = tcp_connect_outcome(&check.stream) {
                    completed.push((idx, success));
                }
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
                    // A successful read never reports more bytes than the
                    // fixed-size stack buffer can hold.
                    debug_assert!(
                        n <= buf.len(),
                        "read reported {n} bytes into a {}-byte buffer",
                        buf.len()
                    );
                    if check.response_buf.len() + n > MAX_HEALTH_RESPONSE_SIZE {
                        completed.push((idx, false));
                        continue;
                    }
                    check.response_buf.extend_from_slice(&buf[..n]);
                    // The accumulator stays within the documented ceiling once
                    // the over-limit guard above has run.
                    debug_assert!(
                        check.response_buf.len() <= MAX_HEALTH_RESPONSE_SIZE,
                        "response buffer must stay within the max health response size"
                    );
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
        completed.sort_by_key(|entry| std::cmp::Reverse(entry.0));
        // We never schedule the same slot for removal twice, and never more
        // removals than there are in-flight checks.
        debug_assert!(
            completed.len() <= in_flight_before,
            "cannot complete more checks ({}) than were in flight ({in_flight_before})",
            completed.len()
        );
        debug_assert!(
            completed.windows(2).all(|w| w[0].0 > w[1].0),
            "completed indices must be strictly descending and unique for swap_remove safety"
        );
        for (idx, success) in completed {
            let len_before = self.in_flight.len();
            let mut check = self.in_flight.swap_remove(idx);
            // swap_remove drops exactly one in-flight slot.
            debug_assert_eq!(
                self.in_flight.len(),
                len_before - 1,
                "swap_remove must drop exactly one in-flight check"
            );
            let _ = registry.deregister(&mut check.stream);
            Self::record_check_result(
                backends,
                &check.cluster_id,
                &check.backend,
                success,
                &check.config,
            );
        }
    }

    /// Apply a probe result to the backend `incarnation` it was launched
    /// for. A result whose incarnation left the cluster (removed, or
    /// replaced by a re-add of the same id and address) is dropped; it never
    /// lands on a sibling at the same address nor on the replacement (#1821).
    fn record_check_result(
        backends: &Rc<RefCell<BackendMap>>,
        cluster_id: &str,
        incarnation: &Weak<RefCell<Backend>>,
        success: bool,
        config: &HealthCheckConfig,
    ) {
        let mut backend_map = backends.borrow_mut();
        let Some(backend_list) = backend_map.backends.get_mut(cluster_id) else {
            return;
        };

        let Some(backend_ref) = backend_list.find_incarnation(incarnation) else {
            debug!(
                "{} dropping a result for a backend no longer in cluster {}",
                log_context!(),
                cluster_id
            );
            return;
        };

        let mut backend = backend_ref.borrow_mut();
        let backend_id = backend.backend_id.to_owned();
        let backend_id = backend_id.as_str();
        let address = backend.address;

        if success {
            // Snapshot the hysteresis status before the counter mutation so we
            // can assert the flip happens exactly at the threshold. Read only
            // inside debug_assert! → dead in release, but must compile.
            let was_healthy = backend.health.is_healthy();
            let transitioned = backend.health.record_success(config.healthy_threshold);
            // A success resets the failure counter and bumps the success
            // counter; the rise counter must stay within [0, threshold] at the
            // transition boundary. `transitioned` is true iff we crossed from
            // unhealthy to healthy at exactly the configured threshold.
            debug_assert!(
                backend.health.consecutive_failures == 0,
                "a recorded success must zero the consecutive-failure counter"
            );
            debug_assert_eq!(
                transitioned,
                !was_healthy && backend.health.is_healthy(),
                "transition flag must be set iff the backend just flipped to healthy"
            );
            debug_assert!(
                !transitioned || backend.health.consecutive_successes >= config.healthy_threshold,
                "an UP transition only fires once the rise counter reaches the healthy threshold"
            );
            debug_assert!(
                !transitioned || backend.health.is_healthy(),
                "after an UP transition the backend must report healthy"
            );
            if transitioned {
                info!(
                    "{} backend {} at {} marked UP (health check passed {} consecutive times) for cluster {}",
                    log_context!(),
                    backend_id,
                    address,
                    config.healthy_threshold,
                    cluster_id
                );
                incr!(names::health_check::UP);
                gauge!(
                    names::backend::AVAILABLE,
                    1,
                    Some(cluster_id),
                    Some(backend_id)
                );
                push_event(Event {
                    kind: EventKind::HealthCheckHealthy as i32,
                    cluster_id: Some(cluster_id.to_owned()),
                    backend_id: Some(backend_id.to_owned()),
                    address: Some(address.into()),
                    metric_detail: None,
                });
            }
            count!(names::health_check::SUCCESS, 1);
        } else {
            let was_healthy = backend.health.is_healthy();
            let transitioned = backend.health.record_failure(config.unhealthy_threshold);
            // A failure resets the success counter and bumps the failure
            // counter; the fall counter must stay within [0, threshold] at the
            // transition boundary. `transitioned` is true iff we crossed from
            // healthy to unhealthy at exactly the configured threshold.
            debug_assert!(
                backend.health.consecutive_successes == 0,
                "a recorded failure must zero the consecutive-success counter"
            );
            debug_assert_eq!(
                transitioned,
                was_healthy && !backend.health.is_healthy(),
                "transition flag must be set iff the backend just flipped to unhealthy"
            );
            debug_assert!(
                !transitioned || backend.health.consecutive_failures >= config.unhealthy_threshold,
                "a DOWN transition only fires once the fall counter reaches the unhealthy threshold"
            );
            debug_assert!(
                !transitioned || !backend.health.is_healthy(),
                "after a DOWN transition the backend must report unhealthy"
            );
            if transitioned {
                warn!(
                    "{} backend {} at {} marked DOWN (health check failed {} consecutive times) for cluster {}",
                    log_context!(),
                    backend_id,
                    address,
                    config.unhealthy_threshold,
                    cluster_id
                );
                incr!(names::health_check::DOWN);
                gauge!(
                    names::backend::AVAILABLE,
                    0,
                    Some(cluster_id),
                    Some(backend_id)
                );
                push_event(Event {
                    kind: EventKind::HealthCheckUnhealthy as i32,
                    cluster_id: Some(cluster_id.to_owned()),
                    backend_id: Some(backend_id.to_owned()),
                    address: Some(address.into()),
                    metric_detail: None,
                });
            }
            count!(names::health_check::FAILURE, 1);
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
        // Gauge pairing: the healthy count is a subset of the total, so the
        // emitted gauge can never exceed the cluster size (it is a usize, but
        // a gauge above `total` would be an accounting bug, not rounding).
        debug_assert!(
            healthy <= total,
            "healthy backend count ({healthy}) must not exceed total ({total})"
        );
        if total > 0 {
            gauge!(
                names::health_check::HEALTHY_BACKENDS,
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
            .retain(|check| *check.cluster_id != *cluster_id);
        // Post-condition: no in-flight probe references the removed cluster.
        debug_assert!(
            self.in_flight.iter().all(|c| *c.cluster_id != *cluster_id),
            "remove_cluster must drop every in-flight check for the cluster"
        );
        debug_assert!(
            !self.last_check_time.contains_key(cluster_id),
            "remove_cluster must forget the cluster's last-check timestamp"
        );
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

/// Judge the final HTTP/1.1 response: interim `1xx` responses (RFC 9110
/// §15.2, e.g. `100 Continue` or `103 Early Hints`) are skipped, header
/// section included, so the verdict is the status of the response that
/// follows them. `101 Switching Protocols` is final. `None` means the
/// final status line has not fully arrived yet.
fn try_parse_status_line(buf: &[u8], config: &HealthCheckConfig) -> Option<bool> {
    let mut head = buf;
    loop {
        let line_end = find_subslice(head, b"\r\n")?;
        let status_line = std::str::from_utf8(&head[..line_end]).ok()?;
        // The status line is the prefix before the first CRLF, so it can
        // never be longer than the buffer it was sliced from.
        debug_assert!(
            status_line.len() < head.len(),
            "status line must be a strict prefix ending before the CRLF"
        );

        let (_, rest) = status_line.split_once(' ')?;
        let status_str = rest.split(' ').next()?;
        let status_code: u32 = status_str.parse().unwrap_or(0);
        if !is_interim_status(status_code) {
            return Some(is_status_healthy(status_code, config));
        }
        // An interim response ends with its (possibly empty) header section.
        let head_end = find_subslice(head, b"\r\n\r\n")? + 4;
        debug_assert!(
            head_end > line_end && head_end <= head.len(),
            "skipping an interim response must consume at least its status line"
        );
        head = &head[head_end..];
    }
}

/// Interim (informational) statuses precede the final response; `101`
/// ends the HTTP exchange and is final for the probe.
fn is_interim_status(status: u32) -> bool {
    (100..200).contains(&status) && status != 101
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Outcome of a connect-only probe on a readiness event: `Some(true)` once
/// the handshake completed, `Some(false)` when the socket reports an error
/// (refused, unreachable, reset), `None` while the connect is still in
/// progress — mio may report writability before it completes, which
/// `peer_addr` answers with `NotConnected`.
fn tcp_connect_outcome(stream: &TcpStream) -> Option<bool> {
    match stream.take_error() {
        Ok(None) => {}
        Ok(Some(_)) | Err(_) => return Some(false),
    }
    match stream.peer_addr() {
        Ok(_) => Some(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotConnected => None,
        Err(_) => Some(false),
    }
}

/// Whether an HTTP probe's response status passes: within one of
/// `accepted_statuses` when that list is set, otherwise any 2xx for
/// `expected_status == 0` or exactly `expected_status`. Validation
/// (`validate_health_check_config` in `command/src/config.rs`) refuses
/// a config that sets both, so the list never silently overrides a
/// non-zero `expected_status`.
fn is_status_healthy(actual: u32, config: &HealthCheckConfig) -> bool {
    let expected = config.expected_status;
    let healthy = if !config.accepted_statuses.is_empty() {
        config
            .accepted_statuses
            .iter()
            .any(|range| (range.start..=range.end).contains(&actual))
    } else if expected == 0 {
        (200..300).contains(&actual)
    } else {
        actual == expected
    };
    // Without a list, a specific expected status means exact equality.
    debug_assert!(
        !config.accepted_statuses.is_empty() || expected == 0 || healthy == (actual == expected),
        "with a specific expected status, health must be exact equality"
    );
    // With a list, health is membership in it and nothing else.
    debug_assert!(
        config.accepted_statuses.is_empty()
            || healthy
                == config
                    .accepted_statuses
                    .iter()
                    .any(|range| range.start <= actual && actual <= range.end),
        "with accepted statuses, health must be membership in one of the ranges"
    );
    healthy
}

/// Compose a bare-minimum h2c (HTTP/2 over cleartext, prior-knowledge)
/// probe: the 24-byte connection preface, an empty client SETTINGS
/// frame (acknowledging the spec-mandated handshake), and a single
/// HEADERS frame on stream 1 carrying `GET <path>` with
/// END_STREAM | END_HEADERS.
///
/// HPACK encoding is delegated to `crate::protocol::mux::hpack::Encoder` —
/// the same encoder the H2 mux uses for live traffic
/// (`lib/src/protocol/mux/converter.rs`). The probe inherits whatever
/// static/dynamic-table behaviour the encoder picks. The connection
/// preface comes from `serializer::H2_PRI` so the probe and the live
/// mux stay in lockstep.
fn build_h2c_probe_bytes(uri: &str, address: SocketAddr) -> Vec<u8> {
    let authority = address.to_string();

    // Build the HPACK header block first so we know its length for the
    // HEADERS frame header. A fresh encoder per probe keeps things
    // stateless — we never reuse the dynamic table across probes.
    let mut encoder = crate::protocol::mux::hpack::Encoder::new();
    let mut hpack: Vec<u8> = Vec::new();
    let headers: [(&[u8], &[u8]); 4] = [
        (b":method", b"GET"),
        (b":scheme", b"http"),
        (b":path", uri.as_bytes()),
        (b":authority", authority.as_bytes()),
    ];
    encoder.encode_into(headers, &mut hpack);

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
    // Post-condition: a well-formed probe begins with the H2 connection
    // preface and carries exactly preface + 2 frame headers + the HPACK block.
    debug_assert!(
        out.starts_with(H2_PRI.as_bytes()),
        "an h2c probe must begin with the connection preface"
    );
    debug_assert_eq!(
        out.len(),
        H2_PRI.len() + FRAME_HEADER_SIZE * 2 + hpack.len(),
        "probe length must be preface + SETTINGS + HEADERS header + HPACK block"
    );
    out
}

/// Walk the buffered H2 frames looking for a HEADERS frame (plus any
/// CONTINUATION frames until END_HEADERS) on stream 1, then HPACK-decode
/// the assembled block via `crate::protocol::mux::hpack::Decoder` and pull `:status`.
///
/// Returns:
///
/// * `Some(true)` — the final `:status` decoded and passes [`is_status_healthy`]
///   (`config.accepted_statuses`, else `config.expected_status`); interim
///   `1xx` HEADERS blocks before it are skipped.
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
    // One HPACK decoder for the whole walk: an interim HEADERS block may
    // insert into the dynamic table the final block then references.
    let mut decoder = crate::protocol::mux::hpack::Decoder::new();

    while !remaining.is_empty() {
        // The `mux::parser::frame_header` is built from `nom::number::complete`
        // primitives which emit `Err::Error` (not `Err::Incomplete`) on short
        // input, so distinguish "header still arriving" from "header arrived
        // but is malformed" by an explicit length check before parsing.
        if remaining.len() < FRAME_HEADER_SIZE {
            return None;
        }
        let consumable = remaining.len();
        let (rest, header) = match frame_header(remaining, MAX_FRAME_SIZE) {
            Ok(parsed) => parsed,
            // Header bytes are present but parser rejected them: malformed
            // framing (oversized payload, invalid stream-id parity, etc.).
            // → probe is unhealthy.
            Err(_) => return Some(false),
        };
        // Parser post-conditions: it consumes exactly the 9-byte header (never
        // grows its input) and honours the size bound it was handed.
        debug_assert!(
            rest.len() < consumable,
            "frame_header must consume at least the fixed frame header"
        );
        debug_assert_eq!(
            consumable - rest.len(),
            FRAME_HEADER_SIZE,
            "frame_header must consume exactly the fixed-size frame header"
        );
        debug_assert!(
            header.payload_len <= MAX_FRAME_SIZE,
            "frame_header must enforce the max-frame-size bound it was given"
        );

        let payload_len = header.payload_len as usize;
        if rest.len() < payload_len {
            // Body of the frame has not arrived yet.
            return None;
        }
        let (payload, after) = rest.split_at(payload_len);
        // Splitting at the declared payload length must not lose bytes: the
        // payload plus the remainder reconstitute `rest`, and the walk makes
        // forward progress so the loop terminates.
        debug_assert_eq!(
            payload.len(),
            payload_len,
            "payload split must yield exactly the declared payload length"
        );
        debug_assert_eq!(
            payload.len() + after.len(),
            rest.len(),
            "payload + remainder must equal the pre-split buffer"
        );
        debug_assert!(
            after.len() < remaining.len(),
            "each iteration must shrink the remaining buffer to guarantee termination"
        );

        match header.frame_type {
            FrameType::Headers if header.stream_id == 1 => {
                let block = strip_padded_priority(payload, header.flags)?;
                let mut accumulator = headers_block.take().unwrap_or_default();
                accumulator.extend_from_slice(block);
                if header.flags & FLAG_END_HEADERS != 0 {
                    match decode_status_from_block(&mut decoder, &accumulator) {
                        // RFC 9113 §8.1: interim responses arrive as their
                        // own HEADERS before the final one; keep walking.
                        Some(code) if is_interim_status(code) => {}
                        Some(code) => return Some(is_status_healthy(code, config)),
                        None => return Some(false),
                    }
                } else {
                    headers_block = Some(accumulator);
                }
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
                    match decode_status_from_block(&mut decoder, &accumulator) {
                        // RFC 9113 §8.1: interim responses arrive as their
                        // own HEADERS before the final one; keep walking.
                        Some(code) if is_interim_status(code) => {}
                        Some(code) => return Some(is_status_healthy(code, config)),
                        None => return Some(false),
                    }
                } else {
                    headers_block = Some(accumulator);
                }
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
/// padding/priority but the payload is too short to satisfy them; the
/// HEADERS arm of `try_parse_h2c_status` propagates it with `?`, so the
/// probe keeps reading and is judged at EOF (unparsable, unhealthy) or
/// when its timeout fires.
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
    // Post-condition: the surviving window `[start, end)` lies inside the
    // input payload and never grows it. A returned slice is therefore a
    // sub-slice of the original frame body.
    debug_assert!(
        start <= end && end <= payload.len(),
        "stripped header window [{start}, {end}) must lie within the payload ({})",
        payload.len()
    );
    let block = payload.get(start..end)?;
    debug_assert!(
        block.len() <= payload.len(),
        "stripped block must never be larger than the original payload"
    );
    Some(block)
}

/// Run the walk's `crate::protocol::mux::hpack::Decoder` over an assembled
/// HEADERS block and return its `:status`. Unknown HPACK encodings,
/// malformed integers, Huffman fallbacks, and `:status` values that fail
/// UTF-8 / numeric parsing all yield `None` — the caller records the probe
/// as unhealthy, never panics.
fn decode_status_from_block(
    decoder: &mut crate::protocol::mux::hpack::Decoder,
    block: &[u8],
) -> Option<u32> {
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
        return None;
    }
    status
}

#[cfg(test)]
mod tests {
    use sozu_command::proto::command::HttpStatusRange;

    use super::*;
    use crate::backends::{Backend, HealthState};

    /// An HTTP-mode config judging statuses by `expected_status` and
    /// `accepted_statuses` (given as `(start, end)` pairs).
    fn status_config(expected: u32, accepted: &[(u32, u32)]) -> HealthCheckConfig {
        HealthCheckConfig {
            expected_status: expected,
            accepted_statuses: accepted
                .iter()
                .map(|&(start, end)| HttpStatusRange { start, end })
                .collect(),
            ..h2c_config(0)
        }
    }

    #[test]
    fn test_is_status_healthy_any_2xx() {
        let config = status_config(0, &[]);
        assert!(is_status_healthy(200, &config));
        assert!(is_status_healthy(204, &config));
        assert!(is_status_healthy(299, &config));
        assert!(!is_status_healthy(301, &config));
        assert!(!is_status_healthy(500, &config));
        assert!(!is_status_healthy(0, &config));
    }

    #[test]
    fn test_is_status_healthy_specific() {
        let config = status_config(200, &[]);
        assert!(is_status_healthy(200, &config));
        assert!(!is_status_healthy(204, &config));
        assert!(!is_status_healthy(500, &config));
    }

    #[test]
    fn accepted_statuses_list_of_codes_accepts_only_those_codes() {
        let config = status_config(0, &[(200, 200), (404, 404)]);
        assert!(is_status_healthy(200, &config));
        assert!(is_status_healthy(404, &config));
        assert!(
            !is_status_healthy(204, &config),
            "a 2xx outside the list fails"
        );
        assert!(!is_status_healthy(403, &config));
        assert!(!is_status_healthy(500, &config));
    }

    #[test]
    fn accepted_statuses_ranges_are_inclusive_at_both_bounds() {
        let config = status_config(0, &[(200, 399), (401, 401)]);
        assert!(is_status_healthy(200, &config));
        assert!(is_status_healthy(302, &config));
        assert!(is_status_healthy(399, &config));
        assert!(is_status_healthy(401, &config));
        assert!(!is_status_healthy(199, &config));
        assert!(!is_status_healthy(400, &config));
        assert!(!is_status_healthy(404, &config));
        assert!(!is_status_healthy(500, &config));
    }

    #[test]
    fn accepted_statuses_any_accepts_every_valid_status_but_no_garbage() {
        // `"any"` parses to 100-599 (`parse_http_status_range` in
        // `command/src/config.rs`).
        let config = status_config(0, &[(100, 599)]);
        for status in [100, 200, 301, 401, 404, 500, 503, 599] {
            assert!(is_status_healthy(status, &config), "{status} must pass");
        }
        // An unparsable status line yields 0, never a pass.
        assert!(!is_status_healthy(0, &config));
        assert!(!is_status_healthy(600, &config));
        let buf = b"HTTP/1.1 500 Internal Server Error\r\n\r\n";
        assert_eq!(try_parse_status_line(buf, &config), Some(true));
        let buf = b"HTTP/1.1 garbage\r\n\r\n";
        assert_eq!(try_parse_status_line(buf, &config), Some(false));
    }

    #[test]
    fn accepted_statuses_replace_the_default_2xx_when_expected_status_is_zero() {
        // `expected_status = 0` is the "unset" value: the list alone decides,
        // so a 2xx outside the list fails and a 404 inside it passes.
        let config = status_config(0, &[(404, 404)]);
        assert!(!is_status_healthy(200, &config));
        assert!(is_status_healthy(404, &config));
        // The h2c path judges `:status` with the same rule.
        let block = encode_response_headers(&[(b":status", b"404")]);
        let buf = frame_with_header(0x01, FLAG_END_HEADERS, 1, &block);
        assert_eq!(try_parse_h2c_status(&buf, &config), Some(true));
        let block = encode_response_headers(&[(b":status", b"200")]);
        let buf = frame_with_header(0x01, FLAG_END_HEADERS, 1, &block);
        assert_eq!(try_parse_h2c_status(&buf, &config), Some(false));
    }

    #[test]
    fn http1_interim_responses_are_skipped_for_the_final_status() {
        // Default 2xx: a `103 Early Hints` with headers, then the final 200.
        let default = status_config(0, &[]);
        let buf = b"HTTP/1.1 103 Early Hints\r\nLink: </a.css>\r\n\r\nHTTP/1.1 200 OK\r\n\r\n";
        assert_eq!(try_parse_status_line(buf, &default), Some(true));
        // `1xx` accepted: an interim `100 Continue` must not stand in for a
        // final 500.
        let informational = status_config(0, &[(100, 199)]);
        let buf = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 500 Internal Server Error\r\n\r\n";
        assert_eq!(try_parse_status_line(buf, &informational), Some(false));
        // `101 Switching Protocols` is final.
        let buf = b"HTTP/1.1 101 Switching Protocols\r\n\r\n";
        assert_eq!(try_parse_status_line(buf, &informational), Some(true));
        // Until the final status line arrives, keep reading.
        for partial in [
            &b"HTTP/1.1 100 Continue\r\n"[..],
            b"HTTP/1.1 100 Continue\r\n\r\n",
            b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200",
        ] {
            assert_eq!(
                try_parse_status_line(partial, &default),
                None,
                "{partial:?}"
            );
        }
    }

    #[test]
    fn h2c_interim_headers_are_skipped_for_the_final_status() {
        // One encoder for both blocks, as a server would use. `x-hint` is
        // not in the HPACK static table, so the encoder inserts it in the
        // dynamic table with the interim block and the final block refers
        // to that entry: a decoder rebuilt per block cannot decode it.
        let mut encoder = crate::protocol::mux::hpack::Encoder::new();
        let mut interim = Vec::new();
        encoder.encode_into(
            [(&b":status"[..], &b"103"[..]), (b"x-hint", b"warm")],
            &mut interim,
        );
        let mut last = Vec::new();
        encoder.encode_into(
            [(&b":status"[..], &b"200"[..]), (b"x-hint", b"warm")],
            &mut last,
        );
        let mut buf = frame_with_header(0x01, FLAG_END_HEADERS, 1, &interim);
        assert_eq!(
            try_parse_h2c_status(&buf, &h2c_config(0)),
            None,
            "wait for the final HEADERS"
        );
        buf.extend_from_slice(&frame_with_header(0x01, FLAG_END_HEADERS, 1, &last));
        assert_eq!(try_parse_h2c_status(&buf, &h2c_config(0)), Some(true));
        // `1xx` accepted: the interim block is still not the verdict.
        let mut encoder = crate::protocol::mux::hpack::Encoder::new();
        let mut interim = Vec::new();
        encoder.encode_into([(&b":status"[..], &b"100"[..])], &mut interim);
        let mut last = Vec::new();
        encoder.encode_into([(&b":status"[..], &b"500"[..])], &mut last);
        let mut buf = frame_with_header(0x01, FLAG_END_HEADERS, 1, &interim);
        buf.extend_from_slice(&frame_with_header(0x01, FLAG_END_HEADERS, 1, &last));
        assert_eq!(
            try_parse_h2c_status(&buf, &status_config(0, &[(100, 199)])),
            Some(false)
        );
    }

    #[test]
    fn tcp_connect_outcome_reports_established_and_refused() {
        // Established: a listener completes the handshake in the kernel
        // without `accept`.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let stream = TcpStream::connect(address).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut outcome = tcp_connect_outcome(&stream);
        while outcome.is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            outcome = tcp_connect_outcome(&stream);
        }
        assert_eq!(outcome, Some(true), "a listening backend must pass");

        // Refused: a socket bound but never listening holds its port, so
        // no concurrent test can take it while the kernel answers RST.
        drop(stream);
        drop(listener);
        let closed =
            socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
        closed
            .bind(&"127.0.0.1:0".parse::<SocketAddr>().unwrap().into())
            .unwrap();
        let address = closed.local_addr().unwrap().as_socket().unwrap();
        let stream = TcpStream::connect(address).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut outcome = tcp_connect_outcome(&stream);
        while outcome.is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            outcome = tcp_connect_outcome(&stream);
        }
        assert_eq!(outcome, Some(false), "a refused connection must fail");
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
            mode: HealthCheckMode::Http as i32,
            accepted_statuses: Vec::new(),
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

    /// The health of the backend `backend_id` at `address` in `cluster`.
    fn is_healthy_by_id(
        backends: &Rc<RefCell<BackendMap>>,
        cluster: &str,
        backend_id: &str,
        address: SocketAddr,
    ) -> bool {
        backends.borrow().backends[cluster]
            .find_backend_by_identity(backend_id, &address)
            .expect("the backend is in the cluster")
            .borrow()
            .health
            .is_healthy()
    }

    /// #1821: a probe launched before `RemoveBackend` can complete after the
    /// same id and address are added again; its result belongs to the
    /// removed incarnation and must not mark the replacement DOWN.
    #[test]
    fn late_result_after_remove_and_readd_same_identity_does_not_mutate_replacement() {
        const CLUSTER: &str = "late-result-cluster";
        const BACKEND: &str = "late-result-backend";
        let address: SocketAddr = "127.0.0.1:23110".parse().unwrap();
        let mut backend_map = BackendMap::new();
        backend_map.add_backend(CLUSTER, Backend::new(BACKEND, address, None, None, None));
        let backends = Rc::new(RefCell::new(backend_map));
        // What `initiate_checks` captures when it launches the probe, and a
        // session still holding the removed backend, which keeps it alive.
        let held = Rc::clone(&backends.borrow().backends[CLUSTER].backends[0]);
        let stale = Rc::downgrade(&held);

        assert!(
            backends
                .borrow_mut()
                .remove_backend(CLUSTER, BACKEND, &address)
        );
        backends
            .borrow_mut()
            .add_backend(CLUSTER, Backend::new(BACKEND, address, None, None, None));

        let mut config = h2c_config(0);
        config.unhealthy_threshold = 1;
        HealthChecker::record_check_result(&backends, CLUSTER, &stale, false, &config);
        assert!(
            is_healthy_by_id(&backends, CLUSTER, BACKEND, address),
            "a late result from the removed incarnation must not mark its replacement DOWN"
        );

        // Positive control: a result for the replacement does apply.
        let current = Rc::downgrade(&backends.borrow().backends[CLUSTER].backends[0]);
        HealthChecker::record_check_result(&backends, CLUSTER, &current, false, &config);
        assert!(
            !is_healthy_by_id(&backends, CLUSTER, BACKEND, address),
            "a result for the live incarnation must mark it DOWN"
        );
    }

    /// #1821: two ids may share an address; a probe result updates the id
    /// that was probed, never the first sibling at that address.
    #[test]
    fn probe_result_updates_the_backend_id_that_was_probed() {
        const CLUSTER: &str = "sibling-cluster";
        let address: SocketAddr = "127.0.0.1:23111".parse().unwrap();
        let mut backend_map = BackendMap::new();
        backend_map.add_backend(CLUSTER, Backend::new("a", address, None, None, None));
        backend_map.add_backend(CLUSTER, Backend::new("b", address, None, None, None));
        let backends = Rc::new(RefCell::new(backend_map));
        let probed = Rc::downgrade(
            backends.borrow().backends[CLUSTER]
                .find_backend_by_identity("b", &address)
                .unwrap(),
        );

        let mut config = h2c_config(0);
        config.unhealthy_threshold = 1;
        HealthChecker::record_check_result(&backends, CLUSTER, &probed, false, &config);
        assert_eq!(
            (
                is_healthy_by_id(&backends, CLUSTER, "a", address),
                is_healthy_by_id(&backends, CLUSTER, "b", address),
            ),
            (true, false),
            "a failed probe for backend b must mutate b, not the first backend at its address"
        );
    }

    fn h2c_config(expected: u32) -> HealthCheckConfig {
        HealthCheckConfig {
            uri: "/health".to_owned(),
            interval: 10,
            timeout: 5,
            healthy_threshold: 3,
            unhealthy_threshold: 3,
            expected_status: expected,
            mode: HealthCheckMode::Http as i32,
            accepted_statuses: Vec::new(),
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
    /// keeps the tests aligned with whatever the encoder picks for static
    /// vs. literal vs. Huffman encoding instead of pinning bytes.
    fn encode_response_headers(headers: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut encoder = crate::protocol::mux::hpack::Encoder::new();
        let mut out = Vec::new();
        encoder.encode_into(headers.iter().copied(), &mut out);
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
        let mut decoder = crate::protocol::mux::hpack::Decoder::new();
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
            .expect("the HPACK decoder decodes a freshly-encoded probe");
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
        // the HPACK decoder.
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
