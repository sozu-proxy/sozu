use std::collections::BTreeMap;

use command::{
    AggregatedMetrics, BackendMetrics, Bucket, FilteredHistogram, FilteredMetrics, Percentiles,
    filtered_metrics::Inner,
};
use prost::UnknownEnumValue;

/// Contains all types received by and sent from Sōzu
pub mod command;

/// Implementation of fmt::Display for the protobuf types, used in the CLI
pub mod display;

#[derive(thiserror::Error, Debug)]
pub enum DisplayError {
    #[error("Could not display content")]
    DisplayContent(String),
    #[error("Error while parsing response to JSON")]
    Json(serde_json::Error),
    #[error("got the wrong response content type: {0}")]
    WrongResponseType(String),
    #[error("Could not format the datetime to ISO 8601")]
    DateTime,
    #[error("unrecognized protobuf variant: {0}")]
    DecodeError(UnknownEnumValue),
}

// Simple helper to build ResponseContent from ContentType
impl From<command::response_content::ContentType> for command::ResponseContent {
    fn from(value: command::response_content::ContentType) -> Self {
        Self {
            content_type: Some(value),
        }
    }
}

// Simple helper to build Request from RequestType
impl From<command::request::RequestType> for command::Request {
    fn from(value: command::request::RequestType) -> Self {
        Self {
            request_type: Some(value),
        }
    }
}

impl AggregatedMetrics {
    /// Merge metrics that were received from several workers
    ///
    /// Each worker gather the same kind of metrics,
    /// for its own proxying logic, and for the same clusters with their backends.
    /// This means we have to reduce each metric from N instances to 1.
    pub fn merge_metrics(&mut self) {
        // avoid copying the worker metrics, by taking them
        let workers = std::mem::take(&mut self.workers);

        for (_worker_id, worker) in workers {
            for (metric_name, new_value) in worker.proxy {
                if new_value.is_mergeable() {
                    self.proxying
                        .entry(metric_name)
                        .and_modify(|old_value| old_value.merge(&new_value))
                        .or_insert(new_value);
                }
            }

            for (cluster_id, mut cluster_metrics) in worker.clusters {
                for (metric_name, new_value) in cluster_metrics.cluster {
                    if new_value.is_mergeable() {
                        let cluster = self.clusters.entry(cluster_id.to_owned()).or_default();

                        cluster
                            .cluster
                            .entry(metric_name)
                            .and_modify(|old_value| old_value.merge(&new_value))
                            .or_insert(new_value);
                    }
                }

                for backend in cluster_metrics.backends.drain(..) {
                    for (metric_name, new_value) in backend.metrics {
                        if new_value.is_mergeable() {
                            let cluster = self.clusters.entry(cluster_id.to_owned()).or_default();

                            let found_backend = cluster
                                .backends
                                .iter_mut()
                                .find(|present| present.backend_id == backend.backend_id);

                            if let Some(existing_backend) = found_backend {
                                let _ = existing_backend
                                    .metrics
                                    .entry(metric_name)
                                    .and_modify(|old_value| old_value.merge(&new_value))
                                    .or_insert(new_value);
                            } else {
                                cluster.backends.push(BackendMetrics {
                                    backend_id: backend.backend_id.clone(),
                                    metrics: BTreeMap::from([(metric_name, new_value)]),
                                });
                            };
                        }
                    }
                }
            }
        }
    }
}

