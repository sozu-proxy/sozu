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
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use mio::{Interest, Token, net::TcpStream};
use rand::{SeedableRng, rngs::StdRng};
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
/// - `peer` — [`Connection::peer_address`]: a snapshot, not a live lookup
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
            peer = $self.frontend.peer_address(),
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
            peer = $self.frontend.peer_address(),
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

use self::h2::MetricEvent;

pub mod answers;
pub mod auth;
pub mod buffer_source;
pub mod connection;
mod converter;
pub mod debug;
mod h1;
mod h2;
mod h2_close;
mod h2_control_tx;
mod h2_drain;
mod h2_flood_detector;
mod h2_flow_control;
mod h2_header_reassembly;
mod h2_scheduler;
mod h2_stream_table;
pub mod h2_transmit;
mod h2_write_pass;
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
    Protocol, ProxySession, Readiness, RetrieveClusterError, SessionIsToBeClosed, SessionMetrics,
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
    buffer_source::{BufferSource, PoolBufferSource},
    connection::Connection,
    debug::{DebugEvent, DebugHistory},
    h1::ConnectionH1,
    h2::CLIENT_PREFACE_SIZE,
    h2::ConnectionH2,
    h2::H2ByteAccounting,
    h2::H2ConnectionConfig,
    h2::{
        H2ControlFlushStage, H2ControlFlushTarget, H2FinalizeTarget, H2ForceDisconnectTarget,
        H2ReadOutcome, H2ReadTarget, H2Shell, H2StreamId, H2WritableStateTarget, H2WriteTarget,
    },
    h2_flood_detector::H2FloodConfig,
    h2_write_pass::H2WritePass,
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
/// traced: `H2Shell::close` and `ConnectionH2::end_stream` iterate their
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

/// An opaque, session-scoped name for one entry of the worker's backend
/// registry.
///
/// It is an index into `Mux::backend_registry` and nothing else: the core
/// can carry it, compare it and hand it back, and cannot turn it into a
/// `Backend`. Only the embedder resolves it, which is what keeps
/// `Rc<RefCell<Backend>>` out of the core (#1340, Question 12).
///
/// Session-scoped rather than derived from `(cluster_id, backend_id)` on
/// purpose. A reload that replaces a registry entry mid-session builds a NEW
/// `Rc`, which takes a NEW slot, so connections opened before it keep charging
/// the handle they were dialled against — exactly what holding the `Rc`
/// inside `Position::Client` used to do. Keying on the backend's name instead
/// would split a balanced start/end pair across two `Backend` values: the
/// superseded one would keep a charge forever and the fresh one would
/// saturate at zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BackendSlot(usize);

/// A backend as the core sees it: the opaque slot, plus the two identity
/// fields the datapath renders.
///
/// `backend_id` and `address` are copied once, when the backend is dialled.
/// Both are immutable for the life of a registry entry, so carrying them is
/// not a cached view of mutable state — no load state crosses this boundary.
/// The mutable half (`active_connections`, `active_requests`, `failures`,
/// `connection_time`, `health`, `status`) stays behind the slot, and the core
/// reaches it only by emitting a [`BackendDelta`].
///
/// `backend_id` is an `Rc<str>` because the `BackendStatus::Connecting` ->
/// `BackendStatus::Connected` transition rebuilds the `Position::Client`
/// variant once per dial. That reconstruction cost one `Rc` bump when the
/// field was an `Rc<RefCell<Backend>>`; a `String` would make it a heap copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendId {
    /// Private to this module: [`Mux::backend`] is the only way out.
    slot: BackendSlot,
    /// `Backend::backend_id`, for log lines and per-backend metric labels.
    pub backend_id: Rc<str>,
    /// `Backend::address`, for log lines and `push_event` payloads.
    pub address: SocketAddr,
}

impl BackendId {
    /// The slot this id names. Module-private on purpose — see [`Mux::backend`]
    /// for the crate-visible resolution path.
    fn slot(&self) -> BackendSlot {
        self.slot
    }
}

/// One accounting change the core decided and the embedder performs.
///
/// LIFECYCLE §9 invariant 14 is a balance, and this is the ledger it balances:
/// every [`BackendChange::StreamsStarted`] unit the mux emits is covered by
/// exactly one [`BackendChange::StreamsEnded`] unit against the same slot.
/// Because every write to `Backend::active_requests` and
/// `Backend::active_connections` on the mux path now goes through
/// `Mux::apply_backend_deltas`, reconciling the emitted stream to zero is a
/// direct test of the invariant rather than a proxy for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackendDelta {
    /// The registry entry to charge.
    pub slot: BackendSlot,
    /// What to charge it.
    pub change: BackendChange,
}

/// The accounting operations [`BackendDelta`] can carry.
///
/// Deliberately only the two counters invariant 14 is about. The health and
/// latency writes the mux also makes (`failures`, `retry_policy`,
/// `connection_time`) are NOT here: they are not part of the balance, and the
/// load balancer reads `retry_policy`, so they stay immediate at their
/// existing embedder-side call sites rather than being deferred to a drain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendChange {
    /// `Backend::active_requests += n` — `n` streams started on this backend.
    StreamsStarted(usize),
    /// `Backend::active_requests -= n`, saturating — `n` streams left it.
    StreamsEnded(usize),
    /// `Backend::dec_connections()` — the connection to this backend closed.
    ConnectionClosed,
}

/// The embedder's table of registry handles, indexed by [`BackendSlot`].
///
/// A newtype rather than a bare `Vec` so the two operations that may cross
/// Question 12's perimeter — minting a slot for a freshly dialled backend,
/// and performing a [`BackendDelta`] against one — are the only ones there
/// are. Nothing hands a `&Rc<RefCell<Backend>>` out of here except
/// [`Mux::backend`], whose one caller is the WebSocket upgrade.
#[derive(Debug, Default)]
pub(crate) struct BackendRegistry(Vec<Rc<RefCell<Backend>>>);

impl BackendRegistry {
    /// Name `backend` with a slot, reusing the one it already has.
    ///
    /// Identity is `Rc::ptr_eq`, not the backend's id or address: two dials
    /// separated by a reload that replaced the registry entry are two
    /// different `Backend` values, and each must keep its own accounting.
    /// The scan is linear over the backends ONE session has dialled, which
    /// is the cluster's backend count at worst — the same order as the
    /// `Router::backends` walk the caller has just done.
    fn intern(&mut self, backend: &Rc<RefCell<Backend>>) -> BackendSlot {
        match self.0.iter().position(|known| Rc::ptr_eq(known, backend)) {
            Some(slot) => BackendSlot(slot),
            None => {
                self.0.push(backend.clone());
                BackendSlot(self.0.len() - 1)
            }
        }
    }

    /// Build the core's view of a backend it is about to be handed.
    fn id_for(&mut self, backend: &Rc<RefCell<Backend>>) -> BackendId {
        let slot = self.intern(backend);
        let borrow = backend.borrow();
        BackendId {
            slot,
            backend_id: Rc::from(borrow.backend_id.as_str()),
            address: borrow.address,
        }
    }

    fn get(&self, slot: BackendSlot) -> Option<&Rc<RefCell<Backend>>> {
        self.0.get(slot.0)
    }

    /// Resolve one of the core's opaque [`BackendId`]s to the registry handle
    /// it names.
    ///
    /// The single crate-visible way back from an id to an
    /// `Rc<RefCell<Backend>>`, so the perimeter Question 12 draws has one
    /// door rather than a `pub` slot every caller can walk through. Its only
    /// callers are `upgrade_mux` in `lib/src/http.rs` and `lib/src/https.rs`,
    /// which hand the live handle to the WebSocket `Pipe` that takes the
    /// backend over.
    ///
    /// It sits here rather than on [`Mux`] because those callers have already
    /// moved `Mux::frontend` out by the time they need it; a field access
    /// borrows disjointly where a `&self` method on `Mux` could not.
    ///
    /// `None` cannot happen for an id this session minted — the table never
    /// shrinks — so a caller treats it as the desync it would be.
    pub(crate) fn handle(&self, backend: &BackendId) -> Option<&Rc<RefCell<Backend>>> {
        self.get(backend.slot())
    }

    /// Perform one accounting change the core decided.
    ///
    /// The before/after pair-assertions that used to sit at each emitting
    /// site live here now: this is where both halves are observable, and one
    /// copy covers every emitter instead of each carrying its own.
    fn apply(&self, delta: BackendDelta) {
        let Some(backend) = self.get(delta.slot) else {
            // Unreachable while the vector only grows, which it does. Report
            // rather than panic: losing one charge is a drifting gauge, and
            // dropping a session over it would be worse.
            error!(
                "{} backend accounting delta {:?} names a slot this session never minted",
                log_module_context!(),
                delta
            );
            return;
        };
        let mut backend = backend.borrow_mut();
        match delta.change {
            BackendChange::StreamsStarted(count) => {
                let before = backend.active_requests;
                backend.active_requests += count;
                debug_assert_eq!(
                    backend.active_requests,
                    before + count,
                    "a StreamsStarted delta must raise active_requests by exactly its count"
                );
            }
            BackendChange::StreamsEnded(count) => {
                // `saturating_sub` is the network-safe floor a desynced peer
                // needs, so the post-relation is all that can be asserted —
                // not that `before >= count`.
                let before = backend.active_requests;
                backend.active_requests = backend.active_requests.saturating_sub(count);
                debug_assert_eq!(
                    backend.active_requests,
                    before.saturating_sub(count),
                    "a StreamsEnded delta must lower active_requests by its count (saturating)"
                );
                debug_assert!(
                    backend.active_requests <= before,
                    "active_requests must not grow on a StreamsEnded delta"
                );
            }
            BackendChange::ConnectionClosed => {
                // `dec_connections` floors at 0 (a double close from a
                // desynced peer must not panic), so the post-relation is
                // "decreased by one, unless already at zero".
                let before = backend.active_connections;
                backend.dec_connections();
                debug_assert_eq!(
                    backend.active_connections,
                    before.saturating_sub(1),
                    "a ConnectionClosed delta must release exactly one backend connection \
                     (saturating at 0)"
                );
            }
        }
        trace!(
            "{} backend accounting {:?} applied: {:#?}",
            log_module_context!(),
            delta.change,
            backend
        );
    }
}

pub enum Position {
    Client(String, BackendId, BackendStatus),
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

    /// The bytes-read event for this side.
    ///
    /// Returns exactly one event — never zero, never two — because both arms
    /// of the match below emit one, so the caller needs no container to carry
    /// it. A caller that is its own shell records it at once; a caller inside
    /// the H2 core queues it. `#[must_use]` is what makes that a compiler
    /// obligation rather than a convention: this leaf is reached from a core
    /// AND from shells, and an event silently dropped here is an upward
    /// drift that never underflows, so nothing would log and nothing would
    /// saturate.
    #[must_use]
    fn bytes_in_event(&self, size: usize) -> MetricEvent {
        match self {
            Position::Client(..) => MetricEvent::BackendBytesIn(size as i64),
            Position::Server => MetricEvent::FrontendBytesIn(size as i64),
        }
    }

