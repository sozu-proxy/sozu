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
use log;
use libc::pid_t;
use serde_json;

use sozu::network::{ProxyOrder,ServerMessage,ServerMessageStatus};
use sozu::command::CommandChannel;

use state::{HttpProxy,TlsProxy,ConfigState};

pub mod data;
pub mod orders;
pub mod client;

use self::data::{ConfigMessage,ConfigMessageAnswer,ConfigMessageStatus,ListenerType};
use self::client::{ConnReadError,CommandClient};

const SERVER: Token = Token(0);

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

//#[derive(Serialize)]
pub struct Listener {
  pub tag:           String,
  pub id:            u8,
  pub listener_type: ListenerType,
  pub channel:       CommandChannel<ProxyOrder,ServerMessage>,
  pub state:         ConfigState,
  pub token:         Option<Token>,
  pub pid:           pid_t,
}

impl Listener {
  pub fn new(tag: String, id: u8, pid: pid_t, listener_type: ListenerType, ip_address: String, port: u16, channel: CommandChannel<ProxyOrder,ServerMessage>) -> Listener {
    let state = match listener_type {
      ListenerType::HTTP  => ConfigState::Http(HttpProxy::new(ip_address, port)),
      ListenerType::HTTPS => ConfigState::Tls(TlsProxy::new(ip_address, port)),
      _                   => unimplemented!(),
    };

    Listener {
      tag:           tag,
      id:            id,
      listener_type: listener_type,
      channel:       channel,
      state:         state,
      token:         None,
      pid:           pid,
    }
  }
}

impl fmt::Debug for Listener {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    write!(f, "Listener {{ tag: {}, listener_type: {:?}, state: {:?} }}", self.tag, self.listener_type, self.state)
  }
}

#[derive(Deserialize,Serialize,Debug)]
pub struct StoredListener {
  pub tag:           String,
  pub listener_type: ListenerType,
  pub state:         ConfigState,
}

impl StoredListener {
  pub fn from_listener(listener: &Listener) -> StoredListener {
    StoredListener {
      tag:           listener.tag.clone(),
      listener_type: listener.listener_type.clone(),
      state:         listener.state.clone(),
    }
  }
}

#[derive(Deserialize,Serialize,Debug)]
pub struct ListenerConfiguration {
  id:        String,
  listeners: Vec<StoredListener>,
}

pub type Tag = String;

struct CommandServer {
  sock:            UnixListener,
  buffer_size:     usize,
  max_buffer_size: usize,
  conns:           Slab<CommandClient,FrontToken>,
  listeners:       HashMap<Token, (Tag, Listener)>,
  listener_count:  usize,
  pub poll:        Poll,
  timer:           Timer<Token>,
}

