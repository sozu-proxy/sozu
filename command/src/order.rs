use std::{error, fmt, net::SocketAddr, str::FromStr};

use crate::{
    certificate::{CertificateAndKey, CertificateFingerprint},
    config::ProxyProtocolConfig,
    response::{
        Backend, HttpFrontend, HttpListenerConfig, HttpsListenerConfig, MessageId, TcpFrontend,
        TcpListenerConfig,
    },
    state::ClusterId,
};

pub const PROTOCOL_VERSION: u8 = 0;

/// A request sent by the CLI (or other) to the main process
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Order {
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

    QueryCertificates(QueryCertificateType),
    QueryClusters(QueryClusterType),
    QueryClustersHashes,
    QueryMetrics(QueryMetricsOptions),

    SoftStop,
    HardStop,

    ConfigureMetrics(MetricsConfiguration),
    Logging(String),

    ReturnListenSockets,
}

impl Order {
    /// determine to which of the three proxies (HTTP, HTTPS, TCP) a request is destined
    pub fn get_destinations(&self) -> ProxyDestinations {
        let mut proxy_destination = ProxyDestinations {
            to_http_proxy: false,
            to_https_proxy: false,
            to_tcp_proxy: false,
        };

        match *self {
            Order::AddHttpFrontend(_) | Order::RemoveHttpFrontend(_) => {
                proxy_destination.to_http_proxy = true
            }

            Order::AddHttpsFrontend(_)
            | Order::RemoveHttpsFrontend(_)
            | Order::AddCertificate(_)
            | Order::ReplaceCertificate(_)
            | Order::RemoveCertificate(_)
            | Order::QueryCertificates(_) => proxy_destination.to_https_proxy = true,

            Order::AddTcpFrontend(_) | Order::RemoveTcpFrontend(_) => {
                proxy_destination.to_tcp_proxy = true
            }

            Order::AddCluster(_)
            | Order::AddBackend(_)
            | Order::RemoveCluster { cluster_id: _ }
            | Order::RemoveBackend(_)
            | Order::SoftStop
            | Order::HardStop
            | Order::Status
            | Order::QueryClusters(_)
            | Order::QueryClustersHashes
            | Order::QueryMetrics(_)
            | Order::Logging(_) => {
                proxy_destination.to_http_proxy = true;
                proxy_destination.to_https_proxy = true;
                proxy_destination.to_tcp_proxy = true;
            }

            // the Add***Listener and other Listener orders will be handled separately
            // by the notify_proxys function, so we don't give them destinations
            Order::AddHttpsListener(_)
            | Order::AddHttpListener(_)
            | Order::AddTcpListener(_)
            | Order::RemoveListener(_)
            | Order::ActivateListener(_)
            | Order::DeactivateListener(_)
            | Order::ConfigureMetrics(_)
            | Order::ReturnListenSockets => {}

            // These won't ever reach a worker anyway
            Order::SaveState { path: _ }
            | Order::LoadState { path: _ }
            | Order::DumpState
            | Order::ListWorkers
            | Order::ListFrontends(_)
            | Order::ListListeners
            | Order::LaunchWorker(_)
            | Order::UpgradeMain
            | Order::UpgradeWorker(_)
            | Order::SubscribeEvents
            | Order::ReloadConfiguration { path: _ } => {}
        }
        proxy_destination
    }
}

/// This is sent only from Sōzu to Sōzu
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Deserialize)]
pub struct InnerOrder {
    pub id: MessageId,
    pub content: Order,
}

impl InnerOrder {
    pub fn new(id: String, content: Order) -> Self {
        Self { id, content }
    }
}

