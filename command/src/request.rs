use std::{collections::BTreeMap, error, fmt, net::SocketAddr, str::FromStr};

use anyhow::Context;

use crate::{
    certificate::{CertificateAndKey, Fingerprint},
    config::ProxyProtocolConfig,
    response::{
        is_default_path_rule, HttpFrontend, HttpListenerConfig, HttpsListenerConfig, MessageId,
        PathRule, RulePosition, TcpListenerConfig,
    },
    state::{ClusterId, RouteKey},
};

pub const PROTOCOL_VERSION: u8 = 0;

/// A request sent by the CLI (or other) to the main process
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Request {
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

    AddHttpFrontend(RequestHttpFrontend),
    RemoveHttpFrontend(RequestHttpFrontend),

    AddHttpsFrontend(RequestHttpFrontend),
    RemoveHttpsFrontend(RequestHttpFrontend),

    AddCertificate(AddCertificate),
    ReplaceCertificate(ReplaceCertificate),
    RemoveCertificate(RemoveCertificate),

    AddTcpFrontend(RequestTcpFrontend),
    RemoveTcpFrontend(RequestTcpFrontend),

    AddBackend(AddBackend),
    RemoveBackend(RemoveBackend),

    AddHttpListener(HttpListenerConfig),
    AddHttpsListener(HttpsListenerConfig),
    AddTcpListener(TcpListenerConfig),

    RemoveListener(RemoveListener),

    ActivateListener(ActivateListener),
    DeactivateListener(DeactivateListener),

    QueryAllCertificates,
    QueryCertificateByFingerprint(Fingerprint),
    QueryCertificatesByDomain(String),

    QueryClusterById(ClusterId),
    QueryClustersByDomain(QueryClusterByDomain),
    QueryClustersHashes,
    QueryMetrics(QueryMetricsOptions),

    SoftStop,
    HardStop,

    ConfigureMetrics(MetricsConfiguration),
    Logging(String),

    ReturnListenSockets,
}

impl Request {
    /// determine to which of the three proxies (HTTP, HTTPS, TCP) a request is destined
    pub fn get_destinations(&self) -> ProxyDestinations {
        let mut proxy_destination = ProxyDestinations {
            to_http_proxy: false,
            to_https_proxy: false,
            to_tcp_proxy: false,
        };

        match *self {
            Request::AddHttpFrontend(_) | Request::RemoveHttpFrontend(_) => {
                proxy_destination.to_http_proxy = true
            }

            Request::AddHttpsFrontend(_)
            | Request::RemoveHttpsFrontend(_)
            | Request::AddCertificate(_)
            | Request::QueryAllCertificates
            | Request::QueryCertificatesByDomain(_)
            | Request::QueryCertificateByFingerprint(_)
            | Request::ReplaceCertificate(_)
            | Request::RemoveCertificate(_) => proxy_destination.to_https_proxy = true,

            Request::AddTcpFrontend(_) | Request::RemoveTcpFrontend(_) => {
                proxy_destination.to_tcp_proxy = true
            }

            Request::AddCluster(_)
            | Request::AddBackend(_)
            | Request::RemoveCluster { cluster_id: _ }
            | Request::RemoveBackend(_)
            | Request::SoftStop
            | Request::HardStop
            | Request::Status
            | Request::QueryClusterById(_)
            | Request::QueryClustersByDomain(_)
            | Request::QueryClustersHashes
            | Request::QueryMetrics(_)
            | Request::Logging(_) => {
                proxy_destination.to_http_proxy = true;
                proxy_destination.to_https_proxy = true;
                proxy_destination.to_tcp_proxy = true;
            }

            // the Add***Listener and other Listener orders will be handled separately
            // by the notify_proxys function, so we don't give them destinations
            Request::AddHttpsListener(_)
            | Request::AddHttpListener(_)
            | Request::AddTcpListener(_)
            | Request::RemoveListener(_)
            | Request::ActivateListener(_)
            | Request::DeactivateListener(_)
            | Request::ConfigureMetrics(_)
            | Request::ReturnListenSockets => {}

            // These won't ever reach a worker anyway
            Request::SaveState { path: _ }
            | Request::LoadState { path: _ }
            | Request::DumpState
            | Request::ListWorkers
            | Request::ListFrontends(_)
            | Request::ListListeners
            | Request::LaunchWorker(_)
            | Request::UpgradeMain
            | Request::UpgradeWorker(_)
            | Request::SubscribeEvents
            | Request::ReloadConfiguration { path: _ } => {}
        }
        proxy_destination
    }

