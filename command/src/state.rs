use std::{
    collections::{
        btree_map::Entry as BTreeMapEntry, hash_map::DefaultHasher, BTreeMap, BTreeSet, HashMap,
        HashSet,
    },
    hash::{Hash, Hasher},
    iter::{repeat, FromIterator},
    net::SocketAddr,
};

use prost::DecodeError;

use crate::{
    certificate::{self, calculate_fingerprint, Fingerprint},
    proto::{
        command::{
            request::RequestType, ActivateListener, AddBackend, AddCertificate, CertificateAndKey,
            Cluster, ClusterInformation, DeactivateListener, FrontendFilters, HttpListenerConfig,
            HttpsListenerConfig, ListedFrontends, ListenerType, ListenersList, PathRule,
            QueryCertificatesFilters, RemoveBackend, RemoveCertificate, RemoveListener,
            ReplaceCertificate, Request, RequestCounts, RequestHttpFrontend, RequestTcpFrontend,
            TcpListenerConfig,
        },
        display::format_request_type,
    },
    response::{Backend, HttpFrontend, TcpFrontend},
    ObjectKind,
};

/// To use throughout Sōzu
pub type ClusterId = String;

#[derive(thiserror::Error, Debug)]
pub enum StateError {
    #[error("Request came in empty")]
    EmptyRequest,
    #[error("dispatching this request did not bring any change to the state")]
    NoChange,
    #[error("State can not handle this request")]
    UndispatchableRequest,
    #[error("Did not find {kind:?} with address or id '{id}'")]
    NotFound { kind: ObjectKind, id: String },
    #[error("{kind:?} '{id}' already exists")]
    Exists { kind: ObjectKind, id: String },
    #[error("Wrong request: {0}")]
    WrongRequest(String),
    #[error("Could not add certificate: {0}")]
    AddCertificate(String),
    #[error("Could not remove certificate: {0}")]
    RemoveCertificate(String),
    #[error("Could not replace certificate: {0}")]
    ReplaceCertificate(String),
    #[error("The provided socket address '{address}' is wrong: {error}")]
    WrongSocketAddress { address: String, error: String },
    #[error(
        "Could not convert the frontend to an insertable one. Frontend: {frontend} error: {error}"
    )]
    FrontendConversion { frontend: String, error: String },
    #[error("Found no matching cluster for the id {0}")]
    HasNoMatchingCluster(ClusterId),
}

