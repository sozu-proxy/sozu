use std::fs;
use std::str;
use std::io::Read;
use std::io::Write;
use std::collections::HashSet;
use serde_json;
use nom::{HexDisplay,IResult,Offset};

use sozu::messages::Order;
use sozu::network::ProxyOrder;
use sozu::network::buffer::Buffer;
use sozu_command::config::Config;
use sozu_command::data::{AnswerData,ConfigCommand,ConfigMessage,ConfigMessageAnswer,ConfigMessageStatus,PROTOCOL_VERSION,WorkerInfo};

use super::{CommandServer,FrontToken,ProxyConfiguration,StoredProxy};
use super::client::parse;

impl CommandServer {
  pub fn handle_client_message(&mut self, token: FrontToken, message: &ConfigMessage) {
    let config_command = message.data.clone();
    match config_command {
      ConfigCommand::SaveState(path) => {
        if let Ok(mut f) = fs::File::create(&path) {
          let mut seen = HashSet::new();
          let mut stored_proxies: Vec<StoredProxy> = Vec::new();

          for &(ref tag, ref proxy) in  self.proxies.values() {
            if !seen.contains(&tag) {
              seen.insert(tag);
              stored_proxies.push( StoredProxy::from_proxy(&proxy) );
            }
          }

          let mut counter = 0usize;
          for proxy in stored_proxies {
            for command in proxy.state.generate_orders() {
              let message = ConfigMessage::new(
                format!("SAVE-{}", counter),
                ConfigCommand::ProxyConfiguration(command),
                Some(proxy.tag.to_string())
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
        let mut seen = HashSet::new();
        let mut stored_proxies: Vec<StoredProxy> = Vec::new();

        for &(ref tag, ref proxy) in  self.proxies.values() {
          if !seen.contains(&tag) {
            seen.insert(tag);
            stored_proxies.push( StoredProxy::from_proxy(&proxy) );
          }
        }

        let conf = ProxyConfiguration {
          id:        message.id.clone(),
          proxies: stored_proxies,
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
            tag:        tag.clone(),
            id:         proxy.id,
            proxy_type: proxy.proxy_type,
            pid:        proxy.pid,
          }
        }).collect();
        self.conns[token].write_message(&ConfigMessageAnswer::new(
          message.id.clone(),
          ConfigMessageStatus::Ok,
          "".to_string(),
          Some(AnswerData::Workers(workers))
        ));
      },
      ConfigCommand::ProxyConfiguration(order) => {
        if let Some(ref tag) = message.proxy {
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

  pub fn load_static_application_configuration(&mut self, config: &Config) {
    //FIXME: too many loops, this could be cleaner
    for message in config.generate_config_messages() {
      if let ConfigCommand::ProxyConfiguration(order) = message.data {
        if let Some(ref tag) = message.proxy {
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
}
