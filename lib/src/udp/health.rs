//! Endpoint-bound active health checks for UDP backends (MASTER-PLAN §7).
//!
//! UDP has no reliable liveness signal of its own, so health is bound to the
//! **endpoint**, not the listener:
//!
//! - **Primary — companion TCP probe**: a non-blocking mio TCP `connect` to a
//!   configurable `tcp_port` (default = the backend data port). Connection
//!   established ⇒ healthy; refused/timeout ⇒ unhealthy. This is the
//!   industry-standard signal (IPVS/Keepalived, HAProxy `check port`, Envoy
//!   `health_check_config.port_value`, nginx `port=`). It is a *hint*, not
//!   proof of UDP reachability — documented as such.
//! - **Secondary — app UDP probe**: send a configured payload, expect any reply
//!   within a timeout; silence ⇒ unhealthy. (State machine wired; see
//!   [`UdpHealthChecker::owns_token`] notes on integration scope.)
//!
//! Results feed `Backend::health` via rise/fall hysteresis
//! ([`HealthState`](crate::backends::HealthState)), so backend **selection**
//! (`BackendMap::next_available_backend_with_key`) skips unhealthy backends.
//! **Fail-open** is inherited: when every backend reads unhealthy the selection
//! routes over the full configured set rather than black-holing. Flows already
//! pinned to a now-unhealthy backend stay until idle-timeout (the flow table
//! pins them; health only steers *new* selections).
//!
//! Everything runs **non-blocking in the event loop** — Sōzu has no worker
//! background threads. Probes are scheduled off `probe_interval_seconds` and
//! never block the loop.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io::ErrorKind,
    net::SocketAddr,
    rc::Rc,
    time::{Duration, Instant},
};

use mio::{
    Interest, Registry, Token,
    net::{TcpStream, UdpSocket},
};
use sozu_command::{proto::command::UdpHealthConfig, state::ClusterId};

use crate::backends::BackendMap;
use crate::metrics::names;
use crate::socket::udp_connect;

/// Canonical log envelope tag for the UDP health prober.
macro_rules! log_context {
    () => {
        "UDP-HEALTH"
    };
}

/// Base of the dedicated mio token namespace for UDP health-check sockets.
/// Chosen disjoint from the HTTP `HealthChecker` range (`1 << 24`) and from
/// slab session tokens: `[BASE, BASE + CAPACITY)`.
const UDP_HEALTH_TOKEN_BASE: usize = (1 << 24) + (1 << 20);
/// Maximum concurrent in-flight UDP health probes.
const UDP_HEALTH_TOKEN_CAPACITY: usize = 1 << 16;

/// Resolved per-cluster UDP health configuration, captured from the cluster's
/// `UdpHealthConfig` (proto) at registration time so the prober does not race a
/// mid-probe reconfig.
#[derive(Clone, Debug)]
pub struct UdpHealthSettings {
    /// Companion TCP probe port; `None` = the backend data port.
    pub tcp_port: Option<u16>,
    /// Consecutive successes before a backend is marked up.
    pub rise: u32,
    /// Consecutive failures before a backend is marked down.
    pub fall: u32,
    /// Seconds between probe cycles.
    pub interval: Duration,
    /// Per-probe response timeout.
    pub timeout: Duration,
    /// Optional app UDP-probe payload (secondary signal).
    pub udp_probe_payload: Option<Vec<u8>>,
}

impl UdpHealthSettings {
    /// Build the resolved settings from a proto `UdpHealthConfig`, applying the
    /// documented defaults (rise 2, fall 3, interval 5s, timeout 2s).
    pub fn from_proto(cfg: &UdpHealthConfig) -> Self {
        UdpHealthSettings {
            tcp_port: cfg.tcp_port.map(|p| p as u16),
            rise: cfg.rise.unwrap_or(2),
            fall: cfg.fall.unwrap_or(3),
            interval: Duration::from_secs(u64::from(cfg.probe_interval_seconds.unwrap_or(5))),
            timeout: Duration::from_secs(u64::from(cfg.probe_timeout_seconds.unwrap_or(2))),
            udp_probe_payload: cfg.udp_probe_payload.clone(),
        }
    }
}

