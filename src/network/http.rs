#![allow(unused_imports)]

use std::thread::{self,Thread,Builder};
use std::sync::mpsc::{self,channel,Receiver};
use mio::tcp::*;
use std::io::{self,Read,ErrorKind};
use mio::*;
use mio::buf::{ByteBuf,MutByteBuf};
use std::collections::HashMap;
use nom::{HexDisplay,IResult};
use std::error::Error;
use mio::util::Slab;
use std::net::SocketAddr;
use std::str::{FromStr, from_utf8};
use time::precise_time_s;
use rand::random;

use parser::http11::{RRequestLine,RequestHeader,request_line,headers};

use messages::{Command,HttpFront};

pub type Host = String;

#[derive(Debug,Clone)]
pub enum ErrorState {
  InvalidHttp,
  MissingHost
}

#[derive(Debug,Clone)]
pub enum LengthInformation {
  Length(usize),
  Chunked,
  Compressed
}

type BackendToken = Token;
#[derive(Debug,Clone)]
pub enum HttpState {
  Initial,
  Error(ErrorState),
  HasRequestLine(usize, RRequestLine),
  HasHost(usize, RRequestLine, Host),
  HeadersParsed(RRequestLine, Host, LengthInformation),
  Proxying(RRequestLine, Host)
  //Proxying(RRequestLine, Host, LengthInformation, BackendToken)
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

impl HttpState {
  pub fn get_host(&self) -> Option<String> {
    match self {
      &HttpState::HasHost(_,_, ref host) |
      &HttpState::Proxying(_, ref host)=> Some(host.clone()),
      _ => None
    }
  }

  pub fn get_uri(&self) -> Option<String> {
    match self {
      &HttpState::HasRequestLine(_, ref rl) |
      &HttpState::HasHost(_, ref rl,_)      |
      &HttpState::Proxying(ref rl, _)         => Some(rl.uri.clone()),
      _ => None
    }
  }

  pub fn get_request_line(&self) -> Option<RRequestLine> {
    match self {
      &HttpState::HasRequestLine(_, ref rl) |
      &HttpState::HasHost(_, ref rl,_)      |
      &HttpState::Proxying(ref rl, _)         => Some(rl.clone()),
      _ => None
    }
  }
}

#[derive(Debug)]
pub enum HttpProxyOrder {
  Command(Command),
  Stop
}

#[derive(Debug)]
pub enum ServerMessage {
  AddedHttpFront,
  RemovedHttpFront,
  AddedInstance,
  RemovedInstance,
  Stopped
}


struct Client {
  sock:           TcpStream,
  backend:        Option<TcpStream>,
  http_state:     HttpState,
  front_buf:      Option<ByteBuf>,
  front_mut_buf:  Option<MutByteBuf>,
  back_buf:       Option<ByteBuf>,
  back_mut_buf:   Option<MutByteBuf>,
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
      front_buf:      None,
      front_mut_buf:  Some(ByteBuf::mut_with_capacity(2048)),
      back_buf:       None,
      back_mut_buf:   Some(ByteBuf::mut_with_capacity(2048)),
      token:          None,
      backend_token:  None,
      back_interest:  EventSet::all(),
      front_interest: EventSet::all(),
      status:         ConnectionStatus::Initial,
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

  pub fn close(&self) {
  }

