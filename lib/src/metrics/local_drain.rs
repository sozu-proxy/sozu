#![allow(dead_code)]
use std::{collections::BTreeMap, str, time::Instant};

use anyhow::Context;
use hdrhistogram::Histogram;

use crate::sozu_command::{
    request::{MetricsConfiguration, QueryMetricsOptions},
    response::{ClusterMetricsData, FilteredData, Percentiles, QueryAnswerMetrics, WorkerMetrics},
};

use super::{MetricData, Subscriber};

/// This is how the metrics are stored in the local drain
#[derive(Debug, Clone)]
pub enum AggregatedMetric {
    Gauge(usize),
    Count(i64),
    Time(Histogram<u32>),
}

impl AggregatedMetric {
    fn new(metric: MetricData) -> anyhow::Result<AggregatedMetric> {
        match metric {
            MetricData::Gauge(value) => Ok(AggregatedMetric::Gauge(value)),
            MetricData::GaugeAdd(value) => Ok(AggregatedMetric::Gauge(value as usize)),
            MetricData::Count(value) => Ok(AggregatedMetric::Count(value)),
            MetricData::Time(value) => {
                let mut histogram = ::hdrhistogram::Histogram::new(3).context(format!(
                    "Could not create histogram for time metric {metric:?}"
                ))?;

                histogram
                    .record(value as u64)
                    .context(format!("could not record time metric: {metric:?}"))?;

                Ok(AggregatedMetric::Time(histogram))
            }
        }
    }

    fn update(&mut self, key: &str, m: MetricData) {
        match (self, m) {
            (&mut AggregatedMetric::Gauge(ref mut v1), MetricData::Gauge(v2)) => {
                *v1 = v2;
            }
            (&mut AggregatedMetric::Gauge(ref mut v1), MetricData::GaugeAdd(v2)) => {
                *v1 = (*v1 as i64 + v2) as usize;
            }
            (&mut AggregatedMetric::Count(ref mut v1), MetricData::Count(v2)) => {
                *v1 += v2;
            }
            (&mut AggregatedMetric::Time(ref mut v1), MetricData::Time(v2)) => {
                if let Err(e) = (*v1).record(v2 as u64) {
                    error!("could not record time metric: {:?}", e.to_string());
                }
            }
            (s, m) => panic!(
                "tried to update metric {key} of value {s:?} with an incompatible metric: {m:?}"
            ),
        }
    }

