#![allow(dead_code, unused_must_use, unused_variables, unused_imports)]

use std::thread::{self,Thread,Builder};
use std::sync::mpsc::{self,channel,Receiver};
use std::net::SocketAddr;
use mio::tcp::*;
use mio::*;
use bytes::{ByteBuf,MutByteBuf};
use std::collections::HashMap;
use std::io::{self,Read,ErrorKind};
use nom::HexDisplay;
use std::error::Error;
use mio::util::Slab;
use std::io::Write;
use std::str::FromStr;
use std::marker::PhantomData;
use std::fmt::Debug;
use time::precise_time_ns;
use rand::random;

use network::{ClientResult,ServerMessage,ConnectionError,SocketType,socket_type,ProxyOrder,RequiredEvents};
use messages::{TcpFront,Command,Instance};

const SERVER: Token = Token(0);
const DEFAULT_FRONT_TIMEOUT: u64 = 50000;
const DEFAULT_BACK_TIMEOUT:  u64 = 50000;
type ClientToken = Token;

#[derive(Debug)]
pub struct Readiness {
  pub front_interest:  EventSet,
  pub back_interest:   EventSet,
  pub front_readiness: EventSet,
  pub back_readiness:  EventSet,
}

impl Readiness {
  pub fn new() -> Readiness {
    Readiness {
      front_interest:  EventSet::none(),
      back_interest:   EventSet::none(),
      front_readiness: EventSet::none(),
      back_readiness:  EventSet::none(),
    }
  }

  pub fn reset(&mut self) {
    self.front_interest  = EventSet::none();
    self.back_interest   = EventSet::none();
    self.front_readiness = EventSet::none();
    self.back_readiness  = EventSet::none();
  }
}

pub trait ProxyClient {
  fn front_socket(&self) -> &TcpStream;
  fn back_socket(&self)  -> Option<&TcpStream>;
  fn front_token(&self)  -> Option<Token>;
  fn back_token(&self)   -> Option<Token>;
  fn log_context(&self)  -> String;
  fn set_back_socket(&mut self, TcpStream);
  fn set_front_token(&mut self, token: Token);
  fn set_back_token(&mut self, token: Token);
  fn front_timeout(&mut self) -> Option<Timeout>;
  fn back_timeout(&mut self)  -> Option<Timeout>;
  fn set_front_timeout(&mut self, timeout: Timeout);
  fn set_back_timeout(&mut self, timeout: Timeout);
  fn set_tokens(&mut self, token: Token, backend: Token);
  fn front_hup(&mut self)     -> ClientResult;
  fn back_hup(&mut self)      -> ClientResult;
  fn readable(&mut self)      -> ClientResult;
  fn writable(&mut self)      -> ClientResult;
  fn back_readable(&mut self) -> ClientResult;
  fn back_writable(&mut self) -> ClientResult;
  fn remove_backend(&mut self) -> (Option<String>, Option<SocketAddr>);
  fn readiness(&mut self)      -> &mut Readiness;
}

pub trait ProxyConfiguration<Server:Handler,Client> {
  fn connect_to_backend(&mut self, client:&mut Client) ->Result<(),ConnectionError>;
  fn notify(&mut self, event_loop: &mut EventLoop<Server>, message: ProxyOrder);
  fn accept(&mut self, token: Token) -> Option<(Client, bool)>;
  fn front_timeout(&self) -> u64;
  fn back_timeout(&self)  -> u64;
  fn close_backend(&mut self, app_id: String, addr: &SocketAddr);
}

pub struct Server<ServerConfiguration,Client> {
  configuration:   ServerConfiguration,
  clients:         Slab<Client>,
  backend:         Slab<ClientToken>,
  max_listeners:   usize,
  max_connections: usize,
}

impl<ServerConfiguration:ProxyConfiguration<Server<ServerConfiguration,Client>, Client>,Client:ProxyClient> Server<ServerConfiguration,Client> {
  pub fn new(max_listeners: usize, max_connections: usize, configuration: ServerConfiguration) -> Self {
    Server {
      configuration:   configuration,
      clients:         Slab::new_starting_at(Token(max_listeners), max_connections),
      backend:         Slab::new_starting_at(Token(max_listeners+max_connections), max_connections),
      max_listeners:   max_listeners,
      max_connections: max_connections,
    }
  }


