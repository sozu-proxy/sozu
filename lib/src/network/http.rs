use std::collections::{HashMap,HashSet};
use std::io::{self,Read,Write,ErrorKind};
use std::rc::{Rc,Weak};
use std::cell::RefCell;
use std::thread::{self,Thread,Builder};
use std::os::unix::io::RawFd;
use std::os::unix::io::{FromRawFd,AsRawFd};
use std::sync::mpsc::{self,channel,Receiver};
use std::net::{SocketAddr,IpAddr,Shutdown};
use std::str::{FromStr, from_utf8, from_utf8_unchecked};
use mio::*;
use mio::net::*;
use mio_uds::UnixStream;
use mio::unix::UnixReady;
use pool::{Pool,Checkout,Reset};
use uuid::Uuid;
use nom::{HexDisplay,IResult};
use rand::random;
use time::{SteadyTime,Duration};
use slab::{Entry,VacantEntry,Slab};

use sozu_command::channel::Channel;
use sozu_command::state::ConfigState;
use sozu_command::scm_socket::ScmSocket;
use sozu_command::messages::{self,Application,Order,HttpFront,HttpProxyConfiguration,OrderMessage,OrderMessageAnswer,OrderMessageStatus};

use network::{AppId,Backend,ClientResult,ConnectionError,RequiredEvents,Protocol,Readiness,SessionMetrics,
  ProxyClient,ProxyConfiguration,AcceptError,BackendConnectAction,BackendConnectionStatus,
  CloseResult};
use network::backends::BackendMap;
use network::buffer_queue::BufferQueue;
use network::protocol::{ProtocolResult,StickySession,TlsHandshake,Http,Pipe};
use network::protocol::http::DefaultAnswerStatus;
use network::proxy::{Server,ProxyChannel,ListenToken,ClientToken,ListenClient};
use network::socket::{SocketHandler,SocketResult,server_bind};
use network::retry::RetryPolicy;
use parser::http11::hostname_and_port;
use network::tcp;
use util::UnwrapLog;
use network::https_rustls;

type BackendToken = Token;

#[derive(PartialEq)]
pub enum ClientStatus {
  Normal,
  DefaultAnswer,
}

pub enum State {
  Http(Http<TcpStream>),
  WebSocket(Pipe<TcpStream>)
}

pub struct Client {
  frontend_token: Token,
  backend_token:  Option<Token>,
  backend:        Option<Rc<RefCell<Backend>>>,
  back_connected: BackendConnectionStatus,
  protocol:       Option<State>,
  pool:           Weak<RefCell<Pool<BufferQueue>>>,
  sticky_session: bool,
  metrics:        SessionMetrics,
  pub app_id:     Option<String>,
}

impl Client {
  pub fn new(sock: TcpStream, token: Token, pool: Weak<RefCell<Pool<BufferQueue>>>, public_address: Option<IpAddr>) -> Option<Client> {
    let protocol = if let Some(pool) = pool.upgrade() {
      let mut p = pool.borrow_mut();
      if let (Some(front_buf), Some(back_buf)) = (p.checkout(), p.checkout()) {
        Some(Http::new(sock, token, front_buf, back_buf, public_address,
          Protocol::HTTP).expect("should create a HTTP state"))
      } else { None }
    } else { None };

    protocol.map(|http| {
      let request_id = Uuid::new_v4().hyphenated().to_string();
      let log_ctx    = format!("{}\tunknown\t", &request_id);
      let mut client = Client {
        backend_token:  None,
        backend:        None,
        back_connected: BackendConnectionStatus::NotConnected,
        protocol:       Some(State::Http(http)),
        frontend_token: token,
        pool:           pool,
        sticky_session: false,
        metrics:        SessionMetrics::new(),
        app_id:         None,
      };

      client.readiness().front_interest = UnixReady::from(Ready::readable()) | UnixReady::hup() | UnixReady::error();

      client
    })
  }

  pub fn upgrade(&mut self) {
    debug!("HTTP::upgrade");
    let protocol = unwrap_msg!(self.protocol.take());
    if let State::Http(http) = protocol {
      debug!("switching to pipe");
      let front_token = http.front_token();
      let back_token  = unwrap_msg!(http.back_token());

      let mut pipe = Pipe::new(http.frontend, Some(unwrap_msg!(http.backend)),
        http.front_buf, http.back_buf, http.public_address);

      pipe.readiness.front_readiness = http.readiness.front_readiness;
      pipe.readiness.back_readiness  = http.readiness.back_readiness;
      pipe.set_front_token(front_token);
      pipe.set_back_token(back_token);

      self.protocol = Some(State::WebSocket(pipe));
    } else {
      self.protocol = Some(protocol);
    }
  }

