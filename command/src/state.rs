use std::{
    collections::{
        BTreeMap, BTreeSet, HashMap, HashSet, btree_map::Entry as BTreeMapEntry,
        hash_map::DefaultHasher,
    },
    fmt,
    fs::File,
    hash::{Hash, Hasher},
    io::Write,
    iter::repeat,
    net::SocketAddr,
};

use prost::{Message, UnknownEnumValue};

use crate::{
    ObjectKind,
    certificate::{CertificateError, Fingerprint, calculate_fingerprint},
    config::validate_sni_pattern,
    proto::{
        command::{
            ActivateListener, AddBackend, AddCertificate, CertificateAndKey, Cluster,
            ClusterInformation, CustomHttpAnswers, DeactivateListener, FrontendFilters,
            HealthChecksList, HttpListenerConfig, HttpsListenerConfig, InitialState,
            ListedFrontends, ListenerType, ListenersList, PathRule, QueryCertificatesFilters,
            RemoveBackend, RemoveCertificate, RemoveListener, ReplaceCertificate, Request,
            RequestCounts, RequestHttpFrontend, RequestTcpFrontend, RequestUdpFrontend,
            SetHealthCheck, SocketAddress, TcpListenerConfig, UdpListenerConfig,
            UpdateHttpListenerConfig, UpdateHttpsListenerConfig, UpdateTcpListenerConfig,
            UpdateUdpListenerConfig, WorkerRequest, request::RequestType,
        },
        display::format_request_type,
    },
    response::{Backend, HttpFrontend, TcpFrontend, UdpFrontend},
};

/// To use throughout Sōzu
pub type ClusterId = String;

#[derive(thiserror::Error)]
pub enum StateError {
    #[error("Request came in empty")]
    EmptyRequest,
    #[error("dispatching this request did not bring any change to the state")]
    NoChange,
    #[error("State can not handle this request")]
    UndispatchableRequest,
    #[error("Did not find {kind:?} with address or id_bytes={}", .id.len())]
    NotFound { kind: ObjectKind, id: String },
    #[error(
        "{kind:?} with id_bytes={} already exists; remove it first, or apply the corresponding update, instead of re-adding it",
        .id.len()
    )]
    Exists { kind: ObjectKind, id: String },
    #[error("Wrong field value: {0}")]
    WrongFieldValue(UnknownEnumValue),
    #[error("Could not add certificate: error_kind={}", certificate_error_kind(.0))]
    AddCertificate(CertificateError),
    #[error("Could not remove certificate: fingerprint_bytes={}", .0.len())]
    RemoveCertificate(String),
    #[error("Could not replace certificate: error_bytes={}", .0.len())]
    ReplaceCertificate(String),
    #[error("Could not convert frontend: frontend_bytes={} error_bytes={}", .frontend.len(), .error.len())]
    FrontendConversion { frontend: String, error: String },
    #[error("Could not write state to file: io_kind={:?} os_code={:?}", .0.kind(), .0.raw_os_error())]
    FileError(std::io::Error),
    #[error("Invalid value for field '{field}': {reason}")]
    InvalidValue {
        field: &'static str,
        reason: &'static str,
    },
    /// Mirrors the worker's `ProxyError::InvalidTcpFrontend`
    /// (`lib/src/tcp.rs`): the master rejects an `AddTcpFrontend` for the
    /// same reasons `validate_new_tcp_front` would, so a route never lands
    /// in `ConfigState` only to be NACKed by every worker on the next
    /// fan-out (sozu-proxy/sozu#1290).
    #[error("invalid TCP frontend for address {address}: reason_bytes={}", .reason.len())]
    InvalidTcpFrontend { address: SocketAddr, reason: String },
}

impl fmt::Debug for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StateError({self})")
    }
}

fn certificate_error_kind(error: &CertificateError) -> &'static str {
    match error {
        CertificateError::ParsePEMCertificate(_) => "parse_pem_certificate",
        CertificateError::ParseX509Certificate(_) => "parse_x509_certificate",
        CertificateError::InvalidTlsVersion(_) => "invalid_tls_version",
        CertificateError::InvalidFingerprint(_) => "invalid_fingerprint",
        CertificateError::LoadFile { .. } => "load_file",
        CertificateError::DecodeError(_) => "decode_error",
    }
}

/// The `ConfigState` represents the state of Sōzu's business, which is to forward traffic
/// from frontends to backends. Hence, it contains all details about:
///
/// - listeners (socket addresses, for TCP and HTTP connections)
/// - frontends (bind to a listener)
/// - backends (to forward connections to)
/// - clusters (routing rules from frontends to backends)
/// - TLS certificates
#[derive(Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigState {
    pub clusters: BTreeMap<ClusterId, Cluster>,
    pub backends: BTreeMap<ClusterId, Vec<Backend>>,
    /// socket address -> HTTP listener
    pub http_listeners: BTreeMap<SocketAddr, HttpListenerConfig>,
    /// socket address -> HTTPS listener
    pub https_listeners: BTreeMap<SocketAddr, HttpsListenerConfig>,
    /// socket address -> TCP listener
    pub tcp_listeners: BTreeMap<SocketAddr, TcpListenerConfig>,
    /// socket address -> UDP listener
    pub udp_listeners: BTreeMap<SocketAddr, UdpListenerConfig>,
    /// HTTP frontends, indexed by a summary of each front's address;hostname;path, for uniqueness.
    /// For example: `"0.0.0.0:8080;lolcatho.st;P/api"`
    pub http_fronts: BTreeMap<String, HttpFrontend>,
    /// indexed by (address, hostname, path)
    pub https_fronts: BTreeMap<String, HttpFrontend>,
    pub tcp_fronts: HashMap<ClusterId, Vec<TcpFrontend>>,
    pub udp_fronts: HashMap<ClusterId, Vec<UdpFrontend>>,
    pub certificates: HashMap<SocketAddr, HashMap<Fingerprint, CertificateAndKey>>,
    /// A census of requests that were received. Name of the request -> number of occurences
    pub request_counts: BTreeMap<String, i32>,
}

impl std::fmt::Debug for ConfigState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let backends_count = self
            .backends
            .values()
            .map(Vec::len)
            .fold(0usize, usize::saturating_add);
        let tcp_frontends_count = self
            .tcp_fronts
            .values()
            .map(Vec::len)
            .fold(0usize, usize::saturating_add);
        let udp_frontends_count = self
            .udp_fronts
            .values()
            .map(Vec::len)
            .fold(0usize, usize::saturating_add);
        let certificates_count = self
            .certificates
            .values()
            .map(HashMap::len)
            .fold(0usize, usize::saturating_add);

        f.debug_struct("ConfigState")
            .field("clusters_count", &self.clusters.len())
            .field("backend_clusters_count", &self.backends.len())
            .field("backends_count", &backends_count)
            .field("http_listeners_count", &self.http_listeners.len())
            .field("https_listeners_count", &self.https_listeners.len())
            .field("tcp_listeners_count", &self.tcp_listeners.len())
            .field("udp_listeners_count", &self.udp_listeners.len())
            .field("http_frontends_count", &self.http_fronts.len())
            .field("https_frontends_count", &self.https_fronts.len())
            .field("tcp_frontend_clusters_count", &self.tcp_fronts.len())
            .field("tcp_frontends_count", &tcp_frontends_count)
            .field("udp_frontend_clusters_count", &self.udp_fronts.len())
            .field("udp_frontends_count", &udp_frontends_count)
            .field("certificate_addresses_count", &self.certificates.len())
            .field("certificates_count", &certificates_count)
            .field("request_counts_count", &self.request_counts.len())
            .finish_non_exhaustive()
    }
}

impl ConfigState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn dispatch(&mut self, request: &Request) -> Result<(), StateError> {
        let request_type = match &request.request_type {
            Some(t) => t,
            None => return Err(StateError::EmptyRequest),
        };

        self.increment_request_count(request);

        let result = match request_type {
            RequestType::AddCluster(cluster) => self.add_cluster(cluster),
            RequestType::RemoveCluster(cluster_id) => self.remove_cluster(cluster_id),
            RequestType::AddHttpListener(listener) => self.add_http_listener(listener),
            RequestType::AddHttpsListener(listener) => self.add_https_listener(listener),
            RequestType::AddTcpListener(listener) => self.add_tcp_listener(listener),
            RequestType::AddUdpListener(listener) => self.add_udp_listener(listener),
            RequestType::RemoveListener(remove) => self.remove_listener(remove),
            RequestType::ActivateListener(activate) => self.activate_listener(activate),
            RequestType::DeactivateListener(deactivate) => self.deactivate_listener(deactivate),
            RequestType::AddHttpFrontend(front) => self.add_http_frontend(front),
            RequestType::RemoveHttpFrontend(front) => self.remove_http_frontend(front),
            RequestType::AddCertificate(add) => self.add_certificate(add),
            RequestType::RemoveCertificate(remove) => self.remove_certificate(remove),
            RequestType::ReplaceCertificate(replace) => self.replace_certificate(replace),
            RequestType::AddHttpsFrontend(front) => self.add_https_frontend(front),
            RequestType::RemoveHttpsFrontend(front) => self.remove_https_frontend(front),
            RequestType::AddTcpFrontend(front) => self.add_tcp_frontend(front),
            RequestType::RemoveTcpFrontend(front) => self.remove_tcp_frontend(front),
            RequestType::AddUdpFrontend(front) => self.add_udp_frontend(front),
            RequestType::RemoveUdpFrontend(front) => self.remove_udp_frontend(front),
            RequestType::AddBackend(add_backend) => self.add_backend(add_backend),
            RequestType::RemoveBackend(backend) => self.remove_backend(backend),
            RequestType::UpdateHttpListener(patch) => self.update_http_listener(patch),
            RequestType::UpdateHttpsListener(patch) => self.update_https_listener(patch),
            RequestType::UpdateTcpListener(patch) => self.update_tcp_listener(patch),
            RequestType::UpdateUdpListener(patch) => self.update_udp_listener(patch),
            RequestType::SetHealthCheck(set) => self.set_health_check(set),
            RequestType::RemoveHealthCheck(cluster_id) => self.remove_health_check(cluster_id),

            // This is to avoid the error message. These request types are
            // worker-only / runtime-only and do not affect the persisted
            // ConfigState (e.g., a worker-side global limit set via
            // SetMaxConnectionsPerIp does NOT survive a worker restart;
            // operators must mirror the change in the TOML to make it
            // sticky).
            RequestType::Logging(_)
            | RequestType::CountRequests(_)
            | RequestType::Status(_)
            | RequestType::SoftStop(_)
            | RequestType::QueryCertificatesFromWorkers(_)
            | RequestType::QueryClusterById(_)
            | RequestType::QueryClustersByDomain(_)
            | RequestType::QueryMetrics(_)
            | RequestType::QueryClustersHashes(_)
            | RequestType::ConfigureMetrics(_)
            | RequestType::SetMetricDetail(_)
            | RequestType::ReturnListenSockets(_)
            | RequestType::SetMaxConnectionsPerIp(_)
            | RequestType::QueryMaxConnectionsPerIp(_)
            | RequestType::HardStop(_) => Ok(()),

            _other_request => Err(StateError::UndispatchableRequest),
        };

        // Run-to-completion postcondition: whatever path `dispatch` took, the
        // cross-map invariants of the model must hold once it returns. We run
        // the full sweep on both success and error: a failed mutating handler
        // (e.g. a duplicate `add_*` or an absent `remove_*`) is required to be
        // a no-op, so the invariants must be intact regardless of the result.
        #[cfg(debug_assertions)]
        self.check_invariants();

