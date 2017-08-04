use std::fs;
use std::str;
use std::process;
use std::io::Read;
use std::io::Write;
use std::iter::FromIterator;
use std::convert::Into;
use std::thread::sleep;
use std::time::Duration;
use std::collections::{HashMap,HashSet};
use std::os::unix::io::{AsRawFd,FromRawFd};
use nix::fcntl::{fcntl,FcntlArg,FdFlag,FD_CLOEXEC};
use slab::Slab;
use serde_json;
use mio_uds::{UnixListener,UnixStream};
use mio::timer;
use mio::{Poll,PollOpt,Ready,Token};
use nom::{HexDisplay,IResult,Offset};

use sozu::messages::{Order,OrderMessage};
use sozu::channel::Channel;
use sozu::network::buffer::Buffer;
use sozu_command::data::{AnswerData,ConfigCommand,ConfigMessage,ConfigMessageAnswer,ConfigMessageStatus,RunState,WorkerInfo};
use sozu_command::state::ConfigState;

use super::{CommandServer,FrontToken,ProxyConfiguration,Worker};
use super::client::parse;
use super::state::OrderState;
use worker::start_worker;
use upgrade::{start_new_master_process,SerializedWorker,UpgradeData};

impl CommandServer {
  pub fn handle_client_message(&mut self, token: FrontToken, message: &ConfigMessage) {
    //info!("handle_client_message: front token = {:?}, message = {:#?}", token, message);
    let config_command = message.data.clone();
    match config_command {
      ConfigCommand::SaveState(path) => {
        self.save_state(token, &message.id, &path);
      },
      ConfigCommand::DumpState => {
        self.dump_state(token, &message.id);
      },
      ConfigCommand::LoadState(path) => {
        self.load_state(Some(token), &message.id, &path);
        //self.answer_success(token, message.id.as_str(), "loaded the configuration", None);
      },
      ConfigCommand::ListWorkers => {
        self.list_workers(token, &message.id);
      },
      ConfigCommand::LaunchWorker(tag) => {
        info!("received LaunchWorker with tag \"{}\"", tag);
        self.launch_worker(token, message, &tag);
      },
      ConfigCommand::UpgradeMaster => {
        self.upgrade_master(token, &message.id);
      },
      ConfigCommand::ProxyConfiguration(order) => {
        self.worker_order(token, &message.id, order, message.proxy_id);
      }
    }
  }

  pub fn answer_success<T,U>(&mut self, token: FrontToken, id: T, message: U, data: Option<AnswerData>)
    where T: Clone+Into<String>,
          U: Clone+Into<String> {
    trace!("answer_success for front token {:?} id {}, message {:#?} data {:#?}", token, id.clone().into(), message.clone().into(), data);
    self.clients[token].push_message(ConfigMessageAnswer::new(
      id.into(),
      ConfigMessageStatus::Ok,
      message.into(),
      data
    ));
  }

  pub fn answer_error<T,U>(&mut self, token: FrontToken, id: T, message: U, data: Option<AnswerData>)
    where T: Clone+Into<String>,
          U: Clone+Into<String> {
    trace!("answer_error for front token {:?} id {}, message {:#?} data {:#?}", token, id.clone().into(), message.clone().into(), data);
    self.clients[token].push_message(ConfigMessageAnswer::new(
      id.into(),
      ConfigMessageStatus::Error,
      message.into(),
      data
    ));

  }

  pub fn save_state(&mut self, token: FrontToken, message_id: &str, path: &str) {
    if let Ok(mut f) = fs::File::create(&path) {

      let mut counter = 0usize;
      for command in self.state.generate_orders() {
        let message = ConfigMessage::new(
          format!("SAVE-{}", counter),
          ConfigCommand::ProxyConfiguration(command),
          None
          );
        f.write_all(&serde_json::to_string(&message).map(|s| s.into_bytes()).unwrap_or(vec!()));
        f.write_all(&b"\n\0"[..]);
        f.sync_all();
        counter += 1;
      }
      f.sync_all();
      self.answer_success(token, message_id, format!("saved {} config messages to {}", counter, path), None);
    } else {
      error!("could not open file: {}", &path);
      self.answer_error(token, message_id, "could not open file", None);
    }
  }

  pub fn dump_state(&mut self, token: FrontToken, message_id: &str) {
    let conf = ProxyConfiguration {
      id:    String::from(message_id),
      state: self.state.clone(),
    };
    self.answer_success(token, message_id, serde_json::to_string(&conf).unwrap_or(String::new()), None);
  }

