use std::{
    cell::RefCell,
    collections::HashMap,
    io::ErrorKind,
    net::{Shutdown, SocketAddr},
    rc::{Rc, Weak},
    time::Duration,
};

use mio::{net::TcpStream, Interest, Token};
use rusty_ulid::Ulid;
use sozu_command::{logging::EndpointRecord, proto::command::ListenerType, ready::Ready};

mod converter;
mod h1;
mod h2;
mod parser;
mod pkawa;
mod serializer;

use crate::{
    backends::{Backend, BackendError},
    http::HttpListener,
    https::HttpsListener,
    pool::{Checkout, Pool},
    protocol::{
        http::editor::HttpContext,
        mux::h2::{H2Settings, H2State, H2StreamId, Prioriser},
        SessionState,
    },
    router::Route,
    server::CONN_RETRIES,
    socket::{FrontRustls, SocketHandler, SocketResult},
    timer::TimeoutContainer,
    BackendConnectionError, L7ListenerHandler, L7Proxy, ListenerHandler, Protocol, ProxySession,
    Readiness, RetrieveClusterError, SessionIsToBeClosed, SessionMetrics, SessionResult,
    StateResult,
};

pub use crate::protocol::mux::{
    h1::ConnectionH1,
    h2::ConnectionH2,
    parser::{error_code_to_str, H2Error},
};

#[macro_export]
macro_rules! println_ {
    ($($t:expr),*) => {
        print!("{}:{} ", file!(), line!());
        println!($($t),*)
        // $(let _ = &$t;)*
    };
}
fn debug_kawa(_kawa: &GenericHttpStream) {
    // kawa::debug_kawa(_kawa);
}

/// Generic Http representation using the Kawa crate using the Checkout of Sozu as buffer
type GenericHttpStream = kawa::Kawa<Checkout>;
type StreamId = u32;
type GlobalStreamId = usize;
pub type MuxClear = Mux<TcpStream, HttpListener>;
pub type MuxTls = Mux<FrontRustls, HttpsListener>;

pub fn fill_default_301_answer<T: kawa::AsBuffer>(kawa: &mut kawa::Kawa<T>, host: &str, uri: &str) {
    kawa.detached.status_line = kawa::StatusLine::Response {
        version: kawa::Version::V20,
        code: 301,
        status: kawa::Store::Static(b"301"),
        reason: kawa::Store::Static(b"Moved Permanently"),
    };
    kawa.push_block(kawa::Block::StatusLine);
    kawa.push_block(kawa::Block::Header(kawa::Pair {
        key: kawa::Store::Static(b"Location"),
        val: kawa::Store::from_string(format!("https://{host}{uri}")),
    }));
    terminate_default_answer(kawa, false);
}

pub fn fill_default_answer<T: kawa::AsBuffer>(kawa: &mut kawa::Kawa<T>, code: u16) {
    kawa.detached.status_line = kawa::StatusLine::Response {
        version: kawa::Version::V20,
        code,
        status: kawa::Store::from_string(code.to_string()),
        reason: kawa::Store::Static(b"Sozu Default Answer"),
    };
    kawa.push_block(kawa::Block::StatusLine);
    terminate_default_answer(kawa, true);
}

pub fn terminate_default_answer<T: kawa::AsBuffer>(kawa: &mut kawa::Kawa<T>, close: bool) {
    if close {
        kawa.push_block(kawa::Block::Header(kawa::Pair {
            key: kawa::Store::Static(b"Cache-Control"),
            val: kawa::Store::Static(b"no-cache"),
        }));
        kawa.push_block(kawa::Block::Header(kawa::Pair {
            key: kawa::Store::Static(b"Connection"),
            val: kawa::Store::Static(b"close"),
        }));
    }
    kawa.push_block(kawa::Block::Header(kawa::Pair {
        key: kawa::Store::Static(b"Content-Length"),
        val: kawa::Store::Static(b"0"),
    }));
    kawa.push_block(kawa::Block::Flags(kawa::Flags {
        end_body: false,
        end_chunk: false,
        end_header: true,
        end_stream: true,
    }));
    kawa.parsing_phase = kawa::ParsingPhase::Terminated;
}

/// Replace the content of the kawa message with a default Sozu answer for a given status code
fn set_default_answer(stream: &mut Stream, readiness: &mut Readiness, code: u16) {
    let kawa = &mut stream.back;
    kawa.clear();
    kawa.storage.clear();
    if code == 301 {
        let host = stream.context.authority.as_deref().unwrap();
        let uri = stream.context.path.as_deref().unwrap();
        fill_default_301_answer(kawa, host, uri);
    } else {
        fill_default_answer(kawa, code);
    }
    stream.context.status = Some(code);
    stream.state = StreamState::Unlinked;
    readiness.interest.insert(Ready::WRITABLE);
}

/// Forcefully terminates a kawa message by setting the "end_stream" flag and setting the parsing_phase to Error.
/// An H2 converter will produce an RstStream frame.
fn forcefully_terminate_answer(stream: &mut Stream, readiness: &mut Readiness, error: H2Error) {
    let kawa = &mut stream.back;
    kawa.out.clear();
    kawa.blocks.clear();
    // kawa.push_block(kawa::Block::Flags(kawa::Flags {
    //     end_body: false,
    //     end_chunk: false,
    //     end_header: false,
    //     end_stream: true,
    // }));
    kawa.parsing_phase
        .error(error_code_to_str(error as u32).into());
    debug_kawa(kawa);
    stream.state = StreamState::Unlinked;
    readiness.interest.insert(Ready::WRITABLE);
}

#[derive(Debug)]
pub enum Position {
    Client(BackendStatus),
    Server,
}

impl Position {
    fn is_server(&self) -> bool {
        match self {
            Position::Client(_) => false,
            Position::Server => true,
        }
    }
    fn is_client(&self) -> bool {
        match self {
            Position::Client(_) => true,
            Position::Server => false,
        }
    }
}