impl FilteredMetrics {
    pub fn merge(&mut self, right: &Self) {
        match (&self.inner, &right.inner) {
            (Some(Inner::Gauge(a)), Some(Inner::Gauge(b))) => {
                *self = Self {
                    inner: Some(Inner::Gauge(a + b)),
                };
            }
            (Some(Inner::Count(a)), Some(Inner::Count(b))) => {
                *self = Self {
                    inner: Some(Inner::Count(a + b)),
                };
            }
            (Some(Inner::Histogram(a)), Some(Inner::Histogram(b))) => {
                let longest_len = a.buckets.len().max(b.buckets.len());

                let mut a_count = 0;
                let mut b_count = 0;
                let buckets = (0..longest_len)
                    .map(|i| {
                        if let Some(a_bucket) = a.buckets.get(i) {
                            a_count = a_bucket.count;
                        }
                        if let Some(b_bucket) = b.buckets.get(i) {
                            b_count = b_bucket.count;
                        }
                        Bucket {
                            le: (1 << i) - 1, // the bucket less-or-equal limits are normalized: 0, 1, 3, 7, 15, ...
                            count: a_count + b_count,
                        }
                    })
                    .collect();

                *self = Self {
                    inner: Some(Inner::Histogram(FilteredHistogram {
                        count: a.count + b.count,
                        sum: a.sum + b.sum,
                        buckets,
                    })),
                };
            }
            (Some(Inner::Percentiles(a)), Some(Inner::Percentiles(b))) => {
                // You cannot statistically merge two percentile summaries
                // without the underlying samples. The companion
                // `<name>_histogram` Inner::Histogram value is the source
                // of truth for accurate aggregation and merges correctly
                // above. We still propagate the percentile shape so legacy
                // consumers reading it observe at least the worst-case
                // upper bound across workers — element-wise max preserves
                // the "is anyone slow?" intent. `samples` and `sum` add so
                // the totals reflect cross-worker volume.
                *self = Self {
                    inner: Some(Inner::Percentiles(Percentiles {
                        samples: a.samples + b.samples,
                        p_50: a.p_50.max(b.p_50),
                        p_90: a.p_90.max(b.p_90),
                        p_99: a.p_99.max(b.p_99),
                        p_99_9: a.p_99_9.max(b.p_99_9),
                        p_99_99: a.p_99_99.max(b.p_99_99),
                        p_99_999: a.p_99_999.max(b.p_99_999),
                        p_100: a.p_100.max(b.p_100),
                        sum: a.sum + b.sum,
                    })),
                };
            }
            _ => {}
        }
    }

