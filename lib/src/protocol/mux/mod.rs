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
    sync::Arc,
    time::{Duration, Instant},
};

use mio::{Token, net::TcpStream};
use rusty_ulid::Ulid;
use sozu_command::{
    logging::ansi_palette,
    proto::command::{Event, EventKind},
    ready::Ready,
};

/// Protocol label + session descriptor used as a prefix on every [`Mux`] log
/// line. Matches the RUSTLS log-context convention:
/// `[<ulid> - - -]\tMUX\tSession(...)\t >>>`. When colored output is enabled
/// (via [`ansi_palette`]) the label is wrapped in bold bright-white ANSI
/// (uniform across every protocol) and the session detail block is rendered
/// in light grey.
///
/// Fields included in the session block:
/// - `frontend` — mio token of the frontend socket
/// - `peer` — peer address (or `None` if the socket is gone)
/// - `streams` — number of streams currently held by the [`Context`]
/// - `backends` — number of backend connections in the [`Router`]
/// - `pending_links` — streams waiting to be linked to a backend
/// - `readiness` — frontend mio readiness snapshot
macro_rules! log_context {
    ($self:expr) => {{
        let (open, reset, grey, gray, white) = ansi_palette();
        format!(
            "[{ulid} - - -]\t{open}MUX{reset}\t{grey}Session{reset}({gray}frontend{reset}={white}{frontend}{reset}, {gray}peer{reset}={white}{peer:?}{reset}, {gray}streams{reset}={white}{streams}{reset}, {gray}backends{reset}={white}{backends}{reset}, {gray}pending_links{reset}={white}{pending_links}{reset}, {gray}readiness{reset}={white}{readiness}{reset})\t >>>",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            ulid = $self.session_ulid,
            frontend = $self.frontend_token.0,
            peer = $self.frontend.socket().peer_addr().ok(),
            streams = $self.context.streams.len(),
            backends = $self.router.backends.len(),
            pending_links = $self.context.pending_links.len(),
            readiness = $self.frontend.readiness(),
        )
    }};
}

/// Lighter variant of [`log_context!`] that omits the
/// `streams`/`backends`/`pending_links` counts. Used at sites where the
/// borrow checker forbids reading `self.router.backends` or
/// `self.context.streams` (e.g. inside a method that already holds a mutable
/// borrow on one of them). The ULID and frontend snapshot still carry enough
/// context to correlate the line back to the rest of the session.
macro_rules! log_context_lite {
    ($self:expr) => {{
        let (open, reset, grey, gray, white) = ansi_palette();
        format!(
            "[{ulid} - - -]\t{open}MUX{reset}\t{grey}Session{reset}({gray}frontend{reset}={white}{frontend}{reset}, {gray}peer{reset}={white}{peer:?}{reset}, {gray}readiness{reset}={white}{readiness}{reset})\t >>>",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            ulid = $self.session_ulid,
            frontend = $self.frontend_token.0,
            peer = $self.frontend.socket().peer_addr().ok(),
            readiness = $self.frontend.readiness(),
        )
    }};
}

/// Module-level prefix for logs emitted from free functions or routing
/// blocks where no [`Mux`] is in scope. Honours the colored flag.
///
/// Two arms:
/// * `log_module_context!()` — zero-arg, legacy `MUX\t >>>` output. Kept
///   for sites without an `HttpContext` in scope (e.g. the generic
///   `trace!` that fires before the variant-specific match).
/// * `log_module_context!($http_context)` — rich form. `$http_context`
///   must be `&HttpContext`. Produces the same
///   `[session req cluster backend]` bracket as RUSTLS/PIPE/TCP followed
///   by a `Session(...)` block, so MUX lines emitted from variant match
///   arms stay filterable by session ULID or request ULID. Mirrors
///   `router.rs:log_module_context!($http_context)` (see there).
macro_rules! log_module_context {
    () => {{
        let (open, reset, _, _, _) = ansi_palette();
        format!("{open}MUX{reset}\t >>>", open = open, reset = reset)
    }};
    ($http_context:expr) => {{
        let (open, reset, grey, gray, white) = ansi_palette();
        let http_ctx: &HttpContext = &$http_context;
        let ctx = http_ctx.log_context();
        format!(
            "{gray}{ctx}{reset}\t{open}MUX{reset}\t{grey}Session{reset}({gray}frontend{reset}={white}{frontend:?}{reset}, {gray}method{reset}={white}{method:?}{reset}, {gray}authority{reset}={white}{authority:?}{reset})\t >>>",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            ctx = ctx,
            frontend = http_ctx.session_address,
            method = http_ctx.method,
            authority = http_ctx.authority,
        )
    }};
}

pub mod answers;
pub mod auth;
pub mod connection;
mod converter;
pub mod debug;
mod h1;
mod h2;
pub mod parser;
mod pkawa;
pub mod router;
pub(crate) mod serializer;
mod shared;
pub mod stream;