#[derive(Debug)]
pub enum BackendStatus {
    Connecting(String),
    Connected(String),
    KeepAlive(String),
    Disconnecting,
}

pub enum MuxResult {
    Continue,
    Upgrade,
    CloseSession,
}

pub trait Endpoint {
    fn readiness(&self, token: Token) -> &Readiness;
    fn readiness_mut(&mut self, token: Token) -> &mut Readiness;
    /// If end_stream is called on a client it means the stream has PROPERLY finished,
    /// the server has completed serving the response and informs the endpoint that this stream won't be used anymore.
    /// If end_stream is called on a server it means the stream was BROKEN, the client was most likely disconnected or encountered an error
    /// it is for the server to decide if the stream can be retried or an error should be sent. It should be GUARANTEED that all bytes from
    /// the backend were read. However it is almost certain that all bytes were not already sent to the client.
    fn end_stream<L: ListenerHandler + L7ListenerHandler>(
        &mut self,
        token: Token,
        stream: GlobalStreamId,
        context: &mut Context<L>,
    );
    /// If start_stream is called on a client it means the stream should be attached to this endpoint,
    /// the stream might be recovering from a disconnection, in any case at this point its response MUST be empty.
    /// If the start_stream is called on a H2 server it means the stream is a server push and its request MUST be empty.
    fn start_stream<L: ListenerHandler + L7ListenerHandler>(
        &mut self,
        token: Token,
        stream: GlobalStreamId,
        context: &mut Context<L>,
    );
}

#[derive(Debug)]
pub enum Connection<Front: SocketHandler> {
    H1(ConnectionH1<Front>),
    H2(ConnectionH2<Front>),
}

impl<Front: SocketHandler> Connection<Front> {
    pub fn new_h1_server(
        front_stream: Front,
        timeout_container: TimeoutContainer,
    ) -> Connection<Front> {
        Connection::H1(ConnectionH1 {
            socket: front_stream,
            position: Position::Server,
            readiness: Readiness {
                interest: Ready::READABLE | Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            requests: 0,
            stream: 0,
            timeout_container,
        })
    }
    pub fn new_h1_client(
        front_stream: Front,
        cluster_id: String,
        timeout_container: TimeoutContainer,
    ) -> Connection<Front> {
        Connection::H1(ConnectionH1 {
            socket: front_stream,
            position: Position::Client(BackendStatus::Connecting(cluster_id)),
            readiness: Readiness {
                interest: Ready::WRITABLE | Ready::READABLE | Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            stream: usize::MAX - 1,
            requests: 0,
            timeout_container,
        })
    }

    pub fn new_h2_server(
        front_stream: Front,
        pool: Weak<RefCell<Pool>>,
        timeout_container: TimeoutContainer,
    ) -> Option<Connection<Front>> {
        let buffer = pool
            .upgrade()
            .and_then(|pool| pool.borrow_mut().checkout())?;
        Some(Connection::H2(ConnectionH2 {
            decoder: hpack::Decoder::new(),
            encoder: hpack::Encoder::new(),
            expect_read: Some((H2StreamId::Zero, 24 + 9)),
            expect_write: None,
            last_stream_id: 0,
            local_settings: H2Settings::default(),
            peer_settings: H2Settings::default(),
            position: Position::Server,
            prioriser: Prioriser::new(),
            readiness: Readiness {
                interest: Ready::READABLE | Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            socket: front_stream,
            state: H2State::ClientPreface,
            streams: HashMap::new(),
            timeout_container,
            window: (1 << 16) - 1,
            zero: kawa::Kawa::new(kawa::Kind::Request, kawa::Buffer::new(buffer)),
        }))
    }
    pub fn new_h2_client(
        front_stream: Front,
        cluster_id: String,
        pool: Weak<RefCell<Pool>>,
        timeout_container: TimeoutContainer,
    ) -> Option<Connection<Front>> {
        let buffer = pool
            .upgrade()
            .and_then(|pool| pool.borrow_mut().checkout())?;
        Some(Connection::H2(ConnectionH2 {
            decoder: hpack::Decoder::new(),
            encoder: hpack::Encoder::new(),
            expect_read: None,
            expect_write: None,
            last_stream_id: 0,
            local_settings: H2Settings::default(),
            peer_settings: H2Settings::default(),
            position: Position::Client(BackendStatus::Connecting(cluster_id)),
            prioriser: Prioriser::new(),
            readiness: Readiness {
                interest: Ready::WRITABLE | Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            socket: front_stream,
            state: H2State::ClientPreface,
            streams: HashMap::new(),
            timeout_container,
            window: (1 << 16) - 1,
            zero: kawa::Kawa::new(kawa::Kind::Request, kawa::Buffer::new(buffer)),
        }))
    }

    pub fn readiness(&self) -> &Readiness {
        match self {
            Connection::H1(c) => &c.readiness,
            Connection::H2(c) => &c.readiness,
        }
    }
    pub fn readiness_mut(&mut self) -> &mut Readiness {
        match self {
            Connection::H1(c) => &mut c.readiness,
            Connection::H2(c) => &mut c.readiness,
        }
    }
    pub fn position(&self) -> &Position {
        match self {
            Connection::H1(c) => &c.position,
            Connection::H2(c) => &c.position,
        }
    }
    pub fn position_mut(&mut self) -> &mut Position {
        match self {
            Connection::H1(c) => &mut c.position,
            Connection::H2(c) => &mut c.position,
        }
    }
    pub fn socket(&self) -> &TcpStream {
        match self {
            Connection::H1(c) => c.socket.socket_ref(),
            Connection::H2(c) => c.socket.socket_ref(),
        }
    }
    pub fn socket_mut(&mut self) -> &mut TcpStream {
        match self {
            Connection::H1(c) => c.socket.socket_mut(),
            Connection::H2(c) => c.socket.socket_mut(),
        }
    }
    pub fn timeout_container(&mut self) -> &mut TimeoutContainer {
        match self {
            Connection::H1(c) => &mut c.timeout_container,
            Connection::H2(c) => &mut c.timeout_container,
        }
    }
    fn force_disconnect(&mut self) -> MuxResult {
        match self {
            Connection::H1(c) => c.force_disconnect(),
            Connection::H2(c) => c.force_disconnect(),
        }
    }

    fn readable<E, L>(&mut self, context: &mut Context<L>, endpoint: E) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        match self {
            Connection::H1(c) => c.readable(context, endpoint),
            Connection::H2(c) => c.readable(context, endpoint),
        }
    }
    fn writable<E, L>(&mut self, context: &mut Context<L>, endpoint: E) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        match self {
            Connection::H1(c) => c.writable(context, endpoint),
            Connection::H2(c) => c.writable(context, endpoint),
        }
    }

    fn close<E, L>(&mut self, context: &mut Context<L>, endpoint: E)
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        match self {
            Connection::H1(c) => c.close(context, endpoint),
            Connection::H2(c) => c.close(context, endpoint),
        }
    }

    fn end_stream<L>(&mut self, stream: GlobalStreamId, context: &mut Context<L>)
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        match self {
            Connection::H1(c) => c.end_stream(stream, context),
            Connection::H2(c) => c.end_stream(stream, context),
        }
    }

