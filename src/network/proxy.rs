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
use std::net::SocketAddr;
use std::str::FromStr;
use std::marker::PhantomData;
use std::fmt::Debug;
use time::precise_time_s;
use rand::random;
use network::{ClientResult,ServerMessage};

use messages::{TcpFront,Command,Instance};

const SERVER: Token = Token(0);
type ClientToken = Token;

pub trait ProxyClient<Server:Handler> {
  fn front_socket(&self) -> &TcpStream;
  fn back_socket(&self)  -> Option<&TcpStream>;
  fn front_token(&self)  -> Option<Token>;
  fn back_token(&self)   -> Option<Token>;
  fn set_back_socket(&mut self, TcpStream);
  fn set_front_token(&mut self, token: Token);
  fn set_back_token(&mut self, token: Token);
  fn set_tokens(&mut self, token: Token, backend: Token);
  fn front_hup(&mut self) -> ClientResult;
  fn back_hup(&mut self) -> ClientResult;
  fn readable(&mut self, event_loop: &mut EventLoop<Server>) -> ClientResult;
  fn writable(&mut self, event_loop: &mut EventLoop<Server>) -> ClientResult;
  fn back_readable(&mut self, event_loop: &mut EventLoop<Server>) -> ClientResult;
  fn back_writable(&mut self, event_loop: &mut EventLoop<Server>) -> ClientResult;
  fn remove_backend(&mut self);
}

pub trait ProxyConfiguration<Server:Handler,Client,Message> {
  fn add_tcp_front(&mut self, port: u16, app_id: &str, event_loop: &mut EventLoop<Server>) -> Option<Token>;
  fn connect_to_backend(&mut self, client:&mut Client) ->Option<TcpStream>;
  fn notify(&mut self, event_loop: &mut EventLoop<Server>, message: Message);
  fn accept(&mut self, token: Token) -> Option<(Client, bool)>;
}

pub struct Server<ServerConfiguration,Client,Message> {
  configuration:   ServerConfiguration,
  clients:         Slab<Client>,
  backend:         Slab<ClientToken>,
  max_listeners:   usize,
  max_connections: usize,
  phantom: PhantomData<Message>
}

impl<ServerConfiguration:ProxyConfiguration<Server<ServerConfiguration,Client,Message>, Client,Message>,Client:ProxyClient<Server<ServerConfiguration,Client,Message>>,Message:Send+Debug> Server<ServerConfiguration,Client,Message> {
  pub fn new(max_listeners: usize, max_connections: usize, configuration: ServerConfiguration) -> Self {
    Server {
      configuration:   configuration,
      clients:         Slab::new_starting_at(Token(max_listeners), max_connections),
      backend:         Slab::new_starting_at(Token(max_listeners+max_connections), max_connections),
      max_listeners:   max_listeners,
      max_connections: max_connections,
      phantom:         PhantomData
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
    if let Some(socket) = self.configuration.connect_to_backend(&mut self.clients[token]) {
      if let Ok(backend_token) = self.backend.insert(token) {
        self.clients[token].set_back_socket(socket);
        self.clients[token].set_back_token(backend_token);

        if let Some(sock) = self.clients[token].back_socket() {
          event_loop.register(sock, backend_token, EventSet::writable(), PollOpt::edge());
        }
        return;
      }
    }
    self.close_client(event_loop, token);
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
    match order {
      ClientResult::CloseClient      => self.close_client(event_loop, token),
      ClientResult::CloseBackend     => self.close_backend(event_loop, token),
      ClientResult::CloseBothSuccess => self.close_client(event_loop, token),
      ClientResult::CloseBothFailure => self.close_client(event_loop, token),
      ClientResult::ConnectBackend   => self.connect_to_backend(event_loop, token),
      ClientResult::Continue         => {}
    }
  }
}

impl<ServerConfiguration:ProxyConfiguration<Server<ServerConfiguration,Client,Msg>, Client,Msg>,Client:ProxyClient<Server<ServerConfiguration,Client,Msg>>,Msg:Send+Debug> Handler for Server<ServerConfiguration,Client,Msg> {
  type Timeout = usize;
  type Message = Msg;

  fn ready(&mut self, event_loop: &mut EventLoop<Self>, token: Token, events: EventSet) {
    trace!("{:?} got events: {:?}", token, events);
    if events.is_readable() {
      trace!("{:?} is readable", token);
      if token.as_usize() < self.max_listeners {
        self.accept(event_loop, token)
      } else if token.as_usize() < self.max_listeners + self.max_connections {
        if self.clients.contains(token) {
          let order = self.clients[token].readable(event_loop);
          self.interpret_client_order(event_loop, token, order);
        } else {
          info!("client {:?} was removed", token);
        }
      } else if token.as_usize() < self.max_listeners + 2 * self.max_connections {
        if let Some(tok) = self.get_client_token(token) {
          let order = self.clients[tok].back_readable(event_loop);
          self.interpret_client_order(event_loop, tok, order);
        }
      }
    }

    if events.is_writable() {
      trace!("{:?} is writable", token);
      if token.as_usize() < self.max_listeners {
        error!("received writable for listener {:?}, this should not happen", token);
      } else  if token.as_usize() < self.max_listeners + self.max_connections {
        if self.clients.contains(token) {
          let order = self.clients[token].writable(event_loop);
          self.interpret_client_order(event_loop, token, order);
        } else {
          info!("client {:?} was removed", token);
        }
      } else if token.as_usize() < self.max_listeners + 2 * self.max_connections {
        if let Some(tok) = self.get_client_token(token) {
          let order = self.clients[tok].back_writable(event_loop);
          self.interpret_client_order(event_loop, tok, order);
        }
      }
    }

    if events.is_hup() {
      if token.as_usize() < self.max_listeners {
        error!("should not happen: server {:?} closed", token);
      } else if token.as_usize() < self.max_listeners + self.max_connections {
        if self.clients.contains(token) {
          if self.clients[token].front_hup() == ClientResult::CloseClient {
            self.close_client(event_loop, token);
          }
        } else {
          info!("client {:?} was removed", token);
        }
      } else if token.as_usize() < self.max_listeners + 2 * self.max_connections {
        if let Some(tok) = self.get_client_token(token) {
          if self.clients[tok].front_hup() == ClientResult::CloseClient {
            self.close_client(event_loop, tok);
          }
        }
      }
    }
  }

  fn notify(&mut self, event_loop: &mut EventLoop<Self>, message: Self::Message) {
    self.configuration.notify(event_loop, message);
  }

  fn timeout(&mut self, event_loop: &mut EventLoop<Self>, timeout: Self::Timeout) {
    warn!("timeout");
  }

  fn interrupted(&mut self, event_loop: &mut EventLoop<Self>) {
    warn!("interrupted");
  }
}

