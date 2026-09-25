//! Protocol-agnostic frontend/backend connection wrapper.
//!
//! [`Connection`] is the H1/H2 dispatch enum used everywhere in the mux
//! layer. Most of the methods are trivial pass-through forwarders to the
//! underlying [`ConnectionH1`] or [`h2::ConnectionH2`] implementation — the
//! local `forward!` macro removes the boilerplate.
//!
//! The two `Endpoint` adaptors (`EndpointServer`, `EndpointClient`) are
//! also defined here: they let a connection call back into either the
//! frontend connection or the backend [`Router`] map without knowing which
//! direction it faces.
//!
//! Edge-trigger discipline lives in `mux/h2.rs` (`writable`) — the canonical
//! home for the `signal_pending_write` / `arm_writable` invariant. This
//! module's abstractions delegate to that discipline through the
//! protocol-specific writers.

use std::{
    fmt::Debug,
    time::{Duration, Instant},
};

use mio::{Token, net::TcpStream};
use rusty_ulid::Ulid;
use sozu_command::{logging::ansi_palette, ready::Ready};

use super::{
    BackendChange, BackendId, BackendStatus, ConnectionH1, Context, Endpoint, GlobalStreamId,
    MuxResult, Position, Router,
    h2::{self, H2Shell, H2StreamId},
    h2_flood_detector,
};
use crate::metrics::names;
use crate::{
    L7ListenerHandler, ListenerHandler, Readiness,
    socket::{SocketHandler, stats::socket_rtt},
};

