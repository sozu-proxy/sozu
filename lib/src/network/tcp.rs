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
use time::{Duration,precise_time_s};
use rand::random;
use uuid::Uuid;
use pool::{Pool,Checkout,Reset};

use sozu_command::buffer::Buffer;
use sozu_command::channel::Channel;
use sozu_command::scm_socket::ScmSocket;
use sozu_command::config::ProxyProtocolConfig;
use sozu_command::messages::{self,TcpFront,Order,OrderMessage,OrderMessageAnswer,OrderMessageStatus};

use network::{AppId,Backend,ClientResult,ConnectionError,RequiredEvents,Protocol,Readiness,SessionMetrics,
  ProxyClient,ProxyConfiguration,AcceptError,BackendConnectAction,BackendConnectionStatus,
  CloseResult};
use network::proxy::{Server,ProxyChannel,ListenToken,ListenPortState,ClientToken,ListenClient};
use network::buffer_queue::BufferQueue;
use network::socket::{SocketHandler,SocketResult,server_bind};
use network::{http,https_rustls};
use network::protocol::{Pipe, ProtocolResult};
use network::protocol::proxy_protocol::send::SendProxyProtocol;
use network::protocol::proxy_protocol::relay::RelayProxyProtocol;

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

pub enum State {
  Pipe(Pipe<TcpStream>),
  SendProxyProtocol(SendProxyProtocol<TcpStream>),
  RelayProxyProtocol(RelayProxyProtocol<TcpStream>),
}

pub struct Client {
  sock:           TcpStream,
  backend:        Option<TcpStream>,
  frontend_token: Token,
  backend_token:  Option<Token>,
  back_connected: BackendConnectionStatus,
  accept_token:   Token,
  status:         ConnectionStatus,
  rx_count:       usize,
  tx_count:       usize,
  app_id:         Option<String>,
  request_id:     String,
  metrics:        SessionMetrics,
  protocol:       Option<State>,
  front_buf:      Option<Checkout<BufferQueue>>,
  back_buf:       Option<Checkout<BufferQueue>>,
}

impl Client {
  fn new(sock: TcpStream, frontend_token: Token, accept_token: Token, front_buf: Checkout<BufferQueue>,
    back_buf: Checkout<BufferQueue>, proxy_protocol: Option<ProxyProtocolConfig>) -> Client {
    let s = sock.try_clone().expect("could not clone the socket");
    let addr = sock.peer_addr().map(|s| s.ip()).ok();
    let mut frontend_buffer = None;
    let mut backend_buffer = None;

      let protocol = match proxy_protocol {
      Some(ProxyProtocolConfig::RelayHeader) => {
        backend_buffer = Some(back_buf);
        Some(State::RelayProxyProtocol(RelayProxyProtocol::new(s, frontend_token, None, front_buf)))
      },
      Some(ProxyProtocolConfig::ExpectHeader) => {
        unimplemented!("ExpectHeader case not handled yet")
      },
      Some(ProxyProtocolConfig::SendHeader) => {
        frontend_buffer = Some(front_buf);
        backend_buffer = Some(back_buf);
        Some(State::SendProxyProtocol(SendProxyProtocol::new(s, frontend_token, None)))
      },
      None => Some(State::Pipe(Pipe::new(s, frontend_token, None, front_buf, back_buf, addr)))
    };

    Client {
      sock:           sock,
      backend:        None,
      frontend_token: frontend_token,
      backend_token:  None,
      back_connected: BackendConnectionStatus::NotConnected,
      accept_token:   accept_token,
      status:         ConnectionStatus::Connected,
      rx_count:       0,
      tx_count:       0,
      app_id:         None,
      request_id:     Uuid::new_v4().hyphenated().to_string(),
      metrics:        SessionMetrics::new(),
      protocol,
      front_buf: frontend_buffer,
      back_buf: backend_buffer,
    }
  }

