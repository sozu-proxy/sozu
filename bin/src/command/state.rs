use mio::{Ready,Token};
use std::collections::{HashMap,HashSet};

use sozu::messages::Order;
use sozu_command::data::{ConfigMessage,ConfigMessageAnswer};
use command::FrontToken;

pub type ClientMessageId  = String;
pub type WorkerMessageId  = String;
pub type WorkerMessageKey = (WorkerMessageId, usize);

#[derive(Clone,Debug)]
pub struct OrderState {
  pub message_match: HashMap<WorkerMessageKey, ClientMessageId>,
  pub state:         HashMap<ClientMessageId, Task>,
}

impl OrderState {
  pub fn new() -> OrderState {
    OrderState {
      message_match: HashMap::new(),
      state:         HashMap::new(),
    }
  }

  pub fn insert_task(&mut self, client_message_id: &str, client_token: Option<FrontToken>) {
    self.state.insert(String::from(client_message_id), Task::new(String::from(client_message_id), client_token));
  }

  pub fn insert_worker_message(&mut self, client_message_id: &str, worker_message_id: &str, worker_token: Token) {
    let key:WorkerMessageKey = (worker_message_id.to_string(), worker_token.0);

    self.message_match.insert(key.clone(), client_message_id.to_string());
    self.state.get_mut(client_message_id).map(|task| {
      task.processing.insert(key);
    });
  }

  pub fn ok(&mut self, worker_message_id: &str, worker_token: Token) -> Option<ClientMessageId> {
    let key:WorkerMessageKey = (worker_message_id.to_string(), worker_token.0);
    //info!("state::ok: waiting for {} messages", self.message_match.len());

    if let Some(client_message_id) = self.message_match.remove(&key) {
      if let Some(ref mut task) = self.state.get_mut(&client_message_id) {
        task.processing.remove(&key);
        task.ok.insert(key.clone());
        if task.processing.is_empty() {
          return Some(client_message_id);
        }
      }
    }

    None
  }

  pub fn error(&mut self, worker_message_id: &str, worker_token: Token) -> Option<ClientMessageId> {
    let key:WorkerMessageKey = (worker_message_id.to_string(), worker_token.0);
    //info!("state::error: waiting for {} messages", self.message_match.len());

    if let Some(client_message_id) = self.message_match.remove(&key) {
      if let Some(ref mut task) = self.state.get_mut(&client_message_id) {
        task.processing.remove(&key);
        task.error.insert(key.clone());
        if task.processing.is_empty() {
          return Some(client_message_id);
        }
      }
    }

    None
  }

  pub fn task(&mut self, client_message_id: &str) -> Option<Task> {
    self.state.remove(client_message_id)
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
  id:             String,
  pub client:     Option<FrontToken>,
  //state: MessageState,
  pub processing: HashSet<WorkerMessageKey>,
  pub ok:         HashSet<WorkerMessageKey>,
  pub error:      HashSet<WorkerMessageKey>,
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
}

