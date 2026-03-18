//! HTTP/1.1 and HTTP/2 multiplexing layer.
//!
//! This module unifies HTTP/1.1 and HTTP/2 behind a single [`Mux`] session
//! state machine that integrates with sozu's mio event loop. The key types:
//!
//! - [`Mux`]: The top-level session state, generic over socket (`TcpStream` or
//!   `FrontRustls`) and listener. Implements `SessionState`.
//! - [`Connection`]: Enum dispatching to [`ConnectionH1`] or [`ConnectionH2`]
//!   for protocol-specific readable/writable logic.
//! - [`Stream`]: Per-request state with front/back kawa buffers, metrics, and
//!   lifecycle tracking. Shared between H1 and H2 paths.
//! - [`Context`]: Per-session context (cluster, backends, routing, timeouts).
//!
//! The H2 implementation handles RFC 9113 framing, HPACK (RFC 7541), flow
//! control, flood detection (CVE-2023-44487, CVE-2019-9512/9514/9515/9518,
//! CVE-2024-27316), and graceful shutdown (double-GOAWAY per RFC 9113 §6.8).

use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
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
pub mod parser;
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
        mux::h2::H2StreamId,
    },
    retry::RetryPolicy,
    router::Route,
    server::{CONN_RETRIES, push_event},
    socket::{FrontRustls, SocketHandler, SocketResult},
    timer::TimeoutContainer,
};

