use std::{
    collections::{
        btree_map::Entry as BTreeMapEntry, hash_map::DefaultHasher, BTreeMap, BTreeSet, HashMap,
        HashSet,
    },
    fmt,
    hash::{Hash, Hasher},
    iter::{repeat, FromIterator},
    net::SocketAddr,
};

use anyhow::{bail, Context};
use serde::de::{self, Visitor};

use crate::{
    certificate::calculate_fingerprint,
    command::{ClusterHash, FrontendFilters, ListedFrontends},
    worker::{
        ActivateListener, AddCertificate, Backend, Certificate, CertificateWithNames, Cluster,
        ClusterInformation, DeactivateListener, Fingerprint, HttpFrontend, HttpListenerConfig,
        HttpsListenerConfig, ListenerType, PathRule, PathRuleKind, RemoveBackend,
        RemoveCertificate, RemoveListener, ReplaceCertificate, TcpFrontend, TcpListenerConfig,
        WorkerOrder,
    },
};

/// To use throughout Sōzu
pub type ClusterId = String;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendsToACluster {
    pub cluster_id: ClusterId,
    pub backends: Vec<Backend>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteKeyToHttpFrontend {
    route_key: RouteKey,
    frontend: HttpFrontend,
}

/// A representation of the entire state of a Sōzu worker
/// Those fields are never accessed
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigState {
    clusters: BTreeMap<ClusterId, Cluster>,
    backends: BTreeMap<ClusterId, Backends>,
    pub http_listeners: BTreeMap<SocketAddr, HttpListenerConfig>,
    pub https_listeners: BTreeMap<SocketAddr, HttpsListenerConfig>,
    pub tcp_listeners: BTreeMap<SocketAddr, TcpListenerConfig>,
    /// HTTP frontends, indexed by RouteKey, a serialization of (address, hostname, path)
    pub http_fronts: BTreeMap<String, HttpFrontend>,
    /// HTTPS frontends, indexed by RouteKey, a serialization of (address, hostname, path)
    pub https_fronts: BTreeMap<String, HttpFrontend>,
    /// TCP frontends, in groups that are indexed by the cluster id
    pub tcp_fronts: BTreeMap<ClusterId, TcpFrontends>,
    /// socket address -> map of certificate
    certificates: BTreeMap<SocketAddr, Certificates>,
}

/// contains a list of backends
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Backends {
    vec: Vec<Backend>,
}

impl Backends {
    fn new() -> Self {
        Self { vec: Vec::new() }
    }
}

/// Contains a map: fingerprint -> certificate
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Certificates {
    map: HashMap<Fingerprint, Certificate>,
}

impl Certificates {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }
}

/// contains a list of TCP frontends
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TcpFrontends {
    pub vec: Vec<TcpFrontend>,
}

impl TcpFrontends {
    fn new() -> Self {
        Self { vec: Vec::new() }
    }
}

impl ConfigState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn dispatch(&mut self, order: &WorkerOrder) -> anyhow::Result<()> {
        match order {
            WorkerOrder::AddCluster(cluster) => self
                .add_cluster(cluster)
                .with_context(|| "Could not add cluster"),
            WorkerOrder::RemoveCluster { cluster_id } => self
                .remove_cluster(cluster_id)
                .with_context(|| "Could not remove cluster"),
            WorkerOrder::AddHttpListener(listener) => self
                .add_http_listener(listener)
                .with_context(|| "Could not add HTTP listener"),
            WorkerOrder::AddHttpsListener(listener) => self
                .add_https_listener(listener)
                .with_context(|| "Could not add HTTPS listener"),
            WorkerOrder::AddTcpListener(listener) => self
                .add_tcp_listener(listener)
                .with_context(|| "Could not add TCP listener"),
            WorkerOrder::RemoveListener(remove) => self
                .remove_listener(remove)
                .with_context(|| "Could not remove listener"),
            WorkerOrder::ActivateListener(activate) => self
                .activate_listener(activate)
                .with_context(|| "Could not activate listener"),
            WorkerOrder::DeactivateListener(deactivate) => self
                .deactivate_listener(deactivate)
                .with_context(|| "Could not deactivate listener"),
            WorkerOrder::AddHttpFrontend(front) => self
                .add_http_frontend(front)
                .with_context(|| "Could not add HTTP frontend"),
            WorkerOrder::RemoveHttpFrontend(front) => self
                .remove_http_frontend(front)
                .with_context(|| "Could not remove HTTP frontend"),
            WorkerOrder::AddCertificate(add) => self
                .add_certificate(add)
                .with_context(|| "Could not add certificate"),
            WorkerOrder::RemoveCertificate(remove) => self
                .remove_certificate(remove)
                .with_context(|| "Could not remove certificate"),
            WorkerOrder::ReplaceCertificate(replace) => self
                .replace_certificate(replace)
                .with_context(|| "Could not replace certificate"),
            WorkerOrder::AddHttpsFrontend(front) => self
                .add_http_frontend(front)
                .with_context(|| "Could not add HTTPS frontend"),
            WorkerOrder::RemoveHttpsFrontend(front) => self
                .remove_http_frontend(front)
                .with_context(|| "Could not remove HTTPS frontend"),
            WorkerOrder::AddTcpFrontend(front) => self
                .add_tcp_frontend(front)
                .with_context(|| "Could not add TCP frontend"),
            WorkerOrder::RemoveTcpFrontend(front) => self
                .remove_tcp_frontend(front)
                .with_context(|| "Could not remove TCP frontend"),
            WorkerOrder::AddBackend(backend) => self
                .add_backend(backend)
                .with_context(|| "Could not add backend"),
            WorkerOrder::RemoveBackend(backend) => self
                .remove_backend(backend)
                .with_context(|| "Could not remove backend"),
            // This is to avoid the error message
            &WorkerOrder::Logging(_)
            | &WorkerOrder::Status
            | &WorkerOrder::SoftStop
            | &WorkerOrder::QueryClusterById { cluster_id: _ }
            | &WorkerOrder::QueryClusterByDomain {
                hostname: _,
                path: _,
            }
            | &WorkerOrder::QueryAllCertificates
            | &WorkerOrder::QueryCertificateByDomain(_)
            | &WorkerOrder::QueryCertificateByFingerprint(_)
            | &WorkerOrder::QueryMetrics(_)
            | &WorkerOrder::QueryClustersHashes
            | &WorkerOrder::ConfigureMetrics(_)
            | &WorkerOrder::ReturnListenSockets
            | &WorkerOrder::HardStop => Ok(()),
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
        match self.http_listeners.entry(listener.address) {
            BTreeMapEntry::Vacant(vacant_entry) => vacant_entry.insert(listener.clone()), // TODO: should we deactivate here?
            BTreeMapEntry::Occupied(_) => {
                bail!("The entry is occupied for address {}", listener.address)
            }
        };
        Ok(())
    }

    fn add_https_listener(&mut self, listener: &HttpsListenerConfig) -> anyhow::Result<()> {
        match self.https_listeners.entry(listener.address) {
            BTreeMapEntry::Vacant(vacant_entry) => vacant_entry.insert(listener.clone()), // TODO: should we deactivate here?
            BTreeMapEntry::Occupied(_) => {
                bail!("The entry is occupied for address {}", listener.address)
            }
        };
        Ok(())
    }