  pub fn set_answer(&mut self, answer: DefaultAnswerStatus, buf: &[u8])  {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http) => http.set_answer(answer, buf),
      _ => {}
    }
  }

  pub fn http(&mut self) -> Option<&mut Http<TcpStream>> {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http) => Some(http),
      _ => None
    }
  }

  fn front_hup(&mut self) -> ClientResult {
    ClientResult::CloseClient
  }

  fn back_hup(&mut self) -> ClientResult {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http)      => http.back_hup(),
      State::WebSocket(ref mut pipe) => pipe.back_hup()
    }
  }

  fn log_context(&self) -> String {
    match *unwrap_msg!(self.protocol.as_ref()) {
      State::Http(ref http) => {
        if let Some(ref app_id) = http.app_id {
          format!("{}\t{}\t", http.request_id, app_id)
        } else {
          format!("{}\tunknown\t", http.request_id)
        }

      },
      _ => "".to_string()
    }
  }

  // Read content from the client
  fn readable(&mut self) -> ClientResult {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http)      => http.readable(&mut self.metrics),
      State::WebSocket(ref mut pipe) => pipe.readable(&mut self.metrics)
    }
  }

  // Forward content to client
  fn writable(&mut self) -> ClientResult {
    match  *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http)      => http.writable(&mut self.metrics),
      State::WebSocket(ref mut pipe) => pipe.writable(&mut self.metrics)
    }
  }

  // Forward content to application
  fn back_writable(&mut self) -> ClientResult {
    match *unwrap_msg!(self.protocol.as_mut())  {
      State::Http(ref mut http)      => http.back_writable(&mut self.metrics),
      State::WebSocket(ref mut pipe) => pipe.back_writable(&mut self.metrics)
    }
  }

  // Read content from application
  fn back_readable(&mut self) -> ClientResult {
    let (upgrade, result) = match  *unwrap_msg!(self.protocol.as_mut())  {
      State::Http(ref mut http)      => http.back_readable(&mut self.metrics),
      State::WebSocket(ref mut pipe) => (ProtocolResult::Continue, pipe.back_readable(&mut self.metrics))
    };

    if upgrade == ProtocolResult::Continue {
      result
    } else {
      self.upgrade();
      match *unwrap_msg!(self.protocol.as_mut()) {
        State::WebSocket(ref mut pipe) => pipe.back_readable(&mut self.metrics),
        _ => result
      }
    }
  }

  fn front_socket(&self) -> &TcpStream {
    match *unwrap_msg!(self.protocol.as_ref()) {
      State::Http(ref http)      => http.front_socket(),
      State::WebSocket(ref pipe) => pipe.front_socket()
    }
  }

  fn back_socket(&self)  -> Option<&TcpStream> {
    match *unwrap_msg!(self.protocol.as_ref()) {
      State::Http(ref http)      => http.back_socket(),
      State::WebSocket(ref pipe) => pipe.back_socket()
    }
  }

  fn back_token(&self)   -> Option<Token> {
    self.backend_token
  }

  fn set_back_socket(&mut self, socket: TcpStream) {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http)      => http.set_back_socket(socket),
      State::WebSocket(ref mut pipe) => {} /*pipe.set_back_socket(unwrap_msg!(socket.try_clone()))*/
    }
  }

  fn set_front_token(&mut self, token: Token) {
    self.frontend_token = token;
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http)      => http.set_front_token(token),
      State::WebSocket(ref mut pipe) => pipe.set_front_token(token)
    }
  }

  fn set_back_token(&mut self, token: Token) {
    self.backend_token = Some(token);
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http)      => http.set_back_token(token),
      State::WebSocket(ref mut pipe) => pipe.set_back_token(token)
    }
  }

  fn back_connected(&self)     -> BackendConnectionStatus {
    self.back_connected
  }

  fn set_back_connected(&mut self, connected: BackendConnectionStatus) {
    self.back_connected = connected;
    if connected == BackendConnectionStatus::Connected {
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

  fn remove_backend(&mut self) -> (Option<String>, Option<SocketAddr>) {
    debug!("{}\tPROXY [{} -> {}] CLOSED BACKEND",
      self.http().map(|h| h.log_ctx.clone()).unwrap_or("".to_string()), self.frontend_token.0,
      self.backend_token.map(|t| format!("{}", t.0)).unwrap_or("-".to_string()));
    let addr:Option<SocketAddr> = self.back_socket().and_then(|sock| sock.peer_addr().ok());
    self.backend_token = None;
    (self.app_id.clone(), addr)
  }

  fn readiness(&mut self) -> &mut Readiness {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http)      => &mut http.readiness,
      State::WebSocket(ref mut pipe) => &mut pipe.readiness
    }
  }
}

impl ProxyClient for Client {
  fn close(&mut self, poll: &mut Poll) -> CloseResult {
    self.metrics.service_stop();
    self.front_socket().shutdown(Shutdown::Both);
    poll.deregister(self.front_socket());

    let mut result = CloseResult::default();

    if let Some(tk) = self.backend_token {
      result.tokens.push(tk)
    }

    if let (Some(app_id), Some(addr)) = self.remove_backend() {
      result.backends.push((app_id, addr.clone()));
    }

    if let Some(sock) = self.back_socket() {
      decr!("backend.connections");
      sock.shutdown(Shutdown::Both);
      poll.deregister(sock);
    }

    result.tokens.push(self.frontend_token);

    result
  }

  fn close_backend(&mut self, _: Token, poll: &mut Poll) -> Option<(String,SocketAddr)> {
    let mut res = None;
    if let (Some(app_id), Some(addr)) = self.remove_backend() {
      res = Some((app_id, addr.clone()));
    }

    if let Some(sock) = self.back_socket() {
      sock.shutdown(Shutdown::Both);
      poll.deregister(sock);
      decr!("backend.connections");
    }

    res
  }

  fn protocol(&self) -> Protocol {
    Protocol::HTTP
  }

  fn process_events(&mut self, token: Token, events: Ready) {
    self.readiness().front_readiness = self.readiness().front_readiness | UnixReady::from(events);
    if self.backend_token ==Some(token) {
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
      } else if self.readiness().back_readiness != UnixReady::from(Ready::empty()) {
        self.set_back_connected(BackendConnectionStatus::Connected);
      }
    }

    let token = self.frontend_token.clone();
    while counter < max_loop_iterations {
      let front_interest = self.readiness().front_interest & self.readiness().front_readiness;
      let back_interest  = self.readiness().back_interest & self.readiness().back_readiness;

      trace!("PROXY\t{:?} {:?} | front: {:?} | back: {:?} ", token, self.readiness(), front_interest, back_interest);

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
          ClientResult::CloseClient => {
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
          ClientResult::CloseClient => {
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
          error!("PROXY client {:?} front error, disconnecting", self.frontend_token);
        } else {
          error!("PROXY client {:?} back error, disconnecting", self.frontend_token);
        }

        self.readiness().front_interest = UnixReady::from(Ready::empty());
        self.readiness().back_interest  = UnixReady::from(Ready::empty());
        return ClientResult::CloseClient;
      }

      counter += 1;
    }

