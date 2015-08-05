use std::vec;

use rustc_serialize::{json, Decodable, Decoder};

#[derive(Debug,Clone,PartialEq,Eq,Hash, RustcDecodable, RustcEncodable)]
pub struct HttpFront {
    pub app_id: String,
    pub hostname: String,
    pub path_begin: String
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, RustcDecodable, RustcEncodable)]
pub struct TcpFront {
    pub app_id: String,
    pub port: u16
}


#[derive(Debug,Clone,PartialEq,Eq,Hash, RustcDecodable,RustcEncodable)]
pub struct Instance {
    pub app_id: String,
    pub ip_address: String,
    pub port: u16
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, RustcEncodable)]
pub enum Command {
    AddHttpFront(HttpFront),
    RemoveHttpFront(HttpFront),

    AddTcpFront(TcpFront),
    RemoveTcpFront(TcpFront),

    AddInstance(Instance),
    RemoveInstance(Instance)
}

impl Command {
  pub fn get_topics(&self) -> Vec<Topic> {
    match self {
      &Command::AddHttpFront(_)    => vec![Topic::HttpProxyConfig],
      &Command::RemoveHttpFront(_) => vec![Topic::HttpProxyConfig],
      &Command::AddTcpFront(_)     => vec![Topic::HttpProxyConfig],
      &Command::RemoveTcpFront(_)  => vec![Topic::HttpProxyConfig],
      &Command::AddInstance(_)     => vec![Topic::HttpProxyConfig, Topic::TcpProxyConfig],
      &Command::RemoveInstance(_)  => vec![Topic::HttpProxyConfig, Topic::TcpProxyConfig]
    }
  }
}

impl Decodable for Command {
  fn decode<D: Decoder>(decoder: &mut D) -> Result<Command, D::Error> {
    decoder.read_struct("root", 0, |decoder| {
      let commandType: String = try!(decoder.read_struct_field("type", 0, |decoder| Decodable::decode(decoder)));

      if &commandType == "ADD_HTTP_FRONT" {
        let acl = try!(decoder.read_struct_field("data", 0, |decoder| Decodable::decode(decoder)));
        Ok(Command::AddHttpFront(acl))
      } else if &commandType == "REMOVE_HTTP_FRONT" {
        let acl = try!(decoder.read_struct_field("data", 0, |decoder| Decodable::decode(decoder)));
        Ok(Command::RemoveHttpFront(acl))
      } else if &commandType == "ADD_TCP_FRONT" {
        let acl = try!(decoder.read_struct_field("data", 0, |decoder| Decodable::decode(decoder)));
        Ok(Command::AddTcpFront(acl))
      } else if &commandType == "REMOVE_TCP_FRONT" {
        let acl = try!(decoder.read_struct_field("data", 0, |decoder| Decodable::decode(decoder)));
        Ok(Command::RemoveTcpFront(acl))
      } else if &commandType == "ADD_INSTANCE" {
        let instance = try!(decoder.read_struct_field("data", 0, |decoder| Decodable::decode(decoder)));
        Ok(Command::AddInstance(instance))
      } else if &commandType == "REMOVE_INSTANCE" {
        let instance = try!(decoder.read_struct_field("data", 0, |decoder| Decodable::decode(decoder)));
        Ok(Command::RemoveInstance(instance))
      } else {
        Err(decoder.error("unrecognized command"))
      }
    })
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash)]
pub enum Topic {
    HttpProxyConfig,
    TcpProxyConfig
}

#[test]
fn add_acl_test() {
  let raw_json = r#"{"type": "ADD_HTTP_FRONT", "data": {"app_id": "xxx", "hostname": "yyy", "path_begin": "xxx"}}"#;
  let command: Command = json::decode(raw_json).unwrap();
  println!("{:?}", command);
  assert!(command == Command::AddHttpFront(HttpFront{
    app_id: String::from("xxx"),
    hostname: String::from("yyy"),
    path_begin: String::from("xxx")
  }));
}

#[test]
fn remove_acl_test() {
  let raw_json = r#"{"type": "REMOVE_HTTP_FRONT", "data": {"app_id": "xxx", "hostname": "yyy", "path_begin": "xxx"}}"#;
  let command: Command = json::decode(raw_json).unwrap();
  println!("{:?}", command);
  assert!(command == Command::RemoveHttpFront(HttpFront{
    app_id: String::from("xxx"),
    hostname: String::from("yyy"),
    path_begin: String::from("xxx")
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
