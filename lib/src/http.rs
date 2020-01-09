use std::collections::HashMap;
use std::io::ErrorKind;
use std::rc::{Rc,Weak};
use std::cell::RefCell;
use std::os::unix::io::IntoRawFd;
use std::net::{SocketAddr,Shutdown};
use std::str::from_utf8_unchecked;
use mio::*;
use mio::net::*;
use mio_uds::UnixStream;
use mio::unix::UnixReady;
use uuid::Uuid;
use time::{SteadyTime,Duration};
use slab::Slab;
use mio_extras::timer::{Timer, Timeout};

use sozu_command::scm_socket::{Listeners,ScmSocket};
use sozu_command::proxy::{Application,ProxyRequestData,HttpFront,HttpListener,
  ProxyRequest,ProxyResponse,ProxyResponseStatus,ProxyEvent};
use sozu_command::logging;
use sozu_command::buffer::fixed::Buffer;

use super::{AppId,Backend,SessionResult,ConnectionError,Protocol,Readiness,SessionMetrics,
  ProxySession,ProxyConfiguration,AcceptError,BackendConnectAction,BackendConnectionStatus,
  CloseResult};
use super::backends::BackendMap;
use super::pool::Pool;
use super::protocol::{ProtocolResult,StickySession,Http,Pipe};
use super::protocol::http::{DefaultAnswerStatus, TimeoutStatus, answers::{DefaultAnswers, CustomAnswers, HttpAnswers}};
use super::protocol::proxy_protocol::expect::ExpectProxyProtocol;
use super::server::{Server,ProxyChannel,ListenToken,ListenPortState,SessionToken,
  ListenSession, CONN_RETRIES, push_event};
use super::socket::server_bind;
use super::retry::RetryPolicy;
use super::protocol::http::parser::{hostname_and_port, RequestState};
use router::Router;
use util::UnwrapLog;

#[derive(PartialEq)]
pub enum SessionStatus {
  Normal,
  DefaultAnswer,
}

pub enum State {
  Expect(ExpectProxyProtocol<TcpStream>),
  Http(Http<TcpStream>),
  WebSocket(Pipe<TcpStream>)
}

pub struct Session {
  frontend_token:     Token,
  backend:            Option<Rc<RefCell<Backend>>>,
  back_connected:     BackendConnectionStatus,
  protocol:           Option<State>,
  pool:               Weak<RefCell<Pool<Buffer>>>,
  metrics:            SessionMetrics,
  pub app_id:         Option<String>,
  sticky_name:        String,
  front_timeout:      Timeout,
  last_event:         SteadyTime,
  pub listen_token:   Token,
  connection_attempt: u8,
  answers:            Rc<RefCell<HttpAnswers>>,
}

impl Session {
  pub fn new(sock: TcpStream, token: Token, pool: Weak<RefCell<Pool<Buffer>>>,
    public_address: SocketAddr, expect_proxy: bool, sticky_name: String, timeout: Timeout,
    answers: Rc<RefCell<HttpAnswers>>, listen_token: Token, delay: Duration) -> Option<Session> {
    let request_id = Uuid::new_v4().to_hyphenated();
    let protocol = if expect_proxy {
      trace!("starting in expect proxy state");
      gauge_add!("protocol.proxy.expect", 1);
      Some(State::Expect(ExpectProxyProtocol::new(sock, token, request_id)))
    } else {
      gauge_add!("protocol.http", 1);
      let session_address = sock.peer_addr().ok();
      Http::new(sock, token, request_id, pool.clone(), public_address,
        session_address, sticky_name.clone(), Protocol::HTTP).map(|http| State::Http(http))
    };

    let metrics = SessionMetrics::new(Some(delay));

    protocol.map(|pr| {
      let mut session = Session {
        backend:            None,
        back_connected:     BackendConnectionStatus::NotConnected,
        protocol:           Some(pr),
        frontend_token:     token,
        pool,
        metrics,
        app_id:             None,
        sticky_name,
        front_timeout:      timeout,
        last_event:         SteadyTime::now(),
        listen_token,
        connection_attempt: 0,
        answers,
      };

      session.front_readiness().interest = UnixReady::from(Ready::readable()) | UnixReady::hup() | UnixReady::error();

      session
    })
  }

  pub fn upgrade(&mut self) -> bool {
    debug!("HTTP::upgrade");
    let protocol = unwrap_msg!(self.protocol.take());
    if let State::Http(http) = protocol {
      debug!("switching to pipe");
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

      gauge_add!("protocol.http", -1);
      gauge_add!("protocol.ws", 1);
      gauge_add!("http.active_requests", -1);
      let mut pipe = Pipe::new(http.frontend, front_token, http.request_id,
        http.app_id, http.backend_id, Some(ws_context),
        Some(unwrap_msg!(http.backend)), front_buf, back_buf, http.session_address, Protocol::HTTP);

      pipe.front_readiness.event = http.front_readiness.event;
      pipe.back_readiness.event  = http.back_readiness.event;
      pipe.set_back_token(back_token);
      //pipe.set_app_id(self.app_id.clone());

      self.protocol = Some(State::WebSocket(pipe));
      true
    } else if let State::Expect(expect) = protocol {
      debug!("switching to HTTP");
      let readiness = expect.readiness;
      if let Some((Some(public_address), Some(client_address))) = expect.addresses.as_ref().map(|add| {
        (add.destination(), add.source())
      }) {
        let http = Http::new(expect.frontend, expect.frontend_token, expect.request_id,
          self.pool.clone(), public_address, Some(client_address),
          self.sticky_name.clone(), Protocol::HTTP).map(|mut http| {
            http.front_readiness.event = readiness.event;

            State::Http(http)
        });

        if http.is_none() {
          //we cannot put back the protocol since we moved the stream
          //self.protocol = Some(State::Expect(expect));
          return false;
        }

        gauge_add!("protocol.proxy.expect", -1);
        gauge_add!("protocol.http", 1);
        self.protocol = http;
        return true;
      }

      //we cannot put back the protocol since we moved the stream
      //self.protocol = Some(State::Expect(expect));
      false
    } else {
      self.protocol = Some(protocol);
      true
    }
  }

