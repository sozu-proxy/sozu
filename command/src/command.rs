use std::{collections::BTreeMap, fmt, net::SocketAddr};

use crate::{
    state::{ClusterId, ConfigState},
    worker::{
        AvailableWorkerMetrics, CertificateWithNames, CertificatesByAddress, ClusterInformation,
        FilteredMetrics, HttpFrontend, HttpListenerConfig, HttpsListenerConfig, TcpFrontend,
        TcpListenerConfig, WorkerMetrics, WorkerOrder,
    },
};

pub const PROTOCOL_VERSION: u8 = 0;

/// Details of a request sent by the CLI (or other) to the main process
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Order {
    /// an order to forward to workers
    Worker(Box<WorkerOrder>),
    /// save Sōzu's parseable state as a file
    SaveState {
        path: String,
    },
    /// load a state file
    LoadState {
        path: String,
    },
    /// dump the state in JSON
    DumpState,
    /// list the workers and their status
    ListWorkers,
    /// list the frontends, filtered by protocol and/or domain
    ListFrontends(FrontendFilters),
    // list all listeners
    ListListeners,
    /// launche a new worker
    LaunchWorker(String),
    /// upgrade the main process
    UpgradeMain,
    /// upgrade an existing worker
    UpgradeWorker(u32),
    /// subscribe to proxy events
    SubscribeEvents,
    /// reload the configuration from the config file, or a new file
    ReloadConfiguration {
        path: Option<String>,
    },
    /// give status of main process and all workers
    Status,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FrontendFilters {
    pub http: bool,
    pub https: bool,
    pub tcp: bool,
    pub domain: Option<String>,
}

/// Sent to the main process by the CLI (or other) through the unix socket
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub version: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<u32>,
    #[serde(flatten)]
    pub order: Order,
}

