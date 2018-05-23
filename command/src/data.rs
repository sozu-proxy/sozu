use serde;
use serde::ser::SerializeMap;
use serde_json;
use std::fmt;
use std::collections::BTreeMap;

use state::ConfigState;
use messages::{AggregatedMetricsData,Order,Query,QueryAnswer, LoadBalancingParams};
use config::LoadBalancingAlgorithms;

pub const PROTOCOL_VERSION: u8 = 0;

#[derive(Debug,Clone,PartialEq,Eq,Hash)]
pub enum ConfigCommand {
  ProxyConfiguration(Order),
  SaveState(String),
  LoadState(String),
  DumpState,
  ListWorkers,
  LaunchWorker(String),
  UpgradeMaster,
  Metrics,
  Query(Query),
  UpgradeWorker(u32),
}

#[derive(Debug,Clone,PartialEq,Eq,Hash)]
pub struct ConfigMessage {
  pub id:       String,
  pub version:  u8,
  pub data:     ConfigCommand,
  pub proxy_id: Option<u32>,
}

impl ConfigMessage {
  pub fn new(id: String, data: ConfigCommand, proxy_id: Option<u32>) -> ConfigMessage {
    ConfigMessage {
      id:       id,
      version:  PROTOCOL_VERSION,
      data:     data,
      proxy_id: proxy_id,
    }
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ConfigMessageStatus {
  Ok,
  Processing,
  Error
}

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AnswerData {
  Workers(Vec<WorkerInfo>),
  Metrics(AggregatedMetricsData),
  Query(BTreeMap<String, QueryAnswer>),
  State(ConfigState),
}

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
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

#[derive(Debug,Clone,Copy,PartialEq,Eq,Hash,Serialize,Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RunState {
  Running,
  Stopping,
  Stopped,
  NotAnswering,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
pub struct WorkerInfo {
  pub id:         u32,
  pub pid:        i32,
  pub run_state:  RunState,
}

#[derive(Deserialize)]
struct SaveStateData {
  path : String
}

enum ConfigMessageField {
  Id,
  Version,
  ProxyId,
  Type,
  Data,
}

impl<'de> serde::Deserialize<'de> for ConfigMessageField {
  fn deserialize<D>(deserializer: D) -> Result<ConfigMessageField, D::Error>
        where D: serde::de::Deserializer<'de> {
    struct ConfigMessageFieldVisitor;
    impl<'de> serde::de::Visitor<'de> for ConfigMessageFieldVisitor {
      type Value = ConfigMessageField;

      fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("expected id, version, proxy id, type or data")
      }

      fn visit_str<E>(self, value: &str) -> Result<ConfigMessageField, E>
        where E: serde::de::Error {
        match value {
          "id"       => Ok(ConfigMessageField::Id),
          "version"  => Ok(ConfigMessageField::Version),
          "type"     => Ok(ConfigMessageField::Type),
          "proxy_id" => Ok(ConfigMessageField::ProxyId),
          "data"     => Ok(ConfigMessageField::Data),
          e => Err(serde::de::Error::custom(format!("expected id, version, proxy id, type or data, got: {}", e))),
        }
      }
    }

    deserializer.deserialize_any(ConfigMessageFieldVisitor)
  }
}

struct ConfigMessageVisitor;
impl<'de> serde::de::Visitor<'de> for ConfigMessageVisitor {
  type Value = ConfigMessage;

  fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
    formatter.write_str("")
  }

  fn visit_map<V>(self, mut visitor: V) -> Result<ConfigMessage, V::Error>
        where V: serde::de::MapAccess<'de> {
    let mut id:Option<String>              = None;
    let mut version:Option<u8>             = None;
    let mut proxy_id: Option<u32>          = None;
    let mut config_type:Option<String>     = None;
    let mut data:Option<serde_json::Value> = None;

    loop {
      match try!(visitor.next_key()) {
        Some(ConfigMessageField::Type)    => { config_type = Some(try!(visitor.next_value())); }
        Some(ConfigMessageField::Id)      => { id = Some(try!(visitor.next_value())); }
        Some(ConfigMessageField::Version) => { version = Some(try!(visitor.next_value())); }
        Some(ConfigMessageField::ProxyId) => { proxy_id = Some(try!(visitor.next_value())); }
        Some(ConfigMessageField::Data)    => { data = Some(try!(visitor.next_value())); }
        None => { break; }
      }
    }

    //println!("decoded type = {:?}, value= {:?}", proxy_type, state);
    let config_type = match config_type {
      Some(config) => config,
      None => return Err(serde::de::Error::missing_field("type")),
    };
    let id = match id {
      Some(id) => id,
      None => return Err(serde::de::Error::missing_field("id")),
    };
    let _version = match version {
      Some(version) => {
        if version > PROTOCOL_VERSION {
          let msg = format!("configuration protocol version mismatch: Sōzu handles up to version {}, the message uses version {}", PROTOCOL_VERSION, version);
          return Err(serde::de::Error::custom(msg));
        } else {
          version
        }
      },
      None => return Err(serde::de::Error::missing_field("version")),
    };

    let data = if &config_type == "PROXY" {
      let data = match data {
        Some(data) => data,
        None => return Err(serde::de::Error::missing_field("data")),
      };
      let command = try!(serde_json::from_value(data).or(Err(serde::de::Error::custom("proxy configuration command"))));
      ConfigCommand::ProxyConfiguration(command)
    } else if &config_type == &"SAVE_STATE" {
      let data = match data {
        Some(data) => data,
        None => return Err(serde::de::Error::missing_field("data")),
      };
      let state: SaveStateData = try!(serde_json::from_value(data).or(Err(serde::de::Error::custom("save state"))));
      ConfigCommand::SaveState(state.path)
    } else if &config_type == &"DUMP_STATE" {
      ConfigCommand::DumpState
    } else if &config_type == &"LOAD_STATE" {
      let data = match data {
        Some(data) => data,
        None => return Err(serde::de::Error::missing_field("data")),
      };
      let state: SaveStateData = try!(serde_json::from_value(data).or(Err(serde::de::Error::custom("save state"))));
      ConfigCommand::LoadState(state.path)
    } else if &config_type == &"LIST_WORKERS" {
      ConfigCommand::ListWorkers
    } else if &config_type == &"LAUNCH_WORKER" {
      let data = match data {
        Some(data) => data,
        None => return Err(serde::de::Error::missing_field("data")),
      };
      ConfigCommand::LaunchWorker(try!(serde_json::from_value(data).or(Err(serde::de::Error::custom("launch worker")))))
    } else if &config_type == &"UPGRADE_MASTER" {
      ConfigCommand::UpgradeMaster
    } else if &config_type == &"UPGRADE_WORKER" {
      let data = match data {
        Some(data) => data,
        None => return Err(serde::de::Error::missing_field("data")),
      };
      ConfigCommand::UpgradeWorker(try!(serde_json::from_value(data).or(Err(serde::de::Error::custom("upgrade worker")))))
    } else if &config_type == &"METRICS" {
      ConfigCommand::Metrics
    } else if &config_type == &"QUERY" {
      let data = match data {
        Some(data) => data,
        None => return Err(serde::de::Error::missing_field("data")),
      };
      ConfigCommand::Query(try!(serde_json::from_value(data).or(Err(serde::de::Error::custom("launch worker")))))
    } else {
      return Err(serde::de::Error::custom("unrecognized command"));
    };

    Ok(ConfigMessage {
      id:      id,
      version: PROTOCOL_VERSION,
      data:    data,
      proxy_id: proxy_id,
    })
  }
}

impl<'de> serde::Deserialize<'de> for ConfigMessage {
  fn deserialize<D>(deserializer: D) -> Result<ConfigMessage, D::Error>
    where D: serde::de::Deserializer<'de> {
    static FIELDS: &'static [&'static str] = &["id", "version", "proxy_id", "type", "data"];
    deserializer.deserialize_struct("ConfigMessage", FIELDS, ConfigMessageVisitor)
  }
}

#[derive(Serialize)]
struct StatePath {
  path: String
}

