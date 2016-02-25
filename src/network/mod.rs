#![allow(dead_code, unused_must_use, unused_variables, unused_imports)]

pub mod buffer;
pub mod metrics;
pub mod amqp;
pub mod http;
pub mod tls;

#[cfg(feature = "splice")]
mod splice;

pub mod tcp;
pub mod proxy;

use mio::Token;
use messages::Command;


#[derive(Debug)]
pub enum ServerMessage {
  AddedFront,
  RemovedFront,
  AddedInstance,
  RemovedInstance,
  Stopped
}

#[derive(Debug)]
pub enum ProxyOrder {
  Command(Command),
  Stop
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
  BackClient
}

pub fn socket_type(token: Token, max_listeners: usize, max_connections: usize) -> Option<SocketType> {
  if token.as_usize() < max_listeners {
    Some(SocketType::Listener)
  } else if token.as_usize() < max_listeners + max_connections {
    Some(SocketType::FrontClient)
  } else if token.as_usize() < max_listeners + 2 * max_connections {
    Some(SocketType::BackClient)
  } else {
    None
  }
}