/// A batch of backends due for probing, grouped by cluster:
/// `(cluster_id, resolved settings, [(backend_id, address)])`.
type ProbeBatch = Vec<(ClusterId, UdpHealthSettings, Vec<(String, SocketAddr)>)>;

/// The transport of an in-flight probe.
///
/// - `Tcp` is the **primary** companion probe: a writable, error-free connected
///   TCP socket is the liveness hint.
/// - `Udp` is the **secondary** app-level probe: a configured payload was sent
///   on a connected UDP socket; **any** readable reply within the timeout is the
///   success signal, silence (timeout) is failure.
enum ProbeSocket {
    Tcp(TcpStream),
    Udp(UdpSocket),
}

impl ProbeSocket {
    /// Deregister the underlying socket from the mio registry on completion.
    fn deregister(&mut self, registry: &Registry) {
        match self {
            ProbeSocket::Tcp(s) => {
                let _ = registry.deregister(s);
            }
            ProbeSocket::Udp(s) => {
                let _ = registry.deregister(s);
            }
        }
    }
}

/// An in-flight non-blocking probe (TCP companion or UDP app-level).
struct InFlightProbe {
    socket: ProbeSocket,
    token: Token,
    cluster_id: ClusterId,
    backend_id: String,
    address: SocketAddr,
    started_at: Instant,
    timeout: Duration,
    rise: u32,
    fall: u32,
}

/// Timer-driven, non-blocking UDP backend health prober. Owned by `UdpProxy`;
/// driven from the server event loop exactly like the HTTP `HealthChecker`
/// (`poll` once per iteration, `ready(token)` on owned readiness).
#[derive(Default)]
pub struct UdpHealthChecker {
    /// Per-cluster resolved settings, set when an `AddCluster` carries a `udp`
    /// block with a health config.
    settings: HashMap<ClusterId, UdpHealthSettings>,
    in_flight: Vec<InFlightProbe>,
    last_check: HashMap<ClusterId, Instant>,
    next_token_id: usize,
    ready_tokens: HashSet<Token>,
}

