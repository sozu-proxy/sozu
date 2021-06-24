use std::str;
use std::time::{Duration,Instant};
use std::collections::VecDeque;
use std::net::SocketAddr;
use mio::net::UdpSocket;
use std::io::{Write,ErrorKind};
use std::collections::HashMap;
use super::writer::{MetricSocket, MetricsWriter};

use super::{Subscriber,MetricData,StoredMetricData};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug,Clone,PartialEq)]
pub struct MetricLine {
  label:      &'static str,
  app_id:     Option<String>,
  backend_id: Option<String>,
  /// in milliseconds
  duration:   usize,
}

pub struct NetworkDrain {
  queue:              VecDeque<MetricLine>,
  pub prefix:         String,
  pub remote:         MetricsWriter,
  is_writable:        bool,
  data:               HashMap<String, StoredMetricData>,
  /// (app_id, key) -> metric
  app_data:           HashMap<(String, String), StoredMetricData>,
  /// (app_id, backend_id, key) -> metric
  backend_data:       HashMap<(String, String, String), StoredMetricData>,
  pub use_tagged_metrics: bool,
  pub origin:         String,
  created:            Instant,
}

impl NetworkDrain {
  pub fn new(prefix: String, socket: UdpSocket, addr: SocketAddr) -> Self {
    socket.connect(addr).unwrap();

    NetworkDrain {
      queue:  VecDeque::new(),
      prefix,
      remote: MetricsWriter::new(MetricSocket {
        addr, socket
      }),
      is_writable: true,
      data: HashMap::new(),
      app_data: HashMap::new(),
      backend_data: HashMap::new(),
      use_tagged_metrics: false,
      origin: String::from("x"),
      created: Instant::now(),
    }
  }

  pub fn writable(&mut self) {
    self.is_writable = true;
  }

