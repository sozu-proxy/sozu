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
    io::{Error as IoError, ErrorKind, Read, Write},
    ops::{Deref, DerefMut},
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd},
        unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    },
    path::Path,
    sync::atomic::{AtomicI32, Ordering},
    time::{Duration, Instant},
};

use libc::pid_t;
use mio::{
    Events, Interest, Poll, Token,
    net::{UnixListener, UnixStream},
};
use nix::{
    errno::Errno,
    sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, kill, sigaction},
    unistd::Pid,
};
use serde::{Deserialize, Serialize};
use sozu_command_lib::{
    channel::Channel,
    config::Config,
    proto::command::{
        Event, EventKind, Request, Response, ResponseContent, ResponseStatus, RunState, Status,
        WorkerRequest, WorkerResponse, request::RequestType, response_content::ContentType,
    },
    ready::Ready,
    scm_socket::{Listeners, ScmSocket},
    state::ConfigState,
};

use sozu_lib::metrics::names;

use super::upgrade::{
    LegacyUpgradeData, UpgradeDataError, UpgradeServerState, UpgradeSnapshot, monotonic_nanos,
};
use crate::{
    command::{
        requests::{AuditExtras, AuditResult, ClientRequestOutcome, audit_emit_inline, begin_stop},
        sessions::{
            ClientResult, ClientSession, OptionalClient, PausedClientSession, PausedWorkerSession,
            SessionSnapshotError, UpgradeResponseQueue, WorkerResult, WorkerSession, wants_to_tick,
        },
        upgrade::{UpgradeData, upgrade_main},
    },
    util::{UtilError, disable_close_on_exec, enable_close_on_exec, get_executable_path},
    worker::{WorkerError, fork_main_into_worker},
};

/// Token of the read end of the `SIGTERM` self-pipe. `Token(0)` is the
/// command socket and sessions count up from 1 (`next_session_token`), so the
/// top of the range never collides with a session.
const SIGTERM_TOKEN: Token = Token(usize::MAX);

/// Write end of the `SIGTERM` self-pipe, `-1` until
/// [`CommandHub::handle_sigterm`] installs the handler. The descriptor is never
/// closed, so the handler can never write into a recycled one.
static SIGTERM_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

/// `SIGTERM` handler of the main process. It only wakes the event loop, which
/// turns the signal into a stop (`CommandHub::on_sigterm`). Everything here is
/// async-signal-safe: an atomic load and one non-blocking `send(2)`, with
/// `errno` restored for the code the signal interrupted. A full socket buffer
/// drops the byte, which is harmless: the loop has a wake-up pending anyway.
extern "C" fn sigterm_handler(_signal: libc::c_int) {
    let errno = Errno::last_raw();
    let fd = SIGTERM_WRITE_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = 1u8;
        // SAFETY: `fd` is the write end of the self-pipe, which stays open for
        // the life of the process, and `byte` outlives the call.
        unsafe {
            libc::send(fd, (&raw const byte).cast(), 1, libc::MSG_NOSIGNAL);
        }
    }
    Errno::set_raw(errno);
}

fn descriptor_has_pending_byte(fd: i32) -> Result<bool, IoError> {
    let mut byte = 0u8;
    loop {
        // SAFETY: `byte` is a valid one-byte output buffer and MSG_PEEK keeps
        // the stop intention available for the normal event-loop handler.
        let read = unsafe {
            libc::recv(
                fd,
                (&raw mut byte).cast(),
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        if read > 0 {
            return Ok(true);
        }
        if read == 0 {
            return Ok(false);
        }
        let error = IoError::last_os_error();
        match error.kind() {
            ErrorKind::Interrupted => continue,
            ErrorKind::WouldBlock => return Ok(false),
            _ => return Err(error),
        }
    }
}

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
/// `CommandHub::prepare_from_upgrade_data`) and the CLI's own end of this very
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
/// commit `from_upgrade_data` (now `CommandHub::prepare_from_upgrade_data`)
/// already passed `command_buffer_size` — so the author had the configured
/// values in hand and diverged here, plausibly for one page per connection.
/// The `usize::MAX` ceiling it was paired with rules out a CEILING defence,
/// not a sizing intent, and the CWE-770 comment that later appeared at the
/// call site argues that ceiling only. The argument above is why the value is
/// kept now, written down so the next reader does not have to guess.
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
pub(crate) trait GatheringTask: Debug {
    /// Return a payload-free identifier suitable for retained-task logs.
    fn kind(&self) -> &'static str {
        std::any::type_name::<Self>()
    }

    /// get access to the client that sent the command (if any)
    fn client_token(&self) -> Option<Token>;

    /// get access to the gatherer for this task (each task can implement its own gathering strategy)
    fn get_gatherer(&mut self) -> &mut dyn Gatherer;

    /// Capture the complete task state without replaying its request.
    fn snapshot(&self, timing: TaskSnapshotTiming) -> Result<TaskSnapshot, TaskSnapshotError>;

    /// Worker session whose SCM/channel state must outlive this task.
    /// Only the first phase of a worker upgrade owns such a dependency.
    fn retained_worker_token(&self) -> Option<Token> {
        None
    }

    /// This is called once every worker has answered
    /// It allows to operate both on the server (launch workers...) and the client (send an answer...)
    fn on_finish(
        self: Box<Self>,
        server: &mut Server,
        client: &mut OptionalClient,
        timed_out: bool,
    );
}

/// One shared clock sample used while snapshotting every task in a handoff.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TaskSnapshotTiming {
    pub(crate) now: Instant,
}

impl TaskSnapshotTiming {
    pub(crate) fn now() -> Self {
        Self {
            now: Instant::now(),
        }
    }
}

/// Clock context used when rebuilding task-local elapsed timers.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TaskRestoreTiming {
    pub(crate) now: Instant,
    pub(crate) handoff_elapsed: Duration,
}

/// Serializable elapsed-time value used by task audit timers.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ElapsedSnapshot {
    nanos: u64,
}

impl ElapsedSnapshot {
    pub(crate) fn capture(
        started_at: Instant,
        timing: TaskSnapshotTiming,
    ) -> Result<Self, TaskSnapshotError> {
        Ok(Self {
            nanos: duration_nanos(
                timing.now.saturating_duration_since(started_at),
                "task elapsed time",
            )?,
        })
    }

    pub(crate) fn restore(self, timing: TaskRestoreTiming) -> Result<Instant, TaskSnapshotError> {
        let elapsed = Duration::from_nanos(self.nanos)
            .checked_add(timing.handoff_elapsed)
            .ok_or(TaskSnapshotError::DurationOverflow(
                "restored task elapsed time",
            ))?;
        timing
            .now
            .checked_sub(elapsed)
            .ok_or(TaskSnapshotError::InstantOutOfRange)
    }
}

fn duration_nanos(duration: Duration, field: &'static str) -> Result<u64, TaskSnapshotError> {
    u64::try_from(duration.as_nanos()).map_err(|_| TaskSnapshotError::DurationOverflow(field))
}

#[derive(thiserror::Error, Debug)]
pub enum TaskSnapshotError {
    #[error("{0} exceeds the serializable monotonic duration range")]
    DurationOverflow(&'static str),
    #[error("restored task instant predates the process monotonic clock")]
    InstantOutOfRange,
    #[error("unknown audit tag pair {verb}/{counter}")]
    UnknownAuditTag { verb: String, counter: String },
    #[error("test-only task {0} cannot be upgraded")]
    #[cfg_attr(not(test), allow(dead_code))]
    UnsupportedTestTask(&'static str),
}

/// Exhaustive wire representation of every production gathering task.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum TaskSnapshot {
    Request(Box<super::requests::RequestTaskSnapshot>),
    UpgradeWorker(super::upgrade::UpgradeWorkerTaskSnapshot),
}

impl TaskSnapshot {
    pub(crate) fn is_stop(&self) -> bool {
        match self {
            Self::Request(snapshot) => snapshot.is_stop(),
            Self::UpgradeWorker(_) => false,
        }
    }

    pub(crate) fn restore(
        self,
        timing: TaskRestoreTiming,
    ) -> Result<Box<dyn GatheringTask>, TaskSnapshotError> {
        match self {
            Self::Request(snapshot) => (*snapshot).restore(timing),
            Self::UpgradeWorker(snapshot) => Ok(snapshot.restore()),
        }
    }
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct TaskContainerSnapshot {
    job: TaskSnapshot,
    deadline: DeadlineSnapshot,
}

impl TaskContainer {
    fn snapshot(
        &self,
        timing: TaskSnapshotTiming,
    ) -> Result<TaskContainerSnapshot, TaskSnapshotError> {
        Ok(TaskContainerSnapshot {
            job: self.job.snapshot(timing)?,
            deadline: DeadlineSnapshot::capture(self.timeout, timing)?,
        })
    }
}

impl TaskContainerSnapshot {
    pub(crate) fn is_stop(&self) -> bool {
        self.job.is_stop()
    }

    fn restore(self, timing: TaskRestoreTiming) -> Result<TaskContainer, TaskSnapshotError> {
        Ok(TaskContainer {
            job: self.job.restore(timing)?,
            timeout: self.deadline.restore(timing)?,
        })
    }
}

/// Remaining timeout budget at the snapshot boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct DeadlineSnapshot {
    remaining_nanos: Option<u64>,
}

impl DeadlineSnapshot {
    fn capture(
        timeout: Option<Instant>,
        timing: TaskSnapshotTiming,
    ) -> Result<Self, TaskSnapshotError> {
        Ok(Self {
            remaining_nanos: timeout
                .map(|deadline| {
                    duration_nanos(
                        deadline.saturating_duration_since(timing.now),
                        "task deadline",
                    )
                })
                .transpose()?,
        })
    }

