use std::thread::{self,Thread,Builder};
use std::sync::mpsc::{self,channel,Receiver};
use mio::net::*;
use mio::*;
use mio_uds::UnixStream;
use mio::unix::UnixReady;
use std::collections::{HashMap,HashSet};
use std::os::unix::io::RawFd;
use std::os::unix::io::{FromRawFd,AsRawFd};
use std::io::{self,Read,ErrorKind};
use nom::HexDisplay;
use std::error::Error;
use slab::{Slab,Entry,VacantEntry};
use std::rc::Rc;
use std::cell::{RefCell,RefMut};
use std::net::{SocketAddr,Shutdown};
use std::str::FromStr;
use time::{Duration,SteadyTime,precise_time_s};
use rand::random;
use uuid::Uuid;
use mio_extras::timer::{Timer,Timeout};

use sozu_command::buffer::Buffer;
use sozu_command::channel::Channel;
use sozu_command::scm_socket::ScmSocket;
use sozu_command::config::{ProxyProtocolConfig, LoadBalancingAlgorithms};
use sozu_command::messages::{self,TcpFront,Order,OrderMessage,OrderMessageAnswer,OrderMessageStatus,LoadBalancingParams};
use sozu_command::messages::TcpListener as TcpListenerConfig;
use sozu_command::logging;

use network::{AppId,Backend,ClientResult,ConnectionError,RequiredEvents,Protocol,Readiness,SessionMetrics,
  ProxyClient,ProxyConfiguration,AcceptError,BackendConnectAction,BackendConnectionStatus,
  CloseResult};
use network::backends::BackendMap;
use network::proxy::{Server,ProxyChannel,ListenToken,ListenPortState,ClientToken,ListenClient, CONN_RETRIES};
use network::pool::{Pool,Checkout,Reset};
use network::buffer_queue::BufferQueue;
use network::socket::{SocketHandler,SocketResult,server_bind};
use network::{http,https_rustls};
use network::protocol::{Pipe, ProtocolResult};
use network::protocol::proxy_protocol::send::SendProxyProtocol;
use network::protocol::proxy_protocol::relay::RelayProxyProtocol;
use network::protocol::proxy_protocol::expect::ExpectProxyProtocol;
use network::retry::RetryPolicy;

use util::UnwrapLog;


const SERVER: Token = Token(0);

#[derive(Debug,Clone,PartialEq,Eq)]
pub enum ConnectionStatus {
  Initial,
  ClientConnected,
  Connected,
  ClientClosed,
  ServerClosed,
  Closed
}

pub enum UpgradeResult {
  Continue,
  Close,
  ConnectBackend,
}

pub enum State {
  Pipe(Pipe<TcpStream>),
  SendProxyProtocol(SendProxyProtocol<TcpStream>),
  RelayProxyProtocol(RelayProxyProtocol<TcpStream>),
  ExpectProxyProtocol(ExpectProxyProtocol<TcpStream>),
}

pub struct Client {
  sock:               TcpStream,
  backend:            Option<Rc<RefCell<Backend>>>,
  frontend_token:     Token,
  backend_token:      Option<Token>,
  back_connected:     BackendConnectionStatus,
  accept_token:       Token,
  status:             ConnectionStatus,
  rx_count:           usize,
  tx_count:           usize,
  app_id:             Option<String>,
  request_id:         String,
  metrics:            SessionMetrics,
  protocol:           Option<State>,
  front_buf:          Option<Checkout<BufferQueue>>,
  back_buf:           Option<Checkout<BufferQueue>>,
  timeout:            Timeout,
  last_event:         SteadyTime,
  connection_attempt: u8,
}

impl Client {
  fn new(sock: TcpStream, frontend_token: Token, accept_token: Token, front_buf: Checkout<BufferQueue>,
    back_buf: Checkout<BufferQueue>, proxy_protocol: Option<ProxyProtocolConfig>, timeout: Timeout) -> Client {
    let s = sock.try_clone().expect("could not clone the socket");
    let addr = sock.peer_addr().map(|s| s.ip()).ok();
    let mut frontend_buffer = None;
    let mut backend_buffer = None;

      let protocol = match proxy_protocol {
      Some(ProxyProtocolConfig::RelayHeader) => {
        backend_buffer = Some(back_buf);
        gauge_add!("protocol.proxy.relay", 1);
        Some(State::RelayProxyProtocol(RelayProxyProtocol::new(s, frontend_token, None, front_buf)))
      },
      Some(ProxyProtocolConfig::ExpectHeader) => {
        frontend_buffer = Some(front_buf);
        backend_buffer = Some(back_buf);
        gauge_add!("protocol.proxy.expect", 1);
        Some(State::ExpectProxyProtocol(ExpectProxyProtocol::new(s, frontend_token)))
      },
      Some(ProxyProtocolConfig::SendHeader) => {
        frontend_buffer = Some(front_buf);
        backend_buffer = Some(back_buf);
        gauge_add!("protocol.proxy.send", 1);
        Some(State::SendProxyProtocol(SendProxyProtocol::new(s, frontend_token, None)))
      },
      None => {
        gauge_add!("protocol.tcp", 1);
        Some(State::Pipe(Pipe::new(s, frontend_token, None, front_buf, back_buf, addr)))
      }
    };

    Client {
      sock:               sock,
      backend:            None,
      frontend_token:     frontend_token,
      backend_token:      None,
      back_connected:     BackendConnectionStatus::NotConnected,
      accept_token:       accept_token,
      status:             ConnectionStatus::Connected,
      rx_count:           0,
      tx_count:           0,
      app_id:             None,
      request_id:         Uuid::new_v4().hyphenated().to_string(),
      metrics:            SessionMetrics::new(),
      protocol,
      front_buf:          frontend_buffer,
      back_buf:           backend_buffer,
      timeout,
      last_event:         SteadyTime::now(),
      connection_attempt: 0,
    }
  }

