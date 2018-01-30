use std::collections::HashMap;
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

use sozu_command::channel::Channel;
use sozu_command::state::ConfigState;
use sozu_command::scm_socket::ScmSocket;
use sozu_command::messages::{self,Application,Order,HttpFront,HttpProxyConfiguration,OrderMessage,OrderMessageAnswer,OrderMessageStatus};

use network::{AppId,Backend,ClientResult,ConnectionError,RequiredEvents,Protocol};
use network::backends::BackendMap;
use network::buffer_queue::BufferQueue;
use network::protocol::{ProtocolResult,StickySession,TlsHandshake,Http,Pipe};
use network::protocol::http::DefaultAnswerStatus;
use network::proxy::{Server,ProxyChannel};
use network::session::{BackendConnectAction,BackendConnectionStatus,ProxyClient,ProxyConfiguration,Readiness,ListenToken,FrontToken,BackToken,AcceptError,Session,SessionMetrics};
use network::socket::{SocketHandler,SocketResult,server_bind};
use network::retry::RetryPolicy;
use parser::http11::hostname_and_port;
use util::UnwrapLog;

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
  pub frontend:   TcpStream,
  backend:        Option<TcpStream>,
  token:          Option<Token>,
  backend_token:  Option<Token>,
  instance:       Option<Rc<RefCell<Backend>>>,
  back_connected: BackendConnectionStatus,
  protocol:       Option<State>,
  pool:           Weak<RefCell<Pool<BufferQueue>>>,
  sticky_session: bool,
  metrics:        SessionMetrics,
}

impl Client {
  pub fn new(sock: TcpStream, pool: Weak<RefCell<Pool<BufferQueue>>>, public_address: Option<IpAddr>) -> Option<Client> {
    let protocol = if let Some(pool) = pool.upgrade() {
      let mut p = pool.borrow_mut();
      if let (Some(front_buf), Some(back_buf)) = (p.checkout(), p.checkout()) {
        Some(Http::new(unwrap_msg!(sock.try_clone()), front_buf, back_buf, public_address,
          Protocol::HTTP).expect("should create a HTTP state"))
      } else { None }
    } else { None };

    protocol.map(|http| {
      let request_id = Uuid::new_v4().hyphenated().to_string();
      let log_ctx    = format!("{}\tunknown\t", &request_id);
      let client = Client {
        backend:        None,
        token:          None,
        backend_token:  None,
        instance:       None,
        back_connected: BackendConnectionStatus::NotConnected,
        protocol:       Some(State::Http(http)),
        frontend:       sock,
        pool:           pool,
        sticky_session: false,
        metrics:        SessionMetrics::new(),
      };

    client
    })
  }

  pub fn upgrade(&mut self) {
    debug!("HTTP::upgrade");
    let protocol = unwrap_msg!(self.protocol.take());
    if let State::Http(http) = protocol {
      debug!("switching to pipe");
      let front_token = unwrap_msg!(http.front_token());
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
}

impl ProxyClient for Client {
  fn front_socket(&self) -> &TcpStream {
    self.frontend.socket_ref()
  }

  fn back_socket(&self)  -> Option<&TcpStream> {
    self.backend.as_ref()
  }

  fn front_token(&self)  -> Option<Token> {
    self.token
  }

  fn back_token(&self)   -> Option<Token> {
    self.backend_token
  }

  fn back_connected(&self)     -> BackendConnectionStatus {
    self.back_connected
  }

  fn set_back_connected(&mut self, connected: BackendConnectionStatus) {
    self.back_connected = connected;
    if connected == BackendConnectionStatus::Connected {
      self.instance.as_ref().map(|instance| {
        let ref mut backend = *instance.borrow_mut();
        //successful connection, rest failure counter
        backend.failures = 0;
        backend.retry_policy.succeed();
      });
    }
  }

  fn close(&mut self) {
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

  fn metrics(&mut self)        -> &mut SessionMetrics {
    &mut self.metrics
  }

  fn set_back_socket(&mut self, socket: TcpStream) {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http)      => http.set_back_socket(unwrap_msg!(socket.try_clone())),
      State::WebSocket(ref mut pipe) => {} /*pipe.set_back_socket(unwrap_msg!(socket.try_clone()))*/
    }
    self.backend         = Some(socket);
  }

  fn set_front_token(&mut self, token: Token) {
    self.token         = Some(token);
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

  fn readiness(&mut self) -> &mut Readiness {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http)      => &mut http.readiness,
      State::WebSocket(ref mut pipe) => &mut pipe.readiness
    }
  }

