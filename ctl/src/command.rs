use sozu_command::config::{Config, ProxyProtocolConfig, LoadBalancingAlgorithms, Listener, FileListenerProtocolConfig};
use sozu_command::channel::Channel;
use sozu_command::certificate::{calculate_fingerprint,split_certificate_chain};
use sozu_command::command::{CommandResponseData,CommandRequestData,CommandRequest,CommandResponse,CommandStatus,RunState,WorkerInfo};
use sozu_command::proxy::{Application, ProxyRequestData, Backend, HttpFront, TcpFront,
  CertificateAndKey, CertFingerprint, Query, QueryAnswer, QueryApplicationType, QueryApplicationDomain,
  FilteredData,
  AddCertificate, RemoveCertificate, ReplaceCertificate, LoadBalancingParams, RemoveBackend, TcpListener, ListenerType,
  TlsVersion, QueryCertificateType, QueryAnswerCertificate, RemoveListener, ActivateListener, DeactivateListener};

use serde_json;
use std::collections::{HashMap,HashSet,BTreeMap};
use std::process::exit;
use std::thread;
use std::sync::{Arc,Mutex};
use std::time::Duration;
use std::sync::mpsc;
use std::net::SocketAddr;
use rand::{thread_rng, Rng};
use prettytable::{Table, Row};
use super::create_channel;
use rand::distributions::Alphanumeric;


// Used to display the JSON response of the status command
#[derive(Serialize, Debug)]
struct WorkerStatus<'a> {
  pub worker: &'a WorkerInfo,
  pub status: &'a String
}

fn generate_id() -> String {
  let s: String = thread_rng().sample_iter(&Alphanumeric).take(6).collect();
  format!("ID-{}", s)
}

fn generate_tagged_id(tag: &str) -> String {
  let s: String = thread_rng().sample_iter(&Alphanumeric).take(6).collect();
  format!("{}-{}", tag, s)
}

// Run the code waiting for messages in a separate thread. Just before finishing the thread sends a message.
// The calling code waits for this message with a timeout.
// Note: This macro is used only for simple command which has any/simple computing
// to do with the message received.
macro_rules! command_timeout {
  ($duration: expr, $block: expr) => (
    if $duration == 0 {
      $block
    } else {
      let (send, recv) = mpsc::channel();

      thread::spawn(move || {
        $block
        send.send(()).unwrap();
      });

      if recv.recv_timeout(Duration::from_millis($duration)).is_err() {
        eprintln!("Command timeout. The proxy didn't send answer");
      }
    }
  )
}

pub fn save_state(mut channel: Channel<CommandRequest,CommandResponse>, timeout: u64, path: String) {
  let id = generate_id();
  channel.write_message(&CommandRequest::new(
    id.clone(),
    CommandRequestData::SaveState { path },
    None,
  ));

  command_timeout!(timeout, {
    match channel.read_message() {
      None          => {
        eprintln!("the proxy didn't answer");
        exit(1);
      },
      Some(message) => {
        if id != message.id {
          eprintln!("received message with invalid id: {:?}", message);
          exit(1);
        }
        match message.status {
          CommandStatus::Processing => {
            // do nothing here
            // for other messages, we would loop over read_message
            // until an error or ok message was sent
          },
          CommandStatus::Error => {
            eprintln!("could not save proxy state: {}", message.message);
            exit(1);
          },
          CommandStatus::Ok => {
            println!("{}", message.message);
          }
        }
      }
    }
  });
}

pub fn load_state(mut channel: Channel<CommandRequest,CommandResponse>, timeout: u64, path: String) {
  let id = generate_id();
  channel.write_message(&CommandRequest::new(
    id.clone(),
    CommandRequestData::LoadState { path: path.clone() },
    None,
  ));

  command_timeout!(timeout, {
    match channel.read_message() {
      None          => {
        eprintln!("the proxy didn't answer");
        exit(1);
      },
      Some(message) => {
        if id != message.id {
          eprintln!("received message with invalid id: {:?}", message);
          exit(1);
        }
        match message.status {
          CommandStatus::Processing => {
            // do nothing here
            // for other messages, we would loop over read_message
            // until an error or ok message was sent
          },
          CommandStatus::Error => {
            eprintln!("could not load proxy state: {}", message.message);
            exit(1);
          },
          CommandStatus::Ok => {
            println!("Proxy state loaded successfully from {}", path);
          }
        }
      }
    };
  });
}

pub fn dump_state(mut channel: Channel<CommandRequest,CommandResponse>, timeout: u64, json: bool) {
  let id = generate_id();
  channel.write_message(&CommandRequest::new(
    id.clone(),
    CommandRequestData::DumpState,
    None,
  ));

  command_timeout!(timeout, {
    match channel.read_message() {
      None          => {
        eprintln!("the proxy didn't answer");
        exit(1);
      },
      Some(message) => {
        if id != message.id {
          eprintln!("received message with invalid id: {:?}", message);
          exit(1);
        }
        match message.status {
          CommandStatus::Processing => {
            // do nothing here
            // for other messages, we would loop over read_message
            // until an error or ok message was sent
          },
          CommandStatus::Error => {
            if json {
              print_json_response(&message.message);
            } else {
              eprintln!("could not dump proxy state: {}", message.message);
            }
            exit(1);
          },
          CommandStatus::Ok => {
            if let Some(CommandResponseData::State(state)) = message.data {
              if json {
                print_json_response(&state);
              } else {
                println!("{:#?}", state);
              }
            } else {
              eprintln!("state dump was empty");
              exit(1);
            }
          }
        }
      }
    }
  });
}

pub fn soft_stop(mut channel: Channel<CommandRequest,CommandResponse>, proxy_id: Option<u32>) {
  println!("shutting down proxy");
  let id = generate_id();
  channel.write_message(&CommandRequest::new(
    id.clone(),
    CommandRequestData::Proxy(ProxyRequestData::SoftStop),
    proxy_id,
  ));

  loop {
    match channel.read_message() {
      None          => {
        eprintln!("the proxy didn't answer");
        exit(1);
      },
      Some(message) => {
        if &id != &message.id {
          eprintln!("received message with invalid id: {:?}", message);
          exit(1);
        }
        match message.status {
          CommandStatus::Processing => {
            println!("Proxy is processing: {}", message.message);
          },
          CommandStatus::Error => {
            eprintln!("could not stop the proxy: {}", message.message);
            exit(1);
          },
          CommandStatus::Ok => {
            println!("Proxy shut down with message: \"{}\"", message.message);
            break;
          }
        }
      }
    }
  }
}

