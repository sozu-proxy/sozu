use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    net::SocketAddr,
    rc::{Rc, Weak},
    time::{Duration, Instant},
};

use mio::net::TcpStream;
use rand::{
    Rng, SeedableRng,
    rngs::{StdRng, SysRng},
};
use sozu_command::{
    proto::command::{
        Event, EventKind, HealthCheckConfig, LoadBalancingAlgorithms, LoadBalancingParams,
        LoadMetric, ShardMode,
    },
    state::ClusterId,
};

use crate::metrics::names;
use crate::{
    PeakEWMA,
    load_balancing::{
        Candidates, LeastLoaded, LoadBalancingAlgorithm, Maglev, PowerOfTwo, Random, Rendezvous,
        RoundRobin, ShuffleSharding, count_sibling_step, hrw_score, outranks_at_address,
    },
    retry::{self, RetryPolicy},
    server::{self, push_event},
};

#[derive(thiserror::Error, Debug)]
pub enum BackendError {
    #[error("No backend found for cluster {0}")]
    NoBackendForCluster(String),
    #[error("Failed to connect to socket with MIO: {0}")]
    MioConnection(std::io::Error),
    #[error("This backend is not in a normal status: status={0:?}")]
    Status(BackendStatus),
    #[error("could not connect {cluster_id} to {backend_address:?} ({failures} failures): {error}")]
    ConnectionFailures {
        cluster_id: String,
        backend_address: SocketAddr,
        failures: usize,
        error: String,
    },
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum BackendStatus {
    Normal,
    Closing,
    Closed,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum HealthStatus {
    Healthy,
    Unhealthy,
}

/// Per-cluster availability state, owned by `BackendList`. Flips between
/// `Available` (≥1 backend can serve traffic) and `AllDown` (every backend
/// fails the `health.is_healthy() && !retry_policy.is_down()` predicate)
/// every time `BackendMap::record_cluster_availability` is invoked.
/// Empty clusters never report `AllDown` to avoid log spam during cluster
/// bootstrap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ClusterAvailability {
    #[default]
    Available,
    AllDown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HealthState {
    pub status: HealthStatus,
    pub consecutive_successes: u32,
    pub consecutive_failures: u32,
}

impl Default for HealthState {
    fn default() -> Self {
        HealthState {
            status: HealthStatus::Healthy,
            consecutive_successes: 0,
            consecutive_failures: 0,
        }
    }
}

impl HealthState {
    /// Record a successful health check. Returns true if the backend transitioned to healthy.
    pub fn record_success(&mut self, healthy_threshold: u32) -> bool {
        let was_unhealthy = self.status == HealthStatus::Unhealthy;
        let successes_before = self.consecutive_successes;
        self.consecutive_failures = 0;
        self.consecutive_successes += 1;

        // A success resets the failure streak and advances the success streak by
        // exactly one — the two counters are never both non-zero afterwards.
        debug_assert_eq!(
            self.consecutive_failures, 0,
            "a success must clear the consecutive-failure streak"
        );
        debug_assert_eq!(
            self.consecutive_successes,
            successes_before + 1,
            "a success must advance the success streak by exactly one"
        );

        if was_unhealthy && self.consecutive_successes >= healthy_threshold {
            self.status = HealthStatus::Healthy;
            // The transition is only reported when crossing the threshold from
            // Unhealthy; a backend that was already Healthy never "transitions".
            debug_assert!(
                self.status == HealthStatus::Healthy,
                "a reported recovery must leave the status Healthy"
            );
            return true;
        }
        false
    }

    /// Record a failed health check. Returns true if the backend transitioned to unhealthy.
    pub fn record_failure(&mut self, unhealthy_threshold: u32) -> bool {
        let was_healthy = self.status == HealthStatus::Healthy;
        let failures_before = self.consecutive_failures;
        self.consecutive_successes = 0;
        self.consecutive_failures += 1;

        // A failure resets the success streak and advances the failure streak by
        // exactly one — symmetric with `record_success`.
        debug_assert_eq!(
            self.consecutive_successes, 0,
            "a failure must clear the consecutive-success streak"
        );
        debug_assert_eq!(
            self.consecutive_failures,
            failures_before + 1,
            "a failure must advance the failure streak by exactly one"
        );

        if was_healthy && self.consecutive_failures >= unhealthy_threshold {
            self.status = HealthStatus::Unhealthy;
            debug_assert!(
                self.status == HealthStatus::Unhealthy,
                "a reported drop must leave the status Unhealthy"
            );
            return true;
        }
        false
    }

    pub fn is_healthy(&self) -> bool {
        self.status == HealthStatus::Healthy
    }
}

#[derive(Debug, PartialEq, Clone)]
pub struct Backend {
    pub sticky_id: Option<String>,
    pub backend_id: String,
    pub address: SocketAddr,
    pub status: BackendStatus,
    pub retry_policy: retry::RetryPolicyWrapper,
    pub active_connections: usize,
    pub active_requests: usize,
    pub failures: usize,
    pub load_balancing_parameters: Option<LoadBalancingParams>,
    pub backup: bool,
    pub connection_time: PeakEWMA,
    pub health: HealthState,
}

impl Backend {
    /// A backend created now, on the wall clock.
    ///
    /// The production default: the one place backend construction reads the
    /// clock, because the connection-time average decays from the instant the
    /// backend was created. A simulator that must not read the wall clock
    /// uses [`Self::new_at`].
    pub fn new(
        backend_id: &str,
        address: SocketAddr,
        sticky_id: Option<String>,
        load_balancing_parameters: Option<LoadBalancingParams>,
        backup: Option<bool>,
    ) -> Backend {
        Self::new_at(
            backend_id,
            address,
            sticky_id,
            load_balancing_parameters,
            backup,
            Instant::now(),
        )
    }

    /// A backend created at `now` (#1684).
    pub fn new_at(
        backend_id: &str,
        address: SocketAddr,
        sticky_id: Option<String>,
        load_balancing_parameters: Option<LoadBalancingParams>,
        backup: Option<bool>,
        now: Instant,
    ) -> Backend {
        let desired_policy = retry::ExponentialBackoffPolicy::new(6);
        Backend {
            sticky_id,
            backend_id: backend_id.to_owned(),
            address,
            status: BackendStatus::Normal,
            retry_policy: desired_policy.into(),
            active_connections: 0,
            active_requests: 0,
            failures: 0,
            load_balancing_parameters,
            backup: backup.unwrap_or(false),
            connection_time: PeakEWMA::new(now),
            health: HealthState::default(),
        }
    }

    pub fn set_closing(&mut self) {
        self.status = BackendStatus::Closing;
    }

    pub fn retry_policy(&mut self) -> &mut retry::RetryPolicyWrapper {
        &mut self.retry_policy
    }

    /// Whether this backend can take a new connection at `now`.
    pub fn can_open(&self, now: Instant) -> bool {
        if !self.health.is_healthy() {
            return false;
        }
        if let Some(action) = self.retry_policy.can_try(now) {
            self.status == BackendStatus::Normal && action == retry::RetryAction::OKAY
        } else {
            false
        }
    }

    /// Canonical "available" check used by per-backend metrics and cluster
    /// availability accounting. Slightly more permissive than `can_open()`:
    /// a backend currently in an exponential-backoff *wait window* still
    /// counts as available because the next call after the window ends
    /// will route to it without operator intervention. The dashboard
    /// reading must reflect "operationally up, not exhausted" rather than
    /// "ready to receive *this* request" — flicking the gauge to 0 on
    /// every transient backoff would drown out genuine `is_down()`
    /// transitions. Pairs with `BackendList::evaluate_availability`,
    /// which applies the same predicate cluster-wide.
    pub fn is_available(&self) -> bool {
        self.health.is_healthy()
            && self.status == BackendStatus::Normal
            && !self.retry_policy.is_down()
    }

    pub fn inc_connections(&mut self) -> Option<usize> {
        let before = self.active_connections;
        if self.status == BackendStatus::Normal {
            self.active_connections += 1;
            // A `Normal` backend always increments by exactly one and reports the
            // post-increment count back to the caller.
            debug_assert_eq!(
                self.active_connections,
                before + 1,
                "inc_connections must add exactly one active connection"
            );
            Some(self.active_connections)
        } else {
            // Non-`Normal` backends refuse new connections and leave the gauge
            // untouched (no silent increment on a Closing/Closed backend).
            debug_assert_eq!(
                self.active_connections, before,
                "inc_connections must not touch the count for a non-Normal backend"
            );
            None
        }
    }

    /// TODO: normalize with saturating_sub()
    pub fn dec_connections(&mut self) -> Option<usize> {
        let before = self.active_connections;
        match self.status {
            BackendStatus::Normal => {
                if self.active_connections > 0 {
                    self.active_connections -= 1;
                }
                // The count drops by one when positive, otherwise saturates at
                // zero — it must never wrap below zero (usize underflow).
                debug_assert!(
                    self.active_connections <= before,
                    "dec_connections must never increase the active-connection count"
                );
                debug_assert_eq!(
                    self.active_connections,
                    before.saturating_sub(1),
                    "dec_connections must drop by exactly one (saturating at zero)"
                );
                Some(self.active_connections)
            }
            BackendStatus::Closed => {
                // A Closed backend has already been retired: nothing to decrement.
                debug_assert_eq!(
                    self.active_connections, before,
                    "dec_connections on a Closed backend must not mutate the count"
                );
                None
            }
            BackendStatus::Closing => {
                if self.active_connections > 0 {
                    self.active_connections -= 1;
                }
                debug_assert_eq!(
                    self.active_connections,
                    before.saturating_sub(1),
                    "dec_connections must drop by exactly one (saturating at zero)"
                );
                if self.active_connections == 0 {
                    self.status = BackendStatus::Closed;
                    // Draining a Closing backend to zero retires it: the
                    // lifecycle advances to Closed and we stop reporting a count.
                    debug_assert_eq!(
                        self.status,
                        BackendStatus::Closed,
                        "a fully drained Closing backend must become Closed"
                    );
                    None
                } else {
                    Some(self.active_connections)
                }
            }
        }
    }

    /// Reserve a connection on this backend for a dial about to start.
    ///
    /// `active_connections += 1`, exactly the count a successful
    /// [`Self::try_connect`] takes, but before the connect: selection takes it
    /// under the same borrow as the choice (`BackendMap::reserve_backend`).
    /// Only a `Normal` backend can be chosen, and only a `Normal` backend
    /// accepts the reservation.
    pub fn reserve_connection(&mut self) -> Result<(), BackendError> {
        match self.inc_connections() {
            Some(_) => Ok(()),
            None => Err(BackendError::Status(self.status.to_owned())),
        }
    }

    /// Release the reservation of a dial whose connect failed at `now`, and
    /// record the failure: `failures += 1` and the retry policy's backoff,
    /// whose jitter is drawn from `rng`.
    ///
    /// Leaves the backend where a failed [`Self::try_connect`] leaves it: the
    /// reservation it releases is the count that call never took.
    pub fn release_failed_dial<R: Rng + ?Sized>(&mut self, now: Instant, rng: &mut R) {
        let (connections_before, failures_before) = (self.active_connections, self.failures);
        self.dec_connections();
        self.failures += 1;
        self.retry_policy.fail(now, rng);
        debug_assert_eq!(
            self.active_connections,
            connections_before.saturating_sub(1),
            "a failed dial must release exactly its one reservation (saturating at 0)"
        );
        debug_assert_eq!(
            self.failures,
            failures_before + 1,
            "a failed dial must advance the failure counter by exactly one"
        );
    }

    /// Record that a connection took `dur` to establish, observed at `now`.
    pub fn set_connection_time(&mut self, dur: Duration, now: Instant) {
        self.connection_time.observe(dur.as_nanos() as f64, now);
    }

    /// The connection-time cost of this backend, as seen at `now`.
    pub fn peak_ewma_connection(&mut self, now: Instant) -> f64 {
        self.connection_time.get(self.active_connections, now)
    }

    /// Open a connection to this backend. A failure at `now` arms the retry
    /// policy, whose backoff jitter is drawn from `rng`.
    pub fn try_connect<R: Rng + ?Sized>(
        &mut self,
        now: Instant,
        rng: &mut R,
    ) -> Result<mio::net::TcpStream, BackendError> {
        if self.status != BackendStatus::Normal {
            return Err(BackendError::Status(self.status.to_owned()));
        }
        // Reaching the connect attempt implies we passed the status gate; the
        // failure counter is whatever prior attempts accumulated.
        debug_assert_eq!(
            self.status,
            BackendStatus::Normal,
            "try_connect only attempts a connection on a Normal backend"
        );
        let failures_before = self.failures;
        let connections_before = self.active_connections;

        match mio::net::TcpStream::connect(self.address) {
            Ok(tcp_stream) => {
                //self.retry_policy.succeed();
                self.inc_connections();
                // Success registers exactly one new active connection and never
                // touches the failure counter.
                debug_assert_eq!(
                    self.active_connections,
                    connections_before + 1,
                    "a successful connect must register exactly one active connection"
                );
                debug_assert_eq!(
                    self.failures, failures_before,
                    "a successful connect must not bump the failure counter"
                );
                Ok(tcp_stream)
            }
            Err(io_error) => {
                self.retry_policy.fail(now, rng);
                self.failures += 1;
                // A failed connect arms the retry policy and advances the
                // failure counter by exactly one, leaving the connection gauge
                // untouched (no connection was established).
                debug_assert_eq!(
                    self.failures,
                    failures_before + 1,
                    "a failed connect must advance the failure counter by exactly one"
                );
                debug_assert_eq!(
                    self.active_connections, connections_before,
                    "a failed connect must not register an active connection"
                );
                // TODO: handle EINPROGRESS. It is difficult. It is discussed here:
                // https://docs.rs/mio/latest/mio/net/struct.TcpStream.html#method.connect
                // with an example code here:
                // https://github.com/Thomasdezeeuw/heph/blob/0c4f1ab3eaf08bea1d65776528bfd6114c9f8374/src/net/tcp/stream.rs#L560-L622
                Err(BackendError::MioConnection(io_error))
            }
        }
    }
}

// when a backend has been removed from configuration and the last connection to
// it has stopped, it will be dropped, so we can notify that the backend server
// can be safely stopped
impl std::ops::Drop for Backend {
    fn drop(&mut self) {
        server::push_event(Event {
            kind: EventKind::RemovedBackendHasNoConnections as i32,
            backend_id: Some(self.backend_id.to_owned()),
            address: Some(self.address.into()),
            cluster_id: None,
            metric_detail: None,
        });
    }
}

#[derive(Debug)]
pub struct BackendMap {
    pub backends: HashMap<ClusterId, BackendList>,
    pub health_check_configs: HashMap<ClusterId, HealthCheckConfig>,
    /// Whether the cluster's backends speak HTTP/2 (cluster.http2 = true).
    /// Mirrors the same backend-capability hint the mux router reads at
    /// `protocol/mux/router.rs::Router::plan_connect`. The health checker uses
    /// it to switch the probe wire format from HTTP/1.1 to h2c so an
    /// h2c-only backend is not probed with an HTTP/1.1 preface that
    /// would always fail.
    pub cluster_http2: HashMap<ClusterId, bool>,
    /// The one source of randomness selection and backoff draw from: it seeds
    /// every cluster's load-balancing policy and supplies the jitter of every
    /// backoff window a failed connection arms.
    ///
    /// [`Self::new`] seeds it from the OS once, so each worker draws an
    /// independent keystream, as the thread-local RNG it replaces did;
    /// [`Self::with_seed`] makes the whole map reproducible for a simulator
    /// (#1684). Taken at construction, never on the datapath.
    rng: StdRng,
}

/// A generator seeded from the OS, for the production defaults of
/// [`BackendMap::new`] and [`BackendList::new`].
fn os_rng() -> StdRng {
    StdRng::try_from_rng(&mut SysRng).expect("failed to seed random number generator from system")
}

impl Default for BackendMap {
    fn default() -> Self {
        Self::new()
    }
}

impl BackendMap {
    /// A backend map whose randomness is seeded from the OS.
    pub fn new() -> BackendMap {
        Self::with_rng(os_rng())
    }

    /// A backend map whose every policy seed and backoff jitter derives from
    /// `seed`, for a deterministic simulator (#1684).
    pub fn with_seed(seed: u64) -> BackendMap {
        Self::with_rng(StdRng::seed_from_u64(seed))
    }

    fn with_rng(rng: StdRng) -> BackendMap {
        BackendMap {
            backends: HashMap::new(),
            health_check_configs: HashMap::new(),
            cluster_http2: HashMap::new(),
            rng,
        }
    }

    /// The map's generator, for a caller that arms a backend's retry policy
    /// outside [`Self::backend_from_cluster_id`] and needs its jitter.
    pub fn rng(&mut self) -> &mut StdRng {
        &mut self.rng
    }

    /// Re-evaluate the availability of `cluster_id`, publish the
    /// `cluster.available_backends` / `cluster.total_backends` gauges,
    /// and emit the transition log + counter + `Event` exactly when the
    /// per-cluster state flips between `Available` and `AllDown`.
    ///
    /// Empty clusters (`total == 0`) never report `AllDown` — avoids log
    /// spam during cluster bootstrap when backends are still being
    /// registered. The (0, 0) gauges are still published so dashboards
    /// see "cluster exists, zero backends configured" as a distinct
    /// state from "cluster doesn't exist".
    ///
    /// Takes `&self` so callers that already hold `&mut BackendMap`
    /// can drop their `&mut BackendList` borrow before invoking it
    /// without re-borrowing.
    pub(crate) fn record_cluster_availability(&self, cluster_id: &str) {
        let Some(list) = self.backends.get(cluster_id) else {
            return;
        };

        let (available, total) = list.evaluate_availability();
        // A subset count can never exceed the whole, and it must match the
        // live backend vector length the helper just walked.
        debug_assert!(
            available <= total,
            "available backends ({available}) cannot exceed total ({total})"
        );
        debug_assert_eq!(
            total,
            list.backends.len(),
            "total must equal the number of registered backends"
        );
        gauge!(
            names::cluster::AVAILABLE_BACKENDS,
            available,
            Some(cluster_id),
            None
        );
        gauge!(
            names::cluster::TOTAL_BACKENDS,
            total,
            Some(cluster_id),
            None
        );

        let new_state = if total > 0 && available == 0 {
            ClusterAvailability::AllDown
        } else {
            ClusterAvailability::Available
        };
        // Empty clusters never report AllDown (avoids bootstrap log spam); a
        // cluster with at least one available backend is always Available.
        debug_assert!(
            !(total == 0 && new_state == ClusterAvailability::AllDown),
            "an empty cluster must never be reported AllDown"
        );
        debug_assert!(
            !(available > 0 && new_state == ClusterAvailability::AllDown),
            "a cluster with an available backend must not be AllDown"
        );

        let prev = list.availability.replace(new_state);
        // The cell now holds exactly the freshly computed state.
        debug_assert_eq!(
            list.availability.get(),
            new_state,
            "the availability cell must latch the newly computed state"
        );
        if prev == new_state {
            return;
        }
        match (prev, new_state) {
            (ClusterAvailability::Available, ClusterAvailability::AllDown) => {
                error!("cluster {}: all {} backends are down", cluster_id, total);
                incr!(
                    names::cluster::NO_AVAILABLE_BACKENDS,
                    Some(cluster_id),
                    None
                );
                push_event(Event {
                    kind: EventKind::NoAvailableBackends as i32,
                    cluster_id: Some(cluster_id.to_owned()),
                    backend_id: None,
                    address: None,
                    metric_detail: None,
                });
            }
            (ClusterAvailability::AllDown, ClusterAvailability::Available) => {
                info!(
                    "cluster {}: backends recovered ({}/{} available)",
                    cluster_id, available, total
                );
                incr!(names::cluster::AVAILABLE_RECOVERED, Some(cluster_id), None);
                push_event(Event {
                    kind: EventKind::ClusterRecovered as i32,
                    cluster_id: Some(cluster_id.to_owned()),
                    backend_id: None,
                    address: None,
                    metric_detail: None,
                });
            }
            _ => {}
        }
    }

    /// Forget everything the map holds for `cluster_id`: its backend list,
    /// with the load-balancing policy and shuffle sharding it carries, its
    /// health-check configuration and its `http2` hint. A session that already
    /// holds one of its backends keeps it until the session closes;
    /// [`Self::close_backend_connection`] ignores a cluster that is gone.
    /// Every backend it drops is marked `Closing`, as `BackendList::remove_backend`
    /// marks the ones it drops, so a connection pooled on it drains the
    /// requests it carries and takes no new one, even once a cluster of the
    /// same id is added again.
    pub fn remove_cluster(&mut self, cluster_id: &str) {
        if let Some(list) = self.backends.remove(cluster_id) {
            for backend in &list.backends {
                backend.borrow_mut().set_closing();
            }
        }
        self.health_check_configs.remove(cluster_id);
        self.cluster_http2.remove(cluster_id);
        debug_assert!(
            !self.backends.contains_key(cluster_id)
                && !self.health_check_configs.contains_key(cluster_id)
                && !self.cluster_http2.contains_key(cluster_id),
            "remove_cluster must leave nothing keyed by the cluster"
        );
    }

    /// Record (or clear) the `cluster.http2` backend-capability hint for
    /// `cluster_id`. The health checker reads the resulting map at probe
    /// time so the wire format follows what the mux router will use to
    /// connect to the same backends.
    pub fn set_cluster_http2(&mut self, cluster_id: &str, http2: bool) {
        if http2 {
            self.cluster_http2.insert(cluster_id.into(), true);
        } else {
            self.cluster_http2.remove(cluster_id);
        }
    }

    pub fn set_health_check_config(&mut self, cluster_id: &str, config: Option<HealthCheckConfig>) {
        match config {
            Some(c) => {
                self.health_check_configs.insert(cluster_id.into(), c);
            }
            None => {
                self.health_check_configs.remove(cluster_id);
                // When the operator drops the health check, any
                // previously-recorded `HealthState::Unhealthy` would
                // otherwise stick — `next_available_backend` keeps
                // skipping the backend even though we have stopped
                // probing it. Reset every backend in the cluster to a
                // pristine healthy state so the load balancer can
                // route again.
                if let Some(backend_list) = self.backends.get(cluster_id) {
                    for backend in &backend_list.backends {
                        backend.borrow_mut().health = HealthState::default();
                    }
                }
                // Re-emit the rollup gauges so dashboards reflect the
                // post-reset availability instead of holding the last
                // health-check value indefinitely.
                self.record_cluster_availability(cluster_id);
            }
        }
    }

    pub fn import_configuration_state(
        &mut self,
        backends: &HashMap<ClusterId, Vec<sozu_command::response::Backend>>,
    ) {
        // Seed the clusters in id order, not in the `HashMap`'s: its
        // `RandomState` iterates differently in every process, so drawing in
        // that order would hand a seeded map's seeds to different clusters on
        // every run (#1684). Sorting the ids allocates once, on this
        // control-plane replay path only.
        let mut cluster_ids: Vec<&ClusterId> = backends.keys().collect();
        cluster_ids.sort_unstable();
        for cluster_id in cluster_ids {
            let list =
                BackendList::import_configuration_state(&backends[cluster_id], self.rng.next_u64());
            self.backends.insert(cluster_id.clone(), list);
        }
        // Replay path inserts every cluster's backend list without
        // touching the gauge emission sites used by add/remove/health.
        // Latch `cluster.available_backends` and `.total_backends` here
        // so a freshly-loaded worker reports correct values on the very
        // first `QueryMetrics` instead of zero until something else
        // mutates each cluster.
        for cluster_id in backends.keys() {
            self.record_cluster_availability(cluster_id);
        }
    }

    pub fn add_backend(&mut self, cluster_id: &str, backend: Backend) {
        let address = backend.address;
        self.get_or_create_backend_list_for_cluster(cluster_id)
            .add_backend(backend);
        // Adding a backend must leave the cluster present and containing the
        // just-added address (whether it created the entry or updated in place).
        debug_assert!(
            self.backends
                .get(cluster_id)
                .is_some_and(|list| list.has_backend(&address)),
            "add_backend must leave the backend present in its cluster"
        );
        // Publish initial gauges and surface the corner case where a fresh
        // cluster's first backend is already down (e.g. registered with a
        // pre-existing failed retry policy). For an `Available` initial
        // backend this is just a (1, 1) gauge emission with no transition.
        self.record_cluster_availability(cluster_id);
    }

    // TODO: return <Result, BackendError>, log the error downstream
    /// Remove the backend identified by `(backend_id, backend_address)` from
    /// `cluster_id` and return whether it was present. A backend of another
    /// id at the same address stays (#1821).
    pub fn remove_backend(
        &mut self,
        cluster_id: &str,
        backend_id: &str,
        backend_address: &SocketAddr,
    ) -> bool {
        let removed = if let Some(backends) = self.backends.get_mut(cluster_id) {
            let removed = backends.remove_backend(backend_id, backend_address);
            if !removed {
                warn!(
                    "No backend matches id {} at address {:?} in cluster {}: nothing removed",
                    backend_id, backend_address, cluster_id
                );
            }
            removed
        } else {
            error!(
                "Backend was already removed: cluster id {}, backend id {}, address {:?}",
                cluster_id, backend_id, backend_address
            );
            return false;
        };
        debug_assert!(
            self.backends.get(cluster_id).is_none_or(|list| list
                .find_backend_by_identity(backend_id, backend_address)
                .is_none()),
            "remove_backend must evict the backend it names"
        );
        // Re-evaluate so removing the last backend logs an explicit
        // `AllDown` transition (or, with `total == 0`, drops back to
        // silent gauges).
        self.record_cluster_availability(cluster_id);
        removed
    }

    // TODO: return <Result, BackendError>, log the error downstream
    pub fn close_backend_connection(&mut self, cluster_id: &str, addr: &SocketAddr) {
        if let Some(cluster_backends) = self.backends.get_mut(cluster_id)
            && let Some(ref mut backend) = cluster_backends.find_backend(addr)
        {
            backend.borrow_mut().dec_connections();
        }
    }

    /// Whether `cluster_id` still holds a backend carrying `backend_id`, at
    /// any address. Backend metrics are labelled by id alone, so every entry
    /// sharing an id feeds one metrics row.
    pub fn has_backend_id(&self, cluster_id: &str, backend_id: &str) -> bool {
        self.backends.get(cluster_id).is_some_and(|backends| {
            backends
                .backends
                .iter()
                .any(|backend| backend.borrow().backend_id == backend_id)
        })
    }

    pub fn has_backend(&self, cluster_id: &str, backend: &Backend) -> bool {
        self.backends
            .get(cluster_id)
            .map(|backends| backends.has_backend(&backend.address))
            .unwrap_or(false)
    }

    /// Select a backend of `cluster_id` at `now` and connect to it.
    ///
    /// The TCP proxy's entry point: it selects and connects in one call, and
    /// `Backend::try_connect` counts the connection once it is established.
    /// The HTTP mux reserves with [`Self::reserve_backend`] instead and
    /// connects on its own side.
    ///
    /// `key` is the client's affinity key, which `HRW` and `MAGLEV` pin the
    /// client on and every other policy ignores: the HTTP, HTTPS and TCP
    /// datapaths pass the one they derived for a cluster using either
    /// policy, and `None` otherwise (see
    /// [`BackendList::next_available_backend_with_key`]).
    pub fn backend_from_cluster_id(
        &mut self,
        cluster_id: &str,
        key: Option<u64>,
        now: Instant,
    ) -> Result<(Rc<RefCell<Backend>>, TcpStream), BackendError> {
        let next_backend = self.select_backend(cluster_id, key, now, &[])?;

        let tcp_stream = {
            let mut borrowed_backend = next_backend.borrow_mut();

            debug!(
                "Connecting {} -> {:?}",
                cluster_id,
                (
                    borrowed_backend.address,
                    borrowed_backend.active_connections,
                    borrowed_backend.failures
                )
            );

            borrowed_backend
                .try_connect(now, &mut self.rng)
                .map_err(|backend_error| BackendError::ConnectionFailures {
                    cluster_id: cluster_id.to_owned(),
                    backend_address: borrowed_backend.address,
                    failures: borrowed_backend.failures,
                    error: backend_error.to_string(),
                })?
        };

        // Connection succeeded: re-evaluate so we capture an
        // AllDown -> Available recovery transition the moment a request
        // first hits a healthy backend after an outage. `next_backend` is
        // not borrowed here (the inner block dropped `borrowed_backend`),
        // so the helper's `BackendList::evaluate_availability` walk is
        // free to call `borrow()` on every backend.
        self.record_cluster_availability(cluster_id);

        // The selected backend is a live member of the cluster it was drawn
        // from — selection never fabricates or returns a stale backend.
        debug_assert!(
            self.backends.get(cluster_id).is_some_and(|list| {
                let picked = next_backend.borrow().address;
                list.has_backend(&picked)
            }),
            "the selected backend must belong to the cluster's live set"
        );

        Ok((next_backend.clone(), tcp_stream))
    }

    /// Select a backend of `cluster_id` at `now` and reserve a connection on
    /// it, without connecting (#1684).
    ///
    /// The reservation is `active_connections += 1`, taken under the same
    /// borrow as the choice, so the next selection weighs it exactly as it
    /// weighed the connection `Backend::try_connect` used to count at the
    /// dial. The caller connects. A `connect(2)` that fails releases the
    /// reservation through [`Backend::release_failed_dial`], which also
    /// records the failure. A dial that connects but is abandoned before its
    /// connection is registered releases it with a plain
    /// `Backend::dec_connections`, recording no failure (the mux's
    /// `BackendChange::ConnectionClosed`, #1713). A connection that starts
    /// keeps it, and its close releases it like any other.
    ///
    /// `key` is the client's affinity key, as for
    /// [`Self::backend_from_cluster_id`].
    pub fn reserve_backend(
        &mut self,
        cluster_id: &str,
        key: Option<u64>,
        now: Instant,
    ) -> Result<Rc<RefCell<Backend>>, BackendError> {
        self.reserve_backend_excluding(cluster_id, key, now, &[])
    }

    /// [`Self::reserve_backend`], preferring a backend whose address is not in
    /// `exclude`: the backends a request already failed to connect to
    /// (sozu-proxy/sozu#1800). See `BackendList::select_with_key_excluding`
    /// for what happens when every selectable backend is excluded.
    pub fn reserve_backend_excluding(
        &mut self,
        cluster_id: &str,
        key: Option<u64>,
        now: Instant,
        exclude: &[SocketAddr],
    ) -> Result<Rc<RefCell<Backend>>, BackendError> {
        let backend = self.select_backend(cluster_id, key, now, exclude)?;
        backend.borrow_mut().reserve_connection()?;
        // Re-evaluate on a successful selection, as the connecting path does
        // on a successful connect, so an AllDown -> Available recovery is
        // reported the moment a request reaches a backend again.
        self.record_cluster_availability(cluster_id);
        Ok(backend)
    }

    /// Reserve a connection on the backend of `cluster_id` that
    /// `sticky_session` names, if it can take one at `now`, falling back to
    /// [`Self::reserve_backend`] under the client's affinity `key`.
    ///
    /// A cookie naming a live backend wins over the key: the key only decides
    /// where a client without a usable cookie lands.
    pub fn reserve_sticky_backend(
        &mut self,
        cluster_id: &str,
        sticky_session: &str,
        key: Option<u64>,
        now: Instant,
    ) -> Result<Rc<RefCell<Backend>>, BackendError> {
        self.reserve_sticky_backend_excluding(cluster_id, sticky_session, key, now, &[])
    }

    /// [`Self::reserve_sticky_backend`], except that a cookie naming a backend
    /// in `exclude` is not followed: the request already failed to connect to
    /// it, so it goes to [`Self::reserve_backend_excluding`] instead
    /// (sozu-proxy/sozu#1800).
    pub fn reserve_sticky_backend_excluding(
        &mut self,
        cluster_id: &str,
        sticky_session: &str,
        key: Option<u64>,
        now: Instant,
        exclude: &[SocketAddr],
    ) -> Result<Rc<RefCell<Backend>>, BackendError> {
        let sticky = self
            .backends
            .get_mut(cluster_id)
            .and_then(|cluster_backends| cluster_backends.find_sticky(sticky_session, now))
            .filter(|backend| !exclude.contains(&backend.borrow().address))
            .cloned();
        match sticky {
            Some(backend) => {
                backend.borrow_mut().reserve_connection()?;
                Ok(backend)
            }
            None => {
                debug!(
                    "Couldn't find a backend corresponding to sticky_session {} for cluster {}",
                    sticky_session, cluster_id
                );
                self.reserve_backend_excluding(cluster_id, key, now, exclude)
            }
        }
    }

    /// Choose a backend of `cluster_id` at `now`, publishing the cluster's
    /// availability when nothing can be chosen.
    fn select_backend(
        &mut self,
        cluster_id: &str,
        key: Option<u64>,
        now: Instant,
        exclude: &[SocketAddr],
    ) -> Result<Rc<RefCell<Backend>>, BackendError> {
        let cluster_backends = self
            .backends
            .get_mut(cluster_id)
            .ok_or(BackendError::NoBackendForCluster(cluster_id.to_owned()))?;

        if cluster_backends.backends.is_empty() {
            // Drop the &mut BackendList borrow before the &self helper call.
            // `total == 0` falls into the "never report AllDown" branch in
            // record_cluster_availability, so this just publishes the (0, 0)
            // gauges.
            let _ = cluster_backends;
            self.record_cluster_availability(cluster_id);
            return Err(BackendError::NoBackendForCluster(cluster_id.to_owned()));
        }
        // Past the empty guard there is at least one backend to pick from.
        debug_assert!(
            !cluster_backends.backends.is_empty(),
            "selection runs only on a non-empty backend list"
        );

        let (picked, outcome) = cluster_backends.select_with_key_excluding(key, now, exclude);
        record_shard_outcome(cluster_id, outcome, picked.is_some());
        match picked {
            Some(backend) => Ok(backend),
            None => {
                // Drop the &mut BackendList before the &self helper call.
                // The helper observes (available=0, total>0) and emits the
                // Available -> AllDown transition (log + counter + Event)
                // exactly once per regime entry. Subsequent calls in the
                // same AllDown regime are no-ops.
                let _ = cluster_backends;
                self.record_cluster_availability(cluster_id);
                Err(BackendError::NoBackendForCluster(cluster_id.to_owned()))
            }
        }
    }

    /// Select a backend for `cluster_id`, optionally pinned by an affinity
    /// `key`, and return its `(backend_id, address)` **without** opening any
    /// connection.
    ///
    /// This is the UDP datapath's selection entry point. Unlike
    /// [`backend_from_cluster_id`](Self::backend_from_cluster_id) — which is
    /// TCP-specific because it calls `Backend::try_connect` and hands back a
    /// `TcpStream` — UDP owns its own per-flow connected `UdpSocket` (created in
    /// the shell via `socket::udp_connect`), so all the map needs to surface is
    /// the chosen endpoint identity. `key` is `Some(flow_hash)` so HRW/Maglev
    /// keep a client flow pinned to one backend; `None` behaves like the legacy
    /// round-robin selection. Fail-open (all-unhealthy ⇒ LB over the full set)
    /// is inherited from [`BackendList::next_available_backend_with_key`].
    pub fn backend_from_cluster_id_with_key(
        &mut self,
        cluster_id: &str,
        key: Option<u64>,
        now: Instant,
    ) -> Result<(String, SocketAddr), BackendError> {
        let cluster_backends = self
            .backends
            .get_mut(cluster_id)
            .ok_or(BackendError::NoBackendForCluster(cluster_id.to_owned()))?;

        if cluster_backends.backends.is_empty() {
            let _ = cluster_backends;
            self.record_cluster_availability(cluster_id);
            return Err(BackendError::NoBackendForCluster(cluster_id.to_owned()));
        }
        debug_assert!(
            !cluster_backends.backends.is_empty(),
            "keyed selection runs only on a non-empty backend list"
        );

        let (picked, outcome) = cluster_backends.select_with_key(key, now);
        record_shard_outcome(cluster_id, outcome, picked.is_some());
        let next_backend = match picked {
            Some(nb) => nb,
            None => {
                let _ = cluster_backends;
                self.record_cluster_availability(cluster_id);
                return Err(BackendError::NoBackendForCluster(cluster_id.to_owned()));
            }
        };

        let (backend_id, address) = {
            let borrowed = next_backend.borrow();
            (borrowed.backend_id.to_owned(), borrowed.address)
        };
        // The keyed selection returns a live member of the cluster — the
        // surfaced identity (id, address) belongs to a registered backend.
        debug_assert!(
            cluster_backends.has_backend(&address),
            "keyed selection must return a backend in the cluster's live set"
        );
        Ok((backend_id, address))
    }

    pub fn set_load_balancing_policy_for_cluster(
        &mut self,
        cluster_id: &str,
        lb_algo: LoadBalancingAlgorithms,
        metric: Option<LoadMetric>,
    ) {
        // The cluster can be created before the backends were registered because of the async config messages.
        // So when we set the load balancing policy, we have to create the backend list if if it doesn't exist yet.
        let seed = self.rng.next_u64();
        let cluster_backends = self.get_or_create_backend_list_for_cluster(cluster_id);
        cluster_backends.set_load_balancing_policy(lb_algo, metric, seed);
    }

    /// Set or clear the shuffle sharding of `cluster_id`, creating its
    /// backend list if the cluster's backends have not arrived yet, as
    /// [`Self::set_load_balancing_policy_for_cluster`] does.
    pub fn set_shuffle_sharding_for_cluster(
        &mut self,
        cluster_id: &str,
        shuffle_sharding: Option<ShuffleSharding>,
    ) {
        self.get_or_create_backend_list_for_cluster(cluster_id)
            .set_shuffle_sharding(shuffle_sharding);
    }

    pub fn get_or_create_backend_list_for_cluster(&mut self, cluster_id: &str) -> &mut BackendList {
        let rng = &mut self.rng;
        self.backends
            .entry(cluster_id.into())
            .or_insert_with(|| BackendList::with_seed(rng.next_u64()))
    }
}

/// The worker's backend registry, seen by the UDP core as a selection view.
///
/// `UdpManager` selects inside the core now (#1340, Question 6) and reaches
/// the registry only through this, for the duration of one admission. It
/// delegates to [`BackendMap::backend_from_cluster_id_with_key`], the
/// connection-free selection entry point — the UDP datapath owns its own
/// per-flow `UdpSocket`, so all the map has to surface is the chosen
/// endpoint's identity.
impl crate::protocol::udp::BackendSource for BackendMap {
    fn select(
        &mut self,
        cluster: &str,
        key: Option<u64>,
        now: Instant,
    ) -> Option<(crate::protocol::udp::BackendId, SocketAddr)> {
        self.backend_from_cluster_id_with_key(cluster, key, now)
            .ok()
    }
}

#[derive(Debug)]
pub struct BackendList {
    pub backends: Vec<Rc<RefCell<Backend>>>,
    pub next_id: u32,
    /// The cluster's selection policy. Crate-private: a Maglev policy keeps
    /// a lookup table built from `backends`, and only
    /// [`Self::set_load_balancing_policy`] seeds it when the policy is
    /// installed, so a direct assignment would leave the table empty or
    /// stale.
    pub(crate) load_balancing: Box<dyn LoadBalancingAlgorithm>,
    /// Latches the fail-open `warn!`. Set to `true` when fail-open routing
    /// emits its entry warning so subsequent routing decisions in the same
    /// regime stay quiet; reset to `false` when a healthy backend is
    /// available again, so the regime-exit transition is logged exactly once.
    /// Without this latch the warning fired per request, which under a
    /// universal outage (the exact scenario fail-open targets) is also the
    /// highest request-rate scenario — log volume would become catastrophic.
    fail_open_warned: bool,
    /// Per-cluster availability latched by `BackendMap::record_cluster_availability`.
    /// `Cell` (not `RefCell`) because the receiver is `&self` and the
    /// state is `Copy`. Worker runtime is single-threaded, so a `Cell` is
    /// sound — no synchronisation needed.
    pub(crate) availability: Cell<ClusterAvailability>,
    /// Positions in `backends` of the current selection's candidates, refilled
    /// by `collect_candidates` on every selection and lent to the policy as a
    /// [`Candidates`] view. Reused rather than rebuilt: `add_backend` reserves
    /// room for every backend, so a selection never allocates and clones no
    /// `Rc` but the one it returns.
    candidates: Vec<usize>,
    /// The cluster's shuffle sharding, `None` when it is off
    /// (sozu-proxy/sozu#524). Set by [`Self::set_shuffle_sharding`].
    shuffle_sharding: Option<ShuffleSharding>,
    /// Reused scratch for a keyed selection's shard: the HRW score of each
    /// primary backend and its position. Reserved in `add_backend` like
    /// `candidates`, so computing a shard never allocates.
    shard_scores: Vec<(f64, usize)>,
    /// The positions of the current selection's shard members, sorted.
    shard: Vec<usize>,
    /// Whether two backends of the list share an address. Recomputed by
    /// `refresh_siblings`, so a list without a shared address — the common
    /// case — skips `collapse_shared_addresses` entirely.
    shares_address: bool,
    /// Sibling ring: `sibling_ring[i]` is the next position whose backend
    /// shares the address of position `i`, cyclically through each address's
    /// ids in list order, and `i` itself for an address with one id. Lets a
    /// selection reach the siblings of an id without scanning the list.
    /// Recomputed by `refresh_siblings` on every mutation of the list or of
    /// a backend's configuration (`add_backend`, `remove_backend`).
    sibling_ring: Vec<usize>,
    /// Whether position `i` ranks for shuffle sharding: a primary backend no
    /// primary sibling outranks ([`outranks_at_address`]). A function of the
    /// configuration alone (address, weight, `backup`, list position), so it
    /// is precomputed by `refresh_siblings`.
    shard_representative: Vec<bool>,
    /// How many positions are `shard_representative`: the distinct primary
    /// addresses a shard size is computed from.
    shard_addresses: usize,
    /// Reused scratch `collapse_shared_addresses` filters the candidates
    /// into, reserved in `add_backend` like `candidates`.
    collapsed: Vec<usize>,
}

/// How shuffle sharding shaped one selection, for the caller that knows the
/// cluster to count it under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShardOutcome {
    /// Sharding is off, inactive below `shard_min_backends`, or the request
    /// had no key: the whole cluster was eligible.
    Unsharded,
    /// The selection stayed inside the client's shard.
    InShard,
    /// No member of the shard could take a connection and `FALLBACK`
    /// selected over the rest of the cluster.
    SpilledOver,
    /// No member of the shard could take a connection and `STRICT` selected
    /// nothing.
    Exhausted,
}

impl Default for BackendList {
    fn default() -> Self {
        Self::new()
    }
}

impl BackendList {
    /// An empty list whose default `Random` policy is seeded from the OS.
    pub fn new() -> BackendList {
        Self::with_seed(os_rng().next_u64())
    }

