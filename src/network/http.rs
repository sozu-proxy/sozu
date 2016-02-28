#![allow(unused_imports)]

use std::thread::{self,Thread,Builder};
use std::sync::mpsc::{self,channel,Receiver};
use std::cmp::min;
use mio::tcp::*;
use std::io::{self,Read,Write,ErrorKind};
use mio::*;
use bytes::{ByteBuf,MutByteBuf};
use bytes::buf::MutBuf;
use std::collections::HashMap;
use std::error::Error;
use mio::util::Slab;
use std::net::SocketAddr;
use std::str::{FromStr, from_utf8};
use time::{Duration, precise_time_s, precise_time_ns};
use rand::random;
use network::{ClientResult,ServerMessage,ConnectionError,ProxyOrder};
use network::proxy::{Server,ProxyConfiguration,ProxyClient};
use network::metrics::{ProxyMetrics,METRICS};
use network::buffer::Buffer;
use network::socket::{SocketHandler,SocketResult};

use parser::http11::{HttpState,parse_request_until_stop, parse_response_until_stop, BufferMove};
use nom::HexDisplay;

use messages::{Command,HttpFront};

type BackendToken = Token;
#[derive(Debug,Clone,PartialEq,Eq)]
pub enum ConnectionStatus {
  Initial,
  ClientConnected,
  Connected,
  ClientClosed,
  ServerClosed,
  Closed
}

pub struct HttpProxy {
  pub state:             HttpState,
  pub front_buf:         Buffer,
  pub back_buf:          Buffer,
  pub start:             u64,
  pub req_size:          usize,
  pub res_size:          usize,
}

impl HttpProxy {
  pub fn readable(&mut self) -> ClientResult {
    debug!("front buffer({} bytes):\n{}", self.front_buf.data().len(), self.front_buf.data().to_hex(8));
    if ! self.state.is_front_proxying() {
      self.state = parse_request_until_stop(&self.state, &mut self.front_buf);
      debug!("parse_request_until_stop returned {:?} => advance: {}", self.state, self.state.req_position);
      if self.state.is_front_error() {
        METRICS.lock().unwrap().time("http_proxy.failure", (precise_time_ns() - self.start) / 1000);
        return ClientResult::CloseClient;
      }
      if self.state.has_host() {
        return ClientResult::ConnectBackend;
      }
    } else {
      debug!("state: {:?}", self.state);
    }
    ClientResult::Continue
  }

  pub fn writable(&mut self, copied: usize) -> ClientResult {
    self.back_buf.consume(copied);
    self.state.back_copied(copied);

    // The response ended, so we can close the connection, or accept a new request
    // FIXME: with Server Side Events, new responses can come
    if self.state.is_back_proxying() && self.state.res_position == 0 {
      let sh = (self.state.front_should_keep_alive(), self.state.back_should_keep_alive());
      trace!("RESPONSE ENDED, SHOULD WE KEEP ALIVE? {:?}", sh);
      let res = match sh {
        (true, true)  => ClientResult::Continue,     // just reset
        (true, false) => ClientResult::CloseBackend,
        (false, _)    => ClientResult::CloseBothSuccess,    // no connection pooling yet
      };
      METRICS.lock().unwrap().time("http_proxy.success", (precise_time_ns() - self.start) / 1000);
      METRICS.lock().unwrap().gauge("http_proxy.request_size", self.req_size as u64);
      METRICS.lock().unwrap().gauge("http_proxy.response_size", self.res_size as u64);
      self.state.reset();
      res
    } else {
      ClientResult::Continue
    }
  }

  pub fn back_readable(&mut self, sz: usize) -> ClientResult {
    self.back_buf.fill(sz);
    self.res_size = self.res_size + sz;

    debug!("back buffer ({} bytes):\n{}", self.back_buf.data().len(), self.back_buf.data().to_hex(8));
    if ! self.state.is_back_proxying() {
      self.state = parse_response_until_stop(&self.state, &mut self.back_buf);
      debug!("parse_response_until_stop returned {:?} => advance: {}", self.state, self.state.res_position);
      if self.state.is_back_error() {
        METRICS.lock().unwrap().time("http_proxy.failure", (precise_time_ns() - self.start) / 1000);
        return ClientResult::CloseBackend;
      }
    }
    ClientResult::Continue
  }

  pub fn back_writable(&mut self, copied: usize) -> ClientResult {
    self.front_buf.consume(copied);
    self.state.front_copied(copied);

    ClientResult::Continue
  }