impl UdpHealthChecker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) a cluster's health settings. `None` removes them
    /// (health disabled for that cluster). When removing, in-flight probes for
    /// the cluster are **deregistered from the mio registry before being
    /// dropped** — mirroring the completion path in [`Self::progress`]. Skipping
    /// the deregister (as the old code did) leaked the probe fd's registration
    /// until the socket was eventually closed, and could leave a stale token
    /// mapping in the poller.
    pub fn set_cluster(
        &mut self,
        cluster_id: &str,
        settings: Option<UdpHealthSettings>,
        registry: &Registry,
    ) {
        match settings {
            Some(s) => {
                self.settings.insert(cluster_id.to_owned(), s);
            }
            None => {
                self.settings.remove(cluster_id);
                self.last_check.remove(cluster_id);
                // Partition in place: deregister the probes belonging to the
                // removed cluster, keep the rest. `retain` cannot deregister
                // (the predicate gets `&`, deregister needs `&mut`), so drain +
                // refill.
                let mut kept = Vec::with_capacity(self.in_flight.len());
                for mut probe in self.in_flight.drain(..) {
                    if probe.cluster_id == cluster_id {
                        probe.socket.deregister(registry);
                    } else {
                        kept.push(probe);
                    }
                }
                self.in_flight = kept;
            }
        }
    }

    /// Drop all state for a removed cluster, deregistering any in-flight probes.
    pub fn remove_cluster(&mut self, cluster_id: &str, registry: &Registry) {
        self.set_cluster(cluster_id, None, registry);
    }

    /// Whether `token` falls in the reserved UDP-health namespace.
    pub fn owns_token(&self, token: Token) -> bool {
        token.0 >= UDP_HEALTH_TOKEN_BASE
            && token.0 < UDP_HEALTH_TOKEN_BASE + UDP_HEALTH_TOKEN_CAPACITY
    }

    /// Record mio readiness for an owned probe socket.
    pub fn ready(&mut self, token: Token) {
        self.ready_tokens.insert(token);
    }

    /// Pick the next free probe token offset, skipping in-flight slots.
    fn allocate_token(&mut self) -> Option<Token> {
        let in_flight: HashSet<usize> = self
            .in_flight
            .iter()
            .map(|p| p.token.0 - UDP_HEALTH_TOKEN_BASE)
            .collect();
        for _ in 0..UDP_HEALTH_TOKEN_CAPACITY {
            let offset = self.next_token_id % UDP_HEALTH_TOKEN_CAPACITY;
            self.next_token_id = self.next_token_id.wrapping_add(1);
            if !in_flight.contains(&offset) {
                let token = Token(UDP_HEALTH_TOKEN_BASE + offset);
                // A freshly allocated probe token must fall in the reserved
                // health namespace (so the proxy demuxes its readiness back to
                // the health checker) and must not collide with an in-flight
                // probe (which would alias two probes onto one socket slot).
                debug_assert!(
                    self.owns_token(token),
                    "allocate_token returned a token outside the health namespace"
                );
                debug_assert!(
                    !self.in_flight.iter().any(|p| p.token == token),
                    "allocate_token returned a token already in flight"
                );
                return Some(token);
            }
        }
        error!(
            "{} token table full ({} in-flight); refusing new probe slot",
            log_context!(),
            in_flight.len()
        );
        None
    }

    /// One event-loop step: start due probes, progress in-flight ones. Never
    /// blocks. No-op when no cluster has health configured and nothing is in
    /// flight.
    pub fn poll(&mut self, backends: &Rc<RefCell<BackendMap>>, registry: &Registry) {
        if self.settings.is_empty() && self.in_flight.is_empty() {
            return;
        }
        self.initiate(backends, registry);
        self.progress(backends, registry);
    }

    fn initiate(&mut self, backends: &Rc<RefCell<BackendMap>>, registry: &Registry) {
        let now = Instant::now();
        let backend_map = backends.borrow();

        // Collect (cluster, settings, [(backend_id, address)]) to probe.
        let mut to_probe: ProbeBatch = Vec::new();
        for (cluster_id, settings) in &self.settings {
            let due = match self.last_check.get(cluster_id) {
                Some(last) => now.duration_since(*last) >= settings.interval,
                None => true,
            };
            if !due {
                continue;
            }
            if let Some(list) = backend_map.backends.get(cluster_id) {
                let targets: Vec<(String, SocketAddr)> =
                    list.backends
                        .iter()
                        .filter(|b| {
                            let b = b.borrow();
                            !self.in_flight.iter().any(|p| {
                                p.cluster_id == *cluster_id && p.backend_id == b.backend_id
                            })
                        })
                        .map(|b| {
                            let b = b.borrow();
                            (b.backend_id.to_owned(), b.address)
                        })
                        .collect();
                if !targets.is_empty() {
                    to_probe.push((cluster_id.to_owned(), settings.clone(), targets));
                }
            }
        }
        drop(backend_map);

        for (cluster_id, settings, targets) in to_probe {
            self.last_check.insert(cluster_id.to_owned(), now);
            for (backend_id, address) in targets {
                // Primary: companion TCP probe (always runs).
                self.spawn_tcp_probe(
                    backends,
                    registry,
                    &cluster_id,
                    &backend_id,
                    address,
                    &settings,
                    now,
                );
                // Secondary: app-level UDP probe, only when a payload is
                // configured. Its result feeds the SAME rise/fall hysteresis, so
                // a configured app probe that goes silent can mark a backend down
                // even while the TCP port answers, and a reply keeps it up.
                if settings.udp_probe_payload.is_some() {
                    self.spawn_udp_probe(
                        backends,
                        registry,
                        &cluster_id,
                        &backend_id,
                        address,
                        &settings,
                        now,
                    );
                }
            }
        }
    }

    /// Spawn one primary companion TCP probe toward `tcp_port` (override) or the
    /// backend data port. Failure to connect/register records a failure straight
    /// away (no in-flight slot).
    #[allow(clippy::too_many_arguments)]
    fn spawn_tcp_probe(
        &mut self,
        backends: &Rc<RefCell<BackendMap>>,
        registry: &Registry,
        cluster_id: &str,
        backend_id: &str,
        address: SocketAddr,
        settings: &UdpHealthSettings,
        now: Instant,
    ) {
        let probe_addr = match settings.tcp_port {
            Some(port) => SocketAddr::new(address.ip(), port),
            None => address,
        };
        let record_failure = || {
            Self::record(
                backends,
                cluster_id,
                backend_id,
                address,
                false,
                settings.rise,
                settings.fall,
            )
        };
        let mut stream = match TcpStream::connect(probe_addr) {
            Ok(stream) => stream,
            Err(_) => return record_failure(),
        };
        let Some(token) = self.allocate_token() else {
            return record_failure();
        };
        if registry
            .register(&mut stream, token, Interest::WRITABLE)
            .is_err()
        {
            return record_failure();
        }
        self.in_flight.push(InFlightProbe {
            socket: ProbeSocket::Tcp(stream),
            token,
            cluster_id: cluster_id.to_owned(),
            backend_id: backend_id.to_owned(),
            address,
            started_at: now,
            timeout: settings.timeout,
            rise: settings.rise,
            fall: settings.fall,
        });
    }

    /// Spawn one secondary app-level UDP probe: a connected non-blocking UDP
    /// socket to the backend data port, the configured payload sent once, and
    /// `Interest::READABLE` armed so any reply wakes [`Self::progress`]. A send
    /// or socket-setup error records a failure immediately; silence is failed by
    /// the timeout in `progress`.
    #[allow(clippy::too_many_arguments)]
    fn spawn_udp_probe(
        &mut self,
        backends: &Rc<RefCell<BackendMap>>,
        registry: &Registry,
        cluster_id: &str,
        backend_id: &str,
        address: SocketAddr,
        settings: &UdpHealthSettings,
        now: Instant,
    ) {
        let Some(payload) = settings.udp_probe_payload.as_deref() else {
            return;
        };
        let record_failure = || {
            Self::record(
                backends,
                cluster_id,
                backend_id,
                address,
                false,
                settings.rise,
                settings.fall,
            )
        };
        // The app probe always targets the backend data port (the UDP service),
        // never `tcp_port` (that is the companion-TCP override only).
        let mut socket = match udp_connect(address) {
            Ok(socket) => socket,
            Err(_) => return record_failure(),
        };
        // Best-effort send of the probe payload. `WouldBlock` is treated as a
        // soft failure to send (rare on a fresh connected socket); a hard error
        // (e.g. immediate ECONNREFUSED) is a failure signal.
        match socket.send(payload) {
            Ok(_) => {}
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => return record_failure(),
            Err(_) => return record_failure(),
        }
        let Some(token) = self.allocate_token() else {
            return record_failure();
        };
        if registry
            .register(&mut socket, token, Interest::READABLE)
            .is_err()
        {
            return record_failure();
        }
        self.in_flight.push(InFlightProbe {
            socket: ProbeSocket::Udp(socket),
            token,
            cluster_id: cluster_id.to_owned(),
            backend_id: backend_id.to_owned(),
            address,
            started_at: now,
            timeout: settings.timeout,
            rise: settings.rise,
            fall: settings.fall,
        });
    }

    fn progress(&mut self, backends: &Rc<RefCell<BackendMap>>, registry: &Registry) {
        let now = Instant::now();
        let ready = std::mem::take(&mut self.ready_tokens);
        let mut completed: Vec<(usize, bool)> = Vec::new();

        for (idx, probe) in self.in_flight.iter_mut().enumerate() {
            if now.duration_since(probe.started_at) > probe.timeout {
                completed.push((idx, false));
                continue;
            }
            if !ready.contains(&probe.token) {
                continue;
            }
            let success = match &mut probe.socket {
                // A writable, error-free connected TCP socket = probe success. A
                // refused/aborted connect surfaces either as a non-`None`
                // `SO_ERROR` (via `take_error`) or as a `peer_addr` failure (the
                // 3-way handshake never completed), so require both to be clean.
                ProbeSocket::Tcp(stream) => {
                    let no_so_error = matches!(stream.take_error(), Ok(None));
                    no_so_error && stream.peer_addr().is_ok()
                }
                // ANY readable reply on the connected UDP probe socket within the
                // timeout is success; a `WouldBlock` here means the readiness was
                // spurious (no datagram yet) — leave it in flight to either read
                // a reply on a later edge or time out.
                ProbeSocket::Udp(socket) => {
                    let mut scratch = [0u8; 16];
                    match socket.recv(&mut scratch) {
                        Ok(_) => true,
                        Err(ref e) if e.kind() == ErrorKind::WouldBlock => continue,
                        // A connected UDP socket surfaces a prior ICMP
                        // port-unreachable as ECONNREFUSED on recv → failure.
                        Err(_) => false,
                    }
                }
            };
            completed.push((idx, success));
        }

        completed.sort_by(|a, b| b.0.cmp(&a.0));
        for (idx, success) in completed {
            let mut probe = self.in_flight.swap_remove(idx);
            probe.socket.deregister(registry);
            Self::record(
                backends,
                &probe.cluster_id,
                &probe.backend_id,
                probe.address,
                success,
                probe.rise,
                probe.fall,
            );
        }
    }

    /// Apply a probe result to the backend's `HealthState` with rise/fall
    /// hysteresis, log + count transitions, and re-evaluate cluster
    /// availability so fail-open / recovery transitions surface.
    fn record(
        backends: &Rc<RefCell<BackendMap>>,
        cluster_id: &str,
        backend_id: &str,
        address: SocketAddr,
        success: bool,
        rise: u32,
        fall: u32,
    ) {
        let mut backend_map = backends.borrow_mut();
        let Some(list) = backend_map.backends.get_mut(cluster_id) else {
            return;
        };
        let Some(backend_ref) = list.find_backend(&address) else {
            return;
        };
        let mut backend = backend_ref.borrow_mut();
        if success {
            if backend.health.record_success(rise) {
                info!(
                    "{} backend {} at {} marked UP (cluster {})",
                    log_context!(),
                    backend_id,
                    address,
                    cluster_id
                );
                incr!(names::udp::BACKEND_HEALTH);
            }
        } else if backend.health.record_failure(fall) {
            warn!(
                "{} backend {} at {} marked DOWN (cluster {})",
                log_context!(),
                backend_id,
                address,
                cluster_id
            );
            incr!(names::udp::BACKEND_HEALTH);
        }
        drop(backend);
        backend_map.record_cluster_availability(cluster_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::HealthState;

    #[test]
    fn hysteresis_rise_fall() {
        // rise=2, fall=3: a single failure does not flip a healthy backend, and
        // a single success does not flip an unhealthy one.
        let mut state = HealthState::default();
        assert!(state.is_healthy());
        assert!(!state.record_failure(3));
        assert!(!state.record_failure(3));
        assert!(state.is_healthy());
        assert!(state.record_failure(3));
        assert!(!state.is_healthy());
        assert!(!state.record_success(2));
        assert!(state.record_success(2));
        assert!(state.is_healthy());
    }

    #[test]
    fn token_namespace_is_disjoint_and_owned() {
        let hc = UdpHealthChecker::new();
        assert!(hc.owns_token(Token(UDP_HEALTH_TOKEN_BASE)));
        assert!(hc.owns_token(Token(UDP_HEALTH_TOKEN_BASE + UDP_HEALTH_TOKEN_CAPACITY - 1)));
        assert!(!hc.owns_token(Token(UDP_HEALTH_TOKEN_BASE - 1)));
        assert!(!hc.owns_token(Token(UDP_HEALTH_TOKEN_BASE + UDP_HEALTH_TOKEN_CAPACITY)));
        // Disjoint from the HTTP HealthChecker base (1 << 24).
        assert!(!hc.owns_token(Token(1 << 24)));
    }

    #[test]
    fn settings_from_proto_defaults() {
        let cfg = UdpHealthConfig {
            mode: None,
            tcp_port: Some(5353),
            rise: None,
            fall: None,
            fail_open: None,
            udp_probe_payload: None,
            probe_interval_seconds: None,
            probe_timeout_seconds: None,
        };
        let s = UdpHealthSettings::from_proto(&cfg);
        assert_eq!(s.tcp_port, Some(5353));
        assert_eq!(s.rise, 2);
        assert_eq!(s.fall, 3);
        assert_eq!(s.interval, Duration::from_secs(5));
        assert_eq!(s.timeout, Duration::from_secs(2));
    }

    #[test]
    fn udp_probe_payload_is_captured() {
        // The secondary app-level probe is gated on `udp_probe_payload` being
        // present; `from_proto` must surface it so `initiate` knows to spawn the
        // UDP probe alongside the TCP one.
        let cfg = UdpHealthConfig {
            mode: Some(sozu_command::proto::command::UdpHealthMode::UdpProbe as i32),
            tcp_port: None,
            rise: Some(1),
            fall: Some(1),
            fail_open: None,
            udp_probe_payload: Some(b"PING".to_vec()),
            probe_interval_seconds: Some(1),
            probe_timeout_seconds: Some(1),
        };
        let s = UdpHealthSettings::from_proto(&cfg);
        assert_eq!(s.udp_probe_payload.as_deref(), Some(&b"PING"[..]));
        assert_eq!(s.tcp_port, None);
    }

    /// Drive the **secondary UDP-probe** result through the SAME `record` path
    /// (and thus the same rise/fall hysteresis) the TCP probe uses, proving a
    /// silent app probe flips a backend DOWN and a reply flips it back UP. No
    /// real I/O: we call `record` with the success/failure a UDP probe would
    /// have produced.
    #[test]
    fn udp_probe_result_feeds_same_hysteresis() {
        use crate::backends::{Backend, BackendMap};

        let cluster = "dns";
        let address: SocketAddr = ([127, 0, 0, 1], 5353).into();
        let backend_map = Rc::new(RefCell::new(BackendMap::new()));
        backend_map
            .borrow_mut()
            .add_backend(cluster, Backend::new("b1", address, None, None, None));
        let (rise, fall) = (2u32, 3u32);

        let is_healthy = |map: &Rc<RefCell<BackendMap>>| {
            let mut m = map.borrow_mut();
            let list = m.backends.get_mut(cluster).unwrap();
            let b = list.find_backend(&address).unwrap();
            b.borrow().health.is_healthy()
        };
        assert!(is_healthy(&backend_map));

        // Secondary UDP probe goes silent: `fall` failures flip the backend DOWN.
        for _ in 0..fall {
            UdpHealthChecker::record(&backend_map, cluster, "b1", address, false, rise, fall);
        }
        assert!(!is_healthy(&backend_map));

        // A reply arrives: `rise` successes flip it back UP (hysteresis holds —
        // one success is not enough).
        UdpHealthChecker::record(&backend_map, cluster, "b1", address, true, rise, fall);
        assert!(!is_healthy(&backend_map));
        UdpHealthChecker::record(&backend_map, cluster, "b1", address, true, rise, fall);
        assert!(is_healthy(&backend_map));
    }
}
