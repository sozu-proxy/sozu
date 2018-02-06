use std::thread::{self,Thread,Builder};
use std::sync::mpsc::{self,channel,Receiver};
use mio::net::*;
use mio::*;
use mio_uds::UnixStream;
use mio::unix::UnixReady;
use std::collections::{HashMap,HashSet};
use std::os::unix::io::RawFd;
use std::os::unix::io::{FromRawFd,AsRawFd};
use std::io::{self,Read,ErrorKind};
use nom::HexDisplay;
use std::error::Error;
use slab::{Slab,Entry,VacantEntry};
use std::rc::Rc;
use std::cell::{RefCell,RefMut};
use std::net::{SocketAddr,Shutdown};
use std::str::FromStr;
use time::{Duration,precise_time_s};
use rand::random;
use uuid::Uuid;
use pool::{Pool,Checkout,Reset};

use sozu_command::buffer::Buffer;
use sozu_command::channel::Channel;
use sozu_command::scm_socket::ScmSocket;
use sozu_command::messages::{self,TcpFront,Order,Instance,OrderMessage,OrderMessageAnswer,OrderMessageStatus};

use network::{AppId,Backend,ClientResult,ConnectionError,RequiredEvents,Protocol};
use network::proxy::{Server,ProxyChannel};
use network::session::{BackendConnectAction,BackendConnectionStatus,ProxyClient,ProxyConfiguration,Readiness,ListenToken,ClientToken,AcceptError,Session,SessionMetrics};
use network::buffer_queue::BufferQueue;
use network::socket::{SocketHandler,SocketResult,server_bind};
use network::protocol::Pipe;

use util::UnwrapLog;


const SERVER: Token = Token(0);

#[derive(Debug,Clone,PartialEq,Eq)]
pub enum ConnectionStatus {
  Initial,
  ClientConnected,
  Connected,
  ClientClosed,
  ServerClosed,
  Closed
}

pub struct Client {
  sock:           TcpStream,
  backend:        Option<TcpStream>,
  token:          Option<Token>,
  backend_token:  Option<Token>,
  accept_token:   ListenToken,
  status:         ConnectionStatus,
  rx_count:       usize,
  tx_count:       usize,
  app_id:         Option<String>,
  request_id:     String,
  metrics:        SessionMetrics,
  protocol:       Pipe<TcpStream>,
}

impl Client {
  fn new(sock: TcpStream, accept_token: ListenToken, front_buf: Checkout<BufferQueue>,
    back_buf: Checkout<BufferQueue>) -> Client {
    let s = sock.try_clone().expect("could not clone the socket");
    let addr = sock.peer_addr().map(|s| s.ip()).ok();

    Client {
      sock:           sock,
      backend:        None,
      token:          None,
      backend_token:  None,
      accept_token:   accept_token,
      status:         ConnectionStatus::Connected,
      rx_count:       0,
      tx_count:       0,
      app_id:         None,
      request_id:     Uuid::new_v4().hyphenated().to_string(),
      metrics:        SessionMetrics::new(),
      protocol:       Pipe::new(s, None, front_buf, back_buf, addr),
    }
  }

  fn log_request(&self) {
    let client = match self.sock.peer_addr().ok() {
      None => String::from("-"),
      Some(SocketAddr::V4(addr)) => format!("{}", addr),
      Some(SocketAddr::V6(addr)) => format!("{}", addr),
    };

    let backend = match self.backend.as_ref().and_then(|backend| backend.peer_addr().ok()) {
      None => String::from("-"),
      Some(SocketAddr::V4(addr)) => format!("{}", addr),
      Some(SocketAddr::V6(addr)) => format!("{}", addr),
    };

    let response_time = self.metrics.response_time().num_milliseconds();
    let service_time  = self.metrics.service_time().num_milliseconds();
    let app_id = self.app_id.clone().unwrap_or(String::from("-"));
    record_request_time!(&app_id, response_time);

    if let Some(backend_id) = self.metrics.backend_id.as_ref() {
      if let Some(backend_response_time) = self.metrics.backend_response_time() {
        record_backend_metrics!(backend_id, backend_response_time.num_milliseconds(),
          self.metrics.backend_bin, self.metrics.backend_bout);
      }
    }

    info!("{}{} -> {}\t{} {} {} {}",
      self.log_context(), client, backend,
      response_time, service_time, self.metrics.bin, self.metrics.bout);
  }

