use serde;
use serde::ser::SerializeMap;
use serde_json;
use std::fmt;
use std::net::SocketAddr;
use std::collections::BTreeMap;

use state::ConfigState;
use proxy::{AggregatedMetricsData,ProxyRequestData,QueryAnswer,ProxyEvent};

pub const PROTOCOL_VERSION: u8 = 0;

#[derive(Debug,Clone,PartialEq,Eq,Hash)]
pub enum CommandRequestData {
  Proxy(ProxyRequestData),
  SaveState(String),
  LoadState(String),
  DumpState,
  ListWorkers,
  LaunchWorker(String),
  UpgradeMaster,
  UpgradeWorker(u32),
  SubscribeEvents,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash)]
pub struct CommandRequest {
  pub id:        String,
  pub version:   u8,
  pub data:      CommandRequestData,
  pub worker_id: Option<u32>,
}

impl CommandRequest {
  pub fn new(id: String, data: CommandRequestData, worker_id: Option<u32>) -> CommandRequest {
    CommandRequest {
      version:  PROTOCOL_VERSION,
      id,
      data,
      worker_id,
    }
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CommandStatus {
  Ok,
  Processing,
  Error
}

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CommandResponseData {
  Workers(Vec<WorkerInfo>),
  Metrics(AggregatedMetricsData),
  Query(BTreeMap<String, QueryAnswer>),
  State(ConfigState),
  Event(Event),
}

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct CommandResponse {
  pub id:      String,
  pub version: u8,
  pub status:  CommandStatus,
  pub message: String,
  pub data:    Option<CommandResponseData>,
}

impl CommandResponse {
  pub fn new(id: String, status: CommandStatus, message: String, data: Option<CommandResponseData>) -> CommandResponse {
    CommandResponse {
      version: PROTOCOL_VERSION,
      id,
      status,
      message,
      data,
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

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Event {
  BackendDown(String, SocketAddr),
  NoAvailableBackends(String),
}

impl From<ProxyEvent> for Event {
  fn from(e: ProxyEvent) -> Self {
    match e {
      ProxyEvent::BackendDown(id, addr) => Event::BackendDown(id, addr),
      ProxyEvent::NoAvailableBackends(app_id) => Event::NoAvailableBackends(app_id),
    }
  }
}

enum CommandRequestField {
  Id,
  Version,
  WorkerId,
  Type,
  Data,
}

impl<'de> serde::Deserialize<'de> for CommandRequestField {
  fn deserialize<D>(deserializer: D) -> Result<CommandRequestField, D::Error>
        where D: serde::de::Deserializer<'de> {
    struct CommandRequestFieldVisitor;
    impl<'de> serde::de::Visitor<'de> for CommandRequestFieldVisitor {
      type Value = CommandRequestField;

      fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("expected id, version, worker id, type or data")
      }

      fn visit_str<E>(self, value: &str) -> Result<CommandRequestField, E>
        where E: serde::de::Error {
        match value {
          "id"        => Ok(CommandRequestField::Id),
          "version"   => Ok(CommandRequestField::Version),
          "type"      => Ok(CommandRequestField::Type),
          "worker_id" => Ok(CommandRequestField::WorkerId),
          "data"      => Ok(CommandRequestField::Data),
          e => Err(serde::de::Error::custom(format!("expected id, version, worker id, type or data, got: {}", e))),
        }
      }
    }

    deserializer.deserialize_any(CommandRequestFieldVisitor)
  }
}

struct CommandRequestVisitor;
impl<'de> serde::de::Visitor<'de> for CommandRequestVisitor {
  type Value = CommandRequest;

  fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
    formatter.write_str("")
  }

  fn visit_map<V>(self, mut visitor: V) -> Result<CommandRequest, V::Error>
        where V: serde::de::MapAccess<'de> {
    let mut id:Option<String>              = None;
    let mut version:Option<u8>             = None;
    let mut worker_id: Option<u32>          = None;
    let mut config_type:Option<String>     = None;
    let mut data:Option<serde_json::Value> = None;

    loop {
      match visitor.next_key()? {
        Some(CommandRequestField::Type)    => { config_type = Some(visitor.next_value()?); }
        Some(CommandRequestField::Id)      => { id = Some(visitor.next_value()?); }
        Some(CommandRequestField::Version) => { version = Some(visitor.next_value()?); }
        Some(CommandRequestField::WorkerId) => { worker_id = Some(visitor.next_value()?); }
        Some(CommandRequestField::Data)    => { data = Some(visitor.next_value()?); }
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

    let data = if config_type == "PROXY" {
      let data = match data {
        Some(data) => data,
        None => return Err(serde::de::Error::missing_field("data")),
      };
      let command = serde_json::from_value(data).or_else(|_| Err(serde::de::Error::custom("proxy configuration command")))?;
      CommandRequestData::Proxy(command)
    } else if config_type == "SAVE_STATE" {
      let data = match data {
        Some(data) => data,
        None => return Err(serde::de::Error::missing_field("data")),
      };
      let state: SaveStateData = serde_json::from_value(data).or_else(|_| Err(serde::de::Error::custom("save state")))?;
      CommandRequestData::SaveState(state.path)
    } else if config_type == "DUMP_STATE" {
      CommandRequestData::DumpState
    } else if config_type == "LOAD_STATE" {
      let data = match data {
        Some(data) => data,
        None => return Err(serde::de::Error::missing_field("data")),
      };
      let state: SaveStateData = serde_json::from_value(data).or_else(|_| Err(serde::de::Error::custom("save state")))?;
      CommandRequestData::LoadState(state.path)
    } else if config_type == "LIST_WORKERS" {
      CommandRequestData::ListWorkers
    } else if config_type == "LAUNCH_WORKER" {
      let data = match data {
        Some(data) => data,
        None => return Err(serde::de::Error::missing_field("data")),
      };
      CommandRequestData::LaunchWorker(serde_json::from_value(data).or_else(|_| Err(serde::de::Error::custom("launch worker")))?)
    } else if config_type == "UPGRADE_MASTER" {
      CommandRequestData::UpgradeMaster
    } else if config_type == "UPGRADE_WORKER" {
      let data = match data {
        Some(data) => data,
        None => return Err(serde::de::Error::missing_field("data")),
      };
      CommandRequestData::UpgradeWorker(serde_json::from_value(data).or_else(|_| Err(serde::de::Error::custom("upgrade worker")))?)
    } else if config_type == "SUBSCRIBE_EVENTS" {
      CommandRequestData::SubscribeEvents
    } else {
      return Err(serde::de::Error::custom("unrecognized command"));
    };

    Ok(CommandRequest {
      id,
      version: PROTOCOL_VERSION,
      data,
      worker_id,
    })
  }
}

impl<'de> serde::Deserialize<'de> for CommandRequest {
  fn deserialize<D>(deserializer: D) -> Result<CommandRequest, D::Error>
    where D: serde::de::Deserializer<'de> {
    static FIELDS: &'static [&'static str] = &["id", "version", "worker_id", "type", "data"];
    deserializer.deserialize_struct("CommandRequest", FIELDS, CommandRequestVisitor)
  }
}

#[derive(Serialize)]
struct StatePath {
  path: String
}

impl serde::Serialize for CommandRequest {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
      where S: serde::Serializer,
  {
    let mut count = 4;
    if self.worker_id.is_some() {
      count += 1;
    }
    let mut map = serializer.serialize_map(Some(count))?;

    map.serialize_entry("id", &self.id)?;
    map.serialize_entry("version", &self.version)?;

    if self.worker_id.is_some() {
      map.serialize_entry("worker_id", self.worker_id.as_ref().unwrap())?;
    }

    match self.data {
      CommandRequestData::Proxy(ref order) => {
        map.serialize_entry("type", "PROXY")?;
        map.serialize_entry("data", order)?;
      },
      CommandRequestData::SaveState(ref path) => {
        map.serialize_entry("type", "SAVE_STATE")?;
        map.serialize_entry("data", &StatePath { path: path.to_string() })?;
      },
      CommandRequestData::LoadState(ref path) => {
        map.serialize_entry("type", "LOAD_STATE")?;
        map.serialize_entry("data", &StatePath { path: path.to_string() })?;
      },
      CommandRequestData::DumpState => {
        map.serialize_entry("type", "DUMP_STATE")?;
      },
      CommandRequestData::ListWorkers => {
        map.serialize_entry("type", "LIST_WORKERS")?;
      },
      CommandRequestData::LaunchWorker(ref tag) => {
        map.serialize_entry("type", "LAUNCH_WORKER")?;
        map.serialize_entry("data", tag)?;
      },
      CommandRequestData::UpgradeMaster => {
        map.serialize_entry("type", "UPGRADE_MASTER")?;
      },
      CommandRequestData::UpgradeWorker(ref id) => {
        map.serialize_entry("type", "UPGRADE_WORKER")?;
        map.serialize_entry("data", id)?;
      },
      CommandRequestData::SubscribeEvents => {
        map.serialize_entry("type", "SUBSCRIBE_EVENTS")?;
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
  use proxy::{Application,CertificateAndKey,CertFingerprint,ProxyRequestData,HttpFront,Backend,
    AppMetricsData,MetricsData,FilteredData,Percentiles,RemoveBackend,
    AddCertificate,RemoveCertificate,LoadBalancingParams,RulePosition,PathRule};
  use config::{LoadBalancingAlgorithms,ProxyProtocolConfig};

  #[test]
  fn config_message_test() {
    let raw_json = r#"{ "id": "ID_TEST", "version": 0, "type": "PROXY", "data":{"type": "ADD_HTTP_FRONT", "data": {"app_id": "xxx", "hostname": "yyy", "path": {"PREFIX": "xxx"}, "address": "0.0.0.0:8080"}} }"#;
    let message: CommandRequest = serde_json::from_str(raw_json).unwrap();
    println!("{:?}", message);
    assert_eq!(message.data, CommandRequestData::Proxy(ProxyRequestData::AddHttpFront(HttpFront{
      app_id: String::from("xxx"),
      hostname: String::from("yyy"),
      path: PathRule::Prefix(String::from("xxx")),
      address: "0.0.0.0:8080".parse().unwrap(),
      position: RulePosition::Tree,
    })));
  }

  #[test]
  fn protocol_version_mismatch_test() {
    let data = include_str!("../assets/protocol_mismatch.json");
    let msg = format!("configuration protocol version mismatch: Sōzu handles up to version {}, the message uses version {} at line 14 column 1", PROTOCOL_VERSION, 1);
    let res: Result<CommandRequest, serde_json::Error> = serde_json::from_str(data);

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

        let message: CommandRequest = serde_json::from_str(data).unwrap();
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

        let message: CommandResponse = serde_json::from_str(data).unwrap();
        assert_eq!(message, $expected_message, "\ndeserialized message:\n{:#?}\n\nexpected message:\n{:#?}", message, $expected_message);

      }

    )
  );

  test_message!(add_application, "../assets/add_application.json", CommandRequest {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     CommandRequestData::Proxy(ProxyRequestData::AddApplication(Application {
                  app_id: String::from("xxx"),
                  sticky_session: true,
                  https_redirect: true,
                  proxy_protocol: Some(ProxyProtocolConfig::ExpectHeader),
                  load_balancing_policy: LoadBalancingAlgorithms::RoundRobin,
                  answer_503: None,
      })),
      worker_id: None
    });

  test_message!(remove_application, "../assets/remove_application.json", CommandRequest {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     CommandRequestData::Proxy(ProxyRequestData::RemoveApplication( String::from("xxx") )),
      worker_id: None
    });

  test_message!(add_http_front, "../assets/add_http_front.json", CommandRequest {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     CommandRequestData::Proxy(ProxyRequestData::AddHttpFront(HttpFront{
                  app_id: String::from("xxx"),
                  hostname: String::from("yyy"),
                  path: PathRule::Prefix(String::from("xxx")),
                  address: "0.0.0.0:8080".parse().unwrap(),
                  position: RulePosition::Tree,
      })),
      worker_id: None
    });

  test_message!(remove_http_front, "../assets/remove_http_front.json", CommandRequest {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     CommandRequestData::Proxy(ProxyRequestData::RemoveHttpFront(HttpFront{
                  app_id: String::from("xxx"),
                  hostname: String::from("yyy"),
                  path: PathRule::Prefix(String::from("xxx")),
                  address: "0.0.0.0:8080".parse().unwrap(),
                  position: RulePosition::Tree,
      })),
      worker_id: None
    });

  test_message!(add_https_front, "../assets/add_https_front.json", CommandRequest {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     CommandRequestData::Proxy(ProxyRequestData::AddHttpsFront(HttpFront{
                  app_id: String::from("xxx"),
                  hostname: String::from("yyy"),
                  path: PathRule::Prefix(String::from("xxx")),
                  address: "0.0.0.0:8443".parse().unwrap(),
                  position: RulePosition::Tree,
      })),
      worker_id: None
    });

  test_message!(remove_https_front, "../assets/remove_https_front.json", CommandRequest {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     CommandRequestData::Proxy(ProxyRequestData::RemoveHttpsFront(HttpFront{
                  app_id: String::from("xxx"),
                  hostname: String::from("yyy"),
                  path: PathRule::Prefix(String::from("xxx")),
                  address: "0.0.0.0:8443".parse().unwrap(),
                  position: RulePosition::Tree,
      })),
      worker_id: None
    });

  const KEY        : &'static str = include_str!("../../lib/assets/key.pem");
  const CERTIFICATE: &'static str = include_str!("../../lib/assets/certificate.pem");
  const CHAIN      : &'static str = include_str!("../../lib/assets/certificate_chain.pem");

  test_message!(add_certificate, "../assets/add_certificate.json", CommandRequest {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     CommandRequestData::Proxy(ProxyRequestData::AddCertificate( AddCertificate{
        front: "0.0.0.0:443".parse().unwrap(),
        certificate: CertificateAndKey {
                  certificate: String::from(CERTIFICATE),
                  certificate_chain: split_certificate_chain(String::from(CHAIN)),
                  key: String::from(KEY),
        },
        names: Vec::new()
      })),
      worker_id: None
    });

  test_message!(remove_certificate, "../assets/remove_certificate.json", CommandRequest {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     CommandRequestData::Proxy(ProxyRequestData::RemoveCertificate(RemoveCertificate {
          front: "0.0.0.0:443".parse().unwrap(),
          fingerprint: CertFingerprint(FromHex::from_hex("ab2618b674e15243fd02a5618c66509e4840ba60e7d64cebec84cdbfeceee0c5").unwrap()),
          names: Vec::new(),
      })),
      worker_id: None
    });

  test_message!(add_backend, "../assets/add_backend.json", CommandRequest {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     CommandRequestData::Proxy(ProxyRequestData::AddBackend(Backend{
                  app_id: String::from("xxx"),
                  backend_id: String::from("xxx-0"),
                  address: "127.0.0.1:8080".parse().unwrap(),
                  load_balancing_parameters: Some(LoadBalancingParams{ weight: 0 }),
                  sticky_id: Some(String::from("xxx-0")),
                  backup: Some(false),
      })),
      worker_id: None
    });

  test_message!(remove_backend, "../assets/remove_backend.json", CommandRequest {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     CommandRequestData::Proxy(ProxyRequestData::RemoveBackend(RemoveBackend{
                  app_id: String::from("xxx"),
                  backend_id: String::from("xxx-0"),
                  address: "127.0.0.1:8080".parse().unwrap(),
      })),
      worker_id: None
    });

  test_message!(soft_stop, "../assets/soft_stop.json", CommandRequest {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     CommandRequestData::Proxy(ProxyRequestData::SoftStop),
      worker_id: Some(0),
    });

  test_message!(hard_stop, "../assets/hard_stop.json", CommandRequest {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     CommandRequestData::Proxy(ProxyRequestData::HardStop),
      worker_id: Some(0),
    });

  test_message!(status, "../assets/status.json", CommandRequest {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     CommandRequestData::Proxy(ProxyRequestData::Status),
      worker_id: Some(0),
    });

  test_message!(load_state, "../assets/load_state.json", CommandRequest {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     CommandRequestData::LoadState(String::from("./config_dump.json")),
      worker_id: None
    });

  test_message!(save_state, "../assets/save_state.json", CommandRequest {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     CommandRequestData::SaveState(String::from("./config_dump.json")),
      worker_id: None
    });

  test_message!(dump_state, "../assets/dump_state.json", CommandRequest {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     CommandRequestData::DumpState,
      worker_id: None
    });

  test_message!(list_workers, "../assets/list_workers.json", CommandRequest {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     CommandRequestData::ListWorkers,
      worker_id: None
    });

  test_message!(upgrade_master, "../assets/upgrade_master.json", CommandRequest {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     CommandRequestData::UpgradeMaster,
      worker_id: None
    });

  test_message!(upgrade_worker, "../assets/upgrade_worker.json", CommandRequest {
      id:       "ID_TEST".to_string(),
      version:  0,
      data:     CommandRequestData::UpgradeWorker(0),
      worker_id: None
    });

  test_message_answer!(answer_workers_status, "../assets/answer_workers_status.json", CommandResponse {
      id:       "ID_TEST".to_string(),
      version:  0,
      status:   CommandStatus::Ok,
      message:  String::from(""),
      data:     Some(CommandResponseData::Workers(vec!(
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

    test_message_answer!(answer_metrics, "../assets/answer_metrics.json", CommandResponse {
      id:       "ID_TEST".to_string(),
      version:  0,
      status:   CommandStatus::Ok,
      message:  String::from(""),
      data:     Some(CommandResponseData::Metrics(AggregatedMetricsData {
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