  fn log_request(&self) {
    let client = match self.sock.peer_addr().ok() {
      None => String::from("-"),
      Some(SocketAddr::V4(addr)) => format!("{}", addr),
      Some(SocketAddr::V6(addr)) => format!("{}", addr),
    };

    let backend_address = self.backend.as_ref().map(|backend| (*backend.borrow_mut()).address);

    let backend = match backend_address {
      None => String::from("-"),
      Some(SocketAddr::V4(addr)) => format!("{}", addr),
      Some(SocketAddr::V6(addr)) => format!("{}", addr),
    };

    let response_time = self.metrics.response_time().num_milliseconds();
    let service_time  = self.metrics.service_time().num_milliseconds();
    let app_id = self.app_id.clone().unwrap_or(String::from("-"));
    time!("request_time", &app_id, response_time);

    if let Some(backend_id) = self.metrics.backend_id.as_ref() {
      if let Some(backend_response_time) = self.metrics.backend_response_time() {
        record_backend_metrics!(app_id, backend_id, backend_response_time.num_milliseconds(),
          self.metrics.backend_bin, self.metrics.backend_bout);
      }
    }

    info!("{}{} -> {}\t{} {} {} {}",
      self.log_context(), client, backend,
      response_time, service_time, self.metrics.bin, self.metrics.bout);
  }

  fn front_hup(&mut self) -> ClientResult {
    self.log_request();

    match self.protocol {
      Some(State::Pipe(ref mut pipe)) => pipe.front_hup(),
      Some(State::SendProxyProtocol(_)) => {
        ClientResult::CloseClient
      },
      _ => unreachable!(),
    }
  }

  fn back_hup(&mut self) -> ClientResult {
    self.log_request();

    match self.protocol {
      Some(State::Pipe(ref mut pipe)) => pipe.back_hup(),
      _ => ClientResult::CloseClient,
    }
  }


  fn log_context(&self) -> String {
    if let Some(ref app_id) = self.app_id {
      format!("{}\t{}\t", self.request_id, app_id)
    } else {
      format!("{}\tunknown\t", self.request_id)
    }
  }

  fn readable(&mut self) -> ClientResult {
    let mut should_upgrade_protocol = ProtocolResult::Continue;

    let res = match self.protocol {
      Some(State::Pipe(ref mut pipe)) => pipe.readable(&mut self.metrics),
      Some(State::RelayProxyProtocol(ref mut pp)) => pp.readable(&mut self.metrics),
      Some(State::ExpectProxyProtocol(ref mut pp)) => {
        let res = pp.readable(&mut self.metrics);
        should_upgrade_protocol = res.0;
        res.1
      },
      _ => ClientResult::Continue,
    };

    if let ProtocolResult::Upgrade = should_upgrade_protocol {
      match self.upgrade() {
        UpgradeResult::Continue => ClientResult::Continue,
        UpgradeResult::Close    => ClientResult::CloseClient,
        UpgradeResult::ConnectBackend => ClientResult::ConnectBackend,
      }
    } else {
      res
    }
  }

  fn writable(&mut self) -> ClientResult {
    match self.protocol {
      Some(State::Pipe(ref mut pipe)) => pipe.writable(&mut self.metrics),
      _ => ClientResult::Continue,
    }
  }

  fn back_readable(&mut self) -> ClientResult {
    match self.protocol {
      Some(State::Pipe(ref mut pipe)) => pipe.back_readable(&mut self.metrics),
      _ => ClientResult::Continue,
    }
  }

  fn back_writable(&mut self) -> ClientResult {
    let mut res = (ProtocolResult::Continue, ClientResult::Continue);

    match self.protocol {
      Some(State::Pipe(ref mut pipe)) => res.1 = pipe.back_writable(&mut self.metrics),
      Some(State::RelayProxyProtocol(ref mut pp)) => {
        res = pp.back_writable(&mut self.metrics);
      },
      Some(State::SendProxyProtocol(ref mut pp)) => {
        res = pp.back_writable(&mut self.metrics);
      }
      _ => unreachable!(),
    };

    if let ProtocolResult::Upgrade = res.0 {
      self.upgrade();
    }

    res.1
  }

  fn front_socket(&self) -> &TcpStream {
    match self.protocol {
      Some(State::Pipe(ref pipe)) => pipe.front_socket(),
      Some(State::SendProxyProtocol(ref pp)) => pp.front_socket(),
      Some(State::RelayProxyProtocol(ref pp)) => pp.front_socket(),
      Some(State::ExpectProxyProtocol(ref pp)) => pp.front_socket(),
      _ => unreachable!(),
    }
  }

  fn back_socket(&self)  -> Option<&TcpStream> {
    match self.protocol {
      Some(State::Pipe(ref pipe)) => pipe.back_socket(),
      Some(State::SendProxyProtocol(ref pp)) => pp.back_socket(),
      Some(State::RelayProxyProtocol(ref pp)) => pp.back_socket(),
      Some(State::ExpectProxyProtocol(_)) => None,
      _ => unreachable!(),
    }
  }

