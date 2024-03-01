use std::{cell::RefCell, collections::HashMap, net::SocketAddr, rc::Rc};

use mio::net::TcpStream;
use time::Duration;

use sozu_command::{
    proto::command::{Event, EventKind, LoadBalancingAlgorithms, LoadBalancingParams, LoadMetric},
    state::ClusterId,
};

use crate::{
    load_balancing::{LeastLoaded, LoadBalancingAlgorithm, PowerOfTwo, Random, RoundRobin},
    retry::{self, RetryPolicy},
    server::{self, push_event},
    PeakEWMA,
};

#[derive(thiserror::Error, Debug)]
pub enum BackendError {
    #[error("No backend found for cluster {0}")]
    NoBackendForCluster(String),
    #[error("Failed to connect to socket with MIO: {0}")]
    MioConnection(std::io::Error),
    #[error("This backend is not in a normal status: status={0:?}")]
    Status(BackendStatus),
    #[error(
        "could not connect {cluster_id} to {backend_address:?} ({failures} failures): {error}"
    )]
    ConnectionFailures {
        cluster_id: String,
        backend_address: SocketAddr,
        failures: usize,
        error: String,
    },
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum BackendStatus {
    Normal,
    Closing,
    Closed,
}

#[derive(Debug, PartialEq, Clone)]
pub struct Backend {
    pub sticky_id: Option<String>,
    pub backend_id: String,
    pub address: SocketAddr,
    pub status: BackendStatus,
    pub retry_policy: retry::RetryPolicyWrapper,
    pub active_connections: usize,
    pub active_requests: usize,
    pub failures: usize,
    pub load_balancing_parameters: Option<LoadBalancingParams>,
    pub backup: bool,
    pub connection_time: PeakEWMA,
}

impl Backend {
    pub fn new(
        backend_id: &str,
        address: SocketAddr,
        sticky_id: Option<String>,
        load_balancing_parameters: Option<LoadBalancingParams>,
        backup: Option<bool>,
    ) -> Backend {
        let desired_policy = retry::ExponentialBackoffPolicy::new(6);
        Backend {
            sticky_id,
            backend_id: backend_id.to_string(),
            address,
            status: BackendStatus::Normal,
            retry_policy: desired_policy.into(),
            active_connections: 0,
            active_requests: 0,
            failures: 0,
            load_balancing_parameters,
            backup: backup.unwrap_or(false),
            connection_time: PeakEWMA::new(),
        }
    }

    pub fn set_closing(&mut self) {
        self.status = BackendStatus::Closing;
    }

    pub fn retry_policy(&mut self) -> &mut retry::RetryPolicyWrapper {
        &mut self.retry_policy
    }

    pub fn can_open(&self) -> bool {
        if let Some(action) = self.retry_policy.can_try() {
            self.status == BackendStatus::Normal && action == retry::RetryAction::OKAY
        } else {
            false
        }
    }

    pub fn inc_connections(&mut self) -> Option<usize> {
        if self.status == BackendStatus::Normal {
            self.active_connections += 1;
            Some(self.active_connections)
        } else {
            None
        }
    }

    /// TODO: normalize with saturating_sub()
    pub fn dec_connections(&mut self) -> Option<usize> {
        match self.status {
            BackendStatus::Normal => {
                if self.active_connections > 0 {
                    self.active_connections -= 1;
                }
                Some(self.active_connections)
            }
            BackendStatus::Closed => None,
            BackendStatus::Closing => {
                if self.active_connections > 0 {
                    self.active_connections -= 1;
                }
                if self.active_connections == 0 {
                    self.status = BackendStatus::Closed;
                    None
                } else {
                    Some(self.active_connections)
                }
            }
        }
    }

    pub fn set_connection_time(&mut self, dur: Duration) {
        self.connection_time.observe(dur.whole_nanoseconds() as f64);
    }

    pub fn peak_ewma_connection(&mut self) -> f64 {
        self.connection_time.get(self.active_connections)
    }