    fn start_stream<L>(&mut self, stream: GlobalStreamId, context: &mut Context<L>)
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        match self {
            Connection::H1(c) => c.start_stream(stream, context),
            Connection::H2(c) => c.start_stream(stream, context),
        }
    }
}

struct EndpointServer<'a, Front: SocketHandler>(&'a mut Connection<Front>);
struct EndpointClient<'a>(&'a mut Router);

// note: EndpointServer are used by client Connection, they do not know the frontend Token
// they will use the Stream's Token which is their backend token
impl<'a, Front: SocketHandler> Endpoint for EndpointServer<'a, Front> {
    fn readiness(&self, _token: Token) -> &Readiness {
        self.0.readiness()
    }
    fn readiness_mut(&mut self, _token: Token) -> &mut Readiness {
        self.0.readiness_mut()
    }

    fn end_stream<L>(&mut self, token: Token, stream: GlobalStreamId, context: &mut Context<L>)
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        // this may be used to forward H2<->H2 RstStream
        // or to handle backend hup
        self.0.end_stream(stream, context);
    }

    fn start_stream<L>(&mut self, token: Token, stream: GlobalStreamId, context: &mut Context<L>)
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        // this may be used to forward H2<->H2 PushPromise
        todo!()
        // self.0.start_stream(stream, context);
    }
}
impl<'a> Endpoint for EndpointClient<'a> {
    fn readiness(&self, token: Token) -> &Readiness {
        self.0.backends.get(&token).unwrap().readiness()
    }
    fn readiness_mut(&mut self, token: Token) -> &mut Readiness {
        self.0.backends.get_mut(&token).unwrap().readiness_mut()
    }

    fn end_stream<L>(&mut self, token: Token, stream: GlobalStreamId, context: &mut Context<L>)
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        self.0
            .backends
            .get_mut(&token)
            .unwrap()
            .end_stream(stream, context);
    }

    fn start_stream<L>(&mut self, token: Token, stream: GlobalStreamId, context: &mut Context<L>)
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        self.0
            .backends
            .get_mut(&token)
            .unwrap()
            .start_stream(stream, context);
    }
}

fn update_readiness_after_read(
    size: usize,
    status: SocketResult,
    readiness: &mut Readiness,
) -> bool {
    println_!("  size={size}, status={status:?}");
    match status {
        SocketResult::Continue => {}
        SocketResult::Closed | SocketResult::Error => {
            readiness.event.remove(Ready::ALL);
        }
        SocketResult::WouldBlock => {
            readiness.event.remove(Ready::READABLE);
        }
    }
    if size > 0 {
        false
    } else {
        readiness.event.remove(Ready::READABLE);
        true
    }
}
fn update_readiness_after_write(
    size: usize,
    status: SocketResult,
    readiness: &mut Readiness,
) -> bool {
    println_!("  size={size}, status={status:?}");
    match status {
        SocketResult::Continue => {}
        SocketResult::Closed | SocketResult::Error => {
            // even if the socket closed there might be something left to read
            readiness.event.remove(Ready::WRITABLE);
        }
        SocketResult::WouldBlock => {
            readiness.event.remove(Ready::WRITABLE);
        }
    }
    if size > 0 {
        false
    } else {
        readiness.event.remove(Ready::WRITABLE);
        true
    }
}

// enum Stream {
//     Idle {
//         window: i32,
//     },
//     Open {
//         window: i32,
//         token: Token,
//         front: GenericHttpStream,
//         back: GenericHttpStream,
//         context: HttpContext,
//     },
//     Reserved {
//         window: i32,
//         token: Token,
//         position: Position,
//         buffer: GenericHttpStream,
//         context: HttpContext,
//     },
//     HalfClosed {
//         window: i32,
//         token: Token,
//         position: Position,
//         buffer: GenericHttpStream,
//         context: HttpContext,
//     },
//     Closed,
// }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    Idle,
    /// the Stream is asking for connection, this will trigger a call to connect
    Link,
    /// the Stream is linked to a Client (note that the client might not be connected)
    Linked(Token),
    /// the Stream was linked to a Client, but the connection closed, the client was removed
    /// and this Stream could not be retried (it should be terminated)
    Unlinked,
    /// the Stream is unlinked and can be reused
    Recycle,
}

impl StreamState {
    fn is_open(&self) -> bool {
        match self {
            StreamState::Idle | StreamState::Recycle => false,
            _ => true,
        }
    }
}