impl From<DecodeError> for StateError {
    fn from(decode_error: DecodeError) -> Self {
        Self::WrongRequest(format!("Wrong field value: {decode_error}"))
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
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigState {
    pub clusters: BTreeMap<ClusterId, Cluster>,
    pub backends: BTreeMap<ClusterId, Vec<Backend>>,
    /// socket address -> HTTP listener
    pub http_listeners: BTreeMap<String, HttpListenerConfig>,
    /// socket address -> HTTPS listener
    pub https_listeners: BTreeMap<String, HttpsListenerConfig>,
    /// socket address -> TCP listener
    pub tcp_listeners: BTreeMap<String, TcpListenerConfig>,
    /// HTTP frontends, indexed by a summary of each front's address;hostname;path, for uniqueness.
    /// For example: `"0.0.0.0:8080;lolcatho.st;P/api"`
    pub http_fronts: BTreeMap<String, HttpFrontend>,
    /// indexed by (address, hostname, path)
    pub https_fronts: BTreeMap<String, HttpFrontend>,
    pub tcp_fronts: HashMap<ClusterId, Vec<TcpFrontend>>,
    pub certificates: HashMap<SocketAddr, HashMap<Fingerprint, CertificateAndKey>>,
    /// A census of requests that were received. Name of the request -> number of occurences
    pub request_counts: BTreeMap<String, i32>,
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

        match request_type {
            RequestType::AddCluster(cluster) => self.add_cluster(cluster),
            RequestType::RemoveCluster(cluster_id) => self.remove_cluster(cluster_id),
            RequestType::AddHttpListener(listener) => self.add_http_listener(listener),
            RequestType::AddHttpsListener(listener) => self.add_https_listener(listener),
            RequestType::AddTcpListener(listener) => self.add_tcp_listener(listener),
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
            RequestType::AddBackend(add_backend) => self.add_backend(add_backend),
            RequestType::RemoveBackend(backend) => self.remove_backend(backend),

            // This is to avoid the error message
            &RequestType::Logging(_)
            | &RequestType::CountRequests(_)
            | &RequestType::Status(_)
            | &RequestType::SoftStop(_)
            | &RequestType::QueryCertificatesFromWorkers(_)
            | &RequestType::QueryClusterById(_)
            | &RequestType::QueryClustersByDomain(_)
            | &RequestType::QueryMetrics(_)
            | &RequestType::QueryClustersHashes(_)
            | &RequestType::ConfigureMetrics(_)
            | &RequestType::ReturnListenSockets(_)
            | &RequestType::HardStop(_) => Ok(()),

            _other_request => Err(StateError::UndispatchableRequest),
        }
    }

    /// Increments the count for this request type
    fn increment_request_count(&mut self, request: &Request) {
        if let Some(request_type) = &request.request_type {
            let count = self
                .request_counts
                .entry(format_request_type(request_type))
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
        let cluster = cluster.clone();
        self.clusters.insert(cluster.cluster_id.clone(), cluster);
        Ok(())
    }

    fn remove_cluster(&mut self, cluster_id: &str) -> Result<(), StateError> {
        match self.clusters.remove(cluster_id) {
            Some(_) => Ok(()),
            None => Err(StateError::NotFound {
                kind: ObjectKind::Cluster,
                id: cluster_id.to_owned(),
            }),
        }
    }

    fn add_http_listener(&mut self, listener: &HttpListenerConfig) -> Result<(), StateError> {
        let address = listener.address.to_string();
        match self.http_listeners.entry(address.clone()) {
            BTreeMapEntry::Vacant(vacant_entry) => vacant_entry.insert(listener.clone()),
            BTreeMapEntry::Occupied(_) => {
                return Err(StateError::Exists {
                    kind: ObjectKind::HttpListener,
                    id: address,
                })
            }
        };
        Ok(())
    }

    fn add_https_listener(&mut self, listener: &HttpsListenerConfig) -> Result<(), StateError> {
        let address = listener.address.to_string();
        match self.https_listeners.entry(address.clone()) {
            BTreeMapEntry::Vacant(vacant_entry) => vacant_entry.insert(listener.clone()),
            BTreeMapEntry::Occupied(_) => {
                return Err(StateError::Exists {
                    kind: ObjectKind::HttpsListener,
                    id: address,
                })
            }
        };
        Ok(())
    }

    fn add_tcp_listener(&mut self, listener: &TcpListenerConfig) -> Result<(), StateError> {
        let address = listener.address.to_string();
        match self.tcp_listeners.entry(address.clone()) {
            BTreeMapEntry::Vacant(vacant_entry) => vacant_entry.insert(listener.clone()),
            BTreeMapEntry::Occupied(_) => {
                return Err(StateError::Exists {
                    kind: ObjectKind::TcpListener,
                    id: address,
                })
            }
        };
        Ok(())
    }

    fn remove_listener(&mut self, remove: &RemoveListener) -> Result<(), StateError> {
        match ListenerType::try_from(remove.proxy)? {
            ListenerType::Http => self.remove_http_listener(&remove.address),
            ListenerType::Https => self.remove_https_listener(&remove.address),
            ListenerType::Tcp => self.remove_tcp_listener(&remove.address),
        }
    }

    fn remove_http_listener(&mut self, address: &str) -> Result<(), StateError> {
        if self.http_listeners.remove(address).is_none() {
            return Err(StateError::NoChange);
        }
        Ok(())
    }

    fn remove_https_listener(&mut self, address: &str) -> Result<(), StateError> {
        if self.https_listeners.remove(address).is_none() {
            return Err(StateError::NoChange);
        }
        Ok(())
    }

    fn remove_tcp_listener(&mut self, address: &str) -> Result<(), StateError> {
        if self.tcp_listeners.remove(address).is_none() {
            return Err(StateError::NoChange);
        }
        Ok(())
    }

    fn activate_listener(&mut self, activate: &ActivateListener) -> Result<(), StateError> {
        match ListenerType::try_from(activate.proxy)? {
            ListenerType::Http => self
                .http_listeners
                .get_mut(&activate.address)
                .map(|listener| listener.active = true)
                .ok_or(StateError::NotFound {
                    kind: ObjectKind::HttpListener,
                    id: activate.address.to_owned(),
                }),
            ListenerType::Https => self
                .https_listeners
                .get_mut(&activate.address)
                .map(|listener| listener.active = true)
                .ok_or(StateError::NotFound {
                    kind: ObjectKind::HttpsListener,
                    id: activate.address.to_owned(),
                }),
            ListenerType::Tcp => self
                .tcp_listeners
                .get_mut(&activate.address)
                .map(|listener| listener.active = true)
                .ok_or(StateError::NotFound {
                    kind: ObjectKind::TcpListener,
                    id: activate.address.to_owned(),
                }),
        }
    }

    fn deactivate_listener(&mut self, deactivate: &DeactivateListener) -> Result<(), StateError> {
        match ListenerType::try_from(deactivate.proxy)? {
            ListenerType::Http => self
                .http_listeners
                .get_mut(&deactivate.address)
                .map(|listener| listener.active = false)
                .ok_or(StateError::NotFound {
                    kind: ObjectKind::HttpListener,
                    id: deactivate.address.to_owned(),
                }),
            ListenerType::Https => self
                .https_listeners
                .get_mut(&deactivate.address)
                .map(|listener| listener.active = false)
                .ok_or(StateError::NotFound {
                    kind: ObjectKind::HttpsListener,
                    id: deactivate.address.to_owned(),
                }),
            ListenerType::Tcp => self
                .tcp_listeners
                .get_mut(&deactivate.address)
                .map(|listener| listener.active = false)
                .ok_or(StateError::NotFound {
                    kind: ObjectKind::TcpListener,
                    id: deactivate.address.to_owned(),
                }),
        }
    }

    fn check_for_cluster_presence(&self, cluster_id: &ClusterId) -> Result<(), StateError> {
        if !self.clusters.contains_key(cluster_id) {
            return Err(StateError::HasNoMatchingCluster(cluster_id.clone()));
        }
        Ok(())
    }

    fn add_http_frontend(&mut self, front: &RequestHttpFrontend) -> Result<(), StateError> {
        self.check_for_cluster_presence(&front.cluster_id)?;

        let front_as_key = front.to_string();

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
                return Err(StateError::Exists {
                    kind: ObjectKind::HttpFrontend,
                    id: front.to_string(),
                })
            }
        };
        Ok(())
    }

