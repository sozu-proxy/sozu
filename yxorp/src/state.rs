use rustc_serialize::{Encodable, Decodable, Encoder, Decoder};
use std::collections::HashMap;
use toml::encode_str;

use yxorp::messages::{Command,HttpFront,TlsFront,Instance};

#[derive(Debug,Clone,PartialEq,Eq,Hash, RustcDecodable, RustcEncodable)]
pub struct HttpProxyInstance {
  ip_address: String,
  port:       u16,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, RustcDecodable, RustcEncodable)]
pub struct HttpProxyFront {
  app_id:     String,
  hostname:   String,
  path_begin: String,
  port:       u16,
  instances:  Vec<HttpProxyInstance>
}

#[derive(Debug,Clone,PartialEq,Eq,RustcDecodable, RustcEncodable)]
pub struct HttpProxy {
  ip_address: String,
  port:       u16,
  fronts:     HashMap<String, HttpProxyFront>
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, RustcDecodable, RustcEncodable)]
pub struct TlsProxyFront {
  app_id:     String,
  hostname:   String,
  path_begin: String,
  port:       u16,
  cert_path:  String,
  key_path:   String,
  instances:  Vec<HttpProxyInstance>
}

#[derive(Debug,Clone,PartialEq,Eq,RustcDecodable, RustcEncodable)]
pub struct TlsProxy {
  ip_address: String,
  port:       u16,
  fronts:     HashMap<String, TlsProxyFront>
}

#[derive(Debug,Clone,PartialEq,Eq)]
pub enum ConfigState {
  Http(HttpProxy),
  Tls(TlsProxy),
}

impl ConfigState {
  pub fn handle_command(&mut self, command: &Command) {
    match *self {
      ConfigState::Http(ref mut state) => state.handle_command(command),
      ConfigState::Tls(ref mut state)  => state.handle_command(command),
    }
  }

  pub fn generate_commands(&self) -> Vec<Command> {
    match *self {
      ConfigState::Http(ref state) => state.generate_commands(),
      ConfigState::Tls(ref state)  => state.generate_commands(),
    }
  }
}

impl Encodable for ConfigState {
  fn encode<E: Encoder>(&self, e: &mut E) -> Result<(), E::Error> {
    match *self {
      ConfigState::Http(ref state) => state.encode(e),
      ConfigState::Tls(ref state)  => state.encode(e),
    }
  }
}

impl HttpProxy {
  pub fn new(ip: String, port: u16) -> HttpProxy {
    HttpProxy {
      ip_address: ip,
      port:       port,
      fronts:     HashMap::new(),
    }
  }

  pub fn handle_command(&mut self, command: &Command) {
    match command {
      &Command::AddHttpFront(ref front) => {
        let f = HttpProxyFront {
          app_id:     front.app_id.clone(),
          hostname:   front.hostname.clone(),
          path_begin: front.path_begin.clone(),
          port:       front.port,
          instances:  Vec::new()
        };
        self.fronts.insert(front.app_id.clone(), f);
      },
      &Command::RemoveHttpFront(ref front) => {
        self.fronts.remove(&front.app_id);
      },
      &Command::AddInstance(ref instance)  => {
        if let Some(front) = self.fronts.get_mut(&instance.app_id) {
          let inst = HttpProxyInstance {
            ip_address: instance.ip_address.clone(),
            port:       instance.port,
          };
          front.instances.push(inst);
        }
      },
      &Command::RemoveInstance(ref instance) => {
        if let Some(front) = self.fronts.get_mut(&instance.app_id) {
          let mut v = Vec::new();
          for el in front.instances.iter() {
            if el.ip_address != instance.ip_address || el.port != instance.port {
              v.push(el.clone());
            }
          }
          front.instances = v;
        }
      }
      _ => {}
    }
  }

  pub fn generate_commands(&self) -> Vec<Command> {
    let mut v = Vec::new();
    for (app_id, front) in &self.fronts {
      v.push(Command::AddHttpFront(HttpFront {
        app_id:     app_id.clone(),
        hostname:   front.hostname.clone(),
        path_begin: front.path_begin.clone(),
        port:       front.port.clone(),
      }));
      for instance in front.instances.iter() {
        v.push(Command::AddInstance(Instance {
          app_id:     app_id.clone(),
          ip_address: instance.ip_address.clone(),
          port:       instance.port.clone(),
        }));
      }
    }

    v
  }
}