  pub fn front_to_copy(&self) -> usize {
    min(self.front_buf.available_data(), self.state.req_position)
  }

  pub fn back_to_copy(&self) -> usize {
    min(self.back_buf.available_data(), self.state.res_position)
  }

}

pub struct Client<Front:SocketHandler> {
  frontend:       Front,
  backend:        Option<TcpStream>,
  http_state:     HttpProxy,
  token:          Option<Token>,
  backend_token:  Option<Token>,
  back_interest:  EventSet,
  front_interest: EventSet,
  status:         ConnectionStatus,
  rx_count:       usize,
  tx_count:       usize,
}

impl<Front:SocketHandler> Client<Front> {
  pub fn new(sock: Front) -> Option<Client<Front>> {
    Some(Client {
      frontend:       sock,
      backend:        None,
      http_state:     HttpProxy {
        state:             HttpState::new(),
        front_buf:         Buffer::with_capacity(12000),
        back_buf:          Buffer::with_capacity(12000),
        start:             precise_time_ns(),
        req_size:          0,
        res_size:          0,
      },
      token:          None,
      backend_token:  None,
      back_interest:  EventSet::all(),
      front_interest: EventSet::all(),
      status:         ConnectionStatus::ClientConnected,
      rx_count:       0,
      tx_count:       0,
    })
  }

  pub fn close(&self) {
  }

  fn tokens(&self) -> Option<(Token,Token)> {
    if let Some(front) = self.token {
      if let Some(back) = self.backend_token {
        return Some((front, back))
      }
    }
    None
  }

  pub fn http_state(&mut self) -> &mut HttpProxy {
    &mut self.http_state
  }

  pub fn set_status(&mut self, status: ConnectionStatus) {
    self.status = status;
  }
}

impl<Front:SocketHandler> ProxyClient for Client<Front> {
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

  fn set_back_socket(&mut self, socket: TcpStream) {
    self.backend         = Some(socket);
  }

  fn set_front_token(&mut self, token: Token) {
    self.token         = Some(token);
  }

  fn set_back_token(&mut self, token: Token) {
    self.backend_token = Some(token);
  }

  fn set_tokens(&mut self, token: Token, backend: Token) {
    self.token         = Some(token);
    self.backend_token = Some(backend);
  }

  fn remove_backend(&mut self) {
    debug!("HTTP PROXY [{} -> {}] CLOSED BACKEND", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize());
    self.backend       = None;
    self.backend_token = None;
  }

  fn front_hup(&mut self) -> ClientResult {
    if  self.status == ConnectionStatus::ServerClosed ||
        self.status == ConnectionStatus::ClientConnected { // the server never answered, the client closed
      self.status = ConnectionStatus::Closed;
      ClientResult::CloseClient
    } else {
      self.status = ConnectionStatus::ClientClosed;
      ClientResult::Continue
    }

  }

  fn back_hup(&mut self) -> ClientResult {
    if self.status == ConnectionStatus::ClientClosed {
      self.status = ConnectionStatus::Closed;
      ClientResult::CloseClient
    } else {
      self.status = ConnectionStatus::ServerClosed;
      ClientResult::Continue
    }
  }

  // Read content from the client
  fn readable(&mut self) -> ClientResult {
    let (sz, res) = self.frontend.socket_read(self.http_state.front_buf.space());
    debug!("FRONT [{:?}]: read {} bytes", self.token, sz);
    self.http_state.front_buf.fill(sz);
    match res {
      SocketResult::Error => ClientResult::CloseClient,
      _                   => {
        if sz > 0 {
        self.http_state.readable()
        } else {
          ClientResult::Continue
        }
      }
    }
  }

  // Forward content to client
  fn writable(&mut self) -> ClientResult {
    if self.http_state.back_buf.available_data() == 0 {
      return ClientResult::Continue;
    }

    let (sz, res) = self.frontend.socket_write(&(self.http_state.back_buf.data())[..self.http_state.back_to_copy()]);
    if let Some((front,back)) = self.tokens() {
      debug!("FRONT [{}<-{}]: wrote {} bytes", front.as_usize(), back.as_usize(), sz);
    }
    self.tx_count = self.tx_count + sz;
    match res {
      SocketResult::Error => ClientResult::CloseClient,
      _                   => {
        if sz > 0 {
        self.http_state.writable(sz)
        } else {
          ClientResult::Continue
        }
      }
    }
  }

