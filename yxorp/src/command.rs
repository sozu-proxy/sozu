use mio::*;
use mio::unix::*;
use bytes::{Buf, ByteBuf, MutByteBuf, SliceBuf};
use mio::util::Slab;
use std::path::PathBuf;
use std::io::{self,Read,Write};
use std::iter::repeat;
use std::thread;
use std::str::from_utf8;

use yxorp::network::buffer::Buffer;

const SERVER: Token = Token(0);
const CLIENT: Token = Token(1);

struct CommandClient {
  sock:     UnixStream,
  buf:      Buffer,
  back_buf: Buffer,
  token:    Option<Token>,
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

  fn conn_readable(&mut self, event_loop: &mut EventLoop<CommandServer>, tok: Token) {
    //debug!("server conn readable; tok={:?}", tok);
    match self.sock.read(self.buf.space()) {
      Ok(0) => {},
      Ok(r) => {
        self.buf.fill(r);
        debug!("UNIX CLIENT[{}] sent {} bytes: {:?}", tok.as_usize(), r, from_utf8(self.buf.data()));
      },
      Err(e) => error!("UNIX CLIENT[{}] read error: {:?}", tok.as_usize(), e),
    }
    let mut interest = EventSet::hup();
    interest.insert(EventSet::readable());
    interest.insert(EventSet::writable());
    event_loop.register(&self.sock, tok, interest, PollOpt::level() | PollOpt::oneshot());

    //TODO: return here the list of messages to transmit
  }

  fn conn_writable(&mut self, event_loop: &mut EventLoop<CommandServer>, tok: Token) {
    //debug!("server conn writable; tok={:?}", tok);
    match self.sock.write(self.back_buf.data()) {
      Ok(0) => {},
      Ok(r) => {
        self.back_buf.consume(r);
      },
      Err(e) => error!("UNIX CLIENT[{}] read error: {:?}", tok.as_usize(), e),
    }
    let mut interest = EventSet::hup();
    interest.insert(EventSet::readable());
    interest.insert(EventSet::writable());
    event_loop.register(&self.sock, tok, interest, PollOpt::level() | PollOpt::oneshot());
  }
}

struct CommandServer {
  sock:  UnixListener,
  conns: Slab<CommandClient>
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
    event_loop.register(&self.conns[tok].sock, tok, interest, PollOpt::level() | PollOpt::oneshot())
      .ok().expect("could not register socket with event loop");

    let mut accept_interest = EventSet::readable();
    event_loop.register(&self.sock, SERVER, accept_interest, PollOpt::level() | PollOpt::oneshot());
    Ok(())
  }

  fn conn<'a>(&'a mut self, tok: Token) -> &'a mut CommandClient {
    &mut self.conns[tok]
  }

  fn new(srv: UnixListener) -> CommandServer {
    let mut v = Vec::with_capacity(2048);
    v.extend(repeat(0).take(2048));
    CommandServer {
      sock:  srv,
      conns: Slab::new_starting_at(Token(1), 128)
    }
  }
}

impl Handler for CommandServer {
	type Timeout = usize;
	type Message = ();

	fn ready(&mut self, event_loop: &mut EventLoop<CommandServer>, token: Token, events: EventSet) {
    //trace!("ready: {:?} -> {:?}", token, events);
    if events.is_hup() {
      if self.conns.contains(token) {
        event_loop.deregister(&self.conns[token].sock);
        self.conns.remove(token);
        trace!("closed client [{}]", token.as_usize());
      }
    }

    if events.is_readable() {
      match token {
        SERVER => self.accept(event_loop).unwrap(),
        _      => {
          if self.conns.contains(token) {
            self.conns[token].conn_readable(event_loop, token)
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
  }

  fn notify(&mut self, event_loop: &mut EventLoop<Self>, message: Self::Message) {
    // TODO: dispatch here to clients the list of messages you want to display
    info!("notify");
  }
}

pub fn start() {
  thread::spawn(move || {
    let mut event_loop = EventLoop::new().unwrap();
    let addr = PathBuf::from("./sock");
println!("aaa");
    let srv = UnixListener::bind(&addr).unwrap();
  println!("bbb");

    info!("listen for connections");
    event_loop.register(&srv, SERVER, EventSet::readable(), PollOpt::level() | PollOpt::oneshot()).unwrap();

println!("ccc");
    event_loop.run(&mut CommandServer::new(srv)).unwrap()
  });
}
