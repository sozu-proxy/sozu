//! Per-client command-socket session state.
//!
//! Tracks every connected CLI/`sozu` command-socket client (pid, comm,
//! authenticated peer credentials) and the in-flight requests waiting on
//! worker responses. Owns the PID-reuse-guarded `peer_comm` snapshot used
//! by the audit envelope so reused PIDs cannot impersonate another
//! command source. Long-form lifecycle: `bin/src/command/LIFECYCLE.md`.

use std::{
    collections::VecDeque,
    fmt::Debug,
    io,
    os::fd::{AsRawFd, IntoRawFd, OwnedFd, RawFd},
    sync::Arc,
    time::SystemTime,
};

use libc::pid_t;
use mio::{Interest, Registry, Token, net::UnixStream as MioUnixStream};
use prost::Message;
use rusty_ulid::Ulid;
use serde::{Deserialize, Serialize};
use sozu_command_lib::{
    channel::{Channel, ChannelError, ChannelSnapshot, ChannelSnapshotError, PausedChannel},
    proto::command::{
        Request, Response, ResponseContent, ResponseStatus, RunState, WorkerInfo, WorkerRequest,
        WorkerResponse,
    },
    ready::Ready,
    scm_socket::{ScmSocket, ScmSocketError},
};

use crate::command::server::{ClientId, MessageClient, PeerCred, WorkerId};

const SESSION_SNAPSHOT_VERSION: u16 = 1;

#[derive(thiserror::Error, Debug)]
pub enum SessionSnapshotError {
    #[error(
        "unsupported {session} session snapshot version {version}; supported version is {supported}"
    )]
    UnsupportedVersion {
        session: &'static str,
        version: u16,
        supported: u16,
    },
    #[error(transparent)]
    Channel(#[from] ChannelSnapshotError),
    #[error("could not validate received SCM descriptor {fd}: {error}")]
    ValidateScmDescriptor { fd: RawFd, error: String },
    #[error("could not activate received SCM descriptor after commit: {0}")]
    ActivateScmDescriptor(#[source] ScmSocketError),
}

/// Serializable state needed to continue one command client after main upgrade.
///
/// Fields stay private so the only construction path captures a coherent live
/// session. The source descriptor is manifest metadata; the Hub supplies the
/// effective descriptor after fork/exec inheritance or explicit duplication.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClientSessionSnapshot {
    version: u16,
    channel_fd: RawFd,
    channel: ChannelSnapshot,
    id: ClientId,
    session_ulid: Ulid,
    token: usize,
    actor_uid: Option<u32>,
    actor_gid: Option<u32>,
    actor_pid: Option<i32>,
    actor_comm: Option<String>,
    actor_user: Option<String>,
    socket_path: String,
    connect_ts: SystemTime,
    requires_post_commit_tick: bool,
}

impl ClientSessionSnapshot {
    /// Descriptor number in the sending process, for the transfer manifest.
    pub fn channel_fd(&self) -> RawFd {
        self.channel_fd
    }

    pub fn buffered_bytes(&self) -> usize {
        self.channel.buffered_bytes()
    }
}

/// Restored client state that is safe to register during PREPARED.
///
/// The active [`ClientSession`] remains inaccessible until `resume`, so this
/// wrapper cannot read, write, or consume its userspace buffers before COMMIT.
pub struct PausedClientSession {
    channel: PausedChannel<Response, Request>,
    id: ClientId,
    session_ulid: Ulid,
    token: Token,
    actor_uid: Option<u32>,
    actor_gid: Option<u32>,
    actor_pid: Option<i32>,
    actor_comm: Option<String>,
    actor_user: Option<String>,
    socket_path: Arc<str>,
    connect_ts: SystemTime,
    requires_post_commit_tick: bool,
}

impl PausedClientSession {
    pub fn id(&self) -> ClientId {
        self.id
    }

    pub fn token(&self) -> Token {
        self.token
    }

    /// Whether the Hub must schedule this session once immediately after COMMIT.
    pub fn requires_post_commit_tick(&self) -> bool {
        self.requires_post_commit_tick
    }

    /// Validate mio registration during PREPARED without performing I/O.
    pub fn register(&mut self, registry: &Registry) -> io::Result<()> {
        self.channel.register(
            registry,
            self.token,
            Interest::READABLE | Interest::WRITABLE,
        )
    }

