use mio::Token;
use std::collections::{HashMap,HashSet};

use sozu_command::data::ConfigMessage;

#[derive(Clone,Debug)]
pub struct InflightOrders {
  pub state: HashMap<String, HashSet<usize>>,
}

impl InflightOrders {
  pub fn new() -> InflightOrders {
    InflightOrders {
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

pub struct OrderState {
  pub message: ConfigMessage,
  pub workers: HashSet<usize>,
}

impl OrderState {


}

