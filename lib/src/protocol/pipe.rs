//! Transparent byte-stream forwarder (TCP + WebSocket post-upgrade).
//!
//! Forwards bytes between front and back through fixed-size buffers without
//! payload inspection. When bytes enter a sozu-owned buffer, the opposite
//! endpoint is armed via `Readiness::arm_writable()` so edge-triggered epoll
//! cannot park buffered data behind a missing writable edge. A backend hang-up
//! only adds the frontend's WRITABLE interest: the event stays whatever the
//! last write left it, so a client that stopped reading parks the pass instead
//! of spinning it. Used as the post-handshake state for raw TCP listeners and
//! after a successful WebSocket upgrade on the H1 path.

use std::{cell::RefCell, net::SocketAddr, rc::Rc};

use mio::{Token, net::TcpStream};
use rusty_ulid::Ulid;
use sozu_command::{
    config::MAX_LOOP_ITERATIONS,
    logging::{EndpointRecord, LogContext, ansi_palette},
    state::ClusterId,
};

use crate::metrics::names;
use crate::{
    L7Proxy, ListenerHandler, Protocol, Readiness, SessionMetrics, SessionResult, StateResult,
    backends::Backend,
    pool::Checkout,
    protocol::{SessionState, http::parser::Method, mux::drain_tls_close_notify},
    socket::{SocketHandler, SocketResult, TransportProtocol, stats::socket_rtt},
    sozu_command::ready::Ready,
    timer::TimeoutContainer,
};

#[cfg(all(target_os = "linux", feature = "splice"))]
use crate::splice::{self, SplicePipe};

/// This macro is defined uniquely in this module to help the tracking of
/// pipelining issues inside Sōzu. Colored output uses bold bright-white
/// (uniform across every protocol) for the protocol label, light grey for the
/// `Session` keyword, gray for keys and bright white for values. The
/// `[ulid - - -]` context comes first to stay aligned with `MUX-*` and
/// `SOCKET` log lines.
macro_rules! log_context {
    ($self:expr) => {{
        let (open, reset, grey, gray, white) = ansi_palette();
        format!(
            "{gray}{ctx}{reset}\t{open}PIPE{reset}\t{grey}Session{reset}({gray}address{reset}={white}{address}{reset}, {gray}frontend{reset}={white}{frontend}{reset}, {gray}frontend_readiness{reset}={white}{frontend_readiness}{reset}, {gray}frontend_status{reset}={white}{frontend_status:?}{reset}, {gray}backend{reset}={white}{backend}{reset}, {gray}backend_status{reset}={white}{backend_status:?}{reset}, {gray}backend_readiness{reset}={white}{backend_readiness}{reset})\t >>>",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            ctx = $self.log_context(),
            address = $self.session_address.map(|addr| addr.to_string()).unwrap_or_else(|| "<none>".to_string()),
            frontend = $self.frontend_token.0,
            frontend_readiness = $self.frontend_readiness,
            frontend_status = $self.frontend_status,
            backend = $self.backend_token.map(|token| token.0.to_string()).unwrap_or_else(|| "<none>".to_string()),
            backend_status = $self.backend_status,
            backend_readiness = $self.backend_readiness,
        )
    }};
}

#[derive(PartialEq, Eq)]
pub enum SessionStatus {
    Normal,
    DefaultAnswer,
}

#[derive(Copy, Clone, Debug)]
enum ConnectionStatus {
    Normal,
    ReadOpen,
    WriteOpen,
    Closed,
}

/// matches sozu_command_lib::logging::access_logs::EndpointRecords
pub enum WebSocketContext {
    Http {
        method: Option<Method>,
        authority: Option<String>,
        path: Option<String>,
        status: Option<u16>,
        reason: Option<String>,
    },
    Tcp,
}

pub struct Pipe<Front: SocketHandler, L: ListenerHandler> {
    backend_buffer: Checkout,
    backend_id: Option<String>,
    /// Routed cluster lifetime inherited from an HTTP request or TCP session
    /// when that owner upgrades into this long-lived pipe.
    cluster_metrics_incarnation: crate::metrics::ClusterMetricsCapture,
    /// Address of the backend this pipe forwards to, recorded when Sōzu
    /// picked it: from `backend` in `Pipe::new` (WebSocket upgrades), or
    /// through `set_backend_address` by the TCP proxy when it dials. The
    /// access log reads it from here instead of asking the socket with
    /// `getpeername(2)` once per session.
    backend_address: Option<SocketAddr>,
    pub backend_readiness: Readiness,
    backend_socket: Option<TcpStream>,
    backend_status: ConnectionStatus,
    backend_token: Option<Token>,
    pub backend: Option<Rc<RefCell<Backend>>>,
    cluster_id: Option<ClusterId>,
    pub container_backend_timeout: Option<TimeoutContainer>,
    pub container_frontend_timeout: Option<TimeoutContainer>,
    frontend_buffer: Checkout,
    /// Set once `SessionState::close` has queued the frontend's TLS
    /// `close_notify`: a second alert would corrupt the TLS stream
    /// (`drain_tls_close_notify`, `lib/src/protocol/mux/shared.rs`).
    close_notify_sent: bool,
    pub frontend_readiness: Readiness,
    frontend_status: ConnectionStatus,
    frontend_token: Token,
    frontend: Front,
    listener: Rc<RefCell<L>>,
    protocol: Protocol,
    /// Connection/session ULID inherited from the parent mux or handshake.
    /// Emitted in the first slot of the legacy log-context bracket.
    session_id: Ulid,
    request_id: Ulid,
    session_address: Option<SocketAddr>,
    websocket_context: WebSocketContext,
    /// Connection-scoped TLS metadata captured at handshake completion,
    /// inherited from the upstream mux `HttpContext` when `Pipe` is created
    /// via WSS upgrade. `None` on plaintext paths (plain TCP, plain WS,
    /// proxy-protocol) where no TLS was terminated by Sōzu.
    tls_version: Option<&'static str>,
    tls_cipher: Option<&'static str>,
    /// Negotiated SNI hostname, pre-lowercased, no port. `None` on plaintext
    /// paths or when the client omitted the SNI extension.
    tls_sni: Option<String>,
    tls_alpn: Option<&'static str>,
    /// Override for the access-log tags lookup key. `None` (every path but
    /// TCP SNI-preread) falls back to the historical bare listener-address
    /// key; SNI-routed TCP sessions carry the matched frontend's composed
    /// key (`sni_tags_key` in `lib/src/tcp.rs`) so one listener's many
    /// SNI/ALPN fronts each log their own tags.
    tags_key: Option<String>,
    /// Kernel-pipe pair used for zero-copy `splice(2)` forwarding on
    /// `Protocol::TCP` listeners. Allocated lazily in `new()` and
    /// `None` for WebSocket-after-upgrade paths or when allocation
    /// failed (caller falls back to the buffered path).
    #[cfg(all(target_os = "linux", feature = "splice"))]
    splice_pipe: Option<SplicePipe>,
}

