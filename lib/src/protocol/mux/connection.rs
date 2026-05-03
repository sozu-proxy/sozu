//! Protocol-agnostic frontend/backend connection wrapper.
//!
//! [`Connection`] is the H1/H2 dispatch enum used everywhere in the mux
//! layer. Most of the methods are trivial pass-through forwarders to the
//! underlying [`ConnectionH1`] or [`ConnectionH2`] implementation — the
//! local `forward!` macro removes the boilerplate.
//!
//! The two `Endpoint` adaptors ([`EndpointServer`], [`EndpointClient`]) are
//! also defined here: they let a connection call back into either the
//! frontend connection or the backend [`Router`] map without knowing which
//! direction it faces.
//!
//! Edge-trigger discipline lives in `mux/h2.rs` (`writable`) — the canonical
//! home for the `signal_pending_write` / `arm_writable` invariant. This
//! module's abstractions delegate to that discipline through the
//! protocol-specific writers.

use std::{
    cell::RefCell,
    fmt::Debug,
    rc::{Rc, Weak},
    time::Instant,
};

use mio::{Token, net::TcpStream};
use rusty_ulid::Ulid;
use sozu_command::{logging::ansi_palette, ready::Ready};

use super::{
    BackendStatus, ConnectionH1, ConnectionH2, Context, Endpoint, GlobalStreamId, MuxResult,
    Position, Router,
    h2::{self, H2StreamId},
};
use crate::{
    L7ListenerHandler, ListenerHandler, Readiness, backends::Backend, pool::Pool,
    socket::SocketHandler, timer::TimeoutContainer,
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
    H2(ConnectionH2<Front>),
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
    (&$self:expr, $field:ident) => {
        match $self {
            Connection::H1(c) => &c.$field,
            Connection::H2(c) => &c.$field,
        }
    };
    (&mut $self:expr, $field:ident) => {
        match $self {
            Connection::H1(c) => &mut c.$field,
            Connection::H2(c) => &mut c.$field,
        }
    };
}

