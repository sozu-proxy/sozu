use std::thread::{self,Thread,Builder};
use std::sync::mpsc::{channel,Receiver};
use mio::tcp::*;
use mio::*;
use mio::buf::{ByteBuf,MutByteBuf};
use std::collections::HashMap;
use std::io::{self,Read,ErrorKind};
use nom::HexDisplay;
use std::error::Error;

const SERVER: Token = Token(0);

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

pub fn start_listener(address: &str) -> (Sender<Message>,thread::JoinHandle<()>)  {
  let mut event_loop:EventLoop<TcpHandler<Client>> = EventLoop::new().unwrap();
  let t2 = event_loop.channel();
  let jg = thread::spawn(move || {
    let listener = NonBlock::new(TcpListener::bind("127.0.0.1:9092").unwrap());
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

  });

  (t2, jg)
}

