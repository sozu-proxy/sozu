use std::{
    collections::{HashMap, VecDeque, hash_map::Entry},
    io::{ErrorKind, Write},
    net::SocketAddr,
    str,
    time::{Duration, Instant},
};

use mio::net::UdpSocket;

use super::{
    MetricValue, StoredMetricValue, Subscriber,
    writer::{MetricSocket, MetricsWriter},
};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricLine {
    label: &'static str,
    cluster_id: Option<String>,
    backend_id: Option<String>,
    /// in milliseconds
    duration: usize,
}

/// gathers metrics and send them on a UDP socket
pub struct NetworkDrain {
    queue: VecDeque<MetricLine>,
    /// a prefix appended to metrics keys, usually "sozu-"
    pub prefix: String,
    /// writes metrics onto a UDP socket
    pub remote: MetricsWriter,
    is_writable: bool,
    proxy_metrics: HashMap<String, StoredMetricValue>,
    /// (cluster_id, key) -> metric
    cluster_metrics: HashMap<(String, String), StoredMetricValue>,
    /// (cluster_id, backend_id, key) -> metric
    backend_metrics: HashMap<(String, String, String), StoredMetricValue>,
    pub use_tagged_metrics: bool,
    pub origin: String,
    created: Instant,
}

impl NetworkDrain {
    pub fn new(prefix: String, socket: UdpSocket, addr: SocketAddr) -> Self {
        socket.connect(addr).unwrap();

        NetworkDrain {
            queue: VecDeque::new(),
            prefix,
            remote: MetricsWriter::new(MetricSocket { addr, socket }),
            is_writable: true,
            proxy_metrics: HashMap::new(),
            cluster_metrics: HashMap::new(),
            backend_metrics: HashMap::new(),
            use_tagged_metrics: false,
            origin: String::from("x"),
            created: Instant::now(),
        }
    }

    pub fn writable(&mut self) {
        self.is_writable = true;
    }

    /// Drop all wire-side metric storage for a cluster: stored counters /
    /// gauges in `cluster_metrics` + `backend_metrics`, and any queued
    /// `MetricLine` carrying the cluster id. Called from
    /// [`super::Aggregator::remove_cluster`] on `RequestType::RemoveCluster`.
    /// Any unsent statsd interval for the cluster is **dropped**, not
    /// emitted: there is no final flush. Operators that need per-removal
    /// delta accuracy must trigger a flush before the IPC. In practice
    /// cluster-removal events are rare and the loss is bounded by the
    /// metrics send cadence (typically ≤1 second).
    pub fn remove_cluster(&mut self, cluster_id: &str) {
        self.cluster_metrics.retain(|(c, _), _| c != cluster_id);
        self.backend_metrics.retain(|(c, _, _), _| c != cluster_id);
        self.queue
            .retain(|m| m.cluster_id.as_deref() != Some(cluster_id));
    }

    /// Drop all wire-side metric storage for one backend: stored counters
    /// in `backend_metrics` and any queued `MetricLine` for that backend.
    /// Called from [`super::Aggregator::remove_backend`] on
    /// `RequestType::RemoveBackend`. Same final-delta-loss caveat as
    /// [`Self::remove_cluster`].
    pub fn remove_backend(&mut self, cluster_id: &str, backend_id: &str) {
        self.backend_metrics
            .retain(|(c, b, _), _| !(c == cluster_id && b == backend_id));
        self.queue.retain(|m| {
            !(m.cluster_id.as_deref() == Some(cluster_id)
                && m.backend_id.as_deref() == Some(backend_id))
        });
    }