  fn log_request(&self) {
    let client = match self.sock.peer_addr().ok() {
      None => String::from("-"),
      Some(SocketAddr::V4(addr)) => format!("{}", addr),
      Some(SocketAddr::V6(addr)) => format!("{}", addr),
    };

    let backend = match self.backend.as_ref().and_then(|backend| backend.peer_addr().ok()) {
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
    match self.protocol {
      Some(State::Pipe(ref mut pipe)) => pipe.readable(&mut self.metrics),
      Some(State::RelayProxyProtocol(ref mut pp)) => pp.readable(&mut self.metrics),
      _ => ClientResult::Continue,
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
        res = pp.back_writable();
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
      _ => unreachable!(),
    }
  }

  fn back_socket(&self)  -> Option<&TcpStream> {
    match self.protocol {
      Some(State::Pipe(ref pipe)) => pipe.back_socket(),
      Some(State::SendProxyProtocol(ref pp)) => pp.back_socket(),
      Some(State::RelayProxyProtocol(ref pp)) => pp.back_socket(),
      _ => unreachable!(),
    }
  }

  pub fn upgrade(&mut self) {
    let protocol = self.protocol.take();

    if let Some(State::SendProxyProtocol(pp)) = protocol {
      if self.back_buf.is_some() && self.front_buf.is_some() {
        self.protocol = Some(
          State::Pipe(pp.into_pipe(self.front_buf.take().unwrap(), self.back_buf.take().unwrap()))
        );
      } else {
        error!("Missing the frontend or backend buffer queue, we can't switch to a pipe");
      }
    } else if let Some(State::RelayProxyProtocol(mut pp)) = protocol {
      if self.back_buf.is_some() {
        self.protocol = Some(
          State::Pipe(pp.into_pipe(self.back_buf.take().unwrap()))
        );
      } else {
        error!("Missing the backend buffer queue, we can't switch to a pipe");
      }
    }
  }

  fn readiness(&mut self) -> &mut Readiness {
    match self.protocol {
      Some(State::Pipe(ref mut pipe)) => pipe.readiness(),
      Some(State::SendProxyProtocol(ref mut pp)) => pp.readiness(),
      Some(State::RelayProxyProtocol(ref mut pp)) => pp.readiness(),
      _ => unreachable!(),
    }
  }

  fn back_token(&self)   -> Option<Token> {
    match self.protocol {
      Some(State::Pipe(ref pipe)) => pipe.back_token(),
      Some(State::SendProxyProtocol(ref pp)) => pp.back_token(),
      Some(State::RelayProxyProtocol(ref pp)) => pp.back_token(),
      _ => unreachable!()
    }
  }

  fn set_back_socket(&mut self, socket: TcpStream) {
    match self.protocol {
      Some(State::Pipe(ref mut pipe)) => pipe.set_back_socket(socket),
      Some(State::SendProxyProtocol(ref mut pp)) => pp.set_back_socket(socket),
      Some(State::RelayProxyProtocol(ref mut pp)) => pp.set_back_socket(socket),
      _ => unreachable!()
    }
  }

  fn set_back_token(&mut self, token: Token) {
    self.backend_token = Some(token);

    match self.protocol {
      Some(State::Pipe(ref mut pipe)) => pipe.set_back_token(token),
      Some(State::SendProxyProtocol(ref mut pp)) => pp.set_back_token(token),
      Some(State::RelayProxyProtocol(ref mut pp)) => pp.set_back_token(token),
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
    }
  }

  fn metrics(&mut self)        -> &mut SessionMetrics {
    &mut self.metrics
  }

  fn remove_backend(&mut self) -> (Option<String>, Option<SocketAddr>) {

    let addr = self.backend.as_ref().and_then(|sock| sock.peer_addr().ok());
    self.backend       = None;
    self.backend_token = None;
    (self.app_id.clone(), addr)
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

    if self.back_connected() == BackendConnectionStatus::Connected {
      gauge_add!("backend.connections", -1);
    }

    if let Some(sock) = self.back_socket() {
      sock.shutdown(Shutdown::Both);
      poll.deregister(sock);
    }

    result.tokens.push(self.frontend_token);

    result
  }

  fn close_backend(&mut self, _: Token, poll: &mut Poll) -> Option<(String,SocketAddr)> {
    let mut res = None;
    if let (Some(app_id), Some(addr)) = self.remove_backend() {
      res = Some((app_id, addr.clone()));
    }

    if let Some(sock) = self.back_socket() {
      sock.shutdown(Shutdown::Both);
      poll.deregister(sock);
      gauge_add!("backend.connections", -1);
    }

    res
  }

  fn protocol(&self) -> Protocol {
    Protocol::TCP
  }

  fn process_events(&mut self, token: Token, events: Ready) {
    if self.frontend_token == token {
      self.readiness().front_readiness = self.readiness().front_readiness | UnixReady::from(events);
    } else if self.backend_token == Some(token) {
      self.readiness().back_readiness = self.readiness().back_readiness | UnixReady::from(events);
    }
  }

  fn ready(&mut self) -> ClientResult {
    let mut counter = 0;
    let max_loop_iterations = 100000;

    self.metrics().service_start();

    if self.back_connected() == BackendConnectionStatus::Connecting {
      if self.readiness().back_readiness.is_hup() {
        //retry connecting the backend
        //FIXME: there should probably be a circuit breaker per client too
        error!("error connecting to backend, trying again");
        self.metrics().service_stop();
        return ClientResult::ReconnectBackend(Some(self.frontend_token), self.backend_token.clone());
      } else if self.readiness().back_readiness != UnixReady::from(Ready::empty()) {
        self.set_back_connected(BackendConnectionStatus::Connected);
      }
    }

    let token = self.frontend_token;
    while counter < max_loop_iterations {
      let front_interest = self.readiness().front_interest & self.readiness().front_readiness;
      let back_interest  = self.readiness().back_interest & self.readiness().back_readiness;

      trace!("PROXY\t{:?} {:?} | front: {:?} | back: {:?} ", token, self.readiness(), front_interest, back_interest);

      if front_interest == UnixReady::from(Ready::empty()) && back_interest == UnixReady::from(Ready::empty()) {
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

      if front_interest.is_hup() {
        let order = self.front_hup();
        match order {
          ClientResult::CloseClient => {
            return order;
          },
          _ => {
            self.readiness().front_readiness.remove(UnixReady::hup());
            return order;
          }
        }
      }

      if back_interest.is_hup() {
        let order = self.back_hup();
        match order {
          ClientResult::CloseClient => {
            return order;
          },
          ClientResult::Continue => {
            self.readiness().front_interest.insert(Ready::writable());
            if ! self.readiness().front_readiness.is_writable() {
              break;
            }
          },
          _ => {
            self.readiness().back_readiness.remove(UnixReady::hup());
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

        self.readiness().front_interest = UnixReady::from(Ready::empty());
        self.readiness().back_interest  = UnixReady::from(Ready::empty());
        return ClientResult::CloseClient;
      }

      counter += 1;
    }

    if counter == max_loop_iterations {
      error!("PROXY\thandling client {:?} went through {} iterations, there's a probable infinite loop bug, closing the connection", self.frontend_token, max_loop_iterations);

      let front_interest = self.readiness().front_interest & self.readiness().front_readiness;
      let back_interest  = self.readiness().back_interest & self.readiness().back_readiness;

      error!("PROXY\t{:?} readiness: {:?} | front: {:?} | back: {:?} ", self.frontend_token.clone(),
        self.readiness(), front_interest, back_interest);

      return ClientResult::CloseClient;
    }

    ClientResult::Continue
  }
}

pub struct ApplicationListener {
  app_id:         String,
  sock:           Option<TcpListener>,
  token:          Option<Token>,
  front_address:  SocketAddr,
  back_addresses: Vec<SocketAddr>,
}

#[derive(Debug)]
pub struct ApplicationConfiguration {
  proxy_protocol: Option<ProxyProtocolConfig>,
}

pub struct ServerConfiguration {
  fronts:          HashMap<String, Token>,
  backends:        HashMap<String, Vec<Backend>>,
  listeners:       HashMap<Token, ApplicationListener>,
  configs:         HashMap<AppId, ApplicationConfiguration>,
  pool:            Rc<RefCell<Pool<BufferQueue>>>,
}

impl ServerConfiguration {
  pub fn new(event_loop: &mut Poll, pool: Rc<RefCell<Pool<BufferQueue>>>,
    mut tcp_listener: Vec<(AppId, TcpListener)>, mut tokens: Vec<Token>) -> (ServerConfiguration, HashSet<Token>) {

    let mut configuration = ServerConfiguration {
      backends:      HashMap::new(),
      listeners:     HashMap::new(),
      configs:       HashMap::new(),
      fronts:        HashMap::new(),
      pool:          pool,
    };

    let mut listener_tokens = HashSet::new();

    for ((app_id, listener), token) in tcp_listener.drain(..).zip(tokens.drain(..)) {
      if let Ok(front) = listener.local_addr() {
        let al = ApplicationListener {
          app_id:         app_id.clone(),
          sock:           Some(listener),
          token:          None,
          front_address:  front,
          back_addresses: Vec::new(),
        };
        if let Some(_) = configuration.add_application_listener(&app_id, al, event_loop, token) {
          listener_tokens.insert(token);
        }
      }
    }

    (configuration, listener_tokens)
  }

  pub fn give_back_listeners(&mut self) -> Vec<(String, TcpListener)> {
    let res = self.listeners.values_mut()
      .filter(|app_listener| app_listener.sock.is_some())
      .map(|app_listener| (app_listener.app_id.clone(), app_listener.sock.take().unwrap()))
      .collect();
    self.listeners.clear();
    res
  }

  fn add_application_listener(&mut self, app_id: &str, al: ApplicationListener, event_loop: &mut Poll, token: Token) -> Option<ListenToken> {
    let front = al.front_address.clone();
    self.listeners.insert(token, al);
    self.fronts.insert(String::from(app_id), token);
    if let Some(ref sock) = self.listeners[&token].sock {
      event_loop.register(sock, token, Ready::readable(), PollOpt::edge());
    }
    info!("started TCP listener for app {} on port {}", app_id, front.port());
    Some(ListenToken(token.0))
  }

  pub fn add_tcp_front(&mut self, app_id: &str, front: &SocketAddr, event_loop: &mut Poll, token: Token) -> Option<ListenToken> {
    if self.fronts.contains_key(app_id) {
      error!("TCP front already exists for app_id {}", app_id);
      return None;
    }

    if let Ok(listener) = server_bind(front) {
      let addresses: Vec<SocketAddr> = if let Some(ads) = self.backends.get(app_id) {
        let v: Vec<SocketAddr> = ads.iter().map(|backend| backend.address).collect();
        v
      } else {
        Vec::new()
      };

      let al = ApplicationListener {
        app_id:         String::from(app_id),
        sock:           Some(listener),
        token:          Some(token),
        front_address:  *front,
        back_addresses: addresses,
      };

      self.add_application_listener(app_id, al, event_loop, token)
    } else {
      error!("could not declare listener for app {} on port {}", app_id, front.port());
      None
    }
  }

  pub fn remove_tcp_front(&mut self, app_id: String, event_loop: &mut Poll) -> Option<ListenToken>{
    info!("removing tcp_front {:?}", app_id);
    // ToDo
    // Removes all listeners for the given app_id
    // an app can't have two listeners. Is this a problem?
    if let Some(&tok) = self.fronts.get(&app_id) {
      if self.listeners.contains_key(&tok) {
        self.listeners[&tok].sock.as_ref().map(|sock| event_loop.deregister(sock));
        self.listeners.remove(&tok);
        warn!("removed server {:?}", tok);
        //self.listeners[tok].sock.shutdown(Shutdown::Both);
        Some(ListenToken(tok.0))
      } else {
        None
      }
    } else {
      None
    }
  }

  pub fn add_backend(&mut self, app_id: &str, backend_id: &str, backend_address: &SocketAddr, event_loop: &mut Poll) -> Option<ListenToken> {
    use std::borrow::BorrowMut;
    if let Some(addrs) = self.backends.get_mut(app_id) {
      let id = addrs.last().map(|mut b| (*b.borrow_mut()).id ).unwrap_or(0) + 1;
      let backend = Backend::new(backend_id, *backend_address, id);
      if !addrs.contains(&backend) {
        addrs.push(backend);
      }
    }

    if self.backends.get(app_id).is_none() {
      let backend = Backend::new(backend_id, *backend_address, 0);
      self.backends.insert(String::from(app_id), vec![backend]);
    }

    let opt_tok = self.fronts.get(app_id).clone();
    if let Some(tok) = opt_tok {
      self.listeners.get_mut(&tok).map(|listener| {
        listener.back_addresses.push(*backend_address);
      });
      //let application_listener = &mut self.listeners[&tok];

      //application_listener.back_addresses.push(*backend_address);
      Some(ListenToken(tok.0))
    } else {
      error!("No front for backend {} in app {}", backend_id, app_id);
      None
    }
  }

  pub fn remove_backend(&mut self, app_id: &str, backend_address: &SocketAddr, event_loop: &mut Poll) -> Option<ListenToken>{
      // ToDo
      None
  }

}

impl ProxyConfiguration<Client> for ServerConfiguration {

  fn connect_to_backend(&mut self, poll: &mut Poll, client: &mut Client, back_token: Token) ->Result<BackendConnectAction,ConnectionError> {
    let len = self.listeners[&client.accept_token].back_addresses.len();

    if len == 0 {
      error!("no backend available");
      return Err(ConnectionError::NoBackendAvailable);
    }

    let rnd = random::<usize>();
    let idx = rnd % len;

    client.app_id = Some(self.listeners[&client.accept_token].app_id.clone());
    let backend_addr = try!(self.listeners[&client.accept_token].back_addresses.get(idx).ok_or(ConnectionError::ToBeDefined));
    let stream = try!(TcpStream::connect(backend_addr).map_err(|_| ConnectionError::ToBeDefined));
    stream.set_nodelay(true);

    poll.register(
      &stream,
      back_token,
      Ready::readable() | Ready::writable() | Ready::from(UnixReady::hup() | UnixReady::error()),
      PollOpt::edge()
    );

    client.set_back_token(back_token);
    client.set_back_socket(stream);
    client.set_back_connected(BackendConnectionStatus::Connecting);
    Ok(BackendConnectAction::New)
  }

  fn notify(&mut self, event_loop: &mut Poll, message: OrderMessage) -> OrderMessageAnswer {
    match message.order {
      /*FIXME
      Order::AddTcpFront(tcp_front) => {
        let addr_string = tcp_front.ip_address + ":" + &tcp_front.port.to_string();
        if let Ok(front) = addr_string.parse() {
          if let Some(token) = self.add_tcp_front(&tcp_front.app_id, &front, event_loop) {
            OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None}
          } else {
            error!("Couldn't add tcp front");
            OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Error(String::from("cannot add tcp front")), data: None}
          }
        } else {
          error!("Couldn't parse tcp front address");
          OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Error(String::from("cannot parse the address")), data: None}
        }
      },
      */
      Order::RemoveTcpFront(front) => {
        trace!("{:?}", front);
        let _ = self.remove_tcp_front(front.app_id, event_loop);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None}
      },
      Order::AddBackend(backend) => {
        let addr_string = backend.ip_address + ":" + &backend.port.to_string();
        let addr = &addr_string.parse().unwrap();
        if let Some(token) = self.add_backend(&backend.app_id, &backend.backend_id, addr, event_loop) {
          OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None}
        } else {
          OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Error(String::from("cannot add tcp backend")), data: None}
        }
      },
      Order::RemoveBackend(backend) => {
        trace!("{:?}", backend);
        let addr_string = backend.ip_address + ":" + &backend.port.to_string();
        let addr = &addr_string.parse().unwrap();
        if let Some(token) = self.remove_backend(&backend.app_id, addr, event_loop) {
          OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None}
        } else {
          error!("Couldn't remove tcp backend");
          OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Error(String::from("cannot remove tcp backend")), data: None}
        }
      },
      Order::SoftStop => {
        info!("{} processing soft shutdown", message.id);
        for listener in self.listeners.values() {
          listener.sock.as_ref().map(|sock| event_loop.deregister(sock));
        }
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Processing, data: None}
      },
      Order::HardStop => {
        info!("{} hard shutdown", message.id);
        for listener in self.listeners.values() {
          listener.sock.as_ref().map(|sock| event_loop.deregister(sock));
        }
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None}
      },
      Order::Status => {
        info!("{} status", message.id);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None}
      },
      Order::Logging(logging_filter) => {
        info!("{} changing logging filter to {}", message.id, logging_filter);
        ::logging::LOGGER.with(|l| {
          let directives = ::logging::parse_logging_spec(&logging_filter);
          l.borrow_mut().set_directives(directives);
        });
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      Order::AddApplication(application) => {
        let config = ApplicationConfiguration {
          proxy_protocol: application.proxy_protocol,
        };
        self.configs.insert(application.app_id, config);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      Order::RemoveApplication(_) => {
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      command => {
        error!("{} unsupported message, ignoring {:?}", message.id, command);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Error(String::from("unsupported message")), data: None}
      }
    }
  }

  fn accept(&mut self, token: ListenToken, poll: &mut Poll, client_token: Token)
    -> Result<(Rc<RefCell<Client>>, bool), AcceptError> {

    let mut p = (*self.pool).borrow_mut();

    if let (Some(front_buf), Some(back_buf)) = (p.checkout(), p.checkout()) {
      let internal_token = Token(token.0);//FIXME: ListenToken(token.0 - 2 - self.base_token);
      if self.listeners.contains_key(&internal_token) {

        let proxy_protocol = self.configs
                                .get(&self.listeners[&internal_token].app_id)
                                .and_then(|c| c.proxy_protocol.clone());

        if let Some(ref listener) = self.listeners[&internal_token].sock.as_ref() {
          listener.accept().map(|(frontend_sock, _)| {
            frontend_sock.set_nodelay(true);
            let mut c = Client::new(frontend_sock, client_token, internal_token, front_buf, back_buf, proxy_protocol);
            incr!("tcp.requests");

            poll.register(
              c.front_socket(),
              client_token,
              Ready::readable() | Ready::writable() | Ready::from(UnixReady::hup() | UnixReady::error()),
              PollOpt::edge()
            );

            (Rc::new(RefCell::new(c)), true)
          }).map_err(|e| {
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
    } else {
      error!("could not get buffers from pool");
      Err(AcceptError::TooManyClients)
    }
  }

  fn accept_flush(&mut self) -> usize {
    let mut counter = 0;
    for ref listener in self.listeners.values() {
      if let Some(ref sock) = listener.sock.as_ref() {
        while sock.accept().is_ok() {
          counter += 1;
        }
      }
    }
    counter
  }

  fn close_backend(&mut self, app_id: String, addr: &SocketAddr) {
    if let Some(app_backends) = self.backends.get_mut(&app_id) {
      if let Some(ref mut backend) = app_backends.iter_mut().find(|backend| &backend.address == addr) {
        backend.dec_connections();
      }
    }
  }

  fn listen_port_state(&self, port: &u16) -> ListenPortState {
    match self.listeners.values().find(|listener| &listener.front_address.port() == port) {
      Some(_) => ListenPortState::InUse,
      None => ListenPortState::Available
    }
  }
}

pub fn start_example() -> Channel<OrderMessage,OrderMessageAnswer> {
  use network::proxy::ProxyClientCast;

  info!("listen for connections");
  let (mut command, channel) = Channel::generate(1000, 10000).expect("should create a channel");
  thread::spawn(move|| {
    info!("starting event loop");
    let mut poll = Poll::new().expect("could not create event loop");
    let max_buffers = 10;
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
      info!("taking token {:?} for metrics", entry.index());
      entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::HTTPListen })));
    }

    let (configuration, tokens) = ServerConfiguration::new(&mut poll, pool, Vec::new(), vec!());
    let (scm_server, scm_client) = UnixStream::pair().unwrap();
    let mut s   = Server::new(poll, channel, ScmSocket::new(scm_server.as_raw_fd()),
      clients, None, None, Some(configuration), None, max_buffers);
    info!("will run");
    s.run();
    info!("ending event loop");
  });
  {
    let front = TcpFront {
      app_id: String::from("yolo"),
      ip_address: String::from("127.0.0.1"),
      port: 1234,
    };
    let backend = messages::Backend {
      app_id: String::from("yolo"),
      backend_id: String::from("yolo-0"),
      ip_address: String::from("127.0.0.1"),
      port: 5678,
    };

    command.write_message(&OrderMessage { id: String::from("ID_YOLO1"), order: Order::AddTcpFront(front) });
    command.write_message(&OrderMessage { id: String::from("ID_YOLO2"), order: Order::AddBackend(backend) });
  }
  {
    let front = TcpFront {
      app_id: String::from("yolo"),
      ip_address: String::from("127.0.0.1"),
      port: 1235,
    };
    let backend = messages::Backend {
      app_id: String::from("yolo"),
      backend_id: String::from("yolo-0"),
      ip_address: String::from("127.0.0.1"),
      port: 5678,
    };
    command.write_message(&OrderMessage { id: String::from("ID_YOLO3"), order: Order::AddTcpFront(front) });
    command.write_message(&OrderMessage { id: String::from("ID_YOLO4"), order: Order::AddBackend(backend) });
  }
  command
}

