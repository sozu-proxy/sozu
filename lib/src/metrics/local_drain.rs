use std::str;
use std::time::Instant;
use std::collections::BTreeMap;
use hdrhistogram::Histogram;
use sozu_command::proxy::{FilteredData,MetricsData,Percentiles,AppMetricsData};

use super::{MetricData,Subscriber};

#[derive(Debug,Clone)]
pub enum AggregatedMetric {
  Gauge(usize),
  Count(i64),
  Time(Histogram<u32>)
}

impl AggregatedMetric {
  fn new(metric: MetricData) -> AggregatedMetric {
    match metric {
      MetricData::Gauge(value) => AggregatedMetric::Gauge(value),
      MetricData::GaugeAdd(value) => AggregatedMetric::Gauge(value as usize),
      MetricData::Count(value) => AggregatedMetric::Count(value),
      MetricData::Time(value)  => {
        //FIXME: do not unwrap here
        let mut h = ::hdrhistogram::Histogram::new(3).unwrap();
        if let Err(e) = h.record(value as u64) {
          error!("could not create histogram with time metric {:?}: {:?}", value, e);
        }
        AggregatedMetric::Time(h)
      }
    }
  }

  fn update(&mut self, key: &'static str, m: MetricData) {
    match (self, m) {
      (&mut AggregatedMetric::Gauge(ref mut v1), MetricData::Gauge(v2)) => {
        *v1 = v2;
      },
      (&mut AggregatedMetric::Gauge(ref mut v1), MetricData::GaugeAdd(v2)) => {
        *v1 = (*v1 as i64 + v2) as usize;
      },
      (&mut AggregatedMetric::Count(ref mut v1), MetricData::Count(v2)) => {
        *v1 += v2;
      },
      (&mut AggregatedMetric::Time(ref mut v1), MetricData::Time(v2)) => {
        if let Err(e) = (*v1).record(v2 as u64) {
          error!("could not add time metric {}={:?} to histogram: {:?}", key, v2, e);
        }
      },
      (s,m) => panic!("tried to update metric {} of value {:?} with an incompatible metric: {:?}", key, s, m)
    }
  }
}

pub fn histogram_to_percentiles(hist: &Histogram<u32>) -> Percentiles {
  Percentiles {
    samples:  hist.len(),
    p_50:     hist.value_at_percentile(50.0),
    p_90:     hist.value_at_percentile(90.0),
    p_99:     hist.value_at_percentile(99.0),
    p_99_9:   hist.value_at_percentile(99.9),
    p_99_99:  hist.value_at_percentile(99.99),
    p_99_999: hist.value_at_percentile(99.999),
    p_100:    hist.value_at_percentile(100.0),
  }
}

pub fn aggregated_to_filtered(value: &AggregatedMetric) -> FilteredData {
  match value {
    &AggregatedMetric::Gauge(i) => FilteredData::Gauge(i),
    &AggregatedMetric::Count(i) => FilteredData::Count(i),
    &AggregatedMetric::Time(ref hist) => {
      FilteredData::Percentiles(histogram_to_percentiles(&hist))
    },
  }
}

#[derive(Clone,Debug)]
pub struct AppMetrics {
  pub data: BTreeMap<String, AggregatedMetric>,
  pub backend_data: BTreeMap<String, BTreeMap<String, AggregatedMetric>>,
}

#[derive(Clone,Debug)]
pub struct BackendMetrics {
  pub cluster_id: String,
  pub data:   BTreeMap<String, AggregatedMetric>,
}

#[derive(Debug)]
pub struct LocalDrain {
  pub prefix:          String,
  pub created:         Instant,
  pub data:            BTreeMap<String, AggregatedMetric>,
  /// cluster_id -> response time histogram (in ms)
  pub cluster_data:        BTreeMap<String, AppMetrics>,
  // backend_id -> response time histogram (in ms)
//  pub backend_data:    BTreeMap<String, BackendMetrics>,
  //pub request_counter: TimeSerie,
  use_tagged_metrics:  bool,
  origin:              String,
}

impl LocalDrain {
  pub fn new(prefix: String) -> Self {
    LocalDrain {
      prefix,
      created:     Instant::now(),
      data:        BTreeMap::new(),
      cluster_data:    BTreeMap::new(),
 //     backend_data: BTreeMap::new(),
      //request_counter: TimeSerie::new(),
      use_tagged_metrics: false,
      origin:      String::from("x"),
    }
  }

