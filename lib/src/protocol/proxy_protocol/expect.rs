use std::io::Read;

use mio::*;
use mio::net::TcpStream;
use nom::{Err, HexDisplay};
use rusty_ulid::Ulid;
use SessionResult;
use Readiness;
use protocol::ProtocolResult;
use socket::{SocketHandler, SocketResult};
use SessionMetrics;
use protocol::pipe::Pipe;
use pool::Checkout;
use super::parser::parse_v2_header;
use super::header::ProxyAddr;
use Protocol;
use sozu_command::ready::Ready;

#[derive(Clone,Copy)]
pub enum HeaderLen {
  V4,
  V6,
  Unix
}

pub struct ExpectProxyProtocol<Front:SocketHandler> {
  pub frontend:       Front,
  pub frontend_token: Token,
  pub request_id:     Ulid,
  pub buf:            [u8; 232],
  pub index:          usize,
  pub header_len:     HeaderLen,
  pub readiness:      Readiness,
  pub addresses:      Option<ProxyAddr>,
}

impl <Front:SocketHandler + Read>ExpectProxyProtocol<Front> {
  pub fn new(frontend: Front, frontend_token: Token, request_id: Ulid) -> Self {
    ExpectProxyProtocol {
      frontend,
      frontend_token,
      request_id,
      buf: [0; 232],
      index: 0,
      header_len: HeaderLen::V4,
      readiness: Readiness {
        interest: Ready::readable(),
        event: Ready::empty(),
      },
      addresses: None,
    }
  }

  pub fn readable(&mut self, metrics: &mut SessionMetrics) -> (ProtocolResult, SessionResult) {
    let total_len = match self.header_len {
      HeaderLen::V4   => 28,
      HeaderLen::V6   => 52,
      HeaderLen::Unix => 232,
    };

    let (sz, res) = self.frontend.socket_read(&mut self.buf[self.index..total_len]);
    trace!("FRONT proxy protocol [{:?}]: read {} bytes and res={:?}, index = {}, total_len = {}",
      self.frontend_token, sz, res, self.index, total_len);

    if sz > 0 {
      self.index += sz;

      count!("bytes_in", sz as i64);
      metrics.bin += sz;

      if self.index == self.buf.len() {
        self.readiness.interest.remove(Ready::readable());
      }
    } else {
      self.readiness.event.remove(Ready::readable());
    }

    if res == SocketResult::Error {
      error!("[{:?}] (expect proxy) front socket error, closing the connection(read {}, wrote {})", self.frontend_token, metrics.bin, metrics.bout);
      metrics.service_stop();
      incr!("proxy_protocol.errors");
      self.readiness.reset();
      return (ProtocolResult::Continue, SessionResult::CloseSession);
    }

    if res == SocketResult::WouldBlock {
      self.readiness.event.remove(Ready::readable());
    }

    match parse_v2_header(&self.buf[..self.index]) {
      Ok((rest, header)) => {
        trace!("got expect header: {:?}, rest.len() = {}", header, rest.len());
        self.addresses = Some(header.addr);
        (ProtocolResult::Upgrade, SessionResult::Continue)
      },
      Err(Err::Incomplete(_)) => {
        match self.header_len {
          HeaderLen::V4 => if self.index == 28 {
            self.header_len = HeaderLen::V6;
          },
          HeaderLen::V6 => if self.index == 52 {
            self.header_len = HeaderLen::Unix;
          },
          HeaderLen::Unix => if self.index == 232 {
            error!("[{:?}] front socket parse error, closing the connection", self.frontend_token);
            metrics.service_stop();
            incr!("proxy_protocol.errors");
            self.readiness.reset();
            return (ProtocolResult::Continue, SessionResult::CloseSession)
          }
        };
        (ProtocolResult::Continue, SessionResult::Continue)
      },
      Err(Err::Error(e)) | Err(Err::Failure(e)) => {
        error!("[{:?}] expect proxy protocol front socket parse error, closing the connection:\n{}", self.frontend_token, e.input.to_hex(16));
        metrics.service_stop();
        incr!("proxy_protocol.errors");
        self.readiness.reset();
        (ProtocolResult::Continue, SessionResult::CloseSession)
      }
    }
  }

