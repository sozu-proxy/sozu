use std::{
    error,
    fmt::{self, Display},
    net::SocketAddr,
    str::FromStr,
};

use anyhow::Context;

use crate::{
    proto::command::{
        request::RequestType, LoadBalancingAlgorithms, PathRuleKind, Request, RequestHttpFrontend,
        RulePosition,
    },
    response::{HttpFrontend, MessageId},
};

impl Request {
    /// determine to which of the three proxies (HTTP, HTTPS, TCP) a request is destined
    pub fn get_destinations(&self) -> ProxyDestinations {
        let mut proxy_destination = ProxyDestinations {
            to_http_proxy: false,
            to_https_proxy: false,
            to_tcp_proxy: false,
        };
        let request_type = match &self.request_type {
            Some(t) => t,
            None => return proxy_destination,
        };

        match request_type {
            RequestType::AddHttpFrontend(_) | RequestType::RemoveHttpFrontend(_) => {
                proxy_destination.to_http_proxy = true
            }

            RequestType::AddHttpsFrontend(_)
            | RequestType::RemoveHttpsFrontend(_)
            | RequestType::AddCertificate(_)
            | RequestType::QueryAllCertificates(_)
            | RequestType::QueryCertificatesByDomain(_)
            | RequestType::QueryCertificateByFingerprint(_)
            | RequestType::ReplaceCertificate(_)
            | RequestType::RemoveCertificate(_) => proxy_destination.to_https_proxy = true,

            RequestType::AddTcpFrontend(_) | RequestType::RemoveTcpFrontend(_) => {
                proxy_destination.to_tcp_proxy = true
            }

            RequestType::AddCluster(_)
            | RequestType::AddBackend(_)
            | RequestType::RemoveCluster(_)
            | RequestType::RemoveBackend(_)
            | RequestType::SoftStop(_)
            | RequestType::HardStop(_)
            | RequestType::Status(_)
            | RequestType::QueryClusterById(_)
            | RequestType::QueryClustersByDomain(_)
            | RequestType::QueryClustersHashes(_)
            | RequestType::QueryMetrics(_)
            | RequestType::Logging(_) => {
                proxy_destination.to_http_proxy = true;
                proxy_destination.to_https_proxy = true;
                proxy_destination.to_tcp_proxy = true;
            }

            // the Add***Listener and other Listener orders will be handled separately
            // by the notify_proxys function, so we don't give them destinations
            RequestType::AddHttpsListener(_)
            | RequestType::AddHttpListener(_)
            | RequestType::AddTcpListener(_)
            | RequestType::RemoveListener(_)
            | RequestType::ActivateListener(_)
            | RequestType::DeactivateListener(_)
            | RequestType::ConfigureMetrics(_)
            | RequestType::ReturnListenSockets(_) => {}

            // These won't ever reach a worker anyway
            RequestType::SaveState(_)
            | RequestType::LoadState(_)
            | RequestType::ListWorkers(_)
            | RequestType::ListFrontends(_)
            | RequestType::ListListeners(_)
            | RequestType::LaunchWorker(_)
            | RequestType::UpgradeMain(_)
            | RequestType::UpgradeWorker(_)
            | RequestType::SubscribeEvents(_)
            | RequestType::ReloadConfiguration(_) => {}
        }
        proxy_destination
    }

