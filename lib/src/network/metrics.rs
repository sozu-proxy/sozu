use std::net::{UdpSocket,SocketAddr};
use std::io::{self,Write,Error,ErrorKind};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;
use std::str;

use network::buffer::Buffer;

lazy_static! {
  pub static ref METRICS: Mutex<ProxyMetrics> = Mutex::new(ProxyMetrics::new(String::from("sozu")));
}

pub struct ProxyMetrics {
  buffer: Buffer,
  prefix: String,
  remote: Option<(SocketAddr, UdpSocket)>,
}

impl ProxyMetrics {
  pub fn new(prefix: String) -> Self {
    ProxyMetrics {
      buffer: Buffer::with_capacity(2048),
      prefix: prefix,
      remote:   None,
    }
  }

  pub fn run() -> thread::JoinHandle<()> {
    thread::spawn(move || {
      loop {
        thread::sleep(Duration::from_millis(500));
        METRICS.lock().unwrap().send();
      }
    })
  }

  pub fn set_up_remote(&mut self, socket: UdpSocket, addr: SocketAddr) {
    self.remote = Some((addr, socket));
  }

  pub fn send(&mut self) -> io::Result<usize> {
    if self.buffer.available_data() > 0 {
      if let Some((ref addr, ref socket)) = self.remote {
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
      }
    } else {
      Err(Error::new(ErrorKind::Other, "no data to send"))
    }
  }

  fn emit(&mut self, metric: &str) -> io::Result<usize> {
    self.buffer.write(metric.as_bytes())
  }


  pub fn count(&mut self, key: &str, count: i64) -> io::Result<()> {
    self.buffer.write_fmt(format_args!("{}.{}:{}|c\n", &self.prefix, key, count))
  }

  pub fn incr(&mut self, key: &str) -> io::Result<()> {
    self.count(key, 1)
  }

  pub fn decr(&mut self, key: &str) -> io::Result<()> {
    self.count(key, -1)
  }

  pub fn time(&mut self, key: &str, time: u64) -> io::Result<()> {
    self.buffer.write_fmt(format_args!("{}.{}:{}|ms\n", self.prefix, key, time))
  }

  pub fn gauge(&mut self, key: &str, value: u64) -> io::Result<()> {
    self.buffer.write_fmt(format_args!("{}.{}:{}|g\n", self.prefix, key, value))
  }

  pub fn meter(&mut self, key: &str, value: u64) -> io::Result<()> {
    self.buffer.write_fmt(format_args!("{}.{}:{}|m\n", self.prefix, key, value))
  }
}

#[macro_export]
macro_rules! count (
  ($key:expr, $value: expr) => (::network::metrics::METRICS.lock().unwrap().count($key, $value);)
);

#[macro_export]
macro_rules! incr (
  ($key:expr) => (::network::metrics::METRICS.lock().unwrap().incr($key);)
);

#[macro_export]
macro_rules! decr (
  ($key:expr) => (::network::metrics::METRICS.lock().unwrap().decr($key);)
);

#[macro_export]
macro_rules! time (
  ($key:expr, $value: expr) => (::network::metrics::METRICS.lock().unwrap().time($key, $value);)
);

#[macro_export]
macro_rules! gauge (
  ($key:expr, $value: expr) => (::network::metrics::METRICS.lock().unwrap().gauge($key, $value);)
);

#[macro_export]
macro_rules! meter (
  ($key:expr, $value: expr) => (::network::metrics::METRICS.lock().unwrap().meter($key, $value);)
);