  fn front_hup(&mut self) -> ClientResult {
    self.log_request();
    self.protocol.front_hup()
  }

  fn back_hup(&mut self) -> ClientResult {
    self.log_request();
    self.protocol.back_hup()
  }

  fn log_context(&self) -> String {
    if let Some(ref app_id) = self.app_id {
      format!("{}\t{}\t", self.request_id, app_id)
    } else {
      format!("{}\tunknown\t", self.request_id)
    }
  }

  fn readable(&mut self) -> ClientResult {
    self.protocol.readable(&mut self.metrics)
  }

  fn writable(&mut self) -> ClientResult {
    self.protocol.writable(&mut self.metrics)
  }

  fn back_readable(&mut self) -> ClientResult {
    self.protocol.back_readable(&mut self.metrics)
  }

  fn back_writable(&mut self) -> ClientResult {
    self.protocol.back_writable(&mut self.metrics)
  }

  fn front_socket(&self) -> &TcpStream {
    self.protocol.front_socket()
  }

  fn front_token(&self)  -> Option<Token> {
    self.protocol.front_token()
  }

  fn back_token(&self)   -> Option<Token> {
    self.protocol.back_token()
  }

  fn back_socket(&self)  -> Option<&TcpStream> {
    self.protocol.back_socket()
  }

  fn set_back_socket(&mut self, socket: TcpStream) {
    self.protocol.set_back_socket(socket);
  }

  fn set_front_token(&mut self, token: Token) {
    self.token         = Some(token);
    self.protocol.set_front_token(token);
  }

  fn set_back_token(&mut self, token: Token) {
    self.backend_token = Some(token);
    self.protocol.set_back_token(token);
  }

  fn back_connected(&self)     -> BackendConnectionStatus {
    //FIXME: handle backends correctly when refactoring the TCP proxy
    BackendConnectionStatus::Connected
  }

  fn set_back_connected(&mut self, _: BackendConnectionStatus) {
  }

  fn metrics(&mut self)        -> &mut SessionMetrics {
    &mut self.metrics
  }

  fn remove_backend(&mut self) -> (Option<String>, Option<SocketAddr>) {

    let addr = self.backend.as_ref().and_then(|sock| sock.peer_addr().ok());
    self.backend       = None;
    self.backend_token = None;
    (self.app_id.clone(), addr)
  }

  fn readiness(&mut self) -> &mut Readiness {
    self.protocol.readiness()
  }

}

impl ProxyClient for Client {
  fn close(&mut self, poll: &mut Poll, configuration: &mut ProxyConfiguration<Client>) -> Vec<Token> {
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
    if let Some(tk) = self.token {
      res.push(tk)
    }
    if let Some(tk) = self.backend_token {
      res.push(tk)
    }

    res
  }

  fn protocol(&self)           -> Protocol {
    Protocol::TCP
  }

  fn process_events(&mut self, token: Token, events: Ready) {
    if self.token == Some(token) {
      self.readiness().front_readiness = self.readiness().front_readiness | UnixReady::from(events);
    } else if self.backend_token == Some(token) {
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

    let token = self.token.clone();
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
          error!("PROXY client {:?} front error, disconnecting", self.token);
        } else {
          error!("PROXY client {:?} back error, disconnecting", self.token);
        }

        self.readiness().front_interest = UnixReady::from(Ready::empty());
        self.readiness().back_interest  = UnixReady::from(Ready::empty());
        return ClientResult::CloseClient;
      }