impl Request {
    pub fn new(id: String, order: Order, worker_id: Option<u32>) -> Request {
        Request {
            version: PROTOCOL_VERSION,
            id,
            order,
            worker_id,
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

/// details of a response sent by the main process to the client,
/// or by a worker to the main process
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ResponseContent {
    /// a list of workers, with ids, pids, statuses
    WorkerInfos(WorkerInfos),

    /// aggregated metrics of main process and workers
    Metrics(AggregatedMetrics),

    /// list available metrics of main process and workers
    AvailableMetrics(AvailableMetrics), // maybe useless

    /// main and worker responses to a same query: worker_id -> response_content
    QueryResponses(QueryResponses),

    /// the state of Sōzu: frontends, backends, listeners, etc.
    State(Box<ConfigState>),

    /// a proxy event
    Event(Event),

    /// a filtered list of frontend
    FrontendList(ListedFrontends),

    /// all listeners
    ListenersList(ListenersList),

    /// A list of cluster id and their hashes
    ClusterHashes(ClusterHashes),

    /// A list of clusters with their details
    ClusterInformations(ClusterInformations),

    /// One certificate with its names
    CertificateWithNames(CertificateWithNames),

    /// All certificates used by by a worker
    WorkerCertificates(WorkerCertificates),

    /// All metrics of a worker
    WorkerMetrics(WorkerMetrics),

    /// All metrics names available in a worker
    AvailableWorkerMetrics(AvailableWorkerMetrics),

    /// returns certificates that match a QueryCertificateByDomain
    CertificatesByDomain(CertificatesByDomain),
}

/// a list of workers, with ids, pids, statuses
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerInfos {
    pub inner: Vec<WorkerInfo>,
}

/// main and worker responses to a same query: worker_id -> response_content
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryResponses {
    pub inner: BTreeMap<String, ResponseContent>,
}

/// A list of cluster id and their hashes
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterHashes {
    pub inner: Vec<ClusterHash>,
}

/// A list of clusters with their details
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterInformations {
    pub inner: Vec<ClusterInformation>,
}

/// returns certificates that match a QueryCertificateByDomain
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertificatesByDomain {
    pub inner: Vec<CertificatesByAddress>,
}

/// All certificates used by by a worker
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerCertificates {
    pub inner: Vec<CertificatesByAddress>,
}

/// cluster id -> hash of cluster information
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterHash {
    pub cluster_id: ClusterId,
    pub hash: u64,
}

/// Aggregated metrics of main process & workers, for the CLI
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AggregatedMetrics {
    /// metric-name -> metric
    pub main: BTreeMap<String, FilteredMetrics>,
    /// worker_id -> worker_metrics
    pub workers: BTreeMap<String, WorkerMetrics>,
}

/// lists of available metrics in the main process and the workers
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailableMetrics {
    pub main: Vec<String>,
    pub workers: BTreeMap<String, AvailableWorkerMetrics>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ListedFrontends {
    pub http_frontends: Vec<HttpFrontend>,
    pub https_frontends: Vec<HttpFrontend>,
    pub tcp_frontends: Vec<TcpFrontend>,
}

/// All listeners, listed for the CLI.
/// the bool indicates if it is active or not
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ListenersList {
    pub http_listeners: BTreeMap<SocketAddr, HttpListenerConfig>,
    pub https_listeners: BTreeMap<SocketAddr, HttpsListenerConfig>,
    pub tcp_listeners: BTreeMap<SocketAddr, TcpListenerConfig>,
}

/// Responses of the main process to the CLI (or other client)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub id: MessageId,
    pub version: u8,
    /// OK | processing | failure
    pub status: ResponseStatus,
    /// a success or error message
    pub message: Option<String>,
    pub content: Option<ResponseContent>,
}

pub type MessageId = String;

impl Response {
    pub fn new(
        id: String,
        status: ResponseStatus,
        message: String,
        content: Option<ResponseContent>,
    ) -> Response {
        Response {
            version: PROTOCOL_VERSION,
            id,
            status,
            message: Some(message),
            content,
        }
    }

    pub fn ok<T>(id: T) -> Self
    where
        T: ToString,
    {
        Self {
            id: id.to_string(),
            version: PROTOCOL_VERSION,
            status: ResponseStatus::Ok,
            message: None,
            content: None,
        }
    }

    pub fn ok_with_content<T>(id: T, content: ResponseContent) -> Self
    where
        T: ToString,
    {
        Self {
            id: id.to_string(),
            version: PROTOCOL_VERSION,
            status: ResponseStatus::Ok,
            message: None,
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
            version: PROTOCOL_VERSION,
            status: ResponseStatus::Failure,
            message: Some(error.to_string()),
            content: None,
        }
    }

    pub fn processing<T>(id: T) -> Self
    where
        T: ToString,
    {
        Self {
            id: id.to_string(),
            version: PROTOCOL_VERSION,
            status: ResponseStatus::Processing,
            message: None,
            content: None,
        }
    }

    pub fn processing_with_content<T>(id: T, content: ResponseContent) -> Self
    where
        T: ToString,
    {
        Self {
            id: id.to_string(),
            version: PROTOCOL_VERSION,
            status: ResponseStatus::Processing,
            message: None,
            content: Some(content),
        }
    }

    pub fn status<T>(id: T, status: ResponseStatus) -> Self
    where
        T: ToString,
    {
        Self {
            id: id.to_string(),
            version: PROTOCOL_VERSION,
            status,
            message: None,
            content: None,
        }
    }
}

impl fmt::Display for Response {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self.message {
            Some(message) => write!(f, "{}-{:?}: {}", self.id, self.status, message),
            None => write!(f, "{}-{:?}", self.id, self.status),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::certificate::split_certificate_chain;
    use crate::config::ProxyProtocolConfig;
    use crate::worker::{
        AddCertificate, Backend, BackendMetrics, Certificate, Cluster, ClusterMetrics,
        FilteredMetrics, Fingerprint, HttpFrontend, LoadBalancingAlgorithms, LoadBalancingParams,
        PathRule, Percentiles, RemoveBackend, RemoveCertificate, RulePosition, TlsVersion,
        WorkerMetrics, WorkerOrder,
    };
    use hex::FromHex;
    use serde_json;

    #[test]
    fn config_message_test() {
        let raw_json = r#"{ "id": "ID_TEST", "version": 0, "type": "WORKER", "data":{"type": "ADD_HTTP_FRONTEND", "data": { "cluster_id": "xxx", "hostname": "yyy", "path": {"KIND": "PREFIX", "VALUE": "xxx"}, "address": "0.0.0.0:8080"}} }"#;
        let message: Request = serde_json::from_str(raw_json).unwrap();
        println!("{message:?}");
        assert_eq!(
            message.order,
            Order::Worker(Box::new(WorkerOrder::AddHttpFrontend(HttpFrontend {
                cluster_id: Some(String::from("xxx")),
                hostname: String::from("yyy"),
                path: PathRule::prefix("xxx"),
                method: None,
                address: "0.0.0.0:8080".parse().unwrap(),
                position: RulePosition::Tree,
                tags: None,
            })))
        );
    }

    macro_rules! test_message (
    ($name: ident, $filename: expr, $expected_message: expr) => (

      #[test]
      fn $name() {
        let data = include_str!($filename);
        let pretty_print = serde_json::to_string_pretty(&$expected_message).expect("should have serialized");
        assert_eq!(&pretty_print, data, "\nserialized message:\n{}\n\nexpected message:\n{}", pretty_print, data);

        let message: Request = serde_json::from_str(data).unwrap();
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

        let message: Response = serde_json::from_str(data).unwrap();
        assert_eq!(message, $expected_message, "\ndeserialized message:\n{:#?}\n\nexpected message:\n{:#?}", message, $expected_message);

      }

    )
  );

    test_message!(
        add_cluster,
        "../assets/add_cluster.json",
        Request {
            id: "ID_TEST".to_string(),
            version: 0,
            order: Order::Worker(Box::new(WorkerOrder::AddCluster(Cluster {
                cluster_id: String::from("xxx"),
                sticky_session: true,
                https_redirect: true,
                proxy_protocol: Some(ProxyProtocolConfig::ExpectHeader),
                load_balancing: LoadBalancingAlgorithms::RoundRobin,
                load_metric: None,
                answer_503: None,
            }))),
            worker_id: None
        }
    );

    test_message!(
        remove_cluster,
        "../assets/remove_cluster.json",
        Request {
            id: "ID_TEST".to_string(),
            version: 0,
            order: Order::Worker(Box::new(WorkerOrder::RemoveCluster {
                cluster_id: String::from("xxx")
            })),
            worker_id: None
        }
    );

    test_message!(
        add_http_front,
        "../assets/add_http_front.json",
        Request {
            id: "ID_TEST".to_string(),
            version: 0,
            order: Order::Worker(Box::new(WorkerOrder::AddHttpFrontend(HttpFrontend {
                cluster_id: Some(String::from("xxx")),
                hostname: String::from("yyy"),
                path: PathRule::prefix("xxx"),
                method: None,
                address: "0.0.0.0:8080".parse().unwrap(),
                position: RulePosition::Tree,
                tags: None,
            }))),
            worker_id: None
        }
    );

    test_message!(
        remove_http_front,
        "../assets/remove_http_front.json",
        Request {
            id: "ID_TEST".to_string(),
            version: 0,
            order: Order::Worker(Box::new(WorkerOrder::RemoveHttpFrontend(HttpFrontend {
                cluster_id: Some(String::from("xxx")),
                hostname: String::from("yyy"),
                path: PathRule::prefix("xxx"),
                method: None,
                address: "0.0.0.0:8080".parse().unwrap(),
                position: RulePosition::Tree,
                tags: Some(BTreeMap::from([
                    ("owner".to_owned(), "John".to_owned()),
                    (
                        "uuid".to_owned(),
                        "0dd8d7b1-a50a-461a-b1f9-5211a5f45a83".to_owned()
                    )
                ]))
            }))),
            worker_id: None
        }
    );

    test_message!(
        add_https_front,
        "../assets/add_https_front.json",
        Request {
            id: "ID_TEST".to_string(),
            version: 0,
            order: Order::Worker(Box::new(WorkerOrder::AddHttpsFrontend(HttpFrontend {
                cluster_id: Some(String::from("xxx")),
                hostname: String::from("yyy"),
                path: PathRule::prefix("xxx"),
                method: None,
                address: "0.0.0.0:8443".parse().unwrap(),
                position: RulePosition::Tree,
                tags: None,
            }))),
            worker_id: None
        }
    );

    test_message!(
        remove_https_front,
        "../assets/remove_https_front.json",
        Request {
            id: "ID_TEST".to_string(),
            version: 0,
            order: Order::Worker(Box::new(WorkerOrder::RemoveHttpsFrontend(HttpFrontend {
                cluster_id: Some(String::from("xxx")),
                hostname: String::from("yyy"),
                path: PathRule::prefix("xxx"),
                method: None,
                address: "0.0.0.0:8443".parse().unwrap(),
                position: RulePosition::Tree,
                tags: Some(BTreeMap::from([
                    ("owner".to_owned(), "John".to_owned()),
                    (
                        "uuid".to_owned(),
                        "0dd8d7b1-a50a-461a-b1f9-5211a5f45a83".to_owned()
                    )
                ]))
            }))),
            worker_id: None
        }
    );

    const KEY: &str = include_str!("../../lib/assets/key.pem");
    const CERTIFICATE: &str = include_str!("../../lib/assets/certificate.pem");
    const CHAIN: &str = include_str!("../../lib/assets/certificate_chain.pem");

    test_message!(
        add_certificate,
        "../assets/add_certificate.json",
        Request {
            id: "ID_TEST".to_string(),
            version: 0,
            order: Order::Worker(Box::new(WorkerOrder::AddCertificate(AddCertificate {
                address: "0.0.0.0:443".parse().unwrap(),
                certificate: Certificate {
                    certificate: String::from(CERTIFICATE),
                    certificate_chain: split_certificate_chain(String::from(CHAIN)),
                    key: String::from(KEY),
                    versions: vec![TlsVersion::TLSv1_2, TlsVersion::TLSv1_3],
                    names: vec![],
                },
                expired_at: None,
            }))),
            worker_id: None
        }
    );

    test_message!(
        remove_certificate,
        "../assets/remove_certificate.json",
        Request {
            id: "ID_TEST".to_string(),
            version: 0,
            order: Order::Worker(Box::new(WorkerOrder::RemoveCertificate(
                RemoveCertificate {
                    address: "0.0.0.0:443".parse().unwrap(),
                    fingerprint: Fingerprint {
                        inner: FromHex::from_hex(
                            "ab2618b674e15243fd02a5618c66509e4840ba60e7d64cebec84cdbfeceee0c5"
                        )
                        .unwrap()
                    },
                }
            ))),
            worker_id: None
        }
    );

    test_message!(
        add_backend,
        "../assets/add_backend.json",
        Request {
            id: "ID_TEST".to_string(),
            version: 0,
            order: Order::Worker(Box::new(WorkerOrder::AddBackend(Backend {
                cluster_id: String::from("xxx"),
                backend_id: String::from("xxx-0"),
                address: "127.0.0.1:8080".parse().unwrap(),
                load_balancing_parameters: Some(LoadBalancingParams { weight: 0 }),
                sticky_id: Some(String::from("xxx-0")),
                backup: Some(false),
            }))),
            worker_id: None
        }
    );

    test_message!(
        remove_backend,
        "../assets/remove_backend.json",
        Request {
            id: "ID_TEST".to_string(),
            version: 0,
            order: Order::Worker(Box::new(WorkerOrder::RemoveBackend(RemoveBackend {
                cluster_id: String::from("xxx"),
                backend_id: String::from("xxx-0"),
                address: "127.0.0.1:8080".parse().unwrap(),
            }))),
            worker_id: None
        }
    );

    test_message!(
        soft_stop,
        "../assets/soft_stop.json",
        Request {
            id: "ID_TEST".to_string(),
            version: 0,
            order: Order::Worker(Box::new(WorkerOrder::SoftStop)),
            worker_id: Some(0),
        }
    );

    test_message!(
        hard_stop,
        "../assets/hard_stop.json",
        Request {
            id: "ID_TEST".to_string(),
            version: 0,
            order: Order::Worker(Box::new(WorkerOrder::HardStop)),
            worker_id: Some(0),
        }
    );

    test_message!(
        status,
        "../assets/status.json",
        Request {
            id: "ID_TEST".to_string(),
            version: 0,
            order: Order::Worker(Box::new(WorkerOrder::Status)),
            worker_id: Some(0),
        }
    );

    test_message!(
        load_state,
        "../assets/load_state.json",
        Request {
            id: "ID_TEST".to_string(),
            version: 0,
            order: Order::LoadState {
                path: String::from("./config_dump.json")
            },
            worker_id: None
        }
    );

    test_message!(
        save_state,
        "../assets/save_state.json",
        Request {
            id: "ID_TEST".to_string(),
            version: 0,
            order: Order::SaveState {
                path: String::from("./config_dump.json")
            },
            worker_id: None
        }
    );

    test_message!(
        dump_state,
        "../assets/dump_state.json",
        Request {
            id: "ID_TEST".to_string(),
            version: 0,
            order: Order::DumpState,
            worker_id: None
        }
    );

    test_message!(
        list_workers,
        "../assets/list_workers.json",
        Request {
            id: "ID_TEST".to_string(),
            version: 0,
            order: Order::ListWorkers,
            worker_id: None
        }
    );

    test_message!(
        upgrade_main,
        "../assets/upgrade_main.json",
        Request {
            id: "ID_TEST".to_string(),
            version: 0,
            order: Order::UpgradeMain,
            worker_id: None
        }
    );

    test_message!(
        upgrade_worker,
        "../assets/upgrade_worker.json",
        Request {
            id: "ID_TEST".to_string(),
            version: 0,
            order: Order::UpgradeWorker(0),
            worker_id: None
        }
    );

    test_message_answer!(
        answer_workers_status,
        "../assets/answer_workers_status.json",
        Response {
            id: "ID_TEST".to_string(),
            version: 0,
            status: ResponseStatus::Ok,
            message: Some(String::from("")),
            content: Some(ResponseContent::WorkerInfos(WorkerInfos {
                inner: vec!(
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
            message: Some(String::from("")),
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
                                    backends: Some(
                                        [BackendMetrics {
                                            backend_id: String::from("cluster_1-0"),
                                            metrics: [
                                                (
                                                    String::from("bytes_in"),
                                                    FilteredMetrics::Count(256)
                                                ),
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
                                        }]
                                        .iter()
                                        .cloned()
                                        .collect(),
                                    )
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
