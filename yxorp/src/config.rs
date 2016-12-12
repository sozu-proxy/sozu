//! parsing data from the configuration file
use std::collections::HashMap;
use std::io::{self,Error,ErrorKind,Read};
use std::fs::File;
use std::str::FromStr;
use command::data::ListenerType;
use toml;

use sozu::messages::{HttpProxyConfiguration,TlsProxyConfiguration};

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

    let public_address = self.public_address.as_ref().and_then(|addr| FromStr::from_str(&addr).ok());
    //FIXME: error message when we cannot parse the address
    address.parse().ok().map(|addr| {
      let mut configuration = HttpProxyConfiguration {
        front: addr,
        public_address: public_address,
        max_connections: self.max_connections,
        buffer_size: self.buffer_size,
        ..Default::default()
      };

      //FIXME: error messages if file not found?
      let mut answer_404 = String::new();
      if self.answer_404.as_ref().and_then(|path| File::open(path).ok())
        .and_then(|mut file| file.read_to_string(&mut answer_404).ok()).is_some() {
        configuration.answer_404 = answer_404;
      }
      let mut answer_503 = String::new();
      if self.answer_503.as_ref().and_then(|path| File::open(path).ok())
        .and_then(|mut file| file.read_to_string(&mut answer_503).ok()).is_some() {
        configuration.answer_503 = answer_503;
      }
      configuration
    })
  }

  pub fn to_tls(&self) -> Option<TlsProxyConfiguration> {
    let mut address = self.address.clone();
    address.push(':');
    address.push_str(&self.port.to_string());

    let public_address     = self.public_address.as_ref().and_then(|addr| FromStr::from_str(&addr).ok());
    let cipher_list:String = self.cipher_list.clone().unwrap_or(String::from(""));
    //FIXME: error message when we cannot parse the address
    address.parse().ok().map(|addr| {
      let mut configuration = TlsProxyConfiguration {
        front: addr,
        public_address: public_address,
        max_connections: self.max_connections,
        buffer_size: self.buffer_size,
        ..Default::default()
      };
      //FIXME: error messages if file not found?
      let mut answer_404 = String::new();
      if self.answer_404.as_ref().and_then(|path| File::open(path).ok())
        .and_then(|mut file| file.read_to_string(&mut answer_404).ok()).is_some() {
        configuration.answer_404 = answer_404;
      }
      let mut answer_503 = String::new();
      if self.answer_503.as_ref().and_then(|path| File::open(path).ok())
        .and_then(|mut file| file.read_to_string(&mut answer_503).ok()).is_some() {
        configuration.answer_503 = answer_503;
      }
      if let Some(cipher_list) = self.cipher_list.as_ref() {
        configuration.cipher_list = cipher_list.clone();
      }
      configuration

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
  use std::collections::HashMap;
  use toml::encode_str;
  use command::data::ListenerType;

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
