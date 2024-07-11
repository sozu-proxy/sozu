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
    filtered_metrics, response_content::ContentType, AvailableMetrics, BackendMetrics, Bucket,
    ClusterMetrics, FilteredHistogram, FilteredMetrics, MetricsConfiguration, Percentiles,
    QueryMetricsOptions, ResponseContent, WorkerMetrics,
};

use crate::metrics::{MetricError, MetricValue, Subscriber};

/// metrics as stored in the local drain
#[derive(Debug, Clone)]
pub enum AggregatedMetric {
    Gauge(usize),
    Count(i64),
    Time(Histogram<u32>),
}

impl AggregatedMetric {
    fn new(metric: MetricValue) -> Result<AggregatedMetric, MetricError> {
        match metric {
            MetricValue::Gauge(value) => Ok(AggregatedMetric::Gauge(value)),
            MetricValue::GaugeAdd(value) => Ok(AggregatedMetric::Gauge(value as usize)),
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
                *v1 = (*v1 as i64 + v2) as usize;
            }
            (&mut AggregatedMetric::Count(ref mut v1), MetricValue::Count(v2)) => {
                *v1 += v2;
            }
            (&mut AggregatedMetric::Time(ref mut v1), MetricValue::Time(v2)) => {
                if let Err(e) = (*v1).record(v2 as u64) {
                    error!("could not record time metric: {:?}", e.to_string());
                }
            }
            (s, m) => panic!(
                "tried to update metric {key} of value {s:?} with an incompatible metric: {m:?}"
            ),
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

pub fn histogram_to_percentiles(hist: &Histogram<u32>) -> Percentiles {
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
pub fn filter_histogram(hist: &Histogram<u32>) -> FilteredMetrics {
    let sum: u64 = hist
        .iter_recorded()
        .map(|item| item.value_iterated_to() * item.count_at_value() as u64)
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
                if let AggregatedMetric::Time(ref hist) = metric {
                    filtered.push((format!("{}_histogram", name), filter_histogram(hist)));
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

    pub fn clear(&mut self) {
        self.cluster_metrics.clear();
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
            "No metric for backend {}",
            backend_id
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
            key,
            cluster_id,
            backend_id,
            metric
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
    use sozu_command::proto::command::{filtered_metrics::Inner, FilteredMetrics};

    use super::*;

    #[test]
    fn receive_and_yield_backend_metrics() {
        let mut local_drain = LocalDrain::new("prefix".to_string());

        local_drain.receive_metric(
            "connections_per_backend",
            Some("test-cluster"),
            Some("test-backend-1"),
            MetricValue::Count(1),
        );
        local_drain.receive_metric(
            "connections_per_backend",
            Some("test-cluster"),
            Some("test-backend-1"),
            MetricValue::Count(1),
        );

        let mut expected_metrics_1 = BTreeMap::new();
        expected_metrics_1.insert(
            "connections_per_backend".to_string(),
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
                    ["connections_per_backend".to_string()].as_ref(),
                )
                .expect("could not query metrics for this backend")
        )
    }

    #[test]
    fn receive_and_yield_cluster_metrics() {
        let mut local_drain = LocalDrain::new("prefix".to_string());
        local_drain.receive_metric(
            "http_errors",
            Some("test-cluster"),
            None,
            MetricValue::Count(1),
        );
        local_drain.receive_metric(
            "http_errors",
            Some("test-cluster"),
            None,
            MetricValue::Count(1),
        );

        let mut map = BTreeMap::new();
        map.insert(
            "http_errors".to_string(),
            FilteredMetrics {
                inner: Some(Inner::Count(2)),
            },
        );
        let expected_cluster_metrics = ClusterMetrics {
            cluster: map,
            backends: vec![],
        };

        let returned_cluster_metrics = local_drain
            .metrics_of_one_cluster("test-cluster", ["http_errors".to_string()].as_ref())
            .expect("could not query metrics for this cluster");

        assert_eq!(expected_cluster_metrics, returned_cluster_metrics);
    }
}
