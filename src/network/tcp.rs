#![allow(dead_code, unused_must_use, unused_variables, unused_imports)]

use std::thread::{self,Thread,Builder};
use std::sync::mpsc::{self,channel,Receiver};
use mio::tcp::*;
use mio::*;
use bytes::{ByteBuf,MutByteBuf};
use std::collections::HashMap;
use std::io::{self,Read,ErrorKind};
use nom::HexDisplay;
use std::error::Error;
use mio::util::Slab;
use std::net::SocketAddr;
use std::str::FromStr;
use time::precise_time_s;
use rand::random;
use network::{ClientResult,ServerMessage};

use messages::{TcpFront,Command,Instance};

const SERVER: Token = Token(0);

#[derive(Debug)]
pub enum TcpProxyOrder {
  Command(Command),
  Stop
}

#[derive(Debug,Clone,PartialEq,Eq)]
pub enum ConnectionStatus {
  Initial,
  ClientConnected,
  Connected,
  ClientClosed,
  ServerClosed,
  Closed
}

#[cfg(not(feature = "splice"))]
struct Client {
  sock:           TcpStream,
  backend:        Option<TcpStream>,
  front_buf:      Option<MutByteBuf>,
  back_buf:       Option<MutByteBuf>,
  token:          Option<Token>,
  backend_token:  Option<Token>,
  accept_token:   Token,
  back_interest:  EventSet,
  front_interest: EventSet,
  status:         ConnectionStatus,
  rx_count:       usize,
  tx_count:       usize
}

#[cfg(feature = "splice")]
struct Client {
  sock:           TcpStream,
  backend:        Option<TcpStream>,
  pipe_in:        splice::Pipe,
  pipe_out:       splice::Pipe,
  data_in:        bool,
  data_out:       bool,
  token:          Option<Token>,
  backend_token:  Option<Token>,
  accept_token:   Token,
  back_interest:  EventSet,
  front_interest: EventSet,
  status:         ConnectionStatus,
  rx_count:       usize,
  tx_count:       usize
}

