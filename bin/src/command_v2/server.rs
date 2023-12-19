use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    ops::{Deref, DerefMut},
};

use anyhow::Context;
use libc::pid_t;
use mio::{
    net::{UnixListener, UnixStream},
    Events, Interest, Poll, Token,
};
use sozu_command_lib::{
    channel::Channel,
    config::Config,
    proto::command::{Request, RunState},
    ready::Ready,
    request::WorkerRequest,
    response::WorkerResponse,
    state::ConfigState,
};

use crate::command_v2::sessions::{ClientResult, ClientSession, WorkerResult, WorkerSession};

/// Gather messages and notifies when there are no more to read.
#[allow(unused)]
pub trait Gatherer {
    fn inc_expected_responses(&mut self, count: usize);

    /// Aggregate a response, return true if enough has been gathered
    fn on_message(
        &mut self,
        server: &mut Server,
        client: &mut ClientSession,
        worker_id: u32,
        message: WorkerResponse,
    ) -> bool {
        unimplemented!("The parent task expected no client");
    }

    /// same as on_message but if no client is involved (a CLI, a connector…)
    fn on_message_no_client(
        &mut self,
        server: &mut Server,
        worker_id: u32,
        message: WorkerResponse,
    ) -> bool {
        unimplemented!("The parent task expected a client");
    }
}

/// This trait is used for command that need to wait for worker responses
#[allow(unused)]
pub trait GatheringTask: Debug {
    fn client_token(&self) -> Option<Token>;
    fn get_gatherer(&mut self) -> &mut dyn Gatherer;
    /// This is called once every worker has answered
    /// It allows to operate both on the server (lauch workers...) and the client (send an answer...)
    fn on_finish(self: Box<Self>, server: &mut Server, client: &mut ClientSession) {}
    fn on_finish_no_client(self: Box<Self>, server: &mut Server) {}
}

#[derive(Debug, Default)]
pub struct DefaultGatherer {
    pub responses: Vec<(u32, WorkerResponse)>,
    pub expected_responses: usize,
}

#[allow(unused)]
impl Gatherer for DefaultGatherer {
    fn inc_expected_responses(&mut self, count: usize) {
        self.expected_responses += count;
    }

    fn on_message(
        &mut self,
        server: &mut Server,
        client: &mut ClientSession,
        worker_id: u32,
        message: WorkerResponse,
    ) -> bool {
        self.responses.push((worker_id, message));
        self.responses.len() >= self.expected_responses
    }

    fn on_message_no_client(
        &mut self,
        server: &mut Server,
        worker_id: u32,
        message: WorkerResponse,
    ) -> bool {
        self.responses.push((worker_id, message));
        self.responses.len() >= self.expected_responses
    }
}

/// A platform to receive client connections, pass orders to workers,
/// gather data, etc.
#[derive(Debug)]
pub struct CommandHub {
    pub server: Server,
    clients: HashMap<Token, ClientSession>,
    tasks: HashMap<usize, Box<dyn GatheringTask>>,
}

/// Manages workers
#[derive(Debug)]
pub struct Server {
    in_flight: HashMap<String, usize>,
    next_client_id: u32,
    next_session_token: usize,
    next_task_id: usize,
    poll: Poll,
    queued_tasks: HashMap<usize, Box<dyn GatheringTask>>,
    /// sessions that have business to do on the next tick
    sessions_to_tick: HashSet<Token>,
    unix_listener: UnixListener,
    pub workers: HashMap<Token, WorkerSession>,
    pub state: ConfigState,
    frontends_count: usize,
    backends_count: usize,
    pub config: Config,
}

impl Deref for CommandHub {
    type Target = Server;

    fn deref(&self) -> &Self::Target {
        &self.server
    }
}
impl DerefMut for CommandHub {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.server
    }
}

impl CommandHub {
    pub fn new(unix_listener: UnixListener, config: Config) -> anyhow::Result<Self> {
        Ok(Self {
            server: Server::new(unix_listener, config)?,
            clients: HashMap::new(),
            tasks: HashMap::new(),
        })
    }

    fn get_client_mut(&mut self, token: &Token) -> Option<(&mut Server, &mut ClientSession)> {
        let Self {
            server, clients, ..
        } = self;
        if let Some(client) = clients.get_mut(token) {
            Some((server, client))
        } else {
            None
        }
    }

