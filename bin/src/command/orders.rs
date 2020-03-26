use std::fs;
use std::str;
use std::process;
use std::io::{self,Read,Write};
use std::convert::Into;
use std::thread::sleep;
use std::time::Duration;
use std::collections::{HashMap,BTreeMap};
use std::os::unix::io::{AsRawFd,FromRawFd};
use slab::Slab;
use serde_json;
use mio::unix::UnixReady;
use mio_uds::{UnixListener,UnixStream};
use mio::{Poll,PollOpt,Ready,Token};
use nom::{Err,HexDisplay,Offset};

use sozu_command::buffer::fixed::Buffer;
use sozu_command::channel::Channel;
use sozu_command::scm_socket::{Listeners, ScmSocket};
use sozu_command::proxy::{ProxyRequestData, ProxyRequest, Query, QueryAnswer,
  QueryApplicationType, MetricsData, AggregatedMetricsData, ProxyResponseData,
  HttpFrontend, TcpFrontend, Route, ProxyResponseStatus};
use sozu_command::command::{CommandResponseData,CommandRequestData,
  CommandRequest,CommandResponse,CommandStatus,RunState,WorkerInfo};
use sozu_command::state::get_application_ids_by_domain;
use sozu_command::logging;
use sozu::metrics::METRICS;

use super::{CommandServer,FrontToken,Worker};
use super::client::parse;
use worker::{start_worker,get_executable_path};
use upgrade::{start_new_master_process,SerializedWorker,UpgradeData};
use util;

use super::executor;
use futures::future::join_all;
use futures::Future;

impl CommandServer {
  pub fn handle_client_message(&mut self, token: FrontToken, message: &CommandRequest) {
    //info!("handle_client_message: front token = {:?}, message = {:#?}", token, message);
    let config_command = message.data.clone();
    match config_command {
      CommandRequestData::SaveState { path } => {
        self.save_state(token, &message.id, &path);
      },
      CommandRequestData::DumpState => {
        self.dump_state(token, &message.id);
      },
      CommandRequestData::LoadState { path } => {
        self.load_state(Some(token), &message.id, &path);
        //self.answer_success(token, message.id.as_str(), "loaded the configuration", None);
      },
      CommandRequestData::ListWorkers => {
        self.list_workers(token, &message.id);
      },
      CommandRequestData::LaunchWorker(tag) => {
        self.launch_worker(token, message, &tag);
      },
      CommandRequestData::UpgradeMaster => {
        self.upgrade_master(token, &message.id);
      },
      CommandRequestData::Proxy(order) => {
        match order {
          ProxyRequestData::Metrics => self.metrics(token, &message.id),
          ProxyRequestData::Query(query) => self.query(token, &message.id, query),
          order => {
            self.worker_order(token, &message.id, order, message.worker_id);
          }
        };
      },
      CommandRequestData::UpgradeWorker(id) => {
        self.upgrade_worker(token, &message.id, id);
      },
      CommandRequestData::SubscribeEvents => {
        self.event_subscribers.push(token);
      },
    }
  }

  pub fn answer_success<T,U>(&mut self, token: FrontToken, id: T, message: U, data: Option<CommandResponseData>)
    where T: Clone+Into<String>,
          U: Clone+Into<String> {
    trace!("answer_success for front token {:?} id {}, message {:#?} data {:#?}", token, id.clone().into(), message.clone().into(), data);
    self.clients[token].push_message(CommandResponse::new(
      id.into(),
      CommandStatus::Ok,
      message.into(),
      data
    ));
  }

  pub fn answer_error<T,U>(&mut self, token: FrontToken, id: T, message: U, data: Option<CommandResponseData>)
    where T: Clone+Into<String>,
          U: Clone+Into<String> {
    trace!("answer_error for front token {:?} id {}, message {:#?} data {:#?}", token, id.clone().into(), message.clone().into(), data);
    self.clients[token].push_message(CommandResponse::new(
      id.into(),
      CommandStatus::Error,
      message.into(),
      data
    ));

  }