  pub fn configuration(&mut self) -> &mut ServerConfiguration {
    &mut self.configuration
  }

  pub fn close_client(&mut self, event_loop: &mut EventLoop<Self>, token: Token) {
    self.clients[token].front_socket().shutdown(Shutdown::Both);
    event_loop.deregister(self.clients[token].front_socket());
    if let Some(sock) = self.clients[token].back_socket() {
      sock.shutdown(Shutdown::Both);
      event_loop.deregister(sock);
    }

    self.close_backend(event_loop, token);
    self.clients.remove(token);
  }

  pub fn close_backend(&mut self, event_loop: &mut EventLoop<Self>, token: Token) {
    if let Some(backend_token) = self.clients[token].back_token() {
      if self.backend.contains(backend_token) {
        self.backend.remove(backend_token);
        if let (Some(app_id), Some(addr)) = self.clients[token].remove_backend() {
          self.configuration.close_backend(app_id, &addr);
        }
      }
    }
  }

  pub fn accept(&mut self, event_loop: &mut EventLoop<Self>, token: Token) {
    if let Some((client, should_connect)) = self.configuration.accept(token) {
      if let Ok(client_token) = self.clients.insert(client) {
        self.clients[client_token].readiness().front_interest = EventSet::readable() | EventSet::hup() | EventSet::error();
        event_loop.register(self.clients[client_token].front_socket(), client_token, EventSet::all(), PollOpt::edge() );
        &self.clients[client_token].set_front_token(client_token);
        if let Ok(timeout) = event_loop.timeout_ms(client_token.as_usize(), self.configuration.front_timeout()) {
          &self.clients[client_token].set_front_timeout(timeout);
        }
        gauge!("accept", 1);
        if should_connect {
          self.connect_to_backend(event_loop, client_token);
        }
      } else {
        error!("PROXY\tcould not add client to slab");
      }
    } else {
      error!("PROXY\tcould not create a client");
    }
  }

  pub fn connect_to_backend(&mut self, event_loop: &mut EventLoop<Self>, token: Token) {
    match self.configuration.connect_to_backend(&mut self.clients[token]) {
      Ok(()) => {
        if let Ok(backend_token) = self.backend.insert(token) {
          self.clients[token].set_back_token(backend_token);

          //self.clients[token].readiness().back_interest = EventSet::writable() | EventSet::hup() | EventSet::error();
          if let Some(sock) = self.clients[token].back_socket() {
            event_loop.register(sock, backend_token, EventSet::all(), PollOpt::edge());
          }
          if let Ok(timeout) = event_loop.timeout_ms(backend_token.as_usize(), self.configuration.back_timeout()) {
            &self.clients[token].set_back_timeout(timeout);
          }
          return;
        }
      },
      Err(ConnectionError::HostNotFound) | Err(ConnectionError::NoBackendAvailable) => {
        self.clients[token].readiness().front_interest = EventSet::writable() | EventSet::hup() | EventSet::error();
        //let mut front_interest = EventSet::hup();
        //front_interest.insert(EventSet::writable());
        //let client = &self.clients[token];

        //event_loop.reregister(client.front_socket(), token, front_interest, PollOpt::level() | PollOpt::oneshot());
      },
      _ => self.close_client(event_loop, token),
    }
  }

  pub fn get_client_token(&self, token: Token) -> Option<Token> {
    if token.as_usize() < self.max_listeners {
      None
    } else if token.as_usize() < self.max_listeners + self.max_connections && self.clients.contains(token) {
      Some(token)
    } else if token.as_usize() < self.max_listeners + 2 * self.max_connections && self.backend.contains(token) {
      if self.clients.contains(self.backend[token]) {
        Some(self.backend[token])
      } else {
        None
      }
    } else {
      None
    }
  }

