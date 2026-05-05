//! A local drain to accumulate metrics and return them in a protobuf format
//!
//! The metrics are stored following this hierarchy (pseudo-rust):
//!
//! ```plain
//! LocalDrain {
//!     proxy_metrics: MetricsMap {
//!         map: BTreeMap<metric_name, AggregatedMetric>
//!     },
//!     cluster_metrics: BTreeMap<cluster_id, LocalClusterMetrics {
//!         cluster: MetricsMap {
//!             map: BTreeMap<metric_name, AggregatedMetric>
//!         },
//!         backends: Vec<LocalBackendMetrics {
//!             backend_id,
//!             MetricsMap {
//!                 map: BTreeMap<metric_name, AggregatedMetric>
//!             },
//!         }>
//!     }>
//! }
//! ```

#![allow(dead_code)]
use std::{
    collections::{BTreeMap, HashSet},
    str,
    time::Instant,
};

use hdrhistogram::Histogram;
use sozu_command::proto::command::{
    AvailableMetrics, BackendMetrics, Bucket, ClusterMetrics, FilteredHistogram, FilteredMetrics,
    MetricsConfiguration, Percentiles, QueryMetricsOptions, ResponseContent, WorkerMetrics,
    filtered_metrics, response_content::ContentType,
};

use crate::metrics::{MetricError, MetricValue, Subscriber};

/// metrics as stored in the local drain
#[derive(Debug, Clone)]
pub enum AggregatedMetric {
    Gauge(usize),
    Count(i64),
    /// `Histogram<u64>` so per-bucket counters do not saturate on long-running
    /// workers. Counts are cumulative since worker start; at sustained
    /// ≥1 M samples/s in a single popular bucket, `u32` would saturate in
    /// roughly 72 minutes — unacceptable for a continuous proxy. Memory cost
    /// is bounded (sigfig=3) and roughly 2× the previous shape per histogram.
    Time(Histogram<u64>),
}

impl AggregatedMetric {
    fn new(metric: MetricValue) -> Result<AggregatedMetric, MetricError> {
        match metric {
            MetricValue::Gauge(value) => Ok(AggregatedMetric::Gauge(value)),
            MetricValue::GaugeAdd(value) => {
                if value >= 0 {
                    Ok(AggregatedMetric::Gauge(value as usize))
                } else {
                    error!(
                        "local drain metric created with negative GaugeAdd({}), clamping to 0",
                        value
                    );
                    Ok(AggregatedMetric::Gauge(0))
                }
            }
            MetricValue::Count(value) => Ok(AggregatedMetric::Count(value)),
            MetricValue::Time(value) => {
                let mut histogram = ::hdrhistogram::Histogram::new(3).map_err(|error| {
                    MetricError::HistogramCreation {
                        time_metric: metric.clone(),
                        error: error.to_string(),
                    }
                })?;

                histogram.record(value as u64).map_err(|error| {
                    MetricError::TimeMetricRecordingError {
                        time_metric: metric.clone(),
                        error: error.to_string(),
                    }
                })?;

                Ok(AggregatedMetric::Time(histogram))
            }
        }
    }

    fn update(&mut self, key: &str, m: MetricValue) {
        match (self, m) {
            (&mut AggregatedMetric::Gauge(ref mut v1), MetricValue::Gauge(v2)) => {
                *v1 = v2;
            }
            (&mut AggregatedMetric::Gauge(ref mut v1), MetricValue::GaugeAdd(v2)) => {
                let res = *v1 as i64 + v2;
                *v1 = if res >= 0 {
                    res as usize
                } else {
                    error!(
                        "local drain metric {} underflow: previous value: {}, adding: {}",
                        key, *v1, v2
                    );
                    0
                };
            }
            (&mut AggregatedMetric::Count(ref mut v1), MetricValue::Count(v2)) => {
                *v1 += v2;
            }
            (&mut AggregatedMetric::Time(ref mut v1), MetricValue::Time(v2)) => {
                if let Err(e) = (*v1).record(v2 as u64) {
                    error!("could not record time metric: {:?}", e.to_string());
                }
            }
            (s, m) => {
                error!(
                    "tried to update metric {} of value {:?} with an incompatible metric: {:?}",
                    key, s, m
                );
            }
        }
    }

    pub fn to_filtered(&self) -> FilteredMetrics {
        match *self {
            AggregatedMetric::Gauge(i) => FilteredMetrics {
                inner: Some(filtered_metrics::Inner::Gauge(i as u64)),
            },
            AggregatedMetric::Count(i) => FilteredMetrics {
                inner: Some(filtered_metrics::Inner::Count(i)),
            },
            AggregatedMetric::Time(ref hist) => FilteredMetrics {
                inner: Some(filtered_metrics::Inner::Percentiles(
                    histogram_to_percentiles(hist),
                )),
            },
        }
    }
}

pub fn histogram_to_percentiles(hist: &Histogram<u64>) -> Percentiles {
    let sum = hist.len() as f64 * hist.mean();
    Percentiles {
        samples: hist.len(),
        p_50: hist.value_at_percentile(50.0),
        p_90: hist.value_at_percentile(90.0),
        p_99: hist.value_at_percentile(99.0),
        p_99_9: hist.value_at_percentile(99.9),
        p_99_99: hist.value_at_percentile(99.99),
        p_99_999: hist.value_at_percentile(99.999),
        p_100: hist.value_at_percentile(100.0),
        sum: sum as u64,
    }
}

