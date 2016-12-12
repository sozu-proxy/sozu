use serde;
use serde_json;
use openssl::ssl;
use mio::channel;
use std::io;
use std::net::{IpAddr,SocketAddr};
use std::default::Default;
use std::convert::From;
use std::sync::mpsc;

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct HttpFront {
    pub app_id:     String,
    pub hostname:   String,
    pub path_begin: String,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct TlsFront {
    pub app_id:            String,
    pub hostname:          String,
    pub path_begin:        String,
    pub certificate:       String,
    pub certificate_chain: Vec<String>,
    pub key:               String,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct TcpFront {
    pub app_id:     String,
    pub ip_address: String,
    pub port:       u16
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct Instance {
    pub app_id:     String,
    pub ip_address: String,
    pub port:       u16
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct HttpProxyConfiguration {
    pub front:           SocketAddr,
    pub front_timeout:   u64,
    pub back_timeout:    u64,
    pub max_connections: usize,
    pub buffer_size:     usize,
    pub public_address:  Option<IpAddr>,
    pub answer_404:      String,
    pub answer_503:      String,
}

impl Default for HttpProxyConfiguration {
  fn default() -> HttpProxyConfiguration {
    HttpProxyConfiguration {
      front:           "127.0.0.1:8080".parse().unwrap(),
      front_timeout:   5000,
      back_timeout:    5000,
      max_connections: 1000,
      buffer_size:     12000,
      public_address:  None,
      answer_404:      String::from("HTTP/1.1 404 Not Found\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"),
      answer_503:      String::from("HTTP/1.1 503 your application is in deployment\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"),
    }
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct TlsProxyConfiguration {
    pub front:           SocketAddr,
    pub front_timeout:   u64,
    pub back_timeout:    u64,
    pub max_connections: usize,
    pub buffer_size:     usize,
    pub public_address:  Option<IpAddr>,
    pub answer_404:      String,
    pub answer_503:      String,
    pub options:         i64,
    pub cipher_list:     String,
}

impl Default for TlsProxyConfiguration {
  fn default() -> TlsProxyConfiguration {
    TlsProxyConfiguration {
      front:           "127.0.0.1:8443".parse().unwrap(),
      front_timeout:   5000,
      back_timeout:    5000,
      max_connections: 1000,
      buffer_size:     12000,
      public_address:  None,
      answer_404:      String::from("HTTP/1.1 404 Not Found\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"),
      answer_503:      String::from("HTTP/1.1 503 your application is in deployment\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"),
      cipher_list:     String::from("ECDHE-ECDSA-CHACHA20-POLY1305:ECDHE-RSA-CHACHA20-POLY1305"),
      options:         (ssl::SSL_OP_CIPHER_SERVER_PREFERENCE | ssl::SSL_OP_NO_COMPRESSION |
                         ssl::SSL_OP_NO_TICKET | ssl::SSL_OP_NO_SSLV2 |
                         ssl::SSL_OP_NO_SSLV3 | ssl::SSL_OP_NO_TLSV1).bits(),
    }
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash)]
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
    TlsProxy(TlsProxyConfiguration),
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
      Command::HttpProxy(_)       => vec![Topic::HttpProxyConfig],
      Command::TlsProxy(_)        => vec![Topic::TlsProxyConfig],
    }
  }
}

enum CommandField {
  Type,
  Data,
}


impl serde::Deserialize for CommandField {
  fn deserialize<D>(deserializer: &mut D) -> Result<CommandField, D::Error>
        where D: serde::de::Deserializer {
    struct CommandFieldVisitor;
    impl serde::de::Visitor for CommandFieldVisitor {
      type Value = CommandField;

      fn visit_str<E>(&mut self, value: &str) -> Result<CommandField, E>
        where E: serde::de::Error {
        match value {
          "type" => Ok(CommandField::Type),
          "data" => Ok(CommandField::Data),
          _ => Err(serde::de::Error::custom("expected type or data")),
        }
      }
    }

    deserializer.deserialize(CommandFieldVisitor)
  }
}

struct CommandVisitor;
impl serde::de::Visitor for CommandVisitor {
  type Value = Command;

  fn visit_map<V>(&mut self, mut visitor: V) -> Result<Command, V::Error>
        where V: serde::de::MapVisitor {
    let mut command_type:Option<String>    = None;
    let mut data:Option<serde_json::Value> = None;

    loop {
      match try!(visitor.visit_key()) {
        Some(CommandField::Type) => { command_type = Some(try!(visitor.visit_value())); }
        Some(CommandField::Data) => { data = Some(try!(visitor.visit_value())); }
        None => { break; }
      }
    }

    //println!("decoded type = {:?}, value= {:?}", command_type, data);
    let command_type = match command_type {
      Some(command) => command,
      None => try!(visitor.missing_field("type")),
    };
    let data = match data {
      Some(data) => data,
      None => try!(visitor.missing_field("data")),
    };

    try!(visitor.end());

    if &command_type == "ADD_HTTP_FRONT" {
      let res = serde_json::from_value(data).or(Err(serde::de::Error::custom("add_http_front")));
      //println!("ADD_HTTP_FRONT => {:?}", res);
      let acl = try!(res);
      Ok(Command::AddHttpFront(acl))
    } else if &command_type == "REMOVE_HTTP_FRONT" {
      let acl = try!(serde_json::from_value(data).or(Err(serde::de::Error::custom("remove_http_front"))));
      Ok(Command::RemoveHttpFront(acl))
    } else if &command_type == "ADD_TLS_FRONT" {
      let acl = try!(serde_json::from_value(data).or(Err(serde::de::Error::custom("add_tls_front"))));
      Ok(Command::AddTlsFront(acl))
    } else if &command_type == "REMOVE_TLS_FRONT" {
      let acl = try!(serde_json::from_value(data).or(Err(serde::de::Error::custom("remove_tls_front"))));
      Ok(Command::RemoveTlsFront(acl))
    } else if &command_type == "ADD_TCP_FRONT" {
      let acl = try!(serde_json::from_value(data).or(Err(serde::de::Error::custom("add_tcp_front"))));
      Ok(Command::AddTcpFront(acl))
    } else if &command_type == "REMOVE_TCP_FRONT" {
      let acl = try!(serde_json::from_value(data).or(Err(serde::de::Error::custom("remove_tcp_front"))));
      Ok(Command::RemoveTcpFront(acl))
    } else if &command_type == "ADD_INSTANCE" {
      let instance = try!(serde_json::from_value(data).or(Err(serde::de::Error::custom("add_instance"))));
      Ok(Command::AddInstance(instance))
    } else if &command_type == "REMOVE_INSTANCE" {
      let instance = try!(serde_json::from_value(data).or(Err(serde::de::Error::custom("remove_instance"))));
      Ok(Command::RemoveInstance(instance))
    } else if &command_type == "CONFIGURE_HTTP_PROXY" {
      let conf = try!(serde_json::from_value(data).or(Err(serde::de::Error::custom("configure_http_proxy"))));
      Ok(Command::HttpProxy(conf))
    } else {
      Err(serde::de::Error::custom("unrecognized command"))
    }
  }
}

impl serde::Deserialize for Command {
  fn deserialize<D>(deserializer: &mut D) -> Result<Command, D::Error>
        where D: serde::de::Deserializer {
    static FIELDS: &'static [&'static str] = &["type", "data"];
    deserializer.deserialize_struct("Command", FIELDS, CommandVisitor)
  }
}

impl serde::Serialize for Command {
  fn serialize<S>(&self, serializer: &mut S) -> Result<(), S::Error>
      where S: serde::Serializer,
  {
    let mut state = try!(serializer.serialize_map(Some(2)));

    match self {
      &Command::AddHttpFront(ref front) => {
        try!(serializer.serialize_map_key(&mut state, "type"));
        try!(serializer.serialize_map_value(&mut state, "ADD_HTTP_FRONT"));
        try!(serializer.serialize_map_key(&mut state, "data"));
        try!(serializer.serialize_map_value(&mut state, front));
      },
      &Command::RemoveHttpFront(ref front) => {
        try!(serializer.serialize_map_key(&mut state, "type"));
        try!(serializer.serialize_map_value(&mut state, "REMOVE_HTTP_FRONT"));
        try!(serializer.serialize_map_key(&mut state, "data"));
        try!(serializer.serialize_map_value(&mut state, front));
      },
      &Command::AddTlsFront(ref front) => {
        try!(serializer.serialize_map_key(&mut state, "type"));
        try!(serializer.serialize_map_value(&mut state, "ADD_TLS_FRONT"));
        try!(serializer.serialize_map_key(&mut state, "data"));
        try!(serializer.serialize_map_value(&mut state, front));
      },
      &Command::RemoveTlsFront(ref front) => {
        try!(serializer.serialize_map_key(&mut state, "type"));
        try!(serializer.serialize_map_value(&mut state, "REMOVE_TLS_FRONT"));
        try!(serializer.serialize_map_key(&mut state, "data"));
        try!(serializer.serialize_map_value(&mut state, front));
      },
      &Command::AddTcpFront(ref front) => {
        try!(serializer.serialize_map_key(&mut state, "type"));
        try!(serializer.serialize_map_value(&mut state, "ADD_TCP_FRONT"));
        try!(serializer.serialize_map_key(&mut state, "data"));
        try!(serializer.serialize_map_value(&mut state, front));
      },
      &Command::RemoveTcpFront(ref front) => {
        try!(serializer.serialize_map_key(&mut state, "type"));
        try!(serializer.serialize_map_value(&mut state, "REMOVE_TCP_FRONT"));
        try!(serializer.serialize_map_key(&mut state, "data"));
        try!(serializer.serialize_map_value(&mut state, front));
      },
      &Command::AddInstance(ref instance) => {
        try!(serializer.serialize_map_key(&mut state, "type"));
        try!(serializer.serialize_map_value(&mut state, "ADD_INSTANCE"));
        try!(serializer.serialize_map_key(&mut state, "data"));
        try!(serializer.serialize_map_value(&mut state, instance));
      },
      &Command::RemoveInstance(ref instance) => {
        try!(serializer.serialize_map_key(&mut state, "type"));
        try!(serializer.serialize_map_value(&mut state, "REMOVE_INSTANCE"));
        try!(serializer.serialize_map_key(&mut state, "data"));
        try!(serializer.serialize_map_value(&mut state, instance));
      },
      &Command::HttpProxy(ref config) => {
        try!(serializer.serialize_map_key(&mut state, "type"));
        try!(serializer.serialize_map_value(&mut state, "CONFIGURE_HTTP_PROXY"));
        try!(serializer.serialize_map_key(&mut state, "data"));
        try!(serializer.serialize_map_value(&mut state, config));
      },
      &Command::TlsProxy(ref config) => {
        try!(serializer.serialize_map_key(&mut state, "type"));
        try!(serializer.serialize_map_value(&mut state, "CONFIGURE_HTTP_PROXY"));
        try!(serializer.serialize_map_key(&mut state, "data"));
        try!(serializer.serialize_map_value(&mut state, config));
      }
    }

    serializer.serialize_map_end(state)
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash)]
pub enum Topic {
    HttpProxyConfig,
    TlsProxyConfig,
    TcpProxyConfig
}

pub trait Sender<T> {
  fn send_message(&self, t:T) -> Result<(), io::Error>;
}

pub trait Receiver<T> {
  fn recv_message(&self) -> Result<T, io::Error>;
}

impl<T> Sender<T> for mpsc::Sender<T> {
  fn send_message(&self, t:T) -> Result<(), io::Error> {
    match self.send(t) {
      Ok(()) => Ok(()),
      Err(mpsc::SendError(_)) => Err(io::Error::new(io::ErrorKind::BrokenPipe, "receiver disconnected"))
    }
  }
}

impl<T> Sender<T> for channel::Sender<T> {
  fn send_message(&self, t:T) -> Result<(), io::Error> {
    match self.send(t) {
      Ok(()) => Ok(()),
      Err(channel::SendError::Io(e)) => Err(e),
      Err(channel::SendError::Disconnected(_)) => Err(io::Error::new(io::ErrorKind::BrokenPipe, "receiver disconnected"))
    }
  }
}

impl<T> Receiver<T> for channel::Receiver<T> {
  fn recv_message(&self) -> Result<T, io::Error> {
    match self.try_recv() {
      Ok(t) => Ok(t),
      Err(mpsc::TryRecvError::Empty) => Err(io::Error::new(io::ErrorKind::WouldBlock, "no data")),
      Err(mpsc::TryRecvError::Disconnected) => Err(io::Error::new(io::ErrorKind::BrokenPipe, "receiver disconnected"))
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json;

  #[test]
  fn add_acl_test() {
    let raw_json = r#"{"type": "ADD_HTTP_FRONT", "data": {"app_id": "xxx", "hostname": "yyy", "path_begin": "xxx", "port": 4242}}"#;
    let command: Command = serde_json::from_str(raw_json).unwrap();
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
    let command: Command = serde_json::from_str(raw_json).unwrap();
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
    let command: Command = serde_json::from_str(raw_json).unwrap();
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
    let command: Command = serde_json::from_str(raw_json).unwrap();
    println!("{:?}", command);
    assert!(command == Command::RemoveInstance(Instance{
      app_id: String::from("xxx"),
      ip_address: String::from("yyy"),
      port: 8080
    }));
  }

  #[test]
  fn http_front_crash_test() {
    let raw_json = r#"{"type": "ADD_HTTP_FRONT", "data": {"app_id": "aa", "hostname": "cltdl.fr", "path_begin": ""}}"#;
    let command: Command = serde_json::from_str(raw_json).unwrap();
    println!("{:?}", command);
    assert!(command == Command::AddHttpFront(HttpFront{
      app_id: String::from("aa"),
      hostname: String::from("cltdl.fr"),
      path_begin: String::from(""),
    }));
  }

  #[test]
  fn http_front_crash_test2() {
    let raw_json = r#"{"app_id": "aa", "hostname": "cltdl.fr", "path_begin": ""}"#;
    let front: HttpFront = serde_json::from_str(raw_json).unwrap();
    println!("{:?}",front);
    assert!(front == HttpFront{
      app_id: String::from("aa"),
      hostname: String::from("cltdl.fr"),
      path_begin: String::from(""),
    });
  }
}

