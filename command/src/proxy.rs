use serde;
use serde::de::{self, Visitor};
use hex::{self,FromHex};
use std::{error,fmt};
use std::cmp::Ordering;
use std::convert::From;
use std::default::Default;
use std::net::SocketAddr;
use std::collections::{HashMap,BTreeMap,HashSet};
use std::str::FromStr;

use config::{ProxyProtocolConfig, LoadBalancingAlgorithms};

pub type MessageId = String;

#[derive(Debug,Clone,PartialEq,Eq, Serialize, Deserialize)]
pub struct ProxyResponse {
  pub id:     MessageId,
  pub status: ProxyResponseStatus,
  pub data:   Option<ProxyResponseData>
}

impl fmt::Display for ProxyResponse {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    write!(f, "{}-{:?}", self.id, self.status)
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub enum ProxyResponseStatus {
  Ok,
  Processing,
  Error(String),
}

#[derive(Debug,Clone,PartialEq,Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ProxyResponseData {
  //placeholder for now
  Metrics(MetricsData),
  Query(QueryAnswer),
  Event(ProxyEvent),
}

#[derive(Debug,Clone,PartialEq,Eq, Serialize, Deserialize)]
pub struct AggregatedMetricsData {
  pub master: BTreeMap<String, FilteredData>,
  pub workers: BTreeMap<String, MetricsData>,
}

#[derive(Debug,Clone,PartialEq,Eq, Serialize, Deserialize)]
pub struct MetricsData {
  pub proxy:    BTreeMap<String, FilteredData>,
  pub clusters: BTreeMap<String, AppMetricsData>,
}

#[derive(Debug,Clone,PartialEq,Eq, Serialize, Deserialize)]
pub struct AppMetricsData {
  pub data: BTreeMap<String, FilteredData>,
  pub backends: BTreeMap<String, BTreeMap<String, FilteredData>>,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FilteredData {
  Gauge(usize),
  Count(i64),
  Time(usize),
  Percentiles(Percentiles),
  TimeSerie(FilteredTimeSerie),
}

#[derive(Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct FilteredTimeSerie {
  pub last_second: u32,
  pub last_minute: Vec<u32>,
  pub last_hour:   Vec<u32>,
}

impl fmt::Debug for FilteredTimeSerie {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    write!(f, "FilteredTimeSerie {{\nlast_second: {},\nlast_minute:\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\nlast_hour:\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n}}",
      self.last_second,
    &self.last_minute[0..10], &self.last_minute[10..20], &self.last_minute[20..30], &self.last_minute[30..40], &self.last_minute[40..50], &self.last_minute[50..60],
    &self.last_hour[0..10], &self.last_hour[10..20], &self.last_hour[20..30], &self.last_hour[30..40], &self.last_hour[40..50], &self.last_hour[50..60])
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct Percentiles {
  pub samples:  u64,
  pub p_50:     u64,
  pub p_90:     u64,
  pub p_99:     u64,
  pub p_99_9:   u64,
  pub p_99_99:  u64,
  pub p_99_999: u64,
  pub p_100:    u64,
}

#[derive(Debug,Clone,PartialEq,Eq, Serialize, Deserialize)]
pub struct BackendMetricsData {
  pub bytes_in:  usize,
  pub bytes_out: usize,
  pub percentiles: Percentiles,
}

#[derive(Debug,Clone,PartialEq,Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ProxyEvent {
  BackendDown(String, SocketAddr),
  NoAvailableBackends(String),
}

#[derive(Debug,Clone,Serialize,Deserialize)]
pub struct ProxyRequest {
  pub id:    MessageId,
  pub order: ProxyRequestData,
}