/// convert a collected histogram to a prometheus-compatible format
pub fn filter_histogram(hist: &Histogram<u64>) -> FilteredMetrics {
    let sum: u64 = hist
        .iter_recorded()
        .map(|item| item.value_iterated_to() * item.count_at_value())
        .sum();

    let mut count = 0;
    let buckets = hist
        .iter_log(1, 2.0)
        .map(|value| {
            count += value.count_since_last_iteration();
            Bucket {
                le: value.value_iterated_to(),
                count,
            }
        })
        .collect();

    FilteredMetrics {
        inner: Some(filtered_metrics::Inner::Histogram(FilteredHistogram {
            sum,
            count,
            buckets,
        })),
    }
}

/// a map of metric_name -> metric value
#[derive(Debug, Clone, Default)]
pub struct MetricsMap {
    map: BTreeMap<String, AggregatedMetric>,
}

impl MetricsMap {
    fn new() -> Self {
        Self {
            map: BTreeMap::new(),
        }
    }

    /// convert a metrics map to a map of filtered metrics,
    /// perform a double conversion for time metrics: to percentiles and to histogram
    fn to_filtered_metrics(&self, filter_by_names: &[String]) -> BTreeMap<String, FilteredMetrics> {
        self.map
            .iter()
            .filter(|(name, _)| {
                if !filter_by_names.is_empty() {
                    filter_by_names.contains(name)
                } else {
                    true
                }
            })
            .flat_map(|(name, metric)| {
                let mut filtered = vec![(name.to_owned(), metric.to_filtered())];

                // convert time metrics to a histogram format, on top of percentiles
                if let AggregatedMetric::Time(hist) = metric {
                    filtered.push((format!("{name}_histogram"), filter_histogram(hist)));
                }
                filtered.into_iter()
            })
            .collect()
    }

    /// update an old value with the new one, or insert the new one
    fn receive_metric(
        &mut self,
        metric_name: &str,
        new_value: MetricValue,
    ) -> Result<(), MetricError> {
        match self.map.get_mut(metric_name) {
            Some(old_value) => old_value.update(metric_name, new_value),
            None => {
                let aggregated_metric = AggregatedMetric::new(new_value)?;
                self.map.insert(metric_name.to_owned(), aggregated_metric);
            }
        }
        Ok(())
    }

    fn metric_names(&self) -> impl Iterator<Item = &str> {
        self.map.keys().map(|name| name.as_str())
    }
}

/// local equivalent to proto::command::ClusterMetrics
#[derive(Debug, Default)]
pub struct LocalClusterMetrics {
    cluster: MetricsMap,
    backends: Vec<LocalBackendMetrics>,
}

impl LocalClusterMetrics {
    fn receive_metric(
        &mut self,
        metric_name: &str,
        metric: MetricValue,
    ) -> Result<(), MetricError> {
        self.cluster.receive_metric(metric_name, metric)
    }

    fn receive_backend_metric(
        &mut self,
        metric_name: &str,
        backend_id: &str,
        new_value: MetricValue,
    ) -> Result<(), MetricError> {
        let backend = self
            .backends
            .iter_mut()
            .find(|backend| backend.backend_id == backend_id);

        if let Some(backend) = backend {
            backend.metrics.receive_metric(metric_name, new_value)?;
            return Ok(());
        }

        let mut metrics = MetricsMap::new();
        metrics.receive_metric(metric_name, new_value)?;

        self.backends.push(LocalBackendMetrics {
            backend_id: backend_id.to_owned(),
            metrics,
        });
        Ok(())
    }

    fn to_filtered_metrics(&self, metric_names: &[String]) -> Result<ClusterMetrics, MetricError> {
        let cluster = self.cluster.to_filtered_metrics(metric_names);

        let mut backends: Vec<BackendMetrics> = Vec::new();
        for backend in &self.backends {
            backends.push(backend.to_filtered_metrics(metric_names)?);
        }
        Ok(ClusterMetrics { cluster, backends })
    }

    fn metric_names(&self) -> impl Iterator<Item = &str> {
        let mut dedup_set = HashSet::new();
        self.cluster
            .metric_names()
            .chain(
                self.backends
                    .iter()
                    .flat_map(|backend| backend.metrics_names()),
            )
            .filter(move |&item| dedup_set.insert(item))
    }

    fn contains_backend(&self, backend_id: &str) -> bool {
        for backend in &self.backends {
            if backend.backend_id == backend_id {
                return true;
            }
        }
        false
    }
}

/// local equivalent to proto::command::BackendMetrics
#[derive(Debug, Clone)]
pub struct LocalBackendMetrics {
    backend_id: String,
    metrics: MetricsMap,
}

impl LocalBackendMetrics {
    fn to_filtered_metrics(&self, metric_names: &[String]) -> Result<BackendMetrics, MetricError> {
        let filtered_backend_metrics = self.metrics.to_filtered_metrics(metric_names);

        Ok(BackendMetrics {
            backend_id: self.backend_id.to_owned(),
            metrics: filtered_backend_metrics,
        })
    }

    fn metrics_names(&self) -> impl Iterator<Item = &str> {
        self.metrics.metric_names()
    }
}

/// This gathers metrics locally, to be queried by the CLI
#[derive(Debug)]
pub struct LocalDrain {
    /// a prefix to metric keys, usually "sozu-"
    pub prefix: String,
    pub created: Instant,
    /// metrics of the proxy server (metric_name -> metric value)
    pub proxy_metrics: MetricsMap,
    /// cluster_id -> cluster_metrics
    cluster_metrics: BTreeMap<String, LocalClusterMetrics>,
    use_tagged_metrics: bool,
    origin: String,
    disable_cluster_metrics: bool,
}

