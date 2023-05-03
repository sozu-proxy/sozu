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
