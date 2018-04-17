#![allow(dead_code, unused_must_use, unused_variables, unused_imports)]

use mio;
use mio::{Poll,Ready};
use mio::unix::UnixReady;
use std::fmt;
use std::net::SocketAddr;
use std::rc::Rc;
use std::cell::RefCell;
use slab::{Entry,VacantEntry};
use time::{precise_time_ns,SteadyTime,Duration};

use sozu_command::messages::{OrderMessage,OrderMessageAnswer};

pub mod buffer_queue;
#[macro_use] pub mod metrics;
pub mod socket;
pub mod trie;
pub mod protocol;
pub mod http;
pub mod backends;
pub mod retry;

#[cfg(feature = "splice")]
mod splice;

pub mod tcp;
pub mod proxy;

#[cfg(feature = "use-openssl")]
pub mod https_openssl;

pub mod https_rustls;

use mio::Token;

use self::retry::RetryPolicy;

pub type AppId = String;

#[derive(Debug,Clone,Copy)]
pub enum Protocol {
  HTTP,
  HTTPS,
  TCP,
  HTTPListen,
  HTTPSListen,
  TCPListen,
  Channel,
  Metrics,
}

#[derive(Debug,Clone,Default)]
pub struct CloseResult {
  pub tokens:   Vec<Token>,
  pub backends: Vec<(String, SocketAddr)>,
}

pub trait ProxyClient {
  fn protocol(&self)  -> Protocol;
  fn ready(&mut self) -> ClientResult;
  fn process_events(&mut self, token: Token, events: Ready);
  fn close(&mut self, poll: &mut Poll) -> CloseResult;
  fn close_backend(&mut self, token: Token, poll: &mut Poll) -> Option<(String, SocketAddr)>;
}

#[derive(Clone,Copy,Debug,PartialEq)]
pub enum BackendConnectionStatus {
  NotConnected,
  Connecting,
  Connected,
}

#[derive(Debug,PartialEq)]
pub enum BackendConnectAction {
  New,
  Reuse,
  Replace,
}

#[derive(Debug,PartialEq)]
pub enum AcceptError {
  IoError,
  TooManyClients,
  WouldBlock,
}

use self::proxy::{ClientToken,ListenToken,ListenPortState};
pub trait ProxyConfiguration<Client> {
  fn connect_to_backend(&mut self, event_loop: &mut Poll, client: &mut Client,
    back_token: Token) ->Result<BackendConnectAction,ConnectionError>;
  fn notify(&mut self, event_loop: &mut Poll, message: OrderMessage) -> OrderMessageAnswer;
  fn accept(&mut self, token: ListenToken, event_loop: &mut Poll, client_token: Token)
    -> Result<(Rc<RefCell<Client>>, bool), AcceptError>;
  fn accept_flush(&mut self) -> usize;
  fn close_backend(&mut self, app_id: String, addr: &SocketAddr);
  fn listen_port_state(&self, port: &u16) -> ListenPortState;
}

#[derive(Debug,PartialEq,Eq)]
pub enum RequiredEvents {
  FrontReadBackNone,
  FrontWriteBackNone,
  FrontReadWriteBackNone,
  FrontNoneBackNone,
  FrontReadBackRead,
  FrontWriteBackRead,
  FrontReadWriteBackRead,
  FrontNoneBackRead,
  FrontReadBackWrite,
  FrontWriteBackWrite,
  FrontReadWriteBackWrite,
  FrontNoneBackWrite,
  FrontReadBackReadWrite,
  FrontWriteBackReadWrite,
  FrontReadWriteBackReadWrite,
  FrontNoneBackReadWrite,
}

impl RequiredEvents {

  pub fn front_readable(&self) -> bool {
    match *self {
      RequiredEvents::FrontReadBackNone
      | RequiredEvents:: FrontReadWriteBackNone
      | RequiredEvents:: FrontReadBackRead
      | RequiredEvents:: FrontReadWriteBackRead
      | RequiredEvents:: FrontReadBackWrite
      | RequiredEvents:: FrontReadWriteBackWrite
      | RequiredEvents:: FrontReadBackReadWrite
      | RequiredEvents:: FrontReadWriteBackReadWrite => true,
      _ => false
    }
  }