  fn parse_headers(state: &HttpState, buf: &MutByteBuf) -> HttpState {
    match state {
      &HttpState::Initial => {
        //println!("buf: {}", buf.bytes().to_hex(8));
        match request_line(buf.bytes()) {
          IResult::Error(e) => {
            //println!("error: {:?}", e);
            HttpState::Error(ErrorState::InvalidHttp)
          },
          IResult::Incomplete(_) => {
            state.clone()
          },
          IResult::Done(i, r)    => {
            if let Some(rl) = RRequestLine::fromRequestLine(r) {
              let s = HttpState::HasRequestLine(buf.bytes().offset(i), rl);
              //println!("now in state: {:?}", s);
              Client::parse_headers(&s, buf)
            } else {
              HttpState::Error(ErrorState::InvalidHttp)
            }
          }
        }
      },
      &HttpState::HasRequestLine(pos, ref rl) => {
        //println!("parsing headers from:\n{}", (&buf.bytes()[pos..]).to_hex(8));
        match headers(&buf.bytes()[pos..]) {
          IResult::Error(e) => {
            //println!("error: {:?}", e);
            HttpState::Error(ErrorState::InvalidHttp)
          },
          IResult::Incomplete(_) => {
            state.clone()
          },
          IResult::Done(i, v)    => {
            //println!("got headers: {:?}", v);
            for header in v.iter() {
              if from_utf8(header.name) == Ok("Host") {
                if let Ok(host) = from_utf8(header.value) {
                  return HttpState::HasHost(buf.bytes().offset(i), rl.clone(), String::from(host));
                } else {
                  return HttpState::Error(ErrorState::InvalidHttp);
                }
              }
            }
            HttpState::HasRequestLine(buf.bytes().offset(i), rl.clone())
         }
        }
      },
      //HasHost(usize,RRequestLine, Host),
      _ => {
        panic!("unimplemented state: {:?}", state);
      }
    }
  }

  // Forward content to client
  fn writable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
    //println!("in writable()");
    if let Some(mut buf) = self.back_buf.take() {
      //println!("in writable 2: back_buf contains {} bytes", buf.remaining());

      match self.sock.try_write_buf(&mut buf) {
        Ok(None) => {
          println!("client flushing buf; WOULDBLOCK");

          self.back_buf = Some(buf);
          self.front_interest.insert(EventSet::writable());
        }
        Ok(Some(r)) => {
          //FIXME what happens if not everything was written?
          //println!("FRONT [{}<-{}]: wrote {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);

          self.back_mut_buf = Some(buf.flip());
          self.tx_count = self.tx_count + r;

          //self.front_interest.insert(EventSet::readable());
          self.front_interest.remove(EventSet::writable());
          self.back_interest.insert(EventSet::readable());
        }
        Err(e) =>  println!("not implemented; client err={:?}", e),
      }
    }
    if let Some(ref sock) = self.backend {
      event_loop.reregister(sock, self.backend_token.unwrap(), self.back_interest, PollOpt::edge()).unwrap();
    }
    event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    Ok(())
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

  fn flip_front_buf(&mut self, buf: MutByteBuf, event_loop: &mut EventLoop<Server>) {
    self.front_interest.remove(EventSet::readable());
    self.back_interest.insert(EventSet::writable());
    // prepare to provide this to writable
    self.front_buf = Some(buf.flip());
    if let Some(ref sock) = self.backend {
      event_loop.reregister(sock, self.backend_token.unwrap(), EventSet::readable(), PollOpt::edge()).unwrap();
    }
    //event_loop.reregister(&self.backend.unwrap(), self.backend_token.unwrap(), self.back_interest, PollOpt::edge() | PollOpt::oneshot());
    event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
  }

  // Read content from the client
  fn readable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
    //println!("in readable()");
    //println!("in readable(): front_mut_buf contains {} bytes", buf.remaining());

    let mut buf = self.front_mut_buf.take().unwrap();
    self.sock.try_read_buf(&mut buf).map(|res| {
      if let Some(r) = res {
        println!("FRONT [{:?}]: read {} bytes", self.token, r);
        if self.is_proxying() {
          //println!("FRONT [{}->{}]: read {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);
          self.flip_front_buf(buf, event_loop);
          self.rx_count = self.rx_count + r;
        } else {
          let state = Client::parse_headers(&self.http_state, &buf);
          if let HttpState::Error(_) = state {
            self.front_mut_buf = Some(buf);
            self.http_state = state;
            return;
          }
          self.http_state = state;
          //println!("new state: {:?}", self.http_state);
          if self.has_host() {
            self.rx_count = buf.remaining();
            self.flip_front_buf(buf, event_loop);
            //println!("is now proxying, front buf flipped");
          } else {
            self.front_mut_buf = Some(buf);
            self.front_interest.insert(EventSet::readable());
            event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
          }
        }
      }
    });
    Ok(())
  }

