use std::net::IpAddr;
use std::io::{Write, ErrorKind};
use std::io::Read;

use mio::*;
use mio::tcp::TcpStream;
use mio::unix::UnixReady;
use network::ClientResult;
use network::Readiness;
use network::BackendConnectionStatus;
use network::protocol::ProtocolResult;
use network::socket::{SocketHandler,SocketResult,server_bind};
use network::protocol::pipe::Pipe;
use network::buffer_queue::BufferQueue;
use pool::Checkout;

use super::header::*;

pub struct SendProxyProtocol<Front:SocketHandler> {
  pub header:         Option<Vec<u8>>,
  pub frontend:       Front,
  pub backend:        Option<TcpStream>,
  pub frontend_token: Option<Token>,
  pub backend_token:  Option<Token>,
  pub readiness:      Readiness,
  cursor_header:      usize,
}

impl <Front:SocketHandler + Read> SendProxyProtocol<Front> {
  pub fn new(frontend: Front, backend: Option<TcpStream>) -> Self {
    SendProxyProtocol {
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
      if let Some(ref mut header) = self.header {
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

  pub fn set_back_connected(&mut self, status: BackendConnectionStatus) {
    if status == BackendConnectionStatus::Connected {
      self.gen_proxy_protocol_header();
    }
  }

  pub fn readiness(&mut self) -> &mut Readiness {
    &mut self.readiness
  }

  fn gen_proxy_protocol_header(&mut self) {
    let addr_frontend = self.frontend.socket_ref().peer_addr().expect("frontend address should be available");

    let addr_backend = self.backend.as_ref().and_then(|socket| {
      socket.peer_addr().map_err(|e| error!("cannot get backend address: {:?}", e)).ok()
    });

    let addr_backend = match addr_backend {
      Some(addr) => addr,
      None       => return,
    };

    // PROXY command hardcoded for now, but we'll use LOCAL when we implement health checks
    let protocol_header = ProxyProtocolHeader::V2(HeaderV2::new(Command::Proxy, addr_frontend, addr_backend));
    self.header = Some(protocol_header.into_bytes());
  }

  pub fn into_pipe(mut self, front_buf: Checkout<BufferQueue>, back_buf: Checkout<BufferQueue>) -> Pipe<Front> {
    let backend_socket = self.backend.take().unwrap();
    let addr = backend_socket.peer_addr().map(|s| s.ip()).ok();

    let mut pipe = Pipe::new(
      self.frontend.take(0).into_inner(),
      Some(backend_socket),
      front_buf,
      back_buf,
      addr,
    );

    pipe.readiness.front_readiness = self.readiness.front_readiness;
    pipe.readiness.back_readiness  = self.readiness.back_readiness;

    if let Some(front_token) = self.frontend_token {
      pipe.set_front_token(front_token);
    }

    if let Some(back_token) = self.backend_token {
      pipe.set_back_token(back_token);
    }

    pipe
  }
}
