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
        Event, EventKind, Request, ResponseContent, ResponseStatus, RunState, Status,
        WorkerRequest, WorkerResponse, request::RequestType, response_content::ContentType,
    },
    ready::Ready,
    scm_socket::{Listeners, ScmSocket, ScmSocketError},
    state::ConfigState,
};

use sozu_lib::metrics::names;

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

/// Per-client capacity both buffers of every command-socket client channel are
/// allocated at, deliberately NOT the global `command_buffer_size`. The one
/// exception is structural, not configurable: `Channel::new` clamps any
/// initial capacity above `max_buffer_size` down to it (sozu-proxy/sozu#1416),
/// so an operator who sets `max_command_buffer_size` below 4096 gets a client
/// channel starting AT that ceiling, with a warning, never above it.
///
/// `command_buffer_size` sizes the one-or-few channel kinds: the
/// supervisor↔worker channels (`crate::worker`, `crate::upgrade`,
/// `CommandHub::from_upgrade_data`) and the CLI's own end of this very
/// connection (`crate::ctl::create_channel`). This is the many-per-process
/// kind: `CommandHub::run`'s accept loop registers one client per `accept()`
/// with no connection cap, so whatever goes here is multiplied by the number
/// of simultaneously connected same-UID processes.
///
/// It is an ADDRESS-SPACE floor, and a RESIDENT one for every page a client
/// has touched. `Channel::new` allocates both buffers eagerly and
/// `Buffer::with_capacity` is `vec![0; capacity]`, which the allocator serves
/// from fresh zero pages that do not fault until written; but
/// `Channel::try_shrink_front_buf` / `try_shrink_back_buf` shrink a grown
/// buffer back to this value and NEVER below. Growth ABOVE the floor is
/// released — `Buffer::shrink` truncates and `shrink_to_fit`s, so a client
/// that grew to 1 MB and drained gets that megabyte back; the floor itself is
/// what is never released while a client stays connected. Keep the two costs
/// apart — the difference between them is large, and mixing them up is how
/// this value gets sized wrong.
///
/// Measured at 2000 clients holding two buffers each, under the jemalloc this
/// binary actually links (`bin/src/main.rs`'s `#[global_allocator]`): feeding
/// `command_buffer_size` in at its 1 MB built-in default costs 4737 MiB of
/// address space and 9 MiB RSS while untouched, rising to 3837 MiB RSS once
/// every page is written — about 2.4 MiB of address space per client, and up
/// to 1.9 MiB resident per client. Today's value costs 18 MiB of address space
/// and 17 MiB RSS across the same 2000 clients, faulted or not: about 9 KiB
/// per client either way.
///
/// So the cheapest attack — connect and send nothing — buys address space, not
/// RSS: a client that never writes faults neither buffer. That is still worth
/// refusing under `RLIMIT_AS` or strict overcommit. The resident cost does not
/// arrive with the first byte either: it arrives page by page, as a client
/// FILLS its buffers. A 200-byte `status` request faults one page, and the
/// same 2000 clients with 1 KiB touched in each buffer cost 28 MiB RSS at the
/// 1 MB size, not the 3837 MiB above. Both multiply against
/// `CommandHub::run`'s uncapped accept loop, and a client that never writes
/// never reaches `command_allowed_uids` (`crate::command::requests`), which
/// rejects at the REQUEST layer, after registration. Capping connections is
/// the fix for that and is a separate change; raising this floor before the
/// cap exists is the wrong order.
///
/// No rationale was ever recorded for the value, and that is checked rather
/// than assumed: `f0ecc544` introduced it as the only commit of PR #1060,
/// which carries zero inline review comments, zero issue comments, and a body
/// that mentions neither `4096` nor a buffer nor a capacity. In that same
/// commit `from_upgrade_data` fifty lines below already passed
/// `command_buffer_size` — so the author had the configured values in hand and
/// diverged here, plausibly for one page per connection. The `usize::MAX`
/// ceiling it was paired with rules out a CEILING defence, not a sizing
/// intent, and the CWE-770 comment that later appeared at the call site argues
/// that ceiling only. The argument above is why the value is kept now, written
/// down so the next reader does not have to guess.
///
/// What keeping it small costs is reallocation, and only on the READ side.
/// `Channel::grow_size` has exactly two call sites, `Channel::readable` and
/// `try_read_delimited_message`, and both grow `front_buf`: a REQUEST that
/// fills the 2 MB default ceiling takes nine doublings from here and one from
/// 1 MB — eight saved, amortised against the socket reads of that same
/// request. A response saves none: `write_delimited_message` doubles in a
/// local variable and calls `back_buf.grow` once, one reallocation from either
/// floor. That is noise. Sizing this channel kind is a memory-per-client
/// decision, not a throughput one, which is why it does not follow the
/// global. Raise `max_command_buffer_size` to carry a larger payload; there is
/// no knob for this floor, and `doc/configure.md` says so.
///
/// Pinned by
/// `client_channel_initial_capacity_is_independent_of_command_buffer_size`.
const CLIENT_CHANNEL_INITIAL_BUFFER_SIZE: u64 = 4096;

/// The `(worker_id, task_id, request_index)` triple [`Server::scatter_on`]
/// embeds in every per-worker request id, `"{worker_id}-{task_id}-{request_index}"`.
///
/// The id carries no request name: a name would be a free-form prefix nothing
/// reads back (the request itself is logged next to the id), and it is exactly
/// what would make this parse ambiguous. `None` for any id that was not built
/// by `scatter_on` (`INITIAL-STATUS-<worker>`, a worker-initiated event), which
/// callers treat as "not attributable".
pub fn parse_scatter_request_id(request_id: &str) -> Option<(WorkerId, TaskId, usize)> {
    let mut segments = request_id.split('-');
    let worker_id = segments.next()?.parse().ok()?;
    let task_id = segments.next()?.parse().ok()?;
    let request_index = segments.next()?.parse().ok()?;
    // POSTCONDITION: exactly three segments. A longer id is some other
    // producer's and must not be attributed to a scattered entry.
    if segments.next().is_some() {
        return None;
    }
    Some((worker_id, task_id, request_index))
}