    /// Expose the active session only after the Hub commits the handoff.
    pub fn resume(self) -> ClientSession {
        ClientSession {
            channel: self.channel.resume(),
            id: self.id,
            session_ulid: self.session_ulid,
            token: self.token,
            actor_uid: self.actor_uid,
            actor_gid: self.actor_gid,
            actor_pid: self.actor_pid,
            actor_comm: self.actor_comm,
            actor_user: self.actor_user,
            socket_path: self.socket_path,
            connect_ts: self.connect_ts,
        }
    }
}

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UpgradeResponseQueue {
    Queued,
    Backpressured,
    Fatal,
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

    /// Capture this session without changing descriptor flags or performing I/O.
    pub fn snapshot(&self) -> ClientSessionSnapshot {
        ClientSessionSnapshot {
            version: SESSION_SNAPSHOT_VERSION,
            channel_fd: self.channel.fd(),
            channel: self.channel.snapshot(),
            id: self.id,
            session_ulid: self.session_ulid,
            token: self.token.0,
            actor_uid: self.actor_uid,
            actor_gid: self.actor_gid,
            actor_pid: self.actor_pid,
            actor_comm: self.actor_comm.clone(),
            actor_user: self.actor_user.clone(),
            socket_path: self.socket_path.to_string(),
            connect_ts: self.connect_ts,
            requires_post_commit_tick: self.channel.front_buf.available_data() > 0
                || self.channel.back_buf.available_data() > 0
                || wants_to_tick(&self.channel),
        }
    }

    /// Queue the one terminal `UpgradeMain` response without losing it when
    /// the inherited client back buffer is temporarily full. Only the new
    /// main calls this, after COMMIT.
    pub(crate) fn try_queue_upgrade_response(
        &mut self,
        response: &Response,
    ) -> UpgradeResponseQueue {
        match self.channel.write_message(response) {
            Ok(()) => {
                self.channel.interest.insert(Ready::WRITABLE);
                UpgradeResponseQueue::Queued
            }
            Err(error) if is_transient_overflow(&error) => {
                self.channel.interest.insert(Ready::WRITABLE);
                UpgradeResponseQueue::Backpressured
            }
            Err(error) => {
                error!("could not queue terminal main-upgrade response: {}", error);
                self.channel.readiness = Ready::ERROR;
                UpgradeResponseQueue::Fatal
            }
        }
    }

    /// Rebuild a client around the descriptor received by the new main.
    ///
    /// The descriptor remains paused after its flags and trusted receiver
    /// limits are validated by `Channel::restore_paused`.
    pub fn restore_paused(
        sock: MioUnixStream,
        snapshot: ClientSessionSnapshot,
        expected_initial_buffer_size: usize,
        expected_max_buffer_size: usize,
    ) -> Result<PausedClientSession, SessionSnapshotError> {
        if snapshot.version != SESSION_SNAPSHOT_VERSION {
            return Err(SessionSnapshotError::UnsupportedVersion {
                session: "client",
                version: snapshot.version,
                supported: SESSION_SNAPSHOT_VERSION,
            });
        }

        let channel = Channel::restore_paused(
            sock,
            snapshot.channel,
            expected_initial_buffer_size,
            expected_max_buffer_size,
        )?;

        Ok(PausedClientSession {
            channel,
            id: snapshot.id,
            session_ulid: snapshot.session_ulid,
            token: Token(snapshot.token),
            actor_uid: snapshot.actor_uid,
            actor_gid: snapshot.actor_gid,
            actor_pid: snapshot.actor_pid,
            actor_comm: snapshot.actor_comm,
            actor_user: snapshot.actor_user,
            socket_path: Arc::from(snapshot.socket_path),
            connect_ts: snapshot.connect_ts,
            requires_post_commit_tick: snapshot.requires_post_commit_tick,
        })
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
    /// computed via the std-only `crate::command::requests::rfc3339_utc`
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
        // POST-CONDITION: a successfully queued response arms WRITABLE so the
        // event loop flushes it to the client; without this interest the
        // response would sit unsent in the back buffer.
        debug_assert!(
            self.channel.interest.is_writable(),
            "send must arm WRITABLE interest so the queued response gets flushed"
        );
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
    if s.chars().all(|c| !is_unsafe_line(c)) {
        return s.to_owned();
    }
    s.chars()
        .map(|c| if is_unsafe_line(c) { '?' } else { c })
        .collect()
}

/// Strict sanitiser for audit-log fields whose values participate in
/// column-boundary parsing, i.e. anything rendered as `, key={value}` in
/// the text sink. On top of [`sanitize_for_audit`]'s control-byte strip,
/// this also replaces `,` and `=` with `?` so an attacker-controlled value
/// cannot forge a fake adjacent KV pair when a SIEM splits on `, ` /  `=`.
///
/// Use this for any audit field whose source is operator-controlled
/// (request-derived strings) rather than master-controlled metadata.
/// Does NOT strip `:` because legitimate values (e.g. `target=address:...`)
/// use `:` as an in-value separator.
pub fn sanitize_for_audit_kv(s: &str) -> String {
    if s.chars().all(|c| !is_unsafe_kv(c)) {
        return s.to_owned();
    }
    s.chars()
        .map(|c| if is_unsafe_kv(c) { '?' } else { c })
        .collect()
}

/// Characters that would break the audit log's row-or-line shape if they
/// reached the text sink unsanitised. Covers the full Unicode control
/// category (`char::is_control()` matches C0, DEL, and C1 — NEL/CSI are in
/// C1 and would otherwise survive a byte-only `< 0x20 || == 0x7f` check),
/// three non-control codepoints that some SIEM normalisers treat as line
/// breaks (U+FEFF BOM, U+2028 LINE SEPARATOR, U+2029 PARAGRAPH SEPARATOR),
/// and the bidirectional override / isolate controls U+202A..=U+202E +
/// U+2066..=U+2069. The bidi class is Trojan-Source-flavoured (CVE-2021-
/// 42574): a Right-to-Left Override inside an audit value visually
/// reverses the bytes that follow when an operator tails the log in a
/// Unicode-aware terminal (`less`, `cat` under a UTF-8 locale,
/// `journalctl`), so the row appears to attribute the action to a
/// different field than it actually carries. The byte-based fast path
/// is gone on purpose: every problematic codepoint above U+007F is
/// multi-byte UTF-8 with every byte `>= 0x80`, so a byte-only `>= 0x20`
/// check would let them through.
#[inline]
fn is_unsafe_line(c: char) -> bool {
    c.is_control()
        || c == '\u{feff}'
        || c == '\u{2028}'
        || c == '\u{2029}'
        || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Strict variant: line-unsafe characters plus the column separators (`,`
/// and `=`) that a SIEM consumer splits on. Does NOT strip `:` — see
/// [`sanitize_for_audit_kv`] for the legitimate-value rationale.
#[inline]
fn is_unsafe_kv(c: char) -> bool {
    is_unsafe_line(c) || c == ',' || c == '='
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
    /// sozu#1313: requests accepted for delivery that did not fit in the
    /// channel back buffer, in scatter order. A BULK sender (`load_state`,
    /// `load_static_config`) queues every entry inside ONE event-loop
    /// iteration, so the buffer reaches `max_buffer_size` long before mio ever
    /// reports WRITABLE; the overflow waits here and is drained from the
    /// WRITABLE path by [`WorkerSession::flush_pending`]. Nothing blocks, and
    /// no entry is dropped. Bounded by the size of the bulk send in flight —
    /// for a replay, the state file the master already holds in its own
    /// `ConfigState`.
    ///
    /// ACCEPTED RESIDUAL: a queue that outlives its task's deadline is still
    /// delivered, so a worker can apply an entry the master already reverted.
    /// That is the same bounded divergence `should_rollback_fanout` documents
    /// for a late `Ok`, and it self-heals on the worker's next state replay.
    pending: VecDeque<WorkerRequest>,
    pub pid: pid_t,
    pub run_state: RunState,
    /// meant to send listeners to the worker upon start
    pub scm_socket: ScmSocket,
    pub token: Token,
}

/// Serializable state needed to continue one worker session after main upgrade.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkerSessionSnapshot {
    version: u16,
    channel_fd: RawFd,
    channel: ChannelSnapshot,
    id: WorkerId,
    pending: VecDeque<WorkerRequest>,
    pid: pid_t,
    run_state: RunState,
    scm_fd: RawFd,
    token: usize,
    requires_post_commit_tick: bool,
}

impl WorkerSessionSnapshot {
    /// Channel descriptor number in the sending process.
    pub fn channel_fd(&self) -> RawFd {
        self.channel_fd
    }

    /// SCM descriptor number in the sending process.
    pub fn scm_fd(&self) -> RawFd {
        self.scm_fd
    }

    pub fn buffered_bytes(&self) -> usize {
        self.channel.buffered_bytes()
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

/// Restored worker state that owns received descriptors but cannot perform I/O.
///
/// Dropping this wrapper before COMMIT closes both the mio channel and the
/// received SCM descriptor. Only `resume` turns the latter into a `ScmSocket`.
pub struct PausedWorkerSession {
    channel: PausedChannel<WorkerRequest, WorkerResponse>,
    id: WorkerId,
    pending: VecDeque<WorkerRequest>,
    pid: pid_t,
    run_state: RunState,
    scm_fd: OwnedFd,
    token: Token,
    requires_post_commit_tick: bool,
}

impl PausedWorkerSession {
    pub fn id(&self) -> WorkerId {
        self.id
    }

    pub fn token(&self) -> Token {
        self.token
    }

    /// Received SCM descriptor owned by this paused wrapper.
    pub fn scm_fd(&self) -> RawFd {
        self.scm_fd.as_raw_fd()
    }

    /// Whether the Hub must schedule this session once immediately after COMMIT.
    pub fn requires_post_commit_tick(&self) -> bool {
        self.requires_post_commit_tick
    }

    /// Validate mio registration during PREPARED without performing I/O.
    pub fn register(&mut self, registry: &Registry) -> io::Result<()> {
        self.channel.register(
            registry,
            self.token,
            Interest::READABLE | Interest::WRITABLE,
        )
    }

    /// Activate the worker after COMMIT, including its SCM socket.
    pub fn resume(self) -> Result<WorkerSession, SessionSnapshotError> {
        let scm_fd = self.scm_fd.into_raw_fd();
        let scm_socket =
            ScmSocket::new(scm_fd).map_err(SessionSnapshotError::ActivateScmDescriptor)?;

        Ok(WorkerSession {
            channel: self.channel.resume(),
            id: self.id,
            pending: self.pending,
            pid: self.pid,
            run_state: self.run_state,
            scm_socket,
            token: self.token,
        })
    }
}

/// The return type of the ready method
#[derive(Debug)]
pub enum WorkerResult {
    NothingToDo,
    NewResponses(Vec<WorkerResponse>),
    CloseSession,
}

/// sozu#1313: is this write failure "no room in the back buffer RIGHT NOW"?
///
/// `MessageTooLarge` covers two different situations: a frame that fits under
/// the channel ceiling but not in what is left of the buffer — transient, the
/// request is parked and written after the next drain — and a frame bigger than
/// the ceiling itself, which no amount of draining will ever admit. Parking the
/// second would hang its task forever, so it stays a hard error and becomes a
/// synthetic `Failure` for the owning task.
fn is_transient_overflow(error: &ChannelError) -> bool {
    matches!(
        error,
        ChannelError::MessageTooLarge { message_len, max, .. } if message_len <= max
    )
}

impl WorkerSession {
    /// Close both descriptors when a restored terminal session has no
    /// remaining task or response correlation. `ScmSocket` deliberately
    /// borrows its raw descriptor, so dropping the session alone closes only
    /// the command channel.
    pub(crate) fn close_restored_descriptors(self) -> io::Result<()> {
        let scm_fd = self.scm_socket.raw_fd();
        drop(self);
        // SAFETY: the restored Hub owns this descriptor after COMMIT and
        // removes the sole WorkerSession that names it before this call.
        if unsafe { libc::close(scm_fd) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

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
            pending: VecDeque::new(),
            pid,
            run_state: RunState::Running,
            scm_socket,
            token,
        }
    }

    /// Capture this worker without changing CLOEXEC, blocking mode, or bytes.
    pub fn snapshot(&self) -> WorkerSessionSnapshot {
        WorkerSessionSnapshot {
            version: SESSION_SNAPSHOT_VERSION,
            channel_fd: self.channel.fd(),
            channel: self.channel.snapshot(),
            id: self.id,
            pending: self.pending.iter().cloned().collect(),
            pid: self.pid,
            run_state: self.run_state,
            scm_fd: self.scm_socket.raw_fd(),
            token: self.token.0,
            requires_post_commit_tick: self.channel.front_buf.available_data() > 0
                || self.channel.back_buf.available_data() > 0
                || !self.pending.is_empty()
                || wants_to_tick(&self.channel),
        }
    }

    /// Rebuild a worker around descriptors received by the new main.
    ///
    /// `received_scm_fd` stays owned by the paused wrapper. This method only
    /// validates it with `F_GETFL`; `ScmSocket::new` and its `F_SETFL` happen
    /// in `PausedWorkerSession::resume` after COMMIT.
    pub fn restore_paused(
        sock: MioUnixStream,
        received_scm_fd: OwnedFd,
        snapshot: WorkerSessionSnapshot,
        expected_initial_buffer_size: usize,
        expected_max_buffer_size: usize,
    ) -> Result<PausedWorkerSession, SessionSnapshotError> {
        if snapshot.version != SESSION_SNAPSHOT_VERSION {
            return Err(SessionSnapshotError::UnsupportedVersion {
                session: "worker",
                version: snapshot.version,
                supported: SESSION_SNAPSHOT_VERSION,
            });
        }

        let scm_fd = received_scm_fd.as_raw_fd();
        // SAFETY: `received_scm_fd` owns a live descriptor. F_GETFL only
        // observes flags and cannot mutate the open file description shared
        // with the old main during PREPARED.
        if unsafe { libc::fcntl(scm_fd, libc::F_GETFL) } < 0 {
            return Err(SessionSnapshotError::ValidateScmDescriptor {
                fd: scm_fd,
                error: io::Error::last_os_error().to_string(),
            });
        }

        let channel = Channel::restore_paused(
            sock,
            snapshot.channel,
            expected_initial_buffer_size,
            expected_max_buffer_size,
        )?;

        Ok(PausedWorkerSession {
            channel,
            id: snapshot.id,
            pending: snapshot.pending,
            pid: snapshot.pid,
            run_state: snapshot.run_state,
            scm_fd: received_scm_fd,
            token: Token(snapshot.token),
            requires_post_commit_tick: snapshot.requires_post_commit_tick,
        })
    }

    /// accept a request for delivery to the worker (the event loop does the
    /// send)
    ///
    /// sozu#1313, two halves:
    ///
    /// - a HARD channel error used to be logged and forgotten while the caller
    ///   still counted the worker in `expected_responses`, so a request that
    ///   never left the master left its task waiting for an answer that could
    ///   not come — with `Timeout::None` (the bulk replay paths), forever. It
    ///   is returned now, so
    ///   [`crate::command::server::Server::scatter_on`] can account the
    ///   (entry, worker) pair as a `Failure`.
    /// - a back buffer at `max_buffer_size` is NOT an error: the request is
    ///   parked in `Self::pending` and returns `Ok`, because it IS accepted
    ///   for delivery. Raising the ceiling would only move the cliff (it exists
    ///   to bound memory), and flushing the socket synchronously here would
    ///   block the single-threaded supervisor — no client, no worker and no
    ///   task deadline is served while a bulk send is in progress, so the very
    ///   deadline this fix arms could not even be observed.
    pub fn send(&mut self, request: &WorkerRequest) -> Result<(), ChannelError> {
        trace!("Sending to worker: {:?}", request);
        // Ordering: once one request is parked, every later one is parked too,
        // so the worker applies the replay in the order the master scattered it.
        if !self.pending.is_empty() {
            self.pending.push_back(request.clone());
            self.channel.interest.insert(Ready::WRITABLE);
            return Ok(());
        }
        match self.channel.write_message(request) {
            Ok(()) => {}
            Err(e) if is_transient_overflow(&e) => {
                self.pending.push_back(request.clone());
            }
            Err(e) => {
                error!("Could not send request to worker {}: {}", self.id, e);
                self.channel.readiness = Ready::ERROR;
                return Err(e);
            }
        }
        self.channel.interest.insert(Ready::WRITABLE);
        // POST-CONDITION: a successfully queued request leaves the channel
        // registered for WRITABLE, so the event loop will flush it. Dropping
        // this interest would strand the request in the back buffer and hang
        // every task waiting on this worker's response.
        debug_assert!(
            self.channel.interest.is_writable(),
            "send must arm WRITABLE interest so the queued request gets flushed"
        );
        Ok(())
    }

    /// sozu#1313: refill the back buffer from [`Self::pending`] once
    /// [`Channel::writable`] has drained it onto the socket.
    ///
    /// Stops at the first request that no longer fits (the buffer is full
    /// again) and keeps WRITABLE armed, so the next writability event resumes
    /// exactly where this one stopped. A hard channel error marks the session
    /// for closing; the requests still parked here are in `Server::in_flight`
    /// and are answered by `CommandHub::fail_in_flight_requests_of_worker`.
    fn flush_pending(&mut self) {
        while let Some(request) = self.pending.pop_front() {
            match self.channel.write_message(&request) {
                Ok(()) => {}
                Err(e) if is_transient_overflow(&e) => {
                    self.pending.push_front(request);
                    break;
                }
                Err(e) => {
                    error!(
                        "Could not send a parked request to worker {}: {}",
                        self.id, e
                    );
                    self.pending.push_front(request);
                    self.channel.readiness = Ready::ERROR;
                    break;
                }
            }
        }
        if !self.pending.is_empty() {
            // INVARIANT: requests are only left parked because the back buffer
            // is full (or the channel is dying), so there is always something
            // for the event loop to flush — `wants_to_tick` and mio's
            // writability event both key on that buffered data.
            debug_assert!(
                self.channel.back_buf.available_data() > 0 || self.channel.readiness.is_error(),
                "a parked request must leave data for the event loop to flush"
            );
            self.channel.interest.insert(Ready::WRITABLE);
        }
    }

    pub fn update_readiness(&mut self, events: Ready) {
        self.channel.handle_events(events);
    }

    /// drive the channel read and write
    pub fn ready(&mut self) -> WorkerResult {
        let status = self.channel.writable();
        trace!("Worker writable: {:?}", status);
        self.flush_pending();
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
    // Spin guard for the compaction retry below. A compaction frees space
    // without consuming anything, so "it freed space" cannot be trusted as
    // progress on its own — a parser that consumes nothing conserves bytes
    // trivially (sozu-proxy/sozu#1436). At most ONE compaction retry is
    // granted between two delivered messages; it is reset on every `Ok`, so
    // the loop can only keep going by either delivering a message or growing
    // capacity, and capacity is capped at `max_buffer_size`. Both bounds are
    // finite and independent of anything the peer chooses.
    let mut retried_after_compaction = false;
    loop {
        let status = channel.readable();
        trace!("Channel readable: {:?}", status);
        let old_capacity = channel.front_buf.capacity();
        let old_space = channel.front_buf.available_space();
        let old_pending = channel.front_buf.available_data();
        let message = channel.read_message();
        match message {
            Ok(message) => {
                messages.push(message);
                retried_after_compaction = false;
            }
            Err(_) => {
                let new_capacity = channel.front_buf.capacity();
                let new_space = channel.front_buf.available_space();
                let new_pending = channel.front_buf.available_data();
                // INVARIANT: the read buffer only ever grows while we drain it
                // (the channel doubles capacity on a partial read, never
                // shrinks mid-loop). A shrink here would mean `read_message`
                // reallocated downward and dropped buffered bytes — silent
                // message corruption.
                debug_assert!(
                    new_capacity >= old_capacity,
                    "channel read buffer must not shrink while draining messages"
                );
                // Added last and checked first (sozu-proxy/sozu#1445): a FAILED
                // parse that RETIRED bytes. Neither anchor below sees one.
                // Capacity is untouched, and `Buffer::consume`
                // (`command/src/buffer/growable.rs`) advances `position`,
                // shifting only past `capacity / 2`, so retiring an eight-byte
                // prefix leaves `end` — and therefore `available_space()` —
                // exactly where it was. The drain then returned on the very
                // parse that re-synchronised the stream, and the bytes still in
                // the socket were stranded on a session `wants_to_tick` below
                // does not re-schedule.
                //
                // Unlike the compaction retry it needs no spin guard:
                // `consume` is the sole writer of `position`, so
                // `available_data()` can only fall by bytes the parser actually
                // retired, and every `continue` here retires at least one. The
                // iteration count is therefore bounded by the bytes the peer
                // writes — the same bound the `Ok` arm has always had — which
                // is also why it resets the compaction guard, exactly as
                // delivering a message does.
                //
                // Which read failures retire bytes, and why refilling after one
                // is safe, is `rearms_readable` (`command/src/channel.rs`).
                // This anchor is the drain half of that same fix: `readable()`
                // refuses to run while `interest` has lost READABLE, so neither
                // half recovers the stranded bytes without the other.
                if new_pending < old_pending {
                    retried_after_compaction = false;
                    continue;
                }

                // Termination anchor. It used to be "capacity stopped growing",
                // which was the only way `read_message` could make room for a
                // frame it could not yet complete. Since sozu-proxy/sozu#1436 it
                // is not: `try_read_delimited_message` compacts the front buffer
                // when its free tail runs out, which frees space WITHOUT
                // changing capacity. Anchored on capacity alone the loop
                // returned on exactly the parse that made the room — one
                // iteration short of the `readable()` that completes the frame —
                // and nothing scheduled that read: `wants_to_tick` below keys
                // only on writability, hup and error, mio is edge-triggered so
                // the readable edge was already spent, and there is no periodic
                // sweep. The bytes sat in the socket forever, which is
                // sozu-proxy/sozu#1436's operator symptom with the channel-level
                // half fixed. So room made is room to be used, whichever way it
                // was made.
                if new_capacity > old_capacity {
                    continue;
                }
                if new_space > old_space && !retried_after_compaction {
                    // One more pass suffices for a compaction: it leaves
                    // `available_space() == capacity - pending`, and the ceiling
                    // guard in `try_read_delimited_message` has already proven
                    // the declared length is within `max_buffer_size`, so the
                    // rest of the frame arrives in a single `readable()`. If the
                    // peer has not sent it yet, its next write raises a fresh
                    // edge — a peer mid-write is not a peer waiting.
                    retried_after_compaction = true;
                    continue;
                }
                return messages;
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

#[cfg(test)]
mod tests {
    use std::{
        io::{ErrorKind, Read},
        os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd},
        os::unix::net::UnixStream as StdUnixStream,
        sync::Arc,
        time::{Duration, Instant, UNIX_EPOCH},
    };

    use mio::{Poll, Token};
    use sozu_command_lib::{
        channel::Channel,
        proto::command::{
            Request, Response, ResponseStatus, RunState, SoftStop, WorkerRequest, WorkerResponse,
            request::RequestType,
        },
        ready::Ready,
        scm_socket::ScmSocket,
    };

    use super::{
        ClientResult, ClientSession, UpgradeResponseQueue, WorkerSession, extract_messages,
        sanitize_for_audit, sanitize_for_audit_kv, wants_to_tick,
    };
    use crate::command::server::PeerCred;

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

    // -----------------------------------------------------------------
    // sanitize_for_audit_kv: strict, used for column-boundary fields
    // -----------------------------------------------------------------

    #[test]
    fn kv_strips_column_comma() {
        // Comma is the row-separator a SIEM splits the audit line on; an
        // operator-supplied value containing `,` would forge a sibling KV
        // pair against `, key=value` parsers.
        assert_eq!(sanitize_for_audit_kv("x,y"), "x?y");
    }

    #[test]
    fn kv_strips_column_equals() {
        // Equals is the column separator inside a `key=value` pair.
        assert_eq!(sanitize_for_audit_kv("x=y"), "x?y");
    }

    #[test]
    fn kv_strips_c1_nel() {
        // U+0085 NEL is a C1 control byte some normalisers treat as a
        // line break. A byte-only `< 0x20 || == 0x7f` predicate would let
        // it through because UTF-8 encodes NEL as `c2 85` (both >= 0x80).
        assert_eq!(sanitize_for_audit_kv("x\u{0085}y"), "x?y");
    }

    #[test]
    fn kv_strips_c1_csi() {
        // U+009B CSI is the ANSI escape introducer — terminals interpret
        // it as the start of a control sequence. Same C1 / byte-only-
        // predicate trap as NEL above.
        assert_eq!(sanitize_for_audit_kv("x\u{009B}y"), "x?y");
    }

    #[test]
    fn kv_strips_bom() {
        // U+FEFF is non-control by category but some pipelines treat a
        // leading BOM as a delimiter; reject it conservatively.
        assert_eq!(sanitize_for_audit_kv("x\u{FEFF}y"), "x?y");
    }

    #[test]
    fn kv_strips_line_separator() {
        // U+2028 LINE SEPARATOR splits the audit row in any consumer that
        // honours the Unicode line-break property.
        assert_eq!(sanitize_for_audit_kv("x\u{2028}y"), "x?y");
    }

    #[test]
    fn kv_strips_paragraph_separator() {
        // U+2029 PARAGRAPH SEPARATOR — same rationale as LINE SEPARATOR.
        assert_eq!(sanitize_for_audit_kv("x\u{2029}y"), "x?y");
    }

    #[test]
    fn kv_preserves_safe_ascii() {
        // No control / column-boundary / line-break character present:
        // the fast path returns the original string unchanged.
        assert_eq!(sanitize_for_audit_kv("safe-id_42"), "safe-id_42");
    }

    #[test]
    fn kv_preserves_in_value_colon() {
        // `:` is not a column separator at the audit-line level (legit
        // values like `target=host:8080` rely on it).
        assert_eq!(sanitize_for_audit_kv("host:8080"), "host:8080");
    }

    // -----------------------------------------------------------------
    // sanitize_for_audit: line-only, no column-boundary stripping
    // -----------------------------------------------------------------

    #[test]
    fn line_keeps_comma() {
        // The weak variant feeds fields rendered outside the `, key=value`
        // shape (the `reason=` column is one big quoted blob), so `,` is
        // legal text and must survive sanitisation.
        assert_eq!(sanitize_for_audit("x,y"), "x,y");
    }

    #[test]
    fn line_keeps_equals() {
        // Same reasoning as `line_keeps_comma`: `=` is legal text inside
        // a quoted reason payload.
        assert_eq!(sanitize_for_audit("x=y"), "x=y");
    }

    #[test]
    fn line_strips_c1_nel() {
        // The weak sanitiser MUST still catch C1 controls — the prior
        // byte-only predicate let them through.
        assert_eq!(sanitize_for_audit("x\u{0085}y"), "x?y");
    }

    #[test]
    fn line_strips_c1_csi() {
        assert_eq!(sanitize_for_audit("x\u{009B}y"), "x?y");
    }

    #[test]
    fn line_strips_bom() {
        assert_eq!(sanitize_for_audit("x\u{FEFF}y"), "x?y");
    }

    #[test]
    fn line_strips_line_separator() {
        assert_eq!(sanitize_for_audit("x\u{2028}y"), "x?y");
    }

    #[test]
    fn line_strips_paragraph_separator() {
        assert_eq!(sanitize_for_audit("x\u{2029}y"), "x?y");
    }

    #[test]
    fn line_strips_c0_control() {
        // C0 controls (tab, LF, NUL, etc.) were the original target of
        // the byte-based predicate; the rewrite must keep covering them.
        assert_eq!(sanitize_for_audit("x\ty\nz\0"), "x?y?z?");
    }

    #[test]
    fn line_strips_del() {
        // DEL (U+007F) is `char::is_control()` true.
        assert_eq!(sanitize_for_audit("x\u{007F}y"), "x?y");
    }

    // -----------------------------------------------------------------
    // bidirectional override / isolate class — Trojan-Source defence
    // -----------------------------------------------------------------

    #[test]
    fn line_strips_rtl_override() {
        // U+202E RIGHT-TO-LEFT OVERRIDE visually reverses the bytes that
        // follow when an operator tails the audit log in a Unicode-aware
        // terminal. The CVE-2021-42574 class — strip before render.
        assert_eq!(sanitize_for_audit("a\u{202E}b"), "a?b");
        assert_eq!(sanitize_for_audit_kv("a\u{202E}b"), "a?b");
    }

    #[test]
    fn line_strips_bidi_override_range() {
        // U+202A..=U+202E are the bidi override controls (LRE, RLE, PDF,
        // LRO, RLO). All have the same audit-row reorder hazard.
        for c in ['\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}'] {
            let input = format!("a{c}b");
            assert_eq!(sanitize_for_audit(&input), "a?b");
            assert_eq!(sanitize_for_audit_kv(&input), "a?b");
        }
    }

    #[test]
    fn line_strips_bidi_isolate_range() {
        // U+2066..=U+2069 are the bidi isolate controls (LRI, RLI, FSI,
        // PDI). Same hazard as the override class.
        for c in ['\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}'] {
            let input = format!("a{c}b");
            assert_eq!(sanitize_for_audit(&input), "a?b");
            assert_eq!(sanitize_for_audit_kv(&input), "a?b");
        }
    }

    #[test]
    fn line_preserves_legitimate_bidi_text() {
        // Plain RTL script content (Hebrew / Arabic) must round-trip
        // through both sanitisers — only the explicit override / isolate
        // controls are rejected.
        let input = "héllo שלום مرحبا";
        assert_eq!(sanitize_for_audit(input), input);
        assert_eq!(sanitize_for_audit_kv(input), input);
    }

    // -----------------------------------------------------------------
    // oversized declared frame length: the supervisor must drop the peer
    // -----------------------------------------------------------------

    /// Regression for sozu-proxy/sozu#1428, the supervisor half of
    /// `command/src/channel.rs`'s
    /// `oversized_declared_length_marks_the_channel_for_closing`.
    ///
    /// `try_read_delimited_message` cannot re-sync past a declared length
    /// above `max_buffer_size`, so it signals the only safe recovery through
    /// `Ready::ERROR`. This pins the two links that turn that signal into the
    /// behaviour `doc/configure_admin_ops.md` §5.2 promises: `wants_to_tick`
    /// must schedule the session even though nothing is buffered to write,
    /// and `ClientSession::ready` must then answer `CloseSession`.
    ///
    /// Before the fix both links were absent — `extract_messages` swallowed
    /// the error with its bare `Err(_)` arm, no readiness bit moved, and the
    /// session stayed registered forever with the poisoned delimiter parked
    /// at the head of its front buffer.
    #[test]
    fn client_session_closes_on_oversized_declared_length() {
        let (channel, mut writer): (Channel<Response, Request>, Channel<Request, Response>) =
            Channel::generate_nonblocking(1000, 10000)
                .expect("could not generate nonblocking channels");

        let mut client = ClientSession::new(
            channel,
            1,
            Token(1),
            PeerCred {
                uid: None,
                gid: None,
                pid: None,
            },
            None,
            None,
            Arc::from("/tmp/sozu-test.sock"),
        );

        // Raw write: `write_delimited_message` refuses to emit a frame this large.
        let oversized: usize = 10_001;
        std::io::Write::write_all(&mut writer.sock, &oversized.to_le_bytes())
            .expect("raw write of the oversized delimiter");

        client.update_readiness(Ready::READABLE);

        // The tick that parses the bad header yields no request: the channel is
        // marked errored from inside `extract_messages`, after `ready`'s own
        // entry check has already run.
        assert!(
            matches!(client.ready(), ClientResult::NothingToDo),
            "the parsing tick yields no request"
        );

        // The event loop must be told to come back, or the mark is never read:
        // nothing is queued to write and no mio event is owed, so `wants_to_tick`
        // is the only thing that re-schedules this session.
        assert!(
            wants_to_tick(&client.channel),
            "a channel marked for closing must be re-scheduled by the event loop"
        );

        assert!(
            matches!(client.ready(), ClientResult::CloseSession),
            "the supervisor must drop a peer that declared an unsatisfiable \
             frame length, instead of leaving the session wedged on a delimiter \
             it can neither complete nor re-sync past"
        );
    }

    /// Regression for the session half of sozu-proxy/sozu#1436: making the
    /// channel *recoverable* is not making the session *recover*.
    ///
    /// `try_read_delimited_message` now compacts the front buffer instead of
    /// returning `BufferFull`, which frees space without changing capacity. But
    /// `extract_messages`' loop terminated on `old_capacity == new_capacity`, so
    /// it returned on exactly the parse that made the room -- one iteration
    /// short of the `readable()` that completes the frame. The remaining bytes
    /// then sat unread in the socket with nothing to schedule another tick:
    /// `wants_to_tick` keys only on `(writable && back_buf.available_data() > 0)
    /// || hup || error`, READABLE is not among them, mio is edge-triggered so
    /// the readable edge was already consumed, and there is no periodic sweep.
    /// For the canonical peer -- write a request, wait for the response --
    /// "recoverable on the peer's next byte" means never.
    ///
    /// The shape, at `capacity == max_buffer_size == 64`: an 8-byte frame (an
    /// empty `Request` is a bare delimiter) at most half the capacity, then a
    /// 64-byte `LoadState` frame, written together in ONE 72-byte write. The
    /// first `readable()` fills the buffer to the brim at `position == 0`
    /// (`Buffer::fill` compacts when a read reaches the end), the decode of the
    /// first frame leaves `position == 8` without reaching `Buffer::consume`'s
    /// `capacity / 2` threshold, and the second frame -- 64 bytes declared, 56
    /// buffered -- lands on the compaction path with 8 bytes still in the
    /// socket.
    ///
    /// Both frames arrive in one tick, so `ready`'s `requests.pop()` returns the
    /// second and logs "more than one request at a time" over the first. That
    /// pipelining loss is pre-existing `ready` behaviour, unrelated to this fix;
    /// what this test pins is that the second frame is delivered at all.
    #[test]
    fn client_session_completes_a_compacted_frame_without_another_peer_write() {
        let capacity = 64u64;
        let (channel, mut writer): (Channel<Response, Request>, Channel<Request, Response>) =
            Channel::generate_nonblocking(capacity, capacity)
                .expect("could not generate nonblocking channels");

        let mut client = ClientSession::new(
            channel,
            1,
            Token(1),
            PeerCred {
                uid: None,
                gid: None,
                pid: None,
            },
            None,
            None,
            Arc::from("/tmp/sozu-test.sock"),
        );

        // Frame one: an empty `Request` frames to nothing but its delimiter, so
        // it is 8 bytes -- comfortably under `Buffer::consume`'s 32-byte shift
        // threshold, which is what leaves `position` stranded mid-buffer.
        writer
            .write_delimited_message(&Request::default())
            .expect("could not frame the first request");
        let mut wire = writer.back_buf.data().to_vec();
        writer.back_buf.consume(wire.len());
        assert!(
            wire.len() <= capacity as usize / 2,
            "the first frame ({} bytes) must stay under the {}-byte shift \
             threshold or the layout under test never forms",
            wire.len(),
            capacity / 2
        );

        // Frame two: a real `LoadState` whose path is sized so the frame is
        // exactly `capacity` -- the largest frame this end may legitimately be
        // asked to accept, and one that cannot fit behind the first frame's
        // leftover offsets.
        let payload = capacity as usize - wire.len() - 2;
        let expected = Request {
            request_type: Some(RequestType::LoadState("x".repeat(payload))),
        };
        writer
            .write_delimited_message(&expected)
            .expect("could not frame the second request");
        let second = writer.back_buf.data().to_vec();
        writer.back_buf.consume(second.len());
        assert_eq!(
            second.len(),
            capacity as usize,
            "the second frame must be exactly the ceiling"
        );
        wire.extend_from_slice(&second);

        // ONE write, then the peer waits for its response -- it owes no further
        // byte, and no further byte is what the defect needed.
        assert_eq!(wire.len(), 72);
        std::io::Write::write_all(&mut writer.sock, &wire)
            .expect("raw write of both frames in a single write");

        client.update_readiness(Ready::READABLE);

        match client.ready() {
            ClientResult::NewRequest(request) => assert_eq!(
                request, expected,
                "the compacted frame must be delivered on this tick: the peer \
                 sent every byte it owes and nothing will schedule another one"
            ),
            other => panic!(
                "expected the second frame to be delivered, got {other:?}\n\
                 NOTE: this is the session half of #1436 -- the channel \
                 compacted, but `extract_messages` returned before the read \
                 that completes the frame"
            ),
        }

        assert_eq!(
            client.channel.front_buf.available_data(),
            0,
            "the whole 72-byte write must have been drained"
        );

        // And this is why the drain loop, not `wants_to_tick`, had to be the
        // fix: nothing re-schedules a session that is merely readable.
        assert!(
            !wants_to_tick(&client.channel),
            "no queued response, no hup, no error: had the drain stopped early \
             the session would never have been ticked again"
        );
    }

    #[test]
    fn main_upgrade_terminal_response_waits_for_inherited_backpressure() {
        let (channel, mut peer): (Channel<Response, Request>, Channel<Request, Response>) =
            Channel::generate_nonblocking(64, 512).expect("could not generate client channels");
        let mut client = ClientSession::new(
            channel,
            1,
            Token(1),
            PeerCred::default(),
            None,
            None,
            Arc::from("/tmp/sozu-test.sock"),
        );
        let existing = Response {
            status: ResponseStatus::Processing.into(),
            message: "existing".repeat(45),
            content: None,
        };
        let terminal = Response {
            status: ResponseStatus::Ok.into(),
            message: "upgrade-complete".repeat(24),
            content: None,
        };
        client
            .channel
            .write_message(&existing)
            .expect("existing response should fit by itself");

        assert_eq!(
            client.try_queue_upgrade_response(&terminal),
            UpgradeResponseQueue::Backpressured,
            "terminal response must remain pending instead of poisoning the client channel"
        );

        client.channel.handle_events(Ready::WRITABLE);
        client
            .channel
            .run()
            .expect("existing response should flush");
        peer.handle_events(Ready::READABLE);
        peer.run().expect("existing response should buffer");
        assert_eq!(
            peer.read_message()
                .expect("existing response should arrive"),
            existing
        );

        assert_eq!(
            client.try_queue_upgrade_response(&terminal),
            UpgradeResponseQueue::Queued
        );
        client.channel.handle_events(Ready::WRITABLE);
        client
            .channel
            .run()
            .expect("terminal response should flush");
        peer.handle_events(Ready::READABLE);
        peer.run().expect("terminal response should buffer");
        assert_eq!(
            peer.read_message()
                .expect("terminal response should arrive once"),
            terminal
        );
        assert!(
            peer.read_message().is_err(),
            "retrying admission must not duplicate the terminal response"
        );
    }

    /// Regression for sozu-proxy/sozu#1445: a malformed frame must not strand
    /// the bytes behind it on a session nothing will schedule again. Which read
    /// failures leave a channel able to make progress, and why, is
    /// `rearms_readable` (`command/src/channel.rs`);
    /// `MessageLengthUnderDelimiter` is the one a peer reaches today.
    ///
    /// The shape, at `capacity == max_buffer_size == 64`: a 40-byte frame, an
    /// 8-byte prefix declaring 3 (below the delimiter, a value no writer can
    /// emit for any payload), and a 46-byte frame, written together in ONE
    /// 94-byte write. The first `readable()` takes 64 of those 94 bytes and
    /// drops READABLE at the ceiling; frame one decodes; the bogus prefix is
    /// consumed -- and the remaining 30 bytes of frame two sat in the socket
    /// with `wants_to_tick` false, no hup and no error.
    ///
    /// Delivery has to happen inside this tick, because `wants_to_tick` below
    /// does not re-schedule a session for being merely readable. So this counts
    /// what the drain hands back rather than asserting a readiness bit -- and
    /// counts it rather than naming one frame, because `ClientSession::ready`
    /// returns `requests.pop()` and which of the two that is, is its own
    /// pre-existing pipelining question rather than this one.
    #[test]
    fn client_session_delivers_the_frame_behind_a_malformed_length_prefix() {
        let capacity = 64u64;
        let (channel, mut writer): (Channel<Response, Request>, Channel<Request, Response>) =
            Channel::generate_nonblocking(capacity, capacity)
                .expect("could not generate nonblocking channels");

        let mut client = ClientSession::new(
            channel,
            1,
            Token(1),
            PeerCred {
                uid: None,
                gid: None,
                pid: None,
            },
            None,
            None,
            Arc::from("/tmp/sozu-test.sock"),
        );

        // Frame one: a well-formed request that decodes on the first parse and
        // leaves the read cursor part-way into the buffer.
        let first = Request {
            request_type: Some(RequestType::LoadState("x".repeat(30))),
        };
        writer
            .write_delimited_message(&first)
            .expect("could not frame the first request");
        let mut wire = writer.back_buf.data().to_vec();
        writer.back_buf.consume(wire.len());
        assert_eq!(wire.len(), 40, "the first frame must be 40 bytes");

        // The malformed prefix: a declared length below `delimiter_size()`, the
        // one value `write_delimited_message` can never emit, so the parser is
        // entitled to skip exactly those 8 bytes and re-align.
        let under_delimiter: usize = 3;
        wire.extend_from_slice(&under_delimiter.to_le_bytes());

        // Frame two: well-formed, and split by the 64-byte ceiling -- 16 of its
        // bytes land in the buffer behind the bogus prefix, 30 stay in the
        // socket and are the bytes that used to be stranded.
        let second_request = Request {
            request_type: Some(RequestType::LoadState("y".repeat(36))),
        };
        writer
            .write_delimited_message(&second_request)
            .expect("could not frame the second request");
        let second = writer.back_buf.data().to_vec();
        writer.back_buf.consume(second.len());
        assert_eq!(second.len(), 46, "the second frame must be 46 bytes");
        wire.extend_from_slice(&second);

        // ONE write, then the peer waits for its response -- it owes no further
        // byte, and no further byte is what the defect needed.
        assert_eq!(wire.len(), 94);
        std::io::Write::write_all(&mut writer.sock, &wire)
            .expect("raw write of the whole 94-byte stream in a single write");

        client.update_readiness(Ready::READABLE);

        let delivered = extract_messages(&mut client.channel);
        assert_eq!(
            delivered.len(),
            2,
            "both frames must be delivered on this tick: the peer sent every \
             byte it owes and nothing will schedule another one.\n\
             NOTE: this is sozu-proxy/sozu#1445 -- 1 here is the frame BEFORE \
             the bogus prefix delivered alone, the 30 bytes behind it left in \
             the socket, because the parse that consumed that prefix returned \
             through `?` without re-arming READABLE and `readable()` could no \
             longer refill the buffer"
        );

        assert_eq!(
            client.channel.front_buf.available_data(),
            0,
            "the whole 94-byte write must have been drained"
        );

        // And this is why the delivery, not a readiness bit, is the assertion:
        // nothing re-schedules a session that is merely readable.
        assert!(
            !wants_to_tick(&client.channel),
            "no queued response, no hup, no error: had the drain stopped early \
             the session would never have been ticked again"
        );
    }

    fn worker_request(id: &str) -> WorkerRequest {
        WorkerRequest {
            id: id.to_owned(),
            content: Request::default(),
        }
    }

    fn fd_is_nonblocking(fd: std::os::fd::RawFd) -> bool {
        // SAFETY: callers pass a live descriptor and F_GETFL only observes its
        // shared open-file-description flags.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert!(flags >= 0, "could not read descriptor flags");
        flags & libc::O_NONBLOCK != 0
    }

    #[test]
    fn client_session_snapshot_round_trips_identity_buffers_and_post_commit_tick() {
        let (channel, mut peer): (Channel<Response, Request>, Channel<Request, Response>) =
            Channel::generate_nonblocking(32, 256).expect("could not generate client channels");
        let mut client = ClientSession::new(
            channel,
            41,
            Token(73),
            PeerCred {
                uid: Some(1000),
                gid: Some(1001),
                pid: Some(4242),
            },
            Some("sozu-cli".to_owned()),
            Some("operator".to_owned()),
            Arc::from("/run/sozu/control.sock"),
        );
        client.session_ulid = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
            .parse()
            .expect("fixed ULID should parse");
        client.connect_ts = UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789);

        let request = Request::from(RequestType::SoftStop(SoftStop {}));
        peer.write_message(&request)
            .expect("could not frame client request");
        peer.handle_events(Ready::WRITABLE);
        peer.run().expect("could not write client request");
        client.update_readiness(Ready::READABLE);
        client
            .channel
            .readable()
            .expect("could not buffer client request");
        client.channel.readiness = Ready::EMPTY;

        let response = Response {
            status: ResponseStatus::Ok.into(),
            message: "original-client-response".to_owned(),
            content: None,
        };
        client.send(response);
        client.channel.readiness = Ready::EMPTY;
        let expected_front = client.channel.front_buf.data().to_owned();
        let expected_back = client.channel.back_buf.data().to_owned();
        let source_fd = client.channel.fd();

        let snapshot = client.snapshot();
        assert_eq!(snapshot.channel_fd(), source_fd);
        let encoded = serde_json::to_vec(&snapshot).expect("client snapshot should serialize");
        let snapshot =
            serde_json::from_slice(&encoded).expect("client snapshot should deserialize");
        let mut paused = ClientSession::restore_paused(client.channel.sock, snapshot, 32, 256)
            .expect("client snapshot should restore paused");
        assert_eq!(paused.id(), 41);
        assert_eq!(paused.token(), Token(73));
        assert!(
            paused.requires_post_commit_tick(),
            "a complete userspace frame with empty readiness needs one post-commit tick"
        );

        let poll = Poll::new().expect("could not create poll registry");
        paused
            .register(poll.registry())
            .expect("paused client should register without I/O");
        peer.handle_events(Ready::READABLE);
        assert_eq!(
            peer.readable().expect("peer read probe should not fail"),
            0,
            "PREPARED registration must not flush the buffered response"
        );

        let mut restored = paused.resume();
        assert_eq!(restored.id, 41);
        assert_eq!(restored.token, Token(73));
        assert_eq!(
            restored.session_ulid.to_string(),
            "01ARZ3NDEKTSV4RRFFQ69G5FAV"
        );
        assert_eq!(restored.actor_uid, Some(1000));
        assert_eq!(restored.actor_gid, Some(1001));
        assert_eq!(restored.actor_pid, Some(4242));
        assert_eq!(restored.actor_comm.as_deref(), Some("sozu-cli"));
        assert_eq!(restored.actor_user.as_deref(), Some("operator"));
        assert_eq!(&*restored.socket_path, "/run/sozu/control.sock");
        assert_eq!(
            restored.connect_ts,
            UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789)
        );
        assert_eq!(restored.channel.front_buf.data(), expected_front);
        assert_eq!(restored.channel.back_buf.data(), expected_back);
        assert_eq!(restored.channel.readiness, Ready::EMPTY);
        assert!(matches!(
            restored.ready(),
            ClientResult::NewRequest(restored_request) if restored_request == request
        ));
        assert!(
            matches!(restored.ready(), ClientResult::NothingToDo),
            "the buffered Stop command must be dispatched exactly once after COMMIT"
        );
    }

    #[test]
    fn client_session_snapshot_rejects_pre_epoch_connect_time() {
        let (channel, _peer): (Channel<Response, Request>, Channel<Request, Response>) =
            Channel::generate_nonblocking(32, 256).expect("could not generate client channels");
        let mut client = ClientSession::new(
            channel,
            1,
            Token(2),
            PeerCred {
                uid: None,
                gid: None,
                pid: None,
            },
            None,
            None,
            Arc::from("/run/sozu/control.sock"),
        );
        client.connect_ts = UNIX_EPOCH - Duration::from_nanos(1);

        let error = serde_json::to_vec(&client.snapshot())
            .expect_err("serde's SystemTime representation rejects pre-epoch values");
        assert!(error.to_string().contains("later than UNIX_EPOCH"));
    }

    #[test]
    fn client_session_restore_rejects_unknown_snapshot_version() {
        let (channel, _peer): (Channel<Response, Request>, Channel<Request, Response>) =
            Channel::generate_nonblocking(32, 256).expect("could not generate client channels");
        let client = ClientSession::new(
            channel,
            1,
            Token(2),
            PeerCred {
                uid: None,
                gid: None,
                pid: None,
            },
            None,
            None,
            Arc::from("/run/sozu/control.sock"),
        );
        let mut snapshot = client.snapshot();
        snapshot.version += 1;

        assert!(matches!(
            ClientSession::restore_paused(client.channel.sock, snapshot, 32, 256),
            Err(super::SessionSnapshotError::UnsupportedVersion {
                session: "client",
                ..
            })
        ));
    }

    #[test]
    fn worker_session_snapshot_round_trips_fifo_and_defers_scm_activation() {
        let (channel, mut peer): (
            Channel<WorkerRequest, WorkerResponse>,
            Channel<WorkerResponse, WorkerRequest>,
        ) = Channel::generate_nonblocking(64, 512).expect("could not generate worker channels");
        let (source_scm_owner, _source_scm_peer) =
            StdUnixStream::pair().expect("could not create source SCM pair");
        let scm_socket =
            ScmSocket::new(source_scm_owner.as_raw_fd()).expect("could not create SCM socket");
        let mut worker = WorkerSession::new(channel, 17, 4242, Token(91), scm_socket);
        worker.run_state = RunState::Stopping;
        worker.pending.extend([
            worker_request("first"),
            worker_request("second"),
            worker_request("third"),
        ]);

        let response = WorkerResponse {
            id: "reply-in-front".to_owned(),
            status: ResponseStatus::Ok.into(),
            message: "ready".to_owned(),
            content: None,
        };
        peer.write_message(&response)
            .expect("could not frame worker response");
        peer.handle_events(Ready::WRITABLE);
        peer.run().expect("could not write worker response");
        worker.update_readiness(Ready::READABLE);
        worker
            .channel
            .readable()
            .expect("could not buffer worker response");
        worker.channel.readiness = Ready::EMPTY;
        worker
            .channel
            .write_message(&worker_request("already-buffered"))
            .expect("could not queue worker request");
        worker.channel.readiness = Ready::EMPTY;
        let expected_front = worker.channel.front_buf.data().to_owned();
        let expected_back = worker.channel.back_buf.data().to_owned();

        let snapshot = worker.snapshot();
        assert_eq!(snapshot.channel_fd(), worker.channel.fd());
        assert_eq!(snapshot.scm_fd(), source_scm_owner.as_raw_fd());
        let encoded = serde_json::to_vec(&snapshot).expect("worker snapshot should serialize");
        let snapshot =
            serde_json::from_slice(&encoded).expect("worker snapshot should deserialize");

        let (received_scm, _received_scm_peer) =
            StdUnixStream::pair().expect("could not create received SCM pair");
        received_scm
            .set_nonblocking(true)
            .expect("could not make received SCM descriptor nonblocking");
        let received_scm_fd = received_scm.into_raw_fd();
        // SAFETY: `into_raw_fd` transferred unique ownership to this test.
        let received_scm = unsafe { OwnedFd::from_raw_fd(received_scm_fd) };
        assert!(fd_is_nonblocking(received_scm_fd));

        let mut paused =
            WorkerSession::restore_paused(worker.channel.sock, received_scm, snapshot, 64, 512)
                .expect("worker snapshot should restore paused");
        assert_eq!(paused.id(), 17);
        assert_eq!(paused.token(), Token(91));
        assert!(paused.requires_post_commit_tick());
        assert!(
            fd_is_nonblocking(received_scm_fd),
            "PREPARED restore must not change SCM blocking mode"
        );
        let poll = Poll::new().expect("could not create poll registry");
        paused
            .register(poll.registry())
            .expect("paused worker should register without I/O");
        assert!(
            fd_is_nonblocking(received_scm_fd),
            "PREPARED registration must not change SCM blocking mode"
        );

        let restored = paused
            .resume()
            .expect("COMMIT should activate the received SCM descriptor");
        assert!(
            !fd_is_nonblocking(received_scm_fd),
            "resume is the first point allowed to make SCM blocking"
        );
        assert_eq!(restored.id, 17);
        assert_eq!(restored.pid, 4242);
        assert_eq!(restored.run_state, RunState::Stopping);
        assert_eq!(restored.token, Token(91));
        assert_eq!(restored.scm_socket.raw_fd(), received_scm_fd);
        assert_eq!(restored.channel.front_buf.data(), expected_front);
        assert_eq!(restored.channel.back_buf.data(), expected_back);
        assert_eq!(
            restored
                .pending
                .iter()
                .map(|request| request.id.as_str())
                .collect::<Vec<_>>(),
            ["first", "second", "third"]
        );
    }

    #[test]
    fn dropping_paused_worker_closes_received_scm_descriptor() {
        let (channel, mut channel_peer): (
            Channel<WorkerRequest, WorkerResponse>,
            Channel<WorkerResponse, WorkerRequest>,
        ) = Channel::generate_nonblocking(64, 512).expect("could not generate worker channels");
        let (source_scm_owner, _source_scm_peer) =
            StdUnixStream::pair().expect("could not create source SCM pair");
        let worker = WorkerSession::new(
            channel,
            17,
            4242,
            Token(91),
            ScmSocket::new(source_scm_owner.as_raw_fd()).expect("could not create SCM socket"),
        );
        let snapshot = worker.snapshot();
        let (received_scm, mut received_scm_peer) =
            StdUnixStream::pair().expect("could not create received SCM pair");
        received_scm_peer
            .set_nonblocking(true)
            .expect("could not make received SCM peer nonblocking");
        let received_scm_fd = received_scm.into_raw_fd();
        // SAFETY: `into_raw_fd` transferred unique ownership to this test.
        let received_scm = unsafe { OwnedFd::from_raw_fd(received_scm_fd) };

        let paused =
            WorkerSession::restore_paused(worker.channel.sock, received_scm, snapshot, 64, 512)
                .expect("worker snapshot should restore paused");
        drop(paused);

        // Observe both original peers before asserting either result. Numeric
        // descriptors can be reused immediately by another parallel test;
        // EOF identifies the socket endpoint and still fails if any real copy
        // remains open across a concurrent fork-to-exec window.
        let channel_eof = wait_for_eof(&mut channel_peer.sock, "paused worker channel");
        let scm_eof = wait_for_eof(&mut received_scm_peer, "paused worker SCM socket");
        assert!(
            channel_eof.is_ok() && scm_eof.is_ok(),
            "channel: {channel_eof:?}; SCM: {scm_eof:?}"
        );
    }
}
