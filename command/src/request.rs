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
            InitialState, IpAddress, LoadBalancingAlgorithms, PathRuleKind, Request,
            RequestHttpFrontend, RulePosition, SocketAddress, Uint128, WorkerRequest, ip_address,
            request::RequestType,
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
    /// determine to which of the four proxies (HTTP, HTTPS, TCP, UDP) a request is destined
    pub fn get_destinations(&self) -> ProxyDestinations {
        let mut proxy_destination = ProxyDestinations {
            to_http_proxy: false,
            to_https_proxy: false,
            to_tcp_proxy: false,
            to_udp_proxy: false,
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

            RequestType::AddUdpFrontend(_) | RequestType::RemoveUdpFrontend(_) => {
                proxy_destination.to_udp_proxy = true
            }

            RequestType::AddCluster(_)
            | RequestType::AddBackend(_)
            | RequestType::RemoveCluster(_)
            | RequestType::RemoveBackend(_)
            | RequestType::SetHealthCheck(_)
            | RequestType::RemoveHealthCheck(_)
            | RequestType::SoftStop(_)
            | RequestType::HardStop(_)
            | RequestType::Status(_) => {
                proxy_destination.to_http_proxy = true;
                proxy_destination.to_https_proxy = true;
                proxy_destination.to_tcp_proxy = true;
                proxy_destination.to_udp_proxy = true;
            }

            // handled at worker level prior to this call
            RequestType::ConfigureMetrics(_)
            | RequestType::SetMetricDetail(_)
            | RequestType::QueryMetrics(_)
            | RequestType::Logging(_)
            | RequestType::QueryClustersHashes(_)
            | RequestType::QueryClusterById(_)
            | RequestType::QueryClustersByDomain(_)
            | RequestType::SetMaxConnectionsPerIp(_)
            | RequestType::QueryMaxConnectionsPerIp(_) => {}

            // the Add***Listener / Update***Listener and other Listener orders will be
            // handled separately by the notify_proxys function, so we don't give them
            // destinations here
            RequestType::AddHttpsListener(_)
            | RequestType::AddHttpListener(_)
            | RequestType::AddTcpListener(_)
            | RequestType::AddUdpListener(_)
            | RequestType::UpdateHttpListener(_)
            | RequestType::UpdateHttpsListener(_)
            | RequestType::UpdateTcpListener(_)
            | RequestType::UpdateUdpListener(_)
            | RequestType::RemoveListener(_)
            | RequestType::ActivateListener(_)
            | RequestType::DeactivateListener(_)
            | RequestType::ReturnListenSockets(_) => {}

            // These won't ever reach a worker anyway
            RequestType::SaveState(_)
            | RequestType::CountRequests(_)
            | RequestType::QueryCertificatesFromTheState(_)
            | RequestType::QueryHealthChecks(_)
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

        // POST: HTTP-frontend orders route to the HTTP proxy ONLY, HTTPS /
        // certificate orders to the HTTPS proxy ONLY, and TCP-frontend orders
        // to the TCP proxy ONLY — a frontend order must never fan out across
        // protocol planes (that would double-apply the order). Cluster-wide
        // and broadcast orders (AddCluster, SoftStop, …) legitimately target
        // all three, so we only assert the single-plane exclusivity here.
        debug_assert!(
            !(proxy_destination.to_http_proxy
                && proxy_destination.to_https_proxy
                && proxy_destination.to_tcp_proxy)
                || matches!(
                    self.request_type,
                    Some(
                        RequestType::AddCluster(_)
                            | RequestType::AddBackend(_)
                            | RequestType::RemoveCluster(_)
                            | RequestType::RemoveBackend(_)
                            | RequestType::SetHealthCheck(_)
                            | RequestType::RemoveHealthCheck(_)
                            | RequestType::SoftStop(_)
                            | RequestType::HardStop(_)
                            | RequestType::Status(_)
                    )
                ),
            "only cluster-wide / broadcast orders may target all three proxy planes"
        );
        // POST: a None request_type carries no destination at all.
        debug_assert!(
            self.request_type.is_some()
                || (!proxy_destination.to_http_proxy
                    && !proxy_destination.to_https_proxy
                    && !proxy_destination.to_tcp_proxy),
            "a request without a request_type must have no proxy destination"
        );
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
    pub to_udp_proxy: bool,
}

impl RequestHttpFrontend {
    /// convert a requested frontend to a usable one by parsing its address
    pub fn to_frontend(self) -> Result<HttpFrontend, RequestError> {
        let requested_hostname = self.hostname.clone();
        let requested_cluster_id = self.cluster_id.clone();
        let frontend = HttpFrontend {
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
            redirect: self.redirect,
            redirect_scheme: self.redirect_scheme,
            redirect_template: self.redirect_template,
            rewrite_host: self.rewrite_host,
            rewrite_path: self.rewrite_path,
            rewrite_port: self.rewrite_port,
            required_auth: self.required_auth,
            headers: self.headers,
            hsts: self.hsts,
        };

        // POST: routing identity (hostname + cluster_id) is carried through
        // unchanged — only the address is reparsed and the position is mapped
        // through the proto enum. A frontend whose hostname or cluster shifted
        // here would route traffic to the wrong place.
        debug_assert_eq!(
            frontend.hostname, requested_hostname,
            "hostname must survive the frontend conversion"
        );
        debug_assert_eq!(
            frontend.cluster_id, requested_cluster_id,
            "cluster_id must survive the frontend conversion"
        );
        Ok(frontend)
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
            "hrw" => Ok(LoadBalancingAlgorithms::Hrw),
            "maglev" => Ok(LoadBalancingAlgorithms::Maglev),
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

        let encoded = SocketAddress {
            port: socket_addr.port() as u32,
            ip: IpAddress {
                inner: Some(ip_inner),
            },
        };

        // POST: the port widens losslessly (u16 → u32) and the proto address
        // family matches the source family — a V4 SocketAddr must never encode
        // as a V6 inner and vice versa, or the reverse `From` would synthesize
        // the wrong address.
        debug_assert_eq!(
            encoded.port,
            socket_addr.port() as u32,
            "port must round-trip losslessly into the proto"
        );
        debug_assert_eq!(
            matches!(encoded.ip.inner, Some(ip_address::Inner::V4(_))),
            socket_addr.is_ipv4(),
            "proto IP family must match the source SocketAddr family"
        );
        encoded
    }
}

impl From<SocketAddress> for SocketAddr {
    fn from(socket_address: SocketAddress) -> Self {
        // PRE: a wire-sourced proto port may exceed u16::MAX (16-bit on the
        // wire is carried as a 32-bit field). This is peer/config input, so we
        // narrow rather than panic; the debug_assert only guards our *own*
        // encoders, which never emit an out-of-range port.
        debug_assert!(
            socket_address.port <= u16::MAX as u32,
            "self-encoded proto port must fit in a u16"
        );
        let had_inner = socket_address.ip.inner.is_some();
        let port = socket_address.port as u16;

        let ip = match socket_address.ip.inner {
            Some(inner) => match inner {
                ip_address::Inner::V4(v4_value) => IpAddr::V4(Ipv4Addr::from(v4_value)),
                ip_address::Inner::V6(v6_value) => IpAddr::V6(Ipv6Addr::from(u128::from(v6_value))),
            },
            None => IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), // should never happen
        };

        let decoded = SocketAddr::new(ip, port);
        // POST: a self-encoded proto address always carries an inner IP, so the
        // unspecified-V4 fallback is only ever reached on malformed peer input.
        debug_assert!(
            had_inner || decoded.ip() == IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            "missing inner IP must decode to the unspecified-V4 sentinel"
        );
        decoded
    }
}

