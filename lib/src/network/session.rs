#![allow(dead_code, unused_must_use, unused_variables, unused_imports)]

use std::thread::{self,Thread,Builder};
use std::sync::mpsc::TryRecvError;
use std::net::{SocketAddr,Shutdown};
use mio::net::*;
use mio::*;
use mio::unix::UnixReady;
use std::collections::{HashSet,HashMap};
use std::io::{self,Read,ErrorKind};
use nom::HexDisplay;
use std::error::Error;
use slab::{Slab,VacantEntry};
use std::io::Write;
use std::str::FromStr;
use std::marker::PhantomData;
use std::fmt::Debug;
use time::{precise_time_ns,SteadyTime,Duration};
use rand::random;

use sozu_command::channel::Channel;
use sozu_command::messages::{self,TcpFront,Order,Instance,MessageId,OrderMessage,
  OrderMessageAnswer,OrderMessageStatus};

use network::{ClientResult,ConnectionError,
  SocketType,Protocol,RequiredEvents};

const SERVER: Token = Token(0);
const DEFAULT_FRONT_TIMEOUT: u64 = 50000;
const DEFAULT_BACK_TIMEOUT:  u64 = 50000;

pub type ProxyChannel = Channel<OrderMessageAnswer,OrderMessage>;

#[derive(Copy,Clone,Debug,PartialEq,Eq,PartialOrd,Ord,Hash)]
pub struct ListenToken(pub usize);
#[derive(Copy,Clone,Debug,PartialEq,Eq,PartialOrd,Ord,Hash)]
pub struct FrontToken(pub usize);
#[derive(Copy,Clone,Debug,PartialEq,Eq,PartialOrd,Ord,Hash)]
pub struct BackToken(pub usize);

impl From<usize> for ListenToken {
    fn from(val: usize) -> ListenToken {
        ListenToken(val)
    }
}

impl From<ListenToken> for usize {
    fn from(val: ListenToken) -> usize {
        val.0
    }
}

impl From<usize> for FrontToken {
    fn from(val: usize) -> FrontToken {
        FrontToken(val)
    }
}

impl From<FrontToken> for usize {
    fn from(val: FrontToken) -> usize {
        val.0
    }
}

impl From<usize> for BackToken {
    fn from(val: usize) -> BackToken {
        BackToken(val)
    }
}

