use std::sync::{Arc,Mutex};
use std::rc::{Rc,Weak};
use std::cell::RefCell;
use std::net::Shutdown;
use std::os::unix::io::AsRawFd;
use mio::*;
use mio::net::*;
use mio_uds::UnixStream;
use mio::unix::UnixReady;
use uuid::Uuid;
use std::io::ErrorKind;
use std::collections::{HashMap, HashSet};
use slab::Slab;
use std::net::SocketAddr;
use std::str::from_utf8_unchecked;
use time::{SteadyTime, Duration};
use openssl::ssl::{self, SslContext, SslContextBuilder, SslMethod, SslAlert,
                   Ssl, SslOptions, SslRef, SslStream, SniError, NameType, SslSessionCacheMode,
                   select_next_proto, AlpnError};
use openssl::x509::X509;
use openssl::dh::Dh;
use openssl::pkey::PKey;
use openssl::hash::MessageDigest;
use openssl::nid;
use openssl::error::ErrorStack;
use openssl::ssl::SslVersion;

use sozu_command::scm_socket::ScmSocket;
use sozu_command::proxy::{Cluster,CertificateFingerprint,CertificateAndKey,
  ProxyRequestData,HttpFrontend,HttpsListener,ProxyRequest,ProxyResponse,
  ProxyResponseStatus,TlsVersion,ProxyEvent,Query,QueryCertificateType,
  QueryAnswer,QueryAnswerCertificate,ProxyResponseData,RemoveListener,
  Route};
use sozu_command::logging;
use sozu_command::buffer::fixed::Buffer;

use protocol::http::{parser::{RequestState,RRequestLine,hostname_and_port}, answers::HttpAnswers};
use pool::Pool;
use {ClusterId,Backend,SessionResult,ConnectionError,Protocol,Readiness,SessionMetrics,
  ProxySession,ProxyConfiguration,AcceptError,BackendConnectAction,BackendConnectionStatus,
  CloseResult};
use backends::BackendMap;
use server::{Server,ProxyChannel,ListenToken,ListenPortState,SessionToken,
  ListenSession, CONN_RETRIES, push_event, TIMER};
use socket::server_bind;
use router::{Router, trie::*};
use protocol::{ProtocolResult,Http,Pipe,StickySession};
use protocol::openssl::TlsHandshake;
use protocol::http::{DefaultAnswerStatus, TimeoutStatus};
use protocol::proxy_protocol::expect::ExpectProxyProtocol;
use protocol::h2::Http2;
use retry::RetryPolicy;
use util::UnwrapLog;
use timer::{Timer,Timeout};

//const SERVER_PROTOS: &'static [u8] = b"\x02h2\x08http/1.1";
//const SERVER_PROTOS: &'static [u8] = b"\x08http/1.1\x02h2";
const SERVER_PROTOS: &'static [u8] = b"\x08http/1.1";

#[derive(Debug,Clone,PartialEq,Eq)]
pub struct TlsApp {
  pub cluster_id:           String,
  pub hostname:         String,
  pub path_begin:       String,
}

pub enum State {
  Expect(ExpectProxyProtocol<TcpStream>, Ssl),
  Handshake(TlsHandshake),
  Http(Http<SslStream<TcpStream>>),
  WebSocket(Pipe<SslStream<TcpStream>>),
  Http2(Http2<SslStream<TcpStream>>),
}

pub enum AlpnProtocols {
    H2,
    Http11,
}

pub struct Session {
  frontend_token:     Token,
  backend:            Option<Rc<RefCell<Backend>>>,
  back_connected:     BackendConnectionStatus,
  protocol:           Option<State>,
  public_address:     SocketAddr,
  ssl:                Option<Ssl>,
  pool:               Weak<RefCell<Pool>>,
  sticky_name:        String,
  metrics:            SessionMetrics,
  pub cluster_id:         Option<String>,
  timeout:            Timeout,
  last_event:         SteadyTime,
  pub listen_token:   Token,
  connection_attempt: u8,
  peer_address:       Option<SocketAddr>,
  answers:            Rc<RefCell<HttpAnswers>>,
}

impl Session {
  pub fn new(ssl:Ssl, sock: TcpStream, token: Token, pool: Weak<RefCell<Pool>>,
    public_address: SocketAddr, expect_proxy: bool, sticky_name: String,
    timeout: Timeout, answers: Rc<RefCell<HttpAnswers>>, listen_token: Token,
    delay: Duration) -> Session {

    let peer_address = if expect_proxy {
      // Will be defined later once the expect proxy header has been received and parsed
      None
    } else {
      sock.peer_addr().ok()
    };

    let request_id = Uuid::new_v4().to_hyphenated();
    let protocol = if expect_proxy {
      trace!("starting in expect proxy state");
      gauge_add!("protocol.proxy.expect", 1);
      Some(State::Expect(ExpectProxyProtocol::new(sock, token, request_id), ssl))
    } else {
      gauge_add!("protocol.tls.handshake", 1);
      Some(State::Handshake(TlsHandshake::new(ssl, sock, request_id, peer_address.clone())))
    };

    let metrics = SessionMetrics::new(Some(delay));

    let mut session = Session {
      frontend_token:     token,
      backend:            None,
      back_connected:     BackendConnectionStatus::NotConnected,
      protocol,
      public_address,
      ssl:                None,
      pool,
      sticky_name,
      metrics,
      cluster_id:             None,
      timeout,
      last_event:         SteadyTime::now(),
      listen_token,
      connection_attempt: 0,
      peer_address,
      answers,
    };
    session.front_readiness().interest = UnixReady::from(Ready::readable()) | UnixReady::hup() | UnixReady::error();

    session
  }

  pub fn http(&self) -> Option<&Http<SslStream<TcpStream>>> {
    self.protocol.as_ref().and_then(|protocol| {
      if let &State::Http(ref http) = protocol {
        Some(http)
      } else {
        None
      }
    })
  }

  pub fn http_mut(&mut self) -> Option<&mut Http<SslStream<TcpStream>>> {
    self.protocol.as_mut().and_then(|protocol| {
      if let &mut State::Http(ref mut http) = protocol {
        Some(http)
      } else {
        None
      }
    })
  }

  pub fn set_answer(&mut self, answer: DefaultAnswerStatus, buf: Option<Rc<Vec<u8>>>)  {
    self.protocol.as_mut().map(|protocol| {
      if let &mut State::Http(ref mut http) = protocol {
        http.set_answer(answer, buf);
      }
    });
  }

  pub fn upgrade(&mut self) -> bool {
    let protocol = unwrap_msg!(self.protocol.take());

    if let State::Expect(expect, ssl) = protocol {
      debug!("switching to TLS handshake");
      if let Some(ref addresses) = expect.addresses {
        if let (Some(public_address), Some(session_address)) = (addresses.destination(), addresses.source()) {
          self.public_address = public_address;
          self.peer_address = Some(session_address);

          let ExpectProxyProtocol { frontend, readiness, request_id, .. } = expect;
          let mut tls = TlsHandshake::new(ssl, frontend, request_id, self.peer_address.clone());
          tls.readiness.event = readiness.event;

          gauge_add!("protocol.proxy.expect", -1);
          gauge_add!("protocol.tls.handshake", 1);
          self.protocol = Some(State::Handshake(tls));
          return true;
        }
      }

      error!("failed to upgrade from expect");
      self.protocol = Some(State::Expect(expect, ssl));
      false

    } else if let State::Handshake(handshake) = protocol {
      let pool = self.pool.clone();
      let readiness = handshake.readiness.clone();

      handshake.stream.as_ref().map(|s| {
        let ssl = s.ssl();
        ssl.version2().map(|version| {
          incr!(version_str(version));
        });
        ssl.current_cipher().map(|c| incr!(c.name()));
      });

      let selected_protocol = {
        let s = handshake.stream.as_ref().and_then(|s| s.ssl().selected_alpn_protocol());
        trace!("selected: {}", unsafe { from_utf8_unchecked(s.unwrap_or(&b""[..])) });

        match s {
          Some(b"h2") => {
            AlpnProtocols::H2
          },
          Some(b"http/1.1") | None => {
            AlpnProtocols::Http11
          }
          Some(s) => {
            error!("unknown alpn protocol: {:?}", s);
            return false;
          }
        }
      };

      match selected_protocol {
        AlpnProtocols::H2 => {
          info!("got h2");
          let mut http = Http2::new(unwrap_msg!(handshake.stream), self.frontend_token.clone(), pool,
          Some(self.public_address.clone()), None, self.sticky_name.clone(), Protocol::HTTPS);

          http.frontend.readiness = readiness;
          http.frontend.readiness.interest = UnixReady::from(Ready::readable()) | UnixReady::hup() | UnixReady::error();

          gauge_add!("protocol.tls.handshake", -1);
          gauge_add!("protocol.http2s", 1);

          self.ssl = handshake.ssl;
          self.protocol = Some(State::Http2(http));
          return true;
        },
        AlpnProtocols::Http11 => {
          let http = Http::new(unwrap_msg!(handshake.stream), self.frontend_token.clone(),
            handshake.request_id, pool, self.public_address.clone(), self.peer_address,
            self.sticky_name.clone(), Protocol::HTTPS, self.answers.clone()).map(|mut http| {

            http.front_readiness = readiness;
            http.front_readiness.interest = UnixReady::from(Ready::readable()) | UnixReady::hup() | UnixReady::error();
            State::Http(http)
          });

          if http.is_none() {
            error!("could not upgrade to HTTP");
            //we cannot put back the protocol since we moved the stream
            //self.protocol = Some(State::Handshake(handshake));
            return false;
          }
          gauge_add!("protocol.tls.handshake", -1);
          gauge_add!("protocol.https", 1);

          self.ssl = handshake.ssl;
          self.protocol = http;
          return true;
        }
      }
    } else if let State::Http(http) = protocol {
      debug!("https switching to wss");
      let front_token = self.frontend_token;
      let back_token  = unwrap_msg!(http.back_token());
      let ws_context = http.websocket_context();

      let front_buf = match http.front_buf {
        Some(buf) => buf.buffer,
        None => if let Some(p) = self.pool.upgrade() {
          if let Some(buf) = p.borrow_mut().checkout() {
            buf
          } else {
            return false;
          }
        } else {
          return false;
        }
      };
      let back_buf = match http.back_buf {
        Some(buf) => buf.buffer,
        None => if let Some(p) = self.pool.upgrade() {
          if let Some(buf) = p.borrow_mut().checkout() {
            buf
          } else {
            return false;
          }
        } else {
          return false;
        }
      };

      gauge_add!("protocol.https", -1);
      gauge_add!("http.active_requests", -1);
      gauge_add!("protocol.wss", 1);

      let mut pipe = Pipe::new(http.frontend, front_token, http.request_id, http.cluster_id, http.backend_id,
        Some(ws_context), Some(unwrap_msg!(http.backend)), front_buf, back_buf, http.session_address, Protocol::HTTPS);

      pipe.front_readiness.event = http.front_readiness.event;
      pipe.back_readiness.event  = http.back_readiness.event;
      pipe.set_back_token(back_token);

      self.protocol = Some(State::WebSocket(pipe));
      true
    } else {
      self.protocol = Some(protocol);
      true
    }
  }