  pub fn interpret_client_order(&mut self, event_loop: &mut EventLoop<Self>, token: Token, order: ClientResult) {
    //println!("INTERPRET ORDER: {:?}", order);
    match order {
      ClientResult::CloseClient      => self.close_client(event_loop, token),
      ClientResult::CloseBackend     => self.close_backend(event_loop, token),
      ClientResult::CloseBothSuccess => self.close_client(event_loop, token),
      ClientResult::CloseBothFailure => self.close_client(event_loop, token),
      ClientResult::ConnectBackend   => {
        //let mut front_interest = EventSet::hup();
        //let mut back_interest  = EventSet::hup();
        //front_interest.insert(EventSet::readable());
        //back_interest.insert(EventSet::writable());
        //self.reregister(event_loop, token, front_interest, back_interest);
        //self.clients[token].readiness().front_interest = EventSet::readable() | EventSet::hup() | EventSet::error();
        //self.clients[token].readiness().back_interest = EventSet::writable() | EventSet::hup() | EventSet::error();
        self.connect_to_backend(event_loop, token)
      },
      ClientResult::Continue         => {
        //let mut front_interest = EventSet::hup() | EventSet::error();
        //let mut back_interest  = EventSet::hup() | EventSet::error();
        //FIXME: instead of reregister, register once with Hup, Read and Write,
        //store the current interest depending on the protocol implementation,
        //and when ready is called, if there is interest, call the function right
        //away, otherwise store its readiness state
        //find a way to call the function later when the protocol shows interest
        //again
        //this will drastically reduce the number of calls to reregister
        /*
        if order.0.front_readable() {
          front_interest.insert(EventSet::readable());
        }
        if order.0.front_writable() {
          front_interest.insert(EventSet::writable());
        }
        if order.0.back_readable() {
          back_interest.insert(EventSet::readable());
        }
        if order.0.back_writable() {
          back_interest.insert(EventSet::writable());
        }
        */
        //self.clients[token].readiness().front_interest = front_interest;
        //self.clients[token].readiness().back_interest  = back_interest;
        //self.reregister(event_loop, token, front_interest, back_interest)
      }
    }
  }

  fn reregister(&self, event_loop: &mut EventLoop<Self>, token: Token, front_interest: EventSet, back_interest: EventSet) {
    let client = &self.clients[token];

    if let Some(frontend_token) = client.front_token() {
      event_loop.reregister(client.front_socket(), frontend_token, front_interest, PollOpt::level() | PollOpt::oneshot());
    }
     if let Some(backend_token) = client.back_token() {
       if let Some(sock) = client.back_socket() {
         event_loop.reregister(sock, backend_token, back_interest, PollOpt::level() | PollOpt::oneshot());
       }
     }
  }
}

impl<ServerConfiguration:ProxyConfiguration<Server<ServerConfiguration,Client>, Client>,Client:ProxyClient> Handler for Server<ServerConfiguration,Client> {
  type Timeout = usize;
  type Message = ProxyOrder;