impl fmt::Display for ProxyRequest {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    write!(f, "{}-{:?}", self.id, self.order)
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ProxyRequestData {
    AddCluster(Cluster),
    RemoveCluster { cluster_id: String },

    AddHttpFrontend(HttpFrontend),
    RemoveHttpFrontend(HttpFrontend),

    AddHttpsFrontend(HttpFrontend),
    RemoveHttpsFrontend(HttpFrontend),

    AddCertificate(AddCertificate),
    ReplaceCertificate(ReplaceCertificate),
    RemoveCertificate(RemoveCertificate),

    AddTcpFrontend(TcpFrontend),
    RemoveTcpFrontend(TcpFrontend),

    AddBackend(Backend),
    RemoveBackend(RemoveBackend),

    AddHttpListener(HttpListener),
    AddHttpsListener(HttpsListener),
    AddTcpListener(TcpListener),

    RemoveListener(RemoveListener),

    ActivateListener(ActivateListener),
    DeactivateListener(DeactivateListener),

    Query(Query),

    SoftStop,
    HardStop,

    Status,
    Metrics,
    Logging(String),

    ReturnListenSockets,
}


//FIXME: make fixed size depending on hash algorithm
#[derive(Clone,PartialEq,Eq,Hash,PartialOrd,Ord)]
pub struct CertificateFingerprint(pub Vec<u8>);

impl fmt::Debug for CertificateFingerprint {
  fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
    write!(f, "CertificateFingerprint({})", hex::encode(&self.0))
  }
}

impl fmt::Display for CertificateFingerprint {
  fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
    write!(f, "{}", hex::encode(&self.0))
  }
}

impl serde::Serialize for CertificateFingerprint {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
      where S: serde::Serializer,
  {
    serializer.serialize_str(&hex::encode(&self.0))
  }
}

struct CertificateFingerprintVisitor;

impl<'de> Visitor<'de> for CertificateFingerprintVisitor {
  type Value = CertificateFingerprint;

  fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
    formatter.write_str("the certificate fingerprint must be in hexadecimal format")
  }

  fn visit_str<E>(self, value: &str) -> Result<CertificateFingerprint, E>
    where E: de::Error
  {
    FromHex::from_hex(value)
      .map_err(|e| E::custom(format!("could not deserialize hex: {:?}", e)))
      .map(CertificateFingerprint)
  }
}

impl<'de> serde::Deserialize<'de> for CertificateFingerprint {
  fn deserialize<D>(deserializer: D) -> Result<CertificateFingerprint, D::Error>
        where D: serde::de::Deserializer<'de> {
    deserializer.deserialize_str(CertificateFingerprintVisitor{})
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct Cluster {
    pub cluster_id:        String,
    pub sticky_session:    bool,
    pub https_redirect:    bool,
    #[serde(default)]
    pub proxy_protocol:    Option<ProxyProtocolConfig>,
    #[serde(rename = "load_balancing_policy")]
    pub load_balancing_policy: LoadBalancingAlgorithms,
    pub answer_503:        Option<String>,
}

fn socketaddr_cmp(a: &SocketAddr, b: &SocketAddr) -> Ordering {
  a.ip().cmp(&b.ip()).then(a.port().cmp(&b.port()))
}

#[derive(Debug,Clone,Copy,PartialEq,Eq,Hash,PartialOrd,Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RulePosition {
  Pre,
  Post,
  Tree,
}