impl serde::Serialize for ConfigMessage {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
      where S: serde::Serializer,
  {
    let mut count = 4;
    if self.proxy_id.is_some() {
      count += 1;
    }
    let mut map = try!(serializer.serialize_map(Some(count)));

    try!(map.serialize_entry("id", &self.id));
    try!(map.serialize_entry("version", &self.version));

    if self.proxy_id.is_some() {
      try!(map.serialize_entry("proxy_id", self.proxy_id.as_ref().unwrap()));
    }

    match self.data {
      ConfigCommand::ProxyConfiguration(ref order) => {
        try!(map.serialize_entry("type", "PROXY"));
        try!(map.serialize_entry("data", order));
      },
      ConfigCommand::SaveState(ref path) => {
        try!(map.serialize_entry("type", "SAVE_STATE"));
        try!(map.serialize_entry("data", &StatePath { path: path.to_string() }));
      },
      ConfigCommand::LoadState(ref path) => {
        try!(map.serialize_entry("type", "LOAD_STATE"));
        try!(map.serialize_entry("data", &StatePath { path: path.to_string() }));
      },
      ConfigCommand::DumpState => {
        try!(map.serialize_entry("type", "DUMP_STATE"));
      },
      ConfigCommand::ListWorkers => {
        try!(map.serialize_entry("type", "LIST_WORKERS"));
      },
      ConfigCommand::LaunchWorker(ref tag) => {
        try!(map.serialize_entry("type", "LAUNCH_WORKER"));
        try!(map.serialize_entry("data", tag));
      },
      ConfigCommand::UpgradeMaster => {
        try!(map.serialize_entry("type", "UPGRADE_MASTER"));
      },
      ConfigCommand::Metrics => {
        try!(map.serialize_entry("type", "METRICS"));
      },
      ConfigCommand::Query(ref query) => {
        try!(map.serialize_entry("type", "QUERY"));
        try!(map.serialize_entry("data", query));
      },
      ConfigCommand::UpgradeWorker(ref id) => {
        try!(map.serialize_entry("type", "UPGRADE_WORKER"));
        try!(map.serialize_entry("data", id));
      },
    };

    map.end()
  }
}


#[cfg(test)]
mod tests {
  use super::*;
  use serde_json;
  use hex::FromHex;
  use certificate::split_certificate_chain;
  use messages::{Application,CertificateAndKey,CertFingerprint,Order,HttpFront,HttpsFront,Backend};
  use messages::{AppMetricsData,MetricsData,FilteredData,Percentiles};
  use messages::{AddCertificate,RemoveCertificate};
  use config::ProxyProtocolConfig;

  #[test]
  fn config_message_test() {
    let raw_json = r#"{ "id": "ID_TEST", "version": 0, "type": "PROXY", "data":{"type": "ADD_HTTP_FRONT", "data": {"app_id": "xxx", "hostname": "yyy", "path_begin": "xxx"}} }"#;
    let message: ConfigMessage = serde_json::from_str(raw_json).unwrap();
    println!("{:?}", message);
    assert_eq!(message.data, ConfigCommand::ProxyConfiguration(Order::AddHttpFront(HttpFront{
      app_id: String::from("xxx"),
      hostname: String::from("yyy"),
      path_begin: String::from("xxx"),
    })));
  }

  #[test]
  fn protocol_version_mismatch_test() {
    let data = include_str!("../assets/protocol_mismatch.json");
    let msg = format!("configuration protocol version mismatch: Sōzu handles up to version {}, the message uses version {} at line 14 column 1", PROTOCOL_VERSION, 1);
    let res: Result<ConfigMessage, serde_json::Error> = serde_json::from_str(data);

    let err = format!("{}", res.unwrap_err());
    assert_eq!(err, msg);
  }

  macro_rules! test_message (
    ($name: ident, $filename: expr, $expected_message: expr) => (

      #[test]
      fn $name() {
        let data = include_str!($filename);
        let pretty_print = serde_json::to_string_pretty(&$expected_message).expect("should have serialized");
        assert_eq!(&pretty_print, data, "\nserialized message:\n{}\n\nexpected message:\n{}", pretty_print, data);

        let message: ConfigMessage = serde_json::from_str(data).unwrap();
        assert_eq!(message, $expected_message, "\ndeserialized message:\n{:#?}\n\nexpected message:\n{:#?}", message, $expected_message);

      }

    )
  );

  macro_rules! test_message_answer (
    ($name: ident, $filename: expr, $expected_message: expr) => (

      #[test]
      fn $name() {
        let data = include_str!($filename);
        let pretty_print = serde_json::to_string_pretty(&$expected_message).expect("should have serialized");
        assert_eq!(&pretty_print, data, "\nserialized message:\n{}\n\nexpected message:\n{}", pretty_print, data);

        let message: ConfigMessageAnswer = serde_json::from_str(data).unwrap();
        assert_eq!(message, $expected_message, "\ndeserialized message:\n{:#?}\n\nexpected message:\n{:#?}", message, $expected_message);

      }

    )
  );