    fn add_tcp_listener(&mut self, listener: &TcpListenerConfig) -> anyhow::Result<()> {
        match self.tcp_listeners.entry(listener.address) {
            BTreeMapEntry::Vacant(vacant_entry) => vacant_entry.insert(listener.clone()),
            BTreeMapEntry::Occupied(_) => {
                bail!("The entry is occupied for address {}", listener.address)
            }
        };
        Ok(())
    }

    fn remove_listener(&mut self, remove: &RemoveListener) -> anyhow::Result<()> {
        match remove.proxy {
            ListenerType::HTTP => self.remove_http_listener(&remove.address),
            ListenerType::HTTPS => self.remove_https_listener(&remove.address),
            ListenerType::TCP => self.remove_tcp_listener(&remove.address),
        }
    }

    fn remove_http_listener(&mut self, address: &SocketAddr) -> anyhow::Result<()> {
        if self.http_listeners.remove(address).is_none() {
            bail!("No http listener to remove at address {}", address);
        }
        Ok(())
    }

    fn remove_https_listener(&mut self, address: &SocketAddr) -> anyhow::Result<()> {
        if self.https_listeners.remove(address).is_none() {
            bail!("No https listener to remove at address {}", address);
        }
        Ok(())
    }

    fn remove_tcp_listener(&mut self, address: &SocketAddr) -> anyhow::Result<()> {
        if self.tcp_listeners.remove(address).is_none() {
            bail!("No tcp listener to remove at address {}", address);
        }
        Ok(())
    }

    fn activate_listener(&mut self, activate: &ActivateListener) -> anyhow::Result<()> {
        match activate.proxy {
            ListenerType::HTTP => {
                if self
                    .http_listeners
                    .get_mut(&activate.address)
                    .map(|listener| listener.activated = true)
                    .is_none()
                {
                    bail!("No http listener found with address {}", activate.address)
                }
            }
            ListenerType::HTTPS => {
                if self
                    .https_listeners
                    .get_mut(&activate.address)
                    .map(|listener| listener.activated = true)
                    .is_none()
                {
                    bail!("No https listener found with address {}", activate.address)
                }
            }
            ListenerType::TCP => {
                if self
                    .tcp_listeners
                    .get_mut(&activate.address)
                    .map(|listener| listener.activated = true)
                    .is_none()
                {
                    bail!("No tcp listener found with address {}", activate.address)
                }
            }
        }
        Ok(())
    }

    fn deactivate_listener(&mut self, deactivate: &DeactivateListener) -> anyhow::Result<()> {
        match deactivate.proxy {
            ListenerType::HTTP => {
                if self
                    .http_listeners
                    .get_mut(&deactivate.address)
                    .map(|listener| listener.activated = false)
                    .is_none()
                {
                    bail!("No http listener found with address {}", deactivate.address)
                }
            }
            ListenerType::HTTPS => {
                if self
                    .https_listeners
                    .get_mut(&deactivate.address)
                    .map(|listener| listener.activated = false)
                    .is_none()
                {
                    bail!(
                        "No https listener found with address {}",
                        deactivate.address
                    )
                }
            }
            ListenerType::TCP => {
                if self
                    .tcp_listeners
                    .get_mut(&deactivate.address)
                    .map(|listener| listener.activated = false)
                    .is_none()
                {
                    bail!("No tcp listener found with address {}", deactivate.address)
                }
            }
        }
        Ok(())
    }

    fn add_http_frontend(&mut self, front: &HttpFrontend) -> anyhow::Result<()> {
        match self.http_fronts.entry(front.route_key()?) {
            BTreeMapEntry::Vacant(e) => e.insert(front.clone()),
            BTreeMapEntry::Occupied(_) => bail!("This frontend is already present: {:?}", front),
        };
        Ok(())
    }