    pub fn send_metrics(&mut self) {
        let now = Instant::now();
        let secs = Duration::new(1, 0);
        let mut send_count = 0;

        // remove metrics that were not touched in the last 10mn
        self.cluster_metrics.retain(|_, ref value| {
            value.updated || now.duration_since(value.last_sent) < Duration::new(600, 00)
        });
        self.backend_metrics.retain(|_, ref value| {
            value.updated || now.duration_since(value.last_sent) < Duration::new(600, 00)
        });

        if !self.is_writable {
            return;
        }

        if self.is_writable {
            for (ref key, ref mut stored_metric) in self
                .proxy_metrics
                .iter_mut()
                .filter(|(_, value)| value.updated && now.duration_since(value.last_sent) > secs)
            {
                //info!("will write {} -> {:#?}", key, stored_metric);
                let res = match stored_metric.data {
                    MetricValue::Gauge(value) => {
                        if self.use_tagged_metrics {
                            self.remote.write_fmt(format_args!(
                                "{}.{},origin={},version={}:{}|g\n",
                                self.prefix, key, self.origin, VERSION, value
                            ))
                        } else {
                            self.remote.write_fmt(format_args!(
                                "{}.{}.{}:{}|g\n",
                                self.prefix, self.origin, key, value
                            ))
                        }
                    }
                    MetricValue::Count(value) => {
                        if value == 0 {
                            stored_metric.last_sent = now;
                        }

                        let res = if self.use_tagged_metrics {
                            self.remote.write_fmt(format_args!(
                                "{}.{},origin={},version={}:{}|c\n",
                                self.prefix, key, self.origin, VERSION, value
                            ))
                        } else {
                            self.remote.write_fmt(format_args!(
                                "{}.{}.{}:{}|c\n",
                                self.prefix, self.origin, key, value
                            ))
                        };

                        if res.is_ok() {
                            stored_metric.data = MetricValue::Count(0);
                        }

                        res
                    }
                    _ => Ok(()),
                };

                match res {
                    Ok(()) => {
                        stored_metric.last_sent = now;
                        stored_metric.updated = false;

                        send_count += 1;
                        if send_count >= 10 {
                            /*if let Err(e) = self.remote.flush() {
                              error!("error flushing metrics socket: {:?}", e);
                            }*/
                            send_count = 0;
                        }
                    }
                    Err(e) => match e.kind() {
                        ErrorKind::WriteZero => {
                            if let Err(e) = self.remote.flush() {
                                error!("error flushing metrics socket: {:?}", e);
                            }
                        }
                        ErrorKind::WouldBlock => {
                            error!("WouldBlock while writing global metrics to socket");
                            self.is_writable = false;
                            break;
                        }
                        e => {
                            error!("metrics socket write error={:?}", e);
                            break;
                        }
                    },
                }
            }
        }

        /*if let Err(e) = self.remote.flush() {
          error!("error flushing metrics socket: {:?}", e);
        }*/

        if self.is_writable {
            for (key, stored_metric) in self
                .cluster_metrics
                .iter_mut()
                .filter(|(_, value)| value.updated && now.duration_since(value.last_sent) > secs)
            {
                //info!("will write {:?} -> {:#?}", key, stored_metric);
                let res = match stored_metric.data {
                    MetricValue::Gauge(value) => {
                        if self.use_tagged_metrics {
                            self.remote.write_fmt(format_args!(
                                "{}.cluster.{},origin={},version={},cluster_id={}:{}|g\n",
                                self.prefix, key.1, self.origin, VERSION, key.0, value
                            ))
                        } else {
                            self.remote.write_fmt(format_args!(
                                "{}.{}.cluster.{}.{}:{}|g\n",
                                self.prefix, self.origin, key.0, key.1, value
                            ))
                        }
                    }
                    MetricValue::Count(value) => {
                        if value == 0 {
                            stored_metric.last_sent = now;
                        }

                        let res = if self.use_tagged_metrics {
                            self.remote.write_fmt(format_args!(
                                "{}.cluster.{},origin={},version={},cluster_id={}:{}|c\n",
                                self.prefix, key.1, self.origin, VERSION, key.0, value
                            ))
                        } else {
                            self.remote.write_fmt(format_args!(
                                "{}.{}.cluster.{}.{}:{}|c\n",
                                self.prefix, self.origin, key.0, key.1, value
                            ))
                        };

                        if res.is_ok() {
                            stored_metric.data = MetricValue::Count(0);
                        }

                        res
                    }
                    _ => Ok(()),
                };

                match res {
                    Ok(()) => {
                        stored_metric.last_sent = now;
                        stored_metric.updated = false;

                        send_count += 1;
                        if send_count >= 10 {
                            /*if let Err(e) = self.remote.flush() {
                              error!("error flushing metrics socket: {:?}", e);
                            }*/
                            send_count = 0;
                        }
                    }
                    Err(e) => match e.kind() {
                        ErrorKind::WriteZero => {
                            if let Err(e) = self.remote.flush() {
                                error!("error flushing metrics socket: {:?}", e);
                            }
                        }
                        ErrorKind::WouldBlock => {
                            error!("WouldBlock while writing cluster metrics to socket");
                            self.is_writable = false;
                            break;
                        }
                        e => {
                            error!("metrics socket write error={:?}", e);
                            break;
                        }
                    },
                }
            }
        }

        /*if let Err(e) = self.remote.flush() {
          error!("error flushing metrics socket: {:?}", e);
        }*/

        if self.is_writable {
            for (key, stored_metric) in self
                .backend_metrics
                .iter_mut()
                .filter(|(_, value)| value.updated && now.duration_since(value.last_sent) > secs)
            {
                //info!("will write {:?} -> {:#?}", key, stored_metric);
                let res = match stored_metric.data {
                    MetricValue::Gauge(value) => {
                        if self.use_tagged_metrics {
                            self.remote.write_fmt(format_args!(
                                "{}.backend.{},origin={},version={},cluster_id={},backend_id={}:{}|g\n",
                                self.prefix, key.2, self.origin, VERSION, key.0, key.1, value
                            ))
                        } else {
                            self.remote.write_fmt(format_args!(
                                "{}.{}.cluster.{}.backend.{}.{}:{}|g\n",
                                self.prefix, self.origin, key.0, key.1, key.2, value
                            ))
                        }
                    }
                    MetricValue::Count(value) => {
                        if value == 0 {
                            stored_metric.last_sent = now;
                        }

                        let res = if self.use_tagged_metrics {
                            self.remote.write_fmt(format_args!(
                                "{}.backend.{},origin={},version={},cluster_id={},backend_id={}:{}|c\n",
                                self.prefix, key.2, self.origin, VERSION, key.0, key.1, value
                            ))
                        } else {
                            self.remote.write_fmt(format_args!(
                                "{}.{}.cluster.{}.backend.{}.{}:{}|c\n",
                                self.prefix, self.origin, key.0, key.1, key.2, value
                            ))
                        };

                        if res.is_ok() {
                            stored_metric.data = MetricValue::Count(0);
                        }

                        res
                    }
                    _ => Ok(()),
                };

                match res {
                    Ok(()) => {
                        stored_metric.last_sent = now;
                        stored_metric.updated = false;

                        send_count += 1;
                        if send_count >= 10 {
                            /*if let Err(e) = self.remote.flush() {
                              error!("error flushing metrics socket: {:?}", e);
                            }*/
                            send_count = 0;
                        }
                    }
                    Err(e) => match e.kind() {
                        ErrorKind::WriteZero => {
                            if let Err(e) = self.remote.flush() {
                                error!("error flushing metrics socket: {:?}", e);
                            }
                        }
                        ErrorKind::WouldBlock => {
                            error!("WouldBlock while writing backend metrics to socket");
                            self.is_writable = false;
                            break;
                        }
                        e => {
                            error!("metrics socket write error={:?}", e);
                            break;
                        }
                    },
                }
            }
        }
        /*if let Err(e) = self.remote.flush() {
          error!("error flushing metrics socket: {:?}", e);
        }*/

        if self.is_writable {
            for metric in self.queue.drain(..) {
                let res = match (metric.cluster_id, metric.backend_id) {
                    (Some(cluster_id), Some(backend_id)) => {
                        if self.use_tagged_metrics {
                            self.remote.write_fmt(format_args!(
                                "{}.backend.{},origin={},version={},cluster_id={},backend_id={}:{}|ms\n",
                                self.prefix, metric.label, self.origin, VERSION, cluster_id, backend_id, metric.duration
                            ))
                        } else {
                            self.remote.write_fmt(format_args!(
                                "{}.{}.cluster.{}.backend.{}.{}:{}|ms\n",
                                self.prefix,
                                self.origin,
                                cluster_id,
                                backend_id,
                                metric.label,
                                metric.duration
                            ))
                        }
                    }
                    (Some(cluster_id), None) => {
                        if self.use_tagged_metrics {
                            self.remote.write_fmt(format_args!(
                                "{}.cluster.{},origin={},version={},cluster_id={}:{}|ms\n",
                                self.prefix,
                                metric.label,
                                self.origin,
                                VERSION,
                                cluster_id,
                                metric.duration
                            ))
                        } else {
                            self.remote.write_fmt(format_args!(
                                "{}.{}.cluster.{}.{}:{}|ms\n",
                                self.prefix, self.origin, cluster_id, metric.label, metric.duration
                            ))
                        }
                    }
                    (None, None) => {
                        if self.use_tagged_metrics {
                            self.remote.write_fmt(format_args!(
                                "{}.{},origin={},version={}:{}|ms\n",
                                self.prefix, metric.label, self.origin, VERSION, metric.duration
                            ))
                        } else {
                            self.remote.write_fmt(format_args!(
                                "{}.{}.{}:{}|ms\n",
                                self.prefix, self.origin, metric.label, metric.duration
                            ))
                        }
                    }
                    _ => Ok(()),
                };

                match res {
                    Ok(()) => {
                        send_count += 1;
                        if send_count >= 10 {
                            /*if let Err(e) = self.remote.flush() {
                              error!("error flushing metrics socket: {:?}", e);
                            }*/
                            send_count = 0;
                        }
                    }
                    Err(e) => match e.kind() {
                        ErrorKind::WriteZero => {
                            if let Err(e) = self.remote.flush() {
                                error!("error flushing metrics socket: {:?}", e);
                            }
                        }
                        ErrorKind::WouldBlock => {
                            error!("WouldBlock while writing timing metrics to socket");
                            self.is_writable = false;
                            break;
                        }
                        e => {
                            error!("metrics socket write error={:?}", e);
                            break;
                        }
                    },
                }
            }

            /*if let Err(e) = self.remote.flush() {
              error!("error flushing metrics socket: {:?}", e);
            }*/
        }
    }
}

