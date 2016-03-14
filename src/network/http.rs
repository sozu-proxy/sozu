#![allow(unused_imports)]

use std::thread::{self,Thread,Builder};
use std::sync::mpsc::{self,channel,Receiver};
use std::cmp::min;
use mio::tcp::*;
use std::io::{self,Read,Write,ErrorKind};
use mio::*;
use bytes::{ByteBuf,MutByteBuf};
use bytes::buf::MutBuf;
use pool::{Pool,Checkout};
use std::collections::HashMap;
use std::error::Error;
use mio::util::Slab;
use std::net::SocketAddr;
use std::str::{FromStr, from_utf8};
use time::{Duration, precise_time_s, precise_time_ns};
use rand::random;
use network::{Backend,ClientResult,ServerMessage,ConnectionError,ProxyOrder,RequiredEvents};
use network::proxy::{Server,ProxyConfiguration,ProxyClient};
use network::metrics::{ProxyMetrics,METRICS};
use network::buffer::Buffer;
use network::socket::{SocketHandler,SocketResult};

use parser::http11::{HttpState,parse_request_until_stop, parse_response_until_stop, BufferMove, RequestState, ResponseState, Chunk};
use nom::HexDisplay;

use messages::{Command,HttpFront};

type BackendToken = Token;

#[derive(PartialEq)]
pub enum ClientStatus {
  Normal,
  DefaultAnswer,
}

pub struct Client<Front:SocketHandler> {
  frontend:       Front,
  backend:        Option<TcpStream>,
  token:          Option<Token>,
  backend_token:  Option<Token>,
  front_timeout:  Option<Timeout>,
  back_timeout:   Option<Timeout>,
  rx_count:       usize,
  tx_count:       usize,
  status:         ClientStatus,

  state:              HttpState,
  front_buf:          Checkout<Buffer>,
  back_buf:           Checkout<Buffer>,
  front_buf_position: usize,
  back_buf_position:  usize,
  start:              u64,
  req_size:           usize,
  res_size:           usize,
}

impl<Front:SocketHandler> Client<Front> {
  pub fn new(sock: Front, front_buf: Checkout<Buffer>, back_buf: Checkout<Buffer>) -> Option<Client<Front>> {
    Some(Client {
      frontend:       sock,
      backend:        None,
      token:          None,
      backend_token:  None,
      front_timeout:  None,
      back_timeout:   None,
      rx_count:       0,
      tx_count:       0,
      status:         ClientStatus::Normal,

      state:              HttpState::new(),
      front_buf:          front_buf,
      back_buf:           back_buf,
      front_buf_position: 0,
      back_buf_position:  0,
      start:              precise_time_ns(),
      req_size:           0,
      res_size:           0,
    })
  }

  pub fn close(&self) {
  }

  pub fn reset(&mut self) {
    //println!("RESET");
    self.state.reset();
    self.front_buf_position = 0;
    self.back_buf_position = 0;
    self.front_buf.reset();
    self.back_buf.reset();
  }

  fn tokens(&self) -> Option<(Token,Token)> {
    if let Some(front) = self.token {
      if let Some(back) = self.backend_token {
        return Some((front, back))
      }
    }
    None
  }

  pub fn state(&mut self) -> &mut HttpState {
    &mut self.state
  }

  pub fn set_state(&mut self, state: HttpState) {
    self.state = state;
  }

