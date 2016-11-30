use mio::*;
use mio::deprecated::unix::*;
use mio::timer::{Timer,Timeout};
use slab::Slab;
use std::fs;
use std::fmt;
use std::path::PathBuf;
use std::io::{self,BufRead,BufReader,Read,Write,ErrorKind};
use std::str::from_utf8;
use std::os::unix::fs::PermissionsExt;
use std::collections::HashMap;
use std::sync::mpsc;
use std::cmp::min;
use std::time::Duration;
use log;
use nom::{IResult,HexDisplay,Offset};
use serde_json;
use serde_json::from_str;

use yxorp::network::{ProxyOrder,ServerMessage};
use yxorp::network::buffer::Buffer;

use state::{HttpProxy,TlsProxy,ConfigState};

pub mod data;
pub mod orders;
use self::data::{ConfigMessage,ListenerDeserializer,ListenerType};

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
  //#[serde(skip_serializing)]
  pub sender:        channel::Sender<ProxyOrder>,
  //#[serde(skip_serializing)]
  pub receiver:      mpsc::Receiver<ServerMessage>,
  pub state:         ConfigState,
}

impl Listener {
  pub fn new(tag: String, id: u8, listener_type: ListenerType, ip_address: String, port: u16, sender: channel::Sender<ProxyOrder>, receiver: mpsc::Receiver<ServerMessage>) -> Listener {
    let state = match listener_type {
      ListenerType::HTTP  => ConfigState::Http(HttpProxy::new(ip_address, port)),
      ListenerType::HTTPS => ConfigState::Tls(TlsProxy::new(ip_address, port)),
      _                   => unimplemented!(),
    };

    Listener {
      tag:           tag,
      id:            id,
      listener_type: listener_type,
      sender:        sender,
      receiver:      receiver,
      state:         state,
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

#[derive(Debug,PartialEq)]
pub enum ConnReadError {
  Continue,
  FullBufferError,
  ParseError,
  SocketError,
}

struct CommandClient {
  sock:        UnixStream,
  buf:         Buffer,
  back_buf:    Buffer,
  token:       Option<Token>,
  message_ids: Vec<String>,
  write_timeout: Option<Timeout>,
}

impl CommandClient {
  fn new(sock: UnixStream, buffer_size: usize) -> CommandClient {
    CommandClient {
      sock:        sock,
      buf:         Buffer::with_capacity(buffer_size),
      back_buf:    Buffer::with_capacity(buffer_size),
      token:       None,
      message_ids: Vec::new(),
      write_timeout: None,
    }
  }

  fn add_message_id(&mut self, id: String) {
    self.message_ids.push(id);
    self.message_ids.sort();
  }

  fn has_message_id(&self, id: &String) ->Option<usize> {
    self.message_ids.binary_search(&id).ok()
  }

  fn remove_message_id(&mut self, index: usize) {
    self.message_ids.remove(index);
  }

  fn conn_readable(&mut self, tok: Token) -> Result<Vec<ConfigMessage>,ConnReadError>{
    trace!("server conn readable; tok={:?}", tok);
    loop {
      let size = self.buf.available_space();
      if size == 0 { break; }

      match self.sock.read(self.buf.space()) {
        Ok(0) => {
          //self.reregister(event_loop, tok);
          break;
          //return None;
        },
        Err(e) => {
          match e.kind() {
            ErrorKind::WouldBlock => {
              break;
            },
            code => {
              log!(log::LogLevel::Error, "UNIX CLIENT[{}] read error (kind: {:?}): {:?}", tok.0, code, e);
              return Err(ConnReadError::SocketError);
            }
          }
        },
        Ok(r) => {
          self.buf.fill(r);
          debug!("UNIX CLIENT[{}] sent {} bytes: {:?}", tok.0, r, from_utf8(self.buf.data()));
        },
      };
    }

    let mut res: Result<Vec<ConfigMessage>,ConnReadError> = Err(ConnReadError::Continue);
    let mut offset = 0usize;
    match parse(self.buf.data()) {
      IResult::Incomplete(_) => {
        if self.buf.available_space() == 0 {
          log!(log::LogLevel::Error, "UNIX CLIENT[{}] buffer full, but not enough data: {:?}", tok.0, from_utf8(self.buf.data()));
          return Err(ConnReadError::FullBufferError);
        }
        return Ok(Vec::new());
      },
      IResult::Error(e)      => {
        log!(log::LogLevel::Error, "UNIX CLIENT[{}] read error: {:?}", tok.0, e);
        return Err(ConnReadError::ParseError);
      },
      IResult::Done(i, v)    => {
        if self.buf.available_data() == i.len() && v.len() == 0 {
          if self.buf.available_space() == 0 {
            log!(log::LogLevel::Error, "UNIX CLIENT[{}] buffer full, cannot parse", tok.0);
            return Err(ConnReadError::FullBufferError);
          } else {
            log!(log::LogLevel::Error, "UNIX CLIENT[{}] parse error", tok.0);
            return Err(ConnReadError::ParseError);
          }
        }
        offset = self.buf.data().offset(i);
        res = Ok(v);
      }
    }
    debug!("parsed {} bytes, result: {:?}", offset, res);
    self.buf.consume(offset);
    return res;
  }

  fn conn_writable(&mut self, tok: Token) {
    if self.write_timeout.is_some() {
      trace!("server conn writable; tok={:?}; waiting for timeout", tok.0);
      return;
    }

    trace!("server conn writable; tok={:?}", tok);
    loop {
      let size = self.back_buf.available_data();
      if size == 0 { break; }

      match self.sock.write(self.back_buf.data()) {
        Ok(0) => {
          //println!("[{}] setting timeout!", tok.0);
          /*FIXME: timeout
          self.write_timeout = event_loop.timeout_ms(self.token.unwrap().0, 700).ok();
          */
          break;
        },
        Ok(r) => {
          self.back_buf.consume(r);
        },
        Err(e) => {
          match e.kind() {
            ErrorKind::WouldBlock => {
              break;
            },
            code => {
              log!(log::LogLevel::Error,"UNIX CLIENT[{}] write error: (kind: {:?}): {:?}", tok.0, code, e);
              return;
            }
          }
        }
      }
    }

    //let mut interest = Ready::hup();
    //interest.insert(Ready::readable());
    //interest.insert(Ready::writable());
    //event_loop.register(&self.sock, tok, interest, PollOpt::edge() | PollOpt::oneshot());
  }
}

fn parse(input: &[u8]) -> IResult<&[u8], Vec<ConfigMessage>> {
  many0!(input,
    complete!(terminated!(map_res!(map_res!(is_not!("\0"), from_utf8), from_str), char!('\0')))
  )
}

struct CommandServer {
  sock:            UnixListener,
  buffer_size:     usize,
  max_buffer_size: usize,
  conns:           Slab<CommandClient,FrontToken>,
  listeners:       HashMap<String, Vec<Listener>>,
  pub poll:        Poll,
  timer:           Timer<Token>,
}

impl CommandServer {
  fn accept(&mut self) -> io::Result<()> {
    debug!("server accepting socket");

    let acc = self.sock.accept();
    if let Ok(sock) = acc {
      let conn = CommandClient::new(sock, self.buffer_size);
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
      self.handle_message(token, message);
    }
  }

  fn new(srv: UnixListener, listeners: HashMap<String, Vec<Listener>>, buffer_size: usize, max_buffer_size: usize, poll: Poll) -> CommandServer {
    //FIXME: verify this
    poll.register(&srv, Token(0), Ready::readable(), PollOpt::edge() | PollOpt::oneshot()).unwrap();
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
      poll:            poll,
      timer:           timer,
    }
  }

  pub fn to_front(&self, token: Token) -> FrontToken {
    FrontToken(token.0 - 2)
  }

  pub fn from_front(&self, token: FrontToken) -> Token {
    Token(token.0 + 2)
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

      //FIXME: manually call the timer instead of relying on a separate thread
      while let Some(timeout_token) = self.timer.poll() {
        self.timeout(timeout_token);
      }
    }
  }

  fn ready(&mut self, token: Token, events: Ready) {
    //trace!("ready: {:?} -> {:?}", token, events);
    if events.is_readable() {
      match token {
        Token(0) => self.accept().unwrap(),
        Token(1) => {
          while let Some(timeout_token) = self.timer.poll() {
            self.timeout(timeout_token);
          }
        },
        _      => {
          let conn_token = self.to_front(token);
          if self.conns.contains(conn_token) {
            let opt_v = self.conns[conn_token].conn_readable(token);
            match opt_v {
              Ok(v) => self.dispatch(conn_token, v),
              Err(ConnReadError::Continue) => {},
              Err(ConnReadError::ParseError) | Err(ConnReadError::SocketError) => {
                self.poll.deregister(&self.conns[conn_token].sock);
                self.conns.remove(conn_token);
                trace!("closed client [{}]", token.0);
              }
              Err(ConnReadError::FullBufferError) => {
                //FIXME: automatically growing by 5kB every time may not be the best idea
                let new_size = min(self.conns[conn_token].buf.capacity()+5000, self.max_buffer_size);
                log!(log::LogLevel::Error, "buffer not large enough, growing to {}", new_size);
                self.conns[conn_token].buf.grow(new_size);
              }
            }
          }
        }
      };
    }

    if events.is_writable() {
      match token {
        Token(0) => panic!("received writable for token 0"),
        Token(1) => panic!("received writable for token 1"),
        _      => {
          let conn_token = self.to_front(token);
          if self.conns.contains(conn_token) {
            if let Some(ref timeout) = self.conns[conn_token].write_timeout {
              self.timer.cancel_timeout(&timeout);
            }
            let res = self.conns[conn_token].conn_writable(token);
            if let Ok(timeout) = self.timer.set_timeout(Duration::from_millis(700), token) {
              self.conns[conn_token].write_timeout = Some(timeout);
            }
            res
          }
        }
      };
    }

    if events.is_hup() {
      let conn_token = self.to_front(token);
      if self.conns.contains(conn_token) {
        self.poll.deregister(&self.conns[conn_token].sock);
        self.conns.remove(conn_token);
        trace!("closed client [{}]", token.0);
      }
    }
  }

  fn timeout(&mut self, token: Token) {
    if token.0 == 0 {
      for listener_vec in self.listeners.values() {
        for listener in listener_vec {
          while let Ok(msg) = listener.receiver.try_recv() {
            //println!("got msg: {:?}", msg);
            for client in self.conns.iter_mut() {
              if let Some(index) = client.has_message_id(&msg.id) {
                client.back_buf.write(&serde_json::to_string(&msg).map(|s| s.into_bytes()).unwrap_or(vec!()));
                client.back_buf.write(&b"\0"[..]);
                client.remove_message_id(index);
              }
            }
          }
        }
      }
      self.timer.set_timeout(Duration::from_millis(700), Token(0));
    } else {
      let conn_token = self.to_front(token);
      if self.conns.contains(conn_token) {
        //FIXME: is it still needed?
        //println!("[{}] timeout ended, registering for writes", timeout);
        self.conns[conn_token].write_timeout = None;
        //let mut interest = Ready::hup();
        //interest.insert(Ready::readable());
        //interest.insert(Ready::writable());
        //event_loop.register(&self.conns[token].sock, token, interest, PollOpt::edge() | PollOpt::oneshot());
      }
    }
  }

}

pub fn start(path: String, mut listeners: HashMap<String, Vec<Listener>>, saved_state: Option<String>, buffer_size: usize,
    max_buffer_size: usize) {
  saved_state.as_ref().map(|state_path| {
    fs::File::open(state_path).map(|f| {
      let reader = BufReader::new(f);
      reader.lines().map(|line_res| {
        line_res.map(|line| {
          if let Ok(listener_state) = from_str::<ListenerDeserializer>(&line) {
            listeners.get_mut(&listener_state.tag).as_mut().map(|listener_vec| {
              for listener in listener_vec.iter_mut() {
                println!("setting listener {} state at {:?}", listener_state.tag, listener_state.state);
                listener.state = listener_state.state.clone();
              }
            });
          }
        })
      }).count();

    });
  });

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
      server.run();
      //event_loop.run(&mut CommandServer::new(srv, listeners, buffer_size, max_buffer_size)).unwrap()
    },
      Err(e) => {
        log!(log::LogLevel::Error, "could not create unix socket: {:?}", e);
      }
  }
}
