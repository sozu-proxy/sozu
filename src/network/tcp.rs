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
use time::{Duration,precise_time_s};
use rand::random;
use uuid::Uuid;
use network::{Backend,ClientResult,ServerMessage,ServerMessageType,ConnectionError,ProxyOrder,RequiredEvents};
use network::proxy::{BackendConnectAction,Server,ProxyClient,ProxyConfiguration,Readiness};
use network::buffer::Buffer;
use network::buffer_queue::BufferQueue;
use network::socket::{SocketHandler,SocketResult,server_bind};
use pool::{Pool,Checkout,Reset};

use messages::{TcpFront,Command,Instance};

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
  front_buf:      Checkout<BufferQueue>,
  back_buf:       Checkout<BufferQueue>,
  token:          Option<Token>,
  backend_token:  Option<Token>,
  accept_token:   Token,
  back_interest:  EventSet,
  front_interest: EventSet,
  front_timeout:  Option<Timeout>,
  back_timeout:   Option<Timeout>,
  status:         ConnectionStatus,
  rx_count:       usize,
  tx_count:       usize,
  app_id:         Option<String>,
  request_id:     String,
  readiness:      Readiness,
}

impl Client {
  fn new(sock: TcpStream, accept_token: Token, front_buf: Checkout<BufferQueue>,
    back_buf: Checkout<BufferQueue>) -> Option<Client> {
    Some(Client {
      sock:           sock,
      backend:        None,
      front_buf:      front_buf,
      back_buf:       back_buf,
      token:          None,
      backend_token:  None,
      accept_token:   accept_token,
      back_interest:  EventSet::all(),
      front_interest: EventSet::all(),
      front_timeout:  None,
      back_timeout:   None,
      status:         ConnectionStatus::Connected,
      rx_count:       0,
      tx_count:       0,
      app_id:         None,
      request_id:     Uuid::new_v4().hyphenated().to_string(),
      readiness:      Readiness::new(),
    })
  }
}

impl ProxyClient for Client {
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

  fn close(&mut self) {
    println!("TCP closing[{:?}] front: {:?}, back: {:?}", self.token, *self.front_buf, *self.back_buf);
  }

  fn log_context(&self) -> String {
    if let Some(ref app_id) = self.app_id {
      format!("TCP\t{}\t{}\t", self.request_id, app_id)
    } else {
      format!("TCP\t{}\tunknown\t", self.request_id)
    }
  }

  fn set_back_socket(&mut self, socket: TcpStream) {
    self.backend       = Some(socket);
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

  //FIXME: too much cloning in there, should optimize
  fn remove_backend(&mut self) -> (Option<String>, Option<SocketAddr>) {
    debug!("{}\tTCP\tPROXY [{} -> {}] CLOSED BACKEND", self.request_id, self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize());
    let addr = self.backend.as_ref().and_then(|sock| sock.peer_addr().ok());
    self.backend       = None;
    self.backend_token = None;
    (self.app_id.clone(), addr)
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

  fn readable(&mut self) -> ClientResult {
    if self.front_buf.buffer.available_space() == 0 {
      self.readiness.front_interest.remove(EventSet::readable());
      return ClientResult::Continue;
    }

    let (sz, res) = self.sock.socket_read(self.front_buf.buffer.space());
    if sz > 0 {
      self.front_buf.buffer.fill(sz);
      self.front_buf.sliced_input(sz);
      self.front_buf.consume_parsed_data(sz);
      self.front_buf.slice_output(sz);
    } else {
      self.readiness.front_readiness.remove(EventSet::readable());
    }
    trace!("{}\tTCP\tFRONT [{}->{}]: read {} bytes", self.request_id, self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), sz);

    match res {
      SocketResult::Error => {
        self.readiness.reset();
        return ClientResult::CloseClient;
      },
      _                   => {
        if res == SocketResult::WouldBlock {
          self.readiness.front_readiness.remove(EventSet::readable());
        }
        self.readiness.back_interest.insert(EventSet::writable());
        return ClientResult::Continue;
      }
    }
  }

  fn writable(&mut self) -> ClientResult {
    if self.back_buf.buffer.available_data() == 0 {
      self.readiness.front_interest.remove(EventSet::writable());
      return ClientResult::Continue;
    }

     let mut sz = 0usize;
     let mut socket_res = SocketResult::Continue;

     while socket_res == SocketResult::Continue && self.back_buf.output_data_size() > 0 {
       let (current_sz, current_res) = self.sock.socket_write(self.back_buf.next_output_data());
       socket_res = current_res;
       self.back_buf.consume_output_data(current_sz);
       sz += current_sz;
     }
     trace!("{}\tTCP\tFRONT [{}<-{}]: wrote {} bytes", self.request_id, self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), sz);

     match socket_res {
       SocketResult::Error => {
         self.readiness.reset();
         ClientResult::CloseBothFailure
       },
       SocketResult::WouldBlock => {
         self.readiness.front_readiness.remove(EventSet::writable());
         self.readiness.back_interest.insert(EventSet::readable());
         ClientResult::Continue
       },
       SocketResult::Continue => {
         self.readiness.back_interest.insert(EventSet::readable());
         ClientResult::Continue
       }
     }
  }