pub fn hard_stop(mut channel: Channel<CommandRequest,CommandResponse>, proxy_id: Option<u32>, timeout: u64) {
  println!("shutting down proxy");
  let id = generate_id();
  channel.write_message(&CommandRequest::new(
    id.clone(),
    CommandRequestData::Proxy(ProxyRequestData::HardStop),
    proxy_id,
  ));

  command_timeout!(timeout,
    loop {
      match channel.read_message() {
        None          => {
          eprintln!("the proxy didn't answer");
          exit(1);
        },
        Some(message) => {
          match message.status {
            CommandStatus::Processing => {
              println!("Proxy is processing: {}", message.message);
            },
            CommandStatus::Error => {
              eprintln!("could not stop the proxy: {}", message.message);
              exit(1);
            },
            CommandStatus::Ok => {
              if &id == &message.id {
                println!("Proxy shut down: {}", message.message);
                break;
              }
            }
          }
        }
      }
    }
  );
}

pub fn upgrade_main(mut channel: Channel<CommandRequest,CommandResponse>,
                  config: &Config) {
  println!("Preparing to upgrade proxy...");

  let id = generate_tagged_id("LIST-WORKERS");
  channel.write_message(&CommandRequest::new(
    id.clone(),
    CommandRequestData::ListWorkers,
    None,
  ));

  match channel.read_message() {
    None          => {
      eprintln!("Error: the proxy didn't list workers");
      exit(1);
    },
    Some(message) => {
      if id != message.id {
        eprintln!("Error: received unexpected message: {:?}", message);
        exit(1);
      }
      match message.status {
        CommandStatus::Processing => {
          eprintln!("Error: the proxy didn't return list of workers immediately");
          exit(1);
        },
        CommandStatus::Error => {
          eprintln!("Error: failed to get the list of worker: {}", message.message);
          exit(1);
        },
        CommandStatus::Ok => {
          if let Some(CommandResponseData::Workers(ref workers)) = message.data {
            let mut table = Table::new();
            table.add_row(row!["Worker", "pid", "run state"]);
            for ref worker in workers.iter() {
              let run_state = format!("{:?}", worker.run_state);
              table.add_row(row![worker.id, worker.pid, run_state]);
            }
            println!("");
            table.printstd();
            println!("");

            let id = generate_tagged_id("UPGRADE-MAIN");
            channel.write_message(&CommandRequest::new(
              id.clone(),
              CommandRequestData::UpgradeMain,
              None,
            ));
            println!("Upgrading main process");

            loop {
              match channel.read_message() {
                None          => {
                  eprintln!("Error: the proxy didn't start main upgrade");
                  exit(1);
                },
                Some(message) => {
                  if &id != &message.id {
                    eprintln!("Error: received unexpected message: {:?}", message);
                    exit(1);
                  }
                  match message.status {
                    CommandStatus::Processing => {},
                    CommandStatus::Error => {
                      eprintln!("Error: failed to upgrade the main: {}", message.message);
                      exit(1);
                    },
                    CommandStatus::Ok => {
                      println!("Main process upgrade succeeded: {}", message.message);
                      break;
                    },
                  }
                }
              }
            }

            // Reconnect to the new main
            println!("Reconnecting to new main process...");
            let mut channel = create_channel(&config).expect("could not reconnect to the command unix socket");

            // Do a rolling restart of the workers
            let running_workers = workers.iter()
              .filter(|worker| worker.run_state == RunState::Running)
              .collect::<Vec<_>>();
            let running_count = running_workers.len();
            for (i, ref worker) in running_workers.iter().enumerate() {
              println!("Upgrading worker {} (of {})", i+1, running_count);

              channel = upgrade_worker(channel, 0, worker.id);
              //thread::sleep(Duration::from_millis(1000));
            }

            println!("Proxy successfully upgraded!");
          }
        }
      }
    }
  }
}

pub fn upgrade_worker(mut channel: Channel<CommandRequest,CommandResponse>, timeout: u64, worker_id: u32) -> Channel<CommandRequest,CommandResponse> {
  println!("upgrading worker {}", worker_id);
  let id = generate_id();
  channel.write_message(&CommandRequest::new(
    id.clone(),
    CommandRequestData::UpgradeWorker(worker_id),
    //FIXME: we should be able to soft stop one specific worker
    None,
  ));

  // We do our own timeout so we can return the Channel object from the thread
  // and avoid ownership issues
  let (send, recv) = mpsc::channel();

  let timeout_thread = thread::spawn(move || {
    loop {
      match channel.read_message() {
        None          => {
          eprintln!("the proxy didn't answer");
          exit(1);
        },
        Some(message) => {
          match message.status {
            CommandStatus::Processing => {
              eprintln!("Worker {} is processing: {}", worker_id, message.message);
            },
            CommandStatus::Error => {
              eprintln!("could not stop the worker {}: {}", worker_id, message.message);
              exit(1);
            },
            CommandStatus::Ok => {
              if &id == &message.id {
                println!("Worker {} shut down: {}", worker_id, message.message);
                break;
              }
            }
          }
        }
      }
    }
    send.send(()).unwrap();
    channel
  });

  if timeout > 0 && recv.recv_timeout(Duration::from_millis(timeout)).is_err() {
    eprintln!("Command timeout. The proxy didn't send answer");
    exit(1);
  }

  timeout_thread.join().expect("upgrade_worker: Timeout thread should correctly terminate")
}

