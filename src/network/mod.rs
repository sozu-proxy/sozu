use std::thread::{self,Thread,Builder};
use std::sync::mpsc::{channel,Receiver};
use mio::tcp::*;
use mio::*;
use mio::buf::{ByteBuf,MutByteBuf};
use std::collections::HashMap;
use std::io::{self,Read,ErrorKind};
use nom::HexDisplay;
use std::error::Error;
use mio::util::Slab;
use std::net::SocketAddr;
use std::str::FromStr;
use time::precise_time_s;

const SERVER: Token = Token(0);

struct Client {
  sock:           TcpStream,
  backend:        TcpStream,
  front_buf:      Option<ByteBuf>,
  front_mut_buf:  Option<MutByteBuf>,
  back_buf:       Option<ByteBuf>,
  back_mut_buf:   Option<MutByteBuf>,
  token:          Option<Token>,
  backend_token:  Option<Token>,
  back_interest:  EventSet,
  front_interest: EventSet
}

impl Client {
  fn new(sock: TcpStream, backend: TcpStream) -> Client {
    Client {
      sock:           sock,
      backend:        backend,
      front_buf:      None,
      front_mut_buf:  Some(ByteBuf::mut_with_capacity(2048)),
      back_buf:       None,
      back_mut_buf:   Some(ByteBuf::mut_with_capacity(2048)),
      token:          None,
      backend_token:  None,
      back_interest:  EventSet::all(),
      front_interest: EventSet::all()
    }
  }

  pub fn set_tokens(&mut self, token: Token, backend: Token) {
    self.token         = Some(token);
    self.backend_token = Some(backend);
  }

  fn writable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
    //println!("in writable()");
    if let Some(mut buf) = self.back_buf.take() {
      //println!("in writable 2");

      match self.sock.try_write_buf(&mut buf) {
        Ok(None) => {
          println!("client flushing buf; WOULDBLOCK");

          self.back_buf = Some(buf);
          self.front_interest.insert(EventSet::writable());
        }
        Ok(Some(r)) => {
          //FIXME what happens if not everything was written?
          println!("FRONT CONN[{:?}]: wrote {} bytes", self.token.unwrap(), r);

          self.back_mut_buf = Some(buf.flip());

          //self.front_interest.insert(EventSet::readable());
          self.front_interest.remove(EventSet::writable());
          self.back_interest.insert(EventSet::readable());
        }
        Err(e) =>  println!("not implemented; client err={:?}", e),
      }
    }
    event_loop.reregister(&self.backend, self.backend_token.unwrap(), self.back_interest, PollOpt::edge() | PollOpt::oneshot());
    event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    Ok(())
  }

  fn readable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
    //println!("in readable()");
    let mut buf = self.front_mut_buf.take().unwrap();

    match self.sock.try_read_buf(&mut buf) {
      Ok(None) => {
        println!("We just got readable, but were unable to read from the socket?");
      }
      Ok(Some(r)) => {
        println!("FRONT CONN[{:?}]: read {} bytes", self.token.unwrap(), r);
        self.front_interest.remove(EventSet::readable());
        self.back_interest.insert(EventSet::writable());
        // prepare to provide this to writable
        self.front_buf = Some(buf.flip());
      }
      Err(e) => {
        println!("not implemented; client err={:?}", e);
        //self.front_interest.remove(EventSet::readable());
      }
    };

    event_loop.reregister(&self.backend, self.backend_token.unwrap(), self.back_interest, PollOpt::edge() | PollOpt::oneshot());
    event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    Ok(())
  }

  fn back_writable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
    //println!("in writable()");
    if let Some(mut buf) = self.front_buf.take() {
      //println!("in writable 2");

      match self.sock.try_write_buf(&mut buf) {
        Ok(None) => {
          println!("client flushing buf; WOULDBLOCK");

          self.front_buf = Some(buf);
          self.back_interest.insert(EventSet::writable());
        }
        Ok(Some(r)) => {
          //FIXME what happens if not everything was written?
          println!("BACK  CONN[{:?}]: wrote {} bytes", self.backend_token.unwrap(), r);

          self.front_mut_buf = Some(buf.flip());

          self.front_interest.insert(EventSet::readable());
          self.back_interest.remove(EventSet::writable());
        }
        Err(e) =>  println!("not implemented; client err={:?}", e),
      }
    }
    event_loop.reregister(&self.backend, self.backend_token.unwrap(), self.back_interest, PollOpt::edge() | PollOpt::oneshot());
    event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    Ok(())
  }

  fn back_readable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
    //println!("in readable()");
    let mut buf = self.back_mut_buf.take().unwrap();

    match self.sock.try_read_buf(&mut buf) {
      Ok(None) => {
        println!("We just got readable, but were unable to read from the socket?");
      }
      Ok(Some(r)) => {
        println!("BACK  CONN[{:?}]: read {} bytes", self.backend_token.unwrap(), r);
        self.back_interest.remove(EventSet::readable());
        self.front_interest.insert(EventSet::writable());
        // prepare to provide this to writable
        self.back_buf = Some(buf.flip());
      }
      Err(e) => {
        println!("not implemented; client err={:?}", e);
        //self.interest.remove(EventSet::readable());
      }
    };

    event_loop.reregister(&self.backend, self.backend_token.unwrap(), self.back_interest, PollOpt::edge() | PollOpt::oneshot());
    event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    Ok(())
  }
}

