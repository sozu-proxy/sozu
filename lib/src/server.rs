//! event loop management
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque, hash_map::Entry},
    hash::{DefaultHasher, Hash, Hasher},
    io::Error as IoError,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    os::unix::io::{AsRawFd, FromRawFd, RawFd},
    rc::Rc,
    str::FromStr,
    sync::LazyLock,
    time::{Duration, Instant},
};

use mio::{
    Events, Interest, Poll, Token,
    net::{TcpListener as MioTcpListener, TcpStream, UdpSocket as MioUdpSocket},
};
use slab::Slab;
use sozu_command::{
    channel::Channel,
    config::MetricDetailLevel,
    logging,
    proto::command::{
        ActivateListener, AddBackend, CertificatesWithFingerprints, Cluster, ClusterHashes,
        ClusterInformations, DeactivateListener, Event, EventKind, HttpListenerConfig,
        HttpsListenerConfig, InitialState, ListenerType, LoadBalancingAlgorithms, LoadMetric,
        MetricDetail, MetricsConfiguration, RemoveBackend, Request, ResponseContent,
        ResponseStatus, ServerConfig, TcpListenerConfig as CommandTcpListener,
        UdpListenerConfig as CommandUdpListener, UpdateHttpListenerConfig,
        UpdateHttpsListenerConfig, UpdateTcpListenerConfig, UpdateUdpListenerConfig, WorkerRequest,
        WorkerResponse, request::RequestType, response_content::ContentType,
    },
    ready::Ready,
    scm_socket::{Listeners, ScmSocket, ScmSocketError},
    state::ConfigState,
};

use crate::metrics::names;
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
    udp,
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

/// Build the `WorkerMetricDetailStatus` content payload returned in
/// every successful `SetMetricDetail` worker response. The master
/// collects these across the fan-out and assembles them into
/// `MetricDetailStatus.workers[<worker_id>]` so the TUI sees each
/// worker's actual aggregator state instead of the master's view.
fn worker_metric_detail_status_content(
    configured: MetricDetailLevel,
    effective: MetricDetailLevel,
    previous_effective: MetricDetailLevel,
    active_lease_count: u32,
) -> ResponseContent {
    use sozu_command::proto::command::WorkerMetricDetailStatus;
    ContentType::WorkerMetricDetailStatus(WorkerMetricDetailStatus {
        configured: MetricDetail::from(configured) as i32,
        effective: MetricDetail::from(effective) as i32,
        previous_effective: MetricDetail::from(previous_effective) as i32,
        active_lease_count,
    })
    .into()
}