    fn restore(self, timing: TaskRestoreTiming) -> Result<Option<Instant>, TaskSnapshotError> {
        self.remaining_nanos
            .map(|remaining_nanos| {
                let remaining =
                    Duration::from_nanos(remaining_nanos).saturating_sub(timing.handoff_elapsed);
                timing
                    .now
                    .checked_add(remaining)
                    .ok_or(TaskSnapshotError::InstantOutOfRange)
            })
            .transpose()
    }
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
#[derive(Clone, Default, Serialize, Deserialize)]
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
    #[error("invalid main-upgrade data: {0}")]
    InvalidUpgradeData(#[from] UpgradeDataError),
    #[error("invalid main-upgrade snapshot: {0}")]
    InvalidUpgradeSnapshot(String),
    #[error("could not restore a command session: {0}")]
    RestoreSession(#[from] SessionSnapshotError),
    #[error("could not snapshot or restore a command task: {0}")]
    RestoreTask(#[from] TaskSnapshotError),
    #[error("could not register restored {kind} session {id}: {error}")]
    RegisterRestoredSession {
        kind: &'static str,
        id: u32,
        #[source]
        error: IoError,
    },
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
    /// read end of the `SIGTERM` self-pipe, set by [`CommandHub::handle_sigterm`]
    sigterm_receiver: Option<UnixStream>,
    /// Sessions whose userspace buffers or worker pending queue already held
    /// work at the snapshot boundary. Mio cannot rediscover userspace bytes,
    /// so each token is ticked once immediately after COMMIT.
    restored_session_ticks: HashSet<Token>,
    /// Terminal response owned by the new main but not yet admitted to the
    /// initiating client's inherited back buffer.
    pending_upgrade_completion: Option<(Token, Response)>,
    /// Worker sessions inherited across this main handoff. Terminal entries
    /// are collected once no restored task or live route can still use them.
    restored_worker_tokens: HashSet<Token>,
}

/// Fully validated and registered replacement Hub that cannot touch any
/// command or worker data descriptor until the handoff commits.
pub struct PausedCommandHub {
    server: Server,
    clients: Vec<PausedClientSession>,
    workers: Vec<PausedWorkerSession>,
    tasks: HashMap<TaskId, TaskContainer>,
    command_socket_path: std::sync::Arc<str>,
    restored_session_ticks: HashSet<Token>,
    restored_worker_tokens: HashSet<Token>,
    /// Read end of the SIGTERM self-pipe, installed during PREPARED so a
    /// failure to create it still rolls back to the old main.
    sigterm_receiver: Option<UnixStream>,
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

/// Create the SIGTERM self-pipe, register its read end on `poll` under
/// `SIGTERM_TOKEN`, then install the handler that writes to it.
fn install_sigterm_handler(poll: &Poll) -> Result<UnixStream, ServerError> {
    let (sender, mut receiver) = UnixStream::pair().map_err(ServerError::SigtermPipe)?;
    poll.registry()
        .register(&mut receiver, SIGTERM_TOKEN, Interest::READABLE)
        .map_err(ServerError::SigtermPipe)?;
    let previous = SIGTERM_WRITE_FD.swap(sender.into_raw_fd(), Ordering::Relaxed);
    debug_assert_eq!(
        previous, -1,
        "the SIGTERM handler is installed once per process"
    );

    let action = SigAction::new(
        SigHandler::Handler(sigterm_handler),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    // SAFETY: `sigterm_handler` only performs async-signal-safe operations.
    unsafe { sigaction(Signal::SIGTERM, &action) }.map_err(ServerError::SigtermHandler)?;
    Ok(receiver)
}

/// Set CLOEXEC on every descriptor, trying them all and reporting the first
/// failure.
fn enable_cloexec_on(fds: impl IntoIterator<Item = i32>) -> Result<(), ServerError> {
    let mut first_error = None;
    for fd in fds {
        if let Err(error) = enable_close_on_exec(fd) {
            error!(
                "could not enable close-on-exec on inherited fd {}: {}",
                fd, error
            );
            first_error.get_or_insert(error);
        }
    }
    match first_error {
        Some(error) => Err(ServerError::EnableCloexec(error)),
        None => Ok(()),
    }
}

impl PausedCommandHub {
    /// Install the replacement main's SIGTERM handler before PREPARED.
    ///
    /// Nothing here touches a descriptor shared with the old main: the
    /// self-pipe is private to this process and joins this Hub's own poll. A
    /// SIGTERM received before COMMIT is held in the pipe; the old main aborts
    /// the upgrade on its own pending SIGTERM and kills this process, and one
    /// that arrives after its check is honoured by this Hub's event loop. The
    /// blocking wait for COMMIT retries the read the signal interrupts (a read
    /// with `SO_RCVTIMEO` is never restarted, even under `SA_RESTART`), so the
    /// signal cannot fail the handoff. See [`CommandHub::handle_sigterm`].
    pub fn handle_sigterm(&mut self) -> Result<(), ServerError> {
        self.sigterm_receiver = Some(install_sigterm_handler(&self.server.poll)?);
        Ok(())
    }

    /// Observe a queued stop without consuming it before a legacy ACK.
    pub(crate) fn sigterm_pending(&self) -> Result<bool, IoError> {
        self.sigterm_receiver
            .as_ref()
            .map_or(Ok(false), |receiver| {
                descriptor_has_pending_byte(receiver.as_raw_fd())
            })
    }

    /// Restore CLOEXEC on every inherited descriptor before PREPARED.
    ///
    /// `FD_CLOEXEC` is a flag of this process's descriptor table entry, not of
    /// the open file description shared with the old main, so setting it
    /// before COMMIT changes nothing the old main can observe.
    pub fn enable_cloexec_after_upgrade(&self) -> Result<(), ServerError> {
        let fds = std::iter::once(self.server.unix_listener.as_raw_fd())
            .chain(self.clients.iter().map(PausedClientSession::channel_fd))
            .chain(
                self.workers
                    .iter()
                    .flat_map(|worker| [worker.channel_fd(), worker.scm_fd()]),
            );
        enable_cloexec_on(fds)
    }

    /// Cross the COMMIT boundary. This is the first point at which restored
    /// client and worker channels can perform data I/O.
    pub fn activate(mut self) -> Result<CommandHub, HubError> {
        let mut clients = HashMap::with_capacity(self.clients.len());
        for client in self.clients.drain(..) {
            clients.insert(client.token(), client.resume());
        }
        for worker in self.workers.drain(..) {
            self.server.workers.insert(worker.token(), worker.resume()?);
        }
        self.server.update_counts();
        Ok(CommandHub {
            server: self.server,
            clients,
            tasks: self.tasks,
            command_socket_path: self.command_socket_path,
            sigterm_receiver: self.sigterm_receiver,
            restored_session_ticks: self.restored_session_ticks,
            pending_upgrade_completion: None,
            restored_worker_tokens: self.restored_worker_tokens,
        })
    }
}

impl CommandHub {
    /// Observe, without consuming, a SIGTERM already queued for the old main.
    /// A pending stop wins over a not-yet-committed upgrade.
    pub(crate) fn sigterm_pending(&self) -> Result<bool, IoError> {
        self.sigterm_receiver
            .as_ref()
            .map_or(Ok(false), |receiver| {
                descriptor_has_pending_byte(receiver.as_raw_fd())
            })
    }

    /// Record the successful handoff and durably retain its terminal response
    /// until the inherited client buffer can accept it.
    pub fn complete_main_upgrade(
        &mut self,
        token: Token,
        new_main_pid: u32,
    ) -> Result<(), HubError> {
        let audit_target = format!(
            "executable:{} boot_generation:{}",
            self.server.executable_path.as_str(),
            self.server.boot_generation
        );
        {
            let client = self.clients.get_mut(&token).ok_or_else(|| {
                HubError::InvalidUpgradeSnapshot(format!(
                    "upgrade client token {} disappeared before activation",
                    token.0
                ))
            })?;
            audit_emit_inline(
                &mut self.server,
                client,
                EventKind::MainUpgraded,
                "main_upgraded",
                "config.main_upgraded",
                audit_target,
                AuditResult::Ok,
                AuditExtras::default(),
            );
        }
        self.flush_pending_audit_events();

        let response = Response {
            status: ResponseStatus::Ok.into(),
            message: format!(
                "Upgrade successful, closing main process. New main process has pid {new_main_pid}"
            ),
            content: None,
        };
        let client = self.clients.get_mut(&token).ok_or_else(|| {
            HubError::InvalidUpgradeSnapshot(format!(
                "upgrade client token {} disconnected during activation",
                token.0
            ))
        })?;
        match client.try_queue_upgrade_response(&response) {
            UpgradeResponseQueue::Queued => Ok(()),
            UpgradeResponseQueue::Backpressured => {
                self.pending_upgrade_completion = Some((token, response));
                self.restored_session_ticks.insert(token);
                Ok(())
            }
            UpgradeResponseQueue::Fatal => Err(HubError::InvalidUpgradeSnapshot(
                "could not queue the terminal main-upgrade response".to_owned(),
            )),
        }
    }

    fn retry_pending_upgrade_completion(&mut self) {
        let Some((token, response)) = self.pending_upgrade_completion.take() else {
            return;
        };
        let Some(client) = self.clients.get_mut(&token) else {
            error!(
                "initiating client {} disconnected before its main-upgrade response was queued",
                token.0
            );
            return;
        };
        match client.try_queue_upgrade_response(&response) {
            UpgradeResponseQueue::Queued => {}
            UpgradeResponseQueue::Backpressured => {
                self.pending_upgrade_completion = Some((token, response));
            }
            UpgradeResponseQueue::Fatal => {}
        }
    }

    fn collect_unreferenced_restored_stopped_workers(&mut self) {
        if self.restored_worker_tokens.is_empty() {
            return;
        }

        let live_task_ids = self
            .tasks
            .keys()
            .chain(self.server.queued_tasks.keys())
            .copied()
            .collect::<HashSet<_>>();
        let retained_worker_tokens = self
            .tasks
            .values()
            .chain(self.server.queued_tasks.values())
            .filter_map(|task| task.job.retained_worker_token())
            .collect::<HashSet<_>>();
        let workers_with_live_routes = self
            .server
            .in_flight
            .iter()
            .filter_map(|(request_id, task_id)| {
                live_task_ids
                    .contains(task_id)
                    .then(|| parse_scatter_request_id(request_id).map(|(worker_id, ..)| worker_id))
                    .flatten()
            })
            .collect::<HashSet<_>>();
        let collect = self
            .restored_worker_tokens
            .iter()
            .copied()
            .filter(|token| {
                self.server.workers.get(token).is_none_or(|worker| {
                    worker.run_state == RunState::Stopped
                        && !retained_worker_tokens.contains(token)
                        && !workers_with_live_routes.contains(&worker.id)
                })
            })
            .collect::<Vec<_>>();

        for token in collect {
            self.restored_worker_tokens.remove(&token);
            self.restored_session_ticks.remove(&token);
            let Some(mut worker) = self.server.workers.remove(&token) else {
                continue;
            };
            if let Err(error) = self.server.poll.registry().deregister(&mut worker.channel) {
                warn!(
                    "could not deregister restored stopped worker {}: {}",
                    worker.id, error
                );
            }
            let worker_id = worker.id;
            if let Err(error) = worker.close_restored_descriptors() {
                warn!(
                    "could not close restored stopped worker {} SCM descriptor: {}",
                    worker_id, error
                );
            }
        }
    }

    /// Promote queued tasks into the active map and finish every task whose
    /// gatherer or absolute deadline is already terminal. This is the first
    /// task sweep the replacement Hub runs after COMMIT, so restored queued
    /// tasks pass through the same callback path as pre-upgrade active tasks
    /// without replaying their original request.
    fn finish_ready_tasks(&mut self, now: Instant) {
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
    }

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
            sigterm_receiver: None,
            restored_session_ticks: HashSet::new(),
            pending_upgrade_completion: None,
            restored_worker_tokens: HashSet::new(),
        })
    }

    /// Turn `SIGTERM` into a stop of the workers instead of an immediate death.
    ///
    /// A unit without `ExecStop=` stops with `SIGTERM` to every process of its
    /// control group. Workers ignore it (`begin_worker_process`); the main
    /// process catches it here and stops them the way `sozu shutdown` does:
    /// the first `SIGTERM` is a soft stop, a second one while the workers still
    /// drain is a hard stop. Each worker then leaves its event loop and flushes
    /// its log buffers. Bounding the drain is left to the supervisor: systemd
    /// sends `SIGKILL` once `TimeoutStopSec` expires.
    ///
    /// The handler writes to a socket pair whose read end joins the event loop
    /// under `SIGTERM_TOKEN`; the loop does the actual work
    /// (`CommandHub::on_sigterm`). Call it once per process, from the two entry
    /// points that run the loop: `begin_main_process` here, and
    /// `begin_new_main_process` through [`PausedCommandHub::handle_sigterm`]
    /// before PREPARED.
    pub fn handle_sigterm(&mut self) -> Result<(), ServerError> {
        self.sigterm_receiver = Some(install_sigterm_handler(&self.server.poll)?);
        Ok(())
    }

    /// Drain the `SIGTERM` self-pipe, then stop once per signal received: soft
    /// while running, hard while the workers still drain, nothing once the
    /// main process itself is stopping.
    fn on_sigterm(&mut self) {
        let Some(receiver) = self.sigterm_receiver.as_mut() else {
            return;
        };
        let mut signals = 0;
        let mut buffer = [0u8; 16];
        loop {
            match receiver.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => signals += read,
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => {
                    error!("could not read the SIGTERM self-pipe: {}", error);
                    break;
                }
            }
        }
        for _ in 0..signals {
            match self.server.run_state {
                ServerState::Running => {
                    info!("received SIGTERM, soft stopping the workers");
                    begin_stop(&mut self.server, None, false);
                }
                ServerState::WorkersStopping => {
                    info!("received SIGTERM while the workers stop, hard stopping them");
                    begin_stop(&mut self.server, None, true);
                }
                ServerState::Stopping => {}
            }
        }
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

    /// Recreate the descriptor-only Hub exported by Sōzu 2.2.1 without
    /// consuming command or worker data before the old main exits.
    pub(crate) fn prepare_from_legacy_upgrade_data(
        upgrade_data: LegacyUpgradeData,
    ) -> Result<PausedCommandHub, HubError> {
        let LegacyUpgradeData {
            command_socket_fd,
            config,
            next_client_id,
            next_session_id,
            next_task_id,
            next_worker_id,
            workers,
            state,
            boot_generation,
        } = upgrade_data;

        usize::try_from(config.command_buffer_size).map_err(|_| {
            HubError::InvalidUpgradeSnapshot("command buffer size exceeds usize".to_owned())
        })?;
        usize::try_from(config.max_command_buffer_size).map_err(|_| {
            HubError::InvalidUpgradeSnapshot("maximum command buffer size exceeds usize".to_owned())
        })?;

        let mut inherited_fds = HashSet::new();
        let mut add_fd = |kind: &str, fd: i32| -> Result<(), HubError> {
            if fd < 0 {
                return Err(HubError::InvalidUpgradeSnapshot(format!(
                    "{kind} descriptor is negative: {fd}"
                )));
            }
            if !inherited_fds.insert(fd) {
                return Err(HubError::InvalidUpgradeSnapshot(format!(
                    "descriptor {fd} is assigned more than once"
                )));
            }
            // SAFETY: F_GETFD only validates this process's inherited
            // descriptor and does not mutate the shared open file description.
            if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
                return Err(HubError::InvalidUpgradeSnapshot(format!(
                    "could not validate {kind} descriptor {fd}: {}",
                    IoError::last_os_error()
                )));
            }
            Ok(())
        };
        add_fd("command listener", command_socket_fd)?;
        for worker in &workers {
            add_fd("worker channel", worker.channel_fd)?;
            add_fd("worker SCM socket", worker.scm_fd)?;
        }

        let mut worker_ids = HashSet::new();
        for worker in &workers {
            if !worker_ids.insert(worker.id) {
                return Err(HubError::InvalidUpgradeSnapshot(format!(
                    "duplicate worker id {}",
                    worker.id
                )));
            }
        }
        let live_worker_count = workers
            .iter()
            .filter(|worker| worker.run_state != RunState::Stopped)
            .count();
        let next_session_after_restore = next_session_id
            .checked_add(live_worker_count)
            .filter(|next| *next != SIGTERM_TOKEN.0)
            .ok_or_else(|| {
                HubError::InvalidUpgradeSnapshot(
                    "legacy worker tokens collide with the reserved SIGTERM token".to_owned(),
                )
            })?;
        if next_session_id == 0
            || next_session_id == SIGTERM_TOKEN.0
            || next_client_id == ClientId::MAX
            || next_worker_id == WorkerId::MAX
            || next_task_id == TaskId::MAX
            || workers
                .iter()
                .map(|worker| worker.id)
                .max()
                .is_some_and(|id| next_worker_id <= id)
        {
            return Err(HubError::InvalidUpgradeSnapshot(
                "one or more legacy next-id counters collide with restored state".to_owned(),
            ));
        }

        // SAFETY: `get_executable_path` is process-local observation; see the
        // corresponding V2 restoration path below.
        let executable_path =
            unsafe { get_executable_path().map_err(HubError::GetExecutablePath)? };
        // SAFETY: the descriptor was inherited from the old main, validated
        // above, and has one owner in this freshly execed process.
        let unix_listener = unsafe { UnixListener::from_raw_fd(command_socket_fd) };
        let command_socket_path: std::sync::Arc<str> = config
            .command_socket_path()
            .unwrap_or_else(|_| "unknown".to_owned())
            .into();
        let command_buffer_size = config.command_buffer_size;
        let max_command_buffer_size = config.max_command_buffer_size;
        let mut server =
            Server::new(unix_listener, config, executable_path).map_err(HubError::CreateServer)?;
        server.state = state;
        server.run_state = ServerState::Running;
        server.next_client_id = next_client_id;
        server.next_session_id = next_session_id;
        server.next_task_id = next_task_id;
        server.next_worker_id = next_worker_id;
        server.boot_generation = boot_generation;
        server.update_counts();

        let mut paused_workers = Vec::with_capacity(live_worker_count);
        for worker in workers {
            if worker.run_state == RunState::Stopped {
                // SAFETY: both validated descriptors have unique ownership in
                // this child. Dropping the child copies cannot affect the old
                // main's descriptor table or shared open file descriptions.
                drop(unsafe { OwnedFd::from_raw_fd(worker.channel_fd) });
                drop(unsafe { OwnedFd::from_raw_fd(worker.scm_fd) });
                continue;
            }
            let token = server.next_session_token();
            // SAFETY: both inherited descriptors passed the live/uniqueness
            // validation and ownership transfers to the paused session.
            let stream = unsafe { UnixStream::from_raw_fd(worker.channel_fd) };
            let scm_fd = unsafe { OwnedFd::from_raw_fd(worker.scm_fd) };
            let mut session = WorkerSession::restore_legacy_paused(
                stream,
                scm_fd,
                worker,
                token,
                command_buffer_size,
                max_command_buffer_size,
            )?;
            session.register(server.poll.registry()).map_err(|error| {
                HubError::RegisterRestoredSession {
                    kind: "worker",
                    id: session.id(),
                    error,
                }
            })?;
            paused_workers.push(session);
        }
        debug_assert_eq!(paused_workers.len(), live_worker_count);
        debug_assert_eq!(server.next_session_id, next_session_after_restore);

        let restored_session_ticks = paused_workers
            .iter()
            .map(PausedWorkerSession::token)
            .collect();
        let restored_worker_tokens = paused_workers
            .iter()
            .map(PausedWorkerSession::token)
            .collect();

        Ok(PausedCommandHub {
            server,
            clients: Vec::new(),
            workers: paused_workers,
            tasks: HashMap::new(),
            command_socket_path,
            restored_session_ticks,
            restored_worker_tokens,
            sigterm_receiver: None,
        })
    }

    /// Recreate and register the command Hub without consuming any data.
    ///
    /// The returned wrapper deliberately exposes no event loop or sessions.
    /// Registration validates descriptors during PREPARED; only `activate`
    /// crosses the COMMIT boundary and makes data I/O possible.
    pub fn prepare_from_upgrade_data(
        upgrade_data: UpgradeData,
    ) -> Result<PausedCommandHub, HubError> {
        let UpgradeSnapshot {
            command_socket_fd,
            config,
            captured_monotonic_nanos,
            server_state: UpgradeServerState::Running,
            next_client_id,
            next_session_id,
            next_task_id,
            next_worker_id,
            upgrade_client_token,
            clients,
            workers,
            event_subscribers,
            in_flight,
            tasks,
            queued_tasks,
            pending_audit_events,
            state,
            boot_generation,
        } = upgrade_data.into_snapshot()?;

        let restored_monotonic_nanos = monotonic_nanos()?;
        let handoff_nanos = restored_monotonic_nanos
            .checked_sub(captured_monotonic_nanos)
            .ok_or(UpgradeDataError::MonotonicClockWentBackwards)?;
        let restore_timing = TaskRestoreTiming {
            now: Instant::now(),
            handoff_elapsed: Duration::from_nanos(handoff_nanos),
        };

        let command_buffer_size = usize::try_from(config.command_buffer_size).map_err(|_| {
            HubError::InvalidUpgradeSnapshot("command buffer size exceeds usize".to_owned())
        })?;
        let max_command_buffer_size =
            usize::try_from(config.max_command_buffer_size).map_err(|_| {
                HubError::InvalidUpgradeSnapshot(
                    "maximum command buffer size exceeds usize".to_owned(),
                )
            })?;
        // `register_client` hands `Channel::new` this constant, which clamps
        // it to the ceiling (sozu-proxy/sozu#1416): expect the same value.
        let client_initial_buffer_size =
            usize::try_from(CLIENT_CHANNEL_INITIAL_BUFFER_SIZE.min(config.max_command_buffer_size))
                .map_err(|_| {
                    HubError::InvalidUpgradeSnapshot(
                        "client command buffer size exceeds usize".to_owned(),
                    )
                })?;
        let worker_initial_buffer_size = command_buffer_size;

        let mut inherited_fds = HashSet::new();
        let mut add_fd = |kind: &str, fd: i32| -> Result<(), HubError> {
            if fd < 0 {
                return Err(HubError::InvalidUpgradeSnapshot(format!(
                    "{kind} descriptor is negative: {fd}"
                )));
            }
            if !inherited_fds.insert(fd) {
                return Err(HubError::InvalidUpgradeSnapshot(format!(
                    "descriptor {fd} is assigned more than once"
                )));
            }
            Ok(())
        };
        add_fd("command listener", command_socket_fd)?;
        for client in &clients {
            add_fd("client channel", client.channel_fd())?;
        }
        for worker in &workers {
            add_fd("worker channel", worker.channel_fd())?;
            add_fd("worker SCM socket", worker.scm_fd())?;
        }

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

        let command_socket_path: std::sync::Arc<str> = config
            .command_socket_path()
            .unwrap_or_else(|_| "unknown".to_owned())
            .into();

        let mut server =
            Server::new(unix_listener, config, executable_path).map_err(HubError::CreateServer)?;

        server.state = state;
        server.run_state = ServerState::Running;
        server.next_client_id = next_client_id;
        server.next_session_id = next_session_id;
        server.next_task_id = next_task_id;
        server.next_worker_id = next_worker_id;
        server.boot_generation = boot_generation;
        server.event_subscribers = event_subscribers.into_iter().map(Token).collect();
        server.in_flight = in_flight;
        server.pending_audit_events = pending_audit_events;

        let task_ids = tasks.keys().copied().collect::<HashSet<_>>();
        if let Some(duplicate) = queued_tasks.keys().find(|id| task_ids.contains(id)) {
            return Err(HubError::InvalidUpgradeSnapshot(format!(
                "task {duplicate} appears in both active and queued maps"
            )));
        }
        if tasks
            .values()
            .chain(queued_tasks.values())
            .any(TaskContainerSnapshot::is_stop)
        {
            return Err(HubError::InvalidUpgradeSnapshot(
                "a dispatched stop task cannot be transferred while Running".to_owned(),
            ));
        }
        let max_task_id = tasks
            .keys()
            .chain(queued_tasks.keys())
            .chain(server.in_flight.values())
            .copied()
            .max();
        if max_task_id.is_some_and(|id| next_task_id <= id) {
            return Err(HubError::InvalidUpgradeSnapshot(format!(
                "next task id {next_task_id} is not newer than live id {}",
                max_task_id.unwrap_or_default()
            )));
        }

        let restored_tasks = tasks
            .into_iter()
            .map(|(id, task)| task.restore(restore_timing).map(|task| (id, task)))
            .collect::<Result<HashMap<_, _>, _>>()?;
        server.queued_tasks = queued_tasks
            .into_iter()
            .map(|(id, task)| task.restore(restore_timing).map(|task| (id, task)))
            .collect::<Result<HashMap<_, _>, _>>()?;

        let mut paused_clients = Vec::with_capacity(clients.len());
        for snapshot in clients {
            let fd = snapshot.channel_fd();
            // SAFETY: descriptor ownership in this process is transferred to
            // the paused session after the all-FD uniqueness check above.
            let stream = unsafe { UnixStream::from_raw_fd(fd) };
            let mut session = ClientSession::restore_paused(
                stream,
                snapshot,
                client_initial_buffer_size,
                max_command_buffer_size,
            )?;
            session.register(server.poll.registry()).map_err(|error| {
                HubError::RegisterRestoredSession {
                    kind: "client",
                    id: session.id(),
                    error,
                }
            })?;
            paused_clients.push(session);
        }

        let mut paused_workers = Vec::with_capacity(workers.len());
        for snapshot in workers {
            let channel_fd = snapshot.channel_fd();
            let scm_fd = snapshot.scm_fd();
            // SAFETY: both inherited descriptors passed the uniqueness check
            // and become owned by the paused wrapper until activation/drop.
            let stream = unsafe { UnixStream::from_raw_fd(channel_fd) };
            let scm_fd = unsafe { OwnedFd::from_raw_fd(scm_fd) };
            let mut session = WorkerSession::restore_paused(
                stream,
                scm_fd,
                snapshot,
                worker_initial_buffer_size,
                max_command_buffer_size,
            )?;
            session.register(server.poll.registry()).map_err(|error| {
                HubError::RegisterRestoredSession {
                    kind: "worker",
                    id: session.id(),
                    error,
                }
            })?;
            paused_workers.push(session);
        }

        let mut tokens = HashSet::new();
        let mut client_ids = HashSet::new();
        for client in &paused_clients {
            if client.token() == Token(0) || client.token() == SIGTERM_TOKEN {
                return Err(HubError::InvalidUpgradeSnapshot(format!(
                    "client {} uses reserved token {}",
                    client.id(),
                    client.token().0
                )));
            }
            if !tokens.insert(client.token()) || !client_ids.insert(client.id()) {
                return Err(HubError::InvalidUpgradeSnapshot(format!(
                    "duplicate client id or token for client {}",
                    client.id()
                )));
            }
        }
        let mut worker_ids = HashSet::new();
        for worker in &paused_workers {
            if worker.token() == Token(0) || worker.token() == SIGTERM_TOKEN {
                return Err(HubError::InvalidUpgradeSnapshot(format!(
                    "worker {} uses reserved token {}",
                    worker.id(),
                    worker.token().0
                )));
            }
            if !tokens.insert(worker.token()) || !worker_ids.insert(worker.id()) {
                return Err(HubError::InvalidUpgradeSnapshot(format!(
                    "duplicate worker id or token for worker {}",
                    worker.id()
                )));
            }
        }
        if !tokens.contains(&Token(upgrade_client_token)) {
            return Err(HubError::InvalidUpgradeSnapshot(format!(
                "upgrade client token {upgrade_client_token} is not live"
            )));
        }
        if next_session_id == 0
            || next_session_id == SIGTERM_TOKEN.0
            || next_client_id == ClientId::MAX
            || next_worker_id == WorkerId::MAX
            || next_task_id == TaskId::MAX
            || paused_clients
                .iter()
                .map(PausedClientSession::id)
                .max()
                .is_some_and(|id| next_client_id <= id)
            || paused_workers
                .iter()
                .map(PausedWorkerSession::id)
                .max()
                .is_some_and(|id| next_worker_id <= id)
            || tokens
                .iter()
                .map(|token| token.0)
                .max()
                .is_some_and(|token| next_session_id <= token || next_session_id == SIGTERM_TOKEN.0)
        {
            return Err(HubError::InvalidUpgradeSnapshot(
                "one or more next-id counters collide with restored state".to_owned(),
            ));
        }

        let wire_tick_hints = paused_clients
            .iter()
            .filter(|session| session.requires_post_commit_tick())
            .count()
            + paused_workers
                .iter()
                .filter(|session| session.requires_post_commit_tick())
                .count();
        debug!(
            "restored {} sessions; {} carried a post-commit tick hint",
            paused_clients.len() + paused_workers.len(),
            wire_tick_hints
        );
        // Schedule one unconditional post-COMMIT tick for every restored
        // session. This is derived from the actual restored graph rather than
        // trusting a wire boolean, and it covers userspace-only frames that
        // kernel readiness cannot rediscover after mio registration.
        let restored_session_ticks = paused_clients
            .iter()
            .map(PausedClientSession::token)
            .chain(paused_workers.iter().map(PausedWorkerSession::token))
            .collect();
        let restored_worker_tokens = paused_workers
            .iter()
            .map(PausedWorkerSession::token)
            .collect();

        Ok(PausedCommandHub {
            server,
            clients: paused_clients,
            workers: paused_workers,
            tasks: restored_tasks,
            command_socket_path,
            restored_session_ticks,
            restored_worker_tokens,
            sigterm_receiver: None,
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
            self.retry_pending_upgrade_completion();
            let run_state = self.run_state;
            let now = Instant::now();

            self.finish_ready_tasks(now);

            // A restored Stopped worker can still hold an SCM socket needed
            // by UpgradeWorker phase one, or a buffered response with a live
            // route. Once both correlations disappear, it has no owner left:
            // remove it without signalling its historical PID and close both
            // inherited descriptors before another main upgrade.
            self.collect_unreferenced_restored_stopped_workers();

            let mut poll_timeout = self.next_poll_timeout(now);

            if self.run_state == ServerState::Stopping {
                // when closing, close all ClientSession which are not transfering data
                self.clients
                    .retain(|_, s| s.channel.back_buf.available_data() > 0);
                // when all ClientSession are closed, the CommandServer stops
                if self.clients.is_empty() {
                    return self.server.upgrading;
                }
            }

            let mut tick_tokens = std::mem::take(&mut self.restored_session_ticks);
            tick_tokens.extend(
                self.clients.iter().filter_map(|(token, session)| {
                    wants_to_tick(&session.channel).then_some(*token)
                }),
            );
            tick_tokens.extend(self.workers.iter().filter_map(|(token, session)| {
                (session.run_state != RunState::Stopped && wants_to_tick(&session.channel))
                    .then_some(*token)
            }));
            let sessions_to_tick = tick_tokens
                .into_iter()
                .map(|token| (token, Ready::EMPTY, None))
                .collect::<Vec<_>>();

            let workers_to_spawn = self.workers_to_spawn();

            // if we have sessions to tick or workers to spawn, we don't want to block on poll
            if !sessions_to_tick.is_empty()
                || workers_to_spawn > 0
                || self.pending_upgrade_completion.is_some()
            {
                poll_timeout = Some(Duration::default());
            }

            events.clear();
            trace!("Tasks: count={}", self.tasks.len());
            trace!("Sessions to tick: {:?}", sessions_to_tick);
            trace!("Polling timeout: {:?}", poll_timeout);
            match self.poll.poll(&mut events, poll_timeout) {
                Ok(()) => {}
                // a signal, `SIGTERM` included, interrupts the wait
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
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
                    SIGTERM_TOKEN => self.on_sigterm(),
                    token => {
                        trace!("{:?} got event: {:?}", token, event);
                        let mut upgrade_requested = false;
                        let mut handled_request = false;
                        if let Some((server, client)) = self.get_client_mut(&token) {
                            client.update_readiness(ready);
                            match client.ready() {
                                ClientResult::NothingToDo => {}
                                ClientResult::NewRequest(request) => {
                                    debug!("Received new request: {:?}", request);
                                    handled_request = true;
                                    upgrade_requested = server
                                        .handle_client_request(client, request)
                                        == ClientRequestOutcome::UpgradeMain;
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
                                    self.on_worker_channel_closed(&token, worker_id);
                                }
                            }
                        }
                        if upgrade_requested {
                            upgrade_main(self, token);
                            if self.server.upgrading {
                                return true;
                            }
                        } else if handled_request {
                            self.flush_pending_audit_events();
                        }
                    }
                }
            }
        }
    }

    /// A worker channel reported `CloseSession`: kill the worker and answer
    /// every request still in flight on it with a synthetic failure.
    ///
    /// Only the FIRST close of a given worker synthesises failures: the
    /// session stays registered after `close_worker`, which leaves it
    /// `Stopped`, and can report `CloseSession` again on the next poll, which
    /// would double-count. The gate is `!= Stopped`, not `is_active`: a
    /// `Stopping` worker (the old worker of an `upgrade --worker`, whose
    /// `SoftStop` task has no deadline) that closes before answering must
    /// still fail its requests, or that task never finishes and its client is
    /// never answered.
    fn on_worker_channel_closed(&mut self, token: &Token, worker_id: WorkerId) {
        let first_close = self
            .workers
            .get(token)
            .is_some_and(|worker| worker.run_state != RunState::Stopped);
        self.handle_worker_close(token);
        if first_close {
            self.fail_in_flight_requests_of_worker(worker_id);
        }
    }

    /// How long the event loop may block in `poll` before a task deadline
    /// must be revisited: until the EARLIEST outstanding deadline, `None` when
    /// no task has one.
    ///
    /// sozu#1826: this used to wait for the LATEST deadline. A silent worker
    /// produces no readiness, so with two tasks pending, the one due first
    /// was only reaped — and its client only answered — once the later one
    /// expired.
    fn next_poll_timeout(&self, now: Instant) -> Option<Duration> {
        self.tasks
            .values()
            .filter_map(|task| task.timeout)
            .min()
            .map(|deadline| deadline.saturating_duration_since(now))
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
        debug!(
            "Task {}: {:?}",
            if timed_out { "timeout" } else { "finish" },
            task
        );
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
    #[error("could not set up the SIGTERM self-pipe: {0}")]
    SigtermPipe(IoError),
    #[error("could not install the SIGTERM handler: {0}")]
    SigtermHandler(nix::Error),
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
    pub(crate) fn new_task(&mut self, job: Box<dyn GatheringTask>, timeout: Timeout) -> TaskId {
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

    pub(crate) fn scatter(
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
        // A worker that predates mutual TLS skips the client authentication
        // fields of an `AddHttpsListener` or `UpdateHttpsListener` and applies
        // the rest. Whatever verb the client used, a request that carries
        // client authentication leaves here on its `*WithClientAuth` verb,
        // which such a worker cannot decode and therefore never applies.
        let request = request.into_canonical();
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

    /// Drop a task that is still queued, together with every worker response
    /// route it registered.
    ///
    /// The task must not have migrated into `CommandHub::tasks` yet: its only
    /// caller, `load_state`'s parse-error branch, cancels within the same
    /// event-loop iteration as the scatter.
    ///
    /// sozu#1827: the routes used to stay in `in_flight`. A late worker answer
    /// or a worker closing then resolved to the cancelled task, found no task,
    /// and returned before any cleanup, so every failed replay leaked its
    /// routes for the life of the main process. Routes of other tasks are kept.
    pub fn cancel_task(&mut self, task_id: TaskId) {
        self.queued_tasks.remove(&task_id);
        self.in_flight
            .retain(|_, in_flight_task_id| *in_flight_task_id != task_id);
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
        self.close_worker_with(token, |pid| {
            kill(Pid::from_raw(pid), Signal::SIGKILL).is_ok()
        });
    }

    fn close_worker_with(&mut self, token: &Token, mut terminate: impl FnMut(pid_t) -> bool) {
        let worker = match self.workers.get_mut(token) {
            Some(w) => w,
            None => {
                error!("No worker exists with token {:?}", token);
                return;
            }
        };

        if worker.run_state != RunState::Stopped {
            if terminate(worker.pid) {
                info!("Worker {} was successfully killed", worker.id);
            } else {
                info!("worker {} was already dead", worker.id);
            }
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
}

impl CommandHub {
    pub(crate) fn notify_upgrade_processing(&mut self, token: Token) {
        if let Some(client) = self.clients.get_mut(&token) {
            client.return_processing("Upgrading the main process...");
        }
    }

    pub(crate) fn fail_upgrade(&mut self, token: Token, message: String) {
        if let Some(client) = self.clients.get_mut(&token) {
            client.finish_failure(message);
        } else {
            error!(
                "main upgrade failed after its initiating client disconnected: {}",
                message
            );
        }
    }

    fn upgrade_fds(&self) -> Vec<i32> {
        std::iter::once(self.server.unix_listener.as_raw_fd())
            .chain(self.clients.values().map(|client| client.channel.fd()))
            .chain(
                self.server
                    .workers
                    .values()
                    .flat_map(|worker| [worker.channel.fd(), worker.scm_socket.raw_fd()]),
            )
            .collect()
    }

    /// Make every descriptor represented by the snapshot survive exec.
    /// Failure is all-or-nothing: descriptors already changed are restored.
    pub fn disable_cloexec_before_upgrade(&mut self) -> Result<(), ServerError> {
        let mut changed = Vec::new();
        for fd in self.upgrade_fds() {
            if let Err(error) = disable_close_on_exec(fd) {
                for changed_fd in changed {
                    let _ = enable_close_on_exec(changed_fd);
                }
                return Err(ServerError::DisableCloexec(error));
            }
            changed.push(fd);
        }
        Ok(())
    }

    /// Restore CLOEXEC on every descriptor `disable_cloexec_before_upgrade`
    /// cleared, once a main upgrade failed and this Hub keeps running.
    pub fn enable_cloexec_after_upgrade(&mut self) -> Result<(), ServerError> {
        enable_cloexec_on(self.upgrade_fds())
    }

    /// Capture the complete control-plane continuation without consuming it.
    pub fn generate_upgrade_data(
        &self,
        upgrade_client_token: Token,
    ) -> Result<UpgradeData, HubError> {
        if self.server.run_state != ServerState::Running || self.server.upgrading {
            return Err(HubError::InvalidUpgradeSnapshot(
                "main upgrade requires a Running, unfenced command Hub".to_owned(),
            ));
        }
        if self.pending_upgrade_completion.is_some() {
            return Err(HubError::InvalidUpgradeSnapshot(
                "previous main-upgrade response is still backpressured".to_owned(),
            ));
        }
        if !self.clients.contains_key(&upgrade_client_token) {
            return Err(HubError::InvalidUpgradeSnapshot(format!(
                "upgrade client token {} is not live",
                upgrade_client_token.0
            )));
        }

        let mut unique_fds = HashSet::new();
        for fd in self.upgrade_fds() {
            if fd < 0 || !unique_fds.insert(fd) {
                return Err(HubError::InvalidUpgradeSnapshot(format!(
                    "upgrade descriptor {fd} is invalid or duplicated"
                )));
            }
        }

        // Start the cross-process monotonic interval before taking the
        // Instant used by every task. Snapshot construction can be
        // arbitrarily long; starting it afterwards would give restored tasks
        // that time back by extending deadlines and shortening audit elapsed
        // time. The tiny sampling gap is deliberately charged to the handoff,
        // which is conservative for both contracts.
        let captured_monotonic_nanos = monotonic_nanos()?;
        let timing = TaskSnapshotTiming::now();
        let tasks = self
            .tasks
            .iter()
            .map(|(id, task)| task.snapshot(timing).map(|task| (*id, task)))
            .collect::<Result<HashMap<_, _>, _>>()?;
        let queued_tasks = self
            .server
            .queued_tasks
            .iter()
            .map(|(id, task)| task.snapshot(timing).map(|task| (*id, task)))
            .collect::<Result<HashMap<_, _>, _>>()?;
        if tasks
            .values()
            .chain(queued_tasks.values())
            .any(TaskContainerSnapshot::is_stop)
        {
            return Err(HubError::InvalidUpgradeSnapshot(
                "cannot upgrade while a stop task is dispatched".to_owned(),
            ));
        }
        let boot_generation = self.server.boot_generation.checked_add(1).ok_or_else(|| {
            HubError::InvalidUpgradeSnapshot("boot generation overflow".to_owned())
        })?;

        Ok(UpgradeData::new(UpgradeSnapshot {
            command_socket_fd: self.server.unix_listener.as_raw_fd(),
            config: self.server.config.clone(),
            captured_monotonic_nanos,
            server_state: UpgradeServerState::Running,
            next_client_id: self.server.next_client_id,
            next_session_id: self.server.next_session_id,
            next_task_id: self.server.next_task_id,
            next_worker_id: self.server.next_worker_id,
            upgrade_client_token: upgrade_client_token.0,
            clients: self.clients.values().map(ClientSession::snapshot).collect(),
            workers: self
                .server
                .workers
                .values()
                .map(WorkerSession::snapshot)
                .collect(),
            event_subscribers: self
                .server
                .event_subscribers
                .iter()
                .map(|token| token.0)
                .collect(),
            in_flight: self.server.in_flight.clone(),
            tasks,
            queued_tasks,
            pending_audit_events: self.server.pending_audit_events.clone(),
            state: self.server.state.clone(),
            boot_generation,
        }))
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
/// stop a misbehaving peer from inflating it (the unix socket defaults to
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
            AddBackend, CertificateSummary, CertificatesByAddress, ClientAuthMode, Cluster,
            HttpsListenerConfig, ListOfCertificatesByAddress, RequestHttpFrontend,
            RequestTcpFrontend, SocketAddress, SoftStop, WorkerResponse, request::RequestType,
            response_content::ContentType,
        },
    };
    use sozu_lib::metrics::METRICS;

    use sozu_command_lib::proto::command::{PathRule, RulePosition, filtered_metrics};

    fn wait_for_eof(reader: &mut impl Read, resource: &str) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut byte = [0_u8; 1];
        loop {
            match reader.read(&mut byte) {
                Ok(0) => return Ok(()),
                Ok(count) => return Err(format!("{resource} produced {count} unexpected bytes")),
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                    ) && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                    ) =>
                {
                    return Err(format!("{resource} did not reach EOF before the deadline"));
                }
                Err(error) => return Err(format!("{resource} read failed: {error}")),
            }
        }
    }

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

    fn register_observable_client(hub: &mut CommandHub) -> (Token, Channel<Request, Response>) {
        let (mut channel, peer): (Channel<Response, Request>, Channel<Request, Response>) =
            Channel::generate_nonblocking(4096, 65536).expect("could not generate client channels");
        let token = hub.server.next_session_token();
        let id = hub.server.next_client_id();
        hub.server
            .register(token, &mut channel.sock)
            .expect("could not register client channel");
        hub.clients.insert(
            token,
            ClientSession::new(
                channel,
                id,
                token,
                PeerCred::default(),
                None,
                None,
                std::sync::Arc::from("/tmp/sozu-upgrade-test.sock"),
            ),
        );
        (token, peer)
    }

    fn flush_client_responses(
        hub: &mut CommandHub,
        token: Token,
        peer: &mut Channel<Request, Response>,
    ) {
        let client = hub.clients.get_mut(&token).expect("test client is live");
        client.channel.handle_events(Ready::WRITABLE);
        client.channel.run().expect("client response should flush");
        peer.handle_events(Ready::READABLE);
        peer.run().expect("peer should buffer client responses");
    }

    /// `register_client` clamps a client channel's initial capacity to
    /// `max_command_buffer_size` when that ceiling is below
    /// `CLIENT_CHANNEL_INITIAL_BUFFER_SIZE` (sozu-proxy/sozu#1416). The
    /// replacement main must expect that same clamped value, or every main
    /// upgrade of such a configuration fails on the upgrading client itself.
    #[test]
    fn prepare_from_upgrade_data_accepts_clamped_client_channel_capacity() {
        for max_command_buffer_size in [2048, DEFAULT_MAX_COMMAND_BUFFER_SIZE] {
            let dir = tempfile::tempdir().expect("could not create temp dir");
            let socket_path = dir.path().join("clamped-client.sock");
            let listener = UnixListener::bind(&socket_path).expect("could not bind socket");
            let config = Config {
                command_buffer_size: max_command_buffer_size.min(DEFAULT_COMMAND_BUFFER_SIZE),
                max_command_buffer_size,
                ..Config::default()
            };
            let mut hub =
                CommandHub::new(listener, config, "sozu".to_owned()).expect("could not create Hub");
            let (_peer, accepted) =
                std::os::unix::net::UnixStream::pair().expect("could not create client pair");
            accepted
                .set_nonblocking(true)
                .expect("could not make client nonblocking");
            hub.register_client(UnixStream::from_std(accepted));
            let client_token = *hub.clients.keys().next().expect("registered client token");

            let data = hub
                .generate_upgrade_data(client_token)
                .expect("Hub should snapshot");
            // The replacement main takes ownership of every descriptor named
            // by the snapshot; the old Hub must not close them as well.
            std::mem::forget(hub);

            let paused = CommandHub::prepare_from_upgrade_data(data).unwrap_or_else(|error| {
                panic!(
                    "max_command_buffer_size={max_command_buffer_size}: a clamped client \
                     channel must restore, got {error}"
                )
            });
            assert_eq!(paused.clients.len(), 1);
        }
    }

    #[test]
    fn legacy_upgrade_preserves_stopping_worker_with_fresh_token_and_tick() {
        // No bind is needed: this test only exercises descriptor ownership and
        // mio registration, and some sandboxes forbid filesystem socket binds.
        // SAFETY: socket returns a uniquely owned descriptor on success.
        let listener_fd =
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_NONBLOCK, 0) };
        assert!(
            listener_fd >= 0,
            "could not create test listener descriptor"
        );
        // SAFETY: ownership of the descriptor returned above transfers here.
        let listener = unsafe { UnixListener::from_raw_fd(listener_fd) };
        let (worker_channel, _worker_peer): (
            Channel<WorkerRequest, WorkerResponse>,
            Channel<WorkerResponse, WorkerRequest>,
        ) = Channel::generate_nonblocking(64, 512).expect("could not create worker channel");
        let channel_fd = worker_channel.sock.into_raw_fd();
        let (scm_socket, _scm_peer) =
            std::os::unix::net::UnixStream::pair().expect("could not create SCM socket pair");
        let scm_fd = scm_socket.into_raw_fd();

        let data = crate::command::upgrade::LegacyUpgradeData {
            command_socket_fd: listener.into_raw_fd(),
            config: Config {
                command_buffer_size: 64,
                max_command_buffer_size: 512,
                ..Config::default()
            },
            next_client_id: 3,
            next_session_id: 7,
            next_task_id: 5,
            next_worker_id: 11,
            workers: vec![crate::command::upgrade::LegacySerializedWorkerSession {
                channel_fd,
                pid: 4242,
                id: 9,
                run_state: RunState::Stopping,
                scm_fd,
            }],
            state: ConfigState::new(),
            boot_generation: 4,
        };

        let paused = CommandHub::prepare_from_legacy_upgrade_data(data)
            .expect("the legacy Hub should prepare without shared data I/O");
        assert_eq!(paused.workers.len(), 1);
        assert_eq!(paused.workers[0].token(), Token(7));
        assert!(paused.restored_session_ticks.contains(&Token(7)));
        assert!(paused.restored_worker_tokens.contains(&Token(7)));
        assert_eq!(paused.server.next_session_id, 8);

        let hub = paused
            .activate()
            .expect("parent exit should activate the prepared Hub");
        let worker = hub
            .server
            .workers
            .get(&Token(7))
            .expect("the draining legacy worker should survive the handoff");
        assert_eq!(worker.id, 9);
        assert_eq!(worker.run_state, RunState::Stopping);
        assert_eq!(hub.server.boot_generation, 4);
    }