    pub fn try_connect(&mut self) -> Result<mio::net::TcpStream, BackendError> {
        if self.status != BackendStatus::Normal {
            return Err(BackendError::Status(self.status.to_owned()));
        }

        match mio::net::TcpStream::connect(self.address) {
            Ok(tcp_stream) => {
                //self.retry_policy.succeed();
                self.inc_connections();
                Ok(tcp_stream)
            }
            Err(io_error) => {
                self.retry_policy.fail();
                self.failures += 1;
                // TODO: handle EINPROGRESS. It is difficult. It is discussed here:
                // https://docs.rs/mio/latest/mio/net/struct.TcpStream.html#method.connect
                // with an example code here:
                // https://github.com/Thomasdezeeuw/heph/blob/0c4f1ab3eaf08bea1d65776528bfd6114c9f8374/src/net/tcp/stream.rs#L560-L622
                Err(BackendError::MioConnection(io_error))
            }
        }
    }
}

// when a backend has been removed from configuration and the last connection to
// it has stopped, it will be dropped, so we can notify that the backend server
// can be safely stopped
impl std::ops::Drop for Backend {
    fn drop(&mut self) {
        server::push_event(Event {
            kind: EventKind::RemovedBackendHasNoConnections as i32,
            backend_id: Some(self.backend_id.clone()),
            address: Some(self.address.into()),
            cluster_id: None,
        });
    }
}

#[derive(Debug)]
pub struct BackendMap {
    pub backends: HashMap<ClusterId, BackendList>,
    pub max_failures: usize,
    pub available: bool,
}

impl Default for BackendMap {
    fn default() -> Self {
        Self::new()
    }
}

impl BackendMap {
    pub fn new() -> BackendMap {
        BackendMap {
            backends: HashMap::new(),
            max_failures: 3,
            available: true,
        }
    }

    pub fn import_configuration_state(
        &mut self,
        backends: &HashMap<ClusterId, Vec<sozu_command::response::Backend>>,
    ) {
        self.backends
            .extend(backends.iter().map(|(cluster_id, backend_vec)| {
                (
                    cluster_id.to_string(),
                    BackendList::import_configuration_state(backend_vec),
                )
            }));
    }

    pub fn add_backend(&mut self, cluster_id: &str, backend: Backend) {
        self.backends
            .entry(cluster_id.to_string())
            .or_default()
            .add_backend(backend);
    }

    // TODO: return <Result, BackendError>, log the error downstream
    pub fn remove_backend(&mut self, cluster_id: &str, backend_address: &SocketAddr) {
        if let Some(backends) = self.backends.get_mut(cluster_id) {
            backends.remove_backend(backend_address);
        } else {
            error!(
                "Backend was already removed: cluster id {}, address {:?}",
                cluster_id, backend_address
            );
        }
    }

    // TODO: return <Result, BackendError>, log the error downstream
    pub fn close_backend_connection(&mut self, cluster_id: &str, addr: &SocketAddr) {
        if let Some(cluster_backends) = self.backends.get_mut(cluster_id) {
            if let Some(ref mut backend) = cluster_backends.find_backend(addr) {
                backend.borrow_mut().dec_connections();
            }
        }
    }

    pub fn has_backend(&self, cluster_id: &str, backend: &Backend) -> bool {
        self.backends
            .get(cluster_id)
            .map(|backends| backends.has_backend(&backend.address))
            .unwrap_or(false)
    }

