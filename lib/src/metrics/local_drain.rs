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
use std::{collections::BTreeMap, str, time::Instant};

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
    Percentiles {
        samples: hist.len(),
        p_50: hist.value_at_percentile(50.0),
        p_90: hist.value_at_percentile(90.0),
        p_99: hist.value_at_percentile(99.0),
        p_99_9: hist.value_at_percentile(99.9),
        p_99_99: hist.value_at_percentile(99.99),
        p_99_999: hist.value_at_percentile(99.999),
        p_100: hist.value_at_percentile(100.0),
    }
}

/// convert a collected histogram to a prometheus-compatible format
pub fn filter_histogram(hist: &Histogram<u32>) -> FilteredMetrics {
    let count = hist.len();
    let mean = hist.mean();
    let min = hist.min();
    let max = hist.max();
    let sum = (count as f64 * mean) as u64;
    let range = max - min;
    let number_of_buckets = 10;
    let mut buckets = Vec::new();

    for i in 0..number_of_buckets {
        let le = min + (i * range) / number_of_buckets;
        let count = hist.count_between(min, min + le);
        buckets.push(Bucket { le, count });
    }
    FilteredMetrics {
        inner: Some(filtered_metrics::Inner::Histogram(FilteredHistogram {
            sum,
            count,
            buckets,
        })),
    }
}

/// a map of metric_name -> metric value
#[derive(Debug, Clone)]
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
    fn to_filtered_metrics(
        &self,
        filter_by_names: &Vec<String>,
    ) -> BTreeMap<String, FilteredMetrics> {
        let mut to_filter = self.map.clone();
        if !filter_by_names.is_empty() {
            to_filter.retain(|key, _| filter_by_names.contains(key));
        }

        let mut filtered_metrics = BTreeMap::new();
        for (name, metric) in to_filter {
            filtered_metrics.insert(name.to_owned(), metric.to_filtered());
            // convert time metrics to a histogram format, on top of percentiles
            match metric {
                AggregatedMetric::Time(ref hist) => {
                    filtered_metrics.insert(format!("{}_hist", name), filter_histogram(hist));
                }
                _ => {}
            }
        }
        debug!("filtered metrics: {:#?}", filtered_metrics);
        filtered_metrics
    }

    fn metric_names(&self) -> Vec<String> {
        self.map.keys().map(|k| k.to_owned()).collect()
    }
}

/// local equivalent to proto::command::ClusterMetrics
#[derive(Debug)]
pub struct LocalClusterMetrics {
    /// metric_name -> metric value
    cluster: MetricsMap,
    // cluster: BTreeMap<String, AggregatedMetric>,
    backends: Vec<LocalBackendMetrics>,
}

impl LocalClusterMetrics {
    fn receive_metric(
        &mut self,
        metric_name: &str,
        metric: MetricValue,
    ) -> Result<(), MetricError> {
        match self.cluster.map.get_mut(metric_name) {
            Some(existing_metric) => existing_metric.update(metric_name, metric),
            None => {
                let aggregated_metric = AggregatedMetric::new(metric)?;
                self.cluster
                    .map
                    .insert(metric_name.to_owned(), aggregated_metric);
            }
        }
        Ok(())
    }

    fn receive_backend_metric(
        &mut self,
        metric_name: &str,
        backend_id: &str,
        metric: MetricValue,
    ) -> Result<(), MetricError> {
        for backend_metrics in self.backends.iter_mut() {
            if backend_metrics.backend_id == backend_id {
                if let Some(existing_metric) = backend_metrics.metrics.map.get_mut(metric_name) {
                    existing_metric.update(metric_name, metric);
                    return Ok(());
                }
            }
        }

        let mut metrics = MetricsMap::new();
        metrics
            .map
            .insert(metric_name.to_owned(), AggregatedMetric::new(metric)?);
        let backend = LocalBackendMetrics {
            backend_id: backend_id.to_owned(),
            metrics,
        };

        self.backends.push(backend);
        Ok(())
    }

