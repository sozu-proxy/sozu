use std::net::IpAddr;
use std::io::{Write, ErrorKind};
use std::io::Read;

use mio::*;
use mio::tcp::TcpStream;
use mio::unix::UnixReady;
use network::{
  SessionMetrics,
  ClientResult,
  Readiness,
  BackendConnectionStatus,
  protocol::{ProtocolResult, pipe::Pipe},
  socket::{SocketHandler, SocketResult, server_bind},
  buffer_queue::BufferQueue,
  pool::Checkout,
};

use super::header::*;

pub struct SendProxyProtocol<Front:SocketHandler> {
  pub header:         Option<Vec<u8>>,
  pub frontend:       Front,
  pub backend:        Option<TcpStream>,
  pub frontend_token: Token,
  pub backend_token:  Option<Token>,
  pub readiness:      Readiness,
  cursor_header:      usize,
}

impl <Front:SocketHandler + Read> SendProxyProtocol<Front> {
  pub fn new(frontend: Front, frontend_token: Token, backend: Option<TcpStream>) -> Self {
    SendProxyProtocol {
      header: None,
      frontend,
      backend,
      frontend_token,
      backend_token:  None,
      readiness: Readiness {
        interest: UnixReady::hup() | UnixReady::error(),
        event:    UnixReady::from(Ready::empty()),
      },
      cursor_header: 0,
    }
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
                return (ProtocolResult::Upgrade, ClientResult::Continue);
              }
            },
            Err(e) => match e.kind() {
              ErrorKind::WouldBlock => {
                self.readiness.event.remove(Ready::writable());
                return (ProtocolResult::Continue, ClientResult::Continue);
              },
              e => {
                incr!("proxy_protocol.errors");
                debug!("send proxy protocol write error {:?}", e);
                return (ProtocolResult::Continue, ClientResult::CloseClient);
              }
            },
          }
        }
      }
    }

    error!("started Send proxy protocol with no header or backend socket");
    (ProtocolResult::Continue, ClientResult::CloseClient)
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

  pub fn set_back_connected(&mut self, status: BackendConnectionStatus) {
    if status == BackendConnectionStatus::Connected {
      self.gen_proxy_protocol_header();
      self.readiness.interest.insert(UnixReady::from(Ready::writable()));
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
      self.frontend,
      self.frontend_token,
      Some(backend_socket),
      front_buf,
      back_buf,
      addr,
    );

    pipe.back_readiness.event  = self.readiness.event;

    if let Some(back_token) = self.backend_token {
      pipe.set_back_token(back_token);
    }

    pipe
  }
}

#[cfg(test)]
mod send_test {

  use super::*;

  use parser::proxy_protocol::parse_v2_header;

  use std::{sync::{Arc, Barrier}, thread::{self, JoinHandle}, time::Duration, net::SocketAddr};
  use mio::net::{TcpListener, TcpStream};
  use std::net::{TcpListener as StdTcpListener, TcpStream as StdTcpStream};
  use std::os::unix::io::{FromRawFd,IntoRawFd};

  #[test]
  fn it_should_send_a_proxy_protocol_header_to_the_upstream_backend() {
    setup_test_logger!();
    let addr_client: SocketAddr = "127.0.0.1:6666".parse().expect("parse address error");
    let addr_backend: SocketAddr = "127.0.0.1:2001".parse().expect("parse address error");
    let barrier = Arc::new(Barrier::new(3));
    let end_barrier = Arc::new(Barrier::new(2));

    start_client(addr_client.clone(), barrier.clone(), end_barrier.clone());
    let backend = start_backend(addr_backend.clone(), barrier.clone(), end_barrier.clone());
    start_middleware(addr_client, addr_backend, barrier.clone());

    backend.join().expect("Couldn't join on the associated backend");
  }

  // Get connection from the client and connect to the backend
  // When connections are etablish we send the proxy protocol header
  fn start_middleware(addr_client: SocketAddr, addr_backend: SocketAddr, barrier: Arc<Barrier>) {
    let client_listener = TcpListener::bind(&addr_client).expect("could not accept client connection");

    let client_stream;
    barrier.wait();

    loop {
      if let Ok((stream, _addr)) = client_listener.accept() {
        client_stream = stream;
        break;
      }
    }

    // connect in blocking first, then convert to a mio socket
    let backend_stream = StdTcpStream::connect(&addr_backend).expect("could not connect to the backend");
    let fd = backend_stream.into_raw_fd();
    let backend_stream = unsafe { TcpStream::from_raw_fd(fd) };

    let mut send_pp = SendProxyProtocol::new(client_stream, Token(0), Some(backend_stream));
    let mut session_metrics = SessionMetrics::new();

    send_pp.set_back_connected(BackendConnectionStatus::Connected);

    loop {
      let (protocol, client) = send_pp.back_writable(&mut session_metrics);
      if client != ClientResult::Continue {
        panic!("state machine error: protocol result = {:?}, client result = {:?}", protocol, client);
      }

      if protocol == ProtocolResult::Upgrade {
        break;
      }
    }
  }

  // Only connect to the middleware
  fn start_client(addr: SocketAddr, barrier: Arc<Barrier>, end_barrier: Arc<Barrier>) {
    thread::spawn(move|| {
      barrier.wait();

      let stream = StdTcpStream::connect(&addr).unwrap();

      end_barrier.wait();
    });
  }

  // Get connection from the middleware read on the socket stream.
  // We check if we receive a valid proxy protocol header
  fn start_backend(addr: SocketAddr, barrier: Arc<Barrier>, end_barrier: Arc<Barrier>) -> JoinHandle<()> {
    let listener = StdTcpListener::bind(&addr).expect("could not start backend");

    thread::spawn(move|| {
      barrier.wait();

      let mut buf: [u8; 28] = [0; 28];
      let (mut conn, _) = listener.accept().expect("could not accept connection from light middleware");
      println!("backend got a connection from the middleware");

      let mut index = 0usize;
      loop {
        if index >= 28 {
          break;
        }

        match conn.read(&mut buf[index..]) {
          Err(e) => match e.kind() {
            ErrorKind::WouldBlock => continue,
            e => {
              end_barrier.wait();
              panic!("read error: {:?}", e);
            }
          },
          Ok(sz) => {
            println!("backend read {} bytes", sz);
            index += sz;
          },
        }
      }

      match parse_v2_header(&buf) {
        Ok((_,_)) => println!("complete header received"),
        err => {
          end_barrier.wait();
          panic!("incorrect proxy protocol header received: {:?}", err);
        }
      };

      end_barrier.wait();
    })
  }
}
