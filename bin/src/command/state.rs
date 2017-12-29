use mio::Token;
use std::collections::{BTreeMap,HashMap,HashSet};

use sozu::network::metrics::METRICS;
use sozu_command::messages::{MetricsData,OrderMessageAnswerData,QueryAnswer,QueryApplicationType};
use sozu_command::state::{ConfigState,get_application_ids_by_domain};
use sozu_command::data::AnswerData;
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
  LoadState,
  WorkerOrder,
  Metrics,
  QueryApplicationsHashes,
  QueryApplications(QueryApplicationType),
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

  pub fn generate_data(self, master_state: &ConfigState) -> Option<AnswerData> {
    trace!("state generate data: type={:?}, data = {:#?}", self.message_type, self.data);
    match self.message_type {
      MessageType::Metrics => {
        let mut data: BTreeMap<String, MetricsData> = self.data.into_iter().filter_map(|(tag, metrics)| {
           if let OrderMessageAnswerData::Metrics(d) = metrics {
             Some((tag, d))
           } else {
             None
           }
        }).collect();
        let master_metrics = METRICS.with(|metrics| {
          (*metrics.borrow_mut()).dump_metrics_data()
        });
        data.insert(String::from("master"), master_metrics);
        Some(AnswerData::Metrics(data))
      },
      MessageType::QueryApplicationsHashes => {
        let mut data: BTreeMap<String, QueryAnswer> = self.data.into_iter().filter_map(|(tag, query)| {
          if let OrderMessageAnswerData::Query(data) = query {
            Some((tag, data))
          } else {
            None
          }
        }).collect();

        data.insert(String::from("master"), QueryAnswer::ApplicationsHashes(master_state.hash_state()));

        Some(AnswerData::Query(data))
      },
      MessageType::QueryApplications(query_type) => {
        let mut data: BTreeMap<String, QueryAnswer> = self.data.into_iter().filter_map(|(tag, query)| {
          if let OrderMessageAnswerData::Query(data) = query {
            Some((tag, data))
          } else {
            None
          }
        }).collect();

        let answer = match query_type {
          QueryApplicationType::AppId(ref app_id) => vec!(master_state.application_state(app_id)),
          QueryApplicationType::Domain(ref domain) => {
            let app_ids = get_application_ids_by_domain(&master_state, domain.hostname.clone(), domain.path_begin.clone());
            app_ids.iter().map(|ref app_id| master_state.application_state(app_id)).collect()
          }
        };

        data.insert(String::from("master"), QueryAnswer::Applications(answer));

        Some(AnswerData::Query(data))
      },

      _ => None,
    }
  }
}