use crate::{
    BackendConnectionError, FrontendFromRequestError, L7ListenerHandler, L7Proxy, ListenerHandler,
    ProxySession, Readiness, RetrieveClusterError, SessionIsToBeClosed, SessionMetrics,
    SessionResult, StateResult,
    backends::{Backend, BackendError},
    http::HttpListener,
    https::HttpsListener,
    pool::{Checkout, Pool},
    protocol::{SessionState, http::editor::HttpContext},
    retry::RetryPolicy,
    server::push_event,
    socket::{FrontRustls, SessionTcpStream, SocketHandler, SocketResult, stats::socket_rtt},
};

pub(crate) use crate::protocol::mux::answers::{
    forcefully_terminate_answer, set_default_answer, set_default_answer_with_retry_after,
};
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
pub type MuxClear = Mux<SessionTcpStream, HttpListener>;
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
    /// Returns the underlying TCP socket for the peer side of a stream.
    ///
    /// Used by access-log emission to capture TCP_INFO RTT for the side the
    /// caller does NOT own directly: a frontend connection (Position::Server)
    /// reads the backend socket through this method, and a backend connection
    /// (Position::Client) reads the frontend socket the same way. `token` is
    /// ignored by [`super::connection::EndpointServer`] (which has a single
    /// frontend connection) and used as a key by
    /// [`super::connection::EndpointClient`] (which keys backends by token).
    /// Returns `None` when the token doesn't resolve, mirroring the existing
    /// fallback paths in `readiness`/`readiness_mut`.
    fn socket(&self, token: Token) -> Option<&TcpStream>;
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
    trace!(
        "{}   size={}, status={:?}",
        log_module_context!(),
        size,
        status
    );
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
pub struct Context<L: ListenerHandler + L7ListenerHandler> {
    pub streams: Vec<Stream>,
    /// Streams whose state is `StreamState::Link` and need backend connection.
    /// Replaces the O(n) scan of `streams` in the ready loop.
    pub pending_links: VecDeque<GlobalStreamId>,
    /// Reverse index: backend token -> global stream IDs currently in
    /// `StreamState::Linked(token)`. Eliminates O(n) scans of `streams`
    /// when handling backend connect/disconnect/timeout/close events.
    pub backend_streams: HashMap<Token, Vec<GlobalStreamId>>,
    pub pool: Weak<RefCell<Pool>>,
    pub listener: Rc<RefCell<L>>,
    /// Connection/session ULID — mirrors `Mux.session_ulid`. Stored here so
    /// per-stream `HttpContext` construction in [`Self::create_stream`] can
    /// stamp the session slot of the log-context bracket without reaching
    /// back into the parent [`Mux`].
    pub session_ulid: Ulid,
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
    /// Snapshot of the SAN set of the certificate Sōzu actually served at
    /// the TLS handshake. Captured once in `https.rs::upgrade_handshake`
    /// from the resolver and frozen for the connection lifetime so H2
    /// stream coalescing (RFC 7540 §9.1.1 / RFC 9113 §9.1.1) accepts any
    /// `:authority` covered by the certificate, with RFC 6125 §6.4.3
    /// wildcard handling. `None` for plaintext listeners or when SNI was
    /// absent. `Some(empty)` when the default cert was served — every
    /// `:authority` is rejected. `Arc` so the snapshot is shared across
    /// every per-stream `HttpContext` without re-allocation.
    pub tls_cert_names: Option<Arc<Vec<String>>>,
    /// Whether the routing layer must reject any request whose authority
    /// host does not exact-match `tls_server_name` (CWE-346 / CWE-444).
    /// Mirrors `HttpsListenerConfig::strict_sni_binding`; captured once
    /// at `Context::new` so routing decisions on each stream avoid a
    /// per-stream `listener.borrow()`.
    pub strict_sni_binding: bool,
    /// Whether the request-side block walk must strip any client-supplied
    /// `X-Real-IP` header before forwarding (anti-spoofing). Mirrors
    /// `HttpListenerConfig::elide_x_real_ip` /
    /// `HttpsListenerConfig::elide_x_real_ip`; captured once at
    /// `Context::new` so per-stream `HttpContext`s do not need to call
    /// `listener.borrow()` again. Independent of `send_x_real_ip`.
    pub elide_x_real_ip: bool,
    /// Whether `on_request_headers` injects a proxy-generated `X-Real-IP`
    /// header carrying the connection peer IP (post-PROXY-v2 unwrap).
    /// Mirrors `HttpListenerConfig::send_x_real_ip` /
    /// `HttpsListenerConfig::send_x_real_ip`; captured once at
    /// `Context::new`. Independent of `elide_x_real_ip`.
    pub send_x_real_ip: bool,
    /// Negotiated TLS protocol version short-form (e.g. `"TLSv1.3"`).
    /// Captured once at handshake completion in `https.rs` and propagated
    /// to every per-stream [`HttpContext`] so the access log can record it
    /// without reaching back into the rustls session per request. `None`
    /// for plaintext listeners.
    pub tls_version: Option<&'static str>,
    /// Negotiated TLS cipher suite short-form (e.g.
    /// `"TLS_AES_128_GCM_SHA256"`). Captured once at handshake completion
    /// and propagated to every per-stream [`HttpContext`]. `None` for
    /// plaintext listeners.
    pub tls_cipher: Option<&'static str>,
    /// Negotiated ALPN protocol short-form (e.g. `"h2"`, `"http/1.1"`).
    /// Captured once at handshake completion and propagated to every
    /// per-stream [`HttpContext`]. `None` for plaintext listeners or when
    /// no ALPN was negotiated.
    pub tls_alpn: Option<&'static str>,
}