/// Module-level prefix used on every log line emitted from this module.
/// Produces a bold bright-white `MUX-CONN` label (uniform across every
/// protocol) when the logger is in colored mode. Session-specific context
/// cannot be derived here because most log sites are inside `Endpoint` adapter
/// methods that only see the backend/frontend maps, not the wrapping
/// [`Connection`].
macro_rules! log_module_context {
    () => {{
        let (open, reset, _, _, _) = ansi_palette();
        format!("{open}MUX-CONN{reset}\t >>>", open = open, reset = reset)
    }};
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Connection<Front: SocketHandler> {
    H1(ConnectionH1<Front>),
    H2(H2Shell<Front>),
}

// Dispatches a method call or field access to the inner H1/H2 connection.
// Used by trivial pass-through methods on Connection<Front> to avoid
// repeating the two-arm match.
macro_rules! forward {
    ($self:expr, $method:ident ( $($args:tt)* )) => {
        match $self {
            Connection::H1(c) => c.$method($($args)*),
            Connection::H2(c) => c.$method($($args)*),
        }
    };
    // The H2 arm reaches one level further in. `Connection::H2` holds an
    // `H2Shell`, whose state machine is `H2Shell::core`; `ConnectionH1` still
    // carries its own fields directly. The asymmetry is the split, not an
    // oversight, and it is confined to these two arms — every METHOD forwarded
    // above is one `H2Shell` answers itself.
    (&$self:expr, $field:ident) => {
        match $self {
            Connection::H1(c) => &c.$field,
            Connection::H2(c) => &c.core.$field,
        }
    };
    (&mut $self:expr, $field:ident) => {
        match $self {
            Connection::H1(c) => &mut c.$field,
            Connection::H2(c) => &mut c.core.$field,
        }
    };
}

impl<Front: SocketHandler> Connection<Front> {
    pub fn new_h1_server(
        session_ulid: Ulid,
        front_stream: Front,
        timeout_duration: Duration,
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
            timeout_duration,
            timeout_deadline: Instant::now().checked_add(timeout_duration),
            parked_on_buffer_pressure: false,
            close_notify_sent: false,
            session_ulid,
            reused_from_pool: false,
        })
    }
    pub fn new_h1_client(
        session_ulid: Ulid,
        front_stream: Front,
        cluster_id: String,
        backend: BackendId,
        timeout_duration: Duration,
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
            timeout_duration,
            timeout_deadline: Instant::now().checked_add(timeout_duration),
            parked_on_buffer_pressure: false,
            close_notify_sent: false,
            session_ulid,
            reused_from_pool: false,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_h2_server(
        session_ulid: Ulid,
        front_stream: Front,
        buffers: &mut dyn super::buffer_source::BufferSource,
        timeout_duration: Duration,
        flood_config: h2_flood_detector::H2FloodConfig,
        connection_config: h2::H2ConnectionConfig,
        stream_idle_timeout: std::time::Duration,
        graceful_shutdown_deadline: Option<std::time::Duration>,
    ) -> Option<Connection<Front>> {
        Some(Connection::H2(H2Shell::new(
            session_ulid,
            front_stream,
            Position::Server,
            buffers,
            flood_config,
            connection_config,
            stream_idle_timeout,
            graceful_shutdown_deadline,
            timeout_duration,
            Some((H2StreamId::Zero, h2::CLIENT_PREFACE_SIZE)),
            Ready::READABLE | Ready::HUP | Ready::ERROR,
        )?))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_h2_client(
        session_ulid: Ulid,
        front_stream: Front,
        cluster_id: String,
        backend: BackendId,
        buffers: &mut dyn super::buffer_source::BufferSource,
        timeout_duration: Duration,
        flood_config: h2_flood_detector::H2FloodConfig,
        connection_config: h2::H2ConnectionConfig,
        stream_idle_timeout: std::time::Duration,
        graceful_shutdown_deadline: Option<std::time::Duration>,
    ) -> Option<Connection<Front>> {
        // Test-only injection point: when set via
        // [`__test_force_h2_client_failure`], pretend the pool was exhausted
        // and return `None`. This mirrors the buffer-pool-exhaustion branch
        // inside [`ConnectionH2::new`] deterministically so E2E tests can
        // exercise `Mux::dial_backend`'s rollback path (FIX-18) without having
        // to starve the pool in-process.
        #[cfg(any(test, feature = "e2e-hooks"))]
        if test_hooks::FORCE_NEW_H2_CLIENT_FAILURE.swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return None;
        }
        Some(Connection::H2(H2Shell::new(
            session_ulid,
            front_stream,
            Position::Client(
                cluster_id,
                backend,
                BackendStatus::Connecting(Instant::now()),
            ),
            buffers,
            flood_config,
            connection_config,
            stream_idle_timeout,
            graceful_shutdown_deadline,
            timeout_duration,
            None,
            Ready::WRITABLE | Ready::HUP | Ready::ERROR,
        )?))
    }

    pub fn readiness(&self) -> &Readiness {
        forward!(&self, readiness)
    }
    pub fn readiness_mut(&mut self) -> &mut Readiness {
        forward!(&mut self, readiness)
    }
    pub fn position(&self) -> &Position {
        forward!(&self, position)
    }
    pub fn position_mut(&mut self) -> &mut Position {
        forward!(&mut self, position)
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
    /// The next instant this connection wants `timeout()` called at, or
    /// `None` for "no timer".
    ///
    /// This replaced `timeout_container()`, which handed the caller the wheel
    /// handle itself. The cores no longer own one: they publish a deadline and
    /// the `Mux` adapter owns every `TimeoutContainer`, so there is exactly one
    /// place that talks to `crate::timer` and exactly one place that can get
    /// the arm / re-arm discipline wrong. See `LIFECYCLE.md` §7.7.
    pub fn poll_timeout(&self) -> Option<Instant> {
        match self {
            Connection::H1(c) => c.poll_timeout(),
            Connection::H2(c) => c.core.poll_timeout(),
        }
    }

    /// The configured idle timeout, carried so the adapter's container keeps a
    /// current `duration()` for whoever inherits it (the WebSocket upgrade
    /// hands it to `Pipe`, which re-arms from it).
    pub fn timeout_duration(&self) -> Duration {
        match self {
            Connection::H1(c) => c.timeout_duration,
            Connection::H2(c) => c.core.timeout_duration,
        }
    }

    /// Push the deadline one full duration out from `now`. Replaces
    /// `timeout_container().set(token)` / `.reset()` at the adapter's sites.
    pub fn arm_timeout(&mut self, now: Instant) {
        match self {
            Connection::H1(c) => c.arm_timeout(now),
            // `ConnectionH2::arm_timeout` reads the connection's own snapshot,
            // which the adapter has not necessarily mirrored yet at the sites
            // that call this; re-arm against the caller's `now` instead.
            Connection::H2(c) => {
                let duration = c.core.timeout_duration;
                c.core.set_timeout_duration(duration, now)
            }
        }
    }

    /// Ask for no timer at all. Replaces `timeout_container().cancel()`.
    pub fn clear_timeout(&mut self) {
        match self {
            Connection::H1(c) => c.clear_timeout(),
            Connection::H2(c) => c.core.clear_timeout(),
        }
    }

    /// Adopt a new configured duration and re-arm from `now`. Replaces
    /// `timeout_container().set_duration(d)`.
    pub fn set_timeout_duration(&mut self, duration: Duration, now: Instant) {
        match self {
            Connection::H1(c) => c.set_timeout_duration(duration, now),
            Connection::H2(c) => c.core.set_timeout_duration(duration, now),
        }
    }

    /// Returns connection-level byte overhead (bin, bout) for H2, (0, 0) for H1.
    pub fn overhead_bytes(&self) -> (usize, usize) {
        match self {
            Connection::H1(_) => (0, 0),
            Connection::H2(c) => (c.core.bytes.overhead_bin, c.core.bytes.overhead_bout),
        }
    }

    pub(super) fn readable<E, L>(&mut self, context: &mut Context<L>, endpoint: E) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        forward!(self, readable(context, endpoint))
    }
    pub(super) fn writable<E, L>(&mut self, context: &mut Context<L>, endpoint: E) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        forward!(self, writable(context, endpoint))
    }

    /// Returns true if this connection could not read because its stream's
    /// kawa buffer was full. Used to prevent the dead-backend check from
    /// closing a backend that still has data in the OS socket buffer.
    pub(super) fn has_buffer_pressure<L>(&self, context: &Context<L>) -> bool
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
    pub(super) fn try_resume_reading<L>(&mut self, context: &Context<L>) -> bool
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
                    trace!(
                        "{} H1 try_resume_reading: re-arming READABLE",
                        log_module_context!()
                    );
                    // Pre: we only reach here because the connection parked on
                    // buffer pressure AND the kawa now has room. Pair the
                    // synthetic READABLE event with that drained-space fact:
                    // edge-triggered epoll won't re-fire on its own, so this is
                    // the sole re-arm path.
                    debug_assert!(
                        c.parked_on_buffer_pressure,
                        "re-arm only fires for a connection parked on buffer pressure"
                    );
                    c.readiness.signal_pending_read();
                    debug_assert!(
                        c.readiness.event.is_readable(),
                        "signal_pending_read must leave a READABLE event queued"
                    );
                    true
                } else {
                    false
                }
            }
            Connection::H2(c) => c.core.try_resume_reading(context),
        }
    }

    /// `now` is the caller's clock snapshot; H2 arms the graceful-shutdown
    /// budget from it. H1 has no multiplex to drain and ignores it.
    pub(super) fn graceful_goaway(&mut self, now: Instant) -> MuxResult {
        match self {
            Connection::H1(_) => MuxResult::Continue,
            Connection::H2(c) => c.graceful_goaway(now),
        }
    }

    pub(super) fn is_draining(&self) -> bool {
        match self {
            Connection::H1(_) => false,
            Connection::H2(c) => c.core.drain.draining(),
        }
    }

    /// Proxy-side graceful-shutdown budget exhaustion check. Only H2
    /// connections carry the timer — H1 has no multiplex to drain, so its
    /// answer is always `false` and the H1 path continues to fall through
    /// to the ordinary single-response close. See
    /// [`h2::ConnectionH2::graceful_shutdown_deadline_elapsed`].
    pub(super) fn graceful_shutdown_deadline_elapsed(&self) -> bool {
        match self {
            Connection::H1(_) => false,
            Connection::H2(c) => c.core.graceful_shutdown_deadline_elapsed(),
        }
    }

    pub(super) fn has_pending_write(&self) -> bool {
        forward!(self, has_pending_write())
    }

    /// Connection-level [`Self::has_pending_write`] extended with a per-stream
    /// back-buffer probe (LIFECYCLE §9 invariant 16). Only H2 multiplexes
    /// multiple streams — H1 falls back to [`Self::has_pending_write`] since
    /// its single-response pipeline already accounts for pending bytes.
    pub(super) fn has_pending_write_including_streams<L>(&self, context: &super::Context<L>) -> bool
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        match self {
            Connection::H1(c) => c.has_pending_write(),
            Connection::H2(c) => c.has_pending_write_full(context),
        }
    }

    pub(super) fn initiate_close_notify(&mut self) -> bool {
        forward!(self, initiate_close_notify())
    }

    pub(super) fn flush_zero_buffer(&mut self) {
        if let Connection::H2(c) = self {
            c.flush_zero_buffer();
        }
    }

    fn pre_close_client_bookkeeping<L>(&self, context: &mut Context<L>)
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        if let Position::Client(cluster_id, backend, _) = self.position() {
            // The close path releases exactly one slot. The embedder performs
            // it and holds the "decreased by one, unless already at zero"
            // relation — see `BackendRegistry::apply`.
            context.record_backend_delta(backend, BackendChange::ConnectionClosed);
            gauge_add!(names::backend::CONNECTIONS, -1);
            // Pair with the `+1` at `router.rs::connect` (new-dial path).
            // This is the graceful-close decrement, used both by the dead
            // backend path in `mod.rs::back_readable` (which routes through
            // `client.close()`) and by any explicit Connection::close.
            gauge_add!(names::backend::POOL_SIZE, -1);
            gauge_add!(
                names::backend::CONNECTIONS_PER_BACKEND,
                -1,
                Some(cluster_id),
                Some(&backend.backend_id)
            );
            trace!("{} connection close: {:?}", log_module_context!(), backend);
        }
    }

    fn pre_end_stream_client_bookkeeping<L>(&self, context: &mut Context<L>)
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        if let Position::Client(_, backend, BackendStatus::Connected) = self.position() {
            // Pairs with the `StreamsStarted(1)` in
            // `post_start_stream_client_bookkeeping`.
            context.record_backend_delta(backend, BackendChange::StreamsEnded(1));
            trace!(
                "{} connection end stream: {:?}",
                log_module_context!(),
                backend
            );
        }
    }

    fn post_start_stream_client_bookkeeping<L>(&self, context: &mut Context<L>)
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        if let Position::Client(_, backend, BackendStatus::Connected) = self.position() {
            // Pairs with the `StreamsEnded(1)` in the end path.
            context.record_backend_delta(backend, BackendChange::StreamsStarted(1));
            trace!(
                "{} connection start stream: {:?}",
                log_module_context!(),
                backend
            );
        }
    }

    pub(super) fn close<E, L>(&mut self, context: &mut Context<L>, endpoint: E)
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        self.pre_close_client_bookkeeping(context);
        forward!(self, close(context, endpoint))
    }

    pub(super) fn end_stream<L>(&mut self, stream: GlobalStreamId, context: &mut Context<L>)
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        self.pre_end_stream_client_bookkeeping(context);
        forward!(self, end_stream(stream, context))
    }

    /// Release the `active_requests` charge [`Self::start_stream`] took, for
    /// a caller that has to abandon a connection it already started a stream
    /// on.
    ///
    /// Its one caller is `Mux::dial_backend`'s `register_socket` rollback,
    /// which drops a connection whose `start_stream` already succeeded. Same
    /// guard as the charge, so the pair holds whatever status the connection
    /// is in: a freshly-dialled one is `BackendStatus::Connecting` and was
    /// never charged, so this correctly emits nothing for it.
    pub(super) fn release_start_stream_charge<L>(&self, context: &mut Context<L>)
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        self.pre_end_stream_client_bookkeeping(context);
    }

    pub(super) fn start_stream<L>(
        &mut self,
        stream: GlobalStreamId,
        context: &mut Context<L>,
    ) -> bool
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        // The charge is emitted AFTER the start is known to have succeeded,
        // never before with a rollback behind it. A refused `start_stream`
        // must not leak an `active_requests` charge onto the backend — it
        // would skew least-loaded balancing forever — and the way to hold
        // that is for the refusal path to have no charge to undo rather than
        // to undo one correctly. Ordering the emit this way is free: neither
        // `ConnectionH1::start_stream` nor `ConnectionH2::start_stream` reads
        // backend load state, so nothing about the decision changes.
        let started = forward!(self, start_stream(stream, context));
        if started {
            self.post_start_stream_client_bookkeeping(context);
        }
        started
    }
}

