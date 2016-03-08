#![allow(dead_code, unused_must_use, unused_variables, unused_imports)]

use std::thread::{self,Thread,Builder};
use std::sync::mpsc::{self,channel,Receiver};
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
use network::metrics::{METRICS,ProxyMetrics};

use messages::{TcpFront,Command,Instance};

const SERVER: Token = Token(0);
const FRONT_TIMEOUT: u64 = 10000;
const BACK_TIMEOUT:  u64 = 10000;
type ClientToken = Token;

pub trait ProxyClient {
  fn front_socket(&self) -> &TcpStream;
  fn back_socket(&self)  -> Option<&TcpStream>;
  fn front_token(&self)  -> Option<Token>;
  fn back_token(&self)   -> Option<Token>;
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
  fn readable(&mut self)      -> (RequiredEvents, ClientResult);
  fn writable(&mut self)      -> (RequiredEvents, ClientResult);
  fn back_readable(&mut self) -> (RequiredEvents, ClientResult);
  fn back_writable(&mut self) -> (RequiredEvents, ClientResult);
  fn remove_backend(&mut self);
}

pub trait ProxyConfiguration<Server:Handler,Client> {
  fn connect_to_backend(&mut self, client:&mut Client) ->Result<TcpStream,ConnectionError>;
  fn notify(&mut self, event_loop: &mut EventLoop<Server>, message: ProxyOrder);
  fn accept(&mut self, token: Token) -> Option<(Client, bool)>;
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

