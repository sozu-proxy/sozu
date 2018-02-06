use std::thread::{self,Thread,Builder};
use std::sync::mpsc::{self,channel,Receiver};
use std::sync::{Arc,Mutex};
use std::rc::{Rc,Weak};
use std::cell::RefCell;
use std::mem;
use std::net::Shutdown;
use mio::*;
use mio::net::*;
use mio_uds::UnixStream;
use mio::unix::UnixReady;
use std::io::{self,Read,Write,ErrorKind,BufReader};
use std::collections::HashMap;
use std::error::Error;
use slab::Slab;
use pool::{Pool,Checkout};
use std::net::{IpAddr,SocketAddr};
use std::str::{FromStr, from_utf8, from_utf8_unchecked};
use time::{precise_time_s, precise_time_ns};
use rand::random;
use rustls::ServerSession;
use nom::IResult;

use sozu_command::buffer::Buffer;
use sozu_command::channel::Channel;
use sozu_command::messages::{self,Application,CertFingerprint,CertificateAndKey,Order,HttpsFront,HttpsProxyConfiguration,OrderMessage,
  OrderMessageAnswer,OrderMessageStatus};

use parser::http11::{HttpState,RequestState,ResponseState,RRequestLine,parse_request_until_stop,hostname_and_port};
use network::buffer_queue::BufferQueue;
use network::{AppId,Backend,ClientResult,ConnectionError,Protocol};
use network::backends::BackendMap;
use network::proxy::{Server,ProxyChannel};
use network::session::{BackendConnectAction,BackendConnectionStatus,ProxyClient,ProxyConfiguration,
  Readiness,ListenToken,AcceptError,Session,SessionMetrics};
use network::http::{self,DefaultAnswers};
use network::socket::{SocketHandler,SocketResult,server_bind,FrontRustls};
use network::trie::*;
use network::protocol::{ProtocolResult,TlsHandshake,Http,Pipe,StickySession};
use network::protocol::http::DefaultAnswerStatus;
use network::retry::RetryPolicy;
use util::UnwrapLog;
use super::configuration::{ServerConfiguration,TlsApp};


type BackendToken = Token;

type ClientToken = Token;

pub enum State {
  Handshake(TlsHandshake),
  Http(Http<FrontRustls>),
  WebSocket(Pipe<FrontRustls>)
}

pub struct TlsClient {
  pub front:          Option<TcpStream>,
  pub front_token:    Option<Token>,
  pub instance:       Option<Rc<RefCell<Backend>>>,
  pub back_connected: BackendConnectionStatus,
  protocol:       Option<State>,
  pub public_address: Option<IpAddr>,
  pool:           Weak<RefCell<Pool<BufferQueue>>>,
  pub sticky_session: bool,
  pub metrics:        SessionMetrics,
}

impl TlsClient {
  pub fn new(ssl: ServerSession, sock: TcpStream, pool: Weak<RefCell<Pool<BufferQueue>>>, public_address: Option<IpAddr>) -> TlsClient {
    //FIXME: we should not need to clone the socket. Maybe do the accept here instead of
    // in TlsHandshake?
    let s = sock.try_clone().expect("could not clone the socket");
    let handshake = TlsHandshake::new(ssl, s);
    TlsClient {
      front:          Some(sock),
      front_token:    None,
      instance:       None,
      back_connected: BackendConnectionStatus::NotConnected,
      protocol:       Some(State::Handshake(handshake)),
      public_address: public_address,
      pool:           pool,
      sticky_session: false,
      metrics:        SessionMetrics::new(),
    }
  }

  pub fn http(&mut self) -> Option<&mut Http<FrontRustls>> {
    self.protocol.as_mut().and_then(|protocol| {
      if let &mut State::Http(ref mut http) = protocol {
        Some(http)
      } else {
        None
      }
    })
  }

  pub fn set_answer(&mut self, answer: DefaultAnswerStatus, buf: &[u8])  {
    self.protocol.as_mut().map(|protocol| {
      if let &mut State::Http(ref mut http) = protocol {
        http.set_answer(answer, buf);
      }
    });
  }

