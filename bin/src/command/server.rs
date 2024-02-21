use std::{
    collections::{HashMap, HashSet},
    fmt::{self, Debug},
    io::Error as IoError,
    ops::{Deref, DerefMut},
    os::fd::{AsRawFd, FromRawFd},
    time::{Duration, Instant},
};

use libc::pid_t;
use mio::{
    net::{UnixListener, UnixStream},
    Events, Interest, Poll, Token,
};
use nix::{
    sys::signal::{kill, Signal},
    unistd::Pid,
};

use sozu_command_lib::{
    channel::Channel,
    config::Config,
    proto::command::{
        request::RequestType, response_content::ContentType, Request, ResponseContent,
        ResponseStatus, RunState, Status, WorkerRequest, WorkerResponse,
    },
    ready::Ready,
    scm_socket::{Listeners, ScmSocket, ScmSocketError},
    state::ConfigState,
};

use crate::{
    command::{
        sessions::{
            wants_to_tick, ClientResult, ClientSession, OptionalClient, WorkerResult, WorkerSession,
        },
        upgrade::UpgradeData,
    },
    util::{disable_close_on_exec, enable_close_on_exec, get_executable_path, UtilError},
    worker::{fork_main_into_worker, WorkerError},
};

use super::upgrade::SerializedWorkerSession;

pub type ClientId = u32;
pub type SessionId = usize;
pub type TaskId = usize;
pub type WorkerId = u32;
pub type RequestId = String;

/// Gather messages and notifies when there are no more left to read.
#[allow(unused)]
pub trait Gatherer {
    /// increment how many responses we expect
    fn inc_expected_responses(&mut self, count: usize);

    /// Return true if enough responses has been gathered
    fn has_finished(&self) -> bool;

    /// Aggregate a response
    fn on_message(
        &mut self,
        server: &mut Server,
        client: &mut OptionalClient,
        worker_id: WorkerId,
        message: WorkerResponse,
    );
}

/// Must be satisfied by commands that need to wait for worker responses
#[allow(unused)]
pub trait GatheringTask: Debug {
    /// get access to the client that sent the command (if any)
    fn client_token(&self) -> Option<Token>;

    /// get access to the gatherer for this task (each task can implement its own gathering strategy)
    fn get_gatherer(&mut self) -> &mut dyn Gatherer;

    /// This is called once every worker has answered
    /// It allows to operate both on the server (launch workers...) and the client (send an answer...)
    fn on_finish(
        self: Box<Self>,
        server: &mut Server,
        client: &mut OptionalClient,
        timed_out: bool,
    );
}

/// Implemented by all objects that can behave like a client (for instance: notify of processing request)
pub trait MessageClient {
    /// return an OK to the client
    fn finish_ok<T: Into<String>>(&mut self, message: T);

    /// return response content to the client
    fn finish_ok_with_content<T: Into<String>>(&mut self, content: ResponseContent, message: T);

    /// return failure to the client
    fn finish_failure<T: Into<String>>(&mut self, message: T);

    /// notify the client about an ongoing task
    fn return_processing<T: Into<String>>(&mut self, message: T);

    /// transmit response content to the client, even though a task is not finished
    fn return_processing_with_content<S: Into<String>>(
        &mut self,
        message: S,
        content: ResponseContent,
    );
}

/// A timeout for the tasks of the main process server
pub enum Timeout {
    None,
    Default,
    #[allow(unused)]
    Custom(Duration),
}

/// Contains a task and its execution timeout
#[derive(Debug)]
struct TaskContainer {
    job: Box<dyn GatheringTask>,
    timeout: Option<Instant>,
}

/// Default strategy when gathering responses from workers
#[derive(Debug, Default)]
pub struct DefaultGatherer {
    /// number of OK responses received from workers
    pub ok: usize,
    /// number of failures received from workers
    pub errors: usize,
    /// worker responses are accumulated here
    pub responses: Vec<(WorkerId, WorkerResponse)>,
    /// number of expected responses, excluding processing responses
    pub expected_responses: usize,
}

#[allow(unused)]
impl Gatherer for DefaultGatherer {
    fn inc_expected_responses(&mut self, count: usize) {
        self.expected_responses += count;
    }

    fn has_finished(&self) -> bool {
        self.ok + self.errors >= self.expected_responses
    }