        result
    }

    /// Full cross-map invariant sweep for the control-plane state model.
    ///
    /// This is the run-to-completion postcondition called via a
    /// `#[cfg(debug_assertions)]` guard at the end of [`Self::dispatch`]. It
    /// encodes the coherence invariants the diff/replay machinery relies on:
    /// every map entry is self-consistent (a value's stored key/cluster_id
    /// matches the key it is filed under), and the public accounting helpers
    /// (`count_frontends`/`count_backends`) agree with the raw map contents.
    ///
    /// Compiled out entirely in release builds (no body, no callers).
    #[cfg(debug_assertions)]
    fn check_invariants(&self) {
        // Listener maps: the value's `address` field must match the SocketAddr
        // key it is filed under, or a hot-upgrade replay (which re-derives the
        // key from the value) would land the entry under a different key.
        for (addr, listener) in &self.http_listeners {
            debug_assert_eq!(
                SocketAddr::from(listener.address),
                *addr,
                "http_listener value address must match its map key"
            );
        }
        for (addr, listener) in &self.https_listeners {
            debug_assert_eq!(
                SocketAddr::from(listener.address),
                *addr,
                "https_listener value address must match its map key"
            );
        }
        for (addr, listener) in &self.tcp_listeners {
            debug_assert_eq!(
                SocketAddr::from(listener.address),
                *addr,
                "tcp_listener value address must match its map key"
            );
        }

        // Clusters: the value's `cluster_id` must match the key it is filed
        // under (replay re-keys on `cluster.cluster_id`).
        for (cluster_id, cluster) in &self.clusters {
            debug_assert_eq!(
                &cluster.cluster_id, cluster_id,
                "cluster value cluster_id must match its map key"
            );
        }

        // Backends: grouped by cluster_id. Every backend in a bucket must carry
        // that bucket's cluster_id (replay groups on `backend.cluster_id`), and
        // the per-cluster Vec must stay deduplicated on (backend_id, address) —
        // `add_backend` upserts, so duplicates would mean lost state.
        for (cluster_id, backends) in &self.backends {
            for backend in backends {
                debug_assert_eq!(
                    &backend.cluster_id, cluster_id,
                    "backend cluster_id must match its bucket key"
                );
            }
            let unique: HashSet<(&String, &SocketAddr)> = backends
                .iter()
                .map(|b| (&b.backend_id, &b.address))
                .collect();
            debug_assert_eq!(
                unique.len(),
                backends.len(),
                "backends within a cluster must be unique on (backend_id, address)"
            );
        }

        // TCP frontends: grouped by cluster_id. Every frontend in a bucket must
        // carry that bucket's cluster_id, and `add_tcp_frontend` rejects exact
        // duplicates so each bucket stays a set.
        for (cluster_id, fronts) in &self.tcp_fronts {
            for front in fronts {
                debug_assert_eq!(
                    &front.cluster_id, cluster_id,
                    "tcp_frontend cluster_id must match its bucket key"
                );
            }
            let unique: HashSet<&TcpFrontend> = fronts.iter().collect();
            debug_assert_eq!(
                unique.len(),
                fronts.len(),
                "tcp frontends within a cluster must be unique"
            );
        }

        // Certificates: nested map keyed by (address, fingerprint). The inner
        // map's fingerprint key is the addressing identity used by diff; we do
        // not recompute it here (expensive), but the outer/inner structure must
        // not hold an empty inner map silently produced outside the API — an
        // empty bucket is a benign no-op for diff/replay, so we only assert the
        // address-key relationship is preserved by construction (trivially true
        // for a BTree/HashMap), leaving the costly fingerprint recompute out.

        // Public accounting helpers must agree with the raw maps. These are the
        // numbers the CLI and metrics surface; a drift here is a real bug.
        let raw_frontends = self.http_fronts.len()
            + self.https_fronts.len()
            + self.count_tcp_frontends_raw()
            + self.udp_fronts.values().map(|v| v.len()).sum::<usize>();
        debug_assert_eq!(
            self.count_frontends(),
            raw_frontends,
            "count_frontends must equal the sum of all frontend map entries"
        );
        let raw_backends: usize = self.backends.values().map(|v| v.len()).sum();
        debug_assert_eq!(
            self.count_backends(),
            raw_backends,
            "count_backends must equal the sum of all backend Vec lengths"
        );
    }

    /// Raw count of TCP frontends across all clusters. Used by the debug-only
    /// [`Self::check_invariants`] to cross-check `count_frontends`, and by
    /// `add_tcp_frontend`'s no-mutation-before-admission accounting.
    /// Compiled unconditionally (unlike `check_invariants`): it is a trivial
    /// sum of bucket lengths, and `debug_assert!` arguments still typecheck
    /// in release builds even though they compile out — a debug-gated helper
    /// referenced there breaks the release/bench build.
    fn count_tcp_frontends_raw(&self) -> usize {
        self.tcp_fronts.values().map(|v| v.len()).sum()
    }

    /// Increments the count for this request type
    fn increment_request_count(&mut self, request: &Request) {
        if let Some(request_type) = &request.request_type {
            let count = self
                .request_counts
                .entry(format_request_type(request_type).to_owned())
                .or_insert(1);
            *count += 1;
        }
    }

    pub fn get_request_counts(&self) -> RequestCounts {
        RequestCounts {
            map: self.request_counts.clone(),
        }
    }

    fn add_cluster(&mut self, cluster: &Cluster) -> Result<(), StateError> {
        // Validate any inline `cluster.health_check` before mutating state so
        // an invalid config (zero thresholds, missing leading `/`, CR/LF/NUL/C0
        // in URI) cannot ride in via the AddCluster path. Without this, TOML
        // reload, SaveState/LoadState, and direct API AddCluster requests
        // bypass the SetHealthCheck-side check and let an attacker-controlled
        // health-check URI smuggle CRLF into outbound HTTP/1.1 probes.
        if let Some(hc) = cluster.health_check.as_ref()
            && let Err(reason) = crate::config::validate_health_check_config(hc)
        {
            return Err(StateError::InvalidValue {
                field: "health_check",
                reason,
            });
        }
        let cluster = cluster.clone();
        // AddCluster is an upsert (replacing an existing cluster_id keeps the
        // entry count flat), so we assert on presence/key-coherence rather than
        // a strict +1 on len.
        let cluster_id = cluster.cluster_id.clone();
        self.clusters.insert(cluster_id.clone(), cluster);
        debug_assert!(
            self.clusters.contains_key(&cluster_id),
            "add_cluster must leave the cluster present in the map"
        );
        debug_assert_eq!(
            self.clusters.get(&cluster_id).map(|c| &c.cluster_id),
            Some(&cluster_id),
            "stored cluster must be keyed by its own cluster_id"
        );
        Ok(())
    }

    fn remove_cluster(&mut self, cluster_id: &str) -> Result<(), StateError> {
        let before = self.clusters.len();
        match self.clusters.remove(cluster_id) {
            Some(_) => {
                debug_assert!(
                    !self.clusters.contains_key(cluster_id),
                    "remove_cluster must evict the cluster"
                );
                debug_assert_eq!(
                    self.clusters.len(),
                    before - 1,
                    "remove_cluster must drop exactly one entry"
                );
                Ok(())
            }
            None => {
                debug_assert_eq!(
                    self.clusters.len(),
                    before,
                    "a failed remove_cluster must not mutate the map"
                );
                Err(StateError::NotFound {
                    kind: ObjectKind::Cluster,
                    id: cluster_id.to_owned(),
                })
            }
        }
    }

    fn set_health_check(&mut self, set: &SetHealthCheck) -> Result<(), StateError> {
        // Validate before mutating state so an invalid config (zero
        // thresholds, missing leading `/`, CR/LF/NUL/C0 in URI) cannot
        // round-trip through SaveState/LoadState. The worker also
        // validates at the SetHealthCheck handler — this is the
        // master-side mirror so off-channel TOML reload paths don't
        // bypass the policy.
        if let Err(reason) = crate::config::validate_health_check_config(&set.config) {
            return Err(StateError::InvalidValue {
                field: "health_check",
                reason,
            });
        }
        match self.clusters.get_mut(&set.cluster_id) {
            Some(cluster) => {
                cluster.health_check = Some(set.config.to_owned());
                Ok(())
            }
            None => Err(StateError::NotFound {
                kind: ObjectKind::Cluster,
                id: set.cluster_id.to_owned(),
            }),
        }
    }

    fn remove_health_check(&mut self, cluster_id: &str) -> Result<(), StateError> {
        match self.clusters.get_mut(cluster_id) {
            Some(cluster) => {
                cluster.health_check = None;
                Ok(())
            }
            None => Err(StateError::NotFound {
                kind: ObjectKind::Cluster,
                id: cluster_id.to_owned(),
            }),
        }
    }

    pub fn list_health_checks(&self, cluster_id: Option<&str>) -> HealthChecksList {
        let map = self
            .clusters
            .iter()
            .filter(|(id, _)| cluster_id.is_none_or(|filter| filter == id.as_str()))
            .filter_map(|(id, cluster)| {
                cluster
                    .health_check
                    .as_ref()
                    .map(|hc| (id.to_owned(), hc.to_owned()))
            })
            .collect();
        HealthChecksList { map }
    }

    fn add_http_listener(&mut self, listener: &HttpListenerConfig) -> Result<(), StateError> {
        let address: SocketAddr = listener.address.into();
        let before = self.http_listeners.len();
        match self.http_listeners.entry(address) {
            BTreeMapEntry::Vacant(vacant_entry) => vacant_entry.insert(listener.clone()),
            BTreeMapEntry::Occupied(_) => {
                debug_assert_eq!(
                    self.http_listeners.len(),
                    before,
                    "a rejected duplicate add_http_listener must not mutate the map"
                );
                return Err(StateError::Exists {
                    kind: ObjectKind::HttpListener,
                    id: address.to_string(),
                });
            }
        };
        debug_assert!(
            self.http_listeners.contains_key(&address),
            "add_http_listener must insert the listener under its address"
        );
        debug_assert_eq!(
            self.http_listeners.len(),
            before + 1,
            "add_http_listener inserts exactly one entry on the vacant path"
        );
        Ok(())
    }

    fn add_https_listener(&mut self, listener: &HttpsListenerConfig) -> Result<(), StateError> {
        let address: SocketAddr = listener.address.into();
        let before = self.https_listeners.len();
        match self.https_listeners.entry(address) {
            BTreeMapEntry::Vacant(vacant_entry) => vacant_entry.insert(listener.clone()),
            BTreeMapEntry::Occupied(_) => {
                debug_assert_eq!(
                    self.https_listeners.len(),
                    before,
                    "a rejected duplicate add_https_listener must not mutate the map"
                );
                return Err(StateError::Exists {
                    kind: ObjectKind::HttpsListener,
                    id: address.to_string(),
                });
            }
        };
        debug_assert!(
            self.https_listeners.contains_key(&address),
            "add_https_listener must insert the listener under its address"
        );
        debug_assert_eq!(
            self.https_listeners.len(),
            before + 1,
            "add_https_listener inserts exactly one entry on the vacant path"
        );
        Ok(())
    }

    fn add_tcp_listener(&mut self, listener: &TcpListenerConfig) -> Result<(), StateError> {
        let address: SocketAddr = listener.address.into();
        let before = self.tcp_listeners.len();
        match self.tcp_listeners.entry(address) {
            BTreeMapEntry::Vacant(vacant_entry) => vacant_entry.insert(*listener),
            BTreeMapEntry::Occupied(_) => {
                debug_assert_eq!(
                    self.tcp_listeners.len(),
                    before,
                    "a rejected duplicate add_tcp_listener must not mutate the map"
                );
                return Err(StateError::Exists {
                    kind: ObjectKind::TcpListener,
                    id: address.to_string(),
                });
            }
        };
        debug_assert!(
            self.tcp_listeners.contains_key(&address),
            "add_tcp_listener must insert the listener under its address"
        );
        debug_assert_eq!(
            self.tcp_listeners.len(),
            before + 1,
            "add_tcp_listener inserts exactly one entry on the vacant path"
        );
        Ok(())
    }

    fn add_udp_listener(&mut self, listener: &UdpListenerConfig) -> Result<(), StateError> {
        let address: SocketAddr = listener.address.into();
        match self.udp_listeners.entry(address) {
            BTreeMapEntry::Vacant(vacant_entry) => vacant_entry.insert(*listener),
            BTreeMapEntry::Occupied(_) => {
                return Err(StateError::Exists {
                    kind: ObjectKind::UdpListener,
                    id: address.to_string(),
                });
            }
        };
        Ok(())
    }

    fn remove_listener(&mut self, remove: &RemoveListener) -> Result<(), StateError> {
        match ListenerType::try_from(remove.proxy).map_err(StateError::WrongFieldValue)? {
            ListenerType::Http => self.remove_http_listener(&remove.address.into()),
            ListenerType::Https => self.remove_https_listener(&remove.address.into()),
            ListenerType::Tcp => self.remove_tcp_listener(&remove.address.into()),
            ListenerType::Udp => self.remove_udp_listener(&remove.address.into()),
        }
    }

    fn remove_http_listener(&mut self, address: &SocketAddr) -> Result<(), StateError> {
        let before = self.http_listeners.len();
        if self.http_listeners.remove(address).is_none() {
            debug_assert_eq!(
                self.http_listeners.len(),
                before,
                "a failed remove_http_listener must not mutate the map"
            );
            return Err(StateError::NoChange);
        }
        debug_assert!(
            !self.http_listeners.contains_key(address),
            "remove_http_listener must evict the address"
        );
        debug_assert_eq!(
            self.http_listeners.len(),
            before - 1,
            "remove_http_listener drops exactly one entry"
        );
        Ok(())
    }

    fn remove_https_listener(&mut self, address: &SocketAddr) -> Result<(), StateError> {
        let before = self.https_listeners.len();
        if self.https_listeners.remove(address).is_none() {
            debug_assert_eq!(
                self.https_listeners.len(),
                before,
                "a failed remove_https_listener must not mutate the map"
            );
            return Err(StateError::NoChange);
        }
        debug_assert!(
            !self.https_listeners.contains_key(address),
            "remove_https_listener must evict the address"
        );
        debug_assert_eq!(
            self.https_listeners.len(),
            before - 1,
            "remove_https_listener drops exactly one entry"
        );
        Ok(())
    }

    fn remove_tcp_listener(&mut self, address: &SocketAddr) -> Result<(), StateError> {
        let before = self.tcp_listeners.len();
        if self.tcp_listeners.remove(address).is_none() {
            debug_assert_eq!(
                self.tcp_listeners.len(),
                before,
                "a failed remove_tcp_listener must not mutate the map"
            );
            return Err(StateError::NoChange);
        }
        debug_assert!(
            !self.tcp_listeners.contains_key(address),
            "remove_tcp_listener must evict the address"
        );
        debug_assert_eq!(
            self.tcp_listeners.len(),
            before - 1,
            "remove_tcp_listener drops exactly one entry"
        );
        Ok(())
    }

    fn remove_udp_listener(&mut self, address: &SocketAddr) -> Result<(), StateError> {
        if self.udp_listeners.remove(address).is_none() {
            return Err(StateError::NoChange);
        }
        Ok(())
    }

    /// Validate and apply a partial patch to an existing HTTP listener.
    ///
    /// Only `Some` fields in the patch are written; `None` fields preserve the
    /// current value. Returns `StateError::NotFound` if the address is unknown,
    /// `StateError::InvalidValue` if a flood-knob value is below the required
    /// minimum.
    fn update_http_listener(&mut self, patch: &UpdateHttpListenerConfig) -> Result<(), StateError> {
        validate_h2_flood_knobs_http(patch)?;

        let address: SocketAddr = patch.address.into();
        let listener =
            self.http_listeners
                .get_mut(&address)
                .ok_or_else(|| StateError::NotFound {
                    kind: ObjectKind::HttpListener,
                    id: address.to_string(),
                })?;

        // Shared session-at-accept / per-connection knobs
        if let Some(v) = patch.public_address {
            listener.public_address = Some(v);
        }
        if let Some(v) = patch.expect_proxy {
            listener.expect_proxy = v;
        }
        if let Some(ref v) = patch.sticky_name {
            listener.sticky_name = v.to_owned();
        }
        if let Some(v) = patch.front_timeout {
            listener.front_timeout = v;
        }
        if let Some(v) = patch.back_timeout {
            listener.back_timeout = v;
        }
        if let Some(v) = patch.connect_timeout {
            listener.connect_timeout = v;
        }
        if let Some(v) = patch.request_timeout {
            listener.request_timeout = v;
        }
        if let Some(patch_answers) = patch.http_answers.as_ref() {
            merge_custom_http_answers(&mut listener.http_answers, patch_answers);
        }
        // H2 flood knobs
        if let Some(v) = patch.h2_max_rst_stream_per_window {
            listener.h2_max_rst_stream_per_window = Some(v);
        }
        if let Some(v) = patch.h2_max_ping_per_window {
            listener.h2_max_ping_per_window = Some(v);
        }
        if let Some(v) = patch.h2_max_settings_per_window {
            listener.h2_max_settings_per_window = Some(v);
        }
        if let Some(v) = patch.h2_max_empty_data_per_window {
            listener.h2_max_empty_data_per_window = Some(v);
        }
        if let Some(v) = patch.h2_max_continuation_frames {
            listener.h2_max_continuation_frames = Some(v);
        }
        if let Some(v) = patch.h2_max_glitch_count {
            listener.h2_max_glitch_count = Some(v);
        }
        if let Some(v) = patch.h2_initial_connection_window {
            listener.h2_initial_connection_window = Some(v);
        }
        if let Some(v) = patch.h2_max_concurrent_streams {
            listener.h2_max_concurrent_streams = Some(v);
        }
        if let Some(v) = patch.h2_stream_shrink_ratio {
            listener.h2_stream_shrink_ratio = Some(v);
        }
        if let Some(v) = patch.h2_max_rst_stream_lifetime {
            listener.h2_max_rst_stream_lifetime = Some(v);
        }
        if let Some(v) = patch.h2_max_rst_stream_abusive_lifetime {
            listener.h2_max_rst_stream_abusive_lifetime = Some(v);
        }
        if let Some(v) = patch.h2_max_rst_stream_emitted_lifetime {
            listener.h2_max_rst_stream_emitted_lifetime = Some(v);
        }
        if let Some(v) = patch.h2_max_header_list_size {
            listener.h2_max_header_list_size = Some(v);
        }
        if let Some(v) = patch.h2_max_header_table_size {
            listener.h2_max_header_table_size = Some(v);
        }
        if let Some(v) = patch.h2_max_header_fields {
            listener.h2_max_header_fields = Some(v);
        }
        if let Some(v) = patch.h2_stream_idle_timeout_seconds {
            listener.h2_stream_idle_timeout_seconds = Some(v);
        }
        // 0 is valid for graceful_shutdown_deadline (means "wait forever")
        if let Some(v) = patch.h2_graceful_shutdown_deadline_seconds {
            listener.h2_graceful_shutdown_deadline_seconds = Some(v);
        }
        if let Some(v) = patch.h2_max_window_update_stream0_per_window {
            listener.h2_max_window_update_stream0_per_window = Some(v);
        }
        if let Some(ref v) = patch.sozu_id_header {
            validate_sozu_id_header(v)?;
            listener.sozu_id_header = Some(v.to_owned());
        }
        Ok(())
    }

    /// Validate and apply a partial patch to an existing HTTPS listener.
    ///
    /// Only `Some` fields in the patch are written; `None` fields preserve the
    /// current value. Returns `StateError::NotFound` if the address is unknown,
    /// `StateError::InvalidValue` if a flood-knob value is below the required
    /// minimum or an ALPN value is unknown.
    fn update_https_listener(
        &mut self,
        patch: &UpdateHttpsListenerConfig,
    ) -> Result<(), StateError> {
        validate_h2_flood_knobs_https(patch)?;

        let address: SocketAddr = patch.address.into();
        let listener =
            self.https_listeners
                .get_mut(&address)
                .ok_or_else(|| StateError::NotFound {
                    kind: ObjectKind::HttpsListener,
                    id: address.to_string(),
                })?;

        // Shared session-at-accept / per-connection knobs
        if let Some(v) = patch.public_address {
            listener.public_address = Some(v);
        }
        if let Some(v) = patch.expect_proxy {
            listener.expect_proxy = v;
        }
        if let Some(ref v) = patch.sticky_name {
            listener.sticky_name = v.to_owned();
        }
        if let Some(v) = patch.front_timeout {
            listener.front_timeout = v;
        }
        if let Some(v) = patch.back_timeout {
            listener.back_timeout = v;
        }
        if let Some(v) = patch.connect_timeout {
            listener.connect_timeout = v;
        }
        if let Some(v) = patch.request_timeout {
            listener.request_timeout = v;
        }
        if let Some(patch_answers) = patch.http_answers.as_ref() {
            merge_custom_http_answers(&mut listener.http_answers, patch_answers);
        }
        // HTTPS-only knobs
        if let Some(ref alpn_wrapper) = patch.alpn_protocols {
            validate_alpn_protocols(&alpn_wrapper.values)?;
            // Empty values vec = reset to default (runtime treats empty as default)
            listener.alpn_protocols = alpn_wrapper.values.clone();
        }
        if let Some(v) = patch.strict_sni_binding {
            listener.strict_sni_binding = Some(v);
        }
        if let Some(v) = patch.disable_http11 {
            listener.disable_http11 = Some(v);
        }
        // H2 flood knobs
        if let Some(v) = patch.h2_max_rst_stream_per_window {
            listener.h2_max_rst_stream_per_window = Some(v);
        }
        if let Some(v) = patch.h2_max_ping_per_window {
            listener.h2_max_ping_per_window = Some(v);
        }
        if let Some(v) = patch.h2_max_settings_per_window {
            listener.h2_max_settings_per_window = Some(v);
        }
        if let Some(v) = patch.h2_max_empty_data_per_window {
            listener.h2_max_empty_data_per_window = Some(v);
        }
        if let Some(v) = patch.h2_max_continuation_frames {
            listener.h2_max_continuation_frames = Some(v);
        }
        if let Some(v) = patch.h2_max_glitch_count {
            listener.h2_max_glitch_count = Some(v);
        }
        if let Some(v) = patch.h2_initial_connection_window {
            listener.h2_initial_connection_window = Some(v);
        }
        if let Some(v) = patch.h2_max_concurrent_streams {
            listener.h2_max_concurrent_streams = Some(v);
        }
        if let Some(v) = patch.h2_stream_shrink_ratio {
            listener.h2_stream_shrink_ratio = Some(v);
        }
        if let Some(v) = patch.h2_max_rst_stream_lifetime {
            listener.h2_max_rst_stream_lifetime = Some(v);
        }
        if let Some(v) = patch.h2_max_rst_stream_abusive_lifetime {
            listener.h2_max_rst_stream_abusive_lifetime = Some(v);
        }
        if let Some(v) = patch.h2_max_rst_stream_emitted_lifetime {
            listener.h2_max_rst_stream_emitted_lifetime = Some(v);
        }
        if let Some(v) = patch.h2_max_header_list_size {
            listener.h2_max_header_list_size = Some(v);
        }
        if let Some(v) = patch.h2_max_header_table_size {
            listener.h2_max_header_table_size = Some(v);
        }
        if let Some(v) = patch.h2_max_header_fields {
            listener.h2_max_header_fields = Some(v);
        }
        if let Some(v) = patch.h2_stream_idle_timeout_seconds {
            listener.h2_stream_idle_timeout_seconds = Some(v);
        }
        // 0 is valid for graceful_shutdown_deadline (means "wait forever")
        if let Some(v) = patch.h2_graceful_shutdown_deadline_seconds {
            listener.h2_graceful_shutdown_deadline_seconds = Some(v);
        }
        if let Some(v) = patch.h2_max_window_update_stream0_per_window {
            listener.h2_max_window_update_stream0_per_window = Some(v);
        }
        if let Some(ref v) = patch.sozu_id_header {
            validate_sozu_id_header(v)?;
            listener.sozu_id_header = Some(v.to_owned());
        }
        Ok(())
    }

    /// Validate and apply a partial patch to an existing TCP listener.
    ///
    /// Only `Some` fields in the patch are written; `None` fields preserve the
    /// current value. Returns `StateError::NotFound` if the address is unknown.
    fn update_tcp_listener(&mut self, patch: &UpdateTcpListenerConfig) -> Result<(), StateError> {
        let address: SocketAddr = patch.address.into();
        let listener =
            self.tcp_listeners
                .get_mut(&address)
                .ok_or_else(|| StateError::NotFound {
                    kind: ObjectKind::TcpListener,
                    id: address.to_string(),
                })?;

        if let Some(v) = patch.public_address {
            listener.public_address = Some(v);
        }
        if let Some(v) = patch.expect_proxy {
            listener.expect_proxy = v;
        }
        if let Some(v) = patch.front_timeout {
            listener.front_timeout = v;
        }
        if let Some(v) = patch.back_timeout {
            listener.back_timeout = v;
        }
        if let Some(v) = patch.connect_timeout {
            listener.connect_timeout = v;
        }
        Ok(())
    }

    /// Validate and apply a partial patch to an existing UDP listener.
    ///
    /// Only `Some` fields in the patch are written; `None` fields preserve the
    /// current value. Returns `StateError::NotFound` if the address is unknown.
    fn update_udp_listener(&mut self, patch: &UpdateUdpListenerConfig) -> Result<(), StateError> {
        let address: SocketAddr = patch.address.into();
        let listener =
            self.udp_listeners
                .get_mut(&address)
                .ok_or_else(|| StateError::NotFound {
                    kind: ObjectKind::UdpListener,
                    id: address.to_string(),
                })?;

        if let Some(v) = patch.public_address {
            listener.public_address = Some(v);
        }
        if let Some(v) = patch.front_timeout {
            listener.front_timeout = v;
        }
        if let Some(v) = patch.back_timeout {
            listener.back_timeout = v;
        }
        if let Some(v) = patch.max_rx_datagram_size {
            listener.max_rx_datagram_size = v;
        }
        if let Some(v) = patch.max_flows {
            listener.max_flows = v;
        }
        Ok(())
    }

    fn activate_listener(&mut self, activate: &ActivateListener) -> Result<(), StateError> {
        match ListenerType::try_from(activate.proxy).map_err(StateError::WrongFieldValue)? {
            ListenerType::Http => self
                .http_listeners
                .get_mut(&activate.address.into())
                .map(|listener| listener.active = true)
                .ok_or(StateError::NotFound {
                    kind: ObjectKind::HttpListener,
                    id: activate.address.to_string(),
                }),
            ListenerType::Https => self
                .https_listeners
                .get_mut(&activate.address.into())
                .map(|listener| listener.active = true)
                .ok_or(StateError::NotFound {
                    kind: ObjectKind::HttpsListener,
                    id: activate.address.to_string(),
                }),
            ListenerType::Tcp => self
                .tcp_listeners
                .get_mut(&activate.address.into())
                .map(|listener| listener.active = true)
                .ok_or(StateError::NotFound {
                    kind: ObjectKind::TcpListener,
                    id: activate.address.to_string(),
                }),
            ListenerType::Udp => self
                .udp_listeners
                .get_mut(&activate.address.into())
                .map(|listener| listener.active = true)
                .ok_or(StateError::NotFound {
                    kind: ObjectKind::UdpListener,
                    id: activate.address.to_string(),
                }),
        }
    }

    fn deactivate_listener(&mut self, deactivate: &DeactivateListener) -> Result<(), StateError> {
        match ListenerType::try_from(deactivate.proxy).map_err(StateError::WrongFieldValue)? {
            ListenerType::Http => self
                .http_listeners
                .get_mut(&deactivate.address.into())
                .map(|listener| listener.active = false)
                .ok_or(StateError::NotFound {
                    kind: ObjectKind::HttpListener,
                    id: deactivate.address.to_string(),
                }),
            ListenerType::Https => self
                .https_listeners
                .get_mut(&deactivate.address.into())
                .map(|listener| listener.active = false)
                .ok_or(StateError::NotFound {
                    kind: ObjectKind::HttpsListener,
                    id: deactivate.address.to_string(),
                }),
            ListenerType::Tcp => self
                .tcp_listeners
                .get_mut(&deactivate.address.into())
                .map(|listener| listener.active = false)
                .ok_or(StateError::NotFound {
                    kind: ObjectKind::TcpListener,
                    id: deactivate.address.to_string(),
                }),
            ListenerType::Udp => self
                .udp_listeners
                .get_mut(&deactivate.address.into())
                .map(|listener| listener.active = false)
                .ok_or(StateError::NotFound {
                    kind: ObjectKind::UdpListener,
                    id: deactivate.address.to_string(),
                }),
        }
    }

    fn add_http_frontend(&mut self, front: &RequestHttpFrontend) -> Result<(), StateError> {
        let front_as_key = front.to_string();
        let before = self.http_fronts.len();

        match self.http_fronts.entry(front.to_string()) {
            BTreeMapEntry::Vacant(e) => {
                e.insert(front.clone().to_frontend().map_err(|into_error| {
                    StateError::FrontendConversion {
                        frontend: front_as_key,
                        error: into_error.to_string(),
                    }
                })?)
            }
            BTreeMapEntry::Occupied(_) => {
                debug_assert_eq!(
                    self.http_fronts.len(),
                    before,
                    "a rejected duplicate add_http_frontend must not mutate the map"
                );
                return Err(StateError::Exists {
                    kind: ObjectKind::HttpFrontend,
                    id: front.to_string(),
                });
            }
        };
        // On the conversion-error path the `?` already returned, so reaching
        // here means exactly one entry was inserted under the route key.
        debug_assert!(
            self.http_fronts.contains_key(&front.to_string()),
            "add_http_frontend must insert the route key on success"
        );
        debug_assert_eq!(
            self.http_fronts.len(),
            before + 1,
            "add_http_frontend inserts exactly one entry on success"
        );
        Ok(())
    }

    fn add_https_frontend(&mut self, front: &RequestHttpFrontend) -> Result<(), StateError> {
        let front_as_key = front.to_string();
        let before = self.https_fronts.len();

        match self.https_fronts.entry(front.to_string()) {
            BTreeMapEntry::Vacant(e) => {
                e.insert(front.clone().to_frontend().map_err(|into_error| {
                    StateError::FrontendConversion {
                        frontend: front_as_key,
                        error: into_error.to_string(),
                    }
                })?)
            }
            BTreeMapEntry::Occupied(_) => {
                debug_assert_eq!(
                    self.https_fronts.len(),
                    before,
                    "a rejected duplicate add_https_frontend must not mutate the map"
                );
                return Err(StateError::Exists {
                    kind: ObjectKind::HttpsFrontend,
                    id: front.to_string(),
                });
            }
        };
        debug_assert!(
            self.https_fronts.contains_key(&front.to_string()),
            "add_https_frontend must insert the route key on success"
        );
        debug_assert_eq!(
            self.https_fronts.len(),
            before + 1,
            "add_https_frontend inserts exactly one entry on success"
        );
        Ok(())
    }

    fn remove_http_frontend(&mut self, front: &RequestHttpFrontend) -> Result<(), StateError> {
        let key = front.to_string();
        let before = self.http_fronts.len();
        self.http_fronts.remove(&key).ok_or(StateError::NotFound {
            kind: ObjectKind::HttpFrontend,
            id: front.to_string(),
        })?;
        debug_assert!(
            !self.http_fronts.contains_key(&key),
            "remove_http_frontend must evict the route key"
        );
        debug_assert_eq!(
            self.http_fronts.len(),
            before - 1,
            "remove_http_frontend drops exactly one entry"
        );
        Ok(())
    }

    fn remove_https_frontend(&mut self, front: &RequestHttpFrontend) -> Result<(), StateError> {
        let key = front.to_string();
        let before = self.https_fronts.len();
        self.https_fronts.remove(&key).ok_or(StateError::NotFound {
            kind: ObjectKind::HttpsFrontend,
            id: front.to_string(),
        })?;
        debug_assert!(
            !self.https_fronts.contains_key(&key),
            "remove_https_frontend must evict the route key"
        );
        debug_assert_eq!(
            self.https_fronts.len(),
            before - 1,
            "remove_https_frontend drops exactly one entry"
        );
        Ok(())
    }

    fn add_certificate(&mut self, add: &AddCertificate) -> Result<(), StateError> {
        let fingerprint = add
            .certificate
            .fingerprint()
            .map_err(StateError::AddCertificate)?;

        let entry = self.certificates.entry(add.address.into()).or_default();

        let mut add = add.clone();
        add.certificate
            .apply_overriding_names()
            .map_err(StateError::AddCertificate)?;

        if entry.contains_key(&fingerprint) {
            let names_bytes = add
                .certificate
                .names
                .iter()
                .fold(0usize, |total, name| total.saturating_add(name.len()));
            info!(
                "Skip loading certificate with fingerprint_bytes={} names_count={} names_bytes={} on listener '{}', the certificate is already present.",
                fingerprint.0.len(),
                add.certificate.names.len(),
                names_bytes,
                add.address
            );
            return Ok(());
        }

        let before = entry.len();
        entry.insert(fingerprint.clone(), add.certificate);
        debug_assert!(
            entry.contains_key(&fingerprint),
            "add_certificate must insert the fingerprint under its address"
        );
        debug_assert_eq!(
            entry.len(),
            before + 1,
            "add_certificate inserts exactly one fingerprint on the new path"
        );
        Ok(())
    }

    fn remove_certificate(&mut self, remove: &RemoveCertificate) -> Result<(), StateError> {
        let fingerprint = Fingerprint(
            hex::decode(&remove.fingerprint)
                .map_err(|decode_error| StateError::RemoveCertificate(decode_error.to_string()))?,
        );

        if let Some(index) = self.certificates.get_mut(&remove.address.into()) {
            index.remove(&fingerprint);
            debug_assert!(
                !index.contains_key(&fingerprint),
                "remove_certificate must evict the fingerprint when the address is known"
            );
        }

        Ok(())
    }

    /// - Remove old certificate from certificates, using the old fingerprint
    /// - calculate the new fingerprint
    /// - insert the new certificate with the new fingerprint as key
    /// - check that the new entry is present in the certificates hashmap
    fn replace_certificate(&mut self, replace: &ReplaceCertificate) -> Result<(), StateError> {
        let replace_address = replace.address.into();
        let old_fingerprint = Fingerprint(
            hex::decode(&replace.old_fingerprint)
                .map_err(|decode_error| StateError::RemoveCertificate(decode_error.to_string()))?,
        );

        self.certificates
            .get_mut(&replace_address)
            .ok_or(StateError::NotFound {
                kind: ObjectKind::Certificate,
                id: replace.address.to_string(),
            })?
            .remove(&old_fingerprint);

        let new_fingerprint = Fingerprint(
            calculate_fingerprint(replace.new_certificate.certificate.as_bytes()).map_err(
                |fingerprint_err| StateError::ReplaceCertificate(fingerprint_err.to_string()),
            )?,
        );

        self.certificates
            .get_mut(&replace_address)
            .map(|certs| certs.insert(new_fingerprint.clone(), replace.new_certificate.clone()));

        if !self
            .certificates
            .get(&replace_address)
            .ok_or(StateError::ReplaceCertificate(
                "Unlikely error. This entry in the certificate hashmap should be present"
                    .to_string(),
            ))?
            .contains_key(&new_fingerprint)
        {
            return Err(StateError::ReplaceCertificate(format!(
                "Failed to insert the new certificate for address {}",
                replace.address
            )));
        }
        // Postcondition: the new fingerprint is keyed under the address, and
        // (unless old and new collide, e.g. a self-replace) the old one is gone.
        debug_assert!(
            self.certificates
                .get(&replace_address)
                .is_some_and(|certs| certs.contains_key(&new_fingerprint)),
            "replace_certificate must leave the new fingerprint present"
        );
        debug_assert!(
            new_fingerprint == old_fingerprint
                || self
                    .certificates
                    .get(&replace_address)
                    .is_none_or(|certs| !certs.contains_key(&old_fingerprint)),
            "replace_certificate must evict the old fingerprint unless it equals the new one"
        );
        Ok(())
    }

    /// Admission control mirrors the worker's `validate_new_tcp_front`
    /// (`lib/src/tcp.rs`) so the master never persists a route its own
    /// workers reject on the next fan-out (sozu-proxy/sozu#1290): the
    /// command server mutates master state before dispatching to workers,
    /// and a worker NACK never rolls the master back. Every check below
    /// runs, and every canonicalization is computed, BEFORE `self.tcp_fronts`
    /// is touched -- an early `Err` return is a true no-op on `self`.
    fn add_tcp_frontend(&mut self, front: &RequestTcpFrontend) -> Result<(), StateError> {
        let address: SocketAddr = front.address.into();

        // Canonicalize first: shape-validate and lowercase `sni` through the
        // SAME `validate_sni_pattern` config-load and the worker use, and
        // reduce `alpn` to its canonical sorted+deduped set. Both canonical
        // forms are what gets stored (so the state's own hashing/diffing/
        // `generate_requests` replay sees one identity per route,
        // independent of how the caller cased/ordered its request) AND what
        // every admission check below compares against.
        let normalized_sni = match &front.sni {
            Some(sni) => Some(validate_sni_pattern(sni).map_err(|config_error| {
                StateError::InvalidTcpFrontend {
                    address,
                    reason: format!("sni {sni:?} failed SNI shape validation: {config_error}"),
                }
            })?),
            None => None,
        };
        let alpn = canonical_tcp_alpn(&front.alpn);

        // Worker parity (`validate_new_tcp_front`'s first no-SNI check):
        // alpn only matches within an SNI-scoped preread, so a frontend
        // without sni would silently ignore its alpn list. Config-load's
        // `to_tcp_front` already rejects this shape; a raw `AddTcpFrontend`
        // over the command socket must not slip past the master either.
        if normalized_sni.is_none() && !alpn.is_empty() {
            return Err(StateError::InvalidTcpFrontend {
                address,
                reason: format!(
                    "alpn = {alpn:?} set without sni: alpn only matches within an SNI-scoped \
                     preread, so a frontend without sni would silently ignore its alpn list"
                ),
            });
        }

        let total_before = self.count_tcp_frontends_raw();

        // Listener-wide scan across ALL clusters' buckets at this address --
        // the previous check only walked `front.cluster_id`'s own bucket, so
        // a duplicate identity, a no-SNI/SNI mix, a second catch-all, or an
        // overlapping ALPN set under a DIFFERENT cluster_id all used to pass
        // here and then fail on every worker.
        let mut listener_has_no_sni_front = false;
        let mut listener_has_sni_front = false;
        for existing in self
            .tcp_fronts
            .values()
            .flatten()
            .filter(|existing| existing.address == address)
        {
            if tcp_frontend_matches(existing, address, &normalized_sni, &alpn) {
                return Err(StateError::Exists {
                    kind: ObjectKind::TcpFrontend,
                    id: format!(
                        "TcpFrontend {{ address: {address}, sni: {normalized_sni:?}, \
                         alpn: {alpn:?} }}"
                    ),
                });
            }
            match &existing.sni {
                None => listener_has_no_sni_front = true,
                Some(existing_sni) => {
                    listener_has_sni_front = true;
                    // Catch-all / ALPN-overlap only matter between two
                    // SNI-scoped fronts sharing the exact same normalized
                    // sni -- exact and wildcard patterns are DISTINCT keys
                    // here, never matched against each other (no trie
                    // matching in `ConfigState`), mirroring the worker's own
                    // `accept_wildcard: false` self-lookup.
                    if normalized_sni.as_deref() != Some(existing_sni.as_str()) {
                        continue;
                    }
                    let existing_is_catch_all = existing.alpn.is_empty();
                    let new_is_catch_all = alpn.is_empty();
                    if existing_is_catch_all && new_is_catch_all {
                        return Err(StateError::InvalidTcpFrontend {
                            address,
                            reason: format!(
                                "sni {existing_sni:?} already has a catch-all (empty alpn) \
                                 frontend: at most one frontend per (address, sni) may omit \
                                 alpn"
                            ),
                        });
                    }
                    // A catch-all coexisting with an ALPN-scoped sibling on
                    // the same sni is legal (worker parity: `AlpnMatcher::Any`
                    // only conflicts with another `Any`, never with a
                    // `OneOf`) -- only two ALPN-scoped fronts can overlap.
                    if !existing_is_catch_all
                        && !new_is_catch_all
                        && let Some(overlap) = alpn
                            .iter()
                            .find(|protocol| existing.alpn.contains(protocol))
                    {
                        return Err(StateError::InvalidTcpFrontend {
                            address,
                            reason: format!(
                                "sni {existing_sni:?} already has a frontend matching ALPN \
                                 protocol {overlap:?}: ALPN matchers for the same (address, \
                                 sni) must not overlap"
                            ),
                        });
                    }
                }
            }
        }
        match &normalized_sni {
            None if listener_has_sni_front => {
                return Err(StateError::InvalidTcpFrontend {
                    address,
                    reason: "a no-SNI frontend cannot be added to a listener that already has \
                             SNI-scoped routes"
                        .to_string(),
                });
            }
            Some(_) if listener_has_no_sni_front => {
                return Err(StateError::InvalidTcpFrontend {
                    address,
                    reason: "an SNI-scoped frontend cannot be added to a listener that already \
                             has a no-SNI catch-all cluster"
                        .to_string(),
                });
            }
            _ => {}
        }
        // POST: every admission check above either returned `Err` or fell
        // through -- none of them may mutate `self`, so the total frontend
        // count observed before the scan must still hold at this point.
        debug_assert_eq!(
            self.count_tcp_frontends_raw(),
            total_before,
            "add_tcp_frontend must not mutate tcp_fronts before every admission check has \
             passed"
        );

        let tcp_frontend = TcpFrontend {
            cluster_id: front.cluster_id.clone(),
            address,
            tags: front.tags.clone(),
            sni: normalized_sni,
            alpn,
        };
        let tcp_frontends = self.tcp_fronts.entry(front.cluster_id.clone()).or_default();
        let before = tcp_frontends.len();
        debug_assert_eq!(
            tcp_frontend.cluster_id, front.cluster_id,
            "the built frontend must carry its bucket's cluster_id"
        );
        tcp_frontends.push(tcp_frontend);
        debug_assert_eq!(
            tcp_frontends.len(),
            before + 1,
            "add_tcp_frontend appends exactly one entry"
        );
        Ok(())
    }

    fn remove_tcp_frontend(
        &mut self,
        front_to_remove: &RequestTcpFrontend,
    ) -> Result<(), StateError> {
        let tcp_frontends =
            self.tcp_fronts
                .get_mut(&front_to_remove.cluster_id)
                .ok_or(StateError::NotFound {
                    kind: ObjectKind::TcpFrontend,
                    id: format!(
                        "RequestTcpFrontend {{ cluster_id: {:?}, address: {:?}, tags: {:?}, sni: {:?}, alpn: {:?} }}",
                        front_to_remove.cluster_id,
                        front_to_remove.address,
                        front_to_remove.tags,
                        front_to_remove.sni,
                        front_to_remove.alpn,
                    ),
                })?;

        let len = tcp_frontends.len();
        let remove_address: SocketAddr = front_to_remove.address.into();
        // Canonicalize for matching, mirroring add_tcp_frontend and the
        // worker: lowercase `sni` (the SNI-preread core normalizes to
        // lowercase before ever looking a route up, and `TcpFrontend.sni` is
        // stored in that same normalized form) and reduce `alpn` to its
        // canonical sorted+deduped set, so a request differing only in sni
        // case or alpn order still matches the stored frontend instead of
        // falling through to `NoChange`. Shape is intentionally NOT
        // re-validated here: a malformed pattern could never have been
        // stored post-fix, so it simply matches nothing below.
        let remove_sni = front_to_remove.sni.as_deref().map(str::to_ascii_lowercase);
        let remove_alpn = canonical_tcp_alpn(&front_to_remove.alpn);
        // INV: removal identity mirrors add_tcp_frontend's (address, sni,
        // alpn) key — removing one SNI-scoped frontend on a listener must
        // not also evict a sibling frontend at the same address with a
        // different sni/alpn.
        tcp_frontends.retain(|front| {
            !tcp_frontend_matches(front, remove_address, &remove_sni, &remove_alpn)
        });
        let after = tcp_frontends.len();
        if after == len {
            return Err(StateError::NoChange);
        }
        // `retain` may drop more than one entry only if duplicates on the same
        // (address, sni, alpn) ever existed; `add_tcp_frontend` forbids that,
        // so a successful removal must drop exactly one and leave none
        // matching.
        debug_assert_eq!(
            after,
            len - 1,
            "remove_tcp_frontend drops exactly one entry"
        );
        debug_assert!(
            !tcp_frontends.iter().any(|f| tcp_frontend_matches(
                f,
                remove_address,
                &remove_sni,
                &remove_alpn
            )),
            "remove_tcp_frontend must leave no frontend matching the removed (address, sni, alpn)"
        );
        Ok(())
    }

    fn add_udp_frontend(&mut self, front: &RequestUdpFrontend) -> Result<(), StateError> {
        let udp_frontends = self.udp_fronts.entry(front.cluster_id.clone()).or_default();

        let udp_frontend = UdpFrontend {
            cluster_id: front.cluster_id.clone(),
            address: front.address.into(),
            tags: front.tags.clone(),
        };
        if udp_frontends.contains(&udp_frontend) {
            return Err(StateError::Exists {
                kind: ObjectKind::UdpFrontend,
                id: format!("{udp_frontend:?}"),
            });
        }

        udp_frontends.push(udp_frontend);
        Ok(())
    }

    fn remove_udp_frontend(
        &mut self,
        front_to_remove: &RequestUdpFrontend,
    ) -> Result<(), StateError> {
        let udp_frontends =
            self.udp_fronts
                .get_mut(&front_to_remove.cluster_id)
                .ok_or(StateError::NotFound {
                    kind: ObjectKind::UdpFrontend,
                    id: format!("{front_to_remove:?}"),
                })?;

        let len = udp_frontends.len();
        udp_frontends.retain(|front| front.address != front_to_remove.address.into());
        if udp_frontends.len() == len {
            return Err(StateError::NoChange);
        }
        Ok(())
    }

    fn add_backend(&mut self, add_backend: &AddBackend) -> Result<(), StateError> {
        let backend = Backend {
            address: add_backend.address.into(),
            cluster_id: add_backend.cluster_id.clone(),
            backend_id: add_backend.backend_id.clone(),
            sticky_id: add_backend.sticky_id.clone(),
            load_balancing_parameters: add_backend.load_balancing_parameters,
            backup: add_backend.backup,
        };
        let backends = self.backends.entry(backend.cluster_id.clone()).or_default();
        let backend_id = backend.backend_id.clone();
        let backend_address = backend.address;
        let before = backends.len();

        // we might be modifying the sticky id or load balancing parameters:
        // the retain drops at most one prior copy (the map stays deduplicated
        // on (backend_id, address)), then we re-push the new version. So the
        // net length grows by exactly one iff this was a brand-new backend.
        let was_present = backends
            .iter()
            .any(|b| b.backend_id == backend_id && b.address == backend_address);
        backends.retain(|b| b.backend_id != backend.backend_id || b.address != backend.address);
        debug_assert_eq!(
            backends.len(),
            before - was_present as usize,
            "the upsert retain must drop exactly the prior copy iff it existed"
        );
        backends.push(backend);
        backends.sort();

        debug_assert_eq!(
            backends.len(),
            before + (!was_present) as usize,
            "add_backend grows the bucket by one iff the backend was new"
        );
        debug_assert_eq!(
            backends
                .iter()
                .filter(|b| b.backend_id == backend_id && b.address == backend_address)
                .count(),
            1,
            "exactly one copy of the upserted backend must remain"
        );
        Ok(())
    }

    fn remove_backend(&mut self, backend: &RemoveBackend) -> Result<(), StateError> {
        let backend_list =
            self.backends
                .get_mut(&backend.cluster_id)
                .ok_or(StateError::NotFound {
                    kind: ObjectKind::Backend,
                    id: backend.backend_id.to_owned(),
                })?;

        let len = backend_list.len();
        let remove_address: SocketAddr = backend.address.into();
        backend_list.retain(|b| b.backend_id != backend.backend_id || b.address != remove_address);
        backend_list.sort();
        let after = backend_list.len();
        if after == len {
            return Err(StateError::NoChange);
        }
        // The list is deduplicated on (backend_id, address), so a matching
        // removal drops exactly one entry and leaves nothing matching.
        debug_assert_eq!(after, len - 1, "remove_backend drops exactly one entry");
        debug_assert!(
            !backend_list
                .iter()
                .any(|b| b.backend_id == backend.backend_id && b.address == remove_address),
            "remove_backend must leave no backend matching (backend_id, address)"
        );
        Ok(())
    }

    /// creates all requests needed to bootstrap the state
    fn generate_requests(&self) -> Vec<Request> {
        let mut v: Vec<Request> = Vec::new();

        for listener in self.http_listeners.values() {
            v.push(RequestType::AddHttpListener(listener.clone()).into());
            if listener.active {
                v.push(
                    RequestType::ActivateListener(ActivateListener {
                        address: listener.address,
                        proxy: ListenerType::Http.into(),
                        from_scm: false,
                    })
                    .into(),
                );
            }
        }

        for listener in self.https_listeners.values() {
            v.push(RequestType::AddHttpsListener(listener.clone()).into());
            if listener.active {
                v.push(
                    RequestType::ActivateListener(ActivateListener {
                        address: listener.address,
                        proxy: ListenerType::Https.into(),
                        from_scm: false,
                    })
                    .into(),
                );
            }
        }

        for listener in self.tcp_listeners.values() {
            v.push(RequestType::AddTcpListener(*listener).into());
            if listener.active {
                v.push(
                    RequestType::ActivateListener(ActivateListener {
                        address: listener.address,
                        proxy: ListenerType::Tcp.into(),
                        from_scm: false,
                    })
                    .into(),
                );
            }
        }

        for listener in self.udp_listeners.values() {
            v.push(RequestType::AddUdpListener(*listener).into());
            if listener.active {
                v.push(
                    RequestType::ActivateListener(ActivateListener {
                        address: listener.address,
                        proxy: ListenerType::Udp.into(),
                        from_scm: false,
                    })
                    .into(),
                );
            }
        }

        for cluster in self.clusters.values() {
            v.push(RequestType::AddCluster(cluster.clone()).into());
        }

        for front in self.http_fronts.values() {
            v.push(RequestType::AddHttpFrontend(front.clone().into()).into());
        }

        for (front, certs) in self.certificates.iter() {
            for certificate_and_key in certs.values() {
                v.push(
                    RequestType::AddCertificate(AddCertificate {
                        address: SocketAddress::from(*front),
                        certificate: certificate_and_key.clone(),
                        expired_at: None,
                    })
                    .into(),
                );
            }
        }

        for front in self.https_fronts.values() {
            v.push(RequestType::AddHttpsFrontend(front.clone().into()).into());
        }

        for front_list in self.tcp_fronts.values() {
            for front in front_list {
                v.push(RequestType::AddTcpFrontend(front.clone().into()).into());
            }
        }

        for front_list in self.udp_fronts.values() {
            for front in front_list {
                v.push(RequestType::AddUdpFrontend(front.clone().into()).into());
            }
        }

        for backend_list in self.backends.values() {
            for backend in backend_list {
                v.push(RequestType::AddBackend(backend.clone().to_add_backend()).into());
            }
        }

        // Bootstrap round-trip: replaying `generate_requests` into a fresh,
        // empty `ConfigState` must reconstruct `self`'s maps exactly. This is
        // the property SaveState/LoadState and worker bootstrap depend on. We
        // compare the business maps only (not `request_counts`, which is
        // bookkeeping mutated by `dispatch`).
        #[cfg(debug_assertions)]
        {
            let mut replayed = ConfigState::new();
            for request in &v {
                debug_assert!(
                    replayed.dispatch(request).is_ok(),
                    "every request from generate_requests must replay cleanly"
                );
            }
            debug_assert!(
                replayed.clusters == self.clusters
                    && replayed.backends == self.backends
                    && replayed.http_listeners == self.http_listeners
                    && replayed.https_listeners == self.https_listeners
                    && replayed.tcp_listeners == self.tcp_listeners
                    && replayed.http_fronts == self.http_fronts
                    && replayed.https_fronts == self.https_fronts
                    && replayed.tcp_fronts == self.tcp_fronts
                    && replayed.certificates == self.certificates,
                "replaying generate_requests into a fresh state must reproduce self"
            );
        }

        v
    }

    pub fn generate_activate_requests(&self) -> Vec<Request> {
        let mut v: Vec<Request> = Vec::new();
        for front in self
            .http_listeners
            .iter()
            .filter(|(_, listener)| listener.active)
            .map(|(k, _)| k)
        {
            v.push(
                RequestType::ActivateListener(ActivateListener {
                    address: SocketAddress::from(*front),
                    proxy: ListenerType::Http.into(),
                    from_scm: false,
                })
                .into(),
            );
        }

        for front in self
            .https_listeners
            .iter()
            .filter(|(_, listener)| listener.active)
            .map(|(k, _)| k)
        {
            v.push(
                RequestType::ActivateListener(ActivateListener {
                    address: SocketAddress::from(*front),
                    proxy: ListenerType::Https.into(),
                    from_scm: false,
                })
                .into(),
            );
        }
        for front in self
            .tcp_listeners
            .iter()
            .filter(|(_, listener)| listener.active)
            .map(|(k, _)| k)
        {
            v.push(
                RequestType::ActivateListener(ActivateListener {
                    address: SocketAddress::from(*front),
                    proxy: ListenerType::Tcp.into(),
                    from_scm: false,
                })
                .into(),
            );
        }
        for front in self
            .udp_listeners
            .iter()
            .filter(|(_, listener)| listener.active)
            .map(|(k, _)| k)
        {
            v.push(
                RequestType::ActivateListener(ActivateListener {
                    address: SocketAddress::from(*front),
                    proxy: ListenerType::Udp.into(),
                    from_scm: false,
                })
                .into(),
            );
        }

        // Symmetry with the active-listener census: exactly one ActivateListener
        // is emitted per active listener across the four maps, no more, no less.
        #[cfg(debug_assertions)]
        {
            let active_listeners = self.http_listeners.values().filter(|l| l.active).count()
                + self.https_listeners.values().filter(|l| l.active).count()
                + self.tcp_listeners.values().filter(|l| l.active).count()
                + self.udp_listeners.values().filter(|l| l.active).count();
            debug_assert_eq!(
                v.len(),
                active_listeners,
                "generate_activate_requests emits one request per active listener"
            );
            debug_assert!(
                v.iter()
                    .all(|r| matches!(r.request_type, Some(RequestType::ActivateListener(_)))),
                "generate_activate_requests must emit only ActivateListener requests"
            );
        }

        v
    }

    pub fn diff(&self, other: &ConfigState) -> Vec<Request> {
        //pub tcp_listeners:   HashMap<SocketAddr, (TcpListener, bool)>,
        let my_tcp_listeners: HashSet<&SocketAddr> = self.tcp_listeners.keys().collect();
        let their_tcp_listeners: HashSet<&SocketAddr> = other.tcp_listeners.keys().collect();
        let removed_tcp_listeners = my_tcp_listeners.difference(&their_tcp_listeners);
        let added_tcp_listeners = their_tcp_listeners.difference(&my_tcp_listeners);

        let my_udp_listeners: HashSet<&SocketAddr> = self.udp_listeners.keys().collect();
        let their_udp_listeners: HashSet<&SocketAddr> = other.udp_listeners.keys().collect();
        let removed_udp_listeners = my_udp_listeners.difference(&their_udp_listeners);
        let added_udp_listeners = their_udp_listeners.difference(&my_udp_listeners);

        let my_http_listeners: HashSet<&SocketAddr> = self.http_listeners.keys().collect();
        let their_http_listeners: HashSet<&SocketAddr> = other.http_listeners.keys().collect();
        let removed_http_listeners = my_http_listeners.difference(&their_http_listeners);
        let added_http_listeners = their_http_listeners.difference(&my_http_listeners);

        let my_https_listeners: HashSet<&SocketAddr> = self.https_listeners.keys().collect();
        let their_https_listeners: HashSet<&SocketAddr> = other.https_listeners.keys().collect();
        let removed_https_listeners = my_https_listeners.difference(&their_https_listeners);
        let added_https_listeners = their_https_listeners.difference(&my_https_listeners);

        let mut v: Vec<Request> = vec![];

        for address in removed_tcp_listeners {
            if self.tcp_listeners[*address].active {
                v.push(
                    RequestType::DeactivateListener(DeactivateListener {
                        address: SocketAddress::from(**address),
                        proxy: ListenerType::Tcp.into(),
                        to_scm: false,
                    })
                    .into(),
                );
            }

            v.push(
                RequestType::RemoveListener(RemoveListener {
                    address: SocketAddress::from(**address),
                    proxy: ListenerType::Tcp.into(),
                })
                .into(),
            );
        }

        for address in added_tcp_listeners.clone() {
            v.push(RequestType::AddTcpListener(other.tcp_listeners[*address]).into());

            if other.tcp_listeners[*address].active {
                v.push(
                    RequestType::ActivateListener(ActivateListener {
                        address: SocketAddress::from(**address),
                        proxy: ListenerType::Tcp.into(),
                        from_scm: false,
                    })
                    .into(),
                );
            }
        }

        for address in removed_udp_listeners {
            if self.udp_listeners[*address].active {
                v.push(
                    RequestType::DeactivateListener(DeactivateListener {
                        address: SocketAddress::from(**address),
                        proxy: ListenerType::Udp.into(),
                        to_scm: false,
                    })
                    .into(),
                );
            }

            v.push(
                RequestType::RemoveListener(RemoveListener {
                    address: SocketAddress::from(**address),
                    proxy: ListenerType::Udp.into(),
                })
                .into(),
            );
        }

        for address in added_udp_listeners.clone() {
            v.push(RequestType::AddUdpListener(other.udp_listeners[*address]).into());

            if other.udp_listeners[*address].active {
                v.push(
                    RequestType::ActivateListener(ActivateListener {
                        address: SocketAddress::from(**address),
                        proxy: ListenerType::Udp.into(),
                        from_scm: false,
                    })
                    .into(),
                );
            }
        }

        for address in removed_http_listeners {
            if self.http_listeners[*address].active {
                v.push(
                    RequestType::DeactivateListener(DeactivateListener {
                        address: SocketAddress::from(**address),
                        proxy: ListenerType::Http.into(),
                        to_scm: false,
                    })
                    .into(),
                );
            }

            v.push(
                RequestType::RemoveListener(RemoveListener {
                    address: SocketAddress::from(**address),
                    proxy: ListenerType::Http.into(),
                })
                .into(),
            );
        }

        for address in added_http_listeners.clone() {
            v.push(RequestType::AddHttpListener(other.http_listeners[*address].clone()).into());

            if other.http_listeners[*address].active {
                v.push(
                    RequestType::ActivateListener(ActivateListener {
                        address: SocketAddress::from(**address),
                        proxy: ListenerType::Http.into(),
                        from_scm: false,
                    })
                    .into(),
                );
            }
        }

        for address in removed_https_listeners {
            if self.https_listeners[*address].active {
                v.push(
                    RequestType::DeactivateListener(DeactivateListener {
                        address: SocketAddress::from(**address),
                        proxy: ListenerType::Https.into(),
                        to_scm: false,
                    })
                    .into(),
                );
            }

            v.push(
                RequestType::RemoveListener(RemoveListener {
                    address: SocketAddress::from(**address),
                    proxy: ListenerType::Https.into(),
                })
                .into(),
            );
        }

        for address in added_https_listeners.clone() {
            v.push(RequestType::AddHttpsListener(other.https_listeners[*address].clone()).into());

            if other.https_listeners[*address].active {
                v.push(
                    RequestType::ActivateListener(ActivateListener {
                        address: SocketAddress::from(**address),
                        proxy: ListenerType::Https.into(),
                        from_scm: false,
                    })
                    .into(),
                );
            }
        }

        for addr in my_tcp_listeners.intersection(&their_tcp_listeners) {
            let my_listener = &self.tcp_listeners[*addr];
            let their_listener = &other.tcp_listeners[*addr];

            if my_listener != their_listener {
                v.push(
                    RequestType::RemoveListener(RemoveListener {
                        address: SocketAddress::from(**addr),
                        proxy: ListenerType::Tcp.into(),
                    })
                    .into(),
                );
                // any added listener should be unactive
                let mut listener_to_add = *their_listener;
                listener_to_add.active = false;
                v.push(RequestType::AddTcpListener(listener_to_add).into());

                // The Remove + Add(active=false) above wipes the listener's
                // active state. Re-emit an ActivateListener whenever the target
                // state keeps it active, otherwise a config change on a still
                // active listener would silently deactivate it on replay. This
                // subsumes the newly-active (`!my.active && their.active`) case:
                // a differing `active` flag always makes the listeners unequal.
                if their_listener.active {
                    v.push(
                        RequestType::ActivateListener(ActivateListener {
                            address: SocketAddress::from(**addr),
                            proxy: ListenerType::Tcp.into(),
                            from_scm: false,
                        })
                        .into(),
                    );
                }
            }

            if my_listener.active && !their_listener.active {
                v.push(
                    RequestType::DeactivateListener(DeactivateListener {
                        address: SocketAddress::from(**addr),
                        proxy: ListenerType::Tcp.into(),
                        to_scm: false,
                    })
                    .into(),
                );
            }
        }

        for addr in my_udp_listeners.intersection(&their_udp_listeners) {
            let my_listener = &self.udp_listeners[*addr];
            let their_listener = &other.udp_listeners[*addr];

            if my_listener != their_listener {
                v.push(
                    RequestType::RemoveListener(RemoveListener {
                        address: SocketAddress::from(**addr),
                        proxy: ListenerType::Udp.into(),
                    })
                    .into(),
                );
                // any added listener should be unactive
                let mut listener_to_add = *their_listener;
                listener_to_add.active = false;
                v.push(RequestType::AddUdpListener(listener_to_add).into());

                // The Remove + Add(active=false) above wipes the listener's
                // active state. Re-emit an ActivateListener whenever the target
                // state keeps it active, otherwise a config change on a still
                // active listener would silently deactivate it on replay. This
                // subsumes the newly-active (`!my.active && their.active`) case:
                // a differing `active` flag always makes the listeners unequal.
                if their_listener.active {
                    v.push(
                        RequestType::ActivateListener(ActivateListener {
                            address: SocketAddress::from(**addr),
                            proxy: ListenerType::Udp.into(),
                            from_scm: false,
                        })
                        .into(),
                    );
                }
            }

            if my_listener.active && !their_listener.active {
                v.push(
                    RequestType::DeactivateListener(DeactivateListener {
                        address: SocketAddress::from(**addr),
                        proxy: ListenerType::Udp.into(),
                        to_scm: false,
                    })
                    .into(),
                );
            }
        }

        for addr in my_http_listeners.intersection(&their_http_listeners) {
            let my_listener = &self.http_listeners[*addr];
            let their_listener = &other.http_listeners[*addr];

            if my_listener != their_listener {
                v.push(
                    RequestType::RemoveListener(RemoveListener {
                        address: SocketAddress::from(**addr),
                        proxy: ListenerType::Http.into(),
                    })
                    .into(),
                );
                // any added listener should be unactive
                let mut listener_to_add = their_listener.clone();
                listener_to_add.active = false;
                v.push(RequestType::AddHttpListener(listener_to_add).into());

                // The Remove + Add(active=false) above wipes the listener's
                // active state. Re-emit an ActivateListener whenever the target
                // state keeps it active, otherwise a config change on a still
                // active listener would silently deactivate it on replay. This
                // subsumes the newly-active (`!my.active && their.active`) case:
                // a differing `active` flag always makes the listeners unequal.
                if their_listener.active {
                    v.push(
                        RequestType::ActivateListener(ActivateListener {
                            address: SocketAddress::from(**addr),
                            proxy: ListenerType::Http.into(),
                            from_scm: false,
                        })
                        .into(),
                    );
                }
            }

            if my_listener.active && !their_listener.active {
                v.push(
                    RequestType::DeactivateListener(DeactivateListener {
                        address: SocketAddress::from(**addr),
                        proxy: ListenerType::Http.into(),
                        to_scm: false,
                    })
                    .into(),
                );
            }
        }

        for addr in my_https_listeners.intersection(&their_https_listeners) {
            let my_listener = &self.https_listeners[*addr];
            let their_listener = &other.https_listeners[*addr];

            if my_listener != their_listener {
                v.push(
                    RequestType::RemoveListener(RemoveListener {
                        address: SocketAddress::from(**addr),
                        proxy: ListenerType::Https.into(),
                    })
                    .into(),
                );
                // any added listener should be unactive
                let mut listener_to_add = their_listener.clone();
                listener_to_add.active = false;
                v.push(RequestType::AddHttpsListener(listener_to_add).into());

                // The Remove + Add(active=false) above wipes the listener's
                // active state. Re-emit an ActivateListener whenever the target
                // state keeps it active, otherwise a config change on a still
                // active listener would silently deactivate it on replay. This
                // subsumes the newly-active (`!my.active && their.active`) case:
                // a differing `active` flag always makes the listeners unequal.
                if their_listener.active {
                    v.push(
                        RequestType::ActivateListener(ActivateListener {
                            address: SocketAddress::from(**addr),
                            proxy: ListenerType::Https.into(),
                            from_scm: false,
                        })
                        .into(),
                    );
                }
            }

            if my_listener.active && !their_listener.active {
                v.push(
                    RequestType::DeactivateListener(DeactivateListener {
                        address: SocketAddress::from(**addr),
                        proxy: ListenerType::Https.into(),
                        to_scm: false,
                    })
                    .into(),
                );
            }
        }

        for (cluster_id, res) in diff_map(self.clusters.iter(), other.clusters.iter()) {
            match res {
                DiffResult::Added | DiffResult::Changed => v.push(
                    RequestType::AddCluster(other.clusters.get(cluster_id).unwrap().clone()).into(),
                ),
                DiffResult::Removed => {
                    v.push(RequestType::RemoveCluster(cluster_id.to_string()).into())
                }
            }
        }

        for ((cluster_id, backend_id), res) in diff_map(
            self.backends.iter().flat_map(|(cluster_id, v)| {
                v.iter()
                    .map(move |backend| ((cluster_id, &backend.backend_id), backend))
            }),
            other.backends.iter().flat_map(|(cluster_id, v)| {
                v.iter()
                    .map(move |backend| ((cluster_id, &backend.backend_id), backend))
            }),
        ) {
            match res {
                DiffResult::Added => {
                    let backend = other
                        .backends
                        .get(cluster_id)
                        .and_then(|v| v.iter().find(|b| &b.backend_id == backend_id))
                        .unwrap();
                    v.push(RequestType::AddBackend(backend.clone().to_add_backend()).into());
                }
                DiffResult::Removed => {
                    let backend = self
                        .backends
                        .get(cluster_id)
                        .and_then(|v| v.iter().find(|b| &b.backend_id == backend_id))
                        .unwrap();

                    v.push(
                        RequestType::RemoveBackend(RemoveBackend {
                            cluster_id: backend.cluster_id.clone(),
                            backend_id: backend.backend_id.clone(),
                            address: SocketAddress::from(backend.address),
                        })
                        .into(),
                    );
                }
                DiffResult::Changed => {
                    let backend = self
                        .backends
                        .get(cluster_id)
                        .and_then(|v| v.iter().find(|b| &b.backend_id == backend_id))
                        .unwrap();

                    v.push(
                        RequestType::RemoveBackend(RemoveBackend {
                            cluster_id: backend.cluster_id.clone(),
                            backend_id: backend.backend_id.clone(),
                            address: SocketAddress::from(backend.address),
                        })
                        .into(),
                    );

                    let backend = other
                        .backends
                        .get(cluster_id)
                        .and_then(|v| v.iter().find(|b| &b.backend_id == backend_id))
                        .unwrap();
                    v.push(RequestType::AddBackend(backend.clone().to_add_backend()).into());
                }
            }
        }

        let mut my_http_fronts: HashSet<(&str, &HttpFrontend)> = HashSet::new();
        for (route, front) in self.http_fronts.iter() {
            my_http_fronts.insert((route, front));
        }
        let mut their_http_fronts: HashSet<(&str, &HttpFrontend)> = HashSet::new();
        for (route, front) in other.http_fronts.iter() {
            their_http_fronts.insert((route, front));
        }

        let removed_http_fronts = my_http_fronts.difference(&their_http_fronts);
        let added_http_fronts = their_http_fronts.difference(&my_http_fronts);

        for &(_, front) in removed_http_fronts {
            v.push(RequestType::RemoveHttpFrontend(front.clone().into()).into());
        }

        for &(_, front) in added_http_fronts {
            v.push(RequestType::AddHttpFrontend(front.clone().into()).into());
        }

        let mut my_https_fronts: HashSet<(&String, &HttpFrontend)> = HashSet::new();
        for (route, front) in self.https_fronts.iter() {
            my_https_fronts.insert((route, front));
        }
        let mut their_https_fronts: HashSet<(&String, &HttpFrontend)> = HashSet::new();
        for (route, front) in other.https_fronts.iter() {
            their_https_fronts.insert((route, front));
        }
        let removed_https_fronts = my_https_fronts.difference(&their_https_fronts);
        let added_https_fronts = their_https_fronts.difference(&my_https_fronts);

        for &(_, front) in removed_https_fronts {
            v.push(RequestType::RemoveHttpsFrontend(front.clone().into()).into());
        }

        for &(_, front) in added_https_fronts {
            v.push(RequestType::AddHttpsFrontend(front.clone().into()).into());
        }

        let mut my_tcp_fronts: HashSet<(&ClusterId, &TcpFrontend)> = HashSet::new();
        for (cluster_id, front_list) in self.tcp_fronts.iter() {
            for front in front_list.iter() {
                my_tcp_fronts.insert((cluster_id, front));
            }
        }
        let mut their_tcp_fronts: HashSet<(&ClusterId, &TcpFrontend)> = HashSet::new();
        for (cluster_id, front_list) in other.tcp_fronts.iter() {
            for front in front_list.iter() {
                their_tcp_fronts.insert((cluster_id, front));
            }
        }

        let removed_tcp_fronts = my_tcp_fronts.difference(&their_tcp_fronts);
        let added_tcp_fronts = their_tcp_fronts.difference(&my_tcp_fronts);

        for &(_, front) in removed_tcp_fronts {
            v.push(RequestType::RemoveTcpFrontend(front.clone().into()).into());
        }

        for &(_, front) in added_tcp_fronts {
            v.push(RequestType::AddTcpFrontend(front.clone().into()).into());
        }

        let mut my_udp_fronts: HashSet<(&ClusterId, &UdpFrontend)> = HashSet::new();
        for (cluster_id, front_list) in self.udp_fronts.iter() {
            for front in front_list.iter() {
                my_udp_fronts.insert((cluster_id, front));
            }
        }
        let mut their_udp_fronts: HashSet<(&ClusterId, &UdpFrontend)> = HashSet::new();
        for (cluster_id, front_list) in other.udp_fronts.iter() {
            for front in front_list.iter() {
                their_udp_fronts.insert((cluster_id, front));
            }
        }

        let removed_udp_fronts = my_udp_fronts.difference(&their_udp_fronts);
        let added_udp_fronts = their_udp_fronts.difference(&my_udp_fronts);

        for &(_, front) in removed_udp_fronts {
            v.push(RequestType::RemoveUdpFrontend(front.clone().into()).into());
        }

        for &(_, front) in added_udp_fronts {
            v.push(RequestType::AddUdpFrontend(front.clone().into()).into());
        }

        //pub certificates:    HashMap<SocketAddr, HashMap<CertificateFingerprint, (CertificateAndKey, Vec<String>)>>,
        let my_certificates: HashSet<(SocketAddr, &Fingerprint)> = HashSet::from_iter(
            self.certificates
                .iter()
                .flat_map(|(addr, certs)| repeat(*addr).zip(certs.keys())),
        );
        let their_certificates: HashSet<(SocketAddr, &Fingerprint)> = HashSet::from_iter(
            other
                .certificates
                .iter()
                .flat_map(|(addr, certs)| repeat(*addr).zip(certs.keys())),
        );

        let removed_certificates = my_certificates.difference(&their_certificates);
        let added_certificates = their_certificates.difference(&my_certificates);

        for &(address, fingerprint) in removed_certificates {
            v.push(
                RequestType::RemoveCertificate(RemoveCertificate {
                    address: SocketAddress::from(address),
                    fingerprint: fingerprint.to_string(),
                })
                .into(),
            );
        }

        for &(address, fingerprint) in added_certificates {
            if let Some(certificate_and_key) = other
                .certificates
                .get(&address)
                .and_then(|certs| certs.get(fingerprint))
            {
                v.push(
                    RequestType::AddCertificate(AddCertificate {
                        address: SocketAddress::from(address),
                        certificate: certificate_and_key.clone(),
                        expired_at: None,
                    })
                    .into(),
                );
            }
        }

        for address in added_tcp_listeners {
            let listener = &other.tcp_listeners[*address];
            if listener.active {
                v.push(
                    RequestType::ActivateListener(ActivateListener {
                        address: listener.address,
                        proxy: ListenerType::Tcp.into(),
                        from_scm: false,
                    })
                    .into(),
                );
            }
        }

        for address in added_udp_listeners {
            let listener = &other.udp_listeners[*address];
            if listener.active {
                v.push(
                    RequestType::ActivateListener(ActivateListener {
                        address: listener.address,
                        proxy: ListenerType::Udp.into(),
                        from_scm: false,
                    })
                    .into(),
                );
            }
        }

        // Replay symmetry: the request set `diff` emits, when replayed onto a
        // clone of `self`, must reproduce `other`'s routing-relevant maps —
        // listeners included, with their `active` flag. This is the property the
        // hot-reconfig fan-out relies on — a worker applies these requests and
        // must converge on `other`. We verify it in debug by actually replaying;
        // `dispatch`'s own invariant sweep runs on every step.
        //
        // One deliberate normalization: `backends`/`tcp_fronts` are compared
        // after dropping empty buckets. `remove_backend`/`remove_tcp_frontend`
        // leave an empty `Vec` under a cluster key, whereas `other` may have no
        // key at all. An empty bucket emits no requests and is semantically
        // equivalent to an absent one, so we normalize it away before comparing.
        #[cfg(debug_assertions)]
        {
            let mut replayed = self.clone();
            for request in &v {
                // A diff request must always be dispatchable onto `self`.
                debug_assert!(
                    replayed.dispatch(request).is_ok(),
                    "every request emitted by diff must replay cleanly onto self"
                );
            }
            let nonempty = |m: &BTreeMap<ClusterId, Vec<Backend>>| {
                m.iter()
                    .filter(|(_, v)| !v.is_empty())
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<BTreeMap<_, _>>()
            };
            let nonempty_tcp = |m: &HashMap<ClusterId, Vec<TcpFrontend>>| {
                m.iter()
                    .filter(|(_, v)| !v.is_empty())
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<HashMap<_, _>>()
            };
            debug_assert!(
                replayed.clusters == other.clusters
                    && nonempty(&replayed.backends) == nonempty(&other.backends)
                    && replayed.http_fronts == other.http_fronts
                    && replayed.https_fronts == other.https_fronts
                    && nonempty_tcp(&replayed.tcp_fronts) == nonempty_tcp(&other.tcp_fronts)
                    && replayed.certificates == other.certificates
                    && replayed.http_listeners == other.http_listeners
                    && replayed.https_listeners == other.https_listeners
                    && replayed.tcp_listeners == other.tcp_listeners
                    && replayed.udp_listeners == other.udp_listeners,
                "replaying diff(self, other) onto self must reproduce other's clusters/backends/frontends/certificates/listeners"
            );
        }

        v
    }

    // FIXME: what about deny rules?
    pub fn hash_state(&self) -> BTreeMap<ClusterId, u64> {
        let mut hm: HashMap<ClusterId, DefaultHasher> = self
            .clusters
            .keys()
            .map(|cluster_id| {
                let mut hasher = DefaultHasher::new();
                self.clusters.get(cluster_id).hash(&mut hasher);
                if let Some(backends) = self.backends.get(cluster_id) {
                    backends.iter().collect::<BTreeSet<_>>().hash(&mut hasher)
                }
                if let Some(tcp_fronts) = self.tcp_fronts.get(cluster_id) {
                    tcp_fronts.iter().collect::<BTreeSet<_>>().hash(&mut hasher)
                }
                (cluster_id.to_owned(), hasher)
            })
            .collect();

        for front in self.http_fronts.values() {
            if let Some(cluster_id) = &front.cluster_id
                && let Some(hasher) = hm.get_mut(cluster_id)
            {
                front.hash(hasher);
            }
        }

        for front in self.https_fronts.values() {
            if let Some(cluster_id) = &front.cluster_id
                && let Some(hasher) = hm.get_mut(cluster_id)
            {
                front.hash(hasher);
            }
        }

        hm.drain()
            .map(|(cluster_id, hasher)| (cluster_id, hasher.finish()))
            .collect()
    }

    /// Gives details about a given cluster.
    /// Types like `HttpFrontend` are converted into protobuf ones, like `RequestHttpFrontend`
    pub fn cluster_state(&self, cluster_id: &str) -> Option<ClusterInformation> {
        let configuration = self.clusters.get(cluster_id).cloned()?;
        info!("{:?}", configuration);

        let http_frontends: Vec<RequestHttpFrontend> = self
            .http_fronts
            .values()
            .filter(|front| front.cluster_id.as_deref() == Some(cluster_id))
            .map(|front| front.clone().into())
            .collect();

        let https_frontends: Vec<RequestHttpFrontend> = self
            .https_fronts
            .values()
            .filter(|front| front.cluster_id.as_deref() == Some(cluster_id))
            .map(|front| front.clone().into())
            .collect();

        let tcp_frontends: Vec<RequestTcpFrontend> = self
            .tcp_fronts
            .get(cluster_id)
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|front| front.clone().into())
            .collect();

        let udp_frontends: Vec<RequestUdpFrontend> = self
            .udp_fronts
            .get(cluster_id)
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|front| front.clone().into())
            .collect();

        let backends: Vec<AddBackend> = self
            .backends
            .get(cluster_id)
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|backend| backend.clone().into())
            .collect();

        Some(ClusterInformation {
            configuration: Some(configuration),
            http_frontends,
            https_frontends,
            tcp_frontends,
            backends,
            udp_frontends,
        })
    }

    pub fn count_backends(&self) -> usize {
        self.backends.values().fold(0, |acc, v| acc + v.len())
    }

    pub fn count_frontends(&self) -> usize {
        self.http_fronts.values().count()
            + self.https_fronts.values().count()
            + self.tcp_fronts.values().fold(0, |acc, v| acc + v.len())
            + self.udp_fronts.values().fold(0, |acc, v| acc + v.len())
    }

    pub fn get_cluster_ids_by_domain(
        &self,
        hostname: String,
        path: Option<String>,
    ) -> HashSet<ClusterId> {
        let mut cluster_ids: HashSet<ClusterId> = HashSet::new();

        self.http_fronts.values().for_each(|front| {
            if domain_check(&front.hostname, &front.path, &hostname, &path)
                && let Some(id) = &front.cluster_id
            {
                cluster_ids.insert(id.to_string());
            }
        });

        self.https_fronts.values().for_each(|front| {
            if domain_check(&front.hostname, &front.path, &hostname, &path)
                && let Some(id) = &front.cluster_id
            {
                cluster_ids.insert(id.to_string());
            }
        });

        cluster_ids
    }

    pub fn get_certificates(
        &self,
        filters: QueryCertificatesFilters,
    ) -> BTreeMap<String, CertificateAndKey> {
        self.certificates
            .values()
            .flat_map(|hash_map| hash_map.iter())
            .filter(|(fingerprint, cert)| {
                if let Some(domain) = &filters.domain {
                    cert.names.contains(domain)
                } else if let Some(f) = &filters.fingerprint {
                    fingerprint.to_string() == *f
                } else {
                    true
                }
            })
            .map(|(fingerprint, cert)| (fingerprint.to_string(), cert.to_owned()))
            .collect()
    }

    pub fn list_frontends(&self, filters: FrontendFilters) -> ListedFrontends {
        // if no http / https / tcp filter is provided, list all of them
        let list_all = !filters.http && !filters.https && !filters.tcp;

        let mut listed_frontends = ListedFrontends::default();

        if filters.http || list_all {
            for http_frontend in self.http_fronts.iter().filter(|f| {
                if let Some(domain) = &filters.domain {
                    f.1.hostname.contains(domain)
                } else {
                    true
                }
            }) {
                listed_frontends
                    .http_frontends
                    .push(http_frontend.1.to_owned().into());
            }
        }

        if filters.https || list_all {
            for https_frontend in self.https_fronts.iter().filter(|f| {
                if let Some(domain) = &filters.domain {
                    f.1.hostname.contains(domain)
                } else {
                    true
                }
            }) {
                listed_frontends
                    .https_frontends
                    .push(https_frontend.1.to_owned().into());
            }
        }

        if (filters.tcp || list_all) && filters.domain.is_none() {
            for tcp_frontend in self.tcp_fronts.values().flat_map(|v| v.iter()) {
                listed_frontends
                    .tcp_frontends
                    .push(tcp_frontend.to_owned().into())
            }
        }

        // `FrontendFilters` has no dedicated `udp` flag, so UDP frontends ride
        // the same default/all-pass path as TCP: surfaced when no protocol
        // filter is set (`list_all`) or when the `tcp` filter is requested.
        // Datagram frontends carry no hostname, so a `domain` filter excludes
        // them (matching the TCP branch).
        if (filters.tcp || list_all) && filters.domain.is_none() {
            for udp_frontend in self.udp_fronts.values().flat_map(|v| v.iter()) {
                listed_frontends
                    .udp_frontends
                    .push(udp_frontend.to_owned().into())
            }
        }

        listed_frontends
    }

    pub fn list_listeners(&self) -> ListenersList {
        ListenersList {
            http_listeners: self
                .http_listeners
                .iter()
                .map(|(addr, listener)| (addr.to_string(), listener.clone()))
                .collect(),
            https_listeners: self
                .https_listeners
                .iter()
                .map(|(addr, listener)| (addr.to_string(), listener.clone()))
                .collect(),
            tcp_listeners: self
                .tcp_listeners
                .iter()
                .map(|(addr, listener)| (addr.to_string(), *listener))
                .collect(),
            udp_listeners: self
                .udp_listeners
                .iter()
                .map(|(addr, listener)| (addr.to_string(), *listener))
                .collect(),
        }
    }

    // create requests needed for a worker to recreate the state
    pub fn produce_initial_state(&self) -> InitialState {
        let mut worker_requests = Vec::new();
        for (counter, request) in self.generate_requests().into_iter().enumerate() {
            worker_requests.push(WorkerRequest::new(format!("SAVE-{counter}"), request));
        }
        InitialState {
            requests: worker_requests,
        }
    }

    /// generate requests necessary to recreate the state,
    /// in protobuf, to a temp file
    pub fn write_initial_state_to_file(&self, file: &mut File) -> Result<usize, StateError> {
        let initial_state = self.produce_initial_state();
        let count = initial_state.requests.len();

        let bytes_to_write = initial_state.encode_to_vec();
        println!("writing {} in the temp file", bytes_to_write.len());
        file.write_all(&bytes_to_write)
            .map_err(StateError::FileError)?;

        file.sync_all().map_err(StateError::FileError)?;

        Ok(count)
    }

    /// generate requests necessary to recreate the state,
    /// write them in a JSON form in a file, separated by \n\0,
    /// returns the number of written requests
    pub fn write_requests_to_file(&self, file: &mut File) -> Result<usize, StateError> {
        let mut counter = 0usize;
        let requests = self.generate_requests();

        for request in requests {
            let message = WorkerRequest::new(format!("SAVE-{counter}"), request);

            file.write_all(
                &serde_json::to_string(&message)
                    .map(|s| s.into_bytes())
                    .unwrap_or_default(),
            )
            .map_err(StateError::FileError)?;

            file.write_all(&b"\n\0"[..])
                .map_err(StateError::FileError)?;

            if counter.is_multiple_of(1000) {
                info!("writing {} commands to file", counter);
                file.sync_all().map_err(StateError::FileError)?;
            }
            counter += 1;
        }
        file.sync_all().map_err(StateError::FileError)?;

        Ok(counter)
    }
}

