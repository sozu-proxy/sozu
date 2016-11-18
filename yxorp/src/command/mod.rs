use mio::*;
use mio::deprecated::unix::*;
use mio::timer::{Timer,Timeout};
use slab::Slab;
use std::path::PathBuf;
use std::io::{self,BufRead,BufReader,Read,Write,ErrorKind};
use std::str::from_utf8;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::collections::HashMap;
use std::sync::mpsc;
use std::cmp::min;
use log;
use nom::{IResult,HexDisplay};
use serde;
use serde_json;
use serde_json::from_str;
use rustc_serialize::{Decodable,Decoder,Encodable,Encoder};
use std::time::Duration;

use yxorp::network::{ProxyOrder,ServerMessage};
use yxorp::network::buffer::Buffer;
use yxorp::messages::Command;

use state::{HttpProxy,TlsProxy,ConfigState};

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

#[derive(Debug,Clone,Copy,PartialEq,Eq,Hash)]
pub enum ListenerType {
  HTTP,
  HTTPS,
  TCP
}

impl Encodable for ListenerType {
  fn encode<E: Encoder>(&self, e: &mut E) -> Result<(), E::Error> {
    match *self {
      ListenerType::HTTP  => e.emit_str("HTTP"),
      ListenerType::HTTPS => e.emit_str("HTTPS"),
      ListenerType::TCP   => e.emit_str("TCP"),
    }
  }
}

impl Decodable for ListenerType {
  fn decode<D: Decoder>(decoder: &mut D) -> Result<ListenerType, D::Error> {
    let tag = try!(decoder.read_str());
    match &tag[..] {
      "HTTP"  => Ok(ListenerType::HTTP),
      "HTTPS" => Ok(ListenerType::HTTPS),
      "TCP"   => Ok(ListenerType::TCP),
      _       => Err(decoder.error("unrecognized listener type"))
    }
  }
}

impl serde::Serialize for ListenerType {
  fn serialize<S>(&self, serializer: &mut S) -> Result<(), S::Error>
      where S: serde::Serializer,
  {
    match *self {
      ListenerType::HTTP  => serializer.serialize_str("HTTP"),
      ListenerType::HTTPS => serializer.serialize_str("HTTPS"),
      ListenerType::TCP   => serializer.serialize_str("TCP"),
    }
  }
}

impl serde::Deserialize for ListenerType {
  fn deserialize<D>(deserializer: &mut D) -> Result<ListenerType, D::Error>
    where D: serde::de::Deserializer
  {
    struct ListenerTypeVisitor;

    impl serde::de::Visitor for ListenerTypeVisitor {
      type Value = ListenerType;

      fn visit_str<E>(&mut self, value: &str) -> Result<ListenerType, E>
        where E: serde::de::Error
        {
          match value {
            "HTTP"  => Ok(ListenerType::HTTP),
            "HTTPS" => Ok(ListenerType::HTTPS),
            "TCP"   => Ok(ListenerType::TCP),
            _ => Err(serde::de::Error::custom("expected HTTP, HTTPS or TCP listener type")),
          }
        }
    }

    deserializer.deserialize(ListenerTypeVisitor)
  }
}


#[derive(Debug,Clone,PartialEq,Eq)]
pub struct ListenerDeserializer {
  pub tag:   String,
  pub state: ConfigState,
}

enum ListenerDeserializerField {
  Tag,
  Type,
  State,
}

impl serde::Deserialize for ListenerDeserializerField {
  fn deserialize<D>(deserializer: &mut D) -> Result<ListenerDeserializerField, D::Error>
        where D: serde::de::Deserializer {
    struct ListenerDeserializerFieldVisitor;
    impl serde::de::Visitor for ListenerDeserializerFieldVisitor {
      type Value = ListenerDeserializerField;

      fn visit_str<E>(&mut self, value: &str) -> Result<ListenerDeserializerField, E>
        where E: serde::de::Error {
        match value {
          "tag"           => Ok(ListenerDeserializerField::Tag),
          "listener_type" => Ok(ListenerDeserializerField::Type),
          "state"         => Ok(ListenerDeserializerField::State),
          _ => Err(serde::de::Error::custom("expected tag, listener_type or state")),
        }
      }
    }

    deserializer.deserialize(ListenerDeserializerFieldVisitor)
  }
}