    fn add_https_frontend(&mut self, front: &RequestHttpFrontend) -> Result<(), StateError> {
        self.check_for_cluster_presence(&front.cluster_id)?;

        let front_as_key = front.to_string();

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
                return Err(StateError::Exists {
                    kind: ObjectKind::HttpsFrontend,
                    id: front.to_string(),
                })
            }
        };
        Ok(())
    }

    fn remove_http_frontend(&mut self, front: &RequestHttpFrontend) -> Result<(), StateError> {
        self.http_fronts
            .remove(&front.to_string())
            .ok_or(StateError::NotFound {
                kind: ObjectKind::HttpFrontend,
                id: front.to_string(),
            })?;
        Ok(())
    }

    fn remove_https_frontend(&mut self, front: &RequestHttpFrontend) -> Result<(), StateError> {
        self.https_fronts
            .remove(&front.to_string())
            .ok_or(StateError::NotFound {
                kind: ObjectKind::HttpsFrontend,
                id: front.to_string(),
            })?;
        Ok(())
    }

    fn add_certificate(&mut self, add: &AddCertificate) -> Result<(), StateError> {
        let fingerprint = Fingerprint(
            calculate_fingerprint(add.certificate.certificate.as_bytes()).map_err(
                |fingerprint_err| StateError::AddCertificate(fingerprint_err.to_string()),
            )?,
        );

        let address = parse_socket_address(&add.address)?;

        let entry = self
            .certificates
            .entry(address)
            .or_insert_with(HashMap::new);

        if entry.contains_key(&fingerprint) {
            info!(
                "Skip loading of certificate '{}' for domain '{}' on listener '{}', the certificate is already present.",
                fingerprint, add.certificate.names.join(", "), add.address
            );
            return Ok(());
        }

        entry.insert(fingerprint, add.certificate.clone());
        Ok(())
    }

    fn remove_certificate(&mut self, remove: &RemoveCertificate) -> Result<(), StateError> {
        let fingerprint = Fingerprint(
            hex::decode(&remove.fingerprint)
                .map_err(|decode_error| StateError::RemoveCertificate(decode_error.to_string()))?,
        );

        let address = parse_socket_address(&remove.address)?;

        if let Some(index) = self.certificates.get_mut(&address) {
            index.remove(&fingerprint);
        }

        Ok(())
    }

    /// - Remove old certificate from certificates, using the old fingerprint
    /// - calculate the new fingerprint
    /// - insert the new certificate with the new fingerprint as key
    /// - check that the new entry is present in the certificates hashmap
    fn replace_certificate(&mut self, replace: &ReplaceCertificate) -> Result<(), StateError> {
        let address = parse_socket_address(&replace.address)?;

        let old_fingerprint = Fingerprint(
            hex::decode(&replace.old_fingerprint)
                .map_err(|decode_error| StateError::RemoveCertificate(decode_error.to_string()))?,
        );

        self.certificates
            .get_mut(&address)
            .ok_or(StateError::NotFound {
                kind: ObjectKind::Certificate,
                id: address.to_string(),
            })?
            .remove(&old_fingerprint);

        let new_fingerprint = Fingerprint(
            calculate_fingerprint(replace.new_certificate.certificate.as_bytes()).map_err(
                |fingerprint_err| StateError::ReplaceCertificate(fingerprint_err.to_string()),
            )?,
        );

        self.certificates
            .get_mut(&address)
            .map(|certs| certs.insert(new_fingerprint.clone(), replace.new_certificate.clone()));

        if !self
            .certificates
            .get(&address)
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
        Ok(())
    }

    fn add_tcp_frontend(&mut self, front: &RequestTcpFrontend) -> Result<(), StateError> {
        self.check_for_cluster_presence(&front.cluster_id)?;

        let tcp_frontends = self
            .tcp_fronts
            .entry(front.cluster_id.clone())
            .or_insert_with(Vec::new);

        let tcp_frontend = TcpFrontend {
            cluster_id: front.cluster_id.clone(),
            address: parse_socket_address(&front.address)?,
            tags: front.tags.clone(),
        };
        if tcp_frontends.contains(&tcp_frontend) {
            return Err(StateError::Exists {
                kind: ObjectKind::TcpFrontend,
                id: format!("{:?}", tcp_frontend),
            });
        }

        tcp_frontends.push(tcp_frontend);
        Ok(())
    }

    fn remove_tcp_frontend(
        &mut self,
        front_to_remove: &RequestTcpFrontend,
    ) -> Result<(), StateError> {
        let address = parse_socket_address(&front_to_remove.address)?;

        let tcp_frontends =
            self.tcp_fronts
                .get_mut(&front_to_remove.cluster_id)
                .ok_or(StateError::NotFound {
                    kind: ObjectKind::TcpFrontend,
                    id: format!("{:?}", front_to_remove),
                })?;

        let len = tcp_frontends.len();
        tcp_frontends.retain(|front| front.address != address);
        if tcp_frontends.len() == len {
            return Err(StateError::NoChange);
        }
        Ok(())
    }

    fn add_backend(&mut self, add_backend: &AddBackend) -> Result<(), StateError> {
        let backend = Backend {
            address: parse_socket_address(&add_backend.address)?,
            cluster_id: add_backend.cluster_id.clone(),
            backend_id: add_backend.backend_id.clone(),
            sticky_id: add_backend.sticky_id.clone(),
            load_balancing_parameters: add_backend.load_balancing_parameters.clone(),
            backup: add_backend.backup,
        };
        let backends = self
            .backends
            .entry(backend.cluster_id.clone())
            .or_insert_with(Vec::new);

        // we might be modifying the sticky id or load balancing parameters
        backends.retain(|b| b.backend_id != backend.backend_id || b.address != backend.address);
        backends.push(backend);
        backends.sort();

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
        backend_list.retain(|b| {
            b.backend_id != backend.backend_id || b.address.to_string() != backend.address
        });
        backend_list.sort();
        if backend_list.len() == len {
            return Err(StateError::NoChange);
        }
        Ok(())
    }

    pub fn generate_requests(&self) -> Vec<Request> {
        let mut v: Vec<Request> = Vec::new();

        for listener in self.http_listeners.values() {
            v.push(RequestType::AddHttpListener(listener.clone()).into());
            if listener.active {
                v.push(
                    RequestType::ActivateListener(ActivateListener {
                        address: listener.address.clone(),
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
                        address: listener.address.clone(),
                        proxy: ListenerType::Https.into(),
                        from_scm: false,
                    })
                    .into(),
                );
            }
        }

        for listener in self.tcp_listeners.values() {
            v.push(RequestType::AddTcpListener(listener.clone()).into());
            if listener.active {
                v.push(
                    RequestType::ActivateListener(ActivateListener {
                        address: listener.address.clone(),
                        proxy: ListenerType::Tcp.into(),
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
                        address: front.to_string(),
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

        for backend_list in self.backends.values() {
            for backend in backend_list {
                v.push(RequestType::AddBackend(backend.clone().to_add_backend()).into());
            }
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
                    address: front.to_string(),
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
                    address: front.to_string(),
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
                    address: front.to_string(),
                    proxy: ListenerType::Tcp.into(),
                    from_scm: false,
                })
                .into(),
            );
        }

        v
    }

    pub fn diff(&self, other: &ConfigState) -> Vec<Request> {
        //pub tcp_listeners:   HashMap<SocketAddr, (TcpListener, bool)>,
        let my_tcp_listeners: HashSet<&String> = self.tcp_listeners.keys().collect();
        let their_tcp_listeners: HashSet<&String> = other.tcp_listeners.keys().collect();
        let removed_tcp_listeners = my_tcp_listeners.difference(&their_tcp_listeners);
        let added_tcp_listeners = their_tcp_listeners.difference(&my_tcp_listeners);

        let my_http_listeners: HashSet<&String> = self.http_listeners.keys().collect();
        let their_http_listeners: HashSet<&String> = other.http_listeners.keys().collect();
        let removed_http_listeners = my_http_listeners.difference(&their_http_listeners);
        let added_http_listeners = their_http_listeners.difference(&my_http_listeners);

        let my_https_listeners: HashSet<&String> = self.https_listeners.keys().collect();
        let their_https_listeners: HashSet<&String> = other.https_listeners.keys().collect();
        let removed_https_listeners = my_https_listeners.difference(&their_https_listeners);
        let added_https_listeners = their_https_listeners.difference(&my_https_listeners);

        let mut v: Vec<Request> = vec![];

        for address in removed_tcp_listeners {
            if self.tcp_listeners[*address].active {
                v.push(
                    RequestType::DeactivateListener(DeactivateListener {
                        address: address.to_string(),
                        proxy: ListenerType::Tcp.into(),
                        to_scm: false,
                    })
                    .into(),
                );
            }

            v.push(
                RequestType::RemoveListener(RemoveListener {
                    address: address.to_string(),
                    proxy: ListenerType::Tcp.into(),
                })
                .into(),
            );
        }

        for address in added_tcp_listeners.clone() {
            v.push(RequestType::AddTcpListener(other.tcp_listeners[*address].clone()).into());

            if other.tcp_listeners[*address].active {
                v.push(
                    RequestType::ActivateListener(ActivateListener {
                        address: address.to_string(),
                        proxy: ListenerType::Tcp.into(),
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
                        address: address.to_string(),
                        proxy: ListenerType::Http.into(),
                        to_scm: false,
                    })
                    .into(),
                );
            }

            v.push(
                RequestType::RemoveListener(RemoveListener {
                    address: address.to_string(),
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
                        address: address.to_string(),
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
                        address: address.to_string(),
                        proxy: ListenerType::Https.into(),
                        to_scm: false,
                    })
                    .into(),
                );
            }

            v.push(
                RequestType::RemoveListener(RemoveListener {
                    address: address.to_string(),
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
                        address: address.to_string(),
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
                        address: addr.to_string(),
                        proxy: ListenerType::Tcp.into(),
                    })
                    .into(),
                );
                // any added listener should be unactive
                let mut listener_to_add = their_listener.clone();
                listener_to_add.active = false;
                v.push(RequestType::AddTcpListener(listener_to_add).into());
            }

            if my_listener.active && !their_listener.active {
                v.push(
                    RequestType::DeactivateListener(DeactivateListener {
                        address: addr.to_string(),
                        proxy: ListenerType::Tcp.into(),
                        to_scm: false,
                    })
                    .into(),
                );
            }

            if !my_listener.active && their_listener.active {
                v.push(
                    RequestType::ActivateListener(ActivateListener {
                        address: addr.to_string(),
                        proxy: ListenerType::Tcp.into(),
                        from_scm: false,
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
                        address: addr.to_string(),
                        proxy: ListenerType::Http.into(),
                    })
                    .into(),
                );
                // any added listener should be unactive
                let mut listener_to_add = their_listener.clone();
                listener_to_add.active = false;
                v.push(RequestType::AddHttpListener(listener_to_add).into());
            }

            if my_listener.active && !their_listener.active {
                v.push(
                    RequestType::DeactivateListener(DeactivateListener {
                        address: addr.to_string(),
                        proxy: ListenerType::Http.into(),
                        to_scm: false,
                    })
                    .into(),
                );
            }

            if !my_listener.active && their_listener.active {
                v.push(
                    RequestType::ActivateListener(ActivateListener {
                        address: addr.to_string(),
                        proxy: ListenerType::Http.into(),
                        from_scm: false,
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
                        address: addr.to_string(),
                        proxy: ListenerType::Https.into(),
                    })
                    .into(),
                );
                // any added listener should be unactive
                let mut listener_to_add = their_listener.clone();
                listener_to_add.active = false;
                v.push(RequestType::AddHttpsListener(listener_to_add).into());
            }

            if my_listener.active && !their_listener.active {
                v.push(
                    RequestType::DeactivateListener(DeactivateListener {
                        address: addr.to_string(),
                        proxy: ListenerType::Https.into(),
                        to_scm: false,
                    })
                    .into(),
                );
            }

            if !my_listener.active && their_listener.active {
                v.push(
                    RequestType::ActivateListener(ActivateListener {
                        address: addr.to_string(),
                        proxy: ListenerType::Https.into(),
                        from_scm: false,
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
                            address: backend.address.to_string(),
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
                            address: backend.address.to_string(),
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
                    address: address.to_string(),
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
                        address: address.to_string(),
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
                        address: listener.address.clone(),
                        proxy: ListenerType::Tcp.into(),
                        from_scm: false,
                    })
                    .into(),
                );
            }
        }

        v
    }

    // FIXME: what about deny rules?
    pub fn hash_state(&self) -> BTreeMap<ClusterId, u64> {
        let mut h: HashMap<_, _> = self
            .clusters
            .keys()
            .map(|cluster_id| {
                let mut s = DefaultHasher::new();
                self.clusters.get(cluster_id).hash(&mut s);
                if let Some(v) = self.backends.get(cluster_id) {
                    v.iter().collect::<BTreeSet<_>>().hash(&mut s)
                }
                if let Some(v) = self.tcp_fronts.get(cluster_id) {
                    v.iter().collect::<BTreeSet<_>>().hash(&mut s)
                }
                (cluster_id.to_string(), s)
            })
            .collect();

        for front in self.http_fronts.values() {
            if let Some(s) = h.get_mut(&front.cluster_id) {
                front.hash(s);
            }
        }

        for front in self.https_fronts.values() {
            if let Some(s) = h.get_mut(&front.cluster_id) {
                front.hash(s);
            }
        }

        h.drain()
            .map(|(cluster_id, hasher)| (cluster_id, hasher.finish()))
            .collect()
    }

    /// Gives details about a given cluster.
    /// Types like `HttpFrontend` are converted into protobuf ones, like `RequestHttpFrontend`
    pub fn cluster_state(&self, cluster_id: &str) -> ClusterInformation {
        let mut http_frontends = Vec::new();
        let mut https_frontends = Vec::new();
        let mut tcp_frontends = Vec::new();
        let mut backends = Vec::new();

        for http_frontend in self.http_fronts.values() {
            if http_frontend.cluster_id == cluster_id {
                http_frontends.push(http_frontend.clone().into());
            }
        }

        for https_frontend in self.https_fronts.values() {
            if https_frontend.cluster_id == cluster_id {
                https_frontends.push(https_frontend.clone().into());
            }
        }

        for tcp_f in self.tcp_fronts.get(cluster_id).cloned().unwrap_or_default() {
            tcp_frontends.push(tcp_f.clone().into());
        }

        for backend in self.backends.get(cluster_id).cloned().unwrap_or_default() {
            backends.push(backend.clone().into())
        }

        ClusterInformation {
            configuration: self.clusters.get(cluster_id).cloned(),
            http_frontends,
            https_frontends,
            tcp_frontends,
            backends,
        }
    }

    pub fn count_backends(&self) -> usize {
        self.backends.values().fold(0, |acc, v| acc + v.len())
    }

    pub fn count_frontends(&self) -> usize {
        self.http_fronts.values().count()
            + self.https_fronts.values().count()
            + self.tcp_fronts.values().fold(0, |acc, v| acc + v.len())
    }

    pub fn get_cluster_ids_by_domain(
        &self,
        hostname: String,
        path: Option<String>,
    ) -> HashSet<ClusterId> {
        let mut cluster_ids: HashSet<ClusterId> = HashSet::new();

        self.http_fronts.values().for_each(|front| {
            if domain_check(&front.hostname, &front.path, &hostname, &path) {
                cluster_ids.insert(front.cluster_id.clone());
            }
        });

        self.https_fronts.values().for_each(|front| {
            if domain_check(&front.hostname, &front.path, &hostname, &path) {
                cluster_ids.insert(front.cluster_id.clone());
            }
        });

        cluster_ids
    }

    pub fn get_certificates(
        &self,
        filters: QueryCertificatesFilters,
    ) -> BTreeMap<String, CertificateAndKey> {
        if let Some(domain) = filters.domain {
            self.certificates
                .values()
                .flat_map(|hash_map| hash_map.iter())
                .flat_map(|(fingerprint, cert)| {
                    if cert.names.is_empty() {
                        let pem = certificate::parse(cert.certificate.as_bytes()).ok()?;
                        let mut c = cert.to_owned();

                        c.names = certificate::get_cn_and_san_attributes(&pem)
                            .ok()?
                            .into_iter()
                            .collect();

                        return Some((fingerprint, c));
                    }

                    Some((fingerprint, cert.to_owned()))
                })
                .filter(|(_, cert)| cert.names.contains(&domain))
                .map(|(fingerprint, cert)| (fingerprint.to_string(), cert))
                .collect()
        } else if let Some(f) = filters.fingerprint {
            self.certificates
                .values()
                .flat_map(|hash_map| hash_map.iter())
                .filter(|(fingerprint, _cert)| fingerprint.to_string() == f)
                .flat_map(|(fingerprint, cert)| {
                    if cert.names.is_empty() {
                        let pem = certificate::parse(cert.certificate.as_bytes()).ok()?;
                        let mut c = cert.to_owned();

                        c.names = certificate::get_cn_and_san_attributes(&pem)
                            .ok()?
                            .into_iter()
                            .collect();

                        return Some((fingerprint, c));
                    }

                    Some((fingerprint, cert.to_owned()))
                })
                .map(|(fingerprint, cert)| (fingerprint.to_string(), cert))
                .collect()
        } else {
            self.certificates
                .values()
                .flat_map(|hash_map| hash_map.iter())
                .flat_map(|(fingerprint, cert)| {
                    if cert.names.is_empty() {
                        let pem = certificate::parse(cert.certificate.as_bytes()).ok()?;
                        let mut c = cert.to_owned();

                        c.names = certificate::get_cn_and_san_attributes(&pem)
                            .ok()?
                            .into_iter()
                            .collect();

                        return Some((fingerprint, c));
                    }

                    Some((fingerprint, cert.to_owned()))
                })
                .map(|(fingerprint, cert)| (fingerprint.to_string(), cert))
                .collect()
        }
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

        listed_frontends
    }

    pub fn list_listeners(&self) -> ListenersList {
        ListenersList {
            http_listeners: self.http_listeners.clone(),
            https_listeners: self.https_listeners.clone(),
            tcp_listeners: self.tcp_listeners.clone(),
        }
    }
}

