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
use slab::{Slab,Entry,VacantEntry};
use std::io::Write;
use std::str::FromStr;
use std::marker::PhantomData;
use std::fmt::Debug;
use std::rc::Rc;
use std::cell::RefCell;
use time::{precise_time_ns,SteadyTime,Duration};
use rand::random;

use sozu_command::channel::Channel;
use sozu_command::messages::{self,TcpFront,Order,Instance,MessageId,OrderMessage,
  OrderMessageAnswer,OrderMessageStatus};

use network::{ClientResult,ConnectionError,SocketType,Protocol,RequiredEvents,
  ProxyClient,ProxyConfiguration,AcceptError,BackendConnectAction,
  CloseResult};

const SERVER: Token = Token(0);
const DEFAULT_FRONT_TIMEOUT: u64 = 50000;
const DEFAULT_BACK_TIMEOUT:  u64 = 50000;

pub type ProxyChannel = Channel<OrderMessageAnswer,OrderMessage>;

#[derive(Copy,Clone,Debug,PartialEq,Eq,PartialOrd,Ord,Hash)]
pub struct ListenToken(pub usize);
#[derive(Copy,Clone,Debug,PartialEq,Eq,PartialOrd,Ord,Hash)]
pub struct ClientToken(pub usize);

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

impl From<usize> for ClientToken {
    fn from(val: usize) -> ClientToken {
        ClientToken(val)
    }
}

impl From<ClientToken> for usize {
    fn from(val: ClientToken) -> usize {
        val.0
    }
}

pub struct Session<ServerConfiguration,Client> {
  pub configuration:   ServerConfiguration,
  clients:         Slab<Rc<RefCell<Client>>,ClientToken>,
  max_listeners:   usize,
  max_connections: usize,
  shutting_down:   Option<MessageId>,
  accept_ready:    HashSet<ListenToken>,
  can_accept:      bool,
  base_token:      usize,
  phantom:         PhantomData<Client>,
}

impl<ServerConfiguration:ProxyConfiguration<Client>,Client:ProxyClient> Session<ServerConfiguration,Client> {
  pub fn new(max_listeners: usize, max_connections: usize, base_token: usize, configuration: ServerConfiguration,
    accept_ready: HashSet<ListenToken>, poll: &mut Poll) -> Self {

    let clients = Slab::with_capacity(max_connections);
    Session {
      configuration:   configuration,
      clients:         clients,
      max_listeners:   max_listeners,
      max_connections: max_connections,
      shutting_down:   None,
      accept_ready:    accept_ready,
      can_accept:      true,
      base_token:      base_token,
      phantom:         PhantomData,
    }
  }

  pub fn to_client(&self, token: Token) -> ClientToken {
    ClientToken(token.0 - 2 - self.max_listeners - self.base_token)
  }

  pub fn from_client(&self, token: ClientToken) -> Token {
    Token(token.0 + 2 + self.max_listeners + self.base_token)
  }

  pub fn from_client_add(&self) -> usize {
    2 + self.max_listeners + self.base_token
  }

  pub fn configuration(&mut self) -> &mut ServerConfiguration {
    &mut self.configuration
  }

  pub fn close_client(&mut self, poll: &mut Poll, token: ClientToken) {
    if self.clients.contains(token) {
      let client = self.clients.remove(token).expect("client shoud be there");
      let CloseResult { tokens, backends } = client.borrow_mut().close(poll);

      for tk in tokens.into_iter() {
        let cl = self.to_client(tk);
        self.clients.remove(cl);
      }

      for (app_id, address) in backends.into_iter() {
        self.configuration.close_backend(app_id, &address);
      }

      decr!("client.connections");
    }

    self.can_accept = true;
  }

  pub fn accept(&mut self, poll: &mut Poll, token: ListenToken) -> bool {
    let add = self.from_client_add();
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

  pub fn connect_to_backend(&mut self, poll: &mut Poll, token: ClientToken) {
    let add = self.from_client_add();
    let res = {
      let cl = self.clients[token].clone();
      let cl2 = self.clients[token].clone();
      let entry = self.clients.vacant_entry().expect("FIXME");
      let entry = entry.insert(cl);
      let back_token = Token(entry.index().0 + add);
      self.configuration.connect_to_backend(poll, cl2, entry, back_token)
    };

    match res {
      Ok(BackendConnectAction::Reuse) => {
        debug!("keepalive, reusing backend connection");
      }
      Ok(BackendConnectAction::Replace) => {
      },
      Ok(BackendConnectAction::New) => {
      },
      Err(ConnectionError::HostNotFound) | Err(ConnectionError::NoBackendAvailable) | Err(ConnectionError::HttpsRedirect) => {
      },
      _ => self.close_client(poll, token),
    }
  }

  pub fn get_client_token(&self, token: Token) -> Option<ClientToken> {
    if token.0 < 2 + self.max_listeners + self.base_token {
      None
    } else if self.clients.contains(self.to_client(token)) {
      Some(self.to_client(token))
    } else {
      None
    }
  }

  pub fn interpret_client_order(&mut self, poll: &mut Poll, token: ClientToken, order: ClientResult) {
    //println!("INTERPRET ORDER: {:?}", order);
    match order {
      ClientResult::CloseClient     => self.close_client(poll, token),
      //FIXME: we do not deregister in close_backend
      ClientResult::CloseBackend(opt) => {
        if let Some(token) = opt {
          let cl = self.to_client(token);
          if let Some(client) = self.clients.remove(cl) {
            let res = client.borrow_mut().close_backend(token, poll);
            if let Some((app_id, address)) = res {
              self.configuration.close_backend(app_id, &address);
            }
          }
        }
      },
      ClientResult::ConnectBackend  => self.connect_to_backend(poll, token),
      ClientResult::Continue        => {}
    }
  }
}

//type Timeout = usize;

impl<ServerConfiguration:ProxyConfiguration<Client>,Client:ProxyClient> Session<ServerConfiguration,Client> {

  pub fn ready(&mut self, poll: &mut Poll, token: Token, events: Ready) {
    //info!("PROXY\t{:?} got events: {:?}", token, events);

    let client_token:ClientToken = match socket_type(token, self.base_token, self.max_listeners, self.max_connections) {
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
        let tok = self.to_client(token);
        if self.clients.contains(tok) {
          self.clients[tok].borrow_mut().process_events(token, events);
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
      let order = self.clients[client_token].borrow_mut().ready();
      //info!("client[{:?} -> {:?}] got events {:?} and returned order {:?}", client_token, self.from_client(client_token), events, order);
      //FIXME: the CloseBackend message might not mean we have nothing else to do
      //with that client
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
  } else if token.0 < 2 + max_listeners + 2 * max_connections + base_token {
    Some(SocketType::FrontClient)
  } else {
    None
  }
}

