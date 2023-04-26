use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap},
    default::Default,
    fmt,
    net::SocketAddr,
};

use crate::{
    proto::command::{
        AddBackend, AggregatedMetrics, AvailableMetrics, CertificateSummary, Cluster,
        ClusterHashes, Event, FilteredTimeSerie, ListenersList, LoadBalancingParams, PathRule,
        PathRuleKind, RequestHttpFrontend, RequestTcpFrontend, ResponseStatus, RulePosition,
        RunState, WorkerInfos, WorkerMetrics,
    },
    request::PROTOCOL_VERSION,
    state::ClusterId,
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

/// details of a response sent by the main process to the client
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ResponseContent {
    /// a list of workers, with ids, pids, statuses
    Workers(WorkerInfos),
    /// aggregated metrics of main process and workers
    Metrics(AggregatedMetrics),
    /// worker responses to a same query: worker_id -> response_content
    WorkerResponses(BTreeMap<String, ResponseContent>),
    /// a proxy event
    Event(Event),
    /// a filtered list of frontend
    FrontendList(ListedFrontends),
    /// all listeners
    ListenersList(ListenersList),

    /// contains proxy & cluster metrics
    WorkerMetrics(WorkerMetrics),

    /// Lists of metrics that are available
    AvailableMetrics(AvailableMetrics),

    Clusters(Vec<ClusterInformation>),
    /// cluster id -> hash of cluster information
    ClustersHashes(ClusterHashes),

    /// a list of certificates for each socket address
    Certificates(HashMap<SocketAddr, Vec<CertificateSummary>>),

    /// returns the certificate matching a request by fingerprint,
    /// and the list of domain names associated
    CertificateByFingerprint(Option<(String, Vec<String>)>),
}

// TODO: the types HttpFrontend, TcpFrontend and Backend are not present,
// and not meant to be present in proto::command. Find a fix, like using the type HttpRequestFrontend
#[derive(Default, Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClusterInformation {
    pub configuration: Option<Cluster>,
    pub http_frontends: Vec<HttpFrontend>,
    pub https_frontends: Vec<HttpFrontend>,
    pub tcp_frontends: Vec<TcpFrontend>,
    pub backends: Vec<Backend>,
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
        let tags = match self.tags {
            Some(tags) => tags,
            None => BTreeMap::new(),
        };
        RequestHttpFrontend {
            cluster_id: self.cluster_id,
            address: self.address.to_string(),
            hostname: self.hostname,
            path: self.path,
            method: self.method,
            position: self.position.into(),
            tags,
        }
    }
}

impl PathRule {
    pub fn prefix<S>(value: S) -> Self
    where
        S: ToString,
    {
        Self {
            kind: PathRuleKind::Prefix.into(),
            value: value.to_string(),
        }
    }

    pub fn regex<S>(value: S) -> Self
    where
        S: ToString,
    {
        Self {
            kind: PathRuleKind::Regex.into(),
            value: value.to_string(),
        }
    }

    pub fn equals<S>(value: S) -> Self
    where
        S: ToString,
    {
        Self {
            kind: PathRuleKind::Equals.into(),
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
                kind: PathRuleKind::Prefix as i32,
                value: prefix,
            },
            (None, Some(regex), _) => PathRule {
                kind: PathRuleKind::Regex as i32,
                value: regex,
            },
            (None, None, Some(equals)) => PathRule {
                kind: PathRuleKind::Equals as i32,
                value: equals,
            },
            _ => PathRule::default(),
        }
    }
}

pub fn is_default_path_rule(p: &PathRule) -> bool {
    PathRuleKind::from_i32(p.kind) == Some(PathRuleKind::Prefix) && p.value.is_empty()
}

impl std::fmt::Display for PathRule {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match PathRuleKind::from_i32(self.kind) {
            Some(PathRuleKind::Prefix) => write!(f, "prefix '{}'", self.value),
            Some(PathRuleKind::Regex) => write!(f, "regexp '{}'", self.value),
            Some(PathRuleKind::Equals) => write!(f, "equals '{}'", self.value),
            None => write!(f, ""),
        }
    }
}

#[derive(Debug, Clone, PartialOrd, Ord, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TcpFrontend {
    pub cluster_id: String,
    pub address: SocketAddr,
    // TODO: remove the Option here, the map may as well be empty
    pub tags: Option<BTreeMap<String, String>>,
}

impl Into<RequestTcpFrontend> for TcpFrontend {
    fn into(self) -> RequestTcpFrontend {
        RequestTcpFrontend {
            cluster_id: self.cluster_id,
            address: self.address.to_string(),
            tags: self.tags.unwrap_or(BTreeMap::new()),
        }
    }
}

