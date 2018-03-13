use std::str;
use std::thread;
use std::sync::Mutex;
use std::cell::RefCell;
use std::time::{Duration,Instant};
use std::iter::repeat;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::fmt::Arguments;
use std::net::SocketAddr;
use mio::net::UdpSocket;
use std::io::{self,Write,Error,ErrorKind};
use nom::HexDisplay;
use hdrhistogram::Histogram;
use sozu_command::buffer::Buffer;
use sozu_command::messages::{FilteredData,MetricsData,Percentiles,BackendMetricsData,FilteredTimeSerie};

thread_local! {
  pub static METRICS: RefCell<ProxyMetrics> = RefCell::new(ProxyMetrics::new(String::from("sozu")))
}

#[derive(Debug,Clone)]
pub enum MetricData {
  Gauge(usize),
  Count(i64),
  Time(Instant, Option<Instant>),
}

#[derive(Debug,Clone)]
pub struct StoredMetricData {
  last_sent: Instant,
  data:      MetricData,
}

#[derive(Clone,Debug)]
pub struct AppMetrics {
  pub response_time: Histogram<u32>,
  pub last_sent:     Instant,
}

#[derive(Clone,Debug)]
pub struct BackendMetrics {
  pub response_time: Histogram<u32>,
  pub bin:           usize,
  pub bout:          usize,
  pub app_id:        String,
  pub last_sent:     Instant,
}

impl BackendMetrics {
  pub fn new(app_id: String, h: Histogram<u32>) -> BackendMetrics {
    BackendMetrics {
      response_time: h,
      bin:           0,
      bout:          0,
      app_id:        app_id,
      last_sent:     Instant::now(),
    }
  }
}

#[derive(Debug)]
pub struct ProxyMetrics {
  pub buffer:          Buffer,
  pub prefix:          String,
  pub created:         Instant,
  pub data:            BTreeMap<String, StoredMetricData>,
  /// app_id -> response time histogram (in ms)
  pub app_data:        BTreeMap<String,AppMetrics>,
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
      app_data:    BTreeMap::new(),
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

  pub fn write(&mut self, args: Arguments) {
    //FIXME: error handling
    self.buffer.write_fmt(args);
    self.send();
  }

  pub fn dump_data(&mut self) -> BTreeMap<String, FilteredData> {
    let mut data: BTreeMap<String, FilteredData> = self.data.iter().filter(|&(_, ref value)| {
      if let MetricData::Time(_,None) = value.data {
        false
      } else {
        true
      }
    }).map(|(ref key, ref value)| {
      let val = match value.data {
        MetricData::Gauge(i) => FilteredData::Gauge(i),
        MetricData::Count(i) => FilteredData::Count(i),
        MetricData::Time(begin, Some(end)) => {
          let duration = end.duration_since(begin);
          let millis = duration.as_secs() * 1000 + (duration.subsec_nanos() / 1000000) as u64;
          FilteredData::Time(millis as usize)
        },
        _ => unreachable!()
      };
      (key.to_string(), val)
    }).collect();

    data.insert(String::from("requests"), FilteredData::TimeSerie(self.request_counter.filtered()));

    data
  }