fn parse_socket_address(address: &str) -> Result<SocketAddr, StateError> {
    address
        .parse::<SocketAddr>()
        .map_err(|parse_error| StateError::WrongSocketAddress {
            address: address.to_owned(),
            error: parse_error.to_string(),
        })
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

    if let Some(ref path) = &path_prefix {
        return path == &front_path_rule.value;
    }

    true
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
impl<
        'a,
        K: Ord,
        V: PartialEq,
        I1: Iterator<Item = (K, &'a V)>,
        I2: Iterator<Item = (K, &'a V)>,
    > std::iter::Iterator for DiffMap<'a, K, V, I1, I2>
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
    use super::*;
    use crate::proto::command::{LoadBalancingParams, RequestHttpFrontend, RulePosition};

    #[test]
    fn serialize() {
        let mut state: ConfigState = Default::default();

        state
            .dispatch(
                &RequestType::AddCluster(Cluster {
                    cluster_id: "cluster_1".to_string(),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state
            .dispatch(
                &RequestType::AddCluster(Cluster {
                    cluster_id: "cluster_2".to_string(),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state
            .dispatch(
                &RequestType::AddHttpFrontend(RequestHttpFrontend {
                    cluster_id: String::from("cluster_1"),
                    hostname: String::from("lolcatho.st:8080"),
                    path: PathRule::prefix(String::from("/")),
                    address: "0.0.0.0:8080".to_string(),
                    position: RulePosition::Tree.into(),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state
            .dispatch(
                &RequestType::AddHttpFrontend(RequestHttpFrontend {
                    cluster_id: String::from("cluster_2"),
                    hostname: String::from("test.local"),
                    path: PathRule::prefix(String::from("/abc")),
                    address: "0.0.0.0:8080".to_string(),
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
                    address: "127.0.0.1:1026".parse().unwrap(),
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
                    address: "127.0.0.2:1027".parse().unwrap(),
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
                    address: "192.167.1.2:1026".parse().unwrap(),
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
                    address: "192.168.1.3:1027".parse().unwrap(),
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
                    address: "192.168.1.3:1027".parse().unwrap(),
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
                &RequestType::AddCluster(Cluster {
                    cluster_id: String::from("cluster_1"),
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

        state
            .dispatch(
                &RequestType::AddHttpFrontend(RequestHttpFrontend {
                    cluster_id: String::from("cluster_1"),
                    hostname: String::from("lolcatho.st:8080"),
                    path: PathRule::prefix(String::from("/")),
                    address: "0.0.0.0:8080".to_string(),
                    position: RulePosition::Post.into(),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state
            .dispatch(
                &RequestType::AddHttpFrontend(RequestHttpFrontend {
                    cluster_id: String::from("cluster_2"),
                    hostname: String::from("test.local"),
                    path: PathRule::prefix(String::from("/abc")),
                    address: "0.0.0.0:8080".to_string(),
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
                    address: "127.0.0.1:1026".parse().unwrap(),
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
                    address: "127.0.0.2:1027".parse().unwrap(),
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
                    address: "192.167.1.2:1026".parse().unwrap(),
                    load_balancing_parameters: Some(LoadBalancingParams::default()),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");

        let mut state2: ConfigState = Default::default();
        state2
            .dispatch(
                &RequestType::AddCluster(Cluster {
                    cluster_id: String::from("cluster_1"),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state2
            .dispatch(
                &RequestType::AddHttpFrontend(RequestHttpFrontend {
                    cluster_id: String::from("cluster_1"),
                    hostname: String::from("lolcatho.st:8080"),
                    path: PathRule::prefix(String::from("/")),
                    address: "0.0.0.0:8080".to_string(),
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
                    address: "127.0.0.1:1026".parse().unwrap(),
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
                    address: "127.0.0.2:1027".parse().unwrap(),
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
                    address: "127.0.0.2:1028".parse().unwrap(),
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
                cluster_id: String::from("cluster_2"),
                hostname: String::from("test.local"),
                path: PathRule::prefix(String::from("/abc")),
                address: "0.0.0.0:8080".to_string(),
                ..Default::default()
            })
            .into(),
            RequestType::RemoveBackend(RemoveBackend {
                cluster_id: String::from("cluster_2"),
                backend_id: String::from("cluster_2-0"),
                address: "192.167.1.2:1026".to_string(),
            })
            .into(),
            RequestType::AddBackend(AddBackend {
                cluster_id: String::from("cluster_1"),
                backend_id: String::from("cluster_1-2"),
                address: "127.0.0.2:1028".to_string(),
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
                    address: "127.0.0.2:1028".to_string(),
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

        let mut cluster_1 = Cluster::default();
        cluster_1.cluster_id = String::from("MyCluster_1");
        let mut cluster_2 = Cluster::default();
        cluster_2.cluster_id = String::from("MyCluster_2");

        config
            .dispatch(&RequestType::AddCluster(cluster_1).into())
            .expect("Could not execute request");

        config
            .dispatch(&RequestType::AddCluster(cluster_2).into())
            .expect("Could not execute request");

        let http_front_cluster1 = RequestHttpFrontend {
            cluster_id: String::from("MyCluster_1"),
            hostname: String::from("lolcatho.st"),
            path: PathRule::prefix(String::from("")),
            address: "0.0.0.0:8080".to_string(),
            ..Default::default()
        };

        let https_front_cluster1 = RequestHttpFrontend {
            cluster_id: String::from("MyCluster_1"),
            hostname: String::from("lolcatho.st"),
            path: PathRule::prefix(String::from("")),
            address: "0.0.0.0:8443".to_string(),
            ..Default::default()
        };

        let http_front_cluster2 = RequestHttpFrontend {
            cluster_id: String::from("MyCluster_2"),
            hostname: String::from("lolcatho.st"),
            path: PathRule::prefix(String::from("/api")),
            address: "0.0.0.0:8080".to_string(),
            ..Default::default()
        };

        let https_front_cluster2 = RequestHttpFrontend {
            cluster_id: String::from("MyCluster_2"),
            hostname: String::from("lolcatho.st"),
            path: PathRule::prefix(String::from("/api")),
            address: "0.0.0.0:8443".to_string(),
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
                    address: "127.0.0.1:1026".to_string(),
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
    fn listener_diff() {
        let mut state: ConfigState = Default::default();
        state
            .dispatch(
                &RequestType::AddTcpListener(TcpListenerConfig {
                    address: "0.0.0.0:1234".parse().unwrap(),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state
            .dispatch(
                &RequestType::ActivateListener(ActivateListener {
                    address: "0.0.0.0:1234".parse().unwrap(),
                    proxy: ListenerType::Tcp.into(),
                    from_scm: false,
                })
                .into(),
            )
            .expect("Could not execute request");
        state
            .dispatch(
                &RequestType::AddHttpListener(HttpListenerConfig {
                    address: "0.0.0.0:8080".parse().unwrap(),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state
            .dispatch(
                &RequestType::AddHttpsListener(HttpsListenerConfig {
                    address: "0.0.0.0:8443".parse().unwrap(),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state
            .dispatch(
                &RequestType::ActivateListener(ActivateListener {
                    address: "0.0.0.0:8443".parse().unwrap(),
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
                    address: "0.0.0.0:1234".parse().unwrap(),
                    expect_proxy: true,
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state2
            .dispatch(
                &RequestType::AddHttpListener(HttpListenerConfig {
                    address: "0.0.0.0:8080".parse().unwrap(),
                    answer_404: "test".to_string(),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state2
            .dispatch(
                &RequestType::ActivateListener(ActivateListener {
                    address: "0.0.0.0:8080".parse().unwrap(),
                    proxy: ListenerType::Http.into(),
                    from_scm: false,
                })
                .into(),
            )
            .expect("Could not execute request");
        state2
            .dispatch(
                &RequestType::AddHttpsListener(HttpsListenerConfig {
                    address: "0.0.0.0:8443".parse().unwrap(),
                    answer_404: String::from("test"),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not execute request");
        state2
            .dispatch(
                &RequestType::ActivateListener(ActivateListener {
                    address: "0.0.0.0:8443".parse().unwrap(),
                    proxy: ListenerType::Https.into(),
                    from_scm: false,
                })
                .into(),
            )
            .expect("Could not execute request");

        let e: Vec<Request> = vec![
            RequestType::RemoveListener(RemoveListener {
                address: "0.0.0.0:1234".parse().unwrap(),
                proxy: ListenerType::Tcp.into(),
            })
            .into(),
            RequestType::AddTcpListener(TcpListenerConfig {
                address: "0.0.0.0:1234".parse().unwrap(),
                expect_proxy: true,
                ..Default::default()
            })
            .into(),
            RequestType::DeactivateListener(DeactivateListener {
                address: "0.0.0.0:1234".parse().unwrap(),
                proxy: ListenerType::Tcp.into(),
                to_scm: false,
            })
            .into(),
            RequestType::RemoveListener(RemoveListener {
                address: "0.0.0.0:8080".parse().unwrap(),
                proxy: ListenerType::Http.into(),
            })
            .into(),
            RequestType::AddHttpListener(HttpListenerConfig {
                address: "0.0.0.0:8080".parse().unwrap(),
                answer_404: String::from("test"),
                ..Default::default()
            })
            .into(),
            RequestType::ActivateListener(ActivateListener {
                address: "0.0.0.0:8080".parse().unwrap(),
                proxy: ListenerType::Http.into(),
                from_scm: false,
            })
            .into(),
            RequestType::RemoveListener(RemoveListener {
                address: "0.0.0.0:8443".parse().unwrap(),
                proxy: ListenerType::Https.into(),
            })
            .into(),
            RequestType::AddHttpsListener(HttpsListenerConfig {
                address: "0.0.0.0:8443".parse().unwrap(),
                answer_404: String::from("test"),
                ..Default::default()
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
            address: "127.0.0.1:8080".to_string(),
            certificate: certificate_and_key,
            expired_at: None,
        };
        state
            .dispatch(&RequestType::AddCertificate(add_certificate).into())
            .expect("Could not add certificate");

        println!("state: {:#?}", state);

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

        println!(
            "found certificate: {:#?}",
            certificates_found_by_fingerprint
        );

        assert!(certificates_found_by_fingerprint.len() >= 1);

        let certificate_found_by_domain_name = state.get_certificates(QueryCertificatesFilters {
            domain: Some("lolcatho.st".to_string()),
            fingerprint: None,
        });

        assert!(certificate_found_by_domain_name.len() >= 1);
    }
}
