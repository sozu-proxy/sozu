//! H1 mux connection wrapper.
//!
//! Hosts the single active H1 stream (`stream: GlobalStreamId`) and wires
//! Kawa-owned H1 parsing + serialization into the shared mux `Context` so the
//! same routing / shutdown / readiness machinery applies across H1 and H2
//! connections. Long-form lifecycle: `lib/src/protocol/mux/LIFECYCLE.md`.

use std::{
    io::IoSlice,
    time::{Duration, Instant},
};

use rusty_ulid::Ulid;
use sozu_command::{logging::ansi_palette, ready::Ready};

use crate::metrics::names;
use crate::{
    L7ListenerHandler, ListenerHandler, Readiness,
    protocol::mux::{
        BackendStatus, Context, DebugEvent, Endpoint, GlobalStreamId, MuxResult, Position,
        StreamState, forcefully_terminate_answer,
        parser::H2Error,
        remove_backend_stream, set_default_answer,
        shared::{EndStreamAction, drain_tls_close_notify, end_stream_decision},
        update_readiness_after_read, update_readiness_after_write,
    },
    socket::{SocketHandler, SocketResult, stats::socket_rtt},
};

/// Prefix applied to every [`ConnectionH1`] log line. Matches the RUSTLS
/// log-context convention (`MUX-H1\tSession(...)\t >>>`). When the logger is
/// in colored mode the label is bold bright-white (uniform across every
/// protocol) and the session detail is rendered in light grey.
///
/// Fields included in the session block (chosen to surface the most common
/// H1 troubleshooting axes — keep-alive churn, stream pinning, buffer-pressure
/// stall and graceful TLS shutdown):
/// - `peer` — peer address via [`SocketHandler::peer_addr`](crate::socket::SocketHandler::peer_addr),
///   snapshotted once into `ConnectionH1::peer_address` at construction rather
///   than looked up live on each expansion. It therefore survives the peer's
///   RST (which is when these lines are read); on a PROXY-protocol frontend it
///   names the advertised client rather than the load balancer, matching the
///   `HTTPS`/`HTTP` line for the same request id; and on a backend connection
///   it names the cluster-configured address even while the async `connect()`
///   is still in flight and `getpeername(2)` would refuse
/// - `position` — `Server` / `Client(...)` orientation
/// - `stream` — currently active [`GlobalStreamId`] (or `none`)
/// - `requests` — request count served on this connection (keep-alive)
/// - `parked` — set when the kawa buffer is full and `READABLE` is suspended
/// - `close_notify` — TLS `close_notify` send state
/// - `readiness` — connection-level mio readiness snapshot
macro_rules! log_context {
    ($self:expr) => {{
        let (open, reset, grey, gray, white) = ansi_palette();
        format!(
            "[{ulid} - - -]\t{open}MUX-H1{reset}\t{grey}Session{reset}({gray}peer{reset}={white}{peer:?}{reset}, {gray}position{reset}={white}{position:?}{reset}, {gray}stream{reset}={white}{stream:?}{reset}, {gray}requests{reset}={white}{requests}{reset}, {gray}parked{reset}={white}{parked}{reset}, {gray}close_notify{reset}={white}{close_notify}{reset}, {gray}readiness{reset}={white}{readiness}{reset})\t >>>",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            ulid = $self.session_ulid,
            peer = $self.peer_address,
            position = $self.position,
            stream = $self.stream,
            requests = $self.requests,
            parked = $self.parked_on_buffer_pressure,
            close_notify = $self.close_notify_sent,
            readiness = $self.readiness,
        )
    }};
}

/// Per-stream variant of [`log_context!`] used when a `HttpContext` is in
/// scope. Fills the `request_id` slot of the bracket so the log line can be
/// grepped by the specific request that triggered it.
#[allow(unused_macros)]
macro_rules! log_context_stream {
    ($self:expr, $http_context:expr) => {{
        let (open, reset, grey, gray, white) = ansi_palette();
        format!(
            "[{ulid} {req} {cluster} {backend}]\t{open}MUX-H1{reset}\t{grey}Session{reset}({gray}peer{reset}={white}{peer:?}{reset}, {gray}position{reset}={white}{position:?}{reset}, {gray}stream{reset}={white}{stream:?}{reset}, {gray}requests{reset}={white}{requests}{reset}, {gray}parked{reset}={white}{parked}{reset}, {gray}close_notify{reset}={white}{close_notify}{reset}, {gray}readiness{reset}={white}{readiness}{reset})\t >>>",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            ulid = $self.session_ulid,
            req = $http_context.id,
            cluster = $http_context.cluster_id.as_deref().unwrap_or("-"),
            backend = $http_context.backend_id.as_deref().unwrap_or("-"),
            peer = $self.peer_address,
            position = $self.position,
            stream = $self.stream,
            requests = $self.requests,
            parked = $self.parked_on_buffer_pressure,
            close_notify = $self.close_notify_sent,
            readiness = $self.readiness,
        )
    }};
}

/// Module-level prefix for logs without a [`ConnectionH1`] in scope. Honours
/// the colored flag.
macro_rules! log_module_context {
    () => {{
        let (open, reset, _, _, _) = ansi_palette();
        format!("{open}MUX-H1{reset}\t >>>", open = open, reset = reset)
    }};
}

/// HTTP/1.1 connection handler within the mux layer.
///
/// Manages a single HTTP/1.1 connection (either frontend or backend),
/// handling request/response forwarding through kawa buffers. Supports
/// keep-alive, chunked transfer encoding, close-delimited responses,
/// and upgrade (e.g., WebSocket).
pub struct ConnectionH1<Front: SocketHandler> {
    pub position: Position,
    pub readiness: Readiness,
    pub requests: usize,
    pub socket: Front,
    /// Peer address of this connection, captured once at construction from
    /// [`SocketHandler::peer_addr`](crate::socket::SocketHandler::peer_addr)
    /// and never re-read.
    ///
    /// This is the `peer=` slot every `log_context!` line carries, and the one
    /// its per-stream twin `log_context_stream!` fills. Both used to build it
    /// from `socket.socket_ref().peer_addr().ok()` — a live `getpeername(2)`
    /// that reached past the trait method added to stop exactly that.
    ///
    /// Both production handlers *prefer* a cached address and fall back to a
    /// live lookup when they hold none: `SessionTcpStream` and `FrontRustls`
    /// each answer `self.configured_peer.or_else(|| self.stream.peer_addr().ok())`
    /// (`socket.rs`). **That fallback arm is reachable** — the direct routes
    /// seed `configured_peer` from a best-effort `peer_addr().ok()` at accept,
    /// an `Option` precisely because that lookup can fail — it is pinned by its
    /// own tests, and nothing here licenses deleting it.
    ///
    /// The snapshot is at least as good on every path, which is the actual
    /// argument. It is taken while the connection is established, so wherever
    /// the fallback would have answered it answers here too; it then survives
    /// the peer's reset, where a later live lookup returns `ENOTCONN` and
    /// renders `peer=None` on exactly the error lines an operator reads during
    /// an incident. The divergence is one-directional — `Some` where `None`
    /// used to appear, never a different address.
    ///
    /// This is `ConnectionH2::peer_address`' shape, so an operator grepping a
    /// single session ULID reads the same `peer=` on the `MUX-H1` and `MUX-H2`
    /// lines of one connection.
    pub(super) peer_address: Option<std::net::SocketAddr>,
    /// Active stream index, or `None` when the connection has no assigned stream
    /// (initial client state before `start_stream`, or after `end_stream` detaches).
    pub stream: Option<GlobalStreamId>,
    /// Configured idle timeout for this connection. The core never arms a
    /// wheel entry itself: it publishes the next instant it wants to be called
    /// back at through `ConnectionH1::poll_timeout`, and the embedder — the
    /// `Mux` adapter — owns the `TimeoutContainer` that reflects it onto the
    /// real timer. See `LIFECYCLE.md` §7.7.
    pub timeout_duration: Duration,
    /// Next instant this connection wants `timeout()` called at, or `None`
    /// when it wants no timer at all.
    pub(super) timeout_deadline: Option<Instant>,
    /// Set when `readable` exits early because the kawa buffer was full.
    /// Edge-triggered epoll will not re-fire READABLE for data already in the
    /// kernel socket buffer, so the cross-readiness mechanism must re-arm it
    /// via `try_resume_reading` once the peer drains the buffer.
    pub parked_on_buffer_pressure: bool,
    /// True once we've asked rustls to emit TLS close_notify for this frontend.
    pub close_notify_sent: bool,
    /// Connection/session ULID propagated from the parent [`super::Mux`]. Used to
    /// stamp the session slot of the `[session req cluster backend]` log
    /// prefix emitted by the local `log_context!` macro.
    pub session_ulid: Ulid,
    /// True once this backend connection has served a stream that it picked
    /// up out of the H1 keep-alive pool, rather than on the fresh dial that
    /// created it.
    ///
    /// This is pingora's `client_reused` bit — the provenance the protocol
    /// layer cannot see but the retry decision needs.
    ///
    /// It does NOT prove what the peer did. An EOF with no response is
    /// indistinguishable at this layer from a stale pool socket, from an
    /// origin that half-closed after processing the request but before
    /// answering, and from one that crashed mid-request: `readable` treats
    /// every `size == 0` alike, and an empty back buffer proves only that no
    /// response byte arrived. What this bit does is bound the window in
    /// which sozu is willing to re-issue: a connection that came out of the
    /// pool has been idle long enough for its peer to have closed it
    /// unobserved, a freshly dialled one has not. What makes re-issuing
    /// permissible at all is the idempotence gate (RFC 9110 §9.2.2) in
    /// `Stream::can_replay_on_fresh_upstream`; this bit only narrows when
    /// that permission is exercised, so only a reused connection arms
    /// request capture (sozu-proxy/sozu#1442). Always false on a
    /// `Position::Server` connection.
    pub reused_from_pool: bool,
}

impl<Front: SocketHandler> std::fmt::Debug for ConnectionH1<Front> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionH1")
            .field("position", &self.position)
            .field("readiness", &self.readiness)
            .field("peer_address", &self.peer_address)
            .field("stream", &self.stream)
            .field("reused_from_pool", &self.reused_from_pool)
            .finish()
    }
}