    /// An empty list whose default `Random` policy is seeded with `seed`.
    ///
    /// [`BackendMap`] builds every list it holds this way, from its own
    /// generator, so a seeded map seeds every cluster (#1684).
    pub fn with_seed(seed: u64) -> BackendList {
        BackendList {
            backends: Vec::new(),
            next_id: 0,
            load_balancing: Box::new(Random::with_seed(seed)),
            fail_open_warned: false,
            availability: Cell::new(ClusterAvailability::Available),
            candidates: Vec::new(),
            shuffle_sharding: None,
            shard_scores: Vec::new(),
            shard: Vec::new(),
            shares_address: false,
            sibling_ring: Vec::new(),
            shard_representative: Vec::new(),
            shard_addresses: 0,
            collapsed: Vec::new(),
        }
    }

    /// Count `(available, total)` for this cluster. Delegates to
    /// `Backend::is_available` so the per-cluster aggregate and the
    /// per-backend `backend.available` gauge stay in lock-step.
    pub(crate) fn evaluate_availability(&self) -> (usize, usize) {
        let total = self.backends.len();
        let available = self
            .backends
            .iter()
            .filter(|b| b.borrow().is_available())
            .count();
        // The available count is a filtered subset of the total, so it can
        // never exceed it, and `total` mirrors the backend vector length.
        debug_assert!(
            available <= total,
            "available ({available}) cannot exceed total ({total})"
        );
        debug_assert_eq!(total, self.backends.len(), "total must equal backend count");
        (available, total)
    }