  pub fn save_state(&mut self, token: FrontToken, message_id: &str, path: &str) {
    if let Ok(mut f) = fs::File::create(&path) {

      let res = self.save_state_to_file(&mut f);

      match res {
        Ok(counter) => {
          info!("wrote {} commands to {}", counter, path);
          self.answer_success(token, message_id, format!("saved {} config messages to {}", counter, path), None);
        },
        Err(e) => {
          error!("failed writing state to file: {:?}", e);
          self.answer_error(token, message_id, "could not save state to file", None);
        }
      }
    } else {
      error!("could not open file: {}", &path);
      self.answer_error(token, message_id, "could not open file", None);
    }
  }

  pub fn save_state_to_file(&mut self, f: &mut fs::File) -> io::Result<usize> {
    let mut counter = 0usize;
    let orders = self.state.generate_orders();

    let res: io::Result<usize> = (move || {
      for command in orders {
        let message = CommandRequest::new(
          format!("SAVE-{}", counter),
          CommandRequestData::Proxy(command),
          None
        );

        f.write_all(&serde_json::to_string(&message).map(|s| s.into_bytes()).unwrap_or(vec!()))?;
        f.write_all(&b"\n\0"[..])?;

        if counter % 1000 == 0 {
          info!("writing command {}", counter);
          f.sync_all()?;
        }
        counter += 1;
      }
      f.sync_all()?;

      Ok(counter)
    })();

    res
  }

  pub fn dump_state(&mut self, token: FrontToken, message_id: &str) {
    let state = self.state.clone();
    self.answer_success(token, message_id, String::new(), Some(CommandResponseData::State(state)));
  }

