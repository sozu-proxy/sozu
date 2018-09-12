use mio::*;
use mio::unix::UnixReady;
use mio_uds::UnixListener;
use slab::Slab;
use std::fs;
use std::fmt;
use std::path::PathBuf;
use std::io::{self,ErrorKind};
use std::os::unix::fs::PermissionsExt;
use std::collections::{HashMap,VecDeque};
use std::time::Duration;
use libc::pid_t;
use nix::unistd::Pid;
use nix::sys::signal::{kill,Signal};
use nix::sys::wait::{waitpid,WaitStatus,WaitPidFlag};

use sozu::network::metrics::METRICS;
use sozu_command::config::Config;
use sozu_command::channel::Channel;
use sozu_command::state::ConfigState;
use sozu_command::data::{ConfigMessage,ConfigMessageAnswer,ConfigMessageStatus,RunState};
use sozu_command::messages::{OrderMessage,OrderMessageAnswer,OrderMessageStatus};
use sozu_command::scm_socket::{Listeners,ScmSocket};

pub mod orders;
pub mod client;
pub mod state;

use worker::{start_worker, get_executable_path};
use self::client::CommandClient;
use self::state::MessageType;

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
  pub scm:           ScmSocket,
}

impl Worker {
  pub fn new(id: u32, pid: pid_t, channel: Channel<OrderMessage,OrderMessageAnswer>, scm: ScmSocket, _: &Config)
    -> Worker {
    Worker {
      id:         id,
      channel:    channel,
      token:      None,
      pid:        pid,
      run_state:  RunState::Running,
      queue:      VecDeque::new(),
      scm:        scm,
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
  config:          Config,
  token_count:     usize,
  order_state:     state::OrderState,
  must_stop:       bool,
  executable_path: String,
  //caching the number of backends instead of going through the whole state.backends hashmap
  backends_count:  usize,
  //caching the number of frontends instead of going through the whole state.http/hhtps/tcp_fronts hashmaps
  frontends_count: usize,
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
      self.poll.register(&self.clients[tok].channel.sock, token,
        Ready::readable() | Ready::writable() | UnixReady::error() | UnixReady::hup(),
        PollOpt::edge())
        .ok().expect("could not register socket with event loop");

      let accept_interest = Ready::readable();
      self.poll.reregister(&self.sock, SERVER, accept_interest, PollOpt::edge())
    } else {
      //FIXME: what do other cases mean, like Ok(None)?
      acc.map(|_| ())
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
      poll.register(&proxy.channel.sock, Token(token_count),
        Ready::readable() | Ready::writable() | UnixReady::error() | UnixReady::hup(),
        PollOpt::edge()).unwrap();
      proxy.token = Some(Token(token_count));
      proxies.insert(Token(token_count), proxy);
    }

    let path = unsafe { get_executable_path() };

    let backends_count = state.count_backends();
    let frontends_count = state.count_frontends();

    CommandServer {
      sock:            srv,
      buffer_size:     config.command_buffer_size,
      max_buffer_size: config.max_command_buffer_size,
      clients:         Slab::with_capacity(128),
      proxies:         proxies,
      next_id:         next_id as u32,
      state:           state,
      poll:            poll,
      config:          config,
      token_count:     token_count,
      order_state:     state::OrderState::new(),
      must_stop:       false,
      executable_path: path,
      backends_count:  backends_count,
      frontends_count:  frontends_count,
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
    let max_poll_errors = 10000;
    let mut current_poll_errors = 0;
    loop {
      if current_poll_errors == max_poll_errors {
        error!("Something is going very wrong. Last {} poll() calls failed, crashing..", current_poll_errors);
        panic!("poll() calls failed {} times in a row", current_poll_errors);
      }

      if let Err(error) = self.poll.poll(&mut events, poll_timeout) {
        error!("Error while polling events: {:?}", error);
        current_poll_errors += 1;
        continue;
      } else {
        current_poll_errors = 0;
      }

      for event in events.iter() {
        self.ready(event.token(), event.readiness());
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

      let clients_not_served = self.clients.iter()
                                            .filter(|c| !c.queue.is_empty())
                                            .count();

      if self.must_stop && clients_not_served == 0 {
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
          let uevent = UnixReady::from(events);
          if uevent.is_hup() {
            if proxy.run_state != RunState::Stopped && proxy.run_state != RunState::Stopping {
              proxy.run_state = RunState::NotAnswering;
            }
          }

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
    if !self.proxies.contains_key(&token) {
      return;
    }

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
          let _ = proxy.channel.readable().map_err(|e| {
            error!("could not read from worker socket: {:?}", e);
          });

          loop {
            if let Some(msg) = proxy.channel.read_message() {
              messages.push(msg);
            } else {
              if (proxy.channel.interest & proxy.channel.readiness).is_readable() {
                let _ = proxy.channel.readable().map_err(|e| {
                  error!("could not read from worker socket: {:?}", e);
                });
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
              let res = proxy.channel.writable();
              if let Err(e) = res {
                error!("could not write to worker socket: {:?}", e);
                if proxy.run_state != RunState::Stopped && proxy.run_state != RunState::Stopping {
                  proxy.run_state = RunState::NotAnswering;
                }
              }
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
      self.handle_worker_message(token, msg);
    }

    let worker_run_state = self.proxies.get(&token).as_ref().map(|proxy| proxy.run_state);
    if self.config.worker_automatic_restart && worker_run_state == Some(RunState::NotAnswering) {
      self.check_worker_status(token);
    }
  }

  pub fn handle_client_events(&mut self, conn_token: FrontToken) {
    if self.clients.contains(conn_token) {

      if UnixReady::from(self.clients[conn_token].channel.readiness).is_hup() {
        let _ = self.poll.deregister(&self.clients[conn_token].channel.sock).map_err(|e| {
          error!("could not unregister client socket: {:?}", e);
        });
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

          {
            let client = &mut self.clients[conn_token];

            if client.channel.readiness() == Ready::empty() {
              break;
            }

            if client.channel.readiness().is_writable() {
              if let Some(msg) = client.queue.pop_front() {
                let write_res = client.channel.write_message(&msg);
                let capacity  = client.channel.back_buf.capacity();
                if !write_res {
                  if client.channel.back_buf.capacity() == capacity {
                    //we cannot grow the channel further
                    error!("cannot write message back to config client: message is larger than max_buffer_size");
                    client.push_message(ConfigMessageAnswer::new(
                      msg.id,
                      ConfigMessageStatus::Error,
                      "cannot write message back to config client because message is larger than max_buffer_size".to_string(),
                      None
                    ));

                  } else {
                    incr!("resp_client_cmd");
                    client.queue.push_front(msg);
                  }
                }
              }
              let _ = client.channel.writable().map_err(|e| {
                error!("could not write to client socket: {:?}", e);
              });

              if !client.queue.is_empty() {
                 client.channel.interest.insert(Ready::writable());
              }
            }
          }

          if self.clients[conn_token].channel.readiness().is_readable() {
            let _ = self.clients[conn_token].channel.readable().map_err(|e| {
              error!("could not read from client socket: {:?}", e);
            });

            loop {
              if let Some(message) = self.clients[conn_token].channel.read_message() {
                incr!("client_cmd");
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

  fn handle_worker_message(&mut self, token: Token, msg: OrderMessageAnswer) {
    trace!("worker handle message: token {:?} got answer msg: {:?}", token, msg);
    if msg.status != OrderMessageStatus::Processing {

      let tag = self.proxies.get(&token).map(|worker| worker.id.to_string()).unwrap_or(String::from(""));

      match msg.status {
        OrderMessageStatus::Processing => {
          //FIXME: right now, do nothing with the curent tasks
        },
        OrderMessageStatus::Error(s) => {
          if let Some(task) = self.order_state.error(&msg.id, token, tag, msg.data) {
            let opt_token = task.client.clone();
            let id        = task.id.clone();
            let ok = task.ok.len();
            let failures = task.error.clone();
            let answer = ConfigMessageAnswer::new(
              id,
              ConfigMessageStatus::Error,
              format!("ok: {} messages, error: {:?}, message: {}", ok, &failures, s.clone()),
              task.generate_data(&self.state),
            );
            error!("{}: {} successful messages, failures: {:?}, reason: {}", msg.id, ok, &failures, s.clone());

            if let Some(client_token) = opt_token {
              self.clients.get_mut(client_token).map(|cl| cl.push_message(answer));
            }
          }
        },
        OrderMessageStatus::Ok => {
          if let Some(task) = self.order_state.ok(&msg.id, token, tag, msg.data) {
            let opt_token = task.client.clone();
            let id        = task.id.clone();
            let answer = if task.error.is_empty() {
              if task.message_type == MessageType::Stop {
                self.must_stop = true;
              }

              if task.message_type == MessageType::Stop || task.message_type == MessageType::StopWorker {
                let ref mut proxy = self.proxies.get_mut(&token).expect("there should be a worker at that token");
                proxy.run_state = RunState::Stopped;
              }

              ConfigMessageAnswer::new(
                id,
                ConfigMessageStatus::Ok,
                format!("ok: {} messages, error: {:#?}", task.ok.len(), task.error),
                task.generate_data(&self.state),
              )
            } else {
              error!("{}: {} successful messages, failures: {:?}", msg.id, task.ok.len(), task.error);

              ConfigMessageAnswer::new(
                id,
                ConfigMessageStatus::Error,
                format!("ok: {:#?}, error: {:#?}", task.ok, task.error),
                None,
              )
            };

            if let Some(client_token) = opt_token {
              self.clients.get_mut(client_token).map(|cl| cl.push_message(answer));
            }
          }
        }
      }
    }
  }

  pub fn check_worker_status(&mut self, token: Token) {
    {
      let ref mut proxy = self.proxies.get_mut(&token).expect("there should be a worker at that token");
      let res = waitpid(Pid::from_raw(proxy.pid), Some(WaitPidFlag::WNOHANG));

      if let Ok(WaitStatus::StillAlive) = res {
        if proxy.run_state == RunState::NotAnswering {
          error!("worker process {} (PID = {}) not answering, killing and replacing", proxy.id, proxy.pid);
          if let Err(e) = kill(Pid::from_raw(proxy.pid), Signal::SIGKILL) {
            error!("failed to kill the worker process: {:?}", e);
          } else {
            proxy.run_state = RunState::Stopped;
          }
        } else {
          return;
        }

      } else {
        if proxy.run_state == RunState::NotAnswering {
          error!("worker process {} (PID = {}) stopped running, replacing", proxy.id, proxy.pid);
        } else {
          error!("failed to check process status: {:?}", res);
          return;
        }
      }

      let _ = self.poll.deregister(&proxy.channel.sock);
    }

    self.proxies.remove(&token);

    incr!("worker_restart");

    let id = self.next_id;
    let listeners = Some(Listeners {
      http: Vec::new(),
      tls:  Vec::new(),
      tcp:  Vec::new(),
    });

    if let Ok(mut worker) = start_worker(id, &self.config, self.executable_path.clone(), &self.state, listeners) {
      info!("created new worker: {}", id);
      self.next_id += 1;
      let worker_token = self.token_count + 1;
      self.token_count = worker_token;
      worker.token     = Some(Token(worker_token));

      debug!("registering new sock {:?} at token {:?} for id {} (sock error: {:?})",
        worker.channel.sock, worker_token, worker.id, worker.channel.sock.take_error());

      self.poll.register(&worker.channel.sock, Token(worker_token),
        Ready::readable() | Ready::writable() | UnixReady::error() | UnixReady::hup(),
        PollOpt::edge()).unwrap();
      worker.token = Some(Token(worker_token));
      self.proxies.insert(Token(worker_token), worker);

    }
  }
}

pub fn start(config: Config, command_socket_path: String, proxies: Vec<Worker>) {
  let saved_state     = config.saved_state_path();

  let event_loop = Poll::new().unwrap();
  let addr = PathBuf::from(&command_socket_path);
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
        let _ = fs::remove_file(&addr).map_err(|e2| {
          error!("could not remove the unix socket: {:?}", e2);
        });
        return;
      }

      let mut server = CommandServer::new(srv, config.clone(), proxies, event_loop);

      server.load_static_application_configuration();

      saved_state.as_ref().map(|state_path| {
        server.load_state(None, "INITIALIZATION", state_path);
      });

      gauge!("configuration.applications", server.state.applications.len());
      gauge!("configuration.backends", server.backends_count);
      gauge!("configuration.frontends", server.frontends_count);

      info!("waiting for configuration client connections");
      server.run();
      //event_loop.run(&mut CommandServer::new(srv, proxies, buffer_size, max_buffer_size)).unwrap()
    },
    Err(e) => {
      error!("could not create unix socket: {:?}", e);
      // the workers did not even get the configuration, we can kill them right away
      for proxy in proxies {
        error!("killing worker n°{} (PID {})", proxy.id, proxy.pid);
        let _ = kill(Pid::from_raw(proxy.pid), Signal::SIGKILL).map_err(|e| {
          error!("could not kill worker: {:?}", e);
        });
      }
    }
  }
}
