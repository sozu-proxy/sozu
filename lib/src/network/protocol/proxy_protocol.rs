use std::net::IpAddr;
use mio::*;
use mio::tcp::TcpStream;
use mio::unix::UnixReady;
use network::{ClientResult, Protocol};
use network::socket::{SocketHandler,SocketResult};
use network::protocol::ProtocolResult;
use network::session::Readiness;
use network::header_proxy_protocol::ProxyProtocolHeader;

pub struct ProxyProtocol<Front:SocketHandler> {
  //header:       ProxyProtocolHeader,
  frontend:       Front,
  backend:        Option<TcpStream>,
  frontend_token: Option<Token>,
  backend_token:  Option<Token>,
  pub readiness:  Readiness,
}

impl <Front:SocketHandler>ProxyProtocol<Front> {
  pub fn new(frontend: Front, backend: Option<TcpStream>) -> Self {
    ProxyProtocol {
      //header,
      frontend,
      backend,
      frontend_token: None,
      backend_token:  None,
      readiness: Readiness {
        front_interest:  UnixReady::hup() | UnixReady::error(),
        back_interest:   UnixReady::from(Ready::writable()) | UnixReady::hup() | UnixReady::error(),
        front_readiness: UnixReady::from(Ready::empty()),
        back_readiness:  UnixReady::from(Ready::empty()),
      },
    }
  }

  // The header is send immediately at once upon the connection is establish
  // and prepended before any data.
  pub fn back_writable(&mut self) -> (ProtocolResult, ClientResult) {
    //TODO: writte the header in the backend
    debug!("Writte proxy protocol header");
    debug!("The header should be receive. We switch to a Pipe");
    (ProtocolResult::Upgrade, ClientResult::Continue)
  }

  pub fn front_socket(&self) -> &TcpStream {
    self.frontend.socket_ref()
  }

  pub fn back_socket(&self) -> Option<&TcpStream> {
    self.backend.as_ref()
  }

  pub fn set_back_socket(&mut self, socket: TcpStream) {
    self.backend = Some(socket);
  }

  pub fn front_token(&self) -> Option<Token> {
    self.frontend_token
  }

  pub fn set_front_token(&mut self, token: Token) {
    self.frontend_token = Some(token);
  }

  pub fn back_token(&self) -> Option<Token> {
    self.backend_token
  }

  pub fn set_back_token(&mut self, token: Token) {
    self.backend_token = Some(token);
  }

  pub fn readiness(&mut self) -> &mut Readiness {
    &mut self.readiness
  }

  pub fn protocol(&self) -> Protocol {
    Protocol::ProxyProtocol
  }
}