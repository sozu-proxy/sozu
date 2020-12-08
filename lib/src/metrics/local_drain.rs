#![allow(dead_code)]
use std::str;
use std::time::Instant;
use time::OffsetDateTime;
use std::convert::TryInto;
use std::collections::BTreeMap;
use hdrhistogram::Histogram;
use sozu_command::proxy::{FilteredData,MetricsData,Percentiles,AppMetricsData,QueryMetricsType};

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
            error!("could not record time metric: {:?}", e);
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
            error!("could not record time metric: {:?}", e);
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

#[derive(Clone,Debug)]
enum MetricKind {
  Gauge,
  Count,
  Time,
}

#[derive(Clone,Debug,PartialEq)]
enum MetricMeta {
    Cluster,
    ClusterBackend,
}

#[derive(Debug)]
pub struct LocalDrain {
  pub prefix:          String,
  pub created:         Instant,
  pub db:              sled::Db,
  pub cluster_tree:    sled::Tree,
  pub backend_tree:    sled::Tree,
  pub data:            BTreeMap<String, AggregatedMetric>,
  metrics:             BTreeMap<String, (MetricMeta, MetricKind)>,
  use_tagged_metrics:  bool,
  origin:              String,
}

impl LocalDrain {
  pub fn new(prefix: String) -> Self {
    let db = sled::Config::new()
        .temporary(true)
        .mode(sled::Mode::LowSpace)
        .open()
        .unwrap();
    let cluster_tree = db.open_tree("cluster").unwrap();
    let backend_tree = db.open_tree("backend").unwrap();

    LocalDrain {
      prefix,
      created:     Instant::now(),
      db,
      cluster_tree,
      backend_tree,
      metrics:     BTreeMap::new(),
      data:        BTreeMap::new(),
      use_tagged_metrics: false,
      origin:      String::from("x"),
    }
  }

  pub fn dump_metrics_data(&mut self) -> MetricsData {
    MetricsData {
      proxy:    self.dump_process_data(),
      clusters: self.dump_cluster_data(),
    }
  }

  pub fn dump_process_data(&mut self) -> BTreeMap<String, FilteredData> {
    let data: BTreeMap<String, FilteredData> = self.data.iter().map(|(ref key, ref value)| {
      (key.to_string(), aggregated_to_filtered(value))
    }).collect();

    data
  }

  pub fn query(&mut self, q: &QueryMetricsType) -> BTreeMap<String, FilteredData> {
      info!("GOT QUERY: {:?}", q);
      match q {
          QueryMetricsType::Cluster { metrics, clusters } => {
              self.query_cluster(metrics, clusters)
          },
          QueryMetricsType::Backend { metrics, backends } => {
              self.query_backend(metrics, backends)
          },
      }
  }

  fn query_cluster(&mut self, metrics: &Vec<String>, clusters: &Vec<String>) -> BTreeMap<String, FilteredData> {
      let mut apps: BTreeMap<String, FilteredData> = BTreeMap::new();

      info!("current metrics: {:#?}", self.metrics);
      for prefix_key in metrics.iter() {
          for cluster_id in clusters.iter() {
              let key = format!("{}\t{}", prefix_key, cluster_id);

              let res = self.metrics.get(&key);
              if res.is_none() {
                  error!("unknown metric key {}", key);
                  continue
              }
              let (meta, kind) = res.unwrap();

              let end = format!("{}\x7F", key);
              for res in self.cluster_tree.range(key.as_bytes()..end.as_bytes()) {
                  let (k, v) = res.unwrap();
                  info!("looking at key: {:?}", std::str::from_utf8(&k));
                  match meta {
                      MetricMeta::Cluster => {
                          let mut it = k.split(|c: &u8| *c == b'\t');
                          let key = std::str::from_utf8(it.next().unwrap()).unwrap();
                          let app_id = std::str::from_utf8(it.next().unwrap()).unwrap();
                          let timestamp:i64 = std::str::from_utf8(it.next().unwrap()).unwrap().parse().unwrap();

                          info!("looking at key = {}, id = {}, ts = {}",
                                key, cluster_id, timestamp);

                          let output_key = format!("{}.{}", cluster_id, key);
                          match kind {
                              MetricKind::Gauge => {
                                  apps.insert(output_key, FilteredData::Gauge(usize::from_le_bytes((*v).try_into().unwrap())));
                              },
                              MetricKind::Count => {
                                  apps.insert(output_key, FilteredData::Count(i64::from_le_bytes((*v).try_into().unwrap())));
                              },
                              MetricKind::Time => {
                                  //unimplemented for now
                              }
                          }
                      },
                      MetricMeta::ClusterBackend => {
                          error!("metric key {} is for backend level metrics", key);
                      }
                  }
              }
          }
      }

      info!("WILL RETURN: {:#?}", apps);
      apps
  }

