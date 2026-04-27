//! H1 mux connection wrapper.
//!
//! Hosts the single active H1 stream (`stream: GlobalStreamId`) and wires
//! Kawa-owned H1 parsing + serialization into the shared mux `Context` so the
//! same routing / shutdown / readiness machinery applies across H1 and H2
//! connections. Long-form lifecycle: `lib/src/protocol/mux/LIFECYCLE.md`.

use std::{io::IoSlice, time::Instant};

use rusty_ulid::Ulid;
use sozu_command::{logging::ansi_palette, ready::Ready};

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
    socket::{SocketHandler, SocketResult},
    timer::TimeoutContainer,
};

/// Prefix applied to every [`ConnectionH1`] log line. Matches the RUSTLS
/// log-context convention (`MUX-H1\tSession(...)\t >>>`). When the logger is
/// in colored mode the label is bold bright-white (uniform across every
/// protocol) and the session detail is rendered in light grey.
///
/// Fields included in the session block (chosen to surface the most common
/// H1 troubleshooting axes — keep-alive churn, stream pinning, buffer-pressure
/// stall and graceful TLS shutdown):
/// - `peer` — peer address (or `None` if the socket is gone)
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
            peer = $self.socket.socket_ref().peer_addr().ok(),
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
            peer = $self.socket.socket_ref().peer_addr().ok(),
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
    /// Active stream index, or `None` when the connection has no assigned stream
    /// (initial client state before `start_stream`, or after `end_stream` detaches).
    pub stream: Option<GlobalStreamId>,
    pub timeout_container: TimeoutContainer,
    /// Set when `readable` exits early because the kawa buffer was full.
    /// Edge-triggered epoll will not re-fire READABLE for data already in the
    /// kernel socket buffer, so the cross-readiness mechanism must re-arm it
    /// via `try_resume_reading` once the peer drains the buffer.
    pub parked_on_buffer_pressure: bool,
    /// True once we've asked rustls to emit TLS close_notify for this frontend.
    pub close_notify_sent: bool,
    /// Connection/session ULID propagated from the parent [`Mux`]. Used to
    /// stamp the session slot of the `[session req cluster backend]` log
    /// prefix emitted by the local `log_context!` macro.
    pub session_ulid: Ulid,
}

impl<Front: SocketHandler> std::fmt::Debug for ConnectionH1<Front> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionH1")
            .field("position", &self.position)
            .field("readiness", &self.readiness)
            .field("socket", &self.socket.socket_ref())
            .field("stream", &self.stream)
            .finish()
    }
}

