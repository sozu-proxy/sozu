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

use anyhow::Context;
use mio::net::UdpSocket;
use sozu_command::proto::command::{
    FilteredMetrics, MetricsConfiguration, QueryMetricsOptions, ResponseContent,
};

use self::{local_drain::LocalDrain, network_drain::NetworkDrain};

thread_local! {
  pub static METRICS: RefCell<Aggregator> = RefCell::new(Aggregator::new(String::from("sozu")));
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
) -> anyhow::Result<()> {
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
}

impl Aggregator {
    pub fn new(prefix: String) -> Aggregator {
        Aggregator {
            prefix: prefix.clone(),
            network: None,
            local: LocalDrain::new(prefix),
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

    pub fn query(&mut self, q: &QueryMetricsOptions) -> anyhow::Result<ResponseContent> {
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
        if let Some(ref mut net) = self.network.as_mut() {
            net.receive_metric(label, cluster_id, backend_id, metric.to_owned());
        }
        self.local
            .receive_metric(label, cluster_id, backend_id, metric);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricLine {
    label: &'static str,
    cluster_id: Option<String>,
    backend_id: Option<String>,
    /// in milliseconds
    duration: usize,
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

pub fn udp_bind() -> anyhow::Result<UdpSocket> {
    let address = "0.0.0.0:0"
        .parse()
        .with_context(|| "could not parse 0.0.0.0:0")?;
    UdpSocket::bind(address).with_context(|| "Could not bind to 0.0.0.0:0 udp socket")
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
    use $crate::metrics::Subscriber;

    $crate::metrics::METRICS.with(|metrics| {
      (*metrics.borrow_mut()).receive_metric($key, $cluster_id, $backend_id, $crate::metrics::MetricValue::Count(1));
    });
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
  })
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
    use $crate::metrics::Subscriber;
    let v = $value;

    $crate::metrics::METRICS.with(|metrics| {
      (*metrics.borrow_mut()).receive_metric($key, $cluster_id, $backend_id, $crate::metrics::MetricValue::GaugeAdd(v));
    });
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
        m.receive_metric("backend_connection_time", Some(cluster_id), Some(backend_id), MetricValue::Time(t.whole_milliseconds() as usize));
      }

      m.receive_metric("requests", Some(cluster_id), Some(backend_id), MetricValue::Count(1));
    });
  }
);