  fn back_readable(&mut self) -> ClientResult {
    if self.back_buf.buffer.available_space() == 0 {
      self.readiness.back_interest.remove(EventSet::readable());
      return ClientResult::Continue;
    }

    if let Some(ref mut sock) = self.backend {
      let (sz, res) = sock.socket_read(self.back_buf.buffer.space());
      self.back_buf.buffer.fill(sz);
      self.back_buf.sliced_input(sz);
      self.back_buf.consume_parsed_data(sz);
      self.back_buf.slice_output(sz);
      trace!("{}\tTCP\tBACK  [{}<-{}]: read {} bytes", self.request_id, self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), sz);

      match res {
        SocketResult::Error => {
          self.readiness.reset();
          return ClientResult::CloseClient;
        },
        _                   => {
          if res == SocketResult::WouldBlock {
            self.readiness.back_readiness.remove(EventSet::readable());
          }
          self.readiness.front_interest.insert(EventSet::writable());
          return ClientResult::Continue;
        }
      }
    } else {
      self.readiness.reset();
      ClientResult::CloseBothFailure
    }
  }

  fn back_writable(&mut self) -> ClientResult {
     if self.front_buf.buffer.available_data() == 0 {
        self.readiness.back_interest.remove(EventSet::writable());
        self.readiness.front_interest.insert(EventSet::readable());
        return ClientResult::Continue;
     }

     let mut sz = 0usize;
     let mut socket_res = SocketResult::Continue;

     if let Some(ref mut sock) = self.backend {
       while socket_res == SocketResult::Continue && self.front_buf.output_data_size() > 0 {
         let (current_sz, current_res) = sock.socket_write(self.front_buf.next_output_data());
         socket_res = current_res;
         self.front_buf.consume_output_data(current_sz);
         sz += current_sz;
       }
     }
    trace!("{}\tTCP\tBACK [{}->{}]: wrote {} bytes", self.request_id, self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), sz);

     match socket_res {
       SocketResult::Error => {
         self.readiness.reset();
         ClientResult::CloseBothFailure
       },
       SocketResult::WouldBlock => {
         self.readiness.back_readiness.remove(EventSet::writable());
         self.readiness.front_interest.insert(EventSet::readable());
         ClientResult::Continue
       },
       SocketResult::Continue => {
         self.readiness.front_interest.insert(EventSet::readable());
         ClientResult::Continue
       }
     }
  }

  fn readiness(&mut self) -> &mut Readiness {
    &mut self.readiness
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
  fronts:          HashMap<String, Token>,
  instances:       HashMap<String, Vec<Backend>>,
  listeners:       Slab<ApplicationListener>,
  tx:              mpsc::Sender<ServerMessage>,
  pool:            Pool<BufferQueue>,
  front_timeout:   u64,
  back_timeout:    u64,
}

impl ServerConfiguration {
  pub fn new(max_listeners: usize,  tx: mpsc::Sender<ServerMessage>) -> ServerConfiguration {
    ServerConfiguration {
      instances:     HashMap::new(),
      listeners:     Slab::new_starting_at(Token(0), max_listeners),
      fronts:        HashMap::new(),
      tx:            tx,
      pool:          Pool::with_capacity(2*max_listeners, 0, || BufferQueue::with_capacity(2048)),
      front_timeout: 5000,
      back_timeout:  5000,
    }
  }

