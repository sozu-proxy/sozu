use serde::{self,Serialize};
use serde::ser::SerializeMap;
use serde::de::{self, Visitor};
use serde_json;
use hex::{FromHex,ToHex};
use openssl::ssl;
use std::net::{IpAddr,SocketAddr};
use std::collections::{BTreeMap,HashSet};
use std::default::Default;
use std::convert::From;
use std::fmt;

use network::metrics::FilteredData;

pub type MessageId = String;

#[derive(Debug,Clone,PartialEq,Eq, Serialize, Deserialize)]
pub struct OrderMessageAnswer {
  pub id:     MessageId,
  pub status: OrderMessageStatus,
  pub data:   Option<OrderMessageAnswerData>
}

impl fmt::Display for OrderMessageAnswer {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    write!(f, "{}-{:?}", self.id, self.status)
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub enum OrderMessageStatus {
  Ok,
  Processing,
  Error(String),
}

#[derive(Debug,Clone,PartialEq,Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderMessageAnswerData {
  //placeholder for now
  Metrics(BTreeMap<String,FilteredData>)
}

#[derive(Debug,Clone,Serialize,Deserialize)]
pub struct OrderMessage {
  pub id:    MessageId,
  pub order: Order,
}

impl fmt::Display for OrderMessage {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    write!(f, "{}-{:?}", self.id, self.order)
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Order {
    AddHttpFront(HttpFront),
    RemoveHttpFront(HttpFront),

    AddHttpsFront(HttpsFront),
    RemoveHttpsFront(HttpsFront),

    AddCertificate(CertificateAndKey),
    RemoveCertificate(CertFingerprint),

    AddTcpFront(TcpFront),
    RemoveTcpFront(TcpFront),

    AddInstance(Instance),
    RemoveInstance(Instance),

    HttpProxy(HttpProxyConfiguration),
    HttpsProxy(HttpsProxyConfiguration),

    SoftStop,
    HardStop,

    Status,
    Metrics,
}


//FIXME: make fixed size depending on hash algorithm
#[derive(Clone,PartialEq,Eq,Hash)]
pub struct CertFingerprint(pub Vec<u8>);

impl fmt::Debug for CertFingerprint {
  fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
    write!(f, "CertFingerprint({})", self.0.to_hex())
  }
}
impl serde::Serialize for CertFingerprint {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
      where S: serde::Serializer,
  {
    serializer.serialize_str(&self.0.to_hex())
  }
}

struct CertFingerprintVisitor;

impl<'de> Visitor<'de> for CertFingerprintVisitor {
  type Value = CertFingerprint;

  fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
    formatter.write_str("the certificate fingerprint must be in hexadecimal format")
  }

  fn visit_str<E>(self, value: &str) -> Result<CertFingerprint, E>
    where E: de::Error
  {
    FromHex::from_hex(value)
      .map_err(|e| E::custom(format!("could not deserialize hex: {:?}", e)))
      .map(|v:Vec<u8>| CertFingerprint(v))
  }
}

