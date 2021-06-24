use mio::net::*;
use rusty_ulid::Ulid;
use {SessionResult,Readiness};
use protocol::ProtocolResult;
use openssl::ssl::{HandshakeError,MidHandshakeSslStream,Ssl,SslStream,NameType,SslVersion};
use std::net::SocketAddr;
use LogDuration;
use SessionMetrics;
use sozu_command::ready::Ready;

pub enum TlsState {
  Initial,
  Handshake,
  Established,
  Error(HandshakeError<TcpStream>),
}

pub struct TlsHandshake {
  pub readiness:       Readiness,
  pub front:           Option<TcpStream>,
  pub request_id:      Ulid,
  pub ssl:             Option<Ssl>,
  pub stream:          Option<SslStream<TcpStream>>,
  mid:                 Option<MidHandshakeSslStream<TcpStream>>,
  state:               TlsState,
  address:             Option<SocketAddr>,
}

impl TlsHandshake {
  pub fn new(ssl:Ssl, sock: TcpStream, request_id: Ulid, address: Option<SocketAddr>) -> TlsHandshake {
    TlsHandshake {
      front:          Some(sock),
      ssl:            Some(ssl),
      mid:            None,
      stream:         None,
      state:          TlsState::Initial,
      readiness:      Readiness {
                        interest: Ready::readable() | Ready::hup() | Ready::error(),
                        event:    Ready::empty(),
      },
      request_id,
      address,
    }
  }

  pub fn readable(&mut self, metrics: &mut SessionMetrics) -> (ProtocolResult,SessionResult) {
    match self.state {
      TlsState::Error(_) => return (ProtocolResult::Continue, SessionResult::CloseSession),
      TlsState::Initial => {
        let ssl     = self.ssl.take().expect("TlsHandshake should have a Ssl backend");
        let sock    = self.front.take().expect("TlsHandshake should have a front socket");
        match ssl.accept(sock) {
          Ok(stream) => {
            self.stream = Some(stream);
            self.state = TlsState::Established;
            return (ProtocolResult::Upgrade, SessionResult::Continue);
          },
          Err(HandshakeError::SetupFailure(e)) => {
            error!("accept: handshake setup failed (client = {:?}): {:?}", self.address, e);
            self.state = TlsState::Error(HandshakeError::SetupFailure(e));
            return (ProtocolResult::Continue, SessionResult::CloseSession);
          },
          Err(HandshakeError::Failure(e)) => {
            {
              if let Some(error_stack) = e.error().ssl_error() {
                let errors = error_stack.errors();
                if errors.len() == 1 {
                  if errors[0].code() == 0x140A1175 {
                    incr!("openssl.inappropriate_fallback.error");
                  } else if errors[0].code() == 0x1408A10B {
                    incr!("openssl.wrong_version_number.error");
                  } else if errors[0].code() == 0x140760FC {
                    incr!("openssl.unknown_protocol.error");
                  } else if errors[0].code() == 0x1407609C {
                    //someone tried to connect in plain HTTP to a TLS server
                    incr!("openssl.http_request.error");
                  } else if errors[0].code() == 0x1422E0EA {
                    // SNI error
                    self.log_request_error(metrics, &e);
                  } else {
                    error!("accept: handshake failed (client = {:?}): {} ({:?})", self.address, e.error(), e);
                  }
                } else if errors.len() == 2 && errors[0].code() == 0x1412E0E2 && errors[1].code() == 0x1408A0E3 {
                  incr!("openssl.sni.error");
                } else {
                  error!("accept: handshake failed (client = {:?}): {} ({:?})", self.address, e.error(), e);
                }
              } else {
                error!("accept: handshake failed (client = {:?}): {} ({:?})", self.address, e.error(), e);
              }
            }
            self.state = TlsState::Error(HandshakeError::Failure(e));
            return (ProtocolResult::Continue, SessionResult::CloseSession);
          },
          Err(HandshakeError::WouldBlock(mid)) => {
            self.state = TlsState::Handshake;
            self.mid = Some(mid);
            self.readiness.event.remove(Ready::readable());
            return (ProtocolResult::Continue, SessionResult::Continue);
          }
        }
      },
      TlsState::Handshake => {
        let mid = self.mid.take().expect("TlsHandshake should have a MidHandshakeSslStream backend");
        match mid.handshake() {
          Ok(stream) => {
            self.stream = Some(stream);
            self.state = TlsState::Established;
            return (ProtocolResult::Upgrade, SessionResult::Continue);
          },
          Err(HandshakeError::SetupFailure(e)) => {
            debug!("mid handshake setup failed (client = {:?}): {:?}", self.address, e);
            self.state = TlsState::Error(HandshakeError::SetupFailure(e));
            return (ProtocolResult::Continue, SessionResult::CloseSession);
          },
          Err(HandshakeError::Failure(e)) => {
            debug!("mid handshake failed (client = {:?}): {:?}", self.address, e);
            self.state = TlsState::Error(HandshakeError::Failure(e));
            return (ProtocolResult::Continue, SessionResult::CloseSession);
          },
          Err(HandshakeError::WouldBlock(new_mid)) => {
            self.state = TlsState::Handshake;
            self.mid = Some(new_mid);
            self.readiness.event.remove(Ready::readable());
            return (ProtocolResult::Continue, SessionResult::Continue);
          }
        }
      },
      TlsState::Established => {
        return (ProtocolResult::Upgrade, SessionResult::Continue);
      }
    }

  }