impl CommandServer {
  fn accept(&mut self) -> io::Result<()> {
    debug!("server accepting socket");

    let acc = self.sock.accept();
    if let Ok(Some((sock, addr))) = acc {
      let conn = CommandClient::new(sock, self.buffer_size, self.max_buffer_size);
      let tok = self.conns.insert(conn)
        .ok().expect("could not add connection to slab");

      // Register the connection
      let token = self.from_front(tok);
      self.conns[tok].token = Some(token);
      let mut interest = Ready::hup();
      interest.insert(Ready::readable());
      interest.insert(Ready::writable());
      self.poll.register(&self.conns[tok].sock, token, interest, PollOpt::edge())
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

  fn new(srv: UnixListener, listeners_map: HashMap<String, Vec<Listener>>, buffer_size: usize, max_buffer_size: usize, poll: Poll) -> CommandServer {
    //FIXME: verify this
    poll.register(&srv, Token(0), Ready::readable(), PollOpt::edge() | PollOpt::oneshot()).unwrap();

    let mut listeners = HashMap::new();

    let mut token_count = 0;
    for (tag, mut listener_list) in listeners_map {
      for listener in listener_list.drain(..) {
        token_count += 1;
        poll.register(&listener.channel.sock, Token(token_count), Ready::all(), PollOpt::edge()).unwrap();
        listeners.insert(Token(token_count), (tag.clone(), listener));
      }
    }
    //let mut timer = timer::Builder::default().tick_duration(Duration::from_millis(1000)).build();
    //FIXME: registering the timer makes the timer thread spin too much
    let mut timer = timer::Timer::default();
    //poll.register(&timer, Token(1), Ready::readable(), PollOpt::edge()).unwrap();
    timer.set_timeout(Duration::from_millis(700), Token(0));
    CommandServer {
      sock:            srv,
      buffer_size:     buffer_size,
      max_buffer_size: max_buffer_size,
      conns:           Slab::with_capacity(128),
      listeners:       listeners,
      listener_count:  token_count,
      poll:            poll,
      timer:           timer,
    }
  }

  pub fn to_front(&self, token: Token) -> FrontToken {
    FrontToken(token.0 - self.listener_count - 1)
  }

  pub fn from_front(&self, token: FrontToken) -> Token {
    Token(token.0 + self.listener_count + 1)
  }

}

type Message = ();

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
      Token(i) if i < self.listener_count + 1 => {
        info!("token({}) worker got event {:?}", i, events);
        //println!("listeners:\n{:?}", self.listeners);

        let mut messages = {
          let &mut (ref tag, ref mut listener) =  self.listeners.get_mut(&Token(i)).unwrap();
          listener.channel.handle_events(events);
          listener.channel.run();

          let mut messages = Vec::new();
          loop {
            let msg = listener.channel.read_message();
            if msg.is_none() {
              break;
            } else {
              messages.push(msg.unwrap());
            }
          }

          messages
        };

        for msg in messages.drain(..) {
          self.listener_handle_message(Token(i), msg);
        }

        {
          let &mut (ref tag, ref mut listener) =  self.listeners.get_mut(&Token(i)).unwrap();
          listener.channel.run();
        }

      },
      _ => {
        let conn_token = self.to_front(token);
        if self.conns.contains(conn_token) {
          self.conns[conn_token].readiness = self.conns[conn_token].readiness | events;

          loop {
            info!("UNIX\t{:?}\t readiness: {:?}", conn_token, self.conns[conn_token].readiness);
            if self.conns[conn_token].readiness.is_none() {
              break;
            }

            if self.conns[conn_token].readiness.is_readable() {
              let opt_v = self.conns[conn_token].conn_readable(token);
              match opt_v {
                Ok(v) => self.dispatch(conn_token, v),
                Err(ConnReadError::Continue) => {},
                Err(ConnReadError::ParseError) | Err(ConnReadError::SocketError) => {
                  self.poll.deregister(&self.conns[conn_token].sock);
                  self.conns.remove(conn_token);
                  trace!("closed client [{}]", token.0);
                  break;
                }
              }
            }

            if self.conns[conn_token].readiness.is_writable() {
              if let Some(ref timeout) = self.conns[conn_token].write_timeout {
                self.timer.cancel_timeout(&timeout);
              }
              let res = self.conns[conn_token].conn_writable(token);
              if let Ok(timeout) = self.timer.set_timeout(Duration::from_millis(700), token) {
                self.conns[conn_token].write_timeout = Some(timeout);
              }
            }

            if self.conns[conn_token].readiness.is_hup() {
              self.poll.deregister(&self.conns[conn_token].sock);
              self.conns.remove(conn_token);
              trace!("closed client [{}]", token.0);
              break;
            }
          }
        }
      }
    }
  }

  fn listener_handle_message(&mut self, token: Token, msg: ServerMessage) {
    println!("got answer msg: {:?}", msg);
    let answer = ConfigMessageAnswer {
      id: msg.id.clone(),
      status: match msg.status {
        ServerMessageStatus::Error(_)   => ConfigMessageStatus::Error,
        ServerMessageStatus::Ok         => ConfigMessageStatus::Ok,
        ServerMessageStatus::Processing => ConfigMessageStatus::Processing,
      },
      message: match msg.status {
                 ServerMessageStatus::Error(s) => s.clone(),
                 _                             => String::new(),
               },
    };
    info!("sending: {:?}", answer);
    for client in self.conns.iter_mut() {
      if let Some(index) = client.has_message_id(&msg.id) {
        client.write_message(&serde_json::to_string(&answer).map(|s| s.into_bytes()).unwrap_or(vec!()));
        client.remove_message_id(index);
      }
    }
  }

}

pub fn start(path: String,  listeners: HashMap<String, Vec<Listener>>, saved_state: Option<String>, buffer_size: usize,
    max_buffer_size: usize) {
  let event_loop = Poll::new().unwrap();
  let addr = PathBuf::from(path);
  if let Err(e) = fs::remove_file(&addr) {
    match e.kind() {
      ErrorKind::NotFound => {},
      _ => {
        log!(log::LogLevel::Error, "could not delete previous socket at {:?}: {:?}", addr, e);
        return;
      }
    }
  }
  match UnixListener::bind(&addr) {
    Ok(srv) => {
      if let Err(e) =  fs::set_permissions(&addr, fs::Permissions::from_mode(0o600)) {
        log!( log::LogLevel::Error, "could not set the unix socket permissions: {:?}", e);
        fs::remove_file(&addr);
        return;
      }
      info!("listen for connections");
      //event_loop.register(&srv, SERVER, Ready::readable(), PollOpt::edge() | PollOpt::oneshot()).unwrap();
      /*FIXME: timeout
       * event_loop.timeout_ms(0, 700);
       */
      let mut server = CommandServer::new(srv, listeners, buffer_size, max_buffer_size, event_loop);
      saved_state.as_ref().map(|state_path| {
        server.load_state("INITIALIZATION", state_path);
      });

      server.run();
      //event_loop.run(&mut CommandServer::new(srv, listeners, buffer_size, max_buffer_size)).unwrap()
    },
      Err(e) => {
        log!(log::LogLevel::Error, "could not create unix socket: {:?}", e);
      }
  }
}