      counter += 1;
    }

    if counter == max_loop_iterations {
      error!("PROXY\thandling client {:?} went through {} iterations, there's a probable infinite loop bug, closing the connection", self.token, max_loop_iterations);

      let front_interest = self.readiness().front_interest & self.readiness().front_readiness;
      let back_interest  = self.readiness().back_interest & self.readiness().back_readiness;

      let token = self.token.clone();
      error!("PROXY\t{:?} readiness: {:?} | front: {:?} | back: {:?} ", token,
        self.readiness(), front_interest, back_interest);

      return ClientResult::CloseClient;
    }

    ClientResult::Continue
  }
}

pub struct ApplicationListener {
  app_id:         String,
  sock:           Option<TcpListener>,
  token:          Option<Token>,
  front_address:  SocketAddr,
  back_addresses: Vec<SocketAddr>
}

pub struct ServerConfiguration {
  fronts:          HashMap<String, ListenToken>,
  instances:       HashMap<String, Vec<Backend>>,
  listeners:       Slab<ApplicationListener,ListenToken>,
  pool:            Rc<RefCell<Pool<BufferQueue>>>,
  base_token:      usize,
}

impl ServerConfiguration {
  pub fn new(max_listeners: usize, start_at: usize, event_loop: &mut Poll, pool: Rc<RefCell<Pool<BufferQueue>>>,
    mut tcp_listener: Vec<(AppId, TcpListener)>) -> (ServerConfiguration, HashSet<ListenToken>) {

    let mut configuration = ServerConfiguration {
      instances:     HashMap::new(),
      listeners:     Slab::with_capacity(max_listeners),
      fronts:        HashMap::new(),
      pool:          pool,
      base_token:    start_at,
    };

    let mut listener_tokens = HashSet::new();

    for (app_id, listener) in tcp_listener.drain(..) {
      if let Ok(front) = listener.local_addr() {
        let al = ApplicationListener {
          app_id:         app_id.clone(),
          sock:           Some(listener),
          token:          None,
          front_address:  front,
          back_addresses: Vec::new()
        };
        if let Some(token) = configuration.add_application_listener(&app_id, al, event_loop) {
          listener_tokens.insert(token);
        }
      }
    }

    (configuration, listener_tokens)
  }

  pub fn give_back_listeners(&mut self) -> Vec<(String, TcpListener)> {
    let res = self.listeners.iter_mut()
      .filter(|app_listener| app_listener.sock.is_some())
      .map(|app_listener| (app_listener.app_id.clone(), app_listener.sock.take().unwrap()))
      .collect();
    self.listeners.clear();
    res
  }

  fn add_application_listener(&mut self, app_id: &str, al: ApplicationListener, event_loop: &mut Poll) -> Option<ListenToken> {
    let front = al.front_address.clone();
    if let Ok(tok) = self.listeners.insert(al) {
      //FIXME: the +2 is probably not necessary here
      self.listeners[tok].token = Some(Token(self.base_token+2+tok.0));
      self.fronts.insert(String::from(app_id), tok);
      if let Some(ref sock) = self.listeners[tok].sock {
        event_loop.register(sock, Token(self.base_token+2+tok.0), Ready::readable(), PollOpt::edge());
      }
      info!("started TCP listener for app {} on port {}", app_id, front.port());
      Some(tok)
    } else {
      error!("could not register listener for app {} on port {}", app_id, front.port());
      None
    }
  }

  fn add_tcp_front(&mut self, app_id: &str, front: &SocketAddr, event_loop: &mut Poll) -> Option<ListenToken> {
    if self.fronts.contains_key(app_id) {
      error!("TCP front already exists for app_id {}", app_id);
      return None;
    }

    if let Ok(listener) = server_bind(front) {
      let addresses: Vec<SocketAddr> = if let Some(ads) = self.instances.get(app_id) {
        let v: Vec<SocketAddr> = ads.iter().map(|backend| backend.address).collect();
        v
      } else {
        Vec::new()
      };

      let al = ApplicationListener {
        app_id:         String::from(app_id),
        sock:           Some(listener),
        token:          None,
        front_address:  *front,
        back_addresses: addresses
      };

      self.add_application_listener(app_id, al, event_loop)
    } else {
      error!("could not declare listener for app {} on port {}", app_id, front.port());
      None
    }
  }

