use sozu_command::ready::Ready;

use crate::{
    println_,
    protocol::mux::{
        debug_kawa, set_default_answer, update_readiness_after_read, update_readiness_after_write,
        BackendStatus, Context, Endpoint, GlobalStreamId, MuxResult, Position, StreamState, forcefully_terminate_answer,
    },
    socket::SocketHandler,
    Readiness,
};

pub struct ConnectionH1<Front: SocketHandler> {
    pub position: Position,
    pub readiness: Readiness,
    pub socket: Front,
    /// note: a Server H1 will always reference stream 0, but a client can reference any stream
    pub stream: GlobalStreamId,
    pub requests: usize,
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
    pub fn readable<E>(&mut self, context: &mut Context, mut endpoint: E) -> MuxResult
    where
        E: Endpoint,
    {
        println_!("======= MUX H1 READABLE {:?}", self.position);
        let stream = &mut context.streams[self.stream];
        let parts = stream.split(&self.position);
        let kawa = parts.rbuffer;
        let (size, status) = self.socket.socket_read(kawa.storage.space());
        kawa.storage.fill(size);
        if update_readiness_after_read(size, status, &mut self.readiness) {
            return MuxResult::Continue;
        }

        let was_initial = kawa.is_initial();
        kawa::h1::parse(kawa, parts.context);
        debug_kawa(kawa);
        if kawa.is_error() {
            match self.position {
                Position::Client(_) => {
                    let StreamState::Linked(token) = stream.state else { unreachable!() };
                    let global_stream_id = self.stream;
                    self.readiness.interest.remove(Ready::ALL);
                    self.end_stream(global_stream_id, context);
                    endpoint.end_stream(token, global_stream_id, context);
                }
                Position::Server => {
                    set_default_answer(&mut stream.back, &mut self.readiness, 400);
                    stream.state = StreamState::Unlinked;
                }
            }
            return MuxResult::Continue;
        }
        if kawa.is_terminated() {
            self.readiness.interest.remove(Ready::READABLE);
        }
        if was_initial && kawa.is_main_phase() {
            match self.position {
                Position::Client(_) => {
                    let StreamState::Linked(token) = stream.state else { unreachable!() };
                    endpoint
                        .readiness_mut(token)
                        .interest
                        .insert(Ready::WRITABLE)
                }
                Position::Server => {
                    self.requests += 1;
                    println_!("REQUESTS: {}", self.requests);
                    stream.state = StreamState::Link
                }
            };
        }
        MuxResult::Continue
    }

    pub fn writable<E>(&mut self, context: &mut Context, mut endpoint: E) -> MuxResult
    where
        E: Endpoint,
    {
        println_!("======= MUX H1 WRITABLE {:?}", self.position);
        let stream = &mut context.streams[self.stream];
        let kawa = stream.wbuffer(&self.position);
        kawa.prepare(&mut kawa::h1::BlockConverter);
        debug_kawa(kawa);
        let bufs = kawa.as_io_slice();
        if bufs.is_empty() {
            self.readiness.interest.remove(Ready::WRITABLE);
            return MuxResult::Continue;
        }
        let (size, status) = self.socket.socket_write_vectored(&bufs);
        kawa.consume(size);
        if update_readiness_after_write(size, status, &mut self.readiness) {
            return MuxResult::Continue;
        }

        if kawa.is_terminated() && kawa.is_completed() {
            match self.position {
                Position::Client(_) => self.readiness.interest.insert(Ready::READABLE),
                Position::Server => {
                    stream.context.reset();
                    stream.back.clear();
                    stream.back.storage.clear();
                    stream.front.clear();
                    // do not clear stream.front.storage because of H1 pipelining
                    if let StreamState::Linked(token) = stream.state {
                        endpoint.end_stream(token, self.stream, context);
                    }
                }
            }
        }
        MuxResult::Continue
    }

    pub fn close<E>(&mut self, context: &mut Context, mut endpoint: E)
    where
        E: Endpoint,
    {
        match self.position {
            Position::Client(BackendStatus::KeepAlive(_))
            | Position::Client(BackendStatus::Disconnecting) => {
                println_!("close detached client ConnectionH1");
                return;
            }
            Position::Client(BackendStatus::Connecting(_))
            | Position::Client(BackendStatus::Connected(_)) => {}
            Position::Server => unreachable!(),
        }
        // reconnection is handled by the server
        let StreamState::Linked(token) = context.streams[self.stream].state else {unreachable!()};
        endpoint.end_stream(token, self.stream, context)
    }

    pub fn end_stream(&mut self, stream: GlobalStreamId, context: &mut Context) {
        assert_eq!(stream, self.stream);
        let stream = &mut context.streams[stream];
        let stream_context = &mut stream.context;
        println_!("end H1 stream {}: {stream_context:#?}", self.stream);
        self.stream = usize::MAX;
        let mut owned_position = Position::Server;
        std::mem::swap(&mut owned_position, &mut self.position);
        match owned_position {
            Position::Client(BackendStatus::Connected(cluster_id))
            | Position::Client(BackendStatus::Connecting(cluster_id)) => {
                self.position = if stream_context.keep_alive_backend {
                    Position::Client(BackendStatus::KeepAlive(cluster_id))
                } else {
                    Position::Client(BackendStatus::Disconnecting)
                }
            }
            Position::Client(BackendStatus::KeepAlive(_))
            | Position::Client(BackendStatus::Disconnecting) => unreachable!(),
            Position::Server => match (stream.front.consumed, stream.back.is_main_phase()) {
                (true, true) => {
                    // we have a "forwardable" answer from the back
                    // if the answer is not terminated we send an RstStream to properly clean the stream
                    // if it is terminated, we finish the transfer, the backend is not necessary anymore
                    if !stream.back.is_terminated() {
                        forcefully_terminate_answer(&mut stream.back, &mut self.readiness);
                    }
                    stream.state = StreamState::Unlinked;
                }
                (true, false) => {
                    set_default_answer(&mut stream.back, &mut self.readiness, 502);
                    stream.state = StreamState::Unlinked;
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

    pub fn start_stream(&mut self, stream: GlobalStreamId, context: &mut Context) {
        println_!("start H1 stream {stream} {:?}", self.readiness);
        self.stream = stream;
        let mut owned_position = Position::Server;
        std::mem::swap(&mut owned_position, &mut self.position);
        match owned_position {
            Position::Client(BackendStatus::KeepAlive(cluster_id)) => {
                self.position = Position::Client(BackendStatus::Connecting(cluster_id))
            }
            Position::Server => unreachable!(),
            _ => {
                self.position = owned_position;
            }
        }
    }
}