  pub fn upgrade(&mut self) -> UpgradeResult {
    let protocol = self.protocol.take();

    if let Some(State::SendProxyProtocol(pp)) = protocol {
      if self.back_buf.is_some() && self.front_buf.is_some() {
        self.protocol = Some(
          State::Pipe(pp.into_pipe(self.front_buf.take().unwrap(), self.back_buf.take().unwrap()))
        );
        gauge_add!("protocol.proxy.send", -1);
        gauge_add!("protocol.tcp", 1);
        UpgradeResult::Continue
      } else {
        error!("Missing the frontend or backend buffer queue, we can't switch to a pipe");
        UpgradeResult::Close
      }
    } else if let Some(State::RelayProxyProtocol(mut pp)) = protocol {
      if self.back_buf.is_some() {
        self.protocol = Some(
          State::Pipe(pp.into_pipe(self.back_buf.take().unwrap()))
        );
        gauge_add!("protocol.proxy.relay", -1);
        gauge_add!("protocol.tcp", 1);
        UpgradeResult::Continue
      } else {
        error!("Missing the backend buffer queue, we can't switch to a pipe");
        UpgradeResult::Close
      }
    } else if let Some(State::ExpectProxyProtocol(mut pp)) = protocol {
      if self.front_buf.is_some() && self.back_buf.is_some() {
        self.protocol = Some(
          State::Pipe(pp.into_pipe(self.front_buf.take().unwrap(), self.back_buf.take().unwrap(), None, None))
        );
        gauge_add!("protocol.proxy.expect", -1);
        gauge_add!("protocol.tcp", 1);
        UpgradeResult::ConnectBackend
      } else {
        error!("Missing the backend buffer queue, we can't switch to a pipe");
        UpgradeResult::Close
      }
    } else {
      UpgradeResult::Close
    }
  }

  fn front_readiness(&mut self) -> &mut Readiness {
    match self.protocol {
      Some(State::Pipe(ref mut pipe)) => pipe.front_readiness(),
      Some(State::SendProxyProtocol(ref mut pp)) => panic!(),
      Some(State::RelayProxyProtocol(ref mut pp)) => pp.front_readiness(),
      Some(State::ExpectProxyProtocol(ref mut pp)) => pp.readiness(),
      _ => unreachable!(),
    }
  }

  fn back_readiness(&mut self) -> Option<&mut Readiness> {
    match self.protocol {
      Some(State::Pipe(ref mut pipe)) => Some(pipe.back_readiness()),
      Some(State::SendProxyProtocol(ref mut pp)) => Some(pp.readiness()),
      Some(State::RelayProxyProtocol(ref mut pp)) => Some(pp.back_readiness()),
      _ => None,
    }
  }

  fn back_token(&self)   -> Option<Token> {
    match self.protocol {
      Some(State::Pipe(ref pipe)) => pipe.back_token(),
      Some(State::SendProxyProtocol(ref pp)) => pp.back_token(),
      Some(State::RelayProxyProtocol(ref pp)) => pp.back_token(),
      Some(State::ExpectProxyProtocol(_)) => None,
      _ => unreachable!()
    }
  }

  fn set_back_socket(&mut self, socket: TcpStream) {
    match self.protocol {
      Some(State::Pipe(ref mut pipe)) => pipe.set_back_socket(socket),
      Some(State::SendProxyProtocol(ref mut pp)) => pp.set_back_socket(socket),
      Some(State::RelayProxyProtocol(ref mut pp)) => pp.set_back_socket(socket),
      Some(State::ExpectProxyProtocol(_)) => panic!("we should not set the back socket for the expect proxy protocol"),
      _ => unreachable!()
    }
  }

  fn set_back_token(&mut self, token: Token) {
    self.backend_token = Some(token);

    match self.protocol {
      Some(State::Pipe(ref mut pipe)) => pipe.set_back_token(token),
      Some(State::SendProxyProtocol(ref mut pp)) => pp.set_back_token(token),
      Some(State::RelayProxyProtocol(ref mut pp)) => pp.set_back_token(token),
      Some(State::ExpectProxyProtocol(_)) => self.backend_token = Some(token),
      _ => unreachable!()
    }
  }

  fn back_connected(&self)     -> BackendConnectionStatus {
    self.back_connected
  }

  fn set_back_connected(&mut self, status: BackendConnectionStatus) {
    self.back_connected = status;
    if status == BackendConnectionStatus::Connected {
      gauge_add!("backend.connections", 1);
      if let Some(State::SendProxyProtocol(ref mut pp)) = self.protocol {
        pp.set_back_connected(BackendConnectionStatus::Connected);
      }

      self.backend.as_ref().map(|backend| {
        let ref mut backend = *backend.borrow_mut();
        //successful connection, rest failure counter
        backend.failures = 0;
        backend.retry_policy.succeed();
      });
    }
  }

  fn metrics(&mut self)        -> &mut SessionMetrics {
    &mut self.metrics
  }

  fn remove_backend(&mut self) -> (Option<String>, Option<SocketAddr>) {

    let addr = self.backend.as_ref().map(|backend| (*backend.borrow_mut()).address);

    self.backend       = None;
    self.backend_token = None;
    (self.app_id.clone(), addr)
  }

    fn reset_connection_attempt(&mut self) {
      self.connection_attempt = 0;
    }
}

impl ProxyClient for Client {
  fn close(&mut self, poll: &mut Poll) -> CloseResult {
    self.metrics.service_stop();
    self.front_socket().shutdown(Shutdown::Both);
    poll.deregister(self.front_socket());

    let mut result = CloseResult::default();

    if let Some(tk) = self.backend_token {
      result.tokens.push(tk)
    }

    if let (Some(app_id), Some(addr)) = self.remove_backend() {
      result.backends.push((app_id, addr.clone()));
    }

    let back_connected = self.back_connected();
    if back_connected != BackendConnectionStatus::NotConnected {
      if let Some(sock) = self.back_socket() {
        sock.shutdown(Shutdown::Both);
        poll.deregister(sock);
      }
    }

    if back_connected == BackendConnectionStatus::Connected {
      gauge_add!("backend.connections", -1);
    }

    self.set_back_connected(BackendConnectionStatus::NotConnected);

    match self.protocol {
      Some(State::Pipe(_)) => gauge_add!("protocol.tcp", -1),
      Some(State::SendProxyProtocol(_)) => gauge_add!("protocol.proxy.send", -1),
      Some(State::RelayProxyProtocol(_)) => gauge_add!("protocol.proxy.relay", -1),
      Some(State::ExpectProxyProtocol(_)) => gauge_add!("protocol.proxy.expect", -1),
      None => {},
    }

    result.tokens.push(self.frontend_token);

    result
  }