  fn add_tcp_front(&mut self, app_id: &str, front: &SocketAddr, event_loop: &mut EventLoop<TcpServer>) -> Option<Token> {
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
        self.listeners[tok].token = Some(tok);
        self.fronts.insert(String::from(app_id), tok);
        event_loop.register(&self.listeners[tok].sock, tok, EventSet::readable(), PollOpt::level());
        info!("TCP\tregistered listener for app {} on port {}", app_id, front.port());
        Some(tok)
      } else {
        error!("TCP\tcould not register listener for app {} on port {}", app_id, front.port());
        None
      }

    } else {
      error!("TCP\tcould not declare listener for app {} on port {}", app_id, front.port());
      None
    }
  }

  pub fn remove_tcp_front(&mut self, app_id: String, event_loop: &mut EventLoop<TcpServer>) -> Option<Token>{
    info!("TCP\tremoving tcp_front {:?}", app_id);
    // ToDo
    // Removes all listeners for the given app_id
    // an app can't have two listeners. Is this a problem?
    if let Some(&tok) = self.fronts.get(&app_id) {
      if self.listeners.contains(tok) {
        event_loop.deregister(&self.listeners[tok].sock);
        self.listeners.remove(tok);
        warn!("TCP\tremoved server {:?}", tok);
        //self.listeners[tok].sock.shutdown(Shutdown::Both);
        Some(tok)
      } else {
        None
      }
    } else {
      None
    }
  }

  pub fn add_instance(&mut self, app_id: &str, instance_address: &SocketAddr, event_loop: &mut EventLoop<TcpServer>) -> Option<Token> {
    if let Some(addrs) = self.instances.get_mut(app_id) {
      let backend = Backend::new(*instance_address);
      addrs.push(backend);
    }

    if self.instances.get(app_id).is_none() {
      let backend = Backend::new(*instance_address);
      self.instances.insert(String::from(app_id), vec![backend]);
    }

    if let Some(&tok) = self.fronts.get(app_id) {
      let application_listener = &mut self.listeners[tok];

      application_listener.back_addresses.push(*instance_address);
      Some(tok)
    } else {
      error!("TCP\tNo front for this instance");
      None
    }
  }

  pub fn remove_instance(&mut self, app_id: &str, instance_address: &SocketAddr, event_loop: &mut EventLoop<TcpServer>) -> Option<Token>{
      // ToDo
      None
  }

}

impl ProxyConfiguration<TcpServer, Client> for ServerConfiguration {

  fn connect_to_backend(&mut self, event_loop: &mut EventLoop<TcpServer>, client:&mut Client) ->Result<BackendConnectAction,ConnectionError> {
    let rnd = random::<usize>();
    let idx = rnd % self.listeners[client.accept_token].back_addresses.len();

    client.app_id = Some(self.listeners[client.accept_token].app_id.clone());
    let backend_addr = try!(self.listeners[client.accept_token].back_addresses.get(idx).ok_or(ConnectionError::ToBeDefined));
    let stream = try!(TcpStream::connect(backend_addr).map_err(|_| ConnectionError::ToBeDefined));
    stream.set_nodelay(true);

    client.set_back_socket(stream);
    client.readiness().front_interest.insert(EventSet::readable() | EventSet::writable());
    client.readiness().back_interest.insert(EventSet::readable() | EventSet::writable());
    Ok(BackendConnectAction::New)
  }

  fn notify(&mut self, event_loop: &mut EventLoop<TcpServer>, message: ProxyOrder) {
    match message {
      ProxyOrder::Command(id, Command::AddTcpFront(tcp_front)) => {
        trace!("TCP\t{:?}", tcp_front);
        let addr_string = tcp_front.ip_address + &tcp_front.port.to_string();
        if let Ok(front) = addr_string.parse() {
          if let Some(token) = self.add_tcp_front(&tcp_front.app_id, &front, event_loop) {
            self.tx.send(ServerMessage{ id: id, message: ServerMessageType::AddedFront});
          } else {
            error!("TCP\tCouldn't add tcp front");
            self.tx.send(ServerMessage{ id: id, message: ServerMessageType::Error(String::from("cannot add tcp front"))});
          }
        } else {
          error!("TCP\tCouldn't parse tcp front address");
          self.tx.send(ServerMessage{ id: id, message: ServerMessageType::Error(String::from("cannot parse the address"))});
        }
      },
      ProxyOrder::Command(id, Command::RemoveTcpFront(front)) => {
        trace!("TCP\t{:?}", front);
        let _ = self.remove_tcp_front(front.app_id, event_loop);
        self.tx.send(ServerMessage{ id: id, message: ServerMessageType::RemovedFront});
      },
      ProxyOrder::Command(id, Command::AddInstance(instance)) => {
        trace!("TCP\t{:?}", instance);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let addr = &addr_string.parse().unwrap();
        if let Some(token) = self.add_instance(&instance.app_id, addr, event_loop) {
          self.tx.send(ServerMessage{ id: id, message: ServerMessageType::AddedInstance});
        } else {
          error!("TCP\tCouldn't add tcp instance");
          self.tx.send(ServerMessage{ id: id, message: ServerMessageType::Error(String::from("cannot add tcp instance"))});
        }
      },
      ProxyOrder::Command(id, Command::RemoveInstance(instance)) => {
        trace!("TCP\t{:?}", instance);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let addr = &addr_string.parse().unwrap();
        if let Some(token) = self.remove_instance(&instance.app_id, addr, event_loop) {
          self.tx.send(ServerMessage{ id: id, message: ServerMessageType::RemovedInstance});
        } else {
          error!("TCP\tCouldn't remove tcp instance");
          self.tx.send(ServerMessage{ id: id, message: ServerMessageType::Error(String::from("cannot remove tcp instance"))});
        }
      },
      ProxyOrder::Stop(id)                   => {
        event_loop.shutdown();
          self.tx.send(ServerMessage{ id: id, message: ServerMessageType::Stopped});
      },
      ProxyOrder::Command(id, _) => {
        error!("TCP\tunsupported message, ignoring");
        self.tx.send(ServerMessage{ id: id, message: ServerMessageType::Error(String::from("unsupported message"))});
      }
    }
  }

