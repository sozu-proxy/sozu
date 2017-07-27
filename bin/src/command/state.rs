use mio::{Ready,Token};
use std::collections::{HashMap,HashSet};

use sozu::messages::Order;
use sozu_command::data::{ConfigMessage,ConfigMessageAnswer};
use command::FrontToken;

#[derive(Clone,Debug)]
pub struct OrderState {
  pub state: HashMap<String, Task>,
}

impl OrderState {
  pub fn new() -> OrderState {
    OrderState {
      state: HashMap::new()
    }
  }

 /* pub fn remove(&mut self, id: &str, token: Token) -> bool {
    if let Some(ref mut workers) = self.state.get_mut(id) {
      workers.remove(&token.0);
    }

    if self.state.get(id).map(|set| set.len()).unwrap_or(0) == 0 {
      self.state.remove(id);
      true
    } else {
      false
    }
  }*/

  pub fn insert(&mut self, id: &str, client_token: Option<FrontToken>, worker_token: Token) {
    self.state.entry(String::from(id)).or_insert(Task::new(String::from(id), client_token)).insert(worker_token.0);
  }

  pub fn ok(&mut self, id: &str, token: Token) -> bool {
    if let Some(ref mut task) = self.state.get_mut(id) {
      task.processing.remove(&token.0);
      task.ok.insert(token.0);
      task.processing.is_empty()
    } else {
      false
    }
  }

  pub fn error(&mut self, id: &str, token: Token) -> bool {
    if let Some(ref mut task) = self.state.get_mut(id) {
      task.processing.remove(&token.0);
      task.error.insert(token.0);
      task.processing.is_empty()
    } else {
      false
    }

  }

  pub fn task(&mut self, id: &str) -> Option<Task> {
    self.state.remove(id)
  }
}

#[derive(Clone,Debug)]
pub enum MessageType {
  Status,
  LoadState,
  SoftStop,
  Config,
}

#[derive(Clone,Debug)]
pub enum MessageState {
  Status(HashSet<usize>),
  LoadState(HashSet<usize>),
  SoftStop(HashSet<usize>),
  Config(HashSet<usize>),
}

#[derive(Clone,Debug)]
pub struct Task {
  id:         String,
  pub client:     Option<FrontToken>,
  //state: MessageState,
  pub processing: HashSet<usize>,
  pub ok:         HashSet<usize>,
  pub error:      HashSet<usize>,
}

impl Task {
  pub fn new(id: String, client: Option<FrontToken>) -> Task {
    Task {
      id:         id,
      client:     client,
      //state: state,
      processing: HashSet::new(),
      ok:         HashSet::new(),
      error:      HashSet::new(),
    }
  }

  pub fn insert(&mut self, token: usize) {
    self.processing.insert(token);
  }
}

