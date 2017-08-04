use mio::*;
use mio::timer::Timer;
use mio_uds::UnixListener;
use slab::Slab;
use std::fs;
use std::fmt;
use std::path::PathBuf;
use std::io::{self,ErrorKind};
use std::os::unix::fs::PermissionsExt;
use std::collections::{HashMap,HashSet,VecDeque};
use std::time::Duration;
use libc::{pid_t,kill};

use sozu::messages::{Order,OrderMessage,OrderMessageAnswer,OrderMessageAnswerData,OrderMessageStatus};
use sozu::channel::Channel;
use sozu::network::metrics::METRICS;
use sozu_command::state::ConfigState;
use sozu_command::data::{AnswerData,ConfigMessage,ConfigMessageAnswer,ConfigMessageStatus,RunState};
use sozu_command::config::Config;

pub mod orders;
pub mod client;
pub mod state;

use self::client::CommandClient;

const SERVER: Token = Token(0);
const HALF_USIZE: usize = 0x8000000000000000usize;

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

pub struct Worker {
  pub id:            u32,
  pub channel:       Channel<OrderMessage,OrderMessageAnswer>,
  pub token:         Option<Token>,
  pub pid:           pid_t,
  pub run_state:     RunState,
  pub queue:         VecDeque<OrderMessage>,
}

impl Worker {
  pub fn new(id: u32, pid: pid_t, channel: Channel<OrderMessage,OrderMessageAnswer>, config: &Config) -> Worker {
    Worker {
      id:         id,
      channel:    channel,
      token:      None,
      pid:        pid,
      run_state:  RunState::Running,
      queue:      VecDeque::new(),
    }
  }

  pub fn push_message(&mut self, message: OrderMessage) {
    self.queue.push_back(message);
    self.channel.interest.insert(Ready::writable());
  }

  pub fn can_handle_events(&self) -> bool {
    self.channel.readiness().is_readable() || (!self.queue.is_empty() && self.channel.readiness().is_writable())
  }
}

impl fmt::Debug for Worker {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    write!(f, "Worker {{ id: {}, run_state: {:?} }}", self.id, self.run_state)
  }
}

#[derive(Deserialize,Serialize,Debug)]
pub struct ProxyConfiguration {
  id:    String,
  state: ConfigState,
}

pub struct CommandServer {
  sock:            UnixListener,
  buffer_size:     usize,
  max_buffer_size: usize,
  clients:         Slab<CommandClient,FrontToken>,
  proxies:         HashMap<Token, Worker>,
  next_id:         u32,
  state:           ConfigState,
  pub poll:        Poll,
  timer:           Timer<Token>,
  config:          Config,
  token_count:     usize,
  order_state:     state::OrderState,
  must_stop:       bool,
}

