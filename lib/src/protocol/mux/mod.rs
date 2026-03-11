use std::{
    cell::RefCell,
    collections::HashMap,
    fmt::Debug,
    io::ErrorKind,
    net::{Shutdown, SocketAddr},
    rc::{Rc, Weak},
    time::{Duration, Instant},
};

use kawa::ParsingPhase;
use mio::{Interest, Token, net::TcpStream};
use rusty_ulid::Ulid;
use sozu_command::{
    logging::EndpointRecord,
    proto::command::{Event, EventKind, ListenerType},
    ready::Ready,
};

mod converter;
mod h1;
mod h2;
mod parser;
mod pkawa;
mod serializer;

use crate::{
    BackendConnectionError, L7ListenerHandler, L7Proxy, ListenerHandler, Protocol, ProxySession,
    Readiness, RetrieveClusterError, SessionIsToBeClosed, SessionMetrics, SessionResult,
    StateResult,
    backends::{Backend, BackendError},
    http::HttpListener,
    https::HttpsListener,
    pool::{Checkout, Pool},
    protocol::{
        SessionState,
        http::{DefaultAnswer, answers::HttpAnswers, editor::HttpContext},
        mux::h2::{H2Settings, H2State, H2StreamId, Prioriser},
    },
    retry::RetryPolicy,
    router::Route,
    server::{CONN_RETRIES, push_event},
    socket::{FrontRustls, SocketHandler, SocketResult},
    timer::TimeoutContainer,
};

pub use crate::protocol::mux::{
    h1::ConnectionH1,
    h2::ConnectionH2,
    parser::{H2Error, error_code_to_str},
};

// ── Tuning Constants ─────────────────────────────────────────────────────────

/// Maximum event loop iterations before forcefully closing a session.
/// Prevents infinite loops from consuming the single-threaded worker.
const MAX_LOOP_ITERATIONS: i32 = 10_000;

// ─────────────────────────────────────────────────────────────────────────────

/// Generic Http representation using the Kawa crate using the Checkout of Sozu as buffer
type GenericHttpStream = kawa::Kawa<Checkout>;
type StreamId = u32;
type GlobalStreamId = usize;
pub type MuxClear = Mux<TcpStream, HttpListener>;
pub type MuxTls = Mux<FrontRustls, HttpsListener>;

pub fn fill_default_301_answer<T: kawa::AsBuffer>(kawa: &mut kawa::Kawa<T>, host: &str, uri: &str) {
    kawa.detached.status_line = kawa::StatusLine::Response {
        version: kawa::Version::V11,
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
    let reason: &'static [u8] = match code {
        400 => b"Bad Request",
        401 => b"Unauthorized",
        404 => b"Not Found",
        408 => b"Request Timeout",
        413 => b"Payload Too Large",
        502 => b"Bad Gateway",
        503 => b"Service Unavailable",
        504 => b"Gateway Timeout",
        507 => b"Insufficient Storage",
        _ => b"Sozu Default Answer",
    };
    kawa.detached.status_line = kawa::StatusLine::Response {
        version: kawa::Version::V11,
        code,
        status: kawa::Store::from_string(code.to_string()),
        reason: kawa::Store::Static(reason),
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
    kawa.push_block(kawa::Block::Flags(kawa::Flags {
        end_body: false,
        end_chunk: false,
        end_header: true,
        end_stream: true,
    }));
    kawa.parsing_phase = kawa::ParsingPhase::Terminated;
}

/// Build a route string from the stream's HTTP context, matching kawa_h1's `get_route()`
fn get_route(context: &HttpContext) -> String {
    if let Some(method) = &context.method {
        if let Some(authority) = &context.authority {
            if let Some(path) = &context.path {
                return format!("{method} {authority}{path}");
            }
            return format!("{method} {authority}");
        }
        return format!("{method}");
    }
    String::new()
}

/// Copy blocks from a rendered `DefaultAnswerStream` into `stream.back`.
///
/// The template-rendered stream uses `SharedBuffer` storage, so any `Store::Slice`
/// references must be captured (converted to owned `Store::Alloc`) before copying.
fn copy_default_answer_to_stream(
    rendered: crate::protocol::http::answers::DefaultAnswerStream,
    kawa: &mut GenericHttpStream,
) {
    let buf = rendered.storage.buffer();

    // Copy the status line, capturing any buffer-dependent Stores
    kawa.detached.status_line = match rendered.detached.status_line {
        kawa::StatusLine::Response {
            version,
            code,
            status,
            reason,
        } => kawa::StatusLine::Response {
            version,
            code,
            status: status.capture(buf),
            reason: reason.capture(buf),
        },
        other => other,
    };
    kawa.push_block(kawa::Block::StatusLine);

    // Copy all remaining blocks, capturing buffer-dependent Stores
    for block in rendered.blocks {
        let captured = match block {
            kawa::Block::StatusLine => continue, // already handled above
            kawa::Block::Header(kawa::Pair { key, val }) => kawa::Block::Header(kawa::Pair {
                key: key.capture(buf),
                val: val.capture(buf),
            }),
            kawa::Block::Chunk(kawa::Chunk { data }) => kawa::Block::Chunk(kawa::Chunk {
                data: data.capture(buf),
            }),
            kawa::Block::Flags(flags) => kawa::Block::Flags(flags),
            kawa::Block::Cookies => kawa::Block::Cookies,
            kawa::Block::ChunkHeader(kawa::ChunkHeader { length }) => {
                kawa::Block::ChunkHeader(kawa::ChunkHeader {
                    length: length.capture(buf),
                })
            }
        };
        kawa.push_block(captured);
    }

    kawa.parsing_phase = rendered.parsing_phase;
    kawa.body_size = rendered.body_size;
}

/// Replace the content of the kawa message with a default Sozu answer for a given status code.
///
/// Uses the listener's `HttpAnswers` templates to produce responses matching the configured
/// custom answers, preserving backward compatibility with the kawa_h1 code path.
fn set_default_answer(
    stream: &mut Stream,
    readiness: &mut Readiness,
    code: u16,
    answers: &HttpAnswers,
) {
    let context = &mut stream.context;
    let kawa = &mut stream.back;
    kawa.clear();
    kawa.storage.clear();
    let key = match code {
        301 => "http.301.redirection",
        400 => "http.400.errors",
        401 => "http.401.errors",
        404 => "http.404.errors",
        408 => "http.408.errors",
        413 => "http.413.errors",
        502 => "http.502.errors",
        503 => "http.503.errors",
        504 => "http.504.errors",
        507 => "http.507.errors",
        _ => "http.other.errors",
    };
    incr!(
        key,
        context.cluster_id.as_deref(),
        context.backend_id.as_deref()
    );

    let request_id = context.id.to_string();
    let route = get_route(context);
    let cluster_id = context.cluster_id.as_deref();
    let backend_id = context.backend_id.as_deref();

    let answer = match code {
        301 => DefaultAnswer::Answer301 {
            location: format!(
                "https://{}{}",
                context.authority.as_deref().unwrap_or_default(),
                context.path.as_deref().unwrap_or_default()
            ),
        },
        400 => DefaultAnswer::Answer400 {
            message: String::new(),
            phase: kawa::ParsingPhaseMarker::Error,
            successfully_parsed: String::from("null"),
            partially_parsed: String::from("null"),
            invalid: String::from("null"),
        },
        401 => DefaultAnswer::Answer401 {},
        404 => DefaultAnswer::Answer404 {},
        408 => DefaultAnswer::Answer408 {
            duration: String::new(),
        },
        502 => DefaultAnswer::Answer502 {
            message: String::new(),
            phase: kawa::ParsingPhaseMarker::Error,
            successfully_parsed: String::from("null"),
            partially_parsed: String::from("null"),
            invalid: String::from("null"),
        },
        503 => DefaultAnswer::Answer503 {
            message: String::new(),
        },
        504 => DefaultAnswer::Answer504 {
            duration: String::new(),
        },
        _ => DefaultAnswer::Answer503 {
            message: format!("Unexpected error code: {code}"),
        },
    };

    if code != 301 {
        context.keep_alive_frontend = false;
    }

    let rendered = answers.get(answer, request_id, cluster_id, backend_id, route);
    copy_default_answer_to_stream(rendered, kawa);

    context.status = Some(code);
    stream.state = StreamState::Unlinked;
    readiness.interest.insert(Ready::WRITABLE);
}

/// Forcefully terminates a kawa message by setting the "end_stream" flag and setting the parsing_phase to Error.
/// An H2 converter will produce an RstStream frame.
fn forcefully_terminate_answer(stream: &mut Stream, readiness: &mut Readiness, error: H2Error) {
    let kawa = &mut stream.back;
    kawa.out.clear();
    kawa.blocks.clear();
    kawa.parsing_phase
        .error(error_code_to_str(error as u32).into());
    stream.state = StreamState::Unlinked;
    readiness.interest.insert(Ready::WRITABLE);
}

pub enum Position {
    Client(String, Rc<RefCell<Backend>>, BackendStatus),
    Server,
}

impl Debug for Position {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client(cluster_id, _, status) => f
                .debug_tuple("Client")
                .field(cluster_id)
                .field(status)
                .finish(),
            Self::Server => write!(f, "Server"),
        }
    }
}