impl<L: ListenerHandler + L7ListenerHandler> Context<L> {
    pub fn new(
        session_ulid: Ulid,
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
        let elide_x_real_ip = listener.borrow().get_elide_x_real_ip();
        let send_x_real_ip = listener.borrow().get_send_x_real_ip();
        Self {
            streams: Vec::new(),
            pending_links: VecDeque::new(),
            backend_streams: HashMap::new(),
            pool,
            listener,
            session_ulid,
            session_address,
            public_address,
            debug: DebugHistory::new(),
            h2_stream_shrink_ratio,
            tls_server_name: None,
            tls_cert_names: None,
            strict_sni_binding,
            elide_x_real_ip,
            send_x_real_ip,
            tls_version: None,
            tls_cipher: None,
            tls_alpn: None,
        }
    }

    pub fn active_len(&self) -> usize {
        self.streams
            .iter()
            .filter(|s| !matches!(s.state, StreamState::Recycle))
            .count()
    }

    /// Shared accessor for the [`HttpContext`] owned by a stream.
    ///
    /// Prefer this over `&self.streams[stream_id].context` at call sites
    /// that only need read access — it keeps the `Stream`/`HttpContext`
    /// relationship encapsulated and reads the same regardless of whether
    /// the caller is inside `Router::connect`, the H2 mux, or a free
    /// helper. Panics on an out-of-bounds `stream_id`, which is the same
    /// behaviour as the raw `streams[sid]` indexing it replaces.
    pub fn http_context(&self, stream_id: GlobalStreamId) -> &HttpContext {
        &self.streams[stream_id].context
    }

    /// Mutable sibling of [`Self::http_context`]. Use when routing
    /// decisions need to stamp `cluster_id` / `backend_id` on the stream's
    /// [`HttpContext`] (e.g. `Router::connect` at the fill-cluster /
    /// fill-backend points).
    pub fn http_context_mut(&mut self, stream_id: GlobalStreamId) -> &mut HttpContext {
        &mut self.streams[stream_id].context
    }

    /// Register a stream as linked to a backend token in the reverse index.
    pub fn link_stream(&mut self, stream_id: GlobalStreamId, token: Token) {
        self.streams[stream_id].state = StreamState::Linked(token);
        self.backend_streams
            .entry(token)
            .or_default()
            .push(stream_id);
    }

    /// Remove a stream from the backend reverse index if it is currently
    /// `Linked`. Returns the backend token if one was removed.
    pub fn unlink_stream(&mut self, stream_id: GlobalStreamId) -> Option<Token> {
        if let StreamState::Linked(token) = self.streams[stream_id].state {
            remove_backend_stream(&mut self.backend_streams, token, stream_id);
            Some(token)
        } else {
            None
        }
    }