impl CommandServer {
  fn accept(&mut self) -> io::Result<()> {
    debug!("server accepting socket");

    let acc = self.sock.accept();
    if let Ok(Some((sock, _))) = acc {
      let conn = CommandClient::new(sock, self.buffer_size, self.max_buffer_size);
      let tok = self.clients.insert(conn)
        .ok().expect("could not add connection to slab");

      // Register the connection
      let token = self.from_front(tok);
      self.clients[tok].token = Some(token);
      let mut interest = Ready::hup();
      interest.insert(Ready::readable());
      interest.insert(Ready::writable());
      self.poll.register(&self.clients[tok].channel.sock, token, Ready::all(), PollOpt::edge())
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

  fn new(srv: UnixListener, config: Config, mut proxy_vec: Vec<Worker>, poll: Poll) -> CommandServer {
    //FIXME: verify this
    poll.register(&srv, Token(0), Ready::readable(), PollOpt::edge() | PollOpt::oneshot()).unwrap();
    METRICS.with(|metrics| {
      if let Some(sock) = (*metrics.borrow()).socket() {
        poll.register(sock, Token(1), Ready::writable(), PollOpt::edge()).expect("should register the metrics socket");
      } else {
        error!("could not register metrics socket");
      }
    });


    let next_id = proxy_vec.len();

    let mut proxies = HashMap::new();

    let mut token_count = 1;
    //FIXME: verify there's at least one worker
    //TODO: make config state from Config ADD IP ADDRESSES AND PORTS
    let state: ConfigState = Default::default();


    for mut proxy in proxy_vec.drain(..) {
      token_count += 1;
      poll.register(&proxy.channel.sock, Token(token_count), Ready::all(), PollOpt::edge()).unwrap();
      proxy.token = Some(Token(token_count));
      proxies.insert(Token(token_count), proxy);
    }

    //let mut timer = timer::Builder::default().tick_duration(Duration::from_millis(1000)).build();
    //FIXME: registering the timer makes the timer thread spin too much
    let mut timer = timer::Timer::default();
    //poll.register(&timer, Token(1), Ready::readable(), PollOpt::edge()).unwrap();
    timer.set_timeout(Duration::from_millis(700), Token(0));

    let buffer_size = config.command_buffer_size.unwrap_or(10000);

    CommandServer {
      sock:            srv,
      buffer_size:     buffer_size,
      max_buffer_size: config.max_command_buffer_size.unwrap_or(buffer_size * 2),
      clients:         Slab::with_capacity(128),
      proxies:         proxies,
      next_id:         next_id as u32,
      state:           state,
      poll:            poll,
      timer:           timer,
      config:          config,
      token_count:     token_count,
      order_state:     state::OrderState::new(),
      must_stop:       false,
    }
  }

  pub fn to_front(&self, token: Token) -> FrontToken {
    FrontToken(token.0 - HALF_USIZE - 2)
  }

  pub fn from_front(&self, token: FrontToken) -> Token {
    Token(token.0 + HALF_USIZE + 2)
  }

}

impl CommandServer {
  pub fn run(&mut self) {
    let mut events = Events::with_capacity(1024);
    let poll_timeout = Some(Duration::from_millis(1000));
    loop {
      self.poll.poll(&mut events, poll_timeout).unwrap();
      for event in events.iter() {
        self.ready(event.token(), event.kind());
      }

      loop {
        let mut did_something = false;
        {
          let tokens: Vec<Token> = self.clients.iter().filter(|client| client.can_handle_events()).map(|client| client.token.unwrap()).collect();

          if ! tokens.is_empty() {
            did_something = true;
          }
          //let messages: Vec<usize> = self.clients.iter().map(|client| client.queue.len()).collect();

          //for ref client in self.clients.iter() {
          //  let ids: Vec<&str> = client.queue.iter().map(|msg| msg.id.as_str()).collect();
            //info!("client readiness = {:#?}, interest = {:#?}, queue = {}", client.channel.readiness,
            //  client.channel.interest, ids.len());
          //}
          //info!("will handle clients: {:#?} (message queues: {:?})", tokens, messages);
          for token in tokens {
            let front = self.to_front(token);
            self.handle_client_events(front);
          }
        }

        {
          let tokens: Vec<Token> = self.proxies.iter().filter(|&(_, worker)| worker.can_handle_events()).map(|(token, _)| token.clone()).collect();

          if ! tokens.is_empty() {
            did_something = true;
          }

          //for (ref token, ref worker) in self.proxies.iter() {
          //  let ids: Vec<&str> = worker.queue.iter().map(|msg| msg.id.as_str()).collect();
            //info!("worker {}, readiness = {:#?}, interest = {:#?}, queue = {}", token.0, worker.channel.readiness,
            //  worker.channel.interest, ids.len());
          //}

          //info!("will handle workers: {:?}", tokens);
          for token in tokens {
            self.handle_worker_events(token);
          }
        }

        if ! did_something {
          break;
        }
      }

      METRICS.with(|metrics| {
        (*metrics.borrow_mut()).send_data();
      });

      if self.must_stop {
        info!("stopping...");
        break;
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
      Token(1) => {
        METRICS.with(|metrics| {
          (*metrics.borrow_mut()).writable();
        });
      },
      Token(i) if i < HALF_USIZE + 1 => {
        if let Some(ref mut proxy) =self.proxies.get_mut(&Token(i)) {
          proxy.channel.handle_events(events);
        }

        self.handle_worker_events(Token(i));

      },
      _ => {
        let conn_token = self.to_front(token);
        if self.clients.contains(conn_token) {
          self.clients[conn_token].channel.handle_events(events);
        }

        self.handle_client_events(conn_token);
      }
    }
    //trace!("ready end: {:?} -> {:?}", token, events);
  }

  pub fn handle_worker_events(&mut self, token: Token) {
    let mut messages = {
      let mut messages = Vec::new();
      let ref mut proxy = self.proxies.get_mut(&token).unwrap();
      loop {
        if !proxy.queue.is_empty() {
          proxy.channel.interest.insert(Ready::writable());
        }

        //trace!("worker[{}] readiness = {:#?}, interest = {:#?}, queue = {} messages", token.0, proxy.channel.readiness,
        //  proxy.channel.interest, proxy.queue.len());

        if proxy.channel.readiness() == Ready::empty() {
          break;
        }

        if proxy.channel.readiness().is_readable() {
          proxy.channel.readable();

          loop {
            if let Some(msg) = proxy.channel.read_message() {
              messages.push(msg);
            } else {
              if (proxy.channel.interest & proxy.channel.readiness).is_readable() {
                proxy.channel.readable();
                continue;
              } else {
                break;
              }
            }
          }
        }

        if !proxy.queue.is_empty() {
          proxy.channel.interest.insert(Ready::writable());
        }

        if proxy.channel.readiness.is_writable() {
          loop {
            if let Some(msg) = proxy.queue.pop_front() {
              if !proxy.channel.write_message(&msg) {
                proxy.queue.push_front(msg);
              }
            }

            if proxy.channel.back_buf.available_data() > 0 {
              proxy.channel.writable();
            }

            if !proxy.channel.readiness.is_writable() {
              break;
            }

            if proxy.channel.back_buf.available_data() == 0 && proxy.queue.len() == 0 {
              break;
            }
          }
        }
      }

      messages
    };

    for msg in messages.drain(..) {
      self.proxy_handle_message(token, msg);
    }
  }

  pub fn handle_client_events(&mut self, conn_token: FrontToken) {
    if self.clients.contains(conn_token) {

      if self.clients[conn_token].channel.readiness.is_hup() {
        self.poll.deregister(&self.clients[conn_token].channel.sock);
        self.clients.remove(conn_token);
        trace!("closed client [{}]", conn_token.0);
      } else {
        loop {
          /*trace!("client complete readiness[{}] = {:#?} (r = {:#?}, i = {:#?}), queue={} elements", conn_token.0,
            self.clients[conn_token].channel.readiness(),
            self.clients[conn_token].channel.readiness,
            self.clients[conn_token].channel.interest,
            self.clients[conn_token].queue.len()
          );*/

          if self.clients[conn_token].channel.readiness() == Ready::empty() {
            break;
          }

          if self.clients[conn_token].channel.readiness().is_writable() {
            if let Some(msg) = self.clients[conn_token].queue.pop_front() {
              if! self.clients[conn_token].channel.write_message(&msg) {
                self.clients[conn_token].queue.push_front(msg);
              }
              self.clients[conn_token].channel.writable();
            }

            if !self.clients[conn_token].queue.is_empty() {
               self.clients[conn_token].channel.interest.insert(Ready::writable());
            }
          }

          if self.clients[conn_token].channel.readiness().is_readable() {
            self.clients[conn_token].channel.readable();
            loop {
              if let Some(message) = self.clients[conn_token].channel.read_message() {
                self.handle_client_message(conn_token, &message);
              } else {
                break;
              }
            }
          }
        }
      }
    }
  }

  fn proxy_handle_message(&mut self, token: Token, msg: OrderMessageAnswer) {
    trace!("proxy handle message: token {:?} got answer msg: {:?}", token, msg);
    if msg.status != OrderMessageStatus::Processing {
      let mut stopping = false;
      if let Some(ref mut proxy) = self.proxies.get_mut(&token) {
        //FIXME:  handle STOP order here
        //if order == Order::SoftStop || order == Order::HardStop {
        //  stopping = true;
        //  proxy.run_state = RunState::Stopped
        //}
      }

      match msg.status {
        OrderMessageStatus::Processing => {
          //FIXME: right now, do nothing with the curent tasks
        },
        OrderMessageStatus::Error(s) => {
          if let Some(client_message_id) = self.order_state.error(&msg.id, token) {
            //FIXME: send message to client here

            if let Some(task) = self.order_state.task(&client_message_id) {
              //info!("TERMINATING task: {:#?}", task);
              let data = match msg.data {
                None => None,
                Some(OrderMessageAnswerData::Metrics(data)) => Some(AnswerData::Metrics(data)),
              };

              let answer = ConfigMessageAnswer::new(
                client_message_id,
                ConfigMessageStatus::Error,
                format!("ok: {} messages, error: {:?}, message: {}", task.ok.len(), task.error, s.clone()),
                data,
              );

              if let Some(client_token) = task.client {
                info!("SENDING to client[{}]: {:#?}", client_token.0, answer);
                self.clients[client_token].push_message(answer);
              }
            }
          }
        },
        OrderMessageStatus::Ok => {
          if let Some(client_message_id) = self.order_state.ok(&msg.id, token) {
            //FIXME: send message to client here
            if let Some(task) = self.order_state.task(&client_message_id) {
              //info!("TERMINATING task: {:#?}", task);

              let answer = if task.error.is_empty() {
                let data = match msg.data {
                  None => None,
                  Some(OrderMessageAnswerData::Metrics(data)) => Some(AnswerData::Metrics(data)),
                };

                ConfigMessageAnswer::new(
                  client_message_id,
                  ConfigMessageStatus::Ok,
                  format!("ok: {} messages, error: {:#?}", task.ok.len(), task.error),
                  data,
                )
              } else {
                ConfigMessageAnswer::new(
                  msg.id.clone(),
                  ConfigMessageStatus::Error,
                  format!("ok: {:#?}, error: {:#?}", task.ok, task.error),
                  None,
                )
              };

              if let Some(client_token) = task.client {
                info!("SENDING to client[{}]: {:#?}", client_token.0, answer);
                self.clients[client_token].push_message(answer);
              }
            }

            //FIXME: the task should hold the message type,
            //so we can know it's a stop message
            if stopping {
              self.must_stop = true;

            }
          }
        }
      }
    }
  }
}

pub fn start(config: Config, proxies: Vec<Worker>) {
  let saved_state     = config.saved_state.clone();

  let event_loop = Poll::new().unwrap();
  let addr = PathBuf::from(&config.command_socket);
  if let Err(e) = fs::remove_file(&addr) {
    match e.kind() {
      ErrorKind::NotFound => {},
      _ => {
        error!("could not delete previous socket at {:?}: {:?}", addr, e);
        return;
      }
    }
  }
  match UnixListener::bind(&addr) {
    Ok(srv) => {
      if let Err(e) =  fs::set_permissions(&addr, fs::Permissions::from_mode(0o600)) {
        error!("could not set the unix socket permissions: {:?}", e);
        fs::remove_file(&addr);
        return;
      }

      let mut server = CommandServer::new(srv, config.clone(), proxies, event_loop);

      server.load_static_application_configuration();

      saved_state.as_ref().map(|state_path| {
        server.load_state(None, "INITIALIZATION", state_path);
      });

      info!("listen for connections");
      server.run();
      //event_loop.run(&mut CommandServer::new(srv, proxies, buffer_size, max_buffer_size)).unwrap()
    },
    Err(e) => {
      error!("could not create unix socket: {:?}", e);
      // the workers did not even get the configuration, we can kill them right away
      for proxy in proxies {
        error!("killing worker n°{} (PID {})", proxy.id, proxy.pid);
        unsafe { kill(proxy.pid, 9); }
      }
    }
  }
}
