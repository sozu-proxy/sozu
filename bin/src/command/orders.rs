use std::fs;
use std::str;
use std::process;
use std::io::Read;
use std::io::Write;
use std::thread::sleep;
use std::time::Duration;
use std::collections::{HashMap,HashSet};
use std::os::unix::io::{AsRawFd,FromRawFd};
use nix::fcntl::{fcntl,FcntlArg,FdFlag,FD_CLOEXEC};
use serde_json;
use slab::Slab;
use mio_uds::{UnixListener,UnixStream};
use mio::timer::{self,Timer};
use mio::{Poll,PollOpt,Ready,Token};
use nom::{HexDisplay,IResult,Offset};

use sozu::messages::Order;
use sozu::channel::Channel;
use sozu::network::ProxyOrder;
use sozu::network::buffer::Buffer;
use sozu_command::config::Config;
use sozu_command::state::ConfigState;
use sozu_command::data::{AnswerData,ConfigCommand,ConfigMessage,ConfigMessageAnswer,ConfigMessageStatus,PROTOCOL_VERSION,RunState,WorkerInfo};

use super::{CommandServer,FrontToken,Proxy,ProxyConfiguration,StoredProxy};
use super::client::parse;
use worker::start_worker;
use upgrade::{start_new_master_process,SerializedWorker,UpgradeData};

