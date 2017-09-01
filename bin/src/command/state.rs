use mio::{Ready,Token};
use std::collections::{BTreeMap,HashMap,HashSet};

use sozu::network::metrics::METRICS;
use sozu_command::messages::{FilteredData,Order,OrderMessageAnswerData};
use sozu_command::data::{AnswerData,ConfigMessage,ConfigMessageAnswer};
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

  pub fn insert_task(&mut self, client_message_id: &str, message_type: MessageType, client_token: Option<FrontToken>) {
    self.state.insert(String::from(client_message_id), Task::new(String::from(client_message_id), message_type, client_token));
  }

  pub fn insert_worker_message(&mut self, client_message_id: &str, worker_message_id: &str, worker_token: Token) {
    let key:WorkerMessageKey = (worker_message_id.to_string(), worker_token.0);

    self.message_match.insert(key.clone(), client_message_id.to_string());
    self.state.get_mut(client_message_id).map(|task| {
      task.processing.insert(key);
    });
  }

  pub fn ok(&mut self, worker_message_id: &str, worker_token: Token, tag: String, data: Option<OrderMessageAnswerData>) -> Option<Task> {
    let key:WorkerMessageKey = (worker_message_id.to_string(), worker_token.0);
    //info!("state::ok: waiting for {} messages", self.message_match.len());

    if let Some(client_message_id) = self.message_match.remove(&key) {
      let should_return = self.state.get_mut(&client_message_id).map(move |ref mut task| {
        task.processing.remove(&key);
        task.ok.insert(key.clone());
        if let Some(d) = data {
          task.data.insert(tag, d);
        }
        task.processing.is_empty()
      }).unwrap_or(false);

      if should_return {
        return Some(self.state.remove(&client_message_id).expect("there should be a task here"));
      }
    }

    None
  }

  pub fn error(&mut self, worker_message_id: &str, worker_token: Token, tag: String, data: Option<OrderMessageAnswerData>) -> Option<Task> {
    let key:WorkerMessageKey = (worker_message_id.to_string(), worker_token.0);
    //info!("state::error: waiting for {} messages", self.message_match.len());

    if let Some(client_message_id) = self.message_match.remove(&key) {
      let should_return = self.state.get_mut(&client_message_id).map(move |ref mut task| {
        task.processing.remove(&key);
        task.error.insert(key.clone());

        if let Some(d) = data {
          task.data.insert(tag, d);
        }

        task.processing.is_empty()
      }).unwrap_or(false);

      if should_return {
        return Some(self.state.remove(&client_message_id).expect("there should be a task here"))
      }
    }

    None
  }
}

#[derive(Clone,Debug,PartialEq)]
pub enum MessageType {
  LaunchWorker,
  LoadState,
  WorkerOrder,
  Metrics,
  Stop,
}

#[derive(Clone,Debug)]
pub struct Task {
  pub id:         String,
  pub client:     Option<FrontToken>,
  pub message_type: MessageType,
  pub processing: HashSet<WorkerMessageKey>,
  pub ok:         HashSet<WorkerMessageKey>,
  pub error:      HashSet<WorkerMessageKey>,
  pub data:       BTreeMap<String,OrderMessageAnswerData>,
}

impl Task {
  pub fn new(id: String, message_type: MessageType, client: Option<FrontToken>) -> Task {
    Task {
      id:           id,
      client:       client,
      message_type: message_type,
      processing:   HashSet::new(),
      ok:           HashSet::new(),
      error:        HashSet::new(),
      data:         BTreeMap::new(),
    }
  }

  pub fn generate_data(mut self) -> Option<AnswerData> {
    match self.message_type {
      MessageType::Metrics => {
        let mut data: BTreeMap<String, BTreeMap<String, FilteredData>> = self.data.into_iter().map(|(tag, metrics)| {
          let OrderMessageAnswerData::Metrics(d) = metrics;
          (tag, d)
        }).collect();
        let master_metrics = METRICS.with(|metrics| {
          (*metrics.borrow()).dump_data()
        });
        data.insert(String::from("master"), master_metrics);
        Some(AnswerData::Metrics(data))
      },
      _ => None,
    }
  }
}