    /// True if the request is a SoftStop or a HardStop
    pub fn is_a_stop(&self) -> bool {
        self == &Self::SoftStop || self == &Self::HardStop
    }
}

/// This is sent only from Sōzu to Sōzu
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Deserialize)]
pub struct WorkerRequest {
    pub id: MessageId,
    pub content: Request,
}

impl WorkerRequest {
    pub fn new(id: String, content: Request) -> Self {
        Self { id, content }
    }
}

impl fmt::Display for WorkerRequest {
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
    pub address: String,
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
    pub address: String,
    pub fingerprint: Fingerprint,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReplaceCertificate {
    pub address: String,
    pub new_certificate: CertificateAndKey,
    pub old_fingerprint: Fingerprint,
    #[serde(skip_serializing_if = "Vec::is_empty", default = "Vec::new")]
    pub new_names: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_expired_at: Option<i64>,
}

/// Meant for outside users, contains a String instead of a SocketAddr
#[derive(Debug, Clone, PartialOrd, Ord, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RequestTcpFrontend {
    pub cluster_id: String,
    pub address: String,
    pub tags: Option<BTreeMap<String, String>>,
}

/// A frontend as requested from the client, with a string SocketAddress
#[derive(Debug, Clone, PartialOrd, Ord, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RequestHttpFrontend {
    /// the route is DENY if none
    pub route: Option<ClusterId>,
    pub address: String,
    pub hostname: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "is_default_path_rule")]
    pub path: PathRule,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default)]
    pub position: RulePosition,
    pub tags: Option<BTreeMap<String, String>>,
}

impl RequestHttpFrontend {
    /// `route_key` returns a representation of the frontend as a route key
    pub fn route_key(&self) -> RouteKey {
        self.into()
    }

