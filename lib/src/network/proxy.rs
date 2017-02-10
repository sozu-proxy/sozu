#![allow(dead_code, unused_must_use, unused_variables, unused_imports)]

use std::thread::{self,Thread,Builder};
//use std::sync::mpsc::{self,channel,Receiver};
use std::sync::mpsc::TryRecvError;
use std::net::SocketAddr;
use mio::tcp::*;
use mio::*;
use mio::timer::{Timer,Timeout};
use mio::channel::Receiver;
use std::collections::HashMap;
use std::io::{self,Read,ErrorKind};
use nom::HexDisplay;
use std::error::Error;
use slab::Slab;
use std::io::Write;
use std::str::FromStr;
use std::marker::PhantomData;
use std::fmt::Debug;
use time::precise_time_ns;
use std::time::Duration;
use rand::random;

use network::{ClientResult,MessageId,ServerMessage,ServerMessageStatus,ConnectionError,
  SocketType,socket_type,Protocol,ProxyOrder,RequiredEvents};
use messages::{self,TcpFront,Order,Instance};
use channel::Channel;

const SERVER: Token = Token(0);
const DEFAULT_FRONT_TIMEOUT: u64 = 50000;
const DEFAULT_BACK_TIMEOUT:  u64 = 50000;

pub type ProxyChannel = Channel<ServerMessage,ProxyOrder>;

#[derive(Copy,Clone,Debug,PartialEq,Eq,PartialOrd,Ord,Hash)]
pub struct ListenToken(pub usize);
#[derive(Copy,Clone,Debug,PartialEq,Eq,PartialOrd,Ord,Hash)]
pub struct FrontToken(pub usize);
#[derive(Copy,Clone,Debug,PartialEq,Eq,PartialOrd,Ord,Hash)]
pub struct BackToken(pub usize);

impl From<usize> for ListenToken {
    fn from(val: usize) -> ListenToken {
        ListenToken(val)
    }
}

impl From<ListenToken> for usize {
    fn from(val: ListenToken) -> usize {
        val.0
    }
}

impl From<usize> for FrontToken {
    fn from(val: usize) -> FrontToken {
        FrontToken(val)
    }
}

impl From<FrontToken> for usize {
    fn from(val: FrontToken) -> usize {
        val.0
    }
}

impl From<usize> for BackToken {
    fn from(val: usize) -> BackToken {
        BackToken(val)
    }
}

impl From<BackToken> for usize {
    fn from(val: BackToken) -> usize {
        val.0
    }
}

#[derive(Debug)]
pub struct Readiness {
  pub front_interest:  Ready,
  pub back_interest:   Ready,
  pub front_readiness: Ready,
  pub back_readiness:  Ready,
}

impl Readiness {
  pub fn new() -> Readiness {
    Readiness {
      front_interest:  Ready::none(),
      back_interest:   Ready::none(),
      front_readiness: Ready::none(),
      back_readiness:  Ready::none(),
    }
  }

  pub fn reset(&mut self) {
    self.front_interest  = Ready::none();
    self.back_interest   = Ready::none();
    self.front_readiness = Ready::none();
    self.back_readiness  = Ready::none();
  }
}

pub trait ProxyClient {
  fn front_socket(&self) -> &TcpStream;
  fn back_socket(&self)  -> Option<&TcpStream>;
  fn front_token(&self)  -> Option<Token>;
  fn back_token(&self)   -> Option<Token>;
  fn close(&mut self);
  fn log_context(&self)  -> String;
  fn set_back_socket(&mut self, TcpStream);
  fn set_front_token(&mut self, token: Token);
  fn set_back_token(&mut self, token: Token);
  fn front_timeout(&mut self) -> Option<Timeout>;
  fn back_timeout(&mut self)  -> Option<Timeout>;
  fn set_front_timeout(&mut self, timeout: Timeout);
  fn set_back_timeout(&mut self, timeout: Timeout);
  fn front_hup(&mut self)     -> ClientResult;
  fn back_hup(&mut self)      -> ClientResult;
  fn readable(&mut self)      -> ClientResult;
  fn writable(&mut self)      -> ClientResult;
  fn back_readable(&mut self) -> ClientResult;
  fn back_writable(&mut self) -> ClientResult;
  fn remove_backend(&mut self) -> (Option<String>, Option<SocketAddr>);
  fn readiness(&mut self)      -> &mut Readiness;
  fn protocol(&self)           -> Protocol;
}