pub use crate::protocol::mux::{
    h1::ConnectionH1, h2::ConnectionH2, h2::H2ByteAccounting, h2::H2DrainState, h2::H2FloodConfig,
    h2::H2FlowControl, parser::H2Error,
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

/// Terminate a default answer with optional `Connection: close` and `Cache-Control` headers.
///
/// Note: the `Connection: close` header is only valid for HTTP/1.1. For H2 streams,
/// the H2 block converter (`is_connection_specific_header`) automatically strips it
/// before serialization, so callers need not guard on the protocol version.
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

/// Build a `DefaultAnswer` variant for the given status code with placeholder fields.
///
/// The mux layer does not have HTTP/1.1 parse state, so error-detail fields
/// (`message`, `phase`, `successfully_parsed`, etc.) are filled with neutral
/// placeholders. The 301 redirect is the only code that needs real context data
/// and is therefore handled inline in [`set_default_answer`].
fn default_answer_for_code(code: u16) -> DefaultAnswer {
    /// Helper for the 400/502 parse-error shape: empty message, Error phase,
    /// `"null"` for each parse-state field.
    fn parse_error_answer_400() -> DefaultAnswer {
        DefaultAnswer::Answer400 {
            message: String::new(),
            phase: kawa::ParsingPhaseMarker::Error,
            successfully_parsed: "null".to_owned(),
            partially_parsed: "null".to_owned(),
            invalid: "null".to_owned(),
        }
    }
    fn parse_error_answer_502() -> DefaultAnswer {
        DefaultAnswer::Answer502 {
            message: String::new(),
            phase: kawa::ParsingPhaseMarker::Error,
            successfully_parsed: "null".to_owned(),
            partially_parsed: "null".to_owned(),
            invalid: "null".to_owned(),
        }
    }

    match code {
        400 => parse_error_answer_400(),
        401 => DefaultAnswer::Answer401 {},
        404 => DefaultAnswer::Answer404 {},
        408 => DefaultAnswer::Answer408 {
            duration: String::new(),
        },
        502 => parse_error_answer_502(),
        503 => DefaultAnswer::Answer503 {
            message: String::new(),
        },
        504 => DefaultAnswer::Answer504 {
            duration: String::new(),
        },
        _ => DefaultAnswer::Answer503 {
            message: format!("Unexpected error code: {code}"),
        },
    }
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

    let answer = if code == 301 {
        DefaultAnswer::Answer301 {
            location: format!(
                "https://{}{}",
                context.authority.as_deref().unwrap_or_default(),
                context.path.as_deref().unwrap_or_default()
            ),
        }
    } else {
        context.keep_alive_frontend = false;
        default_answer_for_code(code)
    };

    let request_id = context.id.to_string();
    let route = context.get_route();
    let cluster_id = context.cluster_id.as_deref();
    let backend_id = context.backend_id.as_deref();

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
    kawa.parsing_phase.error(error.as_str().into());
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

impl Position {
    fn is_server(&self) -> bool {
        match self {
            Position::Client(..) => false,
            Position::Server => true,
        }
    }
    fn is_client(&self) -> bool {
        !self.is_server()
    }

    /// Increment the global `count!()` counter for bytes read on this side.
    pub fn count_bytes_in_counter(&self, size: usize) {
        match self {
            Position::Client(..) => count!("back_bytes_in", size as i64),
            Position::Server => count!("bytes_in", size as i64),
        }
    }

    /// Increment the global `count!()` counter for bytes written on this side.
    pub fn count_bytes_out_counter(&self, size: usize) {
        match self {
            Position::Client(..) => count!("back_bytes_out", size as i64),
            Position::Server => count!("bytes_out", size as i64),
        }
    }

    /// Attribute `size` bytes read to the appropriate `SessionMetrics` field.
    pub fn count_bytes_in(&self, metrics: &mut SessionMetrics, size: usize) {
        match self {
            Position::Client(..) => metrics.backend_bin += size,
            Position::Server => metrics.bin += size,
        }
    }

    /// Attribute `size` bytes written to the appropriate `SessionMetrics` field.
    pub fn count_bytes_out(&self, metrics: &mut SessionMetrics, size: usize) {
        match self {
            Position::Client(..) => metrics.backend_bout += size,
            Position::Server => metrics.bout += size,
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
            stream: Some(0),
            timeout_container,
            parked_on_buffer_pressure: false,
            close_notify_sent: false,
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
            stream: None,
            requests: 0,
            timeout_container,
            parked_on_buffer_pressure: false,
            close_notify_sent: false,
        })
    }

    pub fn new_h2_server(
        front_stream: Front,
        pool: Weak<RefCell<Pool>>,
        timeout_container: TimeoutContainer,
        flood_config: h2::H2FloodConfig,
    ) -> Option<Connection<Front>> {
        Some(Connection::H2(ConnectionH2::new(
            front_stream,
            Position::Server,
            pool,
            flood_config,
            timeout_container,
            Some((H2StreamId::Zero, h2::CLIENT_PREFACE_SIZE)),
            Ready::READABLE | Ready::HUP | Ready::ERROR,
        )?))
    }

    pub fn new_h2_client(
        front_stream: Front,
        cluster_id: String,
        backend: Rc<RefCell<Backend>>,
        pool: Weak<RefCell<Pool>>,
        timeout_container: TimeoutContainer,
        flood_config: h2::H2FloodConfig,
    ) -> Option<Connection<Front>> {
        Some(Connection::H2(ConnectionH2::new(
            front_stream,
            Position::Client(
                cluster_id,
                backend,
                BackendStatus::Connecting(Instant::now()),
            ),
            pool,
            flood_config,
            timeout_container,
            None,
            Ready::WRITABLE | Ready::HUP | Ready::ERROR,
        )?))
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

    /// Returns connection-level byte overhead (bin, bout) for H2, (0, 0) for H1.
    pub fn overhead_bytes(&self) -> (usize, usize) {
        match self {
            Connection::H1(_) => (0, 0),
            Connection::H2(c) => (c.bytes.overhead_bin, c.bytes.overhead_bout),
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

    /// Returns true if this connection could not read because its stream's
    /// kawa buffer was full. Used to prevent the dead-backend check from
    /// closing a backend that still has data in the OS socket buffer.
    fn has_buffer_pressure<L>(&self, context: &Context<L>) -> bool
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        match self {
            Connection::H1(c) => {
                let Some(stream_id) = c.stream else {
                    // No stream assigned — no buffer pressure.
                    return false;
                };
                let kawa = match c.position {
                    Position::Client(..) => &context.streams[stream_id].back,
                    Position::Server => &context.streams[stream_id].front,
                };
                kawa.storage.available_space() == 0
            }
            // H2 connections manage their own flow control via expect_read
            Connection::H2(_) => false,
        }
    }

    /// Re-enable READABLE if this connection is parked waiting for buffer space
    /// and the target stream's buffer now has enough room.
    ///
    /// For H1: checks the `parked_on_buffer_pressure` flag set when `readable`
    /// exits early because the kawa buffer was full. Edge-triggered epoll will
    /// not re-fire READABLE for data already in the kernel socket buffer, so
    /// this is the only path that re-arms it after the peer drains space.
    ///
    /// For H2: checks the `expect_read` field tracking which stream and how
    /// many bytes are needed.
    fn try_resume_reading<L>(&mut self, context: &Context<L>) -> bool
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        match self {
            Connection::H1(c) => {
                if !c.parked_on_buffer_pressure {
                    return false;
                }
                let Some(stream_id) = c.stream else {
                    return false;
                };
                let kawa = match c.position {
                    Position::Client(..) => &context.streams[stream_id].back,
                    Position::Server => &context.streams[stream_id].front,
                };
                if kawa.storage.available_space() > 0 {
                    trace!("H1 try_resume_reading: re-arming READABLE");
                    c.readiness.signal_pending_read();
                    true
                } else {
                    false
                }
            }
            Connection::H2(c) => c.try_resume_reading(context),
        }
    }

    fn graceful_goaway(&mut self) -> MuxResult {
        match self {
            Connection::H1(_) => MuxResult::Continue,
            Connection::H2(c) => c.graceful_goaway(),
        }
    }

    fn is_draining(&self) -> bool {
        match self {
            Connection::H1(_) => false,
            Connection::H2(c) => c.drain.draining,
        }
    }

    fn has_pending_write(&self) -> bool {
        match self {
            Connection::H1(c) => c.has_pending_write(),
            Connection::H2(c) => c.has_pending_write(),
        }
    }

    fn initiate_close_notify(&mut self) -> bool {
        match self {
            Connection::H1(c) => c.initiate_close_notify(),
            Connection::H2(c) => c.initiate_close_notify(),
        }
    }

    fn flush_zero_buffer(&mut self) {
        if let Connection::H2(c) = self {
            c.flush_zero_buffer();
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
                backend_borrow.active_requests = backend_borrow.active_requests.saturating_sub(1);
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
            // Preserve pending WRITABLE/HUP/ERROR bits on half-close.
            // The read side may be closed while we still need one last writable
            // pass to flush queued H2 control frames or TLS close_notify.
            readiness.event.remove(Ready::READABLE);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_readiness_after_read_closed_keeps_writable() {
        let mut readiness = Readiness {
            event: Ready::READABLE | Ready::WRITABLE | Ready::HUP,
            interest: Ready::READABLE | Ready::WRITABLE | Ready::HUP,
        };

        let should_yield = update_readiness_after_read(17, SocketResult::Closed, &mut readiness);

        assert!(!should_yield);
        assert!(!readiness.event.is_readable());
        assert!(readiness.event.is_writable());
        assert!(readiness.event.is_hup());
    }
}

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
    pub fn is_open(&self) -> bool {
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
    /// True when `gauge_add!("http.active_requests", 1)` was emitted for this stream.
    /// Prevents underflow when `generate_access_log` is called for streams that never
    /// had their request fully parsed (idle timeouts, malformed requests).
    pub request_counted: bool,
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
            .field("request_counted", &self.request_counted)
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
            window: i32::try_from(window).unwrap_or(i32::MAX),
            front_received_end_of_stream: false,
            back_received_end_of_stream: false,
            front_data_received: 0,
            back_data_received: 0,
            request_counted: false,
            front: GenericHttpStream::new(kawa::Kind::Request, kawa::Buffer::new(front_buffer)),
            back: GenericHttpStream::new(kawa::Kind::Response, kawa::Buffer::new(back_buffer)),
            context,
            metrics: SessionMetrics::new(None),
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
        message: Option<&str>,
        listener: Rc<RefCell<L>>,
    ) where
        L: ListenerHandler + L7ListenerHandler,
    {
        let context = &self.context;
        if self.request_counted {
            gauge_add!("http.active_requests", -1);
            self.request_counted = false;
        }
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
            message,
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
            #[cfg(feature = "opentelemetry")]
            otel: context.otel.as_ref(),
            #[cfg(not(feature = "opentelemetry"))]
            otel: None,
        };
        self.metrics.register_end_of_session(&context.log_context());
    }
}

/// Maximum number of debug events retained in the ring buffer.
/// Oldest entries are dropped when this limit is reached.
const DEBUG_HISTORY_CAPACITY: usize = 512;

pub struct DebugHistory {
    pub events: VecDeque<DebugEvent>,
    pub is_interesting: bool,
}
impl Default for DebugHistory {
    fn default() -> Self {
        Self {
            events: VecDeque::with_capacity(DEBUG_HISTORY_CAPACITY),
            is_interesting: false,
        }
    }
}
impl DebugHistory {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn push(&mut self, _event: DebugEvent) {
        #[cfg(debug_assertions)]
        {
            if self.events.len() >= DEBUG_HISTORY_CAPACITY {
                self.events.pop_front();
            }
            self.events.push_back(_event);
        }
    }
    pub fn set_interesting(&mut self, _interesting: bool) {
        #[cfg(debug_assertions)]
        {
            self.is_interesting = _interesting;
        }
    }
    pub fn is_interesting(&self) -> bool {
        #[cfg(debug_assertions)]
        {
            self.is_interesting
        }
        #[cfg(not(debug_assertions))]
        {
            false
        }
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
        let http_context = {
            let listener = self.listener.borrow();
            HttpContext::new(
                request_id,
                listener.protocol(),
                self.public_address,
                self.session_address,
                listener.get_sticky_name().to_string(),
            )
        };
        let recycle_slot = self
            .streams
            .iter()
            .position(|s| s.state == StreamState::Recycle);
        if let Some(stream_id) = recycle_slot {
            let stream = &mut self.streams[stream_id];
            trace!("Reuse stream: {}", stream_id);
            stream.state = StreamState::Idle;
            stream.attempts = 0;
            stream.front_received_end_of_stream = false;
            stream.back_received_end_of_stream = false;
            stream.front_data_received = 0;
            stream.back_data_received = 0;
            stream.request_counted = false;
            stream.window = i32::try_from(window).unwrap_or(i32::MAX);
            stream.context = http_context;
            stream.back.clear();
            stream.back.storage.clear();
            stream.front.clear();
            stream.front.storage.clear();
            stream.metrics.reset();
            stream.metrics.start = Some(Instant::now());
            // After recycling a slot, check if the Vec has excessive trailing
            // Recycle entries (more than 2x active streams of total capacity).
            let active = self.active_len();
            let total = self.streams.len();
            if total > 1 && active > 0 && total > active * 2 {
                self.shrink_trailing_recycle();
            }
            return Some(stream_id);
        }
        self.streams
            .push(Stream::new(self.pool.clone(), http_context, window)?);
        Some(self.streams.len() - 1)
    }

    /// Remove consecutive `Recycle` entries from the end of the streams Vec.
    ///
    /// This prevents unbounded growth when H2 streams are created and recycled
    /// over time, reclaiming memory from slots that are no longer needed.
    pub fn shrink_trailing_recycle(&mut self) {
        while self
            .streams
            .last()
            .is_some_and(|s| s.state == StreamState::Recycle)
        {
            self.streams.pop();
        }
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
        #[cfg(debug_assertions)]
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
        stream_context.cluster_id = Some(cluster_id.to_owned());

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
        H2 connecting strategy (least-loaded):
        - look at every backend connection
        - among connected backends for this cluster, pick the one with the fewest active streams
        - fall back to a connecting backend if no connected one exists
        - if no backend is to reuse, ask the router for a socket to the "next in line" backend

        H1 strategy: reuse the first KeepAlive backend for this cluster.
         */

        let mut reuse_token = None;
        let mut best_h2_stream_count = usize::MAX;
        for (token, backend) in &self.backends {
            match (h2, backend.position()) {
                (_, Position::Server) => {
                    error!("Backend connection unexpectedly behaves like a server");
                    continue;
                }
                (_, Position::Client(_, _, BackendStatus::Disconnecting)) => {}

                (true, Position::Client(other_cluster_id, _, BackendStatus::Connected)) => {
                    if *other_cluster_id == cluster_id {
                        // Pick the H2 connection with the fewest active streams
                        let stream_count = match backend {
                            Connection::H2(h2c) => h2c.streams.len(),
                            Connection::H1(_) => 0,
                        };
                        if stream_count < best_h2_stream_count {
                            best_h2_stream_count = stream_count;
                            reuse_token = Some(*token);
                        }
                    }
                }
                (true, Position::Client(other_cluster_id, _, BackendStatus::Connecting(_))) => {
                    // Only use a connecting backend if no connected one was found
                    if *other_cluster_id == cluster_id && best_h2_stream_count == usize::MAX {
                        reuse_token = Some(*token)
                    }
                }
                (true, Position::Client(other_cluster_id, _, BackendStatus::KeepAlive)) => {
                    if *other_cluster_id == cluster_id {
                        error!("ConnectionH2 unexpectedly behaves like H1 with KeepAlive");
                    }
                }

                (false, Position::Client(old_cluster_id, _, BackendStatus::KeepAlive)) => {
                    if *old_cluster_id == cluster_id {
                        reuse_token = Some(*token);
                        break;
                    }
                }
                // can't bundle H1 streams together
                (false, Position::Client(_, _, BackendStatus::Connected))
                | (false, Position::Client(_, _, BackendStatus::Connecting(_))) => {}
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
            let flood_config = context.listener.borrow().get_h2_flood_config();
            let connection = if h2 {
                match Connection::new_h2_client(
                    socket,
                    cluster_id,
                    backend,
                    context.pool.clone(),
                    timeout_container,
                    flood_config,
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
                return Err(RetrieveClusterError::RetrieveFrontend(frontend_error));
            }
        };

        let cluster_id = match route {
            Route::ClusterId(id) => id,
            Route::Deny => {
                trace!("Route::Deny");
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
                    .unwrap_or_else(|| backend.borrow().backend_id.to_owned()),
            );
        }

        context.backend_id = Some(backend.borrow().backend_id.to_owned());
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

impl<Front: SocketHandler + std::fmt::Debug, L: ListenerHandler + L7ListenerHandler> Mux<Front, L> {
    fn delay_close_for_frontend_flush(&mut self, reason: &'static str) -> bool {
        let _ = self.frontend.initiate_close_notify();
        if self.frontend.has_pending_write() {
            let readiness = self.frontend.readiness_mut();
            readiness.interest = Ready::WRITABLE | Ready::HUP | Ready::ERROR;
            readiness.signal_pending_write();
            warn!("Mux delaying close on {}: {:?}", reason, self.frontend);
            true
        } else {
            false
        }
    }
}

#[derive(Debug)]
pub enum DebugEvent {
    EV(Token, Ready),
    ReadyTimestamp(usize),
    LoopStart,
    LoopIteration(i32),
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
    StreamEvent(usize, usize),
    SocketIO(usize, usize, usize),
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
            if !self.delay_close_for_frontend_flush("frontend HUP") {
                warn!("Mux closing on frontend HUP: {:?}", self.frontend);
                return SessionResult::Close;
            }
        }

        // Start service timers on all active streams after the HUP check.
        // This mirrors session-level service_start/service_stop in Http(s)Session::ready()
        // to measure only CPU processing time, excluding epoll wait between cycles.
        for stream in &mut self.context.streams {
            if stream.state.is_open() {
                stream.metrics.service_start();
            }
        }

        let start = Instant::now();
        self.context.debug.push(DebugEvent::ReadyTimestamp(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as usize,
        ));
        trace!("{:?}", start);
        loop {
            self.context.debug.push(DebugEvent::LoopStart);
            loop {
                self.context.debug.push(DebugEvent::LoopIteration(counter));
                if self.frontend.readiness().filter_interest().is_readable() {
                    let res = {
                        let context = &mut self.context;
                        let res = self
                            .frontend
                            .readable(context, EndpointClient(&mut self.router));
                        context.debug.push(DebugEvent::SR(
                            self.frontend_token,
                            res,
                            self.frontend.readiness().clone(),
                        ));
                        res
                    };
                    match res {
                        MuxResult::Continue => {}
                        MuxResult::CloseSession => {
                            if !self.delay_close_for_frontend_flush("frontend readable") {
                                warn!("Mux close from frontend readable: {:?}", self.frontend);
                                return SessionResult::Close;
                            }
                        }
                        MuxResult::Upgrade => return SessionResult::Upgrade,
                    }
                }

                let mut all_backends_readiness_are_empty = true;
                let mut dead_backends = Vec::new();
                for (token, client) in self.router.backends.iter_mut() {
                    let readiness = client.readiness_mut();
                    // Check the raw event for HUP/ERROR — not filter_interest(),
                    // because interest only contains READABLE|WRITABLE and would
                    // always mask out HUP (0b01000) and ERROR (0b00100).
                    let dead = readiness.event.is_hup() || readiness.event.is_error();
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
                                #[cfg(debug_assertions)]
                                self.context
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

                                for stream in &mut self.context.streams {
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
                        let res = {
                            let context = &mut self.context;
                            let res = client.writable(context, EndpointServer(&mut self.frontend));
                            context.debug.push(DebugEvent::CW(
                                *token,
                                res,
                                client.readiness().clone(),
                            ));
                            res
                        };
                        match res {
                            MuxResult::Continue => {}
                            MuxResult::Upgrade => {
                                error!("only frontend connections can trigger Upgrade");
                            }
                            MuxResult::CloseSession => {
                                warn!(
                                    "Mux close from backend writable token={:?}: frontend={:?}",
                                    token, self.frontend
                                );
                                return SessionResult::Close;
                            }
                        }
                        // Cross-readiness: backend wrote → wake frontend reader
                        let context = &mut self.context;
                        self.frontend.try_resume_reading(context);
                    }

                    if client.readiness().filter_interest().is_readable() {
                        let res = {
                            let context = &mut self.context;
                            let res = client.readable(context, EndpointServer(&mut self.frontend));
                            context.debug.push(DebugEvent::CR(
                                *token,
                                res,
                                client.readiness().clone(),
                            ));
                            res
                        };
                        match res {
                            MuxResult::Continue => {}
                            MuxResult::Upgrade => {
                                error!("only frontend connections can trigger Upgrade (readable)");
                            }
                            MuxResult::CloseSession => {
                                warn!(
                                    "Mux close from backend readable token={:?}: frontend={:?}",
                                    token, self.frontend
                                );
                                return SessionResult::Close;
                            }
                        }
                    }

                    if dead
                        && !client.readiness().filter_interest().is_readable()
                        && !client.has_buffer_pressure(&self.context)
                    {
                        self.context
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
                                for stream in &mut self.context.streams {
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
                        client.close(&mut self.context, EndpointServer(&mut self.frontend));
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

                if self.frontend.readiness().filter_interest().is_writable() {
                    let res = {
                        let context = &mut self.context;
                        let res = self
                            .frontend
                            .writable(context, EndpointClient(&mut self.router));
                        context.debug.push(DebugEvent::SW(
                            self.frontend_token,
                            res,
                            self.frontend.readiness().clone(),
                        ));
                        res
                    };
                    match res {
                        MuxResult::Continue => {}
                        MuxResult::CloseSession => {
                            if !self.delay_close_for_frontend_flush("frontend writable") {
                                warn!("Mux close from frontend writable: {:?}", self.frontend);
                                return SessionResult::Close;
                            }
                        }
                        MuxResult::Upgrade => return SessionResult::Upgrade,
                    }
                    // Cross-readiness: frontend wrote → wake parked backends.
                    // If any backend resumes, invalidate the stale readiness
                    // flag so the inner loop continues instead of breaking.
                    let context = &mut self.context;
                    for (_token, backend) in self.router.backends.iter_mut() {
                        if backend.try_resume_reading(context) {
                            all_backends_readiness_are_empty = false;
                        }
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

        // Stop service timers before yielding to epoll, so idle wait time is excluded
        // from the service_time metric. For Close/Upgrade returns, close() handles cleanup.
        for stream in &mut self.context.streams {
            if stream.state.is_open() {
                stream.metrics.service_stop();
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
                        // The frontend timed out while a stream is linked to a backend.
                        // The backend timeout should handle this, but in case the backend
                        // is also stalled, send a 504 and terminate the stream.
                        if !stream.back.consumed {
                            let answers = answers_rc.borrow();
                            set_default_answer(stream, front_readiness, 504, &answers);
                            should_write = true;
                        } else if stream.back.is_completed() {
                            // Response fully proxied, stream can be closed
                        } else if stream.back.is_terminated() || stream.back.is_error() {
                            // Response is terminated/error but not fully written to frontend.
                            // Keep the session alive briefly to flush remaining data.
                            should_close = false;
                        } else {
                            // Partial response in progress — forcefully terminate
                            forcefully_terminate_answer(
                                stream,
                                front_readiness,
                                H2Error::InternalError,
                            );
                            should_write = true;
                        }
                        // end_stream is called in a second pass below to avoid
                        // borrow conflicts on context.streams.
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
            // Second pass: end streams that were linked to backends.
            // This is done separately to avoid borrow conflicts on context.streams.
            let linked_streams: Vec<(GlobalStreamId, Token)> = self
                .context
                .streams
                .iter()
                .enumerate()
                .filter_map(|(id, stream)| {
                    if let StreamState::Linked(back_token) = stream.state {
                        Some((id, back_token))
                    } else {
                        None
                    }
                })
                .collect();
            for (stream_id, back_token) in linked_streams {
                if let Some(backend) = self.router.backends.get_mut(&back_token) {
                    backend.end_stream(stream_id, &mut self.context);
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
                    }
                }
            }
            // Re-arm the backend timeout if the session stays alive (draining streams).
            // Without this, the timeout is consumed and the session becomes immortal
            // until the zombie checker runs.
            if !should_close {
                backend.timeout_container().set(token);
            }
        } else {
            // Session received a timeout for an unknown token, ignore it
            return StateResult::Continue;
        }
        if should_write {
            // Drain as much pending data as possible before closing.
            // A single writable() call is insufficient for large responses —
            // the TLS buffer may need multiple flushes. Without this loop,
            // the session is killed with unflushed TLS data, causing the
            // client to receive a truncated TLS record ("decode error").
            let mut result = StateResult::Continue;
            for _ in 0..16 {
                result = match self
                    .frontend
                    .writable(&mut self.context, EndpointClient(&mut self.router))
                {
                    MuxResult::Continue => StateResult::Continue,
                    MuxResult::Upgrade => StateResult::Upgrade,
                    MuxResult::CloseSession => StateResult::CloseSession,
                };
                if result != StateResult::Continue
                    || !self.frontend.readiness_mut().interest.is_writable()
                {
                    break;
                }
            }
            // Re-arm the frontend timeout so the session doesn't become immortal.
            // The writable call may have partially flushed the response — we need
            // the timeout to fire again if the flush stalls.
            if result == StateResult::Continue {
                self.frontend.timeout_container().set(self.frontend_token);
            }
            return result;
        }
        if should_close {
            if self.delay_close_for_frontend_flush("timeout") {
                warn!(
                    "Mux timeout delaying close for frontend flush: token={:?}, frontend={:?}",
                    token, self.frontend
                );
                self.frontend.timeout_container().set(self.frontend_token);
                return StateResult::Continue;
            }
            if front_is_h2 {
                warn!(
                    "Mux timeout returning CloseSession: token={:?}, frontend={:?}",
                    token, self.frontend
                );
                for (idx, stream) in self.context.streams.iter().enumerate() {
                    if stream.state != StreamState::Recycle {
                        warn!(
                            "  timeout stream[{}]: state={:?}, front_phase={:?}, back_phase={:?}, front_completed={}, back_completed={}",
                            idx,
                            stream.state,
                            stream.front.parsing_phase,
                            stream.back.parsing_phase,
                            stream.front.is_completed(),
                            stream.back.is_completed()
                        );
                    }
                }
            }
            StateResult::CloseSession
        } else {
            // Re-arm the frontend timeout. Without this, the timeout is consumed
            // by triggered() and the session stays alive indefinitely until the
            // zombie checker runs (default: 30 minutes).
            self.frontend.timeout_container().set(self.frontend_token);
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

        // Log active streams at session teardown for timeout diagnosis
        let active_count = self
            .context
            .streams
            .iter()
            .filter(|s| s.state.is_open() && s.metrics.start.is_some())
            .count();
        if active_count > 0 {
            debug!("Session close with {} active stream(s)", active_count);
            for (idx, stream) in self
                .context
                .streams
                .iter()
                .enumerate()
                .filter(|(_, s)| s.state.is_open() && s.metrics.start.is_some())
            {
                let elapsed = stream.metrics.service_time();
                debug!(
                    "  active stream[{}]: state={:?} service_time={:?} method={:?} path={:?} status={:?}",
                    idx,
                    stream.state,
                    elapsed,
                    stream.context.method,
                    stream.context.path,
                    stream.context.status,
                );
            }
            incr!("h2.close_with_active_streams");
        }

        // Distribute H2 connection-level overhead (control frames) across in-flight
        // streams so that access log bytes_in/bytes_out reflect actual wire cost.
        // Integer division may lose up to (active_count - 1) bytes, which is acceptable.
        let active_count = active_count.max(1);
        let (total_overhead_in, total_overhead_out) = self.frontend.overhead_bytes();
        let share_in = total_overhead_in / active_count;
        let share_out = total_overhead_out / active_count;

        // Generate access logs for in-flight streams on session teardown.
        // Skip streams that already had their access log emitted (metrics.start is
        // set to None by metrics.reset() after generate_access_log in the happy path).
        for stream in &mut self.context.streams {
            if stream.state.is_open() && stream.metrics.start.is_some() {
                stream.metrics.bin += share_in;
                stream.metrics.bout += share_out;
                stream.metrics.service_stop();
                if stream.metrics.backend_stop.is_none() {
                    stream.metrics.backend_stop();
                }
                // Only mark as error if the stream had an actual protocol/processing failure
                // (kawa parse error, backend error). Normal timeouts, client disconnects,
                // and graceful connection closures are not errors.
                let is_error = stream.front.is_error() || stream.back.is_error();
                stream.generate_access_log(
                    is_error,
                    Some("session close"),
                    self.context.listener.clone(),
                );
                stream.state = StreamState::Recycle;
            }
        }

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
    }

    fn shutting_down(&mut self) -> SessionIsToBeClosed {
        // RFC 9113 §6.8: initiate graceful shutdown with double-GOAWAY pattern.
        // Only send the phase-1 GOAWAY once. Phase 2 (final GOAWAY with the real
        // last_stream_id) is handled by finalize_write() when all streams drain.
        // Calling graceful_goaway() again would send phase 2 prematurely and
        // force-disconnect before in-flight streams complete.
        if !self.frontend.is_draining() {
            match self.frontend.graceful_goaway() {
                MuxResult::CloseSession => return true,
                MuxResult::Continue => {
                    // graceful_goaway() queued a GOAWAY frame. Flush it directly
                    // since the event loop uses edge-triggered epoll and won't
                    // deliver a new WRITABLE event for an already-writable socket.
                    self.frontend.flush_zero_buffer();
                }
                _ => {}
            }
        } else {
            trace!("shutting_down: already draining, skipping duplicate GOAWAY");
        }
        // Don't close while the frontend still has pending writes (GOAWAY, etc.)
        if self.frontend.has_pending_write() {
            return false;
        }
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
        if can_stop {
            let active_h2_streams = self
                .context
                .streams
                .iter()
                .enumerate()
                .filter(|(_, s)| s.state != StreamState::Recycle)
                .collect::<Vec<_>>();
            if matches!(self.frontend, Connection::H2(_)) && !active_h2_streams.is_empty() {
                warn!(
                    "Mux shutting_down returning true with active H2 streams: {:?}",
                    self.frontend
                );
                for (idx, stream) in active_h2_streams {
                    warn!(
                        "  shutdown stream[{}]: state={:?}, front_phase={:?}, back_phase={:?}, front_completed={}, back_completed={}",
                        idx,
                        stream.state,
                        stream.front.parsing_phase,
                        stream.back.parsing_phase,
                        stream.front.is_completed(),
                        stream.back.is_completed()
                    );
                }
            }
        }
        can_stop
    }
}