struct ListenerDeserializerVisitor;
impl serde::de::Visitor for ListenerDeserializerVisitor {
  type Value = ListenerDeserializer;

  fn visit_map<V>(&mut self, mut visitor: V) -> Result<ListenerDeserializer, V::Error>
        where V: serde::de::MapVisitor {
    let mut tag:Option<String>                 = None;
    let mut listener_type:Option<ListenerType> = None;
    let mut state:Option<serde_json::Value>    = None;

    loop {
      match try!(visitor.visit_key()) {
        Some(ListenerDeserializerField::Type)  => { listener_type = Some(try!(visitor.visit_value())); }
        Some(ListenerDeserializerField::Tag)   => { tag = Some(try!(visitor.visit_value())); }
        Some(ListenerDeserializerField::State) => { state = Some(try!(visitor.visit_value())); }
        None => { break; }
      }
    }

    println!("decoded type = {:?}, value= {:?}", listener_type, state);
    let listener_type = match listener_type {
      Some(listener) => listener,
      None => try!(visitor.missing_field("listener_type")),
    };
    let tag = match tag {
      Some(tag) => tag,
      None => try!(visitor.missing_field("tag")),
    };
    let state = match state {
      Some(state) => state,
      None => try!(visitor.missing_field("state")),
    };

    try!(visitor.end());

    let state = match listener_type {
      ListenerType::HTTP => {
        let http_proxy: HttpProxy = try!(serde_json::from_value(state).or(Err(serde::de::Error::custom("http_proxy"))));
        ConfigState::Http(http_proxy)
      },
      ListenerType::HTTPS => {
        let tls_proxy: TlsProxy = try!(serde_json::from_value(state).or(Err(serde::de::Error::custom("tls_proxy"))));
        ConfigState::Tls(tls_proxy)
      },
      ListenerType::TCP => {
        ConfigState::Tcp
      }
    };

    Ok(ListenerDeserializer {
      tag: tag,
      state: state,
    })
  }
}

impl serde::Deserialize for ListenerDeserializer {
  fn deserialize<D>(deserializer: &mut D) -> Result<ListenerDeserializer, D::Error>
        where D: serde::de::Deserializer {
    static FIELDS: &'static [&'static str] = &["tag", "listener_type", "state"];
    deserializer.deserialize_struct("Listener", FIELDS, ListenerDeserializerVisitor)
  }
}

#[derive(Serialize)]
pub struct Listener {
  pub tag:           String,
  pub listener_type: ListenerType,
  #[serde(skip_serializing)]
  pub sender:        channel::Sender<ProxyOrder>,
  #[serde(skip_serializing)]
  pub receiver:      mpsc::Receiver<ServerMessage>,
  pub state:         ConfigState,
}

impl Listener {
  pub fn new(tag: String, listener_type: ListenerType, ip_address: String, port: u16, sender: channel::Sender<ProxyOrder>, receiver: mpsc::Receiver<ServerMessage>) -> Listener {
    let state = match listener_type {
      ListenerType::HTTP  => ConfigState::Http(HttpProxy::new(ip_address, port)),
      ListenerType::HTTPS => ConfigState::Tls(TlsProxy::new(ip_address, port)),
      _                   => unimplemented!(),
    };

    Listener {
      tag:           tag,
      listener_type: listener_type,
      sender:        sender,
      receiver:      receiver,
      state:         state,
    }
  }
}

#[derive(Serialize)]
pub struct ListenerConfiguration<'a> {
  id:       String,
  listeners: &'a Vec<Listener>,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize)]
pub enum ConfigCommand {
  ProxyConfiguration(Command),
  SaveState(String),
  DumpState,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize)]
pub struct ConfigMessage {
  pub id:       String,
  pub data:     ConfigCommand,
  pub listener: Option<String>,
}