  pub fn remove_tcp_front(&mut self, app_id: String, event_loop: &mut Poll) -> Option<ListenToken>{
    info!("removing tcp_front {:?}", app_id);
    // ToDo
    // Removes all listeners for the given app_id
    // an app can't have two listeners. Is this a problem?
    if let Some(&tok) = self.fronts.get(&app_id) {
      if self.listeners.contains(tok) {
        self.listeners[tok].sock.as_ref().map(|sock| event_loop.deregister(sock));
        self.listeners.remove(tok);
        warn!("removed server {:?}", tok);
        //self.listeners[tok].sock.shutdown(Shutdown::Both);
        Some(tok)
      } else {
        None
      }
    } else {
      None
    }
  }

  pub fn add_instance(&mut self, app_id: &str, instance_id: &str, instance_address: &SocketAddr, event_loop: &mut Poll) -> Option<ListenToken> {
    use std::borrow::BorrowMut;
    if let Some(addrs) = self.instances.get_mut(app_id) {
      let id = addrs.last().map(|mut b| (*b.borrow_mut()).id ).unwrap_or(0) + 1;
      let backend = Backend::new(instance_id, *instance_address, id);
      if !addrs.contains(&backend) {
        addrs.push(backend);
      }
    }

    if self.instances.get(app_id).is_none() {
      let backend = Backend::new(instance_id, *instance_address, 0);
      self.instances.insert(String::from(app_id), vec![backend]);
    }

    if let Some(&tok) = self.fronts.get(app_id) {
      let application_listener = &mut self.listeners[tok];

      application_listener.back_addresses.push(*instance_address);
      Some(tok)
    } else {
      error!("No front for instance {} in app {}", instance_id, app_id);
      None
    }
  }

  pub fn remove_instance(&mut self, app_id: &str, instance_address: &SocketAddr, event_loop: &mut Poll) -> Option<ListenToken>{
      // ToDo
      None
  }

}

impl ProxyConfiguration<Client> for ServerConfiguration {

  fn connect_to_backend(&mut self, poll: &mut Poll, clref: Rc<RefCell<Client>>, entry: Entry<Rc<RefCell<Client>>, ClientToken>, back_token: Token) ->Result<BackendConnectAction,ConnectionError> {
    let mut client = clref.borrow_mut();// (*(*entry.get_mut()).borrow_mut());

    let rnd = random::<usize>();
    let idx = rnd % self.listeners[client.accept_token].back_addresses.len();

    client.app_id = Some(self.listeners[client.accept_token].app_id.clone());
    let backend_addr = try!(self.listeners[client.accept_token].back_addresses.get(idx).ok_or(ConnectionError::ToBeDefined));
    let stream = try!(TcpStream::connect(backend_addr).map_err(|_| ConnectionError::ToBeDefined));
    stream.set_nodelay(true);

    poll.register(
      &stream,
      back_token,
      Ready::readable() | Ready::writable() | Ready::from(UnixReady::hup() | UnixReady::error()),
      PollOpt::edge()
    );

    client.set_back_token(back_token);
    client.set_back_socket(stream);
    client.readiness().front_interest.insert(Ready::readable() | Ready::writable());
    client.readiness().back_interest.insert(Ready::readable() | Ready::writable());
    incr!("backend.connections");
    Ok(BackendConnectAction::New)
  }

