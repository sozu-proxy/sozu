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
            AddBackend, ClientAuthMode, ClientAuthPolicy, HttpsListenerConfig, InitialState,
            IpAddress, LoadBalancingAlgorithms, PathRuleKind, RedirectPolicy, Request,
            RequestHttpFrontend, RulePosition, SocketAddress, Uint128, UpdateHttpsListenerConfig,
            WorkerRequest, ip_address, request::RequestType,
        },
        display::format_request_type,
    },
    response::HttpFrontend,
    state::ClusterId,
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
            RequestType::AddHttpFrontend(_)
            | RequestType::AddHttpFrontendWithSilentClose(_)
            | RequestType::RemoveHttpFrontend(_)
            | RequestType::AddHttpFrontendWithHeaderPredicates(_)
            | RequestType::RemoveHttpFrontendWithHeaderPredicates(_) => {
                proxy_destination.to_http_proxy = true
            }

            RequestType::AddHttpsFrontend(_)
            | RequestType::AddHttpsFrontendWithSilentClose(_)
            | RequestType::RemoveHttpsFrontend(_)
            | RequestType::AddHttpsFrontendWithHeaderPredicates(_)
            | RequestType::RemoveHttpsFrontendWithHeaderPredicates(_)
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
            | RequestType::AddUnixBackend(_)
            | RequestType::RemoveCluster(_)
            | RequestType::RemoveBackend(_)
            | RequestType::SetHealthCheck(_)
            | RequestType::SetProtectedHealthCheck(_)
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
            | RequestType::QueryMaxConnectionsPerIp(_)
            | RequestType::SetMaxConnectionsPerSubnet(_)
            | RequestType::QueryMaxConnectionsPerSubnet(_)
            | RequestType::SetMaxConnectionAttempts(_)
            | RequestType::CloseSessions(_)
            // handled by the worker's protected health check protocol before
            // this call, which only a proxy-bound order reaches
            | RequestType::QueryProtectedHealthCheckSupport(_)
            | RequestType::ProtectedHealthCheckOrder(_) => {}

            // the Add***Listener / Update***Listener and other Listener orders will be
            // handled separately by the notify_proxys function, so we don't give them
            // destinations here
            RequestType::AddHttpsListener(_)
            | RequestType::AddHttpsListenerWithClientAuth(_)
            | RequestType::AddHttpListener(_)
            | RequestType::AddTcpListener(_)
            | RequestType::AddUdpListener(_)
            | RequestType::UpdateHttpListener(_)
            | RequestType::UpdateHttpsListener(_)
            | RequestType::UpdateHttpsListenerWithClientAuth(_)
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
                            | RequestType::AddUnixBackend(_)
                            | RequestType::RemoveCluster(_)
                            | RequestType::RemoveBackend(_)
                            | RequestType::SetHealthCheck(_)
                            | RequestType::SetProtectedHealthCheck(_)
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

impl HttpsListenerConfig {
    /// True when the listener asks for a client certificate.
    ///
    /// Any value other than `CLIENT_AUTH_NONE` counts, including one this
    /// build does not know: it must travel on the verb a pre-mTLS peer
    /// cannot decode, and the worker then rejects it when it builds the
    /// listener.
    pub fn requests_client_auth(&self) -> bool {
        self.client_auth
            .is_some_and(|mode| mode != ClientAuthMode::ClientAuthNone as i32)
    }

    /// The listener's client authentication policy, as a patch would carry it.
    pub fn client_auth_policy(&self) -> ClientAuthPolicy {
        ClientAuthPolicy {
            client_auth: self.client_auth,
            client_ca_certificates: self.client_ca_certificates.clone(),
            client_ca_crls: self.client_ca_crls.clone(),
        }
    }

    /// Replace the client authentication policy with `policy`, all three
    /// fields at once: a CA or CRL the policy leaves out is dropped. The main
    /// process and the workers both apply a patch through this, so their
    /// copies of the listener cannot diverge.
    pub fn apply_client_auth_policy(&mut self, policy: &ClientAuthPolicy) {
        self.client_auth = policy.client_auth;
        self.client_ca_certificates = policy.client_ca_certificates.clone();
        self.client_ca_crls = policy.client_ca_crls.clone();
    }
}

