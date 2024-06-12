use sozu_command::ready::Ready;

use crate::{
    println_,
    protocol::mux::{
        debug_kawa, forcefully_terminate_answer, parser::H2Error, set_default_answer,
        update_readiness_after_read, update_readiness_after_write, BackendStatus, Context,
        Endpoint, GlobalStreamId, MuxResult, Position, StreamState,
    },
    socket::SocketHandler,
    timer::TimeoutContainer,
    L7ListenerHandler, ListenerHandler, Readiness,
};

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
        println_!("======= MUX H1 READABLE {:?}", self.position);
        self.timeout_container.reset();
        let stream = &mut context.streams[self.stream];
        let parts = stream.split(&self.position);
        let kawa = parts.rbuffer;
        let (size, status) = self.socket.socket_read(kawa.storage.space());
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
        debug_kawa(kawa);
        if kawa.is_error() {
            match self.position {
                Position::Client(..) => {
                    let StreamState::Linked(token) = stream.state else {
                        unreachable!()
                    };
                    let global_stream_id = self.stream;
                    self.readiness.interest.remove(Ready::ALL);
                    self.end_stream(global_stream_id, context);
                    endpoint.end_stream(token, global_stream_id, context);
                }
                Position::Server => {
                    set_default_answer(stream, &mut self.readiness, 400);
                }
            }
            return MuxResult::Continue;
        }
        if kawa.is_terminated() {
            self.timeout_container.cancel();
            self.readiness.interest.remove(Ready::READABLE);
        }
        if kawa.is_main_phase() {
            if let StreamState::Linked(token) = stream.state {
                endpoint
                    .readiness_mut(token)
                    .interest
                    .insert(Ready::WRITABLE)
            }
            if !was_main_phase && self.position.is_server() {
                self.requests += 1;
                println_!("REQUESTS: {}", self.requests);
                gauge_add!("http.active_requests", 1);
                stream.state = StreamState::Link
            }
        };
        MuxResult::Continue
    }

    pub fn writable<E, L>(&mut self, context: &mut Context<L>, mut endpoint: E) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        println_!("======= MUX H1 WRITABLE {:?}", self.position);
        self.timeout_container.reset();
        let stream = &mut context.streams[self.stream];
        let parts = stream.split(&self.position);
        let kawa = parts.wbuffer;
        kawa.prepare(&mut kawa::h1::BlockConverter);
        debug_kawa(kawa);
        let bufs = kawa.as_io_slice();
        if bufs.is_empty() {
            self.readiness.interest.remove(Ready::WRITABLE);
            return MuxResult::Continue;
        }
        let (size, status) = self.socket.socket_write_vectored(&bufs);
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
        if update_readiness_after_write(size, status, &mut self.readiness) {
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
                            println!("============== HANDLE UPGRADE!");
                            return MuxResult::Upgrade;
                        }
                        kawa::StatusLine::Response { code: 100, .. } => {
                            println!("============== HANDLE CONTINUE!");
                            // after a 100 continue, we expect the client to continue with its request
                            self.timeout_container.reset();
                            self.readiness.interest.insert(Ready::READABLE);
                            kawa.clear();
                            return MuxResult::Continue;
                        }
                        kawa::StatusLine::Response { code: 103, .. } => {
                            println!("============== HANDLE EARLY HINT!");
                            if let StreamState::Linked(token) = stream.state {
                                // after a 103 early hints, we expect the backend to send its response
                                endpoint
                                    .readiness_mut(token)
                                    .interest
                                    .insert(Ready::READABLE);
                                kawa.clear();
                                return MuxResult::Continue;
                            } else {
                                return MuxResult::CloseSession;
                            }
                        }
                        _ => {}
                    }
                    // ACCESS LOG
                    stream.generate_access_log(
                        false,
                        Some(String::from("H1")),
                        context.listener.clone(),
                    );
                    stream.metrics.reset();
                    let old_state = std::mem::replace(&mut stream.state, StreamState::Unlinked);
                    if stream.context.keep_alive_frontend {
                        self.timeout_container.reset();
                        // println!("{old_state:?} {:?}", self.readiness);
                        if let StreamState::Linked(token) = old_state {
                            // println!("{:?}", endpoint.readiness(token));
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
                println_!("close detached client ConnectionH1");
                return;
            }
            Position::Client(_, _, BackendStatus::Connecting(_))
            | Position::Client(_, _, BackendStatus::Connected) => {}
            Position::Server => unreachable!(),
        }
        // reconnection is handled by the server
        let StreamState::Linked(token) = context.streams[self.stream].state else {
            unreachable!()
        };
        endpoint.end_stream(token, self.stream, context)
    }

    pub fn end_stream<L>(&mut self, stream: GlobalStreamId, context: &mut Context<L>)
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        assert_eq!(stream, self.stream);
        let stream = &mut context.streams[stream];
        let stream_context = &mut stream.context;
        println_!("end H1 stream {}: {stream_context:#?}", self.stream);
        match &mut self.position {
            Position::Client(_, _, BackendStatus::Connecting(_)) => {
                self.stream = usize::MAX;
                self.force_disconnect();
            }
            Position::Client(_, _, status @ BackendStatus::Connected) => {
                self.stream = usize::MAX;
                // keep alive should probably be used only if the http context is fully reset
                // in case end_stream occurs due to an error the connection state is probably
                // unrecoverable and should be terminated
                if stream_context.keep_alive_backend {
                    *status = BackendStatus::KeepAlive;
                } else {
                    self.force_disconnect();
                }
            }
            Position::Client(_, _, BackendStatus::KeepAlive)
            | Position::Client(_, _, BackendStatus::Disconnecting) => unreachable!(),
            Position::Server => match (stream.front.consumed, stream.back.is_main_phase()) {
                (true, true) => {
                    // we have a "forwardable" answer from the back
                    // if the answer is not terminated we send an RstStream to properly clean the stream
                    // if it is terminated, we finish the transfer, the backend is not necessary anymore
                    if !stream.back.is_terminated() {
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
                    // so we can't retry, send a 502 bad gateway instead
                    set_default_answer(stream, &mut self.readiness, 502);
                }
                (false, false) => {
                    // we do not have an answer, but the request is untouched so we can retry
                    println!("H1 RECONNECT");
                    stream.state = StreamState::Link;
                }
                (false, true) => unreachable!(),
            },
        }
    }

    pub fn start_stream<L>(&mut self, stream: GlobalStreamId, context: &mut Context<L>)
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        println_!("start H1 stream {stream} {:?}", self.readiness);
        self.readiness.interest.insert(Ready::ALL);
        self.stream = stream;
        match &mut self.position {
            Position::Client(_, _, status @ BackendStatus::KeepAlive) => {
                *status = BackendStatus::Connected;
            }
            Position::Client(_, _, BackendStatus::Disconnecting) => unreachable!(),
            Position::Client(_, _, _) => {}
            Position::Server => unreachable!(),
        }
    }
}