#[derive(Deserialize)]
struct SaveStateData {
  path : String
}

enum ConfigMessageField {
  Id,
  Listener,
  Type,
  Data,
}


impl serde::Deserialize for ConfigMessageField {
  fn deserialize<D>(deserializer: &mut D) -> Result<ConfigMessageField, D::Error>
        where D: serde::de::Deserializer {
    struct ConfigMessageFieldVisitor;
    impl serde::de::Visitor for ConfigMessageFieldVisitor {
      type Value = ConfigMessageField;

      fn visit_str<E>(&mut self, value: &str) -> Result<ConfigMessageField, E>
        where E: serde::de::Error {
        match value {
          "id"       => Ok(ConfigMessageField::Id),
          "type"     => Ok(ConfigMessageField::Type),
          "listener" => Ok(ConfigMessageField::Listener),
          "data"     => Ok(ConfigMessageField::Data),
          _ => Err(serde::de::Error::custom("expected id, listener, type or data")),
        }
      }
    }

    deserializer.deserialize(ConfigMessageFieldVisitor)
  }
}

struct ConfigMessageVisitor;
impl serde::de::Visitor for ConfigMessageVisitor {
  type Value = ConfigMessage;

  fn visit_map<V>(&mut self, mut visitor: V) -> Result<ConfigMessage, V::Error>
        where V: serde::de::MapVisitor {
    let mut id:Option<String>              = None;
    let mut listener: Option<String>       = None;
    let mut config_type:Option<String>     = None;
    let mut data:Option<serde_json::Value> = None;

    loop {
      match try!(visitor.visit_key()) {
        Some(ConfigMessageField::Type)     => { config_type = Some(try!(visitor.visit_value())); }
        Some(ConfigMessageField::Id)       => { id = Some(try!(visitor.visit_value())); }
        Some(ConfigMessageField::Listener) => { listener = Some(try!(visitor.visit_value())); }
        Some(ConfigMessageField::Data)     => { data = Some(try!(visitor.visit_value())); }
        None => { break; }
      }
    }

    //println!("decoded type = {:?}, value= {:?}", listener_type, state);
    let config_type = match config_type {
      Some(config) => config,
      None => try!(visitor.missing_field("type")),
    };
    let id = match id {
      Some(id) => id,
      None => try!(visitor.missing_field("id")),
    };

    try!(visitor.end());

    let data = if &config_type == "PROXY" {
      let data = match data {
        Some(data) => data,
        None => try!(visitor.missing_field("data")),
      };
      let command = try!(serde_json::from_value(data).or(Err(serde::de::Error::custom("proxy configuration command"))));
      ConfigCommand::ProxyConfiguration(command)
    } else if &config_type == &"SAVE_STATE" {
      let data = match data {
        Some(data) => data,
        None => try!(visitor.missing_field("data")),
      };
      let state: SaveStateData = try!(serde_json::from_value(data).or(Err(serde::de::Error::custom("save state"))));
      ConfigCommand::SaveState(state.path)
    } else if &config_type == &"DUMP_STATE" {
      ConfigCommand::DumpState
    } else {
      return Err(serde::de::Error::custom("unrecognized command"));
    };

    Ok(ConfigMessage {
      id:       id,
      data:     data,
      listener: listener
    })
  }
}