  pub fn send_data(&mut self) {
    let now  = Instant::now();
    let secs = Duration::new(1, 0);
    let mut send_count = 0;

    // remove metrics that were not touched in the last 10mn
    self.app_data.retain(|_, ref value| value.updated || now.duration_since(value.last_sent) < Duration::new(600, 00));
    self.backend_data.retain(|_, ref value| value.updated || now.duration_since(value.last_sent) < Duration::new(600, 00));

    if self.is_writable {

    for (ref key, ref mut stored_metric) in self.data.iter_mut().filter(|&(_, ref value)| value.updated && now.duration_since(value.last_sent) > secs) {
      //info!("will write {} -> {:#?}", key, stored_metric);
      let res = match stored_metric.data {
        MetricData::Gauge(value) => {
          if self.use_tagged_metrics {
            self.remote.write_fmt(format_args!("{}.{},origin={},version={}:{}|g\n", self.prefix, key, self.origin, VERSION, value))
          } else {
            self.remote.write_fmt(format_args!("{}.{}.{}:{}|g\n", self.prefix, self.origin, key, value))
          }
        },
        MetricData::Count(value) => {
          if value == 0 {
            stored_metric.last_sent = now;
          }

          let res = if self.use_tagged_metrics {
            self.remote.write_fmt(format_args!("{}.{},origin={},version={}:{}|c\n", self.prefix, key, self.origin, VERSION, value))
          } else {
            self.remote.write_fmt(format_args!("{}.{}.{}:{}|c\n", self.prefix, self.origin, key, value))
          };

          if res.is_ok() {
            stored_metric.data = MetricData::Count(0);
          }

          res
        },
        _ => {Ok(())}
      };

      match res {
        Ok(()) => {
          stored_metric.last_sent = now;
          stored_metric.updated = false;

          send_count += 1;
          if send_count >= 10 {
            /*if let Err(e) = self.remote.flush() {
              error!("error flushing metrics socket: {:?}", e);
            }*/
            send_count = 0;
          }
        },
        Err(e) => match e.kind() {
          ErrorKind::WriteZero => {
            if let Err(e) = self.remote.flush() {
              error!("error flushing metrics socket: {:?}", e);
            }
          },
          ErrorKind::WouldBlock => {
            error!("WouldBlock while writing global metrics to socket");
            self.is_writable = false;
            break;
          },
          e => {
            error!("metrics socket write error={:?}", e);
            break;
          },
        }
      }
    }
    }

    /*if let Err(e) = self.remote.flush() {
      error!("error flushing metrics socket: {:?}", e);
    }*/

    if self.is_writable {
    for (ref key, ref mut stored_metric) in self.app_data.iter_mut().filter(|&(_, ref value)| value.updated && now.duration_since(value.last_sent) > secs) {
      //info!("will write {:?} -> {:#?}", key, stored_metric);
      let res = match stored_metric.data {
        MetricData::Gauge(value) => {
          if self.use_tagged_metrics {
            self.remote.write_fmt(format_args!("{}.app.{},origin={},version={},app_id={}:{}|g\n",
              self.prefix, key.1, self.origin, VERSION, key.0, value))
          } else {
            self.remote.write_fmt(format_args!("{}.{}.app.{}.{}:{}|g\n", self.prefix, self.origin, key.0, key.1, value))
          }
        },
        MetricData::Count(value) => {
          if value == 0 {
            stored_metric.last_sent = now;
          }

          let res = if self.use_tagged_metrics {
            self.remote.write_fmt(format_args!("{}.app.{},origin={},version={},app_id={}:{}|c\n",
              self.prefix, key.1, self.origin, VERSION, key.0, value))
          } else {
            self.remote.write_fmt(format_args!("{}.{}.app.{}.{}:{}|c\n", self.prefix, self.origin, key.0, key.1, value))
          };

          if res.is_ok() {
            stored_metric.data = MetricData::Count(0);
          }

          res
        },
        _ => {Ok(())}
      };

      match res {
        Ok(()) => {
          stored_metric.last_sent = now;
          stored_metric.updated = false;

          send_count += 1;
          if send_count >= 10 {
            /*if let Err(e) = self.remote.flush() {
              error!("error flushing metrics socket: {:?}", e);
            }*/
            send_count = 0;
          }
        },
        Err(e) => match e.kind() {
          ErrorKind::WriteZero => {
            if let Err(e) = self.remote.flush() {
              error!("error flushing metrics socket: {:?}", e);
            }
          },
          ErrorKind::WouldBlock => {
            error!("WouldBlock while writing app metrics to socket");
            self.is_writable = false;
            break;
          },
          e => {
            error!("metrics socket write error={:?}", e);
            break;
          },
        }
      }
    }
    }

    /*if let Err(e) = self.remote.flush() {
      error!("error flushing metrics socket: {:?}", e);
    }*/

    if self.is_writable {
    for (ref key, ref mut stored_metric) in self.backend_data.iter_mut().filter(|&(_, ref value)| value.updated && now.duration_since(value.last_sent) > secs) {
      //info!("will write {:?} -> {:#?}", key, stored_metric);
      let res = match stored_metric.data {
        MetricData::Gauge(value) => {
          if self.use_tagged_metrics {
            self.remote.write_fmt(format_args!("{}.backend.{},origin={},version={},app_id={},backend_id={}:{}|g\n",
              self.prefix, key.2, self.origin, VERSION, key.0, key.1, value))
          } else {
            self.remote.write_fmt(format_args!("{}.{}.app.{}.backend.{}.{}:{}|g\n", self.prefix, self.origin, key.0, key.1, key.2, value))
          }
        },
        MetricData::Count(value) => {
          if value == 0 {
            stored_metric.last_sent = now;
          }

          let res = if self.use_tagged_metrics {
            self.remote.write_fmt(format_args!("{}.backend.{},origin={},version={},app_id={},backend_id={}:{}|c\n",
              self.prefix, key.2, self.origin, VERSION, key.0, key.1, value))
          } else {
            self.remote.write_fmt(format_args!("{}.{}.app.{}.backend.{}.{}:{}|c\n", self.prefix, self.origin, key.0, key.1, key.2, value))
          };

          if res.is_ok() {
            stored_metric.data = MetricData::Count(0);
          }

          res
        },
        _ => {Ok(())}
      };

      match res {
        Ok(()) => {
          stored_metric.last_sent = now;
          stored_metric.updated = false;

          send_count += 1;
          if send_count >= 10 {
            /*if let Err(e) = self.remote.flush() {
              error!("error flushing metrics socket: {:?}", e);
            }*/
            send_count = 0;
          }
        },
        Err(e) => match e.kind() {
          ErrorKind::WriteZero => {
            if let Err(e) =self.remote.flush() {
              error!("error flushing metrics socket: {:?}", e);
            }
          },
          ErrorKind::WouldBlock => {
            error!("WouldBlock while writing backend metrics to socket");
            self.is_writable = false;
            break;
          },
          e => {
            error!("metrics socket write error={:?}", e);
            break;
          },
        }
      }
    }
    }
    /*if let Err(e) = self.remote.flush() {
      error!("error flushing metrics socket: {:?}", e);
    }*/

    if self.is_writable {
    for metric in self.queue.drain(..) {
       let res = match (metric.app_id, metric.backend_id) {
        (Some(app_id), Some(backend_id)) => {
          if self.use_tagged_metrics {
            self.remote.write_fmt(format_args!("{}.backend.{},origin={},version={},app_id={},backend_id={}:{}|ms\n",
              self.prefix, metric.label, self.origin, VERSION, app_id, backend_id, metric.duration))
          } else {
            self.remote.write_fmt(format_args!("{}.{}.app.{}.backend.{}.{}:{}|ms\n", self.prefix, self.origin, app_id, backend_id,
              metric.label, metric.duration))
          }
        },
        (Some(app_id), None) => {
          if self.use_tagged_metrics {
            self.remote.write_fmt(format_args!("{}.app.{},origin={},version={},app_id={}:{}|ms\n",
              self.prefix, metric.label, self.origin, VERSION, app_id, metric.duration))
          } else {
            self.remote.write_fmt(format_args!("{}.{}.app.{}.{}:{}|ms\n", self.prefix, self.origin, app_id,
              metric.label, metric.duration))
          }
        },
        (None, None) => {
          if self.use_tagged_metrics {
            self.remote.write_fmt(format_args!("{}.{},origin={},version={}:{}|ms\n",
              self.prefix, metric.label, self.origin, VERSION, metric.duration))
          } else {
            self.remote.write_fmt(format_args!("{}.{}.{}:{}|ms\n", self.prefix, self.origin,
              metric.label, metric.duration))
          }
        },
        _ => { Ok(()) }
      };

      match res {
        Ok(()) => {
          send_count += 1;
          if send_count >= 10 {
            /*if let Err(e) = self.remote.flush() {
              error!("error flushing metrics socket: {:?}", e);
            }*/
            send_count = 0;
          }
        },
        Err(e) => match e.kind() {
          ErrorKind::WriteZero => {
            if let Err(e) = self.remote.flush() {
              error!("error flushing metrics socket: {:?}", e);
            }
          },
          ErrorKind::WouldBlock => {
            error!("WouldBlock while writing timing metrics to socket");
            self.is_writable = false;
            break;
          },
          e => {
            error!("metrics socket write error={:?}", e);
            break;
          },
        }
      }
    }

    /*if let Err(e) = self.remote.flush() {
      error!("error flushing metrics socket: {:?}", e);
    }*/
    }
  }
}


