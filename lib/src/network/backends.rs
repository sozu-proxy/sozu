use std::rc::Rc;
use std::cell::RefCell;
use std::net::SocketAddr;
use std::collections::HashMap;
use rand::random;
use mio::net::TcpStream;

use sozu_command::{messages, config::LoadBalancingAlgorithms, messages::LoadBalancingParams};

use network::{AppId,Backend,ConnectionError,load_balancing::*};

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

  pub fn import_configuration_state(&mut self, backends: &HashMap<AppId, Vec<messages::Backend>>) {
    self.backends.extend(backends.iter().map(|(ref app_id, ref backend_vec)| {
      (app_id.to_string(), BackendList::import_configuration_state(backend_vec))
    }));
  }

  pub fn add_backend(&mut self, app_id: &str, backend_id: &str, backend_address: &SocketAddr, load_balancing_parameters: Option<LoadBalancingParams>) {
    self.backends.entry(app_id.to_string()).or_insert(BackendList::new()).add_backend(backend_id, backend_address, load_balancing_parameters);
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
      if app_backends.backends.len() == 0 {
        self.available = false;
        return Err(ConnectionError::NoBackendAvailable);
      }

      for _ in 0..self.max_failures {
        if let Some(ref mut b) = app_backends.next_available_backend() {
          let ref mut backend = *b.borrow_mut();

          debug!("Connecting {} -> {:?}", app_id, (backend.address, backend.active_connections, backend.failures));
          let conn = backend.try_connect();
          if backend.failures >= MAX_FAILURES_PER_BACKEND {
            error!("backend {:?} connections failed {} times, disabling it", (backend.address, backend.active_connections), backend.failures);
          }

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
          }
          return Err(ConnectionError::NoBackendAvailable);
        }
      }
      Err(ConnectionError::NoBackendAvailable)
    } else {
      Err(ConnectionError::NoBackendAvailable)
    }
  }

  pub fn backend_from_sticky_session(&mut self, app_id: &str, sticky_session: u32) -> Result<(Rc<RefCell<Backend>>,TcpStream),ConnectionError> {
    let sticky_conn: Option<Result<(Rc<RefCell<Backend>>,TcpStream),ConnectionError>> = self.backends
      .get_mut(app_id)
      .and_then(|app_backends| app_backends.find_sticky(sticky_session))
      .map(|b| {
        let ref mut backend = *b.borrow_mut();
        let conn = backend.try_connect();
        if backend.failures >= MAX_FAILURES_PER_BACKEND {
          error!("backend {:?} connections failed {} times, disabling it", (backend.address, backend.active_connections), backend.failures);
        }

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

  pub fn set_load_balancing_policy_for_app(&mut self, app_id: &str, lb_algo: LoadBalancingAlgorithms) {
    // The application can be created before the backends were registered because of the async config messages.
    // So when we set the load balancing policy, we have to create the backend list if if it doesn't exist yet.
    let app_backends = self.get_or_create_backend_list_for_app(app_id);
    app_backends.set_load_balancing_policy(lb_algo);
  }

  pub fn get_or_create_backend_list_for_app(&mut self, app_id: &str) -> &mut BackendList {
    self.backends.entry(app_id.to_string()).or_insert(BackendList::new())
  }
}

const MAX_FAILURES_PER_BACKEND: usize = 10;

#[derive(Debug)]
pub struct BackendList {
  pub backends:       Vec<Rc<RefCell<Backend>>>,
  pub next_id:        u32,
  pub load_balancing: Box<LoadBalancingAlgorithm>,
}

impl BackendList {
  pub fn new() -> BackendList {
    BackendList {
      backends:       Vec::new(),
      next_id:        0,
      load_balancing: Box::new(RandomAlgorithm{}),
    }
  }

  pub fn import_configuration_state(backend_vec: &Vec<messages::Backend>) -> BackendList {
    let mut list = BackendList::new();
    for ref backend in backend_vec {
      let addr_string = backend.ip_address.to_string() + ":" + &backend.port.to_string();
      let parsed:Option<SocketAddr> = addr_string.parse().ok();
      if let Some(addr) = parsed {
        list.add_backend(&backend.backend_id, &addr, backend.load_balancing_parameters.clone());
      }
    }

    list
  }

  pub fn add_backend(&mut self, backend_id: &str, backend_address: &SocketAddr, load_balancing_parameters: Option<LoadBalancingParams>) {
    if self.backends.iter().find(|b| &(*b.borrow()).address == backend_address).is_none() {
      let backend = Rc::new(RefCell::new(Backend::new(backend_id, *backend_address, self.next_id, load_balancing_parameters)));
      self.backends.push(backend);
      self.next_id += 1;
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

  pub fn find_sticky(&mut self, sticky_session: u32) -> Option<&mut Rc<RefCell<Backend>>> {
    self.backends.iter_mut()
      .find(|b| b.borrow().id == sticky_session )
      .and_then(|b| {
        if b.borrow().can_open() {
          Some(b)
        } else {
          None
        }
      })
  }

  pub fn available_backends(&mut self) -> Vec<Rc<RefCell<Backend>>> {
    self.backends.iter()
      .filter(|backend| (*backend.borrow()).can_open())
      .map(|backend| (*backend).clone())
      .collect()
  }

  pub fn next_available_backend(&mut self) -> Option<Rc<RefCell<Backend>>> {
    let backends = self.available_backends();

    if backends.len() == 0 {
      None
    } else {
      self.load_balancing.next_available_backend(&backends)
    }
  }

  pub fn set_load_balancing_policy(&mut self, load_balancing_policy: LoadBalancingAlgorithms) {
    match load_balancing_policy {
      LoadBalancingAlgorithms::RoundRobin => self.load_balancing = Box::new(RoundRobinAlgorithm{ next_backend: 0 }),
      LoadBalancingAlgorithms::Random => self.load_balancing = Box::new(RandomAlgorithm{}),
    }
  }
}

#[cfg(test)]
mod backends_test {

  use super::*;
  use std::{thread,sync::mpsc::*,net::TcpListener};


  fn run_mock_tcp_server(addr: &str, stoper: Receiver<()>) {
    let mut run = true;
    let listener = TcpListener::bind(addr).unwrap();
    listener.set_nonblocking(true).expect("Cannot set non-blocking");

    thread::spawn(move || {
      while run {
        for stream in listener.incoming() {
          // accept connections
        }

        if let Ok(()) = stoper.try_recv() {
          run = false;
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

    backend_map.add_backend(app_id, &format!("{}-1", app_id), &(backend_addr.parse().unwrap()), None);

    assert!(backend_map.backend_from_app_id(app_id).is_ok());
    sender.send(());
  }

  #[test]
  fn it_should_not_retrieve_a_backend_from_app_id_when_backend_has_not_been_recorded() {
    let mut backend_map = BackendMap::new();
    let app_not_recorded = "not";
    backend_map.add_backend("foo", "foo-1", &("127.0.0.1:9001".parse().unwrap()), None);

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
    let sticky_session = 2;

    let backend_addr = "127.0.0.1:3456";
    let (sender, receiver) = channel();
    run_mock_tcp_server(backend_addr, receiver);

    backend_map.add_backend(app_id, &format!("{}-1", app_id), &("127.0.0.1:9001".parse().unwrap()), None);
    backend_map.add_backend(app_id, &format!("{}-2", app_id), &("127.0.0.1:9000".parse().unwrap()), None);
    // sticky backend
    backend_map.add_backend(app_id, &format!("{}-3", app_id), &(backend_addr.parse().unwrap()), None);

    assert!(backend_map.backend_from_sticky_session(app_id, sticky_session).is_ok());
    sender.send(());
  }

  #[test]
  fn it_should_not_retrieve_a_backend_from_sticky_session_when_the_backend_has_not_been_recorded() {
    let mut backend_map = BackendMap::new();
    let app_id = "myapp";
    let sticky_session = 2;

    assert!(backend_map.backend_from_sticky_session(app_id, sticky_session).is_err());
  }

  #[test]
  fn it_should_not_retrieve_a_backend_from_sticky_session_when_the_backend_list_is_empty() {
    let mut backend_map = BackendMap::new();
    let myapp_not_recorded = "myapp";
    let sticky_session = 2;

    assert!(backend_map.backend_from_sticky_session(myapp_not_recorded, sticky_session).is_err());
  }

  #[test]
  fn it_should_add_a_backend_when_he_doesnt_already_exist() {
    let backend_id = "myback";
    let mut backends_list = BackendList::new();
    backends_list.add_backend(backend_id, &("127.0.0.1:80".parse().unwrap()), None);

    assert_eq!(1, backends_list.backends.len());
  }

  #[test]
  fn it_should_not_add_a_backend_when_he_already_exist() {
    let backend_id = "myback";
    let mut backends_list = BackendList::new();
    backends_list.add_backend(backend_id, &("127.0.0.1:80".parse().unwrap()), None);

    //same backend id
    backends_list.add_backend(backend_id, &("127.0.0.1:80".parse().unwrap()), None);

    assert_eq!(1, backends_list.backends.len());
  }
}