/// Gather messages and notifies when there are no more left to read.
#[allow(unused)]
pub trait Gatherer {
    /// increment how many responses we expect
    fn inc_expected_responses(&mut self, count: usize);

    /// Called once per [`Server::scatter_on`] fan-out, BEFORE any response
    /// arrives.
    ///
    /// The default implementation only grows the expected-response budget,
    /// which is all a single-request task ever needs. A BULK task — the state
    /// replay and the static-configuration reload, which scatter hundreds of
    /// entries onto the SAME gatherer — overrides it to keep a per-entry
    /// budget keyed by `request_id` (the index `scatter_on` embeds in every
    /// per-worker request id), plus whatever it must derive from the request
    /// itself: sozu#1313 keeps the inverse used to revert an entry no worker
    /// acknowledged, which is only reachable here, while the request is still
    /// in hand.
    fn on_scatter(&mut self, request_id: usize, worker_count: usize, request: &Request) {
        let _ = (request_id, request);
        self.inc_expected_responses(worker_count);
    }

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
    /// Return a payload-free identifier suitable for retained-task logs.
    fn kind(&self) -> &'static str {
        std::any::type_name::<Self>()
    }

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
struct TaskContainer {
    job: Box<dyn GatheringTask>,
    timeout: Option<Instant>,
}

impl Debug for TaskContainer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskContainer")
            .field("task_kind", &self.job.kind())
            .field("timeout_armed", &self.timeout.is_some())
            .finish()
    }
}

/// Default strategy when gathering responses from workers
#[derive(Default)]
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

impl fmt::Debug for DefaultGatherer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DefaultGatherer")
            .field("ok", &self.ok)
            .field("errors", &self.errors)
            .field("responses_count", &self.responses.len())
            .field("expected_responses", &self.expected_responses)
            .finish()
    }
}

