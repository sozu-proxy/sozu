use rand::{thread_rng, seq::SliceRandom};

use Backend;

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
    let res = backends.get(self.next_backend as usize % backends.len())
                      .map(|backend| (*backend).clone());

    self.next_backend = (self.next_backend + 1) % backends.len() as u32;
    res
  }

}

#[derive(Debug)]
pub struct RandomAlgorithm;

impl LoadBalancingAlgorithm for RandomAlgorithm {

  fn next_available_backend(&mut self, backends: &Vec<Rc<RefCell<Backend>>>) -> Option<Rc<RefCell<Backend>>> {
    let mut rng = thread_rng();

    (*backends).choose(&mut rng)
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
  use BackendStatus;
  use retry::{RetryPolicyWrapper, ExponentialBackoffPolicy};

  fn create_backend(id: String, connections: Option<usize>) -> Backend {
    Backend {
      sticky_id: None,
      backend_id: id,
      address: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080),
      status: BackendStatus::Normal,
      retry_policy: RetryPolicyWrapper::ExponentialBackoff(ExponentialBackoffPolicy::new(1)),
      active_connections: connections.unwrap_or(0),
      failures: 0,
      load_balancing_parameters: None,
      backup: false,
    }
  }

  #[test]
  fn it_should_find_the_backend_with_least_connections() {
    let backend_with_least_connection = Rc::new(RefCell::new(create_backend("yolo".to_string(), Some(1))));

    let backends = vec![
      Rc::new(RefCell::new(create_backend("nolo".to_string(), Some(10)))),
      Rc::new(RefCell::new(create_backend("philo".to_string(), Some(20)))),
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

  #[test]
  fn it_should_find_backend_with_roundrobin_when_some_backends_were_removed() {
    let mut backends = vec![
      Rc::new(RefCell::new(create_backend("toto".to_string(), None))),
      Rc::new(RefCell::new(create_backend("voto".to_string(), None))),
      Rc::new(RefCell::new(create_backend("yoto".to_string(), None)))
    ];

    let mut roundrobin = RoundRobinAlgorithm { next_backend: 1 };
    let backend = roundrobin.next_available_backend(&backends);
    assert_eq!(backend.as_ref(), backends.get(1));

    backends.remove(1);

    let backend2 = roundrobin.next_available_backend(&backends);
    assert_eq!(backend2.as_ref(),  backends.get(0));
  }
}