pub struct Backend {
  sock:          TcpListener,
  token:         Option<Token>,
  front_address: SocketAddr,
  back_address:  SocketAddr
}

pub struct Server {
  servers: Slab<Backend>,
  clients: Slab<Client>,
  backend: Slab<Token>
}

impl Server {
  fn new() -> Server {
    Server {
      servers: Slab::new_starting_at(Token(0), 128),
      clients: Slab::new_starting_at(Token(128), 128),
      backend: Slab::new_starting_at(Token(256), 128)
    }
  }

  pub fn add_server(&mut self, front: &SocketAddr, back: &SocketAddr, event_loop: &mut EventLoop<Server>) -> Option<Token> {
    let listener = TcpListener::bind(front).unwrap();
    let back = Backend { sock: listener, token: None, front_address: front.clone(), back_address: back.clone() };
    let tok = self.servers.insert(back)
            .ok().expect("could not add listener to slab");
    self.servers[tok].token = Some(tok);
    event_loop.register_opt(&self.servers[tok].sock, tok, EventSet::readable(), PollOpt::level()).unwrap();
    println!("added server {:?}", tok);
    Some(tok)
  }


  //FIXME: this does not close existing connections, is that what we want?
  pub fn remove_server(&mut self, tok: Token, event_loop: &mut EventLoop<Server>) {
    println!("removing server {:?}", tok);
    if self.servers.contains(tok) {
      event_loop.deregister(&self.servers[tok].sock);
      self.servers.remove(tok);
      //self.servers[tok].sock.shutdown(Shutdown::Both);
    }
  }

  pub fn accept(&mut self, event_loop: &mut EventLoop<Server>, token: Token) {
    let accepted = self.servers[token].sock.accept();
    if let Ok(Some(sock)) = accepted {
      let mut backend = TcpStream::connect(&self.servers[token].back_address).unwrap();

      let tok = self.clients.insert(Client::new(sock, backend))
              .ok().expect("could not add client to slab");

      let backend_tok = self.backend.insert(tok).ok().expect("could not add backend to slab");
      &self.clients[tok].set_tokens(tok, backend_tok);

      event_loop.register_opt(&self.clients[tok].sock, tok, EventSet::all(), PollOpt::edge() | PollOpt::oneshot()).unwrap();
      event_loop.register_opt(&self.clients[tok].backend, backend_tok, EventSet::all(), PollOpt::edge() | PollOpt::oneshot()).unwrap();
      println!("accepted client {:?}", tok);
    } else {
      println!("could not accept connection: {:?}", accepted);
    }
  }
}

impl Handler for Server {
  type Timeout = usize;
  type Message = ();