  pub fn front_writable(&self) -> bool {
    match *self {
        RequiredEvents::FrontWriteBackNone
        | RequiredEvents::FrontReadWriteBackNone
        | RequiredEvents::FrontWriteBackRead
        | RequiredEvents::FrontReadWriteBackRead
        | RequiredEvents::FrontWriteBackWrite
        | RequiredEvents::FrontReadWriteBackWrite
        | RequiredEvents::FrontWriteBackReadWrite
        | RequiredEvents::FrontReadWriteBackReadWrite => true,
        _ => false
    }
  }

  pub fn back_readable(&self) -> bool {
    match *self {
        RequiredEvents::FrontReadBackRead
        | RequiredEvents::FrontWriteBackRead
        | RequiredEvents::FrontReadWriteBackRead
        | RequiredEvents::FrontNoneBackRead
        | RequiredEvents::FrontReadBackReadWrite
        | RequiredEvents::FrontWriteBackReadWrite
        | RequiredEvents::FrontReadWriteBackReadWrite
        | RequiredEvents::FrontNoneBackReadWrite => true,
        _ => false
    }
  }

  pub fn back_writable(&self) -> bool {
    match *self {
        RequiredEvents::FrontReadBackWrite
        | RequiredEvents::FrontWriteBackWrite
        | RequiredEvents::FrontReadWriteBackWrite
        | RequiredEvents::FrontNoneBackWrite
        | RequiredEvents::FrontReadBackReadWrite
        | RequiredEvents::FrontWriteBackReadWrite
        | RequiredEvents::FrontReadWriteBackReadWrite
        | RequiredEvents::FrontNoneBackReadWrite => true,
        _ => false
    }
  }
}

#[derive(Debug,PartialEq,Eq)]
pub enum ClientResult {
  CloseClient,
  CloseBackend(Option<Token>),
  ReconnectBackend(Option<Token>, Option<Token>),
  Continue,
  ConnectBackend
}

#[derive(Debug,PartialEq,Eq)]
pub enum ConnectionError {
  NoHostGiven,
  NoRequestLineGiven,
  HostNotFound,
  NoBackendAvailable,
  ToBeDefined,
  HttpsRedirect
}

#[derive(Debug,PartialEq,Eq)]
pub enum SocketType {
  Listener,
  FrontClient
}

#[derive(Debug,PartialEq,Eq)]
pub enum BackendStatus {
  Normal,
  Closing,
  Closed,
}

#[derive(Debug,PartialEq,Eq)]
pub struct Backend {
  pub id:                 u32,
  pub backend_id:         String,
  pub address:            SocketAddr,
  pub status:             BackendStatus,
  pub retry_policy:       retry::RetryPolicyWrapper,
  pub active_connections: usize,
  pub failures:           usize,
}

impl Backend {
  pub fn new(backend_id: &str, addr: SocketAddr, id: u32) -> Backend {
    let desired_policy = retry::ExponentialBackoffPolicy::new(10);
    Backend {
      id:                 id,
      backend_id:         backend_id.to_string(),
      address:            addr,
      status:             BackendStatus::Normal,
      retry_policy:       desired_policy.into(),
      active_connections: 0,
      failures:           0,
    }
  }

  pub fn set_closing(&mut self) {
    self.status = BackendStatus::Closing;
  }

  pub fn retry_policy(&mut self) -> &mut retry::RetryPolicyWrapper {
    &mut self.retry_policy
  }

  pub fn can_open(&self) -> bool {
    if let Some(action) = self.retry_policy.can_try() {
      self.status == BackendStatus::Normal && action == retry::RetryAction::OKAY
    } else {
      false
    }
  }

  pub fn inc_connections(&mut self) -> Option<usize> {
    if self.status == BackendStatus::Normal {
      self.active_connections += 1;
      Some(self.active_connections)
    } else {
      None
    }
  }

  pub fn dec_connections(&mut self) -> Option<usize> {
    if self.active_connections == 0 {
      self.status = BackendStatus::Closed;
      return None;
    }

    match self.status {
      BackendStatus::Normal => {
        self.active_connections -= 1;
        Some(self.active_connections)
      }
      BackendStatus::Closed  => None,
      BackendStatus::Closing => {
        self.active_connections -= 1;
        if self.active_connections == 0 {
          self.status = BackendStatus::Closed;
          None
        } else {
          Some(self.active_connections)
        }
      },
    }
  }

  pub fn try_connect(&mut self) -> Result<mio::tcp::TcpStream, ConnectionError> {
    if self.status != BackendStatus::Normal {
      return Err(ConnectionError::NoBackendAvailable);
    }

    //FIXME: what happens if the connect() call fails with EINPROGRESS?
    let conn = mio::tcp::TcpStream::connect(&self.address).map_err(|_| ConnectionError::NoBackendAvailable);
    if conn.is_ok() {
      //self.retry_policy.succeed();
      self.inc_connections();
    } else {
      self.retry_policy.fail();
      self.failures += 1;
    }

    conn
  }
}