  // Forward content to application
  fn back_writable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
    if let Some(mut buf) = self.front_buf.take() {
      //println!("in back_writable 2: front_buf contains {} bytes", buf.remaining());

      if let Some(ref mut sock) = self.backend {
        match sock.try_write_buf(&mut buf) {
          Ok(None) => {
            println!("client flushing buf; WOULDBLOCK");

            self.front_buf = Some(buf);
            self.back_interest.insert(EventSet::writable());
          }
          Ok(Some(r)) => {
            //FIXME what happens if not everything was written?
            //println!("BACK  [{}->{}]: wrote {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);

            self.front_mut_buf = Some(buf.flip());

            self.front_interest.insert(EventSet::readable());
            self.back_interest.remove(EventSet::writable());
            self.back_interest.insert(EventSet::readable());
          }
          Err(e) =>  println!("not implemented; client err={:?}", e),
        }
      }
    }

    if let Some(ref sock) = self.backend {
      event_loop.reregister(sock, self.backend_token.unwrap(), self.back_interest, PollOpt::edge()).unwrap();
    }
    event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    Ok(())
  }

  // Read content from application
  fn back_readable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
    let mut buf = self.back_mut_buf.take().unwrap();
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
          self.back_buf = Some(buf.flip());
        }
        Err(e) => {
          println!("not implemented; client err={:?}", e);
          //self.interest.remove(EventSet::readable());
        }
      };
    }

    if let Some(ref sock) = self.backend {
      event_loop.reregister(sock, self.backend_token.unwrap(), self.back_interest, PollOpt::edge()).unwrap();
    }
    event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    Ok(())
  }
}


pub struct ApplicationListener {
  sock:           TcpListener,
  token:          Token,
  front_address:  SocketAddr
}

type ClientToken = Token;

pub struct Server {
  instances:       HashMap<String, Vec<SocketAddr>>,
  listener:        ApplicationListener,
  fronts:          HashMap<String, Vec<HttpFront>>,
  clients:         Slab<Client>,
  backend:         Slab<ClientToken>,
  max_listeners:   usize,
  max_connections: usize,
  tx:              mpsc::Sender<ServerMessage>
}