    /// The bytes-written event for this side. Same one-event guarantee and
    /// same reason for `#[must_use]` as [`Self::bytes_in_event`].
    #[must_use]
    fn bytes_out_event(&self, size: usize) -> MetricEvent {
        match self {
            Position::Client(..) => MetricEvent::BackendBytesOut(size as i64),
            Position::Server => MetricEvent::FrontendBytesOut(size as i64),
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
    /// Smoothed round-trip time of the peer side of a stream, already
    /// sampled.
    ///
    /// Used by access-log emission to report TCP_INFO RTT for the side the
    /// caller does NOT own directly: a frontend connection (Position::Server)
    /// reports the backend's RTT through this method, and a backend connection
    /// (Position::Client) reports the frontend's the same way. `token` is
    /// ignored by `connection::EndpointServer` (which has a single
    /// frontend connection) and used as a key by
    /// `connection::EndpointClient` (which keys backends by token).
    /// Returns `None` when the token doesn't resolve, mirroring the existing
    /// fallback paths in `readiness`/`readiness_mut`, and also when the
    /// platform declines to answer.
    ///
    /// This returns the VALUE, not the socket it came from. The predecessor,
    /// `fn socket(&self, token) -> Option<&TcpStream>`, handed out a
    /// `mio::net::TcpStream` — a concrete OS type — so any connection could
    /// reach any other connection's socket for any purpose, and no in-memory
    /// transport could ever satisfy the trait. RTT is intrinsically a live
    /// socket property and stays on the embedder's side of the boundary; the
    /// cores receive an `Option<Duration>` that was captured for them.
    fn peer_rtt(&self, token: Token) -> Option<Duration>;
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
    /// Where every buffer this session's streams and connections need comes
    /// from, and the only thing allowed to refuse one.
    ///
    /// Boxed rather than concrete so the core names the contract rather than
    /// the worker's [`Pool`]: a refusal is the same event whatever stands
    /// behind it, and the `REFUSED_STREAM` answer to one is the same answer.
    /// [`Context::new`] installs the [`PoolBufferSource`] the proxy runs on.
    pub buffers: Box<dyn BufferSource>,
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
    /// Whether this session's listener speaks HTTPS or plaintext HTTP.
    ///
    /// The one listener value that is genuinely per-connection rather than
    /// per-request: a listener's protocol is fixed by its type and no
    /// `update_config` patch can change it, so capturing it once in
    /// [`Self::new`] frees both readers — [`Self::create_stream`] and the H2
    /// write pass's `:scheme` decision — from borrowing the listener at all.
    /// Contrast the per-request knobs, which [`Self::create_stream`] re-reads
    /// on every stream precisely because a reload *can* change them
    /// (`LIFECYCLE.md` §2.5).
    pub protocol: Protocol,
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
    /// `h2::ConnectionH2::now` at each public entry point — instead of
    /// calling [`Instant::now`] itself, so one pass sees one consistent
    /// "now". Every deadline armed or evaluated inside a pass is therefore
    /// accurate to within that pass — **in either direction**. The error is
    /// not one-sided: an arm site runs at an arbitrary depth into its pass
    /// (a DATA or HEADERS refresh, an outbound-byte refresh) and stamps the
    /// snapshot taken at the START of that pass, so the stored instant is
    /// older than the event it records; an eval site runs near the top of a
    /// pass (`cancel_timed_out_streams` is the first thing
    /// `h2::ConnectionH2::poll_read_target` does). The measured age is therefore
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
    /// Wall-clock sibling of [`Self::now`]: Unix milliseconds for the pass
    /// currently executing.
    ///
    /// [`Self::now`] is an [`Instant`] — an opaque monotonic point with no
    /// epoch and no defined conversion to civil time — so it cannot supply
    /// the 48-bit Unix-millisecond prefix a ULID carries in its high bits
    /// (see [`Self::next_request_id`]). This field is that value, and it is
    /// refreshed by [`Mux`] at exactly the three sites that refresh
    /// [`Self::now`], from the same pass. The two are siblings: a site that
    /// sets one and not the other lets a request id carry the timestamp of
    /// an older pass.
    ///
    /// It is a snapshot rather than a provider closure for the same reason
    /// [`Self::now`] is: the mux's discipline is one clock sample per pass,
    /// read by the core from a field, and a boxed provider would let the
    /// core reach the host at an arbitrary depth into a pass instead.
    pub now_wall_ms: u64,
    /// Entropy source for the 80-bit random suffix of a request ULID.
    ///
    /// `Ulid::generate()` is exactly
    /// `Ulid::from_timestamp_with_rng(unix_epoch_ms(), &mut rand::rng())` —
    /// two ambient reaches (the wall clock and a thread-local RNG) hidden
    /// inside a dependency, invisible to any grep over this module for
    /// `Instant::now` / `SystemTime::now` / `rand`. Holding the RNG here
    /// replaces the second with a field the embedder owns, exactly as
    /// [`Self::now_wall_ms`] replaces the first.
    ///
    /// Seeded once per session in [`Self::new`] from `rand::rng()` — the
    /// same OS-backed reseeding CSPRNG `Ulid::generate()` draws from, so the
    /// unpredictability of a request id is unchanged. A request id that
    /// repeats across sessions would be a regression, so nothing in the mux
    /// re-seeds it; a deterministic simulator assigns this field directly
    /// after construction instead.
    pub request_id_rng: StdRng,
    /// Backend accounting the core decided and the embedder has not performed
    /// yet (#1340, Question 12).
    ///
    /// The core's outbound channel for registry writes, in the same place
    /// `udp`'s `Output` queue sits: a site that used to reach a
    /// `Rc<RefCell<Backend>>` through `Position::Client` and mutate it pushes
    /// a [`BackendDelta`] here instead. `Mux::apply_backend_deltas` is the
    /// only drain, and the only writer of `Backend::active_requests` /
    /// `Backend::active_connections` on the mux path.
    ///
    /// Entries are applied in the order they were pushed, and the drain runs
    /// before anything reads backend load state — in practice, before each
    /// `Router::plan_connect`, whose load balancer is that sole reader. FIFO plus
    /// drain-before-read leaves the counters at every read point exactly
    /// where mutating in place left them, saturation included.
    pub backend_deltas: Vec<BackendDelta>,
}

/// Unix milliseconds for the wall clock, saturating to 0 before the epoch.
///
/// The single host wall-clock reach behind [`Context::now_wall_ms`]. Kept as
/// one function so the three [`Mux`] refresh sites and [`Context::new`] cannot
/// drift apart on the unit or on the pre-epoch case.
fn unix_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
        let protocol = listener.borrow().protocol();
        Self {
            streams: Vec::new(),
            pending_links: VecDeque::new(),
            backend_streams: HashMap::new(),
            buffers: Box::new(PoolBufferSource::new(pool)),
            listener,
            session_ulid,
            session_address,
            public_address,
            debug: DebugHistory::new(),
            h2_stream_shrink_ratio,
            tls_server_name: None,
            tls_cert_names: None,
            protocol,
            tls_version: None,
            tls_cipher: None,
            tls_alpn: None,
            now: Instant::now(),
            now_wall_ms: unix_epoch_ms(),
            request_id_rng: StdRng::from_rng(&mut rand::rng()),
            backend_deltas: Vec::new(),
        }
    }

    /// Record one backend accounting change for the embedder to perform.
    ///
    /// The single push site, so every emitter reads the same way and a
    /// `grep` for `record_backend_delta` enumerates the ledger's inputs.
    pub(super) fn record_backend_delta(&mut self, backend: &BackendId, change: BackendChange) {
        self.backend_deltas.push(BackendDelta {
            slot: backend.slot(),
            change,
        });
    }