pub fn status(mut channel: Channel<CommandRequest,CommandResponse>, json: bool) {
  let id = generate_id();
  channel.write_message(&CommandRequest::new(
    id.clone(),
    CommandRequestData::ListWorkers,
    None,
  ));

  match channel.read_message() {
    None          => {
      eprintln!("the proxy didn't answer");
      exit(1);
    },
    Some(message) => {
      if id != message.id {
        eprintln!("received message with invalid id: {:?}", message);
        exit(1);
      }
      match message.status {
        CommandStatus::Processing => {
          eprintln!("should have obtained an answer immediately");
          exit(1);
        },
        CommandStatus::Error => {
          if json {
            print_json_response(&message.message);
          } else {
            eprintln!("could not get the worker list: {}", message.message);
          }
          exit(1);
        },
        CommandStatus::Ok => {
          //println!("Worker list:\n{:?}", message.data);
          if let Some(CommandResponseData::Workers(ref workers)) = message.data {
            let mut expecting: HashSet<String> = HashSet::new();

            let mut h = HashMap::new();
            for ref worker in workers.iter().filter(|worker| worker.run_state == RunState::Running) {
              let id = generate_id();
              let msg = CommandRequest::new(
                id.clone(),
                CommandRequestData::Proxy(ProxyRequestData::Status),
                Some(worker.id),
              );
              //println!("sending message: {:?}", msg);
              channel.write_message(&msg);
              expecting.insert(id.clone());
              h.insert(id, (worker.id, CommandStatus::Processing));
            }

            let state = Arc::new(Mutex::new(h));
            let st = state.clone();
            let (send, recv) = mpsc::channel();

            thread::spawn(move || {
              loop {
                //println!("expecting: {:?}", expecting);
                if expecting.is_empty() {
                  break;
                }
                match channel.read_message() {
                  None          => {
                    eprintln!("the proxy didn't answer");
                    exit(1);
                  },
                  Some(message) => {
                    //println!("received message: {:?}", message);
                    match message.status {
                      CommandStatus::Processing => {
                      },
                      CommandStatus::Error => {
                        eprintln!("error for message[{}]: {}", message.id, message.message);
                        if expecting.contains(&message.id) {
                          expecting.remove(&message.id);
                          //println!("status message with ID {} done", message.id);
                          if let Ok(mut h) = state.try_lock() {
                            if let Some(data) = h.get_mut(&message.id) {
                              *data = ((*data).0, CommandStatus::Error);
                            }
                          }
                        }
                        exit(1);
                      },
                      CommandStatus::Ok => {
                        if expecting.contains(&message.id) {
                          expecting.remove(&message.id);
                          //println!("status message with ID {} done", message.id);
                          if let Ok(mut h) = state.try_lock() {
                            if let Some(data) = h.get_mut(&message.id) {
                              *data = ((*data).0, CommandStatus::Ok);
                            }
                          }
                        }
                      }
                    }
                  }
                }
              }

              send.send(()).unwrap();
            });

            let finished = recv.recv_timeout(Duration::from_millis(1000)).is_ok();
            let placeholder = if finished {
              String::from("")
            } else {
              String::from("timeout")
            };

            let h2: HashMap<u32, String> = if let Ok(state) = st.try_lock() {
              state.values().map(|&(ref id, ref status)| {
                (*id, String::from(match *status {
                  CommandStatus::Processing => if finished {
                    "processing"
                  } else {
                    "timeout"
                  },
                  CommandStatus::Error      => "error",
                  CommandStatus::Ok         => "ok",
                }))
              }).collect()
            } else {
              HashMap::new()
            };

            if json {
              let workers_status: Vec<WorkerStatus> = workers.iter().map(|ref worker| {
                WorkerStatus {
                  worker: worker,
                  status: h2.get(&worker.id).unwrap_or(&placeholder)
                }
              }).collect();
              print_json_response(&workers_status);
            } else {
              let mut table = Table::new();

              table.add_row(row!["Worker", "pid", "run state", "answer"]);
              for ref worker in workers.iter() {
                let run_state = format!("{:?}", worker.run_state);
                table.add_row(row![worker.id, worker.pid, run_state, h2.get(&worker.id).unwrap_or(&placeholder)]);
              }

              table.printstd();
            }
          }
        }
      }
    }
  }
}

