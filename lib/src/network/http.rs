use std::collections::HashMap;
use std::io::{self,Read,Write,ErrorKind};
use std::rc::{Rc,Weak};
use std::cell::RefCell;
use std::thread::{self,Thread,Builder};
use std::sync::mpsc::{self,channel,Receiver};
use std::net::{SocketAddr,IpAddr,Shutdown};
use std::str::{FromStr, from_utf8, from_utf8_unchecked};
use mio::*;
use mio::net::*;
use mio::timer::Timeout;
use mio_uds::UnixStream;
use mio::unix::UnixReady;
use pool::{Pool,Checkout,Reset};
use uuid::Uuid;
use nom::{HexDisplay,IResult};
use rand::random;

use network::{Backend,ClientResult,ConnectionError,RequiredEvents,Protocol};
use network::buffer_queue::BufferQueue;
use network::protocol::{ProtocolResult,TlsHandshake,Http,Pipe};
use network::proxy::{Server,ProxyChannel};
use network::session::{BackendConnectAction,BackendConnectionStatus,ProxyClient,ProxyConfiguration,Readiness,ListenToken,FrontToken,BackToken,AcceptError,Session};
use network::socket::{SocketHandler,SocketResult,server_bind};
use messages::{self,Order,HttpFront,HttpProxyConfiguration,OrderMessage,OrderMessageAnswer,OrderMessageStatus};
use channel::Channel;
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
  front_timeout:  Option<Timeout>,
  back_timeout:   Option<Timeout>,
  instance:       Option<Rc<RefCell<Backend>>>,
  back_connected: BackendConnectionStatus,
  protocol:       Option<State>,
  pool:           Weak<RefCell<Pool<BufferQueue>>>,
}

impl Client {
  pub fn new(sock: TcpStream, pool: Weak<RefCell<Pool<BufferQueue>>>, public_address: Option<IpAddr>) -> Option<Client> {
    let protocol = if let Some(pool) = pool.upgrade() {
      let mut p = pool.borrow_mut();
      if let (Some(front_buf), Some(back_buf)) = (p.checkout(), p.checkout()) {
        Some(Http::new(unwrap_msg!(sock.try_clone()), front_buf, back_buf, public_address).expect("should create a HTTP state"))
      } else { None }
    } else { None };

    protocol.map(|http| {
      let request_id = Uuid::new_v4().hyphenated().to_string();
      let log_ctx    = format!("{}\tunknown\t", &request_id);
      let client = Client {
        backend:        None,
        token:          None,
        backend_token:  None,
        front_timeout:  None,
        back_timeout:   None,
        instance:       None,
        back_connected: BackendConnectionStatus::NotConnected,
        protocol:       Some(State::Http(http)),
        frontend:       sock,
        pool:           pool,
      };

    client
    })
  }

  pub fn upgrade(&mut self) {
    info!("HTTP::upgrade");
    let protocol = unwrap_msg!(self.protocol.take());
    if let State::Http(http) = protocol {
      info!("switching to pipe");
      let front_token = unwrap_msg!(http.front_token());
      let back_token  = unwrap_msg!(http.back_token());

      let mut pipe = Pipe::new(http.frontend, unwrap_msg!(http.backend),
        http.front_buf, http.back_buf, http.public_address).expect("could not create a Pipe instance");

      pipe.readiness.front_readiness = http.readiness.front_readiness;
      pipe.readiness.back_readiness  = http.readiness.back_readiness;
      pipe.set_front_token(front_token);
      pipe.set_back_token(back_token);

      self.protocol = Some(State::WebSocket(pipe));
    } else {
      self.protocol = Some(protocol);
    }
  }

  pub fn set_answer(&mut self, buf: &[u8])  {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http) => http.set_answer(buf),
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
        //successful connection, rest failure counter
        (*instance.borrow_mut()).failures = 0;
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

  fn front_timeout(&mut self) -> Option<Timeout> {
    self.front_timeout.take()
  }