#[derive(Debug,PartialEq)]
pub enum BackendConnectAction {
  New,
  Reuse,
  Replace,
}

pub trait ProxyConfiguration<Client> {
  fn connect_to_backend(&mut self, event_loop: &mut Poll, client:&mut Client) ->Result<BackendConnectAction,ConnectionError>;
  fn notify(&mut self, event_loop: &mut Poll, message: ProxyOrder);
  fn accept(&mut self, token: ListenToken) -> Option<(Client, bool)>;
  fn close_backend(&mut self, app_id: String, addr: &SocketAddr);
  fn front_timeout(&self) -> u64;
  fn back_timeout(&self)  -> u64;
  fn channel(&mut self)   -> &mut ProxyChannel;
  fn tag(&self)           -> &str;
}

pub struct Server<ServerConfiguration,Client> {
  configuration:   ServerConfiguration,
  clients:         Slab<Client,FrontToken>,
  backend:         Slab<FrontToken,BackToken>,
  max_listeners:   usize,
  max_connections: usize,
  pub poll:        Poll,
  timer:           Timer<Token>,
  shutting_down:   Option<MessageId>,
}

impl<ServerConfiguration:ProxyConfiguration<Client>,Client:ProxyClient> Server<ServerConfiguration,Client> {
  pub fn new(max_listeners: usize, max_connections: usize, configuration: ServerConfiguration, poll: Poll) -> Self {
    let mut configuration = configuration;
    poll.register(configuration.channel(), Token(0), Ready::all(), PollOpt::edge()).expect("should register the channel");
    let clients = Slab::with_capacity(max_connections);
    let backend = Slab::with_capacity(max_connections);
    //let timer   = timer::Builder::default().tick_duration(Duration::from_millis(1000)).build();
    let timer   = Timer::default();
    //FIXME: registering the timer makes the timer thread spin too much
    //poll.register(&timer, Token(1), Ready::readable(), PollOpt::edge()).expect("should register the timer");
    Server {
      configuration:   configuration,
      clients:         clients,
      backend:         backend,
      max_listeners:   max_listeners,
      max_connections: max_connections,
      poll:            poll,
      timer:           timer,
      shutting_down:   None,
    }
  }

  pub fn tag(&self) -> &str {
    self.configuration.tag()
  }

  pub fn to_front(&self, token: Token) -> FrontToken {
    FrontToken(token.0 - 2 - self.max_listeners)
  }

  pub fn to_back(&self, token: Token) -> BackToken {
    BackToken(token.0 - 2 - self.max_listeners - self.max_connections)
  }

  pub fn from_front(&self, token: FrontToken) -> Token {
    Token(token.0 + 2 + self.max_listeners )
  }

  pub fn from_back(&self, token: BackToken) -> Token {
    Token(token.0 + 2 + self.max_listeners + self.max_connections)
  }


  pub fn configuration(&mut self) -> &mut ServerConfiguration {
    &mut self.configuration
  }

  pub fn close_client(&mut self, token: FrontToken) {
    self.clients[token].front_socket().shutdown(Shutdown::Both);
    self.poll.deregister(self.clients[token].front_socket());
    if let Some(sock) = self.clients[token].back_socket() {
      sock.shutdown(Shutdown::Both);
      self.poll.deregister(sock);
    }

    self.close_backend(token);
    self.clients[token].close();
    self.clients.remove(token);
  }

  pub fn close_backend(&mut self, token: FrontToken) {
    if let Some(backend_tk) = self.clients[token].back_token() {
      let backend_token = self.to_back(backend_tk);
      if self.backend.contains(backend_token) {
        self.backend.remove(backend_token);
        if let (Some(app_id), Some(addr)) = self.clients[token].remove_backend() {
          self.configuration.close_backend(app_id, &addr);
        }
      }
    }
  }