impl Subscriber for NetworkDrain {
  fn receive_metric(&mut self, key: &'static str, app_id: Option<&str>, backend_id: Option<&str>, metric: MetricData) {
    if metric.is_time() {
      if let MetricData::Time(millis) = metric {
        self.queue.push_back(MetricLine {
          label: key,
          app_id: app_id.map(|s| s.to_string()),
          backend_id: backend_id.map(|s| s.to_string()),
          duration: millis,
        });
      }
    } else if let Some(id) = app_id {
      if let Some(bid) = backend_id {
        let k = (String::from(id), String::from(bid), String::from(key));
        if !self.backend_data.contains_key(&k) {
          self.backend_data.insert(
            k,
            StoredMetricData::new(self.created, metric)
            );
        } else {
          self.backend_data.get_mut(&k).map(|stored_metric| {
            stored_metric.update(key, metric);
          });
        }
      } else {
        let k = (String::from(id), String::from(key));
        if !self.app_data.contains_key(&k) {
          self.app_data.insert(
            k,
            StoredMetricData::new(self.created, metric)
            );
        } else {
          self.app_data.get_mut(&k).map(|stored_metric| {
            stored_metric.update(key, metric);
          });
        }
      }
    } else if !self.data.contains_key(key) {
      self.data.insert(
        String::from(key),
        StoredMetricData::new(self.created, metric)
        );
    } else {
      self.data.get_mut(key).map(|stored_metric| {
        stored_metric.update(key, metric);
      });
    }
  }
}