impl Subscriber for NetworkDrain {
    fn receive_metric(
        &mut self,
        key: &'static str,
        cluster_id: Option<&str>,
        backend_id: Option<&str>,
        metric: MetricValue,
    ) {
        if metric.is_time() {
            if let MetricValue::Time(millis) = metric {
                self.queue.push_back(MetricLine {
                    label: key,
                    cluster_id: cluster_id.map(|s| s.to_string()),
                    backend_id: backend_id.map(|s| s.to_string()),
                    duration: millis,
                });
            }
            return;
        }

        match (cluster_id, backend_id) {
            (None, _) => {}
            (Some(cid), None) => {
                let k = (String::from(cid), String::from(key));
                if let Entry::Vacant(e) = self.cluster_metrics.entry(k.to_owned()) {
                    e.insert(StoredMetricValue::new(self.created, metric));
                } else if let Some(stored_metric) = self.cluster_metrics.get_mut(&k) {
                    stored_metric.update(key, metric);
                }
                return;
            }
            (Some(cid), Some(bid)) => {
                let k = (String::from(cid), String::from(bid), String::from(key));
                if let Entry::Vacant(e) = self.backend_metrics.entry(k.to_owned()) {
                    e.insert(StoredMetricValue::new(self.created, metric));
                } else if let Some(stored_metric) = self.backend_metrics.get_mut(&k) {
                    stored_metric.update(key, metric);
                }
                return;
            }
        }
        /*
        if let Some(id) = cluster_id {
            match backend_id {
                Some(bid) => {
                    let k = (String::from(id), String::from(bid), String::from(key));
                    if let Entry::Vacant(e) = self.backend_metrics.entry(k.to_owned()) {
                        e.insert(StoredMetricValue::new(self.created, metric));
                    } else if let Some(stored_metric) = self.backend_metrics.get_mut(&k) {
                        stored_metric.update(key, metric);
                    }
                }
                None => {
                    let k = (String::from(id), String::from(key));
                    if let Entry::Vacant(e) = self.cluster_metrics.entry(k.to_owned()) {
                        e.insert(StoredMetricValue::new(self.created, metric));
                    } else if let Some(stored_metric) = self.cluster_metrics.get_mut(&k) {
                        stored_metric.update(key, metric);
                    }
                }
            }
            return;
        }
        */

        if !self.proxy_metrics.contains_key(key) {
            self.proxy_metrics.insert(
                String::from(key),
                StoredMetricValue::new(self.created, metric),
            );
            return;
        }

        if let Some(stored_metric) = self.proxy_metrics.get_mut(key) {
            stored_metric.update(key, metric);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    use mio::net::UdpSocket;

    use super::{MetricLine, NetworkDrain, StoredMetricValue};
    use crate::metrics::MetricValue;

    fn loopback_drain() -> NetworkDrain {
        let socket = UdpSocket::bind(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)))
            .expect("bind loopback UDP for test");
        let local = socket.local_addr().expect("local_addr");
        NetworkDrain::new("test".to_string(), socket, local)
    }