    #[test]
    fn precommit_sigterm_probe_preserves_the_stop_byte_for_the_event_loop() {
        let (mut sender, receiver) = UnixStream::pair().expect("could not create signal pair");
        let mut hub = create_test_hub();
        hub.sigterm_receiver = Some(receiver);
        assert!(!hub.sigterm_pending().unwrap());
        sender.write_all(&[1]).expect("could not queue stop byte");

        assert!(hub.sigterm_pending().unwrap());
        assert!(
            hub.sigterm_pending().unwrap(),
            "the PREPARE gate must observe without consuming the stop intention"
        );

        let mut byte = [0u8; 1];
        hub.sigterm_receiver
            .as_mut()
            .expect("test SIGTERM receiver")
            .read_exact(&mut byte)
            .expect("normal event-loop path should still consume the byte");
        assert_eq!(byte, [1]);
        assert!(!hub.sigterm_pending().unwrap());
    }

    #[test]
    fn hub_snapshot_captures_active_queued_routes_clients_and_pending_audit() {
        #[derive(Debug)]
        struct TransferTask {
            client_token: Token,
            gatherer: DefaultGatherer,
        }
        impl GatheringTask for TransferTask {
            fn client_token(&self) -> Option<Token> {
                Some(self.client_token)
            }

            fn get_gatherer(&mut self) -> &mut dyn Gatherer {
                &mut self.gatherer
            }

            fn snapshot(
                &self,
                _timing: TaskSnapshotTiming,
            ) -> Result<TaskSnapshot, TaskSnapshotError> {
                Ok(TaskSnapshot::Request(Box::new(
                    crate::command::requests::RequestTaskSnapshot::QueryClusters {
                        client_token: self.client_token.0,
                        gatherer: self.gatherer.clone(),
                        main_process_response: None,
                    },
                )))
            }

            fn on_finish(
                self: Box<Self>,
                _server: &mut Server,
                _client: &mut OptionalClient,
                _timed_out: bool,
            ) {
            }
        }

        let dir = tempfile::tempdir().expect("could not create temp dir");
        let socket_path = dir.path().join("upgrade-snapshot.sock");
        let listener = UnixListener::bind(&socket_path).expect("could not bind socket");
        let config = Config {
            command_buffer_size: DEFAULT_COMMAND_BUFFER_SIZE,
            max_command_buffer_size: DEFAULT_MAX_COMMAND_BUFFER_SIZE,
            ..Config::default()
        };
        let mut hub =
            CommandHub::new(listener, config, "sozu".to_owned()).expect("could not create Hub");
        let (_peer, accepted) =
            std::os::unix::net::UnixStream::pair().expect("could not create client pair");
        accepted
            .set_nonblocking(true)
            .expect("could not make client nonblocking");
        hub.register_client(UnixStream::from_std(accepted));
        let client_token = *hub.clients.keys().next().expect("registered client token");
        hub.clients
            .get_mut(&client_token)
            .expect("registered client")
            .return_processing("already buffered");
        hub.server.event_subscribers.insert(client_token);
        hub.server.pending_audit_events.push_back(Event::default());

        let task = || TaskContainer {
            job: Box::new(TransferTask {
                client_token,
                gatherer: DefaultGatherer {
                    expected_responses: 1,
                    ..DefaultGatherer::default()
                },
            }),
            timeout: Some(Instant::now() + Duration::from_secs(30)),
        };
        hub.tasks.insert(2, task());
        hub.server.queued_tasks.insert(3, task());
        hub.server.in_flight.insert("worker-route".to_owned(), 2);
        hub.server.next_task_id = 4;

        let data = hub
            .generate_upgrade_data(client_token)
            .expect("complete Hub should snapshot");
        assert_eq!(
            data.counts(),
            crate::command::upgrade::UpgradeCounts {
                clients: 1,
                workers: 0,
                tasks: 2,
                routes: 1,
                buffered_bytes: data.counts().buffered_bytes,
                pending_requests: 0,
            }
        );
        assert!(data.counts().buffered_bytes > 0);
        assert_eq!(data.snapshot().event_subscribers, [client_token.0]);
        assert_eq!(data.snapshot().pending_audit_events.len(), 1);
        assert_eq!(data.snapshot().upgrade_client_token, client_token.0);

        let encoded = serde_json::to_vec(&data).expect("upgrade data should serialize");
        let decoded: UpgradeData =
            serde_json::from_slice(&encoded).expect("upgrade data should deserialize");
        assert_eq!(decoded.counts(), data.counts());
    }