  // Forward content to application
  fn back_writable(&mut self) -> ClientResult {
    if self.http_state.front_buf.available_data() == 0 {
      return ClientResult::Continue;
    }

    let tokens = self.tokens().clone();
    let res = if let Some(ref mut sock) = self.backend {
      let (sz, res) = sock.socket_write(&(self.http_state.front_buf.data())[..self.http_state.front_to_copy()]);
      if let Some((front,back)) = tokens {
        debug!("BACK  [{}->{}]: wrote {} bytes", front.as_usize(), back.as_usize(), sz);
      }
      match res {
        SocketResult::Error => ClientResult::CloseBothFailure,
        _                   => {
          if sz > 0 {
            self.http_state.back_writable(sz)
          } else {
            ClientResult::Continue
          }
        }
      }
    } else {
      return ClientResult::CloseBothFailure
    };

    res
  }

  // Read content from application
  fn back_readable(&mut self) -> ClientResult {
    let tokens = self.tokens().clone();
    let res = if let Some(ref mut sock) = self.backend {
      let (sz, r) = sock.socket_read(&mut self.http_state.back_buf.space());
      if let Some((front,back)) = tokens {
        debug!("BACK  [{}->{}]: read {} bytes", front.as_usize(), back.as_usize(), sz);
      }
      match r {
        SocketResult::Error => ClientResult::CloseBothFailure,
        _                   => {
          if sz > 0 {
            self.http_state.back_readable(sz)
          } else {
            ClientResult::Continue
          }
        }
      }
    } else {
      return ClientResult::CloseBothFailure;
    };

    res
  }

}

pub struct ApplicationListener {
  sock:           TcpListener,
  token:          Option<Token>,
  front_address:  SocketAddr
}

type ClientToken = Token;

pub struct ServerConfiguration {
  instances: HashMap<String, Vec<SocketAddr>>,
  listeners: Slab<ApplicationListener>,
  fronts:    HashMap<String, Vec<HttpFront>>,
  tx:        mpsc::Sender<ServerMessage>
}

impl ServerConfiguration {
  pub fn new(max_listeners: usize,  tx: mpsc::Sender<ServerMessage>) -> ServerConfiguration {
    ServerConfiguration {
      instances: HashMap::new(),
      listeners: Slab::new_starting_at(Token(0), max_listeners),
      fronts:    HashMap::new(),
      tx:        tx
    }
  }

  pub fn add_http_front(&mut self, http_front: HttpFront, event_loop: &mut EventLoop<HttpServer>) {
    let front2 = http_front.clone();
    let front3 = http_front.clone();
    if let Some(fronts) = self.fronts.get_mut(&http_front.hostname) {
        fronts.push(front2);
    }

    if self.listeners.iter().find(|&l| l.front_address.port() == http_front.port).is_none() {
      self.add_tcp_front(http_front.port, "", event_loop);
    }

    if self.fronts.get(&http_front.hostname).is_none() {
      self.fronts.insert(http_front.hostname, vec![front3]);
    }
  }

  pub fn remove_http_front(&mut self, front: HttpFront, event_loop: &mut EventLoop<HttpServer>) {
    info!("removing http_front {:?}", front);
    if let Some(fronts) = self.fronts.get_mut(&front.hostname) {
      fronts.retain(|f| f != &front);
    }
  }

  pub fn add_instance(&mut self, app_id: &str, instance_address: &SocketAddr, event_loop: &mut EventLoop<HttpServer>) {
    if let Some(addrs) = self.instances.get_mut(app_id) {
        addrs.push(*instance_address);
    }

    if self.instances.get(app_id).is_none() {
      self.instances.insert(String::from(app_id), vec![*instance_address]);
    }
  }