  pub fn dump_percentiles(&self) -> BTreeMap<String, Percentiles> {
    self.app_data.iter().map(|(ref app_id, ref metrics)| {
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

      (app_id.to_string(), percentiles)
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
      applications: self.dump_percentiles(),
      backends:     self.dump_backend_data(),
    }
  }

  pub fn count_add(&mut self, key: &str, count_value: i64) {
    if !self.data.contains_key(key) {
      self.data.insert(
        String::from(key),
        StoredMetricData {
          last_sent: self.created,
          data: MetricData::Count(count_value)
        }
      );
    } else {
      self.data.get_mut(key).map(|stored_metric| {
        if let MetricData::Count(value) = stored_metric.data {
          stored_metric.data = MetricData::Count(value+count_value);
        } else {
          panic!("tried to put a count value in an incompatible metric ({:?}) for key {}", stored_metric.data, key);
        }
      });
    }
  }

  pub fn set_gauge(&mut self, key: &str, gauge_value: usize) {
    if !self.data.contains_key(key) {
      self.data.insert(
        String::from(key),
        StoredMetricData {
          last_sent: self.created,
          data: MetricData::Gauge(gauge_value)
        }
      );
    } else {
      self.data.get_mut(key).map(|stored_metric| {
        if let MetricData::Gauge(_) = stored_metric.data {
          stored_metric.data = MetricData::Gauge(gauge_value);
        } else {
          panic!("tried to put a gauge value in an incompatible metric ({:?}) for key {}", stored_metric.data, key);
        }
      });
    }
  }

  pub fn set_time_begin(&mut self, key: &str) {
    let now = Instant::now();
    //info!("set time begin {} = {:#?}", key, now);
    if !self.data.contains_key(key) {
      self.data.insert(
        String::from(key),
        StoredMetricData {
          last_sent: self.created,
          data: MetricData::Time(now, None)
        }
      );
    } else {
      self.data.get_mut(key).map(|stored_metric| {
        if let MetricData::Time(_,_) = stored_metric.data {
          stored_metric.data = MetricData::Time(now, None);
        } else {
          panic!("tried to put a time value in an incompatible metric ({:?}) for key {}", stored_metric.data, key);
        }
      });
    }
  }

  pub fn set_time_end(&mut self, key: &str) {
    let now = Instant::now();
    //info!("set time end {} = {:#?}", key, now);
    if !self.data.contains_key(key) {
      panic!("tried to set the end time while the begin time was not set");
    } else {
      self.data.get_mut(key).map(|stored_metric| {
        if let MetricData::Time(begin,_) = stored_metric.data {
          stored_metric.data = MetricData::Time(begin, Some(now));
        } else {
          panic!("tried to put a time value in an incompatible metric ({:?}) for key {}", stored_metric.data, key);
        }
      });
    }
  }

  pub fn writable(&mut self) {
    self.is_writable = true;
  }

  pub fn fill_buffer(&mut self) {
    let now  = Instant::now();
    let secs = Duration::new(2, 0);

    //FIXME: fill app and backend data too
    if now.duration_since(self.request_counter.sent_at) > secs {
      let res = if self.use_tagged_metrics {
        self.buffer.write_fmt(format_args!("{}.{},origin={}:{}|g\n", self.prefix, "requests", self.origin, self.request_counter.last_sent))
      } else {
        self.buffer.write_fmt(format_args!("{}.{}.{}:{}|g\n", self.prefix, self.origin, "requests", self.request_counter.last_sent))
      };

      if res.is_ok() {
        self.request_counter.update_sent_at(now);
      }
    }

    for (ref key, ref mut stored_metric) in self.data.iter_mut().filter(|&(_, ref value)| now.duration_since(value.last_sent) > secs) {
      //info!("will write {} -> {:#?}", key, stored_metric);
      match stored_metric.data {
        MetricData::Gauge(value) => {
          let res = if self.use_tagged_metrics {
            self.buffer.write_fmt(format_args!("{}.{},origin={}:{}|g\n", self.prefix, key, self.origin, value))
          } else {
            self.buffer.write_fmt(format_args!("{}.{}.{}:{}|g\n", self.prefix, self.origin, key, value))
          };

          if res.is_ok() {
            stored_metric.last_sent = now;
          } else {
            break;
          }
        },
        MetricData::Count(value) => {
          let res = if self.use_tagged_metrics {
            self.buffer.write_fmt(format_args!("{}.{},origin={}:{}|g\n", self.prefix, key, self.origin, value))
          } else {
            self.buffer.write_fmt(format_args!("{}.{}.{}:{}|g\n", self.prefix, self.origin, key, value))
          };

          if res.is_ok() {
            stored_metric.last_sent = now;
          } else {
            break;
          }
        },
        MetricData::Time(begin, Some(end)) => {
          let duration = end.duration_since(begin);
          let millis = duration.as_secs() * 1000 + (duration.subsec_nanos() / 1000000) as u64;

          let res = if self.use_tagged_metrics {
            self.buffer.write_fmt(format_args!("{}.{},origin={}:{}|ms\n", self.prefix, key, self.origin, millis))
          } else {
            self.buffer.write_fmt(format_args!("{}.{}.{}:{}|ms\n", self.prefix, self.origin, key, millis))
          };

          if res.is_ok() {
            //info!("wrote time to buffer:\n{}", str::from_utf8(self.buffer.data()).unwrap());
            stored_metric.last_sent = now;
          } else {
            break;
          }
        },
        _ => {}
      }
    }

    for (ref key, ref mut bm) in self.backend_data.iter_mut()
      .filter(|&(_,ref bm)| now.duration_since(bm.last_sent) > secs) {

      let bytes_in_res = if self.use_tagged_metrics {
        self.buffer.write_fmt(format_args!("{}.backend.{},origin={},backend_id={}:{}|g\n", self.prefix, "bytes_in", self.origin, key, bm.bin))
      } else {
        self.buffer.write_fmt(format_args!("{}.{}.backend.{}.{}:{}|g\n", self.prefix, self.origin, key, "bytes_in", bm.bin))
      };

      let bytes_out_res = if self.use_tagged_metrics {
        self.buffer.write_fmt(format_args!("{}.backend.{},origin={},backend_id={}:{}|g\n", self.prefix, "bytes_out", self.origin, key, bm.bout))
      } else {
        self.buffer.write_fmt(format_args!("{}.{}.backend.{}.{}:{}|g\n", self.prefix, self.origin, key, "bytes_out", bm.bout))
      };
      if bytes_in_res.is_ok() && bytes_out_res.is_ok() {
        bm.last_sent = now;
      } else {
        break;
      }
    };
  }

  pub fn send_data(&mut self) {
    let mut written = 0usize;
    loop {
      let buflen = self.buffer.available_data();
      self.fill_buffer();
      if self.buffer.available_data() == 0 {
        break;
      }

      if let Some((ref addr, ref socket)) = self.remote {
        if self.is_writable {
          match socket.send_to(self.buffer.data(), addr) {
            Ok(sz) => {
              self.buffer.consume(sz);
              written += sz;
            },
            Err(e) => match e.kind() {
              ErrorKind::WouldBlock => {
                self.is_writable = false;
                break;
              },
              e => {
                error!("metrics socket write error={:?}", e);
                break;
              },
            }
          }
        } else {
          break;
        }
      } else {
        break;
      }

      if buflen == self.buffer.available_data() {
        break;
      }
    }
  }

  pub fn send(&mut self) -> io::Result<usize> {
    //let res = if self.buffer.available_data() >= 512 {
    let res = if let Some((ref addr, ref socket)) = self.remote {
        match socket.send_to(self.buffer.data(), addr) {
          Ok(sz) => {
            self.buffer.consume(sz);
            Ok(sz)
          },
          Err(e) => {
            Err(e)
          }
        }
      } else {
        Err(Error::new(ErrorKind::NotConnected, "metrics socket not set up"))
      };
    //} else {
    //  Err(Error::new(ErrorKind::Other, "no data to send"))
    //};

    res
  }
}

pub fn udp_bind() -> UdpSocket {
  UdpSocket::bind(&("0.0.0.0:0".parse().unwrap())).expect("could not parse address")
}

#[macro_export]
macro_rules! metrics_set_up (
  ($host:expr, $port: expr, $origin: expr, $use_tagged_metrics: expr) => ({
    use std::net::ToSocketAddrs;
    let metrics_socket = $crate::network::metrics::udp_bind();

    debug!("setting up metrics: local address = {:#?}", metrics_socket.local_addr());
    let metrics_host   = ($host, $port).to_socket_addrs().expect("could not parse address").next().expect("could not get first address");
    $crate::network::metrics::METRICS.with(|metrics| {
      (*metrics.borrow_mut()).set_up_remote(metrics_socket, metrics_host);
      (*metrics.borrow_mut()).set_up_origin($origin);
      (*metrics.borrow_mut()).set_up_tagged_metrics($use_tagged_metrics);
    });
  })
);

#[macro_export]
macro_rules! count (
  ($key:expr, $value: expr) => {
    let v = $value;
    $crate::network::metrics::METRICS.with(|metrics| {
      (*metrics.borrow_mut()).count_add($key, v);
    });
  }
);

#[macro_export]
macro_rules! incr (
  ($key:expr) => (count!($key, 1);)
);

#[macro_export]
macro_rules! decr (
  ($key:expr) => (count!($key, -1);)
);

#[macro_export]
macro_rules! gauge (
  ($key:expr, $value: expr) => {
    let v = $value;
    $crate::network::metrics::METRICS.with(|metrics| {
      //(*metrics.borrow_mut()).write(format_args!("{}.{}:{}|g\n", *$crate::logging::TAG, $key, v));
      (*metrics.borrow_mut()).set_gauge($key, v);
    });
  }
);

#[macro_export]
macro_rules! time_begin (
  ($key:expr) => {
    $crate::network::metrics::METRICS.with(|metrics| {
      (*metrics.borrow_mut()).set_time_begin($key);
    });
  }
);

#[macro_export]
macro_rules! time_end (
  ($key:expr) => {
    $crate::network::metrics::METRICS.with(|metrics| {
      (*metrics.borrow_mut()).set_time_end($key);
    });
  }
);

#[macro_export]
macro_rules! record_request_time (
  ($app_id:expr, $value: expr) => {
    let v = $value;
    $crate::network::metrics::METRICS.with(|metrics| {
      let ref mut m = *metrics.borrow_mut();
      let key: &str = $app_id;

      if m.app_data.contains_key(key) {
        let metrics = m.app_data.get_mut(key).unwrap();
        metrics.response_time.record($value as u64);
      } else {
        if let Ok(mut hist) = ::hdrhistogram::Histogram::new(3) {
          hist.record($value as u64);
          let metrics = $crate::network::metrics::AppMetrics {
            response_time: hist,
            last_sent: ::std::time::Instant::now(),
          };
          m.app_data.insert(key.to_string(), metrics);
        }
      }
    });
  }
);

#[macro_export]
macro_rules! record_backend_metrics (
  ($app_id:expr, $backend_id:expr, $response_time: expr, $bin: expr, $bout: expr) => {
    $crate::network::metrics::METRICS.with(|metrics| {
      let ref mut m = *metrics.borrow_mut();
      let key: &str = $backend_id;

      if m.backend_data.contains_key(key) {
        let bm = m.backend_data.get_mut(key).unwrap();
        bm.response_time.record($response_time as u64);
        bm.bin += $bin;
        bm.bout += $bout;
      } else {
        if let Ok(hist) = ::hdrhistogram::Histogram::new(3) {
          let mut bm = $crate::network::metrics::BackendMetrics::new($app_id.clone(), hist);
          bm.response_time.record($response_time as u64);
          bm.bin += $bin;
          bm.bout += $bout;
          m.backend_data.insert(key.to_string(), bm);
        }
      }
    });
  }
);

#[macro_export]
macro_rules! remove_app_metrics (
  ($app_id:expr) => {
    $crate::network::metrics::METRICS.with(|metrics| {
      let ref mut m = *metrics.borrow_mut();
      let key: &str = $app_id;
      m.app_data.remove(key);
    });
  }
);

#[macro_export]
macro_rules! remove_backend_metrics (
  ($backend_id:expr) => {
    $crate::network::metrics::METRICS.with(|metrics| {
      let ref mut m = *metrics.borrow_mut();
      let key: &str = $backend_id;
      m.backend_data.remove(key);
    });
  }
);

///Client-side request errors caused by:
///
/// * Client terminates before sending request
/// * Read error from client
/// * Client timeout
/// * Client terminated connection
#[macro_export]
macro_rules! incr_ereq (
  () => (incr!("ereq");)
);

#[macro_export]
macro_rules! incr_client_cmd (
  () => (incr!("client_cmd");)
);

#[macro_export]
macro_rules! incr_resp_client_cmd (
  () => (incr!("incr_resp_client_cmd");)
);

/// count another accepted request
#[macro_export]
macro_rules! incr_req (
  () => {
    $crate::network::metrics::METRICS.with(|metrics| {
      let ref mut m = *metrics.borrow_mut();
      m.request_counter.increment();
    });
  }
);

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

  pub fn increment(&mut self) {
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

      self.last_second = 1;
    } else {
      self.last_second += 1;
    }

    self.last_sent += 1;
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