  fn timeout(&self, token: Token, timer: &mut Timer<Token>, front_timeout: &Duration) -> ClientResult {
    if self.frontend_token == token {
      let dur = SteadyTime::now() - self.last_event;
      if dur < *front_timeout {
        timer.set_timeout((*front_timeout - dur).to_std().unwrap(), token);
        ClientResult::Continue
      } else {
        ClientResult::CloseClient
      }
    } else {
      // invalid token, obsolete timeout triggered
      ClientResult::Continue
    }
  }

  fn cancel_timeouts(&self, timer: &mut Timer<Token>) {
    timer.cancel_timeout(&self.timeout);
  }

  fn close_backend(&mut self, _: Token, poll: &mut Poll) -> Option<(String,SocketAddr)> {
    let mut res = None;
    if let (Some(app_id), Some(addr)) = self.remove_backend() {
      res = Some((app_id, addr.clone()));
    }

    let back_connected = self.back_connected();
    if back_connected != BackendConnectionStatus::NotConnected {
      if let Some(sock) = self.back_socket() {
        sock.shutdown(Shutdown::Both);
        poll.deregister(sock);
      }
    }

    if back_connected == BackendConnectionStatus::Connected {
      gauge_add!("backend.connections", -1);
    }

    self.set_back_connected(BackendConnectionStatus::NotConnected);

    res
  }

  fn protocol(&self) -> Protocol {
    Protocol::TCP
  }

  fn process_events(&mut self, token: Token, events: Ready) {
    trace!("token {:?} got event {}", token, super::unix_ready_to_string(UnixReady::from(events)));
    self.last_event = SteadyTime::now();

    if self.frontend_token == token {
      self.front_readiness().event = self.front_readiness().event | UnixReady::from(events);
    } else if self.backend_token == Some(token) {
      self.back_readiness().map(|r| r.event = r.event | UnixReady::from(events));
    }
  }

  fn ready(&mut self) -> ClientResult {
    let mut counter = 0;
    let max_loop_iterations = 100000;

    self.metrics().service_start();

    if self.back_connected() == BackendConnectionStatus::Connecting {
      if self.back_readiness().unwrap().event.is_hup() {
        //retry connecting the backend
        error!("error connecting to backend, trying again");
        self.metrics().service_stop();
        self.connection_attempt += 1;
        let backend_token = self.backend_token.take();
        return ClientResult::ReconnectBackend(Some(self.frontend_token), backend_token);
      } else if self.back_readiness().unwrap().event != UnixReady::from(Ready::empty()) {
        self.reset_connection_attempt();
        self.set_back_connected(BackendConnectionStatus::Connected);
      }
    }

    if self.front_readiness().event.is_hup() {
      let order = self.front_hup();
      match order {
        ClientResult::CloseClient => {
          return order;
        },
        _ => {
          self.front_readiness().event.remove(UnixReady::hup());
          return order;
        }
      }
    }

    let token = self.frontend_token;
    while counter < max_loop_iterations {
      let front_interest = self.front_readiness().interest & self.front_readiness().event;
      let back_interest  = self.back_readiness().map(|r| r.interest & r.event).unwrap_or(UnixReady::from(Ready::empty()));

      trace!("PROXY\t{} {:?} {:?} -> {:?}", self.log_context(), token, self.front_readiness().clone(), self.back_readiness());

      if front_interest == UnixReady::from(Ready::empty()) && back_interest == UnixReady::from(Ready::empty()) {
        break;
      }

      if self.back_readiness().map(|r| r.event.is_hup()).unwrap_or(false) && self.front_readiness().interest.is_writable() &&
        ! self.front_readiness().event.is_writable() {
        break;
      }

      if front_interest.is_readable() {
        let order = self.readable();
        trace!("front readable\tinterpreting client order {:?}", order);

        if order != ClientResult::Continue {
          return order;
        }
      }

      if back_interest.is_writable() {
        let order = self.back_writable();
        if order != ClientResult::Continue {
          return order;
        }
      }

      if back_interest.is_readable() {
        let order = self.back_readable();
        if order != ClientResult::Continue {
          return order;
        }
      }

      if front_interest.is_writable() {
        let order = self.writable();
        trace!("front writable\tinterpreting client order {:?}", order);
        if order != ClientResult::Continue {
          return order;
        }
      }

      if back_interest.is_hup() {
        let order = self.back_hup();
        match order {
          ClientResult::CloseClient => {
            return order;
          },
          ClientResult::Continue => {},
          _ => {
            self.back_readiness().map(|r| r.event.remove(UnixReady::hup()));
            return order;
          }
        };
      }

      if front_interest.is_error() || back_interest.is_error() {
        if front_interest.is_error() {
          error!("PROXY client {:?} front error, disconnecting", self.frontend_token);
        } else {
          error!("PROXY client {:?} back error, disconnecting", self.frontend_token);
        }

        self.front_readiness().interest = UnixReady::from(Ready::empty());
        self.back_readiness().map(|r| r.interest  = UnixReady::from(Ready::empty()));
        return ClientResult::CloseClient;
      }

      counter += 1;
    }

    if counter == max_loop_iterations {
      error!("PROXY\thandling client {:?} went through {} iterations, there's a probable infinite loop bug, closing the connection", self.frontend_token, max_loop_iterations);

      let front_interest = self.front_readiness().interest & self.front_readiness().event;
      let back_interest  = self.back_readiness().map(|r| r.interest & r.event).unwrap_or(UnixReady::from(Ready::empty()));

      let back = self.back_readiness().cloned();
      error!("PROXY\t{:?} readiness: front {:?} / back {:?} | front: {:?} | back: {:?} ", self.frontend_token.clone(),
        self.front_readiness(), back, front_interest, back_interest);

      return ClientResult::CloseClient;
    }

    ClientResult::Continue
  }

