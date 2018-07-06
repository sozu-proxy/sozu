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
use network::pool::Checkout;
use parser::proxy_protocol::parse_v2_header;
use super::header::ProxyAddr;

pub struct ExpectProxyProtocol<Front:SocketHandler> {
  pub frontend:       Front,
  pub frontend_token: Token,
  pub front_buf:      Checkout<BufferQueue>,
  pub readiness:      Readiness,
  pub addresses:      Option<ProxyAddr>,
}

impl <Front:SocketHandler + Read>ExpectProxyProtocol<Front> {
  pub fn new(frontend: Front, frontend_token: Token, front_buf: Checkout<BufferQueue>) -> Self {
    println!("expect starting, connection from {:?}", frontend.socket_ref().peer_addr());
    ExpectProxyProtocol {
      frontend,
      frontend_token,
      front_buf,
      readiness: Readiness {
        front_interest:  UnixReady::from(Ready::readable()) | UnixReady::hup() | UnixReady::error(),
        back_interest:   UnixReady::hup() | UnixReady::error(),
        front_readiness: UnixReady::from(Ready::empty()),
        back_readiness:  UnixReady::from(Ready::empty()),
      },
      addresses: None,
    }
  }

  pub fn readable(&mut self, metrics: &mut SessionMetrics) -> (ProtocolResult, ClientResult) {
    let (sz, res) = self.frontend.socket_read(self.front_buf.buffer.space());
    info!("FRONT proxy protocol [{:?}]: read {} bytes and res={:?}", self.frontend_token, sz, res);

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
        return (ProtocolResult::Continue, ClientResult::CloseClient);
      }

      if res == SocketResult::WouldBlock {
        self.readiness.front_readiness.remove(Ready::readable());
      }

      let read_sz = match parse_v2_header(self.front_buf.unparsed_data()) {
        Done(rest, header) => {
          self.addresses = Some(header.addr);
          self.front_buf.next_output_data().offset(rest)
        },
        Incomplete(_) => {
          return (ProtocolResult::Continue, ClientResult::Continue)
        },
        Error(e) => {
          return (ProtocolResult::Continue, ClientResult::CloseClient)
        }
      };

      self.front_buf.consume_parsed_data(read_sz);
      self.front_buf.delete_output(read_sz);
      info!("read {} bytes of proxy protocol, {} remaining", read_sz, self.front_buf.available_input_data());
      return (ProtocolResult::Upgrade, ClientResult::Continue)
    }

    return (ProtocolResult::Continue, ClientResult::Continue);
  }

  pub fn front_socket(&self) -> &TcpStream {
    self.frontend.socket_ref()
  }

  pub fn readiness(&mut self) -> &mut Readiness {
    &mut self.readiness
  }

  pub fn into_pipe(self, back_buf: Checkout<BufferQueue>, backend_socket: Option<TcpStream>, backend_token: Option<Token>) -> Pipe<Front> {
    let addr = if let Some(ref backend_socket) = backend_socket {
      backend_socket.peer_addr().map(|s| s.ip()).ok()
    } else {
      None
    };

    let mut pipe = Pipe::new(
      self.frontend,
      self.frontend_token,
      backend_socket,
      self.front_buf,
      back_buf,
      addr,
    );

    pipe.readiness.front_readiness = self.readiness.front_readiness;
    pipe.readiness.back_readiness  = self.readiness.back_readiness;

    if let Some(backend_token) = backend_token {
      pipe.set_back_token(backend_token);
    }

    pipe
  }
}

#[cfg(test)]
mod expect_test {

 use super::*;

  use parser::proxy_protocol::parse_v2_header;
  use nom::IResult::Done;
  use pool::Pool;

  use std::{thread, thread::JoinHandle, time::Duration, net::{SocketAddr, IpAddr, Ipv4Addr}};
  use mio::net::{TcpListener, TcpStream};
  use std::net::{TcpListener as StdTcpListener, TcpStream as StdTcpStream};

  use network::protocol::proxy_protocol::header::*;

  // Flow diagram of the test below
  //                [connect]   [send proxy protocol]
  //upfront proxy  ----------------------X
  //              /     |           |
  //  sozu     ---------v-----------v----X
  #[test]
  fn middleware_should_receive_proxy_protocol_header_from_an_upfront_middleware() {
    setup_test_logger!();
    let middleware_addr: SocketAddr = "127.0.0.1:3500".parse().expect("parse address error");

    let upfront = start_upfront_middleware(middleware_addr.clone());
    start_middleware(middleware_addr);

    upfront.join().expect("should join");
  }

  // Accept connection from an upfront proxy and expect to read a proxy protocol header in this stream.
  fn start_middleware(middleware_addr: SocketAddr) {
    let upfront_middleware_conn_listener = TcpListener::bind(&middleware_addr).expect("could not accept upfront middleware connection");
    let mut client_stream: Option<TcpStream> = None;

    // mio::TcpListener use a nonblocking mode so we have to loop on accept
    loop {
      if let Ok((stream, _addr)) = upfront_middleware_conn_listener.accept() {
        client_stream = Some(stream);
        break;
      }
    }

    let mut session_metrics = SessionMetrics::new();
    let mut pool = Pool::with_capacity(1, 0, || BufferQueue::with_capacity(16384));
    let mut front_buf = pool.checkout().unwrap();
    let mut expect_pp = ExpectProxyProtocol::new(client_stream.unwrap(), Token(0), front_buf);

    if (ProtocolResult::Upgrade, ClientResult::Continue) != expect_pp.readable(&mut session_metrics) {
      panic!("Should receive a complete proxy protocol header");
    };
  }

  // Connect to the next middleware and send a proxy protocol header
  fn start_upfront_middleware(next_middleware_addr: SocketAddr) -> JoinHandle<()> {
    thread::spawn(move|| {
      let src_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(125, 25, 10, 1)), 8080);
      let dst_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 4, 5, 8)), 4200);
      let proxy_protocol = HeaderV2::new(Command::Local, src_addr, dst_addr).into_bytes();

      match StdTcpStream::connect(&next_middleware_addr) {
        Ok(mut stream) => {
          stream.write(&proxy_protocol);
        },
        Err(e) => panic!("could not connect to the next middleware: {}", e),
      };
    })
  }
}
