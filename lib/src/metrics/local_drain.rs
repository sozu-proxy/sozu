#![allow(dead_code)]
use hdrhistogram::Histogram;
use sozu_command::proxy::{
    AppMetricsData, FilteredData, MetricsConfiguration, MetricsData, Percentiles,
    QueryAnswerMetrics, QueryMetricsType,
};
use std::collections::BTreeMap;
use std::str;
use std::time::Instant;
use time::{Duration, OffsetDateTime};

use super::{MetricData, Subscriber};

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

pub fn aggregated_to_filtered(value: &AggregatedMetric) -> FilteredData {
    match value {
        &AggregatedMetric::Gauge(i) => FilteredData::Gauge(i),
        &AggregatedMetric::Count(i) => FilteredData::Count(i),
        &AggregatedMetric::Time(ref hist) => {
            FilteredData::Percentiles(histogram_to_percentiles(&hist))
        }
    }
}

#[derive(Clone, Debug)]
pub struct AppMetrics {
    pub data: BTreeMap<String, AggregatedMetric>,
    pub backend_data: BTreeMap<String, BTreeMap<String, AggregatedMetric>>,
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

#[derive(Debug)]
pub struct LocalDrain {
    pub prefix: String,
    pub created: Instant,
    pub cluster_tree: BTreeMap<String, u64>,
    pub backend_tree: BTreeMap<String, u64>,
    pub data: BTreeMap<String, AggregatedMetric>,
    metrics: BTreeMap<String, (MetricMeta, MetricKind)>,
    use_tagged_metrics: bool,
    origin: String,
    enabled: bool,
    time_enabled: bool,
}

impl LocalDrain {
    pub fn new(prefix: String) -> Self {
        LocalDrain {
            prefix,
            created: Instant::now(),
            //db,
            cluster_tree: BTreeMap::new(),
            backend_tree: BTreeMap::new(),
            metrics: BTreeMap::new(),
            data: BTreeMap::new(),
            use_tagged_metrics: false,
            origin: String::from("x"),
            enabled: false,
            time_enabled: false,
        }
    }

    fn tree(&mut self, is_backend: bool) -> &mut BTreeMap<String, u64> {
        if is_backend {
            &mut self.backend_tree
        } else {
            &mut self.cluster_tree
        }
    }

    pub fn configure(&mut self, config: &MetricsConfiguration) {
        match config {
            MetricsConfiguration::Enabled(enabled) => {
                self.enabled = *enabled;
            }
            MetricsConfiguration::EnabledTimeMetrics(enabled) => {
                self.time_enabled = *enabled;
            }
            MetricsConfiguration::Clear => {
                self.backend_tree.clear();
                self.cluster_tree.clear();
            }
        }
    }

    pub fn dump_metrics_data(&mut self) -> MetricsData {
        MetricsData {
            proxy: self.dump_process_data(),
            clusters: self.dump_cluster_data(),
        }
    }

    pub fn dump_process_data(&mut self) -> BTreeMap<String, FilteredData> {
        let data: BTreeMap<String, FilteredData> = self
            .data
            .iter()
            .map(|(ref key, ref value)| (key.to_string(), aggregated_to_filtered(value)))
            .collect();

        data
    }

    pub fn query(&mut self, q: &QueryMetricsType) -> QueryAnswerMetrics {
        debug!("query: {:?}", q);
        match q {
            QueryMetricsType::List => {
                QueryAnswerMetrics::List(self.metrics.keys().cloned().collect())
            }
            QueryMetricsType::Cluster {
                metrics,
                clusters,
                date,
            } => self.query_cluster(metrics, clusters, date.clone()),
            QueryMetricsType::Backend {
                metrics,
                backends,
                date,
            } => self.query_backend(metrics, backends, date.clone()),
        }
    }