#[derive(Debug)]
pub(super) struct EndpointServer<'a, Front: SocketHandler>(pub &'a mut Connection<Front>);
#[derive(Debug)]
pub(super) struct EndpointClient<'a>(pub &'a mut Router);

// note: EndpointServer are used by client Connection, they do not know the frontend Token
// they will use the Stream's Token which is their backend token
impl<Front: SocketHandler + Debug> Endpoint for EndpointServer<'_, Front> {
    fn readiness(&self, _token: Token) -> &Readiness {
        self.0.readiness()
    }
    fn readiness_mut(&mut self, _token: Token) -> &mut Readiness {
        self.0.readiness_mut()
    }
    fn peer_rtt(&self, _token: Token) -> Option<Duration> {
        socket_rtt(self.0.socket())
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
                    "{} backend token {:?} missing from backends map (readiness)",
                    log_module_context!(),
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
                    "{} backend token {:?} missing from backends map (readiness_mut)",
                    log_module_context!(),
                    token
                );
                &mut self.0.fallback_readiness
            }
        }
    }
    fn peer_rtt(&self, token: Token) -> Option<Duration> {
        self.0
            .backends
            .get(&token)
            .and_then(|c| socket_rtt(c.socket()))
    }

    fn end_stream<L>(&mut self, token: Token, stream: GlobalStreamId, context: &mut Context<L>)
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        match self.0.backends.get_mut(&token) {
            Some(backend) => backend.end_stream(stream, context),
            None => {
                error!(
                    "{} backend token {:?} missing from backends map (end_stream)",
                    log_module_context!(),
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
                    "{} backend token {:?} missing from backends map (start_stream)",
                    log_module_context!(),
                    token
                );
                false
            }
        }
    }
}

