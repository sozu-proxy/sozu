use std::{
    error,
    fmt::{self, Display},
    fs::File,
    io::{BufReader, Read},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    str::FromStr,
};

use prost::{DecodeError, Message};
use rusty_ulid::Ulid;

use crate::{
    proto::{
        command::{
            ip_address, request::RequestType, InitialState, IpAddress, LoadBalancingAlgorithms,
            PathRuleKind, Request, RequestHttpFrontend, RulePosition, SocketAddress, Uint128,
            WorkerRequest,
        },
        display::format_request_type,
    },
    response::HttpFrontend,
};

#[derive(thiserror::Error, Debug)]
pub enum RequestError {
    #[error("invalid value {value} for field '{name}'")]
    InvalidValue { name: String, value: i32 },
    #[error("Could not read requests from file: {0}")]
    ReadFile(std::io::Error),
    #[error("Could not decode requests: {0}")]
    Decode(DecodeError),
}

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
            | RequestType::QueryCertificatesFromWorkers(_)
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
            | RequestType::Status(_) => {
                proxy_destination.to_http_proxy = true;
                proxy_destination.to_https_proxy = true;
                proxy_destination.to_tcp_proxy = true;
            }

            // handled at worker level prior to this call
            RequestType::ConfigureMetrics(_)
            | RequestType::QueryMetrics(_)
            | RequestType::Logging(_)
            | RequestType::QueryClustersHashes(_)
            | RequestType::QueryClusterById(_)
            | RequestType::QueryClustersByDomain(_) => {}

            // the Add***Listener and other Listener orders will be handled separately
            // by the notify_proxys function, so we don't give them destinations
            RequestType::AddHttpsListener(_)
            | RequestType::AddHttpListener(_)
            | RequestType::AddTcpListener(_)
            | RequestType::RemoveListener(_)
            | RequestType::ActivateListener(_)
            | RequestType::DeactivateListener(_)
            | RequestType::ReturnListenSockets(_) => {}

            // These won't ever reach a worker anyway
            RequestType::SaveState(_)
            | RequestType::CountRequests(_)
            | RequestType::QueryCertificatesFromTheState(_)
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
        matches!(
            self.request_type,
            Some(RequestType::SoftStop(_)) | Some(RequestType::HardStop(_))
        )
    }

    pub fn short_name(&self) -> &str {
        match &self.request_type {
            Some(request_type) => format_request_type(request_type),
            None => "Unallowed",
        }
    }
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

pub fn read_initial_state_from_file(file: &mut File) -> Result<InitialState, RequestError> {
    let mut buf_reader = BufReader::new(file);
    read_initial_state(&mut buf_reader)
}

pub fn read_initial_state<R: Read>(reader: &mut R) -> Result<InitialState, RequestError> {
    let mut buffer = Vec::new();
    reader
        .read_to_end(&mut buffer)
        .map_err(RequestError::ReadFile)?;

    InitialState::decode(&buffer[..]).map_err(RequestError::Decode)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProxyDestinations {
    pub to_http_proxy: bool,
    pub to_https_proxy: bool,
    pub to_tcp_proxy: bool,
}

impl RequestHttpFrontend {
    /// convert a requested frontend to a usable one by parsing its address
    pub fn to_frontend(self) -> Result<HttpFrontend, RequestError> {
        Ok(HttpFrontend {
            address: self.address.into(),
            cluster_id: self.cluster_id,
            hostname: self.hostname,
            path: self.path,
            method: self.method,
            position: RulePosition::try_from(self.position).map_err(|_| {
                RequestError::InvalidValue {
                    name: "position".to_string(),
                    value: self.position,
                }
            })?,
            tags: Some(self.tags),
        })
    }
}

impl Display for RequestHttpFrontend {
    /// Used to create a unique summary of the frontend, used as a key in maps
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match &PathRuleKind::try_from(self.path.kind) {
            Ok(PathRuleKind::Prefix) => {
                format!("{};{};P{}", self.address, self.hostname, self.path.value)
            }
            Ok(PathRuleKind::Regex) => {
                format!("{};{};R{}", self.address, self.hostname, self.path.value)
            }
            Ok(PathRuleKind::Equals) => {
                format!("{};{};={}", self.address, self.hostname, self.path.value)
            }
            Err(e) => format!("Wrong variant of PathRuleKind: {e}"),
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

impl SocketAddress {
    pub fn new_v4(a: u8, b: u8, c: u8, d: u8, port: u16) -> Self {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(a, b, c, d)), port).into()
    }
}

impl From<SocketAddr> for SocketAddress {
    fn from(socket_addr: SocketAddr) -> SocketAddress {
        let ip_inner = match socket_addr {
            SocketAddr::V4(ip_v4_addr) => ip_address::Inner::V4(u32::from(*ip_v4_addr.ip())),
            SocketAddr::V6(ip_v6_addr) => {
                ip_address::Inner::V6(Uint128::from(u128::from(*ip_v6_addr.ip())))
            }
        };

        SocketAddress {
            port: socket_addr.port() as u32,
            ip: IpAddress {
                inner: Some(ip_inner),
            },
        }
    }
}

impl From<SocketAddress> for SocketAddr {
    fn from(socket_address: SocketAddress) -> Self {
        let port = socket_address.port as u16;

        let ip = match socket_address.ip.inner {
            Some(inner) => match inner {
                ip_address::Inner::V4(v4_value) => IpAddr::V4(Ipv4Addr::from(v4_value)),
                ip_address::Inner::V6(v6_value) => IpAddr::V6(Ipv6Addr::from(u128::from(v6_value))),
            },
            None => IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), // should never happen
        };

        SocketAddr::new(ip, port)
    }
}

impl From<Uint128> for u128 {
    fn from(value: Uint128) -> Self {
        value.low as u128 | ((value.high as u128) << 64)
    }
}

impl From<u128> for Uint128 {
    fn from(value: u128) -> Self {
        let low = value as u64;
        let high = (value >> 64) as u64;
        Uint128 { low, high }
    }
}

impl From<i128> for Uint128 {
    fn from(value: i128) -> Self {
        Uint128::from(value as u128)
    }
}

impl From<Ulid> for Uint128 {
    fn from(value: Ulid) -> Self {
        let (low, high) = value.into();
        Uint128 { low, high }
    }
}

impl From<Uint128> for Ulid {
    fn from(value: Uint128) -> Self {
        let Uint128 { low, high } = value;
        Ulid::from((low, high))
    }
}
