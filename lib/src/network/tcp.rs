use std::thread::{self,Thread,Builder};
use std::sync::mpsc::{self,channel,Receiver};
use mio::net::*;
use mio::*;
use mio_uds::UnixStream;
use mio::unix::UnixReady;
use std::collections::HashMap;
use std::io::{self,Read,ErrorKind};
use nom::HexDisplay;
use std::error::Error;
use slab::Slab;
use std::rc::Rc;
use std::cell::RefCell;
use std::net::SocketAddr;
use std::str::FromStr;
use std::borrow::BorrowMut;
use time::{Duration,precise_time_s};
use rand::random;
use uuid::Uuid;
use pool::{Pool,Checkout,Reset};

use sozu_command::channel::Channel;
use sozu_command::buffer::Buffer;
use sozu_command::messages::{self,TcpFront,Order,Instance,OrderMessage,OrderMessageAnswer,OrderMessageStatus};

use network::{Backend,ClientResult,ConnectionError,RequiredEvents,Protocol};
use network::proxy::{Server,ProxyChannel};
use network::session::{BackendConnectAction,BackendConnectionStatus,ProxyClient,ProxyConfiguration,Readiness,ListenToken,FrontToken,BackToken,AcceptError,Session,SessionMetrics};
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
}

impl ProxyClient for Client {
  fn front_socket(&self) -> &TcpStream {
    self.protocol.front_socket()
  }

  fn back_socket(&self)  -> Option<&TcpStream> {
    self.protocol.back_socket()
  }

  fn front_token(&self)  -> Option<Token> {
    self.protocol.front_token()
  }

  fn back_token(&self)   -> Option<Token> {
    self.protocol.back_token()
  }

  fn close(&mut self) {
  }

  fn log_context(&self) -> String {
    if let Some(ref app_id) = self.app_id {
      format!("{}\t{}\t", self.request_id, app_id)
    } else {
      format!("{}\tunknown\t", self.request_id)
    }
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

  fn protocol(&self)           -> Protocol {
    Protocol::TCP
  }

  fn remove_backend(&mut self) -> (Option<String>, Option<SocketAddr>) {

    let addr = self.backend.as_ref().and_then(|sock| sock.peer_addr().ok());
    self.backend       = None;
    self.backend_token = None;
    (self.app_id.clone(), addr)
  }

  fn front_hup(&mut self) -> ClientResult {
    self.log_request();
    self.protocol.front_hup()
  }

  fn back_hup(&mut self) -> ClientResult {
    self.log_request();
    self.protocol.back_hup()
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

  fn readiness(&mut self) -> &mut Readiness {
    self.protocol.readiness()
  }

}

pub struct ApplicationListener {
  app_id:         String,
  sock:           TcpListener,
  token:          Option<Token>,
  front_address:  SocketAddr,
  back_addresses: Vec<SocketAddr>
}

type ClientToken = Token;

pub struct ServerConfiguration {
  fronts:          HashMap<String, ListenToken>,
  instances:       HashMap<String, Vec<Backend>>,
  listeners:       Slab<ApplicationListener,ListenToken>,
  pool:            Rc<RefCell<Pool<BufferQueue>>>,
  base_token:      usize,
}

impl ServerConfiguration {
  pub fn new(max_listeners: usize, start_at: usize, pool: Rc<RefCell<Pool<BufferQueue>>>) -> ServerConfiguration {
    ServerConfiguration {
      instances:     HashMap::new(),
      listeners:     Slab::with_capacity(max_listeners),
      fronts:        HashMap::new(),
      pool:          pool,
      base_token:    start_at,
    }
  }

  fn add_tcp_front(&mut self, app_id: &str, front: &SocketAddr, event_loop: &mut Poll) -> Option<ListenToken> {
    if let Ok(listener) = server_bind(front) {
      let addresses: Vec<SocketAddr> = if let Some(ads) = self.instances.get(app_id) {
        let v: Vec<SocketAddr> = ads.iter().map(|backend| backend.address).collect();
        v
      } else {
        Vec::new()
      };

      let al = ApplicationListener {
        app_id:         String::from(app_id),
        sock:           listener,
        token:          None,
        front_address:  *front,
        back_addresses: addresses
      };

      if let Ok(tok) = self.listeners.insert(al) {
        //FIXME: the +2 is probably not necessary here
        self.listeners[tok].token = Some(Token(self.base_token+2+tok.0));
        self.fronts.insert(String::from(app_id), tok);
        event_loop.register(&self.listeners[tok].sock, Token(self.base_token+2+tok.0), Ready::readable(), PollOpt::edge());
        info!("started TCP listener for app {} on port {}", app_id, front.port());
        Some(tok)
      } else {
        error!("could not register listener for app {} on port {}", app_id, front.port());
        None
      }

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
        event_loop.deregister(&self.listeners[tok].sock);
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

  fn connect_to_backend(&mut self, event_loop: &mut Poll, client:&mut Client) ->Result<BackendConnectAction,ConnectionError> {
    let rnd = random::<usize>();
    let idx = rnd % self.listeners[client.accept_token].back_addresses.len();

    client.app_id = Some(self.listeners[client.accept_token].app_id.clone());
    let backend_addr = try!(self.listeners[client.accept_token].back_addresses.get(idx).ok_or(ConnectionError::ToBeDefined));
    let stream = try!(TcpStream::connect(backend_addr).map_err(|_| ConnectionError::ToBeDefined));
    stream.set_nodelay(true);

    client.set_back_socket(stream);
    client.readiness().front_interest.insert(Ready::readable() | Ready::writable());
    client.readiness().back_interest.insert(Ready::readable() | Ready::writable());
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
          event_loop.deregister(&listener.sock);
        }
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Processing, data: None}
      },
      Order::HardStop => {
        info!("{} hard shutdown", message.id);
        for listener in self.listeners.iter() {
          event_loop.deregister(&listener.sock);
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

  fn accept(&mut self, token: ListenToken) -> Result<(Client, bool), AcceptError> {
    let mut p = (*self.pool).borrow_mut();

    if let (Some(front_buf), Some(back_buf)) = (p.checkout(), p.checkout()) {
      let internal_token = ListenToken(token.0 - 2 - self.base_token);
      if self.listeners.contains(internal_token) {
        self.listeners[internal_token].sock.accept().map(|(frontend_sock, _)| {
          frontend_sock.set_nodelay(true);
          let c = Client::new(frontend_sock, internal_token, front_buf, back_buf);
          incr_req!();
          (c, true)
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
    let configuration = ServerConfiguration::new(10, 12297829382473034410, pool);
    let session = Session::new(10, 500, 12297829382473034410, configuration, &mut poll);
    let mut s   = Server::new(poll, channel, None, None, Some(session), None);
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
  let configuration     = ServerConfiguration::new(max_listeners, 12297829382473034410, pool);
  let session           = Session::new(max_listeners, max_buffers, 12297829382473034410, configuration, &mut poll);
  let mut server        = Server::new(poll, channel, None, None, Some(session), None);

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
