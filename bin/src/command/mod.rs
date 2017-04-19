use mio::*;
use mio::timer::Timer;
use mio_uds::{UnixListener,UnixStream};
use slab::Slab;
use std::fs;
use std::fmt;
use std::path::PathBuf;
use std::io::{self,ErrorKind};
use std::os::unix::fs::PermissionsExt;
use std::collections::HashMap;
use std::time::Duration;
use libc::pid_t;
use serde_json;

use sozu::messages::Order;
use sozu::network::{ProxyOrder,ServerMessage,ServerMessageStatus};
use sozu::channel::Channel;
use sozu_command::state::{HttpProxy,TlsProxy,ConfigState};
use sozu_command::data::{ConfigMessage,ConfigMessageAnswer,ConfigMessageStatus,ProxyType,RunState};
use sozu_command::config::Config;

pub mod orders;
pub mod client;

use self::client::CommandClient;

const SERVER: Token = Token(0);
const HALF_USIZE: usize = 0x8000000000000000usize;

#[derive(Copy,Clone,Debug,PartialEq,Eq,PartialOrd,Ord,Hash)]
pub struct FrontToken(pub usize);

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

pub struct Worker {
  pub id:            u32,
  pub channel:       Channel<ProxyOrder,ServerMessage>,
  pub token:         Option<Token>,
  pub pid:           pid_t,
  pub run_state:     RunState,
  pub inflight:      HashMap<String,Order>,
}

impl Worker {
  pub fn new(id: u32, pid: pid_t, channel: Channel<ProxyOrder,ServerMessage>, config: &Config) -> Worker {
    Worker {
      id:         id,
      channel:    channel,
      token:      None,
      pid:        pid,
      run_state:  RunState::Running,
      inflight:   HashMap::new(),
    }
  }
}

impl fmt::Debug for Worker {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    write!(f, "Worker {{ id: {}, run_state: {:?} }}", self.id, self.run_state)
  }
}

#[derive(Deserialize,Serialize,Debug)]
pub struct ProxyConfiguration {
  id:    String,
  state: ConfigState,
}

pub type Tag = String;

pub struct CommandServer {
  sock:            UnixListener,
  buffer_size:     usize,
  max_buffer_size: usize,
  conns:           Slab<CommandClient,FrontToken>,
  proxies:         HashMap<Token, Worker>,
  next_id:         u32,
  state:           ConfigState,
  pub poll:        Poll,
  timer:           Timer<Token>,
  config:          Config,
  token_count:     usize,
}

impl CommandServer {
  fn accept(&mut self) -> io::Result<()> {
    debug!("server accepting socket");

    let acc = self.sock.accept();
    if let Ok(Some((sock, _))) = acc {
      let conn = CommandClient::new(sock, self.buffer_size, self.max_buffer_size);
      let tok = self.conns.insert(conn)
        .ok().expect("could not add connection to slab");

      // Register the connection
      let token = self.from_front(tok);
      self.conns[tok].token = Some(token);
      let mut interest = Ready::hup();
      interest.insert(Ready::readable());
      interest.insert(Ready::writable());
      self.poll.register(&self.conns[tok].channel.sock, token, interest, PollOpt::edge())
        .ok().expect("could not register socket with event loop");

      let accept_interest = Ready::readable();
      self.poll.reregister(&self.sock, SERVER, accept_interest, PollOpt::edge());
      Ok(())
    } else {
      //FIXME: what do other cases mean, like Ok(None)?
      acc.map(|_| ())
    }
  }

  fn dispatch(&mut self, token: FrontToken, v: Vec<ConfigMessage>) {
    for message in &v {
      self.handle_client_message(token, message);
    }
  }

  fn new(srv: UnixListener, config: Config, mut proxy_vec: Vec<Worker>, poll: Poll) -> CommandServer {
    //FIXME: verify this
    poll.register(&srv, Token(0), Ready::readable(), PollOpt::edge() | PollOpt::oneshot()).unwrap();

    let mut next_id = proxy_vec.len();

    let mut proxies = HashMap::new();

    let mut token_count = 0;
    //FIXME: verify there's at least one worker
    //TODO: make config state from Config ADD IP ADDRESSES AND PORTS
    let state: ConfigState = Default::default();


    for proxy in proxy_vec.drain(..) {
      token_count += 1;
      poll.register(&proxy.channel.sock, Token(token_count), Ready::all(), PollOpt::edge()).unwrap();
      proxies.insert(Token(token_count), proxy);
    }

    //let mut timer = timer::Builder::default().tick_duration(Duration::from_millis(1000)).build();
    //FIXME: registering the timer makes the timer thread spin too much
    let mut timer = timer::Timer::default();
    //poll.register(&timer, Token(1), Ready::readable(), PollOpt::edge()).unwrap();
    timer.set_timeout(Duration::from_millis(700), Token(0));

    let buffer_size = config.command_buffer_size.unwrap_or(10000);

    CommandServer {
      sock:            srv,
      buffer_size:     buffer_size,
      max_buffer_size: config.max_command_buffer_size.unwrap_or(buffer_size * 2),
      conns:           Slab::with_capacity(128),
      proxies:         proxies,
      next_id:         next_id as u32,
      state:           state,
      poll:            poll,
      timer:           timer,
      config:          config,
      token_count:     token_count,
    }
  }

