use std::io::Write;
use std::io::Read;

use mio::*;
use mio::net::TcpStream;
use rusty_ulid::Ulid;
use nom::{Err,Offset};
use SessionResult;
use Readiness;
use protocol::ProtocolResult;
use socket::{SocketHandler, SocketResult};
use SessionMetrics;
use protocol::pipe::Pipe;
use super::parser::parse_v2_header;
use pool::Checkout;
use Protocol;
use sozu_command::ready::Ready;

pub struct RelayProxyProtocol<Front:SocketHandler> {
  pub header_size:    Option<usize>,
  pub frontend:       Front,
  pub request_id:     Ulid,
  pub backend:        Option<TcpStream>,
  pub frontend_token: Token,
  pub backend_token:  Option<Token>,
  pub front_buf:      Checkout,
  pub front_readiness:Readiness,
  pub back_readiness: Readiness,
  cursor_header:      usize,
}

impl <Front:SocketHandler + Read>RelayProxyProtocol<Front> {
  pub fn new(frontend: Front, frontend_token: Token, request_id: Ulid,
    backend: Option<TcpStream>, front_buf: Checkout) -> Self {

    RelayProxyProtocol {
      header_size: None,
      frontend,
      request_id,
      backend,
      frontend_token,
      backend_token:  None,
      front_buf,
      front_readiness: Readiness {
        interest: Ready::readable() | Ready::hup() | Ready::error(),
        event: Ready::empty(),
      },
      back_readiness: Readiness {
        interest: Ready::hup() | Ready::error(),
        event: Ready::empty(),
      },
      cursor_header: 0,
    }
  }

  pub fn readable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
    let (sz, res) = self.frontend.socket_read(self.front_buf.space());
    debug!("FRONT proxy protocol [{:?}]: read {} bytes and res={:?}", self.frontend_token, sz, res);

    if sz > 0 {
      self.front_buf.fill(sz);


      count!("bytes_in", sz as i64);
      metrics.bin += sz;

      if res == SocketResult::Error {
        error!("[{:?}] front socket error, closing the connection", self.frontend_token);
        metrics.service_stop();
        incr!("proxy_protocol.errors");
        self.front_readiness.reset();
        self.back_readiness.reset();
        return SessionResult::CloseSession;
      }

      if res == SocketResult::WouldBlock {
        self.front_readiness.event.remove(Ready::readable());
      }

      let read_sz = match parse_v2_header(self.front_buf.data()) {
        Ok((rest, _)) => {
          self.front_readiness.interest.remove(Ready::readable());
          self.back_readiness.interest.insert(Ready::writable());
          self.front_buf.data().offset(rest)
        },
        Err(Err::Incomplete(_)) => {
          return SessionResult::Continue
        },
        Err(e) => {
          error!("[{:?}] error parsing the proxy protocol header(error={:?}), closing the connection",
            self.frontend_token, e);
          return SessionResult::CloseSession
        }
      };

      self.header_size = Some(read_sz);
      self.front_buf.consume(sz);
      return SessionResult::Continue
    }

    SessionResult::Continue
  }

  // The header is send immediately at once upon the connection is establish
  // and prepended before any data.
  pub fn back_writable(&mut self, metrics: &mut SessionMetrics) -> (ProtocolResult, SessionResult) {
    debug!("Writing proxy protocol header");

    if let Some(ref mut socket) = self.backend {
      if let Some(ref header_size) = self.header_size {
        loop {
          match socket.write(self.front_buf.data()) {
            Ok(sz) => {
              self.cursor_header += sz;

              metrics.backend_bout += sz;
              self.front_buf.consume(sz);

              if self.cursor_header >= *header_size {
                info!("Proxy protocol sent, upgrading");
                return (ProtocolResult::Upgrade, SessionResult::Continue)
              }
            },
            Err(e) => {
              metrics.service_stop();
              incr!("proxy_protocol.errors");
              self.front_readiness.reset();
              self.back_readiness.reset();
              debug!("PROXY PROTOCOL {}", e);
              break;
            },
          }
        }
      }
    }
    (ProtocolResult::Continue, SessionResult::Continue)
  }

  pub fn front_socket(&self) -> &TcpStream {
    self.frontend.socket_ref()
  }

  pub fn front_socket_mut(&mut self) -> &mut TcpStream {
    self.frontend.socket_mut()
  }

  pub fn back_socket(&self) -> Option<&TcpStream> {
    self.backend.as_ref()
  }

  pub fn back_socket_mut(&mut self)  -> Option<&mut TcpStream> {
    self.backend.as_mut()
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

  pub fn front_readiness(&mut self) -> &mut Readiness {
    &mut self.front_readiness
  }

  pub fn back_readiness(&mut self) -> &mut Readiness {
    &mut self.back_readiness
  }

  pub fn into_pipe(mut self, back_buf: Checkout) -> Pipe<Front> {
    let backend_socket = self.backend.take().unwrap();
    let addr = self.front_socket().peer_addr().ok();

    let mut pipe = Pipe::new(
      self.frontend.take(0).into_inner(),
      self.frontend_token,
      self.request_id,
      None,
      None,
      None,
      Some(backend_socket),
      self.front_buf,
      back_buf,
      addr,
      Protocol::TCP
    );

    pipe.front_readiness.event = self.front_readiness.event;
    pipe.back_readiness.event  = self.back_readiness.event;

    if let Some(back_token) = self.backend_token {
      pipe.set_back_token(back_token);
    }

    pipe
  }
}
