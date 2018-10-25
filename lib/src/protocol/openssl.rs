use mio::*;
use mio::net::*;
use mio::unix::UnixReady;
use uuid::adapter::Hyphenated;
use {SessionResult,Readiness};
use protocol::ProtocolResult;
use openssl::ssl::{HandshakeError,MidHandshakeSslStream,Ssl,SslStream};

pub enum TlsState {
  Initial,
  Handshake,
  Established,
  Error(HandshakeError<TcpStream>),
}

pub struct TlsHandshake {
  pub readiness:       Readiness,
  pub front:           Option<TcpStream>,
  pub request_id:      Hyphenated,
  pub ssl:             Option<Ssl>,
  pub stream:          Option<SslStream<TcpStream>>,
  mid:                 Option<MidHandshakeSslStream<TcpStream>>,
  state:               TlsState,
}

impl TlsHandshake {
  pub fn new(ssl:Ssl, sock: TcpStream, request_id: Hyphenated) -> TlsHandshake {
    TlsHandshake {
      front:          Some(sock),
      ssl:            Some(ssl),
      mid:            None,
      stream:         None,
      state:          TlsState::Initial,
      readiness:      Readiness {
                        interest: UnixReady::from(Ready::readable()) | UnixReady::hup() | UnixReady::error(),
                        event:    UnixReady::from(Ready::empty()),
      },
      request_id,
    }
  }

  pub fn readable(&mut self) -> (ProtocolResult,SessionResult) {
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
            error!("accept: handshake setup failed: {:?}", e);
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
                  } else {
                    error!("accept: handshake failed: {:?}", e);
                  }
                } else if errors.len() == 2 && errors[0].code() == 0x1412E0E2 && errors[1].code() == 0x1408A0E3 {
                  incr!("openssl.sni.error");
                } else {
                  error!("accept: handshake failed: {:?}", e);
                }
              } else {
                error!("accept: handshake failed: {:?}", e);
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
            debug!("mid handshake setup failed: {:?}", e);
            self.state = TlsState::Error(HandshakeError::SetupFailure(e));
            return (ProtocolResult::Continue, SessionResult::CloseSession);
          },
          Err(HandshakeError::Failure(e)) => {
            debug!("mid handshake failed: {:?}", e);
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
}