#[allow(dead_code)]
impl Position {
    fn is_server(&self) -> bool {
        match self {
            Position::Client(..) => false,
            Position::Server => true,
        }
    }
    fn is_client(&self) -> bool {
        match self {
            Position::Client(..) => true,
            Position::Server => false,
        }
    }
}

#[derive(Debug)]
pub enum BackendStatus {
    Connecting(Instant),
    Connected,
    KeepAlive,
    Disconnecting,
}

#[derive(Debug, Clone, Copy)]
pub enum MuxResult {
    Continue,
    Upgrade,
    CloseSession,
}

pub trait Endpoint: Debug {
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
    /// Returns false if the stream could not be started (e.g. max concurrent streams reached).
    fn start_stream<L: ListenerHandler + L7ListenerHandler>(
        &mut self,
        token: Token,
        stream: GlobalStreamId,
        context: &mut Context<L>,
    ) -> bool;
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
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
        backend: Rc<RefCell<Backend>>,
        timeout_container: TimeoutContainer,
    ) -> Connection<Front> {
        Connection::H1(ConnectionH1 {
            socket: front_stream,
            position: Position::Client(
                cluster_id,
                backend,
                BackendStatus::Connecting(Instant::now()),
            ),
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
        let local_settings = H2Settings::default();
        let mut decoder = loona_hpack::Decoder::new();
        // RFC 7541 §4.2: enforce SETTINGS_HEADER_TABLE_SIZE as the upper bound
        // for dynamic table size updates from the peer
        decoder.set_max_allowed_table_size(local_settings.settings_header_table_size as usize);
        Some(Connection::H2(ConnectionH2 {
            decoder,
            encoder: loona_hpack::Encoder::new(),
            expect_read: Some((H2StreamId::Zero, h2::CLIENT_PREFACE_SIZE)),
            expect_write: None,
            last_stream_id: 0,
            local_settings,
            peer_settings: H2Settings::default(),
            position: Position::Server,
            prioriser: Prioriser::default(),
            readiness: Readiness {
                interest: Ready::READABLE | Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            socket: front_stream,
            state: H2State::ClientPreface,
            streams: HashMap::new(),
            timeout_container,
            window: h2::DEFAULT_INITIAL_WINDOW_SIZE as i32,
            received_bytes_since_update: 0,
            pending_window_updates: Vec::new(),
            highest_peer_stream_id: 0,
            converter_buf: Vec::new(),
            draining: false,
            peer_last_stream_id: None,
            zero: kawa::Kawa::new(kawa::Kind::Request, kawa::Buffer::new(buffer)),
        }))
    }
    pub fn new_h2_client(
        front_stream: Front,
        cluster_id: String,
        backend: Rc<RefCell<Backend>>,
        pool: Weak<RefCell<Pool>>,
        timeout_container: TimeoutContainer,
    ) -> Option<Connection<Front>> {
        let buffer = pool
            .upgrade()
            .and_then(|pool| pool.borrow_mut().checkout())?;
        let local_settings = H2Settings::default();
        let mut decoder = loona_hpack::Decoder::new();
        // RFC 7541 §4.2: enforce SETTINGS_HEADER_TABLE_SIZE as the upper bound
        // for dynamic table size updates from the peer
        decoder.set_max_allowed_table_size(local_settings.settings_header_table_size as usize);
        Some(Connection::H2(ConnectionH2 {
            decoder,
            encoder: loona_hpack::Encoder::new(),
            expect_read: None,
            expect_write: None,
            last_stream_id: 0,
            local_settings,
            peer_settings: H2Settings::default(),
            position: Position::Client(
                cluster_id,
                backend,
                BackendStatus::Connecting(Instant::now()),
            ),
            prioriser: Prioriser::default(),
            readiness: Readiness {
                interest: Ready::WRITABLE | Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            socket: front_stream,
            state: H2State::ClientPreface,
            streams: HashMap::new(),
            timeout_container,
            window: h2::DEFAULT_INITIAL_WINDOW_SIZE as i32,
            received_bytes_since_update: 0,
            pending_window_updates: Vec::new(),
            highest_peer_stream_id: 0,
            converter_buf: Vec::new(),
            draining: false,
            peer_last_stream_id: None,
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
    #[allow(dead_code)]
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
        match self.position() {
            Position::Client(cluster_id, backend, _) => {
                let mut backend_borrow = backend.borrow_mut();
                backend_borrow.dec_connections();
                gauge_add!("backend.connections", -1);
                gauge_add!(
                    "connections_per_backend",
                    -1,
                    Some(cluster_id),
                    Some(&backend_borrow.backend_id)
                );
                trace!("connection close: {:#?}", backend_borrow);
            }
            Position::Server => {}
        }
        match self {
            Connection::H1(c) => c.close(context, endpoint),
            Connection::H2(c) => c.close(context, endpoint),
        }
    }

    fn end_stream<L>(&mut self, stream: GlobalStreamId, context: &mut Context<L>)
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        if let Position::Client(_, backend, BackendStatus::Connected) = self.position() {
            let mut backend_borrow = backend.borrow_mut();
            backend_borrow.active_requests = backend_borrow.active_requests.saturating_sub(1);
            trace!("connection end stream: {:#?}", backend_borrow);
        }
        match self {
            Connection::H1(c) => c.end_stream(stream, context),
            Connection::H2(c) => c.end_stream(stream, context),
        }
    }

    fn start_stream<L>(&mut self, stream: GlobalStreamId, context: &mut Context<L>) -> bool
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        if let Position::Client(_, backend, BackendStatus::Connected) = self.position() {
            let mut backend_borrow = backend.borrow_mut();
            backend_borrow.active_requests += 1;
            trace!("connection start stream: {:#?}", backend_borrow);
        }
        let started = match self {
            Connection::H1(c) => c.start_stream(stream, context),
            Connection::H2(c) => c.start_stream(stream, context),
        };
        if !started {
            // Undo active_requests increment on failure
            if let Position::Client(_, backend, BackendStatus::Connected) = self.position() {
                let mut backend_borrow = backend.borrow_mut();
                backend_borrow.active_requests -= 1;
            }
        }
        started
    }
}

#[derive(Debug)]
struct EndpointServer<'a, Front: SocketHandler>(&'a mut Connection<Front>);
#[derive(Debug)]
struct EndpointClient<'a>(&'a mut Router);

// note: EndpointServer are used by client Connection, they do not know the frontend Token
// they will use the Stream's Token which is their backend token
impl<Front: SocketHandler + Debug> Endpoint for EndpointServer<'_, Front> {
    fn readiness(&self, _token: Token) -> &Readiness {
        self.0.readiness()
    }
    fn readiness_mut(&mut self, _token: Token) -> &mut Readiness {
        self.0.readiness_mut()
    }

    fn end_stream<L>(&mut self, _token: Token, stream: GlobalStreamId, context: &mut Context<L>)
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        // this may be used to forward H2<->H2 RstStream
        // or to handle backend hup
        self.0.end_stream(stream, context);
    }

    fn start_stream<L>(
        &mut self,
        _token: Token,
        stream: GlobalStreamId,
        context: &mut Context<L>,
    ) -> bool
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        // Forward stream start to the frontend connection.
        // This is used when a backend H2 connection starts a new stream
        // (e.g. for H2<->H2 proxying or PUSH_PROMISE forwarding).
        self.0.start_stream(stream, context)
    }
}
impl Endpoint for EndpointClient<'_> {
    fn readiness(&self, token: Token) -> &Readiness {
        match self.0.backends.get(&token) {
            Some(backend) => backend.readiness(),
            None => {
                error!(
                    "backend token {:?} missing from backends map (readiness)",
                    token
                );
                &self.0.fallback_readiness
            }
        }
    }
    fn readiness_mut(&mut self, token: Token) -> &mut Readiness {
        match self.0.backends.get_mut(&token) {
            Some(backend) => backend.readiness_mut(),
            None => {
                error!(
                    "backend token {:?} missing from backends map (readiness_mut)",
                    token
                );
                &mut self.0.fallback_readiness
            }
        }
    }

    fn end_stream<L>(&mut self, token: Token, stream: GlobalStreamId, context: &mut Context<L>)
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        match self.0.backends.get_mut(&token) {
            Some(backend) => backend.end_stream(stream, context),
            None => {
                error!(
                    "backend token {:?} missing from backends map (end_stream)",
                    token
                );
            }
        }
    }

    fn start_stream<L>(
        &mut self,
        token: Token,
        stream: GlobalStreamId,
        context: &mut Context<L>,
    ) -> bool
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        match self.0.backends.get_mut(&token) {
            Some(backend) => backend.start_stream(stream, context),
            None => {
                error!(
                    "backend token {:?} missing from backends map (start_stream)",
                    token
                );
                false
            }
        }
    }
}

