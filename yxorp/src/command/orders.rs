use mio::*;
use mio::deprecated::unix::*;
use mio::timer::{Timer,Timeout};
use slab::Slab;
use std::path::PathBuf;
use std::io::{self,BufRead,BufReader,Read,Write,ErrorKind};
use std::str::from_utf8;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::collections::HashMap;
use std::sync::mpsc;
use std::cmp::min;
use log;
use nom::{IResult,HexDisplay};
use serde;
use serde_json;
use serde_json::from_str;
use rustc_serialize::{Decodable,Decoder,Encodable,Encoder};
use std::time::Duration;

use yxorp::network::{ProxyOrder,ServerMessage};
use yxorp::network::buffer::Buffer;
use yxorp::messages::Command;

use state::{HttpProxy,TlsProxy,ConfigState};
use super::{CommandServer,FrontToken,Listener,ListenerConfiguration};
use super::data::{ConfigCommand,ConfigMessage};

impl CommandServer {
  pub fn handle_message(&mut self, token: FrontToken, message: &ConfigMessage) {
    let config_command = message.data.clone();
    match config_command {
      ConfigCommand::SaveState(path) => {
        if let Ok(mut f) = fs::File::create(&path) {
          for listener in self.listeners.values() {
            if let Ok(()) = f.write_all(&serde_json::to_string(&listener).map(|s| s.into_bytes()).unwrap_or(vec!())) {
              f.write(&b"\n"[..]);
              f.sync_all();
            }
          }
          // FIXME: should send back a DONE message here
        } else {
          // FIXME: should send back error here
          log!(log::LogLevel::Error, "could not open file: {}", &path);
        }
      },
      ConfigCommand::DumpState => {
        //FIXME:
        let v: &Vec<Listener> = self.listeners.values().next().unwrap();
        let conf = ListenerConfiguration {
          id: message.id.clone(),
          listeners: v,
        };
        let encoded = serde_json::to_string(&conf).map(|s| s.into_bytes()).unwrap_or(vec!());
        if self.conns[token].back_buf.grow(min(encoded.len() + 10, self.max_buffer_size)) {
          log!(log::LogLevel::Info, "write buffer was not large enough, growing to {} bytes", encoded.len());
        }
        self.conns[token].back_buf.write(&encoded);
        self.conns[token].back_buf.write(&b"\0"[..]);
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