    if let Some(backend_token) = self.clients[token].back_token() {
      if self.backend.contains(backend_token) {
        self.backend.remove(backend_token);
      }
    }
    self.clients.remove(token);
  }

  pub fn close_backend(&mut self, event_loop: &mut EventLoop<Self>, token: Token) {
    if let Some(backend_token) = self.clients[token].back_token() {
      if self.backend.contains(backend_token) {
        self.backend.remove(backend_token);
        self.clients[token].remove_backend();
      }
    }
  }

  pub fn accept(&mut self, event_loop: &mut EventLoop<Self>, token: Token) {
    if let Some((client, should_connect)) = self.configuration.accept(token) {
      if let Ok(client_token) = self.clients.insert(client) {
        event_loop.register(self.clients[client_token].front_socket(), client_token, EventSet::readable(), PollOpt::edge());
        &self.clients[client_token].set_front_token(client_token);
        if let Ok(timeout) = event_loop.timeout_ms(client_token.as_usize(), FRONT_TIMEOUT) {
          &self.clients[client_token].set_front_timeout(timeout);
        }
        METRICS.lock().unwrap().gauge("accept", 1);
        if should_connect {
          self.connect_to_backend(event_loop, client_token);
        }
      } else {
        error!("could not add client to slab");
      }
    } else {
      error!("could not create a client");
    }
  }

  pub fn connect_to_backend(&mut self, event_loop: &mut EventLoop<Self>, token: Token) {
    match self.configuration.connect_to_backend(&mut self.clients[token]) {
      Ok(socket) => {
        if let Ok(backend_token) = self.backend.insert(token) {
          self.clients[token].set_back_socket(socket);
          self.clients[token].set_back_token(backend_token);

          if let Some(sock) = self.clients[token].back_socket() {
            event_loop.register(sock, backend_token, EventSet::writable(), PollOpt::edge());
          }
          if let Ok(timeout) = event_loop.timeout_ms(backend_token.as_usize(), BACK_TIMEOUT) {
            &self.clients[token].set_back_timeout(timeout);
          }
          return;
        }
      },
      Err(ConnectionError::HostNotFound) | Err(ConnectionError::NoBackendAvailable) => {
        let mut front_interest = EventSet::hup();
        front_interest.insert(EventSet::writable());
        let client = &self.clients[token];

        event_loop.reregister(client.front_socket(), token, front_interest, PollOpt::level() | PollOpt::oneshot());
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

  pub fn interpret_client_order(&mut self, event_loop: &mut EventLoop<Self>, token: Token, order: (RequiredEvents, ClientResult)) {
    //println!("ORDER: {:?}", order);
    match order.1 {
      ClientResult::CloseClient      => self.close_client(event_loop, token),
      ClientResult::CloseBackend     => self.close_backend(event_loop, token),
      ClientResult::CloseBothSuccess => self.close_client(event_loop, token),
      ClientResult::CloseBothFailure => self.close_client(event_loop, token),
      ClientResult::ConnectBackend   => {
        let mut front_interest = EventSet::hup();
        let mut back_interest  = EventSet::hup();
        front_interest.insert(EventSet::readable());
        back_interest.insert(EventSet::writable());
        self.reregister(event_loop, token, front_interest, back_interest);
        self.connect_to_backend(event_loop, token)
      },
      ClientResult::Continue         => {
        let mut front_interest = EventSet::hup();
        let mut back_interest  = EventSet::hup();
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
        self.reregister(event_loop, token, front_interest, back_interest)
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
    trace!("{:?} got events: {:?}", token, events);
    if events.is_readable() {
      trace!("{:?} is readable", token);

      match socket_type(token, self.max_listeners, self.max_connections) {
        Some(SocketType::Listener) => {
          self.accept(event_loop, token)
        }

        Some(SocketType::FrontClient) => {
          if self.clients.contains(token) {
            let order = self.clients[token].readable();

            // FIXME: should clear the timeout only if data was consumed
            if let Some(timeout) = self.clients[token].front_timeout() {
              //println!("[{}] clearing timeout", token.as_usize());
              event_loop.clear_timeout(timeout);
            }
            if let Ok(timeout) = event_loop.timeout_ms(token.as_usize(), FRONT_TIMEOUT) {
              //println!("[{}] resetting timeout", token.as_usize());
              &self.clients[token].set_front_timeout(timeout);
            }

            self.interpret_client_order(event_loop, token, order);
          } else {
            info!("client {:?} was removed", token);
          }
        }

        Some(SocketType::BackClient) => {
          if let Some(tok) = self.get_client_token(token) {
            let order = self.clients[tok].back_readable();

            // FIXME: should clear the timeout only if data was consumed
            if let Some(timeout) = self.clients[tok].back_timeout() {
              //println!("[{}] clearing timeout", token.as_usize());
              event_loop.clear_timeout(timeout);
            }
            if let Ok(timeout) = event_loop.timeout_ms(token.as_usize(), BACK_TIMEOUT) {
              //println!("[{}] resetting timeout", token.as_usize());
              &self.clients[tok].set_back_timeout(timeout);
            }

            self.interpret_client_order(event_loop, tok, order);
          }
        }

        None => {}
      }
    }

    if events.is_writable() {
      trace!("{:?} is writable", token);

      match socket_type(token, self.max_listeners, self.max_connections) {
        Some(SocketType::Listener) => {
          error!("received writable for listener {:?}, this should not happen", token);
        }

        Some(SocketType::FrontClient) => {
          if self.clients.contains(token) {
            let order = self.clients[token].writable();
            trace!("interpreting client order {:?}", order);
            self.interpret_client_order(event_loop, token, order);
          } else {
            info!("client {:?} was removed", token);
          }
        }

        Some(SocketType::BackClient) => {
          if let Some(tok) = self.get_client_token(token) {
            let order = self.clients[tok].back_writable();
            self.interpret_client_order(event_loop, tok, order);
          }
        }

        None => {}
      }
    }

    if events.is_hup() {
      match socket_type(token, self.max_listeners, self.max_connections) {
        Some(SocketType::Listener) => {
          error!("should not happen: server {:?} closed", token);
        }

        Some(SocketType::FrontClient) => {
          if self.clients.contains(token) {
            if self.clients[token].front_hup() == ClientResult::CloseClient {
              self.close_client(event_loop, token);
            }
          } else {
            info!("client {:?} was removed", token);
          }
        }

        Some(SocketType::BackClient) => {
          if let Some(tok) = self.get_client_token(token) {
            if self.clients[tok].front_hup() == ClientResult::CloseClient {
              self.close_client(event_loop, tok);
            }
          }
        }

        None => {}
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
        error!("the listener socket should have no timeout set");
      },
      Some(SocketType::FrontClient) => {
        if self.clients.contains(token) {
          error!("frontend [{}] got timeout, closing", timeout);
          self.close_client(event_loop, token);
        }
      },
      Some(SocketType::BackClient) => {
        if let Some(tok) = self.get_client_token(token) {
          error!("backend [{}] got timeout, closing", timeout);
          self.close_client(event_loop, tok);
        }
      }
      None => {}
    }
  }

  fn interrupted(&mut self, event_loop: &mut EventLoop<Self>) {
    warn!("interrupted");
  }
}