impl CommandServer {
  pub fn handle_client_message(&mut self, token: FrontToken, message: &ConfigMessage) {
    let config_command = message.data.clone();
    match config_command {
      ConfigCommand::SaveState(path) => {
        if let Ok(mut f) = fs::File::create(&path) {

          let mut counter = 0usize;
          for proxy in self.state.values() {
            for command in proxy.state.generate_orders() {
              let message = ConfigMessage::new(
                format!("SAVE-{}", counter),
                ConfigCommand::ProxyConfiguration(command),
                Some(proxy.tag.to_string()),
                None
              );
              f.write_all(&serde_json::to_string(&message).map(|s| s.into_bytes()).unwrap_or(vec!()));
              f.write_all(&b"\n\0"[..]);
              counter += 1;
            }
            f.sync_all();
          }
          self.conns[token].write_message(&ConfigMessageAnswer::new(
            message.id.clone(),
            ConfigMessageStatus::Ok,
            format!("saved to {}", path),
            None
          ));
        } else {
          error!("could not open file: {}", &path);
          self.conns[token].write_message(&ConfigMessageAnswer::new(
            message.id.clone(),
            ConfigMessageStatus::Error,
            "could not open file".to_string(),
            None
          ));
        }
      },
      ConfigCommand::DumpState => {

        let conf = ProxyConfiguration {
          id:      message.id.clone(),
          proxies: self.state.values().cloned().collect(),
        };
        //let encoded = serde_json::to_string(&conf).map(|s| s.into_bytes()).unwrap_or(vec!());
        self.conns[token].write_message(&ConfigMessageAnswer::new(
          message.id.clone(),
          ConfigMessageStatus::Ok,
          serde_json::to_string(&conf).unwrap_or(String::new()),
          None
        ));
        //self.conns[token].write_message(&encoded);
      },
      ConfigCommand::LoadState(path) => {
        self.load_state(&message.id, &path);
        self.conns[token].write_message(&ConfigMessageAnswer::new(
          message.id.clone(),
          ConfigMessageStatus::Ok,
          "loaded the configuration".to_string(),
          None
        ));
      },
      ConfigCommand::ListWorkers => {
        let workers: Vec<WorkerInfo> = self.proxies.values().map(|&(ref tag, ref proxy)| {
          WorkerInfo {
            tag:        proxy.tag.clone(),
            id:         proxy.id,
            proxy_type: proxy.proxy_type,
            pid:        proxy.pid,
            run_state:  proxy.run_state.clone(),
          }
        }).collect();
        self.conns[token].write_message(&ConfigMessageAnswer::new(
          message.id.clone(),
          ConfigMessageStatus::Ok,
          "".to_string(),
          Some(AnswerData::Workers(workers))
        ));
      },
      ConfigCommand::LaunchWorker(tag) => {
        info!("received LaunchWorker with tag \"{}\"", tag);

        let id = self.next_ids.get(&tag).unwrap_or(&0) + 1;
        if let Some(mut worker) = self.config.proxies.get(&tag).and_then(|config| start_worker(&tag, config, id)) {
          self.conns[token].write_message(&ConfigMessageAnswer::new(
            message.id.clone(),
            ConfigMessageStatus::Processing,
            "sending configuration orders".to_string(),
            None
          ));
          info!("created new worker");

          *self.next_ids.get_mut(&tag).unwrap() += 1;

          let worker_token = self.token_count + 1;
          self.token_count = worker_token;
          worker.token     = Some(Token(worker_token));

          if let Some(&(_, ref previous)) = self.proxies.values().filter(|&&(ref ptag, ref proxy)| {
            ptag == &tag && proxy.run_state == RunState::Running
          }).next() {
            worker.channel.set_blocking(true);

            let mut counter = 0u32;
            for order in previous.state.generate_orders() {
              let message_id = format!("LAUNCH-CONF-{}", counter);
              worker.inflight.insert(message_id.clone(), order.clone());
              let o = order.clone();
              //info!("sending to new worker({}-{}): {} ->  {:?}", tag, worker.id, message_id, order);
              self.conns[token].add_message_id(message_id.clone());
              worker.state.handle_order(&o);
              if !worker.channel.write_message(&ProxyOrder { id: message_id.clone(), order: o }) {
                error!("could not send to new worker({}-{}): {}", tag, worker.id, message_id);
              }

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
          self.proxies.insert(Token(worker_token), (tag, worker));

          self.conns[token].write_message(&ConfigMessageAnswer::new(
            message.id.clone(),
            ConfigMessageStatus::Ok,
            "".to_string(),
            None
          ));
        } else {
          self.conns[token].write_message(&ConfigMessageAnswer::new(
            message.id.clone(),
            ConfigMessageStatus::Error,
            "failed creating worker".to_string(),
            None
          ));
        }
      },
      ConfigCommand::UpgradeMaster => {
        self.disable_cloexec_before_upgrade();
        self.conns[token].channel.set_blocking(true);
        self.conns[token].write_message(&ConfigMessageAnswer::new(
          message.id.clone(),
          ConfigMessageStatus::Processing,
          "".to_string(),
          None
        ));
        let (pid, mut channel) = start_new_master_process(self.generate_upgrade_data());
        channel.set_blocking(true);
        let res = channel.read_message();
        info!("upgrade channel sent: {:?}", res);
        if let Some(true) = res {
          self.conns[token].write_message(&ConfigMessageAnswer::new(
            message.id.clone(),
            ConfigMessageStatus::Ok,
            "new master process launched, closing the old one".to_string(),
            None
          ));
          info!("wrote final message, closing");
          //FIXME: should do some cleanup before exiting
          sleep(Duration::from_secs(2));
          process::exit(0);
        } else {
          self.conns[token].write_message(&ConfigMessageAnswer::new(
            message.id.clone(),
            ConfigMessageStatus::Error,
            "could not upgrade master process".to_string(),
            None
          ));

        }
      },
      ConfigCommand::ProxyConfiguration(order) => {
        if let Some(ref tag) = message.proxy {
          if let &Order::AddTlsFront(ref data) = &order {
            info!("received AddTlsFront(TlsFront {{ app_id: {}, hostname: {}, path_begin: {} }}) with tag {:?}",
            data.app_id, data.hostname, data.path_begin, tag);
          } else {
            info!("received client order {:?} with tag {:?}", order, tag);
          }

          self.state.get_mut(tag).map(|st| st.state.handle_order(&order));

          let mut found = false;
          for &mut (ref proxy_tag, ref mut proxy) in self.proxies.values_mut() {
            if tag == proxy_tag {
              if let Some(id) = message.proxy_id {
                if id != proxy.id {
                  continue;
                }
              }

              if order == Order::SoftStop || order == Order::HardStop {
                proxy.run_state = RunState::Stopping;
              }

              proxy.inflight.insert(message.id.clone(), order.clone());
              let o = order.clone();
              self.conns[token].add_message_id(message.id.clone());
              proxy.state.handle_order(&o);
              proxy.channel.write_message(&ProxyOrder { id: message.id.clone(), order: o });
              proxy.channel.run();
              found = true;
            }
          }

          if !found {
            // FIXME: should send back error here
            error!("no proxy found for tag: {}", tag);
          }

        } else {
          // FIXME: should send back error here
          error!("expecting proxy tag");
        }
      }
    }
  }

  pub fn load_state(&mut self, message_id: &str, path: &str) {
    match fs::File::open(&path) {
      Err(e)   => error!("cannot open file at path '{}': {:?}", path, e),
      Ok(mut file) => {
        //let mut data = vec!();
        let mut buffer = Buffer::with_capacity(16384);
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
            IResult::Done(i, o) => {
              if i.len() > 0 {
                info!("could not parse {} bytes", i.len());
                if previous == buffer.available_data() {
                  break;
                }
              }
              offset = buffer.data().offset(i);

              for message in o {
                if let ConfigCommand::ProxyConfiguration(order) = message.data {

                  if let Some(ref tag) = message.proxy {
                    self.state.get_mut(tag).map(|st| st.state.handle_order(&order));

                    if let &Order::AddTlsFront(ref data) = &order {
                      info!("received AddTlsFront(TlsFront {{ app_id: {}, hostname: {}, path_begin: {} }}) with tag {:?}",
                      data.app_id, data.hostname, data.path_begin, tag);
                    } else {
                      info!("received {:?} with tag {:?}", order, tag);
                    }
                    let mut found = false;
                    for &mut (ref proxy_tag, ref mut proxy) in self.proxies.values_mut() {
                      if tag == proxy_tag {
                        let o = order.clone();
                        proxy.state.handle_order(&o);
                        proxy.channel.write_message(&ProxyOrder { id: message.id.clone(), order: o });
                        proxy.channel.run();
                        found = true;
                      }
                    }

                    if !found {
                      // FIXME: should send back error here
                      error!("no proxy found for tag: {}", tag);
                    }

                  } else {
                    // FIXME: should send back error here
                     error!("expecting proxy tag");
                  }
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
      }
    }
  }

  pub fn load_static_application_configuration(&mut self) {
    //FIXME: too many loops, this could be cleaner
    for message in self.config.generate_config_messages() {
      if let ConfigCommand::ProxyConfiguration(order) = message.data {
        if let Some(ref tag) = message.proxy {
          self.state.get_mut(tag).map(|st| st.state.handle_order(&order));

          if let &Order::AddTlsFront(ref data) = &order {
            info!("received AddTlsFront(TlsFront {{ app_id: {}, hostname: {}, path_begin: {} }}) with tag {:?}",
            data.app_id, data.hostname, data.path_begin, tag);
          } else {
            info!("received {:?} with tag {:?}", order, tag);
          }
          let mut found = false;
          for &mut (ref proxy_tag, ref mut proxy) in self.proxies.values_mut() {
            if tag == proxy_tag {
              let o = order.clone();
              proxy.state.handle_order(&o);
              proxy.channel.write_message(&ProxyOrder { id: message.id.clone(), order: o });
              proxy.channel.run();
              found = true;
            }
          }

          if !found {
            // FIXME: should send back error here
            error!("no proxy found for tag: {}", tag);
          }

        } else {
          // FIXME: should send back error here
          error!("expecting proxy tag");
        }
      }
    }
  }

  pub fn disable_cloexec_before_upgrade(&mut self) {
    for &mut (ref proxy_tag, ref mut proxy) in self.proxies.values_mut() {
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
    for &mut (ref proxy_tag, ref mut proxy) in self.proxies.values_mut() {
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
    let workers: Vec<SerializedWorker> = self.proxies.values().map(|&(_,ref proxy)| SerializedWorker::from_proxy(proxy)).collect();
    let mut seen = HashSet::new();
    let mut state: HashMap<String, StoredProxy> = HashMap::new();

    for &(ref tag, ref proxy) in  self.proxies.values() {
      if !seen.contains(&tag) {
        seen.insert(tag);
        state.insert(tag.to_string(), StoredProxy::from_proxy(&proxy) );
      }
    }

    UpgradeData {
      command:     self.sock.as_raw_fd(),
      config:      self.config.clone(),
      workers:     workers,
      state:       state,
      next_ids:    self.next_ids.clone(),
      token_count: self.token_count,
    }
  }

  pub fn from_upgrade_data(upgrade_data: UpgradeData) -> CommandServer {
    let poll = Poll::new().expect("should create poll object");
    let UpgradeData {
      command: command,
      config: config,
      workers: serialized_workers,
      state: state,
      next_ids: next_ids,
      token_count: token_count,
    } = upgrade_data;

    println!("listener is: {}", command);
    let listener = unsafe { UnixListener::from_raw_fd(command) };
    poll.register(&listener, Token(0), Ready::readable(), PollOpt::edge() | PollOpt::oneshot()).expect("should register listener correctly");


    let buffer_size     = config.command_buffer_size.unwrap_or(10000);
    let max_buffer_size = config.max_command_buffer_size.unwrap_or(buffer_size * 2);

    let workers: HashMap<Token, (String, Proxy)> = serialized_workers.iter().filter_map(|serialized| {
      let stream = unsafe { UnixStream::from_raw_fd(serialized.fd) };
      if let Some(token) = serialized.token {
        info!("registering: {:?}", poll.register(&stream, Token(token), Ready::all(), PollOpt::edge()));
        let worker_state = state.get(&serialized.tag).map(|ser| ser.state.clone()).expect("worker state should be there");
        Some(
          (
            Token(token),
            (serialized.tag.clone(), Proxy {
              tag:        serialized.tag.clone(),
              id:         serialized.id,
              proxy_type: serialized.proxy_type,
              channel:    Channel::new(stream, buffer_size, buffer_size * 2),
              token:      Some(Token(token)),
              pid:        serialized.pid,
              run_state:  serialized.run_state.clone(),
              state:      worker_state,
              //FIXME: transmit those as well?
              inflight:   HashMap::new()
            })
          )
        )
      } else { None }
    }).collect();

    let config_state: HashMap<String, StoredProxy> = state.values().map(|st| {
      (st.tag.to_string(),
        StoredProxy {
          tag:        st.tag.to_string(),
          proxy_type: st.proxy_type,
          state:      st.state.clone()
        }
      )
    }).collect();

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
      conns:           Slab::with_capacity(128),
      proxies:         workers,
      next_ids:        next_ids,
      state:           config_state,
      token_count:     token_count,
    }
  }
}