#[cfg(not(feature = "splice"))]
impl Client {
  fn new(sock: TcpStream, accept_token: Token) -> Option<Client> {
    Some(Client {
      sock:           sock,
      backend:        None,
      front_buf:      Some(ByteBuf::mut_with_capacity(2048)),
      back_buf:       Some(ByteBuf::mut_with_capacity(2048)),
      token:          None,
      backend_token:  None,
      accept_token:   accept_token,
      back_interest:  EventSet::all(),
      front_interest: EventSet::all(),
      status:         ConnectionStatus::Connected,
      rx_count:       0,
      tx_count:       0
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
          //println!("FRONT [{}<-{}]: wrote {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);

          self.tx_count = self.tx_count + r;

          //self.front_interest.insert(EventSet::readable());
          self.front_interest.remove(EventSet::writable());
          self.back_interest.insert(EventSet::readable());
        }
        Err(e) =>  println!("not implemented; client err={:?}", e),
      }
      self.back_buf = Some(b.flip());
    }
    if let Some(ref sock) = self.backend {
      event_loop.reregister(sock, self.backend_token.unwrap(), self.back_interest, PollOpt::edge() | PollOpt::oneshot());
    }
    event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    ClientResult::Continue
  }

  fn readable(&mut self, event_loop: &mut EventLoop<Server>) -> ClientResult {
    let mut buf = self.front_buf.take().unwrap();
    //println!("in readable(): front_mut_buf contains {} bytes", buf.remaining());

    match self.sock.try_read_buf(&mut buf) {
      Ok(None) => {
        println!("We just got readable, but were unable to read from the socket?");
      }
      Ok(Some(r)) => {
        //println!("FRONT [{}->{}]: read {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);
        self.front_interest.remove(EventSet::readable());
        self.back_interest.insert(EventSet::writable());
        self.rx_count = self.rx_count + r;
        // prepare to provide this to writable
      }
      Err(e) => {
        println!("not implemented; client err={:?}", e);
        //self.front_interest.remove(EventSet::readable());
      }
    };
    self.front_buf = Some(buf);

    if let Some(ref sock) = self.backend {
      event_loop.reregister(sock, self.backend_token.unwrap(), self.back_interest, PollOpt::edge() | PollOpt::oneshot());
    }
    event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    ClientResult::Continue
  }

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
            //println!("BACK  [{}->{}]: wrote {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);

            self.front_interest.insert(EventSet::readable());
            self.back_interest.remove(EventSet::writable());
            self.back_interest.insert(EventSet::readable());
          }
          Err(e) =>  println!("not implemented; client err={:?}", e),
        }
      }
      self.front_buf = Some(b.flip());
    }
    if let Some(ref sock) = self.backend {
      event_loop.reregister(sock, self.backend_token.unwrap(), self.back_interest, PollOpt::edge() | PollOpt::oneshot());
    }
    event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    ClientResult::Continue
  }

  fn back_readable(&mut self, event_loop: &mut EventLoop<Server>) -> ClientResult {
    let mut buf = self.back_buf.take().unwrap();
    //println!("in back_readable(): back_mut_buf contains {} bytes", buf.remaining());

    if let Some(ref mut sock) = self.backend {
      match sock.try_read_buf(&mut buf) {
        Ok(None) => {
          println!("We just got readable, but were unable to read from the socket?");
        }
        Ok(Some(r)) => {
          //println!("BACK  [{}<-{}]: read {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);
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

    if let Some(ref sock) = self.backend {
      event_loop.reregister(sock, self.backend_token.unwrap(), self.back_interest, PollOpt::edge() | PollOpt::oneshot());
    }
    event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
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

#[cfg(feature = "splice")]
impl Client {
  fn new(sock: TcpStream, backend: TcpStream, accept_token: Token) -> Option<Client> {
    if let (Some(pipe_in), Some(pipe_out)) = (splice::create_pipe(), splice::create_pipe()) {
      Some(Client {
        sock:           sock,
        backend:        backend,
        pipe_in:        pipe_in,
        pipe_out:       pipe_out,
        data_in:        false,
        data_out:       false,
        token:          None,
        backend_token:  None,
        accept_token:   accept_token,
        back_interest:  EventSet::all(),
        front_interest: EventSet::all(),
        status:         ConnectionStatus::Initial,
        tx_count:       0,
        rx_count:       0
      })
    } else {
      None
    }
  }

  pub fn set_tokens(&mut self, token: Token, backend: Token) {
    self.token         = Some(token);
    self.backend_token = Some(backend);
  }

  fn writable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
    //println!("in writable()");
    if self.data_out {
      match splice::splice_out(self.pipe_out, &self.sock) {
        None => {
          //println!("client flushing buf; WOULDBLOCK");

          self.front_interest.insert(EventSet::writable());
        }
        Some(r) => {
          //FIXME what happens if not everything was written?
          //println!("FRONT [{}<-{}]: wrote {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);

          //self.front_interest.insert(EventSet::readable());
          self.front_interest.remove(EventSet::writable());
          self.back_interest.insert(EventSet::readable());
          self.data_out = false;
          self.tx_count = self.tx_count + r;
        }
      }
      event_loop.reregister(&self.backend, self.backend_token.unwrap(), self.back_interest, PollOpt::edge() | PollOpt::oneshot());
      event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    }
    Ok(())
  }

  fn readable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
    //println!("in readable(): front_mut_buf contains {} bytes", buf.remaining());

    match splice::splice_in(&self.sock, self.pipe_in) {
      None => {
        println!("We just got readable, but were unable to read from the socket?");
      }
      Some(r) => {
        //println!("FRONT [{}->{}]: read {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);
        self.front_interest.remove(EventSet::readable());
        self.back_interest.insert(EventSet::writable());
        self.data_in = true;
        self.rx_count = self.rx_count + r;
      }
    };

    event_loop.reregister(&self.backend, self.backend_token.unwrap(), self.back_interest, PollOpt::edge() | PollOpt::oneshot());
    event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    Ok(())
  }

  fn back_writable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
    //println!("in back_writable 2: front_buf contains {} bytes", buf.remaining());

    if self.data_in {
      match splice::splice_out(self.pipe_in, &self.backend) {
        None => {
          //println!("client flushing buf; WOULDBLOCK");

          self.back_interest.insert(EventSet::writable());
        }
        Some(r) => {
          //FIXME what happens if not everything was written?
          //println!("BACK  [{}->{}]: wrote {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);

          self.front_interest.insert(EventSet::readable());
          self.back_interest.remove(EventSet::writable());
          self.back_interest.insert(EventSet::readable());
          self.data_in = false;
        }
      }
      event_loop.reregister(&self.backend, self.backend_token.unwrap(), self.back_interest, PollOpt::edge() | PollOpt::oneshot());
      event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    }
    Ok(())
  }

  fn back_readable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
    //println!("in back_readable(): back_mut_buf contains {} bytes", buf.remaining());

    match splice::splice_in(&self.backend, self.pipe_out) {
      None => {
        println!("We just got readable, but were unable to read from the socket?");
      }
      Some(r) => {
        //println!("BACK  [{}<-{}]: read {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);
        self.back_interest.remove(EventSet::readable());
        self.front_interest.insert(EventSet::writable());
        self.data_out = true;
      }
    };

    event_loop.reregister(&self.backend, self.backend_token.unwrap(), self.back_interest, PollOpt::edge() | PollOpt::oneshot());
    event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    Ok(())
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
  fronts:    HashMap<String, Token>,
  instances: HashMap<String, Vec<SocketAddr>>,
  listeners: Slab<ApplicationListener>,
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

  pub fn add_tcp_front(&mut self, port: u16, app_id: &str, event_loop: &mut EventLoop<Server>) -> Option<Token> {
    let addr_string = String::from("127.0.0.1:") + &port.to_string();
    let front = &addr_string.parse().unwrap();

    if let Ok(listener) = TcpListener::bind(front) {
      let addresses = if let Some(ads) = self.instances.get(app_id) {
        ads.clone()
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
        self.listeners[tok].token = Some(tok);
        self.fronts.insert(String::from(app_id), tok);
        event_loop.register(&self.listeners[tok].sock, tok, EventSet::readable(), PollOpt::level()).unwrap();
        println!("registered listener for app {} on port {}", app_id, port);
        Some(tok)
      } else {
        println!("could not register listener for app {} on port {}", app_id, port);
        None
      }

    } else {
      println!("could not declare listener for app {} on port {}", app_id, port);
      None
    }
  }

  pub fn remove_tcp_front(&mut self, app_id: String, event_loop: &mut EventLoop<Server>) -> Option<Token>{
    println!("removing tcp_front {:?}", app_id);
    // ToDo
    // Removes all listeners for the given app_id
    // an app can't have two listeners. Is this a problem?
    if let Some(&tok) = self.fronts.get(&app_id) {
      if self.listeners.contains(tok) {
        event_loop.deregister(&self.listeners[tok].sock);
        self.listeners.remove(tok);
        println!("removed server {:?}", tok);
        //self.listeners[tok].sock.shutdown(Shutdown::Both);
        Some(tok)
      } else {
        None
      }
    } else {
      None
    }
  }

  pub fn add_instance(&mut self, app_id: &str, instance_address: &SocketAddr, event_loop: &mut EventLoop<Server>) -> Option<Token> {
    if let Some(addrs) = self.instances.get_mut(app_id) {
        addrs.push(*instance_address);
    }

    if self.instances.get(app_id).is_none() {
      self.instances.insert(String::from(app_id), vec![*instance_address]);
    }

    if let Some(&tok) = self.fronts.get(app_id) {
      let application_listener = &mut self.listeners[tok];

      application_listener.back_addresses.push(*instance_address);
      Some(tok)
    } else {
      println!("No front for this instance");
      None
    }
  }

  pub fn remove_instance(&mut self, app_id: &str, instance_address: &SocketAddr, event_loop: &mut EventLoop<Server>) -> Option<Token>{
      // ToDo
      None
  }

  pub fn accept(&mut self, token: Token) -> Option<(Client, bool)> {
    if self.listeners.contains(token) {
      let accepted = self.listeners[token].sock.accept();

      if let Ok(Some((frontend_sock, _))) = accepted {
        if let Some(c) = Client::new(frontend_sock, token) {
          return Some((c, true));
        }
      }
    }
    None
  }

  pub fn connect_to_backend(&mut self, client:&mut Client) ->Option<TcpStream> {
    let rnd = random::<usize>();
    let idx = rnd % self.listeners[client.accept_token].back_addresses.len();
    if let Some(backend_addr) = self.listeners[client.accept_token].back_addresses.get(idx) {
      TcpStream::connect(backend_addr).ok()
    } else {
      None
    }
  }

  fn notify(&mut self, event_loop: &mut EventLoop<Server>, message: TcpProxyOrder) {
    println!("notified: {:?}", message);
    match message {
      TcpProxyOrder::Command(Command::AddTcpFront(front)) => {
        println!("{:?}", front);
        if let Some(token) = self.add_tcp_front(front.port, &front.app_id, event_loop) {
          self.tx.send(ServerMessage::AddedFront);
        } else {
          println!("Couldn't add tcp front");
        }
      },
      TcpProxyOrder::Command(Command::RemoveTcpFront(front)) => {
        println!("{:?}", front);
        let _ = self.remove_tcp_front(front.app_id, event_loop);
        self.tx.send(ServerMessage::RemovedFront);
      },
      TcpProxyOrder::Command(Command::AddInstance(instance)) => {
        println!("{:?}", instance);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let addr = &addr_string.parse().unwrap();
        if let Some(token) = self.add_instance(&instance.app_id, addr, event_loop) {
          self.tx.send(ServerMessage::AddedInstance);
        } else {
          println!("Couldn't add tcp front");
        }
      },
      TcpProxyOrder::Command(Command::RemoveInstance(instance)) => {
        println!("{:?}", instance);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let addr = &addr_string.parse().unwrap();
        if let Some(token) = self.remove_instance(&instance.app_id, addr, event_loop) {
          self.tx.send(ServerMessage::RemovedInstance);
        } else {
          println!("Couldn't add tcp front");
        }
      },
      TcpProxyOrder::Stop                   => {
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
      configuration:   ServerConfiguration::new(max_listeners, tx),
      clients:         Slab::new_starting_at(Token(max_listeners), max_connections),
      backend:         Slab::new_starting_at(Token(max_listeners+max_connections), max_connections),
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
    if let Some((client, should_connect)) = self.configuration.accept(token) {
      if let Ok(client_token) = self.clients.insert(client) {
        event_loop.register(&self.clients[client_token].sock, client_token, EventSet::readable(), PollOpt::edge()).unwrap();
        &self.clients[client_token].set_front_token(client_token);
        if should_connect {
          self.connect_to_backend(event_loop, client_token);
        }
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
  type Message = TcpProxyOrder;

  fn ready(&mut self, event_loop: &mut EventLoop<Server>, token: Token, events: EventSet) {
    //println!("{:?} got events: {:?}", token, events);
    if events.is_readable() {
      //println!("{:?} is readable", token);
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
      if token.as_usize() < self.max_listeners {
        println!("should not happen: server {:?} closed", token);
      } else if token.as_usize() < self.max_listeners + self.max_connections {
        if self.clients.contains(token) {
          if self.clients[token].front_hup() == ClientResult::CloseClient {
            self.close_client(event_loop, token);
          }
        } else {
          println!("client {:?} was removed", token);
        }
      } else if token.as_usize() < self.max_listeners + 2 * self.max_connections {
        if let Some(tok) = self.get_client_token(token) {
          if self.clients[tok].front_hup() == ClientResult::CloseClient {
            self.close_client(event_loop, tok);
          }
        }
      }
    }
  }

  fn notify(&mut self, event_loop: &mut EventLoop<Self>, message: Self::Message) {
    println!("notified: {:?}", message);
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
  let mut event_loop = EventLoop::new().unwrap();


  println!("listen for connections");
  //event_loop.register(&listener, SERVER, EventSet::readable(), PollOpt::edge() | PollOpt::oneshot()).unwrap();
  let (tx,rx) = channel::<ServerMessage>();
  let mut s = Server::new(10, 500, tx);
  {
    let back: SocketAddr = FromStr::from_str("127.0.0.1:5678").unwrap();
    s.configuration.add_tcp_front(1234, "yolo", &mut event_loop);
    s.configuration.add_instance("yolo", &back, &mut event_loop);
  }
  {
    let back: SocketAddr = FromStr::from_str("127.0.0.1:5678").unwrap();
    s.configuration.add_tcp_front(1235, "yolo", &mut event_loop);
    s.configuration.add_instance("yolo", &back, &mut event_loop);
  }
  thread::spawn(move|| {
    println!("starting event loop");
    event_loop.run(&mut s).unwrap();
    println!("ending event loop");
  });
}

pub fn start_listener(max_listeners: usize, max_connections: usize, tx: mpsc::Sender<ServerMessage>) -> (Sender<TcpProxyOrder>,thread::JoinHandle<()>)  {
  let mut event_loop = EventLoop::new().unwrap();
  let channel = event_loop.channel();
  let notify_tx = tx.clone();
  let mut server = Server::new(max_listeners, max_connections, tx);
  server.configuration.add_tcp_front(8443, "yolo", &mut event_loop);

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
  use super::*;
  use std::net::{TcpListener, TcpStream, Shutdown};
  use std::io::{Read,Write};
  use std::{thread,str};

  #[allow(unused_mut, unused_must_use, unused_variables)]
  #[test]
  fn mi() {
    thread::spawn(|| { start_server(); });
    start();
    thread::sleep_ms(300);

    let mut s1 = TcpStream::connect("127.0.0.1:1234").unwrap();
    let mut s3 = TcpStream::connect("127.0.0.1:1234").unwrap();
    thread::sleep_ms(300);
    let mut s2 = TcpStream::connect("127.0.0.1:1234").unwrap();
    s1.write(&b"hello"[..]);
    println!("s1 sent");
    s2.write(&b"pouet pouet"[..]);
    println!("s2 sent");
    thread::sleep_ms(500);

    let mut res = [0; 128];
    s1.write(&b"coucou"[..]);
    let mut sz1 = s1.read(&mut res[..]).unwrap();
    println!("s1 received {:?}", str::from_utf8(&res[..sz1]));
    assert_eq!(&res[..sz1], &b"hello END"[..]);
    s3.shutdown(Shutdown::Both);
    let sz2 = s2.read(&mut res[..]).unwrap();
    println!("s2 received {:?}", str::from_utf8(&res[..sz2]));
    assert_eq!(&res[..sz2], &b"pouet pouet END"[..]);


    thread::sleep_ms(200);
    thread::sleep_ms(200);
    sz1 = s1.read(&mut res[..]).unwrap();
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
    let listener = TcpListener::bind("127.0.0.1:5678").unwrap();
    fn handle_client(stream: &mut TcpStream, id: u8) {
      let mut buf = [0; 128];
      let response = b" END";
      while let Ok(sz) = stream.read(&mut buf[..]) {
        if sz > 0 {
          //println!("[{}] {:?}", id, str::from_utf8(&buf[..sz]));
          stream.write(&buf[..sz]);
          thread::sleep_ms(20);
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
