//! Per-client command-socket session state.
//!
//! Tracks every connected CLI/`sozu` command-socket client (pid, comm,
//! authenticated peer credentials) and the in-flight requests waiting on
//! worker responses. Owns the PID-reuse-guarded `peer_comm` snapshot used
//! by the audit envelope so reused PIDs cannot impersonate another
//! command source. Long-form lifecycle: `bin/src/command/LIFECYCLE.md`.

use std::{fmt::Debug, sync::Arc, time::SystemTime};

use libc::pid_t;
use mio::Token;
use prost::Message;
use rusty_ulid::Ulid;
use sozu_command_lib::{
    channel::Channel,
    proto::command::{
        Request, Response, ResponseContent, ResponseStatus, RunState, WorkerInfo, WorkerRequest,
        WorkerResponse,
    },
    ready::Ready,
    scm_socket::ScmSocket,
};

use crate::command::server::{ClientId, MessageClient, PeerCred, WorkerId};

/// Track a client from start to finish
#[derive(Debug)]
pub struct ClientSession {
    pub channel: Channel<Response, Request>,
    pub id: ClientId,
    /// Per-connection ULID generated at accept time. Unlike `id` (a monotonic
    /// accept counter), this survives as a grep-correlation key across every
    /// audit log line a sozu CLI invocation produces.
    pub session_ulid: Ulid,
    pub token: Token,
    /// UID of the peer process on the unix socket, captured via `SO_PEERCRED`
    /// at accept time. `None` if the peer credentials could not be read
    /// (e.g. non-Linux build or the syscall failed).
    pub actor_uid: Option<u32>,
    /// GID of the peer process (same `SO_PEERCRED` read). `None` on error /
    /// unsupported platforms.
    pub actor_gid: Option<u32>,
    /// PID of the peer process (same `SO_PEERCRED` read). Rendered in the
    /// audit line so operators can correlate with `journalctl _PID=<pid>`
    /// and `/proc/<pid>`. Note PIDs can be reused — combine with the
    /// per-session ULID for stronger correlation.
    pub actor_pid: Option<i32>,
    /// `/proc/<pid>/comm` at accept time (up to 15 chars per kernel spec).
    /// Useful for distinguishing the `sozu` command-socket client from ad-hoc shells that share a
    /// UID. Cached at accept — never re-read.
    pub actor_comm: Option<String>,
    /// `getpwuid_r(actor_uid)` at accept time. Renders as the POSIX account
    /// name (e.g. `florentin`) in the audit line — more readable than a
    /// bare UID for SOC review. `None` when `actor_uid` is missing or NSS
    /// lookup fails.
    pub actor_user: Option<String>,
    /// Path of the command socket this client connected through, shared as
    /// an `Arc<str>` across every session accepted on the same listener.
    /// Lets multi-instance sozu deployments disambiguate audit lines that
    /// share a SIEM sink.
    pub socket_path: Arc<str>,
    /// Wall-clock time of `accept(2)` for this client connection. Rendered
    /// as RFC 3339 UTC in the audit line so SOC tooling can window
    /// per-session activity (e.g. "all verbs from connections that opened
    /// in the 30s before the incident"). Stored as `SystemTime` so it
    /// survives across the formatting boundary.
    pub connect_ts: SystemTime,
}

/// The return type of the ready method
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum ClientResult {
    NothingToDo,
    NewRequest(Request),
    CloseSession,
}

impl ClientSession {
    pub fn new(
        mut channel: Channel<Response, Request>,
        id: ClientId,
        token: Token,
        peer_cred: PeerCred,
        actor_comm: Option<String>,
        actor_user: Option<String>,
        socket_path: Arc<str>,
    ) -> Self {
        channel.interest = Ready::READABLE | Ready::ERROR | Ready::HUP;
        Self {
            channel,
            id,
            session_ulid: Ulid::generate(),
            token,
            actor_uid: peer_cred.uid,
            actor_gid: peer_cred.gid,
            actor_pid: peer_cred.pid,
            actor_comm,
            actor_user,
            socket_path,
            connect_ts: SystemTime::now(),
        }
    }

    /// Render the captured peer UID for audit logs. Returns the literal
    /// `"unknown"` when the value is missing so log lines stay structured.
    pub fn actor_uid_display(&self) -> String {
        display_or_unknown(self.actor_uid)
    }

