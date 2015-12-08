#![allow(unused_imports)]

use std::thread::{self,Thread,Builder};
use std::sync::mpsc::{self,channel,Receiver};
use mio::tcp::*;
use std::io::{self,Read,ErrorKind};
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

//use parser::http11::{RRequestLine,RequestHeader,request_line,headers};
use parser::http11::{HttpState,parse_headers};

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

struct Client {
  sock:           TcpStream,
  backend:        Option<TcpStream>,
  http_state:     HttpState,
  front_buf:      Option<MutByteBuf>,
  back_buf:       Option<MutByteBuf>,
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
      http_state:     HttpState::Initial,
      front_buf:      Some(ByteBuf::mut_with_capacity(2048)),
      back_buf:       Some(ByteBuf::mut_with_capacity(2048)),
      token:          None,
      backend_token:  None,
      back_interest:  EventSet::all(),
      front_interest: EventSet::all(),
      status:         ConnectionStatus::ClientConnected,
      rx_count:       0,
      tx_count:       0,
    })
  }

  pub fn set_front_token(&mut self, token: Token) {
    self.token         = Some(token);
  }

  pub fn set_back_token(&mut self, token: Token) {
    self.backend_token = Some(token);
  }

  pub fn set_tokens(&mut self, token: Token, backend: Token) {
    self.token         = Some(token);
    self.backend_token = Some(backend);
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

  // Forward content to client
  fn writable(&mut self, event_loop: &mut EventLoop<Server>) -> ClientResult {
    //println!("in writable()");
    if let Some(buf) = self.back_buf.take() {
      //println!("in writable 2: back_buf contains {} bytes", buf.remaining());

      let mut b = buf.flip();
      match self.sock.try_write_buf(&mut b) {
        Ok(None) => {
          println!("client flushing buf; WOULDBLOCK");

          self.front_interest.insert(EventSet::writable());
        }
        Ok(Some(r)) => {
          //FIXME what happens if not everything was written?
          //if let Some((front,back)) = self.tokens() {
          //  println!("FRONT [{}<-{}]: wrote {} bytes", front.as_usize(), back.as_usize(), r);
          //}

          self.tx_count = self.tx_count + r;

          //self.front_interest.insert(EventSet::readable());
          self.front_interest.remove(EventSet::writable());
          self.back_interest.insert(EventSet::readable());
        }
        Err(e) =>  println!("not implemented; client err={:?}", e),
      }
      self.back_buf = Some(b.flip());
    }
    if let Some((frontend_token,backend_token)) = self.tokens() {
      if let Some(ref sock) = self.backend {
        event_loop.reregister(sock, backend_token, self.back_interest, PollOpt::edge()).unwrap();
      }
      event_loop.reregister(&self.sock, frontend_token, self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    }
    ClientResult::Continue
  }

  fn has_host(&self) -> bool {
    if let HttpState::HasHost(_, _, _) = self.http_state {
      true
    } else {
      false
    }
  }
  fn is_proxying(&self) -> bool {
    if let HttpState::Proxying(_, _) = self.http_state {
      true
    } else {
      false
    }
  }

  fn reregister(&mut self, event_loop: &mut EventLoop<Server>) {
    self.front_interest.remove(EventSet::readable());
    self.back_interest.insert(EventSet::writable());
    if let Some((frontend_token,backend_token)) = self.tokens() {
      if let Some(ref sock) = self.backend {
        event_loop.reregister(sock, backend_token, EventSet::readable(), PollOpt::edge()).unwrap();
      }
      event_loop.reregister(&self.sock, frontend_token, self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    }
  }

  // Read content from the client
  fn readable(&mut self, event_loop: &mut EventLoop<Server>) -> ClientResult {
    //println!("in readable()");
    //println!("in readable(): front_mut_buf contains {} bytes", buf.remaining());

    if let Some(mut buf) = self.front_buf.take() {
      match self.sock.try_read_buf(&mut buf) {
        Ok(None) => {
          self.front_interest.insert(EventSet::readable());
        },
        Ok(Some(r)) => {
          println!("FRONT [{:?}]: read {} bytes", self.token, r);
          if self.is_proxying() {
            //if let Some((front,back)) = self.tokens() {
            //  println!("FRONT [{}->{}]: read {} bytes", front.as_usize(), back.as_usize(), r);
            //}
            self.reregister(event_loop);
            self.rx_count = self.rx_count + r;
          } else {
            let state = parse_headers(&self.http_state, &buf.bytes());
            if let HttpState::Error(_) = state {
              self.http_state = state;
              self.front_buf = Some(buf);
              return ClientResult::CloseClient;
            }
            self.http_state = state;
            //println!("new state: {:?}", self.http_state);
            if self.has_host() {
              self.rx_count = buf.remaining();
              self.reregister(event_loop);
              self.front_buf = Some(buf);
              //println!("is now proxying, front buf flipped");
              return ClientResult::ConnectBackend;
            } else {
              self.front_interest.insert(EventSet::readable());
              if let Some(frontend_token) = self.token {
                event_loop.reregister(&self.sock, frontend_token, self.front_interest, PollOpt::edge() | PollOpt::oneshot());
              }
            }
          }
        }
        Err(e) =>  println!("not implemented; client err={:?}", e),
      }
      self.front_buf = Some(buf);
    } else {
      println!("FRONT [{:?}]: front_mut_buf unavailable", self.token);
    }
    ClientResult::Continue
  }

  // Forward content to application
  fn back_writable(&mut self, event_loop: &mut EventLoop<Server>) -> ClientResult {
    if let Some(buf) = self.front_buf.take() {
      //println!("in back_writable 2: front_buf contains {} bytes", buf.remaining());

      let mut b = buf.flip();
      if let Some(ref mut sock) = self.backend {
        match sock.try_write_buf(&mut b) {
          Ok(None) => {
            println!("client flushing buf; WOULDBLOCK");

            self.back_interest.insert(EventSet::writable());
          }
          Ok(Some(r)) => {
            //FIXME what happens if not everything was written?
            //if let Some((front,back)) = self.tokens() {
            //  println!("BACK [{}->{}]: read {} bytes", front.as_usize(), back.as_usize(), r);
            //}

            self.front_interest.insert(EventSet::readable());
            self.back_interest.remove(EventSet::writable());
            self.back_interest.insert(EventSet::readable());
          }
          Err(e) =>  println!("not implemented; client err={:?}", e),
        }
      }
      self.front_buf = Some(b.flip());
    } else {
      println!("BACK [{:?}]: front_buf unavailable", self.token);
    }

    if let Some((frontend_token,backend_token)) = self.tokens() {
      if let Some(ref sock) = self.backend {
        event_loop.reregister(sock, backend_token, self.back_interest, PollOpt::edge()).unwrap();
      }
      event_loop.reregister(&self.sock, frontend_token, self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    }
    ClientResult::Continue
  }

  // Read content from application
  fn back_readable(&mut self, event_loop: &mut EventLoop<Server>) -> ClientResult {
    if let Some(mut buf) = self.back_buf.take() {
      //println!("in back_readable(): back_mut_buf contains {} bytes", buf.remaining());

      if let Some(ref mut sock) = self.backend {
        match sock.try_read_buf(&mut buf) {
          Ok(None) => {
            println!("We just got readable, but were unable to read from the socket?");
          }
          Ok(Some(r)) => {
            //if let Some((front,back)) = self.tokens() {
            //  println!("BACK [{}<-{}]: read {} bytes", front.as_usize(), back.as_usize(), r);
            //}
            self.back_interest.remove(EventSet::readable());
            self.front_interest.insert(EventSet::writable());
            // prepare to provide this to writable
          }
          Err(e) => {
            println!("not implemented; client err={:?}", e);
            //self.interest.remove(EventSet::readable());
          }
        };
      }
      self.back_buf = Some(buf);
    }

    if let Some((frontend_token,backend_token)) = self.tokens() {
      if let Some(ref sock) = self.backend {
        event_loop.reregister(sock, backend_token, self.back_interest, PollOpt::edge()).unwrap();
      }
      event_loop.reregister(&self.sock, frontend_token, self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    }
    ClientResult::Continue
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
  pub fn add_tcp_front(&mut self, port: u16, event_loop: &mut EventLoop<Server>) -> Option<Token> {
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
        event_loop.register(&self.listeners[tok].sock, tok, EventSet::readable(), PollOpt::level()).unwrap();
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

  pub fn add_http_front(&mut self, http_front: HttpFront, event_loop: &mut EventLoop<Server>) {
    let front2 = http_front.clone();
    let front3 = http_front.clone();
    if let Some(fronts) = self.fronts.get_mut(&http_front.hostname) {
        fronts.push(front2);
    }

    if self.listeners.iter().find(|&l| l.front_address.port() == http_front.port).is_none() {
      self.add_tcp_front(http_front.port, event_loop);
    }

    if self.fronts.get(&http_front.hostname).is_none() {
      self.fronts.insert(http_front.hostname, vec![front3]);
    }
  }

  pub fn remove_http_front(&mut self, front: HttpFront, event_loop: &mut EventLoop<Server>) {
    println!("removing http_front {:?}", front);
    if let Some(fronts) = self.fronts.get_mut(&front.hostname) {
      fronts.retain(|f| f != &front);
    }
  }

  pub fn add_instance(&mut self, app_id: &str, instance_address: &SocketAddr, event_loop: &mut EventLoop<Server>) {
    if let Some(addrs) = self.instances.get_mut(app_id) {
        addrs.push(*instance_address);
    }

    if self.instances.get(app_id).is_none() {
      self.instances.insert(String::from(app_id), vec![*instance_address]);
    }
  }

  pub fn remove_instance(&mut self, app_id: &str, instance_address: &SocketAddr, event_loop: &mut EventLoop<Server>) {
      if let Some(instances) = self.instances.get_mut(app_id) {
        instances.retain(|addr| addr != instance_address);
      } else {
        println!("Instance was already removed");
      }
  }

  pub fn backend_from_request(&self, host: &str, uri: &str) -> Option<SocketAddr> {
    println!("Getting a backend for {}", host);
    if let Some(http_fronts) = self.fronts.get(host) {
      // ToDo get the front with the most specific matching path_begin
      println!("Choosing a front from {:?}", http_fronts);
      if let Some(http_front) = http_fronts.get(0) {
        // ToDo round-robin on instances
        println!("Choosing an instance from {:?}", self.instances.get(&http_front.app_id));
        if let Some(app_instances) = self.instances.get(&http_front.app_id) {
          let rnd = random::<usize>();
          let idx = rnd % app_instances.len();
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

  pub fn accept(&mut self, token: Token) -> Option<Client> {
    println!("configuration accept({:?})", token);
    if self.listeners.contains(token) {
      let accepted = self.listeners[token].sock.accept();

      if let Ok(Some((frontend_sock, _))) = accepted {
        return Client::new(frontend_sock);
      }
    } else {
      println!("not found");
    }

    None
  }

  pub fn connect_to_backend(&mut self, client: &mut Client) -> Option<TcpStream> {
    if let (Some(host), Some(rl)) = (client.http_state.get_host(), client.http_state.get_request_line()) {
      if let Some(back) = self.backend_from_request(&host, &rl.uri) {
        if let Ok(socket) = TcpStream::connect(&back) {
          client.http_state = HttpState::Proxying(rl, host);
          client.status     = ConnectionStatus::Connected;
          return Some(socket);
        }
      }
    }
    None
  }

  fn notify(&mut self, event_loop: &mut EventLoop<Server>, message: HttpProxyOrder) {
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
}

pub struct Server {
  configuration:   ServerConfiguration,
  clients:         Slab<Client>,
  backend:         Slab<ClientToken>,
  max_listeners:   usize,
  max_connections: usize,
}

impl Server {
  fn new(max_listeners: usize, max_connections: usize, tx: mpsc::Sender<ServerMessage>) -> Server {
    Server {
      configuration: ServerConfiguration {
        fronts:    HashMap::new(),
        instances: HashMap::new(),
        listeners: Slab::new_starting_at(Token(0), max_listeners),
        tx:        tx
      },
      clients:         Slab::new_starting_at(Token(max_listeners), max_connections),
      backend:         Slab::new_starting_at(Token(max_listeners + max_connections), max_connections),
      max_listeners:   max_listeners,
      max_connections: max_connections,
    }
  }

  pub fn close_client(&mut self, event_loop: &mut EventLoop<Server>, token: Token) {
    self.clients[token].sock.shutdown(Shutdown::Both);
    event_loop.deregister(&self.clients[token].sock);
    if let Some(ref sock) = self.clients[token].backend {
      sock.shutdown(Shutdown::Both);
      event_loop.deregister(sock);
    }

    if let Some(backend_token) = self.clients[token].backend_token {
      if self.backend.contains(backend_token) {
        self.backend.remove(backend_token);
      }
    }
    self.clients.remove(token);
  }

  pub fn accept(&mut self, event_loop: &mut EventLoop<Server>, token: Token) {
    if let Some(client) = self.configuration.accept(token) {
      if let Ok(client_token) = self.clients.insert(client) {
        event_loop.register(&self.clients[client_token].sock, client_token, EventSet::readable(), PollOpt::edge()).unwrap();
        self.clients[client_token].set_front_token(client_token);
      } else {
        println!("could not add client to slab");
      }
    } else {
      println!("could not create a client");
    }
  }

  pub fn connect_to_backend(&mut self, event_loop: &mut EventLoop<Server>, token: Token) {
    if let Some(socket) = self.configuration.connect_to_backend(&mut self.clients[token]) {
      if let Ok(backend_token) = self.backend.insert(token) {
        //println!("backend connected and stored");
        self.clients[token].backend       = Some(socket);
        self.clients[token].backend_token = Some(backend_token);

        if let Some(ref sock) = self.clients[token].backend {
          event_loop.register(sock, backend_token, EventSet::writable(), PollOpt::edge()).unwrap();
        }
        return;
      }
    }
    self.close_client(event_loop, token);
  }

  pub fn get_client_token(&self, token: Token) -> Option<Token> {
    if token.as_usize() < self.max_listeners {
      None
    } else if token.as_usize() < self.max_listeners + self.max_connections && self.clients.contains(token) {
      Some(token)
    } else if token.as_usize() < self.max_listeners + 2 * self.max_connections && self.backend.contains(token) {
      if self.clients.contains(self.backend[token]) {
        Some(self.backend[token])
      } else {
        None
      }
    } else {
      None
    }
  }

  pub fn interpret_client_order(&mut self, event_loop: &mut EventLoop<Server>, token: Token, order: ClientResult) {
    match order {
      ClientResult::CloseClient    => self.close_client(event_loop, token),
      ClientResult::ConnectBackend => self.connect_to_backend(event_loop, token),
      ClientResult::Continue       => {}
    }
  }
}

impl Handler for Server {
  type Timeout = usize;
  type Message = HttpProxyOrder;

  fn ready(&mut self, event_loop: &mut EventLoop<Server>, token: Token, events: EventSet) {
    //println!("{:?} got events: {:?}", token, events);
    if events.is_readable() {
      println!("REA({})", token.as_usize());
      println!("{:?} is readable", token);
      println!("max listeners: {}", self.max_listeners);
      if token.as_usize() < self.max_listeners {
        self.accept(event_loop, token)
      } else if token.as_usize() < self.max_listeners + self.max_connections {
        if self.clients.contains(token) {
          let order = self.clients[token].readable(event_loop);
          self.interpret_client_order(event_loop, token, order);
        } else {
          println!("client {:?} was removed", token);
        }
      } else if token.as_usize() < self.max_listeners + 2 * self.max_connections {
        if let Some(tok) = self.get_client_token(token) {
          self.clients[tok].back_readable(event_loop);
        }
      }
    }

    if events.is_writable() {
      //println!("{:?} is writable", token);
      println!("WRI({})", token.as_usize());
      if token.as_usize() < self.max_listeners {
        println!("received writable for listener {:?}, this should not happen", token);
      } else  if token.as_usize() < self.max_listeners + self.max_connections {
        if self.clients.contains(token) {
          self.clients[token].writable(event_loop);
        } else {
          println!("client {:?} was removed", token);
        }
      } else if token.as_usize() < self.max_listeners + 2 * self.max_connections {
        if let Some(tok) = self.get_client_token(token) {
            self.clients[tok].back_writable(event_loop);
        }
      }
    }

    if events.is_hup() {
      println!("HUP({})", token.as_usize());
      if token == Token(0) {
        println!("should not happen: server {:?} closed", token);
      } else if token.as_usize() < self.max_listeners + self.max_connections {
        if self.clients.contains(token) {
          println!("client {:?} got hup", token.as_usize());
          if self.clients[token].front_hup() == ClientResult::CloseClient {
            self.close_client(event_loop, token);
          }
        } else {
          println!("client {:?} was already removed", token);
        }
      } else if token.as_usize() < self.max_listeners + 2 * self.max_connections {
        if let Some(tok) = self.get_client_token(token) {
          println!("server {} got hup (for client {})", token.as_usize(), tok.as_usize());
          println!("removing server {:?}", token);
          if self.clients[tok].back_hup() == ClientResult::CloseClient {
            self.close_client(event_loop, tok);
          }
        }
      }
      println!("end_hup");
    }
  }

  fn notify(&mut self, event_loop: &mut EventLoop<Self>, message: Self::Message) {
    self.configuration.notify(event_loop, message);
  }

  fn timeout(&mut self, event_loop: &mut EventLoop<Self>, timeout: Self::Timeout) {
    println!("timeout");
  }

  fn interrupted(&mut self, event_loop: &mut EventLoop<Self>) {
    println!("interrupted");
  }
}

pub fn start() {
  // ToDo temporary
  let mut event_loop = EventLoop::new().unwrap();

  let (tx,rx) = channel::<ServerMessage>();
  let channel = event_loop.channel();
  let notify_tx = tx.clone();

  let mut server = Server::new(1, 500, tx);

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

  let mut server = Server::new(max_listeners, max_connections, tx);

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
