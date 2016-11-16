#![allow(dead_code, unused_must_use, unused_variables, unused_imports)]

use std::net::SocketAddr;
use std::fmt;

pub mod buffer;
pub mod buffer_queue;
#[macro_use] pub mod metrics;
pub mod socket;
pub mod trie;
pub mod http;
pub mod tls;

#[cfg(feature = "splice")]
mod splice;

pub mod tcp;
pub mod proxy;

use mio::Token;
use messages::Command;

pub type MessageId = String;

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct ServerMessage {
  pub id:      MessageId,
  pub message: ServerMessageType,
}

impl fmt::Display for ServerMessage {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    write!(f, "{}-{:?}", self.id, self.message)
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub enum ServerMessageType {
  AddedFront,
  RemovedFront,
  AddedInstance,
  RemovedInstance,
  Stopped,
  Error(String),
}

#[derive(Debug)]
pub enum ProxyOrder {
  Command(MessageId,Command),
  Stop(MessageId)
}

impl fmt::Display for ProxyOrder {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    match *self {
      ProxyOrder::Command(ref id, ref msg) => write!(f, "{}-{:?}", id, msg),
      ProxyOrder::Stop(ref id)             => write!(f, "{}-Stop", id)
    }
  }
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
  CloseBackend,
  CloseBothSuccess,
  CloseBothFailure,
  Continue,
  ConnectBackend
}

#[derive(Debug,PartialEq,Eq)]
pub enum ConnectionError {
  NoHostGiven,
  NoRequestLineGiven,
  HostNotFound,
  NoBackendAvailable,
  ToBeDefined
}

#[derive(Debug,PartialEq,Eq)]
pub enum SocketType {
  Listener,
  FrontClient,
  BackClient,
}

pub fn socket_type(token: Token, max_listeners: usize, max_connections: usize) -> Option<SocketType> {
  if token.0 < 2 + max_listeners {
    Some(SocketType::Listener)
  } else if token.0 < 2 + max_listeners + max_connections {
    Some(SocketType::FrontClient)
  } else if token.0 < 2 + max_listeners + 2 * max_connections {
    Some(SocketType::BackClient)
  } else {
    None
  }
}

#[derive(PartialEq,Eq)]
pub enum BackendStatus {
  Normal,
  Closing,
  Closed,
}

pub struct Backend {
  pub address:            SocketAddr,
  pub status:             BackendStatus,
  pub active_connections: usize,
}

impl Backend {
  pub fn new(addr: SocketAddr) -> Backend {
    Backend {
      address:            addr,
      status:             BackendStatus::Normal,
      active_connections: 0,
    }
  }

  pub fn set_closing(&mut self) {
    self.status = BackendStatus::Closing;
  }

  pub fn can_open(&self) -> bool {
    self.status == BackendStatus::Normal
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
}