    pub fn backend_from_cluster_id(
        &mut self,
        cluster_id: &str,
    ) -> Result<(Rc<RefCell<Backend>>, TcpStream), BackendError> {
        let cluster_backends = self
            .backends
            .get_mut(cluster_id)
            .ok_or(BackendError::NoBackendForCluster(cluster_id.to_owned()))?;

        if cluster_backends.backends.is_empty() {
            self.available = false;
            return Err(BackendError::NoBackendForCluster(cluster_id.to_owned()));
        }

        let next_backend = match cluster_backends.next_available_backend() {
            Some(nb) => nb,
            None => {
                if self.available {
                    self.available = false;

                    push_event(Event {
                        kind: EventKind::NoAvailableBackends as i32,
                        cluster_id: Some(cluster_id.to_owned()),
                        backend_id: None,
                        address: None,
                    });
                }
                return Err(BackendError::NoBackendForCluster(cluster_id.to_owned()));
            }
        };

        let mut borrowed_backend = next_backend.borrow_mut();

        debug!(
            "Connecting {} -> {:?}",
            cluster_id,
            (
                borrowed_backend.address,
                borrowed_backend.active_connections,
                borrowed_backend.failures
            )
        );

        let tcp_stream = borrowed_backend.try_connect().map_err(|backend_error| {
            BackendError::ConnectionFailures {
                cluster_id: cluster_id.to_owned(),
                backend_address: borrowed_backend.address,
                failures: borrowed_backend.failures,
                error: backend_error.to_string(),
            }
        })?;
        self.available = true;

        Ok((next_backend.clone(), tcp_stream))
    }

    pub fn backend_from_sticky_session(
        &mut self,
        cluster_id: &str,
        sticky_session: &str,
    ) -> Result<(Rc<RefCell<Backend>>, TcpStream), BackendError> {
        let sticky_conn = self
            .backends
            .get_mut(cluster_id)
            .and_then(|cluster_backends| cluster_backends.find_sticky(sticky_session))
            .map(|backend| {
                let mut borrowed = backend.borrow_mut();
                let conn = borrowed.try_connect();

                conn.map(|tcp_stream| (backend.clone(), tcp_stream))
                    .map_err(|e| {
                        error!(
                            "could not connect {} to {:?} using session {} ({} failures)",
                            cluster_id, borrowed.address, sticky_session, borrowed.failures
                        );
                        e
                    })
            });

        match sticky_conn {
            Some(backend_and_stream) => backend_and_stream,
            None => {
                debug!(
                    "Couldn't find a backend corresponding to sticky_session {} for cluster {}",
                    sticky_session, cluster_id
                );
                self.backend_from_cluster_id(cluster_id)
            }
        }
    }

    pub fn set_load_balancing_policy_for_cluster(
        &mut self,
        cluster_id: &str,
        lb_algo: LoadBalancingAlgorithms,
        metric: Option<LoadMetric>,
    ) {
        // The cluster can be created before the backends were registered because of the async config messages.
        // So when we set the load balancing policy, we have to create the backend list if if it doesn't exist yet.
        let cluster_backends = self.get_or_create_backend_list_for_cluster(cluster_id);
        cluster_backends.set_load_balancing_policy(lb_algo, metric);
    }

    pub fn get_or_create_backend_list_for_cluster(&mut self, cluster_id: &str) -> &mut BackendList {
        self.backends
            .entry(cluster_id.to_string())
            .or_default()
    }
}

#[derive(Debug)]
pub struct BackendList {
    pub backends: Vec<Rc<RefCell<Backend>>>,
    pub next_id: u32,
    pub load_balancing: Box<dyn LoadBalancingAlgorithm>,
}

impl Default for BackendList {
    fn default() -> Self {
        Self::new()
    }
}

impl BackendList {
    pub fn new() -> BackendList {
        BackendList {
            backends: Vec::new(),
            next_id: 0,
            load_balancing: Box::new(Random),
        }
    }

    pub fn import_configuration_state(
        backend_vec: &[sozu_command_lib::response::Backend],
    ) -> BackendList {
        let mut list = BackendList::new();
        for backend in backend_vec {
            let backend = Backend::new(
                &backend.backend_id,
                backend.address,
                backend.sticky_id.clone(),
                backend.load_balancing_parameters.clone(),
                backend.backup,
            );
            list.add_backend(backend);
        }

        list
    }