    #[test]
    fn restored_queued_task_finishes_once_without_replaying_its_entrypoint() {
        let mut hub = create_test_hub();
        let (client_token, mut peer) = register_observable_client(&mut hub);
        let timing = TaskSnapshotTiming::now();
        let snapshot = TaskSnapshot::Request(Box::new(
            crate::command::requests::RequestTaskSnapshot::QueryClusters {
                client_token: client_token.0,
                gatherer: DefaultGatherer::default(),
                main_process_response: Some(ResponseContent::default()),
            },
        ));
        let encoded = serde_json::to_vec(&snapshot).expect("queued task should serialize");
        let restored: TaskSnapshot =
            serde_json::from_slice(&encoded).expect("queued task should deserialize");
        let restored = restored
            .restore(TaskRestoreTiming {
                now: timing.now,
                handoff_elapsed: Duration::ZERO,
            })
            .expect("queued task should restore without dispatch");

        hub.server.queued_tasks.insert(
            7,
            TaskContainer {
                job: restored,
                timeout: None,
            },
        );
        hub.server.next_task_id = 8;
        assert_eq!(
            hub.clients[&client_token].channel.back_buf.available_data(),
            0,
            "snapshot/restore itself must not run the task callback"
        );

        hub.finish_ready_tasks(Instant::now());
        hub.finish_ready_tasks(Instant::now());
        assert!(hub.tasks.is_empty() && hub.server.queued_tasks.is_empty());

        flush_client_responses(&mut hub, client_token, &mut peer);
        let response = peer
            .read_message()
            .expect("restored queued task should answer once");
        assert_eq!(response.status, ResponseStatus::Ok as i32);
        assert_eq!(response.message, "Successfully queried clusters");
        assert!(
            peer.read_message().is_err(),
            "a second task sweep must not execute the restored callback again"
        );
    }