impl LocalDrain {
    pub fn new(prefix: String) -> Self {
        LocalDrain {
            prefix,
            created: Instant::now(),
            proxy_metrics: MetricsMap::new(),
            cluster_metrics: BTreeMap::new(),
            use_tagged_metrics: false,
            origin: String::from("x"),
            disable_cluster_metrics: false,
        }
    }

    pub fn configure(&mut self, config: &MetricsConfiguration) {
        match config {
            MetricsConfiguration::Enabled => self.disable_cluster_metrics = false,
            MetricsConfiguration::Disabled => self.disable_cluster_metrics = true,
            MetricsConfiguration::Clear => self.clear(),
        }
    }

    /// Operator-issued reset (`sozu metrics clear`,
    /// [`MetricsConfiguration::Clear`]). Wipes everything: counts, gauges,
    /// and histograms, proxy-wide and per-cluster. Operators ask for this
    /// explicitly via the CLI; gauge preservation across an explicit clear
    /// is surprising. The wall-clock hourly clear (formerly in
    /// `lib/src/server.rs`) has been removed; the local drain is now
    /// cumulative since worker start, with explicit cluster-removal
    /// lifecycle hooks ([`Self::remove_cluster`], [`Self::remove_backend`])
    /// providing the memory bound the hourly clear used to provide.
    ///
    /// Caveat: issuing `clear()` during live traffic resets in-flight gauge
    /// accuracy. Sessions opened before the clear that decrement gauges on
    /// close (e.g. `connections_per_backend`) land on the saturating-to-zero
    /// path in [`AggregatedMetric::update`] and emit one `error!` log line
    /// per occurrence, naming the offending key. The counter recovers as
    /// new sessions arrive.
    pub fn clear(&mut self) {
        self.proxy_metrics = MetricsMap::new();
        self.cluster_metrics.clear();
    }

    /// Drop all metrics for a cluster from the local drain. Called from the
    /// worker IPC dispatch on [`RequestType::RemoveCluster`]. Ignores
    /// `disable_cluster_metrics`: this method only deletes, never emits, so
    /// it is safe to call regardless of the runtime metrics configuration.
    ///
    /// Race note: late access-log / session metrics can fire from
    /// `lib/src/lib.rs` and `lib/src/protocol/mux/stream.rs` after the IPC
    /// removal. `LocalDrain::receive_cluster_metric` will reinsert a cluster
    /// row via `entry().or_default()` for any such late emission. The ghost
    /// row sits idle until the next `RemoveCluster`, the next `AddCluster`
    /// for the same id, or the next operator clear. Single-threaded worker
    /// makes this trivially correct (no actual race) — a "removed clusters"
    /// denylist would be over-engineered for the resulting cosmetic noise.
    pub fn remove_cluster(&mut self, cluster_id: &str) {
        self.cluster_metrics.remove(cluster_id);
    }

    /// Drop all metrics for one backend within a cluster. Called from the
    /// IPC dispatch on [`RequestType::RemoveBackend`]. Leaves the cluster
    /// entry in place if cluster-level metrics or other backends remain;
    /// otherwise drops the empty cluster row to avoid phantom keyspace.
    /// Ignores `disable_cluster_metrics`: deletes only, never emits.
    pub fn remove_backend(&mut self, cluster_id: &str, backend_id: &str) {
        let drop_cluster = if let Some(cluster) = self.cluster_metrics.get_mut(cluster_id) {
            cluster.backends.retain(|b| b.backend_id != backend_id);
            cluster.cluster.map.is_empty() && cluster.backends.is_empty()
        } else {
            false
        };
        if drop_cluster {
            self.cluster_metrics.remove(cluster_id);
        }
    }

    pub fn query(&mut self, options: &QueryMetricsOptions) -> Result<ResponseContent, MetricError> {
        trace!(
            "The local drain received a metrics query with this options: {:?}",
            options
        );

        let QueryMetricsOptions {
            metric_names,
            cluster_ids,
            backend_ids,
            list,
            no_clusters,
            workers: _workers,
        } = options;

        if *list {
            return self.list_all_metric_names();
        }

        if *no_clusters {
            let proxy_metrics = self.dump_proxy_metrics(metric_names);
            return Ok(ContentType::WorkerMetrics(WorkerMetrics {
                proxy: proxy_metrics,
                clusters: BTreeMap::new(),
            })
            .into());
        }

        let worker_metrics = match (cluster_ids.is_empty(), backend_ids.is_empty()) {
            (false, _) => self.query_clusters(cluster_ids, metric_names)?,
            (true, false) => self.query_backends(backend_ids, metric_names)?,
            (true, true) => self.dump_all_metrics(metric_names)?,
        };

        Ok(ContentType::WorkerMetrics(worker_metrics).into())
    }

    fn list_all_metric_names(&self) -> Result<ResponseContent, MetricError> {
        let proxy_metrics = self
            .proxy_metrics
            .metric_names()
            .map(ToString::to_string)
            .collect();

        let mut dedup_set = HashSet::new();

        let mut cluster_metrics: Vec<String> = self
            .cluster_metrics
            .values()
            .flat_map(|cluster| cluster.metric_names())
            .filter(move |&item| dedup_set.insert(item))
            .map(ToString::to_string)
            .collect();

        cluster_metrics.sort_unstable();

        Ok(ContentType::AvailableMetrics(AvailableMetrics {
            proxy_metrics,
            cluster_metrics,
        })
        .into())
    }

