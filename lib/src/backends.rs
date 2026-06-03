use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    net::SocketAddr,
    rc::Rc,
    time::Duration,
};

use mio::net::TcpStream;
use sozu_command::{
    proto::command::{
        Event, EventKind, HealthCheckConfig, LoadBalancingAlgorithms, LoadBalancingParams,
        LoadMetric,
    },
    state::ClusterId,
};

use crate::metrics::names;
use crate::{
    PeakEWMA,
    load_balancing::{
        LeastLoaded, LoadBalancingAlgorithm, Maglev, PowerOfTwo, Random, Rendezvous, RoundRobin,
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
        self.consecutive_failures = 0;
        self.consecutive_successes += 1;

        if self.status == HealthStatus::Unhealthy && self.consecutive_successes >= healthy_threshold
        {
            self.status = HealthStatus::Healthy;
            return true;
        }
        false
    }

    /// Record a failed health check. Returns true if the backend transitioned to unhealthy.
    pub fn record_failure(&mut self, unhealthy_threshold: u32) -> bool {
        self.consecutive_successes = 0;
        self.consecutive_failures += 1;

        if self.status == HealthStatus::Healthy && self.consecutive_failures >= unhealthy_threshold
        {
            self.status = HealthStatus::Unhealthy;
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
    pub fn new(
        backend_id: &str,
        address: SocketAddr,
        sticky_id: Option<String>,
        load_balancing_parameters: Option<LoadBalancingParams>,
        backup: Option<bool>,
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
            connection_time: PeakEWMA::new(),
            health: HealthState::default(),
        }
    }

    pub fn set_closing(&mut self) {
        self.status = BackendStatus::Closing;
    }

    pub fn retry_policy(&mut self) -> &mut retry::RetryPolicyWrapper {
        &mut self.retry_policy
    }

    pub fn can_open(&self) -> bool {
        if !self.health.is_healthy() {
            return false;
        }
        if let Some(action) = self.retry_policy.can_try() {
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
        if self.status == BackendStatus::Normal {
            self.active_connections += 1;
            Some(self.active_connections)
        } else {
            None
        }
    }

    /// TODO: normalize with saturating_sub()
    pub fn dec_connections(&mut self) -> Option<usize> {
        match self.status {
            BackendStatus::Normal => {
                if self.active_connections > 0 {
                    self.active_connections -= 1;
                }
                Some(self.active_connections)
            }
            BackendStatus::Closed => None,
            BackendStatus::Closing => {
                if self.active_connections > 0 {
                    self.active_connections -= 1;
                }
                if self.active_connections == 0 {
                    self.status = BackendStatus::Closed;
                    None
                } else {
                    Some(self.active_connections)
                }
            }
        }
    }

    pub fn set_connection_time(&mut self, dur: Duration) {
        self.connection_time.observe(dur.as_nanos() as f64);
    }

    pub fn peak_ewma_connection(&mut self) -> f64 {
        self.connection_time.get(self.active_connections)
    }

    pub fn try_connect(&mut self) -> Result<mio::net::TcpStream, BackendError> {
        if self.status != BackendStatus::Normal {
            return Err(BackendError::Status(self.status.to_owned()));
        }

        match mio::net::TcpStream::connect(self.address) {
            Ok(tcp_stream) => {
                //self.retry_policy.succeed();
                self.inc_connections();
                Ok(tcp_stream)
            }
            Err(io_error) => {
                self.retry_policy.fail();
                self.failures += 1;
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
    pub max_failures: usize,
    pub health_check_configs: HashMap<ClusterId, HealthCheckConfig>,
    /// Whether the cluster's backends speak HTTP/2 (cluster.http2 = true).
    /// Mirrors the same backend-capability hint the mux router reads at
    /// `protocol/mux/router.rs::Router::connect`. The health checker uses
    /// it to switch the probe wire format from HTTP/1.1 to h2c so an
    /// h2c-only backend is not probed with an HTTP/1.1 preface that
    /// would always fail.
    pub cluster_http2: HashMap<ClusterId, bool>,
}

impl Default for BackendMap {
    fn default() -> Self {
        Self::new()
    }
}

impl BackendMap {
    pub fn new() -> BackendMap {
        BackendMap {
            backends: HashMap::new(),
            max_failures: 3,
            health_check_configs: HashMap::new(),
            cluster_http2: HashMap::new(),
        }
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

        let prev = list.availability.replace(new_state);
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

    /// Record (or clear) the `cluster.http2` backend-capability hint for
    /// `cluster_id`. The health checker reads the resulting map at probe
    /// time so the wire format follows what the mux router will use to
    /// connect to the same backends.
    pub fn set_cluster_http2(&mut self, cluster_id: &str, http2: bool) {
        if http2 {
            self.cluster_http2.insert(cluster_id.to_owned(), true);
        } else {
            self.cluster_http2.remove(cluster_id);
        }
    }

    pub fn set_health_check_config(&mut self, cluster_id: &str, config: Option<HealthCheckConfig>) {
        match config {
            Some(c) => {
                self.health_check_configs.insert(cluster_id.to_owned(), c);
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
        self.backends
            .extend(backends.iter().map(|(cluster_id, backend_vec)| {
                (
                    cluster_id.to_string(),
                    BackendList::import_configuration_state(backend_vec),
                )
            }));
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
        self.backends
            .entry(cluster_id.to_string())
            .or_default()
            .add_backend(backend);
        // Publish initial gauges and surface the corner case where a fresh
        // cluster's first backend is already down (e.g. registered with a
        // pre-existing failed retry policy). For an `Available` initial
        // backend this is just a (1, 1) gauge emission with no transition.
        self.record_cluster_availability(cluster_id);
    }

    // TODO: return <Result, BackendError>, log the error downstream
    /// Remove every backend at `backend_address` from `cluster_id` and
    /// return the list of `backend_id`s that were dropped. Callers (e.g.
    /// `Server::remove_backend`) iterate over the returned ids to tear
    /// down per-backend metrics so the identity used by the runtime
    /// (address-keyed) matches the identity used by the metrics layer
    /// (id-keyed) — see PR #1252 follow-up review MEDIUM-3.
    pub fn remove_backend(
        &mut self,
        cluster_id: &str,
        backend_address: &SocketAddr,
    ) -> Vec<String> {
        let removed = if let Some(backends) = self.backends.get_mut(cluster_id) {
            backends.remove_backend(backend_address)
        } else {
            error!(
                "Backend was already removed: cluster id {}, address {:?}",
                cluster_id, backend_address
            );
            return Vec::new();
        };
        // Re-evaluate so removing the last backend logs an explicit
        // `AllDown` transition (or, with `total == 0`, drops back to
        // silent gauges).
        self.record_cluster_availability(cluster_id);
        removed
    }

    // TODO: return <Result, BackendError>, log the error downstream
    pub fn close_backend_connection(&mut self, cluster_id: &str, addr: &SocketAddr) {
        if let Some(cluster_backends) = self.backends.get_mut(cluster_id) {
            if let Some(ref mut backend) = cluster_backends.find_backend(addr) {
                backend.borrow_mut().dec_connections();
            }
        }
    }

    pub fn has_backend(&self, cluster_id: &str, backend: &Backend) -> bool {
        self.backends
            .get(cluster_id)
            .map(|backends| backends.has_backend(&backend.address))
            .unwrap_or(false)
    }

    pub fn backend_from_cluster_id(
        &mut self,
        cluster_id: &str,
    ) -> Result<(Rc<RefCell<Backend>>, TcpStream), BackendError> {
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

        let next_backend = match cluster_backends.next_available_backend() {
            Some(nb) => nb,
            None => {
                // Drop the &mut BackendList before the &self helper call.
                // The helper observes (available=0, total>0) and emits the
                // Available -> AllDown transition (log + counter + Event)
                // exactly once per regime entry. Subsequent calls in the
                // same AllDown regime are no-ops.
                let _ = cluster_backends;
                self.record_cluster_availability(cluster_id);
                return Err(BackendError::NoBackendForCluster(cluster_id.to_owned()));
            }
        };

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

            borrowed_backend.try_connect().map_err(|backend_error| {
                BackendError::ConnectionFailures {
                    cluster_id: cluster_id.to_owned(),
                    backend_address: borrowed_backend.address,
                    failures: borrowed_backend.failures,
                    error: backend_error.to_string(),
                }
            })?
        };

        // Connection succeeded: re-evaluate so we capture an
        // AllDown -> Available recovery transition the moment a request
        // first hits a healthy backend after an outage. `next_backend` is
        // not borrowed here (the inner block dropped `borrowed_backend`),
        // so the helper's `BackendList::evaluate_availability` walk is
        // free to call `borrow()` on every backend.
        let _ = cluster_backends;
        self.record_cluster_availability(cluster_id);

        Ok((next_backend.clone(), tcp_stream))
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

        let next_backend = match cluster_backends.next_available_backend_with_key(key) {
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
        Ok((backend_id, address))
    }

    pub fn backend_from_sticky_session(
        &mut self,
        cluster_id: &str,
        sticky_session: &str,
    ) -> Result<(Rc<RefCell<Backend>>, TcpStream), BackendError> {
        let sticky_conn = self
            .backends
            .get_mut(cluster_id)
            .and_then(|cluster_backends| cluster_backends.find_sticky(sticky_session))
            .map(|backend| {
                let mut borrowed = backend.borrow_mut();
                let conn = borrowed.try_connect();

                conn.map(|tcp_stream| (backend.clone(), tcp_stream))
                    .inspect_err(|_| {
                        error!(
                            "could not connect {} to {:?} using session {} ({} failures)",
                            cluster_id, borrowed.address, sticky_session, borrowed.failures
                        )
                    })
            });

        match sticky_conn {
            Some(backend_and_stream) => backend_and_stream,
            None => {
                debug!(
                    "Couldn't find a backend corresponding to sticky_session {} for cluster {}",
                    sticky_session, cluster_id
                );
                self.backend_from_cluster_id(cluster_id)
            }
        }
    }

    pub fn set_load_balancing_policy_for_cluster(
        &mut self,
        cluster_id: &str,
        lb_algo: LoadBalancingAlgorithms,
        metric: Option<LoadMetric>,
    ) {
        // The cluster can be created before the backends were registered because of the async config messages.
        // So when we set the load balancing policy, we have to create the backend list if if it doesn't exist yet.
        let cluster_backends = self.get_or_create_backend_list_for_cluster(cluster_id);
        cluster_backends.set_load_balancing_policy(lb_algo, metric);
    }

    pub fn get_or_create_backend_list_for_cluster(&mut self, cluster_id: &str) -> &mut BackendList {
        self.backends.entry(cluster_id.to_string()).or_default()
    }
}

#[derive(Debug)]
pub struct BackendList {
    pub backends: Vec<Rc<RefCell<Backend>>>,
    pub next_id: u32,
    pub load_balancing: Box<dyn LoadBalancingAlgorithm>,
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
}

impl Default for BackendList {
    fn default() -> Self {
        Self::new()
    }
}

impl BackendList {
    pub fn new() -> BackendList {
        BackendList {
            backends: Vec::new(),
            next_id: 0,
            load_balancing: Box::new(Random),
            fail_open_warned: false,
            availability: Cell::new(ClusterAvailability::Available),
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
        (available, total)
    }

    pub fn import_configuration_state(
        backend_vec: &[sozu_command_lib::response::Backend],
    ) -> BackendList {
        let mut list = BackendList::new();
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
        // Refresh table-based policies (Maglev) off the datapath whenever the
        // full backend set or a weight changes. This is the ONLY place the
        // table is rebuilt on mutation; selection never rebuilds. The default
        // `rebuild` is a no-op for the stateless policies.
        self.load_balancing.rebuild(&self.backends);
    }

    /// Remove every backend at `backend_address` and return the list of
    /// `backend_id`s that were dropped. Two backends with the same address
    /// but distinct ids (A/B test, weighted variant, dedup race) are both
    /// removed here; the caller relies on the returned ids to tear down
    /// matching per-backend state (metrics, health-check). Returning the
    /// ids closes the identity drift between runtime-removal-by-address
    /// and metrics-removal-by-id.
    pub fn remove_backend(&mut self, backend_address: &SocketAddr) -> Vec<String> {
        let mut removed = Vec::new();
        self.backends.retain(|backend| {
            let b = backend.borrow();
            if &b.address == backend_address {
                removed.push(b.backend_id.clone());
                false
            } else {
                true
            }
        });
        // Rebuild table-based policies (Maglev) off the datapath after the set
        // shrinks, only when something was actually removed. No-op for the
        // stateless policies.
        if !removed.is_empty() {
            self.load_balancing.rebuild(&self.backends);
        }
        removed
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

    pub fn find_sticky(&mut self, sticky_session: &str) -> Option<&mut Rc<RefCell<Backend>>> {
        self.backends
            .iter_mut()
            .find(|b| b.borrow().sticky_id.as_deref() == Some(sticky_session))
            .and_then(|b| if b.borrow().can_open() { Some(b) } else { None })
    }

    pub fn available_backends(&mut self, backup: bool) -> Vec<Rc<RefCell<Backend>>> {
        self.backends
            .iter()
            .filter(|backend| {
                let owned = backend.borrow();
                owned.backup == backup && owned.can_open()
            })
            .map(Clone::clone)
            .collect()
    }

    pub fn next_available_backend(&mut self) -> Option<Rc<RefCell<Backend>>> {
        self.next_available_backend_with_key(None)
    }

    /// Pick the next available backend, optionally pinned by an affinity `key`.
    ///
    /// `key` is only consulted by consistent-hashing policies (HRW/Maglev);
    /// every other policy ignores it, so `next_available_backend_with_key(None)`
    /// is byte-for-byte the legacy behavior. The UDP datapath calls this with
    /// `Some(flow_hash)` to keep a client flow pinned to one backend.
    pub fn next_available_backend_with_key(
        &mut self,
        key: Option<u64>,
    ) -> Option<Rc<RefCell<Backend>>> {
        let mut backends = self.available_backends(false);

        if backends.is_empty() {
            backends = self.available_backends(true);
        }

        if !backends.is_empty() {
            // Healthy regime: log the fail-open exit transition exactly once.
            if self.fail_open_warned {
                info!(
                    "fail-open: cluster recovered, {} backends now healthy",
                    backends.len()
                );
                self.fail_open_warned = false;
            }
            return self
                .load_balancing
                .next_available_backend(key, &mut backends);
        }

        // Fail-open: when no backend passes the full `can_open()` gate,
        // route to backends that are administratively `Normal` AND whose
        // retry policy reports `OKAY` (i.e., not currently in
        // exponential-backoff). This prevents a shared dependency outage
        // (e.g., database) from making the entire cluster unavailable while
        // still respecting the per-backend back-off window — hammering a
        // backend at line rate during its back-off would defeat the back-off
        // itself. Ref: Amazon "Implementing Health Checks".
        backends = self
            .backends
            .iter()
            .filter(|b| {
                let owned = b.borrow();
                owned.status == BackendStatus::Normal
                    && matches!(owned.retry_policy.can_try(), Some(retry::RetryAction::OKAY))
            })
            .map(Clone::clone)
            .collect();

        if backends.is_empty() {
            return None;
        }

        // Latched warning + per-decision counter: the warn! fires once on
        // regime entry; the counter is the operator-visible per-request
        // signal that does not drown logs under universal outage.
        if !self.fail_open_warned {
            warn!(
                "fail-open: all backends unhealthy, routing to {} normal backends with retry-policy OKAY",
                backends.len()
            );
            self.fail_open_warned = true;
        }
        count!(names::backend::FAIL_OPEN, 1);

        self.load_balancing
            .next_available_backend(key, &mut backends)
    }

    pub fn set_load_balancing_policy(
        &mut self,
        load_balancing_policy: LoadBalancingAlgorithms,
        metric: Option<LoadMetric>,
    ) {
        match load_balancing_policy {
            LoadBalancingAlgorithms::RoundRobin => {
                self.load_balancing = Box::new(RoundRobin::new())
            }
            LoadBalancingAlgorithms::Random => self.load_balancing = Box::new(Random {}),
            LoadBalancingAlgorithms::LeastLoaded => {
                self.load_balancing = Box::new(LeastLoaded {
                    metric: metric.unwrap_or(LoadMetric::Connections),
                })
            }
            LoadBalancingAlgorithms::PowerOfTwo => {
                self.load_balancing = Box::new(PowerOfTwo {
                    metric: metric.unwrap_or(LoadMetric::Connections),
                })
            }
            // Affinity policies (used by the UDP datapath). They consult the
            // optional hash key; with `None` they fall back to round-robin.
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

#[cfg(test)]
mod backends_test {

    use std::{net::TcpListener, sync::mpsc::*, thread};

    use super::*;

    fn run_mock_tcp_server(addr: &str, stopper: Receiver<()>) {
        let mut run = true;
        let listener = TcpListener::bind(addr).unwrap();

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
    }

    #[test]
    fn it_should_retrieve_a_backend_from_cluster_id_when_backends_have_been_recorded() {
        let mut backend_map = BackendMap::new();
        let cluster_id = "mycluster";

        let backend_addr = "127.0.0.1:1236";
        let (sender, receiver) = channel();
        run_mock_tcp_server(backend_addr, receiver);

        backend_map.add_backend(
            cluster_id,
            Backend::new(
                &format!("{cluster_id}-1"),
                backend_addr.parse().unwrap(),
                None,
                None,
                None,
            ),
        );

        assert!(backend_map.backend_from_cluster_id(cluster_id).is_ok());
        sender.send(()).unwrap();
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
                .backend_from_cluster_id(cluster_not_recorded)
                .is_err()
        );
    }

    #[test]
    fn it_should_not_retrieve_a_backend_from_cluster_id_when_backend_list_is_empty() {
        let mut backend_map = BackendMap::new();

        assert!(backend_map.backend_from_cluster_id("dumb").is_err());
    }

    #[test]
    fn it_should_retrieve_a_backend_from_sticky_session_when_the_backend_has_been_recorded() {
        let mut backend_map = BackendMap::new();
        let cluster_id = "mycluster";
        let sticky_session = "server-2";

        let backend_addr = "127.0.0.1:3456";
        let (sender, receiver) = channel();
        run_mock_tcp_server(backend_addr, receiver);

        backend_map.add_backend(
            cluster_id,
            Backend::new(
                &format!("{cluster_id}-1"),
                "127.0.0.1:9001".parse().unwrap(),
                Some("server-1".to_string()),
                None,
                None,
            ),
        );
        backend_map.add_backend(
            cluster_id,
            Backend::new(
                &format!("{cluster_id}-2"),
                "127.0.0.1:9000".parse().unwrap(),
                Some("server-2".to_string()),
                None,
                None,
            ),
        );
        // sticky backend
        backend_map.add_backend(
            cluster_id,
            Backend::new(
                &format!("{cluster_id}-3"),
                backend_addr.parse().unwrap(),
                Some("server-3".to_string()),
                None,
                None,
            ),
        );

        assert!(
            backend_map
                .backend_from_sticky_session(cluster_id, sticky_session)
                .is_ok()
        );
        sender.send(()).unwrap();
    }

    #[test]
    fn it_should_not_retrieve_a_backend_from_sticky_session_when_the_backend_has_not_been_recorded()
    {
        let mut backend_map = BackendMap::new();
        let cluster_id = "mycluster";
        let sticky_session = "test";

        assert!(
            backend_map
                .backend_from_sticky_session(cluster_id, sticky_session)
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
                .backend_from_sticky_session(mycluster_not_recorded, sticky_session)
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
        assert!(list.available_backends(false).is_empty());
        assert!(list.available_backends(true).is_empty());

        let picked = list.next_available_backend();
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
            backend_rc.borrow_mut().retry_policy().fail();
            assert_eq!(
                Some(retry::RetryAction::WAIT),
                backend_rc.borrow().retry_policy.can_try(),
                "test fixture must place retry policy in WAIT"
            );
        }

        let picked = list.next_available_backend();
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

        assert!(list.next_available_backend().is_some());
        assert!(list.fail_open_warned, "first fail-open must latch");

        assert!(list.next_available_backend().is_some());
        assert!(
            list.fail_open_warned,
            "subsequent fail-open routing keeps the latch"
        );

        // Heal one backend — the next routing call takes the healthy path
        // and must clear the latch (regime exit logged once).
        list.backends[0].borrow_mut().health.status = HealthStatus::Healthy;
        let picked = list.next_available_backend();
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
        backend.retry_policy.succeed();
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
        map.backends
            .insert(cluster_id.to_owned(), BackendList::new());
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
            cluster_id.to_owned(),
            vec![sozu_command_lib::response::Backend {
                cluster_id: cluster_id.to_owned(),
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
            other => panic!("expected WorkerMetrics, got {other:?}"),
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
}
