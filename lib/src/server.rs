//! event loop management
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque, hash_map::Entry},
    hash::{DefaultHasher, Hash, Hasher},
    io::Error as IoError,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    os::unix::io::{AsRawFd, FromRawFd},
    rc::Rc,
    sync::LazyLock,
    time::{Duration, Instant},
};

use mio::{
    Events, Interest, Poll, Token,
    net::{TcpListener as MioTcpListener, TcpStream},
};
use slab::Slab;
use sozu_command::{
    channel::Channel,
    logging,
    proto::command::{
        ActivateListener, AddBackend, CertificatesWithFingerprints, Cluster, ClusterHashes,
        ClusterInformations, DeactivateListener, Event, HttpListenerConfig, HttpsListenerConfig,
        InitialState, ListenerType, LoadBalancingAlgorithms, LoadMetric, MetricsConfiguration,
        RemoveBackend, Request, ResponseStatus, ServerConfig,
        TcpListenerConfig as CommandTcpListener, UpdateHttpListenerConfig,
        UpdateHttpsListenerConfig, UpdateTcpListenerConfig, WorkerRequest, WorkerResponse,
        request::RequestType, response_content::ContentType,
    },
    ready::Ready,
    scm_socket::{Listeners, ScmSocket, ScmSocketError},
    state::ConfigState,
};

use crate::{
    AcceptError, Protocol, ProxyConfiguration, ProxySession, SessionIsToBeClosed,
    backends::{Backend, BackendMap},
    features::FEATURES,
    health_check::HealthChecker,
    http, https,
    metrics::METRICS,
    pool::Pool,
    tcp,
    timer::Timer,
};

// Number of retries to perform on a server after a connection failure
pub const CONN_RETRIES: u8 = 3;

/// Number of bounded buckets for the per-source connect-rate counter.
///
/// `incr!` requires a `&'static str`, so per-IP labelling would either need
/// runtime `Box::leak` per unique source (unbounded under SYN flood — direct
/// OWASP A05 / NIST SP 800-92 cardinality-blow-up risk) or a fixed bucket
/// table. We pick the bucket table: 256 static labels precomputed at startup,
/// each masked subnet hashes into one of them.
///
/// Bucket-noise vs per-IP fidelity is a deliberate trade. Operators wanting
/// per-IP attribution should pair these counters with structured access logs
/// or a downstream rate-limiter; the metric here is for "is some /24 spamming
/// us right now?", not "which IP exactly". 256 buckets keep the memory + UDP
/// statsd cost flat regardless of attacker effort.
pub const PER_SOURCE_BUCKETS: usize = 256;

/// Pre-leaked `&'static str` table for per-source bucket counters.
/// `incr!` requires `&'static str`; we leak once at first access (LazyLock)
/// for `PER_SOURCE_BUCKETS` keys, totalling ~10 KB heap. The leak is bounded
/// by `PER_SOURCE_BUCKETS` and never grows with traffic.
static PER_SOURCE_BUCKET_KEYS: LazyLock<[&'static str; PER_SOURCE_BUCKETS]> = LazyLock::new(|| {
    let mut keys: [&'static str; PER_SOURCE_BUCKETS] = [""; PER_SOURCE_BUCKETS];
    for (i, slot) in keys.iter_mut().enumerate() {
        // e.g. "client.connect.per_source.bucket_042"
        let owned = format!("client.connect.per_source.bucket_{i:03}");
        *slot = Box::leak(owned.into_boxed_str());
    }
    keys
});

/// Mask an IP address to its bounded prefix (/24 for IPv4, /48 for IPv6) and
/// hash it into one of `PER_SOURCE_BUCKETS` slots. The hash is `DefaultHasher`,
/// which is deterministic within a process but salted across runs — fine for
/// telemetry, not suitable for cross-host correlation.
fn per_source_bucket(peer: &SocketAddr) -> &'static str {
    let mut hasher = DefaultHasher::new();
    match peer.ip() {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            // /24 mask: keep first three octets, zero the host portion.
            let masked = Ipv4Addr::new(octets[0], octets[1], octets[2], 0);
            masked.hash(&mut hasher);
        }
        IpAddr::V6(v6) => {
            let octets = v6.octets();
            // /48 mask: keep first 6 bytes, zero the rest.
            let mut masked_octets = [0u8; 16];
            masked_octets[..6].copy_from_slice(&octets[..6]);
            Ipv6Addr::from(masked_octets).hash(&mut hasher);
        }
    }
    let idx = (hasher.finish() as usize) % PER_SOURCE_BUCKETS;
    PER_SOURCE_BUCKET_KEYS[idx]
}

/// Period between two `accept_queue.saturated_seconds` ticks. The counter is
/// incremented once per period while [`SessionManager::can_accept`] is `false`,
/// distinguishing "queue spent N seconds at max" from "queue briefly hit max"
/// — the binary `accept_queue.backpressure` gauge collapses that duration.
const ACCEPT_SATURATION_TICK: Duration = Duration::from_secs(1);

pub type ProxyChannel = Channel<WorkerResponse, WorkerRequest>;

thread_local! {
  pub static QUEUE: RefCell<VecDeque<WorkerResponse>> = const { RefCell::new(VecDeque::new()) };
}

thread_local! {
  pub static TIMER: RefCell<Timer<Token>> = RefCell::new(Timer::default());
}

pub fn push_queue(message: WorkerResponse) {
    QUEUE.with(|queue| {
        (*queue.borrow_mut()).push_back(message);
    });
}