impl<Front: SocketHandler> ConnectionH1<Front> {
    /// The next instant this connection wants its embedder to call `timeout()`
    /// at, or `None` for "no timer". The adapter reflects this onto the real
    /// wheel; nothing here touches `crate::timer`.
    pub(super) fn poll_timeout(&self) -> Option<Instant> {
        self.timeout_deadline
    }

    /// Push the deadline one full [`Self::timeout_duration`] out from `now`.
    /// Replaces the old `TimeoutContainer::reset()` at the same call sites.
    pub(super) fn arm_timeout(&mut self, now: Instant) {
        self.timeout_deadline = now.checked_add(self.timeout_duration);
    }

    /// Ask for no timer at all. Replaces `TimeoutContainer::cancel()`.
    pub(super) fn clear_timeout(&mut self) {
        self.timeout_deadline = None;
    }

    /// Adopt a new configured duration and re-arm from `now`.
    ///
    /// Mirrors `TimeoutContainer::set_duration`, which likewise cancelled and
    /// re-armed rather than keeping the old deadline. It re-arms
    /// unconditionally: the old method re-armed whenever the container had ever
    /// held a token, which every live connection has.
    pub(super) fn set_timeout_duration(&mut self, duration: Duration, now: Instant) {
        self.timeout_duration = duration;
        self.arm_timeout(now);
    }

    fn defer_close_for_tls_flush(&mut self, reason: &'static str) -> MuxResult {
        if self.initiate_close_notify() {
            trace!(
                "{} H1 writable delaying close after {}: stream={:?}, close_notify_sent={}, wants_write={}, readiness={:?}",
                log_context!(self),
                reason,
                self.stream,
                self.close_notify_sent,
                self.socket.socket_wants_write(),
                self.readiness
            );
            MuxResult::Continue
        } else {
            MuxResult::CloseSession
        }
    }

    /// Terminate a close-delimited kawa body by pushing END_STREAM flags.
    /// Called when the backend closes the connection to signal end-of-body
    /// (no Content-Length, no chunked encoding).
    ///
    /// Chunked responses that TCP-close before the terminating `0\r\n\r\n`
    /// are demoted to `ParsingPhase::Error` so the H2 converter emits
    /// RST_STREAM(InternalError) rather than a silent END_STREAM with a
    /// truncated body — RFC 9112 §7.1 requires the zero-chunk terminator.
    fn terminate_close_delimited(kawa: &mut super::GenericHttpStream, stream_id: GlobalStreamId) {
        // Pre: we only synthesize an end-of-body for a response still in its
        // body phase. A kawa already Terminated/Error must not be re-terminated
        // (it would double-push an END_STREAM flag onto the converter).
        debug_assert!(
            !kawa.is_terminated(),
            "terminate_close_delimited must not run on an already-terminated kawa"
        );
        if kawa.body_size == kawa::BodySize::Chunked {
            warn!(
                "{} H1 backend EOF mid-chunked response on stream {}: emitting RST_STREAM",
                log_module_context!(),
                stream_id
            );
            incr!(names::h1::BACKEND_EOF_BEFORE_MESSAGE_COMPLETE);
            kawa.parsing_phase
                .error(kawa::ParsingErrorKind::Processing {
                    message: "INTERNAL_ERROR",
                });
            // Post: a truncated chunked body is demoted to Error so the
            // converter emits RST_STREAM, never a silent END_STREAM.
            debug_assert!(
                kawa.is_error(),
                "truncated chunked response must end in the Error phase"
            );
            return;
        }
        debug!(
            "{} H1 close-delimited EOF on stream {}: terminating body",
            log_module_context!(),
            stream_id
        );
        kawa.push_block(kawa::Block::Flags(kawa::Flags {
            end_body: true,
            end_chunk: false,
            end_header: false,
            end_stream: true,
        }));
        kawa.parsing_phase = kawa::ParsingPhase::Terminated;
        // Post: a close-delimited body is now Terminated so the converter
        // emits DATA with END_STREAM on the last frame.
        debug_assert!(
            kawa.is_terminated(),
            "close-delimited body must end in the Terminated phase"
        );
    }

    pub fn readable<E, L>(&mut self, context: &mut Context<L>, mut endpoint: E) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        trace!(
            "{} ======= MUX H1 READABLE {:?}",
            log_context!(self),
            self.position
        );
        let Some(stream_id) = self.stream else {
            error!(
                "{} readable() called on H1 connection with no active stream",
                log_context!(self)
            );
            return MuxResult::Continue;
        };
        self.arm_timeout(context.now);
        let stream = &mut context.streams[stream_id];
        // The answer registry this request captured when it arrived, not the
        // one a reload may have installed since.
        let answers_rc = stream.answers.clone();
        if stream.metrics.start.is_none() {
            stream.metrics.mark_request_start();
        }
        let parts = stream.split(&self.position);
        let kawa = parts.rbuffer;

        // If the buffer has no space, don't attempt a read — socket_read with
        // an empty buffer returns (0, Continue) which is indistinguishable from
        // a real EOF. Remove READABLE from the event so the inner loop doesn't
        // spin; `try_resume_reading` will re-arm it once the peer drains the
        // buffer (edge-triggered epoll won't re-fire for data already in the
        // kernel socket buffer).
        if kawa.storage.available_space() == 0 {
            self.readiness.event.remove(Ready::READABLE);
            self.parked_on_buffer_pressure = true;
            // Pair the park flag with the cleared READABLE event: only
            // `try_resume_reading` may re-arm it once the peer drains space.
            debug_assert!(
                self.parked_on_buffer_pressure && !self.readiness.event.is_readable(),
                "parking on buffer pressure must clear the READABLE event"
            );
            return MuxResult::Continue;
        }

        self.parked_on_buffer_pressure = false;
        // Pre: we never ask the socket to read into a full buffer (that path
        // returned above) — `socket_read` requires a non-empty target slice.
        let space_before = kawa.storage.available_space();
        debug_assert!(
            space_before > 0,
            "socket_read must target a buffer with free space"
        );
        let (size, status) = self.socket.socket_read(kawa.storage.space());
        // A read cannot deliver more bytes than the buffer had room for.
        // `debug_assert!` only — `size` is socket-derived, never trusted to
        // panic, but a violation here is a SocketHandler contract bug.
        debug_assert!(
            size <= space_before,
            "socket_read returned more bytes than the buffer could hold"
        );
        context.debug.push(DebugEvent::StreamEvent(0, size));
        if size > 0 && self.position.is_client() {
            // The upstream answered, so this attempt is past the replay
            // boundary (`Stream::can_replay_on_fresh_upstream`) whatever the
            // bytes turn out to parse as. Drop the captured request now
            // instead of carrying it to the end of the response.
            *parts.retry_buffer = None;
        }
        kawa.storage.fill(size);
        debug_assert_eq!(
            kawa.storage.available_space(),
            space_before - size,
            "fill must consume exactly `size` bytes of free space"
        );
        crate::protocol::mux::h2::record_metric(self.position.bytes_in_event(size));
        self.position.count_bytes_in(parts.metrics, size);
        if update_readiness_after_read(size, status, &mut self.readiness) {
            // size=0: the socket returned EOF (Closed) or WouldBlock.
            // For a close-delimited backend response (no Content-Length, no
            // chunked), a graceful EOF IS the end-of-body signal. Terminate
            // the kawa so the H2 converter emits DATA with END_STREAM.
            // SocketResult::Error (ECONNRESET etc.) is NOT treated as a valid
            // close-delimiter — transport errors should produce 502, not a
            // truncated response.
            if status == SocketResult::Closed
                && self.position.is_client()
                && kawa.is_main_phase()
                && !kawa.is_terminated()
                && !parts.context.keep_alive_backend
            {
                Self::terminate_close_delimited(kawa, stream_id);
                self.timeout_deadline = None;
                self.readiness.interest.remove(Ready::READABLE);
                if let StreamState::Linked(token) = stream.state {
                    // Signal pending write alongside the WRITABLE interest flip:
                    // edge-triggered epoll won't re-fire for bytes we just queued
                    // onto the peer — the synthetic event is the only wake path.
                    let peer = endpoint.readiness_mut(token);
                    peer.arm_writable();
                }
            }
            return MuxResult::Continue;
        }

        let was_main_phase = kawa.is_main_phase();
        kawa::h1::parse(kawa, parts.context);
        if kawa.is_error() {
            match self.position {
                Position::Client(..) => {
                    incr!(names::http::BACKEND_PARSE_ERRORS);
                    let StreamState::Linked(token) = stream.state else {
                        error!(
                            "{} client stream in error is not in Linked state",
                            log_context!(self)
                        );
                        return MuxResult::CloseSession;
                    };
                    let global_stream_id = stream_id;
                    self.end_stream(global_stream_id, context);
                    endpoint.end_stream(token, global_stream_id, context);
                }
                Position::Server => {
                    incr!(names::http::FRONTEND_PARSE_ERRORS);
                    let answers = answers_rc.borrow();
                    set_default_answer(stream, &mut self.readiness, 400, &answers);
                }
            }
            return MuxResult::Continue;
        }
        // Capture borrow-sensitive values after parsing but before the 1xx block
        // accesses stream.state (which ends the split borrow from `parts`).
        let is_keep_alive_backend = parts.context.keep_alive_backend;
        let is_body_phase_after_parse = kawa.is_main_phase();