impl<Front: SocketHandler, L: ListenerHandler> Pipe<Front, L> {
    /// Instantiate a new Pipe SessionState with:
    ///
    /// - frontend_interest: READABLE | WRITABLE | HUP | ERROR
    /// - frontend_event: EMPTY
    /// - backend_interest: READABLE | WRITABLE | HUP | ERROR
    /// - backend_event: EMPTY
    ///
    /// Remember to set the events from the previous State!
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        backend_buffer: Checkout,
        backend_id: Option<String>,
        backend_socket: Option<TcpStream>,
        backend: Option<Rc<RefCell<Backend>>>,
        container_backend_timeout: Option<TimeoutContainer>,
        container_frontend_timeout: Option<TimeoutContainer>,
        cluster_id: Option<ClusterId>,
        frontend_buffer: Checkout,
        frontend_token: Token,
        frontend: Front,
        listener: Rc<RefCell<L>>,
        protocol: Protocol,
        session_id: Ulid,
        request_id: Ulid,
        session_address: Option<SocketAddr>,
        websocket_context: WebSocketContext,
    ) -> Pipe<Front, L> {
        let frontend_status = ConnectionStatus::Normal;
        let backend_status = if backend_socket.is_none() {
            ConnectionStatus::Closed
        } else {
            ConnectionStatus::Normal
        };

        let backend_address = backend.as_ref().map(|backend| backend.borrow().address);
        let mut session = Pipe {
            backend_buffer,
            backend_id,
            cluster_metrics_incarnation: Default::default(),
            backend_address,
            backend_readiness: Readiness {
                interest: Ready::READABLE | Ready::WRITABLE | Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            backend_socket,
            backend_status,
            backend_token: None,
            backend,
            cluster_id,
            container_backend_timeout,
            container_frontend_timeout,
            frontend_buffer,
            close_notify_sent: false,
            frontend_readiness: Readiness {
                interest: Ready::READABLE | Ready::WRITABLE | Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            frontend_status,
            frontend_token,
            frontend,
            listener,
            protocol,
            session_id,
            request_id,
            session_address,
            websocket_context,
            tls_version: None,
            tls_cipher: None,
            tls_sni: None,
            tls_alpn: None,
            tags_key: None,
            #[cfg(all(target_os = "linux", feature = "splice"))]
            splice_pipe: if protocol == Protocol::TCP {
                SplicePipe::new()
            } else {
                None
            },
        };

        session.arm_inherited_buffer_writes();

        trace!("{} created pipe", log_context!(session));
        session
    }

    fn arm_inherited_buffer_writes(&mut self) {
        if self.backend_buffer.available_data() > 0 {
            self.frontend_readiness.arm_writable();
        }
        if self.frontend_buffer.available_data() > 0 && self.backend_socket.is_some() {
            self.backend_readiness.arm_writable();
        }
    }

    pub fn restore_readiness_events(&mut self, frontend_event: Ready, backend_event: Ready) {
        self.frontend_readiness.event = frontend_event;
        self.backend_readiness.event = backend_event;
        self.arm_inherited_buffer_writes();
    }

    pub(crate) fn set_cluster_metrics_incarnation(
        &mut self,
        incarnation: crate::metrics::ClusterMetricsCapture,
    ) {
        self.cluster_metrics_incarnation = incarnation;
    }

    /// Stamp connection-scoped TLS metadata onto the pipe for access-log
    /// emission. Two caller classes exist:
    ///
    /// - the HTTPS→WSS upgrade path (`https.rs::upgrade_mux`), which stamps
    ///   all four fields captured at handshake time from the prior mux
    ///   `HttpContext` (version/cipher/SNI/ALPN);
    /// - the TCP SNI-preread upgrade paths (`tcp.rs::upgrade_send` and
    ///   `tcp.rs::build_pipe_from_preread`), which stamp the routed SNI and
    ///   the offered-ALPN label with `version`/`cipher` deliberately `None`:
    ///   Sōzu never terminates that TLS session — it only parses the
    ///   ClientHello in passthrough — so it never learns the negotiated
    ///   parameters.
    ///
    /// Plain (non-SNI-routed) TCP, plain WS, and proxy-protocol paths never
    /// call this, so their access logs continue to emit `None` for all TLS
    /// fields.
    pub fn set_tls_metadata(
        &mut self,
        version: Option<&'static str>,
        cipher: Option<&'static str>,
        sni: Option<String>,
        alpn: Option<&'static str>,
    ) {
        self.tls_version = version;
        self.tls_cipher = cipher;
        self.tls_sni = sni;
        self.tls_alpn = alpn;
    }

    pub fn front_socket(&self) -> &TcpStream {
        self.frontend.socket_ref()
    }

    /// The frontend socket handler itself, for what only it knows how to
    /// record: `FrontRustls::readiness_delivered` (`lib/src/https.rs`).
    pub fn front_socket_handler_mut(&mut self) -> &mut Front {
        &mut self.frontend
    }

    pub fn front_socket_mut(&mut self) -> &mut TcpStream {
        self.frontend.socket_mut()
    }

    pub fn back_socket(&self) -> Option<&TcpStream> {
        self.backend_socket.as_ref()
    }

    pub fn back_socket_mut(&mut self) -> Option<&mut TcpStream> {
        self.backend_socket.as_mut()
    }

    pub fn set_back_socket(&mut self, socket: TcpStream) {
        self.backend_socket = Some(socket);
        self.backend_status = ConnectionStatus::Normal;
    }

    pub fn back_token(&self) -> Vec<Token> {
        self.backend_token.iter().cloned().collect()
    }

    fn reset_timeouts(&mut self) {
        if let Some(t) = self.container_frontend_timeout.as_mut()
            && !t.reset()
        {
            error!(
                "{} Could not reset front timeout (pipe)",
                log_context!(self)
            );
        }

        if let Some(t) = self.container_backend_timeout.as_mut()
            && !t.reset()
        {
            error!("{} Could not reset back timeout (pipe)", log_context!(self));
        }
    }

    pub fn set_cluster_id(&mut self, cluster_id: Option<ClusterId>) {
        self.cluster_id = cluster_id;
    }

    /// Override the access-log tags lookup key (see the `tags_key` field
    /// doc). Called from `lib/src/tcp.rs`'s pipe-building upgrade paths for
    /// SNI-routed sessions; `None` (the default) keeps the historical
    /// bare-address lookup.
    pub fn set_tags_key(&mut self, tags_key: Option<String>) {
        self.tags_key = tags_key;
    }

    pub fn set_backend_id(&mut self, backend_id: Option<String>) {
        self.backend_id = backend_id;
    }

    pub fn set_back_token(&mut self, token: Token) {
        self.backend_token = Some(token);
    }

    /// Record the address of the backend Sōzu dialed for this pipe (see the
    /// `backend_address` field doc). The TCP proxy calls it on every dial,
    /// so a retry onto another backend overwrites the previous address.
    pub fn set_backend_address(&mut self, backend_address: Option<SocketAddr>) {
        self.backend_address = backend_address;
    }

    pub fn get_session_address(&self) -> Option<SocketAddr> {
        self.session_address
            .or_else(|| self.frontend.socket_ref().peer_addr().ok())
    }

    /// The address recorded when the backend was chosen, without a
    /// `getpeername(2)`: it stays reported whatever the socket's state, where
    /// `peer_addr` failed with `ENOTCONN` on a backend still connecting or
    /// already reset.
    pub fn get_backend_address(&self) -> Option<SocketAddr> {
        self.backend_address
    }

    fn protocol_string(&self) -> &'static str {
        match self.protocol {
            Protocol::TCP => "TCP",
            Protocol::HTTP => "WS",
            Protocol::HTTPS => match self.frontend.protocol() {
                TransportProtocol::Ssl2 => "WSS-SSL2",
                TransportProtocol::Ssl3 => "WSS-SSL3",
                TransportProtocol::Tls1_0 => "WSS-TLS1.0",
                TransportProtocol::Tls1_1 => "WSS-TLS1.1",
                TransportProtocol::Tls1_2 => "WSS-TLS1.2",
                TransportProtocol::Tls1_3 => "WSS-TLS1.3",
                _ => unreachable!(),
            },
            _ => unreachable!(),
        }
    }

    pub fn log_request(&self, metrics: &SessionMetrics, error: bool, message: Option<&str>) {
        let listener = self.listener.borrow();
        let context = self.log_context();
        let endpoint = self.log_endpoint();
        metrics.register_end_of_session_for_incarnation(&context, self.cluster_metrics_incarnation);
        // TCP SNI-preread sessions carry the matched frontend's own tags
        // key (`set_tags_key`); every other path keeps the historical
        // bare-address lookup.
        let address_key = listener.get_addr().to_string();
        let tags_key = self.tags_key.as_deref().unwrap_or(&address_key);
        log_access!(
            error,
            on_failure: { incr!(names::access_logs::UNSENT) },
            message,
            context,
            session_address: self.get_session_address(),
            backend_address: self.get_backend_address(),
            protocol: self.protocol_string(),
            endpoint,
            tags: listener.get_tags(tags_key),
            client_rtt: socket_rtt(self.front_socket()),
            server_rtt: self.backend_socket.as_ref().and_then(socket_rtt),
            service_time: metrics.service_time(),
            response_time: metrics.backend_response_time(),
            request_time: metrics.request_time(),
            start_time_ns: metrics.start_wall_ns(),
            bytes_in: metrics.bin,
            bytes_out: metrics.bout,
            user_agent: None,
            x_request_id: None,
            // Pipe is post-upgrade; the TLS metadata was captured once at
            // handshake in `https.rs::upgrade_handshake` and plumbed through
            // via `set_tls_metadata`. Plaintext paths leave these fields as
            // `None` — matching the TCP log shape.
            tls_version: self.tls_version,
            tls_cipher: self.tls_cipher,
            tls_sni: self.tls_sni.as_deref(),
            tls_alpn: self.tls_alpn,
            xff_chain: None,
            otel: None,
        );
    }

    pub fn log_request_success(&self, metrics: &SessionMetrics) {
        self.log_request(metrics, false, None);
    }

    pub fn log_request_error(&self, metrics: &SessionMetrics, message: &str) {
        incr!(names::pipe::ERRORS);
        error!(
            "{} Could not process request properly got: {}",
            log_context!(self),
            message
        );
        self.print_state(self.protocol_string());
        self.log_request(metrics, true, Some(message));
    }

    /// Access-log wrapper for benign idle-timeout tear-downs.
    ///
    /// Unlike `log_request_error`, this path logs at `debug!` and skips the
    /// state dump — an idle pipe hitting its front/back_timeout is expected
    /// behaviour (e.g. a WebSocket with no keepalive) and should not pollute
    /// the error stream.
    pub fn log_request_timeout(&self, metrics: &SessionMetrics, message: &str) {
        debug!("{} pipe timeout: {}", log_context!(self), message);
        self.log_request(metrics, true, Some(message));
    }

    /// Bytes currently sitting inside the `splice` frontend→backend
    /// kernel pipe (`0` if splice is disabled or the pipe was not
    /// allocated). Counted as "request in flight" by `check_connections`
    /// so a half-closed session stays alive until the kernel drains.
    #[cfg(all(target_os = "linux", feature = "splice"))]
    fn splice_in_pending(&self) -> usize {
        self.splice_pipe
            .as_ref()
            .map(|p| p.in_pipe_pending)
            .unwrap_or(0)
    }
    #[cfg(not(all(target_os = "linux", feature = "splice")))]
    fn splice_in_pending(&self) -> usize {
        0
    }

    /// Bytes currently sitting inside the `splice` backend→frontend
    /// kernel pipe. Counterpart to `splice_in_pending` for the response
    /// direction.
    #[cfg(all(target_os = "linux", feature = "splice"))]
    fn splice_out_pending(&self) -> usize {
        self.splice_pipe
            .as_ref()
            .map(|p| p.out_pipe_pending)
            .unwrap_or(0)
    }
    #[cfg(not(all(target_os = "linux", feature = "splice")))]
    fn splice_out_pending(&self) -> usize {
        0
    }

    /// Realised kernel-pipe capacity per direction (`0` if splice is
    /// disabled). Drives the "pipe is full" backpressure check in the
    /// splice readable methods and the per-call `len` for `splice_in`.
    #[cfg(all(target_os = "linux", feature = "splice"))]
    fn splice_capacity(&self) -> usize {
        self.splice_pipe.as_ref().map(|p| p.capacity).unwrap_or(0)
    }

    /// Tear down both readiness trackers ahead of a `SessionResult::Close`.
    ///
    /// This is the *write-only-shutdown discipline* (CLAUDE.md gotcha: never
    /// `shutdown(Shutdown::Both)` on a TLS frontend — it emits a TCP RST that
    /// truncates the already-queued response). `Pipe` never issues an explicit
    /// `shutdown`; it closes purely by clearing interest+event so the event
    /// loop stops driving I/O and lets the kernel flush queued bytes, with the
    /// peer close arriving via the normal read path. The post-condition
    /// asserts both trackers are fully cleared.
    fn reset_readiness_for_close(&mut self) {
        self.frontend_readiness.reset();
        self.backend_readiness.reset();
        debug_assert!(
            self.frontend_readiness.interest.is_empty() && self.frontend_readiness.event.is_empty(),
            "frontend readiness must be fully cleared on close (write-only-shutdown discipline)"
        );
        debug_assert!(
            self.backend_readiness.interest.is_empty() && self.backend_readiness.event.is_empty(),
            "backend readiness must be fully cleared on close (write-only-shutdown discipline)"
        );
    }

    /// Wether the session should be kept open, depending on endpoints status
    /// and buffer usage (both in memory and in kernel)
    pub fn check_connections(&self) -> bool {
        // In-flight accounting must never see more *buffered* bytes than the
        // backing Checkout buffer can hold. We intentionally do NOT bound the
        // splice-pending counters by the pipe `capacity`: a kernel pipe buffers
        // well beyond its nominal `F_GETPIPE_SZ` when `splice(2)` moves
        // skb-backed GRO segments, so `splice_*_pending` legitimately exceeds it
        // (see `splice_readable`). A violation here means a `fill`/`consume`
        // elsewhere desynced the counters, corrupting the keep-alive decision.
        debug_assert!(
            self.frontend_buffer.available_data() <= self.frontend_buffer.capacity(),
            "frontend buffered data exceeds its capacity"
        );
        debug_assert!(
            self.backend_buffer.available_data() <= self.backend_buffer.capacity(),
            "backend buffered data exceeds its capacity"
        );

        let request_is_inflight = self.frontend_buffer.available_data() > 0
            || self.frontend_readiness.event.is_readable()
            || self.splice_in_pending() > 0;
        // Plaintext a TLS frontend absorbed is not delivered: `socket_write`
        // reports it consumed while its records still sit inside rustls
        // (at most 64 KiB) until the kernel takes them, so the response is in
        // flight for as long as the frontend wants to write. A plaintext
        // frontend never does (`SocketHandler::socket_wants_write`,
        // `lib/src/socket.rs`).
        let response_is_inflight = self.backend_buffer.available_data() > 0
            || self.backend_readiness.event.is_readable()
            || self.splice_out_pending() > 0
            || self.frontend.socket_wants_write();
        match (self.frontend_status, self.backend_status) {
            (ConnectionStatus::Normal, ConnectionStatus::Normal) => true,
            (ConnectionStatus::Normal, ConnectionStatus::ReadOpen) => true,
            (ConnectionStatus::Normal, ConnectionStatus::WriteOpen) => {
                // technically we should keep it open, but we'll assume that if the front
                // is not readable and there is no in flight data front -> back or back -> front,
                // we'll close the session, otherwise it interacts badly with HTTP connections
                // with Connection: close header and no Content-length
                request_is_inflight || response_is_inflight
            }
            (ConnectionStatus::Normal, ConnectionStatus::Closed) => response_is_inflight,

            (ConnectionStatus::WriteOpen, ConnectionStatus::Normal) => {
                // technically we should keep it open, but we'll assume that if the back
                // is not readable and there is no in flight data back -> front or front -> back, we'll close the session
                request_is_inflight || response_is_inflight
            }
            (ConnectionStatus::WriteOpen, ConnectionStatus::ReadOpen) => true,
            (ConnectionStatus::WriteOpen, ConnectionStatus::WriteOpen) => {
                request_is_inflight || response_is_inflight
            }
            (ConnectionStatus::WriteOpen, ConnectionStatus::Closed) => response_is_inflight,

            (ConnectionStatus::ReadOpen, ConnectionStatus::Normal) => true,
            (ConnectionStatus::ReadOpen, ConnectionStatus::ReadOpen) => false,
            (ConnectionStatus::ReadOpen, ConnectionStatus::WriteOpen) => true,
            (ConnectionStatus::ReadOpen, ConnectionStatus::Closed) => false,

            (ConnectionStatus::Closed, ConnectionStatus::Normal) => request_is_inflight,
            (ConnectionStatus::Closed, ConnectionStatus::ReadOpen) => false,
            (ConnectionStatus::Closed, ConnectionStatus::WriteOpen) => request_is_inflight,
            (ConnectionStatus::Closed, ConnectionStatus::Closed) => false,
        }
    }

    pub fn frontend_hup(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        self.frontend_status = ConnectionStatus::Closed;
        // The frontend hung up: its status is now terminal regardless of
        // which branch we take below (mirrors `backend_hup`).
        debug_assert!(
            matches!(self.frontend_status, ConnectionStatus::Closed),
            "frontend_hup must mark the frontend Closed"
        );
        // EPOLLRDHUP only means the client sent FIN; on a loaded event loop it
        // can coalesce with the payload tail into the same epoll batch, so
        // bytes may still sit in `frontend_buffer` (already read), in the
        // kernel receive buffer (not read yet, signalled by a pending
        // READABLE event), or in the splice `in_pipe` (`splice_in_pending`)
        // — see the sibling `SocketResult::Closed` drains in `readable` and
        // `splice_readable` below and `check_connections`'s
        // `request_is_inflight` (sozu-proxy/sozu#1290, the HUP-path variant
        // of the same close-before-flush truncation).
        let request_is_inflight = self.frontend_buffer.available_data() > 0
            || self.frontend_readiness.event.is_readable()
            || self.splice_in_pending() > 0;
        if request_is_inflight && self.backend_socket.is_some() {
            // Positive space: the branch that keeps the session alive must
            // never be entered without something left to drain.
            debug_assert!(
                self.frontend_buffer.available_data() > 0
                    || self.frontend_readiness.event.is_readable()
                    || self.splice_in_pending() > 0,
                "drain branch entered without any observable in-flight request bytes"
            );
            if self.frontend_readiness.event.is_readable() {
                // Keep reading: the kernel still has the tail of the payload
                // queued behind the FIN. `readable` drains it, possibly in one
                // short read that stops before the EOF (`plain_socket_read`);
                // `frontend_status` is already `Closed`, so the
                // `check_connections` that follows the flush finishes the
                // lifecycle either way, as does a `SocketResult::Closed` read.
                self.frontend_readiness.interest.insert(Ready::READABLE);
            }
            self.backend_readiness.arm_writable();
            debug!(
                "{} Pipe::frontend_hup: frontend connection closed, keeping alive due to inflight request data.",
                log_context!(self)
            );
            SessionResult::Continue
        } else {
            // Negative space: closing outright must never drop bytes still
            // queued for a live backend.
            debug_assert!(
                self.backend_socket.is_none() || self.frontend_buffer.available_data() == 0,
                "close branch entered with backend present but request bytes still queued"
            );
            self.log_request_success(metrics);
            SessionResult::Close
        }
    }

    pub fn backend_hup(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        self.backend_status = ConnectionStatus::Closed;
        // The backend hung up: its status is now terminal regardless of which
        // keep-alive branch we take below.
        debug_assert!(
            matches!(self.backend_status, ConnectionStatus::Closed),
            "backend_hup must mark the backend Closed"
        );
        let pipe_has_data = self.splice_out_pending() > 0;
        if self.backend_buffer.available_data() == 0 && !pipe_has_data {
            // No buffered or in-kernel response data: there is nothing left to
            // drain toward the frontend on this no-data branch.
            debug_assert_eq!(
                self.backend_buffer.available_data(),
                0,
                "no-data branch entered with response bytes still buffered"
            );
            if self.backend_readiness.event.is_readable() {
                self.backend_readiness.interest.insert(Ready::READABLE);
                debug!(
                    "{} Pipe::backend_hup: backend connection closed, keeping alive due to inflight data in kernel.",
                    log_context!(self)
                );
                SessionResult::Continue
            } else {
                match self.flush_frontend_records() {
                    SocketResult::WouldBlock => {
                        // The frontend still owes the client records rustls
                        // absorbed and the socket refused: closing now drops
                        // them. The refusal just consumed the WRITABLE edge,
                        // so only the interest is armed; forcing the event
                        // would make `ready` write, meet the same refusal and
                        // come back here until `MAX_LOOP_ITERATIONS`. The next
                        // real edge runs `writable`, which flushes and then
                        // closes through `check_connections`.
                        self.frontend_readiness.interest.insert(Ready::WRITABLE);
                        self.frontend_readiness.event.remove(Ready::WRITABLE);
                        debug!(
                            "{} Pipe::backend_hup: backend connection closed, keeping alive due to TLS records the frontend socket refused.",
                            log_context!(self)
                        );
                        SessionResult::Continue
                    }
                    SocketResult::Error => {
                        self.log_request_error(metrics, "front socket write error");
                        SessionResult::Close
                    }
                    SocketResult::Continue | SocketResult::Closed => {
                        self.log_request_success(metrics);
                        SessionResult::Close
                    }
                }
            }
        } else {
            debug!(
                "{} Pipe::backend_hup: backend connection closed, keeping alive due to inflight data in buffers.",
                log_context!(self)
            );
            // Interest only, never the event. Every path that queues response
            // bytes (`backend_readable`, `splice_backend_readable`,
            // `arm_inherited_buffer_writes`) has already armed the frontend
            // WRITABLE event, so `writable` has had its chance to run on them.
            // This site runs after `writable` in the same iteration: if that
            // write met EAGAIN it just cleared the event, and setting it again
            // here would defeat the "backend HUP, frontend interested in
            // WRITABLE but no WRITABLE event" break guard in `ready` (and in
            // `TcpSession::ready_inner`, `lib/src/tcp.rs`). The pass would then
            // retry the failing write every iteration until
            // `MAX_LOOP_ITERATIONS` and close the session with the response
            // tail undelivered. Parking instead waits for the client's next
            // EPOLLOUT edge, which sets the event.
            self.frontend_readiness.interest.insert(Ready::WRITABLE);
            if self.backend_readiness.event.is_readable() {
                self.backend_readiness.interest.insert(Ready::READABLE);
            }
            SessionResult::Continue
        }
    }

    /// Hand the records a TLS frontend holds inside rustls to its socket.
    ///
    /// `SocketHandler::socket_write` (`lib/src/socket.rs`) reports plaintext
    /// as written once rustls has absorbed it, and `consume` drops it from
    /// `backend_buffer` at once, so a write that met `WouldBlock` leaves up
    /// to 64 KiB of ciphertext that nothing but another `socket_write` (an
    /// empty one flushes) can send. Returns `Continue` when nothing is left
    /// to send, whatever the handler (a plaintext frontend never wants to
    /// write), and otherwise the result of the flush: `WouldBlock` when the
    /// kernel refused the rest and a later WRITABLE edge must resume,
    /// `Closed` or `Error` when the channel is dead.
    fn flush_frontend_records(&mut self) -> SocketResult {
        if self.frontend.socket_wants_write() {
            self.frontend.socket_write(&[]).1
        } else {
            SocketResult::Continue
        }
    }

    /// Shared tail of `readable`'s and `splice_readable`'s
    /// `SocketResult::Closed` arms: the frontend read side just observed EOF.
    /// Transition `frontend_status` to the half-closed state, clear the
    /// READABLE interest/event (the read side is done), arm the backend
    /// writable when bytes are still queued for it (`has_pending` —
    /// `frontend_buffer.available_data() > 0` for the buffered path,
    /// `splice_in_pending() > 0` for the splice path), and defer teardown to
    /// `check_connections`, which closes only once nothing is left in flight
    /// (mirrors `backend_hup`'s drain branch).
    fn close_frontend_read_side(
        &mut self,
        metrics: &mut SessionMetrics,
        has_pending: bool,
    ) -> SessionResult {
        self.frontend_status = match self.frontend_status {
            ConnectionStatus::Normal => ConnectionStatus::WriteOpen,
            ConnectionStatus::ReadOpen => ConnectionStatus::Closed,
            s => s,
        };
        self.frontend_readiness.event.remove(Ready::READABLE);
        self.frontend_readiness.interest.remove(Ready::READABLE);
        if has_pending && self.backend_socket.is_some() {
            self.backend_readiness.arm_writable();
        }
        if !self.check_connections() {
            self.reset_readiness_for_close();
            self.log_request_success(metrics);
            return SessionResult::Close;
        }
        SessionResult::Continue
    }

    // Read content from the session
    pub fn readable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        // Inherited preread bytes (e.g. SNI ClientHello replay) sit in
        // `frontend_buffer`; splice never drains that userspace buffer, so it
        // must be empty before the fast path takes over.
        #[cfg(all(target_os = "linux", feature = "splice"))]
        if self.protocol == Protocol::TCP
            && self.splice_pipe.is_some()
            && self.frontend_buffer.available_data() == 0
        {
            return self.splice_readable(metrics);
        }

        self.reset_timeouts();

        trace!("{} pipe readable", log_context!(self));
        if self.frontend_buffer.available_space() == 0 {
            self.frontend_readiness.interest.remove(Ready::READABLE);
            self.backend_readiness.arm_writable();
            return SessionResult::Continue;
        }

        let space_before = self.frontend_buffer.available_space();
        let data_before = self.frontend_buffer.available_data();
        let bin_before = metrics.bin;
        let (sz, res) = self.frontend.socket_read(self.frontend_buffer.space());
        // `socket_read` fills `buf[..]` and returns `min(read, buf.len())`; it
        // can never report more bytes than the space slice it was handed.
        debug_assert!(
            sz <= space_before,
            "frontend socket_read reported more bytes ({sz}) than the buffer space offered ({space_before})"
        );
        debug!("{} Read {} bytes", log_context!(self), sz);

        if sz > 0 {
            //FIXME: replace with copy()
            self.frontend_buffer.fill(sz);
            // `fill(sz)` with `sz <= available_space` moves exactly `sz` bytes
            // from free space into readable data — no truncation, no growth.
            debug_assert_eq!(
                self.frontend_buffer.available_data(),
                data_before + sz,
                "fill must grow readable data by exactly the bytes read"
            );

            count!(names::backend::BYTES_IN, sz as i64);
            metrics.bin += sz;
            // Front→proxy ingress metric advances by exactly the bytes read.
            debug_assert_eq!(
                metrics.bin,
                bin_before + sz,
                "metrics.bin must advance by exactly the bytes read"
            );

            if self.frontend_buffer.available_space() == 0 {
                self.frontend_readiness.interest.remove(Ready::READABLE);
            }
            self.backend_readiness.arm_writable();
        } else {
            self.frontend_readiness.event.remove(Ready::READABLE);

            if res == SocketResult::Continue {
                self.frontend_status = match self.frontend_status {
                    ConnectionStatus::Normal => ConnectionStatus::WriteOpen,
                    ConnectionStatus::ReadOpen => ConnectionStatus::Closed,
                    s => s,
                };
            }
        }

        if !self.check_connections() {
            self.reset_readiness_for_close();
            self.log_request_success(metrics);
            return SessionResult::Close;
        }

        match res {
            SocketResult::Error => {
                self.reset_readiness_for_close();
                self.log_request_error(metrics, "front socket read error");
                return SessionResult::Close;
            }
            SocketResult::Closed => {
                // The frontend read side closed (EOF). Bytes it already
                // delivered may still be queued in `frontend_buffer` for the
                // backend; returning `Close` here unconditionally dropped
                // them, silently truncating the stream whenever a front->back
                // tail was still in flight (sozu-proxy/sozu#1279: a
                // payload coalesced with the SNI ClientHello is the
                // reproducer, but the drop hit any plain-TCP upload racing a
                // frontend close). Transition to the half-closed `WriteOpen`
                // status (same mapping as the zero-byte `Continue` read above),
                // arm the backend writable to flush the queue, and defer the
                // teardown to `check_connections`, which closes only once
                // nothing is in flight (mirrors `backend_hup`'s drain branch).
                // `frontend_hup` (EPOLLRDHUP, above) has the same drain
                // requirement for the same reason: FIN can coalesce with the
                // payload tail into one epoll batch on a loaded event loop.
                // So does `splice_readable`'s own `Closed` arm, for bytes
                // sitting in the kernel `in_pipe` instead of `frontend_buffer`.
                let has_pending = self.frontend_buffer.available_data() > 0;
                return self.close_frontend_read_side(metrics, has_pending);
            }
            SocketResult::WouldBlock => {
                self.frontend_readiness.event.remove(Ready::READABLE);
            }
            SocketResult::Continue => {}
        };

        self.backend_readiness.arm_writable();
        SessionResult::Continue
    }

    // Forward content to session
    pub fn writable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        // Mirror of the front-side preread gate: splice never drains the
        // userspace `backend_buffer`, so it must be empty before the fast
        // path takes over.
        #[cfg(all(target_os = "linux", feature = "splice"))]
        if self.protocol == Protocol::TCP
            && self.splice_pipe.is_some()
            && self.backend_buffer.available_data() == 0
        {
            return self.splice_writable(metrics);
        }

        trace!("{} Pipe writable", log_context!(self));
        if self.backend_buffer.available_data() == 0 {
            // Nothing is buffered, but a TLS frontend may still hold records
            // rustls absorbed from an earlier write that met `WouldBlock`.
            // This WRITABLE edge is the one that can send them, and
            // `flush_frontend_records` is the only thing that does:
            // dropping the interest first would strand them until the
            // backend happens to send again.
            let records_pending = self.frontend.socket_wants_write();
            match self.flush_frontend_records() {
                SocketResult::WouldBlock => {
                    // The interest stays, so the next edge resumes the flush.
                    self.frontend_readiness.event.remove(Ready::WRITABLE);
                    return SessionResult::Continue;
                }
                SocketResult::Error => {
                    self.reset_readiness_for_close();
                    self.log_request_error(metrics, "front socket write error");
                    return SessionResult::Close;
                }
                SocketResult::Closed => {
                    self.reset_readiness_for_close();
                    self.log_request_success(metrics);
                    return SessionResult::Close;
                }
                SocketResult::Continue => {}
            }
            self.backend_readiness.interest.insert(Ready::READABLE);
            self.frontend_readiness.interest.remove(Ready::WRITABLE);
            // The records were the last response in flight
            // (`check_connections`), and no later event is guaranteed to end
            // a half-closed session: run the gate every drain runs.
            if records_pending && !self.check_connections() {
                self.reset_readiness_for_close();
                self.log_request_success(metrics);
                return SessionResult::Close;
            }
            return SessionResult::Continue;
        }

        let queued_total = self.backend_buffer.available_data();
        let mut sz = 0usize;
        let mut res = SocketResult::Continue;
        while res == SocketResult::Continue {
            // no more data in buffer, stop here
            if self.backend_buffer.available_data() == 0 {
                count!(names::backend::BYTES_OUT, sz as i64);
                metrics.bout += sz;
                self.backend_readiness.interest.insert(Ready::READABLE);
                self.frontend_readiness.interest.remove(Ready::WRITABLE);
                return SessionResult::Continue;
            }
            let queued = self.backend_buffer.available_data();
            let (current_sz, current_res) = self.frontend.socket_write(self.backend_buffer.data());
            // A partial write can never report more than was queued: the
            // socket writes from `data()` and returns `min(written, data.len())`.
            debug_assert!(
                current_sz <= queued,
                "frontend socket_write reported {current_sz} bytes but only {queued} were queued"
            );
            res = current_res;
            let consumed = self.backend_buffer.consume(current_sz);
            // `consume` drops exactly the written bytes (we already proved
            // `current_sz <= available_data`, so no clamping occurs).
            debug_assert_eq!(
                consumed, current_sz,
                "consume must drop exactly the bytes written to the frontend"
            );
            sz += current_sz;
            // Cumulative transfer never overruns what was queued at entry.
            debug_assert!(
                sz <= queued_total,
                "cumulative frontend write ({sz}) exceeded the queued backend data ({queued_total})"
            );

            if current_sz == 0 && res == SocketResult::Continue {
                self.frontend_status = match self.frontend_status {
                    ConnectionStatus::Normal => ConnectionStatus::ReadOpen,
                    ConnectionStatus::WriteOpen => ConnectionStatus::Closed,
                    s => s,
                };
            }

            if !self.check_connections() {
                metrics.bout += sz;
                count!(names::backend::BYTES_OUT, sz as i64);
                self.reset_readiness_for_close();
                self.log_request_success(metrics);
                return SessionResult::Close;
            }
        }

        if sz > 0 {
            count!(names::backend::BYTES_OUT, sz as i64);
            self.backend_readiness.interest.insert(Ready::READABLE);
            metrics.bout += sz;
        }

        debug!(
            "{} Wrote {} bytes of {}",
            log_context!(self),
            sz,
            self.backend_buffer.available_data()
        );

        match res {
            SocketResult::Error => {
                self.reset_readiness_for_close();
                self.log_request_error(metrics, "front socket write error");
                return SessionResult::Close;
            }
            SocketResult::Closed => {
                self.reset_readiness_for_close();
                self.log_request_success(metrics);
                return SessionResult::Close;
            }
            SocketResult::WouldBlock => {
                self.frontend_readiness.event.remove(Ready::WRITABLE);
            }
            SocketResult::Continue => {}
        }

        SessionResult::Continue
    }

    // Forward content to cluster
    pub fn backend_writable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        // Inherited preread bytes (e.g. SNI ClientHello replay) sit in
        // `frontend_buffer`; splice never drains that userspace buffer, so it
        // must be empty before the fast path takes over.
        #[cfg(all(target_os = "linux", feature = "splice"))]
        if self.protocol == Protocol::TCP
            && self.splice_pipe.is_some()
            && self.frontend_buffer.available_data() == 0
        {
            return self.splice_backend_writable(metrics);
        }

        trace!("{} pipe back_writable", log_context!(self));

        if self.frontend_buffer.available_data() == 0 {
            self.frontend_readiness.interest.insert(Ready::READABLE);
            self.backend_readiness.interest.remove(Ready::WRITABLE);
            return SessionResult::Continue;
        }

        let output_size = self.frontend_buffer.available_data();

        let mut sz = 0usize;
        let mut socket_res = SocketResult::Continue;
        // Set when the loop below exits because `frontend_buffer` just ran
        // dry (as opposed to exiting because of a socket error/close/would-
        // block). `backend` (below) borrows `self.backend_socket` for the
        // whole loop, so the drained-buffer close gate has to live outside
        // this `if let` — it needs `&mut self` for `check_connections` and
        // friends, which would conflict with `backend` if run inline.
        let mut drained = false;

        if let Some(ref mut backend) = self.backend_socket {
            while socket_res == SocketResult::Continue {
                // no more data in buffer, stop here
                if self.frontend_buffer.available_data() == 0 {
                    self.frontend_readiness.interest.insert(Ready::READABLE);
                    self.backend_readiness.interest.remove(Ready::WRITABLE);
                    drained = true;
                    break;
                }

                let queued = self.frontend_buffer.available_data();
                let (current_sz, current_res) = backend.socket_write(self.frontend_buffer.data());
                // A partial write can never report more than was queued.
                debug_assert!(
                    current_sz <= queued,
                    "backend socket_write reported {current_sz} bytes but only {queued} were queued"
                );
                socket_res = current_res;
                let consumed = self.frontend_buffer.consume(current_sz);
                debug_assert_eq!(
                    consumed, current_sz,
                    "consume must drop exactly the bytes written to the backend"
                );
                sz += current_sz;
                // Cumulative transfer never overruns the data queued at entry.
                debug_assert!(
                    sz <= output_size,
                    "cumulative backend write ({sz}) exceeded the queued frontend data ({output_size})"
                );

                if current_sz == 0 && current_res == SocketResult::Continue {
                    self.backend_status = match self.backend_status {
                        ConnectionStatus::Normal => ConnectionStatus::ReadOpen,
                        ConnectionStatus::WriteOpen => ConnectionStatus::Closed,
                        s => s,
                    };
                }
            }
        }

        if drained {
            count!(names::backend::BACK_BYTES_OUT, sz as i64);
            metrics.backend_bout += sz;
            // The queued request bytes just fully drained toward the
            // backend. This early return used to always report `Continue`,
            // even when the frontend read side had already hit EOF
            // (`WriteOpen`/`Closed`, set by `readable`'s/`splice_readable`'s
            // `SocketResult::Closed` arm — sozu-proxy/sozu#1290): the
            // READABLE edge that would have driven a follow-up `readable()`
            // call was already consumed by that arm, so edge-triggered epoll
            // offers no other event to hang the teardown on and the session
            // sat resident until an unrelated event or timeout. Run the same
            // `check_connections` gate every other close goes through so an
            // already-half-closed session tears down as soon as the final
            // queued bytes drain. Healthy sessions (frontend still
            // `Normal`/`ReadOpen`) are unaffected: the gate is skipped
            // entirely and this keeps returning `Continue`, matching the
            // pre-fix behavior for normal keepalive flow.
            if matches!(
                self.frontend_status,
                ConnectionStatus::WriteOpen | ConnectionStatus::Closed
            ) && !self.check_connections()
            {
                self.reset_readiness_for_close();
                self.log_request_success(metrics);
                return SessionResult::Close;
            }
            return SessionResult::Continue;
        }

        let backend_bout_before = metrics.backend_bout;
        count!(names::backend::BACK_BYTES_OUT, sz as i64);
        metrics.backend_bout += sz;
        // Proxy→backend egress metric advances by exactly the bytes written.
        debug_assert_eq!(
            metrics.backend_bout,
            backend_bout_before + sz,
            "metrics.backend_bout must advance by exactly the bytes written"
        );

        if !self.check_connections() {
            self.reset_readiness_for_close();
            self.log_request_success(metrics);
            return SessionResult::Close;
        }

        debug!(
            "{} Wrote {} bytes of {}",
            log_context!(self),
            sz,
            output_size
        );

        match socket_res {
            SocketResult::Error => {
                self.reset_readiness_for_close();
                self.log_request_error(metrics, "back socket write error");
                return SessionResult::Close;
            }
            SocketResult::Closed => {
                self.reset_readiness_for_close();
                self.log_request_success(metrics);
                return SessionResult::Close;
            }
            SocketResult::WouldBlock => {
                self.backend_readiness.event.remove(Ready::WRITABLE);
            }
            SocketResult::Continue => {}
        }
        SessionResult::Continue
    }

    // Read content from cluster
    pub fn backend_readable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        // Mirror of the front-side preread gate: splice never drains the
        // userspace `backend_buffer`, so it must be empty before the fast
        // path takes over.
        #[cfg(all(target_os = "linux", feature = "splice"))]
        if self.protocol == Protocol::TCP
            && self.splice_pipe.is_some()
            && self.backend_buffer.available_data() == 0
        {
            return self.splice_backend_readable(metrics);
        }

        self.reset_timeouts();

        trace!("{} Pipe backend_readable", log_context!(self));
        if self.backend_buffer.available_space() == 0 {
            self.backend_readiness.interest.remove(Ready::READABLE);
            return SessionResult::Continue;
        }

        let space_before = self.backend_buffer.available_space();
        let data_before = self.backend_buffer.available_data();
        let backend_bin_before = metrics.backend_bin;
        if let Some(ref mut backend) = self.backend_socket {
            let (size, remaining) = backend.socket_read(self.backend_buffer.space());
            // `socket_read` reports at most the space slice it was handed.
            debug_assert!(
                size <= space_before,
                "backend socket_read reported more bytes ({size}) than the buffer space offered ({space_before})"
            );
            self.backend_buffer.fill(size);
            // `fill(size)` with `size <= available_space` moves exactly `size`
            // bytes from free space into readable data.
            debug_assert_eq!(
                self.backend_buffer.available_data(),
                data_before + size,
                "fill must grow readable data by exactly the bytes read"
            );

            debug!("{} Read {} bytes", log_context!(self), size);

            if remaining != SocketResult::Continue || size == 0 {
                self.backend_readiness.event.remove(Ready::READABLE);
            }
            if size > 0 {
                self.frontend_readiness.arm_writable();
                count!(names::backend::BACK_BYTES_IN, size as i64);
                metrics.backend_bin += size;
                // Backend→proxy ingress metric advances by exactly bytes read.
                debug_assert_eq!(
                    metrics.backend_bin,
                    backend_bin_before + size,
                    "metrics.backend_bin must advance by exactly the bytes read"
                );
            }

            if size == 0 && remaining == SocketResult::Closed {
                self.backend_status = match self.backend_status {
                    ConnectionStatus::Normal => ConnectionStatus::WriteOpen,
                    ConnectionStatus::ReadOpen => ConnectionStatus::Closed,
                    s => s,
                };

                if !self.check_connections() {
                    self.reset_readiness_for_close();
                    self.log_request_success(metrics);
                    return SessionResult::Close;
                }
            }

            match remaining {
                SocketResult::Error => {
                    self.reset_readiness_for_close();
                    self.log_request_error(metrics, "back socket read error");
                    return SessionResult::Close;
                }
                SocketResult::Closed => {
                    if !self.check_connections() {
                        self.reset_readiness_for_close();
                        self.log_request_success(metrics);
                        return SessionResult::Close;
                    }
                }
                SocketResult::WouldBlock => {
                    self.backend_readiness.event.remove(Ready::READABLE);
                }
                SocketResult::Continue => {}
            }
        }

        SessionResult::Continue
    }

    /// Zero-copy fast path of `readable`: pull bytes off the frontend
    /// socket into the kernel `in_pipe` via `splice(2)`, then mark the
    /// backend writable so the data drains in the next event loop tick.
    ///
    /// Mirrors `readable`'s `ConnectionStatus` transitions and metric
    /// emissions exactly so observability and the `check_connections`
    /// state machine behave the same with or without the feature flag.
    #[cfg(all(target_os = "linux", feature = "splice"))]
    fn splice_readable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        self.reset_timeouts();

        trace!("{} pipe splice_readable", log_context!(self));
        let capacity = self.splice_capacity();
        if self.splice_in_pending() >= capacity {
            // Pipe is full — stop reading and let the backend drain it.
            self.frontend_readiness.interest.remove(Ready::READABLE);
            self.backend_readiness.arm_writable();
            return SessionResult::Continue;
        }

        let pending_before = self.splice_in_pending();
        let bin_before = metrics.bin;
        let pipe_write_end = self.splice_pipe.as_ref().unwrap().in_pipe[1];
        let (sz, res) = splice::splice_in(self.frontend.socket_ref(), pipe_write_end, capacity);
        // `splice_in` is asked for at most `capacity` bytes, so the kernel can
        // never report moving more than that in one call. We deliberately do
        // NOT assert `in_pipe_pending <= capacity`: a kernel pipe buffers well
        // beyond its nominal `F_GETPIPE_SZ` when `splice(2)` moves skb-backed
        // segments — a GRO super-packet on loopback hands a single ring slot far
        // more than a page — so byte-occupancy legitimately exceeds `capacity`.
        // `capacity` is the per-call `len` and a soft backpressure threshold,
        // not a hard occupancy bound.
        debug_assert!(
            sz <= capacity,
            "splice_in reported {sz} bytes but was capped at len {capacity}"
        );
        debug!("{} Spliced {} bytes from frontend", log_context!(self), sz);

        // Same EAGAIN ambiguity as `splice_backend_readable`: with bytes
        // still pending in the in_pipe the kernel may be reporting a full
        // pipe rather than a drained frontend socket. Keep the READABLE
        // event, park the interest until `splice_backend_writable` drains
        // the pipe and re-arms it (mirrors the capacity guard above).
        // This exit skips `check_connections`: the pipe is non-empty so the
        // request is inflight by definition, and `splice_backend_writable`
        // (armed here) runs the check unconditionally on its drain.
        if res == SocketResult::WouldBlock && self.splice_in_pending() > 0 {
            debug_assert_eq!(sz, 0, "splice_in must report 0 bytes on EAGAIN");
            self.frontend_readiness.interest.remove(Ready::READABLE);
            self.backend_readiness.arm_writable();
            debug_assert!(
                self.frontend_readiness.event.is_readable()
                    && !self.frontend_readiness.interest.is_readable(),
                "a pipe-full EAGAIN must keep the READABLE event pending and park the interest"
            );
            debug_assert_eq!(
                self.splice_in_pending(),
                pending_before,
                "a pipe-full EAGAIN must not change the in_pipe accounting"
            );
            return SessionResult::Continue;
        }

        if sz > 0 {
            self.splice_pipe.as_mut().unwrap().in_pipe_pending += sz;
            // Pending advanced by exactly the spliced bytes (tracks real
            // kernel-pipe occupancy; see the capacity note above).
            debug_assert_eq!(
                self.splice_in_pending(),
                pending_before + sz,
                "in_pipe_pending must grow by exactly the spliced bytes"
            );
            count!(names::backend::BYTES_IN, sz as i64);
            metrics.bin += sz;
            debug_assert_eq!(
                metrics.bin,
                bin_before + sz,
                "metrics.bin must advance by exactly the spliced bytes"
            );
            self.backend_readiness.arm_writable();
        } else {
            self.frontend_readiness.event.remove(Ready::READABLE);

            if res == SocketResult::Continue {
                self.frontend_status = match self.frontend_status {
                    ConnectionStatus::Normal => ConnectionStatus::WriteOpen,
                    ConnectionStatus::ReadOpen => ConnectionStatus::Closed,
                    s => s,
                };
            }
        }

        if !self.check_connections() {
            self.reset_readiness_for_close();
            self.log_request_success(metrics);
            return SessionResult::Close;
        }

        match res {
            SocketResult::Error => {
                self.reset_readiness_for_close();
                self.log_request_error(metrics, "splice front socket read error");
                return SessionResult::Close;
            }
            SocketResult::Closed => {
                // The frontend read side closed (EOF). This is the SPLICE
                // sibling of `readable`'s `SocketResult::Closed` drain above
                // (sozu-proxy/sozu#1279) and of `frontend_hup`'s
                // drain branch: bytes already spliced into the kernel
                // `in_pipe` (`splice_in_pending()`) still belong to the
                // backend, and returning `Close` here unconditionally
                // dropped them. Transition to the half-closed `WriteOpen`
                // status, arm the backend writable to drain the kernel pipe
                // via `splice_backend_writable`, and defer teardown to
                // `check_connections`, whose `request_is_inflight` already
                // counts `splice_in_pending() > 0`; once the pipe drains,
                // the next `splice_readable` re-observes EOF with nothing
                // inflight and closes through the `check_connections` gate
                // above this match.
                let has_pending = self.splice_in_pending() > 0;
                return self.close_frontend_read_side(metrics, has_pending);
            }
            SocketResult::WouldBlock => {
                self.frontend_readiness.event.remove(Ready::READABLE);
            }
            SocketResult::Continue => {}
        }

        self.backend_readiness.arm_writable();
        SessionResult::Continue
    }

    /// Zero-copy fast path of `writable`: drain the backend→frontend
    /// kernel `out_pipe` toward the frontend socket via `splice(2)`.
    /// Mirrors `writable`'s loop, status transitions, and metric
    /// emissions.
    #[cfg(all(target_os = "linux", feature = "splice"))]
    fn splice_writable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        trace!("{} Pipe splice_writable", log_context!(self));
        if self.splice_out_pending() == 0 {
            self.backend_readiness.interest.insert(Ready::READABLE);
            self.frontend_readiness.interest.remove(Ready::WRITABLE);
            return SessionResult::Continue;
        }

        let mut sz = 0usize;
        let mut res = SocketResult::Continue;
        while res == SocketResult::Continue {
            let pending = self.splice_out_pending();
            // no more data in pipe, stop here
            if pending == 0 {
                count!(names::backend::BYTES_OUT, sz as i64);
                metrics.bout += sz;
                self.backend_readiness.interest.insert(Ready::READABLE);
                self.frontend_readiness.interest.remove(Ready::WRITABLE);
                return SessionResult::Continue;
            }

            let pipe_read_end = self.splice_pipe.as_ref().unwrap().out_pipe[0];
            let (current_sz, current_res) =
                splice::splice_out(pipe_read_end, self.frontend.socket_ref(), pending);
            // `splice_out` was asked for `pending` bytes and can drain no more
            // than the pipe holds; draining more than `pending` would underflow
            // `out_pipe_pending` below.
            debug_assert!(
                current_sz <= pending,
                "splice_out drained {current_sz} bytes but only {pending} were pending (would underflow)"
            );
            res = current_res;
            if current_sz > 0 {
                self.splice_pipe.as_mut().unwrap().out_pipe_pending -= current_sz;
                debug_assert_eq!(
                    self.splice_out_pending(),
                    pending - current_sz,
                    "out_pipe_pending must shrink by exactly the drained bytes"
                );
            }
            sz += current_sz;

            if current_sz == 0 && res == SocketResult::Continue {
                self.frontend_status = match self.frontend_status {
                    ConnectionStatus::Normal => ConnectionStatus::ReadOpen,
                    ConnectionStatus::WriteOpen => ConnectionStatus::Closed,
                    s => s,
                };
            }

            if !self.check_connections() {
                metrics.bout += sz;
                count!(names::backend::BYTES_OUT, sz as i64);
                self.reset_readiness_for_close();
                self.log_request_success(metrics);
                return SessionResult::Close;
            }
        }

        if sz > 0 {
            count!(names::backend::BYTES_OUT, sz as i64);
            self.backend_readiness.interest.insert(Ready::READABLE);
            metrics.bout += sz;
        }

        debug!(
            "{} Spliced {} bytes (out_pipe_pending={})",
            log_context!(self),
            sz,
            self.splice_out_pending()
        );

        match res {
            SocketResult::Error => {
                self.reset_readiness_for_close();
                self.log_request_error(metrics, "splice front socket write error");
                return SessionResult::Close;
            }
            SocketResult::Closed => {
                self.reset_readiness_for_close();
                self.log_request_success(metrics);
                return SessionResult::Close;
            }
            SocketResult::WouldBlock => {
                self.frontend_readiness.event.remove(Ready::WRITABLE);
            }
            SocketResult::Continue => {}
        }

        SessionResult::Continue
    }

    /// Zero-copy fast path of `backend_writable`: drain the
    /// frontend→backend kernel `in_pipe` toward the backend socket via
    /// `splice(2)`. Mirrors `backend_writable`'s loop, status
    /// transitions, and metric emissions.
    #[cfg(all(target_os = "linux", feature = "splice"))]
    fn splice_backend_writable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        trace!("{} pipe splice_backend_writable", log_context!(self));

        if self.splice_in_pending() == 0 {
            self.frontend_readiness.interest.insert(Ready::READABLE);
            self.backend_readiness.interest.remove(Ready::WRITABLE);
            return SessionResult::Continue;
        }

        let output_size = self.splice_in_pending();
        let mut sz = 0usize;
        let mut socket_res = SocketResult::Continue;

        while socket_res == SocketResult::Continue {
            let pending = self.splice_in_pending();
            // no more data in pipe, stop here
            if pending == 0 {
                self.frontend_readiness.interest.insert(Ready::READABLE);
                self.backend_readiness.interest.remove(Ready::WRITABLE);
                count!(names::backend::BACK_BYTES_OUT, sz as i64);
                metrics.backend_bout += sz;
                // The queued bytes just fully drained out of the kernel
                // `in_pipe`. This early return used to always report
                // `Continue`, even when the frontend read side had already
                // hit EOF (`WriteOpen`/`Closed`, set by `splice_readable`'s
                // `SocketResult::Closed` arm — sozu-proxy/sozu#1290): the
                // READABLE edge that would have driven a follow-up
                // `splice_readable` call was already consumed by that arm, so
                // edge-triggered epoll offers no other event to hang the
                // teardown on and the session sat resident until an
                // unrelated event or timeout. Run the same
                // `check_connections` gate every other close goes through so
                // an already-half-closed session tears down as soon as the
                // final queued bytes drain. Healthy sessions (frontend still
                // `Normal`/`ReadOpen`) are unaffected: the gate is skipped
                // entirely and this keeps returning `Continue`, matching the
                // pre-fix behavior for normal keepalive flow.
                if matches!(
                    self.frontend_status,
                    ConnectionStatus::WriteOpen | ConnectionStatus::Closed
                ) && !self.check_connections()
                {
                    self.reset_readiness_for_close();
                    self.log_request_success(metrics);
                    return SessionResult::Close;
                }
                return SessionResult::Continue;
            }

            let pipe_read_end = self.splice_pipe.as_ref().unwrap().in_pipe[0];
            let (current_sz, current_res) = match self.backend_socket.as_ref() {
                Some(b) => splice::splice_out(pipe_read_end, b, pending),
                None => break,
            };
            // Draining more than `pending` would underflow `in_pipe_pending`.
            debug_assert!(
                current_sz <= pending,
                "splice_out drained {current_sz} bytes but only {pending} were pending (would underflow)"
            );
            socket_res = current_res;
            if current_sz > 0 {
                self.splice_pipe.as_mut().unwrap().in_pipe_pending -= current_sz;
                debug_assert_eq!(
                    self.splice_in_pending(),
                    pending - current_sz,
                    "in_pipe_pending must shrink by exactly the drained bytes"
                );
            }
            sz += current_sz;
            // Cumulative drain never exceeds what was pending at entry.
            debug_assert!(
                sz <= output_size,
                "cumulative splice drain ({sz}) exceeded the bytes pending at entry ({output_size})"
            );

            if current_sz == 0 && current_res == SocketResult::Continue {
                self.backend_status = match self.backend_status {
                    ConnectionStatus::Normal => ConnectionStatus::ReadOpen,
                    ConnectionStatus::WriteOpen => ConnectionStatus::Closed,
                    s => s,
                };
            }
        }

        count!(names::backend::BACK_BYTES_OUT, sz as i64);
        metrics.backend_bout += sz;
        if sz > 0 {
            // A partial drain freed pipe slots: resume the frontend read that
            // `splice_readable` parked on a pipe-full EAGAIN (mirrors
            // `splice_writable`'s re-arm of the backend read).
            self.frontend_readiness.interest.insert(Ready::READABLE);
        }

        if !self.check_connections() {
            self.reset_readiness_for_close();
            self.log_request_success(metrics);
            return SessionResult::Close;
        }

        debug!(
            "{} Spliced {} bytes of {}",
            log_context!(self),
            sz,
            output_size
        );

        match socket_res {
            SocketResult::Error => {
                self.reset_readiness_for_close();
                self.log_request_error(metrics, "splice back socket write error");
                return SessionResult::Close;
            }
            SocketResult::Closed => {
                self.reset_readiness_for_close();
                self.log_request_success(metrics);
                return SessionResult::Close;
            }
            SocketResult::WouldBlock => {
                self.backend_readiness.event.remove(Ready::WRITABLE);
            }
            SocketResult::Continue => {}
        }
        SessionResult::Continue
    }

    /// Zero-copy fast path of `backend_readable`: pull bytes off the
    /// backend socket into the kernel `out_pipe` via `splice(2)`, then
    /// mark the frontend writable so the data drains in the next event
    /// loop tick. Mirrors `backend_readable`'s status transitions and
    /// metric emissions.
    #[cfg(all(target_os = "linux", feature = "splice"))]
    fn splice_backend_readable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        self.reset_timeouts();

        trace!("{} Pipe splice_backend_readable", log_context!(self));
        let capacity = self.splice_capacity();
        if self.splice_out_pending() >= capacity {
            // Pipe is full — stop reading and let the frontend drain it.
            self.backend_readiness.interest.remove(Ready::READABLE);
            self.frontend_readiness.arm_writable();
            return SessionResult::Continue;
        }

        let pending_before = self.splice_out_pending();
        let backend_bin_before = metrics.backend_bin;
        let pipe_write_end = self.splice_pipe.as_ref().unwrap().out_pipe[1];
        let (size, remaining) = match self.backend_socket.as_ref() {
            Some(b) => splice::splice_in(b, pipe_write_end, capacity),
            None => return SessionResult::Continue,
        };
        // `splice_in` is capped at `len = capacity`, so the kernel never reports
        // moving more than that per call. As in `splice_readable`, we do NOT
        // assert `out_pipe_pending <= capacity`: a kernel pipe holds well beyond
        // its nominal `F_GETPIPE_SZ` when `splice(2)` moves skb-backed (GRO)
        // segments, so byte-occupancy legitimately exceeds `capacity` — it is
        // only the per-call `len` and a soft backpressure threshold.
        debug_assert!(
            size <= capacity,
            "splice_in reported {size} bytes but was capped at len {capacity}"
        );

        debug!("{} Spliced {} bytes from backend", log_context!(self), size);

        // `splice(2)` reports EAGAIN both when the socket has nothing left to
        // read and when the pipe has no free slot: a pipe holds
        // `capacity / PAGE_SIZE` slots, every spliced skb fragment takes one
        // whatever its length, and a partially drained slot stays occupied.
        // So the slots run out while `out_pipe_pending < capacity` (MSS-sized
        // segments from a remote backend fill 16 slots at ~23 KiB). An empty
        // pipe always has a free slot, so EAGAIN there is a drained socket;
        // with bytes pending it may be the pipe. Clearing the READABLE event
        // on that ambiguity loses bytes already queued in the socket for
        // good: edge-triggered epoll never re-signals them and the client
        // hangs on the tail of the response. Keep the event, park the
        // interest until `splice_writable` drains the pipe and re-arms it,
        // exactly like the `out_pending >= capacity` guard above.
        let pipe_may_be_full =
            remaining == SocketResult::WouldBlock && self.splice_out_pending() > 0;
        if pipe_may_be_full {
            debug_assert_eq!(size, 0, "splice_in must report 0 bytes on EAGAIN");
            self.backend_readiness.interest.remove(Ready::READABLE);
            self.frontend_readiness.arm_writable();
            debug_assert!(
                self.backend_readiness.event.is_readable()
                    && !self.backend_readiness.interest.is_readable(),
                "a pipe-full EAGAIN must keep the READABLE event pending and park the interest"
            );
            debug_assert_eq!(
                self.splice_out_pending(),
                pending_before,
                "a pipe-full EAGAIN must not change the out_pipe accounting"
            );
            return SessionResult::Continue;
        }
        if remaining != SocketResult::Continue || size == 0 {
            self.backend_readiness.event.remove(Ready::READABLE);
        }
        if size > 0 {
            self.splice_pipe.as_mut().unwrap().out_pipe_pending += size;
            debug_assert_eq!(
                self.splice_out_pending(),
                pending_before + size,
                "out_pipe_pending must grow by exactly the spliced bytes"
            );
            self.frontend_readiness.arm_writable();
            count!(names::backend::BACK_BYTES_IN, size as i64);
            metrics.backend_bin += size;
            debug_assert_eq!(
                metrics.backend_bin,
                backend_bin_before + size,
                "metrics.backend_bin must advance by exactly the spliced bytes"
            );
        }

        if size == 0 && remaining == SocketResult::Closed {
            self.backend_status = match self.backend_status {
                ConnectionStatus::Normal => ConnectionStatus::WriteOpen,
                ConnectionStatus::ReadOpen => ConnectionStatus::Closed,
                s => s,
            };

            if !self.check_connections() {
                self.reset_readiness_for_close();
                self.log_request_success(metrics);
                return SessionResult::Close;
            }
        }

        match remaining {
            SocketResult::Error => {
                self.reset_readiness_for_close();
                self.log_request_error(metrics, "splice back socket read error");
                return SessionResult::Close;
            }
            SocketResult::Closed => {
                if !self.check_connections() {
                    self.reset_readiness_for_close();
                    self.log_request_success(metrics);
                    return SessionResult::Close;
                }
            }
            SocketResult::WouldBlock => {
                self.backend_readiness.event.remove(Ready::READABLE);
            }
            SocketResult::Continue => {}
        }

        SessionResult::Continue
    }

    pub fn log_context(&self) -> LogContext<'_> {
        LogContext {
            session_id: self.session_id,
            request_id: Some(self.request_id),
            cluster_id: self.cluster_id.as_deref(),
            backend_id: self.backend_id.as_deref(),
        }
    }

    fn log_endpoint(&self) -> EndpointRecord<'_> {
        match &self.websocket_context {
            WebSocketContext::Http {
                method,
                authority,
                path,
                status,
                reason,
            } => EndpointRecord::Http {
                method: method.as_deref(),
                authority: authority.as_deref(),
                path: path.as_deref(),
                status: status.to_owned(),
                reason: reason.as_deref(),
            },
            WebSocketContext::Tcp => EndpointRecord::Tcp,
        }
    }
}