impl serde::Deserialize for ConfigMessage {
  fn deserialize<D>(deserializer: &mut D) -> Result<ConfigMessage, D::Error>
    where D: serde::de::Deserializer {
    static FIELDS: &'static [&'static str] = &["id", "listener", "type", "data"];
    deserializer.deserialize_struct("ConfigMessage", FIELDS, ConfigMessageVisitor)
  }
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

  fn conn<'a>(&'a mut self, tok: FrontToken) -> &'a mut CommandClient {
    &mut self.conns[tok]
  }

  fn dispatch(&mut self, token: FrontToken, v: Vec<ConfigMessage>) {
    for message in &v {
      let config_command = message.data.clone();
      match config_command {
        ConfigCommand::SaveState(path) => {
          if let Ok(mut f) = fs::File::create(&path) {
            for listener in self.listeners.values() {
              if let Ok(()) = f.write_all(&serde_json::to_string(&listener).map(|s| s.into_bytes()).unwrap_or(vec!())) {
                f.write(&b"\n"[..]);
                f.sync_all();
              }
            }
            // FIXME: should send back a DONE message here
          } else {
            // FIXME: should send back error here
            log!(log::LogLevel::Error, "could not open file: {}", &path);
          }
        },
        ConfigCommand::DumpState => {
          //FIXME:
          let v: &Vec<Listener> = self.listeners.values().next().unwrap();
          let conf = ListenerConfiguration {
            id: message.id.clone(),
            listeners: v,
          };
          let encoded = serde_json::to_string(&conf).map(|s| s.into_bytes()).unwrap_or(vec!());
          if self.conns[token].back_buf.grow(min(encoded.len() + 10, self.max_buffer_size)) {
            log!(log::LogLevel::Info, "write buffer was not large enough, growing to {} bytes", encoded.len());
          }
          self.conns[token].back_buf.write(&encoded);
          self.conns[token].back_buf.write(&b"\0"[..]);
        },
        ConfigCommand::ProxyConfiguration(command) => {
          if let Some(ref tag) = message.listener {
            if let &Command::AddTlsFront(ref data) = &command {
              log!(log::LogLevel::Info, "received AddTlsFront(TlsFront {{ app_id: {}, hostname: {}, path_begin: {} }}) with tag {:?}",
                data.app_id, data.hostname, data.path_begin, tag);
            } else {
              log!(log::LogLevel::Info, "received {:?} with tag {:?}", command, tag);
            }
            if let Some(ref mut listener_vec) = self.listeners.get_mut (tag) {
              for listener in listener_vec.iter_mut() {
                let cl = command.clone();
                self.conns[token].add_message_id(message.id.clone());
                listener.state.handle_command(&cl);
                listener.sender.send(ProxyOrder::Command(message.id.clone(), cl));
              }
            } else {
              // FIXME: should send back error here
              log!(log::LogLevel::Error, "no listener found for tag: {}", tag);
            }
          } else {
            // FIXME: should send back error here
            log!(log::LogLevel::Error, "expecting listener tag");
          }
        }
      }
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

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json;
  use yxorp::messages::{Command,HttpFront};

  #[test]
  fn config_message_test() {
    let raw_json = r#"{ "id": "ID_TEST", "type": "PROXY", "listener": "HTTP", "data":{"type": "ADD_HTTP_FRONT", "data": {"app_id": "xxx", "hostname": "yyy", "path_begin": "xxx"}} }"#;
    let message: ConfigMessage = serde_json::from_str(raw_json).unwrap();
    println!("{:?}", message);
    assert_eq!(message.listener, Some(String::from("HTTP")));
    assert_eq!(message.data, ConfigCommand::ProxyConfiguration(Command::AddHttpFront(HttpFront{
      app_id: String::from("xxx"),
      hostname: String::from("yyy"),
      path_begin: String::from("xxx"),
    })));
  }

  #[test]
  fn save_state_test() {
    let raw_json = r#"{ "id": "ID_TEST", "type": "SAVE_STATE", "data":{ "path": "./config_dump.json"} }"#;
    let message: ConfigMessage = serde_json::from_str(raw_json).unwrap();
    println!("{:?}", message);
    assert_eq!(message.listener, None);
    assert_eq!(message.data, ConfigCommand::SaveState(String::from("./config_dump.json")));
  }

  #[test]
  fn dump_state_test() {
    println!("A");
    //let raw_json = r#"{ "id": "ID_TEST", "type": "DUMP_STATE" }"#;
    let raw_json = "{ \"id\": \"ID_TEST\", \"type\": \"DUMP_STATE\" }";
    println!("B");
    let message: ConfigMessage = serde_json::from_str(raw_json).unwrap();
    println!("C");
    println!("{:?}", message);
    assert_eq!(message.listener, None);
    println!("D");
    assert_eq!(message.data, ConfigCommand::DumpState);
  }
}
