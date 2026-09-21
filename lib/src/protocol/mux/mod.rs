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
///   `router.rs:log_module_context!($http_context)` (see there). Custom
///   methods render only their byte length, and authority renders only its
///   byte length.
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
            "{gray}{ctx}{reset}\t{open}MUX{reset}\t{grey}Session{reset}({gray}frontend{reset}={white}{frontend:?}{reset}, {gray}method{reset}={white}{method:?}{reset}, {gray}authority_bytes{reset}={white}{authority_bytes:?}{reset})\t >>>",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            ctx = ctx,
            frontend = http_ctx.session_address,
            method = http_ctx.method,
            authority_bytes = http_ctx.authority.as_ref().map(String::len),
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
mod hpack_state;
pub mod parser;
mod pkawa;
pub mod router;
pub(crate) mod serializer;
mod shared;
pub mod stream;

use crate::metrics::names;
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
    timer::TimeoutContainer,
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

/// Debug tripwire for the one-active-stream invariant that
/// `ConnectionH1::end_stream`'s early-return guard rests on.
///
/// `Mux::timeout` retires streams through `Connection::end_stream`, and its
/// proof that no linked stream survives (LIFECYCLE invariant 22) depends on
/// that call actually unlinking. `ConnectionH2::end_stream` always does.
/// `ConnectionH1::end_stream` does NOT: it guards `self.stream != Some(stream)`
/// and returns early, skipping the `Context::unlink_stream`. The guard is safe
/// only because an H1 connection carries at most one linked stream and it
/// equals `self.stream` — `self.stream = Some(..)` occurs only in
/// `ConnectionH1::start_stream`, paired with `Context::link_stream`, and
/// `self.stream = None` only inside `end_stream` itself, after the unlink.
///
/// This is the tripwire for that invariant: a drift between
/// `context.backend_streams` (or a stream's `StreamState::Linked`) and
/// `ConnectionH1::stream` fires here, in every debug / test / e2e build,
/// instead of silently leaving a stream in the reverse index with no backend
/// timer.
///
/// To SEE THIS FIRE: in
/// `a_backend_timeout_leaves_no_linked_stream_behind_on_the_close_path`, set
/// `h1.stream = Some(1)` instead of `Some(0)` while still linking stream 0 to
/// the backend.
///
/// Deliberately at the CALL SITE rather than inside `ConnectionH1::end_stream`,
/// and the reason is scope, not risk. That function has around a dozen callers
/// reaching it through `Connection::end_stream` and the two `Endpoint`
/// adaptors, and the H2-driven ones are precisely the ones this proof never
/// traced: `ConnectionH2::close` and `ConnectionH2::end_stream` iterate their
/// OWN wire map (`for global_stream_id in self.streams.values()`, `h2.rs`) and
/// hand each `StreamState::Linked` gid to the peer, which on an H2-frontend /
/// H1-backend session is a `ConnectionH1`. Add the reset and HUP paths and the
/// set is wider than the claim.
///
/// Asserting inside the function would therefore assert something broader than
/// what was traced. Note what does NOT justify the placement: running the full
/// e2e suite with a `debug_assert_eq!` in that mismatch arm passes with zero
/// hits, and green e2e shows absence of COVERAGE, not absence of the path. The
/// claim is about the two `Mux::timeout` loops, so the assertion is too.
#[cfg(debug_assertions)]
fn debug_assert_h1_owns_stream<Front: SocketHandler>(
    connection: &Connection<Front>,
    stream_id: GlobalStreamId,
) {
    if let Connection::H1(h1) = connection {
        debug_assert_eq!(
            h1.stream,
            Some(stream_id),
            "ConnectionH1::end_stream would skip its unlink: the connection's \
             active stream is {:?}, not the {stream_id} Mux::timeout is ending \
             — the backend index and ConnectionH1::stream have drifted",
            h1.stream,
        );
    }
}

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
            Position::Client(..) => count!(names::backend::BACK_BYTES_IN, size as i64),
            Position::Server => count!(names::backend::BYTES_IN, size as i64),
        }
    }

    /// Increment the global `count!()` counter for bytes written on this side.
    pub fn count_bytes_out_counter(&self, size: usize) {
        match self {
            Position::Client(..) => count!(names::backend::BACK_BYTES_OUT, size as i64),
            Position::Server => count!(names::backend::BYTES_OUT, size as i64),
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
    /// Clock snapshot for the pass currently executing.
    ///
    /// [`Mux`] is the only clock sampler in the mux: it refreshes this field
    /// once per outer [`Mux::ready`] pass and at the top of [`Mux::timeout`]
    /// and [`Mux::shutting_down`]. Every time-based decision in the H2 core
    /// reads this snapshot — mirrored into
    /// [`h2::ConnectionH2::now`] at each public entry point — instead of
    /// calling [`Instant::now`] itself, so one pass sees one consistent
    /// "now". Every deadline armed or evaluated inside a pass is therefore
    /// accurate to within that pass — **in either direction**. The error is
    /// not one-sided: an arm site runs at an arbitrary depth into its pass
    /// (a DATA or HEADERS refresh, an outbound-byte refresh) and stamps the
    /// snapshot taken at the START of that pass, so the stored instant is
    /// older than the event it records; an eval site runs near the top of a
    /// pass (`cancel_timed_out_streams` is the first thing
    /// [`h2::ConnectionH2::readable`] does). The measured age is therefore
    /// inflated by the arm site's depth, and a deadline can fire up to one
    /// pass EARLY as well as one pass late. At base both ends read the real
    /// clock and the comparison was exact; this is the cost of the snapshot.
    ///
    /// The flood and back-pressure windows are the one asymmetric case, and
    /// they fail closed: `now` is constant for the whole pass, so a window
    /// cannot decay part-way through one. A burst arriving during a pass is
    /// weighed in full against the window that was open when the pass
    /// started, where before the change a long pass could decay the counters
    /// under the burst. The window BOUNDARY still carries the same
    /// one-pass error in either direction as every other deadline.
    ///
    /// Note that [`crate::SessionMetrics`] and [`crate::timer::TimeoutContainer`]
    /// deliberately stay on the real clock: metrics want true elapsed time,
    /// and the timeout containers are the embedder's timer, held by the [`Mux`]
    /// adapter in [`Mux::timeouts`] and never touched by a core — which
    /// publishes a deadline stamped from this snapshot instead.
    pub now: Instant,
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
            now: Instant::now(),
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
            stream.metrics.mark_request_start();
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
    /// The adapter's wheel handles, one per live token (the frontend plus
    /// every backend in `router.backends`).
    ///
    /// The H1/H2 cores no longer own a `TimeoutContainer`: they publish the
    /// instant they want to be called back at through `Connection::poll_timeout`
    /// and [`Mux::reschedule`] reflects that onto `crate::timer`, arming only
    /// when what the wheel holds differs from what the core wants. This map is
    /// the ONLY thing in the mux that talks to the timer wheel.
    ///
    /// A handle for a token that has left `router.backends` is dropped by
    /// `reschedule`'s `retain`, and `TimeoutContainer::drop` cancels its entry
    /// — which is why no backend-removal site has to remember to cancel.
    pub timeouts: HashMap<Token, TimeoutContainer>,
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
    fn sync_upgrade_buffers(&mut self) {
        for stream in &mut self.context.streams {
            stream
                .front
                .storage
                .buffer
                .sync(stream.front.storage.end, stream.front.storage.head);
            stream
                .back
                .storage
                .buffer
                .sync(stream.back.storage.end, stream.back.storage.head);
        }
    }

    /// Consume the wheel entry that delivered a timeout and say whether it was
    /// really due.
    ///
    /// Three things happen here, in this order, and all three are load-bearing.
    ///
    /// 1. **Consume.** The wheel has already handed the entry over, so
    ///    [`TimeoutContainer::triggered`] records that the handle holds
    ///    nothing. This is the consume-then-reschedule step, the direct
    ///    counterpart of `UdpManager::handle_timeout` clearing `armed_deadline`
    ///    on entry (`lib/src/protocol/udp/manager.rs`). It matters because
    ///    [`Mux::sync_timeout`] memoizes on "the handle already holds this
    ///    deadline": without the clearing, a delivery that changes nothing is
    ///    memoized away and the session is left with NO wheel entry at all — a
    ///    lost wakeup.
    ///
    /// 2. **Re-validate.** `crate::timer` rounds a delay to the NEAREST tick
    ///    (`timer.rs` `duration_to_tick`) and `Timer::poll` fires everything
    ///    whose tick has come, so an entry armed for deadline `D` is delivered
    ///    from `tick * round(D / tick) - tick/2` onwards. The earliness is
    ///    `(D_ms + 50) mod 100` at the default 100 ms tick, i.e. up to **99 ms**
    ///    — a full tick minus a millisecond, not half a tick; see
    ///    `timer.rs::duration_to_tick` for the two halves of that bound. An
    ///    early delivery is not an expiry: returning
    ///    `false` here leaves the core's deadline untouched, and the
    ///    `reschedule` on the way out of [`SessionState::timeout`] therefore
    ///    puts the entry straight back at the SAME instant.
    ///
    /// 3. **Clear the core's deadline on a real expiry.** The core asked to be
    ///    called back at `D` and has been; leaving `D` in place would have
    ///    `reschedule` re-arm an already-elapsed instant, which the wheel
    ///    re-delivers immediately — a busy loop. The timeout branches re-arm
    ///    with `arm_timeout` on every path that keeps the session alive, which
    ///    is what the old `triggered()` + `set(token)` pair did.
    ///
    /// A token with no core (an unknown token, or a backend already evicted)
    /// is reported due and handled by the caller's unknown-token branch.
    fn consume_timer_entry(&mut self, token: Token) -> bool {
        if let Some(container) = self.timeouts.get_mut(&token) {
            container.triggered();
        }
        let core_deadline = if token == self.frontend_token {
            self.frontend.poll_timeout()
        } else {
            match self.router.backends.get(&token) {
                Some(backend) => backend.poll_timeout(),
                None => return true,
            }
        };
        if core_deadline.is_some_and(|deadline| self.context.now < deadline) {
            return false;
        }
        if token == self.frontend_token {
            self.frontend.clear_timeout();
        } else if let Some(backend) = self.router.backends.get_mut(&token) {
            backend.clear_timeout();
        }
        true
    }

    /// Reflect every core's next deadline onto the timer wheel.
    ///
    /// This is the adapter half of the `poll_timeout()` split, and the direct
    /// analogue of `UdpManager::reschedule` (`lib/src/protocol/udp/manager.rs`).
    /// It runs at the end of every `SessionState` entry point that can change a
    /// deadline — `ready`, `timeout`, `shutting_down`, `cancel_timeouts` —
    /// through wrappers, so no early `return` inside those bodies can skip it.
    /// That structural placement is the point: a hand-placed re-arm at each
    /// exit is exactly what used to be forgotten.
    ///
    /// Departed tokens need no explicit cancel: `retain` drops their handle and
    /// `TimeoutContainer::drop` cancels the entry.
    fn reschedule(&mut self) {
        let frontend_token = self.frontend_token;
        Self::sync_timeout(
            &mut self.timeouts,
            frontend_token,
            self.frontend.poll_timeout(),
            self.frontend.timeout_duration(),
        );
        for (token, backend) in &self.router.backends {
            Self::sync_timeout(
                &mut self.timeouts,
                *token,
                backend.poll_timeout(),
                backend.timeout_duration(),
            );
        }
        let backends = &self.router.backends;
        self.timeouts
            .retain(|token, _| *token == frontend_token || backends.contains_key(token));

        self.debug_assert_timer_coherence();
    }

    /// Arm, re-arm or cancel one token's wheel entry so it matches `next`.
    ///
    /// The memoization — skip the wheel when nothing changed — is what makes
    /// this cheap enough to run on every pass, and it is also where the lost
    /// wakeup lives if it is written carelessly. It is guarded on BOTH halves:
    ///
    /// * `deadline() == next` — the instant has not moved; and
    /// * `is_armed() == next.is_some()` — the wheel actually still HOLDS it.
    ///
    /// The second is not redundant. A delivered entry is gone from the wheel,
    /// and `TimeoutContainer::triggered` clears the deadline alongside it, so
    /// the pair stays honest — that clearing is this design's
    /// consume-then-reschedule step, the same one `UdpManager::handle_timeout`
    /// performs by setting `armed_deadline = None` on entry. Memoize on the
    /// deadline alone, against a mirror that a firing does not clear, and every
    /// wheel delivery that changes nothing becomes a session with no timer.
    fn sync_timeout(
        timeouts: &mut HashMap<Token, TimeoutContainer>,
        token: Token,
        next: Option<Instant>,
        duration: Duration,
    ) {
        let container = timeouts
            .entry(token)
            .or_insert_with(|| TimeoutContainer::new_empty(duration));
        if container.duration() != duration {
            // Keep the handle's configured duration current without moving the
            // armed entry: the WebSocket upgrade hands these containers to
            // `Pipe`, which re-arms from `duration()`.
            container.retune(duration);
        }
        if container.deadline() == next && container.is_armed() == next.is_some() {
            return;
        }
        match next {
            Some(deadline) => container.set_at(token, deadline),
            None => {
                container.cancel();
            }
        }
    }

    /// Timer coherence, carried over from `UdpManager::check_invariants` (6):
    /// for every live token the wheel handle must hold exactly what the core
    /// asked for — armed iff the core wants a timer, at the core's instant when
    /// it does — and no handle may outlive its connection.
    ///
    /// This is the invariant a missed `reschedule` breaks, and it is the reason
    /// the entry points are wrapped rather than trusted.
    fn debug_assert_timer_coherence(&self) {
        #[cfg(debug_assertions)]
        {
            let frontend_token = self.frontend_token;
            for (token, container) in &self.timeouts {
                let core = if *token == frontend_token {
                    self.frontend.poll_timeout()
                } else {
                    match self.router.backends.get(token) {
                        Some(backend) => backend.poll_timeout(),
                        None => {
                            panic!("timer handle for {token:?} outlived its connection")
                        }
                    }
                };
                debug_assert_eq!(
                    container.is_armed(),
                    core.is_some(),
                    "timer coherence for {token:?}: armed={} but the core wants {core:?}",
                    container.is_armed(),
                );
                debug_assert_eq!(
                    container.deadline(),
                    core,
                    "timer coherence for {token:?}: the wheel holds {:?}, the core wants {core:?}",
                    container.deadline(),
                );
            }
        }
    }

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
    /// Thin wrapper over [`Mux::ready_inner`] whose only job is to run
    /// [`Mux::reschedule`] on the way out. `ready_inner` has a dozen `return`
    /// sites; arming the wheel at each of them by hand is precisely the
    /// discipline this refactor exists to remove.
    fn ready(
        &mut self,
        session: Rc<RefCell<dyn ProxySession>>,
        proxy: Rc<RefCell<dyn L7Proxy>>,
        metrics: &mut SessionMetrics,
    ) -> SessionResult {
        let result = self.ready_inner(session, proxy, metrics);
        self.reschedule();
        result
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

    /// Thin wrapper over [`Mux::timeout_inner`]: reschedule on the way out,
    /// then check the two properties carried over from `UdpManager`.
    fn timeout(&mut self, token: Token, metrics: &mut SessionMetrics) -> StateResult {
        let result = self.timeout_inner(token, metrics);
        self.reschedule();

        // Strict-advance guard, carried over from `UdpManager::handle_timeout`
        // (`lib/src/protocol/udp/manager.rs`). After handling a firing at
        // `now`, the entry re-armed for the SAME token must be strictly in the
        // future. A deadline `<= now` makes the wheel re-deliver immediately
        // and the session spins — the canonical sans-io busy loop. The one
        // legitimate way to reach it is a configured timeout of zero, which is
        // an operator asking for exactly that.
        #[cfg(debug_assertions)]
        {
            let now = self.context.now;
            let duration = if token == self.frontend_token {
                Some(self.frontend.timeout_duration())
            } else {
                self.router
                    .backends
                    .get(&token)
                    .map(|b| b.timeout_duration())
            };
            if let Some(container) = self.timeouts.get(&token)
                && let Some(next) = container.deadline()
                && duration.is_some_and(|d| !d.is_zero())
            {
                debug_assert!(
                    next > now,
                    "Mux::timeout must strictly advance past a firing on {token:?}: \
                     armed {next:?} <= fired_at {now:?} (busy-loop)"
                );
            }
        }

        result
    }

    fn cancel_timeouts(&mut self) {
        trace!("{} MuxState::cancel_timeouts", log_context!(self));
        self.frontend.clear_timeout();
        for backend in self.router.backends.values_mut() {
            backend.clear_timeout();
        }
        // Push the cleared deadlines onto the wheel in the same call: the
        // adapter's handles are the only thing holding real entries.
        self.reschedule();
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
            incr!(names::h2::CLOSE_WITH_ACTIVE_STREAMS);
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

        // Release every wheel handle: the session is going away and
        // `TimeoutContainer::drop` cancels each entry. This replaces the
        // per-backend `timeout_container().cancel()` the loop below used to do
        // and additionally covers the frontend, which it never did.
        self.timeouts.clear();

        for (token, client) in &mut self.router.backends {
            let proxy_borrow = proxy.borrow();
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
            if let Err(e) = socket.shutdown(Shutdown::Write)
                && e.kind() != ErrorKind::NotConnected
            {
                error!(
                    "{} error shutting down back socket({:?}): {:?}",
                    log_context_lite!(self),
                    socket,
                    e
                );
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
                    gauge_add!(names::backend::CONNECTIONS, -1);
                    // Second `-1` site for `backend.pool.size` (the first is
                    // in `connection.rs::pre_close_client_bookkeeping`). This
                    // path runs during session teardown when the frontend
                    // session iterates the backends map directly without
                    // routing through `Connection::close`. Both `-1` sites
                    // mirror the single `+1` in router.rs::connect and the
                    // matching `backend.connections, -1` calls already
                    // present here, so symmetry follows from
                    // `backend.connections` correctness.
                    gauge_add!(names::backend::POOL_SIZE, -1);
                    gauge_add!(
                        names::backend::CONNECTIONS_PER_BACKEND,
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

    /// Thin wrapper over [`Mux::shutting_down_inner`]: it drives frontend I/O
    /// and can change a core deadline, so it owes the wheel a reschedule on
    /// every exit.
    fn shutting_down(&mut self) -> SessionIsToBeClosed {
        let result = self.shutting_down_inner();
        self.reschedule();
        result
    }
}

/// The bodies the `SessionState` wrappers above drive. They are inherent
/// methods rather than trait methods so the wrappers can own the one thing
/// every exit path needs: the [`Mux::reschedule`] that pushes the cores'
/// deadlines onto the timer wheel.
impl<Front: SocketHandler + std::fmt::Debug, L: ListenerHandler + L7ListenerHandler> Mux<Front, L> {
    fn ready_inner(
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
            // The mux's clock sample for this pass. Everything time-based
            // below — flood and back-pressure windows, the SETTINGS-ACK
            // deadline, per-stream liveness and flow-control-stall deadlines
            // — reads `context.now` (mirrored into `ConnectionH2::now` at
            // each entry point) rather than calling `Instant::now()` itself.
            //
            // Sampled per OUTER iteration so the inner loop DELIBERATELY
            // shares one instant: that is the property the snapshot exists
            // for — every frame processed in one inner sweep is weighed
            // against the same "now", so a burst cannot decay a rate window
            // out from under itself mid-sweep. It is not a bound on how long
            // the sweep may take: `MAX_LOOP_ITERATIONS` is a count, and a
            // count bounds iterations, not wall clock. `counter` is declared
            // above BOTH loops, so that budget is shared across every outer
            // iteration of one `ready()` call rather than reset per pass.
            // `start` above stays a separate sample so the trace reports the
            // true entry instant.
            self.context.now = Instant::now();
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
                        MuxResult::Upgrade => {
                            self.sync_upgrade_buffers();
                            return SessionResult::Upgrade;
                        }
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
                                        names::backend::UP,
                                        Some(cluster_id),
                                        Some(&backend_borrow.backend_id)
                                    );
                                    gauge!(
                                        names::backend::AVAILABLE,
                                        1,
                                        Some(cluster_id),
                                        Some(&backend_borrow.backend_id)
                                    );
                                    push_event(Event {
                                        kind: EventKind::BackendUp as i32,
                                        backend_id: Some(backend_borrow.backend_id.to_owned()),
                                        address: Some(backend_borrow.address.into()),
                                        cluster_id: Some(cluster_id.to_owned()),
                                        metric_detail: None,
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
                                client.set_timeout_duration(
                                    self.router.configured_backend_timeout,
                                    self.context.now,
                                );
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
                                    names::backend::CONNECTIONS_ERROR,
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
                                        names::backend::DOWN,
                                        Some(cluster_id),
                                        Some(&backend_borrow.backend_id)
                                    );
                                    gauge!(
                                        names::backend::AVAILABLE,
                                        0,
                                        Some(cluster_id),
                                        Some(&backend_borrow.backend_id)
                                    );
                                    push_event(Event {
                                        kind: EventKind::BackendDown as i32,
                                        backend_id: Some(backend_borrow.backend_id.to_owned()),
                                        address: Some(backend_borrow.address.into()),
                                        cluster_id: Some(cluster_id.to_owned()),
                                        metric_detail: None,
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
                            // No explicit timer cancel: the token has left
                            // `router.backends`, so `Mux::reschedule` drops its
                            // handle on the way out of `ready` and
                            // `TimeoutContainer::drop` cancels the entry.
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
                            if let Err(e) = socket.shutdown(Shutdown::Write)
                                && e.kind() != ErrorKind::NotConnected
                            {
                                error!(
                                    "{} error shutting down back socket({:?}): {:?}",
                                    log_context!(self),
                                    socket,
                                    e
                                );
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
                        MuxResult::Upgrade => {
                            self.sync_upgrade_buffers();
                            return SessionResult::Upgrade;
                        }
                    }
                    // Cross-readiness: frontend wrote → wake parked backends.
                    // If any backend resumes, invalidate the stale readiness
                    // flag so the inner loop continues instead of breaking.
                    let context = &mut self.context;
                    for backend in self.router.backends.values_mut() {
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
                    incr!(names::http::INFINITE_LOOP_ERROR);
                    if self.frontend.has_pending_write() {
                        debug!(
                            "{} Mux loop budget exhausted while frontend flush pending: {:?}",
                            log_context!(self),
                            self.frontend
                        );
                        self.frontend.readiness_mut().event.remove(Ready::WRITABLE);
                        self.frontend.arm_timeout(self.context.now);
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
                    .set_timeout_duration(self.configured_frontend_timeout, context.now);
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

    fn timeout_inner(&mut self, token: Token, _metrics: &mut SessionMetrics) -> StateResult {
        trace!("{} MuxState::timeout({:?})", log_context!(self), token);
        // `timeout` runs outside `ready()`, so it is its own sampling point.
        // `cancel_timed_out_streams` below reaps on this snapshot.
        self.context.now = Instant::now();
        // Consume the wheel entry and re-validate it BEFORE the per-token
        // branches, so both of them share one gate. See
        // `Mux::consume_timer_entry`.
        if !self.consume_timer_entry(token) {
            trace!(
                "{} MuxState::timeout: early wheel delivery on {:?}, re-armed",
                log_context!(self),
                token
            );
            return StateResult::Continue;
        }
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
            // The per-stream reaper (bidirectional-idle + outbound
            // flow-control-stall guards) normally runs only from `readable()`,
            // but a fully-silent peer never triggers a read event. Run it on the
            // connection-timeout path too so a window-stalled stream — a buffered
            // response the peer refuses to drain by holding its receive window
            // shut — is reaped and its MAX_CONCURRENT_STREAMS slot freed, instead
            // of lingering until the 30-minute zombie checker. The reaper queues
            // an `RST_STREAM(CANCEL)`; because `has_pending_write()` does NOT
            // observe `pending_rst_streams` (it gates connection close, so a
            // queued RST must not read as "keep open"), set `should_write` via
            // the dedicated `has_pending_control_write()` probe so the reset is
            // actually flushed to the peer before the connection closes — without
            // it, a fully-silent peer's stalled stream is freed but the peer sees
            // only EOF, never the RST(CANCEL).
            if let Connection::H2(h2) = &mut self.frontend {
                h2.cancel_timed_out_streams(
                    &mut self.context,
                    &mut EndpointClient(&mut self.router),
                );
                if h2.has_pending_control_write() {
                    should_write = true;
                }
            }
            if self.frontend.has_pending_write() {
                should_write = true;
            }
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
                    #[cfg(debug_assertions)]
                    debug_assert_h1_owns_stream(backend, stream_id);
                    backend.end_stream(stream_id, &mut self.context);
                }
            }
        } else if let Some(backend) = self.router.backends.get_mut(&token) {
            // Captured before the backend borrow so the re-arm below can reach
            // the pass's clock snapshot.
            let now = self.context.now;
            trace!(
                "{} MuxState::timeout_backend({:#?})",
                log_context_lite!(self),
                backend
            );
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
                #[cfg(debug_assertions)]
                debug_assert_h1_owns_stream(backend, stream_id);
                backend.end_stream(stream_id, &mut self.context);
            }
            // Re-arm the backend timeout if the session stays alive (draining streams).
            // Without this, the timeout is consumed and the session becomes immortal
            // until the zombie checker runs.
            //
            // The `!should_close` condition looks like a hole, and is not. On
            // `should_close`, control falls through to the shared tail below,
            // which can still return `Continue` — through the `should_write`
            // writable loop, or through `delay_close_for_frontend_flush` —
            // having re-armed only the FRONTEND container. That would be a
            // lost wakeup if this backend could still be carrying work with no
            // timer of its own. It cannot, and the chain is four links, all in
            // this file and `connection.rs`/`h1.rs`/`h2.rs`:
            //
            // 1. the loop above iterates `context.backend_streams[&token]` —
            //    every stream currently linked to this backend, and nothing
            //    else — and calls `Connection::end_stream` on EVERY iteration,
            //    outside the if/else chain, so no arm can skip it;
            // 2. `ConnectionH2::end_stream` (`h2.rs`) starts with
            //    `context.unlink_stream(...)` unconditionally.
            //    `ConnectionH1::end_stream` (`h1.rs`) does NOT: it first guards
            //    `if self.stream != Some(stream) { error!(..); return; }` and
            //    that early return skips the unlink. The two are dispatched by
            //    `Connection::end_stream` in `connection.rs`;
            // 3. the H1 guard cannot reject a stream this loop passes it,
            //    because an H1 connection carries at most one linked stream and
            //    it is exactly `self.stream`. That holds because
            //    `self.stream = Some(..)` occurs only in
            //    `ConnectionH1::start_stream`, paired with
            //    `Context::link_stream`, and `self.stream = None` occurs only
            //    inside `end_stream` itself, after the unlink has run. So while
            //    `backend_streams[&token]` names a stream, `self.stream` names
            //    the same one — and `debug_assert_h1_owns_stream`, called
            //    just above every `end_stream` in this function, is the
            //    tripwire that fires if it ever stops being true;
            // 4. so by the time the tail runs, this backend has no linked
            //    stream left. The index-consistency `assert_eq!` at the end of
            //    `Mux::ready` pins the other direction (`backend_streams` is
            //    exactly the set of `StreamState::Linked` streams).
            //
            // `Context::unlink_stream` is the eviction point this path relies
            // on, not the only one in the module: `remove_backend_stream` also
            // has direct callers in `h1.rs` and `h2.rs`. Extra eviction can
            // only strengthen the conclusion, but the premise is "this loop
            // evicts", not "nothing else can".
            //
            // Work attached to this backend AFTERWARDS does not depend on
            // this site either, and since the adapter landed it no longer even
            // depends on the connection doing I/O: a pooled backend stays in
            // `router.backends`, so `Mux::reschedule` keeps its handle armed at
            // whatever deadline the core holds, every pass. (`arm_timeout()` at
            // the top of `ConnectionH1::{readable,writable}` and in
            // `ConnectionH2::{write_streams,handle_headers_frame}` pushes that
            // deadline out on the first pass, but nothing hangs on it.) That is
            // what covers the pool-reuse branch of `Router::connect` never
            // arming a timeout, unlike the fresh-dial branch.
            //
            // The assertion below is the cheap tripwire for link 3: if the
            // impossible becomes possible (a new arm that skips `end_stream`,
            // or an `end_stream` that stops unlinking), it fires here rather
            // than as an immortal backend in production.
            #[cfg(debug_assertions)]
            debug_assert!(
                self.context
                    .backend_streams
                    .get(&token)
                    .is_none_or(|ids| ids.is_empty()),
                "backend {token:?} still holds linked streams {:?} after its \
                 timeout reaped them: on the should_close path nothing re-arms \
                 its container, so those streams would have no backend timer",
                self.context.backend_streams.get(&token),
            );
            if !should_close {
                backend.arm_timeout(now);
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
                self.frontend.arm_timeout(self.context.now);
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
                self.frontend.arm_timeout(self.context.now);
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
            self.frontend.arm_timeout(self.context.now);
            StateResult::Continue
        }
    }

    fn shutting_down_inner(&mut self) -> SessionIsToBeClosed {
        // `shut_down_sessions()` drives this outside `ready()`, so it is its
        // own sampling point, and it is load-bearing rather than belt-and-braces.
        // The chain is: this line refreshes `context.now`;
        // `drive_frontend_shutdown_io` below always reaches `readable()` for an
        // H2 frontend (`force_h2_read` is unconditionally true, so the
        // early return above it cannot fire), and `readable` mirrors
        // `context.now` into `ConnectionH2::now`; the forced-close check that
        // follows then reads a fresh snapshot. Drop this line and a silent
        // draining session propagates the snapshot of its last `ready()` pass
        // forever, so the graceful-shutdown budget never expires. Pinned by
        // `shutting_down_refreshes_the_snapshot_so_the_drain_budget_expires`.
        let now = Instant::now();
        self.context.now = now;
        // RFC 9113 §6.8: initiate graceful shutdown with double-GOAWAY pattern.
        // Only send the initial GOAWAY once. The final GOAWAY (with the real
        // last_stream_id) is handled by finalize_write() when all streams drain.
        // Calling graceful_goaway() again would send the final GOAWAY
        // prematurely and force-disconnect before in-flight streams complete.
        if !self.frontend.is_draining() {
            match self.frontend.graceful_goaway(now) {
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

/// Minimal listener + `Mux` scaffolding shared by the `mod.rs` and `h2.rs`
/// test modules. Lives here rather than in either test module because
/// `Context<L>` is generic over the listener and both need the same `L`.
#[cfg(test)]
pub(crate) mod test_support {
    use std::{
        cell::RefCell,
        collections::BTreeMap,
        net::{SocketAddr, TcpListener as StdTcpListener},
        rc::Rc,
    };

    use sozu_command::logging::CachedTags;

    use super::*;
    use crate::{
        FrontendFromRequestError, L7ListenerHandler, ListenerHandler, Protocol,
        protocol::http::{answers::HttpAnswers, parser::Method},
        router::RouteResult,
    };

    /// Implements exactly the nine required methods of `ListenerHandler` +
    /// `L7ListenerHandler`; every other knob keeps its trait default.
    pub(crate) struct TestListener {
        address: SocketAddr,
        answers: Rc<RefCell<HttpAnswers>>,
    }

    impl TestListener {
        pub(crate) fn new() -> Self {
            Self {
                address: "127.0.0.1:1".parse().expect("test address must parse"),
                answers: Rc::new(RefCell::new(
                    HttpAnswers::new(&BTreeMap::new()).expect("default answers must build"),
                )),
            }
        }
    }

    impl ListenerHandler for TestListener {
        fn get_addr(&self) -> &SocketAddr {
            &self.address
        }
        fn get_tags(&self, _key: &str) -> Option<&CachedTags> {
            None
        }
        fn set_tags(&mut self, _key: String, _tags: Option<BTreeMap<String, String>>) {}
        fn protocol(&self) -> Protocol {
            Protocol::HTTP
        }
        fn public_address(&self) -> SocketAddr {
            self.address
        }
    }

    impl L7ListenerHandler for TestListener {
        fn get_sticky_name(&self) -> &str {
            "SOZUBALANCEID"
        }
        fn get_connect_timeout(&self) -> u32 {
            10
        }
        fn frontend_from_request(
            &self,
            _host: &str,
            _uri: &str,
            _method: &Method,
        ) -> Result<RouteResult, FrontendFromRequestError> {
            Err(FrontendFromRequestError::InvalidCharsAfterHost(
                "test listener routes nothing".to_owned(),
            ))
        }
        fn get_answers(&self) -> &Rc<RefCell<HttpAnswers>> {
            &self.answers
        }
    }

    /// A live, connected, non-blocking loopback socket. Reads return
    /// `WouldBlock` rather than `ECONNREFUSED`, so a `readable()` pass over it
    /// is a no-op instead of a forced disconnect. The accepted peer is returned
    /// so the caller can keep it alive for the duration of the test.
    pub(crate) fn connected_socket() -> (mio::net::TcpStream, std::net::TcpStream) {
        let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let address = listener.local_addr().expect("listener local addr");
        let client = mio::net::TcpStream::connect(address).expect("connect test client");
        let (peer, _) = listener.accept().expect("accept test peer");
        peer.set_nonblocking(true).expect("peer nonblocking");
        (client, peer)
    }

    /// A `Context` backed by [`TestListener`] and a small buffer pool.
    pub(crate) fn test_context(pool: &Rc<RefCell<Pool>>) -> Context<TestListener> {
        Context::new(
            Ulid::generate(),
            Rc::downgrade(pool),
            Rc::new(RefCell::new(TestListener::new())),
            None,
            "127.0.0.1:1".parse().expect("public address must parse"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{connected_socket, test_context};
    use super::*;
    use crate::{
        pool::Pool,
        protocol::mux::h2::{ConnectionH2, H2ConnectionConfig, H2FloodConfig, H2State},
        timer::TimeoutContainer,
    };

    /// `Mux::shutting_down` runs outside `ready()`, driven by
    /// `shut_down_sessions()`. It is therefore its own clock-sampling point,
    /// and that sample is load-bearing: it refreshes `context.now`, which
    /// `drive_frontend_shutdown_io` -> `readable()` mirrors into
    /// `ConnectionH2::now`, which the forced-close check then reads.
    ///
    /// A silent draining session gets no read/write events, so nothing else
    /// refreshes the snapshot. Without this sample the connection would carry
    /// the snapshot of its last `ready()` pass forever and the
    /// graceful-shutdown budget would never expire — the session would hang
    /// past the operator-configured SLA, which is the exact failure the
    /// forced-close deadline exists to prevent.
    ///
    /// The frontend is already draining, so `graceful_goaway` is NOT called on
    /// this path — that is what makes the test discriminating, because the
    /// `now` parameter of `graceful_goaway` cannot cover it.
    ///
    /// To SEE THIS RED: delete `self.context.now = now;` from
    /// `Mux::shutting_down` (keeping `let now` so `graceful_goaway(now)` still
    /// compiles). `readable()` then mirrors the frozen snapshot, the budget
    /// reads as 0s elapsed against a 5s deadline, and the final assertion fails
    /// with `assertion failed: mux.shutting_down()`.
    #[test]
    fn shutting_down_refreshes_the_snapshot_so_the_drain_budget_expires() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 4, 16384)));
        let (socket, _peer) = connected_socket();
        let deadline = Duration::from_secs(5);

        let mut h2 = ConnectionH2::new(
            Ulid::generate(),
            socket,
            Position::Server,
            Rc::downgrade(&pool),
            H2FloodConfig::default(),
            H2ConnectionConfig::default(),
            Duration::from_secs(30),
            Some(deadline),
            Duration::from_secs(30),
            None,
            Ready::READABLE | Ready::HUP | Ready::ERROR,
        )
        .expect("a pool with free buffers must yield an H2 connection");

        // A connection that already sent its initial GOAWAY and armed the
        // budget well over the deadline ago. `shutting_down` takes the
        // already-draining branch, which never calls `graceful_goaway`.
        let armed_at = Instant::now() - Duration::from_secs(60);
        h2.state = H2State::Header;
        h2.drain.draining = true;
        h2.drain.started_at = Some(armed_at);
        let mut frontend = Connection::H2(h2);

        let mut context = test_context(&pool);
        // Freeze the snapshot at the instant the budget was armed, as a
        // session that has seen no event since then would carry it.
        context.now = armed_at;
        // One live stream, so `can_stop` stays false and the forced-close
        // deadline is the only path that can return `true`.
        let stream_id = context
            .create_stream(Ulid::generate(), 1 << 16)
            .expect("test context must create a stream");
        context.streams[stream_id].state = StreamState::Linked(Token(1));
        // Give the connection a live WIRE stream too: with an empty
        // `ConnectionH2::streams`, `finalize_write` sends the FINAL GOAWAY and
        // force-disconnects, so `drive_frontend_shutdown_io` would return true
        // and short-circuit `shutting_down` before the deadline check. The
        // per-stream activity maps are deliberately left empty so
        // `cancel_timed_out_streams` early-returns and cannot queue an RST.
        let Connection::H2(h2) = &mut frontend else {
            unreachable!("frontend was built as H2")
        };
        h2.streams.insert(1, stream_id);

        let mut mux = Mux {
            configured_frontend_timeout: Duration::from_secs(30),
            frontend_token: Token(0),
            frontend,
            router: Router::new(Duration::from_secs(30), Duration::from_secs(30)),
            context,
            session_ulid: Ulid::generate(),
            timeouts: HashMap::new(),
        };

        assert!(
            mux.frontend.is_draining(),
            "precondition: the frontend must already be draining, so this test \
             exercises the branch that never reaches graceful_goaway"
        );

        assert!(
            mux.shutting_down(),
            "shutting_down must refresh the clock snapshot itself, so a silent \
             draining session's forced-close budget still expires"
        );
    }

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

    /// Build a minimal H1 frontend `Mux` with one `Idle` stream. The returned
    /// peer socket must be kept alive for the duration of the test, otherwise
    /// the loopback connection is torn down and `readable()` sees a forced
    /// disconnect.
    ///
    /// An H1 frontend is deliberate: the `StreamState::Idle` arm of
    /// `Mux::timeout` writes a 408 and stamps
    /// `stream.context.access_log_message = Some("client_timeout")`, which is
    /// the crispest available witness that the timeout BODY ran. The H2 `Idle`
    /// arm is silently ignored and would witness nothing.
    fn h1_mux_with_idle_stream(
        pool: &Rc<RefCell<Pool>>,
        frontend_timeout: Duration,
    ) -> (
        Mux<mio::net::TcpStream, test_support::TestListener>,
        std::net::TcpStream,
    ) {
        let (socket, peer) = connected_socket();
        let frontend = Connection::new_h1_server(Ulid::generate(), socket, frontend_timeout);
        let mut context = test_context(pool);
        context
            .create_stream(Ulid::generate(), 1 << 16)
            .expect("test context must create a stream");
        let mux = Mux {
            configured_frontend_timeout: frontend_timeout,
            frontend_token: Token(0),
            frontend,
            router: Router::new(Duration::from_secs(30), Duration::from_secs(30)),
            context,
            session_ulid: Ulid::generate(),
            timeouts: HashMap::new(),
        };
        (mux, peer)
    }

    /// Park the frontend core's next deadline at an exact instant, then push it
    /// onto the wheel the way a real pass would.
    ///
    /// Writes `ConnectionH1::timeout_deadline` directly rather than going
    /// through `arm_timeout`, so a test can stage a deadline in the past ("the
    /// wheel is late") as easily as one in the future, without disturbing the
    /// configured duration the re-arm will use.
    fn arm_frontend_at(
        mux: &mut Mux<mio::net::TcpStream, test_support::TestListener>,
        deadline: Option<Instant>,
    ) {
        let Connection::H1(h1) = &mut mux.frontend else {
            unreachable!("the helper builds an H1 frontend")
        };
        h1.timeout_deadline = deadline;
        mux.reschedule();
    }

    /// Same, for a backend connection that has not been inserted yet.
    fn park_backend_deadline(connection: &mut Connection<SessionTcpStream>, deadline: Instant) {
        let Connection::H1(h1) = connection else {
            unreachable!("the helper builds an H1 backend")
        };
        h1.timeout_deadline = Some(deadline);
    }

    /// The timer wheel rounds a delay to the NEAREST tick and `Timer::poll`
    /// fires everything whose tick has come, so an entry armed for deadline `D`
    /// is handed over from `tick * round(D / tick) - tick/2` onwards — up to
    /// 99 ms early with the default 100 ms tick. An early delivery is not an
    /// expiry: `Mux::timeout` must leave the session alone.
    ///
    /// To SEE THIS RED: make `Mux::consume_timer_entry` return `true`
    /// unconditionally (drop its `core_deadline.is_some_and(...)` early
    /// return). The body then runs on an early delivery and this fails with
    /// `access_log_message == Some("client_timeout")` — a session closed up to
    /// 99 ms before its configured `front_timeout`.
    #[test]
    fn an_early_wheel_delivery_does_not_run_the_timeout_body() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 4, 16384)));
        let (mut mux, _peer) = h1_mux_with_idle_stream(&pool, Duration::from_secs(60));
        let mut metrics = SessionMetrics::new(None);

        // A minute out, then delivered immediately: early, exaggerated so the
        // test cannot be flaky.
        arm_frontend_at(&mut mux, Some(Instant::now() + Duration::from_secs(60)));

        let result = mux.timeout(Token(0), &mut metrics);

        assert_eq!(
            result,
            StateResult::Continue,
            "an early delivery must leave the session running"
        );
        assert_eq!(
            mux.context.streams[0].context.access_log_message, None,
            "the timeout body must not run for a delivery before the deadline"
        );
    }

    /// **The ordering test.** This is the one that proves why the
    /// `poll_timeout()` split (stored deadline first, memoized reschedule
    /// second) must land in that order and not the other way round.
    ///
    /// `Mux::sync_timeout` memoizes: it touches the wheel only when what the
    /// handle holds differs from what the core wants. On an early delivery the
    /// core's deadline has NOT moved — it still wants `D` — so a naive memo
    /// says "nothing to do" and emits nothing. But the wheel entry is already
    /// gone: the delivery consumed it. The session is then left with no timer
    /// at all and nothing to arm one. That is the LOST WAKEUP, and it is
    /// exactly the bug fixed for the UDP shell in `UdpManager::handle_timeout`,
    /// where `reschedule` emitted `ArmTimer` only when the minimum deadline had
    /// MOVED.
    ///
    /// What prevents it here is consume-then-reschedule:
    /// `Mux::consume_timer_entry` calls `TimeoutContainer::triggered`, which
    /// clears the handle's deadline, so the memo correctly reads "the wheel
    /// holds nothing" and re-arms at the same instant.
    ///
    /// To SEE THIS RED — this is the C2-without-C1 tree:
    /// 1. delete `self.deadline = None;` from `TimeoutContainer::triggered`
    ///    (`timer.rs`), and
    /// 2. delete the `&& container.is_armed() == next.is_some()` half of the
    ///    memo in `Mux::sync_timeout`.
    ///
    /// `sync_timeout` then sees `deadline() == Some(D) == next` and emits
    /// nothing, leaving the session alive with no wheel entry. In a DEBUG build
    /// `Mux::debug_assert_timer_coherence` catches it first, inside the
    /// `reschedule` on the way out of `timeout`, with `timer coherence for
    /// Token(0): armed=false but the core wants Some(..)`; the `is_armed()`
    /// assertion below is what catches it with debug assertions off. Either
    /// mutation alone is enough; both together are the shape the refactor would
    /// have had if the memoized reschedule had landed before the stored
    /// deadline.
    #[test]
    fn an_early_wheel_delivery_leaves_a_live_entry_at_the_same_deadline() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 4, 16384)));
        let (mut mux, _peer) = h1_mux_with_idle_stream(&pool, Duration::from_secs(60));
        let mut metrics = SessionMetrics::new(None);

        let deadline = Instant::now() + Duration::from_secs(60);
        arm_frontend_at(&mut mux, Some(deadline));

        let result = mux.timeout(Token(0), &mut metrics);
        assert_eq!(result, StateResult::Continue);

        let container = mux
            .timeouts
            .get(&Token(0))
            .expect("the adapter keeps a handle for the frontend token");
        assert!(
            container.is_armed(),
            "the wheel entry was consumed by the delivery; if the memoized \
             reschedule does not put one back the session has no timer at all \
             — a lost wakeup"
        );
        assert_eq!(
            container.deadline(),
            Some(deadline),
            "the entry must go back at the SAME instant: a later one is a \
             silently extended timeout, a different one is a moved deadline"
        );
        assert_eq!(
            mux.frontend.poll_timeout(),
            Some(deadline),
            "an early delivery must not disturb what the core asked for"
        );
    }

    /// The mirror: a delivery at or past the deadline is a real expiry and must
    /// run the timeout body. Without this, "treat everything as early" would
    /// pass the two tests above and never time anything out.
    ///
    /// To SEE THIS RED: make `Mux::consume_timer_entry` return `false`
    /// unconditionally.
    #[test]
    fn a_delivery_past_the_deadline_runs_the_timeout_body() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 4, 16384)));
        let (mut mux, _peer) = h1_mux_with_idle_stream(&pool, Duration::from_secs(60));
        let mut metrics = SessionMetrics::new(None);

        arm_frontend_at(&mut mux, Some(Instant::now() - Duration::from_secs(1)));

        let _ = mux.timeout(Token(0), &mut metrics);

        assert_eq!(
            mux.context.streams[0].context.access_log_message,
            Some("client_timeout"),
            "a delivery at or past the deadline is a real expiry and must run \
             the timeout body"
        );
    }

    /// A firing for a core that wants no timer at all must fail OPEN — be
    /// reported due — rather than being re-validated away. Failing closed there
    /// would make such a session immortal, which is strictly worse than the
    /// up-to-99 ms-early close the re-validation exists to prevent.
    ///
    /// To SEE THIS RED: change `Mux::consume_timer_entry` to return `false`
    /// when `core_deadline` is `None`.
    #[test]
    fn a_firing_with_no_deadline_is_treated_as_due() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 4, 16384)));
        let (mut mux, _peer) = h1_mux_with_idle_stream(&pool, Duration::from_secs(60));
        let mut metrics = SessionMetrics::new(None);

        arm_frontend_at(&mut mux, None);
        assert_eq!(mux.frontend.poll_timeout(), None, "precondition: no timer");

        let _ = mux.timeout(Token(0), &mut metrics);

        assert_eq!(
            mux.context.streams[0].context.access_log_message,
            Some("client_timeout"),
            "a firing with no deadline must fail open and run the body"
        );
    }

    /// A real expiry must CLEAR the core's elapsed deadline rather than leave
    /// it in place for `reschedule` to re-arm.
    ///
    /// The core asked to be called back at `D` and has been. Leaving `D` there
    /// makes the memoized reschedule arm an already-elapsed instant, which the
    /// wheel re-delivers on the very next tick, and the session spins — the
    /// canonical sans-io busy loop that `UdpManager::handle_timeout`'s
    /// strict-advance `debug_assert!` exists to catch. Every branch of
    /// `Mux::timeout` that keeps the session alive re-arms explicitly with
    /// `arm_timeout`, which is what the old `triggered()` + `set(token)` pair
    /// did.
    ///
    /// Staged on a BACKEND token deliberately. A frontend firing runs
    /// `ConnectionH1::writable`, whose first act is `arm_timeout`, so the
    /// frontend re-arms as a side effect and cannot witness the difference. The
    /// backend `should_close` path re-arms nothing — it is the one place the
    /// elapsed deadline would survive.
    ///
    /// To SEE THIS RED: delete the `clear_timeout()` block from
    /// `Mux::consume_timer_entry`. The strict-advance `debug_assert!` in
    /// `SessionState::timeout` fires first (which is the point of it); with
    /// debug assertions off, these assertions catch it.
    #[test]
    fn a_real_expiry_clears_the_elapsed_deadline_instead_of_re_arming_it() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 8, 16384)));
        let (mut mux, _peer) = h1_mux_with_idle_stream(&pool, Duration::from_secs(60));
        let mut metrics = SessionMetrics::new(None);
        let backend_token = Token(1);

        let (mut connection, _backend_peer) =
            test_backend_connection(&mux, Duration::from_secs(30));
        let Connection::H1(h1) = &mut connection else {
            unreachable!("new_h1_client builds an H1 connection")
        };
        h1.stream = Some(0);
        let fired_at = Instant::now();
        park_backend_deadline(&mut connection, fired_at - Duration::from_secs(1));
        mux.router.backends.insert(backend_token, connection);
        mux.reschedule();

        // One linked stream whose response is already terminated and fully
        // proxied: the arm that leaves `should_close` true, so nothing re-arms
        // the backend.
        mux.context.link_stream(0, backend_token);
        mux.context.streams[0].back.consumed = true;
        mux.context.streams[0].back.parsing_phase = kawa::ParsingPhase::Terminated;

        let _ = mux.timeout(backend_token, &mut metrics);

        let core = mux
            .router
            .backends
            .get(&backend_token)
            .and_then(Connection::poll_timeout);
        assert!(
            core.is_none_or(|next| next > fired_at),
            "after a real expiry the core must want no timer or a strictly \
             later one, never the instant that just fired ({core:?})"
        );
        assert!(
            mux.timeouts
                .get(&backend_token)
                .and_then(TimeoutContainer::deadline)
                .is_none_or(|next| next > fired_at),
            "and the wheel must not be holding the elapsed instant either"
        );
    }

    /// A backend that leaves `router.backends` must take its wheel handle with
    /// it. Nothing cancels it explicitly any more — `reschedule`'s `retain`
    /// drops the handle and `TimeoutContainer::drop` cancels the entry — so the
    /// eviction has to be pinned somewhere.
    ///
    /// To SEE THIS RED: delete the `self.timeouts.retain(...)` call from
    /// `Mux::reschedule`. The handle then outlives its connection and
    /// `debug_assert_timer_coherence` panics with "timer handle for Token(1)
    /// outlived its connection".
    #[test]
    fn a_departed_backend_releases_its_wheel_handle() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 8, 16384)));
        let (mut mux, _peer) = h1_mux_with_idle_stream(&pool, Duration::from_secs(60));
        let backend_token = Token(1);
        let (connection, _backend_peer) = test_backend_connection(&mux, Duration::from_secs(30));
        mux.router.backends.insert(backend_token, connection);

        mux.reschedule();
        assert!(
            mux.timeouts.contains_key(&backend_token),
            "a live backend must hold a wheel handle"
        );

        mux.router.backends.remove(&backend_token);
        mux.reschedule();

        assert!(
            !mux.timeouts.contains_key(&backend_token),
            "a departed backend must not leave a wheel handle behind"
        );
    }

    /// An H1 backend connection on a live loopback socket, plus the peer the
    /// caller must keep alive.
    fn test_backend_connection(
        mux: &Mux<mio::net::TcpStream, test_support::TestListener>,
        duration: Duration,
    ) -> (Connection<SessionTcpStream>, std::net::TcpStream) {
        let (backend_socket, backend_peer) = connected_socket();
        let backend_address = "127.0.0.1:2".parse().expect("backend address must parse");
        let backend = Rc::new(RefCell::new(Backend::new(
            "test-backend",
            backend_address,
            None,
            None,
            None,
        )));
        let connection = Connection::new_h1_client(
            Ulid::generate(),
            SessionTcpStream::new(backend_socket, mux.session_ulid, Some(backend_address)),
            "test-cluster".to_owned(),
            backend,
            duration,
        );
        (connection, backend_peer)
    }

    /// The loose end flagged as UNVERIFIED in the `poll_timeout` series: in the
    /// backend-token branch of `Mux::timeout`, the backend's timer is consumed
    /// unconditionally but re-armed only when `!should_close`, while the shared
    /// tail can still return `Continue`. If a backend could reach that tail
    /// still holding linked streams, those streams would be left with no
    /// backend timer at all — a lost wakeup.
    ///
    /// It cannot, and this pins the reason: the branch calls
    /// `Connection::end_stream` for every id in `context.backend_streams`.
    /// `ConnectionH2::end_stream` starts with `Context::unlink_stream`;
    /// `ConnectionH1::end_stream` guards `self.stream != Some(stream)` and
    /// returns early first, which is safe only because an H1 connection carries
    /// at most one linked stream and it equals `self.stream` (the comment at
    /// the re-arm site has the full chain). This test sets
    /// `h1.stream = Some(0)` by hand, so it exercises the MATCHED path only —
    /// the mismatch arm is unreached here. It is covered by that invariant plus
    /// the `debug_assert_h1_owns_stream` tripwire that runs at every
    /// `end_stream` call in `Mux::timeout`, not by a test that drives the arm
    /// itself. It drives the exact shape the issue asked for — a backend
    /// stalled after a PARTIAL response (`back.consumed`, not terminated),
    /// which is the arm that sets `should_write` while leaving `should_close`
    /// true, i.e. the arm that skips the re-arm.
    ///
    /// To SEE THIS RED: delete the `self.context.unlink_stream(stream_id);`
    /// from the "forcefully terminate it" arm AND the
    /// `backend.end_stream(stream_id, &mut self.context);` at the bottom of
    /// that loop — this arm evicts through both, so either alone still clears
    /// the index. The companion test
    /// `a_completed_backend_response_timeout_unlinks_through_end_stream_alone`
    /// covers the arm where `end_stream` IS the sole eviction.
    #[test]
    fn a_backend_timeout_leaves_no_linked_stream_behind_on_the_close_path() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 8, 16384)));
        let (mut mux, _peer) = h1_mux_with_idle_stream(&pool, Duration::from_secs(60));
        let mut metrics = SessionMetrics::new(None);
        let backend_token = Token(1);

        let (mut connection, _backend_peer) =
            test_backend_connection(&mux, Duration::from_secs(30));
        let Connection::H1(h1) = &mut connection else {
            unreachable!("new_h1_client builds an H1 connection")
        };
        h1.stream = Some(0);
        // Due, not early: the entry really elapsed.
        park_backend_deadline(&mut connection, Instant::now() - Duration::from_secs(1));
        mux.router.backends.insert(backend_token, connection);
        mux.reschedule();

        mux.context.link_stream(0, backend_token);
        // A response that started but never finished.
        mux.context.streams[0].back.consumed = true;
        assert_eq!(
            mux.context
                .backend_streams
                .get(&backend_token)
                .map(Vec::len),
            Some(1),
            "precondition: the backend carries exactly one linked stream"
        );

        let _ = mux.timeout(backend_token, &mut metrics);

        assert!(
            mux.context
                .backend_streams
                .get(&backend_token)
                .is_none_or(|ids| ids.is_empty()),
            "every stream linked to a timed-out backend must be unlinked before \
             the shared tail runs, because the tail may return Continue with \
             only the frontend timer re-armed"
        );
        assert_eq!(
            mux.context.streams[0].context.access_log_message,
            Some("backend_response_timeout"),
            "precondition: this is the partial-response arm, the one that \
             leaves should_close true and therefore skips the backend re-arm"
        );
    }

    /// The other half of the loose-end proof. In the "response terminated and
    /// fully proxied" arm of the backend-timeout branch there is no
    /// `unlink_stream` call of its own: the only thing that evicts the stream
    /// from `context.backend_streams` on this arm is the unconditional
    /// `backend.end_stream(...)` at the bottom of the loop. As above, this test
    /// sets `h1.stream = Some(0)` so `ConnectionH1::end_stream` takes its
    /// matched path; the mismatch arm is unreached, and guarded by
    /// `debug_assert_h1_owns_stream` rather than by a test. That arm also
    /// leaves `should_close` true and `should_write` false, which is exactly
    /// the `delay_close_for_frontend_flush("timeout")` path the issue named as
    /// the one that can return `Continue` with only the frontend re-armed.
    ///
    /// To SEE THIS RED: delete `backend.end_stream(stream_id, &mut
    /// self.context);` from the bottom of that loop.
    #[test]
    fn a_completed_backend_response_timeout_unlinks_through_end_stream_alone() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 8, 16384)));
        let (mut mux, _peer) = h1_mux_with_idle_stream(&pool, Duration::from_secs(60));
        let mut metrics = SessionMetrics::new(None);
        let backend_token = Token(1);

        let (mut connection, _backend_peer) =
            test_backend_connection(&mux, Duration::from_secs(30));
        let Connection::H1(h1) = &mut connection else {
            unreachable!("new_h1_client builds an H1 connection")
        };
        h1.stream = Some(0);
        park_backend_deadline(&mut connection, Instant::now() - Duration::from_secs(1));
        mux.router.backends.insert(backend_token, connection);
        mux.reschedule();

        mux.context.link_stream(0, backend_token);
        // Terminated AND fully proxied: the arm that does nothing but fall
        // through to `end_stream`.
        mux.context.streams[0].back.consumed = true;
        mux.context.streams[0].back.parsing_phase = kawa::ParsingPhase::Terminated;
        assert!(
            mux.context.streams[0].back.is_terminated()
                && mux.context.streams[0].back.is_completed(),
            "precondition: this test needs the terminated-and-completed arm"
        );

        let _ = mux.timeout(backend_token, &mut metrics);

        assert!(
            mux.context
                .backend_streams
                .get(&backend_token)
                .is_none_or(|ids| ids.is_empty()),
            "end_stream is the sole eviction point on this arm; without it the \
             backend reaches the shared tail still holding a linked stream and \
             with no timer of its own"
        );
        assert_eq!(
            mux.context.streams[0].context.access_log_message, None,
            "precondition: a fully proxied response is not a timeout outcome, \
             so this arm leaves should_write false and reaches \
             delay_close_for_frontend_flush"
        );
    }
}