  pub fn remove_instance(&mut self, app_id: &str, instance_address: &SocketAddr, event_loop: &mut EventLoop<HttpServer>) {
      if let Some(instances) = self.instances.get_mut(app_id) {
        instances.retain(|addr| addr != instance_address);
      } else {
        error!("Instance was already removed");
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

  pub fn backend_from_request(&self, host: &str, uri: &str) -> Option<SocketAddr> {
    if let Some(http_front) = self.frontend_from_request(host, uri) {
      // ToDo round-robin on instances
      if let Some(app_instances) = self.instances.get(&http_front.app_id) {
        let rnd = random::<usize>();
        let idx = rnd % app_instances.len();
        debug!("Connecting {} -> {:?}", host, app_instances.get(idx));
        app_instances.get(idx).map(|& addr| addr)
      } else {
        None
      }
    } else {
      None
    }
  }

}

impl ProxyConfiguration<HttpServer,Client<TcpStream>> for ServerConfiguration {
  fn add_tcp_front(&mut self, port: u16, app_id: &str, event_loop: &mut EventLoop<HttpServer>) -> Option<Token> {
    let addr_string = String::from("127.0.0.1:") + &port.to_string();
    let front = &addr_string.parse().unwrap();

    if let Ok(listener) = TcpListener::bind(front) {

      let al = ApplicationListener {
        sock:           listener,
        token:          None,
        front_address:  *front,
      };

      if let Ok(tok) = self.listeners.insert(al) {
        self.listeners[tok].token = Some(tok);
        //self.fronts.insert(String::from(app_id), tok);
        event_loop.register(&self.listeners[tok].sock, tok, EventSet::readable(), PollOpt::level());
        info!("registered listener({}) on port {}", tok.as_usize(), port);
        Some(tok)
      } else {
        error!("could not register listener on port {}", port);
        None
      }
    } else {
      error!("could not declare listener on port {}", port);
      None
    }
  }

  fn connect_to_backend(&mut self, client: &mut Client<TcpStream>) -> Result<TcpStream,ConnectionError> {
      let host   = try!(client.http_state.state.get_host().ok_or(ConnectionError::NoHostGiven));
      let rl     = try!(client.http_state.state.get_request_line().ok_or(ConnectionError::NoRequestLineGiven));
      let back   = try!(self.backend_from_request(&host, &rl.uri).ok_or(ConnectionError::HostNotFound));
      let socket = try!(TcpStream::connect(&back).map_err(|_| ConnectionError::ToBeDefined));

      client.status = ConnectionStatus::Connected;

      Ok(socket)
  }

  fn notify(&mut self, event_loop: &mut EventLoop<HttpServer>, message: ProxyOrder) {
  // ToDo temporary
    trace!("notified: {:?}", message);
    match message {
      ProxyOrder::Command(Command::AddHttpFront(front)) => {
        info!("add front {:?}", front);
          self.add_http_front(front, event_loop);
          self.tx.send(ServerMessage::AddedFront);
      },
      ProxyOrder::Command(Command::RemoveHttpFront(front)) => {
        info!("remove front {:?}", front);
        self.remove_http_front(front, event_loop);
        self.tx.send(ServerMessage::RemovedFront);
      },
      ProxyOrder::Command(Command::AddInstance(instance)) => {
        info!("add instance {:?}", instance);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let parsed:Option<SocketAddr> = addr_string.parse().ok();
        if let Some(addr) = parsed {
          self.add_instance(&instance.app_id, &addr, event_loop);
          self.tx.send(ServerMessage::AddedInstance);
        }
      },
      ProxyOrder::Command(Command::RemoveInstance(instance)) => {
        info!("remove instance {:?}", instance);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let parsed:Option<SocketAddr> = addr_string.parse().ok();
        if let Some(addr) = parsed {
          self.remove_instance(&instance.app_id, &addr, event_loop);
          self.tx.send(ServerMessage::RemovedInstance);
        }
      },
      ProxyOrder::Stop                   => {
        event_loop.shutdown();
      },
      _ => {
        debug!("unsupported message, ignoring");
      }
    }
  }

  fn accept(&mut self, token: Token) -> Option<(Client<TcpStream>, bool)> {
    if self.listeners.contains(token) {
      let accepted = self.listeners[token].sock.accept();

      if let Ok(Some((frontend_sock, _))) = accepted {
        if let Some(c) = Client::new(frontend_sock) {
          return Some((c, false))
        }
      }
    } else {
      error!("listener not found for this socket accept");
    }

    None
  }


}

pub type HttpServer = Server<ServerConfiguration,Client<TcpStream>>;

pub fn start() {
  // ToDo temporary
  let mut event_loop = EventLoop::new().unwrap();

  let (tx,rx) = channel::<ServerMessage>();
  let channel = event_loop.channel();
  let notify_tx = tx.clone();

  let configuration = ServerConfiguration::new(1, tx);
  let mut server = HttpServer::new(1, 500, configuration);

  let join_guard = thread::spawn(move|| {
    debug!("starting event loop");
    event_loop.run(&mut server).unwrap();
    debug!("ending event loop");
    notify_tx.send(ServerMessage::Stopped);
  });


  //println!("listen for connections");
  //event_loop.register(&listener, SERVER, EventSet::readable(), PollOpt::edge() | PollOpt::oneshot()).unwrap();
  //let mut s = Server::new(10, 500, tx);
  //{
  //  let back: SocketAddr = FromStr::from_str("127.0.0.1:5678").unwrap();
  //  s.add_tcp_front(1234, "yolo", &mut event_loop);
  //  s.add_instance("yolo", &back, &mut event_loop);
  //}
  //{
  //  let back: SocketAddr = FromStr::from_str("127.0.0.1:5678").unwrap();
  //  s.add_tcp_front(1235, "yolo", &mut event_loop);
  //  s.add_instance("yolo", &back, &mut event_loop);
  //}
  //thread::spawn(move|| {
  //  println!("starting event loop");
  //  event_loop.run(&mut s).unwrap();
  //  println!("ending event loop");
  //});
}

pub fn start_listener(front: SocketAddr, max_listeners: usize, max_connections: usize, tx: mpsc::Sender<ServerMessage>) -> (Sender<ProxyOrder>,thread::JoinHandle<()>)  {
  let mut event_loop = EventLoop::new().unwrap();
  let channel = event_loop.channel();
  let notify_tx = tx.clone();

  let configuration = ServerConfiguration::new(max_listeners, tx);
  let mut server = HttpServer::new(max_listeners, max_connections, configuration);

  let join_guard = thread::spawn(move|| {
    debug!("starting event loop");
    event_loop.run(&mut server).unwrap();
    debug!("ending event loop");
    notify_tx.send(ServerMessage::Stopped);
  });

  (channel, join_guard)
}

#[cfg(test)]
mod tests {
  extern crate tiny_http;
  use super::*;
  use mio::util::Slab;
  use std::collections::HashMap;
  use std::net::{TcpListener, TcpStream, Shutdown};
  use std::io::{Read,Write};
  use std::{thread,str};
  use std::sync::mpsc::channel;
  use std::net::SocketAddr;
  use std::str::FromStr;
  use std::time::Duration;
  use messages::{Command,HttpFront,Instance};
  use network::{ProxyOrder,ServerMessage};

  #[allow(unused_mut, unused_must_use, unused_variables)]
  #[test]
  fn mi() {
    thread::spawn(|| { start_server(); });
    let front: SocketAddr = FromStr::from_str("127.0.0.1:1024").unwrap();
    let (tx,rx) = channel::<ServerMessage>();
    let (sender, jg) = start_listener(front, 10, 10, tx.clone());
    let front = HttpFront { app_id: String::from("app_1"), hostname: String::from("localhost:1024"), path_begin: String::from("/"), port: 1024 };
    sender.send(ProxyOrder::Command(Command::AddHttpFront(front)));
    let instance = Instance { app_id: String::from("app_1"), ip_address: String::from("127.0.0.1"), port: 1025 };
    sender.send(ProxyOrder::Command(Command::AddInstance(instance)));
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
        assert_eq!(sz, 154);
        //assert!(false);
      }
    }
  }

  use self::tiny_http::{ServerBuilder, Response};

  #[allow(unused_mut, unused_must_use, unused_variables)]
  fn start_server() {
    thread::spawn(move|| {
      let server = ServerBuilder::new().with_port(1025).build().unwrap();
      println!("starting web server");

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

    let mut fronts = HashMap::new();
    fronts.insert("lolcatho.st".to_owned(), vec![
      HttpFront { app_id: app_id1, hostname: "lolcatho.st".to_owned(), path_begin: uri1, port: 8080 },
      HttpFront { app_id: app_id2, hostname: "lolcatho.st".to_owned(), path_begin: uri2, port: 8080 },
      HttpFront { app_id: app_id3, hostname: "lolcatho.st".to_owned(), path_begin: uri3, port: 8080 }
    ]);
    fronts.insert("other.domain".to_owned(), vec![
      HttpFront { app_id: "app_1".to_owned(), hostname: "other.domain".to_owned(), path_begin: "/test".to_owned(), port: 8080 },
    ]);

    let (tx,rx) = channel::<ServerMessage>();

    let server_config = ServerConfiguration {
      instances: HashMap::new(),
      listeners: Slab::new(10),
      fronts:    fronts,
      tx:        tx
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