  fn ready(&mut self, event_loop: &mut EventLoop<Server>, token: Token, events: EventSet) {
    //println!("{:?} got events: {:?}", token, events);
    if events.is_readable() {
      //println!("{:?} is readable", token);
      if token.as_usize() < 128 {
        self.accept(event_loop, token)
      } else if token.as_usize() < 256 {
        if self.clients.contains(token) {
          self.clients[token].readable(event_loop);
        } else {
          println!("client {:?} was removed", token);
        }
      } else if token.as_usize() >= 256 {
        if self.backend.contains(token) {
          let tok = self.backend[token];
          if self.clients.contains(tok) {
            self.clients[tok].back_readable(event_loop);
          } else {
            println!("client {:?} was removed", token);
          }
        } else {
          println!("backend {:?} was removed", token);
        }
      }
      //match token {
      //  SERVER => self.server.accept(event_loop).unwrap(),
      //  i => self.server.conn_readable(event_loop, i).unwrap()
     // }
    }

    if events.is_writable() {
      //println!("{:?} is writable", token);
      if token.as_usize() < 128 {
        println!("received writable for listener {:?}, this should not happen", token);
      } else  if token.as_usize() < 256 {
        if self.clients.contains(token) {
          self.clients[token].writable(event_loop);
        } else {
          println!("client {:?} was removed", token);
        }
      } else if token.as_usize() >= 256 {
        if self.backend.contains(token) {
          let tok = self.backend[token];
          if self.clients.contains(tok) {
            self.clients[tok].back_writable(event_loop);
          } else {
            println!("client {:?} was removed", token);
          }
        } else {
          println!("backend {:?} was removed", token);
        }
      }
      //match token {
      //  SERVER => panic!("received writable for token 0"),
        //CLIENT => self.client.writable(event_loop).unwrap(),
      //  _ => self.server.conn_writable(event_loop, token).unwrap()
      //};
    }

    if events.is_hup() {
      if token.as_usize() < 128 {
        println!("should not happen: server {:?} closed", token);
      } else if token.as_usize() < 256 {
        if self.clients.contains(token) {
          println!("removing client {:?}", token);
          let back_tok = self.clients[token].backend_token.unwrap();
          {
            let sock        = &mut self.clients[token].sock;
            event_loop.deregister(sock);
          };
          {
            let backend     = &mut self.clients[token].backend;
            event_loop.deregister(backend);
          }
          self.clients.remove(token);
          self.backend.remove(back_tok);
        } else {
          println!("client {:?} was removed", token);
        }
      } else if token.as_usize() >= 256 {
        if self.backend.contains(token) {
          let tok = self.backend[token];
          if self.clients.contains(tok) {
            println!("removing client {:?}", tok);
            {
              let sock        = &mut self.clients[tok].sock;
              event_loop.deregister(sock);
            }
            {
              let backend     = &mut self.clients[tok].backend;
              event_loop.deregister(backend);
            }
            self.clients.remove(tok);
            self.backend.remove(token);
          } else {
            println!("client {:?} was removed", token);
          }
        } else {

          println!("backend {:?} was removed", token);
        }

      }
    }
  }
}

pub fn start() {
  let mut event_loop = EventLoop::new().unwrap();


  println!("listen for connections");
  //event_loop.register_opt(&listener, SERVER, EventSet::readable(), PollOpt::edge() | PollOpt::oneshot()).unwrap();
  let mut s = Server::new();
  {
    let front: SocketAddr = FromStr::from_str("127.0.0.1:1234").unwrap();
    let back: SocketAddr = FromStr::from_str("127.0.0.1:5678").unwrap();
    s.add_server(&front, &back, &mut event_loop);
  }
  {
    let front: SocketAddr = FromStr::from_str("127.0.0.1:1235").unwrap();
    let back: SocketAddr = FromStr::from_str("127.0.0.1:5678").unwrap();
    s.add_server(&front, &back, &mut event_loop);
  }
  thread::spawn(move|| {
    println!("starting event loop");
    event_loop.run(&mut s).unwrap();
    println!("ending event loop");
  });
}

//pub fn start_listener(address: &str) -> (Sender<Message>,thread::JoinHandle<()>)  {
pub fn start_listener(address: &str) -> (u8,thread::JoinHandle<()>)  {
 // let mut event_loop:EventLoop<TcpHandler<Client>> = EventLoop::new().unwrap();
  //let t2 = event_loop.channel();
  let s = String::new() + address.clone();
  let jg = thread::spawn(move || {
/*    let listener = NonBlock::new(TcpListener::bind(&s[..]).unwrap());
    event_loop.register(&listener, SERVER).unwrap();
    //let t = storage(&event_loop.channel(), "pouet");

    event_loop.run(&mut TcpHandler {
      listener: listener,
      //storage_tx: t,
      counter: 0,
      token_index: 1, // 0 is the server socket
      clients: HashMap::new(),
      available_tokens: Vec::new()
    }).unwrap();
*/
  });

  (1, jg)
}