pub struct Stream {
    // pub request_id: Ulid,
    pub window: i32,
    pub attempts: u8,
    pub state: StreamState,
    pub received_end_of_stream: bool,
    pub front: GenericHttpStream,
    pub back: GenericHttpStream,
    pub context: HttpContext,
}

/// This struct allows to mutably borrow the read and write buffers (dependant on the position)
/// as well as the context of a Stream at the same time
pub struct StreamParts<'a> {
    pub window: &'a mut i32,
    pub rbuffer: &'a mut GenericHttpStream,
    pub wbuffer: &'a mut GenericHttpStream,
    pub context: &'a mut HttpContext,
}

impl Stream {
    pub fn new(pool: Weak<RefCell<Pool>>, context: HttpContext, window: u32) -> Option<Self> {
        let (front_buffer, back_buffer) = match pool.upgrade() {
            Some(pool) => {
                let mut pool = pool.borrow_mut();
                match (pool.checkout(), pool.checkout()) {
                    (Some(front_buffer), Some(back_buffer)) => (front_buffer, back_buffer),
                    _ => return None,
                }
            }
            None => return None,
        };
        Some(Self {
            state: StreamState::Idle,
            attempts: 0,
            window: window as i32,
            received_end_of_stream: false,
            front: GenericHttpStream::new(kawa::Kind::Request, kawa::Buffer::new(front_buffer)),
            back: GenericHttpStream::new(kawa::Kind::Response, kawa::Buffer::new(back_buffer)),
            context,
        })
    }
    pub fn split(&mut self, position: &Position) -> StreamParts<'_> {
        match position {
            Position::Client(_) => StreamParts {
                window: &mut self.window,
                rbuffer: &mut self.back,
                wbuffer: &mut self.front,
                context: &mut self.context,
            },
            Position::Server => StreamParts {
                window: &mut self.window,
                rbuffer: &mut self.front,
                wbuffer: &mut self.back,
                context: &mut self.context,
            },
        }
    }
    pub fn rbuffer(&mut self, position: &Position) -> &mut GenericHttpStream {
        match position {
            Position::Client(_) => &mut self.back,
            Position::Server => &mut self.front,
        }
    }
    pub fn wbuffer(&mut self, position: &Position) -> &mut GenericHttpStream {
        match position {
            Position::Client(_) => &mut self.front,
            Position::Server => &mut self.back,
        }
    }
    pub fn generate_access_log<L>(
        &mut self,
        error: bool,
        message: Option<String>,
        listener: Rc<RefCell<L>>,
    ) where
        L: ListenerHandler + L7ListenerHandler,
    {
        let protocol = match self.context.protocol {
            Protocol::HTTP => "http",
            Protocol::HTTPS => "https",
            _ => unreachable!(),
        };

        let endpoint = EndpointRecord::Http {
            method: self.context.method.as_deref(),
            authority: self.context.authority.as_deref(),
            path: self.context.path.as_deref(),
            reason: self.context.reason.as_deref(),
            status: self.context.status,
        };

        let listener = listener.borrow();
        let tags = self.context.authority.as_deref().and_then(|host| {
            let hostname = match host.split_once(':') {
                None => host,
                Some((hostname, _)) => hostname,
            };
            listener.get_tags(hostname)
        });

        log_access! {
            error,
            on_failure: { incr!("unsent-access-logs") },
            message: message.as_deref(),
            context: self.context.log_context(),
            session_address: self.context.session_address,
            backend_address: self.context.backend_address,
            protocol,
            endpoint,
            tags,
            client_rtt: None, //socket_rtt(self.front_socket()),
            server_rtt: None, //self.backend_socket.as_ref().and_then(socket_rtt),
            service_time: Duration::from_micros(0), //metrics.service_time(),
            response_time: Some(Duration::from_micros(0)), //metrics.response_time(),
            request_time: Duration::from_micros(0), // metrics.request_time(),
            bytes_in: 0, //metrics.bin,
            bytes_out: 0, //metrics.bout,
            user_agent: self.context.user_agent.as_deref(),
        };
    }
}

pub struct Context<L: ListenerHandler + L7ListenerHandler> {
    pub streams: Vec<Stream>,
    pub pool: Weak<RefCell<Pool>>,
    pub listener: Rc<RefCell<L>>,
    pub session_address: Option<SocketAddr>,
    pub public_address: SocketAddr,
}

impl<L: ListenerHandler + L7ListenerHandler> Context<L> {
    pub fn new(
        pool: Weak<RefCell<Pool>>,
        listener: Rc<RefCell<L>>,
        session_address: Option<SocketAddr>,
        public_address: SocketAddr,
    ) -> Self {
        Self {
            streams: Vec::new(),
            pool,
            listener,
            session_address,
            public_address,
        }
    }

    pub fn create_stream(&mut self, request_id: Ulid, window: u32) -> Option<GlobalStreamId> {
        let listener = self.listener.borrow();
        let http_context = HttpContext::new(
            request_id,
            listener.protocol(),
            listener.sticky_name().to_string(),
            self.public_address,
            self.session_address,
        );
        for (stream_id, stream) in self.streams.iter_mut().enumerate() {
            if stream.state == StreamState::Recycle {
                println_!("Reuse stream: {stream_id}");
                stream.state = StreamState::Idle;
                stream.attempts = 0;
                stream.window = window as i32;
                stream.context = http_context;
                stream.back.clear();
                stream.back.storage.clear();
                stream.front.clear();
                stream.front.storage.clear();
                return Some(stream_id);
            }
        }
        self.streams
            .push(Stream::new(self.pool.clone(), http_context, window)?);
        Some(self.streams.len() - 1)
    }
}

pub struct Router {
    pub backends: HashMap<Token, Connection<TcpStream>>,
    pub configured_backend_timeout: Duration,
    pub configured_connect_timeout: Duration,
}