  test_message!(add_application, "../assets/add_application.json", ConfigMessage {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     ConfigCommand::ProxyConfiguration(Order::AddApplication(Application {
                  app_id: String::from("xxx"),
                  sticky_session: true,
                  https_redirect: true,
                  proxy_protocol: Some(ProxyProtocolConfig::ExpectHeader),
                  load_balancing_policy: LoadBalancingAlgorithms::RoundRobin,
      })),
      proxy_id: None
    });

  test_message!(remove_application, "../assets/remove_application.json", ConfigMessage {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     ConfigCommand::ProxyConfiguration(Order::RemoveApplication( String::from("xxx") )),
      proxy_id: None
    });

  test_message!(add_http_front, "../assets/add_http_front.json", ConfigMessage {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     ConfigCommand::ProxyConfiguration(Order::AddHttpFront(HttpFront{
                  app_id: String::from("xxx"),
                  hostname: String::from("yyy"),
                  path_begin: String::from("xxx"),
      })),
      proxy_id: None
    });

  test_message!(remove_http_front, "../assets/remove_http_front.json", ConfigMessage {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     ConfigCommand::ProxyConfiguration(Order::RemoveHttpFront(HttpFront{
                  app_id: String::from("xxx"),
                  hostname: String::from("yyy"),
                  path_begin: String::from("xxx"),
      })),
      proxy_id: None
    });

  test_message!(add_https_front, "../assets/add_https_front.json", ConfigMessage {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     ConfigCommand::ProxyConfiguration(Order::AddHttpsFront(HttpsFront{
                  app_id: String::from("xxx"),
                  hostname: String::from("yyy"),
                  path_begin: String::from("xxx"),
                  fingerprint: CertFingerprint(FromHex::from_hex("ab2618b674e15243fd02a5618c66509e4840ba60e7d64cebec84cdbfeceee0c5").unwrap()),
      })),
      proxy_id: None
    });

  test_message!(remove_https_front, "../assets/remove_https_front.json", ConfigMessage {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     ConfigCommand::ProxyConfiguration(Order::RemoveHttpsFront(HttpsFront{
                  app_id: String::from("xxx"),
                  hostname: String::from("yyy"),
                  path_begin: String::from("xxx"),
                  fingerprint: CertFingerprint(FromHex::from_hex("ab2618b674e15243fd02a5618c66509e4840ba60e7d64cebec84cdbfeceee0c5").unwrap()),
      })),
      proxy_id: None
    });

  const KEY        : &'static str = include_str!("../../lib/assets/key.pem");
  const CERTIFICATE: &'static str = include_str!("../../lib/assets/certificate.pem");
  const CHAIN      : &'static str = include_str!("../../lib/assets/certificate_chain.pem");

  test_message!(add_certificate, "../assets/add_certificate.json", ConfigMessage {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     ConfigCommand::ProxyConfiguration(Order::AddCertificate( AddCertificate{
        certificate: CertificateAndKey {
                  certificate: String::from(CERTIFICATE),
                  certificate_chain: split_certificate_chain(String::from(CHAIN)),
                  key: String::from(KEY),
        },
        names: Vec::new()
      })),
      proxy_id: None
    });

  test_message!(remove_certificate, "../assets/remove_certificate.json", ConfigMessage {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     ConfigCommand::ProxyConfiguration(Order::RemoveCertificate(RemoveCertificate {
          fingerprint: CertFingerprint(FromHex::from_hex("ab2618b674e15243fd02a5618c66509e4840ba60e7d64cebec84cdbfeceee0c5").unwrap()),
          names: Vec::new(),
      })),
      proxy_id: None
    });

  test_message!(add_backend, "../assets/add_backend.json", ConfigMessage {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     ConfigCommand::ProxyConfiguration(Order::AddBackend(Backend{
                  app_id: String::from("xxx"),
                  backend_id: String::from("xxx-0"),
                  ip_address: String::from("127.0.0.1"),
                  port: 8080,
                  load_balancing_parameters: Some(LoadBalancingParams{ weight: 0 }),
      })),
      proxy_id: None
    });

