use serde;
use serde_json;
use rustc_serialize::{Decodable,Decoder,Encodable,Encoder};
use state::{HttpProxy,TlsProxy,ConfigState};
use sozu::messages::Command;

#[derive(Debug,Clone,Copy,PartialEq,Eq,Hash)]
pub enum ListenerType {
  HTTP,
  HTTPS,
  TCP
}

impl Encodable for ListenerType {
  fn encode<E: Encoder>(&self, e: &mut E) -> Result<(), E::Error> {
    match *self {
      ListenerType::HTTP  => e.emit_str("HTTP"),
      ListenerType::HTTPS => e.emit_str("HTTPS"),
      ListenerType::TCP   => e.emit_str("TCP"),
    }
  }
}

impl Decodable for ListenerType {
  fn decode<D: Decoder>(decoder: &mut D) -> Result<ListenerType, D::Error> {
    let tag = try!(decoder.read_str());
    match &tag[..] {
      "HTTP"  => Ok(ListenerType::HTTP),
      "HTTPS" => Ok(ListenerType::HTTPS),
      "TCP"   => Ok(ListenerType::TCP),
      _       => Err(decoder.error("unrecognized listener type"))
    }
  }
}

impl serde::Serialize for ListenerType {
  fn serialize<S>(&self, serializer: &mut S) -> Result<(), S::Error>
      where S: serde::Serializer,
  {
    match *self {
      ListenerType::HTTP  => serializer.serialize_str("HTTP"),
      ListenerType::HTTPS => serializer.serialize_str("HTTPS"),
      ListenerType::TCP   => serializer.serialize_str("TCP"),
    }
  }
}

impl serde::Deserialize for ListenerType {
  fn deserialize<D>(deserializer: &mut D) -> Result<ListenerType, D::Error>
    where D: serde::de::Deserializer
  {
    struct ListenerTypeVisitor;

    impl serde::de::Visitor for ListenerTypeVisitor {
      type Value = ListenerType;

      fn visit_str<E>(&mut self, value: &str) -> Result<ListenerType, E>
        where E: serde::de::Error
        {
          match value {
            "HTTP"  => Ok(ListenerType::HTTP),
            "HTTPS" => Ok(ListenerType::HTTPS),
            "TCP"   => Ok(ListenerType::TCP),
            _ => Err(serde::de::Error::custom("expected HTTP, HTTPS or TCP listener type")),
          }
        }
    }

    deserializer.deserialize(ListenerTypeVisitor)
  }
}


#[derive(Debug,Clone,PartialEq,Eq)]
pub struct ListenerDeserializer {
  pub tag:   String,
  pub state: ConfigState,
}

enum ListenerDeserializerField {
  Tag,
  Type,
  State,
}

impl serde::Deserialize for ListenerDeserializerField {
  fn deserialize<D>(deserializer: &mut D) -> Result<ListenerDeserializerField, D::Error>
        where D: serde::de::Deserializer {
    struct ListenerDeserializerFieldVisitor;
    impl serde::de::Visitor for ListenerDeserializerFieldVisitor {
      type Value = ListenerDeserializerField;

      fn visit_str<E>(&mut self, value: &str) -> Result<ListenerDeserializerField, E>
        where E: serde::de::Error {
        match value {
          "tag"           => Ok(ListenerDeserializerField::Tag),
          "listener_type" => Ok(ListenerDeserializerField::Type),
          "state"         => Ok(ListenerDeserializerField::State),
          _ => Err(serde::de::Error::custom("expected tag, listener_type or state")),
        }
      }
    }

    deserializer.deserialize(ListenerDeserializerFieldVisitor)
  }
}

struct ListenerDeserializerVisitor;
impl serde::de::Visitor for ListenerDeserializerVisitor {
  type Value = ListenerDeserializer;

  fn visit_map<V>(&mut self, mut visitor: V) -> Result<ListenerDeserializer, V::Error>
        where V: serde::de::MapVisitor {
    let mut tag:Option<String>                 = None;
    let mut listener_type:Option<ListenerType> = None;
    let mut state:Option<serde_json::Value>    = None;

    loop {
      match try!(visitor.visit_key()) {
        Some(ListenerDeserializerField::Type)  => { listener_type = Some(try!(visitor.visit_value())); }
        Some(ListenerDeserializerField::Tag)   => { tag = Some(try!(visitor.visit_value())); }
        Some(ListenerDeserializerField::State) => { state = Some(try!(visitor.visit_value())); }
        None => { break; }
      }
    }

    println!("decoded type = {:?}, value= {:?}", listener_type, state);
    let listener_type = match listener_type {
      Some(listener) => listener,
      None => try!(visitor.missing_field("listener_type")),
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

    let state = match listener_type {
      ListenerType::HTTP => {
        let http_proxy: HttpProxy = try!(serde_json::from_value(state).or(Err(serde::de::Error::custom("http_proxy"))));
        ConfigState::Http(http_proxy)
      },
      ListenerType::HTTPS => {
        let tls_proxy: TlsProxy = try!(serde_json::from_value(state).or(Err(serde::de::Error::custom("tls_proxy"))));
        ConfigState::Tls(tls_proxy)
      },
      ListenerType::TCP => {
        ConfigState::Tcp
      }
    };

    Ok(ListenerDeserializer {
      tag: tag,
      state: state,
    })
  }
}

impl serde::Deserialize for ListenerDeserializer {
  fn deserialize<D>(deserializer: &mut D) -> Result<ListenerDeserializer, D::Error>
        where D: serde::de::Deserializer {
    static FIELDS: &'static [&'static str] = &["tag", "listener_type", "state"];
    deserializer.deserialize_struct("Listener", FIELDS, ListenerDeserializerVisitor)
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize)]
pub enum ConfigCommand {
  ProxyConfiguration(Command),
  SaveState(String),
  LoadState(String),
  DumpState,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash)]
