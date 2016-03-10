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
use log;
use rustc_serialize::json::decode;
use nom::{IResult,HexDisplay};

use yxorp::network::ProxyOrder;
use yxorp::network::buffer::Buffer;
use yxorp::messages::Command;

const SERVER: Token = Token(0);

#[derive(Debug,Clone,PartialEq,Eq,Hash, RustcDecodable, RustcEncodable)]
pub struct ConfigMessage {
    pub listener: String,
    pub command:  Command
}

struct CommandClient {
  sock:      UnixStream,
  buf:       Buffer,
  back_buf:  Buffer,
  token:     Option<Token>,
}

impl CommandClient {
  fn new(sock: UnixStream) -> CommandClient {
    CommandClient {
      sock:     sock,
      buf:      Buffer::with_capacity(1024),
      back_buf: Buffer::with_capacity(2048),
      token:    None,
    }
  }

  fn reregister(&self,  event_loop: &mut EventLoop<CommandServer>, tok: Token) {
    let mut interest = EventSet::hup();
    interest.insert(EventSet::readable());
    interest.insert(EventSet::writable());
    event_loop.register(&self.sock, tok, interest, PollOpt::level() | PollOpt::oneshot());
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
    trace!("server conn writable; tok={:?}", tok);
    match self.sock.write(self.back_buf.data()) {
      Ok(0) => {},
      Ok(r) => {
        self.back_buf.consume(r);
      },
      Err(e) => log!(log::LogLevel::Error,"UNIX CLIENT[{}] read error: {:?}", tok.as_usize(), e),
    }
    let mut interest = EventSet::hup();
    interest.insert(EventSet::readable());
    interest.insert(EventSet::writable());
    event_loop.register(&self.sock, tok, interest, PollOpt::level() | PollOpt::oneshot());
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
  listeners: HashMap<String, Sender<ProxyOrder>>,
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
    event_loop.register(&self.conns[tok].sock, tok, interest, PollOpt::level())
      .ok().expect("could not register socket with event loop");

    let accept_interest = EventSet::readable();
    event_loop.reregister(&self.sock, SERVER, accept_interest, PollOpt::level());
    Ok(())
  }

  fn conn<'a>(&'a mut self, tok: Token) -> &'a mut CommandClient {
    &mut self.conns[tok]
  }

  fn dispatch(&self, v: Vec<ConfigMessage>) {
    for message in &v {
      let command = message.command.clone();
      if let Some(ref listener) = self.listeners.get(&message.listener) {
        listener.send(ProxyOrder::Command(command));
      } else {
        log!(log::LogLevel::Error, "no listener found for tag: {}", message.listener);
      }
    }
  }

  fn new(srv: UnixListener, listeners: HashMap<String, Sender<ProxyOrder>>) -> CommandServer {
    let mut v = Vec::with_capacity(2048);
    v.extend(repeat(0).take(2048));
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
              self.dispatch(v);
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
}

pub fn start(folder: String, listeners: HashMap<String, Sender<ProxyOrder>>) {
  thread::spawn(move || {
    let mut event_loop = EventLoop::new().unwrap();
    let addr = PathBuf::from(folder).join(&PathBuf::from("./sock"));
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
        event_loop.register(&srv, SERVER, EventSet::readable(), PollOpt::level() | PollOpt::oneshot()).unwrap();

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
    let raw_json = r#"{ "listener": "HTTP", "command":{"type": "ADD_HTTP_FRONT", "data": {"app_id": "xxx", "hostname": "yyy", "path_begin": "xxx", "port": 4242}} }"#;
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
