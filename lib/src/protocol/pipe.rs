//! Transparent byte-stream forwarder (TCP + WebSocket post-upgrade).
//!
//! Forwards bytes between front and back through fixed-size buffers without
//! payload inspection. When bytes enter a sozu-owned buffer, the opposite
//! endpoint is armed via `Readiness::arm_writable()` so edge-triggered epoll
//! cannot park buffered data behind a missing writable edge. Used as the
//! post-handshake state for raw TCP listeners and after a successful WebSocket
//! upgrade on the H1 path.

use std::{cell::RefCell, net::SocketAddr, rc::Rc};

use mio::{Token, net::TcpStream};
use rusty_ulid::Ulid;
use sozu_command::{
    config::MAX_LOOP_ITERATIONS,
    logging::{EndpointRecord, LogContext, ansi_palette},
};

use crate::metrics::names;
use crate::{
    L7Proxy, ListenerHandler, Protocol, Readiness, SessionMetrics, SessionResult, StateResult,
    backends::Backend,
    pool::Checkout,
    protocol::{SessionState, http::parser::Method},
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
    pub backend_readiness: Readiness,
    backend_socket: Option<TcpStream>,
    backend_status: ConnectionStatus,
    backend_token: Option<Token>,
    pub backend: Option<Rc<RefCell<Backend>>>,
    cluster_id: Option<String>,
    pub container_backend_timeout: Option<TimeoutContainer>,
    pub container_frontend_timeout: Option<TimeoutContainer>,
    frontend_buffer: Checkout,
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
        cluster_id: Option<String>,
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

        let mut session = Pipe {
            backend_buffer,
            backend_id,
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

    /// Stamp connection-scoped TLS metadata captured at handshake time onto
    /// the pipe for access-log emission. Called from the HTTPS→WSS upgrade
    /// path in `https.rs::upgrade_mux` after the `Pipe` has been built from
    /// the prior mux `HttpContext`. Leaves plaintext paths (plain TCP, plain
    /// WS, proxy-protocol) untouched so their access logs continue to emit
    /// `None` for all TLS fields.
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

    pub fn set_cluster_id(&mut self, cluster_id: Option<String>) {
        self.cluster_id = cluster_id;
    }

    pub fn set_backend_id(&mut self, backend_id: Option<String>) {
        self.backend_id = backend_id;
    }

    pub fn set_back_token(&mut self, token: Token) {
        self.backend_token = Some(token);
    }

    pub fn get_session_address(&self) -> Option<SocketAddr> {
        self.session_address
            .or_else(|| self.frontend.socket_ref().peer_addr().ok())
    }

    pub fn get_backend_address(&self) -> Option<SocketAddr> {
        self.backend_socket
            .as_ref()
            .and_then(|backend| backend.peer_addr().ok())
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
        metrics.register_end_of_session(&context);
        log_access!(
            error,
            on_failure: { incr!(names::access_logs::UNSENT) },
            message,
            context,
            session_address: self.get_session_address(),
            backend_address: self.get_backend_address(),
            protocol: self.protocol_string(),
            endpoint,
            tags: listener.get_tags(&listener.get_addr().to_string()),
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
        let response_is_inflight = self.backend_buffer.available_data() > 0
            || self.backend_readiness.event.is_readable()
            || self.splice_out_pending() > 0;
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
        // bytes may still sit in `frontend_buffer` (already read) or in the
        // kernel receive buffer (not read yet, signalled by a pending
        // READABLE event) — see the sibling `SocketResult::Closed` drain in
        // `readable` above and `check_connections`'s `request_is_inflight`
        // (sozu-proxy/sozu#1290, the HUP-path sibling of Defect C).
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
                // queued behind the FIN. The `SocketResult::Closed` arm in
                // `readable` finishes the lifecycle once that tail hits EOF.
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
                self.log_request_success(metrics);
                SessionResult::Close
            }
        } else {
            debug!(
                "{} Pipe::backend_hup: backend connection closed, keeping alive due to inflight data in buffers.",
                log_context!(self)
            );
            self.frontend_readiness.arm_writable();
            if self.backend_readiness.event.is_readable() {
                self.backend_readiness.interest.insert(Ready::READABLE);
            }
            SessionResult::Continue
        }
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
                // tail was still in flight (sozu-proxy/sozu#1279 Defect C: a
                // payload coalesced with the SNI ClientHello is the
                // reproducer, but the drop hit any plain-TCP upload racing a
                // frontend close). Transition to the half-closed `WriteOpen`
                // status exactly as the WouldBlock/Continue path does below,
                // arm the backend writable to flush the queue, and defer the
                // teardown to `check_connections`, which closes only once
                // nothing is in flight (mirrors `backend_hup`'s drain branch).
                // `frontend_hup` (EPOLLRDHUP, below) has the same drain
                // requirement for the same reason: FIN can coalesce with the
                // payload tail into one epoll batch on a loaded event loop.
                // So does `splice_readable`'s own `Closed` arm, for bytes
                // sitting in the kernel `in_pipe` instead of `frontend_buffer`.
                self.frontend_status = match self.frontend_status {
                    ConnectionStatus::Normal => ConnectionStatus::WriteOpen,
                    ConnectionStatus::ReadOpen => ConnectionStatus::Closed,
                    s => s,
                };
                self.frontend_readiness.event.remove(Ready::READABLE);
                self.frontend_readiness.interest.remove(Ready::READABLE);
                if self.frontend_buffer.available_data() > 0 && self.backend_socket.is_some() {
                    self.backend_readiness.arm_writable();
                }
                if !self.check_connections() {
                    self.reset_readiness_for_close();
                    self.log_request_success(metrics);
                    return SessionResult::Close;
                }
                return SessionResult::Continue;
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
        // Inherited preread bytes sit in `backend_buffer`; splice never
        // drains that userspace buffer, so it must be empty before the fast
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
            self.backend_readiness.interest.insert(Ready::READABLE);
            self.frontend_readiness.interest.remove(Ready::WRITABLE);
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

        if let Some(ref mut backend) = self.backend_socket {
            while socket_res == SocketResult::Continue {
                // no more data in buffer, stop here
                if self.frontend_buffer.available_data() == 0 {
                    self.frontend_readiness.interest.insert(Ready::READABLE);
                    self.backend_readiness.interest.remove(Ready::WRITABLE);
                    count!(names::backend::BACK_BYTES_OUT, sz as i64);
                    metrics.backend_bout += sz;
                    return SessionResult::Continue;
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
        // Inherited preread bytes sit in `backend_buffer`; splice never
        // drains that userspace buffer, so it must be empty before the fast
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
                // (sozu-proxy/sozu#1279 Defect C) and of `frontend_hup`'s
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
                self.frontend_status = match self.frontend_status {
                    ConnectionStatus::Normal => ConnectionStatus::WriteOpen,
                    ConnectionStatus::ReadOpen => ConnectionStatus::Closed,
                    s => s,
                };
                self.frontend_readiness.event.remove(Ready::READABLE);
                self.frontend_readiness.interest.remove(Ready::READABLE);
                if self.splice_in_pending() > 0 && self.backend_socket.is_some() {
                    self.backend_readiness.arm_writable();
                }
                if !self.check_connections() {
                    self.reset_readiness_for_close();
                    self.log_request_success(metrics);
                    return SessionResult::Close;
                }
                return SessionResult::Continue;
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

        if self.frontend_readiness.event.is_hup() {
            return SessionResult::Close;
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
    use crate::pool::Pool;

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
    /// (sozu-proxy/sozu#1279 Defect C): when the frontend read side reaches
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

        // Draining now delivers every queued byte to the backend.
        assert_eq!(pipe.backend_writable(&mut metrics), SessionResult::Continue);
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
    /// no pending READABLE event), `frontend_hup` keeps today's legacy
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
    /// data loss (sozu-proxy/sozu#1279 Defect C / #1290): `splice_readable`'s
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
        frontend_peer
            .write_all(&payload)
            .expect("write frontend payload");

        // Splice the payload into the kernel in_pipe (bounded retries: the
        // loopback delivery and the per-call capacity cap may need several
        // calls, and an early call can observe WouldBlock).
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
        // backend socket buffer cannot deadlock the loop).
        let mut received = Vec::with_capacity(payload.len());
        for _ in 0..100_000 {
            let drain = pipe.splice_backend_writable(&mut metrics);
            assert_ne!(
                drain,
                SessionResult::Close,
                "draining the kernel pipe must not tear the session down mid-flight"
            );
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

        // Nothing inflight and the frontend is half-closed: the session is
        // now closeable, and the next EOF re-observation closes it through
        // `splice_readable`'s check_connections gate.
        assert!(
            !pipe.check_connections(),
            "with nothing inflight and the frontend closed, the session must be closeable"
        );
        assert_eq!(
            pipe.splice_readable(&mut metrics),
            SessionResult::Close,
            "re-observing EOF with nothing inflight must close the session"
        );
    }
}