    pub fn create_stream(&mut self, request_id: Ulid, window: u32) -> Option<GlobalStreamId> {
        let http_context = {
            let listener = self.listener.borrow();
            let mut http_context = HttpContext::new(
                self.session_ulid,
                request_id,
                listener.protocol(),
                self.public_address,
                self.session_address,
                listener.get_sticky_name().to_string(),
                listener.get_sozu_id_header().to_string(),
                self.elide_x_real_ip,
                self.send_x_real_ip,
            );
            // Propagate the connection-scoped TLS SNI onto every per-stream
            // HttpContext so `route_from_request` can enforce the SNI ↔
            // `:authority` binding for each H2 stream independently.
            http_context.tls_server_name = self.tls_server_name.clone();
            // Mirror the frozen-at-handshake SAN snapshot. `Arc` clone is a
            // refcount bump, not a deep copy — every per-stream
            // `HttpContext` shares the same `Vec<String>`.
            http_context.tls_cert_names = self.tls_cert_names.clone();
            // Mirror the listener's strict_sni_binding flag onto each
            // HttpContext so the routing layer can honor operator opt-outs
            // without reaching back into the listener on every request.
            http_context.strict_sni_binding = self.strict_sni_binding;
            // Propagate the connection-scoped TLS metadata onto every
            // per-stream HttpContext so the access log can record it without
            // touching the rustls session on every request. These are
            // `&'static str` borrows from the rustls label tables — copy is
            // a pointer move.
            http_context.tls_version = self.tls_version;
            http_context.tls_cipher = self.tls_cipher;
            http_context.tls_alpn = self.tls_alpn;
            http_context
        };
        let recycle_slot = self
            .streams
            .iter()
            .position(|s| s.state == StreamState::Recycle);
        if let Some(stream_id) = recycle_slot {
            let stream = &mut self.streams[stream_id];
            trace!("{} Reuse stream: {}", log_module_context!(), stream_id);
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

/// Remove `stream_id` from the backend-token reverse index for `token`.
/// Free function to allow split borrows when `context.streams` is already
/// mutably borrowed (preventing a `Context::unlink_stream` call).
pub(super) fn remove_backend_stream(
    index: &mut HashMap<Token, Vec<GlobalStreamId>>,
    token: Token,
    stream_id: GlobalStreamId,
) {
    if let Some(ids) = index.get_mut(&token) {
        ids.retain(|&id| id != stream_id);
        if ids.is_empty() {
            index.remove(&token);
        }
    }
}

pub struct Mux<Front: SocketHandler, L: ListenerHandler + L7ListenerHandler> {
    pub configured_frontend_timeout: Duration,
    pub frontend_token: Token,
    pub frontend: Connection<Front>,
    pub router: Router,
    pub context: Context<L>,
    /// Per-session correlation ID generated at construction time. Included in
    /// every log line emitted from this module so all events for a single
    /// frontend connection can be reassembled (independent of the ephemeral
    /// per-stream request id used by access logs).
    pub session_ulid: Ulid,
}

impl<Front: SocketHandler, L: ListenerHandler + L7ListenerHandler> Mux<Front, L> {
    pub fn front_socket(&self) -> &TcpStream {
        self.frontend.socket()
    }
}

impl<Front: SocketHandler + std::fmt::Debug, L: ListenerHandler + L7ListenerHandler> Mux<Front, L> {
    fn delay_close_for_frontend_flush(&mut self, reason: &'static str) -> bool {
        let _ = self.frontend.initiate_close_notify();
        // LIFECYCLE §9 invariant 16: consult per-stream back-buffers in
        // addition to the connection-level pending-write predicate so
        // shutdown does not close while any open H2 stream still has
        // kawa bytes queued after a voluntary scheduler yield.
        if self
            .frontend
            .has_pending_write_including_streams(&self.context)
        {
            let readiness = self.frontend.readiness_mut();
            readiness.interest = Ready::WRITABLE | Ready::HUP | Ready::ERROR;
            readiness.signal_pending_write();
            debug!(
                "{} Mux delaying close on {}: {:?}",
                log_context!(self),
                reason,
                self.frontend
            );
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
            debug!(
                "{} Mux closing on frontend HUP: {:?}",
                log_context!(self),
                self.frontend
            );
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
        trace!("{} {:?}", log_context!(self), start);
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
                                debug!(
                                    "{} Mux close from frontend readable: {:?}",
                                    log_context!(self),
                                    self.frontend
                                );
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
                        trace!(
                            "{} Backend({:?}) -> {:?}",
                            log_context_lite!(self),
                            token,
                            readiness
                        );
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
                                        "{} backend server {} at {} is up",
                                        log_context_lite!(self),
                                        backend_borrow.backend_id,
                                        backend_borrow.address
                                    );
                                    incr!(
                                        "backend.up",
                                        Some(cluster_id),
                                        Some(&backend_borrow.backend_id)
                                    );
                                    gauge!(
                                        "backend.available",
                                        1,
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

                                if let Some(ids) = self.context.backend_streams.get(token) {
                                    for &stream_id in ids {
                                        self.context.streams[stream_id].metrics.backend_connected();
                                        backend_borrow.active_requests += 1;
                                    }
                                }
                                trace!(
                                    "{} connection success: {:#?}",
                                    log_context_lite!(self),
                                    backend_borrow
                                );
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
                                error!(
                                    "{} backend connection cannot be in Server position",
                                    log_context_lite!(self)
                                );
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
                                error!(
                                    "{} only frontend connections can trigger Upgrade",
                                    log_context_lite!(self)
                                );
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
                                error!(
                                    "{} only frontend connections can trigger Upgrade (readable)",
                                    log_context_lite!(self)
                                );
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
                        trace!("{} Closing {:#?}", log_context_lite!(self), client);
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
                                        "{} backend server {} at {} is down",
                                        log_context_lite!(self),
                                        backend_borrow.backend_id,
                                        backend_borrow.address
                                    );
                                    incr!(
                                        "backend.down",
                                        Some(cluster_id),
                                        Some(&backend_borrow.backend_id)
                                    );
                                    gauge!(
                                        "backend.available",
                                        0,
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
                                trace!(
                                    "{} connection fail: {:#?}",
                                    log_context_lite!(self),
                                    backend_borrow
                                );
                            }
                            Position::Client(_, backend, _) => {
                                let mut backend_borrow = backend.borrow_mut();
                                let count = self
                                    .context
                                    .backend_streams
                                    .get(token)
                                    .map_or(0, |ids| ids.len());
                                backend_borrow.active_requests =
                                    backend_borrow.active_requests.saturating_sub(count);
                            }
                            Position::Server => {
                                error!(
                                    "{} dead backend cannot be in Server position",
                                    log_context_lite!(self)
                                );
                            }
                        }
                        client.close(&mut self.context, EndpointServer(&mut self.frontend));
                        dead_backends.push(*token);
                    }

                    if !client.readiness().filter_interest().is_empty() {
                        all_backends_readiness_are_empty = false;
                    }
                }
                // Remove dead backends from the map BEFORE handling
                // backend_close. client.close() already decremented
                // connections_per_backend / backend.connections gauges in
                // the loop above; if we return SessionResult::Close before
                // removing them, Mux::close() would decrement again
                // (double-decrement → gauge underflow).
                if !dead_backends.is_empty() {
                    for token in &dead_backends {
                        let proxy_borrow = proxy.borrow();
                        if let Some(mut client) = self.router.backends.remove(token) {
                            client.timeout_container().cancel();
                            let socket = client.socket_mut();
                            if let Err(e) = proxy_borrow.deregister_socket(socket) {
                                error!(
                                    "{} error deregistering back socket({:?}): {:?}",
                                    log_context!(self),
                                    socket,
                                    e
                                );
                            }
                            // invariant: write-only shutdown — Shutdown::Both on a TLS frontend
                            // discards the receive buffer and elicits TCP RST, truncating the
                            // already-queued response. Canonical write-up: `lib/src/https.rs:650-655`.
                            // Backend sockets follow the same discipline for symmetry.
                            if let Err(e) = socket.shutdown(Shutdown::Write) {
                                if e.kind() != ErrorKind::NotConnected {
                                    error!(
                                        "{} error shutting down back socket({:?}): {:?}",
                                        log_context!(self),
                                        socket,
                                        e
                                    );
                                }
                            }
                        } else {
                            error!("{} session {:?} has no backend!", log_context!(self), token);
                        }
                        if !proxy_borrow.remove_session(*token) {
                            error!(
                                "{} session {:?} was already removed!",
                                log_context!(self),
                                token
                            );
                        }
                    }
                    trace!("{} FRONTEND: {:#?}", log_context!(self), self.frontend);
                    trace!(
                        "{} BACKENDS: {:#?}",
                        log_context!(self),
                        self.router.backends
                    );
                }
                if let Some((reason, token)) = backend_close {
                    if !self.delay_close_for_frontend_flush(reason) {
                        debug!(
                            "{} Mux close from {} token={:?}: frontend={:?}",
                            log_context!(self),
                            reason,
                            token,
                            self.frontend
                        );
                        return SessionResult::Close;
                    }
                    all_backends_readiness_are_empty = false;
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
                                debug!(
                                    "{} Mux close from frontend writable: {:?}",
                                    log_context!(self),
                                    self.frontend
                                );
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
                            "{} Mux loop budget exhausted while frontend flush pending: {:?}",
                            log_context!(self),
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
            while let Some(stream_id) = context.pending_links.pop_front() {
                let Some(stream) = context.streams.get(stream_id) else {
                    continue;
                };
                if stream.state != StreamState::Link {
                    continue;
                }
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
                    self.frontend_token,
                ) {
                    Ok(_) => {
                        let state = context.streams[stream_id].state;
                        context.debug.push(DebugEvent::CC(stream_id, state));
                    }
                    Err(error) => {
                        trace!("{} Connection error: {}", log_module_context!(), error);
                        let stream = &mut context.streams[stream_id];
                        let answers = answers_rc.borrow();
                        use BackendConnectionError as BE;
                        match error {
                            BE::MaxConnectionRetries(_)
                            | BE::MaxSessionsMemory
                            | BE::MaxBuffers => {
                                warn!(
                                    "{} backend retry budget exhausted: {}",
                                    log_module_context!(stream.context),
                                    error
                                );
                                set_default_answer(stream, front_readiness, 503, &answers);
                            }
                            BE::Backend(BackendError::NoBackendForCluster(_)) => {
                                set_default_answer(stream, front_readiness, 503, &answers);
                            }
                            BE::RetrieveClusterError(RetrieveClusterError::RetrieveFrontend(
                                ref err,
                            )) => {
                                // RFC 9110 §15.5.1: a malformed authority is a
                                // 400. A syntactically valid authority that
                                // simply has no matching frontend stays on the
                                // historical 404 path.
                                let code = match err {
                                    FrontendFromRequestError::HostParse { .. }
                                    | FrontendFromRequestError::InvalidCharsAfterHost(_) => 400,
                                    FrontendFromRequestError::NoClusterFound(_) => 404,
                                };
                                set_default_answer(stream, front_readiness, code, &answers);
                            }
                            BE::RetrieveClusterError(RetrieveClusterError::UnauthorizedRoute) => {
                                set_default_answer(stream, front_readiness, 401, &answers);
                            }
                            BE::RetrieveClusterError(
                                RetrieveClusterError::SniAuthorityMismatch { .. },
                            ) => {
                                // RFC 9110 §15.5.20: 421 Misdirected Request is the
                                // semantically correct status for an authority that
                                // does not belong to this TLS connection. The
                                // http.sni_authority_mismatch metric emitted in
                                // `route_from_request` remains the durable signal;
                                // the 421 body here is what a client sees and may
                                // retry on a fresh TLS connection with a matching SNI.
                                set_default_answer(stream, front_readiness, 421, &answers);
                            }
                            BE::RetrieveClusterError(RetrieveClusterError::HttpsRedirect) => {
                                // Use the redirect status stashed by `Router::route_from_request`
                                // (#1009). Falls back to 301 for the legacy
                                // `cluster.https_redirect = true` path that does
                                // not set the field.
                                let code = stream.context.redirect_status.unwrap_or(301);
                                set_default_answer(stream, front_readiness, code, &answers);
                            }

                            BE::Backend(ref e) => {
                                error!("{} backend connection error: {}", log_module_context!(), e);
                                set_default_answer(stream, front_readiness, 503, &answers);
                            }
                            BE::RetrieveClusterError(ref other) => {
                                error!(
                                    "{} unexpected RetrieveClusterError variant: {:?}",
                                    log_module_context!(),
                                    other
                                );
                                set_default_answer(stream, front_readiness, 503, &answers);
                            }
                            // TCP specific error
                            BE::NotFound(ref msg) => {
                                error!(
                                    "{} NotFound is TCP-specific, not reachable in mux: {:?}",
                                    log_module_context!(),
                                    msg
                                );
                                set_default_answer(stream, front_readiness, 503, &answers);
                            }
                            // Per-(cluster, source-IP) connection limit reached.
                            // Emit HTTP 429 with the resolved `Retry-After`. The
                            // value is computed in `Router::connect` (where the
                            // SessionManager + cluster override are reachable)
                            // and stashed on the stream context just before the
                            // error is returned, so the answer engine can render
                            // (or elide) the header without re-deriving the
                            // resolution chain here.
                            BE::TooManyConnectionsPerIp { ref cluster_id } => {
                                debug!(
                                    "{} per-(cluster, source-IP) limit hit for cluster {:?}",
                                    log_module_context!(),
                                    cluster_id
                                );
                                let retry_after = stream.context.retry_after_seconds;
                                set_default_answer_with_retry_after(
                                    stream,
                                    front_readiness,
                                    429,
                                    &answers,
                                    retry_after,
                                );
                            }
                        }
                        context.debug.push(DebugEvent::CCF(stream_id, error));
                    }
                }
                // All routing error arms now set a default answer, transitioning
                // the stream out of Link state. No re-enqueue needed.
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

        #[cfg(debug_assertions)]
        {
            // Verify backend_streams index matches actual stream states.
            let mut expected: HashMap<Token, Vec<GlobalStreamId>> = HashMap::new();
            for (id, stream) in self.context.streams.iter().enumerate() {
                if let StreamState::Linked(token) = stream.state {
                    expected.entry(token).or_default().push(id);
                }
            }
            assert_eq!(
                expected.len(),
                self.context.backend_streams.len(),
                "backend_streams index key count mismatch: expected={:?}, actual={:?}",
                expected,
                self.context.backend_streams
            );
            for (token, mut expected_ids) in expected {
                let mut actual_ids = self
                    .context
                    .backend_streams
                    .get(&token)
                    .cloned()
                    .unwrap_or_default();
                expected_ids.sort();
                actual_ids.sort();
                assert_eq!(
                    expected_ids, actual_ids,
                    "backend_streams index mismatch for token {token:?}",
                );
            }
        }

        SessionResult::Continue
    }

    fn update_readiness(&mut self, token: Token, events: Ready) {
        trace!("{} EVENTS: {:?} on {:?}", log_context!(self), events, token);
        self.context.debug.push(DebugEvent::EV(token, events));
        if token == self.frontend_token {
            self.frontend.readiness_mut().event |= events;
        } else if let Some(c) = self.router.backends.get_mut(&token) {
            c.readiness_mut().event |= events;
        }
    }

    fn timeout(&mut self, token: Token, _metrics: &mut SessionMetrics) -> StateResult {
        trace!("{} MuxState::timeout({:?})", log_context!(self), token);
        let front_is_h2 = match self.frontend {
            Connection::H1(_) => false,
            Connection::H2(_) => true,
        };
        let answers_rc = self.context.listener.borrow().get_answers().clone();
        let mut should_close = true;
        let mut should_write = false;
        if self.frontend_token == token {
            trace!(
                "{} MuxState::timeout_frontend({:#?})",
                log_context!(self),
                self.frontend
            );
            self.frontend.timeout_container().triggered();
            let front_readiness = self.frontend.readiness_mut();
            for stream_id in 0..self.context.streams.len() {
                match self.context.streams[stream_id].state {
                    StreamState::Idle => {
                        // In h1 an Idle stream is always the first request, so we can send a 408
                        // In h2 an Idle stream doesn't necessarily hold a request yet,
                        // in most cases it was just reserved, so we can just ignore them.
                        if !front_is_h2 {
                            let answers = answers_rc.borrow();
                            let stream = &mut self.context.streams[stream_id];
                            stream.context.access_log_message = Some("client_timeout");
                            set_default_answer(stream, front_readiness, 408, &answers);
                            should_write = true;
                        }
                    }
                    StreamState::Link => {
                        // This is an unusual case, as we have both a complete request and no
                        // available backend yet. For now, we answer with 503.
                        // Not a timeout-driven outcome from the operator's
                        // perspective — leave access_log_message as None.
                        let answers = answers_rc.borrow();
                        let stream = &mut self.context.streams[stream_id];
                        set_default_answer(stream, front_readiness, 503, &answers);
                        should_write = true;
                    }
                    StreamState::Linked(_) => {
                        // The frontend timed out while a stream is linked to a backend.
                        // The backend timeout should handle this, but in case the backend
                        // is also stalled, send a 504 and terminate the stream.
                        if !self.context.streams[stream_id].back.consumed {
                            self.context.unlink_stream(stream_id);
                            let answers = answers_rc.borrow();
                            let stream = &mut self.context.streams[stream_id];
                            stream.context.access_log_message =
                                Some("client_timeout_during_response");
                            set_default_answer(stream, front_readiness, 504, &answers);
                            should_write = true;
                        } else if self.context.streams[stream_id].back.is_completed() {
                            // Response fully proxied, stream can be closed
                        } else if self.context.streams[stream_id].back.is_terminated()
                            || self.context.streams[stream_id].back.is_error()
                        {
                            // Response is terminated/error but not fully written to frontend.
                            // Keep the session alive briefly to flush remaining data.
                            should_close = false;
                        } else {
                            // Partial response in progress — forcefully terminate
                            self.context.unlink_stream(stream_id);
                            let stream = &mut self.context.streams[stream_id];
                            stream.context.access_log_message =
                                Some("client_timeout_during_response");
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
                        if !self.context.streams[stream_id].back.is_completed() {
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
            trace!(
                "{} MuxState::timeout_backend({:#?})",
                log_context_lite!(self),
                backend
            );
            backend.timeout_container().triggered();
            let front_readiness = self.frontend.readiness_mut();
            let linked_ids: Vec<GlobalStreamId> = self
                .context
                .backend_streams
                .get(&token)
                .map_or_else(Vec::new, |ids| ids.to_owned());
            for stream_id in linked_ids {
                // This stream is linked to the backend that timedout
                if self.context.streams[stream_id].back.is_terminated()
                    || self.context.streams[stream_id].back.is_error()
                {
                    trace!(
                        "{} Stream terminated or in error, do nothing, just wait a bit more",
                        log_module_context!()
                    );
                    // Nothing to do, simply wait for the remaining bytes to be proxied
                    if !self.context.streams[stream_id].back.is_completed() {
                        should_close = false;
                    }
                } else if !self.context.streams[stream_id].back.consumed {
                    // The response has not started yet
                    trace!(
                        "{} Stream still waiting for response, send 504",
                        log_module_context!()
                    );
                    self.context.unlink_stream(stream_id);
                    let answers = answers_rc.borrow();
                    let stream = &mut self.context.streams[stream_id];
                    stream.context.access_log_message = Some("backend_timeout");
                    set_default_answer(stream, front_readiness, 504, &answers);
                    should_write = true;
                } else {
                    trace!(
                        "{} Stream waiting for end of response, forcefully terminate it",
                        log_module_context!()
                    );
                    self.context.unlink_stream(stream_id);
                    let stream = &mut self.context.streams[stream_id];
                    stream.context.access_log_message = Some("backend_response_timeout");
                    forcefully_terminate_answer(stream, front_readiness, H2Error::InternalError);
                    should_write = true;
                }
                backend.end_stream(stream_id, &mut self.context);
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
            //
            // The constant 16 is empirical: it papers over a missing
            // invariant-15 hop in the H2 mux state machine where the
            // writable readiness signal is not always re-armed after a
            // partial flush. Long-term plan: reach invariant-15 closure
            // and remove this loop. See `lib/src/protocol/mux/LIFECYCLE.md`.
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
                    "{} Mux timeout delaying close for frontend flush: token={:?}, frontend={:?}",
                    log_context!(self),
                    token,
                    self.frontend
                );
                self.frontend.timeout_container().set(self.frontend_token);
                return StateResult::Continue;
            }
            if front_is_h2 {
                debug!(
                    "{} Mux timeout returning CloseSession: token={:?}, frontend={:?}",
                    log_context!(self),
                    token,
                    self.frontend
                );
                for (idx, stream) in self.context.streams.iter().enumerate() {
                    if stream.state != StreamState::Recycle {
                        debug!(
                            "{}   timeout stream[{}]: state={:?}, front_phase={:?}, back_phase={:?}, front_completed={}, back_completed={}",
                            log_context!(self),
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
        trace!("{} MuxState::cancel_timeouts", log_context!(self));
        self.frontend.timeout_container().cancel();
        for backend in self.router.backends.values_mut() {
            backend.timeout_container().cancel();
        }
    }

    fn print_state(&self, _context: &str) {
        // The trait-required `context: &str` parameter (protocol tag like
        // "HTTPS"/"HTTP" passed by callers) predates the unified
        // `log_context!(self)` envelope. The canonical `MUX` tag lives
        // inside `log_context!`, so we ignore the parameter here and emit
        // the bracketed Session(...) block instead, mirroring the second
        // `error!` in this function.
        error!(
            "\
{} Session(Mux)
\tFrontend:
\t\ttoken: {:?}\treadiness: {:?}
\tBackend(s):",
            log_context!(self),
            self.frontend_token,
            self.frontend.readiness()
        );
        for (backend_token, backend) in &self.router.backends {
            error!(
                "{} \t\ttoken: {:?}\treadiness: {:?}",
                log_context!(self),
                backend_token,
                backend.readiness()
            )
        }
    }

    fn close(&mut self, proxy: Rc<RefCell<dyn L7Proxy>>, _metrics: &mut SessionMetrics) {
        if self.context.debug.is_interesting() {
            warn!("{} {:?}", log_context!(self), self.context.debug.events);
        }
        debug!("{} MUX CLOSE", log_context!(self));
        trace!("{} FRONTEND: {:#?}", log_context!(self), self.frontend);
        trace!(
            "{} BACKENDS: {:#?}",
            log_context!(self),
            self.router.backends
        );

        // Log active streams at session teardown for timeout diagnosis
        let active_count = self
            .context
            .streams
            .iter()
            .filter(|s| s.state.is_open() && s.metrics.start.is_some())
            .count();
        if active_count > 0 {
            debug!(
                "{} Session close with {} active stream(s)",
                log_context!(self),
                active_count
            );
            for (idx, stream) in self
                .context
                .streams
                .iter()
                .enumerate()
                .filter(|(_, s)| s.state.is_open() && s.metrics.start.is_some())
            {
                let elapsed = stream.metrics.service_time();
                debug!(
                    "{}   active stream[{}]: state={:?} service_time={:?} method={:?} path={:?} status={:?}",
                    log_context!(self),
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
        // Frontend RTT is the same for every stream on this session — snapshot
        // it once outside the loop instead of paying one TCP_INFO syscall per
        // open stream.
        let client_rtt = socket_rtt(self.frontend.socket());
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
                let server_rtt = stream.linked_token().and_then(|token| {
                    self.router
                        .backends
                        .get(&token)
                        .and_then(|c| socket_rtt(c.socket()))
                });
                stream.generate_access_log(
                    is_error,
                    Some("session close"),
                    self.context.listener.clone(),
                    client_rtt,
                    server_rtt,
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
                error!(
                    "{} error deregistering back socket({:?}): {:?}",
                    log_context_lite!(self),
                    socket,
                    e
                );
            }
            // invariant: write-only shutdown — Shutdown::Both on a TLS frontend
            // discards the receive buffer and elicits TCP RST, truncating the
            // already-queued response. Canonical write-up: `lib/src/https.rs:650-655`.
            // Backend sockets follow the same discipline for symmetry.
            if let Err(e) = socket.shutdown(Shutdown::Write) {
                if e.kind() != ErrorKind::NotConnected {
                    error!(
                        "{} error shutting down back socket({:?}): {:?}",
                        log_context_lite!(self),
                        socket,
                        e
                    );
                }
            }
            if !proxy_borrow.remove_session(*token) {
                error!(
                    "{} session {:?} was already removed!",
                    log_context_lite!(self),
                    token
                );
            }

            match client.position() {
                Position::Client(cluster_id, backend, _) => {
                    let mut backend_borrow = backend.borrow_mut();
                    backend_borrow.dec_connections();
                    gauge_add!("backend.connections", -1);
                    // Second `-1` site for `backend.pool.size` (the first is
                    // in `connection.rs::pre_close_client_bookkeeping`). This
                    // path runs during session teardown when the frontend
                    // session iterates the backends map directly without
                    // routing through `Connection::close`. Both `-1` sites
                    // mirror the single `+1` in router.rs::connect and the
                    // matching `backend.connections, -1` calls already
                    // present here, so symmetry follows from
                    // `backend.connections` correctness.
                    gauge_add!("backend.pool.size", -1);
                    gauge_add!(
                        "connections_per_backend",
                        -1,
                        Some(cluster_id),
                        Some(&backend_borrow.backend_id)
                    );
                    let count = self
                        .context
                        .backend_streams
                        .get(token)
                        .map_or(0, |ids| ids.len());
                    backend_borrow.active_requests =
                        backend_borrow.active_requests.saturating_sub(count);
                    trace!(
                        "{} connection (session) closed: {:#?}",
                        log_context_lite!(self),
                        backend_borrow
                    );
                }
                Position::Server => {
                    error!(
                        "{} close_backend called on Server position",
                        log_context_lite!(self)
                    );
                }
            }
        }
        // Clear the reverse index after all backends have decremented their
        // active_requests counters (which depend on the index for stream counts).
        self.context.backend_streams.clear();
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
            trace!(
                "{} shutting_down: already draining, skipping duplicate GOAWAY",
                log_context!(self)
            );
            // shut_down_sessions() runs outside ready(), so retry flushing any
            // previously-buffered GOAWAY/TLS records on each pass.
            self.frontend.flush_zero_buffer();
        }
        if self.drive_frontend_shutdown_io() {
            return true;
        }
        // Forced-close deadline: once the H2 listener's
        // `h2_graceful_shutdown_deadline_seconds` budget has elapsed from
        // the moment `graceful_goaway` armed `drain.started_at`, stop
        // waiting for streams and tear the session down. `drive_frontend_
        // shutdown_io` above already had a chance to flush any pending
        // TLS/GOAWAY records; this branch accepts that some bytes may be
        // lost in exchange for honoring the operator-configured SLA.
        // Listeners that disable the knob (`= 0` → `None`) short-circuit
        // the check inside `graceful_shutdown_deadline_elapsed`.
        if self.frontend.graceful_shutdown_deadline_elapsed() {
            debug!(
                "{} Mux shutting_down: graceful-shutdown deadline elapsed, forcing close",
                log_context!(self)
            );
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
                    "{} Mux shutting_down returning true with active H2 streams: {:?}",
                    log_context!(self),
                    self.frontend
                );
                for (idx, stream) in active_h2_streams {
                    debug!(
                        "{}   shutdown stream[{}]: state={:?}, front_phase={:?}, back_phase={:?}, front_completed={}, back_completed={}",
                        log_context!(self),
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
