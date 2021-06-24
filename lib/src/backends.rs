use std::rc::Rc;
use std::cell::RefCell;
use std::net::SocketAddr;
use std::collections::HashMap;
use mio::net::TcpStream;

use sozu_command::{proxy, config::LoadBalancingAlgorithms};

use super::{AppId,Backend,ConnectionError,load_balancing::*};
use server::push_event;

#[derive(Debug)]
pub struct BackendMap {
  pub backends:     HashMap<AppId, BackendList>,
  pub max_failures: usize,
  pub available:    bool,
}

impl BackendMap {
  pub fn new() -> BackendMap {
    BackendMap {
      backends:     HashMap::new(),
      max_failures: 3,
      available:    true,
    }
  }

  pub fn import_configuration_state(&mut self, backends: &HashMap<AppId, Vec<proxy::Backend>>) {
    self.backends.extend(backends.iter().map(|(ref app_id, ref backend_vec)| {
      (app_id.to_string(), BackendList::import_configuration_state(backend_vec))
    }));
  }

  pub fn add_backend(&mut self, app_id: &str, backend: Backend) {
    self.backends.entry(app_id.to_string()).or_insert_with(BackendList::new).add_backend(backend);
  }

  pub fn remove_backend(&mut self, app_id: &str, backend_address: &SocketAddr) {
    if let Some(backends) = self.backends.get_mut(app_id) {
      backends.remove_backend(backend_address);
    } else {
      error!("Backend was already removed: app id {}, address {:?}", app_id, backend_address);
    }
  }

  pub fn close_backend_connection(&mut self, app_id: &str, addr: &SocketAddr) {
    if let Some(app_backends) = self.backends.get_mut(app_id) {
      if let Some(ref mut backend) = app_backends.find_backend(addr) {
        (*backend.borrow_mut()).dec_connections();
      }
    }
  }

  pub fn has_backend(&self, app_id: &str, backend: &Backend) -> bool {
    self.backends.get(app_id).map(|backends| {
      backends.has_backend(&backend.address)
    }).unwrap_or(false)
  }

  pub fn backend_from_app_id(&mut self, app_id: &str) -> Result<(Rc<RefCell<Backend>>,TcpStream),ConnectionError> {
    if let Some(ref mut app_backends) = self.backends.get_mut(app_id) {
      if app_backends.backends.is_empty() {
        self.available = false;
        return Err(ConnectionError::NoBackendAvailable);
      }

      if let Some(ref mut b) = app_backends.next_available_backend() {
        let ref mut backend = *b.borrow_mut();

        debug!("Connecting {} -> {:?}", app_id, (backend.address, backend.active_connections, backend.failures));
        let conn = backend.try_connect();

        let res = conn.map(|c| {
          (b.clone(), c)
        }).map_err(|e| {
          error!("could not connect {} to {:?} ({} failures)", app_id, backend.address, backend.failures);
          e
        });

        if res.is_ok() {
          self.available = true;
        }

        return res;
      } else {
        if self.available {
          error!("no more available backends for app {}", app_id);
          self.available = false;

          push_event(proxy::ProxyEvent::NoAvailableBackends(app_id.to_string()));
        }
        return Err(ConnectionError::NoBackendAvailable);
      }
    } else {
      Err(ConnectionError::NoBackendAvailable)
    }
  }

  pub fn backend_from_sticky_session(&mut self, app_id: &str, sticky_session: &str) -> Result<(Rc<RefCell<Backend>>,TcpStream),ConnectionError> {
    let sticky_conn: Option<Result<(Rc<RefCell<Backend>>,TcpStream),ConnectionError>> = self.backends
      .get_mut(app_id)
      .and_then(|app_backends| app_backends.find_sticky(sticky_session))
      .map(|b| {
        let ref mut backend = *b.borrow_mut();
        let conn = backend.try_connect();

        conn.map(|c| (b.clone(), c)).map_err(|e| {
          error!("could not connect {} to {:?} using session {} ({} failures)",
            app_id, backend.address, sticky_session, backend.failures);
          e
        })
      });

    if let Some(res) = sticky_conn {
      return res;
    } else {
      debug!("Couldn't find a backend corresponding to sticky_session {} for app {}", sticky_session, app_id);
      return self.backend_from_app_id(app_id);
    }
  }

