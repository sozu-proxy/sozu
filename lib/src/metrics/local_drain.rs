#![allow(dead_code)]
use std::{collections::BTreeMap, str, time::Instant};

use hdrhistogram::Histogram;

use crate::sozu_command::proxy::{
    ClusterMetricsData, FilteredData, MetricsConfiguration, Percentiles, QueryAnswerMetrics,
    QueryMetricsType, WorkerMetrics,
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
    fn new(metric: MetricData) -> AggregatedMetric {
        match metric {
            MetricData::Gauge(value) => AggregatedMetric::Gauge(value),
            MetricData::GaugeAdd(value) => AggregatedMetric::Gauge(value as usize),
            MetricData::Count(value) => AggregatedMetric::Count(value),
            MetricData::Time(value) => {
                //FIXME: do not expect here. We'll need error management all the way up.
                // this will compile but may panic. DO NOT SHIP THIS INTO PRODUCTION
                let mut histogram = ::hdrhistogram::Histogram::new(3)
                    .expect("could not create histogram for a time metric");
                if let Err(e) = histogram.record(value as u64) {
                    error!("could not record time metric: {:?}", e);
                }
                AggregatedMetric::Time(histogram)
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
                    error!("could not record time metric: {:?}", e);
                }
            }
            (s, m) => panic!(
                "tried to update metric {} of value {:?} with an incompatible metric: {:?}",
                key, s, m
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

    pub fn query(&mut self, query_type: &QueryMetricsType) -> QueryAnswerMetrics {
        trace!(
            "The local drain received this query: {:?}\n, here is the local drain: {:?}",
            query_type,
            self
        );
        match query_type {
            QueryMetricsType::List => self.list_all_metric_names(),
            QueryMetricsType::Cluster {
                metrics,
                cluster_ids,
                date, // to be removed
            } => self.query_clusters(metrics, cluster_ids),
            QueryMetricsType::Backend {
                metrics,
                backend_ids,
                date, // to be removed
            } => self.query_backends(metrics, backend_ids),
            QueryMetricsType::All => self.dump_all_metrics(),
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

    pub fn dump_all_metrics(&mut self) -> QueryAnswerMetrics {
        QueryAnswerMetrics::All(WorkerMetrics {
            proxy: Some(self.dump_proxy_metrics()),
            clusters: Some(self.dump_cluster_metrics()),
        })
    }

    pub fn dump_proxy_metrics(&mut self) -> BTreeMap<String, FilteredData> {
        self.proxy_metrics
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_filtered()))
            .collect()
    }

    pub fn dump_cluster_metrics(&mut self) -> BTreeMap<String, ClusterMetricsData> {
        let mut cluster_data = BTreeMap::new();

        for cluster_id in self.get_cluster_ids() {
            cluster_data.insert(
                cluster_id.to_owned(),
                self.metrics_of_one_cluster(&cluster_id),
            );
        }

        cluster_data
    }

    // TODO: filter with metric names
    fn metrics_of_one_cluster(&self, cluster_id: &str) -> ClusterMetricsData {
        let cluster: BTreeMap<String, FilteredData> = match self.cluster_metrics.get(cluster_id) {
            Some(cluster_metrics) => cluster_metrics
                .iter()
                .map(|entry| (entry.0.to_owned(), entry.1.to_filtered()))
                .collect::<BTreeMap<String, FilteredData>>(),
            None => BTreeMap::new(), // this is unlikely, but we should propagate an error here
        };

        let mut backends = BTreeMap::new();
        for backend_id in self.get_backend_ids(&cluster_id) {
            backends.insert(
                backend_id.to_owned(),
                self.metrics_of_one_backend(&backend_id),
            );
        }
        ClusterMetricsData {
            cluster: Some(cluster),
            backends: Some(backends),
        }
    }

    // TODO: filter with metric names
    fn metrics_of_one_backend(&self, backend_id: &str) -> BTreeMap<String, FilteredData> {
        match self.cluster_metrics.get(backend_id) {
            Some(backend_metrics) => backend_metrics
                .iter()
                .map(|entry| (entry.0.to_owned(), entry.1.to_filtered()))
                .collect::<BTreeMap<String, FilteredData>>(),

            None => return BTreeMap::new(), // unlikely, but we should propagate an error here
        }
    }

    fn query_clusters(
        &mut self,
        metric_names: &Vec<String>, // TODO: filter with metric names
        cluster_ids: &Vec<String>,
    ) -> QueryAnswerMetrics {
        debug!("Querying cluster with ids: {:?}", cluster_ids);
        let mut clusters: BTreeMap<String, ClusterMetricsData> = BTreeMap::new();

        // TODO if metric_names is Some(names), filter. If empty, provide all metrics
        for cluster_id in cluster_ids {
            clusters.insert(
                cluster_id.to_owned(),
                self.metrics_of_one_cluster(cluster_id),
            );
        }

        trace!("query result: {:#?}", clusters);
        QueryAnswerMetrics::All(WorkerMetrics {
            proxy: None,
            clusters: Some(clusters),
        })
    }

    fn query_backends(
        &mut self,
        metric_names: &[String], // TODO: filter with metric names
        backend_ids: &[String],
    ) -> QueryAnswerMetrics {
        let mut clusters: BTreeMap<String, ClusterMetricsData> = BTreeMap::new();

        // if metric_names is empty, provide all metrics for those backends
        for backend_id in backend_ids {
            let cluster_id = match self.backend_to_cluster.get(backend_id) {
                Some(id) => id.to_owned(),
                None => continue,
            };

            let mut backend_map: BTreeMap<String, BTreeMap<String, FilteredData>> = BTreeMap::new();
            backend_map.insert(
                backend_id.to_owned(),
                self.metrics_of_one_backend(&backend_id),
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
        QueryAnswerMetrics::All(WorkerMetrics {
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

        // TODO: make this trace! again
        debug!(
            "cluster metric: {} {} {:?} {:?}",
            metric_name, cluster_id, backend_id, metric_value
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
                submap.insert(metric_name.to_owned(), AggregatedMetric::new(metric_value));
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
        debug!(
            "receiving metric with key {}, cluster_id: {:?}, backend_id: {:?}, metric data: {:?}",
            key, cluster_id, backend_id, metric
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
                self.proxy_metrics
                    .insert(String::from(key), AggregatedMetric::new(metric));
            }
        }
    }
}