impl From<BackToken> for usize {
    fn from(val: BackToken) -> usize {
        val.0
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

pub trait ProxyClient {
  fn front_socket(&self) -> &TcpStream;
  fn back_socket(&self)  -> Option<&TcpStream>;
  fn front_token(&self)  -> Option<Token>;
  fn back_token(&self)   -> Option<Token>;
  fn close(&mut self);
  fn set_back_socket(&mut self, TcpStream);
  fn set_front_token(&mut self, token: Token);
  fn set_back_token(&mut self, token: Token);
  fn back_connected(&self)     -> BackendConnectionStatus;
  fn set_back_connected(&mut self, connected: BackendConnectionStatus);
  fn remove_backend(&mut self) -> (Option<String>, Option<SocketAddr>);
  fn readiness(&mut self)      -> &mut Readiness;
  fn protocol(&self)           -> Protocol;
  fn metrics(&mut self)        -> &mut SessionMetrics;
  fn ready(&mut self)          -> ClientResult;
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

pub trait ProxyConfiguration<Client> {
  fn connect_to_backend(&mut self, event_loop: &mut Poll, client:&mut Client) ->Result<BackendConnectAction,ConnectionError>;
  fn notify(&mut self, event_loop: &mut Poll, message: OrderMessage) -> OrderMessageAnswer;
  fn accept(&mut self, token: ListenToken, event_loop: &mut Poll, entry: VacantEntry<Client, FrontToken>,
           client_token: Token) -> Result<(FrontToken, bool), AcceptError>;
  fn close_backend(&mut self, app_id: String, addr: &SocketAddr);
}

pub struct Session<ServerConfiguration,Client> {
  pub configuration:   ServerConfiguration,
  clients:         Slab<Client,FrontToken>,
  backend:         Slab<FrontToken,BackToken>,
  max_listeners:   usize,
  max_connections: usize,
  shutting_down:   Option<MessageId>,
  accept_ready:    HashSet<ListenToken>,
  can_accept:      bool,
  base_token:      usize,
}

impl<ServerConfiguration:ProxyConfiguration<Client>,Client:ProxyClient> Session<ServerConfiguration,Client> {
  pub fn new(max_listeners: usize, max_connections: usize, base_token: usize, configuration: ServerConfiguration,
    accept_ready: HashSet<ListenToken>, poll: &mut Poll) -> Self {

    let clients = Slab::with_capacity(max_connections);
    let backend = Slab::with_capacity(max_connections);
    Session {
      configuration:   configuration,
      clients:         clients,
      backend:         backend,
      max_listeners:   max_listeners,
      max_connections: max_connections,
      shutting_down:   None,
      accept_ready:    accept_ready,
      can_accept:      true,
      base_token:      base_token,
    }
  }

  pub fn to_front(&self, token: Token) -> FrontToken {
    FrontToken(token.0 - 2 - self.max_listeners - self.base_token)
  }

  pub fn to_back(&self, token: Token) -> BackToken {
    BackToken(token.0 - 2 - self.max_listeners - self.max_connections - self.base_token)
  }

  pub fn from_front(&self, token: FrontToken) -> Token {
    Token(token.0 + 2 + self.max_listeners + self.base_token)
  }

  pub fn from_front_add(&self) -> usize {
    2 + self.max_listeners + self.base_token
  }

  pub fn from_back(&self, token: BackToken) -> Token {
    Token(token.0 + 2 + self.max_listeners + self.max_connections + self.base_token)
  }


  pub fn configuration(&mut self) -> &mut ServerConfiguration {
    &mut self.configuration
  }

  pub fn close_client(&mut self, poll: &mut Poll, token: FrontToken) {
    self.clients[token].metrics().service_stop();
    self.clients[token].front_socket().shutdown(Shutdown::Both);
    poll.deregister(self.clients[token].front_socket());
    if let Some(sock) = self.clients[token].back_socket() {
      sock.shutdown(Shutdown::Both);
      poll.deregister(sock);
    }

    self.close_backend(token);
    self.clients[token].close();
    self.clients.remove(token);
    decr!("client.connections");

    self.can_accept = true;
  }

  pub fn close_backend(&mut self, token: FrontToken) {
    if let Some(backend_tk) = self.clients[token].back_token() {
      let backend_token = self.to_back(backend_tk);
      if self.backend.contains(backend_token) {
        self.backend.remove(backend_token);
        if let (Some(app_id), Some(addr)) = self.clients[token].remove_backend() {
          self.configuration.close_backend(app_id, &addr);
          decr!("backend.connections");
        }
      }
    }
  }

  pub fn accept(&mut self, poll: &mut Poll, token: ListenToken) -> bool {
    let add = self.from_front_add();
    let res = {
      let entry = self.clients.vacant_entry().expect("FIXME");
      let client_token = Token(entry.index().0 + add);
      self.configuration.accept(token, poll, entry, client_token)
    };

    match res {
      Err(AcceptError::IoError) => {
        //FIXME: do we stop accepting?
        false
      },
      Err(AcceptError::TooManyClients) => {
        self.can_accept = false;
        false
      },
      Err(AcceptError::WouldBlock) => {
        self.accept_ready.remove(&token);
        false
      },
      Ok((client_token, should_connect)) => {
        if should_connect {
           self.connect_to_backend(poll, client_token);
        }

        true
      }
    }
  }

  pub fn connect_to_backend(&mut self, poll: &mut Poll, token: FrontToken) {
    match self.configuration.connect_to_backend(poll, &mut self.clients[token]) {
      Ok(BackendConnectAction::Reuse) => {
        debug!("keepalive, reusing backend connection");
      }
      Ok(BackendConnectAction::Replace) => {
        if let Some(backend_token) = self.clients[token].back_token() {
          if let Some(sock) = self.clients[token].back_socket() {
            poll.register(
              sock,
              backend_token,
              Ready::readable() | Ready::writable() | Ready::from(UnixReady::hup() | UnixReady::error()),
              PollOpt::edge()
            );
          }

          return;
        }
      },
      Ok(BackendConnectAction::New) => {
        incr!("backend.connections");
        if let Ok(backend_token) = self.backend.insert(token) {
          let back = self.from_back(backend_token);
          self.clients[token].set_back_token(back);

          //self.clients[token].readiness().back_interest = Ready::writable() | UnixReady::hup() | UnixReady::error();
          if let Some(sock) = self.clients[token].back_socket() {
            poll.register(
              sock,
              back,
              Ready::readable() | Ready::writable() | Ready::from(UnixReady::hup() | UnixReady::error()),
              PollOpt::edge()
            );
          }

          let back = self.from_back(backend_token);
          return;
        }
      },
      Err(ConnectionError::HostNotFound) | Err(ConnectionError::NoBackendAvailable) | Err(ConnectionError::HttpsRedirect) => {
        self.clients[token].readiness().front_interest = UnixReady::from(Ready::writable()) | UnixReady::hup() | UnixReady::error();
        self.clients[token].readiness().back_interest  = UnixReady::hup() | UnixReady::error();
      },
      _ => self.close_client(poll, token),
    }
  }

  pub fn get_client_token(&self, token: Token) -> Option<FrontToken> {
    if token.0 < 2 + self.max_listeners + self.base_token {
      None
    } else if token.0 < 2 + self.max_listeners + self.max_connections + self.base_token && self.clients.contains(self.to_front(token)) {
      Some(self.to_front(token))
    } else if token.0 < 2 + self.max_listeners + 2 * self.max_connections + self.base_token && self.backend.contains(self.to_back(token)) {
      if self.clients.contains(self.backend[self.to_back(token)]) {
        Some(self.backend[self.to_back(token)])
      } else {
        None
      }
    } else {
      None
    }
  }

  pub fn interpret_client_order(&mut self, poll: &mut Poll, token: FrontToken, order: ClientResult) {
    //println!("INTERPRET ORDER: {:?}", order);
    match order {
      ClientResult::CloseClient     => self.close_client(poll, token),
      //FIXME: we do not deregister in close_backend
      ClientResult::CloseBackend    => self.close_backend(token),
      ClientResult::CloseBoth       => self.close_client(poll, token),
      ClientResult::ConnectBackend  => self.connect_to_backend(poll, token),
      ClientResult::Continue        => {}
    }
  }
}

//type Timeout = usize;

impl<ServerConfiguration:ProxyConfiguration<Client>,Client:ProxyClient> Session<ServerConfiguration,Client> {

  pub fn ready(&mut self, poll: &mut Poll, token: Token, events: Ready) {
    //info!("PROXY\t{:?} got events: {:?}", token, events);

    let client_token:FrontToken = match socket_type(token, self.base_token, self.max_listeners, self.max_connections) {
      Some(SocketType::Listener) => {
        if events.is_readable() {
          self.accept_ready.insert(ListenToken(token.0));
          loop {
            if !self.accept(poll, ListenToken(token.0)) {
              break;
            }
          }
          return;
        }

        if events.is_writable() {
          error!("received writable for listener {:?}, this should not happen", token);
          return;
        }

        if UnixReady::from(events).is_hup() {
          error!("should not happen: server {:?} closed", token);
          return;
        }
        unreachable!();
      },
      Some(SocketType::FrontClient) => {
        let front_token = self.to_front(token);
        if self.clients.contains(front_token) {
          self.clients[front_token].readiness().front_readiness = self.clients[front_token].readiness().front_readiness | UnixReady::from(events);
          front_token
        } else {
          return;
        }
      },
      Some(SocketType::BackClient) => {
        if let Some(tok) = self.get_client_token(token) {
          self.clients[tok].metrics().service_start();

          self.clients[tok].readiness().back_readiness = self.clients[tok].readiness().back_readiness | UnixReady::from(events);

          if self.clients[tok].back_connected() == BackendConnectionStatus::Connecting {
            if self.clients[tok].readiness().back_readiness.is_hup() {
              //retry connecting the backend
              //FIXME: there should probably be a circuit breaker per client too
              error!("error connecting to backend, trying again");
              self.connect_to_backend(poll, tok);
            } else {
              self.clients[tok].set_back_connected(BackendConnectionStatus::Connected);
            }
          }

          self.clients[tok].metrics().service_stop();

          tok
        } else {
          return;
        }
      },
      None => {
        return;
      }
    };

    loop {
    //self.client_ready(poll, client_token, events);
      let order = self.clients[client_token].ready();
      info!("client[{:?}] got events {:?} and returned order {:?}", client_token, events, order);
      let is_connect = order == ClientResult::ConnectBackend;
      self.interpret_client_order(poll, client_token, order);

      // if we had to connect to a backend server, go back to the loop
      // I'm not sure we would have anything to do right away, though,
      // so maybe we can just stop there for that client?
      // also the events would change?
      if !is_connect {
        break;
      }
    }
  }

  fn notify(&mut self, poll: &mut Poll, message: OrderMessage) -> OrderMessageAnswer {
    self.configuration.notify(poll, message)
  }

  pub fn handle_remaining_readiness(&mut self, poll: &mut Poll) {
    // try to accept again after handling all client events,
    // since we might have released a few client slots
    if !self.accept_ready.is_empty() && self.can_accept {
      loop {
        if let Some(token) = self.accept_ready.iter().next().map(|token| ListenToken(token.0)) {
          if !self.accept(poll, token) {
            if !self.accept_ready.is_empty() && self.can_accept {
              break;
            }
          }
        } else {
          // we don't have any more elements to loop over
          break;
        }
      }
    }
  }
}

pub fn socket_type(token: Token, base_token: usize, max_listeners: usize, max_connections: usize) -> Option<SocketType> {
  if token.0 < 2 + max_listeners + base_token {
    Some(SocketType::Listener)
  } else if token.0 < 2 + max_listeners + max_connections + base_token {
    Some(SocketType::FrontClient)
  } else if token.0 < 2 + max_listeners + 2 * max_connections + base_token {
    Some(SocketType::BackClient)
  } else {
    None
  }
}