#[cfg(test)]
mod tests {
  use super::*;
  use std::net::{TcpListener, TcpStream, Shutdown};
  use std::io::{Read,Write};
  use std::{thread,str};
  use std::sync::mpsc;
  use time;

  #[test]
  fn mi() {
    thread::spawn(|| { start_server(); });
    start();
    thread::sleep_ms(300);

    let mut s1 = TcpStream::connect("127.0.0.1:1234").unwrap();
    let mut s3 = TcpStream::connect("127.0.0.1:1234").unwrap();
    thread::sleep_ms(300);
    let mut s2 = TcpStream::connect("127.0.0.1:1234").unwrap();
    s1.write(&b"hello"[..]);
    println!("s1 sent");
    s2.write(&b"pouet pouet"[..]);
    println!("s2 sent");
    thread::sleep_ms(500);

    let mut res = [0; 128];
    s1.write(&b"coucou"[..]);
    let mut sz1 = s1.read(&mut res[..]).unwrap();
    println!("s1 received {:?}", str::from_utf8(&res[..sz1]));
    assert_eq!(&res[..sz1], &b"hello"[..]);
    s3.shutdown(Shutdown::Both);
    let sz2 = s2.read(&mut res[..]).unwrap();
    println!("s2 received {:?}", str::from_utf8(&res[..sz2]));
    assert_eq!(&res[..sz2], &b"pouet pouet"[..]);


    thread::sleep_ms(200);
    thread::sleep_ms(200);
    sz1 = s1.read(&mut res[..]).unwrap();
    println!("s1 received again({}): {:?}", sz1, str::from_utf8(&res[..sz1]));
    assert_eq!(&res[..sz1], &b"coucou"[..]);
    //assert!(false);
  }

  /*
  #[test]
  fn concurrent() {
    let thread_nb = 127;

    thread::spawn(|| { start_server(); });
    start();
    thread::sleep_ms(300);

    let (tx, rx) = mpsc::channel();

    let begin = time::precise_time_s();
    for i in 0..thread_nb {
      let id = i;
      let tx = tx.clone();
      thread::Builder::new().name(id.to_string()).spawn(move || {
        let s = format!("[{}] Hello world!\n", id);
        let v: Vec<u8> = s.bytes().collect();
        if let Ok(mut conn) = TcpStream::connect("127.0.0.1:1234") {
          let mut res = [0; 128];
          for j in 0..1000 {
            conn.write(&v[..]);

            if j % 5 == 0 {
              if let Ok(sz) = conn.read(&mut res[..]) {
                //println!("[{}] received({}): {:?}", id, sz, str::from_utf8(&res[..sz]));
              } else {
                println!("failed reading");
                tx.send(());
                return;
              }
            }
          }
          tx.send(());
          return;
        } else {
          println!("failed connecting");
          tx.send(());
          return;
        }
      });
    }
    //thread::sleep_ms(5000);
    for i in 0..thread_nb {
      rx.recv();
    }
    let end = time::precise_time_s();
    println!("executed in {} seconds", end - begin);
    //assert!(false);
  }
  */

  #[allow(unused_mut, unused_must_use, unused_variables)]
  fn start_server() {
    let listener = TcpListener::bind("127.0.0.1:5678").unwrap();
    fn handle_client(stream: &mut TcpStream, id: u8) {
      let mut buf = [0; 128];
      let response = b"COIN COIN";
      while let Ok(sz) = stream.read(&mut buf[..]) {
        if sz > 0 {
          println!("[{}] {:?}", id, str::from_utf8(&buf[..sz]));
          //stream.write(&buf[..sz]);
          thread::sleep_ms(200);
          stream.write(&response[..]);
        }
      }
    }

    let mut count = 0;
    thread::spawn(move|| {
      for conn in listener.incoming() {
        match conn {
          Ok(mut stream) => {
            thread::spawn(move|| {
              println!("got a new client: {}", count);
              handle_client(&mut stream, count)
            });
          }
          Err(e) => { println!("connection failed"); }
        }
        count += 1;
      }
    });
  }

}
