//! Transparent byte-stream forwarder (TCP + WebSocket post-upgrade).
//!
//! Forwards bytes between front and back through fixed-size buffers without
//! payload inspection. Readiness is managed via direct mio interest toggles
//! (no `signal_pending_write` / `arm_writable` here — those belong to the
//! mux H2 path). Used as the post-handshake state for raw TCP listeners and
//! after a successful WebSocket upgrade on the H1 path.

use std::{cell::RefCell, net::SocketAddr, rc::Rc};

use mio::{Token, net::TcpStream};
use rusty_ulid::Ulid;
use sozu_command::{
    config::MAX_LOOP_ITERATIONS,
    logging::{EndpointRecord, LogContext, ansi_palette},
};

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

        let session = Pipe {
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

        trace!("{} created pipe", log_context!(session));
        session
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
        if let Some(t) = self.container_frontend_timeout.as_mut() {
            if !t.reset() {
                error!(
                    "{} Could not reset front timeout (pipe)",
                    log_context!(self)
                );
            }
        }

        if let Some(t) = self.container_backend_timeout.as_mut() {
            if !t.reset() {
                error!("{} Could not reset back timeout (pipe)", log_context!(self));
            }
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
            on_failure: { incr!("unsent-access-logs") },
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
        incr!("pipe.errors");
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

    /// Wether the session should be kept open, depending on endpoints status
    /// and buffer usage (both in memory and in kernel)
    pub fn check_connections(&self) -> bool {
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
        self.log_request_success(metrics);
        self.frontend_status = ConnectionStatus::Closed;
        SessionResult::Close
    }

    pub fn backend_hup(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        self.backend_status = ConnectionStatus::Closed;
        let pipe_has_data = self.splice_out_pending() > 0;
        if self.backend_buffer.available_data() == 0 && !pipe_has_data {
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
            self.frontend_readiness.interest.insert(Ready::WRITABLE);
            if self.backend_readiness.event.is_readable() {
                self.backend_readiness.interest.insert(Ready::READABLE);
            }
            SessionResult::Continue
        }
    }

    // Read content from the session
    pub fn readable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        #[cfg(all(target_os = "linux", feature = "splice"))]
        if self.protocol == Protocol::TCP && self.splice_pipe.is_some() {
            return self.splice_readable(metrics);
        }

        self.reset_timeouts();

        trace!("{} pipe readable", log_context!(self));
        if self.frontend_buffer.available_space() == 0 {
            self.frontend_readiness.interest.remove(Ready::READABLE);
            self.backend_readiness.interest.insert(Ready::WRITABLE);
            return SessionResult::Continue;
        }

        let (sz, res) = self.frontend.socket_read(self.frontend_buffer.space());
        debug!("{} Read {} bytes", log_context!(self), sz);

        if sz > 0 {
            //FIXME: replace with copy()
            self.frontend_buffer.fill(sz);

            count!("bytes_in", sz as i64);
            metrics.bin += sz;

            if self.frontend_buffer.available_space() == 0 {
                self.frontend_readiness.interest.remove(Ready::READABLE);
            }
            self.backend_readiness.interest.insert(Ready::WRITABLE);
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
            self.frontend_readiness.reset();
            self.backend_readiness.reset();
            self.log_request_success(metrics);
            return SessionResult::Close;
        }

        match res {
            SocketResult::Error => {
                self.frontend_readiness.reset();
                self.backend_readiness.reset();
                self.log_request_error(metrics, "front socket read error");
                return SessionResult::Close;
            }
            SocketResult::Closed => {
                self.frontend_readiness.reset();
                self.backend_readiness.reset();
                self.log_request_success(metrics);
                return SessionResult::Close;
            }
            SocketResult::WouldBlock => {
                self.frontend_readiness.event.remove(Ready::READABLE);
            }
            SocketResult::Continue => {}
        };

        self.backend_readiness.interest.insert(Ready::WRITABLE);
        SessionResult::Continue
    }

    // Forward content to session
    pub fn writable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        #[cfg(all(target_os = "linux", feature = "splice"))]
        if self.protocol == Protocol::TCP && self.splice_pipe.is_some() {
            return self.splice_writable(metrics);
        }

        trace!("{} Pipe writable", log_context!(self));
        if self.backend_buffer.available_data() == 0 {
            self.backend_readiness.interest.insert(Ready::READABLE);
            self.frontend_readiness.interest.remove(Ready::WRITABLE);
            return SessionResult::Continue;
        }

        let mut sz = 0usize;
        let mut res = SocketResult::Continue;
        while res == SocketResult::Continue {
            // no more data in buffer, stop here
            if self.backend_buffer.available_data() == 0 {
                count!("bytes_out", sz as i64);
                metrics.bout += sz;
                self.backend_readiness.interest.insert(Ready::READABLE);
                self.frontend_readiness.interest.remove(Ready::WRITABLE);
                return SessionResult::Continue;
            }
            let (current_sz, current_res) = self.frontend.socket_write(self.backend_buffer.data());
            res = current_res;
            self.backend_buffer.consume(current_sz);
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
                count!("bytes_out", sz as i64);
                self.frontend_readiness.reset();
                self.backend_readiness.reset();
                self.log_request_success(metrics);
                return SessionResult::Close;
            }
        }

        if sz > 0 {
            count!("bytes_out", sz as i64);
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
                self.frontend_readiness.reset();
                self.backend_readiness.reset();
                self.log_request_error(metrics, "front socket write error");
                return SessionResult::Close;
            }
            SocketResult::Closed => {
                self.frontend_readiness.reset();
                self.backend_readiness.reset();
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
        #[cfg(all(target_os = "linux", feature = "splice"))]
        if self.protocol == Protocol::TCP && self.splice_pipe.is_some() {
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
                    count!("back_bytes_out", sz as i64);
                    metrics.backend_bout += sz;
                    return SessionResult::Continue;
                }

                let (current_sz, current_res) = backend.socket_write(self.frontend_buffer.data());
                socket_res = current_res;
                self.frontend_buffer.consume(current_sz);
                sz += current_sz;

                if current_sz == 0 && current_res == SocketResult::Continue {
                    self.backend_status = match self.backend_status {
                        ConnectionStatus::Normal => ConnectionStatus::ReadOpen,
                        ConnectionStatus::WriteOpen => ConnectionStatus::Closed,
                        s => s,
                    };
                }
            }
        }

        count!("back_bytes_out", sz as i64);
        metrics.backend_bout += sz;

        if !self.check_connections() {
            self.frontend_readiness.reset();
            self.backend_readiness.reset();
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
                self.frontend_readiness.reset();
                self.backend_readiness.reset();
                self.log_request_error(metrics, "back socket write error");
                return SessionResult::Close;
            }
            SocketResult::Closed => {
                self.frontend_readiness.reset();
                self.backend_readiness.reset();
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
        #[cfg(all(target_os = "linux", feature = "splice"))]
        if self.protocol == Protocol::TCP && self.splice_pipe.is_some() {
            return self.splice_backend_readable(metrics);
        }

        self.reset_timeouts();

        trace!("{} Pipe backend_readable", log_context!(self));
        if self.backend_buffer.available_space() == 0 {
            self.backend_readiness.interest.remove(Ready::READABLE);
            return SessionResult::Continue;
        }

        if let Some(ref mut backend) = self.backend_socket {
            let (size, remaining) = backend.socket_read(self.backend_buffer.space());
            self.backend_buffer.fill(size);

            debug!("{} Read {} bytes", log_context!(self), size);

            if remaining != SocketResult::Continue || size == 0 {
                self.backend_readiness.event.remove(Ready::READABLE);
            }
            if size > 0 {
                self.frontend_readiness.interest.insert(Ready::WRITABLE);
                count!("back_bytes_in", size as i64);
                metrics.backend_bin += size;
            }

            if size == 0 && remaining == SocketResult::Closed {
                self.backend_status = match self.backend_status {
                    ConnectionStatus::Normal => ConnectionStatus::WriteOpen,
                    ConnectionStatus::ReadOpen => ConnectionStatus::Closed,
                    s => s,
                };

                if !self.check_connections() {
                    self.frontend_readiness.reset();
                    self.backend_readiness.reset();
                    self.log_request_success(metrics);
                    return SessionResult::Close;
                }
            }

            match remaining {
                SocketResult::Error => {
                    self.frontend_readiness.reset();
                    self.backend_readiness.reset();
                    self.log_request_error(metrics, "back socket read error");
                    return SessionResult::Close;
                }
                SocketResult::Closed => {
                    if !self.check_connections() {
                        self.frontend_readiness.reset();
                        self.backend_readiness.reset();
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
            self.backend_readiness.interest.insert(Ready::WRITABLE);
            return SessionResult::Continue;
        }

        let pipe_write_end = self.splice_pipe.as_ref().unwrap().in_pipe[1];
        let (sz, res) = splice::splice_in(self.frontend.socket_ref(), pipe_write_end, capacity);
        debug!("{} Spliced {} bytes from frontend", log_context!(self), sz);

        if sz > 0 {
            self.splice_pipe.as_mut().unwrap().in_pipe_pending += sz;
            count!("bytes_in", sz as i64);
            metrics.bin += sz;
            self.backend_readiness.interest.insert(Ready::WRITABLE);
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
            self.frontend_readiness.reset();
            self.backend_readiness.reset();
            self.log_request_success(metrics);
            return SessionResult::Close;
        }

        match res {
            SocketResult::Error => {
                self.frontend_readiness.reset();
                self.backend_readiness.reset();
                self.log_request_error(metrics, "splice front socket read error");
                return SessionResult::Close;
            }
            SocketResult::Closed => {
                self.frontend_readiness.reset();
                self.backend_readiness.reset();
                self.log_request_success(metrics);
                return SessionResult::Close;
            }
            SocketResult::WouldBlock => {
                self.frontend_readiness.event.remove(Ready::READABLE);
            }
            SocketResult::Continue => {}
        }

        self.backend_readiness.interest.insert(Ready::WRITABLE);
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
                count!("bytes_out", sz as i64);
                metrics.bout += sz;
                self.backend_readiness.interest.insert(Ready::READABLE);
                self.frontend_readiness.interest.remove(Ready::WRITABLE);
                return SessionResult::Continue;
            }

            let pipe_read_end = self.splice_pipe.as_ref().unwrap().out_pipe[0];
            let (current_sz, current_res) =
                splice::splice_out(pipe_read_end, self.frontend.socket_ref(), pending);
            res = current_res;
            if current_sz > 0 {
                self.splice_pipe.as_mut().unwrap().out_pipe_pending -= current_sz;
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
                count!("bytes_out", sz as i64);
                self.frontend_readiness.reset();
                self.backend_readiness.reset();
                self.log_request_success(metrics);
                return SessionResult::Close;
            }
        }

        if sz > 0 {
            count!("bytes_out", sz as i64);
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
                self.frontend_readiness.reset();
                self.backend_readiness.reset();
                self.log_request_error(metrics, "splice front socket write error");
                return SessionResult::Close;
            }
            SocketResult::Closed => {
                self.frontend_readiness.reset();
                self.backend_readiness.reset();
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
                count!("back_bytes_out", sz as i64);
                metrics.backend_bout += sz;
                return SessionResult::Continue;
            }

            let pipe_read_end = self.splice_pipe.as_ref().unwrap().in_pipe[0];
            let (current_sz, current_res) = match self.backend_socket.as_ref() {
                Some(b) => splice::splice_out(pipe_read_end, b, pending),
                None => break,
            };
            socket_res = current_res;
            if current_sz > 0 {
                self.splice_pipe.as_mut().unwrap().in_pipe_pending -= current_sz;
            }
            sz += current_sz;

            if current_sz == 0 && current_res == SocketResult::Continue {
                self.backend_status = match self.backend_status {
                    ConnectionStatus::Normal => ConnectionStatus::ReadOpen,
                    ConnectionStatus::WriteOpen => ConnectionStatus::Closed,
                    s => s,
                };
            }
        }

        count!("back_bytes_out", sz as i64);
        metrics.backend_bout += sz;

        if !self.check_connections() {
            self.frontend_readiness.reset();
            self.backend_readiness.reset();
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
                self.frontend_readiness.reset();
                self.backend_readiness.reset();
                self.log_request_error(metrics, "splice back socket write error");
                return SessionResult::Close;
            }
            SocketResult::Closed => {
                self.frontend_readiness.reset();
                self.backend_readiness.reset();
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
            self.frontend_readiness.interest.insert(Ready::WRITABLE);
            return SessionResult::Continue;
        }

        let pipe_write_end = self.splice_pipe.as_ref().unwrap().out_pipe[1];
        let (size, remaining) = match self.backend_socket.as_ref() {
            Some(b) => splice::splice_in(b, pipe_write_end, capacity),
            None => return SessionResult::Continue,
        };

        debug!("{} Spliced {} bytes from backend", log_context!(self), size);

        if remaining != SocketResult::Continue || size == 0 {
            self.backend_readiness.event.remove(Ready::READABLE);
        }
        if size > 0 {
            self.splice_pipe.as_mut().unwrap().out_pipe_pending += size;
            self.frontend_readiness.interest.insert(Ready::WRITABLE);
            count!("back_bytes_in", size as i64);
            metrics.backend_bin += size;
        }

        if size == 0 && remaining == SocketResult::Closed {
            self.backend_status = match self.backend_status {
                ConnectionStatus::Normal => ConnectionStatus::WriteOpen,
                ConnectionStatus::ReadOpen => ConnectionStatus::Closed,
                s => s,
            };

            if !self.check_connections() {
                self.frontend_readiness.reset();
                self.backend_readiness.reset();
                self.log_request_success(metrics);
                return SessionResult::Close;
            }
        }

        match remaining {
            SocketResult::Error => {
                self.frontend_readiness.reset();
                self.backend_readiness.reset();
                self.log_request_error(metrics, "splice back socket read error");
                return SessionResult::Close;
            }
            SocketResult::Closed => {
                if !self.check_connections() {
                    self.frontend_readiness.reset();
                    self.backend_readiness.reset();
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

            incr!("http.infinite_loop.error");
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