impl Router {
    pub fn new(configured_backend_timeout: Duration, configured_connect_timeout: Duration) -> Self {
        Self {
            backends: HashMap::new(),
            configured_backend_timeout,
            configured_connect_timeout,
        }
    }

    fn connect<L: ListenerHandler + L7ListenerHandler, P: L7Proxy>(
        &mut self,
        stream_id: GlobalStreamId,
        context: &mut Context<L>,
        session: Rc<RefCell<dyn ProxySession>>,
        proxy: Rc<RefCell<P>>,
        metrics: &mut SessionMetrics,
    ) -> Result<(), BackendConnectionError> {
        let stream = &mut context.streams[stream_id];
        // when reused, a stream should be detached from its old connection, if not we could end
        // with concurrent connections on a single endpoint
        assert!(matches!(stream.state, StreamState::Link));
        if stream.attempts >= CONN_RETRIES {
            return Err(BackendConnectionError::MaxConnectionRetries(
                stream.context.cluster_id.clone(),
            ));
        }
        stream.attempts += 1;

        let stream_context = &mut stream.context;
        let cluster_id = self
            .route_from_request(stream_context, &context.listener)
            .map_err(BackendConnectionError::RetrieveClusterError)?;
        stream_context.cluster_id = Some(cluster_id.clone());

        let (frontend_should_stick, frontend_should_redirect_https, h2) = proxy
            .borrow()
            .clusters()
            .get(&cluster_id)
            .map(|cluster| {
                (
                    cluster.sticky_session,
                    cluster.https_redirect,
                    cluster.http2,
                )
            })
            .unwrap_or((false, false, false));

        if frontend_should_redirect_https && matches!(proxy.borrow().kind(), ListenerType::Http) {
            return Err(BackendConnectionError::RetrieveClusterError(
                RetrieveClusterError::HttpsRedirect,
            ));
        }

        /*
        Current h2 connecting strategy:
        - look at every backend connection
        - reuse the first connected backend that belongs to the same cluster
        - or, reuse the last connecting backend that belonds to the same cluster
        - if no backend is to reuse, ask the router for a socket to the "next in line" backend for that cluster

        We may want to change to:
        - ask the router the name of the "next in line" backend for that cluster
        - if we already have a backend connected to this name, reuse it
        - if not, create a new socket to it
         */

        let mut reuse_token = None;
        // let mut priority = 0;
        let mut reuse_connecting = true;
        for (token, backend) in &self.backends {
            match (h2, reuse_connecting, backend.position()) {
                (_, _, Position::Server) => {
                    unreachable!("Backend connection behaves like a server")
                }
                (_, _, Position::Client(BackendStatus::Disconnecting)) => {}
                (true, false, Position::Client(BackendStatus::Connecting(_))) => {}

                (true, _, Position::Client(BackendStatus::Connected(other_cluster_id))) => {
                    if *other_cluster_id == cluster_id {
                        reuse_token = Some(*token);
                        reuse_connecting = false;
                        break;
                    }
                }
                (true, true, Position::Client(BackendStatus::Connecting(other_cluster_id))) => {
                    if *other_cluster_id == cluster_id {
                        reuse_token = Some(*token)
                    }
                }
                (true, _, Position::Client(BackendStatus::KeepAlive(other_cluster_id))) => {
                    if *other_cluster_id == cluster_id {
                        unreachable!("ConnectionH2 behaves like H1")
                    }
                }

                (false, _, Position::Client(BackendStatus::KeepAlive(old_cluster_id))) => {
                    if *old_cluster_id == cluster_id {
                        reuse_token = Some(*token);
                        reuse_connecting = false;
                        break;
                    }
                }
                // can't bundle H1 streams together
                (false, _, Position::Client(BackendStatus::Connected(_)))
                | (false, _, Position::Client(BackendStatus::Connecting(_))) => {}
            }
        }
        println_!("connect: {cluster_id} (stick={frontend_should_stick}, h2={h2}) -> (reuse={reuse_token:?})");

        let token = if let Some(token) = reuse_token {
            println_!("reused backend: {:#?}", self.backends.get(&token).unwrap());
            token
        } else {
            let mut socket = self.backend_from_request(
                &cluster_id,
                frontend_should_stick,
                stream_context,
                proxy.clone(),
                &context.listener,
                metrics,
            )?;

            if let Err(e) = socket.set_nodelay(true) {
                error!(
                    "error setting nodelay on back socket({:?}): {:?}",
                    socket, e
                );
            }
            // self.backend_readiness.interest = Ready::WRITABLE | Ready::HUP | Ready::ERROR;
            // self.backend_connection_status = BackendConnectionStatus::Connecting(Instant::now());

            let token = proxy.borrow().add_session(session);

            if let Err(e) = proxy.borrow().register_socket(
                &mut socket,
                token,
                Interest::READABLE | Interest::WRITABLE,
            ) {
                error!("error registering back socket({:?}): {:?}", socket, e);
            }

            let timeout_container = TimeoutContainer::new(self.configured_connect_timeout, token);
            let connection = if h2 {
                match Connection::new_h2_client(
                    socket,
                    cluster_id,
                    context.pool.clone(),
                    timeout_container,
                ) {
                    Some(connection) => connection,
                    None => return Err(BackendConnectionError::MaxBuffers),
                }
            } else {
                Connection::new_h1_client(socket, cluster_id, timeout_container)
            };
            self.backends.insert(token, connection);
            token
        };

        // link stream to backend
        stream.state = StreamState::Linked(token);
        // link backend to stream
        self.backends
            .get_mut(&token)
            .unwrap()
            .start_stream(stream_id, context);
        Ok(())
    }