    /// Render the captured peer GID. `"unknown"` when absent.
    pub fn actor_gid_display(&self) -> String {
        display_or_unknown(self.actor_gid)
    }

    /// Render the captured peer PID. `"unknown"` when absent.
    pub fn actor_pid_display(&self) -> String {
        display_or_unknown(self.actor_pid)
    }

    /// Render the connection-accept timestamp as an RFC 3339 UTC string,
    /// computed via the std-only [`crate::command::requests::rfc3339_utc`]
    /// helper. Caller is `audit_log_context!`.
    pub fn connect_ts_display(&self) -> String {
        crate::command::requests::rfc3339_utc(self.connect_ts)
    }

    /// Render the captured `/proc/<pid>/comm` string, sanitized for audit
    /// output (control chars stripped — `comm` is kernel-truncated but
    /// cannot contain any tab/newline already). `"unknown"` when absent.
    pub fn actor_comm_display(&self) -> String {
        display_sanitized_or_unknown(self.actor_comm.as_deref())
    }

    /// Render the resolved POSIX account name (`getpwuid_r(uid)`),
    /// sanitized for audit output. `"unknown"` when absent.
    pub fn actor_user_display(&self) -> String {
        display_sanitized_or_unknown(self.actor_user.as_deref())
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

/// Replace ASCII control characters (`\x00..=\x1f`, `\x7f`) in `s` with `?`
/// so they cannot forge an additional audit line via `\n` / `\t` / ANSI
/// escape sequences. Cheap: single-pass, only allocates when a replacement
/// is needed.
///
/// Load-bearing: the audit log's tab-delimited layout is forgeable if any
/// audit field contains a literal `\t` or `\n`. Applied at render time by
/// the `audit_log_context!` macro.
pub fn sanitize_for_audit(s: &str) -> String {
    if s.bytes().all(|b| b >= 0x20 && b != 0x7f) {
        return s.to_owned();
    }
    s.chars()
        .map(|c| {
            if (c as u32) < 0x20 || c == '\x7f' {
                '?'
            } else {
                c
            }
        })
        .collect()
}

/// QW8 helper: render `Option<T>` for audit output. `Some(v)` becomes
/// `v.to_string()`, `None` becomes the literal `"unknown"`. Used by the
/// `actor_*_display` accessors on `ClientSession` so the five near-
/// identical 4-line methods collapse to one-line wrappers around a
/// single rendering policy.
pub fn display_or_unknown<T: ToString>(value: Option<T>) -> String {
    match value {
        Some(v) => v.to_string(),
        None => String::from("unknown"),
    }
}

/// QW8 companion: render `Option<&str>` through `sanitize_for_audit` so
/// `actor_user_display` / `actor_comm_display` cannot regress against
/// the audit-line forgery defence. `None` → `"unknown"`.
pub fn display_sanitized_or_unknown(value: Option<&str>) -> String {
    match value {
        Some(s) => sanitize_for_audit(s),
        None => String::from("unknown"),
    }
}

impl MessageClient for ClientSession {
    fn finish_ok<T: Into<String>>(&mut self, message: T) {
        let message = message.into();
        debug!("{}", message);
        self.send(Response {
            status: ResponseStatus::Ok.into(),
            message,
            content: None,
        })
    }

    fn finish_ok_with_content<T: Into<String>>(&mut self, content: ResponseContent, message: T) {
        let message = message.into();
        debug!("{}", message);
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
        debug!("{}", message);
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
        debug!("{}", message);
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
            None => debug!("{}", message.into()),
            Some(client) => client.finish_ok(message),
        }
    }

    fn finish_ok_with_content<T: Into<String>>(&mut self, content: ResponseContent, message: T) {
        match self {
            None => debug!("{}", message.into()),
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
            None => debug!("{}", message.into()),
            Some(client) => client.return_processing(message),
        }
    }

    fn return_processing_with_content<S: Into<String>>(
        &mut self,
        message: S,
        content: ResponseContent,
    ) {
        match self {
            None => debug!("{}", message.into()),
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
    Tx: Debug + Default + Message,
    Rx: Debug + Default + Message,
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
