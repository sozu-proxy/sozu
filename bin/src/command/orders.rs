use std::fs;
use std::str;
use std::io::Read;
use std::io::Write;
use std::collections::HashSet;
use log;
use serde_json;
use nom::{HexDisplay,IResult,Offset};

use sozu::messages::Command;
use sozu::network::ProxyOrder;
use sozu::network::buffer::Buffer;

use super::{CommandServer,FrontToken,ProxyConfiguration,StoredProxy};
use super::data::{ConfigCommand,ConfigMessage,ConfigMessageAnswer,ConfigMessageStatus};
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
            for command in proxy.state.generate_commands() {
              let message = ConfigMessage {
                id:       format!("SAVE-{}", counter),
                proxy: Some(proxy.tag.to_string()),
                data:     ConfigCommand::ProxyConfiguration(command)
              };
              f.write_all(&serde_json::to_string(&message).map(|s| s.into_bytes()).unwrap_or(vec!()));
              f.write_all(&b"\n\0"[..]);
              counter += 1;
            }
            f.sync_all();
          }
          self.conns[token].write_message(&ConfigMessageAnswer {
            id:      message.id.clone(),
            status:  ConfigMessageStatus::Ok,
            message: format!("saved to {}", path)
          });
        } else {
          log!(log::LogLevel::Error, "could not open file: {}", &path);
          self.conns[token].write_message(&ConfigMessageAnswer {
            id:      message.id.clone(),
            status:  ConfigMessageStatus::Error,
            message: "could not open file".to_string()
          });
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
        self.conns[token].write_message(&ConfigMessageAnswer {
          id:      message.id.clone(),
          status:  ConfigMessageStatus::Ok,
          message: serde_json::to_string(&conf).unwrap_or(String::new())
        });
        //self.conns[token].write_message(&encoded);
      },
      ConfigCommand::LoadState(path) => {
        self.load_state(&message.id, &path);
        self.conns[token].write_message(&ConfigMessageAnswer {
          id:      message.id.clone(),
          status:  ConfigMessageStatus::Ok,
          message: "loaded the configuration".to_string()
        });
      },
      ConfigCommand::ProxyConfiguration(command) => {
        if let Some(ref tag) = message.proxy {
          if let &Command::AddTlsFront(ref data) = &command {
            log!(log::LogLevel::Info, "received AddTlsFront(TlsFront {{ app_id: {}, hostname: {}, path_begin: {} }}) with tag {:?}",
            data.app_id, data.hostname, data.path_begin, tag);
          } else {
            log!(log::LogLevel::Info, "received {:?} with tag {:?}", command, tag);
          }

          let mut found = false;
          for &mut (ref proxy_tag, ref mut proxy) in self.proxies.values_mut() {
            if tag == proxy_tag {
              let cl = command.clone();
              self.conns[token].add_message_id(message.id.clone());
              proxy.state.handle_command(&cl);
              proxy.channel.write_message(&ProxyOrder { id: message.id.clone(), command: cl });
              proxy.channel.run();
              found = true;
            }
          }

          if !found {
            // FIXME: should send back error here
            log!(log::LogLevel::Error, "no proxy found for tag: {}", tag);
          }

        } else {
          // FIXME: should send back error here
          log!(log::LogLevel::Error, "expecting proxy tag");
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
                if let ConfigCommand::ProxyConfiguration(command) = message.data {
                  if let Some(ref tag) = message.proxy {
                    if let &Command::AddTlsFront(ref data) = &command {
                      log!(log::LogLevel::Info, "received AddTlsFront(TlsFront {{ app_id: {}, hostname: {}, path_begin: {} }}) with tag {:?}",
                      data.app_id, data.hostname, data.path_begin, tag);
                    } else {
                      log!(log::LogLevel::Info, "received {:?} with tag {:?}", command, tag);
                    }
                    let mut found = false;
                    for &mut (ref proxy_tag, ref mut proxy) in self.proxies.values_mut() {
                      if tag == proxy_tag {
                        let cl = command.clone();
                        proxy.state.handle_command(&cl);
                        proxy.channel.write_message(&ProxyOrder { id: message.id.clone(), command: cl });
                        proxy.channel.run();
                        found = true;
                      }
                    }

                    if !found {
                      // FIXME: should send back error here
                      log!(log::LogLevel::Error, "no proxy found for tag: {}", tag);
                    }

                  } else {
                    // FIXME: should send back error here
                    log!(log::LogLevel::Error, "expecting proxy tag");
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
}