  fn front_hup(&mut self)     -> SessionResult {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http)      => http.front_hup(),
      State::WebSocket(ref mut pipe) => pipe.front_hup(&mut self.metrics),
      State::Http2(ref mut http)     => http.front_hup(),
      State::Handshake(_)            => {
        SessionResult::CloseSession
      },
      State::Expect(_,_)             => {
        SessionResult::CloseSession
      }
    }
  }

  fn back_hup(&mut self)      -> SessionResult {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http)      => http.back_hup(),
      State::WebSocket(ref mut pipe) => pipe.back_hup(&mut self.metrics),
      State::Http2(ref mut http)     => http.back_hup(),
      State::Handshake(_)            => {
        error!("why a backend HUP event while still in frontend handshake?");
        SessionResult::CloseSession
      },
      State::Expect(_,_)             => {
        error!("why a backend HUP event while still in frontend proxy protocol expect?");
        SessionResult::CloseSession
      }
    }
  }

  fn log_context(&self)  -> String {
    if let &State::Http(ref http) = unwrap_msg!(self.protocol.as_ref()) {
      http.log_context().to_string()
    } else {
      "".to_string()
    }
  }

  fn readable(&mut self)      -> SessionResult {
    let (upgrade, result) = match *unwrap_msg!(self.protocol.as_mut()) {
      State::Expect(ref mut expect,_)     => expect.readable(&mut self.metrics),
      State::Handshake(ref mut handshake) => handshake.readable(&mut self.metrics),
      State::Http(ref mut http)           => (ProtocolResult::Continue, http.readable(&mut self.metrics)),
      State::Http2(ref mut http)          => (ProtocolResult::Continue, http.readable(&mut self.metrics)),
      State::WebSocket(ref mut pipe)      => (ProtocolResult::Continue, pipe.readable(&mut self.metrics)),
    };

    if upgrade == ProtocolResult::Continue {
      result
    } else {
        if self.upgrade() {
        match *unwrap_msg!(self.protocol.as_mut()) {
          State::Http(ref mut http)  => http.readable(&mut self.metrics),
          State::Http2(ref mut http) => http.readable(&mut self.metrics),
          _ => result
        }
      } else {
        SessionResult::CloseSession
      }
    }
  }

  fn writable(&mut self)      -> SessionResult {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Expect(_,_)             => SessionResult::CloseSession,
      State::Handshake(_)            => SessionResult::CloseSession,
      State::Http(ref mut http)      => http.writable(&mut self.metrics),
      State::Http2(ref mut http)     => http.writable(&mut self.metrics),
      State::WebSocket(ref mut pipe) => pipe.writable(&mut self.metrics),
    }
  }

  fn back_readable(&mut self) -> SessionResult {
    let (upgrade, result) = match *unwrap_msg!(self.protocol.as_mut()) {
      State::Expect(_,_)             => (ProtocolResult::Continue, SessionResult::CloseSession),
      State::Http(ref mut http)      => http.back_readable(&mut self.metrics),
      State::Http2(ref mut http)     => (ProtocolResult::Continue, http.back_readable(&mut self.metrics)),
      State::Handshake(_)            => (ProtocolResult::Continue, SessionResult::CloseSession),
      State::WebSocket(ref mut pipe) => (ProtocolResult::Continue, pipe.back_readable(&mut self.metrics)),
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
        SessionResult::CloseSession
      }
    }
  }

  fn back_writable(&mut self) -> SessionResult {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Expect(_,_)             => SessionResult::CloseSession,
      State::Handshake(_)            => SessionResult::CloseSession,
      State::Http(ref mut http)      => http.back_writable(&mut self.metrics),
      State::Http2(ref mut http)     => http.back_writable(&mut self.metrics),
      State::WebSocket(ref mut pipe) => pipe.back_writable(&mut self.metrics),
    }
  }

  fn front_socket(&self) -> Option<&TcpStream> {
    match unwrap_msg!(self.protocol.as_ref()) {
      &State::Expect(ref expect,_)     => Some(expect.front_socket()),
      &State::Handshake(ref handshake) => handshake.socket(),
      &State::Http(ref http)           => Some(http.front_socket()),
      &State::Http2(ref http)          => Some(http.front_socket()),
      &State::WebSocket(ref pipe)      => Some(pipe.front_socket()),
    }
  }

  fn back_socket(&self)  -> Option<&TcpStream> {
    match unwrap_msg!(self.protocol.as_ref()) {
      &State::Expect(_,_)         => None,
      &State::Handshake(_)        => None,
      &State::Http(ref http)      => http.back_socket(),
      &State::Http2(ref http)     => http.back_socket(),
      &State::WebSocket(ref pipe) => pipe.back_socket(),
    }
  }

  fn back_token(&self)   -> Option<Token> {
    match unwrap_msg!(self.protocol.as_ref()) {
      &State::Expect(_,_)         => None,
      &State::Handshake(_)        => None,
      &State::Http(ref http)      => http.back_token(),
      &State::Http2(ref http)     => http.back_token(),
      &State::WebSocket(ref pipe) => pipe.back_token(),
    }
  }

  fn set_back_socket(&mut self, sock:TcpStream) {
    let backend_address = self.backend.as_ref().map(|b| b.borrow().address).unwrap();
    unwrap_msg!(self.http_mut()).set_back_socket(sock, backend_address)
  }

  fn set_back_token(&mut self, token: Token) {
     match *unwrap_msg!(self.protocol.as_mut()) {
       State::Http(ref mut http)      => http.set_back_token(token),
       State::Http2(ref mut http)     => http.set_back_token(token),
       State::WebSocket(ref mut pipe) => pipe.set_back_token(token),
       _ => {}
     }
  }

  fn back_connected(&self)     -> BackendConnectionStatus {
    self.back_connected
  }

  fn set_back_connected(&mut self, connected: BackendConnectionStatus) {
    self.back_connected = connected;

    if connected == BackendConnectionStatus::Connected {
      gauge_add!("connections", 1, self.cluster_id.as_ref().map(|s| s.as_str()), self.metrics.backend_id.as_ref().map(|s| s.as_str()));

      self.backend.as_ref().map(|backend| {
        let ref mut backend = *backend.borrow_mut();

        if backend.retry_policy.is_down() {
          incr!("up", self.cluster_id.as_ref().map(|s| s.as_str()), self.metrics.backend_id.as_ref().map(|s| s.as_str()));
          info!("backend server {} at {} is up", backend.backend_id, backend.address);
          push_event(ProxyEvent::BackendUp(backend.backend_id.clone(), backend.address));
        }

        backend.failures = 0;
        backend.retry_policy.succeed();
      });
    }
  }

  fn metrics(&mut self)        -> &mut SessionMetrics {
    &mut self.metrics
  }

  fn remove_backend(&mut self) {
    if let Some(backend) = self.backend.take() {
       self.http_mut().map(|h| h.clear_back_token());

       (*backend.borrow_mut()).dec_connections();
    }
  }

  fn front_readiness(&mut self)      -> &mut Readiness {
    let r = match *unwrap_msg!(self.protocol.as_mut()) {
      State::Expect(ref mut expect, _)    => &mut expect.readiness,
      State::Handshake(ref mut handshake) => &mut handshake.readiness,
      State::Http(ref mut http)           => http.front_readiness(),
      State::Http2(ref mut http)          => http.front_readiness(),
      State::WebSocket(ref mut pipe)      => &mut pipe.front_readiness,
    };
    //info!("current readiness: {:?}", r);
    r
  }

  fn back_readiness(&mut self)      -> Option<&mut Readiness> {
    let r = match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http)           => Some(http.back_readiness()),
      State::Http2(ref mut http)          => Some(http.back_readiness()),
      State::WebSocket(ref mut pipe)      => Some(&mut pipe.back_readiness),
      _ => None,
    };
    //info!("current readiness: {:?}", r);
    r
  }

  fn fail_backend_connection(&mut self) {
    self.backend.as_ref().map(|backend| {
      let ref mut backend = *backend.borrow_mut();
      backend.failures += 1;

      let already_unavailable = backend.retry_policy.is_down();
      backend.retry_policy.fail();
      incr!("connections.error", self.cluster_id.as_ref().map(|s| s.as_str()), self.metrics.backend_id.as_ref().map(|s| s.as_str()));
      if !already_unavailable && backend.retry_policy.is_down() {
        error!("backend server {} at {} is down", backend.backend_id, backend.address);
        incr!("down", self.cluster_id.as_ref().map(|s| s.as_str()), self.metrics.backend_id.as_ref().map(|s| s.as_str()));

        push_event(ProxyEvent::BackendDown(backend.backend_id.clone(), backend.address));
      }
    });
  }

  fn reset_connection_attempt(&mut self) {
    self.connection_attempt = 0;
  }
}

