use std::collections::HashMap;
use std::io::{self,Error,ErrorKind,Read};
use std::fs::File;
use command::ListenerType;
use toml;

#[derive(Debug,Clone,PartialEq,Eq,Hash, RustcDecodable, RustcEncodable)]
pub struct ListenerConfig {
  pub listener_type:   ListenerType,
  pub address:         String,
  pub port:            u16,
  pub max_connections: usize,
  pub buffer_size:     usize,
  pub answer_404:      Option<String>,
  pub answer_503:      Option<String>,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, RustcDecodable, RustcEncodable)]
pub struct MetricsConfig {
  pub address: String,
  pub port:    u16,
}

#[derive(Debug,Clone,PartialEq,Eq, RustcDecodable, RustcEncodable)]
pub struct Config {
  pub command_socket: String,
  pub command_buffer_size: Option<usize>,
  pub max_command_buffer_size: Option<usize>,
  pub saved_state:    Option<String>,
  pub metrics:        MetricsConfig,
  pub listeners:      HashMap<String, ListenerConfig>,
  pub log_level:      Option<String>,
}

impl Config {
  pub fn load_from_path(path: &str) -> io::Result<Config> {
    let mut f = try!(File::open(path));
    let mut data = String::new();
    try!(f.read_to_string(&mut data));

    let mut parser = toml::Parser::new(&data[..]);
    if let Some(config) = parser.parse().and_then(|t| toml::decode(toml::Value::Table(t))) {
      Ok(config)
    } else {
      Err(Error::new(ErrorKind::InvalidData, format!("could not parse the configuration file: {:?}", parser.errors)))
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
      buffer_size: 12000,
      answer_404: Some(String::from("404.html")),
      answer_503: None,
    });
    map.insert(String::from("TLS"), ListenerConfig {
      listener_type: ListenerType::HTTPS,
      address: String::from("127.0.0.1"),
      port: 8080,
      max_connections: 500,
      buffer_size: 12000,
      answer_404: Some(String::from("404.html")),
      answer_503: None,
    });
    let config = Config {
      command_socket: String::from("./command_folder/sock"),
      saved_state: None,
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