        // 1xx informational responses (100 Continue, 103 Early Hints): the H1
        // parser treats them as complete (Terminated + end_stream=true), but for
        // H2 frontends they must be forwarded WITHOUT END_STREAM so the real
        // response can follow on the same stream. Also keep READABLE interest
        // so the backend can send the final response.
        let is_1xx_backend = if self.position.is_client() {
            if let kawa::StatusLine::Response { code, .. } = &kawa.detached.status_line {
                if (100..200).contains(code) {
                    debug!(
                        "{} H1 backend: received {} informational response",
                        log_context!(self),
                        code
                    );
                    for block in &mut kawa.blocks {
                        if let kawa::Block::Flags(flags) = block {
                            flags.end_stream = false;
                            flags.end_body = false;
                        }
                    }
                    true
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        };
        if kawa.is_terminated() && !is_1xx_backend {
            self.timeout_deadline = None;
            self.readiness.interest.remove(Ready::READABLE);
        }
        if kawa.is_main_phase() {
            if !was_main_phase && self.position.is_client() {
                // The backend's response headers have just finished parsing:
                // the client twin of the `is_server()` branch below, on the
                // same header -> body edge. This is the whole of
                // `backend_header_time`'s H1 arming — nginx's
                // `$upstream_header_time` (sozu-proxy/sozu#426).
                //
                // Unconditional, including on a 1xx: a 100-Continue or a 103
                // Early Hints clears the back buffer in
                // `ConnectionH1::writable`, so the FINAL response re-enters
                // this edge and overwrites, which is the response
                // `SessionMetrics::backend_stop` also anchors on. A 101 has no
                // successor and correctly keeps its own instant.
                parts.metrics.backend_headers_received();
            }
            if !was_main_phase && self.position.is_server() {
                if parts.context.method.is_none()
                    || parts.context.authority.is_none()
                    || parts.context.path.is_none()
                {
                    if let kawa::StatusLine::Request {
                        version: kawa::Version::V10,
                        ..
                    } = kawa.detached.status_line
                    {
                        error!(
                            "{} Unexpected malformed request: HTTP/1.0 from {:?} with {:?} {:?} {:?}",
                            log_context!(self),
                            parts.context.session_address,
                            parts.context.method,
                            parts.context.authority,
                            parts.context.path
                        );
                    } else {
                        error!("{} Unexpected malformed request", log_context!(self));
                        kawa::debug_kawa(kawa);
                    }
                    let answers = answers_rc.borrow();
                    set_default_answer(stream, &mut self.readiness, 400, &answers);
                    return MuxResult::Continue;
                }
                // First-seen request on this (server) connection: the keep-alive
                // request counter advances by exactly one and the matching
                // `http.active_requests` gauge `+1` is paired with flipping
                // `request_counted` true (the `generate_access_log` `-1` is
                // gated on that flag, so they must stay balanced).
                let requests_before = self.requests;
                let links_before = context.pending_links.len();
                self.requests += 1;
                debug_assert_eq!(
                    self.requests,
                    requests_before + 1,
                    "server keep-alive request counter must advance by exactly one"
                );
                trace!("{} REQUESTS: {}", log_context!(self), self.requests);
                incr!(names::http::REQUESTS);
                gauge_add!(names::http::ACTIVE_REQUESTS, 1);
                parts.metrics.service_start();
                // Set request_counted after the last use of `parts` to satisfy the borrow checker
                stream.request_counted = true;
                stream.state = StreamState::Link;
                context.pending_links.push_back(stream_id);
                // Post: the stream is queued for backend linking exactly once
                // and is now in the Link state the ready loop expects.
                debug_assert!(
                    stream.request_counted,
                    "request_counted must be set when the active-requests gauge is incremented"
                );
                debug_assert_eq!(
                    stream.state,
                    StreamState::Link,
                    "a first-seen request must transition the stream to Link"
                );
                debug_assert_eq!(
                    context.pending_links.len(),
                    links_before + 1,
                    "a first-seen request must enqueue exactly one pending link"
                );
            }
            if let StreamState::Linked(token) = stream.state {
                // Signal pending write alongside the WRITABLE interest flip: the
                // bytes we just parsed live in sozu's buffers, not the kernel,
                // so edge-triggered epoll won't re-fire on its own.
                let peer = endpoint.readiness_mut(token);
                peer.arm_writable();
            }
        };
        // 1xx informational: kawa sets `detached.status_line` as soon as the
        // status line parses and only then moves to `ParsingPhase::Headers`,
        // so a read that stops inside the 1xx headers leaves
        // `is_main_phase()` false while `is_1xx_backend` is already true. On
        // that pass only this arm wakes the frontend, early, before the 1xx is
        // complete; the pass that completes the headers re-arms it through the
        // `Linked` arm above either way. When the whole 1xx arrives in one
        // read it parses to `Terminated`, which kawa counts as a main phase, so
        // the `Linked` arm above has already fired and this call is redundant
        // (`arm_writable` is idempotent).
        if is_1xx_backend && let StreamState::Linked(token) = stream.state {
            let peer = endpoint.readiness_mut(token);
            peer.arm_writable();
        }

        // Close-delimited response: socket_read returned (size > 0, Closed) —
        // the last data chunk arrived together with the EOF in a single read.
        // After parsing the data above, terminate the kawa now so the H2
        // converter emits END_STREAM on the last DATA frame.
        if status == SocketResult::Closed
            && self.position.is_client()
            && is_body_phase_after_parse
            && !is_keep_alive_backend
            && !context.streams[stream_id].back.is_terminated()
        {
            let kawa = &mut context.streams[stream_id].back;
            Self::terminate_close_delimited(kawa, stream_id);
            self.timeout_deadline = None;
            self.readiness.interest.remove(Ready::READABLE);
        }

        MuxResult::Continue
    }

    pub fn writable<E, L>(&mut self, context: &mut Context<L>, mut endpoint: E) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        trace!(
            "{} ======= MUX H1 WRITABLE {:?}",
            log_context!(self),
            self.position
        );
        let Some(stream_id) = self.stream else {
            if self.socket.socket_wants_write() {
                let (size, status) = self.socket.socket_write_vectored(&[]);
                let _ = update_readiness_after_write(size, status, &mut self.readiness);
                if self.socket.socket_wants_write() {
                    self.readiness.signal_pending_write();
                }
            }
            return MuxResult::Continue;
        };
        // One clock read for the whole pass: the two later re-arms below
        // (100-Continue, keep-alive recycle) run while `context` is mutably
        // reborrowed for the stream, so they cannot reach `context.now`.
        let now = context.now;
        self.arm_timeout(now);
        let stream = &mut context.streams[stream_id];
        let parts = stream.split(&self.position);
        let kawa = parts.wbuffer;
        // Apply per-frontend response-side header edits stashed by the
        // routing layer at request time. Only the Server-position pass
        // touches the response back-kawa; the Client-position pass
        // (writing the request to the backend) has already had its
        // edits applied in `Router::route_from_request`.
        //
        // Drained via `mem::take` so the injection runs exactly once
        // per response. H1 keep-alive can re-enter this writable path
        // for the same stream when the backend response spans more
        // than one TCP read; without the take, the second pass would
        // re-insert the same headers (typically as duplicate STS lines
        // on the wire — RFC 6797 §6.1 expects a single header). On H2
        // the same multi-prepare-cycle pattern surfaces as a
        // `H2BlockConverter::finalize` "out buffer not empty" leak.
        if matches!(self.position, Position::Server) && !parts.context.headers_response.is_empty() {
            let edits = std::mem::take(&mut parts.context.headers_response);
            super::shared::apply_response_header_edits(kawa, &edits);
        }
        kawa.prepare(&mut kawa::h1::BlockConverter);
        let mut io_slices = Vec::new();
        for block in kawa.out.iter() {
            match block {
                kawa::OutBlock::Delimiter => break,
                kawa::OutBlock::Store(store) => {
                    io_slices.push(IoSlice::new(store.data(kawa.storage.buffer())));
                }
            }
        }
        let can_finalize_server_close = matches!(self.position, Position::Server)
            && kawa.is_terminated()
            && kawa.is_completed();
        if io_slices.is_empty() && !self.socket.socket_wants_write() && !can_finalize_server_close {
            self.readiness.interest.remove(Ready::WRITABLE);
            return MuxResult::Continue;
        }
        let tls_only_flush = io_slices.is_empty();
        // Total bytes we offered the socket across the gathered slices; a
        // vectored write can never report more consumed than we handed it.
        let queued: usize = io_slices.iter().map(|s| s.len()).sum();
        let (size, status) = self.socket.socket_write_vectored(&io_slices);
        debug_assert!(
            size <= queued,
            "socket_write_vectored reported more bytes written than were queued"
        );
        // Capture the bytes the socket accepted BEFORE `kawa::Kawa::consume`
        // drops them from the front buffer and shifts the storage: past that
        // point sozu no longer holds the request, so a stale pooled upstream
        // could not be retried without this copy. Every proxy that retries a
        // request already written upstream pays the same copy — HAProxy
        // ("requires to allocate a buffer and copy the whole request into
        // it, so it has memory and performance impacts"), pingora
        // (`enable_retry_buffering`, a fixed 64 KiB `FixedBuffer`).
        //
        // It is charged ONLY to a connection that came out of the keep-alive
        // pool: `reused_from_pool` is set by `start_stream` on the
        // KeepAlive -> Connected transition and nowhere else, so a freshly
        // dialled upstream and every `Position::Server` write cost nothing.
        // That is pingora's `RetryType::ReusedOnly` boundary, argued in
        // `Stream::can_replay_on_fresh_upstream` (sozu-proxy/sozu#1442).
        //
        // Overflowing one front buffer truncates the capture to `None` and
        // the request stops being replayable, rather than the buffer
        // growing: HAProxy, same reason — "Requests not fitting in a single
        // buffer will never be retried".
        //
        // The bytes are appended straight onto the capture, with the budget
        // decided first: `size` is already known, `kawa` was moved out of
        // `parts` above so `parts.retry_buffer` is independently borrowable,
        // and the slices still alias `kawa.storage` until `consume` below.
        // Staging them through a temporary `Vec` first would cost one extra
        // allocation, one extra copy and one free on every write of every
        // pooled connection.
        let retry_budget = kawa.storage.capacity();
        if self.reused_from_pool {
            match parts.retry_buffer.as_mut() {
                // The capture has to stay a byte-exact prefix of what the
                // socket accepted, so a write that would carry it past one
                // front buffer drops it whole rather than truncating it.
                Some(buffer) if size <= retry_budget.saturating_sub(buffer.len()) => {
                    buffer.reserve(size);
                    let mut remaining = size;
                    for slice in &io_slices {
                        if remaining == 0 {
                            break;
                        }
                        let take = remaining.min(slice.len());
                        buffer.extend_from_slice(&slice[..take]);
                        remaining -= take;
                    }
                    debug_assert_eq!(
                        remaining, 0,
                        "the replay capture must mirror every byte the socket accepted"
                    );
                }
                _ => *parts.retry_buffer = None,
            }
        }
        context.debug.push(DebugEvent::StreamEvent(1, size));
        kawa.consume(size);
        crate::protocol::mux::h2::record_metric(self.position.bytes_out_event(size));
        self.position.count_bytes_out(parts.metrics, size);
        let should_yield = update_readiness_after_write(size, status, &mut self.readiness);
        if self.socket.socket_wants_write() {
            self.readiness.signal_pending_write();
            // Pair the queued-write signal with the socket's own report: we
            // only synthesize a WRITABLE event when the socket still has bytes
            // buffered (edge-triggered epoll won't re-fire on its own).
            debug_assert!(
                self.readiness.event.is_writable(),
                "signal_pending_write must leave a WRITABLE event queued"
            );
            return MuxResult::Continue;
        }
        if !tls_only_flush && should_yield {
            return MuxResult::Continue;
        }

        if kawa.is_terminated() && kawa.is_completed() {
            match self.position {
                Position::Client(..) => self.readiness.interest.insert(Ready::READABLE),
                Position::Server => {
                    if stream.context.closing {
                        return self.defer_close_for_tls_flush("closing-context");
                    }
                    let kawa = &mut stream.back;
                    match kawa.detached.status_line {
                        kawa::StatusLine::Response { code: 101, .. } => {
                            debug!("{} ============== HANDLE UPGRADE!", log_context!(self));
                            stream.metrics.backend_stop();
                            let client_rtt = socket_rtt(self.socket.socket_ref());
                            let server_rtt =
                                stream.linked_token().and_then(|t| endpoint.peer_rtt(t));
                            for event in stream
                                .generate_access_log(
                                    false,
                                    Some("H1::Upgrade"),
                                    context.listener.clone(),
                                    client_rtt,
                                    server_rtt,
                                )
                                .into_iter()
                                .flatten()
                            {
                                crate::protocol::mux::h2::record_metric(event);
                            }
                            return MuxResult::Upgrade;
                        }
                        kawa::StatusLine::Response { code: 100, .. } => {
                            debug!("{} ============== HANDLE CONTINUE!", log_context!(self));
                            // After a 100 Continue, we expect the client to continue
                            // with its request body. Do NOT call generate_access_log
                            // here — the final response will emit the access log.
                            // Calling it here would double-decrement http.active_requests.
                            self.timeout_deadline = now.checked_add(self.timeout_duration);
                            self.readiness.interest.insert(Ready::READABLE);
                            kawa.clear();
                            stream.metrics.backend_stop();
                            if let StreamState::Linked(token) = stream.state {
                                endpoint
                                    .readiness_mut(token)
                                    .interest
                                    .insert(Ready::READABLE);
                            }
                            return MuxResult::Continue;
                        }
                        kawa::StatusLine::Response { code: 103, .. } => {
                            debug!("{} ============== HANDLE EARLY HINT!", log_context!(self));
                            // Do NOT call generate_access_log for 103 Early Hints.
                            // The final response will emit the access log.
                            // Calling it here would double-decrement http.active_requests.
                            if let StreamState::Linked(token) = stream.state {
                                // after a 103 early hints, we expect the backend to send its response
                                endpoint
                                    .readiness_mut(token)
                                    .interest
                                    .insert(Ready::READABLE);
                                kawa.clear();
                                stream.metrics.backend_stop();
                                return MuxResult::Continue;
                            } else {
                                stream.metrics.backend_stop();
                                let client_rtt = socket_rtt(self.socket.socket_ref());
                                let server_rtt =
                                    stream.linked_token().and_then(|t| endpoint.peer_rtt(t));
                                for event in stream
                                    .generate_access_log(
                                        false,
                                        Some("H1::EarlyHint"),
                                        context.listener.clone(),
                                        client_rtt,
                                        server_rtt,
                                    )
                                    .into_iter()
                                    .flatten()
                                {
                                    crate::protocol::mux::h2::record_metric(event);
                                }
                                return self.defer_close_for_tls_flush("early-hint");
                            }
                        }
                        _ => {}
                    }
                    incr!(names::http::E2E_HTTP11);
                    stream.metrics.backend_stop();
                    let client_rtt = socket_rtt(self.socket.socket_ref());
                    let server_rtt = stream.linked_token().and_then(|t| endpoint.peer_rtt(t));
                    for event in stream
                        .generate_access_log(
                            false,
                            Some("H1::Complete"),
                            context.listener.clone(),
                            client_rtt,
                            server_rtt,
                        )
                        .into_iter()
                        .flatten()
                    {
                        crate::protocol::mux::h2::record_metric(event);
                    }
                    stream.metrics.reset();
                    let old_state = std::mem::replace(&mut stream.state, StreamState::Unlinked);
                    if let StreamState::Linked(token) = old_state {
                        remove_backend_stream(&mut context.backend_streams, token, stream_id);
                    }
                    if stream.context.keep_alive_frontend {
                        self.timeout_deadline = now.checked_add(self.timeout_duration);
                        if let StreamState::Linked(token) = old_state {
                            endpoint.end_stream(token, stream_id, context);
                        }
                        self.readiness.interest.insert(Ready::READABLE);
                        let stream = &mut context.streams[stream_id];
                        stream.context.reset();
                        stream.back.clear();
                        stream.back.storage.clear();
                        stream.front.clear();
                        // do not stream.front.storage.clear() because of H1 pipelining
                        stream.attempts = 0;
                        // The next pipelined request gets its own replay
                        // decision: `start_stream` re-arms capture only if it
                        // again picks a connection out of the keep-alive pool.
                        stream.forget_upstream_replay();
                        // Transition back to Idle so buffered pipelined requests
                        // trigger a phase transition on the next readable() call.
                        stream.state = StreamState::Idle;
                        // Post: the keep-alive reset leaves a clean, closed slot
                        // ready for the next pipelined request. `request_counted`
                        // was already cleared by `generate_access_log` above, so
                        // the slot carries no pending active-requests charge.
                        debug_assert_eq!(
                            stream.state,
                            StreamState::Idle,
                            "keep-alive reset must return the stream to Idle"
                        );
                        debug_assert_eq!(stream.attempts, 0, "keep-alive reset must zero attempts");
                        debug_assert!(
                            !stream.request_counted,
                            "keep-alive reset must leave no counted request (active-requests leak)"
                        );
                        debug_assert!(
                            stream.back.storage.is_empty(),
                            "keep-alive reset must drain the response storage"
                        );
                        // HTTP/1.1 pipelining: if there's still data in the frontend
                        // storage (pipelined requests already read from the socket),
                        // parse it now. We can't rely on a new READABLE event because
                        // the socket buffer may be empty — all requests were already
                        // read into kawa storage in the first socket_read.
                        if !stream.front.storage.is_empty() {
                            kawa::h1::parse(&mut stream.front, &mut stream.context);
                            let is_error = stream.front.is_error();
                            let is_main = stream.front.is_main_phase();
                            let malformed = is_main
                                && (stream.context.method.is_none()
                                    || stream.context.authority.is_none()
                                    || stream.context.path.is_none());
                            if is_error || malformed {
                                let answers_rc = stream.answers.clone();
                                let answers = answers_rc.borrow();
                                set_default_answer(stream, &mut self.readiness, 400, &answers);
                            } else if is_main {
                                self.requests += 1;
                                incr!(names::http::REQUESTS);
                                gauge_add!(names::http::ACTIVE_REQUESTS, 1);
                                stream.metrics.service_start();
                                stream.request_counted = true;
                                stream.state = StreamState::Link;
                                context.pending_links.push_back(stream_id);
                            }
                            // else: incomplete parse, wait for more data via READABLE
                        }
                    } else {
                        return self.defer_close_for_tls_flush("response-complete");
                    }
                }
            }
        }
        MuxResult::Continue
    }