    fn is_mergeable(&self) -> bool {
        match &self.inner {
            Some(Inner::Gauge(_))
            | Some(Inner::Count(_))
            | Some(Inner::Histogram(_))
            | Some(Inner::Percentiles(_)) => true,
            // Inner::Time and Inner::Timeserie are never used in Sōzu
            Some(Inner::Time(_)) | Some(Inner::TimeSerie(_)) | None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::AggregatedMetrics;
    use super::command::{
        Bucket, ClusterMetrics, FilteredHistogram, FilteredMetrics, Percentiles, WorkerMetrics,
        filtered_metrics::Inner,
    };

    #[test]
    fn merge_relocates_single_worker_to_top_level() {
        // Regression: a one-worker fleet must populate `clusters` and
        // `proxying` so CLI/TUI consumers reading those maps see the
        // worker's data. `std::mem::take(&mut self.workers)` empties the
        // per-worker map after relocation, which is the documented
        // contract when the caller asked for the merged shape.
        let mut worker = WorkerMetrics {
            proxy: BTreeMap::new(),
            clusters: BTreeMap::new(),
        };
        worker.proxy.insert(
            "requests".to_owned(),
            FilteredMetrics {
                inner: Some(Inner::Count(42)),
            },
        );
        let mut cluster = ClusterMetrics {
            cluster: BTreeMap::new(),
            backends: Vec::new(),
        };
        cluster.cluster.insert(
            "requests".to_owned(),
            FilteredMetrics {
                inner: Some(Inner::Count(7)),
            },
        );
        worker.clusters.insert("cluster-a".to_owned(), cluster);

        let mut agg = AggregatedMetrics {
            main: BTreeMap::new(),
            workers: BTreeMap::from([("0".to_owned(), worker)]),
            clusters: BTreeMap::new(),
            proxying: BTreeMap::new(),
        };

        agg.merge_metrics();

        assert!(
            agg.workers.is_empty(),
            "merge takes ownership of the per-worker map"
        );
        assert_eq!(
            agg.proxying.get("requests"),
            Some(&FilteredMetrics {
                inner: Some(Inner::Count(42)),
            }),
            "single worker's proxy counter must surface in proxying"
        );
        let cluster_a = agg
            .clusters
            .get("cluster-a")
            .expect("cluster row must surface in top-level clusters");
        assert_eq!(
            cluster_a.cluster.get("requests"),
            Some(&FilteredMetrics {
                inner: Some(Inner::Count(7)),
            })
        );
    }

    #[test]
    fn merge_counts_and_gauges() {
        let mut gauge_a = FilteredMetrics {
            inner: Some(Inner::Gauge(4)),
        };
        let gauge_b = FilteredMetrics {
            inner: Some(Inner::Gauge(4)),
        };

        gauge_a.merge(&gauge_b);

        assert_eq!(
            gauge_a,
            FilteredMetrics {
                inner: Some(Inner::Gauge(8)),
            }
        );

        let mut count_a = FilteredMetrics {
            inner: Some(Inner::Count(3)),
        };
        let count_b = FilteredMetrics {
            inner: Some(Inner::Count(3)),
        };

        count_a.merge(&count_b);

        assert_eq!(
            count_a,
            FilteredMetrics {
                inner: Some(Inner::Count(6)),
            }
        );
    }

    #[test]
    fn merge_percentiles_takes_max_per_quantile() {
        // Multi-worker percentile aggregation propagates the worst-case
        // quantile across workers and accumulates samples + sum so the
        // surfaced summary remains the "is anyone slow?" upper bound.
        let mut left = FilteredMetrics {
            inner: Some(Inner::Percentiles(Percentiles {
                samples: 100,
                p_50: 5,
                p_90: 20,
                p_99: 100,
                p_99_9: 200,
                p_99_99: 250,
                p_99_999: 300,
                p_100: 400,
                sum: 12_000,
            })),
        };
        let right = FilteredMetrics {
            inner: Some(Inner::Percentiles(Percentiles {
                samples: 50,
                p_50: 7,
                p_90: 15,
                p_99: 80,
                p_99_9: 240,
                p_99_99: 245,
                p_99_999: 290,
                p_100: 380,
                sum: 6_000,
            })),
        };
        left.merge(&right);
        assert_eq!(
            left,
            FilteredMetrics {
                inner: Some(Inner::Percentiles(Percentiles {
                    samples: 150,
                    p_50: 7,
                    p_90: 20,
                    p_99: 100,
                    p_99_9: 240,
                    p_99_99: 250,
                    p_99_999: 300,
                    p_100: 400,
                    sum: 18_000,
                })),
            }
        );
    }

    #[test]
    fn merge_histograms() {
        let mut histogram_a = FilteredMetrics {
            inner: Some(Inner::Histogram(FilteredHistogram {
                sum: 95,
                count: 30,
                buckets: vec![
                    Bucket { le: 0, count: 1 },
                    Bucket { le: 1, count: 2 },
                    Bucket { le: 3, count: 10 },
                    Bucket { le: 7, count: 25 },
                    Bucket { le: 15, count: 27 },
                    Bucket { le: 31, count: 30 },
                ],
            })),
        };

        let histogram_b = FilteredMetrics {
            inner: Some(Inner::Histogram(FilteredHistogram {
                sum: 82,
                count: 40,
                buckets: vec![
                    Bucket { le: 0, count: 0 },
                    Bucket { le: 1, count: 0 },
                    Bucket { le: 3, count: 12 },
                    Bucket { le: 7, count: 30 },
                    Bucket { le: 15, count: 40 },
                    // note: there is no bucket for "le: 31"
                ],
            })),
        };

        histogram_a.merge(&histogram_b);

        let merged_histogram = FilteredMetrics {
            inner: Some(Inner::Histogram(FilteredHistogram {
                sum: 177,
                count: 70,
                buckets: vec![
                    Bucket { le: 0, count: 1 },
                    Bucket { le: 1, count: 2 },
                    Bucket { le: 3, count: 22 },
                    Bucket { le: 7, count: 55 },
                    Bucket { le: 15, count: 67 },
                    Bucket { le: 31, count: 70 }, // note: the total count of histogram b is added, even though histogram b has no bucket
                ],
            })),
        };

        assert_eq!(histogram_a, merged_histogram);
    }
}