pub fn start(max_buffers: usize, buffer_size:usize, channel: ProxyChannel) {
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
    info!("taking token {:?} for metrics", entry.index());
    entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::HTTPListen })));
  }

  let token = {
    let entry = clients.vacant_entry().expect("client list should have enough room at startup");
    let e = entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::HTTPListen })));
    Token(e.index().0)
  };

  let (configuration, tokens) = ServerConfiguration::new(&mut poll, pool, Vec::new(), vec!(token));
  let (scm_server, scm_client) = UnixStream::pair().unwrap();
  let mut server = Server::new(poll, channel, ScmSocket::new(scm_server.as_raw_fd()), clients, None, None, Some(configuration), None, max_buffers);

  info!("starting event loop");
  server.run();
  info!("ending event loop");
}


#[cfg(test)]
mod tests {
  use super::*;
  use std::net::{TcpListener, TcpStream, Shutdown};
  use std::io::{Read,Write};
  use std::time::Duration;
  use std::{thread,str};

  #[allow(unused_mut, unused_must_use, unused_variables)]
  #[test]
  fn mi() {
    setup_test_logger!();
    thread::spawn(|| { start_server(); });
    let tx = start_example();
    thread::sleep(Duration::from_millis(300));

    let mut s1 = TcpStream::connect("127.0.0.1:1234").expect("could not parse address");
    let mut s3 = TcpStream::connect("127.0.0.1:1234").expect("could not parse address");
    thread::sleep(Duration::from_millis(300));
    let mut s2 = TcpStream::connect("127.0.0.1:1234").expect("could not parse address");
    s1.write(&b"hello"[..]);
    println!("s1 sent");
    s2.write(&b"pouet pouet"[..]);
    println!("s2 sent");
    thread::sleep(Duration::from_millis(500));

    let mut res = [0; 128];
    s1.write(&b"coucou"[..]);
    let mut sz1 = s1.read(&mut res[..]).expect("could not read from socket");
    println!("s1 received {:?}", str::from_utf8(&res[..sz1]));
    assert_eq!(&res[..sz1], &b"hello END"[..]);
    s3.shutdown(Shutdown::Both);
    let sz2 = s2.read(&mut res[..]).expect("could not read from socket");
    println!("s2 received {:?}", str::from_utf8(&res[..sz2]));
    assert_eq!(&res[..sz2], &b"pouet pouet END"[..]);


    thread::sleep(Duration::from_millis(400));
    sz1 = s1.read(&mut res[..]).expect("could not read from socket");
    println!("s1 received again({}): {:?}", sz1, str::from_utf8(&res[..sz1]));
    assert_eq!(&res[..sz1], &b"coucou END"[..]);
    //assert!(false);
  }