    fn query_metric(
        &self,
        key: &str,
        is_backend: bool,
        timestamp: i64,
        kind: MetricKind,
    ) -> Option<FilteredData> {
        let tree = if is_backend {
            &self.backend_tree
        } else {
            &self.cluster_tree
        };

        if kind == MetricKind::Time {
            let mut percentiles = Percentiles::default();

            if let Some(v) = tree.get(&format!("{}.count \t{}", key, timestamp)) {
                percentiles.samples = *v;
            }
            if let Some(v) = tree.get(&format!("{}.p50 \t{}", key, timestamp)) {
                percentiles.p_50 = *v;
            }
            if let Some(v) = tree.get(&format!("{}.p90 \t{}", key, timestamp)) {
                percentiles.p_90 = *v;
            }
            if let Some(v) = tree.get(&format!("{}.p99 \t{}", key, timestamp)) {
                percentiles.p_99 = *v;
            }
            if let Some(v) = tree.get(&format!("{}.p99.9 \t{}", key, timestamp)) {
                percentiles.p_99_9 = *v;
            }
            if let Some(v) = tree.get(&format!("{}.p99.99 \t{}", key, timestamp)) {
                percentiles.p_99_99 = *v;
            }
            if let Some(v) = tree.get(&format!("{}.p99.999 \t{}", key, timestamp)) {
                percentiles.p_99_999 = *v;
            }
            if let Some(v) = tree.get(&format!("{}.p100 \t{}", key, timestamp)) {
                percentiles.p_100 = *v;
            }

            return Some(FilteredData::Percentiles(percentiles));
        }

        if let Some(v) = tree.get(&format!("{}\t{}", key, timestamp)) {
            match kind {
                MetricKind::Gauge => {
                    return Some(FilteredData::Gauge(*v as usize));
                }
                MetricKind::Count => {
                    return Some(FilteredData::Count(*v as i64));
                }
                MetricKind::Time => {}
            }
        }

        None
    }

    fn query_cluster(
        &mut self,
        metrics: &Vec<String>,
        clusters: &Vec<String>,
        date: Option<i64>,
    ) -> QueryAnswerMetrics {
        let mut apps: BTreeMap<String, BTreeMap<String, FilteredData>> = BTreeMap::new();
        for cluster_id in clusters.iter() {
            apps.insert(cluster_id.to_string(), BTreeMap::new());
        }

        let timestamp = date.unwrap_or_else(|| {
            let now = OffsetDateTime::now_utc();
            let last_minute = now - Duration::seconds(now.second() as i64);
            last_minute.unix_timestamp()
        });

        trace!("current metrics: {:#?}", self.metrics);

        for prefix_key in metrics.iter() {
            for cluster_id in clusters.iter() {
                let key = format!("{}\t{}", prefix_key, cluster_id);

                let res = self.metrics.get(&key);
                if res.is_none() {
                    //error!("unknown metric key {}", key);
                    continue;
                }
                let (meta, kind) = res.unwrap();

                if *meta == MetricMeta::ClusterBackend {
                    error!("{} is a backend metric", key);
                    continue;
                }

                if let Some(filtered_data) = self.query_metric(&key, false, timestamp, *kind) {
                    apps.get_mut(cluster_id)
                        .unwrap()
                        .insert(key.to_string(), filtered_data);
                }
            }
        }

        trace!("query result: {:#?}", apps);
        QueryAnswerMetrics::Cluster(apps)
    }

    fn query_backend(
        &mut self,
        metrics: &Vec<String>,
        backends: &Vec<(String, String)>,
        date: Option<i64>,
    ) -> QueryAnswerMetrics {
        let mut backend_data: BTreeMap<String, BTreeMap<String, BTreeMap<String, FilteredData>>> =
            BTreeMap::new();
        for (cluster_id, backend_id) in backends.iter() {
            let t = backend_data
                .entry(cluster_id.to_string())
                .or_insert_with(BTreeMap::new);
            t.insert(backend_id.to_string(), BTreeMap::new());
        }

        let timestamp = date.unwrap_or_else(|| {
            let now = OffsetDateTime::now_utc();
            let last_minute = now - Duration::seconds(now.second() as i64);
            last_minute.unix_timestamp()
        });

        trace!("current metrics: {:#?}", self.metrics);
        for prefix_key in metrics.iter() {
            for (cluster_id, backend_id) in backends.iter() {
                let key = format!("{}\t{}\t{}", prefix_key, cluster_id, backend_id);

                let res = self.metrics.get(&key);
                if res.is_none() {
                    //error!("unknown metric key {}", key);
                    continue;
                }
                let (meta, kind) = res.unwrap();

                if *meta == MetricMeta::Cluster {
                    error!("{} is a cluster metric", key);
                    continue;
                }

                if let Some(filtered_data) = self.query_metric(&key, true, timestamp, *kind) {
                    backend_data
                        .get_mut(cluster_id)
                        .unwrap()
                        .get_mut(backend_id)
                        .unwrap()
                        .insert(key.to_string(), filtered_data);
                }
            }
        }

        trace!("query result: {:#?}", backend_data);
        QueryAnswerMetrics::Backend(backend_data)
    }