  pub fn set_answer(&mut self, buf: &[u8])  {
    self.back_buf.reset();
    self.back_buf.write(buf);
    self.status = ClientStatus::DefaultAnswer;
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
  fn readable(&mut self) -> (RequiredEvents,ClientResult) {
    if self.status == ClientStatus::DefaultAnswer {
      return (RequiredEvents::FrontWriteBackNone, ClientResult::Continue)
    }

    trace!("readable REQ pos: {}, buf pos: {}, available: {}", self.state.req_position, self.front_buf_position, self.front_buf.available_data());
    assert!(!self.state.is_front_error());

    if self.front_buf.available_space() == 0 {
      if self.backend_token == None {
        // We don't have a backend to empty the buffer into, close the connection
        error!("[{:?}] front buffer full, no backend, closing the connection", self.token);
        return (RequiredEvents::FrontNoneBackNone, ClientResult::CloseClient)
      } else {
        return (RequiredEvents::FrontNoneBackWrite, ClientResult::Continue)
      }
    }

    let has_host = self.state.has_host();
    let (sz, res) = self.frontend.socket_read(self.front_buf.space());
    debug!("FRONT [{:?}]: read {} bytes", self.token, sz);
    self.front_buf.fill(sz);
    match res {
      SocketResult::Error => return (RequiredEvents::FrontNoneBackNone, ClientResult::CloseClient),
      _                   => {
        if !has_host {
          self.state = parse_request_until_stop(&self.state, &mut self.front_buf, 0);
          debug!("parse_request_until_stop returned {:?} => advance: {}", self.state, self.state.req_position);
          if self.state.is_front_error() {
            METRICS.lock().unwrap().time("http_proxy.failure", (precise_time_ns() - self.start) / 1000);
            return (RequiredEvents::FrontNoneBackNone, ClientResult::CloseClient);
          }

          if self.state.has_host() {
            return (RequiredEvents::FrontReadBackWrite, ClientResult::ConnectBackend)
          } else {
            return (RequiredEvents::FrontReadBackNone, ClientResult::Continue)
          }
        } else {
          match self.state.request {
            RequestState::Request(_,_,_) | RequestState::RequestWithBody(_,_,_,_) => {
              //FIXME: should only read as much data as needed (ie not further than req_position)
              if self.front_buf_position + self.front_buf.available_data() >= self.state.req_position {
                return  (RequiredEvents::FrontNoneBackWrite, ClientResult::Continue)
              } else {
                return  (RequiredEvents::FrontReadBackWrite, ClientResult::Continue)
              }
            },
            RequestState::RequestWithBodyChunks(_,_,_,ch) => {
              if ch == Chunk::Ended {
                panic!("front read should have stopped on chunk ended");
                return (RequiredEvents::FrontNoneBackWrite, ClientResult::Continue);
              } else if ch == Chunk::Error {
                panic!("front read should have stopped on chunk error");
                return (RequiredEvents::FrontNoneBackNone, ClientResult::CloseClient);
              } else {
                if self.front_buf_position + self.front_buf.available_data() >= self.state.req_position {
                  let next_start: usize = self.state.req_position - self.front_buf_position;
                  self.state = parse_request_until_stop(&self.state, &mut self.front_buf, next_start);
                  debug!("parse_request_until_stop returned {:?} => advance: {}", self.state, self.state.req_position);
                  if self.state.is_front_error() {
                    METRICS.lock().unwrap().time("http_proxy.failure", (precise_time_ns() - self.start) / 1000);
                    return (RequiredEvents::FrontNoneBackNone, ClientResult::CloseClient);
                  }

                  if let RequestState::RequestWithBodyChunks(_,_,_,Chunk::Ended) = self.state.request {
                    return (RequiredEvents::FrontNoneBackWrite, ClientResult::Continue);
                  } else {
                    return (RequiredEvents::FrontReadBackWrite, ClientResult::Continue);
                  }
                } else {
                  return (RequiredEvents::FrontReadBackWrite, ClientResult::Continue);
                }
              }
            },
            _ => {
              let next_start: usize = self.state.req_position - self.front_buf_position;
              self.state = parse_request_until_stop(&self.state, &mut self.front_buf, next_start);
              debug!("parse_request_until_stop returned {:?} => advance: {}", self.state, self.state.req_position);
              if self.state.is_front_error() {
                METRICS.lock().unwrap().time("http_proxy.failure", (precise_time_ns() - self.start) / 1000);
                return (RequiredEvents::FrontNoneBackNone, ClientResult::CloseClient);
              }

              if let RequestState::Request(_,_,_) = self.state.request {
                return (RequiredEvents::FrontNoneBackWrite, ClientResult::Continue);
              } else {
                return (RequiredEvents::FrontReadBackWrite, ClientResult::Continue);
              }
            }
          }
        }
      }
    }
  }

  // Forward content to client
  fn writable(&mut self) -> (RequiredEvents, ClientResult) {
    if self.status == ClientStatus::DefaultAnswer {
      let (sz, res) = self.frontend.socket_write(self.back_buf.data());
      self.back_buf.consume(sz);

      if self.back_buf.available_data() == 0 {
        return (RequiredEvents::FrontNoneBackNone, ClientResult::CloseClient);
      }

      return match res {
        SocketResult::Error => (RequiredEvents::FrontNoneBackNone, ClientResult::CloseClient),
        _                   => (RequiredEvents::FrontWriteBackNone, ClientResult::Continue)
      };
    }

    trace!("writable RES pos: {}, buf pos: {}, available: {}", self.state.res_position, self.back_buf_position, self.back_buf.available_data());
    //assert!(self.back_buf_position + self.back_buf.available_data() <= self.state.res_position);
    if self.back_buf.available_data() == 0 {
      return (RequiredEvents::FrontNoneBackRead, ClientResult::Continue);
    }

    let to_copy = min(self.state.res_position - self.back_buf_position, self.back_buf.available_data());
    let (sz, res) = self.frontend.socket_write(&(self.back_buf.data())[..to_copy]);
    self.back_buf.consume(sz);
    self.back_buf_position += sz;
    if let Some((front,back)) = self.tokens() {
      debug!("FRONT [{}<-{}]: wrote {} bytes", front.as_usize(), back.as_usize(), sz);
    }
    match res {
      SocketResult::Error => (RequiredEvents::FrontNoneBackNone, ClientResult::CloseClient),
      _                   => {
        if self.back_buf_position == self.state.res_position {
          match self.state.response {
            ResponseState::ResponseWithBodyChunks(_,_,Chunk::Ended) => {
              self.reset();
              (RequiredEvents::FrontReadBackNone, ClientResult::Continue)
            },
            ResponseState::ResponseWithBodyChunks(_,_,_) => (RequiredEvents::FrontReadBackNone, ClientResult::Continue),
            ResponseState::ResponseWithBody(_,_,_)       => {
              self.reset();
              (RequiredEvents::FrontReadBackNone, ClientResult::Continue)
            },
            ResponseState::Response(_,_) => (RequiredEvents::FrontNoneBackRead, ClientResult::Continue),
            _ => (RequiredEvents::FrontNoneBackNone, ClientResult::CloseBothFailure)
          }
        } else {
          (RequiredEvents::FrontWriteBackRead, ClientResult::Continue)
        }
      }
    }
  }

  // Forward content to application
  fn back_writable(&mut self) -> (RequiredEvents, ClientResult) {
    if self.status == ClientStatus::DefaultAnswer {
      return (RequiredEvents::FrontWriteBackNone, ClientResult::Continue)
    }

    trace!("back writable REQ pos: {}, buf pos: {}, available: {}", self.state.req_position, self.front_buf_position, self.front_buf.available_data());
    //assert!(self.front_buf_position + self.front_buf.available_data() <= self.state.req_position);
    if self.front_buf.available_data() == 0 {
      return (RequiredEvents::FrontReadBackNone, ClientResult::Continue);
    }

    let to_copy = min(self.state.req_position - self.front_buf_position, self.front_buf.available_data());
    let tokens = self.tokens().clone();
    let res = if let Some(ref mut sock) = self.backend {
      let (sz, socket_res) = sock.socket_write(&(self.front_buf.data())[..to_copy]);
      self.front_buf.consume(sz);
      self.front_buf_position += sz;
      if let Some((front,back)) = tokens {
        debug!("BACK  [{}->{}]: wrote {} bytes", front.as_usize(), back.as_usize(), sz);
      }
      match socket_res {
        SocketResult::Error => (RequiredEvents::FrontNoneBackNone, ClientResult::CloseBothFailure),
        _                   => {
          // FIXME/ should read exactly as much data as needed
          if self.front_buf_position >= self.state.req_position {
            match self.state.request {
              RequestState::RequestWithBodyChunks(_,_,_,Chunk::Ended) => (RequiredEvents::FrontNoneBackRead, ClientResult::Continue),
              RequestState::RequestWithBodyChunks(_,_,_,_) => (RequiredEvents::FrontReadBackNone, ClientResult::Continue),
              RequestState::RequestWithBody(_,_,_,_)       => (RequiredEvents::FrontNoneBackRead, ClientResult::Continue),
              RequestState::Request(_,_,_) => (RequiredEvents::FrontNoneBackRead, ClientResult::Continue),
              _ => (RequiredEvents::FrontNoneBackNone, ClientResult::CloseBothFailure)
            }
          } else {
            (RequiredEvents::FrontReadBackWrite, ClientResult::Continue)
          }
        }
      }
    } else {
      return (RequiredEvents::FrontNoneBackNone, ClientResult::CloseBothFailure)
    };

    res
  }

  // Read content from application
  fn back_readable(&mut self) -> (RequiredEvents, ClientResult) {
    if self.status == ClientStatus::DefaultAnswer {
      return (RequiredEvents::FrontWriteBackNone, ClientResult::Continue)
    }

    trace!("writable RES pos: {}, buf pos: {}, available: {}", self.state.res_position, self.back_buf_position, self.back_buf.available_data());
    //assert!(self.back_buf_position + self.back_buf.available_data() <= self.state.res_position);

    if self.back_buf.available_space() == 0 {
      //println!("BACK BUFFER FULL({} bytes): TOKENS {:?} {:?}", self.back_buf.available_data(), self.token, self.backend_token);
      return (RequiredEvents::FrontWriteBackNone, ClientResult::Continue);
    }

    let tokens = self.tokens().clone();
    if let Some(ref mut sock) = self.backend {
      let (sz, r) = sock.socket_read(&mut self.back_buf.space());
      self.back_buf.fill(sz);
      if let Some((front,back)) = tokens {
        debug!("BACK  [{}->{}]: read {} bytes", front.as_usize(), back.as_usize(), sz);
      }
      match r {
        SocketResult::Error => (RequiredEvents::FrontNoneBackNone, ClientResult::CloseBothFailure),
        _                   => {
          match self.state.response {
            ResponseState::Response(_,_) => {
              //FIXME: this keeps happening, why? Readable event already in queue when parsing ended?
              error!("should not go back in back_readable if the whole response was parsed");
              return  (RequiredEvents::FrontWriteBackNone, ClientResult::Continue);
            },
            ResponseState::ResponseWithBody(_,_,_) => {
              //FIXME: should only read as much data as needed (ie not further than req_position)
              if self.back_buf_position + self.back_buf.available_data() >= self.state.res_position {
                return  (RequiredEvents::FrontWriteBackNone, ClientResult::Continue)
              } else {
                return  (RequiredEvents::FrontWriteBackRead, ClientResult::Continue)
              }
            },
            ResponseState::ResponseWithBodyChunks(_,_,ch) => {
              if ch == Chunk::Ended {
                panic!("back read should have stopped on chunk ended");
                return (RequiredEvents::FrontWriteBackNone, ClientResult::Continue);
              } else if ch == Chunk::Error {
                panic!("back read should have stopped on chunk error");
                return (RequiredEvents::FrontNoneBackNone, ClientResult::CloseClient);
              } else {
                if self.back_buf_position + self.back_buf.available_data() >= self.state.res_position {
                  let next_start: usize = self.state.res_position - self.back_buf_position;
                  self.state = parse_response_until_stop(&self.state, &mut self.back_buf, next_start);
                  debug!("parse_response_until_stop returned {:?} => advance: {}", self.state, self.state.res_position);
                  if self.state.is_back_error() {
                    METRICS.lock().unwrap().time("http_proxy.failure", (precise_time_ns() - self.start) / 1000);
                    return (RequiredEvents::FrontNoneBackNone, ClientResult::CloseBothFailure);
                  }

                  if let ResponseState::ResponseWithBodyChunks(_,_,Chunk::Ended) = self.state.response {
                    return (RequiredEvents::FrontWriteBackNone, ClientResult::Continue);
                  } else {
                    return (RequiredEvents::FrontWriteBackRead, ClientResult::Continue);
                  }
                } else {
                  return (RequiredEvents::FrontWriteBackRead, ClientResult::Continue);
                }
              }
            },
            ResponseState::Error(_) => panic!("back read should have stopped on responsestate error"),
            _ => {
              let next_start: usize = self.state.res_position - self.back_buf_position;
              self.state = parse_response_until_stop(&self.state, &mut self.back_buf, next_start);
              debug!("parse_response_until_stop returned {:?} => advance: {}", self.state, self.state.res_position);
              if self.state.is_back_error() {
                METRICS.lock().unwrap().time("http_proxy.failure", (precise_time_ns() - self.start) / 1000);
                return (RequiredEvents::FrontNoneBackNone, ClientResult::CloseBothFailure);
              }

              if let ResponseState::Response(_,_) = self.state.response {
                return (RequiredEvents::FrontWriteBackNone, ClientResult::Continue);
              } else {
                return (RequiredEvents::FrontWriteBackRead, ClientResult::Continue);
              }
            }
          }
        }
      }
    } else {
      return (RequiredEvents::FrontNoneBackNone, ClientResult::CloseBothFailure);
    }
  }

}

type ClientToken = Token;

#[allow(non_snake_case)]
pub struct DefaultAnswers {
  pub NotFound:           Vec<u8>,
  pub ServiceUnavailable: Vec<u8>
}

pub struct ServerConfiguration {
  listener:  TcpListener,
  address:   SocketAddr,
  instances: HashMap<String, Vec<Backend>>,
  fronts:    HashMap<String, Vec<HttpFront>>,
  tx:        mpsc::Sender<ServerMessage>,
  pool:      Pool<Buffer>,
  answers:   DefaultAnswers,
  front_timeout:   u64,
  back_timeout:    u64,
}

impl ServerConfiguration {
  pub fn new(address: SocketAddr, tx: mpsc::Sender<ServerMessage>, max_connections: usize, event_loop: &mut EventLoop<HttpServer>) -> io::Result<ServerConfiguration> {
    match TcpListener::bind(&address) {
      Ok(sock) => {
        event_loop.register(&sock, Token(0), EventSet::readable(), PollOpt::level());
        Ok(ServerConfiguration {
          listener:  sock,
          address:   address,
          instances: HashMap::new(),
          fronts:    HashMap::new(),
          tx:        tx,
          pool:      Pool::with_capacity(2*max_connections, 0, || Buffer::with_capacity(12000)),
          front_timeout: 50000,
          back_timeout:  50000,
          answers:   DefaultAnswers {
            NotFound: Vec::from(&b"HTTP/1.1 404 Not Found\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"[..]),
            ServiceUnavailable: Vec::from(&b"HTTP/1.1 503 your application is in deployment\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"[..]),
          }
        })
      },
      Err(e) => {
        error!("could not create listener {:?}: {:?}", address, e);
        Err(e)
      }
    }
  }