  pub fn set_answer(&mut self, answer: DefaultAnswerStatus, buf: Rc<Vec<u8>>)  {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http) => http.set_answer(answer, buf),
      _ => {}
    }
  }

  pub fn http(&self) -> Option<&Http<TcpStream>> {
    self.protocol.as_ref().and_then(|protocol| match protocol {
      State::Http(ref http) => Some(http),
      _ => None
    })
  }

  pub fn http_mut(&mut self) -> Option<&mut Http<TcpStream>> {
    self.protocol.as_mut().and_then(|protocol| match protocol {
      State::Http(ref mut http) => Some(http),
      _ => None
    })
  }

  fn front_hup(&mut self) -> SessionResult {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http)      => http.front_hup(),
      State::WebSocket(ref mut pipe) => pipe.front_hup(&mut self.metrics),
      _                              => SessionResult::CloseSession,
    }
  }

  fn back_hup(&mut self) -> SessionResult {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http)      => http.back_hup(),
      State::WebSocket(ref mut pipe) => pipe.back_hup(&mut self.metrics),
      _                              => SessionResult::CloseSession,
    }
  }

  fn log_context(&self) -> String {
    match *unwrap_msg!(self.protocol.as_ref()) {
      State::Http(ref http) => {
        if let Some(ref app_id) = http.app_id {
          format!("{}\t{}\t", http.request_id, app_id)
        } else {
          format!("{}\t-\t", http.request_id)
        }

      },
      _ => "".to_string()
    }
  }

  // Read content from the frontend
  fn readable(&mut self) -> SessionResult {
    let (upgrade, result) = match *unwrap_msg!(self.protocol.as_mut()) {
      State::Expect(ref mut expect)  => expect.readable(&mut self.metrics),
      State::Http(ref mut http)      => (ProtocolResult::Continue, http.readable(&mut self.metrics)),
      State::WebSocket(ref mut pipe) => (ProtocolResult::Continue, pipe.readable(&mut self.metrics)),
    };

    if upgrade == ProtocolResult::Continue {
      result
    } else if self.upgrade() {
      match *unwrap_msg!(self.protocol.as_mut()) {
        State::Http(ref mut http) => http.readable(&mut self.metrics),
        _ => result
      }
    } else {
      error!("failed protocol upgrade");
      SessionResult::CloseSession
    }
  }

  // Forward content to the frontend
  fn writable(&mut self) -> SessionResult {
    match  *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http)      => http.writable(&mut self.metrics),
      State::WebSocket(ref mut pipe) => pipe.writable(&mut self.metrics),
      State::Expect(_)               => SessionResult::CloseSession,
    }
  }

  // Forward content to application
  fn back_writable(&mut self) -> SessionResult {
    match *unwrap_msg!(self.protocol.as_mut())  {
      State::Http(ref mut http)      => http.back_writable(&mut self.metrics),
      State::WebSocket(ref mut pipe) => pipe.back_writable(&mut self.metrics),
      State::Expect(_)               => SessionResult::CloseSession,
    }
  }

  // Read content from application
  fn back_readable(&mut self) -> SessionResult {
    let (upgrade, result) = match  *unwrap_msg!(self.protocol.as_mut())  {
      State::Http(ref mut http)      => http.back_readable(&mut self.metrics),
      State::WebSocket(ref mut pipe) => (ProtocolResult::Continue, pipe.back_readable(&mut self.metrics)),
      State::Expect(_)               => return SessionResult::CloseSession,
    };

    if upgrade == ProtocolResult::Continue {
      result
    } else if self.upgrade() {
      match *unwrap_msg!(self.protocol.as_mut()) {
        State::WebSocket(ref mut pipe) => pipe.back_readable(&mut self.metrics),
        _ => result
      }
    } else {
      error!("failed protocol upgrade");
      SessionResult::CloseSession
    }
  }

  fn front_socket(&self) -> &TcpStream {
    match *unwrap_msg!(self.protocol.as_ref()) {
      State::Http(ref http)      => http.front_socket(),
      State::WebSocket(ref pipe) => pipe.front_socket(),
      State::Expect(ref expect)  => expect.front_socket(),
    }
  }

  fn back_socket(&self)  -> Option<&TcpStream> {
    match *unwrap_msg!(self.protocol.as_ref()) {
      State::Http(ref http)      => http.back_socket(),
      State::WebSocket(ref pipe) => pipe.back_socket(),
      State::Expect(_)           => None,
    }
  }

  fn back_token(&self)   -> Option<Token> {
    match *unwrap_msg!(self.protocol.as_ref()) {
      State::Http(ref http)      => http.back_token(),
      State::WebSocket(ref pipe) => pipe.back_token(),
      State::Expect(_)           => None,
    }
  }

  fn set_back_socket(&mut self, socket: TcpStream) {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http) => http.set_back_socket(socket, self.backend.as_ref().map(|b| b.borrow().address).unwrap()),
      // not passing it here since we should already have a connection available
      State::WebSocket(_)       => {},
      State::Expect(_)          => {},
    }
  }

  fn set_back_token(&mut self, token: Token) {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http)      => http.set_back_token(token),
      State::WebSocket(ref mut pipe) => pipe.set_back_token(token),
      State::Expect(_)               => {},
    }
  }

  fn back_connected(&self)     -> BackendConnectionStatus {
    self.back_connected
  }

  fn set_back_connected(&mut self, connected: BackendConnectionStatus) {
    self.back_connected = connected;
    if connected == BackendConnectionStatus::Connected {
      gauge_add!("connections", 1, self.app_id.as_ref().map(|s| s.as_str()), self.metrics.backend_id.as_ref().map(|s| s.as_str()));
      self.backend.as_ref().map(|backend| {
        let ref mut backend = *backend.borrow_mut();
        //successful connection, rest failure counter
        backend.failures = 0;
        backend.retry_policy.succeed();
      });
    }
  }

  fn metrics(&mut self)        -> &mut SessionMetrics {
    &mut self.metrics
  }

  fn remove_backend(&mut self) {
    /*debug!("{}\tPROXY [{} -> {}] CLOSED BACKEND",
      self.http().map(|h| h.log_ctx.clone()).unwrap_or_else(|| "".to_string()), self.frontend_token.0,
      self.back_token().map(|t| format!("{}", t.0)).unwrap_or_else(|| "-".to_string()));
    */

    if let Some(backend) = self.backend.take() {
      self.http_mut().map(|h| h.clear_back_token());

      (*backend.borrow_mut()).dec_connections();
    }
  }

  fn front_readiness(&mut self) -> &mut Readiness {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http)      => &mut http.front_readiness,
      State::WebSocket(ref mut pipe) => &mut pipe.front_readiness,
      State::Expect(ref mut expect)  => &mut expect.readiness,
    }
  }

  fn back_readiness(&mut self) -> Option<&mut Readiness> {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http)      => Some(&mut http.back_readiness),
      State::WebSocket(ref mut pipe) => Some(&mut pipe.back_readiness),
      _ => None,
    }
  }

  fn fail_backend_connection(&mut self) {
    self.backend.as_ref().map(|backend| {
      let ref mut backend = *backend.borrow_mut();
      backend.failures += 1;

      let already_unavailable = backend.retry_policy.is_down();
      backend.retry_policy.fail();
      incr!("connections.error", self.app_id.as_ref().map(|s| s.as_str()), self.metrics.backend_id.as_ref().map(|s| s.as_str()));
      if !already_unavailable && backend.retry_policy.is_down() {
        error!("backend server {} at {} is down", backend.backend_id, backend.address);
        incr!("down", self.app_id.as_ref().map(|s| s.as_str()), self.metrics.backend_id.as_ref().map(|s| s.as_str()));

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
    self.metrics.service_stop();
    if let Err(e) = self.front_socket().shutdown(Shutdown::Both) {
      if e.kind() != ErrorKind::NotConnected {
        error!("error shutting down front socket({:?}): {:?}", self.front_socket(), e);
      }
    }

    if let Err(e) = poll.deregister(self.front_socket()) {
      error!("error deregistering front socket({:?}): {:?}", self.front_socket(), e);
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
      Some(State::Expect(_)) => gauge_add!("protocol.proxy.expect", -1),
      Some(State::Http(_)) => gauge_add!("protocol.http", -1),
      Some(State::WebSocket(_)) => gauge_add!("protocol.ws", -1),
      None => {},
    }

    result.tokens.push(self.frontend_token);

    result
  }

  fn timeout(&mut self, token: Token, timer: &mut Timer<Token>, front_timeout: &Duration) -> SessionResult {
    if self.frontend_token == token {
      let dur = SteadyTime::now() - self.last_event;
      if dur < *front_timeout {
        timer.set_timeout((*front_timeout - dur).to_std().unwrap(), token);
        SessionResult::Continue
      } else {
        match self.http().map(|h| h.timeout_status()) {
          Some(TimeoutStatus::Request) => {
            let answer = self.answers.borrow().get(DefaultAnswerStatus::Answer408, None);
            self.set_answer(DefaultAnswerStatus::Answer408, answer);
            self.writable()
          },
          Some(TimeoutStatus::Response) => {
            let answer = self.answers.borrow().get(DefaultAnswerStatus::Answer504, None);
            self.set_answer(DefaultAnswerStatus::Answer504, answer);
            self.writable()
          },
          _ => {
            SessionResult::CloseSession
          }
        }
      }
    } else {
      SessionResult::Continue
    }
  }

  fn cancel_timeouts(&self, timer: &mut Timer<Token>) {
    timer.cancel_timeout(&self.front_timeout);
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
            error!("error shutting down back socket({:?}): {:?}", sock, e);
          }
        }
        if let Err(e) = poll.deregister(sock) {
          error!("error shutting down back socket({:?}): {:?}", sock, e);
        }
      }
    }

    if back_connected == BackendConnectionStatus::Connected {
      gauge_add!("connections", -1, self.app_id.as_ref().map(|s| s.as_str()), self.metrics.backend_id.as_ref().map(|s| s.as_str()));
    }

    self.set_back_connected(BackendConnectionStatus::NotConnected);

    self.http_mut().map(|h| {
      h.clear_back_token();
      h.remove_backend();
    });
  }

  fn protocol(&self) -> Protocol {
    Protocol::HTTP
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

    let token = self.frontend_token;
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
          SessionResult::Continue => {},
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
      incr!("http.infinite_loop.error");

      let front_interest = self.front_readiness().interest & self.front_readiness().event;
      let back_interest  = self.back_readiness().map(|r| r.interest & r.event);

      let token = self.frontend_token;
      let back = self.back_readiness().cloned();
      error!("PROXY\t{:?} readiness: {:?} -> {:?} | front: {:?} | back: {:?} ", token,
        self.front_readiness(), back, front_interest, back_interest);
      self.print_state();

      return SessionResult::CloseSession;
    }

    SessionResult::Continue
  }

  fn shutting_down(&mut self) -> SessionResult {
    match &mut self.protocol {
      Some(State::Http(h)) => h.shutting_down(),
      _    => SessionResult::CloseSession,
    }
  }

  fn last_event(&self) -> SteadyTime {
    self.last_event
  }

  fn print_state(&self) {
    let p:String = match &self.protocol {
      Some(State::Expect(_))    => String::from("Expect"),
      Some(State::Http(h))      => h.print_state("HTTP"),
      Some(State::WebSocket(_)) => String::from("WS"),
      None                      => String::from("None"),
    };

    let rf = match *unwrap_msg!(self.protocol.as_ref()) {
      State::Expect(ref expect)  => &expect.readiness,
      State::Http(ref http)      => &http.front_readiness,
      State::WebSocket(ref pipe) => &pipe.front_readiness,
    };
    let rb = match *unwrap_msg!(self.protocol.as_ref()) {
      State::Http(ref http)      => Some(&http.back_readiness),
      State::WebSocket(ref pipe) => Some(&pipe.back_readiness),
      _ => None,
    };

    error!("zombie session[{:?} => {:?}], state => readiness: {:?} -> {:?}, protocol: {}, app_id: {:?}, back_connected: {:?}, metrics: {:?}",
      self.frontend_token, self.back_token(), rf, rb, p, self.app_id, self.back_connected, self.metrics);
  }

  fn tokens(&self) -> Vec<Token> {
    let mut v = vec![self.frontend_token];
    if let Some(tk) = self.back_token() {
      v.push(tk)
    }

    v
  }
}