    fn seed_cluster(drain: &mut NetworkDrain, cluster: &str, key: &str, count: i64) {
        drain.cluster_metrics.insert(
            (cluster.to_string(), key.to_string()),
            StoredMetricValue::new(drain.created, MetricValue::Count(count)),
        );
    }

    fn seed_backend(drain: &mut NetworkDrain, cluster: &str, backend: &str, key: &str, count: i64) {
        drain.backend_metrics.insert(
            (cluster.to_string(), backend.to_string(), key.to_string()),
            StoredMetricValue::new(drain.created, MetricValue::Count(count)),
        );
    }

    fn queue_line(cluster: Option<&str>, backend: Option<&str>) -> MetricLine {
        MetricLine {
            label: "test_metric",
            cluster_id: cluster.map(str::to_string),
            backend_id: backend.map(str::to_string),
            duration: 0,
        }
    }

    #[test]
    fn network_drain_remove_cluster_drops_cluster_and_backend_keys() {
        let mut drain = loopback_drain();

        seed_cluster(&mut drain, "cluster-a", "metric_a", 1);
        seed_cluster(&mut drain, "cluster-b", "metric_a", 2);
        seed_backend(&mut drain, "cluster-a", "backend-1", "metric_b", 3);
        seed_backend(&mut drain, "cluster-b", "backend-2", "metric_b", 4);

        drain.remove_cluster("cluster-a");

        assert!(
            !drain
                .cluster_metrics
                .contains_key(&("cluster-a".to_string(), "metric_a".to_string())),
            "cluster-a stored counter must be gone"
        );
        assert!(
            !drain.backend_metrics.contains_key(&(
                "cluster-a".to_string(),
                "backend-1".to_string(),
                "metric_b".to_string()
            )),
            "cluster-a backend stored counter must be gone"
        );
        assert!(
            drain
                .cluster_metrics
                .contains_key(&("cluster-b".to_string(), "metric_a".to_string())),
            "cluster-b counter must survive"
        );
        assert!(
            drain.backend_metrics.contains_key(&(
                "cluster-b".to_string(),
                "backend-2".to_string(),
                "metric_b".to_string()
            )),
            "cluster-b backend counter must survive"
        );
    }