    /// Full invariant sweep over the backend list. Called as a `debug_assert!`
    /// postcondition by every mutating method so any cross-field corruption
    /// (duplicate addresses, a `next_id` that underflowed below the registered
    /// count) surfaces immediately under test/fuzz.
    #[cfg(debug_assertions)]
    fn check_invariants(&self) {
        // `next_id` is a monotonically incremented registration counter: it is
        // bumped once per *newly inserted* backend (never decremented, even on
        // removal), so it is always at least the current live count.
        debug_assert!(
            self.next_id as usize >= self.backends.len(),
            "next_id ({}) must be >= live backend count ({})",
            self.next_id,
            self.backends.len()
        );
        // The precomputed sibling state gates and drives every selection: a
        // stale value would silently hand a policy one candidate per id
        // again, or rank a wrong representative. Check it in `O(n)` plus the
        // rings' lengths, so the sweep stays affordable on large lists.
        let n = self.backends.len();
        debug_assert_eq!(self.sibling_ring.len(), n, "one ring slot per backend");
        debug_assert_eq!(self.shard_representative.len(), n);
        // A permutation whose links join ids of one address...
        let mut targeted = vec![false; n];
        for (index, &next) in self.sibling_ring.iter().enumerate() {
            debug_assert!(!targeted[next], "the sibling ring is a permutation");
            targeted[next] = true;
            debug_assert_eq!(
                self.backends[next].borrow().address,
                self.backends[index].borrow().address,
                "a sibling ring links only ids of one address"
            );
        }
        // ...with exactly one cycle per address: every id of an address
        // reaches the same cycle leader, the cycle's smallest position.
        let mut leaders: HashMap<SocketAddr, usize> = HashMap::new();
        for index in 0..n {
            let mut leader = index;
            let mut next = self.sibling_ring[index];
            while next != index {
                leader = leader.min(next);
                next = self.sibling_ring[next];
            }
            let address = self.backends[index].borrow().address;
            debug_assert_eq!(
                *leaders.entry(address).or_insert(leader),
                leader,
                "an address has one sibling ring"
            );
            let backend = self.backends[index].borrow();
            let mut represents = !backend.backup;
            let mut sibling = self.sibling_ring[index];
            while sibling != index {
                let other = self.backends[sibling].borrow();
                represents &=
                    other.backup || !outranks_at_address(&other, sibling, &backend, index);
                sibling = self.sibling_ring[sibling];
            }
            debug_assert_eq!(
                self.shard_representative[index], represents,
                "shard_representative must match the configuration"
            );
        }
        debug_assert_eq!(
            self.shares_address,
            leaders.len() < n,
            "shares_address must match the backend list"
        );
        debug_assert_eq!(
            self.shard_addresses,
            self.shard_representative.iter().filter(|r| **r).count()
        );
        // `(backend_id, address)` is the identity `remove_backend` keys on; two
        // live backends may legitimately share an address (A/B variant) but
        // must then differ by `backend_id`. The pair is therefore unique
        // across the live set.
        for (i, a) in self.backends.iter().enumerate() {
            let a = a.borrow();
            for b in self.backends.iter().skip(i + 1) {
                let b = b.borrow();
                debug_assert!(
                    a.address != b.address || a.backend_id != b.backend_id,
                    "duplicate (address, backend_id) in the live set: {:?} / {}",
                    a.address,
                    a.backend_id
                );
            }
        }
    }

    /// A list holding `backend_vec`, its default policy seeded with `seed`.
    pub fn import_configuration_state(
        backend_vec: &[sozu_command_lib::response::Backend],
        seed: u64,
    ) -> BackendList {
        let mut list = BackendList::with_seed(seed);
        for backend in backend_vec {
            let backend = Backend::new(
                &backend.backend_id,
                backend.address,
                backend.sticky_id.clone(),
                backend.load_balancing_parameters,
                backend.backup,
            );
            list.add_backend(backend);
        }

        list
    }

    pub fn add_backend(&mut self, backend: Backend) {
        let address = backend.address;
        let len_before = self.backends.len();
        let next_id_before = self.next_id;
        let existed = self.backends.iter().any(|b| {
            b.borrow().address == backend.address && b.borrow().backend_id == backend.backend_id
        });
        match self.backends.iter_mut().find(|b| {
            b.borrow().address == backend.address && b.borrow().backend_id == backend.backend_id
        }) {
            None => {
                let backend = Rc::new(RefCell::new(backend));
                self.backends.push(backend);
                self.next_id += 1;
            }
            // the backend already exists, update the configuration while
            // keeping connection retry state
            Some(old_backend) => {
                let mut b = old_backend.borrow_mut();
                b.sticky_id.clone_from(&backend.sticky_id);
                b.load_balancing_parameters
                    .clone_from(&backend.load_balancing_parameters);
                b.backup = backend.backup;
            }
        }
        // Insert grows the list by exactly one and bumps `next_id`; an update
        // in place leaves both untouched. The address is present either way.
        debug_assert_eq!(
            self.backends.len(),
            len_before + (!existed) as usize,
            "add_backend grows the list by one only on a genuine insert"
        );
        debug_assert_eq!(
            self.next_id,
            next_id_before + (!existed) as u32,
            "next_id advances by one only on a genuine insert"
        );
        debug_assert!(
            self.has_backend(&address),
            "add_backend must leave the backend present in the list"
        );
        // Refresh table-based policies (Maglev) off the datapath whenever the
        // full backend set or a weight changes. This is the ONLY place the
        // table is rebuilt on mutation; selection never rebuilds. The default
        // `rebuild` is a no-op for the stateless policies.
        self.load_balancing.rebuild(&self.backends);
        // Size the candidate buffer for the whole list here, on the control
        // plane, so no selection ever grows it.
        self.candidates.clear();
        self.candidates.reserve(self.backends.len());
        self.shard_scores.clear();
        self.shard_scores.reserve(self.backends.len());
        self.shard.clear();
        self.shard.reserve(self.backends.len());
        self.collapsed.clear();
        self.collapsed.reserve(self.backends.len());
        self.refresh_siblings();
        #[cfg(debug_assertions)]
        self.check_invariants();
    }

    /// Recompute the sibling state — [`Self::shares_address`],
    /// [`Self::sibling_ring`], [`Self::shard_representative`] and
    /// [`Self::shard_addresses`] — from the backend list and its
    /// configuration. Control plane only: it allocates a map of the
    /// addresses, linear in the list length, plus, for each shared address
    /// of `g` ids, up to `g²` comparisons to rank them.
    fn refresh_siblings(&mut self) {
        let mut groups: HashMap<SocketAddr, Vec<usize>> = HashMap::new();
        for (index, backend) in self.backends.iter().enumerate() {
            groups
                .entry(backend.borrow().address)
                .or_default()
                .push(index);
        }
        self.sibling_ring.clear();
        self.sibling_ring.extend(0..self.backends.len());
        self.shares_address = false;
        for group in groups.values() {
            if group.len() > 1 {
                self.shares_address = true;
                for (rank, &index) in group.iter().enumerate() {
                    self.sibling_ring[index] = group[(rank + 1) % group.len()];
                }
            }
        }
        self.shard_representative.clear();
        for (index, backend) in self.backends.iter().enumerate() {
            let backend = backend.borrow();
            let mut represents = !backend.backup;
            let mut sibling = self.sibling_ring[index];
            while represents && sibling != index {
                let other = self.backends[sibling].borrow();
                represents = other.backup || !outranks_at_address(&other, sibling, &backend, index);
                sibling = self.sibling_ring[sibling];
            }
            self.shard_representative.push(represents);
        }
        self.shard_addresses = self.shard_representative.iter().filter(|r| **r).count();
    }

