use mio::{Ready,Token};
use std::collections::{HashMap,HashSet};

use sozu::messages::Order;
use sozu_command::data::{ConfigMessage,ConfigMessageAnswer};
use command::FrontToken;

#[derive(Clone,Debug)]
pub struct OrderState {
  pub state: HashMap<String, HashSet<usize>>,
}

impl OrderState {
  pub fn new() -> OrderState {
    OrderState {
      state: HashMap::new()
    }
  }

  pub fn remove(&mut self, id: &str, token: Token) -> bool {
    if let Some(ref mut workers) = self.state.get_mut(id) {
      workers.remove(&token.0);
    }

    if self.state.get(id).map(|set| set.len()).unwrap_or(0) == 0 {
      self.state.remove(id);
      true
    } else {
      false
    }
  }

  pub fn insert(&mut self, id: &str, token: Token) {
    self.state.entry(String::from(id)).or_insert(HashSet::new()).insert(token.0);
  }
}

/*
pub struct Task {
  Status,
}*/