impl ProxySession for Session {

  fn close(&mut self, poll: &mut Poll) -> CloseResult {
    //println!("TLS closing[{:?}] temp->front: {:?}, temp->back: {:?}", self.frontend_token, *self.temp.front_buf, *self.temp.back_buf);
    self.http_mut().map(|http| http.close());
    self.metrics.service_stop();
    if let Some(front_socket) = self.front_socket() {
      if let Err(e) = front_socket.shutdown(Shutdown::Both) {
        if e.kind() != ErrorKind::NotConnected {
          error!("error closing front socket({:?}): {:?}", front_socket, e);
        }
      }
      if let Err(e) = poll.deregister(front_socket) {
        error!("error deregistering front socket({:?}): {:?}", front_socket, e);
      }
    }

    let mut result = CloseResult::default();

    if let Some(tk) = self.back_token() {
      result.tokens.push(tk)
    }

    //FIXME: should we really pass a token here?
    self.close_backend(Token(0), poll);

    if let Some(State::Http(ref http)) = self.protocol {
      //if the state was initial, the connection was already reset
      if http.request != Some(RequestState::Initial) {
        gauge_add!("http.active_requests", -1);
      }
    }

    match self.protocol {
      Some(State::Expect(_,_)) => gauge_add!("protocol.proxy.expect", -1),
      Some(State::Handshake(_)) => gauge_add!("protocol.tls.handshake", -1),
      Some(State::Http(_)) => gauge_add!("protocol.https", -1),
      Some(State::Http2(_)) => gauge_add!("protocol.http2s", -1),
      Some(State::WebSocket(_)) => gauge_add!("protocol.wss", -1),
      None => {}
    }

    result.tokens.push(self.frontend_token);

    result
  }

  fn timeout(&mut self, token: Token, front_timeout: &Duration) -> SessionResult {
    if self.frontend_token == token {
      let dur = SteadyTime::now() - self.last_event;
      if dur < *front_timeout {
        TIMER.with(|timer| {
            timer.borrow_mut().set_timeout((*front_timeout - dur).to_std().unwrap(), token);
        });
        SessionResult::Continue
      } else {
        match self.http().map(|h| h.timeout_status()) {
          Some(TimeoutStatus::Request) => {
            self.set_answer(DefaultAnswerStatus::Answer408, None);
            self.writable()
          },
          Some(TimeoutStatus::Response) => {
            self.set_answer(DefaultAnswerStatus::Answer504, None);
            self.writable()
          },
          _ => {
            SessionResult::CloseSession
          }
        }
      }
    } else {
      // invalid token, obsolete timeout triggered
      SessionResult::Continue
    }
  }

  fn cancel_timeouts(&self) {
      TIMER.with(|timer| {
          timer.borrow_mut().cancel_timeout(&self.timeout);
      });
  }

  //FIXME: check the token passed as argument
  fn close_backend(&mut self, _: Token, poll: &mut Poll) {
    self.remove_backend();

    let back_connected = self.back_connected();
    if back_connected != BackendConnectionStatus::NotConnected {
       self.back_readiness().map(|r| r.event = UnixReady::from(Ready::empty()));
      if let Some(sock) = self.back_socket() {
        if let Err(e) = sock.shutdown(Shutdown::Both) {
          if e.kind() != ErrorKind::NotConnected {
            error!("error closing back socket({:?}): {:?}", sock, e);
          }
        }
        if let Err(e) = poll.deregister(sock) {
          error!("error deregistering back socket({:?}): {:?}", sock, e);
        }
      }
    }

    if back_connected == BackendConnectionStatus::Connected {
      gauge_add!("connections", -1, self.cluster_id.as_ref().map(|s| s.as_str()), self.metrics.backend_id.as_ref().map(|s| s.as_str()));
    }

    self.set_back_connected(BackendConnectionStatus::NotConnected);

    self.http_mut().map(|h| {
      h.clear_back_token();
      h.remove_backend();
    });
  }

  fn protocol(&self) -> Protocol {
    Protocol::HTTPS
  }

  fn process_events(&mut self, token: Token, events: Ready) {
    trace!("token {:?} got event {}", token, super::unix_ready_to_string(UnixReady::from(events)));
    self.last_event = SteadyTime::now();
    self.metrics.wait_start();

    if self.frontend_token == token {
      self.front_readiness().event = self.front_readiness().event | UnixReady::from(events);
    } else if self.back_token() == Some(token) {
      self.back_readiness().map(|r| r.event = r.event | UnixReady::from(events));
    }
  }

  fn ready(&mut self) -> SessionResult {
    let mut counter = 0;
    let max_loop_iterations = 100000;

    self.metrics().service_start();

    if self.back_connected() == BackendConnectionStatus::Connecting &&
      self.back_readiness().map(|r| r.event != UnixReady::from(Ready::empty())).unwrap_or(false) {

      if self.back_readiness().map(|r| r.event.is_hup()).unwrap_or(false) ||
        !self.http_mut().map(|h| h.test_back_socket()).unwrap_or(false) {

        //retry connecting the backend
        error!("{} error connecting to backend, trying again", self.log_context());
        self.metrics().service_stop();
        self.connection_attempt += 1;
        self.fail_backend_connection();

        let backend_token = self.back_token();
        return SessionResult::ReconnectBackend(Some(self.frontend_token), backend_token);
      } else {
        self.metrics().backend_connected();
        self.reset_connection_attempt();
        self.set_back_connected(BackendConnectionStatus::Connected);
      }
    }

    if self.front_readiness().event.is_hup() {
      let order = self.front_hup();
      match order {
        SessionResult::CloseSession => {
          return order;
        },
        _ => {
          self.front_readiness().event.remove(UnixReady::hup());
          return order;
        }
      }
    }

    let token = self.frontend_token.clone();
    while counter < max_loop_iterations {
      let front_interest = self.front_readiness().interest & self.front_readiness().event;
      let back_interest  = self.back_readiness().map(|r| r.interest & r.event).unwrap_or(UnixReady::from(Ready::empty()));

      trace!("PROXY\t{} {:?} {:?} -> {:?}", self.log_context(), token, self.front_readiness().clone(), self.back_readiness());

      if front_interest == UnixReady::from(Ready::empty()) && back_interest == UnixReady::from(Ready::empty()) {
        break;
      }

      if self.back_readiness().map(|r| r.event.is_hup()).unwrap_or(false) && self.front_readiness().interest.is_writable() &&
        ! self.front_readiness().event.is_writable() {
        break;
      }

      if front_interest.is_readable() {
        let order = self.readable();
        trace!("front readable\tinterpreting session order {:?}", order);

        if order != SessionResult::Continue {
          return order;
        }
      }

      if back_interest.is_writable() {
        let order = self.back_writable();
        if order != SessionResult::Continue {
          return order;
        }
      }

      if back_interest.is_readable() {
        let order = self.back_readable();
        if order != SessionResult::Continue {
          return order;
        }
      }

      if front_interest.is_writable() {
        let order = self.writable();
        trace!("front writable\tinterpreting session order {:?}", order);
        if order != SessionResult::Continue {
          return order;
        }
      }

      if back_interest.is_hup() {
        let order = self.back_hup();
        match order {
          SessionResult::CloseSession => {
            return order;
          },
          SessionResult::Continue => {
            /*self.front_readiness().interest.insert(Ready::writable());
            if ! self.front_readiness().event.is_writable() {
              error!("got back socket HUP but there's still data, and front is not writable yet(openssl)[{:?} => {:?}]", self.frontend_token, self.back_token());
              break;
            }*/
          },
          _ => {
            self.back_readiness().map(|r| r.event.remove(UnixReady::hup()));
            return order;
          }
        };
      }

      if front_interest.is_error() || back_interest.is_error() {
        if front_interest.is_error() {
          error!("PROXY session {:?} front error, disconnecting", self.frontend_token);
        } else {
          error!("PROXY session {:?} back error, disconnecting", self.frontend_token);
        }

        self.front_readiness().interest = UnixReady::from(Ready::empty());
        self.back_readiness().map(|r| r.interest  = UnixReady::from(Ready::empty()));
        return SessionResult::CloseSession;
      }

      counter += 1;
    }

    if counter == max_loop_iterations {
      error!("PROXY\thandling session {:?} went through {} iterations, there's a probable infinite loop bug, closing the connection", self.frontend_token, max_loop_iterations);
      incr!("https_openssl.infinite_loop.error");

      let front_interest = self.front_readiness().interest & self.front_readiness().event;
      let back_interest  = self.back_readiness().map(|r| r.interest & r.event);

      let token = self.frontend_token.clone();
      let back = self.back_readiness().cloned();
      error!("PROXY\t{:?} readiness: {:?} -> {:?} | front: {:?} | back: {:?} ", token,
        self.front_readiness(), back, front_interest, back_interest);
      self.print_state();

      return SessionResult::CloseSession;
    }

    SessionResult::Continue
  }

