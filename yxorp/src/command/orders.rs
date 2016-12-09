use std::io::Write;
use std::fs;
use std::str;
use std::cmp::min;
use std::io::Read;
use log;
use serde_json;
use nom::IResult;
use yxorp::messages::Command;
use yxorp::network::ProxyOrder;

use super::{CommandServer,FrontToken,Listener,ListenerConfiguration,StoredListener,parse};
use super::data::{ConfigCommand,ConfigMessage};

impl CommandServer {
  pub fn handle_message(&mut self, token: FrontToken, message: &ConfigMessage) {
    let config_command = message.data.clone();
    match config_command {
      ConfigCommand::SaveState(path) => {
        if let Ok(mut f) = fs::File::create(&path) {
          let stored_listeners: Vec<StoredListener> = self.listeners.values()
            .map(|listener_list| StoredListener::from_listener(listener_list.first().unwrap())).collect();

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
          self.conns[token].back_buf.write(b"could not open file\0");
        }
      },
      ConfigCommand::DumpState => {
        //FIXME:
        //let v: &Vec<Listener> = self.listeners.values().next().unwrap();
        let stored_listeners = self.listeners.values()
          .map(|listener_list| StoredListener::from_listener(listener_list.first().unwrap())).collect();
        let conf = ListenerConfiguration {
          id:        message.id.clone(),
          listeners: stored_listeners,
        };
        let encoded = serde_json::to_string(&conf).map(|s| s.into_bytes()).unwrap_or(vec!());
        if self.conns[token].back_buf.grow(min(encoded.len() + 10, self.max_buffer_size)) {
          log!(log::LogLevel::Info, "write buffer was not large enough, growing to {} bytes", encoded.len());
        }
        info!("dumping state to {:?}:\n{}", token, str::from_utf8(&encoded).unwrap());
        self.conns[token].back_buf.write(&encoded);
        self.conns[token].back_buf.write(&b"\0"[..]);
      },
      ConfigCommand::LoadState(path) => {
        self.load_state(&message.id, &path);
        self.conns[token].back_buf.write(b"loaded the configuration\0");
      },
      ConfigCommand::ProxyConfiguration(command) => {
        if let Some(ref tag) = message.listener {
          if let &Command::AddTlsFront(ref data) = &command {
            log!(log::LogLevel::Info, "received AddTlsFront(TlsFront {{ app_id: {}, hostname: {}, path_begin: {} }}) with tag {:?}",
            data.app_id, data.hostname, data.path_begin, tag);
          } else {
            log!(log::LogLevel::Info, "received {:?} with tag {:?}", command, tag);
          }
          if let Some(ref mut listener_vec) = self.listeners.get_mut (tag) {
            for listener in listener_vec.iter_mut() {
              let cl = command.clone();
              self.conns[token].add_message_id(message.id.clone());
              listener.state.handle_command(&cl);
              listener.sender.send(ProxyOrder::Command(message.id.clone(), cl));
            }
          } else {
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
        let mut data = vec!();
        //FIXME: we should read in streaming here
        file.read_to_end(&mut data);
        match parse(&data) {
          IResult::Done(i, o) => {
            if i.len() > 0 {
              info!("could not parse {} bytes", i.len());
            }

            for message in o {
              if let ConfigCommand::ProxyConfiguration(command) = message.data {
                if let Some(ref tag) = message.listener {
                  if let &Command::AddTlsFront(ref data) = &command {
                    log!(log::LogLevel::Info, "received AddTlsFront(TlsFront {{ app_id: {}, hostname: {}, path_begin: {} }}) with tag {:?}",
                    data.app_id, data.hostname, data.path_begin, tag);
                  } else {
                    log!(log::LogLevel::Info, "received {:?} with tag {:?}", command, tag);
                  }
                  if let Some(ref mut listener_vec) = self.listeners.get_mut(tag) {
                    for listener in listener_vec.iter_mut() {
                      let cl = command.clone();
                      listener.state.handle_command(&cl);
                      listener.sender.send(ProxyOrder::Command(message.id.clone(), cl));
                    }
                  } else {
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
          e => error!("saved state parse error: {:?}", e),
        }
      }
    }
  }
}