  fn notify(&mut self, event_loop: &mut Poll, message: OrderMessage) -> OrderMessageAnswer {
    match message.order {
      Order::AddTcpFront(tcp_front) => {
        let addr_string = tcp_front.ip_address + ":" + &tcp_front.port.to_string();
        if let Ok(front) = addr_string.parse() {
          if let Some(token) = self.add_tcp_front(&tcp_front.app_id, &front, event_loop) {
            OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None}
          } else {
            error!("Couldn't add tcp front");
            OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Error(String::from("cannot add tcp front")), data: None}
          }
        } else {
          error!("Couldn't parse tcp front address");
          OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Error(String::from("cannot parse the address")), data: None}
        }
      },
      Order::RemoveTcpFront(front) => {
        trace!("{:?}", front);
        let _ = self.remove_tcp_front(front.app_id, event_loop);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None}
      },
      Order::AddInstance(instance) => {
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let addr = &addr_string.parse().unwrap();
        if let Some(token) = self.add_instance(&instance.app_id, &instance.instance_id, addr, event_loop) {
          OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None}
        } else {
          OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Error(String::from("cannot add tcp instance")), data: None}
        }
      },
      Order::RemoveInstance(instance) => {
        trace!("{:?}", instance);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let addr = &addr_string.parse().unwrap();
        if let Some(token) = self.remove_instance(&instance.app_id, addr, event_loop) {
          OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None}
        } else {
          error!("Couldn't remove tcp instance");
          OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Error(String::from("cannot remove tcp instance")), data: None}
        }
      },
      Order::SoftStop => {
        info!("{} processing soft shutdown", message.id);
        for listener in self.listeners.iter() {
          listener.sock.as_ref().map(|sock| event_loop.deregister(sock));
        }
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Processing, data: None}
      },
      Order::HardStop => {
        info!("{} hard shutdown", message.id);
        for listener in self.listeners.iter() {
          listener.sock.as_ref().map(|sock| event_loop.deregister(sock));
        }
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None}
      },
      Order::Status => {
        info!("{} status", message.id);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None}
      },
      Order::Logging(logging_filter) => {
        info!("{} changing logging filter to {}", message.id, logging_filter);
        ::logging::LOGGER.with(|l| {
          let directives = ::logging::parse_logging_spec(&logging_filter);
          l.borrow_mut().set_directives(directives);
        });
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      // these messages are useless for now
      Order::AddApplication(_) | Order::RemoveApplication(_) => {
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      }
      command => {
        error!("{} unsupported message, ignoring {:?}", message.id, command);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Error(String::from("unsupported message")), data: None}
      }
    }
  }

  fn accept(&mut self, token: ListenToken, poll: &mut Poll, entry: VacantEntry<Rc<RefCell<Client>>, ClientToken>,
           client_token: Token) -> Result<(ClientToken, bool), AcceptError> {
    let mut p = (*self.pool).borrow_mut();

    if let (Some(front_buf), Some(back_buf)) = (p.checkout(), p.checkout()) {
      let internal_token = ListenToken(token.0 - 2 - self.base_token);
      if self.listeners.contains(internal_token) {
        if let Some(ref listener) = self.listeners[internal_token].sock.as_ref() {
          listener.accept().map(|(frontend_sock, _)| {
            frontend_sock.set_nodelay(true);
            let mut c = Client::new(frontend_sock, internal_token, front_buf, back_buf);
            incr_req!();

            c.readiness().front_interest = UnixReady::from(Ready::readable()) | UnixReady::hup() | UnixReady::error();
            c.set_front_token(client_token);
            poll.register(
              c.front_socket(),
              client_token,
              Ready::readable() | Ready::writable() | Ready::from(UnixReady::hup() | UnixReady::error()),
              PollOpt::edge()
            );

            let index = entry.index();
            entry.insert(Rc::new(RefCell::new(c)));

            (index, true)
          }).map_err(|e| {
            match e.kind() {
              ErrorKind::WouldBlock => AcceptError::WouldBlock,
              other => {
                error!("accept() IO error: {:?}", e);
                AcceptError::IoError
              }
            }
          })
        } else {
          Err(AcceptError::IoError)
        }
      } else {
        Err(AcceptError::IoError)
      }
    } else {
      error!("could not get buffers from pool");
      Err(AcceptError::TooManyClients)
    }
  }

  fn close_backend(&mut self, app_id: String, addr: &SocketAddr) {
    if let Some(app_instances) = self.instances.get_mut(&app_id) {
      if let Some(ref mut backend) = app_instances.iter_mut().find(|backend| &backend.address == addr) {
        backend.dec_connections();
      }
    }
  }
}