  pub fn upgrade(&mut self) -> bool {
    let protocol = unwrap_msg!(self.protocol.take());

    if let State::Handshake(handshake) = protocol {
      if let Some(pool) = self.pool.upgrade() {
        let mut p = pool.borrow_mut();

        if let (Some(front_buf), Some(back_buf)) = (p.checkout(), p.checkout()) {
          let front_stream = FrontRustls {
            stream:  handshake.stream,
            session: handshake.session,
          };

          let mut http = Http::new(front_stream, front_buf,
            back_buf, self.public_address.clone(), Protocol::HTTPS).unwrap();

          http.readiness = handshake.readiness;
          http.readiness.front_interest = UnixReady::from(Ready::readable()) | UnixReady::hup() | UnixReady::error();
          http.set_front_token(unwrap_msg!(self.front_token.as_ref()).clone());
          self.protocol = Some(State::Http(http));
          return true;
        } else {
          error!("could not get buffers");
          //FIXME: must return an error and stop the connection here
        }
      }
      false
    } else if let State::Http(http) = protocol {
      debug!("https switching to wss");
      let front_token = unwrap_msg!(http.front_token());
      let back_token  = unwrap_msg!(http.back_token());

      let mut pipe = Pipe::new(http.frontend, http.backend,
        http.front_buf, http.back_buf, http.public_address);

      pipe.readiness.front_readiness = http.readiness.front_readiness;
      pipe.readiness.back_readiness  = http.readiness.back_readiness;
      pipe.set_front_token(front_token);
      pipe.set_back_token(back_token);

      self.protocol = Some(State::WebSocket(pipe));
      true
    } else {
      self.protocol = Some(protocol);
      true
    }
  }

  fn front_hup(&mut self)     -> ClientResult {
    self.http().map(|h| h.front_hup()).unwrap_or(ClientResult::CloseClient)
  }