  pub fn set_load_balancing_policy_for_app(&mut self, app_id: &str, lb_algo: LoadBalancingAlgorithms, metric: Option<proxy::LoadMetric>) {
    // The application can be created before the backends were registered because of the async config messages.
    // So when we set the load balancing policy, we have to create the backend list if if it doesn't exist yet.
    let app_backends = self.get_or_create_backend_list_for_app(app_id);
    app_backends.set_load_balancing_policy(lb_algo, metric);
  }

  pub fn get_or_create_backend_list_for_app(&mut self, app_id: &str) -> &mut BackendList {
    self.backends.entry(app_id.to_string()).or_insert_with(BackendList::new)
  }
}

#[derive(Debug)]
pub struct BackendList {
  pub backends:       Vec<Rc<RefCell<Backend>>>,
  pub next_id:        u32,
  pub load_balancing: Box<dyn LoadBalancingAlgorithm>,
}

impl BackendList {
  pub fn new() -> BackendList {
    BackendList {
      backends:       Vec::new(),
      next_id:        0,
      load_balancing: Box::new(Random),
    }
  }

  pub fn import_configuration_state(backend_vec: &Vec<proxy::Backend>) -> BackendList {
    let mut list = BackendList::new();
    for ref backend in backend_vec {
      let backend = Backend::new(&backend.backend_id, backend.address, backend.sticky_id.clone(), backend.load_balancing_parameters.clone(), backend.backup);
      list.add_backend(backend);
    }

    list
  }

  pub fn add_backend(&mut self, backend: Backend) {
    match self.backends.iter_mut().find(|b| {
        (*b.borrow()).address == backend.address
            && (*b.borrow()).backend_id == backend.backend_id
    }) {
        None => {
            let backend = Rc::new(RefCell::new(backend));
            self.backends.push(backend);
            self.next_id += 1;
        },
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
    self.backends.retain(|backend| &(*backend.borrow()).address != backend_address);
  }

  pub fn has_backend(&self, backend_address: &SocketAddr) -> bool {
    self.backends.iter().any(|backend| &(*backend.borrow()).address == backend_address)
  }

  pub fn find_backend(&mut self, backend_address: &SocketAddr) -> Option<&mut Rc<RefCell<Backend>>> {
    self.backends.iter_mut().find(|backend| &(*backend.borrow()).address == backend_address)
  }

  pub fn find_sticky(&mut self, sticky_session: &str) -> Option<&mut Rc<RefCell<Backend>>> {
    self.backends.iter_mut()
      .find(|b| b.borrow().sticky_id.as_ref().map(|s| s.as_str()) == Some(sticky_session) )
      .and_then(|b| {
        if b.borrow().can_open() {
          Some(b)
        } else {
          None
        }
      })
  }

  pub fn available_backends(&mut self, backup: bool) -> Vec<Rc<RefCell<Backend>>> {
    self.backends.iter()
      .filter(|backend| (*backend.borrow()).backup == backup && (*backend.borrow()).can_open())
      .map(|backend| (*backend).clone())
      .collect()
  }

  pub fn next_available_backend(&mut self) -> Option<Rc<RefCell<Backend>>> {
    let mut backends = self.available_backends(false);

    if backends.is_empty() {
      backends = self.available_backends(true);
    }

    if backends.is_empty() {
      None
    } else {
      self.load_balancing.next_available_backend(&mut backends)
    }
  }

  pub fn set_load_balancing_policy(&mut self, load_balancing_policy: LoadBalancingAlgorithms, metric: Option<proxy::LoadMetric>) {
    match load_balancing_policy {
      LoadBalancingAlgorithms::RoundRobin => self.load_balancing = Box::new(RoundRobin::new()),
      LoadBalancingAlgorithms::Random => self.load_balancing = Box::new(Random{}),
      LoadBalancingAlgorithms::LeastLoaded => self.load_balancing = Box::new(LeastLoaded{ metric: metric.clone().unwrap_or(proxy::LoadMetric::Connections) }),
      LoadBalancingAlgorithms::PowerOfTwo => self.load_balancing = Box::new(PowerOfTwo{ metric: metric.clone().unwrap_or(proxy::LoadMetric::Connections) }),
    }
  }
}

#[cfg(test)]
mod backends_test {