    /// Keep one candidate per address, its representative
    /// ([`outranks_at_address`]: the heaviest eligible id, the first in list
    /// order among equals), so every policy gives an address the share of
    /// one backend however many ids it carries (decided 2026-10-04). Runs on
    /// the candidates that passed the tier and shard filters, so a down or
    /// backing-off id never hides an eligible sibling. A filter: the
    /// candidates stay in list order, as `Candidates::new` requires.
    ///
    /// Free when no address is shared. Otherwise one pass over the
    /// candidates, where only an id with a sibling walks its address's ring
    /// (`sibling_ring`) and looks each sibling up among the candidates by
    /// binary search: linear in the `c` candidates plus, for each shared
    /// address of `g` ids, up to `g²` ring steps (about `g²/2` when the
    /// walks stop at the first outranking sibling), each with an
    /// `O(log c)` lookup — never a scan of the list per candidate.
    /// Allocates nothing (`collapsed` is reserved by `add_backend`).
    ///
    /// A list whose `backends` a caller grew or shrank directly, bypassing
    /// `add_backend`/`remove_backend`, has a ring of another length: the
    /// collapse is then skipped rather than indexing a stale ring.
    fn collapse_shared_addresses(&mut self) -> usize {
        if !self.shares_address || self.sibling_ring.len() != self.backends.len() {
            return self.candidates.len();
        }
        let backends = &self.backends;
        let candidates = &self.candidates;
        let ring = &self.sibling_ring;
        self.collapsed.clear();
        for &position in candidates {
            let mut outranked = false;
            let mut sibling = ring[position];
            if sibling != position {
                let backend = backends[position].borrow();
                while !outranked && sibling != position {
                    count_sibling_step();
                    outranked = candidates.binary_search(&sibling).is_ok()
                        && outranks_at_address(
                            &backends[sibling].borrow(),
                            sibling,
                            &backend,
                            position,
                        );
                    sibling = ring[sibling];
                }
            }
            if !outranked {
                self.collapsed.push(position);
            }
        }
        std::mem::swap(&mut self.candidates, &mut self.collapsed);
        // `collapsed` now holds the uncollapsed candidates: collapsing only
        // drops ids, and keeps one per address, so a non-empty set stays so.
        debug_assert!(
            self.candidates.len() <= self.collapsed.len(),
            "collapsing never adds a candidate"
        );
        debug_assert!(
            self.collapsed.is_empty() || !self.candidates.is_empty(),
            "every address keeps one candidate"
        );
        self.candidates.len()
    }

    /// Set or clear the cluster's shuffle sharding. Takes effect from the
    /// next selection.
    pub fn set_shuffle_sharding(&mut self, shuffle_sharding: Option<ShuffleSharding>) {
        self.shuffle_sharding = shuffle_sharding;
    }

    /// Fill `self.shard` with the positions of `key`'s shard, sorted, and
    /// say whether sharding applies to this selection at all.
    ///
    /// The shard is the top `k` of the HRW ranking of `key` over the
    /// **configured** primary addresses, healthy or not, with every primary
    /// id at those addresses (one address is one share, see
    /// `collapse_shared_addresses`): a backend going down
    /// must not pull another one into the shard, or a shard could never be
    /// exhausted and isolation would leak exactly when it matters. Ties are
    /// broken by list position so the shard is a function of the scores.
    /// Partitioning with `select_nth_unstable_by` is `O(N)`; each address's
    /// representative is precomputed (`shard_representative`), and a shared
    /// address brings its other ids in by walking its ring, so a shared
    /// address adds work only for its own ids. The buffers are reserved by
    /// `add_backend`, so this allocates nothing. If a caller grew or shrank
    /// the public `backends` directly, the precomputed state has another
    /// length and is ignored: every primary then ranks as its own address,
    /// as before shared addresses were collapsed, and nothing is indexed out
    /// of bounds.
    fn compute_shard(&mut self, key: Option<u64>) -> bool {
        let (Some(sharding), Some(key)) = (self.shuffle_sharding, key) else {
            return false;
        };
        // One address is one share: a shard counts and ranks primary
        // ADDRESSES, each scored through its representative primary id
        // (`outranks_at_address`), and takes in every primary id at the
        // addresses it selects, so the sibling of a down representative still
        // serves inside the shard. Without a shared address every primary is
        // its own representative.
        let backends = &self.backends;
        let shares_address = self.shares_address;
        let synced = self.shard_representative.len() == backends.len()
            && self.sibling_ring.len() == backends.len();
        let ring: &[usize] = if synced { &self.sibling_ring } else { &[] };
        // Size the shard before hashing anything, so a cluster below
        // `shard_min_backends` pays no walk and no hash.
        let primaries = if synced {
            self.shard_addresses
        } else {
            backends
                .iter()
                .filter(|backend| !backend.borrow().backup)
                .count()
        };
        let Some(k) = sharding.shard_size(primaries) else {
            return false;
        };
        self.shard_scores.clear();
        for (index, backend) in backends.iter().enumerate() {
            let represents = match self.shard_representative.get(index) {
                Some(&represents) if synced => represents,
                _ => !backend.borrow().backup,
            };
            if represents {
                self.shard_scores
                    .push((hrw_score(key, &backend.borrow()), index));
            }
        }
        debug_assert_eq!(
            self.shard_scores.len(),
            primaries,
            "one score per primary address"
        );
        let by_rank = |a: &(f64, usize), b: &(f64, usize)| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1));
        if k < self.shard_scores.len() {
            self.shard_scores.select_nth_unstable_by(k - 1, by_rank);
        }
        self.shard.clear();
        for &(_, representative) in &self.shard_scores[..k] {
            self.shard.push(representative);
            let next = |position: usize| ring.get(position).copied().unwrap_or(representative);
            let mut sibling = next(representative);
            while sibling != representative {
                count_sibling_step();
                if !backends[sibling].borrow().backup {
                    self.shard.push(sibling);
                }
                sibling = next(sibling);
            }
        }
        self.shard.sort_unstable();
        debug_assert!(
            self.shard.len() >= k,
            "a shard holds every primary id of its k addresses"
        );
        debug_assert!(
            shares_address || self.shard.len() == k,
            "without a shared address a shard holds exactly k backends"
        );
        debug_assert!(
            self.shard
                .iter()
                .all(|&index| !self.backends[index].borrow().backup),
            "a shard holds primary backends only"
        );
        true
    }

    /// Keep only the candidates that belong to the current shard; the
    /// candidates stay in list order, as `Candidates::new` requires.
    fn retain_shard_candidates(&mut self) -> usize {
        let shard = &self.shard;
        self.candidates
            .retain(|index| shard.binary_search(index).is_ok());
        self.candidates.len()
    }

    /// Remove the backend identified by `(backend_id, backend_address)`, the
    /// identity `AddBackend` and `ConfigState` key backends on, and return
    /// whether it was present. Another id at the same address (A/B test,
    /// weighted variant) is a distinct backend and stays (#1821).
    pub fn remove_backend(&mut self, backend_id: &str, backend_address: &SocketAddr) -> bool {
        let len_before = self.backends.len();
        let mut removed = false;
        self.backends.retain(|backend| {
            let mut b = backend.borrow_mut();
            if b.backend_id == backend_id && &b.address == backend_address {
                removed = true;
                // A session may still hold this backend: retire it so none
                // of its pooled connections takes a new request.
                b.set_closing();
                false
            } else {
                true
            }
        });
        // The list holds each `(backend_id, address)` at most once, so it
        // shrinks by exactly one entry when the identity was present.
        debug_assert_eq!(
            self.backends.len(),
            len_before - removed as usize,
            "remove_backend must drop exactly the backend it reports"
        );
        debug_assert!(
            self.find_backend_by_identity(backend_id, backend_address)
                .is_none(),
            "remove_backend must evict the backend it names"
        );
        // Rebuild table-based policies (Maglev) off the datapath after the set
        // shrinks, only when something was actually removed. No-op for the
        // stateless policies.
        if removed {
            self.load_balancing.rebuild(&self.backends);
            self.refresh_siblings();
        }
        #[cfg(debug_assertions)]
        self.check_invariants();
        removed
    }

    /// The live backend identified by `(backend_id, backend_address)`.
    pub fn find_backend_by_identity(
        &self,
        backend_id: &str,
        backend_address: &SocketAddr,
    ) -> Option<&Rc<RefCell<Backend>>> {
        self.backends.iter().find(|backend| {
            let b = backend.borrow();
            b.backend_id == backend_id && b.address == *backend_address
        })
    }

    /// The live entry that is the `incarnation` a health probe captured when
    /// it launched, or `None` once that backend left the list: removed, or
    /// replaced by a backend re-added under the same id and address, which is
    /// a new incarnation. Comparing allocations is sound because the `Weak`
    /// keeps its allocation alive, so no later backend can reuse it (#1821).
    pub fn find_incarnation(
        &self,
        incarnation: &Weak<RefCell<Backend>>,
    ) -> Option<&Rc<RefCell<Backend>>> {
        self.backends
            .iter()
            .find(|backend| std::ptr::eq(Rc::as_ptr(backend), incarnation.as_ptr()))
    }

    pub fn has_backend(&self, backend_address: &SocketAddr) -> bool {
        self.backends
            .iter()
            .any(|backend| backend.borrow().address == *backend_address)
    }

    pub fn find_backend(
        &mut self,
        backend_address: &SocketAddr,
    ) -> Option<&mut Rc<RefCell<Backend>>> {
        self.backends
            .iter_mut()
            .find(|backend| backend.borrow().address == *backend_address)
    }

    /// The backend `sticky_session` names, if it can take a connection at
    /// `now`.
    pub fn find_sticky(
        &mut self,
        sticky_session: &str,
        now: Instant,
    ) -> Option<&mut Rc<RefCell<Backend>>> {
        self.backends
            .iter_mut()
            .find(|b| b.borrow().sticky_id.as_deref() == Some(sticky_session))
            .filter(|b| b.borrow().can_open(now))
    }

    /// The backends of one tier that can take a connection now, cloned into
    /// a fresh `Vec`. Selection does not call this: it walks the same
    /// predicate, `is_tier_candidate`, into a reused buffer instead.
    pub fn available_backends(&mut self, backup: bool, now: Instant) -> Vec<Rc<RefCell<Backend>>> {
        self.backends
            .iter()
            .filter(|backend| is_tier_candidate(&backend.borrow(), backup, now))
            .map(Clone::clone)
            .collect()
    }

    /// Refill `candidates` with the position of every backend `keep` accepts,
    /// in list order, and return how many there are. Allocation-free once
    /// `add_backend` has reserved the buffer.
    fn collect_candidates(&mut self, keep: impl Fn(&Backend) -> bool) -> usize {
        self.candidates.clear();
        for (index, backend) in self.backends.iter().enumerate() {
            if keep(&backend.borrow()) {
                self.candidates.push(index);
            }
        }
        debug_assert!(
            self.candidates.len() <= self.backends.len(),
            "candidate set cannot be larger than the full backend list"
        );
        debug_assert!(
            self.candidates.windows(2).all(|pair| pair[0] < pair[1]),
            "candidate positions must follow list order"
        );
        self.candidates.len()
    }

    /// Pick the next available backend at `now`.
    pub fn next_available_backend(&mut self, now: Instant) -> Option<Rc<RefCell<Backend>>> {
        self.next_available_backend_with_key(None, now)
    }

    /// Pick the next available backend, optionally pinned by an affinity `key`.
    ///
    /// `key` is only consulted by consistent-hashing policies (HRW/Maglev);
    /// every other policy ignores it, so `next_available_backend_with_key(None)`
    /// is byte-for-byte the legacy behavior. The UDP datapath calls this with
    /// `Some(flow_hash)` to keep a client flow pinned to one backend; HTTP,
    /// HTTPS and TCP reach it through [`BackendMap::backend_from_cluster_id`]
    /// with the client affinity key of a cluster that uses HRW or Maglev.
    ///
    /// `now` is the instant the selection happens at: it decides which
    /// backends are out of their backoff window and how far each
    /// connection-time average has decayed, so the same `now` over the same
    /// state picks the same backend.
    pub fn next_available_backend_with_key(
        &mut self,
        key: Option<u64>,
        now: Instant,
    ) -> Option<Rc<RefCell<Backend>>> {
        self.select_with_key(key, now).0
    }

    /// [`Self::next_available_backend_with_key`], also saying how shuffle
    /// sharding shaped the selection, for [`BackendMap`] to count under the
    /// cluster's id.
    ///
    /// Tiers, in order: the primary backends that can open, then the backup
    /// backends, then fail-open over every backend whose retry policy allows
    /// a try. When the cluster shards and `key` is known, the primary tier is
    /// first narrowed to the client's shard. An exhausted shard then either
    /// spills over to the unsharded tiers (`FALLBACK`) or ends the selection
    /// empty (`STRICT`), except in the fail-open regime — no backend of the
    /// whole cluster can open — where `STRICT` fails open inside its shard
    /// only.
    pub(crate) fn select_with_key(
        &mut self,
        key: Option<u64>,
        now: Instant,
    ) -> (Option<Rc<RefCell<Backend>>>, ShardOutcome) {
        self.select_tiers(key, now, &[])
    }

    /// [`Self::select_with_key`] for a request that already failed to connect
    /// to the backends at the addresses in `exclude` (sozu-proxy/sozu#1800).
    ///
    /// Every policy picks from the candidate set this list builds, so leaving
    /// those backends out of it is what makes a retry skip them under round
    /// robin, random, least loaded, power of two, HRW and Maglev alike.
    ///
    /// The exclusion only narrows the healthy tiers, primary then backup. When
    /// it leaves both empty — every backend that can take a connection was
    /// already tried by this request — the selection runs again without it,
    /// exactly as [`Self::select_with_key`] would: the request retries a
    /// backend it already tried rather than failing, and the fail-open regime
    /// still decides when no backend can take a connection at all. A backend
    /// that failed is usually out of the healthy tiers already, its retry
    /// policy holding it in back-off; the exclusion matters when that back-off
    /// expires while the request is still retrying, which a connect timeout as
    /// long as the back-off makes routine.
    pub(crate) fn select_with_key_excluding(
        &mut self,
        key: Option<u64>,
        now: Instant,
        exclude: &[SocketAddr],
    ) -> (Option<Rc<RefCell<Backend>>>, ShardOutcome) {
        if !exclude.is_empty() {
            let selection = self.select_tiers(key, now, exclude);
            if selection.0.is_some() {
                debug_assert!(
                    selection
                        .0
                        .as_ref()
                        .is_some_and(|backend| !exclude.contains(&backend.borrow().address)),
                    "an excluding selection must not return an excluded backend"
                );
                return selection;
            }
        }
        self.select_tiers(key, now, &[])
    }

    /// The selection itself. A non-empty `exclude` removes those addresses
    /// from the healthy tiers and skips the fail-open regime, which
    /// [`Self::select_with_key_excluding`] reaches through a second call
    /// without exclusion.
    fn select_tiers(
        &mut self,
        key: Option<u64>,
        now: Instant,
        exclude: &[SocketAddr],
    ) -> (Option<Rc<RefCell<Backend>>>, ShardOutcome) {
        let candidate = |backend: &Backend, backup: bool| {
            is_tier_candidate(backend, backup, now) && !exclude.contains(&backend.address)
        };
        let sharded = self.compute_shard(key);
        let strict = sharded
            && self
                .shuffle_sharding
                .is_some_and(|sharding| sharding.mode == ShardMode::Strict);
        let mut outcome = if sharded {
            ShardOutcome::InShard
        } else {
            ShardOutcome::Unsharded
        };

        let mut available = self.collect_candidates(|backend| candidate(backend, false));
        if sharded {
            // Primary backends able to take a connection, inside the shard
            // or not: under `STRICT` they, and only they, decide between
            // refusing and failing open. Backups sit outside every shard, so
            // a healthy backup must not turn a strict shard's fail-open into
            // a refusal.
            let primaries_can_open = available != 0;
            let in_shard = self.retain_shard_candidates();
            if in_shard != 0 {
                available = in_shard;
            } else if strict {
                if primaries_can_open {
                    // Primary backends outside the shard could serve, but
                    // strict isolation forbids them.
                    return (None, ShardOutcome::Exhausted);
                }
                // Fail-open regime of the primary tier: fall through with an
                // empty primary tier; the backup tier is skipped below for a
                // strict shard, and fail-open is narrowed to the shard.
                available = 0;
            } else {
                outcome = ShardOutcome::SpilledOver;
                available = self.collect_candidates(|backend| candidate(backend, false));
            }
        }

        if available == 0 && !strict {
            available = self.collect_candidates(|backend| candidate(backend, true));
        }

        if available != 0 {
            // Healthy regime: log the fail-open exit transition exactly once.
            if self.fail_open_warned {
                info!(
                    "fail-open: cluster recovered, {} backends now healthy",
                    available
                );
                self.fail_open_warned = false;
            }
            self.collapse_shared_addresses();
            let picked = self.load_balancing.next_available_backend(
                key,
                Candidates::with_siblings(
                    &self.backends,
                    &self.candidates,
                    &self.sibling_ring,
                    now,
                ),
            );
            debug_assert!(
                picked.as_ref().is_none_or(|b| {
                    let addr = b.borrow().address;
                    self.backends.iter().any(|x| x.borrow().address == addr)
                }),
                "selection must return a backend present in the live list"
            );
            debug_assert!(
                outcome != ShardOutcome::InShard
                    || picked.as_ref().is_none_or(|b| {
                        let addr = b.borrow().address;
                        self.shard
                            .iter()
                            .any(|&index| self.backends[index].borrow().address == addr)
                    }),
                "an in-shard selection must return a member of the shard"
            );
            let outcome = self.settle_spill_outcome(outcome, picked.as_ref());
            return (picked, outcome);
        }

        // Fail-open: when no backend passes the full `can_open()` gate,
        // route to backends that are administratively `Normal` AND whose
        // retry policy reports `OKAY` (i.e., not currently in
        // exponential-backoff). This prevents a shared dependency outage
        // (e.g., database) from making the entire cluster unavailable while
        // still respecting the per-backend back-off window — hammering a
        // backend at line rate during its back-off would defeat the back-off
        // itself. Ref: Amazon "Implementing Health Checks".
        //
        // An excluding selection stops before this regime: it is decided by
        // the second, unexcluding call `select_with_key_excluding` makes.
        if !exclude.is_empty() {
            return (None, outcome);
        }
        let mut available = self.collect_candidates(|backend| {
            backend.status == BackendStatus::Normal
                && matches!(
                    backend.retry_policy.can_try(now),
                    Some(retry::RetryAction::OKAY)
                )
        });
        if strict {
            // A strict shard fails open inside itself only.
            available = self.retain_shard_candidates();
            if available == 0 {
                return (None, ShardOutcome::Exhausted);
            }
        }

        if available == 0 {
            return (None, outcome);
        }

        // Latched warning + per-decision counter: the warn! fires once on
        // regime entry; the counter is the operator-visible per-request
        // signal that does not drown logs under universal outage.
        if !self.fail_open_warned {
            warn!(
                "fail-open: all backends unhealthy, routing to {} normal backends with retry-policy OKAY",
                available
            );
            self.fail_open_warned = true;
        }
        count!(names::backend::FAIL_OPEN, 1);

        self.collapse_shared_addresses();
        let picked = self.load_balancing.next_available_backend(
            key,
            Candidates::with_siblings(&self.backends, &self.candidates, &self.sibling_ring, now),
        );
        let outcome = self.settle_spill_outcome(outcome, picked.as_ref());
        (picked, outcome)
    }

    /// A `FALLBACK` selection that left its exhausted shard but whose pick is
    /// a shard member after all — fail-open over the whole cluster can land
    /// back in the shard — stayed in the shard, and is not a spill-over.
    fn settle_spill_outcome(
        &self,
        outcome: ShardOutcome,
        picked: Option<&Rc<RefCell<Backend>>>,
    ) -> ShardOutcome {
        match (outcome, picked) {
            (ShardOutcome::SpilledOver, Some(picked))
                if self
                    .shard
                    .iter()
                    .any(|&index| Rc::ptr_eq(&self.backends[index], picked)) =>
            {
                ShardOutcome::InShard
            }
            _ => outcome,
        }
    }

    /// Replace the cluster's policy. `seed` seeds the policies that draw at
    /// random, `Random` and `PowerOfTwo`; the others ignore it.
    ///
    /// This is the only way to change the policy: it builds the Maglev
    /// lookup table from the current backends when it installs `Maglev`.
    /// Code outside this crate cannot assign the field directly:
    ///
    /// ```
    /// use sozu_command_lib::proto::command::LoadBalancingAlgorithms;
    /// use sozu_lib::backends::{Backend, BackendList};
    ///
    /// let mut list = BackendList::with_seed(1);
    /// list.add_backend(Backend::new("b1", "127.0.0.1:8080".parse().unwrap(), None, None, None));
    /// list.set_load_balancing_policy(LoadBalancingAlgorithms::Maglev, None, 1);
    /// ```
    ///
    /// ```compile_fail,E0616
    /// use sozu_lib::{backends::BackendList, load_balancing::Maglev};
    ///
    /// let mut list = BackendList::with_seed(1);
    /// list.load_balancing = Box::new(Maglev::new());
    /// ```
    pub fn set_load_balancing_policy(
        &mut self,
        load_balancing_policy: LoadBalancingAlgorithms,
        metric: Option<LoadMetric>,
        seed: u64,
    ) {
        match load_balancing_policy {
            LoadBalancingAlgorithms::RoundRobin => {
                self.load_balancing = Box::new(RoundRobin::new())
            }
            LoadBalancingAlgorithms::Random => {
                self.load_balancing = Box::new(Random::with_seed(seed))
            }
            LoadBalancingAlgorithms::LeastLoaded => {
                self.load_balancing = Box::new(LeastLoaded {
                    metric: metric.unwrap_or(LoadMetric::Connections),
                })
            }
            LoadBalancingAlgorithms::PowerOfTwo => {
                self.load_balancing = Box::new(PowerOfTwo::with_seed(
                    seed,
                    metric.unwrap_or(LoadMetric::Connections),
                ))
            }
            // Affinity policies. They consult the client key every datapath
            // derives (UDP affinity key; HTTP/HTTPS/TCP source IP, header or
            // cookie); with `None` they fall back to round-robin.
            LoadBalancingAlgorithms::Hrw => self.load_balancing = Box::new(Rendezvous::new()),
            LoadBalancingAlgorithms::Maglev => {
                let mut maglev = Maglev::new();
                // Seed the lookup table from the currently-known backends so
                // selection is correct before the first control-plane rebuild.
                maglev.rebuild(&self.backends);
                self.load_balancing = Box::new(maglev);
            }
        }
    }
}