  fn ready(&mut self, event_loop: &mut EventLoop<Self>, token: Token, events: EventSet) {
    trace!("PROXY\t{:?} got events: {:?}", token, events);

    let client_token:Token = match socket_type(token, self.max_listeners, self.max_connections) {
      Some(SocketType::Listener) => {
        if events.is_readable() {
          self.accept(event_loop, token);
          return;
        }

        if events.is_writable() {
          error!("PROXY\treceived writable for listener {:?}, this should not happen", token);
          return;
        }

        if events.is_hup() {
          error!("PROXY\tshould not happen: server {:?} closed", token);
          return;
        }
        unreachable!();
      },
      Some(SocketType::FrontClient) => {
        if self.clients.contains(token) {
          self.clients[token].readiness().front_readiness = self.clients[token].readiness().front_readiness | events;
          token
        } else {
          return;
        }
      },
      Some(SocketType::BackClient) => {
        if let Some(tok) = self.get_client_token(token) {
          self.clients[tok].readiness().back_readiness = self.clients[tok].readiness().back_readiness | events;
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
      //println!("PROXY\tclient:{:?} readiness: {:?}", client_token, self.clients[client_token].readiness());
      let front_interest = self.clients[client_token].readiness().front_interest &
        self.clients[client_token].readiness().front_readiness;
      let back_interest = self.clients[client_token].readiness().back_interest &
        self.clients[client_token].readiness().back_readiness;

      //println!("PROXY\tclient:{:?} front: {:?}", client_token, front_interest);
      //println!("PROXY\tclient:{:?} back:  {:?}", client_token, back_interest);
      //println!("PROXY\tclients: {:?}", self.clients);

      if front_interest == EventSet::none() && back_interest == EventSet::none() {
        //println!("quitting loop\n");
        break;
      } else {
        //println!("will handle");
        //println!("\n");
      }

      if !self.clients.contains(client_token) {
        //println!("the client was deleted, quitting loop\n");
        break;
      }

      if front_interest.is_readable() {
        let order = self.clients[client_token].readable();

        // FIXME: should clear the timeout only if data was consumed
        if let Some(timeout) = self.clients[client_token].front_timeout() {
          //println!("[{}] clearing timeout", token.as_usize());
          event_loop.clear_timeout(timeout);
        }
        if let Ok(timeout) = event_loop.timeout_ms(token.as_usize(), self.configuration.front_timeout()) {
          //println!("[{}] resetting timeout", token.as_usize());
          &self.clients[client_token].set_front_timeout(timeout);
        }

        self.interpret_client_order(event_loop, client_token, order);
        //self.clients[client_token].readiness().front_readiness.remove(EventSet::readable());
      }

      if back_interest.is_readable() {
        let order = self.clients[client_token].back_readable();

        // FIXME: should clear the timeout only if data was consumed
        if let Some(timeout) = self.clients[client_token].back_timeout() {
          //println!("[{}] clearing timeout", token.as_usize());
          event_loop.clear_timeout(timeout);
        }
        if let Ok(timeout) = event_loop.timeout_ms(token.as_usize(), self.configuration.back_timeout()) {
          //println!("[{}] resetting timeout", token.as_usize());
          &self.clients[client_token].set_back_timeout(timeout);
        }

        self.interpret_client_order(event_loop, client_token, order);
        //self.clients[client_token].readiness().back_readiness.remove(EventSet::readable());
      }

      if front_interest.is_writable() {
        let order = self.clients[client_token].writable();
        trace!("PROXY\tinterpreting client order {:?}", order);
        self.interpret_client_order(event_loop, client_token, order);
        //self.clients[client_token].readiness().front_readiness.remove(EventSet::writable());
      }

      if back_interest.is_writable() {
        let order = self.clients[client_token].back_writable();
        self.interpret_client_order(event_loop, client_token, order);
        //self.clients[client_token].readiness().back_readiness.remove(EventSet::writable());
      }

      if front_interest.is_hup() {
        if self.clients[client_token].front_hup() == ClientResult::CloseClient {
          self.close_client(event_loop, client_token);
          self.clients[client_token].readiness().front_interest = EventSet::none();
          self.clients[client_token].readiness().back_interest  = EventSet::none();
          self.clients[client_token].readiness().back_readiness.remove(EventSet::hup());
        }
        self.clients[client_token].readiness().front_readiness.remove(EventSet::hup());
      }
      if !self.clients.contains(client_token) {
        //println!("the client was deleted, quitting loop\n");
        break;
      }

      if back_interest.is_hup() {
        if self.clients[client_token].front_hup() == ClientResult::CloseClient {
          self.close_client(event_loop, client_token);
          self.clients[client_token].readiness().front_interest = EventSet::none();
          self.clients[client_token].readiness().back_interest  = EventSet::none();
          self.clients[client_token].readiness().front_readiness.remove(EventSet::hup());
        }
        self.clients[client_token].readiness().back_readiness.remove(EventSet::hup());
      }
    }
  }

  fn notify(&mut self, event_loop: &mut EventLoop<Self>, message: Self::Message) {
    self.configuration.notify(event_loop, message);
  }

  fn timeout(&mut self, event_loop: &mut EventLoop<Self>, timeout: Self::Timeout) {
    let token = Token(timeout);
    match socket_type(token, self.max_listeners, self.max_connections) {
      Some(SocketType::Listener) => {
        error!("PROXY\tthe listener socket should have no timeout set");
      },
      Some(SocketType::FrontClient) => {
        if self.clients.contains(token) {
          debug!("PROXY\tfrontend [{}] got timeout, closing", timeout);
          self.close_client(event_loop, token);
        }
      },
      Some(SocketType::BackClient) => {
        if let Some(tok) = self.get_client_token(token) {
          debug!("PROXY\tbackend [{}] got timeout, closing", timeout);
          self.close_client(event_loop, tok);
        }
      }
      None => {}
    }
  }

  fn interrupted(&mut self, event_loop: &mut EventLoop<Self>) {
    warn!("PROXY\tinterrupted");
  }
}