  fn protocol(&self)           -> Protocol {
    Protocol::HTTP
  }

  fn remove_backend(&mut self) -> (Option<String>, Option<SocketAddr>) {
    debug!("{}\tPROXY [{} -> {}] CLOSED BACKEND", self.http().map(|h| h.log_ctx.clone()).unwrap_or("".to_string()), unwrap_msg!(self.token).0,
      unwrap_msg!(self.backend_token).0);
    let addr:Option<SocketAddr> = self.backend.as_ref().and_then(|sock| sock.peer_addr().ok());
    self.backend       = None;
    self.backend_token = None;
    (unwrap_msg!(self.http()).app_id.clone(), addr)
  }

  fn front_hup(&mut self) -> ClientResult {
    if self.backend_token == None {
      ClientResult::CloseClient
    } else {
      ClientResult::CloseBoth
    }
  }

  fn back_hup(&mut self) -> ClientResult {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http)      => http.back_hup(),
      State::WebSocket(ref mut pipe) => pipe.back_hup()
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
}

type ClientToken = Token;

#[allow(non_snake_case)]
pub struct DefaultAnswers {
  pub NotFound:           Vec<u8>,
  pub ServiceUnavailable: Vec<u8>
}

pub type Hostname = String;

pub struct ServerConfiguration {
  listener:        Option<TcpListener>,
  address:         SocketAddr,
  instances:       BackendMap,
  fronts:          HashMap<Hostname, Vec<HttpFront>>,
  applications:    HashMap<AppId, Application>,
  pool:            Rc<RefCell<Pool<BufferQueue>>>,
  answers:         DefaultAnswers,
  config:          HttpProxyConfiguration,
}

