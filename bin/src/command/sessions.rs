//! Per-client command-socket session state.
//!
//! Tracks every connected CLI/`sozu` command-socket client (pid, comm,
//! authenticated peer credentials) and the in-flight requests waiting on
//! worker responses. Owns the PID-reuse-guarded `peer_comm` snapshot used
//! by the audit envelope so reused PIDs cannot impersonate another
//! command source. Long-form lifecycle: `bin/src/command/LIFECYCLE.md`.

use std::{collections::VecDeque, fmt::Debug, sync::Arc, time::SystemTime};

use libc::pid_t;
use mio::Token;
use prost::Message;
use rusty_ulid::Ulid;
use sozu_command_lib::{
    channel::{Channel, ChannelError},
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
    use std::sync::Arc;

    use mio::Token;
    use sozu_command_lib::{
        channel::Channel,
        proto::command::{Request, Response, request::RequestType},
        ready::Ready,
    };

    use super::{
        ClientResult, ClientSession, extract_messages, sanitize_for_audit, sanitize_for_audit_kv,
        wants_to_tick,
    };
    use crate::command::server::PeerCred;

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
}