// TODO: should contain HttpFrontendConfig and TcpFrontendConfig, or types written in protobuf
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

impl fmt::Display for RunState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
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

impl fmt::Display for FilteredTimeSerie {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "FilteredTimeSerie {{\nlast_second: {},\nlast_minute:\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\nlast_hour:\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n}}",
            self.last_second,
            &self.last_minute[0..10], &self.last_minute[10..20], &self.last_minute[20..30], &self.last_minute[30..40], &self.last_minute[40..50], &self.last_minute[50..60],
            &self.last_hour[0..10], &self.last_hour[10..20], &self.last_hour[20..30], &self.last_hour[30..40], &self.last_hour[40..50], &self.last_hour[50..60])
    }
}

fn socketaddr_cmp(a: &SocketAddr, b: &SocketAddr) -> Ordering {
    a.ip().cmp(&b.ip()).then(a.port().cmp(&b.port()))
}

#[cfg(test)]
mod tests {
    use crate::proto::command::{
        filtered_metrics, AggregatedMetrics, BackendMetrics, ClusterMetrics, FilteredMetrics,
        Percentiles, WorkerInfo, WorkerMetrics,
    };

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
            content: Some(ResponseContent::Workers(WorkerInfos {
                vec: vec!(
                    WorkerInfo {
                        id: 1,
                        pid: 5678,
                        run_state: RunState::Running as i32,
                    },
                    WorkerInfo {
                        id: 0,
                        pid: 1234,
                        run_state: RunState::Stopping as i32,
                    },
                )
            })),
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
                    (
                        String::from("sozu.gauge"),
                        FilteredMetrics {
                            inner: Some(filtered_metrics::Inner::Gauge(1))
                        }
                    ),
                    (
                        String::from("sozu.count"),
                        FilteredMetrics {
                            inner: Some(filtered_metrics::Inner::Count(-2))
                        }
                    ),
                    (
                        String::from("sozu.time"),
                        FilteredMetrics {
                            inner: Some(filtered_metrics::Inner::Time(1234))
                        }
                    ),
                ]
                .iter()
                .cloned()
                .collect(),
                workers: [(
                    String::from("0"),
                    WorkerMetrics {
                        proxy: [
                            (
                                String::from("sozu.gauge"),
                                FilteredMetrics {
                                    inner: Some(filtered_metrics::Inner::Gauge(1))
                                }
                            ),
                            (
                                String::from("sozu.count"),
                                FilteredMetrics {
                                    inner: Some(filtered_metrics::Inner::Count(-2))
                                }
                            ),
                            (
                                String::from("sozu.time"),
                                FilteredMetrics {
                                    inner: Some(filtered_metrics::Inner::Time(1234))
                                }
                            ),
                        ]
                        .iter()
                        .cloned()
                        .collect(),
                        clusters: [(
                            String::from("cluster_1"),
                            ClusterMetrics {
                                cluster: [(
                                    String::from("request_time"),
                                    FilteredMetrics {
                                        inner: Some(filtered_metrics::Inner::Percentiles(
                                            Percentiles {
                                                samples: 42,
                                                p_50: 1,
                                                p_90: 2,
                                                p_99: 10,
                                                p_99_9: 12,
                                                p_99_99: 20,
                                                p_99_999: 22,
                                                p_100: 30,
                                            }
                                        ))
                                    }
                                )]
                                .iter()
                                .cloned()
                                .collect(),
                                backends: vec![BackendMetrics {
                                    backend_id: String::from("cluster_1-0"),
                                    metrics: [
                                        (
                                            String::from("bytes_in"),
                                            FilteredMetrics {
                                                inner: Some(filtered_metrics::Inner::Count(256))
                                            }
                                        ),
                                        (
                                            String::from("bytes_out"),
                                            FilteredMetrics {
                                                inner: Some(filtered_metrics::Inner::Count(128))
                                            }
                                        ),
                                        (
                                            String::from("percentiles"),
                                            FilteredMetrics {
                                                inner: Some(filtered_metrics::Inner::Percentiles(
                                                    Percentiles {
                                                        samples: 42,
                                                        p_50: 1,
                                                        p_90: 2,
                                                        p_99: 10,
                                                        p_99_9: 12,
                                                        p_99_99: 20,
                                                        p_99_999: 22,
                                                        p_100: 30,
                                                    }
                                                ))
                                            }
                                        )
                                    ]
                                    .iter()
                                    .cloned()
                                    .collect()
                                }],
                            }
                        )]
                        .iter()
                        .cloned()
                        .collect()
                    }
                )]
                .iter()
                .cloned()
                .collect()
            }))
        }
    );
}