    pub fn to_filtered(&self) -> FilteredData {
        match *self {
            AggregatedMetric::Gauge(i) => FilteredData::Gauge(i),
            AggregatedMetric::Count(i) => FilteredData::Count(i),
            AggregatedMetric::Time(ref hist) => {
                FilteredData::Percentiles(histogram_to_percentiles(hist))
            }
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

#[derive(Clone, Debug)]
pub struct BackendMetrics {
    pub cluster_id: String,
    pub data: BTreeMap<String, AggregatedMetric>,
}

#[derive(Copy, Clone, Debug, PartialEq)]
enum MetricKind {
    Gauge,
    Count,
    Time,
}

#[derive(Clone, Debug, PartialEq)]
enum MetricMeta {
    Cluster,
    ClusterBackend,
}

/// This gathers metrics locally, to be queried by the CLI
#[derive(Debug)]
pub struct LocalDrain {
    /// a prefix to metric keys, usually "sozu-"
    pub prefix: String,
    pub created: Instant,
    /// metric_name -> metric value
    pub proxy_metrics: BTreeMap<String, AggregatedMetric>,
    /// backend_id -> cluster_id
    backend_to_cluster: BTreeMap<String, String>,
    /// BTreeMap<cluster_id OR backend_id, BTreeMap<metric_name, AggregatedMetric>>
    /// if the key is a backend_id, we'll retrieve the cluster_id in the backend_to_cluster map
    cluster_metrics: BTreeMap<String, BTreeMap<String, AggregatedMetric>>,
    use_tagged_metrics: bool,
    origin: String,
    enabled: bool,
}

impl LocalDrain {
    pub fn new(prefix: String) -> Self {
        LocalDrain {
            prefix,
            created: Instant::now(),
            proxy_metrics: BTreeMap::new(),
            cluster_metrics: BTreeMap::new(),
            backend_to_cluster: BTreeMap::new(),
            use_tagged_metrics: false,
            origin: String::from("x"),
            enabled: false,
        }
    }

    pub fn configure(&mut self, config: &MetricsConfiguration) {
        match config {
            MetricsConfiguration::Enabled(enabled) => {
                self.enabled = *enabled;
            }
            MetricsConfiguration::Clear => self.clear(),
        }
    }

    pub fn clear(&mut self) {
        self.backend_to_cluster.clear();
        self.cluster_metrics.clear();
    }

    fn get_cluster_ids(&self) -> Vec<String> {
        self.cluster_metrics
            .iter()
            .filter_map(
                |entry| match self.backend_to_cluster.contains_key(entry.0) {
                    true => Some(entry.0.to_owned()),
                    false => None,
                },
            )
            .collect()
    }

    fn get_backend_ids(&self, cluster_id: &str) -> Vec<String> {
        self.backend_to_cluster
            .iter()
            .filter_map(|backend_to_cluster| {
                if backend_to_cluster.1 == cluster_id {
                    Some(backend_to_cluster.0.to_owned())
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn query(&mut self, options: &QueryMetricsOptions) -> QueryAnswerMetrics {
        trace!(
            "The local drain received a metrics query with this options: {:?}",
            options
        );

        let QueryMetricsOptions {
            metric_names,
            cluster_ids,
            backend_ids,
            list,
        } = options;

        if *list {
            return self.list_all_metric_names();
        }

        let worker_metrics = match (cluster_ids.is_empty(), backend_ids.is_empty()) {
            (false, _) => self.query_clusters(cluster_ids, metric_names),
            (true, false) => self.query_backends(backend_ids, metric_names),
            (true, true) => self.dump_all_metrics(metric_names),
        };

        match worker_metrics {
            Ok(worker_metrics) => QueryAnswerMetrics::All(worker_metrics),
            Err(e) => QueryAnswerMetrics::Error(e.to_string()),
        }
    }

    fn list_all_metric_names(&self) -> QueryAnswerMetrics {
        let proxy_metrics_names = self.proxy_metrics.keys().cloned().collect();

        let mut cluster_metrics_names = Vec::new();

        for cluster_metrics_entry in self.cluster_metrics.iter() {
            for (metric_name, _) in cluster_metrics_entry.1.iter() {
                cluster_metrics_names.push(metric_name.to_owned());
            }
        }
        QueryAnswerMetrics::List((proxy_metrics_names, cluster_metrics_names))
    }

    pub fn dump_all_metrics(
        &mut self,
        metric_names: &Vec<String>,
    ) -> anyhow::Result<WorkerMetrics> {
        Ok(WorkerMetrics {
            proxy: Some(self.dump_proxy_metrics(metric_names)),
            clusters: Some(self.dump_cluster_metrics(metric_names)?),
        })
    }

    pub fn dump_proxy_metrics(
        &mut self,
        metric_names: &Vec<String>,
    ) -> BTreeMap<String, FilteredData> {
        self.proxy_metrics
            .iter()
            .filter(|(key, _)| {
                if metric_names.is_empty() {
                    true
                } else {
                    metric_names.contains(key)
                }
            })
            .map(|(key, value)| (key.to_string(), value.to_filtered()))
            .collect()
    }

    pub fn dump_cluster_metrics(
        &mut self,
        metric_names: &Vec<String>,
    ) -> anyhow::Result<BTreeMap<String, ClusterMetricsData>> {
        let mut cluster_data = BTreeMap::new();

        for cluster_id in self.get_cluster_ids() {
            let cluster_metrics = self
                .metrics_of_one_cluster(&cluster_id, metric_names)
                .context(format!(
                    "Could not retrieve metrics of cluster {cluster_id}"
                ))?;
            cluster_data.insert(cluster_id.to_owned(), cluster_metrics);
        }

        Ok(cluster_data)
    }

    fn metrics_of_one_cluster(
        &self,
        cluster_id: &str,
        metric_names: &Vec<String>,
    ) -> anyhow::Result<ClusterMetricsData> {
        let raw_metrics = self
            .cluster_metrics
            .get(cluster_id)
            .context(format!("No metrics found for cluster with id {cluster_id}"))?;

        let cluster: BTreeMap<String, FilteredData> = raw_metrics
            .iter()
            .filter(|entry| {
                if metric_names.is_empty() {
                    true
                } else {
                    metric_names.contains(entry.0)
                }
            })
            .map(|entry| (entry.0.to_owned(), entry.1.to_filtered()))
            .collect::<BTreeMap<String, FilteredData>>();

        let mut backends = BTreeMap::new();
        for backend_id in self.get_backend_ids(cluster_id) {
            let backend_metrics = self
                .metrics_of_one_backend(&backend_id, metric_names)
                .context(format!(
                    "Could not retrieve metrics for backend {backend_id} of cluster {cluster_id}"
                ))?;
            backends.insert(backend_id.to_owned(), backend_metrics);
        }
        Ok(ClusterMetricsData {
            cluster: Some(cluster),
            backends: Some(backends),
        })
    }

    fn metrics_of_one_backend(
        &self,
        backend_id: &str,
        metric_names: &Vec<String>,
    ) -> anyhow::Result<BTreeMap<String, FilteredData>> {
        let backend_metrics = self
            .cluster_metrics
            .get(backend_id)
            .context(format!("No metrics found for backend with id {backend_id}"))?;

        let filtered_backend_metrics = backend_metrics
            .iter()
            .filter(|entry| {
                if metric_names.is_empty() {
                    true
                } else {
                    metric_names.contains(entry.0)
                }
            })
            .map(|entry| (entry.0.to_owned(), entry.1.to_filtered()))
            .collect::<BTreeMap<String, FilteredData>>();

        Ok(filtered_backend_metrics)
    }

    fn query_clusters(
        &mut self,
        cluster_ids: &Vec<String>,
        metric_names: &Vec<String>,
    ) -> anyhow::Result<WorkerMetrics> {
        debug!("Querying cluster with ids: {:?}", cluster_ids);
        let mut clusters: BTreeMap<String, ClusterMetricsData> = BTreeMap::new();

        for cluster_id in cluster_ids {
            clusters.insert(
                cluster_id.to_owned(),
                self.metrics_of_one_cluster(cluster_id, metric_names)?,
            );
        }

        trace!("query result: {:#?}", clusters);
        Ok(WorkerMetrics {
            proxy: None,
            clusters: Some(clusters),
        })
    }

    fn query_backends(
        &mut self,
        backend_ids: &Vec<String>,
        metric_names: &Vec<String>,
    ) -> anyhow::Result<WorkerMetrics> {
        let mut clusters: BTreeMap<String, ClusterMetricsData> = BTreeMap::new();

        for backend_id in backend_ids {
            let cluster_id = self
                .backend_to_cluster
                .get(backend_id)
                .context(format!("No metrics found for backend with id {backend_id}"))?
                .to_owned();

            let mut backend_map: BTreeMap<String, BTreeMap<String, FilteredData>> = BTreeMap::new();
            backend_map.insert(
                backend_id.to_owned(),
                self.metrics_of_one_backend(backend_id, metric_names)?,
            );

            clusters.insert(
                cluster_id,
                ClusterMetricsData {
                    cluster: None,
                    backends: Some(backend_map),
                },
            );
        }

        trace!("query result: {:#?}", clusters);
        Ok(WorkerMetrics {
            proxy: None,
            clusters: Some(clusters),
        })
    }

    fn receive_cluster_metric(
        &mut self,
        metric_name: &str,
        cluster_id: &str,
        backend_id: Option<&str>,
        metric_value: MetricData,
    ) {
        if !self.enabled {
            return;
        }

        trace!(
            "cluster metric: {} {} {:?} {:?}",
            metric_name,
            cluster_id,
            backend_id,
            metric_value
        );

        let cluster_or_backend_id = match backend_id {
            Some(backend_id) => {
                self.backend_to_cluster
                    .insert(backend_id.to_owned(), cluster_id.to_owned());
                backend_id
            }
            None => cluster_id,
        };

        let submap = self
            .cluster_metrics
            .entry(cluster_or_backend_id.to_owned())
            .or_insert(BTreeMap::new());

        match submap.get_mut(metric_name) {
            Some(existing_metric) => existing_metric.update(metric_name, metric_value),
            None => {
                let aggregated_metric = match AggregatedMetric::new(metric_value) {
                    Ok(m) => m,
                    Err(e) => {
                        return error!("Could not aggregate metric: {}", e.to_string());
                    }
                };
                submap.insert(metric_name.to_owned(), aggregated_metric);
            }
        }
    }
}

impl Subscriber for LocalDrain {
    fn receive_metric(
        &mut self,
        key: &'static str,
        cluster_id: Option<&str>,
        backend_id: Option<&str>,
        metric: MetricData,
    ) {
        trace!(
            "receiving metric with key {}, cluster_id: {:?}, backend_id: {:?}, metric data: {:?}",
            key,
            cluster_id,
            backend_id,
            metric
        );

        // cluster metrics
        if let Some(id) = cluster_id {
            self.receive_cluster_metric(key, id, backend_id, metric);
            return;
        }

        // proxy metrics
        match self.proxy_metrics.get_mut(key) {
            Some(stored_metric) => stored_metric.update(key, metric),
            None => {
                let aggregated_metric = match AggregatedMetric::new(metric) {
                    Ok(m) => m,
                    Err(e) => {
                        return error!("Could not aggregate metric: {}", e.to_string());
                    }
                };

                self.proxy_metrics
                    .insert(String::from(key), aggregated_metric);
            }
        }
    }
}