impl<Front: SocketHandler, L: ListenerHandler> SessionState for Pipe<Front, L> {
    fn ready(
        &mut self,
        _session: Rc<RefCell<dyn crate::ProxySession>>,
        _proxy: Rc<RefCell<dyn crate::L7Proxy>>,
        metrics: &mut SessionMetrics,
    ) -> SessionResult {
        let mut counter = 0;

        // A client that sends its last frame and half-closes can deliver both
        // as ONE `READABLE | HUP` event (Linux coalesces `EPOLLIN |
        // EPOLLRDHUP`). Closing on the bare HUP dropped that frame, and the
        // session never reached the access log: `frontend_hup` drains what is
        // in flight, or logs and closes when nothing is, exactly as
        // `TcpSession::ready_inner` (`lib/src/tcp.rs`) does for a TCP pipe.
        if self.frontend_readiness.event.is_hup() {
            let session_result = self.frontend_hup(metrics);
            if session_result != SessionResult::Continue {
                return session_result;
            }
            // `frontend_hup` kept the session alive to drain the request
            // bytes. The client already sent its FIN, so edge-triggered epoll
            // offers no further frontend event: clear the consumed HUP bit
            // and let the loop below read the kernel tail and flush
            // `frontend_buffer` in this same pass.
            self.frontend_readiness.event.remove(Ready::HUP);
        }

        while counter < MAX_LOOP_ITERATIONS {
            let frontend_interest = self.frontend_readiness.filter_interest();
            let backend_interest = self.backend_readiness.filter_interest();

            trace!(
                "{} Frontend interest({:?}), backend interest({:?})",
                log_context!(self),
                frontend_interest,
                backend_interest
            );
            if frontend_interest.is_empty() && backend_interest.is_empty() {
                break;
            }

            if self.backend_readiness.event.is_hup()
                && self.frontend_readiness.interest.is_writable()
                && !self.frontend_readiness.event.is_writable()
            {
                break;
            }

            if frontend_interest.is_readable() && self.readable(metrics) == SessionResult::Close {
                return SessionResult::Close;
            }

            if backend_interest.is_writable()
                && self.backend_writable(metrics) == SessionResult::Close
            {
                return SessionResult::Close;
            }

            if backend_interest.is_readable()
                && self.backend_readable(metrics) == SessionResult::Close
            {
                return SessionResult::Close;
            }

            if frontend_interest.is_writable() && self.writable(metrics) == SessionResult::Close {
                return SessionResult::Close;
            }

            if backend_interest.is_hup() && self.backend_hup(metrics) == SessionResult::Close {
                return SessionResult::Close;
            }

            if frontend_interest.is_error() {
                error!(
                    "{} Frontend socket error, disconnecting",
                    log_context!(self)
                );

                self.frontend_readiness.interest = Ready::EMPTY;
                self.backend_readiness.interest = Ready::EMPTY;

                // Every other exit of this loop ends the session through a
                // handler that logs it: this one must too, or the session
                // never reaches the access log or the end-of-session metrics.
                self.log_request(metrics, true, Some("frontend socket error"));
                return SessionResult::Close;
            }

            if backend_interest.is_error() && self.backend_hup(metrics) == SessionResult::Close {
                self.frontend_readiness.interest = Ready::EMPTY;
                self.backend_readiness.interest = Ready::EMPTY;

                error!("{} Backend socket error, disconnecting", log_context!(self));
                return SessionResult::Close;
            }

            counter += 1;
        }

        if counter >= MAX_LOOP_ITERATIONS {
            error!(
                "{}\tHandling session went through {} iterations, there's a probable infinite loop bug, closing the connection",
                log_context!(self),
                MAX_LOOP_ITERATIONS
            );

            incr!(names::http::INFINITE_LOOP_ERROR);
            self.print_state(self.protocol_string());
            self.log_request(metrics, true, Some("session loop limit reached"));

            return SessionResult::Close;
        }

        SessionResult::Continue
    }