    /// Mint the request ULID for a stream this session is about to open.
    ///
    /// Replaces `Ulid::generate()` at the mux's single production ULID site.
    /// `generate()` composes two ambient reaches — the wall clock and a
    /// thread-local RNG — inside `rusty_ulid`; this composes the same ULID
    /// from [`Self::now_wall_ms`] and [`Self::request_id_rng`], the two
    /// sources the embedder owns. Same bytes, same layout, no host reach
    /// from inside the core.
    ///
    /// `Ulid::from_timestamp_with_rng` panics above `0xFFFF_FFFF_FFFF` ms
    /// (year 10889); [`Self::now_wall_ms`] is a Unix-millisecond count, so
    /// reaching it requires a host clock set eight millennia ahead.
    pub fn next_request_id(&mut self) -> Ulid {
        Ulid::from_timestamp_with_rng(self.now_wall_ms, &mut self.request_id_rng)
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
    /// the caller is inside `Router::plan_connect`, the H2 mux, or a free
    /// helper. Panics on an out-of-bounds `stream_id`, which is the same
    /// behaviour as the raw `streams[sid]` indexing it replaces.
    pub fn http_context(&self, stream_id: GlobalStreamId) -> &HttpContext {
        &self.streams[stream_id].context
    }

    /// Mutable sibling of [`Self::http_context`]. Use when routing
    /// decisions need to stamp `cluster_id` / `backend_id` on the stream's
    /// [`HttpContext`] (e.g. `Router::plan_connect` at the fill-cluster /
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

    /// Open (or recycle) the stream slot for a request that has just arrived,
    /// capturing the listener configuration it will run on.
    ///
    /// This is the mux's single listener-snapshot point. `ConnectionH2` calls
    /// it from its HEADERS branch, so an H2 stream captures the listener as
    /// the operator had it configured when that request's HEADERS landed, and
    /// finishes on that capture whatever a concurrent reload does; the next
    /// HEADERS on the same connection takes a fresh one. The H1 sessions in
    /// `http.rs` / `https.rs` call it once per connection instead, because one
    /// H1 connection owns one slot that `HttpContext::reset` deliberately
    /// carries across keep-alive requests. `LIFECYCLE.md` §2.5 states the
    /// resulting staleness window for both.
    ///
    /// Every value read under the borrow below is re-read on each call for
    /// that reason. The two that are NOT read here — [`Self::protocol`] and
    /// the TLS fields — are connection-scoped by nature and captured once.
    pub fn create_stream(&mut self, request_id: Ulid, window: u32) -> Option<GlobalStreamId> {
        let (http_context, answers) = {
            let listener = self.listener.borrow();
            let mut http_context = HttpContext::new(
                self.session_ulid,
                request_id,
                self.protocol,
                self.public_address,
                self.session_address,
                listener.get_sticky_name().to_string(),
                listener.get_sozu_id_header().to_string(),
                listener.get_elide_x_real_ip(),
                listener.get_send_x_real_ip(),
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
            http_context.strict_sni_binding = listener.get_strict_sni_binding();
            // Propagate the connection-scoped TLS metadata onto every
            // per-stream HttpContext so the access log can record it without
            // touching the rustls session on every request. These are
            // `&'static str` borrows from the rustls label tables — copy is
            // a pointer move.
            http_context.tls_version = self.tls_version;
            http_context.tls_cipher = self.tls_cipher;
            http_context.tls_alpn = self.tls_alpn;
            // The answer registry this request will render any default answer
            // from. Cloning the handle here — rather than borrowing the
            // listener again at each `set_default_answer` site — is what binds
            // a stream to the templates that were installed when it started.
            (http_context, listener.get_answers().clone())
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
            // The ONE place `request_counted` is cleared without emitting the
            // paired `http.active_requests` decrement, and it must stay the
            // only one. A slot reaches `StreamState::Recycle` only through
            // `Stream::generate_access_log`, which already emitted the `-1`
            // and cleared the flag, so this is re-initialisation of an
            // already-false field rather than a decrement that went missing.
            // `Stream::new` asserts the same invariant for a fresh slot
            // ("new stream must not have a counted request"); this is its
            // reuse-path counterpart. A future site that clears this flag
            // without accounting for the gauge is a silent upward leak: the
            // `+1` stays in the aggregate with nothing left to pair it, and
            // an aggregate that only drifts up never underflows, so nothing
            // logs and nothing saturates.
            stream.request_counted = false;
            stream.window = i32::try_from(window).unwrap_or(i32::MAX);
            stream.context = http_context;
            // A recycled slot takes the fresh capture too: the request that
            // released it ran on whatever the listener held then, and the one
            // taking it over must not inherit that.
            stream.answers = answers;
            stream.back.clear();
            stream.back.storage.clear();
            stream.front.clear();
            stream.front.storage.clear();
            stream.forget_upstream_replay();
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
        let stream = Stream::new(&mut *self.buffers, http_context, answers, window)?;
        self.streams.push(stream);
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
    /// and `Mux::reschedule` reflects that onto `crate::timer`, arming only
    /// when what the wheel holds differs from what the core wants. This map is
    /// the ONLY thing in the mux that talks to the timer wheel.
    ///
    /// A handle for a token that has left `router.backends` is dropped by
    /// `reschedule`'s `retain`, and `TimeoutContainer::drop` cancels its entry
    /// — which is why no backend-removal site has to remember to cancel.
    pub timeouts: HashMap<Token, TimeoutContainer>,
    /// The registry handles the core's [`BackendSlot`]s stand for.
    ///
    /// The embedder's half of Question 12's perimeter: the core carries an
    /// opaque slot, this resolves it, and `Mux::apply_backend_deltas` is
    /// what turns the core's [`BackendDelta`]s into writes on the worker's
    /// `Backend` values. Indexed by slot.
    ///
    /// A dial reuses an existing slot when `Rc::ptr_eq` matches, so the
    /// vector is bounded by the number of DISTINCT backends a session dials
    /// rather than by its request count. Entries are never removed while the
    /// session lives: a delta pushed before a backend connection closed must
    /// still resolve when the drain reaches it, and an index that moved would
    /// silently mis-charge it.
    pub(crate) backend_registry: BackendRegistry,
    /// Per-session correlation ID generated at construction time. Included in
    /// every log line emitted from this module so all events for a single
    /// frontend connection can be reassembled (independent of the ephemeral
    /// per-stream request id used by access logs).
    pub session_ulid: Ulid,
}

/// Answer the per-`(cluster, source-IP)` limit step the core paused at.
///
/// The embedder half of [`router::ConnectStep::CheckIpLimit`]. The core
/// cannot do this itself: it is a consult of
/// `SessionManager::cluster_ip_at_limit` **and**, on the admitting path, a
/// call to `SessionManager::track_cluster_ip`, which mutates
/// `connections_per_cluster_ip` — worker-global state keyed on
/// `(cluster, ip)` across every session, and read again by
/// `lib/src/tcp.rs`'s own gate for the TCP proxy.
///
/// **The track happens here, before the decision resumes, and that ordering
/// is the point.** An `Admitted` verdict asserts to the core that the slot
/// has been taken. Resuming as `Admitted` without having tracked would let
/// the stream through uncounted, and because the counter is worker-global
/// the next session from the same IP would find a slot that should already
/// be gone. `an_admitted_stream_is_counted_before_the_decision_resumes`
/// pins it.
///
/// A free function rather than a `Mux` method so it can be driven directly:
/// the ordering above is the thing worth testing, and testing it through a
/// copy of this logic would prove nothing about this logic.
fn consult_ip_gate(
    sessions: &Rc<RefCell<crate::server::SessionManager>>,
    frontend_token: Token,
    resume: &router::ConnectResume,
) -> router::IpGateVerdict {
    // BOTH caps are consulted here, through the one combined gate:
    // the pre-existing per-(cluster, source-IP) cap and the
    // per-(cluster, source-SUBNET) cap that stands beside it. A
    // connection is admitted only when both allow it, which is what
    // makes "10 per IP AND 100 per /64" expressible. The subnet cap is
    // `0` (disabled) by default, in which case this is byte-for-byte
    // the previous per-IP consult.
    let at_limit = sessions.borrow().cluster_connection_at_limit(
        frontend_token,
        resume.cluster_id(),
        &resume.ip(),
        resume.max_connections_per_ip(),
        resume.max_connections_per_subnet(),
    );
    if at_limit {
        let retry_after = sessions
            .borrow()
            .effective_retry_after(resume.cluster_retry_after());
        return router::IpGateVerdict::AtLimit { retry_after };
    }
    // Idempotent track — H2 streams to the same `(cluster, ip)` share a
    // single slot in the per-token set. The decrement happens wholesale on
    // session close, via `untrack_all_cluster_ip`.
    sessions.borrow_mut().track_cluster_connection(
        frontend_token,
        resume.cluster_id().to_owned(),
        resume.ip(),
        resume.max_connections_per_subnet(),
    );
    router::IpGateVerdict::Admitted
}

impl<Front: SocketHandler, L: ListenerHandler + L7ListenerHandler> Mux<Front, L> {
    pub fn front_socket(&self) -> &TcpStream {
        self.frontend.socket()
    }

    /// Perform every backend accounting change the core has decided since the
    /// last drain, in the order it decided them.
    ///
    /// The ONLY writer of `Backend::active_requests` and
    /// `Backend::active_connections` on the mux path. `mem::take` empties the
    /// queue before the first change is applied, so running this twice in a
    /// row is idempotent — the second call finds nothing and performs
    /// nothing.
    ///
    /// Called from the four `Mux` wrappers that already owe the timer wheel a
    /// `reschedule` on every exit, and once more immediately before each
    /// `Router::plan_connect`, because that call's load balancer is the only
    /// reader of the counters this drains.
    pub(crate) fn apply_backend_deltas(&mut self) {
        for delta in std::mem::take(&mut self.context.backend_deltas) {
            self.backend_registry.apply(delta);
        }
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

    /// Sample the frontend's TCP round-trip time for this pass and hand it to
    /// the H2 core.
    ///
    /// The counterpart of `unix_epoch_ms` for a value that costs a syscall
    /// instead of a clock read, and kept as one function for the same reason:
    /// the three entry points below cannot drift apart on the gate or on
    /// which socket is read.
    ///
    /// Called from [`SessionState::ready`], [`Mux::timeout_inner`] and
    /// [`Mux::shutting_down_inner`] — every `Mux` entry point that can reach
    /// `ConnectionH2::snapshot_rtts` and therefore emit an access log. The
    /// last two matter as much as the first: `timeout_inner` runs
    /// `ConnectionH2::cancel_timed_out_streams` precisely when the peer has
    /// gone silent and no `ready()` has run for a while, which is when the
    /// previous pass's sample is oldest. `Mux::close` is deliberately NOT on
    /// the list: it already takes its own sample for its own teardown loop,
    /// and `H2Shell::close` emits no access log.
    ///
    /// H1 is not gated out by accident. `ConnectionH1` reads its own socket at
    /// each access log, one stream at a time, and an H1 connection carries one
    /// request at a time, so there is nothing for a per-pass sample to
    /// amortise there — taking one would add a syscall per pass and change
    /// nothing a reader of the log can see.
    ///
    /// Issue #1339 Q11. The rejected alternative was to refresh the value at
    /// every `ConnectionH2` entry point the way `Context::now` is refreshed;
    /// it measured 7.8–9.7× the syscall rate for +6.5% / +5.3% CPU.
    fn refresh_client_rtt(&mut self) {
        if let Connection::H2(connection) = &mut self.frontend {
            connection.core.client_rtt = socket_rtt(connection.socket.socket_ref());
        }
    }
}

impl<Front: SocketHandler + std::fmt::Debug, L: ListenerHandler + L7ListenerHandler> SessionState
    for Mux<Front, L>
{
    /// Thin wrapper over `Mux::ready_inner` whose only job is to run
    /// `Mux::reschedule` on the way out, and to take this pass's single
    /// frontend-RTT sample on the way in. `ready_inner` has a dozen `return`
    /// sites; arming the wheel at each of them by hand is precisely the
    /// discipline this refactor exists to remove.
    ///
    /// The sample is taken HERE and not inside `ready_inner`'s outer loop,
    /// where `Context::now` is refreshed. The clock is free, so sharing one
    /// instant per outer iteration costs nothing; the RTT is a
    /// `getsockopt(TCP_INFO)` syscall, and an outer loop that goes round
    /// several times would pay it several times per readiness sweep. One
    /// sweep, one syscall — see `Mux::refresh_client_rtt`.
    fn ready(
        &mut self,
        session: Rc<RefCell<dyn ProxySession>>,
        proxy: Rc<RefCell<dyn L7Proxy>>,
        metrics: &mut SessionMetrics,
    ) -> SessionResult {
        self.refresh_client_rtt();
        let result = self.ready_inner(session, proxy, metrics);
        self.apply_backend_deltas();
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

    /// Thin wrapper over `Mux::timeout_inner`: reschedule on the way out,
    /// then check the two properties carried over from `UdpManager`.
    fn timeout(&mut self, token: Token, metrics: &mut SessionMetrics) -> StateResult {
        let result = self.timeout_inner(token, metrics);
        self.apply_backend_deltas();
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
                for event in stream
                    .generate_access_log(
                        is_error,
                        Some("session close"),
                        self.context.listener.clone(),
                        client_rtt,
                        server_rtt,
                    )
                    .into_iter()
                    .flatten()
                {
                    crate::protocol::mux::h2::record_metric(event);
                }
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
            // already-queued response. Canonical write-up: `HttpsSession::close`
            // (`lib/src/https.rs`). Backend sockets follow the same discipline for symmetry.
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
                    // Session teardown iterates the backends map directly
                    // instead of routing through `Connection::close`, so it
                    // emits the same two accounting changes that path emits.
                    // Going through the ledger rather than mutating here is
                    // what lets the invariant-14 balance be read off one
                    // stream of deltas — see `Mux::apply_backend_deltas`.
                    let count = self
                        .context
                        .backend_streams
                        .get(token)
                        .map_or(0, |ids| ids.len());
                    let (slot, backend_id) = (backend.slot(), backend.backend_id.clone());
                    self.context.backend_deltas.push(BackendDelta {
                        slot,
                        change: BackendChange::StreamsEnded(count),
                    });
                    self.context.backend_deltas.push(BackendDelta {
                        slot,
                        change: BackendChange::ConnectionClosed,
                    });
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
                        Some(&backend_id)
                    );
                    trace!(
                        "{} connection (session) closed: {:?}",
                        log_context_lite!(self),
                        backend
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
        // Clear the reverse index after all backends have charged their
        // `StreamsEnded` deltas (whose counts come from that index).
        self.context.backend_streams.clear();

        // The session is going away, so this is the last chance to settle the
        // ledger against the worker's registry. Everything the frontend close
        // and the loop above decided is performed here.
        self.apply_backend_deltas();
    }

    /// Thin wrapper over `Mux::shutting_down_inner`: it drives frontend I/O
    /// and can change a core deadline, so it owes the wheel a reschedule on
    /// every exit.
    fn shutting_down(&mut self) -> SessionIsToBeClosed {
        let result = self.shutting_down_inner();
        self.apply_backend_deltas();
        self.reschedule();
        result
    }
}

/// The bodies the `SessionState` wrappers above drive. They are inherent
/// methods rather than trait methods so the wrappers can own the one thing
/// every exit path needs: the [`Mux::reschedule`] that pushes the cores'
/// deadlines onto the timer wheel.
impl<Front: SocketHandler + std::fmt::Debug, L: ListenerHandler + L7ListenerHandler> Mux<Front, L> {
    /// Fulfil a [`router::ConnectPlan::Dial`]: select a backend, dial it,
    /// build the connection, register it, and hand it to
    /// `Router::commit_dialed`.
    ///
    /// This is the embedder half of Question 6's split
    /// ([#1340](https://github.com/sozu-proxy/sozu/issues/1340)). Every step
    /// here reaches something the core has no business holding — the
    /// `BackendMap`, a `connect(2)`, `setsockopt`, the session slab, the mio
    /// registry — and the order is the one `Router::plan_connect` used, unchanged.
    ///
    /// SECURITY (CWE-400): every stateful side-effect (the
    /// `backend.connections` / `backend.pool.size` /
    /// `connections_per_backend` gauges, the slab session, the mio
    /// registration, the router-map entry, `stream.metrics.backend_start`) is
    /// deferred until AFTER both the `Connection` constructor and
    /// `start_stream` have succeeded. If either fails this returns `Err`
    /// without leaking a slab entry, an epoll registration, a gauge counter
    /// or a router-map entry. The `TcpStream` lives on the stack and is moved
    /// into the `Connection`; on failure the `Connection` — or the raw
    /// `TcpStream`, for the pool-exhaustion branch that drops inside
    /// `Connection::new_h2_client` — is dropped, closing the fd. No token is
    /// allocated before that point, so there is nothing to roll back.
    ///
    /// Taken as free functions over `&mut` fields rather than `&mut self` so
    /// the caller can keep its `&mut self.context` binding across the call.
    #[allow(clippy::too_many_arguments)]
    fn dial_backend(
        router: &mut Router,
        backend_registry: &mut BackendRegistry,
        stream_id: GlobalStreamId,
        context: &mut Context<L>,
        session: &Rc<RefCell<dyn ProxySession>>,
        proxy: &Rc<RefCell<dyn L7Proxy>>,
        cluster_id: &str,
        h2: bool,
        frontend_should_stick: bool,
    ) -> Result<(), BackendConnectionError> {
        let (socket, backend) = router.backend_from_request(
            cluster_id,
            frontend_should_stick,
            &mut context.streams[stream_id].context,
            proxy.clone(),
        )?;

        if let Err(e) = socket.set_nodelay(true) {
            error!(
                "{} error setting nodelay on back socket({:?}): {:?}",
                log_module_context!(context.http_context(stream_id)),
                socket,
                e
            );
        }

        // The one place a registry handle becomes an opaque id: the
        // embedder's table names it, copies the two identity fields the
        // datapath renders, and the `Rc` goes no further. Everything below
        // this line — and every `Position::Client` built from it — holds
        // `backend`, never the handle.
        let backend = backend_registry.id_for(&backend);

        // Cache the backend's configured address so SOCKET log lines fired on
        // ECONNREFUSED (or any failed async `connect()`) can still render
        // `peer=<backend>` — `getpeername(2)` returns ENOTCONN in that state,
        // so the live lookup path would show `peer=None` exactly when the
        // operator needs the backend id.
        let backend_peer = Some(backend.address);
        let socket = SessionTcpStream::new(socket, context.session_ulid, backend_peer);

        let flood_config = context.listener.borrow().get_h2_flood_config();
        let connection_config = context.listener.borrow().get_h2_connection_config();
        let stream_idle_timeout = context.listener.borrow().get_h2_stream_idle_timeout();
        let graceful_shutdown_deadline = context
            .listener
            .borrow()
            .get_h2_graceful_shutdown_deadline();
        let backend_id_for_gauge = backend.backend_id.to_string();
        let mut connection = if h2 {
            match Connection::new_h2_client(
                context.session_ulid,
                socket,
                cluster_id.to_owned(),
                backend,
                &mut *context.buffers,
                router.configured_connect_timeout,
                flood_config,
                connection_config,
                stream_idle_timeout,
                graceful_shutdown_deadline,
            ) {
                Some(connection) => connection,
                // Pool exhaustion: the socket was already dropped by
                // `new_h2_client` and no side-effect has been committed.
                None => return Err(BackendConnectionError::MaxBuffers),
            }
        } else {
            Connection::new_h1_client(
                context.session_ulid,
                socket,
                cluster_id.to_owned(),
                backend,
                router.configured_connect_timeout,
            )
        };

        // Check the backend can accept a new stream BEFORE committing any
        // registry state. `start_stream` records its `StreamsStarted` delta
        // only once the start has succeeded, so a refusal leaves the backend
        // accounting untouched rather than needing a rollback.
        if !connection.start_stream(stream_id, context) {
            error!(
                "{} Backend rejected stream start (max concurrent streams reached)",
                log_module_context!(context.http_context(stream_id))
            );
            // `connection` (socket + pending timeout deadline) drops here; no
            // wheel entry was ever armed for it.
            return Err(BackendConnectionError::MaxSessionsMemory);
        }

        // --- Happy path: commit side-effects in one atomic-ish block ---
        let stream = &mut context.streams[stream_id];
        stream.metrics.backend_start();
        stream.metrics.backend_id = stream.context.backend_id.to_owned();
        gauge_add!(names::backend::CONNECTIONS, 1);
        // `backend.pool.size` mirrors `backend.connections` exactly: one entry
        // per `Router::backends` token. The `-1` partners live in
        // `connection.rs::pre_close_client_bookkeeping` (graceful close) and
        // in `Mux::close` (session teardown). Symmetric pairing with both
        // decrement sites is the only defence against the gauge underflow
        // class of bug fixed by ff401b54 / aadb3fa4.
        gauge_add!(names::backend::POOL_SIZE, 1);
        gauge_add!(
            names::backend::CONNECTIONS_PER_BACKEND,
            1,
            Some(cluster_id),
            Some(&backend_id_for_gauge)
        );

        let token = proxy.borrow().add_session(session.clone());

        {
            let socket_ref = connection.socket_mut();
            if let Err(e) = proxy.borrow().register_socket(
                socket_ref,
                token,
                Interest::READABLE | Interest::WRITABLE,
            ) {
                // SECURITY (CWE-400): treat mio registration failure as a hard
                // connect failure. Without this rollback the gauges
                // (`backend.connections`, `backend.pool.size`,
                // `connections_per_backend`) and the slab session leak until
                // the connect timeout fires. Under fd pressure
                // (EMFILE/ENFILE) this can occur in tight bursts and poison
                // capacity dashboards.
                error!(
                    "{} error registering back socket: {:?} — rolling back",
                    log_module_context!(context.http_context(stream_id)),
                    e
                );
                // Undo the gauge increments committed above.
                gauge_add!(names::backend::CONNECTIONS, -1);
                gauge_add!(names::backend::POOL_SIZE, -1);
                gauge_add!(
                    names::backend::CONNECTIONS_PER_BACKEND,
                    -1,
                    Some(cluster_id),
                    Some(&backend_id_for_gauge)
                );
                // Release the `active_requests` charge `start_stream` took.
                //
                // The comment this replaces claimed the drop released it "via
                // the regular session drop path
                // (`pre_close_client_bookkeeping`)". That was never true:
                // `Connection` has no `impl Drop`, so dropping it runs no
                // bookkeeping at all. It was harmless only because the charge
                // is guarded on `BackendStatus::Connected` and a
                // freshly-dialled connection is `Connecting`, so there was
                // nothing to release. Emitting the release explicitly, under
                // the same guard the charge uses, makes the pair hold however
                // the status is reached rather than by accident.
                connection.release_start_stream_charge(context);
                proxy.borrow().remove_session(token);
                return Err(BackendConnectionError::MaxSessionsMemory);
            }
        }

        router.commit_dialed(stream_id, context, token, connection);
        Ok(())
    }

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
            self.context.now_wall_ms = unix_epoch_ms();
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

                                // Health and latency are not invariant 14 and
                                // the load balancer READS `retry_policy`, so
                                // these three writes stay immediate, against
                                // the handle the slot resolves to. Only the
                                // `active_requests` charge below is a delta.
                                let Some(handle) = self.backend_registry.get(backend.slot()) else {
                                    error!(
                                        "{} connected backend names a slot this session never                                          minted: {:?}",
                                        log_context_lite!(self),
                                        backend
                                    );
                                    continue;
                                };
                                let mut backend_borrow = handle.borrow_mut();
                                if backend_borrow.retry_policy.is_down() {
                                    info!(
                                        "{} backend server {} at {} is up",
                                        log_context_lite!(self),
                                        backend.backend_id,
                                        backend.address
                                    );
                                    incr!(
                                        names::backend::UP,
                                        Some(cluster_id),
                                        Some(&backend.backend_id)
                                    );
                                    gauge!(
                                        names::backend::AVAILABLE,
                                        1,
                                        Some(cluster_id),
                                        Some(&backend.backend_id)
                                    );
                                    push_event(Event {
                                        kind: EventKind::BackendUp as i32,
                                        backend_id: Some(backend.backend_id.to_string()),
                                        address: Some(backend.address.into()),
                                        cluster_id: Some(cluster_id.to_owned()),
                                        metric_detail: None,
                                    });
                                }

                                //successful connection, reset failure counter
                                backend_borrow.failures = 0;
                                backend_borrow.set_connection_time(start.elapsed());
                                backend_borrow.retry_policy.succeed();
                                drop(backend_borrow);

                                // These streams linked while the connection
                                // was still `Connecting`, so
                                // `Connection::start_stream`'s `Connected`
                                // guard skipped their charge. This is where
                                // every first stream of a fresh dial is
                                // charged — which is why the ledger cannot be
                                // core-only and still balance.
                                let mut started = 0;
                                if let Some(ids) = self.context.backend_streams.get(token) {
                                    started = ids.len();
                                    for &stream_id in ids {
                                        self.context.streams[stream_id].metrics.backend_connected();
                                    }
                                }
                                if started > 0 {
                                    let slot = backend.slot();
                                    self.context.backend_deltas.push(BackendDelta {
                                        slot,
                                        change: BackendChange::StreamsStarted(started),
                                    });
                                }
                                trace!(
                                    "{} connection success: {:?}",
                                    log_context_lite!(self),
                                    backend
                                );
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
                                // A dial that never completed charged nothing
                                // to `active_requests`, so this arm touches
                                // only the health counters — not the ledger.
                                let Some(handle) = self.backend_registry.get(backend.slot()) else {
                                    error!(
                                        "{} failed backend names a slot this session never                                          minted: {:?}",
                                        log_context_lite!(self),
                                        backend
                                    );
                                    continue;
                                };
                                let mut backend_borrow = handle.borrow_mut();
                                backend_borrow.failures += 1;

                                let already_unavailable = backend_borrow.retry_policy.is_down();
                                backend_borrow.retry_policy.fail();
                                incr!(
                                    names::backend::CONNECTIONS_ERROR,
                                    Some(cluster_id),
                                    Some(&backend.backend_id)
                                );
                                if !already_unavailable && backend_borrow.retry_policy.is_down() {
                                    error!(
                                        "{} backend server {} at {} is down",
                                        log_context_lite!(self),
                                        backend.backend_id,
                                        backend.address
                                    );
                                    incr!(
                                        names::backend::DOWN,
                                        Some(cluster_id),
                                        Some(&backend.backend_id)
                                    );
                                    gauge!(
                                        names::backend::AVAILABLE,
                                        0,
                                        Some(cluster_id),
                                        Some(&backend.backend_id)
                                    );
                                    push_event(Event {
                                        kind: EventKind::BackendDown as i32,
                                        backend_id: Some(backend.backend_id.to_string()),
                                        address: Some(backend.address.into()),
                                        cluster_id: Some(cluster_id.to_owned()),
                                        metric_detail: None,
                                    });
                                }
                                drop(backend_borrow);
                                trace!(
                                    "{} connection fail: {:?}",
                                    log_context_lite!(self),
                                    backend
                                );
                            }
                            Position::Client(_, backend, _) => {
                                // A backend that dies past `Connecting` still
                                // owes the charge every stream it carries
                                // took — the same `StreamsEnded(count)` the
                                // teardown path emits.
                                let count = self
                                    .context
                                    .backend_streams
                                    .get(token)
                                    .map_or(0, |ids| ids.len());
                                self.context.backend_deltas.push(BackendDelta {
                                    slot: backend.slot(),
                                    change: BackendChange::StreamsEnded(count),
                                });
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
                            // already-queued response. Canonical write-up: `HttpsSession::close`
                            // (`lib/src/https.rs`). Backend sockets follow the same discipline for symmetry.
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
                // Settle the ledger before the ONLY reader of backend load
                // state on this path runs: the load balancer reached from
                // `Mux::dial_backend` below weighs `active_requests` and
                // `connection_time`. Draining here is what keeps a second
                // stream linking in the same pass seeing the first stream's
                // charge, exactly as it did when the charge was applied in
                // place. It is also what makes the view each selection reads
                // current: `Backend::try_connect` increments
                // `active_connections` synchronously at the dial, inside this
                // same loop iteration, so the next iteration's selection sees
                // it (#1340, Question 6's second wrinkle).
                for delta in std::mem::take(&mut context.backend_deltas) {
                    self.backend_registry.apply(delta);
                }
                // Build the routing view once for this decision, from a single
                // `proxy.borrow()`. Holding one `Ref` for the call is what
                // gives every read inside it the same cluster map; the two
                // sites used to take separate borrows. A second immutable
                // borrow inside (`plan_connect`'s limit gate, and
                // `dial_backend`'s `add_session` / `register_socket`) is fine
                // — nothing on this path borrows the proxy mutably.
                let proxy_ref = proxy.borrow();
                let view = router::RoutingView::new(proxy_ref.clusters(), proxy_ref.kind());
                match self
                    .router
                    .plan_connect(stream_id, context, &view)
                    .and_then(|step| match step {
                        router::ConnectStep::Decided(plan) => Ok(plan),
                        // The core resolved a cluster and paused: the
                        // per-(cluster, source-IP) gate is a consult AND a
                        // mutation of worker-global session state, so the core
                        // cannot perform it. Answer it here, then resume.
                        //
                        // The track must land BEFORE the decision resumes, not
                        // after: `cluster_ip_at_limit` is read by every other
                        // session on this worker and by `lib/src/tcp.rs`'s own
                        // gate, so a stream admitted but not yet counted is a
                        // slot a concurrent stream can take twice.
                        router::ConnectStep::CheckIpLimit(resume) => {
                            let verdict = consult_ip_gate(
                                &proxy.borrow().sessions(),
                                self.frontend_token,
                                &resume,
                            );
                            self.router
                                .plan_connect_resume(stream_id, context, resume, verdict)
                        }
                    })
                    .and_then(|plan| match plan {
                        router::ConnectPlan::Attached => Ok(()),
                        router::ConnectPlan::Dial {
                            cluster_id,
                            h2,
                            frontend_should_stick,
                        } => Self::dial_backend(
                            &mut self.router,
                            &mut self.backend_registry,
                            stream_id,
                            context,
                            &session,
                            &proxy,
                            &cluster_id,
                            h2,
                            frontend_should_stick,
                        ),
                    }) {
                    Ok(_) => {
                        let state = context.streams[stream_id].state;
                        context.debug.push(DebugEvent::CC(stream_id, state));
                    }
                    Err(error) => {
                        trace!("{} Connection error: {}", log_module_context!(), error);
                        let stream = &mut context.streams[stream_id];
                        // The registry this stream captured when its request
                        // arrived, not whatever the listener holds now.
                        let answers_rc = stream.answers.clone();
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
                            // value is computed in `Router::plan_connect` (where the
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
                            // A stale-upstream replay whose preconditions no
                            // longer hold (sozu-proxy/sozu#1442). 502 is the
                            // answer this stream received before the replay
                            // existed, so refusing costs the client nothing it
                            // would not already have paid — and costs it far
                            // less than H1 wire bytes framed as an HTTP/2 DATA
                            // payload, or a second partial copy of the request
                            // trailing the first.
                            BE::ReplayRefused(reason) => {
                                warn!(
                                    "{} stale-upstream replay refused: {}",
                                    log_module_context!(stream.context),
                                    reason
                                );
                                set_default_answer(stream, front_readiness, 502, &answers);
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
        self.context.now_wall_ms = unix_epoch_ms();
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
        // Deliberately BELOW the early-delivery gate, unlike the clock above
        // it: `consume_timer_entry` reads `context.now` to decide, so the
        // clock has to be fresh before it, while the RTT is only wanted by
        // the access logs `cancel_timed_out_streams` emits further down. The
        // wheel rounds to the nearest tick and re-validates, so a firing that
        // turns out to be early is common (§7.6 of `LIFECYCLE.md`) — sampling
        // above the gate would spend a `getsockopt(TCP_INFO)` on every one of
        // those do-nothing passes, which is the cost this whole change is
        // accounted in. A silent peer is what brings us here and is exactly
        // when the last `ready()` sample is oldest, so the sample itself is
        // load-bearing; its position is not symmetry with the clock.
        self.refresh_client_rtt();
        let front_is_h2 = match self.frontend {
            Connection::H1(_) => false,
            Connection::H2(_) => true,
        };
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
            // observe the `H2ControlTx` queue (it gates connection close, so a
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
                if h2.core.has_pending_control_write() {
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
                            // Each stream answers out of the registry it
                            // captured when its request arrived; a listener
                            // reload since then belongs to the next one.
                            let answers_rc = self.context.streams[stream_id].answers.clone();
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
                        let answers_rc = self.context.streams[stream_id].answers.clone();
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
                            let answers_rc = self.context.streams[stream_id].answers.clone();
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
                    let answers_rc = self.context.streams[stream_id].answers.clone();
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
            // `ConnectionH2::{handle_write,handle_headers_frame}` pushes that
            // deadline out on the first pass that moves a stream's bytes — not
            // on every pass, LIFECYCLE §9 invariant 9 — but nothing hangs on
            // it.) That is what covers the pool-reuse branch of
            // `Router::plan_connect` never arming a timeout, unlike the fresh-dial
            // branch.
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
        self.context.now_wall_ms = unix_epoch_ms();
        // Same reasoning for the frontend RTT: `drive_frontend_shutdown_io`
        // below reaches `readable()` and `writable()`, so every
        // `ConnectionH2::snapshot_rtts` site is live on this path and a
        // draining session would otherwise log the sample of its last
        // `ready()` pass for the whole drain.
        self.refresh_client_rtt();
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
        can_stop
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
        sticky_name: String,
        elide_x_real_ip: bool,
    }

    pub(crate) const TEST_STICKY_NAME: &str = "SOZUBALANCEID";

    impl TestListener {
        pub(crate) fn new() -> Self {
            Self {
                address: "127.0.0.1:1".parse().expect("test address must parse"),
                answers: test_answers(),
                sticky_name: TEST_STICKY_NAME.to_owned(),
                elide_x_real_ip: false,
            }
        }

        /// Stand in for an operator listener reload: change the per-request
        /// knobs and publish a brand-new answer registry under a new `Rc`,
        /// exactly as `HttpListener::update_config` does. Returns the newly
        /// published registry.
        ///
        /// The registry must be *published*, not edited: a test that mutated
        /// the existing one in place would prove nothing about the per-stream
        /// capture, because both streams would still be holding the same
        /// object.
        pub(crate) fn reload(
            &mut self,
            sticky_name: &str,
            elide_x_real_ip: bool,
        ) -> Rc<RefCell<HttpAnswers>> {
            self.sticky_name = sticky_name.to_owned();
            self.elide_x_real_ip = elide_x_real_ip;
            self.answers = test_answers();
            self.answers.clone()
        }
    }

    /// A fresh, empty answer registry — what a test `Stream` renders from
    /// when the test is not about templates.
    pub(crate) fn test_answers() -> Rc<RefCell<HttpAnswers>> {
        Rc::new(RefCell::new(
            HttpAnswers::new(&BTreeMap::new()).expect("default answers must build"),
        ))
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
            &self.sticky_name
        }
        fn get_elide_x_real_ip(&self) -> bool {
            self.elide_x_real_ip
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
    use super::test_support::{TEST_STICKY_NAME, connected_socket, test_context};
    use super::*;
    use crate::{
        pool::Pool,
        protocol::http::parser::Method,
        protocol::mux::{
            h2::{H2ConnectionConfig, H2State},
            h2_flood_detector::H2FloodConfig,
        },
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

        let mut h2 = h2::H2Shell::new(
            Ulid::generate(),
            socket,
            Position::Server,
            &mut PoolBufferSource::new(Rc::downgrade(&pool)),
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
        h2.core.state = H2State::Header;
        h2.core.drain.__test_arm_draining(armed_at);
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
        h2.core.__test_insert_wire_mapping_only(1, stream_id);

        let mut mux = Mux {
            configured_frontend_timeout: Duration::from_secs(30),
            frontend_token: Token(0),
            frontend,
            router: Router::new(Duration::from_secs(30), Duration::from_secs(30)),
            context,
            session_ulid: Ulid::generate(),
            timeouts: HashMap::new(),
            backend_registry: BackendRegistry::default(),
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
            backend_registry: BackendRegistry::default(),
        };
        (mux, peer)
    }

    /// Stage the exact shape of sozu-proxy/sozu#1442: a request fully written
    /// onto a REUSED keep-alive upstream that then closed without answering.
    ///
    /// `front.consumed` is what `end_stream_decision` reads to mean "the
    /// request left the front buffer"; the armed `retry_buffer` is what
    /// `ConnectionH1::writable` fills, and only ever on a connection whose
    /// `reused_from_pool` is set.
    fn stale_pooled_upstream_stream(
        pool: &Rc<RefCell<Pool>>,
        method: Method,
    ) -> (
        Mux<mio::net::TcpStream, test_support::TestListener>,
        std::net::TcpStream,
    ) {
        let (mut mux, peer) = h1_mux_with_idle_stream(pool, Duration::from_secs(60));
        let stream = &mut mux.context.streams[0];
        // The capture is armed while the method is still `Get`, and the method
        // under test is set afterwards. Since sozu-proxy/sozu#1450
        // `arm_upstream_replay` refuses to capture a non-idempotent request at
        // all, so arming under `method` would leave the POST and `PATCH` cases
        // below with no capture — and they would then be observing the ARM
        // guard rather than the conjunct they exist to pin. Staging it this way
        // keeps `Stream::can_replay_on_fresh_upstream`'s own idempotence
        // conjunct as the thing under test: dropping that conjunct still
        // reddens them. Production cannot reach this state any more, because a
        // request's method does not change after its headers are parsed, which
        // is exactly why the conjunct is now defense in depth and why these
        // tests are what keep it honest.
        stream.context.method = Some(Method::Get);
        stream.arm_upstream_replay();
        stream
            .retry_buffer
            .as_mut()
            .expect("arm_upstream_replay must install a buffer")
            .extend_from_slice(b"GET /api HTTP/1.1\r\nHost: localhost\r\n\r\n");
        stream.context.method = Some(method);
        stream.front.consumed = true;
        // The upstream produced nothing: not in body phase, nothing forwarded,
        // nothing buffered.
        assert!(
            !stream.back.is_main_phase() && !stream.back.consumed && stream.back.storage.is_empty(),
            "precondition: the upstream must have produced no response at all"
        );
        (mux, peer)
    }

    /// Concatenate every byte `ConnectionH1::writable` would hand the socket,
    /// in queue order. Mirrors its own `kawa.out` walk.
    fn queued_request_bytes(front: &GenericHttpStream) -> Vec<u8> {
        let buffer = front.storage.buffer();
        front
            .out
            .iter()
            .map(|block| match block {
                kawa::OutBlock::Store(store) => store.data(buffer).to_vec(),
                kawa::OutBlock::Delimiter => Vec::new(),
            })
            .collect::<Vec<_>>()
            .concat()
    }

    /// Stage the partial-write shape of sozu-proxy/sozu#1442 on the front
    /// kawa: a real request, parsed and serialized exactly as
    /// `ConnectionH1::writable` serializes it, of which the socket accepted
    /// everything but the last `unwritten_tail` bytes.
    ///
    /// `kawa::Kawa::consume` pushes the partially consumed store back to the
    /// FRONT of `out` (kawa-0.7.1 `storage/repr.rs`), so the refused tail
    /// stays queued ahead of anything appended afterwards. That is what makes
    /// the queue position of the replay load-bearing.
    ///
    /// Returns the complete serialization, which a correct replay reproduces
    /// byte-for-byte.
    fn stage_partially_written_pooled_request(
        stream: &mut Stream,
        unwritten_tail: usize,
    ) -> Vec<u8> {
        let request: &[u8] = b"GET /api HTTP/1.1\r\nHost: localhost\r\nX-Tail: unwritten\r\n\r\n";
        stream.front.storage.space()[..request.len()].copy_from_slice(request);
        stream.front.storage.fill(request.len());
        kawa::h1::parse(&mut stream.front, &mut stream.context);
        assert!(
            stream.front.is_main_phase(),
            "the staged request must parse"
        );
        stream.front.prepare(&mut kawa::h1::BlockConverter);
        assert!(
            stream.front.blocks.is_empty(),
            "prepare must drain blocks, as it has by the time writable writes"
        );

        let serialized = queued_request_bytes(&stream.front);
        let accepted = serialized
            .len()
            .checked_sub(unwritten_tail)
            .expect("the unwritten tail must fit inside the serialized request");
        stream.context.method = Some(Method::Get);
        // Through `arm_upstream_replay` rather than by assigning the field:
        // the capture owns a charge against `MAX_ARMED_REPLAY_CAPTURES`, so a
        // hand-built one would release a charge it never took.
        stream.arm_upstream_replay();
        stream
            .retry_buffer
            .as_mut()
            .expect("arm_upstream_replay must install a buffer")
            .extend_from_slice(&serialized[..accepted]);
        stream.front.consume(accepted);
        stream.front.consumed = true;
        serialized
    }

    /// The replay must be queued AHEAD of the bytes the kernel refused.
    ///
    /// `ConnectionH1::writable` handles a partial `socket_write_vectored` by
    /// calling `signal_pending_write()` and returning `MuxResult::Continue`,
    /// which leaves the unwritten remainder queued: `kawa::Kawa::consume`
    /// pushes the partially consumed store back to the FRONT of `out`
    /// (kawa-0.7.1 `storage/repr.rs`). `Kawa::push_out` APPENDS, so queueing
    /// the capture with it would hand the healthy backend `[tail][head]` — a
    /// request mangled mid-token, on the wire, to a backend that never saw
    /// the first attempt.
    ///
    /// Reachable on an idempotent request large enough that the kernel
    /// accepts only part of one write to a pooled upstream, followed by that
    /// stale peer EOFing without answering.
    ///
    /// The remainder spans zero, one, three and thirteen `out` entries across
    /// the cases below: `consume` pushes back at most ONE partially consumed
    /// store and drops every entry before it, so how many survive depends on
    /// where the split falls. Prepending is correct for all of them because
    /// `VecDeque::push_front` preserves the relative order of what is already
    /// queued, and the capture is exactly the complementary prefix.
    ///
    /// To SEE THIS RED: in `Stream::queue_upstream_replay`, swap the
    /// `front.out.push_front(...)` back for `self.front.push_out(...)`.
    #[test]
    fn a_replay_is_queued_ahead_of_the_unwritten_tail() {
        for (unwritten_tail, queued_entries) in [(0, 0), (2, 1), (21, 3), (100, 13)] {
            let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 8, 16384)));
            let (mut mux, _peer) = h1_mux_with_idle_stream(&pool, Duration::from_secs(60));
            let stream = &mut mux.context.streams[0];
            let serialized = stage_partially_written_pooled_request(stream, unwritten_tail);
            assert_eq!(
                stream.front.out.len(),
                queued_entries,
                "staging a {unwritten_tail}-byte tail must leave {queued_entries} queued entries"
            );
            assert_eq!(
                shared::end_stream_decision(stream),
                shared::EndStreamAction::ReplayOnFreshBackend,
                "a partially written request on a stale pooled upstream is still replayable"
            );

            stream
                .queue_upstream_replay()
                .expect("the staged stream carries a capture");

            assert_eq!(
                queued_request_bytes(&stream.front),
                serialized,
                "with a {unwritten_tail}-byte unwritten tail the fresh backend must \
                 receive the request in its original order, not the tail first"
            );
        }
    }

    /// The defect of sozu-proxy/sozu#1442. A request written onto a pooled
    /// keep-alive upstream that the peer had already closed must be replayed
    /// on a fresh backend, not answered `502 Bad Gateway`: nothing was
    /// observed by the client, so re-issuing it is unobservable. Measured
    /// 2026-09-22 with only the replay decision reverted: 25 failures over
    /// 576 pooled `test_issue_806` trials (4.3% per trial), 25 red runs of
    /// 25; green on 25 of 25 with the replay in place.
    ///
    /// To SEE THIS RED: in `super::shared::end_stream_decision`, replace the
    /// `can_replay_on_fresh_upstream()` branch with a bare
    /// `EndStreamAction::SendDefault(502)` — the pre-fix shape.
    #[test]
    fn a_stale_pooled_upstream_that_never_answered_is_replayed_not_502() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 8, 16384)));
        let (mux, _peer) = stale_pooled_upstream_stream(&pool, Method::Get);
        assert_eq!(
            shared::end_stream_decision(&mux.context.streams[0]),
            shared::EndStreamAction::ReplayOnFreshBackend,
        );
    }

    /// The method veto. nginx keeps `non_idempotent` out of the
    /// `proxy_next_upstream` default so `POST, LOCK, PATCH` are "not passed
    /// to the next server if a request has been sent to an upstream server";
    /// pingora's default `error_while_proxy` calls `set_retry(false)` on
    /// `!method.is_idempotent()`. Replaying a POST can duplicate a write the
    /// origin may already have committed.
    ///
    /// To SEE THIS RED: drop the `Method::is_idempotent` conjunct from
    /// `Stream::can_replay_on_fresh_upstream`.
    #[test]
    fn a_non_idempotent_request_is_never_replayed_on_a_stale_upstream() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 8, 16384)));
        let (mux, _peer) = stale_pooled_upstream_stream(&pool, Method::Post);
        assert_eq!(
            shared::end_stream_decision(&mux.context.streams[0]),
            shared::EndStreamAction::SendDefault(502),
        );
    }