    pub fn dump_all_metrics(
        &mut self,
        metric_names: &[String],
    ) -> Result<WorkerMetrics, MetricError> {
        Ok(WorkerMetrics {
            proxy: self.dump_proxy_metrics(metric_names),
            clusters: self.dump_cluster_metrics(metric_names)?,
        })
    }

    pub fn dump_proxy_metrics(
        &mut self,
        metric_names: &[String],
    ) -> BTreeMap<String, FilteredMetrics> {
        self.proxy_metrics.to_filtered_metrics(metric_names)
    }

    pub fn dump_cluster_metrics(
        &mut self,
        metric_names: &[String],
    ) -> Result<BTreeMap<String, ClusterMetrics>, MetricError> {
        self.cluster_metrics
            .keys()
            .map(|cluster_id| {
                Ok((
                    cluster_id.to_owned(),
                    self.metrics_of_one_cluster(cluster_id, metric_names)?,
                ))
            })
            .collect()
    }

    fn metrics_of_one_cluster(
        &self,
        cluster_id: &str,
        metric_names: &[String],
    ) -> Result<ClusterMetrics, MetricError> {
        let local_cluster_metrics = self
            .cluster_metrics
            .get(cluster_id)
            .ok_or(MetricError::NoMetrics(cluster_id.to_owned()))?;

        let filtered = local_cluster_metrics.to_filtered_metrics(metric_names)?;

        Ok(filtered)
    }

    fn metrics_of_one_backend(
        &self,
        backend_id: &str,
        metric_names: &[String],
    ) -> Result<BackendMetrics, MetricError> {
        for cluster_metrics in self.cluster_metrics.values() {
            if let Some(backend_metrics) = cluster_metrics
                .backends
                .iter()
                .find(|backend_metrics| backend_metrics.backend_id == backend_id)
            {
                return backend_metrics.to_filtered_metrics(metric_names);
            }
        }

        Err(MetricError::NoMetrics(format!(
            "No metric for backend {backend_id}"
        )))
    }

    fn query_clusters(
        &mut self,
        cluster_ids: &[String],
        metric_names: &[String],
    ) -> Result<WorkerMetrics, MetricError> {
        debug!("Querying cluster with ids: {:?}", cluster_ids);
        let mut clusters: BTreeMap<String, ClusterMetrics> = BTreeMap::new();

        for cluster_id in cluster_ids {
            clusters.insert(
                cluster_id.to_owned(),
                self.metrics_of_one_cluster(cluster_id, metric_names)?,
            );
        }

        trace!("query result: {:#?}", clusters);
        Ok(WorkerMetrics {
            proxy: BTreeMap::new(),
            clusters,
        })
    }

    fn query_backends(
        &mut self,
        backend_ids: &[String],
        metric_names: &[String],
    ) -> Result<WorkerMetrics, MetricError> {
        let mut clusters: BTreeMap<String, ClusterMetrics> = BTreeMap::new();

        for backend_id in backend_ids {
            // find the cluster
            let (cluster_id, cluster) = match self
                .cluster_metrics
                .iter()
                .find(|(_, cluster)| cluster.contains_backend(backend_id))
            {
                Some(cluster) => cluster,
                None => continue,
            };

            let mut backend_metrics = Vec::new();
            for backend in &cluster.backends {
                backend_metrics.push(backend.to_filtered_metrics(metric_names)?);
            }

            clusters.insert(
                cluster_id.to_owned(),
                ClusterMetrics {
                    cluster: BTreeMap::new(),
                    backends: backend_metrics,
                },
            );
        }

        trace!("query result: {:#?}", clusters);
        Ok(WorkerMetrics {
            proxy: BTreeMap::new(),
            clusters,
        })
    }

    fn receive_cluster_metric(
        &mut self,
        metric_name: &str,
        cluster_id: &str,
        metric: MetricValue,
    ) -> Result<(), MetricError> {
        if self.disable_cluster_metrics {
            return Ok(());
        }

        let local_cluster_metric = self
            .cluster_metrics
            .entry(cluster_id.to_owned())
            .or_default();

        local_cluster_metric.receive_metric(metric_name, metric)
    }

    fn receive_backend_metric(
        &mut self,
        metric_name: &str,
        cluster_id: &str,
        backend_id: &str,
        metric: MetricValue,
    ) -> Result<(), MetricError> {
        if self.disable_cluster_metrics {
            return Ok(());
        }

        let local_cluster_metric = self
            .cluster_metrics
            .entry(cluster_id.to_owned())
            .or_default();

        local_cluster_metric.receive_backend_metric(metric_name, backend_id, metric)
    }

    fn receive_proxy_metric(
        &mut self,
        metric_name: &str,
        metric: MetricValue,
    ) -> Result<(), MetricError> {
        match self.proxy_metrics.map.get_mut(metric_name) {
            Some(stored_metric) => stored_metric.update(metric_name, metric),
            None => {
                let aggregated_metric = AggregatedMetric::new(metric)?;

                self.proxy_metrics
                    .map
                    .insert(String::from(metric_name), aggregated_metric);
            }
        }
        Ok(())
    }
}

impl Subscriber for LocalDrain {
    fn receive_metric(
        &mut self,
        key: &'static str,
        cluster_id: Option<&str>,
        backend_id: Option<&str>,
        metric: MetricValue,
    ) {
        trace!(
            "receiving metric with key {}, cluster_id: {:?}, backend_id: {:?}, metric data: {:?}",
            key, cluster_id, backend_id, metric
        );

        let receive_result = match (cluster_id, backend_id) {
            (Some(cluster_id), Some(backend_id)) => {
                self.receive_backend_metric(key, cluster_id, backend_id, metric)
            }
            (Some(cluster_id), None) => self.receive_cluster_metric(key, cluster_id, metric),
            (None, _) => self.receive_proxy_metric(key, metric),
        };

        if let Err(e) = receive_result {
            error!("Could not receive metric: {}", e);
        }
    }
}
#[cfg(test)]
mod tests {
    use sozu_command::proto::command::{FilteredMetrics, filtered_metrics::Inner};