    if counter == max_loop_iterations {
      error!("PROXY\thandling client {:?} went through {} iterations, there's a probable infinite loop bug, closing the connection", self.frontend_token, max_loop_iterations);

      let front_interest = self.readiness().front_interest & self.readiness().front_readiness;
      let back_interest  = self.readiness().back_interest & self.readiness().back_readiness;

      let token = self.frontend_token.clone();
      error!("PROXY\t{:?} readiness: {:?} | front: {:?} | back: {:?} ", token,
        self.readiness(), front_interest, back_interest);

      return ClientResult::CloseClient;
    }

    ClientResult::Continue
  }
}

#[allow(non_snake_case)]
pub struct DefaultAnswers {
  pub NotFound:           Vec<u8>,
  pub ServiceUnavailable: Vec<u8>
}

pub type Hostname = String;

pub struct ServerConfiguration {
  listener:        Option<TcpListener>,
  address:         SocketAddr,
  backends:        BackendMap,
  fronts:          HashMap<Hostname, Vec<HttpFront>>,
  applications:    HashMap<AppId, Application>,
  pool:            Rc<RefCell<Pool<BufferQueue>>>,
  answers:         DefaultAnswers,
  config:          HttpProxyConfiguration,
}

impl ServerConfiguration {
  pub fn new(config: HttpProxyConfiguration, event_loop: &mut Poll,
    pool: Rc<RefCell<Pool<BufferQueue>>>, tcp_listener: Option<TcpListener>, token: Token) -> (ServerConfiguration,HashSet<Token>) {

    let front = config.front;

    let listener = tcp_listener.or_else(|| server_bind(&config.front).map_err(|e| {
      error!("could not create listener {:?}: {:?}", front, e);
   }).ok());

    let mut listeners = HashSet::new();
    if let Some(ref sock) = listener {
      event_loop.register(sock, token, Ready::readable(), PollOpt::edge());
      listeners.insert(token);
    }


    let default = DefaultAnswers {
      NotFound: Vec::from(if config.answer_404.len() > 0 {
                  config.answer_404.as_bytes()
                } else {
                  &b"HTTP/1.1 404 Not Found\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"[..]
                }),
      ServiceUnavailable: Vec::from(if config.answer_503.len() > 0 {
                  config.answer_503.as_bytes()
                } else {
                  &b"HTTP/1.1 503 your application is in deployment\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"[..]
                }),
    };

    (ServerConfiguration {
      listener:       listener,
      address:        config.front,
      applications:   HashMap::new(),
      backends:       BackendMap::new(),
      fronts:         HashMap::new(),
      pool:           pool,
      answers:        default,
      config:         config,
    }, listeners)
  }

  pub fn give_back_listener(&mut self) -> Option<TcpListener> {
    self.listener.take()
  }

  pub fn add_application(&mut self, application: Application, event_loop: &mut Poll) {
    self.applications.insert(application.app_id.clone(), application);
  }

  pub fn remove_application(&mut self, app_id: &str, event_loop: &mut Poll) {
    self.applications.remove(app_id);
  }

  pub fn add_http_front(&mut self, mut http_front: HttpFront, event_loop: &mut Poll) -> Result<(), String> {
    match ::idna::domain_to_ascii(&http_front.hostname) {
      Ok(hostname) => {
        http_front.hostname = hostname;
        let front2 = http_front.clone();
        let front3 = http_front.clone();
        if let Some(fronts) = self.fronts.get_mut(&http_front.hostname) {
            if !fronts.contains(&front2) {
              fronts.push(front2);
            }
        }

        // FIXME: check that http front port matches the listener's port
        // FIXME: separate the port and hostname, match the hostname separately

        if self.fronts.get(&http_front.hostname).is_none() {
          self.fronts.insert(http_front.hostname, vec![front3]);
        }
        Ok(())
      },
      Err(_) => Err(format!("Couldn't parse hostname {} to ascii", http_front.hostname))
    }
  }

  pub fn remove_http_front(&mut self, mut front: HttpFront, event_loop: &mut Poll) -> Result<(), String> {
    debug!("removing http_front {:?}", front);
    match ::idna::domain_to_ascii(&front.hostname) {
      Ok(hostname) => {
        front.hostname = hostname;
        if let Some(fronts) = self.fronts.get_mut(&front.hostname) {
          fronts.retain(|f| f != &front);
        }
        Ok(())
      },
      Err(_) => Err(format!("Couldn't parse hostname {} to ascii", front.hostname))
    }
  }

  pub fn add_backend(&mut self, app_id: &str, backend_id: &str, backend_address: &SocketAddr, event_loop: &mut Poll) {
    self.backends.add_backend(app_id, backend_id, backend_address);
  }

  pub fn remove_backend(&mut self, app_id: &str, backend_address: &SocketAddr, event_loop: &mut Poll) {
    self.backends.remove_backend(app_id, backend_address);
  }

