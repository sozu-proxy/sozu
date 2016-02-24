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


#[derive(Debug)]
pub enum ServerMessage {
  AddedFront,
  RemovedFront,
  AddedInstance,
  RemovedInstance,
  Stopped
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
  NoHostNorPathGiven,
  HostNotFound,
  NoBackendAvailable,
  ToBeDefined
}