    use super::*;
    use crate::metrics::names;

    #[test]
    fn receive_and_yield_backend_metrics() {
        let mut local_drain = LocalDrain::new("prefix".to_string());

        local_drain.receive_metric(
            names::backend::CONNECTIONS_PER_BACKEND,
            Some("test-cluster"),
            Some("test-backend-1"),
            MetricValue::Count(1),
        );
        local_drain.receive_metric(
            names::backend::CONNECTIONS_PER_BACKEND,
            Some("test-cluster"),
            Some("test-backend-1"),
            MetricValue::Count(1),
        );

        let mut expected_metrics_1 = BTreeMap::new();
        expected_metrics_1.insert(
            names::backend::CONNECTIONS_PER_BACKEND.to_string(),
            FilteredMetrics {
                inner: Some(Inner::Count(2)),
            },
        );

        let expected_backend_metrics = BackendMetrics {
            backend_id: "test-backend-1".to_string(),
            metrics: expected_metrics_1,
        };

        assert_eq!(
            expected_backend_metrics,
            local_drain
                .metrics_of_one_backend(
                    "test-backend-1",
                    [names::backend::CONNECTIONS_PER_BACKEND.to_string()].as_ref(),
                )
                .expect("could not query metrics for this backend")
        )
    }

    #[test]
    fn receive_and_yield_cluster_metrics() {
        // Mirrors what `kawa_h1::log_request_error` and
        // `mux::stream::generate_access_log` emit when `metrics.detail` is
        // `cluster`: the dotted metric name `http.errors` aggregated under
        // the cluster (with the backend label dropped centrally).
        let mut local_drain = LocalDrain::new("prefix".to_string());
        for _ in 0..2 {
            local_drain.receive_metric(
                "http.errors",
                Some("test-cluster"),
                None,
                MetricValue::Count(1),
            );
        }

        let mut map = BTreeMap::new();
        map.insert(
            "http.errors".to_string(),
            FilteredMetrics {
                inner: Some(Inner::Count(2)),
            },
        );
        let expected_cluster_metrics = ClusterMetrics {
            cluster: map,
            backends: vec![],
        };

        let returned_cluster_metrics = local_drain
            .metrics_of_one_cluster("test-cluster", ["http.errors".to_string()].as_ref())
            .expect("could not query metrics for this cluster");

        assert_eq!(expected_cluster_metrics, returned_cluster_metrics);
    }

    #[test]
    fn receive_and_yield_http_error_metrics_per_backend() {
        // Same shape as the cluster-level test above, but with the backend
        // label preserved (the `metrics.detail = "backend"` path).
        let mut local_drain = LocalDrain::new("prefix".to_string());

        for _ in 0..3 {
            local_drain.receive_metric(
                "http.errors",
                Some("test-cluster"),
                Some("test-backend"),
                MetricValue::Count(1),
            );
        }

        let backend_metrics = local_drain
            .metrics_of_one_backend("test-backend", ["http.errors".to_string()].as_ref())
            .expect("could not query metrics for this backend");

        assert_eq!(
            backend_metrics.metrics.get("http.errors"),
            Some(&FilteredMetrics {
                inner: Some(Inner::Count(3)),
            })
        );
    }

    #[test]
    fn gauge_add_negative_constructor_clamps_to_zero() {
        // GaugeAdd(-1) as the first metric for a backend should clamp to 0,
        // not silently wrap to usize::MAX via `value as usize`.
        let mut local_drain = LocalDrain::new("prefix".to_string());

        local_drain.receive_metric(
            names::backend::CONNECTIONS_PER_BACKEND,
            Some("test-cluster"),
            Some("test-backend-1"),
            MetricValue::GaugeAdd(-1),
        );

        let result = local_drain
            .metrics_of_one_backend(
                "test-backend-1",
                [names::backend::CONNECTIONS_PER_BACKEND.to_string()].as_ref(),
            )
            .expect("could not query metrics for this backend");

        let gauge_value = match result
            .metrics
            .get(names::backend::CONNECTIONS_PER_BACKEND)
            .and_then(|m| m.inner.as_ref())
        {
            Some(Inner::Gauge(v)) => *v,
            other => panic!("expected Gauge, got {other:?}"),
        };

        assert_eq!(
            gauge_value, 0,
            "negative GaugeAdd as first metric must clamp to 0, not wrap"
        );
    }