  fn last_event(&self) -> SteadyTime {
    self.last_event
  }

  fn print_state(&self) {
    let p:String = match &self.protocol {
      Some(State::ExpectProxyProtocol(_)) => String::from("Expect"),
      Some(State::SendProxyProtocol(_))   => String::from("Send"),
      Some(State::RelayProxyProtocol(_))  => String::from("Relay"),
      Some(State::Pipe(_))                => String::from("TCP"),
      None                                => String::from("None"),
    };

    let rf = match *unwrap_msg!(self.protocol.as_ref()) {
      State::ExpectProxyProtocol(ref expect) => &expect.readiness,
      State::SendProxyProtocol(ref send)     => &send.readiness,
      State::RelayProxyProtocol(ref relay)   => &relay.front_readiness,
      State::Pipe(ref pipe)                  => &pipe.front_readiness,
    };

    let rb = match *unwrap_msg!(self.protocol.as_ref()) {
      State::RelayProxyProtocol(ref relay)   => Some(&relay.back_readiness),
      State::Pipe(ref pipe)                  => Some(&pipe.back_readiness),
      _ => None,
    };

    error!("zombie client[{:?} => {:?}], state => readiness: {:?} -> {:?}, protocol: {}, app_id: {:?}, back_connected: {:?}, metrics: {:?}",
      self.frontend_token, self.back_token(), rf, rb, p, self.app_id, self.back_connected, self.metrics);
  }

  fn tokens(&self) -> Vec<Token> {
    let mut v = vec![self.frontend_token];
    if let Some(tk) = self.back_token() {
      v.push(tk)
    }

    v
  }

}

pub struct Listener {
  app_id:   Option<String>,
  listener: Option<TcpListener>,
  token:    Token,
  address:  SocketAddr,
  pool:     Rc<RefCell<Pool<BufferQueue>>>,
  config:   TcpListenerConfig,
  active:   bool,
}

impl Listener {
  fn new(config: TcpListenerConfig, pool: Rc<RefCell<Pool<BufferQueue>>>, token: Token) -> Listener {
    Listener {
      app_id: None,
      listener: None,
      token,
      address: config.front,
      pool,
      config,
      active: false,
    }
  }

  pub fn activate(&mut self, event_loop: &mut Poll, tcp_listener: Option<TcpListener>) -> Option<Token> {
    if self.active {
      return None;
    }

    let listener = tcp_listener.or_else(|| server_bind(&self.config.front).map_err(|e| {
      error!("could not create listener {:?}: {:?}", self.config.front, e);
    }).ok());


    if let Some(ref sock) = listener {
      event_loop.register(sock, self.token, Ready::readable(), PollOpt::edge());
    } else {
      return None;
    }

    self.listener = listener;
    self.active = true;
    Some(self.token)
  }
}

#[derive(Debug)]
pub struct ApplicationConfiguration {
  proxy_protocol: Option<ProxyProtocolConfig>,
  load_balancing_policy: LoadBalancingAlgorithms,
}

pub struct ServerConfiguration {
  fronts:    HashMap<String, Token>,
  backends:  BackendMap,
  listeners: HashMap<Token, Listener>,
  configs:   HashMap<AppId, ApplicationConfiguration>,
}

impl ServerConfiguration {
  pub fn new() -> ServerConfiguration {

    ServerConfiguration {
      backends:  BackendMap::new(),
      listeners: HashMap::new(),
      configs:   HashMap::new(),
      fronts:    HashMap::new(),
    }
  }

  pub fn add_listener(&mut self, config: TcpListenerConfig, pool: Rc<RefCell<Pool<BufferQueue>>>, token: Token) -> Option<Token> {
    if self.listeners.contains_key(&token) {
      None
    } else {
      let listener = Listener::new(config, pool, token);
      self.listeners.insert(listener.token.clone(), listener);
      Some(token)
    }
  }

  pub fn activate_listener(&mut self, event_loop: &mut Poll, addr: &SocketAddr, tcp_listener: Option<TcpListener>) -> Option<Token> {
    for listener in self.listeners.values_mut() {
      if &listener.address == addr {
        return listener.activate(event_loop, tcp_listener);
      }
    }
    None
  }

  pub fn give_back_listeners(&mut self) -> Vec<(SocketAddr, TcpListener)> {
    let res = self.listeners.values_mut()
      .filter(|app_listener| app_listener.listener.is_some())
      .map(|app_listener| (app_listener.address.clone(), app_listener.listener.take().unwrap()))
      .collect();
    res
  }

  pub fn add_tcp_front(&mut self, app_id: &str, front: &SocketAddr, event_loop: &mut Poll) -> bool {
    if let Some(listener) = self.listeners.values_mut().find(|l| l.address == *front) {
      self.fronts.insert(app_id.to_string(), listener.token);
      info!("add_tcp_front: fronts are now: {:?}", self.fronts);
      listener.app_id = Some(app_id.to_string());
      true
    } else {
      false
    }
  }

  pub fn remove_tcp_front(&mut self, front: SocketAddr, event_loop: &mut Poll) -> bool {
    if let Some(listener) = self.listeners.values_mut().find(|l| l.address == front) {
      if let Some(app_id) = listener.app_id.take() {
        self.fronts.remove(&app_id);
      }
      true
    } else {
      false
    }
  }

  pub fn add_backend(&mut self, app_id: &str, backend: Backend, event_loop: &mut Poll) {
    self.backends.add_backend(app_id, backend.clone());
  }