  fn back_hup(&mut self)      -> ClientResult {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http)      => http.back_hup(),
      State::WebSocket(ref mut pipe) => pipe.back_hup(),
      State::Handshake(_)            => {
        error!("why a backend HUP event while still in frontend handshake?");
        ClientResult::CloseClient
      }
    }
  }

  fn log_context(&self)  -> String {
    if let &State::Http(ref http) = unwrap_msg!(self.protocol.as_ref()) {
      http.log_context()
    } else {
      "".to_string()
    }
  }

  fn readable(&mut self)      -> ClientResult {
    let (upgrade, result) = match *unwrap_msg!(self.protocol.as_mut()) {
      State::Handshake(ref mut handshake) => handshake.readable(),
      State::Http(ref mut http)           => (ProtocolResult::Continue, http.readable(&mut self.metrics)),
      State::WebSocket(ref mut pipe)      => (ProtocolResult::Continue, pipe.readable(&mut self.metrics)),
    };

    if upgrade == ProtocolResult::Continue {
      result
    } else {
        if self.upgrade() {
        match *unwrap_msg!(self.protocol.as_mut()) {
          State::Http(ref mut http) => http.readable(&mut self.metrics),
          _ => result
        }
      } else {
        ClientResult::CloseClient
      }
    }
  }

  fn writable(&mut self)      -> ClientResult {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Handshake(ref mut handshake) => ClientResult::CloseClient,
      State::Http(ref mut http)           => http.writable(&mut self.metrics),
      State::WebSocket(ref mut pipe)      => pipe.writable(&mut self.metrics),
    }
  }

  fn back_readable(&mut self) -> ClientResult {
    let (upgrade, result) = match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http)           => http.back_readable(&mut self.metrics),
      State::Handshake(ref mut handshake) => (ProtocolResult::Continue, ClientResult::CloseClient),
      State::WebSocket(ref mut pipe)      => (ProtocolResult::Continue, pipe.back_readable(&mut self.metrics)),
    };

    if upgrade == ProtocolResult::Continue {
      result
    } else {
      if self.upgrade() {
        match *unwrap_msg!(self.protocol.as_mut()) {
          State::WebSocket(ref mut pipe) => pipe.back_readable(&mut self.metrics),
          _ => result
        }
      } else {
        ClientResult::CloseBoth
      }
    }
  }

  fn back_writable(&mut self) -> ClientResult {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Handshake(ref mut handshake) => ClientResult::CloseClient,
      State::Http(ref mut http)           => http.back_writable(&mut self.metrics),
      State::WebSocket(ref mut pipe)      => pipe.back_writable(&mut self.metrics),
    }
  }

  pub fn front_socket(&self) -> &TcpStream {
    unwrap_msg!(self.front.as_ref())
  }

  pub fn back_socket(&self)  -> Option<&TcpStream> {
    match unwrap_msg!(self.protocol.as_ref()) {
      &State::Handshake(ref handshake) => None,
      &State::Http(ref http)           => http.back_socket(),
      &State::WebSocket(ref pipe)      => pipe.back_socket(),
    }
  }

  pub fn front_token(&self)  -> Option<Token> {
    self.front_token
  }

  pub fn back_token(&self)   -> Option<Token> {
    if let &State::Http(ref http) = unwrap_msg!(self.protocol.as_ref()) {
      http.back_token()
    } else {
      None
    }
  }

  pub fn set_back_socket(&mut self, sock:TcpStream) {
    unwrap_msg!(self.http()).set_back_socket(sock)
  }

  pub fn set_front_token(&mut self, token: Token) {
    self.front_token = Some(token);
    self.protocol.as_mut().map(|p| match *p {
      State::Http(ref mut http) => http.set_front_token(token),
      _                         => {}
    });
  }

  pub fn set_back_token(&mut self, token: Token) {
    unwrap_msg!(self.http()).set_back_token(token)
  }

  fn back_connected(&self)     -> BackendConnectionStatus {
    self.back_connected
  }

  fn set_back_connected(&mut self, connected: BackendConnectionStatus) {
    self.back_connected = connected;

    if connected == BackendConnectionStatus::Connected {
      self.instance.as_ref().map(|instance| {
        let ref mut backend = *instance.borrow_mut();
        backend.failures = 0;
        backend.retry_policy.succeed();
      });
    }
  }

  fn metrics(&mut self)        -> &mut SessionMetrics {
    &mut self.metrics
  }

  fn remove_backend(&mut self) -> (Option<String>, Option<SocketAddr>) {
    unwrap_msg!(self.http()).remove_backend()
  }

  pub fn readiness(&mut self)      -> &mut Readiness {
    let r = match *unwrap_msg!(self.protocol.as_mut()) {
      State::Handshake(ref mut handshake) => &mut handshake.readiness,
      State::Http(ref mut http)           => http.readiness(),
      State::WebSocket(ref mut pipe)      => &mut pipe.readiness,
    };
    //info!("current readiness: {:?}", r);
    r
  }
}

impl ProxyClient for TlsClient {

  fn close(&mut self, poll: &mut Poll, configuration: &mut ProxyConfiguration<TlsClient>) -> Vec<Token> {
    //println!("TLS closing[{:?}] temp->front: {:?}, temp->back: {:?}", self.token, *self.temp.front_buf, *self.temp.back_buf);
    self.http().map(|http| http.close());
    self.metrics.service_stop();
    self.front_socket().shutdown(Shutdown::Both);
    poll.deregister(self.front_socket());

    if let (Some(app_id), Some(addr)) = self.remove_backend() {
      configuration.close_backend(app_id, &addr);
      decr!("backend.connections");
    }

    if let Some(sock) = self.back_socket() {
      sock.shutdown(Shutdown::Both);
      poll.deregister(sock);
    }

    let mut res = vec!();
    if let Some(tk) = self.front_token {
      res.push(tk)
    }
    if let Some(tk) = self.back_token() {
      res.push(tk)
    }

    res
  }

