#![allow(dead_code, unused_must_use, unused_variables, unused_imports)]

use std::thread::{self,Thread,Builder};
use std::sync::mpsc::TryRecvError;
use std::net::{SocketAddr,Shutdown};
use mio::net::*;
use mio::*;
use mio::unix::UnixReady;
use mio::timer::{Timer,Timeout};
use std::collections::{HashSet,HashMap};
use std::io::{self,Read,ErrorKind};
use nom::HexDisplay;
use std::error::Error;
use slab::Slab;
use std::io::Write;
use std::str::FromStr;
use std::marker::PhantomData;
use std::fmt::Debug;
use time::precise_time_ns;
use std::time::Duration;
use rand::random;

use network::{ClientResult,ConnectionError,
  SocketType,Protocol,RequiredEvents};
use messages::{self,TcpFront,Order,Instance,MessageId,OrderMessage,
  OrderMessageAnswer,OrderMessageStatus};
use channel::Channel;

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

pub trait ProxyClient {
  fn front_socket(&self) -> &TcpStream;
  fn back_socket(&self)  -> Option<&TcpStream>;
  fn front_token(&self)  -> Option<Token>;
  fn back_token(&self)   -> Option<Token>;
  fn close(&mut self);
  fn log_context(&self)  -> String;
  fn set_back_socket(&mut self, TcpStream);
  fn set_front_token(&mut self, token: Token);
  fn set_back_token(&mut self, token: Token);
  fn back_connected(&self)     -> BackendConnectionStatus;
  fn set_back_connected(&mut self, connected: BackendConnectionStatus);
  fn front_timeout(&mut self) -> Option<Timeout>;
  fn back_timeout(&mut self)  -> Option<Timeout>;
  fn set_front_timeout(&mut self, timeout: Timeout);
  fn set_back_timeout(&mut self, timeout: Timeout);
  fn front_hup(&mut self)     -> ClientResult;
  fn back_hup(&mut self)      -> ClientResult;
  fn readable(&mut self)      -> ClientResult;
  fn writable(&mut self)      -> ClientResult;
  fn back_readable(&mut self) -> ClientResult;
  fn back_writable(&mut self) -> ClientResult;
  fn remove_backend(&mut self) -> (Option<String>, Option<SocketAddr>);
  fn readiness(&mut self)      -> &mut Readiness;
  fn protocol(&self)           -> Protocol;
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
  fn accept(&mut self, token: ListenToken) -> Result<(Client, bool), AcceptError>;
  fn close_backend(&mut self, app_id: String, addr: &SocketAddr);
  fn front_timeout(&self) -> u64;
  fn back_timeout(&self)  -> u64;
}

pub struct Session<ServerConfiguration,Client> {
  configuration:   ServerConfiguration,
  clients:         Slab<Client,FrontToken>,
  backend:         Slab<FrontToken,BackToken>,
  max_listeners:   usize,
  max_connections: usize,
  timer:           Timer<Token>,
  shutting_down:   Option<MessageId>,
  accept_ready:    HashSet<ListenToken>,
  can_accept:      bool,
  base_token:      usize,
}

