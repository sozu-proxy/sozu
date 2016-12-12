use std::collections::HashMap;

use sozu::messages::{Command,HttpFront,TlsFront,Instance};

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct HttpProxyInstance {
  ip_address: String,
  port:       u16,
}

pub type AppId = String;

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct HttpProxyFront {
  app_id:     AppId,
  hostname:   String,
  path_begin: String,
}

#[derive(Debug,Clone,PartialEq,Eq, Serialize, Deserialize)]
pub struct HttpProxy {
  ip_address: String,
  port:       u16,
  fronts:     HashMap<AppId, Vec<HttpProxyFront>>,
  instances:  HashMap<AppId, Vec<HttpProxyInstance>>,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct TlsProxyFront {
  app_id:      AppId,
  hostname:    String,
  path_begin:  String,
  certificate: String,
  key:         String,
  certificate_chain: Vec<String>,
}

#[derive(Debug,Clone,PartialEq,Eq,Serialize, Deserialize)]
pub struct TlsProxy {
  ip_address: String,
  port:       u16,
  fronts:     HashMap<AppId, Vec<TlsProxyFront>>,
  instances:  HashMap<AppId, Vec<HttpProxyInstance>>,
}

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub enum ConfigState {
  Http(HttpProxy),
  Tls(TlsProxy),
  Tcp
}

impl ConfigState {
  pub fn handle_command(&mut self, command: &Command) {
    match *self {
      ConfigState::Http(ref mut state) => state.handle_command(command),
      ConfigState::Tls(ref mut state)  => state.handle_command(command),
      ConfigState::Tcp                 => {},
    }
  }

  pub fn generate_commands(&self) -> Vec<Command> {
    match *self {
      ConfigState::Http(ref state) => state.generate_commands(),
      ConfigState::Tls(ref state)  => state.generate_commands(),
      ConfigState::Tcp             => vec!(),
    }
  }
}

impl HttpProxy {
  pub fn new(ip: String, port: u16) -> HttpProxy {
    HttpProxy {
      ip_address: ip,
      port:       port,
      fronts:     HashMap::new(),
      instances:  HashMap::new(),
    }
  }

  pub fn handle_command(&mut self, command: &Command) {
    match command {
      &Command::AddHttpFront(ref front) => {
        let f = HttpProxyFront {
          app_id:     front.app_id.clone(),
          hostname:   front.hostname.clone(),
          path_begin: front.path_begin.clone(),
        };
        if self.fronts.contains_key(&front.app_id) {
          self.fronts.get_mut(&front.app_id).map(|front| {
            if !front.contains(&f) {
              front.push(f);
            }
          });
        } else {
          self.fronts.insert(front.app_id.clone(), vec!(f));
        }
      },
      &Command::RemoveHttpFront(ref front) => {
        if let Some(front_list) = self.fronts.get_mut(&front.app_id) {
          front_list.retain(|el| el.hostname != front.hostname || el.path_begin != front.path_begin);
        }
      },
      &Command::AddInstance(ref instance)  => {
        let inst = HttpProxyInstance {
          ip_address: instance.ip_address.clone(),
          port:       instance.port,
        };
        if self.instances.contains_key(&instance.app_id) {
          self.instances.get_mut(&instance.app_id).map(|instance| {
            if !instance.contains(&inst) {
              instance.push(inst);
            }
          });
        } else {
          self.instances.insert(instance.app_id.clone(), vec!(inst));
        }
      },
      &Command::RemoveInstance(ref instance) => {
        if let Some(instance_list) = self.instances.get_mut(&instance.app_id) {
          instance_list.retain(|el| el.ip_address != instance.ip_address || el.port != instance.port);
          /*let mut v = Vec::new();
          for el in front.instances.iter() {
            if el.ip_address != instance.ip_address || el.port != instance.port {
              v.push(el.clone());
            }
          }
          front.instances = v;
          */
        }
      }
      _ => {}
    }
  }

  pub fn generate_commands(&self) -> Vec<Command> {
    let mut v = Vec::new();
    for (app_id, front_list) in self.fronts.iter() {
      for front in front_list {
        v.push(Command::AddHttpFront(HttpFront {
          app_id:     app_id.clone(),
          hostname:   front.hostname.clone(),
          path_begin: front.path_begin.clone(),
        }));
      }
    }

    for (app_id,instance_list) in self.instances.iter() {
      for instance in instance_list {
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
      instances:  HashMap::new(),
    }
  }

  pub fn handle_command(&mut self, command: &Command) {
    match command {
      &Command::AddTlsFront(ref front) => {
        let f = TlsProxyFront {
          app_id:      front.app_id.clone(),
          hostname:    front.hostname.clone(),
          path_begin:  front.path_begin.clone(),
          certificate: front.certificate.clone(),
          certificate_chain: front.certificate_chain.clone(),
          key:         front.key.clone(),
        };
        if self.fronts.contains_key(&front.app_id) {
          self.fronts.get_mut(&front.app_id).map(|front| {
            if !front.contains(&f) {
              front.push(f);
            }
          });
        } else {
          self.fronts.insert(front.app_id.clone(), vec!(f));
        }
      },
      &Command::RemoveTlsFront(ref front) => {
        self.fronts.remove(&front.app_id);
      },
      &Command::AddInstance(ref instance)  => {
        let inst = HttpProxyInstance {
          ip_address: instance.ip_address.clone(),
          port:       instance.port,
        };
        if self.instances.contains_key(&instance.app_id) {
          self.instances.get_mut(&instance.app_id).map(|instance| {
            if !instance.contains(&inst) {
              instance.push(inst);
            }
          });
        } else {
          self.instances.insert(instance.app_id.clone(), vec!(inst));
        }
      },
      &Command::RemoveInstance(ref instance) => {
        if let Some(instance_list) = self.instances.get_mut(&instance.app_id) {
          instance_list.retain(|el| el.ip_address != instance.ip_address || el.port != instance.port);
          /*let mut v = Vec::new();
          for el in instance_list.iter() {
            if el.ip_address != instance.ip_address || el.port != instance.port {
              v.push(el.clone());
            }
          }
          front.instances = v;*/
        }
      }
      _ => {}
    }
  }

  pub fn generate_commands(&self) -> Vec<Command> {
    let mut v = Vec::new();
    for (app_id, front_list) in &self.fronts {
      for front in front_list {
        v.push(Command::AddTlsFront(TlsFront {
          app_id:      app_id.clone(),
          hostname:    front.hostname.clone(),
          path_begin:  front.path_begin.clone(),
          certificate: front.certificate.clone(),
          certificate_chain: front.certificate_chain.clone(),
          key:         front.key.clone(),
        }));
      }
    }
    for (app_id, instance_list) in &self.instances {
      for instance in instance_list {
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
  use sozu::messages::{Command,HttpFront,Instance};

  #[test]
  fn serialize() {
    let mut state = HttpProxy::new(String::from("127.0.0.1"), 80);
    state.handle_command(&Command::AddHttpFront(HttpFront { app_id: String::from("app_1"), hostname: String::from("lolcatho.st:8080"), path_begin: String::from("/") }));
    state.handle_command(&Command::AddHttpFront(HttpFront { app_id: String::from("app_2"), hostname: String::from("test.local"), path_begin: String::from("/abc") }));
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
