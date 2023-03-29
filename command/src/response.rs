use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap},
    default::Default,
    fmt,
    net::SocketAddr,
};

use crate::{
    certificate::{CertificateSummary, TlsVersion},
    request::{
        default_sticky_name, is_false, AddBackend, Cluster, LoadBalancingParams,
        RequestHttpFrontend, RequestTcpFrontend, PROTOCOL_VERSION,
    },
    state::{ClusterId, ConfigState},
};

/// Responses of the main process to the CLI (or other client)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub id: String,
    pub version: u8,
    pub status: ResponseStatus,
    pub message: String,
    pub content: Option<ResponseContent>,
}

impl Response {
    pub fn new(
        // id: String,
        status: ResponseStatus,
        message: String,
        content: Option<ResponseContent>,
    ) -> Response {
        Response {
            version: PROTOCOL_VERSION,
            id: "generic-response-id-to-be-removed".to_string(),
            status,
            message,
            content,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ResponseStatus {
    Ok,
    Processing,
    Failure,
}

/// details of a response sent by the main process to the client
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ResponseContent {
    /// a list of workers, with ids, pids, statuses
    Workers(Vec<WorkerInfo>),
    /// aggregated metrics of main process and workers
    Metrics(AggregatedMetrics),
    /// worker responses to a same query: worker_id -> response_content
    WorkerResponses(BTreeMap<String, ResponseContent>),
    /// the state of Sōzu: frontends, backends, listeners, etc.
    State(Box<ConfigState>),
    /// a proxy event
    Event(Event),
    /// a filtered list of frontend
    FrontendList(ListedFrontends),
    // this is new
    Status(Vec<WorkerInfo>),
    /// all listeners
    ListenersList(ListenersList),

    /// contains proxy & cluster metrics
    WorkerMetrics(WorkerMetrics),

    /// Lists of metrics that are available
    AvailableMetrics(AvailableMetrics),

    Clusters(Vec<ClusterInformation>),
    /// cluster id -> hash of cluster information
    ClustersHashes(BTreeMap<String, u64>),

    /// a list of certificates for each socket address
    Certificates(HashMap<SocketAddr, Vec<CertificateSummary>>),

    /// returns the certificate matching a request by fingerprint,
    /// and the list of domain names associated
    CertificateByFingerprint(Option<(String, Vec<String>)>),
}

#[derive(Default, Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClusterInformation {
    pub configuration: Option<Cluster>,
    pub http_frontends: Vec<HttpFrontend>,
    pub https_frontends: Vec<HttpFrontend>,
    pub tcp_frontends: Vec<TcpFrontend>,
    pub backends: Vec<Backend>,
}

/// lists of available metrics in a worker, or in the main process (in which case there are no cluster metrics)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailableMetrics {
    pub proxy_metrics: Vec<String>,
    pub cluster_metrics: Vec<String>,
}

#[derive(Debug, Clone, PartialOrd, Ord, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HttpFrontend {
    /// Send a 401, DENY, if cluster_id is None
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

impl Into<RequestHttpFrontend> for HttpFrontend {
    fn into(self) -> RequestHttpFrontend {
        RequestHttpFrontend {
            cluster_id: self.cluster_id,
            address: self.address.to_string(),
            hostname: self.hostname,
            path: self.path,
            method: self.method,
            position: self.position,
            tags: self.tags,
        }
    }
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
// #[serde(rename_all = "SCREAMING_SNAKE_CASE")]
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

pub fn is_default_path_rule(p: &PathRule) -> bool {
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
pub struct TcpFrontend {
    pub cluster_id: String,
    pub address: SocketAddr,
    pub tags: Option<BTreeMap<String, String>>,
}

impl Into<RequestTcpFrontend> for TcpFrontend {
    fn into(self) -> RequestTcpFrontend {
        RequestTcpFrontend {
            cluster_id: self.cluster_id,
            address: self.address.to_string(),
            tags: self.tags,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ListedFrontends {
    pub http_frontends: Vec<HttpFrontend>,
    pub https_frontends: Vec<HttpFrontend>,
    pub tcp_frontends: Vec<TcpFrontend>,
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

impl Backend {
    pub fn to_add_backend(self) -> AddBackend {
        AddBackend {
            cluster_id: self.cluster_id,
            address: self.address.to_string(),
            sticky_id: self.sticky_id,
            backend_id: self.backend_id,
            load_balancing_parameters: self.load_balancing_parameters,
            backup: self.backup,
        }
    }
}

/// All listeners, listed for the CLI.
/// the bool indicates if it is active or not
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ListenersList {
    /// address -> (listener_config, activated)
    pub http_listeners: HashMap<String, (HttpListenerConfig, bool)>,
    pub https_listeners: HashMap<String, (HttpsListenerConfig, bool)>,
    pub tcp_listeners: HashMap<String, TcpListenerConfig>,
}

/// details of an HTTP listener, sent by the main process to the worker
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HttpListenerConfig {
    pub address: String,
    pub public_address: Option<String>,
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
}

/// details of an HTTPS listener, sent by the main process to the worker
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HttpsListenerConfig {
    pub address: String,
    pub public_address: Option<String>,
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
}

/// details of an TCP listener, sent by the main process to the worker
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TcpListenerConfig {
    pub address: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_address: Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "is_false")]
    pub expect_proxy: bool,
    pub front_timeout: u32,
    pub back_timeout: u32,
    pub connect_timeout: u32,
    /// should default to false
    pub active: bool,
}

/// Runstate of a worker
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RunState {
    Running,
    Stopping,
    Stopped,
    NotAnswering,
}

impl fmt::Display for RunState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkerInfo {
    pub id: u32,
    pub pid: i32,
    pub run_state: RunState,
}

/// a backend event that happened on a proxy
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Event {
    BackendDown(String, SocketAddr),
    BackendUp(String, SocketAddr),
    NoAvailableBackends(String),
    /// indicates a backend that was removed from configuration has no lingering connections
    /// so it can be safely stopped
    RemovedBackendHasNoConnections(String, SocketAddr),
}

#[derive(Serialize)]
struct StatePath {
    path: String,
}

pub type MessageId = String;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerResponse {
    pub id: MessageId,
    pub status: ResponseStatus,
    pub message: String,
    pub content: Option<ResponseContent>,
}

impl WorkerResponse {
    pub fn ok<T>(id: T) -> Self
    where
        T: ToString,
    {
        Self {
            id: id.to_string(),
            message: String::new(),
            status: ResponseStatus::Ok,
            content: None,
        }
    }

    pub fn ok_with_content<T>(id: T, content: ResponseContent) -> Self
    where
        T: ToString,
    {
        Self {
            id: id.to_string(),
            status: ResponseStatus::Ok,
            message: String::new(),
            content: Some(content),
        }
    }

    pub fn error<T, U>(id: T, error: U) -> Self
    where
        T: ToString,
        U: ToString,
    {
        Self {
            id: id.to_string(),
            message: error.to_string(),
            status: ResponseStatus::Failure,
            content: None,
        }
    }

    pub fn processing<T>(id: T) -> Self
    where
        T: ToString,
    {
        Self {
            id: id.to_string(),
            message: String::new(),
            status: ResponseStatus::Processing,
            content: None,
        }
    }

    pub fn status<T>(id: T, status: ResponseStatus) -> Self
    where
        T: ToString,
    {
        Self {
            id: id.to_string(),
            message: String::new(),
            status,
            content: None,
        }
    }
}

impl fmt::Display for WorkerResponse {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}-{:?}", self.id, self.status)
    }
}

/// Aggregated metrics of main process & workers, for the CLI
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AggregatedMetrics {
    pub main: BTreeMap<String, FilteredMetrics>,
    pub workers: BTreeMap<String, WorkerMetrics>,
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

fn socketaddr_cmp(a: &SocketAddr, b: &SocketAddr) -> Ordering {
    a.ip().cmp(&b.ip()).then(a.port().cmp(&b.port()))
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! test_message_answer (
        ($name: ident, $filename: expr, $expected_message: expr) => (

          #[test]
          fn $name() {
            let data = include_str!($filename);
            let pretty_print = serde_json::to_string_pretty(&$expected_message)
                .expect("should have serialized");
            assert_eq!(
                &pretty_print,
                data,
                "\nserialized message:\n{}\n\nexpected message:\n{}",
                pretty_print,
                data
            );

            let message: Response = serde_json::from_str(data).unwrap();
            assert_eq!(
                message,
                $expected_message,
                "\ndeserialized message:\n{:#?}\n\nexpected message:\n{:#?}",
                message,
                $expected_message
            );
          }
        )
      );

    test_message_answer!(
        answer_workers_status,
        "../assets/answer_workers_status.json",
        Response {
            id: "ID_TEST".to_string(),
            version: 0,
            status: ResponseStatus::Ok,
            message: String::from(""),
            content: Some(ResponseContent::Workers(vec!(
                WorkerInfo {
                    id: 1,
                    pid: 5678,
                    run_state: RunState::Running,
                },
                WorkerInfo {
                    id: 0,
                    pid: 1234,
                    run_state: RunState::Stopping,
                },
            ))),
        }
    );

    test_message_answer!(
        answer_metrics,
        "../assets/answer_metrics.json",
        Response {
            id: "ID_TEST".to_string(),
            version: 0,
            status: ResponseStatus::Ok,
            message: String::from(""),
            content: Some(ResponseContent::Metrics(AggregatedMetrics {
                main: [
                    (String::from("sozu.gauge"), FilteredMetrics::Gauge(1)),
                    (String::from("sozu.count"), FilteredMetrics::Count(-2)),
                    (String::from("sozu.time"), FilteredMetrics::Time(1234)),
                ]
                .iter()
                .cloned()
                .collect(),
                workers: [(
                    String::from("0"),
                    WorkerMetrics {
                        proxy: Some(
                            [
                                (String::from("sozu.gauge"), FilteredMetrics::Gauge(1)),
                                (String::from("sozu.count"), FilteredMetrics::Count(-2)),
                                (String::from("sozu.time"), FilteredMetrics::Time(1234)),
                            ]
                            .iter()
                            .cloned()
                            .collect()
                        ),
                        clusters: Some(
                            [(
                                String::from("cluster_1"),
                                ClusterMetrics {
                                    cluster: Some(
                                        [(
                                            String::from("request_time"),
                                            FilteredMetrics::Percentiles(Percentiles {
                                                samples: 42,
                                                p_50: 1,
                                                p_90: 2,
                                                p_99: 10,
                                                p_99_9: 12,
                                                p_99_99: 20,
                                                p_99_999: 22,
                                                p_100: 30,
                                            })
                                        )]
                                        .iter()
                                        .cloned()
                                        .collect()
                                    ),
                                    backends: Some(vec![BackendMetrics {
                                        backend_id: String::from("cluster_1-0"),
                                        metrics: [
                                            (String::from("bytes_in"), FilteredMetrics::Count(256)),
                                            (
                                                String::from("bytes_out"),
                                                FilteredMetrics::Count(128)
                                            ),
                                            (
                                                String::from("percentiles"),
                                                FilteredMetrics::Percentiles(Percentiles {
                                                    samples: 42,
                                                    p_50: 1,
                                                    p_90: 2,
                                                    p_99: 10,
                                                    p_99_9: 12,
                                                    p_99_99: 20,
                                                    p_99_999: 22,
                                                    p_100: 30,
                                                })
                                            )
                                        ]
                                        .iter()
                                        .cloned()
                                        .collect()
                                    }]),
                                }
                            )]
                            .iter()
                            .cloned()
                            .collect()
                        )
                    }
                )]
                .iter()
                .cloned()
                .collect()
            }))
        }
    );
}
