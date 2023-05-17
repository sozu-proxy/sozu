use std::{
    collections::{
        btree_map::Entry as BTreeMapEntry, hash_map::DefaultHasher, BTreeMap, BTreeSet, HashMap,
        HashSet,
    },
    hash::{Hash, Hasher},
    iter::{repeat, FromIterator},
    net::SocketAddr,
};

use anyhow::{bail, Context};

use crate::{
    certificate::{calculate_fingerprint, Fingerprint},
    proto::command::{
        request::RequestType, ActivateListener, AddBackend, AddCertificate, CertificateAndKey,
        Cluster, ClusterInformation, DeactivateListener, FrontendFilters, HttpListenerConfig,
        HttpsListenerConfig, ListedFrontends, ListenerType, ListenersList, PathRule,
        QueryCertificatesFilters, RemoveBackend, RemoveCertificate, RemoveListener,
        ReplaceCertificate, Request, RequestHttpFrontend, RequestTcpFrontend, TcpListenerConfig,
    },
    response::{Backend, HttpFrontend, TcpFrontend},
};

/// To use throughout Sōzu
pub type ClusterId = String;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigState {
    pub clusters: BTreeMap<ClusterId, Cluster>,
    pub backends: BTreeMap<ClusterId, Vec<Backend>>,
    pub http_listeners: BTreeMap<String, HttpListenerConfig>,
    pub https_listeners: BTreeMap<String, HttpsListenerConfig>,
    pub tcp_listeners: BTreeMap<String, TcpListenerConfig>,
    /// HTTP frontends, indexed by a summary of each front's address;hostname;path, for uniqueness.
    /// For example: `"0.0.0.0:8080;lolcatho.st;P/api"`
    pub http_fronts: BTreeMap<String, HttpFrontend>,
    /// indexed by (address, hostname, path)
    pub https_fronts: BTreeMap<String, HttpFrontend>,
    pub tcp_fronts: HashMap<ClusterId, Vec<TcpFrontend>>,
    pub certificates: HashMap<SocketAddr, HashMap<Fingerprint, CertificateAndKey>>,
}

