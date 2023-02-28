use std::{
    cmp::Ordering, collections::BTreeMap, convert::From, default::Default, error, fmt,
    net::SocketAddr, str::FromStr,
};

use anyhow::Context;
use hex::{self, FromHex};
use serde::{
    self,
    de::{self, Visitor},
};

use crate::{
    command::MessageId,
    config::{
        ProxyProtocolConfig, DEFAULT_CIPHER_SUITES, DEFAULT_GROUPS_LIST,
        DEFAULT_RUSTLS_CIPHER_LIST, DEFAULT_SIGNATURE_ALGORITHMS,
    },
    state::{ClusterId, RouteKey},
};

/// lists of available metrics in a worker
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailableWorkerMetrics {
    pub proxy_metrics: Vec<String>,
    pub cluster_metrics: Vec<String>,
}

/// All metrics of a worker: proxy and clusters
/// Populated by Options so partial results can be sent
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerMetrics {
    /// Metrics of the worker process, key -> value
    pub proxy: Option<BTreeMap<String, FilteredMetrics>>,
    /// cluster_id -> cluster_metrics
    pub clusters: Option<BTreeMap<String, ClusterMetrics>>,
}

/// the metrics of a given cluster, with several backends
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterMetrics {
    /// metric name -> metric value
    pub cluster: Option<BTreeMap<String, FilteredMetrics>>,
    /// backend_id -> (metric name-> metric value)
    pub backends: Option<Vec<BackendMetrics>>,
}

/// the metrics of a given backend
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendMetrics {
    pub backend_id: String,
    /// metric name -> metric value
    pub metrics: BTreeMap<String, FilteredMetrics>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FilteredMetrics {
    Gauge(usize),
    Count(i64),
    Time(usize),
    Percentiles(Percentiles),
    TimeSerie(FilteredTimeSerie),
}

#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FilteredTimeSerie {
    pub last_second: u32,
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub last_minute: Vec<u32>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub last_hour: Vec<u32>,
}

impl fmt::Debug for FilteredTimeSerie {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "FilteredTimeSerie {{\nlast_second: {},\nlast_minute:\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\nlast_hour:\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n}}",
      self.last_second,
    &self.last_minute[0..10], &self.last_minute[10..20], &self.last_minute[20..30], &self.last_minute[30..40], &self.last_minute[40..50], &self.last_minute[50..60],
    &self.last_hour[0..10], &self.last_hour[10..20], &self.last_hour[20..30], &self.last_hour[30..40], &self.last_hour[40..50], &self.last_hour[50..60])
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Percentiles {
    pub samples: u64,
    pub p_50: u64,
    pub p_90: u64,
    pub p_99: u64,
    pub p_99_9: u64,
    pub p_99_99: u64,
    pub p_99_999: u64,
    pub p_100: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendMetricsData {
    pub bytes_in: usize,
    pub bytes_out: usize,
    pub percentiles: Percentiles,
}

/// A message receivable by a worker
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerRequest {
    pub id: MessageId,
    pub order: WorkerOrder,
}

impl fmt::Display for WorkerRequest {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}-{:?}", self.id, self.order)
    }
}

/// An order sent by the main process to the workers
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WorkerOrder {
    AddCluster(Cluster),
    RemoveCluster {
        cluster_id: String,
    },

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

    AddHttpListener(HttpListenerConfig),
    AddHttpsListener(HttpsListenerConfig),
    AddTcpListener(TcpListenerConfig),

    RemoveListener(RemoveListener),

    ActivateListener(ActivateListener),
    DeactivateListener(DeactivateListener),

    QueryClusterById {
        cluster_id: String,
    },
    QueryClusterByDomain {
        hostname: String,
        path: Option<String>,
    },

    QueryAllCertificates,
    QueryCertificateByDomain(String),
    QueryCertificateByFingerprint(Vec<u8>),

    QueryMetrics(QueryMetricsOptions),

    QueryClustersHashes,
    SoftStop,
    HardStop,

    Status,
    ConfigureMetrics(MetricsConfiguration),
    Logging(String),

    ReturnListenSockets,
}