  pub fn remove_backend(&mut self, app_id: &str, backend_address: &SocketAddr, event_loop: &mut Poll) {
    let backend = self.backends.remove_backend(app_id, backend_address);
  }

  fn backend_from_app_id(&mut self, client: &mut Client, app_id: &str) -> Result<TcpStream,ConnectionError> {
    match self.backends.backend_from_app_id(app_id) {
      Err(e) => Err(e),
      Ok((backend, conn))  => {
        client.back_connected = BackendConnectionStatus::Connecting;
        client.backend = Some(backend);

        Ok(conn)
      }
    }
  }
}

impl ProxyConfiguration<Client> for ServerConfiguration {

  fn connect_to_backend(&mut self, poll: &mut Poll, client: &mut Client, back_token: Token) ->Result<BackendConnectAction,ConnectionError> {
    if self.listeners[&client.accept_token].app_id.is_none() {
      error!("no TCP application corresponds to that front address");
      return Err(ConnectionError::HostNotFound);
    }

    let app_id = self.listeners[&client.accept_token].app_id.clone();
    client.app_id = app_id.clone();
    let app_id = app_id.unwrap();

    // Circuit breaker
    if client.back_connected == BackendConnectionStatus::Connecting {
      client.backend.as_ref().map(|backend| {
        let ref mut backend = *backend.borrow_mut();
        backend.dec_connections();
        backend.failures += 1;

        let already_unavailable = backend.retry_policy.is_down();
        backend.retry_policy.fail();
        incr!("backend.connections.error");
        if !already_unavailable && backend.retry_policy.is_down() {
          incr!("backend.down");
        }
      });

      //deregister back socket if it is the wrong one or if it was not connecting
      client.backend = None;
      client.back_connected = BackendConnectionStatus::NotConnected;
      client.back_readiness().map(|r| {
        r.interest = UnixReady::from(Ready::empty());
        r.event = UnixReady::from(Ready::empty());
      });
      client.back_socket().as_ref().map(|sock| {
        poll.deregister(*sock);
        sock.shutdown(Shutdown::Both);
      });

      if client.connection_attempt == CONN_RETRIES {
        error!("{} max connection attempt reached", client.log_context());
        return Err(ConnectionError::NoBackendAvailable)
      }
    }

    let conn = self.backend_from_app_id(client, &app_id);
    match conn {
      Ok(stream) => {
        stream.set_nodelay(true);

        poll.register(
          &stream,
          back_token,
          Ready::readable() | Ready::writable() | Ready::from(UnixReady::hup() | UnixReady::error()),
          PollOpt::edge()
        );

        client.set_back_token(back_token);
        client.set_back_socket(stream);
        Ok(BackendConnectAction::New)
      },
      Err(ConnectionError::NoBackendAvailable) => Err(ConnectionError::NoBackendAvailable),
      Err(e) => {
        panic!("tcp connect_to_backend: unexpected error: {:?}", e);
      }
    }
  }

  fn notify(&mut self, event_loop: &mut Poll, message: OrderMessage) -> OrderMessageAnswer {
    match message.order {
      Order::AddTcpFront(front) => {
        let _ = self.add_tcp_front(&front.app_id, &front.address, event_loop);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None}
      },
      Order::RemoveTcpFront(front) => {
        let _ = self.remove_tcp_front(front.address.clone(), event_loop);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None}
      },
      Order::AddBackend(backend) => {
        let new_backend = Backend::new(&backend.backend_id, backend.address.clone(), backend.sticky_id.clone(), backend.load_balancing_parameters, backend.backup);
        self.add_backend(&backend.app_id, new_backend, event_loop);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None}
      },
      Order::RemoveBackend(backend) => {
        trace!("{:?}", backend);
        self.remove_backend(&backend.app_id, &backend.address, event_loop);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None}
      },
      Order::SoftStop => {
        info!("{} processing soft shutdown", message.id);
        for (_, l) in self.listeners.iter_mut() {
          l.listener.take().map(|sock| event_loop.deregister(&sock));
        }
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Processing, data: None}
      },
      Order::HardStop => {
        info!("{} hard shutdown", message.id);
        for (_, mut l) in self.listeners.drain() {
          l.listener.take().map(|sock| event_loop.deregister(&sock));
        }
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None}
      },
      Order::Status => {
        info!("{} status", message.id);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None}
      },
      Order::Logging(logging_filter) => {
        info!("{} changing logging filter to {}", message.id, logging_filter);
        logging::LOGGER.with(|l| {
          let directives = logging::parse_logging_spec(&logging_filter);
          l.borrow_mut().set_directives(directives);
        });
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      Order::AddApplication(application) => {
        let config = ApplicationConfiguration {
          proxy_protocol: application.proxy_protocol,
          load_balancing_policy: application.load_balancing_policy,
        };
        self.configs.insert(application.app_id.clone(), config);
        self.backends.set_load_balancing_policy_for_app(&application.app_id, application.load_balancing_policy);

        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      Order::RemoveApplication(_) => {
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      Order::RemoveListener(remove) => {
        fixme!();
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Error(String::from("unimplemented")), data: None }
      },
      command => {
        error!("{} unsupported message, ignoring {:?}", message.id, command);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Error(String::from("unsupported message")), data: None}
      }
    }
  }

  fn accept(&mut self, token: ListenToken) -> Result<TcpStream, AcceptError> {
    let internal_token = Token(token.0);
    if let Some(listener) = self.listeners.get_mut(&internal_token) {
      if let Some(ref tcp_listener) = listener.listener.as_ref() {
        tcp_listener.accept().map(|(frontend_sock, _)| frontend_sock).map_err(|e| {
          match e.kind() {
            ErrorKind::WouldBlock => AcceptError::WouldBlock,
            other => {
              error!("accept() IO error: {:?}", e);
              AcceptError::IoError
            }
          }
        })
      } else {
        Err(AcceptError::IoError)
      }
    } else {
      Err(AcceptError::IoError)
    }
  }

  fn create_client(&mut self, frontend_sock: TcpStream, token: ListenToken, poll: &mut Poll, client_token: Token, timeout: Timeout)
    -> Result<(Rc<RefCell<Client>>, bool), AcceptError> {
    let internal_token = Token(token.0);
    if let Some(listener) = self.listeners.get_mut(&internal_token) {
      let mut p = (*listener.pool).borrow_mut();

      if let (Some(front_buf), Some(back_buf)) = (p.checkout(), p.checkout()) {
        let proxy_protocol = self.configs
                                .get(listener.app_id.as_ref().unwrap())
                                .and_then(|c| c.proxy_protocol.clone());

        frontend_sock.set_nodelay(true);
        let c = Client::new(frontend_sock, client_token, internal_token, front_buf, back_buf, proxy_protocol.clone(),
        timeout);
        incr!("tcp.requests");

        poll.register(
          c.front_socket(),
          client_token,
          Ready::readable() | Ready::writable() | Ready::from(UnixReady::hup() | UnixReady::error()),
          PollOpt::edge()
          );

        let should_connect_backend = proxy_protocol != Some(ProxyProtocolConfig::ExpectHeader);
        Ok((Rc::new(RefCell::new(c)), should_connect_backend))
      } else {
        error!("could not get buffers from pool");
        Err(AcceptError::TooManyClients)
      }
    } else {
      Err(AcceptError::IoError)
    }
  }

  fn close_backend(&mut self, app_id: String, addr: &SocketAddr) {
    self.backends.close_backend_connection(&app_id, &addr);
  }

  fn listen_port_state(&self, port: &u16) -> ListenPortState {
    match self.listeners.values().find(|listener| &listener.address.port() == port) {
      Some(_) => ListenPortState::InUse,
      None => ListenPortState::Available
    }
  }
}