#[derive(Debug)]
pub struct Readiness {
  pub front_interest:  UnixReady,
  pub back_interest:   UnixReady,
  pub front_readiness: UnixReady,
  pub back_readiness:  UnixReady,
}

impl Readiness {
  pub fn new() -> Readiness {
    Readiness {
      front_interest:  UnixReady::from(Ready::empty()),
      back_interest:   UnixReady::from(Ready::empty()),
      front_readiness: UnixReady::from(Ready::empty()),
      back_readiness:  UnixReady::from(Ready::empty()),
    }
  }

  pub fn reset(&mut self) {
    self.front_interest  = UnixReady::from(Ready::empty());
    self.back_interest   = UnixReady::from(Ready::empty());
    self.front_readiness = UnixReady::from(Ready::empty());
    self.back_readiness  = UnixReady::from(Ready::empty());
  }
}

#[derive(Clone,Debug)]
pub struct SessionMetrics {
  /// date at which we started handling that request
  pub start:        Option<SteadyTime>,
  /// time actually spent handling the request
  pub service_time: Duration,
  /// bytes received by the frontend
  pub bin:          usize,
  /// bytes sent by the frontend
  pub bout:         usize,

  /// date at which we started working on the request
  pub service_start: Option<SteadyTime>,

  pub backend_id:    Option<String>,
  pub backend_start: Option<SteadyTime>,
  pub backend_stop:  Option<SteadyTime>,
  pub backend_bin:   usize,
  pub backend_bout:  usize,
}

impl SessionMetrics {
  pub fn new() -> SessionMetrics {
    SessionMetrics {
      start:         Some(SteadyTime::now()),
      service_time:  Duration::seconds(0),
      bin:           0,
      bout:          0,
      service_start: None,
      backend_id:    None,
      backend_start: None,
      backend_stop:  None,
      backend_bin:   0,
      backend_bout:  0,
    }
  }

  pub fn reset(&mut self) {
    self.start         = None;
    self.service_time  = Duration::seconds(0);
    self.bin           = 0;
    self.bout          = 0;
    self.service_start = None;
    self.backend_id    = None;
    self.backend_start = None;
    self.backend_stop  = None;
    self.backend_bin   = 0;
    self.backend_bout  = 0;
  }

  pub fn start(&mut self) {
    if self.start.is_none() {
      self.start = Some(SteadyTime::now());
    }
  }

  pub fn service_start(&mut self) {
    if self.start.is_none() {
      self.start = Some(SteadyTime::now());
    }

    self.service_start = Some(SteadyTime::now());
  }

  pub fn service_stop(&mut self) {
    if self.service_start.is_some() {
      let start = self.service_start.take().unwrap();
      let duration = SteadyTime::now() - start;
      self.service_time = self.service_time + duration;
    }
  }

  pub fn service_time(&self) -> Duration {
    match self.service_start {
      Some(start) => {
        let last_duration = SteadyTime::now() - start;
        self.service_time + last_duration
      },
      None        => self.service_time,
    }
  }

  pub fn response_time(&self) -> Duration {
    match self.start {
      Some(start) => SteadyTime::now() - start,
      None        => Duration::seconds(0),
    }
  }

  pub fn backend_start(&mut self) {
    self.backend_start = Some(SteadyTime::now());
  }

  pub fn backend_stop(&mut self) {
    self.backend_stop = Some(SteadyTime::now());
  }

  pub fn backend_response_time(&self) -> Option<Duration> {
    match (self.backend_start, self.backend_stop) {
      (Some(start), Some(end)) => {
        Some(end - start)
      },
      (Some(start), None) => Some(SteadyTime::now() - start),
      _ => None
    }
  }
}

pub struct LogDuration(Duration);

impl fmt::Display for LogDuration {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    let secs = self.0.num_seconds();
    if secs >= 10 {
      return write!(f, "{}s", secs);
    }

    let ms = self.0.num_milliseconds();

    if ms < 10 {
      if let Some(us) = self.0.num_microseconds() {
        if us >= 10 {
          return write!(f, "{}μs", us);
        }

        if let Some(ns) = self.0.num_nanoseconds() {
          return write!(f, "{}ns", ns);
        }
      }
    }

    write!(f, "{}ms", ms)
  }
}