    fn remove_http_frontend(&mut self, front: &HttpFrontend) -> anyhow::Result<()> {
        if self.http_fronts.remove(&front.route_key()?).is_none() {
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
        let fingerprint = Fingerprint {
            inner: calculate_fingerprint(add.certificate.certificate.as_bytes())
                .with_context(|| "cannot calculate the certificate's fingerprint")?,
        };

        let entry = self
            .certificates
            .entry(add.address)
            .or_insert_with(Certificates::new);

        if entry.map.contains_key(&fingerprint) {
            info!("Skip loading of certificate '{}' for domain '{}' on listener '{}', the certificate is already present.", fingerprint, add.certificate.names.join(", "), add.address);
            return Ok(());
        }

        entry.map.insert(fingerprint, add.certificate.clone());
        Ok(())
    }

    fn remove_certificate(&mut self, remove: &RemoveCertificate) -> anyhow::Result<()> {
        if let Some(certificates) = self.certificates.get_mut(&remove.address) {
            certificates.map.remove(&remove.fingerprint);
        }

        Ok(())
    }

    /// - Remove old certificate from certificates, using the old fingerprint
    /// - calculate the new fingerprint
    /// - insert the new certificate with the new fingerprint as key
    /// - check that the new entry is present in the certificates hashmap
    fn replace_certificate(&mut self, replace: &ReplaceCertificate) -> anyhow::Result<()> {
        self.certificates
            .get_mut(&replace.address)
            .with_context(|| format!("No certificate to replace for address {}", replace.address))?
            .map
            .remove(&replace.old_fingerprint);

        let new_fingerprint = Fingerprint {
            inner: calculate_fingerprint(replace.new_certificate.certificate.as_bytes())
                .with_context(|| "cannot obtain the certificate's fingerprint")?,
        };

        self.certificates.get_mut(&replace.address).map(|certs| {
            certs
                .map
                .insert(new_fingerprint.clone(), replace.new_certificate.clone())
        });

        if !self
            .certificates
            .get(&replace.address)
            .with_context(|| {
                "Unlikely error. This entry in the certificate hashmap should be present"
            })?
            .map
            .contains_key(&new_fingerprint)
        {
            bail!(format!(
                "Failed to insert the new certificate for address {}",
                replace.address
            ))
        }
        Ok(())
    }

    fn add_tcp_frontend(&mut self, front: &TcpFrontend) -> anyhow::Result<()> {
        let tcp_frontends = self
            .tcp_fronts
            .entry(front.cluster_id.clone())
            .or_insert_with(TcpFrontends::new);
        if tcp_frontends.vec.contains(front) {
            bail!("This tcp frontend is already present: {:?}", front);
        }
        tcp_frontends.vec.push(front.clone());
        Ok(())
    }

    fn remove_tcp_frontend(&mut self, front_to_remove: &TcpFrontend) -> anyhow::Result<()> {
        let tcp_frontends = self
            .tcp_fronts
            .get_mut(&front_to_remove.cluster_id)
            .with_context(|| {
                format!(
                    "cluster {} has no frontends at {} (custom tags: {:?})",
                    front_to_remove.cluster_id, front_to_remove.address, front_to_remove.tags
                )
            })?;

        let len = tcp_frontends.vec.len();
        tcp_frontends
            .vec
            .retain(|front| front.address != front_to_remove.address);
        if tcp_frontends.vec.len() == len {
            bail!("Removed no frontend");
        }
        Ok(())
    }

    fn add_backend(&mut self, backend: &Backend) -> anyhow::Result<()> {
        let backends = self
            .backends
            .entry(backend.cluster_id.clone())
            .or_insert_with(Backends::new);

        // we might be modifying the sticky id or load balancing parameters
        backends
            .vec
            .retain(|b| b.backend_id != backend.backend_id || b.address != backend.address);
        backends.vec.push(backend.clone());
        backends.vec.sort();

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

        let len = backend_list.vec.len();
        backend_list
            .vec
            .retain(|b| b.backend_id != backend.backend_id || b.address != backend.address);
        backend_list.vec.sort();
        if backend_list.vec.len() == len {
            bail!("Removed no backend");
        }
        Ok(())
    }

    pub fn generate_orders(&self) -> Vec<WorkerOrder> {
        let mut v = Vec::new();

        for listener in self.http_listeners.values() {
            v.push(WorkerOrder::AddHttpListener(listener.clone()));
            if listener.activated {
                v.push(WorkerOrder::ActivateListener(ActivateListener {
                    address: listener.address,
                    proxy: ListenerType::HTTP,
                    from_scm: false,
                }));
            }
        }

        for listener in self.https_listeners.values() {
            v.push(WorkerOrder::AddHttpsListener(listener.clone()));
            if listener.activated {
                v.push(WorkerOrder::ActivateListener(ActivateListener {
                    address: listener.address,
                    proxy: ListenerType::HTTPS,
                    from_scm: false,
                }));
            }
        }

        for listener in self.tcp_listeners.values() {
            v.push(WorkerOrder::AddTcpListener(listener.clone()));
            if listener.activated {
                v.push(WorkerOrder::ActivateListener(ActivateListener {
                    address: listener.address,
                    proxy: ListenerType::TCP,
                    from_scm: false,
                }));
            }
        }

        for cluster in self.clusters.values() {
            v.push(WorkerOrder::AddCluster(cluster.clone()));
        }

        for front in self.http_fronts.values() {
            v.push(WorkerOrder::AddHttpFrontend(front.clone()));
        }

        for (front, certs) in self.certificates.iter() {
            for certificate in certs.map.values() {
                v.push(WorkerOrder::AddCertificate(AddCertificate {
                    address: *front,
                    certificate: certificate.clone(),
                    expired_at: None,
                }));
            }
        }

        for front in self.https_fronts.values() {
            v.push(WorkerOrder::AddHttpsFrontend(front.clone()));
        }

        for front_list in self.tcp_fronts.values() {
            for front in &front_list.vec {
                v.push(WorkerOrder::AddTcpFrontend(front.clone()));
            }
        }

        for backend_list in self.backends.values() {
            for backend in &backend_list.vec {
                v.push(WorkerOrder::AddBackend(backend.clone()));
            }
        }

        v
    }

    pub fn generate_activate_orders(&self) -> Vec<WorkerOrder> {
        let mut v = Vec::new();
        for front in self
            .http_listeners
            .iter()
            .filter(|(_, listener)| listener.activated)
            .map(|(k, _)| k)
        {
            v.push(WorkerOrder::ActivateListener(ActivateListener {
                address: *front,
                proxy: ListenerType::HTTP,
                from_scm: false,
            }));
        }

        for front in self
            .https_listeners
            .iter()
            .filter(|(_, listener)| listener.activated)
            .map(|(k, _)| k)
        {
            v.push(WorkerOrder::ActivateListener(ActivateListener {
                address: *front,
                proxy: ListenerType::HTTPS,
                from_scm: false,
            }));
        }
        for front in self
            .tcp_listeners
            .iter()
            .filter(|(_, listener)| listener.activated)
            .map(|(k, _)| k)
        {
            v.push(WorkerOrder::ActivateListener(ActivateListener {
                address: *front,
                proxy: ListenerType::TCP,
                from_scm: false,
            }));
        }

        v
    }

    pub fn diff(&self, other: &ConfigState) -> Vec<WorkerOrder> {
        //pub tcp_listeners:   HashMap<SocketAddr, (TcpListener, bool)>,
        let my_tcp_listeners: HashSet<&SocketAddr> = self.tcp_listeners.keys().collect();
        let their_tcp_listeners: HashSet<&SocketAddr> = other.tcp_listeners.keys().collect();
        let removed_tcp_listeners = my_tcp_listeners.difference(&their_tcp_listeners);
        let added_tcp_listeners = their_tcp_listeners.difference(&my_tcp_listeners);

        let my_http_listeners: HashSet<&SocketAddr> = self.http_listeners.keys().collect();
        let their_http_listeners: HashSet<&SocketAddr> = other.http_listeners.keys().collect();
        let removed_http_listeners = my_http_listeners.difference(&their_http_listeners);
        let added_http_listeners = their_http_listeners.difference(&my_http_listeners);

        let my_https_listeners: HashSet<&SocketAddr> = self.https_listeners.keys().collect();
        let their_https_listeners: HashSet<&SocketAddr> = other.https_listeners.keys().collect();
        let removed_https_listeners = my_https_listeners.difference(&their_https_listeners);
        let added_https_listeners = their_https_listeners.difference(&my_https_listeners);

        let mut diff_orders = vec![];

        for address in removed_tcp_listeners {
            if self.tcp_listeners[address].activated {
                diff_orders.push(WorkerOrder::DeactivateListener(DeactivateListener {
                    address: **address,
                    proxy: ListenerType::TCP,
                    to_scm: false,
                }));
            }

            diff_orders.push(WorkerOrder::RemoveListener(RemoveListener {
                address: **address,
                proxy: ListenerType::TCP,
            }));
        }

        for address in added_tcp_listeners.clone() {
            diff_orders.push(WorkerOrder::AddTcpListener(
                other.tcp_listeners[address].clone(),
            ));

            if other.tcp_listeners[address].activated {
                diff_orders.push(WorkerOrder::ActivateListener(ActivateListener {
                    address: **address,
                    proxy: ListenerType::TCP,
                    from_scm: false,
                }));
            }
        }

        for address in removed_http_listeners {
            if self.http_listeners[address].activated {
                diff_orders.push(WorkerOrder::DeactivateListener(DeactivateListener {
                    address: **address,
                    proxy: ListenerType::HTTP,
                    to_scm: false,
                }));
            }

            diff_orders.push(WorkerOrder::RemoveListener(RemoveListener {
                address: **address,
                proxy: ListenerType::HTTP,
            }));
        }

        for address in added_http_listeners.clone() {
            diff_orders.push(WorkerOrder::AddHttpListener(
                other.http_listeners[address].clone(),
            ));

            if other.http_listeners[address].activated {
                diff_orders.push(WorkerOrder::ActivateListener(ActivateListener {
                    address: **address,
                    proxy: ListenerType::HTTP,
                    from_scm: false,
                }));
            }
        }

        for address in removed_https_listeners {
            if self.https_listeners[address].activated {
                diff_orders.push(WorkerOrder::DeactivateListener(DeactivateListener {
                    address: **address,
                    proxy: ListenerType::HTTPS,
                    to_scm: false,
                }));
            }

            diff_orders.push(WorkerOrder::RemoveListener(RemoveListener {
                address: **address,
                proxy: ListenerType::HTTPS,
            }));
        }

        for address in added_https_listeners.clone() {
            diff_orders.push(WorkerOrder::AddHttpsListener(
                other.https_listeners[address].clone(),
            ));

            if other.https_listeners[address].activated {
                diff_orders.push(WorkerOrder::ActivateListener(ActivateListener {
                    address: **address,
                    proxy: ListenerType::HTTPS,
                    from_scm: false,
                }));
            }
        }

        for addr in my_tcp_listeners.intersection(&their_tcp_listeners) {
            let my_listener = &self.tcp_listeners[addr];
            let their_listener = &other.tcp_listeners[addr];

            if my_listener != their_listener {
                diff_orders.push(WorkerOrder::RemoveListener(RemoveListener {
                    address: **addr,
                    proxy: ListenerType::TCP,
                }));

                diff_orders.push(WorkerOrder::AddTcpListener(TcpListenerConfig {
                    activated: false,
                    ..their_listener.clone()
                }));
            }

            if my_listener.activated && !their_listener.activated {
                diff_orders.push(WorkerOrder::DeactivateListener(DeactivateListener {
                    address: **addr,
                    proxy: ListenerType::TCP,
                    to_scm: false,
                }));
            }

            if !my_listener.activated && their_listener.activated {
                diff_orders.push(WorkerOrder::ActivateListener(ActivateListener {
                    address: **addr,
                    proxy: ListenerType::TCP,
                    from_scm: false,
                }));
            }
        }

        for addr in my_http_listeners.intersection(&their_http_listeners) {
            let my_listener = &self.http_listeners[addr];
            let their_listener = other.http_listeners[addr].clone();

            if my_listener != &their_listener {
                diff_orders.push(WorkerOrder::RemoveListener(RemoveListener {
                    address: **addr,
                    proxy: ListenerType::HTTP,
                }));

                diff_orders.push(WorkerOrder::AddHttpListener(HttpListenerConfig {
                    activated: false, // make sure the AddListener order has an unactivated config
                    ..their_listener.clone()
                }))
            }

            if my_listener.activated && !their_listener.activated {
                diff_orders.push(WorkerOrder::DeactivateListener(DeactivateListener {
                    address: **addr,
                    proxy: ListenerType::HTTP,
                    to_scm: false,
                }));
            }

            if !my_listener.activated && their_listener.activated {
                diff_orders.push(WorkerOrder::ActivateListener(ActivateListener {
                    address: **addr,
                    proxy: ListenerType::HTTP,
                    from_scm: false,
                }));
            }
        }

        for addr in my_https_listeners.intersection(&their_https_listeners) {
            let my_listener = &self.https_listeners[addr];
            let their_listener = other.https_listeners[addr].clone();

            if my_listener != &their_listener {
                diff_orders.push(WorkerOrder::RemoveListener(RemoveListener {
                    address: **addr,
                    proxy: ListenerType::HTTPS,
                }));

                diff_orders.push(WorkerOrder::AddHttpsListener(HttpsListenerConfig {
                    activated: false, // make sure the AddListener order has an unactivated config
                    ..their_listener.clone()
                }));
            }

            if my_listener.activated && !their_listener.activated {
                diff_orders.push(WorkerOrder::DeactivateListener(DeactivateListener {
                    address: **addr,
                    proxy: ListenerType::HTTPS,
                    to_scm: false,
                }));
            }

            if !my_listener.activated && their_listener.activated {
                diff_orders.push(WorkerOrder::ActivateListener(ActivateListener {
                    address: **addr,
                    proxy: ListenerType::HTTPS,
                    from_scm: false,
                }));
            }
        }

        for (cluster_id, res) in diff_map(self.clusters.iter(), other.clusters.iter()) {
            match res {
                DiffResult::Added | DiffResult::Changed => diff_orders.push(
                    WorkerOrder::AddCluster(other.clusters.get(cluster_id).unwrap().clone()),
                ),
                DiffResult::Removed => diff_orders.push(WorkerOrder::RemoveCluster {
                    cluster_id: cluster_id.to_string(),
                }),
            }
        }

        for ((cluster_id, backend_id), res) in diff_map(
            self.backends.iter().flat_map(|(cluster_id, backends)| {
                backends
                    .vec
                    .iter()
                    .map(move |backend| ((cluster_id, &backend.backend_id), backend))
            }),
            other.backends.iter().flat_map(|(cluster_id, backends)| {
                backends
                    .vec
                    .iter()
                    .map(move |backend| ((cluster_id, &backend.backend_id), backend))
            }),
        ) {
            match res {
                DiffResult::Added => {
                    let backend = other
                        .backends
                        .get(cluster_id)
                        .and_then(|b| b.vec.iter().find(|b| &b.backend_id == backend_id))
                        .unwrap();
                    diff_orders.push(WorkerOrder::AddBackend(backend.clone()));
                }
                DiffResult::Removed => {
                    let backend = self
                        .backends
                        .get(cluster_id)
                        .and_then(|b| b.vec.iter().find(|b| &b.backend_id == backend_id))
                        .unwrap();

                    diff_orders.push(WorkerOrder::RemoveBackend(RemoveBackend {
                        cluster_id: backend.cluster_id.clone(),
                        backend_id: backend.backend_id.clone(),
                        address: backend.address,
                    }));
                }
                DiffResult::Changed => {
                    let backend = self
                        .backends
                        .get(cluster_id)
                        .and_then(|b| b.vec.iter().find(|b| &b.backend_id == backend_id))
                        .unwrap();

                    diff_orders.push(WorkerOrder::RemoveBackend(RemoveBackend {
                        cluster_id: backend.cluster_id.clone(),
                        backend_id: backend.backend_id.clone(),
                        address: backend.address,
                    }));

                    let backend = other
                        .backends
                        .get(cluster_id)
                        .and_then(|b| b.vec.iter().find(|b| &b.backend_id == backend_id))
                        .unwrap();
                    diff_orders.push(WorkerOrder::AddBackend(backend.clone()));
                }
            }
        }

        let mut my_http_fronts: HashSet<(&String, &HttpFrontend)> = HashSet::new();
        for (route, front) in self.http_fronts.iter() {
            my_http_fronts.insert((route, front));
        }
        let mut their_http_fronts: HashSet<(&String, &HttpFrontend)> = HashSet::new();
        for (route, front) in other.http_fronts.iter() {
            their_http_fronts.insert((route, front));
        }

        let removed_http_fronts = my_http_fronts.difference(&their_http_fronts);
        let added_http_fronts = their_http_fronts.difference(&my_http_fronts);

        for &(_, front) in removed_http_fronts {
            diff_orders.push(WorkerOrder::RemoveHttpFrontend(front.clone()));
        }

        for &(_, front) in added_http_fronts {
            diff_orders.push(WorkerOrder::AddHttpFrontend(front.clone()));
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
            diff_orders.push(WorkerOrder::RemoveHttpsFrontend(front.clone()));
        }

        for &(_, front) in added_https_fronts {
            diff_orders.push(WorkerOrder::AddHttpsFrontend(front.clone()));
        }

        let mut my_tcp_fronts: HashSet<(&ClusterId, &TcpFrontend)> = HashSet::new();
        for (cluster_id, front_list) in self.tcp_fronts.iter() {
            for front in &front_list.vec {
                my_tcp_fronts.insert((cluster_id, front));
            }
        }
        let mut their_tcp_fronts: HashSet<(&ClusterId, &TcpFrontend)> = HashSet::new();
        for (cluster_id, front_list) in other.tcp_fronts.iter() {
            for front in &front_list.vec {
                their_tcp_fronts.insert((cluster_id, front));
            }
        }

        let removed_tcp_fronts = my_tcp_fronts.difference(&their_tcp_fronts);
        let added_tcp_fronts = their_tcp_fronts.difference(&my_tcp_fronts);

        for &(_, front) in removed_tcp_fronts {
            diff_orders.push(WorkerOrder::RemoveTcpFrontend(front.clone()));
        }

        for &(_, front) in added_tcp_fronts {
            diff_orders.push(WorkerOrder::AddTcpFrontend(front.clone()));
        }

        //pub certificates:    HashMap<SocketAddr, HashMap<CertificateFingerprint, (CertificateAndKey, Vec<String>)>>,
        let my_certificates: HashSet<(SocketAddr, &Fingerprint)> = HashSet::from_iter(
            self.certificates
                .iter()
                .flat_map(|(addr, certs)| repeat(*addr).zip(certs.map.keys())),
        );
        let their_certificates: HashSet<(SocketAddr, &Fingerprint)> = HashSet::from_iter(
            other
                .certificates
                .iter()
                .flat_map(|(addr, certs)| repeat(*addr).zip(certs.map.keys())),
        );

        let removed_certificates = my_certificates.difference(&their_certificates);
        let added_certificates = their_certificates.difference(&my_certificates);

        for &(address, fingerprint) in removed_certificates {
            diff_orders.push(WorkerOrder::RemoveCertificate(RemoveCertificate {
                address,
                fingerprint: fingerprint.clone(),
            }));
        }

        for &(address, fingerprint) in added_certificates {
            if let Some(certificate) = other
                .certificates
                .get(&address)
                .and_then(|certs| certs.map.get(fingerprint))
            {
                diff_orders.push(WorkerOrder::AddCertificate(AddCertificate {
                    address,
                    certificate: certificate.clone(),
                    expired_at: None,
                }));
            }
        }

        for address in added_tcp_listeners {
            let listener = &other.tcp_listeners[address];
            if listener.activated {
                diff_orders.push(WorkerOrder::ActivateListener(ActivateListener {
                    address: listener.address,
                    proxy: ListenerType::TCP,
                    from_scm: false,
                }));
            }
        }

        diff_orders
    }

    // FIXME: what about deny rules?
    pub fn hash_state(&self) -> Vec<ClusterHash> {
        let mut h: HashMap<_, _> = self
            .clusters
            .keys()
            .map(|cluster_id| {
                let mut s = DefaultHasher::new();
                self.clusters.get(cluster_id).hash(&mut s);
                if let Some(backends) = self.backends.get(cluster_id) {
                    backends.vec.iter().collect::<BTreeSet<_>>().hash(&mut s)
                }
                if let Some(tcp_fronts) = self.tcp_fronts.get(cluster_id) {
                    tcp_fronts.vec.iter().collect::<BTreeSet<_>>().hash(&mut s)
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
            .map(|(cluster_id, hasher)| ClusterHash {
                cluster_id,
                hash: hasher.finish(),
            })
            .collect()
    }

    pub fn cluster_state(&self, cluster_id: &str) -> ClusterInformation {
        ClusterInformation {
            configuration: self.clusters.get(cluster_id).cloned(),
            http_frontends: self
                .http_fronts
                .iter()
                .filter_map(|(_k, v)| match &v.cluster_id {
                    None => None,
                    Some(id) => {
                        if id == cluster_id {
                            Some(v)
                        } else {
                            None
                        }
                    }
                })
                .cloned()
                .collect(),
            https_frontends: self
                .https_fronts
                .iter()
                .filter_map(|(_k, v)| match &v.cluster_id {
                    None => None,
                    Some(id) => {
                        if id == cluster_id {
                            Some(v)
                        } else {
                            None
                        }
                    }
                })
                .cloned()
                .collect(),
            tcp_frontends: self
                .tcp_fronts
                .get(cluster_id)
                .cloned()
                .unwrap_or_default()
                .vec,
            backends: self
                .backends
                .get(cluster_id)
                .cloned()
                .unwrap_or_default()
                .vec,
        }
    }

    /// yield the current number of clusters in the state
    pub fn count_clusters(&self) -> usize {
        self.clusters.len()
    }

    pub fn count_backends(&self) -> usize {
        self.backends
            .values()
            .fold(0, |acc, backends| acc + backends.vec.len())
    }

    pub fn count_frontends(&self) -> usize {
        self.http_fronts.values().count()
            + self.https_fronts.values().count()
            + self.tcp_fronts.values().fold(0, |acc, t| acc + t.vec.len())
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
                    .push(http_frontend.1.to_owned());
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
                    .push(https_frontend.1.to_owned());
            }
        }

        if (filters.tcp || list_all) && filters.domain.is_none() {
            for tcp_frontend in self
                .tcp_fronts
                .values()
                .flat_map(|fronts| fronts.vec.iter())
            {
                listed_frontends.tcp_frontends.push(tcp_frontend.to_owned())
            }
        }
        listed_frontends
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

pub fn get_cluster_ids_by_domain(
    state: &ConfigState,
    hostname: String,
    path: Option<String>,
) -> HashSet<ClusterId> {
    let mut cluster_ids: HashSet<ClusterId> = HashSet::new();

    state.http_fronts.values().for_each(|front| {
        if domain_check(&front.hostname, &front.path, &hostname, &path) {
            if let Some(id) = &front.cluster_id {
                cluster_ids.insert(id.to_string());
            }
        }
    });

    state.https_fronts.values().for_each(|front| {
        if domain_check(&front.hostname, &front.path, &hostname, &path) {
            if let Some(id) = &front.cluster_id {
                cluster_ids.insert(id.to_string());
            }
        }
    });

    cluster_ids
}

/// returns the certificate and the list of names
pub fn get_certificate(state: &ConfigState, fingerprint: &[u8]) -> Option<CertificateWithNames> {
    state
        .certificates
        .values()
        .filter_map(|certs| {
            certs.map.get(&Fingerprint {
                inner: fingerprint.to_vec(),
            })
        })
        .map(|cert| CertificateWithNames {
            certificate: cert.certificate.clone(),
            names: cert.names.clone(),
        })
        .next()
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
    use crate::worker::{
        Backend, HttpFrontend, LoadBalancingAlgorithms, LoadBalancingParams, PathRule,
        RulePosition, WorkerOrder,
    };

    #[test]
    fn serialize() {
        let mut state: ConfigState = Default::default();
        state
            .dispatch(&WorkerOrder::AddHttpFrontend(HttpFrontend {
                cluster_id: Some(String::from("cluster_1")),
                hostname: String::from("lolcatho.st:8080"),
                path: PathRule::prefix("/"),
                method: None,
                address: "0.0.0.0:8080".parse().unwrap(),
                position: RulePosition::Tree,
                tags: None,
            }))
            .expect("Could not execute order");
        state
            .dispatch(&WorkerOrder::AddHttpFrontend(HttpFrontend {
                cluster_id: Some(String::from("cluster_2")),
                hostname: String::from("test.local"),
                path: PathRule::prefix("/abc"),
                method: None,
                address: "0.0.0.0:8080".parse().unwrap(),
                position: RulePosition::Pre,
                tags: None,
            }))
            .expect("Could not execute order");
        state
            .dispatch(&WorkerOrder::AddBackend(Backend {
                cluster_id: String::from("cluster_1"),
                backend_id: String::from("cluster_1-0"),
                address: "127.0.0.1:1026".parse().unwrap(),
                load_balancing_parameters: Some(LoadBalancingParams::default()),
                sticky_id: None,
                backup: None,
            }))
            .expect("Could not execute order");
        state
            .dispatch(&WorkerOrder::AddBackend(Backend {
                cluster_id: String::from("cluster_1"),
                backend_id: String::from("cluster_1-1"),
                address: "127.0.0.2:1027".parse().unwrap(),
                load_balancing_parameters: Some(LoadBalancingParams::default()),
                sticky_id: None,
                backup: None,
            }))
            .expect("Could not execute order");
        state
            .dispatch(&WorkerOrder::AddBackend(Backend {
                cluster_id: String::from("cluster_2"),
                backend_id: String::from("cluster_2-0"),
                address: "192.167.1.2:1026".parse().unwrap(),
                load_balancing_parameters: Some(LoadBalancingParams::default()),
                sticky_id: None,
                backup: None,
            }))
            .expect("Could not execute order");
        state
            .dispatch(&WorkerOrder::AddBackend(Backend {
                cluster_id: String::from("cluster_1"),
                backend_id: String::from("cluster_1-3"),
                address: "192.168.1.3:1027".parse().unwrap(),
                load_balancing_parameters: Some(LoadBalancingParams::default()),
                sticky_id: None,
                backup: None,
            }))
            .expect("Could not execute order");
        state
            .dispatch(&WorkerOrder::RemoveBackend(RemoveBackend {
                cluster_id: String::from("cluster_1"),
                backend_id: String::from("cluster_1-3"),
                address: "192.168.1.3:1027".parse().unwrap(),
            }))
            .expect("Could not execute order");

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
            .dispatch(&WorkerOrder::AddHttpFrontend(HttpFrontend {
                cluster_id: Some(String::from("cluster_1")),
                hostname: String::from("lolcatho.st:8080"),
                path: PathRule::prefix("/"),
                method: None,
                address: "0.0.0.0:8080".parse().unwrap(),
                position: RulePosition::Post,
                tags: None,
            }))
            .expect("Could not execute order");
        state
            .dispatch(&WorkerOrder::AddHttpFrontend(HttpFrontend {
                cluster_id: Some(String::from("cluster_2")),
                hostname: String::from("test.local"),
                path: PathRule::prefix("/abc"),
                method: None,
                address: "0.0.0.0:8080".parse().unwrap(),
                position: RulePosition::Tree,
                tags: None,
            }))
            .expect("Could not execute order");
        state
            .dispatch(&WorkerOrder::AddBackend(Backend {
                cluster_id: String::from("cluster_1"),
                backend_id: String::from("cluster_1-0"),
                address: "127.0.0.1:1026".parse().unwrap(),
                load_balancing_parameters: Some(LoadBalancingParams::default()),
                sticky_id: None,
                backup: None,
            }))
            .expect("Could not execute order");
        state
            .dispatch(&WorkerOrder::AddBackend(Backend {
                cluster_id: String::from("cluster_1"),
                backend_id: String::from("cluster_1-1"),
                address: "127.0.0.2:1027".parse().unwrap(),
                load_balancing_parameters: Some(LoadBalancingParams::default()),
                sticky_id: None,
                backup: None,
            }))
            .expect("Could not execute order");
        state
            .dispatch(&WorkerOrder::AddBackend(Backend {
                cluster_id: String::from("cluster_2"),
                backend_id: String::from("cluster_2-0"),
                address: "192.167.1.2:1026".parse().unwrap(),
                load_balancing_parameters: Some(LoadBalancingParams::default()),
                sticky_id: None,
                backup: None,
            }))
            .expect("Could not execute order");
        state
            .dispatch(&WorkerOrder::AddCluster(Cluster {
                cluster_id: String::from("cluster_2"),
                sticky_session: true,
                https_redirect: true,
                proxy_protocol: None,
                load_balancing: LoadBalancingAlgorithms::RoundRobin,
                load_metric: None,
                answer_503: None,
            }))
            .expect("Could not execute order");

        let mut state2: ConfigState = Default::default();
        state2
            .dispatch(&WorkerOrder::AddHttpFrontend(HttpFrontend {
                cluster_id: Some(String::from("cluster_1")),
                hostname: String::from("lolcatho.st:8080"),
                path: PathRule::prefix("/"),
                address: "0.0.0.0:8080".parse().unwrap(),
                method: None,
                position: RulePosition::Post,
                tags: None,
            }))
            .expect("Could not execute order");
        state2
            .dispatch(&WorkerOrder::AddBackend(Backend {
                cluster_id: String::from("cluster_1"),
                backend_id: String::from("cluster_1-0"),
                address: "127.0.0.1:1026".parse().unwrap(),
                load_balancing_parameters: Some(LoadBalancingParams::default()),
                sticky_id: None,
                backup: None,
            }))
            .expect("Could not execute order");
        state2
            .dispatch(&WorkerOrder::AddBackend(Backend {
                cluster_id: String::from("cluster_1"),
                backend_id: String::from("cluster_1-1"),
                address: "127.0.0.2:1027".parse().unwrap(),
                load_balancing_parameters: Some(LoadBalancingParams::default()),
                sticky_id: None,
                backup: None,
            }))
            .expect("Could not execute order");
        state2
            .dispatch(&WorkerOrder::AddBackend(Backend {
                cluster_id: String::from("cluster_1"),
                backend_id: String::from("cluster_1-2"),
                address: "127.0.0.2:1028".parse().unwrap(),
                load_balancing_parameters: Some(LoadBalancingParams::default()),
                sticky_id: None,
                backup: None,
            }))
            .expect("Could not execute order");
        state2
            .dispatch(&WorkerOrder::AddCluster(Cluster {
                cluster_id: String::from("cluster_3"),
                sticky_session: false,
                https_redirect: false,
                proxy_protocol: None,
                load_balancing: LoadBalancingAlgorithms::RoundRobin,
                load_metric: None,
                answer_503: None,
            }))
            .expect("Could not execute order");

        let e = vec![
            WorkerOrder::RemoveHttpFrontend(HttpFrontend {
                cluster_id: Some(String::from("cluster_2")),
                hostname: String::from("test.local"),
                path: PathRule::prefix("/abc"),
                method: None,
                address: "0.0.0.0:8080".parse().unwrap(),
                position: RulePosition::Tree,
                tags: None,
            }),
            WorkerOrder::RemoveBackend(RemoveBackend {
                cluster_id: String::from("cluster_2"),
                backend_id: String::from("cluster_2-0"),
                address: "192.167.1.2:1026".parse().unwrap(),
            }),
            WorkerOrder::AddBackend(Backend {
                cluster_id: String::from("cluster_1"),
                backend_id: String::from("cluster_1-2"),
                address: "127.0.0.2:1028".parse().unwrap(),
                load_balancing_parameters: Some(LoadBalancingParams::default()),
                sticky_id: None,
                backup: None,
            }),
            WorkerOrder::RemoveCluster {
                cluster_id: String::from("cluster_2"),
            },
            WorkerOrder::AddCluster(Cluster {
                cluster_id: String::from("cluster_3"),
                sticky_session: false,
                https_redirect: false,
                proxy_protocol: None,
                load_balancing: LoadBalancingAlgorithms::RoundRobin,
                load_metric: None,
                answer_503: None,
            }),
        ];
        let expected_diff: HashSet<&WorkerOrder> = HashSet::from_iter(e.iter());

        let d = state.diff(&state2);
        let diff = HashSet::from_iter(d.iter());
        println!("diff orders:\n{diff:#?}\n");
        println!("expected diff orders:\n{expected_diff:#?}\n");

        let hash1 = state.hash_state();
        let hash2 = state2.hash_state();
        let mut state3 = state.clone();
        state3
            .dispatch(&WorkerOrder::AddBackend(Backend {
                cluster_id: String::from("cluster_1"),
                backend_id: String::from("cluster_1-2"),
                address: "127.0.0.2:1028".parse().unwrap(),
                load_balancing_parameters: Some(LoadBalancingParams::default()),
                sticky_id: None,
                backup: None,
            }))
            .expect("Could not execute order");
        let hash3 = state3.hash_state();
        println!("state 1 hashes: {hash1:#?}");
        println!("state 2 hashes: {hash2:#?}");
        println!("state 3 hashes: {hash3:#?}");

        assert_eq!(diff, expected_diff);
    }

    #[test]
    fn cluster_ids_by_domain() {
        let mut config = ConfigState::new();
        let http_front_cluster1 = HttpFrontend {
            cluster_id: Some(String::from("MyCluster_1")),
            hostname: String::from("lolcatho.st"),
            path: PathRule::prefix(""),
            method: None,
            address: "0.0.0.0:8080".parse().unwrap(),
            position: RulePosition::Tree,
            tags: None,
        };

        let https_front_cluster1 = HttpFrontend {
            cluster_id: Some(String::from("MyCluster_1")),
            hostname: String::from("lolcatho.st"),
            path: PathRule::prefix(""),
            method: None,
            address: "0.0.0.0:8443".parse().unwrap(),
            position: RulePosition::Tree,
            tags: None,
        };

        let http_front_cluster2 = HttpFrontend {
            cluster_id: Some(String::from("MyCluster_2")),
            hostname: String::from("lolcatho.st"),
            path: PathRule::prefix("/api"),
            method: None,
            address: "0.0.0.0:8080".parse().unwrap(),
            position: RulePosition::Tree,
            tags: None,
        };

        let https_front_cluster2 = HttpFrontend {
            cluster_id: Some(String::from("MyCluster_2")),
            hostname: String::from("lolcatho.st"),
            path: PathRule::prefix("/api"),
            method: None,
            address: "0.0.0.0:8443".parse().unwrap(),
            position: RulePosition::Tree,
            tags: None,
        };

        let add_http_front_order_cluster1 = WorkerOrder::AddHttpFrontend(http_front_cluster1);
        let add_http_front_order_cluster2 = WorkerOrder::AddHttpFrontend(http_front_cluster2);
        let add_https_front_order_cluster1 = WorkerOrder::AddHttpsFrontend(https_front_cluster1);
        let add_https_front_order_cluster2 = WorkerOrder::AddHttpsFrontend(https_front_cluster2);
        config
            .dispatch(&add_http_front_order_cluster1)
            .expect("Could not execute order");
        config
            .dispatch(&add_http_front_order_cluster2)
            .expect("Could not execute order");
        config
            .dispatch(&add_https_front_order_cluster1)
            .expect("Could not execute order");
        config
            .dispatch(&add_https_front_order_cluster2)
            .expect("Could not execute order");

        let mut cluster1_cluster2: HashSet<ClusterId> = HashSet::new();
        cluster1_cluster2.insert(String::from("MyCluster_1"));
        cluster1_cluster2.insert(String::from("MyCluster_2"));

        let mut cluster2: HashSet<ClusterId> = HashSet::new();
        cluster2.insert(String::from("MyCluster_2"));

        let empty: HashSet<ClusterId> = HashSet::new();
        assert_eq!(
            get_cluster_ids_by_domain(&config, String::from("lolcatho.st"), None),
            cluster1_cluster2
        );
        assert_eq!(
            get_cluster_ids_by_domain(
                &config,
                String::from("lolcatho.st"),
                Some(String::from("/api"))
            ),
            cluster2
        );
        assert_eq!(
            get_cluster_ids_by_domain(&config, String::from("lolcathost"), None),
            empty
        );
        assert_eq!(
            get_cluster_ids_by_domain(
                &config,
                String::from("lolcathost"),
                Some(String::from("/sozu"))
            ),
            empty
        );
    }

    #[test]
    fn duplicate_backends() {
        let mut state: ConfigState = Default::default();
        state
            .dispatch(&WorkerOrder::AddBackend(Backend {
                cluster_id: String::from("cluster_1"),
                backend_id: String::from("cluster_1-0"),
                address: "127.0.0.1:1026".parse().unwrap(),
                load_balancing_parameters: Some(LoadBalancingParams::default()),
                sticky_id: None,
                backup: None,
            }))
            .expect("Could not execute order");

        let b = Backend {
            cluster_id: String::from("cluster_1"),
            backend_id: String::from("cluster_1-0"),
            address: "127.0.0.1:1026".parse().unwrap(),
            load_balancing_parameters: Some(LoadBalancingParams::default()),
            sticky_id: Some("sticky".to_string()),
            backup: None,
        };

        state
            .dispatch(&WorkerOrder::AddBackend(b.clone()))
            .expect("Could not execute order");

        assert_eq!(state.backends.get("cluster_1").unwrap().vec, vec![b]);
    }

    #[test]
    fn listener_diff() {
        let mut state: ConfigState = Default::default();
        state
            .dispatch(&WorkerOrder::AddTcpListener(TcpListenerConfig {
                address: "0.0.0.0:1234".parse().unwrap(),
                ..Default::default()
            }))
            .expect("Could not execute order");
        state
            .dispatch(&WorkerOrder::ActivateListener(ActivateListener {
                address: "0.0.0.0:1234".parse().unwrap(),
                proxy: ListenerType::TCP,
                from_scm: false,
            }))
            .expect("Could not execute order");
        state
            .dispatch(&WorkerOrder::AddHttpListener(HttpListenerConfig {
                address: "0.0.0.0:8080".parse().unwrap(),
                answer_404: String::new(),
                answer_503: String::new(),
                sticky_name: String::new(),
                ..Default::default()
            }))
            .expect("Could not execute order");
        state
            .dispatch(&WorkerOrder::AddHttpsListener(HttpsListenerConfig {
                address: "0.0.0.0:8443".parse().unwrap(),
                answer_404: String::new(),
                answer_503: String::new(),
                sticky_name: String::new(),
                versions: vec![],
                cipher_list: vec![],
                cipher_suites: vec![],
                signature_algorithms: vec![],
                groups_list: vec![],
                ..Default::default()
            }))
            .expect("Could not execute order");
        state
            .dispatch(&WorkerOrder::ActivateListener(ActivateListener {
                address: "0.0.0.0:8443".parse().unwrap(),
                proxy: ListenerType::HTTPS,
                from_scm: false,
            }))
            .expect("Could not execute order");

        let mut state2: ConfigState = Default::default();
        state2
            .dispatch(&WorkerOrder::AddTcpListener(TcpListenerConfig {
                address: "0.0.0.0:1234".parse().unwrap(),
                expect_proxy: true, // the difference is here
                ..Default::default()
            }))
            .expect("Could not execute order");
        state2
            .dispatch(&WorkerOrder::AddHttpListener(HttpListenerConfig {
                address: "0.0.0.0:8080".parse().unwrap(),
                answer_404: "test".to_string(), // the difference is here
                answer_503: String::new(),
                sticky_name: String::new(),
                ..Default::default()
            }))
            .expect("Could not execute order");
        state2
            .dispatch(&WorkerOrder::ActivateListener(ActivateListener {
                address: "0.0.0.0:8080".parse().unwrap(),
                proxy: ListenerType::HTTP,
                from_scm: false,
            }))
            .expect("Could not execute order");
        state2
            .dispatch(&WorkerOrder::AddHttpsListener(HttpsListenerConfig {
                address: "0.0.0.0:8443".parse().unwrap(),
                answer_404: String::from("test"), // the difference is here
                answer_503: String::new(),
                sticky_name: String::new(),
                versions: vec![],
                cipher_list: vec![],
                cipher_suites: vec![],
                signature_algorithms: vec![],
                groups_list: vec![],
                ..Default::default()
            }))
            .expect("Could not execute order");
        state2
            .dispatch(&WorkerOrder::ActivateListener(ActivateListener {
                address: "0.0.0.0:8443".parse().unwrap(),
                proxy: ListenerType::HTTPS,
                from_scm: false,
            }))
            .expect("Could not execute order");

        let expected_diff = vec![
            WorkerOrder::RemoveListener(RemoveListener {
                address: "0.0.0.0:1234".parse().unwrap(),
                proxy: ListenerType::TCP,
            }),
            WorkerOrder::AddTcpListener(TcpListenerConfig {
                address: "0.0.0.0:1234".parse().unwrap(),
                expect_proxy: true,
                ..Default::default()
            }),
            WorkerOrder::DeactivateListener(DeactivateListener {
                address: "0.0.0.0:1234".parse().unwrap(),
                proxy: ListenerType::TCP,
                to_scm: false,
            }),
            WorkerOrder::RemoveListener(RemoveListener {
                address: "0.0.0.0:8080".parse().unwrap(),
                proxy: ListenerType::HTTP,
            }),
            WorkerOrder::AddHttpListener(HttpListenerConfig {
                address: "0.0.0.0:8080".parse().unwrap(),
                answer_404: String::from("test"),
                answer_503: String::new(),
                sticky_name: String::new(),
                ..Default::default()
            }),
            WorkerOrder::ActivateListener(ActivateListener {
                address: "0.0.0.0:8080".parse().unwrap(),
                proxy: ListenerType::HTTP,
                from_scm: false,
            }),
            WorkerOrder::RemoveListener(RemoveListener {
                address: "0.0.0.0:8443".parse().unwrap(),
                proxy: ListenerType::HTTPS,
            }),
            WorkerOrder::AddHttpsListener(HttpsListenerConfig {
                address: "0.0.0.0:8443".parse().unwrap(),
                answer_404: String::from("test"),
                answer_503: String::new(),
                sticky_name: String::new(),
                versions: vec![],
                cipher_list: vec![],
                cipher_suites: vec![],
                signature_algorithms: vec![],
                groups_list: vec![],
                ..Default::default()
            }),
        ];

        let found_diff = state.diff(&state2);
        //let diff: HashSet<&ProxyRequestOrder> = HashSet::from_iter(d.iter());
        println!("expected diff orders:\n{expected_diff:#?}\n");
        println!("found diff orders:\n{found_diff:#?}\n");

        let _hash1 = state.hash_state();
        let _hash2 = state2.hash_state();

        assert_eq!(found_diff, expected_diff);
    }
}

/// `RouteKey` is the routing key built from the following tuple.
/// Once serialized, it is used as a unique key in maps to identify a frontend
#[derive(PartialOrd, Ord, Debug, Clone, PartialEq, Eq, Hash)]
pub struct RouteKey {
    pub address: SocketAddr,
    hostname: String,
    pub path_rule: PathRule,
    pub method: Option<String>,
}

impl serde::Serialize for RouteKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut s = match &self.path_rule.kind {
            PathRuleKind::Prefix => format!(
                "{};{};P{}",
                self.address, self.hostname, self.path_rule.value
            ),
            PathRuleKind::Regex => format!(
                "{};{};R{}",
                self.address, self.hostname, self.path_rule.value
            ),
            PathRuleKind::Equals => format!(
                "{};{};={}",
                self.address, self.hostname, self.path_rule.value
            ),
        };

        if let Some(method) = &self.method {
            s = format!("{s};{method}");
        }

        serializer.serialize_str(&s)
    }
}

impl From<HttpFrontend> for RouteKey {
    fn from(frontend: HttpFrontend) -> Self {
        Self {
            address: frontend.address,
            hostname: frontend.hostname,
            path_rule: frontend.path,
            method: frontend.method,
        }
    }
}

impl From<&HttpFrontend> for RouteKey {
    fn from(frontend: &HttpFrontend) -> Self {
        Self {
            address: frontend.address,
            hostname: frontend.hostname.clone(),
            path_rule: frontend.path.clone(),
            method: frontend.method.clone(),
        }
    }
}

struct RouteKeyVisitor {}

impl<'de> Visitor<'de> for RouteKeyVisitor {
    type Value = RouteKey;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("parsing a route key")
    }

    fn visit_str<E>(self, value: &str) -> Result<RouteKey, E>
    where
        E: de::Error,
    {
        let mut it = value.split(';');
        let address = it
            .next()
            .ok_or_else(|| E::custom("invalid format".to_string()))
            .and_then(|s| {
                s.parse::<SocketAddr>()
                    .map_err(|e| E::custom(format!("could not deserialize SocketAddr: {e:?}")))
            })?;

        let hostname = it
            .next()
            .ok_or_else(|| E::custom("invalid format".to_string()))?;

        let path_rule_str = it
            .next()
            .ok_or_else(|| E::custom("invalid format".to_string()))?;

        let path_rule = match path_rule_str.chars().next() {
            Some('R') => PathRule::regex(&path_rule_str[1..]),
            Some('P') => PathRule::prefix(&path_rule_str[1..]),
            Some('=') => PathRule::equals(&path_rule_str[1..]),
            _ => return Err(E::custom("invalid path rule".to_string())),
        };

        let method = it.next().map(String::from);

        Ok(RouteKey {
            address,
            hostname: hostname.to_string(),
            path_rule,
            method,
        })
    }
}

impl<'de> serde::Deserialize<'de> for RouteKey {
    fn deserialize<D>(deserializer: D) -> Result<RouteKey, D::Error>
    where
        D: serde::de::Deserializer<'de>,
    {
        deserializer.deserialize_str(RouteKeyVisitor {})
    }
}
