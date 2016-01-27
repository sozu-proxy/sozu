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
use time::precise_time_s;
use rand::random;
use network::{ClientResult,ServerMessage};
use network::proxy::{Server,ProxyConfiguration,ProxyClient};
use network::buffer::Buffer;

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

#[derive(Debug)]
pub enum HttpProxyOrder {
  Command(Command),
  Stop
}

pub struct HttpProxy {
  pub state:             HttpState,
  pub front_buf:         Buffer,
  pub back_buf:          Buffer,
}

impl HttpProxy {
  pub fn readable(&mut self) -> ClientResult {
    //println!("front buffer({} bytes):\n{}", self.front_buf.data().len(), self.front_buf.data().to_hex(8));
    if ! self.state.is_front_proxying() {
      self.state = parse_request_until_stop(&self.state, &mut self.front_buf);
      //println!("parse_request_until_stop returned {:?} => advance: {}", self.state, self.state.req_position);
      if self.state.is_front_error() {
        return ClientResult::CloseClient;
      }
      if self.state.has_host() {
        return ClientResult::ConnectBackend;
      }
    } else {
      println!("state: {:?}", self.state);
    }
    ClientResult::Continue
  }

  pub fn writable(&mut self, copied: usize) -> ClientResult {
    self.state.back_copied(copied);

    // The response ended, so we can close the connection, or accept a new request
    // FIXME: with Server Side Events, new responses can come
    if self.state.is_back_proxying() && self.state.res_position == 0 {
      let sh = (self.state.front_should_keep_alive(), self.state.back_should_keep_alive());
      println!("RESPONSE ENDED, SHOULD WE KEEP ALIVE? {:?}", sh);
      let res = match sh {
        (true, true)  => ClientResult::Continue,     // just reset
        (true, false) => ClientResult::CloseBackend,
        (false, _)    => ClientResult::CloseBoth,    // no connection pooling yet
      };
      self.state.reset();
      res
    } else {
      ClientResult::Continue
    }
  }

  pub fn back_readable(&mut self) -> ClientResult {
    //println!("back buffer ({} bytes):\n{}", self.back_buf.data().len(), self.back_buf.data().to_hex(8));
    if ! self.state.is_back_proxying() {
      self.state = parse_response_until_stop(&self.state, &mut self.back_buf);
      //println!("parse_response_until_stop returned {:?} => advance: {}", self.state, self.state.res_position);
      if self.state.is_back_error() {
        return ClientResult::CloseBackend;
      }
    }
    ClientResult::Continue
  }