  pub fn front_socket(&self) -> &TcpStream {
    self.frontend.socket_ref()
  }

  pub fn front_socket_mut(&mut self) -> &mut TcpStream {
    self.frontend.socket_mut()
  }

  pub fn readiness(&mut self) -> &mut Readiness {
    &mut self.readiness
  }

  pub fn into_pipe(self, front_buf: Checkout, back_buf: Checkout,
    backend_socket: Option<TcpStream>, backend_token: Option<Token>) -> Pipe<Front> {

    let addr = self.front_socket().peer_addr().ok();

    let mut pipe = Pipe::new(
      self.frontend,
      self.frontend_token,
      self.request_id,
      None,
      None,
      None,
      backend_socket,
      front_buf,
      back_buf,
      addr,
      Protocol::TCP
    );

    pipe.front_readiness.event = self.readiness.event;

    if let Some(backend_token) = backend_token {
      pipe.set_back_token(backend_token);
    }

    pipe
  }
}

#[cfg(test)]
mod expect_test {

  use super::*;
  use std::{sync::{Arc, Barrier}, thread::{self, JoinHandle}, net::{SocketAddr, IpAddr, Ipv4Addr}};
  use mio::net::TcpListener;
  use std::net::{TcpStream as StdTcpStream};
  use std::io::Write;
  use rusty_ulid::Ulid;

  use protocol::proxy_protocol::header::*;

  // Flow diagram of the test below
  //                [connect]   [send proxy protocol]
  //upfront proxy  ----------------------X
  //              /     |           |
  //  sozu     ---------v-----------v----X
  #[test]
  fn middleware_should_receive_proxy_protocol_header_from_an_upfront_middleware() {
    setup_test_logger!();
    let middleware_addr: SocketAddr = "127.0.0.1:3500".parse().expect("parse address error");
    let barrier = Arc::new(Barrier::new(2));

    let upfront = start_upfront_middleware(middleware_addr.clone(), barrier.clone());
    start_middleware(middleware_addr, barrier);

    upfront.join().expect("should join");
  }

  // Accept connection from an upfront proxy and expect to read a proxy protocol header in this stream.
  fn start_middleware(middleware_addr: SocketAddr, barrier: Arc<Barrier>) {
    let upfront_middleware_conn_listener = TcpListener::bind(middleware_addr).expect("could not accept upfront middleware connection");
    let session_stream;
    barrier.wait();

    // mio::TcpListener use a nonblocking mode so we have to loop on accept
    loop {
      if let Ok((stream, _addr)) = upfront_middleware_conn_listener.accept() {
        session_stream = stream;
        break;
      }
    }

    let mut session_metrics = SessionMetrics::new(None);
    let mut expect_pp = ExpectProxyProtocol::new(session_stream, Token(0),
      Ulid::generate());

    let mut res = (ProtocolResult::Continue, SessionResult::Continue);
    while res == (ProtocolResult::Continue, SessionResult::Continue) {
      res = expect_pp.readable(&mut session_metrics);
    }

    if res != (ProtocolResult::Upgrade, SessionResult::Continue) {
      panic!("Should receive a complete proxy protocol header, res = {:?}", res);
    };
  }

  // Connect to the next middleware and send a proxy protocol header
  fn start_upfront_middleware(next_middleware_addr: SocketAddr, barrier: Arc<Barrier>) -> JoinHandle<()> {
    thread::spawn(move|| {
      let src_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(125, 25, 10, 1)), 8080);
      let dst_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 4, 5, 8)), 4200);
      let proxy_protocol = HeaderV2::new(Command::Local, src_addr, dst_addr).into_bytes();

      barrier.wait();
      match StdTcpStream::connect(&next_middleware_addr) {
        Ok(mut stream) => {
          stream.write(&proxy_protocol).unwrap();
        },
        Err(e) => panic!("could not connect to the next middleware: {}", e),
      };
    })
  }
}