    pub fn add_backend(&mut self, backend: Backend) {
        match self.backends.iter_mut().find(|b| {
            b.borrow().address == backend.address && b.borrow().backend_id == backend.backend_id
        }) {
            None => {
                let backend = Rc::new(RefCell::new(backend));
                self.backends.push(backend);
                self.next_id += 1;
            }
            // the backend already exists, update the configuration while
            // keeping connection retry state
            Some(old_backend) => {
                let mut b = old_backend.borrow_mut();
                b.sticky_id = backend.sticky_id.clone();
                b.load_balancing_parameters = backend.load_balancing_parameters.clone();
                b.backup = backend.backup;
            }
        }
    }

    pub fn remove_backend(&mut self, backend_address: &SocketAddr) {
        self.backends
            .retain(|backend| &backend.borrow().address != backend_address);
    }

    pub fn has_backend(&self, backend_address: &SocketAddr) -> bool {
        self.backends
            .iter()
            .any(|backend| backend.borrow().address == *backend_address)
    }

    pub fn find_backend(
        &mut self,
        backend_address: &SocketAddr,
    ) -> Option<&mut Rc<RefCell<Backend>>> {
        self.backends
            .iter_mut()
            .find(|backend| backend.borrow().address == *backend_address)
    }

    pub fn find_sticky(&mut self, sticky_session: &str) -> Option<&mut Rc<RefCell<Backend>>> {
        self.backends
            .iter_mut()
            .find(|b| b.borrow().sticky_id.as_deref() == Some(sticky_session))
            .and_then(|b| if b.borrow().can_open() { Some(b) } else { None })
    }

    pub fn available_backends(&mut self, backup: bool) -> Vec<Rc<RefCell<Backend>>> {
        self.backends
            .iter()
            .filter(|backend| {
                let owned = backend.borrow();
                owned.backup == backup && owned.can_open()
            })
            .map(Clone::clone)
            .collect()
    }

    pub fn next_available_backend(&mut self) -> Option<Rc<RefCell<Backend>>> {
        let mut backends = self.available_backends(false);

        if backends.is_empty() {
            backends = self.available_backends(true);
        }

        if backends.is_empty() {
            return None;
        }

        self.load_balancing.next_available_backend(&mut backends)
    }

    pub fn set_load_balancing_policy(
        &mut self,
        load_balancing_policy: LoadBalancingAlgorithms,
        metric: Option<LoadMetric>,
    ) {
        match load_balancing_policy {
            LoadBalancingAlgorithms::RoundRobin => {
                self.load_balancing = Box::new(RoundRobin::new())
            }
            LoadBalancingAlgorithms::Random => self.load_balancing = Box::new(Random {}),
            LoadBalancingAlgorithms::LeastLoaded => {
                self.load_balancing = Box::new(LeastLoaded {
                    metric: metric.unwrap_or(LoadMetric::Connections),
                })
            }
            LoadBalancingAlgorithms::PowerOfTwo => {
                self.load_balancing = Box::new(PowerOfTwo {
                    metric: metric.unwrap_or(LoadMetric::Connections),
                })
            }
        }
    }
}

#[cfg(test)]
mod backends_test {

    use super::*;
    use std::{net::TcpListener, sync::mpsc::*, thread};

    fn run_mock_tcp_server(addr: &str, stopper: Receiver<()>) {
        let mut run = true;
        let listener = TcpListener::bind(addr).unwrap();

        thread::spawn(move || {
            while run {
                for _stream in listener.incoming() {
                    // accept connections
                    if let Ok(()) = stopper.try_recv() {
                        run = false;
                    }
                }
            }
        });
    }

    #[test]
    fn it_should_retrieve_a_backend_from_cluster_id_when_backends_have_been_recorded() {
        let mut backend_map = BackendMap::new();
        let cluster_id = "mycluster";

        let backend_addr = "127.0.0.1:1236";
        let (sender, receiver) = channel();
        run_mock_tcp_server(backend_addr, receiver);

        backend_map.add_backend(
            cluster_id,
            Backend::new(
                &format!("{cluster_id}-1"),
                backend_addr.parse().unwrap(),
                None,
                None,
                None,
            ),
        );

        assert!(backend_map.backend_from_cluster_id(cluster_id).is_ok());
        sender.send(()).unwrap();
    }