/// Test-only injection hooks for the mux layer.
///
/// These are compiled **only** when running `cargo test` (or with
/// `cfg(test)` enabled); downstream code must not rely on them. They exist
/// so end-to-end tests can drive hard-to-reach code paths — buffer-pool
/// exhaustion during backend attach, stream-ID exhaustion — without
/// having to reproduce the underlying resource starvation in-process.
#[cfg(any(test, feature = "e2e-hooks"))]
pub mod test_hooks {
    use std::sync::atomic::AtomicBool;

    /// When `true`, the next call to [`super::Connection::new_h2_client`]
    /// returns `None` as if the buffer pool were exhausted. The flag is
    /// consumed (reset to `false`) by that call so each opt-in is scoped
    /// to exactly one attempted backend attach.
    pub static FORCE_NEW_H2_CLIENT_FAILURE: AtomicBool = AtomicBool::new(false);

    /// Arm or disarm the `new_h2_client` failure injection. Returns the
    /// previous value so tests can stack-save/restore if they run in
    /// parallel (`cargo test` defaults to serial for this crate because
    /// of global registries, but keep the API honest).
    pub fn __test_force_h2_client_failure(on: bool) -> bool {
        FORCE_NEW_H2_CLIENT_FAILURE.swap(on, std::sync::atomic::Ordering::SeqCst)
    }
}