/// Build a `METRIC_DETAIL_CHANGED` event carrying the worker-local
/// transition payload (previous/effective levels + transition kind).
/// `client_id` is `Some(_)` for explicit apply/clear, `None` for the
/// polled janitor's bulk expiry. The master folds this Event into the
/// audit log alongside operator-initiated transitions emitted at the
/// dispatch site in `bin/src/command/requests.rs::worker_request`.
fn push_metric_detail_transition(
    previous: MetricDetailLevel,
    effective: MetricDetailLevel,
    transition_kind: &'static str,
    client_id: Option<String>,
) {
    use sozu_command::proto::command::MetricDetailTransition;
    // No-op when nothing actually changed. Defence-in-depth — every
    // caller already gates on `previous != effective`, but
    // double-checking here means future call sites can't accidentally
    // emit a "ghost" transition.
    if previous == effective {
        return;
    }
    push_event(Event {
        kind: EventKind::MetricDetailChanged as i32,
        cluster_id: None,
        backend_id: None,
        address: None,
        metric_detail: Some(MetricDetailTransition {
            previous_effective: MetricDetail::from(previous) as i32,
            effective: MetricDetail::from(effective) as i32,
            transition_kind: transition_kind.to_owned(),
            client_id,
        }),
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
    /// `Router::plan_connect` resolves to a fresh `(cluster, ip)` pair, and
    /// decremented when the session closes. Empty when the feature is
    /// unused.
    ///
    /// ── Why nested maps instead of `HashMap<(String, IpAddr), usize>` ──
    ///
    /// The per-request hot path (`cluster_ip_at_limit`, called from
    /// `mux/router::plan_connect` for every cluster-resolving request) used
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
    /// `mux/router::plan_connect`. The nested-map storage lets both lookups
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
        // Pure query: the limit==0 branch already returned, so any work below
        // runs only with a positive cap.
        debug_assert!(
            limit > 0,
            "limit==0 (unlimited) must have returned before reaching the bounded check"
        );
        let already_tracked = self
            .cluster_ip_tracks
            .get(&token)
            .and_then(|by_cluster| by_cluster.get(cluster_id))
            .is_some_and(|ips| ips.contains(ip));
        if already_tracked {
            // Reverse-index/forward-count coherence: if this token already
            // holds a slot for (cluster, ip), the forward count must be > 0
            // (decrement-to-zero reaps the inner entry in untrack_all).
            debug_assert!(
                self.connections_per_cluster_ip
                    .get(cluster_id)
                    .and_then(|by_ip| by_ip.get(ip))
                    .is_some_and(|c| *c > 0),
                "a tracked (token, cluster, ip) slot must have a positive forward count"
            );
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
        // Snapshot the forward count for this (cluster, ip) before the insert
        // so we can pair-assert the delta. Ungated `let`: read only inside the
        // debug_assert! below → optimised out in release (no E0425).
        let count_before = self
            .connections_per_cluster_ip
            .get(&cluster_id)
            .and_then(|by_ip| by_ip.get(&ip))
            .copied()
            .unwrap_or(0);
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
                .entry(cluster_id.clone())
                .or_default()
                .entry(ip)
                .or_insert(0) += 1;
        }
        // Postconditions: the reverse index now records this (token, cluster,
        // ip), and the forward count advanced by exactly `inserted as usize`
        // (idempotent: a repeat call for the same triple is a no-op on both).
        debug_assert!(
            self.cluster_ip_tracks
                .get(&token)
                .and_then(|by_cluster| by_cluster.get(&cluster_id))
                .is_some_and(|ips| ips.contains(&ip)),
            "track must leave the (token, cluster, ip) recorded in the reverse index"
        );
        debug_assert_eq!(
            self.connections_per_cluster_ip
                .get(&cluster_id)
                .and_then(|by_ip| by_ip.get(&ip))
                .copied()
                .unwrap_or(0),
            count_before + inserted as usize,
            "forward count must advance by exactly 1 on first track, 0 on a repeat"
        );
        #[cfg(debug_assertions)]
        self.check_invariants();
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
        // The reverse index for this token was just drained by `remove`; no
        // other code path re-inserts it within this call.
        debug_assert!(
            !self.cluster_ip_tracks.contains_key(&token),
            "untrack_all must evict the token from the reverse index"
        );
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
        // No orphan bookkeeping survives: the forward map must not retain a
        // cluster with an empty inner ip-map, nor an ip whose count is zero.
        debug_assert!(
            self.connections_per_cluster_ip
                .values()
                .all(|by_ip| !by_ip.is_empty() && by_ip.values().all(|&c| c > 0)),
            "untrack_all must not leave empty inner maps or zero-count ips behind"
        );
        #[cfg(debug_assertions)]
        self.check_invariants();
    }

    /// Wipe every per-(cluster, source-IP) accounting bucket. Called by
    /// the runtime `SetMaxConnectionsPerIp(0)` path so disabling the
    /// feature does not leave dead bookkeeping behind that a future
    /// re-enable would consult.
    pub fn clear_cluster_ip_tracking(&mut self) {
        self.cluster_ip_tracks.clear();
        self.connections_per_cluster_ip.clear();
        // Both halves of the per-(cluster, ip) accounting are now empty; a
        // future re-enable starts from a clean slate.
        debug_assert!(
            self.cluster_ip_tracks.is_empty() && self.connections_per_cluster_ip.is_empty(),
            "clear must wipe both the reverse index and the forward count map"
        );
        #[cfg(debug_assertions)]
        self.check_invariants();
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
        let threshold = 10 + 2 * self.max_connections;
        // The gate must leave headroom above the connection cap so listener /
        // system slots (Channel, Metrics, Timer, listeners) are never starved
        // by frontend connections alone.
        debug_assert!(
            threshold > self.max_connections,
            "accept gate must sit strictly above max_connections to reserve system slots"
        );
        threshold
    }

    /// Check the number of connections against max_connections, and the slab capacity.
    /// Returns false if limits are reached.
    pub fn check_limits(&mut self) -> bool {
        // Live-count invariant: the accounted connection count never exceeds
        // the configured cap (incr() enforces this, decr() never underflows).
        debug_assert!(
            self.nb_connections <= self.max_connections,
            "nb_connections must never exceed max_connections"
        );
        if self.nb_connections >= self.max_connections {
            error!("max number of session connection reached, flushing the accept queue");
            gauge!(names::accept_queue::BACKPRESSURE, 1);
            self.can_accept = false;
            // A negative result must have closed the accept gate.
            debug_assert!(
                !self.can_accept,
                "refusing at the cap must clear can_accept"
            );
            return false;
        }

        if self.at_capacity() {
            error!("not enough memory to accept another session, flushing the accept queue");
            error!(
                "nb_connections: {}, max_connections: {}",
                self.nb_connections, self.max_connections
            );
            gauge!(names::accept_queue::BACKPRESSURE, 1);
            self.can_accept = false;

            debug_assert!(
                !self.can_accept,
                "refusing at slab capacity must clear can_accept"
            );
            return false;
        }

        // A positive result means there is room under both gates.
        debug_assert!(
            self.nb_connections < self.max_connections && !self.at_capacity(),
            "check_limits returned room while a gate was actually saturated"
        );
        true
    }

    pub fn to_session(token: Token) -> SessionToken {
        SessionToken(token.0)
    }

    pub fn incr(&mut self) {
        let before = self.nb_connections;
        self.nb_connections += 1;
        assert!(self.nb_connections <= self.max_connections);
        // The counter advances by exactly one per accepted session.
        debug_assert_eq!(
            self.nb_connections,
            before + 1,
            "incr must raise nb_connections by exactly one"
        );
        // `client.connections_max` and `client.connections_percent` are
        // emitted from the run loop alongside `process.uptime_seconds` /
        // `server.live` so all proxy gauges advance in lock-step. Keeping
        // `client.connections` per-event preserves the high-resolution
        // signal scrapers expect.
        gauge!(names::client::CONNECTIONS, self.nb_connections);
    }

    /// Decrements the number of sessions, start accepting new connections
    /// if the capacity limit of 90% has not been reached.
    pub fn decr(&mut self) {
        assert!(self.nb_connections != 0);
        let before = self.nb_connections;
        self.nb_connections -= 1;
        // Mirror of incr: exactly one connection is released, no underflow.
        debug_assert_eq!(
            self.nb_connections,
            before - 1,
            "decr must lower nb_connections by exactly one"
        );
        gauge!(names::client::CONNECTIONS, self.nb_connections);

        // do not be ready to accept right away, wait until we get back to 10% capacity
        if !self.can_accept && self.nb_connections < self.max_connections * 90 / 100 {
            debug!(
                "nb_connections = {}, max_connections = {}, starting to accept again",
                self.nb_connections, self.max_connections
            );
            gauge!(names::accept_queue::BACKPRESSURE, 0);
            self.can_accept = true;
        }
    }

    /// Full cross-field invariant sweep for the session manager. Called as a
    /// `debug_assert!`-guarded postcondition from the mutating cluster-IP
    /// tracking methods. Asserts logic-bug conditions only — every clause
    /// holds on any well-formed manager regardless of traffic.
    #[cfg(debug_assertions)]
    fn check_invariants(&self) {
        // 1. Live connection count never exceeds the configured cap.
        debug_assert!(
            self.nb_connections <= self.max_connections,
            "nb_connections {} exceeds max_connections {}",
            self.nb_connections,
            self.max_connections
        );
        // 2. The forward count map never retains an empty inner ip-map or a
        //    zero count (both are reaped on the last untrack).
        debug_assert!(
            self.connections_per_cluster_ip
                .values()
                .all(|by_ip| !by_ip.is_empty() && by_ip.values().all(|&c| c > 0)),
            "connections_per_cluster_ip holds an empty inner map or a zero count"
        );
        // 3. Reverse-index → forward-count coherence: every (token, cluster,
        //    ip) recorded in the reverse index must have a positive forward
        //    count. The forward count is the sum of contributing tokens, so a
        //    tracked slot can never point at a missing/zero count.
        debug_assert!(
            self.cluster_ip_tracks.values().all(|by_cluster| {
                by_cluster.iter().all(|(cluster_id, ips)| {
                    ips.iter().all(|ip| {
                        self.connections_per_cluster_ip
                            .get(cluster_id)
                            .and_then(|by_ip| by_ip.get(ip))
                            .is_some_and(|&c| c > 0)
                    })
                })
            }),
            "a tracked (token, cluster, ip) slot has no positive forward count"
        );
        // 4. The reverse index never retains an empty inner structure.
        debug_assert!(
            self.cluster_ip_tracks.values().all(|by_cluster| {
                !by_cluster.is_empty() && by_cluster.values().all(|ips| !ips.is_empty())
            }),
            "cluster_ip_tracks retains an empty per-token or per-cluster entry"
        );
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
    udp: Rc<RefCell<udp::UdpProxy>>,
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

        // UDP proxy is constructed internally (no constructor parameter) so the
        // public `Server::new` / `try_new_from_config` signatures stay
        // unchanged — bin/ is not forced to change for construction.
        let udp = Rc::new(RefCell::new({
            let registry = poll
                .registry()
                .try_clone()
                .map_err(ServerError::CloneRegistry)?;

            udp::UdpProxy::new(
                registry,
                sessions.clone(),
                pool.clone(),
                backends.clone(),
                server_config.max_connections as usize,
                server_config.buffer_size as usize,
            )
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
            udp,
            zombie_check_interval: Duration::from_secs(u64::from(
                server_config.zombie_check_interval,
            )),
        };

        // Take the retiring worker's listening sockets BEFORE applying the
        // initial state, because that state is what adopts them.
        //
        // `fork_main_into_worker` (`bin/src/worker.rs`) sends the descriptors
        // over the SCM socket right after the fork — before the main process
        // sends the initial `Status` request — and
        // `ConfigState::produce_initial_state` carries one `ActivateListener`
        // per active listener. Receiving them afterwards left `scm_listeners`
        // empty for the whole initial state, so every `ActivateListener` took
        // the `server_bind` / `udp_bind` branch (which succeeds only because
        // of `SO_REUSEPORT`), the listener came up `active`, and the inherited
        // descriptor was then dropped unused by the next, short-circuiting
        // `activate()` — discarding the accept backlog (TCP/HTTP/HTTPS) and
        // the receive buffer (UDP) the hand-off exists to preserve (sozu#1342).
        //
        // Blocking here is the same blocking read as before, only earlier: the
        // sole production caller of `fork_main_into_worker`,
        // `CommandServer::launch_new_worker`, always passes `Some(listeners)`,
        // so a fresh start receives an empty manifest immediately and every
        // listener binds its own socket exactly as it always did.
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

        server.report_unclaimed_inherited_sockets();

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
            time!(
                names::event_loop::EPOLL_TIME,
                (after_epoll - self.loop_start).as_millis()
            );
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
                    token if self.udp.borrow().health_owns_token(token) => {
                        self.udp.borrow_mut().health_ready(token);
                    }
                    token => self.ready(token, Ready::from(event)),
                }
            }

            if let Some(t) = self.should_poll_at.as_ref()
                && *t <= Instant::now()
            {
                while let Some(t) = TIMER.with(|timer| timer.borrow_mut().poll()) {
                    //info!("polled for timeout: {:?}", t);
                    self.timeout(t);
                }
            }
            self.handle_remaining_readiness();
            self.create_sessions();

            self.should_poll_at = TIMER.with(|timer| timer.borrow().next_poll_date());

            self.zombie_check();
            self.health_checker
                .poll(&self.backends, self.poll.registry());
            // Drive the UDP endpoint health prober (TCP-probe + hysteresis +
            // fail-open). Non-blocking; no-op when no UDP cluster has health
            // configured.
            self.udp.borrow_mut().health_poll();

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

                gauge!(names::client::CONNECTIONS, nb_connections);
                gauge!(names::client::CONNECTIONS_MAX, max_connections);
                if let Some(percent) = (nb_connections * 100).checked_div(max_connections) {
                    gauge!(names::client::CONNECTIONS_PERCENT, percent);
                }

                gauge!(names::slab::ENTRIES, slab_len);
                gauge!(names::slab::CAPACITY, slab_capacity);
                if let Some(percent) = (slab_len * 100).checked_div(slab_capacity) {
                    gauge!(names::slab::USAGE_PERCENT, percent);
                }
                if let Some(percent) = (slab_len * 100).checked_div(accept_threshold) {
                    gauge!(names::slab::ACCEPT_THRESHOLD_PERCENT, percent);
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
                gauge!(names::buffer::IN_USE, used);
                gauge!(names::buffer::CAPACITY, capacity);
                if let Some(percent) = (used * 100).checked_div(capacity) {
                    gauge!(names::buffer::USAGE_PERCENT, percent);
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
                    incr!(names::accept_queue::SATURATED_SECONDS);
                }
                self.last_saturation_tick = now;
            }
            // Process / runtime gauges sampled once per loop iteration. Same
            // batch as `client.connections` so dashboards see them update in
            // lock-step.
            gauge!(
                names::process::UPTIME_SECONDS,
                self.started_at.elapsed().as_secs() as usize
            );
            // `server.live` flips to 0 once a graceful shutdown is requested,
            // matching Envoy's `server.live` semantics. L4 health checks
            // (HAProxy / cloud LBs) can poll this gauge to drain a worker
            // before the OS-level termination signal lands.
            gauge!(
                names::server::LIVE,
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
        time!(
            names::event_loop::EVENT_LOOP_TIME,
            (now - self.loop_start).as_millis()
        );

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
        // `now` is sampled this iteration and we only get here past the
        // interval gate, so the check timestamp advances monotonically.
        debug_assert!(
            now >= self.last_zombie_check,
            "zombie-check timestamp must never move backwards"
        );
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

        // Listen/system sessions report `Instant::now()` as their last event,
        // so `now - last_event` is ~0 and never exceeds the interval: a
        // listener can never be collected as a zombie. Assert the set is free
        // of listen protocols before we reap it.
        debug_assert!(
            !self.sessions.borrow().slab.iter().any(|(_, session)| {
                let s = session.borrow();
                zombie_tokens.contains(&s.frontend_token())
                    && matches!(
                        s.protocol(),
                        Protocol::HTTPListen | Protocol::HTTPSListen | Protocol::TCPListen
                    )
            }),
            "zombie reaping must never target a listener session"
        );

        let zombie_count = zombie_tokens.len() as i64;
        count!(names::misc::ZOMBIES, zombie_count);

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
                let slab_before = self.sessions.borrow().slab.len();
                let session = { self.sessions.borrow_mut().slab.remove(token.0) };
                session.borrow_mut().close();
                self.sessions.borrow_mut().decr();
                // The removed token is truly gone afterwards. The slab may shrink
                // by MORE than one: `close()` also frees the session's backend
                // slab slot(s) (the multi-token pattern), so assert it shrank by
                // at least the frontend slot we just removed, not exactly one.
                debug_assert!(
                    !self.sessions.borrow().slab.contains(token.0),
                    "removed token must be absent from the slab"
                );
                debug_assert!(
                    self.sessions.borrow().slab.len() < slab_before,
                    "removing a present session must free at least its own slab slot"
                );
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
        // Postcondition: no surviving slab entry still references any of the
        // closed frontend tokens — both the direct remove and the dangling
        // sweep together leave the slab clean of these sessions.
        debug_assert!(
            !self
                .sessions
                .borrow()
                .slab
                .iter()
                .any(|(_, session)| tokens.contains(&session.borrow().frontend_token())),
            "no slab entry may reference a closed frontend token after teardown"
        );
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
            if let Some(last) = self.last_shutting_down_message
                && (now - last) > Duration::from_secs(5)
            {
                info!(
                    "closed {} sessions, {} sessions left, base_sessions_count = {}",
                    sessions_count - new_sessions_count,
                    new_sessions_count,
                    self.base_sessions_count
                );
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

                    if self.channel.back_buf.available_data() > 0
                        && let Err(e) = self.channel.writable()
                    {
                        error!("error writing to channel: {:?}", e);
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
        // Polled lease-expiry janitor: SetMetricDetail leases self-expire after
        // their TTL so a crashed `sozu top` cannot permanently elevate metrics
        // cardinality. The janitor runs at most every LEASE_TICK_INTERVAL,
        // gated by `lease_tick_due` so the hot path of `notify` doesn't pay
        // the HashMap walk on every iteration. Single-threaded worker, so
        // `borrow_mut` is safe here. The same `now` reading is also threaded
        // into `lease_apply` below (`SetMetricDetail` arm) so the whole lease
        // lifecycle for this `notify` call is anchored to one clock read.
        let now = std::time::Instant::now();
        // Capture (previous, effective) before releasing the borrow so we
        // can emit an Event afterwards. Holding `METRICS.borrow_mut`
        // across `push_event` would re-enter the same thread-local from
        // inside `QUEUE.with` (safe but conceptually noisy); the
        // two-step split keeps the borrow scopes minimal.
        let lease_tick_transition = METRICS.with(|metrics| {
            let mut m = metrics.borrow_mut();
            if !m.lease_tick_due(now) {
                return None;
            }
            let previous = m.lease_tick(now)?;
            let effective = m.detail_effective();
            Some((previous, effective))
        });
        if let Some((previous, effective)) = lease_tick_transition {
            // The janitor retired one or more leases AND the effective
            // level moved. Surface the worker-local transition as an
            // Event so the master folds it into the audit log (closes
            // the gap where TUI-crashed lease expiry was previously
            // silent). `client_id` is `None` because the janitor may
            // have retired multiple leases at once.
            push_metric_detail_transition(previous, effective, "lease_tick_expired", None);
        }
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
            // Runtime cardinality lease verb — apply, renew, or clear a lease
            // on this worker's `Aggregator`. The lease bumps `effective` to
            // `max(configured, max(active leases))`; expiry runs on the polled
            // janitor below. Master-side aggregation into `MetricDetailStatus`
            // lands in a follow-up; for now the worker acks with a bare OK so
            // the existing `worker_request` fan-out path can collect.
            Some(RequestType::SetMetricDetail(req)) => {
                // Master populates the peer binding from the connecting
                // `ClientSession` before fan-out (`bin/src/command/
                // requests.rs::worker_request`). A pre-binding caller or a
                // platform without `SO_PEERCRED` yields `PeerBinding::default()`
                // — clears against that lease are accepted from anyone, per
                // the proto contract on `SetMetricDetail.peer_pid`.
                let presented_binding = crate::metrics::PeerBinding {
                    pid: req.peer_pid,
                    // Master sends Crockford-base32 ULIDs (`Ulid::to_string`);
                    // accept those, with a fallback hex parse for callers that
                    // happen to send `0x…` form. A failed parse degrades to
                    // `None` — the lease store treats that as "binding
                    // unknown" per the proto contract.
                    session_ulid: req.peer_session_ulid.as_deref().and_then(|s| {
                        rusty_ulid::Ulid::from_str(s)
                            .map(u128::from)
                            .ok()
                            .or_else(|| u128::from_str_radix(s.trim_start_matches("0x"), 16).ok())
                    }),
                };
                if req.clear.unwrap_or(false) {
                    // Defense-in-depth: the master pre-validates `client_id`
                    // length at the dispatch site, but worker IPC is not
                    // master-only — fuzz harnesses, serial_test-flagged
                    // integration tests, and future internal callers can
                    // issue an oversized clear directly. Mirror the apply
                    // path's `ClientIdTooLong` arm so an unbounded HashMap
                    // lookup is never driven by an operator-supplied string
                    // here either. The reason string echoes the byte length
                    // but not the operator bytes themselves (symmetric with
                    // the audit-column-smuggling guard on the apply path).
                    if req.client_id.len() > crate::metrics::LEASE_CLIENT_ID_MAX_BYTES {
                        let msg = format!(
                            "SetMetricDetail: clear client_id length {} exceeds {} bytes",
                            req.client_id.len(),
                            crate::metrics::LEASE_CLIENT_ID_MAX_BYTES,
                        );
                        error!("{}", msg);
                        push_queue(WorkerResponse::error(message.id.clone(), msg));
                        return;
                    }
                    // Capture transition fields + post-clear snapshot
                    // before releasing the borrow so we can emit an
                    // Event after AND build the WorkerMetricDetailStatus
                    // payload that the master folds into
                    // `MetricDetailStatus.workers[<worker_id>]`. Without
                    // this payload the master used its own view as a
                    // stand-in for the worker's per-aggregator state.
                    let (outcome, effective_after, configured_after, lease_count_after) = METRICS
                        .with(|metrics| {
                            let mut m = metrics.borrow_mut();
                            let outcome = m.lease_clear(&req.client_id, presented_binding);
                            (
                                outcome,
                                m.detail_effective(),
                                m.detail_configured(),
                                m.lease_count(),
                            )
                        });
                    match outcome {
                        crate::metrics::LeaseClearOutcome::Cleared { previous_effective } => {
                            push_metric_detail_transition(
                                previous_effective,
                                effective_after,
                                "lease_clear",
                                Some(req.client_id.clone()),
                            );
                            push_queue(WorkerResponse::ok_with_content(
                                message.id.clone(),
                                worker_metric_detail_status_content(
                                    configured_after,
                                    effective_after,
                                    previous_effective,
                                    lease_count_after,
                                ),
                            ));
                        }
                        crate::metrics::LeaseClearOutcome::NotFound => {
                            // Silent no-op: no lease existed for that
                            // id. The worker's state is unchanged so
                            // previous_effective == effective.
                            push_queue(WorkerResponse::ok_with_content(
                                message.id.clone(),
                                worker_metric_detail_status_content(
                                    configured_after,
                                    effective_after,
                                    effective_after,
                                    lease_count_after,
                                ),
                            ));
                        }
                        crate::metrics::LeaseClearOutcome::Unauthorized => {
                            // Do NOT echo `req.client_id` here: the operator-
                            // supplied bytes flow back through the master's
                            // worker→reason aggregation into the audit line's
                            // `reason=` column, which is sanitised for control
                            // bytes only. The dedicated `lease_id=` audit
                            // column already carries the operator string
                            // through `sanitize_for_audit_kv`, so re-embedding
                            // it here would let a value containing `,` or `=`
                            // forge a sibling KV pair against SIEM consumers
                            // that split on `, key=value`.
                            let msg = "SetMetricDetail: clear refused (peer \
                                 binding does not match the apply-time owner)"
                                .to_owned();
                            error!("{}", msg);
                            push_queue(WorkerResponse::error(message.id.clone(), msg));
                        }
                    }
                    return;
                }
                let detail_proto = match req.detail {
                    Some(d) => d,
                    None => {
                        // Operator-supplied `client_id` is intentionally
                        // omitted from the reason string: the dedicated
                        // `lease_id=` audit column carries it through the
                        // strict KV sanitiser. See the matching comment on
                        // the `Unauthorized` arm above for the column-
                        // smuggling rationale.
                        let msg = "SetMetricDetail without `detail` and without `clear`".to_owned();
                        error!("{}", msg);
                        push_queue(WorkerResponse::error(message.id.clone(), msg));
                        return;
                    }
                };
                let detail_enum = match MetricDetail::try_from(detail_proto) {
                    Ok(d) => d,
                    Err(e) => {
                        let msg =
                            format!("SetMetricDetail: invalid MetricDetail variant {detail_proto}");
                        error!("{}: {}", msg, e);
                        push_queue(WorkerResponse::error(message.id.clone(), msg));
                        return;
                    }
                };
                let level = MetricDetailLevel::from(detail_enum);
                // Bound the worst case BEFORE we touch the aggregator: the
                // proto contract on `SetMetricDetail.ttl_seconds` says the
                // worker rejects values larger than `LEASE_TTL_MAX` so a
                // stuck operator-side renewer (or a buggy third-party client)
                // cannot lock the worker into elevated cardinality. The
                // `Aggregator::lease_apply` clamp is still in place as a
                // defence-in-depth net for code paths that bypass this
                // dispatch (proto fuzzing, future internal callers).
                if let Some(t) = req.ttl_seconds
                    && u64::from(t) > crate::metrics::LEASE_TTL_MAX.as_secs()
                {
                    let msg = format!(
                        "SetMetricDetail: ttl_seconds={t} exceeds LEASE_TTL_MAX={}",
                        crate::metrics::LEASE_TTL_MAX.as_secs()
                    );
                    error!("{}", msg);
                    push_queue(WorkerResponse::error(message.id.clone(), msg));
                    return;
                }
                let ttl_seconds = req.ttl_seconds.filter(|&t| t > 0).unwrap_or_else(|| {
                    // The default fits in a u32 by construction
                    // (LEASE_TTL_DEFAULT = 60 s); the lossy `as u32` cast
                    // is replaced with a checked conversion so any
                    // future tweak past `u32::MAX` seconds (≈ 136 years)
                    // can't silently truncate. Falls through to 60 s on
                    // the theoretical overflow path.
                    u32::try_from(crate::metrics::LEASE_TTL_DEFAULT.as_secs()).unwrap_or(60)
                });
                let ttl = std::time::Duration::from_secs(ttl_seconds.into());
                let (outcome, configured_after, lease_count_after) = METRICS.with(|metrics| {
                    let mut m = metrics.borrow_mut();
                    let outcome =
                        m.lease_apply(req.client_id.clone(), level, ttl, presented_binding, now);
                    (outcome, m.detail_configured(), m.lease_count())
                });
                match outcome {
                    crate::metrics::LeaseApplyOutcome::Applied {
                        previous_effective,
                        new_effective,
                    } => {
                        push_metric_detail_transition(
                            previous_effective,
                            new_effective,
                            "lease_apply",
                            Some(req.client_id.clone()),
                        );
                        push_queue(WorkerResponse::ok_with_content(
                            message.id.clone(),
                            worker_metric_detail_status_content(
                                configured_after,
                                new_effective,
                                previous_effective,
                                lease_count_after,
                            ),
                        ));
                    }
                    crate::metrics::LeaseApplyOutcome::ClientIdTooLong => {
                        let msg = format!(
                            "SetMetricDetail: client_id length {} exceeds {} bytes",
                            req.client_id.len(),
                            crate::metrics::LEASE_CLIENT_ID_MAX_BYTES,
                        );
                        error!("{}", msg);
                        push_queue(WorkerResponse::error(message.id.clone(), msg));
                    }
                    crate::metrics::LeaseApplyOutcome::TableFull => {
                        // Same audit-column-smuggling guard as the
                        // `Unauthorized` and missing-detail arms: the
                        // operator-supplied `client_id` is already rendered
                        // safely through the strict KV sanitiser in the
                        // audit envelope's `lease_id=` column, so we keep
                        // it out of the reason string.
                        let msg = format!(
                            "SetMetricDetail: lease table at capacity ({} entries); reject new \
                             apply — operators must retry after an active lease expires or is \
                             cleared",
                            crate::metrics::LEASE_TABLE_CAP,
                        );
                        error!("{}", msg);
                        push_queue(WorkerResponse::error(message.id.clone(), msg));
                    }
                    crate::metrics::LeaseApplyOutcome::TtlOutOfRange => {
                        // Unreachable in the normal flow: the dispatch-time
                        // gate above already rejected ttl > LEASE_TTL_MAX.
                        // Surface explicitly so any future bypass (proto
                        // fuzzing, internal callers) fails loud rather
                        // than silently capping the lessor's intent.
                        let msg = format!(
                            "SetMetricDetail: ttl exceeds LEASE_TTL_MAX={} (internal contract \
                             violation: dispatch gate should have rejected)",
                            crate::metrics::LEASE_TTL_MAX.as_secs(),
                        );
                        error!("{}", msg);
                        push_queue(WorkerResponse::error(message.id.clone(), msg));
                    }
                    crate::metrics::LeaseApplyOutcome::Unauthorized => {
                        // A renewal arrived against an existing lease whose
                        // apply-time peer binding does not match the
                        // presented one. The `client_id` is intentionally
                        // omitted from the error string — the audit-log
                        // row already carries it in the dedicated
                        // `lease_id` column, and echoing it here would
                        // route operator-controlled bytes through the
                        // freeform reason field.
                        let msg = "SetMetricDetail: renewal refused (peer binding does not \
                                   match the apply-time owner)"
                            .to_owned();
                        error!("{}", msg);
                        push_queue(WorkerResponse::error(message.id.clone(), msg));
                    }
                }
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
            // A filter carrying a fingerprint is answered from this worker's
            // own `ConfigState` rather than from its resolver. Note that
            // `get_certificates` tests its domain arm FIRST, so a request
            // carrying BOTH filters is answered by exact SAN equality on the
            // domain and the fingerprint is ignored — the one path where
            // `--domain` is not the trie lookup `query_certificate_for_domain`
            // and `bin`'s `certificates_serving_domain` both perform
            // (sozu#1383). `QueryCertificatesFilters` documents that the two
            // filters do not compound; `doc/configure_cli.md` says to pass one
            // at a time.
            Some(RequestType::QueryCertificatesFromWorkers(filters))
                if filters.fingerprint.is_some() =>
            {
                let certs = self.config_state.get_certificates(filters.clone());
                let response = if !certs.is_empty() {
                    WorkerResponse::ok_with_content(
                        message.id.clone(),
                        ContentType::CertificatesWithFingerprints(CertificatesWithFingerprints {
                            certs,
                        })
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
                if let Some(hc) = cluster.health_check.as_ref()
                    && let Err(reason) = sozu_command::config::validate_health_check_config(hc)
                {
                    push_queue(worker_response_error(req_id, reason));
                    return;
                }
                self.add_cluster(cluster);
                // Re-arm the metric drain tombstone in case this cluster id
                // was previously removed — without this the drain would
                // continue dropping every emission for the resurrected
                // cluster. Idempotent on a fresh id.
                METRICS.with(|metrics| {
                    (*metrics.borrow_mut()).add_cluster(&cluster.cluster_id);
                });
                //not returning because the message must still be handled by each proxy
            }
            Some(RequestType::RemoveCluster(ref cluster_id)) => {
                self.remove_health_check_state(cluster_id);
                METRICS.with(|metrics| {
                    (*metrics.borrow_mut()).remove_cluster(cluster_id);
                });
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
        if proxy_destinations.to_udp_proxy {
            let udp_proxy_response = self.udp.borrow_mut().notify(request.clone());
            if udp_proxy_response.is_failure() || notify_response.is_none() {
                notify_response = Some(udp_proxy_response);
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
            Some(RequestType::AddUdpListener(listener)) => {
                push_queue(self.notify_add_udp_listener(&req_id, listener));
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
            Some(RequestType::UpdateUdpListener(patch)) => {
                push_queue(self.notify_update_udp_listener(&req_id, patch));
            }
            Some(RequestType::RemoveListener(ref remove)) => {
                debug!("{} remove {:?} listener {:?}", req_id, remove.proxy, remove);
                // We only remove a listener that was previously added, so the
                // base count is at least 1 — the subtraction cannot underflow.
                debug_assert!(
                    self.base_sessions_count > 0,
                    "removing a listener with base_sessions_count == 0 would underflow"
                );
                self.base_sessions_count -= 1;
                let listener_type = ListenerType::try_from(remove.proxy);
                // `RemoveListener` is the sole release point of the slab slot
                // `AddListener` reserved (see `reserve_listen_token`): no
                // proxy's `remove_listener` touches the session slab, so
                // without this the slot leaks one slab key per add/remove
                // cycle — and `base_sessions_count`, decremented just above,
                // drifts below the number of reserved slots. Read the token
                // BEFORE the proxy drops the listener that holds it.
                let address: std::net::SocketAddr = remove.address.into();
                let listen_token = match listener_type {
                    Ok(ListenerType::Http) => self.http.borrow().listener_token(address),
                    Ok(ListenerType::Https) => self.https.borrow().listener_token(address),
                    Ok(ListenerType::Tcp) => self.tcp.borrow().listener_token(address),
                    Ok(ListenerType::Udp) => self.udp.borrow().listener_token(address),
                    Err(_) => None,
                };
                let response = match listener_type {
                    Ok(ListenerType::Http) => self.http.borrow_mut().notify(request),
                    Ok(ListenerType::Https) => self.https.borrow_mut().notify(request),
                    Ok(ListenerType::Tcp) => self.tcp.borrow_mut().notify(request),
                    Ok(ListenerType::Udp) => self.udp.borrow_mut().notify(request),
                    Err(_) => WorkerResponse::error(req_id, "Wrong variant ListenerType"),
                };
                if let Some(token) = listen_token {
                    self.sessions.borrow_mut().slab.try_remove(token.0);
                    // Same rule as the deactivate arms: a listen token with no
                    // slab slot must not stay queued for a deferred accept.
                    self.accept_ready.remove(&ListenToken(token.0));
                    info!("released listen token {:?}", token);
                }
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
        // Runtime removal is address-keyed and drops every backend at this
        // address (A/B test, weighted variant, dedup race). The metrics
        // layer is id-keyed — fan out one `remove_backend` per actually-
        // removed id so the two identities stay in sync. Without this the
        // `backend_id` field on the IPC message could name "A" while the
        // runtime dropped both "A" and "B" at the same address, leaving
        // "B"'s metrics rows orphaned forever.
        let removed_ids = self
            .backends
            .borrow_mut()
            .remove_backend(&backend.cluster_id, &address);
        if removed_ids.is_empty() {
            // Edge case: BackendList returned nothing (address never
            // existed in this cluster). Honour the request's stated id
            // anyway so a no-op request still tidies any orphan metric
            // row from a prior identity-drift state.
            METRICS.with(|metrics| {
                (*metrics.borrow_mut()).remove_backend(&backend.cluster_id, &backend.backend_id);
            });
        } else {
            METRICS.with(|metrics| {
                let mut metrics = metrics.borrow_mut();
                for id in &removed_ids {
                    metrics.remove_backend(&backend.cluster_id, id);
                }
            });
        }

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
        // The vacant entry's key is free now and becomes the listener's token.
        let slab_before = session_manager.slab.len();
        debug_assert!(
            !session_manager
                .slab
                .contains(session_manager.slab.vacant_key()),
            "the next vacant slab key must be free before insertion"
        );
        let entry = session_manager.slab.vacant_entry();
        let token = Token(entry.key());

        match self.http.borrow_mut().add_listener(listener, token) {
            Ok(_token) => {
                entry.insert(Rc::new(RefCell::new(ListenSession {
                    protocol: Protocol::HTTPListen,
                })));
                // The listener session occupies exactly the token's slab key,
                // and the slab grew by exactly one slot.
                debug_assert!(
                    session_manager.slab.contains(token.0),
                    "listener insert must occupy the token's slab key"
                );
                debug_assert_eq!(
                    session_manager.slab.len(),
                    slab_before + 1,
                    "adding a listener must occupy exactly one slab slot"
                );
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
        let slab_before = session_manager.slab.len();
        debug_assert!(
            !session_manager
                .slab
                .contains(session_manager.slab.vacant_key()),
            "the next vacant slab key must be free before insertion"
        );
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
                debug_assert!(
                    session_manager.slab.contains(token.0),
                    "listener insert must occupy the token's slab key"
                );
                debug_assert_eq!(
                    session_manager.slab.len(),
                    slab_before + 1,
                    "adding a listener must occupy exactly one slab slot"
                );
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
        let slab_before = session_manager.slab.len();
        debug_assert!(
            !session_manager
                .slab
                .contains(session_manager.slab.vacant_key()),
            "the next vacant slab key must be free before insertion"
        );
        let entry = session_manager.slab.vacant_entry();
        let token = Token(entry.key());

        match self.tcp.borrow_mut().add_listener(listener, token) {
            Ok(_token) => {
                entry.insert(Rc::new(RefCell::new(ListenSession {
                    protocol: Protocol::TCPListen,
                })));
                debug_assert!(
                    session_manager.slab.contains(token.0),
                    "listener insert must occupy the token's slab key"
                );
                debug_assert_eq!(
                    session_manager.slab.len(),
                    slab_before + 1,
                    "adding a listener must occupy exactly one slab slot"
                );
                self.base_sessions_count += 1;
                WorkerResponse::ok(req_id)
            }
            Err(e) => worker_response_error(req_id, format!("Could not add TCP listener: {e}")),
        }
    }

    fn notify_add_udp_listener(
        &mut self,
        req_id: &str,
        listener: CommandUdpListener,
    ) -> WorkerResponse {
        debug!("{} add udp listener {:?}", req_id, listener);

        if self.sessions.borrow().at_capacity() {
            return worker_response_error(req_id, "session list is full, cannot add a listener");
        }

        let mut session_manager = self.sessions.borrow_mut();
        let entry = session_manager.slab.vacant_entry();
        let token = Token(entry.key());

        match self.udp.borrow_mut().add_listener(listener, token) {
            Ok(_token) => {
                entry.insert(Rc::new(RefCell::new(ListenSession {
                    protocol: Protocol::UDPListen,
                })));
                self.base_sessions_count += 1;
                WorkerResponse::ok(req_id)
            }
            Err(e) => worker_response_error(req_id, format!("Could not add UDP listener: {e}")),
        }
    }

    fn notify_update_udp_listener(
        &mut self,
        req_id: &str,
        patch: UpdateUdpListenerConfig,
    ) -> WorkerResponse {
        debug!("{} update udp listener {:?}", req_id, patch.address);
        match self.udp.borrow_mut().update_listener(patch) {
            Ok(()) => WorkerResponse::ok(req_id),
            Err(e) => worker_response_error(req_id, format!("Could not update UDP listener: {e}")),
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

    /// Re-arm a deactivated listener's slab slot with the inert
    /// [`ListenSession`] placeholder that `AddListener` first installed there.
    ///
    /// DESIGN (2026-09-18) — a listener owns exactly one slab slot for its
    /// whole `AddListener` -> `RemoveListener` lifetime, activated or not.
    /// The alternative was to stop retaining the token across a deactivate and
    /// allocate a fresh slab key on every activate; it was rejected because
    /// the token is the *identity* of a listener in all four proxies — their
    /// `listeners` maps (plus UDP's `managers`, `cluster_for_listener` and
    /// `listener_sessions`) are keyed by it, `give_back_listener` hands the
    /// same token back and `activate()` returns it again — so a fresh key
    /// would mean rekeying five maps across four proxies on every activate,
    /// far more machinery than the defect warrants.
    ///
    /// Reserving the slot is what makes the retained token honest, and it
    /// closes both halves of the defect at once:
    ///
    /// * the slot is never vacant, so `Server::ready` keeps dispatching the
    ///   listen token after a deactivate/activate cycle. Freeing it left the
    ///   socket re-registered under a token with no slab entry — `ready()`
    ///   silently drops the event, and the reactivated listener was deaf
    ///   while `ActivateListener` had reported success;
    /// * the slab can never hand that key to another session, so the UDP
    ///   activate path can no longer overwrite a live session with its
    ///   listener session. Mode 2 is gone by construction, not by check.
    ///
    /// `RemoveListener` is the sole release point (see `notify_proxys`). No
    /// proxy's `remove_listener` touches the session slab, so before this
    /// change the deactivate arms were the ONLY code that ever freed a
    /// listener's slot — removing a listener that had not been deactivated
    /// first already stranded its slab key (and, for UDP, the last
    /// `UdpListenerSession` reference, hence the listener's open `UdpSocket`).
    /// Reserving the slot here makes that release mandatory rather than
    /// incidental.
    fn reserve_listen_token(&mut self, token: Token, protocol: Protocol) {
        let mut sessions = self.sessions.borrow_mut();
        match sessions.slab.get_mut(token.0) {
            Some(slot) => {
                *slot = Rc::new(RefCell::new(ListenSession { protocol }));
                info!("reserved listen token {:?} for {:?}", token, protocol);
            }
            // Unreachable while the lifetime above holds: `AddListener`
            // inserted the slot and only `RemoveListener` frees it. Loud
            // rather than silent, because the listener is deaf if it happens.
            None => error!(
                "listen token {:?} ({:?}) has no slab slot to reserve; the listener would not be reachable if reactivated",
                token, protocol
            ),
        }
    }

    /// Say what the initial state left unclaimed in the SCM table.
    ///
    /// A descriptor is kept whenever no listener exists at its address yet,
    /// because an `AddListener` + `ActivateListener` pair can still arrive and
    /// adopt it — `UpgradeWorkerTask` (`bin/src/command/upgrade.rs`) scatters
    /// `generate_activate_requests()` only after `Server::new` has returned, so
    /// there is no point during startup at which a leftover is provably
    /// unclaimable. Retention is therefore open-ended, and the one thing this
    /// worker can do is make it visible.
    ///
    /// An address that already has a listener is the expected shape (an initial
    /// state carrying the listeners inactive, activated straight afterwards) and
    /// is reported at `info`. An address with no listener at all is the one an
    /// operator needs to see: nothing in this worker's state mentions it, so the
    /// descriptor is the retiring worker's listening socket, still bound with
    /// `SO_REUSEPORT` and registered with no event loop, which is expected to
    /// keep taking a share of new connections that nothing accepts. (That
    /// expectation follows from `SO_REUSEPORT` load-balancing semantics; nothing
    /// here measures it.) That is reported at `warn`.
    fn report_unclaimed_inherited_sockets(&self) {
        let Some(scm_listeners) = self.scm_listeners.as_ref() else {
            return;
        };
        let leftovers: [(&str, &Vec<(SocketAddr, RawFd)>); 4] = [
            ("HTTP", &scm_listeners.http),
            ("HTTPS", &scm_listeners.tls),
            ("TCP", &scm_listeners.tcp),
            ("UDP", &scm_listeners.udp),
        ];
        for (label, table) in leftovers {
            for (address, fd) in table {
                let has_listener = match label {
                    "HTTP" => self.http.borrow().listener_token(*address).is_some(),
                    "HTTPS" => self.https.borrow().listener_token(*address).is_some(),
                    "TCP" => self.tcp.borrow().listener_token(*address).is_some(),
                    _ => self.udp.borrow().listener_token(*address).is_some(),
                };
                if has_listener {
                    info!(
                        "inherited {} listening socket for {} (fd {}) is held for an \
                         ActivateListener this worker has not processed yet",
                        label, address, fd
                    );
                } else {
                    warn!(
                        "inherited {} listening socket for {} (fd {}) is held with no listener \
                         at that address: until an AddListener plus ActivateListener for {} \
                         adopts it, it is expected to keep taking a share of new connections \
                         that nothing accepts (inferred from SO_REUSEPORT semantics, not \
                         measured here)",
                        label, address, fd, address
                    );
                }
            }
        }
    }

    fn notify_activate_listener(
        &mut self,
        req_id: &str,
        activate: &ActivateListener,
    ) -> WorkerResponse {
        use crate::InheritedSocketFate;

        debug!(
            "{} activate {:?} listener {:?}",
            req_id, activate.proxy, activate
        );

        let address: std::net::SocketAddr = activate.address.into();

        match ListenerType::try_from(activate.proxy) {
            Ok(ListenerType::Http) => {
                let listener = match self.http.borrow().inherited_socket_fate(&address) {
                    InheritedSocketFate::Adopted => self
                        .scm_listeners
                        .as_mut()
                        .and_then(|listeners| listeners.get_http(&address))
                        // SAFETY: `fd` was just received from the supervisor via SCM_RIGHTS
                        // (see `command/src/scm_socket.rs`) and is not owned elsewhere — the
                        // `Listeners` table removes it on `get_http`. Ownership transfers to
                        // the mio wrapper, whose `Drop` closes the descriptor.
                        .map(|fd| unsafe { MioTcpListener::from_raw_fd(fd) }),
                    InheritedSocketFate::Refused => {
                        discard_inherited_socket::<MioTcpListener>(
                            self.scm_listeners
                                .as_mut()
                                .and_then(|listeners| listeners.get_http(&address)),
                            &address,
                            "HTTP",
                        );
                        None
                    }
                    // Left in the SCM table on purpose: a later `AddListener` +
                    // `ActivateListener` for this address can still adopt it.
                    InheritedSocketFate::Unclaimed => None,
                };

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
                let listener = match self.https.borrow().inherited_socket_fate(&address) {
                    InheritedSocketFate::Adopted => self
                        .scm_listeners
                        .as_mut()
                        .and_then(|listeners| listeners.get_https(&address))
                        // SAFETY: `fd` was just received from the supervisor via SCM_RIGHTS
                        // (see `command/src/scm_socket.rs`) and is not owned elsewhere — the
                        // `Listeners` table removes it on `get_https`. Ownership transfers to
                        // the mio wrapper, whose `Drop` closes the descriptor.
                        .map(|fd| unsafe { MioTcpListener::from_raw_fd(fd) }),
                    InheritedSocketFate::Refused => {
                        discard_inherited_socket::<MioTcpListener>(
                            self.scm_listeners
                                .as_mut()
                                .and_then(|listeners| listeners.get_https(&address)),
                            &address,
                            "HTTPS",
                        );
                        None
                    }
                    // Left in the SCM table on purpose: a later `AddListener` +
                    // `ActivateListener` for this address can still adopt it.
                    InheritedSocketFate::Unclaimed => None,
                };

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
                let listener = match self.tcp.borrow().inherited_socket_fate(&address) {
                    InheritedSocketFate::Adopted => self
                        .scm_listeners
                        .as_mut()
                        .and_then(|listeners| listeners.get_tcp(&address))
                        // SAFETY: `fd` was just received from the supervisor via SCM_RIGHTS
                        // (see `command/src/scm_socket.rs`) and is not owned elsewhere — the
                        // `Listeners` table removes it on `get_tcp`. Ownership transfers to
                        // the mio wrapper, whose `Drop` closes the descriptor.
                        .map(|fd| unsafe { MioTcpListener::from_raw_fd(fd) }),
                    InheritedSocketFate::Refused => {
                        discard_inherited_socket::<MioTcpListener>(
                            self.scm_listeners
                                .as_mut()
                                .and_then(|listeners| listeners.get_tcp(&address)),
                            &address,
                            "TCP",
                        );
                        None
                    }
                    // Left in the SCM table on purpose: a later `AddListener` +
                    // `ActivateListener` for this address can still adopt it.
                    InheritedSocketFate::Unclaimed => None,
                };

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
            Ok(ListenerType::Udp) => {
                let socket = match self.udp.borrow().inherited_socket_fate(&address) {
                    InheritedSocketFate::Adopted => self
                        .scm_listeners
                        .as_mut()
                        .and_then(|listeners| listeners.get_udp(&address))
                        // SAFETY: `fd` was just received from the supervisor via
                        // SCM_RIGHTS (see `command/src/scm_socket.rs`) and is not
                        // owned elsewhere — `Listeners::get_udp` removes it from
                        // the table. Ownership transfers to the mio `UdpSocket`
                        // wrapper, whose `Drop` closes the descriptor. `O_NONBLOCK`
                        // + `SO_REUSE*` are file-description flags preserved across
                        // SCM + exec.
                        .map(|fd| unsafe { MioUdpSocket::from_raw_fd(fd) }),
                    InheritedSocketFate::Refused => {
                        discard_inherited_socket::<MioUdpSocket>(
                            self.scm_listeners
                                .as_mut()
                                .and_then(|listeners| listeners.get_udp(&address)),
                            &address,
                            "UDP",
                        );
                        None
                    }
                    // Left in the SCM table on purpose: a later `AddListener` +
                    // `ActivateListener` for this address can still adopt it.
                    InheritedSocketFate::Unclaimed => None,
                };

                let activated_token = self.udp.borrow_mut().activate_listener(&address, socket);
                match activated_token {
                    Ok(token) => {
                        // UDP never uses accept()/create_session: replace the
                        // `ListenSession` placeholder at the listener token with
                        // the real `UdpListenerSession` so the READABLE
                        // registration drives `Server::ready`'s generic path
                        // into `UdpListenerSession::update_readiness`.
                        //
                        // Install it once per ACTIVATION, not once per request.
                        // `Ok(token)` does not mean this call activated
                        // anything: `UdpListener::activate` short-circuits on
                        // its own `active` flag and answers `Ok(self.token)` for
                        // a listener that is already up. Nothing upstream
                        // filters that repeat out either —
                        // `ConfigState::activate_listener` sets `active = true`
                        // and answers `Ok(())` however often it is asked, so the
                        // main process forwards a second
                        // `sozu listener udp activate`, and
                        // `ConfigState::generate_requests` re-emits
                        // `ActivateListener` for every active listener on each
                        // state replay.
                        //
                        // Rebuilding on a repeat would therefore overwrite a
                        // LIVE session: the proxy keeps the shared `UdpManager`
                        // and its flow table, but the replacement session starts
                        // with empty `upstream_sockets` / `upstream_to_flow` /
                        // `flow_to_upstream`, so every in-flight flow stops
                        // forwarding and its upstream slab slot can never be
                        // released (`on_close_flow` reaches it only through
                        // `flow_to_upstream`). `close()` is a `ProxySession`
                        // method and not `Drop`, so dropping the displaced
                        // session runs no teardown on the way out.
                        //
                        // `has_listener_session` is the precise "already
                        // installed" test — `build_session` is its only
                        // producer, and deactivate / remove / soft+hard stop are
                        // its only consumers — paired with the reserved slab
                        // slot still being present, so a slot that vanished
                        // still falls through to the invariant-break report
                        // below. It deliberately tracks the INSTALLED session
                        // and not the listener's `active` flag: the SCM
                        // hand-off's `give_back_listeners` clears `active` and
                        // takes the socket without removing the session, and
                        // skipping the rebuild there is right — the retained
                        // session reaches the freshly bound socket through the
                        // shared `Rc<RefCell<UdpListener>>`, so its flows
                        // survive the re-exec instead of being orphaned. A repeat then answers `ok` having touched
                        // nothing: the listener is active and its session is in
                        // the slab, which is exactly what the caller asked for,
                        // and it matches what the HTTP/HTTPS/TCP arms already
                        // answer for their own repeats.
                        let session_installed = self.udp.borrow().has_listener_session(token)
                            && self.sessions.borrow().slab.contains(token.0);
                        if !session_installed {
                            // The slot is reserved for the listener's whole
                            // lifetime (`reserve_listen_token`), so both failures
                            // below are invariant breaks; report them instead of
                            // returning `ok` for a listener whose socket is
                            // registered under a token `Server::ready` would
                            // ignore.
                            let session = match self.udp.borrow_mut().build_session(token) {
                                Some(session) => session,
                                None => {
                                    return worker_response_error(
                                        req_id,
                                        format!(
                                            "Could not build the UDP listener session for {address}"
                                        ),
                                    );
                                }
                            };
                            let mut sessions = self.sessions.borrow_mut();
                            match sessions.slab.get_mut(token.0) {
                                Some(slot) => *slot = session,
                                None => {
                                    return worker_response_error(
                                        req_id,
                                        format!(
                                            "UDP listener {address} has no session slot at {token:?}"
                                        ),
                                    );
                                }
                            }
                        }
                        WorkerResponse::ok(req_id)
                    }
                    Err(activate_error) => worker_response_error(
                        req_id,
                        format!("Could not activate UDP listener: {activate_error}"),
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

                // The slot stays RESERVED for the deactivated listener — see
                // `reserve_listen_token`. It carries the same inert placeholder
                // `AddListener` installed, so a later `ActivateListener` finds
                // its token still valid and no other session can take the key.
                self.reserve_listen_token(token, Protocol::HTTPListen);
                // The listen token may still be queued for a deferred accept
                // (`ready()` enqueues it whenever the listener is readable but
                // `can_accept` is false). The listener has just given its
                // socket back, so `handle_remaining_readiness` must not call
                // `accept()` on it.
                self.accept_ready.remove(&ListenToken(token.0));

                if deactivate.to_scm {
                    self.unblock_scm_socket();
                    let listeners = Listeners {
                        http: vec![(address, listener.as_raw_fd())],
                        tls: vec![],
                        tcp: vec![],
                        udp: vec![],
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
                // See the HTTP arm: the slot stays reserved, and a
                // socket-less listener must not stay queued for an accept.
                self.reserve_listen_token(token, Protocol::HTTPSListen);
                self.accept_ready.remove(&ListenToken(token.0));

                if deactivate.to_scm {
                    self.unblock_scm_socket();
                    let listeners = Listeners {
                        http: vec![],
                        tls: vec![(address, listener.as_raw_fd())],
                        tcp: vec![],
                        udp: vec![],
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
                // See the HTTP arm: the slot stays reserved, and a
                // socket-less listener must not stay queued for an accept.
                self.reserve_listen_token(token, Protocol::TCPListen);
                self.accept_ready.remove(&ListenToken(token.0));

                if deactivate.to_scm {
                    self.unblock_scm_socket();
                    let listeners = Listeners {
                        http: vec![],
                        tls: vec![],
                        tcp: vec![(address, listener.as_raw_fd())],
                        udp: vec![],
                    };
                    info!("sending TCP listener: {:?}", listeners);
                    let res = self.scm.send_listeners(&listeners);

                    self.block_scm_socket();

                    info!("sent TCP listener: {:?}", res);
                }
                WorkerResponse::ok(req_id)
            }
            Ok(ListenerType::Udp) => {
                let (token, mut listener) = match self.udp.borrow_mut().give_back_listener(address)
                {
                    Ok((token, listener)) => (token, listener),
                    Err(e) => {
                        return worker_response_error(
                            req_id,
                            format!(
                                "Could not deactivate UDP listener at address {address:?}: {e}"
                            ),
                        );
                    }
                };

                if let Err(e) = self.poll.registry().deregister(&mut listener) {
                    error!(
                        "error deregistering UDP listen socket({:?}): {:?}",
                        deactivate, e
                    );
                }
                // See the HTTP arm. For UDP this ALSO drops the live
                // `UdpListenerSession` the activate path installed here,
                // putting the slot back to the placeholder — the flows it owned
                // were already torn down by `give_back_listener`.
                self.reserve_listen_token(token, Protocol::UDPListen);
                // A UDP listen token never reaches `accept_ready` (`ready()`
                // only enqueues the three accept-driven listen protocols), but
                // the removal keeps the rule uniform across the four arms.
                self.accept_ready.remove(&ListenToken(token.0));

                if deactivate.to_scm {
                    self.unblock_scm_socket();
                    let listeners = Listeners {
                        http: vec![],
                        tls: vec![],
                        tcp: vec![],
                        udp: vec![(address, listener.as_raw_fd())],
                    };
                    info!("sending UDP listener: {:?}", listeners);
                    let res = self.scm.send_listeners(&listeners);

                    self.block_scm_socket();

                    info!("sent UDP listener: {:?}", res);
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

        let mut udp_listeners = self.udp.borrow_mut().give_back_listeners();
        for &mut (_, ref mut sock) in udp_listeners.iter_mut() {
            if let Err(e) = self.poll.registry().deregister(sock) {
                error!("error deregistering UDP listen socket({:?}): {:?}", sock, e);
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
            udp: udp_listeners
                .iter()
                .map(|(addr, listener)| (*addr, listener.as_raw_fd()))
                .collect(),
        };
        // Each handed-back listener is collected exactly once: the assembled
        // fd lists mirror the give_back lists one-to-one (the maps above are
        // straight `.iter().map().collect()` with no filtering or dedup).
        debug_assert_eq!(
            listeners.http.len(),
            http_listeners.len(),
            "every HTTP listener must be collected exactly once"
        );
        debug_assert_eq!(
            listeners.tls.len(),
            https_listeners.len(),
            "every HTTPS listener must be collected exactly once"
        );
        debug_assert_eq!(
            listeners.tcp.len(),
            tcp_listeners.len(),
            "every TCP listener must be collected exactly once"
        );
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

        // Past the guard, `accepted_protocol` is one of the three listen
        // variants — the inner dispatch's `unreachable!` arm relies on this.
        debug_assert!(
            matches!(
                accepted_protocol,
                Protocol::TCPListen | Protocol::HTTPListen | Protocol::HTTPSListen
            ),
            "accept dispatch must run with a listen protocol only"
        );

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
                    incr!(names::listener::ACCEPTED_TOTAL);
                    incr!(proto_key);
                    if let Some(peer_addr) = peer.as_ref() {
                        incr!(per_source_bucket(peer_addr));
                    }
                    let queue_before = self.accept_queue.len();
                    self.accept_queue.push_back((
                        sock,
                        token,
                        accepted_protocol,
                        Instant::now(),
                        peer,
                    ));
                    // One accepted socket enqueues exactly one entry.
                    debug_assert_eq!(
                        self.accept_queue.len(),
                        queue_before + 1,
                        "each accepted socket must enqueue exactly one entry"
                    );
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

        gauge!(names::accept_queue::CONNECTIONS, self.accept_queue.len());
    }

    pub fn create_sessions(&mut self) {
        while let Some((sock, token, protocol, timestamp, _peer)) = self.accept_queue.pop_back() {
            let wait_time = Instant::now() - timestamp;
            time!(names::accept_queue::WAIT_TIME, wait_time.as_millis());
            if wait_time > self.accept_queue_timeout {
                incr!(names::accept_queue::TIMEOUT);
                continue;
            }

            if !self.sessions.borrow_mut().check_limits() {
                // The socket we just popped will not be served, plus every
                // remaining queued socket below `break` will time out.
                // `listener.connection_capped` counts the popped socket so
                // the counter aligns with `check_limits` invocations rather
                // than with queue depth at the time of refusal.
                incr!(names::listener::CONNECTION_CAPPED);

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

                count!(names::sessions::EVICTED, evicted as i64);
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
            // The accept path only ever enqueues listen protocols, so the
            // `_ => panic!` arm below is genuinely unreachable. Assert the set
            // here so a future enqueue regression trips in debug, not prod.
            debug_assert!(
                matches!(
                    protocol,
                    Protocol::TCPListen | Protocol::HTTPListen | Protocol::HTTPSListen
                ),
                "accept queue must only hold listen protocols, got {protocol:?}"
            );
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
            let nb_before = self.sessions.borrow().nb_connections;
            self.sessions.borrow_mut().incr();
            // A successfully created session bumps the live count by one.
            debug_assert_eq!(
                self.sessions.borrow().nb_connections,
                nb_before + 1,
                "create_sessions must account exactly one new connection per created session"
            );
        }

        gauge!(names::accept_queue::CONNECTIONS, self.accept_queue.len());
    }

    pub fn ready(&mut self, token: Token, events: Ready) {
        trace!("PROXY\t{:?} got events: {:?}", token, events);

        let session_token = token.0;
        if self.sessions.borrow().slab.contains(session_token) {
            //info!("sessions contains {:?}", session_token);
            let protocol = self.sessions.borrow().slab[session_token]
                .borrow()
                .protocol();
            // NOTE: `token` is NOT necessarily the session's frontend token. A
            // session is registered under BOTH its frontend and its backend slab
            // slots (the multi-token pattern: `connect_to_backend` inserts the
            // same session Rc under a second vacant key), so `ready()` is also
            // dispatched with the backend token, where `frontend_token() !=
            // session_token`. There is therefore no `token == frontend_token`
            // identity to assert here.
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
                // A listen token queued here while `can_accept` was false can
                // outlive its slab entry. `notify_deactivate_listener` no
                // longer frees that entry — it RESERVES the slot, re-arming it
                // with the inert `ListenSession` placeholder (see
                // `reserve_listen_token`) — so after a deactivate the lookup
                // below still succeeds and it is the `accept_ready` purge in
                // those arms that keeps a socket-less listener out of
                // `accept()`. `RemoveListener` is the sole release point of the
                // slot (`notify_proxys`), and it purges the token too, so a
                // token reaching this loop with no slab entry means those
                // purges were bypassed. Indexing the slab with it panicked the
                // worker, hence the fallible `get`. Purging it HERE as well as
                // at those sites is what keeps the loop finite: `accept()`
                // below only removes the token on `WouldBlock`/error, so a
                // `continue` without the removal would hand `iter().next()` the
                // same stale token forever.
                let protocol = self
                    .sessions
                    .borrow()
                    .slab
                    .get(token.0)
                    .map(|session| session.borrow().protocol());
                let Some(protocol) = protocol else {
                    error!(
                        "accept_ready holds listen token {:?}, which no longer has a session; dropping it",
                        token
                    );
                    self.accept_ready.remove(&token);
                    continue;
                };
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
                            | Protocol::UDPListen
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

/// Close an inherited listening socket this worker is not going to use, and
/// say so.
///
/// Reached only for [`crate::InheritedSocketFate::Refused`]: a listener is
/// already up at `address` on a socket of its own, so `activate()`
/// short-circuits and nothing consumes this descriptor. That is a statement
/// about now, not about the lifetime — `DeactivateListener` then
/// `ActivateListener` would bring the address back to `Adopted`, and because
/// `scm_listeners` is filled once and never refilled, closing here means such a
/// reactivation binds a fresh socket. See [`crate::InheritedSocketFate::Refused`]
/// for why that trade is taken: a queued backlog decays, whereas an unaccepted
/// socket in the address's `SO_REUSEPORT` group is expected to go on taking a
/// share of new connections for as long as it stays open. That expectation
/// follows from `SO_REUSEPORT` load-balancing semantics; nothing here measures
/// it.
///
/// The close is explicit rather than a dropped wrapper falling out of scope: it
/// is the same syscall either way, but this one is deliberate, attributable and
/// logged, which is precisely what sozu#1342 was missing.
fn discard_inherited_socket<T: FromRawFd>(fd: Option<RawFd>, address: &SocketAddr, protocol: &str) {
    let Some(fd) = fd else {
        return;
    };
    warn!(
        "closing the inherited {} listening socket for {}: this worker already has an active \
         listener there, so nothing will adopt this descriptor unless that listener is \
         deactivated and reactivated, and until then it is expected to keep taking a share of \
         new connections that nothing accepts (inferred from SO_REUSEPORT semantics, not \
         measured here)",
        protocol, address
    );
    // SAFETY: `fd` was just removed from the SCM table by `Listeners::get_*`,
    // which hands over ownership, so no other owner survives. The wrapper is
    // built only to be dropped: `Drop` is what performs the close.
    drop(unsafe { T::from_raw_fd(fd) });
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
mod state_error_log_tests {
    use sozu_command::state::StateError;

    #[test]
    fn worker_state_dispatch_error_sink_bounds_the_reason() {
        const REASON_SECRET: &str = "WORKER_STATE_ERROR_REASON_SECRET_SENTINEL";

        let reason = format!("{REASON_SECRET}{}", "x".repeat(4096));
        let reason_len = reason.len();
        let output = crate::capture_test_logs(move || {
            let error = StateError::InvalidTcpFrontend {
                address: "127.0.0.1:443"
                    .parse()
                    .expect("test TCP frontend address must parse"),
                reason,
            };
            error!("Could not execute order on config state: {}", error);
        });

        assert!(
            !output.contains(REASON_SECRET),
            "worker state-error sink leaked the reason: {output}"
        );
        assert!(
            output.contains(&format!("reason_bytes={reason_len}")),
            "worker state-error sink omitted bounded reason metadata: {output}"
        );
        assert!(
            output.len() <= 512,
            "worker state-error sink is not bounded: {} bytes",
            output.len()
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

#[cfg(test)]
mod accept_ready_tests {
    use sozu_command::{
        config::ListenerBuilder,
        proto::command::{DeactivateListener, ListenerType, SocketAddress},
    };

    use super::*;
    use crate::testing::{ServerParts, prebuild_server, provide_port};

    /// A worker holding one activated TCP listener, reachable through the very
    /// same `Server` surface the event loop drives. The listen token carries a
    /// `ListenSession` placeholder in the slab, exactly like
    /// `tcp::testing::start_tcp_worker` installs it.
    pub(super) fn server_with_tcp_listener() -> (Server, Token, SocketAddress) {
        let ServerParts {
            event_loop,
            registry,
            sessions,
            pool,
            backends,
            server_scm_socket,
            server_config,
            ..
        // `send_scm = true`: `Server::new` makes a BLOCKING
        // `receive_listeners()` call before it applies the initial state, so
        // the client side must have queued its (empty) listener set first --
        // exactly what `tcp::testing::start_tcp_worker` does.
        } = prebuild_server(16, 16384, true).expect("could not prebuild a test server");
        let (_command_channel, proxy_channel) =
            Channel::generate(1000, 10000).expect("could not generate a test channel");

        let address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let listener_config = ListenerBuilder::new_tcp(address)
            .to_tcp(None)
            .expect("could not build a TcpListenerConfig for the test");

        let listen_token = {
            let mut session_manager = sessions.borrow_mut();
            let entry = session_manager.slab.vacant_entry();
            let token = Token(entry.key());
            entry.insert(Rc::new(RefCell::new(ListenSession {
                protocol: Protocol::TCPListen,
            })));
            token
        };

        let mut tcp_proxy =
            tcp::TcpProxy::new(registry, sessions.clone(), pool.clone(), backends.clone());
        tcp_proxy
            .add_listener(listener_config, listen_token)
            .expect("could not add the test TCP listener");
        tcp_proxy
            .activate_listener(&address.into(), None)
            .expect("could not activate the test TCP listener");

        let server = Server::new(
            event_loop,
            proxy_channel,
            server_scm_socket,
            sessions,
            pool,
            backends,
            None,
            None,
            Some(tcp_proxy),
            server_config,
            None,
            false,
        )
        .expect("could not build the test server");

        // `_command_channel` is dropped here on purpose: nothing in these
        // tests reads the worker's side of the channel.
        (server, listen_token, address)
    }

    /// `ready()` queues a listen token in `accept_ready` whenever its listener
    /// is readable but the worker cannot accept (buffer-pool backpressure), and
    /// only `accept()` — which never runs while `can_accept` is false — takes
    /// it back out. Deactivating that listener hands its socket back, so
    /// without this purge the deferred accept pass called `accept()` on a
    /// socket-less listener (and, before the slot was reserved for the
    /// listener's whole lifetime, indexed a vacant slab key).
    ///
    /// To SEE THIS RED: drop `self.accept_ready.remove(&ListenToken(token.0))`
    /// from `notify_deactivate_listener`'s TCP arm — the last assertion below
    /// fails.
    #[test]
    fn deactivating_a_listener_drops_its_pending_accept_token() {
        let (mut server, listen_token, address) = server_with_tcp_listener();

        // The backpressure state: readable listener, worker at capacity.
        server.accept_ready.insert(ListenToken(listen_token.0));

        let response = server.notify_deactivate_listener(
            "test-deactivate",
            &DeactivateListener {
                address,
                proxy: ListenerType::Tcp as i32,
                to_scm: false,
            },
        );
        assert_eq!(
            response.status,
            ResponseStatus::Ok as i32,
            "deactivating an activated TCP listener must succeed: {response:?}"
        );
        assert!(
            server.sessions.borrow().slab.contains(listen_token.0),
            "deactivating a listener must KEEP its slab slot reserved — see \
             `reserve_listen_token`; the accept purge below is what stops the \
             deferred pass from accepting on a socket-less listener"
        );
        assert!(
            !server.accept_ready.contains(&ListenToken(listen_token.0)),
            "a deactivated listener must not stay queued for a deferred accept"
        );

        // The pass that used to panic.
        server.handle_remaining_readiness();
    }

    /// Structural safety for the deferred-accept pass itself: whatever else
    /// ever drops a listen token's slab entry, indexing the slab with a token
    /// that is no longer there must not take the worker down. Dropping the
    /// stale token is also what keeps THIS branch finite: `accept()` only
    /// removes a token on `WouldBlock`/error, so re-entering the loop without
    /// the removal would spin on the same vacant slot forever.
    ///
    /// To SEE THIS RED: restore `self.sessions.borrow().slab[token.0]` in
    /// `handle_remaining_readiness` — this test then panics with
    /// "invalid key".
    #[test]
    fn a_stale_accept_token_is_dropped_instead_of_panicking() {
        let (mut server, listen_token, _address) = server_with_tcp_listener();

        server.sessions.borrow_mut().slab.remove(listen_token.0);
        server.accept_ready.insert(ListenToken(listen_token.0));

        server.handle_remaining_readiness();

        assert!(
            server.accept_ready.is_empty(),
            "a listen token with no session must be dropped from accept_ready"
        );
    }
}
/// The listener slab slot is reserved for a listener's whole `AddListener` ->
/// `RemoveListener` lifetime. These tests pin both ends of that lifetime and
/// the deactivate/reactivate cycle in between; see `reserve_listen_token` for
/// the design and the alternative it rejects.
#[cfg(test)]
mod listener_lifecycle_tests {
    use std::net::UdpSocket as StdUdpSocket;

    use sozu_command::{
        config::ListenerBuilder,
        proto::command::{
            ActivateListener, DeactivateListener, ListenerType, LoadBalancingParams,
            RemoveListener, RequestUdpFrontend, SocketAddress, filtered_metrics,
        },
    };

    use super::accept_ready_tests::server_with_tcp_listener;
    use super::*;
    use crate::{
        metrics::{METRICS, names},
        testing::{ServerParts, prebuild_server, provide_port},
    };

    /// A worker with no listener at all, driven through `Server`'s own request
    /// surface. Same shape as `accept_ready_tests::server_with_tcp_listener`,
    /// minus the pre-installed listener — `Server::new` builds its own UDP
    /// proxy, which is the one these tests exercise.
    fn bare_server() -> Server {
        let ServerParts {
            event_loop,
            sessions,
            pool,
            backends,
            server_scm_socket,
            server_config,
            ..
        } = prebuild_server(16, 16384, true).expect("could not prebuild a test server");
        let (_command_channel, proxy_channel) =
            Channel::generate(1000, 10000).expect("could not generate a test channel");

        Server::new(
            event_loop,
            proxy_channel,
            server_scm_socket,
            sessions,
            pool,
            backends,
            None,
            None,
            None,
            server_config,
            None,
            false,
        )
        .expect("could not build the test server")
    }

    fn add_udp_listener(server: &mut Server, address: SocketAddress) -> Token {
        let listener_config = ListenerBuilder::new_udp(address)
            .to_udp(None)
            .expect("could not build a UdpListenerConfig for the test");
        let response = server.notify_add_udp_listener("test-add-udp", listener_config);
        assert_eq!(
            response.status,
            ResponseStatus::Ok as i32,
            "adding a UDP listener must succeed: {response:?}"
        );
        server
            .udp
            .borrow()
            .listener_token(address.into())
            .expect("the added UDP listener must own a token")
    }

    fn activate(server: &mut Server, address: SocketAddress, proxy: ListenerType) {
        let response = server.notify_activate_listener(
            "test-activate",
            &ActivateListener {
                address,
                proxy: proxy as i32,
                from_scm: false,
            },
        );
        assert_eq!(
            response.status,
            ResponseStatus::Ok as i32,
            "activating a {proxy:?} listener must succeed: {response:?}"
        );
    }

    fn deactivate(server: &mut Server, address: SocketAddress, proxy: ListenerType) {
        let response = server.notify_deactivate_listener(
            "test-deactivate",
            &DeactivateListener {
                address,
                proxy: proxy as i32,
                to_scm: false,
            },
        );
        assert_eq!(
            response.status,
            ResponseStatus::Ok as i32,
            "deactivating a {proxy:?} listener must succeed: {response:?}"
        );
    }

    /// The protocol recorded in the slab slot at `token`, and whether that slot
    /// holds the inert `ListenSession` placeholder. `ListenSession` reports
    /// `Token(0)` as its frontend token while a real `UdpListenerSession`
    /// reports its own listen token — the only way to tell the two apart, since
    /// both answer `Protocol::UDPListen`.
    fn slot_state(server: &Server, token: Token) -> Option<(Protocol, bool)> {
        let sessions = server.sessions.borrow();
        let session = sessions.slab.get(token.0)?;
        let session = session.borrow();
        Some((session.protocol(), session.frontend_token() == Token(0)))
    }

    /// FAILURE MODE 1, the whole point of this change: a deactivated listener
    /// that is activated again must be reachable from the event loop.
    ///
    /// The proxies retain the listener's token across a deactivate
    /// (`give_back_listener` and `activate()` both hand back the same one), and
    /// `Server::ready` ignores any token the slab does not contain. Freeing the
    /// slot on deactivate therefore left `ActivateListener` re-registering the
    /// socket under a dead token: the request answered `ok` and the listener
    /// never saw another readiness event.
    ///
    /// `can_accept` is forced to `false` so `ready()` parks the token in
    /// `accept_ready` instead of immediately draining it in `accept()` —
    /// membership there is the observable proof that the event reached the
    /// listener's dispatch path at all.
    ///
    /// To SEE THIS RED: in `notify_deactivate_listener`'s TCP arm, replace
    /// `self.reserve_listen_token(token, Protocol::TCPListen);` with
    /// `self.sessions.borrow_mut().slab.remove(token.0);` — the reactivated
    /// listener's readiness event is then dropped, and the assertion fails
    /// with "a reactivated listener must still be dispatched by `ready()`".
    #[test]
    fn a_reactivated_tcp_listener_is_reachable_from_the_event_loop() {
        let (mut server, listen_token, address) = server_with_tcp_listener();

        deactivate(&mut server, address, ListenerType::Tcp);
        activate(&mut server, address, ListenerType::Tcp);

        server.sessions.borrow_mut().can_accept = false;
        server.ready(listen_token, Ready::READABLE);

        assert!(
            server.accept_ready.contains(&ListenToken(listen_token.0)),
            "a reactivated listener must still be dispatched by `ready()`"
        );
    }

    /// The UDP half of failure mode 1. UDP never accepts, so its activate path
    /// installs a real `UdpListenerSession` at the listen token instead — which
    /// it skipped outright when the slot had been freed, leaving the socket
    /// registered against the placeholder-less token and every datagram
    /// unread, while the request still answered `ok`.
    ///
    /// To SEE THIS RED: in `notify_deactivate_listener`'s UDP arm, replace
    /// `self.reserve_listen_token(token, Protocol::UDPListen);` with
    /// `self.sessions.borrow_mut().slab.remove(token.0);` — the activate arm
    /// then returns an error response ("has no session slot"), so `activate()`
    /// panics on the status assertion.
    #[test]
    fn a_reactivated_udp_listener_gets_its_session_back() {
        let mut server = bare_server();
        let address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let listen_token = add_udp_listener(&mut server, address);

        assert_eq!(
            slot_state(&server, listen_token),
            Some((Protocol::UDPListen, true)),
            "an added, not-yet-activated UDP listener holds the placeholder"
        );

        activate(&mut server, address, ListenerType::Udp);
        assert_eq!(
            slot_state(&server, listen_token),
            Some((Protocol::UDPListen, false)),
            "activating must install the real UdpListenerSession"
        );

        deactivate(&mut server, address, ListenerType::Udp);
        activate(&mut server, address, ListenerType::Udp);

        assert_eq!(
            slot_state(&server, listen_token),
            Some((Protocol::UDPListen, false)),
            "reactivating must reinstall the real UdpListenerSession"
        );
    }

    /// FAILURE MODE 2, now impossible by construction: while the slot was
    /// freed on deactivate, the slab was free to hand that key to the next
    /// session, and the UDP activate path — which found `contains` true and
    /// assigned — overwrote that live session with its listener session.
    ///
    /// The reserved slot is never vacant, so `vacant_entry()` can no longer
    /// return it. This test pins that, and ONLY that: the session created while
    /// the listener is deactivated must land on a different key and survive the
    /// reactivate. It says nothing about what the activate arm writes into the
    /// listener's OWN key, because the reactivate here follows a deactivate and
    /// so takes the full activation path. The repeat with no deactivate in
    /// between — where the arm's answer is `Ok(token)` from
    /// `UdpListener::activate`'s `active` short-circuit and the listener's own
    /// key holds a LIVE `UdpListenerSession` — is FAILURE MODE 3, pinned by
    /// `a_duplicate_udp_activate_keeps_the_live_listener_session` and the three
    /// tests beside it. Naming this one for "a live session" hid that gap: it
    /// stayed green throughout.
    ///
    /// To SEE THIS RED: in `notify_deactivate_listener`'s UDP arm, replace
    /// `self.reserve_listen_token(token, Protocol::UDPListen);` with
    /// `self.sessions.borrow_mut().slab.remove(token.0);` — the interloper
    /// then takes the listener's key and the `assert_ne!` below fails with
    /// "a deactivated listener's key must not be handed to another session".
    #[test]
    fn a_deactivated_udp_listeners_key_is_not_handed_to_another_session() {
        let mut server = bare_server();
        let address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let listen_token = add_udp_listener(&mut server, address);
        activate(&mut server, address, ListenerType::Udp);
        deactivate(&mut server, address, ListenerType::Udp);

        // Whatever the worker admits next takes the slab's next vacant key.
        // `Protocol::Channel` is just a marker no listener ever reports.
        let interloper = {
            let mut sessions = server.sessions.borrow_mut();
            let entry = sessions.slab.vacant_entry();
            let key = entry.key();
            entry.insert(Rc::new(RefCell::new(ListenSession {
                protocol: Protocol::Channel,
            })));
            Token(key)
        };
        assert_ne!(
            interloper, listen_token,
            "a deactivated listener's key must not be handed to another session"
        );

        activate(&mut server, address, ListenerType::Udp);

        assert_eq!(
            slot_state(&server, interloper),
            Some((Protocol::Channel, true)),
            "reactivating a listener must not overwrite another session's slot"
        );
    }

    /// The cluster every UDP data-path test in this module routes through.
    const UDP_FLOW_CLUSTER: &str = "udp-duplicate-activate";

    /// The session Rc currently occupying `token`. Identity (`Rc::ptr_eq`)
    /// across an operation is the direct statement that the operation did not
    /// replace the live session — `UdpListenerSession`'s per-flow maps are
    /// private to `udp.rs`, so identity plus an end-to-end datagram are what
    /// this module can observe.
    fn slab_session(server: &Server, token: Token) -> Rc<RefCell<dyn ProxySession>> {
        server
            .sessions
            .borrow()
            .slab
            .get(token.0)
            .expect("the listener must own a slab slot")
            .clone()
    }

    /// The current value of the `udp.active_flows` gauge for this test thread.
    /// `METRICS` is a `thread_local!` and libtest gives each test its own
    /// thread, so this reads only what the test itself emitted.
    fn active_flows_gauge() -> i64 {
        METRICS.with(|metrics| {
            metrics
                .borrow_mut()
                .dump_local_proxy_metrics()
                .get(names::udp::ACTIVE_FLOWS)
                .and_then(|metric| match metric.inner {
                    Some(filtered_metrics::Inner::Gauge(value)) => Some(value as i64),
                    _ => None,
                })
                .unwrap_or(0)
        })
    }

    /// A worker holding one activated UDP listener, a frontend and one backend,
    /// plus the real client and backend sockets. The UDP data path is driven for
    /// real on loopback because the orphaning these tests pin — the shell's
    /// `upstream_to_flow` / `flow_to_upstream` / `upstream_sockets` maps going
    /// empty under a live `UdpManager` — is only observable end to end.
    fn udp_worker_with_backend(
        server: &mut Server,
        address: SocketAddress,
    ) -> (StdUdpSocket, StdUdpSocket) {
        server.notify_proxys(WorkerRequest {
            id: "test-add-udp-front".to_owned(),
            content: RequestType::AddUdpFrontend(RequestUdpFrontend {
                cluster_id: UDP_FLOW_CLUSTER.to_owned(),
                address,
                tags: Default::default(),
            })
            .into(),
        });

        let backend_socket =
            StdUdpSocket::bind("127.0.0.1:0").expect("could not bind the test UDP backend");
        backend_socket
            .set_read_timeout(Some(Duration::from_millis(500)))
            .expect("could not set the backend read timeout");
        let backend_address = backend_socket
            .local_addr()
            .expect("the test UDP backend must have a local address");

        server.notify_proxys(WorkerRequest {
            id: "test-add-udp-backend".to_owned(),
            content: RequestType::AddBackend(AddBackend {
                cluster_id: UDP_FLOW_CLUSTER.to_owned(),
                backend_id: format!("{UDP_FLOW_CLUSTER}-0"),
                address: backend_address.into(),
                load_balancing_parameters: Some(LoadBalancingParams::default()),
                sticky_id: None,
                backup: None,
            })
            .into(),
        });

        let client_socket =
            StdUdpSocket::bind("127.0.0.1:0").expect("could not bind the test UDP client");
        (client_socket, backend_socket)
    }

    /// Send one datagram from `client` to the listener and drive a single
    /// READABLE event through `Server::ready`, exactly as the event loop does.
    /// Returns what reached the backend within its read timeout, or `None` when
    /// nothing was forwarded. Loopback `send_to` has already queued the datagram
    /// on the listener socket by the time it returns, so one pass is enough.
    fn forward_one(
        server: &mut Server,
        listen_token: Token,
        listen_address: SocketAddress,
        client: &StdUdpSocket,
        backend: &StdUdpSocket,
        payload: &[u8],
    ) -> Option<Vec<u8>> {
        let target: std::net::SocketAddr = listen_address.into();
        client
            .send_to(payload, target)
            .expect("the test client must reach the UDP listener");
        server.ready(listen_token, Ready::READABLE);

        let mut received = [0u8; 64];
        backend
            .recv_from(&mut received)
            .ok()
            .map(|(len, _)| received[..len].to_vec())
    }

    /// FAILURE MODE 3, the residual of the listener-slot change: a SECOND
    /// `ActivateListener` for a listener that is already active must not rebuild
    /// its session.
    ///
    /// `UdpListener::activate` short-circuits on its own `active` flag and
    /// answers `Ok(self.token)` without doing any work, so `Ok(token)` does not
    /// mean this call activated anything — and the arm used to run
    /// `build_session` + `*slot = session` on that answer regardless.
    ///
    /// To SEE THIS RED: in `notify_activate_listener`'s UDP arm, drop the
    /// `if !session_installed` guard so `build_session` and the slab write run
    /// unconditionally again — the second activate then installs a fresh
    /// session and the `Rc::ptr_eq` assertion fails with "a repeated
    /// ActivateListener must not replace the live UDP listener session".
    #[test]
    fn a_duplicate_udp_activate_keeps_the_live_listener_session() {
        let mut server = bare_server();
        let address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let listen_token = add_udp_listener(&mut server, address);
        activate(&mut server, address, ListenerType::Udp);

        let installed = slab_session(&server, listen_token);

        let response = server.notify_activate_listener(
            "test-activate-twice",
            &ActivateListener {
                address,
                proxy: ListenerType::Udp as i32,
                from_scm: false,
            },
        );
        // The response an already-active listener must get. `ok` and not an
        // error: `ConfigState::activate_listener` answers `Ok(())` for the same
        // repeat, `generate_activate_requests` re-emits one per active listener
        // on every state replay, and the HTTP/HTTPS/TCP arms all answer `ok`
        // for their own repeats. Erroring here would fail an ordinary replay and
        // make the worker disagree with the state the main process persisted.
        assert_eq!(
            response.status,
            ResponseStatus::Ok as i32,
            "a repeated ActivateListener on an active UDP listener must answer ok: {response:?}"
        );
        assert_eq!(
            slot_state(&server, listen_token),
            Some((Protocol::UDPListen, false)),
            "the real UdpListenerSession must still be the one in the slab"
        );
        assert!(
            Rc::ptr_eq(&installed, &slab_session(&server, listen_token)),
            "a repeated ActivateListener must not replace the live UDP listener session"
        );
    }

    /// The consequence of that rebuild, end to end. The displaced session took
    /// the whole shell side of every flow with it — `upstream_sockets`,
    /// `upstream_to_flow`, `flow_to_upstream` — while the proxy kept the shared
    /// `UdpManager` and its flow table. `close()` is a `ProxySession` method and
    /// not `Drop`, so nothing tore those flows down; they simply stopped
    /// forwarding while `udp.active_flows` went on counting them.
    ///
    /// To SEE THIS RED: in `notify_activate_listener`'s UDP arm, drop the
    /// `if !session_installed` guard — the datagram sent after the duplicate
    /// activate then never reaches the backend and the assertion fails with
    /// "an in-flight UDP flow must survive a repeated ActivateListener".
    #[test]
    fn in_flight_udp_flows_survive_a_duplicate_udp_activate() {
        let mut server = bare_server();
        let address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let listen_token = add_udp_listener(&mut server, address);
        activate(&mut server, address, ListenerType::Udp);
        let (client, backend) = udp_worker_with_backend(&mut server, address);

        assert_eq!(
            forward_one(
                &mut server,
                listen_token,
                address,
                &client,
                &backend,
                b"before"
            )
            .as_deref(),
            Some(&b"before"[..]),
            "the flow must be established before the duplicate activate"
        );
        assert_eq!(
            active_flows_gauge(),
            1,
            "one established flow must be on the gauge"
        );

        activate(&mut server, address, ListenerType::Udp);

        // The gauge counts what the manager holds, and the manager is retained
        // across the rebuild — so the gauge alone cannot tell a live flow from
        // an orphaned one. Assert both together: the flow the gauge claims must
        // still be a flow the data path can serve.
        assert_eq!(
            active_flows_gauge(),
            1,
            "a repeated ActivateListener must neither open nor evict a flow"
        );
        assert_eq!(
            forward_one(
                &mut server,
                listen_token,
                address,
                &client,
                &backend,
                b"after"
            )
            .as_deref(),
            Some(&b"after"[..]),
            "an in-flight UDP flow must survive a repeated ActivateListener"
        );
    }

    /// The resource the rebuild strands for good. Every flow holds one extra
    /// slab slot for its upstream socket, and `on_close_flow` reaches that slot
    /// only through `flow_to_upstream`. A replacement session's map is empty, so
    /// the eventual manager-driven teardown frees the flow, balances the gauge —
    /// and leaves the upstream slot allocated for the worker's whole life.
    ///
    /// To SEE THIS RED: in `notify_activate_listener`'s UDP arm, drop the
    /// `if !session_installed` guard — the deactivate then leaves the upstream
    /// slot behind and the assertion fails with "tearing the listener down must
    /// release every per-flow slab slot".
    #[test]
    fn a_duplicate_udp_activate_strands_no_upstream_slab_slot() {
        let mut server = bare_server();
        let address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let listen_token = add_udp_listener(&mut server, address);
        activate(&mut server, address, ListenerType::Udp);
        let (client, backend) = udp_worker_with_backend(&mut server, address);

        // Baseline AFTER the listener exists: its own reserved slot is part of
        // `base_sessions_count` and is not what this test measures.
        let baseline = server.sessions.borrow().slab.len();

        assert!(
            forward_one(
                &mut server,
                listen_token,
                address,
                &client,
                &backend,
                b"flow"
            )
            .is_some(),
            "the flow must be established before the duplicate activate"
        );
        assert_eq!(
            server.sessions.borrow().slab.len(),
            baseline + 1,
            "an established flow owns exactly one upstream slab slot"
        );

        activate(&mut server, address, ListenerType::Udp);
        deactivate(&mut server, address, ListenerType::Udp);

        assert_eq!(
            server.sessions.borrow().slab.len(),
            baseline,
            "tearing the listener down must release every per-flow slab slot"
        );
        assert_eq!(
            active_flows_gauge(),
            0,
            "tearing the listener down must return the active-flows gauge to zero"
        );
    }

    /// The path that reaches this arm without anyone typing a command:
    /// `load_state` replays the state the main process persisted, and
    /// `ConfigState::generate_activate_requests` emits one `ActivateListener`
    /// per ACTIVE listener. For a worker whose listener is already up, that
    /// replay is a duplicate activate by construction — the ordinary case, not
    /// an operator mistake, which is why this arm answers `ok` instead of
    /// surfacing the repeat as an error.
    ///
    /// Driven through the real `ConfigState` rather than a hand-written
    /// `ActivateListener`, so the test breaks if that emission ever changes.
    ///
    /// To SEE THIS RED: in `notify_activate_listener`'s UDP arm, drop the
    /// `if !session_installed` guard — the replay then rebuilds the session and
    /// the `Rc::ptr_eq` assertion fails with "replaying ActivateListener must
    /// leave the live UDP listener session in place".
    #[test]
    fn replaying_activate_listener_is_a_clean_no_op_for_an_active_udp_listener() {
        let mut server = bare_server();
        let address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let listener_config = ListenerBuilder::new_udp(address)
            .to_udp(None)
            .expect("could not build a UdpListenerConfig for the test");

        // Through `notify_proxys` so the worker's own `ConfigState` records the
        // listener and its activation, exactly as a live worker's does.
        server.notify_proxys(WorkerRequest {
            id: "test-add-udp".to_owned(),
            content: RequestType::AddUdpListener(listener_config).into(),
        });
        server.notify_proxys(WorkerRequest {
            id: "test-activate".to_owned(),
            content: RequestType::ActivateListener(ActivateListener {
                address,
                proxy: ListenerType::Udp as i32,
                from_scm: false,
            })
            .into(),
        });
        let listen_token = server
            .udp
            .borrow()
            .listener_token(address.into())
            .expect("the added UDP listener must own a token");
        let installed = slab_session(&server, listen_token);

        let replayed: Vec<ActivateListener> = server
            .config_state
            .generate_activate_requests()
            .into_iter()
            .filter_map(|request| match request.request_type {
                Some(RequestType::ActivateListener(activate)) => Some(activate),
                _ => None,
            })
            .collect();
        assert_eq!(
            replayed.len(),
            1,
            "an active UDP listener must be replayed exactly once: {replayed:?}"
        );

        for activate in &replayed {
            let response = server.notify_activate_listener("test-replay", activate);
            assert_eq!(
                response.status,
                ResponseStatus::Ok as i32,
                "a replayed ActivateListener must answer ok: {response:?}"
            );
        }

        assert_eq!(
            slot_state(&server, listen_token),
            Some((Protocol::UDPListen, false)),
            "the replay must leave the real UdpListenerSession in the slab"
        );
        assert!(
            Rc::ptr_eq(&installed, &slab_session(&server, listen_token)),
            "replaying ActivateListener must leave the live UDP listener session in place"
        );
    }

    /// The other end of the lifetime. Reserving the slot across a deactivate is
    /// only sound if something releases it: no proxy's `remove_listener`
    /// touches the session slab, so `RemoveListener` is the sole release point.
    /// Without it each add/remove cycle would strand one slab key — and
    /// `base_sessions_count`, decremented on every `RemoveListener`, would
    /// drift below the number of occupied slots, so a soft stop would wait for
    /// sessions that do not exist.
    ///
    /// Three full cycles, because a leak is only visible as growth, and every
    /// other one removes the listener while it is still ACTIVATED: that path
    /// drops a live `UdpListenerSession` out of the slab — the Rc that, left
    /// there, also pinned the listener's `UdpSocket` open and kept `ready()`
    /// dispatching to a removed listener.
    ///
    /// To SEE THIS RED: drop the `if let Some(token) = listen_token` block from
    /// `notify_proxys`'s `RemoveListener` arm — the slab then grows by one slot
    /// per cycle and the assertion fails with "cycle 0: removing a listener
    /// must release its reserved slot".
    #[test]
    fn removing_a_listener_releases_its_reserved_slab_slot() {
        let mut server = bare_server();
        let baseline = server.sessions.borrow().slab.len();
        let base_sessions_count = server.base_sessions_count;

        for cycle in 0..3 {
            let address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
            let listen_token = add_udp_listener(&mut server, address);
            activate(&mut server, address, ListenerType::Udp);
            if cycle % 2 == 0 {
                deactivate(&mut server, address, ListenerType::Udp);
            }

            server.notify_proxys(WorkerRequest {
                id: "test-remove".to_owned(),
                content: RequestType::RemoveListener(RemoveListener {
                    address,
                    proxy: ListenerType::Udp as i32,
                })
                .into(),
            });

            assert!(
                !server.sessions.borrow().slab.contains(listen_token.0),
                "cycle {cycle}: removing a listener must release its reserved slot"
            );
            assert_eq!(
                server.sessions.borrow().slab.len(),
                baseline,
                "cycle {cycle}: the slab must return to its baseline occupancy"
            );
            assert_eq!(
                server.base_sessions_count, base_sessions_count,
                "cycle {cycle}: base_sessions_count must track the reserved slots"
            );
        }
    }
}

/// The SCM hand-off a worker upgrade performs: the retiring worker's listening
/// sockets travel to the new worker over the SCM socket, and the new worker
/// must ADOPT them instead of binding fresh ones.
#[cfg(test)]
mod scm_listener_handoff_tests {
    use std::{
        net::{TcpStream as StdTcpStream, UdpSocket as StdUdpSocket},
        os::fd::{IntoRawFd, RawFd},
    };

    use sozu_command::{config::ListenerBuilder, proto::command::SocketAddress};

    use super::*;
    use crate::{
        ProxyError,
        socket::{server_bind, udp_bind},
        testing::{ServerParts, prebuild_server, provide_port},
    };

    /// A `ConfigState` holding one ACTIVE listener per protocol, i.e. what the
    /// main process carries into `produce_initial_state` for a worker upgrade.
    fn state_with_four_active_listeners(
        http_address: SocketAddress,
        https_address: SocketAddress,
        tcp_address: SocketAddress,
        udp_address: SocketAddress,
    ) -> ConfigState {
        let mut state = ConfigState::new();
        let adds: Vec<(Request, SocketAddress, ListenerType)> = vec![
            (
                RequestType::AddHttpListener(
                    ListenerBuilder::new_http(http_address)
                        .to_http(None)
                        .expect("could not build the test HTTP listener config"),
                )
                .into(),
                http_address,
                ListenerType::Http,
            ),
            (
                RequestType::AddHttpsListener(
                    ListenerBuilder::new_https(https_address)
                        .to_tls(None)
                        .expect("could not build the test HTTPS listener config"),
                )
                .into(),
                https_address,
                ListenerType::Https,
            ),
            (
                RequestType::AddTcpListener(
                    ListenerBuilder::new_tcp(tcp_address)
                        .to_tcp(None)
                        .expect("could not build the test TCP listener config"),
                )
                .into(),
                tcp_address,
                ListenerType::Tcp,
            ),
            (
                RequestType::AddUdpListener(
                    ListenerBuilder::new_udp(udp_address)
                        .to_udp(None)
                        .expect("could not build the test UDP listener config"),
                )
                .into(),
                udp_address,
                ListenerType::Udp,
            ),
        ];
        for (add, address, proxy) in adds {
            state.dispatch(&add).expect("could not add the listener");
            state
                .dispatch(
                    &RequestType::ActivateListener(ActivateListener {
                        address,
                        proxy: proxy.into(),
                        from_scm: false,
                    })
                    .into(),
                )
                .expect("could not mark the listener active");
        }
        state
    }

    /// The SCM table for `label`, so a test can assert on retention per protocol.
    fn inherited_table<'a>(server: &'a Server, label: &str) -> &'a Vec<(SocketAddr, RawFd)> {
        let scm_listeners = server
            .scm_listeners
            .as_ref()
            .expect("scm_listeners must be present");
        match label {
            "HTTP" => &scm_listeners.http,
            "HTTPS" => &scm_listeners.tls,
            "TCP" => &scm_listeners.tcp,
            "UDP" => &scm_listeners.udp,
            other => panic!("unknown protocol label {other}"),
        }
    }

    /// Whether `fd` is still the listening socket bound to `port`.
    ///
    /// Deliberately identity-aware rather than a bare "is it open" check: the
    /// test binary runs its tests in parallel, so a descriptor closed here can
    /// be handed straight back out to an unrelated file opened by another test
    /// and a liveness-only probe would call that "still open". Every port in
    /// these tests comes from `provide_port()` and is therefore unique to one
    /// test, so `getsockname` answering with that exact port is proof the
    /// descriptor is still the same socket.
    fn descriptor_is_socket_on_port(fd: RawFd, port: u16) -> bool {
        let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        // SAFETY: `getsockname` writes at most `length` bytes into `storage`
        // and updates `length`; both outlive the call. A closed, reused or
        // non-socket descriptor answers -1, which is the answer under test.
        let result = unsafe {
            libc::getsockname(
                fd,
                (&raw mut storage).cast::<libc::sockaddr>(),
                &raw mut length,
            )
        };
        if result != 0 || storage.ss_family != libc::AF_INET as libc::sa_family_t {
            return false;
        }
        // SAFETY: the family check above proves the kernel filled a
        // `sockaddr_in`, which `sockaddr_storage` is aligned and sized for.
        let inet = unsafe { &*(&raw const storage).cast::<libc::sockaddr_in>() };
        u16::from_be(inet.sin_port) == port
    }

    /// The datagram queued on the inherited UDP socket before the hand-off.
    const QUEUED_DATAGRAM: &[u8] = b"queued-before-the-handoff";

    /// A worker that inherits its predecessor's listening sockets must adopt
    /// them, not bind its own and leave the inherited descriptors unused.
    ///
    /// `fork_main_into_worker` (`bin/src/worker.rs`) sends the retiring
    /// worker's descriptors over the SCM socket immediately after the fork,
    /// before the main process sends anything else, and
    /// `ConfigState::produce_initial_state` carries one `ActivateListener` per
    /// active listener. `Server::new` must therefore have received those
    /// descriptors BEFORE it applies the initial state: otherwise every
    /// `ActivateListener` takes the `server_bind` / `udp_bind` branch — which
    /// succeeds only because of `SO_REUSEPORT` — and the inherited descriptor
    /// is dropped unused, discarding the accept backlog (TCP/HTTP/HTTPS) and
    /// the receive buffer (UDP) the hand-off exists to preserve.
    ///
    /// The three TCP-family assertions read `accept_queue`, which
    /// `notify_activate_listener` fills through `Server::accept` right after a
    /// successful activation; the UDP assertion reads the listener's own
    /// socket, since UDP has no accept path.
    ///
    /// To SEE THIS RED: in `Server::new`, move the `receive_listeners()` block
    /// back below the `if let Some(state) = initial_state` block. All four
    /// assertions fail — each listener binds its own socket, and the queued
    /// connections and datagram stay on the abandoned descriptors.
    #[test]
    fn inherited_listener_sockets_are_adopted_by_the_initial_activation() {
        let ServerParts {
            event_loop,
            sessions,
            pool,
            backends,
            client_scm_socket,
            server_scm_socket,
            server_config,
            ..
        } = prebuild_server(16, 16384, false).expect("could not prebuild a test server");
        let (_command_channel, proxy_channel) =
            Channel::generate(1000, 10000).expect("could not generate a test channel");

        let http_address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let https_address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let tcp_address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let udp_address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());

        // The retiring worker's sockets, bound exactly the way a live worker
        // binds them.
        let http_listener =
            server_bind(http_address.into()).expect("could not bind the inherited HTTP socket");
        let https_listener =
            server_bind(https_address.into()).expect("could not bind the inherited HTTPS socket");
        let tcp_listener =
            server_bind(tcp_address.into()).expect("could not bind the inherited TCP socket");
        let udp_socket =
            udp_bind(udp_address.into()).expect("could not bind the inherited UDP socket");

        // Queue on each inherited socket exactly what the hand-off exists to
        // preserve: a completed connection in the accept backlog, and a
        // datagram in the receive buffer. The clients stay alive for the whole
        // test so the connections are not closed from under the backlog.
        let _http_client = StdTcpStream::connect::<SocketAddr>(http_address.into())
            .expect("could not queue a connection on the inherited HTTP socket");
        let _https_client = StdTcpStream::connect::<SocketAddr>(https_address.into())
            .expect("could not queue a connection on the inherited HTTPS socket");
        let _tcp_client = StdTcpStream::connect::<SocketAddr>(tcp_address.into())
            .expect("could not queue a connection on the inherited TCP socket");
        let datagram_sender =
            StdUdpSocket::bind("127.0.0.1:0").expect("could not bind the test datagram sender");
        datagram_sender
            .send_to(QUEUED_DATAGRAM, SocketAddr::from(udp_address))
            .expect("could not queue a datagram on the inherited UDP socket");

        // The main process's side of the hand-off: send the descriptors, then
        // close its own copies, exactly as `fork_main_into_worker` does.
        let listeners = Listeners {
            http: vec![(http_address.into(), http_listener.into_raw_fd())],
            tls: vec![(https_address.into(), https_listener.into_raw_fd())],
            tcp: vec![(tcp_address.into(), tcp_listener.into_raw_fd())],
            udp: vec![(udp_address.into(), udp_socket.into_raw_fd())],
        };
        client_scm_socket
            .send_listeners(&listeners)
            .expect("could not send the inherited listeners");
        listeners.close();

        // The state the main process writes for the new worker. Built through
        // `ConfigState` and `produce_initial_state` rather than hand-rolled, so
        // the emission order under test is the real generator's: this breaks if
        // `generate_requests` ever stops pairing each `AddXxxListener` with the
        // `ActivateListener` that follows it.
        let initial_state =
            state_with_four_active_listeners(http_address, https_address, tcp_address, udp_address)
                .produce_initial_state();
        assert_eq!(
            initial_state.requests.len(),
            8,
            "the generator must emit one Add and one Activate per active listener"
        );

        let server = Server::new(
            event_loop,
            proxy_channel,
            server_scm_socket,
            sessions,
            pool,
            backends,
            None,
            None,
            None,
            server_config,
            Some(initial_state),
            false,
        )
        .expect("could not build the test server");

        let accepted: Vec<Protocol> = server
            .accept_queue
            .iter()
            .map(|(_, _, protocol, _, _)| *protocol)
            .collect();
        assert!(
            accepted.contains(&Protocol::HTTPListen),
            "the connection queued on the inherited HTTP socket must survive the hand-off, got {accepted:?}"
        );
        assert!(
            accepted.contains(&Protocol::HTTPSListen),
            "the connection queued on the inherited HTTPS socket must survive the hand-off, got {accepted:?}"
        );
        assert!(
            accepted.contains(&Protocol::TCPListen),
            "the connection queued on the inherited TCP socket must survive the hand-off, got {accepted:?}"
        );

        let (_, adopted_udp_socket) = server
            .udp
            .borrow_mut()
            .give_back_listeners()
            .pop()
            .expect("the UDP listener must hold a socket after activation");
        let mut buffer = [0u8; 64];
        let received = adopted_udp_socket.recv_from(&mut buffer);
        match received {
            Ok((length, _)) => assert_eq!(
                &buffer[..length],
                QUEUED_DATAGRAM,
                "the datagram queued on the inherited UDP socket must survive the hand-off"
            ),
            Err(error) => panic!(
                "the datagram queued on the inherited UDP socket must survive the hand-off, got {error:?}"
            ),
        }
    }

    /// An `ActivateListener` for a listener that is ALREADY active must dispose
    /// of the inherited descriptor deliberately, not leave it in the table.
    ///
    /// `activate()` short-circuits on `if self.active`, so nothing adopts that
    /// descriptor while the listener holds the address — a deactivate plus
    /// reactivate would, which is why `InheritedSocketFate::Refused` documents
    /// the close as a trade rather than an impossibility. Leaving it in the SCM
    /// table is not neutral either: it is the retiring worker's listening
    /// socket, still bound with `SO_REUSEPORT` and registered with no event
    /// loop, which is expected to keep taking a share of new connections that
    /// nothing accepts. Nothing here measures that expectation — the assertion
    /// below checks only that the descriptor is gone from the table and closed.
    ///
    /// Receiving the SCM listeners before the initial state means the ordinary
    /// upgrade no longer reaches this state, so the test constructs it directly.
    ///
    /// To SEE THIS RED: make any arm's `InheritedSocketFate::Refused` branch
    /// return `None` without discarding — that protocol's descriptor stays in the
    /// table and open, and both assertions below fail. This guards the
    /// retain-everything regression; the ORIGINAL defect closed the descriptor
    /// here too, by dropping the wrapper, and is guarded by
    /// `inherited_listener_sockets_are_adopted_by_the_initial_activation`
    /// instead.
    #[test]
    fn a_repeated_activation_closes_the_descriptor_it_cannot_adopt_now() {
        let ServerParts {
            event_loop,
            sessions,
            pool,
            backends,
            server_scm_socket,
            server_config,
            ..
        } = prebuild_server(16, 16384, true).expect("could not prebuild a test server");
        let (_command_channel, proxy_channel) =
            Channel::generate(1000, 10000).expect("could not generate a test channel");

        let http_address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let https_address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let tcp_address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let udp_address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());

        // A worker whose four listeners are up on their OWN sockets: the initial
        // state activates them and `scm_listeners` is empty, every ordinary start.
        let initial_state =
            state_with_four_active_listeners(http_address, https_address, tcp_address, udp_address)
                .produce_initial_state();
        let mut server = Server::new(
            event_loop,
            proxy_channel,
            server_scm_socket,
            sessions,
            pool,
            backends,
            None,
            None,
            None,
            server_config,
            Some(initial_state),
            false,
        )
        .expect("could not build the test server");

        // Now hand that already-active worker a descriptor per address.
        let http_fd = server_bind(http_address.into())
            .expect("could not bind the spare HTTP socket")
            .into_raw_fd();
        let https_fd = server_bind(https_address.into())
            .expect("could not bind the spare HTTPS socket")
            .into_raw_fd();
        let tcp_fd = server_bind(tcp_address.into())
            .expect("could not bind the spare TCP socket")
            .into_raw_fd();
        let udp_fd = udp_bind(udp_address.into())
            .expect("could not bind the spare UDP socket")
            .into_raw_fd();
        {
            let scm_listeners = server
                .scm_listeners
                .as_mut()
                .expect("Server::new must have populated scm_listeners");
            scm_listeners.http.push((http_address.into(), http_fd));
            scm_listeners.tls.push((https_address.into(), https_fd));
            scm_listeners.tcp.push((tcp_address.into(), tcp_fd));
            scm_listeners.udp.push((udp_address.into(), udp_fd));
        }

        for (address, proxy) in [
            (http_address, ListenerType::Http),
            (https_address, ListenerType::Https),
            (tcp_address, ListenerType::Tcp),
            (udp_address, ListenerType::Udp),
        ] {
            let response = server.notify_activate_listener(
                "test-repeat",
                &ActivateListener {
                    address,
                    proxy: proxy.into(),
                    from_scm: false,
                },
            );
            assert_eq!(
                response.status,
                ResponseStatus::Ok as i32,
                "a repeated {proxy:?} activation must still answer ok: {response:?}"
            );
        }

        let scm_listeners = server
            .scm_listeners
            .as_ref()
            .expect("scm_listeners must still be present");
        for (label, table, fd, port) in [
            (
                "HTTP",
                &scm_listeners.http,
                http_fd,
                http_address.port as u16,
            ),
            (
                "HTTPS",
                &scm_listeners.tls,
                https_fd,
                https_address.port as u16,
            ),
            ("TCP", &scm_listeners.tcp, tcp_fd, tcp_address.port as u16),
            ("UDP", &scm_listeners.udp, udp_fd, udp_address.port as u16),
        ] {
            assert!(
                !table.iter().any(|(_, entry)| *entry == fd),
                "the inherited {label} descriptor this activation cannot adopt must leave the SCM table"
            );
            assert!(
                !descriptor_is_socket_on_port(fd, port),
                "the inherited {label} descriptor this activation cannot adopt must be closed, \
                 not left bound in the address's SO_REUSEPORT group, where it is expected — \
                 inferred, not measured here — to go on taking a share of new connections"
            );
        }
    }

    /// An inherited descriptor whose address has NO listener yet must stay in
    /// the SCM table, and a later `AddListener` + `ActivateListener` must adopt
    /// it — backlog and all.
    ///
    /// This is a real upgrade shape, not a hypothetical: `Worker::upgrade`
    /// (`e2e/src/sozu/worker.rs`) hands the new worker a state whose listeners
    /// are inactive and sends `generate_activate_requests()` afterwards, and
    /// `UpgradeWorkerTask` (`bin/src/command/upgrade.rs`) scatters those
    /// requests only once `Server::new` has returned. Closing leftovers at the
    /// end of the initial state would destroy exactly this case, which is why
    /// `InheritedSocketFate::Unclaimed` keeps them.
    ///
    /// To SEE THIS RED: make any arm's `InheritedSocketFate::Unclaimed` branch
    /// take the descriptor out of the table like the other two — the first
    /// activation consumes and closes it, so the listener added afterwards binds
    /// its own socket and that protocol's assertions fail.
    #[test]
    fn an_inherited_descriptor_with_no_listener_yet_is_adopted_by_a_later_activation() {
        let ServerParts {
            event_loop,
            sessions,
            pool,
            backends,
            server_scm_socket,
            server_config,
            ..
        } = prebuild_server(16, 16384, true).expect("could not prebuild a test server");
        let (_command_channel, proxy_channel) =
            Channel::generate(1000, 10000).expect("could not generate a test channel");

        let http_address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let https_address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let tcp_address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let udp_address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());

        let mut server = Server::new(
            event_loop,
            proxy_channel,
            server_scm_socket,
            sessions,
            pool,
            backends,
            None,
            None,
            None,
            server_config,
            None,
            false,
        )
        .expect("could not build the test server");

        // Four inherited descriptors and no listener anywhere. The TCP-family
        // ones each carry a completed connection so the adoption can be shown to
        // preserve the backlog, not merely the descriptor.
        let http_listener =
            server_bind(http_address.into()).expect("could not bind the inherited HTTP socket");
        let https_listener =
            server_bind(https_address.into()).expect("could not bind the inherited HTTPS socket");
        let tcp_listener =
            server_bind(tcp_address.into()).expect("could not bind the inherited TCP socket");
        let udp_socket =
            udp_bind(udp_address.into()).expect("could not bind the inherited UDP socket");
        let _http_client = StdTcpStream::connect::<SocketAddr>(http_address.into())
            .expect("could not queue a connection on the inherited HTTP socket");
        let _https_client = StdTcpStream::connect::<SocketAddr>(https_address.into())
            .expect("could not queue a connection on the inherited HTTPS socket");
        let _tcp_client = StdTcpStream::connect::<SocketAddr>(tcp_address.into())
            .expect("could not queue a connection on the inherited TCP socket");
        let (http_fd, https_fd, tcp_fd, udp_fd) = (
            http_listener.into_raw_fd(),
            https_listener.into_raw_fd(),
            tcp_listener.into_raw_fd(),
            udp_socket.into_raw_fd(),
        );
        {
            let scm_listeners = server
                .scm_listeners
                .as_mut()
                .expect("Server::new must have populated scm_listeners");
            scm_listeners.http.push((http_address.into(), http_fd));
            scm_listeners.tls.push((https_address.into(), https_fd));
            scm_listeners.tcp.push((tcp_address.into(), tcp_fd));
            scm_listeners.udp.push((udp_address.into(), udp_fd));
        }

        let add_requests =
            state_with_four_active_listeners(http_address, https_address, tcp_address, udp_address);
        let cases = [
            ("HTTP", http_address, ListenerType::Http, http_fd),
            ("HTTPS", https_address, ListenerType::Https, https_fd),
            ("TCP", tcp_address, ListenerType::Tcp, tcp_fd),
            ("UDP", udp_address, ListenerType::Udp, udp_fd),
        ];

        // No listener at any of the addresses: every activation fails, and every
        // descriptor must survive it untouched.
        for (label, address, proxy, fd) in cases {
            let response = server.notify_activate_listener(
                "test-early",
                &ActivateListener {
                    address,
                    proxy: proxy.into(),
                    from_scm: false,
                },
            );
            assert_ne!(
                response.status,
                ResponseStatus::Ok as i32,
                "activating {label} with no listener must fail: {response:?}"
            );
            assert!(
                inherited_table(&server, label)
                    .iter()
                    .any(|(_, e)| *e == fd),
                "the {label} descriptor must stay in the SCM table while no listener owns \
                 its address"
            );
            assert!(
                descriptor_is_socket_on_port(fd, address.port as u16),
                "the {label} descriptor must stay open while no listener owns its address"
            );
        }

        // The listeners arrive, then their activations: every descriptor is
        // adopted, so nothing is left in the table and nothing was closed unused.
        for request in add_requests.produce_initial_state().requests {
            server.notify_proxys(request);
        }
        for (label, _address, _proxy, fd) in cases {
            assert!(
                !inherited_table(&server, label)
                    .iter()
                    .any(|(_, e)| *e == fd),
                "the retained {label} descriptor must be adopted by the later activation"
            );
        }
        let accepted: Vec<Protocol> = server
            .accept_queue
            .iter()
            .map(|(_, _, protocol, _, _)| *protocol)
            .collect();
        for expected in [
            Protocol::HTTPListen,
            Protocol::HTTPSListen,
            Protocol::TCPListen,
        ] {
            assert!(
                accepted.contains(&expected),
                "the connection queued on the retained {expected:?} descriptor must be served \
                 by the later activation, got {accepted:?}"
            );
        }
    }

    /// A socket parked by a failed registration must not be mistaken for a
    /// live, registered one.
    ///
    /// Every consumer of the listener's socket field reads its `Some` as
    /// "registered and live": `give_back_listener` hands it back as an
    /// activated socket and answers `Ok`, `soft_stop` / `hard_stop` deregister
    /// it and fold a failure into `ProxyError::SoftStop` / `HardStop`, `accept`
    /// accepts on it. Parking there made a listener that had simply never come
    /// up look activated — and, for a registration failure that leaves the
    /// socket unregistered, made `deregister` answer `ENOENT` and report a
    /// failed shutdown. The park therefore lives in its own `parked_listener` /
    /// `parked_socket` field, which nothing but `activate()` reads.
    ///
    /// `give_back_listener` is the assertion that falsifies the mutation:
    /// `ProxyError::UnactivatedListener` keys on the live field being `None`,
    /// so parking into it turns the answer into `Ok`. The stop assertions ride
    /// along to pin the shutdown behaviour, but they cannot redden on this
    /// repro — the EEXIST it uses means the descriptor IS registered (under
    /// another token), so `deregister` succeeds either way.
    ///
    /// To SEE THIS RED: in any `activate()`, replace
    /// `self.parked_listener = Some(listener)` (or `self.parked_socket = Some(socket)`)
    /// with `self.listener = Some(listener)` / `self.socket = Some(socket)` —
    /// that protocol's `give_back_listener` assertion fails.
    #[test]
    fn a_socket_parked_by_a_failed_registration_is_not_mistaken_for_a_live_one() {
        let ServerParts {
            registry,
            sessions,
            pool,
            backends,
            ..
        } = prebuild_server(16, 16384, true).expect("could not prebuild a test server");

        let token_for = |protocol: Protocol| {
            let mut manager = sessions.borrow_mut();
            let entry = manager.slab.vacant_entry();
            let token = Token(entry.key());
            entry.insert(Rc::new(RefCell::new(ListenSession { protocol })));
            token
        };
        // A socket whose descriptor is already registered on the same epoll
        // instance, so `activate()`'s own `register` answers EEXIST and parks.
        let pre_registered = |address: SocketAddress| {
            let mut socket =
                server_bind(address.into()).expect("could not bind the test listening socket");
            registry
                .register(&mut socket, Token(usize::MAX - 2), Interest::READABLE)
                .expect("could not pre-register the test socket");
            socket
        };

        let http_address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let mut http_proxy = http::HttpProxy::new(
            registry.try_clone().expect("could not clone the registry"),
            sessions.clone(),
            pool.clone(),
            backends.clone(),
        );
        http_proxy
            .add_listener(
                ListenerBuilder::new_http(http_address)
                    .to_http(None)
                    .expect("could not build the test HTTP listener config"),
                token_for(Protocol::HTTPListen),
            )
            .expect("could not add the test HTTP listener");
        assert!(
            http_proxy
                .activate_listener(&http_address.into(), Some(pre_registered(http_address)))
                .is_err(),
            "the pre-registered descriptor must make the HTTP registration fail"
        );
        assert!(
            matches!(
                http_proxy.give_back_listener(http_address.into()),
                Err(ProxyError::UnactivatedListener)
            ),
            "an HTTP listener holding only a parked socket must not answer as activated"
        );
        assert!(
            http_proxy.soft_stop().is_ok(),
            "a parked socket must not fail an HTTP soft stop"
        );

        let https_address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let mut https_proxy = https::HttpsProxy::new(
            registry.try_clone().expect("could not clone the registry"),
            sessions.clone(),
            pool.clone(),
            backends.clone(),
        );
        https_proxy
            .add_listener(
                ListenerBuilder::new_https(https_address)
                    .to_tls(None)
                    .expect("could not build the test HTTPS listener config"),
                token_for(Protocol::HTTPSListen),
            )
            .expect("could not add the test HTTPS listener");
        assert!(
            https_proxy
                .activate_listener(&https_address.into(), Some(pre_registered(https_address)))
                .is_err(),
            "the pre-registered descriptor must make the HTTPS registration fail"
        );
        assert!(
            matches!(
                https_proxy.give_back_listener(https_address.into()),
                Err(ProxyError::UnactivatedListener)
            ),
            "an HTTPS listener holding only a parked socket must not answer as activated"
        );
        assert!(
            https_proxy.hard_stop().is_ok(),
            "a parked socket must not fail an HTTPS hard stop"
        );

        let tcp_address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let mut tcp_proxy = tcp::TcpProxy::new(
            registry.try_clone().expect("could not clone the registry"),
            sessions.clone(),
            pool.clone(),
            backends.clone(),
        );
        tcp_proxy
            .add_listener(
                ListenerBuilder::new_tcp(tcp_address)
                    .to_tcp(None)
                    .expect("could not build the test TCP listener config"),
                token_for(Protocol::TCPListen),
            )
            .expect("could not add the test TCP listener");
        assert!(
            tcp_proxy
                .activate_listener(&tcp_address.into(), Some(pre_registered(tcp_address)))
                .is_err(),
            "the pre-registered descriptor must make the TCP registration fail"
        );
        assert!(
            matches!(
                tcp_proxy.give_back_listener(tcp_address.into()),
                Err(ProxyError::UnactivatedListener)
            ),
            "a TCP listener holding only a parked socket must not answer as activated"
        );

        let udp_address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let mut udp_proxy = udp::UdpProxy::new(
            registry.try_clone().expect("could not clone the registry"),
            sessions.clone(),
            pool.clone(),
            backends.clone(),
            16,
            16384,
        );
        udp_proxy
            .add_listener(
                ListenerBuilder::new_udp(udp_address)
                    .to_udp(None)
                    .expect("could not build the test UDP listener config"),
                token_for(Protocol::UDP),
            )
            .expect("could not add the test UDP listener");
        let mut udp_socket =
            udp_bind(udp_address.into()).expect("could not bind the test UDP socket");
        registry
            .register(&mut udp_socket, Token(usize::MAX - 2), Interest::READABLE)
            .expect("could not pre-register the test UDP socket");
        assert!(
            udp_proxy
                .activate_listener(&udp_address.into(), Some(udp_socket))
                .is_err(),
            "the pre-registered descriptor must make the UDP registration fail"
        );
        assert!(
            matches!(
                udp_proxy.give_back_listener(udp_address.into()),
                Err(ProxyError::UnactivatedListener)
            ),
            "a UDP listener holding only a parked socket must not answer as activated"
        );
    }

    /// A failed mio registration must not close the socket it was handed.
    ///
    /// `activate()` moves the socket into a local to register it. Propagating a
    /// registration error straight from that local drops it — and closes a
    /// descriptor `Listeners::get_*` has already removed from the SCM table,
    /// which is the sozu#1342 close in a second place. `Registry::register`
    /// answers `EEXIST` for a descriptor already registered on the same epoll
    /// instance, which is what the test arranges and what a `deactivate` whose
    /// `deregister` only logged leaves behind.
    ///
    /// To SEE THIS RED: in any `activate()`, go back to
    /// `registry.register(..).map_err(..)?` followed by `self.listener = Some(listener)`
    /// — the `?` drops the local and that protocol's assertion fails.
    #[test]
    fn a_failed_registration_does_not_close_the_socket_it_was_handed() {
        let ServerParts {
            registry,
            sessions,
            pool,
            backends,
            ..
        } = prebuild_server(16, 16384, true).expect("could not prebuild a test server");

        let token_for = |protocol: Protocol| {
            let mut manager = sessions.borrow_mut();
            let entry = manager.slab.vacant_entry();
            let token = Token(entry.key());
            entry.insert(Rc::new(RefCell::new(ListenSession { protocol })));
            token
        };

        // Hand each proxy a socket whose descriptor is ALREADY registered on the
        // same epoll instance, so `activate()`'s own `register` answers EEXIST.
        let handed_out = |address: SocketAddress| {
            let mut socket =
                server_bind(address.into()).expect("could not bind the test listening socket");
            registry
                .register(&mut socket, Token(usize::MAX - 1), Interest::READABLE)
                .expect("could not pre-register the test socket");
            let fd = socket.as_raw_fd();
            (socket, fd)
        };

        let http_address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let token = token_for(Protocol::HTTPListen);
        let mut http_proxy = http::HttpProxy::new(
            registry.try_clone().expect("could not clone the registry"),
            sessions.clone(),
            pool.clone(),
            backends.clone(),
        );
        http_proxy
            .add_listener(
                ListenerBuilder::new_http(http_address)
                    .to_http(None)
                    .expect("could not build the test HTTP listener config"),
                token,
            )
            .expect("could not add the test HTTP listener");
        let (socket, http_fd) = handed_out(http_address);
        let result = http_proxy.activate_listener(&http_address.into(), Some(socket));
        assert!(
            result.is_err(),
            "the pre-registered descriptor must make the HTTP registration fail"
        );
        assert!(
            descriptor_is_socket_on_port(http_fd, http_address.port as u16),
            "a failed HTTP registration must not close the socket it was handed"
        );

        let tcp_address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let token = token_for(Protocol::TCPListen);
        let mut tcp_proxy = tcp::TcpProxy::new(
            registry.try_clone().expect("could not clone the registry"),
            sessions.clone(),
            pool.clone(),
            backends.clone(),
        );
        tcp_proxy
            .add_listener(
                ListenerBuilder::new_tcp(tcp_address)
                    .to_tcp(None)
                    .expect("could not build the test TCP listener config"),
                token,
            )
            .expect("could not add the test TCP listener");
        let (socket, tcp_fd) = handed_out(tcp_address);
        let result = tcp_proxy.activate_listener(&tcp_address.into(), Some(socket));
        assert!(
            result.is_err(),
            "the pre-registered descriptor must make the TCP registration fail"
        );
        assert!(
            descriptor_is_socket_on_port(tcp_fd, tcp_address.port as u16),
            "a failed TCP registration must not close the socket it was handed"
        );

        let https_address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let token = token_for(Protocol::HTTPSListen);
        let mut https_proxy = https::HttpsProxy::new(
            registry.try_clone().expect("could not clone the registry"),
            sessions.clone(),
            pool.clone(),
            backends.clone(),
        );
        https_proxy
            .add_listener(
                ListenerBuilder::new_https(https_address)
                    .to_tls(None)
                    .expect("could not build the test HTTPS listener config"),
                token,
            )
            .expect("could not add the test HTTPS listener");
        let (socket, https_fd) = handed_out(https_address);
        let result = https_proxy.activate_listener(&https_address.into(), Some(socket));
        assert!(
            result.is_err(),
            "the pre-registered descriptor must make the HTTPS registration fail"
        );
        assert!(
            descriptor_is_socket_on_port(https_fd, https_address.port as u16),
            "a failed HTTPS registration must not close the socket it was handed"
        );

        let udp_address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let token = token_for(Protocol::UDP);
        let mut udp_proxy = udp::UdpProxy::new(
            registry.try_clone().expect("could not clone the registry"),
            sessions.clone(),
            pool.clone(),
            backends.clone(),
            16,
            16384,
        );
        udp_proxy
            .add_listener(
                ListenerBuilder::new_udp(udp_address)
                    .to_udp(None)
                    .expect("could not build the test UDP listener config"),
                token,
            )
            .expect("could not add the test UDP listener");
        let mut socket = udp_bind(udp_address.into()).expect("could not bind the test UDP socket");
        registry
            .register(&mut socket, Token(usize::MAX - 1), Interest::READABLE)
            .expect("could not pre-register the test UDP socket");
        let udp_fd = socket.as_raw_fd();
        let result = udp_proxy.activate_listener(&udp_address.into(), Some(socket));
        assert!(
            result.is_err(),
            "the pre-registered descriptor must make the UDP registration fail"
        );
        assert!(
            descriptor_is_socket_on_port(udp_fd, udp_address.port as u16),
            "a failed UDP registration must not close the socket it was handed"
        );
    }
}