  pub fn load_state(&mut self, token_opt: Option<FrontToken>, message_id: &str, path: &str) {
    match fs::File::open(&path) {
      Err(e)   => error!("cannot open file at path '{}': {:?}", path, e),
      Ok(mut file) => {
        //let mut data = vec!();
        let mut buffer = Buffer::with_capacity(200000);
        self.order_state.insert_task(message_id, token_opt);

        info!("starting to load state from {}", path);

        let mut counter = 0;
        loop {
          let previous = buffer.available_data();
          //FIXME: we should read in streaming here
          if let Ok(sz) = file.read(buffer.space()) {
            buffer.fill(sz);
          } else {
            error!("error reading state file");
            break;
          }

          if buffer.available_data() == 0 {
            break;
          }


          let mut offset = 0;
          match parse(buffer.data()) {
            IResult::Done(i, orders) => {
              if i.len() > 0 {
                //info!("could not parse {} bytes", i.len());
                if previous == buffer.available_data() {
                  break;
                }
              }
              offset = buffer.data().offset(i);

              let mut new_state = self.state.clone();
              for message in orders {
                if let ConfigCommand::ProxyConfiguration(order) = message.data {
                  new_state.handle_order(&order);
                }
              }

              let diff = self.state.diff(&new_state);
              for order in diff {
                self.state.handle_order(&order);

                /* if let &Order::AddHttpsFront(ref data) = &order {
                  info!("load state AddHttpsFront(HttpsFront {{ app_id: {}, hostname: {}, path_begin: {} }})",
                    data.app_id, data.hostname, data.path_begin);
                } else {
                  info!("load state {:?}", order);
                } */

                let mut found = false;
                let id = format!("LOAD-STATE-{}-{}", message_id, counter);

                for ref mut proxy in self.proxies.values_mut() {
                  let o = order.clone();
                  proxy.push_message(OrderMessage { id: id.clone(), order: o });
                  self.order_state.insert_worker_message(message_id, &id, proxy.token.expect("worker should have a token"));
                  found = true;

                }
                counter += 1;

                if !found {
                  // FIXME: should send back error here
                  error!("no proxy found");
                }
              }
            },
            IResult::Incomplete(_) => {
              if buffer.available_data() == buffer.capacity() {
                error!("message too big, stopping parsing:\n{}", buffer.data().to_hex(16));
                break;
              }
            }
            IResult::Error(e) => {
              error!("saved state parse error: {:?}", e);
              break;
            },
          }
          buffer.consume(offset);
        }
        if counter > 0 {
        info!("state loaded from {}, will start sending {} ùessages to workers", path, counter);
        } else {
          info!("no messages sent to workers: local state already had those messages");
          if let Some(task) = self.order_state.task(message_id) {
            if let Some(token) = token_opt {
              let answer = ConfigMessageAnswer::new(
                message_id.to_string(),
                ConfigMessageStatus::Ok,
                format!("ok: 0 messages, error: 0"),
                None
              );
              self.clients[token].push_message(answer);
            }

          }
        }
      }
    }
  }

  pub fn list_workers(&mut self, token: FrontToken, message_id: &str) {
    let workers: Vec<WorkerInfo> = self.proxies.values().map(|ref proxy| {
      WorkerInfo {
        id:         proxy.id,
        pid:        proxy.pid,
        run_state:  proxy.run_state.clone(),
      }
    }).collect();
    self.answer_success(token, message_id, "", Some(AnswerData::Workers(workers)));
  }