    /// An extension method sozu does not know the semantics of is treated
    /// like POST. nginx's list is a closed `POST, LOCK, PATCH`; sozu parses
    /// `PATCH` as `Method::Custom`, so refusing every `Custom` is what makes
    /// PATCH non-replayable here without maintaining a second list.
    #[test]
    fn an_unknown_method_is_never_replayed_on_a_stale_upstream() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 8, 16384)));
        let (mux, _peer) = stale_pooled_upstream_stream(&pool, Method::new(b"PATCH"));
        assert!(matches!(
            mux.context.streams[0].context.method,
            Some(Method::Custom(_))
        ));
        assert_eq!(
            shared::end_stream_decision(&mux.context.streams[0]),
            shared::EndStreamAction::SendDefault(502),
        );
    }

    /// pingora's `RetryType::ReusedOnly`. The same EOF means "the pool handed
    /// out a socket the peer had already closed" on a reused connection and
    /// "the origin accepted the request and then died" on a fresh dial; only
    /// the first is replayable. A fresh dial never arms the capture, so the
    /// absent buffer IS the provenance check.
    ///
    /// To SEE THIS RED: drop the `retry_buffer.is_some()` conjunct from
    /// `Stream::can_replay_on_fresh_upstream` — the provenance check itself.
    /// NOT `ConnectionH1::writable`'s `reused_from_pool` guard: this test
    /// never calls `writable`. It stages the capture through
    /// `stale_pooled_upstream_stream` and clears it with
    /// `Stream::forget_upstream_replay`, so a recipe naming the write path
    /// cannot redden it however plausible it reads.
    #[test]
    fn a_request_written_to_a_freshly_dialled_upstream_is_not_replayed() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 8, 16384)));
        let (mut mux, _peer) = stale_pooled_upstream_stream(&pool, Method::Get);
        mux.context.streams[0].forget_upstream_replay();
        assert_eq!(
            shared::end_stream_decision(&mux.context.streams[0]),
            shared::EndStreamAction::SendDefault(502),
        );
    }

    /// The boundary itself: once a response byte exists the request is no
    /// longer replayable. nginx — "passing a request to the next server is
    /// only possible if nothing has been sent to a client yet". A byte merely
    /// BUFFERED is already refused here, which is stricter: a partial status
    /// line is HAProxy's `junk-response`, absent from every default.
    ///
    /// To SEE THIS RED: drop the `back.storage.is_empty()` conjunct from
    /// `Stream::can_replay_on_fresh_upstream`.
    #[test]
    fn a_buffered_response_byte_forecloses_the_replay() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 8, 16384)));
        let (mut mux, _peer) = stale_pooled_upstream_stream(&pool, Method::Get);
        let back = &mut mux.context.streams[0].back;
        back.storage.space()[..4].copy_from_slice(b"HTTP");
        back.storage.fill(4);
        assert!(!back.is_main_phase(), "a bare `HTTP` is not yet a response");
        assert_eq!(
            shared::end_stream_decision(&mux.context.streams[0]),
            shared::EndStreamAction::SendDefault(502),
        );
    }

    /// A response byte already FORWARDED to the client is the same refusal,
    /// reached through the other conjunct.
    ///
    /// To SEE THIS RED: drop the `!back.consumed` conjunct from
    /// `Stream::can_replay_on_fresh_upstream`.
    #[test]
    fn a_forwarded_response_byte_forecloses_the_replay() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 8, 16384)));
        let (mut mux, _peer) = stale_pooled_upstream_stream(&pool, Method::Get);
        mux.context.streams[0].back.consumed = true;
        assert_eq!(
            shared::end_stream_decision(&mux.context.streams[0]),
            shared::EndStreamAction::SendDefault(502),
        );
    }

    /// The replay re-serializes the captured bytes and nothing else: they go
    /// back into `front.out` as an owned store, `front.blocks` stays empty so
    /// `kawa::Kawa::prepare` contributes nothing, and the buffer is taken so
    /// one capture cannot be replayed twice. That is not a per-REQUEST bound:
    /// `ConnectionH1::start_stream` arms a fresh capture on the next pooled
    /// attempt, and `CONN_RETRIES` is what bounds the request as a whole.
    ///
    /// To SEE THIS RED: make `Stream::queue_upstream_replay` clone the buffer
    /// instead of taking it.
    #[test]
    fn queueing_a_replay_moves_the_captured_bytes_into_the_front_buffer() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 8, 16384)));
        let (mut mux, _peer) = stale_pooled_upstream_stream(&pool, Method::Get);
        let stream = &mut mux.context.streams[0];
        let captured = stream
            .retry_buffer
            .as_ref()
            .expect("the staged stream carries a capture")
            .to_vec();

        assert_eq!(stream.queue_upstream_replay(), Some(captured.len()));

        assert!(
            stream.retry_buffer.is_none(),
            "the capture must be taken, not cloned: it cannot outlive its replay"
        );
        assert!(
            stream.front.blocks.is_empty(),
            "prepare must have nothing to convert, or it would prepend a \
             second copy of the request line"
        );
        let buffer = stream.front.storage.buffer();
        let queued: Vec<u8> = stream
            .front
            .out
            .iter()
            .map(|block| match block {
                kawa::OutBlock::Store(store) => store.data(buffer).to_vec(),
                kawa::OutBlock::Delimiter => Vec::new(),
            })
            .collect::<Vec<_>>()
            .concat();
        assert_eq!(
            queued, captured,
            "the replayed request must be byte-identical to the first attempt"
        );
        assert_eq!(
            shared::end_stream_decision(stream),
            shared::EndStreamAction::SendDefault(502),
            "with the capture spent and nothing re-arming it, a second stale \
             upstream falls back to 502"
        );
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
            test_backend_connection(&mut mux, Duration::from_secs(30));
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
        let (connection, _backend_peer) =
            test_backend_connection(&mut mux, Duration::from_secs(30));
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

    // ── Endpoint::peer_rtt is keyed by token ────────────────────────────
    //
    // `EndpointClient` keys backends by token; `EndpointServer` has a single
    // frontend and ignores the token. The pair matters because the two sides
    // populate DIFFERENT access-log cells — `snapshot_rtts` reads the local
    // socket for `client_rtt` and this trait method for `server_rtt` — so a
    // lookup that returned the wrong connection's RTT would mislabel the
    // value rather than lose it, which no downstream assertion would catch.

    /// An unknown token yields `None`, not some other backend's RTT.
    ///
    /// The premise is asserted first: on a live loopback socket
    /// `getsockopt(TCP_INFO)` succeeds, so a KNOWN token really does return
    /// `Some`. Without that half, this test would pass against a
    /// `peer_rtt` that always answered `None`.
    ///
    /// TO SEE THIS RED: in `EndpointClient::peer_rtt` (`connection.rs`),
    /// replace `.get(&token)` with `.values().next()`. The unknown token then
    /// resolves to the only backend in the map and the final assertion fails
    /// with `an unknown token must not resolve to another backend's RTT`.
    #[test]
    fn endpoint_client_peer_rtt_is_keyed_by_token() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 4, 16384)));
        let (mut mux, _frontend_peer) = h1_mux_with_idle_stream(&pool, Duration::from_secs(30));
        let backend_token = Token(1);
        let (connection, _backend_peer) =
            test_backend_connection(&mut mux, Duration::from_secs(30));
        mux.router.backends.insert(backend_token, connection);

        let endpoint = EndpointClient(&mut mux.router);

        assert!(
            endpoint.peer_rtt(backend_token).is_some(),
            "premise: TCP_INFO answers on a live loopback backend, so a known \
             token returns Some — otherwise the assertion below proves nothing"
        );
        assert!(
            endpoint.peer_rtt(Token(99)).is_none(),
            "an unknown token must not resolve to another backend's RTT"
        );
    }

    /// `EndpointServer` ignores the token: it holds one frontend, and every
    /// token must report that frontend's RTT rather than `None`.
    ///
    /// TO SEE THIS RED: in `EndpointServer::peer_rtt` (`connection.rs`),
    /// return `None` instead of `socket_rtt(self.0.socket())`. It fails on
    /// the first loop iteration with `EndpointServer must report its single
    /// frontend's RTT for any token` — the second never runs, which is why
    /// the loop is two tokens rather than an assertion about "any".
    #[test]
    fn endpoint_server_peer_rtt_ignores_the_token() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 4, 16384)));
        let (mut mux, _frontend_peer) = h1_mux_with_idle_stream(&pool, Duration::from_secs(30));

        let endpoint = EndpointServer(&mut mux.frontend);

        for token in [Token(0), Token(7)] {
            assert!(
                endpoint.peer_rtt(token).is_some(),
                "EndpointServer must report its single frontend's RTT for any token"
            );
        }
    }

    /// A `Mux` pass replaces the carried frontend-RTT sample, so a stream
    /// finishing in a LATER pass reports a freshly measured value.
    ///
    /// The other half of `snapshot_rtts_reports_the_carried_pass_sample_to_every_stream`
    /// (`h2.rs`). That one pins "one value for every stream in a pass"; this
    /// one pins "a new value on the next pass". Neither is the contract
    /// alone — a value that was carried but never refreshed would satisfy the
    /// first while freezing the access log's `client_rtt` at the connection's
    /// very first sample forever.
    ///
    /// Entered through [`SessionState::timeout`] rather than
    /// [`SessionState::ready`]. Both call the same
    /// [`Mux::refresh_client_rtt`], which exists as one function so the three
    /// entry points cannot drift apart, but `ready` takes
    /// `Rc<RefCell<dyn ProxySession>>` and `Rc<RefCell<dyn L7Proxy>>` and this
    /// crate has no test implementation of either — `L7Proxy::sessions` alone
    /// would require a live `SessionManager`. `timeout` needs no mock and is
    /// not a lesser path: it is where `cancel_timed_out_streams` reaps a
    /// silent peer's streams and emits their access logs, which is exactly
    /// when the previous pass's sample is oldest.
    ///
    /// TO SEE THIS RED: delete the `self.refresh_client_rtt();` line from
    /// [`Mux::timeout_inner`]. The frontend then still carries the sentinel
    /// and the final assertion fails with `a Mux pass must replace the
    /// carried frontend RTT with a fresh sample, got Some(4321s)`.
    #[test]
    fn a_mux_pass_refreshes_the_carried_client_rtt() {
        /// A value no loopback SRTT can take, standing for the sample an
        /// earlier pass left behind.
        const STALE_SAMPLE: Duration = Duration::from_secs(4321);

        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 4, 16384)));
        let (socket, _peer) = connected_socket();
        let h2 = h2::H2Shell::new(
            Ulid::generate(),
            socket,
            Position::Server,
            &mut PoolBufferSource::new(Rc::downgrade(&pool)),
            H2FloodConfig::default(),
            H2ConnectionConfig::default(),
            Duration::from_secs(30),
            None,
            Duration::from_secs(30),
            None,
            Ready::READABLE | Ready::HUP | Ready::ERROR,
        )
        .expect("a pool with free buffers must yield an H2 connection");

        let mut frontend = Connection::H2(h2);
        let Connection::H2(h2) = &mut frontend else {
            unreachable!("frontend was built as H2")
        };
        h2.core.client_rtt = Some(STALE_SAMPLE);
        // A deadline already in the past, so `Mux::consume_timer_entry`
        // accepts the firing as a real expiry and the body below it runs.
        // Without this the connection carries the 30 s deadline it armed at
        // construction, the firing re-validates as an early wheel delivery,
        // and `timeout_inner` returns before reaching anything this test is
        // about.
        h2.core.timeout_deadline = Some(Instant::now() - Duration::from_secs(1));

        let mut mux = Mux {
            configured_frontend_timeout: Duration::from_secs(30),
            frontend_token: Token(0),
            frontend,
            router: Router::new(Duration::from_secs(30), Duration::from_secs(30)),
            context: test_context(&pool),
            session_ulid: Ulid::generate(),
            timeouts: HashMap::new(),
            backend_registry: BackendRegistry::default(),
        };

        let mut metrics = SessionMetrics::new(None);
        let _ = mux.timeout(Token(0), &mut metrics);

        let Connection::H2(h2) = &mux.frontend else {
            unreachable!("frontend was built as H2")
        };
        assert!(
            h2.core.client_rtt.is_some(),
            "premise: TCP_INFO must answer on a live loopback frontend, \
             otherwise this test cannot tell a refresh from a failed read"
        );
        assert!(
            h2.core.client_rtt.is_some_and(|rtt| rtt != STALE_SAMPLE),
            "a Mux pass must replace the carried frontend RTT with a fresh \
             sample, got {:?}",
            h2.core.client_rtt
        );
    }

    /// An H1 backend connection on a live loopback socket, plus the peer the
    /// caller must keep alive.
    fn test_backend_connection(
        mux: &mut Mux<mio::net::TcpStream, test_support::TestListener>,
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
        // Through the embedder's table, exactly as a real dial does: the
        // handle stops here and the connection carries the opaque id.
        let backend = mux.backend_registry.id_for(&backend);
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
            test_backend_connection(&mut mux, Duration::from_secs(30));
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
            test_backend_connection(&mut mux, Duration::from_secs(30));
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

    /// Q7 of sozu-proxy/sozu#1340: a stream runs on the listener
    /// configuration that was in force when its request arrived, and the next
    /// request on the same connection picks up a reload that landed in
    /// between. The staleness window is therefore exactly one request, and
    /// this test is where that window is pinned — see `LIFECYCLE.md` §2.5.
    ///
    /// Both halves matter and they fail for opposite reasons:
    ///
    /// - freezing the listener at [`Context::new`] would satisfy the first
    ///   half and break the second — the reload would never reach the
    ///   connection at all, which is the construction-time snapshot the
    ///   maintainer rejected because hot reconfiguration is the point of this
    ///   proxy;
    /// - borrowing the listener live on the datapath satisfies the second and
    ///   breaks the first — a request already in flight would answer under a
    ///   cookie name, an `X-Real-IP` policy or an error page installed after
    ///   it started.
    ///
    /// To SEE THIS RED: give `Context` back an `elide_x_real_ip: bool` field
    /// set in [`Context::new`] from `listener.borrow().get_elide_x_real_ip()`,
    /// and make [`Context::create_stream`] pass `self.elide_x_real_ip` to
    /// `HttpContext::new` instead of `listener.get_elide_x_real_ip()`. The
    /// second stream then still carries the pre-reload value.
    #[test]
    fn a_listener_reload_between_two_streams_moves_only_the_second() {
        setup_test_logger!();
        let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 10, 16_384)));
        let mut context = test_context(&pool);

        let before = context.listener.borrow().get_answers().clone();
        let first = context
            .create_stream(Ulid::generate(), 1 << 16)
            .expect("the first stream must check out its buffers");

        // The operator reloads the listener while the first request is still
        // in flight. `reload` publishes a new answer registry under a new
        // `Rc`, exactly as `HttpListener::update_config` does.
        let after = context.listener.borrow_mut().reload("RELOADEDID", true);
        assert!(
            !Rc::ptr_eq(&before, &after),
            "precondition: the reload must PUBLISH a registry, not rewrite the \
             captured one — otherwise this test cannot tell a snapshot from a \
             shared handle"
        );

        let second = context
            .create_stream(Ulid::generate(), 1 << 16)
            .expect("the second stream must check out its buffers");
        assert_ne!(
            first, second,
            "precondition: the second request must land in its own slot, not \
             recycle the first one's"
        );

        // The next request picks the reload up.
        assert_eq!(
            context.streams[second].context.sticky_name, "RELOADEDID",
            "a request arriving after the reload must use the new sticky name"
        );
        assert!(
            context.streams[second].context.elide_x_real_ip,
            "a request arriving after the reload must use the new X-Real-IP \
             policy: a listener frozen at Context::new never sees a reload on \
             a connection that is already open"
        );
        assert!(
            Rc::ptr_eq(&context.streams[second].answers, &after),
            "a request arriving after the reload must render from the newly \
             published answer registry"
        );

        // The request already in flight does not.
        assert_eq!(
            context.streams[first].context.sticky_name, TEST_STICKY_NAME,
            "a request in flight keeps the sticky name it started under"
        );
        assert!(
            !context.streams[first].context.elide_x_real_ip,
            "a request in flight keeps the X-Real-IP policy it started under"
        );
        assert!(
            Rc::ptr_eq(&context.streams[first].answers, &before),
            "a request in flight keeps the answer registry it captured: a live \
             listener borrow on the write, reset or access-log path would hand \
             it a page installed after it started"
        );
    }

    // ── peer-address snapshot in the MUX log envelope ────────────────────
    //
    // `log_context!` / `log_context_lite!` render `peer=` from
    // `Connection::peer_address` (`lib/src/protocol/mux/connection.rs`), the
    // snapshot the connection took at construction — not a live
    // `getpeername(2)`. Both tests below make the two answers differ on
    // purpose: the socket is genuinely connected to a loopback listener, so a
    // live lookup succeeds and would print `127.0.0.1:<port>`; only a macro
    // reading the snapshot can print `10.0.0.42:12345`.
    //
    // That gap is the PROXY-protocol symptom in miniature — the frontend's real
    // peer is the load balancer while the snapshot holds the advertised client
    // — and the same read is what keeps the slot populated after the peer's
    // RST, when `getpeername(2)` answers ENOTCONN and a live lookup collapses
    // to `None` on exactly the error lines an operator is reading. Until this
    // change the `MUX` envelope printed one peer while the `MUX-H1`, `MUX-H2`,
    // `SOCKET` and `HTTPS` lines of the same ULID printed another.
    //
    // Mirrors `h2::tests::log_context_renders_the_cached_peer_not_a_live_lookup`
    // and `h1::tests::log_context_renders_the_proxy_advertised_peer_not_the_load_balancer`,
    // which pin the same slot one layer down in the per-protocol envelopes.
    //
    // A counting test would not discriminate here, unlike
    // `h1::tests::log_context_reads_the_peer_address_once_per_connection`:
    // before this change the macros never reached `SocketHandler::peer_addr` at
    // all — they went through `Connection::socket()` to mio's inherent method —
    // so a trait-call counter reads zero on both sides. The assertions are
    // therefore on the rendered VALUE.

    /// A live, established loopback connection plus the address
    /// `getpeername(2)` reports for it. The listener is returned so the
    /// connection stays up for the whole test: these tests assert the snapshot
    /// wins even while the live lookup is perfectly healthy, so a half-dead
    /// socket would weaken them rather than strengthen them.
    ///
    /// Connects with a blocking `std` socket and converts afterwards rather
    /// than reusing `connected_socket`, because `mio::net::TcpStream::connect`
    /// returns before the handshake completes and the live lookup below is a
    /// load-bearing premise, not a convenience.
    fn connected_loopback_stream() -> (std::net::TcpListener, TcpStream, SocketAddr) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("test listener must bind to a loopback port");
        let live_peer = listener
            .local_addr()
            .expect("test listener must report its local address");
        let stream =
            std::net::TcpStream::connect(live_peer).expect("loopback connect must complete");
        stream
            .set_nonblocking(true)
            .expect("mio requires a nonblocking stream");
        (listener, TcpStream::from_std(stream), live_peer)
    }

    /// Address the frontend handler snapshots. Deliberately non-loopback so it
    /// cannot collide with whatever ephemeral port the live lookup reports.
    const SNAPSHOT_PEER: &str = "10.0.0.42:12345";

    /// An H1 frontend `Mux` whose connection snapshotted [`SNAPSHOT_PEER`]
    /// while its socket is really connected to `live_peer`.
    /// `Connection::new_h1_server` takes that snapshot itself, from the
    /// handler's `SocketHandler::peer_addr`, so nothing here writes the field
    /// behind the production path's back.
    ///
    /// An H1 frontend only because it is the cheaper of the two to build: the
    /// accessor under test resolves both `Connection` arms to the same
    /// `peer_address` field through `forward!`, and the two macros are defined
    /// once for every frontend.
    fn mux_with_snapshotted_peer(
        pool: &Rc<RefCell<Pool>>,
        stream: TcpStream,
    ) -> Mux<SessionTcpStream, test_support::TestListener> {
        let session_ulid = Ulid::generate();
        let socket = SessionTcpStream::new(
            stream,
            session_ulid,
            Some(
                SNAPSHOT_PEER
                    .parse()
                    .expect("the snapshotted peer literal must parse"),
            ),
        );
        Mux {
            configured_frontend_timeout: Duration::from_secs(60),
            frontend_token: Token(0),
            frontend: Connection::new_h1_server(session_ulid, socket, Duration::from_secs(60)),
            router: Router::new(Duration::from_secs(30), Duration::from_secs(30)),
            context: test_context(pool),
            session_ulid,
            timeouts: HashMap::new(),
            backend_registry: BackendRegistry::default(),
        }
    }

    /// To SEE THIS RED: in `log_context!` (mod.rs), put
    /// `peer = $self.frontend.socket().peer_addr().ok(),` back in place of
    /// `peer = $self.frontend.peer_address(),`. `Connection::socket()` hands
    /// back a `&mio::net::TcpStream`, so the slot resolves to mio's *inherent*
    /// `peer_addr` — a live `getpeername(2)` — and the rendered line carries
    /// the loopback address the socket is really connected to.
    #[test]
    fn log_context_renders_the_snapshotted_peer_not_a_live_lookup() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 4, 16384)));
        let (_listener, stream, live_peer) = connected_loopback_stream();
        let mux = mux_with_snapshotted_peer(&pool, stream);

        // Premise of the test: the live lookup is healthy and disagrees with the
        // snapshot. Without it the assertions below could pass for the wrong
        // reason (both answers happening to be the same address).
        assert_eq!(
            mux.frontend.socket().peer_addr().ok(),
            Some(live_peer),
            "the test socket must be genuinely connected, so a live lookup succeeds"
        );
        assert_ne!(
            SNAPSHOT_PEER,
            live_peer.to_string(),
            "the snapshotted and live addresses must differ for this test to discriminate"
        );

        let rendered = log_context!(mux);

        assert!(
            rendered.contains(&format!("peer=Some({SNAPSHOT_PEER})")),
            "the MUX peer= slot must render the snapshotted address; rendered: {rendered}"
        );
        assert!(
            !rendered.contains(&live_peer.to_string()),
            "the MUX peer= slot must not fall back to a live getpeername(2); rendered: {rendered}"
        );
    }

    /// The lighter envelope reads the same slot. Separate from its full
    /// sibling because the two macros carry their own copy of the `peer` line:
    /// fixing one and not the other would render two different peers for one
    /// session depending only on which borrow the callsite happened to hold.
    ///
    /// To SEE THIS RED: in `log_context_lite!` (mod.rs), put
    /// `peer = $self.frontend.socket().peer_addr().ok(),` back in place of
    /// `peer = $self.frontend.peer_address(),`.
    #[test]
    fn log_context_lite_renders_the_snapshotted_peer_not_a_live_lookup() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 4, 16384)));
        let (_listener, stream, live_peer) = connected_loopback_stream();
        let mux = mux_with_snapshotted_peer(&pool, stream);

        assert_eq!(
            mux.frontend.socket().peer_addr().ok(),
            Some(live_peer),
            "the test socket must be genuinely connected, so a live lookup succeeds"
        );

        let rendered = log_context_lite!(mux);

        assert!(
            rendered.contains(&format!("peer=Some({SNAPSHOT_PEER})")),
            "the lite MUX peer= slot must render the snapshotted address; rendered: {rendered}"
        );
        assert!(
            !rendered.contains(&live_peer.to_string()),
            "the lite MUX peer= slot must not fall back to a live getpeername(2); \
             rendered: {rendered}"
        );
    }
}