pub type TcpServer = Session<ServerConfiguration,Client>;

pub fn start_example() -> Channel<OrderMessage,OrderMessageAnswer> {

  info!("listen for connections");
  let (mut command, channel) = Channel::generate(1000, 10000).expect("should create a channel");
  thread::spawn(move|| {
    info!("starting event loop");
    let mut poll = Poll::new().expect("could not create event loop");
    let max_buffers = 10;
    let buffer_size = 16384;
    let pool = Rc::new(RefCell::new(
      Pool::with_capacity(2*max_buffers, 0, || BufferQueue::with_capacity(buffer_size))
    ));
    let (configuration, tokens) = ServerConfiguration::new(10, 12297829382473034410, &mut poll, pool, Vec::new());
    let session = Session::new(10, 500, 12297829382473034410, configuration, tokens, &mut poll);
    let (scm_server, scm_client) = UnixStream::pair().unwrap();
    let mut s   = Server::new(poll, channel, ScmSocket::new(scm_server.as_raw_fd()),
      None, None, Some(session), None);
    info!("will run");
    s.run();
    info!("ending event loop");
  });
  {
    let front = TcpFront {
      app_id: String::from("yolo"),
      ip_address: String::from("127.0.0.1"),
      port: 1234,
    };
    let instance = Instance {
      app_id: String::from("yolo"),
      instance_id: String::from("yolo-0"),
      ip_address: String::from("127.0.0.1"),
      port: 5678,
    };

    command.write_message(&OrderMessage { id: String::from("ID_YOLO1"), order: Order::AddTcpFront(front) });
    command.write_message(&OrderMessage { id: String::from("ID_YOLO2"), order: Order::AddInstance(instance) });
  }
  {
    let front = TcpFront {
      app_id: String::from("yolo"),
      ip_address: String::from("127.0.0.1"),
      port: 1235,
    };
    let instance = Instance {
      app_id: String::from("yolo"),
      instance_id: String::from("yolo-0"),
      ip_address: String::from("127.0.0.1"),
      port: 5678,
    };
    command.write_message(&OrderMessage { id: String::from("ID_YOLO3"), order: Order::AddTcpFront(front) });
    command.write_message(&OrderMessage { id: String::from("ID_YOLO4"), order: Order::AddInstance(instance) });
  }
  command
}

pub fn start(max_listeners: usize, max_buffers: usize, buffer_size:usize, channel: ProxyChannel) {
  let mut poll          = Poll::new().expect("could not create event loop");
  let pool = Rc::new(RefCell::new(
    Pool::with_capacity(2*max_buffers, 0, || BufferQueue::with_capacity(buffer_size))
  ));
  let (configuration, tokens) = ServerConfiguration::new(max_listeners, 12297829382473034410, &mut poll, pool, Vec::new());
  let session           = Session::new(max_listeners, max_buffers, 12297829382473034410, configuration, tokens, &mut poll);
  let (scm_server, scm_client) = UnixStream::pair().unwrap();
  let mut server        = Server::new(poll, channel, ScmSocket::new(scm_server.as_raw_fd()), None, None, Some(session), None);

  info!("starting event loop");
  server.run();
  info!("ending event loop");
}


#[cfg(test)]
mod tests {
  use super::*;
  use std::net::{TcpListener, TcpStream, Shutdown};
  use std::io::{Read,Write};
  use std::time::Duration;
  use std::{thread,str};