    fn to_filtered_metrics(
        &self,
        metric_names: &Vec<String>,
    ) -> Result<ClusterMetrics, MetricError> {
        let cluster = self.cluster.to_filtered_metrics(metric_names);

        let mut backends: Vec<BackendMetrics> = Vec::new();
        for backend in &self.backends {
            backends.push(backend.to_filtered_metrics(metric_names)?);
        }
        Ok(ClusterMetrics { cluster, backends })
    }

    fn metric_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.cluster.metric_names();

        for backend in &self.backends {
            for name in backend.metrics_names() {
                names.push(name);
            }
        }
        names
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
    /// metric_name -> value
    metrics: MetricsMap,
}

impl LocalBackendMetrics {
    fn to_filtered_metrics(
        &self,
        metric_names: &Vec<String>,
    ) -> Result<BackendMetrics, MetricError> {
        let filtered_backend_metrics = self.metrics.to_filtered_metrics(metric_names);

        Ok(BackendMetrics {
            backend_id: self.backend_id.to_owned(),
            metrics: filtered_backend_metrics,
        })
    }

    fn metrics_names(&self) -> Vec<String> {
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
        let proxy_metrics = self.proxy_metrics.metric_names();

        let mut cluster_metrics_names = Vec::new();

        for (_, cluster_metrics) in self.cluster_metrics.iter() {
            for metric_name in cluster_metrics.metric_names() {
                cluster_metrics_names.push(metric_name.to_owned());
            }
        }
        cluster_metrics_names.sort();
        cluster_metrics_names.dedup();

        Ok(ContentType::AvailableMetrics(AvailableMetrics {
            proxy_metrics,
            cluster_metrics: cluster_metrics_names,
        })
        .into())
    }

    pub fn dump_all_metrics(
        &mut self,
        metric_names: &Vec<String>,
    ) -> Result<WorkerMetrics, MetricError> {
        Ok(WorkerMetrics {
            proxy: self.dump_proxy_metrics(metric_names),
            clusters: self.dump_cluster_metrics(metric_names)?,
        })
    }

    pub fn dump_proxy_metrics(
        &mut self,
        metric_names: &Vec<String>,
    ) -> BTreeMap<String, FilteredMetrics> {
        self.proxy_metrics.to_filtered_metrics(metric_names)
        // filter_aggregated_metrics(&self.proxy_metrics, metric_names)
    }

    pub fn dump_cluster_metrics(
        &mut self,
        metric_names: &Vec<String>,
    ) -> Result<BTreeMap<String, ClusterMetrics>, MetricError> {
        let mut cluster_data = BTreeMap::new();

        for cluster_id in self.cluster_metrics.keys() {
            let cluster_metrics = self.metrics_of_one_cluster(cluster_id, metric_names)?;
            cluster_data.insert(cluster_id.to_owned(), cluster_metrics);
        }

        Ok(cluster_data)
    }

    fn metrics_of_one_cluster(
        &self,
        cluster_id: &str,
        metric_names: &Vec<String>,
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
        metric_names: &Vec<String>,
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
        cluster_ids: &Vec<String>,
        metric_names: &Vec<String>,
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
        backend_ids: &Vec<String>,
        metric_names: &Vec<String>,
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

        let local_cluster_metric =
            self.cluster_metrics
                .entry(cluster_id.to_owned())
                .or_insert(LocalClusterMetrics {
                    cluster: MetricsMap::new(),
                    backends: Vec::new(),
                });

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

        let local_cluster_metric =
            self.cluster_metrics
                .entry(cluster_id.to_owned())
                .or_insert(LocalClusterMetrics {
                    cluster: MetricsMap::new(),
                    backends: Vec::new(),
                });

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
            error!("Could not receive metric: {}", e.to_string());
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
                    &["connections_per_backend".to_string()].to_vec(),
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
            .metrics_of_one_cluster("test-cluster", &["http_errors".to_string()].to_vec())
            .expect("could not query metrics for this cluster");

        assert_eq!(expected_cluster_metrics, returned_cluster_metrics);
    }
}