  fn query_backend(&mut self, metrics: &Vec<String>, backends: &Vec<(String,String)>) -> BTreeMap<String, FilteredData> {
      let mut backend_data: BTreeMap<String, FilteredData> = BTreeMap::new();

      info!("current metrics: {:#?}", self.metrics);
      for prefix_key in metrics.iter() {
          for (cluster_id, backend_id) in backends.iter() {
              let key = format!("{}\t{}\t{}", prefix_key, cluster_id, backend_id);

              let res = self.metrics.get(&key);
              if res.is_none() {
                  error!("unknown metric key {}", key);
                  continue
              }
              let (meta, kind) = res.unwrap();

              let end = format!("{}\x7F", key);
              for res in self.backend_tree.range(key.as_bytes()..end.as_bytes()) {
                  let (k, v) = res.unwrap();
                  info!("looking at key: {:?}", std::str::from_utf8(&k));
                  match meta {
                      MetricMeta::Cluster => {
                          error!("metric key {} is for cluster level metrics", key);
                      },
                      MetricMeta::ClusterBackend => {
                          let mut it = k.split(|c: &u8| *c == b'\t');
                          let key = std::str::from_utf8(it.next().unwrap()).unwrap();
                          let app_id = std::str::from_utf8(it.next().unwrap()).unwrap();
                          let backend_id = std::str::from_utf8(it.next().unwrap()).unwrap();
                          let timestamp:i64 = std::str::from_utf8(it.next().unwrap()).unwrap().parse().unwrap();

                          info!("looking at key = {}, cluster id = {}, bid: {}, ts = {}",
                                key, app_id, backend_id, timestamp);

                          let output_key = format!("{}.{}.{}", cluster_id, backend_id, key);
                          match kind {
                              MetricKind::Gauge => {
                                  backend_data.insert(output_key, FilteredData::Gauge(usize::from_le_bytes((*v).try_into().unwrap())));
                              },
                              MetricKind::Count => {
                                  backend_data.insert(output_key, FilteredData::Count(i64::from_le_bytes((*v).try_into().unwrap())));
                              },
                              MetricKind::Time => {
                                  //unimplemented for now
                              }
                          }
                      }
                  }

              }


          }
      }

      info!("WILL RETURN: {:#?}", backend_data);
      backend_data
  }