pub struct ConfigMessage {
  pub id:       String,
  pub data:     ConfigCommand,
  pub listener: Option<String>,
}

#[derive(Deserialize)]
struct SaveStateData {
  path : String
}

enum ConfigMessageField {
  Id,
  Listener,
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
          "type"     => Ok(ConfigMessageField::Type),
          "listener" => Ok(ConfigMessageField::Listener),
          "data"     => Ok(ConfigMessageField::Data),
          _ => Err(serde::de::Error::custom("expected id, listener, type or data")),
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
    let mut listener: Option<String>       = None;
    let mut config_type:Option<String>     = None;
    let mut data:Option<serde_json::Value> = None;

    loop {
      match try!(visitor.visit_key()) {
        Some(ConfigMessageField::Type)     => { config_type = Some(try!(visitor.visit_value())); }
        Some(ConfigMessageField::Id)       => { id = Some(try!(visitor.visit_value())); }
        Some(ConfigMessageField::Listener) => { listener = Some(try!(visitor.visit_value())); }
        Some(ConfigMessageField::Data)     => { data = Some(try!(visitor.visit_value())); }
        None => { break; }
      }
    }

    //println!("decoded type = {:?}, value= {:?}", listener_type, state);
    let config_type = match config_type {
      Some(config) => config,
      None => try!(visitor.missing_field("type")),
    };
    let id = match id {
      Some(id) => id,
      None => try!(visitor.missing_field("id")),
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
    } else {
      return Err(serde::de::Error::custom("unrecognized command"));
    };

    Ok(ConfigMessage {
      id:       id,
      data:     data,
      listener: listener
    })
  }
}

impl serde::Deserialize for ConfigMessage {
  fn deserialize<D>(deserializer: &mut D) -> Result<ConfigMessage, D::Error>
    where D: serde::de::Deserializer {
    static FIELDS: &'static [&'static str] = &["id", "listener", "type", "data"];
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
    let mut state = if self.listener.is_some() {
      try!(serializer.serialize_map(Some(4)))
    } else {
      try!(serializer.serialize_map(Some(3)))
    };

    try!(serializer.serialize_map_key(&mut state, "id"));
    try!(serializer.serialize_map_value(&mut state, &self.id));

    if self.listener.is_some() {
      try!(serializer.serialize_map_key(&mut state, "listener"));
      try!(serializer.serialize_map_value(&mut state, self.listener.as_ref().unwrap()));
    }

    match self.data {
      ConfigCommand::ProxyConfiguration(ref command) => {
        try!(serializer.serialize_map_key(&mut state, "type"));
        try!(serializer.serialize_map_value(&mut state, "PROXY"));
        try!(serializer.serialize_map_key(&mut state, "data"));
        try!(serializer.serialize_map_value(&mut state, command));
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
      }
    };

    serializer.serialize_map_end(state)
  }
}


#[cfg(test)]
mod tests {
  use super::*;
  use serde_json;
  use sozu::messages::{Command,HttpFront};

  #[test]
  fn config_message_test() {
    let raw_json = r#"{ "id": "ID_TEST", "type": "PROXY", "listener": "HTTP", "data":{"type": "ADD_HTTP_FRONT", "data": {"app_id": "xxx", "hostname": "yyy", "path_begin": "xxx"}} }"#;
    let message: ConfigMessage = serde_json::from_str(raw_json).unwrap();
    println!("{:?}", message);
    assert_eq!(message.listener, Some(String::from("HTTP")));
    assert_eq!(message.data, ConfigCommand::ProxyConfiguration(Command::AddHttpFront(HttpFront{
      app_id: String::from("xxx"),
      hostname: String::from("yyy"),
      path_begin: String::from("xxx"),
    })));
  }

  #[test]
  fn save_state_test() {
    let raw_json = r#"{ "id": "ID_TEST", "type": "SAVE_STATE", "data":{ "path": "./config_dump.json"} }"#;
    let message: ConfigMessage = serde_json::from_str(raw_json).unwrap();
    println!("{:?}", message);
    assert_eq!(message.listener, None);
    assert_eq!(message.data, ConfigCommand::SaveState(String::from("./config_dump.json")));
  }

  #[test]
  fn dump_state_test() {
    println!("A");
    //let raw_json = r#"{ "id": "ID_TEST", "type": "DUMP_STATE" }"#;
    let raw_json = "{ \"id\": \"ID_TEST\", \"type\": \"DUMP_STATE\" }";
    println!("B");
    let message: ConfigMessage = serde_json::from_str(raw_json).unwrap();
    println!("C");
    println!("{:?}", message);
    assert_eq!(message.listener, None);
    println!("D");
    assert_eq!(message.data, ConfigCommand::DumpState);
  }
}