  pub fn load_state(&mut self, token_opt: Option<FrontToken>, message_id: &str, path: &str) {
    match fs::File::open(&path) {
      Err(e)   => {
        error!("cannot open file at path '{}': {:?}", path, e);
        if let Some(token) = token_opt {
          self.answer_error(token, message_id, format!("cannot open file at path '{}': {:?}", path, e), None);
        }
      },
      Ok(mut file) => {
        let mut buffer = Buffer::with_capacity(200000);

        info!("starting to load state from {}", path);

        let mut message_counter = 0;
        let mut diff_counter = 0;

        let mut futures = Vec::new();
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
            Ok((i, orders)) => {
              if i.len() > 0 {
                //info!("could not parse {} bytes", i.len());
                if previous == buffer.available_data() {
                  error!("error consuming load state message");
                  break;
                }
              }
              offset = buffer.data().offset(i);

              if orders.iter().find(|o| {
                if o.version > sozu_command::command::PROTOCOL_VERSION {
                  error!("configuration protocol version mismatch: Sōzu handles up to version {}, the message uses version {}", sozu_command::command::PROTOCOL_VERSION, o.version);
                  true
                } else {
                  false
                }
              }).is_some() {
                break;
              }

              let mut new_state = self.state.clone();
              for message in orders {
                if let CommandRequestData::Proxy(order) = message.data {
                  message_counter += 1;
                  new_state.handle_order(&order);
                }
              }

              let diff = self.state.diff(&new_state);
              for order in diff {
                diff_counter += 1;
                self.state.handle_order(&order);

                let mut found = false;
                let id = format!("LOAD-STATE-{}-{}", message_id, diff_counter);

                for ref mut worker in self.workers.values_mut()
                  .filter(|worker| worker.run_state != RunState::Stopping && worker.run_state != RunState::Stopped) {
                  let o = order.clone();
                  futures.push(
                    executor::send(worker.token.expect("worker should have a token"), ProxyRequest { id: id.clone(), order: o })
                  );
                  found = true;

                }

                if !found {
                  // FIXME: should send back error here
                  error!("no worker found");
                }
              }
            },
            Err(Err::Incomplete(_)) => {
              if buffer.available_data() == buffer.capacity() {
                error!("message too big, stopping parsing:\n{}", buffer.data().to_hex(16));
                break;
              }
            }
            Err(e) => {
              error!("saved state parse error: {:?}", e);
              break;
            },
          }
          buffer.consume(offset);
        }

        error!("stopped loading data from file, remaining: {} bytes, saw {} messages, generated {} diff messages",
          buffer.available_data(), message_counter, diff_counter);
        if diff_counter > 0 {
          info!("state loaded from {}, will start sending {} messages to workers", path, diff_counter);
          let id = message_id.to_string();
          executor::Executor::execute(
            //FIXME: join_all will stop at the first error, and we will end up accumulating messages
            join_all(futures).map(move |v| {
              info!("load_state: {} messages loaded", v.len());
              if let Some(token) = token_opt {
                executor::Executor::send_client(token, CommandResponse::new(
                  id,
                  CommandStatus::Ok,
                  format!("ok: {} messages, error: 0", v.len()),
                  None
                ));
              }
            }).map_err(|e| {
              error!("load_state error: {}", e);
            })
          );
        } else {
          info!("no messages sent to workers: local state already had those messages");
          if let Some(token) = token_opt {
            let answer = CommandResponse::new(
              message_id.to_string(),
              CommandStatus::Ok,
              format!("ok: 0 messages, error: 0"),
              None
            );
            self.clients[token].push_message(answer);
          }
        }

        self.backends_count = self.state.count_backends();
        self.frontends_count = self.state.count_frontends();
        gauge!("configuration.clusters", self.state.clusters.len());
        gauge!("configuration.backends", self.backends_count);
        gauge!("configuration.frontends", self.frontends_count);
      }
    }
  }

  pub fn list_workers(&mut self, token: FrontToken, message_id: &str) {
    let workers: Vec<WorkerInfo> = self.workers.values().map(|ref worker| {
      WorkerInfo {
        id:         worker.id,
        pid:        worker.pid,
        run_state:  worker.run_state.clone(),
      }
    }).collect();
    self.answer_success(token, message_id, "", Some(CommandResponseData::Workers(workers)));
  }

  pub fn launch_worker(&mut self, token: FrontToken, message: &CommandRequest, tag: &str) {
    let id = self.next_id;
    if let Ok(mut worker) = start_worker(id, &self.config, self.executable_path.clone(), &self.state, None) {
      self.clients[token].push_message(CommandResponse::new(
          message.id.clone(),
          CommandStatus::Processing,
          "sending configuration orders".to_string(),
          None
          ));
      info!("created new worker: {}", id);

      self.next_id += 1;

      let worker_token = self.token_count + 1;
      self.token_count = worker_token;
      worker.token     = Some(Token(worker_token));

      debug!("registering new sock {:?} at token {:?} for tag {} and id {} (sock error: {:?})", worker.channel.sock,
      worker_token, tag, worker.id, worker.channel.sock.take_error());
      self.poll.register(&worker.channel.sock, Token(worker_token),
        Ready::readable() | Ready::writable() | UnixReady::error() | UnixReady::hup(),
        PollOpt::edge()).unwrap();
      worker.token = Some(Token(worker_token));

      info!("sending listeners: to the new worker: {:?}", worker.scm.send_listeners(&Listeners {
        http: Vec::new(),
        tls:  Vec::new(),
        tcp:  Vec::new(),
      }));

      let activate_orders = self.state.generate_activate_orders();
      let mut count = 0;
      for order in activate_orders.into_iter() {
        worker.push_message(ProxyRequest {
          id: format!("{}-ACTIVATE-{}", id, count),
          order
        });
        count += 1;
      }

      self.workers.insert(Token(worker_token), worker);

      self.answer_success(token, message.id.as_str(), "", None);
    } else {
      self.answer_error(token, message.id.as_str(), "failed creating worker", None);
    }
  }

  pub fn upgrade_worker(&mut self, token: FrontToken, message_id: &str, id: u32) {
    info!("client[{}] msg {} wants to upgrade worker {}", token.0, message_id, id);

    // same as launch_worker
    let next_id = self.next_id;
    let worker_token = self.token_count + 1;
    let mut worker = if let Ok(mut worker) = start_worker(next_id, &self.config, self.executable_path.clone(), &self.state, None) {
      self.clients[token].push_message(CommandResponse::new(
          String::from(message_id),
          CommandStatus::Processing,
          "sending configuration orders".to_string(),
          None
          ));
      info!("created new worker: {}", next_id);

      self.next_id += 1;

      self.token_count = worker_token;
      worker.token     = Some(Token(worker_token));

      debug!("registering new sock {:?} at token {:?} for tag {} and id {} (sock error: {:?})", worker.channel.sock,
      worker_token, "upgrade", worker.id, worker.channel.sock.take_error());
      self.poll.register(&worker.channel.sock, Token(worker_token),
        Ready::readable() | Ready::writable() | UnixReady::error() | UnixReady::hup(),
        PollOpt::edge()).unwrap();
      worker.token = Some(Token(worker_token));

      worker
    } else {
      return self.answer_error(token, message_id, "failed creating worker", None);
    };

    if self.workers.values().find(|worker| {
      worker.id == id && worker.run_state != RunState::Stopping && worker.run_state != RunState::Stopped
    }).is_none() {
      self.answer_error(token, message_id, "worker not found", None);
      return;
    }

    let mut listeners = None;
    {
      let old_worker = self.workers.values_mut().filter(|worker| worker.id == id).next().unwrap();

      old_worker.channel.set_blocking(true);
      old_worker.channel.write_message(&ProxyRequest { id: String::from(message_id), order: ProxyRequestData::ReturnListenSockets });
      info!("sent returnlistensockets message to worker");
      old_worker.channel.set_blocking(false);

      let mut counter   = 0;

      loop {
        old_worker.scm.set_blocking(true);
        if let Some(l) = old_worker.scm.receive_listeners() {
          listeners = Some(l);
          break;
        } else {
          counter += 1;
          if counter == 50 {
            break;
          }
          sleep(Duration::from_millis(100));
        }
      }
      old_worker.run_state = RunState::Stopping;
      let old_worker_token = old_worker.token.expect("worker should have a valid token");
      executor::Executor::execute(
        executor::send(
          old_worker_token,
          ProxyRequest { id: message_id.to_string(), order: ProxyRequestData::SoftStop })
        .map(move |_| {
          executor::Executor::stop_worker(old_worker_token)
        }).map_err(|s| {
          error!("error stopping worker: {:?}", s);
        })
      );
    }

    match listeners {
      Some(l) => {
        info!("sending listeners: to the new worker: {:?}", worker.scm.send_listeners(&l));
        l.close();
      },
      None => error!("could not get the list of listeners from the previous worker"),
    };
    let activate_orders = self.state.generate_activate_orders();
    let mut count = 0;
    for order in activate_orders.into_iter() {
      worker.push_message(ProxyRequest {
        id: format!("{}-ACTIVATE-{}", message_id, count),
        order
      });
      count += 1;
    }
    self.workers.insert(Token(worker_token), worker);

    self.answer_success(token, message_id, "", None);
  }

  pub fn upgrade_master(&mut self, token: FrontToken, message_id: &str) {
    self.disable_cloexec_before_upgrade();
    //FIXME: do we need to be blocking here?
    self.clients[token].channel.set_blocking(true);
    self.clients[token].channel.write_message(&CommandResponse::new(
        String::from(message_id),
        CommandStatus::Processing,
        "".to_string(),
        None
        ));
    let (pid, mut channel) = start_new_master_process(self.executable_path.clone(), self.generate_upgrade_data());
    channel.set_blocking(true);
    let res = channel.read_message();
    debug!("upgrade channel sent: {:?}", res);
    if let Some(true) = res {
      self.clients[token].channel.write_message(&CommandResponse::new(
        message_id.into(),
        CommandStatus::Ok,
        format!("new master process launched with pid {}, closing the old one", pid),
        None
      ));
      info!("wrote final message, closing");
      //FIXME: should do some cleanup before exiting
      sleep(Duration::from_secs(2));
      process::exit(0);
    } else {
      self.answer_error(token, message_id, "could not upgrade master process", None);
    }
  }

  pub fn metrics(&mut self, token: FrontToken, message_id: &str) {
    let mut futures = Vec::new();
    let id = message_id.to_string();

    for ref mut worker in self.workers.values_mut()
      .filter(|worker| worker.run_state != RunState::Stopped) {

      let tag = worker.id.to_string();
      futures.push(
        executor::send(
          worker.token.expect("worker should have a token"),
          ProxyRequest { id: id.clone(), order: ProxyRequestData::Metrics }).map(|data| (tag, data))
      );
    }

    let master_metrics = METRICS.with(|metrics| {
      (*metrics.borrow_mut()).dump_process_data()
    });

    executor::Executor::execute(
      //FIXME: join_all will stop at the first error, and we will end up accumulating messages
      join_all(futures).map(move |v| {
        let data: BTreeMap<String, MetricsData> = v.into_iter().filter_map(|(tag, metrics)| {
          if let Some(ProxyResponseData::Metrics(d)) = metrics.data {
            Some((tag, d))
          } else {
            None
          }
        }).collect();

        let aggregated_data = AggregatedMetricsData {
          master: master_metrics,
          workers: data,
        };

        executor::Executor::send_client(token, CommandResponse::new(
          id,
          CommandStatus::Ok,
          String::new(),
          Some(CommandResponseData::Metrics(aggregated_data))
        ));
      }).map_err(|e| {
        error!("metrics error: {}", e);
      })
    );
  }

  pub fn query(&mut self, token: FrontToken, message_id: &str, query: Query) {
    let id = message_id.to_string();
    let mut futures = Vec::new();
    for ref mut worker in self.workers.values_mut()
      .filter(|worker| worker.run_state != RunState::Stopped) {

      let tag = worker.id.to_string();
      futures.push(
        executor::send(
          worker.token.expect("worker should have a token"),
          ProxyRequest { id: id.clone(), order: ProxyRequestData::Query(query.clone()) }).map(|data| (tag, data))
      );

    }

    let f = join_all(futures).map(move |v| {
      let data: BTreeMap<String, QueryAnswer> = v.into_iter().filter_map(|(tag, query)| {
        if let Some(ProxyResponseData::Query(d)) = query.data {
          Some((tag, d))
        } else {
          None
        }
      }).collect();
      data
    });

    match &query {
      &Query::ApplicationsHashes => {
        let master = QueryAnswer::ApplicationsHashes(self.state.hash_state());

        executor::Executor::execute(f.map(move |mut data| {
          data.insert(String::from("master"), master);

          executor::Executor::send_client(token, CommandResponse::new(
            id,
            CommandStatus::Ok,
            String::new(),
            Some(CommandResponseData::Query(data))
          ));
        }).map_err(|e| {
          //FIXME: send back errors
          error!("metrics error: {}", e);
        }));
      },
      &Query::Applications(ref query_type) => {
        let master = match query_type {
          QueryApplicationType::ClusterId(ref cluster_id) => vec!(self.state.application_state(cluster_id)),
          QueryApplicationType::Domain(ref domain) => {
            let cluster_ids = get_application_ids_by_domain(&self.state, domain.hostname.clone(), domain.path.clone());
            cluster_ids.iter().map(|ref cluster_id| self.state.application_state(cluster_id)).collect()
          }
        };

        executor::Executor::execute(f.map(move |mut data| {
          data.insert(String::from("master"), QueryAnswer::Applications(master));

          executor::Executor::send_client(token, CommandResponse::new(
            id,
            CommandStatus::Ok,
            String::new(),
            Some(CommandResponseData::Query(data))
          ));
        }).map_err(|e| {
          //FIXME: send back errors
          error!("metrics error: {}", e);
        }));
      },
      &Query::Certificates(_) => {
        executor::Executor::execute(f.map(move |data| {
          info!("certificates query received: {:?}", data);

          executor::Executor::send_client(token, CommandResponse::new(
            id,
            CommandStatus::Ok,
            String::new(),
            Some(CommandResponseData::Query(data))
          ));
        }).map_err(|e| {
          //FIXME: send back errors
          error!("certificates query error: {}", e);
        }));
      },
    };
  }

  pub fn worker_order(&mut self, token: FrontToken, message_id: &str, order: ProxyRequestData, worker_id: Option<u32>) {
    if let &ProxyRequestData::AddCertificate(_) = &order {
      debug!("workerconfig client order AddCertificate()");
    } else {
      debug!("workerconfig client order {:?}", order);
    }

    if let &ProxyRequestData::Logging(ref logging_filter) = &order {
      debug!("Changing master log level to {}", logging_filter);
      logging::LOGGER.with(|l| {
        let directives = logging::parse_logging_spec(&logging_filter);
        l.borrow_mut().set_directives(directives);
      });
      // also change / set the content of RUST_LOG so future workers / main thread
      // will have the new logging filter value
      ::std::env::set_var("RUST_LOG", logging_filter);
    }

    if !self.state.handle_order(&order) {
      // Check if the backend or frontend exist before deleting it
      if worker_id.is_none() {
        match order {
          ProxyRequestData::RemoveBackend(ref backend) => {
            let msg = format!("No such backend {} at {} for the cluster {}", backend.backend_id, backend.address, backend.cluster_id);
            error!("{}", msg);
            self.answer_error(token, message_id, msg, None);
            return;
          },
          ProxyRequestData::RemoveHttpFrontend(HttpFrontend{ ref route, ref address, .. })
          | ProxyRequestData::RemoveHttpsFrontend(HttpFrontend{ ref route, ref address, .. }) => {
            let msg = match route {
                Route::ClusterId(cluster_id) => format!("No such frontend at {} for the cluster {}", address, cluster_id),
                Route::Deny => format!("No such frontend at {}", address),
            };
            error!("{}", msg);
            self.answer_error(token, message_id, msg, None);
            return;
          },
          | ProxyRequestData::RemoveTcpFrontend(TcpFrontend{ ref cluster_id, ref address }) => {
            let msg = format!("No such frontend at {} for the cluster {}", address, cluster_id);
            error!("{}", msg);
            self.answer_error(token, message_id, msg, None);
            return;
          },
          _ => {},
        };
      }
    }

    if self.config.automatic_state_save {
      if order != ProxyRequestData::SoftStop || order != ProxyRequestData::HardStop {
        if let Some(path) = self.config.saved_state.clone() {
          if let Ok(mut f) = fs::File::create(&path) {
            let _ = self.save_state_to_file(&mut f).map_err(|e| {
              error!("could not save state automatically to {}: {:?}", path, e);
            });
          }
        }
      }
    }

    let mut found = false;
    let mut futures = Vec::new();
    for ref mut worker in self.workers.values_mut()
      .filter(|worker| worker.run_state != RunState::Stopping && worker.run_state != RunState::Stopped) {

      if let Some(id) = worker_id {
        if id != worker.id {
          continue;
        }
      }

      let worker_token = worker.token.expect("worker should have a token");
      let should_stop_worker = order == ProxyRequestData::SoftStop || order == ProxyRequestData::HardStop;
      if should_stop_worker {
        worker.run_state = RunState::Stopping;
      }

      let id = worker.id.clone();
      futures.push(
        Box::new(executor::send(
          worker_token,
          ProxyRequest { id: message_id.to_string(), order: order.clone() })
        .map(move |r| {
          if should_stop_worker {
            executor::Executor::stop_worker(worker_token)
          }
          (id, r)
        })
      ));

      found = true;
    }

    if !found {
      // FIXME: should send back error here
      error!("no worker found");
    }

    let id = message_id.to_string();
    let should_stop_master = (order == ProxyRequestData::SoftStop || order == ProxyRequestData::HardStop) && worker_id.is_none();
    let f = join_all(futures).map(move |r| {
      if should_stop_master {
        executor::Executor::stop_master();
      }

      r
    });

    executor::Executor::execute(
      f.map(move |v| {
          let mut messages = vec![];
          let mut has_error = false;
          for response in v.iter() {
              if let ProxyResponseStatus::Error(ref e) = response.1.status {
                messages.push(format!("{}: {}", response.0, e));
                has_error = true;
              } else {
                messages.push(format!("{}: OK", response.0));
              }

          }
          if has_error {
              executor::Executor::send_client(token, CommandResponse::new(
                      id,
                      CommandStatus::Error,
                      messages.join(", "),
                      None
                      ));

          } else {
              executor::Executor::send_client(token, CommandResponse::new(
                      id,
                      CommandStatus::Ok,
                      String::new(),
                      None
                      ));
          }
      }).map_err(|e| {
        error!("worker_state error: {}", e);
      })
    );

    match order {
      ProxyRequestData::AddBackend(_)
      | ProxyRequestData::RemoveBackend(_) => self.backends_count = self.state.count_backends(),
      ProxyRequestData::AddHttpFrontend(_)
      | ProxyRequestData::AddHttpsFrontend(_)
      | ProxyRequestData::AddTcpFrontend(_)
      | ProxyRequestData::RemoveHttpFrontend(_)
      | ProxyRequestData::RemoveHttpsFrontend(_)
      | ProxyRequestData::RemoveTcpFrontend(_) => self.frontends_count = self.state.count_frontends(),
      _ => {}
    };

    gauge!("configuration.clusters", self.state.clusters.len());
    gauge!("configuration.backends", self.backends_count);
    gauge!("configuration.frontends", self.frontends_count);
  }

  pub fn load_static_application_configuration(&mut self) {
    //FIXME: too many loops, this could be cleaner
    for message in self.config.generate_config_messages() {
      if let CommandRequestData::Proxy(order) = message.data {
        self.state.handle_order(&order);

        if let &ProxyRequestData::AddCertificate(_) = &order {
          debug!("config generated AddCertificate( ... )");
        } else {
          debug!("config generated {:?}", order);
        }
        let mut found = false;
        for ref mut worker in self.workers.values_mut()
          .filter(|worker| worker.run_state != RunState::Stopping && worker.run_state != RunState::Stopped) {

          let o = order.clone();
          worker.push_message(ProxyRequest { id: message.id.clone(), order: o });
          found = true;
        }

        if !found {
          // FIXME: should send back error here
          error!("no worker found");
        }
      }
    }

    self.backends_count = self.state.count_backends();
    self.frontends_count = self.state.count_frontends();
    gauge!("configuration.clusters", self.state.clusters.len());
    gauge!("configuration.backends", self.backends_count);
    gauge!("configuration.frontends", self.frontends_count);
  }

  pub fn disable_cloexec_before_upgrade(&mut self) {
    for ref mut worker in self.workers.values() {
      if worker.run_state == RunState::Running {
        util::disable_close_on_exec(worker.channel.sock.as_raw_fd());
      }
    }
    trace!("disabling cloexec on listener: {}", self.sock.as_raw_fd());
    util::disable_close_on_exec(self.sock.as_raw_fd());
  }

  pub fn enable_cloexec_after_upgrade(&mut self) {
    for ref mut worker in self.workers.values() {
      if worker.run_state == RunState::Running {
        util::enable_close_on_exec(worker.channel.sock.as_raw_fd());
      }
    }
        util::enable_close_on_exec(self.sock.as_raw_fd());
  }

  pub fn generate_upgrade_data(&self) -> UpgradeData {
    let workers: Vec<SerializedWorker> = self.workers.values().map(|ref worker| SerializedWorker::from_worker(worker)).collect();
    //FIXME: ensure there's at least one worker
    let state = self.state.clone();

    UpgradeData {
      command:     self.sock.as_raw_fd(),
      config:      self.config.clone(),
      workers:     workers,
      state:       state,
      next_id:     self.next_id,
      token_count: self.token_count,
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
    } = upgrade_data;

    debug!("listener is: {}", command);
    let listener = unsafe { UnixListener::from_raw_fd(command) };
    poll.register(&listener, Token(0), Ready::readable(), PollOpt::edge() | PollOpt::oneshot()).expect("should register listener correctly");


    let buffer_size     = config.command_buffer_size;
    let max_buffer_size = config.max_command_buffer_size;

    let workers: HashMap<Token, Worker> = workers.iter().filter_map(|serialized| {
      if serialized.run_state == RunState::Stopped {
        return None;
      }

      let stream = unsafe { UnixStream::from_raw_fd(serialized.fd) };
      if let Some(token) = serialized.token {
        let _register = poll.register(&stream, Token(token),
          Ready::readable() | Ready::writable() | UnixReady::error() | UnixReady::hup(),
          PollOpt::edge());
        debug!("registering: {:?}", _register);

        let mut channel = Channel::new(stream, buffer_size, buffer_size * 2);
        channel.readiness.insert(Ready::writable());
        Some(
          (
            Token(token),
            Worker {
              id:         serialized.id,
              channel:    channel,
              token:      Some(Token(token)),
              pid:        serialized.pid,
              run_state:  serialized.run_state.clone(),
              queue:      serialized.queue.clone().into(),
              scm:        ScmSocket::new(serialized.scm),
            }
          )
        )
      } else { None }
    }).collect();

    let config_state = state.clone();

    let backends_count  = config_state.count_backends();
    let frontends_count = config_state.count_frontends();

    let path = unsafe { get_executable_path() };
    CommandServer {
      sock:              listener,
      poll:              poll,
      config:            config,
      buffer_size:       buffer_size,
      max_buffer_size:   max_buffer_size,
      //FIXME: deserialize client connections as well, otherwise they might leak?
      clients:           Slab::with_capacity(1024),
      event_subscribers: Vec::new(),
      workers:           workers,
      next_id:           next_id,
      state:             config_state,
      token_count:       token_count,

      //FIXME: deserialize this as well
      must_stop:         false,
      executable_path:   path,
      backends_count:    backends_count,
      frontends_count:   frontends_count,
    }
  }
}
