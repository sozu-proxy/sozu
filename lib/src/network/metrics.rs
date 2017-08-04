use std::str;
use std::thread;
use std::sync::Mutex;
use std::cell::RefCell;
use std::time::Duration;
use std::fmt::Arguments;
use std::net::SocketAddr;
use mio::net::UdpSocket;
use std::io::{self,Write,Error,ErrorKind};
use nom::HexDisplay;

use network::buffer::Buffer;

thread_local! {
  pub static METRICS: RefCell<ProxyMetrics> = RefCell::new(ProxyMetrics::new(String::from("sozu")))
}


pub struct ProxyMetrics {
  pub buffer: Buffer,
  pub prefix: String,
  pub is_writable: bool,
  remote: Option<(SocketAddr, UdpSocket)>,
}

impl ProxyMetrics {
  pub fn new(prefix: String) -> Self {
    ProxyMetrics {
      buffer: Buffer::with_capacity(2048),
      prefix: prefix,
      remote:   None,
      is_writable: false,
    }
  }

  /*
  pub fn run() -> thread::JoinHandle<()> {
    thread::spawn(move || {
      loop {
        thread::sleep(Duration::from_millis(500));
        METRICS.lock().unwrap().send();
      }
    })
  }*/

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

  pub fn writable(&mut self) {
    info!("called METRICS WRITABLE");

    self.is_writable = true;
  }

  pub fn send_data(&mut self) {
    //info!("called METRICS SEND DATA");
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
macro_rules! metrics_set_up (
  ($host:expr, $port: expr) => {
    let metrics_socket = ::mio::net::UdpSocket::bind(&("0.0.0.0:0".parse().unwrap())).expect("could not parse address");
    info!("setting up metrics: local address = {:#?}", metrics_socket.local_addr());
    let metrics_host   = ($host, $port).to_socket_addrs().expect("could not parse address").next().expect("could not get first address");
    METRICS.with(|metrics| {
      (*metrics.borrow_mut()).set_up_remote(metrics_socket, metrics_host);
    });
  }
);

#[macro_export]
macro_rules! count (
  ($key:expr, $value: expr) => {
    let v = $value;
    ::network::metrics::METRICS.with(|metrics| {
      (*metrics.borrow_mut()).write(format_args!("{}.{}:{}|c\n", *$crate::logging::TAG, $key, v));
      (*metrics.borrow_mut()).send();
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
macro_rules! time (
  ($key:expr, $value: expr) => {
    let v = $value;
    ::network::metrics::METRICS.with(|metrics| {
      (*metrics.borrow_mut()).write(format_args!("{}.{}:{}|ms\n", *$crate::logging::TAG, $key, v));
      (*metrics.borrow_mut()).send();
    });
  }
);

#[macro_export]
macro_rules! gauge (
  ($key:expr, $value: expr) => {
    let v = $value;
    ::network::metrics::METRICS.with(|metrics| {
      info!("gauge {} -> {}", $key, v);
      (*metrics.borrow_mut()).write(format_args!("{}.{}:{}|g\n", *$crate::logging::TAG, $key, v));
      (*metrics.borrow_mut()).send();
    });
  }
);

#[macro_export]
macro_rules! meter (
  ($key:expr, $value: expr) =>  {
    let v = $value;
    ::network::metrics::METRICS.with(|metrics| {
      (*metrics.borrow_mut()).write(format_args!("{}.{}:{}|m\n", *$crate::logging::TAG, $key, v));
      (*metrics.borrow_mut()).send();
    });
  }
);