    fn on_message(
        &mut self,
        server: &mut Server,
        client: &mut OptionalClient,
        worker_id: WorkerId,
        message: WorkerResponse,
    ) {
        match ResponseStatus::try_from(message.status) {
            Ok(ResponseStatus::Ok) => self.ok += 1,
            Ok(ResponseStatus::Failure) => self.errors += 1,
            Ok(ResponseStatus::Processing) => client.return_processing(format!(
                "Worker {} is processing {}. {}",
                worker_id, message.id, message.message
            )),
            Err(e) => warn!("error decoding response status: {}", e),
        }
        self.responses.push((worker_id, message));
    }
}

#[derive(thiserror::Error, Debug)]
pub enum HubError {
    #[error("could not create main server: {0}")]
    CreateServer(ServerError),
    #[error("could not get executable path")]
    GetExecutablePath(UtilError),
    #[error("could not create SCM socket for worker {0}: {1}")]
    CreateScmSocket(u32, ScmSocketError),
}

/// A platform to receive client connections, pass orders to workers,
/// gather data, etc.
#[derive(Debug)]
pub struct CommandHub {
    /// contains workers and the event loop
    pub server: Server,
    /// keeps track of agents that contacted Sōzu on the UNIX socket
    clients: HashMap<Token, ClientSession>,
    /// register tasks, for parallel execution
    tasks: HashMap<TaskId, TaskContainer>,
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
    pub fn new(
        unix_listener: UnixListener,
        config: Config,
        executable_path: String,
    ) -> Result<Self, HubError> {
        Ok(Self {
            server: Server::new(unix_listener, config, executable_path)
                .map_err(HubError::CreateServer)?,
            clients: HashMap::new(),
            tasks: HashMap::new(),
        })
    }

    fn register_client(&mut self, mut stream: UnixStream) {
        let token = self.next_session_token();
        if let Err(err) = self.register(token, &mut stream) {
            error!("Could not register client: {}", err);
        }
        let channel = Channel::new(stream, 4096, u64::MAX);
        let id = self.next_client_id();
        let session = ClientSession::new(channel, id, token);
        info!("Register new client: {}", id);
        debug!("{:#?}", session);
        self.clients.insert(token, session);
    }

    fn get_client_mut(&mut self, token: &Token) -> Option<(&mut Server, &mut ClientSession)> {
        self.clients
            .get_mut(token)
            .map(|client| (&mut self.server, client))
    }

    /// recreate the command hub when upgrading the main process
    pub fn from_upgrade_data(upgrade_data: UpgradeData) -> Result<Self, HubError> {
        let UpgradeData {
            command_socket_fd,
            config,
            workers,
            state,
            next_client_id,
            next_session_id,
            next_task_id,
            next_worker_id,
        } = upgrade_data;

        let executable_path =
            unsafe { get_executable_path().map_err(HubError::GetExecutablePath)? };

        let unix_listener = unsafe { UnixListener::from_raw_fd(command_socket_fd) };

        let command_buffer_size = config.command_buffer_size;
        let max_command_buffer_size = config.max_command_buffer_size;

        let mut server =
            Server::new(unix_listener, config, executable_path).map_err(HubError::CreateServer)?;

        server.state = state;
        server.update_counts();
        server.next_client_id = next_client_id;
        server.next_session_id = next_session_id;
        server.next_task_id = next_task_id;
        server.next_worker_id = next_worker_id;

        for worker in workers
            .iter()
            .filter(|w| w.run_state != RunState::Stopped && w.run_state != RunState::Stopping)
        {
            let worker_stream = unsafe { UnixStream::from_raw_fd(worker.channel_fd) };
            let channel: Channel<WorkerRequest, WorkerResponse> =
                Channel::new(worker_stream, command_buffer_size, max_command_buffer_size);

            let scm_socket = ScmSocket::new(worker.scm_fd)
                .map_err(|scm_err| HubError::CreateScmSocket(worker.id, scm_err))?;

            if let Err(err) = server.register_worker(worker.id, worker.pid, channel, scm_socket) {
                error!("could not register worker: {}", err);
            }
        }

        Ok(CommandHub {
            server,
            clients: HashMap::new(),
            tasks: HashMap::new(),
        })
    }