  pub fn frontend_from_request(&self, host: &str, uri: &str) -> Option<&HttpFront> {
    let host: &str = if let IResult::Done(i, (hostname, port)) = hostname_and_port(host.as_bytes()) {
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

    if let Some(http_fronts) = self.fronts.get(host) {
      let matching_fronts = http_fronts.iter().filter(|f| uri.starts_with(&f.path_begin)); // ToDo match on uri
      let mut front = None;

      for f in matching_fronts {
        if front.is_none() {
          front = Some(f);
        }

        if let Some(ff) = front {
          if f.path_begin.len() > ff.path_begin.len() {
            front = Some(f)
          }
        }
      }
      front
    } else {
      None
    }
  }

  pub fn backend_from_app_id(&mut self, client: &mut Client, app_id: &str, front_should_stick: bool) -> Result<TcpStream,ConnectionError> {
    client.http().map(|h| h.set_app_id(String::from(app_id)));

    match self.backends.backend_from_app_id(app_id) {
      Err(e) => {
        client.set_answer(DefaultAnswerStatus::Answer503, &self.answers.ServiceUnavailable);
        Err(e)
      },
      Ok((backend, conn))  => {
        client.back_connected = BackendConnectionStatus::Connecting;
        if front_should_stick {
          client.http().map(|http| http.sticky_session = Some(StickySession::new(backend.borrow().id.clone())));
        }
        client.metrics.backend_id = Some(backend.borrow().backend_id.clone());
        client.metrics.backend_start();
        client.backend = Some(backend);

        Ok(conn)
      }
    }
  }

  pub fn backend_from_sticky_session(&mut self, client: &mut Client, app_id: &str, sticky_session: u32) -> Result<TcpStream,ConnectionError> {
    client.http().map(|h| h.set_app_id(String::from(app_id)));

    match self.backends.backend_from_sticky_session(app_id, sticky_session) {
      Err(e) => {
        debug!("Couldn't find a backend corresponding to sticky_session {} for app {}", sticky_session, app_id);
        client.set_answer(DefaultAnswerStatus::Answer503, &self.answers.ServiceUnavailable);
        Err(e)
      },
      Ok((backend, conn))  => {
        client.back_connected = BackendConnectionStatus::Connecting;
        client.http().map(|http| http.sticky_session = Some(StickySession::new(backend.borrow().id.clone())));
        client.metrics.backend_id = Some(backend.borrow().backend_id.clone());
        client.metrics.backend_start();
        client.backend = Some(backend);

        Ok(conn)
      }
    }
  }
}

impl ProxyConfiguration<Client> for ServerConfiguration {
  fn connect_to_backend(&mut self, poll: &mut Poll, client: &mut Client, back_token: Token) -> Result<BackendConnectAction,ConnectionError> {
    let h = try!(client.http().unwrap().state.as_ref().unwrap().get_host().ok_or(ConnectionError::NoHostGiven));

    let host: &str = if let IResult::Done(i, (hostname, port)) = hostname_and_port(h.as_bytes()) {
      if i != &b""[..] {
        error!("connect_to_backend: invalid remaining chars after hostname. Host: {}", h);
        return Err(ConnectionError::ToBeDefined);
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
      return Err(ConnectionError::ToBeDefined);
    };

    let sticky_session = client.http().unwrap().state.as_ref().unwrap().get_request_sticky_session();

    //FIXME: too many unwraps here
    let rl     = try!(client.http().unwrap().state.as_ref().unwrap().get_request_line().ok_or(ConnectionError::NoRequestLineGiven));
    if let Some(app_id) = self.frontend_from_request(&host, &rl.uri).map(|ref front| front.app_id.clone()) {

      let front_should_redirect_https = self.applications.get(&app_id).map(|ref app| app.https_redirect).unwrap_or(false);
      if front_should_redirect_https {
        let answer = format!("HTTP/1.1 301 Moved Permanently\r\nContent-Length: 0\r\nLocation: https://{}{}\r\n\r\n", host, rl.uri);
        //FIXME: the buffer is copied but it's not needed
        client.set_answer(DefaultAnswerStatus::Answer301, answer.as_bytes());
        client.readiness().front_interest = UnixReady::from(Ready::writable()) | UnixReady::hup() | UnixReady::error();
        client.readiness().back_interest  = UnixReady::hup() | UnixReady::error();
        return Err(ConnectionError::HttpsRedirect);
      }

      let front_should_stick = self.applications.get(&app_id).map(|ref app| app.sticky_session).unwrap_or(false);

      if (client.http().map(|h| h.app_id.as_ref()).unwrap_or(None) == Some(&app_id)) && client.back_connected == BackendConnectionStatus::Connected {
        if client.backend.as_ref().map(|backend| {
          let ref backend = *backend.borrow();
          self.backends.has_backend(&app_id, backend)
        }).unwrap_or(false) {
          //matched on keepalive
          client.metrics.backend_id = client.backend.as_ref().map(|i| i.borrow().backend_id.clone());
          client.metrics.backend_start();
          return Ok(BackendConnectAction::Reuse);
        } else {

          client.backend = None;
          client.back_connected = BackendConnectionStatus::NotConnected;
          //client.readiness().back_interest  = UnixReady::from(Ready::empty());
          client.readiness().back_readiness = UnixReady::from(Ready::empty());
          client.back_socket().as_ref().map(|sock| {
            poll.deregister(*sock);
            sock.shutdown(Shutdown::Both);
          });
        }
      }

      // circuit breaker
      if client.back_connected == BackendConnectionStatus::Connecting {
        client.backend.as_ref().map(|backend| {
          let ref mut backend = *backend.borrow_mut();
          backend.dec_connections();
          backend.failures += 1;

          let already_unavailable = backend.retry_policy.is_down();
          backend.retry_policy.fail();
          count!("backend.connections.error", 1);
          if !already_unavailable && backend.retry_policy.is_down() {
            count!("backend.down", 1);
          }
        });

        //deregister back socket if it is the wrong one or if it was not connecting
        client.backend = None;
        client.back_connected = BackendConnectionStatus::NotConnected;
        client.readiness().back_interest  = UnixReady::from(Ready::empty());
        client.readiness().back_readiness = UnixReady::from(Ready::empty());
        client.back_socket().as_ref().map(|sock| {
          poll.deregister(*sock);
          sock.shutdown(Shutdown::Both);
        });
      }

      let old_app_id = client.http().and_then(|ref http| http.app_id.clone());
      client.app_id = Some(app_id.clone());

      let conn = match (front_should_stick, sticky_session) {
        (true, Some(session)) => self.backend_from_sticky_session(client, &app_id, session),
        _ => self.backend_from_app_id(client, &app_id, front_should_stick),
      };

      match conn {
        Ok(socket) => {
          let new_app_id = client.http().and_then(|ref http| http.app_id.clone());

          if old_app_id.is_some() && old_app_id != new_app_id {
            client.backend = None;
            client.back_connected = BackendConnectionStatus::NotConnected;
            client.readiness().back_readiness = UnixReady::from(Ready::empty());
            client.back_socket().as_ref().map(|sock| {
              poll.deregister(*sock);
              sock.shutdown(Shutdown::Both);
            });
            // we still want to use the new socket
            client.readiness().back_interest  = UnixReady::from(Ready::writable());
          }


          socket.set_nodelay(true);
          client.readiness().back_interest.insert(Ready::writable());
          client.readiness().back_interest.insert(UnixReady::hup());
          client.readiness().back_interest.insert(UnixReady::error());
          if old_app_id == new_app_id {
            poll.register(
              &socket,
              client.back_token().expect("FIXME"),
              Ready::readable() | Ready::writable() | Ready::from(UnixReady::hup() | UnixReady::error()),
              PollOpt::edge()
            );
            client.set_back_socket(socket);
            Ok(BackendConnectAction::Replace)
          } else {
            poll.register(
              &socket,
              back_token,
              Ready::readable() | Ready::writable() | Ready::from(UnixReady::hup() | UnixReady::error()),
              PollOpt::edge()
            );

            client.set_back_socket(socket);
            client.set_back_token(back_token);
            incr!("backend.connections");
            Ok(BackendConnectAction::New)
          }
          //Ok(())
        },
        Err(ConnectionError::NoBackendAvailable) => {
          client.set_answer(DefaultAnswerStatus::Answer503, &self.answers.ServiceUnavailable);
          client.readiness().front_interest = UnixReady::from(Ready::writable()) | UnixReady::hup() | UnixReady::error();
          client.readiness().back_interest  = UnixReady::hup() | UnixReady::error();
          Err(ConnectionError::NoBackendAvailable)
        }
        Err(ConnectionError::HostNotFound) => {
          client.set_answer(DefaultAnswerStatus::Answer404, &self.answers.NotFound);
          client.readiness().front_interest = UnixReady::from(Ready::writable()) | UnixReady::hup() | UnixReady::error();
          client.readiness().back_interest  = UnixReady::hup() | UnixReady::error();
          Err(ConnectionError::HostNotFound)
        }
        e => panic!(e)
      }
    } else {
      client.set_answer(DefaultAnswerStatus::Answer404, &self.answers.NotFound);
      client.readiness().front_interest = UnixReady::from(Ready::writable()) | UnixReady::hup() | UnixReady::error();
      Err(ConnectionError::HostNotFound)
    }
  }

  fn notify(&mut self, event_loop: &mut Poll, message: OrderMessage) -> OrderMessageAnswer {
    // ToDo temporary
    //trace!("{} notified", message);
    match message.order {
      Order::AddApplication(application) => {
        debug!("{} add application {:?}", message.id, application);
        self.add_application(application, event_loop);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      Order::RemoveApplication(application) => {
        debug!("{} remove application {:?}", message.id, application);
        remove_app_metrics!(&application);
        self.remove_application(&application, event_loop);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      Order::AddHttpFront(front) => {
        debug!("{} add front {:?}", message.id, front);
        match self.add_http_front(front, event_loop) {
          Ok(_) => OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None },
          Err(err) => OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Error(err), data: None }
        }
      },
      Order::RemoveHttpFront(front) => {
        debug!("{} front {:?}", message.id, front);
        match self.remove_http_front(front, event_loop) {
          Ok(_) => OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None },
          Err(err) => OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Error(err), data: None }
        }
      },
      Order::AddBackend(backend) => {
        debug!("{} add backend {:?}", message.id, backend);
        let addr_string = backend.ip_address + ":" + &backend.port.to_string();
        let parsed:Option<SocketAddr> = addr_string.parse().ok();
        if let Some(addr) = parsed {
          self.add_backend(&backend.app_id, &backend.backend_id, &addr, event_loop);
          OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
        } else {
          OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Error(String::from("cannot parse the address")), data: None }
        }
      },
      Order::RemoveBackend(backend) => {
        debug!("{} remove backend {:?}", message.id, backend);
        remove_backend_metrics!(&backend.backend_id);
        let addr_string = backend.ip_address + ":" + &backend.port.to_string();
        let parsed:Option<SocketAddr> = addr_string.parse().ok();
        if let Some(addr) = parsed {
          self.remove_backend(&backend.app_id, &addr, event_loop);
          OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
        } else {
          OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Error(String::from("cannot parse the address")), data: None }
        }
      },
      Order::HttpProxy(configuration) => {
        debug!("{} modifying proxy configuration: {:?}", message.id, configuration);
        self.answers = DefaultAnswers {
          NotFound:           configuration.answer_404.into_bytes(),
          ServiceUnavailable: configuration.answer_503.into_bytes(),
        };
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      Order::SoftStop => {
        info!("{} processing soft shutdown", message.id);
        //FIXME: handle shutdown
        //event_loop.shutdown();
        if let Some(ref sock) = self.listener {
          event_loop.deregister(sock);
        }
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Processing, data: None }
      },
      Order::HardStop => {
        info!("{} hard shutdown", message.id);
        //FIXME: handle shutdown
        //event_loop.shutdown();
        if let Some(ref sock) = self.listener {
          event_loop.deregister(sock);
        }
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      Order::Status => {
        debug!("{} status", message.id);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      Order::Logging(logging_filter) => {
        info!("{} changing logging filter to {}", message.id, logging_filter);
        ::logging::LOGGER.with(|l| {
          let directives = ::logging::parse_logging_spec(&logging_filter);
          l.borrow_mut().set_directives(directives);
        });
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      command => {
        debug!("{} unsupported message, ignoring: {:?}", message.id, command);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Error(String::from("unsupported message")), data: None }
      }
    }
  }

