use std::str;
use std::thread;
use std::sync::Mutex;
use std::cell::RefCell;
use std::time::{Duration,Instant};
use std::collections::BTreeMap;
use std::fmt::Arguments;
use std::net::SocketAddr;
use mio::net::UdpSocket;
use std::io::{self,Write,Error,ErrorKind};
use nom::HexDisplay;

use network::buffer::Buffer;

thread_local! {
  pub static METRICS: RefCell<ProxyMetrics> = RefCell::new(ProxyMetrics::new(String::from("sozu")))
}

#[derive(Debug,Clone)]
pub enum MetricData {
  Gauge(usize),
  Count(i64),
  Time(Instant, Option<Instant>),
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FilteredData {
  Gauge(usize),
  Count(i64),
  Time(usize),
}

#[derive(Debug,Clone)]
pub struct StoredMetricData {
  last_sent: Instant,
  data:      MetricData,
}

#[derive(Debug)]
pub struct ProxyMetrics {
  pub buffer:      Buffer,
  pub prefix:      String,
  pub created:     Instant,
  pub data:        BTreeMap<String, StoredMetricData>,
  pub is_writable: bool,
  remote:          Option<(SocketAddr, UdpSocket)>,
}

impl ProxyMetrics {
  pub fn new(prefix: String) -> Self {
    ProxyMetrics {
      buffer:      Buffer::with_capacity(2048),
      prefix:      prefix,
      created:     Instant::now(),
      data:        BTreeMap::new(),
      remote:      None,
      is_writable: false,
    }
  }

  pub fn set_up_remote(&mut self, socket: UdpSocket, addr: SocketAddr) {
    self.remote = Some((addr, socket));
  }

  pub fn socket(&self) -> Option<&UdpSocket> {
    self.remote.as_ref().map(|remote| &remote.1)
  }

  pub fn write(&mut self, args: Arguments) {
    //FIXME: error handling
    self.buffer.write_fmt(args);
    self.send();
  }

  pub fn dump_data(&self) -> BTreeMap<String, FilteredData> {
    self.data.iter().filter(|&(_, ref value)| {
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
    }).collect()
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
    for (ref key, ref mut stored_metric) in self.data.iter_mut().filter(|&(_, ref value)| now.duration_since(value.last_sent) > secs) {
      //info!("will write {} -> {:#?}", key, stored_metric);
      match stored_metric.data {
        MetricData::Gauge(value) => {
          if self.buffer.write_fmt(format_args!("{}.{}:{}|g\n", self.prefix, key, value)).is_ok() {
            stored_metric.last_sent = now;
          } else {
            break;
          }
        },
        MetricData::Count(value) => {
          if self.buffer.write_fmt(format_args!("{}.{}:{}|g\n", self.prefix, key, value)).is_ok() {
            stored_metric.last_sent = now;
          } else {
            break;
          }
        },
        MetricData::Time(begin, Some(end)) => {
          let duration = end.duration_since(begin);
          let millis = duration.as_secs() * 1000 + (duration.subsec_nanos() / 1000000) as u64;
          if self.buffer.write_fmt(format_args!("{}.{}:{}|ms\n", self.prefix, key, millis)).is_ok() {
            //info!("wrote time to buffer:\n{}", str::from_utf8(self.buffer.data()).unwrap());
            stored_metric.last_sent = now;
          } else {
            break;
          }
        },
        _ => {}
      }
    }
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

#[macro_export]
macro_rules! metrics_set_up (
  ($host:expr, $port: expr) => {
    let metrics_socket = ::mio::net::UdpSocket::bind(&("0.0.0.0:0".parse().unwrap())).expect("could not parse address");
    info!("setting up metrics: local address = {:#?}", metrics_socket.local_addr());
    let metrics_host   = ($host, $port).to_socket_addrs().expect("could not parse address").next().expect("could not get first address");
    $crate::network::metrics::METRICS.with(|metrics| {
      (*metrics.borrow_mut()).set_up_remote(metrics_socket, metrics_host);
    });
  }
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
      info!("gauge {} -> {}", $key, v);
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