    /// contains the main event loop
    /// - accept clients
    /// - receive requests from clients and responses from workers
    /// - dispatch these message to the [Server]
    /// - manage timeouts of tasks
    pub fn run(&mut self) {
        let mut events = Events::with_capacity(100);
        debug!("running the command hub: {:#?}", self);

        loop {
            let run_state = self.run_state;
            let now = Instant::now();

            let mut tasks = std::mem::take(&mut self.tasks);
            let mut queued_tasks = std::mem::take(&mut self.server.queued_tasks);
            self.tasks = tasks
                .drain()
                .chain(queued_tasks.drain())
                .filter_map(|(task_id, mut task)| {
                    if task.job.get_gatherer().has_finished() {
                        self.handle_finishing_task(task_id, task, false);
                        return None;
                    }
                    if let Some(timeout) = task.timeout {
                        if timeout < now {
                            self.handle_finishing_task(task_id, task, true);
                            return None;
                        }
                    }
                    Some((task_id, task))
                })
                .collect();

            let next_timeout = self.tasks.values().filter_map(|t| t.timeout).max();
            let mut poll_timeout = next_timeout.map(|t| t.saturating_duration_since(now));

            if self.run_state == ServerState::Stopping {
                // when closing, close all ClientSession which are not transfering data
                self.clients
                    .retain(|_, s| s.channel.back_buf.available_data() > 0);
                // when all ClientSession are closed, the CommandServer stops
                if self.clients.is_empty() {
                    break;
                }
            }

            let sessions_to_tick = self
                .clients
                .iter()
                .filter_map(|(t, s)| {
                    if wants_to_tick(&s.channel) {
                        Some((*t, Ready::EMPTY, None))
                    } else {
                        None
                    }
                })
                .chain(self.workers.iter().filter_map(|(token, session)| {
                    if session.run_state != RunState::Stopped && wants_to_tick(&session.channel) {
                        Some((*token, Ready::EMPTY, None))
                    } else {
                        None
                    }
                }))
                .collect::<Vec<_>>();

            let workers_to_spawn = self.workers_to_spawn();

            // if we have sessions to tick or workers to spawn, we don't want to block on poll
            if !sessions_to_tick.is_empty() || workers_to_spawn > 0 {
                poll_timeout = Some(Duration::default());
            }

            events.clear();
            trace!("Tasks: {:?}", self.tasks);
            trace!("Sessions to tick: {:?}", sessions_to_tick);
            trace!("Polling timeout: {:?}", poll_timeout);
            match self.poll.poll(&mut events, poll_timeout) {
                Ok(()) => {}
                Err(error) => error!("Error while polling: {:?}", error),
            }

            self.automatic_worker_spawn(workers_to_spawn);

            let events = sessions_to_tick.into_iter().chain(
                events
                    .into_iter()
                    .map(|event| (event.token(), Ready::from(event), Some(event))),
            );
            for (token, ready, event) in events {
                match token {
                    Token(0) => {
                        if run_state == ServerState::Stopping {
                            // do not accept new clients when stopping
                            continue;
                        }
                        if ready.is_readable() {
                            while let Ok((stream, _addr)) = self.unix_listener.accept() {
                                self.register_client(stream);
                            }
                        }
                    }
                    token => {
                        trace!("{:?} got event: {:?}", token, event);
                        if let Some((server, client)) = self.get_client_mut(&token) {
                            client.update_readiness(ready);
                            match client.ready() {
                                ClientResult::NothingToDo => {}
                                ClientResult::NewRequest(request) => {
                                    debug!("Received new request: {:?}", request);
                                    server.handle_client_request(client, request);
                                }
                                ClientResult::CloseSession => {
                                    info!("Closing client {}", client.id);
                                    debug!("{:#?}", client);
                                    self.event_subscribers.remove(&token);
                                    self.clients.remove(&token);
                                }
                            }
                        } else if let Some(worker) = self.workers.get_mut(&token) {
                            if run_state == ServerState::Stopping {
                                // do not read responses from workers when stopping
                                continue;
                            }
                            worker.update_readiness(ready);
                            let worker_id = worker.id;
                            match worker.ready() {
                                WorkerResult::NothingToDo => {}
                                WorkerResult::NewResponses(responses) => {
                                    for response in responses {
                                        self.handle_worker_response(worker_id, response);
                                    }
                                }
                                WorkerResult::CloseSession => self.handle_worker_close(&token),
                            }
                        }
                    }
                }
            }
        }
    }

