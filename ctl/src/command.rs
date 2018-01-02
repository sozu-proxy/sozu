use sozu_command::config::Config;
use sozu_command::channel::Channel;
use sozu_command::certificate::{calculate_fingerprint,split_certificate_chain};
use sozu_command::data::{AnswerData,ConfigCommand,ConfigMessage,ConfigMessageAnswer,ConfigMessageStatus,RunState};
use sozu_command::messages::{Application, Order, Instance, HttpFront, HttpsFront, TcpFront,
  CertificateAndKey, CertFingerprint, Query, QueryAnswer, QueryApplicationType, QueryApplicationDomain};

use serde_json;
use std::collections::{HashMap,HashSet};
use std::process::exit;
use std::thread;
use std::sync::{Arc,Mutex};
use std::time::Duration;
use std::sync::mpsc;
use rand::{thread_rng, Rng};
use prettytable::Table;
use prettytable::row::Row;
use super::create_channel;


fn generate_id() -> String {
  let s: String = thread_rng().gen_ascii_chars().take(6).collect();
  format!("ID-{}", s)
}

fn generate_tagged_id(tag: &str) -> String {
  let s: String = thread_rng().gen_ascii_chars().take(6).collect();
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

pub fn save_state(mut channel: Channel<ConfigMessage,ConfigMessageAnswer>, timeout: u64, path: String) {
  let id = generate_id();
  channel.write_message(&ConfigMessage::new(
    id.clone(),
    ConfigCommand::SaveState(path),
    None,
  ));

  command_timeout!(timeout, {
    match channel.read_message() {
      None          => {
        println!("the proxy didn't answer");
        exit(1);
      },
      Some(message) => {
        if id != message.id {
          println!("received message with invalid id: {:?}", message);
          exit(1);
        }
        match message.status {
          ConfigMessageStatus::Processing => {
            // do nothing here
            // for other messages, we would loop over read_message
            // until an error or ok message was sent
          },
          ConfigMessageStatus::Error => {
            println!("could not save proxy state: {}", message.message);
            exit(1);
          },
          ConfigMessageStatus::Ok => {
            println!("{}", message.message);
          }
        }
      }
    }
  });
}

pub fn load_state(mut channel: Channel<ConfigMessage,ConfigMessageAnswer>, timeout: u64, path: String) {
  let id = generate_id();
  channel.write_message(&ConfigMessage::new(
    id.clone(),
    ConfigCommand::LoadState(path.clone()),
    None,
  ));

  command_timeout!(timeout, {
    match channel.read_message() {
      None          => {
        println!("the proxy didn't answer");
        exit(1);
      },
      Some(message) => {
        if id != message.id {
          println!("received message with invalid id: {:?}", message);
          exit(1);
        }
        match message.status {
          ConfigMessageStatus::Processing => {
            // do nothing here
            // for other messages, we would loop over read_message
            // until an error or ok message was sent
          },
          ConfigMessageStatus::Error => {
            println!("could not load proxy state: {}", message.message);
            exit(1);
          },
          ConfigMessageStatus::Ok => {
            println!("Proxy state loaded successfully from {}", path);
          }
        }
      }
    };
  });
}

pub fn dump_state(mut channel: Channel<ConfigMessage,ConfigMessageAnswer>, timeout: u64, json: bool) {
  let id = generate_id();
  channel.write_message(&ConfigMessage::new(
    id.clone(),
    ConfigCommand::DumpState,
    None,
  ));

  command_timeout!(timeout, {
    match channel.read_message() {
      None          => {
        println!("the proxy didn't answer");
        exit(1);
      },
      Some(message) => {
        if id != message.id {
          println!("received message with invalid id: {:?}", message);
          exit(1);
        }
        match message.status {
          ConfigMessageStatus::Processing => {
            // do nothing here
            // for other messages, we would loop over read_message
            // until an error or ok message was sent
          },
          ConfigMessageStatus::Error => {
            if json {
              print_json_response(&message.message);
            } else {
              println!("could not dump proxy state: {}", message.message);
            }
            exit(1);
          },
          ConfigMessageStatus::Ok => {
            if let Some(AnswerData::State(state)) = message.data {
              if json {
                print_json_response(&state);
              } else {
                println!("{:#?}", state);
              }
            } else {
              println!("state dump was empty");
              exit(1);
            }
          }
        }
      }
    }
  });
}

pub fn soft_stop(mut channel: Channel<ConfigMessage,ConfigMessageAnswer>) {
  println!("shutting down proxy");
  let id = generate_id();
  channel.write_message(&ConfigMessage::new(
    id.clone(),
    ConfigCommand::ProxyConfiguration(Order::SoftStop),
    //FIXME: we should be able to soft stop one specific worker
    None,
  ));

  loop {
    match channel.read_message() {
      None          => println!("the proxy didn't answer"),
      Some(message) => {
        if &id != &message.id {
          println!("received message with invalid id: {:?}", message);
          return;
        }
        match message.status {
          ConfigMessageStatus::Processing => {
            println!("Proxy is processing: {}", message.message);
          },
          ConfigMessageStatus::Error => {
            println!("could not stop the proxy: {}", message.message);
          },
          ConfigMessageStatus::Ok => {
            println!("Proxy shut down: {}", message.message);
            break;
          }
        }
      }
    }
  }
}

pub fn hard_stop(mut channel: Channel<ConfigMessage,ConfigMessageAnswer>, timeout: u64) {
  println!("shutting down proxy");
  let id = generate_id();
  channel.write_message(&ConfigMessage::new(
    id.clone(),
    ConfigCommand::ProxyConfiguration(Order::HardStop),
    //FIXME: we should be able to soft stop one specific worker
    None,
  ));

  command_timeout!(timeout,
    loop {
      match channel.read_message() {
        None          => println!("the proxy didn't answer"),
        Some(message) => {
          match message.status {
            ConfigMessageStatus::Processing => {
              println!("Proxy is processing: {}", message.message);
            },
            ConfigMessageStatus::Error => {
              println!("could not stop the proxy: {}", message.message);
            },
            ConfigMessageStatus::Ok => {
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

pub fn upgrade(mut channel: Channel<ConfigMessage,ConfigMessageAnswer>,
                  socket_path: &str) {
  println!("Preparing to upgrade proxy...");

  let id = generate_tagged_id("LIST-WORKERS");
  channel.write_message(&ConfigMessage::new(
    id.clone(),
    ConfigCommand::ListWorkers,
    None,
  ));

  match channel.read_message() {
    None          => println!("Error: the proxy didn't list workers"),
    Some(message) => {
      if id != message.id {
        println!("Error: received unexpected message: {:?}", message);
        return;
      }
      match message.status {
        ConfigMessageStatus::Processing => {
          println!("Error: the proxy didn't return list of workers immediately");
          return;
        },
        ConfigMessageStatus::Error => {
          println!("Error: failed to get the list of worker: {}", message.message);
          return
        },
        ConfigMessageStatus::Ok => {
          if let Some(AnswerData::Workers(ref workers)) = message.data {
            let mut table = Table::new();
            table.add_row(row!["Worker", "pid", "run state"]);
            for ref worker in workers.iter() {
              let run_state = format!("{:?}", worker.run_state);
              table.add_row(row![worker.id, worker.pid, run_state]);
            }
            println!("");
            table.printstd();
            println!("");

            let id = generate_tagged_id("UPGRADE-MASTER");
            channel.write_message(&ConfigMessage::new(
              id.clone(),
              ConfigCommand::UpgradeMaster,
              None,
            ));
            println!("Upgrading master process");

            loop {
              match channel.read_message() {
                None          => {
                  println!("Error: the proxy didn't start master upgrade");
                  return;
                },
                Some(message) => {
                  if &id != &message.id {
                    println!("Error: received unexpected message: {:?}", message);
                    return;
                  }
                  match message.status {
                    ConfigMessageStatus::Processing => {},
                    ConfigMessageStatus::Error => {
                      println!("Error: failed to upgrade the master: {}", message.message);
                      return;
                    },
                    ConfigMessageStatus::Ok => {
                      println!("Master process upgrade succeeded: {}", message.message);
                      break;
                    },
                  }
                }
              }
            }

            // Reconnect to the new master
            println!("Reconnecting to new master process...");
            let mut channel = create_channel(socket_path).expect("could not reconnect to the command unix socket");

            // Do a rolling restart of the workers
            let running_workers = workers.iter()
              .filter(|worker| worker.run_state == RunState::Running)
              .collect::<Vec<_>>();
            let running_count = running_workers.len();
            for (i, ref worker) in running_workers.iter().enumerate() {
              println!("Upgrading worker {} (of {})", i+1, running_count);

              let id = generate_tagged_id("LAUNCH-WORKER");
              let msg = ConfigMessage::new(
                id.clone(),
                ConfigCommand::LaunchWorker("BLAH".to_string()),
                None,
              );
              channel.write_message(&msg);

              loop {
                match channel.read_message() {
                  None          => {
                    println!("Error: the proxy didn't launch a new worker");
                    return;
                  },
                  Some(message) => {
                    if &id != &message.id {
                      println!("Error: received unexpected message: {:?}", message);
                      return;
                    }
                    match message.status {
                      ConfigMessageStatus::Processing => {},
                      ConfigMessageStatus::Error => {
                        println!("Error: failed to launch a new worker: {}", message.message);
                        return;
                      },
                      ConfigMessageStatus::Ok => break,
                    }
                  }
                }
              }

              // Stop the old worker
              let id = generate_tagged_id("SOFT-STOP-WORKER");
              let msg = ConfigMessage::new(
                id.clone(),
                ConfigCommand::ProxyConfiguration(Order::SoftStop),
                Some(worker.id),
              );
              channel.write_message(&msg);

              loop {
                match channel.read_message() {
                  None          => {
                    println!("Error: the proxy didn't stop the old worker");
                    return;
                  }
                  Some(message) => {
                    if &id != &message.id {
                      println!("Error: received unexpected message: {:?}", message);
                      return;
                    }
                    match message.status {
                      ConfigMessageStatus::Processing => {},
                      ConfigMessageStatus::Error => {
                        println!("Error: failed to stop old worker: {}", message.message);
                        return;
                      },
                      ConfigMessageStatus::Ok => break,
                    }
                  }
                }
              }
            }

            println!("Proxy successfully upgraded!");
          }
        }
      }
    }
  }
}

pub fn status(mut channel: Channel<ConfigMessage,ConfigMessageAnswer>) {
  let id = generate_id();
  channel.write_message(&ConfigMessage::new(
    id.clone(),
    ConfigCommand::ListWorkers,
    None,
  ));

  match channel.read_message() {
    None          => {
      println!("the proxy didn't answer");
      exit(1);
    },
    Some(message) => {
      if id != message.id {
        println!("received message with invalid id: {:?}", message);
        exit(1);
      }
      match message.status {
        ConfigMessageStatus::Processing => {
          println!("should have obtained an answer immediately");
          exit(1);
        },
        ConfigMessageStatus::Error => {
          println!("could not get the worker list: {}", message.message);
          exit(1);
        },
        ConfigMessageStatus::Ok => {
          //println!("Worker list:\n{:?}", message.data);
          if let Some(AnswerData::Workers(ref workers)) = message.data {
            let mut expecting: HashSet<String> = HashSet::new();

            let mut h = HashMap::new();
            for ref worker in workers.iter().filter(|worker| worker.run_state == RunState::Running) {
              let id = generate_id();
              let msg = ConfigMessage::new(
                id.clone(),
                ConfigCommand::ProxyConfiguration(Order::Status),
                Some(worker.id),
              );
              //println!("sending message: {:?}", msg);
              channel.write_message(&msg);
              expecting.insert(id.clone());
              h.insert(id, (worker.id, ConfigMessageStatus::Processing));
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
                    println!("the proxy didn't answer");
                    exit(1);
                  },
                  Some(message) => {
                    //println!("received message: {:?}", message);
                    match message.status {
                      ConfigMessageStatus::Processing => {
                      },
                      ConfigMessageStatus::Error => {
                        println!("error for message[{}]: {}", message.id, message.message);
                        if expecting.contains(&message.id) {
                          expecting.remove(&message.id);
                          //println!("status message with ID {} done", message.id);
                          if let Ok(mut h) = state.try_lock() {
                            if let Some(data) = h.get_mut(&message.id) {
                              *data = ((*data).0, ConfigMessageStatus::Error);
                            }
                          }
                        }
                      },
                      ConfigMessageStatus::Ok => {
                        if expecting.contains(&message.id) {
                          expecting.remove(&message.id);
                          //println!("status message with ID {} done", message.id);
                          if let Ok(mut h) = state.try_lock() {
                            if let Some(data) = h.get_mut(&message.id) {
                              *data = ((*data).0, ConfigMessageStatus::Ok);
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

            let mut h2: HashMap<u32, String> = if let Ok(mut state) = st.try_lock() {
              state.values().map(|&(ref id, ref status)| {
                (*id, String::from(match *status {
                  ConfigMessageStatus::Processing => if finished {
                    "processing"
                  } else {
                    "timeout"
                  },
                  ConfigMessageStatus::Error      => "error",
                  ConfigMessageStatus::Ok         => "ok",
                }))
              }).collect()
            } else {
              HashMap::new()
            };

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

pub fn metrics(mut channel: Channel<ConfigMessage,ConfigMessageAnswer>, json: bool) {
  let id = generate_id();
  //println!("will send message for metrics with id {}", id);
  channel.write_message(&ConfigMessage::new(
    id.clone(),
    ConfigCommand::Metrics,
    None,
  ));
  //println!("message sent");

  loop {
    match channel.read_message() {
      None          => println!("the proxy didn't answer"),
      Some(message) => {
        match message.status {
          ConfigMessageStatus::Processing => {
            println!("Proxy is processing: {}", message.message);
          },
          ConfigMessageStatus::Error => {
            if json {
              print_json_response(&message.message);
            } else {
              println!("could not stop the proxy: {}", message.message);
            }
          },
          ConfigMessageStatus::Ok => {
            if &id == &message.id {
              //println!("Sozu metrics:\n{}\n{:#?}", message.message, message.data);

              if let Some(AnswerData::Metrics(mut data)) = message.data {
                if json {
                  print_json_response(&data);
                  return;
                }

                if let Some(master) = data.remove("master") {
                  let mut master_table = Table::new();
                  master_table.add_row(row![String::from("key"), String::from("value")]);

                  println!("master process metrics:\n");
                  for (ref key, ref value) in master.proxy.iter() {
                    master_table.add_row(row![key.to_string(), format!("{:?}", value)]);
                     //println!("{}:\t{:?}", key, value);
                  }

                  master_table.printstd();
                }

                println!("\nworker metrics:\n");

                let mut proxy_table = Table::new();
                let mut header = Vec::new();
                header.push(cell!("key"));
                for ref key in data.keys() {
                  header.push(cell!(&key));
                }
                proxy_table.add_row(Row::new(header));

                let mut application_table = Table::new();
                let mut header = Vec::new();
                header.push(cell!("application"));
                for ref key in data.keys() {
                  header.push(cell!(&format!("{} samples", key)));
                  header.push(cell!(&format!("{} p50%", key)));
                  header.push(cell!(&format!("{} p90%", key)));
                  header.push(cell!(&format!("{} p99%", key)));
                  header.push(cell!(&format!("{} p99.9%", key)));
                  header.push(cell!(&format!("{} p99.99%", key)));
                  header.push(cell!(&format!("{} p99.999%", key)));
                  header.push(cell!(&format!("{} p100%", key)));
                }
                application_table.add_row(Row::new(header));

                let mut backend_table = Table::new();
                let mut header = Vec::new();
                header.push(cell!("backend"));
                for ref key in data.keys() {
                  header.push(cell!(&format!("{} bytes out", key)));
                  header.push(cell!(&format!("{} bytes in", key)));
                  header.push(cell!(&format!("{} samples", key)));
                  header.push(cell!(&format!("{} p50%", key)));
                  header.push(cell!(&format!("{} p90%", key)));
                  header.push(cell!(&format!("{} p99%", key)));
                  header.push(cell!(&format!("{} p99.9%", key)));
                  header.push(cell!(&format!("{} p99.99%", key)));
                  header.push(cell!(&format!("{} p99.999%", key)));
                  header.push(cell!(&format!("{} p100%", key)));
                }
                backend_table.add_row(Row::new(header));


                let mut proxy_data = HashMap::new();
                let mut application_data = HashMap::new();
                let mut backend_data = HashMap::new();

                for ref metrics in data.values() {
                  for (ref key, ref value) in metrics.proxy.iter() {
                    (*(proxy_data.entry(key.clone()).or_insert(Vec::new()))).push(value.clone());
                  }

                  for (ref key, ref percentiles) in metrics.applications.iter() {
                    let mut entry = application_data.entry(key.clone()).or_insert(Vec::new());
                    (*entry).push(percentiles.samples);
                    (*entry).push(percentiles.p_50);
                    (*entry).push(percentiles.p_90);
                    (*entry).push(percentiles.p_99);
                    (*entry).push(percentiles.p_99_9);
                    (*entry).push(percentiles.p_99_99);
                    (*entry).push(percentiles.p_99_999);
                    (*entry).push(percentiles.p_100);
                  }

                  for (ref key, ref back) in metrics.backends.iter() {
                    let mut entry = backend_data.entry(key.clone()).or_insert(Vec::new());
                    let bout = back.bytes_out as u64;
                    let bin  = back.bytes_in as u64;
                    (*entry).push(bout);
                    (*entry).push(bin);
                    (*entry).push(back.percentiles.samples);
                    (*entry).push(back.percentiles.p_50);
                    (*entry).push(back.percentiles.p_90);
                    (*entry).push(back.percentiles.p_99);
                    (*entry).push(back.percentiles.p_99_9);
                    (*entry).push(back.percentiles.p_99_99);
                    (*entry).push(back.percentiles.p_99_999);
                    (*entry).push(back.percentiles.p_100);
                  }
                }

                for (ref key, ref values) in proxy_data.iter() {
                  let mut row = Vec::new();
                  row.push(cell!(key));

                  for val in values.iter() {
                    row.push(cell!(format!("{:?}", val)));
                  }

                  proxy_table.add_row(Row::new(row));
                }

                proxy_table.printstd();

                println!("\napplication metrics:\n");

                for (ref key, ref values) in application_data.iter() {
                  let mut row = Vec::new();
                  row.push(cell!(key));

                  for val in values.iter() {
                    row.push(cell!(format!("{:?}", val)));
                  }

                  application_table.add_row(Row::new(row));
                }

                application_table.printstd();

                println!("\nbackend metrics:\n");

                for (ref key, ref values) in backend_data.iter() {
                  let mut row = Vec::new();
                  row.push(cell!(key));

                  for val in values.iter() {
                    row.push(cell!(format!("{:?}", val)));
                  }

                  backend_table.add_row(Row::new(row));
                }

                backend_table.printstd();
                break;
              }
            }
          }
        }
      }
    }
  }
}

pub fn add_application(channel: Channel<ConfigMessage,ConfigMessageAnswer>, timeout: u64, app_id: &str, sticky_session: bool, https_redirect: bool) {
  order_command(channel, timeout, Order::AddApplication(Application {
    app_id:         String::from(app_id),
    sticky_session: sticky_session,
    https_redirect: https_redirect
  }));
}

pub fn remove_application(channel: Channel<ConfigMessage,ConfigMessageAnswer>, timeout: u64, app_id: &str) {
  order_command(channel, timeout, Order::RemoveApplication(String::from(app_id)));
}

pub fn add_http_frontend(channel: Channel<ConfigMessage,ConfigMessageAnswer>, timeout: u64, app_id: &str, hostname: &str, path_begin: &str, certificate: Option<String>) {
  if let Some(certificate_path) = certificate {
    match Config::load_file_bytes(&certificate_path) {
      Ok(data) => {
        match calculate_fingerprint(&data) {
          None              => println!("could not calculate fingerprint for certificate"),
          Some(fingerprint) => {
            order_command(channel, timeout, Order::AddHttpsFront(HttpsFront {
              app_id: String::from(app_id),
              hostname: String::from(hostname),
              path_begin: String::from(path_begin),
              fingerprint: CertFingerprint(fingerprint),
            }));
          },
        }
      },
      Err(e) => println!("could not load file: {:?}", e)
    }
  } else {
    order_command(channel, timeout, Order::AddHttpFront(HttpFront {
      app_id: String::from(app_id),
      hostname: String::from(hostname),
      path_begin: String::from(path_begin),
    }));
  }
}

pub fn remove_http_frontend(channel: Channel<ConfigMessage,ConfigMessageAnswer>, timeout: u64, app_id: &str, hostname: &str, path_begin: &str, certificate: Option<String>) {
  if let Some(certificate_path) = certificate {
    match Config::load_file_bytes(&certificate_path) {
      Ok(data) => {
        match calculate_fingerprint(&data) {
          None              => println!("could not calculate fingerprint for certificate"),
          Some(fingerprint) => {
            order_command(channel, timeout, Order::RemoveHttpsFront(HttpsFront {
              app_id: String::from(app_id),
              hostname: String::from(hostname),
              path_begin: String::from(path_begin),
              fingerprint: CertFingerprint(fingerprint),
            }));
          },
        }
      },
      Err(e) => println!("could not load file: {:?}", e)
    }
  } else {
    order_command(channel, timeout, Order::RemoveHttpFront(HttpFront {
      app_id: String::from(app_id),
      hostname: String::from(hostname),
      path_begin: String::from(path_begin),
    }));
  }
}


pub fn add_backend(channel: Channel<ConfigMessage,ConfigMessageAnswer>, timeout: u64, app_id: &str, instance_id: &str, ip: &str, port: u16) {
  order_command(channel, timeout, Order::AddInstance(Instance {
      app_id: String::from(app_id),
      instance_id: String::from(instance_id),
      ip_address: String::from(ip),
      port: port
    }));
}

pub fn remove_backend(channel: Channel<ConfigMessage,ConfigMessageAnswer>, timeout: u64, app_id: &str, instance_id: &str, ip: &str, port: u16) {
    order_command(channel, timeout, Order::RemoveInstance(Instance {
      app_id: String::from(app_id),
      instance_id: String::from(instance_id),
      ip_address: String::from(ip),
      port: port
    }));
}

pub fn add_certificate(channel: Channel<ConfigMessage,ConfigMessageAnswer>, timeout: u64, certificate_path: &str, certificate_chain_path: &str, key_path: &str) {
  match Config::load_file(certificate_path) {
    Err(e) => println!("could not load certificate: {:?}", e),
    Ok(certificate) => {
      match Config::load_file(certificate_chain_path).map(split_certificate_chain) {
        Err(e) => println!("could not load certificate chain: {:?}", e),
        Ok(certificate_chain) => {
          match Config::load_file(key_path) {
            Err(e) => println!("could not load key: {:?}", e),
            Ok(key) => {
              order_command(channel, timeout, Order::AddCertificate(CertificateAndKey {
                certificate: certificate,
                certificate_chain: certificate_chain,
                key: key
              }));

            }
          }
        }
      }
    }
  }
}

pub fn remove_certificate(channel: Channel<ConfigMessage,ConfigMessageAnswer>, timeout: u64, certificate_path: &str) {
  match Config::load_file_bytes(certificate_path) {
    Ok(data) => {
      match calculate_fingerprint(&data) {
        Some(fingerprint) => order_command(channel, timeout, Order::RemoveCertificate(CertFingerprint(fingerprint))),
        None              => println!("could not calculate finrprint for certificate")
      }
    },
    Err(e) => println!("could not load file: {:?}", e)
  }
}

pub fn add_tcp_frontend(channel: Channel<ConfigMessage,ConfigMessageAnswer>, timeout: u64, app_id: &str, ip_address: &str, port: u16) {
  order_command(channel, timeout, Order::AddTcpFront(TcpFront {
    app_id: String::from(app_id),
    ip_address: String::from(ip_address),
    port: port
  }));
}

pub fn remove_tcp_frontend(channel: Channel<ConfigMessage,ConfigMessageAnswer>, timeout: u64, app_id: &str, ip_address: &str, port: u16) {
  order_command(channel, timeout, Order::RemoveTcpFront(TcpFront {
    app_id: String::from(app_id),
    ip_address: String::from(ip_address),
    port: port
  }));
}

pub fn query_application(mut channel: Channel<ConfigMessage,ConfigMessageAnswer>, json: bool, application_id: Option<String>, domain: Option<String>) {
  if application_id.is_some() && domain.is_some() {
    println!("Error: Either request an application ID or a domain name");
    return;
  }

  let command = if let Some(ref app_id) = application_id {
    ConfigCommand::Query(Query::Applications(QueryApplicationType::AppId(app_id.to_string())))
  } else if let Some(ref domain) = domain {
    let splitted: Vec<String> = domain.splitn(2, "/").map(|elem| elem.to_string()).collect();

    if splitted.len() == 0 {
      println!("Domain can't be empty");
      return;
    }

    let query_domain = QueryApplicationDomain {
      hostname: splitted.get(0).expect("Domain can't be empty").clone(),
      path_begin: splitted.get(1).cloned().map(|path| format!("/{}", path)) // We add the / again because of the splitn removing it
    };

    ConfigCommand::Query(Query::Applications(QueryApplicationType::Domain(query_domain)))
  } else {
    ConfigCommand::Query(Query::ApplicationsHashes)
  };

  let id = generate_id();
  channel.write_message(&ConfigMessage::new(
    id.clone(),
    command,
    None,
  ));

  match channel.read_message() {
    None          => println!("the proxy didn't answer"),
    Some(message) => {
      if id != message.id {
        println!("received message with invalid id: {:?}", message);
        return;
      }
      match message.status {
        ConfigMessageStatus::Processing => {
          // do nothing here
          // for other messages, we would loop over read_message
          // until an error or ok message was sent
        },
        ConfigMessageStatus::Error => {
          if json {
            print_json_response(&message.message);
          } else {
            println!("could not query proxy state: {}", message.message);
          }
        },
        ConfigMessageStatus::Ok => {
          if let Some(needle) = application_id.or(domain) {
            if let Some(AnswerData::Query(data)) = message.data {
              if json {
                print_json_response(&data);
                return;
              }

              let mut application_table = Table::new();
              let mut header = Vec::new();
              header.push(cell!("id"));
              header.push(cell!("sticky_session"));
              for ref key in data.keys() {
                header.push(cell!(&key));
              }
              application_table.add_row(Row::new(header));

              let mut frontend_table = Table::new();
              let mut header = Vec::new();
              header.push(cell!("id"));
              header.push(cell!("hostname"));
              header.push(cell!("path begin"));
              for ref key in data.keys() {
                header.push(cell!(&key));
              }
              frontend_table.add_row(Row::new(header));

              let mut https_frontend_table = Table::new();
              let mut header = Vec::new();
              header.push(cell!("id"));
              header.push(cell!("hostname"));
              header.push(cell!("path begin"));
              header.push(cell!("fingerprint"));
              for ref key in data.keys() {
                header.push(cell!(&key));
              }
              https_frontend_table.add_row(Row::new(header));

              let mut backend_table = Table::new();
              let mut header = Vec::new();
              header.push(cell!("instance id"));
              header.push(cell!("IP address"));
              header.push(cell!("port"));
              for ref key in data.keys() {
                header.push(cell!(&key));
              }
              backend_table.add_row(Row::new(header));

              let keys : HashSet<&String> = data.keys().collect();

              let mut application_data = HashMap::new();
              let mut frontend_data = HashMap::new();
              let mut https_frontend_data = HashMap::new();
              let mut backend_data = HashMap::new();

              for (ref key, ref metrics) in data.iter() {
                //let m: u8 = metrics;
                if let &QueryAnswer::Applications(ref apps) = *metrics {
                  for app in apps.iter() {
                    let mut entry = application_data.entry(app).or_insert(Vec::new());
                    entry.push(key.clone());

                    for frontend in app.http_frontends.iter() {
                      let mut entry = frontend_data.entry(frontend).or_insert(Vec::new());
                      entry.push(key.clone());
                    }

                    for frontend in app.https_frontends.iter() {
                      let mut entry = https_frontend_data.entry(frontend).or_insert(Vec::new());
                      entry.push(key.clone());
                    }

                    for backend in app.backends.iter() {
                      let mut entry = backend_data.entry(backend).or_insert(Vec::new());
                      entry.push(key.clone());
                    }
                  }
                }
              }

              println!("Application level configuration for {}:\n", needle);

              for (ref key, ref values) in application_data.iter() {
                let mut row = Vec::new();
                row.push(cell!(key.configuration.clone().map(|conf| conf.app_id).unwrap_or(String::from(""))));
                row.push(cell!(key.configuration.clone().map(|conf| conf.sticky_session).unwrap_or(false)));

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
                row.push(cell!(format!("{}", key.fingerprint)));

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

              println!("\nbackends configuration for {}:\n", needle);

              for (ref key, ref values) in backend_data.iter() {
                let mut row = Vec::new();
                row.push(cell!(key.instance_id));
                row.push(cell!(key.ip_address));
                row.push(cell!(format!("{}", key.port)));

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
            if let Some(AnswerData::Query(data)) = message.data {
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
                    (*(query_data.entry(key.clone()).or_insert(Vec::new()))).push(value.clone());
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

pub fn logging_filter(channel: Channel<ConfigMessage,ConfigMessageAnswer>, timeout: u64, filter: &str) {
  order_command(channel, timeout, Order::Logging(String::from(filter)));
}

fn order_command(mut channel: Channel<ConfigMessage,ConfigMessageAnswer>, timeout: u64, order: Order) {
  let id = generate_id();
  channel.write_message(&ConfigMessage::new(
    id.clone(),
    ConfigCommand::ProxyConfiguration(order.clone()),
    None,
  ));

  command_timeout!(timeout, {
    match channel.read_message() {
      None          => println!("the proxy didn't answer"),
      Some(message) => {
        if id != message.id {
          println!("received message with invalid id: {:?}", message);
          return;
        }
        match message.status {
          ConfigMessageStatus::Processing => {
            // do nothing here
            // for other messages, we would loop over read_message
            // until an error or ok message was sent
          },
          ConfigMessageStatus::Error => {
            println!("could not execute order: {}", message.message);
            exit(1);
          },
          ConfigMessageStatus::Ok => {
            //deactivate success messages for now
            /*
            match order {
              Order::AddApplication(_) => println!("application added : {}", message.message),
              Order::RemoveApplication(_) => println!("application removed : {} ", message.message),
              Order::AddInstance(_) => println!("backend added : {}", message.message),
              Order::RemoveInstance(_) => println!("backend removed : {} ", message.message),
              Order::AddCertificate(_) => println!("certificate added: {}", message.message),
              Order::RemoveCertificate(_) => println!("certificate removed: {}", message.message),
              Order::AddHttpFront(_) => println!("front added: {}", message.message),
              Order::RemoveHttpFront(_) => println!("front removed: {}", message.message),
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
  match serde_json::to_string(&input) {
    Ok(to_print) => println!("{}", to_print),
    Err(e) => {
      eprintln!("Error while parsing response to JSON: {:?}", e);
      exit(1);
    }
  };
}