pub fn metrics(mut channel: Channel<CommandRequest,CommandResponse>, json: bool) {
  let id = generate_id();
  //println!("will send message for metrics with id {}", id);
  channel.write_message(&CommandRequest::new(
    id.clone(),
    CommandRequestData::Proxy(ProxyRequestData::Metrics),
    None,
  ));
  //println!("message sent");

  loop {
    match channel.read_message() {
      None          => {
        eprintln!("the proxy didn't answer");
        exit(1);
      },
      Some(message) => {
        match message.status {
          CommandStatus::Processing => {
            println!("Proxy is processing: {}", message.message);
          },
          CommandStatus::Error => {
            if json {
              print_json_response(&message.message);
            } else {
              eprintln!("could not stop the proxy: {}", message.message);
              exit(1);
            }
          },
          CommandStatus::Ok => {
            if &id == &message.id {
              //println!("Sozu metrics:\n{}\n{:#?}", message.message, message.data);

              if let Some(CommandResponseData::Metrics(data)) = message.data {
                if json {
                  print_json_response(&data);
                  return;
                }

                let mut main_table = Table::new();
                main_table.add_row(row![String::from("Main process")]);
                main_table.add_row(row![String::from("key"), String::from("Count"), String::from("Gauge")]);

                for (ref key, ref value) in data.main.iter() {
                  match value {
                    FilteredData::Count(c) => {main_table.add_row(row![key.to_string(), c, String::new()]);},
                    FilteredData::Gauge(c) => { main_table.add_row(row![key.to_string(), String::new(), c]);},
                    r => {
                      println!("unexpected metric: {:?}", r);
                      main_table.add_row(row![key.to_string(), String::new(), String::new()]);
                    }
                  }
                }

                main_table.printstd();

                println!("\nworker metrics:\n");

                let mut proxy_table = Table::new();
                let mut row = vec![cell!("Workers")];
                for key in data.workers.keys() {
                  row.push(cell!(key));
                  row.push(cell!(""));
                  row.push(cell!(""));
                  row.push(cell!(""));
                  row.push(cell!(""));
                  row.push(cell!(""));
                  row.push(cell!(""));
                  row.push(cell!(""));
                  row.push(cell!(""));
                }
                proxy_table.add_row(Row::new(row));

                let mut worker_keys = HashSet::new();
                let mut header = Vec::new();
                header.push(cell!("key"));
                for key in data.workers.keys() {
                  header.push(cell!("Count"));
                  header.push(cell!("Gauge"));
                  header.push(cell!("p50"));
                  header.push(cell!("p90"));
                  header.push(cell!("p99"));
                  header.push(cell!("p99.9"));
                  header.push(cell!("p99.99"));
                  header.push(cell!("p99.999"));
                  header.push(cell!("p100"));
                  worker_keys.insert(key);
                }
                proxy_table.add_row(Row::new(header.clone()));

                let mut proxy_metrics = HashSet::new();
                for metrics in data.workers.values() {
                  for key in metrics.proxy.keys() {
                    proxy_metrics.insert(key);
                  }
                }

                for key in proxy_metrics.iter() {
                  let k: &str = key;
                  let mut row = Vec::new();
                  row.push(cell!(k.to_string()));
                  for worker_key in worker_keys.iter() {
                    let wk: &str = worker_key;

                    match data.workers[wk].proxy.get(k) {
                      None => {
                        row.push(cell!(""));
                        row.push(cell!(""));
                        row.push(cell!(""));
                        row.push(cell!(""));
                        row.push(cell!(""));
                        row.push(cell!(""));
                        row.push(cell!(""));
                        row.push(cell!(""));
                        row.push(cell!(""));
                      },
                      Some(FilteredData::Count(c)) => {
                        row.push(cell!(c));
                        row.push(cell!(""));
                        row.push(cell!(""));
                        row.push(cell!(""));
                        row.push(cell!(""));
                        row.push(cell!(""));
                        row.push(cell!(""));
                        row.push(cell!(""));
                        row.push(cell!(""));
                      },
                      Some(FilteredData::Gauge(c)) => {
                        row.push(cell!(""));
                        row.push(cell!(c));
                        row.push(cell!(""));
                        row.push(cell!(""));
                        row.push(cell!(""));
                        row.push(cell!(""));
                        row.push(cell!(""));
                        row.push(cell!(""));
                        row.push(cell!(""));
                      },
                      Some(FilteredData::Percentiles(p)) => {
                        row.push(cell!(p.samples));
                        row.push(cell!(""));
                        row.push(cell!(p.p_50));
                        row.push(cell!(p.p_90));
                        row.push(cell!(p.p_99));
                        row.push(cell!(p.p_99_9));
                        row.push(cell!(p.p_99_99));
                        row.push(cell!(p.p_99_999));
                        row.push(cell!(p.p_100));
                      },
                      r => {
                        println!("unexpected metric: {:?}", r);
                        row.push(cell!(""));
                        row.push(cell!(""));
                        row.push(cell!(""));
                        row.push(cell!(""));
                        row.push(cell!(""));
                        row.push(cell!(""));
                        row.push(cell!(""));
                        row.push(cell!(""));
                        row.push(cell!(""));
                      }
                    }
                  }

                  proxy_table.add_row(Row::new(row));
                }

                proxy_table.printstd();

                println!("\napplication metrics:\n");

                let mut app_ids = HashSet::new();
                for metrics in data.workers.values() {
                  for key in metrics.applications.keys() {
                    app_ids.insert(key);
                  }
                }

                for app_id in app_ids.iter() {
                  let id: &str = app_id;

                  let mut application_table = Table::new();

                  let mut row = vec![cell!(id)];
                  for key in data.workers.keys() {
                    row.push(cell!(key));
                    row.push(cell!(""));
                    row.push(cell!(""));
                    row.push(cell!(""));
                    row.push(cell!(""));
                    row.push(cell!(""));
                    row.push(cell!(""));
                    row.push(cell!(""));
                    row.push(cell!(""));
                  }
                  application_table.add_row(Row::new(row));
                  application_table.add_row(Row::new(header.clone()));

                  let mut app_metrics = HashSet::new();
                  let mut backend_ids = HashSet::new();

                  for worker in data.workers.values() {
                    if let Some(app) = worker.applications.get(id) {
                      for k in app.data.keys() {
                        app_metrics.insert(k);
                      }

                      for k in app.backends.keys() {
                        backend_ids.insert(k);
                      }
                    }
                  }

                  for app_metric in app_metrics.iter() {
                    let metric: &str = app_metric;
                    let mut row = Vec::new();
                    row.push(cell!(metric.to_string()));

                    for worker in data.workers.values() {
                      match worker.applications.get(id).and_then(|app| app.data.get(metric)) {
                        None => {
                          row.push(cell!(""));
                          row.push(cell!(""));
                          row.push(cell!(""));
                          row.push(cell!(""));
                          row.push(cell!(""));
                          row.push(cell!(""));
                          row.push(cell!(""));
                          row.push(cell!(""));
                          row.push(cell!(""));
                        },
                        Some(FilteredData::Count(c)) => {
                          row.push(cell!(c));
                          row.push(cell!(""));
                          row.push(cell!(""));
                          row.push(cell!(""));
                          row.push(cell!(""));
                          row.push(cell!(""));
                          row.push(cell!(""));
                          row.push(cell!(""));
                          row.push(cell!(""));
                        },
                        Some(FilteredData::Gauge(c)) => {
                          row.push(cell!(""));
                          row.push(cell!(c));
                          row.push(cell!(""));
                          row.push(cell!(""));
                          row.push(cell!(""));
                          row.push(cell!(""));
                          row.push(cell!(""));
                          row.push(cell!(""));
                          row.push(cell!(""));
                        }
                        Some(FilteredData::Percentiles(p)) => {
                          row.push(cell!(p.samples));
                          row.push(cell!(""));
                          row.push(cell!(p.p_50));
                          row.push(cell!(p.p_90));
                          row.push(cell!(p.p_99));
                          row.push(cell!(p.p_99_9));
                          row.push(cell!(p.p_99_99));
                          row.push(cell!(p.p_99_999));
                          row.push(cell!(p.p_100));
                        },
                        r => {
                          println!("unexpected metric: {:?}", r);
                          row.push(cell!(""));
                          row.push(cell!(""));
                          row.push(cell!(""));
                          row.push(cell!(""));
                          row.push(cell!(""));
                          row.push(cell!(""));
                          row.push(cell!(""));
                          row.push(cell!(""));
                          row.push(cell!(""));
                        },
                      }
                    }
                    application_table.add_row(Row::new(row));
                  }
                  application_table.printstd();

                  for backend_id in backend_ids.iter() {
                    let backend: &str = backend_id;
                    let mut backend_table = Table::new();

                    let mut row = vec![cell!(format!("{}: {}", id, backend))];
                    for key in data.workers.keys() {
                      row.push(cell!(key));
                      row.push(cell!(""));
                      row.push(cell!(""));
                      row.push(cell!(""));
                      row.push(cell!(""));
                      row.push(cell!(""));
                      row.push(cell!(""));
                      row.push(cell!(""));
                      row.push(cell!(""));
                    }
                    backend_table.add_row(Row::new(row));
                    backend_table.add_row(Row::new(header.clone()));

                    let mut backend_metrics = HashSet::new();
                    for worker in data.workers.values() {
                      if let Some(app) = worker.applications.get(id) {
                        for b in app.backends.values() {
                          for k in b.keys() {
                            backend_metrics.insert(k);
                          }
                        }
                      }
                    }

                    for backend_metric in backend_metrics.iter() {
                      let metric: &str = backend_metric;
                      let mut row = Vec::new();
                      row.push(cell!(metric.to_string()));

                      for worker in data.workers.values() {
                        match worker.applications.get(id).and_then(|app| app.backends.get(backend))
                          .and_then(|back| back.get(metric)) {
                          None => {
                            row.push(cell!(""));
                            row.push(cell!(""));
                            row.push(cell!(""));
                            row.push(cell!(""));
                            row.push(cell!(""));
                            row.push(cell!(""));
                            row.push(cell!(""));
                            row.push(cell!(""));
                            row.push(cell!(""));
                          },
                          Some(FilteredData::Count(c)) => {
                            row.push(cell!(c));
                            row.push(cell!(""));
                            row.push(cell!(""));
                            row.push(cell!(""));
                            row.push(cell!(""));
                            row.push(cell!(""));
                            row.push(cell!(""));
                            row.push(cell!(""));
                            row.push(cell!(""));
                          },
                          Some(FilteredData::Gauge(c)) => {
                            row.push(cell!(""));
                            row.push(cell!(c));
                            row.push(cell!(""));
                            row.push(cell!(""));
                            row.push(cell!(""));
                            row.push(cell!(""));
                            row.push(cell!(""));
                            row.push(cell!(""));
                            row.push(cell!(""));
                          }
                          Some(FilteredData::Percentiles(p)) => {
                            row.push(cell!(p.samples));
                            row.push(cell!(""));
                            row.push(cell!(p.p_50));
                            row.push(cell!(p.p_90));
                            row.push(cell!(p.p_99));
                            row.push(cell!(p.p_99_9));
                            row.push(cell!(p.p_99_99));
                            row.push(cell!(p.p_99_999));
                            row.push(cell!(p.p_100));
                          },
                          r => {
                            println!("unexpected metric: {:?}", r);
                            row.push(cell!(""));
                            row.push(cell!(""));
                            row.push(cell!(""));
                            row.push(cell!(""));
                            row.push(cell!(""));
                            row.push(cell!(""));
                            row.push(cell!(""));
                            row.push(cell!(""));
                            row.push(cell!(""));
                          },
                        }
                      }
                      backend_table.add_row(Row::new(row));
                    }

                    backend_table.printstd();
                  }
                }

                break;
              }
            }
          }
        }
      }
    }
  }
}

