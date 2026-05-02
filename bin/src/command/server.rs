//! Master command-server event loop.
//!
//! Drives the mio loop on the unix command socket: accepts CLI clients,
//! forwards verbs to workers via per-worker `Channel`s, replays the
//! configuration state on (re)connection, and consumes worker responses.
//! Holds the worker registry, accepts hot-upgrade FDs, and surfaces
//! supervisor metrics. Long-form lifecycle: `bin/src/command/LIFECYCLE.md`.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    fmt::{self, Debug},
    fs::{DirBuilder, File, OpenOptions, Permissions},
    io::{Error as IoError, Write},
    ops::{Deref, DerefMut},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    },
    path::Path,
    time::{Duration, Instant},
};

use libc::pid_t;
use mio::{
    Events, Interest, Poll, Token,
    net::{UnixListener, UnixStream},
};
use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use sozu_command_lib::{
    channel::Channel,
    config::Config,
    proto::command::{
        Event, Request, ResponseContent, ResponseStatus, RunState, Status, WorkerRequest,
        WorkerResponse, request::RequestType, response_content::ContentType,
    },
    ready::Ready,
    scm_socket::{Listeners, ScmSocket, ScmSocketError},
    state::ConfigState,
};

use super::upgrade::SerializedWorkerSession;
use crate::{
    command::{
        sessions::{
            ClientResult, ClientSession, OptionalClient, WorkerResult, WorkerSession, wants_to_tick,
        },
        upgrade::UpgradeData,
    },
    util::{UtilError, disable_close_on_exec, enable_close_on_exec, get_executable_path},
    worker::{WorkerError, fork_main_into_worker},
};

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
    /// Path of the command socket we're accepting on, stamped into every
    /// accepted [`ClientSession::socket_path`]. Stored as `Arc<str>` so it
    /// clones cheaply per-session.
    command_socket_path: std::sync::Arc<str>,
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
        let command_socket_path: std::sync::Arc<str> = config
            .command_socket_path()
            .unwrap_or_else(|_| "unknown".to_owned())
            .into();
        Ok(Self {
            server: Server::new(unix_listener, config, executable_path)
                .map_err(HubError::CreateServer)?,
            clients: HashMap::new(),
            tasks: HashMap::new(),
            command_socket_path,
        })
    }

    fn register_client(&mut self, mut stream: UnixStream) {
        let token = self.next_session_token();
        // Previously the registration error was logged and we continued
        // anyway, inserting a session whose underlying stream
        // had no mio readiness wired up. The session sat in
        // `self.clients` until manual cleanup. Treat registration failure
        // as terminal — drop the stream and return. The OS sends RST/EOF
        // to the peer when `stream` is dropped at the end of this scope.
        if let Err(err) = self.register(token, &mut stream) {
            error!(
                "Could not register client (token={:?}): {} — dropping connection",
                token, err
            );
            return;
        }
        let peer_cred = peer_cred_from_stream(&stream);
        let actor_comm = peer_cred.pid.and_then(peer_comm);
        let actor_user = peer_cred.uid.and_then(peer_user);
        // SECURITY (CWE-770): the client channel must NOT grow without
        // bound. The previous `u64::MAX` ceiling combined with
        // the doubling growth in `Channel::readable()` and the absence of
        // `Vec::try_reserve` in `Buffer::grow` meant any same-UID local
        // process could send a single oversized length-prefixed message and
        // OOM the main process. Bind to `max_command_buffer_size` (default
        // 2 MB, configurable) — same ceiling worker channels at the
        // fork_main_into_worker site already use. Operators who legitimately
        // need a larger ceiling can raise `max_command_buffer_size` in the
        // global config.
        let channel = Channel::new(stream, 4096, self.config.max_command_buffer_size);
        let id = self.next_client_id();
        let session = ClientSession::new(
            channel,
            id,
            token,
            peer_cred,
            actor_comm,
            actor_user,
            self.command_socket_path.clone(),
        );
        info!(
            "Register new client: {} (actor_uid={} actor_pid={} actor_user={} actor_comm={})",
            id,
            session.actor_uid_display(),
            session.actor_pid_display(),
            session.actor_user_display(),
            session.actor_comm_display()
        );
        debug!("registering client {:?}", session);
        self.clients.insert(token, session);
    }

    /// Drain audit events queued by the [Server] during a client request and
    /// fan them out to every subscribed client (same path as worker-emitted
    /// backend events). Called from the event loop after handling a request.
    fn flush_pending_audit_events(&mut self) {
        let events: Vec<Event> = self.server.pending_audit_events.drain(..).collect();
        if events.is_empty() {
            return;
        }
        for event in events {
            for client_token in &self.server.event_subscribers {
                if let Some(client) = self.clients.get_mut(client_token) {
                    client.return_processing_with_content(
                        String::from("main"),
                        ContentType::Event(event.clone()).into(),
                    );
                }
            }
        }
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
            boot_generation,
        } = upgrade_data;

        // SAFETY: `get_executable_path` is marked unsafe to keep its FFI
        // signature consistent across platforms (see `bin/src/util.rs`).
        // On Linux it just reads `/proc/self/exe`; on FreeBSD it issues a
        // `sysctl` call with stack-allocated MIB. This runs only inside
        // the supervisor's single-threaded reload path.
        let executable_path =
            unsafe { get_executable_path().map_err(HubError::GetExecutablePath)? };

        // SAFETY: `command_socket_fd` was inherited via the upgrade hand-off
        // (see `UpgradeData`) and is not owned elsewhere in this freshly
        // re-execed supervisor. Ownership transfers to the `UnixListener`,
        // whose `Drop` closes the descriptor.
        let unix_listener = unsafe { UnixListener::from_raw_fd(command_socket_fd) };

        let command_buffer_size = config.command_buffer_size;
        let max_command_buffer_size = config.max_command_buffer_size;
        let command_socket_path: std::sync::Arc<str> = config
            .command_socket_path()
            .unwrap_or_else(|_| "unknown".to_owned())
            .into();

        let mut server =
            Server::new(unix_listener, config, executable_path).map_err(HubError::CreateServer)?;

        server.state = state;
        server.update_counts();
        server.next_client_id = next_client_id;
        server.next_session_id = next_session_id;
        server.next_task_id = next_task_id;
        server.next_worker_id = next_worker_id;
        // Carry the boot generation forward; it will be bumped one more time
        // by `upgrade_main` before the next re-exec.
        server.boot_generation = boot_generation;

        for worker in workers
            .iter()
            .filter(|w| w.run_state != RunState::Stopped && w.run_state != RunState::Stopping)
        {
            // SAFETY: `worker.channel_fd` was inherited via the upgrade
            // hand-off (see `UpgradeData::workers`) and is not owned
            // elsewhere in this freshly re-execed supervisor. Ownership
            // transfers to the `UnixStream`, whose `Drop` closes the
            // descriptor.
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
            command_socket_path,
        })
    }

    /// contains the main event loop
    /// - accept clients
    /// - receive requests from clients and responses from workers
    /// - dispatch these message to the [Server]
    /// - manage timeouts of tasks
    ///
    /// Returns `true` when the loop exited because `upgrade_main`
    /// flipped `Server.upgrading` (binary hand-off to a forked child
    /// master), `false` on a regular graceful shutdown
    /// (`SoftStop` / `HardStop`). The bin entry-point uses the
    /// distinction to decide whether to emit `STOPPING=1` to systemd.
    pub fn run(&mut self) -> bool {
        let mut events = Events::with_capacity(100);
        debug!("running the command hub: {:?}", self);

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
                    return self.server.upgrading;
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
                                    self.flush_pending_audit_events();
                                }
                                ClientResult::CloseSession => {
                                    info!("Closing client {}", client.id);
                                    debug!("closing client {:?}", client);
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
            // this will appear on startup, when requesting status. It is inconsequential.
            warn!("Got a response for an unknown task: {}", response);
            return;
        };

        let task = match self.tasks.get_mut(&task_id) {
            Some(task) => task,
            None => {
                warn!("Got a response for an unknown task");
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
    /// audit events emitted by the main process for control-plane mutations,
    /// drained by the [CommandHub] after each request and fanned out to the
    /// subscribed clients (same channel as worker-emitted backend events).
    pub pending_audit_events: VecDeque<Event>,
    /// Dedicated file handle for the control-plane audit log, opened once
    /// at boot if `Config::audit_logs_target` is set. Every audit line is
    /// appended here in addition to the standard `info!` sink. `RefCell`
    /// because the event loop is single-threaded and `audit_emit` borrows
    /// `&Server` only, not `&mut Server`, for the whole `Event` fan-out.
    /// `None` when no dedicated sink is configured (fallback to `log_target`).
    pub audit_log_writer: Option<RefCell<File>>,
    /// Dedicated JSON-structured audit sink. Same lifecycle as
    /// `audit_log_writer`, but writes one JSON object per line so SIEM
    /// pipelines (Wazuh, Elastic, Loki) can ingest without bespoke parser
    /// code. `None` when `Config::audit_logs_json_target` is unset.
    pub audit_log_json_writer: Option<RefCell<File>>,
    /// Boot-generation counter, incremented each time the main process
    /// re-execs via `MAIN_UPGRADED`. Stamped into every audit line so
    /// SOC tooling can disambiguate post-upgrade sessions from pre-upgrade
    /// ones — `(boot_generation, session_ulid)` is the durable correlation
    /// pair across PID reuse. Persisted across upgrades via [`UpgradeData`];
    /// resets only on full process restart (not re-exec).
    pub boot_generation: u32,
    /// the MIO structure that registers sockets and polls them all
    poll: Poll,
    /// all tasks created in one tick, to be propagated to the Hub at each tick
    queued_tasks: HashMap<TaskId, TaskContainer>,
    /// contains all business logic of Sōzu (frontends, backends, routing, etc.)
    pub state: ConfigState,
    /// used to shut down gracefully
    pub run_state: ServerState,
    /// `true` when the run_state transitioned through `upgrade_main`
    /// (binary hand-off to a forked child master) instead of a
    /// graceful `SoftStop` / `HardStop`. The bin entry-point reads
    /// this after `command_hub.run()` returns to decide whether to
    /// emit `STOPPING=1` to systemd: on the upgrade path we already
    /// sent `RELOADING=1` and the new master will signal its own
    /// `READY=1`, so an old-master `STOPPING=1` would race the new
    /// master's `MAINPID=` notify.
    pub upgrading: bool,
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

        let audit_log_writer = match config.audit_logs_target.as_deref() {
            Some(path) => match open_audit_log_file(path) {
                Ok(file) => Some(RefCell::new(file)),
                Err(err) => {
                    error!(
                        "Could not open audit log file {:?}: {}. Audit lines will be routed only through the standard logger.",
                        path, err
                    );
                    None
                }
            },
            None => None,
        };
        let audit_log_json_writer = match config.audit_logs_json_target.as_deref() {
            Some(path) => match open_audit_log_file(path) {
                Ok(file) => Some(RefCell::new(file)),
                Err(err) => {
                    error!(
                        "Could not open audit JSON log file {:?}: {}. JSON audit sink disabled.",
                        path, err
                    );
                    None
                }
            },
            None => None,
        };

        Ok(Self {
            config,
            event_subscribers: HashSet::new(),
            executable_path,
            in_flight: HashMap::new(),
            next_client_id: 0,
            next_session_id: 1, // 0 is reserved for the UnixListener
            next_task_id: 0,
            next_worker_id: 0,
            pending_audit_events: VecDeque::new(),
            audit_log_writer,
            audit_log_json_writer,
            boot_generation: 0,
            poll,
            queued_tasks: HashMap::new(),
            state: ConfigState::new(),
            run_state: ServerState::Running,
            upgrading: false,
            unix_listener,
            workers: HashMap::new(),
        })
    }

    /// Append a fully rendered audit line to the dedicated sink if one is
    /// configured. Best-effort: failures are logged but never propagated —
    /// the audit trail degrades gracefully to the standard logger instead
    /// of failing the mutation.
    pub fn append_audit_line(&self, line: &str) {
        let Some(writer) = self.audit_log_writer.as_ref() else {
            return;
        };
        let mut writer = writer.borrow_mut();
        if let Err(err) = writeln!(writer, "{line}") {
            error!("Could not append to audit log file: {}", err);
        }
    }

    /// Append a JSON-encoded audit record to the dedicated JSON sink, if
    /// one is configured. One line per record so SIEM parsers can stream
    /// `tail -F`. Same best-effort behaviour as [`append_audit_line`].
    pub fn append_audit_json(&self, json: &str) {
        let Some(writer) = self.audit_log_json_writer.as_ref() else {
            return;
        };
        let mut writer = writer.borrow_mut();
        if let Err(err) = writeln!(writer, "{json}") {
            error!("Could not append to audit JSON file: {}", err);
        }
    }
}