pub fn start(config: TcpListenerConfig, max_buffers: usize, buffer_size:usize, channel: ProxyChannel) {
  use network::proxy::ProxyClientCast;

  let mut poll          = Poll::new().expect("could not create event loop");
  let pool = Rc::new(RefCell::new(
    Pool::with_capacity(2*max_buffers, 0, || BufferQueue::with_capacity(buffer_size))
  ));

  let mut clients: Slab<Rc<RefCell<ProxyClientCast>>,ClientToken> = Slab::with_capacity(max_buffers);
  {
    let entry = clients.vacant_entry().expect("client list should have enough room at startup");
    info!("taking token {:?} for channel", entry.index());
    entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::HTTPListen })));
  }
  {
    let entry = clients.vacant_entry().expect("client list should have enough room at startup");
    info!("taking token {:?} for timer", entry.index());
    entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::HTTPListen })));
  }
  {
    let entry = clients.vacant_entry().expect("client list should have enough room at startup");
    info!("taking token {:?} for metrics", entry.index());
    entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::HTTPListen })));
  }

  let token = {
    let entry = clients.vacant_entry().expect("client list should have enough room at startup");
    let e = entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::HTTPListen })));
    Token(e.index().0)
  };

  let front = config.front.clone();
  let mut configuration = ServerConfiguration::new();
  let _ = configuration.add_listener(config, pool.clone(), token);
  let _ = configuration.activate_listener(&mut poll, &front, None);
  let (scm_server, scm_client) = UnixStream::pair().unwrap();
  let mut server = Server::new(poll, channel, ScmSocket::new(scm_server.as_raw_fd()), clients,
    pool, None ,None, Some(configuration), None, max_buffers, 60, 1800, 60);

  info!("starting event loop");
  server.run();
  info!("ending event loop");
}


#[cfg(test)]
mod tests {
  use super::*;
  use std::net::{TcpListener, TcpStream, Shutdown, IpAddr, Ipv4Addr, SocketAddr};
  use std::io::{Read,Write};
  use std::time::Duration;
  use std::{thread,str};
  use std::sync::{Arc, Barrier};
  use std::sync::atomic::{AtomicBool, Ordering, ATOMIC_BOOL_INIT};
  use sozu_command::scm_socket::Listeners;
  use std::os::unix::io::IntoRawFd;
  static TEST_FINISHED: AtomicBool = ATOMIC_BOOL_INIT;

  #[allow(unused_mut, unused_must_use, unused_variables)]
  #[test]
  fn mi() {
    setup_test_logger!();
    let barrier = Arc::new(Barrier::new(2));
    start_server(barrier.clone());
    let tx = start_proxy();
    barrier.wait();

    let mut s1 = TcpStream::connect("127.0.0.1:1234").expect("could not parse address");
    let mut s3 = TcpStream::connect("127.0.0.1:1234").expect("could not parse address");
    let mut s2 = TcpStream::connect("127.0.0.1:1234").expect("could not parse address");

    s1.write(&b"hello "[..]).map_err(|e| {
      TEST_FINISHED.store(true, Ordering::Relaxed);
      e
    }).unwrap();
    println!("s1 sent");

    s2.write(&b"pouet pouet"[..]).map_err(|e| {
      TEST_FINISHED.store(true, Ordering::Relaxed);
      e
    }).unwrap();

    println!("s2 sent");

    let mut res = [0; 128];
    s1.write(&b"coucou"[..]).map_err(|e| {
      TEST_FINISHED.store(true, Ordering::Relaxed);
      e
    }).unwrap();

    s3.shutdown(Shutdown::Both);
    let sz2 = s2.read(&mut res[..]).map_err(|e| {
      TEST_FINISHED.store(true, Ordering::Relaxed);
      e
    }).expect("could not read from socket");
    println!("s2 received {:?}", str::from_utf8(&res[..sz2]));
    assert_eq!(&res[..sz2], &b"pouet pouet"[..]);

    let sz1 = s1.read(&mut res[..]).map_err(|e| {
      TEST_FINISHED.store(true, Ordering::Relaxed);
      e
    }).expect("could not read from socket");
    println!("s1 received again({}): {:?}", sz1, str::from_utf8(&res[..sz1]));
    assert_eq!(&res[..sz1], &b"hello coucou"[..]);
    TEST_FINISHED.store(true, Ordering::Relaxed);
  }