  fn shutting_down(&mut self) -> SessionResult {
    match &mut self.protocol {
      Some(State::Http(h)) => h.shutting_down(),
      Some(State::Handshake(_)) => SessionResult::Continue,
      _    => SessionResult::CloseSession,
    }
  }

  fn last_event(&self) -> SteadyTime {
    self.last_event
  }

  fn print_state(&self) {
    let p:String = match &self.protocol {
      Some(State::Expect(_,_))  => String::from("Expect"),
      Some(State::Handshake(_)) => String::from("Handshake"),
      Some(State::Http(h))      => h.print_state("HTTPS"),
      Some(State::Http2(h))     => format!("HTTP2S: {:?}", h.state),
      Some(State::WebSocket(_)) => String::from("WSS"),
      None                      => String::from("None"),
    };

    let rf = match *unwrap_msg!(self.protocol.as_ref()) {
      State::Expect(ref expect, _)    => &expect.readiness,
      State::Handshake(ref handshake) => &handshake.readiness,
      State::Http(ref http)           => &http.front_readiness,
      State::Http2(ref http)          => &http.frontend.readiness,
      State::WebSocket(ref pipe)      => &pipe.front_readiness,
    };
    let rb = match *unwrap_msg!(self.protocol.as_ref()) {
      State::Http(ref http)           => Some(&http.back_readiness),
      State::Http2(ref http)          => Some(&http.back_readiness),
      State::WebSocket(ref pipe)      => Some(&pipe.back_readiness),
      _ => None,
    };

    error!("zombie session[{:?} => {:?}], state => readiness: {:?} -> {:?}, protocol: {}, cluster_id: {:?}, back_connected: {:?}, metrics: {:?}",
      self.frontend_token, self.back_token(), rf, rb, p, self.cluster_id, self.back_connected, self.metrics);
  }

  fn tokens(&self) -> Vec<Token> {
    let mut v = vec![self.frontend_token];
    if let Some(tk) = self.back_token() {
      v.push(tk)
    }

    v
  }
}

fn get_cert_common_name(cert: &X509) -> Option<String> {
    cert.subject_name().entries_by_nid(nid::Nid::COMMONNAME).next().and_then(|name| name.data().as_utf8().ok().map(|name| (&*name).to_string()))
}

fn get_cert_names(cert: &X509) -> HashSet<String> {
  let mut  h = HashSet::new();

  // warning: we should not use the common name anymore
  if let Some(name) = get_cert_common_name(&cert) {
    h.insert(name);
  }

  if let Some(names) = cert.subject_alt_names() {
    for name in names.iter().filter_map(|general_name|
        general_name.dnsname().map(|name| String::from(name))
      ) {
      h.insert(name);
    }
  }

  h
}


pub type HostName  = String;
pub type PathBegin = String;
pub struct TlsData {
  context:     SslContext,
  certificate: Vec<u8>,
}

pub struct Listener {
  listener:        Option<TcpListener>,
  address:         SocketAddr,
  fronts:          Router,
  domains:         Arc<Mutex<TrieNode<CertificateFingerprint>>>,
  contexts:        Arc<Mutex<HashMap<CertificateFingerprint,TlsData>>>,
  default_context: SslContext,
  answers:         Rc<RefCell<HttpAnswers>>,
  config:          HttpsListener,
  ssl_options:     SslOptions,
  pub token:       Token,
  active:          bool,
}

impl Listener {
  pub fn new(config: HttpsListener, token: Token) -> Listener {

    let contexts:HashMap<CertificateFingerprint,TlsData> = HashMap::new();
    let domains      = TrieNode::root();
    let fronts       = Router::new();
    let rc_ctx       = Arc::new(Mutex::new(contexts));
    let ref_ctx      = rc_ctx.clone();
    let rc_domains   = Arc::new(Mutex::new(domains));
    let ref_domains  = rc_domains.clone();

    let (default_context, ssl_options):(SslContext, SslOptions) =
      Self::create_default_context(&config, ref_ctx, ref_domains).expect("could not create default context");

    Listener {
      listener:        None,
      address:         config.address.clone(),
      domains:         rc_domains,
      default_context: default_context,
      contexts:        rc_ctx,
      answers:         Rc::new(RefCell::new(HttpAnswers::new(&config.answer_404, &config.answer_503))),
      active:          false,
      fronts,
      config,
      ssl_options,
      token,
    }
  }

  pub fn activate(&mut self, event_loop: &mut Poll, tcp_listener: Option<TcpListener>) -> Option<Token> {
    if self.active {
      return Some(self.token);
    }

    let listener = tcp_listener.or_else(|| server_bind(&self.config.address).map_err(|e| {
      error!("could not create listener {:?}: {:?}", self.config.address, e);
    }).ok());


    if let Some(ref sock) = listener {
      if let Err(e) = event_loop.register(sock, self.token, Ready::readable(), PollOpt::edge()) {
        error!("cannot register listen socket({:?}): {:?}", sock, e);
      }
    } else {
      return None;
    }

    self.listener = listener;
    self.active = true;
    Some(self.token)
  }

  pub fn create_default_context(config: &HttpsListener, ref_ctx: Arc<Mutex<HashMap<CertificateFingerprint,TlsData>>>,
    ref_domains: Arc<Mutex<TrieNode<CertificateFingerprint>>>) -> Option<(SslContext, SslOptions)> {
    let ctx = SslContext::builder(SslMethod::tls());
    if let Err(e) = ctx {
      error!("error building SSL context: {:?}", e);
      return None;
    }

    let mut context = ctx.expect("should have built a correct SSL context");

    let mut mode = ssl::SslMode::ENABLE_PARTIAL_WRITE;
    mode.insert(ssl::SslMode::ACCEPT_MOVING_WRITE_BUFFER);
    //FIXME: maybe activate ssl::SslMode::RELEASE_BUFFERS to save some memory?
    context.set_mode(mode);


    let mut ssl_options = ssl::SslOptions::CIPHER_SERVER_PREFERENCE | ssl::SslOptions::NO_COMPRESSION | ssl::SslOptions::NO_TICKET;
    let mut versions = ssl::SslOptions::NO_SSLV2 | ssl::SslOptions::NO_SSLV3 |
      ssl::SslOptions::NO_TLSV1 | ssl::SslOptions::NO_TLSV1_1 |
      ssl::SslOptions::NO_TLSV1_2
      //FIXME: the value will not be available if openssl-sys does not find an OpenSSL v1.1
      /*| ssl::SslOptions::NO_TLSV1_3*/;

    for version in config.versions.iter() {
      match version {
        TlsVersion::SSLv2   => versions.remove(ssl::SslOptions::NO_SSLV2),
        TlsVersion::SSLv3   => versions.remove(ssl::SslOptions::NO_SSLV3),
        TlsVersion::TLSv1_0 => versions.remove(ssl::SslOptions::NO_TLSV1),
        TlsVersion::TLSv1_1 => versions.remove(ssl::SslOptions::NO_TLSV1_1),
        TlsVersion::TLSv1_2 => versions.remove(ssl::SslOptions::NO_TLSV1_2),
        //TlsVersion::TLSv1_3 => versions.remove(ssl::SslOptions::NO_TLSV1_3),
        s         => error!("unrecognized TLS version: {:?}", s)
      };
    }

    ssl_options.insert(versions);
    trace!("parsed tls options: {:?}", ssl_options);

    context.set_options(ssl_options);
    context.set_session_cache_size(1);
    context.set_session_cache_mode(SslSessionCacheMode::OFF);

    if let Err(e) = setup_curves(&mut context) {
      error!("could not setup curves for openssl: {:?}", e);
    }

    if let Err(e) = setup_dh(&mut context) {
      error!("could not setup DH for openssl: {:?}", e);
    }

    if let Err(e) = context.set_cipher_list(&config.cipher_list) {
      error!("could not set context cipher list: {:?}", e);
    }

    if let Err(e) = context.set_alpn_protos(SERVER_PROTOS) {
      error!("could not set ALPN protocols: {:?}", e);
    }

    context.set_alpn_select_callback(move |_ssl: &mut SslRef, client_protocols: &[u8]| {
      debug!("got protocols list from client: {:?}", unsafe { from_utf8_unchecked(client_protocols) });
      match select_next_proto(SERVER_PROTOS, client_protocols) {
        None => Err(AlpnError::ALERT_FATAL),
        Some(selected) => {
          debug!("selected protocol with ALPN: {:?}", unsafe { from_utf8_unchecked(selected) });
          Ok(selected)
        }
      }
    });

    let mut cert_read = &include_bytes!("../assets/certificate.pem")[..];
    let mut key_read  = &include_bytes!("../assets/key.pem")[..];

    if let (Ok(cert), Ok(key)) = (X509::from_pem(&mut cert_read), PKey::private_key_from_pem(&mut key_read)) {
      if let Err(e) = context.set_certificate(&cert) {
        error!("error adding certificate to context: {:?}", e);
      }
      if let Err(e) = context.set_private_key(&key) {
        error!("error adding private key to context: {:?}", e);
      }
    }

    context.set_servername_callback(move |ssl: &mut SslRef, alert: &mut SslAlert| {
      let contexts = unwrap_msg!(ref_ctx.lock());
      let domains  = unwrap_msg!(ref_domains.lock());

      trace!("ref: {:?}", ssl);
      if let Some(servername) = ssl.servername(NameType::HOST_NAME).map(|s| s.to_string()) {
        debug!("looking for fingerprint for {:?}", servername);
        if let Some(kv) = domains.domain_lookup(servername.as_bytes(), true) {
          debug!("looking for context for {:?} with fingerprint {:?}", servername, kv.1);
          if let Some(ref tls_data) = contexts.get(&kv.1) {
            debug!("found context for {:?}", servername);

            let context: &SslContext = &tls_data.context;
            if let Ok(()) = ssl.set_ssl_context(context) {
              debug!("servername is now {:?}", ssl.servername(NameType::HOST_NAME));

              return Ok(());
            } else {
              error!("could not set context for {:?}", servername);
            }
          } else {
            error!("no context found for {:?}", servername);
          }
        } else {
          //error!("unrecognized server name: {}", servername);
        }
      } else {
        //error!("no server name information found");
      }

      incr!("openssl.sni.error");
      *alert = SslAlert::UNRECOGNIZED_NAME;
      return Err(SniError::ALERT_FATAL);
    });

    Some((context.build(), ssl_options))
  }

