use mio::*;
use mio::net::*;
use mio::unix::UnixReady;
use pool::Checkout;
use network::buffer_queue::BufferQueue;
use openssl::ssl::{self,HandshakeError,MidHandshakeSslStream,Ssl,SslStream};
use network::{ClientResult,Protocol};
use network::session::Readiness;
use network::protocol::ProtocolResult;

pub enum TlsState {
  Initial,
  Handshake,
  Established,
  Error,
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
      TlsState::Error   => return (ProtocolResult::Continue, ClientResult::CloseClient),
      TlsState::Initial => {
        let ssl     = self.ssl.take().expect("TlsHandshake should have a Ssl instance");
        let sock    = self.front.take().expect("TlsHandshake should have a front socket");
        let version = ssl.version();
        match ssl.accept(sock) {
          Ok(stream) => {
            self.stream = Some(stream);
            return (ProtocolResult::Upgrade, ClientResult::Continue);
          },
          Err(HandshakeError::SetupFailure(e)) => {
            error!("accept: handshake setup failed: {:?}", e);
            self.state = TlsState::Error;
            return (ProtocolResult::Continue, ClientResult::CloseClient);
          },
          Err(HandshakeError::Failure(e)) => {
            error!("accept: handshake failed: {:?}", e);
            self.state = TlsState::Error;
            return (ProtocolResult::Continue, ClientResult::CloseClient);
          },
          Err(HandshakeError::Interrupted(mid)) => {
            self.state = TlsState::Handshake;
            self.mid = Some(mid);
            self.readiness.front_readiness.remove(Ready::readable());
            return (ProtocolResult::Continue, ClientResult::Continue);
          }
        }
      },
      TlsState::Handshake => {
        let mid = self.mid.take().expect("TlsHandshake should have a MidHandshakeSslStream instance");
        let version = mid.ssl().version();
        match mid.handshake() {
          Ok(stream) => {
            self.stream = Some(stream);
            return (ProtocolResult::Upgrade, ClientResult::Continue);
          },
          Err(HandshakeError::SetupFailure(e)) => {
            debug!("mid handshake setup failed: {:?}", e);
            self.state = TlsState::Error;
            return (ProtocolResult::Continue, ClientResult::CloseClient);
          },
          Err(HandshakeError::Failure(e)) => {
            debug!("mid handshake failed: {:?}", e);
            self.state = TlsState::Error;
            return (ProtocolResult::Continue, ClientResult::CloseClient);
          },
          Err(HandshakeError::Interrupted(new_mid)) => {
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

  fn protocol(&self)           -> Protocol {
    Protocol::HTTPS
  }
}