impl fmt::Display for InnerOrder {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}-{:?}", self.id, self.content)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProxyDestinations {
    pub to_http_proxy: bool,
    pub to_https_proxy: bool,
    pub to_tcp_proxy: bool,
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

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FrontendFilters {
    pub http: bool,
    pub https: bool,
    pub tcp: bool,
    pub domain: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AddCertificate {
    pub address: SocketAddr,
    pub certificate: CertificateAndKey,
    #[serde(skip_serializing_if = "Vec::is_empty", default = "Vec::new")]
    pub names: Vec<String>,
    /// The `expired_at` override certificate expiration, the value of the field
    /// is a unix timestamp
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expired_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RemoveCertificate {
    pub address: SocketAddr,
    pub fingerprint: CertificateFingerprint,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReplaceCertificate {
    pub address: SocketAddr,
    pub new_certificate: CertificateAndKey,
    pub old_fingerprint: CertificateFingerprint,
    #[serde(skip_serializing_if = "Vec::is_empty", default = "Vec::new")]
    pub new_names: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_expired_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RemoveBackend {
    pub cluster_id: String,
    pub backend_id: String,
    pub address: SocketAddr,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MetricsConfiguration {
    Enabled(bool),
    Clear,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum QueryClusterType {
    ClusterId(String),
    Domain(QueryClusterDomain),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct QueryClusterDomain {
    pub hostname: String,
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum QueryCertificateType {
    All,
    Domain(String),
    Fingerprint(Vec<u8>),
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

pub fn is_false(b: &bool) -> bool {
    !*b
}

fn is_default<T: Default + PartialEq>(t: &T) -> bool {
    t == &T::default()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::certificate::{split_certificate_chain, TlsVersion};
    use crate::config::ProxyProtocolConfig;
    use crate::response::{Backend, HttpFrontend, PathRule, Route, RulePosition};
    use hex::FromHex;
    use serde_json;

    #[test]
    fn config_message_test() {
        let raw_json = r#"{ "id": "ID_TEST", "version": 0, "type": "ADD_HTTP_FRONTEND", "content":{"route": {"CLUSTER_ID": "xxx"}, "hostname": "yyy", "path": {"PREFIX": "xxx"}, "address": "0.0.0.0:8080"}}"#;
        let message: Order = serde_json::from_str(raw_json).unwrap();
        println!("{message:?}");
        assert_eq!(
            message,
            Order::AddHttpFrontend(HttpFrontend {
                route: Route::ClusterId(String::from("xxx")),
                hostname: String::from("yyy"),
                path: PathRule::Prefix(String::from("xxx")),
                method: None,
                address: "0.0.0.0:8080".parse().unwrap(),
                position: RulePosition::Tree,
                tags: None,
            })
        );
    }

    macro_rules! test_message (
    ($name: ident, $filename: expr, $expected_message: expr) => (

      #[test]
      fn $name() {
        let data = include_str!($filename);
        let pretty_print = serde_json::to_string_pretty(&$expected_message).expect("should have serialized");
        assert_eq!(&pretty_print, data, "\nserialized message:\n{}\n\nexpected message:\n{}", pretty_print, data);

        let message: Order = serde_json::from_str(data).unwrap();
        assert_eq!(message, $expected_message, "\ndeserialized message:\n{:#?}\n\nexpected message:\n{:#?}", message, $expected_message);

      }

    )
  );

    test_message!(
        add_cluster,
        "../assets/add_cluster.json",
        Order::AddCluster(Cluster {
            cluster_id: String::from("xxx"),
            sticky_session: true,
            https_redirect: true,
            proxy_protocol: Some(ProxyProtocolConfig::ExpectHeader),
            load_balancing: LoadBalancingAlgorithms::RoundRobin,
            load_metric: None,
            answer_503: None,
        })
    );

    test_message!(
        remove_cluster,
        "../assets/remove_cluster.json",
        Order::RemoveCluster {
            cluster_id: String::from("xxx")
        }
    );

    test_message!(
        add_http_front,
        "../assets/add_http_front.json",
        Order::AddHttpFrontend(HttpFrontend {
            route: Route::ClusterId(String::from("xxx")),
            hostname: String::from("yyy"),
            path: PathRule::Prefix(String::from("xxx")),
            method: None,
            address: "0.0.0.0:8080".parse().unwrap(),
            position: RulePosition::Tree,
            tags: None,
        })
    );

    test_message!(
        remove_http_front,
        "../assets/remove_http_front.json",
        Order::RemoveHttpFrontend(HttpFrontend {
            route: Route::ClusterId(String::from("xxx")),
            hostname: String::from("yyy"),
            path: PathRule::Prefix(String::from("xxx")),
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
        })
    );

    test_message!(
        add_https_front,
        "../assets/add_https_front.json",
        Order::AddHttpsFrontend(HttpFrontend {
            route: Route::ClusterId(String::from("xxx")),
            hostname: String::from("yyy"),
            path: PathRule::Prefix(String::from("xxx")),
            method: None,
            address: "0.0.0.0:8443".parse().unwrap(),
            position: RulePosition::Tree,
            tags: None,
        })
    );

    test_message!(
        remove_https_front,
        "../assets/remove_https_front.json",
        Order::RemoveHttpsFrontend(HttpFrontend {
            route: Route::ClusterId(String::from("xxx")),
            hostname: String::from("yyy"),
            path: PathRule::Prefix(String::from("xxx")),
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
        })
    );

    const KEY: &str = include_str!("../../lib/assets/key.pem");
    const CERTIFICATE: &str = include_str!("../../lib/assets/certificate.pem");
    const CHAIN: &str = include_str!("../../lib/assets/certificate_chain.pem");

    test_message!(
        add_certificate,
        "../assets/add_certificate.json",
        Order::AddCertificate(AddCertificate {
            address: "0.0.0.0:443".parse().unwrap(),
            certificate: CertificateAndKey {
                certificate: String::from(CERTIFICATE),
                certificate_chain: split_certificate_chain(String::from(CHAIN)),
                key: String::from(KEY),
                versions: vec![TlsVersion::TLSv1_2, TlsVersion::TLSv1_3],
            },
            names: vec![],
            expired_at: None,
        })
    );

    test_message!(
        remove_certificate,
        "../assets/remove_certificate.json",
        Order::RemoveCertificate(RemoveCertificate {
            address: "0.0.0.0:443".parse().unwrap(),
            fingerprint: CertificateFingerprint(
                FromHex::from_hex(
                    "ab2618b674e15243fd02a5618c66509e4840ba60e7d64cebec84cdbfeceee0c5"
                )
                .unwrap()
            ),
        })
    );

    test_message!(
        add_backend,
        "../assets/add_backend.json",
        Order::AddBackend(Backend {
            cluster_id: String::from("xxx"),
            backend_id: String::from("xxx-0"),
            address: "127.0.0.1:8080".parse().unwrap(),
            load_balancing_parameters: Some(LoadBalancingParams { weight: 0 }),
            sticky_id: Some(String::from("xxx-0")),
            backup: Some(false),
        })
    );

    test_message!(
        remove_backend,
        "../assets/remove_backend.json",
        Order::RemoveBackend(RemoveBackend {
            cluster_id: String::from("xxx"),
            backend_id: String::from("xxx-0"),
            address: "127.0.0.1:8080".parse().unwrap(),
        })
    );

    test_message!(soft_stop, "../assets/soft_stop.json", Order::SoftStop);

    test_message!(hard_stop, "../assets/hard_stop.json", Order::HardStop);

    test_message!(status, "../assets/status.json", Order::Status);

    test_message!(
        load_state,
        "../assets/load_state.json",
        Order::LoadState {
            path: String::from("./config_dump.json")
        }
    );

    test_message!(
        save_state,
        "../assets/save_state.json",
        Order::SaveState {
            path: String::from("./config_dump.json")
        }
    );

    test_message!(dump_state, "../assets/dump_state.json", Order::DumpState);

    test_message!(
        list_workers,
        "../assets/list_workers.json",
        Order::ListWorkers
    );

    test_message!(
        upgrade_main,
        "../assets/upgrade_main.json",
        Order::UpgradeMain
    );

    test_message!(
        upgrade_worker,
        "../assets/upgrade_worker.json",
        Order::UpgradeWorker(0)
    );

    #[test]
    fn add_front_test() {
        let raw_json = r#"{"type": "ADD_HTTP_FRONTEND", "content": {"route": { "CLUSTER_ID": "xxx"}, "hostname": "yyy", "path": {"PREFIX": "xxx"}, "address": "127.0.0.1:4242", "sticky_session": false}}"#;
        let command: Order = serde_json::from_str(raw_json).expect("could not parse json");
        println!("{command:?}");
        assert!(
            command
                == Order::AddHttpFrontend(HttpFrontend {
                    route: Route::ClusterId(String::from("xxx")),
                    hostname: String::from("yyy"),
                    path: PathRule::Prefix(String::from("xxx")),
                    method: None,
                    address: "127.0.0.1:4242".parse().unwrap(),
                    position: RulePosition::Tree,
                    tags: None,
                })
        );
    }

    #[test]
    fn remove_front_test() {
        let raw_json = r#"{"type": "REMOVE_HTTP_FRONTEND", "content": {"route": {"CLUSTER_ID": "xxx"}, "hostname": "yyy", "path": {"PREFIX": "xxx"}, "address": "127.0.0.1:4242", "tags": { "owner": "John", "id": "some-long-id" }}}"#;
        let command: Order = serde_json::from_str(raw_json).expect("could not parse json");
        println!("{command:?}");
        assert!(
            command
                == Order::RemoveHttpFrontend(HttpFrontend {
                    route: Route::ClusterId(String::from("xxx")),
                    hostname: String::from("yyy"),
                    path: PathRule::Prefix(String::from("xxx")),
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
        let raw_json = r#"{"type": "ADD_BACKEND", "content": {"cluster_id": "xxx", "backend_id": "xxx-0", "address": "0.0.0.0:8080", "load_balancing_parameters": { "weight": 0 }}}"#;
        let command: Order = serde_json::from_str(raw_json).expect("could not parse json");
        println!("{command:?}");
        assert!(
            command
                == Order::AddBackend(Backend {
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
        let raw_json = r#"{"type": "REMOVE_BACKEND", "content": {"cluster_id": "xxx", "backend_id": "xxx-0", "address": "0.0.0.0:8080"}}"#;
        let command: Order = serde_json::from_str(raw_json).expect("could not parse json");
        println!("{command:?}");
        assert!(
            command
                == Order::RemoveBackend(RemoveBackend {
                    cluster_id: String::from("xxx"),
                    backend_id: String::from("xxx-0"),
                    address: "0.0.0.0:8080".parse().unwrap(),
                })
        );
    }

    #[test]
    fn http_front_crash_test() {
        let raw_json = r#"{"type": "ADD_HTTP_FRONTEND", "content": {"route": {"CLUSTER_ID": "aa"}, "hostname": "cltdl.fr", "path": {"PREFIX": ""}, "address": "127.0.0.1:4242", "tags": { "owner": "John", "id": "some-long-id" }}}"#;
        let command: Order = serde_json::from_str(raw_json).expect("could not parse json");
        println!("{command:?}");
        assert!(
            command
                == Order::AddHttpFrontend(HttpFrontend {
                    route: Route::ClusterId(String::from("aa")),
                    hostname: String::from("cltdl.fr"),
                    path: PathRule::Prefix(String::from("")),
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
        let raw_json = r#"{"route": {"CLUSTER_ID": "aa"}, "hostname": "cltdl.fr", "path": {"PREFIX": ""}, "address": "127.0.0.1:4242" }"#;
        let front: HttpFrontend = serde_json::from_str(raw_json).expect("could not parse json");
        println!("{front:?}");
        assert!(
            front
                == HttpFrontend {
                    route: Route::ClusterId(String::from("aa")),
                    hostname: String::from("cltdl.fr"),
                    path: PathRule::Prefix(String::from("")),
                    method: None,
                    address: "127.0.0.1:4242".parse().unwrap(),
                    position: RulePosition::Tree,
                    tags: None,
                }
        );
    }
}
