use std::net::IpAddr;
use std::io::{Write, ErrorKind};
use std::io::Read;

use mio::*;
use mio::tcp::TcpStream;
use mio::unix::UnixReady;
use nom::IResult::*;
use nom::Offset;
use network::protocol::proxy_protocol::header;
use network::{Protocol, ClientResult};
use network::Readiness;
use network::protocol::ProtocolResult;
use network::socket::{SocketHandler, SocketResult};
use network::buffer_queue::BufferQueue;
use network::SessionMetrics;
use network::protocol::pipe::Pipe;
use parser::proxy_protocol::parse_v2_header;
use pool::Checkout;

pub struct RelayProxyProtocol<Front:SocketHandler> {
  pub header_size:    Option<usize>,
  pub frontend:       Front,
  pub backend:        Option<TcpStream>,
  pub frontend_token: Token,
  pub backend_token:  Option<Token>,
  pub front_buf:      Checkout<BufferQueue>,
  pub readiness:      Readiness,
  cursor_header:      usize,
}

impl <Front:SocketHandler + Read>RelayProxyProtocol<Front> {
  pub fn new(frontend: Front, frontend_token: Token,backend: Option<TcpStream>, front_buf: Checkout<BufferQueue>) -> Self {
    RelayProxyProtocol {
      header_size: None,
      frontend,
      backend,
      frontend_token,
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


      count!("bytes_in", sz as i64);
      metrics.bin += sz;

      if res == SocketResult::Error {
        error!("[{:?}] front socket error, closing the connection", self.frontend_token);
        metrics.service_stop();
        incr!("proxy_protocol.errors");
        self.readiness.reset();
        return ClientResult::CloseClient;
      }

      if res == SocketResult::WouldBlock {
        self.readiness.front_readiness.remove(Ready::readable());
      }

      let read_sz = match parse_v2_header(self.front_buf.unparsed_data()) {
        Done(rest, header) => {
          self.readiness.front_interest.remove(Ready::readable());
          self.readiness.back_interest.insert(Ready::writable());
          self.front_buf.next_output_data().offset(rest)
        },
        Incomplete(_) => {
          return ClientResult::Continue
        },
        Error(e) => {
          return ClientResult::CloseClient
        }
      };

      self.header_size = Some(read_sz);
      self.front_buf.consume_parsed_data(sz);
      self.front_buf.slice_output(sz);
      return ClientResult::Continue
    }

    return ClientResult::Continue;
  }

  // The header is send immediately at once upon the connection is establish
  // and prepended before any data.
  pub fn back_writable(&mut self, metrics: &mut SessionMetrics) -> (ProtocolResult, ClientResult) {
    debug!("Writing proxy protocol header");

    if let Some(ref mut socket) = self.backend {
      if let Some(ref header_size) = self.header_size {
        loop {
          match socket.write(self.front_buf.next_output_data()) {
            Ok(sz) => {
              self.cursor_header += sz;

              metrics.backend_bout += sz;
              self.front_buf.consume_output_data(sz);

              if self.cursor_header >= *header_size {
                info!("Proxy protocol sent, upgrading");
                return (ProtocolResult::Upgrade, ClientResult::Continue)
              }
            },
            Err(e) => {
              metrics.service_stop();
              incr!("proxy_protocol.errors");
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

  pub fn back_token(&self) -> Option<Token> {
    self.backend_token
  }

  pub fn set_back_token(&mut self, token: Token) {
    self.backend_token = Some(token);
  }

  pub fn readiness(&mut self) -> &mut Readiness {
    &mut self.readiness
  }

  pub fn into_pipe(mut self, back_buf: Checkout<BufferQueue>) -> Pipe<Front> {
    let backend_socket = self.backend.take().unwrap();
    let addr = backend_socket.peer_addr().map(|s| s.ip()).ok();

    let mut pipe = Pipe::new(
      self.frontend.take(0).into_inner(),
      self.frontend_token,
      Some(backend_socket),
      self.front_buf,
      back_buf,
      addr,
    );

    pipe.readiness.front_readiness = self.readiness.front_readiness;
    pipe.readiness.back_readiness  = self.readiness.back_readiness;

    if let Some(back_token) = self.backend_token {
      pipe.set_back_token(back_token);
    }

    pipe
  }
}