impl<Front: SocketHandler> ConnectionH1<Front> {
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
        if kawa.body_size == kawa::BodySize::Chunked {
            warn!(
                "{} H1 backend EOF mid-chunked response on stream {}: emitting RST_STREAM",
                log_module_context!(),
                stream_id
            );
            incr!("h1.backend_eof_before_message_complete");
            kawa.parsing_phase
                .error(kawa::ParsingErrorKind::Processing {
                    message: "INTERNAL_ERROR",
                });
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
        self.timeout_container.reset();
        let answers_rc = context.listener.borrow().get_answers().clone();
        let stream = &mut context.streams[stream_id];
        if stream.metrics.start.is_none() {
            stream.metrics.start = Some(Instant::now());
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
            return MuxResult::Continue;
        }

        self.parked_on_buffer_pressure = false;
        let (size, status) = self.socket.socket_read(kawa.storage.space());
        context.debug.push(DebugEvent::StreamEvent(0, size));
        kawa.storage.fill(size);
        self.position.count_bytes_in_counter(size);
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
                self.timeout_container.cancel();
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
                    incr!("http.backend_parse_errors");
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
                    incr!("http.frontend_parse_errors");
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
            self.timeout_container.cancel();
            self.readiness.interest.remove(Ready::READABLE);
        }
        if kawa.is_main_phase() {
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
                self.requests += 1;
                trace!("{} REQUESTS: {}", log_context!(self), self.requests);
                incr!("http.requests");
                gauge_add!("http.active_requests", 1);
                parts.metrics.service_start();
                // Set request_counted after the last use of `parts` to satisfy the borrow checker
                stream.request_counted = true;
                stream.state = StreamState::Link;
                context.pending_links.push_back(stream_id);
            }
            if let StreamState::Linked(token) = stream.state {
                // Signal pending write alongside the WRITABLE interest flip: the
                // bytes we just parsed live in sozu's buffers, not the kernel,
                // so edge-triggered epoll won't re-fire on its own.
                let peer = endpoint.readiness_mut(token);
                peer.arm_writable();
            }
        };
        // 1xx informational: the 100 response skips main_phase (goes straight to
        // Terminated), so the normal "set endpoint writable" above never fires.
        // Trigger the frontend to write the 1xx response after all borrows end.
        if is_1xx_backend {
            if let StreamState::Linked(token) = stream.state {
                let peer = endpoint.readiness_mut(token);
                peer.arm_writable();
            }
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
            self.timeout_container.cancel();
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
        self.timeout_container.reset();
        let stream = &mut context.streams[stream_id];
        let parts = stream.split(&self.position);
        let kawa = parts.wbuffer;
        // Apply per-frontend response-side header edits stashed by the
        // routing layer at request time. Only the Server-position pass
        // touches the response back-kawa; the Client-position pass
        // (writing the request to the backend) has already had its
        // edits applied in `Router::route_from_request`.
        if matches!(self.position, Position::Server) && !parts.context.headers_response.is_empty() {
            super::shared::apply_response_header_edits(kawa, &parts.context.headers_response);
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
        let (size, status) = self.socket.socket_write_vectored(&io_slices);
        context.debug.push(DebugEvent::StreamEvent(1, size));
        kawa.consume(size);
        self.position.count_bytes_out_counter(size);
        self.position.count_bytes_out(parts.metrics, size);
        let should_yield = update_readiness_after_write(size, status, &mut self.readiness);
        if self.socket.socket_wants_write() {
            self.readiness.signal_pending_write();
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
                            stream.generate_access_log(
                                false,
                                Some("H1::Upgrade"),
                                context.listener.clone(),
                            );
                            return MuxResult::Upgrade;
                        }
                        kawa::StatusLine::Response { code: 100, .. } => {
                            debug!("{} ============== HANDLE CONTINUE!", log_context!(self));
                            // After a 100 Continue, we expect the client to continue
                            // with its request body. Do NOT call generate_access_log
                            // here — the final response will emit the access log.
                            // Calling it here would double-decrement http.active_requests.
                            self.timeout_container.reset();
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
                                stream.generate_access_log(
                                    false,
                                    Some("H1::EarlyHint"),
                                    context.listener.clone(),
                                );
                                return self.defer_close_for_tls_flush("early-hint");
                            }
                        }
                        _ => {}
                    }
                    incr!("http.e2e.http11");
                    stream.metrics.backend_stop();
                    stream.generate_access_log(
                        false,
                        Some("H1::Complete"),
                        context.listener.clone(),
                    );
                    stream.metrics.reset();
                    let old_state = std::mem::replace(&mut stream.state, StreamState::Unlinked);
                    if let StreamState::Linked(token) = old_state {
                        remove_backend_stream(&mut context.backend_streams, token, stream_id);
                    }
                    if stream.context.keep_alive_frontend {
                        self.timeout_container.reset();
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
                        // Transition back to Idle so buffered pipelined requests
                        // trigger a phase transition on the next readable() call.
                        stream.state = StreamState::Idle;
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
                                let answers_rc = context.listener.borrow().get_answers().clone();
                                let answers = answers_rc.borrow();
                                set_default_answer(stream, &mut self.readiness, 400, &answers);
                            } else if is_main {
                                self.requests += 1;
                                incr!("http.requests");
                                gauge_add!("http.active_requests", 1);
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
        if !self.close_notify_sent {
            trace!("{} H1 initiating CLOSE_NOTIFY", log_context!(self));
            self.socket.socket_close();
            self.close_notify_sent = true;
        }
        if self.socket.socket_wants_write() {
            self.readiness.arm_writable();
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
        context.unlink_stream(stream);
        let answers_rc = context.listener.borrow().get_answers().clone();
        let stream_id = stream;
        let stream = &mut context.streams[stream_id];
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
                self.readiness.interest.remove(Ready::ALL);
                self.force_disconnect();
            }
            Position::Client(_, _, status @ BackendStatus::Connected) => {
                self.stream = None;
                if stream.state != StreamState::Recycle {
                    stream.state = StreamState::Unlinked;
                }
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
            },
        }
    }

    pub fn start_stream<L>(&mut self, stream: GlobalStreamId, _context: &mut Context<L>) -> bool
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
        match &mut self.position {
            Position::Client(_, _, status @ BackendStatus::KeepAlive) => {
                *status = BackendStatus::Connected;
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