  pub fn back_writable(&mut self, copied: usize) -> ClientResult {
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

struct Client {
  sock:           TcpStream,
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

impl Client {
  fn new(sock: TcpStream) -> Option<Client> {
    Some(Client {
      sock:           sock,
      backend:        None,
      http_state:     HttpProxy {
        state:             HttpState::new(),
        front_buf:         Buffer::with_capacity(12000),
        back_buf:          Buffer::with_capacity(12000),
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


  pub fn tokens(&self) -> Option<(Token,Token)> {
    if let Some(front) = self.token {
      if let Some(back) = self.backend_token {
        return Some((front, back))
      }
    }
    None
  }

  pub fn close(&self) {
  }

  fn reregister(&mut self, event_loop: &mut EventLoop<HttpServer>) {
    self.front_interest.insert(EventSet::readable());
    self.front_interest.insert(EventSet::writable());
    self.back_interest.insert(EventSet::readable());
    self.back_interest.insert(EventSet::writable());
    //println!("REREGISTER: {:?}", self.tokens());
    if let Some(frontend_token) = self.token {
      //println!("reregister frontend {}: {:?}", frontend_token.as_usize(), self.front_interest);
      event_loop.reregister(&self.sock, frontend_token, self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    }
    if let Some(backend_token) = self.backend_token {
      if let Some(ref sock) = self.backend {
        //println!("reregister backend {}: {:?}", backend_token.as_usize(), self.back_interest);
        event_loop.reregister(sock, backend_token, self.back_interest, PollOpt::edge() | PollOpt::oneshot());
      }
    }
  }

}

impl ProxyClient<HttpServer> for Client {
  fn front_socket(&self) -> &TcpStream {
    &self.sock
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
    println!("HTTP PROXY [{} -> {}] CLOSED BACKEND", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize());
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
  fn readable(&mut self, event_loop: &mut EventLoop<HttpServer>) -> ClientResult {
    //println!("readable(): front_buf contains {} data, {} space", buf.available_data(), buf.available_space());
    match self.sock.read(&mut self.http_state.front_buf.space()) {
      Ok(0) => {},
      Ok(r) => {
        if let Some(back) = self.back_token() {
        //  println!("FRONT [{} -> {}]: read {} bytes", self.token.unwrap().as_usize(), back.as_usize(), r);
        } else {
        //  println!("FRONT [{}]: read {} bytes", self.token.unwrap().as_usize(), r);
        }
        self.http_state.front_buf.fill(r);
        self.reregister(event_loop);
        return self.http_state.readable();
      }
      Err(e) => match e.kind() {
        ErrorKind::WouldBlock => {}
        ErrorKind::BrokenPipe => {
          println!("broken pipe reading from the client");
          return ClientResult::CloseClient;
        },
        _ => {
          println!("not implemented; client err={:?}", e);
          return ClientResult::CloseBoth;
        },
      }
    }
    self.reregister(event_loop);
    ClientResult::Continue
  }

  // Forward content to client
  fn writable(&mut self, event_loop: &mut EventLoop<HttpServer>) -> ClientResult {
    //println!("writable(): back_buf contains {} data, {} space", self.back_buf.available_data(), self.back_buf.available_space());

    if self.http_state.back_buf.available_data() == 0 {
      return ClientResult::Continue;
    }

    let res = match self.sock.write(&(self.http_state.back_buf.data())[..self.http_state.back_to_copy()]) {
      Ok(0) => { ClientResult::Continue }
      Ok(r) => {
        self.http_state.back_buf.consume(r);
        //FIXME what happens if not everything was written?
        if let Some((front,back)) = self.tokens() {
        //  println!("FRONT [{}<-{}]: wrote {} bytes", front.as_usize(), back.as_usize(), r);
        }

        self.tx_count = self.tx_count + r;
        self.http_state.writable(r)
      }
      Err(e) => match e.kind() {
        ErrorKind::WouldBlock => { ClientResult::Continue }
        ErrorKind::BrokenPipe => {
          println!("broken pipe writing to the client");
          return ClientResult::CloseBoth;
        },
        _ => {
          println!("not implemented; client err={:?}", e);
          ClientResult::CloseBoth
        }
      }
    };

    self.reregister(event_loop);
    res
  }

  // Forward content to application
  fn back_writable(&mut self, event_loop: &mut EventLoop<HttpServer>) -> ClientResult {
    //println!("back_writable: front_buf contains {} data, {} space", buf.available_data(), buf.available_space());
    let tokens = self.tokens();
    let res = if let Some(ref mut sock) = self.backend {
      match sock.write(&(self.http_state.front_buf.data())[..self.http_state.front_to_copy()]) {
        Ok(0) => { ClientResult::Continue }
        Ok(r) => {
          //FIXME what happens if not everything was written?
          if let Some((front,back)) = tokens {
          //  println!("BACK  [{}->{}]: read {} bytes", front.as_usize(), back.as_usize(), r);
          }

          self.http_state.back_writable(r)
        }
        Err(e) => match e.kind() {
          ErrorKind::WouldBlock => { ClientResult::Continue },
          ErrorKind::BrokenPipe => {
            println!("broken pipe writing to the backend");
            return ClientResult::CloseBoth;
          },
          _ => {
            println!("not implemented; client err={:?}", e);
            return ClientResult::CloseBoth;
          },
        }
      }
    } else {
      ClientResult::CloseBoth
    };

    self.reregister(event_loop);
    res
  }

  // Read content from application
  fn back_readable(&mut self, event_loop: &mut EventLoop<HttpServer>) -> ClientResult {
    //println!("back_readable(): back_buf contains {} data, {} space", self.back_buf.available_data(), self.back_buf.available_space());
    let tokens = self.tokens();

    let res = if let Some(ref mut sock) = self.backend {
      match sock.read(&mut self.http_state.back_buf.space()) {
        Ok(0) => {
          //println!("We just got readable, but were unable to read from the socket?");
          ClientResult::Continue
        }
        Ok(r) => {
          self.http_state.back_buf.fill(r);
          if let Some((front,back)) = tokens {
          //  println!("BACK  [{}<-{}]: read {} bytes", front.as_usize(), back.as_usize(), r);
          }
          self.http_state.back_readable()
        }
        Err(e) => match e.kind() {
          ErrorKind::WouldBlock => { ClientResult::Continue },
          ErrorKind::BrokenPipe => {
            println!("broken pipe reading from the backend");
            return ClientResult::CloseBoth;
          },
          _ => {
            println!("not implemented; client err={:?}", e);
            return ClientResult::CloseBoth;
          }
        }
      }
    } else {
      ClientResult::CloseBoth
    };

    self.reregister(event_loop);
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
    println!("removing http_front {:?}", front);
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
        println!("Instance was already removed");
      }
  }

  pub fn backend_from_request(&self, host: &str, uri: &str) -> Option<SocketAddr> {
    if let Some(http_fronts) = self.fronts.get(host) {
      // ToDo get the front with the most specific matching path_begin
      if let Some(http_front) = http_fronts.get(0) {
        // ToDo round-robin on instances
        if let Some(app_instances) = self.instances.get(&http_front.app_id) {
          let rnd = random::<usize>();
          let idx = rnd % app_instances.len();
          println!("Connecting {} -> {:?}", host, app_instances.get(idx));
          app_instances.get(idx).map(|& addr| addr)
        } else {
          None
        }
      } else {
        None
      }
    } else {
      None
    }
  }

}

impl ProxyConfiguration<HttpServer,Client,HttpProxyOrder> for ServerConfiguration {
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
        println!("registered listener({}) on port {}", tok.as_usize(), port);
        Some(tok)
      } else {
        println!("could not register listener on port {}", port);
        None
      }

    } else {
      println!("could not declare listener on port {}", port);
      None
    }
  }

  fn connect_to_backend(&mut self, client: &mut Client) -> Option<TcpStream> {
    if let (Some(host), Some(rl)) = (client.http_state.state.get_host(), client.http_state.state.get_request_line()) {
      if let Some(back) = self.backend_from_request(&host, &rl.uri) {
        if let Ok(socket) = TcpStream::connect(&back) {
          client.status     = ConnectionStatus::Connected;
          return Some(socket);
        }
      }
    }
    None
  }

  fn notify(&mut self, event_loop: &mut EventLoop<HttpServer>, message: HttpProxyOrder) {
  // ToDo temporary
    println!("notified: {:?}", message);
    match message {
      HttpProxyOrder::Command(Command::AddHttpFront(front)) => {
        println!("add front {:?}", front);
          self.add_http_front(front, event_loop);
          self.tx.send(ServerMessage::AddedFront);
      },
      HttpProxyOrder::Command(Command::RemoveHttpFront(front)) => {
        println!("remove front {:?}", front);
        self.remove_http_front(front, event_loop);
        self.tx.send(ServerMessage::RemovedFront);
      },
      HttpProxyOrder::Command(Command::AddInstance(instance)) => {
        println!("add instance {:?}", instance);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let parsed:Option<SocketAddr> = addr_string.parse().ok();
        if let Some(addr) = parsed {
          self.add_instance(&instance.app_id, &addr, event_loop);
          self.tx.send(ServerMessage::AddedInstance);
        }
      },
      HttpProxyOrder::Command(Command::RemoveInstance(instance)) => {
        println!("remove instance {:?}", instance);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let parsed:Option<SocketAddr> = addr_string.parse().ok();
        if let Some(addr) = parsed {
          self.remove_instance(&instance.app_id, &addr, event_loop);
          self.tx.send(ServerMessage::RemovedInstance);
        }
      },
      HttpProxyOrder::Stop                   => {
        event_loop.shutdown();
      },
      _ => {
        println!("unsupported message, ignoring");
      }
    }
  }

  fn accept(&mut self, token: Token) -> Option<(Client, bool)> {
    if self.listeners.contains(token) {
      let accepted = self.listeners[token].sock.accept();

      if let Ok(Some((frontend_sock, _))) = accepted {
        if let Some(c) = Client::new(frontend_sock) {
          return Some((c, false))
        }
      }
    } else {
      println!("not found");
    }

    None
  }


}

pub type HttpServer = Server<ServerConfiguration,Client,HttpProxyOrder>;

pub fn start() {
  // ToDo temporary
  let mut event_loop = EventLoop::new().unwrap();

  let (tx,rx) = channel::<ServerMessage>();
  let channel = event_loop.channel();
  let notify_tx = tx.clone();

  let configuration = ServerConfiguration::new(1, tx);
  let mut server = HttpServer::new(1, 500, configuration);

  let join_guard = thread::spawn(move|| {
    println!("starting event loop");
    event_loop.run(&mut server).unwrap();
    println!("ending event loop");
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

pub fn start_listener(front: SocketAddr, max_listeners: usize, max_connections: usize, tx: mpsc::Sender<ServerMessage>) -> (Sender<HttpProxyOrder>,thread::JoinHandle<()>)  {
  let mut event_loop = EventLoop::new().unwrap();
  let channel = event_loop.channel();
  let notify_tx = tx.clone();

  let configuration = ServerConfiguration::new(max_listeners, tx);
  let mut server = HttpServer::new(max_listeners, max_connections, configuration);

  let join_guard = thread::spawn(move|| {
    println!("starting event loop");
    event_loop.run(&mut server).unwrap();
    println!("ending event loop");
    notify_tx.send(ServerMessage::Stopped);
  });

  (channel, join_guard)
}

#[cfg(test)]
mod tests {
  extern crate tiny_http;
  use super::*;
  use std::net::{TcpListener, TcpStream, Shutdown};
  use std::io::{Read,Write};
  use std::{thread,str};
  use std::sync::mpsc::channel;
  use std::net::SocketAddr;
  use std::str::FromStr;
  use std::time::Duration;
  use messages::{Command,HttpFront,Instance};
  use network::ServerMessage;

  #[allow(unused_mut, unused_must_use, unused_variables)]
  #[test]
  fn mi() {
    thread::spawn(|| { start_server(); });
    let front: SocketAddr = FromStr::from_str("127.0.0.1:1024").unwrap();
    let (tx,rx) = channel::<ServerMessage>();
    let (sender, jg) = start_listener(front, 10, 10, tx.clone());
    let front = HttpFront { app_id: String::from("app_1"), hostname: String::from("localhost:1024"), path_begin: String::from("/"), port: 1024 };
    sender.send(HttpProxyOrder::Command(Command::AddHttpFront(front)));
    let instance = Instance { app_id: String::from("app_1"), ip_address: String::from("127.0.0.1"), port: 1025 };
    sender.send(HttpProxyOrder::Command(Command::AddInstance(instance)));
    println!("test received: {:?}", rx.recv());
    println!("test received: {:?}", rx.recv());
    thread::sleep_ms(300);

    let mut client = TcpStream::connect(("127.0.0.1", 1024)).unwrap();
    // 5 seconds of timeout
    client.set_read_timeout(Some(Duration::new(5,0)));
    thread::sleep_ms(100);
    let mut w  = client.write(&b"GET / HTTP/1.1\r\nHost: localhost:1024\r\nConnection: Close\r\n\r\n"[..]);
    println!("http client write: {:?}", w);
    let mut buffer = [0;4096];
    thread::sleep_ms(500);
    let mut r = client.read(&mut buffer[..]);
    println!("http client read: {:?}", r);
    match r {
      Err(e)      => assert!(false, "client request should not fail. Error: {:?}",e),
      Ok(sz) => {
        // Read the Response.
        println!("read response");

        println!("Response: {}", str::from_utf8(&buffer[..]).unwrap());

        //thread::sleep_ms(300);
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
}