  pub fn accept(&mut self, token: ListenToken) {
    if let Some((client, should_connect)) = self.configuration.accept(token) {
      if let Ok(client_token) = self.clients.insert(client) {
        self.clients[client_token].readiness().front_interest = Ready::readable() | Ready::hup() | Ready::error();
        self.poll.register(self.clients[client_token].front_socket(), self.from_front(client_token), Ready::all(), PollOpt::edge() );
        let front = self.from_front(client_token);
        &self.clients[client_token].set_front_token(front);

        let front = self.from_front(client_token);
        /*
        if let Ok(timeout) = self.timer.set_timeout(Duration::from_millis(self.configuration.front_timeout()), front) {
          &self.clients[client_token].set_front_timeout(timeout);
        }
        */
        gauge!("accept", 1);
        if should_connect {
          self.connect_to_backend(client_token);
        }
      } else {
        error!("PROXY\tcould not add client to slab");
      }
    } else {
      error!("PROXY\tcould not create a client");
    }
  }

  pub fn connect_to_backend(&mut self, token: FrontToken) {
    match self.configuration.connect_to_backend(&mut self.poll, &mut self.clients[token]) {
      Ok(BackendConnectAction::Reuse) => {
        debug!("keepalive, reusing backend connection");
      }
      Ok(BackendConnectAction::Replace) => {
        if let Some(backend_token) = self.clients[token].back_token() {
          if let Some(sock) = self.clients[token].back_socket() {
            self.poll.register(sock, backend_token, Ready::all(), PollOpt::edge());
          }

          /*
          if let Ok(timeout) = self.timer.set_timeout(Duration::from_millis(self.configuration.back_timeout()), backend_token) {
            &self.clients[token].set_back_timeout(timeout);
          }
          */
          return;
        }
      },
      Ok(BackendConnectAction::New) => {
        if let Ok(backend_token) = self.backend.insert(token) {
          let back = self.from_back(backend_token);
          self.clients[token].set_back_token(back);

          //self.clients[token].readiness().back_interest = Ready::writable() | Ready::hup() | Ready::error();
          if let Some(sock) = self.clients[token].back_socket() {
            self.poll.register(sock, back, Ready::all(), PollOpt::edge());
          }

          let back = self.from_back(backend_token);
          /*
          if let Ok(timeout) = self.timer.set_timeout(Duration::from_millis(self.configuration.back_timeout()), back) {
            &self.clients[token].set_back_timeout(timeout);
          }
          */
          return;
        }
      },
      Err(ConnectionError::HostNotFound) | Err(ConnectionError::NoBackendAvailable) => {
        self.clients[token].readiness().front_interest = Ready::writable() | Ready::hup() | Ready::error();
        self.clients[token].readiness().back_interest  = Ready::hup() | Ready::error();
        //let mut front_interest = Ready::hup();
        //front_interest.insert(Ready::writable());
        //let client = &self.clients[token];

        //event_loop.reregister(client.front_socket(), token, front_interest, PollOpt::level() | PollOpt::oneshot());
      },
      _ => self.close_client(token),
    }
  }

  pub fn get_client_token(&self, token: Token) -> Option<FrontToken> {
    if token.0 < 2 + self.max_listeners {
      None
    } else if token.0 < 2 + self.max_listeners + self.max_connections && self.clients.contains(self.to_front(token)) {
      Some(self.to_front(token))
    } else if token.0 < 2 + self.max_listeners + 2 * self.max_connections && self.backend.contains(self.to_back(token)) {
      if self.clients.contains(self.backend[self.to_back(token)]) {
        Some(self.backend[self.to_back(token)])
      } else {
        None
      }
    } else {
      None
    }
  }

  pub fn interpret_client_order(&mut self, token: FrontToken, order: ClientResult) {
    //println!("INTERPRET ORDER: {:?}", order);
    match order {
      ClientResult::CloseClient      => self.close_client(token),
      ClientResult::CloseBackend     => self.close_backend(token),
      ClientResult::CloseBothSuccess => self.close_client(token),
      ClientResult::CloseBothFailure => self.close_client(token),
      ClientResult::ConnectBackend   => self.connect_to_backend(token),
      ClientResult::Continue         => {}
    }
  }

/*
  fn reregister(&self, token: Token, front_interest: Ready, back_interest: Ready) {
    let client = &self.clients[token];

    if let Some(frontend_token) = client.front_token() {
      self.poll.reregister(client.front_socket(), frontend_token, front_interest, PollOpt::level() | PollOpt::oneshot());
    }
     if let Some(backend_token) = client.back_token() {
       if let Some(sock) = client.back_socket() {
         self.poll.reregister(sock, backend_token, back_interest, PollOpt::level() | PollOpt::oneshot());
       }
     }
  }
*/
}