impl Server {
  fn new(listener: ApplicationListener, max_connections: usize, tx: mpsc::Sender<ServerMessage>) -> Server {
    Server {
      instances:       HashMap::new(),
      listener:        listener,
      fronts:          HashMap::new(),
      clients:         Slab::new_starting_at(Token(1), max_connections),
      backend:         Slab::new_starting_at(Token(1 + max_connections), max_connections),
      max_listeners:   1,
      max_connections: max_connections,
      tx:              tx
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

  pub fn add_http_front(&mut self, http_front: HttpFront, event_loop: &mut EventLoop<Server>) {
    let front2 = http_front.clone();
    let front3 = http_front.clone();
    if let Some(fronts) = self.fronts.get_mut(&http_front.hostname) {
        fronts.push(front2);
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

  pub fn backend_from_request(&self, host: &String, uri: &String) -> Option<SocketAddr> {
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

  pub fn accept(&mut self, event_loop: &mut EventLoop<Server>, token: Token) {
    let application_listener = &self.listener;
    let accepted = application_listener.sock.accept();

    if let Ok(Some(frontend_sock)) = accepted {
      if let Some(client) = Client::new(frontend_sock) {
        if let Ok(client_token) = self.clients.insert(client) {
            event_loop.register_opt(&self.clients[client_token].sock, client_token, EventSet::readable(), PollOpt::edge()).unwrap();
            self.clients[client_token].set_front_token(client_token);
            self.clients[client_token].status = ConnectionStatus::ClientConnected;
        } else {
          println!("could not add client to slab");
        }
      } else {
        println!("could not create a client");
      }
    } else {
      println!("could not accept connection: {:?}", accepted);
    }
  }

  pub fn connect_to_backend(&mut self, event_loop: &mut EventLoop<Server>, token: Token) {
    let host = self.clients[token].http_state.get_host().unwrap();
    let uri  = self.clients[token].http_state.get_uri().unwrap();
    if let Some(back) = self.backend_from_request(&host, &uri) {
      if let Ok(socket) = TcpStream::connect(&back) {
        if let Ok(backend_token) = self.backend.insert(token) {
          //println!("backend connected and stored");
          self.clients[token].backend       = Some(socket);
          self.clients[token].backend_token = Some(backend_token);
          //event_loop.register_opt(&self.clients[token].backend.unwrap(), backend_token, EventSet::readable(), PollOpt::edge()).unwrap();
          self.clients[token].status = ConnectionStatus::Connected;
          if let Some(ref sock) = self.clients[token].backend {
            event_loop.register_opt(sock, backend_token, EventSet::writable(), PollOpt::edge()).unwrap();
          }
          let rl = self.clients[token].http_state.get_request_line().unwrap();
          self.clients[token].http_state = HttpState::Proxying(rl, host);
        } else {
          self.close_client(event_loop, token);
        }
      } else {
        self.close_client(event_loop, token);
      }
    } else {
      self.close_client(event_loop, token);
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
      //println!("{:?} is readable", token);
      if token == Token(0) {
        self.accept(event_loop, token)
      } else if token.as_usize() < self.max_listeners + self.max_connections {
        if self.clients.contains(token) {
          self.clients[token].readable(event_loop);

          if let HttpState::HasHost(_,_,_) = self.clients[token].http_state {
            self.connect_to_backend(event_loop, token);
          } else if let HttpState::Error(_) = self.clients[token].http_state {
            self.close_client(event_loop, token);
          }
        } else {
          println!("client {:?} was removed", token);
        }
      } else if token.as_usize() < self.max_listeners + 2 * self.max_connections {
        if self.backend.contains(token) {
          let tok = self.backend[token];
          if self.clients.contains(tok) {
            self.clients[tok].back_readable(event_loop);
          } else {
            println!("client {:?} was removed", token);
          }
        } else {
          println!("backend {:?} was removed", token);
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
        if self.backend.contains(token) {
          let tok = self.backend[token];
          if self.clients.contains(tok) {
            self.clients[tok].back_writable(event_loop);
          } else {
            println!("client {:?} was removed", token);
          }
        } else {
          println!("backend {:?} was removed", token);
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
          if  self.clients[token].status == ConnectionStatus::ServerClosed ||
              self.clients[token].status == ConnectionStatus::ClientConnected { // the server never answered, the client closed
            self.clients[token].status = ConnectionStatus::Closed;
            self.close_client(event_loop, token);
            println!("removed");
          } else {
            self.clients[token].status = ConnectionStatus::ClientClosed;
          }
          //self.clients[token].close();
        } else {
          println!("client {:?} was already removed", token);
        }
      } else if token.as_usize() < self.max_listeners + 2 * self.max_connections {
        if self.backend.contains(token) {
          let tok = self.backend[token];
          if self.clients.contains(tok) {
            println!("server {} got hup (for client {})", token.as_usize(), tok.as_usize());
            println!("removing server {:?}", token);
            if self.clients[tok].status == ConnectionStatus::ClientClosed {
              self.clients[tok].status = ConnectionStatus::Closed;
              self.close_client(event_loop, tok);
              println!("removed");
            } else {
              self.clients[tok].status = ConnectionStatus::ClientClosed;
            }
            //self.clients[tok].close();
          } else {
            println!("client {:?} was already removed", token);
          }
        } else {

          println!("backend {:?} was already removed", token);
        }
      }
      println!("end_hup");
    }
  }

  fn notify(&mut self, event_loop: &mut EventLoop<Self>, message: Self::Message) {
  // ToDo temporary
    println!("notified: {:?}", message);
    match message {
      HttpProxyOrder::Command(Command::AddHttpFront(front)) => {
        println!("add front {:?}", front);
          self.add_http_front(front, event_loop);
          self.tx.send(ServerMessage::AddedHttpFront);
      },
      HttpProxyOrder::Command(Command::RemoveHttpFront(front)) => {
        println!("remove front {:?}", front);
        self.remove_http_front(front, event_loop);
        self.tx.send(ServerMessage::RemovedHttpFront);
      },
      HttpProxyOrder::Command(Command::AddInstance(instance)) => {
        println!("add instance {:?}", instance);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let addr = &addr_string.parse().unwrap();
        self.add_instance(&instance.app_id, addr, event_loop);
        self.tx.send(ServerMessage::AddedInstance);
      },
      HttpProxyOrder::Command(Command::RemoveInstance(instance)) => {
        println!("remove instance {:?}", instance);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let addr = &addr_string.parse().unwrap();
        self.remove_instance(&instance.app_id, addr, event_loop);
        self.tx.send(ServerMessage::RemovedInstance);
      },
      HttpProxyOrder::Stop                   => {
        event_loop.shutdown();
      },
      _ => {
        println!("unsupported message, ignoring");
      }
    }
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
  let front: SocketAddr = FromStr::from_str("127.0.0.1:8080").unwrap();

  let tcp_listener = TcpListener::bind(&front).unwrap();
  let listener = ApplicationListener {
    sock:           tcp_listener,
    token:          Token(0),
    front_address:  front
  };

  event_loop.register_opt(&listener.sock, listener.token, EventSet::readable(), PollOpt::edge()).unwrap();

  let mut server = Server::new(listener, 500, tx);

  let join_guard = thread::spawn(move|| {
    println!("starting event loop");
    event_loop.run(&mut server).unwrap();
    println!("ending event loop");
    notify_tx.send(ServerMessage::Stopped);
  });


  //println!("listen for connections");
  //event_loop.register_opt(&listener, SERVER, EventSet::readable(), PollOpt::edge() | PollOpt::oneshot()).unwrap();
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

  let tcp_listener = TcpListener::bind(&front).unwrap();
  let listener = ApplicationListener {
    sock:           tcp_listener,
    token:          Token(0),
    front_address:  front
  };

  event_loop.register_opt(&listener.sock, listener.token, EventSet::readable(), PollOpt::edge()).unwrap();

  let mut server = Server::new(listener, max_connections, tx);

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
  extern crate hyper;
  use super::*;
  use std::net::{TcpListener, TcpStream, Shutdown};
  use std::io::{Read,Write};
  use std::{thread,str};
  use std::sync::mpsc::channel;
  use std::net::SocketAddr;
  use std::str::FromStr;
  use self::hyper::Client;
  use self::hyper::header::Connection;
  use messages::{Command,HttpFront,Instance};

  #[allow(unused_mut, unused_must_use, unused_variables)]
  #[test]
  fn mi() {
    thread::spawn(|| { start_server(); });
    let front: SocketAddr = FromStr::from_str("127.0.0.1:8080").unwrap();
    let (tx,rx) = channel::<ServerMessage>();
    let (sender, jg) = start_listener(front, 10, 10, tx.clone());
    let front = HttpFront { app_id: String::from("app_1"), hostname: String::from("localhost:8080"), path_begin: String::from("/") };
    sender.send(HttpProxyOrder::Command(Command::AddHttpFront(front)));
    let instance = Instance { app_id: String::from("app_1"), ip_address: String::from("127.0.0.1"), port: 5678 };
    sender.send(HttpProxyOrder::Command(Command::AddInstance(instance)));
    println!("test received: {:?}", rx.recv());
    println!("test received: {:?}", rx.recv());
    thread::sleep_ms(300);

    let mut client = Client::new();
    // Creating an outgoing request.
    println!("client request");
    let mut r = client.get("http://localhost:8080/")
      // set a header
      .header(Connection::close())
      // let 'er go!
      .send();
     println!("client request sent: {:?}", r);
     let mut res = r.unwrap();

    // Read the Response.
    println!("read response");
    let mut body = String::new();
    let r = res.read_to_string(&mut body);
    println!("res: {:?}", r);

    println!("Response: {}", body);

    thread::sleep_ms(300);
    assert_eq!(&body, &"Hello World!"[..]);
    //assert!(false);
  }

  use self::hyper::server::Request;
  use self::hyper::server::Response;
  use self::hyper::net::Fresh;

  fn hello(_: Request, res: Response<Fresh>) {
    println!("backend web server got request");
    res.send(b"Hello World!").unwrap();
    println!("backend web server sent response");
  }

  #[allow(unused_mut, unused_must_use, unused_variables)]
  fn start_server() {
    thread::spawn(move|| {
      hyper::Server::http("127.0.0.1:5678").unwrap().handle(hello);
    });
  }

}
