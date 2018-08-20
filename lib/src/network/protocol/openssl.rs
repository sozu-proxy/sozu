use mio::*;
use mio::net::*;
use mio::unix::UnixReady;
use network::buffer_queue::BufferQueue;
use network::pool::Checkout;
use network::{ClientResult,Readiness};
use network::protocol::ProtocolResult;
use openssl::ssl::{self,HandshakeError,MidHandshakeSslStream,Ssl,SslStream};

pub enum TlsState {
  Initial,
  Handshake,
  Established,
  Error(HandshakeError<TcpStream>),
}

pub struct TlsHandshake {
  pub readiness:       Readiness,
  pub front:           Option<TcpStream>,
  pub ssl:             Option<Ssl>,
  pub stream:          Option<SslStream<TcpStream>>,
  mid:                 Option<MidHandshakeSslStream<TcpStream>>,
  state:               TlsState,
}

impl TlsHandshake {
  pub fn new(ssl:Ssl, sock: TcpStream) -> TlsHandshake {
    TlsHandshake {
      front:          Some(sock),
      ssl:            Some(ssl),
      mid:            None,
      stream:         None,
      state:          TlsState::Initial,
      readiness:      Readiness {
                        front_interest:  UnixReady::from(Ready::readable()) | UnixReady::hup() | UnixReady::error(),
                        back_interest:   UnixReady::from(Ready::empty()),
                        front_readiness: UnixReady::from(Ready::empty()),
                        back_readiness:  UnixReady::from(Ready::empty()),
      },
    }
  }

  pub fn readable(&mut self) -> (ProtocolResult,ClientResult) {
    match self.state {
      TlsState::Error(_) => return (ProtocolResult::Continue, ClientResult::CloseClient),
      TlsState::Initial => {
        let ssl     = self.ssl.take().expect("TlsHandshake should have a Ssl backend");
        let sock    = self.front.take().expect("TlsHandshake should have a front socket");
        match ssl.accept(sock) {
          Ok(stream) => {
            self.stream = Some(stream);
            self.state = TlsState::Established;
            return (ProtocolResult::Upgrade, ClientResult::Continue);
          },
          Err(HandshakeError::SetupFailure(e)) => {
            error!("accept: handshake setup failed: {:?}", e);
            self.state = TlsState::Error(HandshakeError::SetupFailure(e));
            return (ProtocolResult::Continue, ClientResult::CloseClient);
          },
          Err(HandshakeError::Failure(e)) => {
            {
              if let Some(error_stack) = e.error().ssl_error() {
                let errors = error_stack.errors();
                if errors.len() == 2 && errors[0].code() == 0x1412E0E2 && errors[1].code() == 0x1408A0E3 {
                  incr!("openssl_sni_error");
                } else {
                  error!("accept: handshake failed: {:?}", e);
                }
              } else {
                error!("accept: handshake failed: {:?}", e);
              }
            }
            self.state = TlsState::Error(HandshakeError::Failure(e));
            return (ProtocolResult::Continue, ClientResult::CloseClient);
          },
          Err(HandshakeError::WouldBlock(mid)) => {
            self.state = TlsState::Handshake;
            self.mid = Some(mid);
            self.readiness.front_readiness.remove(Ready::readable());
            return (ProtocolResult::Continue, ClientResult::Continue);
          }
        }
      },
      TlsState::Handshake => {
        let mid = self.mid.take().expect("TlsHandshake should have a MidHandshakeSslStream backend");
        match mid.handshake() {
          Ok(stream) => {
            self.stream = Some(stream);
            self.state = TlsState::Established;
            return (ProtocolResult::Upgrade, ClientResult::Continue);
          },
          Err(HandshakeError::SetupFailure(e)) => {
            debug!("mid handshake setup failed: {:?}", e);
            self.state = TlsState::Error(HandshakeError::SetupFailure(e));
            return (ProtocolResult::Continue, ClientResult::CloseClient);
          },
          Err(HandshakeError::Failure(e)) => {
            debug!("mid handshake failed: {:?}", e);
            self.state = TlsState::Error(HandshakeError::Failure(e));
            return (ProtocolResult::Continue, ClientResult::CloseClient);
          },
          Err(HandshakeError::WouldBlock(new_mid)) => {
            self.state = TlsState::Handshake;
            self.mid = Some(new_mid);
            self.readiness.front_readiness.remove(Ready::readable());
            return (ProtocolResult::Continue, ClientResult::Continue);
          }
        }
      },
      TlsState::Established => {
        return (ProtocolResult::Upgrade, ClientResult::Continue);
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