  test_message!(remove_backend, "../assets/remove_backend.json", ConfigMessage {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     ConfigCommand::ProxyConfiguration(Order::RemoveBackend(Backend{
                  app_id: String::from("xxx"),
                  backend_id: String::from("xxx-0"),
                  ip_address: String::from("127.0.0.1"),
                  port: 8080,
                  load_balancing_parameters: None,
      })),
      proxy_id: None
    });

  test_message!(soft_stop, "../assets/soft_stop.json", ConfigMessage {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     ConfigCommand::ProxyConfiguration(Order::SoftStop),
      proxy_id: Some(0),
    });

  test_message!(hard_stop, "../assets/hard_stop.json", ConfigMessage {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     ConfigCommand::ProxyConfiguration(Order::HardStop),
      proxy_id: Some(0),
    });

  test_message!(status, "../assets/status.json", ConfigMessage {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     ConfigCommand::ProxyConfiguration(Order::Status),
      proxy_id: Some(0),
    });

  test_message!(load_state, "../assets/load_state.json", ConfigMessage {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     ConfigCommand::LoadState(String::from("./config_dump.json")),
      proxy_id: None
    });

  test_message!(save_state, "../assets/save_state.json", ConfigMessage {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     ConfigCommand::SaveState(String::from("./config_dump.json")),
      proxy_id: None
    });

  test_message!(dump_state, "../assets/dump_state.json", ConfigMessage {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     ConfigCommand::DumpState,
      proxy_id: None
    });

  test_message!(list_workers, "../assets/list_workers.json", ConfigMessage {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     ConfigCommand::ListWorkers,
      proxy_id: None
    });

  test_message!(upgrade_master, "../assets/upgrade_master.json", ConfigMessage {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     ConfigCommand::UpgradeMaster,
      proxy_id: None
    });

  test_message!(upgrade_worker, "../assets/upgrade_worker.json", ConfigMessage {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     ConfigCommand::UpgradeWorker(0),
      proxy_id: None
    });

  test_message_answer!(answer_workers_status, "../assets/answer_workers_status.json", ConfigMessageAnswer {
      id:       "ID_TEST".to_string(),
      version:  0,
      status:   ConfigMessageStatus::Ok,
      message:  String::from(""),
      data:     Some(AnswerData::Workers(vec!(
        WorkerInfo {
          id:        1,
          pid:       5678,
          run_state: RunState::Running,
        },
        WorkerInfo {
          id:        0,
          pid:       1234,
          run_state: RunState::Stopping,
        },
      ))),
    });

    test_message_answer!(answer_metrics, "../assets/answer_metrics.json", ConfigMessageAnswer {
      id:       "ID_TEST".to_string(),
      version:  0,
      status:   ConfigMessageStatus::Ok,
      message:  String::from(""),
      data:     Some(AnswerData::Metrics(AggregatedMetricsData {
        master: [
          (String::from("sozu.gauge"), FilteredData::Gauge(1)),
          (String::from("sozu.count"), FilteredData::Count(-2)),
          (String::from("sozu.time"),  FilteredData::Time(1234)),
        ].iter().cloned().collect(),
        workers: [
          (String::from("0"), MetricsData {
            proxy: [
              (String::from("sozu.gauge"), FilteredData::Gauge(1)),
              (String::from("sozu.count"), FilteredData::Count(-2)),
              (String::from("sozu.time"),  FilteredData::Time(1234)),
            ].iter().cloned().collect(),
            applications: [
              (String::from("app_1"), AppMetricsData {
                data: [
                  (String::from("request_time"), FilteredData::Percentiles(Percentiles {
                    samples: 42,
                    p_50: 1,
                    p_90: 2,
                    p_99: 10,
                    p_99_9: 12,
                    p_99_99: 20,
                    p_99_999: 22,
                    p_100: 30,
                  }))
                ].iter().cloned().collect(),
                backends: [
                  (String::from("app_1-0"), [
                    (String::from("bytes_in"),  FilteredData::Count(256)),
                    (String::from("bytes_out"), FilteredData::Count(128)),
                    (String::from("percentiles"), FilteredData::Percentiles(Percentiles {
                      samples: 42,
                      p_50: 1,
                      p_90: 2,
                      p_99: 10,
                      p_99_9: 12,
                      p_99_99: 20,
                      p_99_999: 22,
                      p_100: 30,
                    }))
                  ].iter().cloned().collect())
                ].iter().cloned().collect(),
              })
            ].iter().cloned().collect()
          })
        ].iter().cloned().collect()
      }))
    });
}