impl<Front: SocketHandler> Connection<Front> {
    pub fn new_h1_server(
        session_ulid: Ulid,
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
            session_ulid,
        })
    }
    pub fn new_h1_client(
        session_ulid: Ulid,
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
            session_ulid,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_h2_server(
        session_ulid: Ulid,
        front_stream: Front,
        pool: Weak<RefCell<Pool>>,
        timeout_container: TimeoutContainer,
        flood_config: h2::H2FloodConfig,
        connection_config: h2::H2ConnectionConfig,
        stream_idle_timeout: std::time::Duration,
        graceful_shutdown_deadline: Option<std::time::Duration>,
    ) -> Option<Connection<Front>> {
        Some(Connection::H2(ConnectionH2::new(
            session_ulid,
            front_stream,
            Position::Server,
            pool,
            flood_config,
            connection_config,
            stream_idle_timeout,
            graceful_shutdown_deadline,
            timeout_container,
            Some((H2StreamId::Zero, h2::CLIENT_PREFACE_SIZE)),
            Ready::READABLE | Ready::HUP | Ready::ERROR,
        )?))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_h2_client(
        session_ulid: Ulid,
        front_stream: Front,
        cluster_id: String,
        backend: Rc<RefCell<Backend>>,
        pool: Weak<RefCell<Pool>>,
        timeout_container: TimeoutContainer,
        flood_config: h2::H2FloodConfig,
        connection_config: h2::H2ConnectionConfig,
        stream_idle_timeout: std::time::Duration,
        graceful_shutdown_deadline: Option<std::time::Duration>,
    ) -> Option<Connection<Front>> {
        // Test-only injection point: when set via
        // [`__test_force_h2_client_failure`], pretend the pool was exhausted
        // and return `None`. This mirrors the buffer-pool-exhaustion branch
        // inside [`ConnectionH2::new`] deterministically so E2E tests can
        // exercise `Router::connect`'s rollback path (FIX-18) without having
        // to starve the pool in-process.
        #[cfg(any(test, feature = "e2e-hooks"))]
        if test_hooks::FORCE_NEW_H2_CLIENT_FAILURE.swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return None;
        }
        Some(Connection::H2(ConnectionH2::new(
            session_ulid,
            front_stream,
            Position::Client(
                cluster_id,
                backend,
                BackendStatus::Connecting(Instant::now()),
            ),
            pool,
            flood_config,
            connection_config,
            stream_idle_timeout,
            graceful_shutdown_deadline,
            timeout_container,
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
    pub fn timeout_container(&mut self) -> &mut TimeoutContainer {
        forward!(&mut self, timeout_container)
    }

    /// Returns connection-level byte overhead (bin, bout) for H2, (0, 0) for H1.
    pub fn overhead_bytes(&self) -> (usize, usize) {
        match self {
            Connection::H1(_) => (0, 0),
            Connection::H2(c) => (c.bytes.overhead_bin, c.bytes.overhead_bout),
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
                    c.readiness.signal_pending_read();
                    true
                } else {
                    false
                }
            }
            Connection::H2(c) => c.try_resume_reading(context),
        }
    }

    pub(super) fn graceful_goaway(&mut self) -> MuxResult {
        match self {
            Connection::H1(_) => MuxResult::Continue,
            Connection::H2(c) => c.graceful_goaway(),
        }
    }

    pub(super) fn is_draining(&self) -> bool {
        match self {
            Connection::H1(_) => false,
            Connection::H2(c) => c.drain.draining,
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
            Connection::H2(c) => c.graceful_shutdown_deadline_elapsed(),
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

    fn pre_close_client_bookkeeping(&self) {
        if let Position::Client(cluster_id, backend, _) = self.position() {
            let mut backend_borrow = backend.borrow_mut();
            backend_borrow.dec_connections();
            gauge_add!("backend.connections", -1);
            // Pair with the `+1` at `router.rs::connect` (new-dial path).
            // This is the graceful-close decrement, used both by the dead
            // backend path in `mod.rs::back_readable` (which routes through
            // `client.close()`) and by any explicit Connection::close.
            gauge_add!("backend.pool.size", -1);
            gauge_add!(
                "connections_per_backend",
                -1,
                Some(cluster_id),
                Some(&backend_borrow.backend_id)
            );
            trace!(
                "{} connection close: {:#?}",
                log_module_context!(),
                backend_borrow
            );
        }
    }

    fn pre_end_stream_client_bookkeeping(&self) {
        if let Position::Client(_, backend, BackendStatus::Connected) = self.position() {
            let mut backend_borrow = backend.borrow_mut();
            backend_borrow.active_requests = backend_borrow.active_requests.saturating_sub(1);
            trace!(
                "{} connection end stream: {:#?}",
                log_module_context!(),
                backend_borrow
            );
        }
    }

    fn pre_start_stream_client_bookkeeping(&self) {
        if let Position::Client(_, backend, BackendStatus::Connected) = self.position() {
            let mut backend_borrow = backend.borrow_mut();
            backend_borrow.active_requests += 1;
            trace!(
                "{} connection start stream: {:#?}",
                log_module_context!(),
                backend_borrow
            );
        }
    }

    pub(super) fn close<E, L>(&mut self, context: &mut Context<L>, endpoint: E)
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        self.pre_close_client_bookkeeping();
        forward!(self, close(context, endpoint))
    }

    pub(super) fn end_stream<L>(&mut self, stream: GlobalStreamId, context: &mut Context<L>)
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        self.pre_end_stream_client_bookkeeping();
        forward!(self, end_stream(stream, context))
    }

    pub(super) fn start_stream<L>(
        &mut self,
        stream: GlobalStreamId,
        context: &mut Context<L>,
    ) -> bool
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        self.pre_start_stream_client_bookkeeping();
        let started = forward!(self, start_stream(stream, context));
        if !started {
            // Undo active_requests increment on failure
            self.pre_end_stream_client_bookkeeping();
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
    fn socket(&self, _token: Token) -> Option<&TcpStream> {
        Some(self.0.socket())
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
    fn socket(&self, token: Token) -> Option<&TcpStream> {
        self.0.backends.get(&token).map(|c| c.socket())
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