  fn accept(&mut self, token: Token) -> Option<(Client, bool)> {
    if let (Some(front_buf), Some(back_buf)) = (self.pool.checkout(), self.pool.checkout()) {
      if self.listeners.contains(token) {
        let accepted = self.listeners[token].sock.accept();

        if let Ok(Some((frontend_sock, _))) = accepted {
          frontend_sock.set_nodelay(true);
          if let Some(c) = Client::new(frontend_sock, token, front_buf, back_buf) {
            return Some((c, true));
          }
        }
      }
    } else {
      error!("TCP\tcould not get buffers from pool");
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

pub type TcpServer = Server<ServerConfiguration,Client>;

pub fn start() {
  let mut event_loop = EventLoop::new().unwrap();


  info!("TCP\tlisten for connections");
  //event_loop.register(&listener, SERVER, EventSet::readable(), PollOpt::edge() | PollOpt::oneshot()).unwrap();
  let (tx,rx) = channel::<ServerMessage>();
  let configuration = ServerConfiguration::new(10, tx);
  let mut s = TcpServer::new(10, 500, configuration);
  {
    let front: SocketAddr = FromStr::from_str("127.0.0.1:1234").unwrap();
    let back: SocketAddr  = FromStr::from_str("127.0.0.1:5678").unwrap();
    s.configuration().add_tcp_front("yolo", &front, &mut event_loop);
    s.configuration().add_instance("yolo", &back, &mut event_loop);
  }
  {
    let front: SocketAddr = FromStr::from_str("127.0.0.1:1235").unwrap();
    let back: SocketAddr  = FromStr::from_str("127.0.0.1:5678").unwrap();
    s.configuration().add_tcp_front("yolo", &front, &mut event_loop);
    s.configuration().add_instance("yolo", &back, &mut event_loop);
  }
  thread::spawn(move|| {
    info!("TCP\tstarting event loop");
    event_loop.run(&mut s).unwrap();
    info!("TCP\tending event loop");
  });
}

pub fn start_listener(max_listeners: usize, max_connections: usize, tx: mpsc::Sender<ServerMessage>) -> (Sender<ProxyOrder>,thread::JoinHandle<()>)  {
  let mut event_loop = EventLoop::new().unwrap();
  let channel = event_loop.channel();
  let notify_tx = tx.clone();
  let configuration = ServerConfiguration::new(max_listeners, tx);
  let mut server = TcpServer::new(max_listeners, max_connections, configuration);
  let front: SocketAddr = FromStr::from_str("127.0.0.1:8443").unwrap();
  server.configuration().add_tcp_front("yolo", &front, &mut event_loop);

  let join_guard = thread::spawn(move|| {
    info!("TCP\tstarting event loop");
    event_loop.run(&mut server).unwrap();
    info!("TCP\tending event loop");
    //notify_tx.send(ServerMessage::Stopped);
  });

  (channel, join_guard)
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
    thread::spawn(|| { start_server(); });
    start();
    thread::sleep(Duration::from_millis(300));

    let mut s1 = TcpStream::connect("127.0.0.1:1234").unwrap();
    let mut s3 = TcpStream::connect("127.0.0.1:1234").unwrap();
    thread::sleep(Duration::from_millis(300));
    let mut s2 = TcpStream::connect("127.0.0.1:1234").unwrap();
    s1.write(&b"hello"[..]);
    println!("s1 sent");
    s2.write(&b"pouet pouet"[..]);
    println!("s2 sent");
    thread::sleep(Duration::from_millis(500));

    let mut res = [0; 128];
    s1.write(&b"coucou"[..]);
    let mut sz1 = s1.read(&mut res[..]).unwrap();
    println!("s1 received {:?}", str::from_utf8(&res[..sz1]));
    assert_eq!(&res[..sz1], &b"hello END"[..]);
    s3.shutdown(Shutdown::Both);
    let sz2 = s2.read(&mut res[..]).unwrap();
    println!("s2 received {:?}", str::from_utf8(&res[..sz2]));
    assert_eq!(&res[..sz2], &b"pouet pouet END"[..]);


    thread::sleep(Duration::from_millis(400));
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