    fn handle_worker_response(&mut self, worker_id: WorkerId, response: WorkerResponse) {
        // transmit backend events to subscribing clients
        if let Some(ResponseContent {
            content_type: Some(ContentType::Event(event)),
        }) = response.content
        {
            for client_token in &self.server.event_subscribers {
                if let Some(client) = self.clients.get_mut(client_token) {
                    client.return_processing_with_content(
                        format!("{worker_id}"),
                        ContentType::Event(event.clone()).into(),
                    );
                }
            }
            return;
        }

        let Some(task_id) = self.in_flight.get(&response.id).copied() else {
            error!("Got a response for an unknown task: {}", response);
            return;
        };

        let task = match self.tasks.get_mut(&task_id) {
            Some(task) => task,
            None => {
                error!("Got a response for an unknown task");
                return;
            }
        };

        let client = &mut task
            .job
            .client_token()
            .and_then(|token| self.clients.get_mut(&token));
        task.job
            .get_gatherer()
            .on_message(&mut self.server, client, worker_id, response);
    }

    fn handle_finishing_task(&mut self, task_id: TaskId, task: TaskContainer, timed_out: bool) {
        if timed_out {
            debug!("Task timeout: {:?}", task);
        } else {
            debug!("Task finish: {:?}", task);
        }
        let client = &mut task
            .job
            .client_token()
            .and_then(|token| self.clients.get_mut(&token));
        task.job.on_finish(&mut self.server, client, false);
        self.in_flight
            .retain(|_, in_flight_task_id| *in_flight_task_id != task_id);
    }
}

#[derive(thiserror::Error, Debug)]
pub enum ServerError {
    #[error("Could not create Poll with MIO: {0:?}")]
    CreatePoll(IoError),
    #[error("Could not register channel in MIO registry: {0:?}")]
    RegisterChannel(IoError),
    #[error("Could not fork the main into a new worker: {0}")]
    ForkMain(WorkerError),
    #[error("Did not find worker. This should NOT happen.")]
    WorkerNotFound,
    #[error("could not enable cloexec: {0}")]
    EnableCloexec(UtilError),
    #[error("could not disable cloexec: {0}")]
    DisableCloexec(UtilError),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ServerState {
    Running,
    WorkersStopping,
    Stopping,
}

/// Manages workers
/// Functions as an executer for tasks that have two steps:
/// - scatter to workers
/// - gather worker responses
/// - trigger a finishing function when all responses are gathered
pub struct Server {
    pub config: Config,
    /// Sōzu clients that subscribed to events
    pub event_subscribers: HashSet<Token>,
    /// path to the executable binary of Sōzu (for upgrading)
    pub executable_path: String,
    /// keep track of the tasks
    in_flight: HashMap<RequestId, TaskId>,
    next_client_id: ClientId,
    next_session_id: SessionId,
    next_task_id: TaskId,
    next_worker_id: WorkerId,
    /// the MIO structure that registers sockets and polls them all
    poll: Poll,
    /// all tasks created in one tick, to be propagated to the Hub at each tick
    queued_tasks: HashMap<TaskId, TaskContainer>,
    /// contains all business logic of Sōzu (frontends, backends, routing, etc.)
    pub state: ConfigState,
    /// used to shut down gracefully
    pub run_state: ServerState,
    /// the UNIX socket on which to receive clients
    unix_listener: UnixListener,
    /// the Sōzu processes running parallel to the main process.
    /// The workers perform the whole business of proxying and must be
    /// synchronized at all times.
    pub workers: HashMap<Token, WorkerSession>,
}

impl Server {
    fn new(
        mut unix_listener: UnixListener,
        config: Config,
        executable_path: String,
    ) -> Result<Self, ServerError> {
        let poll = mio::Poll::new().map_err(ServerError::CreatePoll)?;
        poll.registry()
            .register(
                &mut unix_listener,
                Token(0),
                Interest::READABLE | Interest::WRITABLE,
            )
            .map_err(ServerError::RegisterChannel)?;

        Ok(Self {
            config,
            event_subscribers: HashSet::new(),
            executable_path,
            in_flight: HashMap::new(),
            next_client_id: 0,
            next_session_id: 1, // 0 is reserved for the UnixListener
            next_task_id: 0,
            next_worker_id: 0,
            poll,
            queued_tasks: HashMap::new(),
            state: ConfigState::new(),
            run_state: ServerState::Running,
            unix_listener,
            workers: HashMap::new(),
        })
    }