pub type Hostname = String;

pub struct Listener {
  listener:       Option<TcpListener>,
  pub address:    SocketAddr,
  fronts:         Router,
  answers:        Rc<RefCell<HttpAnswers>>,
  config:         HttpListener,
  pub token:      Token,
  pub active:     bool,
}

pub struct Proxy {
  listeners:    HashMap<Token,Listener>,
  backends:     Rc<RefCell<BackendMap>>,
  applications: HashMap<AppId, Application>,
  pool:         Rc<RefCell<Pool<Buffer>>>,
}

impl Proxy {
  pub fn new(pool: Rc<RefCell<Pool<Buffer>>>, backends: Rc<RefCell<BackendMap>>) -> Proxy {
    Proxy {
      listeners:      HashMap::new(),
      applications:   HashMap::new(),
      backends,
      pool,
    }
  }

  pub fn add_listener(&mut self, config: HttpListener, token: Token) -> Option<Token> {
    if self.listeners.contains_key(&token) {
      None
    } else {
     let listener = Listener::new(config, token);
      self.listeners.insert(listener.token, listener);
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
      (l.address, l.listener.take().unwrap())
    }).collect()
  }

  pub fn give_back_listener(&mut self, address: SocketAddr) -> Option<(Token, TcpListener)> {
    self.listeners.values_mut().find(|l| l.address == address).and_then(|l| {
      l.listener.take().map(|sock| (l.token, sock))
    })
  }

  pub fn add_application(&mut self, mut application: Application) {
    if let Some(answer_503) = application.answer_503.as_ref() {
      for l in self.listeners.values_mut() {
        l.answers.borrow_mut().add_custom_answer(&application.app_id, &answer_503);
      }
    }

    self.applications.insert(application.app_id.clone(), application);
  }

  pub fn remove_application(&mut self, app_id: &str) {
    self.applications.remove(app_id);

    for l in self.listeners.values_mut() {
      l.answers.borrow_mut().remove_custom_answer(app_id);
    }
  }

  pub fn backend_from_request(&mut self, session: &mut Session, app_id: &str,
  front_should_stick: bool) -> Result<TcpStream,ConnectionError> {
    session.http_mut().map(|h| h.set_app_id(String::from(app_id)));

    let sticky_session = session.http()
      .and_then(|http| http.request.as_ref())
      .and_then(|r| r.get_sticky_session());

    let res = match (front_should_stick, sticky_session) {
      (true, Some(sticky_session)) => {
        self.backends.borrow_mut().backend_from_sticky_session(app_id, &sticky_session)
          .map_err(|e| {
            debug!("Couldn't find a backend corresponding to sticky_session {} for app {}", sticky_session, app_id);
            e
          })
      },
      _ => self.backends.borrow_mut().backend_from_app_id(app_id),
    };

    match res {
      Err(e) => {
        let answer = self.get_service_unavailable_answer(Some(app_id), session.listen_token);
        session.set_answer(DefaultAnswerStatus::Answer503, answer);
        Err(e)
      },
      Ok((backend, conn))  => {
        if front_should_stick {
          let sticky_name =  self.listeners[&session.listen_token].config.sticky_name.clone();
          session.http_mut().map(|http| {
            http.sticky_session =
              Some(StickySession::new(backend.borrow().sticky_id.clone().unwrap_or_else(|| {
                backend.borrow().backend_id.clone()})));
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

  fn app_id_from_request(&mut self, session: &mut Session) -> Result<String, ConnectionError> {
    let h = session.http().and_then(|h| h.request.as_ref())
      .and_then(|s| s.get_host()).ok_or(ConnectionError::NoHostGiven)?;

    let host: &str = if let Ok((i, (hostname, port))) = hostname_and_port(h.as_bytes()) {
      if i != &b""[..] {
        error!("connect_to_backend: invalid remaining chars after hostname. Host: {}", h);
        let answer = self.listeners[&session.listen_token].answers.borrow().get(DefaultAnswerStatus::Answer400, None);
        session.set_answer(DefaultAnswerStatus::Answer400, answer);
        return Err(ConnectionError::InvalidHost);
      }

      //FIXME: we should check that the port is right too

      if port == Some(&b"80"[..]) {
      // it is alright to call from_utf8_unchecked,
      // we already verified that there are only ascii
      // chars in there
        unsafe { from_utf8_unchecked(hostname) }
      } else {
        &h
      }
    } else {
      error!("hostname parsing failed for: '{}'", h);
      let answer = self.listeners[&session.listen_token].answers.borrow().get(DefaultAnswerStatus::Answer400, None);
      session.set_answer(DefaultAnswerStatus::Answer400, answer);
      return Err(ConnectionError::InvalidHost);
    };

    let rl = session.http().and_then(|h| h.request.as_ref())
      .and_then(|s| s.get_request_line()).ok_or(ConnectionError::NoRequestLineGiven)?;

    let app_id = match self.listeners.get(&session.listen_token).as_ref()
      .and_then(|l| l.frontend_from_request(&host, &rl.uri)) {
      Some(app_id) => app_id,
      None => {
        let answer = self.listeners[&session.listen_token].answers.borrow().get(DefaultAnswerStatus::Answer404, None);
        session.set_answer(DefaultAnswerStatus::Answer404, answer);
        return Err(ConnectionError::HostNotFound);
      }
    };

    let front_should_redirect_https = self.applications.get(&app_id).map(|ref app| app.https_redirect).unwrap_or(false);
    if front_should_redirect_https {
      let answer = format!("HTTP/1.1 301 Moved Permanently\r\nContent-Length: 0\r\nLocation: https://{}{}\r\n\r\n", host, rl.uri);
      session.set_answer(DefaultAnswerStatus::Answer301, Rc::new(answer.into_bytes()));
      return Err(ConnectionError::HttpsRedirect);
    }

    Ok(app_id)
  }

  fn check_circuit_breaker(&mut self, session: &mut Session) -> Result<(), ConnectionError> {
    if session.connection_attempt == CONN_RETRIES {
      error!("{} max connection attempt reached", session.log_context());
      let answer = self.get_service_unavailable_answer(session.app_id.as_ref().map(|app_id| app_id.as_str()), session.listen_token);
      session.set_answer(DefaultAnswerStatus::Answer503, answer);
      Err(ConnectionError::NoBackendAvailable)
    } else {
      Ok(())
    }
  }

  fn get_service_unavailable_answer(&self, app_id: Option<&str>, listen_token: Token) -> Rc<Vec<u8>> {
    self.listeners[&listen_token].answers.borrow().get(DefaultAnswerStatus::Answer503, app_id)
  }
}

impl Listener {
  pub fn new(config: HttpListener, token: Token) -> Listener {
    Listener {
      listener: None,
      address: config.front,
      fronts:  Router::new(),
      answers: Rc::new(RefCell::new(HttpAnswers::new(&config.answer_404, &config.answer_503))),
      config,
      token,
      active: false,
    }
  }

  pub fn activate(&mut self, event_loop: &mut Poll, tcp_listener: Option<TcpListener>) -> Option<Token> {
    if self.active {
      return Some(self.token);
    }

    let listener = tcp_listener.or_else(|| server_bind(&self.config.front).map_err(|e| {
      error!("could not create listener {:?}: {:?}", self.config.front, e);
    }).ok());

    if let Some(ref sock) = listener {
      if let Err(e) = event_loop.register(sock, self.token, Ready::readable(), PollOpt::edge()) {
        error!("error registering listener socket({:?}): {:?}", sock, e);
      }
    } else {
      return None;
    }

    self.listener = listener;
    self.active = true;
    Some(self.token)
  }

  pub fn add_http_front(&mut self, mut http_front: HttpFront) -> Result<(), String> {
    //FIXME: proper error reporting
    if self.fronts.add_http_front(http_front) {
      Ok(())
    } else {
      Err(String::from("could not add HTTP front"))
    }
  }

  pub fn remove_http_front(&mut self, mut http_front: HttpFront) -> Result<(), String> {
    debug!("removing http_front {:?}", http_front);
    //FIXME: proper error reporting
    if self.fronts.remove_http_front(http_front) {
      Ok(())
    } else {
      Err(String::from("could not remove HTTP front"))
    }
  }

  pub fn frontend_from_request(&self, host: &str, uri: &str) -> Option<String> {
    let host: &str = if let Ok((i, (hostname, _))) = hostname_and_port(host.as_bytes()) {
      if i != &b""[..] {
        error!("frontend_from_request: invalid remaining chars after hostname. Host: {}", host);
        return None;
      }

      /*if port == Some(&b"80"[..]) {
      // it is alright to call from_utf8_unchecked,
      // we already verified that there are only ascii
      // chars in there
        unsafe { from_utf8_unchecked(hostname) }
      } else {
        host
      }
      */
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
          ErrorKind::WouldBlock => AcceptError::WouldBlock,
          _ => {
            error!("accept() IO error: {:?}", e);
            AcceptError::IoError
          }
        }
      }).map(|(sock,_)| sock)
    } else {
      error!("cannot accept connections, no listening socket available");
      Err(AcceptError::IoError)
    }
  }
}

impl ProxyConfiguration<Session> for Proxy {
  fn connect_to_backend(&mut self, poll: &mut Poll, session: &mut Session, back_token: Token) -> Result<BackendConnectAction,ConnectionError> {
    let old_app_id = session.http().and_then(|ref http| http.app_id.clone());
    let old_back_token = session.back_token();

    self.check_circuit_breaker(session)?;

    let app_id = self.app_id_from_request(session)?;

    if (session.http().and_then(|h| h.app_id.as_ref()) == Some(&app_id)) && session.back_connected == BackendConnectionStatus::Connected {
      let has_backend = session.backend.as_ref().map(|backend| {
          let ref backend = *backend.borrow();
          self.backends.borrow().has_backend(&app_id, backend)
        }).unwrap_or(false);

      let is_valid_backend_socket = has_backend &&
        session.http_mut().map(|h| h.is_valid_backend_socket()).unwrap_or(false);

      if is_valid_backend_socket {
        //matched on keepalive
        session.metrics.backend_id = session.backend.as_ref().map(|i| i.borrow().backend_id.clone());
        session.metrics.backend_start();
        return Ok(BackendConnectAction::Reuse);
      } else if let Some(token) = session.back_token() {
        session.close_backend(token, poll);

        //reset the back token here so we can remove it
        //from the slab after backend_from* fails
        session.set_back_token(token);
      }
    }

    //replacing with a connection to another application
    if old_app_id.is_some() && old_app_id.as_ref() != Some(&app_id) {
      if let Some(token) = session.back_token() {
        session.close_backend(token, poll);

        //reset the back token here so we can remove it
        //from the slab after backend_from* fails
        session.set_back_token(token);
      }
    }

    session.app_id = Some(app_id.clone());

    let front_should_stick = self.applications.get(&app_id).map(|ref app| app.sticky_session).unwrap_or(false);
    let socket = self.backend_from_request(session, &app_id, front_should_stick)?;

    session.http_mut().map(|http| {
      http.app_id = Some(app_id.clone());
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
    // ToDo temporary
    //trace!("{} notified", message);
    match message.order {
      ProxyRequestData::AddApplication(application) => {
        debug!("{} add application {:?}", message.id, application);
        self.add_application(application);
        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
      },
      ProxyRequestData::RemoveApplication(application) => {
        debug!("{} remove application {:?}", message.id, application);
        self.remove_application(&application);
        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
      },
      ProxyRequestData::AddHttpFront(front) => {
        debug!("{} add front {:?}", message.id, front);
        if let Some(listener) = self.listeners.values_mut().find(|l| l.address == front.address) {
          match listener.add_http_front(front) {
            Ok(_) => ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None },
            Err(err) => ProxyResponse{ id: message.id, status: ProxyResponseStatus::Error(err), data: None }
          }
        } else {
          panic!("no HTTP listener found for front: {:?}", front);

          //let (listener, tokens) = Listener::new(HttpListener::default(), event_loop,
          //  self.pool.clone(), None, token: Token) -> (Listener,HashSet<Token>
        }
      },
      ProxyRequestData::RemoveHttpFront(front) => {
        debug!("{} front {:?}", message.id, front);
        if let Some(listener) = self.listeners.values_mut().find(|l| l.address == front.address) {
          match (*listener).remove_http_front(front) {
            Ok(_) => ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None },
            Err(err) => ProxyResponse{ id: message.id, status: ProxyResponseStatus::Error(err), data: None }
          }
        } else {
          panic!("trying to remove front from non existing listener");
        }
      },
      ProxyRequestData::RemoveListener(remove) => {
        debug!("removing HTTP listener at address {:?}", remove.front);
        if !self.remove_listener(remove.front) {
          ProxyResponse {
              id: message.id,
              status: ProxyResponseStatus::Error(format!("no HTTP listener to remove at address {:?}", remove.front)), 
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
        for (_, mut l) in self.listeners.drain() {
          l.listener.take().map(|sock| {
            if let Err(e) = event_loop.deregister(&sock) {
              error!("error deregistering listen socket({:?}): {:?}", sock, e);
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
        info!("{} changing logging filter to {}", message.id, logging_filter);
        logging::LOGGER.with(|l| {
          let directives = logging::parse_logging_spec(&logging_filter);
          l.borrow_mut().set_directives(directives);
        });
        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
      },
      command => {
        debug!("{} unsupported message for HTTP proxy, ignoring: {:?}", message.id, command);
        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Error(String::from("unsupported message")), data: None }
      }
    }
  }

  fn accept(&mut self, token: ListenToken) -> Result<TcpStream, AcceptError> {
    self.listeners.get_mut(&Token(token.0)).unwrap().accept()
  }

  fn create_session(&mut self, frontend_sock: TcpStream, listen_token: ListenToken,
    poll: &mut Poll, session_token: Token, timeout: Timeout, delay: Duration)
  -> Result<(Rc<RefCell<Session>>, bool), AcceptError> {
    if let Some(ref listener) = self.listeners.get(&Token(listen_token.0)) {
      if let Err(e) = frontend_sock.set_nodelay(true) {
        error!("error setting nodelay on front socket({:?}): {:?}", frontend_sock, e);
      }
      if let Some(c) = Session::new(frontend_sock, session_token, Rc::downgrade(&self.pool),
      listener.config.public_address.unwrap_or(listener.config.front),
      listener.config.expect_proxy, listener.config.sticky_name.clone(), timeout,
      listener.answers.clone(), listener.token, delay) {
        if let Err(e) = poll.register(
          c.front_socket(),
          session_token,
          Ready::readable() | Ready::writable() | Ready::from(UnixReady::hup() | UnixReady::error()),
          PollOpt::edge()
          ) {
            error!("error deregistering listen socket({:?}): {:?}", c.front_socket(), e);
          }

        Ok((Rc::new(RefCell::new(c)), false))
      } else {
        Err(AcceptError::TooManySessions)
      }
    } else {
      //FIXME
      Err(AcceptError::IoError)
    }

  }

  fn listen_port_state(&self, port: &u16) -> ListenPortState {
    //FIXME TOKEN
    fixme!();
    ListenPortState::Available
    //let token = Token(0);
    //if port == &self.listeners[&token].address.port() { ListenPortState::InUse } else { ListenPortState::Available }
  }
}

pub fn start(config: HttpListener, channel: ProxyChannel, max_buffers: usize, buffer_size: usize) {
  use super::server::{self,ProxySessionCast};
  let mut event_loop  = Poll::new().expect("could not create event loop");

  let pool = Rc::new(RefCell::new(
    Pool::with_capacity(2*max_buffers, 0, || Buffer::with_capacity(buffer_size))
  ));
  let backends = Rc::new(RefCell::new(BackendMap::new()));
  let mut sessions: Slab<Rc<RefCell<ProxySessionCast>>,SessionToken> = Slab::with_capacity(max_buffers);
  {
    let entry = sessions.vacant_entry().expect("session list should have enough room at startup");
    info!("taking token {:?} for channel", entry.index());
    entry.insert(Rc::new(RefCell::new(ListenSession { protocol: Protocol::HTTPListen })));
  }
  {
    let entry = sessions.vacant_entry().expect("session list should have enough room at startup");
    info!("taking token {:?} for timer", entry.index());
    entry.insert(Rc::new(RefCell::new(ListenSession { protocol: Protocol::HTTPListen })));
  }
  {
    let entry = sessions.vacant_entry().expect("session list should have enough room at startup");
    info!("taking token {:?} for metrics", entry.index());
    entry.insert(Rc::new(RefCell::new(ListenSession { protocol: Protocol::HTTPListen })));
  }

  let token = {
    let entry = sessions.vacant_entry().expect("session list should have enough room at startup");
    let e = entry.insert(Rc::new(RefCell::new(ListenSession { protocol: Protocol::HTTPListen })));
    Token(e.index().0)
  };

  let front = config.front;
  let mut proxy = Proxy::new(pool.clone(), backends.clone());
  let _ = proxy.add_listener(config, token);
  let _ = proxy.activate_listener(&mut event_loop, &front, None);
  let (scm_server, scm_client) = UnixStream::pair().unwrap();
  let scm = ScmSocket::new(scm_client.into_raw_fd());
  if let Err(e) = scm.send_listeners(&Listeners {
    http: Vec::new(),
    tls:  Vec::new(),
    tcp:  Vec::new(),
  }) {
    error!("error sending empty listeners: {:?}", e);
  }

  let mut server_config: server::ServerConfig = Default::default();
  server_config.max_connections = max_buffers;
  let mut server    = Server::new(event_loop, channel, ScmSocket::new(scm_server.into_raw_fd()),
    sessions, pool, backends, Some(proxy), None, None, server_config, None);

  println!("starting event loop");
  server.run();
  println!("ending event loop");
}

#[cfg(test)]
mod tests {
  extern crate tiny_http;
  use super::*;
  use std::net::TcpStream;
  use std::io::{Read,Write};
  use std::{thread,str};
  use std::sync::{
    Arc, Barrier,
  };
  use std::net::SocketAddr;
  use std::str::FromStr;
  use std::time::Duration;
  use sozu_command::proxy::{ProxyRequestData,HttpFront,Backend,HttpListener,ProxyRequest,LoadBalancingParams,PathRule,RulePosition};
  use sozu_command::config::LoadBalancingAlgorithms;
  use sozu_command::channel::Channel;

  /*
  #[test]
  #[cfg(target_pointer_width = "64")]
  fn size_test() {
    assert_size!(ExpectProxyProtocol<mio::net::TcpStream>, 520);
    assert_size!(Http<mio::net::TcpStream>, 1016);
    assert_size!(Pipe<mio::net::TcpStream>, 224);
    assert_size!(State, 1024);
    // fails depending on the platform?
    //assert_size!(Session, 1240);
  }
  */

  #[test]
  fn mi() {
    setup_test_logger!();
    let barrier = Arc::new(Barrier::new(2));
    start_server(1025, barrier.clone());
    barrier.wait();

    let front: SocketAddr = FromStr::from_str("127.0.0.1:1024").expect("could not parse address");
    let config = HttpListener {
      front,
      ..Default::default()
    };

    let (mut command, channel) = Channel::generate(1000, 10000).expect("should create a channel");
    let jg = thread::spawn(move || {
      setup_test_logger!();
      start(config, channel, 10, 16384);
    });

    let front = HttpFront { app_id: String::from("app_1"), address: "127.0.0.1:1024".parse().unwrap(), hostname: String::from("localhost"), path: PathRule::Prefix(String::from("/")), position: RulePosition::Tree };
    command.write_message(&ProxyRequest { id: String::from("ID_ABCD"), order: ProxyRequestData::AddHttpFront(front) });
    let backend = Backend { app_id: String::from("app_1"),backend_id: String::from("app_1-0"), address: "127.0.0.1:1025".parse().unwrap(), load_balancing_parameters: Some(LoadBalancingParams::default()), sticky_id: None, backup: None };
    command.write_message(&ProxyRequest { id: String::from("ID_EFGH"), order: ProxyRequestData::AddBackend(backend) });

    println!("test received: {:?}", command.read_message());
    println!("test received: {:?}", command.read_message());

    let mut client = TcpStream::connect(("127.0.0.1", 1024)).expect("could not parse address");

    // 5 seconds of timeout
    client.set_read_timeout(Some(Duration::new(5,0)));
    let w = client.write(&b"GET / HTTP/1.1\r\nHost: localhost:1024\r\nConnection: Close\r\n\r\n"[..]);
    println!("http client write: {:?}", w);

    barrier.wait();
    let mut buffer = [0;4096];
    let mut index = 0;

    loop {
      assert!(index <= 201);
      if index == 201 {
        break;
      }

      let r = client.read(&mut buffer[index..]);
      println!("http client read: {:?}", r);
      match r {
        Err(e)      => assert!(false, "client request should not fail. Error: {:?}",e),
        Ok(sz) => {
          index += sz;
        }
      }
    }
    println!("Response: {}", str::from_utf8(&buffer[..index]).expect("could not make string from buffer"));
  }

  #[test]
  fn keep_alive() {
    setup_test_logger!();
    let barrier = Arc::new(Barrier::new(2));
    start_server(1028, barrier.clone());
    barrier.wait();

    let front: SocketAddr = FromStr::from_str("127.0.0.1:1031").expect("could not parse address");
    let config = HttpListener {
      front,
      ..Default::default()
    };

    let (mut command, channel) = Channel::generate(1000, 10000).expect("should create a channel");

    let jg = thread::spawn(move|| {
      start(config, channel, 10, 16384);
    });

    let front = HttpFront { app_id: String::from("app_1"), address: "127.0.0.1:1031".parse().unwrap(), hostname: String::from("localhost"), path: PathRule::Prefix(String::from("/")), position: RulePosition::Tree };
    command.write_message(&ProxyRequest { id: String::from("ID_ABCD"), order: ProxyRequestData::AddHttpFront(front) });
    let backend = Backend { app_id: String::from("app_1"), backend_id: String::from("app_1-0"), address: "127.0.0.1:1028".parse().unwrap(), load_balancing_parameters: Some(LoadBalancingParams::default()), sticky_id: None, backup: None };
    command.write_message(&ProxyRequest { id: String::from("ID_EFGH"), order: ProxyRequestData::AddBackend(backend) });

    println!("test received: {:?}", command.read_message());
    println!("test received: {:?}", command.read_message());

    let mut client = TcpStream::connect(("127.0.0.1", 1031)).expect("could not parse address");
    // 5 seconds of timeout
    client.set_read_timeout(Some(Duration::new(5,0)));

    let w = client.write(&b"GET / HTTP/1.1\r\nHost: localhost:1031\r\n\r\n"[..]).unwrap();
    println!("http client write: {:?}", w);
    barrier.wait();

    let mut buffer = [0;4096];
    let mut index = 0;

    loop {
      assert!(index <= 201);
      if index == 201 {
        break;
      }

      let r = client.read(&mut buffer[index..]);
      println!("http client read: {:?}", r);
      match r {
        Err(e)      => assert!(false, "client request should not fail. Error: {:?}",e),
        Ok(sz) => {
          index += sz;
        }
      }
    }
    println!("Response: {}", str::from_utf8(&buffer[..index]).expect("could not make string from buffer"));

    println!("first request ended, will send second one");
    let w2 = client.write(&b"GET / HTTP/1.1\r\nHost: localhost:1031\r\n\r\n"[..]);
    println!("http client write: {:?}", w2);
    barrier.wait();

    let mut buffer2 = [0;4096];
    let mut index = 0;

    loop {
      assert!(index <= 201);
      if index == 201 {
        break;
      }

      let r2 = client.read(&mut buffer2[index..]);
      println!("http client read: {:?}", r2);
      match r2 {
        Err(e)      => assert!(false, "client request should not fail. Error: {:?}",e),
        Ok(sz) => {
          index += sz;
        }
      }
    }
    println!("Response: {}", str::from_utf8(&buffer2[..index]).expect("could not make string from buffer"));
  }

  #[test]
  fn https_redirect() {
    setup_test_logger!();
    let front: SocketAddr = FromStr::from_str("127.0.0.1:1041").expect("could not parse address");
    let config = HttpListener {
      front,
      ..Default::default()
    };

    let (mut command, channel) = Channel::generate(1000, 10000).expect("should create a channel");
    let jg = thread::spawn(move || {
      setup_test_logger!();
      start(config, channel, 10, 16384);
    });

    let application = Application { app_id: String::from("app_1"), sticky_session: false, https_redirect: true, proxy_protocol: None, load_balancing_policy: LoadBalancingAlgorithms::default(), answer_503: None };
    command.write_message(&ProxyRequest { id: String::from("ID_ABCD"), order: ProxyRequestData::AddApplication(application) });
    let front = HttpFront { app_id: String::from("app_1"), address: "127.0.0.1:1041".parse().unwrap(), hostname: String::from("localhost"), path: PathRule::Prefix(String::from("/")), position: RulePosition::Tree };
    command.write_message(&ProxyRequest { id: String::from("ID_EFGH"), order: ProxyRequestData::AddHttpFront(front) });
    let backend = Backend { app_id: String::from("app_1"),backend_id: String::from("app_1-0"), address: "127.0.0.1:1040".parse().unwrap(), load_balancing_parameters: Some(LoadBalancingParams::default()), sticky_id: None, backup: None };
    command.write_message(&ProxyRequest { id: String::from("ID_IJKL"), order: ProxyRequestData::AddBackend(backend) });

    println!("test received: {:?}", command.read_message());
    println!("test received: {:?}", command.read_message());
    println!("test received: {:?}", command.read_message());

    let mut client = TcpStream::connect(("127.0.0.1", 1041)).expect("could not parse address");
    // 5 seconds of timeout
    client.set_read_timeout(Some(Duration::new(5,0)));

    let w = client.write(&b"GET /redirected?true HTTP/1.1\r\nHost: localhost\r\nConnection: Close\r\n\r\n"[..]);
    println!("http client write: {:?}", w);

    let expected_answer = "HTTP/1.1 301 Moved Permanently\r\nContent-Length: 0\r\nLocation: https://localhost/redirected?true\r\n\r\n";
    let mut buffer = [0;4096];
    let mut index = 0;
    loop {
      assert!(index <= expected_answer.len());
      if index == expected_answer.len() {
        break;
      }

      let r = client.read(&mut buffer[..]);
      println!("http client read: {:?}", r);
      match r {
        Err(e)      => assert!(false, "client request should not fail. Error: {:?}",e),
        Ok(sz) => {
          index += sz;
        }
      }
    }

    let answer = str::from_utf8(&buffer[..index]).expect("could not make string from buffer");
    println!("Response: {}", answer);
    assert_eq!(answer, expected_answer);
  }


  use self::tiny_http::{Server, Response};

  fn start_server(port: u16, barrier: Arc<Barrier>) {
    thread::spawn(move|| {
      let server = Server::http(&format!("127.0.0.1:{}", port)).expect("could not create server");
      info!("starting web server in port {}", port);
      barrier.wait();

      for request in server.incoming_requests() {
        println!("backend web server got request -> method: {:?}, url: {:?}, headers: {:?}",
          request.method(),
          request.url(),
          request.headers()
        );

        let response = Response::from_string("hello world");
        request.respond(response).unwrap();
        println!("backend web server sent response");
        barrier.wait();
        println!("server session stopped");
      }

      println!("server on port {} closed", port);
    });
  }

  #[test]
  fn frontend_from_request_test() {
    let app_id1 = "app_1".to_owned();
    let app_id2 = "app_2".to_owned();
    let app_id3 = "app_3".to_owned();
    let uri1 = "/".to_owned();
    let uri2 = "/yolo".to_owned();
    let uri3 = "/yolo/swag".to_owned();

    let mut fronts = Router::new();
    fronts.add_http_front(HttpFront { app_id: app_id1, address: "0.0.0.0:80".parse().unwrap(), hostname: "lolcatho.st".to_owned(),
                              path: PathRule::Prefix(uri1), position: RulePosition::Tree });
    fronts.add_http_front(HttpFront { app_id: app_id2, address: "0.0.0.0:80".parse().unwrap(), hostname: "lolcatho.st".to_owned(),
                              path: PathRule::Prefix(uri2), position: RulePosition::Tree });
    fronts.add_http_front(HttpFront { app_id: app_id3, address: "0.0.0.0:80".parse().unwrap(), hostname: "lolcatho.st".to_owned(),
                              path: PathRule::Prefix(uri3), position: RulePosition::Tree });
    fronts.add_http_front(HttpFront { app_id: "app_1".to_owned(), address: "0.0.0.0:80".parse().unwrap(), hostname: "other.domain".to_owned(), path: PathRule::Prefix("/test".to_owned()), position: RulePosition::Tree });

    let front: SocketAddr = FromStr::from_str("127.0.0.1:1030").expect("could not parse address");
    let listener = Listener {
      listener: None,
      address:  front,
      fronts,
      answers: Rc::new(RefCell::new(HttpAnswers::new("HTTP/1.1 404 Not Found\r\n\r\n", "HTTP/1.1 503 your application is in deployment\r\n\r\n"))),
      config: Default::default(),
      token: Token(0),
      active: true,
    };

    let frontend1 = listener.frontend_from_request("lolcatho.st", "/");
    let frontend2 = listener.frontend_from_request("lolcatho.st", "/test");
    let frontend3 = listener.frontend_from_request("lolcatho.st", "/yolo/test");
    let frontend4 = listener.frontend_from_request("lolcatho.st", "/yolo/swag");
    let frontend5 = listener.frontend_from_request("domain", "/");
    assert_eq!(frontend1.expect("should find frontend"), "app_1");
    assert_eq!(frontend2.expect("should find frontend"), "app_1");
    assert_eq!(frontend3.expect("should find frontend"), "app_2");
    assert_eq!(frontend4.expect("should find frontend"), "app_3");
    assert_eq!(frontend5, None);
  }
}
