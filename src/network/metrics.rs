use std::net::{UdpSocket,SocketAddr};
use std::io::{self,Write,Error,ErrorKind};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;
use std::str;

use network::buffer::Buffer;

lazy_static! {
  pub static ref METRICS: Mutex<ProxyMetrics> = Mutex::new(ProxyMetrics::new(String::from("yxorp")));
}

pub struct ProxyMetrics {
  buffer: Buffer,
  prefix: String,
  remote: Option<(SocketAddr, UdpSocket)>,
}

impl ProxyMetrics {
  pub fn new(prefix: String) -> Self {
    ProxyMetrics {
      buffer: Buffer::with_capacity(500),
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


  pub fn count(&mut self, key: &str, count: i64) -> io::Result<usize> {
    println!("COUNT");
    let fmt = format!("{}.{}:{}|c\n", &self.prefix, key, count);
    self.emit(&fmt)
  }

  pub fn incr(&mut self, key: &str) -> io::Result<usize> {
    self.count(key, 1)
  }

  pub fn decr(&mut self, key: &str) -> io::Result<usize> {
    self.count(key, -1)
  }

  pub fn time(&mut self, key: &str, time: u64) -> io::Result<usize> {
    let fmt = format!("{}.{}:{}|ms\n", self.prefix, key, time);
    self.emit(&fmt)
  }

  pub fn gauge(&mut self, key: &str, value: u64) -> io::Result<usize> {
    let fmt = format!("{}.{}:{}|g\n", self.prefix, key, value);
    self.emit(&fmt)
  }

  pub fn meter(&mut self, key: &str, value: u64) -> io::Result<usize> {
    let fmt = format!("{}.{}:{}|m\n", self.prefix, key, value);
    self.emit(&fmt)
  }
}
