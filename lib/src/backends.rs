use std::{cell::RefCell, collections::HashMap, net::SocketAddr, rc::Rc};

use anyhow::{bail, Context};
use mio::net::TcpStream;

use sozu_command::{
    proto::command::LoadBalancingAlgorithms, request::LoadMetric, response::Event, state::ClusterId,
};

use crate::server::push_event;

use super::{load_balancing::*, Backend};

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
            .or_insert_with(BackendList::new)
            .add_backend(backend);
    }

    // TODO: return anyhow::Result with context, log the error downstream
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

    // TODO: return anyhow::Result with context, log the error downstream
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
    ) -> anyhow::Result<(Rc<RefCell<Backend>>, TcpStream)> {
        let cluster_backends = self
            .backends
            .get_mut(cluster_id)
            .with_context(|| format!("No backend found for cluster {cluster_id}"))?;

        if cluster_backends.backends.is_empty() {
            self.available = false;
            bail!(format!(
                "Found an empty backend list for cluster {cluster_id}"
            ));
        }

        let next_backend = match cluster_backends.next_available_backend() {
            Some(nb) => nb,
            None => {
                if self.available {
                    self.available = false;

                    push_event(Event::NoAvailableBackends(cluster_id.to_string()));
                }
                bail!("No more backend available for cluster {}", cluster_id);
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

        let tcp_stream = borrowed_backend.try_connect().with_context(|| {
            format!(
                "could not connect {} to {:?} ({} failures)",
                cluster_id, borrowed_backend.address, borrowed_backend.failures
            )
        })?;
        self.available = true;

        Ok((next_backend.clone(), tcp_stream))
    }

    pub fn backend_from_sticky_session(
        &mut self,
        cluster_id: &str,
        sticky_session: &str,
    ) -> anyhow::Result<(Rc<RefCell<Backend>>, TcpStream)> {
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
            .or_insert_with(BackendList::new)
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