fn update_readiness_after_read(
    size: usize,
    status: SocketResult,
    readiness: &mut Readiness,
) -> bool {
    trace!("  size={}, status={:?}", size, status);
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
    trace!("  size={}, status={:?}", size, status);
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
        !matches!(self, StreamState::Idle | StreamState::Recycle)
    }
}

pub struct Stream {
    pub window: i32,
    pub attempts: u8,
    pub state: StreamState,
    /// True when the frontend connection has received end_of_stream from the client.
    pub front_received_end_of_stream: bool,
    /// True when the backend connection has received end_of_stream from the backend server.
    pub back_received_end_of_stream: bool,
    /// Tracks total DATA payload bytes received on the frontend for content-length validation (RFC 9113 §8.1.1)
    pub front_data_received: usize,
    /// Tracks total DATA payload bytes received on the backend for content-length validation (RFC 9113 §8.1.1)
    pub back_data_received: usize,
    pub front: GenericHttpStream,
    pub back: GenericHttpStream,
    pub context: HttpContext,
    pub metrics: SessionMetrics,
}

struct KawaSummary<'a>(&'a GenericHttpStream);
impl Debug for KawaSummary<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kawa")
            .field("kind", &self.0.kind)
            .field("parsing_phase", &self.0.parsing_phase)
            .field("body_size", &self.0.body_size)
            .field("consumed", &self.0.consumed)
            .field("expects", &self.0.expects)
            .field("blocks", &self.0.blocks.len())
            .field("out", &self.0.out.len())
            .field("storage_start", &self.0.storage.start)
            .field("storage_head", &self.0.storage.head)
            .field("storage_end", &self.0.storage.end)
            .finish()
    }
}
impl Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stream")
            .field("window", &self.window)
            .field("attempts", &self.attempts)
            .field("state", &self.state)
            .field(
                "front_received_end_of_stream",
                &self.front_received_end_of_stream,
            )
            .field(
                "back_received_end_of_stream",
                &self.back_received_end_of_stream,
            )
            .field("front_data_received", &self.front_data_received)
            .field("back_data_received", &self.back_data_received)
            .field("front", &KawaSummary(&self.front))
            .field("back", &KawaSummary(&self.back))
            .field("context", &self.context)
            .field("metrics", &self.metrics)
            .finish()
    }
}