    /// True if the request is a SoftStop or a HardStop
    pub fn is_a_stop(&self) -> bool {
        match self.request_type {
            Some(RequestType::SoftStop(_)) | Some(RequestType::HardStop(_)) => true,
            _ => false,
        }
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

impl RequestHttpFrontend {
    /// convert a requested frontend to a usable one by parsing its address
    pub fn to_frontend(self) -> anyhow::Result<HttpFrontend> {
        Ok(HttpFrontend {
            address: self
                .address
                .parse::<SocketAddr>()
                .with_context(|| "wrong socket address")?,
            cluster_id: self.cluster_id,
            hostname: self.hostname,
            path: self.path,
            method: self.method,
            position: RulePosition::from_i32(self.position)
                .with_context(|| "wrong i32 value for RulePosition")?,
            tags: Some(self.tags),
        })
    }
}

impl Display for RequestHttpFrontend {
    /// Used to create a unique summary of the frontend, used as a key in maps
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match &PathRuleKind::from_i32(self.path.kind) {
            Some(PathRuleKind::Prefix) => {
                format!("{};{};P{}", self.address, self.hostname, self.path.value)
            }
            Some(PathRuleKind::Regex) => {
                format!("{};{};R{}", self.address, self.hostname, self.path.value)
            }
            Some(PathRuleKind::Equals) => {
                format!("{};{};={}", self.address, self.hostname, self.path.value)
            }
            None => String::from("Wrong variant of PathRuleKind"),
        };

        match &self.method {
            Some(method) => write!(f, "{s};{method}"),
            None => write!(f, "{s}"),
        }
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

pub fn is_false(b: &bool) -> bool {
    !*b
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::{
        certificate::split_certificate_chain,
        proto::command::{
            AddBackend, AddCertificate, CertificateAndKey, Cluster, HardStop, ListWorkers,
            LoadBalancingParams, PathRule, ProxyProtocolConfig, RemoveBackend, RemoveCertificate,
            RulePosition, SoftStop, Status, TlsVersion, UpgradeMain,
        },
        response::HttpFrontend,
    };
    use serde_json;

    #[test]
    fn config_message_test() {
        let raw_json = r#"{ "request_type": {"ADD_HTTP_FRONTEND": {"cluster_id": "xxx", "hostname": "yyy", "path": {"kind": 0, "value": "xxx"}, "method": null, "position": 2, "address": "0.0.0.0:8080", "tags": {}}}}"#;
        let message: Request = serde_json::from_str(raw_json).unwrap();
        println!("{message:?}");
        assert_eq!(
            message,
            Request {
                request_type: Some(RequestType::AddHttpFrontend(RequestHttpFrontend {
                    cluster_id: Some(String::from("xxx")),
                    hostname: String::from("yyy"),
                    path: PathRule::prefix(String::from("xxx")),
                    address: "0.0.0.0:8080".to_string(),
                    ..Default::default()
                }))
            }
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
        assert_eq!(
            message,
            $expected_message,
            "\ndeserialized message:\n{:#?}\n\nexpected message:\n{:#?}", message, $expected_message
        );
      }

    )
  );

    test_message!(
        add_cluster,
        "../assets/add_cluster.json",
        Request {
            request_type: Some(RequestType::AddCluster(Cluster {
                cluster_id: String::from("xxx"),
                sticky_session: true,
                https_redirect: true,
                proxy_protocol: Some(ProxyProtocolConfig::ExpectHeader as i32),
                load_balancing: LoadBalancingAlgorithms::RoundRobin as i32,
                ..Default::default()
            }))
        }
    );

    test_message!(
        remove_cluster,
        "../assets/remove_cluster.json",
        Request {
            request_type: Some(RequestType::RemoveCluster(String::from("xxx")))
        }
    );

    test_message!(
        add_http_front,
        "../assets/add_http_front.json",
        Request {
            request_type: Some(RequestType::AddHttpFrontend(RequestHttpFrontend {
                cluster_id: Some(String::from("xxx")),
                hostname: String::from("yyy"),
                path: PathRule::prefix(String::from("xxx")),
                address: "0.0.0.0:8080".to_string(),
                ..Default::default()
            }))
        }
    );

    test_message!(
        remove_http_front,
        "../assets/remove_http_front.json",
        Request {
            request_type: Some(RequestType::RemoveHttpFrontend(RequestHttpFrontend {
                cluster_id: Some(String::from("xxx")),
                hostname: String::from("yyy"),
                path: PathRule::prefix(String::from("xxx")),
                address: "0.0.0.0:8080".to_string(),
                tags: BTreeMap::from([
                    ("owner".to_owned(), "John".to_owned()),
                    (
                        "uuid".to_owned(),
                        "0dd8d7b1-a50a-461a-b1f9-5211a5f45a83".to_owned()
                    )
                ]),
                ..Default::default()
            }))
        }
    );

    test_message!(
        add_https_front,
        "../assets/add_https_front.json",
        Request {
            request_type: Some(RequestType::AddHttpsFrontend(RequestHttpFrontend {
                cluster_id: Some(String::from("xxx")),
                hostname: String::from("yyy"),
                path: PathRule::prefix(String::from("xxx")),
                address: "0.0.0.0:8443".to_string(),
                ..Default::default()
            }))
        }
    );

    test_message!(
        remove_https_front,
        "../assets/remove_https_front.json",
        Request {
            request_type: Some(RequestType::RemoveHttpsFrontend(RequestHttpFrontend {
                cluster_id: Some(String::from("xxx")),
                hostname: String::from("yyy"),
                path: PathRule::prefix(String::from("xxx")),
                address: "0.0.0.0:8443".to_string(),
                tags: BTreeMap::from([
                    ("owner".to_owned(), "John".to_owned()),
                    (
                        "uuid".to_owned(),
                        "0dd8d7b1-a50a-461a-b1f9-5211a5f45a83".to_owned()
                    )
                ]),
                ..Default::default()
            }))
        }
    );

    const KEY: &str = include_str!("../../lib/assets/key.pem");
    const CERTIFICATE: &str = include_str!("../../lib/assets/certificate.pem");
    const CHAIN: &str = include_str!("../../lib/assets/certificate_chain.pem");

    test_message!(
        add_certificate,
        "../assets/add_certificate.json",
        Request {
            request_type: Some(RequestType::AddCertificate(AddCertificate {
                address: "0.0.0.0:443".parse().unwrap(),
                certificate: CertificateAndKey {
                    certificate: String::from(CERTIFICATE),
                    certificate_chain: split_certificate_chain(String::from(CHAIN)),
                    key: String::from(KEY),
                    versions: vec![TlsVersion::TlsV12.into(), TlsVersion::TlsV13.into()],
                    names: vec![],
                },
                expired_at: None,
            }))
        }
    );

    test_message!(
        remove_certificate,
        "../assets/remove_certificate.json",
        Request {
            request_type: Some(RequestType::RemoveCertificate(RemoveCertificate {
                address: "0.0.0.0:443".parse().unwrap(),
                fingerprint: "ab2618b674e15243fd02a5618c66509e4840ba60e7d64cebec84cdbfeceee0c5"
                    .to_owned(),
            }))
        }
    );

    test_message!(
        add_backend,
        "../assets/add_backend.json",
        Request {
            request_type: Some(RequestType::AddBackend(AddBackend {
                cluster_id: String::from("xxx"),
                backend_id: String::from("xxx-0"),
                address: "127.0.0.1:8080".to_string(),
                load_balancing_parameters: Some(LoadBalancingParams { weight: 0 }),
                sticky_id: Some(String::from("xxx-0")),
                backup: Some(false),
            }))
        }
    );

    test_message!(
        remove_backend,
        "../assets/remove_backend.json",
        Request {
            request_type: Some(RequestType::RemoveBackend(RemoveBackend {
                cluster_id: String::from("xxx"),
                backend_id: String::from("xxx-0"),
                address: "127.0.0.1:8080".parse().unwrap(),
            }))
        }
    );

    test_message!(
        soft_stop,
        "../assets/soft_stop.json",
        Request {
            request_type: Some(RequestType::SoftStop(SoftStop {}))
        }
    );

    test_message!(
        hard_stop,
        "../assets/hard_stop.json",
        Request {
            request_type: Some(RequestType::HardStop(HardStop {}))
        }
    );

    test_message!(
        status,
        "../assets/status.json",
        Request {
            request_type: Some(RequestType::Status(Status {}))
        }
    );

    test_message!(
        load_state,
        "../assets/load_state.json",
        Request {
            request_type: Some(RequestType::LoadState(String::from("./config_dump.json")))
        }
    );

    test_message!(
        save_state,
        "../assets/save_state.json",
        Request {
            request_type: Some(RequestType::SaveState(String::from("./config_dump.json")))
        }
    );

    test_message!(
        list_workers,
        "../assets/list_workers.json",
        Request {
            request_type: Some(RequestType::ListWorkers(ListWorkers {}))
        }
    );

    test_message!(
        upgrade_main,
        "../assets/upgrade_main.json",
        Request {
            request_type: Some(RequestType::UpgradeMain(UpgradeMain {}))
        }
    );

    test_message!(
        upgrade_worker,
        "../assets/upgrade_worker.json",
        Request {
            request_type: Some(RequestType::UpgradeWorker(0))
        }
    );

    #[test]
    fn add_front_test() {
        let raw_json = r#"{"request_type": {"ADD_HTTP_FRONTEND": {"cluster_id": "xxx", "hostname": "yyy", "path": {"kind": 0, "value": "xxx"}, "method": null, "position": 2, "address": "127.0.0.1:4242", "sticky_session": false, "tags": {}}}}"#;
        let parsed_request: Request = serde_json::from_str(raw_json).expect("could not parse json");
        println!("parsed: {:?}", parsed_request);

        let add_http_frontend = Request {
            request_type: Some(RequestType::AddHttpFrontend(RequestHttpFrontend {
                cluster_id: Some(String::from("xxx")),
                hostname: String::from("yyy"),
                path: PathRule::prefix(String::from("xxx")),
                address: "127.0.0.1:4242".to_string(),
                ..Default::default()
            })),
        };
        println!("expected: {:?}", add_http_frontend);
        assert!(parsed_request == add_http_frontend);
    }

    #[test]
    fn remove_front_test() {
        let raw_json = r#"{"request_type": {"REMOVE_HTTP_FRONTEND": {"cluster_id": "xxx", "hostname": "yyy", "path": {"kind": 0, "value": "xxx"}, "position": 2, "address": "127.0.0.1:4242", "tags": { "owner": "John", "id": "some-long-id" }}}}"#;
        let command: Request = serde_json::from_str(raw_json).expect("could not parse json");
        println!("{command:?}");
        let remove_http_frontend = Request {
            request_type: Some(RequestType::RemoveHttpFrontend(RequestHttpFrontend {
                cluster_id: Some(String::from("xxx")),
                hostname: String::from("yyy"),
                path: PathRule::prefix(String::from("xxx")),
                address: "127.0.0.1:4242".to_string(),
                tags: BTreeMap::from([
                    ("owner".to_owned(), "John".to_owned()),
                    ("id".to_owned(), "some-long-id".to_owned()),
                ]),
                ..Default::default()
            })),
        };
        assert!(command == remove_http_frontend);
    }

    #[test]
    fn add_backend_test() {
        let raw_json = r#"{"request_type": {"ADD_BACKEND": {"cluster_id": "xxx", "backend_id": "xxx-0", "address": "0.0.0.0:8080", "load_balancing_parameters": { "weight": 0 }}}}"#;
        let command: Request = serde_json::from_str(raw_json).expect("could not parse json");
        println!("{command:?}");
        let add_backend = Request {
            request_type: Some(RequestType::AddBackend(AddBackend {
                cluster_id: String::from("xxx"),
                backend_id: String::from("xxx-0"),
                address: "0.0.0.0:8080".to_string(),
                load_balancing_parameters: Some(LoadBalancingParams { weight: 0 }),
                ..Default::default()
            })),
        };
        assert!(command == add_backend);
    }

    #[test]
    fn remove_backend_test() {
        let raw_json = r#"{"request_type": {"REMOVE_BACKEND": {"cluster_id": "xxx", "backend_id": "xxx-0", "address": "0.0.0.0:8080"}}}"#;
        let command: Request = serde_json::from_str(raw_json).expect("could not parse json");
        println!("{command:?}");
        let remove_backend = Request {
            request_type: Some(RequestType::RemoveBackend(RemoveBackend {
                cluster_id: String::from("xxx"),
                backend_id: String::from("xxx-0"),
                address: "0.0.0.0:8080".parse().unwrap(),
            })),
        };
        assert!(command == remove_backend);
    }

    #[test]
    fn http_front_crash_test() {
        let raw_json = r#"{"request_type": {"ADD_HTTP_FRONTEND": {"cluster_id": "aa", "hostname": "cltdl.fr", "path": {"kind": 0, "value": ""}, "position": 2, "address": "127.0.0.1:4242", "tags": { "owner": "John", "id": "some-long-id" }}}}"#;
        let command: Request = serde_json::from_str(raw_json).expect("could not parse json");
        println!("{command:?}");
        let add_http_frontend = Request {
            request_type: Some(RequestType::AddHttpFrontend(RequestHttpFrontend {
                cluster_id: Some(String::from("aa")),
                hostname: String::from("cltdl.fr"),
                path: PathRule::prefix(String::from("")),
                address: "127.0.0.1:4242".to_string(),
                tags: BTreeMap::from([
                    ("owner".to_owned(), "John".to_owned()),
                    ("id".to_owned(), "some-long-id".to_owned()),
                ]),
                ..Default::default()
            })),
        };
        assert_eq!(command, add_http_frontend);
    }

    #[test]
    fn http_front_crash_test2() {
        let raw_json = r#"{"cluster_id": "aa", "hostname": "cltdl.fr", "path": {"kind": 0, "value": "something"}, "position": "TREE", "address": "127.0.0.1:4242"}"#;
        let parsed_front: HttpFrontend =
            serde_json::from_str(raw_json).expect("could not parse json");
        println!("{parsed_front:?}");
        let expected_front = HttpFrontend {
            cluster_id: Some(String::from("aa")),
            hostname: String::from("cltdl.fr"),
            path: PathRule::prefix(String::from("something")),
            method: None,
            address: "127.0.0.1:4242".parse().unwrap(),
            position: RulePosition::Tree,
            tags: None,
        };
        assert_eq!(parsed_front, expected_front);
    }
}
