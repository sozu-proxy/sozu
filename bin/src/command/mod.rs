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

use sozu::metrics::METRICS;
use sozu_command::config::Config;
use sozu_command::channel::Channel;
use sozu_command::state::ConfigState;
use sozu_command::command::{self,CommandRequest,CommandResponse,CommandResponseData,CommandStatus,RunState};
use sozu_command::proxy::{ProxyRequest,ProxyResponse,ProxyResponseData,Route};
use sozu_command::scm_socket::{Listeners,ScmSocket};

pub mod executor;
pub mod orders;
pub mod client;

use worker::{start_worker, get_executable_path};
use self::client::CommandClient;
use self::executor::{Executor, StateChange};

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
  pub channel:       Channel<ProxyRequest,ProxyResponse>,
  pub token:         Option<Token>,
  pub pid:           pid_t,
  pub run_state:     RunState,
  pub queue:         VecDeque<ProxyRequest>,
  pub scm:           ScmSocket,
}

impl Worker {
  pub fn new(id: u32, pid: pid_t, channel: Channel<ProxyRequest,ProxyResponse>, scm: ScmSocket, _: &Config)
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

  pub fn push_message(&mut self, message: ProxyRequest) {
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
  sock:              UnixListener,
  buffer_size:       usize,
  max_buffer_size:   usize,
  clients:           Slab<CommandClient,FrontToken>,
  workers:           HashMap<Token, Worker>,
  event_subscribers: Vec<FrontToken>,
  next_id:           u32,
  state:             ConfigState,
  pub poll:          Poll,
  config:            Config,
  token_count:       usize,
  must_stop:         bool,
  executable_path:   String,
  //caching the number of backends instead of going through the whole state.backends hashmap
  backends_count:    usize,
  //caching the number of frontends instead of going through the whole state.http/hhtps/tcp_fronts hashmaps
  frontends_count:   usize,
}

impl CommandServer {
  fn accept(&mut self) -> io::Result<()> {
    debug!("server accepting socket");

    let acc = self.sock.accept();
    if let Ok(Some((sock, _))) = acc {
      let conn = CommandClient::new(sock, self.buffer_size, self.max_buffer_size);
      let tok = self.clients.insert(conn)
        .map_err(|_| io::Error::new(ErrorKind::ConnectionRefused, "could not add client to slab"))?;

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

  fn new(srv: UnixListener, config: Config, mut worker_vec: Vec<Worker>, poll: Poll) -> CommandServer {
    //FIXME: verify this
    poll.register(&srv, Token(0), Ready::readable(), PollOpt::edge() | PollOpt::oneshot()).unwrap();
    METRICS.with(|metrics| {
      if let Some(sock) = (*metrics.borrow()).socket() {
        poll.register(sock, Token(1), Ready::writable(), PollOpt::edge()).expect("should register the metrics socket");
      } else {
        error!("could not register metrics socket");
      }
    });


    let next_id = worker_vec.len();

    let mut workers = HashMap::new();

    let mut token_count = 1;
    //FIXME: verify there's at least one worker
    //TODO: make config state from Config ADD IP ADDRESSES AND PORTS
    let state: ConfigState = Default::default();


    for mut worker in worker_vec.drain(..) {
      token_count += 1;
      poll.register(&worker.channel.sock, Token(token_count),
        Ready::readable() | Ready::writable() | UnixReady::error() | UnixReady::hup(),
        PollOpt::edge()).unwrap();
      worker.token = Some(Token(token_count));
      workers.insert(Token(token_count), worker);
    }

    let path = unsafe { get_executable_path() };

    let backends_count = state.count_backends();
    let frontends_count = state.count_frontends();

    CommandServer {
      sock:              srv,
      buffer_size:       config.command_buffer_size,
      max_buffer_size:   config.max_command_buffer_size,
      clients:           Slab::with_capacity(1024),
      event_subscribers: Vec::new(),
      workers:           workers,
      next_id:           next_id as u32,
      state:             state,
      poll:              poll,
      config:            config,
      token_count:       token_count,
      must_stop:         false,
      executable_path:   path,
      backends_count:    backends_count,
      frontends_count:   frontends_count,
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
          let tokens: Vec<Token> = self.workers.iter().filter(|&(_, worker)| worker.can_handle_events()).map(|(token, _)| token.clone()).collect();

          if ! tokens.is_empty() {
            did_something = true;
          }

          //for (ref token, ref worker) in self.workers.iter() {
          //  let ids: Vec<&str> = worker.queue.iter().map(|msg| msg.id.as_str()).collect();
            //info!("worker {}, readiness = {:#?}, interest = {:#?}, queue = {}", token.0, worker.channel.readiness,
            //  worker.channel.interest, ids.len());
          //}

          //info!("will handle workers: {:?}", tokens);
          for token in tokens {
            self.handle_worker_events(token);
          }
        }

        self.run_executor();
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
        if let Some(ref mut worker) =self.workers.get_mut(&Token(i)) {
          worker.channel.handle_events(events);
          let uevent = UnixReady::from(events);
          if uevent.is_hup() {
            if worker.run_state != RunState::Stopped && worker.run_state != RunState::Stopping {
              worker.run_state = RunState::NotAnswering;
            }
            if worker.run_state == RunState::Stopping {
              worker.run_state = RunState::Stopped;
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

  pub fn run_executor(&mut self) {
    Executor::run();

    while let Some((client_token, answer)) = Executor::get_client_message() {
      self.clients.get_mut(client_token).map(|cl| cl.push_message(answer));
    }

    while let Some((worker_token, message)) = Executor::get_worker_message() {
      self.workers.get_mut(&worker_token).map(|w| w.push_message(message));
    }

    while let Some(state_change) = Executor::get_state_change() {
      match state_change {
        StateChange::StopWorker(token) => {
          self.workers.get_mut(&token).map(|w| w.run_state = RunState::Stopped);
        },
        StateChange::StopMaster => {
          self.must_stop = true;
        }
      }
    }
  }

  pub fn handle_worker_events(&mut self, token: Token) {
    if !self.workers.contains_key(&token) {
      return;
    }

    let mut messages = {
      let mut messages = Vec::new();
      let ref mut worker = self.workers.get_mut(&token).unwrap();
      loop {
        if !worker.queue.is_empty() {
          worker.channel.interest.insert(Ready::writable());
        }

        //trace!("worker[{}] readiness = {:#?}, interest = {:#?}, queue = {} messages", token.0, worker.channel.readiness,
        //  worker.channel.interest, worker.queue.len());

        if worker.channel.readiness() == Ready::empty() {
          break;
        }

        if worker.channel.readiness().is_readable() {
          let _ = worker.channel.readable().map_err(|e| {
            error!("could not read from worker socket: {:?}", e);
          });

          loop {
            if let Some(msg) = worker.channel.read_message() {
              messages.push(msg);
            } else {
              if (worker.channel.interest & worker.channel.readiness).is_readable() {
                let _ = worker.channel.readable().map_err(|e| {
                  error!("could not read from worker socket: {:?}", e);
                });
                continue;
              } else {
                break;
              }
            }
          }
        }

        if !worker.queue.is_empty() {
          worker.channel.interest.insert(Ready::writable());
        }

        if worker.channel.readiness.is_writable() {
          loop {
            if let Some(msg) = worker.queue.pop_front() {
              if !worker.channel.write_message(&msg) {
                worker.queue.push_front(msg);
              }
            }

            if worker.channel.back_buf.available_data() > 0 {
              let res = worker.channel.writable();
              if let Err(e) = res {
                error!("could not write to worker socket: {:?}", e);
                if worker.run_state != RunState::Stopped && worker.run_state != RunState::Stopping {
                  worker.run_state = RunState::NotAnswering;
                }
              }
            }

            if !worker.channel.readiness.is_writable() {
              break;
            }

            if worker.channel.back_buf.available_data() == 0 && worker.queue.len() == 0 {
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

    self.run_executor();

    let worker_run_state = self.workers.get(&token).as_ref().map(|worker| worker.run_state);
    if (self.config.worker_automatic_restart && worker_run_state == Some(RunState::NotAnswering)) || worker_run_state == Some(RunState::Stopping) {
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

        if let Some(pos) = self.event_subscribers.iter().position(|t| t == &conn_token) {
          let _ = self.event_subscribers.remove(pos);
        }

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
                    client.push_message(CommandResponse::new(
                      msg.id,
                      CommandStatus::Error,
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

            self.run_executor();
          }
        }
      }
    }
  }

  fn handle_worker_message(&mut self, token: Token, msg: ProxyResponse) {
    if let Some(ProxyResponseData::Event(data)) = msg.data {
      let event: command::Event = data.into();
      for client_token in self.event_subscribers.iter() {
        let event = CommandResponse::new(
          msg.id.to_string(),
          CommandStatus::Processing,
          format!("{}", token.0),
          Some(CommandResponseData::Event(event.clone()))
        );

        self.clients.get_mut(*client_token).map(|cl| cl.push_message(event));
      }
    } else {
      Executor::handle_message(token, msg);
    }
  }

  pub fn check_worker_status(&mut self, token: Token) {
    {
      let ref mut worker = self.workers.get_mut(&token).expect("there should be a worker at that token");
      let res = kill(Pid::from_raw(worker.pid), None);

      if let Ok(()) = res {
        if worker.run_state == RunState::NotAnswering {
          error!("worker process {} (PID = {}) not answering, killing and replacing", worker.id, worker.pid);
          if let Err(e) = kill(Pid::from_raw(worker.pid), Signal::SIGKILL) {
            error!("failed to kill the worker process: {:?}", e);
          } else {
            worker.run_state = RunState::Stopped;
          }
        } else {
          return;
        }

      } else {
        if worker.run_state == RunState::NotAnswering {
          error!("worker process {} (PID = {}) stopped running, replacing", worker.id, worker.pid);
        } else if worker.run_state == RunState::Stopping {
          info!("worker process {} (PID = {}) not detected, assuming it stopped", worker.id, worker.pid);
          worker.run_state = RunState::Stopped;
          let _ = self.poll.deregister(&worker.channel.sock);
          return;
        } else {
          error!("failed to check process status: {:?}", res);
          return;
        }
      }

      let _ = self.poll.deregister(&worker.channel.sock);
    }

    self.workers.remove(&token);

    if self.config.worker_automatic_restart {
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

        let mut count = 0;
        let mut orders = self.state.generate_activate_orders();
        for order in orders.drain(..) {
          worker.push_message(ProxyRequest {
            id: format!("RESTART-{}-ACTIVATE-{}", id, count),
            order
          });
          count += 1;
        }

        self.poll.register(&worker.channel.sock, Token(worker_token),
          Ready::readable() | Ready::writable() | UnixReady::error() | UnixReady::hup(),
          PollOpt::edge()).unwrap();
        worker.token = Some(Token(worker_token));
        self.workers.insert(Token(worker_token), worker);

      }
    }
  }
}

pub fn start(config: Config, command_socket_path: String, workers: Vec<Worker>) {
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

      let mut server = CommandServer::new(srv, config.clone(), workers, event_loop);

      server.load_static_application_configuration();

      saved_state.as_ref().map(|state_path| {
        server.load_state(None, "INITIALIZATION", state_path);
      });

      gauge!("configuration.applications", server.state.applications.len());
      gauge!("configuration.backends", server.backends_count);
      gauge!("configuration.frontends", server.frontends_count);

      info!("waiting for configuration client connections");
      server.run();
      //event_loop.run(&mut CommandServer::new(srv, workers, buffer_size, max_buffer_size)).unwrap()
    },
    Err(e) => {
      error!("could not create unix socket: {:?}", e);
      // the workers did not even get the configuration, we can kill them right away
      for worker in workers {
        error!("killing worker n°{} (PID {})", worker.id, worker.pid);
        let _ = kill(Pid::from_raw(worker.pid), Signal::SIGKILL).map_err(|e| {
          error!("could not kill worker: {:?}", e);
        });
      }
    }
  }
}