  pub fn dump_cluster_data(&mut self) -> BTreeMap<String,AppMetricsData> {
      let mut apps = BTreeMap::new();

      for (key, (meta, kind)) in self.metrics.iter() {
          let end = format!("{}\x7F", key);

          match meta {
              MetricMeta::Cluster => {
                  for res in self.cluster_tree.range(key.as_bytes()..end.as_bytes()) {
                      let (k, v) = res.unwrap();

                      let mut it = k.split(|c: &u8| *c == b'\t');
                      let key = std::str::from_utf8(it.next().unwrap()).unwrap();
                      let app_id = std::str::from_utf8(it.next().unwrap()).unwrap();
                      let timestamp:i64 = std::str::from_utf8(it.next().unwrap()).unwrap().parse().unwrap();

                      info!("looking at key = {}, id = {}, ts = {}",
                            key, app_id, timestamp);

                      let metrics_data = apps.entry(app_id.to_string()).or_insert_with(AppMetricsData::new);
                      match kind {
                          MetricKind::Gauge => {
                              if metrics_data.data.contains_key(key) {
                                  let v2 = metrics_data.data.get(key).unwrap().clone();
                              } else {
                                  metrics_data.data.insert(key.to_string(), FilteredData::Gauge(usize::from_le_bytes((*v).try_into().unwrap())));
                              }
                          },
                          MetricKind::Count => {
                              if metrics_data.data.contains_key(key) {
                                  let v2 = metrics_data.data.get(key).unwrap().clone();
                              } else {
                                  metrics_data.data.insert(key.to_string(), FilteredData::Count(i64::from_le_bytes((*v).try_into().unwrap())));
                              }
                          },
                          MetricKind::Time => {
                              //unimplemented for now
                          }
                      }
                  }
              },
              MetricMeta::ClusterBackend => {
                  for res in self.backend_tree.range(key.as_bytes()..end.as_bytes()) {
                      let (k, v) = res.unwrap();

                      let mut it = k.split(|c: &u8| *c == b'\t');
                      let key = std::str::from_utf8(it.next().unwrap()).unwrap();
                      let app_id = std::str::from_utf8(it.next().unwrap()).unwrap();
                      let backend_id = std::str::from_utf8(it.next().unwrap()).unwrap();
                      let timestamp:i64 = std::str::from_utf8(it.next().unwrap()).unwrap().parse().unwrap();

                      info!("looking at key = {}, ap id = {}, bid: {}, ts = {}",
                            key, app_id, backend_id, timestamp);

                      let app_metrics_data = apps.entry(app_id.to_string()).or_insert_with(AppMetricsData::new);
                      let backend_metrics_data = app_metrics_data.backends.entry(backend_id.to_string()).or_insert_with(BTreeMap::new);
                      match kind {
                          MetricKind::Gauge => {
                              if backend_metrics_data.contains_key(key) {
                                  let v2 = backend_metrics_data.get(key).unwrap().clone();
                              } else {
                                  backend_metrics_data.insert(key.to_string(), FilteredData::Gauge(usize::from_le_bytes((*v).try_into().unwrap())));
                              }
                          },
                          MetricKind::Count => {
                              if backend_metrics_data.contains_key(key) {
                                  let v2 = backend_metrics_data.get(key).unwrap().clone();
                              } else {
                                  backend_metrics_data.insert(key.to_string(), FilteredData::Count(i64::from_le_bytes((*v).try_into().unwrap())));
                              }
                          },
                          MetricKind::Time => {
                              //unimplemented for now
                          }
                      }
                  }
              },
          }
      }


      // still clear the DB for now
      //self.db.clear();

      apps
  }

  fn receive_cluster_metric(&mut self, key: &'static str, id: &str, backend_id: Option<&str>, metric: MetricData) {
      info!("metric: {} {} {:?} {:?}", key, id, backend_id, metric);
      // the final character is necessary to start iterating after the tab that is used in backend
      // metrics
      self.store_metric(&format!("{}\t{}", key, id), id, None, &metric);
      if let Some(bid) = backend_id {
          self.store_metric(&format!("{}\t{}\t{}", key, id, bid), id, backend_id, &metric);
      }
  }