    fn route_from_request<L: ListenerHandler + L7ListenerHandler>(
        &mut self,
        context: &mut HttpContext,
        listener: &Rc<RefCell<L>>,
    ) -> Result<String, RetrieveClusterError> {
        let (host, uri, method) = match context.extract_route() {
            Ok(tuple) => tuple,
            Err(cluster_error) => {
                // we are past kawa parsing if it succeeded this can't fail
                // if the request was malformed it was caught by kawa and we sent a 400
                panic!("{cluster_error}");
            }
        };

        let route_result = listener.borrow().frontend_from_request(host, uri, method);

        let route = match route_result {
            Ok(route) => route,
            Err(frontend_error) => {
                println!("{}", frontend_error);
                // self.set_answer(DefaultAnswerStatus::Answer404, None);
                return Err(RetrieveClusterError::RetrieveFrontend(frontend_error));
            }
        };

        let cluster_id = match route {
            Route::Cluster(id) => id,
            Route::Deny => {
                println!("Route::Deny");
                // self.set_answer(DefaultAnswerStatus::Answer401, None);
                return Err(RetrieveClusterError::UnauthorizedRoute);
            }
        };

        Ok(cluster_id)
    }

    pub fn backend_from_request<L: ListenerHandler + L7ListenerHandler, P: L7Proxy>(
        &mut self,
        cluster_id: &str,
        frontend_should_stick: bool,
        context: &mut HttpContext,
        proxy: Rc<RefCell<P>>,
        listener: &Rc<RefCell<L>>,
        _metrics: &mut SessionMetrics,
    ) -> Result<TcpStream, BackendConnectionError> {
        let (backend, conn) = self
            .get_backend_for_sticky_session(
                cluster_id,
                frontend_should_stick,
                context.sticky_session_found.as_deref(),
                proxy,
            )
            .map_err(|backend_error| {
                println!("{backend_error}");
                // self.set_answer(DefaultAnswerStatus::Answer503, None);
                BackendConnectionError::Backend(backend_error)
            })?;

        if frontend_should_stick {
            // update sticky name in case it changed I guess?
            context.sticky_name = listener.borrow().sticky_name().to_string();

            context.sticky_session = Some(
                backend
                    .borrow()
                    .sticky_id
                    .clone()
                    .unwrap_or_else(|| backend.borrow().backend_id.clone()),
            );
        }

        // metrics.backend_id = Some(backend.borrow().backend_id.clone());
        // metrics.backend_start();
        // self.set_backend_id(backend.borrow().backend_id.clone());
        // self.backend = Some(backend);
        context.backend_id = Some(backend.borrow().backend_id.clone());
        context.backend_address = Some(backend.borrow().address);

        Ok(conn)
    }

    fn get_backend_for_sticky_session<P: L7Proxy>(
        &self,
        cluster_id: &str,
        frontend_should_stick: bool,
        sticky_session: Option<&str>,
        proxy: Rc<RefCell<P>>,
    ) -> Result<(Rc<RefCell<Backend>>, TcpStream), BackendError> {
        match (frontend_should_stick, sticky_session) {
            (true, Some(sticky_session)) => proxy
                .borrow()
                .backends()
                .borrow_mut()
                .backend_from_sticky_session(cluster_id, sticky_session),
            _ => proxy
                .borrow()
                .backends()
                .borrow_mut()
                .backend_from_cluster_id(cluster_id),
        }
    }
}

pub struct Mux<Front: SocketHandler, L: ListenerHandler + L7ListenerHandler> {
    pub configured_frontend_timeout: Duration,
    pub frontend_token: Token,
    pub frontend: Connection<Front>,
    pub router: Router,
    pub context: Context<L>,
}

impl<Front: SocketHandler, L: ListenerHandler + L7ListenerHandler> Mux<Front, L> {
    pub fn front_socket(&self) -> &TcpStream {
        self.frontend.socket()
    }
}

