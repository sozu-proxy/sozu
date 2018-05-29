use rand::random;

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
    let sz = backends.len() as u32;
    let res = backends.get(self.next_backend as usize)
                      .map(|backend| (*backend).clone());

    self.next_backend = if self.next_backend + 1 == sz {
      0
    } else {
      self.next_backend + 1
    };

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
    let rnd = random::<usize>();
    let idx = rnd % backends.len();

    backends.get(idx)
            .map(|backend| (*backend).clone())
  }

}