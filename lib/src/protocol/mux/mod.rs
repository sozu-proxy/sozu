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
    fmt::Debug,
    io::ErrorKind,
    net::{Shutdown, SocketAddr},
    rc::{Rc, Weak},
    time::{Duration, Instant},
};

use mio::{Token, net::TcpStream};
use rusty_ulid::Ulid;
use sozu_command::{
    proto::command::{Event, EventKind},
    ready::Ready,
};

pub mod answers;
pub mod connection;
mod converter;
pub mod debug;
mod h1;
mod h2;
pub mod parser;
mod pkawa;
pub mod router;
mod serializer;
mod shared;
pub mod stream;

use crate::{
    BackendConnectionError, L7ListenerHandler, L7Proxy, ListenerHandler, ProxySession, Readiness,
    RetrieveClusterError, SessionIsToBeClosed, SessionMetrics, SessionResult, StateResult,
    backends::{Backend, BackendError},
    http::HttpListener,
    https::HttpsListener,
    pool::{Checkout, Pool},
    protocol::{SessionState, http::editor::HttpContext},
    retry::RetryPolicy,
    server::push_event,
    socket::{FrontRustls, SocketHandler, SocketResult},
};

pub(crate) use crate::protocol::mux::answers::{forcefully_terminate_answer, set_default_answer};
use crate::protocol::mux::connection::{EndpointClient, EndpointServer};
pub use crate::protocol::mux::{
    answers::terminate_default_answer,
    connection::Connection,
    debug::{DebugEvent, DebugHistory},
    h1::ConnectionH1,
    h2::ConnectionH2,
    h2::H2ByteAccounting,
    h2::H2ConnectionConfig,
    h2::H2DrainState,
    h2::H2FloodConfig,
    h2::H2FlowControl,
    parser::H2Error,
    router::Router,
    stream::{Stream, StreamParts, StreamState},
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

/// Shared logic for half-close accounting: clear `bit` from `readiness.event` on
/// socket errors/would-block, and return `true` (yield) when no bytes were
/// transferred so the caller can park the half. Rationale for clearing only
/// `bit` (not both halves) on `Closed`: the opposite half may still need one
/// last pass to flush queued frames or the TLS close_notify.
fn update_readiness(
    size: usize,
    status: SocketResult,
    readiness: &mut Readiness,
    bit: Ready,
) -> bool {
    trace!("  size={}, status={:?}", size, status);
    match status {
        SocketResult::Continue => {}
        SocketResult::Closed | SocketResult::Error | SocketResult::WouldBlock => {
            readiness.event.remove(bit);
        }
    }
    if size > 0 {
        false
    } else {
        readiness.event.remove(bit);
        true
    }
}

fn update_readiness_after_read(
    size: usize,
    status: SocketResult,
    readiness: &mut Readiness,
) -> bool {
    update_readiness(size, status, readiness, Ready::READABLE)
}

fn update_readiness_after_write(
    size: usize,
    status: SocketResult,
    readiness: &mut Readiness,
) -> bool {
    update_readiness(size, status, readiness, Ready::WRITABLE)
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

pub struct Context<L: ListenerHandler + L7ListenerHandler> {
    pub streams: Vec<Stream>,
    pub pool: Weak<RefCell<Pool>>,
    pub listener: Rc<RefCell<L>>,
    pub session_address: Option<SocketAddr>,
    pub public_address: SocketAddr,
    pub debug: DebugHistory,
    /// Shrink threshold ratio for recycled stream slots.
    /// Vec is shrunk when total_slots > active_streams * ratio.
    pub h2_stream_shrink_ratio: usize,
    /// TLS SNI value negotiated at handshake, propagated to every
    /// per-stream [`HttpContext`] so the routing layer can enforce
    /// the SNI ↔ `:authority` binding on every H2 stream (and the
    /// single H1 request). `None` for plaintext listeners or when
    /// the client omitted the SNI extension. Stored pre-lowercased
    /// and without a port for cheap exact-match comparison.
    pub tls_server_name: Option<String>,
    /// Whether the routing layer must reject any request whose authority
    /// host does not exact-match `tls_server_name` (CWE-346 / CWE-444).
    /// Mirrors `HttpsListenerConfig::strict_sni_binding`; captured once
    /// at `Context::new` so routing decisions on each stream avoid a
    /// per-stream `listener.borrow()`.
    pub strict_sni_binding: bool,
}

impl<L: ListenerHandler + L7ListenerHandler> Context<L> {
    pub fn new(
        pool: Weak<RefCell<Pool>>,
        listener: Rc<RefCell<L>>,
        session_address: Option<SocketAddr>,
        public_address: SocketAddr,
    ) -> Self {
        let h2_stream_shrink_ratio = listener
            .borrow()
            .get_h2_connection_config()
            .stream_shrink_ratio as usize;
        let strict_sni_binding = listener.borrow().get_strict_sni_binding();
        Self {
            streams: Vec::new(),
            pool,
            listener,
            session_address,
            public_address,
            debug: DebugHistory::new(),
            h2_stream_shrink_ratio,
            tls_server_name: None,
            strict_sni_binding,
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
            let mut http_context = HttpContext::new(
                request_id,
                listener.protocol(),
                self.public_address,
                self.session_address,
                listener.get_sticky_name().to_string(),
            );
            // Propagate the connection-scoped TLS SNI onto every per-stream
            // HttpContext so `route_from_request` can enforce the SNI ↔
            // `:authority` binding for each H2 stream independently.
            http_context.tls_server_name = self.tls_server_name.clone();
            // Mirror the listener's strict_sni_binding flag onto each
            // HttpContext so the routing layer can honor operator opt-outs
            // without reaching back into the listener on every request.
            http_context.strict_sni_binding = self.strict_sni_binding;
            http_context
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
            if total > 1 && active > 0 && total > active * self.h2_stream_shrink_ratio {
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
            debug!("Mux delaying close on {}: {:?}", reason, self.frontend);
            true
        } else {
            false
        }
    }

    /// Drive the frontend I/O path during shutdown, when the server is polling
    /// `shutting_down()` outside the normal epoll readiness loop.
    ///
    /// This is required for H2 graceful shutdown because a stream may still
    /// need one last readable pass to observe the peer's END_STREAM or one last
    /// writable pass to retire the stream, emit GOAWAY, or flush TLS records.
    fn drive_frontend_shutdown_io(&mut self) -> SessionIsToBeClosed {
        let force_h2_read = matches!(self.frontend, Connection::H2(_));
        let force_h2_write = matches!(self.frontend, Connection::H2(_));
        let readiness = self.frontend.readiness().clone();
        if !force_h2_read
            && !force_h2_write
            && readiness.event.is_empty()
            && !self.frontend.has_pending_write()
        {
            return false;
        }

        if force_h2_read || self.frontend.readiness().event.is_readable() {
            self.frontend
                .readiness_mut()
                .interest
                .insert(Ready::READABLE);
            match self
                .frontend
                .readable(&mut self.context, EndpointClient(&mut self.router))
            {
                MuxResult::Continue => {}
                MuxResult::CloseSession | MuxResult::Upgrade => return true,
            }
        }

        if !force_h2_write
            && !self.frontend.has_pending_write()
            && !self.frontend.readiness().event.is_writable()
        {
            return false;
        }

        let mut iterations = 0;
        loop {
            self.frontend
                .readiness_mut()
                .interest
                .insert(Ready::WRITABLE);
            if force_h2_write {
                self.frontend.readiness_mut().signal_pending_write();
            }
            match self
                .frontend
                .writable(&mut self.context, EndpointClient(&mut self.router))
            {
                MuxResult::Continue => {}
                MuxResult::CloseSession | MuxResult::Upgrade => return true,
            }

            iterations += 1;
            if iterations >= MAX_LOOP_ITERATIONS
                || (!self.frontend.has_pending_write()
                    && !self.frontend.readiness().event.is_writable())
            {
                break;
            }
        }
        false
    }
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

        if self.frontend.readiness().event.is_hup()
            && !self.delay_close_for_frontend_flush("frontend HUP")
        {
            debug!("Mux closing on frontend HUP: {:?}", self.frontend);
            return SessionResult::Close;
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
                                debug!("Mux close from frontend readable: {:?}", self.frontend);
                                return SessionResult::Close;
                            }
                        }
                        MuxResult::Upgrade => return SessionResult::Upgrade,
                    }
                }

                let mut all_backends_readiness_are_empty = true;
                let mut dead_backends = Vec::new();
                let mut backend_close: Option<(&'static str, Token)> = None;
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
                                backend_close = Some(("backend writable", *token));
                                break;
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
                                backend_close = Some(("backend readable", *token));
                                break;
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
                if let Some((reason, token)) = backend_close {
                    if !self.delay_close_for_frontend_flush(reason) {
                        debug!(
                            "Mux close from {} token={:?}: frontend={:?}",
                            reason, token, self.frontend
                        );
                        return SessionResult::Close;
                    }
                    all_backends_readiness_are_empty = false;
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
                                debug!("Mux close from frontend writable: {:?}", self.frontend);
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
                    if self.frontend.has_pending_write() {
                        debug!(
                            "Mux loop budget exhausted while frontend flush pending: {:?}",
                            self.frontend
                        );
                        self.frontend.readiness_mut().event.remove(Ready::WRITABLE);
                        self.frontend.timeout_container().set(self.frontend_token);
                        break;
                    }
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
                                BE::RetrieveClusterError(
                                    RetrieveClusterError::SniAuthorityMismatch { .. },
                                ) => {
                                    // RFC 9110 §15.5.20: 421 Misdirected Request is the
                                    // semantically correct status for an authority that
                                    // does not belong to this TLS connection. Sozu does
                                    // not currently ship a 421 answer template, so the
                                    // closest existing refusal (401 Unauthorized) is
                                    // used — still terminal, still sets keep_alive=false.
                                    // The dedicated http.sni_authority_mismatch metric
                                    // emitted in `route_from_request` is the durable
                                    // signal for operators. Upgrading to a real 421
                                    // template is tracked separately.
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
                debug!(
                    "Mux timeout delaying close for frontend flush: token={:?}, frontend={:?}",
                    token, self.frontend
                );
                self.frontend.timeout_container().set(self.frontend_token);
                return StateResult::Continue;
            }
            if front_is_h2 {
                debug!(
                    "Mux timeout returning CloseSession: token={:?}, frontend={:?}",
                    token, self.frontend
                );
                for (idx, stream) in self.context.streams.iter().enumerate() {
                    if stream.state != StreamState::Recycle {
                        debug!(
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
            // shut_down_sessions() runs outside ready(), so retry flushing any
            // previously-buffered GOAWAY/TLS records on each pass.
            self.frontend.flush_zero_buffer();
        }
        if self.drive_frontend_shutdown_io() {
            return true;
        }
        if matches!(self.frontend, Connection::H2(_)) && self.frontend.is_draining() {
            for stream in &mut self.context.streams {
                if stream.front_received_end_of_stream {
                    continue;
                }
                if !matches!(stream.state, StreamState::Linked(_) | StreamState::Unlinked) {
                    continue;
                }
                if stream.front.consumed
                    && stream.front.storage.is_empty()
                    && stream.front.is_completed()
                {
                    stream.front_received_end_of_stream = true;
                    self.frontend
                        .readiness_mut()
                        .interest
                        .insert(Ready::WRITABLE);
                    self.frontend.readiness_mut().signal_pending_write();
                }
            }
        }
        let mut can_stop = true;
        for stream in &mut self.context.streams {
            match stream.state {
                StreamState::Linked(_) => {
                    can_stop = false;
                }
                StreamState::Unlinked => {
                    kawa::debug_kawa(&stream.front);
                    kawa::debug_kawa(&stream.back);
                    if stream.is_quiesced() {
                        continue;
                    }
                    stream.context.closing = true;
                    can_stop = false;
                }
                _ => {}
            }
        }
        if self.frontend.has_pending_write() {
            return false;
        }
        if can_stop {
            let active_h2_streams = self
                .context
                .streams
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    if s.state == StreamState::Recycle {
                        return false;
                    }
                    if s.state == StreamState::Unlinked && s.is_quiesced() {
                        return false;
                    }
                    true
                })
                .collect::<Vec<_>>();
            if matches!(self.frontend, Connection::H2(_)) && !active_h2_streams.is_empty() {
                debug!(
                    "Mux shutting_down returning true with active H2 streams: {:?}",
                    self.frontend
                );
                for (idx, stream) in active_h2_streams {
                    debug!(
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
        if can_stop {
            return true;
        }

        false
    }
}
