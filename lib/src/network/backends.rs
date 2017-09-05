use std::rc::Rc;
use std::cell::RefCell;
use std::net::SocketAddr;
use std::collections::HashMap;
use rand::random;
use mio::net::TcpStream;

use std::{cmp, thread, time};

use network::{AppId,Backend,ConnectionError};

pub struct BackendMap {
  pub instances:    HashMap<AppId, BackendList>,
  pub retry_policy: ExponentialBackoffPolicy,
  pub max_failures: usize,
}

impl BackendMap {
  pub fn new() -> BackendMap {
    BackendMap {
      instances:    HashMap::new(),
      retry_policy: ExponentialBackoffPolicy::new(MAX_FAILURES_PER_BACKEND),
      max_failures: 3,
    }
  }

  pub fn add_instance(&mut self, app_id: &str, instance_address: &SocketAddr) {
    self.instances.entry(app_id.to_string()).or_insert(BackendList::new()).add_instance(instance_address);
  }

  pub fn remove_instance(&mut self, app_id: &str, instance_address: &SocketAddr) {
    if let Some(instances) = self.instances.get_mut(app_id) {
      instances.remove_instance(instance_address);
    } else {
      error!("Instance was already removed");
    }
  }

  pub fn close_backend_connection(&mut self, app_id: &str, addr: &SocketAddr) {
    if let Some(app_instances) = self.instances.get_mut(app_id) {
      if let Some(ref mut backend) = app_instances.find_instance(addr) {
        (*backend.borrow_mut()).dec_connections();
      }
    }
  }

  pub fn backend_from_app_id(&mut self, app_id: &str) -> Result<(Rc<RefCell<Backend>>,TcpStream),ConnectionError> {
    if let Some(ref mut app_instances) = self.instances.get_mut(app_id) {
      if app_instances.instances.len() == 0 {
        return Err(ConnectionError::NoBackendAvailable);
      }

      let mut retry_policy: &mut ExponentialBackoffPolicy = &mut self.retry_policy;
      retry_policy.try(|| {
        if let Some(ref mut b) = app_instances.next_available_instance() {
          let ref mut backend = *b.borrow_mut();

          info!("Connecting {} -> {:?}", app_id, (backend.address, backend.active_connections, backend.failures));

          let conn = backend.try_connect();
          if backend.failures >= MAX_FAILURES_PER_BACKEND {
            error!("backend {:?} connections failed {} times, disabling it", (backend.address, backend.active_connections), backend.failures);
          }

          return conn.map(|c| (b.clone(), c));
        }

        error!("no more available backends for app {}", app_id);
        Err(ConnectionError::NoBackendAvailable)
      })
    } else {
      Err(ConnectionError::NoBackendAvailable)
    }
  }

  pub fn backend_from_sticky_session(&mut self, app_id: &str, sticky_session: u32) -> Result<(Rc<RefCell<Backend>>,TcpStream),ConnectionError> {
    let sticky_conn: Option<Result<(Rc<RefCell<Backend>>,TcpStream),ConnectionError>>;
    {
      let retry_policy: &mut ExponentialBackoffPolicy = &mut self.retry_policy;
      sticky_conn = self.instances
        .get_mut(app_id)
        .and_then(|app_instances| app_instances.find_sticky(sticky_session))
        .map(|b| {
          retry_policy.try(|| {
            let ref mut backend = *b.borrow_mut();
            let conn = backend.try_connect();
            info!("Connecting {} -> {:?} using session {}", app_id, (backend.address, backend.active_connections, backend.failures), sticky_session);
            if backend.failures >= MAX_FAILURES_PER_BACKEND {
              error!("backend {:?} connections failed {} times, disabling it", (backend.address, backend.active_connections), backend.failures);
            }

            conn.map(|c| (b.clone(), c))
          })
        });
    }

    if let Some(res) = sticky_conn {
      return res;
    } else {
      debug!("Couldn't find a backend corresponding to sticky_session {} for app {}", sticky_session, app_id);
      return self.backend_from_app_id(app_id);
    }
  }
}

const MAX_FAILURES_PER_BACKEND: usize = 10;

pub trait RetryPolicy {
  fn max_tries(&self) -> usize;
  fn current_tries(&self) -> usize;
  fn try<T, F: FnMut() -> Result<T, ConnectionError>>(&mut self, action: F) -> Result<T, ConnectionError>;

  fn can_try(&self) -> bool {
    self.current_tries() <= self.max_tries()
  }
}

#[derive(Debug)]
pub struct ExponentialBackoffPolicy {
  max_tries: usize,
  current_tries: usize
}

impl ExponentialBackoffPolicy {
  pub fn new(max_tries: usize) -> Self {
    ExponentialBackoffPolicy {
      max_tries: max_tries,
      current_tries: 0
    }
  }
}

impl RetryPolicy for ExponentialBackoffPolicy {
  fn max_tries(&self) -> usize {
    self.max_tries
  }

  fn current_tries(&self) -> usize {
    self.current_tries
  }

  fn try<T, F: FnMut() -> Result<T, ConnectionError>>(&mut self, mut action: F) -> Result<T, ConnectionError> {
    if self.can_try() {
      if let Ok(result) = action() {
        self.current_tries = 0;

        return Ok(result)
      } else {
        let millis = cmp::max(1, 1 << self.current_tries) * 1000;
        let sleep_duration = time::Duration::from_millis(millis);
        thread::sleep(sleep_duration);

        self.current_tries += 1;
        return self.try(action);
      }
    } else {
        return Err(ConnectionError::NoBackendAvailable);
    }
  }
}

pub struct BackendList {
  pub instances: Vec<Rc<RefCell<Backend>>>,
  pub next_id:   u32,
}

impl BackendList {
  pub fn new() -> BackendList {
    BackendList {
      instances: Vec::new(),
      next_id:   0,
    }
  }

  pub fn add_instance(&mut self, instance_address: &SocketAddr) {
    if self.instances.iter().find(|b| &(*b.borrow()).address == instance_address).is_none() {
      let backend = Rc::new(RefCell::new(Backend::new(*instance_address, self.next_id)));
      self.instances.push(backend);
      self.next_id += 1;
    }
  }

  pub fn remove_instance(&mut self, instance_address: &SocketAddr) {
    self.instances.retain(|backend| &(*backend.borrow()).address != instance_address);
  }

  pub fn find_instance(&mut self, instance_address: &SocketAddr) -> Option<&mut Rc<RefCell<Backend>>> {
    self.instances.iter_mut().find(|backend| &(*backend.borrow()).address == instance_address)
  }

  pub fn find_sticky(&mut self, sticky_session: u32) -> Option<&mut Rc<RefCell<Backend>>> {
    self.instances.iter_mut()
      .find(|b| b.borrow().id == sticky_session )
      .and_then(|b| {
        if b.borrow().can_open(MAX_FAILURES_PER_BACKEND) {
          Some(b)
        } else {
          None
        }
      })
  }

  pub fn available_instances(&mut self) -> Vec<&mut Rc<RefCell<Backend>>> {
    self.instances.iter_mut()
      .filter(|backend| (*backend.borrow()).can_open(MAX_FAILURES_PER_BACKEND))
      .collect()
  }

  pub fn next_available_instance(&mut self) -> Option<&mut Rc<RefCell<Backend>>> {
    let mut instances:Vec<&mut Rc<RefCell<Backend>>> = self.available_instances();
    if instances.is_empty() {
      return None;
    }

    let rnd = random::<usize>();
    let idx = rnd % instances.len();

    Some(instances.remove(idx))
  }
}