    /// convert a requested frontend to a usable one by parsing its address
    pub fn to_frontend(self) -> anyhow::Result<HttpFrontend> {
        Ok(HttpFrontend {
            address: self
                .address
                .parse::<SocketAddr>()
                .with_context(|| "wrong socket address")?,
            route: self.route,
            hostname: self.hostname,
            path: self.path,
            method: self.method,
            position: self.position,
            tags: self.tags,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RemoveBackend {
    pub cluster_id: String,
    pub backend_id: String,
    pub address: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AddBackend {
    pub cluster_id: String,
    pub backend_id: String,
    pub address: String,
    pub sticky_id: Option<String>,
    pub load_balancing_parameters: Option<LoadBalancingParams>,
    pub backup: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MetricsConfiguration {
    Enabled,
    Disabled,
    Clear,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct QueryClusterByDomain {
    pub hostname: String,
    pub path: Option<String>,
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
    use crate::response::{HttpFrontend, PathRule, RulePosition};
    use hex::FromHex;
    use serde_json;

    #[test]
    fn config_message_test() {
        let raw_json = r#"{ "id": "ID_TEST", "version": 0, "type": "ADD_HTTP_FRONTEND", "content":{"route": "xxx", "hostname": "yyy", "path": {"kind": "PREFIX", "value": "xxx"}, "address": "0.0.0.0:8080"}}"#;
        let message: Request = serde_json::from_str(raw_json).unwrap();
        println!("{message:?}");
        assert_eq!(
            message,
            Request::AddHttpFrontend(RequestHttpFrontend {
                route: Some(String::from("xxx")),
                hostname: String::from("yyy"),
                path: PathRule::prefix(String::from("xxx")),
                method: None,
                address: "0.0.0.0:8080".to_string(),
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

        let message: Request = serde_json::from_str(data).unwrap();
        assert_eq!(message, $expected_message, "\ndeserialized message:\n{:#?}\n\nexpected message:\n{:#?}", message, $expected_message);

      }

    )
  );

    test_message!(
        add_cluster,
        "../assets/add_cluster.json",
        Request::AddCluster(Cluster {
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
        Request::RemoveCluster {
            cluster_id: String::from("xxx")
        }
    );

    test_message!(
        add_http_front,
        "../assets/add_http_front.json",
        Request::AddHttpFrontend(RequestHttpFrontend {
            route: Some(String::from("xxx")),
            hostname: String::from("yyy"),
            path: PathRule::prefix(String::from("xxx")),
            method: None,
            address: "0.0.0.0:8080".to_string(),
            position: RulePosition::Tree,
            tags: None,
        })
    );

    test_message!(
        remove_http_front,
        "../assets/remove_http_front.json",
        Request::RemoveHttpFrontend(RequestHttpFrontend {
            route: Some(String::from("xxx")),
            hostname: String::from("yyy"),
            path: PathRule::prefix(String::from("xxx")),
            method: None,
            address: "0.0.0.0:8080".to_string(),
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
        Request::AddHttpsFrontend(RequestHttpFrontend {
            route: Some(String::from("xxx")),
            hostname: String::from("yyy"),
            path: PathRule::prefix(String::from("xxx")),
            method: None,
            address: "0.0.0.0:8443".to_string(),
            position: RulePosition::Tree,
            tags: None,
        })
    );

    test_message!(
        remove_https_front,
        "../assets/remove_https_front.json",
        Request::RemoveHttpsFrontend(RequestHttpFrontend {
            route: Some(String::from("xxx")),
            hostname: String::from("yyy"),
            path: PathRule::prefix(String::from("xxx")),
            method: None,
            address: "0.0.0.0:8443".to_string(),
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
        Request::AddCertificate(AddCertificate {
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
        Request::RemoveCertificate(RemoveCertificate {
            address: "0.0.0.0:443".parse().unwrap(),
            fingerprint: Fingerprint(
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
        Request::AddBackend(AddBackend {
            cluster_id: String::from("xxx"),
            backend_id: String::from("xxx-0"),
            address: "127.0.0.1:8080".to_string(),
            load_balancing_parameters: Some(LoadBalancingParams { weight: 0 }),
            sticky_id: Some(String::from("xxx-0")),
            backup: Some(false),
        })
    );

    test_message!(
        remove_backend,
        "../assets/remove_backend.json",
        Request::RemoveBackend(RemoveBackend {
            cluster_id: String::from("xxx"),
            backend_id: String::from("xxx-0"),
            address: "127.0.0.1:8080".parse().unwrap(),
        })
    );

    test_message!(soft_stop, "../assets/soft_stop.json", Request::SoftStop);

    test_message!(hard_stop, "../assets/hard_stop.json", Request::HardStop);

    test_message!(status, "../assets/status.json", Request::Status);

    test_message!(
        load_state,
        "../assets/load_state.json",
        Request::LoadState {
            path: String::from("./config_dump.json")
        }
    );

    test_message!(
        save_state,
        "../assets/save_state.json",
        Request::SaveState {
            path: String::from("./config_dump.json")
        }
    );

    test_message!(dump_state, "../assets/dump_state.json", Request::DumpState);

    test_message!(
        list_workers,
        "../assets/list_workers.json",
        Request::ListWorkers
    );

    test_message!(
        upgrade_main,
        "../assets/upgrade_main.json",
        Request::UpgradeMain
    );

    test_message!(
        upgrade_worker,
        "../assets/upgrade_worker.json",
        Request::UpgradeWorker(0)
    );

    #[test]
    fn add_front_test() {
        let raw_json = r#"{"type": "ADD_HTTP_FRONTEND", "content": {"route": "xxx", "hostname": "yyy", "path": {"kind": "PREFIX", "value": "xxx"}, "address": "127.0.0.1:4242", "sticky_session": false}}"#;
        let command: Request = serde_json::from_str(raw_json).expect("could not parse json");
        println!("{command:?}");
        assert!(
            command
                == Request::AddHttpFrontend(RequestHttpFrontend {
                    route: Some(String::from("xxx")),
                    hostname: String::from("yyy"),
                    path: PathRule::prefix(String::from("xxx")),
                    method: None,
                    address: "127.0.0.1:4242".to_string(),
                    position: RulePosition::Tree,
                    tags: None,
                })
        );
    }

    #[test]
    fn remove_front_test() {
        let raw_json = r#"{"type": "REMOVE_HTTP_FRONTEND", "content": {"route": "xxx", "hostname": "yyy", "path": {"kind": "PREFIX", "value": "xxx"}, "address": "127.0.0.1:4242", "tags": { "owner": "John", "id": "some-long-id" }}}"#;
        let command: Request = serde_json::from_str(raw_json).expect("could not parse json");
        println!("{command:?}");
        assert!(
            command
                == Request::RemoveHttpFrontend(RequestHttpFrontend {
                    route: Some(String::from("xxx")),
                    hostname: String::from("yyy"),
                    path: PathRule::prefix(String::from("xxx")),
                    method: None,
                    address: "127.0.0.1:4242".to_string(),
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
        let command: Request = serde_json::from_str(raw_json).expect("could not parse json");
        println!("{command:?}");
        assert!(
            command
                == Request::AddBackend(AddBackend {
                    cluster_id: String::from("xxx"),
                    backend_id: String::from("xxx-0"),
                    address: "0.0.0.0:8080".to_string(),
                    sticky_id: None,
                    load_balancing_parameters: Some(LoadBalancingParams { weight: 0 }),
                    backup: None,
                })
        );
    }

    #[test]
    fn remove_backend_test() {
        let raw_json = r#"{"type": "REMOVE_BACKEND", "content": {"cluster_id": "xxx", "backend_id": "xxx-0", "address": "0.0.0.0:8080"}}"#;
        let command: Request = serde_json::from_str(raw_json).expect("could not parse json");
        println!("{command:?}");
        assert!(
            command
                == Request::RemoveBackend(RemoveBackend {
                    cluster_id: String::from("xxx"),
                    backend_id: String::from("xxx-0"),
                    address: "0.0.0.0:8080".parse().unwrap(),
                })
        );
    }

    #[test]
    fn http_front_crash_test() {
        let raw_json = r#"{"type": "ADD_HTTP_FRONTEND", "content": {"route": "aa", "hostname": "cltdl.fr", "path": {"kind": "PREFIX", "value": ""}, "address": "127.0.0.1:4242", "tags": { "owner": "John", "id": "some-long-id" }}}"#;
        let command: Request = serde_json::from_str(raw_json).expect("could not parse json");
        println!("{command:?}");
        assert!(
            command
                == Request::AddHttpFrontend(RequestHttpFrontend {
                    route: Some(String::from("aa")),
                    hostname: String::from("cltdl.fr"),
                    path: PathRule::prefix(String::from("")),
                    method: None,
                    address: "127.0.0.1:4242".to_string(),
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
        let raw_json = r#"{"route": "aa", "hostname": "cltdl.fr", "path": {"kind": "PREFIX", "value": ""}, "address": "127.0.0.1:4242" }"#;
        let front: HttpFrontend = serde_json::from_str(raw_json).expect("could not parse json");
        println!("{front:?}");
        assert!(
            front
                == HttpFrontend {
                    route: Some(String::from("aa")),
                    hostname: String::from("cltdl.fr"),
                    path: PathRule::prefix(String::from("")),
                    method: None,
                    address: "127.0.0.1:4242".parse().unwrap(),
                    position: RulePosition::Tree,
                    tags: None,
                }
        );
    }
}
