use serde;
use serde_json;
use state::{HttpProxy,TlsProxy,ConfigState};
use sozu::messages::Order;

pub const PROTOCOL_VERSION: u8 = 0;

#[derive(Debug,Clone,Copy,PartialEq,Eq,Hash)]
pub enum ProxyType {
  HTTP,
  HTTPS,
  TCP
}

impl serde::Serialize for ProxyType {
  fn serialize<S>(&self, serializer: &mut S) -> Result<(), S::Error>
      where S: serde::Serializer,
  {
    match *self {
      ProxyType::HTTP  => serializer.serialize_str("HTTP"),
      ProxyType::HTTPS => serializer.serialize_str("HTTPS"),
      ProxyType::TCP   => serializer.serialize_str("TCP"),
    }
  }
}

impl serde::Deserialize for ProxyType {
  fn deserialize<D>(deserializer: &mut D) -> Result<ProxyType, D::Error>
    where D: serde::de::Deserializer
  {
    struct ProxyTypeVisitor;

    impl serde::de::Visitor for ProxyTypeVisitor {
      type Value = ProxyType;

      fn visit_str<E>(&mut self, value: &str) -> Result<ProxyType, E>
        where E: serde::de::Error
        {
          match value {
            "HTTP"  => Ok(ProxyType::HTTP),
            "HTTPS" => Ok(ProxyType::HTTPS),
            "TCP"   => Ok(ProxyType::TCP),
            _ => Err(serde::de::Error::custom("expected HTTP, HTTPS or TCP proxy type")),
          }
        }
    }

    deserializer.deserialize(ProxyTypeVisitor)
  }
}


#[derive(Debug,Clone,PartialEq,Eq)]
pub struct ProxyDeserializer {
  pub tag:   String,
  pub state: ConfigState,
}

enum ProxyDeserializerField {
  Tag,
  Type,
  State,
}

impl serde::Deserialize for ProxyDeserializerField {
  fn deserialize<D>(deserializer: &mut D) -> Result<ProxyDeserializerField, D::Error>
        where D: serde::de::Deserializer {
    struct ProxyDeserializerFieldVisitor;
    impl serde::de::Visitor for ProxyDeserializerFieldVisitor {
      type Value = ProxyDeserializerField;

      fn visit_str<E>(&mut self, value: &str) -> Result<ProxyDeserializerField, E>
        where E: serde::de::Error {
        match value {
          "tag"        => Ok(ProxyDeserializerField::Tag),
          "proxy_type" => Ok(ProxyDeserializerField::Type),
          "state"      => Ok(ProxyDeserializerField::State),
          _ => Err(serde::de::Error::custom("expected tag, proxy_type or state")),
        }
      }
    }

    deserializer.deserialize(ProxyDeserializerFieldVisitor)
  }
}

struct ProxyDeserializerVisitor;
impl serde::de::Visitor for ProxyDeserializerVisitor {
  type Value = ProxyDeserializer;

  fn visit_map<V>(&mut self, mut visitor: V) -> Result<ProxyDeserializer, V::Error>
        where V: serde::de::MapVisitor {
    let mut tag:Option<String>              = None;
    let mut proxy_type:Option<ProxyType>    = None;
    let mut state:Option<serde_json::Value> = None;

    loop {
      match try!(visitor.visit_key()) {
        Some(ProxyDeserializerField::Type)  => { proxy_type = Some(try!(visitor.visit_value())); }
        Some(ProxyDeserializerField::Tag)   => { tag = Some(try!(visitor.visit_value())); }
        Some(ProxyDeserializerField::State) => { state = Some(try!(visitor.visit_value())); }
        None => { break; }
      }
    }

    println!("decoded type = {:?}, value= {:?}", proxy_type, state);
    let proxy_type = match proxy_type {
      Some(proxy) => proxy,
      None => try!(visitor.missing_field("proxy_type")),
    };
    let tag = match tag {
      Some(tag) => tag,
      None => try!(visitor.missing_field("tag")),
    };
    let state = match state {
      Some(state) => state,
      None => try!(visitor.missing_field("state")),
    };

    try!(visitor.end());

    let state = match proxy_type {
      ProxyType::HTTP => {
        let http_proxy: HttpProxy = try!(serde_json::from_value(state).or(Err(serde::de::Error::custom("http_proxy"))));
        ConfigState::Http(http_proxy)
      },
      ProxyType::HTTPS => {
        let tls_proxy: TlsProxy = try!(serde_json::from_value(state).or(Err(serde::de::Error::custom("tls_proxy"))));
        ConfigState::Tls(tls_proxy)
      },
      ProxyType::TCP => {
        ConfigState::Tcp
      }
    };

    Ok(ProxyDeserializer {
      tag: tag,
      state: state,
    })
  }
}