  pub fn launch_worker(&mut self, token: FrontToken, message: &ConfigMessage, tag: &str) {
    let id = self.next_id + 1;
    if let Ok(mut worker) = start_worker(id, &self.config) {
      self.clients[token].push_message(ConfigMessageAnswer::new(
          message.id.clone(),
          ConfigMessageStatus::Processing,
          "sending configuration orders".to_string(),
          None
          ));
      info!("created new worker");

      self.next_id += 1;

      let worker_token = self.token_count + 1;
      self.token_count = worker_token;
      worker.token     = Some(Token(worker_token));

      if let Some(ref previous) = self.proxies.values().filter(|ref proxy| {
        proxy.run_state == RunState::Running
      }).next() {
        worker.channel.set_blocking(true);

        self.order_state.insert_task(message.id.as_str(), Some(token));
        let mut counter = 0u32;
        for order in self.state.generate_orders() {
          let worker_message_id = format!("LAUNCH-CONF-{}", counter);
          self.order_state.insert_worker_message(message.id.as_str(), worker_message_id.as_str(), Token(worker_token));

          let o = order.clone();
          //info!("sending to new worker({}-{}): {} ->  {:?}", tag, worker.id, message_id, order);
          self.clients[token].add_message_id(worker_message_id.clone());
          worker.push_message(OrderMessage { id: worker_message_id.clone(), order: o });

          let received = worker.channel.read_message();
          info!("worker ({}-{}) sent: {:?}", tag, worker.id, received);
          //worker.channel.run();
          counter += 1;
        }
        worker.channel.set_blocking(false);
      }

      info!("registering new sock {:?} at token {:?} for tag {} and id {} (sock error: {:?})", worker.channel.sock,
      worker_token, tag, worker.id, worker.channel.sock.take_error());
      self.poll.register(&worker.channel.sock, Token(worker_token), Ready::all(), PollOpt::edge()).unwrap();
      worker.token = Some(Token(worker_token));
      self.proxies.insert(Token(worker_token), worker);

      self.answer_success(token, message.id.as_str(), "", None);
    } else {
      self.answer_error(token, message.id.as_str(), "failed creating worker", None);
    }
  }

  pub fn upgrade_master(&mut self, token: FrontToken, message_id: &str) {
    self.disable_cloexec_before_upgrade();
    //FIXME: do we need to be blocking here?
    self.clients[token].channel.set_blocking(true);
    self.clients[token].channel.write_message(&ConfigMessageAnswer::new(
        String::from(message_id),
        ConfigMessageStatus::Processing,
        "".to_string(),
        None
        ));
    let (pid, mut channel) = start_new_master_process(self.generate_upgrade_data());
    channel.set_blocking(true);
    let res = channel.read_message();
    info!("upgrade channel sent: {:?}", res);
    if let Some(true) = res {
      self.answer_success(token, message_id, "new master process launched, closing the old one", None);
      info!("wrote final message, closing");
      //FIXME: should do some cleanup before exiting
      sleep(Duration::from_secs(2));
      process::exit(0);
    } else {
      self.answer_error(token, message_id, "could not upgrade master process", None);
    }
  }

  pub fn worker_order(&mut self, token: FrontToken, message_id: &str, order: Order, proxy_id: Option<u32>) {
    if let &Order::AddHttpsFront(ref data) = &order {
      info!("proxyconfig client order AddHttpsFront(HttpsFront {{ app_id: {}, hostname: {}, path_begin: {} }})",
      data.app_id, data.hostname, data.path_begin);
    } else {
      info!("proxyconfig client order {:?}", order);
    }

    self.state.handle_order(&order);
    self.order_state.insert_task(message_id, Some(token));

    let mut found = false;
    for ref mut proxy in self.proxies.values_mut() {
      if let Some(id) = proxy_id {
        if id != proxy.id {
          continue;
        }
      }

      if order == Order::SoftStop || order == Order::HardStop {
        proxy.run_state = RunState::Stopping;
      }


      self.order_state.insert_worker_message(message_id, message_id, proxy.token.expect("worker should have a valid token"));
      trace!("sending to {:?}, inflight is now {:#?}", proxy.token.expect("worker should have a valid token").0, self.order_state);

      let o = order.clone();
      self.clients[token].add_message_id(String::from(message_id));
      proxy.push_message(OrderMessage { id: String::from(message_id), order: o });
      found = true;
    }

    if !found {
      // FIXME: should send back error here
      error!("no proxy found");
    }
  }

  pub fn load_static_application_configuration(&mut self) {
    //FIXME: too many loops, this could be cleaner
    for message in self.config.generate_config_messages() {
      if let ConfigCommand::ProxyConfiguration(order) = message.data {
        self.state.handle_order(&order);

        if let &Order::AddHttpsFront(ref data) = &order {
          info!("config generated AddHttpsFront(HttpsFront {{ app_id: {}, hostname: {}, path_begin: {} }})",
          data.app_id, data.hostname, data.path_begin);
        } else {
          info!("config generated {:?}", order);
        }
        let mut found = false;
        for ref mut proxy in self.proxies.values_mut() {
          let o = order.clone();
          proxy.push_message(OrderMessage { id: message.id.clone(), order: o });
          found = true;
        }

        if !found {
          // FIXME: should send back error here
          error!("no proxy found");
        }
      }
    }
  }