  #[allow(unused_mut, unused_must_use, unused_variables)]
  fn start_server(barrier: Arc<Barrier>) {
    let listener = TcpListener::bind("127.0.0.1:5678").expect("could not parse address");
    fn handle_client(stream: &mut TcpStream, id: u8) {
      let mut buf = [0; 128];
      let response = b" END";
      while let Ok(sz) = stream.read(&mut buf[..]) {
        if sz > 0 {
          println!("ECHO[{}] got \"{:?}\"", id, str::from_utf8(&buf[..sz]));
          stream.write(&buf[..sz]);
        }
        if TEST_FINISHED.load(Ordering::Relaxed) {
          println!("backend server stopping");
          break;
        }
      }
    }

    let mut count = 0;
    thread::spawn(move|| {
      barrier.wait();
      for conn in listener.incoming() {
        match conn {
          Ok(mut stream) => {
            thread::spawn(move|| {
              println!("got a new client: {}", count);
              handle_client(&mut stream, count)
            });
          }
          Err(e) => { println!("connection failed"); }
        }
        count += 1;
      }
    });
  }

  pub fn start_proxy() -> Channel<OrderMessage,OrderMessageAnswer> {
    use network::proxy::ProxyClientCast;

    info!("listen for connections");
    let (mut command, channel) = Channel::generate(1000, 10000).expect("should create a channel");
    thread::spawn(move|| {
      info!("starting event loop");
      let mut poll = Poll::new().expect("could not create event loop");
      let max_buffers = 100;
      let buffer_size = 16384;
      let pool = Rc::new(RefCell::new(
        Pool::with_capacity(2*max_buffers, 0, || BufferQueue::with_capacity(buffer_size))
      ));

      let mut clients: Slab<Rc<RefCell<ProxyClientCast>>,ClientToken> = Slab::with_capacity(max_buffers);
      {
        let entry = clients.vacant_entry().expect("client list should have enough room at startup");
        info!("taking token {:?} for channel", entry.index());
        entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::HTTPListen })));
      }
      {
        let entry = clients.vacant_entry().expect("client list should have enough room at startup");
        info!("taking token {:?} for timer", entry.index());
        entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::HTTPListen })));
      }
      {
        let entry = clients.vacant_entry().expect("client list should have enough room at startup");
        info!("taking token {:?} for metrics", entry.index());
        entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::HTTPListen })));
      }

      let mut configuration = ServerConfiguration::new();
      let listener_config = TcpListenerConfig {
        front: "127.0.0.1:1234".parse().unwrap(),
        public_address: None,
        expect_proxy: false,
      };

      {
        let front = listener_config.front.clone();
        let entry = clients.vacant_entry().expect("client list should have enough room at startup");
        let _ = configuration.add_listener(listener_config, pool.clone(), Token(entry.index().0));
        let _ = configuration.activate_listener(&mut poll, &front, None);
        entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::TCPListen })));
      }

      let (scm_server, scm_client) = UnixStream::pair().unwrap();
      let scm = ScmSocket::new(scm_client.into_raw_fd());
      scm.send_listeners(Listeners {
        http: Vec::new(),
        tls:  Vec::new(),
        tcp:  Vec::new(),
      });
      let mut s   = Server::new(poll, channel, ScmSocket::new(scm_server.into_raw_fd()),
        clients, pool, None, None, Some(configuration), None, max_buffers, 60, 1800, 60);
      info!("will run");
      s.run();
      info!("ending event loop");
    });

    command.set_blocking(true);
    {
      let front = TcpFront {
        app_id: String::from("yolo"),
        address: "127.0.0.1:1234".parse().unwrap(),
      };
      let backend = messages::Backend {
        app_id: String::from("yolo"),
        backend_id: String::from("yolo-0"),
        address: "127.0.0.1:5678".parse().unwrap(),
        load_balancing_parameters: Some(LoadBalancingParams::default()),
        sticky_id: None,
        backup: None,
      };

      command.write_message(&OrderMessage { id: String::from("ID_YOLO1"), order: Order::AddTcpFront(front) });
      command.write_message(&OrderMessage { id: String::from("ID_YOLO2"), order: Order::AddBackend(backend) });
    }
    {
      let front = TcpFront {
        app_id: String::from("yolo"),
        address: "127.0.0.1:1235".parse().unwrap(),
      };
      let backend = messages::Backend {
        app_id: String::from("yolo"),
        backend_id: String::from("yolo-0"),
        address: "127.0.0.1:5678".parse().unwrap(),
        load_balancing_parameters: Some(LoadBalancingParams::default()),
        sticky_id: None,
        backup: None,
      };
      command.write_message(&OrderMessage { id: String::from("ID_YOLO3"), order: Order::AddTcpFront(front) });
      command.write_message(&OrderMessage { id: String::from("ID_YOLO4"), order: Order::AddBackend(backend) });
    }

    println!("read_message: {:?}", command.read_message().unwrap());
    println!("read_message: {:?}", command.read_message().unwrap());
    println!("read_message: {:?}", command.read_message().unwrap());
    println!("read_message: {:?}", command.read_message().unwrap());

    command
  }
}
