use std::collections::HashMap;
use std::io::{self,Error,ErrorKind,Read};
use std::fs::File;
use command::ListenerType;
use toml::decode_str;

#[derive(Debug,Clone,PartialEq,Eq,Hash, RustcDecodable, RustcEncodable)]
pub struct ListenerConfig {
  pub listener_type:   ListenerType,
  pub address:         String,
  pub port:            u16,
  pub max_connections: usize,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, RustcDecodable, RustcEncodable)]
pub struct MetricsConfig {
  pub address: String,
  pub port:    u16,
}

#[derive(Debug,Clone,PartialEq,Eq, RustcDecodable, RustcEncodable)]
pub struct Config {
  pub command_socket: String,
  pub metrics:        MetricsConfig,
  pub listeners:      HashMap<String, ListenerConfig>,
}

impl Config {
  pub fn load_from_path(path: &str) -> io::Result<Config> {
    let mut f = try!(File::open(path));
    let mut data = String::new();
    try!(f.read_to_string(&mut data));

    if let Some(config) = decode_str(data.as_str()) {
      Ok(config)
    } else {
      Err(Error::new(ErrorKind::InvalidData, "could not parse the configuration file"))
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use rustc_serialize::Encodable;
  use std::collections::HashMap;
  use toml::{encode_str,decode_str};
  use command::ListenerType;

  #[test]
  fn serialize() {
    let mut map = HashMap::new();
    map.insert(String::from("HTTP"), ListenerConfig {
      listener_type: ListenerType::HTTP,
      address: String::from("127.0.0.1"),
      port: 8080,
      max_connections: 500,
    });
    map.insert(String::from("TLS"), ListenerConfig {
      listener_type: ListenerType::HTTPS,
      address: String::from("127.0.0.1"),
      port: 8080,
      max_connections: 500,
    });
    let config = Config {
      command_socket: String::from("./command_folder/sock"),
      metrics: MetricsConfig {
        address: String::from("192.168.59.103"),
        port:    8125,
      },
      listeners: map
    };

    let encoded = encode_str(&config);
    println!("conf:\n{}", encoded);
  }
}