  pub fn add_http_front(&mut self, http_front: HttpFront, event_loop: &mut EventLoop<HttpServer>) {
    let front2 = http_front.clone();
    let front3 = http_front.clone();
    if let Some(fronts) = self.fronts.get_mut(&http_front.hostname) {
        fronts.push(front2);
    }

    // FIXME: check that http front port matches the listener's port

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
      let mut backend = Backend::new(*instance_address);
      addrs.push(backend);
    }

    if self.instances.get(app_id).is_none() {
      let mut backend = Backend::new(*instance_address);
      self.instances.insert(String::from(app_id), vec![backend]);
    }
  }

  pub fn remove_instance(&mut self, app_id: &str, instance_address: &SocketAddr, event_loop: &mut EventLoop<HttpServer>) {
      if let Some(instances) = self.instances.get_mut(app_id) {
        instances.retain(|backend| &backend.address != instance_address);
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

  pub fn backend_from_request(&mut self, client: &mut Client<TcpStream>, host: &str, uri: &str) -> Result<SocketAddr,ConnectionError> {
    // FIXME: the app id clone here is probably very inefficient
    if let Some(app_id) = self.frontend_from_request(host, uri).map(|ref front| front.app_id.clone()) {
      // ToDo round-robin on instances
      if let Some(ref mut app_instances) = self.instances.get_mut(&app_id) {
        if app_instances.len() == 0 {
          client.set_answer(&self.answers.ServiceUnavailable);
          return Err(ConnectionError::NoBackendAvailable);
        }
        let rnd = random::<usize>();
        let mut instances:Vec<&mut Backend> = app_instances.iter_mut().filter(|backend| backend.can_open()).collect();
        let idx = rnd % instances.len();
        info!("Connecting {} -> {:?}", host, instances.get(idx).map(|backend| (backend.address, backend.active_connections)));
        instances.get_mut(idx).map(|ref mut backend| {
          backend.inc_connections();
          backend.address
        }).ok_or(ConnectionError::NoBackendAvailable)
      } else {
        //FIXME: send 503 here
        client.set_answer(&self.answers.ServiceUnavailable);
        Err(ConnectionError::NoBackendAvailable)
      }
    } else {
      // FIXME: send 404 here
      client.set_answer(&self.answers.NotFound);
      Err(ConnectionError::HostNotFound)
    }
  }

}

impl ProxyConfiguration<HttpServer,Client<TcpStream>> for ServerConfiguration {
  fn connect_to_backend(&mut self, client: &mut Client<TcpStream>) -> Result<(),ConnectionError> {
      let host   = try!(client.state.get_host().ok_or(ConnectionError::NoHostGiven));
      let rl     = try!(client.state.get_request_line().ok_or(ConnectionError::NoRequestLineGiven));
      let back   = try!(self.backend_from_request(client, &host, &rl.uri));
      let conn   = TcpStream::connect(&back);

      if let Ok(socket) = conn {
        socket.set_nodelay(true);
        client.set_back_socket(socket);
        Ok(())
      } else {
        //FIXME: send 503 here
        client.set_answer(&self.answers.ServiceUnavailable);
        Err(ConnectionError::NoBackendAvailable)
      }
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
      ProxyOrder::Command(Command::HttpProxy(configuration)) => {
        info!("modifying proxy configuration: {:?}", configuration);
        self.front_timeout = configuration.front_timeout;
        self.back_timeout  = configuration.back_timeout;
        self.answers = DefaultAnswers {
          NotFound:           configuration.answer_404.into_bytes(),
          ServiceUnavailable: configuration.answer_503.into_bytes(),
        };
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
    if let (Some(front_buf), Some(back_buf)) = (self.pool.checkout(), self.pool.checkout()) {
      let accepted = self.listener.accept();

      if let Ok(Some((frontend_sock, _))) = accepted {
        frontend_sock.set_nodelay(true);
        if let Some(c) = Client::new(frontend_sock, front_buf, back_buf) {
          return Some((c, false))
        }
      } else {
        error!("could not accept: {:?}", accepted);
      }
    } else {
      error!("could not get buffers from pool");
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

pub type HttpServer = Server<ServerConfiguration,Client<TcpStream>>;

pub fn start_listener(front: SocketAddr, max_connections: usize, tx: mpsc::Sender<ServerMessage>) -> (Sender<ProxyOrder>,thread::JoinHandle<()>)  {
  let mut event_loop = EventLoop::new().unwrap();
  let channel = event_loop.channel();
  let notify_tx = tx.clone();

  let configuration = ServerConfiguration::new(front, tx, max_connections, &mut event_loop).unwrap();
  let mut server = HttpServer::new(1, max_connections, configuration);

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
  use network::buffer::Buffer;
  use pool::Pool;

  #[allow(unused_mut, unused_must_use, unused_variables)]
  #[test]
  fn mi() {
    thread::spawn(|| { start_server(); });
    let front: SocketAddr = FromStr::from_str("127.0.0.1:1024").unwrap();
    let (tx,rx) = channel::<ServerMessage>();
    let (sender, jg) = start_listener(front, 10, tx.clone());
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
      HttpFront { app_id: app_id1, hostname: "lolcatho.st".to_owned(), path_begin: uri1, port: 8080 },
      HttpFront { app_id: app_id2, hostname: "lolcatho.st".to_owned(), path_begin: uri2, port: 8080 },
      HttpFront { app_id: app_id3, hostname: "lolcatho.st".to_owned(), path_begin: uri3, port: 8080 }
    ]);
    fronts.insert("other.domain".to_owned(), vec![
      HttpFront { app_id: "app_1".to_owned(), hostname: "other.domain".to_owned(), path_begin: "/test".to_owned(), port: 8080 },
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
      pool:      Pool::with_capacity(1,0, || Buffer::with_capacity(12000)),
      front_timeout: 50000,
      back_timeout:  50000,
      answers:   DefaultAnswers {
        NotFound: Vec::from(&b"HTTP/1.1 404 Not Found\r\n\r\n"[..]),
        ServiceUnavailable: Vec::from(&b"HTTP/1.1 503 your application is in deployment\r\n\r\n"[..]),
      }
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