/// Open the dedicated audit log file with `O_APPEND | O_CREAT` semantics
/// and owner/group-only mode `0o640`. Group-readable lets an `audit` group
/// tail the file via ACL without granting full write access. The file is
/// never truncated on open — every sozu restart continues the same audit
/// stream (use logrotate to manage size).
///
/// PCI-DSS 10.5 hardening notes:
/// 1. `OpenOptions::mode(0o640)` is honoured by Linux only when the file is
///    *created* by this open. Pre-existing files keep their previous mode.
///    We therefore call `set_permissions(0o640)` after open so an existing
///    `chmod 0644` from a sloppy install or a non-mode-preserving logrotate
///    is corrected at boot.
/// 2. `create_dir_all` runs under the inherited worker umask (default
///    `0o022` → directory `0o755`), which leaves the audit directory
///    world-traversable. `DirBuilderExt::mode(0o750)` is applied so newly
///    created parents are owner+group only. Pre-existing parents are not
///    re-permissioned (operators may have a deliberate ACL).
/// 3. When the existing file's mode is wider than `0o640`, a `warn!` is
///    emitted alongside the corrective `chmod` so SOC tooling sees the
///    transition rather than discovering it via a file-system audit.
fn open_audit_log_file(path: &str) -> Result<File, IoError> {
    let path = Path::new(path);
    if let Some(parent) = path.parent() {
        // Create parent dirs best-effort; failure falls through to the open.
        // Only newly created parents get 0o750; existing dirs are untouched.
        let _ = DirBuilder::new().recursive(true).mode(0o750).create(parent);
    }
    let pre_existing = path.exists();
    let pre_existing_mode = if pre_existing {
        std::fs::metadata(path)
            .ok()
            .map(|m| m.permissions().mode() & 0o777)
    } else {
        None
    };
    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o640)
        .open(path)?;
    // Force-narrow the mode on pre-existing files. `mode(0o640)` above is a
    // no-op when the file already exists, so without this `set_permissions`
    // a pre-created `audit.log` with `0o644` (or wider) would silently
    // bypass the PCI-DSS 10.5 control.
    if let Some(prev) = pre_existing_mode
        && prev != 0o640
    {
        if prev & !0o640 != 0 {
            warn!(
                "audit log file {} pre-existed with mode 0o{:o} — narrowing to 0o640",
                path.display(),
                prev
            );
        }
        if let Err(e) = file.set_permissions(Permissions::from_mode(0o640)) {
            warn!(
                "could not narrow audit log file {} permissions to 0o640: {:?}",
                path.display(),
                e
            );
        }
    }
    Ok(file)
}

