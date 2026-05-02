//! Metrics façade.
//!
//! Defines the per-thread `Aggregator`, the `incr!`/`count!`/`gauge!`/
//! `gauge_add!`/`time!` macros consumed across the lib + bin crates, the
//! local-vs-network drain dispatch, and the label allow/deny filtering
//! used to keep cardinality bounded. Gauge underflow is treated as a
//! correctness bug (saturating clamp + warn log), not a rounding artefact.

mod local_drain;
mod network_drain;
mod writer;

use std::{
    cell::RefCell,
    collections::BTreeMap,
    io::{self, Write},
    net::SocketAddr,
    str,
    time::Instant,
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

pub struct Aggregator {
    /// appended to metric keys, usually "sozu-"
    prefix: String,
    /// gathers metrics and sends them on a UDP socket
    network: Option<NetworkDrain>,
    /// gather metrics locally, queried by the CLI
    local: LocalDrain,
    /// Cardinality knob — filters `(cluster_id, backend_id)` labels at
    /// emission time. Defaults to `Cluster` to preserve the historical
    /// pre-knob behaviour (cluster-scoped metrics emitted, backend-scoped
    /// labels dropped before this knob existed only when the macros didn't
    /// pass them).
    detail: MetricDetailLevel,
}

impl Aggregator {
    pub fn new(prefix: String) -> Aggregator {
        Aggregator {
            prefix: prefix.clone(),
            network: None,
            local: LocalDrain::new(prefix),
            detail: MetricDetailLevel::default(),
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

    /// Cardinality knob — see [`MetricDetailLevel`] and
    /// [`filter_labels_for_detail`] for the per-level filtering rules.
    pub fn set_up_detail(&mut self, detail: MetricDetailLevel) {
        self.detail = detail;
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
        // CLI view consistent with what statsd receives on the wire.
        let (cluster_id, backend_id) =
            filter_labels_for_detail(self.detail, cluster_id, backend_id);
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

      m.receive_metric("bytes_in", Some(cluster_id), Some(backend_id), MetricValue::Count($bin as i64));
      m.receive_metric("bytes_out", Some(cluster_id), Some(backend_id), MetricValue::Count($bout as i64));
      m.receive_metric("backend_response_time", Some(cluster_id), Some(backend_id), MetricValue::Time($response_time as usize));
      if let Some(t) = $backend_connection_time {
        m.receive_metric("backend_connection_time", Some(cluster_id), Some(backend_id), MetricValue::Time(t.as_millis() as usize));
      }

      m.receive_metric("requests", Some(cluster_id), Some(backend_id), MetricValue::Count(1));
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
        assert_eq!(agg.detail, MetricDetailLevel::Cluster);
    }
}