    #[test]
    fn gauge_add_negative_on_existing_zero_clamps() {
        // GaugeAdd(-1) on an existing Gauge(0) should clamp to 0.
        let mut local_drain = LocalDrain::new("prefix".to_string());

        // First, create the gauge with a positive value then bring it to 0.
        local_drain.receive_metric(
            names::backend::CONNECTIONS_PER_BACKEND,
            Some("test-cluster"),
            Some("test-backend-1"),
            MetricValue::GaugeAdd(1),
        );
        local_drain.receive_metric(
            names::backend::CONNECTIONS_PER_BACKEND,
            Some("test-cluster"),
            Some("test-backend-1"),
            MetricValue::GaugeAdd(-1),
        );

        // Now apply a second -1, which should clamp to 0 (the underflow case).
        local_drain.receive_metric(
            names::backend::CONNECTIONS_PER_BACKEND,
            Some("test-cluster"),
            Some("test-backend-1"),
            MetricValue::GaugeAdd(-1),
        );

        let result = local_drain
            .metrics_of_one_backend(
                "test-backend-1",
                [names::backend::CONNECTIONS_PER_BACKEND.to_string()].as_ref(),
            )
            .expect("could not query metrics for this backend");

        let gauge_value = match result
            .metrics
            .get(names::backend::CONNECTIONS_PER_BACKEND)
            .and_then(|m| m.inner.as_ref())
        {
            Some(Inner::Gauge(v)) => *v,
            other => panic!("expected Gauge, got {other:?}"),
        };

        assert_eq!(gauge_value, 0, "double-decrement on gauge must clamp to 0");
    }

    #[test]
    fn clear_wipes_everything() {
        // Operator-issued clear (`MetricsConfiguration::Clear`) must wipe
        // every map: proxy_metrics, per-cluster gauges, counts, histograms,
        // and per-backend metrics. The wall-clock hourly clear is gone;
        // this is now the only bulk reset and is operator-explicit.
        let mut local_drain = LocalDrain::new("prefix".to_string());

        local_drain.receive_metric("proxy_count", None, None, MetricValue::Count(7));
        local_drain.receive_metric("proxy_gauge", None, None, MetricValue::Gauge(11));
        local_drain.receive_metric(
            "connections_per_backend",
            Some("test-cluster"),
            Some("test-backend-1"),
            MetricValue::GaugeAdd(3),
        );
        local_drain.receive_metric(
            "backend.connections.error",
            Some("test-cluster"),
            Some("test-backend-1"),
            MetricValue::Count(5),
        );
        local_drain.receive_metric(
            "backend.response.time",
            Some("test-cluster"),
            Some("test-backend-1"),
            MetricValue::Time(42),
        );

        local_drain.clear();

        // Proxy-wide map: empty.
        let proxy = local_drain.dump_proxy_metrics(&Vec::new());
        assert!(proxy.is_empty(), "proxy_metrics must be empty after clear");

        // Per-cluster lookup: NoMetrics for both cluster-id and backend-id.
        let cluster_lookup = local_drain.metrics_of_one_cluster("test-cluster", &[]);
        assert!(
            cluster_lookup.is_err(),
            "cluster row must be gone after clear"
        );
        let backend_lookup = local_drain.metrics_of_one_backend("test-backend-1", &[]);
        assert!(
            backend_lookup.is_err(),
            "backend row must be gone after clear"
        );
    }

    #[test]
    fn clear_then_gauge_increment_starts_from_zero() {
        // After a full clear, a fresh Gauge increment must start from 0,
        // not stack onto the pre-clear value.
        let mut local_drain = LocalDrain::new("prefix".to_string());

        local_drain.receive_metric(
            names::backend::CONNECTIONS_PER_BACKEND,
            Some("test-cluster"),
            Some("test-backend-1"),
            MetricValue::GaugeAdd(3),
        );

        local_drain.clear();

        local_drain.receive_metric(
            names::backend::CONNECTIONS_PER_BACKEND,
            Some("test-cluster"),
            Some("test-backend-1"),
            MetricValue::GaugeAdd(2),
        );

        let result = local_drain
            .metrics_of_one_backend(
                "test-backend-1",
                [names::backend::CONNECTIONS_PER_BACKEND.to_string()].as_ref(),
            )
            .expect("backend row must exist after fresh emission");

        let gauge_value = match result
            .metrics
            .get(names::backend::CONNECTIONS_PER_BACKEND)
            .and_then(|m| m.inner.as_ref())
        {
            Some(Inner::Gauge(v)) => *v,
            other => panic!("expected Gauge, got {other:?}"),
        };

        assert_eq!(
            gauge_value, 2,
            "post-clear GaugeAdd(2) must start from 0, not stack onto pre-clear 3"
        );
    }

    #[test]
    fn receive_and_yield_http_status_metrics() {
        // Sanity check that per-code counters (`http.status.{200,…}`) and the
        // bucket counter (`http.status.2xx`) are stored independently and
        // retrievable per backend, mirroring what the H1 + H2 emission paths
        // produce after `crate::metrics::http_status_code_metric_name` short-
        // lists a code.
        let mut local_drain = LocalDrain::new("prefix".to_string());

        for _ in 0..2 {
            local_drain.receive_metric(
                names::http::STATUS_2XX,
                Some("test-cluster"),
                Some("test-backend"),
                MetricValue::Count(1),
            );
            local_drain.receive_metric(
                names::http_status::S200,
                Some("test-cluster"),
                Some("test-backend"),
                MetricValue::Count(1),
            );
        }
        local_drain.receive_metric(
            names::http_status::S404,
            Some("test-cluster"),
            Some("test-backend"),
            MetricValue::Count(1),
        );

        let backend_metrics = local_drain
            .metrics_of_one_backend(
                "test-backend",
                [
                    names::http::STATUS_2XX.to_string(),
                    names::http_status::S200.to_string(),
                    names::http_status::S404.to_string(),
                ]
                .as_ref(),
            )
            .expect("could not query metrics for this backend");

        assert_eq!(
            backend_metrics.metrics.get(names::http::STATUS_2XX),
            Some(&FilteredMetrics {
                inner: Some(Inner::Count(2)),
            })
        );
        assert_eq!(
            backend_metrics.metrics.get(names::http_status::S200),
            Some(&FilteredMetrics {
                inner: Some(Inner::Count(2)),
            })
        );
        assert_eq!(
            backend_metrics.metrics.get(names::http_status::S404),
            Some(&FilteredMetrics {
                inner: Some(Inner::Count(1)),
            })
        );
    }