    fn get_last_before(&self, start: &str, end: &str, is_backend: bool) -> Option<u64> {
        let tree = if is_backend {
            &self.backend_tree
        } else {
            &self.cluster_tree
        };

        //if let Some((k, v)) = tree.get_lt(end.as_bytes())? {
        if let Some((k, v)) = tree.range(start.to_string()..end.to_string()).rev().next() {
            if k.starts_with(start) {
                /*
                if is_backend {
                    let mut it = k.split(|c: &u8| *c == b'\t');
                    let key = std::str::from_utf8(it.next().unwrap()).unwrap();
                    let cluster_id = std::str::from_utf8(it.next().unwrap()).unwrap();
                    let backend_id = std::str::from_utf8(it.next().unwrap()).unwrap();
                    let timestamp:&str = std::str::from_utf8(it.next().unwrap()).unwrap();//.parse().unwrap();

                    let value = usize::from_le_bytes((*v).try_into().unwrap());
                    info!("looking at key = {}, id = {}, backend_id = {}, ts = {} -> {}",
                          key, cluster_id, backend_id, timestamp, value);
                } else {
                    info!("current key: {}", std::str::from_utf8(&k).unwrap());
                    let mut it = k.split(|c: &u8| *c == b'\t');
                    let key = std::str::from_utf8(it.next().unwrap()).unwrap();
                    let cluster_id = std::str::from_utf8(it.next().unwrap()).unwrap();
                    let timestamp:&str = std::str::from_utf8(it.next().unwrap()).unwrap();//.parse().unwrap();

                    let value = usize::from_le_bytes((*v).try_into().unwrap());
                    info!("looking at key = {}, id = {}, ts = {} -> {}",
                          key, cluster_id, timestamp, value);
                }*/

                return Some(*v);
            }
        }

        None
    }

