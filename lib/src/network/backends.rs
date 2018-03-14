use std::rc::Rc;
use std::cell::RefCell;
use std::net::SocketAddr;
use std::collections::HashMap;
use rand::random;
use mio::net::TcpStream;

use sozu_command::messages;

use network::{AppId,Backend,ConnectionError};

#[derive(Debug)]
pub struct BackendMap {
  pub backends:     HashMap<AppId, BackendList>,
  pub max_failures: usize,
}

impl BackendMap {
  pub fn new() -> BackendMap {
    BackendMap {
      backends:     HashMap::new(),
      max_failures: 3,
    }
  }

  pub fn import_configuration_state(&mut self, backends: &HashMap<AppId, Vec<messages::Backend>>) {
    self.backends.extend(backends.iter().map(|(ref app_id, ref backend_vec)| {
      (app_id.to_string(), BackendList::import_configuration_state(backend_vec))
    }));
  }

  pub fn add_backend(&mut self, app_id: &str, backend_id: &str, backend_address: &SocketAddr) {
    self.backends.entry(app_id.to_string()).or_insert(BackendList::new()).add_backend(backend_id, backend_address);
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

          return conn.map(|c| (b.clone(), c)).map_err(|e| {
            error!("could not connect {} to {:?} ({} failures)", app_id, backend.address, backend.failures);
            e
          });
        } else {
          error!("no more available backends for app {}", app_id);
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
}

const MAX_FAILURES_PER_BACKEND: usize = 10;

#[derive(Debug)]
pub struct BackendList {
  pub backends:  Vec<Rc<RefCell<Backend>>>,
  pub next_id:   u32,
}

impl BackendList {
  pub fn new() -> BackendList {
    BackendList {
      backends:  Vec::new(),
      next_id:   0,
    }
  }

  pub fn import_configuration_state(backend_vec: &Vec<messages::Backend>) -> BackendList {
    let mut list = BackendList::new();
    for ref backend in backend_vec {
      let addr_string = backend.ip_address.to_string() + ":" + &backend.port.to_string();
      let parsed:Option<SocketAddr> = addr_string.parse().ok();
      if let Some(addr) = parsed {
        list.add_backend(&backend.backend_id, &addr);
      }
    }

    list
  }

  pub fn add_backend(&mut self, backend_id: &str, backend_address: &SocketAddr) {
    if self.backends.iter().find(|b| &(*b.borrow()).address == backend_address).is_none() {
      let backend = Rc::new(RefCell::new(Backend::new(backend_id, *backend_address, self.next_id)));
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

  pub fn available_backends(&mut self) -> Vec<&mut Rc<RefCell<Backend>>> {
    self.backends.iter_mut()
      .filter(|backend| (*backend.borrow()).can_open())
      .collect()
  }

  pub fn next_available_backend(&mut self) -> Option<&mut Rc<RefCell<Backend>>> {
    let mut backends:Vec<&mut Rc<RefCell<Backend>>> = self.available_backends();
    if backends.is_empty() {
      return None;
    }

    let rnd = random::<usize>();
    let idx = rnd % backends.len();

    Some(backends.remove(idx))
  }
}