  /*
  #[allow(unused_mut, unused_must_use, unused_variables)]
  #[test]
  fn concurrent() {
    use std::sync::mpsc;
    use time;
    let thread_nb = 127;

    thread::spawn(|| { start_server(); });
    start();
    thread::sleep_ms(300);

    let (tx, rx) = mpsc::channel();

    let begin = time::precise_time_s();
    for i in 0..thread_nb {
      let id = i;
      let tx = tx.clone();
      thread::Builder::new().name(id.to_string()).spawn(move || {
        let s = format!("[{}] Hello world!\n", id);
        let v: Vec<u8> = s.bytes().collect();
        if let Ok(mut conn) = TcpStream::connect("127.0.0.1:1234") {
          let mut res = [0; 128];
          for j in 0..10000 {
            conn.write(&v[..]);

            if j % 5 == 0 {
              if let Ok(sz) = conn.read(&mut res[..]) {
                //println!("[{}] received({}): {:?}", id, sz, str::from_utf8(&res[..sz]));
              } else {
                println!("failed reading");
                tx.send(());
                return;
              }
            }
          }
          tx.send(());
          return;
        } else {
          println!("failed connecting");
          tx.send(());
          return;
        }
      });
    }
    //thread::sleep_ms(5000);
    for i in 0..thread_nb {
      rx.recv();
    }
    let end = time::precise_time_s();
    println!("executed in {} seconds", end - begin);
    assert!(false);
  }
  */

  #[allow(unused_mut, unused_must_use, unused_variables)]
  fn start_server() {
    let listener = TcpListener::bind("127.0.0.1:5678").expect("could not parse address");
    fn handle_client(stream: &mut TcpStream, id: u8) {
      let mut buf = [0; 128];
      let response = b" END";
      while let Ok(sz) = stream.read(&mut buf[..]) {
        if sz > 0 {
          println!("ECHO[{}] got \"{:?}\"", id, str::from_utf8(&buf[..sz]));
          stream.write(&buf[..sz]);
          thread::sleep(Duration::from_millis(20));
          stream.write(&response[..]);
        }
      }
    }

    let mut count = 0;
    thread::spawn(move|| {
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

}