  use super::*;
  use std::{thread,sync::mpsc::*,net::TcpListener};


  fn run_mock_tcp_server(addr: &str, stopper: Receiver<()>) {
    let mut run = true;
    let listener = TcpListener::bind(addr).unwrap();

    thread::spawn(move || {
      while run {
        for stream in listener.incoming() {
          // accept connections
          if let Ok(()) = stopper.try_recv() {
            run = false;
          }
        }

      }
    });
  }

  #[test]
  fn it_should_retrieve_a_backend_from_app_id_when_backends_have_been_recorded() {
    let mut backend_map = BackendMap::new();
    let app_id = "myapp";

    let backend_addr = "127.0.0.1:1236";
    let (sender, receiver) = channel();
    run_mock_tcp_server(backend_addr, receiver);

    backend_map.add_backend(app_id, Backend::new(&format!("{}-1", app_id), backend_addr.parse().unwrap(), None, None, None));

    assert!(backend_map.backend_from_app_id(app_id).is_ok());
    sender.send(()).unwrap();
  }

  #[test]
  fn it_should_not_retrieve_a_backend_from_app_id_when_backend_has_not_been_recorded() {
    let mut backend_map = BackendMap::new();
    let app_not_recorded = "not";
    backend_map.add_backend("foo", Backend::new("foo-1", "127.0.0.1:9001".parse().unwrap(), None, None, None));

    assert!(backend_map.backend_from_app_id(app_not_recorded).is_err());
  }

  #[test]
  fn it_should_not_retrieve_a_backend_from_app_id_when_backend_list_is_empty() {
    let mut backend_map = BackendMap::new();

    assert!(backend_map.backend_from_app_id("dumb").is_err());
  }

  #[test]
  fn it_should_retrieve_a_backend_from_sticky_session_when_the_backend_has_been_recorded() {
    let mut backend_map = BackendMap::new();
    let app_id = "myapp";
    let sticky_session = "server-2";

    let backend_addr = "127.0.0.1:3456";
    let (sender, receiver) = channel();
    run_mock_tcp_server(backend_addr, receiver);

    backend_map.add_backend(app_id, Backend::new(&format!("{}-1", app_id), "127.0.0.1:9001".parse().unwrap(), Some("server-1".to_string()), None, None));
    backend_map.add_backend(app_id, Backend::new(&format!("{}-2", app_id), "127.0.0.1:9000".parse().unwrap(), Some("server-2".to_string()), None, None));
    // sticky backend
    backend_map.add_backend(app_id, Backend::new(&format!("{}-3", app_id), backend_addr.parse().unwrap(), Some("server-3".to_string()), None, None));

    assert!(backend_map.backend_from_sticky_session(app_id, sticky_session).is_ok());
    sender.send(()).unwrap();
  }

  #[test]
  fn it_should_not_retrieve_a_backend_from_sticky_session_when_the_backend_has_not_been_recorded() {
    let mut backend_map = BackendMap::new();
    let app_id = "myapp";
    let sticky_session = "test";

    assert!(backend_map.backend_from_sticky_session(app_id, sticky_session).is_err());
  }

  #[test]
  fn it_should_not_retrieve_a_backend_from_sticky_session_when_the_backend_list_is_empty() {
    let mut backend_map = BackendMap::new();
    let myapp_not_recorded = "myapp";
    let sticky_session = "test";

    assert!(backend_map.backend_from_sticky_session(myapp_not_recorded, sticky_session).is_err());
  }

  #[test]
  fn it_should_add_a_backend_when_he_doesnt_already_exist() {
    let backend_id = "myback";
    let mut backends_list = BackendList::new();
    backends_list.add_backend(Backend::new(backend_id, "127.0.0.1:80".parse().unwrap(), None, None, None));

    assert_eq!(1, backends_list.backends.len());
  }

  #[test]
  fn it_should_not_add_a_backend_when_he_already_exist() {
    let backend_id = "myback";
    let mut backends_list = BackendList::new();
    backends_list.add_backend(Backend::new(backend_id, "127.0.0.1:80".parse().unwrap(), None, None, None));

    //same backend id
    backends_list.add_backend(Backend::new(backend_id, "127.0.0.1:80".parse().unwrap(), None, None, None));

    assert_eq!(1, backends_list.backends.len());
  }
}