impl TlsProxy {
  pub fn new(ip: String, port: u16) -> TlsProxy {
    TlsProxy {
      ip_address: ip,
      port:       port,
      fronts:     HashMap::new(),
    }
  }

  pub fn handle_command(&mut self, command: &Command) {
    match command {
      &Command::AddTlsFront(ref front) => {
        let f = TlsProxyFront {
          app_id:     front.app_id.clone(),
          hostname:   front.hostname.clone(),
          path_begin: front.path_begin.clone(),
          port:       front.port,
          cert_path:  front.cert_path.clone(),
          key_path:   front.key_path.clone(),
          instances:  Vec::new()
        };
        self.fronts.insert(front.app_id.clone(), f);
      },
      &Command::RemoveTlsFront(ref front) => {
        self.fronts.remove(&front.app_id);
      },
      &Command::AddInstance(ref instance)  => {
        if let Some(front) = self.fronts.get_mut(&instance.app_id) {
          let inst = HttpProxyInstance {
            ip_address: instance.ip_address.clone(),
            port:       instance.port,
          };
          front.instances.push(inst);
        }
      },
      &Command::RemoveInstance(ref instance) => {
        if let Some(front) = self.fronts.get_mut(&instance.app_id) {
          let mut v = Vec::new();
          for el in front.instances.iter() {
            if el.ip_address != instance.ip_address || el.port != instance.port {
              v.push(el.clone());
            }
          }
          front.instances = v;
        }
      }
      _ => {}
    }
  }

  pub fn generate_commands(&self) -> Vec<Command> {
    let mut v = Vec::new();
    for (app_id, front) in &self.fronts {
      v.push(Command::AddTlsFront(TlsFront {
        app_id:     app_id.clone(),
        hostname:   front.hostname.clone(),
        path_begin: front.path_begin.clone(),
        port:       front.port.clone(),
        cert_path:  front.cert_path.clone(),
        key_path:   front.key_path.clone(),
      }));
      for instance in front.instances.iter() {
        v.push(Command::AddInstance(Instance {
          app_id:     app_id.clone(),
          ip_address: instance.ip_address.clone(),
          port:       instance.port.clone(),
        }));
      }
    }

    v
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use yxorp::messages::{Command,HttpFront,Instance};
  use rustc_serialize::Encodable;
  use toml::decode_str;

  #[test]
  fn serialize() {
    let mut state = HttpProxy::new(String::from("127.0.0.1"), 80);
    state.handle_command(&Command::AddHttpFront(HttpFront { app_id: String::from("app_1"), hostname: String::from("lolcatho.st:8080"), path_begin: String::from("/"), port: 8080 }));
    state.handle_command(&Command::AddHttpFront(HttpFront { app_id: String::from("app_2"), hostname: String::from("test.local"), path_begin: String::from("/abc"), port: 80 }));
    state.handle_command(&Command::AddInstance(Instance { app_id: String::from("app_1"), ip_address: String::from("127.0.0.1"), port: 1026 }));
    state.handle_command(&Command::AddInstance(Instance { app_id: String::from("app_1"), ip_address: String::from("127.0.0.2"), port: 1027 }));
    state.handle_command(&Command::AddInstance(Instance { app_id: String::from("app_2"), ip_address: String::from("192.167.1.2"), port: 1026 }));
    state.handle_command(&Command::AddInstance(Instance { app_id: String::from("app_1"), ip_address: String::from("192.168.1.3"), port: 1027 }));
    state.handle_command(&Command::RemoveInstance(Instance { app_id: String::from("app_1"), ip_address: String::from("192.168.1.3"), port: 1027 }));

    /*
    let encoded = state.encode();
    println!("serialized:\n{}", encoded);

    let new_state: Option<HttpProxy> = decode_str(&encoded);
    println!("deserialized:\n{:?}", new_state);
    assert_eq!(new_state, Some(state));
    */
    //assert!(false);
  }
}