  pub fn dump_metrics_data(&mut self) -> MetricsData {
    MetricsData {
      proxy:        self.dump_process_data(),
      clusters: self.dump_cluster_data(),
    }
  }

  pub fn dump_process_data(&mut self) -> BTreeMap<String, FilteredData> {
    let data: BTreeMap<String, FilteredData> = self.data.iter().map(|(ref key, ref value)| {
      (key.to_string(), aggregated_to_filtered(value))
    }).collect();

    data
  }

  pub fn dump_cluster_data(&mut self) -> BTreeMap<String,AppMetricsData> {
    let data = self.cluster_data.iter().map(|(ref cluster_id, ref cluster)| {
      let data = cluster.data.iter().map(|(ref key, ref value)| {
         (key.to_string(), aggregated_to_filtered(value))
       }).collect();
      let backends = cluster.backend_data.iter().map(|(ref backend_id, ref backend_data)| {
        let b = backend_data.iter().map(|(ref key, ref value)| {
         (key.to_string(), aggregated_to_filtered(value))
        }).collect();

        (backend_id.to_string(), b)
      }).collect();

      (cluster_id.to_string(), AppMetricsData { data, backends })
    }).collect();

    self.cluster_data.clear();

    data
  }

  pub fn clear(&mut self) {
    self.cluster_data.clear();
  }
}


impl Subscriber for LocalDrain {
  fn receive_metric(&mut self, key: &'static str, cluster_id: Option<&str>, backend_id: Option<&str>, metric: MetricData) {
    if let Some(id) = cluster_id {
      if !self.cluster_data.contains_key(id) {
        self.cluster_data.insert(
          String::from(id),
          AppMetrics {
            data: BTreeMap::new(),
            backend_data: BTreeMap::new(),
          }
        );
      }

      self.cluster_data.get_mut(id).map(|cluster| {
        if let Some(bid) = backend_id {
          if !cluster.backend_data.contains_key(bid) {
            cluster.backend_data.insert(
              String::from(bid),
              BTreeMap::new()
            );
          }

          cluster.backend_data.get_mut(bid).map(|backend_data| {
            if !backend_data.contains_key(key) {
              backend_data.insert(
                String::from(key),
                AggregatedMetric::new(metric)
              );
            } else {
              backend_data.get_mut(key).map(|stored_metric| {
                stored_metric.update(key, metric);
              });
            }
          });
        } else if !cluster.data.contains_key(key) {
          cluster.data.insert(
            String::from(key),
            AggregatedMetric::new(metric)
          );
        } else {
          cluster.data.get_mut(key).map(|stored_metric| {
            stored_metric.update(key, metric);
          });
        }
      });
    } else if !self.data.contains_key(key) {
      self.data.insert(
        String::from(key),
        AggregatedMetric::new(metric)
        );
    } else {
      self.data.get_mut(key).map(|stored_metric| {
        stored_metric.update(key, metric);
      });
    }
  }
}