pub fn reload_configuration(mut channel: Channel<CommandRequest,CommandResponse>, path: Option<String>, json: bool) {
  let id = generate_id();
  channel.write_message(&CommandRequest::new(
    id.clone(),
    CommandRequestData::ReloadConfiguration { path },
    None,
  ));

  match channel.read_message() {
    None          => {
      eprintln!("the proxy didn't answer");
      exit(1);
    },
    Some(message) => {
      if id != message.id {
        eprintln!("received message with invalid id: {:?}", message);
        exit(1);
      }
      match message.status {
        CommandStatus::Processing => {
          eprintln!("should have obtained an answer immediately");
          exit(1);
        },
        CommandStatus::Error => {
          if json {
            print_json_response(&message.message);
          } else {
            eprintln!("could not get the worker list: {}", message.message);
          }
          exit(1);
        },
        CommandStatus::Ok => {
          if json {
            print_json_response(&message.message);
          } else {
            println!("Reloaded configuration: {}", message.message);
          }
        }
      }
    }
  }
}

pub fn add_application(channel: Channel<CommandRequest,CommandResponse>, timeout: u64, app_id: &str, sticky_session: bool, https_redirect: bool, send_proxy: bool, expect_proxy: bool, load_balancing: LoadBalancingAlgorithms) {
  let proxy_protocol = match (send_proxy, expect_proxy) {
    (true, true) => Some(ProxyProtocolConfig::RelayHeader),
    (true, false) => Some(ProxyProtocolConfig::SendHeader),
    (false, true) => Some(ProxyProtocolConfig::ExpectHeader),
    _ => None,
  };

  order_command(channel, timeout, ProxyRequestData::AddApplication(Application {
    app_id: String::from(app_id),
    sticky_session,
    https_redirect,
    proxy_protocol,
    load_balancing,
    load_metric: None,
    answer_503: None,
  }));
}

pub fn remove_application(channel: Channel<CommandRequest,CommandResponse>, timeout: u64, app_id: &str) {
  order_command(channel, timeout, ProxyRequestData::RemoveApplication(String::from(app_id)));
}

pub fn add_http_frontend(channel: Channel<CommandRequest,CommandResponse>, timeout: u64, app_id: &str,
  address: SocketAddr, hostname: &str, path_begin: &str, https: bool) {
  if https {
    order_command(channel, timeout, ProxyRequestData::AddHttpsFront(HttpFront {
      app_id: String::from(app_id),
      address,
      hostname: String::from(hostname),
      path_begin: String::from(path_begin),
    }));
  } else {
    order_command(channel, timeout, ProxyRequestData::AddHttpFront(HttpFront {
      app_id: String::from(app_id),
      address,
      hostname: String::from(hostname),
      path_begin: String::from(path_begin),
    }));
  }
}

pub fn remove_http_frontend(channel: Channel<CommandRequest,CommandResponse>, timeout: u64, app_id: &str,
  address: SocketAddr, hostname: &str, path_begin: &str, https: bool) {
  if https {
    order_command(channel, timeout, ProxyRequestData::RemoveHttpsFront(HttpFront {
      app_id: String::from(app_id),
      address,
      hostname: String::from(hostname),
      path_begin: String::from(path_begin),
    }));
  } else {
    order_command(channel, timeout, ProxyRequestData::RemoveHttpFront(HttpFront {
      app_id: String::from(app_id),
      address,
      hostname: String::from(hostname),
      path_begin: String::from(path_begin),
    }));
  }
}


pub fn add_backend(channel: Channel<CommandRequest,CommandResponse>, timeout: u64, app_id: &str,
  backend_id: &str, address: SocketAddr, sticky_id: Option<String>, backup: Option<bool>) {
  order_command(channel, timeout, ProxyRequestData::AddBackend(Backend {
      app_id: String::from(app_id),
      address: address,
      backend_id: String::from(backend_id),
      load_balancing_parameters: Some(LoadBalancingParams::default()),
      sticky_id: sticky_id,
      backup:    backup
    }));
}

pub fn remove_backend(channel: Channel<CommandRequest,CommandResponse>, timeout: u64, app_id: &str,
  backend_id: &str, address: SocketAddr) {
  order_command(channel, timeout, ProxyRequestData::RemoveBackend(RemoveBackend {
    app_id: String::from(app_id),
    address: address,
    backend_id: String::from(backend_id),
  }));
}

pub fn add_certificate(channel: Channel<CommandRequest,CommandResponse>, timeout: u64, address: SocketAddr,
  certificate_path: &str, certificate_chain_path: &str, key_path: &str, versions: Vec<TlsVersion>) {
  if let Some(new_certificate) = load_full_certificate(certificate_path,
                                                       certificate_chain_path,
                                                       key_path, versions) {
    order_command(channel, timeout, ProxyRequestData::AddCertificate(AddCertificate {
      front: address,
      certificate: new_certificate,
      names: Vec::new(),
    }));
  }
}

pub fn remove_certificate(channel: Channel<CommandRequest,CommandResponse>, timeout: u64, address: SocketAddr,
  certificate_path: Option<&str>, fingerprint: Option<&str>) {
  if certificate_path.is_some() && fingerprint.is_some() {
    eprintln!("Error: Either provide the certificate's path or its fingerprint");
    exit(1);
  }

  if certificate_path.is_none() && fingerprint.is_none() {
    eprintln!("Error: Either provide the certificate's path or its fingerprint");
    exit(1);
  }

    if let Some(fingerprint) = fingerprint.and_then(|s| {
        match hex::decode(s) {
            Ok(v) => Some(CertFingerprint(v)),
            Err(e) => {
                eprintln!("Error decoding the certificate fingerprint (expected hexadecimal data): {:?}", e);
                None
            }
        }
    }).or(certificate_path.and_then(get_certificate_fingerprint)) {
    order_command(channel, timeout, ProxyRequestData::RemoveCertificate(RemoveCertificate {
      front: address,
      fingerprint: fingerprint,
      names: Vec::new(),
    }));
  }
}

pub fn replace_certificate(channel: Channel<CommandRequest,CommandResponse>, timeout: u64, address: SocketAddr,
  new_certificate_path: &str, new_certificate_chain_path: &str, new_key_path: &str,
  old_certificate_path: Option<&str>, old_fingerprint: Option<&str>,
  versions: Vec<TlsVersion>)
{
  if old_certificate_path.is_some() && old_fingerprint.is_some() {
    eprintln!("Error: Either provide the old certificate's path or its fingerprint");
    exit(1);
  }

  if old_certificate_path.is_none() && old_fingerprint.is_none() {
    eprintln!("Error: Either provide the old certificate's path or its fingerprint");
    exit(1);
  }

  if let Some(new_certificate) = load_full_certificate(new_certificate_path,
                                                       new_certificate_chain_path,
                                                       new_key_path, versions) {
    if let Some(old_fingerprint) = old_fingerprint.and_then(|s| {
        match hex::decode(s) {
            Ok(v) => Some(CertFingerprint(v)),
            Err(e) => {
                eprintln!("Error decoding the certificate fingerprint (expected hexadecimal data): {:?}", e);
                None
            }
        }
    }).or(old_certificate_path.and_then(get_certificate_fingerprint)) {
      order_command(channel, timeout, ProxyRequestData::ReplaceCertificate(ReplaceCertificate {
        front: address,
        new_certificate,
        old_fingerprint,
        new_names: Vec::new(),
        old_names: Vec::new()
      }));
    }
  }
}