//type Timeout = usize;
type Message = ProxyOrder;

impl<ServerConfiguration:ProxyConfiguration<Client>,Client:ProxyClient> Server<ServerConfiguration,Client> {
  pub fn run(&mut self) {
    //FIXME: make those parameters configurable?
    let mut events = Events::with_capacity(1024);
    let poll_timeout = Some(Duration::from_millis(1000));
    loop {
      self.poll.poll(&mut events, poll_timeout).expect("should be able to poll for events");

      for event in events.iter() {
        if event.token() == Token(0) {
          let kind = event.kind();
          if kind.is_error() {
            error!("{}\terror reading from command channel", self.tag());
            continue;
          }
          if kind.is_hup() {
            error!("{}\tcommand channel was closed", self.tag());
            continue;
          }
          self.configuration.channel().handle_events(kind);
          self.configuration.channel().run();

          // loop here because iterations has borrow issues
          loop {
            let msg = self.configuration.channel().read_message();
            info!("{}\tgot message: {:?}", self.tag(), msg);

            // if the message was too large, we grow the buffer and retry to read if possible
            if msg.is_none() {
              if (self.configuration.channel().interest & self.configuration.channel().readiness).is_readable() {
                self.configuration.channel().run();
                continue;
              } else {
                break;
              }
            }

            let msg = msg.expect("the message should be valid");
            if let Order::HardStop = msg.order {
              self.notify(msg);
              self.configuration.channel().run();
              //FIXME: it's a bit brutal
              return;
            } else if let Order::SoftStop = msg.order {
              self.shutting_down = Some(msg.id.clone());
              self.notify(msg);
            } else {
              self.notify(msg);
            }
          }

          self.configuration.channel().run();
        } else if event.token() == Token(1) {
          while let Some(token) = self.timer.poll() {
            self.timeout(token);
          }
        } else {
          self.ready(event.token(), event.kind());
        }
      }

      //FIXME: manually call the timer instead of relying on a separate thread
      while let Some(token) = self.timer.poll() {
        self.timeout(token);
      }

      if self.shutting_down.is_some() && self.clients.len() == 0 {
        info!("last client stopped, shutting down!");
        self.configuration.channel().write_message(&ServerMessage{ id: self.shutting_down.take().expect("should have shut down correctly"), status: ServerMessageStatus::Ok});
        self.configuration.channel().run();
        return;
      }
    }
  }

