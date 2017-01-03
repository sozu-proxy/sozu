use std::fs;
use std::str;
use std::cmp::min;
use std::io::Read;
use std::io::Write;
use std::collections::HashSet;
use log;
use serde_json;
use nom::{HexDisplay,IResult,Offset};

use sozu::messages::Command;
use sozu::network::ProxyOrder;
use sozu::network::buffer::Buffer;

use super::{CommandServer,FrontToken,ListenerConfiguration,StoredListener};
use super::data::{ConfigCommand,ConfigMessage};
use super::client::parse;

impl CommandServer {
  pub fn handle_message(&mut self, token: FrontToken, message: &ConfigMessage) {
    let config_command = message.data.clone();
    match config_command {
      ConfigCommand::SaveState(path) => {
        if let Ok(mut f) = fs::File::create(&path) {
          let mut seen = HashSet::new();
          let mut stored_listeners: Vec<StoredListener> = Vec::new();

          for &(ref tag, ref listener) in  self.listeners.values() {
            if !seen.contains(&tag) {
              seen.insert(tag);
              stored_listeners.push( StoredListener::from_listener(&listener) );
            }
          }

          let mut counter = 0usize;
          for listener in stored_listeners {
            for command in listener.state.generate_commands() {
              let message = ConfigMessage {
                id:       format!("SAVE-{}", counter),
                listener: Some(listener.tag.to_string()),
                data:     ConfigCommand::ProxyConfiguration(command)
              };
              f.write_all(&serde_json::to_string(&message).map(|s| s.into_bytes()).unwrap_or(vec!()));
              f.write_all(&b"\n\0"[..]);
              counter += 1;
            }
            f.sync_all();
          }
          // FIXME: should send back a DONE message here
        } else {
          // FIXME: should send back error here
          log!(log::LogLevel::Error, "could not open file: {}", &path);
          self.conns[token].write_message(b"could not open file");
        }
      },
      ConfigCommand::DumpState => {
        let mut seen = HashSet::new();
        let mut stored_listeners: Vec<StoredListener> = Vec::new();

        for &(ref tag, ref listener) in  self.listeners.values() {
          if !seen.contains(&tag) {
            seen.insert(tag);
            stored_listeners.push( StoredListener::from_listener(&listener) );
          }
        }

        let conf = ListenerConfiguration {
          id:        message.id.clone(),
          listeners: stored_listeners,
        };
        let encoded = serde_json::to_string(&conf).map(|s| s.into_bytes()).unwrap_or(vec!());
        self.conns[token].write_message(&encoded);
      },
      ConfigCommand::LoadState(path) => {
        self.load_state(&message.id, &path);
        self.conns[token].write_message(b"loaded the configuration");
      },
      ConfigCommand::ProxyConfiguration(command) => {
        if let Some(ref tag) = message.listener {
          if let &Command::AddTlsFront(ref data) = &command {
            log!(log::LogLevel::Info, "received AddTlsFront(TlsFront {{ app_id: {}, hostname: {}, path_begin: {} }}) with tag {:?}",
            data.app_id, data.hostname, data.path_begin, tag);
          } else {
            log!(log::LogLevel::Info, "received {:?} with tag {:?}", command, tag);
          }

          let mut found = false;
          for &mut (ref listener_tag, ref mut listener) in self.listeners.values_mut() {
            if tag == listener_tag {
              let cl = command.clone();
              self.conns[token].add_message_id(message.id.clone());
              listener.state.handle_command(&cl);
              listener.channel.write_message(ProxyOrder { id: message.id.clone(), command: cl });
              listener.channel.run();
              found = true;
            }
          }

          if !found {
            // FIXME: should send back error here
            log!(log::LogLevel::Error, "no listener found for tag: {}", tag);
          }

        } else {
          // FIXME: should send back error here
          log!(log::LogLevel::Error, "expecting listener tag");
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
                  if let Some(ref tag) = message.listener {
                    if let &Command::AddTlsFront(ref data) = &command {
                      log!(log::LogLevel::Info, "received AddTlsFront(TlsFront {{ app_id: {}, hostname: {}, path_begin: {} }}) with tag {:?}",
                      data.app_id, data.hostname, data.path_begin, tag);
                    } else {
                      log!(log::LogLevel::Info, "received {:?} with tag {:?}", command, tag);
                    }
                    let mut found = false;
                    for &mut (ref listener_tag, ref mut listener) in self.listeners.values_mut() {
                      if tag == listener_tag {
                        let cl = command.clone();
                        listener.state.handle_command(&cl);
                        listener.channel.write_message(ProxyOrder { id: message.id.clone(), command: cl });
                        listener.channel.run();
                        found = true;
                      }
                    }

                    if !found {
                      // FIXME: should send back error here
                      log!(log::LogLevel::Error, "no listener found for tag: {}", tag);
                    }

                  } else {
                    // FIXME: should send back error here
                    log!(log::LogLevel::Error, "expecting listener tag");
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