  fn accept(&mut self, token: ListenToken, poll: &mut Poll, client_token: Token)
    -> Result<(Rc<RefCell<Client>>, bool), AcceptError> {

    if let Some(ref sock) = self.listener {
      sock.accept().map_err(|e| {
        match e.kind() {
          ErrorKind::WouldBlock => AcceptError::WouldBlock,
          other => {
            error!("accept() IO error: {:?}", e);
            AcceptError::IoError
          }
        }
      }).and_then(|(frontend_sock, _)| {
        frontend_sock.set_nodelay(true);
        if let Some(c) = Client::new(frontend_sock, client_token, Rc::downgrade(&self.pool), self.config.public_address) {
          poll.register(
            c.front_socket(),
            client_token,
            Ready::readable() | Ready::writable() | Ready::from(UnixReady::hup() | UnixReady::error()),
            PollOpt::edge()
          );

          Ok((Rc::new(RefCell::new(c)), false))
        } else {
          Err(AcceptError::TooManyClients)
        }
      })
    } else {
      error!("cannot accept connections, no listening socket available");
      Err(AcceptError::IoError)
    }
  }

  fn accept_flush(&mut self) {
    if let Some(ref sock) = self.listener {
      while sock.accept().is_ok() {
        error!("accepting and closing connection");
      }
    }
  }