impl RequestType {
    /// The request that adds this HTTPS listener, on the verb its client
    /// authentication policy calls for.
    ///
    /// A listener that asks for a client certificate goes out as
    /// `AddHttpsListenerWithClientAuth`, which a worker or main process that
    /// predates mutual TLS cannot decode, so it fails the request instead of
    /// building the listener without client authentication. Any other
    /// listener keeps `AddHttpsListener`, which every version understands.
    pub fn add_https_listener(listener: HttpsListenerConfig) -> Self {
        if listener.requests_client_auth() {
            RequestType::AddHttpsListenerWithClientAuth(listener)
        } else {
            RequestType::AddHttpsListener(listener)
        }
    }

    /// The request that applies this HTTPS listener patch: a patch that
    /// replaces the client authentication policy goes out as
    /// `UpdateHttpsListenerWithClientAuth`, which a worker that predates the
    /// policy field cannot decode, so it fails the patch instead of keeping
    /// the CA and CRLs it had. Any other patch keeps `UpdateHttpsListener`.
    pub fn update_https_listener(patch: UpdateHttpsListenerConfig) -> Self {
        if patch.client_auth_policy.is_some() {
            RequestType::UpdateHttpsListenerWithClientAuth(patch)
        } else {
            RequestType::UpdateHttpsListener(patch)
        }
    }

    /// The request that adds this backend, on the verb its endpoint calls for.
    ///
    /// A backend with a `unix_path` goes out as `AddUnixBackend`, which a
    /// worker or main process that predates UNIX backends cannot decode: it
    /// sees no known request, so it never registers the backend under its
    /// identity `address` nor dials that over TCP, and it sends no response.
    /// Any other backend keeps `AddBackend`, which every version understands.
    pub fn add_backend(backend: AddBackend) -> Self {
        if backend.unix_path.is_some() {
            RequestType::AddUnixBackend(backend)
        } else {
            RequestType::AddBackend(backend)
        }
    }

    /// The same request, with a backend addition, an HTTPS listener addition
    /// or patch, or an HTTP(S) frontend addition or removal moved to the verb
    /// [`RequestType::add_backend`], [`RequestType::add_https_listener`],
    /// [`RequestType::update_https_listener`],
    /// [`RequestType::add_http_frontend`],
    /// [`RequestType::remove_http_frontend`],
    /// [`RequestType::add_https_frontend`] or
    /// [`RequestType::remove_https_frontend`] picks for it. Every other
    /// request is returned unchanged.
    ///
    /// The main process applies this to every request it fans out to the
    /// workers, so a client that sends client authentication or a `unix_path`
    /// on a historical verb cannot get it silently dropped by a worker that
    /// predates it.
    pub fn into_canonical(self) -> Self {
        match self {
            RequestType::AddBackend(backend) | RequestType::AddUnixBackend(backend) => {
                RequestType::add_backend(backend)
            }
            RequestType::AddHttpsListener(listener)
            | RequestType::AddHttpsListenerWithClientAuth(listener) => {
                RequestType::add_https_listener(listener)
            }
            RequestType::UpdateHttpsListener(patch)
            | RequestType::UpdateHttpsListenerWithClientAuth(patch) => {
                RequestType::update_https_listener(patch)
            }
            RequestType::AddHttpFrontend(front)
            | RequestType::AddHttpFrontendWithSilentClose(front)
            | RequestType::AddHttpFrontendWithHeaderPredicates(front) => {
                RequestType::add_http_frontend(front)
            }
            RequestType::RemoveHttpFrontend(front)
            | RequestType::RemoveHttpFrontendWithHeaderPredicates(front) => {
                RequestType::remove_http_frontend(front)
            }
            RequestType::AddHttpsFrontend(front)
            | RequestType::AddHttpsFrontendWithSilentClose(front)
            | RequestType::AddHttpsFrontendWithHeaderPredicates(front) => {
                RequestType::add_https_frontend(front)
            }
            RequestType::RemoveHttpsFrontend(front)
            | RequestType::RemoveHttpsFrontendWithHeaderPredicates(front) => {
                RequestType::remove_https_frontend(front)
            }
            other => other,
        }
    }