    pub fn force_disconnect(&mut self) -> MuxResult {
        match &mut self.position {
            Position::Client(_, _, status) => {
                *status = BackendStatus::Disconnecting;
                self.readiness.event = Ready::HUP;
                debug!(
                    "{} H1 force_disconnect client: stream={:?}, wants_write={}, readiness={:?}",
                    log_context!(self),
                    self.stream,
                    self.socket.socket_wants_write(),
                    self.readiness
                );
                MuxResult::Continue
            }
            Position::Server => {
                if self.socket.socket_wants_write() {
                    debug!(
                        "{} H1 force_disconnect delaying close: stream={:?}, wants_write=true, readiness={:?}",
                        log_context!(self),
                        self.stream,
                        self.readiness
                    );
                    self.readiness.interest = Ready::WRITABLE | Ready::HUP | Ready::ERROR;
                    self.readiness.signal_pending_write();
                    MuxResult::Continue
                } else {
                    debug!(
                        "{} H1 force_disconnect closing session: stream={:?}, wants_write=false, readiness={:?}",
                        log_context!(self),
                        self.stream,
                        self.readiness
                    );
                    MuxResult::CloseSession
                }
            }
        }
    }

    pub fn has_pending_write(&self) -> bool {
        self.socket.socket_wants_write()
    }

    pub fn initiate_close_notify(&mut self) -> bool {
        if !self.position.is_server() {
            return false;
        }
        // Past the guard we are always server-side; close_notify is a
        // frontend-only TLS concern.
        debug_assert!(
            self.position.is_server(),
            "initiate_close_notify past the guard must be server-side"
        );
        if !self.close_notify_sent {
            trace!("{} H1 initiating CLOSE_NOTIFY", log_context!(self));
            self.socket.socket_close();
            self.close_notify_sent = true;
        }
        // `close_notify` is monotone: once requested it stays sent for the
        // connection's lifetime (a second send would corrupt the TLS stream).
        debug_assert!(
            self.close_notify_sent,
            "close_notify_sent must be set once initiate_close_notify has run"
        );
        if self.socket.socket_wants_write() {
            self.readiness.arm_writable();
            // arm_writable pairs interest + event so the deferred TLS flush is
            // actually scheduled under edge-triggered epoll.
            debug_assert!(
                self.readiness.interest.is_writable() && self.readiness.event.is_writable(),
                "arm_writable must set both WRITABLE interest and event"
            );
            true
        } else {
            false
        }
    }