  fn close_backend(&mut self, app_id: String, addr: &SocketAddr) {
    self.backends.close_backend_connection(&app_id, &addr);
  }
}

pub struct InitialServerConfiguration {
  backends:        BackendMap,
  fronts:          HashMap<Hostname, Vec<HttpFront>>,
  applications:    HashMap<AppId, Application>,
  answers:         DefaultAnswers,
  config:          HttpProxyConfiguration,
}

impl InitialServerConfiguration {
  pub fn new(config: HttpProxyConfiguration, state: &ConfigState) -> InitialServerConfiguration {

    let answers = DefaultAnswers {
      NotFound: Vec::from(if config.answer_404.len() > 0 {
                  config.answer_404.as_bytes()
                } else {
                  &b"HTTP/1.1 404 Not Found\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"[..]
                }),
      ServiceUnavailable: Vec::from(if config.answer_503.len() > 0 {
        config.answer_503.as_bytes()
      } else {
        &b"HTTP/1.1 503 your application is in deployment\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"[..]
      }),
    };

    let mut backends = BackendMap::new();
    backends.import_configuration_state(&state.backends);

    InitialServerConfiguration {
      backends:     backends,
      fronts:       state.http_fronts.clone(),
      applications: state.applications.clone(),
      answers:      answers,
      config:       config,
    }
  }

  pub fn start_listening(self, event_loop: &mut Poll, start_at:usize, pool: Rc<RefCell<Pool<BufferQueue>>>) -> io::Result<ServerConfiguration> {

    let front = self.config.front;
    match server_bind(&front) {
      Ok(sock) => {
        event_loop.register(&sock, Token(start_at), Ready::readable(), PollOpt::edge());

        Ok(ServerConfiguration {
          listener:      Some(sock),
          address:       self.config.front,
          applications:  self.applications,
          backends:      self.backends,
          fronts:        self.fronts,
          pool:          pool,
          answers:       self.answers,
          config:        self.config,
        })
      },
      Err(e) => {
        let formatted_err = format!("could not create listener {:?}: {:?}", front, e);
        error!("{}", formatted_err);
        Err(e)
      }
    }

  }
}


pub fn start(config: HttpProxyConfiguration, channel: ProxyChannel, max_buffers: usize, buffer_size: usize) {
  use network::proxy::ProxyClientCast;
  let mut event_loop  = Poll::new().expect("could not create event loop");
  let max_listeners   = 1;

  let pool = Rc::new(RefCell::new(
    Pool::with_capacity(2*max_buffers, 0, || BufferQueue::with_capacity(buffer_size))
  ));
  let mut clients: Slab<Rc<RefCell<ProxyClientCast>>,ClientToken> = Slab::with_capacity(max_buffers);
  {
    let entry = clients.vacant_entry().expect("client list should have enough room at startup");
    info!("taking token {:?} for channel", entry.index());
    entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::HTTPListen })));
  }
  {
    let entry = clients.vacant_entry().expect("client list should have enough room at startup");
    info!("taking token {:?} for metrics", entry.index());
    entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::HTTPListen })));
  }

  let token = {
    let entry = clients.vacant_entry().expect("client list should have enough room at startup");
    let e = entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::HTTPListen })));
    Token(e.index().0)
  };
  let (configuration, listeners) = ServerConfiguration::new(config, &mut event_loop, pool, None, token);
  let (scm_server, scm_client) = UnixStream::pair().unwrap();

  let mut server    = Server::new(event_loop, channel, ScmSocket::new(scm_server.as_raw_fd()),
    clients, Some(configuration), None, None, None, max_buffers);

  println!("starting event loop");
  server.run();
  println!("ending event loop");
}

#[cfg(test)]
mod tests {
  extern crate tiny_http;
  use super::*;
  use slab::Slab;
  use mio::Poll;
  use std::collections::HashMap;
  use std::net::{TcpListener, TcpStream, Shutdown};
  use std::io::{Read,Write};
  use std::{thread,str};
  use std::sync::mpsc::channel;
  use std::net::SocketAddr;
  use std::str::FromStr;
  use std::time::Duration;
  use sozu_command::messages::{Order,HttpFront,Backend,HttpProxyConfiguration,OrderMessage,OrderMessageAnswer};
  use network::buffer_queue::BufferQueue;
  use pool::Pool;