  pub fn add_https_front(&mut self, tls_front: HttpFrontend) -> bool {
    self.fronts.add_http_front(tls_front)
  }

  pub fn remove_https_front(&mut self, tls_front: HttpFrontend) -> bool {
    debug!("removing tls_front {:?}", tls_front);
    self.fronts.remove_http_front(tls_front)
  }

  pub fn add_certificate(&mut self, certificate_and_key: CertificateAndKey) -> bool {
    //FIXME: insert some error management with a Result here
    let c = SslContext::builder(SslMethod::tls());
    if c.is_err() { return false; }
    let mut ctx = c.expect("should have built a correct SSL context");
    ctx.set_options(self.ssl_options);
    ctx.set_session_cache_size(1);
    ctx.set_session_cache_mode(SslSessionCacheMode::OFF);

    if let Err(e) = setup_curves(&mut ctx) {
      error!("could not setup curves for openssl: {:?}", e);
    }

    if let Err(e) = setup_dh(&mut ctx) {
      error!("could not setup DH for openssl: {:?}", e);
    }

    if let Err(e) = ctx.set_cipher_list(&self.config.cipher_list) {
      error!("cannot set context cipher list: {:?}", e);
    }

    if let Err(e) = ctx.set_alpn_protos(SERVER_PROTOS) {
      error!("could not set ALPN protocols: {:?}", e);
    }

    ctx.set_alpn_select_callback(move |_ssl: &mut SslRef, client_protocols: &[u8]| {
      debug!("got protocols list from client: {:?}", unsafe { from_utf8_unchecked(client_protocols) });
      match select_next_proto(SERVER_PROTOS, client_protocols) {
        None => Err(AlpnError::ALERT_FATAL),
        Some(selected) => {
          debug!("selected protocol with ALPN: {:?}", unsafe { from_utf8_unchecked(selected) });
          Ok(selected)
        }
      }
    });

    let mut cert_read  = &certificate_and_key.certificate.as_bytes()[..];
    let mut key_read   = &certificate_and_key.key.as_bytes()[..];
    let cert_chain: Vec<X509> = certificate_and_key.certificate_chain.iter().filter_map(|c| {
      X509::from_pem(c.as_bytes()).ok()
    }).collect();

    if let (Ok(cert), Ok(key)) = (X509::from_pem(&mut cert_read), PKey::private_key_from_pem(&mut key_read)) {
      //FIXME: would need more logs here

      //FIXME
      let fingerprint = CertificateFingerprint(unwrap_msg!(cert.digest(MessageDigest::sha256()).map(|d| d.to_vec())));
      let names = get_cert_names(&cert);
      debug!("got certificate names: {:?}", names);

      if let Err(e) = ctx.set_certificate(&cert) {
        error!("error adding certificate to context: {:?}", e);
      }
      if let Err(e) = ctx.set_private_key(&key) {
        error!("error adding private key to context: {:?}", e);
      }

      for cert in cert_chain {
        if let Err(e) = ctx.add_extra_chain_cert(cert) {
          error!("error adding chain certificate to context: {:?}", e);
        }
      }

      let ref_ctx = self.contexts.clone();
      let ref_domains = self.domains.clone();
      ctx.set_servername_callback(move |ssl: &mut SslRef, alert: &mut SslAlert| {
        let contexts = unwrap_msg!(ref_ctx.lock());
        let domains  = unwrap_msg!(ref_domains.lock());

        trace!("ref: {:?}", ssl);
        if let Some(servername) = ssl.servername(NameType::HOST_NAME).map(|s| s.to_string()) {
          debug!("looking for fingerprint for {:?}", servername);
          if let Some(kv) = domains.domain_lookup(servername.as_bytes(), true) {
            debug!("looking for context for {:?} with fingerprint {:?}", servername, kv.1);
            if let Some(ref tls_data) = contexts.get(&kv.1) {
              debug!("found context for {:?}", servername);

              let context: &SslContext = &tls_data.context;
              if let Ok(()) = ssl.set_ssl_context(context) {
                debug!("servername is now {:?}", ssl.servername(NameType::HOST_NAME));

                return Ok(());
              } else {
                error!("could not set context for {:?}", servername);
              }
            } else {
              error!("no context found for {:?}", servername);
            }
          } else {
            //error!("unrecognized server name: {}", servername);
          }
        } else {
          //error!("no server name information found");
        }

        incr!("openssl.sni.error");
        *alert = SslAlert::UNRECOGNIZED_NAME;
        return Err(SniError::ALERT_FATAL);
      });

      let tls_data = TlsData {
        context:     ctx.build(),
        certificate: cert_read.to_vec(),
      };

      // if the name or the fingerprint are already used,
      // those insertions should fail, because it would be
      // from the same certificate
      // Add a refcount?
      //FIXME: this is blocking
      //this lock is only obtained from this thread, so is it alright?
      {
        let mut contexts = unwrap_msg!(self.contexts.lock());
        if !contexts.contains_key(&fingerprint) {
          contexts.insert(fingerprint.clone(), tls_data);
        }
      }
      {
        let mut domains = unwrap_msg!(self.domains.lock());

        for name in names {
          domains.domain_insert(name.into_bytes(), fingerprint.clone());
        }
      }

      true
    } else {
      false
    }
  }

  // FIXME: return an error if the cert is still in use
  pub fn remove_certificate(&mut self, fingerprint: CertificateFingerprint) {
    debug!("removing certificate {:?}", fingerprint);
    let mut contexts = unwrap_msg!(self.contexts.lock());
    let mut domains  = unwrap_msg!(self.domains.lock());

    if let Some(data) = contexts.remove(&fingerprint) {
      if let Ok(cert) = X509::from_pem(&data.certificate) {
        let names = get_cert_names(&cert);

        for name in names {
          domains.domain_remove(&name.into_bytes());
        }
      }
    }
  }

  // ToDo factor out with http.rs
  pub fn frontend_from_request(&self, host: &str, uri: &str) -> Option<Route> {
    let host: &str = if let Ok((i, (hostname, _))) = hostname_and_port(host.as_bytes()) {
      if i != &b""[..] {
        error!("frontend_from_request: invalid remaining chars after hostname. Host: {}", host);
        return None;
      }

      // it is alright to call from_utf8_unchecked,
      // we already verified that there are only ascii
      // chars in there
      unsafe { from_utf8_unchecked(hostname) }
    } else {
      error!("hostname parsing failed for: '{}'", host);
      return None;
    };

    self.fronts.lookup(host.as_bytes(), uri.as_bytes())
  }

  fn accept(&mut self) -> Result<TcpStream, AcceptError> {
    if let Some(ref sock) = self.listener {
      sock.accept().map_err(|e| {
        match e.kind() {
          ErrorKind::WouldBlock => {
            AcceptError::WouldBlock
          },
          _ => {
            error!("accept() IO error: {:?}", e);
            AcceptError::IoError
          }
        }
      }).map(|(sock,_)| {
        sock
      })
    } else {
      error!("cannot accept connections, no listening socket available");
      Err(AcceptError::IoError)
    }
  }
}

pub struct Proxy {
  listeners:      HashMap<Token, Listener>,
  clusters:       HashMap<ClusterId, Cluster>,
  backends:       Rc<RefCell<BackendMap>>,
  pool:           Rc<RefCell<Pool>>,
}

impl Proxy {
  pub fn new(pool: Rc<RefCell<Pool>>, backends: Rc<RefCell<BackendMap>>) -> Proxy {
    Proxy {
      listeners : HashMap::new(),
      clusters: HashMap::new(),
      backends,
      pool,
    }
  }

  pub fn add_listener(&mut self, config: HttpsListener, token: Token) -> Option<Token> {
    if self.listeners.contains_key(&token) {
      None
    } else {
      let listener = Listener::new(config, token);
      self.listeners.insert(listener.token.clone(), listener);
      Some(token)
    }
  }

  pub fn remove_listener(&mut self, address: SocketAddr) -> bool {
    let len = self.listeners.len();
    self.listeners.retain(|_, l| l.address != address);

    self.listeners.len() < len
  }

  pub fn activate_listener(&mut self, event_loop: &mut Poll, addr: &SocketAddr, tcp_listener: Option<TcpListener>) -> Option<Token> {
    for listener in self.listeners.values_mut() {
      if &listener.address == addr {
        return listener.activate(event_loop, tcp_listener);
      }
    }
    None
  }

