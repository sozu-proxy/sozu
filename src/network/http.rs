use std::collections::HashMap;
use std::io::{self,Read,Write,ErrorKind};
use std::thread::{self,Thread,Builder};
use std::sync::mpsc::{self,channel,Receiver};
use std::net::{SocketAddr,IpAddr};
use std::str::{FromStr, from_utf8, from_utf8_unchecked};
use mio::*;
use mio::tcp::*;
use mio::timer::Timeout;
use pool::{Pool,Checkout,Reset};
use uuid::Uuid;
use nom::{HexDisplay,IResult};
use rand::random;

use network::{Backend,ClientResult,ServerMessage,ServerMessageType,ConnectionError,ProxyOrder,RequiredEvents,Protocol};
use network::buffer_queue::BufferQueue;
use network::protocol::{ProtocolResult,TlsHandshake,Http};
use network::proxy::{BackendConnectAction,Server,ProxyConfiguration,ProxyClient,Readiness,ListenToken,FrontToken,BackToken};
use network::socket::{SocketHandler,SocketResult,server_bind};
use messages::{self,Command,HttpFront,HttpProxyConfiguration};
use parser::http11::hostname_and_port;

type BackendToken = Token;

#[derive(PartialEq)]
pub enum ClientStatus {
  Normal,
  DefaultAnswer,
}

pub struct Client {
  pub frontend:   TcpStream,
  backend:        Option<TcpStream>,
  token:          Option<Token>,
  backend_token:  Option<Token>,
  front_timeout:  Option<Timeout>,
  back_timeout:   Option<Timeout>,
  http:           Http<TcpStream>,
}

impl Client {
  pub fn new(server_context: &str, sock: TcpStream, front_buf: Checkout<BufferQueue>, back_buf: Checkout<BufferQueue>, public_address: Option<IpAddr>) -> Option<Client> {
    let request_id = Uuid::new_v4().hyphenated().to_string();
    let log_ctx    = format!("{}\t{}\tunknown\t", server_context, &request_id);
    let client = Client {
      backend:        None,
      token:          None,
      backend_token:  None,
      front_timeout:  None,
      back_timeout:   None,
      http:           Http::new(server_context, sock.try_clone().unwrap(), front_buf, back_buf, public_address).unwrap(),
      frontend:       sock,
    };

    Some(client)
  }

  pub fn reset(&mut self) {
    self.http.reset()
  }