  #[allow(unused_mut, unused_must_use, unused_variables)]
  #[test]
  fn mi() {
    setup_test_logger!();
    start_server(1025);
    let front: SocketAddr = FromStr::from_str("127.0.0.1:1024").expect("could not parse address");
    let config = HttpProxyConfiguration {
      front: front,
      ..Default::default()
    };

    let (mut command, channel) = Channel::generate(1000, 10000).expect("should create a channel");
    let jg = thread::spawn(move || {
      setup_test_logger!();
      start(config, channel, 10, 16384);
    });

    let front = HttpFront { app_id: String::from("app_1"), hostname: String::from("localhost"), path_begin: String::from("/") };
    command.write_message(&OrderMessage { id: String::from("ID_ABCD"), order: Order::AddHttpFront(front) });
    let backend = Backend { app_id: String::from("app_1"),backend_id: String::from("app_1-0"), ip_address: String::from("127.0.0.1"), port: 1025 };
    command.write_message(&OrderMessage { id: String::from("ID_EFGH"), order: Order::AddBackend(backend) });

    println!("test received: {:?}", command.read_message());
    println!("test received: {:?}", command.read_message());
    thread::sleep(Duration::from_millis(300));

    let mut client = TcpStream::connect(("127.0.0.1", 1024)).expect("could not parse address");
    // 5 seconds of timeout
    client.set_read_timeout(Some(Duration::new(5,0)));
    thread::sleep(Duration::from_millis(100));
    let mut w  = client.write(&b"GET / HTTP/1.1\r\nHost: localhost:1024\r\nConnection: Close\r\n\r\n"[..]);
    println!("http client write: {:?}", w);
    let mut buffer = [0;4096];
    thread::sleep(Duration::from_millis(500));
    let mut r = client.read(&mut buffer[..]);
    println!("http client read: {:?}", r);
    match r {
      Err(e)      => assert!(false, "client request should not fail. Error: {:?}",e),
      Ok(sz) => {
        // Read the Response.
        println!("read response");

        println!("Response: {}", str::from_utf8(&buffer[..]).expect("could not make string from buffer"));

        //thread::sleep(Duration::from_millis(300));
        //assert_eq!(&body, &"Hello World!"[..]);
        assert_eq!(sz, 201);
        //assert!(false);
      }
    }
  }

  #[allow(unused_mut, unused_must_use, unused_variables)]
  #[test]
  fn keep_alive() {
    setup_test_logger!();
    start_server(1028);
    let front: SocketAddr = FromStr::from_str("127.0.0.1:1031").expect("could not parse address");
    let config = HttpProxyConfiguration {
      front: front,
      ..Default::default()
    };

    let (mut command, channel) = Channel::generate(1000, 10000).expect("should create a channel");

    let jg = thread::spawn(move|| {
      start(config, channel, 10, 16384);
    });

    let front = HttpFront { app_id: String::from("app_1"), hostname: String::from("localhost"), path_begin: String::from("/") };
    command.write_message(&OrderMessage { id: String::from("ID_ABCD"), order: Order::AddHttpFront(front) });
    let backend = Backend { app_id: String::from("app_1"), backend_id: String::from("app_1-0"), ip_address: String::from("127.0.0.1"), port: 1028 };
    command.write_message(&OrderMessage { id: String::from("ID_EFGH"), order: Order::AddBackend(backend) });

    println!("test received: {:?}", command.read_message());
    println!("test received: {:?}", command.read_message());
    thread::sleep(Duration::from_millis(300));

    let mut client = TcpStream::connect(("127.0.0.1", 1031)).expect("could not parse address");
    // 5 seconds of timeout
    client.set_read_timeout(Some(Duration::new(5,0)));
    thread::sleep(Duration::from_millis(100));
    let mut w  = client.write(&b"GET / HTTP/1.1\r\nHost: localhost:1031\r\n\r\n"[..]);
    println!("http client write: {:?}", w);
    let mut buffer = [0;4096];
    thread::sleep(Duration::from_millis(500));
    let mut r = client.read(&mut buffer[..]);
    println!("http client read: {:?}", r);
    match r {
      Err(e)      => assert!(false, "client request should not fail. Error: {:?}",e),
      Ok(sz) => {
        // Read the Response.
        println!("read response");

        println!("Response: {}", str::from_utf8(&buffer[..]).expect("could not make string from buffer"));

        //thread::sleep(Duration::from_millis(300));
        //assert_eq!(&body, &"Hello World!"[..]);
        assert_eq!(sz, 201);
        //assert!(false);
      }
    }

    println!("first request ended, will send second one");
    let mut buffer2 = [0;4096];
    let mut w2  = client.write(&b"GET / HTTP/1.1\r\nHost: localhost:1031\r\n\r\n"[..]);
    println!("http client write: {:?}", w2);
    thread::sleep(Duration::from_millis(500));
    let mut r2 = client.read(&mut buffer2[..]);
    println!("http client read: {:?}", r2);
    match r2 {
      Err(e)      => assert!(false, "client request should not fail. Error: {:?}",e),
      Ok(sz) => {
        // Read the Response.
        println!("read response");

        println!("Response: {}", str::from_utf8(&buffer2[..]).expect("could not make string from buffer"));

        //thread::sleep(Duration::from_millis(300));
        //assert_eq!(&body, &"Hello World!"[..]);
        assert_eq!(sz, 201);
        //assert!(false);
      }
    }
  }