//FIXME: make fixed size depending on hash algorithm
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fingerprint(pub Vec<u8>);

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(f, "CertificateFingerprint({})", hex::encode(&self.0))
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(f, "{}", hex::encode(&self.0))
    }
}

impl serde::Serialize for Fingerprint {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&hex::encode(&self.0))
    }
}

struct FingerprintVisitor;

impl<'de> Visitor<'de> for FingerprintVisitor {
    type Value = Fingerprint;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("the certificate fingerprint must be in hexadecimal format")
    }

    fn visit_str<E>(self, value: &str) -> Result<Fingerprint, E>
    where
        E: de::Error,
    {
        FromHex::from_hex(value)
            .map_err(|e| E::custom(format!("could not deserialize hex: {e:?}")))
            .map(Fingerprint)
    }
}

impl<'de> serde::Deserialize<'de> for Fingerprint {
    fn deserialize<D>(deserializer: D) -> Result<Fingerprint, D::Error>
    where
        D: serde::de::Deserializer<'de>,
    {
        deserializer.deserialize_str(FingerprintVisitor {})
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Cluster {
    pub cluster_id: ClusterId,
    #[serde(default)]
    #[serde(skip_serializing_if = "is_false")]
    pub sticky_session: bool,
    #[serde(default)]
    #[serde(skip_serializing_if = "is_false")]
    pub https_redirect: bool,
    #[serde(default)]
    #[serde(skip_serializing_if = "is_default")]
    pub proxy_protocol: Option<ProxyProtocolConfig>,
    #[serde(rename = "load_balancing")]
    #[serde(default)]
    #[serde(skip_serializing_if = "is_default")]
    pub load_balancing: LoadBalancingAlgorithms,
    #[serde(default)]
    #[serde(skip_serializing_if = "is_default")]
    pub answer_503: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load_metric: Option<LoadMetric>,
}

fn socketaddr_cmp(a: &SocketAddr, b: &SocketAddr) -> Ordering {
    a.ip().cmp(&b.ip()).then(a.port().cmp(&b.port()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
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

/// A filter for the path of incoming requests
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub struct PathRule {
    /// Either Prefix, Regex or Equals
    pub kind: PathRuleKind,
    pub value: String,
}

/// The kind of filter used for path rules
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PathRuleKind {
    /// filters paths that start with a pattern, typically "/api"
    Prefix,
    /// filters paths that match a regex pattern
    Regex,
    /// filters paths that exactly match a pattern, no more, no less
    Equals,
}

impl PathRule {
    pub fn prefix<S>(value: S) -> Self
    where
        S: ToString,
    {
        Self {
            kind: PathRuleKind::Prefix,
            value: value.to_string(),
        }
    }

    pub fn regex<S>(value: S) -> Self
    where
        S: ToString,
    {
        Self {
            kind: PathRuleKind::Regex,
            value: value.to_string(),
        }
    }

    pub fn equals<S>(value: S) -> Self
    where
        S: ToString,
    {
        Self {
            kind: PathRuleKind::Equals,
            value: value.to_string(),
        }
    }

    pub fn from_cli_options(
        path_prefix: Option<String>,
        path_regex: Option<String>,
        path_equals: Option<String>,
    ) -> Self {
        match (path_prefix, path_regex, path_equals) {
            (Some(prefix), _, _) => PathRule {
                kind: PathRuleKind::Prefix,
                value: prefix,
            },
            (None, Some(regex), _) => PathRule {
                kind: PathRuleKind::Regex,
                value: regex,
            },
            (None, None, Some(equals)) => PathRule {
                kind: PathRuleKind::Equals,
                value: equals,
            },
            _ => PathRule::default(),
        }
    }
}

impl Default for PathRule {
    fn default() -> Self {
        PathRule {
            kind: PathRuleKind::Prefix,
            value: String::new(),
        }
    }
}

fn is_default_path_rule(p: &PathRule) -> bool {
    p.kind == PathRuleKind::Prefix && p.value.is_empty()
}

impl std::fmt::Display for PathRule {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.kind {
            PathRuleKind::Prefix => write!(f, "prefix '{}'", self.value),
            PathRuleKind::Regex => write!(f, "regexp '{}'", self.value),
            PathRuleKind::Equals => write!(f, "equals '{}'", self.value),
        }
    }
}

#[derive(Debug, Clone, PartialOrd, Ord, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HttpFrontend {
    /// The cluster to which the frontend belongs
    /// If None, send a 401 default answer
    pub cluster_id: Option<ClusterId>,
    pub address: SocketAddr,
    pub hostname: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "is_default_path_rule")]
    pub path: PathRule,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default)]
    pub position: RulePosition,
    pub tags: Option<BTreeMap<String, String>>,
}

