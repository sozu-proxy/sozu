use rand::random;

use network::Backend;
use sozu_command::config::LoadBalancingAlgorithms;

use std::{ rc::Rc, cell::RefCell };
use std::fmt::Debug;

pub trait LoadBalacingAlg: Debug {
  fn next_available_backend<'a>(&mut self, backends: &'a mut Vec<Rc<RefCell<Backend>>>) -> Option<&'a mut Rc<RefCell<Backend>>>;
}

#[derive(Debug)]
pub struct RoundRobinAlg {
  pub next_backend: u32,
}

impl LoadBalacingAlg for RoundRobinAlg {

  fn next_available_backend<'a>(&mut self , backends: &'a mut Vec<Rc<RefCell<Backend>>>) -> Option<&'a mut Rc<RefCell<Backend>>> {
    let sz = backends.len() as u32;
    let res = backends.get_mut(self.next_backend as usize);

    self.next_backend = if self.next_backend + 1 == sz {
      0
    } else {
      self.next_backend + 1
    };

    res
  }

}

impl RoundRobinAlg {

  fn new() -> Self {
    Self {
      next_backend: 0,
    }
  }

}

#[derive(Debug)]
pub struct RandAlg;

impl LoadBalacingAlg for RandAlg {

  fn next_available_backend<'a>(&mut self, backends: &'a mut Vec<Rc<RefCell<Backend>>>) -> Option<&'a mut Rc<RefCell<Backend>>> {
    let rnd = random::<usize>();
    let idx = rnd % backends.len();

    backends.get_mut(idx)
  }

}