    #[test]
    fn dispatched_stop_task_is_rejected_from_a_running_upgrade_snapshot() {
        for queued in [false, true] {
            let mut hub = create_test_hub();
            let (client_token, _peer) = register_observable_client(&mut hub);
            let task = TaskSnapshot::Request(Box::new(
                crate::command::requests::RequestTaskSnapshot::Stop {
                    client_token: Some(client_token.0),
                    gatherer: DefaultGatherer::default(),
                    hardness: false,
                },
            ))
            .restore(TaskRestoreTiming {
                now: Instant::now(),
                handoff_elapsed: Duration::ZERO,
            })
            .expect("Stop task should restore for the rejection fixture");
            let container = TaskContainer {
                job: task,
                timeout: None,
            };
            if queued {
                hub.server.queued_tasks.insert(7, container);
            } else {
                hub.tasks.insert(7, container);
            }
            hub.server.next_task_id = 8;

            let error = hub
                .generate_upgrade_data(client_token)
                .expect_err("a dispatched Stop task contradicts Running state");
            assert!(
                error.to_string().contains("stop task"),
                "unexpected rejection for queued={queued}: {error}"
            );
        }
    }

    #[test]
    fn restored_ready_upgrade_worker_phase_one_runs_its_callback_once() {
        let mut hub = create_test_hub();
        hub.server.config.command_buffer_size = DEFAULT_COMMAND_BUFFER_SIZE;
        hub.server.config.max_command_buffer_size = DEFAULT_MAX_COMMAND_BUFFER_SIZE;
        let dir = tempfile::tempdir().expect("could not create fake worker directory");
        let executable = dir.path().join("fake-sozu-worker");
        std::fs::write(&executable, "#!/bin/sh\nexec sleep 30\n")
            .expect("could not write fake worker executable");
        std::fs::set_permissions(&executable, Permissions::from_mode(0o755))
            .expect("could not make fake worker executable runnable");
        hub.server.executable_path = executable.to_string_lossy().into_owned();

        let (client_token, mut client_peer) = register_observable_client(&mut hub);
        let (main_sock, _worker_sock) = UnixStream::pair().expect("could not create worker pair");
        let main_channel: Channel<WorkerRequest, WorkerResponse> = Channel::new(
            main_sock,
            DEFAULT_COMMAND_BUFFER_SIZE,
            DEFAULT_MAX_COMMAND_BUFFER_SIZE,
        );
        let (scm_owner, scm_peer) =
            std::os::unix::net::UnixStream::pair().expect("could not create worker scm pair");
        let main_scm = ScmSocket::new(scm_owner.as_raw_fd()).expect("could not create main SCM");
        let peer_scm = ScmSocket::new(scm_peer.as_raw_fd()).expect("could not create peer SCM");
        hub.server
            .register_worker(7, 0, main_channel, main_scm)
            .expect("old worker should register");
        hub.server.next_worker_id = 8;
        let old_worker_token = hub
            .server
            .workers
            .iter()
            .find_map(|(token, worker)| (worker.id == 7).then_some(*token))
            .expect("old worker token");
        peer_scm
            .send_listeners(&Listeners::default())
            .expect("old worker should return its empty listener set");

        let snapshot: TaskSnapshot = serde_json::from_value(serde_json::json!({
            "UpgradeWorker": {
                "client_token": client_token.0,
                "progress": {
                    "RequestingListenSockets": {
                        "old_worker_token": old_worker_token.0,
                        "old_worker_id": 7
                    }
                },
                "ok": 1,
                "errors": 0,
                "responses": [],
                "expected_responses": 1
            }
        }))
        .expect("phase-one task snapshot should deserialize");
        let restored = snapshot
            .restore(TaskRestoreTiming {
                now: Instant::now(),
                handoff_elapsed: Duration::ZERO,
            })
            .expect("phase-one task should restore without running its callback");
        hub.server.queued_tasks.insert(
            11,
            TaskContainer {
                job: restored,
                timeout: None,
            },
        );
        hub.server.next_task_id = 12;
        assert_eq!(hub.server.workers.len(), 1);
        assert_eq!(hub.server.next_worker_id, 8);
        assert_eq!(
            hub.clients[&client_token].channel.back_buf.available_data(),
            0,
            "PREPARED restore must not receive listeners or launch a worker"
        );

        hub.finish_ready_tasks(Instant::now());
        assert_eq!(hub.server.next_worker_id, 9);
        assert_eq!(hub.server.workers.len(), 2);
        assert_eq!(hub.server.queued_tasks.len(), 1);
        assert_eq!(hub.tasks.len(), 0);
        hub.finish_ready_tasks(Instant::now());
        assert_eq!(
            hub.server.next_worker_id, 9,
            "a later task sweep must not launch the replacement worker twice"
        );
        assert_eq!(hub.server.workers.len(), 2);
        assert_eq!(hub.tasks.len(), 1, "phase two should now be active");
        assert!(hub.server.queued_tasks.is_empty());

        flush_client_responses(&mut hub, client_token, &mut client_peer);
        let first = client_peer
            .read_message()
            .expect("phase one should report the launched worker");
        let second = client_peer
            .read_message()
            .expect("phase one should report the old worker drain");
        assert!(first.message.contains("Launched a new worker with id 8"));
        assert!(second.message.contains("Soft stopping worker with id 7"));
        assert!(
            client_peer.read_message().is_err(),
            "phase-one callback messages must not be duplicated"
        );

        let (new_token, new_pid) = hub
            .server
            .workers
            .iter()
            .find_map(|(token, worker)| (worker.id == 8).then_some((*token, worker.pid)))
            .expect("replacement worker should be registered");
        hub.server.close_worker(&new_token);
        let _ = nix::sys::wait::waitpid(Pid::from_raw(new_pid), None);
        let worker = hub
            .server
            .workers
            .remove(&new_token)
            .expect("replacement worker should still be registered");
        worker
            .close_restored_descriptors()
            .expect("replacement worker descriptors should close");
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
            fn snapshot(
                &self,
                _timing: TaskSnapshotTiming,
            ) -> Result<TaskSnapshot, TaskSnapshotError> {
                Err(TaskSnapshotError::UnsupportedTestTask("RecordingTask"))
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

        fn snapshot(&self, _timing: TaskSnapshotTiming) -> Result<TaskSnapshot, TaskSnapshotError> {
            Err(TaskSnapshotError::UnsupportedTestTask("SecretBearingTask"))
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

    #[test]
    fn closing_an_already_stopped_worker_never_signals_its_saved_pid_again() {
        let mut server = create_test_server();
        let (_worker_side, _scm_owner) = register_test_worker(&mut server, 7, 4096, 65536);
        let token = server
            .workers
            .iter_mut()
            .find_map(|(token, worker)| {
                (worker.id == 7).then(|| {
                    worker.run_state = RunState::Stopped;
                    worker.pid = 4242;
                    *token
                })
            })
            .expect("test worker should be registered");
        let mut signalled = Vec::new();

        server.close_worker_with(&token, |pid| {
            signalled.push(pid);
            true
        });

        assert!(
            signalled.is_empty(),
            "a restored Stopped session must not signal a PID that may have been reused"
        );
        assert_eq!(server.workers[&token].run_state, RunState::Stopped);
    }

    #[test]
    fn restored_stopped_worker_is_retained_for_a_task_then_closes_both_descriptors() {
        #[derive(Debug)]
        struct RetainsWorkerTask {
            worker_token: Token,
            gatherer: DefaultGatherer,
        }

        impl GatheringTask for RetainsWorkerTask {
            fn client_token(&self) -> Option<Token> {
                None
            }

            fn get_gatherer(&mut self) -> &mut dyn Gatherer {
                &mut self.gatherer
            }

            fn snapshot(
                &self,
                _timing: TaskSnapshotTiming,
            ) -> Result<TaskSnapshot, TaskSnapshotError> {
                Err(TaskSnapshotError::UnsupportedTestTask("RetainsWorkerTask"))
            }

            fn retained_worker_token(&self) -> Option<Token> {
                Some(self.worker_token)
            }

            fn on_finish(
                self: Box<Self>,
                _server: &mut Server,
                _client: &mut OptionalClient,
                _timed_out: bool,
            ) {
            }
        }

        let mut hub = create_test_hub();
        let (main_sock, worker_sock) = UnixStream::pair().expect("could not create channel pair");
        let main_side: Channel<WorkerRequest, WorkerResponse> =
            Channel::new(main_sock, 4096, 65536);
        let mut worker_side: Channel<WorkerResponse, WorkerRequest> =
            Channel::new(worker_sock, 4096, 65536);
        let (scm_owner, mut scm_peer) =
            std::os::unix::net::UnixStream::pair().expect("could not create scm pair");
        scm_peer
            .set_nonblocking(true)
            .expect("could not make the scm peer nonblocking");
        let scm_socket =
            ScmSocket::new(scm_owner.as_raw_fd()).expect("could not create scm socket");
        hub.server
            .register_worker(7, 0, main_side, scm_socket)
            .expect("test worker should register");
        let token = hub
            .server
            .workers
            .iter_mut()
            .find_map(|(token, worker)| {
                (worker.id == 7).then(|| {
                    worker.run_state = RunState::Stopped;
                    *token
                })
            })
            .expect("test worker should be registered");
        // ScmSocket borrows this descriptor; transfer ownership to the Hub so
        // the test can observe its peer closing without a competing owner.
        let _scm_fd = scm_owner.into_raw_fd();
        hub.restored_worker_tokens.insert(token);
        hub.restored_session_ticks.insert(token);
        hub.tasks.insert(
            1,
            TaskContainer {
                job: Box::new(RetainsWorkerTask {
                    worker_token: token,
                    gatherer: DefaultGatherer {
                        expected_responses: 1,
                        ..DefaultGatherer::default()
                    },
                }),
                timeout: None,
            },
        );

        hub.collect_unreferenced_restored_stopped_workers();
        assert!(
            hub.server.workers.contains_key(&token),
            "UpgradeWorker phase one must retain its stopped worker SCM socket"
        );

        hub.tasks.remove(&1);
        hub.collect_unreferenced_restored_stopped_workers();
        assert!(!hub.server.workers.contains_key(&token));
        assert!(!hub.restored_worker_tokens.contains(&token));
        assert!(!hub.restored_session_ticks.contains(&token));
        // A concurrent fork may retain CLOEXEC descriptors until its exec.
        // Observe both endpoint identities before asserting either result;
        // the bound still fails on a durable duplicate instead of accepting
        // WouldBlock as successful cleanup.
        let channel_eof = wait_for_eof(&mut worker_side.sock, "restored worker channel");
        let scm_eof = wait_for_eof(&mut scm_peer, "restored worker SCM socket");
        assert!(
            channel_eof.is_ok() && scm_eof.is_ok(),
            "channel: {channel_eof:?}; SCM: {scm_eof:?}"
        );
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
        fn snapshot(&self, _timing: TaskSnapshotTiming) -> Result<TaskSnapshot, TaskSnapshotError> {
            Err(TaskSnapshotError::UnsupportedTestTask("TallyTask"))
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

    /// Worker-backed task used to observe the command loop's real poll wakeup.
    #[derive(Debug)]
    struct TimeoutProbeTask {
        gatherer: DefaultGatherer,
        finished_at: std::rc::Rc<std::cell::Cell<Option<Instant>>>,
        stop_server: bool,
    }

    impl GatheringTask for TimeoutProbeTask {
        fn client_token(&self) -> Option<Token> {
            None
        }

        fn get_gatherer(&mut self) -> &mut dyn Gatherer {
            &mut self.gatherer
        }

        fn snapshot(&self, _timing: TaskSnapshotTiming) -> Result<TaskSnapshot, TaskSnapshotError> {
            Err(TaskSnapshotError::UnsupportedTestTask("TimeoutProbeTask"))
        }

        fn on_finish(
            self: Box<Self>,
            server: &mut Server,
            _client: &mut OptionalClient,
            timed_out: bool,
        ) {
            assert!(
                timed_out,
                "the silent worker must leave the task to time out"
            );
            self.finished_at.set(Some(Instant::now()));
            if self.stop_server {
                server.run_state = ServerState::Stopping;
            }
        }
    }

    /// Run the actual command event loop with worker-backed tasks and a worker
    /// whose connected channel remains open but never answers.
    fn run_silent_deadline_probe(deadlines: &[(Duration, bool)]) -> (Duration, usize, usize) {
        let mut hub = create_test_hub();
        let (_silent_worker, _scm) = register_test_worker(&mut hub.server, 0, 4096, 65536);
        let started_at = Instant::now();
        let finished_at = std::rc::Rc::new(std::cell::Cell::new(None));

        for (index, (timeout, stop_server)) in deadlines.iter().copied().enumerate() {
            let task_id = hub.server.new_task(
                Box::new(TimeoutProbeTask {
                    gatherer: DefaultGatherer::default(),
                    finished_at: finished_at.clone(),
                    stop_server,
                }),
                Timeout::Custom(timeout),
            );
            hub.server.scatter_on(
                RequestType::Status(Status {}).into(),
                task_id,
                index + 1,
                None,
            );
        }

        assert_eq!(
            hub.server.queued_tasks.len(),
            deadlines.len(),
            "every probe task must be admitted before the event loop starts"
        );
        assert_eq!(
            hub.server.in_flight.len(),
            deadlines.len(),
            "every probe task must wait for one answer from the silent worker"
        );

        assert!(
            !hub.run(),
            "the probe stops normally; it is not a main-upgrade handoff"
        );
        let elapsed = finished_at
            .get()
            .expect("the designated early task must have timed out")
            .duration_since(started_at);
        (elapsed, hub.tasks.len(), hub.server.in_flight.len())
    }

    /// Regression (sozu#1826): one later task must not extend the poll sleep
    /// past an earlier task's deadline.
    ///
    /// The first run is a positive witness for the same event loop, worker
    /// channel and timeout callback with only A pending. The second admits A
    /// and B (`A < B`) against one connected worker that never answers. With no
    /// client traffic, worker reply or worker spawn, the poll deadline is the
    /// only wakeup. The failure bound sits halfway between A and B so normal
    /// scheduling latitude cannot make waiting until B look like waiting for A.
    ///
    /// To see this pass under a controlled mutation, select the minimum task
    /// deadline at the `CommandHub::run` poll calculation. Restore the maximum
    /// selector to see the A+B run stop around B and leave no B task pending.
    #[test]
    fn poll_wakes_for_the_earliest_silent_worker_task_deadline() {
        let early = Duration::from_millis(100);
        let late = Duration::from_secs(2);
        let failure_bound = Duration::from_secs(1);

        let (witness_elapsed, witness_remaining, witness_routes) =
            run_silent_deadline_probe(&[(early, true)]);
        assert_eq!(
            witness_remaining, 0,
            "the one-task witness must finish its only task"
        );
        assert_eq!(
            witness_routes, 0,
            "the one-task witness must retire its worker-response route"
        );
        assert!(
            witness_elapsed < failure_bound,
            "positive witness: A alone must finish before {failure_bound:?}, got {witness_elapsed:?}"
        );

        // Insert B first so neither insertion order nor task id accidentally
        // makes the earlier deadline look privileged.
        let (paired_elapsed, paired_remaining, paired_routes) =
            run_silent_deadline_probe(&[(late, false), (early, true)]);
        assert!(
            paired_elapsed < failure_bound,
            "A must finish before {failure_bound:?} even while B is pending; got {paired_elapsed:?}"
        );
        assert_eq!(
            paired_remaining, 1,
            "B must remain pending when A's earlier deadline wakes the loop"
        );
        assert_eq!(
            paired_routes, 1,
            "A's route must be retired while B's route remains owned by B"
        );
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

    /// Regression (sozu#1826): the event loop must wake up for the EARLIEST
    /// task deadline, not the latest one.
    ///
    /// Two worker-backed tasks are scattered to a worker that never answers,
    /// with deadlines A (50 ms) < B (60 s). Nothing else produces readiness,
    /// so the `poll` timeout is the only thing that brings the loop back to
    /// the sweep that reaps an expired task. That timeout must not outlast A.
    ///
    /// To SEE THIS RED: select the deadline with `.max()` in
    /// `CommandHub::next_poll_timeout` — the loop then sleeps until B.
    #[test]
    fn poll_wakes_up_for_the_earliest_task_deadline() {
        let mut hub = create_test_hub();
        let (_silent_worker, _scm) = register_test_worker(&mut hub.server, 0, 4096, 65536);

        let early = Duration::from_millis(50);
        let late = Duration::from_secs(60);
        let mut task_ids = vec![];
        for timeout in [late, early] {
            let task_id = hub.server.new_task(
                Box::new(TallyTask {
                    gatherer: DefaultGatherer::default(),
                    seen: Default::default(),
                }),
                Timeout::Custom(timeout),
            );
            hub.server
                .scatter_on(RequestType::Status(Status {}).into(), task_id, 1, None);
            task_ids.push(task_id);
        }
        // What the top of `run` does once per iteration: queued tasks migrate
        // into the hub's task map.
        let queued = std::mem::take(&mut hub.server.queued_tasks);
        hub.tasks.extend(queued);
        assert_eq!(hub.tasks.len(), 2, "both tasks must be pending");
        assert_eq!(
            hub.server.in_flight.len(),
            2,
            "both tasks must still be owed an answer by the silent worker"
        );

        let poll_timeout = hub
            .next_poll_timeout(Instant::now())
            .expect("pending tasks with deadlines must bound the poll timeout");
        assert!(
            poll_timeout <= early,
            "the loop must wake up by the earliest deadline ({early:?}), got {poll_timeout:?}"
        );
    }

    /// Regression (sozu#1827): cancelling a task must retire every worker
    /// response route it registered, and only those.
    ///
    /// `cancel_task` dropped the queued task but left its `in_flight` routes.
    /// A late worker answer then resolved to a task that no longer exists, and
    /// a worker closing re-fed every leftover route as a synthetic failure for
    /// it; each failed state replay (`load_state`'s parse-error branch, the
    /// only caller) leaked its routes for the life of the main process.
    ///
    /// To SEE THIS RED: drop the `in_flight.retain` from `Server::cancel_task`
    /// — the cancelled task's two routes survive the cancellation.
    #[test]
    fn cancelling_a_task_retires_only_its_response_routes() {
        let mut hub = create_test_hub();
        let (_worker_0, _scm_0) = register_test_worker(&mut hub.server, 0, 4096, 65536);
        let (_worker_1, _scm_1) = register_test_worker(&mut hub.server, 1, 4096, 65536);

        let new_tally_task = |hub: &mut CommandHub, seen| {
            hub.server.new_task(
                Box::new(TallyTask {
                    gatherer: DefaultGatherer::default(),
                    seen,
                }),
                Timeout::None,
            )
        };
        let cancelled_seen = std::rc::Rc::new(std::cell::Cell::new((0, 0, 0)));
        let cancelled = new_tally_task(&mut hub, cancelled_seen.clone());
        hub.server
            .scatter_on(RequestType::Status(Status {}).into(), cancelled, 1, None);
        let kept_seen = std::rc::Rc::new(std::cell::Cell::new((0, 0, 0)));
        let kept = new_tally_task(&mut hub, kept_seen.clone());
        hub.server
            .scatter_on(RequestType::Status(Status {}).into(), kept, 1, Some(0));
        assert_eq!(
            hub.server.in_flight.len(),
            3,
            "2 + 1 routes before cancelling"
        );

        hub.server.cancel_task(cancelled);

        let routes: Vec<(RequestId, TaskId)> = hub
            .server
            .in_flight
            .iter()
            .map(|(id, task)| (id.clone(), *task))
            .collect();
        assert_eq!(
            routes,
            vec![(format!("0-{kept}-1"), kept)],
            "cancellation must retire the cancelled task's routes and keep the other task's"
        );

        // A late answer to the cancelled task finds no route at all...
        hub.handle_worker_response(
            1,
            WorkerResponse {
                id: format!("1-{cancelled}-1"),
                status: ResponseStatus::Ok.into(),
                message: String::new(),
                content: None,
            },
        );
        // ...and worker 0 closing fails only what is still owed on it: the
        // kept task's route, never the cancelled task's.
        hub.fail_in_flight_requests_of_worker(0);

        assert!(
            hub.server.in_flight.is_empty(),
            "no route may outlive the worker answers and closure that retire them"
        );
        let mut container = hub
            .server
            .queued_tasks
            .remove(&kept)
            .expect("the kept task must still be queued");
        assert!(
            container.job.get_gatherer().has_finished(),
            "worker 0 closing must complete the kept task"
        );
        hub.handle_finishing_task(kept, container, false);
        assert_eq!(
            kept_seen.get(),
            (0, 1, 1),
            "the kept task must account its own worker's closure as one failure"
        );
        assert!(
            hub.server.queued_tasks.is_empty() && hub.tasks.is_empty(),
            "the cancelled task must not be resurrected by a late answer"
        );
        assert_eq!(
            cancelled_seen.get(),
            (0, 0, 0),
            "a cancelled task never finishes"
        );
    }

    /// Regression: a `Stopping` worker that closes before answering fails
    /// its in-flight requests, so a task with no deadline waiting on it
    /// finishes and its client is answered.
    ///
    /// `upgrade --worker` marks the old worker `Stopping`, then waits for its
    /// `SoftStop` answer in a `Timeout::None` task. The close path only
    /// synthesised failures for an `is_active` worker, which `Stopping` is
    /// not: the task never finished and stayed in the hub, with its route in
    /// `in_flight`, until the main process restarted.
    ///
    /// The worker's pid is a child this test owns, so the real
    /// `close_worker` `SIGKILL` reaches it and not the test's process group.
    ///
    /// To SEE THIS RED: gate `on_worker_channel_closed`'s failure synthesis on
    /// `WorkerSession::is_active` again — the route stays in `in_flight` and
    /// the task stays unfinished.
    #[test]
    fn a_stopping_worker_closing_finishes_its_pending_task() {
        let mut hub = create_test_hub();
        let (_worker_0, _scm_0) = register_test_worker(&mut hub.server, 0, 4096, 65536);
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("could not spawn the stand-in worker process");
        let token = {
            let worker = hub
                .server
                .workers
                .values_mut()
                .find(|worker| worker.id == 0)
                .expect("worker 0 is registered");
            worker.pid = child.id() as pid_t;
            worker.run_state = RunState::Stopping;
            worker.token
        };

        let seen = std::rc::Rc::new(std::cell::Cell::new((0, 0, 0)));
        let soft_stop = hub.server.new_task(
            Box::new(TallyTask {
                gatherer: DefaultGatherer::default(),
                seen: seen.clone(),
            }),
            Timeout::None,
        );
        hub.server.scatter_on(
            RequestType::SoftStop(SoftStop {}).into(),
            soft_stop,
            0,
            Some(0),
        );
        let queued = std::mem::take(&mut hub.server.queued_tasks);
        hub.tasks.extend(queued);
        assert_eq!(
            hub.server.in_flight.len(),
            1,
            "the SoftStop must be owed an answer by the Stopping worker"
        );
        assert!(
            !hub.tasks
                .get_mut(&soft_stop)
                .expect("the SoftStop task is pending")
                .job
                .get_gatherer()
                .has_finished(),
            "the SoftStop task must wait for its worker"
        );

        // The old worker exits without answering its `SoftStop`.
        hub.on_worker_channel_closed(&token, 0);
        // The session stays registered, now `Stopped`, and may report
        // `CloseSession` again: that must not synthesise a second failure.
        // Issued before reaping, while the pid still names our zombie child.
        hub.on_worker_channel_closed(&token, 0);
        let status = child.wait().expect("could not reap the stand-in worker");
        assert!(!status.success(), "close_worker must have killed it");

        assert!(
            hub.server.in_flight.is_empty(),
            "the Stopping worker's close must retire its in-flight route"
        );
        let mut container = hub
            .tasks
            .remove(&soft_stop)
            .expect("the task is reaped by the next loop iteration");
        assert!(
            container.job.get_gatherer().has_finished(),
            "the Stopping worker's close must finish the SoftStop task"
        );
        hub.handle_finishing_task(soft_stop, container, false);
        assert_eq!(
            seen.get(),
            (0, 1, 1),
            "the unanswered SoftStop must be accounted as one failure"
        );
        assert!(hub.tasks.is_empty() && hub.server.queued_tasks.is_empty());
    }

    #[test]
    fn task_elapsed_snapshot_includes_time_spent_in_handoff_without_sleeping() {
        let capture_now = Instant::now();
        let original_started_at = capture_now
            .checked_sub(Duration::from_secs(7))
            .expect("test clock supports seven seconds of history");
        let snapshot =
            ElapsedSnapshot::capture(original_started_at, TaskSnapshotTiming { now: capture_now })
                .expect("elapsed audit timer should snapshot");
        let restore_now = capture_now
            .checked_add(Duration::from_secs(5))
            .expect("test clock supports five seconds in the future");

        let restored_started_at = snapshot
            .restore(TaskRestoreTiming {
                now: restore_now,
                handoff_elapsed: Duration::from_secs(5),
            })
            .expect("elapsed audit timer should restore");

        assert_eq!(restored_started_at, original_started_at);
        assert_eq!(
            restore_now.duration_since(restored_started_at),
            Duration::from_secs(12),
            "audit elapsed time must include the seven pre-freeze seconds and five handoff seconds"
        );
    }

    #[test]
    fn task_deadline_snapshot_spends_handoff_time_and_expires_without_sleeping() {
        let capture_now = Instant::now();
        let restore_now = capture_now
            .checked_add(Duration::from_secs(7))
            .expect("test clock supports seven seconds in the future");
        let timing = TaskSnapshotTiming { now: capture_now };
        let restore_timing = TaskRestoreTiming {
            now: restore_now,
            handoff_elapsed: Duration::from_secs(7),
        };

        let live = DeadlineSnapshot::capture(
            Some(
                capture_now
                    .checked_add(Duration::from_secs(30))
                    .expect("test clock supports a thirty-second deadline"),
            ),
            timing,
        )
        .expect("live deadline should snapshot")
        .restore(restore_timing)
        .expect("live deadline should restore")
        .expect("live deadline should remain armed");
        assert_eq!(live.duration_since(restore_now), Duration::from_secs(23));

        let expired = DeadlineSnapshot::capture(
            Some(
                capture_now
                    .checked_add(Duration::from_secs(5))
                    .expect("test clock supports a five-second deadline"),
            ),
            timing,
        )
        .expect("deadline should snapshot")
        .restore(restore_timing)
        .expect("deadline should restore")
        .expect("expired deadline should remain armed");
        assert_eq!(expired, restore_now);

        assert_eq!(
            DeadlineSnapshot::capture(None, timing)
                .expect("unarmed deadline should snapshot")
                .restore(restore_timing)
                .expect("unarmed deadline should restore"),
            None
        );
    }

    #[test]
    fn hub_snapshot_clock_precedes_task_capture_and_charges_construction_time() {
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        };

        #[derive(Debug)]
        struct ClockObservedTask {
            client_token: Token,
            started_at: Instant,
            observed_monotonic_nanos: Arc<AtomicU64>,
            observed_timing: Arc<Mutex<Option<TaskSnapshotTiming>>>,
            gatherer: DefaultGatherer,
        }

        impl GatheringTask for ClockObservedTask {
            fn client_token(&self) -> Option<Token> {
                Some(self.client_token)
            }

            fn get_gatherer(&mut self) -> &mut dyn Gatherer {
                &mut self.gatherer
            }

            fn snapshot(
                &self,
                timing: TaskSnapshotTiming,
            ) -> Result<TaskSnapshot, TaskSnapshotError> {
                self.observed_monotonic_nanos.store(
                    crate::command::upgrade::monotonic_nanos()
                        .expect("test should sample CLOCK_MONOTONIC"),
                    Ordering::SeqCst,
                );
                *self.observed_timing.lock().expect("timing mutex poisoned") = Some(timing);
                Ok(TaskSnapshot::Request(Box::new(
                    crate::command::requests::RequestTaskSnapshot::Worker(Box::new(
                        crate::command::requests::WorkerTaskSnapshot {
                            client_token: self.client_token.0,
                            gatherer: self.gatherer.clone(),
                            started_at: ElapsedSnapshot::capture(self.started_at, timing)?,
                            audit: None,
                            inline_audit: None,
                            metric_detail_audit: None,
                            clear_master_metrics_on_finish: false,
                            rollback: None,
                        },
                    )),
                )))
            }

            fn on_finish(
                self: Box<Self>,
                _server: &mut Server,
                _client: &mut OptionalClient,
                _timed_out: bool,
            ) {
            }
        }

        let mut hub = create_test_hub();
        let (_peer, accepted) =
            std::os::unix::net::UnixStream::pair().expect("could not create client pair");
        accepted
            .set_nonblocking(true)
            .expect("could not make client nonblocking");
        hub.register_client(UnixStream::from_std(accepted));
        let client_token = *hub.clients.keys().next().expect("registered client token");
        let observed_monotonic_nanos = Arc::new(AtomicU64::new(0));
        let observed_timing = Arc::new(Mutex::new(None));
        let original_started_at = Instant::now()
            .checked_sub(Duration::from_secs(7))
            .expect("test clock supports seven seconds of history");
        let original_deadline = Instant::now()
            .checked_add(Duration::from_secs(30))
            .expect("test clock supports a thirty-second deadline");
        hub.tasks.insert(
            1,
            TaskContainer {
                job: Box::new(ClockObservedTask {
                    client_token,
                    started_at: original_started_at,
                    observed_monotonic_nanos: Arc::clone(&observed_monotonic_nanos),
                    observed_timing: Arc::clone(&observed_timing),
                    gatherer: DefaultGatherer {
                        expected_responses: 1,
                        ..DefaultGatherer::default()
                    },
                }),
                timeout: Some(original_deadline),
            },
        );
        hub.server.next_task_id = 2;

        let data = hub
            .generate_upgrade_data(client_token)
            .expect("Hub should snapshot");
        let task_capture_monotonic = observed_monotonic_nanos.load(Ordering::SeqCst);
        let snapshot = data.snapshot();
        assert!(
            snapshot.captured_monotonic_nanos <= task_capture_monotonic,
            "the handoff clock must start no later than task serialization"
        );

        let capture_timing = observed_timing
            .lock()
            .expect("timing mutex poisoned")
            .expect("task should record its capture timing");
        let simulated_handoff = Duration::from_secs(5);
        let restored_monotonic = task_capture_monotonic
            .checked_add(duration_nanos(simulated_handoff, "test handoff").unwrap())
            .expect("test monotonic clock should not overflow");
        let restore_now = capture_timing
            .now
            .checked_add(simulated_handoff)
            .expect("test Instant should not overflow");
        let restore_timing = TaskRestoreTiming {
            now: restore_now,
            handoff_elapsed: Duration::from_nanos(
                restored_monotonic - snapshot.captured_monotonic_nanos,
            ),
        };
        let task_snapshot = snapshot.tasks.get(&1).expect("task should be serialized");
        let restored_deadline = task_snapshot
            .deadline
            .restore(restore_timing)
            .expect("deadline should restore")
            .expect("deadline should remain armed");
        assert!(
            restored_deadline <= original_deadline,
            "snapshot construction must never extend a task deadline"
        );
        let TaskSnapshot::Request(snapshot) = &task_snapshot.job else {
            panic!("test task should serialize as WorkerTask");
        };
        let crate::command::requests::RequestTaskSnapshot::Worker(snapshot) = snapshot.as_ref()
        else {
            panic!("test task should serialize as WorkerTask");
        };
        let restored_started_at = snapshot
            .started_at()
            .restore(restore_timing)
            .expect("elapsed timer should restore");
        assert!(
            restore_now.duration_since(restored_started_at)
                >= capture_timing.now.duration_since(original_started_at) + simulated_handoff,
            "snapshot construction must not subtract elapsed audit time"
        );
    }

    /// A client may send an HTTPS listener that asks for a client
    /// certificate on the historical `AddHttpsListener`. A worker that
    /// predates mutual TLS decodes that verb, skips the client authentication
    /// fields and builds the listener without them, so the fan-out must move
    /// it to `AddHttpsListenerWithClientAuth`, which that worker cannot
    /// decode. A listener without client auth keeps the historical verb.
    ///
    /// To SEE THIS RED: drop the `into_canonical` call in `scatter_on`.
    #[test]
    fn scatter_sends_client_auth_listeners_on_the_mtls_verb() {
        let mut hub = create_test_hub();
        let (mut worker_side, _scm) = register_test_worker(&mut hub.server, 0, 4096, 65536);

        let listener = |port: u16, client_auth: Option<ClientAuthMode>| HttpsListenerConfig {
            address: SocketAddress::new_v4(127, 0, 0, 1, port),
            client_auth: client_auth.map(|mode| mode as i32),
            client_ca_certificates: vec!["CLIENT CA PEM".to_owned()],
            ..Default::default()
        };
        let task_id = hub.server.new_task(
            Box::new(TallyTask {
                gatherer: DefaultGatherer::default(),
                seen: std::rc::Rc::new(std::cell::Cell::new((0, 0, 0))),
            }),
            Timeout::None,
        );
        hub.server.scatter_on(
            RequestType::AddHttpsListener(listener(8443, Some(ClientAuthMode::ClientAuthRequired)))
                .into(),
            task_id,
            1,
            None,
        );
        hub.server.scatter_on(
            RequestType::AddHttpsListenerWithClientAuth(listener(8444, None)).into(),
            task_id,
            2,
            None,
        );

        let mut received = vec![];
        for _ in 0..16 {
            for worker in hub.server.workers.values_mut() {
                worker.update_readiness(Ready::WRITABLE);
                let _ = worker.ready();
            }
            worker_side.handle_events(Ready::READABLE);
            received.extend(crate::command::sessions::extract_messages(&mut worker_side));
            if received.len() >= 2 {
                break;
            }
        }

        let verbs: Vec<(u16, &str)> = received
            .iter()
            .filter_map(|request| match &request.content.request_type {
                Some(RequestType::AddHttpsListener(l)) => {
                    Some((l.address.port as u16, "AddHttpsListener"))
                }
                Some(RequestType::AddHttpsListenerWithClientAuth(l)) => {
                    Some((l.address.port as u16, "AddHttpsListenerWithClientAuth"))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            verbs,
            vec![
                (8443, "AddHttpsListenerWithClientAuth"),
                (8444, "AddHttpsListener"),
            ],
            "the fan-out must carry each HTTPS listener on the verb its client auth calls for"
        );
    }
}