  fn store_metric(&mut self, key_prefix: &str, id: &str, backend_id: Option<&str>, metric: &MetricData) {
      info!("metric: {} {} {:?} {:?}", key_prefix, id, backend_id, metric);

      if !self.metrics.contains_key(key_prefix) {
          let kind = match metric {
              MetricData::Gauge(_) => MetricKind::Gauge,
              MetricData::GaugeAdd(_) => MetricKind::Gauge,
              MetricData::Count(_) => MetricKind::Count,
              MetricData::Time(_) => MetricKind::Time,
          };
          let meta = if backend_id.is_some() {
              MetricMeta::ClusterBackend
          } else {
              MetricMeta::Cluster
          };

          self.metrics.insert(key_prefix.to_string(), (meta, kind));
          let end = format!("{}\x7F", key_prefix);
          if backend_id.is_some() {
              self.backend_tree.insert(end.as_bytes(), &0u64.to_le_bytes()).unwrap();
          } else {
              self.cluster_tree.insert(end.as_bytes(), &0u64.to_le_bytes()).unwrap();
          }
      }

      match metric {
          MetricData::Gauge(i) => {
              self.store_gauge(&key_prefix, *i, backend_id.is_some());
          },
          MetricData::GaugeAdd(i) => {
              self.add_gauge(&key_prefix, *i, backend_id.is_some());
          },
          MetricData::Count(i) => {
              self.store_count(&key_prefix, *i, backend_id.is_some());
          },
          MetricData::Time(i) => {
              //self.db.insert(db_key.as_bytes(), &i.to_le_bytes()).unwrap();
          },
      }

      /*
      if let (Some(first), Some(second)) = (self.db.first().unwrap(), self.db.last().unwrap()) {
        for res in self.db.range(first.0..second.0) {
            let (k, v) = res.unwrap();
            info!("{} -> {:?}", unsafe { std::str::from_utf8_unchecked(&k) }, u64::from_le_bytes((*v).try_into().unwrap()));

        }
      }
      //info!("metrics: {:?}", self.metrics);
      info!("db size: {:?}", self.db.size_on_disk());
      */
  }

  fn store_gauge(&mut self, key: &str, i: usize, is_backend: bool) {
      let now = OffsetDateTime::now_utc();
      let timestamp = now.unix_timestamp();
      let complete_key = format!("{}\t{}", key, timestamp);

      info!("store gauge at {} -> {}", complete_key, i);
      if is_backend {
          self.backend_tree.insert(complete_key.as_bytes(), &i.to_le_bytes()).unwrap();
      } else {
          self.cluster_tree.insert(complete_key.as_bytes(), &i.to_le_bytes()).unwrap();
      }

      // we change the minute, aggregate the 60 measurements from the last minute
      if now.second() == 0 {
          self.aggregate_gauge(key, now, is_backend);
      }
  }

  fn add_gauge(&mut self, key: &str, i: i64, is_backend: bool) {
      let now = OffsetDateTime::now_utc();
      let timestamp = now.unix_timestamp();
      let complete_key = format!("{}\t{}", key, timestamp);
      let one_minute_ago = format!("{}\t{}", key, timestamp - 60);

      let tree = if is_backend {
          &mut self.backend_tree
      } else {
          &mut self.cluster_tree
      };
      info!("add gauge at {} -> {}", complete_key, i);
      match tree.range(one_minute_ago.as_bytes()..=complete_key.as_bytes()).rev().next() {
          None | Some(Err(_)) => {
              tree.insert(complete_key.as_bytes(), &i.to_le_bytes()).unwrap();
          },
          Some(Ok((_, v))) => {
              let i2 = i64::from_le_bytes((*v).try_into().unwrap());
              tree.insert(complete_key.as_bytes(), &(i+i2).to_le_bytes()).unwrap();
          }
      };

      // we change the minute, aggregate the 60 measurements from the last minute
      if now.second() == 0 {
          self.aggregate_gauge(key, now, is_backend);
      }
  }

