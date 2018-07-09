use rand::{thread_rng, Rng};

use network::Backend;
use sozu_command::config::LoadBalancingAlgorithms;

use std::{ rc::Rc, cell::RefCell };
use std::fmt::Debug;

pub trait LoadBalancingAlgorithm: Debug {
  fn next_available_backend(&mut self, backends: &Vec<Rc<RefCell<Backend>>>) -> Option<Rc<RefCell<Backend>>>;
}

#[derive(Debug)]
pub struct RoundRobinAlgorithm {
  pub next_backend: u32,
}

impl LoadBalancingAlgorithm for RoundRobinAlgorithm {

  fn next_available_backend(&mut self , backends: &Vec<Rc<RefCell<Backend>>>) -> Option<Rc<RefCell<Backend>>> {
    let res = backends.get(self.next_backend as usize)
                      .map(|backend| (*backend).clone());

    self.next_backend = (self.next_backend + 1) % backends.len() as u32;
    res
  }

}

impl RoundRobinAlgorithm {

  fn new() -> Self {
    Self {
      next_backend: 0,
    }
  }

}

#[derive(Debug)]
pub struct RandomAlgorithm;

impl LoadBalancingAlgorithm for RandomAlgorithm {

  fn next_available_backend(&mut self, backends: &Vec<Rc<RefCell<Backend>>>) -> Option<Rc<RefCell<Backend>>> {
    let mut rng = thread_rng();

    rng.choose(backends)
      .map(|backend| (*backend).clone())
  }

}

#[derive(Debug)]
pub struct LeastConnectionsAlgorithm;

impl LoadBalancingAlgorithm for LeastConnectionsAlgorithm {

  fn next_available_backend(&mut self, backends: &Vec<Rc<RefCell<Backend>>>) -> Option<Rc<RefCell<Backend>>> {
    backends
      .iter()
      .min_by_key(|backend| backend.borrow().active_connections)
      .map(|backend| (*backend).clone())
  }

}

#[cfg(test)]
mod test {
  use super::*;
  use std::net::{IpAddr, Ipv4Addr, SocketAddr};
  use network::BackendStatus;
  use network::retry::{RetryPolicyWrapper, ExponentialBackoffPolicy};

  #[test]
  fn it_should_find_the_backend_with_least_connections() {
    let least_active_connections = 1;

    let backend_with_least_connection = Rc::new(RefCell::new(Backend {
      sticky_id: None,
      backend_id: "yolo".to_string(),
      address: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080),
      status: BackendStatus::Normal,
      retry_policy: RetryPolicyWrapper::ExponentialBackoff(ExponentialBackoffPolicy::new(1)),
      active_connections: least_active_connections,
      failures: 0,
      load_balancing_parameters: None,
    }));

    let backends = vec![
      Rc::new(RefCell::new(Backend {
        sticky_id: None,
        backend_id: "nolo".to_string(),
        address: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080),
        status: BackendStatus::Normal,
        retry_policy: RetryPolicyWrapper::ExponentialBackoff(ExponentialBackoffPolicy::new(1)),
        active_connections: 10,
        failures: 0,
        load_balancing_parameters: None,
      })),
      Rc::new(RefCell::new(Backend {
        sticky_id: None,
        backend_id: "philo".to_string(),
        address: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080),
        status: BackendStatus::Normal,
        retry_policy: RetryPolicyWrapper::ExponentialBackoff(ExponentialBackoffPolicy::new(1)),
        active_connections: 20,
        failures: 0,
        load_balancing_parameters: None,
      })),
      backend_with_least_connection.clone(),
    ];

    let mut least_connection_algorithm = LeastConnectionsAlgorithm{};

    let backend_res = least_connection_algorithm.next_available_backend(&backends).unwrap();
    let backend = backend_res.borrow();

    assert!(*backend == *backend_with_least_connection.borrow());
  }

  #[test]
  fn it_shouldnt_find_backend_with_least_connections_when_list_is_empty() {
    let backends = vec![];

    let mut least_connection_algorithm = LeastConnectionsAlgorithm{};

    let backend = least_connection_algorithm.next_available_backend(&backends);
    assert!(backend.is_none());
  }
}