    #[test]
    fn network_drain_remove_cluster_drains_queue_for_that_cluster() {
        let mut drain = loopback_drain();

        drain.queue.push_back(queue_line(Some("cluster-a"), None));
        drain
            .queue
            .push_back(queue_line(Some("cluster-a"), Some("backend-1")));
        drain.queue.push_back(queue_line(Some("cluster-b"), None));
        drain.queue.push_back(queue_line(None, None));

        drain.remove_cluster("cluster-a");

        let remaining: Vec<_> = drain.queue.iter().map(|m| m.cluster_id.clone()).collect();
        assert_eq!(remaining.len(), 2, "two non-cluster-a lines must survive");
        assert!(
            remaining.iter().all(|c| c.as_deref() != Some("cluster-a")),
            "no cluster-a line must remain in queue"
        );
    }

    #[test]
    fn network_drain_remove_backend_drops_only_one_backend() {
        let mut drain = loopback_drain();

        seed_cluster(&mut drain, "cluster-a", "metric_a", 1);
        seed_backend(&mut drain, "cluster-a", "backend-1", "metric_b", 2);
        seed_backend(&mut drain, "cluster-a", "backend-2", "metric_b", 3);

        drain.remove_backend("cluster-a", "backend-1");

        assert!(
            !drain.backend_metrics.contains_key(&(
                "cluster-a".to_string(),
                "backend-1".to_string(),
                "metric_b".to_string()
            )),
            "backend-1 stored counter must be gone"
        );
        assert!(
            drain.backend_metrics.contains_key(&(
                "cluster-a".to_string(),
                "backend-2".to_string(),
                "metric_b".to_string()
            )),
            "backend-2 stored counter must survive"
        );
        assert!(
            drain
                .cluster_metrics
                .contains_key(&("cluster-a".to_string(), "metric_a".to_string())),
            "cluster-level entry must survive a backend removal"
        );
    }

    #[test]
    fn network_drain_remove_backend_drains_only_that_backend_from_queue() {
        let mut drain = loopback_drain();

        drain
            .queue
            .push_back(queue_line(Some("cluster-a"), Some("backend-1")));
        drain
            .queue
            .push_back(queue_line(Some("cluster-a"), Some("backend-2")));
        drain.queue.push_back(queue_line(Some("cluster-a"), None));
        drain
            .queue
            .push_back(queue_line(Some("cluster-b"), Some("backend-1")));

        drain.remove_backend("cluster-a", "backend-1");

        let remaining: Vec<_> = drain
            .queue
            .iter()
            .map(|m| (m.cluster_id.clone(), m.backend_id.clone()))
            .collect();
        assert_eq!(remaining.len(), 3, "only the targeted line must drop");
        assert!(
            remaining.iter().all(|(c, b)| {
                !(c.as_deref() == Some("cluster-a") && b.as_deref() == Some("backend-1"))
            }),
            "no (cluster-a, backend-1) line must remain"
        );
    }
}