  fn store_count(&mut self, key: &str, i: i64, is_backend: bool) {
      let now = OffsetDateTime::now_utc();
      let timestamp = now.unix_timestamp();
      let complete_key = format!("{}\t{}", key, timestamp);

      let tree = if is_backend {
          &mut self.backend_tree
      } else {
          &mut self.cluster_tree
      };
      info!("store count at {} -> {}", complete_key, i);
      match tree.get(complete_key.as_bytes()).unwrap() {
          None => {
              tree.insert(complete_key.as_bytes(), &i.to_le_bytes()).unwrap();
          },
          Some(v) => {
              let i2 = i64::from_le_bytes((*v).try_into().unwrap());
              tree.insert(complete_key.as_bytes(), &(i+i2).to_le_bytes()).unwrap();
          }
      };

      // we change the minute, aggregate the 60 measurements from the last minute
      if now.second() == 0 {
          self.aggregate_count(key, now, is_backend);
      }
  }

  fn aggregate_gauge(&mut self, key: &str, now: OffsetDateTime, is_backend: bool) {
      let timestamp = now.unix_timestamp();
      let one_hour_ago = format!("{}\t{}", key, timestamp - 3600);
      let one_minute_ago = format!("{}\t{}", key, timestamp - 60);
      let now_key = format!("{}\t{}", key, timestamp);

      let tree = if is_backend {
          &mut self.backend_tree
      } else {
          &mut self.cluster_tree
      };

      // aggregate 60 measures in a point at the last minute
      let mut value = None;
      for res in tree.range(one_minute_ago.as_bytes()..now_key.as_bytes()) {
          let (k, v) = res.unwrap();
          value = Some(usize::from_le_bytes((*v).try_into().unwrap()));
          info!("removing {} -> {:?}", unsafe { std::str::from_utf8_unchecked(&k) }, u64::from_le_bytes((*v).try_into().unwrap()));
          tree.remove(k).unwrap();
      }

      if let Some(v) = value {
          info!("reinserting {} -> {:?}", one_minute_ago, v);
          tree.insert(one_minute_ago.as_bytes(), &v.to_le_bytes()).unwrap();
      }

      // aggregate 60 measures in a point at the last hour
      if now.minute() == 0 {
          let mut value = None;
          for res in tree.range(one_hour_ago.as_bytes()..one_minute_ago.as_bytes()) {
              let (k, v) = res.unwrap();
              value = Some(usize::from_le_bytes((*v).try_into().unwrap()));
              info!("removing {} -> {:?}", unsafe { std::str::from_utf8_unchecked(&k) }, u64::from_le_bytes((*v).try_into().unwrap()));
              tree.remove(k).unwrap();
          }

          if let Some(v) = value {
              info!("reinserting {} -> {:?}", one_hour_ago, v);
              tree.insert(one_minute_ago.as_bytes(), &v.to_le_bytes()).unwrap();
          }

          // remove all measures older than 24h
          let one_day_ago = format!("{}\t{}", key, timestamp - 3600 * 24);
          for res in tree.range(key.as_bytes()..one_day_ago.as_bytes()) {
              let (k, v) = res.unwrap();
              value = Some(usize::from_le_bytes((*v).try_into().unwrap()));
              info!("removing {} -> {:?} (more than 24h)", unsafe { std::str::from_utf8_unchecked(&k) }, value);
              tree.remove(k).unwrap();
          }
      }

  }

