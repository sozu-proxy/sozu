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
use network::{Backend,ClientResult,ServerMessage,ServerMessageType,ConnectionError,ProxyOrder,RequiredEvents};
use network::proxy::{Server,ProxyClient,ProxyConfiguration};

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

#[cfg(not(feature = "splice"))]
pub struct Client {
  sock:           TcpStream,
  backend:        Option<TcpStream>,
  front_buf:      Option<MutByteBuf>,
  back_buf:       Option<MutByteBuf>,
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
}

#[cfg(feature = "splice")]
pub struct Client {
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
  front_timeout:  Option<Timeout>,
  back_timeout:   Option<Timeout>,
  status:         ConnectionStatus,
  rx_count:       usize,
  tx_count:       usize,
  app_id:         Option<String>,
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
      front_timeout:  None,
      back_timeout:   None,
      status:         ConnectionStatus::Connected,
      rx_count:       0,
      tx_count:       0,
      app_id:         None,
    })
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
        front_timeout:  None,
        back_timeout:   None,
        status:         ConnectionStatus::Initial,
        tx_count:       0,
        rx_count:       0,
        app_id:         None,
      })
    } else {
      None
    }
  }

  pub fn set_tokens(&mut self, token: Token, backend: Token) {
    self.token         = Some(token);
    self.backend_token = Some(backend);
  }

  fn writable(&mut self) -> io::Result<()> {
    trace!("in writable()");
    if self.data_out {
      match splice::splice_out(self.pipe_out, &self.sock) {
        None => {
          trace!("client flushing buf; WOULDBLOCK");

          self.front_interest.insert(EventSet::writable());
        }
        Some(r) => {
          //FIXME what happens if not everything was written?
          debug!("FRONT [{}<-{}]: wrote {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);

          //self.front_interest.insert(EventSet::readable());
          self.front_interest.remove(EventSet::writable());
          self.back_interest.insert(EventSet::readable());
          self.data_out = false;
          self.tx_count = self.tx_count + r;
        }
      }
    }
    Ok(())
  }

  fn readable(&mut self) -> io::Result<()> {
    //trace!("in readable(): front_mut_buf contains {} bytes", buf.remaining());

    match splice::splice_in(&self.sock, self.pipe_in) {
      None => {
        error!("We just got readable, but were unable to read from the socket?");
      }
      Some(r) => {
        debug!("FRONT [{}->{}]: read {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);
        self.front_interest.remove(EventSet::readable());
        self.back_interest.insert(EventSet::writable());
        self.data_in = true;
        self.rx_count = self.rx_count + r;
      }
    };

    Ok(())
  }

  fn back_writable(&mut self) -> io::Result<()> {
    //trace!("in back_writable 2: front_buf contains {} bytes", buf.remaining());

    if self.data_in {
      match splice::splice_out(self.pipe_in, &self.backend) {
        None => {
          error!("client flushing buf; WOULDBLOCK");

          self.back_interest.insert(EventSet::writable());
        }
        Some(r) => {
          //FIXME what happens if not everything was written?
          debug!("BACK  [{}->{}]: wrote {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);

          self.front_interest.insert(EventSet::readable());
          self.back_interest.remove(EventSet::writable());
          self.back_interest.insert(EventSet::readable());
          self.data_in = false;
        }
      }
    }
    Ok(())
  }

  fn back_readable(&mut self) -> io::Result<()> {
    trace!("in back_readable(): back_mut_buf contains {} bytes", buf.remaining());

    match splice::splice_in(&self.backend, self.pipe_out) {
      None => {
        error!("We just got readable, but were unable to read from the socket?");
      }
      Some(r) => {
        debug!("BACK  [{}<-{}]: read {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);
        self.back_interest.remove(EventSet::readable());
        self.front_interest.insert(EventSet::writable());
        self.data_out = true;
      }
    };

    Ok(())
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
    debug!("TCP PROXY [{} -> {}] CLOSED BACKEND", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize());
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

  fn writable(&mut self) -> (RequiredEvents, ClientResult) {
    trace!("in writable()");
    if let Some(buf) = self.back_buf.take() {
      //trace!("in writable 2: back_buf contains {} bytes", buf.remaining());

      let mut b = buf.flip();
      match self.sock.try_write_buf(&mut b) {
        Ok(None) => {
          error!("client flushing buf; WOULDBLOCK");

          self.front_interest.insert(EventSet::writable());
        }
        Ok(Some(r)) => {
          //FIXME what happens if not everything was written?
          debug!("FRONT [{}<-{}]: wrote {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);

          self.tx_count = self.tx_count + r;

          //self.front_interest.insert(EventSet::readable());
          self.front_interest.remove(EventSet::writable());
          self.back_interest.insert(EventSet::readable());
        }
        Err(e) =>  error!("not implemented; client err={:?}", e),
      }
      self.back_buf = Some(b.flip());
    }
    (RequiredEvents::FrontReadWriteBackReadWrite, ClientResult::Continue)
  }

  fn readable(&mut self) -> (RequiredEvents, ClientResult) {
    let mut buf = self.front_buf.take().unwrap();
    //trace!("in readable(): front_mut_buf contains {} bytes", buf.remaining());

    match self.sock.try_read_buf(&mut buf) {
      Ok(None) => {
        error!("We just got readable, but were unable to read from the socket?");
      }
      Ok(Some(r)) => {
        debug!("FRONT [{}->{}]: read {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);
        self.front_interest.remove(EventSet::readable());
        self.back_interest.insert(EventSet::writable());
        self.rx_count = self.rx_count + r;
        // prepare to provide this to writable
      }
      Err(e) => {
        error!("not implemented; client err={:?}", e);
        //self.front_interest.remove(EventSet::readable());
      }
    };
    self.front_buf = Some(buf);

    (RequiredEvents::FrontReadWriteBackReadWrite, ClientResult::Continue)
  }

  fn back_writable(&mut self) -> (RequiredEvents, ClientResult) {
    if let Some(buf) = self.front_buf.take() {
      //trace!("in back_writable 2: front_buf contains {} bytes", buf.remaining());

      let mut b = buf.flip();
      if let Some(ref mut sock) = self.backend {
        match sock.try_write_buf(&mut b) {
          Ok(None) => {
            error!("client flushing buf; WOULDBLOCK");

            self.back_interest.insert(EventSet::writable());
          }
          Ok(Some(r)) => {
            //FIXME what happens if not everything was written?
            debug!("BACK  [{}->{}]: wrote {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);

            self.front_interest.insert(EventSet::readable());
            self.back_interest.remove(EventSet::writable());
            self.back_interest.insert(EventSet::readable());
          }
          Err(e) =>  error!("not implemented; client err={:?}", e),
        }
      }
      self.front_buf = Some(b.flip());
    }
    (RequiredEvents::FrontReadWriteBackReadWrite, ClientResult::Continue)
  }

  fn back_readable(&mut self) -> (RequiredEvents, ClientResult) {
    let mut buf = self.back_buf.take().unwrap();
    //trace!("in back_readable(): back_mut_buf contains {} bytes", buf.remaining());

    if let Some(ref mut sock) = self.backend {
      match sock.try_read_buf(&mut buf) {
        Ok(None) => {
          error!("We just got readable, but were unable to read from the socket?");
        }
        Ok(Some(r)) => {
          debug!("BACK  [{}<-{}]: read {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);
          self.back_interest.remove(EventSet::readable());
          self.front_interest.insert(EventSet::writable());
          // prepare to provide this to writable
        }
        Err(e) => {
          error!("not implemented; client err={:?}", e);
          //self.interest.remove(EventSet::readable());
        }
      };
    }
    self.back_buf = Some(buf);

    (RequiredEvents::FrontReadWriteBackReadWrite, ClientResult::Continue)
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
  instances: HashMap<String, Vec<Backend>>,
  listeners: Slab<ApplicationListener>,
  tx:        mpsc::Sender<ServerMessage>,
  front_timeout:   u64,
  back_timeout:    u64,
}

impl ServerConfiguration {
  pub fn new(max_listeners: usize,  tx: mpsc::Sender<ServerMessage>) -> ServerConfiguration {
    ServerConfiguration {
      instances: HashMap::new(),
      listeners: Slab::new_starting_at(Token(0), max_listeners),
      fronts:    HashMap::new(),
      tx:        tx,
      front_timeout: 50000,
      back_timeout:  50000,
    }
  }

  fn add_tcp_front(&mut self, app_id: &str, front: &SocketAddr, event_loop: &mut EventLoop<TcpServer>) -> Option<Token> {
    if let Ok(listener) = TcpListener::bind(front) {
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
        info!("registered listener for app {} on port {}", app_id, front.port());
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

  pub fn remove_tcp_front(&mut self, app_id: String, event_loop: &mut EventLoop<TcpServer>) -> Option<Token>{
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
      error!("No front for this instance");
      None
    }
  }

  pub fn remove_instance(&mut self, app_id: &str, instance_address: &SocketAddr, event_loop: &mut EventLoop<TcpServer>) -> Option<Token>{
      // ToDo
      None
  }

}

impl ProxyConfiguration<TcpServer, Client> for ServerConfiguration {

  fn connect_to_backend(&mut self, client:&mut Client) ->Result<(),ConnectionError> {
    let rnd = random::<usize>();
    let idx = rnd % self.listeners[client.accept_token].back_addresses.len();

    client.app_id = Some(self.listeners[client.accept_token].app_id.clone());
    let backend_addr = try!(self.listeners[client.accept_token].back_addresses.get(idx).ok_or(ConnectionError::ToBeDefined));
    let stream = try!(TcpStream::connect(backend_addr).map_err(|_| ConnectionError::ToBeDefined));
    stream.set_nodelay(true);

    client.set_back_socket(stream);
    Ok(())
  }

  fn notify(&mut self, event_loop: &mut EventLoop<TcpServer>, message: ProxyOrder) {
    match message {
      ProxyOrder::Command(id, Command::AddTcpFront(tcp_front)) => {
        trace!("{:?}", tcp_front);
        let addr_string = tcp_front.ip_address + &tcp_front.port.to_string();
        if let Ok(front) = addr_string.parse() {
          if let Some(token) = self.add_tcp_front(&tcp_front.app_id, &front, event_loop) {
            self.tx.send(ServerMessage{ id: id, message: ServerMessageType::AddedFront});
          } else {
            error!("Couldn't add tcp front");
          }
        } else {
          error!("Couldn't parse tcp front address");
        }
      },
      ProxyOrder::Command(id, Command::RemoveTcpFront(front)) => {
        trace!("{:?}", front);
        let _ = self.remove_tcp_front(front.app_id, event_loop);
        self.tx.send(ServerMessage{ id: id, message: ServerMessageType::RemovedFront});
      },
      ProxyOrder::Command(id, Command::AddInstance(instance)) => {
        trace!("{:?}", instance);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let addr = &addr_string.parse().unwrap();
        if let Some(token) = self.add_instance(&instance.app_id, addr, event_loop) {
          self.tx.send(ServerMessage{ id: id, message: ServerMessageType::AddedInstance});
        } else {
          error!("Couldn't add tcp front");
        }
      },
      ProxyOrder::Command(id, Command::RemoveInstance(instance)) => {
        trace!("{:?}", instance);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let addr = &addr_string.parse().unwrap();
        if let Some(token) = self.remove_instance(&instance.app_id, addr, event_loop) {
          self.tx.send(ServerMessage{ id: id, message: ServerMessageType::RemovedInstance});
        } else {
          error!("Couldn't add tcp front");
        }
      },
      ProxyOrder::Stop(id)                   => {
        event_loop.shutdown();
          self.tx.send(ServerMessage{ id: id, message: ServerMessageType::Stopped});
      },
      _ => {
        error!("unsupported message, ignoring");
      }
    }
  }

  fn accept(&mut self, token: Token) -> Option<(Client, bool)> {
    if self.listeners.contains(token) {
      let accepted = self.listeners[token].sock.accept();

      if let Ok(Some((frontend_sock, _))) = accepted {
        frontend_sock.set_nodelay(true);
        if let Some(c) = Client::new(frontend_sock, token) {
          return Some((c, true));
        }
      }
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


  info!("listen for connections");
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
    info!("starting event loop");
    event_loop.run(&mut s).unwrap();
    info!("ending event loop");
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
    info!("starting event loop");
    event_loop.run(&mut server).unwrap();
    info!("ending event loop");
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
          //println!("[{}] {:?}", id, str::from_utf8(&buf[..sz]));
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