    /// - fork the main process into a new worker
    /// - register the worker in mio
    /// - send a Status request to the new worker
    pub fn launch_new_worker(
        &mut self,
        listeners: Option<Listeners>,
    ) -> Result<&mut WorkerSession, ServerError> {
        let worker_id = self.next_worker_id();
        let (worker_pid, main_to_worker_channel, main_to_worker_scm) = fork_main_into_worker(
            &worker_id.to_string(),
            &self.config,
            self.executable_path.clone(),
            &self.state,
            Some(listeners.unwrap_or_default()),
        )
        .map_err(ServerError::ForkMain)?;

        let worker_session = self.register_worker(
            worker_id,
            worker_pid,
            main_to_worker_channel,
            main_to_worker_scm,
        )?;

        // TODO: make sure the worker is registered as NotAnswering,
        // and create a task that will pass it to Running when it respond OK to this request:
        worker_session.send(&WorkerRequest {
            id: format!("INITIAL-STATUS-{worker_id}"),
            content: RequestType::Status(Status {}).into(),
        });

        Ok(worker_session)
    }

    /// count backends and frontends in the cache, update gauge metrics
    pub fn update_counts(&mut self) {
        gauge!("configuration.clusters", self.state.clusters.len());
        gauge!("configuration.backends", self.state.count_backends());
        gauge!("configuration.frontends", self.state.count_frontends());
    }

    fn next_session_token(&mut self) -> Token {
        let token = Token(self.next_session_id);
        self.next_session_id += 1;
        token
    }
    fn next_client_id(&mut self) -> ClientId {
        let id = self.next_client_id;
        self.next_client_id += 1;
        id
    }

    fn next_task_id(&mut self) -> TaskId {
        let id = self.next_task_id;
        self.next_task_id += 1;
        id
    }

    fn next_worker_id(&mut self) -> WorkerId {
        let id = self.next_worker_id;
        self.next_worker_id += 1;
        id
    }

    fn register(&mut self, token: Token, stream: &mut UnixStream) -> Result<(), ServerError> {
        self.poll
            .registry()
            .register(stream, token, Interest::READABLE | Interest::WRITABLE)
            .map_err(ServerError::RegisterChannel)
    }

    /// returns None if the worker is not alive
    pub fn get_active_worker_by_id(&self, id: WorkerId) -> Option<&WorkerSession> {
        self.workers
            .values()
            .find(|worker| worker.id == id && worker.is_active())
    }

    /// register a worker session in the server, return the mutable worker session
    pub fn register_worker(
        &mut self,
        worker_id: WorkerId,
        pid: pid_t,
        mut channel: Channel<WorkerRequest, WorkerResponse>,
        scm_socket: ScmSocket,
    ) -> Result<&mut WorkerSession, ServerError> {
        let token = self.next_session_token();
        self.register(token, &mut channel.sock)?;
        self.workers.insert(
            token,
            WorkerSession::new(channel, worker_id, pid, token, scm_socket),
        );
        self.workers
            .get_mut(&token)
            .ok_or(ServerError::WorkerNotFound)
    }

    /// Add a task in a queue to make it accessible until the next tick
    pub fn new_task(&mut self, job: Box<dyn GatheringTask>, timeout: Timeout) -> TaskId {
        let task_id = self.next_task_id();
        let timeout = match timeout {
            Timeout::None => None,
            Timeout::Default => Some(Duration::from_secs(self.config.worker_timeout as u64)),
            Timeout::Custom(duration) => Some(duration),
        }
        .map(|duration| Instant::now() + duration);
        self.queued_tasks
            .insert(task_id, TaskContainer { job, timeout });
        task_id
    }

    pub fn scatter(
        &mut self,
        request: Request,
        job: Box<dyn GatheringTask>,
        timeout: Timeout,
        target: Option<WorkerId>, // if None, scatter to all workers
    ) {
        let task_id = self.new_task(job, timeout);

        self.scatter_on(request, task_id, 0, target);
    }

    pub fn scatter_on(
        &mut self,
        request: Request,
        task_id: TaskId,
        request_id: usize,
        target: Option<WorkerId>,
    ) {
        let task = match self.queued_tasks.get_mut(&task_id) {
            Some(task) => task,
            None => {
                error!("no task found with id {}", task_id);
                return;
            }
        };

        let mut worker_count = 0;
        let mut worker_request = WorkerRequest {
            id: String::new(),
            content: request,
        };

        for worker in self.workers.values_mut().filter(|w| {
            target
                .map(|id| id == w.id && w.run_state != RunState::Stopped)
                .unwrap_or(w.run_state != RunState::Stopped)
        }) {
            worker_count += 1;
            worker_request.id = format!(
                "{}-{}-{}-{}",
                worker_request.content.short_name(),
                worker.id,
                task_id,
                request_id,
            );
            debug!("scattering to worker {}: {:?}", worker.id, worker_request);
            worker.send(&worker_request);
            self.in_flight.insert(worker_request.id, task_id);
        }
        task.job.get_gatherer().inc_expected_responses(worker_count);
    }