pub fn push_event(event: Event) {
    QUEUE.with(|queue| {
        (*queue.borrow_mut()).push_back(WorkerResponse {
            id: "EVENT".to_string(),
            message: String::new(),
            status: ResponseStatus::Processing.into(),
            content: Some(ContentType::Event(event).into()),
        });
    });
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ListenToken(pub usize);
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionToken(pub usize);

impl From<usize> for ListenToken {
    fn from(val: usize) -> ListenToken {
        ListenToken(val)
    }
}

impl From<ListenToken> for usize {
    fn from(val: ListenToken) -> usize {
        val.0
    }
}

impl From<usize> for SessionToken {
    fn from(val: usize) -> SessionToken {
        SessionToken(val)
    }
}

impl From<SessionToken> for usize {
    fn from(val: SessionToken) -> usize {
        val.0
    }
}

pub struct SessionManager {
    pub max_connections: usize,
    pub nb_connections: usize,
    pub can_accept: bool,
    pub slab: Slab<Rc<RefCell<dyn ProxySession>>>,
    /// Default per-(cluster, source-IP) connection limit. `0` disables
    /// the feature; cluster-level overrides take precedence at check
    /// time.
    pub max_connections_per_ip: u64,
    /// Default `Retry-After` header value (seconds) for HTTP 429
    /// responses emitted on per-(cluster, source-IP) limit hit. `0`
    /// omits the header.
    pub retry_after: u32,
    /// Active **frontend connections** per `(cluster_id, source_ip)`.
    /// Each frontend session contributes AT MOST 1 to the count for any
    /// given `(cluster, ip)` pair, regardless of how many streams it
    /// multiplexes to that cluster (an H2 connection serving 100
    /// streams to cluster X from IP 1.2.3.4 still counts as 1). The
    /// counter is incremented the first time a session's
    /// `Router::connect` resolves to a fresh `(cluster, ip)` pair, and
    /// decremented when the session closes. Empty when the feature is
    /// unused.
    ///
    /// ── Why nested maps instead of `HashMap<(String, IpAddr), usize>` ──
    ///
    /// The per-request hot path (`cluster_ip_at_limit`, called from
    /// `mux/router::connect` for every cluster-resolving request) used
    /// to allocate a `String` to build the compound key on every
    /// lookup. Splitting the storage so the outer key is `String`
    /// lets the lookup take `&str` — `HashMap::get(cluster_id)` on a
    /// `HashMap<String, _>` accepts a borrow via the `Borrow<str>`
    /// impl. The hot path no longer allocates; the only `String` clone
    /// is in `track_cluster_ip`, which runs at most once per
    /// `(cluster, ip)` pair per session. Memory footprint is unchanged
    /// in steady state — entries are still reaped to zero on session
    /// close.
    connections_per_cluster_ip: HashMap<String, HashMap<IpAddr, usize>>,
    /// Reverse index: per-token map of `cluster_id` → set of source IPs
    /// already counted against `connections_per_cluster_ip`. Used to
    /// make `track_cluster_ip` idempotent within a session (so H2
    /// streams to the same cluster from the same client only consume
    /// one slot in the limit) and to drain a session's contributions on
    /// close. Same nesting rationale as above.
    cluster_ip_tracks: HashMap<Token, HashMap<String, HashSet<IpAddr>>>,
}

impl SessionManager {
    pub fn new(
        slab: Slab<Rc<RefCell<dyn ProxySession>>>,
        max_connections: usize,
        max_connections_per_ip: u64,
        retry_after: u32,
    ) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(SessionManager {
            max_connections,
            nb_connections: 0,
            can_accept: true,
            slab,
            max_connections_per_ip,
            retry_after,
            connections_per_cluster_ip: HashMap::new(),
            cluster_ip_tracks: HashMap::new(),
        }))
    }

    /// Resolve the effective per-(cluster, source-IP) limit. `override_value`
    /// is the cluster-level setting from the proto `Cluster` message:
    /// `None` inherits the global default, `Some(0)` is explicit
    /// "unlimited", `Some(n > 0)` overrides.
    pub fn effective_max_connections_per_ip(&self, override_value: Option<u64>) -> u64 {
        override_value.unwrap_or(self.max_connections_per_ip)
    }

    /// Resolve the effective `Retry-After` header value. `Some(0)` (or
    /// the global default of 0) signals "omit the header" — caller
    /// must skip emission rather than render `Retry-After: 0`.
    pub fn effective_retry_after(&self, override_value: Option<u32>) -> u32 {
        override_value.unwrap_or(self.retry_after)
    }

    /// Returns `true` when admitting `token` to one more connection for
    /// `(cluster, ip)` would exceed the resolved limit. `0` is treated
    /// as unlimited. A token that already holds a slot for this
    /// `(cluster, ip)` is NEVER at the limit — H2 sessions multiplex
    /// many streams to the same cluster on a single connection, and
    /// the limit governs distinct frontend connections, not streams.
    ///
    /// Hot-path: called for every cluster-resolving request from
    /// `mux/router::connect`. The nested-map storage lets both lookups
    /// borrow `cluster_id` and `ip`; no per-call allocation runs here
    /// in steady state.
    pub fn cluster_ip_at_limit(
        &self,
        token: Token,
        cluster_id: &str,
        ip: &IpAddr,
        override_value: Option<u64>,
    ) -> bool {
        let limit = self.effective_max_connections_per_ip(override_value);
        if limit == 0 {
            return false;
        }
        if self
            .cluster_ip_tracks
            .get(&token)
            .and_then(|by_cluster| by_cluster.get(cluster_id))
            .is_some_and(|ips| ips.contains(ip))
        {
            return false;
        }
        self.connections_per_cluster_ip
            .get(cluster_id)
            .and_then(|by_ip| by_ip.get(ip))
            .is_some_and(|c| (*c as u64) >= limit)
    }

    /// Account `token`'s active connection against `(cluster, ip)`.
    /// Idempotent within a token: a second call for the same
    /// `(cluster, ip)` is a no-op so H2 retries / multi-stream opens
    /// to the same cluster do not double-count.
    ///
    /// Allocates a single owned `String` per `(token, cluster)` pair on
    /// first observation — `entry(cluster_id.clone())` materialises a
    /// new outer-map slot. Subsequent IPs under the same `(token,
    /// cluster)` reuse the existing slot.
    pub fn track_cluster_ip(&mut self, token: Token, cluster_id: String, ip: IpAddr) {
        let inserted = self
            .cluster_ip_tracks
            .entry(token)
            .or_default()
            .entry(cluster_id.clone())
            .or_default()
            .insert(ip);
        if inserted {
            *self
                .connections_per_cluster_ip
                .entry(cluster_id)
                .or_default()
                .entry(ip)
                .or_insert(0) += 1;
        }
    }

    /// Drain every `(cluster, ip)` slot held by `token` and apply the
    /// matching decrements. Called on session teardown only — there is
    /// no per-stream untrack because the limit is per-connection, not
    /// per-stream. Removes empty inner maps so the outer
    /// `connections_per_cluster_ip` does not retain `(cluster_id,
    /// empty_map)` orphans across cluster lifetimes.
    pub fn untrack_all_cluster_ip(&mut self, token: Token) {
        let Some(by_cluster) = self.cluster_ip_tracks.remove(&token) else {
            return;
        };
        for (cluster_id, ips) in by_cluster {
            let Entry::Occupied(mut outer) = self.connections_per_cluster_ip.entry(cluster_id)
            else {
                continue;
            };
            for ip in ips {
                if let Entry::Occupied(mut inner) = outer.get_mut().entry(ip) {
                    let count = inner.get_mut();
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        inner.remove();
                    }
                }
            }
            if outer.get().is_empty() {
                outer.remove();
            }
        }
    }

    /// Wipe every per-(cluster, source-IP) accounting bucket. Called by
    /// the runtime `SetMaxConnectionsPerIp(0)` path so disabling the
    /// feature does not leave dead bookkeeping behind that a future
    /// re-enable would consult.
    pub fn clear_cluster_ip_tracking(&mut self) {
        self.cluster_ip_tracks.clear();
        self.connections_per_cluster_ip.clear();
    }

    /// The slab is considered at capacity if it contains more sessions than twice max_connections
    pub fn at_capacity(&self) -> bool {
        self.slab.len() >= self.accept_slab_threshold()
    }

    /// The slab fill level at which `at_capacity` flips to true and the
    /// accept queue is flushed. Reported as `slab.accept_threshold` so the
    /// per-iteration `slab.accept_threshold_percent` gauge in the run loop
    /// can chart proximity to this gate, distinct from raw slab usage.
    ///
    /// The constant `10 + 2 * max_connections` is the historical pre-knob
    /// budget; configured slab capacity is
    /// `10 + slab_entries_per_connection * max_connections` (see
    /// `command/src/config.rs`) and can be larger, so `slab.usage_percent`
    /// (against `slab.capacity()`) and `slab.accept_threshold_percent`
    /// (against this gate) are emitted as independent gauges.
    pub fn accept_slab_threshold(&self) -> usize {
        10 + 2 * self.max_connections
    }

    /// Check the number of connections against max_connections, and the slab capacity.
    /// Returns false if limits are reached.
    pub fn check_limits(&mut self) -> bool {
        if self.nb_connections >= self.max_connections {
            error!("max number of session connection reached, flushing the accept queue");
            gauge!("accept_queue.backpressure", 1);
            self.can_accept = false;
            return false;
        }

        if self.at_capacity() {
            error!("not enough memory to accept another session, flushing the accept queue");
            error!(
                "nb_connections: {}, max_connections: {}",
                self.nb_connections, self.max_connections
            );
            gauge!("accept_queue.backpressure", 1);
            self.can_accept = false;

            return false;
        }

        true
    }

    pub fn to_session(token: Token) -> SessionToken {
        SessionToken(token.0)
    }

    pub fn incr(&mut self) {
        self.nb_connections += 1;
        assert!(self.nb_connections <= self.max_connections);
        // `client.connections_max` and `client.connections_percent` are
        // emitted from the run loop alongside `process.uptime_seconds` /
        // `server.live` so all proxy gauges advance in lock-step. Keeping
        // `client.connections` per-event preserves the high-resolution
        // signal scrapers expect.
        gauge!("client.connections", self.nb_connections);
    }

    /// Decrements the number of sessions, start accepting new connections
    /// if the capacity limit of 90% has not been reached.
    pub fn decr(&mut self) {
        assert!(self.nb_connections != 0);
        self.nb_connections -= 1;
        gauge!("client.connections", self.nb_connections);

        // do not be ready to accept right away, wait until we get back to 10% capacity
        if !self.can_accept && self.nb_connections < self.max_connections * 90 / 100 {
            debug!(
                "nb_connections = {}, max_connections = {}, starting to accept again",
                self.nb_connections, self.max_connections
            );
            gauge!("accept_queue.backpressure", 0);
            self.can_accept = true;
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum ServerError {
    #[error("could not create event loop with MIO poll: {0}")]
    CreatePoll(IoError),
    #[error("could not clone the MIO registry: {0}")]
    CloneRegistry(IoError),
    #[error("could not register the channel: {0}")]
    RegisterChannel(IoError),
    #[error("{msg}:{scm_err}")]
    ScmSocket {
        msg: String,
        scm_err: ScmSocketError,
    },
}

/// `Server` handles the event loop, the listeners, the sessions and
/// communication with the configuration channel.
///
/// A listener wraps a listen socket, the associated proxying protocols
/// (HTTP, HTTPS and TCP) and the routing configuration for clusters.
/// Listeners handle creating sessions from accepted sockets.
///
/// A session manages a "front" socket for a connected client, and all
/// of the associated data (back socket, protocol state machine, buffers,
/// metrics...).
///
/// `Server` gets configuration updates from the channel (domIN/path routes,
/// backend server address...).
///
/// Listeners and sessions are all stored in a slab structure to index them
/// by a [Token], they all have to implement the [ProxySession] trait.
pub struct Server {
    accept_queue_timeout: Duration,
    /// Tuple layout: `(socket, listen token, protocol, accept time, peer
    /// address)`. The peer is captured via `TcpStream::peer_addr()` at accept
    /// time so the `client.connect.per_source.*` counter can be attributed
    /// without the socket having to be alive at session-creation time. The
    /// peer is `Option` because `peer_addr()` is best-effort: a peer that
    /// races to close before we read it is rare but possible.
    accept_queue: VecDeque<(
        TcpStream,
        ListenToken,
        Protocol,
        Instant,
        Option<SocketAddr>,
    )>,
    /// When the accept queue saturates and `check_limits` refuses, evict the
    /// oldest non-listener sessions to make room. Default off — see
    /// `command::config::DEFAULT_EVICT_ON_QUEUE_FULL` for the rationale.
    evict_on_queue_full: bool,
    accept_ready: HashSet<ListenToken>,
    backends: Rc<RefCell<BackendMap>>,
    base_sessions_count: usize,
    channel: ProxyChannel,
    config_state: ConfigState,
    current_poll_errors: i32,
    health_checker: HealthChecker,
    http: Rc<RefCell<http::HttpProxy>>,
    https: Rc<RefCell<https::HttpsProxy>>,
    last_sessions_len: usize,
    last_shutting_down_message: Option<Instant>,
    last_zombie_check: Instant,
    loop_start: Instant,
    /// Wall-clock anchor for the `process.uptime_seconds` gauge. Captured once
    /// in [`Server::new`]; never reset on hot upgrades (the new worker that
    /// inherits FDs is a fresh process and starts its own counter).
    started_at: Instant,
    /// Last time the 1Hz `accept_queue.saturated_seconds` ticker fired. The
    /// counter is incremented once per [`ACCEPT_SATURATION_TICK`] while
    /// `SessionManager::can_accept` is `false`, so dashboards can plot the
    /// time spent saturated rather than just whether saturation occurred.
    last_saturation_tick: Instant,
    max_poll_errors: i32, // TODO: make this configurable? this defaults to 10000 for now
    /// Shared reference to the buffer pool the protocol stacks check buffers
    /// in and out of. Held here so the run loop can sample
    /// `buffer.in_use` / `buffer.capacity` / `buffer.usage_percent` once per
    /// iteration without requiring each protocol module to expose its own
    /// snapshot.
    pool: Rc<RefCell<Pool>>,
    pub poll: Poll,
    poll_timeout: Option<Duration>, // TODO: make this configurable? this defaults to 1000 milliseconds for now
    scm_listeners: Option<Listeners>,
    scm: ScmSocket,
    sessions: Rc<RefCell<SessionManager>>,
    should_poll_at: Option<Instant>,
    shutting_down: Option<String>,
    tcp: Rc<RefCell<tcp::TcpProxy>>,
    zombie_check_interval: Duration,
}

impl Server {
    pub fn try_new_from_config(
        worker_to_main_channel: ProxyChannel,
        worker_to_main_scm: ScmSocket,
        config: ServerConfig,
        initial_state: InitialState,
        expects_initial_status: bool,
    ) -> Result<Self, ServerError> {
        let event_loop = Poll::new().map_err(ServerError::CreatePoll)?;
        // Commit the operator-configured Basic-auth credential cap (or
        // keep the built-in default) once per worker process. The
        // `OnceLock` rejects any later attempt to change the value, so
        // calling here — before any L7 listener has accepted a request
        // — guarantees the cap is in force the first time `mux::auth`
        // runs.
        if let Some(cap) = config.basic_auth_max_credential_bytes {
            crate::protocol::mux::auth::set_max_decoded_credential_bytes(cap as usize);
        }
        // Same set-once-per-worker-boot pattern for the splice kernel-pipe
        // capacity. The setter no-ops on `0` so an explicit zero in config
        // does not collapse the pipe to PAGE_SIZE; the kernel still applies
        // page-rounding and `/proc/sys/fs/pipe-max-size` clamping at
        // SplicePipe::new time. Cfg-gated because the splice module only
        // exists on Linux + `splice` feature.
        #[cfg(all(target_os = "linux", feature = "splice"))]
        if let Some(cap) = config.splice_pipe_capacity_bytes {
            crate::splice::set_pipe_capacity(cap as usize);
        }
        let pool = Rc::new(RefCell::new(Pool::with_capacity(
            config.min_buffers as usize,
            config.max_buffers as usize,
            config.buffer_size as usize,
        )));
        let backends = Rc::new(RefCell::new(BackendMap::new()));

        // Note: slab_capacity uses 4x multiplier (up from 2x) to account for H2
        // multiplexing where each session can have multiple backend connections.
        // Newer `optional` proto fields fall through to the
        // command-lib defaults when an older worker manager omits them.
        let sessions: Rc<RefCell<SessionManager>> = SessionManager::new(
            Slab::with_capacity(config.slab_capacity() as usize),
            config.max_connections as usize,
            config
                .max_connections_per_ip
                .unwrap_or(sozu_command::config::DEFAULT_MAX_CONNECTIONS_PER_IP),
            config
                .retry_after
                .unwrap_or(sozu_command::config::DEFAULT_RETRY_AFTER),
        );
        {
            let mut s = sessions.borrow_mut();
            let entry = s.slab.vacant_entry();
            trace!("taking token {:?} for channel", SessionToken(entry.key()));
            entry.insert(Rc::new(RefCell::new(ListenSession {
                protocol: Protocol::Channel,
            })));
        }
        {
            let mut s = sessions.borrow_mut();
            let entry = s.slab.vacant_entry();
            trace!("taking token {:?} for metrics", SessionToken(entry.key()));
            entry.insert(Rc::new(RefCell::new(ListenSession {
                protocol: Protocol::Timer,
            })));
        }
        {
            let mut s = sessions.borrow_mut();
            let entry = s.slab.vacant_entry();
            trace!("taking token {:?} for metrics", SessionToken(entry.key()));
            entry.insert(Rc::new(RefCell::new(ListenSession {
                protocol: Protocol::Metrics,
            })));
        }

        Server::new(
            event_loop,
            worker_to_main_channel,
            worker_to_main_scm,
            sessions,
            pool,
            backends,
            None,
            None,
            None,
            config,
            Some(initial_state),
            expects_initial_status,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        poll: Poll,
        mut channel: ProxyChannel,
        scm: ScmSocket,
        sessions: Rc<RefCell<SessionManager>>,
        pool: Rc<RefCell<Pool>>,
        backends: Rc<RefCell<BackendMap>>,
        http: Option<http::HttpProxy>,
        https: Option<https::HttpsProxy>,
        tcp: Option<tcp::TcpProxy>,
        server_config: ServerConfig,
        initial_state: Option<InitialState>,
        expects_initial_status: bool,
    ) -> Result<Self, ServerError> {
        FEATURES.with(|_features| {
            // initializing feature flags
        });

        poll.registry()
            .register(
                &mut channel,
                Token(0),
                Interest::READABLE | Interest::WRITABLE,
            )
            .map_err(ServerError::RegisterChannel)?;

        METRICS.with(|metrics| {
            if let Some(sock) = (*metrics.borrow_mut()).socket_mut() {
                poll.registry()
                    .register(sock, Token(2), Interest::WRITABLE)
                    .expect("should register the metrics socket");
            }
        });

        let base_sessions_count = sessions.borrow().slab.len();

        let http = Rc::new(RefCell::new(match http {
            Some(http) => http,
            None => {
                let registry = poll
                    .registry()
                    .try_clone()
                    .map_err(ServerError::CloneRegistry)?;

                http::HttpProxy::new(registry, sessions.clone(), pool.clone(), backends.clone())
            }
        }));

        let https = Rc::new(RefCell::new(match https {
            Some(https) => https,
            None => {
                let registry = poll
                    .registry()
                    .try_clone()
                    .map_err(ServerError::CloneRegistry)?;

                https::HttpsProxy::new(registry, sessions.clone(), pool.clone(), backends.clone())
            }
        }));

        let tcp = Rc::new(RefCell::new(match tcp {
            Some(tcp) => tcp,
            None => {
                let registry = poll
                    .registry()
                    .try_clone()
                    .map_err(ServerError::CloneRegistry)?;

                tcp::TcpProxy::new(registry, sessions.clone(), pool.clone(), backends.clone())
            }
        }));

        let mut server = Server {
            accept_queue_timeout: Duration::from_secs(u64::from(
                server_config.accept_queue_timeout,
            )),
            accept_queue: VecDeque::new(),
            evict_on_queue_full: server_config.evict_on_queue_full.unwrap_or(false),
            accept_ready: HashSet::new(),
            backends,
            base_sessions_count,
            channel,
            config_state: ConfigState::new(),
            current_poll_errors: 0,
            health_checker: HealthChecker::new(),
            http,
            https,
            last_sessions_len: 0, // to be reset on server run
            last_shutting_down_message: None,
            last_zombie_check: Instant::now(), // to be reset on server run
            loop_start: Instant::now(),        // to be reset on server run
            started_at: Instant::now(),        // captured once, never reset
            last_saturation_tick: Instant::now(), // 1Hz saturation ticker anchor
            max_poll_errors: 10000,            // TODO: make it configurable?
            pool,
            poll_timeout: Some(Duration::from_millis(1000)), // TODO: make it configurable?
            poll,
            scm_listeners: None,
            scm,
            sessions,
            should_poll_at: None,
            shutting_down: None,
            tcp,
            zombie_check_interval: Duration::from_secs(u64::from(
                server_config.zombie_check_interval,
            )),
        };

        // initialize the worker with the state we got from a file
        if let Some(state) = initial_state {
            for request in state.requests {
                trace!("generating initial config request: {:#?}", request);
                server.notify_proxys(request);
            }

            // do not send back answers to the initialization messages
            QUEUE.with(|queue| {
                (*queue.borrow_mut()).clear();
            });
        }

        if expects_initial_status {
            // the main process sends a Status message, so we can notify it
            // when the initial state is loaded
            server.block_channel();
            let msg = server.channel.read_message();
            debug!("got message: {:?}", msg);

            if let Ok(WorkerRequest {
                id,
                content:
                    Request {
                        request_type: Some(RequestType::Status(_)),
                    },
            }) = msg
            {
                if let Err(e) = server.channel.write_message(&WorkerResponse::ok(id)) {
                    error!("Could not send an ok to the main process: {}", e);
                }
            } else {
                panic!(
                    "plz give me a status request first when I start, you sent me this instead: {msg:?}"
                );
            }
            server.unblock_channel();
        }

        info!("will try to receive listeners");
        server
            .scm
            .set_blocking(true)
            .map_err(|scm_err| ServerError::ScmSocket {
                msg: "Could not set the scm socket to blocking".to_string(),
                scm_err,
            })?;
        let listeners =
            server
                .scm
                .receive_listeners()
                .map_err(|scm_err| ServerError::ScmSocket {
                    msg: "could not receive listeners from the scm socket".to_string(),
                    scm_err,
                })?;
        server
            .scm
            .set_blocking(false)
            .map_err(|scm_err| ServerError::ScmSocket {
                msg: "Could not set the scm socket to unblocking".to_string(),
                scm_err,
            })?;
        info!("received listeners: {:?}", listeners);
        server.scm_listeners = Some(listeners);

        Ok(server)
    }

    /// The server runs in a loop until a shutdown is ordered
    pub fn run(&mut self) {
        let mut events = Events::with_capacity(1024); // TODO: make event capacity configurable?
        self.last_sessions_len = self.sessions.borrow().slab.len();

        self.last_zombie_check = Instant::now();
        self.loop_start = Instant::now();

        loop {
            self.check_for_poll_errors();

            let timeout = self.reset_loop_time_and_get_timeout();

            match self.poll.poll(&mut events, timeout) {
                Ok(_) => self.current_poll_errors = 0,
                Err(error) => {
                    error!("Error while polling events: {:?}", error);
                    self.current_poll_errors += 1;
                    continue;
                }
            }

            let after_epoll = Instant::now();
            time!("epoll_time", (after_epoll - self.loop_start).as_millis());
            self.loop_start = after_epoll;

            self.send_queue();

            for event in events.iter() {
                match event.token() {
                    // this is the command channel
                    Token(0) => {
                        if event.is_error() {
                            error!("error reading from command channel");
                            continue;
                        }
                        if event.is_read_closed() || event.is_write_closed() {
                            error!("command channel was closed");
                            return;
                        }
                        let ready = Ready::from(event);
                        self.channel.handle_events(ready);

                        // loop here because iterations has borrow issues
                        loop {
                            QUEUE.with(|queue| {
                                if !(*queue.borrow()).is_empty() {
                                    self.channel.interest.insert(Ready::WRITABLE);
                                }
                            });

                            //trace!("WORKER[{}] channel readiness={:?}, interest={:?}, queue={} elements",
                            //  line!(), self.channel.readiness, self.channel.interest, self.queue.len());
                            if self.channel.readiness() == Ready::EMPTY {
                                break;
                            }

                            // exit the big loop if the message is HardStop
                            if self.read_channel_messages_and_notify() {
                                return;
                            }

                            QUEUE.with(|queue| {
                                if !(*queue.borrow()).is_empty() {
                                    self.channel.interest.insert(Ready::WRITABLE);
                                }
                            });

                            self.send_queue();
                        }
                    }
                    // timer tick
                    Token(1) => {
                        while let Some(t) = TIMER.with(|timer| timer.borrow_mut().poll()) {
                            self.timeout(t);
                        }
                    }
                    // metrics socket is writable
                    Token(2) => METRICS.with(|metrics| {
                        (*metrics.borrow_mut()).writable();
                    }),
                    // ListenToken: 1 listener <=> 1 token
                    // ProtocolToken (HTTP/HTTPS/TCP): 1 connection <=> 1 token
                    token if self.health_checker.owns_token(token) => {
                        self.health_checker.ready(token);
                    }
                    token => self.ready(token, Ready::from(event)),
                }
            }

            if let Some(t) = self.should_poll_at.as_ref() {
                if *t <= Instant::now() {
                    while let Some(t) = TIMER.with(|timer| timer.borrow_mut().poll()) {
                        //info!("polled for timeout: {:?}", t);
                        self.timeout(t);
                    }
                }
            }
            self.handle_remaining_readiness();
            self.create_sessions();

            self.should_poll_at = TIMER.with(|timer| timer.borrow().next_poll_date());

            self.zombie_check();
            self.health_checker
                .poll(&self.backends, self.poll.registry());

            let now = time::OffsetDateTime::now_utc();
            // clear the local metrics drain every plain hour (01:00, 02:00, etc.) to prevent memory overuse
            // TODO: have one-hour-lasting metrics instead
            if now.minute() == 00 && now.second() == 0 {
                METRICS.with(|metrics| {
                    (*metrics.borrow_mut()).clear_local();
                });
            }

            // Frontend session gauges. `client.connections` keeps the
            // per-event signal from `SessionManager::incr/decr`; the rest
            // follow the once-per-iteration batching contract this run loop
            // uses for `process.uptime_seconds` / `server.live`.
            //
            // `slab.usage_percent` charts pure slab utilisation against
            // `slab.capacity()`. `slab.accept_threshold_percent` charts how
            // close the slab is to the `at_capacity()` accept gate
            // (`10 + 2 * max_connections`, see
            // `SessionManager::accept_slab_threshold`). Configured slab
            // capacity can be larger than that gate (`slab_entries_per_connection`
            // > 2), so the two gauges are kept independent on purpose.
            {
                let sessions = self.sessions.borrow();
                let nb_connections = sessions.nb_connections;
                let max_connections = sessions.max_connections;
                let slab_len = sessions.slab.len();
                let slab_capacity = sessions.slab.capacity();
                let accept_threshold = sessions.accept_slab_threshold();

                gauge!("client.connections", nb_connections);
                gauge!("client.connections_max", max_connections);
                if max_connections > 0 {
                    gauge!(
                        "client.connections_percent",
                        nb_connections * 100 / max_connections
                    );
                }

                gauge!("slab.entries", slab_len);
                gauge!("slab.capacity", slab_capacity);
                if slab_capacity > 0 {
                    gauge!("slab.usage_percent", slab_len * 100 / slab_capacity);
                }
                if accept_threshold > 0 {
                    gauge!(
                        "slab.accept_threshold_percent",
                        slab_len * 100 / accept_threshold
                    );
                }
            }
            // Buffer pool gauges. `buffer.in_use` replaces the older
            // `buffer.number` (renamed in `lib/src/pool.rs` for naming
            // consistency with the surrounding `buffer.*` keys).
            // `buffer.usage_percent` is computed against the configured
            // `buffer.capacity` so dashboards can chart pool pressure.
            {
                let pool = self.pool.borrow();
                let used = pool.inner.used();
                let capacity = pool.inner.capacity();
                gauge!("buffer.in_use", used);
                gauge!("buffer.capacity", capacity);
                if capacity > 0 {
                    gauge!("buffer.usage_percent", used * 100 / capacity);
                }
            }
            // 1Hz tick for `accept_queue.saturated_seconds`. Increments once
            // per `ACCEPT_SATURATION_TICK` while `SessionManager::can_accept`
            // is `false`. Distinguishes "queue spent N seconds at max" from
            // "queue briefly hit max" — the binary `accept_queue.backpressure`
            // gauge collapses that duration. Sampled here rather than via a
            // dedicated mio timer because the run loop ticks at least once
            // per `poll_timeout` (1s by default), which is granular enough.
            let now = Instant::now();
            if now.duration_since(self.last_saturation_tick) >= ACCEPT_SATURATION_TICK {
                if !self.sessions.borrow().can_accept {
                    incr!("accept_queue.saturated_seconds");
                }
                self.last_saturation_tick = now;
            }
            // Process / runtime gauges sampled once per loop iteration. Same
            // batch as `client.connections` so dashboards see them update in
            // lock-step.
            gauge!(
                "process.uptime_seconds",
                self.started_at.elapsed().as_secs() as usize
            );
            // `server.live` flips to 0 once a graceful shutdown is requested,
            // matching Envoy's `server.live` semantics. L4 health checks
            // (HAProxy / cloud LBs) can poll this gauge to drain a worker
            // before the OS-level termination signal lands.
            gauge!(
                "server.live",
                if self.shutting_down.is_some() { 0 } else { 1 }
            );
            METRICS.with(|metrics| {
                (*metrics.borrow_mut()).send_data();
            });

            if self.shutting_down.is_some() && self.shut_down_sessions() {
                return;
            }
        }
    }

    fn check_for_poll_errors(&mut self) {
        if self.current_poll_errors >= self.max_poll_errors {
            error!(
                "Something is going very wrong. Last {} poll() calls failed, crashing..",
                self.current_poll_errors
            );
            panic!(
                "poll() calls failed {} times in a row",
                self.current_poll_errors
            );
        }
    }

    fn reset_loop_time_and_get_timeout(&mut self) -> Option<Duration> {
        let now = Instant::now();
        time!("event_loop_time", (now - self.loop_start).as_millis());

        let mut timeout = match self.should_poll_at.as_ref() {
            None => self.poll_timeout,
            Some(i) => {
                if *i <= now {
                    self.poll_timeout
                } else {
                    let dur = *i - now;
                    match self.poll_timeout {
                        None => Some(dur),
                        Some(t) => {
                            if t < dur {
                                Some(t)
                            } else {
                                Some(dur)
                            }
                        }
                    }
                }
            }
        };

        if self.shutting_down.is_some() {
            let shutdown_tick = Duration::from_millis(100);
            timeout = match timeout {
                None => Some(shutdown_tick),
                Some(current) => Some(current.min(shutdown_tick)),
            };
        }

        self.loop_start = now;
        timeout
    }

    /// Returns true if hardstop
    fn read_channel_messages_and_notify(&mut self) -> bool {
        if !self.channel.readiness().is_readable() {
            return false;
        }

        if let Err(e) = self.channel.readable() {
            error!("error reading from channel: {:?}", e);
        }

        loop {
            let request = self.channel.read_message();
            debug!("Received request {:?}", request);
            match request {
                Ok(request) => match request.content.request_type {
                    Some(RequestType::HardStop(_)) => {
                        let req_id = request.id.clone();
                        self.notify(request);
                        if let Err(e) = self.channel.write_message(&WorkerResponse::ok(req_id)) {
                            error!("Could not send ok response to the main process: {}", e);
                        }
                        if let Err(e) = self.channel.run() {
                            error!("Error while running the server channel: {}", e);
                        }
                        return true;
                    }
                    Some(RequestType::SoftStop(_)) => {
                        self.shutting_down = Some(request.id.clone());
                        self.last_sessions_len = self.sessions.borrow().slab.len();
                        self.notify(request);
                    }
                    Some(RequestType::ReturnListenSockets(_)) => {
                        info!("received ReturnListenSockets order");
                        match self.return_listen_sockets() {
                            Ok(_) => push_queue(WorkerResponse::ok(request.id)),
                            Err(error) => push_queue(worker_response_error(
                                request.id,
                                format!("Could not send listeners on scm socket: {error:?}"),
                            )),
                        }
                    }
                    _ => self.notify(request),
                },
                // Not an error per se, occurs when there is nothing to read
                Err(_) => {
                    // if the message was too large, we grow the buffer and retry to read if possible
                    if (self.channel.interest & self.channel.readiness).is_readable() {
                        if let Err(e) = self.channel.readable() {
                            error!("error reading from channel: {:?}", e);
                        }
                        continue;
                    }
                    break;
                }
            }
        }
        false
    }

    /// Scans all sessions that have been inactive for longer than the configured interval
    fn zombie_check(&mut self) {
        let now = Instant::now();
        if now - self.last_zombie_check < self.zombie_check_interval {
            return;
        }
        info!("zombie check");
        self.last_zombie_check = now;

        let mut zombie_tokens = HashSet::new();

        // find the zombie sessions
        for (_index, session) in self
            .sessions
            .borrow_mut()
            .slab
            .iter_mut()
            .filter(|(_, c)| now - c.borrow().last_event() > self.zombie_check_interval)
        {
            let session_token = session.borrow().frontend_token();
            if !zombie_tokens.contains(&session_token) {
                session.borrow().print_session();
                zombie_tokens.insert(session_token);
            }
        }

        let zombie_count = zombie_tokens.len() as i64;
        count!("zombies", zombie_count);

        let remaining_count = self.shut_down_sessions_by_frontend_tokens(zombie_tokens);
        info!(
            "removing {} zombies ({} remaining entries after close)",
            zombie_count, remaining_count
        );
    }

    /// Calls close on targeted sessions, yields the number of entries in the slab
    /// that were not properly removed
    fn shut_down_sessions_by_frontend_tokens(&self, tokens: HashSet<Token>) -> usize {
        if tokens.is_empty() {
            return 0;
        }

        // close the sessions associated with the tokens
        for token in &tokens {
            if self.sessions.borrow().slab.contains(token.0) {
                let session = { self.sessions.borrow_mut().slab.remove(token.0) };
                session.borrow_mut().close();
                self.sessions.borrow_mut().decr();
            }
        }

        // find the entries of closed sessions in the session manager (they should not be there)
        let mut dangling_entries = HashSet::new();
        for (entry_key, session) in &self.sessions.borrow().slab {
            if tokens.contains(&session.borrow().frontend_token()) {
                dangling_entries.insert(entry_key);
            }
        }

        // remove these from the session manager
        let mut dangling_entries_count = 0;
        for entry_key in dangling_entries {
            let mut sessions = self.sessions.borrow_mut();
            if sessions.slab.contains(entry_key) {
                sessions.slab.remove(entry_key);
                dangling_entries_count += 1;
            }
        }
        dangling_entries_count
    }

    /// Order sessions to shut down, check that they are all down
    fn shut_down_sessions(&mut self) -> bool {
        let sessions_count = self.sessions.borrow().slab.len();
        let mut sessions_to_shut_down = HashSet::new();

        for (_key, session) in &self.sessions.borrow().slab {
            let mut session = session.borrow_mut();
            if session.shutting_down() {
                debug!(
                    "Server killing session from shutting_down: token={:?}, protocol={:?}",
                    session.frontend_token(),
                    session.protocol()
                );
                sessions_to_shut_down.insert(Token(session.frontend_token().0));
            }
        }
        let _ = self.shut_down_sessions_by_frontend_tokens(sessions_to_shut_down);

        let new_sessions_count = self.sessions.borrow().slab.len();

        if new_sessions_count < sessions_count {
            let now = Instant::now();
            if let Some(last) = self.last_shutting_down_message {
                if (now - last) > Duration::from_secs(5) {
                    info!(
                        "closed {} sessions, {} sessions left, base_sessions_count = {}",
                        sessions_count - new_sessions_count,
                        new_sessions_count,
                        self.base_sessions_count
                    );
                }
            }
            self.last_shutting_down_message = Some(now);
        }

        if new_sessions_count <= self.base_sessions_count {
            info!("last session stopped, shutting down!");
            if let Err(e) = self.channel.run() {
                error!("Error while running the server channel: {}", e);
            }
            // self.block_channel();
            let id = self
                .shutting_down
                .take()
                .expect("should have shut down correctly"); // panicking here makes sense actually

            debug!("Responding OK to main process for request {}", id);

            let proxy_response = WorkerResponse::ok(id);
            if let Err(e) = self.channel.write_message(&proxy_response) {
                error!("Could not write response to the main process: {}", e);
            }
            if let Err(e) = self.channel.run() {
                error!("Error while running the server channel: {}", e);
            }
            return true;
        }

        if new_sessions_count < self.last_sessions_len {
            info!(
                "shutting down, {} slab elements remaining (base: {})",
                new_sessions_count - self.base_sessions_count,
                self.base_sessions_count
            );
            self.last_sessions_len = new_sessions_count;
        }

        false
    }

    fn kill_session(&self, session: Rc<RefCell<dyn ProxySession>>) {
        let token = session.borrow().frontend_token();
        let _ = self.shut_down_sessions_by_frontend_tokens(HashSet::from([token]));
    }

    fn send_queue(&mut self) {
        if self.channel.readiness.is_writable() {
            QUEUE.with(|q| {
                let mut queue = q.borrow_mut();
                loop {
                    if let Some(resp) = queue.pop_front() {
                        debug!("Sending response {:?}", resp);
                        if let Err(e) = self.channel.write_message(&resp) {
                            error!("Could not write message {} on the channel: {}", resp, e);
                            queue.push_front(resp);
                        }
                    }

                    if self.channel.back_buf.available_data() > 0 {
                        if let Err(e) = self.channel.writable() {
                            error!("error writing to channel: {:?}", e);
                        }
                    }

                    if !self.channel.readiness.is_writable() {
                        break;
                    }

                    if self.channel.back_buf.available_data() == 0 && queue.is_empty() {
                        break;
                    }
                }
            });
        }
    }

    fn notify(&mut self, message: WorkerRequest) {
        match &message.content.request_type {
            Some(RequestType::ConfigureMetrics(configuration)) => {
                match MetricsConfiguration::try_from(*configuration) {
                    Ok(metrics_config) => {
                        METRICS.with(|metrics| {
                            (*metrics.borrow_mut()).configure(&metrics_config);
                            push_queue(WorkerResponse::ok(message.id));
                        });
                    }
                    Err(e) => {
                        error!("Error configuring metrics: {}", e);
                        push_queue(WorkerResponse::error(message.id, e));
                    }
                }
                return;
            }
            Some(RequestType::QueryMetrics(query_metrics_options)) => {
                METRICS.with(|metrics| {
                    match (*metrics.borrow_mut()).query(query_metrics_options) {
                        Ok(c) => push_queue(WorkerResponse::ok_with_content(message.id, c)),
                        Err(e) => {
                            error!("Error querying metrics: {}", e);
                            push_queue(WorkerResponse::error(message.id, e))
                        }
                    }
                });
                return;
            }
            Some(RequestType::Logging(logging_filter)) => {
                info!(
                    "{} changing logging filter to {}",
                    message.id, logging_filter
                );
                // there should not be any errors as it was already parsed by the main process
                let (directives, _errors) = logging::parse_logging_spec(logging_filter);
                logging::LOGGER.with(|logger| {
                    logger.borrow_mut().set_directives(directives);
                });
                push_queue(WorkerResponse::ok(message.id));
                return;
            }
            Some(RequestType::QueryClustersHashes(_)) => {
                push_queue(WorkerResponse::ok_with_content(
                    message.id.clone(),
                    ContentType::ClusterHashes(ClusterHashes {
                        map: self.config_state.hash_state(),
                    })
                    .into(),
                ));
                return;
            }
            Some(RequestType::QueryClusterById(cluster_id)) => {
                push_queue(WorkerResponse::ok_with_content(
                    message.id.clone(),
                    ContentType::Clusters(ClusterInformations {
                        vec: self
                            .config_state
                            .cluster_state(cluster_id)
                            .map_or(vec![], |ci| vec![ci]),
                    })
                    .into(),
                ));
            }
            Some(RequestType::SetMaxConnectionsPerIp(limit)) => {
                let mut sessions = self.sessions.borrow_mut();
                let previous = sessions.max_connections_per_ip;
                sessions.max_connections_per_ip = *limit;
                // Disabling the feature on the fly should not leave
                // stale `(cluster, ip)` entries behind: drain the
                // bookkeeping so a re-enable starts from a clean slate
                // and `cluster_ip_at_limit` does not consult dead state.
                if *limit == 0 {
                    sessions.clear_cluster_ip_tracking();
                }
                info!(
                    "{} updated global max_connections_per_ip from {} to {}",
                    message.id, previous, limit
                );
                push_queue(WorkerResponse::ok(message.id));
                return;
            }
            Some(RequestType::QueryMaxConnectionsPerIp(_)) => {
                let limit = self.sessions.borrow().max_connections_per_ip;
                push_queue(WorkerResponse::ok_with_content(
                    message.id,
                    ContentType::MaxConnectionsPerIpLimit(
                        sozu_command::proto::command::MaxConnectionsPerIpLimit { limit },
                    )
                    .into(),
                ));
                return;
            }
            Some(RequestType::QueryClustersByDomain(domain)) => {
                let cluster_ids = self
                    .config_state
                    .get_cluster_ids_by_domain(domain.hostname.clone(), domain.path.clone());
                let vec = cluster_ids
                    .iter()
                    .filter_map(|cluster_id| self.config_state.cluster_state(cluster_id))
                    .collect();

                push_queue(WorkerResponse::ok_with_content(
                    message.id.clone(),
                    ContentType::Clusters(ClusterInformations { vec }).into(),
                ));
                return;
            }
            Some(RequestType::QueryCertificatesFromWorkers(filters)) => {
                if filters.fingerprint.is_some() {
                    let certs = self.config_state.get_certificates(filters.clone());
                    let response = if !certs.is_empty() {
                        WorkerResponse::ok_with_content(
                            message.id.clone(),
                            ContentType::CertificatesWithFingerprints(
                                CertificatesWithFingerprints { certs },
                            )
                            .into(),
                        )
                    } else {
                        worker_response_error(
                            message.id.clone(),
                            "Could not find certificate for this fingerprint",
                        )
                    };
                    push_queue(response);
                    return;
                }
                // if all certificates are queried, or filtered by domain name,
                // the request will be handled by the https proxy
            }
            _other_request => {}
        }
        self.notify_proxys(message);
    }

    pub fn notify_proxys(&mut self, request: WorkerRequest) {
        if let Err(e) = self.config_state.dispatch(&request.content) {
            error!("Could not execute order on config state: {}", e);
        }

        let req_id = request.id.clone();

        match request.content.request_type {
            Some(RequestType::AddCluster(ref cluster)) => {
                // Mirror the master-side ConfigState::add_cluster check so
                // off-channel paths (TOML reload, SaveState/LoadState, direct
                // API) that smuggle a malformed `cluster.health_check` cannot
                // arm the worker's BackendList::set_health_check_config with
                // a CRLF/NUL/C0 URI or zero thresholds. The SetHealthCheck
                // handler below already runs the same validation; this is the
                // AddCluster mirror.
                if let Some(hc) = cluster.health_check.as_ref() {
                    if let Err(reason) = sozu_command::config::validate_health_check_config(hc) {
                        push_queue(worker_response_error(req_id, reason));
                        return;
                    }
                }
                self.add_cluster(cluster);
                //not returning because the message must still be handled by each proxy
            }
            Some(RequestType::RemoveCluster(ref cluster_id)) => {
                self.remove_health_check_state(cluster_id);
                //not returning because the message must still be handled by each proxy
            }
            Some(RequestType::SetHealthCheck(ref set)) => {
                if let Err(reason) = sozu_command::config::validate_health_check_config(&set.config)
                {
                    push_queue(worker_response_error(req_id, reason));
                    return;
                }
                self.backends
                    .borrow_mut()
                    .set_health_check_config(&set.cluster_id, Some(set.config.to_owned()));
                push_queue(WorkerResponse::ok(req_id));
                return;
            }
            Some(RequestType::RemoveHealthCheck(ref cluster_id)) => {
                self.remove_health_check_state(cluster_id);
                push_queue(WorkerResponse::ok(req_id));
                return;
            }
            Some(RequestType::AddBackend(ref backend)) => {
                push_queue(self.add_backend(&req_id, backend));
                return;
            }
            Some(RequestType::RemoveBackend(ref remove_backend)) => {
                push_queue(self.remove_backend(&req_id, remove_backend));
                return;
            }
            _ => {}
        };

        let proxy_destinations = request.content.get_destinations();
        let mut notify_response = None;
        if proxy_destinations.to_http_proxy {
            notify_response = Some(self.http.borrow_mut().notify(request.clone()));
        }
        if proxy_destinations.to_https_proxy {
            let http_proxy_response = self.https.borrow_mut().notify(request.clone());
            if http_proxy_response.is_failure() || notify_response.is_none() {
                notify_response = Some(http_proxy_response);
            }
        }
        if proxy_destinations.to_tcp_proxy {
            let tcp_proxy_response = self.tcp.borrow_mut().notify(request.clone());
            if tcp_proxy_response.is_failure() || notify_response.is_none() {
                notify_response = Some(tcp_proxy_response);
            }
        }
        if let Some(response) = notify_response {
            push_queue(response);
        }

        match request.content.request_type {
            // special case for adding listeners, because we need to register a listener
            Some(RequestType::AddHttpListener(listener)) => {
                push_queue(self.notify_add_http_listener(&req_id, listener));
            }
            Some(RequestType::AddHttpsListener(listener)) => {
                push_queue(self.notify_add_https_listener(&req_id, listener));
            }
            Some(RequestType::AddTcpListener(listener)) => {
                push_queue(self.notify_add_tcp_listener(&req_id, listener));
            }
            Some(RequestType::UpdateHttpListener(patch)) => {
                push_queue(self.notify_update_http_listener(&req_id, patch));
            }
            Some(RequestType::UpdateHttpsListener(patch)) => {
                push_queue(self.notify_update_https_listener(&req_id, patch));
            }
            Some(RequestType::UpdateTcpListener(patch)) => {
                push_queue(self.notify_update_tcp_listener(&req_id, patch));
            }
            Some(RequestType::RemoveListener(ref remove)) => {
                debug!("{} remove {:?} listener {:?}", req_id, remove.proxy, remove);
                self.base_sessions_count -= 1;
                let response = match ListenerType::try_from(remove.proxy) {
                    Ok(ListenerType::Http) => self.http.borrow_mut().notify(request),
                    Ok(ListenerType::Https) => self.https.borrow_mut().notify(request),
                    Ok(ListenerType::Tcp) => self.tcp.borrow_mut().notify(request),
                    Err(_) => WorkerResponse::error(req_id, "Wrong variant ListenerType"),
                };
                push_queue(response);
            }
            Some(RequestType::ActivateListener(ref activate)) => {
                push_queue(self.notify_activate_listener(&req_id, activate));
            }
            Some(RequestType::DeactivateListener(ref deactivate)) => {
                push_queue(self.notify_deactivate_listener(&req_id, deactivate));
            }
            _other_request => {}
        };
    }

    fn add_cluster(&mut self, cluster: &Cluster) {
        let mut backends = self.backends.borrow_mut();
        backends.set_load_balancing_policy_for_cluster(
            &cluster.cluster_id,
            LoadBalancingAlgorithms::try_from(cluster.load_balancing).unwrap_or_default(),
            cluster
                .load_metric
                .and_then(|n| LoadMetric::try_from(n).ok()),
        );
        backends.set_health_check_config(&cluster.cluster_id, cluster.health_check.to_owned());
        backends.set_cluster_http2(&cluster.cluster_id, cluster.http2.unwrap_or(false));
    }

    fn add_backend(&mut self, req_id: &str, add_backend: &AddBackend) -> WorkerResponse {
        let new_backend = Backend::new(
            &add_backend.backend_id,
            add_backend.address.into(),
            add_backend.sticky_id.clone(),
            add_backend.load_balancing_parameters,
            add_backend.backup,
        );
        self.backends
            .borrow_mut()
            .add_backend(&add_backend.cluster_id, new_backend);

        WorkerResponse::ok(req_id)
    }

    fn remove_health_check_state(&mut self, cluster_id: &str) {
        self.health_checker.remove_cluster(cluster_id);
        self.backends
            .borrow_mut()
            .health_check_configs
            .remove(cluster_id);
    }

    fn remove_backend(&mut self, req_id: &str, backend: &RemoveBackend) -> WorkerResponse {
        let address = backend.address.into();
        self.backends
            .borrow_mut()
            .remove_backend(&backend.cluster_id, &address);

        WorkerResponse::ok(req_id)
    }

    fn notify_add_http_listener(
        &mut self,
        req_id: &str,
        listener: HttpListenerConfig,
    ) -> WorkerResponse {
        debug!("{} add http listener {:?}", req_id, listener);

        if self.sessions.borrow().at_capacity() {
            return worker_response_error(req_id, "session list is full, cannot add a listener");
        }

        let mut session_manager = self.sessions.borrow_mut();
        let entry = session_manager.slab.vacant_entry();
        let token = Token(entry.key());

        match self.http.borrow_mut().add_listener(listener, token) {
            Ok(_token) => {
                entry.insert(Rc::new(RefCell::new(ListenSession {
                    protocol: Protocol::HTTPListen,
                })));
                self.base_sessions_count += 1;
                WorkerResponse::ok(req_id)
            }
            Err(e) => worker_response_error(req_id, format!("Could not add HTTP listener: {e}")),
        }
    }

    fn notify_add_https_listener(
        &mut self,
        req_id: &str,
        listener: HttpsListenerConfig,
    ) -> WorkerResponse {
        debug!("{} add https listener {:?}", req_id, listener);

        if self.sessions.borrow().at_capacity() {
            return worker_response_error(req_id, "session list is full, cannot add a listener");
        }

        let mut session_manager = self.sessions.borrow_mut();
        let entry = session_manager.slab.vacant_entry();
        let token = Token(entry.key());

        match self
            .https
            .borrow_mut()
            .add_listener(listener.clone(), token)
        {
            Ok(_token) => {
                entry.insert(Rc::new(RefCell::new(ListenSession {
                    protocol: Protocol::HTTPSListen,
                })));
                self.base_sessions_count += 1;
                WorkerResponse::ok(req_id)
            }
            Err(e) => worker_response_error(req_id, format!("Could not add HTTPS listener: {e}")),
        }
    }

    fn notify_add_tcp_listener(
        &mut self,
        req_id: &str,
        listener: CommandTcpListener,
    ) -> WorkerResponse {
        debug!("{} add tcp listener {:?}", req_id, listener);

        if self.sessions.borrow().at_capacity() {
            return worker_response_error(req_id, "session list is full, cannot add a listener");
        }

        let mut session_manager = self.sessions.borrow_mut();
        let entry = session_manager.slab.vacant_entry();
        let token = Token(entry.key());

        match self.tcp.borrow_mut().add_listener(listener, token) {
            Ok(_token) => {
                entry.insert(Rc::new(RefCell::new(ListenSession {
                    protocol: Protocol::TCPListen,
                })));
                self.base_sessions_count += 1;
                WorkerResponse::ok(req_id)
            }
            Err(e) => worker_response_error(req_id, format!("Could not add TCP listener: {e}")),
        }
    }

    fn notify_update_http_listener(
        &mut self,
        req_id: &str,
        patch: UpdateHttpListenerConfig,
    ) -> WorkerResponse {
        debug!("{} update http listener {:?}", req_id, patch.address);
        match self.http.borrow_mut().update_listener(patch) {
            Ok(()) => WorkerResponse::ok(req_id),
            Err(e) => worker_response_error(req_id, format!("Could not update HTTP listener: {e}")),
        }
    }

    fn notify_update_https_listener(
        &mut self,
        req_id: &str,
        patch: UpdateHttpsListenerConfig,
    ) -> WorkerResponse {
        debug!("{} update https listener {:?}", req_id, patch.address);
        match self.https.borrow_mut().update_listener(patch) {
            Ok(()) => WorkerResponse::ok(req_id),
            Err(e) => {
                worker_response_error(req_id, format!("Could not update HTTPS listener: {e}"))
            }
        }
    }

    fn notify_update_tcp_listener(
        &mut self,
        req_id: &str,
        patch: UpdateTcpListenerConfig,
    ) -> WorkerResponse {
        debug!("{} update tcp listener {:?}", req_id, patch.address);
        match self.tcp.borrow_mut().update_listener(patch) {
            Ok(()) => WorkerResponse::ok(req_id),
            Err(e) => worker_response_error(req_id, format!("Could not update TCP listener: {e}")),
        }
    }

    fn notify_activate_listener(
        &mut self,
        req_id: &str,
        activate: &ActivateListener,
    ) -> WorkerResponse {
        debug!(
            "{} activate {:?} listener {:?}",
            req_id, activate.proxy, activate
        );

        let address: std::net::SocketAddr = activate.address.into();

        match ListenerType::try_from(activate.proxy) {
            Ok(ListenerType::Http) => {
                let listener = self
                    .scm_listeners
                    .as_mut()
                    .and_then(|s| s.get_http(&address))
                    // SAFETY: `fd` was just received from the supervisor via SCM_RIGHTS
                    // (see `command/src/scm_socket.rs`) and is not owned elsewhere — the
                    // `ScmListeners` map removes it on `get_http`. Ownership transfers to
                    // the mio wrapper, whose `Drop` closes the descriptor.
                    .map(|fd| unsafe { MioTcpListener::from_raw_fd(fd) });

                let activated_token = self.http.borrow_mut().activate_listener(&address, listener);
                match activated_token {
                    Ok(token) => {
                        self.accept(ListenToken(token.0), Protocol::HTTPListen);
                        WorkerResponse::ok(req_id)
                    }
                    Err(activate_error) => worker_response_error(
                        req_id,
                        format!("Could not activate HTTP listener: {activate_error}"),
                    ),
                }
            }
            Ok(ListenerType::Https) => {
                let listener = self
                    .scm_listeners
                    .as_mut()
                    .and_then(|s| s.get_https(&address))
                    // SAFETY: `fd` was just received from the supervisor via SCM_RIGHTS
                    // (see `command/src/scm_socket.rs`) and is not owned elsewhere — the
                    // `ScmListeners` map removes it on `get_https`. Ownership transfers to
                    // the mio wrapper, whose `Drop` closes the descriptor.
                    .map(|fd| unsafe { MioTcpListener::from_raw_fd(fd) });

                let activated_token = self
                    .https
                    .borrow_mut()
                    .activate_listener(&address, listener);
                match activated_token {
                    Ok(token) => {
                        self.accept(ListenToken(token.0), Protocol::HTTPSListen);
                        WorkerResponse::ok(req_id)
                    }
                    Err(activate_error) => worker_response_error(
                        req_id,
                        format!("Could not activate HTTPS listener: {activate_error}"),
                    ),
                }
            }
            Ok(ListenerType::Tcp) => {
                let listener = self
                    .scm_listeners
                    .as_mut()
                    .and_then(|s| s.get_tcp(&address))
                    // SAFETY: `fd` was just received from the supervisor via SCM_RIGHTS
                    // (see `command/src/scm_socket.rs`) and is not owned elsewhere — the
                    // `ScmListeners` map removes it on `get_tcp`. Ownership transfers to
                    // the mio wrapper, whose `Drop` closes the descriptor.
                    .map(|fd| unsafe { MioTcpListener::from_raw_fd(fd) });

                let listener_token = self.tcp.borrow_mut().activate_listener(&address, listener);
                match listener_token {
                    Ok(token) => {
                        self.accept(ListenToken(token.0), Protocol::TCPListen);
                        WorkerResponse::ok(req_id)
                    }
                    Err(activate_error) => worker_response_error(
                        req_id,
                        format!("Could not activate TCP listener: {activate_error}"),
                    ),
                }
            }
            Err(_) => worker_response_error(req_id, "Wrong variant for ListenerType on request"),
        }
    }

    fn notify_deactivate_listener(
        &mut self,
        req_id: &str,
        deactivate: &DeactivateListener,
    ) -> WorkerResponse {
        debug!(
            "{} deactivate {:?} listener {:?}",
            req_id, deactivate.proxy, deactivate
        );

        let address: std::net::SocketAddr = deactivate.address.into();

        match ListenerType::try_from(deactivate.proxy) {
            Ok(ListenerType::Http) => {
                let (token, mut listener) = match self.http.borrow_mut().give_back_listener(address)
                {
                    Ok((token, listener)) => (token, listener),
                    Err(e) => {
                        return worker_response_error(
                            req_id,
                            format!(
                                "Couldn't deactivate HTTP listener at address {address:?}: {e}"
                            ),
                        );
                    }
                };

                if let Err(e) = self.poll.registry().deregister(&mut listener) {
                    error!(
                        "error deregistering HTTP listen socket({:?}): {:?}",
                        deactivate, e
                    );
                }

                {
                    let mut sessions = self.sessions.borrow_mut();
                    if sessions.slab.contains(token.0) {
                        sessions.slab.remove(token.0);
                        info!("removed listen token {:?}", token);
                    }
                }

                if deactivate.to_scm {
                    self.unblock_scm_socket();
                    let listeners = Listeners {
                        http: vec![(address, listener.as_raw_fd())],
                        tls: vec![],
                        tcp: vec![],
                    };
                    info!("sending HTTP listener: {:?}", listeners);
                    let res = self.scm.send_listeners(&listeners);

                    self.block_scm_socket();

                    info!("sent HTTP listener: {:?}", res);
                }
                WorkerResponse::ok(req_id)
            }
            Ok(ListenerType::Https) => {
                let (token, mut listener) = match self
                    .https
                    .borrow_mut()
                    .give_back_listener(address)
                {
                    Ok((token, listener)) => (token, listener),
                    Err(e) => {
                        return worker_response_error(
                            req_id,
                            format!(
                                "Couldn't deactivate HTTPS listener at address {address:?}: {e}",
                            ),
                        );
                    }
                };
                if let Err(e) = self.poll.registry().deregister(&mut listener) {
                    error!(
                        "error deregistering HTTPS listen socket({:?}): {:?}",
                        deactivate, e
                    );
                }
                if self.sessions.borrow().slab.contains(token.0) {
                    self.sessions.borrow_mut().slab.remove(token.0);
                    info!("removed listen token {:?}", token);
                }

                if deactivate.to_scm {
                    self.unblock_scm_socket();
                    let listeners = Listeners {
                        http: vec![],
                        tls: vec![(address, listener.as_raw_fd())],
                        tcp: vec![],
                    };
                    info!("sending HTTPS listener: {:?}", listeners);
                    let res = self.scm.send_listeners(&listeners);

                    self.block_scm_socket();

                    info!("sent HTTPS listener: {:?}", res);
                }
                WorkerResponse::ok(req_id)
            }
            Ok(ListenerType::Tcp) => {
                let (token, mut listener) = match self.tcp.borrow_mut().give_back_listener(address)
                {
                    Ok((token, listener)) => (token, listener),
                    Err(e) => {
                        return worker_response_error(
                            req_id,
                            format!(
                                "Could not deactivate TCP listener at address {address:?}: {e}"
                            ),
                        );
                    }
                };

                if let Err(e) = self.poll.registry().deregister(&mut listener) {
                    error!(
                        "error deregistering TCP listen socket({:?}): {:?}",
                        deactivate, e
                    );
                }
                if self.sessions.borrow().slab.contains(token.0) {
                    self.sessions.borrow_mut().slab.remove(token.0);
                    info!("removed listen token {:?}", token);
                }

                if deactivate.to_scm {
                    self.unblock_scm_socket();
                    let listeners = Listeners {
                        http: vec![],
                        tls: vec![],
                        tcp: vec![(address, listener.as_raw_fd())],
                    };
                    info!("sending TCP listener: {:?}", listeners);
                    let res = self.scm.send_listeners(&listeners);

                    self.block_scm_socket();

                    info!("sent TCP listener: {:?}", res);
                }
                WorkerResponse::ok(req_id)
            }
            Err(_) => worker_response_error(req_id, "Wrong variant for ListenerType on request"),
        }
    }

    /// Send all socket addresses and file descriptors of all proxies, via the scm socket
    pub fn return_listen_sockets(&mut self) -> Result<(), ScmSocketError> {
        self.unblock_scm_socket();

        let mut http_listeners = self.http.borrow_mut().give_back_listeners();
        for &mut (_, ref mut sock) in http_listeners.iter_mut() {
            if let Err(e) = self.poll.registry().deregister(sock) {
                error!(
                    "error deregistering HTTP listen socket({:?}): {:?}",
                    sock, e
                );
            }
        }

        let mut https_listeners = self.https.borrow_mut().give_back_listeners();
        for &mut (_, ref mut sock) in https_listeners.iter_mut() {
            if let Err(e) = self.poll.registry().deregister(sock) {
                error!(
                    "error deregistering HTTPS listen socket({:?}): {:?}",
                    sock, e
                );
            }
        }

        let mut tcp_listeners = self.tcp.borrow_mut().give_back_listeners();
        for &mut (_, ref mut sock) in tcp_listeners.iter_mut() {
            if let Err(e) = self.poll.registry().deregister(sock) {
                error!("error deregistering TCP listen socket({:?}): {:?}", sock, e);
            }
        }

        // use as_raw_fd because the listeners should be dropped after sending them
        let listeners = Listeners {
            http: http_listeners
                .iter()
                .map(|(addr, listener)| (*addr, listener.as_raw_fd()))
                .collect(),
            tls: https_listeners
                .iter()
                .map(|(addr, listener)| (*addr, listener.as_raw_fd()))
                .collect(),
            tcp: tcp_listeners
                .iter()
                .map(|(addr, listener)| (*addr, listener.as_raw_fd()))
                .collect(),
        };
        info!("sending default listeners: {:?}", listeners);
        let res = self.scm.send_listeners(&listeners);

        self.block_scm_socket();

        info!("sent default listeners: {:?}", res);
        res
    }

    fn block_scm_socket(&mut self) {
        if let Err(e) = self.scm.set_blocking(true) {
            error!("Could not block scm socket: {}", e);
        }
    }

    fn unblock_scm_socket(&mut self) {
        if let Err(e) = self.scm.set_blocking(false) {
            error!("Could not unblock scm socket: {}", e);
        }
    }

    pub fn to_session(&self, token: Token) -> SessionToken {
        SessionToken(token.0)
    }

    pub fn from_session(&self, token: SessionToken) -> Token {
        Token(token.0)
    }

    pub fn accept(&mut self, token: ListenToken, protocol: Protocol) {
        // Per-protocol counter key. Keeping the namespace static (3 keys +
        // aggregate) is a deliberate cardinality cap: per-listener-address
        // labelling would require runtime `Box::leak` because `incr!` takes
        // `&'static str`, and listener addresses can be reconfigured at
        // runtime by the control plane. Operators wanting per-listener
        // attribution should correlate with the listener-protocol breakdown
        // below.
        //
        // Non-listen protocols reach this code only on an invariant break
        // upstream (`ready()` dispatched a non-listen `Protocol` to
        // `accept()`). Log and return rather than panicking — defense in
        // depth on the accept path, which is process-fatal if it aborts.
        let (proto_key, accepted_protocol) = match protocol {
            Protocol::TCPListen => ("listener.accepted.tcp", Protocol::TCPListen),
            Protocol::HTTPListen => ("listener.accepted.http", Protocol::HTTPListen),
            Protocol::HTTPSListen => ("listener.accepted.https", Protocol::HTTPSListen),
            other => {
                warn!(
                    "accept() called with non-listen protocol {:?} on token {:?}; skipping",
                    other, token
                );
                return;
            }
        };

        loop {
            let result = match accepted_protocol {
                Protocol::TCPListen => self.tcp.borrow_mut().accept(token),
                Protocol::HTTPListen => self.http.borrow_mut().accept(token),
                Protocol::HTTPSListen => self.https.borrow_mut().accept(token),
                // The outer match populates `accepted_protocol` only with the
                // three listen variants and returns early otherwise — this
                // arm is structurally unreachable.
                other => unreachable!(
                    "accept dispatch reached non-listen protocol {:?} after outer guard",
                    other
                ),
            };
            match result {
                Ok(sock) => {
                    // peer_addr() is one syscall (`getpeername(2)`) and runs
                    // exactly once per accepted socket. It can fail if the
                    // peer raced to close — recorded as `None` and silently
                    // skipped for the per-source counter.
                    let peer = sock.peer_addr().ok();
                    incr!("listener.accepted.total");
                    incr!(proto_key);
                    if let Some(peer_addr) = peer.as_ref() {
                        incr!(per_source_bucket(peer_addr));
                    }
                    self.accept_queue.push_back((
                        sock,
                        token,
                        accepted_protocol,
                        Instant::now(),
                        peer,
                    ));
                }
                Err(AcceptError::WouldBlock) => {
                    self.accept_ready.remove(&token);
                    break;
                }
                Err(other) => {
                    error!(
                        "error accepting {:?} sockets: {:?}",
                        accepted_protocol, other
                    );
                    self.accept_ready.remove(&token);
                    break;
                }
            }
        }

        gauge!("accept_queue.connections", self.accept_queue.len());
    }

    pub fn create_sessions(&mut self) {
        while let Some((sock, token, protocol, timestamp, _peer)) = self.accept_queue.pop_back() {
            let wait_time = Instant::now() - timestamp;
            time!("accept_queue.wait_time", wait_time.as_millis());
            if wait_time > self.accept_queue_timeout {
                incr!("accept_queue.timeout");
                continue;
            }

            if !self.sessions.borrow_mut().check_limits() {
                // The socket we just popped will not be served, plus every
                // remaining queued socket below `break` will time out.
                // `listener.connection_capped` counts the popped socket so
                // the counter aligns with `check_limits` invocations rather
                // than with queue depth at the time of refusal.
                incr!("listener.connection_capped");

                if !self.evict_on_queue_full {
                    break;
                }

                // Skip eviction during graceful shutdown — defeats the
                // shutting_down semantics and is wasted work since the
                // worker is winding down anyway.
                if self.shutting_down.is_some() {
                    break;
                }

                // Evict 1% of `max_connections` per iteration. Conservative
                // ratio: large enough to make meaningful progress clearing
                // the accept queue, small enough to limit collateral damage
                // to active sessions. The cap loop re-checks limits after
                // each eviction round, so multiple rounds can run if the
                // queue has many pending connections. Decoupled from
                // `slab_entries_per_connection` (which only sizes the slab,
                // not `max_connections`).
                let to_evict = (self.sessions.borrow().max_connections / 100).max(1);
                let evicted = self.evict_least_active_sessions(to_evict);
                if evicted == 0 {
                    // Informational, not an invariant break: the worker may
                    // be at boot, or every active session is a system
                    // protocol (Channel/Metrics/Timer/listeners) and is
                    // ineligible. Stay at warn so operators see it in info-
                    // level production logs.
                    warn!("evict_on_queue_full enabled but no candidate sessions to evict");
                    break;
                }

                count!("sessions.evicted", evicted as i64);
                warn!(
                    "evicted {} least recently active sessions to make room",
                    evicted
                );

                if !self.sessions.borrow_mut().check_limits() {
                    break;
                }
            }

            //FIXME: check the timestamp
            //TODO: create_session should return the session and
            // the server should insert it in the the SessionManager
            match protocol {
                Protocol::TCPListen => {
                    let proxy = self.tcp.clone();
                    if self
                        .tcp
                        .borrow_mut()
                        .create_session(sock, token, wait_time, proxy)
                        .is_err()
                    {
                        break;
                    }
                }
                Protocol::HTTPListen => {
                    let proxy = self.http.clone();
                    if self
                        .http
                        .borrow_mut()
                        .create_session(sock, token, wait_time, proxy)
                        .is_err()
                    {
                        break;
                    }
                }
                Protocol::HTTPSListen => {
                    if self
                        .https
                        .borrow_mut()
                        .create_session(sock, token, wait_time, self.https.clone())
                        .is_err()
                    {
                        break;
                    }
                }
                _ => panic!("should not call accept() on a HTTP, HTTPS or TCP session"),
            };
            self.sessions.borrow_mut().incr();
        }

        gauge!("accept_queue.connections", self.accept_queue.len());
    }

    pub fn ready(&mut self, token: Token, events: Ready) {
        trace!("PROXY\t{:?} got events: {:?}", token, events);

        let session_token = token.0;
        if self.sessions.borrow().slab.contains(session_token) {
            //info!("sessions contains {:?}", session_token);
            let protocol = self.sessions.borrow().slab[session_token]
                .borrow()
                .protocol();
            //info!("protocol: {:?}", protocol);
            match protocol {
                Protocol::HTTPListen | Protocol::HTTPSListen | Protocol::TCPListen => {
                    //info!("PROTOCOL IS LISTEN");
                    if events.is_readable() {
                        self.accept_ready.insert(ListenToken(token.0));
                        if self.sessions.borrow().can_accept {
                            self.accept(ListenToken(token.0), protocol);
                        }
                        return;
                    }

                    if events.is_writable() {
                        error!(
                            "received writable for listener {:?}, this should not happen",
                            token
                        );
                        return;
                    }

                    if events.is_hup() {
                        error!("should not happen: server {:?} closed", token);
                        return;
                    }

                    unreachable!();
                }
                _ => {}
            }

            let session = self.sessions.borrow_mut().slab[session_token].clone();
            session.borrow_mut().update_readiness(token, events);
            if session.borrow_mut().ready(session.clone()) {
                debug!(
                    "Server killing session from ready: token={:?}, protocol={:?}, events={:?}",
                    token, protocol, events
                );
                self.kill_session(session);
            }
        }
    }

    pub fn timeout(&mut self, token: Token) {
        trace!("PROXY\t{:?} got timeout", token);

        let session_token = token.0;
        if self.sessions.borrow().slab.contains(session_token) {
            let session = self.sessions.borrow_mut().slab[session_token].clone();
            if session.borrow_mut().timeout(token) {
                debug!(
                    "Server killing session from timeout: token={:?}, protocol={:?}",
                    token,
                    session.borrow().protocol()
                );
                self.kill_session(session);
            }
        }
    }

    pub fn handle_remaining_readiness(&mut self) {
        // try to accept again after handling all session events,
        // since we might have released a few session slots
        if self.sessions.borrow().can_accept && !self.accept_ready.is_empty() {
            while let Some(token) = self
                .accept_ready
                .iter()
                .next()
                .map(|token| ListenToken(token.0))
            {
                let protocol = self.sessions.borrow().slab[token.0].borrow().protocol();
                self.accept(token, protocol);
                if !self.sessions.borrow().can_accept || self.accept_ready.is_empty() {
                    break;
                }
            }
        }
    }
    fn block_channel(&mut self) {
        if let Err(e) = self.channel.blocking() {
            error!("Could not block channel: {}", e);
        }
    }
    fn unblock_channel(&mut self) {
        if let Err(e) = self.channel.nonblocking() {
            error!("Could not block channel: {}", e);
        }
    }

    /// Evict the `count` least-recently-active non-listener sessions and
    /// return how many tokens were enqueued for shutdown. Used by
    /// `create_sessions` when the accept queue is saturated and the
    /// `evict_on_queue_full` knob is set.
    ///
    /// Uses `select_nth_unstable_by_key` (introselect, O(n) average) to
    /// partition the oldest `count` sessions in-place rather than a full
    /// O(n log n) sort. The candidate `Vec` is unavoidable because of
    /// `RefCell` borrow rules — the immutable borrow on `self.sessions`
    /// must drop before `shut_down_sessions_by_frontend_tokens` can take
    /// its mutable borrow.
    fn evict_least_active_sessions(&self, count: usize) -> usize {
        if count == 0 {
            return 0;
        }

        let tokens = {
            let sessions = self.sessions.borrow();
            let mut candidates: Vec<(Token, Instant)> = sessions
                .slab
                .iter()
                .filter(|(_, session)| {
                    !matches!(
                        session.borrow().protocol(),
                        Protocol::HTTPListen
                            | Protocol::HTTPSListen
                            | Protocol::TCPListen
                            | Protocol::Channel
                            | Protocol::Metrics
                            | Protocol::Timer
                    )
                })
                .map(|(_, session)| {
                    let s = session.borrow();
                    (s.frontend_token(), s.last_event())
                })
                .collect();

            // Early return is load-bearing: the `pivot` computation below
            // does `count.min(len) - 1`, which underflows on empty input.
            if candidates.is_empty() {
                return 0;
            }

            let pivot = count.min(candidates.len()) - 1;
            candidates.select_nth_unstable_by_key(pivot, |&(_, last_event)| last_event);

            candidates[..=pivot]
                .iter()
                .map(|&(token, _)| token)
                .collect::<HashSet<Token>>()
        };

        let evicted = tokens.len();
        self.shut_down_sessions_by_frontend_tokens(tokens);
        evicted
    }
}

/// log the error together with the request id
/// create a WorkerResponse
fn worker_response_error<S: ToString, T: ToString>(request_id: S, error: T) -> WorkerResponse {
    error!(
        "error on request {}, {}",
        request_id.to_string(),
        error.to_string()
    );
    WorkerResponse::error(request_id, error)
}

pub struct ListenSession {
    pub protocol: Protocol,
}

impl ProxySession for ListenSession {
    fn last_event(&self) -> Instant {
        Instant::now()
    }

    fn print_session(&self) {}

    fn frontend_token(&self) -> Token {
        Token(0)
    }

    fn protocol(&self) -> Protocol {
        self.protocol
    }

    fn ready(&mut self, _session: Rc<RefCell<dyn ProxySession>>) -> SessionIsToBeClosed {
        false
    }

    fn shutting_down(&mut self) -> SessionIsToBeClosed {
        false
    }

    fn update_readiness(&mut self, _token: Token, _events: Ready) {}

    fn close(&mut self) {}

    fn timeout(&mut self, _token: Token) -> SessionIsToBeClosed {
        error!(
            "called ProxySession::timeout(token={:?}, time) on ListenSession {{ protocol: {:?} }}",
            _token, self.protocol
        );
        false
    }
}

#[cfg(test)]
mod accept_telemetry_tests {
    use super::*;

    /// Two IPv4 addresses sharing the same /24 must hash to the same bucket;
    /// the masking logic guarantees this regardless of the host octet.
    #[test]
    fn per_source_bucket_collapses_ipv4_slash24() {
        let a: SocketAddr = "203.0.113.5:1234".parse().unwrap();
        let b: SocketAddr = "203.0.113.250:9999".parse().unwrap();
        assert_eq!(
            per_source_bucket(&a),
            per_source_bucket(&b),
            "addresses in the same /24 must land in the same bucket"
        );
    }

    /// Two IPv6 addresses sharing the same /48 must hash to the same bucket.
    #[test]
    fn per_source_bucket_collapses_ipv6_slash48() {
        let a: SocketAddr = "[2001:db8:1234::1]:443".parse().unwrap();
        let b: SocketAddr = "[2001:db8:1234:abcd::ffff]:8443".parse().unwrap();
        assert_eq!(
            per_source_bucket(&a),
            per_source_bucket(&b),
            "addresses in the same /48 must land in the same bucket"
        );
    }

    /// Every bucket label must be one of `PER_SOURCE_BUCKETS` precomputed
    /// statics — the cardinality cap is the load-bearing property.
    #[test]
    fn per_source_bucket_keys_are_bounded() {
        assert_eq!(PER_SOURCE_BUCKET_KEYS.len(), PER_SOURCE_BUCKETS);
        for (i, key) in PER_SOURCE_BUCKET_KEYS.iter().enumerate() {
            let expected = format!("client.connect.per_source.bucket_{i:03}");
            assert_eq!(*key, expected.as_str());
        }
    }

    /// A modest sample across distinct /24 prefixes should hit a healthy
    /// number of distinct buckets — guards against the hash collapsing.
    #[test]
    fn per_source_bucket_distributes_distinct_subnets() {
        let mut hits = std::collections::HashSet::new();
        for i in 0..200u8 {
            let addr: SocketAddr = format!("10.0.{i}.42:80").parse().unwrap();
            hits.insert(per_source_bucket(&addr));
        }
        // With 200 distinct /24 prefixes hashed into 256 buckets we expect
        // many distinct labels — assert a conservative lower bound that
        // tolerates birthday collisions.
        assert!(
            hits.len() >= 100,
            "expected at least 100 distinct buckets across 200 /24s, got {}",
            hits.len()
        );
    }
}

#[cfg(test)]
mod eviction_tests {
    use std::collections::HashSet;
    use std::time::{Duration, Instant};

    use mio::Token;

    /// `select_nth_unstable_by_key` partitions in O(n) so that the first
    /// `pivot + 1` entries are the `pivot + 1` smallest by key. This guards
    /// against a future refactor swapping the comparator orientation.
    #[test]
    fn select_nth_finds_oldest_sessions() {
        let now = Instant::now();
        let mut candidates = [
            (Token(1), now - Duration::from_secs(10)), // 10s old
            (Token(2), now - Duration::from_secs(50)), // 50s old (oldest)
            (Token(3), now - Duration::from_secs(5)),  // 5s old (newest)
            (Token(4), now - Duration::from_secs(30)), // 30s old
            (Token(5), now - Duration::from_secs(20)), // 20s old
        ];

        let count = 2;
        let pivot = count.min(candidates.len()) - 1;
        candidates.select_nth_unstable_by_key(pivot, |&(_, last_event)| last_event);

        let selected: HashSet<Token> = candidates[..=pivot]
            .iter()
            .map(|&(token, _)| token)
            .collect();

        assert_eq!(selected.len(), 2);
        assert!(
            selected.contains(&Token(2)),
            "should contain 50s-old session"
        );
        assert!(
            selected.contains(&Token(4)),
            "should contain 30s-old session"
        );
    }

    /// When `count` exceeds available candidates, the pivot collapses to
    /// `len - 1` so we evict everything; this test pins that behaviour
    /// against a future refactor that might silently truncate.
    #[test]
    fn select_nth_with_count_exceeding_candidates() {
        let now = Instant::now();
        let mut candidates = [(Token(1), now - Duration::from_secs(10))];

        let count = 5;
        let pivot = count.min(candidates.len()) - 1;
        candidates.select_nth_unstable_by_key(pivot, |&(_, last_event)| last_event);

        let selected: HashSet<Token> = candidates[..=pivot]
            .iter()
            .map(|&(token, _)| token)
            .collect();

        assert_eq!(selected.len(), 1);
        assert!(selected.contains(&Token(1)));
    }
}