/// Validate all H2 flood knobs in an HTTP listener patch.
///
/// Every flood-detector knob (including stream-0 WINDOW_UPDATE) requires a
/// value `>= 1`. Passing `0` would disable the detector entirely and leave the
/// proxy open to CVE-2023-44487 and related attacks. The runtime constructor
/// `H2FloodConfig::new()` applies the same `.max(1)` clamping, but a raw
/// protobuf client can bypass the CLI layer, so we enforce the bound here too.
///
/// `h2_max_concurrent_streams` and `h2_stream_shrink_ratio` are connection-
/// config knobs that also require `>= 1`.
///
/// `h2_graceful_shutdown_deadline_seconds = 0` is intentionally **allowed** —
/// it means "wait forever (no forced close after GOAWAY)".
pub fn validate_h2_flood_knobs_http(patch: &UpdateHttpListenerConfig) -> Result<(), StateError> {
    macro_rules! require_ge1 {
        ($field:expr, $name:literal) => {
            if let Some(0) = $field {
                return Err(StateError::InvalidValue {
                    field: $name,
                    reason: "must be >= 1",
                });
            }
        };
    }
    require_ge1!(
        patch.h2_max_rst_stream_per_window,
        "h2_max_rst_stream_per_window"
    );
    require_ge1!(patch.h2_max_ping_per_window, "h2_max_ping_per_window");
    require_ge1!(
        patch.h2_max_settings_per_window,
        "h2_max_settings_per_window"
    );
    require_ge1!(
        patch.h2_max_empty_data_per_window,
        "h2_max_empty_data_per_window"
    );
    require_ge1!(
        patch.h2_max_continuation_frames,
        "h2_max_continuation_frames"
    );
    require_ge1!(patch.h2_max_glitch_count, "h2_max_glitch_count");
    require_ge1!(
        patch.h2_max_window_update_stream0_per_window,
        "h2_max_window_update_stream0_per_window"
    );
    require_ge1!(patch.h2_max_concurrent_streams, "h2_max_concurrent_streams");
    // Shrink ratio runtime floor is 2 (lib/src/protocol/mux/h2.rs ~448 .max(2));
    // anything lower is silently promoted so reject at control plane.
    if let Some(v) = patch.h2_stream_shrink_ratio
        && v < 2
    {
        return Err(StateError::InvalidValue {
            field: "h2_stream_shrink_ratio",
            reason: "must be >= 2",
        });
    }
    // Lifetime caps and HPACK limits — must be >= 1 or the runtime trips on the
    // first qualifying frame. doc/configure.md advertises "u64 (>= 1)" etc.
    require_ge1!(
        patch.h2_max_rst_stream_lifetime,
        "h2_max_rst_stream_lifetime"
    );
    require_ge1!(
        patch.h2_max_rst_stream_abusive_lifetime,
        "h2_max_rst_stream_abusive_lifetime"
    );
    require_ge1!(
        patch.h2_max_rst_stream_emitted_lifetime,
        "h2_max_rst_stream_emitted_lifetime"
    );
    require_ge1!(patch.h2_max_header_list_size, "h2_max_header_list_size");
    require_ge1!(patch.h2_max_header_table_size, "h2_max_header_table_size");
    require_ge1!(patch.h2_max_header_fields, "h2_max_header_fields");
    Ok(())
}

