use std::fmt::Debug;

use libc::pid_t;
use mio::Token;
use serde::{de::DeserializeOwned, Serialize};

use sozu_command_lib::{
    channel::Channel,
    proto::command::{
        Request, Response, ResponseContent, ResponseStatus, RunState, WorkerInfo, WorkerRequest,
        WorkerResponse,
    },
    ready::Ready,
    scm_socket::ScmSocket,
};

use crate::command::server::{ClientId, MessageClient, WorkerId};

/// Track a client from start to finish
#[derive(Debug)]
pub struct ClientSession {
    pub channel: Channel<Response, Request>,
    pub id: ClientId,
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
    pub fn new(mut channel: Channel<Response, Request>, id: ClientId, token: Token) -> Self {
        channel.interest = Ready::READABLE | Ready::ERROR | Ready::HUP;
        Self { channel, id, token }
    }

    /// queue a response for the client (the event loop does the send)
    fn send(&mut self, response: Response) {
        if let Err(e) = self.channel.write_message(&response) {
            error!("error writing on channel: {}", e);
            self.channel.readiness = Ready::ERROR;
            return;
        }
        self.channel.interest.insert(Ready::WRITABLE);
    }

    pub fn update_readiness(&mut self, events: Ready) {
        self.channel.handle_events(events);
    }

    /// drive the channel read and write
    pub fn ready(&mut self) -> ClientResult {
        if self.channel.readiness.is_error() || self.channel.readiness.is_hup() {
            return ClientResult::CloseSession;
        }

        let status = self.channel.writable();
        trace!("client writable: {:?}", status);
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

impl MessageClient for ClientSession {
    fn finish_ok<T: Into<String>>(&mut self, message: T) {
        let message = message.into();
        info!("{}", message);
        self.send(Response {
            status: ResponseStatus::Ok.into(),
            message,
            content: None,
        })
    }

    fn finish_ok_with_content<T: Into<String>>(&mut self, content: ResponseContent, message: T) {
        let message = message.into();
        info!("{}", message);
        self.send(Response {
            status: ResponseStatus::Ok.into(),
            message,
            content: Some(content),
        })
    }

    fn finish_failure<T: Into<String>>(&mut self, message: T) {
        let message = message.into();
        error!("{}", message);
        self.send(Response {
            status: ResponseStatus::Failure.into(),
            message,
            content: None,
        })
    }

    fn return_processing<S: Into<String>>(&mut self, message: S) {
        let message = message.into();
        info!("{}", message);
        self.send(Response {
            status: ResponseStatus::Processing.into(),
            message,
            content: None,
        });
    }

    fn return_processing_with_content<S: Into<String>>(
        &mut self,
        message: S,
        content: ResponseContent,
    ) {
        let message = message.into();
        info!("{}", message);
        self.send(Response {
            status: ResponseStatus::Processing.into(),
            message,
            content: Some(content),
        });
    }
}

pub type OptionalClient<'a> = Option<&'a mut ClientSession>;

impl MessageClient for OptionalClient<'_> {
    fn finish_ok<T: Into<String>>(&mut self, message: T) {
        match self {
            None => info!("{}", message.into()),
            Some(client) => client.finish_ok(message),
        }
    }

    fn finish_ok_with_content<T: Into<String>>(&mut self, content: ResponseContent, message: T) {
        match self {
            None => info!("{}", message.into()),
            Some(client) => client.finish_ok_with_content(content, message),
        }
    }

    fn finish_failure<T: Into<String>>(&mut self, message: T) {
        match self {
            None => error!("{}", message.into()),
            Some(client) => client.finish_failure(message),
        }
    }

    fn return_processing<T: Into<String>>(&mut self, message: T) {
        match self {
            None => info!("{}", message.into()),
            Some(client) => client.return_processing(message),
        }
    }

    fn return_processing_with_content<S: Into<String>>(
        &mut self,
        message: S,
        content: ResponseContent,
    ) {
        match self {
            None => info!("{}", message.into()),
            Some(client) => client.return_processing_with_content(message, content),
        }
    }
}

/// Follow a worker throughout its lifetime (launching, communitation, softstop/hardstop)
#[derive(Debug)]
pub struct WorkerSession {
    pub channel: Channel<WorkerRequest, WorkerResponse>,
    pub id: WorkerId,
    pub pid: pid_t,
    pub run_state: RunState,
    /// meant to send listeners to the worker upon start
    pub scm_socket: ScmSocket,
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
        id: WorkerId,
        pid: pid_t,
        token: Token,
        scm_socket: ScmSocket,
    ) -> Self {
        channel.interest = Ready::READABLE | Ready::ERROR | Ready::HUP;
        Self {
            channel,
            id,
            pid,
            run_state: RunState::Running,
            scm_socket,
            token,
        }
    }

    /// queue a request for the worker (the event loop does the send)
    pub fn send(&mut self, request: &WorkerRequest) {
        trace!("Sending to worker: {:?}", request);
        if let Err(e) = self.channel.write_message(request) {
            error!("Could not send request to worker: {}", e);
            self.channel.readiness = Ready::ERROR;
            return;
        }
        self.channel.interest.insert(Ready::WRITABLE);
    }

    pub fn update_readiness(&mut self, events: Ready) {
        self.channel.handle_events(events);
    }

    /// drive the channel read and write
    pub fn ready(&mut self) -> WorkerResult {
        let status = self.channel.writable();
        trace!("Worker writable: {:?}", status);
        let responses = extract_messages(&mut self.channel);
        if !responses.is_empty() {
            return WorkerResult::NewResponses(responses);
        }

        if self.channel.readiness.is_error() || self.channel.readiness.is_hup() {
            debug!("worker {} is unresponsive, closing the session", self.id);
            return WorkerResult::CloseSession;
        }

        WorkerResult::NothingToDo
    }

    /// get the run state of the worker (defaults to NotAnswering)
    pub fn querying_info(&self) -> WorkerInfo {
        let run_state = match self.run_state {
            RunState::Stopping => RunState::Stopping,
            RunState::Stopped => RunState::Stopped,
            RunState::Running | RunState::NotAnswering => RunState::NotAnswering,
        };
        WorkerInfo {
            id: self.id,
            pid: self.pid,
            run_state: run_state as i32,
        }
    }

    pub fn is_active(&self) -> bool {
        self.run_state != RunState::Stopping && self.run_state != RunState::Stopped
    }
}

/// read and parse messages (Requests or Responses) from the channel
pub fn extract_messages<Tx, Rx>(channel: &mut Channel<Tx, Rx>) -> Vec<Rx>
where
    Tx: Debug + Serialize,
    Rx: Debug + DeserializeOwned,
{
    let mut messages = Vec::new();
    loop {
        let status = channel.readable();
        trace!("Channel readable: {:?}", status);
        let old_capacity = channel.front_buf.capacity();
        let message = channel.read_message();
        match message {
            Ok(message) => messages.push(message),
            Err(_) => {
                if old_capacity == channel.front_buf.capacity() {
                    return messages;
                }
            }
        }
    }
}

/// used by the event loop to know wether to call ready on a session,
/// given the state of its channel
pub fn wants_to_tick<Tx, Rx>(channel: &Channel<Tx, Rx>) -> bool {
    (channel.readiness.is_writable() && channel.back_buf.available_data() > 0)
        || (channel.readiness.is_hup() || channel.readiness.is_error())
}