    pub fn cancel_task(&mut self, task_id: TaskId) {
        self.queued_tasks.remove(&task_id);
    }

    /// Called when the main cannot communicate anymore with a worker (it's channel closed)
    /// Calls Self::close_worker which makes sure the worker is killed to prevent it from
    /// going rogue if it wasn't the case
    pub fn handle_worker_close(&mut self, token: &Token) {
        match self.workers.get(token) {
            Some(worker) => {
                info!("closing session of worker {}", worker.id);
                trace!("closing worker session {:?}", worker);
            }
            None => {
                error!("No worker exists with token {:?}", token);
                return;
            }
        };

        self.close_worker(token);
    }

    /// returns how many workers should be started to reach config count
    pub fn workers_to_spawn(&self) -> u16 {
        if self.config.worker_automatic_restart && self.run_state == ServerState::Running {
            self.config
                .worker_count
                .saturating_sub(self.alive_workers() as u16)
        } else {
            0
        }
    }

    /// spawn brand new workers
    pub fn automatic_worker_spawn(&mut self, count: u16) {
        if count == 0 {
            return;
        }

        info!("Automatically restarting {} workers", count);
        for _ in 0..count {
            if let Err(err) = self.launch_new_worker(None) {
                error!("could not launch new worker: {}", err);
            }
        }
    }

    fn alive_workers(&self) -> usize {
        self.workers
            .values()
            .filter(|worker| worker.is_active())
            .count()
    }

    /// kill the worker process
    pub fn close_worker(&mut self, token: &Token) {
        let worker = match self.workers.get_mut(token) {
            Some(w) => w,
            None => {
                error!("No worker exists with token {:?}", token);
                return;
            }
        };

        match kill(Pid::from_raw(worker.pid), Signal::SIGKILL) {
            Ok(()) => info!("Worker {} was successfully killed", worker.id),
            Err(_) => info!("worker {} was already dead", worker.id),
        }
        worker.run_state = RunState::Stopped;
    }

    /// Make the file descriptors of the channel survive the upgrade
    pub fn disable_cloexec_before_upgrade(&mut self) -> Result<i32, ServerError> {
        trace!(
            "disabling cloexec on listener with file descriptor: {}",
            self.unix_listener.as_raw_fd()
        );

        disable_close_on_exec(self.unix_listener.as_raw_fd()).map_err(ServerError::DisableCloexec)
    }

    /// This enables workers to be notified in case the main process dies
    pub fn enable_cloexec_after_upgrade(&mut self) -> Result<i32, ServerError> {
        for worker in self.workers.values_mut() {
            if worker.run_state == RunState::Running {
                let _ = enable_close_on_exec(worker.channel.fd()).map_err(|e| {
                    error!(
                        "could not enable close on exec for worker {}: {}",
                        worker.id, e
                    );
                });
            }
        }
        enable_close_on_exec(self.unix_listener.as_raw_fd()).map_err(ServerError::EnableCloexec)
    }

    /// summarize the server into what is needed to recreate it, when upgrading
    pub fn generate_upgrade_data(&self) -> UpgradeData {
        UpgradeData {
            command_socket_fd: self.unix_listener.as_raw_fd(),
            config: self.config.clone(),
            workers: self
                .workers
                .values()
                .filter_map(|session| match SerializedWorkerSession::try_from(session) {
                    Ok(serialized_session) => Some(serialized_session),
                    Err(err) => {
                        error!("failed to serialize worker session: {}", err);
                        None
                    }
                })
                .collect(),
            state: self.state.clone(),
            next_client_id: self.next_client_id,
            next_session_id: self.next_session_id,
            next_task_id: self.next_task_id,
            next_worker_id: self.next_worker_id,
        }
    }
}

impl Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Server")
            .field("config", &self.config)
            .field("event_subscribers", &self.event_subscribers)
            .field("executable_path", &self.executable_path)
            .field("in_flight", &self.in_flight)
            .field("next_client_id", &self.next_client_id)
            .field("next_session_id", &self.next_session_id)
            .field("next_task_id", &self.next_task_id)
            .field("next_worker_id", &self.next_worker_id)
            .field("poll", &self.poll)
            .field("queued_tasks", &self.queued_tasks)
            .field("run_state", &self.run_state)
            .field("unix_listener", &self.unix_listener)
            .field("workers", &self.workers)
            .finish()
    }
}
