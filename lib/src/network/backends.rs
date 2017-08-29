use std::rc::Rc;
use std::cell::RefCell;
use std::net::SocketAddr;
use std::collections::HashMap;
use rand::random;
use mio::net::TcpStream;

use network::{AppId,Backend,ConnectionError};

pub struct BackendMap {
  pub instances: HashMap<AppId, Vec<Rc<RefCell<Backend>>>>,
}

impl BackendMap {
  pub fn new() -> BackendMap {
    BackendMap {
      instances: HashMap::new(),
    }
  }

  pub fn add_instance(&mut self, app_id: &str, instance_address: &SocketAddr) {
    if let Some(addrs) = self.instances.get_mut(app_id) {
      let id = addrs.last().map(|b| (*b.borrow_mut()).id ).unwrap_or(0) + 1;
      let backend = Rc::new(RefCell::new(Backend::new(*instance_address, id)));
      if !addrs.contains(&backend) {
        addrs.push(backend);
      }
    }

    if self.instances.get(app_id).is_none() {
      let backend = Backend::new(*instance_address, 0);
      self.instances.insert(String::from(app_id), vec![Rc::new(RefCell::new(backend))]);
    }
  }

  pub fn remove_instance(&mut self, app_id: &str, instance_address: &SocketAddr) {
    if let Some(instances) = self.instances.get_mut(app_id) {
      instances.retain(|backend| &(*backend.borrow()).address != instance_address);
    } else {
      error!("Instance was already removed");
    }
  }

  pub fn close_backend_connection(&mut self, app_id: &str, addr: &SocketAddr) {
    if let Some(app_instances) = self.instances.get_mut(app_id) {
      if let Some(ref mut backend) = app_instances.iter_mut().find(|backend| &(*backend.borrow()).address == addr) {
        (*backend.borrow_mut()).dec_connections();
      }
    }
  }

  pub fn backend_from_app_id(&mut self, app_id: &str) -> Result<(Rc<RefCell<Backend>>,TcpStream),ConnectionError> {
    if let Some(ref mut app_instances) = self.instances.get_mut(app_id) {
      if app_instances.len() == 0 {
        return Err(ConnectionError::NoBackendAvailable);
      }

      //FIXME: hardcoded for now, these should come from configuration
      let max_failures_per_backend:usize = 10;
      let max_failures:usize             = 3;

      for _ in 0..max_failures {
        //FIXME: it's probably pretty wasteful to refilter every time here
        let mut instances:Vec<&mut Rc<RefCell<Backend>>> = app_instances.iter_mut().filter(|backend| (*backend.borrow()).can_open(max_failures_per_backend)).collect();
        if instances.is_empty() {
          error!("no more available backends for app {}", app_id);
          return Err(ConnectionError::NoBackendAvailable);
        }
        let rnd = random::<usize>();
        let idx = rnd % instances.len();

        let conn = instances.get_mut(idx).ok_or(ConnectionError::NoBackendAvailable).and_then(|ref mut b| {
          let ref mut backend = *b.borrow_mut();
          info!("Connecting {} -> {:?}", app_id, (backend.address, backend.active_connections, backend.failures));
          let conn = backend.try_connect(max_failures_per_backend);
          if backend.failures >= max_failures_per_backend {
            error!("backend {:?} connections failed {} times, disabling it", (backend.address, backend.active_connections), backend.failures);
          }

          conn.map(|c| (b.clone(), c))
        });

        if conn.is_ok() {
          return conn;
        }
      }
      Err(ConnectionError::NoBackendAvailable)
    } else {
      Err(ConnectionError::NoBackendAvailable)
    }
  }

  pub fn backend_from_sticky_session(&mut self, app_id: &str, sticky_session: u32) -> Result<(Rc<RefCell<Backend>>,TcpStream),ConnectionError> {
    let max_failures_per_backend = 10;
    if let Some(ref mut app_instances) = self.instances.get_mut(app_id) {
      let sticky_backend: Option<&mut Rc<RefCell<Backend>>> = app_instances.iter_mut().find(|b| {
        let backend = &*b.borrow();
        backend.id == sticky_session && backend.can_open(max_failures_per_backend)
      });

      if let Some(b) = sticky_backend {
        //FIXME: hardcoded for now, these should come from configuration
        let ref mut backend = *b.borrow_mut();
        let conn = backend.try_connect(max_failures_per_backend);
        info!("Connecting {} -> {:?} using session {}", app_id, (backend.address, backend.active_connections, backend.failures), sticky_session);
        if backend.failures >= max_failures_per_backend {
          error!("backend {:?} connections failed {} times, disabling it", (backend.address, backend.active_connections), backend.failures);
        }

        return conn.map(|c| (b.clone(), c));
      }
    }

    debug!("Couldn't find a backend corresponding to sticky_session {} for app {}", sticky_session, app_id);
    Err(ConnectionError::NoBackendAvailable)
  }
}
