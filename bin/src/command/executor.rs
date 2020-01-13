use mio::Token;
use futures::prelude::*;
use futures::executor::{Notify, Spawn, spawn};
use futures::task::{self, Task};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use slab::Slab;
use std::collections::{HashSet, VecDeque};
use sozu_command::proxy::{ProxyRequest, ProxyResponse, ProxyResponseStatus};
use sozu_command::command::CommandResponse;
use super::FrontToken;

lazy_static! {
  static ref EXECUTOR: Arc<Executor> = {
    Arc::new(Executor {
      to_notify: Mutex::new(HashMap::new()),
      inner: Mutex::new(Runner::new()),
      messages: Mutex::new(HashMap::new()),
      worker_queue: Mutex::new(VecDeque::new()),
      client_queue: Mutex::new(VecDeque::new()),
      state_queue: Mutex::new(VecDeque::new()),
    })
  };
}

pub struct Executor {
  pub inner: Mutex<Runner>,
  pub to_notify: Mutex<HashMap<(Token, String, MessageStatus), Task>>,
  pub messages: Mutex<HashMap<(Token, String, MessageStatus), ProxyResponse>>,
  pub worker_queue: Mutex<VecDeque<(Token, ProxyRequest)>>,
  pub client_queue: Mutex<VecDeque<(FrontToken, CommandResponse)>>,
  pub state_queue: Mutex<VecDeque<StateChange>>,
}

pub struct Runner {
  pub ready: HashSet<usize>,
  pub tasks: Slab<Spawn<Box<dyn Future<Item = (), Error = ()> + Send>>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageStatus {
  Processing,
  Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum StateChange {
  StopWorker(Token),
  StopMaster,
}

impl Runner {
  pub fn new() -> Runner {
    Runner {
      ready: HashSet::new(),
      tasks: Slab::with_capacity(10000),
    }
  }

  pub fn run(&mut self) {
    for id in self.ready.drain() {
      //println!("run({})", id);
      if self.tasks.contains(id) {
        //println!("tasks has id: {}", id);
        match self.tasks[id].poll_future_notify(&(EXECUTOR.clone()), id) {
          Err(e) => {
            error!("error executing future[{}]: {:?}", id, e);
            self.tasks.remove(id);
          },
          Ok(Async::Ready(())) => {
            trace!("finished executing future[{}]", id);
            self.tasks.remove(id);
          },
          Ok(Async::NotReady) => {
            //println!("not ready");
          },
        }
      }
    }

  }
}

impl Executor {
  pub fn register(worker: Token, message_id: &str, status: MessageStatus, task: Task) {
    //println!("register({:?}, {}, {:?})", worker, message_id, task);
    let mut to_notify = EXECUTOR.to_notify.lock().unwrap();
    to_notify.insert((worker, message_id.to_string(), status), task);
  }

  pub fn send_worker(worker: Token, message: ProxyRequest) {
    let mut queue = EXECUTOR.worker_queue.lock().unwrap();
    queue.push_back((worker, message));
  }

  pub fn get_worker_message() -> Option<(Token, ProxyRequest)> {
    let mut queue = EXECUTOR.worker_queue.lock().unwrap();
    queue.pop_front()
  }

  pub fn send_client(client: FrontToken, message: CommandResponse) {
    let mut queue = EXECUTOR.client_queue.lock().unwrap();
    queue.push_back((client, message));
  }

  pub fn get_client_message() -> Option<(FrontToken, CommandResponse)> {
    let mut queue = EXECUTOR.client_queue.lock().unwrap();
    queue.pop_front()
  }

  pub fn stop_worker(worker: Token) {
    let mut queue = EXECUTOR.state_queue.lock().unwrap();
    queue.push_back(StateChange::StopWorker(worker));
  }

  pub fn stop_master() {
    let mut queue = EXECUTOR.state_queue.lock().unwrap();
    queue.push_back(StateChange::StopMaster);
  }

  pub fn get_state_change() -> Option<StateChange> {
    let mut queue = EXECUTOR.state_queue.lock().unwrap();
    queue.pop_front()
  }

  pub fn handle_message(worker: Token, message: ProxyResponse) {
    trace!("executor handle_message({:?}, {}, {:?})", worker, message.id, message);

    let status = match message.status {
      ProxyResponseStatus::Processing => MessageStatus::Processing,
      _ => MessageStatus::Other
    };

    let task = {
      let mut to_notify = EXECUTOR.to_notify.lock().unwrap();

      match to_notify.remove(&(worker, message.id.to_string(), status)) {
        None => {
          trace!("no task waiting for {}", message.id);
          None
        },
        Some(task) => {
          Some(task.clone())
        }
      }
    };

    {
      let mut messages = EXECUTOR.messages.lock().unwrap();
      trace!("inserting message({:?}, {}", worker, message.id);
      messages.insert((worker, message.id.to_string(), status), message);
    }

    if let Some(t) = task {
      t.notify();
    }
  }

  pub fn execute(s: impl Future<Item = (), Error = ()> + Send + 'static) {
    let mut inner = EXECUTOR.inner.lock().unwrap();
    if let Ok(id) = inner.tasks.insert(spawn(Box::new(s))) {
      inner.ready.insert(id);
    }
  }

  pub fn run() {
    let mut inner = EXECUTOR.inner.lock().unwrap();
    inner.run();
  }

  pub fn get_message(worker: Token, message_id: &str, status: MessageStatus) -> Option<ProxyResponse> {
    {
      let mut messages = EXECUTOR.messages.lock().unwrap();
      messages.remove(&(worker, message_id.to_string(), status))
    }
  }

  pub fn peek_message(worker: Token, message_id: &str, status: MessageStatus) -> bool {
    {
      let messages = EXECUTOR.messages.lock().unwrap();
      messages.contains_key(&(worker, message_id.to_string(), status))
    }
  }
}

impl Notify for Executor {
  fn notify(&self, id: usize) {
    let mut inner = self.inner.lock().unwrap();
    inner.ready.insert(id);
  }
}

pub struct FutureAnswer {
  worker_id: Token,
  message_id: String,
}

impl Future for FutureAnswer {
  type Item  = ProxyResponse;
  type Error = String;
  fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
    //FIXME: handle workers disconnected
    match Executor::get_message(self.worker_id, &self.message_id, MessageStatus::Other) {
      Some(message) => {
        match message.status {
          ProxyResponseStatus::Ok => Ok(Async::Ready(message)),
          ProxyResponseStatus::Error(_) => Ok(Async::Ready(message)),
          _ => panic!(),
        }
      },
      None => {
        Executor::register(self.worker_id, &self.message_id, MessageStatus::Other, task::current());
        Ok(Async::NotReady)
      }
    }
  }
}

pub struct FutureProcessing {
  worker_id: Token,
  message_id: String,
}

impl Stream for FutureProcessing {
  type Item  = ProxyResponse;
  type Error = ();
  fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
    //FIXME: handle workers disconnected
    if Executor::peek_message(self.worker_id, &self.message_id, MessageStatus::Other) {
      return Ok(Async::Ready(None));
    } else {
      Executor::register(self.worker_id, &self.message_id, MessageStatus::Other, task::current());
    }

