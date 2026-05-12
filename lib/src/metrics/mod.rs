//! Metrics façade.
//!
//! Defines the per-thread `Aggregator`, the `incr!`/`count!`/`gauge!`/
//! `gauge_add!`/`time!` macros consumed across the lib + bin crates, the
//! local-vs-network drain dispatch, and the label allow/deny filtering
//! used to keep cardinality bounded. Gauge underflow is treated as a
//! correctness bug (saturating clamp + warn log), not a rounding artefact.

mod local_drain;
pub mod names;
mod network_drain;
mod writer;

use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    io::{self, Write},
    net::SocketAddr,
    str,
    time::{Duration, Instant},
};

use mio::net::UdpSocket;
use sozu_command::config::MetricDetailLevel;
use sozu_command::proto::command::{
    FilteredMetrics, MetricsConfiguration, QueryMetricsOptions, ResponseContent,
};

use crate::metrics::{local_drain::LocalDrain, network_drain::NetworkDrain};

/// Filter `(cluster_id, backend_id)` labels at emission time according to the
/// configured [`MetricDetailLevel`]. Each level is a SUPERSET of the previous
/// in cardinality terms:
///
/// - `Process`: drop both labels — proxy-only counters (smallest keyspace).
/// - `Frontend`: same as `Process` today; reserved for the per-listener
///   counters tracked as a follow-up. Listed in the proto + config so
///   operators can opt in once per-listener labels land.
/// - `Cluster`: keep `cluster_id`, drop `backend_id` — current default.
/// - `Backend`: keep both — current historical behaviour.
///
/// Pure function so the filter can be unit-tested exhaustively without the
/// drain machinery in scope.
fn filter_labels_for_detail<'a>(
    detail: MetricDetailLevel,
    cluster_id: Option<&'a str>,
    backend_id: Option<&'a str>,
) -> (Option<&'a str>, Option<&'a str>) {
    match detail {
        MetricDetailLevel::Process | MetricDetailLevel::Frontend => (None, None),
        MetricDetailLevel::Cluster => (cluster_id, None),
        MetricDetailLevel::Backend => (cluster_id, backend_id),
    }
}

/// Map a numeric HTTP status code to its dedicated counter name, if any.
///
/// Returns `Some("http.status.<code>")` for the eighteen status codes Sōzu
/// either generates as a default answer or that operators routinely chart
/// (`200/201/204`, `301/302/304`, `400/401/403/404/408/413/429`, plus the
/// `5xx` family Sōzu can synthesise — `500/502/503/504/507`). All other
/// codes return `None` so the bucket counter (`http.status.{1xx,…,other}`)
/// remains the sole emission and metric cardinality stays bounded.
///
/// Hoisted out of the protocol modules so H1 (`kawa_h1::save_http_status_metric`)
/// and H2 (`mux::stream::generate_access_log`) cannot drift on which codes
/// get a per-code counter.
pub(crate) fn http_status_code_metric_name(status: u16) -> Option<&'static str> {
    match status {
        200 => Some("http.status.200"),
        201 => Some("http.status.201"),
        204 => Some("http.status.204"),
        301 => Some("http.status.301"),
        302 => Some("http.status.302"),
        304 => Some("http.status.304"),
        400 => Some("http.status.400"),
        401 => Some("http.status.401"),
        403 => Some("http.status.403"),
        404 => Some("http.status.404"),
        408 => Some("http.status.408"),
        413 => Some("http.status.413"),
        429 => Some("http.status.429"),
        500 => Some("http.status.500"),
        502 => Some("http.status.502"),
        503 => Some("http.status.503"),
        504 => Some("http.status.504"),
        507 => Some("http.status.507"),
        _ => None,
    }
}

thread_local! {
  pub static METRICS: RefCell<Aggregator> = RefCell::new(Aggregator::new(String::from("sozu")));
}