/// Count what shuffle sharding did to one selection of `cluster_id`: a
/// spill-over that found a backend outside the shard, or a strict refusal.
/// A selection that stayed in its shard, or was not sharded, counts nothing.
fn record_shard_outcome(cluster_id: &str, outcome: ShardOutcome, picked: bool) {
    match outcome {
        ShardOutcome::SpilledOver if picked => {
            incr!(names::backend::SHARD_SPILLOVER, Some(cluster_id), None);
        }
        ShardOutcome::Exhausted => {
            debug_assert!(!picked, "a strict exhausted shard selects nothing");
            incr!(names::backend::SHARD_EXHAUSTED, Some(cluster_id), None);
        }
        ShardOutcome::Unsharded | ShardOutcome::InShard | ShardOutcome::SpilledOver => {}
    }
}

/// Whether `backend` is a candidate of the primary (`backup == false`) or the
/// backup tier: it belongs to that tier and can take a connection at `now`.
fn is_tier_candidate(backend: &Backend, backup: bool, now: Instant) -> bool {
    backend.backup == backup && backend.can_open(now)
}

#[cfg(test)]
mod backends_test {

    use std::{net::TcpListener, sync::mpsc::*, thread};

    use super::*;

    /// Start a TCP server that accepts connections, and return its address.
    ///
    /// The kernel picks the port (`:0`): a fixed port collides with any other
    /// process bound to it, including a concurrent `cargo test` of this crate,
    /// and fails the test with `AddrInUse` before it asserts anything.
    fn run_mock_tcp_server(stopper: Receiver<()>) -> SocketAddr {
        let mut run = true;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        thread::spawn(move || {
            while run {
                for _stream in listener.incoming() {
                    // accept connections
                    if let Ok(()) = stopper.try_recv() {
                        run = false;
                    }
                }
            }
        });
        addr
    }

    #[test]
    fn it_should_retrieve_a_backend_from_cluster_id_when_backends_have_been_recorded() {
        let mut backend_map = BackendMap::new();
        let cluster_id = "mycluster";

        let (sender, receiver) = channel();
        let backend_addr = run_mock_tcp_server(receiver);

        backend_map.add_backend(
            cluster_id,
            Backend::new(&format!("{cluster_id}-1"), backend_addr, None, None, None),
        );

        assert!(
            backend_map
                .backend_from_cluster_id(cluster_id, None, Instant::now())
                .is_ok()
        );
        sender.send(()).unwrap();
    }

    /// A session keeps the `Rc` of a backend it dialled after the map drops
    /// it; the mux pool reads that handle's status to refuse reusing its
    /// connection, so removal must leave it `Closing`.
    #[test]
    fn removing_a_backend_or_its_cluster_marks_it_closing() {
        let mut backend_map = BackendMap::new();
        let first: SocketAddr = "127.0.0.1:9001".parse().unwrap();
        let second: SocketAddr = "127.0.0.1:9002".parse().unwrap();
        backend_map.add_backend("foo", Backend::new("foo-1", first, None, None, None));
        backend_map.add_backend("foo", Backend::new("foo-2", second, None, None, None));
        let held: Vec<Rc<RefCell<Backend>>> = backend_map.backends["foo"].backends.clone();

        assert!(backend_map.remove_backend("foo", "foo-1", &first));
        assert_eq!(held[0].borrow().status, BackendStatus::Closing);
        assert_eq!(held[1].borrow().status, BackendStatus::Normal);

        backend_map.remove_cluster("foo");
        assert_eq!(held[1].borrow().status, BackendStatus::Closing);

        // The same address added again is a new backend, not the retired one.
        backend_map.add_backend("foo", Backend::new("foo-2", second, None, None, None));
        let readded = &backend_map.backends["foo"].backends[0];
        assert!(!Rc::ptr_eq(readded, &held[1]));
        assert_eq!(readded.borrow().status, BackendStatus::Normal);
    }

    /// `AddBackend` admits two ids at one address; removing one of them must
    /// keep the other routable (#1821).
    #[test]
    fn removing_one_backend_id_keeps_its_same_address_sibling() {
        let mut backend_map = BackendMap::new();
        let shared: SocketAddr = "127.0.0.1:9003".parse().unwrap();
        backend_map.add_backend("foo", Backend::new("foo-a", shared, None, None, None));
        backend_map.add_backend("foo", Backend::new("foo-b", shared, None, None, None));
        let held: Vec<Rc<RefCell<Backend>>> = backend_map.backends["foo"].backends.clone();

        assert!(backend_map.remove_backend("foo", "foo-a", &shared));
        assert_eq!(held[0].borrow().status, BackendStatus::Closing);
        assert_eq!(held[1].borrow().status, BackendStatus::Normal);
        let list = &backend_map.backends["foo"];
        assert_eq!(list.backends.len(), 1);
        assert!(Rc::ptr_eq(
            list.find_backend_by_identity("foo-b", &shared).unwrap(),
            &held[1]
        ));
        assert!(list.find_backend_by_identity("foo-a", &shared).is_none());

        // Removing it again, or an id never added, removes nothing.
        assert!(!backend_map.remove_backend("foo", "foo-a", &shared));
        assert!(!backend_map.remove_backend("foo", "foo-c", &shared));
        assert_eq!(backend_map.backends["foo"].backends.len(), 1);
    }

    #[test]
    fn it_should_not_retrieve_a_backend_from_cluster_id_when_backend_has_not_been_recorded() {
        let mut backend_map = BackendMap::new();
        let cluster_not_recorded = "not";
        backend_map.add_backend(
            "foo",
            Backend::new("foo-1", "127.0.0.1:9001".parse().unwrap(), None, None, None),
        );

        assert!(
            backend_map
                .backend_from_cluster_id(cluster_not_recorded, None, Instant::now())
                .is_err()
        );
    }

    #[test]
    fn it_should_not_retrieve_a_backend_from_cluster_id_when_backend_list_is_empty() {
        let mut backend_map = BackendMap::new();

        assert!(
            backend_map
                .backend_from_cluster_id("dumb", None, Instant::now())
                .is_err()
        );
    }

    #[test]
    fn it_should_retrieve_a_backend_from_sticky_session_when_the_backend_has_been_recorded() {
        let mut backend_map = BackendMap::new();
        let cluster_id = "mycluster";
        let sticky_session = "server-2";

        for (index, port) in [(1u16, 9001u16), (2, 9000), (3, 9002)] {
            backend_map.add_backend(
                cluster_id,
                Backend::new(
                    &format!("{cluster_id}-{index}"),
                    SocketAddr::from(([127, 0, 0, 1], port)),
                    Some(format!("server-{index}")),
                    None,
                    None,
                ),
            );
        }

        let backend = backend_map
            .reserve_sticky_backend(cluster_id, sticky_session, None, Instant::now())
            .expect("the backend the sticky session names can take a connection");
        let backend = backend.borrow();
        assert_eq!(
            backend.backend_id, "mycluster-2",
            "a sticky session must reach the backend it names"
        );
        assert_eq!(
            backend.active_connections, 1,
            "reaching it reserves a connection"
        );
    }

    #[test]
    fn it_should_not_retrieve_a_backend_from_sticky_session_when_the_backend_has_not_been_recorded()
    {
        let mut backend_map = BackendMap::new();
        let cluster_id = "mycluster";
        let sticky_session = "test";

        assert!(
            backend_map
                .reserve_sticky_backend(cluster_id, sticky_session, None, Instant::now())
                .is_err()
        );
    }

    #[test]
    fn it_should_not_retrieve_a_backend_from_sticky_session_when_the_backend_list_is_empty() {
        let mut backend_map = BackendMap::new();
        let mycluster_not_recorded = "mycluster";
        let sticky_session = "test";

        assert!(
            backend_map
                .reserve_sticky_backend(
                    mycluster_not_recorded,
                    sticky_session,
                    None,
                    Instant::now()
                )
                .is_err()
        );
    }

    #[test]
    fn it_should_add_a_backend_when_he_doesnt_already_exist() {
        let backend_id = "myback";
        let mut backends_list = BackendList::new();
        backends_list.add_backend(Backend::new(
            backend_id,
            "127.0.0.1:80".parse().unwrap(),
            None,
            None,
            None,
        ));

        assert_eq!(1, backends_list.backends.len());
    }

    #[test]
    fn it_should_not_add_a_backend_when_he_already_exist() {
        let backend_id = "myback";
        let mut backends_list = BackendList::new();
        backends_list.add_backend(Backend::new(
            backend_id,
            "127.0.0.1:80".parse().unwrap(),
            None,
            None,
            None,
        ));

        //same backend id
        backends_list.add_backend(Backend::new(
            backend_id,
            "127.0.0.1:80".parse().unwrap(),
            None,
            None,
            None,
        ));

        assert_eq!(1, backends_list.backends.len());
    }

    /// Build a backend addressed at 127.0.0.1:port and force it Unhealthy
    /// without going through the health-check loop.
    fn unhealthy_backend(id: &str, port: u16) -> Backend {
        let mut backend = Backend::new(
            id,
            format!("127.0.0.1:{port}").parse().unwrap(),
            None,
            None,
            None,
        );
        // Threshold = 1 transitions on the first failure.
        backend.health.record_failure(1);
        assert!(!backend.health.is_healthy());
        backend
    }

    #[test]
    fn fail_open_picks_normal_backend_in_retry_policy_okay() {
        // All backends are unhealthy but their retry policy is fresh (OKAY),
        // so fail-open must select one. A fresh ExponentialBackoffPolicy
        // returns OKAY on `can_try()` until the first `fail()` arms a wait
        // window.
        let mut list = BackendList::new();
        list.add_backend(unhealthy_backend("b1", 9001));
        list.add_backend(unhealthy_backend("b2", 9002));

        // Sanity: `available_backends` returns nothing (the regular path).
        assert!(list.available_backends(false, Instant::now()).is_empty());
        assert!(list.available_backends(true, Instant::now()).is_empty());

        let picked = list.next_available_backend(Instant::now());
        assert!(
            picked.is_some(),
            "fail-open must pick a Normal+OKAY backend"
        );
        assert!(list.fail_open_warned, "regime entry must latch the warn!");
    }

    #[test]
    fn fail_open_skips_backend_in_retry_backoff() {
        // Same shape as above, but each backend's retry policy is in the
        // WAIT window after a recorded failure. Fail-open must NOT pick any
        // of them — hammering a backend at line rate during its back-off
        // window is exactly what the back-off is protecting against — and
        // the regime-entry warn! must NOT latch (no log spam either).
        let mut list = BackendList::new();
        list.add_backend(unhealthy_backend("b1", 9011));
        list.add_backend(unhealthy_backend("b2", 9012));
        for backend_rc in &list.backends {
            backend_rc
                .borrow_mut()
                .retry_policy()
                .fail(Instant::now(), &mut rand::rng());
            assert_eq!(
                Some(retry::RetryAction::WAIT),
                backend_rc.borrow().retry_policy.can_try(Instant::now()),
                "test fixture must place retry policy in WAIT"
            );
        }

        let picked = list.next_available_backend(Instant::now());
        assert!(
            picked.is_none(),
            "fail-open must skip backends whose retry policy is in WAIT"
        );
        assert!(
            !list.fail_open_warned,
            "no candidate backends, no regime entry"
        );
    }

    #[test]
    fn fail_open_warn_latched() {
        // First call enters the regime → latch flips, warn! fires.
        // Second call stays in the regime → latch stays, no second warn!.
        // Recovering one backend → latch clears on the next routing call.
        let mut list = BackendList::new();
        list.add_backend(unhealthy_backend("b1", 9021));
        list.add_backend(unhealthy_backend("b2", 9022));

        assert!(list.next_available_backend(Instant::now()).is_some());
        assert!(list.fail_open_warned, "first fail-open must latch");

        assert!(list.next_available_backend(Instant::now()).is_some());
        assert!(
            list.fail_open_warned,
            "subsequent fail-open routing keeps the latch"
        );

        // Heal one backend — the next routing call takes the healthy path
        // and must clear the latch (regime exit logged once).
        list.backends[0].borrow_mut().health.status = HealthStatus::Healthy;
        let picked = list.next_available_backend(Instant::now());
        assert!(
            picked.is_some(),
            "regular path must select the healed backend"
        );
        assert!(
            !list.fail_open_warned,
            "regime exit must clear the latch so the next entry is logged again"
        );
    }

