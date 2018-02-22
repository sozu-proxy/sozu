use std::net::IpAddr;
use std::io::{Write, ErrorKind};
use mio::*;
use mio::tcp::TcpStream;
use mio::unix::UnixReady;
use network::{ClientResult, Protocol};
use network::socket::{SocketHandler,SocketResult};
use network::protocol::ProtocolResult;
use network::session::Readiness;
use network::header_proxy_protocol::*;

pub struct ProxyProtocol<Front:SocketHandler> {
  pub header:     Option<ProxyProtocolHeader>,
  pub frontend:   Front,
  pub backend:    Option<TcpStream>,
  frontend_token: Option<Token>,
  backend_token:  Option<Token>,
  pub readiness:  Readiness,
  cursor_header:  usize,
}

impl <Front:SocketHandler>ProxyProtocol<Front> {
  pub fn new(frontend: Front, backend: Option<TcpStream>) -> Self {
    ProxyProtocol {
      header: None,
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
      cursor_header: 0,
    }
  }

  // The header is send immediately at once upon the connection is establish
  // and prepended before any data.
  pub fn back_writable(&mut self) -> (ProtocolResult, ClientResult) {
    debug!("Writing proxy protocol header");

    if let Some(ref mut socket) = self.backend {
      if let Some(ref header) = self.header {
        let mut header = header.into_bytes();
        loop {
          match socket.write(&mut header[self.cursor_header..]) {
            Ok(sz) => {
              self.cursor_header += sz;

              if self.cursor_header == header.len() {
                debug!("Proxy protocol sent, upgrading");
                return (ProtocolResult::Upgrade, ClientResult::Continue)
              }
            },
            Err(e) => {
              debug!("PROXY PROTOCOL {}", e);
              break;
            },
          }
        }
      }
    }
    (ProtocolResult::Continue, ClientResult::Continue)
  }

  pub fn front_socket(&self) -> &TcpStream {
    self.frontend.socket_ref()
  }

  pub fn back_socket(&self) -> Option<&TcpStream> {
    self.backend.as_ref()
  }

  pub fn set_back_socket(&mut self, socket: TcpStream) {
    self.gen_proxy_protocol_header(&socket);
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

  fn gen_proxy_protocol_header(&mut self, socket: &TcpStream) {
    let addr_frontend = self.frontend.socket_ref().peer_addr().unwrap();
    let addr_backend = socket.peer_addr().unwrap();

    let protocol_header = ProxyProtocolHeader::V1(HeaderV1::new(addr_frontend, addr_backend));
    self.header = Some(protocol_header);
  }
}