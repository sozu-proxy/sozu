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

use self::data::{ConfigMessage,ConfigMessageAnswer,ConfigMessageStatus,ProxyType};
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
pub struct Proxy {
  pub tag:           String,
  pub id:            u8,
  pub proxy_type:    ProxyType,
  pub channel:       CommandChannel<ProxyOrder,ServerMessage>,
  pub state:         ConfigState,
  pub token:         Option<Token>,
  pub pid:           pid_t,
}

impl Proxy {
  pub fn new(tag: String, id: u8, pid: pid_t, proxy_type: ProxyType, ip_address: String, port: u16, channel: CommandChannel<ProxyOrder,ServerMessage>) -> Proxy {
    let state = match proxy_type {
      ProxyType::HTTP  => ConfigState::Http(HttpProxy::new(ip_address, port)),
      ProxyType::HTTPS => ConfigState::Tls(TlsProxy::new(ip_address, port)),
      _                => unimplemented!(),
    };

    Proxy {
      tag:        tag,
      id:         id,
      proxy_type: proxy_type,
      channel:    channel,
      state:      state,
      token:      None,
      pid:        pid,
    }
  }
}

impl fmt::Debug for Proxy {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    write!(f, "Proxy {{ tag: {}, proxy_type: {:?}, state: {:?} }}", self.tag, self.proxy_type, self.state)
  }
}

#[derive(Deserialize,Serialize,Debug)]
pub struct StoredProxy {
  pub tag:        String,
  pub proxy_type: ProxyType,
  pub state:      ConfigState,
}

impl StoredProxy {
  pub fn from_proxy(proxy: &Proxy) -> StoredProxy {
    StoredProxy {
      tag:        proxy.tag.clone(),
      proxy_type: proxy.proxy_type.clone(),
      state:      proxy.state.clone(),
    }
  }
}

#[derive(Deserialize,Serialize,Debug)]
pub struct ProxyConfiguration {
  id:      String,
  proxies: Vec<StoredProxy>,
}

pub type Tag = String;

struct CommandServer {
  sock:            UnixListener,
  buffer_size:     usize,
  max_buffer_size: usize,
  conns:           Slab<CommandClient,FrontToken>,
  proxies:         HashMap<Token, (Tag, Proxy)>,
  proxy_count:     usize,
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

  fn new(srv: UnixListener, proxies_map: HashMap<String, Vec<Proxy>>, buffer_size: usize, max_buffer_size: usize, poll: Poll) -> CommandServer {
    //FIXME: verify this
    poll.register(&srv, Token(0), Ready::readable(), PollOpt::edge() | PollOpt::oneshot()).unwrap();

    let mut proxies = HashMap::new();

    let mut token_count = 0;
    for (tag, mut proxy_list) in proxies_map {
      for proxy in proxy_list.drain(..) {
        token_count += 1;
        poll.register(&proxy.channel.sock, Token(token_count), Ready::all(), PollOpt::edge()).unwrap();
        proxies.insert(Token(token_count), (tag.clone(), proxy));
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
      proxies:       proxies,
      proxy_count:  token_count,
      poll:            poll,
      timer:           timer,
    }
  }

  pub fn to_front(&self, token: Token) -> FrontToken {
    FrontToken(token.0 - self.proxy_count - 1)
  }

  pub fn from_front(&self, token: FrontToken) -> Token {
    Token(token.0 + self.proxy_count + 1)
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
      Token(i) if i < self.proxy_count + 1 => {
        info!("token({}) worker got event {:?}", i, events);
        //println!("proxies:\n{:?}", self.proxies);

        let mut messages = {
          let &mut (ref tag, ref mut proxy) =  self.proxies.get_mut(&Token(i)).unwrap();
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
          let &mut (ref tag, ref mut proxy) =  self.proxies.get_mut(&Token(i)).unwrap();
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
        client.write_message(&answer);
        client.remove_message_id(index);
      }
    }
  }

}

pub fn start(path: String,  proxies: HashMap<String, Vec<Proxy>>, saved_state: Option<String>, buffer_size: usize,
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
      let mut server = CommandServer::new(srv, proxies, buffer_size, max_buffer_size, event_loop);
      saved_state.as_ref().map(|state_path| {
        server.load_state("INITIALIZATION", state_path);
      });

      server.run();
      //event_loop.run(&mut CommandServer::new(srv, proxies, buffer_size, max_buffer_size)).unwrap()
    },
      Err(e) => {
        log!(log::LogLevel::Error, "could not create unix socket: {:?}", e);
      }
  }
}