    // ── #892: per-cluster availability tracker ──────────────────────────

    /// Build a backend that passes the `evaluate_availability` predicate
    /// (`status == Normal && health.is_healthy() && !retry_policy.is_down()`).
    /// A `Backend::new` returns Normal/Healthy with a fresh
    /// ExponentialBackoffPolicy that reports `is_down() == false` until the
    /// first `fail()`, so this is just `Backend::new` with a stable address.
    fn healthy_backend(id: &str, port: u16) -> Backend {
        Backend::new(
            id,
            format!("127.0.0.1:{port}").parse().unwrap(),
            None,
            None,
            None,
        )
    }

    #[test]
    fn is_available_requires_health_status_and_retry_policy() {
        // Fresh backend: Healthy + Normal + retry_policy fresh (OKAY).
        let mut backend = Backend::new("b", "127.0.0.1:9050".parse().unwrap(), None, None, None);
        assert!(backend.is_available(), "fresh backend must be available");

        // Unhealthy fails the predicate even with everything else OK.
        backend.health.record_failure(1);
        assert!(!backend.is_available(), "unhealthy must not be available");

        // Restore health, then drive retry policy into the exhausted-budget
        // state via the test-only helper. Calling `fail()` in a tight loop
        // would early-return on the second invocation because the
        // exponential-backoff window has not elapsed yet, so the natural
        // path needs real-time sleeps the unit test cannot afford.
        backend.health.status = HealthStatus::Healthy;
        assert!(backend.is_available());
        backend.retry_policy.force_down();
        assert!(
            backend.retry_policy.is_down(),
            "test setup: retry policy budget must be exhausted",
        );
        assert!(
            !backend.is_available(),
            "retry-policy backoff must fail the predicate"
        );

        // Reset retry, switch lifecycle to Closing.
        backend.retry_policy.succeed(Instant::now());
        backend.set_closing();
        assert!(
            !backend.is_available(),
            "Closing lifecycle status must fail the predicate"
        );
    }

    #[test]
    fn evaluate_availability_empty_list_returns_zero_zero() {
        let list = BackendList::new();
        assert_eq!((0, 0), list.evaluate_availability());
    }

    #[test]
    fn evaluate_availability_counts_only_healthy_normal_not_in_backoff() {
        let mut list = BackendList::new();
        list.add_backend(healthy_backend("b-ok-1", 9101));
        list.add_backend(healthy_backend("b-ok-2", 9102));
        list.add_backend(unhealthy_backend("b-bad", 9103));
        let (available, total) = list.evaluate_availability();
        assert_eq!(3, total, "every configured backend counts toward total");
        assert_eq!(
            2, available,
            "only the two healthy backends pass the predicate"
        );
    }

    #[test]
    fn evaluate_availability_excludes_retry_policy_down() {
        let mut list = BackendList::new();
        list.add_backend(healthy_backend("b-fresh", 9111));
        list.add_backend(healthy_backend("b-fail", 9112));
        // Drive the second backend's retry policy into is_down() via the
        // test-only force_down() helper. A natural exhaustion would
        // require waiting through the exponential-backoff windows
        // (`fail()` early-returns when called inside one).
        list.backends[1].borrow_mut().retry_policy.force_down();
        let (available, total) = list.evaluate_availability();
        assert_eq!(2, total);
        assert_eq!(
            1, available,
            "retry-policy is_down() backend must be excluded even when health.is_healthy()"
        );
    }

    #[test]
    fn record_cluster_availability_flips_to_alldown_then_idempotent() {
        let mut map = BackendMap::new();
        let cluster_id = "c-flap";
        map.add_backend(cluster_id, unhealthy_backend("b1", 9201));
        // After add_backend the helper has run; total=1, available=0,
        // so the cell must already be AllDown.
        let list = map.backends.get(cluster_id).expect("cluster present");
        assert_eq!(
            ClusterAvailability::AllDown,
            list.availability.get(),
            "single unhealthy backend must drive the cell to AllDown"
        );
        // Calling again in the same regime is a no-op (Cell already AllDown).
        map.record_cluster_availability(cluster_id);
        let list = map.backends.get(cluster_id).expect("cluster present");
        assert_eq!(
            ClusterAvailability::AllDown,
            list.availability.get(),
            "repeat call must keep the cell at AllDown without flipping"
        );
    }

    #[test]
    fn record_cluster_availability_recovers_to_available() {
        let mut map = BackendMap::new();
        let cluster_id = "c-recover";
        map.add_backend(cluster_id, unhealthy_backend("b1", 9301));
        assert_eq!(
            ClusterAvailability::AllDown,
            map.backends.get(cluster_id).unwrap().availability.get()
        );
        // Heal the backend in place and re-evaluate. Without going through
        // a routing call the helper still fires from add_backend / HC, so
        // here we drive it manually.
        map.backends.get_mut(cluster_id).unwrap().backends[0]
            .borrow_mut()
            .health
            .status = HealthStatus::Healthy;
        map.record_cluster_availability(cluster_id);
        assert_eq!(
            ClusterAvailability::Available,
            map.backends.get(cluster_id).unwrap().availability.get(),
            "healed backend must flip the cell back to Available"
        );
    }

    #[test]
    fn record_cluster_availability_empty_cluster_stays_available() {
        let mut map = BackendMap::new();
        let cluster_id = "c-empty";
        map.backends.insert(cluster_id.into(), BackendList::new());
        // total == 0 path: never report AllDown — avoids log spam during
        // cluster bootstrap when backends are still being registered.
        map.record_cluster_availability(cluster_id);
        assert_eq!(
            ClusterAvailability::Available,
            map.backends.get(cluster_id).unwrap().availability.get(),
            "empty cluster must keep the cell at the default Available"
        );
    }

    #[test]
    fn record_cluster_availability_missing_cluster_is_noop() {
        let map = BackendMap::new();
        // No panic, no insert — just an early return.
        map.record_cluster_availability("c-absent");
        assert!(
            !map.backends.contains_key("c-absent"),
            "helper must not insert a BackendList for an unknown cluster_id"
        );
    }

    #[test]
    fn import_configuration_state_latches_cluster_rollup_gauges() {
        use crate::metrics::METRICS;
        use sozu_command_lib::proto::command::QueryMetricsOptions;
        // Unique cluster id so the assertion is not perturbed by gauges
        // left in the thread-local METRICS aggregator by sibling tests.
        let cluster_id = "c-import-rollup-9701";
        let mut map = BackendMap::new();
        let mut input = HashMap::new();
        input.insert(
            cluster_id.into(),
            vec![sozu_command_lib::response::Backend {
                cluster_id: cluster_id.into(),
                backend_id: "b1".to_owned(),
                address: "127.0.0.1:9701".parse().unwrap(),
                sticky_id: None,
                load_balancing_parameters: None,
                backup: None,
            }],
        );
        map.import_configuration_state(&input);
        let response = METRICS
            .with(|m| {
                m.borrow_mut().query(&QueryMetricsOptions {
                    metric_names: vec![
                        names::cluster::AVAILABLE_BACKENDS.to_owned(),
                        names::cluster::TOTAL_BACKENDS.to_owned(),
                    ],
                    cluster_ids: vec![cluster_id.to_owned()],
                    backend_ids: vec![],
                    list: false,
                    no_clusters: false,
                    workers: false,
                })
            })
            .expect("metrics query succeeds");
        let cluster_metrics = match response.content_type {
            Some(
                sozu_command_lib::proto::command::response_content::ContentType::WorkerMetrics(wm),
            ) => wm,
            Some(_) => panic!("expected WorkerMetrics, got another response variant"),
            None => panic!("expected WorkerMetrics, got no response content"),
        };
        let cm = cluster_metrics
            .clusters
            .get(cluster_id)
            .expect("imported cluster must have a ClusterMetrics entry");
        // Without the import-time `record_cluster_availability` call the
        // two rollup gauges would be absent here. The fix guarantees the
        // pair lands without waiting for any follow-up backend mutation.
        assert!(
            cm.cluster.contains_key(names::cluster::AVAILABLE_BACKENDS),
            "cluster.available_backends gauge must be latched at import time"
        );
        assert!(
            cm.cluster.contains_key(names::cluster::TOTAL_BACKENDS),
            "cluster.total_backends gauge must be latched at import time"
        );
    }

    #[test]
    fn set_health_check_config_none_re_emits_rollup_after_reset() {
        let mut map = BackendMap::new();
        let cluster_id = "c-hc-reset";
        // Seed the cluster with an unhealthy backend so `add_backend`
        // drives the `availability` cell to AllDown.
        map.add_backend(cluster_id, unhealthy_backend("b1", 9801));
        assert_eq!(
            ClusterAvailability::AllDown,
            map.backends.get(cluster_id).unwrap().availability.get(),
            "test setup: unhealthy backend must register the cell at AllDown"
        );
        // Disabling the health check resets backend health to the default
        // pristine state AND must re-emit the rollup so the cell reflects
        // the post-reset availability instead of the stale AllDown.
        map.set_health_check_config(cluster_id, None);
        assert_eq!(
            ClusterAvailability::Available,
            map.backends.get(cluster_id).unwrap().availability.get(),
            "set_health_check_config(None) must re-emit the rollup after \
             resetting backend health, otherwise dashboards stay stuck at AllDown"
        );
    }

    // ----- #1684: selection reads neither the wall clock nor an ambient RNG -----

    /// Four backends of `cluster_id` under `policy`, created at `now` in a map
    /// that seeded the policy, and the addresses of `draws` selections at `now`.
    fn draws(
        map: &mut BackendMap,
        cluster_id: &str,
        policy: LoadBalancingAlgorithms,
        now: Instant,
        draws: usize,
    ) -> Vec<SocketAddr> {
        map.set_load_balancing_policy_for_cluster(
            cluster_id,
            policy,
            Some(LoadMetric::Connections),
        );
        for index in 0..4u16 {
            map.add_backend(
                cluster_id,
                Backend::new_at(
                    &format!("{cluster_id}-{index}"),
                    SocketAddr::from(([127, 0, 0, 1], 9100 + index)),
                    None,
                    None,
                    None,
                    now,
                ),
            );
        }
        let list = map
            .backends
            .get_mut(cluster_id)
            .expect("the policy call created the list");
        (0..draws)
            .map(|_| {
                list.next_available_backend(now)
                    .expect("four fresh backends can all open")
                    .borrow()
                    .address
            })
            .collect()
    }

    /// #1684: a map built from a seed seeds every policy it creates from it,
    /// so one seed yields one sequence of picks.
    ///
    /// TO SEE THIS RED: build `BackendMap::with_seed` from `os_rng()` and
    /// ignore the seed; two maps of one seed then disagree.
    #[test]
    fn one_map_seed_yields_one_selection_sequence() {
        let now = Instant::now();
        for policy in [
            LoadBalancingAlgorithms::Random,
            LoadBalancingAlgorithms::PowerOfTwo,
        ] {
            let first = draws(&mut BackendMap::with_seed(7), "c", policy, now, 48);
            assert_eq!(
                first,
                draws(&mut BackendMap::with_seed(7), "c", policy, now, 48),
                "{policy:?}: one seed must yield one sequence of picks"
            );
            assert_ne!(
                first,
                draws(&mut BackendMap::with_seed(8), "c", policy, now, 48),
                "{policy:?}: the picks must come from the map's seed, so another seed differs"
            );
        }
    }

    /// The addresses of `draws` selections at `now` from `cluster_id`.
    fn picks_of(
        map: &mut BackendMap,
        cluster_id: &str,
        now: Instant,
        draws: usize,
    ) -> Vec<SocketAddr> {
        let list = map
            .backends
            .get_mut(cluster_id)
            .expect("the cluster was created");
        (0..draws)
            .map(|_| {
                list.next_available_backend(now)
                    .expect("fresh backends can all open")
                    .borrow()
                    .address
            })
            .collect()
    }

    /// #1684: a cluster the map creates while adding a backend, before any
    /// policy is set, is seeded from the map too, not from the OS.
    ///
    /// TO SEE THIS RED: create the list in `BackendMap::add_backend` with
    /// `or_default()`, which seeds its `Random` policy from the OS.
    #[test]
    fn a_cluster_created_by_adding_a_backend_is_seeded_by_the_map() {
        let now = Instant::now();
        let sequence = || {
            let mut map = BackendMap::with_seed(7);
            for index in 0..4u16 {
                map.add_backend(
                    "c",
                    Backend::new_at(
                        &format!("c-{index}"),
                        SocketAddr::from(([127, 0, 0, 1], 9300 + index)),
                        None,
                        None,
                        None,
                        now,
                    ),
                );
            }
            picks_of(&mut map, "c", now, 48)
        };
        assert_eq!(
            sequence(),
            sequence(),
            "a list created by add_backend must draw its seed from the map"
        );
    }

    /// #1684: importing a configuration state seeds each cluster from the
    /// map in cluster-id order, so one map seed gives every cluster the same
    /// seed whatever order the state's `HashMap` iterates in.
    ///
    /// TO SEE THIS RED: draw the seeds while iterating `backends` directly in
    /// `BackendMap::import_configuration_state`; each `HashMap` has its own
    /// `RandomState`, so two imports hand the seeds out in different orders.
    #[test]
    fn importing_a_state_seeds_its_clusters_in_a_fixed_order() {
        const CLUSTERS: u16 = 16;
        let now = Instant::now();
        let state = |ids: &mut dyn Iterator<Item = u16>| {
            let mut state: HashMap<ClusterId, Vec<sozu_command_lib::response::Backend>> =
                HashMap::new();
            for id in ids {
                let cluster_id: ClusterId = format!("import-{id:02}").into();
                let backends = (0..4u16)
                    .map(|index| sozu_command_lib::response::Backend {
                        cluster_id: cluster_id.clone(),
                        backend_id: format!("{cluster_id}-{index}"),
                        address: SocketAddr::from(([127, 0, 0, 1], 9400 + 4 * id + index)),
                        sticky_id: None,
                        load_balancing_parameters: None,
                        backup: None,
                    })
                    .collect();
                state.insert(cluster_id, backends);
            }
            state
        };
        let forward = state(&mut (0..CLUSTERS));
        let backward = state(&mut (0..CLUSTERS).rev());

        let mut first = BackendMap::with_seed(7);
        first.import_configuration_state(&forward);
        let mut second = BackendMap::with_seed(7);
        second.import_configuration_state(&backward);
        for id in 0..CLUSTERS {
            let cluster_id = format!("import-{id:02}");
            assert_eq!(
                picks_of(&mut first, &cluster_id, now, 32),
                picks_of(&mut second, &cluster_id, now, 32),
                "{cluster_id}: one map seed must give each imported cluster one seed"
            );
        }
    }

    /// Regression guard moved from `load_balancing.rs`, where it held
    /// `Random::new()`: the production seed must never be a shared constant.
    /// If it were, every worker, on every cold start, would draw from a
    /// bit-for-bit identical keystream and pick the same backend at every
    /// step — the correlated-load event uniform selection exists to prevent,
    /// arriving at a synchronised redeploy. The OS seed now enters through
    /// `BackendMap::new`, which seeds every cluster's policy, so two maps (two
    /// workers) and two clusters of one map must each diverge.
    ///
    /// 48 draws over four backends collide with probability ~4^-48 if the
    /// keystreams are independent, so this cannot flake.
    #[test]
    fn random_policies_seeded_by_the_os_are_not_correlated() {
        let now = Instant::now();
        let policy = LoadBalancingAlgorithms::Random;
        let mut map = BackendMap::new();
        let first = draws(&mut map, "a", policy, now, 48);
        assert_ne!(
            first,
            draws(&mut BackendMap::new(), "a", policy, now, 48),
            "two BackendMap::new() must NOT seed Random from a shared/correlated keystream"
        );
        assert_ne!(
            first,
            draws(&mut map, "b", policy, now, 48),
            "two clusters of one map must NOT draw from a shared keystream"
        );
    }

    /// The same guard for `PowerOfTwo`'s tie-break, moved from
    /// `load_balancing.rs`. Every backend starts at zero load, so a tie (and
    /// its coin flip) is the common case: a shared seed would resolve every
    /// worker's first ties identically after a synchronised redeploy,
    /// reintroducing the herding P2C exists to prevent.
    #[test]
    fn power_of_two_policies_seeded_by_the_os_are_not_correlated() {
        let now = Instant::now();
        let policy = LoadBalancingAlgorithms::PowerOfTwo;
        let mut map = BackendMap::new();
        let first = draws(&mut map, "a", policy, now, 48);
        assert_ne!(
            first,
            draws(&mut BackendMap::new(), "a", policy, now, 48),
            "two BackendMap::new() must NOT seed PowerOfTwo from a shared/correlated keystream"
        );
        assert_ne!(
            first,
            draws(&mut map, "b", policy, now, 48),
            "two clusters of one map must NOT draw from a shared keystream"
        );
    }