    /// The request that adds this HTTP frontend, on the verb it calls for.
    ///
    /// A frontend with header predicates goes out as
    /// `AddHttpFrontendWithHeaderPredicates`, which a worker that predates
    /// header routing cannot decode, so it fails the request instead of
    /// installing the frontend without its guard. Otherwise a frontend whose
    /// redirect policy is `SILENT_CLOSE` goes out as
    /// `AddHttpFrontendWithSilentClose`, which a worker that predates the
    /// policy cannot decode, so it fails the request instead of resolving the
    /// unknown policy to `FORWARD` and forwarding the traffic. Any other
    /// frontend keeps `AddHttpFrontend`, which every version understands.
    pub fn add_http_frontend(front: RequestHttpFrontend) -> Self {
        if !front.header_predicates.is_empty() {
            RequestType::AddHttpFrontendWithHeaderPredicates(front)
        } else if front.redirect == Some(RedirectPolicy::SilentClose as i32) {
            RequestType::AddHttpFrontendWithSilentClose(front)
        } else {
            RequestType::AddHttpFrontend(front)
        }
    }

    /// As [`RequestType::add_http_frontend`], for the removal. An old worker
    /// would otherwise remove the unpredicated sibling sharing the key.
    pub fn remove_http_frontend(front: RequestHttpFrontend) -> Self {
        if front.header_predicates.is_empty() {
            RequestType::RemoveHttpFrontend(front)
        } else {
            RequestType::RemoveHttpFrontendWithHeaderPredicates(front)
        }
    }

    /// As [`RequestType::add_http_frontend`], for an HTTPS frontend.
    pub fn add_https_frontend(front: RequestHttpFrontend) -> Self {
        if !front.header_predicates.is_empty() {
            RequestType::AddHttpsFrontendWithHeaderPredicates(front)
        } else if front.redirect == Some(RedirectPolicy::SilentClose as i32) {
            RequestType::AddHttpsFrontendWithSilentClose(front)
        } else {
            RequestType::AddHttpsFrontend(front)
        }
    }

    /// As [`RequestType::remove_http_frontend`], for an HTTPS frontend.
    pub fn remove_https_frontend(front: RequestHttpFrontend) -> Self {
        if front.header_predicates.is_empty() {
            RequestType::RemoveHttpsFrontend(front)
        } else {
            RequestType::RemoveHttpsFrontendWithHeaderPredicates(front)
        }
    }
}

impl Request {
    /// [`RequestType::into_canonical`] applied to the request type.
    pub fn into_canonical(self) -> Self {
        Request {
            request_type: self.request_type.map(RequestType::into_canonical),
        }
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
            cluster_id: self.cluster_id.map(ClusterId::from),
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
            header_predicates: self.header_predicates,
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
            frontend.cluster_id.as_deref(),
            requested_cluster_id.as_deref(),
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
            Some(method) => write!(f, "{s};{method}")?,
            None => write!(f, "{s}")?,
        }

