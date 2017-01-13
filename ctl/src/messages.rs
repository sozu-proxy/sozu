use serde;
use sozu::messages::Order;

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
 pub enum ConfigMessageStatus {
   Ok,
   Processing,
   Error
 }

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize)]
pub enum ConfigCommand {
  ProxyConfiguration(Order),
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

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
pub struct ConfigMessageAnswer {
  pub id:      String,
  pub status:  ConfigMessageStatus,
  pub message: String
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize)]
pub struct StatePath<'a> {
  pub path: &'a str
}

impl serde::Serialize for ConfigMessage {
  fn serialize<S>(&self, serializer: &mut S) -> Result<(), S::Error>
      where S: serde::Serializer,
  {
    let mut len = if self.listener.is_some() { 4 } else { 3 };
    if self.data == ConfigCommand::DumpState { len = 2; }

    let mut state = try!(serializer.serialize_map(Some(len)));

    try!(serializer.serialize_map_key(&mut state, "id"));
    try!(serializer.serialize_map_value(&mut state, &self.id));

    if let Some(ref listener) = self.listener {
      try!(serializer.serialize_map_key(&mut state, "listener"));
      try!(serializer.serialize_map_value(&mut state, listener));
    }

    match &self.data {
      &ConfigCommand::ProxyConfiguration(ref command) => {
        try!(serializer.serialize_map_key(&mut state, "type"));
        try!(serializer.serialize_map_value(&mut state, "PROXY"));

        try!(serializer.serialize_map_key(&mut state, "data"));
        try!(serializer.serialize_map_value(&mut state, command));
      },
      &ConfigCommand::SaveState(ref path) => {
        try!(serializer.serialize_map_key(&mut state, "type"));
        try!(serializer.serialize_map_value(&mut state, "SAVE_STATE"));

        try!(serializer.serialize_map_key(&mut state, "data"));
        try!(serializer.serialize_map_value(&mut state, &StatePath {
          path: path
        }));
      },
      &ConfigCommand::LoadState(ref path) => {
        try!(serializer.serialize_map_key(&mut state, "type"));
        try!(serializer.serialize_map_value(&mut state, "LOAD_STATE"));

        try!(serializer.serialize_map_key(&mut state, "data"));
        try!(serializer.serialize_map_value(&mut state, &StatePath {
          path: path
        }));
      },
      &ConfigCommand::DumpState => {
        try!(serializer.serialize_map_key(&mut state, "type"));
        try!(serializer.serialize_map_value(&mut state, "DUMP_STATE"));
      },
    }
    serializer.serialize_map_end(state)
  }
}

