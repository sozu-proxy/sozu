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