  pub fn set_answer(&mut self, buf: &[u8])  {
    self.http.set_answer(buf);
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

  fn close(&mut self) {
  }

  fn log_context(&self) -> String {
    if let Some(ref app_id) = self.http.app_id {
      format!("{}\t{}\t{}\t", self.http.server_context, self.http.request_id, app_id)
    } else {
      format!("{}\t{}\tunknown\t", self.http.server_context, self.http.request_id)
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
    self.http.set_back_socket(socket.try_clone().unwrap());
    self.backend         = Some(socket);
  }

  fn set_front_token(&mut self, token: Token) {
    self.token         = Some(token);
    self.http.set_front_token(token);
  }

  fn set_back_token(&mut self, token: Token) {
    self.backend_token = Some(token);
    self.http.set_back_token(token);
  }

  fn set_tokens(&mut self, token: Token, backend: Token) {
    self.token         = Some(token);
    self.backend_token = Some(backend);
    self.http.set_front_token(token);
    self.http.set_back_token(backend);
  }

  fn readiness(&mut self) -> &mut Readiness {
    &mut self.http.readiness
  }

  fn protocol(&self)           -> Protocol {
    Protocol::HTTP
  }

  //FIXME: unwrap bad, bad rust coder
  fn remove_backend(&mut self) -> (Option<String>, Option<SocketAddr>) {
    debug!("{}\tPROXY [{} -> {}] CLOSED BACKEND", self.http.log_ctx, self.token.unwrap().0, self.backend_token.unwrap().0);
    let addr:Option<SocketAddr> = self.backend.as_ref().and_then(|sock| sock.peer_addr().ok());
    self.backend       = None;
    self.backend_token = None;
    (self.http.app_id.clone(), addr)
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
    self.http.readable()
  }

  // Forward content to client
  fn writable(&mut self) -> ClientResult {
    self.http.writable()
  }

  // Forward content to application
  fn back_writable(&mut self) -> ClientResult {
    self.http.back_writable()
  }

  // Read content from application
  fn back_readable(&mut self) -> ClientResult {
    self.http.back_readable()
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

pub struct ServerConfiguration<Tx> {
  listener:        TcpListener,
  address:         SocketAddr,
  instances:       HashMap<AppId, Vec<Backend>>,
  fronts:          HashMap<Hostname, Vec<HttpFront>>,
  tx:              Tx,
  pool:            Pool<BufferQueue>,
  answers:         DefaultAnswers,
  front_timeout:   u64,
  back_timeout:    u64,
  config:          HttpProxyConfiguration,
  tag:             String,
}

impl<Tx: messages::Sender<ServerMessage>> ServerConfiguration<Tx> {
  pub fn new(tag: String, config: HttpProxyConfiguration, tx: Tx, event_loop: &mut Poll, start_at:usize) -> io::Result<ServerConfiguration<Tx>> {
    let front = config.front;
    match server_bind(&config.front) {
      Ok(sock) => {
        event_loop.register(&sock, Token(start_at), Ready::readable(), PollOpt::level());
        Ok(ServerConfiguration {
          listener:      sock,
          address:       config.front,
          instances:     HashMap::new(),
          fronts:        HashMap::new(),
          tx:            tx,
          pool:          Pool::with_capacity(2*config.max_connections, 0, || BufferQueue::with_capacity(config.buffer_size)),
          //FIXME: make the timeout values configurable
          front_timeout: 5000,
          back_timeout:  5000,
          answers:       DefaultAnswers {
                           NotFound: Vec::from(&b"HTTP/1.1 404 Not Found\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"[..]),
                           ServiceUnavailable: Vec::from(&b"HTTP/1.1 503 your application is in deployment\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"[..]),
          },
          config:        config,
          tag:           tag,
        })
      },
      Err(e) => {
        error!("{}\tcould not create listener {:?}: {:?}", tag, front, e);
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
    info!("{}\tremoving http_front {:?}", self.tag, front);
    if let Some(fronts) = self.fronts.get_mut(&front.hostname) {
      fronts.retain(|f| f != &front);
    }
  }

  pub fn add_instance(&mut self, app_id: &str, instance_address: &SocketAddr, event_loop: &mut Poll) {
    if let Some(addrs) = self.instances.get_mut(app_id) {
      let backend = Backend::new(*instance_address);
      if !addrs.contains(&backend) {
        addrs.push(backend);
      }
    }

    if self.instances.get(app_id).is_none() {
      let backend = Backend::new(*instance_address);
      self.instances.insert(String::from(app_id), vec![backend]);
    }
  }

  pub fn remove_instance(&mut self, app_id: &str, instance_address: &SocketAddr, event_loop: &mut Poll) {
      if let Some(instances) = self.instances.get_mut(app_id) {
        instances.retain(|backend| &backend.address != instance_address);
      } else {
        error!("{}\tInstance was already removed", self.tag);
      }
  }

  pub fn frontend_from_request(&self, host: &str, uri: &str) -> Option<&HttpFront> {
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
    client.http.app_id = Some(String::from(app_id));
    //FIXME: round-robin on instances
    if let Some(ref mut app_instances) = self.instances.get_mut(app_id) {
      if app_instances.len() == 0 {
        client.set_answer(&self.answers.ServiceUnavailable);
        return Err(ConnectionError::NoBackendAvailable);
      }
      let rnd = random::<usize>();
      let mut instances:Vec<&mut Backend> = app_instances.iter_mut().filter(|backend| backend.can_open()).collect();
      let idx = rnd % instances.len();
      info!("{}\tConnecting {} -> {:?}", client.http.log_ctx, app_id, instances.get(idx).map(|backend| (backend.address, backend.active_connections)));
      instances.get_mut(idx).ok_or(ConnectionError::NoBackendAvailable).and_then(|ref mut backend| {
        let conn: Result<TcpStream, ConnectionError> = TcpStream::connect(&backend.address).map_err(|_| ConnectionError::NoBackendAvailable);
        if conn.is_ok() {
          backend.inc_connections();
        }
        conn
      })
    } else {
      Err(ConnectionError::NoBackendAvailable)
    }
  }
}

impl<Tx: messages::Sender<ServerMessage>> ProxyConfiguration<Client> for ServerConfiguration<Tx> {
  fn connect_to_backend(&mut self, event_loop: &mut Poll, client: &mut Client) -> Result<BackendConnectAction,ConnectionError> {
    let h = try!(client.http.state.as_ref().unwrap().get_host().ok_or(ConnectionError::NoHostGiven));

    let host: &str = if let IResult::Done(i, (hostname, port)) = hostname_and_port(h.as_bytes()) {
      if i != &b""[..] {
        error!("{}\tinvalid remaining chars after hostname", self.tag);
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
      error!("{}\thostname parsing failed", self.tag);
      return Err(ConnectionError::ToBeDefined);
    };

    let rl     = try!(client.http.state.as_ref().unwrap().get_request_line().ok_or(ConnectionError::NoRequestLineGiven));
    if let Some(app_id) = self.frontend_from_request(&host, &rl.uri).map(|ref front| front.app_id.clone()) {
      if client.http.app_id.as_ref() == Some(&app_id) {
        //matched on keepalive
        return Ok(BackendConnectAction::Reuse)
      }

      let reused = client.http.app_id.is_some();
      if reused {
        let sock = client.backend.as_ref().unwrap();
        event_loop.deregister(sock);
        sock.shutdown(Shutdown::Both);
      }
      //FIXME: deregister back socket, since it is the wrong one

      let conn   = self.backend_from_app_id(client, &app_id);
      match conn {
        Ok(socket) => {
          socket.set_nodelay(true);
          client.set_back_socket(socket);
          client.readiness().back_interest.insert(Ready::writable());
          client.readiness().back_interest.insert(Ready::hup());
          client.readiness().back_interest.insert(Ready::error());
          if reused {
            Ok(BackendConnectAction::Replace)
          } else {
            Ok(BackendConnectAction::New)
          }
          //Ok(())
        },
        Err(ConnectionError::NoBackendAvailable) => {
          client.set_answer(&self.answers.ServiceUnavailable);
          client.readiness().front_interest = Ready::writable() | Ready::hup() | Ready::error();
          Err(ConnectionError::NoBackendAvailable)
        }
        Err(ConnectionError::HostNotFound) => {
          client.set_answer(&self.answers.NotFound);
          client.readiness().front_interest = Ready::writable() | Ready::hup() | Ready::error();
          Err(ConnectionError::HostNotFound)
        }
        e => panic!(e)
      }
    } else {
      client.set_answer(&self.answers.NotFound);
      client.readiness().front_interest = Ready::writable() | Ready::hup() | Ready::error();
      Err(ConnectionError::HostNotFound)
    }
  }

  fn notify(&mut self, event_loop: &mut Poll, message: ProxyOrder) {
  // ToDo temporary
    trace!("{}\t{} notified", self.tag, message);
    match message {
      ProxyOrder::Command(id, Command::AddHttpFront(front)) => {
        info!("{}\t{} add front {:?}", self.tag, id, front);
          self.add_http_front(front, event_loop);
          self.tx.send_message(ServerMessage{ id: id, message: ServerMessageType::AddedFront});
      },
      ProxyOrder::Command(id, Command::RemoveHttpFront(front)) => {
        info!("{}\t{} front {:?}", self.tag, id, front);
        self.remove_http_front(front, event_loop);
        self.tx.send_message(ServerMessage{ id: id, message: ServerMessageType::RemovedFront});
      },
      ProxyOrder::Command(id, Command::AddInstance(instance)) => {
        info!("{}\t{} add instance {:?}", self.tag, id, instance);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let parsed:Option<SocketAddr> = addr_string.parse().ok();
        if let Some(addr) = parsed {
          self.add_instance(&instance.app_id, &addr, event_loop);
          self.tx.send_message(ServerMessage{ id: id, message: ServerMessageType::AddedInstance});
        } else {
          self.tx.send_message(ServerMessage{ id: id, message: ServerMessageType::Error(String::from("cannot parse the address"))});
        }
      },
      ProxyOrder::Command(id, Command::RemoveInstance(instance)) => {
        info!("{}\t{} remove instance {:?}", self.tag, id, instance);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let parsed:Option<SocketAddr> = addr_string.parse().ok();
        if let Some(addr) = parsed {
          self.remove_instance(&instance.app_id, &addr, event_loop);
          self.tx.send_message(ServerMessage{ id: id, message: ServerMessageType::RemovedInstance});
        } else {
          self.tx.send_message(ServerMessage{ id: id, message: ServerMessageType::Error(String::from("cannot parse the address"))});
        }
      },
      ProxyOrder::Command(id, Command::HttpProxy(configuration)) => {
        info!("{}\t{} modifying proxy configuration: {:?}", self.tag, id, configuration);
        self.front_timeout = configuration.front_timeout;
        self.back_timeout  = configuration.back_timeout;
        self.answers = DefaultAnswers {
          NotFound:           configuration.answer_404.into_bytes(),
          ServiceUnavailable: configuration.answer_503.into_bytes(),
        };
      },
      ProxyOrder::Stop(id)                   => {
        info!("{}\t{} shutdown", self.tag, id);
        //FIXME: handle shutdown
        //event_loop.shutdown();
        self.tx.send_message(ServerMessage{ id: id, message: ServerMessageType::Stopped});
      },
      ProxyOrder::Command(id, msg) => {
        debug!("{}\t{} unsupported message, ignoring: {:?}", self.tag, id, msg);
        self.tx.send_message(ServerMessage{ id: id, message: ServerMessageType::Error(String::from("unsupported message"))});
      }
    }
  }

  fn accept(&mut self, token: ListenToken) -> Option<(Client, bool)> {
    if let (Some(front_buf), Some(back_buf)) = (self.pool.checkout(), self.pool.checkout()) {
      let accepted = self.listener.accept();

      if let Ok((frontend_sock, _)) = accepted {
        frontend_sock.set_nodelay(true);
        if let Some(mut c) = Client::new(&self.tag, frontend_sock, front_buf, back_buf, self.config.public_address) {
          c.readiness().front_interest.insert(Ready::readable());
          c.readiness().back_interest.remove(Ready::readable() | Ready::writable());
          return Some((c, false))
        }
      } else {
        error!("{}\tcould not accept: {:?}", self.tag, accepted);
      }
    } else {
      error!("{}\tcould not get buffers from pool", self.tag);
    }
    None
  }

  fn close_backend(&mut self, app_id: String, addr: &SocketAddr) {
    if let Some(app_instances) = self.instances.get_mut(&app_id) {
      if let Some(ref mut backend) = app_instances.iter_mut().find(|backend| &backend.address == addr) {
        backend.dec_connections();
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

pub type HttpServer<Tx,Rx> = Server<ServerConfiguration<Tx>,Client,Rx>;

pub fn start_listener<Tx,Rx>(tag:String, config: HttpProxyConfiguration, tx: Tx, receiver: Rx)
  where Tx: messages::Sender<ServerMessage>,
        Rx: Evented+messages::Receiver<ProxyOrder> {

  let mut event_loop  = Poll::new().unwrap();
  let max_connections = config.max_connections;
  let max_listeners   = 1;
  // start at max_listeners + 1 because token(0) is the channel, and token(1) is the timer
  let configuration = ServerConfiguration::new(tag.clone(), config, tx, &mut event_loop, 1 + max_listeners).unwrap();
  let mut server = HttpServer::new(max_listeners, max_connections, configuration, event_loop, receiver);

  info!("{}\tstarting event loop", &tag);
  server.run();
  //event_loop.run(&mut server).unwrap();
  info!("{}\tending event loop", &tag);
}

#[cfg(test)]
mod tests {
  extern crate tiny_http;
  use super::*;
  use slab::Slab;
  use mio::{channel,Poll};
  use std::collections::HashMap;
  use std::net::{TcpListener, TcpStream, Shutdown};
  use std::io::{Read,Write};
  use std::{thread,str};
  use std::sync::mpsc::channel;
  use std::net::SocketAddr;
  use std::str::FromStr;
  use std::time::Duration;
  use messages::{Command,HttpFront,Instance,HttpProxyConfiguration};
  use network::{ProxyOrder,ServerMessage};
  use network::buffer_queue::BufferQueue;
  use pool::Pool;

  #[allow(unused_mut, unused_must_use, unused_variables)]
  #[test]
  fn mi() {
    start_server(1025);
    let front: SocketAddr = FromStr::from_str("127.0.0.1:1024").unwrap();
    let (tx,rx) = channel::<ServerMessage>();
    let config = HttpProxyConfiguration {
      front: front,
      max_connections: 10,
      buffer_size: 12000,
      ..Default::default()
    };

    let (sender, receiver) = channel::channel::<ProxyOrder>();
    let jg = thread::spawn(move || {
      start_listener(String::from("HTTP"), config, tx.clone(), receiver);
    });

    let front = HttpFront { app_id: String::from("app_1"), hostname: String::from("localhost:1024"), path_begin: String::from("/") };
    sender.send(ProxyOrder::Command(String::from("ID_ABCD"), Command::AddHttpFront(front)));
    let instance = Instance { app_id: String::from("app_1"), ip_address: String::from("127.0.0.1"), port: 1025 };
    sender.send(ProxyOrder::Command(String::from("ID_EFGH"), Command::AddInstance(instance)));
    println!("test received: {:?}", rx.recv());
    println!("test received: {:?}", rx.recv());
    thread::sleep(Duration::from_millis(300));

    let mut client = TcpStream::connect(("127.0.0.1", 1024)).unwrap();
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

        println!("Response: {}", str::from_utf8(&buffer[..]).unwrap());

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
    start_server(1028);
    let front: SocketAddr = FromStr::from_str("127.0.0.1:1031").unwrap();
    let (tx,rx) = channel::<ServerMessage>();
    let config = HttpProxyConfiguration {
      front: front,
      max_connections: 10,
      buffer_size: 12000,
      ..Default::default()
    };
    let (sender, receiver) = channel::channel::<ProxyOrder>();
    let jg = thread::spawn(move|| {
      start_listener(String::from("HTTP"), config, tx.clone(), receiver);
    });
    let front = HttpFront { app_id: String::from("app_1"), hostname: String::from("localhost:1031"), path_begin: String::from("/") };
    sender.send(ProxyOrder::Command(String::from("ID_ABCD"), Command::AddHttpFront(front)));
    let instance = Instance { app_id: String::from("app_1"), ip_address: String::from("127.0.0.1"), port: 1028 };
    sender.send(ProxyOrder::Command(String::from("ID_EFGH"), Command::AddInstance(instance)));
    println!("test received: {:?}", rx.recv());
    println!("test received: {:?}", rx.recv());
    thread::sleep(Duration::from_millis(300));

    let mut client = TcpStream::connect(("127.0.0.1", 1031)).unwrap();
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

        println!("Response: {}", str::from_utf8(&buffer[..]).unwrap());

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

        println!("Response: {}", str::from_utf8(&buffer2[..]).unwrap());

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
      let server = ServerBuilder::new().with_port(port).build().unwrap();
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

    let (tx,rx) = channel::<ServerMessage>();

    let front: SocketAddr = FromStr::from_str("127.0.0.1:1030").unwrap();
    let listener = tcp::TcpListener::bind(&front).unwrap();
    let server_config = ServerConfiguration {
      listener:  listener,
      address:   front,
      instances: HashMap::new(),
      fronts:    fronts,
      tx:        tx,
      pool:      Pool::with_capacity(1,0, || BufferQueue::with_capacity(12000)),
      front_timeout: 50000,
      back_timeout:  50000,
      answers:   DefaultAnswers {
        NotFound: Vec::from(&b"HTTP/1.1 404 Not Found\r\n\r\n"[..]),
        ServiceUnavailable: Vec::from(&b"HTTP/1.1 503 your application is in deployment\r\n\r\n"[..]),
      },
      config: Default::default(),
      tag:  String::from("HTTP"),
    };

    let frontend1 = server_config.frontend_from_request("lolcatho.st", "/");
    let frontend2 = server_config.frontend_from_request("lolcatho.st", "/test");
    let frontend3 = server_config.frontend_from_request("lolcatho.st", "/yolo/test");
    let frontend4 = server_config.frontend_from_request("lolcatho.st", "/yolo/swag");
    let frontend5 = server_config.frontend_from_request("domain", "/");
    assert_eq!(frontend1.unwrap().app_id, "app_1");
    assert_eq!(frontend2.unwrap().app_id, "app_1");
    assert_eq!(frontend3.unwrap().app_id, "app_2");
    assert_eq!(frontend4.unwrap().app_id, "app_3");
    assert_eq!(frontend5, None);
  }
}