  pub fn to_front(&self, token: Token) -> FrontToken {
    FrontToken(token.0 - HALF_USIZE - 1)
  }

  pub fn from_front(&self, token: FrontToken) -> Token {
    Token(token.0 + HALF_USIZE + 1)
  }

}

impl CommandServer {
  pub fn run(&mut self) {
    let mut events = Events::with_capacity(1024);
    let poll_timeout = Some(Duration::from_millis(1000));
    loop {
      self.poll.poll(&mut events, poll_timeout).unwrap();
      for event in events.iter() {
        self.ready(event.token(), event.kind());
      }
    }
  }

  fn ready(&mut self, token: Token, events: Ready) {
    //trace!("ready: {:?} -> {:?}", token, events);
    match token {
      Token(0) => {
        if events.is_readable() {
          self.accept().unwrap();
        } else {
          error!("received writable for token 0");
        }
      },
      Token(i) if i < HALF_USIZE + 1 => {
        let mut messages = {
          let ref mut proxy =  self.proxies.get_mut(&Token(i)).unwrap();
          proxy.channel.handle_events(events);
          proxy.channel.run();

          let mut messages = Vec::new();
          loop {
            let msg = proxy.channel.read_message();
            if msg.is_none() {
              break;
            } else {
              messages.push(msg.unwrap());
            }
          }

          messages
        };

        for msg in messages.drain(..) {
          self.proxy_handle_message(Token(i), msg);
        }

        {
          let ref mut proxy =  self.proxies.get_mut(&Token(i)).unwrap();
          proxy.channel.run();
        }

      },
      _ => {
        let conn_token = self.to_front(token);
        if self.conns.contains(conn_token) {
          self.conns[conn_token].channel.handle_events(events);
          self.conns[conn_token].channel.run();

          //FIXME: handle deconnection
          loop {
            let message = self.conns[conn_token].channel.read_message();

            // if the message was too large, we grow the buffer and retry to read if possible
            if message.is_none() {
              if self.conns[conn_token].channel.readiness.is_hup() {
                break;
              }

              if (self.conns[conn_token].channel.interest & self.conns[conn_token].channel.readiness).is_readable() {
                self.conns[conn_token].channel.run();
                continue;
              } else {
                break;
              }
            }

            let message = message.unwrap();
            self.handle_client_message(conn_token, &message);

          }

          self.conns[conn_token].channel.run();

          if self.conns[conn_token].channel.readiness.is_hup() {
            self.poll.deregister(&self.conns[conn_token].channel.sock);
            self.conns.remove(conn_token);
            trace!("closed client [{}]", token.0);
          }
        }
      }
    }
  }

  fn proxy_handle_message(&mut self, token: Token, msg: ServerMessage) {
    //println!("got answer msg: {:?}", msg);
    if msg.status != ServerMessageStatus::Processing {
      if let Some(ref mut proxy) = self.proxies.get_mut(&token) {
        if let Some(order) = proxy.inflight.remove(&msg.id) {
          info!("REMOVING INFLIGHT MESSAGE {}: {:?}", msg.id, order);
          // handle message completion here
          // there will probably be other cases to handle in the future
          if order == Order::SoftStop || order == Order::HardStop {
            proxy.run_state = RunState::Stopped
          }
        }
      }
    }

    let answer = ConfigMessageAnswer::new(
      msg.id.clone(),
      match msg.status {
        ServerMessageStatus::Error(_)   => ConfigMessageStatus::Error,
        ServerMessageStatus::Ok         => ConfigMessageStatus::Ok,
        ServerMessageStatus::Processing => ConfigMessageStatus::Processing,
      },
      match msg.status {
        ServerMessageStatus::Error(s) => s.clone(),
        _                             => String::new(),
      },
      None,
    );

    info!("sending: {:?}", answer);
    for client in self.conns.iter_mut() {
      if let Some(index) = client.has_message_id(&msg.id) {
        client.write_message(&answer);
        if answer.status != ConfigMessageStatus::Processing {
          client.remove_message_id(index);
        }
      }
      client.channel.run();
    }
  }

}

pub fn start(config: Config, proxies: Vec<Worker>) {
  let saved_state     = config.saved_state.clone();

  let event_loop = Poll::new().unwrap();
  let addr = PathBuf::from(&config.command_socket);
  if let Err(e) = fs::remove_file(&addr) {
    match e.kind() {
      ErrorKind::NotFound => {},
      _ => {
        error!("could not delete previous socket at {:?}: {:?}", addr, e);
        return;
      }
    }
  }
  match UnixListener::bind(&addr) {
    Ok(srv) => {
      if let Err(e) =  fs::set_permissions(&addr, fs::Permissions::from_mode(0o600)) {
        error!("could not set the unix socket permissions: {:?}", e);
        fs::remove_file(&addr);
        return;
      }

      let mut server = CommandServer::new(srv, config.clone(), proxies, event_loop);

      server.load_static_application_configuration();

      saved_state.as_ref().map(|state_path| {
        server.load_state("INITIALIZATION", state_path);
      });

      info!("listen for connections");
      server.run();
      //event_loop.run(&mut CommandServer::new(srv, proxies, buffer_size, max_buffer_size)).unwrap()
    },
      Err(e) => {
        error!("could not create unix socket: {:?}", e);
      }
  }
}