/// Validate all H2 flood knobs in an HTTPS listener patch (same rules as HTTP).
pub fn validate_h2_flood_knobs_https(patch: &UpdateHttpsListenerConfig) -> Result<(), StateError> {
    macro_rules! require_ge1 {
        ($field:expr, $name:literal) => {
            if let Some(0) = $field {
                return Err(StateError::InvalidValue {
                    field: $name,
                    reason: "must be >= 1",
                });
            }
        };
    }
    require_ge1!(
        patch.h2_max_rst_stream_per_window,
        "h2_max_rst_stream_per_window"
    );
    require_ge1!(patch.h2_max_ping_per_window, "h2_max_ping_per_window");
    require_ge1!(
        patch.h2_max_settings_per_window,
        "h2_max_settings_per_window"
    );
    require_ge1!(
        patch.h2_max_empty_data_per_window,
        "h2_max_empty_data_per_window"
    );
    require_ge1!(
        patch.h2_max_continuation_frames,
        "h2_max_continuation_frames"
    );
    require_ge1!(patch.h2_max_glitch_count, "h2_max_glitch_count");
    require_ge1!(
        patch.h2_max_window_update_stream0_per_window,
        "h2_max_window_update_stream0_per_window"
    );
    require_ge1!(patch.h2_max_concurrent_streams, "h2_max_concurrent_streams");
    if let Some(v) = patch.h2_stream_shrink_ratio
        && v < 2
    {
        return Err(StateError::InvalidValue {
            field: "h2_stream_shrink_ratio",
            reason: "must be >= 2",
        });
    }
    require_ge1!(
        patch.h2_max_rst_stream_lifetime,
        "h2_max_rst_stream_lifetime"
    );
    require_ge1!(
        patch.h2_max_rst_stream_abusive_lifetime,
        "h2_max_rst_stream_abusive_lifetime"
    );
    require_ge1!(
        patch.h2_max_rst_stream_emitted_lifetime,
        "h2_max_rst_stream_emitted_lifetime"
    );
    require_ge1!(patch.h2_max_header_list_size, "h2_max_header_list_size");
    require_ge1!(patch.h2_max_header_table_size, "h2_max_header_table_size");
    require_ge1!(patch.h2_max_header_fields, "h2_max_header_fields");
    Ok(())
}