/// This struct allows to mutably borrow the read and write buffers (dependant on the position)
/// as well as the context and metrics of a Stream at the same time
pub struct StreamParts<'a> {
    pub window: &'a mut i32,
    pub rbuffer: &'a mut GenericHttpStream,
    pub wbuffer: &'a mut GenericHttpStream,
    /// Tracks whether end_of_stream has been received on the read side of this connection.
    pub received_end_of_stream: &'a mut bool,
    /// Tracks total DATA payload bytes received on the read side (for content-length validation).
    pub data_received: &'a mut usize,
    pub context: &'a mut HttpContext,
    pub metrics: &'a mut SessionMetrics,
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
            front_received_end_of_stream: false,
            back_received_end_of_stream: false,
            front_data_received: 0,
            back_data_received: 0,
            front: GenericHttpStream::new(kawa::Kind::Request, kawa::Buffer::new(front_buffer)),
            back: GenericHttpStream::new(kawa::Kind::Response, kawa::Buffer::new(back_buffer)),
            context,
            metrics: SessionMetrics::new(None), // FIXME
        })
    }
    pub fn split(&mut self, position: &Position) -> StreamParts<'_> {
        match position {
            Position::Client(..) => StreamParts {
                window: &mut self.window,
                rbuffer: &mut self.back,
                wbuffer: &mut self.front,
                received_end_of_stream: &mut self.back_received_end_of_stream,
                data_received: &mut self.back_data_received,
                context: &mut self.context,
                metrics: &mut self.metrics,
            },
            Position::Server => StreamParts {
                window: &mut self.window,
                rbuffer: &mut self.front,
                wbuffer: &mut self.back,
                received_end_of_stream: &mut self.front_received_end_of_stream,
                data_received: &mut self.front_data_received,
                context: &mut self.context,
                metrics: &mut self.metrics,
            },
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
        let context = &self.context;
        gauge_add!("http.active_requests", -1);
        if error {
            incr!("http.errors");
        }
        let protocol = match context.protocol {
            Protocol::HTTP => "http",
            Protocol::HTTPS => "https",
            other => {
                error!(
                    "mux streams only handle HTTP or HTTPS protocols, got {:?}",
                    other
                );
                "unknown"
            }
        };

        // Save the HTTP status code of the backend response
        let key = if let Some(status) = context.status {
            match status {
                100..=199 => "http.status.1xx",
                200..=299 => "http.status.2xx",
                300..=399 => "http.status.3xx",
                400..=499 => "http.status.4xx",
                500..=599 => "http.status.5xx",
                _ => "http.status.other",
            }
        } else {
            "http.status.none"
        };
        // if context.cluster_id.is_some() {
        //     incr!(key);
        // }
        incr!(
            key,
            context.cluster_id.as_deref(),
            context.backend_id.as_deref()
        );

        let endpoint = EndpointRecord::Http {
            method: context.method.as_deref(),
            authority: context.authority.as_deref(),
            path: context.path.as_deref(),
            reason: context.reason.as_deref(),
            status: context.status,
        };

        let listener = listener.borrow();
        let tags = context.authority.as_deref().and_then(|host| {
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
            context: context.log_context(),
            session_address: context.session_address,
            backend_address: context.backend_address,
            protocol,
            endpoint,
            tags,
            client_rtt: None, //socket_rtt(self.front_socket()),
            server_rtt: None, //self.backend_socket.as_ref().and_then(socket_rtt),
            service_time: self.metrics.service_time(),
            response_time: self.metrics.backend_response_time(),
            request_time: self.metrics.request_time(),
            bytes_in: self.metrics.bin,
            bytes_out: self.metrics.bout,
            user_agent: context.user_agent.as_deref(),
            otel: None,
        };
        self.metrics.register_end_of_session(&context.log_context());
    }
}

#[derive(Default)]
pub struct DebugHistory {
    pub events: Vec<DebugEvent>,
    pub is_interesting: bool,
}
impl DebugHistory {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn push(&mut self, _event: DebugEvent) {
        //self.events.push(_event);
    }
    pub fn set_interesting(&mut self, _val: bool) {
        //self.is_interesting = _val;
    }
    pub fn is_interesting(&mut self) -> bool {
        self.is_interesting
    }
}