    /// #1684: the connection-time average decays on the instants it is
    /// given. A backend created at `t0` and read at `t0 + 1s` has aged by
    /// exactly one decay constant, `e^-1`, whatever the wall clock did.
    ///
    /// TO SEE THIS RED: take `now` from `Instant::now()` inside
    /// `PeakEWMA::observe`; almost no real time passes, so the average does
    /// not decay and the cost stays at its 50 ms default.
    #[test]
    fn the_connection_time_average_decays_on_the_injected_clock() {
        let t0 = Instant::now();
        let mut backend = Backend::new_at(
            "ewma",
            SocketAddr::from(([127, 0, 0, 1], 9200)),
            None,
            None,
            None,
            t0,
        );
        let default_rtt = backend.connection_time.rtt;
        let cost = backend.peak_ewma_connection(t0 + Duration::from_secs(1));
        assert_eq!(
            cost,
            default_rtt * (-1f64).exp(),
            "one second after creation, the average must have decayed by e^-1"
        );

        // One sequence of instants, one average, bit for bit.
        let replay = |backend: &mut Backend| {
            backend.set_connection_time(Duration::from_millis(80), t0 + Duration::from_millis(10));
            backend.set_connection_time(Duration::from_millis(20), t0 + Duration::from_millis(900));
            backend.peak_ewma_connection(t0 + Duration::from_millis(2_500))
        };
        let mut twin = Backend::new_at(
            "ewma",
            SocketAddr::from(([127, 0, 0, 1], 9200)),
            None,
            None,
            None,
            t0,
        );
        let mut again = Backend::new_at(
            "ewma",
            SocketAddr::from(([127, 0, 0, 1], 9200)),
            None,
            None,
            None,
            t0,
        );
        assert_eq!(
            replay(&mut twin).to_bits(),
            replay(&mut again).to_bits(),
            "one sequence of instants must yield one average, bit for bit"
        );
    }

    // ----- #524: shuffle sharding over the HRW ranking -----

    /// `n` primary backends on distinct loopback ports, under `policy`, sharded
    /// by `sharding`.
    fn sharded_list(
        n: u16,
        policy: LoadBalancingAlgorithms,
        sharding: Option<ShuffleSharding>,
    ) -> BackendList {
        let now = Instant::now();
        let mut list = BackendList::with_seed(524);
        list.set_load_balancing_policy(policy, Some(LoadMetric::Connections), 524);
        for index in 0..n {
            list.add_backend(Backend::new_at(
                &format!("shard-{index}"),
                SocketAddr::from(([127, 0, 0, 1], 20_000 + index)),
                None,
                None,
                None,
                now,
            ));
        }
        list.set_shuffle_sharding(sharding);
        list
    }

    fn sharding(percent: u32, min_backends: u32, mode: ShardMode) -> Option<ShuffleSharding> {
        Some(ShuffleSharding {
            percent,
            min_backends,
            mode,
        })
    }

    /// The addresses of `key`'s shard in `list`, and its members ranked best
    /// first.
    fn shard_of(list: &mut BackendList, key: u64) -> Vec<SocketAddr> {
        assert!(list.compute_shard(Some(key)), "the list must shard");
        let mut ranked: Vec<(f64, SocketAddr)> = list
            .shard
            .iter()
            .map(|&index| {
                let backend = list.backends[index].borrow();
                (hrw_score(key, &backend), backend.address)
            })
            .collect();
        ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
        ranked.into_iter().map(|(_, address)| address).collect()
    }

    /// #524: a shard is the top `k` of the key's HRW ranking over every
    /// primary backend — the same backends whatever their order in the list,
    /// on every call.
    #[test]
    fn a_shard_is_the_top_of_the_hrw_ranking() {
        let unweighted_list = || {
            sharded_list(
                12,
                LoadBalancingAlgorithms::RoundRobin,
                sharding(25, 8, ShardMode::Fallback),
            )
        };
        let mut unweighted = unweighted_list();
        // The same backends with non-uniform weights: the ranking is the
        // weighted HRW score, so heavier backends rank higher more often.
        let mut weighted = unweighted_list();
        for (index, backend) in weighted.backends.iter().enumerate() {
            backend.borrow_mut().load_balancing_parameters = Some(LoadBalancingParams {
                weight: 10 + 90 * (index as i32 % 4),
            });
        }
        let mut weights_moved_a_shard = false;
        for list in [&mut unweighted, &mut weighted] {
            for key in 0..64u64 {
                let shard = shard_of(list, key);
                assert_eq!(shard.len(), 3, "25% of 12 backends is a shard of 3");
                assert_eq!(shard, shard_of(list, key), "a shard is deterministic");
                let mut everyone: Vec<(f64, SocketAddr)> = list
                    .backends
                    .iter()
                    .map(|backend| {
                        let backend = backend.borrow();
                        (hrw_score(key, &backend), backend.address)
                    })
                    .collect();
                everyone.sort_by(|a, b| b.0.total_cmp(&a.0));
                let top: Vec<SocketAddr> = everyone.iter().take(3).map(|(_, a)| *a).collect();
                assert_eq!(shard, top, "key {key}: the shard is the HRW top 3");
            }
        }
        for key in 0..64u64 {
            weights_moved_a_shard |= shard_of(&mut unweighted, key) != shard_of(&mut weighted, key);
        }
        assert!(
            weights_moved_a_shard,
            "the weighted ranking must differ from the unweighted one for some key"
        );
    }

    /// #524: a change of N moves a client only at the tail of its ranking.
    /// Adding a backend with k unchanged replaces at most the lowest-ranked
    /// member; adding one that makes k grow keeps every member; removing a
    /// non-member changes nothing, and removing a member keeps the others.
    ///
    /// TO SEE THIS RED: rank by list position instead of HRW score in
    /// `BackendList::compute_shard`; adding a backend then reshuffles shards
    /// that should not move.
    #[test]
    fn a_shard_changes_only_at_its_tail_when_backends_change() {
        let now = Instant::now();
        let extra = |port: u16| {
            Backend::new_at(
                &format!("extra-{port}"),
                SocketAddr::from(([127, 0, 0, 1], port)),
                None,
                None,
                None,
                now,
            )
        };
        for key in 0..128u64 {
            // 10% of 10 and of 11 both give k = max(2, ceil(1.0 | 1.1)) = 2,
            // so k stays 2 across the add.
            let mut list = sharded_list(
                10,
                LoadBalancingAlgorithms::RoundRobin,
                sharding(10, 8, ShardMode::Fallback),
            );
            let before = shard_of(&mut list, key);
            list.add_backend(extra(21_000));
            let after = shard_of(&mut list, key);
            let lost: Vec<_> = before.iter().filter(|a| !after.contains(a)).collect();
            assert!(lost.len() <= 1, "key {key}: at most one member displaced");
            if let Some(lost) = lost.first() {
                assert_eq!(
                    Some(*lost),
                    before.last(),
                    "key {key}: only the lowest-ranked member may be displaced"
                );
            }

            // k grows with N: every member stays.
            let mut list = sharded_list(
                10,
                LoadBalancingAlgorithms::RoundRobin,
                sharding(20, 8, ShardMode::Fallback),
            );
            let before = shard_of(&mut list, key);
            list.add_backend(extra(21_001));
            let after = shard_of(&mut list, key);
            assert_eq!((before.len(), after.len()), (2, 3));
            assert!(
                before.iter().all(|a| after.contains(a)),
                "key {key}: a growing shard keeps its members"
            );

            // Removing a non-member changes nothing; removing a member keeps
            // the others.
            let mut list = sharded_list(
                10,
                LoadBalancingAlgorithms::RoundRobin,
                sharding(20, 8, ShardMode::Fallback),
            );
            let before = shard_of(&mut list, key);
            let outsider = list
                .backends
                .iter()
                .map(|b| b.borrow().address)
                .find(|a| !before.contains(a))
                .expect("a shard of 2 among 10 leaves outsiders");
            // `sharded_list` names each backend after its port.
            let id_of = |address: &SocketAddr| format!("shard-{}", address.port() - 20_000);
            assert!(list.remove_backend(&id_of(&outsider), &outsider));
            assert_eq!(shard_of(&mut list, key), before, "key {key}");
            assert!(list.remove_backend(&id_of(&before[0]), &before[0]));
            assert!(
                shard_of(&mut list, key).contains(&before[1]),
                "key {key}: removing one member keeps the other"
            );
        }
    }

    /// #524: every selection of a keyed client stays in its shard, under a
    /// policy that would otherwise visit every backend.
    #[test]
    fn a_sharded_selection_stays_in_the_shard() {
        let now = Instant::now();
        for policy in [
            LoadBalancingAlgorithms::RoundRobin,
            LoadBalancingAlgorithms::LeastLoaded,
            LoadBalancingAlgorithms::Random,
            LoadBalancingAlgorithms::Hrw,
        ] {
            let mut list = sharded_list(8, policy, sharding(25, 8, ShardMode::Fallback));
            for key in 0..32u64 {
                let shard = shard_of(&mut list, key);
                for _ in 0..8 {
                    let (picked, outcome) = list.select_with_key(Some(key), now);
                    let picked = picked.expect("a healthy shard selects").borrow().address;
                    assert_eq!(outcome, ShardOutcome::InShard);
                    assert!(
                        shard.contains(&picked),
                        "{policy:?} key {key}: {picked} is outside {shard:?}"
                    );
                }
            }
            // No key, or fewer primaries than the threshold: not sharded.
            assert_eq!(list.select_with_key(None, now).1, ShardOutcome::Unsharded);
            let mut small = sharded_list(7, policy, sharding(25, 8, ShardMode::Fallback));
            assert_eq!(
                small.select_with_key(Some(1), now).1,
                ShardOutcome::Unsharded
            );
        }
    }

    /// Put every member of `key`'s shard out of selection, as a failed
    /// connection does: its retry policy enters its back-off window.
    fn fail_shard(list: &mut BackendList, key: u64) -> Vec<SocketAddr> {
        let shard = shard_of(list, key);
        let now = Instant::now();
        for backend in &list.backends {
            let mut backend = backend.borrow_mut();
            if shard.contains(&backend.address) {
                backend.retry_policy.fail(now, &mut rand::rng());
            }
        }
        shard
    }

    /// #524: with its whole shard down, `STRICT` selects nothing although the
    /// rest of the cluster is healthy, and `FALLBACK` spills over to a backend
    /// outside the shard.
    ///
    /// TO SEE THIS RED: in `BackendList::select_with_key`, spill over in both
    /// modes (drop the `strict` early return); the strict selection then
    /// finds a backend outside the shard.
    #[test]
    fn an_exhausted_shard_refuses_in_strict_and_spills_in_fallback() {
        let now = Instant::now();
        for key in 0..32u64 {
            let mut strict = sharded_list(
                8,
                LoadBalancingAlgorithms::RoundRobin,
                sharding(25, 8, ShardMode::Strict),
            );
            fail_shard(&mut strict, key);
            let (picked, outcome) = strict.select_with_key(Some(key), now);
            assert!(picked.is_none(), "key {key}: strict never leaves the shard");
            assert_eq!(outcome, ShardOutcome::Exhausted);

            let mut fallback = sharded_list(
                8,
                LoadBalancingAlgorithms::RoundRobin,
                sharding(25, 8, ShardMode::Fallback),
            );
            let shard = fail_shard(&mut fallback, key);
            let (picked, outcome) = fallback.select_with_key(Some(key), now);
            let picked = picked.expect("fallback spills over").borrow().address;
            assert_eq!(outcome, ShardOutcome::SpilledOver);
            assert!(
                !shard.contains(&picked),
                "key {key}: the spill leaves the shard"
            );
        }
    }

    /// #524: a strict shard fails open inside itself only. When no backend of
    /// the whole cluster passes its health check (the fail-open regime), a
    /// strict selection still picks a shard member.
    #[test]
    fn a_strict_shard_fails_open_inside_itself() {
        let now = Instant::now();
        for key in 0..16u64 {
            let mut list = sharded_list(
                8,
                LoadBalancingAlgorithms::RoundRobin,
                sharding(25, 8, ShardMode::Strict),
            );
            let shard = shard_of(&mut list, key);
            for backend in &list.backends {
                backend.borrow_mut().health.status = HealthStatus::Unhealthy;
            }
            let (picked, outcome) = list.select_with_key(Some(key), now);
            let picked = picked.expect("fail-open still routes").borrow().address;
            assert!(
                shard.contains(&picked),
                "key {key}: fail-open stays in the shard"
            );
            assert_eq!(outcome, ShardOutcome::InShard);
        }
    }

    /// Fail every member of `key`'s shard by health check, as a probe does:
    /// they stay `Normal` with an `OKAY` retry policy, so fail-open may still
    /// pick them.
    fn unhealthy_shard(list: &mut BackendList, key: u64) -> Vec<SocketAddr> {
        let shard = shard_of(list, key);
        for backend in &list.backends {
            let mut backend = backend.borrow_mut();
            if shard.contains(&backend.address) {
                backend.health.status = HealthStatus::Unhealthy;
            }
        }
        shard
    }

    /// #524 review L3: `STRICT` refuses when its shard fails its health
    /// checks while primary backends outside it are healthy — the fail-open
    /// regime is not reached, because the cluster is not all down.
    ///
    /// TO SEE THIS RED: make the `primaries_can_open` guard of
    /// `BackendList::select_with_key` false; the strict shard then fails open
    /// and picks one of its unhealthy members.
    #[test]
    fn a_strict_shard_failing_health_checks_refuses_while_others_are_healthy() {
        let now = Instant::now();
        for key in 0..32u64 {
            let mut list = sharded_list(
                8,
                LoadBalancingAlgorithms::RoundRobin,
                sharding(25, 8, ShardMode::Strict),
            );
            unhealthy_shard(&mut list, key);
            let (picked, outcome) = list.select_with_key(Some(key), now);
            assert!(
                picked.is_none(),
                "key {key}: strict must refuse, not fail open"
            );
            assert_eq!(outcome, ShardOutcome::Exhausted);
        }
    }

    /// #524 review M1: backups sit outside every shard, so a healthy backup
    /// must not decide a strict shard's fate. With every primary failing its
    /// health checks, a strict shard fails open inside itself whether or not
    /// a backup is healthy.
    ///
    /// TO SEE THIS RED: count the backup tier in `primaries_can_open`
    /// (`available != 0 || <any backup can open>`); the healthy backup then
    /// turns the fail-open into a refusal.
    #[test]
    fn a_healthy_backup_does_not_turn_a_strict_fail_open_into_a_refusal() {
        let now = Instant::now();
        for key in 0..32u64 {
            let mut list = sharded_list(
                8,
                LoadBalancingAlgorithms::RoundRobin,
                sharding(25, 8, ShardMode::Strict),
            );
            let shard = shard_of(&mut list, key);
            for backend in &list.backends {
                backend.borrow_mut().health.status = HealthStatus::Unhealthy;
            }
            list.add_backend(Backend::new_at(
                "healthy-backup",
                SocketAddr::from(([127, 0, 0, 1], 23_000)),
                None,
                None,
                Some(true),
                now,
            ));
            let (picked, outcome) = list.select_with_key(Some(key), now);
            let picked = picked
                .expect("a strict shard fails open inside itself")
                .borrow()
                .address;
            assert!(
                shard.contains(&picked),
                "key {key}: never the backup, never outside"
            );
            assert_eq!(outcome, ShardOutcome::InShard);
        }
    }

    /// #524 review L2: in `FALLBACK`, fail-open over the whole cluster can
    /// pick a member of the exhausted shard; that pick stayed in the shard
    /// and must not count as a spill-over.
    ///
    /// TO SEE THIS RED: drop the `settle_spill_outcome` call after the
    /// fail-open pick in `BackendList::select_with_key`; an in-shard pick is
    /// then reported `SpilledOver`.
    #[test]
    fn a_fallback_fail_open_pick_inside_the_shard_is_not_a_spill_over() {
        let now = Instant::now();
        let key = 3;
        let mut list = sharded_list(
            8,
            LoadBalancingAlgorithms::RoundRobin,
            sharding(25, 8, ShardMode::Fallback),
        );
        let shard = shard_of(&mut list, key);
        for backend in &list.backends {
            backend.borrow_mut().health.status = HealthStatus::Unhealthy;
        }
        let (mut in_shard, mut outside) = (0, 0);
        // Round-robin over the fail-open set visits every backend in 8 picks.
        for _ in 0..8 {
            let (picked, outcome) = list.select_with_key(Some(key), now);
            let picked = picked.expect("fail-open routes").borrow().address;
            if shard.contains(&picked) {
                assert_eq!(outcome, ShardOutcome::InShard, "{picked} is a shard member");
                in_shard += 1;
            } else {
                assert_eq!(outcome, ShardOutcome::SpilledOver, "{picked} is outside");
                outside += 1;
            }
        }
        assert_eq!(
            (in_shard, outside),
            (2, 6),
            "round-robin visits every backend once"
        );
    }

    /// #524: a sticky cookie naming a live backend wins, even outside the
    /// client's shard.
    #[test]
    fn a_sticky_backend_outside_the_shard_wins() {
        let now = Instant::now();
        let key = 7;
        let mut map = BackendMap::with_seed(524);
        map.set_load_balancing_policy_for_cluster(
            "sharded",
            LoadBalancingAlgorithms::RoundRobin,
            None,
        );
        for index in 0..8u16 {
            map.add_backend(
                "sharded",
                Backend::new_at(
                    &format!("shard-{index}"),
                    SocketAddr::from(([127, 0, 0, 1], 22_000 + index)),
                    Some(format!("sticky-{index}")),
                    None,
                    None,
                    now,
                ),
            );
        }
        map.set_shuffle_sharding_for_cluster("sharded", sharding(25, 8, ShardMode::Strict));
        let list = map.backends.get_mut("sharded").expect("the cluster exists");
        let shard = shard_of(list, key);
        let outsider = list
            .backends
            .iter()
            .find(|b| !shard.contains(&b.borrow().address))
            .map(|b| {
                b.borrow()
                    .sticky_id
                    .clone()
                    .expect("every backend has a sticky id")
            })
            .expect("a shard of 2 among 8 leaves outsiders");
        let reserved = map
            .reserve_sticky_backend("sharded", &outsider, Some(key), now)
            .expect("the sticky backend is live");
        assert_eq!(
            reserved.borrow().sticky_id.as_deref(),
            Some(outsider.as_str())
        );
        assert!(!shard.contains(&reserved.borrow().address));
    }