impl ConfigState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn dispatch(&mut self, request: &Request) -> anyhow::Result<()> {
        let request_type = match &request.request_type {
            Some(t) => t,
            None => bail!("Empty request!"),
        };
        match request_type {
            RequestType::AddCluster(cluster) => self
                .add_cluster(cluster)
                .with_context(|| "Could not add cluster"),
            RequestType::RemoveCluster(cluster_id) => self
                .remove_cluster(cluster_id)
                .with_context(|| "Could not remove cluster"),
            RequestType::AddHttpListener(listener) => self
                .add_http_listener(listener)
                .with_context(|| "Could not add HTTP listener"),
            RequestType::AddHttpsListener(listener) => self
                .add_https_listener(listener)
                .with_context(|| "Could not add HTTPS listener"),
            RequestType::AddTcpListener(listener) => self
                .add_tcp_listener(listener)
                .with_context(|| "Could not add TCP listener"),
            RequestType::RemoveListener(remove) => self
                .remove_listener(remove)
                .with_context(|| "Could not remove listener"),
            RequestType::ActivateListener(activate) => self
                .activate_listener(activate)
                .with_context(|| "Could not activate listener"),
            RequestType::DeactivateListener(deactivate) => self
                .deactivate_listener(deactivate)
                .with_context(|| "Could not deactivate listener"),
            RequestType::AddHttpFrontend(front) => self
                .add_http_frontend(front)
                .with_context(|| "Could not add HTTP frontend"),
            RequestType::RemoveHttpFrontend(front) => self
                .remove_http_frontend(front)
                .with_context(|| "Could not remove HTTP frontend"),
            RequestType::AddCertificate(add) => self
                .add_certificate(add)
                .with_context(|| "Could not add certificate"),
            RequestType::RemoveCertificate(remove) => self
                .remove_certificate(remove)
                .with_context(|| "Could not remove certificate"),
            RequestType::ReplaceCertificate(replace) => self
                .replace_certificate(replace)
                .with_context(|| "Could not replace certificate"),
            RequestType::AddHttpsFrontend(front) => self
                .add_https_frontend(front)
                .with_context(|| "Could not add HTTPS frontend"),
            RequestType::RemoveHttpsFrontend(front) => self
                .remove_https_frontend(front)
                .with_context(|| "Could not remove HTTPS frontend"),
            RequestType::AddTcpFrontend(front) => self
                .add_tcp_frontend(front)
                .with_context(|| "Could not add TCP frontend"),
            RequestType::RemoveTcpFrontend(front) => self
                .remove_tcp_frontend(front)
                .with_context(|| "Could not remove TCP frontend"),
            RequestType::AddBackend(add_backend) => self
                .add_backend(add_backend)
                .with_context(|| "Could not add backend"),
            RequestType::RemoveBackend(backend) => self
                .remove_backend(backend)
                .with_context(|| "Could not remove backend"),
            // This is to avoid the error message
            &RequestType::Logging(_)
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

            other_request => {
                bail!("state cannot handle request message: {:#?}", other_request);
            }
        }
    }

    fn add_cluster(&mut self, cluster: &Cluster) -> anyhow::Result<()> {
        let cluster = cluster.clone();
        self.clusters.insert(cluster.cluster_id.clone(), cluster);
        Ok(())
    }

    fn remove_cluster(&mut self, cluster_id: &str) -> anyhow::Result<()> {
        match self.clusters.remove(cluster_id) {
            Some(_) => Ok(()),
            None => bail!("No cluster found with this id"),
        }
    }

    fn add_http_listener(&mut self, listener: &HttpListenerConfig) -> anyhow::Result<()> {
        match self.http_listeners.entry(listener.address.to_string()) {
            BTreeMapEntry::Vacant(vacant_entry) => vacant_entry.insert(listener.clone()),
            BTreeMapEntry::Occupied(_) => {
                bail!("The entry is occupied for address {}", listener.address)
            }
        };
        Ok(())
    }

    fn add_https_listener(&mut self, listener: &HttpsListenerConfig) -> anyhow::Result<()> {
        match self.https_listeners.entry(listener.address.clone()) {
            BTreeMapEntry::Vacant(vacant_entry) => vacant_entry.insert(listener.clone()),
            BTreeMapEntry::Occupied(_) => {
                bail!("The entry is occupied for address {}", listener.address)
            }
        };
        Ok(())
    }

    fn add_tcp_listener(&mut self, listener: &TcpListenerConfig) -> anyhow::Result<()> {
        match self.tcp_listeners.entry(listener.address.clone()) {
            BTreeMapEntry::Vacant(vacant_entry) => vacant_entry.insert(listener.clone()),
            BTreeMapEntry::Occupied(_) => {
                bail!("The entry is occupied for address {}", listener.address)
            }
        };
        Ok(())
    }

    fn remove_listener(&mut self, remove: &RemoveListener) -> anyhow::Result<()> {
        match ListenerType::from_i32(remove.proxy) {
            Some(ListenerType::Http) => self.remove_http_listener(&remove.address),
            Some(ListenerType::Https) => self.remove_https_listener(&remove.address),
            Some(ListenerType::Tcp) => self.remove_tcp_listener(&remove.address),
            None => bail!("Wrong ListenerType on RemoveListener request"),
        }
    }

    fn remove_http_listener(&mut self, address: &str) -> anyhow::Result<()> {
        if self.http_listeners.remove(address).is_none() {
            bail!("No http listener to remove at address {}", address);
        }
        Ok(())
    }

    fn remove_https_listener(&mut self, address: &str) -> anyhow::Result<()> {
        if self.https_listeners.remove(address).is_none() {
            bail!("No https listener to remove at address {}", address);
        }
        Ok(())
    }

    fn remove_tcp_listener(&mut self, address: &str) -> anyhow::Result<()> {
        if self.tcp_listeners.remove(address).is_none() {
            bail!("No tcp listener to remove at address {}", address);
        }
        Ok(())
    }

    fn activate_listener(&mut self, activate: &ActivateListener) -> anyhow::Result<()> {
        match ListenerType::from_i32(activate.proxy) {
            Some(ListenerType::Http) => {
                if self
                    .http_listeners
                    .get_mut(&activate.address)
                    .map(|listener| listener.active = true)
                    .is_none()
                {
                    bail!("No http listener found with address {}", activate.address)
                }
            }
            Some(ListenerType::Https) => {
                if self
                    .https_listeners
                    .get_mut(&activate.address)
                    .map(|listener| listener.active = true)
                    .is_none()
                {
                    bail!("No https listener found with address {}", activate.address)
                }
            }
            Some(ListenerType::Tcp) => {
                if self
                    .tcp_listeners
                    .get_mut(&activate.address)
                    .map(|listener| listener.active = true)
                    .is_none()
                {
                    bail!("No tcp listener found with address {}", activate.address)
                }
            }
            None => bail!("Wrong variant for ListenerType on request"),
        }
        Ok(())
    }

    fn deactivate_listener(&mut self, deactivate: &DeactivateListener) -> anyhow::Result<()> {
        match ListenerType::from_i32(deactivate.proxy) {
            Some(ListenerType::Http) => {
                if self
                    .http_listeners
                    .get_mut(&deactivate.address)
                    .map(|listener| listener.active = false)
                    .is_none()
                {
                    bail!("No http listener found with address {}", deactivate.address)
                }
            }
            Some(ListenerType::Https) => {
                if self
                    .https_listeners
                    .get_mut(&deactivate.address)
                    .map(|listener| listener.active = false)
                    .is_none()
                {
                    bail!(
                        "No https listener found with address {}",
                        deactivate.address
                    )
                }
            }
            Some(ListenerType::Tcp) => {
                if self
                    .tcp_listeners
                    .get_mut(&deactivate.address)
                    .map(|listener| listener.active = false)
                    .is_none()
                {
                    bail!("No tcp listener found with address {}", deactivate.address)
                }
            }
            None => bail!("Wrong variant for ListenerType on request"),
        }
        Ok(())
    }

    fn add_http_frontend(&mut self, front: &RequestHttpFrontend) -> anyhow::Result<()> {
        match self.http_fronts.entry(front.to_string()) {
            BTreeMapEntry::Vacant(e) => e.insert(front.clone().to_frontend()?),
            BTreeMapEntry::Occupied(_) => bail!("This frontend is already present: {:?}", front),
        };
        Ok(())
    }

    fn add_https_frontend(&mut self, front: &RequestHttpFrontend) -> anyhow::Result<()> {
        match self.https_fronts.entry(front.to_string()) {
            BTreeMapEntry::Vacant(e) => e.insert(front.clone().to_frontend()?),
            BTreeMapEntry::Occupied(_) => bail!("This frontend is already present: {:?}", front),
        };
        Ok(())
    }

    fn remove_http_frontend(&mut self, front: &RequestHttpFrontend) -> anyhow::Result<()> {
        if self.http_fronts.remove(&front.to_string()).is_none() {
            let error_msg = match &front.cluster_id {
                Some(cluster_id) => format!(
                    "No such frontend at {} for the cluster {}",
                    front.address, cluster_id
                ),
                None => format!("No such frontend at {}", front.address),
            };
            bail!(error_msg);
        }
        Ok(())
    }

    fn remove_https_frontend(&mut self, front: &RequestHttpFrontend) -> anyhow::Result<()> {
        if self.https_fronts.remove(&front.to_string()).is_none() {
            let error_msg = match &front.cluster_id {
                Some(cluster_id) => format!(
                    "No such frontend at {} for the cluster {}",
                    front.address, cluster_id
                ),
                None => format!("No such frontend at {}", front.address),
            };
            bail!(error_msg);
        }
        Ok(())
    }

    fn add_certificate(&mut self, add: &AddCertificate) -> anyhow::Result<()> {
        let fingerprint = Fingerprint(
            calculate_fingerprint(add.certificate.certificate.as_bytes())
                .with_context(|| "cannot calculate the certificate's fingerprint")?,
        );

        let address = add
            .address
            .parse()
            .with_context(|| "Could not parse socket address")?;

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

    fn remove_certificate(&mut self, remove: &RemoveCertificate) -> anyhow::Result<()> {
        let fingerprint = Fingerprint(
            hex::decode(&remove.fingerprint)
                .with_context(|| "Failed at decoding the string (expected hexadecimal data)")?,
        );

        let address = remove
            .address
            .parse()
            .with_context(|| "Could not parse socket address")?;

        if let Some(index) = self.certificates.get_mut(&address) {
            index.remove(&fingerprint);
        }

        Ok(())
    }

    /// - Remove old certificate from certificates, using the old fingerprint
    /// - calculate the new fingerprint
    /// - insert the new certificate with the new fingerprint as key
    /// - check that the new entry is present in the certificates hashmap
    fn replace_certificate(&mut self, replace: &ReplaceCertificate) -> anyhow::Result<()> {
        let address = replace
            .address
            .parse()
            .with_context(|| "Could not parse socket address")?;

        let old_fingerprint = Fingerprint(
            hex::decode(&replace.old_fingerprint)
                .with_context(|| "Failed at decoding the string (expected hexadecimal data)")?,
        );

        self.certificates
            .get_mut(&address)
            .with_context(|| format!("No certificate to replace for address {}", replace.address))?
            .remove(&old_fingerprint);

        let new_fingerprint = Fingerprint(
            calculate_fingerprint(replace.new_certificate.certificate.as_bytes())
                .with_context(|| "cannot obtain the certificate's fingerprint")?,
        );

        self.certificates
            .get_mut(&address)
            .map(|certs| certs.insert(new_fingerprint.clone(), replace.new_certificate.clone()));

        if !self
            .certificates
            .get(&address)
            .with_context(|| {
                "Unlikely error. This entry in the certificate hashmap should be present"
            })?
            .contains_key(&new_fingerprint)
        {
            bail!(format!(
                "Failed to insert the new certificate for address {}",
                replace.address
            ))
        }
        Ok(())
    }

    fn add_tcp_frontend(&mut self, front: &RequestTcpFrontend) -> anyhow::Result<()> {
        let tcp_frontends = self
            .tcp_fronts
            .entry(front.cluster_id.clone())
            .or_insert_with(Vec::new);

        let tcp_frontend = TcpFrontend {
            cluster_id: front.cluster_id.clone(),
            address: front
                .address
                .parse()
                .with_context(|| "wrong socket address")?,
            tags: front.tags.clone(),
        };
        if tcp_frontends.contains(&tcp_frontend) {
            bail!("This tcp frontend is already present: {:?}", tcp_frontend);
        }

        tcp_frontends.push(tcp_frontend);
        Ok(())
    }

    fn remove_tcp_frontend(&mut self, front_to_remove: &RequestTcpFrontend) -> anyhow::Result<()> {
        let address = front_to_remove
            .address
            .parse()
            .with_context(|| "wrong socket address")?;

        let tcp_frontends = self
            .tcp_fronts
            .get_mut(&front_to_remove.cluster_id)
            .with_context(|| {
                format!(
                    "cluster {} has no frontends at {} (custom tags: {:?})",
                    front_to_remove.cluster_id, front_to_remove.address, front_to_remove.tags
                )
            })?;

        let len = tcp_frontends.len();
        tcp_frontends.retain(|front| front.address != address);
        if tcp_frontends.len() == len {
            bail!("Removed no frontend");
        }
        Ok(())
    }

    fn add_backend(&mut self, add_backend: &AddBackend) -> anyhow::Result<()> {
        let backend = Backend {
            address: add_backend
                .address
                .parse()
                .with_context(|| "wrong socket address")?,
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

    fn remove_backend(&mut self, backend: &RemoveBackend) -> anyhow::Result<()> {
        let backend_list = self
            .backends
            .get_mut(&backend.cluster_id)
            .with_context(|| {
                format!(
                    "cluster {} has no backends {} at {}",
                    backend.cluster_id, backend.backend_id, backend.address,
                )
            })?;

        let len = backend_list.len();
        backend_list.retain(|b| {
            b.backend_id != backend.backend_id || b.address.to_string() != backend.address
        });
        backend_list.sort();
        if backend_list.len() == len {
            bail!("Removed no backend");
        }
        Ok(())
    }

    pub fn generate_requests(&self) -> Vec<Request> {
        let mut v = Vec::new();

        for listener in self.http_listeners.values() {
            v.push(Request {
                request_type: Some(RequestType::AddHttpListener(listener.clone())),
            });
            if listener.active {
                v.push(Request {
                    request_type: Some(RequestType::ActivateListener(ActivateListener {
                        address: listener.address.clone(),
                        proxy: ListenerType::Http.into(),
                        from_scm: false,
                    })),
                });
            }
        }

        for listener in self.https_listeners.values() {
            v.push(Request {
                request_type: Some(RequestType::AddHttpsListener(listener.clone())),
            });
            if listener.active {
                v.push(Request {
                    request_type: Some(RequestType::ActivateListener(ActivateListener {
                        address: listener.address.clone(),
                        proxy: ListenerType::Https.into(),
                        from_scm: false,
                    })),
                });
            }
        }

        for listener in self.tcp_listeners.values() {
            v.push(Request {
                request_type: Some(RequestType::AddTcpListener(listener.clone())),
            });
            if listener.active {
                v.push(Request {
                    request_type: Some(RequestType::ActivateListener(ActivateListener {
                        address: listener.address.clone(),
                        proxy: ListenerType::Tcp.into(),
                        from_scm: false,
                    })),
                });
            }
        }

        for cluster in self.clusters.values() {
            v.push(Request {
                request_type: Some(RequestType::AddCluster(cluster.clone())),
            });
        }

        for front in self.http_fronts.values() {
            v.push(Request {
                request_type: Some(RequestType::AddHttpFrontend(front.clone().into())),
            });
        }

        for (front, certs) in self.certificates.iter() {
            for certificate_and_key in certs.values() {
                v.push(Request {
                    request_type: Some(RequestType::AddCertificate(AddCertificate {
                        address: front.to_string(),
                        certificate: certificate_and_key.clone(),
                        expired_at: None,
                    })),
                });
            }
        }

        for front in self.https_fronts.values() {
            v.push(Request {
                request_type: Some(RequestType::AddHttpsFrontend(front.clone().into())),
            });
        }

        for front_list in self.tcp_fronts.values() {
            for front in front_list {
                v.push(Request {
                    request_type: Some(RequestType::AddTcpFrontend(front.clone().into())),
                });
            }
        }

        for backend_list in self.backends.values() {
            for backend in backend_list {
                v.push(Request {
                    request_type: Some(RequestType::AddBackend(backend.clone().to_add_backend())),
                });
            }
        }

        v
    }

    pub fn generate_activate_requests(&self) -> Vec<Request> {
        let mut v = Vec::new();
        for front in self
            .http_listeners
            .iter()
            .filter(|(_, listener)| listener.active)
            .map(|(k, _)| k)
        {
            v.push(Request {
                request_type: Some(RequestType::ActivateListener(ActivateListener {
                    address: front.to_string(),
                    proxy: ListenerType::Http.into(),
                    from_scm: false,
                })),
            });
        }

        for front in self
            .https_listeners
            .iter()
            .filter(|(_, listener)| listener.active)
            .map(|(k, _)| k)
        {
            v.push(Request {
                request_type: Some(RequestType::ActivateListener(ActivateListener {
                    address: front.to_string(),
                    proxy: ListenerType::Https.into(),
                    from_scm: false,
                })),
            });
        }
        for front in self
            .tcp_listeners
            .iter()
            .filter(|(_, listener)| listener.active)
            .map(|(k, _)| k)
        {
            v.push(Request {
                request_type: Some(RequestType::ActivateListener(ActivateListener {
                    address: front.to_string(),
                    proxy: ListenerType::Tcp.into(),
                    from_scm: false,
                })),
            });
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

        let mut v = vec![];

        for address in removed_tcp_listeners {
            if self.tcp_listeners[*address].active {
                v.push(Request {
                    request_type: Some(RequestType::DeactivateListener(DeactivateListener {
                        address: address.to_string(),
                        proxy: ListenerType::Tcp.into(),
                        to_scm: false,
                    })),
                });
            }

            v.push(Request {
                request_type: Some(RequestType::RemoveListener(RemoveListener {
                    address: address.to_string(),
                    proxy: ListenerType::Tcp.into(),
                })),
            });
        }

        for address in added_tcp_listeners.clone() {
            v.push(Request {
                request_type: Some(RequestType::AddTcpListener(
                    other.tcp_listeners[*address].clone(),
                )),
            });

            if other.tcp_listeners[*address].active {
                v.push(Request {
                    request_type: Some(RequestType::ActivateListener(ActivateListener {
                        address: address.to_string(),
                        proxy: ListenerType::Tcp.into(),
                        from_scm: false,
                    })),
                });
            }
        }

        for address in removed_http_listeners {
            if self.http_listeners[*address].active {
                v.push(Request {
                    request_type: Some(RequestType::DeactivateListener(DeactivateListener {
                        address: address.to_string(),
                        proxy: ListenerType::Http.into(),
                        to_scm: false,
                    })),
                });
            }

            v.push(Request {
                request_type: Some(RequestType::RemoveListener(RemoveListener {
                    address: address.to_string(),
                    proxy: ListenerType::Http.into(),
                })),
            });
        }

        for address in added_http_listeners.clone() {
            v.push(Request {
                request_type: Some(RequestType::AddHttpListener(
                    other.http_listeners[*address].clone(),
                )),
            });

            if other.http_listeners[*address].active {
                v.push(Request {
                    request_type: Some(RequestType::ActivateListener(ActivateListener {
                        address: address.to_string(),
                        proxy: ListenerType::Http.into(),
                        from_scm: false,
                    })),
                });
            }
        }

        for address in removed_https_listeners {
            if self.https_listeners[*address].active {
                v.push(Request {
                    request_type: Some(RequestType::DeactivateListener(DeactivateListener {
                        address: address.to_string(),
                        proxy: ListenerType::Https.into(),
                        to_scm: false,
                    })),
                });
            }

            v.push(Request {
                request_type: Some(RequestType::RemoveListener(RemoveListener {
                    address: address.to_string(),
                    proxy: ListenerType::Https.into(),
                })),
            });
        }

        for address in added_https_listeners.clone() {
            v.push(Request {
                request_type: Some(RequestType::AddHttpsListener(
                    other.https_listeners[*address].clone(),
                )),
            });

            if other.https_listeners[*address].active {
                v.push(Request {
                    request_type: Some(RequestType::ActivateListener(ActivateListener {
                        address: address.to_string(),
                        proxy: ListenerType::Https.into(),
                        from_scm: false,
                    })),
                });
            }
        }

        for addr in my_tcp_listeners.intersection(&their_tcp_listeners) {
            let my_listener = &self.tcp_listeners[*addr];
            let their_listener = &other.tcp_listeners[*addr];

            if my_listener != their_listener {
                v.push(Request {
                    request_type: Some(RequestType::RemoveListener(RemoveListener {
                        address: addr.to_string(),
                        proxy: ListenerType::Tcp.into(),
                    })),
                });
                // any added listener should be unactive
                let mut listener_to_add = their_listener.clone();
                listener_to_add.active = false;
                v.push(Request {
                    request_type: Some(RequestType::AddTcpListener(listener_to_add)),
                });
            }

            if my_listener.active && !their_listener.active {
                v.push(Request {
                    request_type: Some(RequestType::DeactivateListener(DeactivateListener {
                        address: addr.to_string(),
                        proxy: ListenerType::Tcp.into(),
                        to_scm: false,
                    })),
                });
            }

            if !my_listener.active && their_listener.active {
                v.push(Request {
                    request_type: Some(RequestType::ActivateListener(ActivateListener {
                        address: addr.to_string(),
                        proxy: ListenerType::Tcp.into(),
                        from_scm: false,
                    })),
                });
            }
        }

        for addr in my_http_listeners.intersection(&their_http_listeners) {
            let my_listener = &self.http_listeners[*addr];
            let their_listener = &other.http_listeners[*addr];

            if my_listener != their_listener {
                v.push(Request {
                    request_type: Some(RequestType::RemoveListener(RemoveListener {
                        address: addr.to_string(),
                        proxy: ListenerType::Http.into(),
                    })),
                });
                // any added listener should be unactive
                let mut listener_to_add = their_listener.clone();
                listener_to_add.active = false;
                v.push(Request {
                    request_type: Some(RequestType::AddHttpListener(listener_to_add)),
                });
            }

            if my_listener.active && !their_listener.active {
                v.push(Request {
                    request_type: Some(RequestType::DeactivateListener(DeactivateListener {
                        address: addr.to_string(),
                        proxy: ListenerType::Http.into(),
                        to_scm: false,
                    })),
                });
            }

            if !my_listener.active && their_listener.active {
                v.push(Request {
                    request_type: Some(RequestType::ActivateListener(ActivateListener {
                        address: addr.to_string(),
                        proxy: ListenerType::Http.into(),
                        from_scm: false,
                    })),
                });
            }
        }

        for addr in my_https_listeners.intersection(&their_https_listeners) {
            let my_listener = &self.https_listeners[*addr];
            let their_listener = &other.https_listeners[*addr];

            if my_listener != their_listener {
                v.push(Request {
                    request_type: Some(RequestType::RemoveListener(RemoveListener {
                        address: addr.to_string(),
                        proxy: ListenerType::Https.into(),
                    })),
                });
                // any added listener should be unactive
                let mut listener_to_add = their_listener.clone();
                listener_to_add.active = false;
                v.push(Request {
                    request_type: Some(RequestType::AddHttpsListener(listener_to_add)),
                });
            }

            if my_listener.active && !their_listener.active {
                v.push(Request {
                    request_type: Some(RequestType::DeactivateListener(DeactivateListener {
                        address: addr.to_string(),
                        proxy: ListenerType::Https.into(),
                        to_scm: false,
                    })),
                });
            }

            if !my_listener.active && their_listener.active {
                v.push(Request {
                    request_type: Some(RequestType::ActivateListener(ActivateListener {
                        address: addr.to_string(),
                        proxy: ListenerType::Https.into(),
                        from_scm: false,
                    })),
                });
            }
        }

        for (cluster_id, res) in diff_map(self.clusters.iter(), other.clusters.iter()) {
            match res {
                DiffResult::Added | DiffResult::Changed => v.push(Request {
                    request_type: Some(RequestType::AddCluster(
                        other.clusters.get(cluster_id).unwrap().clone(),
                    )),
                }),
                DiffResult::Removed => v.push(Request {
                    request_type: Some(RequestType::RemoveCluster(cluster_id.to_string())),
                }),
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
                    v.push(Request {
                        request_type: Some(RequestType::AddBackend(
                            backend.clone().to_add_backend(),
                        )),
                    });
                }
                DiffResult::Removed => {
                    let backend = self
                        .backends
                        .get(cluster_id)
                        .and_then(|v| v.iter().find(|b| &b.backend_id == backend_id))
                        .unwrap();

                    v.push(Request {
                        request_type: Some(RequestType::RemoveBackend(RemoveBackend {
                            cluster_id: backend.cluster_id.clone(),
                            backend_id: backend.backend_id.clone(),
                            address: backend.address.to_string(),
                        })),
                    });
                }
                DiffResult::Changed => {
                    let backend = self
                        .backends
                        .get(cluster_id)
                        .and_then(|v| v.iter().find(|b| &b.backend_id == backend_id))
                        .unwrap();

                    v.push(Request {
                        request_type: Some(RequestType::RemoveBackend(RemoveBackend {
                            cluster_id: backend.cluster_id.clone(),
                            backend_id: backend.backend_id.clone(),
                            address: backend.address.to_string(),
                        })),
                    });

                    let backend = other
                        .backends
                        .get(cluster_id)
                        .and_then(|v| v.iter().find(|b| &b.backend_id == backend_id))
                        .unwrap();
                    v.push(Request {
                        request_type: Some(RequestType::AddBackend(
                            backend.clone().to_add_backend(),
                        )),
                    });
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
            v.push(Request {
                request_type: Some(RequestType::RemoveHttpFrontend(front.clone().into())),
            });
        }

        for &(_, front) in added_http_fronts {
            v.push(Request {
                request_type: Some(RequestType::AddHttpFrontend(front.clone().into())),
            });
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
            v.push(Request {
                request_type: Some(RequestType::RemoveHttpsFrontend(front.clone().into())),
            });
        }

        for &(_, front) in added_https_fronts {
            v.push(Request {
                request_type: Some(RequestType::AddHttpsFrontend(front.clone().into())),
            });
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
            v.push(Request {
                request_type: Some(RequestType::RemoveTcpFrontend(front.clone().into())),
            });
        }

        for &(_, front) in added_tcp_fronts {
            v.push(Request {
                request_type: Some(RequestType::AddTcpFrontend(front.clone().into())),
            });
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
            v.push(Request {
                request_type: Some(RequestType::RemoveCertificate(RemoveCertificate {
                    address: address.to_string(),
                    fingerprint: fingerprint.to_string(),
                })),
            });
        }

        for &(address, fingerprint) in added_certificates {
            if let Some(certificate_and_key) = other
                .certificates
                .get(&address)
                .and_then(|certs| certs.get(fingerprint))
            {
                v.push(Request {
                    request_type: Some(RequestType::AddCertificate(AddCertificate {
                        address: address.to_string(),
                        certificate: certificate_and_key.clone(),
                        expired_at: None,
                    })),
                });
            }
        }

        for address in added_tcp_listeners {
            let listener = &other.tcp_listeners[*address];
            if listener.active {
                v.push(Request {
                    request_type: Some(RequestType::ActivateListener(ActivateListener {
                        address: listener.address.clone(),
                        proxy: ListenerType::Tcp.into(),
                        from_scm: false,
                    })),
                });
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
            if let Some(cluster_id) = &front.cluster_id {
                if let Some(s) = h.get_mut(cluster_id) {
                    front.hash(s);
                }
            }
        }

        for front in self.https_fronts.values() {
            if let Some(cluster_id) = &front.cluster_id {
                if let Some(s) = h.get_mut(cluster_id) {
                    front.hash(s);
                }
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
            if let Some(id) = &http_frontend.cluster_id {
                if id == cluster_id {
                    http_frontends.push(http_frontend.clone().into());
                }
            }
        }

        for https_frontend in self.https_fronts.values() {
            if let Some(id) = &https_frontend.cluster_id {
                if id == cluster_id {
                    https_frontends.push(https_frontend.clone().into());
                }
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
                if let Some(id) = &front.cluster_id {
                    cluster_ids.insert(id.to_string());
                }
            }
        });

        self.https_fronts.values().for_each(|front| {
            if domain_check(&front.hostname, &front.path, &hostname, &path) {
                if let Some(id) = &front.cluster_id {
                    cluster_ids.insert(id.to_string());
                }
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
                .filter(|(_, cert)| cert.names.contains(&domain))
                .map(|(fingerprint, cert)| (fingerprint.to_string(), cert.clone()))
                .collect()
        } else if let Some(f) = filters.fingerprint {
            self.certificates
                .values()
                .flat_map(|hash_map| hash_map.iter())
                .filter(|(fingerprint, _cert)| fingerprint.to_string() == f)
                .map(|(fingerprint, cert)| (fingerprint.to_string(), cert.clone()))
                .collect()
        } else {
            self.certificates
                .values()
                .flat_map(|hash_map| hash_map.iter())
                .map(|(fingerprint, cert)| (fingerprint.to_string(), cert.clone()))
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
            .dispatch(&Request {
                request_type: Some(RequestType::AddHttpFrontend(RequestHttpFrontend {
                    cluster_id: Some(String::from("cluster_1")),
                    hostname: String::from("lolcatho.st:8080"),
                    path: PathRule::prefix(String::from("/")),
                    address: "0.0.0.0:8080".to_string(),
                    position: RulePosition::Tree.into(),
                    ..Default::default()
                })),
            })
            .expect("Could not execute request");
        state
            .dispatch(&Request {
                request_type: Some(RequestType::AddHttpFrontend(RequestHttpFrontend {
                    cluster_id: Some(String::from("cluster_2")),
                    hostname: String::from("test.local"),
                    path: PathRule::prefix(String::from("/abc")),
                    address: "0.0.0.0:8080".to_string(),
                    position: RulePosition::Pre.into(),
                    ..Default::default()
                })),
            })
            .expect("Could not execute request");
        state
            .dispatch(&Request {
                request_type: Some(RequestType::AddBackend(AddBackend {
                    cluster_id: String::from("cluster_1"),
                    backend_id: String::from("cluster_1-0"),
                    address: "127.0.0.1:1026".parse().unwrap(),
                    ..Default::default()
                })),
            })
            .expect("Could not execute request");
        state
            .dispatch(&Request {
                request_type: Some(RequestType::AddBackend(AddBackend {
                    cluster_id: String::from("cluster_1"),
                    backend_id: String::from("cluster_1-1"),
                    address: "127.0.0.2:1027".parse().unwrap(),
                    ..Default::default()
                })),
            })
            .expect("Could not execute request");
        state
            .dispatch(&Request {
                request_type: Some(RequestType::AddBackend(AddBackend {
                    cluster_id: String::from("cluster_2"),
                    backend_id: String::from("cluster_2-0"),
                    address: "192.167.1.2:1026".parse().unwrap(),
                    ..Default::default()
                })),
            })
            .expect("Could not execute request");
        state
            .dispatch(&Request {
                request_type: Some(RequestType::AddBackend(AddBackend {
                    cluster_id: String::from("cluster_1"),
                    backend_id: String::from("cluster_1-3"),
                    address: "192.168.1.3:1027".parse().unwrap(),
                    ..Default::default()
                })),
            })
            .expect("Could not execute request");
        state
            .dispatch(&Request {
                request_type: Some(RequestType::RemoveBackend(RemoveBackend {
                    cluster_id: String::from("cluster_1"),
                    backend_id: String::from("cluster_1-3"),
                    address: "192.168.1.3:1027".parse().unwrap(),
                })),
            })
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
            .dispatch(&Request {
                request_type: Some(RequestType::AddHttpFrontend(RequestHttpFrontend {
                    cluster_id: Some(String::from("cluster_1")),
                    hostname: String::from("lolcatho.st:8080"),
                    path: PathRule::prefix(String::from("/")),
                    address: "0.0.0.0:8080".to_string(),
                    position: RulePosition::Post.into(),
                    ..Default::default()
                })),
            })
            .expect("Could not execute request");
        state
            .dispatch(&Request {
                request_type: Some(RequestType::AddHttpFrontend(RequestHttpFrontend {
                    cluster_id: Some(String::from("cluster_2")),
                    hostname: String::from("test.local"),
                    path: PathRule::prefix(String::from("/abc")),
                    address: "0.0.0.0:8080".to_string(),
                    ..Default::default()
                })),
            })
            .expect("Could not execute request");
        state
            .dispatch(&Request {
                request_type: Some(RequestType::AddBackend(AddBackend {
                    cluster_id: String::from("cluster_1"),
                    backend_id: String::from("cluster_1-0"),
                    address: "127.0.0.1:1026".parse().unwrap(),
                    load_balancing_parameters: Some(LoadBalancingParams::default()),
                    ..Default::default()
                })),
            })
            .expect("Could not execute request");
        state
            .dispatch(&Request {
                request_type: Some(RequestType::AddBackend(AddBackend {
                    cluster_id: String::from("cluster_1"),
                    backend_id: String::from("cluster_1-1"),
                    address: "127.0.0.2:1027".parse().unwrap(),
                    load_balancing_parameters: Some(LoadBalancingParams::default()),
                    ..Default::default()
                })),
            })
            .expect("Could not execute request");
        state
            .dispatch(&Request {
                request_type: Some(RequestType::AddBackend(AddBackend {
                    cluster_id: String::from("cluster_2"),
                    backend_id: String::from("cluster_2-0"),
                    address: "192.167.1.2:1026".parse().unwrap(),
                    load_balancing_parameters: Some(LoadBalancingParams::default()),
                    ..Default::default()
                })),
            })
            .expect("Could not execute request");
        state
            .dispatch(&Request {
                request_type: Some(RequestType::AddCluster(Cluster {
                    cluster_id: String::from("cluster_2"),
                    sticky_session: true,
                    https_redirect: true,
                    ..Default::default()
                })),
            })
            .expect("Could not execute request");

        let mut state2: ConfigState = Default::default();
        state2
            .dispatch(&Request {
                request_type: Some(RequestType::AddHttpFrontend(RequestHttpFrontend {
                    cluster_id: Some(String::from("cluster_1")),
                    hostname: String::from("lolcatho.st:8080"),
                    path: PathRule::prefix(String::from("/")),
                    address: "0.0.0.0:8080".to_string(),
                    position: RulePosition::Post.into(),
                    ..Default::default()
                })),
            })
            .expect("Could not execute request");
        state2
            .dispatch(&Request {
                request_type: Some(RequestType::AddBackend(AddBackend {
                    cluster_id: String::from("cluster_1"),
                    backend_id: String::from("cluster_1-0"),
                    address: "127.0.0.1:1026".parse().unwrap(),
                    load_balancing_parameters: Some(LoadBalancingParams::default()),
                    ..Default::default()
                })),
            })
            .expect("Could not execute request");
        state2
            .dispatch(&Request {
                request_type: Some(RequestType::AddBackend(AddBackend {
                    cluster_id: String::from("cluster_1"),
                    backend_id: String::from("cluster_1-1"),
                    address: "127.0.0.2:1027".parse().unwrap(),
                    load_balancing_parameters: Some(LoadBalancingParams::default()),
                    ..Default::default()
                })),
            })
            .expect("Could not execute request");
        state2
            .dispatch(&Request {
                request_type: Some(RequestType::AddBackend(AddBackend {
                    cluster_id: String::from("cluster_1"),
                    backend_id: String::from("cluster_1-2"),
                    address: "127.0.0.2:1028".parse().unwrap(),
                    load_balancing_parameters: Some(LoadBalancingParams::default()),
                    ..Default::default()
                })),
            })
            .expect("Could not execute request");
        state2
            .dispatch(&Request {
                request_type: Some(RequestType::AddCluster(Cluster {
                    cluster_id: String::from("cluster_3"),
                    sticky_session: false,
                    https_redirect: false,
                    ..Default::default()
                })),
            })
            .expect("Could not execute request");

        let e = vec![
            Request {
                request_type: Some(RequestType::RemoveHttpFrontend(RequestHttpFrontend {
                    cluster_id: Some(String::from("cluster_2")),
                    hostname: String::from("test.local"),
                    path: PathRule::prefix(String::from("/abc")),
                    address: "0.0.0.0:8080".to_string(),
                    ..Default::default()
                })),
            },
            Request {
                request_type: Some(RequestType::RemoveBackend(RemoveBackend {
                    cluster_id: String::from("cluster_2"),
                    backend_id: String::from("cluster_2-0"),
                    address: "192.167.1.2:1026".to_string(),
                })),
            },
            Request {
                request_type: Some(RequestType::AddBackend(AddBackend {
                    cluster_id: String::from("cluster_1"),
                    backend_id: String::from("cluster_1-2"),
                    address: "127.0.0.2:1028".to_string(),
                    load_balancing_parameters: Some(LoadBalancingParams::default()),
                    ..Default::default()
                })),
            },
            Request {
                request_type: Some(RequestType::RemoveCluster(String::from("cluster_2"))),
            },
            Request {
                request_type: Some(RequestType::AddCluster(Cluster {
                    cluster_id: String::from("cluster_3"),
                    sticky_session: false,
                    https_redirect: false,
                    ..Default::default()
                })),
            },
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
            .dispatch(&Request {
                request_type: Some(RequestType::AddBackend(AddBackend {
                    cluster_id: String::from("cluster_1"),
                    backend_id: String::from("cluster_1-2"),
                    address: "127.0.0.2:1028".to_string(),
                    load_balancing_parameters: Some(LoadBalancingParams::default()),
                    ..Default::default()
                })),
            })
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
            address: "0.0.0.0:8080".to_string(),
            ..Default::default()
        };

        let https_front_cluster1 = RequestHttpFrontend {
            cluster_id: Some(String::from("MyCluster_1")),
            hostname: String::from("lolcatho.st"),
            path: PathRule::prefix(String::from("")),
            address: "0.0.0.0:8443".to_string(),
            ..Default::default()
        };

        let http_front_cluster2 = RequestHttpFrontend {
            cluster_id: Some(String::from("MyCluster_2")),
            hostname: String::from("lolcatho.st"),
            path: PathRule::prefix(String::from("/api")),
            address: "0.0.0.0:8080".to_string(),
            ..Default::default()
        };

        let https_front_cluster2 = RequestHttpFrontend {
            cluster_id: Some(String::from("MyCluster_2")),
            hostname: String::from("lolcatho.st"),
            path: PathRule::prefix(String::from("/api")),
            address: "0.0.0.0:8443".to_string(),
            ..Default::default()
        };

        let add_http_front_cluster1 = Request {
            request_type: Some(RequestType::AddHttpFrontend(http_front_cluster1)),
        };
        let add_http_front_cluster2 = Request {
            request_type: Some(RequestType::AddHttpFrontend(http_front_cluster2)),
        };
        let add_https_front_cluster1 = Request {
            request_type: Some(RequestType::AddHttpsFrontend(https_front_cluster1)),
        };
        let add_https_front_cluster2 = Request {
            request_type: Some(RequestType::AddHttpsFrontend(https_front_cluster2)),
        };
        config
            .dispatch(&add_http_front_cluster1)
            .expect("Could not execute request");
        config
            .dispatch(&add_http_front_cluster2)
            .expect("Could not execute request");
        config
            .dispatch(&add_https_front_cluster1)
            .expect("Could not execute request");
        config
            .dispatch(&add_https_front_cluster2)
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
            .dispatch(&Request {
                request_type: Some(RequestType::AddBackend(AddBackend {
                    cluster_id: String::from("cluster_1"),
                    backend_id: String::from("cluster_1-0"),
                    address: "127.0.0.1:1026".to_string(),
                    load_balancing_parameters: Some(LoadBalancingParams::default()),
                    ..Default::default()
                })),
            })
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
            .dispatch(&Request {
                request_type: Some(RequestType::AddBackend(b.clone().to_add_backend())),
            })
            .expect("Could not execute order");

        assert_eq!(state.backends.get("cluster_1").unwrap(), &vec![b]);
    }

    #[test]
    fn listener_diff() {
        let mut state: ConfigState = Default::default();
        state
            .dispatch(&Request {
                request_type: Some(RequestType::AddTcpListener(TcpListenerConfig {
                    address: "0.0.0.0:1234".parse().unwrap(),
                    ..Default::default()
                })),
            })
            .expect("Could not execute request");
        state
            .dispatch(&Request {
                request_type: Some(RequestType::ActivateListener(ActivateListener {
                    address: "0.0.0.0:1234".parse().unwrap(),
                    proxy: ListenerType::Tcp.into(),
                    from_scm: false,
                })),
            })
            .expect("Could not execute request");
        state
            .dispatch(&Request {
                request_type: Some(RequestType::AddHttpListener(HttpListenerConfig {
                    address: "0.0.0.0:8080".parse().unwrap(),
                    ..Default::default()
                })),
            })
            .expect("Could not execute request");
        state
            .dispatch(&Request {
                request_type: Some(RequestType::AddHttpsListener(HttpsListenerConfig {
                    address: "0.0.0.0:8443".parse().unwrap(),
                    ..Default::default()
                })),
            })
            .expect("Could not execute request");
        state
            .dispatch(&Request {
                request_type: Some(RequestType::ActivateListener(ActivateListener {
                    address: "0.0.0.0:8443".parse().unwrap(),
                    proxy: ListenerType::Https.into(),
                    from_scm: false,
                })),
            })
            .expect("Could not execute request");

        let mut state2: ConfigState = Default::default();
        state2
            .dispatch(&Request {
                request_type: Some(RequestType::AddTcpListener(TcpListenerConfig {
                    address: "0.0.0.0:1234".parse().unwrap(),
                    expect_proxy: true,
                    ..Default::default()
                })),
            })
            .expect("Could not execute request");
        state2
            .dispatch(&Request {
                request_type: Some(RequestType::AddHttpListener(HttpListenerConfig {
                    address: "0.0.0.0:8080".parse().unwrap(),
                    answer_404: "test".to_string(),
                    ..Default::default()
                })),
            })
            .expect("Could not execute request");
        state2
            .dispatch(&Request {
                request_type: Some(RequestType::ActivateListener(ActivateListener {
                    address: "0.0.0.0:8080".parse().unwrap(),
                    proxy: ListenerType::Http.into(),
                    from_scm: false,
                })),
            })
            .expect("Could not execute request");
        state2
            .dispatch(&Request {
                request_type: Some(RequestType::AddHttpsListener(HttpsListenerConfig {
                    address: "0.0.0.0:8443".parse().unwrap(),
                    answer_404: String::from("test"),
                    ..Default::default()
                })),
            })
            .expect("Could not execute request");
        state2
            .dispatch(&Request {
                request_type: Some(RequestType::ActivateListener(ActivateListener {
                    address: "0.0.0.0:8443".parse().unwrap(),
                    proxy: ListenerType::Https.into(),
                    from_scm: false,
                })),
            })
            .expect("Could not execute request");

        let e = vec![
            Request {
                request_type: Some(RequestType::RemoveListener(RemoveListener {
                    address: "0.0.0.0:1234".parse().unwrap(),
                    proxy: ListenerType::Tcp.into(),
                })),
            },
            Request {
                request_type: Some(RequestType::AddTcpListener(TcpListenerConfig {
                    address: "0.0.0.0:1234".parse().unwrap(),
                    expect_proxy: true,
                    ..Default::default()
                })),
            },
            Request {
                request_type: Some(RequestType::DeactivateListener(DeactivateListener {
                    address: "0.0.0.0:1234".parse().unwrap(),
                    proxy: ListenerType::Tcp.into(),
                    to_scm: false,
                })),
            },
            Request {
                request_type: Some(RequestType::RemoveListener(RemoveListener {
                    address: "0.0.0.0:8080".parse().unwrap(),
                    proxy: ListenerType::Http.into(),
                })),
            },
            Request {
                request_type: Some(RequestType::AddHttpListener(HttpListenerConfig {
                    address: "0.0.0.0:8080".parse().unwrap(),
                    answer_404: String::from("test"),
                    ..Default::default()
                })),
            },
            Request {
                request_type: Some(RequestType::ActivateListener(ActivateListener {
                    address: "0.0.0.0:8080".parse().unwrap(),
                    proxy: ListenerType::Http.into(),
                    from_scm: false,
                })),
            },
            Request {
                request_type: Some(RequestType::RemoveListener(RemoveListener {
                    address: "0.0.0.0:8443".parse().unwrap(),
                    proxy: ListenerType::Https.into(),
                })),
            },
            Request {
                request_type: Some(RequestType::AddHttpsListener(HttpsListenerConfig {
                    address: "0.0.0.0:8443".parse().unwrap(),
                    answer_404: String::from("test"),
                    ..Default::default()
                })),
            },
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
            .dispatch(&Request {
                request_type: Some(RequestType::AddCertificate(add_certificate)),
            })
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