pub struct Context<L: ListenerHandler + L7ListenerHandler> {
    pub streams: Vec<Stream>,
    pub pool: Weak<RefCell<Pool>>,
    pub listener: Rc<RefCell<L>>,
    pub session_address: Option<SocketAddr>,
    pub public_address: SocketAddr,
    pub debug: DebugHistory,
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
            debug: DebugHistory::new(),
        }
    }

    pub fn active_len(&self) -> usize {
        self.streams
            .iter()
            .filter(|s| !matches!(s.state, StreamState::Recycle))
            .count()
    }

    pub fn create_stream(&mut self, request_id: Ulid, window: u32) -> Option<GlobalStreamId> {
        let listener = self.listener.borrow();
        let http_context = HttpContext::new(
            request_id,
            listener.protocol(),
            self.public_address,
            self.session_address,
            listener.get_sticky_name().to_string(),
        );
        for (stream_id, stream) in self.streams.iter_mut().enumerate() {
            if stream.state == StreamState::Recycle {
                trace!("Reuse stream: {}", stream_id);
                stream.state = StreamState::Idle;
                stream.attempts = 0;
                stream.front_received_end_of_stream = false;
                stream.back_received_end_of_stream = false;
                stream.front_data_received = 0;
                stream.back_data_received = 0;
                stream.window = window as i32;
                stream.context = http_context;
                stream.back.clear();
                stream.back.storage.clear();
                stream.front.clear();
                stream.front.storage.clear();
                stream.metrics.reset();
                stream.metrics.start = Some(Instant::now());
                return Some(stream_id);
            }
        }
        self.streams
            .push(Stream::new(self.pool.clone(), http_context, window)?);
        Some(self.streams.len() - 1)
    }
}

#[derive(Debug)]
pub struct Router {
    pub backends: HashMap<Token, Connection<TcpStream>>,
    pub configured_backend_timeout: Duration,
    pub configured_connect_timeout: Duration,
    /// Fallback readiness used when a backend token is missing from the map.
    /// This prevents panicking in the Endpoint trait methods that return references.
    fallback_readiness: Readiness,
}

impl Router {
    pub fn new(configured_backend_timeout: Duration, configured_connect_timeout: Duration) -> Self {
        Self {
            backends: HashMap::new(),
            configured_backend_timeout,
            configured_connect_timeout,
            fallback_readiness: Readiness::new(),
        }
    }