impl<'de> serde::Deserialize<'de> for CertFingerprint {
  fn deserialize<D>(deserializer: D) -> Result<CertFingerprint, D::Error>
        where D: serde::de::Deserializer<'de> {
    deserializer.deserialize_str(CertFingerprintVisitor{})
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct HttpFront {
    pub app_id:     String,
    pub hostname:   String,
    pub path_begin: String,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct CertificateAndKey {
    pub certificate:       String,
    pub certificate_chain: Vec<String>,
    pub key:               String,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct HttpsFront {
    pub app_id:       String,
    pub hostname:     String,
    pub path_begin:   String,
    pub fingerprint:  CertFingerprint,
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
      front:           "127.0.0.1:8080".parse().expect("could not parse address"),
      front_timeout:   5000,
      back_timeout:    5000,
      max_connections: 1000,
      buffer_size:     16384,
      public_address:  None,
      answer_404:      String::from("HTTP/1.1 404 Not Found\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"),
      answer_503:      String::from("HTTP/1.1 503 your application is in deployment\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"),
    }
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct HttpsProxyConfiguration {
    pub front:                     SocketAddr,
    pub front_timeout:             u64,
    pub back_timeout:              u64,
    pub max_connections:           usize,
    pub buffer_size:               usize,
    pub public_address:            Option<IpAddr>,
    pub answer_404:                String,
    pub answer_503:                String,
    pub options:                   u64,
    pub cipher_list:               String,
    pub default_name:              Option<String>,
    pub default_app_id:            Option<String>,
    pub default_certificate:       Option<Vec<u8>>,
    pub default_key:               Option<Vec<u8>>,
    pub default_certificate_chain: Option<String>,
}

impl Default for HttpsProxyConfiguration {
  fn default() -> HttpsProxyConfiguration {
    HttpsProxyConfiguration {
      front:           "127.0.0.1:8443".parse().expect("could not parse address"),
      front_timeout:   5000,
      back_timeout:    5000,
      max_connections: 1000,
      buffer_size:     16384,
      public_address:  None,
      answer_404:      String::from("HTTP/1.1 404 Not Found\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"),
      answer_503:      String::from("HTTP/1.1 503 your application is in deployment\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"),
      cipher_list:     String::from(
        "ECDHE-ECDSA-CHACHA20-POLY1305:ECDHE-RSA-CHACHA20-POLY1305:\
        ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256:\
        ECDHE-ECDSA-AES256-GCM-SHA384:ECDHE-RSA-AES256-GCM-SHA384:\
        DHE-RSA-AES128-GCM-SHA256:DHE-RSA-AES256-GCM-SHA384:\
        ECDHE-ECDSA-AES128-SHA256:ECDHE-RSA-AES128-SHA256:\
        ECDHE-ECDSA-AES128-SHA:ECDHE-RSA-AES256-SHA384:\
        ECDHE-RSA-AES128-SHA:ECDHE-ECDSA-AES256-SHA384:\
        ECDHE-ECDSA-AES256-SHA:ECDHE-RSA-AES256-SHA:\
        DHE-RSA-AES128-SHA256:DHE-RSA-AES128-SHA:DHE-RSA-AES256-SHA256:\
        DHE-RSA-AES256-SHA:ECDHE-ECDSA-DES-CBC3-SHA:\
        ECDHE-RSA-DES-CBC3-SHA:EDH-RSA-DES-CBC3-SHA:\
        AES128-GCM-SHA256:AES256-GCM-SHA384:AES128-SHA256:\
        AES256-SHA256:AES128-SHA:AES256-SHA:DES-CBC3-SHA:!DSS"),
      options:         (ssl::SSL_OP_CIPHER_SERVER_PREFERENCE | ssl::SSL_OP_NO_COMPRESSION |
                         ssl::SSL_OP_NO_TICKET | ssl::SSL_OP_NO_SSLV2 |
                         ssl::SSL_OP_NO_SSLV3 | ssl::SSL_OP_NO_TLSV1).bits(),
      default_name:        Some(String::from("lolcatho.st")),
      default_app_id:      None,

      default_certificate: Some(Vec::from(&include_bytes!("../assets/certificate.pem")[..])),
      default_key:         Some(Vec::from(&include_bytes!("../assets/key.pem")[..])),
      default_certificate_chain: None,
    }
  }
}


impl Order {
  pub fn get_topics(&self) -> HashSet<Topic> {
    match *self {
      Order::AddHttpFront(_)      => [Topic::HttpProxyConfig].iter().cloned().collect(),
      Order::RemoveHttpFront(_)   => [Topic::HttpProxyConfig].iter().cloned().collect(),
      Order::AddHttpsFront(_)     => [Topic::HttpsProxyConfig].iter().cloned().collect(),
      Order::RemoveHttpsFront(_)  => [Topic::HttpsProxyConfig].iter().cloned().collect(),
      Order::AddCertificate(_)    => [Topic::HttpsProxyConfig].iter().cloned().collect(),
      Order::RemoveCertificate(_) => [Topic::HttpsProxyConfig].iter().cloned().collect(),
      Order::AddTcpFront(_)       => [Topic::TcpProxyConfig].iter().cloned().collect(),
      Order::RemoveTcpFront(_)    => [Topic::TcpProxyConfig].iter().cloned().collect(),
      Order::AddInstance(_)       => [Topic::HttpProxyConfig, Topic::HttpsProxyConfig, Topic::TcpProxyConfig].iter().cloned().collect(),
      Order::RemoveInstance(_)    => [Topic::HttpProxyConfig, Topic::HttpsProxyConfig, Topic::TcpProxyConfig].iter().cloned().collect(),
      Order::HttpProxy(_)         => [Topic::HttpProxyConfig].iter().cloned().collect(),
      Order::HttpsProxy(_)        => [Topic::HttpsProxyConfig].iter().cloned().collect(),
      Order::SoftStop             => [Topic::HttpProxyConfig, Topic::HttpsProxyConfig, Topic::TcpProxyConfig].iter().cloned().collect(),
      Order::HardStop             => [Topic::HttpProxyConfig, Topic::HttpsProxyConfig, Topic::TcpProxyConfig].iter().cloned().collect(),
      Order::Status               => [Topic::HttpProxyConfig, Topic::HttpsProxyConfig, Topic::TcpProxyConfig].iter().cloned().collect(),
      Order::Metrics              => HashSet::new(),
    }
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash)]
pub enum Topic {
    HttpProxyConfig,
    HttpsProxyConfig,
    TcpProxyConfig
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json;

  #[test]
  fn add_acl_test() {
    let raw_json = r#"{"type": "ADD_HTTP_FRONT", "data": {"app_id": "xxx", "hostname": "yyy", "path_begin": "xxx", "port": 4242}}"#;
    let command: Order = serde_json::from_str(raw_json).expect("could not parse json");
    println!("{:?}", command);
    assert!(command == Order::AddHttpFront(HttpFront{
      app_id: String::from("xxx"),
      hostname: String::from("yyy"),
      path_begin: String::from("xxx"),
    }));
  }

  #[test]
  fn remove_acl_test() {
    let raw_json = r#"{"type": "REMOVE_HTTP_FRONT", "data": {"app_id": "xxx", "hostname": "yyy", "path_begin": "xxx", "port": 4242}}"#;
    let command: Order = serde_json::from_str(raw_json).expect("could not parse json");
    println!("{:?}", command);
    assert!(command == Order::RemoveHttpFront(HttpFront{
      app_id: String::from("xxx"),
      hostname: String::from("yyy"),
      path_begin: String::from("xxx"),
    }));
  }


  #[test]
  fn add_instance_test() {
    let raw_json = r#"{"type": "ADD_INSTANCE", "data": {"app_id": "xxx", "ip_address": "yyy", "port": 8080}}"#;
    let command: Order = serde_json::from_str(raw_json).expect("could not parse json");
    println!("{:?}", command);
    assert!(command == Order::AddInstance(Instance{
      app_id: String::from("xxx"),
      ip_address: String::from("yyy"),
      port: 8080
    }));
  }

  #[test]
  fn remove_instance_test() {
    let raw_json = r#"{"type": "REMOVE_INSTANCE", "data": {"app_id": "xxx", "ip_address": "yyy", "port": 8080}}"#;
    let command: Order = serde_json::from_str(raw_json).expect("could not parse json");
    println!("{:?}", command);
    assert!(command == Order::RemoveInstance(Instance{
      app_id: String::from("xxx"),
      ip_address: String::from("yyy"),
      port: 8080
    }));
  }

  #[test]
  fn http_front_crash_test() {
    let raw_json = r#"{"type": "ADD_HTTP_FRONT", "data": {"app_id": "aa", "hostname": "cltdl.fr", "path_begin": ""}}"#;
    let command: Order = serde_json::from_str(raw_json).expect("could not parse json");
    println!("{:?}", command);
    assert!(command == Order::AddHttpFront(HttpFront{
      app_id: String::from("aa"),
      hostname: String::from("cltdl.fr"),
      path_begin: String::from(""),
    }));
  }

  #[test]
  fn http_front_crash_test2() {
    let raw_json = r#"{"app_id": "aa", "hostname": "cltdl.fr", "path_begin": ""}"#;
    let front: HttpFront = serde_json::from_str(raw_json).expect("could not parse json");
    println!("{:?}",front);
    assert!(front == HttpFront{
      app_id: String::from("aa"),
      hostname: String::from("cltdl.fr"),
      path_begin: String::from(""),
    });
  }
}
