use rand::{thread_rng, seq::SliceRandom,
  distributions::{Distribution, WeightedIndex}, Rng};

use Backend;

use std::{ rc::Rc, cell::RefCell };
use std::fmt::Debug;
use sozu_command::proxy::LoadMetric;

pub trait LoadBalancingAlgorithm: Debug {
  fn next_available_backend(&mut self, backends: &mut Vec<Rc<RefCell<Backend>>>) -> Option<Rc<RefCell<Backend>>>;
}

#[derive(Debug)]
pub struct RoundRobin {
  pub next_backend: u32,
}

impl LoadBalancingAlgorithm for RoundRobin {

  fn next_available_backend(&mut self , backends: &mut Vec<Rc<RefCell<Backend>>>) -> Option<Rc<RefCell<Backend>>> {
    let res = backends.get(self.next_backend as usize % backends.len())
                      .map(|backend| (*backend).clone());

    self.next_backend = (self.next_backend + 1) % backends.len() as u32;
    res
  }

}

impl RoundRobin {
  pub fn new() -> Self {
    Self {
      next_backend: 0,
    }
  }

}

#[derive(Debug)]
pub struct Random;

impl LoadBalancingAlgorithm for Random {

  fn next_available_backend(&mut self, backends: &mut Vec<Rc<RefCell<Backend>>>) -> Option<Rc<RefCell<Backend>>> {
    let mut rng = thread_rng();
    let weights: Vec<u8> = backends.iter()
        .map(|b| b.borrow().load_balancing_parameters.as_ref().map(|p| p.weight).unwrap_or(100))
        .collect();

    if let Ok(dist) = WeightedIndex::new(&weights) {
        let index = dist.sample(&mut rng);
        backends.get(index).cloned()
    } else {
        (*backends).choose(&mut rng)
            .map(|backend| (*backend).clone())
    }
  }

}

#[derive(Debug)]
pub struct LeastLoaded { pub metric: LoadMetric }

impl LoadBalancingAlgorithm for LeastLoaded {

  fn next_available_backend(&mut self, backends: &mut Vec<Rc<RefCell<Backend>>>) -> Option<Rc<RefCell<Backend>>> {
    let opt_b = match self.metric {
        LoadMetric::Connections => backends
            .iter_mut()
            .min_by_key(|backend| backend.borrow().active_connections),
        LoadMetric::Requests => backends
            .iter_mut()
            .min_by_key(|backend| backend.borrow().active_requests),
        LoadMetric::ConnectionTime => {
            let mut b = None;
            for backend in backends.iter_mut() {
                let cost2 = backend.borrow_mut().peak_ewma_connection();

                match b.take() {
                    None => b = Some((cost2, backend)),
                    Some((cost1, back1)) => {
                        if cost1 <= cost2 {
                            b = Some((cost1, back1));
                        } else {
                            b = Some((cost2, backend));
                        }
                    }
                }
            }

            b.map(|(_cost, backend)| backend)
        }
    };
    opt_b.map(|backend| (*backend).clone())
  }
}

#[derive(Debug)]
pub struct PowerOfTwo{ pub metric: LoadMetric }

impl LoadBalancingAlgorithm for PowerOfTwo {
    fn next_available_backend(&mut self, backends: &mut Vec<Rc<RefCell<Backend>>>) -> Option<Rc<RefCell<Backend>>> {
        let mut first = None;
        let mut second = None;

        for backend in backends.iter_mut() {
            let measure = match self.metric {
                LoadMetric::Connections => backend.borrow().active_connections as f64,
                LoadMetric::Requests => backend.borrow().active_requests as f64,
                LoadMetric::ConnectionTime => backend.borrow_mut().peak_ewma_connection(),
            };

            if first.is_none() {
                first = Some((measure, backend));
            } else if second.is_none() {
                if first.as_ref().unwrap().0 <= measure {
                    second = Some((measure, backend));
                } else {
                    second = first.take();
                    first = Some((measure, backend));
                }
            } else {
                if first.as_ref().unwrap().0 <= measure {
                    if measure < second.as_ref().unwrap().0 {
                        second = Some((measure, backend));
                    } // other case: we don't change anything
                } else {
                    second = first.take();
                    first = Some((measure, backend));
                }
            }
        }

        match (first, second) {
            (None, None) => None,
            (Some((_, b)), None) => Some(b.clone()),
            // should not happen, but let's be exhaustive
            (None, Some((_, b))) => Some(b.clone()),
            (Some((_, b1)), Some((_, b2))) => {
                if thread_rng().gen_bool(0.5) {
                    Some(b1.clone())
                } else {
                    Some(b2.clone())
                }
            }
        }
    }

}


#[cfg(test)]
mod test {
  use super::*;
  use std::net::{IpAddr, Ipv4Addr, SocketAddr};
  use {BackendStatus, PeakEWMA};
  use retry::{RetryPolicyWrapper, ExponentialBackoffPolicy};
  use sozu_command::proxy::LoadMetric;

  fn create_backend(id: String, connections: Option<usize>) -> Backend {
    Backend {
      sticky_id: None,
      backend_id: id,
      address: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080),
      status: BackendStatus::Normal,
      retry_policy: RetryPolicyWrapper::ExponentialBackoff(ExponentialBackoffPolicy::new(1)),
      active_connections: connections.unwrap_or(0),
      active_requests: 0,
      failures: 0,
      load_balancing_parameters: None,
      backup: false,
      connection_time: PeakEWMA::new(),
    }
  }

  #[test]
  fn it_should_find_the_backend_with_least_connections() {
    let backend_with_least_connection = Rc::new(RefCell::new(create_backend("yolo".to_string(), Some(1))));

    let mut backends = vec![
      Rc::new(RefCell::new(create_backend("nolo".to_string(), Some(10)))),
      Rc::new(RefCell::new(create_backend("philo".to_string(), Some(20)))),
      backend_with_least_connection.clone(),
    ];

    let mut least_connection_algorithm = LeastLoaded{ metric: LoadMetric::Connections };

    let backend_res = least_connection_algorithm.next_available_backend(&mut backends).unwrap();
    let backend = backend_res.borrow();

    assert!(*backend == *backend_with_least_connection.borrow());
  }

  #[test]
  fn it_shouldnt_find_backend_with_least_connections_when_list_is_empty() {
    let mut backends = vec![];

    let mut least_connection_algorithm = LeastLoaded{ metric: LoadMetric::Connections };

    let backend = least_connection_algorithm.next_available_backend(&mut backends);
    assert!(backend.is_none());
  }

  #[test]
  fn it_should_find_backend_with_roundrobin_when_some_backends_were_removed() {
    let mut backends = vec![
      Rc::new(RefCell::new(create_backend("toto".to_string(), None))),
      Rc::new(RefCell::new(create_backend("voto".to_string(), None))),
      Rc::new(RefCell::new(create_backend("yoto".to_string(), None)))
    ];

    let mut roundrobin = RoundRobin { next_backend: 1 };
    let backend = roundrobin.next_available_backend(&mut backends);
    assert_eq!(backend.as_ref(), backends.get(1));

    backends.remove(1);

    let backend2 = roundrobin.next_available_backend(&mut backends);
    assert_eq!(backend2.as_ref(),  backends.get(0));
  }
}