    fn connect<L: ListenerHandler + L7ListenerHandler>(
        &mut self,
        stream_id: GlobalStreamId,
        context: &mut Context<L>,
        session: Rc<RefCell<dyn ProxySession>>,
        proxy: Rc<RefCell<dyn L7Proxy>>,
    ) -> Result<(), BackendConnectionError> {
        let stream = &mut context.streams[stream_id];
        // when reused, a stream should be detached from its old connection, if not we could end
        // with concurrent connections on a single endpoint
        if !matches!(stream.state, StreamState::Link) {
            error!(
                "stream {} expected to be in Link state, got {:?}",
                stream_id, stream.state
            );
            return Err(BackendConnectionError::MaxSessionsMemory);
        }
        context
            .debug
            .push(DebugEvent::Str(stream.context.get_route()));
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
                    cluster.http2.unwrap_or(false),
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
        let reuse_connecting = true;
        for (token, backend) in &self.backends {
            match (h2, reuse_connecting, backend.position()) {
                (_, _, Position::Server) => {
                    error!("Backend connection unexpectedly behaves like a server");
                    continue;
                }
                (_, _, Position::Client(_, _, BackendStatus::Disconnecting)) => {}
                (true, false, Position::Client(_, _, BackendStatus::Connecting(_))) => {}

                (true, _, Position::Client(other_cluster_id, _, BackendStatus::Connected)) => {
                    if *other_cluster_id == cluster_id {
                        reuse_token = Some(*token);
                        break;
                    }
                }
                (
                    true,
                    true,
                    Position::Client(other_cluster_id, _, BackendStatus::Connecting(_)),
                ) => {
                    if *other_cluster_id == cluster_id {
                        reuse_token = Some(*token)
                    }
                }
                (true, _, Position::Client(other_cluster_id, _, BackendStatus::KeepAlive)) => {
                    if *other_cluster_id == cluster_id {
                        error!("ConnectionH2 unexpectedly behaves like H1 with KeepAlive");
                    }
                }

                (false, _, Position::Client(old_cluster_id, _, BackendStatus::KeepAlive)) => {
                    if *old_cluster_id == cluster_id {
                        reuse_token = Some(*token);
                        break;
                    }
                }
                // can't bundle H1 streams together
                (false, _, Position::Client(_, _, BackendStatus::Connected))
                | (false, _, Position::Client(_, _, BackendStatus::Connecting(_))) => {}
            }
        }
        trace!(
            "connect: {} (stick={}, h2={}) -> (reuse={:?})",
            cluster_id, frontend_should_stick, h2, reuse_token
        );

        let token = if let Some(token) = reuse_token {
            trace!("reused backend: {:#?}", self.backends.get(&token));
            token
        } else {
            let (mut socket, backend) = self.backend_from_request(
                &cluster_id,
                frontend_should_stick,
                stream_context,
                proxy.clone(),
                &context.listener,
            )?;
            stream.metrics.backend_start();
            stream.metrics.backend_id = stream.context.backend_id.to_owned();
            gauge_add!("backend.connections", 1);
            gauge_add!(
                "connections_per_backend",
                1,
                Some(&cluster_id),
                Some(&backend.borrow().backend_id)
            );

            if let Err(e) = socket.set_nodelay(true) {
                error!(
                    "error setting nodelay on back socket({:?}): {:?}",
                    socket, e
                );
            }

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
                    backend,
                    context.pool.clone(),
                    timeout_container,
                ) {
                    Some(connection) => connection,
                    None => return Err(BackendConnectionError::MaxBuffers),
                }
            } else {
                Connection::new_h1_client(socket, cluster_id, backend, timeout_container)
            };
            self.backends.insert(token, connection);
            token
        };

        // Link backend to stream, checking if the backend can accept it
        let Some(backend_conn) = self.backends.get_mut(&token) else {
            error!(
                "just-inserted or reused backend token {:?} missing from backends map",
                token
            );
            return Err(BackendConnectionError::MaxSessionsMemory);
        };
        let started = backend_conn.start_stream(stream_id, context);
        if !started {
            error!("Backend rejected stream start (max concurrent streams reached)");
            return Err(BackendConnectionError::MaxSessionsMemory);
        }
        // Reborrow stream to set linked state
        context.streams[stream_id].state = StreamState::Linked(token);

        // For reused backends: set context fields and metrics lifecycle
        if reuse_token.is_some() {
            if let Some(backend_conn) = self.backends.get(&token) {
                if let Position::Client(_, backend_ref, _) = backend_conn.position() {
                    let backend = backend_ref.borrow();
                    let stream = &mut context.streams[stream_id];
                    stream.context.backend_id = Some(backend.backend_id.to_owned());
                    stream.context.backend_address = Some(backend.address);
                    stream.metrics.backend_id = Some(backend.backend_id.to_owned());
                    stream.metrics.backend_start();
                    stream.metrics.backend_connected();
                }
            }
        }
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
                error!(
                    "Malformed request in connect (should be caught at parsing) {:?}: {}",
                    context, cluster_error
                );
                return Err(cluster_error);
            }
        };

        let route_result = listener.borrow().frontend_from_request(host, uri, method);

        let route = match route_result {
            Ok(route) => route,
            Err(frontend_error) => {
                trace!("{}", frontend_error);
                // self.set_answer(DefaultAnswerStatus::Answer404, None);
                return Err(RetrieveClusterError::RetrieveFrontend(frontend_error));
            }
        };

        let cluster_id = match route {
            Route::ClusterId(id) => id,
            Route::Deny => {
                trace!("Route::Deny");
                // self.set_answer(DefaultAnswerStatus::Answer401, None);
                return Err(RetrieveClusterError::UnauthorizedRoute);
            }
        };

        Ok(cluster_id)
    }

    pub fn backend_from_request<L: ListenerHandler + L7ListenerHandler>(
        &mut self,
        cluster_id: &str,
        frontend_should_stick: bool,
        context: &mut HttpContext,
        proxy: Rc<RefCell<dyn L7Proxy>>,
        listener: &Rc<RefCell<L>>,
    ) -> Result<(TcpStream, Rc<RefCell<Backend>>), BackendConnectionError> {
        let (backend, conn) = self
            .get_backend_for_sticky_session(
                cluster_id,
                frontend_should_stick,
                context.sticky_session_found.as_deref(),
                proxy,
            )
            .map_err(|backend_error| {
                trace!("{}", backend_error);
                // self.set_answer(DefaultAnswerStatus::Answer503, None);
                BackendConnectionError::Backend(backend_error)
            })?;

        if frontend_should_stick {
            // update sticky name in case it changed I guess?
            context.sticky_name = listener.borrow().get_sticky_name().to_string();

            context.sticky_session = Some(
                backend
                    .borrow()
                    .sticky_id
                    .clone()
                    .unwrap_or_else(|| backend.borrow().backend_id.clone()),
            );
        }

        context.backend_id = Some(backend.borrow().backend_id.clone());
        context.backend_address = Some(backend.borrow().address);

        Ok((conn, backend))
    }

    fn get_backend_for_sticky_session(
        &self,
        cluster_id: &str,
        frontend_should_stick: bool,
        sticky_session: Option<&str>,
        proxy: Rc<RefCell<dyn L7Proxy>>,
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

#[derive(Debug)]
pub enum DebugEvent {
    EV(Token, Ready),
    R(usize),
    L1,
    L2(i32),
    SR(Token, MuxResult, Readiness),
    SW(Token, MuxResult, Readiness),
    CW(Token, MuxResult, Readiness),
    CR(Token, MuxResult, Readiness),
    CC(usize, StreamState),
    CCS(Token, String),
    CCF(usize, BackendConnectionError),
    CH(Token, Readiness),
    S(u32, usize, ParsingPhase, usize, usize),
    Str(String),
    I1(usize),
    I2(usize, usize),
    I3(usize, usize, usize),
}

impl<Front: SocketHandler + std::fmt::Debug, L: ListenerHandler + L7ListenerHandler> SessionState
    for Mux<Front, L>
{
    fn ready(
        &mut self,
        session: Rc<RefCell<dyn ProxySession>>,
        proxy: Rc<RefCell<dyn L7Proxy>>,
        _metrics: &mut SessionMetrics,
    ) -> SessionResult {
        let mut counter = 0;

        if self.frontend.readiness().event.is_hup() {
            return SessionResult::Close;
        }

        let start = Instant::now();
        self.context.debug.push(DebugEvent::R(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as usize,
        ));
        trace!("{:?}", start);
        loop {
            self.context.debug.push(DebugEvent::L1);
            loop {
                let context = &mut self.context;
                context.debug.push(DebugEvent::L2(counter));
                if self.frontend.readiness().filter_interest().is_readable() {
                    let res = self
                        .frontend
                        .readable(context, EndpointClient(&mut self.router));
                    context.debug.push(DebugEvent::SR(
                        self.frontend_token,
                        res,
                        self.frontend.readiness().clone(),
                    ));
                    match res {
                        MuxResult::Continue => {}
                        MuxResult::CloseSession => return SessionResult::Close,
                        MuxResult::Upgrade => return SessionResult::Upgrade,
                    }
                }

                let mut all_backends_readiness_are_empty = true;
                let context = &mut self.context;
                let mut dead_backends = Vec::new();
                for (token, client) in self.router.backends.iter_mut() {
                    let readiness = client.readiness_mut();
                    let dead = readiness.filter_interest().is_hup()
                        || readiness.filter_interest().is_error();
                    if dead {
                        trace!("Backend({:?}) -> {:?}", token, readiness);
                        readiness.event.remove(Ready::WRITABLE);
                    }

                    if client.readiness().filter_interest().is_writable() {
                        let position = client.position_mut();
                        match position {
                            Position::Client(
                                cluster_id,
                                backend,
                                BackendStatus::Connecting(start),
                            ) => {
                                context
                                    .debug
                                    .push(DebugEvent::CCS(*token, cluster_id.clone()));

                                let mut backend_borrow = backend.borrow_mut();
                                if backend_borrow.retry_policy.is_down() {
                                    info!(
                                        "backend server {} at {} is up",
                                        backend_borrow.backend_id, backend_borrow.address
                                    );
                                    incr!(
                                        "backend.up",
                                        Some(cluster_id),
                                        Some(&backend_borrow.backend_id)
                                    );
                                    push_event(Event {
                                        kind: EventKind::BackendUp as i32,
                                        backend_id: Some(backend_borrow.backend_id.to_owned()),
                                        address: Some(backend_borrow.address.into()),
                                        cluster_id: Some(cluster_id.to_owned()),
                                    });
                                }

                                //successful connection, reset failure counter
                                backend_borrow.failures = 0;
                                backend_borrow.set_connection_time(start.elapsed());
                                backend_borrow.retry_policy.succeed();

                                for stream in &mut context.streams {
                                    match stream.state {
                                        StreamState::Linked(back_token) if back_token == *token => {
                                            stream.metrics.backend_connected();
                                            backend_borrow.active_requests += 1;
                                        }
                                        _ => {}
                                    }
                                }
                                trace!("connection success: {:#?}", backend_borrow);
                                drop(backend_borrow);
                                *position = Position::Client(
                                    std::mem::take(cluster_id),
                                    backend.clone(),
                                    BackendStatus::Connected,
                                );
                                client
                                    .timeout_container()
                                    .set_duration(self.router.configured_backend_timeout);
                            }
                            Position::Client(..) => {}
                            Position::Server => {
                                error!("backend connection cannot be in Server position");
                            }
                        }
                        let res = client.writable(context, EndpointServer(&mut self.frontend));
                        context
                            .debug
                            .push(DebugEvent::CW(*token, res, client.readiness().clone()));
                        match res {
                            MuxResult::Continue => {}
                            MuxResult::Upgrade => {
                                error!("only frontend connections can trigger Upgrade");
                            }
                            MuxResult::CloseSession => return SessionResult::Close,
                        }
                    }

                    if client.readiness().filter_interest().is_readable() {
                        let res = client.readable(context, EndpointServer(&mut self.frontend));
                        context
                            .debug
                            .push(DebugEvent::CR(*token, res, client.readiness().clone()));
                        match res {
                            MuxResult::Continue => {}
                            MuxResult::Upgrade => {
                                error!("only frontend connections can trigger Upgrade (readable)");
                            }
                            MuxResult::CloseSession => return SessionResult::Close,
                        }
                    }

                    if dead && !client.readiness().filter_interest().is_readable() {
                        context
                            .debug
                            .push(DebugEvent::CH(*token, client.readiness().clone()));
                        trace!("Closing {:#?}", client);
                        match client.position() {
                            Position::Client(cluster_id, backend, BackendStatus::Connecting(_)) => {
                                let mut backend_borrow = backend.borrow_mut();
                                backend_borrow.failures += 1;

                                let already_unavailable = backend_borrow.retry_policy.is_down();
                                backend_borrow.retry_policy.fail();
                                incr!(
                                    "backend.connections.error",
                                    Some(cluster_id),
                                    Some(&backend_borrow.backend_id)
                                );
                                if !already_unavailable && backend_borrow.retry_policy.is_down() {
                                    error!(
                                        "backend server {} at {} is down",
                                        backend_borrow.backend_id, backend_borrow.address
                                    );
                                    incr!(
                                        "backend.down",
                                        Some(cluster_id),
                                        Some(&backend_borrow.backend_id)
                                    );
                                    push_event(Event {
                                        kind: EventKind::BackendDown as i32,
                                        backend_id: Some(backend_borrow.backend_id.to_owned()),
                                        address: Some(backend_borrow.address.into()),
                                        cluster_id: Some(cluster_id.to_owned()),
                                    });
                                }
                                trace!("connection fail: {:#?}", backend_borrow);
                            }
                            Position::Client(_, backend, _) => {
                                let mut backend_borrow = backend.borrow_mut();
                                for stream in &mut context.streams {
                                    match stream.state {
                                        StreamState::Linked(back_token) if back_token == *token => {
                                            backend_borrow.active_requests =
                                                backend_borrow.active_requests.saturating_sub(1);
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            Position::Server => {
                                error!("dead backend cannot be in Server position");
                            }
                        }
                        client.close(context, EndpointServer(&mut self.frontend));
                        dead_backends.push(*token);
                    }

                    if !client.readiness().filter_interest().is_empty() {
                        all_backends_readiness_are_empty = false;
                    }
                }
                if !dead_backends.is_empty() {
                    for token in &dead_backends {
                        let proxy_borrow = proxy.borrow();
                        if let Some(mut client) = self.router.backends.remove(token) {
                            client.timeout_container().cancel();
                            let socket = client.socket_mut();
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
                        }
                    }
                    trace!("FRONTEND: {:#?}", self.frontend);
                    trace!("BACKENDS: {:#?}", self.router.backends);
                }

                let context = &mut self.context;
                if self.frontend.readiness().filter_interest().is_writable() {
                    let res = self
                        .frontend
                        .writable(context, EndpointClient(&mut self.router));
                    context.debug.push(DebugEvent::SW(
                        self.frontend_token,
                        res,
                        self.frontend.readiness().clone(),
                    ));
                    match res {
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
                if counter >= MAX_LOOP_ITERATIONS {
                    incr!("http.infinite_loop.error");
                    return SessionResult::Close;
                }
            }

            let context = &mut self.context;
            let answers_rc = context.listener.borrow().get_answers().clone();
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
                    match self
                        .router
                        .connect(stream_id, context, session.clone(), proxy.clone())
                    {
                        Ok(_) => {
                            let state = context.streams[stream_id].state;
                            context.debug.push(DebugEvent::CC(stream_id, state));
                        }
                        Err(error) => {
                            trace!("Connection error: {}", error);
                            let stream = &mut context.streams[stream_id];
                            let answers = answers_rc.borrow();
                            use BackendConnectionError as BE;
                            match error {
                                BE::Backend(BackendError::NoBackendForCluster(_))
                                | BE::MaxConnectionRetries(_)
                                | BE::MaxSessionsMemory
                                | BE::MaxBuffers => {
                                    set_default_answer(stream, front_readiness, 503, &answers);
                                }
                                BE::RetrieveClusterError(
                                    RetrieveClusterError::RetrieveFrontend(_),
                                ) => {
                                    set_default_answer(stream, front_readiness, 404, &answers);
                                }
                                BE::RetrieveClusterError(
                                    RetrieveClusterError::UnauthorizedRoute,
                                ) => {
                                    set_default_answer(stream, front_readiness, 401, &answers);
                                }
                                BE::RetrieveClusterError(RetrieveClusterError::HttpsRedirect) => {
                                    set_default_answer(stream, front_readiness, 301, &answers);
                                }

                                BE::Backend(_) => {}
                                BE::RetrieveClusterError(ref other) => {
                                    error!("unexpected RetrieveClusterError variant: {:?}", other);
                                    set_default_answer(stream, front_readiness, 503, &answers);
                                }
                                // TCP specific error
                                BE::NotFound(ref msg) => {
                                    error!(
                                        "NotFound is TCP-specific, not reachable in mux: {:?}",
                                        msg
                                    );
                                    set_default_answer(stream, front_readiness, 503, &answers);
                                }
                            }
                            context.debug.push(DebugEvent::CCF(stream_id, error));
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
        trace!("EVENTS: {:?} on {:?}", events, token);
        self.context.debug.push(DebugEvent::EV(token, events));
        if token == self.frontend_token {
            self.frontend.readiness_mut().event |= events;
        } else if let Some(c) = self.router.backends.get_mut(&token) {
            c.readiness_mut().event |= events;
        }
    }

    fn timeout(&mut self, token: Token, _metrics: &mut SessionMetrics) -> StateResult {
        trace!("MuxState::timeout({:?})", token);
        let front_is_h2 = match self.frontend {
            Connection::H1(_) => false,
            Connection::H2(_) => true,
        };
        let answers_rc = self.context.listener.borrow().get_answers().clone();
        let mut should_close = true;
        let mut should_write = false;
        if self.frontend_token == token {
            trace!("MuxState::timeout_frontend({:#?})", self.frontend);
            self.frontend.timeout_container().triggered();
            let front_readiness = self.frontend.readiness_mut();
            for stream in &mut self.context.streams {
                match stream.state {
                    StreamState::Idle => {
                        // In h1 an Idle stream is always the first request, so we can send a 408
                        // In h2 an Idle stream doesn't necessarily hold a request yet,
                        // in most cases it was just reserved, so we can just ignore them.
                        if !front_is_h2 {
                            let answers = answers_rc.borrow();
                            set_default_answer(stream, front_readiness, 408, &answers);
                            should_write = true;
                        }
                    }
                    StreamState::Link => {
                        // This is an unusual case, as we have both a complete request and no
                        // available backend yet. For now, we answer with 503
                        let answers = answers_rc.borrow();
                        set_default_answer(stream, front_readiness, 503, &answers);
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
            trace!("MuxState::timeout_backend({:#?})", backend);
            backend.timeout_container().triggered();
            let front_readiness = self.frontend.readiness_mut();
            for stream_id in 0..self.context.streams.len() {
                let stream = &mut self.context.streams[stream_id];
                if let StreamState::Linked(back_token) = stream.state {
                    if token == back_token {
                        // This stream is linked to the backend that timedout
                        if stream.back.is_terminated() || stream.back.is_error() {
                            trace!(
                                "Stream terminated or in error, do nothing, just wait a bit more"
                            );
                            // Nothing to do, simply wait for the remaining bytes to be proxied
                            if !stream.back.is_completed() {
                                should_close = false;
                            }
                        } else if !stream.back.consumed {
                            // The response has not started yet
                            trace!("Stream still waiting for response, send 504");
                            let answers = answers_rc.borrow();
                            set_default_answer(stream, front_readiness, 504, &answers);
                            should_write = true;
                        } else {
                            trace!("Stream waiting for end of response, forcefully terminate it");
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
        trace!("MuxState::cancel_timeouts");
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

    fn close(&mut self, proxy: Rc<RefCell<dyn L7Proxy>>, _metrics: &mut SessionMetrics) {
        if self.context.debug.is_interesting() {
            warn!("{:?}", self.context.debug.events);
        }
        debug!("MUX CLOSE");
        trace!("FRONTEND: {:#?}", self.frontend);
        trace!("BACKENDS: {:#?}", self.router.backends);

        self.frontend
            .close(&mut self.context, EndpointClient(&mut self.router));

        for (token, client) in &mut self.router.backends {
            let proxy_borrow = proxy.borrow();
            client.timeout_container().cancel();
            let socket = client.socket_mut();
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
            }

            match client.position() {
                Position::Client(cluster_id, backend, _) => {
                    let mut backend_borrow = backend.borrow_mut();
                    backend_borrow.dec_connections();
                    gauge_add!("backend.connections", -1);
                    gauge_add!(
                        "connections_per_backend",
                        -1,
                        Some(cluster_id),
                        Some(&backend_borrow.backend_id)
                    );
                    for stream in &mut self.context.streams {
                        match stream.state {
                            StreamState::Linked(back_token) if back_token == *token => {
                                backend_borrow.active_requests =
                                    backend_borrow.active_requests.saturating_sub(1);
                            }
                            _ => {}
                        }
                    }
                    trace!("connection (session) closed: {:#?}", backend_borrow);
                }
                Position::Server => {
                    error!("close_backend called on Server position");
                }
            }
        }
        /*
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
                kawa.prepare(&mut kawa::h1::BlockConverter);
                let out = kawa.as_io_slice();
                let mut writer = std::io::BufWriter::new(Vec::new());
                let amount = writer.write_vectored(&out).unwrap();
                println_!(
                    "amount: {amount}\n{}",
                    String::from_utf8_lossy(writer.buffer())
                );
            }
        }
        */
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