    pub fn run(&mut self) -> ! {
        let mut events = Events::with_capacity(100);
        println!("{self:#?}");

        loop {
            let CommandHub {
                server:
                    Server {
                        queued_tasks,
                        sessions_to_tick,
                        workers,
                        ..
                    },
                tasks,
                clients,
            } = self;
            tasks.extend(queued_tasks.drain());

            println!("Sessions to tick: {:?}", sessions_to_tick);
            for token in sessions_to_tick.drain() {
                if let Some(client) = clients.get_mut(&token) {
                    client.ready();
                } else if let Some(worker) = workers.get_mut(&token) {
                    worker.ready();
                }
            }

            events.clear();
            match self.poll.poll(&mut events, None) {
                Ok(()) => {}
                Err(error) => error!("Error while polling: {:?}", error),
            }

            for event in &events {
                match event.token() {
                    Token(0) => {
                        if event.is_readable() {
                            while let Ok((mut stream, addr)) = self.unix_listener.accept() {
                                info!("New client connected: {:?}", addr);
                                let token = self.next_session_token();
                                self.register(token, &mut stream);
                                let channel = Channel::new(stream, 4096, usize::MAX);
                                let id = self.next_client_id();
                                let session = ClientSession::new(channel, id, token);
                                info!("{:#?}", session);
                                self.clients.insert(token, session);
                            }
                        }
                    }
                    token => {
                        info!("{:?} got event: {:?}", token, event);
                        if let Some((server, client)) = self.get_client_mut(&token) {
                            client.update_readiness(Ready::from(event));
                            match client.ready() {
                                ClientResult::NothingToDo => {}
                                ClientResult::NewRequest(request) => {
                                    server.handle_request(client, request);
                                }
                                ClientResult::CloseSession => {
                                    println!("Closing client {client:#?}");
                                    self.clients.remove(&token);
                                }
                            }
                        } else if let Some(worker) = self.workers.get_mut(&token) {
                            worker.update_readiness(Ready::from(event));
                            let worker_id = worker.id;
                            match worker.ready() {
                                WorkerResult::NothingToDo => {}
                                WorkerResult::NewResponses(responses) => {
                                    for response in responses {
                                        self.handle_response(worker_id, response);
                                    }
                                }
                                WorkerResult::CloseSession => {
                                    self.workers.remove(&token);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn handle_response(&mut self, worker_id: u32, response: WorkerResponse) {
        let Some(task_id) = self.in_flight.get(&response.id).copied() else {
            println!("==========HANDLE_RESPONSE not in flight: {}", response);
            return;
        };
        let CommandHub {
            server,
            clients,
            tasks,
        } = self;
        let task = tasks.get_mut(&task_id).unwrap();

        if let Some(token) = task.client_token() {
            let client = clients.get_mut(&token).unwrap();
            let is_finished = task
                .get_gatherer()
                .on_message(server, client, worker_id, response);
            if is_finished {
                let task = self.tasks.remove(&task_id).unwrap();
                task.on_finish(server, client);
                self.in_flight.retain(|_, val| *val != task_id);
            }
            self.tick_later(token);
        } else {
            let is_finished =
                task.get_gatherer()
                    .on_message_no_client(&mut self.server, worker_id, response);
            if is_finished {
                let task = self.tasks.remove(&task_id).unwrap();
                task.on_finish_no_client(&mut self.server);
                self.in_flight.retain(|_, val| *val != task_id);
            }
        }
    }
}

impl Server {
    fn new(mut unix_listener: UnixListener, config: Config) -> anyhow::Result<Self> {
        let poll = mio::Poll::new().with_context(|| "Poll::new() failed")?;
        poll.registry()
            .register(
                &mut unix_listener,
                Token(0),
                Interest::READABLE | Interest::WRITABLE,
            )
            .with_context(|| "should register the channel")?;

        Ok(Self {
            in_flight: HashMap::new(),
            next_client_id: 0,
            next_session_token: 1, // 0 is reserved for the UnixListener
            next_task_id: 0,
            poll,
            state: Default::default(),
            queued_tasks: HashMap::new(),
            sessions_to_tick: HashSet::new(),
            unix_listener,
            workers: HashMap::new(),
            frontends_count: 0,
            backends_count: 0,
            config,
        })
    }

    /// count backends and frontends in the cache, update gauge metrics
    pub fn update_counts(&mut self) {
        self.backends_count = self.state.count_backends();
        self.frontends_count = self.state.count_frontends();
        gauge!("configuration.clusters", self.state.clusters.len());
        gauge!("configuration.backends", self.backends_count);
        gauge!("configuration.frontends", self.frontends_count);
    }

    fn next_session_token(&mut self) -> Token {
        let token = Token(self.next_session_token);
        self.next_session_token += 1;
        token
    }
    fn next_client_id(&mut self) -> u32 {
        let id = self.next_client_id;
        self.next_client_id += 1;
        id
    }

    fn next_task_id(&mut self) -> usize {
        let id = self.next_task_id;
        self.next_task_id += 1;
        id
    }

    fn register(&mut self, token: Token, stream: &mut UnixStream) {
        self.poll
            .registry()
            .register(stream, token, Interest::READABLE | Interest::WRITABLE)
            .expect("could not register channel");
    }

    pub fn register_worker(
        &mut self,
        id: u32,
        pid: pid_t,
        mut channel: Channel<WorkerRequest, WorkerResponse>,
    ) {
        let token = self.next_session_token();
        self.register(token, &mut channel.sock);
        self.workers
            .insert(token, WorkerSession::new(channel, id, pid, token));
    }

    /// Add a task in a queue to make it accessible until the next tick
    pub fn new_task(&mut self, task: Box<dyn GatheringTask>) -> usize {
        let task_id = self.next_task_id();
        self.queued_tasks.insert(task_id, task);
        task_id
    }

    pub fn scatter(&mut self, request: Request, task: Box<dyn GatheringTask>) {
        let task_id = self.next_task_id();
        self.queued_tasks.insert(task_id, task);
        self.scatter_on(request, task_id, 0);
    }

    pub fn scatter_on(&mut self, request: Request, task_id: usize, request_id: usize) {
        let Server {
            workers,
            queued_tasks,
            sessions_to_tick,
            ..
        } = self;

        let task = queued_tasks.get_mut(&task_id).unwrap();

        let mut worker_count = 0;
        let mut worker_request = WorkerRequest {
            id: String::new(),
            content: request,
        };
        for worker in workers
            .values_mut()
            .filter(|w| w.run_state == RunState::Running)
        {
            worker_count += 1;
            worker_request.id = format!("{}-{}-query-{}", task_id, request_id, worker.id);
            worker.send(&worker_request);
            self.in_flight.insert(worker_request.id, task_id);
            sessions_to_tick.insert(worker.token);
        }
        task.get_gatherer().inc_expected_responses(worker_count);
    }

    pub fn tick_later(&mut self, token: Token) {
        self.sessions_to_tick.insert(token);
    }
}