    #[test]
    fn remove_cluster_drops_all_cluster_metrics() {
        // RemoveCluster IPC must wipe every per-cluster + per-backend entry
        // for the cluster, including gauges. Mirrors the worker dispatch arm
        // wired into `Server::notify_proxys` in `lib/src/server.rs`.
        let mut local_drain = LocalDrain::new("prefix".to_string());

        local_drain.receive_metric(
            "connections_per_backend",
            Some("cluster-a"),
            Some("backend-a"),
            MetricValue::GaugeAdd(2),
        );
        local_drain.receive_metric(
            "backend.connections.error",
            Some("cluster-a"),
            Some("backend-a"),
            MetricValue::Count(3),
        );
        local_drain.receive_metric(
            "backend.response.time",
            Some("cluster-a"),
            Some("backend-a"),
            MetricValue::Time(11),
        );

        local_drain.remove_cluster("cluster-a");

        assert!(
            local_drain
                .metrics_of_one_cluster("cluster-a", &[])
                .is_err(),
            "cluster row must be gone after remove_cluster"
        );
        assert!(
            local_drain
                .metrics_of_one_backend("backend-a", &[])
                .is_err(),
            "backend row must be gone after remove_cluster"
        );
    }

    #[test]
    fn remove_cluster_does_not_touch_other_clusters() {
        let mut local_drain = LocalDrain::new("prefix".to_string());

        local_drain.receive_metric(
            "backend.connections.error",
            Some("cluster-a"),
            Some("backend-a"),
            MetricValue::Count(1),
        );
        local_drain.receive_metric(
            "backend.connections.error",
            Some("cluster-b"),
            Some("backend-b"),
            MetricValue::Count(7),
        );

        local_drain.remove_cluster("cluster-a");

        assert!(
            local_drain
                .metrics_of_one_cluster("cluster-a", &[])
                .is_err(),
            "cluster-a must be gone"
        );
        let surviving = local_drain
            .metrics_of_one_backend(
                "backend-b",
                ["backend.connections.error".to_string()].as_ref(),
            )
            .expect("backend-b must still be reachable");
        match surviving
            .metrics
            .get("backend.connections.error")
            .and_then(|m| m.inner.as_ref())
        {
            Some(Inner::Count(v)) => assert_eq!(*v, 7, "cluster-b counter intact"),
            other => panic!("expected Count for cluster-b, got {other:?}"),
        }
    }

    #[test]
    fn remove_backend_drops_one_backend_keeps_cluster_row() {
        // Removing one backend must leave the cluster row alive when other
        // backends or cluster-level metrics remain.
        let mut local_drain = LocalDrain::new("prefix".to_string());

        // Cluster-level metric (no backend_id).
        local_drain.receive_metric(
            "cluster.connections.error",
            Some("cluster-a"),
            None,
            MetricValue::Count(2),
        );
        // Two backends under cluster-a.
        local_drain.receive_metric(
            "backend.connections.error",
            Some("cluster-a"),
            Some("backend-1"),
            MetricValue::Count(5),
        );
        local_drain.receive_metric(
            "backend.connections.error",
            Some("cluster-a"),
            Some("backend-2"),
            MetricValue::Count(9),
        );

        local_drain.remove_backend("cluster-a", "backend-1");

        assert!(
            local_drain
                .metrics_of_one_backend("backend-1", &[])
                .is_err(),
            "backend-1 must be gone"
        );
        let backend_2 = local_drain
            .metrics_of_one_backend(
                "backend-2",
                ["backend.connections.error".to_string()].as_ref(),
            )
            .expect("backend-2 must still be reachable");
        match backend_2
            .metrics
            .get("backend.connections.error")
            .and_then(|m| m.inner.as_ref())
        {
            Some(Inner::Count(v)) => assert_eq!(*v, 9, "backend-2 counter intact"),
            other => panic!("expected Count for backend-2, got {other:?}"),
        }
        // Cluster row still reachable through its cluster-level metric.
        let cluster = local_drain
            .metrics_of_one_cluster(
                "cluster-a",
                ["cluster.connections.error".to_string()].as_ref(),
            )
            .expect("cluster-a row must survive when cluster-level metrics remain");
        match cluster
            .cluster
            .get("cluster.connections.error")
            .and_then(|m| m.inner.as_ref())
        {
            Some(Inner::Count(v)) => assert_eq!(*v, 2, "cluster-level counter intact"),
            other => panic!("expected Count for cluster-level, got {other:?}"),
        }
    }

    #[test]
    fn remove_backend_drops_empty_cluster_row() {
        // Removing the only backend with no cluster-level metrics must drop
        // the cluster row to avoid phantom keyspace.
        let mut local_drain = LocalDrain::new("prefix".to_string());

        local_drain.receive_metric(
            "backend.connections.error",
            Some("cluster-a"),
            Some("backend-1"),
            MetricValue::Count(5),
        );

        local_drain.remove_backend("cluster-a", "backend-1");

        assert!(
            local_drain
                .metrics_of_one_cluster("cluster-a", &[])
                .is_err(),
            "empty cluster row must be dropped"
        );
    }