    pub fn dump_cluster_data(&mut self) -> BTreeMap<String, AppMetricsData> {
        let mut apps = BTreeMap::new();

        /*
        for (key, (meta, kind)) in self.metrics.iter() {
            let end = format!("{}\x7F", key);

            match meta {
                MetricMeta::Cluster => {
                    for res in self.cluster_tree.range(key.as_bytes()..end.as_bytes()) {
                        let (k, v) = res?;

                        let mut it = k.split(|c: &u8| *c == b'\t');
                        let key = std::str::from_utf8(it.next().unwrap()).unwrap();
                        let app_id = std::str::from_utf8(it.next().unwrap()).unwrap();
                        //let timestamp:i64 = std::str::from_utf8(it.next().unwrap()).unwrap().parse().unwrap();
                        let timestamp = std::str::from_utf8(it.next().unwrap()).unwrap();//.parse().unwrap();

                        info!("looking at key = {}, id = {}, ts = {}",
                              key, app_id, timestamp);

                        let metrics_data = apps.entry(app_id.to_string()).or_insert_with(AppMetricsData::new);
                        match kind {
                            MetricKind::Gauge => {
                                /*if metrics_data.data.contains_key(key) {
                                    let v2 = metrics_data.data.get(key).unwrap().clone();
                                } else {*/
                                    metrics_data.data.insert(key.to_string(), FilteredData::Gauge(usize::from_le_bytes((*v).try_into().unwrap())));
                                //}
                            },
                            MetricKind::Count => {
                                /*if metrics_data.data.contains_key(key) {
                                    let v2 = metrics_data.data.get(key).unwrap().clone();
                                } else {*/
                                    metrics_data.data.insert(key.to_string(), FilteredData::Count(i64::from_le_bytes((*v).try_into().unwrap())));
                                //}
                            },
                            MetricKind::Time => {
                                //unimplemented for now
                            }
                        }
                    }
                },
                MetricMeta::ClusterBackend => {
                    for res in self.backend_tree.range(key.as_bytes()..end.as_bytes()) {
                        let (k, v) = res?;

                        let mut it = k.split(|c: &u8| *c == b'\t');
                        let key = std::str::from_utf8(it.next().unwrap()).unwrap();
                        let app_id = std::str::from_utf8(it.next().unwrap()).unwrap();
                        let backend_id = std::str::from_utf8(it.next().unwrap()).unwrap();
                        //let timestamp:i64 = std::str::from_utf8(it.next().unwrap()).unwrap().parse().unwrap();
                        let timestamp = std::str::from_utf8(it.next().unwrap()).unwrap();//.parse().unwrap();

                        info!("looking at key = {}, cluster id = {}, bid: {}, ts = {}",
                              key, app_id, backend_id, timestamp);

                        let app_metrics_data = apps.entry(app_id.to_string()).or_insert_with(AppMetricsData::new);
                        let backend_metrics_data = app_metrics_data.backends.entry(backend_id.to_string()).or_insert_with(BTreeMap::new);
                        match kind {
                            MetricKind::Gauge => {
                                /*if backend_metrics_data.contains_key(key) {
                                    let v2 = backend_metrics_data.get(key).unwrap().clone();
                                } else {*/
                                    backend_metrics_data.insert(key.to_string(), FilteredData::Gauge(usize::from_le_bytes((*v).try_into().unwrap())));
                                //}
                            },
                            MetricKind::Count => {
                                /*if backend_metrics_data.contains_key(key) {
                                    let v2 = backend_metrics_data.get(key).unwrap().clone();
                                } else {*/
                                    backend_metrics_data.insert(key.to_string(), FilteredData::Count(i64::from_le_bytes((*v).try_into().unwrap())));
                                //}
                            },
                            MetricKind::Time => {
                                //unimplemented for now
                            }
                        }
                    }
                },
            }
        }
        */

        // still clear the DB for now
        //self.db.clear();

        apps
    }

    fn receive_cluster_metric(
        &mut self,
        key: &str,
        cluster_id: &str,
        backend_id: Option<&str>,
        metric: MetricData,
    ) {
        if !self.enabled {
            return;
        }

        trace!(
            "metric: {} {} {:?} {:?}",
            key,
            cluster_id,
            backend_id,
            metric
        );

        if let MetricData::Time(t) = metric {
            self.store_time_metric(key, cluster_id, None, t);
            if backend_id.is_some() {
                self.store_time_metric(key, cluster_id, backend_id, t);
            }
        } else {
            self.store_metric(
                &format!("{}\t{}", key, cluster_id),
                cluster_id,
                None,
                &metric,
            );

            if let Some(bid) = backend_id {
                self.store_metric(
                    &format!("{}\t{}\t{}", key, cluster_id, bid),
                    cluster_id,
                    backend_id,
                    &metric,
                );
            }
        }
    }

    fn store_metric(
        &mut self,
        key_prefix: &str,
        id: &str,
        backend_id: Option<&str>,
        metric: &MetricData,
    ) {
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
        }

        match metric {
            MetricData::Gauge(i) => {
                self.store_gauge(&key_prefix, *i, backend_id.is_some());
            }
            MetricData::GaugeAdd(i) => {
                self.add_gauge(&key_prefix, *i, backend_id.is_some());
            }
            MetricData::Count(i) => {
                self.store_count(&key_prefix, *i, backend_id.is_some());
            }
            MetricData::Time(_) => {}
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

        trace!("store gauge at {} -> {}", complete_key, i);
        self.tree(is_backend).insert(complete_key, i as u64);

        // aggregate at the last hour
        let second = now.second();
        if second != 0 {
            let previous_minute = now - time::Duration::seconds(second as i64);
            let timestamp = previous_minute.unix_timestamp();

            let complete_key = format!("{}\t{}", key, timestamp);

            if !self.tree(is_backend).contains_key(&complete_key) {
                self.tree(is_backend).insert(complete_key, i as u64);
            }

            let minute = previous_minute.minute();
            if minute != 0 {
                let previous_hour = now - time::Duration::minutes(minute as i64);
                let timestamp = previous_hour.unix_timestamp();

                let complete_key = format!("{}\t{}", key, timestamp);
                if !self.tree(is_backend).contains_key(&complete_key) {
                    self.tree(is_backend).insert(complete_key, i as u64);
                }
            }
        }
    }

    fn add_gauge(&mut self, key: &str, i: i64, is_backend: bool) {
        let now = OffsetDateTime::now_utc();
        let timestamp = now.unix_timestamp();
        let complete_key = format!("{}\t{}", key, timestamp);

        trace!("add gauge at {} -> {}", complete_key, i);

        let value = match self.tree(is_backend).get(&complete_key) {
            Some(v) => *v as i64,
            // start from the last known value, or zero
            None => match self.get_last_before(key, &complete_key, is_backend) {
                None => 0i64,
                Some(v) => {
                    //i64::from_le_bytes((*v).try_into().unwrap())
                    v as i64
                }
            },
        };

        let new_value = value + i;
        self.tree(is_backend).insert(complete_key, new_value as u64);

        // aggregate at the last hour
        let second = now.second();
        if second != 0 {
            let previous_minute = now - time::Duration::seconds(second as i64);
            let timestamp = previous_minute.unix_timestamp();

            let complete_key = format!("{}\t{}", key, timestamp);

            let value = match self.tree(is_backend).get(&complete_key) {
                Some(v) => *v as i64,
                // start from the last known value, or zero
                None => match self.get_last_before(key, &complete_key, is_backend) {
                    None => 0i64,
                    Some(v) => {
                        //i64::from_le_bytes((*v).try_into().unwrap())
                        v as i64
                    }
                },
            };

            let new_value = value + i;
            self.tree(is_backend).insert(complete_key, new_value as u64);

            let minute = previous_minute.minute();
            if minute != 0 {
                let previous_hour = now - time::Duration::minutes(minute as i64);
                let timestamp = previous_hour.unix_timestamp();

                let complete_key = format!("{}\t{}", key, timestamp);

                let value = match self.tree(is_backend).get(&complete_key) {
                    Some(v) => *v as i64,
                    // start from the last known value, or zero
                    None => match self.get_last_before(key, &complete_key, is_backend) {
                        None => 0i64,
                        Some(v) => v as i64,
                    },
                };

                let new_value = value + i;
                self.tree(is_backend).insert(complete_key, new_value as u64);
            }
        }
    }

    fn store_count(&mut self, key: &str, i: i64, is_backend: bool) {
        let now = OffsetDateTime::now_utc();
        let timestamp = now.unix_timestamp();
        let complete_key = format!("{}\t{}", key, timestamp);

        trace!("store count at {} -> {}", complete_key, i);

        match self.tree(is_backend).get_mut(&complete_key) {
            None => {
                self.tree(is_backend).insert(complete_key, i as u64);
            }
            Some(v) => {
                *v = *v + i as u64;
            }
        };

        // aggregate at the last hour
        let second = now.second();
        if second != 0 {
            let previous_minute = now - time::Duration::seconds(second as i64);
            let timestamp = previous_minute.unix_timestamp();

            let complete_key = format!("{}\t{}", key, timestamp);
            match self.tree(is_backend).get_mut(&complete_key) {
                None => {
                    self.tree(is_backend).insert(complete_key, i as u64);
                }
                Some(v) => {
                    *v = *v + i as u64;
                }
            };

            let minute = previous_minute.minute();
            if minute != 0 {
                let previous_hour = now - time::Duration::minutes(minute as i64);
                let timestamp = previous_hour.unix_timestamp();

                let complete_key = format!("{}\t{}", key, timestamp);
                match self.tree(is_backend).get_mut(&complete_key) {
                    None => {
                        self.tree(is_backend).insert(complete_key, i as u64);
                    }
                    Some(v) => {
                        *v = *v + i as u64;
                    }
                };
            }
        }
    }

    fn clear_gauge(&mut self, key: &str, now: OffsetDateTime, is_backend: bool) {
        let timestamp = now.unix_timestamp();
        let one_hour_ago = format!("{}\t{}", key, timestamp - 3600);
        let one_minute_ago = format!("{}\t{}", key, timestamp - 60);
        let now_key = format!("{}\t{}", key, timestamp);

        let tree = if is_backend {
            &mut self.backend_tree
        } else {
            &mut self.cluster_tree
        };

        let mut to_remove = Vec::new();
        // aggregate 60 measures in a point at the last minute
        for (k, v) in tree.range(one_minute_ago.clone()..now_key) {
            debug!("removing {} -> {:?}", k, v);

            to_remove.push(k.to_string());
        }

        // aggregate 60 measures in a point at the last hour
        if now.minute() == 0 {
            for (k, v) in tree.range(one_hour_ago..one_minute_ago) {
                debug!("removing {} -> {:?}", &k, v);
                to_remove.push(k.to_string());
            }

            // remove all measures older than 24h
            let one_day_ago = format!("{}\t{}", key, timestamp - 3600 * 24);
            for (k, v) in tree.range(key.to_string()..one_day_ago) {
                debug!("removing {} -> {:?} (more than 24h)", k, v);
                to_remove.push(k.to_string());
            }
        }

        for k in to_remove.drain(..) {
            tree.remove(&k);
        }
    }

    /*fn clear_count(&mut self, key: &str, now: OffsetDateTime, is_backend: bool) -> Result<(), sled::Error> {
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
        for res in tree.range(one_minute_ago.as_bytes()..now_key.as_bytes()) {
            let (k, v) = res?;
            debug!("removing {} -> {:?}", unsafe { std::str::from_utf8_unchecked(&k) }, u64::from_le_bytes((*v).try_into().unwrap()));
            tree.remove(k)?;
        }

        // remove all measures older than 24h
        if now.minute() == 0 {
            for res in tree.range(one_hour_ago.as_bytes()..one_minute_ago.as_bytes()) {
                let (k, v) = res?;
                debug!("removing {} -> {:?}", unsafe { std::str::from_utf8_unchecked(&k) }, u64::from_le_bytes((*v).try_into().unwrap()));
                tree.remove(k)?;
            }

            // remove all measures older than 24h
            let one_day_ago = format!("{}\t{}", key, timestamp - 3600 * 24);
            for res in tree.range(key.as_bytes()..one_day_ago.as_bytes()) {
                let (k, v) = res?;
                debug!("removing {} -> {:?} (more than 24h)", unsafe { std::str::from_utf8_unchecked(&k) }, i64::from_le_bytes((*v).try_into().unwrap()));
                tree.remove(k)?;
            }
        }

        Ok(())
    }*/

    fn clear_time(&mut self, key: &str, now: OffsetDateTime, is_backend: bool) {
        self.clear_time_metric(&format!("{}.count", key), now, is_backend);
        self.clear_time_metric(&format!("{}.mean", key), now, is_backend);
        self.clear_time_metric(&format!("{}.var", key), now, is_backend);
        self.clear_time_metric(&format!("{}.p50", key), now, is_backend);
        self.clear_time_metric(&format!("{}.p90", key), now, is_backend);
        self.clear_time_metric(&format!("{}.p99", key), now, is_backend);
        self.clear_time_metric(&format!("{}.p99.9", key), now, is_backend);
        self.clear_time_metric(&format!("{}.p99.99", key), now, is_backend);
        self.clear_time_metric(&format!("{}.p99.999", key), now, is_backend);
        self.clear_time_metric(&format!("{}.p100", key), now, is_backend);
    }

    fn clear_time_metric(&mut self, key: &str, now: OffsetDateTime, is_backend: bool) {
        let timestamp = now.unix_timestamp();
        let one_hour_ago = format!("{}\t{}", key, timestamp - 3600);
        let one_minute_ago = format!("{}\t{}", key, timestamp - 60);
        let now_key = format!("{}\t{}", key, timestamp);

        let tree = if is_backend {
            &mut self.backend_tree
        } else {
            &mut self.cluster_tree
        };

        let mut to_remove = Vec::new();
        // aggregate 60 measures in a point at the last hour
        for (k, v) in tree.range(one_minute_ago.clone()..now_key) {
            debug!("removing {} -> {:?}", k, v);
            to_remove.push(k.clone());
        }

        // remove all measures older than 24h
        if now.minute() == 0 {
            for (k, v) in tree.range(one_hour_ago..one_minute_ago) {
                debug!("removing {} -> {:?}", k, v);
                to_remove.push(k.clone());
            }

            // remove all measures older than 24h
            let one_day_ago = format!("{}\t{}", key, timestamp - 3600 * 24);
            for (k, v) in tree.range(key.to_string()..one_day_ago) {
                debug!("removing {} -> {:?} (more than 24h)", k, v);
                to_remove.push(k.clone());
            }
        }

        for k in to_remove.drain(..) {
            tree.remove(&k);
        }
    }

    fn store_time_metric(
        &mut self,
        key: &str,
        cluster_id: &str,
        backend_id: Option<&str>,
        t: usize,
    ) {
        if !self.time_enabled {
            return;
        }

        let now = OffsetDateTime::now_utc();
        //let timestamp = now.unix_timestamp();
        //let _res = self.store_time_metric_at(key, cluster_id, backend_id, timestamp, t)?;

        let second = now.second();
        // we also aggregate at second zero
        //if second != 0 {
        let previous_minute = now - time::Duration::seconds(second as i64);
        let timestamp = previous_minute.unix_timestamp();
        let _res = self.store_time_metric_at(key, cluster_id, backend_id, timestamp, t);
        //} else {
        //}
    }

    fn store_time_metric_at(
        &mut self,
        key: &str,
        cluster_id: &str,
        backend_id: Option<&str>,
        timestamp: i64,
        t: usize,
    ) {
        let key_prefix = if let Some(bid) = backend_id {
            format!("{}\t{}\t{}", key, cluster_id, bid)
        } else {
            format!("{}\t{}", key, cluster_id)
        };

        if !self.metrics.contains_key(&key_prefix) {
            let meta = if backend_id.is_some() {
                MetricMeta::ClusterBackend
            } else {
                MetricMeta::Cluster
            };

            self.metrics
                .insert(key_prefix.to_string(), (meta, MetricKind::Time));
        }

        let tree = if backend_id.is_some() {
            &mut self.backend_tree
        } else {
            &mut self.cluster_tree
        };

        let count_key = format!("{}.count \t{}", key_prefix, timestamp);
        let mean_key = format!("{}.mean \t{}", key_prefix, timestamp);
        let var_key = format!("{}.var \t{}", key_prefix, timestamp);
        let p50_key = format!("{}.p50 \t{}", key_prefix, timestamp);
        let p90_key = format!("{}.p90 \t{}", key_prefix, timestamp);
        let p99_key = format!("{}.p99 \t{}", key_prefix, timestamp);
        let p99_9_key = format!("{}.p99.9 \t{}", key_prefix, timestamp);
        let p99_99_key = format!("{}.p99.99 \t{}", key_prefix, timestamp);
        let p99_999_key = format!("{}.p99.999 \t{}", key_prefix, timestamp);
        let p100_key = format!("{}.p100 \t{}", key_prefix, timestamp);

        match tree.get(&count_key) {
            None => {
                tree.insert(count_key, 1u64);
                tree.insert(mean_key, t as u64);
                tree.insert(var_key, 0u64);
                tree.insert(p50_key, t as u64);
                tree.insert(p90_key, t as u64);
                tree.insert(p99_key, t as u64);
                tree.insert(p99_9_key, t as u64);
                tree.insert(p99_99_key, t as u64);
                tree.insert(p99_999_key, t as u64);
                tree.insert(p100_key, t as u64);
            }
            Some(v) => {
                let old_count = *v;
                tree.insert(count_key, old_count + 1);

                let mean = if let Some(mean_v) = tree.get_mut(&mean_key) {
                    let old_mean = *mean_v as f64;
                    let new_mean =
                        (old_mean * old_count as f64 + t as f64) / (old_count as f64 + 1f64);

                    *mean_v = new_mean.floor() as u64;
                    new_mean
                } else {
                    return;
                };

                let standard_dev = if let Some(var_v) = tree.get_mut(&var_key) {
                    let old_var = *var_v as f64;
                    let deviation = t as f64 - mean;
                    let new_var = (old_var * old_count as f64 + deviation * deviation)
                        / (old_count as f64 + 1f64);
                    *var_v = new_var.floor() as u64;

                    new_var.sqrt()
                } else {
                    return;
                };

                if let Some(old_v) = tree.get(&p50_key) {
                    let new_percentile = calculate_percentile(
                        *old_v as usize,
                        t,
                        standard_dev,
                        0.50f64,
                        );
                    tree.insert(p50_key, new_percentile as u64);
                }

                if let Some(old_v) = tree.get(&p90_key) {
                    let new_percentile = calculate_percentile(
                        *old_v as usize,
                        t,
                        standard_dev,
                        0.90f64,
                        );
                    tree.insert(p90_key, new_percentile as u64);
                }

                if let Some(old_v) = tree.get(&p99_key) {
                    let new_percentile = calculate_percentile(
                        *old_v as usize,
                        t,
                        standard_dev,
                        0.99f64,
                        );
                    tree.insert(p99_key, new_percentile as u64);
                }

                if let Some(old_v) = tree.get(&p99_9_key) {
                    let new_percentile = calculate_percentile(
                        *old_v as usize,
                        t,
                        standard_dev,
                        0.999f64,
                        );
                    tree.insert(p99_9_key, new_percentile as u64);
                }

                if let Some(old_v) = tree.get(&p99_99_key) {
                    let new_percentile = calculate_percentile(
                        *old_v as usize,
                        t,
                        standard_dev,
                        0.9999f64,
                        );
                    tree.insert(p99_99_key, new_percentile as u64);
                }

                if let Some(old_v) = tree.get(&p99_999_key) {
                    let new_percentile = calculate_percentile(
                        *old_v as usize,
                        t,
                        standard_dev,
                        0.99999f64,
                        );
                    tree.insert(p99_999_key, new_percentile as u64);
                }

                if let Some(old_v) = tree.get(&p100_key) {
                    // the 100 percentile is the largest value
                    if t as u64 > *old_v {
                        tree.insert(p100_key, t as u64);
                    }
                }
            }
        };
    }

    pub fn clear(&mut self, now: OffsetDateTime) {
        if !self.enabled {
            return;
        }

        info!(
            "will clear old data from the metrics database ({} points)",
            self.cluster_tree.len() + self.backend_tree.len()
        );

        /*
        info!("current keys:");
        if let (Some(first), Some(second)) = (self.cluster_tree.first()?, self.cluster_tree.last()?) {
          for res in self.cluster_tree.range(first.0..second.0) {
              let (k, v) = res?;
              info!("{} -> {:?}", unsafe { std::str::from_utf8_unchecked(&k) }, u64::from_le_bytes((*v).try_into().unwrap()));

          }
        }
        if let (Some(first), Some(second)) = (self.backend_tree.first()?, self.backend_tree.last()?) {
          for res in self.backend_tree.range(first.0..second.0) {
              let (k, v) = res?;
              info!("{} -> {:?}", unsafe { std::str::from_utf8_unchecked(&k) }, u64::from_le_bytes((*v).try_into().unwrap()));

          }
        }*/

        let metrics = self.metrics.clone();
        for (key, (meta, kind)) in metrics.iter() {
            info!("will aggregate metrics for key '{}'", key);

            let is_backend = *meta == MetricMeta::ClusterBackend;
            match kind {
                MetricKind::Gauge => {
                    self.clear_gauge(key, now, is_backend);
                }
                MetricKind::Count => {
                    self.clear_gauge(key, now, is_backend);
                }
                MetricKind::Time => {
                    self.clear_time(key, now, is_backend);
                }
            }
        }

        /*
        info!("remaining keys:");
        if let (Some(first), Some(second)) = (self.cluster_tree.first()?, self.cluster_tree.last()?) {
          for res in self.cluster_tree.range(first.0..second.0) {
              let (k, v) = res?;
              info!("{} -> {:?}", unsafe { std::str::from_utf8_unchecked(&k) }, u64::from_le_bytes((*v).try_into().unwrap()));

          }
        }
        if let (Some(first), Some(second)) = (self.backend_tree.first()?, self.backend_tree.last()?) {
          for res in self.backend_tree.range(first.0..second.0) {
              let (k, v) = res?;
              info!("{} -> {:?}", unsafe { std::str::from_utf8_unchecked(&k) }, u64::from_le_bytes((*v).try_into().unwrap()));

          }
        }

        info!("db size({} points): {:?} bytes",
          self.cluster_tree.len() + self.backend_tree.len(),
          self.db.size_on_disk());
          */
        info!(
            "db size({} points)",
            self.cluster_tree.len() + self.backend_tree.len()
        );
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
        if let Some(id) = cluster_id {
            self.receive_cluster_metric(key, id, backend_id, metric);
        } else if !self.data.contains_key(key) {
            self.data
                .insert(String::from(key), AggregatedMetric::new(metric));
        } else {
            self.data.get_mut(key).map(|stored_metric| {
                stored_metric.update(key, metric);
            });
        }
    }
}

// implementation of an algorithm from https://mjambon.com/2016-07-23-moving-percentile/
fn calculate_percentile(
    old_value: usize,
    measure: usize,
    standard_deviation: f64,
    percentile: f64,
) -> usize {
    // to be adated can be between 0.01 and 0.001
    let r = 0.01f64;
    let delta = standard_deviation * r;

    if measure == old_value {
        old_value
    } else if measure < old_value {
        let new_value = old_value as f64 - delta / percentile;
        new_value as usize
    } else {
        let new_value = old_value as f64 + delta / (1f64 - percentile);
        new_value as usize
    }
}