  fn aggregate_count(&mut self, key: &str, now: OffsetDateTime, is_backend: bool) {
      let timestamp = now.unix_timestamp();
      let one_hour_ago = format!("{}\t{}", key, timestamp - 3600);
      let one_minute_ago = format!("{}\t{}", key, timestamp - 60);
      let now_key = format!("{}\t{}", key, timestamp);

      let tree = if is_backend {
          &mut self.backend_tree
      } else {
          &mut self.cluster_tree
      };

      // aggregate 60 measures in a point at the last hour
      let mut value = 0i64;
      let mut found = false;
      for res in tree.range(one_minute_ago.as_bytes()..now_key.as_bytes()) {
          found = true;
          let (k, v) = res.unwrap();
          value += i64::from_le_bytes((*v).try_into().unwrap());
          info!("removing {} -> {:?}", unsafe { std::str::from_utf8_unchecked(&k) }, u64::from_le_bytes((*v).try_into().unwrap()));
          tree.remove(k).unwrap();
      }

      if found {
          info!("reinserting {} -> {:?}", one_minute_ago, value);
          tree.insert(one_minute_ago.as_bytes(), &value.to_le_bytes()).unwrap();
      }

      // remove all measures older than 24h
      if now.minute() == 0 {
          let mut value = 0i64;
          let mut found = false;
          for res in tree.range(one_hour_ago.as_bytes()..one_minute_ago.as_bytes()) {
              found = true;
              let (k, v) = res.unwrap();
              value += i64::from_le_bytes((*v).try_into().unwrap());
              info!("removing {} -> {:?}", unsafe { std::str::from_utf8_unchecked(&k) }, u64::from_le_bytes((*v).try_into().unwrap()));
              tree.remove(k).unwrap();
          }

          if found {
              info!("reinserting {} -> {:?}", one_hour_ago, value);
              tree.insert(one_hour_ago.as_bytes(), &value.to_le_bytes()).unwrap();
          }

          // remove all measures older than 24h
          let one_day_ago = format!("{}\t{}", key, timestamp - 3600 * 24);
          for res in tree.range(key.as_bytes()..one_day_ago.as_bytes()) {
              let (k, v) = res.unwrap();
              value = i64::from_le_bytes((*v).try_into().unwrap());
              info!("removing {} -> {:?} (more than 24h)", unsafe { std::str::from_utf8_unchecked(&k) }, value);
              tree.remove(k).unwrap();
          }
      }
  }

  pub fn clear(&mut self, now: OffsetDateTime) {
      info!("will clear old data from the metrics database");
      //self.db.clear();
      //

      let metrics = self.metrics.clone();
      for (key, (meta, kind)) in metrics.iter() {
          info!("will aggregate metrics for key '{}'", key);

          let is_backend = *meta == MetricMeta::ClusterBackend;
          match kind {
              MetricKind::Gauge => {
                  self.aggregate_gauge(key, now, is_backend);
              },
              MetricKind::Count => {
                  self.aggregate_count(key, now, is_backend);
              },
              MetricKind::Time => {
              }
          }

          let tree = match meta {
              MetricMeta::Cluster => &mut self.cluster_tree,
              MetricMeta::ClusterBackend => &mut self.backend_tree,
          };

          // check if we removed all the points for this metric
          let end = format!("{}\x7F", key);
          if let Some((k, _)) = tree.get_gt(key.as_bytes()).unwrap() {
              if &k == end.as_bytes() {
                  info!("removing key {} from metrics", key);
                  tree.remove(k).unwrap();
                  self.metrics.remove(key);
              }
          }
      }

      info!("remaining keys:");
      if let (Some(first), Some(second)) = (self.cluster_tree.first().unwrap(), self.cluster_tree.last().unwrap()) {
        for res in self.cluster_tree.range(first.0..second.0) {
            let (k, v) = res.unwrap();
            info!("{} -> {:?}", unsafe { std::str::from_utf8_unchecked(&k) }, u64::from_le_bytes((*v).try_into().unwrap()));

        }
      }
      if let (Some(first), Some(second)) = (self.backend_tree.first().unwrap(), self.backend_tree.last().unwrap()) {
        for res in self.backend_tree.range(first.0..second.0) {
            let (k, v) = res.unwrap();
            info!("{} -> {:?}", unsafe { std::str::from_utf8_unchecked(&k) }, u64::from_le_bytes((*v).try_into().unwrap()));

        }
      }
      info!("db size: {:?}", self.db.size_on_disk());
  }

}


impl Subscriber for LocalDrain {
  fn receive_metric(&mut self, key: &'static str, cluster_id: Option<&str>, backend_id: Option<&str>, metric: MetricData) {
    if let Some(id) = cluster_id {
      self.receive_cluster_metric(key, id, backend_id, metric);
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