    #[test]
    fn remove_backend_unknown_cluster_is_noop() {
        let mut local_drain = LocalDrain::new("prefix".to_string());

        local_drain.receive_metric(
            "backend.connections.error",
            Some("cluster-a"),
            Some("backend-1"),
            MetricValue::Count(5),
        );

        // Should not panic, should not mutate.
        local_drain.remove_backend("nonexistent", "backend-x");

        let backend = local_drain
            .metrics_of_one_backend(
                "backend-1",
                ["backend.connections.error".to_string()].as_ref(),
            )
            .expect("backend-1 must still be reachable");
        match backend
            .metrics
            .get("backend.connections.error")
            .and_then(|m| m.inner.as_ref())
        {
            Some(Inner::Count(v)) => assert_eq!(*v, 5),
            other => panic!("expected Count, got {other:?}"),
        }
    }

    #[test]
    fn count_is_cumulative_across_many_emissions() {
        // Counters in `cluster_metrics` must be cumulative since worker
        // start (no hourly clear). Verifies the contract that replaces the
        // wall-clock reset.
        let mut local_drain = LocalDrain::new("prefix".to_string());

        for _ in 0..100 {
            local_drain.receive_metric(
                "backend.connections.error",
                Some("cluster-a"),
                Some("backend-1"),
                MetricValue::Count(1),
            );
        }
        let backend = local_drain
            .metrics_of_one_backend(
                "backend-1",
                ["backend.connections.error".to_string()].as_ref(),
            )
            .expect("backend-1 must be reachable");
        match backend
            .metrics
            .get("backend.connections.error")
            .and_then(|m| m.inner.as_ref())
        {
            Some(Inner::Count(v)) => assert_eq!(*v, 100),
            other => panic!("expected Count(100), got {other:?}"),
        }

        for _ in 0..100 {
            local_drain.receive_metric(
                "backend.connections.error",
                Some("cluster-a"),
                Some("backend-1"),
                MetricValue::Count(1),
            );
        }
        let backend = local_drain
            .metrics_of_one_backend(
                "backend-1",
                ["backend.connections.error".to_string()].as_ref(),
            )
            .expect("backend-1 must be reachable");
        match backend
            .metrics
            .get("backend.connections.error")
            .and_then(|m| m.inner.as_ref())
        {
            Some(Inner::Count(v)) => assert_eq!(*v, 200, "counter must remain cumulative"),
            other => panic!("expected Count(200), got {other:?}"),
        }
    }

    #[test]
    fn time_histogram_is_cumulative_across_many_emissions() {
        // HDR histogram samples accumulate since worker start. With
        // Histogram<u64>, per-bucket counters do not saturate at realistic
        // sample volumes.
        let mut local_drain = LocalDrain::new("prefix".to_string());

        for ms in 0..100 {
            local_drain.receive_metric(
                "backend.response.time",
                Some("cluster-a"),
                Some("backend-1"),
                MetricValue::Time(ms),
            );
        }
        let backend = local_drain
            .metrics_of_one_backend("backend-1", ["backend.response.time".to_string()].as_ref())
            .expect("backend-1 must be reachable");
        let pct = match backend
            .metrics
            .get("backend.response.time")
            .and_then(|m| m.inner.as_ref())
        {
            Some(Inner::Percentiles(p)) => *p,
            other => panic!("expected Percentiles, got {other:?}"),
        };
        assert_eq!(pct.samples, 100, "first batch: 100 samples");
        let p99_first = pct.p_99;

        for ms in 0..100 {
            local_drain.receive_metric(
                "backend.response.time",
                Some("cluster-a"),
                Some("backend-1"),
                MetricValue::Time(ms),
            );
        }
        let backend = local_drain
            .metrics_of_one_backend("backend-1", ["backend.response.time".to_string()].as_ref())
            .expect("backend-1 must be reachable");
        let pct = match backend
            .metrics
            .get("backend.response.time")
            .and_then(|m| m.inner.as_ref())
        {
            Some(Inner::Percentiles(p)) => *p,
            other => panic!("expected Percentiles, got {other:?}"),
        };
        assert_eq!(
            pct.samples, 200,
            "second batch: histogram still cumulative, total 200 samples"
        );
        // Same value distribution, so p99 should be stable around the same value.
        assert!(
            pct.p_99 >= p99_first.saturating_sub(1) && pct.p_99 <= p99_first.saturating_add(1),
            "p99 stable across cumulative batches (first={p99_first}, second={})",
            pct.p_99
        );
    }

    #[test]
    fn gauge_add_underflow_no_panic_in_debug() {
        // After `clear()` (or first emission), a negative GaugeAdd must
        // saturate to 0 and emit an `error!` log line — never panic, in
        // either debug or release builds. This guards the symmetry between
        // `MetricValue::update` and `AggregatedMetric::update` after the
        // `debug_assert!` was removed from the former.
        let mut local_drain = LocalDrain::new("prefix".to_string());

        local_drain.receive_metric(
            "connections_per_backend",
            Some("cluster-a"),
            Some("backend-1"),
            MetricValue::Gauge(0),
        );
        local_drain.receive_metric(
            "connections_per_backend",
            Some("cluster-a"),
            Some("backend-1"),
            MetricValue::GaugeAdd(-5),
        );

        let backend = local_drain
            .metrics_of_one_backend(
                "backend-1",
                ["connections_per_backend".to_string()].as_ref(),
            )
            .expect("backend-1 must be reachable");
        match backend
            .metrics
            .get("connections_per_backend")
            .and_then(|m| m.inner.as_ref())
        {
            Some(Inner::Gauge(v)) => assert_eq!(*v, 0, "underflow must saturate to 0"),
            other => panic!("expected Gauge, got {other:?}"),
        }
    }
}
