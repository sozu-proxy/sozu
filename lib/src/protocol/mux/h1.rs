use std::time::Instant;

use sozu_command::ready::Ready;

use crate::{
    L7ListenerHandler, ListenerHandler, Readiness,
    protocol::mux::{
        BackendStatus, Context, DebugEvent, Endpoint, GlobalStreamId, MuxResult, Position,
        StreamState, forcefully_terminate_answer, parser::H2Error, set_default_answer,
        update_readiness_after_read, update_readiness_after_write,
    },
    socket::{SocketHandler, SocketResult},
    timer::TimeoutContainer,
};

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
                "H1 writable delaying close after {}: stream={:?}, close_notify_sent={}, wants_write={}, readiness={:?}",
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
    fn terminate_close_delimited(kawa: &mut super::GenericHttpStream, stream_id: GlobalStreamId) {
        debug!(
            "H1 close-delimited EOF on stream {}: terminating body",
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
        trace!("======= MUX H1 READABLE {:?}", self.position);
        let Some(stream_id) = self.stream else {
            error!("readable() called on H1 connection with no active stream");
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
                    endpoint
                        .readiness_mut(token)
                        .interest
                        .insert(Ready::WRITABLE);
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
                        error!("client stream in error is not in Linked state");
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
                    debug!("H1 backend: received {} informational response", code);
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
                            "Unexpected malformed request: HTTP/1.0 from {:?} with {:?} {:?} {:?}",
                            parts.context.session_address,
                            parts.context.method,
                            parts.context.authority,
                            parts.context.path
                        );
                    } else {
                        error!("Unexpected malformed request");
                        kawa::debug_kawa(kawa);
                    }
                    let answers = answers_rc.borrow();
                    set_default_answer(stream, &mut self.readiness, 400, &answers);
                    return MuxResult::Continue;
                }
                self.requests += 1;
                trace!("REQUESTS: {}", self.requests);
                incr!("http.requests");
                gauge_add!("http.active_requests", 1);
                parts.metrics.service_start();
                // Set request_counted after the last use of `parts` to satisfy the borrow checker
                stream.request_counted = true;
                stream.state = StreamState::Link
            }
            if let StreamState::Linked(token) = stream.state {
                endpoint
                    .readiness_mut(token)
                    .interest
                    .insert(Ready::WRITABLE)
            }
        };
        // 1xx informational: the 100 response skips main_phase (goes straight to
        // Terminated), so the normal "set endpoint writable" above never fires.
        // Trigger the frontend to write the 1xx response after all borrows end.
        if is_1xx_backend {
            if let StreamState::Linked(token) = stream.state {
                endpoint
                    .readiness_mut(token)
                    .interest
                    .insert(Ready::WRITABLE);
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
        trace!("======= MUX H1 WRITABLE {:?}", self.position);
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
        kawa.prepare(&mut kawa::h1::BlockConverter);
        let bufs = kawa.as_io_slice();
        let can_finalize_server_close = matches!(self.position, Position::Server)
            && kawa.is_terminated()
            && kawa.is_completed();
        if bufs.is_empty() && !self.socket.socket_wants_write() && !can_finalize_server_close {
            self.readiness.interest.remove(Ready::WRITABLE);
            return MuxResult::Continue;
        }
        let tls_only_flush = bufs.is_empty();
        let (size, status) = self.socket.socket_write_vectored(&bufs);
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
                            debug!("============== HANDLE UPGRADE!");
                            stream.metrics.backend_stop();
                            stream.generate_access_log(
                                false,
                                Some("H1::Upgrade"),
                                context.listener.clone(),
                            );
                            return MuxResult::Upgrade;
                        }
                        kawa::StatusLine::Response { code: 100, .. } => {
                            debug!("============== HANDLE CONTINUE!");
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
                            debug!("============== HANDLE EARLY HINT!");
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
                    "H1 force_disconnect client: stream={:?}, wants_write={}, readiness={:?}",
                    self.stream,
                    self.socket.socket_wants_write(),
                    self.readiness
                );
                MuxResult::Continue
            }
            Position::Server => {
                if self.socket.socket_wants_write() {
                    debug!(
                        "H1 force_disconnect delaying close: stream={:?}, wants_write=true, readiness={:?}",
                        self.stream, self.readiness
                    );
                    self.readiness.interest = Ready::WRITABLE | Ready::HUP | Ready::ERROR;
                    self.readiness.signal_pending_write();
                    MuxResult::Continue
                } else {
                    debug!(
                        "H1 force_disconnect closing session: stream={:?}, wants_write=false, readiness={:?}",
                        self.stream, self.readiness
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
            trace!("H1 initiating CLOSE_NOTIFY");
            self.socket.socket_close();
            self.close_notify_sent = true;
        }
        if self.socket.socket_wants_write() {
            self.readiness.interest.insert(Ready::WRITABLE);
            self.readiness.signal_pending_write();
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
                trace!("close detached client ConnectionH1");
                return;
            }
            Position::Client(_, _, BackendStatus::Connecting(_))
            | Position::Client(_, _, BackendStatus::Connected) => {
                debug!("BACKEND CLOSING FOR: {:?} {:?}", self.position, self.stream);
            }
            Position::Server => {
                let tls_pending_before = self.socket.socket_wants_write();
                if !self.close_notify_sent {
                    trace!("H1 SENDING CLOSE NOTIFY");
                    self.socket.socket_close();
                    self.close_notify_sent = true;
                }
                let mut drain_rounds = 0;
                while self.socket.socket_wants_write() && drain_rounds < 16 {
                    let (_size, status) = self.socket.socket_write_vectored(&[]);
                    drain_rounds += 1;
                    match status {
                        SocketResult::WouldBlock | SocketResult::Error | SocketResult::Closed => {
                            break;
                        }
                        SocketResult::Continue => {}
                    }
                }
                let tls_pending_after = self.socket.socket_wants_write();
                if tls_pending_after {
                    error!(
                        "H1 TLS buffer NOT fully drained on close: pending_before={}, pending_after={}, drain_rounds={}, stream={:?}, close_notify_sent={}, readiness={:?}",
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
            trace!("closing detached H1 client with no active stream");
            return;
        };
        // reconnection is handled by the server
        let StreamState::Linked(token) = context.streams[stream_id].state else {
            trace!(
                "closing detached H1 client in state {:?} on stream {}",
                context.streams[stream_id].state, stream_id
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
                "end_stream called with stream {} but expected {:?}",
                stream, self.stream
            );
            return;
        }
        let answers_rc = context.listener.borrow().get_answers().clone();
        let stream = &mut context.streams[stream];
        let stream_context = &mut stream.context;
        trace!("end H1 stream {:?}: {:#?}", self.stream, stream_context);
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
                error!("end_stream called on KeepAlive or Disconnecting H1 client");
            }
            Position::Server => match (stream.front.consumed, stream.back.is_main_phase()) {
                (true, true) => {
                    // we have a "forwardable" answer from the back
                    // if the answer is not terminated we send an RstStream to properly clean the stream
                    // if it is terminated, we finish the transfer, the backend is not necessary anymore
                    if !stream.context.keep_alive_backend {
                        debug!("CLOSE DELIMITED");
                        stream.state = StreamState::Unlinked;
                        self.readiness.interest.insert(Ready::WRITABLE);
                    } else if !stream.back.is_terminated() {
                        forcefully_terminate_answer(
                            stream,
                            &mut self.readiness,
                            H2Error::InternalError,
                        );
                    } else {
                        stream.state = StreamState::Unlinked;
                        self.readiness.interest.insert(Ready::WRITABLE);
                    }
                }
                (true, false) => {
                    // we do not have an answer, but the request has already been partially consumed
                    // so we can't retry
                    let answers = answers_rc.borrow();
                    if stream.back.is_error() {
                        // The backend sent an invalid response → 502 Bad Gateway
                        set_default_answer(stream, &mut self.readiness, 502, &answers);
                    } else {
                        // The backend closed without sending a response → 503 Service Unavailable
                        // (matches kawa_h1: "Backend closed after consuming part of the request")
                        set_default_answer(stream, &mut self.readiness, 503, &answers);
                    }
                }
                (false, false) => {
                    // we do not have an answer, but the request is untouched so we can retry
                    debug!("H1 RECONNECT");
                    stream.state = StreamState::Link;
                }
                (false, true) => {
                    error!("backend response in main phase but request not yet consumed");
                }
            },
        }
    }

    pub fn start_stream<L>(&mut self, stream: GlobalStreamId, _context: &mut Context<L>) -> bool
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        trace!("start H1 stream {} {:?}", stream, self.readiness);
        self.readiness.interest.insert(Ready::ALL);
        self.stream = Some(stream);
        match &mut self.position {
            Position::Client(_, _, status @ BackendStatus::KeepAlive) => {
                *status = BackendStatus::Connected;
            }
            Position::Client(_, _, BackendStatus::Disconnecting) => {
                error!("start_stream called on Disconnecting H1 client");
                return false;
            }
            Position::Client(_, _, _) => {}
            Position::Server => {
                error!("start_stream must not be called on H1 server connection");
                return false;
            }
        }
        true
    }
}