    /// #524: a sharded selection allocates nothing: the scores, the shard and
    /// the candidates live in buffers `add_backend` reserved.
    ///
    /// TO SEE THIS RED: collect the shard into a fresh `Vec` in
    /// `BackendList::compute_shard`; each selection then allocates.
    #[test]
    fn a_sharded_selection_allocates_nothing() {
        use std::hint::black_box;

        use crate::test_allocations::allocations;

        let now = Instant::now();
        let mut list = sharded_list(
            16,
            LoadBalancingAlgorithms::LeastLoaded,
            sharding(25, 8, ShardMode::Fallback),
        );
        // Warm-up: the first selection may settle lazily built policy state.
        drop(list.select_with_key(Some(0), now));
        let mut allocated = 0;
        for key in 0..256u64 {
            let before = allocations();
            let picked = black_box(&mut list).select_with_key(black_box(Some(key)), now);
            allocated += allocations() - before;
            assert_eq!(picked.1, ShardOutcome::InShard);
        }
        assert_eq!(
            allocated, 0,
            "256 sharded selections made {allocated} allocations"
        );
    }
}

/// A retry skips the backends its request already failed to connect to,
/// under every load-balancing policy (sozu-proxy/sozu#1800).
#[cfg(test)]
mod exclusion_tests {
    use super::*;

    const POLICIES: [LoadBalancingAlgorithms; 6] = [
        LoadBalancingAlgorithms::RoundRobin,
        LoadBalancingAlgorithms::Random,
        LoadBalancingAlgorithms::LeastLoaded,
        LoadBalancingAlgorithms::PowerOfTwo,
        LoadBalancingAlgorithms::Hrw,
        LoadBalancingAlgorithms::Maglev,
    ];

    fn address(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    /// A list of `count` healthy backends on ports 9100.. under `policy`.
    fn list(policy: LoadBalancingAlgorithms, count: u16) -> BackendList {
        let mut list = BackendList::with_seed(7);
        for index in 0..count {
            list.add_backend(Backend::new(
                &format!("b{index}"),
                address(9100 + index),
                None,
                None,
                None,
            ));
        }
        list.set_load_balancing_policy(policy, None, 11);
        list
    }

    #[test]
    fn a_selection_never_returns_an_excluded_backend_under_any_policy() {
        let now = Instant::now();
        let exclude = [address(9100), address(9101), address(9102)];
        for policy in POLICIES {
            let mut list = list(policy, 4);
            for key in 0..32 {
                // HRW and Maglev pin on the key; the others ignore it.
                let (picked, _) = list.select_with_key_excluding(Some(key), now, &exclude);
                let picked = picked
                    .expect("one backend is not excluded")
                    .borrow()
                    .address;
                assert_eq!(
                    picked,
                    address(9103),
                    "{policy:?} picked {picked}, which the request already tried"
                );
            }
        }
    }

    #[test]
    fn an_empty_exclusion_selects_exactly_as_before() {
        let now = Instant::now();
        for policy in POLICIES {
            let mut excluding = list(policy, 4);
            let mut plain = list(policy, 4);
            for key in 0..16 {
                let left = excluding
                    .select_with_key_excluding(Some(key), now, &[])
                    .0
                    .map(|backend| backend.borrow().address);
                let right = plain
                    .select_with_key(Some(key), now)
                    .0
                    .map(|backend| backend.borrow().address);
                assert_eq!(left, right, "{policy:?} diverged on key {key}");
            }
        }
    }

    #[test]
    fn excluding_every_backend_falls_back_to_the_plain_selection() {
        let now = Instant::now();
        let exclude = [address(9100), address(9101)];
        for policy in POLICIES {
            let mut list = list(policy, 2);
            let (picked, _) = list.select_with_key_excluding(Some(3), now, &exclude);
            assert!(
                picked.is_some(),
                "{policy:?}: a request that tried every backend retries one of them"
            );
        }
    }

    #[test]
    fn an_excluded_sticky_backend_is_not_followed() {
        let now = Instant::now();
        let mut map = BackendMap::with_seed(5);
        for index in 0..2u16 {
            map.add_backend(
                "cluster",
                Backend::new(
                    &format!("b{index}"),
                    address(9200 + index),
                    Some(format!("sticky{index}")),
                    None,
                    None,
                ),
            );
        }
        let followed = map
            .reserve_sticky_backend_excluding("cluster", "sticky0", None, now, &[])
            .expect("a backend is selectable");
        assert_eq!(followed.borrow().address, address(9200));
        let skipped = map
            .reserve_sticky_backend_excluding("cluster", "sticky0", None, now, &[address(9200)])
            .expect("a backend is selectable");
        assert_eq!(
            skipped.borrow().address,
            address(9201),
            "the cookie names a backend the request already failed to reach"
        );
    }
}

/// Two backend ids at one address share that address's load, under every
/// policy: the address is one candidate, whatever the number of ids
/// configured on it.
#[cfg(test)]
mod same_address_tests {
    use super::*;
    use crate::load_balancing::affinity_key_from_value;

    const POLICIES: [LoadBalancingAlgorithms; 6] = [
        LoadBalancingAlgorithms::RoundRobin,
        LoadBalancingAlgorithms::Random,
        LoadBalancingAlgorithms::LeastLoaded,
        LoadBalancingAlgorithms::PowerOfTwo,
        LoadBalancingAlgorithms::Hrw,
        LoadBalancingAlgorithms::Maglev,
    ];

    /// Picks per measured distribution.
    const PICKS: u32 = 20_000;

    const SHARED: SocketAddr = SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
        9300,
    );
    const OTHER: SocketAddr = SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
        9301,
    );

    fn weighted(id: &str, address: SocketAddr, weight: Option<i32>) -> Backend {
        Backend::new(
            id,
            address,
            None,
            weight.map(|weight| LoadBalancingParams { weight }),
            None,
        )
    }

    /// `a` and `b` at [`SHARED`], `c` at [`OTHER`], in that list order.
    fn shared_list(policy: LoadBalancingAlgorithms, a: Backend, b: Backend) -> BackendList {
        let mut list = BackendList::with_seed(1839);
        list.add_backend(a);
        list.add_backend(b);
        list.add_backend(weighted("c", OTHER, None));
        list.set_load_balancing_policy(policy, Some(LoadMetric::Connections), 1839);
        list
    }

    fn equal_list(policy: LoadBalancingAlgorithms) -> BackendList {
        shared_list(
            policy,
            weighted("a", SHARED, None),
            weighted("b", SHARED, None),
        )
    }

    /// Run [`PICKS`] selections and return the picked backend ids. A
    /// load-aware policy sees each pick as one more open connection, and an
    /// affinity policy sees a fresh, well-spread client key per pick.
    fn picks(list: &mut BackendList) -> Vec<String> {
        let now = Instant::now();
        (0..PICKS)
            .map(|index| {
                let key = affinity_key_from_value(&index.to_le_bytes());
                let picked = list
                    .next_available_backend_with_key(Some(key), now)
                    .expect("three healthy backends");
                let mut picked = picked.borrow_mut();
                picked.active_connections += 1;
                picked.backend_id.to_owned()
            })
            .collect()
    }

    fn share_of(ids: &[String], wanted: &[&str]) -> f64 {
        ids.iter()
            .filter(|id| wanted.contains(&id.as_str()))
            .count() as f64
            / ids.len() as f64
    }

    /// The decision of 2026-10-04: one address, one share. Two ids at one
    /// address and one id at another, equal weights: each address takes
    /// half the selections, as HRW already gave it.
    ///
    /// TO SEE THIS RED: remove the `collapse_shared_addresses` calls from
    /// `BackendList::select_tiers`; round robin, random, least loaded and
    /// power of two then give the shared address two thirds.
    #[test]
    fn a_shared_address_takes_the_share_of_one_backend_under_every_policy() {
        let skewed: Vec<String> = POLICIES
            .into_iter()
            .filter_map(|policy| {
                let mut list = equal_list(policy);
                let shared = share_of(&picks(&mut list), &["a", "b"]);
                (!(0.47..=0.53).contains(&shared)).then(|| format!("{policy:?}: {shared:.3}"))
            })
            .collect();
        assert!(
            skewed.is_empty(),
            "the shared address did not take 1/2 of the selections under {skewed:?}"
        );
    }

    /// The shared address is served by its first id in list order when the
    /// weights are equal: the second id is never picked while the first can
    /// take a connection.
    #[test]
    fn the_first_id_at_a_shared_address_represents_it() {
        for policy in POLICIES {
            let mut list = equal_list(policy);
            let ids = picks(&mut list);
            assert!(
                ids.iter().all(|id| id != "b"),
                "{policy:?} picked the second id of the shared address"
            );
            assert!(ids.iter().any(|id| id == "a"), "{policy:?} never picked a");
        }
    }

    /// A down id never serves its address: its eligible sibling does, and
    /// the address keeps the share of one backend.
    #[test]
    fn a_down_id_yields_its_address_to_an_eligible_sibling() {
        for policy in POLICIES {
            let mut down = weighted("a", SHARED, None);
            down.health.record_failure(1);
            assert!(!down.health.is_healthy());
            let mut list = shared_list(policy, down, weighted("b", SHARED, None));
            let ids = picks(&mut list);
            assert!(
                ids.iter().all(|id| id != "a"),
                "{policy:?} picked the unhealthy id"
            );
            let shared = share_of(&ids, &["b"]);
            assert!(
                (0.47..=0.53).contains(&shared),
                "{policy:?} gave the shared address {shared:.3} of the selections, not 1/2"
            );
        }
    }

    /// Unequal weights at one address: the heaviest id represents it, as the
    /// HRW score — monotonic in weight for one address — has always decided.
    #[test]
    fn the_heaviest_id_at_a_shared_address_represents_it() {
        for policy in POLICIES {
            let mut list = shared_list(
                policy,
                weighted("a", SHARED, Some(50)),
                weighted("b", SHARED, Some(200)),
            );
            let ids = picks(&mut list);
            assert!(
                ids.iter().all(|id| id != "a"),
                "{policy:?} picked the lighter id of the shared address"
            );
        }
    }

    /// A shard counts addresses, not ids: eight addresses, one carrying two
    /// ids, at 25% make shards of two addresses, and a shard that holds the
    /// shared address holds both its ids, so the sibling of a down
    /// representative still serves inside the shard.
    #[test]
    fn a_shard_counts_a_shared_address_once_and_holds_all_its_ids() {
        let mut list = BackendList::with_seed(524);
        list.set_load_balancing_policy(LoadBalancingAlgorithms::Hrw, None, 524);
        for index in 0..8u16 {
            list.add_backend(Backend::new(
                &format!("shard-{index}"),
                SocketAddr::from(([127, 0, 0, 1], 20_000 + index)),
                None,
                None,
                None,
            ));
        }
        let shared = SocketAddr::from(([127, 0, 0, 1], 20_000));
        list.add_backend(Backend::new("shard-0-bis", shared, None, None, None));
        list.set_shuffle_sharding(Some(ShuffleSharding {
            percent: 25,
            min_backends: 4,
            mode: ShardMode::Fallback,
        }));

        let mut holding_shared = 0;
        for key in 0..256u64 {
            assert!(list.compute_shard(Some(key)), "the list must shard");
            let mut addresses: Vec<SocketAddr> = list
                .shard
                .iter()
                .map(|&index| list.backends[index].borrow().address)
                .collect();
            let ids = addresses.len();
            addresses.sort_unstable();
            addresses.dedup();
            assert_eq!(addresses.len(), 2, "key {key}: a shard holds two addresses");
            if addresses.contains(&shared) {
                holding_shared += 1;
                assert_eq!(ids, 3, "key {key}: the shared address brings both ids");
            } else {
                assert_eq!(ids, 2, "key {key}: one id per unshared address");
            }
        }
        assert!(
            holding_shared > 0,
            "some shard must hold the shared address"
        );
    }

    /// `LEAST_LOADED` and `POWER_OF_TWO` read an address's load as the sum
    /// over its ids: the representative `a` is idle, but its sibling `b`
    /// carries 5 connections, so the address weighs 5 against `c`'s 3 and
    /// `c` is picked.
    #[test]
    fn a_load_aware_policy_reads_the_load_of_the_whole_address() {
        let now = Instant::now();
        for policy in [
            LoadBalancingAlgorithms::LeastLoaded,
            LoadBalancingAlgorithms::PowerOfTwo,
        ] {
            let mut list = equal_list(policy);
            list.backends[1].borrow_mut().active_connections = 5;
            list.backends[2].borrow_mut().active_connections = 3;
            for key in 0..64 {
                let picked = list
                    .next_available_backend_with_key(Some(key), now)
                    .expect("three healthy backends");
                assert_eq!(
                    picked.borrow().backend_id,
                    "c",
                    "{policy:?} compared the representative's own load, not its address's"
                );
            }
        }
    }

    /// The cost guard: one shared pair among 1000 backends costs a selection
    /// a few units of shared-address work (`SIBLING_STEPS`: one per
    /// representative comparison or sibling-ring step), not one per
    /// candidate. A scan of the candidates per candidate costs a million.
    ///
    /// TO SEE THIS RED: replace the ring walk in
    /// `BackendList::collapse_shared_addresses` with
    /// `candidates.iter().any(|&other| outranks_at_address(..))`.
    #[test]
    fn one_shared_pair_costs_a_selection_constant_work() {
        use crate::load_balancing::SIBLING_STEPS;

        let now = Instant::now();
        // Built once: the debug-only invariant sweep after each insertion is
        // quadratic in the list, so a list per case would dominate the run.
        let mut list = BackendList::with_seed(1856);
        for index in 0..1000u16 {
            let address = SocketAddr::from(([10, (index >> 8) as u8, index as u8, 1], 8080));
            list.add_backend(Backend::new(
                &format!("b{index}"),
                address,
                None,
                None,
                None,
            ));
        }
        let shared = SocketAddr::from(([10, 0, 0, 1], 8080));
        list.add_backend(Backend::new("dup", shared, None, None, None));
        for sharded in [false, true] {
            list.set_shuffle_sharding(sharded.then_some(ShuffleSharding {
                percent: 25,
                min_backends: 8,
                mode: ShardMode::Fallback,
            }));
            for policy in POLICIES {
                list.set_load_balancing_policy(policy, Some(LoadMetric::Connections), 1856);
                let mut worst = 0;
                for key in 0..64u64 {
                    SIBLING_STEPS.with(|steps| steps.set(0));
                    assert!(list.select_with_key(Some(key), now).0.is_some());
                    worst = worst.max(SIBLING_STEPS.with(|steps| steps.get()));
                }
                assert!(
                    worst <= 8,
                    "{policy:?} (sharded: {sharded}) spent {worst} steps on one shared pair"
                );
            }
        }
    }

    /// `backends` is public: a library user may push or pop a backend
    /// without `add_backend`/`remove_backend`, leaving the sibling state of
    /// another length. Selection must then neither panic nor index a stale
    /// ring, under every policy, sharded or not.
    #[test]
    fn a_list_edited_directly_still_selects_without_panicking() {
        let now = Instant::now();
        let sharding = Some(ShuffleSharding {
            percent: 50,
            min_backends: 2,
            mode: ShardMode::Fallback,
        });
        for policy in POLICIES {
            for sharded in [false, true] {
                // Grown: a pushed sibling of the shared address, and a
                // pushed backend at a new address.
                let mut grown = equal_list(policy);
                grown.set_shuffle_sharding(sharded.then_some(sharding).flatten());
                for (id, port) in [("pushed-sibling", 9300), ("pushed", 9302)] {
                    let address = SocketAddr::from(([127, 0, 0, 1], port));
                    grown
                        .backends
                        .push(Rc::new(RefCell::new(weighted(id, address, None))));
                }
                // Shrunk: the last backend popped, the ring still covers it.
                let mut shrunk = equal_list(policy);
                shrunk.set_shuffle_sharding(sharded.then_some(sharding).flatten());
                shrunk.backends.pop();
                for list in [&mut grown, &mut shrunk] {
                    for key in 0..64 {
                        assert!(
                            list.select_with_key(Some(key), now).0.is_some(),
                            "{policy:?} (sharded: {sharded}) selected nothing"
                        );
                    }
                }
            }
        }
    }

    /// Collapsing allocates nothing: the scratch buffer is reserved on the
    /// control plane like the candidate buffer.
    #[test]
    fn a_selection_over_a_shared_address_allocates_nothing() {
        use std::hint::black_box;

        use crate::test_allocations::allocations;

        let now = Instant::now();
        for policy in POLICIES {
            let mut list = equal_list(policy);
            drop(list.select_with_key(Some(0), now));
            let mut allocated = 0;
            for key in 0..256u64 {
                let before = allocations();
                let picked = black_box(&mut list).select_with_key(black_box(Some(key)), now);
                allocated += allocations() - before;
                assert!(picked.0.is_some());
            }
            assert_eq!(
                allocated, 0,
                "{policy:?}: 256 selections made {allocated} allocations"
            );
        }
    }
}