  fn ready(&mut self, token: Token, events: Ready) {
    //info!("PROXY\t{:?} got events: {:?}", token, events);

    let client_token:FrontToken = match socket_type(token, self.max_listeners, self.max_connections) {
      Some(SocketType::Listener) => {
        if events.is_readable() {
          self.accept(ListenToken(token.0));
          return;
        }

        if events.is_writable() {
          error!("{}\treceived writable for listener {:?}, this should not happen", self.tag(), token);
          return;
        }

        if events.is_hup() {
          error!("{}\tshould not happen: server {:?} closed", self.tag(), token);
          return;
        }
        unreachable!();
      },
      Some(SocketType::FrontClient) => {
        let front_token = self.to_front(token);
        if self.clients.contains(front_token) {
          self.clients[front_token].readiness().front_readiness = self.clients[front_token].readiness().front_readiness | events;
          front_token
        } else {
          return;
        }
      },
      Some(SocketType::BackClient) => {
        if let Some(tok) = self.get_client_token(token) {
          self.clients[tok].readiness().back_readiness = self.clients[tok].readiness().back_readiness | events;
          tok
        } else {
          return;
        }
      },
      None => {
        return;
      }
    };

    loop {
      let front_interest = self.clients[client_token].readiness().front_interest &
        self.clients[client_token].readiness().front_readiness;
      let back_interest = self.clients[client_token].readiness().back_interest &
        self.clients[client_token].readiness().back_readiness;

      //info!("PROXY\t{:?} {:?} | {:?} front: {:?} | back: {:?} ", client_token, events, self.clients[client_token].readiness(), front_interest, back_interest);

      if front_interest == Ready::none() && back_interest == Ready::none() {
        break;
      }

      if !self.clients.contains(client_token) {
        break;
      }

      if front_interest.is_readable() {
        let order = self.clients[client_token].readable();
        trace!("{}\tfront readable\tinterpreting client order {:?}", self.tag(), order);

        // FIXME: should clear the timeout only if data was consumed
        if let Some(timeout) = self.clients[client_token].front_timeout() {
          //println!("[{}] clearing timeout", token.as_usize());
          self.timer.cancel_timeout(&timeout);
        }
        let front = self.from_front(client_token);
        /*
        if let Ok(timeout) = self.timer.set_timeout(Duration::from_millis(self.configuration.front_timeout()), front) {
          //println!("[{}] resetting timeout", front);
          &self.clients[client_token].set_front_timeout(timeout);
        }
        */

        self.interpret_client_order(client_token, order);
        //self.clients[client_token].readiness().front_readiness.remove(Ready::readable());
      }

      if back_interest.is_writable() {
        let order = self.clients[client_token].back_writable();
        self.interpret_client_order(client_token, order);
        //self.clients[client_token].readiness().back_readiness.remove(Ready::writable());
      }

      if back_interest.is_readable() {
        let order = self.clients[client_token].back_readable();

        // FIXME: should clear the timeout only if data was consumed
        if let Some(timeout) = self.clients[client_token].back_timeout() {
          //println!("[{}] clearing timeout", token.as_usize());
          self.timer.cancel_timeout(&timeout);
        }
        /*
        if let Some(back) = self.clients[client_token].back_token() {
          if let Ok(timeout) = self.timer.set_timeout(Duration::from_millis(self.configuration.back_timeout()), back) {
            //println!("[{}] resetting timeout", back);
            &self.clients[client_token].set_back_timeout(timeout);
          }
        }
        */

        self.interpret_client_order(client_token, order);
        //self.clients[client_token].readiness().back_readiness.remove(Ready::readable());
      }

      if front_interest.is_writable() {
        let order = self.clients[client_token].writable();
        trace!("{}\tfront writable\tinterpreting client order {:?}", self.tag(), order);
        self.interpret_client_order(client_token, order);
        //self.clients[client_token].readiness().front_readiness.remove(Ready::writable());
      }

      if !self.clients.contains(client_token) {
        break;
      }

      if front_interest.is_hup() {
        if self.clients[client_token].front_hup() == ClientResult::CloseClient {
          self.clients[client_token].readiness().front_interest = Ready::none();
          self.clients[client_token].readiness().back_interest  = Ready::none();
          self.clients[client_token].readiness().back_readiness.remove(Ready::hup());
          self.close_client(client_token);
          break;
        } else {
          self.clients[client_token].readiness().front_readiness.remove(Ready::hup());
        }
      }

      if back_interest.is_hup() {
        if self.clients[client_token].front_hup() == ClientResult::CloseClient {
          self.clients[client_token].readiness().front_interest = Ready::none();
          self.clients[client_token].readiness().back_interest  = Ready::none();
          self.clients[client_token].readiness().front_readiness.remove(Ready::hup());
          self.close_client(client_token);
          break;
        } else {
          self.clients[client_token].readiness().back_readiness.remove(Ready::hup());
        }
      }

      if front_interest.is_error() || back_interest.is_error() {
        error!("PROXY client {:?} got an error: front: {:?} back: {:?}", client_token, front_interest,
          back_interest);
        self.clients[client_token].readiness().front_interest = Ready::none();
        self.clients[client_token].readiness().back_interest  = Ready::none();
        self.close_client(client_token);
        break;
      }
    }
  }

  fn notify(&mut self, message: Message) {
    self.configuration.notify(&mut self.poll, message);
  }

  fn timeout(&mut self, token: Token) {
    match socket_type(token, self.max_listeners, self.max_connections) {
      Some(SocketType::Listener) => {
        error!("PROXY\tthe listener socket should have no timeout set");
      },
      Some(SocketType::FrontClient) => {
        let front_token = self.to_front(token);
        if self.clients.contains(front_token) {
          debug!("PROXY\tfrontend [{:?}] got timeout, closing", token);
          self.close_client(front_token);
        }
      },
      Some(SocketType::BackClient) => {
        if let Some(tok) = self.get_client_token(token) {
          debug!("PROXY\tbackend [{:?}] got timeout, closing", token);
          self.close_client(tok);
        }
      }
      None => {}
    }
  }
}
