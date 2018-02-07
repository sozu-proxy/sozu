use std::net::IpAddr;
use mio::*;
use mio::tcp::TcpStream;
use mio::unix::UnixReady;
use network::{ClientResult, Protocol};
use network::socket::{SocketHandler,SocketResult};
use network::protocol::ProtocolResult;
use network::session::Readiness;
use network::header_proxy_protocol::ProxyProtocolHeader;

pub struct ProxyProtocol {
  header:        ProxyProtocolHeader,
  backend:       Option<TcpStream>,
  pub readiness: Readiness,
}

impl ProxyProtocol {
  pub fn new(backend: Option<TcpStream>, header: ProxyProtocolHeader) -> Self {
    ProxyProtocol {
      header,
      backend,
      readiness: Readiness {
        front_interest:  UnixReady::from(UnixReady::hup() | UnixReady::error()),
        back_interest:   UnixReady::from(Ready::writable()),
        front_readiness: UnixReady::from(Ready::empty()),
        back_readiness:  UnixReady::from(Ready::empty()),
      },
    }
  }

  // The header is send immediately at once upon the connection is establish
  // and prepended before any data.
  pub fn back_writable(&mut self) -> (ProtocolResult, ClientResult) {
    //TODO: writte the header in the backend
    trace!("Writte proxy protocol header");
    trace!("The header should be receive. We switch to a Pipe");
    (ProtocolResult::Upgrade, ClientResult::Continue)
  }

  pub fn set_back_socket(&mut self, socket: TcpStream) {
    self.backend = Some(socket);
  }

  pub fn readiness(&mut self) -> &mut Readiness {
    &mut self.readiness
  }

  pub fn protocol(&self) -> Protocol {
    Protocol::ProxyProtocol
  }
}