  pub fn disable_cloexec_before_upgrade(&mut self) {
    for ref mut proxy in self.proxies.values() {
      if proxy.run_state == RunState::Running {
        let flags = fcntl(proxy.channel.sock.as_raw_fd(), FcntlArg::F_GETFD).unwrap();
        let mut new_flags = FdFlag::from_bits(flags).unwrap();
        new_flags.remove(FD_CLOEXEC);
        fcntl(proxy.channel.sock.as_raw_fd(), FcntlArg::F_SETFD(new_flags));
      }
    }
    info!("disabling cloexec on listener: {}", self.sock.as_raw_fd());
    let flags = fcntl(self.sock.as_raw_fd(), FcntlArg::F_GETFD).unwrap();
    let mut new_flags = FdFlag::from_bits(flags).unwrap();
    new_flags.remove(FD_CLOEXEC);
    fcntl(self.sock.as_raw_fd(), FcntlArg::F_SETFD(new_flags));
  }

  pub fn enable_cloexec_after_upgrade(&mut self) {
    for ref mut proxy in self.proxies.values() {
      if proxy.run_state == RunState::Running {
        let flags = fcntl(proxy.channel.sock.as_raw_fd(), FcntlArg::F_GETFD).unwrap();
        let mut new_flags = FdFlag::from_bits(flags).unwrap();
        new_flags.insert(FD_CLOEXEC);
        fcntl(proxy.channel.sock.as_raw_fd(), FcntlArg::F_SETFD(new_flags));
      }
    }
    let flags = fcntl(self.sock.as_raw_fd(), FcntlArg::F_GETFD).unwrap();
    let mut new_flags = FdFlag::from_bits(flags).unwrap();
    new_flags.insert(FD_CLOEXEC);
    fcntl(self.sock.as_raw_fd(), FcntlArg::F_SETFD(new_flags));
  }

  pub fn generate_upgrade_data(&self) -> UpgradeData {
    let workers: Vec<SerializedWorker> = self.proxies.values().map(|ref proxy| SerializedWorker::from_proxy(proxy)).collect();
    //FIXME: ensure there's at least one worker
    let state = self.state.clone();

    UpgradeData {
      command:     self.sock.as_raw_fd(),
      config:      self.config.clone(),
      workers:     workers,
      state:       state,
      next_id:     self.next_id,
      token_count: self.token_count,
      //order_state: self.order_state.state.clone(),
    }
  }

  pub fn from_upgrade_data(upgrade_data: UpgradeData) -> CommandServer {
    let poll = Poll::new().expect("should create poll object");
    let UpgradeData {
      command,
      config,
      workers,
      state,
      next_id,
      token_count,
      //order_state,
    } = upgrade_data;

    println!("listener is: {}", command);
    let listener = unsafe { UnixListener::from_raw_fd(command) };
    poll.register(&listener, Token(0), Ready::readable(), PollOpt::edge() | PollOpt::oneshot()).expect("should register listener correctly");


    let buffer_size     = config.command_buffer_size.unwrap_or(10000);
    let max_buffer_size = config.max_command_buffer_size.unwrap_or(buffer_size * 2);

    let workers: HashMap<Token, Worker> = workers.iter().filter_map(|serialized| {
      let stream = unsafe { UnixStream::from_raw_fd(serialized.fd) };
      if let Some(token) = serialized.token {
        info!("registering: {:?}", poll.register(&stream, Token(token), Ready::all(), PollOpt::edge()));
        let worker_state = state.clone();
        Some(
          (
            Token(token),
            Worker {
              id:         serialized.id,
              channel:    Channel::new(stream, buffer_size, buffer_size * 2),
              token:      Some(Token(token)),
              pid:        serialized.pid,
              run_state:  serialized.run_state.clone(),
              queue:      serialized.queue.clone().into(),
            }
          )
        )
      } else { None }
    }).collect();

    let config_state = state.clone();

    let mut timer = timer::Timer::default();
    timer.set_timeout(Duration::from_millis(700), Token(0));

    CommandServer {
      sock:            listener,
      poll:            poll,
      timer:           timer,
      config:          config,
      buffer_size:     buffer_size,
      max_buffer_size: max_buffer_size,
      //FIXME: deserialize client connections as well, otherwise they might leak?
      clients:         Slab::with_capacity(128),
      proxies:         workers,
      next_id:         next_id,
      state:           config_state,
      token_count:     token_count,

      //FIXME: deserialize this as well
      order_state:     OrderState::new(),
      must_stop:       false,
    }
  }
}
