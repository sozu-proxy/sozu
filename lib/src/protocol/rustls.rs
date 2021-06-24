use mio::net::*;
use rusty_ulid::Ulid;
use std::io::ErrorKind;
use {SessionResult,Readiness};
use protocol::ProtocolResult;
use rustls::{ServerSession, Session};
use Ready;

pub enum TlsState {
  Initial,
  Handshake,
  Established,
  Error,
}

pub struct TlsHandshake {
  pub stream:    TcpStream,
  pub session:   ServerSession,
  pub readiness: Readiness,
  pub request_id: Ulid,
}

impl TlsHandshake {
  pub fn new(session: ServerSession, stream: TcpStream, request_id: Ulid) -> TlsHandshake {
    TlsHandshake {
      stream,
      session,
      readiness: Readiness {
        interest: Ready::readable() | Ready::hup() | Ready::error(),
        event: Ready::empty(),
      },
      request_id,
    }
  }

  pub fn readable(&mut self) -> (ProtocolResult,SessionResult) {
    let mut can_read  = true;

    loop {
      let mut can_work = false;

      if self.session.wants_read() && can_read {
        can_work = true;

        match self.session.read_tls(&mut self.stream) {
          Ok(0) => {
            error!("connection closed during handshake");
            return (ProtocolResult::Continue, SessionResult::CloseSession);
          },
          Ok(_) => {
          },
          Err(e) => match e.kind() {
            ErrorKind::WouldBlock => {
              self.readiness.event.remove(Ready::readable());
              can_read = false
            },
            _ => {
              error!("could not perform handshake: {:?}", e);
              return (ProtocolResult::Continue, SessionResult::CloseSession);
            }
          }
        }

        if let Err(e) = self.session.process_new_packets() {
          error!("could not perform handshake: {:?}", e);
          return (ProtocolResult::Continue, SessionResult::CloseSession);
        }
      }

      if !can_work {
        break;
      }
    }

    if !self.session.wants_read() {
      self.readiness.interest.remove(Ready::readable());
    }

    if self.session.wants_write() {
      self.readiness.interest.insert(Ready::writable());
    }

    if self.session.is_handshaking() {
      (ProtocolResult::Continue, SessionResult::Continue)
    } else {
      // handshake might be finished but we still have something to send
      if self.session.wants_write() {
        (ProtocolResult::Continue, SessionResult::Continue)
      } else {
        self.readiness.interest.insert(Ready::readable());
        self.readiness.event.insert(Ready::readable());
        self.readiness.interest.insert(Ready::writable());
        (ProtocolResult::Upgrade, SessionResult::Continue)
      }
    }
  }

  pub fn writable(&mut self) -> (ProtocolResult,SessionResult) {
    let mut can_write = true;

    loop {
      let mut can_work = false;

      if self.session.wants_write() && can_write {
        can_work = true;

        match self.session.write_tls(&mut self.stream) {
          Ok(_) => {},
          Err(e) => match e.kind() {
            ErrorKind::WouldBlock => {
              self.readiness.event.remove(Ready::writable());
              can_write = false
            },
            _ => {
              error!("could not perform handshake: {:?}", e);
              return (ProtocolResult::Continue, SessionResult::CloseSession);
            }
          }
        }

        if let Err(e) = self.session.process_new_packets() {
          error!("could not perform handshake: {:?}", e);
          return (ProtocolResult::Continue, SessionResult::CloseSession);
        }
      }

      if !can_work {
        break;
      }
    }

    if !self.session.wants_write() {
      self.readiness.interest.remove(Ready::writable());
    }

    if self.session.wants_read() {
      self.readiness.interest.insert(Ready::readable());
    }

    if self.session.is_handshaking() {
      (ProtocolResult::Continue, SessionResult::Continue)
    } else if self.session.wants_read() {
      self.readiness.interest.insert(Ready::readable());
      (ProtocolResult::Upgrade, SessionResult::Continue)
    } else {
      self.readiness.interest.insert(Ready::writable());
      self.readiness.interest.insert(Ready::readable());
      (ProtocolResult::Upgrade, SessionResult::Continue)
    }
  }
}