  fn protocol(&self)           -> Protocol {
    Protocol::HTTPS
  }

  fn process_events(&mut self, token: Token, events: Ready) {
    if self.front_token == Some(token) {
      self.readiness().front_readiness = self.readiness().front_readiness | UnixReady::from(events);
    } else if self.back_token() == Some(token) {
      self.readiness().back_readiness = self.readiness().back_readiness | UnixReady::from(events);
    }
  }

  fn ready(&mut self) -> ClientResult {
    let mut counter = 0;
    let max_loop_iterations = 100000;

    self.metrics().service_start();

    if self.back_connected() == BackendConnectionStatus::Connecting {
      if self.readiness().back_readiness.is_hup() {
        //retry connecting the backend
        //FIXME: there should probably be a circuit breaker per client too
        error!("error connecting to backend, trying again");
        self.metrics().service_stop();
        return ClientResult::ConnectBackend;
      } else {
        self.set_back_connected(BackendConnectionStatus::Connected);
      }
    }

    let token = self.front_token.clone();
    while counter < max_loop_iterations {
      let front_interest = self.readiness().front_interest & self.readiness().front_readiness;
      let back_interest  = self.readiness().back_interest & self.readiness().back_readiness;

      //info!("PROXY\t{:?} {:?} | front: {:?} | back: {:?} ", token, self.readiness(), front_interest, back_interest);

      if front_interest == UnixReady::from(Ready::empty()) && back_interest == UnixReady::from(Ready::empty()) {
        break;
      }

      if front_interest.is_readable() {
        let order = self.readable();
        trace!("front readable\tinterpreting client order {:?}", order);

        if order != ClientResult::Continue {
          return order;
        }
      }

      if back_interest.is_writable() {
        let order = self.back_writable();
        if order != ClientResult::Continue {
          return order;
        }
      }

      if back_interest.is_readable() {
        let order = self.back_readable();
        if order != ClientResult::Continue {
          return order;
        }
      }

      if front_interest.is_writable() {
        let order = self.writable();
        trace!("front writable\tinterpreting client order {:?}", order);
        if order != ClientResult::Continue {
          return order;
        }
      }

      if front_interest.is_hup() {
        let order = self.front_hup();
        match order {
          ClientResult::CloseClient |  ClientResult::CloseBoth => {
            return order;
          },
          _ => {
            self.readiness().front_readiness.remove(UnixReady::hup());
            return order;
          }
        }
      }

      if back_interest.is_hup() {
        let order = self.back_hup();
        match order {
          ClientResult::CloseClient |  ClientResult::CloseBoth => {
            return order;
          },
          ClientResult::Continue => {
            self.readiness().front_interest.insert(Ready::writable());
            if ! self.readiness().front_readiness.is_writable() {
              break;
            }
          },
          _ => {
            self.readiness().back_readiness.remove(UnixReady::hup());
            return order;
          }
        };
      }

      if front_interest.is_error() || back_interest.is_error() {
        if front_interest.is_error() {
          error!("PROXY client {:?} front error, disconnecting", self.front_token);
        } else {
          error!("PROXY client {:?} back error, disconnecting", self.front_token);
        }

        self.readiness().front_interest = UnixReady::from(Ready::empty());
        self.readiness().back_interest  = UnixReady::from(Ready::empty());
        return ClientResult::CloseClient;
      }

      counter += 1;
    }

    if counter == max_loop_iterations {
      error!("PROXY\thandling client {:?} went through {} iterations, there's a probable infinite loop bug, closing the connection", self.front_token, max_loop_iterations);

      let front_interest = self.readiness().front_interest & self.readiness().front_readiness;
      let back_interest  = self.readiness().back_interest & self.readiness().back_readiness;

      let token = self.front_token.clone();
      error!("PROXY\t{:?} readiness: {:?} | front: {:?} | back: {:?} ", token,
        self.readiness(), front_interest, back_interest);

      return ClientResult::CloseClient;
    }

    ClientResult::Continue
  }
}