  pub fn give_back_listeners(&mut self) -> Vec<(SocketAddr, TcpListener)> {
    self.listeners.values_mut().filter(|l| l.listener.is_some()).map(|l| {
      (l.address.clone(), l.listener.take().unwrap())
    }).collect()
  }

  pub fn give_back_listener(&mut self, address: SocketAddr) -> Option<(Token, TcpListener)> {
    self.listeners.values_mut().find(|l| l.address == address).and_then(|l| {
      l.listener.take().map(|sock| (l.token, sock))
    })
  }

  pub fn add_cluster(&mut self, mut cluster: Cluster) {
    if let Some(answer_503) = cluster.answer_503.take() {
      for l in self.listeners.values_mut() {
        l.answers.borrow_mut().add_custom_answer(&cluster.cluster_id, &answer_503);
      }
    }

    self.clusters.insert(cluster.cluster_id.clone(), cluster);
  }

  pub fn remove_cluster(&mut self, cluster_id: &str) {
    self.clusters.remove(cluster_id);
    for l in self.listeners.values_mut() {
      l.answers.borrow_mut().remove_custom_answer(cluster_id);
    }
  }

  pub fn backend_from_request(&mut self, session: &mut Session, cluster_id: &str,
  front_should_stick: bool) -> Result<TcpStream,ConnectionError> {
    session.http_mut().map(|h| h.set_cluster_id(String::from(cluster_id)));

    let sticky_session = session.http().and_then(|http| http.request.as_ref())
      .and_then(|r| r.get_sticky_session());

    let res = match (front_should_stick, sticky_session) {
      (true, Some(sticky_session)) => {
        self.backends.borrow_mut().backend_from_sticky_session(cluster_id, &sticky_session)
          .map_err(|e| {
            debug!("Couldn't find a backend corresponding to sticky_session {} for app {}", sticky_session, cluster_id);
            e
          })
      },
      _ => self.backends.borrow_mut().backend_from_cluster_id(cluster_id),
    };

    match res {
      Err(e) => {
        session.set_answer(DefaultAnswerStatus::Answer503, None);
        Err(e)
      },
      Ok((backend, conn))  => {
        if front_should_stick {
          let sticky_name = self.listeners[&session.listen_token].config.sticky_name.clone();
          session.http_mut().map(|http| {
            http.sticky_session =
              Some(StickySession::new(backend.borrow().sticky_id.clone().unwrap_or(backend.borrow().backend_id.clone())));
            http.sticky_name = sticky_name;
          });
        }
        session.metrics.backend_id = Some(backend.borrow().backend_id.clone());
        session.metrics.backend_start();
        session.http_mut().map(|http| {
          http.set_backend_id(backend.borrow().backend_id.clone());
        });
        session.backend = Some(backend);

        Ok(conn)
      }
    }
  }

  fn cluster_id_from_request(&mut self,  session: &mut Session) -> Result<String, ConnectionError> {
    let h = session.http().and_then(|h| h.request.as_ref())
      .and_then(|s| s.get_host()).ok_or(ConnectionError::NoHostGiven)?;

    let host: &str = if let Ok((i, (hostname, port))) = hostname_and_port(h.as_bytes()) {
      if i != &b""[..] {
        error!("connect_to_backend: invalid remaining chars after hostname. Host: {}", h);
        session.set_answer(DefaultAnswerStatus::Answer400, None);
        return Err(ConnectionError::InvalidHost);
      }

      // it is alright to call from_utf8_unchecked,
      // we already verified that there are only ascii
      // chars in there
      let hostname_str =  unsafe { from_utf8_unchecked(hostname) };

      //FIXME: what if we don't use SNI?
      let servername: Option<String> = session.http()
        .and_then(|h| h.frontend.ssl().servername(NameType::HOST_NAME)).map(|s| s.to_string());

      if servername.as_ref().map(|s| s.as_str()) != Some(hostname_str) {
        error!("TLS SNI hostname '{:?}' and Host header '{}' don't match", servername, hostname_str);
        /*FIXME: deactivate this check for a temporary test
        unwrap_msg!(session.http()).set_answer(DefaultAnswerStatus::Answer404, None);
        return Err(ConnectionError::HostNotFound);
        */
      }

      //FIXME: we should check that the port is right too

      if port == Some(&b"443"[..]) {
        hostname_str
      } else {
        &h
      }
    } else {
      error!("hostname parsing failed");
      session.set_answer(DefaultAnswerStatus::Answer400, None);
      return Err(ConnectionError::InvalidHost);
    };

    let rl:&RRequestLine = session.http().and_then(|h| h.request.as_ref()).and_then(|r| r.get_request_line())
      .ok_or(ConnectionError::NoRequestLineGiven)?;
    match self.listeners.get(&session.listen_token).as_ref()
      .and_then(|l| l.frontend_from_request(&host, &rl.uri)) {
      Some(Route::ClusterId(cluster_id)) => Ok(cluster_id),
      Some(Route::Deny) => {
        session.set_answer(DefaultAnswerStatus::Answer401, None);
        Err(ConnectionError::Unauthorized)
      },
      None => {
        session.set_answer(DefaultAnswerStatus::Answer404, None);
        Err(ConnectionError::HostNotFound)
      }
    }
  }

  fn check_circuit_breaker(&mut self, session: &mut Session) -> Result<(), ConnectionError> {
    if session.connection_attempt == CONN_RETRIES {
      error!("{} max connection attempt reached", session.log_context());
      session.set_answer(DefaultAnswerStatus::Answer503, None);
      Err(ConnectionError::NoBackendAvailable)
    } else {
      Ok(())
    }
  }
}

impl ProxyConfiguration<Session> for Proxy {
  fn accept(&mut self, token: ListenToken) -> Result<TcpStream, AcceptError> {
    self.listeners.get_mut(&Token(token.0)).unwrap().accept()
  }

  fn create_session(&mut self, frontend_sock: TcpStream, token: ListenToken, poll: &mut Poll, session_token: Token, timeout: Timeout, delay: Duration)
    -> Result<(Rc<RefCell<Session>>, bool), AcceptError> {
    if let Some(ref listener) = self.listeners.get(&Token(token.0)) {
      if let Err(e) = frontend_sock.set_nodelay(true) {
        error!("error setting nodelay on front socket({:?}): {:?}", frontend_sock, e);
      }
      if let Ok(ssl) = Ssl::new(&listener.default_context) {
        if let Err(e) = poll.register(
          &frontend_sock,
          session_token,
          Ready::readable() | Ready::writable() | Ready::from(UnixReady::hup() | UnixReady::error()),
          PollOpt::edge()
          ) {
          error!("error registering front socket({:?}): {:?}", frontend_sock, e);
        }

        let c = Session::new(ssl, frontend_sock, session_token, Rc::downgrade(&self.pool),
          listener.config.public_address.unwrap_or(listener.config.address),
          listener.config.expect_proxy, listener.config.sticky_name.clone(),
          timeout, listener.answers.clone(), Token(token.0), delay);

        Ok((Rc::new(RefCell::new(c)), false))
      } else {
        error!("could not create ssl context");
        Err(AcceptError::IoError)
      }
    } else {
      Err(AcceptError::IoError)
    }
  }

  fn connect_to_backend(&mut self, poll: &mut Poll,  session: &mut Session, back_token: Token) -> Result<BackendConnectAction,ConnectionError> {
    let old_cluster_id = session.http().and_then(|ref http| http.cluster_id.clone());
    let old_back_token = session.back_token();

    self.check_circuit_breaker(session)?;

    let cluster_id = self.cluster_id_from_request(session)?;

    if (session.http().and_then(|h| h.cluster_id.as_ref()) == Some(&cluster_id)) && session.back_connected == BackendConnectionStatus::Connected {
      let has_backend = session.backend.as_ref().map(|backend| {
         let ref backend = *backend.borrow();
         self.backends.borrow().has_backend(&cluster_id, backend)
        }).unwrap_or(false);

      let is_valid_backend_socket = has_backend &&
        session.http_mut().map(|h| h.is_valid_backend_socket()).unwrap_or(false);

      if is_valid_backend_socket {
        //matched on keepalive
        session.metrics.backend_id = session.backend.as_ref().map(|i| i.borrow().backend_id.clone());
        session.metrics.backend_start();
        return Ok(BackendConnectAction::Reuse);
      } else {
        if let Some(token) = session.back_token() {
          session.close_backend(token, poll);

          //reset the back token here so we can remove it
          //from the slab after backend_from* fails
          session.set_back_token(token);
        }
      }
    }

    //replacing with a connection to another application
    if old_cluster_id.is_some() && old_cluster_id.as_ref() != Some(&cluster_id) {
      if let Some(token) = session.back_token() {
        session.close_backend(token, poll);

        //reset the back token here so we can remove it
        //from the slab after backend_from* fails
        session.set_back_token(token);
      }
    }

    session.cluster_id = Some(cluster_id.clone());

    let front_should_stick = self.clusters.get(&cluster_id).map(|ref app| app.sticky_session).unwrap_or(false);
    let socket = self.backend_from_request(session, &cluster_id, front_should_stick)?;

    session.http_mut().map(|http| {
      http.cluster_id = Some(cluster_id.clone());
    });

    if let Err(e) = socket.set_nodelay(true) {
      error!("error setting nodelay on back socket({:?}): {:?}", socket, e);
    }
    session.back_readiness().map(|r| {
      r.interest = UnixReady::from(Ready::writable()) | UnixReady::hup() | UnixReady::error();
    });


    session.back_connected = BackendConnectionStatus::Connecting;
    if let Some(back_token) = old_back_token {
      session.set_back_token(back_token);
      if let Err(e) = poll.register(
        &socket,
        back_token,
        Ready::readable() | Ready::writable() | Ready::from(UnixReady::hup() | UnixReady::error()),
        PollOpt::edge()
      ) {
        error!("error registering back socket({:?}): {:?}", socket, e);
      }

      session.set_back_socket(socket);
      Ok(BackendConnectAction::Replace)
    } else {
      if let Err(e) = poll.register(
        &socket,
        back_token,
        Ready::readable() | Ready::writable() | Ready::from(UnixReady::hup() | UnixReady::error()),
        PollOpt::edge()
      ) {
        error!("error registering back socket({:?}): {:?}", socket, e);
      }

      session.set_back_socket(socket);
      session.set_back_token(back_token);
      Ok(BackendConnectAction::New)
    }
  }