  fn back_timeout(&mut self) -> Option<Timeout> {
    self.back_timeout.take()
  }

  fn set_front_timeout(&mut self, timeout: Timeout) {
    self.front_timeout = Some(timeout)
  }

  fn set_back_timeout(&mut self, timeout: Timeout) {
    self.back_timeout = Some(timeout)
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
      ClientResult::Continue
    }
  }

  fn back_hup(&mut self) -> ClientResult {
    if self.token == None {
      ClientResult::CloseClient
    } else {
      ClientResult::Continue
    }
  }

  // Read content from the client
  fn readable(&mut self) -> ClientResult {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http)      => http.readable(),
      State::WebSocket(ref mut pipe) => pipe.readable()
    }
  }

  // Forward content to client
  fn writable(&mut self) -> ClientResult {
    match  *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http)      => http.writable(),
      State::WebSocket(ref mut pipe) => pipe.writable()
    }
  }

  // Forward content to application
  fn back_writable(&mut self) -> ClientResult {
    match *unwrap_msg!(self.protocol.as_mut())  {
      State::Http(ref mut http)      => http.back_writable(),
      State::WebSocket(ref mut pipe) => pipe.back_writable()
    }
  }

  // Read content from application
  fn back_readable(&mut self) -> ClientResult {
    let (upgrade, result) = match  *unwrap_msg!(self.protocol.as_mut())  {
      State::Http(ref mut http)      => http.back_readable(),
      State::WebSocket(ref mut pipe) => (ProtocolResult::Continue, pipe.back_readable())
    };

    if upgrade == ProtocolResult::Continue {
      result
    } else {
      self.upgrade();
      match *unwrap_msg!(self.protocol.as_mut()) {
        State::WebSocket(ref mut pipe) => pipe.back_readable(),
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

pub type AppId    = String;
pub type Hostname = String;

pub struct ServerConfiguration {
  listener:        TcpListener,
  address:         SocketAddr,
  instances:       HashMap<AppId, Vec<Rc<RefCell<Backend>>>>,
  fronts:          HashMap<Hostname, Vec<HttpFront>>,
  pool:            Rc<RefCell<Pool<BufferQueue>>>,
  answers:         DefaultAnswers,
  front_timeout:   u64,
  back_timeout:    u64,
  config:          HttpProxyConfiguration,
}

impl ServerConfiguration {
  pub fn new(config: HttpProxyConfiguration, event_loop: &mut Poll, start_at:usize) -> io::Result<ServerConfiguration> {
    count!("pouet", 1);
    let front = config.front;
    match server_bind(&config.front) {
      Ok(sock) => {
        event_loop.register(&sock, Token(start_at), Ready::readable(), PollOpt::edge());

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

        Ok(ServerConfiguration {
          listener:      sock,
          address:       config.front,
          instances:     HashMap::new(),
          fronts:        HashMap::new(),
          pool:          Rc::new(RefCell::new(
                           Pool::with_capacity(2*config.max_connections, 0, || BufferQueue::with_capacity(config.buffer_size))
          )),
          //FIXME: make the timeout values configurable
          front_timeout: 5000,
          back_timeout:  5000,
          answers:       default,
          config:        config,
        })
      },
      Err(e) => {
        let formatted_err = format!("could not create listener {:?}: {:?}", front, e);
        error!("{}", formatted_err);
        //FIXME: return an error if listener creation failed
        //channel.write_message(&OrderMessageAnswer{id: String::from("listener_failed"), status: OrderMessageStatus::Error(formatted_err)});
        //channel.run();
        Err(e)
      }
    }
  }

  pub fn add_http_front(&mut self, http_front: HttpFront, event_loop: &mut Poll) {
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
  }

  pub fn remove_http_front(&mut self, front: HttpFront, event_loop: &mut Poll) {
    info!("removing http_front {:?}", front);
    if let Some(fronts) = self.fronts.get_mut(&front.hostname) {
      fronts.retain(|f| f != &front);
    }
  }

  pub fn add_instance(&mut self, app_id: &str, instance_address: &SocketAddr, event_loop: &mut Poll) {
    if let Some(addrs) = self.instances.get_mut(app_id) {
      let backend = Rc::new(RefCell::new(Backend::new(*instance_address)));
      if !addrs.contains(&backend) {
        addrs.push(backend);
      }
    }

    if self.instances.get(app_id).is_none() {
      let backend = Backend::new(*instance_address);
      self.instances.insert(String::from(app_id), vec![Rc::new(RefCell::new(backend))]);
    }
  }

  pub fn remove_instance(&mut self, app_id: &str, instance_address: &SocketAddr, event_loop: &mut Poll) {
      if let Some(instances) = self.instances.get_mut(app_id) {
        instances.retain(|backend| &(*backend.borrow()).address != instance_address);
      } else {
        error!("Instance was already removed");
      }
  }

  pub fn frontend_from_request(&self, host: &str, uri: &str) -> Option<&HttpFront> {
    let host: &str = if let IResult::Done(i, (hostname, port)) = hostname_and_port(host.as_bytes()) {
      if i != &b""[..] {
        error!("invalid remaining chars after hostname");
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

  pub fn backend_from_app_id(&mut self, client: &mut Client, app_id: &str) -> Result<TcpStream,ConnectionError> {
    // FIXME: the app id clone here is probably very inefficient
    //if let Some(app_id) = self.frontend_from_request(host, uri).map(|ref front| front.http.app_id.clone()) {
    client.http().map(|h| h.app_id = Some(String::from(app_id)));
    //FIXME: round-robin on instances
    if let Some(ref mut app_instances) = self.instances.get_mut(app_id) {
      if app_instances.len() == 0 {
        client.set_answer(&self.answers.ServiceUnavailable);
        return Err(ConnectionError::NoBackendAvailable);
      }

      //FIXME: hardcoded for now, these should come from configuration
      let max_failures_per_backend:usize = 10;
      let max_failures:usize             = 3;


      for _ in 0..max_failures {
        //FIXME: it's probably pretty wasteful to refilter every time here
        let mut instances:Vec<&mut Rc<RefCell<Backend>>> = app_instances.iter_mut().filter(|backend| (*backend.borrow()).can_open(max_failures_per_backend)).collect();
        if instances.is_empty() {
          error!("no more available backends for app {}", app_id);
          return Err(ConnectionError::NoBackendAvailable);
        }
        let rnd = random::<usize>();
        let idx = rnd % instances.len();

        let conn = instances.get_mut(idx).ok_or(ConnectionError::NoBackendAvailable).and_then(|ref mut b| {
          let ref mut backend = *b.borrow_mut();
          info!("{}\tConnecting {} -> {:?}", client.http().map(|h| h.log_ctx.clone()).unwrap_or("".to_string()), app_id, (backend.address, backend.active_connections, backend.failures));
          let conn = backend.try_connect(max_failures_per_backend);
          if backend.failures >= max_failures_per_backend {
            error!("{}\tbackend {:?} connections failed {} times, disabling it", client.http().map(|h| h.log_ctx.clone()).unwrap_or("".to_string()), (backend.address, backend.active_connections), backend.failures);
          }

          if conn.is_ok() {
            client.back_connected = BackendConnectionStatus::Connecting;
            client.instance = Some(b.clone());
          }

          conn
        });

        if conn.is_ok() {
          return conn;
        }
      }
      Err(ConnectionError::NoBackendAvailable)
    } else {
      Err(ConnectionError::NoBackendAvailable)
    }
  }
}

impl ProxyConfiguration<Client> for ServerConfiguration {
  fn connect_to_backend(&mut self, event_loop: &mut Poll, client: &mut Client) -> Result<BackendConnectAction,ConnectionError> {
    let h = try!(client.http().unwrap().state.as_ref().unwrap().get_host().ok_or(ConnectionError::NoHostGiven));

    let host: &str = if let IResult::Done(i, (hostname, port)) = hostname_and_port(h.as_bytes()) {
      if i != &b""[..] {
        error!("invalid remaining chars after hostname");
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

    //FIXME: too many unwraps here
    let rl     = try!(client.http().unwrap().state.as_ref().unwrap().get_request_line().ok_or(ConnectionError::NoRequestLineGiven));
    if let Some(app_id) = self.frontend_from_request(&host, &rl.uri).map(|ref front| front.app_id.clone()) {
      if (client.http().map(|h| h.app_id.as_ref()).unwrap_or(None) == Some(&app_id)) && client.back_connected == BackendConnectionStatus::Connected {
        //matched on keepalive
        return Ok(BackendConnectAction::Reuse);
      }

      // circuit breaker
      if client.back_connected == BackendConnectionStatus::Connecting {
        client.instance.as_ref().map(|instance| {
          let ref mut backend = *instance.borrow_mut();
          backend.dec_connections();
          backend.failures += 1;
        });
      }

      let reused = client.http().map(|http| http.app_id.is_some()).unwrap_or(false);
      //deregister back socket if it is the wrong one or if it was not connecting
      if reused || client.back_connected == BackendConnectionStatus::Connecting {
        client.instance = None;
        client.back_connected = BackendConnectionStatus::NotConnected;
        client.readiness().back_interest  = UnixReady::from(Ready::empty());
        client.readiness().back_readiness = UnixReady::from(Ready::empty());
        let sock = unwrap_msg!(client.backend.as_ref());
        event_loop.deregister(sock);
        sock.shutdown(Shutdown::Both);
      }

      let conn   = self.backend_from_app_id(client, &app_id);
      match conn {
        Ok(socket) => {
          socket.set_nodelay(true);
          client.set_back_socket(socket);
          client.readiness().back_interest.insert(Ready::writable());
          client.readiness().back_interest.insert(UnixReady::hup());
          client.readiness().back_interest.insert(UnixReady::error());
          if reused {
            Ok(BackendConnectAction::Replace)
          } else {
            Ok(BackendConnectAction::New)
          }
          //Ok(())
        },
        Err(ConnectionError::NoBackendAvailable) => {
          client.set_answer(&self.answers.ServiceUnavailable);
          client.readiness().front_interest = UnixReady::from(Ready::writable()) | UnixReady::hup() | UnixReady::error();
          Err(ConnectionError::NoBackendAvailable)
        }
        Err(ConnectionError::HostNotFound) => {
          client.set_answer(&self.answers.NotFound);
          client.readiness().front_interest = UnixReady::from(Ready::writable()) | UnixReady::hup() | UnixReady::error();
          Err(ConnectionError::HostNotFound)
        }
        e => panic!(e)
      }
    } else {
      client.set_answer(&self.answers.NotFound);
      client.readiness().front_interest = UnixReady::from(Ready::writable()) | UnixReady::hup() | UnixReady::error();
      Err(ConnectionError::HostNotFound)
    }
  }

  fn notify(&mut self, event_loop: &mut Poll, message: OrderMessage) -> OrderMessageAnswer {
    // ToDo temporary
    //trace!("{} notified", message);
    match message.order {
      Order::AddHttpFront(front) => {
        info!("{} add front {:?}", message.id, front);
        self.add_http_front(front, event_loop);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      Order::RemoveHttpFront(front) => {
        info!("{} front {:?}", message.id, front);
        self.remove_http_front(front, event_loop);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      Order::AddInstance(instance) => {
        info!("{} add instance {:?}", message.id, instance);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let parsed:Option<SocketAddr> = addr_string.parse().ok();
        if let Some(addr) = parsed {
          self.add_instance(&instance.app_id, &addr, event_loop);
          OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
        } else {
          OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Error(String::from("cannot parse the address")), data: None }
        }
      },
      Order::RemoveInstance(instance) => {
        info!("{} remove instance {:?}", message.id, instance);
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
        info!("{} modifying proxy configuration: {:?}", message.id, configuration);
        self.front_timeout = configuration.front_timeout;
        self.back_timeout  = configuration.back_timeout;
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
        event_loop.deregister(&self.listener);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Processing, data: None }
      },
      Order::HardStop => {
        info!("{} hard shutdown", message.id);
        //FIXME: handle shutdown
        //event_loop.shutdown();
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      Order::Status => {
        info!("{} status", message.id);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      command => {
        debug!("{} unsupported message, ignoring: {:?}", message.id, command);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Error(String::from("unsupported message")), data: None }
      }
    }
  }

  fn accept(&mut self, token: ListenToken) -> Result<(Client, bool), AcceptError> {
    self.listener.accept().map_err(|e| {
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
  }

  fn close_backend(&mut self, app_id: String, addr: &SocketAddr) {
    if let Some(app_instances) = self.instances.get_mut(&app_id) {
      if let Some(ref mut backend) = app_instances.iter_mut().find(|backend| &(*backend.borrow()).address == addr) {
        (*backend.borrow_mut()).dec_connections();
      }
    }
  }

  fn front_timeout(&self) -> u64 {
    self.front_timeout
  }

  fn back_timeout(&self)  -> u64 {
    self.back_timeout
  }
}

pub type HttpServer = Session<ServerConfiguration,Client>;

pub fn start(config: HttpProxyConfiguration, channel: ProxyChannel) {
  let mut event_loop  = Poll::new().expect("could not create event loop");
  let max_connections = config.max_connections;
  let max_listeners   = 1;

  // start at max_listeners + 1 because token(0) is the channel, and token(1) is the timer
  if let Ok(configuration) = ServerConfiguration::new(config, &mut event_loop, 1 + max_listeners) {
    let mut session = Session::new(max_listeners, max_connections, 0, configuration, &mut event_loop);
    let mut server  = Server::new(event_loop, channel, Some(session), None, None);

    info!("starting event loop");
    server.run();
    info!("ending event loop");
  }
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
  use messages::{Order,HttpFront,Instance,HttpProxyConfiguration,OrderMessage,OrderMessageAnswer};
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
      max_connections: 10,
      buffer_size: 16384,
      ..Default::default()
    };

    let (mut command, channel) = Channel::generate(1000, 10000).expect("should create a channel");
    let jg = thread::spawn(move || {
      start(config, channel);
    });

    let front = HttpFront { app_id: String::from("app_1"), hostname: String::from("localhost"), path_begin: String::from("/") };
    command.write_message(&OrderMessage { id: String::from("ID_ABCD"), order: Order::AddHttpFront(front) });
    let instance = Instance { app_id: String::from("app_1"), ip_address: String::from("127.0.0.1"), port: 1025 };
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
        assert_eq!(sz, 204);
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
      max_connections: 10,
      buffer_size: 16384,
      ..Default::default()
    };

    let (mut command, channel) = Channel::generate(1000, 10000).expect("should create a channel");

    let jg = thread::spawn(move|| {
      start(config, channel);
    });

    let front = HttpFront { app_id: String::from("app_1"), hostname: String::from("localhost"), path_begin: String::from("/") };
    command.write_message(&OrderMessage { id: String::from("ID_ABCD"), order: Order::AddHttpFront(front) });
    let instance = Instance { app_id: String::from("app_1"), ip_address: String::from("127.0.0.1"), port: 1028 };
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
        assert_eq!(sz, 204);
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
        assert_eq!(sz, 204);
        //assert!(false);
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

  use mio::tcp;
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
    let listener = tcp::TcpListener::bind(&front).expect("should bind TCP socket");
    let server_config = ServerConfiguration {
      listener:  listener,
      address:   front,
      instances: HashMap::new(),
      fronts:    fronts,
      pool:      Rc::new(RefCell::new(Pool::with_capacity(1,0, || BufferQueue::with_capacity(16384)))),
      front_timeout: 50000,
      back_timeout:  50000,
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