#[allow(unused)]
impl Gatherer for DefaultGatherer {
    fn inc_expected_responses(&mut self, count: usize) {
        let before = self.expected_responses;
        self.expected_responses += count;
        // INVARIANT: the expected-response budget advances by exactly `count`
        // — this is the number tracked against `ok + errors` to decide when a
        // scattered task has heard from every worker. An off-by-one here means
        // the task either finishes early (drops a worker's answer) or hangs
        // until timeout.
        debug_assert_eq!(
            self.expected_responses,
            before + count,
            "inc_expected_responses must grow the budget by exactly count"
        );
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
        // Snapshot the accounting before classifying so the post-conditions
        // can assert that each message bumps at most one terminal counter and
        // is always recorded exactly once.
        let ok_before = self.ok;
        let errors_before = self.errors;
        let responses_before = self.responses.len();
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
        // INVARIANT: a single worker message counts toward completion at most
        // once. An Ok bumps `ok`, a Failure bumps `errors`, a Processing /
        // undecodable status bumps neither (it is not terminal). Double-
        // counting would let `has_finished` fire before every worker replied.
        debug_assert!(
            self.ok - ok_before <= 1 && self.errors - errors_before <= 1,
            "on_message must advance a terminal counter by at most one"
        );
        debug_assert_eq!(
            (self.ok - ok_before) + (self.errors - errors_before),
            usize::from(matches!(
                ResponseStatus::try_from(self.responses[responses_before].1.status),
                Ok(ResponseStatus::Ok | ResponseStatus::Failure)
            )),
            "exactly one terminal counter advances iff the message was Ok/Failure"
        );
        // INVARIANT: every message is archived exactly once for `on_finish`.
        debug_assert_eq!(
            self.responses.len(),
            responses_before + 1,
            "on_message must record the response exactly once"
        );
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
        // global config. That paragraph is about the CEILING only; the
        // initial capacity next to it is `CLIENT_CHANNEL_INITIAL_BUFFER_SIZE`
        // and its own rationale is recorded there.
        let channel = Channel::new(
            stream,
            CLIENT_CHANNEL_INITIAL_BUFFER_SIZE,
            self.config.max_command_buffer_size,
        );
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
        // `token` came from the monotonic `next_session_token`, so it must not
        // already key a live client; a collision would evict an existing
        // client session and leak its channel/fd.
        debug_assert!(
            !self.clients.contains_key(&token),
            "register_client must use a fresh session token"
        );
        let clients_before = self.clients.len();
        self.clients.insert(token, session);
        // INVARIANT: exactly one client was registered, and it maps back from
        // its token carrying the id/token we stamped. Every later lookup
        // (`get_client_mut`, the event-loop dispatch) keys on this token.
        debug_assert_eq!(
            self.clients.len(),
            clients_before + 1,
            "register_client must add exactly one client session"
        );
        debug_assert!(
            self.clients
                .get(&token)
                .is_some_and(|c| c.token == token && c.id == id),
            "registered client must map back from its token with matching id"
        );
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
                    if let Some(timeout) = task.timeout
                        && timeout < now
                    {
                        self.handle_finishing_task(task_id, task, true);
                        return None;
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
            trace!("Tasks: count={}", self.tasks.len());
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
                                WorkerResult::CloseSession => {
                                    // Only the FIRST close of a given worker
                                    // synthesises failures: the session stays
                                    // registered after `close_worker` and can
                                    // report `CloseSession` again on the next
                                    // poll, which would double-count.
                                    let was_active = self
                                        .workers
                                        .get(&token)
                                        .is_some_and(WorkerSession::is_active);
                                    self.handle_worker_close(&token);
                                    if was_active {
                                        self.fail_in_flight_requests_of_worker(worker_id);
                                    }
                                }
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
            // Worker-local METRIC_DETAIL_CHANGED transitions (lease tick
            // expiry, worker arm apply/clear) are emitted by the worker
            // via the same `Event` channel that carries backend health
            // signals; the master folds them into the audit log here
            // alongside operator-initiated transitions audited from
            // `requests.rs::worker_request`. Without this, the worker's
            // polled janitor expiring a lease would leave no audit
            // trail, masking implicit cardinality changes from SOC tools.
            if event.kind == EventKind::MetricDetailChanged as i32
                && let Some(transition) = event.metric_detail.as_ref()
            {
                crate::command::requests::audit_worker_metric_detail_transition(
                    &mut self.server,
                    worker_id,
                    transition,
                );
            }
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

        // sozu#1313: a task scattered during THIS event-loop iteration is
        // still parked in `Server::queued_tasks` — it only migrates into
        // `self.tasks` at the top of the next iteration. A synthetic failure
        // raised in the same iteration as the scatter (a worker closing while
        // its requests are in flight) must reach it all the same, so both maps
        // are searched. The container is taken OUT of its map because
        // `on_message` needs `&mut self.server`, which `queued_tasks` is part
        // of; it is put back verbatim below.
        let (mut container, was_queued) = match self.tasks.remove(&task_id) {
            Some(task) => (task, false),
            None => match self.server.queued_tasks.remove(&task_id) {
                Some(task) => (task, true),
                None => {
                    warn!("Got a response for an unknown task");
                    return;
                }
            },
        };

        // sozu#1313: a TERMINAL answer retires its in-flight entry right away.
        // The entry used to live until the whole task finished, so a worker
        // that answered the first entries of a bulk replay and then closed had
        // every one of its ids re-fed as a synthetic `Failure` by
        // `fail_in_flight_requests_of_worker` — phantom rejections that failed
        // a replay the fleet had applied. `Processing` is not terminal: the
        // real answer is still owed. `handle_finishing_task`'s `retain` stays
        // as the safety net for entries nobody ever answered.
        let terminal = matches!(
            ResponseStatus::try_from(response.status),
            Ok(ResponseStatus::Ok | ResponseStatus::Failure)
        );
        if terminal {
            self.server.in_flight.remove(&response.id);
        }

        let client = &mut container
            .job
            .client_token()
            .and_then(|token| self.clients.get_mut(&token));
        container
            .job
            .get_gatherer()
            .on_message(&mut self.server, client, worker_id, response);

        if was_queued {
            self.server.queued_tasks.insert(task_id, container);
        } else {
            self.tasks.insert(task_id, container);
        }
    }

    /// sozu#1313: answer every request still in flight on a worker that just
    /// closed with a synthetic `Failure`.
    ///
    /// A closed worker never answers. Without this, `ok + errors` can never
    /// reach `expected_responses`, so `has_finished` never fires and the task
    /// only ends through its deadline — which, on the bulk replay paths that
    /// used to scatter with `Timeout::None`, meant never: the client waited
    /// forever. Routed through [`Self::handle_worker_response`] so the
    /// accounting, the per-entry attribution and the rollback decision are the
    /// ones a real rejection would have produced.
    fn fail_in_flight_requests_of_worker(&mut self, worker_id: WorkerId) {
        let orphaned: Vec<RequestId> = self
            .server
            .in_flight
            .keys()
            .filter(|request_id| {
                parse_scatter_request_id(request_id).is_some_and(|(id, ..)| id == worker_id)
            })
            .cloned()
            .collect();
        if orphaned.is_empty() {
            return;
        }
        info!(
            "Worker {} closed with {} requests in flight, accounting them as failures",
            worker_id,
            orphaned.len()
        );
        for id in orphaned {
            self.handle_worker_response(
                worker_id,
                WorkerResponse {
                    id,
                    status: ResponseStatus::Failure.into(),
                    message: format!("worker {worker_id} closed before answering"),
                    content: None,
                },
            );
        }
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
        // Forward the real `timed_out` the two call sites pass (has_finished →
        // false, timeout expiry → true). A previous hard-coded `false` made
        // `timed_out` always false in every `on_finish`: it left the audit
        // `FanoutStatus::Timeout`/`result` branches unreachable (a timed-out
        // command reported success) and neutralised `WorkerTask`'s rollback
        // decision, which reads this flag (sozu#1301, sozu#1314 — a panicking
        // worker never answers, so the timeout path is the ONLY way its
        // rejection is ever observed). Pinned by
        // `handle_finishing_task_forwards_timed_out_flag`.
        task.job.on_finish(&mut self.server, client, timed_out);
        self.in_flight
            .retain(|_, in_flight_task_id| *in_flight_task_id != task_id);
        // POST-CONDITION: every in-flight entry pointing at this finished task
        // has been purged. A leftover would make a late/duplicate worker
        // response resolve to a task that is already gone, mis-routing it.
        debug_assert!(
            self.in_flight.values().all(|id| *id != task_id),
            "handle_finishing_task must purge all in-flight entries for the finished task"
        );
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
    /// `tail -F`. Same best-effort behaviour as [`Self::append_audit_line`].
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
        if let Err(send_error) = worker_session.send(&WorkerRequest {
            id: format!("INITIAL-STATUS-{worker_id}"),
            content: RequestType::Status(Status {}).into(),
        }) {
            error!(
                "could not send the initial status request to worker {}: {}",
                worker_id, send_error
            );
        }

        Ok(worker_session)
    }

    /// count backends and frontends in the cache, update gauge metrics
    pub fn update_counts(&mut self) {
        gauge!(names::configuration::CLUSTERS, self.state.clusters.len());
        gauge!(names::configuration::BACKENDS, self.state.count_backends());
        gauge!(
            names::configuration::FRONTENDS,
            self.state.count_frontends()
        );
    }

    /// Queue an audit event for fan-out to subscribed clients. Drained by
    /// `CommandHub::flush_pending_audit_events` after every request handler.
    pub fn push_audit_event(&mut self, event: Event) {
        self.pending_audit_events.push_back(event);
    }

    fn next_session_token(&mut self) -> Token {
        // Token(0) is permanently reserved for the UnixListener; every handed
        // out session token must therefore be strictly positive.
        debug_assert!(
            self.next_session_id >= 1,
            "session ids start at 1; Token(0) is reserved for the listener"
        );
        let before = self.next_session_id;
        let token = Token(self.next_session_id);
        self.next_session_id += 1;
        debug_assert_eq!(
            self.next_session_id,
            before + 1,
            "session id counter must advance by exactly one"
        );
        token
    }
    fn next_client_id(&mut self) -> ClientId {
        let id = self.next_client_id;
        self.next_client_id += 1;
        // Monotonic, gap-free allocation: the next id is exactly one past the
        // one we just handed out, so no two clients ever share an id.
        debug_assert_eq!(
            self.next_client_id,
            id + 1,
            "client id counter must advance by exactly one"
        );
        id
    }

    fn next_task_id(&mut self) -> TaskId {
        let id = self.next_task_id;
        self.next_task_id += 1;
        debug_assert_eq!(
            self.next_task_id,
            id + 1,
            "task id counter must advance by exactly one"
        );
        id
    }

    fn next_worker_id(&mut self) -> WorkerId {
        let id = self.next_worker_id;
        self.next_worker_id += 1;
        debug_assert_eq!(
            self.next_worker_id,
            id + 1,
            "worker id counter must advance by exactly one"
        );
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
        // `next_session_token` hands out a strictly increasing token, so the
        // slot we are about to fill must be empty — a collision would evict a
        // live worker session and orphan its channel.
        debug_assert!(
            !self.workers.contains_key(&token),
            "register_worker must use a fresh, unoccupied session token"
        );
        let workers_before = self.workers.len();
        self.register(token, &mut channel.sock)?;
        self.workers.insert(
            token,
            WorkerSession::new(channel, worker_id, pid, token, scm_socket),
        );
        // INVARIANT: exactly one worker session was added, and it is the one
        // keyed by `token` carrying the id/pid we just registered. This is the
        // bookkeeping anchor every later lookup (`scatter_on`, `close_worker`,
        // `handle_worker_response`) relies on.
        debug_assert_eq!(
            self.workers.len(),
            workers_before + 1,
            "register_worker must add exactly one worker session"
        );
        debug_assert!(
            self.workers
                .get(&token)
                .is_some_and(|w| w.token == token && w.id == worker_id && w.pid == pid),
            "registered worker must map back from its token with matching id/pid"
        );
        self.workers
            .get_mut(&token)
            .ok_or(ServerError::WorkerNotFound)
    }

    /// Resolve a [`Timeout`] into the absolute instant the event loop compares
    /// against. Shared by [`Self::new_task`] and [`Self::rearm_task_timeout`]
    /// so both express the same policy once.
    fn deadline(&self, timeout: Timeout) -> Option<Instant> {
        match timeout {
            Timeout::None => None,
            Timeout::Default => Some(Duration::from_secs(self.config.worker_timeout as u64)),
            Timeout::Custom(duration) => Some(duration),
        }
        .map(|duration| Instant::now() + duration)
    }

    /// sozu#1313: re-arm the deadline of a task that is still queued.
    ///
    /// A bulk replay only learns how many entries it scattered once the state
    /// file is fully parsed, and its deadline must start counting from the END
    /// of the fan-out: a long parse would otherwise eat the whole budget
    /// before the first worker was even asked.
    pub fn rearm_task_timeout(&mut self, task_id: TaskId, timeout: Timeout) {
        let deadline = self.deadline(timeout);
        match self.queued_tasks.get_mut(&task_id) {
            Some(task) => task.timeout = deadline,
            None => error!("no queued task found with id {}", task_id),
        }
    }

    /// Add a task in a queue to make it accessible until the next tick
    pub fn new_task(&mut self, job: Box<dyn GatheringTask>, timeout: Timeout) -> TaskId {
        let task_id = self.next_task_id();
        // `next_task_id` is monotonic, so this id must be unused in the queue;
        // reusing one would silently drop the job already parked there.
        debug_assert!(
            !self.queued_tasks.contains_key(&task_id),
            "new_task must allocate a fresh, unused task id"
        );
        let queued_before = self.queued_tasks.len();
        let timeout = self.deadline(timeout);
        self.queued_tasks
            .insert(task_id, TaskContainer { job, timeout });
        // INVARIANT: exactly one task was queued, retrievable by the id we
        // return — the caller (`scatter`/`scatter_on`) immediately looks it
        // up by this id.
        debug_assert_eq!(
            self.queued_tasks.len(),
            queued_before + 1,
            "new_task must queue exactly one task"
        );
        debug_assert!(
            self.queued_tasks.contains_key(&task_id),
            "the queued task must be retrievable by the returned id"
        );
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
        if !self.queued_tasks.contains_key(&task_id) {
            error!("no task found with id {}", task_id);
            return;
        }

        let mut worker_count = 0;
        // sozu#1313: every (entry, worker) pair whose request could not even be
        // queued on the worker channel. It is still counted in `worker_count`
        // — and therefore in the expected-response budget — then answered with
        // a synthetic `Failure` below, so `has_finished` can fire and the
        // rollback attribution sees the rejection. Dropping it silently, as
        // before, made `ok + errors` unreachable and hung the task.
        let mut write_failures: Vec<(WorkerId, RequestId)> = Vec::new();
        let mut worker_request = WorkerRequest {
            id: String::new(),
            content: request,
        };

        // Snapshot the in-flight map size before the fan-out so the
        // post-condition can assert the per-call delta. Note the in-flight
        // map and `expected_responses` both accumulate across repeated
        // `scatter_on` calls on the same task (see `requests.rs::load_state`,
        // `upgrade.rs`), so only the increment of THIS call is assertable,
        // never an absolute total.
        let in_flight_before = self.in_flight.len();

        for worker in self.workers.values_mut().filter(|w| {
            target
                .map(|id| id == w.id && w.run_state != RunState::Stopped)
                .unwrap_or(w.run_state != RunState::Stopped)
        }) {
            worker_count += 1;
            worker_request.id = format!("{}-{}-{}", worker.id, task_id, request_id);
            debug!("scattering to worker {}: {:?}", worker.id, worker_request);
            match worker.send(&worker_request) {
                Ok(()) => {
                    self.in_flight.insert(worker_request.id.clone(), task_id);
                }
                // No response can ever arrive for a request that never left the
                // master, so no in-flight entry is registered for it.
                Err(_) => write_failures.push((worker.id, worker_request.id.clone())),
            }
        }
        if let Some(task) = self.queued_tasks.get_mut(&task_id) {
            task.job
                .get_gatherer()
                .on_scatter(request_id, worker_count, &worker_request.content);
        }

        // INVARIANT: every worker we scattered to within this call has a
        // distinct request id (the id embeds the unique worker id, plus the
        // task and request indices), so the in-flight map must have grown by
        // exactly the number of requests we managed to queue. A smaller delta
        // would mean an id collision overwrote an in-flight entry, which would
        // silently lose a worker's response and hang the task until timeout.
        debug_assert_eq!(
            self.in_flight.len(),
            in_flight_before + worker_count - write_failures.len(),
            "scatter_on must register exactly one in-flight entry per successfully queued request"
        );

        if write_failures.is_empty() {
            return;
        }
        // Take the task out of the queue so `&mut self` is free for
        // `on_message` — the very method the event loop uses for a real worker
        // answer, so a write failure is accounted through exactly one path.
        // The task is still QUEUED here (it only migrates into
        // `CommandHub::tasks` at the top of the next iteration), which is why
        // the synthetic failure is delivered here rather than through
        // `CommandHub::handle_worker_response`.
        let Some(mut container) = self.queued_tasks.remove(&task_id) else {
            return;
        };
        for (worker_id, failed_request_id) in write_failures {
            let response = WorkerResponse {
                id: failed_request_id,
                status: ResponseStatus::Failure.into(),
                // No operator value: the channel error carries buffer sizes
                // only and is already logged by `WorkerSession::send`.
                message: format!("could not queue the request on worker {worker_id}"),
                content: None,
            };
            container
                .job
                .get_gatherer()
                .on_message(self, &mut None, worker_id, response);
        }
        self.queued_tasks.insert(task_id, container);
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
        // POST-CONDITION: a closed worker is terminal — `Stopped` excludes it
        // from `is_active`/`alive_workers`/`scatter_on` targeting, so no later
        // request can be fanned out to a killed process.
        debug_assert_eq!(
            self.workers.get(token).map(|w| w.run_state),
            Some(RunState::Stopped),
            "close_worker must leave the worker in the Stopped run state"
        );
        debug_assert!(
            !self
                .workers
                .get(token)
                .is_some_and(WorkerSession::is_active),
            "a closed worker must not report as active"
        );
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
        config::{Config, DEFAULT_COMMAND_BUFFER_SIZE, DEFAULT_MAX_COMMAND_BUFFER_SIZE},
        proto::command::{
            AddBackend, CertificateSummary, CertificatesByAddress, Cluster,
            ListOfCertificatesByAddress, RequestHttpFrontend, RequestTcpFrontend, SocketAddress,
            WorkerResponse, request::RequestType, response_content::ContentType,
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

    fn create_test_hub() -> CommandHub {
        let dir = tempfile::tempdir().expect("Could not create temp dir");
        let socket_path = dir.path().join("test.sock");
        let unix_listener = UnixListener::bind(&socket_path).expect("Could not bind socket");
        CommandHub::new(unix_listener, Config::default(), "sozu".to_owned())
            .expect("Could not create command hub")
    }

    /// Regression (sozu#1430): the command-socket client channel is sized at
    /// `CLIENT_CHANNEL_INITIAL_BUFFER_SIZE`, never at the global
    /// `command_buffer_size`, and the two are deliberately independent.
    ///
    /// `command_buffer_size` sizes the one-or-few channel kinds — the
    /// supervisor↔worker channels and the CLI's own end of this very
    /// connection. This kind is many-per-process: `CommandHub::run`'s accept
    /// loop registers one client per `accept()` and caps nothing. The value is
    /// an address-space floor per client, and a resident one for every page a
    /// client touches, because `try_shrink_front_buf` / `try_shrink_back_buf`
    /// never shrink below it. `CLIENT_CHANNEL_INITIAL_BUFFER_SIZE` carries the
    /// measurements and the argument; this test only pins the wiring.
    ///
    /// A future change may still decide to follow the global; it then has to
    /// delete this test and argue the memory it costs, rather than move the
    /// per-client floor by two orders of magnitude in passing.
    #[test]
    fn client_channel_initial_capacity_is_independent_of_command_buffer_size() {
        let dir = tempfile::tempdir().expect("Could not create temp dir");
        let socket_path = dir.path().join("test.sock");
        let unix_listener = UnixListener::bind(&socket_path).expect("Could not bind socket");

        // Both keys are set EXPLICITLY, from the constants themselves. The
        // built-in defaults are applied by `ConfigBuilder::into_config`
        // (`command/src/config.rs`), which this test never runs, and `Config`
        // derives `Default` — so `..Default::default()` alone would leave both
        // at `0` and the fixture would prove nothing. Naming the constants
        // instead of copying their values keeps the fixture the case the
        // decision is about (a deployment that omits both keys) even if either
        // default is changed.
        let config = Config {
            command_buffer_size: DEFAULT_COMMAND_BUFFER_SIZE,
            max_command_buffer_size: DEFAULT_MAX_COMMAND_BUFFER_SIZE,
            ..Default::default()
        };
        assert_ne!(
            config.command_buffer_size, CLIENT_CHANNEL_INITIAL_BUFFER_SIZE,
            "the fixture must separate the two values or this test proves nothing"
        );

        let mut hub = CommandHub::new(unix_listener, config, "sozu".to_owned())
            .expect("Could not create command hub");

        let (_client_side, accepted) =
            std::os::unix::net::UnixStream::pair().expect("could not create a socket pair");
        accepted
            .set_nonblocking(true)
            .expect("could not set the accepted stream nonblocking");
        hub.register_client(UnixStream::from_std(accepted));

        assert_eq!(
            hub.clients.len(),
            1,
            "register_client must have inserted exactly one client session"
        );
        let session = hub
            .clients
            .values()
            .next()
            .expect("the registered client session");

        assert_eq!(
            session.channel.front_buf.capacity() as u64,
            CLIENT_CHANNEL_INITIAL_BUFFER_SIZE,
            "client channel front buffer must start at the per-client floor"
        );
        assert_eq!(
            session.channel.back_buf.capacity() as u64,
            CLIENT_CHANNEL_INITIAL_BUFFER_SIZE,
            "client channel back buffer must start at the per-client floor"
        );
    }

    /// Regression (sozu#1301, sozu#1314): `handle_finishing_task` must forward
    /// its `timed_out` argument to `GatheringTask::on_finish`, not a hard-coded
    /// literal. A previous `false` literal made `timed_out` always false in
    /// every `on_finish` — neutralising `WorkerTask`'s rollback decision, which
    /// reads that flag, and leaving the audit `FanoutStatus::Timeout`/`result`
    /// branches unreachable (a timed-out command reported success).
    ///
    /// This is the propagation half of the sozu#1314 guarantee: a panicking
    /// worker emits no response and no `expected_responses` decrement, so the
    /// timeout path is the only way its rejection ever reaches
    /// `should_rollback_fanout` (bin/src/command/requests.rs), whose decision
    /// table is pinned by `rollback_predicate_tests`.
    #[test]
    fn handle_finishing_task_forwards_timed_out_flag() {
        use std::cell::Cell;
        use std::rc::Rc;

        #[derive(Debug)]
        struct RecordingTask {
            gatherer: DefaultGatherer,
            seen: Rc<Cell<Option<bool>>>,
        }
        impl GatheringTask for RecordingTask {
            fn client_token(&self) -> Option<Token> {
                None
            }
            fn get_gatherer(&mut self) -> &mut dyn Gatherer {
                &mut self.gatherer
            }
            fn on_finish(
                self: Box<Self>,
                _server: &mut Server,
                _client: &mut OptionalClient,
                timed_out: bool,
            ) {
                self.seen.set(Some(timed_out));
            }
        }

        for expected in [false, true] {
            let mut hub = create_test_hub();
            let seen = Rc::new(Cell::new(None));
            let task = TaskContainer {
                job: Box::new(RecordingTask {
                    gatherer: DefaultGatherer::default(),
                    seen: seen.clone(),
                }),
                timeout: None,
            };
            // task_id 0 is fine: `handle_finishing_task` takes the task by value
            // and only uses the id to purge in-flight entries (none exist here).
            hub.handle_finishing_task(0, task, expected);
            assert_eq!(
                seen.get(),
                Some(expected),
                "handle_finishing_task must forward timed_out={expected} to on_finish"
            );
        }
    }

    #[derive(Debug)]
    struct SecretBearingTask {
        gatherer: DefaultGatherer,
        secret: String,
    }

    impl GatheringTask for SecretBearingTask {
        fn client_token(&self) -> Option<Token> {
            None
        }

        fn get_gatherer(&mut self) -> &mut dyn Gatherer {
            &mut self.gatherer
        }

        fn on_finish(
            self: Box<Self>,
            _server: &mut Server,
            _client: &mut OptionalClient,
            _timed_out: bool,
        ) {
        }
    }

    #[test]
    fn task_sink_debug_does_not_format_concrete_task_payloads() {
        const TASK_SECRET: &str = "RETAINED_TASK_PAYLOAD_SECRET_SENTINEL";

        let secret = format!("{TASK_SECRET}{}", "x".repeat(4096));
        let secret_len = secret.len();
        let job = SecretBearingTask {
            gatherer: DefaultGatherer::default(),
            secret,
        };
        assert_eq!(
            job.secret.len(),
            secret_len,
            "fixture must retain the full task payload before logging"
        );
        let task = TaskContainer {
            job: Box::new(job),
            timeout: None,
        };
        let output = format!("Task finish: {task:?}");

        assert!(
            !output.contains(TASK_SECRET),
            "task completion sink leaked the concrete task payload: {output}"
        );
        assert!(
            output.contains("SecretBearingTask"),
            "task completion sink omitted the bounded task kind: {output}"
        );
        assert!(
            output.len() <= 512,
            "task completion sink output is not bounded: {} bytes",
            output.len()
        );
    }

    #[test]
    fn default_gatherer_debug_bounds_retained_certificate_query_responses() {
        const DOMAIN_SECRET: &str = "RETAINED_QUERY_DOMAIN_SECRET_SENTINEL";
        const FINGERPRINT_SECRET: &str = "RETAINED_QUERY_FINGERPRINT_SECRET_SENTINEL";
        const RESPONSE_ID_SECRET: &str = "RETAINED_QUERY_RESPONSE_ID_SECRET_SENTINEL";
        const MESSAGE_SECRET: &str = "RETAINED_QUERY_MESSAGE_SECRET_SENTINEL";

        let long_value = |marker: &str| format!("{marker}{}", "x".repeat(4096));
        let content: ResponseContent =
            ContentType::CertificatesByAddress(ListOfCertificatesByAddress {
                certificates: vec![CertificatesByAddress {
                    address: Default::default(),
                    certificate_summaries: vec![CertificateSummary {
                        domain: long_value(DOMAIN_SECRET),
                        fingerprint: long_value(FINGERPRINT_SECRET),
                    }],
                }],
            })
            .into();
        let responses = (0..128)
            .map(|worker_id| {
                (
                    worker_id,
                    WorkerResponse {
                        id: long_value(RESPONSE_ID_SECRET),
                        status: ResponseStatus::Ok as i32,
                        message: long_value(MESSAGE_SECRET),
                        content: Some(content.clone()),
                    },
                )
            })
            .collect();
        let gatherer = DefaultGatherer {
            ok: 128,
            errors: 0,
            responses,
            expected_responses: 128,
        };

        let output = format!("{gatherer:?}");

        for secret in [
            DOMAIN_SECRET,
            FINGERPRINT_SECRET,
            RESPONSE_ID_SECRET,
            MESSAGE_SECRET,
        ] {
            assert!(
                !output.contains(secret),
                "DefaultGatherer Debug leaked retained response marker {secret}: {output}"
            );
        }
        assert!(
            output.contains("responses_count: 128"),
            "DefaultGatherer Debug omitted the retained response count: {output}"
        );
        assert!(
            output.len() <= 512,
            "DefaultGatherer Debug output is not cardinality-bounded: {} bytes",
            output.len()
        );
    }

    #[test]
    fn update_counts_reflects_state() {
        let mut server = create_test_server();

        // initially empty
        server.update_counts();
        assert_eq!(read_gauge(names::configuration::CLUSTERS), Some(0));
        assert_eq!(read_gauge(names::configuration::BACKENDS), Some(0));
        assert_eq!(read_gauge(names::configuration::FRONTENDS), Some(0));

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
        assert_eq!(read_gauge(names::configuration::CLUSTERS), Some(0));

        // update_counts should refresh gauges
        server.update_counts();
        assert_eq!(read_gauge(names::configuration::CLUSTERS), Some(1));
        assert_eq!(read_gauge(names::configuration::BACKENDS), Some(3));
        assert_eq!(read_gauge(names::configuration::FRONTENDS), Some(2));
    }
    /// Register a worker whose channel is capped at `max_buffer_size`, so the
    /// test controls exactly when a queued request stops fitting.
    fn register_test_worker(
        server: &mut Server,
        worker_id: WorkerId,
        buffer_size: u64,
        max_buffer_size: u64,
    ) -> (
        Channel<WorkerResponse, WorkerRequest>,
        std::os::unix::net::UnixStream,
    ) {
        let (main_sock, worker_sock) = UnixStream::pair().expect("could not create a socket pair");
        let main_side: Channel<WorkerRequest, WorkerResponse> =
            Channel::new(main_sock, buffer_size, max_buffer_size);
        // The worker end reads with a comfortable ceiling: only the master's
        // back buffer is under test here.
        let worker_side: Channel<WorkerResponse, WorkerRequest> =
            Channel::new(worker_sock, 4096, 65536);
        // `ScmSocket` borrows the descriptor; the stream is returned so it
        // outlives the worker session.
        let (scm_main, _scm_worker) =
            std::os::unix::net::UnixStream::pair().expect("could not create an scm pair");
        let scm_socket = ScmSocket::new(scm_main.as_raw_fd()).expect("could not create scm socket");
        server
            .register_worker(worker_id, 0, main_side, scm_socket)
            .expect("could not register the test worker");
        (worker_side, scm_main)
    }

    /// A task that records its final tally, so a test can assert what the
    /// gatherer accounted without reaching into a boxed `dyn GatheringTask`.
    #[derive(Debug)]
    struct TallyTask {
        gatherer: DefaultGatherer,
        seen: std::rc::Rc<std::cell::Cell<(usize, usize, usize)>>,
    }

    impl GatheringTask for TallyTask {
        fn client_token(&self) -> Option<Token> {
            None
        }
        fn get_gatherer(&mut self) -> &mut dyn Gatherer {
            &mut self.gatherer
        }
        fn on_finish(
            self: Box<Self>,
            _server: &mut Server,
            _client: &mut OptionalClient,
            _timed_out: bool,
        ) {
            self.seen.set((
                self.gatherer.ok,
                self.gatherer.errors,
                self.gatherer.expected_responses,
            ));
        }
    }

    /// Regression (sozu#1313): a request that can NEVER be delivered on a
    /// worker channel must be accounted as a `Failure` for the owning task.
    ///
    /// `scatter_on` counted every targeted worker in `expected_responses`
    /// whether or not the write succeeded, and `WorkerSession::send` only
    /// logged the error. `ok + errors` could therefore never reach `expected`,
    /// `has_finished` never fired, and on the bulk replay paths — which
    /// scattered with `Timeout::None` — the task and the client waiting on it
    /// hung forever, which is the "no responses at all" half of the incident.
    ///
    /// The frame here is larger than the channel ceiling itself, so no amount
    /// of draining will ever admit it: unlike a transient overflow it is NOT
    /// parked in `pending` (parking it would hang the task just as thoroughly),
    /// it fails immediately. See `sessions.rs::is_transient_overflow`.
    ///
    /// To SEE THIS RED: make `WorkerSession::send` swallow the error again
    /// (`return Ok(())` instead of `Err(e)`), or drop the synthetic-failure
    /// block at the end of `scatter_on` — `has_finished` is then false and the
    /// tally stays `(0, 0, 1)`.
    #[test]
    fn a_request_that_cannot_be_queued_is_accounted_as_a_failure() {
        let mut hub = create_test_hub();
        // An 8-byte ceiling cannot hold even the length prefix plus payload of
        // the smallest request, at any time.
        let (_worker_side, _scm) = register_test_worker(&mut hub.server, 0, 8, 8);

        let seen = std::rc::Rc::new(std::cell::Cell::new((0, 0, 0)));
        let task_id = hub.server.new_task(
            Box::new(TallyTask {
                gatherer: DefaultGatherer::default(),
                seen: seen.clone(),
            }),
            Timeout::None,
        );
        hub.server
            .scatter_on(RequestType::Status(Status {}).into(), task_id, 1, None);

        assert!(
            hub.server.in_flight.is_empty(),
            "a request that never left the master must register no in-flight entry"
        );
        let mut container = hub
            .server
            .queued_tasks
            .remove(&task_id)
            .expect("the task must still be queued");
        assert!(
            container.job.get_gatherer().has_finished(),
            "a write failure must complete the fan-out instead of hanging it"
        );
        hub.handle_finishing_task(task_id, container, false);
        assert_eq!(
            seen.get(),
            (0, 1, 1),
            "the unqueueable request must be accounted as exactly one failure"
        );
    }

    /// Regression (sozu#1313): closing a worker must only fail the requests
    /// that are STILL in flight on it, never the ones it already answered.
    ///
    /// `in_flight` was purged only when the whole task finished, so a worker
    /// that answered the first entries of a bulk replay and then crashed had
    /// EVERY one of its ids re-fed as a synthetic `Failure`. A replay the fleet
    /// actually applied was then reported to the client — and to the audit
    /// trail — as failed, and the per-entry rollback saw phantom rejections.
    ///
    /// To SEE THIS RED: drop the terminal-response `in_flight.remove` from
    /// `handle_worker_response` — the tally below becomes `(1, 1, 2)` and the
    /// task reports itself finished before worker 1 ever answered.
    #[test]
    fn closing_a_worker_only_fails_its_unanswered_requests() {
        let mut hub = create_test_hub();
        let (_worker_0, _scm_0) = register_test_worker(&mut hub.server, 0, 4096, 65536);
        let (_worker_1, _scm_1) = register_test_worker(&mut hub.server, 1, 4096, 65536);

        let seen = std::rc::Rc::new(std::cell::Cell::new((0, 0, 0)));
        let task_id = hub.server.new_task(
            Box::new(TallyTask {
                gatherer: DefaultGatherer::default(),
                seen: seen.clone(),
            }),
            Timeout::None,
        );
        hub.server
            .scatter_on(RequestType::Status(Status {}).into(), task_id, 1, None);
        assert_eq!(
            hub.server.in_flight.len(),
            2,
            "the entry must be in flight on both workers"
        );

        // Worker 0 applies the entry...
        hub.handle_worker_response(
            0,
            WorkerResponse {
                id: format!("0-{task_id}-1"),
                status: ResponseStatus::Ok.into(),
                message: String::new(),
                content: None,
            },
        );
        assert_eq!(
            hub.server.in_flight.len(),
            1,
            "an answered request must leave the in-flight map immediately"
        );
        // ...and then dies. Only worker 1 is still owed an answer.
        hub.fail_in_flight_requests_of_worker(0);

        let mut container = hub
            .server
            .queued_tasks
            .remove(&task_id)
            .expect("the task must still be queued");
        assert!(
            !container.job.get_gatherer().has_finished(),
            "the task must still wait for the worker that has not answered"
        );
        hub.handle_finishing_task(task_id, container, false);
        assert_eq!(
            seen.get(),
            (1, 0, 2),
            "a dead worker's already-answered requests must not be re-counted as failures"
        );
    }

    /// Regression (sozu#1313): a bulk scatter larger than the channel back
    /// buffer must be delivered in full and in order, not truncated at the
    /// ceiling.
    ///
    /// `load_state` queues every saved entry onto every worker inside ONE
    /// event-loop iteration, with no flush in between. Past `max_buffer_size`
    /// (2 MB by default) `write_delimited_message` refused the frame and the
    /// entry was dropped — while still being counted in `expected_responses`,
    /// so the task hung. The overflow now goes to the per-worker `pending`
    /// queue and is drained from the WRITABLE path, which is exactly what this
    /// test drives: no synchronous flush, no sleep, no blocked event loop.
    ///
    /// To SEE THIS RED: make `WorkerSession::send` return the
    /// `MessageTooLarge` error instead of queueing (and drop `flush_pending`)
    /// — only the handful of requests that fit under the ceiling arrive.
    #[test]
    fn a_bulk_scatter_larger_than_the_back_buffer_is_fully_delivered() {
        const ENTRIES: usize = 500;

        let mut hub = create_test_hub();
        // 4 KiB ceiling: 500 queued requests overflow it many times over,
        // exactly as a large state file overflows the 2 MB default.
        let (mut worker_side, _scm) = register_test_worker(&mut hub.server, 0, 512, 4096);

        let seen = std::rc::Rc::new(std::cell::Cell::new((0, 0, 0)));
        let task_id = hub.server.new_task(
            Box::new(TallyTask {
                gatherer: DefaultGatherer::default(),
                seen: seen.clone(),
            }),
            Timeout::None,
        );
        for request_id in 1..=ENTRIES {
            hub.server.scatter_on(
                RequestType::Status(Status {}).into(),
                task_id,
                request_id,
                None,
            );
        }

        assert_eq!(
            hub.server.in_flight.len(),
            ENTRIES,
            "every entry must be accepted for delivery, none dropped at the ceiling"
        );

        // Drive the event loop's WRITABLE path: `ready()` drains the back
        // buffer onto the socket and refills it from `pending`, exactly as the
        // supervisor does on each mio writability event. The worker end reads
        // in between, as a live worker would.
        let mut received = vec![];
        for _ in 0..(ENTRIES * 4) {
            for worker in hub.server.workers.values_mut() {
                worker.update_readiness(Ready::WRITABLE);
                let _ = worker.ready();
            }
            worker_side.handle_events(Ready::READABLE);
            received.extend(crate::command::sessions::extract_messages(&mut worker_side));
            if received.len() == ENTRIES {
                break;
            }
        }

        let ids: Vec<String> = received.into_iter().map(|request| request.id).collect();
        let expected: Vec<String> = (1..=ENTRIES)
            .map(|request_id| format!("0-{task_id}-{request_id}"))
            .collect();
        assert_eq!(
            ids, expected,
            "every scattered entry must reach the worker, in order, past the back buffer ceiling"
        );
    }
}