  pub fn socket(&self) -> Option<&TcpStream> {
    match self.state {
      TlsState::Initial => self.front.as_ref(),
      TlsState::Handshake => self.mid.as_ref().map(|mid| mid.get_ref()),
      TlsState::Established => self.stream.as_ref().map(|stream| stream.get_ref()),
      TlsState::Error(ref error) => {
        match error {
          &HandshakeError::SetupFailure(_) => {
            self.front.as_ref().or_else(|| self.mid.as_ref().map(|mid| mid.get_ref()))
          },
          &HandshakeError::Failure(ref mid) | &HandshakeError::WouldBlock(ref mid) => Some(mid.get_ref())
        }
      }
    }
  }

  pub fn socket_mut(&mut self) -> Option<&mut TcpStream> {
    match self.state {
      TlsState::Initial => self.front.as_mut(),
      TlsState::Handshake => self.mid.as_mut().map(|mid| mid.get_mut()),
      TlsState::Established => self.stream.as_mut().map(|stream| stream.get_mut()),
      TlsState::Error(HandshakeError::Failure(ref mut mid)) |
      TlsState::Error(HandshakeError::WouldBlock(ref mut mid)) => {
          Some(mid.get_mut())
      },
      TlsState::Error(HandshakeError::SetupFailure(_)) => {
          self.front.as_mut().or(self.mid.as_mut().map(|mid| mid.get_mut()))
      }
    }
  }

  pub fn log_request_error(&mut self, metrics: &mut SessionMetrics, handshake: &MidHandshakeSslStream<TcpStream>) {
    metrics.service_stop();

    let session = match self.address.or_else(|| handshake.get_ref().peer_addr().ok()) {
      None => String::from("-"),
      Some(SocketAddr::V4(addr)) => format!("{}", addr),
      Some(SocketAddr::V6(addr)) => format!("{}", addr),
    };

    let backend = "-";

    let response_time = metrics.response_time();
    let service_time  = metrics.service_time();

    let proto = handshake.ssl().version2().map(|version| match version {
      SslVersion::SSL3 => "HTTPS-SSL3",
      SslVersion::TLS1 => "HTTPS-TLS1.0",
      SslVersion::TLS1_1 => "HTTPS-TLS1.1",
      SslVersion::TLS1_2 => "HTTPS-TLS1.2",
      _ => "HTTPS-TLS1.3",
    }).unwrap_or("HTTPS");

    let message = handshake.ssl().servername(NameType::HOST_NAME)
      .map(|s| format!("unknown server name: {}", s))
      .unwrap_or("no SNI".to_string());

    error_access!("{} - -\t{} -> {}\t{} {} {} {}\t{} | {}",
      self.request_id, session, backend,
      LogDuration(response_time), LogDuration(service_time), metrics.bin, metrics.bout,
      proto, message);
  }
}