  fn notify(&mut self, event_loop: &mut Poll, message: ProxyRequest) -> ProxyResponse {
    //trace!("{} notified", message);
    match message.order {
      ProxyRequestData::AddCluster(cluster) => {
        debug!("{} add cluster {:?}", message.id, cluster);
        self.add_cluster(cluster);
        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
      },
      ProxyRequestData::RemoveCluster { cluster_id } => {
        debug!("{} remove cluster {:?}", message.id, cluster_id);
        self.remove_cluster(&cluster_id);
        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
      },
      ProxyRequestData::AddHttpsFrontend(front) => {
        //info!("HTTPS\t{} add front {:?}", id, front);
        if let Some(listener) = self.listeners.values_mut().find(|l| l.address == front.address) {
          listener.add_https_front(front);
          ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
        } else {
          panic!("adding front {:?} to unknown listener", front);
        }
      },
      ProxyRequestData::RemoveHttpsFrontend(front) => {
        //info!("HTTPS\t{} remove front {:?}", id, front);
        if let Some(listener) = self.listeners.values_mut().find(|l| l.address == front.address) {
          listener.remove_https_front(front);
          ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
        } else {
          panic!();
        }
      },
      ProxyRequestData::AddCertificate(add_certificate) => {
        if let Some(listener) = self.listeners.values_mut().find(|l| l.address == add_certificate.address) {
          //info!("HTTPS\t{} add certificate: {:?}", id, certificate_and_key);
          listener.add_certificate(add_certificate.certificate);
          ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
        } else {
          error!("adding certificate to unknown listener");
          ProxyResponse{ id: message.id, status: ProxyResponseStatus::Error(String::from("unsupported message")), data: None }
        }
      },
      ProxyRequestData::RemoveCertificate(remove_certificate) => {
        if let Some(listener) = self.listeners.values_mut().find(|l| l.address == remove_certificate.address) {
          //info!("TLS\t{} remove certificate with fingerprint {:?}", id, fingerprint);
          listener.remove_certificate(remove_certificate.fingerprint);
          //FIXME: should return an error if certificate still has fronts referencing it
          ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
        } else {
          panic!();
        }
      },
      ProxyRequestData::ReplaceCertificate(replace) => {
        if let Some(listener) = self.listeners.values_mut().find(|l| l.address == replace.address) {
          //info!("TLS\t{} replace certificate of fingerprint {:?} with {:?}", id,
          //  replace.old_fingerprint, replace.new_certificate);
          listener.remove_certificate(replace.old_fingerprint);
          listener.add_certificate(replace.new_certificate);
          //FIXME: should return an error if certificate still has fronts referencing it
          ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
        } else {
          panic!();
        }
      },
      ProxyRequestData::RemoveListener(remove) => {
        debug!("removing HTTPS listener at address {:?}", remove.address);
        if !self.remove_listener(remove.address) {
          ProxyResponse {
              id: message.id,
              status: ProxyResponseStatus::Error(format!("no HTTPS listener to remove at address {:?}", remove.address)),
              data: None
          }
        } else {
          ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
        }
      },
      ProxyRequestData::SoftStop => {
        info!("{} processing soft shutdown", message.id);
        for (_, l) in self.listeners.iter_mut() {
          l.listener.take().map(|sock| {
            if let Err(e) = event_loop.deregister(&sock) {
              error!("error deregistering listen socket({:?}): {:?}", sock, e);
            }
          });
        }
        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Processing, data: None }
      },
      ProxyRequestData::HardStop => {
        info!("{} hard shutdown", message.id);
        for (_, mut l) in self.listeners.drain(){
          l.listener.take().map(|sock| {
            if let Err(e) = event_loop.deregister(&sock) {
              error!("error dereginstering listen socket({:?}): {:?}", sock, e);
            }
          });
        }
        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Processing, data: None }
      },
      ProxyRequestData::Status => {
        debug!("{} status", message.id);
        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
      },
      ProxyRequestData::Logging(logging_filter) => {
        debug!("{} changing logging filter to {}", message.id, logging_filter);
        logging::LOGGER.with(|l| {
          let directives = logging::parse_logging_spec(&logging_filter);
          l.borrow_mut().set_directives(directives);
        });
        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
      },
      ProxyRequestData::Query(Query::Certificates(QueryCertificateType::All)) => {
        let res = self.listeners.iter().map(|(_addr, listener)| {
          let mut domains = unwrap_msg!(listener.domains.lock()).to_hashmap();
          let res = domains.drain().map(|(k, v)| {
            (String::from_utf8(k).unwrap(), v.0.clone())
          }).collect();

          (listener.address, res)
        }).collect::<HashMap<_,_>>();

        info!("got Certificates::All query, answering with {:?}", res);

        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok,
          data: Some(ProxyResponseData::Query(QueryAnswer::Certificates(QueryAnswerCertificate::All(res)))) }
      },
      ProxyRequestData::Query(Query::Certificates(QueryCertificateType::Domain(d))) => {
        let res = self.listeners.iter().map(|(_addr, listener)| {
          let domains  = unwrap_msg!(listener.domains.lock());
          (listener.address, domains.domain_lookup(d.as_bytes(), true).map(|(k, v)| {
            (String::from_utf8(k.to_vec()).unwrap(), v.0.clone())
          }))
        }).collect::<HashMap<_,_>>();

        info!("got Certificates::Domain({}) query, answering with {:?}", d, res);

        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok,
          data: Some(ProxyResponseData::Query(QueryAnswer::Certificates(QueryAnswerCertificate::Domain(res)))) }
      }
      command => {
        error!("{} unsupported message for OpenSSL proxy, ignoring {:?}", message.id, command);
        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Error(String::from("unsupported message")), data: None }
      }
    }
  }

  fn listen_port_state(&self, port: &u16) -> ListenPortState {
    fixme!();
    ListenPortState::Available
    //if port == &self.address.port() { ListenPortState::InUse } else { ListenPortState::Available }
  }
}


#[cfg(ossl101)]
pub fn setup_curves(ctx: &mut SslContextBuilder) -> Result<(), ErrorStack> {
  use openssl::ec::EcKey;
  use openssl::nid;

  let curve = EcKey::from_curve_name(nid::X9_62_PRIME256V1)?;
  ctx.set_tmp_ecdh(&curve)
}

#[cfg(ossl102)]
fn setup_curves(ctx: &mut SslContextBuilder) -> Result<(), ErrorStack> {
  match Dh::get_2048_256() {
    Ok(dh) => ctx.set_tmp_dh(&dh),
    Err(e) => {
      return Err(e)
    }
  };
  ctx.set_ecdh_auto(true)
}

#[cfg(ossl110)]
fn setup_curves(ctx: &mut SslContextBuilder) -> Result<(), ErrorStack> {
  use openssl::ec::EcKey;

  let curve = EcKey::from_curve_name(nid::Nid::X9_62_PRIME256V1)?;
  ctx.set_tmp_ecdh(&curve)
}

#[cfg(all(not(ossl101), not(ossl102), not(ossl110)))]
fn setup_curves(_: &mut SslContextBuilder) -> Result<(), ErrorStack> {
  compile_error!("unsupported openssl version, please open an issue");
  Ok(())
}

fn setup_dh(ctx: &mut SslContextBuilder) -> Result<(), ErrorStack> {
  let dh = Dh::params_from_pem(
b"
-----BEGIN DH PARAMETERS-----
MIIBCAKCAQEAm7EQIiluUX20jnC3NGhikNVGH7MSlRIgTpkUlCyWrmlJci+Pu54Q
YzOZw3tw4g84+ipTzpdGuUvZxNd4Y4ZUp/XsdZgnWMfV+y9OuyYAKWGPTtEO/HUy
6buhvi5qahIXdxUm0dReFuOTrDOphNjHXggU8Iwey3U54aL+KVqLMAP+Tev8jrQN
A0mv4X8dmdvCv7LEZPVYFJb3Uprwd60ThSl3NKNTMDnwnRtXpIrSZIZcJh5IER7S
/IexUuBsopcTVIcHfLyaECIbMKnZfyrkAJfHqRWEpPOzk1euVw0hw3SkDJREfB21
yD0TrUjkXyjV/zczIYiYSROg9OE5UgYqswIBAg==
-----END DH PARAMETERS-----
",
      )?;
  ctx.set_tmp_dh(&dh)
}

