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
      println!("in writable 2");

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
    println!("in readable()");
    let mut buf = self.front_mut_buf.take().unwrap();

    match self.sock.try_read_buf(&mut buf) {
      Ok(None) => {
        panic!("We just got readable, but were unable to read from the socket?");
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
      println!("in writable 2");

      match self.sock.try_write_buf(&mut buf) {
        Ok(None) => {
          println!("client flushing buf; WOULDBLOCK");

          self.front_buf = Some(buf);
          self.back_interest.insert(EventSet::writable());
        }
        Ok(Some(r)) => {
          //FIXME what happens if not everything was written?
          println!("BACK CONN[{:?}]:  wrote {} bytes", self.backend_token.unwrap(), r);

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
        panic!("We just got readable, but were unable to read from the socket?");
      }
      Ok(Some(r)) => {
        println!("BACK CONN[{:?}]:  read {} bytes", self.backend_token.unwrap(), r);
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

pub struct Server {
  servers: Slab<TcpListener>,
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

  pub fn add_server(&mut self, srv: TcpListener, event_loop: &mut EventLoop<Server>) {
    let tok = self.servers.insert(srv)
            .ok().expect("could not add listener to slab");
    //event_loop.register_opt(&self.servers[tok], tok, EventSet::all(), PollOpt::edge() | PollOpt::oneshot()).unwrap();
    event_loop.register_opt(&self.servers[tok], tok, EventSet::readable(), PollOpt::level()).unwrap();
    //event_loop.register(&self.servers[tok], tok).unwrap();
    println!("added server {:?}", tok);
  }

  pub fn accept(&mut self, event_loop: &mut EventLoop<Server>, token: Token) {
    let accepted = self.servers[token].accept();
    if let Ok(Some(sock)) = accepted {
      let addr: SocketAddr = FromStr::from_str("127.0.0.1:5678").unwrap();
      let mut backend = TcpStream::connect(&addr).unwrap();

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

  let addr: SocketAddr = FromStr::from_str("127.0.0.1:1234").unwrap();
  let listener = TcpListener::bind(&addr).unwrap();

  println!("listen for connections");
  //event_loop.register_opt(&listener, SERVER, EventSet::readable(), PollOpt::edge() | PollOpt::oneshot()).unwrap();
  let mut s = Server::new();
  s.add_server(listener, &mut event_loop);
  thread::spawn(move|| {
    println!("starting event loop");
    event_loop.run(&mut s).unwrap();
    println!("ending event loop");
  });
}

/*
pub struct NetworkState {
  pub socket: NonBlock<TcpStream>,
  pub state:  ClientState,
  pub token:  usize,
  pub buffer: Option<MutByteBuf>
}

pub struct Client {
  network_state: NetworkState
}

pub struct Listener {
  t: Thread,
  rw: Receiver<u8>
}

pub trait NetworkClient {
  fn new(stream: NonBlock<TcpStream>, index: usize) -> Self;
  fn handle_message(&mut self, buffer: &mut ByteBuf) -> ClientErr;
  fn network_state(&mut self) -> &mut NetworkState;

  fn state(&mut self) -> ClientState {
    self.network_state().state.clone()
  }

  fn set_state(&mut self, st: ClientState) {
    self.network_state().state = st;
  }

  fn buffer(&mut self) -> Option<MutByteBuf> {
    self.network_state().buffer.take()
  }

  fn set_buffer(&mut self, buf: MutByteBuf) {
    self.network_state().buffer = Some(buf);
  }

  fn socket(&mut self) -> &mut NonBlock<TcpStream> {
    &mut self.network_state().socket
  }

  fn read_size(&mut self) -> ClientResult {
    let mut size_buf = ByteBuf::mut_with_capacity(4);
    match self.socket().read(&mut size_buf) {
      Ok(Some(size)) => {
        if size != 4 {
          Err(ClientErr::Continue)
        } else {
          let mut b = size_buf.flip();
          let b1 = b.read_byte().unwrap();
          let b2 = b.read_byte().unwrap();
          let b3 = b.read_byte().unwrap();
          let b4 = b.read_byte().unwrap();
          let sz = ((b1 as u32) << 24) + ((b2 as u32) << 16) + ((b3 as u32) << 8) + b4 as u32;
          //println!("found size: {}", sz);
          Ok(sz as usize)
        }
      },
      Ok(None) => Err(ClientErr::Continue),
      Err(e) => {
        match e.kind() {
          ErrorKind::BrokenPipe => {
            println!("broken pipe, removing client");
            Err(ClientErr::ShouldClose)
          },
          _ => {
            println!("error writing: {:?} | {:?} | {:?} | {:?}", e, e.description(), e.cause(), e.kind());
            Err(ClientErr::Continue)
          }
        }
      }
    }
  }

  fn read_to_buf(&mut self, buffer: &mut MutByteBuf) -> ClientResult {
    let mut bytes_read: usize = 0;
    loop {
      println!("remaining space: {}", buffer.remaining());
      match self.socket().read(buffer) {
        Ok(a) => {
          if a == None || a == Some(0) {
            println!("breaking because a == {:?}", a);
            break;
          }
          println!("Ok({:?})", a);
          if let Some(just_read) = a {
            bytes_read += just_read;
          }
        },
        Err(e) => {
          match e.kind() {
            ErrorKind::BrokenPipe => {
              println!("broken pipe, removing client");
              return Err(ClientErr::ShouldClose)
            },
            _ => {
              println!("error writing: {:?} | {:?} | {:?} | {:?}", e, e.description(), e.cause(), e.kind());
              return Err(ClientErr::Continue)
            }
          }
        }
      }
    }
    Ok(bytes_read)
  }

  fn write(&mut self, msg: &[u8]) -> ClientResult {
    match self.socket().write_slice(msg) {
      Ok(Some(o))  => {
        println!("sent message: {:?}", o);
        Ok(o)
      },
      Ok(None) => Err(ClientErr::Continue),
      Err(e) => {
        match e.kind() {
          ErrorKind::BrokenPipe => {
            println!("broken pipe, removing client");
            Err(ClientErr::ShouldClose)
          },
          _ => {
            println!("error writing: {:?} | {:?} | {:?} | {:?}", e, e.description(), e.cause(), e.kind());
            Err(ClientErr::Continue)
          }
        }
      }
    }
  }
}

impl NetworkClient for Client {
  fn new(stream: NonBlock<TcpStream>, index: usize) -> Client {
    Client{
      network_state: NetworkState {
        socket: stream,
        state: ClientState::Normal,
        token: index,
        buffer: None
      }
    }
  }

  fn network_state(&mut self) -> &mut NetworkState {
    &mut self.network_state
  }

  fn handle_message(&mut self, buffer: &mut ByteBuf) ->ClientErr {
    let size = buffer.remaining();
    let mut res: Vec<u8> = Vec::with_capacity(size);
    unsafe {
      res.set_len(size);
    }
    buffer.read_slice(&mut res[..]);
    println!("handle_message got {} bytes:\n{}", (&res[..]).len(), (&res[..]).to_hex(8));
    ClientErr::Continue
  }
}


#[derive(Debug)]
pub enum Message {
  Stop,
  Data(Vec<u8>),
  Close(usize)
}

#[derive(Debug,Clone)]
pub enum ClientState {
  Normal,
  Await(usize)
}

#[derive(Debug)]
pub enum ClientErr {
  Continue,
  ShouldClose
}

pub type ClientResult = Result<usize, ClientErr>;

struct Server {
  sock: TcpListener,
  conns: Slab<mioco::IOHandle>,
  pub clients:     HashMap<usize, Client>,
}

pub struct TcpHandler<Client: NetworkClient> {
  pub listener:    NonBlock<TcpListener>,
  //pub storage_tx : mpsc::Sender<storage::Request>,
  pub counter:     u8,
  pub token_index: usize,
  pub clients:     HashMap<usize, Client>,
  pub available_tokens: Vec<usize>
}


impl<Client: NetworkClient> TcpHandler<Client> {
  fn accept(&mut self, event_loop: &mut EventLoop<Self>) {
    if let Ok(Some(stream)) = self.listener.accept() {
      let index = self.next_token();
      println!("got client n°{:?}", index);
      let token = Token(index);
      event_loop.register_opt(&stream, token, Interest::all(), PollOpt::edge());
      self.clients.insert(index, Client::new(stream, index));
    } else {
      println!("invalid connection");
    }
  }

  fn client_read(&mut self, event_loop: &mut EventLoop<Self>, tk: usize) {
    //println!("client n°{:?} readable", tk);
    if let Some(mut client) = self.clients.get_mut(&tk) {

      match client.state() {
        ClientState::Normal => {
          //println!("state normal");
          match client.read_size() {
            Ok(size) => {
              let mut buffer = ByteBuf::mut_with_capacity(size);

              let capacity = buffer.remaining();  // actual buffer capacity may be higher
              //println!("capacity: {}", capacity);
              if let Err(ClientErr::ShouldClose) = client.read_to_buf(&mut buffer) {
                event_loop.channel().send(Message::Close(tk));
              }

              if capacity - buffer.remaining() < size {
                //println!("read {} bytes", capacity - buffer.remaining());
                //client.state = ClientState::Await(size - (capacity - buffer.remaining()));
                client.set_state(ClientState::Await(size - (capacity - buffer.remaining())));
                //client.buffer = Some(buffer);
                client.set_buffer(buffer);
              } else {
                //println!("got enough bytes: {}", capacity - buffer.remaining());
                let mut text = String::new();
                let mut buf = buffer.flip();
                if let ClientErr::ShouldClose = client.handle_message(&mut buf) {
                  event_loop.channel().send(Message::Close(tk));
                }
              }
            },
            Err(ClientErr::ShouldClose) => {
              println!("should close");
              event_loop.channel().send(Message::Close(tk));
            },
            a => {
              println!("other error: {:?}", a);
            }
          }
        },
        ClientState::Await(sz) => {
          println!("awaits {} bytes", sz);
          //let mut buffer = client.buffer.take().unwrap();
          let mut buffer = client.buffer().unwrap();
          let capacity = buffer.remaining();

          if let Err(ClientErr::ShouldClose) = client.read_to_buf(&mut buffer) {
            event_loop.channel().send(Message::Close(tk));
          }
          if capacity - buffer.remaining() < sz {
            //client.state  = ClientState::Await(sz - (capacity - buffer.remaining()));
            client.set_state(ClientState::Await(sz - (capacity - buffer.remaining())));
            //client.buffer = Some(buffer);
            client.set_buffer(buffer);
          } else {
            let mut text = String::new();
            let mut buf = buffer.flip();
            if let ClientErr::ShouldClose = client.handle_message(&mut buf) {
              event_loop.channel().send(Message::Close(tk));
            }
          }
        }
      }
    }
  }

  fn client_write(&mut self, event_loop: &mut EventLoop<Self>, tk: usize) {
    //println!("client n°{:?} readable", tk);
    if let Some(mut client) = self.clients.get_mut(&tk) {
      let s = b"";
      client.write(s);
    }
  }


  fn next_token(&mut self) -> usize {
    match self.available_tokens.pop() {
      None        => {
        let index = self.token_index;
        self.token_index += 1;
        index
      },
      Some(index) => {
        index
      }
    }
  }

  fn close(&mut self, token: usize) {
    self.clients.remove(&token);
    self.available_tokens.push(token);
  }
}

impl<Client:NetworkClient> Handler for TcpHandler<Client> {
  type Timeout = ();
  type Message = Message;

  fn readable(&mut self, event_loop: &mut EventLoop<Self>, token: Token, _: ReadHint) {
    //println!("readable");
    match token {
      SERVER => {
        self.accept(event_loop);
      },
      Token(tk) => {
        self.client_read(event_loop, tk);
      }
    }
  }

  fn writable(&mut self, event_loop: &mut EventLoop<Self>, token: Token) {
    match token {
      SERVER => {
        println!("server writeable");
      },
      Token(tk) => {
        //println!("client n°{:?} writeable", tk);
        //self.client_write(event_loop, tk);
      }
    }
  }

  fn notify(&mut self, _reactor: &mut EventLoop<Self>, msg: Message) {
    println!("notify: {:?}", msg);
    match msg {
      Message::Close(token) => {
        println!("closing client n°{:?}", token);
        self.close(token)
      },
      _                     => println!("unknown message: {:?}", msg)
    }
  }

  fn timeout(&mut self, event_loop: &mut EventLoop<Self>, timeout: Self::Timeout) {
    println!("timeout");
  }

  fn interrupted(&mut self, event_loop: &mut EventLoop<Self>) {
    println!("interrupted");
  }
}
*/


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

  #[test]
  fn mi() {
    thread::spawn(|| { start_server(); });
    start();
    thread::sleep_ms(300);

    let mut s1 = TcpStream::connect("127.0.0.1:1234").unwrap();
    let mut s3 = TcpStream::connect("127.0.0.1:1234").unwrap();
    thread::sleep_ms(300);
    s3.shutdown(Shutdown::Both);
    let mut s2 = TcpStream::connect("127.0.0.1:1234").unwrap();
    s1.write(&b"hello"[..]);
    println!("s1 sent");
    s2.write(&b"pouet pouet"[..]);
    println!("s2 sent");
    thread::sleep_ms(500);

    let mut res = [0; 128];
    let mut sz1 = s1.read(&mut res[..]).unwrap();
    println!("s1 received {:?}", str::from_utf8(&res[..sz1]));
    assert_eq!(&res[..sz1], &b"hello"[..]);
    let sz2 = s2.read(&mut res[..]).unwrap();
    println!("s2 received {:?}", str::from_utf8(&res[..sz2]));
    assert_eq!(&res[..sz2], &b"pouet pouet"[..]);

    //s1.shutdown(Shutdown::Both);
    //s2.shutdown(Shutdown::Both);

    thread::sleep_ms(200);
    s1.write(&b"coucou"[..]);
    thread::sleep_ms(200);
    sz1 = s1.read(&mut res[..]).unwrap();
    println!("s1 received {:?}", str::from_utf8(&res[..sz1]));
    assert!(false);
  }

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

/*
  #[allow(unused_mut, unused_must_use, unused_variables)]
  #[test]
  fn hello() {
    start_listener("127.0.0.1:5678");
    start_server();
    let mut s1 = TcpStream::connect("127.0.0.1:5678").unwrap();
    let mut s2 = TcpStream::connect("127.0.0.1:5678").unwrap();
    s1.write(&b"hello"[..]);
    s2.write(&b"pouet pouet"[..]);

    let mut res = [0; 128];
    let sz2 = s2.read(&mut res[..]).unwrap();
    assert_eq!(&res[..sz2], &b"pouet pouet"[..]);
    let sz1 = s1.read(&mut res[..]).unwrap();
    assert_eq!(&res[..sz1], &b"hello"[..]);
  }
*/
}
