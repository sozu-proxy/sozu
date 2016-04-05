use mio::*;
use mio::unix::*;
use mio::util::Slab;
use std::path::PathBuf;
use std::io::{self,Read,Write,ErrorKind};
use std::iter::repeat;
use std::thread;
use std::str::from_utf8;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::collections::HashMap;
use std::sync::mpsc;
use log;
use rustc_serialize::json::{decode,encode};
use rustc_serialize::{Encodable,Decodable,Encoder,Decoder};
use nom::{IResult,HexDisplay};

use yxorp::network::{ProxyOrder,ServerMessage};
use yxorp::network::buffer::Buffer;
use yxorp::messages::Command;

use state::{HttpProxy,TlsProxy,ConfigState};

const SERVER: Token = Token(0);

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

pub struct Listener {
  pub tag:           String,
  pub listener_type: ListenerType,
  pub sender:        Sender<ProxyOrder>,
  pub receiver:      mpsc::Receiver<ServerMessage>,
  pub state:         ConfigState,
}

impl Listener {
  pub fn new(tag: String, listener_type: ListenerType, ip_address: String, port: u16, sender: Sender<ProxyOrder>, receiver: mpsc::Receiver<ServerMessage>) -> Listener {
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

impl Encodable for Listener {
  fn encode<E: Encoder>(&self, e: &mut E) -> Result<(), E::Error> {
    e.emit_map(3, |e| {
      try!(e.emit_map_elt_key(0, |e| "tag".encode(e)));
      try!(e.emit_map_elt_val(0, |e| self.tag.encode(e)));
      try!(e.emit_map_elt_key(1, |e| "listener_type".encode(e)));
      try!(e.emit_map_elt_val(1, |e| self.listener_type.encode(e)));
      try!(e.emit_map_elt_key(2, |e| "state".encode(e)));
      try!(e.emit_map_elt_val(2, |e| self.state.encode(e)));
      Ok(())
    })
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, RustcDecodable, RustcEncodable)]
pub struct ConfigMessage {
    pub id:       String,
    pub listener: String,
    pub command:  Command
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
  fn new(sock: UnixStream) -> CommandClient {
    CommandClient {
      sock:        sock,
      buf:         Buffer::with_capacity(1024),
      back_buf:    Buffer::with_capacity(10000),
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

  fn reregister(&self,  event_loop: &mut EventLoop<CommandServer>, tok: Token) {
    let mut interest = EventSet::hup();
    interest.insert(EventSet::readable());
    interest.insert(EventSet::writable());
    event_loop.register(&self.sock, tok, interest, PollOpt::edge() | PollOpt::oneshot());
  }

  fn conn_readable(&mut self, event_loop: &mut EventLoop<CommandServer>, tok: Token) -> Option<Vec<ConfigMessage>>{
    trace!("server conn readable; tok={:?}", tok);
    match self.sock.read(self.buf.space()) {
      Ok(0) => {
        self.reregister(event_loop, tok);
        return None;
      },
      Err(e) => {
        log!(log::LogLevel::Error, "UNIX CLIENT[{}] read error: {:?}", tok.as_usize(), e);
        return None;
      },
      Ok(r) => {
        self.buf.fill(r);
        debug!("UNIX CLIENT[{}] sent {} bytes: {:?}", tok.as_usize(), r, from_utf8(self.buf.data()));
      },
    };

    let mut res: Option<Vec<ConfigMessage>> = None;
    let mut offset = 0usize;
    match parse(self.buf.data()) {
      IResult::Incomplete(_) => {
        return Some(Vec::new());
      },
      IResult::Error(e)      => {
        log!(log::LogLevel::Error, "UNIX CLIENT[{}] read error: {:?}", tok.as_usize(), e);
        return None;
      },
      IResult::Done(i, v)    => {
        offset = self.buf.data().offset(i);
        res = Some(v);
      }
    }
    debug!("parsed {} bytes, result: {:?}", offset, res);
    self.buf.consume(offset);
    return res;
  }

  fn conn_writable(&mut self, event_loop: &mut EventLoop<CommandServer>, tok: Token) {
    if self.write_timeout.is_some() {
      trace!("server conn writable; tok={:?}; waiting for timeout", tok.as_usize());
      return;
    }

    trace!("server conn writable; tok={:?}", tok);
    match self.sock.write(self.back_buf.data()) {
      Ok(0) => {
        //println!("[{}] setting timeout!", tok.as_usize());
        self.write_timeout = event_loop.timeout_ms(self.token.unwrap().as_usize(), 700).ok();
      },
      Ok(r) => {
        self.back_buf.consume(r);
      },
      Err(e) => log!(log::LogLevel::Error,"UNIX CLIENT[{}] read error: {:?}", tok.as_usize(), e),
    }
    let mut interest = EventSet::hup();
    interest.insert(EventSet::readable());
    interest.insert(EventSet::writable());
    event_loop.register(&self.sock, tok, interest, PollOpt::edge() | PollOpt::oneshot());
  }
}

fn parse(input: &[u8]) -> IResult<&[u8], Vec<ConfigMessage>> {
  many0!(input,
    complete!(terminated!(map_res!(map_res!(is_not!("\0"), from_utf8), decode), char!('\0')))
  )
}

struct CommandServer {
  sock:  UnixListener,
  conns: Slab<CommandClient>,
  listeners: HashMap<String, Listener>,
}

impl CommandServer {
  fn accept(&mut self, event_loop: &mut EventLoop<CommandServer>) -> io::Result<()> {
    debug!("server accepting socket");

    let sock = self.sock.accept().unwrap().unwrap();
    let conn = CommandClient::new(sock);
    let tok = self.conns.insert(conn)
      .ok().expect("could not add connection to slab");

    // Register the connection
    self.conns[tok].token = Some(tok);
    let mut interest = EventSet::hup();
    interest.insert(EventSet::readable());
    interest.insert(EventSet::writable());
    event_loop.register(&self.conns[tok].sock, tok, interest, PollOpt::edge())
      .ok().expect("could not register socket with event loop");

    let accept_interest = EventSet::readable();
    event_loop.reregister(&self.sock, SERVER, accept_interest, PollOpt::edge());
    Ok(())
  }

  fn conn<'a>(&'a mut self, tok: Token) -> &'a mut CommandClient {
    &mut self.conns[tok]
  }

  fn dispatch(&mut self, token: Token, v: Vec<ConfigMessage>) {
    for message in &v {
      let command = message.command.clone();
      if let Some(ref mut listener) = self.listeners.get_mut (&message.listener) {
        if message.command == Command::DumpConfiguration {
          self.conns[token].back_buf.write(&encode(*listener).unwrap().into_bytes());
          self.conns[token].back_buf.write(&b"\0"[..]);
        } else {
          self.conns[token].add_message_id(message.id.clone());
          listener.state.handle_command(&command);
          listener.sender.send(ProxyOrder::Command(message.id.clone(), command));
        }
      } else {
        log!(log::LogLevel::Error, "no listener found for tag: {}", message.listener);
      }
    }
  }

  fn new(srv: UnixListener, listeners: HashMap<String, Listener>) -> CommandServer {
    CommandServer {
      sock:  srv,
      conns: Slab::new_starting_at(Token(1), 128),
      listeners: listeners,
    }
  }
}

impl Handler for CommandServer {
	type Timeout = usize;
	type Message = ();

	fn ready(&mut self, event_loop: &mut EventLoop<CommandServer>, token: Token, events: EventSet) {
    //trace!("ready: {:?} -> {:?}", token, events);
    if events.is_readable() {
      match token {
        SERVER => self.accept(event_loop).unwrap(),
        _      => {
          if self.conns.contains(token) {
            let opt_v = self.conns[token].conn_readable(event_loop, token);
            if let Some(v) = opt_v {
              self.dispatch(token, v);
            }
          }
        }
      };
    }

    if events.is_writable() {
      match token {
        SERVER => panic!("received writable for token 0"),
        _      => {
          if self.conns.contains(token) {
            self.conns[token].conn_writable(event_loop, token)
          }
        }
      };
    }

    if events.is_hup() {
      if self.conns.contains(token) {
        event_loop.deregister(&self.conns[token].sock);
        self.conns.remove(token);
        trace!("closed client [{}]", token.as_usize());
      }
    }
  }

  fn notify(&mut self, event_loop: &mut EventLoop<Self>, message: Self::Message) {
    // TODO: dispatch here to clients the list of messages you want to display
    info!("notify");
  }

  fn timeout(&mut self, event_loop: &mut EventLoop<Self>, timeout: Self::Timeout) {
    if timeout == 0 {
      for listener in self.listeners.values() {
        while let Ok(msg) = listener.receiver.try_recv() {
          //println!("got msg: {:?}", msg);
          for client in self.conns.iter_mut() {
            if let Some(index) = client.has_message_id(&msg.id) {
              client.back_buf.write(&encode(&msg).unwrap().into_bytes());
              client.back_buf.write(&b"\0"[..]);
              client.remove_message_id(index);
            }
          }
        }
      }
      event_loop.timeout_ms(0, 700);
    } else {
      let token = Token(timeout);
      if self.conns.contains(token) {
        //println!("[{}] timeout ended, registering for writes", timeout);
        self.conns[token].write_timeout = None;
        let mut interest = EventSet::hup();
        interest.insert(EventSet::readable());
        interest.insert(EventSet::writable());
        event_loop.register(&self.conns[token].sock, token, interest, PollOpt::edge() | PollOpt::oneshot());
      }
    }
  }

}

pub fn start(path: String, listeners: HashMap<String, Listener>) {
  thread::spawn(move || {
    let mut event_loop = EventLoop::new().unwrap();
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
        event_loop.register(&srv, SERVER, EventSet::readable(), PollOpt::edge() | PollOpt::oneshot()).unwrap();
        event_loop.timeout_ms(0, 700);

        event_loop.run(&mut CommandServer::new(srv, listeners)).unwrap()
      },
      Err(e) => {
        log!(log::LogLevel::Error, "could not create unix socket: {:?}", e);
      }
    }
  });
}

#[cfg(test)]
mod tests {
  use super::*;
  use rustc_serialize::json;
  use yxorp::messages::{Command,HttpFront};

  #[test]
  fn config_message_test() {
    let raw_json = r#"{ "id": "ID_TEST", "listener": "HTTP", "command":{"type": "ADD_HTTP_FRONT", "data": {"app_id": "xxx", "hostname": "yyy", "path_begin": "xxx", "port": 4242}} }"#;
    let config: ConfigMessage = json::decode(raw_json).unwrap();
    println!("{:?}", config);
    assert_eq!(config.listener, "HTTP");
    assert_eq!(config.command, Command::AddHttpFront(HttpFront{
      app_id: String::from("xxx"),
      hostname: String::from("yyy"),
      path_begin: String::from("xxx"),
      port: 4242
    }));
  }
}
