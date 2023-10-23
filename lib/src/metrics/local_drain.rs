#![allow(dead_code)]
use std::{collections::BTreeMap, str, time::Instant};

use hdrhistogram::Histogram;

use sozu_command::proto::command::{
    filtered_metrics, response_content::ContentType, AvailableMetrics, BackendMetrics,
    ClusterMetrics, FilteredMetrics, MetricsConfiguration, Percentiles, QueryMetricsOptions,
    ResponseContent, WorkerMetrics,
};

use crate::metrics::{MetricError, MetricValue, Subscriber};

/// This is how the metrics are stored in the local drain
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
    disable_cluster_metrics: bool,
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
        } = options;

        if *list {
            return self.list_all_metric_names();
        }

        let worker_metrics = match (cluster_ids.is_empty(), backend_ids.is_empty()) {
            (false, _) => self.query_clusters(cluster_ids, metric_names)?,
            (true, false) => self.query_backends(backend_ids, metric_names)?,
            (true, true) => self.dump_all_metrics(metric_names)?,
        };

        Ok(ContentType::WorkerMetrics(worker_metrics).into())
    }

    fn list_all_metric_names(&self) -> Result<ResponseContent, MetricError> {
        let proxy_metrics = self.proxy_metrics.keys().cloned().collect();

        let mut cluster_metrics = Vec::new();

        for cluster_metrics_entry in self.cluster_metrics.iter() {
            for (metric_name, _) in cluster_metrics_entry.1.iter() {
                cluster_metrics.push(metric_name.to_owned());
            }
        }
        Ok(ContentType::AvailableMetrics(AvailableMetrics {
            proxy_metrics,
            cluster_metrics,
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
    ) -> Result<BTreeMap<String, ClusterMetrics>, MetricError> {
        let mut cluster_data = BTreeMap::new();

        for cluster_id in self.get_cluster_ids() {
            let cluster_metrics = self.metrics_of_one_cluster(&cluster_id, metric_names)?;
            cluster_data.insert(cluster_id.to_owned(), cluster_metrics);
        }

        Ok(cluster_data)
    }

    fn metrics_of_one_cluster(
        &self,
        cluster_id: &str,
        metric_names: &Vec<String>,
    ) -> Result<ClusterMetrics, MetricError> {
        let raw_metrics = self
            .cluster_metrics
            .get(cluster_id)
            .ok_or(MetricError::NoMetrics(cluster_id.to_owned()))?;

        let cluster: BTreeMap<String, FilteredMetrics> = raw_metrics
            .iter()
            .filter(|entry| {
                if metric_names.is_empty() {
                    true
                } else {
                    metric_names.contains(entry.0)
                }
            })
            .map(|entry| (entry.0.to_owned(), entry.1.to_filtered()))
            .collect::<BTreeMap<String, FilteredMetrics>>();

        let mut backends = Vec::new();
        for backend_id in self.get_backend_ids(cluster_id) {
            let backend_metrics = self.metrics_of_one_backend(&backend_id, metric_names)?;
            backends.push(backend_metrics);
        }
        Ok(ClusterMetrics { cluster, backends })
    }

    fn metrics_of_one_backend(
        &self,
        backend_id: &str,
        metric_names: &Vec<String>,
    ) -> Result<BackendMetrics, MetricError> {
        let backend_metrics = self
            .cluster_metrics
            .get(backend_id)
            .ok_or(MetricError::NoMetrics(backend_id.to_owned()))?;

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
            .collect::<BTreeMap<String, FilteredMetrics>>();

        Ok(BackendMetrics {
            backend_id: backend_id.to_owned(),
            metrics: filtered_backend_metrics,
        })
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
            let cluster_id = self
                .backend_to_cluster
                .get(backend_id)
                .ok_or(MetricError::NoMetrics(backend_id.to_owned()))?
                .to_owned();

            let backends = vec![self.metrics_of_one_backend(backend_id, metric_names)?];

            clusters.insert(
                cluster_id,
                ClusterMetrics {
                    cluster: BTreeMap::new(),
                    backends,
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
        backend_id: Option<&str>,
        metric_value: MetricValue,
    ) {
        if self.disable_cluster_metrics {
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
        metric: MetricValue,
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