impl serde::Deserialize for ProxyDeserializer {
  fn deserialize<D>(deserializer: &mut D) -> Result<ProxyDeserializer, D::Error>
        where D: serde::de::Deserializer {
    static FIELDS: &'static [&'static str] = &["tag", "proxy_type", "state"];
    deserializer.deserialize_struct("Proxy", FIELDS, ProxyDeserializerVisitor)
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize)]
pub enum ConfigCommand {
  ProxyConfiguration(Order),
  SaveState(String),
  LoadState(String),
  DumpState,
  ListWorkers,
  LaunchWorker(String)
}

#[derive(Debug,Clone,PartialEq,Eq,Hash)]
pub struct ConfigMessage {
  pub id:       String,
  pub version:  u8,
  pub data:     ConfigCommand,
  pub proxy:    Option<String>,
  pub proxy_id: Option<u8>,
}

impl ConfigMessage {
  pub fn new(id: String, data: ConfigCommand, proxy: Option<String>, proxy_id: Option<u8>) -> ConfigMessage {
    ConfigMessage {
      id:       id,
      version:  PROTOCOL_VERSION,
      data:     data,
      proxy:    proxy,
      proxy_id: proxy_id,
    }
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
pub enum ConfigMessageStatus {
  Ok,
  Processing,
  Error
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
pub enum AnswerData {
  Workers(Vec<WorkerInfo>),
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
pub struct ConfigMessageAnswer {
  pub id:      String,
  pub version: u8,
  pub status:  ConfigMessageStatus,
  pub message: String,
  pub data:    Option<AnswerData>,
}

impl ConfigMessageAnswer {
  pub fn new(id: String, status: ConfigMessageStatus, message: String, data: Option<AnswerData>) -> ConfigMessageAnswer {
    ConfigMessageAnswer {
      id:      id,
      version: PROTOCOL_VERSION,
      status:  status,
      message: message,
      data:    data,
    }
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
pub enum RunState {
  Running,
  Stopping,
  Stopped,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
pub struct WorkerInfo {
  pub id:         u8,
  pub pid:        i32,
  pub tag:        String,
  pub proxy_type: ProxyType,
  pub run_state:  RunState,
}

#[derive(Deserialize)]
struct SaveStateData {
  path : String
}

enum ConfigMessageField {
  Id,
  Version,
  Proxy,
  ProxyId,
  Type,
  Data,
}

impl serde::Deserialize for ConfigMessageField {
  fn deserialize<D>(deserializer: &mut D) -> Result<ConfigMessageField, D::Error>
        where D: serde::de::Deserializer {
    struct ConfigMessageFieldVisitor;
    impl serde::de::Visitor for ConfigMessageFieldVisitor {
      type Value = ConfigMessageField;

      fn visit_str<E>(&mut self, value: &str) -> Result<ConfigMessageField, E>
        where E: serde::de::Error {
        match value {
          "id"       => Ok(ConfigMessageField::Id),
          "version"  => Ok(ConfigMessageField::Version),
          "type"     => Ok(ConfigMessageField::Type),
          "proxy"    => Ok(ConfigMessageField::Proxy),
          "proxy_id" => Ok(ConfigMessageField::ProxyId),
          "data"     => Ok(ConfigMessageField::Data),
          e => Err(serde::de::Error::custom(format!("expected id, version, proxy, type or data, got: {}", e))),
        }
      }
    }

    deserializer.deserialize(ConfigMessageFieldVisitor)
  }
}

struct ConfigMessageVisitor;
impl serde::de::Visitor for ConfigMessageVisitor {
  type Value = ConfigMessage;

  fn visit_map<V>(&mut self, mut visitor: V) -> Result<ConfigMessage, V::Error>
        where V: serde::de::MapVisitor {
    let mut id:Option<String>              = None;
    let mut version:Option<u8>             = None;
    let mut proxy: Option<String>          = None;
    let mut proxy_id: Option<u8>           = None;
    let mut config_type:Option<String>     = None;
    let mut data:Option<serde_json::Value> = None;

    loop {
      match try!(visitor.visit_key()) {
        Some(ConfigMessageField::Type)    => { config_type = Some(try!(visitor.visit_value())); }
        Some(ConfigMessageField::Id)      => { id = Some(try!(visitor.visit_value())); }
        Some(ConfigMessageField::Version) => { version = Some(try!(visitor.visit_value())); }
        Some(ConfigMessageField::Proxy)   => { proxy = Some(try!(visitor.visit_value())); }
        Some(ConfigMessageField::ProxyId) => { proxy_id = Some(try!(visitor.visit_value())); }
        Some(ConfigMessageField::Data)    => { data = Some(try!(visitor.visit_value())); }
        None => { break; }
      }
    }

    //println!("decoded type = {:?}, value= {:?}", proxy_type, state);
    let config_type = match config_type {
      Some(config) => config,
      None => try!(visitor.missing_field("type")),
    };
    let id = match id {
      Some(id) => id,
      None => try!(visitor.missing_field("id")),
    };
    let version = match version {
      Some(version) => version,
      None => try!(visitor.missing_field("version")),
    };

    try!(visitor.end());

    let data = if &config_type == "PROXY" {
      let data = match data {
        Some(data) => data,
        None => try!(visitor.missing_field("data")),
      };
      let command = try!(serde_json::from_value(data).or(Err(serde::de::Error::custom("proxy configuration command"))));
      ConfigCommand::ProxyConfiguration(command)
    } else if &config_type == &"SAVE_STATE" {
      let data = match data {
        Some(data) => data,
        None => try!(visitor.missing_field("data")),
      };
      let state: SaveStateData = try!(serde_json::from_value(data).or(Err(serde::de::Error::custom("save state"))));
      ConfigCommand::SaveState(state.path)
    } else if &config_type == &"DUMP_STATE" {
      ConfigCommand::DumpState
    } else if &config_type == &"LOAD_STATE" {
      let data = match data {
        Some(data) => data,
        None => try!(visitor.missing_field("data")),
      };
      let state: SaveStateData = try!(serde_json::from_value(data).or(Err(serde::de::Error::custom("save state"))));
      ConfigCommand::LoadState(state.path)
    } else if &config_type == &"LIST_WORKERS" {
      ConfigCommand::ListWorkers
    } else if &config_type == &"LAUNCH_WORKER" {
      let data = match data {
        Some(data) => data,
        None => try!(visitor.missing_field("data")),
      };
      ConfigCommand::LaunchWorker(try!(serde_json::from_value(data).or(Err(serde::de::Error::custom("launch worker")))))
    } else {
      return Err(serde::de::Error::custom("unrecognized command"));
    };

    Ok(ConfigMessage {
      id:      id,
      version: PROTOCOL_VERSION,
      data:    data,
      proxy:   proxy,
      proxy_id: proxy_id,
    })
  }
}

impl serde::Deserialize for ConfigMessage {
  fn deserialize<D>(deserializer: &mut D) -> Result<ConfigMessage, D::Error>
    where D: serde::de::Deserializer {
    static FIELDS: &'static [&'static str] = &["id", "version", "proxy", "type", "data"];
    deserializer.deserialize_struct("ConfigMessage", FIELDS, ConfigMessageVisitor)
  }
}

#[derive(Serialize)]
struct StatePath {
  path: String
}

impl serde::Serialize for ConfigMessage {
  fn serialize<S>(&self, serializer: &mut S) -> Result<(), S::Error>
      where S: serde::Serializer,
  {
    let mut count = 4;
    if self.proxy.is_some() {
      count += 1;
    }
    if self.proxy_id.is_some() {
      count += 1;
    }
    let mut state = try!(serializer.serialize_map(Some(count)));

    try!(serializer.serialize_map_key(&mut state, "id"));
    try!(serializer.serialize_map_value(&mut state, &self.id));

    try!(serializer.serialize_map_key(&mut state, "version"));
    try!(serializer.serialize_map_value(&mut state, &self.version));

    if self.proxy.is_some() {
      try!(serializer.serialize_map_key(&mut state, "proxy"));
      try!(serializer.serialize_map_value(&mut state, self.proxy.as_ref().unwrap()));
    }

    if self.proxy_id.is_some() {
      try!(serializer.serialize_map_key(&mut state, "proxy_id"));
      try!(serializer.serialize_map_value(&mut state, self.proxy_id.as_ref().unwrap()));
    }

    match self.data {
      ConfigCommand::ProxyConfiguration(ref order) => {
        try!(serializer.serialize_map_key(&mut state, "type"));
        try!(serializer.serialize_map_value(&mut state, "PROXY"));
        try!(serializer.serialize_map_key(&mut state, "data"));
        try!(serializer.serialize_map_value(&mut state, order));
      },
      ConfigCommand::SaveState(ref path) => {
        try!(serializer.serialize_map_key(&mut state, "type"));
        try!(serializer.serialize_map_value(&mut state, "SAVE_STATE"));
        try!(serializer.serialize_map_key(&mut state, "data"));
        try!(serializer.serialize_map_value(&mut state, StatePath { path: path.to_string() }));
      },
      ConfigCommand::LoadState(ref path) => {
        try!(serializer.serialize_map_key(&mut state, "type"));
        try!(serializer.serialize_map_value(&mut state, "LOAD_STATE"));
        try!(serializer.serialize_map_key(&mut state, "data"));
        try!(serializer.serialize_map_value(&mut state, StatePath { path: path.to_string() }));
      },
      ConfigCommand::DumpState => {
        try!(serializer.serialize_map_key(&mut state, "type"));
        try!(serializer.serialize_map_value(&mut state, "DUMP_STATE"));
      },
      ConfigCommand::ListWorkers => {
        try!(serializer.serialize_map_key(&mut state, "type"));
        try!(serializer.serialize_map_value(&mut state, "LIST_WORKERS"));
      },
      ConfigCommand::LaunchWorker(ref tag) => {
        try!(serializer.serialize_map_key(&mut state, "type"));
        try!(serializer.serialize_map_value(&mut state, "LAUNCH_WORKER"));
        try!(serializer.serialize_map_key(&mut state, "data"));
        try!(serializer.serialize_map_value(&mut state, tag));
      }
    };

    serializer.serialize_map_end(state)
  }
}


#[cfg(test)]
mod tests {
  use super::*;
  use serde_json;
  use sozu::messages::{Order,HttpFront};

  #[test]
  fn config_message_test() {
    let raw_json = r#"{ "id": "ID_TEST", "version": 0, "type": "PROXY", "proxy": "HTTP", "data":{"type": "ADD_HTTP_FRONT", "data": {"app_id": "xxx", "hostname": "yyy", "path_begin": "xxx"}} }"#;
    let message: ConfigMessage = serde_json::from_str(raw_json).unwrap();
    println!("{:?}", message);
    assert_eq!(message.proxy, Some(String::from("HTTP")));
    assert_eq!(message.data, ConfigCommand::ProxyConfiguration(Order::AddHttpFront(HttpFront{
      app_id: String::from("xxx"),
      hostname: String::from("yyy"),
      path_begin: String::from("xxx"),
    })));
  }

  #[test]
  fn save_state_test() {
    let raw_json = r#"{ "id": "ID_TEST", "version": 0, "type": "SAVE_STATE", "data":{ "path": "./config_dump.json"} }"#;
    let message: ConfigMessage = serde_json::from_str(raw_json).unwrap();
    println!("{:?}", message);
    assert_eq!(message.proxy, None);
    assert_eq!(message.data, ConfigCommand::SaveState(String::from("./config_dump.json")));
  }

  #[test]
  fn dump_state_test() {
    println!("A");
    //let raw_json = r#"{ "id": "ID_TEST", "type": "DUMP_STATE" }"#;
    let raw_json = "{ \"id\": \"ID_TEST\", \"version\": 0, \"type\": \"DUMP_STATE\" }";
    println!("B");
    let message: ConfigMessage = serde_json::from_str(raw_json).unwrap();
    println!("C");
    println!("{:?}", message);
    assert_eq!(message.proxy, None);
    println!("D");
    assert_eq!(message.data, ConfigCommand::DumpState);
  }
}