/*
#[derive(Debug)]
pub struct ProxyMetrics {
  pub buffer:          Buffer,
  pub prefix:          String,
  pub created:         Instant,
  pub data:            BTreeMap<String, StoredMetricData>,
  /// cluster_id -> response time histogram (in ms)
  pub cluster_data:        BTreeMap<String,AppMetrics>,
  /// backend_id -> response time histogram (in ms)
  pub backend_data:    BTreeMap<String,BackendMetrics>,
  pub request_counter: TimeSerie,
  pub is_writable:     bool,
  use_tagged_metrics:  bool,
  origin:              String,
  remote:              Option<(SocketAddr, UdpSocket)>,
}

impl ProxyMetrics {
  pub fn new(prefix: String) -> Self {
    ProxyMetrics {
      buffer:      Buffer::with_capacity(2048),
      prefix:      prefix,
      created:     Instant::now(),
      data:        BTreeMap::new(),
      cluster_data:    BTreeMap::new(),
      backend_data: BTreeMap::new(),
      request_counter: TimeSerie::new(),
      is_writable: false,
      use_tagged_metrics: false,
      remote:      None,
      origin:      String::from("x"),
    }
  }

  pub fn set_up_remote(&mut self, socket: UdpSocket, addr: SocketAddr) {
    self.remote = Some((addr, socket));
  }

  pub fn set_up_origin(&mut self, origin: String) {
    self.origin = origin;
  }

  pub fn set_up_tagged_metrics(&mut self, tagged: bool) {
    self.use_tagged_metrics = tagged;
  }

  pub fn socket(&self) -> Option<&UdpSocket> {
    self.remote.as_ref().map(|remote| &remote.1)
  }


  pub fn dump_percentiles(&self) -> BTreeMap<String, Percentiles> {
    self.cluster_data.iter().map(|(ref cluster_id, ref metrics)| {
      let percentiles = Percentiles {
        samples:  metrics.response_time.len(),
        p_50:     metrics.response_time.value_at_percentile(50.0),
        p_90:     metrics.response_time.value_at_percentile(90.0),
        p_99:     metrics.response_time.value_at_percentile(99.0),
        p_99_9:   metrics.response_time.value_at_percentile(99.9),
        p_99_99:  metrics.response_time.value_at_percentile(99.99),
        p_99_999: metrics.response_time.value_at_percentile(99.999),
        p_100:    metrics.response_time.value_at_percentile(100.0),
      };

      (cluster_id.to_string(), percentiles)
    }).collect()
  }

  pub fn dump_backend_data(&self) -> BTreeMap<String, BackendMetricsData> {
    self.backend_data.iter().map(|(ref backend_id, ref bm)| {
      let percentiles = Percentiles {
        samples:  bm.response_time.len(),
        p_50:     bm.response_time.value_at_percentile(50.0),
        p_90:     bm.response_time.value_at_percentile(90.0),
        p_99:     bm.response_time.value_at_percentile(99.0),
        p_99_9:   bm.response_time.value_at_percentile(99.9),
        p_99_99:  bm.response_time.value_at_percentile(99.99),
        p_99_999: bm.response_time.value_at_percentile(99.999),
        p_100:    bm.response_time.value_at_percentile(100.0),
      };

      let data = BackendMetricsData {
        bytes_in:  bm.bin,
        bytes_out: bm.bout,
        percentiles: percentiles,
      };

      (backend_id.to_string(), data)
    }).collect()
  }

  pub fn dump_metrics_data(&mut self) -> MetricsData {
    MetricsData {
      proxy:        self.dump_data(),
      clusters: self.dump_percentiles(),
      backends:     self.dump_backend_data(),
    }
  }
}
*/

/*
#[derive(Debug,Clone)]
pub struct TimeSerie {
  sent_at:           Instant,
  updated_second_at: Instant,
  updated_minute_at: Instant,
  last_sent:         u32,
  last_second:       u32,
  last_minute:       VecDeque<u32>,
  last_hour:         VecDeque<u32>,
}

impl TimeSerie {
  pub fn new() -> TimeSerie {
    TimeSerie {
      sent_at:           Instant::now(),
      updated_second_at: Instant::now(),
      updated_minute_at: Instant::now(),
      last_sent:         0,
      last_second:       0,
      last_minute:       repeat(0).take(60).collect(),
      last_hour:         repeat(0).take(60).collect(),
    }
  }

  pub fn add(&mut self, value: u32) {
    let now = Instant::now();

    if now - self.updated_minute_at > Duration::from_secs(60) {
      self.updated_minute_at = now;
      let _ = self.last_hour.pop_front();

      self.last_hour.push_back( self.last_minute.iter().sum() );
    }

    if now - self.updated_second_at > Duration::from_secs(1) {
      self.updated_second_at = now;
      let _ = self.last_minute.pop_front();

      self.last_minute.push_back( self.last_second );

      self.last_second = value;
    } else {
      self.last_second += value;
    }

    self.last_sent += value;
  }

  pub fn increment(&mut self) {
    self.add(1);
  }

  pub fn filtered(&mut self) -> FilteredTimeSerie {
    let now = Instant::now();

    if now - self.updated_minute_at > Duration::from_secs(60) {
      self.updated_minute_at = now;
      let _ = self.last_hour.pop_front();

      self.last_hour.push_back( self.last_minute.iter().sum() );
    }

    if now - self.updated_second_at > Duration::from_secs(1) {
      self.updated_second_at = now;
      let _ = self.last_minute.pop_front();

      self.last_minute.push_back( self.last_second );

      self.last_second = 0;
    }

    FilteredTimeSerie {
      last_second: self.last_second,
      last_minute: self.last_minute.iter().cloned().collect(),
      last_hour:   self.last_hour.iter().cloned().collect(),
    }
  }

  pub fn update_sent_at(&mut self, now: Instant) {
    self.sent_at   = now;
    self.last_sent = 0;
  }
}
*/
