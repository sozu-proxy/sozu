use std::net::IpAddr;
use std::io::{Write, ErrorKind};
use mio::*;
use mio::tcp::TcpStream;
use mio::unix::UnixReady;
use nom::IResult::*;

use network::protocol::proxy_protocol::header;
use network::{Protocol, ClientResult};
use network::Readiness;
use network::protocol::ProtocolResult;
use network::socket::{SocketHandler, SocketResult};
use network::buffer_queue::BufferQueue;
use network::SessionMetrics;
use parser::proxy_protocol::parse_v2_header;
use pool::Checkout;

pub struct FrontendProxyProtocol<Front:SocketHandler> {
  pub header:     Option<Vec<u8>>,
  pub frontend:   Front,
  pub backend:    Option<TcpStream>,
  frontend_token: Option<Token>,
  backend_token:  Option<Token>,
  pub front_buf:  Checkout<BufferQueue>,
  pub readiness:  Readiness,
  cursor_header:  usize,
}

impl <Front:SocketHandler>FrontendProxyProtocol<Front> {
  pub fn new(frontend: Front, backend: Option<TcpStream>, front_buf: Checkout<BufferQueue>) -> Self {
    FrontendProxyProtocol {
      header: None,
      frontend,
      backend,
      frontend_token: None,
      backend_token:  None,
      front_buf,
      readiness: Readiness {
        front_interest:  UnixReady::from(Ready::readable()) | UnixReady::hup() | UnixReady::error(),
        back_interest:   UnixReady::hup() | UnixReady::error(),
        front_readiness: UnixReady::from(Ready::empty()),
        back_readiness:  UnixReady::from(Ready::empty()),
      },
      cursor_header: 0,
    }
  }

  pub fn readable(&mut self, metrics: &mut SessionMetrics) -> ClientResult {
    let (sz, res) = self.frontend.socket_read(self.front_buf.buffer.space());
    debug!("FRONT proxy protocol [{:?}]: read {} bytes and res={:?}", self.frontend_token, sz, res);

    if sz > 0 {
      self.front_buf.buffer.fill(sz);
      self.front_buf.sliced_input(sz);
      self.front_buf.consume_parsed_data(sz);
      self.front_buf.slice_output(sz);

      let buf = self.front_buf.next_output_data();

      count!("bytes_in", sz as i64);
      metrics.bin += sz;
    
      match res {
        SocketResult::Continue
        | SocketResult::WouldBlock => {
          match parse_v2_header(buf) {
            Done(rest, header) => {
              debug!("We received the proxy protocol header: {:?}", header);
              self.header = Some(header.into_bytes());
              self.readiness.back_interest.insert(Ready::writable());
              self.readiness.front_readiness.remove(Ready::readable());
              return ClientResult::Continue
            },
            Incomplete(_) => {
              return ClientResult::Continue
            },
            Error(e) => {
              return ClientResult::CloseClient
            }
          }
        },
        SocketResult::Error => {
          error!("[{:?}] front socket error, closing the connection", self.frontend_token);
          metrics.service_stop();
          incr_ereq!();
          self.readiness.reset();
          return ClientResult::CloseClient
        }
      }
    }

    return ClientResult::Continue;
  }

  // The header is send immediately at once upon the connection is establish
  // and prepended before any data.
  pub fn back_writable(&mut self, metrics: &mut SessionMetrics) -> (ProtocolResult, ClientResult) {
    debug!("Writing proxy protocol header");

    if let Some(ref mut socket) = self.backend {
      if let Some(ref mut header) = self.header {
        loop {
          match socket.write(&mut header[self.cursor_header..]) {
            Ok(sz) => {
              self.cursor_header += sz;

              metrics.backend_bout += sz;

              if self.cursor_header == header.len() {
                debug!("Proxy protocol sent, upgrading");
                return (ProtocolResult::Upgrade, ClientResult::Continue)
              }
            },
            Err(e) => {
              metrics.service_stop();
              incr_ereq!();
              self.readiness.reset();
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

  pub fn readiness(&mut self) -> &mut Readiness {
    &mut self.readiness
  }

  pub fn protocol(&self) -> Protocol {
    Protocol::ProxyProtocol
  }
}