/// Validate a `sozu_id_header` value against RFC 9110 §5.1 header-name grammar.
///
/// Rejects empty strings and strings containing CR, LF, colon, space, or tab —
/// a conservative approximation of the token grammar that covers all practical
/// injection vectors without a full RFC 9110 tokenizer.
/// Merge a `CustomHttpAnswers` patch into the listener's stored answers,
/// preserving any field not present in the patch.
///
/// Field-mask semantic: `None` in `patch` means "preserve", `Some` means
/// "replace". A no-op patch (all-None) leaves `target` untouched. If `target`
/// is currently `None`, initialize it from the patch (any `None` field stays
/// `None` so hot-upgrade replay sees the same partial state).
pub fn merge_custom_http_answers(
    target: &mut Option<CustomHttpAnswers>,
    patch: &CustomHttpAnswers,
) {
    let current = target.get_or_insert_with(CustomHttpAnswers::default);
    macro_rules! merge_field {
        ($field:ident) => {
            if let Some(ref v) = patch.$field {
                current.$field = Some(v.clone());
            }
        };
    }
    merge_field!(answer_301);
    merge_field!(answer_400);
    merge_field!(answer_401);
    merge_field!(answer_404);
    merge_field!(answer_408);
    merge_field!(answer_413);
    merge_field!(answer_421);
    merge_field!(answer_502);
    merge_field!(answer_503);
    merge_field!(answer_504);
    merge_field!(answer_507);
}

/// Validate an `AlpnProtocols` patch: each value must be "h2" or "http/1.1".
/// Empty values vec is allowed (reset-to-default).
pub fn validate_alpn_protocols(values: &[String]) -> Result<(), StateError> {
    for value in values {
        if value != "h2" && value != "http/1.1" {
            return Err(StateError::InvalidValue {
                field: "alpn_protocols",
                reason: "each value must be \"h2\" or \"http/1.1\"",
            });
        }
    }
    Ok(())
}

/// Validate a `sozu_id_header` value against the RFC 9110 §5.1 `token` grammar:
///
/// ```text
/// token  = 1*tchar
/// tchar  = "!" / "#" / "$" / "%" / "&" / "'" / "*" / "+" / "-" / "." /
///          "^" / "_" / "`" / "|" / "~" / DIGIT / ALPHA
/// ```
///
/// Rejects empty strings, non-ASCII bytes, controls (including CR/LF/tab),
/// separators (including colon and space), and any other non-`tchar` byte.
pub fn validate_sozu_id_header(value: &str) -> Result<(), StateError> {
    if value.is_empty() {
        return Err(StateError::InvalidValue {
            field: "sozu_id_header",
            reason: "must not be empty",
        });
    }
    for b in value.bytes() {
        let is_tchar = b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'!' | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'*'
                    | b'+'
                    | b'-'
                    | b'.'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'|'
                    | b'~'
            );
        if !is_tchar {
            return Err(StateError::InvalidValue {
                field: "sozu_id_header",
                reason: "must be a valid HTTP header name (RFC 9110 §5.1 token: alphanumeric or one of !#$%&'*+-.^_`|~)",
            });
        }
    }
    Ok(())
}

fn domain_check(
    front_hostname: &str,
    front_path_rule: &PathRule,
    hostname: &str,
    path_prefix: &Option<String>,
) -> bool {
    if hostname != front_hostname {
        return false;
    }

    if let Some(path) = &path_prefix {
        return path == &front_path_rule.value;
    }

    true
}

/// Canonicalize a TCP frontend's ALPN list into its sorted, deduplicated set
/// representation. ALPN matching (the worker's `AlpnMatcher::OneOf` in
/// `lib/src/tcp.rs`) is semantically a *set* of protocol names, not an
/// ordered list: comparing `Vec`s directly treats `["h2", "http/1.1"]` and
/// `["http/1.1", "h2"]` as distinct identities, silently admitting a
/// duplicate/overlapping frontend the worker rejects on the next fan-out
/// (sozu-proxy/sozu#1290). Used both to canonicalize a frontend for storage
/// and to canonicalize a candidate for comparison, so `TcpFrontend.alpn` is
/// always already in this form.
fn canonical_tcp_alpn(alpn: &[String]) -> Vec<String> {
    let mut alpn = alpn.to_vec();
    alpn.sort();
    alpn.dedup();
    // POST: strictly increasing (sorted, no adjacent duplicates) — the
    // single property every caller relies on to treat two canonical lists
    // as set-equal via plain `==`.
    debug_assert!(
        alpn.windows(2).all(|pair| pair[0] < pair[1]),
        "canonical_tcp_alpn must return a strictly sorted, duplicate-free list"
    );
    alpn
}

/// Identity predicate for a TCP frontend: `(address, sni, alpn)`, not the
/// whole struct -- two frontends differing only in `tags` still collide
/// here even though they compare unequal as full structs, since they'd
/// still match the exact same wire traffic. `sni` and `alpn` are expected
/// to already be in canonical form (normalized-lowercase sni, sorted+deduped
/// alpn via [`canonical_tcp_alpn`]) on both sides -- this is a plain
/// equality check, not itself a canonicalizer. Shared by
/// `add_tcp_frontend`'s duplicate check and `remove_tcp_frontend`'s
/// retain/postcondition so the three call sites can never drift apart
/// (sozu-proxy/sozu#1290).
fn tcp_frontend_matches(
    front: &TcpFrontend,
    address: SocketAddr,
    sni: &Option<String>,
    alpn: &[String],
) -> bool {
    front.address == address && front.sni == *sni && front.alpn.as_slice() == alpn
}

