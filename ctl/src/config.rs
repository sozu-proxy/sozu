//! parsing data from the configuration file
use std::collections::HashMap;
use std::io::{self,Error,ErrorKind,Read};
use std::fs::File;
use std::str::FromStr;
use serde::Deserialize;
use toml;

use sozu::messages::{HttpProxyConfiguration,TlsProxyConfiguration};


#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct Config {
  pub command_socket: String,
}

impl Config {
  pub fn load_from_path(path: &str) -> io::Result<Config> {
    let mut f = try!(File::open(path));
    let mut data = String::new();
    try!(f.read_to_string(&mut data));

    let mut parser = toml::Parser::new(&data[..]);
    if let Some(table) = parser.parse() {
      match Deserialize::deserialize(&mut toml::Decoder::new(toml::Value::Table(table))) {
        Ok(config) => Ok(config),
        Err(e)     => {
          println!("decoding error: {:?}", e);
          Err(Error::new(
            ErrorKind::InvalidData,
            format!("decoding error: {:?}", e))
          )
        }
      }
    } else {
      println!("error parsing file:");
      let mut offset = 0usize;
      let mut index  = 0usize;
      for line in data.lines() {
        for error in &parser.errors {
          if error.lo >= offset && error.lo <= offset + line.len() {
            println!("line {}: {}", index, error.desc);
          }
        }
        offset += line.len();
        index  += 1;
      }
      Err(Error::new(ErrorKind::InvalidData, format!("could not parse the configuration file: {:?}", parser.errors)))
    }
  }
}

