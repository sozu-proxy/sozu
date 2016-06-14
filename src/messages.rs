
use rustc_serialize::{Decodable, Decoder};

#[derive(Debug,Clone,PartialEq,Eq,Hash, RustcDecodable, RustcEncodable)]
pub struct HttpFront {
    pub app_id: String,
    pub hostname: String,
    pub path_begin: String,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, RustcDecodable, RustcEncodable)]
pub struct TlsFront {
    pub app_id: String,
    pub hostname: String,
    pub path_begin: String,
    pub certificate: Vec<u8>,
    pub key:         Vec<u8>,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, RustcDecodable, RustcEncodable)]
pub struct TcpFront {
    pub app_id: String,
    pub ip_address: String,
    pub port: u16
}


#[derive(Debug,Clone,PartialEq,Eq,Hash, RustcDecodable,RustcEncodable)]
pub struct Instance {
    pub app_id: String,
    pub ip_address: String,
    pub port: u16
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, RustcDecodable, RustcEncodable)]
pub struct HttpProxyConfiguration {
    pub front_timeout: u64,
    pub back_timeout:  u64,
    pub answer_404:    String,
    pub answer_503:    String,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, RustcEncodable)]
pub enum Command {
    AddHttpFront(HttpFront),
    RemoveHttpFront(HttpFront),

    AddTlsFront(TlsFront),
    RemoveTlsFront(TlsFront),

    AddTcpFront(TcpFront),
    RemoveTcpFront(TcpFront),

    AddInstance(Instance),
    RemoveInstance(Instance),

    HttpProxy(HttpProxyConfiguration),
}

impl Command {
  pub fn get_topics(&self) -> Vec<Topic> {
    match *self {
      Command::AddHttpFront(_)    => vec![Topic::HttpProxyConfig                       ],
      Command::RemoveHttpFront(_) => vec![Topic::HttpProxyConfig                       ],
      Command::AddTlsFront(_)     => vec![Topic::TlsProxyConfig                        ],
      Command::RemoveTlsFront(_)  => vec![Topic::TlsProxyConfig                        ],
      Command::AddTcpFront(_)     => vec![Topic::TcpProxyConfig                        ],
      Command::RemoveTcpFront(_)  => vec![Topic::TcpProxyConfig                        ],
      Command::AddInstance(_)     => vec![Topic::HttpProxyConfig, Topic::TlsProxyConfig, Topic::TcpProxyConfig],
      Command::RemoveInstance(_)  => vec![Topic::HttpProxyConfig, Topic::TlsProxyConfig, Topic::TcpProxyConfig],
      Command::HttpProxy(_)       => vec![Topic::HttpProxyConfig, Topic::TlsProxyConfig],
    }
  }
}

impl Decodable for Command {
  fn decode<D: Decoder>(decoder: &mut D) -> Result<Command, D::Error> {
    decoder.read_struct("root", 0, |decoder| {
      let command_type: String = try!(decoder.read_struct_field("type", 0, |decoder| Decodable::decode(decoder)));

      if &command_type == "ADD_HTTP_FRONT" {
        let acl = try!(decoder.read_struct_field("data", 0, |decoder| Decodable::decode(decoder)));
        Ok(Command::AddHttpFront(acl))
      } else if &command_type == "REMOVE_HTTP_FRONT" {
        let acl = try!(decoder.read_struct_field("data", 0, |decoder| Decodable::decode(decoder)));
        Ok(Command::RemoveHttpFront(acl))
      } else if &command_type == "ADD_TLS_FRONT" {
        let acl = try!(decoder.read_struct_field("data", 0, |decoder| Decodable::decode(decoder)));
        Ok(Command::AddTlsFront(acl))
      } else if &command_type == "REMOVE_TLS_FRONT" {
        let acl = try!(decoder.read_struct_field("data", 0, |decoder| Decodable::decode(decoder)));
        Ok(Command::RemoveTlsFront(acl))
      } else if &command_type == "ADD_TCP_FRONT" {
        let acl = try!(decoder.read_struct_field("data", 0, |decoder| Decodable::decode(decoder)));
        Ok(Command::AddTcpFront(acl))
      } else if &command_type == "REMOVE_TCP_FRONT" {
        let acl = try!(decoder.read_struct_field("data", 0, |decoder| Decodable::decode(decoder)));
        Ok(Command::RemoveTcpFront(acl))
      } else if &command_type == "ADD_INSTANCE" {
        let instance = try!(decoder.read_struct_field("data", 0, |decoder| Decodable::decode(decoder)));
        Ok(Command::AddInstance(instance))
      } else if &command_type == "REMOVE_INSTANCE" {
        let instance = try!(decoder.read_struct_field("data", 0, |decoder| Decodable::decode(decoder)));
        Ok(Command::RemoveInstance(instance))
      } else if &command_type == "CONFIGURE_HTTP_PROXY" {
        let conf = try!(decoder.read_struct_field("data", 0, |decoder| Decodable::decode(decoder)));
        Ok(Command::HttpProxy(conf))
      } else {
        Err(decoder.error("unrecognized command"))
      }
    })
  }
}


#[derive(Debug,Clone,PartialEq,Eq,Hash)]
pub enum Topic {
    HttpProxyConfig,
    TlsProxyConfig,
    TcpProxyConfig
}

#[cfg(test)]
mod tests {
  use super::*;
  use rustc_serialize::json;

  #[test]
  fn add_acl_test() {
    let raw_json = r#"{"type": "ADD_HTTP_FRONT", "data": {"app_id": "xxx", "hostname": "yyy", "path_begin": "xxx", "port": 4242}}"#;
    let command: Command = json::decode(raw_json).unwrap();
    println!("{:?}", command);
    assert!(command == Command::AddHttpFront(HttpFront{
      app_id: String::from("xxx"),
      hostname: String::from("yyy"),
      path_begin: String::from("xxx"),
    }));
  }

  #[test]
  fn remove_acl_test() {
    let raw_json = r#"{"type": "REMOVE_HTTP_FRONT", "data": {"app_id": "xxx", "hostname": "yyy", "path_begin": "xxx", "port": 4242}}"#;
    let command: Command = json::decode(raw_json).unwrap();
    println!("{:?}", command);
    assert!(command == Command::RemoveHttpFront(HttpFront{
      app_id: String::from("xxx"),
      hostname: String::from("yyy"),
      path_begin: String::from("xxx"),
    }));
  }


  #[test]
  fn add_instance_test() {
    let raw_json = r#"{"type": "ADD_INSTANCE", "data": {"app_id": "xxx", "ip_address": "yyy", "port": 8080}}"#;
    let command: Command = json::decode(raw_json).unwrap();
    println!("{:?}", command);
    assert!(command == Command::AddInstance(Instance{
      app_id: String::from("xxx"),
      ip_address: String::from("yyy"),
      port: 8080
    }));
  }

  #[test]
  fn remove_instance_test() {
    let raw_json = r#"{"type": "REMOVE_INSTANCE", "data": {"app_id": "xxx", "ip_address": "yyy", "port": 8080}}"#;
    let command: Command = json::decode(raw_json).unwrap();
    println!("{:?}", command);
    assert!(command == Command::RemoveInstance(Instance{
      app_id: String::from("xxx"),
      ip_address: String::from("yyy"),
      port: 8080
    }));
  }
}