struct DiffMap<'a, K: Ord, V, I1, I2> {
    my_it: I1,
    other_it: I2,
    my: Option<(K, &'a V)>,
    other: Option<(K, &'a V)>,
}

//fn diff_map<'a, K:Ord, V: PartialEq>(my: &'a BTreeMap<K,V>, other: &'a BTreeMap<K,V>) -> DiffMap<'a,K,V> {
fn diff_map<
    'a,
    K: Ord,
    V: PartialEq,
    I1: Iterator<Item = (K, &'a V)>,
    I2: Iterator<Item = (K, &'a V)>,
>(
    my: I1,
    other: I2,
) -> DiffMap<'a, K, V, I1, I2> {
    DiffMap {
        my_it: my,
        other_it: other,
        my: None,
        other: None,
    }
}

enum DiffResult {
    Added,
    Removed,
    Changed,
}

// this will iterate over the keys of both iterators
// since keys are sorted, it should be easy to see which ones are in common or not
impl<'a, K: Ord, V: PartialEq, I1: Iterator<Item = (K, &'a V)>, I2: Iterator<Item = (K, &'a V)>>
    std::iter::Iterator for DiffMap<'a, K, V, I1, I2>
{
    type Item = (K, DiffResult);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.my.is_none() {
                self.my = self.my_it.next();
            }
            if self.other.is_none() {
                self.other = self.other_it.next();
            }

            match (self.my.take(), self.other.take()) {
                // there are no more elements in my_it, all the next elements in other
                // should be added
                // if other was none, we will stop the iterator there
                (None, other) => return other.map(|(k, _)| (k, DiffResult::Added)),
                // there are no more elements in other_it, all the next elements in my
                // should be removed
                (Some((k, _)), None) => return Some((k, DiffResult::Removed)),
                // element is present in my but not other
                (Some((k1, _v1)), Some((k2, v2))) if k1 < k2 => {
                    self.other = Some((k2, v2));
                    return Some((k1, DiffResult::Removed));
                }
                // element is present in other byt not in my
                (Some((k1, v1)), Some((k2, _v2))) if k1 > k2 => {
                    self.my = Some((k1, v1));
                    return Some((k2, DiffResult::Added));
                }
                (Some((k1, v1)), Some((_k2, v2))) if v1 != v2 => {
                    // key is present in both, if elements have changed
                    // return a value, otherwise go to the next key for both maps
                    return Some((k1, DiffResult::Changed));
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rand::{RngExt, rng, seq::SliceRandom};

    use super::*;
    use crate::proto::command::{
        CustomHttpAnswers, Header, HeaderPosition, HstsConfig, LoadBalancingParams, PathRuleKind,
        RedirectPolicy, RedirectScheme, RequestHttpFrontend, RequestTcpFrontend,
        RequestUdpFrontend, RulePosition, UdpListenerConfig, UpdateUdpListenerConfig,
    };

    fn capture_test_logs(run: impl FnOnce() + Send + 'static) -> String {
        let receiver = std::net::UdpSocket::bind("127.0.0.1:0")
            .expect("test log receiver must bind to a loopback port");
        let target = format!(
            "udp://{}",
            receiver
                .local_addr()
                .expect("test log receiver must have a local address")
        );

        std::thread::spawn(move || {
            crate::logging::Logger::init(
                "state-log-redaction-test".to_owned(),
                "info",
                &target,
                false,
                None,
                None,
                None,
            )
            .expect("test logger must initialize");
            run();
        })
        .join()
        .expect("log-producing test thread must not panic");

        receiver
            .set_nonblocking(true)
            .expect("test log receiver must become nonblocking");
        let mut output = String::new();
        let mut datagram = vec![0; 65_507];
        loop {
            match receiver.recv(&mut datagram) {
                Ok(length) => output.push_str(&String::from_utf8_lossy(&datagram[..length])),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("test log receiver failed: {error}"),
            }
        }
        assert!(!output.is_empty(), "test log capture received no datagrams");
        output
    }

    #[test]
    fn duplicate_certificate_log_redacts_names() {
        const NAME_SECRET: &str = "DUPLICATE_CERTIFICATE_NAME_SECRET_SENTINEL";

        let name = format!("{NAME_SECRET}{}", "x".repeat(4096));
        let name_len = name.len();
        let certificate = CertificateAndKey {
            certificate: include_str!("../assets/certificate.pem").to_owned(),
            key: include_str!("../assets/key.pem").to_owned(),
            certificate_chain: Vec::new(),
            versions: Vec::new(),
            names: vec![name],
        };
        let fingerprint = certificate
            .fingerprint()
            .expect("test certificate fingerprint must be computable")
            .to_string();
        let output = capture_test_logs(move || {
            let mut state = ConfigState::default();
            let add = AddCertificate {
                address: SocketAddress::new_v4(127, 0, 0, 1, 8443),
                certificate,
                expired_at: None,
            };

            state
                .add_certificate(&add)
                .expect("first certificate insertion must succeed");
            state
                .add_certificate(&add)
                .expect("duplicate certificate insertion must be skipped");
        });

        assert!(
            !output.contains(NAME_SECRET),
            "duplicate certificate log leaked certificate name {NAME_SECRET}"
        );
        assert!(
            !output.contains(&fingerprint),
            "duplicate certificate log leaked certificate fingerprint {fingerprint}"
        );
        for metadata in [
            "fingerprint_bytes=32".to_owned(),
            "names_count=1".to_owned(),
            format!("names_bytes={name_len}"),
        ] {
            assert!(
                output.contains(&metadata),
                "duplicate certificate log omitted bounded metadata {metadata}: {output}"
            );
        }
        assert!(
            output.len() <= 512,
            "duplicate certificate log is not bounded: {} bytes",
            output.len()
        );
    }

    #[test]
    fn frontend_state_error_redacts_display_key() {
        const HOSTNAME_SECRET: &str = "STATE_ERROR_HOSTNAME_SECRET_SENTINEL";
        const PATH_SECRET: &str = "STATE_ERROR_PATH_SECRET_SENTINEL";
        const METHOD_SECRET: &str = "STATE_ERROR_METHOD_SECRET_SENTINEL";

        let long_value = |marker: &str| format!("{marker}{}", "x".repeat(4096));
        let front = RequestHttpFrontend {
            cluster_id: Some("cluster".to_owned()),
            address: SocketAddress::new_v4(127, 0, 0, 1, 8080),
            hostname: long_value(HOSTNAME_SECRET),
            path: PathRule {
                kind: PathRuleKind::Prefix as i32,
                value: long_value(PATH_SECRET),
            },
            method: Some(long_value(METHOD_SECRET)),
            position: RulePosition::Tree as i32,
            tags: BTreeMap::new(),
            redirect: None,
            redirect_scheme: None,
            redirect_template: None,
            rewrite_host: None,
            rewrite_path: None,
            rewrite_port: None,
            required_auth: None,
            headers: Vec::new(),
            hsts: None,
        };
        let mut state = ConfigState::default();
        state
            .add_http_frontend(&front)
            .expect("first frontend insertion must succeed");
        assert!(
            state
                .http_fronts
                .keys()
                .next()
                .expect("raw state key must be retained")
                .contains(HOSTNAME_SECRET),
            "state storage must retain the raw operator-facing frontend key"
        );

        let error = state
            .add_http_frontend(&front)
            .expect_err("duplicate frontend insertion must return StateError::Exists");
        let raw_frontend_key = front.to_string();
        match &error {
            StateError::Exists { id, .. } => assert_eq!(
                id, &raw_frontend_key,
                "StateError must preserve the public raw frontend identifier"
            ),
            other => panic!("expected StateError::Exists, got {other:?}"),
        }
        for (label, output) in [
            ("Display", error.to_string()),
            ("Debug", format!("{error:?}")),
        ] {
            for secret in [HOSTNAME_SECRET, PATH_SECRET, METHOD_SECRET] {
                assert!(
                    !output.contains(secret),
                    "StateError {label} leaked frontend marker {secret}"
                );
            }
            let metadata = format!("id_bytes={}", raw_frontend_key.len());
            assert!(
                output.contains(&metadata),
                "StateError {label} omitted bounded metadata {metadata}: {output}"
            );
            assert!(
                output.len() <= 512,
                "StateError {label} output is not bounded: {} bytes",
                output.len()
            );
        }
    }

    #[test]
    fn state_error_formatting_bounds_every_string_bearing_variant() {
        const SECRET: &str = "STATE_ERROR_VARIANT_SECRET_SENTINEL";

        let long_value = || format!("{SECRET}{}", "x".repeat(4096));
        let errors = [
            StateError::NotFound {
                kind: ObjectKind::Backend,
                id: long_value(),
            },
            StateError::Exists {
                kind: ObjectKind::Cluster,
                id: long_value(),
            },
            StateError::AddCertificate(CertificateError::ParsePEMCertificate(long_value())),
            StateError::RemoveCertificate(long_value()),
            StateError::ReplaceCertificate(long_value()),
            StateError::FrontendConversion {
                frontend: long_value(),
                error: long_value(),
            },
            StateError::FileError(std::io::Error::other(long_value())),
            StateError::InvalidTcpFrontend {
                address: "127.0.0.1:443"
                    .parse()
                    .expect("test TCP frontend address must parse"),
                reason: long_value(),
            },
        ];

        for error in errors {
            for (label, output) in [
                ("Display", error.to_string()),
                ("Debug", format!("{error:?}")),
            ] {
                assert!(
                    !output.contains(SECRET),
                    "StateError {label} leaked a string-bearing variant: {output}"
                );
                assert!(
                    output.len() <= 512,
                    "StateError {label} is not bounded: {} bytes",
                    output.len()
                );
            }
        }
    }

    #[test]
    fn tcp_frontend_not_found_error_retains_the_raw_identity() {
        const TCP_SECRET: &str = "STATE_ERROR_TCP_IDENTITY_SECRET_SENTINEL";

        let long_value = |suffix: &str| format!("{TCP_SECRET}_{suffix}{}", "x".repeat(1024));
        let frontend = RequestTcpFrontend {
            cluster_id: long_value("cluster"),
            address: SocketAddress::new_v4(127, 0, 0, 1, 443),
            tags: BTreeMap::from([(long_value("tag-key"), long_value("tag-value"))]),
            sni: Some(long_value("sni")),
            alpn: vec![long_value("alpn")],
        };
        let error = ConfigState::default()
            .remove_tcp_frontend(&frontend)
            .expect_err("removing from a missing cluster must return StateError::NotFound");

        match &error {
            StateError::NotFound { id, .. } => assert!(
                id.contains(TCP_SECRET),
                "StateError must retain the raw TCP frontend identity: {id}"
            ),
            other => panic!("expected StateError::NotFound, got {other:?}"),
        }
        for output in [error.to_string(), format!("{error:?}")] {
            assert!(
                !output.contains(TCP_SECRET),
                "StateError formatting leaked the retained TCP identity: {output}"
            );
            assert!(
                output.len() <= 512,
                "StateError formatting is not bounded: {} bytes",
                output.len()
            );
        }
    }

    #[test]
    fn retained_state_debug_redacts_http_frontend_and_collection_data() {
        const CLUSTER_SECRET: &str = "RETAINED_CLUSTER_SECRET_SENTINEL";
        const HOSTNAME_SECRET: &str = "RETAINED_HOSTNAME_SECRET_SENTINEL";
        const PATH_SECRET: &str = "RETAINED_PATH_SECRET_SENTINEL";
        const METHOD_SECRET: &str = "RETAINED_METHOD_SECRET_SENTINEL";
        const TAG_KEY_SECRET: &str = "RETAINED_TAG_KEY_SECRET_SENTINEL";
        const TAG_VALUE_SECRET: &str = "RETAINED_TAG_VALUE_SECRET_SENTINEL";
        const REDIRECT_TEMPLATE_SECRET: &str = "RETAINED_REDIRECT_TEMPLATE_SECRET_SENTINEL";
        const REWRITE_HOST_SECRET: &str = "RETAINED_REWRITE_HOST_SECRET_SENTINEL";
        const REWRITE_PATH_SECRET: &str = "RETAINED_REWRITE_PATH_SECRET_SENTINEL";
        const HEADER_KEY_SECRET: &str = "RETAINED_HEADER_KEY_SECRET_SENTINEL";
        const HEADER_VALUE_SECRET: &str = "RETAINED_HEADER_VALUE_SECRET_SENTINEL";
        const REQUEST_COUNT_SECRET: &str = "RETAINED_REQUEST_COUNT_SECRET_SENTINEL";

        let long_value = |marker: &str| format!("{marker}{}", "x".repeat(4096));
        let request = RequestHttpFrontend {
            cluster_id: Some(long_value(CLUSTER_SECRET)),
            address: SocketAddress::new_v4(127, 0, 0, 1, 8443),
            hostname: long_value(HOSTNAME_SECRET),
            path: PathRule {
                kind: PathRuleKind::Regex as i32,
                value: long_value(PATH_SECRET),
            },
            method: Some(long_value(METHOD_SECRET)),
            position: RulePosition::Tree as i32,
            tags: BTreeMap::from([(long_value(TAG_KEY_SECRET), long_value(TAG_VALUE_SECRET))]),
            redirect: Some(RedirectPolicy::PermanentRedirect as i32),
            redirect_scheme: Some(RedirectScheme::UseHttps as i32),
            redirect_template: Some(long_value(REDIRECT_TEMPLATE_SECRET)),
            rewrite_host: Some(long_value(REWRITE_HOST_SECRET)),
            rewrite_path: Some(long_value(REWRITE_PATH_SECRET)),
            rewrite_port: Some(9443),
            required_auth: Some(true),
            headers: vec![Header {
                position: HeaderPosition::Both as i32,
                key: long_value(HEADER_KEY_SECRET),
                val: long_value(HEADER_VALUE_SECRET),
            }],
            hsts: Some(HstsConfig {
                enabled: Some(true),
                max_age: Some(31_536_000),
                include_subdomains: Some(true),
                preload: Some(false),
                force_replace_backend: Some(true),
            }),
        };
        let retained = request
            .clone()
            .to_frontend()
            .expect("adversarial frontend must convert");
        let retained_debug = format!("{retained:?}");

        let mut state = ConfigState::default();
        state
            .dispatch(&RequestType::AddHttpFrontend(request).into())
            .expect("adversarial frontend must be retained");
        state
            .request_counts
            .insert(long_value(REQUEST_COUNT_SECRET), 7);
        let retained_key = state
            .http_fronts
            .keys()
            .next()
            .expect("dispatch must retain the frontend's Display key");
        assert!(retained_key.contains(HOSTNAME_SECRET));
        assert!(retained_key.contains(PATH_SECRET));
        assert!(retained_key.contains(METHOD_SECRET));
        let state_debug = format!("{state:?}");

        let secrets = [
            CLUSTER_SECRET,
            HOSTNAME_SECRET,
            PATH_SECRET,
            METHOD_SECRET,
            TAG_KEY_SECRET,
            TAG_VALUE_SECRET,
            REDIRECT_TEMPLATE_SECRET,
            REWRITE_HOST_SECRET,
            REWRITE_PATH_SECRET,
            HEADER_KEY_SECRET,
            HEADER_VALUE_SECRET,
            REQUEST_COUNT_SECRET,
        ];
        for (label, output) in [
            ("HttpFrontend", &retained_debug),
            ("ConfigState", &state_debug),
        ] {
            for secret in secrets {
                assert!(
                    !output.contains(secret),
                    "{label} Debug leaked retained marker {secret}"
                );
            }
            assert!(
                output.len() <= 2048,
                "{label} Debug output is not bounded: {} bytes",
                output.len()
            );
        }

        for metadata in [
            format!("cluster_id_len: Some({})", long_value(CLUSTER_SECRET).len()),
            "address: 127.0.0.1:8443".to_owned(),
            format!("hostname_len: {}", long_value(HOSTNAME_SECRET).len()),
            "path_kind: 1".to_owned(),
            format!("path_len: {}", long_value(PATH_SECRET).len()),
            format!("method_len: Some({})", long_value(METHOD_SECRET).len()),
            "position: Tree".to_owned(),
            "tags_count: Some(1)".to_owned(),
            format!(
                "tags_len: Some({})",
                long_value(TAG_KEY_SECRET).len() + long_value(TAG_VALUE_SECRET).len()
            ),
            "redirect: Some(4)".to_owned(),
            "redirect_scheme: Some(2)".to_owned(),
            format!(
                "redirect_template_len: Some({})",
                long_value(REDIRECT_TEMPLATE_SECRET).len()
            ),
            format!(
                "rewrite_host_len: Some({})",
                long_value(REWRITE_HOST_SECRET).len()
            ),
            format!(
                "rewrite_path_len: Some({})",
                long_value(REWRITE_PATH_SECRET).len()
            ),
            "rewrite_port: Some(9443)".to_owned(),
            "required_auth: Some(true)".to_owned(),
            "headers_count: 1".to_owned(),
            format!(
                "headers_len: {}",
                long_value(HEADER_KEY_SECRET).len() + long_value(HEADER_VALUE_SECRET).len()
            ),
            "hsts: Some(HstsConfig".to_owned(),
        ] {
            assert!(
                retained_debug.contains(&metadata),
                "HttpFrontend Debug omitted safe metadata {metadata}: {retained_debug}"
            );
        }
        for metadata in [
            "http_frontends_count: 1",
            "backends_count: 0",
            "tcp_frontends_count: 0",
            "udp_frontends_count: 0",
            "certificates_count: 0",
            "request_counts_count: 2",
        ] {
            assert!(
                state_debug.contains(metadata),
                "ConfigState Debug omitted count metadata {metadata}: {state_debug}"
            );
        }
    }

    #[test]
    fn serialize() {
        let mut state: ConfigState = Default::default();
        state
            .dispatch(
                &RequestType::AddHttpFrontend(RequestHttpFrontend {
                    cluster_id: Some(String::from("cluster_1")),
                    hostname: String::from("lolcatho.st:8080"),
                    path: PathRule::prefix(String::from("/")),
                    address: SocketAddress::new_v4(0, 0, 0, 0, 8080),
                    position: RulePosition::Tree.into(),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state
            .dispatch(
                &RequestType::AddHttpFrontend(RequestHttpFrontend {
                    cluster_id: Some(String::from("cluster_2")),
                    hostname: String::from("test.local"),
                    path: PathRule::prefix(String::from("/abc")),
                    address: SocketAddress::new_v4(0, 0, 0, 0, 8080),
                    position: RulePosition::Pre.into(),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state
            .dispatch(
                &RequestType::AddBackend(AddBackend {
                    cluster_id: String::from("cluster_1"),
                    backend_id: String::from("cluster_1-0"),
                    address: SocketAddress::new_v4(127, 0, 0, 1, 1026),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state
            .dispatch(
                &RequestType::AddBackend(AddBackend {
                    cluster_id: String::from("cluster_1"),
                    backend_id: String::from("cluster_1-1"),
                    address: SocketAddress::new_v4(127, 0, 0, 1, 1027),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state
            .dispatch(
                &RequestType::AddBackend(AddBackend {
                    cluster_id: String::from("cluster_2"),
                    backend_id: String::from("cluster_2-0"),
                    address: SocketAddress::new_v4(192, 167, 1, 2, 1026),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state
            .dispatch(
                &RequestType::AddBackend(AddBackend {
                    cluster_id: String::from("cluster_1"),
                    backend_id: String::from("cluster_1-3"),
                    address: SocketAddress::new_v4(192, 168, 1, 3, 1027),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state
            .dispatch(
                &RequestType::RemoveBackend(RemoveBackend {
                    cluster_id: String::from("cluster_1"),
                    backend_id: String::from("cluster_1-3"),
                    address: SocketAddress::new_v4(192, 168, 1, 3, 1027),
                })
                .into(),
            )
            .expect("Could not execute request");

        /*
        let encoded = state.encode();
        println!("serialized:\n{}", encoded);

        let new_state: Option<HttpProxy> = decode_str(&encoded);
        println!("deserialized:\n{:?}", new_state);
        assert_eq!(new_state, Some(state));
        */
        //assert!(false);
    }

    #[test]
    fn diff() {
        let mut state: ConfigState = Default::default();
        state
            .dispatch(
                &RequestType::AddHttpFrontend(RequestHttpFrontend {
                    cluster_id: Some(String::from("cluster_1")),
                    hostname: String::from("lolcatho.st:8080"),
                    path: PathRule::prefix(String::from("/")),
                    address: SocketAddress::new_v4(0, 0, 0, 0, 8080),
                    position: RulePosition::Post.into(),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state
            .dispatch(
                &RequestType::AddHttpFrontend(RequestHttpFrontend {
                    cluster_id: Some(String::from("cluster_2")),
                    hostname: String::from("test.local"),
                    path: PathRule::prefix(String::from("/abc")),
                    address: SocketAddress::new_v4(0, 0, 0, 0, 8080),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state
            .dispatch(
                &RequestType::AddBackend(AddBackend {
                    cluster_id: String::from("cluster_1"),
                    backend_id: String::from("cluster_1-0"),
                    address: SocketAddress::new_v4(127, 0, 0, 1, 1026),
                    load_balancing_parameters: Some(LoadBalancingParams::default()),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state
            .dispatch(
                &RequestType::AddBackend(AddBackend {
                    cluster_id: String::from("cluster_1"),
                    backend_id: String::from("cluster_1-1"),
                    address: SocketAddress::new_v4(127, 0, 0, 2, 1027),
                    load_balancing_parameters: Some(LoadBalancingParams::default()),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state
            .dispatch(
                &RequestType::AddBackend(AddBackend {
                    cluster_id: String::from("cluster_2"),
                    backend_id: String::from("cluster_2-0"),
                    address: SocketAddress::new_v4(192, 167, 1, 2, 1026),
                    load_balancing_parameters: Some(LoadBalancingParams::default()),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state
            .dispatch(
                &RequestType::AddCluster(Cluster {
                    cluster_id: String::from("cluster_2"),
                    sticky_session: true,
                    https_redirect: true,
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");

        let mut state2: ConfigState = Default::default();
        state2
            .dispatch(
                &RequestType::AddHttpFrontend(RequestHttpFrontend {
                    cluster_id: Some(String::from("cluster_1")),
                    hostname: String::from("lolcatho.st:8080"),
                    path: PathRule::prefix(String::from("/")),
                    address: SocketAddress::new_v4(0, 0, 0, 0, 8080),
                    position: RulePosition::Post.into(),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state2
            .dispatch(
                &RequestType::AddBackend(AddBackend {
                    cluster_id: String::from("cluster_1"),
                    backend_id: String::from("cluster_1-0"),
                    address: SocketAddress::new_v4(127, 0, 0, 1, 1026),
                    load_balancing_parameters: Some(LoadBalancingParams::default()),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state2
            .dispatch(
                &RequestType::AddBackend(AddBackend {
                    cluster_id: String::from("cluster_1"),
                    backend_id: String::from("cluster_1-1"),
                    address: SocketAddress::new_v4(127, 0, 0, 2, 1027),
                    load_balancing_parameters: Some(LoadBalancingParams::default()),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state2
            .dispatch(
                &RequestType::AddBackend(AddBackend {
                    cluster_id: String::from("cluster_1"),
                    backend_id: String::from("cluster_1-2"),
                    address: SocketAddress::new_v4(127, 0, 0, 2, 1028),
                    load_balancing_parameters: Some(LoadBalancingParams::default()),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state2
            .dispatch(
                &RequestType::AddCluster(Cluster {
                    cluster_id: String::from("cluster_3"),
                    sticky_session: false,
                    https_redirect: false,
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");

        let e: Vec<Request> = vec![
            RequestType::RemoveHttpFrontend(RequestHttpFrontend {
                cluster_id: Some(String::from("cluster_2")),
                hostname: String::from("test.local"),
                path: PathRule::prefix(String::from("/abc")),
                address: SocketAddress::new_v4(0, 0, 0, 0, 8080),
                ..Default::default()
            })
            .into(),
            RequestType::RemoveBackend(RemoveBackend {
                cluster_id: String::from("cluster_2"),
                backend_id: String::from("cluster_2-0"),
                address: SocketAddress::new_v4(192, 167, 1, 2, 1026),
            })
            .into(),
            RequestType::AddBackend(AddBackend {
                cluster_id: String::from("cluster_1"),
                backend_id: String::from("cluster_1-2"),
                address: SocketAddress::new_v4(127, 0, 0, 2, 1028),
                load_balancing_parameters: Some(LoadBalancingParams::default()),
                ..Default::default()
            })
            .into(),
            RequestType::RemoveCluster(String::from("cluster_2")).into(),
            RequestType::AddCluster(Cluster {
                cluster_id: String::from("cluster_3"),
                sticky_session: false,
                https_redirect: false,
                ..Default::default()
            })
            .into(),
        ];
        let expected_diff: HashSet<&Request> = HashSet::from_iter(e.iter());

        let d = state.diff(&state2);
        let diff = HashSet::from_iter(d.iter());
        println!("diff requests:\n{diff:#?}\n");
        println!("expected diff requests:\n{expected_diff:#?}\n");

        let hash1 = state.hash_state();
        let hash2 = state2.hash_state();
        let mut state3 = state.clone();
        state3
            .dispatch(
                &RequestType::AddBackend(AddBackend {
                    cluster_id: String::from("cluster_1"),
                    backend_id: String::from("cluster_1-2"),
                    address: SocketAddress::new_v4(127, 0, 0, 2, 1028),
                    load_balancing_parameters: Some(LoadBalancingParams::default()),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        let hash3 = state3.hash_state();
        println!("state 1 hashes: {hash1:#?}");
        println!("state 2 hashes: {hash2:#?}");
        println!("state 3 hashes: {hash3:#?}");

        assert_eq!(diff, expected_diff);
    }

    #[test]
    fn cluster_ids_by_domain() {
        let mut config = ConfigState::new();
        let http_front_cluster1 = RequestHttpFrontend {
            cluster_id: Some(String::from("MyCluster_1")),
            hostname: String::from("lolcatho.st"),
            path: PathRule::prefix(String::from("")),
            address: SocketAddress::new_v4(0, 0, 0, 0, 8080),
            ..Default::default()
        };

        let https_front_cluster1 = RequestHttpFrontend {
            cluster_id: Some(String::from("MyCluster_1")),
            hostname: String::from("lolcatho.st"),
            path: PathRule::prefix(String::from("")),
            address: SocketAddress::new_v4(0, 0, 0, 0, 8443),
            ..Default::default()
        };

        let http_front_cluster2 = RequestHttpFrontend {
            cluster_id: Some(String::from("MyCluster_2")),
            hostname: String::from("lolcatho.st"),
            path: PathRule::prefix(String::from("/api")),
            address: SocketAddress::new_v4(0, 0, 0, 0, 8080),
            ..Default::default()
        };

        let https_front_cluster2 = RequestHttpFrontend {
            cluster_id: Some(String::from("MyCluster_2")),
            hostname: String::from("lolcatho.st"),
            path: PathRule::prefix(String::from("/api")),
            address: SocketAddress::new_v4(0, 0, 0, 0, 8443),
            ..Default::default()
        };

        config
            .dispatch(&RequestType::AddHttpFrontend(http_front_cluster1).into())
            .expect("Could not execute request");
        config
            .dispatch(&RequestType::AddHttpFrontend(http_front_cluster2).into())
            .expect("Could not execute request");
        config
            .dispatch(&RequestType::AddHttpsFrontend(https_front_cluster1).into())
            .expect("Could not execute request");
        config
            .dispatch(&RequestType::AddHttpsFrontend(https_front_cluster2).into())
            .expect("Could not execute request");

        let mut cluster1_cluster2: HashSet<ClusterId> = HashSet::new();
        cluster1_cluster2.insert(String::from("MyCluster_1"));
        cluster1_cluster2.insert(String::from("MyCluster_2"));

        let mut cluster2: HashSet<ClusterId> = HashSet::new();
        cluster2.insert(String::from("MyCluster_2"));

        let empty: HashSet<ClusterId> = HashSet::new();
        assert_eq!(
            config.get_cluster_ids_by_domain(String::from("lolcatho.st"), None),
            cluster1_cluster2
        );
        assert_eq!(
            config
                .get_cluster_ids_by_domain(String::from("lolcatho.st"), Some(String::from("/api"))),
            cluster2
        );
        assert_eq!(
            config.get_cluster_ids_by_domain(String::from("lolcathost"), None),
            empty
        );
        assert_eq!(
            config
                .get_cluster_ids_by_domain(String::from("lolcathost"), Some(String::from("/sozu"))),
            empty
        );
    }

    #[test]
    fn duplicate_backends() {
        let mut state: ConfigState = Default::default();
        state
            .dispatch(
                &RequestType::AddBackend(AddBackend {
                    cluster_id: String::from("cluster_1"),
                    backend_id: String::from("cluster_1-0"),
                    address: SocketAddress::new_v4(127, 0, 0, 1, 1026),
                    load_balancing_parameters: Some(LoadBalancingParams::default()),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");

        let b = Backend {
            cluster_id: String::from("cluster_1"),
            backend_id: String::from("cluster_1-0"),
            address: "127.0.0.1:1026".parse().unwrap(),
            load_balancing_parameters: Some(LoadBalancingParams::default()),
            sticky_id: Some("sticky".to_string()),
            backup: None,
        };

        state
            .dispatch(&RequestType::AddBackend(b.clone().to_add_backend()).into())
            .expect("Could not execute order");

        assert_eq!(state.backends.get("cluster_1").unwrap(), &vec![b]);
    }

    #[test]
    fn remove_backend() {
        let mut state: ConfigState = Default::default();
        state
            .dispatch(
                &RequestType::AddCluster(Cluster {
                    cluster_id: String::from("cluster_1"),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");

        for i in 0..10 {
            state
                .dispatch(
                    &RequestType::AddBackend(AddBackend {
                        cluster_id: String::from("cluster_1"),
                        backend_id: format!("cluster_1-{i}"),
                        address: SocketAddress::new_v4(127, 0, 0, 1, 1026),
                        ..Default::default()
                    })
                    .into(),
                )
                .expect("Could not execute request");
        }

        assert_eq!(state.backends.get("cluster_1").unwrap().len(), 10);

        let remove_backend_2 = RequestType::RemoveBackend(RemoveBackend {
            cluster_id: String::from("cluster_1"),
            backend_id: String::from("cluster_1-0"),
            address: SocketAddress::new_v4(127, 0, 0, 1, 1026),
        })
        .into();

        let remove_backend_result = state.dispatch(&remove_backend_2);

        assert!(remove_backend_result.is_ok());
        assert_eq!(state.backends.get("cluster_1").unwrap().len(), 9);

        let redundant_remove = state.dispatch(&remove_backend_2);
        assert!(matches!(redundant_remove, Err(StateError::NoChange)));
        assert_eq!(state.backends.get("cluster_1").unwrap().len(), 9);
    }

    #[test]
    fn remove_backends_randomly() {
        let mut state: ConfigState = Default::default();
        state
            .dispatch(
                &RequestType::AddCluster(Cluster {
                    cluster_id: String::from("cluster_1"),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");

        for _ in 0..1000 {
            for i in 0..10 {
                state
                    .dispatch(
                        &RequestType::AddBackend(AddBackend {
                            cluster_id: String::from("cluster_1"),
                            backend_id: format!("cluster_1-{i}"),
                            address: SocketAddress::new_v4(127, 0, 0, 1, 1026),
                            ..Default::default()
                        })
                        .into(),
                    )
                    .expect("Could not execute request");
            }

            let mut rng = rng();
            let mut indexes = vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9];
            indexes.shuffle(&mut rng);
            let random_count = rng.random_range(1..indexes.len());
            let random_indexes: Vec<i32> = indexes.into_iter().take(random_count).collect();

            for j in random_indexes {
                let remove_backend_result = state.dispatch(
                    &RequestType::RemoveBackend(RemoveBackend {
                        cluster_id: String::from("cluster_1"),
                        backend_id: format!("cluster_1-{j}"),
                        address: SocketAddress::new_v4(127, 0, 0, 1, 1026),
                    })
                    .into(),
                );
                assert!(remove_backend_result.is_ok());
            }
        }
    }

    #[test]
    fn listener_diff() {
        let mut state: ConfigState = Default::default();
        let custom_http_answers = Some(CustomHttpAnswers {
            answer_404: Some("test".to_string()),
            ..Default::default()
        });
        state
            .dispatch(
                &RequestType::AddTcpListener(TcpListenerConfig {
                    address: SocketAddress::new_v4(0, 0, 0, 0, 1234),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state
            .dispatch(
                &RequestType::ActivateListener(ActivateListener {
                    address: SocketAddress::new_v4(0, 0, 0, 0, 1234),
                    proxy: ListenerType::Tcp.into(),
                    from_scm: false,
                })
                .into(),
            )
            .expect("Could not execute request");
        state
            .dispatch(
                &RequestType::AddHttpListener(HttpListenerConfig {
                    address: SocketAddress::new_v4(0, 0, 0, 0, 8080),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state
            .dispatch(
                &RequestType::AddHttpsListener(HttpsListenerConfig {
                    address: SocketAddress::new_v4(0, 0, 0, 0, 8443),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state
            .dispatch(
                &RequestType::ActivateListener(ActivateListener {
                    address: SocketAddress::new_v4(0, 0, 0, 0, 8443),
                    proxy: ListenerType::Https.into(),
                    from_scm: false,
                })
                .into(),
            )
            .expect("Could not execute request");

        let mut state2: ConfigState = Default::default();
        state2
            .dispatch(
                &RequestType::AddTcpListener(TcpListenerConfig {
                    address: SocketAddress::new_v4(0, 0, 0, 0, 1234),
                    expect_proxy: true,
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state2
            .dispatch(
                &RequestType::AddHttpListener(HttpListenerConfig {
                    address: SocketAddress::new_v4(0, 0, 0, 0, 8080),
                    http_answers: custom_http_answers.clone(),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state2
            .dispatch(
                &RequestType::ActivateListener(ActivateListener {
                    address: SocketAddress::new_v4(0, 0, 0, 0, 8080),
                    proxy: ListenerType::Http.into(),
                    from_scm: false,
                })
                .into(),
            )
            .expect("Could not execute request");
        state2
            .dispatch(
                &RequestType::AddHttpsListener(HttpsListenerConfig {
                    address: SocketAddress::new_v4(0, 0, 0, 0, 8443),
                    http_answers: custom_http_answers.clone(),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state2
            .dispatch(
                &RequestType::ActivateListener(ActivateListener {
                    address: SocketAddress::new_v4(0, 0, 0, 0, 8443),
                    proxy: ListenerType::Https.into(),
                    from_scm: false,
                })
                .into(),
            )
            .expect("Could not execute request");

        let e: Vec<Request> = vec![
            RequestType::RemoveListener(RemoveListener {
                address: SocketAddress::new_v4(0, 0, 0, 0, 1234),
                proxy: ListenerType::Tcp.into(),
            })
            .into(),
            RequestType::AddTcpListener(TcpListenerConfig {
                address: SocketAddress::new_v4(0, 0, 0, 0, 1234),
                expect_proxy: true,
                ..Default::default()
            })
            .into(),
            RequestType::DeactivateListener(DeactivateListener {
                address: SocketAddress::new_v4(0, 0, 0, 0, 1234),
                proxy: ListenerType::Tcp.into(),
                to_scm: false,
            })
            .into(),
            RequestType::RemoveListener(RemoveListener {
                address: SocketAddress::new_v4(0, 0, 0, 0, 8080),
                proxy: ListenerType::Http.into(),
            })
            .into(),
            RequestType::AddHttpListener(HttpListenerConfig {
                address: SocketAddress::new_v4(0, 0, 0, 0, 8080),
                http_answers: custom_http_answers.clone(),
                ..Default::default()
            })
            .into(),
            RequestType::ActivateListener(ActivateListener {
                address: SocketAddress::new_v4(0, 0, 0, 0, 8080),
                proxy: ListenerType::Http.into(),
                from_scm: false,
            })
            .into(),
            RequestType::RemoveListener(RemoveListener {
                address: SocketAddress::new_v4(0, 0, 0, 0, 8443),
                proxy: ListenerType::Https.into(),
            })
            .into(),
            RequestType::AddHttpsListener(HttpsListenerConfig {
                address: SocketAddress::new_v4(0, 0, 0, 0, 8443),
                http_answers: custom_http_answers.clone(),
                ..Default::default()
            })
            .into(),
            // The 8443 HTTPS listener is active in both states but its content
            // changed (custom answers). The Remove + Add(active=false) above
            // wipes the active flag, so diff must re-emit an ActivateListener to
            // keep the listener live across the hot reconfig. Without it the
            // worker would silently deactivate the listener on replay.
            RequestType::ActivateListener(ActivateListener {
                address: SocketAddress::new_v4(0, 0, 0, 0, 8443),
                proxy: ListenerType::Https.into(),
                from_scm: false,
            })
            .into(),
        ];

        let diff = state.diff(&state2);
        //let diff: HashSet<&RequestContent> = HashSet::from_iter(d.iter());
        println!("expected diff requests:\n{e:#?}\n");
        println!("diff requests:\n{diff:#?}\n");

        let _hash1 = state.hash_state();
        let _hash2 = state2.hash_state();

        assert_eq!(diff, e);

        // Round-trip: replaying diff(state -> state2) onto a clone of `state`
        // must reproduce `state2`'s listener maps EXACTLY, active flag included.
        // This is the hot-reconfig correctness property — a worker applies these
        // requests and must converge on the target state. In particular the 8443
        // HTTPS listener (active in both states, content changed) must come back
        // ACTIVE, which is the bug the ActivateListener re-emission above fixes.
        let mut replayed = state.clone();
        for request in &diff {
            replayed
                .dispatch(request)
                .expect("every diff request must replay cleanly onto the source state");
        }
        assert_eq!(
            replayed.tcp_listeners, state2.tcp_listeners,
            "replayed tcp_listeners must match the target state"
        );
        assert_eq!(
            replayed.http_listeners, state2.http_listeners,
            "replayed http_listeners must match the target state"
        );
        assert_eq!(
            replayed.https_listeners, state2.https_listeners,
            "replayed https_listeners must match the target state"
        );
        // Explicitly assert the still-active listener stays active across the
        // config change (the core of the fixed bug).
        let replayed_8443 = replayed
            .https_listeners
            .get(&SocketAddr::from(SocketAddress::new_v4(0, 0, 0, 0, 8443)))
            .expect("8443 HTTPS listener must exist after replay");
        assert!(
            replayed_8443.active,
            "the 8443 HTTPS listener must stay ACTIVE across a config change"
        );
    }

    #[test]
    fn certificate_retrieval() {
        let mut state: ConfigState = Default::default();
        let certificate_and_key = CertificateAndKey {
            certificate: String::from(include_str!("../assets/certificate.pem")),
            key: String::from(include_str!("../assets/key.pem")),
            certificate_chain: vec![],
            versions: vec![],
            names: vec!["lolcatho.st".to_string()],
        };
        let add_certificate = AddCertificate {
            address: SocketAddress::new_v4(127, 0, 0, 1, 8080),
            certificate: certificate_and_key,
            expired_at: None,
        };
        state
            .dispatch(&RequestType::AddCertificate(add_certificate).into())
            .expect("Could not add certificate");

        println!("state: {state:#?}");

        // let fingerprint: Fingerprint = serde_json::from_str(
        //     "\"ab2618b674e15243fd02a5618c66509e4840ba60e7d64cebec84cdbfeceee0c5\"",
        // )
        // .expect("Could not deserialize the fingerprint");

        let certificates_found_by_fingerprint = state.get_certificates(QueryCertificatesFilters {
            domain: None,
            fingerprint: Some(
                "ab2618b674e15243fd02a5618c66509e4840ba60e7d64cebec84cdbfeceee0c5".to_string(),
            ),
        });

        println!("found certificate: {certificates_found_by_fingerprint:#?}");

        assert!(!certificates_found_by_fingerprint.is_empty());

        let certificate_found_by_domain_name = state.get_certificates(QueryCertificatesFilters {
            domain: Some("lolcatho.st".to_string()),
            fingerprint: None,
        });

        assert!(!certificate_found_by_domain_name.is_empty());
    }

    #[test]
    fn count_backends_across_clusters() {
        let mut state: ConfigState = Default::default();

        assert_eq!(state.count_backends(), 0);

        state
            .dispatch(
                &RequestType::AddBackend(AddBackend {
                    cluster_id: String::from("cluster_1"),
                    backend_id: String::from("cluster_1-0"),
                    address: SocketAddress::new_v4(127, 0, 0, 1, 1026),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        assert_eq!(state.count_backends(), 1);

        state
            .dispatch(
                &RequestType::AddBackend(AddBackend {
                    cluster_id: String::from("cluster_1"),
                    backend_id: String::from("cluster_1-1"),
                    address: SocketAddress::new_v4(127, 0, 0, 1, 1027),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        assert_eq!(state.count_backends(), 2);

        // add backend to a second cluster
        state
            .dispatch(
                &RequestType::AddBackend(AddBackend {
                    cluster_id: String::from("cluster_2"),
                    backend_id: String::from("cluster_2-0"),
                    address: SocketAddress::new_v4(192, 168, 1, 1, 8080),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        assert_eq!(state.count_backends(), 3);

        // remove a backend and verify count decreases
        state
            .dispatch(
                &RequestType::RemoveBackend(RemoveBackend {
                    cluster_id: String::from("cluster_1"),
                    backend_id: String::from("cluster_1-0"),
                    address: SocketAddress::new_v4(127, 0, 0, 1, 1026),
                })
                .into(),
            )
            .expect("Could not execute request");
        assert_eq!(state.count_backends(), 2);
    }

    #[test]
    fn count_frontends_across_types() {
        let mut state: ConfigState = Default::default();

        assert_eq!(state.count_frontends(), 0);

        // add an HTTP frontend
        state
            .dispatch(
                &RequestType::AddHttpFrontend(RequestHttpFrontend {
                    cluster_id: Some(String::from("cluster_1")),
                    hostname: String::from("example.com"),
                    path: PathRule::prefix(String::from("/")),
                    address: SocketAddress::new_v4(0, 0, 0, 0, 8080),
                    position: RulePosition::Tree.into(),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        assert_eq!(state.count_frontends(), 1);

        // add an HTTPS frontend
        state
            .dispatch(
                &RequestType::AddHttpsFrontend(RequestHttpFrontend {
                    cluster_id: Some(String::from("cluster_1")),
                    hostname: String::from("secure.example.com"),
                    path: PathRule::prefix(String::from("/")),
                    address: SocketAddress::new_v4(0, 0, 0, 0, 8443),
                    position: RulePosition::Tree.into(),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        assert_eq!(state.count_frontends(), 2);

        // add a TCP frontend
        state
            .dispatch(
                &RequestType::AddTcpFrontend(RequestTcpFrontend {
                    cluster_id: String::from("cluster_2"),
                    address: SocketAddress::new_v4(0, 0, 0, 0, 5432),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        assert_eq!(state.count_frontends(), 3);

        // add a second TCP frontend on the same cluster
        state
            .dispatch(
                &RequestType::AddTcpFrontend(RequestTcpFrontend {
                    cluster_id: String::from("cluster_2"),
                    address: SocketAddress::new_v4(0, 0, 0, 0, 5433),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        assert_eq!(state.count_frontends(), 4);

        // remove the HTTP frontend
        state
            .dispatch(
                &RequestType::RemoveHttpFrontend(RequestHttpFrontend {
                    cluster_id: Some(String::from("cluster_1")),
                    hostname: String::from("example.com"),
                    path: PathRule::prefix(String::from("/")),
                    address: SocketAddress::new_v4(0, 0, 0, 0, 8080),
                    position: RulePosition::Tree.into(),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        assert_eq!(state.count_frontends(), 3);
    }

    // ── helpers ────────────────────────────────────────────────────────────────

    fn make_https_listener(address: SocketAddress) -> HttpsListenerConfig {
        HttpsListenerConfig {
            address,
            sticky_name: "SOZUBALANCEID".to_owned(),
            front_timeout: 60,
            back_timeout: 30,
            connect_timeout: 3,
            request_timeout: 10,
            ..Default::default()
        }
    }

    fn make_http_listener(address: SocketAddress) -> HttpListenerConfig {
        HttpListenerConfig {
            address,
            sticky_name: "SOZUBALANCEID".to_owned(),
            front_timeout: 60,
            back_timeout: 30,
            connect_timeout: 3,
            request_timeout: 10,
            ..Default::default()
        }
    }

    fn make_tcp_listener(address: SocketAddress) -> TcpListenerConfig {
        TcpListenerConfig {
            address,
            front_timeout: 60,
            back_timeout: 30,
            connect_timeout: 3,
            ..Default::default()
        }
    }

    fn make_udp_listener(address: SocketAddress, active: bool) -> UdpListenerConfig {
        UdpListenerConfig {
            address,
            public_address: None,
            front_timeout: 30,
            back_timeout: 30,
            max_rx_datagram_size: 1500,
            max_flows: 0,
            active,
        }
    }

    /// Mandatory roundtrip guard for the UDP control-plane data model.
    ///
    /// 1. Build a state holding an active UDP listener + UDP frontend, run
    ///    `generate_requests()`, replay every emitted request into a fresh
    ///    `ConfigState`, and assert the two states are byte-for-byte equal.
    /// 2. Prove a `diff()` between an empty state and the UDP-bearing state
    ///    produces requests that, replayed, reconstruct the UDP listener and
    ///    frontend (a hot-add path), and that the reverse diff tears them
    ///    down again.
    #[test]
    fn test_udp_state_roundtrip() {
        let address = SocketAddress::new_v4(127, 0, 0, 1, 5353);

        let mut state = ConfigState::default();
        state
            .dispatch(&RequestType::AddUdpListener(make_udp_listener(address, true)).into())
            .expect("could not add udp listener");
        state
            .dispatch(
                &RequestType::ActivateListener(ActivateListener {
                    address,
                    proxy: ListenerType::Udp.into(),
                    from_scm: false,
                })
                .into(),
            )
            .expect("could not activate udp listener");
        state
            .dispatch(
                &RequestType::AddUdpFrontend(RequestUdpFrontend {
                    cluster_id: "udp_cluster".to_string(),
                    address,
                    tags: BTreeMap::from([("owner".to_string(), "team".to_string())]),
                })
                .into(),
            )
            .expect("could not add udp frontend");

        assert_eq!(state.udp_listeners.len(), 1);
        assert!(state.udp_listeners[&address.into()].active);
        assert_eq!(
            state.udp_fronts.get("udp_cluster").map(Vec::len),
            Some(1usize)
        );

        // `request_counts` is a runtime census side-effect of `dispatch`; it
        // diverges by construction whenever the number/shape of replayed
        // requests differs from the originals, so compare the logical config
        // with the census cleared on both sides.
        let logical = |s: &ConfigState| {
            let mut c = s.clone();
            c.request_counts.clear();
            c
        };

        // 1. generate_requests → replay → equal
        let mut replayed = ConfigState::default();
        for request in state.generate_requests() {
            replayed
                .dispatch(&request)
                .expect("could not replay generated request");
        }
        assert_eq!(
            logical(&state),
            logical(&replayed),
            "UDP listener + frontend must survive generate_requests → replay"
        );

        // 2. diff from empty reconstructs the UDP objects
        let empty = ConfigState::default();
        let mut from_diff = ConfigState::default();
        for request in empty.diff(&state) {
            from_diff
                .dispatch(&request)
                .expect("could not replay diff request");
        }
        assert_eq!(
            logical(&state),
            logical(&from_diff),
            "diff(empty -> state) must reconstruct the UDP listener + frontend"
        );

        // reverse diff tears them back down to empty
        let mut torn_down = state.clone();
        for request in state.diff(&empty) {
            torn_down
                .dispatch(&request)
                .expect("could not replay teardown diff request");
        }
        assert!(
            torn_down.udp_listeners.is_empty(),
            "diff(state -> empty) must remove the UDP listener"
        );
        assert!(
            torn_down
                .udp_fronts
                .get("udp_cluster")
                .map(Vec::is_empty)
                .unwrap_or(true),
            "diff(state -> empty) must remove the UDP frontend"
        );

        // update path: a partial patch mutates the stored listener in place
        state
            .dispatch(
                &RequestType::UpdateUdpListener(UpdateUdpListenerConfig {
                    address,
                    max_flows: Some(4096),
                    front_timeout: Some(15),
                    ..Default::default()
                })
                .into(),
            )
            .expect("could not update udp listener");
        let updated = &state.udp_listeners[&address.into()];
        assert_eq!(updated.max_flows, 4096);
        assert_eq!(updated.front_timeout, 15);
        assert_eq!(
            updated.back_timeout, 30,
            "unpatched field must be preserved"
        );
    }

    /// Mandatory roundtrip + identity guard for SNI/ALPN-scoped TCP
    /// frontends (sozu-proxy/sozu#1279).
    ///
    /// 1. Build a state holding a TCP listener with non-default SNI-preread
    ///    knobs plus two SNI-scoped frontends on the same address, run
    ///    `generate_requests()`, replay every emitted request into a fresh
    ///    `ConfigState`, and assert the two states are byte-for-byte equal.
    /// 2. Prove `diff()` between an empty state and this state reconstructs
    ///    the listener + both frontends (hot-add), and the reverse diff
    ///    tears them back down.
    /// 3. Prove add/remove identity is `(address, sni, alpn)`: removing one
    ///    SNI-scoped sibling leaves the other untouched, and a
    ///    address-only removal (mismatched sni/alpn) is rejected rather
    ///    than silently nuking every frontend at that address.
    #[test]
    fn test_tcp_sni_alpn_state_roundtrip() {
        let address = SocketAddress::new_v4(127, 0, 0, 1, 6443);

        let mut state = ConfigState::default();
        state
            .dispatch(
                &RequestType::AddTcpListener(TcpListenerConfig {
                    address,
                    front_timeout: 60,
                    back_timeout: 30,
                    connect_timeout: 3,
                    active: true,
                    sni_preread_timeout: Some(2),
                    sni_preread_max_bytes: Some(8192),
                    ..Default::default()
                })
                .into(),
            )
            .expect("could not add tcp listener");
        state
            .dispatch(
                &RequestType::ActivateListener(ActivateListener {
                    address,
                    proxy: ListenerType::Tcp.into(),
                    from_scm: false,
                })
                .into(),
            )
            .expect("could not activate tcp listener");

        let frontend_a = RequestTcpFrontend {
            cluster_id: "cluster_sni_a".to_string(),
            address,
            tags: BTreeMap::new(),
            sni: Some("example.com".to_string()),
            alpn: vec!["h2".to_string()],
        };
        let frontend_b = RequestTcpFrontend {
            cluster_id: "cluster_sni_b".to_string(),
            address,
            tags: BTreeMap::new(),
            sni: Some("other.example.com".to_string()),
            alpn: vec![],
        };
        state
            .dispatch(&RequestType::AddTcpFrontend(frontend_a.clone()).into())
            .expect("could not add sni-a tcp frontend");
        state
            .dispatch(&RequestType::AddTcpFrontend(frontend_b.clone()).into())
            .expect("could not add sni-b tcp frontend");

        assert_eq!(state.tcp_listeners.len(), 1);
        assert!(state.tcp_listeners[&address.into()].active);
        assert_eq!(
            state.tcp_listeners[&address.into()].sni_preread_timeout,
            Some(2)
        );
        assert_eq!(
            state.tcp_listeners[&address.into()].sni_preread_max_bytes,
            Some(8192)
        );
        assert_eq!(state.count_tcp_frontends_raw(), 2);

        // `request_counts` is a runtime census side-effect of `dispatch`; it
        // diverges by construction whenever the number/shape of replayed
        // requests differs from the originals, so compare the logical config
        // with the census cleared on both sides.
        let logical = |s: &ConfigState| {
            let mut c = s.clone();
            c.request_counts.clear();
            c
        };

        // 1. generate_requests → replay → equal
        let mut replayed = ConfigState::default();
        for request in state.generate_requests() {
            replayed
                .dispatch(&request)
                .expect("could not replay generated request");
        }
        assert_eq!(
            logical(&state),
            logical(&replayed),
            "SNI/ALPN TCP listener + frontends must survive generate_requests → replay"
        );

        // 2. diff from empty reconstructs the TCP objects
        let empty = ConfigState::default();
        let mut from_diff = ConfigState::default();
        for request in empty.diff(&state) {
            from_diff
                .dispatch(&request)
                .expect("could not replay diff request");
        }
        assert_eq!(
            logical(&state),
            logical(&from_diff),
            "diff(empty -> state) must reconstruct the tcp listener + both sni frontends"
        );

        // reverse diff tears them back down to empty
        let mut torn_down = state.clone();
        for request in state.diff(&empty) {
            torn_down
                .dispatch(&request)
                .expect("could not replay teardown diff request");
        }
        assert!(
            torn_down.tcp_listeners.is_empty(),
            "diff(state -> empty) must remove the tcp listener"
        );
        assert_eq!(
            torn_down.count_tcp_frontends_raw(),
            0,
            "diff(state -> empty) must remove both sni frontends"
        );

        // 3a. an address-only removal (mismatched sni/alpn) must not match
        // either sibling — identity is (address, sni, alpn), not address
        // alone.
        let mismatched_removal = state.dispatch(
            &RequestType::RemoveTcpFrontend(RequestTcpFrontend {
                cluster_id: "cluster_sni_a".to_string(),
                address,
                tags: BTreeMap::new(),
                sni: None,
                alpn: vec![],
            })
            .into(),
        );
        assert!(
            mismatched_removal.is_err(),
            "removing by address alone (no matching sni/alpn) must be rejected"
        );
        assert_eq!(
            state.count_tcp_frontends_raw(),
            2,
            "a rejected mismatched removal must not change either sibling"
        );

        // 3b. removing the exact (address, sni, alpn) identity of frontend_a
        // must drop only that entry and leave frontend_b untouched.
        state
            .dispatch(&RequestType::RemoveTcpFrontend(frontend_a).into())
            .expect("could not remove sni-a tcp frontend");
        assert_eq!(state.count_tcp_frontends_raw(), 1);
        let remaining = state
            .tcp_fronts
            .get("cluster_sni_b")
            .expect("cluster_sni_b bucket must survive");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].sni, Some("other.example.com".to_string()));
    }

    // ── master-side TCP frontend admission parity (sozu-proxy/sozu#1290) ──────
    //
    // The command server mutates master `ConfigState` before fanning a
    // request out to workers, and a worker NACK never rolls the master back.
    // These tests prove the master rejects everything the worker's
    // `validate_new_tcp_front` (`lib/src/tcp.rs`) would reject, BEFORE any
    // mutation -- every rejection case asserts `hash_state()` and
    // `count_tcp_frontends_raw()` (plus the relevant bucket lengths) are
    // byte-for-byte unchanged, not just that the call returned `Err`.

    fn add_tcp_test_cluster(state: &mut ConfigState, cluster_id: &str) {
        state
            .dispatch(
                &RequestType::AddCluster(Cluster {
                    cluster_id: cluster_id.to_string(),
                    sticky_session: false,
                    https_redirect: false,
                    ..Default::default()
                })
                .into(),
            )
            .expect("could not add cluster");
    }

    /// The (address, sni, alpn) identity check must span ALL clusters at a
    /// listener address, not just the candidate's own `cluster_id` bucket --
    /// otherwise the same identity under a different cluster_id is silently
    /// accepted by the master and then rejected by every worker.
    #[test]
    fn add_tcp_frontend_rejects_cross_cluster_duplicate_identity() {
        let address = SocketAddress::new_v4(127, 0, 0, 1, 9001);
        let mut state = ConfigState::new();
        add_tcp_test_cluster(&mut state, "cluster_x");
        add_tcp_test_cluster(&mut state, "cluster_y");

        state
            .dispatch(
                &RequestType::AddTcpFrontend(RequestTcpFrontend {
                    cluster_id: "cluster_x".to_string(),
                    address,
                    tags: BTreeMap::new(),
                    sni: Some("example.com".to_string()),
                    alpn: vec!["h2".to_string()],
                })
                .into(),
            )
            .expect("could not add cluster_x tcp frontend");

        let hash_before = state.hash_state();
        let count_before = state.count_tcp_frontends_raw();

        let result = state.dispatch(
            &RequestType::AddTcpFrontend(RequestTcpFrontend {
                cluster_id: "cluster_y".to_string(),
                address,
                tags: BTreeMap::new(),
                sni: Some("example.com".to_string()),
                alpn: vec!["h2".to_string()],
            })
            .into(),
        );
        assert!(
            result.is_err(),
            "the same (address, sni, alpn) under a different cluster_id must be rejected, \
             got: {result:?}"
        );
        assert_eq!(
            state.hash_state(),
            hash_before,
            "a rejected cross-cluster duplicate must not change the state hash"
        );
        assert_eq!(
            state.count_tcp_frontends_raw(),
            count_before,
            "a rejected cross-cluster duplicate must not change the frontend count"
        );
        assert_eq!(state.tcp_fronts.get("cluster_x").map(Vec::len), Some(1));
        assert_eq!(
            state.tcp_fronts.get("cluster_y").map(Vec::len).unwrap_or(0),
            0
        );
    }

    /// ALPN identity is a *set*: the same protocols in a different order
    /// must still collide as a duplicate, even within one cluster's bucket.
    #[test]
    fn add_tcp_frontend_rejects_reordered_alpn_duplicate() {
        let address = SocketAddress::new_v4(127, 0, 0, 1, 9002);
        let mut state = ConfigState::new();
        add_tcp_test_cluster(&mut state, "cluster_reorder");

        state
            .dispatch(
                &RequestType::AddTcpFrontend(RequestTcpFrontend {
                    cluster_id: "cluster_reorder".to_string(),
                    address,
                    tags: BTreeMap::new(),
                    sni: Some("example.com".to_string()),
                    alpn: vec!["h2".to_string(), "http/1.1".to_string()],
                })
                .into(),
            )
            .expect("could not add first tcp frontend");

        let hash_before = state.hash_state();
        let count_before = state.count_tcp_frontends_raw();

        let result = state.dispatch(
            &RequestType::AddTcpFrontend(RequestTcpFrontend {
                cluster_id: "cluster_reorder".to_string(),
                address,
                tags: BTreeMap::new(),
                sni: Some("example.com".to_string()),
                alpn: vec!["http/1.1".to_string(), "h2".to_string()],
            })
            .into(),
        );
        assert!(
            result.is_err(),
            "the same ALPN set in a different order must be rejected as a duplicate \
             identity, got: {result:?}"
        );
        assert_eq!(state.hash_state(), hash_before);
        assert_eq!(state.count_tcp_frontends_raw(), count_before);
        assert_eq!(
            state.tcp_fronts.get("cluster_reorder").map(Vec::len),
            Some(1)
        );
    }

    /// Symmetric counterpart: removal must also treat ALPN as a set, so a
    /// request differing only in protocol order still matches the stored
    /// (canonical) frontend instead of falling through to `NoChange`.
    #[test]
    fn remove_tcp_frontend_matches_reordered_alpn() {
        let address = SocketAddress::new_v4(127, 0, 0, 1, 9003);
        let mut state = ConfigState::new();
        add_tcp_test_cluster(&mut state, "cluster_remove_reorder");

        state
            .dispatch(
                &RequestType::AddTcpFrontend(RequestTcpFrontend {
                    cluster_id: "cluster_remove_reorder".to_string(),
                    address,
                    tags: BTreeMap::new(),
                    sni: Some("example.com".to_string()),
                    alpn: vec!["h2".to_string(), "http/1.1".to_string()],
                })
                .into(),
            )
            .expect("could not add tcp frontend");

        let result = state.dispatch(
            &RequestType::RemoveTcpFrontend(RequestTcpFrontend {
                cluster_id: "cluster_remove_reorder".to_string(),
                address,
                tags: BTreeMap::new(),
                sni: Some("example.com".to_string()),
                alpn: vec!["http/1.1".to_string(), "h2".to_string()],
            })
            .into(),
        );
        assert!(
            result.is_ok(),
            "removing with the same ALPN set in a different order must succeed, got: {result:?}"
        );
        assert_eq!(state.count_tcp_frontends_raw(), 0);
    }

    /// ALPN-set overlap on the same (address, normalized sni) must be
    /// rejected even across different clusters -- the previous check never
    /// looked outside the candidate's own cluster bucket at all.
    #[test]
    fn add_tcp_frontend_rejects_alpn_overlap_across_clusters_same_sni() {
        let address = SocketAddress::new_v4(127, 0, 0, 1, 9004);
        let mut state = ConfigState::new();
        add_tcp_test_cluster(&mut state, "cluster_overlap_a");
        add_tcp_test_cluster(&mut state, "cluster_overlap_b");

        state
            .dispatch(
                &RequestType::AddTcpFrontend(RequestTcpFrontend {
                    cluster_id: "cluster_overlap_a".to_string(),
                    address,
                    tags: BTreeMap::new(),
                    sni: Some("example.com".to_string()),
                    alpn: vec!["h2".to_string()],
                })
                .into(),
            )
            .expect("could not add cluster_overlap_a tcp frontend");

        let hash_before = state.hash_state();
        let count_before = state.count_tcp_frontends_raw();

        let result = state.dispatch(
            &RequestType::AddTcpFrontend(RequestTcpFrontend {
                cluster_id: "cluster_overlap_b".to_string(),
                address,
                tags: BTreeMap::new(),
                sni: Some("example.com".to_string()),
                alpn: vec!["h2".to_string(), "http/1.1".to_string()],
            })
            .into(),
        );
        assert!(
            result.is_err(),
            "an overlapping ALPN set on the same (address, sni) under a different cluster_id \
             must be rejected, got: {result:?}"
        );
        assert_eq!(state.hash_state(), hash_before);
        assert_eq!(state.count_tcp_frontends_raw(), count_before);
        assert_eq!(
            state
                .tcp_fronts
                .get("cluster_overlap_b")
                .map(Vec::len)
                .unwrap_or(0),
            0
        );
    }

    /// Listener-wide no-SNI/SNI mixing must be enforced in both directions,
    /// across cluster buckets.
    #[test]
    fn add_tcp_frontend_rejects_no_sni_sni_mixing_both_directions() {
        let address_a = SocketAddress::new_v4(127, 0, 0, 1, 9005);
        let address_b = SocketAddress::new_v4(127, 0, 0, 1, 9006);
        let mut state = ConfigState::new();
        add_tcp_test_cluster(&mut state, "cluster_mix_1");
        add_tcp_test_cluster(&mut state, "cluster_mix_2");
        add_tcp_test_cluster(&mut state, "cluster_mix_3");
        add_tcp_test_cluster(&mut state, "cluster_mix_4");

        // Direction 1: a no-SNI frontend exists; adding an SNI-scoped
        // frontend at the SAME address must be rejected.
        state
            .dispatch(
                &RequestType::AddTcpFrontend(RequestTcpFrontend {
                    cluster_id: "cluster_mix_1".to_string(),
                    address: address_a,
                    tags: BTreeMap::new(),
                    sni: None,
                    alpn: vec![],
                })
                .into(),
            )
            .expect("could not add no-sni tcp frontend");
        let hash_before_a = state.hash_state();
        let count_before_a = state.count_tcp_frontends_raw();
        let result_a = state.dispatch(
            &RequestType::AddTcpFrontend(RequestTcpFrontend {
                cluster_id: "cluster_mix_2".to_string(),
                address: address_a,
                tags: BTreeMap::new(),
                sni: Some("example.com".to_string()),
                alpn: vec![],
            })
            .into(),
        );
        assert!(
            result_a.is_err(),
            "an SNI-scoped frontend must not be added to a listener with an existing no-SNI \
             front, got: {result_a:?}"
        );
        assert_eq!(state.hash_state(), hash_before_a);
        assert_eq!(state.count_tcp_frontends_raw(), count_before_a);

        // Direction 2: an SNI-scoped frontend exists; adding a no-SNI
        // frontend at the SAME address must be rejected.
        state
            .dispatch(
                &RequestType::AddTcpFrontend(RequestTcpFrontend {
                    cluster_id: "cluster_mix_3".to_string(),
                    address: address_b,
                    tags: BTreeMap::new(),
                    sni: Some("example.com".to_string()),
                    alpn: vec![],
                })
                .into(),
            )
            .expect("could not add sni-scoped tcp frontend");
        let hash_before_b = state.hash_state();
        let count_before_b = state.count_tcp_frontends_raw();
        let result_b = state.dispatch(
            &RequestType::AddTcpFrontend(RequestTcpFrontend {
                cluster_id: "cluster_mix_4".to_string(),
                address: address_b,
                tags: BTreeMap::new(),
                sni: None,
                alpn: vec![],
            })
            .into(),
        );
        assert!(
            result_b.is_err(),
            "a no-SNI frontend must not be added to a listener with existing SNI-scoped \
             fronts, got: {result_b:?}"
        );
        assert_eq!(state.hash_state(), hash_before_b);
        assert_eq!(state.count_tcp_frontends_raw(), count_before_b);
    }

    /// A malformed SNI pattern must be rejected by the master itself, not
    /// merely by config-load's `to_tcp_front` -- a raw `AddTcpFrontend` over
    /// the command socket, or a `LoadState` replay, bypasses `config.rs`
    /// entirely.
    #[test]
    fn add_tcp_frontend_rejects_malformed_sni_shape() {
        let address = SocketAddress::new_v4(127, 0, 0, 1, 9007);
        let mut state = ConfigState::new();
        add_tcp_test_cluster(&mut state, "cluster_malformed");

        let hash_before = state.hash_state();
        let count_before = state.count_tcp_frontends_raw();

        let result = state.dispatch(
            &RequestType::AddTcpFrontend(RequestTcpFrontend {
                cluster_id: "cluster_malformed".to_string(),
                address,
                tags: BTreeMap::new(),
                // Leading empty label -- rejected by `validate_sni_pattern`.
                sni: Some(".example.com".to_string()),
                alpn: vec![],
            })
            .into(),
        );
        assert!(
            result.is_err(),
            "a malformed SNI pattern must be rejected by the master, not merely by \
             config-load, got: {result:?}"
        );
        assert_eq!(state.hash_state(), hash_before);
        assert_eq!(state.count_tcp_frontends_raw(), count_before);
        assert_eq!(
            state
                .tcp_fronts
                .get("cluster_malformed")
                .map(Vec::len)
                .unwrap_or(0),
            0
        );
    }

    /// A non-empty `alpn` without `sni` must be rejected by the master:
    /// alpn only matches within an SNI-scoped preread, so a no-SNI frontend
    /// would silently ignore its alpn list. Config-load (`to_tcp_front`) and
    /// the worker both reject this shape; a raw `AddTcpFrontend` over the
    /// command socket must not slip past the master.
    #[test]
    fn add_tcp_frontend_rejects_alpn_without_sni() {
        let address = SocketAddress::new_v4(127, 0, 0, 1, 9009);
        let mut state = ConfigState::new();
        add_tcp_test_cluster(&mut state, "cluster_alpn_no_sni");

        let hash_before = state.hash_state();
        let count_before = state.count_tcp_frontends_raw();

        let result = state.dispatch(
            &RequestType::AddTcpFrontend(RequestTcpFrontend {
                cluster_id: "cluster_alpn_no_sni".to_string(),
                address,
                tags: BTreeMap::new(),
                sni: None,
                alpn: vec!["h2".to_string()],
            })
            .into(),
        );
        assert!(
            result.is_err(),
            "a non-empty alpn without sni must be rejected by the master, got: {result:?}"
        );
        assert_eq!(
            state.hash_state(),
            hash_before,
            "a rejected alpn-without-sni add must not change the state hash"
        );
        assert_eq!(
            state.count_tcp_frontends_raw(),
            count_before,
            "a rejected alpn-without-sni add must not change the frontend count"
        );
        assert_eq!(
            state
                .tcp_fronts
                .get("cluster_alpn_no_sni")
                .map(Vec::len)
                .unwrap_or(0),
            0
        );
    }

    /// Worker-parity guard: an exact catch-all and a wildcard catch-all on
    /// the SAME address are DISTINCT (address, sni) keys and must both be
    /// accepted -- the listener-wide admission checks above must not
    /// over-forbid this legal combination by conflating exact and wildcard
    /// SNI as the same key.
    #[test]
    fn add_tcp_frontend_accepts_exact_and_wildcard_catch_all_siblings() {
        let address = SocketAddress::new_v4(127, 0, 0, 1, 9008);
        let mut state = ConfigState::new();
        add_tcp_test_cluster(&mut state, "cluster_exact_catchall");
        add_tcp_test_cluster(&mut state, "cluster_wildcard_catchall");

        state
            .dispatch(
                &RequestType::AddTcpFrontend(RequestTcpFrontend {
                    cluster_id: "cluster_exact_catchall".to_string(),
                    address,
                    tags: BTreeMap::new(),
                    sni: Some("example.com".to_string()),
                    alpn: vec![],
                })
                .into(),
            )
            .expect("could not add exact catch-all tcp frontend");

        let result = state.dispatch(
            &RequestType::AddTcpFrontend(RequestTcpFrontend {
                cluster_id: "cluster_wildcard_catchall".to_string(),
                address,
                tags: BTreeMap::new(),
                sni: Some("*.example.com".to_string()),
                alpn: vec![],
            })
            .into(),
        );
        assert!(
            result.is_ok(),
            "an exact catch-all and a wildcard catch-all on the same address must both be \
             accepted, got: {result:?}"
        );
        assert_eq!(state.count_tcp_frontends_raw(), 2);
    }

    /// `list_frontends` must surface UDP frontends alongside TCP ones. The
    /// proto `FrontendFilters` has no `udp` flag, so UDP rides the default
    /// (all-pass) and `tcp` filters; a `domain` filter excludes both.
    #[test]
    fn list_frontends_includes_udp() {
        let tcp_addr = SocketAddress::new_v4(0, 0, 0, 0, 6379);
        let udp_addr = SocketAddress::new_v4(0, 0, 0, 0, 5353);

        let mut state = ConfigState::default();
        state
            .dispatch(
                &RequestType::AddTcpFrontend(RequestTcpFrontend {
                    cluster_id: "tcp_cluster".to_string(),
                    address: tcp_addr,
                    ..Default::default()
                })
                .into(),
            )
            .expect("could not add tcp frontend");
        state
            .dispatch(
                &RequestType::AddUdpFrontend(RequestUdpFrontend {
                    cluster_id: "udp_cluster".to_string(),
                    address: udp_addr,
                    ..Default::default()
                })
                .into(),
            )
            .expect("could not add udp frontend");

        // default filters (all false, no domain) → list everything, incl. UDP
        let all = state.list_frontends(FrontendFilters::default());
        assert_eq!(all.tcp_frontends.len(), 1, "tcp frontend must be listed");
        assert_eq!(
            all.udp_frontends.len(),
            1,
            "udp frontend must be listed under the default all-pass path"
        );
        assert_eq!(all.udp_frontends[0].cluster_id, "udp_cluster");
        assert_eq!(all.udp_frontends[0].address, udp_addr);

        // explicit `tcp` filter surfaces both TCP and UDP (no `udp` flag exists)
        let tcp_only = state.list_frontends(FrontendFilters {
            tcp: true,
            ..Default::default()
        });
        assert_eq!(tcp_only.tcp_frontends.len(), 1);
        assert_eq!(
            tcp_only.udp_frontends.len(),
            1,
            "udp frontends ride the tcp filter"
        );

        // an `http` filter excludes both TCP and UDP
        let http_only = state.list_frontends(FrontendFilters {
            http: true,
            ..Default::default()
        });
        assert!(http_only.tcp_frontends.is_empty());
        assert!(http_only.udp_frontends.is_empty());

        // a `domain` filter excludes UDP (no hostname), matching TCP behaviour
        let domain_filtered = state.list_frontends(FrontendFilters {
            domain: Some("example.com".to_string()),
            ..Default::default()
        });
        assert!(domain_filtered.tcp_frontends.is_empty());
        assert!(domain_filtered.udp_frontends.is_empty());
    }

    // ── update_https_listener ──────────────────────────────────────────────────

    /// Happy path: patching two H2 flood knobs updates the map entry; all other
    /// fields are left untouched.
    #[test]
    fn update_https_listener_happy_path_h2_knobs() {
        let addr = SocketAddress::new_v4(0, 0, 0, 0, 8443);
        let mut state = ConfigState::new();
        state
            .dispatch(&RequestType::AddHttpsListener(make_https_listener(addr)).into())
            .unwrap();

        let patch = UpdateHttpsListenerConfig {
            address: addr,
            h2_max_rst_stream_per_window: Some(50),
            h2_max_ping_per_window: Some(20),
            ..Default::default()
        };
        state
            .dispatch(&RequestType::UpdateHttpsListener(patch).into())
            .expect("update must succeed");

        let listener = state
            .https_listeners
            .get(&SocketAddr::from(addr))
            .expect("listener must be present");
        assert_eq!(listener.h2_max_rst_stream_per_window, Some(50));
        assert_eq!(listener.h2_max_ping_per_window, Some(20));
        // Untouched fields must be unchanged
        assert_eq!(listener.front_timeout, 60);
        assert_eq!(listener.h2_max_settings_per_window, None);
    }

    /// NotFound: patching a listener address that was never registered returns
    /// `StateError::NotFound`.
    #[test]
    fn update_https_listener_not_found() {
        let mut state = ConfigState::new();
        let patch = UpdateHttpsListenerConfig {
            address: SocketAddress::new_v4(1, 2, 3, 4, 9999),
            h2_max_rst_stream_per_window: Some(50),
            ..Default::default()
        };
        let err = state
            .dispatch(&RequestType::UpdateHttpsListener(patch).into())
            .unwrap_err();
        assert!(
            matches!(
                err,
                StateError::NotFound {
                    kind: ObjectKind::HttpsListener,
                    ..
                }
            ),
            "expected NotFound, got: {err}"
        );
    }

    /// No-op: a patch with only `address` set (all options None) is Ok and does
    /// not change any field.
    #[test]
    fn update_https_listener_noop_patch() {
        let addr = SocketAddress::new_v4(0, 0, 0, 0, 8443);
        let mut state = ConfigState::new();
        let original = make_https_listener(addr);
        state
            .dispatch(&RequestType::AddHttpsListener(original.clone()).into())
            .unwrap();

        let patch = UpdateHttpsListenerConfig {
            address: addr,
            ..Default::default()
        };
        state
            .dispatch(&RequestType::UpdateHttpsListener(patch).into())
            .expect("no-op patch must succeed");

        let listener = state.https_listeners.get(&SocketAddr::from(addr)).unwrap();
        assert_eq!(listener.front_timeout, original.front_timeout);
        assert_eq!(
            listener.h2_max_rst_stream_per_window,
            original.h2_max_rst_stream_per_window
        );
    }

    /// InvalidValue: setting a flood knob to 0 must be rejected.
    #[test]
    fn update_https_listener_invalid_value_flood_knob_zero() {
        let addr = SocketAddress::new_v4(0, 0, 0, 0, 8443);
        let mut state = ConfigState::new();
        state
            .dispatch(&RequestType::AddHttpsListener(make_https_listener(addr)).into())
            .unwrap();

        let patch = UpdateHttpsListenerConfig {
            address: addr,
            h2_max_rst_stream_per_window: Some(0),
            ..Default::default()
        };
        let err = state
            .dispatch(&RequestType::UpdateHttpsListener(patch).into())
            .unwrap_err();
        assert!(
            matches!(
                err,
                StateError::InvalidValue {
                    field: "h2_max_rst_stream_per_window",
                    ..
                }
            ),
            "expected InvalidValue for flood knob 0, got: {err}"
        );
    }

    /// AddCluster: an inline `cluster.health_check` with a CRLF-bearing URI
    /// must be rejected before the cluster lands in `ConfigState`. Without
    /// this guard, TOML reload / SaveState / direct API AddCluster requests
    /// bypass the SetHealthCheck-side check and let an attacker-controlled
    /// health-check URI smuggle CR/LF into outbound HTTP/1.1 probes.
    #[test]
    fn add_cluster_invalid_health_check_uri_rejected() {
        use crate::proto::command::HealthCheckConfig;

        let mut state = ConfigState::new();
        let err = state
            .dispatch(
                &RequestType::AddCluster(Cluster {
                    cluster_id: String::from("evil_cluster"),
                    health_check: Some(HealthCheckConfig {
                        uri: String::from("/foo\r\nGET /admin"),
                        interval: 5_000,
                        timeout: 1_000,
                        healthy_threshold: 2,
                        unhealthy_threshold: 2,
                        ..Default::default()
                    }),
                    ..Default::default()
                })
                .into(),
            )
            .unwrap_err();

        assert!(
            matches!(
                err,
                StateError::InvalidValue {
                    field: "health_check",
                    ..
                }
            ),
            "expected InvalidValue for CRLF-bearing health-check URI, got: {err}"
        );
        assert!(
            !state.clusters.contains_key("evil_cluster"),
            "cluster must not be inserted when health_check fails validation",
        );
    }

    /// ALPN validation: reject unknown ALPN values.
    #[test]
    fn update_https_listener_alpn_unknown_value_rejected() {
        use crate::proto::command::AlpnProtocols;

        let addr = SocketAddress::new_v4(0, 0, 0, 0, 8443);
        let mut state = ConfigState::new();
        state
            .dispatch(&RequestType::AddHttpsListener(make_https_listener(addr)).into())
            .unwrap();

        let patch = UpdateHttpsListenerConfig {
            address: addr,
            alpn_protocols: Some(AlpnProtocols {
                values: vec!["h3".to_owned()],
            }),
            ..Default::default()
        };
        let err = state
            .dispatch(&RequestType::UpdateHttpsListener(patch).into())
            .unwrap_err();
        assert!(
            matches!(
                err,
                StateError::InvalidValue {
                    field: "alpn_protocols",
                    ..
                }
            ),
            "expected InvalidValue for unknown ALPN, got: {err}"
        );
    }

    /// ALPN validation: empty values vec = reset to default, must be accepted.
    #[test]
    fn update_https_listener_alpn_empty_reset_accepted() {
        use crate::proto::command::AlpnProtocols;

        let addr = SocketAddress::new_v4(0, 0, 0, 0, 8443);
        let mut state = ConfigState::new();
        let mut listener = make_https_listener(addr);
        listener.alpn_protocols = vec!["h2".to_owned()];
        state
            .dispatch(&RequestType::AddHttpsListener(listener).into())
            .unwrap();

        let patch = UpdateHttpsListenerConfig {
            address: addr,
            alpn_protocols: Some(AlpnProtocols { values: vec![] }),
            ..Default::default()
        };
        state
            .dispatch(&RequestType::UpdateHttpsListener(patch).into())
            .expect("empty ALPN reset must succeed");

        let listener = state.https_listeners.get(&SocketAddr::from(addr)).unwrap();
        assert!(
            listener.alpn_protocols.is_empty(),
            "ALPN must have been reset to empty"
        );
    }

    /// ALPN validation: valid values ["h2", "http/1.1"] must be accepted.
    #[test]
    fn update_https_listener_alpn_valid_values_accepted() {
        use crate::proto::command::AlpnProtocols;

        let addr = SocketAddress::new_v4(0, 0, 0, 0, 8443);
        let mut state = ConfigState::new();
        state
            .dispatch(&RequestType::AddHttpsListener(make_https_listener(addr)).into())
            .unwrap();

        let patch = UpdateHttpsListenerConfig {
            address: addr,
            alpn_protocols: Some(AlpnProtocols {
                values: vec!["h2".to_owned(), "http/1.1".to_owned()],
            }),
            ..Default::default()
        };
        state
            .dispatch(&RequestType::UpdateHttpsListener(patch).into())
            .expect("valid ALPN must be accepted");

        let listener = state.https_listeners.get(&SocketAddr::from(addr)).unwrap();
        assert_eq!(listener.alpn_protocols, vec!["h2", "http/1.1"]);
    }

    /// ALPN absent wrapper: when `alpn_protocols` is None in the patch, the
    /// listener's ALPN field must not be touched.
    #[test]
    fn update_https_listener_alpn_absent_preserves_existing() {
        let addr = SocketAddress::new_v4(0, 0, 0, 0, 8443);
        let mut state = ConfigState::new();
        let mut listener = make_https_listener(addr);
        listener.alpn_protocols = vec!["h2".to_owned()];
        state
            .dispatch(&RequestType::AddHttpsListener(listener).into())
            .unwrap();

        // patch with no alpn_protocols field
        let patch = UpdateHttpsListenerConfig {
            address: addr,
            front_timeout: Some(10),
            ..Default::default()
        };
        state
            .dispatch(&RequestType::UpdateHttpsListener(patch).into())
            .unwrap();

        let listener = state.https_listeners.get(&SocketAddr::from(addr)).unwrap();
        assert_eq!(
            listener.alpn_protocols,
            vec!["h2"],
            "ALPN must be preserved when not patched"
        );
    }

    /// sozu_id_header validation: empty string must be rejected.
    #[test]
    fn update_https_listener_sozu_id_header_empty_rejected() {
        let addr = SocketAddress::new_v4(0, 0, 0, 0, 8443);
        let mut state = ConfigState::new();
        state
            .dispatch(&RequestType::AddHttpsListener(make_https_listener(addr)).into())
            .unwrap();

        let patch = UpdateHttpsListenerConfig {
            address: addr,
            sozu_id_header: Some(String::new()),
            ..Default::default()
        };
        let err = state
            .dispatch(&RequestType::UpdateHttpsListener(patch).into())
            .unwrap_err();
        assert!(
            matches!(
                err,
                StateError::InvalidValue {
                    field: "sozu_id_header",
                    ..
                }
            ),
            "expected InvalidValue for empty header name, got: {err}"
        );
    }

    /// sozu_id_header validation: value containing colon must be rejected.
    #[test]
    fn update_https_listener_sozu_id_header_colon_rejected() {
        let addr = SocketAddress::new_v4(0, 0, 0, 0, 8443);
        let mut state = ConfigState::new();
        state
            .dispatch(&RequestType::AddHttpsListener(make_https_listener(addr)).into())
            .unwrap();

        let patch = UpdateHttpsListenerConfig {
            address: addr,
            sozu_id_header: Some("bad: value".to_owned()),
            ..Default::default()
        };
        let err = state
            .dispatch(&RequestType::UpdateHttpsListener(patch).into())
            .unwrap_err();
        assert!(
            matches!(
                err,
                StateError::InvalidValue {
                    field: "sozu_id_header",
                    ..
                }
            ),
            "expected InvalidValue for header name with colon, got: {err}"
        );
    }

    /// sozu_id_header validation: well-formed token must be accepted.
    #[test]
    fn update_https_listener_sozu_id_header_valid_accepted() {
        let addr = SocketAddress::new_v4(0, 0, 0, 0, 8443);
        let mut state = ConfigState::new();
        state
            .dispatch(&RequestType::AddHttpsListener(make_https_listener(addr)).into())
            .unwrap();

        let patch = UpdateHttpsListenerConfig {
            address: addr,
            sozu_id_header: Some("X-Edge-Id".to_owned()),
            ..Default::default()
        };
        state
            .dispatch(&RequestType::UpdateHttpsListener(patch).into())
            .expect("valid header name must be accepted");

        let listener = state.https_listeners.get(&SocketAddr::from(addr)).unwrap();
        assert_eq!(listener.sozu_id_header.as_deref(), Some("X-Edge-Id"));
    }

    /// h2_graceful_shutdown_deadline_seconds = 0 must be allowed (means "wait forever").
    #[test]
    fn update_https_listener_graceful_shutdown_zero_allowed() {
        let addr = SocketAddress::new_v4(0, 0, 0, 0, 8443);
        let mut state = ConfigState::new();
        state
            .dispatch(&RequestType::AddHttpsListener(make_https_listener(addr)).into())
            .unwrap();

        let patch = UpdateHttpsListenerConfig {
            address: addr,
            h2_graceful_shutdown_deadline_seconds: Some(0),
            ..Default::default()
        };
        state
            .dispatch(&RequestType::UpdateHttpsListener(patch).into())
            .expect("graceful_shutdown_deadline=0 must be allowed");

        let listener = state.https_listeners.get(&SocketAddr::from(addr)).unwrap();
        assert_eq!(listener.h2_graceful_shutdown_deadline_seconds, Some(0));
    }

    // ── update_http_listener ───────────────────────────────────────────────────

    /// Happy path for HTTP listener patch.
    #[test]
    fn update_http_listener_happy_path() {
        let addr = SocketAddress::new_v4(0, 0, 0, 0, 8080);
        let mut state = ConfigState::new();
        state
            .dispatch(&RequestType::AddHttpListener(make_http_listener(addr)).into())
            .unwrap();

        let patch = UpdateHttpListenerConfig {
            address: addr,
            front_timeout: Some(15),
            h2_max_rst_stream_per_window: Some(25),
            ..Default::default()
        };
        state
            .dispatch(&RequestType::UpdateHttpListener(patch).into())
            .expect("HTTP update must succeed");

        let listener = state.http_listeners.get(&SocketAddr::from(addr)).unwrap();
        assert_eq!(listener.front_timeout, 15);
        assert_eq!(listener.h2_max_rst_stream_per_window, Some(25));
        // untouched
        assert_eq!(listener.back_timeout, 30);
    }

    /// HTTP listener: flood knob 0 is rejected.
    #[test]
    fn update_http_listener_flood_knob_zero_rejected() {
        let addr = SocketAddress::new_v4(0, 0, 0, 0, 8080);
        let mut state = ConfigState::new();
        state
            .dispatch(&RequestType::AddHttpListener(make_http_listener(addr)).into())
            .unwrap();

        let patch = UpdateHttpListenerConfig {
            address: addr,
            h2_max_window_update_stream0_per_window: Some(0),
            ..Default::default()
        };
        let err = state
            .dispatch(&RequestType::UpdateHttpListener(patch).into())
            .unwrap_err();
        assert!(
            matches!(
                err,
                StateError::InvalidValue {
                    field: "h2_max_window_update_stream0_per_window",
                    ..
                }
            ),
            "expected InvalidValue, got: {err}"
        );
    }

    // ── update_tcp_listener ────────────────────────────────────────────────────

    /// Happy path for TCP listener patch.
    #[test]
    fn update_tcp_listener_happy_path() {
        let addr = SocketAddress::new_v4(0, 0, 0, 0, 9000);
        let mut state = ConfigState::new();
        state
            .dispatch(&RequestType::AddTcpListener(make_tcp_listener(addr)).into())
            .unwrap();

        let patch = UpdateTcpListenerConfig {
            address: addr,
            front_timeout: Some(5),
            ..Default::default()
        };
        state
            .dispatch(&RequestType::UpdateTcpListener(patch).into())
            .expect("TCP update must succeed");

        let listener = state.tcp_listeners.get(&SocketAddr::from(addr)).unwrap();
        assert_eq!(listener.front_timeout, 5);
        assert_eq!(listener.back_timeout, 30); // untouched
    }

    /// TCP listener: NotFound when address is unknown.
    #[test]
    fn update_tcp_listener_not_found() {
        let mut state = ConfigState::new();
        let patch = UpdateTcpListenerConfig {
            address: SocketAddress::new_v4(9, 9, 9, 9, 9999),
            front_timeout: Some(5),
            ..Default::default()
        };
        let err = state
            .dispatch(&RequestType::UpdateTcpListener(patch).into())
            .unwrap_err();
        assert!(
            matches!(
                err,
                StateError::NotFound {
                    kind: ObjectKind::TcpListener,
                    ..
                }
            ),
            "expected NotFound, got: {err}"
        );
    }

    /// `ConfigState::dispatch` MUST treat `SetMetricDetail` as a
    /// runtime-only verb (no persisted state mutation). A future
    /// refactor that drops the variant from the no-op match arm and
    /// falls through to the catch-all would silently re-break the
    /// SetMetricDetail dispatch path with `UndispatchableRequest`.
    #[test]
    fn dispatch_passes_through_set_metric_detail() {
        use crate::proto::command::{MetricDetail, SetMetricDetail};
        let mut state = ConfigState::new();
        let req: Request = RequestType::SetMetricDetail(SetMetricDetail {
            client_id: "test:1".to_owned(),
            detail: Some(MetricDetail::DetailBackend as i32),
            ttl_seconds: Some(60),
            clear: Some(false),
            reason: Some("regression-guard".to_owned()),
            peer_pid: None,
            peer_session_ulid: None,
        })
        .into();
        state
            .dispatch(&req)
            .expect("SetMetricDetail must traverse dispatch without UndispatchableRequest");
    }
}