        // Header predicates are part of the frontend's identity
        // (sozu-proxy/sozu#1253): a canary frontend that differs from its
        // base only by a predicate must not collide with it in
        // `ConfigState`. Nothing is appended without predicates, so every
        // pre-existing key is unchanged. Names compare case-insensitively
        // and the list is a conjunction, so the key lowercases names and
        // sorts and deduplicates entries, as the router does: two spellings of one rule share one key. Values
        // are `Debug`-quoted so a `"` or `;` inside one cannot forge a
        // boundary.
        if !self.header_predicates.is_empty() {
            let mut predicates: Vec<(String, i32, Option<&str>)> = self
                .header_predicates
                .iter()
                .map(|predicate| {
                    (
                        predicate.name.to_ascii_lowercase(),
                        predicate.kind,
                        predicate.value.as_deref(),
                    )
                })
                .collect();
            predicates.sort();
            predicates.dedup();
            for (name, kind, value) in predicates {
                write!(f, ";H{kind}:{name:?}")?;
                if let Some(value) = value {
                    write!(f, "={value:?}")?;
                }
            }
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use prost::Message;

    use super::*;
    use crate::proto::command::{HeaderMatchKind, HeaderPredicate};

    fn https_listener(client_auth: Option<i32>) -> HttpsListenerConfig {
        HttpsListenerConfig {
            address: SocketAddress::new_v4(127, 0, 0, 1, 8443),
            client_auth,
            client_ca_certificates: vec!["CLIENT CA PEM".to_owned()],
            ..Default::default()
        }
    }

    const NONE: i32 = ClientAuthMode::ClientAuthNone as i32;
    const OPTIONAL: i32 = ClientAuthMode::ClientAuthOptional as i32;
    const REQUIRED: i32 = ClientAuthMode::ClientAuthRequired as i32;

    #[test]
    fn add_https_listener_picks_the_verb_from_the_client_auth_mode() {
        for client_auth in [None, Some(NONE)] {
            assert!(
                matches!(
                    RequestType::add_https_listener(https_listener(client_auth)),
                    RequestType::AddHttpsListener(_)
                ),
                "a listener without client auth ({client_auth:?}) keeps the verb every worker decodes"
            );
        }
        // 999 names no mode: it must not travel on the verb an old worker
        // would decode and silently strip, the worker rejects it at build.
        for client_auth in [Some(OPTIONAL), Some(REQUIRED), Some(999)] {
            assert!(
                matches!(
                    RequestType::add_https_listener(https_listener(client_auth)),
                    RequestType::AddHttpsListenerWithClientAuth(_)
                ),
                "a listener asking for a client certificate ({client_auth:?}) needs the mTLS verb"
            );
        }
    }

    /// A worker that predates `RedirectPolicy::SilentClose` resolves the
    /// unknown enum value to `Forward` and would forward the traffic, so a
    /// silent-close frontend must leave on a verb such a worker cannot decode.
    #[test]
    fn silent_close_frontends_travel_on_their_own_verb() {
        let front = |redirect: Option<RedirectPolicy>| RequestHttpFrontend {
            hostname: "example.com".to_owned(),
            redirect: redirect.map(|policy| policy as i32),
            ..Default::default()
        };
        let silent = front(Some(RedirectPolicy::SilentClose));
        assert_eq!(
            RequestType::AddHttpFrontend(silent.clone()).into_canonical(),
            RequestType::AddHttpFrontendWithSilentClose(silent.clone()),
            "silent close sent on the historical verb must be moved to its own verb"
        );
        assert_eq!(
            RequestType::AddHttpsFrontend(silent.clone()).into_canonical(),
            RequestType::AddHttpsFrontendWithSilentClose(silent.clone()),
        );
        for policy in [
            None,
            Some(RedirectPolicy::Forward),
            Some(RedirectPolicy::Unauthorized),
        ] {
            let plain = front(policy);
            assert_eq!(
                RequestType::AddHttpFrontendWithSilentClose(plain.clone()).into_canonical(),
                RequestType::AddHttpFrontend(plain.clone()),
                "a frontend that is not silent close keeps the verb every worker decodes"
            );
            assert_eq!(
                RequestType::AddHttpsFrontendWithSilentClose(plain.clone()).into_canonical(),
                RequestType::AddHttpsFrontend(plain),
            );
        }
    }

    /// The silent-close verbs sit on tags 70 and 71. Tags 66-69 belong to
    /// the header-predicate verbs and 72 to `AddUnixBackend`, and a
    /// worker that predates these tags sees an unknown `oneof` member, so the
    /// request carries no route there instead of a forwarding one.
    #[test]
    fn silent_close_verbs_use_their_allocated_wire_tags() {
        // A 2.3.1 worker drops the silent-close verbs as unknown members and
        // keeps decoding the historical verbs of a non-silent frontend.
        let silent = RequestHttpFrontend {
            hostname: "example.com".to_owned(),
            redirect: Some(RedirectPolicy::SilentClose as i32),
            ..Default::default()
        };
        assert_eq!(
            decode_on_a_legacy_worker(RequestType::add_http_frontend(silent.clone())),
            None
        );
        assert_eq!(
            decode_on_a_legacy_worker(RequestType::add_https_frontend(silent.clone())),
            None
        );
        let plain = RequestHttpFrontend {
            redirect: Some(RedirectPolicy::Forward as i32),
            ..silent
        };
        assert_eq!(
            decode_on_a_legacy_worker(RequestType::add_http_frontend(plain.clone())),
            Some(LegacyRequestType::AddHttpFrontend(plain.clone()))
        );
        assert_eq!(
            decode_on_a_legacy_worker(RequestType::add_https_frontend(plain.clone())),
            Some(LegacyRequestType::AddHttpsFrontend(plain))
        );

        let front = RequestHttpFrontend {
            hostname: "example.com".to_owned(),
            redirect: Some(RedirectPolicy::SilentClose as i32),
            ..Default::default()
        };
        let encode = |request_type| {
            Request {
                request_type: Some(request_type),
            }
            .encode_to_vec()
        };
        // key = (tag << 3) | length-delimited(2), as a varint
        assert_eq!(
            &encode(RequestType::AddHttpFrontendWithSilentClose(front.clone()))[..2],
            &[0xB2, 0x04],
            "tag 70"
        );
        assert_eq!(
            &encode(RequestType::AddHttpsFrontendWithSilentClose(front.clone()))[..2],
            &[0xBA, 0x04],
            "tag 71"
        );
        // the legacy verbs keep their historical tags
        let legacy = encode(RequestType::AddHttpFrontend(front));
        assert_ne!(&legacy[..2], &[0xB2, 0x04]);
    }

    #[test]
    fn into_canonical_moves_https_listeners_to_their_verb_both_ways() {
        let required = https_listener(Some(REQUIRED));
        assert_eq!(
            RequestType::AddHttpsListener(required.clone()).into_canonical(),
            RequestType::AddHttpsListenerWithClientAuth(required.clone()),
            "client auth sent on the historical verb must be moved to the mTLS verb"
        );
        assert_eq!(
            RequestType::AddHttpsListenerWithClientAuth(required.clone()).into_canonical(),
            RequestType::AddHttpsListenerWithClientAuth(required),
        );

        let none = https_listener(None);
        assert_eq!(
            RequestType::AddHttpsListenerWithClientAuth(none.clone()).into_canonical(),
            RequestType::AddHttpsListener(none.clone()),
            "a listener without client auth goes back to the verb every worker decodes"
        );
        assert_eq!(
            Request::from(RequestType::AddHttpsListener(none.clone())).into_canonical(),
            Request::from(RequestType::AddHttpsListener(none)),
        );

        let other = RequestType::SoftStop(Default::default());
        assert_eq!(other.clone().into_canonical(), other);
        assert_eq!(
            Request { request_type: None }.into_canonical(),
            Request { request_type: None }
        );
    }

    /// The `WorkerRequest` schema of a worker that predates mutual TLS, cut
    /// down to what matters here: the envelope, and the one verb such a
    /// worker had for adding an HTTPS listener. prost skips every tag a
    /// schema does not declare, exactly as it does for the verbs elided here.
    #[derive(Clone, PartialEq, Message)]
    struct LegacyWorkerRequest {
        #[prost(string, tag = "1")]
        id: String,
        #[prost(message, optional, tag = "2")]
        content: Option<LegacyRequest>,
    }

    #[derive(Clone, PartialEq, Message)]
    struct LegacyRequest {
        #[prost(oneof = "LegacyRequestType", tags = "15, 17, 27, 48")]
        request_type: Option<LegacyRequestType>,
    }

    #[derive(Clone, PartialEq, prost::Oneof)]
    enum LegacyRequestType {
        /// `add_http_frontend`, understood by every worker.
        #[prost(message, tag = "15")]
        AddHttpFrontend(RequestHttpFrontend),
        /// `add_https_frontend`, understood by every worker.
        #[prost(message, tag = "17")]
        AddHttpsFrontend(RequestHttpFrontend),
        /// `add_https_listener`, holding an `HttpsListenerConfig` that has no
        /// client authentication field.
        #[prost(message, tag = "27")]
        AddHttpsListener(LegacyHttpsListenerConfig),
        /// `update_https_listener`, holding an `UpdateHttpsListenerConfig`
        /// that has no client authentication policy field.
        #[prost(message, tag = "48")]
        UpdateHttpsListener(LegacyHttpsListenerConfig),
    }

    #[derive(Clone, PartialEq, Message)]
    struct LegacyHttpsListenerConfig {
        #[prost(message, optional, tag = "1")]
        address: Option<SocketAddress>,
    }

    fn decode_on_a_legacy_worker(request: RequestType) -> Option<LegacyRequestType> {
        let bytes = WorkerRequest::new("ID-1".to_owned(), request.into()).encode_to_vec();
        let legacy = LegacyWorkerRequest::decode(bytes.as_slice())
            .expect("a legacy worker decodes the envelope of every request");
        assert_eq!(legacy.id, "ID-1");
        legacy
            .content
            .expect("the envelope always carries a request")
            .request_type
    }

    /// A main process that knows mutual TLS talking to a worker that does
    /// not, the state between `UpgradeMain` and the replacement of the last
    /// old worker.
    #[test]
    fn a_worker_that_predates_mtls_cannot_decode_a_listener_that_requires_it() {
        let required = https_listener(Some(REQUIRED));

        // The failure mode the dedicated verb exists for: on the historical
        // verb, the old worker sees a plain HTTPS listener, client auth gone.
        assert!(
            matches!(
                decode_on_a_legacy_worker(RequestType::AddHttpsListener(required.clone())),
                Some(LegacyRequestType::AddHttpsListener(_))
            ),
            "precondition: the historical verb decodes on an old worker without its client auth"
        );

        // What the main process actually sends: nothing the old worker can
        // build a listener from, so it fails the request instead.
        assert_eq!(
            decode_on_a_legacy_worker(
                RequestType::AddHttpsListener(required.clone()).into_canonical()
            ),
            None,
            "a listener that requires a client certificate must be undecodable by an old worker"
        );
        assert_eq!(
            decode_on_a_legacy_worker(RequestType::add_https_listener(required)),
            None
        );

        // Positive space: a listener without client auth stays readable, so
        // a mixed fleet keeps serving every listener it can serve safely.
        assert_eq!(
            decode_on_a_legacy_worker(RequestType::add_https_listener(https_listener(None))),
            Some(LegacyRequestType::AddHttpsListener(
                LegacyHttpsListenerConfig {
                    address: Some(SocketAddress::new_v4(127, 0, 0, 1, 8443)),
                }
            ))
        );
    }

    fn policy_patch(policy: Option<ClientAuthPolicy>) -> UpdateHttpsListenerConfig {
        UpdateHttpsListenerConfig {
            address: SocketAddress::new_v4(127, 0, 0, 1, 8443),
            client_auth_policy: policy,
            ..Default::default()
        }
    }

    #[test]
    fn update_https_listener_picks_the_verb_from_the_policy_field() {
        assert!(matches!(
            RequestType::update_https_listener(policy_patch(None)),
            RequestType::UpdateHttpsListener(_)
        ));
        // Any policy, even one turning client auth off: a worker that ignored
        // it would keep enforcing, or keep trusting, what the patch replaces.
        for client_auth in [None, Some(NONE), Some(REQUIRED)] {
            let policy = ClientAuthPolicy {
                client_auth,
                ..Default::default()
            };
            assert!(
                matches!(
                    RequestType::update_https_listener(policy_patch(Some(policy))),
                    RequestType::UpdateHttpsListenerWithClientAuth(_)
                ),
                "a patch carrying a policy ({client_auth:?}) needs the mTLS verb"
            );
        }
        let with_policy = policy_patch(Some(ClientAuthPolicy::default()));
        assert_eq!(
            RequestType::UpdateHttpsListener(with_policy.clone()).into_canonical(),
            RequestType::UpdateHttpsListenerWithClientAuth(with_policy)
        );
        assert_eq!(
            RequestType::UpdateHttpsListenerWithClientAuth(policy_patch(None)).into_canonical(),
            RequestType::UpdateHttpsListener(policy_patch(None))
        );
    }

    #[test]
    fn a_worker_that_predates_mtls_cannot_decode_a_client_auth_policy_patch() {
        let refresh = policy_patch(Some(https_listener(Some(REQUIRED)).client_auth_policy()));
        assert!(
            matches!(
                decode_on_a_legacy_worker(RequestType::UpdateHttpsListener(refresh.clone())),
                Some(LegacyRequestType::UpdateHttpsListener(_))
            ),
            "precondition: the historical verb decodes on an old worker without its policy"
        );
        assert_eq!(
            decode_on_a_legacy_worker(RequestType::update_https_listener(refresh)),
            None,
            "a patch replacing the client auth policy must be undecodable by an old worker"
        );
        assert!(matches!(
            decode_on_a_legacy_worker(RequestType::update_https_listener(policy_patch(None))),
            Some(LegacyRequestType::UpdateHttpsListener(_))
        ));
    }

    #[test]
    fn applying_a_client_auth_policy_replaces_all_of_it() {
        let mut listener = https_listener(Some(REQUIRED));
        listener.client_ca_crls = vec!["OLD CRL".to_owned()];
        let policy = ClientAuthPolicy {
            client_auth: Some(OPTIONAL),
            client_ca_certificates: vec!["NEW CA".to_owned()],
            client_ca_crls: vec![],
        };
        listener.apply_client_auth_policy(&policy);
        assert_eq!(listener.client_auth_policy(), policy);
        assert!(
            listener.client_ca_crls.is_empty(),
            "a CRL the policy leaves out is dropped, not kept"
        );
    }

    /// `WorkerRequest` of a worker that predates header routing: the four
    /// historical frontend verbs, holding a `RequestHttpFrontend` that has no
    /// `header_predicates` field.
    #[derive(Clone, PartialEq, Message)]
    struct LegacyFrontendWorkerRequest {
        #[prost(string, tag = "1")]
        id: String,
        #[prost(message, optional, tag = "2")]
        content: Option<LegacyFrontendRequest>,
    }

    #[derive(Clone, PartialEq, Message)]
    struct LegacyFrontendRequest {
        #[prost(oneof = "LegacyFrontendRequestType", tags = "15, 16, 17, 18")]
        request_type: Option<LegacyFrontendRequestType>,
    }

    #[derive(Clone, PartialEq, prost::Oneof)]
    enum LegacyFrontendRequestType {
        #[prost(message, tag = "15")]
        AddHttp(LegacyHttpFrontend),
        #[prost(message, tag = "16")]
        RemoveHttp(LegacyHttpFrontend),
        #[prost(message, tag = "17")]
        AddHttps(LegacyHttpFrontend),
        #[prost(message, tag = "18")]
        RemoveHttps(LegacyHttpFrontend),
    }

    #[derive(Clone, PartialEq, Message)]
    struct LegacyHttpFrontend {
        #[prost(string, tag = "3")]
        hostname: String,
    }

    fn decode_frontend_on_a_legacy_worker(
        request: RequestType,
    ) -> Option<LegacyFrontendRequestType> {
        let bytes = WorkerRequest::new("ID-1".to_owned(), request.into()).encode_to_vec();
        LegacyFrontendWorkerRequest::decode(bytes.as_slice())
            .expect("a legacy worker decodes the envelope of every request")
            .content
            .expect("the envelope always carries a request")
            .request_type
    }

    fn frontend(with_predicate: bool) -> RequestHttpFrontend {
        RequestHttpFrontend {
            cluster_id: Some("canary".to_owned()),
            address: SocketAddress::new_v4(127, 0, 0, 1, 8080),
            hostname: "example.com".to_owned(),
            header_predicates: if with_predicate {
                vec![HeaderPredicate {
                    name: "X-Canary".to_owned(),
                    kind: HeaderMatchKind::Present as i32,
                    value: None,
                }]
            } else {
                Vec::new()
            },
            ..Default::default()
        }
    }

    #[test]
    fn frontend_verbs_follow_the_header_predicates() {
        let guarded = frontend(true);
        let plain = frontend(false);
        assert!(matches!(
            RequestType::add_http_frontend(guarded.clone()),
            RequestType::AddHttpFrontendWithHeaderPredicates(_)
        ));
        assert!(matches!(
            RequestType::remove_http_frontend(guarded.clone()),
            RequestType::RemoveHttpFrontendWithHeaderPredicates(_)
        ));
        assert!(matches!(
            RequestType::add_https_frontend(guarded.clone()),
            RequestType::AddHttpsFrontendWithHeaderPredicates(_)
        ));
        assert!(matches!(
            RequestType::remove_https_frontend(guarded.clone()),
            RequestType::RemoveHttpsFrontendWithHeaderPredicates(_)
        ));
        // routes without predicates keep the historical verbs
        assert_eq!(
            RequestType::add_http_frontend(plain.clone()),
            RequestType::AddHttpFrontend(plain.clone())
        );
        assert_eq!(
            RequestType::remove_https_frontend(plain.clone()),
            RequestType::RemoveHttpsFrontend(plain.clone())
        );
        // whatever verb the client used, the fan-out sends the canonical one
        assert_eq!(
            RequestType::AddHttpFrontend(guarded.clone()).into_canonical(),
            RequestType::AddHttpFrontendWithHeaderPredicates(guarded.clone())
        );
        assert_eq!(
            RequestType::RemoveHttpsFrontendWithHeaderPredicates(plain.clone()).into_canonical(),
            RequestType::RemoveHttpsFrontend(plain)
        );
    }

    #[test]
    fn a_worker_that_predates_header_routing_cannot_decode_a_guarded_frontend() {
        let guarded = frontend(true);
        // The failure mode the dedicated verbs exist for: on a historical
        // verb the old worker installs the route and drops its guard.
        assert!(
            matches!(
                decode_frontend_on_a_legacy_worker(RequestType::AddHttpFrontend(guarded.clone())),
                Some(LegacyFrontendRequestType::AddHttp(_))
            ),
            "precondition: the historical verb decodes on an old worker without its guard"
        );
        for request in [
            RequestType::add_http_frontend(guarded.clone()),
            RequestType::remove_http_frontend(guarded.clone()),
            RequestType::add_https_frontend(guarded.clone()),
            RequestType::remove_https_frontend(guarded.clone()),
            RequestType::AddHttpsFrontend(guarded.clone()).into_canonical(),
            RequestType::RemoveHttpFrontend(guarded.clone()).into_canonical(),
        ] {
            assert_eq!(
                decode_frontend_on_a_legacy_worker(request),
                None,
                "a guarded frontend must be undecodable by an old worker"
            );
        }
        // Positive space: unguarded frontends stay readable in a mixed fleet.
        assert_eq!(
            decode_frontend_on_a_legacy_worker(RequestType::add_http_frontend(frontend(false))),
            Some(LegacyFrontendRequestType::AddHttp(LegacyHttpFrontend {
                hostname: "example.com".to_owned()
            }))
        );
    }

    /// The tags are wire contract: 65 belongs to `close_sessions`, 70-72 to
    /// other pending verbs, so a drift here collides with them.
    #[test]
    fn header_predicate_verbs_use_the_allocated_wire_tags() {
        let guarded = frontend(true);
        for (request, tag) in [
            (RequestType::add_http_frontend(guarded.clone()), 66u32),
            (RequestType::remove_http_frontend(guarded.clone()), 67),
            (RequestType::add_https_frontend(guarded.clone()), 68),
            (RequestType::remove_https_frontend(guarded.clone()), 69),
        ] {
            let bytes = Request::from(request).encode_to_vec();
            // first byte is the key of the oneof field: (tag << 3) | length-delimited
            let (key, _) = prost::encoding::decode_varint(&mut bytes.as_slice())
                .map(|k| (k, ()))
                .expect("a request starts with a field key");
            assert_eq!(key >> 3, u64::from(tag));
            assert_eq!(key & 7, 2);
        }
    }
}