impl<ServerConfiguration:ProxyConfiguration<Client>,Client:ProxyClient> Session<ServerConfiguration,Client> {
  pub fn new(max_listeners: usize, max_connections: usize, base_token: usize, configuration: ServerConfiguration, poll: &mut Poll) -> Self {
    let clients = Slab::with_capacity(max_connections);
    let backend = Slab::with_capacity(max_connections);
    //let timer   = timer::Builder::default().tick_duration(Duration::from_millis(1000)).build();
    let timer   = Timer::default();
    //FIXME: registering the timer makes the timer thread spin too much
    //poll.register(&timer, Token(1), Ready::readable(), PollOpt::edge()).expect("should register the timer");
    Session {
      configuration:   configuration,
      clients:         clients,
      backend:         backend,
      max_listeners:   max_listeners,
      max_connections: max_connections,
      timer:           timer,
      shutting_down:   None,
      accept_ready:    HashSet::new(),
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

  pub fn from_back(&self, token: BackToken) -> Token {
    Token(token.0 + 2 + self.max_listeners + self.max_connections + self.base_token)
  }


  pub fn configuration(&mut self) -> &mut ServerConfiguration {
    &mut self.configuration
  }

  pub fn close_client(&mut self, poll: &mut Poll, token: FrontToken) {
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
    match self.configuration.accept(token) {
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
      Ok((client, should_connect)) => {
        if let Ok(client_token) = self.clients.insert(client) {
          self.clients[client_token].readiness().front_interest = UnixReady::from(Ready::readable()) | UnixReady::hup() | UnixReady::error();
          poll.register(
            self.clients[client_token].front_socket(),
            self.from_front(client_token),
            Ready::readable() | Ready::writable() | Ready::from(UnixReady::hup() | UnixReady::error()),
            PollOpt::edge()
          );
          let front = self.from_front(client_token);
          &self.clients[client_token].set_front_token(front);

          let front = self.from_front(client_token);
          /*
          if let Ok(timeout) = self.timer.set_timeout(Duration::from_millis(self.configuration.front_timeout()), front) {
          &self.clients[client_token].set_front_timeout(timeout);
          }
          */
          incr!("client.connections");
          if should_connect {
            self.connect_to_backend(poll, client_token);
          }

          // should continue accepting
          true
        } else {
          error!("PROXY\tcould not add client to slab");
          false
        }
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

          /*
          if let Ok(timeout) = self.timer.set_timeout(Duration::from_millis(self.configuration.back_timeout()), backend_token) {
            &self.clients[token].set_back_timeout(timeout);
          }
          */
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
          /*
          if let Ok(timeout) = self.timer.set_timeout(Duration::from_millis(self.configuration.back_timeout()), back) {
            &self.clients[token].set_back_timeout(timeout);
          }
          */
          return;
        }
      },
      Err(ConnectionError::HostNotFound) | Err(ConnectionError::NoBackendAvailable) => {
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
      ClientResult::CloseClient      => self.close_client(poll, token),
      //FIXME: we do not deregister in close_backend
      ClientResult::CloseBackend     => self.close_backend(token),
      ClientResult::CloseBothSuccess => self.close_client(poll, token),
      ClientResult::CloseBothFailure => self.close_client(poll, token),
      ClientResult::ConnectBackend   => self.connect_to_backend(poll, token),
      ClientResult::Continue         => {}
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
      let front_interest = self.clients[client_token].readiness().front_interest &
        self.clients[client_token].readiness().front_readiness;
      let back_interest = self.clients[client_token].readiness().back_interest &
        self.clients[client_token].readiness().back_readiness;

      //info!("PROXY\t{:?} {:?} | {:?} front: {:?} | back: {:?} ", client_token, events, self.clients[client_token].readiness(), front_interest, back_interest);

      if front_interest == UnixReady::from(Ready::empty()) && back_interest == UnixReady::from(Ready::empty()) {
        break;
      }

      if !self.clients.contains(client_token) {
        break;
      }

      if front_interest.is_readable() {
        let order = self.clients[client_token].readable();
        trace!("front readable\tinterpreting client order {:?}", order);

        // FIXME: should clear the timeout only if data was consumed
        if let Some(timeout) = self.clients[client_token].front_timeout() {
          //println!("[{}] clearing timeout", token.as_usize());
          self.timer.cancel_timeout(&timeout);
        }
        let front = self.from_front(client_token);
        /*
        if let Ok(timeout) = self.timer.set_timeout(Duration::from_millis(self.configuration.front_timeout()), front) {
          //println!("[{}] resetting timeout", front);
          &self.clients[client_token].set_front_timeout(timeout);
        }
        */

        self.interpret_client_order(poll, client_token, order);
        //self.clients[client_token].readiness().front_readiness.remove(Ready::readable());
        if !self.clients.contains(client_token) {
          break;
        }

      }

      if back_interest.is_writable() {
        let order = self.clients[client_token].back_writable();
        self.interpret_client_order(poll, client_token, order);
        //self.clients[client_token].readiness().back_readiness.remove(Ready::writable());
        if !self.clients.contains(client_token) {
          break;
        }

      }

      if back_interest.is_readable() {
        let order = self.clients[client_token].back_readable();

        // FIXME: should clear the timeout only if data was consumed
        if let Some(timeout) = self.clients[client_token].back_timeout() {
          //println!("[{}] clearing timeout", token.as_usize());
          self.timer.cancel_timeout(&timeout);
        }
        /*
        if let Some(back) = self.clients[client_token].back_token() {
          if let Ok(timeout) = self.timer.set_timeout(Duration::from_millis(self.configuration.back_timeout()), back) {
            //println!("[{}] resetting timeout", back);
            &self.clients[client_token].set_back_timeout(timeout);
          }
        }
        */

        self.interpret_client_order(poll, client_token, order);
        //self.clients[client_token].readiness().back_readiness.remove(Ready::readable());
        if !self.clients.contains(client_token) {
          break;
        }

      }

      if front_interest.is_writable() {
        let order = self.clients[client_token].writable();
        trace!("front writable\tinterpreting client order {:?}", order);
        self.interpret_client_order(poll, client_token, order);
        //self.clients[client_token].readiness().front_readiness.remove(Ready::writable());
      }

      if !self.clients.contains(client_token) {
        break;
      }

      if front_interest.is_hup() {
        if self.clients[client_token].front_hup() == ClientResult::CloseClient {
          self.clients[client_token].readiness().front_interest = UnixReady::from(Ready::empty());
          self.clients[client_token].readiness().back_interest  = UnixReady::from(Ready::empty());
          self.clients[client_token].readiness().back_readiness.remove(UnixReady::hup());
          self.close_client(poll, client_token);
          break;
        } else {
          self.clients[client_token].readiness().front_readiness.remove(UnixReady::hup());
        }
      }

      if back_interest.is_hup() {
        if self.clients[client_token].front_hup() == ClientResult::CloseClient {
          self.clients[client_token].readiness().front_interest = UnixReady::from(Ready::empty());
          self.clients[client_token].readiness().back_interest  = UnixReady::from(Ready::empty());
          self.clients[client_token].readiness().front_readiness.remove(UnixReady::hup());
          self.close_client(poll, client_token);
          break;
        } else {
          self.clients[client_token].readiness().back_readiness.remove(UnixReady::hup());
        }
      }

      if front_interest.is_error() || back_interest.is_error() {
        if front_interest.is_error() {
          error!("PROXY client {:?} front error, disconnecting", client_token);
        } else {
          error!("PROXY client {:?} back error, disconnecting", client_token);
        }
        self.clients[client_token].readiness().front_interest = UnixReady::from(Ready::empty());
        self.clients[client_token].readiness().back_interest  = UnixReady::from(Ready::empty());
        self.close_client(poll, client_token);
        break;
      }
    }
  }

  fn notify(&mut self, poll: &mut Poll, message: OrderMessage) -> OrderMessageAnswer {
    self.configuration.notify(poll, message)
  }

  fn timeout(&mut self, poll: &mut Poll, token: Token) {
    match socket_type(token, self.base_token, self.max_listeners, self.max_connections) {
      Some(SocketType::Listener) => {
        error!("PROXY\tthe listener socket should have no timeout set");
      },
      Some(SocketType::FrontClient) => {
        let front_token = self.to_front(token);
        if self.clients.contains(front_token) {
          debug!("PROXY\tfrontend [{:?}] got timeout, closing", token);
          self.close_client(poll, front_token);
        }
      },
      Some(SocketType::BackClient) => {
        if let Some(tok) = self.get_client_token(token) {
          debug!("PROXY\tbackend [{:?}] got timeout, closing", token);
          self.close_client(poll, tok);
        }
      }
      None => {}
    }
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