use server::HttpsProvider;
pub fn start(config: HttpsListener, channel: ProxyChannel, max_buffers: usize, buffer_size: usize) {
  use server::{self,ProxySessionCast};

  let mut event_loop  = Poll::new().expect("could not create event loop");

  let pool = Rc::new(RefCell::new(
    Pool::with_capacity(2*max_buffers, buffer_size)
  ));
  let backends = Rc::new(RefCell::new(BackendMap::new()));

  let mut sessions: Slab<Rc<RefCell<dyn ProxySessionCast>>> = Slab::with_capacity(max_buffers);
  {
    let entry = sessions.vacant_entry();
    info!("taking token {:?} for channel", SessionToken(entry.key()));
    entry.insert(Rc::new(RefCell::new(ListenSession { protocol: Protocol::HTTPListen })));
  }
  {
    let entry = sessions.vacant_entry();
    info!("taking token {:?} for timer", SessionToken(entry.key()));
    entry.insert(Rc::new(RefCell::new(ListenSession { protocol: Protocol::HTTPListen })));
  }
  {
    let entry = sessions.vacant_entry();
    info!("taking token {:?} for metrics", SessionToken(entry.key()));
    entry.insert(Rc::new(RefCell::new(ListenSession { protocol: Protocol::HTTPListen })));
  }

  let token = {
    let entry = sessions.vacant_entry();
    let key = entry.key();
    let e = entry.insert(Rc::new(RefCell::new(ListenSession { protocol: Protocol::HTTPListen })));
    Token(key)
  };

  let mut configuration = Proxy::new(pool.clone(), backends.clone());
  let address = config.address.clone();
  if configuration.add_listener(config, token).is_some() {
    if configuration.activate_listener(&mut event_loop, &address, None).is_some() {

      let (scm_server, _scm_client) = UnixStream::pair().unwrap();
      let mut server_config: server::ServerConfig = Default::default();
      server_config.max_connections = max_buffers;
      let mut server  = Server::new(event_loop, channel, ScmSocket::new(scm_server.as_raw_fd()),
        sessions, pool, backends, None, Some(HttpsProvider::Openssl(configuration)), None, server_config, None);

      info!("starting event loop");
      server.run();
      info!("ending event loop");
    }
  }
}

#[cfg(test)]
mod tests {
  extern crate tiny_http;
  use super::*;
  use std::collections::HashMap;
  use std::net::SocketAddr;
  use std::str::FromStr;
  use std::rc::Rc;
  use std::sync::{Arc,Mutex};
  use router::{trie::TrieNode,Router,PathRule};
  use sozu_command::proxy::Route;
  use openssl::ssl::{SslContext, SslMethod};

  /*
  #[test]
  #[cfg(target_pointer_width = "64")]
  fn size_test() {
    assert_size!(ExpectProxyProtocol<mio::net::TcpStream>, 520);
    assert_size!(TlsHandshake, 216);
    assert_size!(Http<SslStream<mio::net::TcpStream>>, 1016);
    assert_size!(Pipe<SslStream<mio::net::TcpStream>>, 224);
    assert_size!(State, 1024);
    // fails depending on the platform?
    //assert_size!(Session, 1320);

    assert_size!(SslStream<mio::net::TcpStream>, 16);
    assert_size!(Ssl, 8);
  }
  */

  #[test]
  fn frontend_from_request_test() {
    let cluster_id1 = "app_1".to_owned();
    let cluster_id2 = "app_2".to_owned();
    let cluster_id3 = "app_3".to_owned();
    let uri1 = "/".to_owned();
    let uri2 = "/yolo".to_owned();
    let uri3 = "/yolo/swag".to_owned();

    let mut fronts = Router::new();
    assert!(fronts.add_tree_rule("lolcatho.st".as_bytes(), PathRule::Prefix(uri1), Route::ClusterId(cluster_id1.clone())));
    assert!(fronts.add_tree_rule("lolcatho.st".as_bytes(), PathRule::Prefix(uri2), Route::ClusterId(cluster_id2)));
    assert!(fronts.add_tree_rule("lolcatho.st".as_bytes(), PathRule::Prefix(uri3), Route::ClusterId(cluster_id3)));
    assert!(fronts.add_tree_rule("other.domain".as_bytes(), PathRule::Prefix("test".to_string()), Route::ClusterId(cluster_id1)));

    let contexts   = HashMap::new();
    let rc_ctx     = Arc::new(Mutex::new(contexts));
    let domains    = TrieNode::root();
    let rc_domains = Arc::new(Mutex::new(domains));

    let context    = SslContext::builder(SslMethod::tls()).expect("could not create a SslContextBuilder");

    let front: SocketAddr = FromStr::from_str("127.0.0.1:1032").expect("test address 127.0.0.1:1032 should be parsed");
    let listener = Listener {
      listener:  None,
      address:   front,
      fronts:    fronts,
      domains:   rc_domains,
      default_context: context.build(),
      contexts: rc_ctx,
      answers:   Rc::new(RefCell::new(HttpAnswers::new("HTTP/1.1 404 Not Found\r\n\r\n", "HTTP/1.1 503 your application is in deployment\r\n\r\n"))),
      config: Default::default(),
      ssl_options: ssl::SslOptions::CIPHER_SERVER_PREFERENCE | ssl::SslOptions::NO_COMPRESSION | ssl::SslOptions::NO_TICKET |
        ssl::SslOptions::NO_SSLV2 | ssl::SslOptions::NO_SSLV3 | ssl::SslOptions::NO_TLSV1 | ssl::SslOptions::NO_TLSV1_1,
      token: Token(0),
      active: true,
    };


    println!("TEST {}", line!());
    let frontend1 = listener.frontend_from_request("lolcatho.st", "/");
    assert_eq!(frontend1.expect("should find a frontend"), Route::ClusterId("app_1".to_string()));
    println!("TEST {}", line!());
    let frontend2 = listener.frontend_from_request("lolcatho.st", "/test");
    assert_eq!(frontend2.expect("should find a frontend"), Route::ClusterId("app_1".to_string()));
    println!("TEST {}", line!());
    let frontend3 = listener.frontend_from_request("lolcatho.st", "/yolo/test");
    assert_eq!(frontend3.expect("should find a frontend"), Route::ClusterId("app_2".to_string()));
    println!("TEST {}", line!());
    let frontend4 = listener.frontend_from_request("lolcatho.st", "/yolo/swag");
    assert_eq!(frontend4.expect("should find a frontend"), Route::ClusterId("app_3".to_string()));
    println!("TEST {}", line!());
    let frontend5 = listener.frontend_from_request("domain", "/");
    assert_eq!(frontend5, None);
   // assert!(false);
  }

  #[test]
  fn wildcard_certificate_names() {
    let data = include_bytes!("../assets/services.crt");
    let cert = X509::from_pem(data).unwrap();

    let names = get_cert_names(&cert);

    assert_eq!(names, ["services.clever-cloud.com", "*.services.clever-cloud.com"].iter().map(|s| String::from(*s)).collect());

    println!("got names: {:?}", names);

    let mut trie = TrieNode::root();

    trie.domain_insert("*.services.clever-cloud.com".as_bytes().to_vec(), 1u8);
    trie.domain_insert("*.clever-cloud.com".as_bytes().to_vec(), 2u8);
    trie.domain_insert("services.clever-cloud.com".as_bytes().to_vec(), 0u8);
    trie.domain_insert("abprefix.services.clever-cloud.com".as_bytes().to_vec(), 3u8);
    trie.domain_insert("cdprefix.services.clever-cloud.com".as_bytes().to_vec(), 4u8);

    let res = trie.domain_lookup(b"test.services.clever-cloud.com", true);
    println!("query result: {:?}", res);

    assert_eq!(
      trie.domain_lookup(b"pgstudio.services.clever-cloud.com", true),
      Some(&("*.services.clever-cloud.com".as_bytes().to_vec(), 1u8)));
    assert_eq!(
      trie.domain_lookup(b"test-prefix.services.clever-cloud.com", true),
      Some(&("*.services.clever-cloud.com".as_bytes().to_vec(), 1u8)));
  }

  #[test]
  fn wildcard_with_subdomains() {

    let mut trie = TrieNode::root();

    trie.domain_insert("*.test.example.com".as_bytes().to_vec(), 1u8);
    trie.domain_insert("hello.sub.test.example.com".as_bytes().to_vec(), 2u8);

    let res = trie.domain_lookup(b"sub.test.example.com", true);
    println!("query result: {:?}", res);

    assert_eq!(
      trie.domain_lookup(b"sub.test.example.com", true),
      Some(&("*.test.example.com".as_bytes().to_vec(), 1u8)));
    assert_eq!(
      trie.domain_lookup(b"hello.sub.test.example.com", true),
      Some(&("hello.sub.test.example.com".as_bytes().to_vec(), 2u8)));

    // now try in a different order
    let mut trie = TrieNode::root();

    trie.domain_insert("hello.sub.test.example.com".as_bytes().to_vec(), 2u8);
    trie.domain_insert("*.test.example.com".as_bytes().to_vec(), 1u8);

    let res = trie.domain_lookup(b"sub.test.example.com", true);
    println!("query result: {:?}", res);

    assert_eq!(
      trie.domain_lookup(b"sub.test.example.com", true),
      Some(&("*.test.example.com".as_bytes().to_vec(), 1u8)));
    assert_eq!(
      trie.domain_lookup(b"hello.sub.test.example.com", true),
      Some(&("hello.sub.test.example.com".as_bytes().to_vec(), 2u8)));
  }
}

fn version_str(version: SslVersion) -> &'static str {
  match version {
    SslVersion::SSL3 => "tls.version.SSLv3",
    SslVersion::TLS1 => "tls.version.TLSv1_0",
    SslVersion::TLS1_1 => "tls.version.TLSv1_1",
    SslVersion::TLS1_2 => "tls.version.TLSv1_2",
    //SslVersion::TLS1_3 => "tls.version.TLSv1_3",
    _ => "tls.version.TLSv1_3",
    //_ => "tls.version.Unknown",
  }
}
