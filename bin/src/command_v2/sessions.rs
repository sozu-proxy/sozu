use std::fmt::Debug;

use libc::pid_t;
use mio::Token;
use serde::{de::DeserializeOwned, Serialize};
use sozu_command_lib::{
    channel::Channel,
    proto::command::{Request, Response, ResponseContent, ResponseStatus, RunState},
    ready::Ready,
    request::WorkerRequest,
    response::WorkerResponse,
};

/// Follows a client request from start to finish
#[derive(Debug)]
pub struct ClientSession {
    pub channel: Channel<Response, Request>,
    pub id: u32,
    pub token: Token,
}

/// The return type of the ready method
#[derive(Debug)]
pub enum ClientResult {
    NothingToDo,
    NewRequest(Request),
    CloseSession,
}

impl ClientSession {
    pub fn new(mut channel: Channel<Response, Request>, id: u32, token: Token) -> Self {
        channel.interest = Ready::READABLE | Ready::ERROR | Ready::HUP;
        Self { channel, id, token }
    }

    fn finish(&mut self, response: Response) {
        println!("Writing message on client channel");
        self.channel.write_message(&response).unwrap();
        self.channel.interest.insert(Ready::WRITABLE);
    }

    pub fn finish_ok(&mut self, content: Option<ResponseContent>) {
        self.finish(Response {
            status: ResponseStatus::Ok.into(),
            message: "Successfully handled request".to_owned(),
            content,
        })
    }

    pub fn finish_failure<T: Into<String>>(&mut self, error: T) {
        self.finish(Response {
            status: ResponseStatus::Failure.into(),
            message: error.into(),
            content: None,
        })
    }

    pub fn return_processing<S: Into<String>>(&mut self, message: S) {
        let message = message.into();
        println!("Return: {}", message);
        let message = Response {
            status: ResponseStatus::Processing.into(),
            message,
            content: None,
        };
        self.channel.write_message(&message).unwrap();
        self.channel.interest.insert(Ready::WRITABLE);
    }

    pub fn update_readiness(&mut self, events: Ready) {
        self.channel.handle_events(events);
    }

    pub fn ready(&mut self) -> ClientResult {
        if self.channel.readiness.is_error() || self.channel.readiness.is_hup() {
            return ClientResult::CloseSession;
        }

        let status = self.channel.writable();
        println!("Client writable: {status:?}");
        let mut requests = extract_messages(&mut self.channel);
        match requests.pop() {
            Some(request) => {
                if !requests.is_empty() {
                    error!("more than one request at a time");
                }
                ClientResult::NewRequest(request)
            }
            None => ClientResult::NothingToDo,
        }
    }
}

/// Follow a worker thoughout its lifetime (launching, communitation, softstop/hardstop)
#[derive(Debug)]
pub struct WorkerSession {
    pub channel: Channel<WorkerRequest, WorkerResponse>,
    pub id: u32,
    pub pid: pid_t,
    pub run_state: RunState,
    pub token: Token,
}

/// The return type of the ready method
#[derive(Debug)]
pub enum WorkerResult {
    NothingToDo,
    NewResponses(Vec<WorkerResponse>),
    CloseSession,
}

impl WorkerSession {
    pub fn new(
        mut channel: Channel<WorkerRequest, WorkerResponse>,
        id: u32,
        pid: pid_t,
        token: Token,
    ) -> Self {
        channel.interest = Ready::READABLE | Ready::ERROR | Ready::HUP;
        Self {
            channel,
            id,
            pid,
            run_state: RunState::Running,
            token,
        }
    }

    pub fn send(&mut self, message: &WorkerRequest) {
        println!("Sending to worker: {message:?}");
        self.channel.write_message(message).unwrap();
        self.channel.interest.insert(Ready::WRITABLE);
    }

    pub fn update_readiness(&mut self, events: Ready) {
        self.channel.handle_events(events);
    }

    pub fn ready(&mut self) -> WorkerResult {
        if self.channel.readiness.is_error() || self.channel.readiness.is_hup() {
            return WorkerResult::CloseSession;
        }

        let status = self.channel.writable();
        println!("Worker writable: {status:?}");
        let responses = extract_messages(&mut self.channel);
        if responses.is_empty() {
            WorkerResult::NothingToDo
        } else {
            WorkerResult::NewResponses(responses)
        }
    }
}

fn extract_messages<Tx, Rx>(channel: &mut Channel<Tx, Rx>) -> Vec<Rx>
where
    Tx: Debug + Serialize,
    Rx: Debug + DeserializeOwned,
{
    let mut messages = Vec::new();
    loop {
        let status = channel.readable();
        println!("Channel readable: {status:?}");
        let old_capacity = channel.front_buf.capacity();
        let message = channel.read_message();
        match message {
            Ok(message) => messages.push(message),
            Err(error) => {
                error!("Channel read_message: {:?}", error);
                if old_capacity == channel.front_buf.capacity() {
                    return messages;
                }
            }
        }
    }
}