impl ServerConfiguration {
  pub fn new(config: HttpProxyConfiguration, event_loop: &mut Poll, start_at:usize,
    pool: Rc<RefCell<Pool<BufferQueue>>>, tcp_listener: Option<TcpListener>) -> ServerConfiguration {

    let front = config.front;

    let listener = tcp_listener.or_else(|| server_bind(&config.front).map_err(|e| {
      error!("could not create listener {:?}: {:?}", front, e);
    }).ok());

    if let Some(ref sock) = listener {
      event_loop.register(sock, Token(start_at), Ready::readable(), PollOpt::edge());
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

    ServerConfiguration {
      listener:       listener,
      address:        config.front,
      applications:   HashMap::new(),
      instances:      BackendMap::new(),
      fronts:         HashMap::new(),
      pool:           pool,
      answers:        default,
      config:         config,
    }

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

  pub fn add_instance(&mut self, app_id: &str, instance_id: &str, instance_address: &SocketAddr, event_loop: &mut Poll) {
    self.instances.add_instance(app_id, instance_id, instance_address);
  }

  pub fn remove_instance(&mut self, app_id: &str, instance_address: &SocketAddr, event_loop: &mut Poll) {
    self.instances.remove_instance(app_id, instance_address);
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
      error!("hostname parsing failed");
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

    match self.instances.backend_from_app_id(app_id) {
      Err(e) => {
        client.set_answer(DefaultAnswerStatus::Answer503, &self.answers.ServiceUnavailable);
        Err(e)
      },
      Ok((backend, conn))  => {
        client.back_connected = BackendConnectionStatus::Connecting;
        if front_should_stick {
          client.http().map(|http| http.sticky_session = Some(StickySession::new(backend.borrow().id.clone())));
        }
        client.metrics.backend_id = Some(backend.borrow().instance_id.clone());
        client.metrics.backend_start();
        client.instance = Some(backend);

        Ok(conn)
      }
    }
  }

  pub fn backend_from_sticky_session(&mut self, client: &mut Client, app_id: &str, sticky_session: u32) -> Result<TcpStream,ConnectionError> {
    client.http().map(|h| h.set_app_id(String::from(app_id)));

    match self.instances.backend_from_sticky_session(app_id, sticky_session) {
      Err(e) => {
        debug!("Couldn't find a backend corresponding to sticky_session {} for app {}", sticky_session, app_id);
        client.set_answer(DefaultAnswerStatus::Answer503, &self.answers.ServiceUnavailable);
        Err(e)
      },
      Ok((backend, conn))  => {
        client.back_connected = BackendConnectionStatus::Connecting;
        client.http().map(|http| http.sticky_session = Some(StickySession::new(backend.borrow().id.clone())));
        client.metrics.backend_id = Some(backend.borrow().instance_id.clone());
        client.metrics.backend_start();
        client.instance = Some(backend);

        Ok(conn)
      }
    }
  }
}

impl ProxyConfiguration<Client> for ServerConfiguration {
  fn connect_to_backend(&mut self, event_loop: &mut Poll, client: &mut Client) -> Result<BackendConnectAction,ConnectionError> {
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
      error!("hostname parsing failed");
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
        return Err(ConnectionError::HttpsRedirect);
      }

      let front_should_stick = self.applications.get(&app_id).map(|ref app| app.sticky_session).unwrap_or(false);

      if (client.http().map(|h| h.app_id.as_ref()).unwrap_or(None) == Some(&app_id)) && client.back_connected == BackendConnectionStatus::Connected {
        if client.instance.as_ref().map(|instance| {
          let ref backend = *instance.borrow();
          self.instances.has_backend(&app_id, backend)
        }).unwrap_or(false) {
          //matched on keepalive
          client.metrics.backend_id = client.instance.as_ref().map(|i| i.borrow().instance_id.clone());
          client.metrics.backend_start();
          return Ok(BackendConnectAction::Reuse);
        } else {

          client.instance = None;
          client.back_connected = BackendConnectionStatus::NotConnected;
          //client.readiness().back_interest  = UnixReady::from(Ready::empty());
          client.readiness().back_readiness = UnixReady::from(Ready::empty());
          client.back_socket().as_ref().map(|sock| {
            event_loop.deregister(*sock);
            sock.shutdown(Shutdown::Both);
          });
        }
      }

      // circuit breaker
      if client.back_connected == BackendConnectionStatus::Connecting {
        client.instance.as_ref().map(|instance| {
          let ref mut backend = *instance.borrow_mut();
          backend.dec_connections();
          backend.failures += 1;
          backend.retry_policy.fail();
        });

        //deregister back socket if it is the wrong one or if it was not connecting
        client.instance = None;
        client.back_connected = BackendConnectionStatus::NotConnected;
        client.readiness().back_interest  = UnixReady::from(Ready::empty());
        client.readiness().back_readiness = UnixReady::from(Ready::empty());
        client.back_socket().as_ref().map(|sock| {
          event_loop.deregister(*sock);
          sock.shutdown(Shutdown::Both);
        });
      }

      let old_app_id = client.http().and_then(|ref http| http.app_id.clone());

      let conn = match (front_should_stick, sticky_session) {
        (true, Some(session)) => self.backend_from_sticky_session(client, &app_id, session),
        _ => self.backend_from_app_id(client, &app_id, front_should_stick),
      };

      match conn {
        Ok(socket) => {
          let new_app_id = client.http().and_then(|ref http| http.app_id.clone());

          if old_app_id.is_some() && old_app_id != new_app_id {
            client.instance = None;
            client.back_connected = BackendConnectionStatus::NotConnected;
            client.readiness().back_readiness = UnixReady::from(Ready::empty());
            client.back_socket().as_ref().map(|sock| {
              event_loop.deregister(*sock);
              sock.shutdown(Shutdown::Both);
            });
            // we still want to use the new socket
            client.readiness().back_interest  = UnixReady::from(Ready::writable());
          }

          socket.set_nodelay(true);
          client.set_back_socket(socket);
          client.readiness().back_interest.insert(Ready::writable());
          client.readiness().back_interest.insert(UnixReady::hup());
          client.readiness().back_interest.insert(UnixReady::error());
          if old_app_id == new_app_id {
            Ok(BackendConnectAction::Replace)
          } else {
            Ok(BackendConnectAction::New)
          }
          //Ok(())
        },
        Err(ConnectionError::NoBackendAvailable) => {
          client.set_answer(DefaultAnswerStatus::Answer503, &self.answers.ServiceUnavailable);
          client.readiness().front_interest = UnixReady::from(Ready::writable()) | UnixReady::hup() | UnixReady::error();
          Err(ConnectionError::NoBackendAvailable)
        }
        Err(ConnectionError::HostNotFound) => {
          client.set_answer(DefaultAnswerStatus::Answer404, &self.answers.NotFound);
          client.readiness().front_interest = UnixReady::from(Ready::writable()) | UnixReady::hup() | UnixReady::error();
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
      Order::AddInstance(instance) => {
        debug!("{} add instance {:?}", message.id, instance);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let parsed:Option<SocketAddr> = addr_string.parse().ok();
        if let Some(addr) = parsed {
          self.add_instance(&instance.app_id, &instance.instance_id, &addr, event_loop);
          OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
        } else {
          OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Error(String::from("cannot parse the address")), data: None }
        }
      },
      Order::RemoveInstance(instance) => {
        debug!("{} remove instance {:?}", message.id, instance);
        remove_backend_metrics!(&instance.instance_id);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let parsed:Option<SocketAddr> = addr_string.parse().ok();
        if let Some(addr) = parsed {
          self.remove_instance(&instance.app_id, &addr, event_loop);
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

  fn accept(&mut self, token: ListenToken) -> Result<(Client, bool), AcceptError> {
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
        if let Some(mut c) = Client::new(frontend_sock, Rc::downgrade(&self.pool), self.config.public_address) {
          c.readiness().front_interest.insert(Ready::readable());
          c.readiness().back_interest.remove(Ready::readable() | Ready::writable());
          Ok((c, false))
        } else {
          Err(AcceptError::TooManyClients)
        }
      })
    } else {
      error!("cannot accept connections, no listening socket available");
      Err(AcceptError::IoError)
    }
  }

  fn close_backend(&mut self, app_id: String, addr: &SocketAddr) {
    self.instances.close_backend_connection(&app_id, &addr);
  }
}

pub struct InitialServerConfiguration {
  instances:       BackendMap,
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

    let mut instances = BackendMap::new();
    instances.import_configuration_state(&state.instances);

    InitialServerConfiguration {
      instances:    instances,
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
          instances:     self.instances,
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


pub type HttpServer = Session<ServerConfiguration,Client>;

pub fn start(config: HttpProxyConfiguration, channel: ProxyChannel, max_buffers: usize, buffer_size: usize) {
  let mut event_loop  = Poll::new().expect("could not create event loop");
  let max_listeners   = 1;

  let pool = Rc::new(RefCell::new(
    Pool::with_capacity(2*max_buffers, 0, || BufferQueue::with_capacity(buffer_size))
  ));
  // start at max_listeners + 1 because token(0) is the channel, and token(1) is the timer
  let configuration = ServerConfiguration::new(config, &mut event_loop, 1 + max_listeners, pool, None);
  let session       = Session::new(max_listeners, max_buffers, 0, configuration, &mut event_loop);
  let (scm_server, scm_client) = UnixStream::pair().unwrap();
  let mut server    = Server::new(event_loop, channel, ScmSocket::new(scm_server.as_raw_fd()),
    Some(session), None, None, None);

  info!("starting event loop");
  server.run();
  info!("ending event loop");
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
  use sozu_command::messages::{Order,HttpFront,Instance,HttpProxyConfiguration,OrderMessage,OrderMessageAnswer};
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
      start(config, channel, 10, 16384);
    });

    let front = HttpFront { app_id: String::from("app_1"), hostname: String::from("localhost"), path_begin: String::from("/") };
    command.write_message(&OrderMessage { id: String::from("ID_ABCD"), order: Order::AddHttpFront(front) });
    let instance = Instance { app_id: String::from("app_1"),instance_id: String::from("app_1-0"), ip_address: String::from("127.0.0.1"), port: 1025 };
    command.write_message(&OrderMessage { id: String::from("ID_EFGH"), order: Order::AddInstance(instance) });

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
    let instance = Instance { app_id: String::from("app_1"), instance_id: String::from("app_1-0"), ip_address: String::from("127.0.0.1"), port: 1028 };
    command.write_message(&OrderMessage { id: String::from("ID_EFGH"), order: Order::AddInstance(instance) });

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

    let application = Application { app_id: String::from("app_1"), sticky_session: false, https_redirect: true };
    command.write_message(&OrderMessage { id: String::from("ID_ABCD"), order: Order::AddApplication(application) });
    let front = HttpFront { app_id: String::from("app_1"), hostname: String::from("localhost"), path_begin: String::from("/") };
    command.write_message(&OrderMessage { id: String::from("ID_EFGH"), order: Order::AddHttpFront(front) });
    let instance = Instance { app_id: String::from("app_1"),instance_id: String::from("app_1-0"), ip_address: String::from("127.0.0.1"), port: 1040 };
    command.write_message(&OrderMessage { id: String::from("ID_IJKL"), order: Order::AddInstance(instance) });

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
      instances: BackendMap::new(),
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
