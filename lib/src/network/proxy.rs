#![allow(dead_code, unused_must_use, unused_variables, unused_imports)]

use std::thread::{self,Thread,Builder};
use std::sync::mpsc::TryRecvError;
use std::net::{SocketAddr,Shutdown};
use mio::net::*;
use mio::*;
use mio::unix::UnixReady;
use mio::timer::{Timer,Timeout};
use std::collections::{HashSet,HashMap};
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
  SocketType,Protocol,ProxyOrder,RequiredEvents};
use network::{http,tls,tcp};
use network::session::{BackToken,FrontToken,ListenToken,ProxyClient,ProxyConfiguration,Readiness,Session};
use messages::{self,TcpFront,Order,Instance};
use channel::Channel;

const SERVER: Token = Token(0);
const DEFAULT_FRONT_TIMEOUT: u64 = 50000;
const DEFAULT_BACK_TIMEOUT:  u64 = 50000;

pub type ProxyChannel = Channel<ServerMessage,ProxyOrder>;

#[derive(Debug,Clone,PartialEq)]
enum ProxyType {
  HTTP,
  HTTPS,
  TCP,
}

pub struct Server<Client> {
  clients:         Slab<Client,FrontToken>,
  backend:         Slab<FrontToken,BackToken>,
  max_listeners:   usize,
  max_connections: usize,
  pub poll:        Poll,
  timer:           Timer<Token>,
  shutting_down:   Option<MessageId>,
  accept_ready:    HashSet<ListenToken>,
  can_accept:      bool,
  channel:         ProxyChannel,
  http:            Option<Session<http::ServerConfiguration, http::Client>>,
  https:           Option<Session<tls::ServerConfiguration, tls::TlsClient>>,
  tcp:             Option<Session<tcp::ServerConfiguration, tcp::Client>>,
}

impl<Client:ProxyClient> Server<Client> {
  pub fn new(max_listeners: usize, max_connections: usize, poll: Poll, channel: ProxyChannel,
    http: Option<Session<http::ServerConfiguration, http::Client>>, https: Option<Session<tls::ServerConfiguration, tls::TlsClient>>,
    tcp: Option<Session<tcp::ServerConfiguration, tcp::Client>>) -> Self {
    poll.register(
      &channel,
      Token(0),
      Ready::readable() | Ready::writable() | Ready::from(UnixReady::hup() | UnixReady::error()),
      PollOpt::edge()
    ).expect("should register the channel");

    let clients = Slab::with_capacity(max_connections);
    let backend = Slab::with_capacity(max_connections);
    //let timer   = timer::Builder::default().tick_duration(Duration::from_millis(1000)).build();
    let timer   = Timer::default();
    //FIXME: registering the timer makes the timer thread spin too much
    //poll.register(&timer, Token(1), Ready::readable(), PollOpt::edge()).expect("should register the timer");
    Server {
      clients:         clients,
      backend:         backend,
      max_listeners:   max_listeners,
      max_connections: max_connections,
      poll:            poll,
      timer:           timer,
      shutting_down:   None,
      accept_ready:    HashSet::new(),
      can_accept:      true,
      channel:         channel,
      http:            http,
      https:           https,
      tcp:             tcp,
    }
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
}

//type Timeout = usize;
type Message = ProxyOrder;

impl<Client:ProxyClient> Server<Client> {
  pub fn run(&mut self) {
    //FIXME: make those parameters configurable?
    let mut events = Events::with_capacity(1024);
    let poll_timeout = Some(Duration::from_millis(1000));
    loop {
      self.poll.poll(&mut events, poll_timeout).expect("should be able to poll for events");

      for event in events.iter() {
        if event.token() == Token(0) {
          let kind = event.readiness();
          if UnixReady::from(kind).is_error() {
            error!("error reading from command channel");
            continue;
          }
          if UnixReady::from(kind).is_hup() {
            error!("command channel was closed");
            continue;
          }
          self.channel.handle_events(kind);
          self.channel.run();

          // loop here because iterations has borrow issues
          loop {
            let msg = self.channel.read_message();
            //info!("got message: {:?}", msg);

            // if the message was too large, we grow the buffer and retry to read if possible
            if msg.is_none() {
              if (self.channel.interest & self.channel.readiness).is_readable() {
                self.channel.run();
                continue;
              } else {
                break;
              }
            }

            let msg = msg.expect("the message should be valid");
            if let Order::HardStop = msg.order {
              self.notify(msg);
              self.channel.run();
              //FIXME: it's a bit brutal
              return;
            } else if let Order::SoftStop = msg.order {
              self.shutting_down = Some(msg.id.clone());
              self.notify(msg);
            } else {
              self.notify(msg);
            }
          }

          self.channel.run();
        } else if event.token() == Token(1) {
          while let Some(token) = self.timer.poll() {
            self.timeout(token);
          }
        } else {
          //self.ready(event.token(), event.readiness());
          match proxy_type(event.token().0) {
            ProxyType::HTTP  => if let Some(mut http) = self.http.take() {
              http.ready(&mut self.poll, event.token(), event.readiness());
              self.http = Some(http);
            },
            ProxyType::HTTPS => if let Some(mut https) = self.https.take() {
              https.ready(&mut self.poll, event.token(), event.readiness());
              self.https = Some(https);
            },
            ProxyType::TCP   => if let Some(mut tcp) = self.tcp.take() {
              tcp.ready(&mut self.poll, event.token(), event.readiness());
              self.tcp = Some(tcp);
            },
          };
        }
      }

      if let Some(mut http) = self.http.take() {
        http.handle_remaining_readiness(&mut self.poll);
        self.http = Some(http);
      }
      if let Some(mut https) = self.https.take() {
        https.handle_remaining_readiness(&mut self.poll);
        self.https = Some(https);
      }
      if let Some(mut tcp) = self.tcp.take() {
        tcp.handle_remaining_readiness(&mut self.poll);
        self.tcp = Some(tcp);
      }

      //FIXME: manually call the timer instead of relying on a separate thread
      while let Some(token) = self.timer.poll() {
        self.timeout(token);
      }

      if self.shutting_down.is_some() && self.clients.len() == 0 {
        info!("last client stopped, shutting down!");
        self.channel.write_message(&ServerMessage{ id: self.shutting_down.take().expect("should have shut down correctly"), status: ServerMessageStatus::Ok});
        self.channel.run();
        return;
      }
    }
  }

  fn notify(&mut self, message: Message) {
    if let Some(mut http) = self.http.take() {
      http.configuration().notify(&mut self.poll, &mut self.channel, message.clone());
      self.http = Some(http);
    }
    if let Some(mut https) = self.https.take() {
      https.configuration().notify(&mut self.poll, &mut self.channel, message.clone());
      self.https = Some(https);
    }
    if let Some(mut tcp) = self.tcp.take() {
      tcp.configuration().notify(&mut self.poll, &mut self.channel, message);
      self.tcp = Some(tcp);
    }
  }

  fn timeout(&mut self, token: Token) {
    /*
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
    */
  }
}

fn proxy_type(token: usize) -> ProxyType {
  if token < 6148914691236517205 {
    ProxyType::HTTP
  } else if token < 12297829382473034410 {
    ProxyType::HTTPS
  } else {
    ProxyType::TCP
  }
}