impl From<Uint128> for u128 {
    fn from(value: Uint128) -> Self {
        let combined = value.low as u128 | ((value.high as u128) << 64);
        // POST: the two 64-bit halves occupy disjoint bit ranges, so the low
        // half is recoverable as the bottom 64 bits and the high half as the
        // top 64 bits — the pack is bijective.
        debug_assert_eq!(
            combined as u64, value.low,
            "low half must be the bottom 64 bits"
        );
        debug_assert_eq!(
            (combined >> 64) as u64,
            value.high,
            "high half must be the top 64 bits"
        );
        combined
    }
}

impl From<u128> for Uint128 {
    fn from(value: u128) -> Self {
        let low = value as u64;
        let high = (value >> 64) as u64;
        let packed = Uint128 { low, high };
        // POST: splitting then recombining reproduces the original u128 — the
        // split-into-halves and join-from-halves operations are mutual
        // inverses (no bit is lost or duplicated).
        debug_assert_eq!(
            u128::from(Uint128 {
                low: packed.low,
                high: packed.high
            }),
            value,
            "u128 → Uint128 → u128 must round-trip"
        );
        packed
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
        let packed = Uint128 { low, high };
        // POST: the (low, high) tuple is carried verbatim into the proto, so
        // re-reading it reconstructs the same Ulid — the encoding loses no bits.
        debug_assert_eq!(
            Ulid::from((packed.low, packed.high)),
            value,
            "Ulid → Uint128 must preserve all 128 bits"
        );
        packed
    }
}

impl From<Uint128> for Ulid {
    fn from(value: Uint128) -> Self {
        let Uint128 { low, high } = value;
        let ulid = Ulid::from((low, high));
        // POST: the decode is the exact inverse of the encode above — the same
        // halves go back out, so Uint128 → Ulid → Uint128 round-trips.
        debug_assert_eq!(
            Uint128::from(ulid),
            value,
            "Uint128 → Ulid must preserve all 128 bits"
        );
        ulid
    }
}