  #[allow(unused_mut, unused_must_use, unused_variables)]
  #[test]
  fn mi() {
    setup_test_logger!();
    thread::spawn(|| { start_server(); });
    let tx = start_example();
    thread::sleep(Duration::from_millis(300));

    let mut s1 = TcpStream::connect("127.0.0.1:1234").expect("could not parse address");
    let mut s3 = TcpStream::connect("127.0.0.1:1234").expect("could not parse address");
    thread::sleep(Duration::from_millis(300));
    let mut s2 = TcpStream::connect("127.0.0.1:1234").expect("could not parse address");
    s1.write(&b"hello"[..]);
    println!("s1 sent");
    s2.write(&b"pouet pouet"[..]);
    println!("s2 sent");
    thread::sleep(Duration::from_millis(500));

    let mut res = [0; 128];
    s1.write(&b"coucou"[..]);
    let mut sz1 = s1.read(&mut res[..]).expect("could not read from socket");
    println!("s1 received {:?}", str::from_utf8(&res[..sz1]));
    assert_eq!(&res[..sz1], &b"hello END"[..]);
    s3.shutdown(Shutdown::Both);
    let sz2 = s2.read(&mut res[..]).expect("could not read from socket");
    println!("s2 received {:?}", str::from_utf8(&res[..sz2]));
    assert_eq!(&res[..sz2], &b"pouet pouet END"[..]);


    thread::sleep(Duration::from_millis(400));
    sz1 = s1.read(&mut res[..]).expect("could not read from socket");
    println!("s1 received again({}): {:?}", sz1, str::from_utf8(&res[..sz1]));
    assert_eq!(&res[..sz1], &b"coucou END"[..]);
    //assert!(false);
  }

  /*
  #[allow(unused_mut, unused_must_use, unused_variables)]
  #[test]
  fn concurrent() {
    use std::sync::mpsc;
    use time;
    let thread_nb = 127;

    thread::spawn(|| { start_server(); });
    start();
    thread::sleep_ms(300);

    let (tx, rx) = mpsc::channel();

    let begin = time::precise_time_s();
    for i in 0..thread_nb {
      let id = i;
      let tx = tx.clone();
      thread::Builder::new().name(id.to_string()).spawn(move || {
        let s = format!("[{}] Hello world!\n", id);
        let v: Vec<u8> = s.bytes().collect();
        if let Ok(mut conn) = TcpStream::connect("127.0.0.1:1234") {
          let mut res = [0; 128];
          for j in 0..10000 {
            conn.write(&v[..]);

            if j % 5 == 0 {
              if let Ok(sz) = conn.read(&mut res[..]) {
                //println!("[{}] received({}): {:?}", id, sz, str::from_utf8(&res[..sz]));
              } else {
                println!("failed reading");
                tx.send(());
                return;
              }
            }
          }
          tx.send(());
          return;
        } else {
          println!("failed connecting");
          tx.send(());
          return;
        }
      });
    }
    //thread::sleep_ms(5000);
    for i in 0..thread_nb {
      rx.recv();
    }
    let end = time::precise_time_s();
    println!("executed in {} seconds", end - begin);
    assert!(false);
  }
  */

  #[allow(unused_mut, unused_must_use, unused_variables)]
  fn start_server() {
    let listener = TcpListener::bind("127.0.0.1:5678").expect("could not parse address");
    fn handle_client(stream: &mut TcpStream, id: u8) {
      let mut buf = [0; 128];
      let response = b" END";
      while let Ok(sz) = stream.read(&mut buf[..]) {
        if sz > 0 {
          println!("ECHO[{}] got \"{:?}\"", id, str::from_utf8(&buf[..sz]));
          stream.write(&buf[..sz]);
          thread::sleep(Duration::from_millis(20));
          stream.write(&response[..]);
        }
      }
    }

    let mut count = 0;
    thread::spawn(move|| {
      for conn in listener.incoming() {
        match conn {
          Ok(mut stream) => {
            thread::spawn(move|| {
              println!("got a new client: {}", count);
              handle_client(&mut stream, count)
            });
          }
          Err(e) => { println!("connection failed"); }
        }
        count += 1;
      }
    });
  }

}
