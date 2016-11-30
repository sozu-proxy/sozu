use std::io::Write;
use std::fs;
use std::str;
use std::cmp::min;
use log;
use serde_json;
use yxorp::messages::Command;
use yxorp::network::ProxyOrder;

use super::{CommandServer,FrontToken,Listener,ListenerConfiguration,StoredListener};
use super::data::{ConfigCommand,ConfigMessage};

impl CommandServer {
  pub fn handle_message(&mut self, token: FrontToken, message: &ConfigMessage) {
    let config_command = message.data.clone();
    match config_command {
      ConfigCommand::SaveState(path) => {
        if let Ok(mut f) = fs::File::create(&path) {
          let stored_listeners: Vec<StoredListener> = self.listeners.values()
            .map(|listener_list| StoredListener::from_listener(listener_list.first().unwrap())).collect();
          if let Ok(()) = f.write_all(&serde_json::to_string(&stored_listeners).map(|s| s.into_bytes()).unwrap_or(vec!())) {
            f.sync_all();
            //FIXME: define proper answer format
            self.conns[token].back_buf.write(b"OK\0");
          } else {
            self.conns[token].back_buf.write(b"could not parse configuration\0");
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
        let mut saved_state = String::new();
        //if fs::File::open(path).and_then(|file| file.read_to_string(&mut saved_state)).is_ok() {
        match fs::File::open(&path) {
          Err(e)   => error!("cannot open file at path '{}': {:?}", path, e),
          Ok(file) => {
            let conf_res: serde_json::error::Result<Vec<StoredListener>> = serde_json::from_reader(file);
            match conf_res {
              Err(e)   => error!("error loading configuration from file: {:?}", e),
              Ok(conf) => {
                //info!("loaded the configuration: {:?}", conf);
                self.conns[token].back_buf.write(b"loaded the configuration\0");
                for stored_listener in conf {
                  let commands = stored_listener.state.generate_commands();
                  //info!("saved listener '{}' (type {:?}) commands: {:?}", stored_listener.tag,
                  stored_listener.listener_type, commands);
                  if let Some(ref mut listener_vec) = self.listeners.get_mut (&stored_listener.tag) {
                    for listener in listener_vec.iter_mut() {
                      for command in &commands {
                        self.conns[token].add_message_id(message.id.clone());
                        listener.state.handle_command(&command);
                        listener.sender.send(ProxyOrder::Command(message.id.clone(), command.clone()));
                      }
                    }
                  }
                }
              },
            }
          }
        }
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
}