    match Executor::get_message(self.worker_id, &self.message_id, MessageStatus::Processing) {
      Some(message) => {
        match message.status {
          ProxyResponseStatus::Processing => Ok(Async::Ready(Some(message))),
          _ => panic!(),
        }
      },
      None => {
        Executor::register(self.worker_id, &self.message_id, MessageStatus::Processing, task::current());
        Ok(Async::NotReady)
      }
    }
  }
}

pub fn send_processing(worker_id: Token, message: ProxyRequest) -> (FutureProcessing, FutureAnswer) {
  let message_id = message.id.to_string();
  Executor::send_worker(worker_id, message);
  (FutureProcessing {
    worker_id,
    message_id: message_id.clone()
  },

  FutureAnswer {
    worker_id,
    message_id
  })
}

pub fn send(worker_id: Token, message: ProxyRequest) -> FutureAnswer {
  let message_id = message.id.to_string();
  Executor::send_worker(worker_id, message);
  FutureAnswer {
    worker_id,
    message_id,
  }
}

#[cfg(test)]
mod tests {
  use mio::Token;
  use super::*;
  use futures::executor::spawn;
  use futures::task;
  use futures::future::{lazy, result};
  use sozu_command::proxy::{ProxyRequestData,ProxyResponseStatus};
  use sozu_command::command::CommandStatus;

  #[test]
  fn executor() {
    Executor::execute(lazy(||{
      let (processing, msg_future) = send_processing(Token(0), ProxyRequest { id: "test".to_string(), order: ProxyRequestData::Status });
      processing.for_each(|msg| {
        println!("TEST: got processing message: {:?}", msg);
        Ok(())
      }).join(msg_future.map(|msg| {
          println!("TEST: future got msg: {:?}", msg);
          Executor::send_client(FrontToken(1), CommandResponse::new(
            "test".to_string(),
            CommandStatus::Ok,
            "ok".to_string(),
            None
          ));
        }).map_err(|e| {
          println!("TEST: got error: {:?}", e);
        })
      ).map(|_| ())
    }));
    Executor::run();

    Executor::handle_message(Token(0), ProxyResponse{
      id: "test".to_string(),
      status: ProxyResponseStatus::Processing,
      data: None
    });

    Executor::run();

    Executor::handle_message(Token(0), ProxyResponse{
      id: "test".to_string(),
      status: ProxyResponseStatus::Ok,
      data: None
    });

    Executor::run();

    assert_eq!(
      Executor::get_client_message(),
      Some((FrontToken(1),
        CommandResponse::new(
          "test".to_string(),
          CommandStatus::Ok,
          "ok".to_string(),
          None
        )
      ))
    );
  }
}