pub fn add_tcp_frontend(channel: Channel<CommandRequest,CommandResponse>, timeout: u64, app_id: &str,
  address: SocketAddr) {
  order_command(channel, timeout, ProxyRequestData::AddTcpFront(TcpFront {
    app_id: String::from(app_id),
    address,
  }));
}

pub fn remove_tcp_frontend(channel: Channel<CommandRequest,CommandResponse>, timeout: u64, app_id: &str,
  address: SocketAddr) {
  order_command(channel, timeout, ProxyRequestData::RemoveTcpFront(TcpFront {
    app_id: String::from(app_id),
    address,
  }));
}

pub fn add_http_listener(channel: Channel<CommandRequest,CommandResponse>, timeout: u64, address: SocketAddr, public_address: Option<SocketAddr>,
  answer_404: Option<String>, answer_503: Option<String>, expect_proxy: bool, sticky_name: Option<String>) {
  let mut listener = Listener::new(address, FileListenerProtocolConfig::Http);
  listener.public_address = public_address;
  listener.answer_404 = answer_404;
  listener.answer_503 = answer_503;
  listener.expect_proxy = Some(expect_proxy);
  if let Some(sticky_name) = sticky_name {
    listener.sticky_name = sticky_name;
  }

  match listener.to_http(None, None, None) {
    Some(conf) => order_command(channel, timeout, ProxyRequestData::AddHttpListener(conf)),
    None => eprintln!("Error creating HTTP listener")
  };
}

pub fn add_https_listener(channel: Channel<CommandRequest,CommandResponse>, timeout: u64, address: SocketAddr, public_address: Option<SocketAddr>,
  answer_404: Option<String>, answer_503: Option<String>, tls_versions: Vec<TlsVersion>, cipher_list: Option<String>,
  rustls_cipher_list: Vec<String>, expect_proxy: bool, sticky_name: Option<String>) {
  let mut listener = Listener::new(address, FileListenerProtocolConfig::Https);
  listener.public_address = public_address;
  listener.answer_404 = answer_404;
  listener.answer_503 = answer_503;
  listener.expect_proxy = Some(expect_proxy);
  if let Some(sticky_name) = sticky_name {
    listener.sticky_name = sticky_name;
  }
  listener.cipher_list = cipher_list;
  listener.tls_versions = if tls_versions.len() == 0 { None } else { Some(tls_versions) };
  listener.rustls_cipher_list = if rustls_cipher_list.len() == 0 { None } else { Some(rustls_cipher_list) };

  match listener.to_tls(None, None, None) {
    Some(conf) => order_command(channel, timeout, ProxyRequestData::AddHttpsListener(conf)),
    None => eprintln!("Error creating HTTPS listener")
  };
}

pub fn add_tcp_listener(channel: Channel<CommandRequest,CommandResponse>, timeout: u64, address: SocketAddr,
  public_address: Option<SocketAddr>, expect_proxy: bool) {
  order_command(channel, timeout, ProxyRequestData::AddTcpListener(TcpListener {
    front: address,
    public_address,
    expect_proxy,
    front_timeout: 60,
    back_timeout: 30,
    connect_timeout: 3,
  }));
}

pub fn remove_listener(channel: Channel<CommandRequest,CommandResponse>, timeout: u64, address: SocketAddr, proxy: ListenerType) {
  order_command(channel, timeout, ProxyRequestData::RemoveListener(RemoveListener {
    front: address,
    proxy
  }));
}

pub fn activate_listener(channel: Channel<CommandRequest,CommandResponse>, timeout: u64, address: SocketAddr, proxy: ListenerType) {
  order_command(channel, timeout, ProxyRequestData::ActivateListener(ActivateListener {
    front: address,
    proxy,
    from_scm: false
  }));
}

pub fn deactivate_listener(channel: Channel<CommandRequest,CommandResponse>, timeout: u64, address: SocketAddr, proxy: ListenerType) {
  order_command(channel, timeout, ProxyRequestData::DeactivateListener(DeactivateListener {
    front: address,
    proxy,
    to_scm: false
  }));
}