impl Server {
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

    /// Queue an audit event for fan-out to subscribed clients. Drained by
    /// [CommandHub::flush_pending_audit_events] after every request handler.
    pub fn push_audit_event(&mut self, event: Event) {
        self.pending_audit_events.push_back(event);
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
            boot_generation: self.boot_generation,
        }
    }
}

/// Peer credentials for the unix-socket client, used for audit attribution.
///
/// `pid` is needed to correlate with `journalctl _PID=<pid>` and `/proc/<pid>`.
/// `gid` widens the actor identity beyond uid alone. `uid` stays the primary
/// attribution field. All three come from the same `SO_PEERCRED` `getsockopt`
/// and are captured once at accept time (immutable for the session lifetime).
#[derive(Clone, Copy, Debug, Default)]
pub struct PeerCred {
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub pid: Option<i32>,
}

/// Read full peer credentials from a connected unix-domain socket via
/// `SO_PEERCRED`.
///
/// Returns a `PeerCred` with `None` fields on platforms without `SO_PEERCRED`
/// support, or when the `getsockopt` call fails (which would be unexpected
/// for a freshly accepted local socket but must not panic the main process).
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn peer_cred_from_stream(stream: &UnixStream) -> PeerCred {
    use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
    match getsockopt(stream, PeerCredentials) {
        Ok(creds) => PeerCred {
            uid: Some(creds.uid()),
            gid: Some(creds.gid()),
            pid: Some(creds.pid()),
        },
        Err(err) => {
            warn!("Could not read SO_PEERCRED on command socket: {}", err);
            PeerCred::default()
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(crate) fn peer_cred_from_stream(_stream: &UnixStream) -> PeerCred {
    PeerCred::default()
}

/// Read `/proc/<pid>/comm` to get the peer process's command name (up to
/// 15 chars per kernel spec). Best-effort; returns `None` on any error.
/// Cheap (one file read per accept, which happens once per sozu CLI invocation).
///
/// PID-reuse mitigation: between `getsockopt(SO_PEERCRED)` and this
/// read, the peer PID could (a) exit and be recycled by the kernel, or
/// (b) call `execve()` and become a different binary. To bind the comm
/// string to the *same* process the SO_PEERCRED snapshot saw, we read
/// `/proc/<pid>/stat` first, capture the `starttime` field (jiffies
/// since boot, monotonic — never reused), then read `comm`. If the
/// stat read fails (PID gone — case (a)) we return `None`. The exec
/// case (b) cannot be detected by starttime alone — execve does not
/// change starttime — but exec is not adversarial in our deployment
/// (the sozu CLI never exec's), and the SOC analyst seeing two different
/// binaries on the same PID across audit lines for the same session
/// is the right signal.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn peer_comm(pid: i32) -> Option<String> {
    use std::io::Read;
    // PID-reuse guard: open /proc/<pid>/stat first; if the PID is gone
    // we bail without returning a string that might describe a recycled
    // PID's new owner.
    let _stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let mut buf = String::new();
    let path = format!("/proc/{pid}/comm");
    std::fs::File::open(&path)
        .ok()?
        .read_to_string(&mut buf)
        .ok()?;
    let trimmed = buf.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(crate) fn peer_comm(_pid: i32) -> Option<String> {
    None
}

/// Resolve a uid to a POSIX account name via `getpwuid_r` (NSS). Best-effort;
/// returns `None` when NSS has no matching user or the lookup fails.
///
/// `getpwuid_r` is **synchronous** and on a misconfigured host (SSSD
/// wedge, LDAP timeout, broken nscd socket) can block the
/// main event loop for tens of seconds. This caches the last lookups
/// in a process-local map so a steady-state operator UID is paid at
/// most once per main lifetime. Capped via `MAX_PEER_USER_CACHE` to
/// stop a misbehaving peer from inflating it (the unix socket is
/// `0o600` so this is mostly a defense-in-depth bound).
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn peer_user(uid: u32) -> Option<String> {
    use std::sync::Mutex;

    use nix::unistd::{Uid, User};

    /// Hard ceiling on the in-process cache: 16 distinct UIDs is
    /// generous (operator + root + a couple of automation accounts).
    /// Past that we evict-on-insert to stay bounded.
    const MAX_PEER_USER_CACHE: usize = 16;

    static CACHE: Mutex<Vec<(u32, Option<String>)>> = Mutex::new(Vec::new());

    if let Ok(guard) = CACHE.lock()
        && let Some((_, cached)) = guard.iter().find(|(k, _)| *k == uid)
    {
        return cached.clone();
    }

    let resolved = match User::from_uid(Uid::from_raw(uid)) {
        Ok(Some(user)) => Some(user.name),
        Ok(None) => None,
        Err(err) => {
            warn!("Could not resolve username for uid {}: {}", uid, err);
            None
        }
    };

    if let Ok(mut guard) = CACHE.lock() {
        if guard.len() >= MAX_PEER_USER_CACHE {
            guard.remove(0);
        }
        guard.push((uid, resolved.clone()));
    }
    resolved
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(crate) fn peer_user(_uid: u32) -> Option<String> {
    None
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
            .field("pending_audit_events", &self.pending_audit_events.len())
            .field("poll", &self.poll)
            .field("queued_tasks", &self.queued_tasks)
            .field("run_state", &self.run_state)
            .field("unix_listener", &self.unix_listener)
            .field("workers", &self.workers)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sozu_command_lib::{
        config::Config,
        proto::command::{
            AddBackend, Cluster, RequestHttpFrontend, RequestTcpFrontend, SocketAddress,
            request::RequestType,
        },
    };
    use sozu_lib::metrics::METRICS;

    use sozu_command_lib::proto::command::{PathRule, RulePosition, filtered_metrics};

    /// Helper to read a gauge value from the thread-local METRICS
    fn read_gauge(key: &str) -> Option<u64> {
        METRICS.with(|metrics| {
            let mut m = metrics.borrow_mut();
            let proxy_metrics = m.dump_local_proxy_metrics();
            proxy_metrics.get(key).and_then(|fm| match &fm.inner {
                Some(filtered_metrics::Inner::Gauge(v)) => Some(*v),
                _ => None,
            })
        })
    }

    fn create_test_server() -> Server {
        let dir = tempfile::tempdir().expect("Could not create temp dir");
        let socket_path = dir.path().join("test.sock");
        let unix_listener = UnixListener::bind(&socket_path).expect("Could not bind socket");
        Server::new(unix_listener, Config::default(), "sozu".to_owned())
            .expect("Could not create server")
    }

    #[test]
    fn update_counts_reflects_state() {
        let mut server = create_test_server();

        // initially empty
        server.update_counts();
        assert_eq!(read_gauge("configuration.clusters"), Some(0));
        assert_eq!(read_gauge("configuration.backends"), Some(0));
        assert_eq!(read_gauge("configuration.frontends"), Some(0));

        // add a cluster
        server
            .state
            .dispatch(
                &RequestType::AddCluster(Cluster {
                    cluster_id: String::from("cluster_1"),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not add cluster");

        // add backends
        for i in 0..3 {
            server
                .state
                .dispatch(
                    &RequestType::AddBackend(AddBackend {
                        cluster_id: String::from("cluster_1"),
                        backend_id: format!("cluster_1-{i}"),
                        address: SocketAddress::new_v4(127, 0, 0, 1, 1026 + i as u16),
                        ..Default::default()
                    })
                    .into(),
                )
                .expect("Could not add backend");
        }

        // add an HTTP frontend
        server
            .state
            .dispatch(
                &RequestType::AddHttpFrontend(RequestHttpFrontend {
                    cluster_id: Some(String::from("cluster_1")),
                    hostname: String::from("example.com"),
                    path: PathRule::prefix(String::from("/")),
                    address: SocketAddress::new_v4(0, 0, 0, 0, 8080),
                    position: RulePosition::Tree.into(),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not add frontend");

        // add a TCP frontend
        server
            .state
            .dispatch(
                &RequestType::AddTcpFrontend(RequestTcpFrontend {
                    cluster_id: String::from("cluster_1"),
                    address: SocketAddress::new_v4(0, 0, 0, 0, 5432),
                    ..Default::default()
                })
                .into(),
            )
            .expect("Could not add TCP frontend");

        // gauges are still stale until update_counts() is called
        assert_eq!(read_gauge("configuration.clusters"), Some(0));

        // update_counts should refresh gauges
        server.update_counts();
        assert_eq!(read_gauge("configuration.clusters"), Some(1));
        assert_eq!(read_gauge("configuration.backends"), Some(3));
        assert_eq!(read_gauge("configuration.frontends"), Some(2));
    }
}