    #[test]
    fn it_should_not_retrieve_a_backend_from_cluster_id_when_backend_has_not_been_recorded() {
        let mut backend_map = BackendMap::new();
        let cluster_not_recorded = "not";
        backend_map.add_backend(
            "foo",
            Backend::new("foo-1", "127.0.0.1:9001".parse().unwrap(), None, None, None),
        );

        assert!(backend_map
            .backend_from_cluster_id(cluster_not_recorded)
            .is_err());
    }

    #[test]
    fn it_should_not_retrieve_a_backend_from_cluster_id_when_backend_list_is_empty() {
        let mut backend_map = BackendMap::new();

        assert!(backend_map.backend_from_cluster_id("dumb").is_err());
    }

    #[test]
    fn it_should_retrieve_a_backend_from_sticky_session_when_the_backend_has_been_recorded() {
        let mut backend_map = BackendMap::new();
        let cluster_id = "mycluster";
        let sticky_session = "server-2";

        let backend_addr = "127.0.0.1:3456";
        let (sender, receiver) = channel();
        run_mock_tcp_server(backend_addr, receiver);

        backend_map.add_backend(
            cluster_id,
            Backend::new(
                &format!("{cluster_id}-1"),
                "127.0.0.1:9001".parse().unwrap(),
                Some("server-1".to_string()),
                None,
                None,
            ),
        );
        backend_map.add_backend(
            cluster_id,
            Backend::new(
                &format!("{cluster_id}-2"),
                "127.0.0.1:9000".parse().unwrap(),
                Some("server-2".to_string()),
                None,
                None,
            ),
        );
        // sticky backend
        backend_map.add_backend(
            cluster_id,
            Backend::new(
                &format!("{cluster_id}-3"),
                backend_addr.parse().unwrap(),
                Some("server-3".to_string()),
                None,
                None,
            ),
        );

        assert!(backend_map
            .backend_from_sticky_session(cluster_id, sticky_session)
            .is_ok());
        sender.send(()).unwrap();
    }

    #[test]
    fn it_should_not_retrieve_a_backend_from_sticky_session_when_the_backend_has_not_been_recorded()
    {
        let mut backend_map = BackendMap::new();
        let cluster_id = "mycluster";
        let sticky_session = "test";

        assert!(backend_map
            .backend_from_sticky_session(cluster_id, sticky_session)
            .is_err());
    }

    #[test]
    fn it_should_not_retrieve_a_backend_from_sticky_session_when_the_backend_list_is_empty() {
        let mut backend_map = BackendMap::new();
        let mycluster_not_recorded = "mycluster";
        let sticky_session = "test";

        assert!(backend_map
            .backend_from_sticky_session(mycluster_not_recorded, sticky_session)
            .is_err());
    }

    #[test]
    fn it_should_add_a_backend_when_he_doesnt_already_exist() {
        let backend_id = "myback";
        let mut backends_list = BackendList::new();
        backends_list.add_backend(Backend::new(
            backend_id,
            "127.0.0.1:80".parse().unwrap(),
            None,
            None,
            None,
        ));

        assert_eq!(1, backends_list.backends.len());
    }

    #[test]
    fn it_should_not_add_a_backend_when_he_already_exist() {
        let backend_id = "myback";
        let mut backends_list = BackendList::new();
        backends_list.add_backend(Backend::new(
            backend_id,
            "127.0.0.1:80".parse().unwrap(),
            None,
            None,
            None,
        ));

        //same backend id
        backends_list.add_backend(Backend::new(
            backend_id,
            "127.0.0.1:80".parse().unwrap(),
            None,
            None,
            None,
        ));

        assert_eq!(1, backends_list.backends.len());
    }
}