pub fn query_application(mut channel: Channel<CommandRequest,CommandResponse>, json: bool, application_id: Option<String>, domain: Option<String>) {
  if application_id.is_some() && domain.is_some() {
    eprintln!("Error: Either request an application ID or a domain name");
    exit(1);
  }

  let command = if let Some(ref app_id) = application_id {
    CommandRequestData::Proxy(ProxyRequestData::Query(Query::Applications(QueryApplicationType::AppId(app_id.to_string()))))
  } else if let Some(ref domain) = domain {
    let splitted: Vec<String> = domain.splitn(2, "/").map(|elem| elem.to_string()).collect();

    if splitted.len() == 0 {
      eprintln!("Domain can't be empty");
      exit(1);
    }

    let query_domain = QueryApplicationDomain {
      hostname: splitted.get(0).expect("Domain can't be empty").clone(),
      path_begin: splitted.get(1).cloned().map(|path| format!("/{}", path)) // We add the / again because of the splitn removing it
    };

    CommandRequestData::Proxy(ProxyRequestData::Query(Query::Applications(QueryApplicationType::Domain(query_domain))))
  } else {
    CommandRequestData::Proxy(ProxyRequestData::Query(Query::ApplicationsHashes))
  };

  let id = generate_id();
  channel.write_message(&CommandRequest::new(
    id.clone(),
    command,
    None,
  ));

  match channel.read_message() {
    None          => {
      eprintln!("the proxy didn't answer");
      exit(1);
    },
    Some(message) => {
      if id != message.id {
        eprintln!("received message with invalid id: {:?}", message);
        exit(1);
      }
      match message.status {
        CommandStatus::Processing => {
          // do nothing here
          // for other messages, we would loop over read_message
          // until an error or ok message was sent
        },
        CommandStatus::Error => {
          if json {
            print_json_response(&message.message);
          } else {
            eprintln!("could not query proxy state: {}", message.message);
          }
          exit(1);
        },
        CommandStatus::Ok => {
          if let Some(needle) = application_id.or(domain) {
            if let Some(CommandResponseData::Query(data)) = message.data {
              if json {
                print_json_response(&data);
                return;
              }

              let application_headers = vec!["id", "sticky_session", "https_redirect"];
              let mut application_table = create_queried_application_table(application_headers, &data);

              let http_headers = vec!["id", "hostname", "path begin"];
              let mut frontend_table = create_queried_application_table(http_headers, &data);

              let https_headers = vec!["id", "hostname", "path begin"];
              let mut https_frontend_table = create_queried_application_table(https_headers, &data);

              let tcp_headers = vec!["id", "address"];
              let mut tcp_frontend_table = create_queried_application_table(tcp_headers, &data);

              let backend_headers = vec!["backend id", "IP address", "Backup"];
              let mut backend_table = create_queried_application_table(backend_headers, &data);

              let keys : HashSet<&String> = data.keys().collect();

              let mut application_data = HashMap::new();
              let mut frontend_data = HashMap::new();
              let mut https_frontend_data = HashMap::new();
              let mut tcp_frontend_data = HashMap::new();
              let mut backend_data = HashMap::new();

              for (ref key, ref metrics) in data.iter() {
                //let m: u8 = metrics;
                if let &QueryAnswer::Applications(ref apps) = *metrics {
                  for app in apps.iter() {
                    let entry = application_data.entry(app).or_insert(Vec::new());
                    entry.push((*key).clone());

                    for frontend in app.http_frontends.iter() {
                      let entry = frontend_data.entry(frontend).or_insert(Vec::new());
                      entry.push((*key).clone());
                    }

                    for frontend in app.https_frontends.iter() {
                      let entry = https_frontend_data.entry(frontend).or_insert(Vec::new());
                      entry.push((*key).clone());
                    }

                    for frontend in app.tcp_frontends.iter() {
                      let entry = tcp_frontend_data.entry(frontend).or_insert(Vec::new());
                      entry.push((*key).clone());
                    }

                    for backend in app.backends.iter() {
                      let entry = backend_data.entry(backend).or_insert(Vec::new());
                      entry.push((*key).clone());
                    }
                  }
                }
              }

              println!("Application level configuration for {}:\n", needle);

              for (ref key, ref values) in application_data.iter() {
                let mut row = Vec::new();
                row.push(cell!(key.configuration.clone().map(|conf| conf.app_id).unwrap_or(String::from(""))));
                row.push(cell!(key.configuration.clone().map(|conf| conf.sticky_session).unwrap_or(false)));
                row.push(cell!(key.configuration.clone().map(|conf| conf.https_redirect).unwrap_or(false)));

                for val in values.iter() {
                  if keys.contains(val) {
                    row.push(cell!(String::from("X")));
                  } else {
                    row.push(cell!(String::from("")));
                  }
                }

                application_table.add_row(Row::new(row));
              }

              application_table.printstd();

              println!("\nHTTP frontends configuration for {}:\n", needle);

              for (ref key, ref values) in frontend_data.iter() {
                let mut row = Vec::new();
                row.push(cell!(key.app_id));
                row.push(cell!(key.hostname));
                row.push(cell!(key.path_begin));

                for val in values.iter() {
                  if keys.contains(val) {
                    row.push(cell!(String::from("X")));
                  } else {
                    row.push(cell!(String::from("")));
                  }
                }

                frontend_table.add_row(Row::new(row));
              }

              frontend_table.printstd();

              println!("\nHTTPS frontends configuration for {}:\n", needle);

              for (ref key, ref values) in https_frontend_data.iter() {
                let mut row = Vec::new();
                row.push(cell!(key.app_id));
                row.push(cell!(key.hostname));
                row.push(cell!(key.path_begin));

                for val in values.iter() {
                  if keys.contains(val) {
                    row.push(cell!(String::from("X")));
                  } else {
                    row.push(cell!(String::from("")));
                  }
                }

                https_frontend_table.add_row(Row::new(row));
              }

              https_frontend_table.printstd();

              println!("\nTCP frontends configuration for {}:\n", needle);

              for (ref key, ref values) in tcp_frontend_data.iter() {
                let mut row = Vec::new();
                row.push(cell!(key.app_id));
                row.push(cell!(format!("{}", key.address)));

                for val in values.iter() {
                  if keys.contains(val) {
                    row.push(cell!(String::from("X")));
                  } else {
                    row.push(cell!(String::from("")));
                  }
                }

                tcp_frontend_table.add_row(Row::new(row));
              }

              tcp_frontend_table.printstd();

              println!("\nbackends configuration for {}:\n", needle);

              for (ref key, ref values) in backend_data.iter() {
                let mut row = Vec::new();
                let backend_backup = key.backup.map(|b| if b { "X" } else { "" }).unwrap_or("");
                row.push(cell!(key.backend_id));
                row.push(cell!(format!("{}", key.address)));
                row.push(cell!(backend_backup));

                for val in values.iter() {
                  if keys.contains(val) {
                    row.push(cell!(String::from("X")));
                  } else {
                    row.push(cell!(String::from("")));
                  }
                }

                backend_table.add_row(Row::new(row));
              }

              backend_table.printstd();
            }
          } else {
            if let Some(CommandResponseData::Query(data)) = message.data {
              let mut table = Table::new();
              let mut header = Vec::new();
              header.push(cell!("key"));
              for ref key in data.keys() {
                header.push(cell!(&key));
              }
              header.push(cell!("desynchronized"));
              table.add_row(Row::new(header));

              let mut query_data = HashMap::new();

              for ref metrics in data.values() {
                //let m: u8 = metrics;
                if let &QueryAnswer::ApplicationsHashes(ref apps) = *metrics {
                  for (ref key, ref value) in apps.iter() {
                    (*(query_data.entry((*key).clone()).or_insert(Vec::new()))).push(*value);
                  }
                }
              }

              for (ref key, ref values) in query_data.iter() {
                let mut row = Vec::new();
                row.push(cell!(key));

                for val in values.iter() {
                  row.push(cell!(format!("{}", val)));
                }

                let hs: HashSet<&u64> = values.iter().cloned().collect();

                let diff = hs.len() > 1;

                if diff {
                  row.push(cell!(String::from("X")));
                } else {
                  row.push(cell!(String::from("")));
                }


                table.add_row(Row::new(row));
              }

              table.printstd();
            }
          }
        }
      }
    }
  }
}

