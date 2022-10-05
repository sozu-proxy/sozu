use std::{
    collections::{hash_map::Entry, HashMap, VecDeque},
    io::{ErrorKind, Write},
    net::SocketAddr,
    str,
    time::{Duration, Instant},
};

use mio::net::UdpSocket;

use super::{
    writer::{MetricSocket, MetricsWriter},
    MetricData, StoredMetricData, Subscriber,
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
    proxy_metrics: HashMap<String, StoredMetricData>,
    /// (cluster_id, key) -> metric
    cluster_metrics: HashMap<(String, String), StoredMetricData>,
    /// (cluster_id, backend_id, key) -> metric
    backend_metrics: HashMap<(String, String, String), StoredMetricData>,
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

    pub fn send_data(&mut self) {
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

        if self.is_writable {
            for (ref key, ref mut stored_metric) in
                self.proxy_metrics.iter_mut().filter(|&(_, ref value)| {
                    value.updated && now.duration_since(value.last_sent) > secs
                })
            {
                //info!("will write {} -> {:#?}", key, stored_metric);
                let res = match stored_metric.data {
                    MetricData::Gauge(value) => {
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
                    MetricData::Count(value) => {
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
                            stored_metric.data = MetricData::Count(0);
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
            for (key, mut stored_metric) in
                self.cluster_metrics.iter_mut().filter(|&(_, ref value)| {
                    value.updated && now.duration_since(value.last_sent) > secs
                })
            {
                //info!("will write {:?} -> {:#?}", key, stored_metric);
                let res = match stored_metric.data {
                    MetricData::Gauge(value) => {
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
                    MetricData::Count(value) => {
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
                            stored_metric.data = MetricData::Count(0);
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
            for (key, mut stored_metric) in self
                .backend_metrics
                .iter_mut()
                .filter(|(_, value)| value.updated && now.duration_since(value.last_sent) > secs)
            {
                //info!("will write {:?} -> {:#?}", key, stored_metric);
                let res = match stored_metric.data {
                    MetricData::Gauge(value) => {
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
                    MetricData::Count(value) => {
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
                            stored_metric.data = MetricData::Count(0);
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
        metric: MetricData,
    ) {
        if metric.is_time() {
            if let MetricData::Time(millis) = metric {
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
                    e.insert(StoredMetricData::new(self.created, metric));
                } else if let Some(stored_metric) = self.cluster_metrics.get_mut(&k) {
                    stored_metric.update(key, metric);
                }
                return;
            }
            (Some(cid), Some(bid)) => {
                let k = (String::from(cid), String::from(bid), String::from(key));
                if let Entry::Vacant(e) = self.backend_metrics.entry(k.to_owned()) {
                    e.insert(StoredMetricData::new(self.created, metric));
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
                        e.insert(StoredMetricData::new(self.created, metric));
                    } else if let Some(stored_metric) = self.backend_metrics.get_mut(&k) {
                        stored_metric.update(key, metric);
                    }
                }
                None => {
                    let k = (String::from(id), String::from(key));
                    if let Entry::Vacant(e) = self.cluster_metrics.entry(k.to_owned()) {
                        e.insert(StoredMetricData::new(self.created, metric));
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
                StoredMetricData::new(self.created, metric),
            );
            return;
        }

        if let Some(stored_metric) = self.proxy_metrics.get_mut(key) {
            stored_metric.update(key, metric);
        }
    }
}
