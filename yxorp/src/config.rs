use std::collections::HashMap;
use std::io::{self,Error,ErrorKind,Read};
use std::fs::File;
use command::ListenerType;
use toml;

use yxorp::messages::{HttpProxyConfiguration,TlsProxyConfiguration};

#[derive(Debug,Clone,PartialEq,Eq,Hash, RustcDecodable, RustcEncodable)]
pub struct ListenerConfig {
  pub listener_type:   ListenerType,
  pub address:         String,
  pub public_address:  Option<String>,
  pub port:            u16,
  pub max_connections: usize,
  pub buffer_size:     usize,
  pub answer_404:      Option<String>,
  pub answer_503:      Option<String>,
  pub cipher_list:     Option<String>,
  pub worker_count:    Option<u16>,
}

impl ListenerConfig {
  pub fn to_http(&self) -> Option<HttpProxyConfiguration> {
    let mut address = self.address.clone();
    address.push(':');
    address.push_str(&self.port.to_string());

    //FIXME: error message when we cannot parse the address
    address.parse().ok().map(|addr| {
    HttpProxyConfiguration {
      front: addr,
      public_address: self.public_address.clone(),
      max_connections: self.max_connections,
      buffer_size: self.buffer_size,
      //FIXME: handle the default case,
      answer_404: String::from(""),
      answer_503: String::from(""),
      ..Default::default()
    }
    })
  }

  pub fn to_tls(&self) -> Option<TlsProxyConfiguration> {
    let mut address = self.address.clone();
    address.push(':');
    address.push_str(&self.port.to_string());

    let cipher_list:String = self.cipher_list.clone().unwrap_or(String::from(""));
    //FIXME: error message when we cannot parse the address
    address.parse().ok().map(|addr| {
    TlsProxyConfiguration {
      front: addr,
      public_address: self.public_address.clone(),
      max_connections: self.max_connections,
      buffer_size: self.buffer_size,
      //FIXME: handle the default case,
      answer_404: String::from(""),
      answer_503: String::from(""),
      cipher_list: cipher_list,
      ..Default::default()
    }
    })
  }
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
      public_address: None,
      cipher_list: None,
      worker_count: None,
    });
    map.insert(String::from("TLS"), ListenerConfig {
      listener_type: ListenerType::HTTPS,
      address: String::from("127.0.0.1"),
      port: 8080,
      max_connections: 500,
      buffer_size: 12000,
      answer_404: Some(String::from("404.html")),
      answer_503: None,
      public_address: None,
      cipher_list: None,
      worker_count: None,
    });
    let config = Config {
      command_socket: String::from("./command_folder/sock"),
      saved_state: None,
      command_buffer_size: None,
      max_command_buffer_size: None,
      log_level: None,
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