impl Default for RulePosition {
  fn default() -> Self {
    RulePosition::Tree
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,PartialOrd,Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PathRule {
  Prefix(String),
  Regex(String),
}

impl std::fmt::Display for PathRule {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    match self {
      PathRule::Prefix(s) => write!(f, "prefix '{}'", s),
      PathRule::Regex(r) => write!(f, "regexp '{}'", r.as_str()),
    }
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Route {
  // send a 401 default answer
  Deny,
  ClusterId(String),
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct HttpFrontend {
    pub route:      Route,
    pub address:    SocketAddr,
    pub hostname:   String,
    pub path:       PathRule,
    #[serde(default)]
    pub position:   RulePosition,
}

impl Ord for HttpFrontend {
  fn cmp(&self, o: &HttpFrontend) -> Ordering {
    self.route.cmp(&o.route)
      .then(self.hostname.cmp(&o.hostname))
      .then(self.path.cmp(&o.path))
      .then(socketaddr_cmp(&self.address, &o.address))
      .then(self.position.cmp(&o.position))
  }
}

impl PartialOrd for HttpFrontend {
  fn partial_cmp(&self, other: &HttpFrontend) -> Option<Ordering> {
    Some(self.cmp(other))
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct CertificateAndKey {
    pub certificate:       String,
    pub certificate_chain: Vec<String>,
    pub key:               String,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct AddCertificate {
    pub address:     SocketAddr,
    pub certificate: CertificateAndKey,
    #[serde(default)]
    #[serde(skip_serializing_if="Vec::is_empty")]
    pub names: Vec<String>,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct RemoveCertificate {
    pub address:     SocketAddr,
    pub fingerprint: CertificateFingerprint,
    #[serde(default)]
    #[serde(skip_serializing_if="Vec::is_empty")]
    pub names: Vec<String>,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct ReplaceCertificate {
    pub address:         SocketAddr,
    pub new_certificate: CertificateAndKey,
    pub old_fingerprint: CertificateFingerprint,
    #[serde(default)]
    #[serde(skip_serializing_if="Vec::is_empty")]
    pub old_names: Vec<String>,
    #[serde(default)]
    #[serde(skip_serializing_if="Vec::is_empty")]
    pub new_names: Vec<String>,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct TcpFrontend {
    pub cluster_id:  String,
    pub address: SocketAddr,
}

impl Ord for TcpFrontend {
  fn cmp(&self, o: &TcpFrontend) -> Ordering {
    self.cluster_id.cmp(&o.cluster_id)
      .then(socketaddr_cmp(&self.address, &o.address))
  }
}

impl PartialOrd for TcpFrontend {
  fn partial_cmp(&self, other: &TcpFrontend) -> Option<Ordering> {
    Some(self.cmp(other))
  }
}


#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct Backend {
    pub cluster_id:     String,
    pub backend_id: String,
    pub address:    SocketAddr,
    #[serde(default)]
    pub sticky_id:  Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if="Option::is_none")]
    pub load_balancing_parameters: Option<LoadBalancingParams>,
    #[serde(default)]
    pub backup:     Option<bool>,
}

impl Ord for Backend {
  fn cmp(&self, o: &Backend) -> Ordering {
    self.cluster_id.cmp(&o.cluster_id)
      .then(self.backend_id.cmp(&o.backend_id))
      .then(self.sticky_id.cmp(&o.sticky_id))
      .then(self.load_balancing_parameters.cmp(&o.load_balancing_parameters))
      .then(self.backup.cmp(&o.backup))
      .then(socketaddr_cmp(&self.address, &o.address))
  }
}

impl PartialOrd for Backend {
  fn partial_cmp(&self, other: &Backend) -> Option<Ordering> {
    Some(self.cmp(other))
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct RemoveBackend {
    pub cluster_id:     String,
    pub backend_id: String,
    pub address:    SocketAddr,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,PartialOrd,Ord, Serialize, Deserialize)]
pub struct LoadBalancingParams {
    pub weight: u8,
}

impl Default for LoadBalancingParams {
  fn default() -> Self {
    Self {
      weight: 0,
    }
  }
}

pub fn default_sticky_name() -> String {
  String::from("SOZUBALANCEID")
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ListenerType {
  HTTP,
  HTTPS,
  TCP,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct RemoveListener {
  pub address: SocketAddr,
  pub proxy:   ListenerType,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct ActivateListener {
  pub address:  SocketAddr,
  pub proxy:    ListenerType,
  pub from_scm: bool,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct DeactivateListener {
  pub address: SocketAddr,
  pub proxy:   ListenerType,
  pub to_scm:  bool,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct HttpListener {
    pub address:        SocketAddr,
    pub public_address: Option<SocketAddr>,
    pub answer_404:     String,
    pub answer_503:     String,
    #[serde(default)]
    pub expect_proxy:   bool,
    #[serde(default = "default_sticky_name")]
    pub sticky_name:    String,
}

impl Default for HttpListener {
  fn default() -> HttpListener {
    HttpListener {
      address:           "127.0.0.1:8080".parse().expect("could not parse address"),
      public_address:  None,
      answer_404:      String::from("HTTP/1.1 404 Not Found\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"),
      answer_503:      String::from("HTTP/1.1 503 your application is in deployment\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"),
      expect_proxy:    false,
      sticky_name:     String::from("SOZUBALANCEID"),
    }
  }
}

#[derive(Debug,Copy,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsProvider {
  Openssl,
  Rustls,
}

impl Default for TlsProvider {
    fn default() -> TlsProvider { TlsProvider::Rustls }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub enum TlsVersion {
  SSLv2,
  SSLv3,
  #[serde(rename = "TLSv1")]
  TLSv1_0,
  #[serde(rename = "TLSv1.1")]
  TLSv1_1,
  #[serde(rename = "TLSv1.2")]
  TLSv1_2,
  #[serde(rename = "TLSv1.3")]
  TLSv1_3,
}

impl FromStr for TlsVersion {
  type Err = ParseErrorTlsVersion;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    match s {
      "SSLv2"   => Ok(TlsVersion::SSLv2),
      "SSLv3"   => Ok(TlsVersion::SSLv3),
      "TLSv1"   => Ok(TlsVersion::TLSv1_0),
      "TLSv1.1" => Ok(TlsVersion::TLSv1_1),
      "TLSv1.2" => Ok(TlsVersion::TLSv1_2),
      "TLSv1.3" => Ok(TlsVersion::TLSv1_3),
      _ => Err(ParseErrorTlsVersion{})
    }
  }
}

#[derive(Debug)]
pub struct ParseErrorTlsVersion;

impl fmt::Display for ParseErrorTlsVersion {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    write!(f, "Cannot find the TLS version")
  }
}

impl error::Error for ParseErrorTlsVersion {
  fn description(&self) -> &str {
    "Cannot find the TLS version"
  }

  fn cause(&self) -> Option<&error::Error> {
    None
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct HttpsListener {
    pub address:            SocketAddr,
    pub public_address:     Option<SocketAddr>,
    pub answer_404:         String,
    pub answer_503:         String,
    pub versions:           Vec<TlsVersion>,
    pub cipher_list:        String,
    pub rustls_cipher_list: Vec<String>,
    #[serde(default)]
    pub tls_provider:       TlsProvider,
    #[serde(default)]
    pub expect_proxy:       bool,
    #[serde(default = "default_sticky_name")]
    pub sticky_name:        String,
}

impl Default for HttpsListener {
  fn default() -> HttpsListener {
    HttpsListener {
      address:         "127.0.0.1:8443".parse().expect("could not parse address"),
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
      rustls_cipher_list:  vec!(),
      versions:            vec!(TlsVersion::TLSv1_2),
      tls_provider:        TlsProvider::Rustls,
      expect_proxy:        false,
      sticky_name:     String::from("SOZUBALANCEID"),
    }
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct TcpListener {
  pub address:        SocketAddr,
  pub public_address: Option<SocketAddr>,
  #[serde(default)]
  pub expect_proxy:   bool,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Query {
  Applications(QueryApplicationType),
  Certificates(QueryCertificateType),
  ApplicationsHashes,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum QueryApplicationType {
  ClusterId(String),
  Domain(QueryApplicationDomain)
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
pub struct QueryApplicationDomain {
  pub hostname: String,
  pub path: Option<String>
}

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum QueryCertificateType {
  All,
  Domain(String),
  Fingerprint(Vec<u8>),
}

#[derive(Debug,Clone,PartialEq,Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum QueryAnswer {
  Applications(Vec<QueryAnswerApplication>),
  /// application id, hash of application information
  ApplicationsHashes(BTreeMap<String, u64>),
  Certificates(QueryAnswerCertificate),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct QueryAnswerApplication {
  pub configuration:   Option<Cluster>,
  pub http_frontends:  Vec<HttpFrontend>,
  pub https_frontends: Vec<HttpFrontend>,
  pub tcp_frontends:   Vec<TcpFrontend>,
  pub backends:        Vec<Backend>,
}

impl Default for QueryAnswerApplication {
  fn default() -> QueryAnswerApplication {
    QueryAnswerApplication {
      configuration: None,
      http_frontends: vec!(),
      https_frontends: vec!(),
      tcp_frontends: vec!(),
      backends: vec!()
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueryAnswerCertificate {
  /// returns a list of domain -> fingerprint
  All(HashMap<SocketAddr, BTreeMap<String, Vec<u8>>>),
  /// returns a fingerprint
  Domain(HashMap<SocketAddr, Option<(String, Vec<u8>)>>),
  /// returns the certificate
  Fingerprint(Option<(String, Vec<String>)>),
}

impl ProxyRequestData {
  pub fn get_topics(&self) -> HashSet<Topic> {
    match *self {
      ProxyRequestData::AddCluster(_)          => [Topic::HttpProxyConfig, Topic::HttpsProxyConfig, Topic::TcpProxyConfig].iter().cloned().collect(),
      ProxyRequestData::RemoveCluster{ref cluster_id} => [Topic::HttpProxyConfig, Topic::HttpsProxyConfig, Topic::TcpProxyConfig].iter().cloned().collect(),
      ProxyRequestData::AddHttpFrontend(_)     => [Topic::HttpProxyConfig].iter().cloned().collect(),
      ProxyRequestData::RemoveHttpFrontend(_)  => [Topic::HttpProxyConfig].iter().cloned().collect(),
      ProxyRequestData::AddHttpsFrontend(_)    => [Topic::HttpsProxyConfig].iter().cloned().collect(),
      ProxyRequestData::RemoveHttpsFrontend(_) => [Topic::HttpsProxyConfig].iter().cloned().collect(),
      ProxyRequestData::AddCertificate(_)      => [Topic::HttpsProxyConfig].iter().cloned().collect(),
      ProxyRequestData::ReplaceCertificate(_)  => [Topic::HttpsProxyConfig].iter().cloned().collect(),
      ProxyRequestData::RemoveCertificate(_)   => [Topic::HttpsProxyConfig].iter().cloned().collect(),
      ProxyRequestData::AddTcpFrontend(_)      => [Topic::TcpProxyConfig].iter().cloned().collect(),
      ProxyRequestData::RemoveTcpFrontend(_)   => [Topic::TcpProxyConfig].iter().cloned().collect(),
      ProxyRequestData::AddBackend(_)          => [Topic::HttpProxyConfig, Topic::HttpsProxyConfig, Topic::TcpProxyConfig].iter().cloned().collect(),
      ProxyRequestData::RemoveBackend(_)       => [Topic::HttpProxyConfig, Topic::HttpsProxyConfig, Topic::TcpProxyConfig].iter().cloned().collect(),
      ProxyRequestData::AddHttpListener(_)     => [Topic::HttpProxyConfig].iter().cloned().collect(),
      ProxyRequestData::AddHttpsListener(_)    => [Topic::HttpsProxyConfig].iter().cloned().collect(),
      ProxyRequestData::AddTcpListener(_)      => [Topic::TcpProxyConfig].iter().cloned().collect(),
      ProxyRequestData::RemoveListener(_)      => [Topic::HttpProxyConfig, Topic::HttpsProxyConfig, Topic::TcpProxyConfig].iter().cloned().collect(),
      ProxyRequestData::ActivateListener(_)    => [Topic::HttpProxyConfig, Topic::HttpsProxyConfig, Topic::TcpProxyConfig].iter().cloned().collect(),
      ProxyRequestData::DeactivateListener(_)  => [Topic::HttpProxyConfig, Topic::HttpsProxyConfig, Topic::TcpProxyConfig].iter().cloned().collect(),
      ProxyRequestData::Query(_)               => [Topic::HttpsProxyConfig].iter().cloned().collect(),
      ProxyRequestData::SoftStop               => [Topic::HttpProxyConfig, Topic::HttpsProxyConfig, Topic::TcpProxyConfig].iter().cloned().collect(),
      ProxyRequestData::HardStop               => [Topic::HttpProxyConfig, Topic::HttpsProxyConfig, Topic::TcpProxyConfig].iter().cloned().collect(),
      ProxyRequestData::Status                 => [Topic::HttpProxyConfig, Topic::HttpsProxyConfig, Topic::TcpProxyConfig].iter().cloned().collect(),
      ProxyRequestData::Metrics                => HashSet::new(),
      ProxyRequestData::Logging(_)             => [Topic::HttpsProxyConfig, Topic::HttpProxyConfig, Topic::TcpProxyConfig].iter().cloned().collect(),
      ProxyRequestData::ReturnListenSockets    => HashSet::new(),
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
  fn add_front_test() {
    let raw_json = r#"{"type": "ADD_HTTP_FRONTEND", "data": {"route": { "CLUSTER_ID": "xxx"}, "hostname": "yyy", "path": {"PREFIX": "xxx"}, "address": "127.0.0.1:4242", "sticky_session": false}}"#;
    let command: ProxyRequestData = serde_json::from_str(raw_json).expect("could not parse json");
    println!("{:?}", command);
    assert!(command == ProxyRequestData::AddHttpFrontend(HttpFrontend{
      route: Route::ClusterId(String::from("xxx")),
      hostname: String::from("yyy"),
      path: PathRule::Prefix(String::from("xxx")),
      address: "127.0.0.1:4242".parse().unwrap(),
      position: RulePosition::Tree,
    }));
  }

  #[test]
  fn remove_front_test() {
    let raw_json = r#"{"type": "REMOVE_HTTP_FRONTEND", "data": {"route": {"CLUSTER_ID": "xxx"}, "hostname": "yyy", "path": {"PREFIX": "xxx"}, "address": "127.0.0.1:4242"}}"#;
    let command: ProxyRequestData = serde_json::from_str(raw_json).expect("could not parse json");
    println!("{:?}", command);
    assert!(command == ProxyRequestData::RemoveHttpFrontend(HttpFrontend{
      route: Route::ClusterId(String::from("xxx")),
      hostname: String::from("yyy"),
      path: PathRule::Prefix(String::from("xxx")),
      address: "127.0.0.1:4242".parse().unwrap(),
      position: RulePosition::Tree,
    }));
  }


  #[test]
  fn add_backend_test() {
    let raw_json = r#"{"type": "ADD_BACKEND", "data": {"cluster_id": "xxx", "backend_id": "xxx-0", "address": "0.0.0.0:8080", "load_balancing_parameters": { "weight": 0 }}}"#;
    let command: ProxyRequestData = serde_json::from_str(raw_json).expect("could not parse json");
    println!("{:?}", command);
    assert!(command == ProxyRequestData::AddBackend(Backend{
      cluster_id: String::from("xxx"),
      backend_id: String::from("xxx-0"),
      address: "0.0.0.0:8080".parse().unwrap(),
      sticky_id: None,
      load_balancing_parameters: Some(LoadBalancingParams{ weight: 0 }),
      backup: None,
    }));
  }

  #[test]
  fn remove_backend_test() {
    let raw_json = r#"{"type": "REMOVE_BACKEND", "data": {"cluster_id": "xxx", "backend_id": "xxx-0", "address": "0.0.0.0:8080"}}"#;
    let command: ProxyRequestData = serde_json::from_str(raw_json).expect("could not parse json");
    println!("{:?}", command);
    assert!(command == ProxyRequestData::RemoveBackend(RemoveBackend {
      cluster_id: String::from("xxx"),
      backend_id: String::from("xxx-0"),
      address: "0.0.0.0:8080".parse().unwrap(),
    }));
  }

  #[test]
  fn http_front_crash_test() {
    let raw_json = r#"{"type": "ADD_HTTP_FRONTEND", "data": {"route": {"CLUSTER_ID": "aa"}, "hostname": "cltdl.fr", "path": {"PREFIX": ""}, "address": "127.0.0.1:4242"}}"#;
    let command: ProxyRequestData = serde_json::from_str(raw_json).expect("could not parse json");
    println!("{:?}", command);
    assert!(command == ProxyRequestData::AddHttpFrontend(HttpFrontend{
      route: Route::ClusterId(String::from("aa")),
      hostname: String::from("cltdl.fr"),
      path: PathRule::Prefix(String::from("")),
      address: "127.0.0.1:4242".parse().unwrap(),
      position: RulePosition::Tree,
    }));
  }

  #[test]
  fn http_front_crash_test2() {
    let raw_json = r#"{"route": {"CLUSTER_ID": "aa"}, "hostname": "cltdl.fr", "path": {"PREFIX": ""}, "address": "127.0.0.1:4242" }"#;
    let front: HttpFrontend = serde_json::from_str(raw_json).expect("could not parse json");
    println!("{:?}",front);
    assert!(front == HttpFrontend{
      route: Route::ClusterId(String::from("aa")),
      hostname: String::from("cltdl.fr"),
      path: PathRule::Prefix(String::from("")),
      address: "127.0.0.1:4242".parse().unwrap(),
      position: RulePosition::Tree,
    });
  }
}