    pub fn close<E, L>(&mut self, context: &mut Context<L>, mut endpoint: E)
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        match self.position {
            Position::Client(_, _, BackendStatus::KeepAlive)
            | Position::Client(_, _, BackendStatus::Disconnecting) => {
                trace!("{} close detached client ConnectionH1", log_context!(self));
                return;
            }
            Position::Client(_, _, BackendStatus::Connecting(_))
            | Position::Client(_, _, BackendStatus::Connected) => {
                debug!(
                    "{} BACKEND CLOSING FOR: {:?} {:?}",
                    log_context!(self),
                    self.position,
                    self.stream
                );
            }
            Position::Server => {
                let tls_pending_before = self.socket.socket_wants_write();
                let (tls_pending_after, drain_rounds) =
                    drain_tls_close_notify(&mut self.socket, &mut self.close_notify_sent);
                if tls_pending_after {
                    error!(
                        "{} H1 TLS buffer NOT fully drained on close: pending_before={}, pending_after={}, drain_rounds={}, stream={:?}, close_notify_sent={}, readiness={:?}",
                        log_context!(self),
                        tls_pending_before,
                        tls_pending_after,
                        drain_rounds,
                        self.stream,
                        self.close_notify_sent,
                        self.readiness
                    );
                }
                return;
            }
        }
        let Some(stream_id) = self.stream else {
            trace!(
                "{} closing detached H1 client with no active stream",
                log_context!(self)
            );
            return;
        };
        // reconnection is handled by the server
        let StreamState::Linked(token) = context.streams[stream_id].state else {
            trace!(
                "{} closing detached H1 client in state {:?} on stream {}",
                log_context!(self),
                context.streams[stream_id].state,
                stream_id
            );
            return;
        };
        endpoint.end_stream(token, stream_id, context)
    }

    pub fn end_stream<L>(&mut self, stream: GlobalStreamId, context: &mut Context<L>)
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        if self.stream != Some(stream) {
            error!(
                "{} end_stream called with stream {} but expected {:?}",
                log_context!(self),
                stream,
                self.stream
            );
            return;
        }
        // Reached only on the matched-stream path: the connection's active
        // stream is exactly the one being ended.
        debug_assert_eq!(
            self.stream,
            Some(stream),
            "end_stream past the guard must target the active stream"
        );
        context.unlink_stream(stream);
        // Post: whatever backend token this stream was Linked to no longer
        // lists it in the reverse index. `unlink_stream` is idempotent — it
        // evicts only while the stream is still `Linked` — so a subsequent
        // end/close cannot double-remove it. It is NOT the module's only
        // eviction point: `remove_backend_stream` has direct callers here and
        // in `h2.rs`. Extra eviction is harmless; what this post-condition
        // claims is that THIS path evicted.
        // (The `state` field is still `Linked` here; the arms below retire it.)
        #[cfg(debug_assertions)]
        if let StreamState::Linked(token) = context.streams[stream].state {
            debug_assert!(
                context
                    .backend_streams
                    .get(&token)
                    .is_none_or(|ids| !ids.contains(&stream)),
                "unlink_stream must evict the stream from the backend reverse index"
            );
        }
        let stream_id = stream;
        let stream = &mut context.streams[stream_id];
        let answers_rc = stream.answers.clone();
        let stream_context = &mut stream.context;
        trace!(
            "{} end H1 stream {:?}: {:#?}",
            log_context!(self),
            self.stream,
            stream_context
        );
        match &mut self.position {
            Position::Client(_, _, BackendStatus::Connecting(_)) => {
                self.stream = None;
                if stream.state != StreamState::Recycle {
                    stream.state = StreamState::Unlinked;
                }
                // Post: the client connection detaches from its stream and the
                // slot is retired (Unlinked) unless already marked for reuse —
                // never left dangling in an open/Linked state.
                debug_assert!(
                    self.stream.is_none(),
                    "client end_stream must detach the stream"
                );
                debug_assert!(
                    !matches!(stream.state, StreamState::Linked(_)),
                    "detached stream must not remain Linked"
                );
                self.readiness.interest.remove(Ready::ALL);
                self.force_disconnect();
            }
            Position::Client(_, _, status @ BackendStatus::Connected) => {
                self.stream = None;
                if stream.state != StreamState::Recycle {
                    stream.state = StreamState::Unlinked;
                }
                debug_assert!(
                    self.stream.is_none(),
                    "client end_stream must detach the stream"
                );
                debug_assert!(
                    !matches!(stream.state, StreamState::Linked(_)),
                    "detached stream must not remain Linked"
                );
                self.readiness.interest.remove(Ready::ALL);
                // keep alive should probably be used only if the http context is fully reset
                // in case end_stream occurs due to an error the connection state is probably
                // unrecoverable and should be terminated
                if stream_context.keep_alive_backend && stream.back.is_terminated() {
                    *status = BackendStatus::KeepAlive;
                } else {
                    self.force_disconnect();
                }
            }
            Position::Client(_, _, BackendStatus::KeepAlive)
            | Position::Client(_, _, BackendStatus::Disconnecting) => {
                error!(
                    "{} end_stream called on KeepAlive or Disconnecting H1 client",
                    log_context!(self)
                );
            }
            Position::Server => match end_stream_decision(stream) {
                EndStreamAction::ForwardTerminated => {
                    debug!("{} CLOSING H1 TERMINATED STREAM", log_context!(self));
                    stream.state = StreamState::Unlinked;
                    self.readiness.interest.insert(Ready::WRITABLE);
                    // End-of-stream was already queued into kawa by the parser;
                    // no fresh WRITABLE event will arrive from the kernel.
                    self.readiness.signal_pending_write();
                }
                EndStreamAction::CloseDelimited => {
                    debug!("{} CLOSE DELIMITED", log_context!(self));
                    stream.state = StreamState::Unlinked;
                    self.readiness.arm_writable();
                }
                EndStreamAction::ForwardUnterminated => {
                    debug!("{} CLOSING H1 UNTERMINATED STREAM", log_context!(self));
                    forcefully_terminate_answer(
                        stream,
                        &mut self.readiness,
                        H2Error::InternalError,
                    );
                }
                EndStreamAction::SendDefault(status) => {
                    let answers = answers_rc.borrow();
                    set_default_answer(stream, &mut self.readiness, status, &answers);
                }
                EndStreamAction::Reconnect => {
                    debug!("{} H1 RECONNECT", log_context!(self));
                    stream.state = StreamState::Link;
                    context.pending_links.push_back(stream_id);
                }
                EndStreamAction::ReplayOnFreshBackend => match stream.queue_upstream_replay() {
                    Some(len) => {
                        debug!(
                            "{} H1 REPLAY {} request bytes on a fresh backend",
                            log_context!(self),
                            len
                        );
                        incr!(
                            names::backend::RETRY_STALE_UPSTREAM,
                            stream.context.cluster_id.as_deref(),
                            stream.context.backend_id.as_deref()
                        );
                        stream.state = StreamState::Link;
                        context.pending_links.push_back(stream_id);
                        // `Router::plan_connect` still gates on `stream.attempts`
                        // against `CONN_RETRIES`, so a cluster whose backends
                        // are all stale cannot loop: the budget runs out and
                        // the caller answers 503.
                    }
                    None => {
                        // Unreachable while `end_stream_decision` only picks
                        // this action behind `can_replay_on_fresh_upstream`,
                        // which requires the buffer. Degrade to the answer
                        // the old code sent rather than stranding the stream.
                        error!(
                            "{} replay selected with no captured request",
                            log_context!(self)
                        );
                        let answers = answers_rc.borrow();
                        set_default_answer(stream, &mut self.readiness, 502, &answers);
                    }
                },
            },
        }
    }

    pub fn start_stream<L>(&mut self, stream: GlobalStreamId, context: &mut Context<L>) -> bool
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        trace!(
            "{} start H1 stream {} {:?}",
            log_context!(self),
            stream,
            self.readiness
        );
        self.readiness.interest.insert(Ready::ALL);
        self.stream = Some(stream);
        debug_assert_eq!(
            self.stream,
            Some(stream),
            "start_stream must pin the connection's active stream"
        );
        match &mut self.position {
            Position::Client(_, _, status @ BackendStatus::KeepAlive) => {
                *status = BackendStatus::Connected;
                // This stream is being written onto a socket that has been
                // idle in the pool, so the peer may already have closed it
                // without sozu noticing. Record the provenance and arm
                // request capture so the write path below can keep the bytes
                // needed to replay elsewhere (sozu-proxy/sozu#1442).
                self.reused_from_pool = true;
                context.streams[stream].arm_upstream_replay();
                // A keep-alive client transitions to Connected when it picks up
                // a new stream; it must not stay parked in KeepAlive.
                debug_assert!(
                    matches!(
                        self.position,
                        Position::Client(_, _, BackendStatus::Connected)
                    ),
                    "a reused keep-alive client must become Connected on start_stream"
                );
            }
            Position::Client(_, _, BackendStatus::Disconnecting) => {
                error!(
                    "{} start_stream called on Disconnecting H1 client",
                    log_context!(self)
                );
                return false;
            }
            Position::Client(_, _, _) => {}
            Position::Server => {
                error!(
                    "{} start_stream must not be called on H1 server connection",
                    log_context!(self)
                );
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, cell::RefCell, io::Write, rc::Rc};

    use super::*;
    use crate::{
        Protocol as TransportKind,
        pool::Pool,
        protocol::{
            kawa_h1::editor::HttpContext,
            mux::{
                BackendId, BackendSlot, Connection,
                connection::{EndpointClient, EndpointServer},
                router::Router,
                test_support::{connected_socket, test_context},
            },
        },
        socket::SessionTcpStream,
    };

    // ── The `peer=` slot of the MUX-H1 log prefix ───────────────────────
    //
    // These pin that the slot is the address the connection snapshotted at
    // construction, not a live `getpeername(2)` taken per expansion. The
    // difference is invisible while a connection is healthy and is the whole
    // story once it is not: `getpeername(2)` answers ENOTCONN after the peer
    // resets, and it reports the transport peer — the load balancer — on a
    // PROXY-protocol frontend.

    /// Address the handler caches. Deliberately non-loopback so it cannot
    /// collide with whatever ephemeral port a live lookup would report.
    const CACHED_PEER: &str = "10.0.0.42:12345";

    fn cached_peer() -> std::net::SocketAddr {
        CACHED_PEER
            .parse()
            .expect("the cached peer literal must parse")
    }

    /// A live, established loopback connection plus the address
    /// `getpeername(2)` reports for it. The listener is returned so the
    /// connection stays up for the whole test: these tests assert the snapshot
    /// wins even while the live lookup is perfectly healthy, so a half-dead
    /// socket would weaken them rather than strengthen them.
    fn connected_loopback_stream() -> (
        std::net::TcpListener,
        mio::net::TcpStream,
        std::net::SocketAddr,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("test listener must bind to a loopback port");
        let live_peer = listener
            .local_addr()
            .expect("test listener must report its local address");
        let stream =
            std::net::TcpStream::connect(live_peer).expect("loopback connect must complete");
        stream
            .set_nonblocking(true)
            .expect("mio requires a nonblocking stream");
        (listener, mio::net::TcpStream::from_std(stream), live_peer)
    }

    /// A socket that was never connected, and therefore one whose
    /// `getpeername(2)` fails with `ENOTCONN` *deterministically*.
    ///
    /// This is the one reliable way to stage a failing live lookup. The
    /// obvious alternative — connect to a closed port and wait for the RST —
    /// is a race: whether the kernel has collected the RST yet, and so whether
    /// `getpeername(2)` has started refusing, is not something a test may
    /// assert on. An unconnected socket is in the refusing state from birth,
    /// which is the state a reset peer leaves behind and the state these
    /// tests are about.
    fn unconnected_stream() -> mio::net::TcpStream {
        let socket = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        )
        .expect("a test socket must be creatable");
        socket
            .set_nonblocking(true)
            .expect("mio requires a nonblocking stream");
        mio::net::TcpStream::from_std(std::net::TcpStream::from(socket))
    }

    /// An opaque backend id for a `Position::Client` connection. The slot is
    /// never resolved by anything under test — `log_context!` renders
    /// `position` and nothing looks the backend up — so a standalone id is
    /// enough and spares these tests a whole `Mux` and backend registry.
    fn test_backend_id(address: std::net::SocketAddr) -> BackendId {
        BackendId {
            slot: BackendSlot(0),
            backend_id: Rc::from("test-backend"),
            address,
        }
    }

    /// Unwrap the H1 arm of a freshly built connection.
    fn h1_of<Front: SocketHandler>(connection: Connection<Front>) -> ConnectionH1<Front> {
        match connection {
            Connection::H1(connection) => connection,
            Connection::H2(_) => unreachable!("new_h1_* builds an H1 connection"),
        }
    }

    /// Symptom 1 — `getpeername(2)` answers `ENOTCONN` once the peer has
    /// reset, so the pre-fix macro rendered `peer=None` on exactly the
    /// `error!` lines an operator reads during an incident. The address was
    /// least available precisely when it mattered most.
    ///
    /// Staged with a never-connected socket rather than a reset one, because
    /// only the former refuses deterministically — see [`unconnected_stream`].
    /// A peer reset is one way to reach that state; what this pins is the
    /// rendering once the socket is in it, which is the part the macro owns.
    ///
    /// To SEE THIS RED: in `log_context!`, put
    /// `peer = $self.socket.socket_ref().peer_addr().ok(),` back in place of
    /// `peer = $self.peer_address,`. The slot collapses to `peer=None`.
    #[test]
    fn log_context_renders_the_peer_when_the_live_lookup_fails() {
        let stream = unconnected_stream();
        let cached = cached_peer();

        // Premise: the live lookup really is refusing, so the assertion below
        // cannot pass for the wrong reason.
        assert_eq!(
            stream.peer_addr().ok(),
            None,
            "an unconnected socket must refuse getpeername(2)"
        );

        let session_ulid = Ulid::generate();
        let connection = h1_of(Connection::new_h1_server(
            session_ulid,
            SessionTcpStream::new(stream, session_ulid, Some(cached)),
            Duration::from_secs(30),
        ));

        let rendered = log_context!(connection);

        assert!(
            rendered.contains(&format!("peer=Some({cached})")),
            "the MUX-H1 peer= slot must survive a failed live lookup: {rendered}"
        );
        assert!(
            !rendered.contains("peer=None"),
            "the MUX-H1 peer= slot must not collapse to None while a snapshot \
             exists: {rendered}"
        );
    }

    /// Symptom 2 — a backend connection is built from a nonblocking
    /// `connect()` that has not completed, so `getpeername(2)` refuses for the
    /// whole window in which an ECONNREFUSED line is emitted. The dial path
    /// caches the cluster-configured backend address into
    /// `SessionTcpStream::configured_peer` for exactly that reason, and the H1
    /// macro has to consult it rather than the raw stream.
    ///
    /// This is the `Position::Client` constructor, which `ConnectionH2` has no
    /// counterpart for: `ConnectionH1` serves backend connections too.
    ///
    /// To SEE THIS RED: in `log_context!`, put
    /// `peer = $self.socket.socket_ref().peer_addr().ok(),` back in place of
    /// `peer = $self.peer_address,`. The backend id the operator needs is
    /// replaced by `peer=None`.
    #[test]
    fn log_context_renders_the_backend_peer_while_the_connect_is_in_flight() {
        let stream = unconnected_stream();
        let backend_address = cached_peer();

        // Premise: this is the state a dial in flight really leaves the socket
        // in — the live lookup refuses.
        assert_eq!(
            stream.peer_addr().ok(),
            None,
            "a connect() still in flight must refuse getpeername(2)"
        );

        let session_ulid = Ulid::generate();
        let connection = h1_of(Connection::new_h1_client(
            session_ulid,
            SessionTcpStream::new(stream, session_ulid, Some(backend_address)),
            "test-cluster".to_owned(),
            test_backend_id(backend_address),
            Duration::from_secs(30),
        ));

        let rendered = log_context!(connection);

        assert!(
            rendered.contains(&format!("peer=Some({backend_address})")),
            "a backend MUX-H1 line must name the configured backend: {rendered}"
        );
        assert!(
            !rendered.contains("peer=None"),
            "a backend MUX-H1 line must not collapse to None while the dial is \
             in flight: {rendered}"
        );
    }

    /// Symptom 3 — on a PROXY-protocol frontend the cached address is the
    /// advertised client while the socket's own peer is the load balancer.
    /// `upgrade_expect` adopts the advertised source into the handler's
    /// `configured_peer`, and the pre-fix macro read straight past it: for one
    /// request id the `HTTP` line named the client and the `MUX-H1` line named
    /// the load balancer.
    ///
    /// The live lookup is deliberately HEALTHY here and disagrees with the
    /// cache, which is what separates this from
    /// [`log_context_renders_the_peer_when_the_live_lookup_fails`] — a single
    /// test cannot be red for both reasons.
    ///
    /// This stages the cleartext expect-proxy route, whose handler really is a
    /// `SessionTcpStream`. The TLS route composes the same macro with
    /// `FrontRustls::peer_addr`, whose own PROXY seeding is pinned in
    /// `https.rs` by `front_rustls_peer_snapshot_is_the_session_peer_not_the_accepted_socket`.
    ///
    /// To SEE THIS RED: in `log_context!`, put
    /// `peer = $self.socket.socket_ref().peer_addr().ok(),` back in place of
    /// `peer = $self.peer_address,`. The rendered line then carries the
    /// loopback address the socket is really connected to — the load balancer
    /// — so the first assertion fails on the missing advertised client.
    #[test]
    fn log_context_renders_the_proxy_advertised_peer_not_the_load_balancer() {
        let (_listener, stream, live_peer) = connected_loopback_stream();
        let advertised = cached_peer();

        // Premise: the live lookup is healthy and disagrees with the cache, so
        // the assertions below cannot pass for the wrong reason.
        assert_eq!(
            stream.peer_addr().ok(),
            Some(live_peer),
            "the test socket must be genuinely connected, so a live lookup succeeds"
        );
        assert_ne!(
            advertised, live_peer,
            "the advertised and transport addresses must differ for this test to discriminate"
        );

        let session_ulid = Ulid::generate();
        let connection = h1_of(Connection::new_h1_server(
            session_ulid,
            SessionTcpStream::new(stream, session_ulid, Some(advertised)),
            Duration::from_secs(30),
        ));

        let rendered = log_context!(connection);

        assert!(
            rendered.contains(&format!("peer=Some({advertised})")),
            "MUX-H1 must render the PROXY-advertised client: {rendered}"
        );
        assert!(
            !rendered.contains(&live_peer.to_string()),
            "MUX-H1 must not fall back to the transport peer, which is the load \
             balancer on a PROXY frontend: {rendered}"
        );
    }

    /// The per-stream twin fills the same slot from the same snapshot.
    ///
    /// `log_context_stream!` has no production expansion in this file today
    /// and carries `#[allow(unused_macros)]`, so nothing else type-checks its
    /// body at all. That is the reason to pin it rather than a reason to skip
    /// it: leaving twin macros on different sources is how they drift apart,
    /// and the divergence would be worse than either being wrong alone — the
    /// same session would render two different peers depending on whether a
    /// stream happened to be in scope at the callsite.
    ///
    /// To SEE THIS RED: in `log_context_stream!`, put
    /// `peer = $self.socket.socket_ref().peer_addr().ok(),` back in place of
    /// `peer = $self.peer_address,`. The per-stream envelope then carries the
    /// loopback address while the connection envelope carries the cache.
    #[test]
    fn log_context_stream_renders_the_snapshot_not_a_live_lookup() {
        let (_listener, stream, live_peer) = connected_loopback_stream();
        let advertised = cached_peer();

        assert_eq!(
            stream.peer_addr().ok(),
            Some(live_peer),
            "the test socket must be genuinely connected, so a live lookup succeeds"
        );

        let session_ulid = Ulid::generate();
        let connection = h1_of(Connection::new_h1_server(
            session_ulid,
            SessionTcpStream::new(stream, session_ulid, Some(advertised)),
            Duration::from_secs(30),
        ));
        let http_context = test_http_context(session_ulid);

        let rendered = log_context_stream!(connection, http_context);

        assert!(
            rendered.contains(&format!("peer=Some({advertised})")),
            "the per-stream MUX-H1 envelope must render the snapshot: {rendered}"
        );
        assert!(
            !rendered.contains(&live_peer.to_string()),
            "the per-stream MUX-H1 envelope must not fall back to the live \
             getpeername(2) answer: {rendered}"
        );
    }

    /// A [`SocketHandler`] that counts how often it is asked for the peer
    /// address, delegating every other method to a real loopback stream.
    ///
    /// The tests above pin WHICH address the log prefix renders. None of them
    /// can see how many times the connection reaches into the socket to get
    /// it, because both production handlers answer from a cache and so give
    /// the same answer however often they are asked. This handler makes the
    /// count observable.
    struct PeerAddrCountingSocket {
        stream: mio::net::TcpStream,
        peer: std::net::SocketAddr,
        calls: Rc<Cell<usize>>,
    }

    impl SocketHandler for PeerAddrCountingSocket {
        fn socket_read(&mut self, buf: &mut [u8]) -> (usize, SocketResult) {
            self.stream.socket_read(buf)
        }

        fn socket_write(&mut self, buf: &[u8]) -> (usize, SocketResult) {
            self.stream.socket_write(buf)
        }

        fn socket_write_vectored(&mut self, bufs: &[IoSlice]) -> (usize, SocketResult) {
            self.stream.socket_write_vectored(bufs)
        }

        fn socket_ref(&self) -> &mio::net::TcpStream {
            &self.stream
        }

        fn socket_mut(&mut self) -> &mut mio::net::TcpStream {
            &mut self.stream
        }

        fn peer_addr(&self) -> Option<std::net::SocketAddr> {
            self.calls.set(self.calls.get() + 1);
            Some(self.peer)
        }

        fn protocol(&self) -> crate::socket::TransportProtocol {
            crate::socket::TransportProtocol::Tcp
        }

        fn read_error(&self) {}

        fn write_error(&self) {}
    }

    /// The `peer=` slot is read from the socket exactly ONCE per connection —
    /// at construction — however many log lines the connection renders.
    ///
    /// This is the property that makes `peer_address` worth having, and it is
    /// the only one that separates the snapshot from simply calling the trait
    /// method in the macro. It is not about which address appears (the tests
    /// above own that): for a handler that already caches, reading the cache
    /// and reading a snapshot taken from that cache agree by construction, so
    /// substituting `peer = $self.socket.peer_addr(),` leaves every one of
    /// them green. What the snapshot changed is WHICH object a log line reads,
    /// and only a call count can observe that.
    ///
    /// TO SEE THIS RED: in `log_context!`, put
    /// `peer = $self.socket.peer_addr(),` — the call-the-trait-method shape —
    /// in place of `peer = $self.peer_address,`. The count then rises by one
    /// per rendered line and the final assertion fails with
    /// `the peer address must be read once, at construction, not once per log line:
    /// left: 4, right: 1`.
    #[test]
    fn log_context_reads_the_peer_address_once_per_connection() {
        let (_listener, stream, _live_peer) = connected_loopback_stream();
        let calls = Rc::new(Cell::new(0usize));
        let peer = cached_peer();
        let socket = PeerAddrCountingSocket {
            stream,
            peer,
            calls: Rc::clone(&calls),
        };

        let connection = h1_of(Connection::new_h1_server(
            Ulid::generate(),
            socket,
            Duration::from_secs(30),
        ));

        assert_eq!(
            calls.get(),
            1,
            "construction takes exactly one peer_addr() snapshot"
        );

        // Premise: the rendered lines really do carry the address, so a zero
        // count below would mean the slot went missing rather than that it
        // became free.
        for _ in 0..3 {
            let rendered = log_context!(connection);
            assert!(
                rendered.contains(&format!("peer=Some({peer})")),
                "each rendered line must still carry the peer: {rendered}"
            );
        }

        assert_eq!(
            calls.get(),
            1,
            "the peer address must be read once, at construction, not once per log line"
        );
    }

    /// The three fields `log_context_stream!` reads out of an `HttpContext`,
    /// with every other field at an inert default. Built directly rather than
    /// through a `Stream`, because the macro touches no buffer and a real
    /// stream would drag a whole `Pool` in behind it.
    fn test_http_context(session_ulid: Ulid) -> HttpContext {
        HttpContext {
            keep_alive_backend: true,
            keep_alive_frontend: true,
            sticky_session_found: None,
            method: None,
            authority: None,
            path: None,
            status: None,
            reason: None,
            user_agent: None,
            x_request_id: None,
            xff_chain: None,
            #[cfg(feature = "opentelemetry")]
            otel: None,
            closing: false,
            session_id: session_ulid,
            id: Ulid::generate(),
            backend_id: None,
            cluster_id: None,
            protocol: TransportKind::HTTP,
            public_address: "127.0.0.1:0"
                .parse()
                .expect("the public address literal must parse"),
            session_address: None,
            sticky_name: String::new(),
            sticky_session: None,
            backend_address: None,
            tls_server_name: None,
            tls_cert_names: None,
            strict_sni_binding: false,
            elide_x_real_ip: false,
            send_x_real_ip: false,
            tls_version: None,
            tls_cipher: None,
            tls_alpn: None,
            sozu_id_header: String::from("Sozu-Id"),
            redirect_location: None,
            www_authenticate: None,
            original_authority: None,
            headers_response: Vec::new(),
            retry_after_seconds: None,
            frontend_redirect_template: None,
            redirect_status: None,
            tags: None,
            access_log_message: None,
        }
    }

    /// `ConnectionH1`'s `Debug` renders the peer address the connection
    /// snapshotted at construction, not the transport address the kernel knows.
    ///
    /// This is the twin of `ConnectionH2`'s `Debug`, which already renders
    /// `peer_address` and names no socket at all. `ConnectionH1` was the last
    /// `Debug` in this module handing `mio::net::TcpStream`'s own `Debug` a
    /// `socket` field through `SocketHandler::socket_ref` — a local address, a
    /// peer address and a file descriptor — and so the last one whose rendered
    /// line could disagree with the `peer=` slot every `log_context!` line of
    /// the same connection already carries.
    ///
    /// Staged like
    /// [`log_context_renders_the_proxy_advertised_peer_not_the_load_balancer`]:
    /// a genuinely connected loopback socket whose live lookup is healthy and
    /// disagrees with the address its handler declares, which is the
    /// PROXY-protocol frontend's shape. This pins the VALUE rendered rather
    /// than a count — the two addresses differ, so only one of them can appear.
    ///
    /// To SEE THIS RED: in `impl Debug for ConnectionH1`, put
    /// `.field("socket", &self.socket.socket_ref())` back in place of
    /// `.field("peer_address", &self.peer_address)`. The rendered struct then
    /// carries the loopback address the socket is really connected to — the
    /// load balancer — so the first assertion fails on the missing advertised
    /// client and the second on the transport address being present.
    #[test]
    fn debug_renders_the_proxy_advertised_peer_not_the_transport_socket() {
        let (_listener, stream, live_peer) = connected_loopback_stream();
        let advertised = cached_peer();

        // Premise: the live lookup is healthy and disagrees with the declared
        // address, so the assertions below cannot pass for the wrong reason.
        assert_eq!(
            stream.peer_addr().ok(),
            Some(live_peer),
            "the test socket must be genuinely connected, so a live lookup succeeds"
        );
        assert_ne!(
            advertised, live_peer,
            "the declared and transport addresses must differ for this test to discriminate"
        );

        let session_ulid = Ulid::generate();
        let connection = h1_of(Connection::new_h1_server(
            session_ulid,
            SessionTcpStream::new(stream, session_ulid, Some(advertised)),
            Duration::from_secs(30),
        ));

        let rendered = format!("{connection:?}");

        assert!(
            rendered.contains(&format!("Some({advertised})")),
            "the ConnectionH1 Debug must render the declared peer: {rendered}"
        );
        assert!(
            !rendered.contains(&live_peer.to_string()),
            "the ConnectionH1 Debug must not render the transport address, which \
             is the load balancer on a PROXY frontend: {rendered}"
        );
    }

    // ── `backend_header_time`, the H1 half (sozu-proxy/sozu#426) ─────────
    //
    // DO NOT READ THESE TESTS AS COVERING H2. They drive
    // `ConnectionH1::readable` only. The H2 twin lives in
    // `ConnectionH2::handle_headers_frame` (`lib/src/protocol/mux/h2.rs`) and
    // is pinned by its own test there.

    /// A `Position::Client` H1 backend connection whose stream is linked to a
    /// frontend, staged at the exact moment a real dial completes: the
    /// connection is `Connected`, `backend_connected` is armed, and not one
    /// response byte has arrived.
    ///
    /// Both loopback peers are returned and must be held for the whole test —
    /// dropping either tears the connection down and turns the next
    /// `readable()` into a forced disconnect instead of a parse.
    /// Held together rather than returned as a tuple, the way
    /// `LedgerFixture` (`lib/src/protocol/mux/h2.rs`) is: the buffer pool and
    /// both loopback peers are lifetime ballast with no business in a test
    /// body, and a six-element tuple is `clippy::type_complexity`.
    struct BackendReadFixture {
        context: Context<crate::protocol::mux::test_support::TestListener>,
        frontend: Connection<mio::net::TcpStream>,
        client: ConnectionH1<mio::net::TcpStream>,
        backend_peer: std::net::TcpStream,
        _frontend_peer: std::net::TcpStream,
        _pool: Rc<RefCell<Pool>>,
    }

    fn linked_backend_connection() -> BackendReadFixture {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 8, 16384)));
        let mut context = test_context(&pool);
        context
            .create_stream(Ulid::generate(), 1 << 16)
            .expect("the test pool must hand out stream buffers");

        let (front_socket, front_peer) = connected_socket();
        let frontend =
            Connection::new_h1_server(Ulid::generate(), front_socket, Duration::from_secs(60));

        let (back_socket, back_peer) = connected_socket();
        let session_ulid = Ulid::generate();
        let mut client = h1_of(Connection::new_h1_client(
            session_ulid,
            back_socket,
            "test-cluster".to_owned(),
            test_backend_id(cached_peer()),
            Duration::from_secs(60),
        ));
        // `new_h1_client` opens in `Connecting`; move it the way a completed
        // dial does, so the read path runs its `Connected` accounting.
        if let Position::Client(_, _, status) = &mut client.position {
            *status = BackendStatus::Connected;
        }
        client.stream = Some(0);

        let stream = &mut context.streams[0];
        stream.state = StreamState::Linked(mio::Token(1));
        stream.metrics.backend_id = Some("test-backend".into());
        stream.metrics.backend_start();
        stream.metrics.backend_connected();

        BackendReadFixture {
            context,
            frontend,
            client,
            backend_peer: back_peer,
            _frontend_peer: front_peer,
            _pool: pool,
        }
    }

    /// Hand `bytes` to the backend peer and drive `ConnectionH1::readable`
    /// until `done` holds, bounded.
    ///
    /// The loop is not a retry papering over a flaky assertion: a loopback
    /// write is a syscall away, not instantaneous, so a single `readable()`
    /// may legitimately see `WouldBlock` and read nothing. The bound is what
    /// keeps a genuine failure a failure instead of a hang.
    fn feed_backend<F>(
        peer: &mut std::net::TcpStream,
        client: &mut ConnectionH1<mio::net::TcpStream>,
        context: &mut Context<crate::protocol::mux::test_support::TestListener>,
        frontend: &mut Connection<mio::net::TcpStream>,
        bytes: &[u8],
        done: F,
    ) where
        F: Fn(&Context<crate::protocol::mux::test_support::TestListener>) -> bool,
    {
        peer.write_all(bytes)
            .expect("the loopback peer must accept the staged response bytes");
        for _ in 0..64 {
            client.readiness.event.insert(Ready::READABLE);
            client.readable(context, EndpointServer(frontend));
            if done(context) {
                return;
            }
        }
        panic!("the backend read path never reached the staged state");
    }

    /// The whole point of the metric, pinned as an ordering: the H1 backend
    /// time-to-first-header-byte instant is taken when the response HEADERS
    /// finish parsing, which is strictly before the response ENDS.
    ///
    /// Two phases, and the split is what makes this discriminating. After the
    /// headers alone (`Content-Length: 2`, body withheld) the header instant
    /// must already exist while `backend_stop` — the response-end marker every
    /// one of `ConnectionH1::writable`'s five sites owns — must still be
    /// absent. An instant placed at any of those sites cannot satisfy that.
    /// After the body, the instant must be UNCHANGED: the transition fires on
    /// the header -> body edge, not on every read.
    ///
    /// The ordering assertion is then `header_time <= response_time` on
    /// `Duration`s, not on the millisecond values the histogram stores: at
    /// millisecond resolution a fast test rounds both to 0 and the relation
    /// would hold however wrong the placement is.
    ///
    /// `backend_stop` is marked by the test rather than driven through
    /// `ConnectionH1::writable` because that pass calls
    /// `SessionMetrics::reset` immediately after its access log, which wipes
    /// every instant before a test could read one.
    ///
    /// To SEE THIS RED: move `parts.metrics.backend_headers_received();` out
    /// of the `!was_main_phase && self.position.is_client()` branch of
    /// `ConnectionH1::readable` and put it immediately after the
    /// `stream.metrics.backend_stop();` that precedes `Some("H1::Complete")`
    /// in `ConnectionH1::writable`. Phase one then reports no header time at
    /// all.
    #[test]
    fn the_h1_backend_header_instant_lands_on_the_headers_not_the_response_end() {
        let mut fixture = linked_backend_connection();
        let BackendReadFixture {
            context,
            frontend,
            client,
            backend_peer,
            ..
        } = &mut fixture;

        // Premise: nothing has been received, so neither instant can be stale.
        assert_eq!(context.streams[0].metrics.backend_header_time(), None);
        assert_eq!(context.streams[0].metrics.backend_stop, None);

        feed_backend(
            backend_peer,
            client,
            context,
            frontend,
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n",
            |context| context.streams[0].back.is_main_phase(),
        );

        let after_headers = context.streams[0]
            .metrics
            .backend_headers_received
            .expect("the response headers must arm the backend header instant");
        assert!(
            !context.streams[0].back.is_terminated(),
            "premise: the response is not over — its 2-byte body is still \
             outstanding, so this phase cannot be observing the response end"
        );
        assert_eq!(
            context.streams[0].metrics.backend_stop, None,
            "the response-end marker must still be absent while only the \
             headers have arrived"
        );

        feed_backend(backend_peer, client, context, frontend, b"ok", |context| {
            context.streams[0].back.is_terminated()
        });

        assert_eq!(
            context.streams[0].metrics.backend_headers_received,
            Some(after_headers),
            "the instant is taken on the header -> body edge, so the body \
             reads must not move it"
        );

        // What the `Some("H1::Complete")` arm of `ConnectionH1::writable` does
        // once the whole response has been forwarded.
        context.streams[0].metrics.backend_stop();

        let metrics = &context.streams[0].metrics;
        let header_time = metrics
            .backend_header_time()
            .expect("a completed H1 response must report a backend header time");
        let response_time = metrics
            .backend_response_time()
            .expect("a completed H1 response must report a backend response time");
        assert!(
            header_time <= response_time,
            "the first response-header byte arrives before the last response \
             byte: header_time {header_time:?} must not exceed response_time \
             {response_time:?}"
        );
    }

    // ── The access log of a request the parse rejected (sozu-proxy/sozu#1085) ──
    //
    // DO NOT READ THESE TESTS AS COVERING H2. They drive a `Position::Server`
    // `ConnectionH1` only. An H2 request whose field block `handle_header`
    // (`lib/src/protocol/mux/pkawa.rs`) refuses takes another path,
    // `record_rejected_request` in the same file, and is covered by the
    // `h2` tests beside `rejected_request_line`
    // (`lib/src/protocol/mux/stream.rs`).

    /// Drive `request` through a real `Position::Server` `ConnectionH1`, the
    /// way a client socket does: `readable` until the parse is rejected, then
    /// `writable` until the 400 is written and its access log emitted.
    /// Returns every log line the run produced.
    ///
    /// Driving both halves, rather than calling `Stream::generate_access_log`
    /// on a staged stream, is what makes these tests say which site emits the
    /// line and that the request bytes are still in the front buffer when it
    /// does.
    ///
    /// The loops are bounded for the reason `feed_backend` gives: a loopback
    /// write is a syscall away, not instantaneous.
    fn access_log_of_a_rejected_h1_request(request: &'static [u8]) -> String {
        crate::capture_test_logs(move || {
            let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 8, 16384)));
            let mut context = test_context(&pool);
            context
                .create_stream(Ulid::generate(), 1 << 16)
                .expect("the test pool must hand out stream buffers");
            let (front_socket, mut front_peer) = connected_socket();
            let mut server = h1_of(Connection::new_h1_server(
                Ulid::generate(),
                front_socket,
                Duration::from_secs(60),
            ));
            server.stream = Some(0);
            let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));

            front_peer
                .write_all(request)
                .expect("the loopback peer must accept the staged request bytes");
            for _ in 0..64 {
                server.readiness.event.insert(Ready::READABLE);
                server.readable(&mut context, EndpointClient(&mut router));
                if context.streams[0].front.is_error() {
                    break;
                }
            }
            assert!(
                context.streams[0].front.is_error(),
                "premise: the staged request must be rejected by the parse"
            );
            assert_eq!(
                context.streams[0].context.method, None,
                "premise: the rejection happened before `HttpContext` captured \
                 the request line, which is the whole defect"
            );

            // `ConnectionH1::writable` resets the stream metrics right after
            // it emits the access log, so a cleared start instant is the
            // observable "the line went out".
            for _ in 0..64 {
                server.readiness.event.insert(Ready::WRITABLE);
                server.writable(&mut context, EndpointClient(&mut router));
                if context.streams[0].metrics.start.is_none() {
                    break;
                }
            }
        })
    }

    /// A header the parser refuses, after a well-formed request line: the
    /// access log must still name the method and the path.
    ///
    /// The authority is asserted ABSENT, on purpose. This request carries a
    /// `Host` line before the bad one, so a `Host` block exists here — but a
    /// client that puts the bad line first gets no `Host` block at all, so
    /// reading it would log an unvalidated value whenever the client chose
    /// to. Only an authority the request line itself carries is logged.
    ///
    /// To SEE THIS RED: in `Stream::generate_access_log`
    /// (`lib/src/protocol/mux/stream.rs`), replace the
    /// `rejected_request_line(&self.front)` call with
    /// `None::<RejectedRequestLine<'_>>`. The line then renders
    /// `- - - 400`.
    #[test]
    fn a_header_parse_error_keeps_the_method_and_path_in_the_access_log() {
        let output = access_log_of_a_rejected_h1_request(
            b"GET /diag?x=1 HTTP/1.1\r\nHost: example.com\r\nBad Header: x\r\n\r\n",
        );

        assert!(
            output.contains("GET /diag?x=1 400"),
            "the access log of a request rejected on a header must carry its \
             method and path, got: {output}"
        );
        assert!(
            output.contains("- GET /diag?x=1 400"),
            "the authority of an origin-form request rejected on a header must \
             not be read from a `Host` line, got: {output}"
        );
        assert!(
            output.contains("H1::Complete"),
            "the 400 of a parse error is logged by `ConnectionH1::writable` once \
             the answer is written, not at session close, got: {output}"
        );
    }

    /// Sōzu's own CL.TE guard in `HttpContext::on_request_headers`
    /// (`lib/src/protocol/kawa_h1/editor.rs`) rejects a request kawa parsed
    /// cleanly, and returns before the request line is captured. Here kawa's
    /// `process_headers` already resolved the authority and the path, so all
    /// three fields are logged.
    ///
    /// `identity` then `chunked` is the pair that reaches that guard: kawa
    /// combines it to a chunked-final coding and accepts it, and the guard
    /// refuses to forward two TE lines. A lone `Transfer-Encoding: gzip`
    /// would not — kawa refuses it itself, before resolving the authority.
    ///
    /// To SEE THIS RED: the same substitution as
    /// `a_header_parse_error_keeps_the_method_and_path_in_the_access_log`.
    #[test]
    fn a_cl_te_rejection_keeps_the_whole_request_line_in_the_access_log() {
        let output = access_log_of_a_rejected_h1_request(
            b"POST /upload HTTP/1.1\r\nHost: example.com\r\n\
              Transfer-Encoding: identity\r\nTransfer-Encoding: chunked\r\n\r\n",
        );

        assert!(
            output.contains("example.com POST /upload 400"),
            "the access log of a request rejected by the CL.TE guard must carry \
             its authority, method and path, got: {output}"
        );
    }

    /// The one authority a header rejection may log: the one the request
    /// line itself carries. An absolute-form target names it, so it is
    /// logged, split from the path the way kawa splits it.
    ///
    /// To SEE THIS RED: the same substitution as
    /// `a_header_parse_error_keeps_the_method_and_path_in_the_access_log`.
    #[test]
    fn a_header_parse_error_logs_the_authority_an_absolute_form_carries() {
        let output = access_log_of_a_rejected_h1_request(
            b"GET http://example.com:8080/abs HTTP/1.1\r\nBad Header: x\r\n\r\n",
        );

        assert!(
            output.contains("example.com:8080 GET /abs 400"),
            "the access log of an absolute-form request rejected on a header \
             must carry the authority of its request-target, got: {output}"
        );
    }
}