#[derive(thiserror::Error, Debug)]
pub enum MetricError {
    #[error("Could not parse udp address {address}: {error}")]
    WrongUdpAddress { address: String, error: String },
    #[error("Could not bind to udp address {address}: {error}")]
    UdpBind { address: String, error: String },
    #[error("No metrics found for object with id {0}")]
    NoMetrics(String),
    #[error("Could not create histogram for time metric {time_metric:?}: {error}")]
    HistogramCreation {
        time_metric: MetricValue,
        error: String,
    },
    #[error("could not record time metric {time_metric:?}: {error}")]
    TimeMetricRecordingError {
        time_metric: MetricValue,
        error: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetricValue {
    Gauge(usize),
    GaugeAdd(i64),
    Count(i64),
    Time(usize),
}

impl MetricValue {
    fn is_time(&self) -> bool {
        matches!(self, &MetricValue::Time(_))
    }

    fn update(&mut self, key: &'static str, m: MetricValue) -> bool {
        match (self, m) {
            (&mut MetricValue::Gauge(ref mut v1), MetricValue::Gauge(v2)) => {
                let changed = *v1 != v2;
                *v1 = v2;
                changed
            }
            (&mut MetricValue::Gauge(ref mut v1), MetricValue::GaugeAdd(v2)) => {
                debug_assert!(
                    *v1 as i64 + v2 >= 0,
                    "metric {key} underflow: previous value: {v1}, adding: {v2}"
                );
                let changed = v2 != 0;
                let res = *v1 as i64 + v2;
                *v1 = if res >= 0 {
                    res as usize
                } else {
                    error!(
                        "metric {} underflow: previous value: {}, adding: {}",
                        key, v1, v2
                    );
                    0
                };

                changed
            }
            (&mut MetricValue::Count(ref mut v1), MetricValue::Count(v2)) => {
                let changed = v2 != 0;
                *v1 += v2;
                changed
            }
            (s, m) => panic!(
                "tried to update metric {key} of value {s:?} with an incompatible metric: {m:?}"
            ),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StoredMetricValue {
    last_sent: Instant,
    updated: bool,
    data: MetricValue,
}

impl StoredMetricValue {
    pub fn new(last_sent: Instant, data: MetricValue) -> StoredMetricValue {
        StoredMetricValue {
            last_sent,
            updated: true,
            data: if let MetricValue::GaugeAdd(v) = data {
                if v >= 0 {
                    MetricValue::Gauge(v as usize)
                } else {
                    MetricValue::Gauge(0)
                }
            } else {
                data
            },
        }
    }

    pub fn update(&mut self, key: &'static str, m: MetricValue) {
        let updated = self.data.update(key, m);
        if !self.updated {
            self.updated = updated;
        }
    }
}

pub fn setup<O: Into<String>>(
    metrics_host: &SocketAddr,
    origin: O,
    use_tagged_metrics: bool,
    prefix: Option<String>,
    detail: MetricDetailLevel,
) -> Result<(), MetricError> {
    let metrics_socket = udp_bind()?;

    debug!(
        "setting up metrics: local address = {:#?}",
        metrics_socket.local_addr()
    );

    METRICS.with(|metrics| {
        if let Some(p) = prefix {
            (*metrics.borrow_mut()).set_up_prefix(p);
        }
        (*metrics.borrow_mut()).set_up_remote(metrics_socket, *metrics_host);
        (*metrics.borrow_mut()).set_up_origin(origin.into());
        (*metrics.borrow_mut()).set_up_tagged_metrics(use_tagged_metrics);
        (*metrics.borrow_mut()).set_up_detail(detail);
    });
    Ok(())
}

pub trait Subscriber {
    fn receive_metric(
        &mut self,
        label: &'static str,
        cluster_id: Option<&str>,
        backend_id: Option<&str>,
        metric: MetricValue,
    );
}

/// How often `lease_tick` actually does work; cheaper than recomputing the
/// effective level on every metric emission. Polled at the top of the worker's
/// `notify` loop, so the cadence floats with traffic but is bounded by this.
const LEASE_TICK_INTERVAL: Duration = Duration::from_secs(5);

/// Hard cap on lease TTL, mirroring the proto comment on `SetMetricDetail`.
/// Bounds the worst-case effect of a stuck renewer.
pub const LEASE_TTL_MAX: Duration = Duration::from_secs(300);

/// Default lease TTL applied when the proto request omits `ttl_seconds` (or
/// passes `0`). Matches the proto comment.
pub const LEASE_TTL_DEFAULT: Duration = Duration::from_secs(60);

/// Hard cap on the number of simultaneous leases held by the aggregator.
/// `lease_apply` rejects new entries (with [`LeaseApplyOutcome::Capped`])
/// once the table reaches this size. Bounds the lease table's memory and
/// neutralises the CWE-770 vector where a same-UID attacker rolls
/// `client_id` faster than expiry to grow the map unbounded. 64 is well
/// above any plausible TUI fleet (one TUI per operator + a handful of
/// metric scrapers); legitimate callers renewing the same `client_id`
/// REPLACE rather than insert and therefore don't bump the count.
pub const LEASE_TABLE_CAP: usize = 64;

/// Hard cap on `client_id` length accepted by `lease_apply`. The TUI
/// uses `top:<pid>:<8 hex chars>` ≤ 24 bytes; 64 leaves headroom for
/// other operator-side scrapers while keeping the lease table's per-
/// entry memory bounded and the audit-log lease_id column small.
pub const LEASE_CLIENT_ID_MAX_BYTES: usize = 64;

/// Outcome of [`Aggregator::lease_apply`]. The success arm returns the
/// `(previous_effective, new_effective)` pair the caller can use to
/// decide whether to emit a `MetricDetailChanged` audit event; the
/// failure arms surface bounded-input rejections.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseApplyOutcome {
    /// Lease inserted / renewed.
    Applied {
        previous_effective: MetricDetailLevel,
        new_effective: MetricDetailLevel,
    },
    /// `client_id` length exceeds [`LEASE_CLIENT_ID_MAX_BYTES`].
    ClientIdTooLong,
    /// Lease table is at [`LEASE_TABLE_CAP`] and the request is a new
    /// insert (not a renewal). Callers MUST surface this as a `FAILURE`
    /// to the wire so the lessor can back off.
    TableFull,
    /// The requested TTL exceeds [`LEASE_TTL_MAX`]. This arm is
    /// theoretically unreachable in the normal flow because the
    /// dispatch site rejects out-of-range values before reaching the
    /// aggregator; surfacing it explicitly catches callers that bypass
    /// dispatch (proto fuzzing, future internal use) instead of
    /// silently clamping their intent.
    TtlOutOfRange,
    /// A renewal request was presented with a [`PeerBinding`] that
    /// disagrees with the existing lease's apply-time binding. Returned
    /// only when the existing binding is fully known (per
    /// [`PeerBinding::is_known`]); unknown-binding leases (no
    /// `SO_PEERCRED` available, pre-binding callers) continue to
    /// accept any renewer. Closes the same-UID `client_id`-collision
    /// takeover where an attacker re-applies a lease against a
    /// victim's id and replaces the binding to lock the victim out of
    /// their own clear.
    Unauthorized,
}

/// Master-populated peer binding stored alongside each lease so subsequent
/// `clear` requests can be authorised against the apply-time owner. The
/// binding pairs an OS pid (from `SO_PEERCRED` on Linux, captured by the
/// master at command-socket accept time) with the master-side connection
/// session ULID. A clear request must present BOTH values matching the
/// apply-time binding. A binding of `(None, None)` ("unknown") at apply
/// time means the connection had no peer credentials available — the
/// worker then accepts any clear for that `client_id` to preserve compat
/// with non-Linux callers and intermediate proxies. See the proto comment
/// on `SetMetricDetail.peer_pid` for the trust model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PeerBinding {
    pub pid: Option<i32>,
    /// Session ULID rendered as a `u128` (the master's session anchor).
    /// Stored as the raw u128 rather than the original `Ulid` to avoid
    /// dragging that crate into the metrics-aggregator dependencies.
    pub session_ulid: Option<u128>,
}

impl PeerBinding {
    /// `true` when both halves are known. A fully-known binding is the
    /// only one against which `clear` may be authorised; partial bindings
    /// (one half `None`) degrade to "accept any clear" per the proto
    /// contract on `SetMetricDetail.peer_pid` / `peer_session_ulid`.
    pub fn is_known(&self) -> bool {
        self.pid.is_some() && self.session_ulid.is_some()
    }

    /// True when `self` and `other` are compatible (same `pid` + same
    /// `session_ulid`) AND `self.is_known()`. Used by `lease_clear` to
    /// reject mismatched clears.
    pub fn matches(&self, other: &PeerBinding) -> bool {
        self.is_known() && self.pid == other.pid && self.session_ulid == other.session_ulid
    }
}

/// One lease entry kept inside [`Aggregator::leases`]. Carries the
/// requested cardinality level, the absolute expiry instant, and the
/// master-supplied [`PeerBinding`] captured at apply time.
#[derive(Clone, Copy, Debug)]
struct LeaseEntry {
    level: MetricDetailLevel,
    expires_at: Instant,
    binding: PeerBinding,
}

/// Outcome of [`Aggregator::lease_clear`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseClearOutcome {
    /// The lease was found, the binding matched (or was unknown at apply
    /// time), and the entry has been removed. Carries the worker's
    /// previous effective level so the caller can decide whether to
    /// emit a `MetricDetailChanged` audit event.
    Cleared {
        previous_effective: MetricDetailLevel,
    },
    /// No lease was held by the requested `client_id`. Silent no-op.
    NotFound,
    /// A lease existed but the peer binding presented with the clear did
    /// not match the apply-time binding. The lease is left intact. The
    /// worker MUST surface this as a `FAILURE` response to discourage
    /// guessing attacks against another operator's lease.
    Unauthorized,
}

pub struct Aggregator {
    /// appended to metric keys, usually "sozu-"
    prefix: String,
    /// gathers metrics and sends them on a UDP socket
    network: Option<NetworkDrain>,
    /// gather metrics locally, queried by the CLI
    local: LocalDrain,
    /// Static cardinality knob — set once at boot from `MetricsConfig.detail`.
    /// Filters `(cluster_id, backend_id)` labels at emission time. Each level
    /// is a SUPERSET of the previous one (`Process ⊆ Frontend ⊆ Cluster ⊆ Backend`).
    configured: MetricDetailLevel,
    /// Effective cardinality knob actually applied at emission. Equal to
    /// `max(configured, max(active leases))`. Recomputed only when leases
    /// change, so the hot path (`receive_metric`) reads a single field.
    effective: MetricDetailLevel,
    /// Active TTL leases keyed by `client_id` from `SetMetricDetail`. A live
    /// lease holds the worker's effective level at-or-above its requested
    /// detail until it expires or is explicitly cleared. Multiple clients
    /// (e.g. several `sozu top` sessions) lease independently.
    leases: HashMap<String, LeaseEntry>,
    /// Wall-clock anchor for the polled lease janitor. Updated on every
    /// `lease_tick` call regardless of whether expiry happened, so the
    /// caller's "is it time to tick?" check stays cheap.
    last_lease_tick: Instant,
}

impl Aggregator {
    pub fn new(prefix: String) -> Aggregator {
        let default_detail = MetricDetailLevel::default();
        Aggregator {
            prefix: prefix.clone(),
            network: None,
            local: LocalDrain::new(prefix),
            configured: default_detail,
            effective: default_detail,
            leases: HashMap::new(),
            last_lease_tick: Instant::now(),
        }
    }

    pub fn set_up_prefix(&mut self, prefix: String) {
        self.prefix = prefix;
    }

    pub fn set_up_remote(&mut self, socket: UdpSocket, addr: SocketAddr) {
        self.network = Some(NetworkDrain::new(self.prefix.clone(), socket, addr));
    }

    pub fn set_up_origin(&mut self, origin: String) {
        if let Some(n) = self.network.as_mut() {
            n.origin = origin;
        }
    }

    pub fn set_up_tagged_metrics(&mut self, tagged: bool) {
        if let Some(n) = self.network.as_mut() {
            n.use_tagged_metrics = tagged;
        }
    }

    /// Set the static cardinality floor (`MetricsConfig.detail` from the TOML
    /// configuration). Live leases applied via [`Self::lease_apply`] can
    /// elevate the effective level at runtime; the configured floor is the
    /// lower bound the worker falls back to when no lease is active.
    ///
    /// See [`MetricDetailLevel`] and [`filter_labels_for_detail`] for the
    /// per-level filtering rules.
    pub fn set_up_detail(&mut self, detail: MetricDetailLevel) {
        self.configured = detail;
        self.recompute_effective();
    }

    /// Returns the static (configured) cardinality floor. Independent of
    /// runtime leases.
    pub fn detail_configured(&self) -> MetricDetailLevel {
        self.configured
    }

    /// Returns the cardinality level actually applied to emissions. Equal to
    /// `max(configured, max(active leases))`.
    pub fn detail_effective(&self) -> MetricDetailLevel {
        self.effective
    }

    /// Apply or renew a runtime cardinality lease for `client_id`. If a lease
    /// for the same client already exists it is REPLACED (used for renewals).
    /// `ttl` values above [`LEASE_TTL_MAX`] are **rejected** with
    /// [`LeaseApplyOutcome::TtlOutOfRange`] (NOT clamped); callers must
    /// handle that arm or pre-validate the TTL. On success the call returns
    /// `(previous_effective, new_effective)` so callers can decide whether
    /// to emit a `MetricDetailChanged` audit event.
    ///
    /// The proto contract on `SetMetricDetail.ttl_seconds` is that the worker
    /// **rejects** out-of-range values with a `FAILURE` response — that
    /// enforcement lives at the dispatch site in `lib/src/server.rs::notify`.
    /// `lease_apply` itself returns [`LeaseApplyOutcome::TtlOutOfRange`]
    /// when called with `ttl > LEASE_TTL_MAX` so callers that bypass the
    /// dispatch site (proto fuzzing, future internal use) see a loud
    /// rejection instead of silently capped semantics. Same shape for
    /// over-long `client_id` and a full lease table — see
    /// [`LeaseApplyOutcome`] for the failure arms.
    pub fn lease_apply(
        &mut self,
        client_id: String,
        level: MetricDetailLevel,
        ttl: Duration,
        binding: PeerBinding,
    ) -> LeaseApplyOutcome {
        if client_id.len() > LEASE_CLIENT_ID_MAX_BYTES {
            return LeaseApplyOutcome::ClientIdTooLong;
        }
        if ttl > LEASE_TTL_MAX {
            return LeaseApplyOutcome::TtlOutOfRange;
        }
        // Cap the table size BEFORE the insert, but only when the caller
        // is inserting a fresh entry. Renewals (same `client_id` already
        // present) REPLACE the existing entry and therefore keep the
        // count stable — they must always succeed so an active operator
        // never loses their lease just because the table is full.
        let is_renewal = self.leases.contains_key(&client_id);
        if !is_renewal && self.leases.len() >= LEASE_TABLE_CAP {
            return LeaseApplyOutcome::TableFull;
        }
        // Renewal-binding gate: when a lease already exists for this
        // `client_id` and its apply-time binding is fully known, the
        // renewer's presented binding MUST match. Without this check
        // any same-UID caller that learns the `client_id` (PID from
        // /proc, suffix from audit log) could re-apply against it,
        // overwriting the binding to point at the attacker's session
        // and then driving the victim's Drop-time `clear` into
        // `Unauthorized`. Unknown apply-time bindings continue to
        // accept any renewer per the proto contract on
        // `SetMetricDetail.peer_pid` / `peer_session_ulid`.
        if is_renewal
            && let Some(entry) = self.leases.get(&client_id)
            && entry.binding.is_known()
            && !entry.binding.matches(&binding)
        {
            return LeaseApplyOutcome::Unauthorized;
        }
        let expires_at = Instant::now() + ttl;
        self.leases.insert(
            client_id,
            LeaseEntry {
                level,
                expires_at,
                binding,
            },
        );
        let previous_effective = self.effective;
        self.recompute_effective();
        LeaseApplyOutcome::Applied {
            previous_effective,
            new_effective: self.effective,
        }
    }

    /// Explicitly release a lease keyed by `client_id`. The clear is
    /// authorised against the apply-time [`PeerBinding`] when one was
    /// recorded — see [`LeaseClearOutcome`] for the three result states.
    /// A clear request with `presented = PeerBinding::default()` matches
    /// only leases whose apply-time binding was also unknown, preserving
    /// compat with pre-binding callers and platforms without
    /// `SO_PEERCRED`.
    pub fn lease_clear(&mut self, client_id: &str, presented: PeerBinding) -> LeaseClearOutcome {
        let Some(entry) = self.leases.get(client_id) else {
            return LeaseClearOutcome::NotFound;
        };
        // If the apply-time binding is fully known we MUST authorise the
        // clear against it; presenting `default()` (an empty binding) is
        // a mismatch. If the apply-time binding is unknown (a pre-binding
        // caller, or a non-Linux peer with no credentials), we permit
        // any clear — there is nothing to authorise against.
        if entry.binding.is_known() && !entry.binding.matches(&presented) {
            return LeaseClearOutcome::Unauthorized;
        }
        self.leases.remove(client_id);
        let previous = self.effective;
        self.recompute_effective();
        LeaseClearOutcome::Cleared {
            previous_effective: previous,
        }
    }

    /// Returns the number of active (non-expired-as-of-last-tick) leases.
    /// Surfaced in `WorkerMetricDetailStatus` so the TUI can show
    /// "another client is still leasing this level".
    pub fn lease_count(&self) -> u32 {
        self.leases.len() as u32
    }

    /// Polled lease-expiry janitor. Called from the worker's `notify` loop
    /// (and from periodic timers); cheap when nothing has expired. Returns
    /// `Some(previous_effective)` when at least one lease expired AND that
    /// expiry actually moved the effective level (so the caller can emit a
    /// `MetricDetailChanged` audit event), or `None` for the no-change path.
    ///
    /// `now` is parameterised so unit tests can advance the clock
    /// deterministically without sleeping.
    pub fn lease_tick(&mut self, now: Instant) -> Option<MetricDetailLevel> {
        self.last_lease_tick = now;
        let before = self.leases.len();
        self.leases.retain(|_, entry| entry.expires_at > now);
        if self.leases.len() == before {
            return None;
        }
        let previous = self.effective;
        self.recompute_effective();
        if previous != self.effective {
            Some(previous)
        } else {
            None
        }
    }

    /// True when at least [`LEASE_TICK_INTERVAL`] has passed since the last
    /// `lease_tick`. Use to gate the polled janitor at the top of `notify`
    /// without paying a HashMap walk on every event-loop iteration.
    pub fn lease_tick_due(&self, now: Instant) -> bool {
        now.duration_since(self.last_lease_tick) >= LEASE_TICK_INTERVAL
    }

    /// Recompute `effective = max(configured, max(active leases))`. Cheap (one
    /// linear pass over the lease table) and only called on apply/clear/tick,
    /// never on the metric-emission hot path.
    fn recompute_effective(&mut self) {
        let mut max_lease = self.configured;
        for entry in self.leases.values() {
            if entry.level > max_lease {
                max_lease = entry.level;
            }
        }
        self.effective = max_lease;
    }

    pub fn socket(&self) -> Option<&UdpSocket> {
        self.network.as_ref().map(|n| &n.remote.get_ref().socket)
    }

    pub fn socket_mut(&mut self) -> Option<&mut UdpSocket> {
        self.network
            .as_mut()
            .map(|n| &mut n.remote.get_mut().socket)
    }

    pub fn count_add(&mut self, key: &'static str, count_value: i64) {
        self.receive_metric(key, None, None, MetricValue::Count(count_value));
    }

    pub fn set_gauge(&mut self, key: &'static str, gauge_value: usize) {
        self.receive_metric(key, None, None, MetricValue::Gauge(gauge_value));
    }

    pub fn gauge_add(&mut self, key: &'static str, gauge_value: i64) {
        self.receive_metric(key, None, None, MetricValue::GaugeAdd(gauge_value));
    }

    pub fn writable(&mut self) {
        if let Some(ref mut net) = self.network.as_mut() {
            net.writable();
        }
    }

    pub fn send_data(&mut self) {
        if let Some(ref mut net) = self.network.as_mut() {
            net.send_metrics();
        }
    }

    pub fn dump_local_proxy_metrics(&mut self) -> BTreeMap<String, FilteredMetrics> {
        self.local.dump_proxy_metrics(&Vec::new())
    }

    pub fn query(&mut self, q: &QueryMetricsOptions) -> Result<ResponseContent, MetricError> {
        self.local.query(q)
    }

    pub fn clear_local(&mut self) {
        self.local.clear();
    }

    pub fn configure(&mut self, config: &MetricsConfiguration) {
        self.local.configure(config);
    }
}

impl Subscriber for Aggregator {
    fn receive_metric(
        &mut self,
        label: &'static str,
        cluster_id: Option<&str>,
        backend_id: Option<&str>,
        metric: MetricValue,
    ) {
        // Apply the cardinality knob BEFORE handing the metric to either
        // drain. Both drains see the same filtered labels, keeping the local
        // CLI view consistent with what statsd receives on the wire. Reads
        // `effective` (max of the configured floor and any active leases),
        // which is recomputed off the hot path.
        let (cluster_id, backend_id) =
            filter_labels_for_detail(self.effective, cluster_id, backend_id);
        if let Some(ref mut net) = self.network.as_mut() {
            net.receive_metric(label, cluster_id, backend_id, metric.to_owned());
        }
        self.local
            .receive_metric(label, cluster_id, backend_id, metric);
    }
}

pub struct MetricSocket {
    pub addr: SocketAddr,
    pub socket: UdpSocket,
}

impl Write for MetricSocket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.socket.send_to(buf, self.addr)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn udp_bind() -> Result<UdpSocket, MetricError> {
    let address = "0.0.0.0:0";

    let udp_address =
        address
            .parse::<SocketAddr>()
            .map_err(|parse_error| MetricError::WrongUdpAddress {
                address: address.to_owned(),
                error: parse_error.to_string(),
            })?;

    UdpSocket::bind(udp_address).map_err(|parse_error| MetricError::UdpBind {
        address: udp_address.to_string(),
        error: parse_error.to_string(),
    })
}

/// adds a value to a counter
#[macro_export]
macro_rules! count (
  ($key:expr, $value: expr) => ({
    let v = $value;
    $crate::metrics::METRICS.with(|metrics| {
      (*metrics.borrow_mut()).count_add($key, v);
    });
  })
);

/// adds 1 to a counter
#[macro_export]
macro_rules! incr (
  ($key:expr) => (count!($key, 1));
  ($key:expr, $cluster_id:expr, $backend_id:expr) => {
    {
        use $crate::metrics::Subscriber;

        $crate::metrics::METRICS.with(|metrics| {
          (*metrics.borrow_mut()).receive_metric($key, $cluster_id, $backend_id, $crate::metrics::MetricValue::Count(1));
        });
    }
  }
);

#[macro_export]
macro_rules! decr (
  ($key:expr) => (count!($key, -1))
);

#[macro_export]
macro_rules! gauge (
  ($key:expr, $value: expr) => ({
    let v = $value;
    $crate::metrics::METRICS.with(|metrics| {
      (*metrics.borrow_mut()).set_gauge($key, v);
    });
  });
  ($key:expr, $value:expr, $cluster_id:expr, $backend_id:expr) => {
    {
        use $crate::metrics::Subscriber;
        let v = $value;

        $crate::metrics::METRICS.with(|metrics| {
          (*metrics.borrow_mut()).receive_metric($key, $cluster_id, $backend_id, $crate::metrics::MetricValue::Gauge(v as usize));
        });
    }
  }
);

#[macro_export]
macro_rules! gauge_add (
  ($key:expr, $value: expr) => ({
    let v = $value;
    $crate::metrics::METRICS.with(|metrics| {
      (*metrics.borrow_mut()).gauge_add($key, v);
    });
  });
  ($key:expr, $value:expr, $cluster_id:expr, $backend_id:expr) => {
    {
        use $crate::metrics::Subscriber;
        let v = $value;

        $crate::metrics::METRICS.with(|metrics| {
          (*metrics.borrow_mut()).receive_metric($key, $cluster_id, $backend_id, $crate::metrics::MetricValue::GaugeAdd(v));
        });
    }
  }
);

#[macro_export]
macro_rules! time (
  ($key:expr, $value: expr) => ({
    use $crate::metrics::{MetricValue,Subscriber};
    let v = $value;
    $crate::metrics::METRICS.with(|metrics| {
      let m = &mut *metrics.borrow_mut();

      m.receive_metric($key, None, None, MetricValue::Time(v as usize));
    });
  });
  ($key:expr, $cluster_id:expr, $value: expr) => ({
    use $crate::metrics::{MetricValue,Subscriber};
    let v = $value;
    $crate::metrics::METRICS.with(|metrics| {
      let m = &mut *metrics.borrow_mut();
      let cluster: &str = $cluster_id;

      m.receive_metric($key, Some(cluster), None, MetricValue::Time(v as usize));
    });
  })
);

#[macro_export]
macro_rules! record_backend_metrics (
  ($cluster_id:expr, $backend_id:expr, $response_time: expr, $backend_connection_time: expr, $bin: expr, $bout: expr) => {
    use $crate::metrics::{MetricValue,Subscriber};
    $crate::metrics::METRICS.with(|metrics| {
      let m = &mut *metrics.borrow_mut();
      let cluster_id: &str = $cluster_id;
      let backend_id: &str = $backend_id;

      m.receive_metric($crate::metrics::names::backend::BYTES_IN, Some(cluster_id), Some(backend_id), MetricValue::Count($bin as i64));
      m.receive_metric($crate::metrics::names::backend::BYTES_OUT, Some(cluster_id), Some(backend_id), MetricValue::Count($bout as i64));
      m.receive_metric($crate::metrics::names::backend::RESPONSE_TIME, Some(cluster_id), Some(backend_id), MetricValue::Time($response_time as usize));
      if let Some(t) = $backend_connection_time {
        m.receive_metric($crate::metrics::names::backend::CONNECTION_TIME, Some(cluster_id), Some(backend_id), MetricValue::Time(t.as_millis() as usize));
      }

      m.receive_metric($crate::metrics::names::backend::REQUESTS, Some(cluster_id), Some(backend_id), MetricValue::Count(1));
    });
  }
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_labels_process_drops_both() {
        assert_eq!(
            filter_labels_for_detail(MetricDetailLevel::Process, Some("c"), Some("b")),
            (None, None),
        );
    }

    #[test]
    fn filter_labels_frontend_drops_both_today() {
        // Reserved for per-listener counters; same as Process for now.
        assert_eq!(
            filter_labels_for_detail(MetricDetailLevel::Frontend, Some("c"), Some("b")),
            (None, None),
        );
    }

    #[test]
    fn filter_labels_cluster_keeps_cluster_drops_backend() {
        assert_eq!(
            filter_labels_for_detail(MetricDetailLevel::Cluster, Some("c"), Some("b")),
            (Some("c"), None),
        );
    }

    #[test]
    fn filter_labels_backend_keeps_both() {
        assert_eq!(
            filter_labels_for_detail(MetricDetailLevel::Backend, Some("c"), Some("b")),
            (Some("c"), Some("b")),
        );
    }

    #[test]
    fn filter_labels_none_in_none_out() {
        // Absent labels stay absent regardless of detail level — the filter
        // never invents a label, only drops.
        for detail in [
            MetricDetailLevel::Process,
            MetricDetailLevel::Frontend,
            MetricDetailLevel::Cluster,
            MetricDetailLevel::Backend,
        ] {
            assert_eq!(filter_labels_for_detail(detail, None, None), (None, None));
        }
    }

    #[test]
    fn aggregator_default_detail_is_cluster() {
        // Pre-knob behaviour preserved: if a worker / process never calls
        // `set_up_detail`, cluster-scoped metrics still reach the drains.
        let agg = Aggregator::new("sozu".to_owned());
        assert_eq!(agg.detail_configured(), MetricDetailLevel::Cluster);
        assert_eq!(agg.detail_effective(), MetricDetailLevel::Cluster);
        assert_eq!(agg.lease_count(), 0);
    }

    /// Fully-known binding used by tests that don't otherwise care about the
    /// peer-binding mechanic. Two distinct values (`OWNER_*` / `OTHER_*`)
    /// let `lease_clear` tests assert authorised vs unauthorised paths.
    fn owner_binding() -> PeerBinding {
        PeerBinding {
            pid: Some(1234),
            session_ulid: Some(0x0123_4567_89ab_cdef_0123_4567_89ab_cdefu128),
        }
    }

    fn other_binding() -> PeerBinding {
        PeerBinding {
            pid: Some(5678),
            session_ulid: Some(0xfedc_ba98_7654_3210_fedc_ba98_7654_3210u128),
        }
    }

    /// Extract `(previous_effective, new_effective)` from a successful
    /// `lease_apply`; panics on any failure arm so tests that don't care
    /// about the failure paths stay compact.
    fn unwrap_applied(outcome: LeaseApplyOutcome) -> (MetricDetailLevel, MetricDetailLevel) {
        match outcome {
            LeaseApplyOutcome::Applied {
                previous_effective,
                new_effective,
            } => (previous_effective, new_effective),
            other => panic!("expected LeaseApplyOutcome::Applied, got {other:?}"),
        }
    }

    #[test]
    fn lease_apply_elevates_effective_above_configured() {
        // Configured floor stays at Cluster; a lease for Backend lifts the
        // effective level until the lease expires or is cleared.
        let mut agg = Aggregator::new("sozu".to_owned());
        agg.set_up_detail(MetricDetailLevel::Cluster);
        let (prev, new) = unwrap_applied(agg.lease_apply(
            "test:1".to_owned(),
            MetricDetailLevel::Backend,
            Duration::from_secs(60),
            PeerBinding::default(),
        ));
        assert_eq!(prev, MetricDetailLevel::Cluster);
        assert_eq!(new, MetricDetailLevel::Backend);
        assert_eq!(agg.detail_configured(), MetricDetailLevel::Cluster);
        assert_eq!(agg.detail_effective(), MetricDetailLevel::Backend);
        assert_eq!(agg.lease_count(), 1);
    }

    #[test]
    fn lease_apply_below_configured_does_not_lower_effective() {
        // A lease can only ELEVATE the floor, never push below `configured`.
        let mut agg = Aggregator::new("sozu".to_owned());
        agg.set_up_detail(MetricDetailLevel::Backend);
        let (prev, new) = unwrap_applied(agg.lease_apply(
            "test:1".to_owned(),
            MetricDetailLevel::Cluster,
            Duration::from_secs(60),
            PeerBinding::default(),
        ));
        assert_eq!(prev, MetricDetailLevel::Backend);
        assert_eq!(new, MetricDetailLevel::Backend);
    }

    #[test]
    fn lease_apply_rejects_client_id_over_cap() {
        // Defence-in-depth: even if dispatch lets a too-long id through,
        // the aggregator refuses to store it.
        let mut agg = Aggregator::new("sozu".to_owned());
        let too_long = "x".repeat(LEASE_CLIENT_ID_MAX_BYTES + 1);
        assert_eq!(
            agg.lease_apply(
                too_long,
                MetricDetailLevel::Backend,
                Duration::from_secs(60),
                PeerBinding::default(),
            ),
            LeaseApplyOutcome::ClientIdTooLong
        );
        assert_eq!(agg.lease_count(), 0);
    }

    #[test]
    fn lease_apply_rejects_when_table_is_full() {
        // Fill the table to capacity with distinct client_ids; one more
        // insert is refused. A RENEWAL of an existing entry must still
        // succeed (replaces in place, count unchanged).
        let mut agg = Aggregator::new("sozu".to_owned());
        for i in 0..LEASE_TABLE_CAP {
            assert!(matches!(
                agg.lease_apply(
                    format!("client:{i:02}"),
                    MetricDetailLevel::Backend,
                    Duration::from_secs(60),
                    PeerBinding::default(),
                ),
                LeaseApplyOutcome::Applied { .. }
            ));
        }
        assert_eq!(agg.lease_count() as usize, LEASE_TABLE_CAP);
        // New distinct client → rejected.
        assert_eq!(
            agg.lease_apply(
                "newcomer".to_owned(),
                MetricDetailLevel::Backend,
                Duration::from_secs(60),
                PeerBinding::default(),
            ),
            LeaseApplyOutcome::TableFull,
        );
        assert_eq!(agg.lease_count() as usize, LEASE_TABLE_CAP);
        // Renewal of an existing entry → still accepted.
        assert!(matches!(
            agg.lease_apply(
                "client:00".to_owned(),
                MetricDetailLevel::Backend,
                Duration::from_secs(120),
                PeerBinding::default(),
            ),
            LeaseApplyOutcome::Applied { .. }
        ));
        assert_eq!(agg.lease_count() as usize, LEASE_TABLE_CAP);
    }

    #[test]
    fn lease_apply_rejects_ttl_over_max() {
        // The aggregator no longer silently clamps oversize TTLs.
        let mut agg = Aggregator::new("sozu".to_owned());
        assert_eq!(
            agg.lease_apply(
                "client:0".to_owned(),
                MetricDetailLevel::Backend,
                LEASE_TTL_MAX + Duration::from_secs(1),
                PeerBinding::default(),
            ),
            LeaseApplyOutcome::TtlOutOfRange,
        );
        assert_eq!(agg.lease_count(), 0);
    }

    #[test]
    fn lease_apply_renewal_replaces_previous_for_same_client() {
        // The renewer client re-sends with the same `client_id`; the entry
        // is REPLACED (not duplicated). Lease count stays at 1.
        // Unknown bindings on both sides skip the renewal-binding gate.
        let mut agg = Aggregator::new("sozu".to_owned());
        let _ = agg.lease_apply(
            "renewer".to_owned(),
            MetricDetailLevel::Backend,
            Duration::from_secs(30),
            PeerBinding::default(),
        );
        let _ = agg.lease_apply(
            "renewer".to_owned(),
            MetricDetailLevel::Backend,
            Duration::from_secs(60),
            PeerBinding::default(),
        );
        assert_eq!(agg.lease_count(), 1);
    }

    #[test]
    fn lease_apply_renewal_rejects_foreign_binding() {
        // Same-UID `client_id` collision attack: the victim applies with
        // a known binding; an attacker that learns the `client_id`
        // attempts to renew under a different (pid, session_ulid). The
        // renewal must be refused so the victim's apply-time binding
        // remains the sole authoritative owner — both for subsequent
        // renewals AND for the victim's own Drop-time `clear`.
        let mut agg = Aggregator::new("sozu".to_owned());
        let victim = PeerBinding {
            pid: Some(4242),
            session_ulid: Some(0x0123_4567_89AB_CDEF_FEDC_BA98_7654_3210),
        };
        let outcome = agg.lease_apply(
            "topcli".to_owned(),
            MetricDetailLevel::Backend,
            Duration::from_secs(60),
            victim,
        );
        assert!(
            matches!(outcome, LeaseApplyOutcome::Applied { .. }),
            "victim's initial apply must succeed"
        );
        let attacker = PeerBinding {
            pid: Some(9999),
            session_ulid: Some(0xDEAD_BEEF_DEAD_BEEF_DEAD_BEEF_DEAD_BEEF),
        };
        let outcome = agg.lease_apply(
            "topcli".to_owned(),
            MetricDetailLevel::Backend,
            Duration::from_secs(60),
            attacker,
        );
        assert_eq!(
            outcome,
            LeaseApplyOutcome::Unauthorized,
            "renewal with a mismatched known binding must be refused"
        );
        // The victim can still clear their own lease — proof the
        // refused attempt did not corrupt the stored binding.
        let clear = agg.lease_clear("topcli", victim);
        assert!(
            matches!(clear, LeaseClearOutcome::Cleared { .. }),
            "victim's original binding must still clear cleanly after \
             the foreign-binding renewal was refused"
        );
    }

    #[test]
    fn lease_apply_renewal_with_matching_binding_succeeds() {
        // Symmetry case: the legitimate owner re-applies with the same
        // (pid, session_ulid). The renewal must succeed so the TUI's
        // own renewer thread keeps the lease alive across its TTL.
        let mut agg = Aggregator::new("sozu".to_owned());
        let owner = PeerBinding {
            pid: Some(1234),
            session_ulid: Some(0xAAAA_BBBB_CCCC_DDDD_EEEE_FFFF_0000_1111),
        };
        let _ = agg.lease_apply(
            "topcli".to_owned(),
            MetricDetailLevel::Backend,
            Duration::from_secs(30),
            owner,
        );
        let outcome = agg.lease_apply(
            "topcli".to_owned(),
            MetricDetailLevel::Backend,
            Duration::from_secs(60),
            owner,
        );
        assert!(
            matches!(outcome, LeaseApplyOutcome::Applied { .. }),
            "renewal with matching binding must succeed (otherwise the \
             TUI's own renewer thread would be locked out)"
        );
    }

    #[test]
    fn lease_apply_max_merge_two_clients() {
        // Two clients, two levels: effective = max(both leases, configured).
        // Use `Process` as the floor so the Frontend lease is observable
        // after the Backend lease is cleared (otherwise the configured
        // Cluster floor would mask the Frontend lease).
        let mut agg = Aggregator::new("sozu".to_owned());
        agg.set_up_detail(MetricDetailLevel::Process);
        let _ = agg.lease_apply(
            "scraper".to_owned(),
            MetricDetailLevel::Frontend,
            Duration::from_secs(60),
            PeerBinding::default(),
        );
        let _ = agg.lease_apply(
            "topcli".to_owned(),
            MetricDetailLevel::Backend,
            Duration::from_secs(60),
            PeerBinding::default(),
        );
        assert_eq!(agg.detail_effective(), MetricDetailLevel::Backend);
        assert_eq!(agg.lease_count(), 2);
        // Clearing the higher lease drops effective back to the lower one.
        let outcome = agg.lease_clear("topcli", PeerBinding::default());
        assert_eq!(
            outcome,
            LeaseClearOutcome::Cleared {
                previous_effective: MetricDetailLevel::Backend,
            }
        );
        assert_eq!(agg.detail_effective(), MetricDetailLevel::Frontend);
        assert_eq!(agg.lease_count(), 1);
    }

    #[test]
    fn lease_clear_unknown_id_is_silent_noop() {
        // Mismatched IDs are silently ignored (other clients' leases unaffected).
        let mut agg = Aggregator::new("sozu".to_owned());
        let _ = agg.lease_apply(
            "real".to_owned(),
            MetricDetailLevel::Backend,
            Duration::from_secs(60),
            PeerBinding::default(),
        );
        assert_eq!(
            agg.lease_clear("ghost", PeerBinding::default()),
            LeaseClearOutcome::NotFound
        );
        assert_eq!(agg.detail_effective(), MetricDetailLevel::Backend);
        assert_eq!(agg.lease_count(), 1);
    }

    #[test]
    fn lease_clear_with_matching_binding_authorised() {
        // Apply with a known binding, clear with the same binding -> Cleared.
        let mut agg = Aggregator::new("sozu".to_owned());
        let _ = agg.lease_apply(
            "owner-lease".to_owned(),
            MetricDetailLevel::Backend,
            Duration::from_secs(60),
            owner_binding(),
        );
        let outcome = agg.lease_clear("owner-lease", owner_binding());
        assert!(matches!(outcome, LeaseClearOutcome::Cleared { .. }));
        assert_eq!(agg.lease_count(), 0);
    }

    #[test]
    fn lease_clear_with_mismatched_binding_is_unauthorized() {
        // Apply with one binding, attempt clear with a different binding ->
        // Unauthorized; lease left intact.
        let mut agg = Aggregator::new("sozu".to_owned());
        let _ = agg.lease_apply(
            "owner-lease".to_owned(),
            MetricDetailLevel::Backend,
            Duration::from_secs(60),
            owner_binding(),
        );
        let outcome = agg.lease_clear("owner-lease", other_binding());
        assert_eq!(outcome, LeaseClearOutcome::Unauthorized);
        assert_eq!(agg.lease_count(), 1);
    }

    #[test]
    fn lease_clear_unknown_apply_binding_accepts_any_clear() {
        // Pre-binding / non-Linux apply -> any clear authorised.
        let mut agg = Aggregator::new("sozu".to_owned());
        let _ = agg.lease_apply(
            "legacy".to_owned(),
            MetricDetailLevel::Backend,
            Duration::from_secs(60),
            PeerBinding::default(),
        );
        let outcome = agg.lease_clear("legacy", owner_binding());
        assert!(matches!(outcome, LeaseClearOutcome::Cleared { .. }));
        assert_eq!(agg.lease_count(), 0);
    }

    #[test]
    fn lease_clear_known_apply_rejects_default_clear() {
        // Known apply binding -> a default ("unknown") clear is rejected.
        let mut agg = Aggregator::new("sozu".to_owned());
        let _ = agg.lease_apply(
            "owner-lease".to_owned(),
            MetricDetailLevel::Backend,
            Duration::from_secs(60),
            owner_binding(),
        );
        let outcome = agg.lease_clear("owner-lease", PeerBinding::default());
        assert_eq!(outcome, LeaseClearOutcome::Unauthorized);
    }

    #[test]
    fn lease_tick_expires_only_past_due_leases() {
        // `lease_tick(now)` parameterises the clock so we can test expiry
        // without sleeping. Setup: one lease past due, one still active.
        // Use `Process` as the floor so the surviving Frontend lease drives
        // the effective level after the Backend lease expires (otherwise
        // the Cluster floor would mask it).
        let mut agg = Aggregator::new("sozu".to_owned());
        agg.set_up_detail(MetricDetailLevel::Process);
        let now = Instant::now();
        // Inject directly into the table to control expires_at deterministically.
        agg.leases.insert(
            "expired".to_owned(),
            LeaseEntry {
                level: MetricDetailLevel::Backend,
                expires_at: now - Duration::from_secs(1),
                binding: PeerBinding::default(),
            },
        );
        agg.leases.insert(
            "live".to_owned(),
            LeaseEntry {
                level: MetricDetailLevel::Frontend,
                expires_at: now + Duration::from_secs(60),
                binding: PeerBinding::default(),
            },
        );
        agg.recompute_effective();
        assert_eq!(agg.detail_effective(), MetricDetailLevel::Backend);
        let prev = agg.lease_tick(now);
        assert_eq!(prev, Some(MetricDetailLevel::Backend));
        assert_eq!(agg.detail_effective(), MetricDetailLevel::Frontend);
        assert_eq!(agg.lease_count(), 1);
    }

    #[test]
    fn lease_tick_no_change_returns_none() {
        // No leases -> no-op tick, no audit signal.
        let mut agg = Aggregator::new("sozu".to_owned());
        assert!(agg.lease_tick(Instant::now()).is_none());
    }

    #[test]
    fn lease_apply_at_max_ttl_succeeds() {
        // Boundary: exactly LEASE_TTL_MAX is allowed; LEASE_TTL_MAX + 1ns is
        // rejected (covered by lease_apply_rejects_ttl_over_max above).
        let mut agg = Aggregator::new("sozu".to_owned());
        let now = Instant::now();
        let outcome = agg.lease_apply(
            "max".to_owned(),
            MetricDetailLevel::Backend,
            LEASE_TTL_MAX,
            PeerBinding::default(),
        );
        assert!(matches!(outcome, LeaseApplyOutcome::Applied { .. }));
        let entry = agg.leases.get("max").unwrap();
        assert!(entry.expires_at <= now + LEASE_TTL_MAX + Duration::from_millis(50));
    }
}