impl HttpFrontend {
    /// `is_cluster_id` check if the frontend is dedicated to the given cluster_id
    pub fn is_cluster_id(&self, cluster_id: &str) -> bool {
        matches!(&self.cluster_id, Some(id) if id == cluster_id)
    }

    /// `route_key` returns a representation of the frontend as a route key
    pub fn route_key(&self) -> anyhow::Result<String> {
        let route_key = RouteKey::from(self);
        serde_json::to_string(&route_key)
            .with_context(|| "could not serialize route key for this frontend")
    }

    pub fn display_cluster_id(&self) -> String {
        match &self.cluster_id {
            Some(id) => id.clone(),
            None => String::from("deny"),
        }
    }
}

/// Everything necessary to add a certificate
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Certificate {
    pub certificate: String,
    pub certificate_chain: Vec<String>,
    pub key: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub versions: Vec<TlsVersion>,
    /// hostnames linked to the certificate
    #[serde(skip_serializing_if = "Vec::is_empty", default = "Vec::new")]
    pub names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AddCertificate {
    pub address: SocketAddr,
    pub certificate: Certificate,
    /// The `expired_at` override certificate expiration, the value of the field
    /// is a unix timestamp
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expired_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RemoveCertificate {
    pub address: SocketAddr,
    pub fingerprint: Fingerprint,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReplaceCertificate {
    pub address: SocketAddr,
    pub new_certificate: Certificate,
    pub old_fingerprint: Fingerprint,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_expired_at: Option<i64>,
}

#[derive(Debug, Clone, PartialOrd, Ord, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TcpFrontend {
    pub cluster_id: String,
    pub address: SocketAddr,
    pub tags: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Backend {
    pub cluster_id: String,
    pub backend_id: String,
    pub address: SocketAddr,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sticky_id: Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load_balancing_parameters: Option<LoadBalancingParams>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup: Option<bool>,
}

impl Ord for Backend {
    fn cmp(&self, o: &Backend) -> Ordering {
        self.cluster_id
            .cmp(&o.cluster_id)
            .then(self.backend_id.cmp(&o.backend_id))
            .then(self.sticky_id.cmp(&o.sticky_id))
            .then(
                self.load_balancing_parameters
                    .cmp(&o.load_balancing_parameters),
            )
            .then(self.backup.cmp(&o.backup))
            .then(socketaddr_cmp(&self.address, &o.address))
    }
}

impl PartialOrd for Backend {
    fn partial_cmp(&self, other: &Backend) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RemoveBackend {
    pub cluster_id: String,
    pub backend_id: String,
    pub address: SocketAddr,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadBalancingAlgorithms {
    RoundRobin,
    Random,
    LeastLoaded,
    PowerOfTwo,
}

impl Default for LoadBalancingAlgorithms {
    fn default() -> Self {
        LoadBalancingAlgorithms::RoundRobin
    }
}

#[derive(Debug)]
pub struct ParseErrorLoadBalancing;

impl fmt::Display for ParseErrorLoadBalancing {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Cannot find the load balancing policy asked")
    }
}

impl error::Error for ParseErrorLoadBalancing {
    fn description(&self) -> &str {
        "Cannot find the load balancing policy asked"
    }

    fn cause(&self) -> Option<&dyn error::Error> {
        None
    }
}

impl FromStr for LoadBalancingAlgorithms {
    type Err = ParseErrorLoadBalancing;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "round_robin" => Ok(LoadBalancingAlgorithms::RoundRobin),
            "random" => Ok(LoadBalancingAlgorithms::Random),
            "power_of_two" => Ok(LoadBalancingAlgorithms::PowerOfTwo),
            "least_loaded" => Ok(LoadBalancingAlgorithms::LeastLoaded),
            _ => Err(ParseErrorLoadBalancing {}),
        }
    }
}
#[derive(Default, Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LoadBalancingParams {
    pub weight: u8,
}

/// how sozu measures which backend is less loaded
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadMetric {
    /// number of TCP connections
    Connections,
    /// number of active HTTP requests
    Requests,
    /// time to connect to the backend, weighted by the number of active connections (peak EWMA)
    ConnectionTime,
}

pub fn default_sticky_name() -> String {
    String::from("SOZUBALANCEID")
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ListenerType {
    HTTP,
    HTTPS,
    TCP,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RemoveListener {
    pub address: SocketAddr,
    pub proxy: ListenerType,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ActivateListener {
    pub address: SocketAddr,
    pub proxy: ListenerType,
    pub from_scm: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeactivateListener {
    pub address: SocketAddr,
    pub proxy: ListenerType,
    pub to_scm: bool,
}

/// details of an HTTP listener, sent by the main process to the worker
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HttpListenerConfig {
    pub address: SocketAddr,
    pub public_address: Option<SocketAddr>,
    pub answer_404: String,
    pub answer_503: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "is_false")]
    pub expect_proxy: bool,
    /// identifies sticky sessions
    #[serde(default = "default_sticky_name")]
    pub sticky_name: String,
    /// client inactive time
    pub front_timeout: u32,
    /// backend server inactive time
    pub back_timeout: u32,
    /// time to connect to the backend
    pub connect_timeout: u32,
    /// max time to send a complete request
    pub request_timeout: u32,
    pub activated: bool,
}

// TODO: set the default values elsewhere, see #873
impl Default for HttpListenerConfig {
    fn default() -> HttpListenerConfig {
        HttpListenerConfig {
            address: "127.0.0.1:8080".parse().expect("could not parse address"),
            public_address: None,
            answer_404: String::from("HTTP/1.1 404 Not Found\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"),
            answer_503: String::from("HTTP/1.1 503 Service Unavailable\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"),
            expect_proxy: false,
            sticky_name: String::from("SOZUBALANCEID"),
            front_timeout: 60,
            back_timeout: 30,
            connect_timeout: 3,
            request_timeout: 10,
            activated: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
            "SSLv2" => Ok(TlsVersion::SSLv2),
            "SSLv3" => Ok(TlsVersion::SSLv3),
            "TLSv1" => Ok(TlsVersion::TLSv1_0),
            "TLSv1.1" => Ok(TlsVersion::TLSv1_1),
            "TLSv1.2" => Ok(TlsVersion::TLSv1_2),
            "TLSv1.3" => Ok(TlsVersion::TLSv1_3),
            _ => Err(ParseErrorTlsVersion {}),
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

    fn cause(&self) -> Option<&dyn error::Error> {
        None
    }
}

// TODO: set the default values elsewhere, see #873
/// details of an HTTPS listener, sent by the main process to the worker
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HttpsListenerConfig {
    pub address: SocketAddr,
    pub public_address: Option<SocketAddr>,
    pub answer_404: String,
    pub answer_503: String,
    pub versions: Vec<TlsVersion>,
    pub cipher_list: Vec<String>,
    #[serde(default)]
    pub cipher_suites: Vec<String>,
    #[serde(default)]
    pub signature_algorithms: Vec<String>,
    #[serde(default)]
    pub groups_list: Vec<String>,
    #[serde(default)]
    pub expect_proxy: bool,
    #[serde(default = "default_sticky_name")]
    pub sticky_name: String,
    #[serde(default)]
    pub certificate: Option<String>,
    #[serde(default)]
    pub certificate_chain: Vec<String>,
    #[serde(default)]
    pub key: Option<String>,
    pub front_timeout: u32,
    pub back_timeout: u32,
    pub connect_timeout: u32,
    /// max time to send a complete request
    pub request_timeout: u32,
    pub activated: bool,
}

impl Default for HttpsListenerConfig {
    fn default() -> HttpsListenerConfig {
        HttpsListenerConfig {
            address: "127.0.0.1:8443".parse().expect("could not parse address"),
            public_address: None,
            answer_404: String::from("HTTP/1.1 404 Not Found\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"),
            answer_503: String::from("HTTP/1.1 503 Service Unavailable\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"),
            cipher_list: DEFAULT_RUSTLS_CIPHER_LIST.into_iter().map(String::from).collect(),
            cipher_suites: DEFAULT_CIPHER_SUITES.into_iter().map(String::from).collect(),
            signature_algorithms: DEFAULT_SIGNATURE_ALGORITHMS.into_iter().map(String::from).collect(),
            groups_list: DEFAULT_GROUPS_LIST.into_iter().map(String::from).collect(),
            versions: vec!(TlsVersion::TLSv1_2),
            expect_proxy: false,
            sticky_name: String::from("SOZUBALANCEID"),
            certificate: None,
            certificate_chain: vec![],
            key: None,
            front_timeout: 60,
            back_timeout: 30,
            connect_timeout: 3,
            request_timeout: 10,
            activated: false,
        }
    }
}

/// details of an TCP listener, sent by the main process to the worker
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TcpListenerConfig {
    pub address: SocketAddr,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_address: Option<SocketAddr>,
    #[serde(default)]
    #[serde(skip_serializing_if = "is_false")]
    pub expect_proxy: bool,
    pub front_timeout: u32,
    pub back_timeout: u32,
    pub connect_timeout: u32,
    pub activated: bool,
}

impl Default for TcpListenerConfig {
    fn default() -> Self {
        TcpListenerConfig {
            address: "0.0.0.0:1234".parse().expect("could not parse address"),
            public_address: None,
            expect_proxy: false,
            front_timeout: 60,
            back_timeout: 30,
            connect_timeout: 3,
            activated: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MetricsConfiguration {
    Enabled,
    Disabled,
    Clear,
}

/// Options originating from the command line
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
pub struct QueryMetricsOptions {
    pub list: bool,
    pub cluster_ids: Vec<String>,
    pub backend_ids: Vec<String>,
    pub metric_names: Vec<String>,
}

/// all information about one cluster, sent by a worker
#[derive(Default, Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClusterInformation {
    pub configuration: Option<Cluster>,
    pub http_frontends: Vec<HttpFrontend>,
    pub https_frontends: Vec<HttpFrontend>,
    pub tcp_frontends: Vec<TcpFrontend>,
    pub backends: Vec<Backend>,
}

/// TODO: rename me
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertificateWithNames {
    pub certificate: String,
    /// hostnames linked to the certificate
    pub names: Vec<String>, // TODO: check if this corresponds to Common Name
}

/// all certificate summaries for a given address
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertificatesByAddress {
    pub address: SocketAddr,
    pub summaries: Vec<CertificateSummary>,
}

/// domain name and fingerprint of a certificate
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertificateSummary {
    pub domain: String,
    pub fingerprint: Fingerprint,
}

impl WorkerOrder {
    /// determine to which of the three proxies (HTTP, HTTPS, TCP) a request is destined
    pub fn get_destinations(&self) -> ProxyDestinations {
        let mut proxy_destination = ProxyDestinations {
            to_http_proxy: false,
            to_https_proxy: false,
            to_tcp_proxy: false,
        };

        match *self {
            WorkerOrder::AddHttpFrontend(_) | WorkerOrder::RemoveHttpFrontend(_) => {
                proxy_destination.to_http_proxy = true
            }

            WorkerOrder::AddHttpsFrontend(_)
            | WorkerOrder::RemoveHttpsFrontend(_)
            | WorkerOrder::AddCertificate(_)
            | WorkerOrder::ReplaceCertificate(_)
            | WorkerOrder::RemoveCertificate(_)
            | WorkerOrder::QueryClustersHashes
            | WorkerOrder::QueryCertificateByDomain(_)
            | WorkerOrder::QueryAllCertificates
            | WorkerOrder::QueryCertificateByFingerprint(_) => {
                proxy_destination.to_https_proxy = true
            }

            WorkerOrder::AddTcpFrontend(_) | WorkerOrder::RemoveTcpFrontend(_) => {
                proxy_destination.to_tcp_proxy = true
            }

            WorkerOrder::AddCluster(_)
            | WorkerOrder::AddBackend(_)
            | WorkerOrder::RemoveCluster { cluster_id: _ }
            | WorkerOrder::RemoveBackend(_)
            | WorkerOrder::SoftStop
            | WorkerOrder::HardStop
            | WorkerOrder::QueryMetrics(_)
            | WorkerOrder::QueryClusterById { cluster_id: _ }
            | WorkerOrder::QueryClusterByDomain {
                hostname: _,
                path: _,
            }
            | WorkerOrder::Status
            | WorkerOrder::Logging(_) => {
                proxy_destination.to_http_proxy = true;
                proxy_destination.to_https_proxy = true;
                proxy_destination.to_tcp_proxy = true;
            }

            // the Add***Listener and other Listener orders will be handled separately
            // by the notify_proxys function, so we don't give them destinations
            WorkerOrder::AddHttpsListener(_)
            | WorkerOrder::AddHttpListener(_)
            | WorkerOrder::AddTcpListener(_)
            | WorkerOrder::RemoveListener(_)
            | WorkerOrder::ActivateListener(_)
            | WorkerOrder::DeactivateListener(_)
            | WorkerOrder::ConfigureMetrics(_)
            | WorkerOrder::ReturnListenSockets => {}
        }
        proxy_destination
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProxyDestinations {
    pub to_http_proxy: bool,
    pub to_https_proxy: bool,
    pub to_tcp_proxy: bool,
}

/*
fn is_true(b: &bool) -> bool {
    *b
}*/

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_default<T: Default + PartialEq>(t: &T) -> bool {
    t == &T::default()
}

/*
TODO: make sure HttpFrontend.cluster_id is printed as "deny" when None
impl std::fmt::Display for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            None => write!(f, "deny"),
            Some(string) => write!(f, "{string}"),
        }
    }
}
*/

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json;

    #[test]
    fn add_front_test() {
        let raw_json = r#"{"type": "ADD_HTTP_FRONTEND", "data": {"cluster_id": "xxx", "hostname": "yyy", "path": {"KIND": "PREFIX", "VALUE": "xxx"}, "address": "127.0.0.1:4242", "sticky_session": false}}"#;
        let command: WorkerOrder = serde_json::from_str(raw_json).expect("could not parse json");
        println!("{command:?}");
        assert!(
            command
                == WorkerOrder::AddHttpFrontend(HttpFrontend {
                    cluster_id: Some(String::from("xxx")),
                    hostname: String::from("yyy"),
                    path: PathRule::prefix("xxx"),
                    method: None,
                    address: "127.0.0.1:4242".parse().unwrap(),
                    position: RulePosition::Tree,
                    tags: None,
                })
        );
    }

    #[test]
    fn remove_front_test() {
        let raw_json = r#"{"type": "REMOVE_HTTP_FRONTEND", "data": {"cluster_id": "xxx", "hostname": "yyy", "path": {"KIND": "PREFIX", "VALUE": "xxx"}, "address": "127.0.0.1:4242", "tags": { "owner": "John", "id": "some-long-id" }}}"#;
        let command: WorkerOrder = serde_json::from_str(raw_json).expect("could not parse json");
        println!("{command:?}");
        assert!(
            command
                == WorkerOrder::RemoveHttpFrontend(HttpFrontend {
                    cluster_id: Some(String::from("xxx")),
                    hostname: String::from("yyy"),
                    path: PathRule::prefix("xxx"),
                    method: None,
                    address: "127.0.0.1:4242".parse().unwrap(),
                    position: RulePosition::Tree,
                    tags: Some(BTreeMap::from([
                        ("owner".to_owned(), "John".to_owned()),
                        ("id".to_owned(), "some-long-id".to_owned())
                    ])),
                })
        );
    }

    #[test]
    fn add_backend_test() {
        let raw_json = r#"{"type": "ADD_BACKEND", "data": {"cluster_id": "xxx", "backend_id": "xxx-0", "address": "0.0.0.0:8080", "load_balancing_parameters": { "weight": 0 }}}"#;
        let command: WorkerOrder = serde_json::from_str(raw_json).expect("could not parse json");
        println!("{command:?}");
        assert!(
            command
                == WorkerOrder::AddBackend(Backend {
                    cluster_id: String::from("xxx"),
                    backend_id: String::from("xxx-0"),
                    address: "0.0.0.0:8080".parse().unwrap(),
                    sticky_id: None,
                    load_balancing_parameters: Some(LoadBalancingParams { weight: 0 }),
                    backup: None,
                })
        );
    }

    #[test]
    fn remove_backend_test() {
        let raw_json = r#"{"type": "REMOVE_BACKEND", "data": {"cluster_id": "xxx", "backend_id": "xxx-0", "address": "0.0.0.0:8080"}}"#;
        let command: WorkerOrder = serde_json::from_str(raw_json).expect("could not parse json");
        println!("{command:?}");
        assert!(
            command
                == WorkerOrder::RemoveBackend(RemoveBackend {
                    cluster_id: String::from("xxx"),
                    backend_id: String::from("xxx-0"),
                    address: "0.0.0.0:8080".parse().unwrap(),
                })
        );
    }

    #[test]
    fn http_front_crash_test() {
        let raw_json = r#"{"type": "ADD_HTTP_FRONTEND", "data": {"cluster_id": "aa", "hostname": "cltdl.fr", "path": {"KIND": "PREFIX", "VALUE": ""}, "address": "127.0.0.1:4242", "tags": { "owner": "John", "id": "some-long-id" }}}"#;
        let command: WorkerOrder = serde_json::from_str(raw_json).expect("could not parse json");
        println!("{command:?}");
        assert!(
            command
                == WorkerOrder::AddHttpFrontend(HttpFrontend {
                    cluster_id: Some(String::from("aa")),
                    hostname: String::from("cltdl.fr"),
                    path: PathRule::default(),
                    method: None,
                    address: "127.0.0.1:4242".parse().unwrap(),
                    position: RulePosition::Tree,
                    tags: Some(BTreeMap::from([
                        ("owner".to_owned(), "John".to_owned()),
                        ("id".to_owned(), "some-long-id".to_owned())
                    ])),
                })
        );
    }

    #[test]
    fn http_front_crash_test2() {
        let raw_json = r#"{"cluster_id": "aa", "hostname": "cltdl.fr", "path": {"KIND": "PREFIX", "VALUE": ""}, "address": "127.0.0.1:4242" }"#;
        let front: HttpFrontend = serde_json::from_str(raw_json).expect("could not parse json");
        println!("{front:?}");
        assert!(
            front
                == HttpFrontend {
                    cluster_id: Some(String::from("aa")),
                    hostname: String::from("cltdl.fr"),
                    path: PathRule::default(),
                    method: None,
                    address: "127.0.0.1:4242".parse().unwrap(),
                    position: RulePosition::Tree,
                    tags: None,
                }
        );
    }
}