  #[allow(unused_mut, unused_must_use, unused_variables)]
  #[test]
  fn https_redirect() {
    setup_test_logger!();
    start_server(1040);
    let front: SocketAddr = FromStr::from_str("127.0.0.1:1041").expect("could not parse address");
    let config = HttpProxyConfiguration {
      front: front,
      ..Default::default()
    };

    let (mut command, channel) = Channel::generate(1000, 10000).expect("should create a channel");
    let jg = thread::spawn(move || {
      start(config, channel, 10, 16384);
    });

    let application = Application { app_id: String::from("app_1"), sticky_session: false, https_redirect: true, proxy_protocol: false };
    command.write_message(&OrderMessage { id: String::from("ID_ABCD"), order: Order::AddApplication(application) });
    let front = HttpFront { app_id: String::from("app_1"), hostname: String::from("localhost"), path_begin: String::from("/") };
    command.write_message(&OrderMessage { id: String::from("ID_EFGH"), order: Order::AddHttpFront(front) });
    let backend = Backend { app_id: String::from("app_1"),backend_id: String::from("app_1-0"), ip_address: String::from("127.0.0.1"), port: 1040 };
    command.write_message(&OrderMessage { id: String::from("ID_IJKL"), order: Order::AddBackend(backend) });

    println!("test received: {:?}", command.read_message());
    println!("test received: {:?}", command.read_message());
    println!("test received: {:?}", command.read_message());
    thread::sleep(Duration::from_millis(300));

    let mut client = TcpStream::connect(("127.0.0.1", 1041)).expect("could not parse address");
    // 5 seconds of timeout
    client.set_read_timeout(Some(Duration::new(5,0)));
    thread::sleep(Duration::from_millis(100));
    let mut w  = client.write(&b"GET /redirected?true HTTP/1.1\r\nHost: localhost\r\nConnection: Close\r\n\r\n"[..]);
    println!("http client write: {:?}", w);
    let mut buffer = [0;4096];
    thread::sleep(Duration::from_millis(500));
    let mut r = client.read(&mut buffer[..]);
    println!("http client read: {:?}", r);
    match r {
      Err(e)      => assert!(false, "client request should not fail. Error: {:?}",e),
      Ok(sz) => {
        let answer = str::from_utf8(&buffer[0..sz]).expect("could not make string from buffer");
        // Read the Response.
        println!("read response");
        println!("Response: {}", answer);
        let expected_answer = "HTTP/1.1 301 Moved Permanently\r\nContent-Length: 0\r\nLocation: https://localhost/redirected?true\r\n\r\n";
        assert_eq!(answer, expected_answer);
      }
    }
  }


  use self::tiny_http::{ServerBuilder, Response};

  #[allow(unused_mut, unused_must_use, unused_variables)]
  fn start_server(port: u16) {
    thread::spawn(move|| {
      let server = ServerBuilder::new().with_port(port).build().expect("could not create server");
      println!("starting web server in port {}", port);

      for request in server.incoming_requests() {
        println!("backend web server got request -> method: {:?}, url: {:?}, headers: {:?}",
          request.method(),
          request.url(),
          request.headers()
        );

        let response = Response::from_string("hello world");
        request.respond(response);
        println!("backend web server sent response");
      }

      println!("server on port {} closed", port);
    });
  }

  use mio::net;
  #[test]
  fn frontend_from_request_test() {
    let app_id1 = "app_1".to_owned();
    let app_id2 = "app_2".to_owned();
    let app_id3 = "app_3".to_owned();
    let uri1 = "/".to_owned();
    let uri2 = "/yolo".to_owned();
    let uri3 = "/yolo/swag".to_owned();

    let mut fronts = HashMap::new();
    fronts.insert("lolcatho.st".to_owned(), vec![
      HttpFront { app_id: app_id1, hostname: "lolcatho.st".to_owned(), path_begin: uri1 },
      HttpFront { app_id: app_id2, hostname: "lolcatho.st".to_owned(), path_begin: uri2 },
      HttpFront { app_id: app_id3, hostname: "lolcatho.st".to_owned(), path_begin: uri3 }
    ]);
    fronts.insert("other.domain".to_owned(), vec![
      HttpFront { app_id: "app_1".to_owned(), hostname: "other.domain".to_owned(), path_begin: "/test".to_owned() },
    ]);

    let front: SocketAddr = FromStr::from_str("127.0.0.1:1030").expect("could not parse address");
    let listener = net::TcpListener::bind(&front).expect("should bind TCP socket");
    let server_config = ServerConfiguration {
      listener:  Some(listener),
      address:   front,
      applications: HashMap::new(),
      backends:  BackendMap::new(),
      fronts:    fronts,
      pool:      Rc::new(RefCell::new(Pool::with_capacity(1,0, || BufferQueue::with_capacity(16384)))),
      answers:   DefaultAnswers {
        NotFound: Vec::from(&b"HTTP/1.1 404 Not Found\r\n\r\n"[..]),
        ServiceUnavailable: Vec::from(&b"HTTP/1.1 503 your application is in deployment\r\n\r\n"[..]),
      },
      config: Default::default(),
    };

    let frontend1 = server_config.frontend_from_request("lolcatho.st", "/");
    let frontend2 = server_config.frontend_from_request("lolcatho.st", "/test");
    let frontend3 = server_config.frontend_from_request("lolcatho.st", "/yolo/test");
    let frontend4 = server_config.frontend_from_request("lolcatho.st", "/yolo/swag");
    let frontend5 = server_config.frontend_from_request("domain", "/");
    assert_eq!(frontend1.expect("should find frontend").app_id, "app_1");
    assert_eq!(frontend2.expect("should find frontend").app_id, "app_1");
    assert_eq!(frontend3.expect("should find frontend").app_id, "app_2");
    assert_eq!(frontend4.expect("should find frontend").app_id, "app_3");
    assert_eq!(frontend5, None);
  }
}