pub fn query_certificate(mut channel: Channel<CommandRequest,CommandResponse>, json: bool, fingerprint: Option<String>, domain: Option<String>) {

  let query = match (fingerprint, domain) {
    (None, None) => QueryCertificateType::All,
    (Some(f), None) => {
      match hex::decode(f) {
        Err(e) => {
          eprintln!("invalid fingerprint: {:?}", e);
          exit(1);
        },
        Ok(f) => QueryCertificateType::Fingerprint(f),
      }
    },
    (None, Some(d)) => QueryCertificateType::Domain(d),
    (Some(_), Some(_)) => {
      eprintln!("Error: Either request a fingerprint or a domain name");
      exit(1);
    }
  };

  let command = CommandRequestData::Proxy(ProxyRequestData::Query(Query::Certificates(query)));

  let id = generate_id();
  channel.write_message(&CommandRequest::new(
    id.clone(),
    command,
    None,
  ));

  match channel.read_message() {
    None          => {
      eprintln!("the proxy didn't answer");
      exit(1);
    },
    Some(message) => {
      if id != message.id {
        eprintln!("received message with invalid id: {:?}", message);
        exit(1);
      }
      match message.status {
        CommandStatus::Processing => {
          // do nothing here
          // for other messages, we would loop over read_message
          // until an error or ok message was sent
        },
        CommandStatus::Error => {
          if json {
            print_json_response(&message.message);
          } else {
            eprintln!("could not query proxy state: {}", message.message);
          }
          exit(1);
        },
        CommandStatus::Ok => {
            if let Some(CommandResponseData::Query(data)) = message.data {
              if json {
                print_json_response(&data);
                return;
              }

              //println!("received: {:?}", data);
              let it = data.iter().map(|(k,v)| {
                match v {
                  QueryAnswer::Certificates(c) => (k, c),
                  v => {
                    eprintln!("unexpected certificates query answer: {:?}", v);
                    exit(1);
                  }
                }});

              for (k, v) in it {
                println!("process '{}':", k);

                match v {
                  QueryAnswerCertificate::All(h) => {
                    for (addr, h2) in  h.iter() {
                      println!("\t{}:", addr);

                      for (domain, fingerprint) in h2.iter() {
                        println!("\t\t{}:\t{}", domain, hex::encode(fingerprint));
                      }

                      println!("");
                    }
                  },
                  QueryAnswerCertificate::Domain(h) => {
                    for (addr, opt) in  h.iter() {
                      println!("\t{}:", addr);
                      if let Some((key, fingerprint)) = opt {
                        println!("\t\t{}:\t{}", key, hex::encode(fingerprint));
                      } else {
                        println!("\t\tnot found");
                      }

                      println!("");
                    }

                  },
                  QueryAnswerCertificate::Fingerprint(opt) => {
                    if let Some((s, v)) = opt {
                      println!("\tfrontends: {:?}\ncertificate:\n{}", v, s);
                    } else {
                      println!("\tnot found");
                    }
                  }
                }
                println!("");
              }

            } else {
              eprintln!("unexpected response: {:?}", message.data);
              exit(1);

            }
        }
      }
    }
  }
}

pub fn logging_filter(channel: Channel<CommandRequest,CommandResponse>, timeout: u64, filter: &str) {
  order_command(channel, timeout, ProxyRequestData::Logging(String::from(filter)));
}

pub fn events(mut channel: Channel<CommandRequest,CommandResponse>) {
  let id = generate_id();
  channel.write_message(&CommandRequest::new(
    id.clone(),
    CommandRequestData::SubscribeEvents,
    None,
  ));

  loop {
    match channel.read_message() {
      None          => {
        eprintln!("the proxy didn't answer");
        exit(1);
      },
      Some(message) => {
        match message.status {
          CommandStatus::Processing => {
            if let Some(CommandResponseData::Event(event)) = message.data {
              println!("got event from worker({}): {:?}", message.message, event);
            }
          },
          CommandStatus::Error => {
            eprintln!("could not get proxy events: {}", message.message);
            exit(1);
          },
          CommandStatus::Ok => {
            println!("{}", message.message);
            return;
          }
        }
      }
    }
  }
}

fn order_command(mut channel: Channel<CommandRequest,CommandResponse>, timeout: u64, order: ProxyRequestData) {
  let id = generate_id();
  channel.write_message(&CommandRequest::new(
    id.clone(),
    CommandRequestData::Proxy(order.clone()),
    None,
  ));

  command_timeout!(timeout, {
    match channel.read_message() {
      None          => {
        eprintln!("the proxy didn't answer");
        exit(1);
      },
      Some(message) => {
        if id != message.id {
          eprintln!("received message with invalid id: {:?}", message);
          exit(1);
        }
        match message.status {
          CommandStatus::Processing => {
            // do nothing here
            // for other messages, we would loop over read_message
            // until an error or ok message was sent
          },
          CommandStatus::Error => {
            eprintln!("could not execute order: {}", message.message);
            exit(1);
          },
          CommandStatus::Ok => {
            //deactivate success messages for now
            /*
            match order {
              ProxyRequestData::AddApplication(_) => println!("application added : {}", message.message),
              ProxyRequestData::RemoveApplication(_) => println!("application removed : {} ", message.message),
              ProxyRequestData::AddBackend(_) => println!("backend added : {}", message.message),
              ProxyRequestData::RemoveBackend(_) => println!("backend removed : {} ", message.message),
              ProxyRequestData::AddCertificate(_) => println!("certificate added: {}", message.message),
              ProxyRequestData::RemoveCertificate(_) => println!("certificate removed: {}", message.message),
              ProxyRequestData::AddHttpFront(_) => println!("front added: {}", message.message),
              ProxyRequestData::RemoveHttpFront(_) => println!("front removed: {}", message.message),
              _ => {
                // do nothing for now
              }
            }
            */
          }
        }
      }
    }
  });
}

fn print_json_response<T: ::serde::Serialize>(input: &T) {
  match serde_json::to_string_pretty(&input) {
    Ok(to_print) => println!("{}", to_print),
    Err(e) => {
      eprintln!("Error while parsing response to JSON: {:?}", e);
      exit(1);
    }
  };
}

fn load_full_certificate(certificate_path: &str, certificate_chain_path: &str,
                         key_path: &str, versions: Vec<TlsVersion>) -> Option<CertificateAndKey> {
  match Config::load_file(certificate_path) {
    Err(e) => {
      eprintln!("could not load certificate: {:?}", e);
      exit(1);
    },
    Ok(certificate) => {
      match Config::load_file(certificate_chain_path).map(split_certificate_chain) {
        Err(e) => {
          eprintln!("could not load certificate chain: {:?}", e);
          exit(1);
        },
        Ok(certificate_chain) => {
          match Config::load_file(key_path) {
            Err(e) => {
              eprintln!("could not load key: {:?}", e);
              exit(1);
            },
            Ok(key) => {
              Some(CertificateAndKey {
                certificate,
                certificate_chain,
                key,
                versions,
              })
            }
          }
        }
      }
    }
  }
}

fn get_certificate_fingerprint(certificate_path: &str) -> Option<CertFingerprint> {
  match Config::load_file_bytes(certificate_path) {
    Ok(data) => {
      match calculate_fingerprint(&data) {
        Some(fingerprint) => Some(CertFingerprint(fingerprint)),
        None              => {
          eprintln!("could not calculate finrprint for certificate");
          exit(1);
        }
      }
    },
    Err(e) => {
      eprintln!("could not load file: {:?}", e);
      exit(1);
    }
  }
}

fn create_queried_application_table(headers: Vec<&str>, data: &BTreeMap<String, QueryAnswer>) -> Table {
  let mut table = Table::new();
  let mut row_header: Vec<_> = headers.iter().map(|h| cell!(h)).collect();
  for ref key in data.keys() {
    row_header.push(cell!(&key));
  }
  table.add_row(Row::new(row_header));
  table
}
