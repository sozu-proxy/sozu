use std::time::Instant;

use sozu_command::ready::Ready;

use crate::{
    L7ListenerHandler, ListenerHandler, Readiness,
    protocol::mux::{
        BackendStatus, Context, DebugEvent, Endpoint, GlobalStreamId, MuxResult, Position,
        StreamState, forcefully_terminate_answer, parser::H2Error, set_default_answer,
        update_readiness_after_read, update_readiness_after_write,
    },
    socket::SocketHandler,
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
    /// note: a Server H1 will always reference stream 0, but a client can reference any stream
    pub stream: GlobalStreamId,
    pub timeout_container: TimeoutContainer,
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
    pub fn readable<E, L>(&mut self, context: &mut Context<L>, mut endpoint: E) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        trace!("======= MUX H1 READABLE {:?}", self.position);
        self.timeout_container.reset();
        let answers_rc = context.listener.borrow().get_answers().clone();
        let stream = &mut context.streams[self.stream];
        if stream.metrics.start.is_none() {
            stream.metrics.start = Some(Instant::now());
        }
        let parts = stream.split(&self.position);
        let kawa = parts.rbuffer;
        let (size, status) = self.socket.socket_read(kawa.storage.space());
        context.debug.push(DebugEvent::StreamEvent(0, size));
        kawa.storage.fill(size);
        match self.position {
            Position::Client(..) => {
                count!("back_bytes_in", size as i64);
                parts.metrics.backend_bin += size;
            }
            Position::Server => {
                count!("bytes_in", size as i64);
                parts.metrics.bin += size;
            }
        }
        if update_readiness_after_read(size, status, &mut self.readiness) {
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
                    let global_stream_id = self.stream;
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
        MuxResult::Continue
    }

    pub fn writable<E, L>(&mut self, context: &mut Context<L>, mut endpoint: E) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        trace!("======= MUX H1 WRITABLE {:?}", self.position);
        self.timeout_container.reset();
        let stream = &mut context.streams[self.stream];
        let parts = stream.split(&self.position);
        let kawa = parts.wbuffer;
        kawa.prepare(&mut kawa::h1::BlockConverter);
        let bufs = kawa.as_io_slice();
        if bufs.is_empty() && !self.socket.socket_wants_write() {
            self.readiness.interest.remove(Ready::WRITABLE);
            return MuxResult::Continue;
        }
        let (size, status) = self.socket.socket_write_vectored(&bufs);
        context.debug.push(DebugEvent::StreamEvent(1, size));
        kawa.consume(size);
        match self.position {
            Position::Client(..) => {
                count!("back_bytes_out", size as i64);
                parts.metrics.backend_bout += size;
            }
            Position::Server => {
                count!("bytes_out", size as i64);
                parts.metrics.bout += size;
            }
        }
        if update_readiness_after_write(size, status, &mut self.readiness)
            || self.socket.socket_wants_write()
        {
            return MuxResult::Continue;
        }

        if kawa.is_terminated() && kawa.is_completed() {
            match self.position {
                Position::Client(..) => self.readiness.interest.insert(Ready::READABLE),
                Position::Server => {
                    if stream.context.closing {
                        return MuxResult::CloseSession;
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
                                return MuxResult::CloseSession;
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
                            endpoint.end_stream(token, self.stream, context);
                        }
                        self.readiness.interest.insert(Ready::READABLE);
                        let stream = &mut context.streams[self.stream];
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
                        return MuxResult::CloseSession;
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
                MuxResult::Continue
            }
            Position::Server => MuxResult::CloseSession,
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
                debug!("BACKEND CLOSING FOR: {:?} {}", self.position, self.stream);
            }
            Position::Server => {
                trace!("H1 SENDING CLOSE NOTIFY");
                self.socket.socket_close();
                let _ = self.socket.socket_write_vectored(&[]);
                return;
            }
        }
        // reconnection is handled by the server
        let StreamState::Linked(token) = context.streams[self.stream].state else {
            error!("closing H1 client is not in Linked state");
            return;
        };
        endpoint.end_stream(token, self.stream, context)
    }

    pub fn end_stream<L>(&mut self, stream: GlobalStreamId, context: &mut Context<L>)
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        if stream != self.stream {
            error!(
                "end_stream called with stream {} but expected {}",
                stream, self.stream
            );
            return;
        }
        let answers_rc = context.listener.borrow().get_answers().clone();
        let stream = &mut context.streams[stream];
        let stream_context = &mut stream.context;
        trace!("end H1 stream {}: {:#?}", self.stream, stream_context);
        match &mut self.position {
            Position::Client(_, _, BackendStatus::Connecting(_)) => {
                self.stream = usize::MAX;
                self.readiness.interest.remove(Ready::ALL);
                self.force_disconnect();
            }
            Position::Client(_, _, status @ BackendStatus::Connected) => {
                self.stream = usize::MAX;
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
        self.stream = stream;
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