impl<Front: SocketHandler + std::fmt::Debug, L: ListenerHandler + L7ListenerHandler> SessionState
    for Mux<Front, L>
{
    fn ready<P: L7Proxy>(
        &mut self,
        session: Rc<RefCell<dyn ProxySession>>,
        proxy: Rc<RefCell<P>>,
        metrics: &mut SessionMetrics,
    ) -> SessionResult {
        let mut counter = 0;
        let max_loop_iterations = 100000;

        if self.frontend.readiness().event.is_hup() {
            return SessionResult::Close;
        }

        let start = std::time::Instant::now();
        println_!("{start:?}");
        loop {
            loop {
                let context = &mut self.context;
                if self.frontend.readiness().filter_interest().is_readable() {
                    match self
                        .frontend
                        .readable(context, EndpointClient(&mut self.router))
                    {
                        MuxResult::Continue => {}
                        MuxResult::CloseSession => return SessionResult::Close,
                        MuxResult::Upgrade => return SessionResult::Upgrade,
                    }
                }

                let mut all_backends_readiness_are_empty = true;
                let context = &mut self.context;
                let mut dead_backends = Vec::new();
                for (token, backend) in self.router.backends.iter_mut() {
                    let readiness = backend.readiness_mut();
                    // println!("{token:?} -> {readiness:?}");
                    let dead = readiness.filter_interest().is_hup()
                        || readiness.filter_interest().is_error();
                    if dead {
                        println_!("Backend({token:?}) -> {readiness:?}");
                        readiness.event.remove(Ready::WRITABLE);
                    }

                    if backend.readiness().filter_interest().is_writable() {
                        let position = backend.position_mut();
                        match position {
                            Position::Client(BackendStatus::Connecting(cluster_id)) => {
                                *position = Position::Client(BackendStatus::Connected(
                                    std::mem::take(cluster_id),
                                ));
                                backend
                                    .timeout_container()
                                    .set_duration(self.router.configured_backend_timeout);
                            }
                            _ => {}
                        }
                        match backend.writable(context, EndpointServer(&mut self.frontend)) {
                            MuxResult::Continue => {}
                            MuxResult::Upgrade => unreachable!(), // only frontend can upgrade
                            MuxResult::CloseSession => return SessionResult::Close,
                        }
                    }

                    if backend.readiness().filter_interest().is_readable() {
                        match backend.readable(context, EndpointServer(&mut self.frontend)) {
                            MuxResult::Continue => {}
                            MuxResult::Upgrade => unreachable!(), // only frontend can upgrade
                            MuxResult::CloseSession => return SessionResult::Close,
                        }
                    }

                    if dead && !backend.readiness().filter_interest().is_readable() {
                        println_!("Closing {:#?}", backend);
                        backend.close(context, EndpointServer(&mut self.frontend));
                        dead_backends.push(*token);
                    }

                    if !backend.readiness().filter_interest().is_empty() {
                        all_backends_readiness_are_empty = false;
                    }
                }
                if !dead_backends.is_empty() {
                    for token in &dead_backends {
                        let proxy_borrow = proxy.borrow();
                        if let Some(mut backend) = self.router.backends.remove(token) {
                            backend.timeout_container().cancel();
                            let socket = backend.socket_mut();
                            if let Err(e) = proxy_borrow.deregister_socket(socket) {
                                error!("error deregistering back socket({:?}): {:?}", socket, e);
                            }
                            if let Err(e) = socket.shutdown(Shutdown::Both) {
                                if e.kind() != ErrorKind::NotConnected {
                                    error!(
                                        "error shutting down back socket({:?}): {:?}",
                                        socket, e
                                    );
                                }
                            }
                        } else {
                            error!("session {:?} has no backend!", token);
                        }
                        if !proxy_borrow.remove_session(*token) {
                            error!("session {:?} was already removed!", token);
                        } else {
                            // println!("SUCCESS: session {token:?} was removed!");
                        }
                    }
                    println_!("FRONTEND: {:#?}", self.frontend);
                    println_!("BACKENDS: {:#?}", self.router.backends);
                }

                let context = &mut self.context;
                if self.frontend.readiness().filter_interest().is_writable() {
                    match self
                        .frontend
                        .writable(context, EndpointClient(&mut self.router))
                    {
                        MuxResult::Continue => {}
                        MuxResult::CloseSession => return SessionResult::Close,
                        MuxResult::Upgrade => return SessionResult::Upgrade,
                    }
                }

                if self.frontend.readiness().filter_interest().is_empty()
                    && all_backends_readiness_are_empty
                {
                    break;
                }

                counter += 1;
                if counter >= max_loop_iterations {
                    incr!("http.infinite_loop.error");
                    return SessionResult::Close;
                }
            }

            let context = &mut self.context;
            let mut dirty = false;
            for stream_id in 0..context.streams.len() {
                if context.streams[stream_id].state == StreamState::Link {
                    // Before the first request triggers a stream Link, the frontend timeout is set
                    // to a shorter request_timeout, here we switch to the longer nominal timeout
                    self.frontend
                        .timeout_container()
                        .set_duration(self.configured_frontend_timeout);
                    let front_readiness = self.frontend.readiness_mut();
                    dirty = true;
                    match self.router.connect(
                        stream_id,
                        context,
                        session.clone(),
                        proxy.clone(),
                        metrics,
                    ) {
                        Ok(_) => {}
                        Err(error) => {
                            println_!("Connection error: {error}");
                            let stream = &mut context.streams[stream_id];
                            use BackendConnectionError as BE;
                            match error {
                                BE::Backend(BackendError::NoBackendForCluster(_))
                                | BE::MaxConnectionRetries(_)
                                | BE::MaxSessionsMemory
                                | BE::MaxBuffers => {
                                    set_default_answer(stream, front_readiness, 503);
                                }
                                BE::RetrieveClusterError(
                                    RetrieveClusterError::RetrieveFrontend(_),
                                ) => {
                                    set_default_answer(stream, front_readiness, 404);
                                }
                                BE::RetrieveClusterError(
                                    RetrieveClusterError::UnauthorizedRoute,
                                ) => {
                                    set_default_answer(stream, front_readiness, 401);
                                }
                                BE::RetrieveClusterError(RetrieveClusterError::HttpsRedirect) => {
                                    set_default_answer(stream, front_readiness, 301);
                                }

                                BE::Backend(_) => {}
                                BE::RetrieveClusterError(_) => unreachable!(),
                                // TCP specific error
                                BE::NotFound(_) => unreachable!(),
                            }
                        }
                    }
                }
            }
            if !dirty {
                break;
            }
        }

        SessionResult::Continue
    }

    fn update_readiness(&mut self, token: Token, events: Ready) {
        println!("EVENTS: {events:?} on {token:?}");
        if token == self.frontend_token {
            self.frontend.readiness_mut().event |= events;
        } else if let Some(c) = self.router.backends.get_mut(&token) {
            c.readiness_mut().event |= events;
        }
    }

    fn timeout(&mut self, token: Token, _metrics: &mut SessionMetrics) -> StateResult {
        println_!("MuxState::timeout({token:?})");
        let front_is_h2 = match self.frontend {
            Connection::H1(_) => false,
            Connection::H2(_) => true,
        };
        let mut should_close = true;
        let mut should_write = false;
        if self.frontend_token == token {
            println_!("MuxState::timeout_frontend({:#?})", self.frontend);
            self.frontend.timeout_container().triggered();
            let front_readiness = self.frontend.readiness_mut();
            for stream in &mut self.context.streams {
                match stream.state {
                    StreamState::Idle => {
                        // In h1 an Idle stream is always the first request, so we can send a 408
                        // In h2 an Idle stream doesn't necessarily hold a request yet,
                        // in most cases it was just reserved, so we can just ignore them.
                        if !front_is_h2 {
                            set_default_answer(stream, front_readiness, 408);
                            should_write = true;
                        }
                    }
                    StreamState::Link => {
                        // This is an unusual case, as we have both a complete request and no
                        // available backend yet. For now, we answer with 503
                        set_default_answer(stream, front_readiness, 503);
                        should_write = true;
                    }
                    StreamState::Linked(_) => {
                        // A stream Linked to a backend is waiting for the response, not the request.
                        // For streaming or malformed requests, it is possible that the request is not
                        // terminated at this point. For now, we do nothing
                        should_close = false;
                    }
                    StreamState::Unlinked => {
                        // A stream Unlinked already has a response and its backend closed.
                        // In case it hasn't finished proxying we wait. Otherwise it is a stream
                        // kept alive for a new request, which can be killed.
                        if !stream.back.is_completed() {
                            should_close = false;
                        }
                    }
                    StreamState::Recycle => {
                        // A recycled stream is an h2 stream which doesn't hold a request anymore.
                        // We can ignore it.
                    }
                }
            }
        } else if let Some(backend) = self.router.backends.get_mut(&token) {
            println_!("MuxState::timeout_backend({:#?})", backend);
            backend.timeout_container().triggered();
            let front_readiness = self.frontend.readiness_mut();
            for stream_id in 0..self.context.streams.len() {
                let stream = &mut self.context.streams[stream_id];
                if let StreamState::Linked(back_token) = stream.state {
                    if token == back_token {
                        // This stream is linked to the backend that timedout
                        if stream.back.is_terminated() || stream.back.is_error() {
                            println!(
                                "Stream terminated or in error, do nothing, just wait a bit more"
                            );
                            // Nothing to do, simply wait for the remaining bytes to be proxied
                            if !stream.back.is_completed() {
                                should_close = false;
                            }
                        } else if !stream.back.consumed {
                            // The response has not started yet
                            println!("Stream still waiting for response, send 504");
                            set_default_answer(stream, front_readiness, 504);
                            should_write = true;
                        } else {
                            println!("Stream waiting for end of response, forcefully terminate it");
                            forcefully_terminate_answer(
                                stream,
                                front_readiness,
                                H2Error::InternalError,
                            );
                            should_write = true;
                        }
                        backend.end_stream(stream_id, &mut self.context);
                        // backend.force_disconnect();
                    }
                }
            }
        } else {
            // Session received a timeout for an unknown token, ignore it
            return StateResult::Continue;
        }
        if should_write {
            return match self
                .frontend
                .writable(&mut self.context, EndpointClient(&mut self.router))
            {
                MuxResult::Continue => StateResult::Continue,
                MuxResult::Upgrade => StateResult::Upgrade,
                MuxResult::CloseSession => StateResult::CloseSession,
            };
        }
        if should_close {
            StateResult::CloseSession
        } else {
            StateResult::Continue
        }
    }

    fn cancel_timeouts(&mut self) {
        println_!("MuxState::cancel_timeouts");
        self.frontend.timeout_container().cancel();
        for backend in self.router.backends.values_mut() {
            backend.timeout_container().cancel();
        }
    }

    fn print_state(&self, context: &str) {
        error!(
            "\
{} Session(Mux)
\tFrontend:
\t\ttoken: {:?}\treadiness: {:?}
\tBackend(s):",
            context,
            self.frontend_token,
            self.frontend.readiness()
        );
        for (backend_token, backend) in &self.router.backends {
            error!(
                "\t\ttoken: {:?}\treadiness: {:?}",
                backend_token,
                backend.readiness()
            )
        }
    }

    fn close<P: L7Proxy>(&mut self, proxy: Rc<RefCell<P>>, _metrics: &mut SessionMetrics) {
        println_!("FRONTEND: {:#?}", self.frontend);
        println_!("BACKENDS: {:#?}", self.router.backends);

        for (token, backend) in &mut self.router.backends {
            let proxy_borrow = proxy.borrow();
            backend.timeout_container().cancel();
            let socket = backend.socket_mut();
            if let Err(e) = proxy_borrow.deregister_socket(socket) {
                error!("error deregistering back socket({:?}): {:?}", socket, e);
            }
            if let Err(e) = socket.shutdown(Shutdown::Both) {
                if e.kind() != ErrorKind::NotConnected {
                    error!("error shutting down back socket({:?}): {:?}", socket, e);
                }
            }
            if !proxy_borrow.remove_session(*token) {
                error!("session {:?} was already removed!", token);
            } else {
                // println!("SUCCESS: session {token:?} was removed!");
            }
        }
        let s = match &mut self.frontend {
            Connection::H1(c) => &mut c.socket,
            Connection::H2(c) => &mut c.socket,
        };
        let mut b = [0; 1024];
        let (size, status) = s.socket_read(&mut b);
        println_!("socket: {size} {status:?} {:?}", &b[..size]);
        for stream in &mut self.context.streams {
            for kawa in [&mut stream.front, &mut stream.back] {
                kawa::debug_kawa(kawa);
                // kawa.prepare(&mut kawa::h1::BlockConverter);
                // let out = kawa.as_io_slice();
                // let mut writer = std::io::BufWriter::new(Vec::new());
                // let amount = writer.write_vectored(&out).unwrap();
                // println_!(
                //     "amount: {amount}\n{}",
                //     String::from_utf8_lossy(writer.buffer())
                // );
            }
        }
    }

    fn shutting_down(&mut self) -> SessionIsToBeClosed {
        let mut can_stop = true;
        for stream in &mut self.context.streams {
            match stream.state {
                StreamState::Linked(_) => {
                    can_stop = false;
                }
                StreamState::Unlinked => {
                    let front = &stream.front;
                    let back = &stream.back;
                    kawa::debug_kawa(front);
                    kawa::debug_kawa(back);
                    if front.is_initial()
                        && front.storage.is_empty()
                        && back.is_initial()
                        && back.storage.is_empty()
                    {
                        continue;
                    }
                    stream.context.closing = true;
                    can_stop = false;
                }
                _ => {}
            }
        }
        can_stop
    }
}