    fn update_readiness(&mut self, token: Token, events: Ready) {
        if self.frontend_token == token {
            self.frontend_readiness.event |= events;
        } else if self.backend_token == Some(token) {
            self.backend_readiness.event |= events;
        }
    }

    fn timeout(&mut self, token: Token, metrics: &mut SessionMetrics) -> StateResult {
        //info!("got timeout for token: {:?}", token);
        if self.frontend_token == token {
            self.log_request_timeout(metrics, "frontend socket timeout");
            if let Some(timeout) = self.container_frontend_timeout.as_mut() {
                timeout.triggered()
            }
            return StateResult::CloseSession;
        }

        if self.backend_token == Some(token) {
            //info!("backend timeout triggered for token {:?}", token);
            if let Some(timeout) = self.container_backend_timeout.as_mut() {
                timeout.triggered()
            }

            self.log_request_timeout(metrics, "backend socket timeout");
            return StateResult::CloseSession;
        }

        error!("{} Got timeout for an invalid token", log_context!(self));
        self.log_request_error(metrics, "invalid token timeout");
        StateResult::CloseSession
    }

    fn cancel_timeouts(&mut self) {
        self.container_frontend_timeout.as_mut().map(|t| t.cancel());
        self.container_backend_timeout.as_mut().map(|t| t.cancel());
    }

    fn close(&mut self, _proxy: Rc<RefCell<dyn L7Proxy>>, _metrics: &mut SessionMetrics) {
        if let Some(backend) = self.backend.as_mut() {
            let mut backend = backend.borrow_mut();
            backend.active_requests = backend.active_requests.saturating_sub(1);
        }

        // The TLS teardown the session's own `close` expects from its state
        // (`HttpsSession::close` in `lib/src/https.rs` shuts the TCP write
        // side right after): RFC 8446 section 6.1 wants a `close_notify`
        // before it, and the alert and the records still inside rustls only
        // leave through a flush, as the mux does on close. Bounded, because
        // this runs inside the single-threaded event loop; whatever the
        // kernel still refuses is reported. A plaintext frontend (TCP,
        // `ws://`) has neither a close nor a record to flush, so this is a
        // flag write for it.
        let (records_pending, drain_rounds) =
            drain_tls_close_notify(&mut self.frontend, &mut self.close_notify_sent);
        if records_pending {
            warn!(
                "{} Pipe TLS records NOT fully flushed on close: drain_rounds={}, frontend_readiness={:?}",
                log_context!(self),
                drain_rounds,
                self.frontend_readiness
            );
        }
    }

    fn print_state(&self, context: &str) {
        error!(
            "\
{} {} Session(Pipe)
\tFrontend:
\t\ttoken: {:?}\treadiness: {:?}
\tBackend:
\t\ttoken: {:?}\treadiness: {:?}",
            log_context!(self),
            context,
            self.frontend_token,
            self.frontend_readiness,
            self.backend_token,
            self.backend_readiness
        );
    }

