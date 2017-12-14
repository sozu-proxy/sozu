#![allow(dead_code, unused_must_use, unused_variables, unused_imports)]

use mio;
use std::fmt;
use std::net::SocketAddr;

pub mod buffer_queue;
#[macro_use] pub mod metrics;
pub mod socket;
pub mod trie;
pub mod protocol;
pub mod http;
pub mod backends;
pub mod retry;

#[cfg(feature = "splice")]
mod splice;

pub mod tcp;
pub mod proxy;
pub mod session;

#[cfg(feature = "use_openssl")]
pub mod https_openssl;

#[cfg(feature = "use_openssl")]
pub mod https {
  pub type ServerConfiguration = https_openssl::ServerConfiguration;
  pub type TlsClient           = https_openssl::TlsClient;
}

#[cfg(not(feature = "use_openssl"))]
pub mod https_rustls;

#[cfg(not(feature = "use_openssl"))]
pub mod https {
  pub type ServerConfiguration = super::https_rustls::configuration::ServerConfiguration;
  pub type TlsClient           = super::https_rustls::client::TlsClient;
}

use mio::Token;

use self::retry::RetryPolicy;

pub type AppId = String;

#[derive(Debug,Clone,Copy)]
pub enum Protocol {
  HTTP,
  HTTPS,
  TCP
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
  CloseBoth,
  Continue,
  ConnectBackend
}

#[derive(Debug,PartialEq,Eq)]
pub enum ConnectionError {
  NoHostGiven,
  NoRequestLineGiven,
  HostNotFound,
  NoBackendAvailable,
  ToBeDefined,
  HttpsRedirect
}

#[derive(Debug,PartialEq,Eq)]
pub enum SocketType {
  Listener,
  FrontClient,
  BackClient,
}

#[derive(Debug,PartialEq,Eq)]
pub enum BackendStatus {
  Normal,
  Closing,
  Closed,
}

#[derive(Debug,PartialEq,Eq)]
pub struct Backend {
  pub id:                 u32,
  pub instance_id:        String,
  pub address:            SocketAddr,
  pub status:             BackendStatus,
  pub retry_policy:       retry::RetryPolicyWrapper,
  pub active_connections: usize,
  pub failures:           usize,
}

impl Backend {
  pub fn new(instance_id: &str, addr: SocketAddr, id: u32) -> Backend {
    let desired_policy = retry::ExponentialBackoffPolicy::new(10);
    Backend {
      id:                 id,
      instance_id:        instance_id.to_string(),
      address:            addr,
      status:             BackendStatus::Normal,
      retry_policy:       desired_policy.into(),
      active_connections: 0,
      failures:           0,
    }
  }

  pub fn set_closing(&mut self) {
    self.status = BackendStatus::Closing;
  }

  pub fn retry_policy(&mut self) -> &mut retry::RetryPolicyWrapper {
    &mut self.retry_policy
  }

  pub fn can_open(&self) -> bool {
    if let Some(action) = self.retry_policy.can_try() {
      self.status == BackendStatus::Normal && action == retry::RetryAction::OKAY
    } else {
      false
    }
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

  pub fn try_connect(&mut self) -> Result<mio::tcp::TcpStream, ConnectionError> {
    if self.status != BackendStatus::Normal {
      return Err(ConnectionError::NoBackendAvailable);
    }

    //FIXME: what happens if the connect() call fails with EINPROGRESS?
    let conn = mio::tcp::TcpStream::connect(&self.address).map_err(|_| ConnectionError::NoBackendAvailable);
    if conn.is_ok() {
      //self.retry_policy.succeed();
      self.inc_connections();
    } else {
      self.retry_policy.fail();
      self.failures += 1;
    }

    conn
  }
}