    /// A WebSocket pipe is targeted by the backend it was upgraded on.
    fn targets(&self, target: &crate::SessionTarget) -> bool {
        self.cluster_id.as_deref().is_some_and(|cluster_id| {
            target.matches(cluster_id, self.backend_id.as_deref(), self.backend_address)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        io::Write,
        net::{TcpListener as StdTcpListener, TcpStream as StdTcpStream},
        time::Duration,
    };

    use super::*;
    use crate::{pool::Pool, socket::FrontRustls};

    struct TestListener {
        address: SocketAddr,
    }

    impl ListenerHandler for TestListener {
        fn get_addr(&self) -> &SocketAddr {
            &self.address
        }

        fn get_tags(&self, _key: &str) -> Option<&sozu_command::logging::CachedTags> {
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

    fn connected_pair() -> (StdTcpStream, StdTcpStream) {
        let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let address = listener.local_addr().expect("listener local addr");
        let client = StdTcpStream::connect(address).expect("connect test client");
        let (server, _) = listener.accept().expect("accept test server");
        client.set_nonblocking(true).expect("client nonblocking");
        server.set_nonblocking(true).expect("server nonblocking");
        (client, server)
    }

    #[test]
    fn backend_readable_arms_frontend_writable_event_when_buffering_response() {
        let (frontend_peer, frontend_socket) = connected_pair();
        let (mut backend_peer, backend_socket) = connected_pair();

        let mut pool = Pool::with_capacity(2, 2, 4096);
        let backend_buffer = pool.checkout().expect("backend buffer");
        let frontend_buffer = pool.checkout().expect("frontend buffer");
        let address = "127.0.0.1:0".parse().expect("test address");
        let listener = Rc::new(RefCell::new(TestListener { address }));

        let mut pipe = Pipe::new(
            backend_buffer,
            None,
            Some(TcpStream::from_std(backend_socket)),
            None,
            None,
            None,
            None,
            frontend_buffer,
            Token(0),
            TcpStream::from_std(frontend_socket),
            listener,
            Protocol::HTTP,
            Ulid::generate(),
            Ulid::generate(),
            None,
            WebSocketContext::Tcp,
        );
        pipe.set_back_token(Token(1));
        pipe.frontend_readiness.event = Ready::EMPTY;
        pipe.frontend_readiness.interest = Ready::READABLE | Ready::HUP | Ready::ERROR;
        pipe.backend_readiness.event = Ready::READABLE;
        pipe.backend_readiness.interest = Ready::READABLE | Ready::HUP | Ready::ERROR;

        backend_peer
            .write_all(b"server-speaks-first")
            .expect("write backend payload");

        let mut metrics = SessionMetrics::new(Some(Duration::ZERO));
        assert_eq!(pipe.backend_readable(&mut metrics), SessionResult::Continue);

        assert!(
            pipe.backend_buffer.available_data() > 0,
            "backend_readable must buffer backend bytes"
        );
        assert!(
            pipe.frontend_readiness.interest.is_writable(),
            "buffered backend bytes must arm frontend WRITABLE interest"
        );
        assert!(
            pipe.frontend_readiness.event.is_writable(),
            "buffered backend bytes must queue a frontend WRITABLE event"
        );

        drop(frontend_peer);
    }

    #[test]
    fn frontend_readable_arms_backend_writable_event_when_buffering_request() {
        let (mut frontend_peer, frontend_socket) = connected_pair();
        let (backend_peer, backend_socket) = connected_pair();

        let mut pool = Pool::with_capacity(2, 2, 4096);
        let backend_buffer = pool.checkout().expect("backend buffer");
        let frontend_buffer = pool.checkout().expect("frontend buffer");
        let address = "127.0.0.1:0".parse().expect("test address");
        let listener = Rc::new(RefCell::new(TestListener { address }));

        let mut pipe = Pipe::new(
            backend_buffer,
            None,
            Some(TcpStream::from_std(backend_socket)),
            None,
            None,
            None,
            None,
            frontend_buffer,
            Token(0),
            TcpStream::from_std(frontend_socket),
            listener,
            Protocol::HTTP,
            Ulid::generate(),
            Ulid::generate(),
            None,
            WebSocketContext::Tcp,
        );
        pipe.set_back_token(Token(1));
        pipe.frontend_readiness.event = Ready::READABLE;
        pipe.frontend_readiness.interest = Ready::READABLE | Ready::HUP | Ready::ERROR;
        pipe.backend_readiness.event = Ready::EMPTY;
        pipe.backend_readiness.interest = Ready::READABLE | Ready::HUP | Ready::ERROR;

        frontend_peer
            .write_all(b"client-speaks-after-upgrade")
            .expect("write frontend payload");

        let mut metrics = SessionMetrics::new(Some(Duration::ZERO));
        assert_eq!(pipe.readable(&mut metrics), SessionResult::Continue);

        assert!(
            pipe.frontend_buffer.available_data() > 0,
            "readable must buffer frontend bytes"
        );
        assert!(
            pipe.backend_readiness.interest.is_writable(),
            "buffered frontend bytes must arm backend WRITABLE interest"
        );
        assert!(
            pipe.backend_readiness.event.is_writable(),
            "buffered frontend bytes must queue a backend WRITABLE event"
        );

        drop(backend_peer);
    }

    #[test]
    fn restore_readiness_events_rearms_inherited_buffered_writes() {
        let (frontend_peer, frontend_socket) = connected_pair();
        let (backend_peer, backend_socket) = connected_pair();

        let mut pool = Pool::with_capacity(2, 2, 4096);
        let mut backend_buffer = pool.checkout().expect("backend buffer");
        let mut frontend_buffer = pool.checkout().expect("frontend buffer");
        backend_buffer
            .write_all(b"backend bytes inherited from 101 read")
            .expect("write backend buffer");
        frontend_buffer
            .write_all(b"frontend bytes inherited from upgrade read")
            .expect("write frontend buffer");
        let address = "127.0.0.1:0".parse().expect("test address");
        let listener = Rc::new(RefCell::new(TestListener { address }));

        let mut pipe = Pipe::new(
            backend_buffer,
            None,
            Some(TcpStream::from_std(backend_socket)),
            None,
            None,
            None,
            None,
            frontend_buffer,
            Token(0),
            TcpStream::from_std(frontend_socket),
            listener,
            Protocol::HTTP,
            Ulid::generate(),
            Ulid::generate(),
            None,
            WebSocketContext::Tcp,
        );

        pipe.restore_readiness_events(Ready::EMPTY, Ready::EMPTY);

        assert!(
            pipe.frontend_readiness.event.is_writable(),
            "restoring inherited frontend events must not park backend-buffered bytes"
        );
        assert!(
            pipe.backend_readiness.event.is_writable(),
            "restoring inherited backend events must not park frontend-buffered bytes"
        );

        drop(frontend_peer);
        drop(backend_peer);
    }

    /// Regression guard for the splice/preread interaction: when `Pipe` is
    /// constructed with the splice fast path available (`Protocol::TCP`)
    /// AND a non-empty inherited `frontend_buffer` (the SNI-preread
    /// ClientHello replay scenario), `backend_writable` must drain those
    /// buffered bytes to the backend socket through the normal buffered
    /// path first. Without the gate in `backend_writable`, this call would
    /// dispatch straight to `splice_backend_writable`, which only drains the
    /// kernel pipe (`splice_in_pending`) — 0 here — and returns immediately
    /// without ever touching `frontend_buffer`, silently dropping the
    /// preread bytes.
    #[cfg(all(target_os = "linux", feature = "splice"))]
    #[test]
    fn backend_writable_drains_inherited_frontend_buffer_before_splice_engages() {
        use std::io::Read;

        let (frontend_peer, frontend_socket) = connected_pair();
        let (mut backend_peer, backend_socket) = connected_pair();

        let mut pool = Pool::with_capacity(2, 2, 4096);
        let backend_buffer = pool.checkout().expect("backend buffer");
        let mut frontend_buffer = pool.checkout().expect("frontend buffer");
        frontend_buffer
            .write_all(b"inherited-preread-client-hello")
            .expect("write inherited frontend buffer");
        let address = "127.0.0.1:0".parse().expect("test address");
        let listener = Rc::new(RefCell::new(TestListener { address }));

        let mut pipe = Pipe::new(
            backend_buffer,
            None,
            Some(TcpStream::from_std(backend_socket)),
            None,
            None,
            None,
            None,
            frontend_buffer,
            Token(0),
            TcpStream::from_std(frontend_socket),
            listener,
            Protocol::TCP,
            Ulid::generate(),
            Ulid::generate(),
            None,
            WebSocketContext::Tcp,
        );
        pipe.set_back_token(Token(1));

        assert!(
            pipe.splice_pipe.is_some(),
            "Protocol::TCP must allocate the splice kernel pipe for this test to be meaningful"
        );
        assert!(
            pipe.frontend_buffer.available_data() > 0,
            "test setup must inherit a non-empty frontend buffer"
        );

        let mut metrics = SessionMetrics::new(Some(Duration::ZERO));
        assert_eq!(pipe.backend_writable(&mut metrics), SessionResult::Continue);

        assert_eq!(
            pipe.frontend_buffer.available_data(),
            0,
            "inherited preread bytes must drain through the buffered path before splice engages"
        );

        let mut received = [0u8; 64];
        let n = backend_peer
            .read(&mut received)
            .expect("backend socket must have received the drained preread bytes");
        assert_eq!(&received[..n], b"inherited-preread-client-hello");

        drop(frontend_peer);
    }

    /// Regression guard for the close-before-flush data loss
    /// (sozu-proxy/sozu#1279): when the frontend read side reaches
    /// EOF while `frontend_buffer` still holds bytes queued for the backend,
    /// `readable` must NOT return `Close` (which discarded them, silently
    /// truncating the stream -- the reproducer was a payload coalesced with an
    /// SNI ClientHello). It must keep the session alive to drain, then the
    /// queued bytes must reach the backend and only then may the session
    /// close.
    #[test]
    fn frontend_eof_flushes_queued_backend_bytes_before_closing() {
        use std::io::{Read, Write};

        let (frontend_peer, frontend_socket) = connected_pair();
        let (mut backend_peer, backend_socket) = connected_pair();

        let mut pool = Pool::with_capacity(2, 2, 4096);
        let backend_buffer = pool.checkout().expect("backend buffer");
        let mut frontend_buffer = pool.checkout().expect("frontend buffer");
        // Bytes the frontend already delivered, still queued for the backend.
        frontend_buffer
            .write_all(b"front-to-back-tail-still-queued")
            .expect("seed queued frontend bytes");
        let address = "127.0.0.1:0".parse().expect("test address");
        let listener = Rc::new(RefCell::new(TestListener { address }));

        let mut pipe = Pipe::new(
            backend_buffer,
            None,
            Some(TcpStream::from_std(backend_socket)),
            None,
            None,
            None,
            None,
            frontend_buffer,
            Token(0),
            TcpStream::from_std(frontend_socket),
            listener,
            Protocol::TCP,
            Ulid::generate(),
            Ulid::generate(),
            None,
            WebSocketContext::Tcp,
        );
        pipe.set_back_token(Token(1));
        pipe.frontend_readiness.interest = Ready::READABLE | Ready::HUP | Ready::ERROR;
        pipe.frontend_readiness.event = Ready::READABLE;

        // The frontend closes right after delivering its bytes (no gap): the
        // pipe reads EOF while `frontend_buffer` is still non-empty.
        drop(frontend_peer);

        // Nonblocking EOF is not observable on the very first read on every
        // platform, so retry `readable` (bounded, no sleep) until it observes
        // the close. `readable` must never report `Close` while bytes remain
        // queued.
        let mut metrics = SessionMetrics::new(Some(Duration::ZERO));
        let mut saw_eof = false;
        for _ in 0..100_000 {
            let result = pipe.readable(&mut metrics);
            assert_ne!(
                result,
                SessionResult::Close,
                "readable must not close while {} queued backend bytes remain",
                pipe.frontend_buffer.available_data()
            );
            if matches!(pipe.frontend_status, ConnectionStatus::WriteOpen) {
                saw_eof = true;
                break;
            }
        }
        assert!(
            saw_eof,
            "frontend EOF was never observed within the retry budget"
        );
        assert!(
            pipe.frontend_buffer.available_data() > 0,
            "the queued backend bytes must survive the frontend EOF, not be dropped"
        );

        // Draining now delivers every queued byte to the backend. Nothing
        // else is in flight (backend still `Normal`) and the frontend read
        // side already hit EOF (`WriteOpen`), so `backend_writable` closes
        // the session itself as soon as the drain completes -- it does not
        // wait for a follow-up event that edge-triggered epoll would never
        // deliver (sozu-proxy/sozu#1290).
        assert_eq!(pipe.backend_writable(&mut metrics), SessionResult::Close);
        assert_eq!(
            pipe.frontend_buffer.available_data(),
            0,
            "the queued bytes must all drain to the backend after EOF"
        );

        let mut received = Vec::new();
        // Read until we have the full payload (a single read may segment).
        for _ in 0..100_000 {
            let mut buf = [0u8; 64];
            match backend_peer.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    received.extend_from_slice(&buf[..n]);
                    if received.len() >= b"front-to-back-tail-still-queued".len() {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(_) => break,
            }
        }
        assert_eq!(
            received, b"front-to-back-tail-still-queued",
            "the backend must receive the queued tail byte-for-byte, not a truncation"
        );
    }

    /// Regression guard for the missing completion edge on the deferred
    /// close (sozu-proxy/sozu#1290): the test above proves the queued bytes
    /// survive the frontend's EOF; this proves the other half of the fix --
    /// that draining those bytes closes the session immediately, from that
    /// single `backend_writable` event, with no extra event injected by the
    /// test. Before the fix, `backend_writable`'s drained-buffer early
    /// return always reported `Continue`, even with the frontend already at
    /// EOF and nothing else in flight: edge-triggered epoll had already
    /// consumed the READABLE/HUP edge that signalled the EOF, so no other
    /// event existed to hang a follow-up close decision on and the session
    /// sat resident until an unrelated event or timeout.
    #[test]
    fn frontend_eof_with_queued_bytes_closes_on_the_draining_backend_writable_call() {
        use std::io::Read;

        let (mut frontend_peer, frontend_socket) = connected_pair();
        let (mut backend_peer, backend_socket) = connected_pair();

        let mut pool = Pool::with_capacity(2, 2, 4096);
        let backend_buffer = pool.checkout().expect("backend buffer");
        let frontend_buffer = pool.checkout().expect("frontend buffer");
        let address = "127.0.0.1:0".parse().expect("test address");
        let listener = Rc::new(RefCell::new(TestListener { address }));

        let mut pipe = Pipe::new(
            backend_buffer,
            None,
            Some(TcpStream::from_std(backend_socket)),
            None,
            None,
            None,
            None,
            frontend_buffer,
            Token(0),
            TcpStream::from_std(frontend_socket),
            listener,
            Protocol::HTTP,
            Ulid::generate(),
            Ulid::generate(),
            None,
            WebSocketContext::Tcp,
        );
        pipe.set_back_token(Token(1));
        pipe.frontend_readiness.interest = Ready::READABLE | Ready::HUP | Ready::ERROR;
        pipe.frontend_readiness.event = Ready::READABLE;

        // The frontend delivers its bytes then closes right away -- no gap
        // -- so the real `readable()` call below observes EOF while bytes
        // are still queued in `frontend_buffer`.
        frontend_peer
            .write_all(b"queued-request-tail")
            .expect("write frontend payload");
        drop(frontend_peer);

        // Drive the REAL ready pass: keep calling `readable` (bounded, no
        // sleep) until it observes the close, exactly like the event loop
        // would after mio reports the frontend's HUP/EOF.
        let mut metrics = SessionMetrics::new(Some(Duration::ZERO));
        let mut saw_eof = false;
        for _ in 0..100_000 {
            let result = pipe.readable(&mut metrics);
            assert_ne!(
                result,
                SessionResult::Close,
                "readable must not close while queued backend bytes remain"
            );
            if matches!(pipe.frontend_status, ConnectionStatus::WriteOpen) {
                saw_eof = true;
                break;
            }
        }
        assert!(
            saw_eof,
            "frontend EOF was never observed within the retry budget"
        );
        assert!(
            pipe.frontend_buffer.available_data() > 0,
            "the queued bytes must survive the frontend EOF"
        );
        assert!(
            pipe.backend_readiness.interest.is_writable()
                && pipe.backend_readiness.event.is_writable(),
            "observing EOF with queued bytes must arm backend WRITABLE to drain them"
        );

        // The backend becomes writable exactly once -- the real event the
        // arming above promises -- and drains everything in that single
        // call. No extra readable()/frontend_hup()/check_connections() call
        // is injected here: the session must close from THIS event alone.
        let result = pipe.backend_writable(&mut metrics);

        assert_eq!(
            result,
            SessionResult::Close,
            "the single draining backend_writable call must close the session by itself -- \
             edge-triggered epoll already consumed the EOF edge and offers no other event to \
             hang the teardown on"
        );
        assert_eq!(
            pipe.frontend_buffer.available_data(),
            0,
            "the queued bytes must have fully drained before the session closed"
        );

        let mut received = Vec::new();
        for _ in 0..100_000 {
            let mut buf = [0u8; 64];
            match backend_peer.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    received.extend_from_slice(&buf[..n]);
                    if received.len() >= b"queued-request-tail".len() {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(_) => break,
            }
        }
        assert_eq!(
            received, b"queued-request-tail",
            "the backend must still receive the queued bytes byte-for-byte before the close"
        );
    }

    /// Regression guard for the HUP-path sibling of the close-before-flush
    /// data loss (sozu-proxy/sozu#1290): `command/src/ready.rs`'s
    /// `From<&mio::event::Event>` sets `Ready::HUP` whenever
    /// `is_read_closed()`/`is_write_closed()` fires, independently of
    /// `Ready::READABLE` -- so a client FIN that coalesces with the payload
    /// tail on a loaded event loop delivers a SINGLE epoll batch carrying
    /// BOTH bits. `frontend_hup` must not drop bytes still queued in
    /// `frontend_buffer` (already read) or still pending in the kernel
    /// receive buffer (signalled by the retained READABLE event) just
    /// because HUP also fired.
    #[test]
    fn frontend_hup_drains_inflight_request_bytes_before_closing() {
        let (frontend_peer, frontend_socket) = connected_pair();
        let (_backend_peer, backend_socket) = connected_pair();

        let mut pool = Pool::with_capacity(2, 2, 4096);
        let backend_buffer = pool.checkout().expect("backend buffer");
        let mut frontend_buffer = pool.checkout().expect("frontend buffer");
        frontend_buffer
            .write_all(b"front-to-back-tail-still-queued")
            .expect("seed queued frontend bytes");
        let address = "127.0.0.1:0".parse().expect("test address");
        let listener = Rc::new(RefCell::new(TestListener { address }));

        let mut pipe = Pipe::new(
            backend_buffer,
            None,
            Some(TcpStream::from_std(backend_socket)),
            None,
            None,
            None,
            None,
            frontend_buffer,
            Token(0),
            TcpStream::from_std(frontend_socket),
            listener,
            Protocol::TCP,
            Ulid::generate(),
            Ulid::generate(),
            None,
            WebSocketContext::Tcp,
        );
        pipe.set_back_token(Token(1));
        // The kernel receive buffer still has a tail behind the FIN: both
        // bits set in the same batch, exactly as `ready.rs` would produce.
        pipe.frontend_readiness.interest = Ready::READABLE | Ready::HUP | Ready::ERROR;
        pipe.frontend_readiness.event = Ready::READABLE | Ready::HUP;
        pipe.backend_readiness.interest = Ready::READABLE | Ready::HUP | Ready::ERROR;
        pipe.backend_readiness.event = Ready::EMPTY;

        let mut metrics = SessionMetrics::new(Some(Duration::ZERO));
        let result = pipe.frontend_hup(&mut metrics);

        assert_eq!(
            result,
            SessionResult::Continue,
            "frontend_hup must keep the session alive while request bytes are still in flight"
        );
        assert!(
            matches!(pipe.frontend_status, ConnectionStatus::Closed),
            "frontend_hup must still mark the frontend Closed even on the drain branch"
        );
        assert!(
            pipe.backend_readiness.interest.is_writable()
                && pipe.backend_readiness.event.is_writable(),
            "the drain branch must arm backend WRITABLE to flush the queued frontend bytes"
        );
        assert!(
            pipe.frontend_readiness.interest.is_readable(),
            "the drain branch must retain frontend READABLE interest to read the kernel tail to EOF"
        );

        drop(frontend_peer);
    }

    /// Sibling of the above: when nothing is in flight (no buffered bytes,
    /// no pending READABLE event), `frontend_hup` keeps its pre-existing
    /// behavior of closing immediately -- there is nothing left to drain.
    #[test]
    fn frontend_hup_closes_immediately_when_nothing_is_inflight() {
        let (frontend_peer, frontend_socket) = connected_pair();
        let (_backend_peer, backend_socket) = connected_pair();

        let mut pool = Pool::with_capacity(2, 2, 4096);
        let backend_buffer = pool.checkout().expect("backend buffer");
        let frontend_buffer = pool.checkout().expect("frontend buffer");
        let address = "127.0.0.1:0".parse().expect("test address");
        let listener = Rc::new(RefCell::new(TestListener { address }));

        let mut pipe = Pipe::new(
            backend_buffer,
            None,
            Some(TcpStream::from_std(backend_socket)),
            None,
            None,
            None,
            None,
            frontend_buffer,
            Token(0),
            TcpStream::from_std(frontend_socket),
            listener,
            Protocol::TCP,
            Ulid::generate(),
            Ulid::generate(),
            None,
            WebSocketContext::Tcp,
        );
        pipe.set_back_token(Token(1));
        // HUP only, no readable event, no queued bytes: nothing to drain.
        pipe.frontend_readiness.interest = Ready::READABLE | Ready::HUP | Ready::ERROR;
        pipe.frontend_readiness.event = Ready::HUP;
        pipe.backend_readiness.interest = Ready::READABLE | Ready::HUP | Ready::ERROR;
        pipe.backend_readiness.event = Ready::EMPTY;

        let mut metrics = SessionMetrics::new(Some(Duration::ZERO));
        let result = pipe.frontend_hup(&mut metrics);

        assert_eq!(
            result,
            SessionResult::Close,
            "frontend_hup must close immediately when nothing is in flight (unchanged legacy behavior)"
        );
        assert!(
            matches!(pipe.frontend_status, ConnectionStatus::Closed),
            "frontend_hup must mark the frontend Closed"
        );
        assert!(
            !pipe.backend_readiness.event.is_writable(),
            "the close branch must not arm backend WRITABLE when there is nothing queued"
        );

        drop(frontend_peer);
    }

    /// Regression guard for the SPLICE sibling of the close-before-flush
    /// data loss (sozu-proxy/sozu#1279 / #1290): `splice_readable`'s
    /// `SocketResult::Closed` arm used to return `Close` unconditionally,
    /// dropping whatever `splice_in_pending()` bytes sat in the kernel
    /// `in_pipe` when the frontend's FIN was observed. It must instead keep
    /// the session alive until `splice_backend_writable` drains the kernel
    /// pipe, then close on the next EOF re-observation once nothing is
    /// inflight.
    #[cfg(all(target_os = "linux", feature = "splice"))]
    #[test]
    fn splice_readable_eof_drains_kernel_pipe_bytes_before_closing() {
        use std::io::Read;

        let (mut frontend_peer, frontend_socket) = connected_pair();
        let (mut backend_peer, backend_socket) = connected_pair();

        let mut pool = Pool::with_capacity(2, 2, 4096);
        let backend_buffer = pool.checkout().expect("backend buffer");
        // The frontend buffer stays EMPTY: the splice fast path only engages
        // when no inherited userspace bytes remain (see `readable`'s gate).
        let frontend_buffer = pool.checkout().expect("frontend buffer");
        let address = "127.0.0.1:0".parse().expect("test address");
        let listener = Rc::new(RefCell::new(TestListener { address }));

        let mut pipe = Pipe::new(
            backend_buffer,
            None,
            Some(TcpStream::from_std(backend_socket)),
            None,
            None,
            None,
            None,
            frontend_buffer,
            Token(0),
            TcpStream::from_std(frontend_socket),
            listener,
            Protocol::TCP,
            Ulid::generate(),
            Ulid::generate(),
            None,
            WebSocketContext::Tcp,
        );
        pipe.set_back_token(Token(1));
        pipe.frontend_readiness.interest = Ready::READABLE | Ready::HUP | Ready::ERROR;
        pipe.frontend_readiness.event = Ready::READABLE;

        assert!(
            pipe.splice_pipe.is_some(),
            "Protocol::TCP must allocate the splice kernel pipe for this test to be meaningful"
        );

        // 32 KiB: below the 64 KiB default pipe capacity so the whole
        // payload fits the kernel in_pipe without backpressure pauses.
        let payload = vec![0x5a_u8; 32 * 1024];
        // Environment precondition: once this user's pipe buffers exceed
        // `fs.pipe-user-pages-soft`, Linux creates every new pipe with 2 pages
        // (8 KiB) and refuses `F_SETPIPE_SZ` with EPERM, so `splice_readable`
        // parks at its capacity guard and the payload can never sit whole in
        // the in_pipe. Fail loudly and name the host limit rather than report
        // a false regression.
        let capacity = pipe.splice_capacity();
        assert!(
            capacity >= payload.len(),
            "environment precondition not met: splice pipe capacity is {capacity} bytes, \
             below the {}-byte payload; this user's pipe buffers likely exceed \
             fs.pipe-user-pages-soft ({}). Close pipe-heavy processes or raise the limit, \
             then rerun: this is a host limit, not a Sozu regression",
            payload.len(),
            std::fs::read_to_string("/proc/sys/fs/pipe-user-pages-soft")
                .map(|limit| limit.trim().to_owned())
                .unwrap_or_else(|error| format!("unreadable: {error}")),
        );
        frontend_peer
            .write_all(&payload)
            .expect("write frontend payload");

        // Splice the payload into the kernel in_pipe (bounded retries:
        // loopback delivers the payload in several chunks, and an early
        // call can observe WouldBlock).
        let mut metrics = SessionMetrics::new(Some(Duration::ZERO));
        for _ in 0..100_000 {
            if pipe.splice_in_pending() >= payload.len() {
                break;
            }
            let result = pipe.splice_readable(&mut metrics);
            assert_ne!(
                result,
                SessionResult::Close,
                "splice_readable must not close while splicing the payload in"
            );
        }
        assert_eq!(
            pipe.splice_in_pending(),
            payload.len(),
            "the whole payload must sit in the kernel in_pipe before EOF"
        );

        // The frontend closes right after its payload: FIN behind the
        // spliced bytes.
        drop(frontend_peer);

        // Keep calling until EOF is observed (WriteOpen transition). The
        // buggy arm returned `Close` here, dropping the kernel-pipe bytes.
        let mut saw_eof = false;
        for _ in 0..100_000 {
            let result = pipe.splice_readable(&mut metrics);
            assert_ne!(
                result,
                SessionResult::Close,
                "splice_readable must not close while {} kernel-pipe bytes remain",
                pipe.splice_in_pending()
            );
            if matches!(pipe.frontend_status, ConnectionStatus::WriteOpen) {
                saw_eof = true;
                break;
            }
        }
        assert!(
            saw_eof,
            "frontend EOF was never observed within the retry budget"
        );
        assert_eq!(
            pipe.splice_in_pending(),
            payload.len(),
            "the kernel-pipe bytes must survive the frontend EOF, not be dropped"
        );
        assert!(
            pipe.backend_readiness.interest.is_writable()
                && pipe.backend_readiness.event.is_writable(),
            "EOF with kernel-pipe bytes pending must arm backend WRITABLE to drain them"
        );

        // Drain the kernel pipe to the backend and read the peer: the bytes
        // must arrive byte-identical (interleave drain + read so a full
        // backend socket buffer cannot deadlock the loop). Once the kernel
        // pipe is fully drained with the frontend already at EOF, the fix
        // under test (sozu-proxy/sozu#1290) closes the session as part of
        // that same drain call -- so `drain == Close` is now expected, but
        // ONLY once nothing is left pending; closing any earlier would be
        // the mid-flight truncation this test guards against. Keep reading
        // from `backend_peer` regardless of the session's reported status:
        // the bytes were already handed off to the kernel by `splice_out`,
        // independently of whether `Pipe` still considers itself open.
        let mut received = Vec::with_capacity(payload.len());
        let mut closed_during_drain = false;
        for _ in 0..100_000 {
            let drain = pipe.splice_backend_writable(&mut metrics);
            if drain == SessionResult::Close {
                assert_eq!(
                    pipe.splice_in_pending(),
                    0,
                    "the session must only close once the kernel pipe is fully drained, \
                     never mid-flight"
                );
                closed_during_drain = true;
            }
            let mut buf = [0u8; 16384];
            match backend_peer.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => received.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => break,
            }
            if received.len() >= payload.len() {
                break;
            }
        }
        assert_eq!(
            received, payload,
            "the backend must receive the spliced tail byte-for-byte, not a truncation"
        );
        assert_eq!(
            pipe.splice_in_pending(),
            0,
            "the kernel in_pipe must be fully drained"
        );
        assert!(
            closed_during_drain,
            "the fix under test (sozu-proxy/sozu#1290) must close the session as soon as \
             the kernel pipe drains, without waiting for a separate re-observation event"
        );

        // Nothing inflight and the frontend is half-closed: the session was
        // already closed above, during the drain itself. Re-observing EOF
        // here proves that closed state is stable under a repeat call,
        // rather than being the sole mechanism that closes the session (as
        // it was before the fix).
        assert!(
            !pipe.check_connections(),
            "with nothing inflight and the frontend closed, the session must be closeable"
        );
        assert_eq!(
            pipe.splice_readable(&mut metrics),
            SessionResult::Close,
            "re-observing EOF with nothing inflight must (still) close the session"
        );
    }

    /// Regression guard for the splice fast path: `splice(2)` from a socket
    /// into a pipe returns `EAGAIN` both when the *socket* has nothing left
    /// to read AND when the *pipe* has no free slot. A pipe holds
    /// `capacity / PAGE_SIZE` slots, every spliced skb fragment occupies one
    /// slot whatever its length, and a partially drained slot stays occupied.
    /// So the slots run out while `out_pipe_pending < capacity` (MSS-sized
    /// segments from a remote backend: 16 slots x ~1400 B, far below 64 KiB).
    /// Treating that `EAGAIN` as a drained socket clears the READABLE event;
    /// edge-triggered epoll never re-signals bytes already queued in the
    /// socket, so the tail of the response sits in the kernel recv queue
    /// forever and the client hangs at the end of the transfer.
    ///
    /// The test shrinks the pipe to a single 4 KiB slot and drains one byte
    /// of it so the slot is still held: the next `splice_in` hits `EAGAIN`
    /// with bytes still queued in the backend socket.
    #[cfg(all(target_os = "linux", feature = "splice"))]
    #[test]
    fn splice_backend_readable_keeps_readable_event_when_pipe_slots_are_exhausted() {
        use std::os::unix::io::AsRawFd;

        fn unread_bytes(socket: &TcpStream) -> usize {
            let mut n: libc::c_int = 0;
            let ret = unsafe { libc::ioctl(socket.as_raw_fd(), libc::FIONREAD, &mut n) };
            assert_eq!(ret, 0, "FIONREAD must succeed");
            n as usize
        }

        let (_frontend_peer, frontend_socket) = connected_pair();
        let (mut backend_peer, backend_socket) = connected_pair();

        let mut pool = Pool::with_capacity(2, 2, 4096);
        let backend_buffer = pool.checkout().expect("backend buffer");
        let frontend_buffer = pool.checkout().expect("frontend buffer");
        let address = "127.0.0.1:0".parse().expect("test address");
        let listener = Rc::new(RefCell::new(TestListener { address }));

        let mut pipe = Pipe::new(
            backend_buffer,
            None,
            Some(TcpStream::from_std(backend_socket)),
            None,
            None,
            None,
            None,
            frontend_buffer,
            Token(0),
            TcpStream::from_std(frontend_socket),
            listener,
            Protocol::TCP,
            Ulid::generate(),
            Ulid::generate(),
            None,
            WebSocketContext::Tcp,
        );
        pipe.set_back_token(Token(1));
        pipe.frontend_readiness.interest = Ready::READABLE | Ready::HUP | Ready::ERROR;
        pipe.backend_readiness.interest = Ready::READABLE | Ready::HUP | Ready::ERROR;
        pipe.backend_readiness.event = Ready::READABLE;

        // Shrink both kernel pipes to a single page: one slot each.
        let slot = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        assert!(slot > 0, "sysconf(_SC_PAGESIZE) must succeed");
        {
            let splice_pipe = pipe
                .splice_pipe
                .as_mut()
                .expect("Protocol::TCP must allocate the splice kernel pipe");
            for fd in [splice_pipe.in_pipe[0], splice_pipe.out_pipe[0]] {
                let ret = unsafe { libc::fcntl(fd, libc::F_SETPIPE_SZ, slot as libc::c_int) };
                assert_eq!(ret, slot as libc::c_int, "F_SETPIPE_SZ(page) must succeed");
            }
            splice_pipe.capacity = slot;
        }
        let capacity = pipe.splice_capacity();
        assert_eq!(capacity, slot);

        // Two slots' worth of response bytes queued in the backend socket.
        let payload = vec![0x5a_u8; 2 * slot];
        backend_peer
            .write_all(&payload)
            .expect("write backend payload");
        for _ in 0..1000 {
            if unread_bytes(pipe.backend_socket.as_ref().unwrap()) >= payload.len() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            unread_bytes(pipe.backend_socket.as_ref().unwrap()),
            payload.len(),
            "the whole payload must be queued in the backend socket"
        );

        // First pass fills the single slot exactly: byte accounting agrees
        // with the kernel and the guard `out_pending >= capacity` engages.
        let mut metrics = SessionMetrics::new(Some(Duration::ZERO));
        assert_ne!(
            pipe.splice_backend_readable(&mut metrics),
            SessionResult::Close
        );
        let first = pipe.splice_out_pending();
        assert!(
            first > 0 && first <= slot,
            "the single slot must absorb part of the payload (got {first})"
        );
        assert!(
            pipe.backend_readiness.event.is_readable(),
            "the READABLE event must survive a successful splice"
        );

        // Partial drain toward the frontend (a short write): the slot is
        // still occupied but `out_pipe_pending` is now below `capacity`.
        {
            let pipe_read_end = pipe.splice_pipe.as_ref().unwrap().out_pipe[0];
            let (drained, res) = splice::splice_out(pipe_read_end, pipe.frontend.socket_ref(), 1);
            assert_eq!((drained, res), (1, SocketResult::Continue));
            pipe.splice_pipe.as_mut().unwrap().out_pipe_pending -= drained;
        }
        assert!(pipe.splice_out_pending() < capacity);

        // Second pass: `splice_in` hits EAGAIN because the pipe has no free
        // slot, NOT because the socket is drained.
        pipe.backend_readiness.interest.insert(Ready::READABLE);
        assert_ne!(
            pipe.splice_backend_readable(&mut metrics),
            SessionResult::Close
        );

        let still_queued = unread_bytes(pipe.backend_socket.as_ref().unwrap());
        assert_eq!(
            still_queued,
            payload.len() - first,
            "precondition: the rest of the payload must still sit in the socket"
        );
        assert!(
            pipe.backend_readiness.event.is_readable(),
            "READABLE event must survive a pipe-full EAGAIN: {still_queued} bytes are still \
             queued in the backend socket and edge-triggered epoll will never re-signal them"
        );
        assert!(
            !pipe.backend_readiness.interest.is_readable(),
            "READABLE interest must be parked while the pipe is full, or the ready loop spins"
        );
        assert!(
            pipe.frontend_readiness.event.is_writable()
                && pipe.frontend_readiness.interest.is_writable(),
            "the frontend must be armed writable to drain the pipe"
        );

        // Recovery: draining the pipe toward the frontend re-arms the
        // backend READABLE interest; with the event still pending, the next
        // pass splices the second slot's worth of bytes out of the socket
        // without any new epoll edge.
        assert_ne!(pipe.splice_writable(&mut metrics), SessionResult::Close);
        assert_eq!(
            pipe.splice_out_pending(),
            0,
            "the pipe must be fully drained"
        );
        assert!(
            pipe.backend_readiness.interest.is_readable()
                && pipe.backend_readiness.event.is_readable(),
            "draining the pipe must re-arm the backend read"
        );
        assert_ne!(
            pipe.splice_backend_readable(&mut metrics),
            SessionResult::Close
        );
        assert!(
            unread_bytes(pipe.backend_socket.as_ref().unwrap()) < still_queued,
            "the remaining response bytes must start leaving the backend socket"
        );
        assert!(pipe.splice_out_pending() > 0);

        // Drain socket -> pipe -> frontend until the socket is empty.
        for _ in 0..1000 {
            if unread_bytes(pipe.backend_socket.as_ref().unwrap()) == 0 {
                break;
            }
            assert_ne!(pipe.splice_writable(&mut metrics), SessionResult::Close);
            pipe.backend_readiness.interest.insert(Ready::READABLE);
            assert_ne!(
                pipe.splice_backend_readable(&mut metrics),
                SessionResult::Close
            );
        }
        assert_eq!(unread_bytes(pipe.backend_socket.as_ref().unwrap()), 0);
        if pipe.splice_out_pending() == 0 {
            // Re-create the ambiguous state: one slot's worth in the pipe.
            backend_peer.write_all(&payload[..slot]).expect("write");
            for _ in 0..1000 {
                if unread_bytes(pipe.backend_socket.as_ref().unwrap()) >= slot {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            pipe.backend_readiness.interest.insert(Ready::READABLE);
            pipe.backend_readiness.event.insert(Ready::READABLE);
            assert_ne!(
                pipe.splice_backend_readable(&mut metrics),
                SessionResult::Close
            );
            assert_eq!(unread_bytes(pipe.backend_socket.as_ref().unwrap()), 0);
        }
        assert!(pipe.splice_out_pending() > 0);

        // Socket empty and pipe non-empty: EAGAIN is now ambiguous again,
        // the event stays pending and the interest is parked until the
        // next drain, after which the genuine drained-socket EAGAIN (empty
        // pipe) clears the event.
        pipe.backend_readiness.interest.insert(Ready::READABLE);
        assert_ne!(
            pipe.splice_backend_readable(&mut metrics),
            SessionResult::Close
        );
        assert!(pipe.backend_readiness.event.is_readable());
        assert!(!pipe.backend_readiness.interest.is_readable());
        assert_ne!(pipe.splice_writable(&mut metrics), SessionResult::Close);
        assert_eq!(pipe.splice_out_pending(), 0);
        assert_ne!(
            pipe.splice_backend_readable(&mut metrics),
            SessionResult::Close
        );
        assert!(
            !pipe.backend_readiness.event.is_readable(),
            "a drained socket with an empty pipe must clear the READABLE event"
        );
    }

    #[cfg(all(target_os = "linux", feature = "splice"))]
    fn exercise_splice_readable_pipe_full_recovery(force_growth_refusal: bool) -> bool {
        use std::{io::Read, os::unix::io::AsRawFd};

        fn unread_bytes(socket: &TcpStream) -> usize {
            let mut n: libc::c_int = 0;
            let ret = unsafe { libc::ioctl(socket.as_raw_fd(), libc::FIONREAD, &mut n) };
            assert_eq!(ret, 0, "FIONREAD must succeed");
            n as usize
        }

        fn discard_in_pipe(pipe: &mut Pipe<TcpStream, TestListener>) {
            let pending = pipe.splice_in_pending();
            let pipe_read_end = pipe.splice_pipe.as_ref().unwrap().in_pipe[0];
            let mut discarded = vec![0_u8; pending];
            let mut offset = 0;
            while offset < pending {
                // SAFETY: `pipe_read_end` remains owned by `pipe`, and the
                // writable slice covers exactly the bytes still requested.
                let read = unsafe {
                    libc::read(
                        pipe_read_end,
                        discarded[offset..].as_mut_ptr().cast(),
                        pending - offset,
                    )
                };
                assert!(
                    read > 0,
                    "read must drain the populated pipe: {}",
                    std::io::Error::last_os_error()
                );
                offset += read as usize;
            }
            pipe.splice_pipe.as_mut().unwrap().in_pipe_pending = 0;
        }

        fn request_pipe_capacity(
            fd: libc::c_int,
            requested: usize,
            force_growth_refusal: bool,
        ) -> usize {
            // SAFETY: `fd` is a live pipe descriptor owned by the test's
            // `SplicePipe`; fcntl neither retains the descriptor nor a pointer.
            let requested = if force_growth_refusal {
                -1
            } else {
                libc::c_int::try_from(requested).expect("requested pipe size must fit c_int")
            };
            let set_ret = unsafe { libc::fcntl(fd, libc::F_SETPIPE_SZ, requested) };
            if force_growth_refusal {
                assert_eq!(set_ret, -1, "negative pipe growth must be refused");
            } else if set_ret < 0 {
                eprintln!(
                    "F_SETPIPE_SZ({requested}) unavailable for test pipe: {}",
                    std::io::Error::last_os_error()
                );
            }
            // SAFETY: same live pipe descriptor as above.
            let actual = unsafe { libc::fcntl(fd, libc::F_GETPIPE_SZ) };
            assert!(actual > 0, "F_GETPIPE_SZ must succeed");
            actual as usize
        }

        fn set_buffer(socket: &impl AsRawFd, opt: libc::c_int, bytes: libc::c_int) {
            let ret = unsafe {
                libc::setsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    opt,
                    &bytes as *const libc::c_int as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                )
            };
            assert_eq!(ret, 0, "setsockopt must succeed");
        }

        /// A connected pair whose buffers are pinned tiny *before* the
        /// handshake: SO_RCVBUF on the listener (inherited by the accepted
        /// socket, so the advertised window starts small) and SO_SNDBUF on
        /// the connecting side. Together they bound how many bytes the leg
        /// can absorb at once to a few KiB, with no autotuning and no
        /// dependence on window-update timing.
        fn tiny_backend_pair() -> (StdTcpStream, StdTcpStream) {
            let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind test listener");
            set_buffer(&listener, libc::SO_RCVBUF, 4096);
            let address = listener.local_addr().expect("listener local addr");
            let socket = socket2_connect_with_sndbuf(address, 4096);
            let (peer, _) = listener.accept().expect("accept test server");
            socket.set_nonblocking(true).expect("client nonblocking");
            peer.set_nonblocking(true).expect("server nonblocking");
            (peer, socket)
        }

        fn socket2_connect_with_sndbuf(address: SocketAddr, bytes: libc::c_int) -> StdTcpStream {
            use std::os::unix::io::FromRawFd;
            let fd =
                unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
            assert!(fd >= 0, "socket() must succeed");
            let socket = unsafe { StdTcpStream::from_raw_fd(fd) };
            set_buffer(&socket, libc::SO_SNDBUF, bytes);
            let SocketAddr::V4(v4) = address else {
                panic!("test listener must be IPv4")
            };
            let sin = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: v4.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(v4.ip().octets()),
                },
                sin_zero: [0; 8],
            };
            let ret = unsafe {
                libc::connect(
                    fd,
                    &sin as *const libc::sockaddr_in as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            };
            assert_eq!(
                ret,
                0,
                "connect() must succeed: {}",
                std::io::Error::last_os_error()
            );
            socket
        }

        let (mut frontend_peer, frontend_socket) = connected_pair();
        let (mut backend_peer, backend_socket) = tiny_backend_pair();

        let mut pool = Pool::with_capacity(2, 2, 4096);
        let backend_buffer = pool.checkout().expect("backend buffer");
        let frontend_buffer = pool.checkout().expect("frontend buffer");
        let address = "127.0.0.1:0".parse().expect("test address");
        let listener = Rc::new(RefCell::new(TestListener { address }));

        let mut pipe = Pipe::new(
            backend_buffer,
            None,
            Some(TcpStream::from_std(backend_socket)),
            None,
            None,
            None,
            None,
            frontend_buffer,
            Token(0),
            TcpStream::from_std(frontend_socket),
            listener,
            Protocol::TCP,
            Ulid::generate(),
            Ulid::generate(),
            None,
            WebSocketContext::Tcp,
        );
        pipe.set_back_token(Token(1));
        pipe.frontend_readiness.interest = Ready::READABLE | Ready::HUP | Ready::ERROR;
        pipe.frontend_readiness.event = Ready::READABLE;
        pipe.backend_readiness.interest = Ready::READABLE | Ready::HUP | Ready::ERROR;

        let slot = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        assert!(slot > 0, "sysconf(_SC_PAGESIZE) must succeed");
        {
            let splice_pipe = pipe
                .splice_pipe
                .as_mut()
                .expect("Protocol::TCP must allocate the splice kernel pipe");
            for fd in [splice_pipe.in_pipe[0], splice_pipe.out_pipe[0]] {
                let ret = unsafe { libc::fcntl(fd, libc::F_SETPIPE_SZ, slot as libc::c_int) };
                assert_eq!(ret, slot as libc::c_int, "F_SETPIPE_SZ(page) must succeed");
            }
            splice_pipe.capacity = slot;
        }

        // The mandatory drain/retry consumes at most two slots in total. A
        // third keeps bytes queued for the independent multi-slot phase.
        let payload = vec![0x5a_u8; 3 * slot];
        frontend_peer
            .write_all(&payload)
            .expect("write frontend payload");
        for _ in 0..1000 {
            if unread_bytes(pipe.frontend.socket_ref()) >= payload.len() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(unread_bytes(pipe.frontend.socket_ref()), payload.len());

        let mut metrics = SessionMetrics::new(Some(Duration::ZERO));
        assert_ne!(pipe.splice_readable(&mut metrics), SessionResult::Close);
        let first = pipe.splice_in_pending();
        assert!(
            first > 0 && first <= slot,
            "one slot absorbed (got {first})"
        );

        // Partial drain toward the backend keeps the slot occupied.
        {
            let pipe_read_end = pipe.splice_pipe.as_ref().unwrap().in_pipe[0];
            let backend = pipe.backend_socket.as_ref().unwrap();
            let (drained, res) = splice::splice_out(pipe_read_end, backend, 1);
            assert_eq!((drained, res), (1, SocketResult::Continue));
            pipe.splice_pipe.as_mut().unwrap().in_pipe_pending -= drained;
        }

        pipe.frontend_readiness.interest.insert(Ready::READABLE);
        assert_ne!(pipe.splice_readable(&mut metrics), SessionResult::Close);
        let still_queued = unread_bytes(pipe.frontend.socket_ref());
        assert_eq!(still_queued, payload.len() - first);
        assert!(
            pipe.frontend_readiness.event.is_readable(),
            "READABLE event must survive a pipe-full EAGAIN with {still_queued} bytes queued"
        );
        assert!(
            !pipe.frontend_readiness.interest.is_readable(),
            "READABLE interest must be parked while the pipe is full"
        );
        assert!(
            pipe.backend_readiness.event.is_writable()
                && pipe.backend_readiness.interest.is_writable(),
            "the backend must be armed writable to drain the pipe"
        );

        // Mandatory recovery: drain through the production path until the
        // single slot is free. This must re-arm the frontend read while the
        // READABLE event preserved above drives the retry, with no readiness
        // injected by the test.
        let mut backend_buf = [0_u8; 16 * 1024];
        for _ in 0..1000 {
            assert_ne!(
                pipe.splice_backend_writable(&mut metrics),
                SessionResult::Close
            );
            loop {
                match backend_peer.read(&mut backend_buf) {
                    Ok(0) => panic!("backend peer closed while draining the pipe"),
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) => panic!("failed to read drained pipe bytes: {error}"),
                }
            }
            if pipe.splice_in_pending() == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            pipe.splice_in_pending(),
            0,
            "the single-slot pipe must fully drain"
        );
        assert!(
            pipe.frontend_readiness.interest.is_readable()
                && pipe.frontend_readiness.event.is_readable(),
            "the real backend drain must re-arm the preserved frontend read"
        );
        let unread_before_retry = unread_bytes(pipe.frontend.socket_ref());
        assert_eq!(unread_before_retry, still_queued);
        assert_ne!(pipe.splice_readable(&mut metrics), SessionResult::Close);
        let unread_after_retry = unread_bytes(pipe.frontend.socket_ref());
        assert!(
            unread_after_retry < unread_before_retry,
            "the preserved READABLE event must resume frontend progress after the drain \
             ({unread_before_retry} before, {unread_after_retry} after)"
        );
        assert!(pipe.splice_in_pending() > 0);

        // Empty the recovered slot before starting the independent partial-
        // drain phase, then request sixteen page-derived slots. Pipe growth is
        // a capability rather than a production requirement, so use the
        // realised size and skip only this phase when the kernel denies it.
        discard_in_pipe(&mut pipe);
        let requested_capacity = slot.checked_mul(16).expect("pipe size must fit usize");
        let capacity = {
            let splice_pipe = pipe.splice_pipe.as_mut().unwrap();
            let in_capacity = request_pipe_capacity(
                splice_pipe.in_pipe[0],
                requested_capacity,
                force_growth_refusal,
            );
            let out_capacity = request_pipe_capacity(
                splice_pipe.out_pipe[0],
                requested_capacity,
                force_growth_refusal,
            );
            let capacity = in_capacity.min(out_capacity);
            splice_pipe.capacity = capacity;
            capacity
        };
        let minimum_capacity = slot
            .checked_mul(2)
            .expect("minimum pipe size must fit usize")
            .max(32 * 1024);
        if capacity < minimum_capacity {
            eprintln!(
                "skipping multi-slot partial-drain phase: realised capacity {capacity} is below \
                 required {minimum_capacity}"
            );
            return false;
        }

        // Fill the pipe well past what the backend leg can absorb (tiny
        // pinned SO_SNDBUF + SO_RCVBUF, a few KiB in total), so that a drain
        // can only move part of it.
        frontend_peer
            .write_all(&vec![0x5a_u8; capacity])
            .expect("write more frontend payload");
        for _ in 0..1000 {
            if pipe.splice_in_pending() >= capacity / 2 {
                break;
            }
            pipe.frontend_readiness.interest.insert(Ready::READABLE);
            pipe.frontend_readiness.event.insert(Ready::READABLE);
            assert_ne!(pipe.splice_readable(&mut metrics), SessionResult::Close);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            pipe.splice_in_pending() >= capacity / 2,
            "the pipe must hold at least half its realised capacity (got {})",
            pipe.splice_in_pending()
        );
        pipe.frontend_readiness.interest.remove(Ready::READABLE);

        // One drain: the backend leg absorbs a few KiB (its pinned buffers)
        // then EAGAINs, leaving most of the pipe pending -- a partial drain
        // with no timing involved.
        let pending_before_drain = pipe.splice_in_pending();
        assert_ne!(
            pipe.splice_backend_writable(&mut metrics),
            SessionResult::Close
        );
        let pending = pipe.splice_in_pending();
        assert!(
            pending < pending_before_drain,
            "the backend must accept some bytes ({pending_before_drain} pending)"
        );
        assert!(
            pending > 0,
            "the drain must be partial, not complete (accepted {})",
            pending_before_drain - pending
        );
        assert!(
            pipe.frontend_readiness.interest.is_readable(),
            "a partial drain of the pipe must re-arm the frontend read"
        );

        // Re-arming is the partial-drain contract. Data progress is a separate
        // assertion: release every occupied slot first, then compare FIONREAD
        // immediately around the retry so no stale pre-write baseline is used.
        discard_in_pipe(&mut pipe);
        for _ in 0..1000 {
            if unread_bytes(pipe.frontend.socket_ref()) > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let unread_before_retry = unread_bytes(pipe.frontend.socket_ref());
        assert!(
            unread_before_retry > 0,
            "request bytes must remain queued for the free-slot retry"
        );
        assert!(
            pipe.frontend_readiness.interest.is_readable()
                && pipe.frontend_readiness.event.is_readable(),
            "the partial backend drain must leave the frontend retry runnable"
        );
        assert_ne!(pipe.splice_readable(&mut metrics), SessionResult::Close);
        let unread_after_retry = unread_bytes(pipe.frontend.socket_ref());
        assert!(
            unread_after_retry < unread_before_retry,
            "a retry with free pipe slots must consume request bytes \
             ({unread_before_retry} before, {unread_after_retry} after)"
        );
        assert!(pipe.splice_in_pending() > 0);
        true
    }

    /// Frontend-direction twin of the test above: `splice_readable` must
    /// keep the READABLE event on a pipe-full EAGAIN, and a *partial*
    /// `splice_backend_writable` drain must re-arm the frontend read (the
    /// response direction already did so via `splice_writable`).
    #[cfg(all(target_os = "linux", feature = "splice"))]
    #[test]
    fn splice_readable_keeps_readable_event_when_pipe_slots_are_exhausted() {
        exercise_splice_readable_pipe_full_recovery(false);
    }

    #[cfg(all(target_os = "linux", feature = "splice"))]
    #[test]
    fn splice_readable_recovers_when_pipe_growth_is_refused() {
        assert!(!exercise_splice_readable_pipe_full_recovery(true));
    }
    /// The access log's backend address is the one recorded when the backend
    /// was chosen, never a `getpeername(2)` on the backend socket: that call
    /// cost one syscall per session, and failed with `ENOTCONN` (logging no
    /// address) on a backend still connecting or already reset.
    ///
    /// The backend socket here is refused: nothing listens on its port, so
    /// on loopback it is reset before the first assertion and `peer_addr`
    /// can only fail. The address must still be reported, first from the
    /// `Backend` handed to `Pipe::new` (the WebSocket upgrade path), then from
    /// `set_backend_address` (the TCP proxy's dial, overwritten on a retry).
    ///
    /// To SEE THIS RED: make `Pipe::get_backend_address` return
    /// `self.backend_socket.as_ref().and_then(|backend| backend.peer_addr().ok())`
    /// again. The first assertion then fails with `None`.
    #[test]
    fn backend_address_is_reported_without_asking_the_socket() {
        let refused = {
            let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind a throwaway listener");
            listener
                .local_addr()
                .expect("throwaway listener local addr")
        };
        let backend_socket = TcpStream::connect(refused).expect("a nonblocking connect starts");
        let (_frontend_peer, frontend_socket) = connected_pair();

        let mut pool = Pool::with_capacity(2, 2, 4096);
        let backend_buffer = pool.checkout().expect("backend buffer");
        let frontend_buffer = pool.checkout().expect("frontend buffer");
        let address = "127.0.0.1:0".parse().expect("test address");
        let listener = Rc::new(RefCell::new(TestListener { address }));
        let backend = Rc::new(RefCell::new(Backend::new(
            "backend-refused",
            refused,
            None,
            None,
            None,
        )));

        let mut pipe = Pipe::new(
            backend_buffer,
            Some("backend-refused".to_owned()),
            Some(backend_socket),
            Some(backend),
            None,
            None,
            None,
            frontend_buffer,
            Token(0),
            TcpStream::from_std(frontend_socket),
            listener,
            Protocol::HTTP,
            Ulid::generate(),
            Ulid::generate(),
            None,
            WebSocketContext::Tcp,
        );
        assert_eq!(
            pipe.get_backend_address(),
            Some(refused),
            "the address of the backend handed to Pipe::new must be reported \
             even though its socket was refused"
        );

        let retried: SocketAddr = "192.0.2.7:4242".parse().expect("retry address");
        pipe.set_backend_address(Some(retried));
        assert_eq!(
            pipe.get_backend_address(),
            Some(retried),
            "a later dial must replace the reported backend address"
        );
    }

    // ------------------------------------------------------------------
    // WebSocket pipe teardown: frontend hang-up and unflushed TLS records
    // ------------------------------------------------------------------

    /// `SessionState::ready` ignores the session and proxy handles it is
    /// given (the same stand-ins as the expect stage's tests), so neither
    /// answers anything.
    struct UnusedSession;

    impl crate::ProxySession for UnusedSession {
        fn protocol(&self) -> Protocol {
            unreachable!("Pipe::ready never asks the session")
        }
        fn ready(
            &mut self,
            _session: Rc<RefCell<dyn crate::ProxySession>>,
        ) -> crate::SessionIsToBeClosed {
            unreachable!("Pipe::ready never asks the session")
        }
        fn update_readiness(&mut self, _token: Token, _events: Ready) {
            unreachable!("Pipe::ready never asks the session")
        }
        fn close(&mut self) {
            unreachable!("Pipe::ready never asks the session")
        }
        fn timeout(&mut self, _t: Token) -> crate::SessionIsToBeClosed {
            unreachable!("Pipe::ready never asks the session")
        }
        fn last_event(&self) -> std::time::Instant {
            unreachable!("Pipe::ready never asks the session")
        }
        fn print_session(&self) {
            unreachable!("Pipe::ready never asks the session")
        }
        fn frontend_token(&self) -> Token {
            unreachable!("Pipe::ready never asks the session")
        }
        fn shutting_down(&mut self) -> crate::SessionIsToBeClosed {
            unreachable!("Pipe::ready never asks the session")
        }
    }

    struct UnusedProxy;

    impl crate::L7Proxy for UnusedProxy {
        fn kind(&self) -> sozu_command::proto::command::ListenerType {
            unreachable!("Pipe::ready never asks the proxy")
        }
        fn register_socket(
            &self,
            _socket: &mut TcpStream,
            _token: Token,
            _interest: mio::Interest,
        ) -> Result<(), std::io::Error> {
            unreachable!("Pipe::ready never asks the proxy")
        }
        fn add_session(&self, _session: Rc<RefCell<dyn crate::ProxySession>>) -> Token {
            unreachable!("Pipe::ready never asks the proxy")
        }
        fn remove_session(&self, _token: Token) -> bool {
            unreachable!("Pipe::ready never asks the proxy")
        }
        fn clusters(
            &self,
        ) -> &std::collections::HashMap<ClusterId, sozu_command::proto::command::Cluster> {
            unreachable!("Pipe::ready never asks the proxy")
        }
        fn sessions(&self) -> Rc<RefCell<crate::server::SessionManager>> {
            unreachable!("Pipe::ready never asks the proxy")
        }
    }

    type UnusedHandles = (
        Rc<RefCell<dyn crate::ProxySession>>,
        Rc<RefCell<dyn crate::L7Proxy>>,
    );

    fn unused_handles() -> UnusedHandles {
        (
            Rc::new(RefCell::new(UnusedSession)),
            Rc::new(RefCell::new(UnusedProxy)),
        )
    }

    /// The endpoint a WebSocket pipe carries after an HTTP upgrade, as
    /// `HttpsSession::upgrade_mux` and its plaintext twin build it.
    fn websocket_endpoint() -> WebSocketContext {
        WebSocketContext::Http {
            method: None,
            authority: None,
            path: None,
            status: None,
            reason: None,
        }
    }

    /// The `revents` `poll(2)` reports for `fd` once any of `events` is set,
    /// or after `timeout`.
    fn poll_revents(fd: std::os::fd::RawFd, events: libc::c_short, timeout: Duration) -> i16 {
        let mut pollfd = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        // SAFETY: one valid `pollfd` borrowed for the duration of the call.
        let ready = unsafe { libc::poll(&mut pollfd, 1, timeout.as_millis() as libc::c_int) };
        assert!(
            ready >= 0,
            "poll(2) failed: {}",
            std::io::Error::last_os_error()
        );
        pollfd.revents
    }

    /// A plaintext WebSocket pipe (`Protocol::HTTP`, as an `ws://` upgrade
    /// creates it) over a connected frontend and backend.
    fn plain_websocket_pipe(
        pool: &mut Pool,
        frontend_socket: StdTcpStream,
        backend_socket: StdTcpStream,
    ) -> Pipe<TcpStream, TestListener> {
        let address = "127.0.0.1:0".parse().expect("test address");
        let listener = Rc::new(RefCell::new(TestListener { address }));
        let mut pipe = Pipe::new(
            pool.checkout().expect("backend buffer"),
            None,
            Some(TcpStream::from_std(backend_socket)),
            None,
            None,
            None,
            None,
            pool.checkout().expect("frontend buffer"),
            Token(0),
            TcpStream::from_std(frontend_socket),
            listener,
            Protocol::HTTP,
            Ulid::generate(),
            Ulid::generate(),
            None,
            websocket_endpoint(),
        );
        pipe.set_back_token(Token(1));
        pipe
    }

    /// The client's last WebSocket frame is on the wire together with its FIN
    /// (`EPOLLIN | EPOLLRDHUP`, one coalesced event): the pipe must hand the
    /// frame to the backend before it closes, as `Pipe::frontend_hup` does for
    /// a TCP session.
    ///
    /// `Pipe::ready` returns `Close` as soon as the frontend event carries
    /// HUP, before it reads the kernel tail, so the frame is dropped.
    #[cfg(target_os = "linux")]
    #[test]
    fn websocket_frontend_hup_delivers_the_clients_last_frame_before_closing() {
        use std::{io::Read, net::Shutdown, os::fd::AsRawFd};

        const LAST_FRAME: &[u8] = b"last websocket frame, sent right before the FIN";

        let (mut frontend_peer, frontend_socket) = connected_pair();
        let (mut backend_peer, backend_socket) = connected_pair();

        frontend_peer
            .write_all(LAST_FRAME)
            .expect("the client writes its last frame");
        frontend_peer
            .shutdown(Shutdown::Write)
            .expect("the client half-closes");
        // Premise: the kernel really holds both the frame and the FIN.
        let wanted = libc::POLLIN | libc::POLLRDHUP;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut revents = 0;
        while revents & wanted != wanted && std::time::Instant::now() < deadline {
            revents = poll_revents(
                frontend_socket.as_raw_fd(),
                wanted,
                Duration::from_millis(50),
            );
        }
        assert_eq!(
            revents & wanted,
            wanted,
            "premise: the frame and the FIN must both be queued on the frontend socket"
        );

        let mut pool = Pool::with_capacity(2, 2, 4096);
        let mut pipe = plain_websocket_pipe(&mut pool, frontend_socket, backend_socket);
        // The event epoll reports for a frame followed by a FIN.
        pipe.update_readiness(Token(0), Ready::READABLE | Ready::HUP);

        let (session, proxy) = unused_handles();
        let mut metrics = SessionMetrics::new(Some(Duration::ZERO));
        let result = pipe.ready(session, proxy, &mut metrics);

        let mut received = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while received.len() < LAST_FRAME.len() && std::time::Instant::now() < deadline {
            let mut buf = [0u8; 128];
            match backend_peer.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => received.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::yield_now(),
                Err(e) => panic!("backend read failed: {e}"),
            }
        }
        assert_eq!(
            String::from_utf8_lossy(&received),
            String::from_utf8_lossy(LAST_FRAME),
            "the client's last frame never reached the backend: Pipe::ready returned {result:?} \
             on the frontend HUP without draining the kernel tail"
        );
    }

    /// A WebSocket client that closes (FIN, nothing in flight) ends its
    /// session: the pipe must write the access log and the end-of-session
    /// metrics once, as `Pipe::frontend_hup` does.
    ///
    /// `Pipe::ready` returns `Close` on the HUP without any `log_request*`
    /// call, so `access_logs.count` never moves.
    #[cfg(target_os = "linux")]
    #[test]
    fn websocket_frontend_hup_is_access_logged_once() {
        use std::{net::Shutdown, os::fd::AsRawFd};

        fn idle_pipe_after_fin(
            pool: &mut Pool,
        ) -> (Pipe<TcpStream, TestListener>, [StdTcpStream; 2]) {
            let (frontend_peer, frontend_socket) = connected_pair();
            let (backend_peer, backend_socket) = connected_pair();
            frontend_peer
                .shutdown(Shutdown::Write)
                .expect("the client half-closes");
            let revents = poll_revents(
                frontend_socket.as_raw_fd(),
                libc::POLLRDHUP,
                Duration::from_secs(5),
            );
            assert_ne!(
                revents & libc::POLLRDHUP,
                0,
                "premise: the FIN must be queued on the frontend socket"
            );
            (
                plain_websocket_pipe(pool, frontend_socket, backend_socket),
                [frontend_peer, backend_peer],
            )
        }

        let mut pool = Pool::with_capacity(4, 4, 4096);
        let mut metrics = SessionMetrics::new(Some(Duration::ZERO));
        let access_logs = || crate::socket::tests::proxy_count(names::access_logs::COUNT);

        // Premise: the oracle sees a close that logs. `frontend_hup` is the
        // drain-then-log entry the TCP session uses.
        let before = access_logs();
        let (mut tcp_style, _peers) = idle_pipe_after_fin(&mut pool);
        assert_eq!(tcp_style.frontend_hup(&mut metrics), SessionResult::Close);
        assert_eq!(
            access_logs() - before,
            1,
            "premise: Pipe::frontend_hup must move access_logs.count by one"
        );

        // The WebSocket entry: the same event through `SessionState::ready`.
        let before = access_logs();
        let (mut websocket, _peers) = idle_pipe_after_fin(&mut pool);
        websocket.update_readiness(Token(0), Ready::HUP);
        let (session, proxy) = unused_handles();
        let result = websocket.ready(session, proxy, &mut metrics);
        assert_eq!(result, SessionResult::Close);
        assert_eq!(
            access_logs() - before,
            1,
            "a WebSocket session that ends on the client's FIN must reach the access log \
             and the end-of-session metrics once"
        );
    }

    /// Lets `Pipe<FrontRustls, _>` run over a real socket pair whose peer is
    /// an in-memory rustls client.
    struct WssFixture {
        pipe: Pipe<FrontRustls, TestListener>,
        client: rustls::ClientConnection,
        client_socket: StdTcpStream,
        metrics: SessionMetrics,
        /// Plaintext bytes handed to the pipe through `backend_buffer`.
        loaded: usize,
        /// Plaintext bytes the client has decrypted so far.
        received: usize,
        _backend_peer: StdTcpStream,
        _pool: Pool,
    }

    /// Move every pending TLS byte from `from` to `to`, in memory.
    fn pump_tls<A, B>(
        from: &mut rustls::ConnectionCommon<A>,
        to: &mut rustls::ConnectionCommon<B>,
    ) {
        let mut wire = Vec::new();
        while from.wants_write() {
            from.write_tls(&mut wire)
                .expect("a TLS flight must serialize into memory");
        }
        let mut cursor = std::io::Cursor::new(wire.as_slice());
        while (cursor.position() as usize) < wire.len() {
            to.read_tls(&mut cursor)
                .expect("a TLS flight must be readable from memory");
            to.process_new_packets()
                .expect("a TLS flight must process cleanly");
        }
    }

    impl WssFixture {
        fn new() -> Self {
            use socket2::{Domain, SockRef, Socket, Type};

            // Small kernel buffers on both ends, so a few KiB of ciphertext
            // fill the connection while the client does not read.
            let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind the WSS listener");
            let client_socket =
                Socket::new(Domain::IPV4, Type::STREAM, None).expect("a client socket");
            client_socket
                .set_recv_buffer_size(4096)
                .expect("SO_RCVBUF on the client");
            client_socket
                .connect(&listener.local_addr().expect("listener address").into())
                .expect("the client connects");
            let client_socket = StdTcpStream::from(client_socket);
            let (frontend_socket, _) = listener.accept().expect("accept the client");
            SockRef::from(&frontend_socket)
                .set_send_buffer_size(4096)
                .expect("SO_SNDBUF on the frontend");
            client_socket
                .set_nonblocking(true)
                .expect("client nonblocking");
            frontend_socket
                .set_nonblocking(true)
                .expect("frontend nonblocking");

            // Settle a TLS 1.3 handshake in memory, so the real socket only
            // ever carries application records.
            let (mut session, mut client) =
                crate::socket::rustls_read_tests::fresh_pair(Vec::new());
            for _ in 0..16 {
                if !client.is_handshaking() && !session.is_handshaking() && !client.wants_write() {
                    break;
                }
                pump_tls(&mut client, &mut session);
                pump_tls(&mut session, &mut client);
            }
            pump_tls(&mut session, &mut client);
            assert!(
                !session.is_handshaking() && !client.is_handshaking(),
                "premise: the in-memory handshake must complete"
            );
            assert!(
                !session.wants_write() && !client.wants_write(),
                "premise: no TLS record may be pending after the handshake"
            );

            let (_backend_peer, backend_socket) = connected_pair();
            // A buffer holds the largest burst the tests hand to the pipe.
            let mut pool = Pool::with_capacity(2, 2, 65536);
            let address = "127.0.0.1:0".parse().expect("test address");
            let listener = Rc::new(RefCell::new(TestListener { address }));
            let mut pipe = Pipe::new(
                pool.checkout().expect("backend buffer"),
                None,
                Some(TcpStream::from_std(backend_socket)),
                None,
                None,
                None,
                None,
                pool.checkout().expect("frontend buffer"),
                Token(0),
                FrontRustls {
                    stream: TcpStream::from_std(frontend_socket),
                    session,
                    peer_disconnected: false,
                    peer_reset: false,
                    tls_fatal: false,
                    session_ulid: Ulid::generate(),
                    configured_peer: None,
                    recv_memory: crate::socket::RecvMemory::default(),
                },
                listener,
                Protocol::HTTPS,
                Ulid::generate(),
                Ulid::generate(),
                None,
                websocket_endpoint(),
            );
            pipe.set_back_token(Token(1));

            Self {
                pipe,
                client,
                client_socket,
                metrics: SessionMetrics::new(Some(Duration::ZERO)),
                loaded: 0,
                received: 0,
                _backend_peer,
                _pool: pool,
            }
        }

        /// What the backend sent: `backend_readable` queues `bytes` in
        /// `backend_buffer` and arms the frontend WRITABLE interest and event;
        /// the WRITABLE event then runs the response direction.
        fn push(&mut self, bytes: &[u8]) -> SessionResult {
            self.pipe
                .backend_buffer
                .write_all(bytes)
                .expect("the backend buffer holds the burst");
            self.loaded += bytes.len();
            self.pipe.frontend_readiness.arm_writable();
            self.pipe.writable(&mut self.metrics)
        }

        /// The client reads its socket dry and decrypts what arrived.
        fn drain_client(&mut self) {
            use std::io::Read;

            loop {
                match self.client.read_tls(&mut self.client_socket) {
                    Ok(0) => break,
                    Ok(_) => {
                        self.client
                            .process_new_packets()
                            .expect("the client must accept the server's records");
                        let mut buf = [0u8; 4096];
                        loop {
                            match self.client.reader().read(&mut buf) {
                                Ok(0) => break,
                                Ok(n) => self.received += n,
                                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                                Err(e) => panic!("client plaintext read failed: {e}"),
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => panic!("client socket read failed: {e}"),
                }
            }
        }

        /// Premise: a small burst round-trips, so a shortfall later is the
        /// pipe's and not the fixture's.
        fn prove_the_round_trip(&mut self) {
            use std::os::fd::AsRawFd;

            assert_eq!(self.push(b"premise"), SessionResult::Continue);
            let revents = poll_revents(
                self.client_socket.as_raw_fd(),
                libc::POLLIN,
                Duration::from_secs(5),
            );
            assert_ne!(revents & libc::POLLIN, 0, "premise: the record must arrive");
            self.drain_client();
            assert_eq!(
                (self.received, self.pipe.frontend.socket_wants_write()),
                (self.loaded, false),
                "premise: a small burst must be decrypted by the client and leave rustls empty"
            );
        }

        /// Hand the pipe a burst while the connection is full: rustls absorbs
        /// all of it (its send buffer holds 64 KiB), the kernel takes only a
        /// few KiB, and `backend_buffer` is empty again, so the pipe believes
        /// the whole burst delivered.
        fn strand_a_burst_in_rustls(&mut self) {
            let burst = vec![0x5a; 40 * 1024];
            assert_eq!(self.push(&burst), SessionResult::Continue);
            assert_eq!(
                self.pipe.backend_buffer.available_data(),
                0,
                "premise: rustls must absorb the whole burst"
            );
            assert!(
                self.pipe.frontend.socket_wants_write(),
                "premise: rustls must still hold ciphertext the full connection refused"
            );
            assert!(
                !self.pipe.frontend_readiness.event.is_writable(),
                "premise: the WouldBlock write must have consumed the WRITABLE event"
            );
        }

        /// The kernel's WRITABLE edge for the frontend socket, once the
        /// client has read enough for the connection to accept bytes again.
        fn deliver_the_writable_edge(&mut self) -> bool {
            use std::os::fd::AsRawFd;

            let writable = poll_revents(
                self.pipe.front_socket().as_raw_fd(),
                libc::POLLOUT,
                Duration::from_millis(20),
            ) & libc::POLLOUT
                != 0;
            if writable {
                self.pipe.update_readiness(Token(0), Ready::WRITABLE);
            }
            writable
        }
    }

    /// Plaintext that rustls absorbed is not delivered: it is 40 KiB of
    /// records the connection refused. While the backend stays open and says
    /// nothing more, the only thing that can send them is the next WRITABLE
    /// edge, and `Pipe::writable` answers that edge with an empty
    /// `backend_buffer` by dropping the WRITABLE interest, without the
    /// flushing `socket_write(&[])` that would hand them to the socket.
    #[test]
    fn wss_pipe_tls_records_are_flushed_when_the_buffer_is_empty() {
        let mut fixture = WssFixture::new();
        fixture.prove_the_round_trip();
        fixture.strand_a_burst_in_rustls();

        // The client catches up, round after round, as a slow reader does.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while fixture.received < fixture.loaded && std::time::Instant::now() < deadline {
            fixture.drain_client();
            if fixture.deliver_the_writable_edge()
                && fixture
                    .pipe
                    .frontend_readiness
                    .filter_interest()
                    .is_writable()
            {
                assert_eq!(
                    fixture.pipe.writable(&mut fixture.metrics),
                    SessionResult::Continue
                );
            }
        }
        fixture.drain_client();

        assert_eq!(
            fixture.received,
            fixture.loaded,
            "{} of {} plaintext bytes never reached the WSS client: rustls still holds {} \
             (wants_write={}) and Pipe::writable left them unflushed on every WRITABLE edge",
            fixture.loaded - fixture.received,
            fixture.loaded,
            if fixture.pipe.frontend.socket_wants_write() {
                "their ciphertext"
            } else {
                "nothing"
            },
            fixture.pipe.frontend.socket_wants_write(),
        );
    }

    /// The backend closes while the same burst sits in rustls. The pipe
    /// counts only `backend_buffer`, not what rustls holds, as the response
    /// in flight, so `backend_hup` closes the session and the records are
    /// lost: nothing after `Close` flushes rustls.
    #[test]
    fn wss_pipe_tls_records_survive_the_backend_hup() {
        let mut fixture = WssFixture::new();
        fixture.prove_the_round_trip();
        fixture.strand_a_burst_in_rustls();

        let (session, proxy) = unused_handles();

        // The backend closes: HUP on its token, nothing buffered.
        fixture.pipe.update_readiness(Token(1), Ready::HUP);
        let first = fixture
            .pipe
            .ready(session.clone(), proxy.clone(), &mut fixture.metrics);
        assert_ne!(
            first,
            SessionResult::Close,
            "the first ready pass closed before the client read anything"
        );

        // The client reads some, the connection accepts bytes again.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        fixture.drain_client();
        let mut edge = fixture.deliver_the_writable_edge();
        while !edge && std::time::Instant::now() < deadline {
            fixture.drain_client();
            edge = fixture.deliver_the_writable_edge();
        }
        // Without the edge the second pass would stop at the loop head and
        // prove nothing.
        assert!(
            edge,
            "premise: the frontend WRITABLE edge must arrive before the second pass"
        );
        let second = fixture.pipe.ready(session, proxy, &mut fixture.metrics);

        let pending = fixture.pipe.frontend.socket_wants_write();
        fixture.drain_client();
        assert!(
            second != SessionResult::Close || !pending,
            "ready returned Close on the backend HUP while rustls still held ciphertext \
             ({} of {} plaintext bytes had not reached the WSS client)",
            fixture.loaded - fixture.received,
            fixture.loaded
        );
    }

    /// The backend's last burst and its EOF arrive together while the
    /// client's connection is full: `backend_readable` has met the EOF
    /// (`WriteOpen`) and `writable` hands the burst to rustls, which absorbs
    /// it all and leaves the ciphertext behind a `WouldBlock`. The
    /// `check_connections` inside `writable` must count what rustls holds as
    /// the response in flight, or it closes the half-closed session with the
    /// records unsent; the flush that finally empties rustls must then end
    /// the session, because no later event is owed to it.
    #[test]
    fn wss_pipe_outlives_the_backend_eof_until_rustls_is_flushed() {
        let mut fixture = WssFixture::new();
        fixture.prove_the_round_trip();
        fixture.pipe.backend_status = ConnectionStatus::WriteOpen;

        let burst = vec![0x5a; 40 * 1024];
        let first = fixture.push(&burst);
        assert!(
            fixture.pipe.frontend.socket_wants_write(),
            "premise: rustls must hold ciphertext the full connection refused"
        );
        assert_eq!(
            first,
            SessionResult::Continue,
            "writable closed a session whose rustls still held {} of {} plaintext bytes",
            fixture.loaded - fixture.received,
            fixture.loaded
        );

        // The client catches up; the session must end on the flush that
        // empties rustls, and not before.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut last = SessionResult::Continue;
        while last == SessionResult::Continue && std::time::Instant::now() < deadline {
            fixture.drain_client();
            if fixture.deliver_the_writable_edge()
                && fixture
                    .pipe
                    .frontend_readiness
                    .filter_interest()
                    .is_writable()
            {
                last = fixture.pipe.writable(&mut fixture.metrics);
                assert!(
                    last == SessionResult::Continue || !fixture.pipe.frontend.socket_wants_write(),
                    "writable closed the session with ciphertext still inside rustls"
                );
            }
        }
        fixture.drain_client();

        assert_eq!(
            last,
            SessionResult::Close,
            "the flush that emptied rustls must end the half-closed session"
        );
        assert_eq!(
            fixture.received, fixture.loaded,
            "every plaintext byte must reach the WSS client before the session closes"
        );
    }

    /// A frontend socket error ends a WebSocket session too, and that exit
    /// of `Pipe::ready` must reach the access log and the end-of-session
    /// metrics once, as every handler that returns `Close` does.
    #[test]
    fn websocket_frontend_error_is_access_logged_once() {
        let (_frontend_peer, frontend_socket) = connected_pair();
        let (_backend_peer, backend_socket) = connected_pair();
        let mut pool = Pool::with_capacity(2, 2, 4096);
        let mut pipe = plain_websocket_pipe(&mut pool, frontend_socket, backend_socket);
        let mut metrics = SessionMetrics::new(Some(Duration::ZERO));
        let access_logs = || crate::socket::tests::proxy_count(names::access_logs::COUNT);

        let before = access_logs();
        pipe.update_readiness(Token(0), Ready::ERROR);
        let (session, proxy) = unused_handles();
        let result = pipe.ready(session, proxy, &mut metrics);
        assert_eq!(result, SessionResult::Close);
        assert_eq!(
            access_logs() - before,
            1,
            "a WebSocket session that ends on a frontend socket error must reach the access log \
             and the end-of-session metrics once"
        );
    }

    /// Write to `stream` until its kernel send buffer refuses more, so the
    /// next write the code under test makes meets `EAGAIN`. The peer never
    /// reads, so the window stays closed and nothing drains the buffer. Both
    /// ends are pinned small first: an explicit `SO_SNDBUF`/`SO_RCVBUF` turns
    /// off the kernel's buffer autotuning, which would otherwise grow the
    /// send buffer again after the first `EAGAIN`.
    fn fill_send_buffer(stream: &mut StdTcpStream, peer: &StdTcpStream) {
        socket2::SockRef::from(peer)
            .set_recv_buffer_size(16 * 1024)
            .expect("the peer receive buffer must be settable");
        socket2::SockRef::from(&*stream)
            .set_send_buffer_size(4096)
            .expect("the send buffer must be settable");
        for chunk_len in [65_536usize, 4_096, 256, 1] {
            let chunk = vec![0u8; chunk_len];
            loop {
                match stream.write(&chunk) {
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) => panic!("filling the send buffer failed: {error}"),
                }
            }
        }
    }

    /// The backend has hung up with a response tail still in `backend_buffer`
    /// and the client's receive window is closed. One pass must write once,
    /// meet `EAGAIN`, and then park on the "backend HUP, frontend interested
    /// in WRITABLE but no WRITABLE event" break guard until the client's
    /// EPOLLOUT edge comes back; it must not spin until `MAX_LOOP_ITERATIONS`
    /// and close the session with the tail undelivered.
    ///
    /// `Pipe::backend_hup` re-arms the frontend WRITABLE *event* (not only
    /// the interest) on its "bytes still buffered" branch, which cancels the
    /// `EAGAIN` that `writable` had just recorded, so the guard never holds
    /// and every iteration makes one more failing `send(2)`.
    ///
    /// To SEE THIS RED: in the buffered-data branch of `Pipe::backend_hup`,
    /// replace `self.frontend_readiness.interest.insert(Ready::WRITABLE)` with
    /// `self.frontend_readiness.arm_writable()`, the call it had before. The
    /// pass then runs to `MAX_LOOP_ITERATIONS` and the first assertion fails
    /// with `left: 1` against `right: 0`.
    #[test]
    fn backend_hup_with_a_blocked_frontend_parks_instead_of_spinning_to_the_loop_cap() {
        use crate::socket::tests::proxy_count;

        let (frontend_peer, mut frontend_server) = connected_pair();
        fill_send_buffer(&mut frontend_server, &frontend_peer);
        let (_backend_peer, backend_socket) = connected_pair();

        let tail = [b'x'; 2048];
        let mut pool = Pool::with_capacity(2, 2, 4096);
        let mut backend_buffer = pool.checkout().expect("backend buffer");
        let frontend_buffer = pool.checkout().expect("frontend buffer");
        backend_buffer
            .write_all(&tail)
            .expect("queue the response tail");
        let address = "127.0.0.1:0".parse().expect("test address");
        let listener = Rc::new(RefCell::new(TestListener { address }));

        let mut pipe = Pipe::new(
            backend_buffer,
            None,
            Some(TcpStream::from_std(backend_socket)),
            None,
            None,
            None,
            None,
            frontend_buffer,
            Token(0),
            TcpStream::from_std(frontend_server),
            listener,
            Protocol::HTTP,
            Ulid::generate(),
            Ulid::generate(),
            None,
            WebSocketContext::Tcp,
        );
        pipe.set_back_token(Token(1));
        // The client's EPOLLOUT edge just arrived, and so did the backend's
        // hang-up: the state a slow reader leaves behind after one burst.
        pipe.frontend_readiness.event = Ready::WRITABLE;
        pipe.backend_readiness.event = Ready::HUP;

        let session: Rc<RefCell<dyn crate::ProxySession>> =
            Rc::new(RefCell::new(crate::server::ListenSession {
                protocol: Protocol::HTTPListen,
            }));
        let proxy: Rc<RefCell<dyn crate::L7Proxy>> = Rc::new(RefCell::new(UnusedProxy));
        let mut metrics = SessionMetrics::new(Some(Duration::ZERO));

        let capped_before = proxy_count(names::http::INFINITE_LOOP_ERROR);
        let result = pipe.ready(session, proxy, &mut metrics);
        let capped = proxy_count(names::http::INFINITE_LOOP_ERROR) - capped_before;

        assert_eq!(
            capped, 0,
            "a blocked client must not drive the pass to MAX_LOOP_ITERATIONS \
             ({MAX_LOOP_ITERATIONS} failing writes in one ready())"
        );
        assert_eq!(
            result,
            SessionResult::Continue,
            "the session must stay open until the client drains the response tail"
        );
        assert_eq!(
            pipe.backend_buffer.available_data(),
            tail.len(),
            "the undelivered tail must still be queued for the client"
        );
    }
}
