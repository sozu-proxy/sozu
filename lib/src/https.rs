//! HTTPS proxy entry point.
//!
//! Owns the TLS listener config (rustls), the ALPN-driven post-handshake
//! mux dispatch (`h2` → `ConnectionH2`, `http/1.1` → `ConnectionH1`,
//! neither → reject + `https.alpn.rejected.{unsupported,http11_disabled}`
//! metrics), the SNI binding policy (`strict_sni_binding`), and the
//! listener-update surface called from the command socket. Front-end H2
//! is gated by ALPN here; `cluster.http2` is a backend-capability hint.
//! Frontend rustls handshake I/O lives in `lib/src/protocol/rustls.rs`;
//! certificate resolution lives in `lib/src/tls.rs`.

use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, hash_map::Entry},
    io::ErrorKind,
    net::{Shutdown, SocketAddr as StdSocketAddr},
    os::unix::io::AsRawFd,
    rc::{Rc, Weak},
    str::{from_utf8, from_utf8_unchecked},
    sync::Arc,
    time::{Duration, Instant},
};

use mio::{
    Interest, Registry, Token,
    net::{TcpListener as MioTcpListener, TcpStream as MioTcpStream},
    unix::SourceFd,
};
use rustls::{
    CipherSuite, ProtocolVersion, ServerConfig as RustlsServerConfig, ServerConnection,
    SupportedCipherSuite, crypto::CryptoProvider,
};
use rusty_ulid::Ulid;
use sozu_command::{
    certificate::Fingerprint,
    config::{DEFAULT_ALPN_PROTOCOLS, DEFAULT_CIPHER_LIST},
    proto::command::{
        AddCertificate, CertificateSummary, CertificatesByAddress, Cluster, HttpsListenerConfig,
        ListOfCertificatesByAddress, ListenerType, RemoveCertificate, RemoveListener,
        ReplaceCertificate, RequestHttpFrontend, ResponseContent, TlsVersion,
        UpdateHttpsListenerConfig, WorkerRequest, WorkerResponse, request::RequestType,
        response_content::ContentType,
    },
    ready::Ready,
    response::HttpFrontend,
    state::{
        ClusterId, validate_alpn_protocols, validate_h2_flood_knobs_https, validate_sozu_id_header,
    },
};

use crate::{
    AcceptError, CachedTags, FrontendFromRequestError, L7ListenerHandler, L7Proxy, ListenerError,
    ListenerHandler, Protocol, ProxyConfiguration, ProxyError, ProxySession, SessionIsToBeClosed,
    SessionMetrics, SessionResult, StateMachineBuilder, StateResult,
    backends::BackendMap,
    crypto::{cipher_suite_by_name, default_provider, kx_group_by_name},
    pool::Pool,
    protocol::{
        Pipe, SessionState,
        http::answers::HttpAnswers,
        http::parser::{Method, hostname_and_port},
        mux::{self, Mux, MuxTls},
        proxy_protocol::expect::ExpectProxyProtocol,
        rustls::TlsHandshake,
    },
    router::{RouteResult, Router},
    server::{ListenToken, SessionManager},
    socket::{FrontRustls, server_bind},
    timer::TimeoutContainer,
    tls::MutexCertificateResolver,
    util::UnwrapLog,
};

StateMachineBuilder! {
    /// The various Stages of an HTTPS connection:
    ///
    /// - optional (ExpectProxyProtocol)
    /// - TLS handshake
    /// - HTTP or HTTP2 (via Mux)
    /// - WebSocket (passthrough), only from HTTP/1.1
    enum HttpsStateMachine impl SessionState {
        Expect(ExpectProxyProtocol<MioTcpStream>, ServerConnection),
        Handshake(TlsHandshake),
        Mux(MuxTls),
        WebSocket(Pipe<FrontRustls, HttpsListener>),
    }
}

enum AlpnProtocol {
    H2,
    Http11,
}

/// Module-level prefix for log lines emitted from this file when no session
/// is in scope. Produces a bold bright-white `HTTPS` label in colored mode.
/// Used by [`HttpsProxy`] / [`HttpsListener`] callbacks (`notify`,
/// `add_cluster`, `add_*_frontend`, `accept`, `soft_stop`, `hard_stop`)
/// which own a token map keyed by listener and have no `frontend_token` of
/// their own.
macro_rules! log_module_context {
    () => {{
        let (open, reset, _, _, _) = sozu_command::logging::ansi_palette();
        format!("{open}HTTPS{reset}\t >>>", open = open, reset = reset)
    }};
}

/// Per-session prefix for log lines emitted with an [`HttpsSession`] in
/// scope. Renders the canonical `\tHTTPS\tSession(...)\t >>>` envelope from
/// the session's `frontend_token` and `peer_address`. Operators can grep-
/// correlate against the token id (and the peer address when present)
/// across log lines for the same TLS connection.
macro_rules! log_context {
    ($self:expr) => {{
        let (open, reset, grey, gray, white) = sozu_command::logging::ansi_palette();
        format!(
            "{open}HTTPS{reset}\t{grey}Session{reset}({gray}frontend{reset}={white}{frontend}{reset}, {gray}peer{reset}={white}{peer}{reset})\t >>>",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            frontend = $self.frontend_token.0,
            peer = $self.peer_address.map(|a| a.to_string()).unwrap_or_else(|| "<none>".to_string()),
        )
    }};
}

pub struct HttpsSession {
    configured_backend_timeout: Duration,
    configured_connect_timeout: Duration,
    configured_frontend_timeout: Duration,
    frontend_token: Token,
    has_been_closed: bool,
    last_event: Instant,
    listener: Rc<RefCell<HttpsListener>>,
    metrics: SessionMetrics,
    peer_address: Option<StdSocketAddr>,
    pool: Weak<RefCell<Pool>>,
    proxy: Rc<RefCell<HttpsProxy>>,
    public_address: StdSocketAddr,
    state: HttpsStateMachine,
}

impl HttpsSession {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        configured_backend_timeout: Duration,
        configured_connect_timeout: Duration,
        configured_frontend_timeout: Duration,
        configured_request_timeout: Duration,
        expect_proxy: bool,
        listener: Rc<RefCell<HttpsListener>>,
        pool: Weak<RefCell<Pool>>,
        proxy: Rc<RefCell<HttpsProxy>>,
        public_address: StdSocketAddr,
        rustls_details: ServerConnection,
        sock: MioTcpStream,
        token: Token,
        wait_time: Duration,
    ) -> HttpsSession {
        let peer_address = if expect_proxy {
            // Will be defined later once the expect proxy header has been received and parsed
            None
        } else {
            sock.peer_addr().ok()
        };

        let request_id = Ulid::generate();
        let container_frontend_timeout = TimeoutContainer::new(configured_request_timeout, token);

        let state = if expect_proxy {
            trace!("{} starting in expect proxy state", log_module_context!());
            gauge_add!("protocol.proxy.expect", 1);
            HttpsStateMachine::Expect(
                ExpectProxyProtocol::new(container_frontend_timeout, sock, token, request_id),
                rustls_details,
            )
        } else {
            gauge_add!("protocol.tls.handshake", 1);
            HttpsStateMachine::Handshake(TlsHandshake::new(
                container_frontend_timeout,
                rustls_details,
                sock,
                token,
                request_id,
                peer_address,
            ))
        };

        let metrics = SessionMetrics::new(Some(wait_time));
        HttpsSession {
            configured_backend_timeout,
            configured_connect_timeout,
            configured_frontend_timeout,
            frontend_token: token,
            has_been_closed: false,
            last_event: Instant::now(),
            listener,
            metrics,
            peer_address,
            pool,
            proxy,
            public_address,
            state,
        }
    }

    pub fn upgrade(&mut self) -> SessionIsToBeClosed {
        debug!("{} upgrade", log_context!(self));
        let new_state = match self.state.take() {
            HttpsStateMachine::Expect(expect, ssl) => self.upgrade_expect(expect, ssl),
            HttpsStateMachine::Handshake(handshake) => self.upgrade_handshake(handshake),
            HttpsStateMachine::Mux(mux) => self.upgrade_mux(mux),
            HttpsStateMachine::WebSocket(wss) => self.upgrade_websocket(wss),
            HttpsStateMachine::FailedUpgrade(_) => {
                // Reaching this arm means a prior upgrade already returned
                // `None` and the session should have been closed. Fall back
                // to closing cleanly instead of panicking the worker.
                error!(
                    "{} upgrade called on FailedUpgrade state; closing session",
                    log_context!(self)
                );
                None
            }
        };

        match new_state {
            Some(state) => {
                self.state = state;
                false
            }
            // The state stays FailedUpgrade, but the Session should be closed right after
            None => true,
        }
    }

    fn upgrade_expect(
        &mut self,
        mut expect: ExpectProxyProtocol<MioTcpStream>,
        ssl: ServerConnection,
    ) -> Option<HttpsStateMachine> {
        if let Some(ref addresses) = expect.addresses {
            if let (Some(public_address), Some(session_address)) =
                (addresses.destination(), addresses.source())
            {
                self.public_address = public_address;
                self.peer_address = Some(session_address);

                let ExpectProxyProtocol {
                    container_frontend_timeout,
                    frontend,
                    frontend_readiness: readiness,
                    request_id,
                    ..
                } = expect;

                let mut handshake = TlsHandshake::new(
                    container_frontend_timeout,
                    ssl,
                    frontend,
                    self.frontend_token,
                    request_id,
                    self.peer_address,
                );
                // Transfer both interest and event from the proxy protocol state,
                // so the event loop properly monitors the socket after the transition.
                handshake.frontend_readiness = readiness;
                handshake.frontend_readiness.event.insert(Ready::READABLE);

                gauge_add!("protocol.proxy.expect", -1);
                gauge_add!("protocol.tls.handshake", 1);
                return Some(HttpsStateMachine::Handshake(handshake));
            }
        }

        // currently, only happens in expect proxy protocol with AF_UNSPEC address
        if !expect.container_frontend_timeout.cancel() {
            error!(
                "{} failed to cancel request timeout on expect upgrade phase for 'expect proxy protocol with AF_UNSPEC address'",
                log_context!(self)
            );
        }

        None
    }

    fn upgrade_handshake(&mut self, handshake: TlsHandshake) -> Option<HttpsStateMachine> {
        // Capture the SNI as an owned, already-lowercased String so it outlives
        // the `handshake.session` move below. Lowercasing here once avoids
        // doing it on every route decision (RFC 9110 §4.2.3 says hostnames are
        // case-insensitive); no port is ever part of an SNI value (RFC 6066
        // §3 — `HostName` is a dns_name, no port).
        // RFC 1034 §3.1 absolute-form: `example.com.` and `example.com`
        // are the same host. rustls hands us the wire-form SNI verbatim;
        // strip a single trailing dot so a legitimate client emitting
        // absolute-form SNI does not get its
        // `host` / `:authority` rejected by `authority_matches_sni` for a
        // length mismatch. Empty / no-SNI is unaffected.
        let sni_owned: Option<String> = handshake
            .session
            .server_name()
            .map(|s| s.to_ascii_lowercase())
            .map(|mut s| {
                if s.ends_with('.') {
                    s.pop();
                }
                s
            });
        let alpn = handshake.session.alpn_protocol();
        let alpn = alpn.and_then(|alpn| from_utf8(alpn).ok());
        debug!(
            "{} successful TLS handshake with, received: {:?} {:?}",
            log_context!(self),
            sni_owned,
            alpn
        );

        // Reject clients that fail to negotiate `h2` when the listener is
        // configured as H2-only: silently falling back to HTTP/1.1 would let a
        // downgrade-capable peer bypass H2-specific protections advertised
        // for this listener (Pass 5 Medium #4 of the security audit).
        let disable_http11 = self.listener.borrow().is_http11_disabled();
        // Pair the parsed AlpnProtocol with the on-the-wire label so the
        // access log can record it as a `&'static str` without re-stringifying
        // the protocol enum on every request. Unknown ALPN values still bail
        // out below — only successful negotiations propagate to the log.
        let (alpn, alpn_label): (AlpnProtocol, Option<&'static str>) = match alpn {
            Some("http/1.1") => {
                if disable_http11 {
                    incr!("https.alpn.rejected.http11_disabled");
                    warn!(
                        "{} rejecting TLS connection: listener is H2-only but client negotiated http/1.1",
                        log_context!(self)
                    );
                    return None;
                }
                (AlpnProtocol::Http11, Some("http/1.1"))
            }
            Some("h2") => (AlpnProtocol::H2, Some("h2")),
            Some(other) => {
                // This branch was not metered, so any operator dashboard
                // graphing `https.alpn.rejected.*`
                // missed unknown-protocol refusals (e.g. an `h3` mistake
                // bleeding through some misconfiguration). Add a dedicated
                // counter so the SOC's "ALPN refusal" ratebar matches the
                // sum of the labelled buckets.
                incr!("https.alpn.rejected.unsupported");
                error!(
                    "{} unsupported ALPN protocol: {}",
                    log_context!(self),
                    other
                );
                return None;
            }
            // Some clients don't fill in the ALPN protocol. By default we
            // downgrade to HTTP/1.1 to preserve compatibility; on an H2-only
            // listener we instead drop the connection.
            None => {
                if disable_http11 {
                    incr!("https.alpn.rejected.http11_disabled");
                    warn!(
                        "{} rejecting TLS connection: listener is H2-only but client did not negotiate ALPN",
                        log_context!(self)
                    );
                    return None;
                }
                (AlpnProtocol::Http11, None)
            }
        };

        // Capture the negotiated TLS metadata as `&'static str` labels for the
        // access log alongside the existing metric counters. Both calls are
        // single rustls accessors — duplicating them keeps the metric path
        // unchanged and avoids mutating-after-move on `handshake.session`.
        let tls_version_label = handshake
            .session
            .protocol_version()
            .and_then(rustls_version_label);
        let tls_cipher_label = handshake
            .session
            .negotiated_cipher_suite()
            .and_then(rustls_ciphersuite_label);
        if let Some(version) = handshake.session.protocol_version() {
            incr!(rustls_version_str(version));
        };
        if let Some(cipher) = handshake.session.negotiated_cipher_suite() {
            incr!(rustls_ciphersuite_str(cipher));
        };

        gauge_add!("protocol.tls.handshake", -1);

        let session_ulid = rusty_ulid::Ulid::generate();
        let front_stream = FrontRustls {
            stream: handshake.stream,
            session: handshake.session,
            peer_disconnected: false,
            peer_reset: false,
            session_ulid,
        };
        let router = mux::Router::new(
            self.configured_backend_timeout,
            self.configured_connect_timeout,
        );
        let mut context = mux::Context::new(
            session_ulid,
            self.pool.clone(),
            self.listener.clone(),
            self.peer_address,
            self.public_address,
        );
        // Bind the TLS SNI to this session so the routing layer can reject any
        // H2 stream whose `:authority` crosses the TLS trust boundary (see
        // `route_from_request`).
        context.tls_server_name = sni_owned;
        // Stamp the connection-scoped TLS metadata so every per-stream
        // HttpContext created by `Context::create_stream` inherits it for
        // the access log without re-querying rustls.
        context.tls_version = tls_version_label;
        context.tls_cipher = tls_cipher_label;
        context.tls_alpn = alpn_label;
        let mut frontend = match alpn {
            AlpnProtocol::Http11 => {
                incr!("http.alpn.http11");
                context.create_stream(handshake.request_id, 1 << 16)?;
                mux::Connection::new_h1_server(
                    session_ulid,
                    front_stream,
                    handshake.container_frontend_timeout,
                )
            }
            AlpnProtocol::H2 => {
                incr!("http.alpn.h2");
                let flood_config = self.listener.borrow().get_h2_flood_config();
                let connection_config = self.listener.borrow().get_h2_connection_config();
                let stream_idle_timeout = self.listener.borrow().get_h2_stream_idle_timeout();
                let graceful_shutdown_deadline =
                    self.listener.borrow().get_h2_graceful_shutdown_deadline();
                mux::Connection::new_h2_server(
                    session_ulid,
                    front_stream,
                    self.pool.clone(),
                    handshake.container_frontend_timeout,
                    flood_config,
                    connection_config,
                    stream_idle_timeout,
                    graceful_shutdown_deadline,
                )?
            }
        };
        // Ensure the upgraded connection can both read and write immediately.
        // With TLS 1.3 + NewSessionTicket, the upgrade may happen from writable()
        // where READABLE is no longer in the event (consumed by the prior readable()
        // call). The HTTP/2 preface may already be in rustls's plaintext buffer
        // (not on the TCP socket), so no new READABLE event from epoll will arrive.
        // Without WRITABLE in the event, the H2 state machine cannot transition from
        // reading the preface to writing SETTINGS, causing a deadlock with clients
        // (like hyper) that wait for the server's SETTINGS before proceeding.
        frontend
            .readiness_mut()
            .event
            .insert(Ready::READABLE | Ready::WRITABLE);

        gauge_add!("protocol.https", 1);
        Some(HttpsStateMachine::Mux(Mux {
            configured_frontend_timeout: self.configured_frontend_timeout,
            frontend_token: self.frontend_token,
            frontend,
            context,
            router,
            session_ulid,
        }))
    }

    fn upgrade_mux(&self, mut mux: MuxTls) -> Option<HttpsStateMachine> {
        debug!("{} mux switching to wss", log_context!(self));
        let Some(stream) = mux.context.streams.pop() else {
            error!(
                "{} upgrade_mux: no stream attached to the TLS mux session, closing",
                log_context!(self)
            );
            return None;
        };
        // http.active_requests was already decremented by generate_access_log()
        // in h1.rs before MuxResult::Upgrade was returned to us.

        let (frontend_readiness, frontend_socket, mut container_frontend_timeout) =
            match mux.frontend {
                mux::Connection::H1(mux::ConnectionH1 {
                    readiness,
                    socket,
                    timeout_container,
                    ..
                }) => (readiness, socket, timeout_container),
                mux::Connection::H2(_) => {
                    error!(
                        "{} only h1<->h1 connections can upgrade to websocket",
                        log_context!(self)
                    );
                    return None;
                }
            };

        let mux::StreamState::Linked(back_token) = stream.state else {
            error!(
                "{} upgrading stream should be linked to a backend",
                log_context!(self)
            );
            return None;
        };
        let Some(backend) = mux.router.backends.remove(&back_token) else {
            error!(
                "{} upgrade_mux: backend for token {:?} is missing (already disconnected?), closing",
                log_context!(self),
                back_token
            );
            return None;
        };
        let (cluster_id, backend, backend_readiness, backend_socket, mut container_backend_timeout) =
            match backend {
                mux::Connection::H1(mux::ConnectionH1 {
                    position:
                        mux::Position::Client(cluster_id, backend, mux::BackendStatus::Connected),
                    readiness,
                    socket,
                    timeout_container,
                    ..
                }) => (cluster_id, backend, readiness, socket, timeout_container),
                mux::Connection::H1(_) => {
                    error!(
                        "{} the backend disconnected just after upgrade, abort",
                        log_context!(self)
                    );
                    return None;
                }
                mux::Connection::H2(_) => {
                    error!(
                        "{} only h1<->h1 connections can upgrade to websocket",
                        log_context!(self)
                    );
                    return None;
                }
            };

        let ws_context = stream.context.websocket_context();

        container_frontend_timeout.reset();
        container_backend_timeout.reset();

        let backend_id = backend.borrow().backend_id.clone();
        // Unwrap the `SessionTcpStream` that the mux put around every backend
        // TCP socket — `Pipe::backend_socket` is typed `Option<TcpStream>`.
        let backend_socket = backend_socket.stream;
        let mut pipe = Pipe::new(
            stream.back.storage.buffer,
            Some(backend_id),
            Some(backend_socket),
            Some(backend),
            Some(container_backend_timeout),
            Some(container_frontend_timeout),
            Some(cluster_id),
            stream.front.storage.buffer,
            self.frontend_token,
            frontend_socket,
            self.listener.clone(),
            Protocol::HTTPS,
            stream.context.session_id,
            stream.context.id,
            stream.context.session_address,
            ws_context,
        );

        pipe.frontend_readiness.event = frontend_readiness.event;
        pipe.backend_readiness.event = backend_readiness.event;
        pipe.set_back_token(back_token);
        // Carry the connection-scoped TLS metadata captured at handshake time
        // into the post-upgrade WSS pipe so its access log records the same
        // version/cipher/sni/alpn the H1 request log already emitted. `clone`
        // on the SNI is the only heap touch — the other three are
        // `&'static str` borrows into the rustls label tables.
        pipe.set_tls_metadata(
            stream.context.tls_version,
            stream.context.tls_cipher,
            stream.context.tls_server_name.clone(),
            stream.context.tls_alpn,
        );

        // http.active_requests was already decremented by generate_access_log()
        // in h1.rs when the 101 response was written (before MuxResult::Upgrade).
        gauge_add!("protocol.https", -1);
        gauge_add!("protocol.wss", 1);
        gauge_add!("websocket.active_requests", 1);
        Some(HttpsStateMachine::WebSocket(pipe))
    }

    fn upgrade_websocket(
        &self,
        wss: Pipe<FrontRustls, HttpsListener>,
    ) -> Option<HttpsStateMachine> {
        // what do we do here?
        error!(
            "{} upgrade called on WSS, this should not happen",
            log_context!(self)
        );
        Some(HttpsStateMachine::WebSocket(wss))
    }
}

impl ProxySession for HttpsSession {
    fn close(&mut self) {
        if self.has_been_closed {
            return;
        }

        trace!("{} closing HTTPS session", log_context!(self));
        self.metrics.service_stop();

        // Restore gauges
        match self.state.marker() {
            StateMarker::Expect => gauge_add!("protocol.proxy.expect", -1),
            StateMarker::Handshake => gauge_add!("protocol.tls.handshake", -1),
            StateMarker::Mux => gauge_add!("protocol.https", -1),
            StateMarker::WebSocket => {
                gauge_add!("protocol.wss", -1);
                gauge_add!("websocket.active_requests", -1);
            }
        }

        if self.state.failed() {
            match self.state.marker() {
                StateMarker::Expect => incr!("https.upgrade.expect.failed"),
                StateMarker::Handshake => incr!("https.upgrade.handshake.failed"),
                StateMarker::Mux => incr!("https.upgrade.mux.failed"),
                StateMarker::WebSocket => incr!("https.upgrade.wss.failed"),
            }
            // FailedUpgrade means the socket was consumed by a failed upgrade
            // attempt, so we can only close the state (no-op) and remove the
            // session — cancel_timeouts / front_socket are unreachable.
            self.state.close(self.proxy.clone(), &mut self.metrics);
            self.proxy.borrow().remove_session(self.frontend_token);
            self.has_been_closed = true;
            return;
        }

        self.state.cancel_timeouts();
        // defer backend closing to the state
        // in case of https it should also send a close notify on the client before the socket is closed below
        self.state.close(self.proxy.clone(), &mut self.metrics);

        // Shut down the write side only. shutdown(Both) includes SHUT_RD which
        // discards unread data in the receive buffer (e.g. client's GOAWAY, ACKs).
        // On Linux, close() after SHUT_RD with discarded receive data sends TCP RST
        // instead of FIN, destroying any data still in the send buffer — including
        // TLS records that the drain loop just flushed. Using SHUT_WR only sends
        // FIN after all send buffer data is delivered, preserving the response.
        let front_socket = self.state.front_socket();
        if let Err(e) = front_socket.shutdown(Shutdown::Write) {
            // error 107 NotConnected can happen when was never fully connected, or was already disconnected due to error
            if e.kind() != ErrorKind::NotConnected {
                error!(
                    "{} error shutting down front socket({:?}): {:?}",
                    log_context!(self),
                    front_socket,
                    e
                );
            }
        }

        // deregister the frontend and remove it
        let proxy = self.proxy.borrow();
        let fd = front_socket.as_raw_fd();
        if let Err(e) = proxy.registry.deregister(&mut SourceFd(&fd)) {
            error!(
                "{} error deregistering front socket({:?}) while closing HTTPS session: {:?}",
                log_context!(self),
                fd,
                e
            );
        }
        proxy.remove_session(self.frontend_token);

        self.has_been_closed = true;
    }

    fn timeout(&mut self, token: Token) -> SessionIsToBeClosed {
        let session_result = self.state.timeout(token, &mut self.metrics);
        if session_result == StateResult::CloseSession {
            debug!(
                "{} HTTPS timeout requested close: token={:?}, marker={:?}",
                log_context!(self),
                token,
                self.state.marker()
            );
        }
        session_result == StateResult::CloseSession
    }

    fn protocol(&self) -> Protocol {
        Protocol::HTTPS
    }

    fn update_readiness(&mut self, token: Token, events: Ready) {
        trace!(
            "{} token {:?} got event {}",
            log_context!(self),
            token,
            super::ready_to_string(events)
        );
        self.last_event = Instant::now();
        self.metrics.wait_start();
        self.state.update_readiness(token, events);
    }

    fn ready(&mut self, session: Rc<RefCell<dyn ProxySession>>) -> SessionIsToBeClosed {
        self.metrics.service_start();

        let session_result =
            self.state
                .ready(session.clone(), self.proxy.clone(), &mut self.metrics);

        let to_be_closed = match session_result {
            SessionResult::Close => true,
            SessionResult::Continue => false,
            SessionResult::Upgrade => match self.upgrade() {
                false => self.ready(session),
                true => true,
            },
        };
        if to_be_closed {
            debug!(
                "{} HTTPS ready requested close: marker={:?}",
                log_context!(self),
                self.state.marker()
            );
        }

        self.metrics.service_stop();
        to_be_closed
    }

    fn shutting_down(&mut self) -> SessionIsToBeClosed {
        self.state.shutting_down()
    }

    fn last_event(&self) -> Instant {
        self.last_event
    }

    fn print_session(&self) {
        self.state.print_state("HTTPS");
        error!("{} Metrics: {:?}", log_context!(self), self.metrics);
    }

    fn frontend_token(&self) -> Token {
        self.frontend_token
    }
}

pub type HostName = String;
pub type PathBegin = String;

pub struct HttpsListener {
    active: bool,
    address: StdSocketAddr,
    answers: Rc<RefCell<HttpAnswers>>,
    config: HttpsListenerConfig,
    fronts: Router,
    listener: Option<MioTcpListener>,
    resolver: Arc<MutexCertificateResolver>,
    rustls_details: Arc<RustlsServerConfig>,
    tags: BTreeMap<String, CachedTags>,
    token: Token,
}

impl ListenerHandler for HttpsListener {
    fn get_addr(&self) -> &StdSocketAddr {
        &self.address
    }

    fn get_tags(&self, key: &str) -> Option<&CachedTags> {
        self.tags.get(key)
    }

    fn set_tags(&mut self, key: String, tags: Option<BTreeMap<String, String>>) {
        match tags {
            Some(tags) => self.tags.insert(key, CachedTags::new(tags)),
            None => self.tags.remove(&key),
        };
    }

    fn protocol(&self) -> Protocol {
        Protocol::HTTPS
    }

    fn public_address(&self) -> StdSocketAddr {
        self.config
            .public_address
            .map(|addr| addr.into())
            .unwrap_or(self.address)
    }
}

impl L7ListenerHandler for HttpsListener {
    fn get_sticky_name(&self) -> &str {
        &self.config.sticky_name
    }

    fn get_sozu_id_header(&self) -> &str {
        self.config
            .sozu_id_header
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("Sozu-Id")
    }

    fn get_connect_timeout(&self) -> u32 {
        self.config.connect_timeout
    }

    fn frontend_from_request(
        &self,
        host: &str,
        uri: &str,
        method: &Method,
    ) -> Result<RouteResult, FrontendFromRequestError> {
        let start = Instant::now();
        let (remaining_input, (hostname, _)) = match hostname_and_port(host.as_bytes()) {
            Ok(tuple) => tuple,
            Err(parse_error) => {
                // parse_error contains a slice of given_host, which should NOT escape this scope
                return Err(FrontendFromRequestError::HostParse {
                    host: host.to_owned(),
                    error: parse_error.to_string(),
                });
            }
        };

        if remaining_input != &b""[..] {
            return Err(FrontendFromRequestError::InvalidCharsAfterHost(
                host.to_owned(),
            ));
        }

        // it is alright to call from_utf8_unchecked,
        // we already verified that there are only ascii
        // chars in there
        // SAFETY: `hostname` was just produced by `hostname_and_port` (see
        // `lib/src/protocol/kawa_h1/parser.rs:133`), which only accepts
        // bytes matching `is_hostname_char` (alphanumeric, `-`, `.`, plus
        // `_` under the tolerant-http1-parser feature). All accepted
        // bytes are ASCII (≤ 0x7F), so the slice is valid single-byte UTF-8.
        let host = unsafe { from_utf8_unchecked(hostname) };

        let route = self.fronts.lookup(host, uri, method).map_err(|e| {
            incr!("http.failed_backend_matching");
            FrontendFromRequestError::NoClusterFound(e)
        })?;

        let now = Instant::now();

        if let Some(cluster) = route.cluster_id.as_deref() {
            time!("frontend_matching_time", cluster, (now - start).as_millis());
        }

        Ok(route)
    }

    fn get_answers(&self) -> &Rc<RefCell<HttpAnswers>> {
        &self.answers
    }

    fn get_h2_flood_config(&self) -> crate::protocol::mux::H2FloodConfig {
        let defaults = crate::protocol::mux::H2FloodConfig::default();
        crate::protocol::mux::H2FloodConfig {
            max_rst_stream_per_window: self
                .config
                .h2_max_rst_stream_per_window
                .unwrap_or(defaults.max_rst_stream_per_window),
            max_ping_per_window: self
                .config
                .h2_max_ping_per_window
                .unwrap_or(defaults.max_ping_per_window),
            max_settings_per_window: self
                .config
                .h2_max_settings_per_window
                .unwrap_or(defaults.max_settings_per_window),
            max_empty_data_per_window: self
                .config
                .h2_max_empty_data_per_window
                .unwrap_or(defaults.max_empty_data_per_window),
            max_window_update_stream0_per_window: self
                .config
                .h2_max_window_update_stream0_per_window
                .unwrap_or(defaults.max_window_update_stream0_per_window),
            max_continuation_frames: self
                .config
                .h2_max_continuation_frames
                .unwrap_or(defaults.max_continuation_frames),
            max_glitch_count: self
                .config
                .h2_max_glitch_count
                .unwrap_or(defaults.max_glitch_count),
            max_rst_stream_lifetime: self
                .config
                .h2_max_rst_stream_lifetime
                .unwrap_or(defaults.max_rst_stream_lifetime),
            max_rst_stream_abusive_lifetime: self
                .config
                .h2_max_rst_stream_abusive_lifetime
                .unwrap_or(defaults.max_rst_stream_abusive_lifetime),
            max_rst_stream_emitted_lifetime: self
                .config
                .h2_max_rst_stream_emitted_lifetime
                .unwrap_or(defaults.max_rst_stream_emitted_lifetime),
            max_header_list_size: self
                .config
                .h2_max_header_list_size
                .unwrap_or(defaults.max_header_list_size),
            max_header_table_size: self
                .config
                .h2_max_header_table_size
                .unwrap_or(defaults.max_header_table_size),
        }
    }

    fn get_h2_connection_config(&self) -> crate::protocol::mux::H2ConnectionConfig {
        crate::protocol::mux::H2ConnectionConfig::from_optional(
            self.config.h2_initial_connection_window,
            self.config.h2_max_concurrent_streams,
            self.config.h2_stream_shrink_ratio,
        )
    }

    fn get_strict_sni_binding(&self) -> bool {
        // Phase 1D enforced SNI↔:authority binding unconditionally; this
        // listener knob preserves that behavior by default and lets
        // operators opt out when cross-SNI routing is intentional.
        //
        // Codex G10 note: `strict_sni_binding = false` theoretically allows
        // an attacker to present many distinct SNIs on the same TCP
        // connection. rustls 0.23 **bans TLS renegotiation outright** (see
        // `rustls::server::ClientHello` which is consumed during the initial
        // handshake only), so a single TCP connection gets exactly one SNI
        // for its lifetime — the cross-SNI-flood vector is not reachable in
        // practice. Kept documented here so a future rustls upgrade that
        // reintroduces renegotiation (vanishingly unlikely) surfaces the
        // assumption during review.
        self.config.strict_sni_binding.unwrap_or(true)
    }

    fn get_elide_x_real_ip(&self) -> bool {
        self.config.elide_x_real_ip.unwrap_or(false)
    }

    fn get_send_x_real_ip(&self) -> bool {
        self.config.send_x_real_ip.unwrap_or(false)
    }

    fn get_h2_stream_idle_timeout(&self) -> std::time::Duration {
        // Inherit `back_timeout` when the knob is unset so listeners tuned for
        // long-running backends do not cancel streams at the 30 s security
        // floor. The `max(30, …)` keeps the baseline slow-multiplex mitigation
        // when `back_timeout` is shorter than 30 s. Explicit values (including
        // ones below 30 s) win — operators under a slow-multiplex attack can
        // lower the per-stream deadline to cap buffer pinning.
        let seconds = self
            .config
            .h2_stream_idle_timeout_seconds
            .map(|s| u64::from(s.max(1)))
            .unwrap_or_else(|| u64::from(self.config.back_timeout).max(30));
        std::time::Duration::from_secs(seconds)
    }

    fn get_h2_graceful_shutdown_deadline(&self) -> Option<std::time::Duration> {
        match self.config.h2_graceful_shutdown_deadline_seconds {
            None => Some(std::time::Duration::from_secs(5)),
            Some(0) => None,
            Some(s) => Some(std::time::Duration::from_secs(u64::from(s))),
        }
    }
}

impl HttpsListener {
    /// Whether this listener rejects clients that do not negotiate `h2`
    /// via TLS ALPN (including those that omit ALPN). Reads the
    /// `disable_http11` knob; defaults to `false` to preserve the
    /// historical behavior where a missing ALPN silently downgrades
    /// to HTTP/1.1.
    pub fn is_http11_disabled(&self) -> bool {
        self.config.disable_http11.unwrap_or(false)
    }

    pub fn try_new(
        config: HttpsListenerConfig,
        token: Token,
    ) -> Result<HttpsListener, ListenerError> {
        let resolver = Arc::new(MutexCertificateResolver::default());

        let server_config = Arc::new(Self::create_rustls_context(&config, resolver.to_owned())?);

        let answers = {
            // Reconcile the legacy `http_answers` per-status fields with
            // the new template map: the new map wins on collision, the
            // legacy fields fill in any status the operator hasn't yet
            // migrated.
            let mut answers_map = config.answers.clone();
            if let Some(ref legacy) = config.http_answers {
                crate::protocol::http::answers::merge_legacy_into_map(&mut answers_map, legacy);
            }
            HttpAnswers::new(&answers_map)
                .map_err(|(name, error)| ListenerError::TemplateParse(name, error))?
        };

        Ok(HttpsListener {
            listener: None,
            address: config.address.into(),
            resolver,
            rustls_details: server_config,
            active: false,
            fronts: Router::new(),
            answers: Rc::new(RefCell::new(answers)),
            config,
            token,
            tags: BTreeMap::new(),
        })
    }

    pub fn activate(
        &mut self,
        registry: &Registry,
        tcp_listener: Option<MioTcpListener>,
    ) -> Result<Token, ListenerError> {
        if self.active {
            return Ok(self.token);
        }
        let address: StdSocketAddr = self.config.address.into();

        let mut listener = match tcp_listener {
            Some(tcp_listener) => tcp_listener,
            None => {
                server_bind(address).map_err(|server_bind_error| ListenerError::Activation {
                    address,
                    error: server_bind_error.to_string(),
                })?
            }
        };

        registry
            .register(&mut listener, self.token, Interest::READABLE)
            .map_err(ListenerError::SocketRegistration)?;

        self.listener = Some(listener);
        self.active = true;
        Ok(self.token)
    }

    pub fn create_rustls_context(
        config: &HttpsListenerConfig,
        resolver: Arc<MutexCertificateResolver>,
    ) -> Result<RustlsServerConfig, ListenerError> {
        let cipher_names = if config.cipher_list.is_empty() {
            DEFAULT_CIPHER_LIST.to_vec()
        } else {
            config
                .cipher_list
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
        };

        let ciphers = cipher_names
            .into_iter()
            .filter_map(|cipher| {
                cipher_suite_by_name(cipher).or_else(|| {
                    error!(
                        "{} unknown or unsupported cipher: {:?}",
                        log_module_context!(),
                        cipher
                    );
                    None
                })
            })
            .collect::<Vec<_>>();

        let versions = config
            .versions
            .iter()
            .filter_map(|version| match TlsVersion::try_from(*version) {
                Ok(TlsVersion::TlsV12) => Some(&rustls::version::TLS12),
                Ok(TlsVersion::TlsV13) => Some(&rustls::version::TLS13),
                Ok(other_version) => {
                    error!(
                        "{} unsupported TLS version {:?}",
                        log_module_context!(),
                        other_version
                    );
                    None
                }
                Err(_) => {
                    error!("{} unsupported TLS version", log_module_context!());
                    None
                }
            })
            .collect::<Vec<_>>();

        let kx_groups = if config.groups_list.is_empty() {
            default_provider().kx_groups
        } else {
            config
                .groups_list
                .iter()
                .filter_map(|group| match kx_group_by_name(group) {
                    Some(kx) => Some(kx),
                    None => {
                        debug!("key exchange group {:?} not supported by the compiled crypto provider, skipping", group);
                        None
                    }
                })
                .collect::<Vec<_>>()
        };

        let provider = CryptoProvider {
            cipher_suites: ciphers,
            kx_groups,
            ..default_provider()
        };

        let mut server_config = RustlsServerConfig::builder_with_provider(provider.into())
            .with_protocol_versions(&versions[..])
            .map_err(|err| ListenerError::BuildRustls(err.to_string()))?
            .with_no_client_auth()
            .with_cert_resolver(resolver);
        server_config.send_tls13_tickets = config.send_tls13_tickets as usize;

        server_config.alpn_protocols = if config.alpn_protocols.is_empty() {
            DEFAULT_ALPN_PROTOCOLS
                .iter()
                .map(|p| p.as_bytes().to_vec())
                .collect()
        } else {
            config
                .alpn_protocols
                .iter()
                .map(|p| p.as_bytes().to_vec())
                .collect()
        };

        Ok(server_config)
    }

    /// Apply a partial-update patch to this listener's live configuration.
    ///
    /// Fields absent in the patch (i.e. `None`) are preserved unchanged.
    /// If `alpn_protocols` is present the rustls `ServerConfig` is rebuilt —
    /// in-flight handshakes keep the old Arc; new ones see the new one.
    /// If `http_answers` is present only the listener-default templates are
    /// replaced; per-cluster overrides in `cluster_custom_answers` are kept.
    pub fn update_config(
        &mut self,
        patch: &UpdateHttpsListenerConfig,
    ) -> Result<(), ListenerError> {
        // Defense-in-depth validation: main-process ConfigState::dispatch
        // validates before scatter, but a raw protobuf client or state replay
        // may reach the worker without that check. `StateError` lifts into
        // `ListenerError` via `From` so `?` suffices.
        validate_h2_flood_knobs_https(patch)?;
        if let Some(ref alpn) = patch.alpn_protocols {
            validate_alpn_protocols(&alpn.values)?;
        }
        if let Some(ref hdr) = patch.sozu_id_header {
            validate_sozu_id_header(hdr)?;
        }

        // --- simple field patches ---
        if let Some(v) = patch.public_address {
            self.config.public_address = Some(v);
        }
        if let Some(v) = patch.expect_proxy {
            self.config.expect_proxy = v;
        }
        if let Some(ref v) = patch.sticky_name {
            self.config.sticky_name = v.to_owned();
        }
        if let Some(v) = patch.front_timeout {
            self.config.front_timeout = v;
        }
        if let Some(v) = patch.back_timeout {
            self.config.back_timeout = v;
        }
        if let Some(v) = patch.connect_timeout {
            self.config.connect_timeout = v;
        }
        if let Some(v) = patch.request_timeout {
            self.config.request_timeout = v;
        }
        if let Some(v) = patch.strict_sni_binding {
            self.config.strict_sni_binding = Some(v);
        }
        if let Some(v) = patch.disable_http11 {
            self.config.disable_http11 = Some(v);
        }
        if let Some(ref v) = patch.sozu_id_header {
            self.config.sozu_id_header = Some(v.to_owned());
        }
        if let Some(v) = patch.elide_x_real_ip {
            self.config.elide_x_real_ip = Some(v);
        }
        if let Some(v) = patch.send_x_real_ip {
            self.config.send_x_real_ip = Some(v);
        }

        // --- H2 flood knobs ---
        if let Some(v) = patch.h2_max_rst_stream_per_window {
            self.config.h2_max_rst_stream_per_window = Some(v);
        }
        if let Some(v) = patch.h2_max_ping_per_window {
            self.config.h2_max_ping_per_window = Some(v);
        }
        if let Some(v) = patch.h2_max_settings_per_window {
            self.config.h2_max_settings_per_window = Some(v);
        }
        if let Some(v) = patch.h2_max_empty_data_per_window {
            self.config.h2_max_empty_data_per_window = Some(v);
        }
        if let Some(v) = patch.h2_max_continuation_frames {
            self.config.h2_max_continuation_frames = Some(v);
        }
        if let Some(v) = patch.h2_max_glitch_count {
            self.config.h2_max_glitch_count = Some(v);
        }
        if let Some(v) = patch.h2_initial_connection_window {
            self.config.h2_initial_connection_window = Some(v);
        }
        if let Some(v) = patch.h2_max_concurrent_streams {
            self.config.h2_max_concurrent_streams = Some(v);
        }
        if let Some(v) = patch.h2_stream_shrink_ratio {
            self.config.h2_stream_shrink_ratio = Some(v);
        }
        if let Some(v) = patch.h2_max_rst_stream_lifetime {
            self.config.h2_max_rst_stream_lifetime = Some(v);
        }
        if let Some(v) = patch.h2_max_rst_stream_abusive_lifetime {
            self.config.h2_max_rst_stream_abusive_lifetime = Some(v);
        }
        if let Some(v) = patch.h2_max_rst_stream_emitted_lifetime {
            self.config.h2_max_rst_stream_emitted_lifetime = Some(v);
        }
        if let Some(v) = patch.h2_max_header_list_size {
            self.config.h2_max_header_list_size = Some(v);
        }
        if let Some(v) = patch.h2_max_header_table_size {
            self.config.h2_max_header_table_size = Some(v);
        }
        if let Some(v) = patch.h2_stream_idle_timeout_seconds {
            self.config.h2_stream_idle_timeout_seconds = Some(v);
        }
        if let Some(v) = patch.h2_graceful_shutdown_deadline_seconds {
            self.config.h2_graceful_shutdown_deadline_seconds = Some(v);
        }
        if let Some(v) = patch.h2_max_window_update_stream0_per_window {
            self.config.h2_max_window_update_stream0_per_window = Some(v);
        }

        // --- ALPN rebuild (may force a rustls ServerConfig rebuild) ---
        //
        // Transactional: build the candidate rustls context first using a
        // **cloned** config that carries the new ALPN. Only if the build
        // succeeds do we commit `self.config.alpn_protocols` and swap the
        // Arc. This ensures a rustls failure (crypto provider transient,
        // resolver error, etc.) leaves the listener observably unchanged —
        // the master-side state would still diverge from the worker-side
        // refusal, but the worker itself stays consistent.
        if let Some(ref alpn_wrapper) = patch.alpn_protocols {
            let mut candidate = self.config.clone();
            candidate.alpn_protocols = alpn_wrapper.values.clone();
            let new_rustls = Arc::new(Self::create_rustls_context(
                &candidate,
                self.resolver.clone(),
            )?);
            // Build succeeded — commit.
            self.config.alpn_protocols = alpn_wrapper.values.clone();
            self.rustls_details = new_rustls;
        }

        // HTTP answers: merge legacy `http_answers` and the new `answers`
        // map on top of the existing config, then rebuild the listener-level
        // template registry. Per-cluster overrides in
        // `HttpAnswers::cluster_answers` are preserved across the rebuild.
        let answers_changed = patch.http_answers.is_some() || !patch.answers.is_empty();
        if answers_changed {
            if let Some(ref new_answers) = patch.http_answers {
                crate::sozu_command::state::merge_custom_http_answers(
                    &mut self.config.http_answers,
                    new_answers,
                );
            }
            for (code, body) in &patch.answers {
                if !body.is_empty() {
                    self.config.answers.insert(code.clone(), body.clone());
                }
            }

            let mut answers_map = self.config.answers.clone();
            if let Some(ref legacy) = self.config.http_answers {
                crate::protocol::http::answers::merge_legacy_into_map(&mut answers_map, legacy);
            }
            let mut rebuilt = HttpAnswers::new(&answers_map)
                .map_err(|(name, error)| ListenerError::TemplateParse(name, error))?;
            let preserved = std::mem::take(&mut self.answers.borrow_mut().cluster_answers);
            rebuilt.cluster_answers = preserved;
            *self.answers.borrow_mut() = rebuilt;
        }

        Ok(())
    }

    pub fn add_https_front(&mut self, tls_front: HttpFrontend) -> Result<(), ListenerError> {
        self.fronts
            .add_http_front(&tls_front)
            .map_err(ListenerError::AddFrontend)
    }

    pub fn remove_https_front(&mut self, tls_front: HttpFrontend) -> Result<(), ListenerError> {
        debug!(
            "{} removing tls_front {:?}",
            log_module_context!(),
            tls_front
        );
        self.fronts
            .remove_http_front(&tls_front)
            .map_err(ListenerError::RemoveFrontend)
    }

    fn accept(&mut self) -> Result<MioTcpStream, AcceptError> {
        if let Some(ref sock) = self.listener {
            sock.accept()
                .map_err(|e| match e.kind() {
                    ErrorKind::WouldBlock => AcceptError::WouldBlock,
                    _ => {
                        error!("{} accept() IO error: {:?}", log_module_context!(), e);
                        AcceptError::IoError
                    }
                })
                .map(|(sock, _)| sock)
        } else {
            error!(
                "{} cannot accept connections, no listening socket available",
                log_module_context!()
            );
            Err(AcceptError::IoError)
        }
    }
}

pub struct HttpsProxy {
    listeners: HashMap<Token, Rc<RefCell<HttpsListener>>>,
    clusters: HashMap<ClusterId, Cluster>,
    backends: Rc<RefCell<BackendMap>>,
    pool: Rc<RefCell<Pool>>,
    registry: Registry,
    sessions: Rc<RefCell<SessionManager>>,
}

impl HttpsProxy {
    pub fn new(
        registry: Registry,
        sessions: Rc<RefCell<SessionManager>>,
        pool: Rc<RefCell<Pool>>,
        backends: Rc<RefCell<BackendMap>>,
    ) -> HttpsProxy {
        HttpsProxy {
            listeners: HashMap::new(),
            clusters: HashMap::new(),
            backends,
            pool,
            registry,
            sessions,
        }
    }

    pub fn add_listener(
        &mut self,
        config: HttpsListenerConfig,
        token: Token,
    ) -> Result<Token, ProxyError> {
        match self.listeners.entry(token) {
            Entry::Vacant(entry) => {
                let https_listener =
                    HttpsListener::try_new(config, token).map_err(ProxyError::AddListener)?;
                entry.insert(Rc::new(RefCell::new(https_listener)));
                Ok(token)
            }
            _ => Err(ProxyError::ListenerAlreadyPresent),
        }
    }

    pub fn remove_listener(
        &mut self,
        remove: RemoveListener,
    ) -> Result<Option<ResponseContent>, ProxyError> {
        let len = self.listeners.len();

        let remove_address = remove.address.into();
        self.listeners
            .retain(|_, listener| listener.borrow().address != remove_address);

        if !self.listeners.len() < len {
            info!(
                "{} no HTTPS listener to remove at address {}",
                log_module_context!(),
                remove_address
            )
        }
        Ok(None)
    }

    pub fn soft_stop(&mut self) -> Result<(), ProxyError> {
        let listeners: HashMap<_, _> = self.listeners.drain().collect();
        let mut socket_errors = vec![];
        for (_, l) in listeners.iter() {
            if let Some(mut sock) = l.borrow_mut().listener.take() {
                debug!("{} deregistering socket {:?}", log_module_context!(), sock);
                if let Err(e) = self.registry.deregister(&mut sock) {
                    let error = format!("socket {sock:?}: {e:?}");
                    socket_errors.push(error);
                }
            }
        }

        if !socket_errors.is_empty() {
            return Err(ProxyError::SoftStop {
                proxy_protocol: "HTTPS".to_string(),
                error: format!("Error deregistering listen sockets: {socket_errors:?}"),
            });
        }

        Ok(())
    }

    pub fn hard_stop(&mut self) -> Result<(), ProxyError> {
        let mut listeners: HashMap<_, _> = self.listeners.drain().collect();
        let mut socket_errors = vec![];
        for (_, l) in listeners.drain() {
            if let Some(mut sock) = l.borrow_mut().listener.take() {
                debug!("{} deregistering socket {:?}", log_module_context!(), sock);
                if let Err(e) = self.registry.deregister(&mut sock) {
                    let error = format!("socket {sock:?}: {e:?}");
                    socket_errors.push(error);
                }
            }
        }

        if !socket_errors.is_empty() {
            return Err(ProxyError::HardStop {
                proxy_protocol: "HTTPS".to_string(),
                error: format!("Error deregistering listen sockets: {socket_errors:?}"),
            });
        }

        Ok(())
    }

    pub fn query_all_certificates(&mut self) -> Result<Option<ResponseContent>, ProxyError> {
        let certificates = self
            .listeners
            .values()
            .map(|listener| {
                let owned = listener.borrow();
                let resolver = unwrap_msg!(owned.resolver.0.lock());
                let certificate_summaries = resolver
                    .domains
                    .to_hashmap()
                    .drain()
                    .map(|(k, fingerprint)| CertificateSummary {
                        domain: String::from_utf8(k).unwrap(),
                        fingerprint: fingerprint.to_string(),
                    })
                    .collect();

                CertificatesByAddress {
                    address: owned.address.into(),
                    certificate_summaries,
                }
            })
            .collect();

        info!(
            "{} got Certificates::All query, answering with {:?}",
            log_module_context!(),
            certificates
        );

        Ok(Some(
            ContentType::CertificatesByAddress(ListOfCertificatesByAddress { certificates }).into(),
        ))
    }

    pub fn query_certificate_for_domain(
        &mut self,
        domain: String,
    ) -> Result<Option<ResponseContent>, ProxyError> {
        let certificates = self
            .listeners
            .values()
            .map(|listener| {
                let owned = listener.borrow();
                let resolver = unwrap_msg!(owned.resolver.0.lock());
                let mut certificate_summaries = vec![];

                if let Some((k, fingerprint)) = resolver.domain_lookup(domain.as_bytes(), true) {
                    certificate_summaries.push(CertificateSummary {
                        domain: String::from_utf8(k.to_vec()).unwrap(),
                        fingerprint: fingerprint.to_string(),
                    });
                }
                CertificatesByAddress {
                    address: owned.address.into(),
                    certificate_summaries,
                }
            })
            .collect();

        info!(
            "{} got Certificates::Domain({}) query, answering with {:?}",
            log_module_context!(),
            domain,
            certificates
        );

        Ok(Some(
            ContentType::CertificatesByAddress(ListOfCertificatesByAddress { certificates }).into(),
        ))
    }

    pub fn activate_listener(
        &mut self,
        addr: &StdSocketAddr,
        tcp_listener: Option<MioTcpListener>,
    ) -> Result<Token, ProxyError> {
        let listener = self
            .listeners
            .values()
            .find(|listener| listener.borrow().address == *addr)
            .ok_or(ProxyError::NoListenerFound(addr.to_owned()))?;

        listener
            .borrow_mut()
            .activate(&self.registry, tcp_listener)
            .map_err(|listener_error| ProxyError::ListenerActivation {
                address: *addr,
                listener_error,
            })
    }

    pub fn give_back_listeners(&mut self) -> Vec<(StdSocketAddr, MioTcpListener)> {
        self.listeners
            .values()
            .filter_map(|listener| {
                let mut owned = listener.borrow_mut();
                if let Some(listener) = owned.listener.take() {
                    // Reset `active` so a subsequent `activate()` re-binds
                    // instead of short-circuiting on the stale flag.
                    owned.active = false;
                    return Some((owned.address, listener));
                }

                None
            })
            .collect()
    }

    pub fn give_back_listener(
        &mut self,
        address: StdSocketAddr,
    ) -> Result<(Token, MioTcpListener), ProxyError> {
        let listener = self
            .listeners
            .values()
            .find(|listener| listener.borrow().address == address)
            .ok_or(ProxyError::NoListenerFound(address))?;

        let mut owned = listener.borrow_mut();

        let taken_listener = owned
            .listener
            .take()
            .ok_or(ProxyError::UnactivatedListener)?;

        // Reset `active` so a subsequent `activate()` re-binds instead of
        // short-circuiting on the stale flag.
        owned.active = false;

        Ok((owned.token, taken_listener))
    }

    /// Apply a partial-update patch to the identified HTTPS listener.
    pub fn update_listener(&mut self, patch: UpdateHttpsListenerConfig) -> Result<(), ProxyError> {
        let address: std::net::SocketAddr = patch.address.into();
        let listener = self
            .listeners
            .values()
            .find(|l| l.borrow().address == address)
            .ok_or(ProxyError::NoListenerFound(address))?;
        listener
            .borrow_mut()
            .update_config(&patch)
            .map_err(|listener_error| ProxyError::ListenerActivation {
                address,
                listener_error,
            })
    }

    pub fn add_cluster(
        &mut self,
        mut cluster: Cluster,
    ) -> Result<Option<ResponseContent>, ProxyError> {
        let mut cluster_overrides = cluster.answers.clone();
        if let Some(answer_503) = cluster.answer_503.take() {
            cluster_overrides
                .entry("503".to_owned())
                .or_insert(answer_503);
        }
        if !cluster_overrides.is_empty() {
            for listener in self.listeners.values() {
                listener
                    .borrow()
                    .answers
                    .borrow_mut()
                    .add_cluster_answers(&cluster.cluster_id, &cluster_overrides)
                    .map_err(|(status, error)| {
                        ProxyError::AddCluster(ListenerError::TemplateParse(status, error))
                    })?;
            }
        }
        self.clusters.insert(cluster.cluster_id.clone(), cluster);
        Ok(None)
    }

    pub fn remove_cluster(
        &mut self,
        cluster_id: &str,
    ) -> Result<Option<ResponseContent>, ProxyError> {
        self.clusters.remove(cluster_id);
        for listener in self.listeners.values() {
            listener
                .borrow()
                .answers
                .borrow_mut()
                .remove_cluster_answers(cluster_id);
        }

        Ok(None)
    }

    pub fn add_https_frontend(
        &mut self,
        front: RequestHttpFrontend,
    ) -> Result<Option<ResponseContent>, ProxyError> {
        let front = front.clone().to_frontend().map_err(|request_error| {
            ProxyError::WrongInputFrontend {
                front: Box::new(front),
                error: request_error.to_string(),
            }
        })?;

        let mut listener = self
            .listeners
            .values()
            .find(|l| l.borrow().address == front.address)
            .ok_or(ProxyError::NoListenerFound(front.address))?
            .borrow_mut();

        listener.set_tags(front.hostname.to_owned(), front.tags.to_owned());
        listener
            .add_https_front(front)
            .map_err(ProxyError::AddFrontend)?;
        Ok(None)
    }

    pub fn remove_https_frontend(
        &mut self,
        front: RequestHttpFrontend,
    ) -> Result<Option<ResponseContent>, ProxyError> {
        let front = front.clone().to_frontend().map_err(|request_error| {
            ProxyError::WrongInputFrontend {
                front: Box::new(front),
                error: request_error.to_string(),
            }
        })?;

        let mut listener = self
            .listeners
            .values()
            .find(|l| l.borrow().address == front.address)
            .ok_or(ProxyError::NoListenerFound(front.address))?
            .borrow_mut();

        let hostname = front.hostname.to_owned();

        listener
            .remove_https_front(front)
            .map_err(ProxyError::RemoveFrontend)?;

        if !listener.fronts.has_hostname(&hostname) {
            listener.set_tags(hostname, None);
        }
        Ok(None)
    }

    pub fn add_certificate(
        &mut self,
        add_certificate: AddCertificate,
    ) -> Result<Option<ResponseContent>, ProxyError> {
        let address = add_certificate.address.into();

        let listener = self
            .listeners
            .values()
            .find(|l| l.borrow().address == address)
            .ok_or(ProxyError::NoListenerFound(address))?
            .borrow_mut();

        let mut resolver = listener
            .resolver
            .0
            .lock()
            .map_err(|e| ProxyError::Lock(e.to_string()))?;

        resolver
            .add_certificate(&add_certificate)
            .map_err(ProxyError::AddCertificate)?;

        Ok(None)
    }

    //FIXME: should return an error if certificate still has fronts referencing it
    pub fn remove_certificate(
        &mut self,
        remove_certificate: RemoveCertificate,
    ) -> Result<Option<ResponseContent>, ProxyError> {
        let address = remove_certificate.address.into();

        let fingerprint = Fingerprint(
            hex::decode(&remove_certificate.fingerprint)
                .map_err(ProxyError::WrongCertificateFingerprint)?,
        );

        let listener = self
            .listeners
            .values()
            .find(|l| l.borrow().address == address)
            .ok_or(ProxyError::NoListenerFound(address))?
            .borrow_mut();

        let mut resolver = listener
            .resolver
            .0
            .lock()
            .map_err(|e| ProxyError::Lock(e.to_string()))?;

        resolver
            .remove_certificate(&fingerprint)
            .map_err(ProxyError::RemoveCertificate)?;

        Ok(None)
    }

    //FIXME: should return an error if certificate still has fronts referencing it
    pub fn replace_certificate(
        &mut self,
        replace_certificate: ReplaceCertificate,
    ) -> Result<Option<ResponseContent>, ProxyError> {
        let address = replace_certificate.address.into();

        let listener = self
            .listeners
            .values()
            .find(|l| l.borrow().address == address)
            .ok_or(ProxyError::NoListenerFound(address))?
            .borrow_mut();

        let mut resolver = listener
            .resolver
            .0
            .lock()
            .map_err(|e| ProxyError::Lock(e.to_string()))?;

        resolver
            .replace_certificate(&replace_certificate)
            .map_err(ProxyError::ReplaceCertificate)?;

        Ok(None)
    }
}

impl ProxyConfiguration for HttpsProxy {
    fn accept(&mut self, token: ListenToken) -> Result<MioTcpStream, AcceptError> {
        match self.listeners.get(&Token(token.0)) {
            Some(listener) => listener.borrow_mut().accept(),
            None => Err(AcceptError::IoError),
        }
    }

    fn create_session(
        &mut self,
        mut frontend_sock: MioTcpStream,
        token: ListenToken,
        wait_time: Duration,
        proxy: Rc<RefCell<Self>>,
    ) -> Result<(), AcceptError> {
        let listener = self
            .listeners
            .get(&Token(token.0))
            .ok_or(AcceptError::IoError)?;
        if let Err(e) = frontend_sock.set_nodelay(true) {
            error!(
                "{} error setting nodelay on front socket({:?}): {:?}",
                log_module_context!(),
                frontend_sock,
                e
            );
        }

        let owned = listener.borrow();
        let rustls_details = ServerConnection::new(owned.rustls_details.clone()).map_err(|e| {
            error!(
                "{} failed to create server session: {:?}",
                log_module_context!(),
                e
            );
            AcceptError::IoError
        })?;

        let mut session_manager = self.sessions.borrow_mut();
        let entry = session_manager.slab.vacant_entry();
        let session_token = Token(entry.key());

        self.registry
            .register(
                &mut frontend_sock,
                session_token,
                Interest::READABLE | Interest::WRITABLE,
            )
            .map_err(|register_error| {
                error!(
                    "{} error registering front socket({:?}): {:?}",
                    log_module_context!(),
                    frontend_sock,
                    register_error
                );
                AcceptError::RegisterError
            })?;

        let public_address: StdSocketAddr = match owned.config.public_address {
            Some(pub_addr) => pub_addr.into(),
            None => owned.config.address.into(),
        };

        let session = Rc::new(RefCell::new(HttpsSession::new(
            Duration::from_secs(owned.config.back_timeout as u64),
            Duration::from_secs(owned.config.connect_timeout as u64),
            Duration::from_secs(owned.config.front_timeout as u64),
            Duration::from_secs(owned.config.request_timeout as u64),
            owned.config.expect_proxy,
            listener.clone(),
            Rc::downgrade(&self.pool),
            proxy,
            public_address,
            rustls_details,
            frontend_sock,
            session_token,
            wait_time,
        )));
        entry.insert(session);

        Ok(())
    }

    fn notify(&mut self, request: WorkerRequest) -> WorkerResponse {
        let request_id = request.id.clone();

        let request_type = match request.content.request_type {
            Some(t) => t,
            None => return WorkerResponse::error(request_id, "Empty request"),
        };

        let content_result = match request_type {
            RequestType::AddCluster(cluster) => {
                debug!(
                    "{} {} add cluster {:?}",
                    log_module_context!(),
                    request_id,
                    cluster
                );
                self.add_cluster(cluster)
            }
            RequestType::RemoveCluster(cluster_id) => {
                debug!(
                    "{} {} remove cluster {:?}",
                    log_module_context!(),
                    request_id,
                    cluster_id
                );
                self.remove_cluster(&cluster_id)
            }
            RequestType::AddHttpsFrontend(front) => {
                debug!(
                    "{} {} add https front {:?}",
                    log_module_context!(),
                    request_id,
                    front
                );
                self.add_https_frontend(front)
            }
            RequestType::RemoveHttpsFrontend(front) => {
                debug!(
                    "{} {} remove https front {:?}",
                    log_module_context!(),
                    request_id,
                    front
                );
                self.remove_https_frontend(front)
            }
            RequestType::AddCertificate(add_certificate) => {
                debug!(
                    "{} {} add certificate: {:?}",
                    log_module_context!(),
                    request_id,
                    add_certificate
                );
                self.add_certificate(add_certificate)
            }
            RequestType::RemoveCertificate(remove_certificate) => {
                debug!(
                    "{} {} remove certificate: {:?}",
                    log_module_context!(),
                    request_id,
                    remove_certificate
                );
                self.remove_certificate(remove_certificate)
            }
            RequestType::ReplaceCertificate(replace_certificate) => {
                debug!(
                    "{} {} replace certificate: {:?}",
                    log_module_context!(),
                    request_id,
                    replace_certificate
                );
                self.replace_certificate(replace_certificate)
            }
            RequestType::RemoveListener(remove) => {
                debug!(
                    "{} removing HTTPS listener at address {:?}",
                    log_module_context!(),
                    remove.address
                );
                self.remove_listener(remove)
            }
            RequestType::SoftStop(_) => {
                debug!(
                    "{} {} processing soft shutdown",
                    log_module_context!(),
                    request_id
                );
                match self.soft_stop() {
                    Ok(_) => {
                        info!(
                            "{} {} soft stop successful",
                            log_module_context!(),
                            request_id
                        );
                        return WorkerResponse::processing(request.id);
                    }
                    Err(e) => Err(e),
                }
            }
            RequestType::HardStop(_) => {
                debug!(
                    "{} {} processing hard shutdown",
                    log_module_context!(),
                    request_id
                );
                match self.hard_stop() {
                    Ok(_) => {
                        debug!(
                            "{} {} hard stop successful",
                            log_module_context!(),
                            request_id
                        );
                        return WorkerResponse::processing(request.id);
                    }
                    Err(e) => Err(e),
                }
            }
            RequestType::Status(_) => {
                debug!("{} {} status", log_module_context!(), request_id);
                Ok(None)
            }
            RequestType::QueryCertificatesFromWorkers(filters) => {
                if let Some(domain) = filters.domain {
                    debug!(
                        "{} {} query certificate for domain {}",
                        log_module_context!(),
                        request_id,
                        domain
                    );
                    self.query_certificate_for_domain(domain)
                } else {
                    debug!(
                        "{} {} query all certificates",
                        log_module_context!(),
                        request_id
                    );
                    self.query_all_certificates()
                }
            }
            other_request => {
                debug!(
                    "{} {} unsupported message for HTTPS proxy, ignoring {:?}",
                    log_module_context!(),
                    request.id,
                    other_request
                );
                Err(ProxyError::UnsupportedMessage)
            }
        };

        match content_result {
            Ok(content) => {
                debug!("{} {} successful", log_module_context!(), request_id);
                match content {
                    Some(content) => WorkerResponse::ok_with_content(request_id, content),
                    None => WorkerResponse::ok(request_id),
                }
            }
            Err(proxy_error) => {
                debug!(
                    "{} {} unsuccessful: {}",
                    log_module_context!(),
                    request_id,
                    proxy_error
                );
                WorkerResponse::error(request_id, proxy_error)
            }
        }
    }
}
impl L7Proxy for HttpsProxy {
    fn kind(&self) -> ListenerType {
        ListenerType::Https
    }

    fn register_socket(
        &self,
        socket: &mut MioTcpStream,
        token: Token,
        interest: Interest,
    ) -> Result<(), std::io::Error> {
        self.registry.register(socket, token, interest)
    }

    fn deregister_socket(&self, tcp_stream: &mut MioTcpStream) -> Result<(), std::io::Error> {
        self.registry.deregister(tcp_stream)
    }

    fn add_session(&self, session: Rc<RefCell<dyn ProxySession>>) -> Token {
        let mut session_manager = self.sessions.borrow_mut();
        let entry = session_manager.slab.vacant_entry();
        let token = Token(entry.key());
        let _entry = entry.insert(session);
        token
    }

    fn remove_session(&self, token: Token) -> bool {
        let mut sessions = self.sessions.borrow_mut();
        // Mirror of HttpProxy::remove_session — drain the per-(cluster,
        // source-IP) accounting before the slab slot is reused.
        sessions.untrack_all_cluster_ip(token);
        sessions.slab.try_remove(token.0).is_some()
    }

    fn backends(&self) -> Rc<RefCell<BackendMap>> {
        self.backends.clone()
    }

    fn clusters(&self) -> &HashMap<ClusterId, Cluster> {
        &self.clusters
    }

    fn sessions(&self) -> Rc<RefCell<SessionManager>> {
        self.sessions.clone()
    }
}

/// Used for metrics keeping
fn rustls_version_str(version: ProtocolVersion) -> &'static str {
    match version {
        ProtocolVersion::SSLv2 => "tls.version.SSLv2",
        ProtocolVersion::SSLv3 => "tls.version.SSLv3",
        ProtocolVersion::TLSv1_0 => "tls.version.TLSv1_0",
        ProtocolVersion::TLSv1_1 => "tls.version.TLSv1_1",
        ProtocolVersion::TLSv1_2 => "tls.version.TLSv1_2",
        ProtocolVersion::TLSv1_3 => "tls.version.TLSv1_3",
        ProtocolVersion::DTLSv1_0 => "tls.version.DTLSv1_0",
        ProtocolVersion::DTLSv1_2 => "tls.version.DTLSv1_2",
        ProtocolVersion::DTLSv1_3 => "tls.version.DTLSv1_3",
        ProtocolVersion::Unknown(_) => "tls.version.Unknown",
        _ => "tls.version.unimplemented",
    }
}

/// Short label suitable for access logs (e.g. `"TLSv1.3"`).
///
/// Distinct from [`rustls_version_str`] which prefixes with `tls.version.`
/// for metric ingestion. Returns `None` for variants Sōzu does not know how
/// to label, so the access log records `tls_version` as absent rather than
/// emitting a misleading `"unimplemented"` literal.
pub(crate) fn rustls_version_label(version: ProtocolVersion) -> Option<&'static str> {
    match version {
        ProtocolVersion::SSLv2 => Some("SSLv2"),
        ProtocolVersion::SSLv3 => Some("SSLv3"),
        ProtocolVersion::TLSv1_0 => Some("TLSv1.0"),
        ProtocolVersion::TLSv1_1 => Some("TLSv1.1"),
        ProtocolVersion::TLSv1_2 => Some("TLSv1.2"),
        ProtocolVersion::TLSv1_3 => Some("TLSv1.3"),
        ProtocolVersion::DTLSv1_0 => Some("DTLSv1.0"),
        ProtocolVersion::DTLSv1_2 => Some("DTLSv1.2"),
        ProtocolVersion::DTLSv1_3 => Some("DTLSv1.3"),
        _ => None,
    }
}

/// Used for metrics keeping
fn rustls_ciphersuite_str(cipher: SupportedCipherSuite) -> &'static str {
    match cipher.suite() {
        CipherSuite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256 => {
            "tls.cipher.TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256"
        }
        CipherSuite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256 => {
            "tls.cipher.TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256"
        }
        CipherSuite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256 => {
            "tls.cipher.TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256"
        }
        CipherSuite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384 => {
            "tls.cipher.TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384"
        }
        CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256 => {
            "tls.cipher.TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256"
        }
        CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384 => {
            "tls.cipher.TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384"
        }
        CipherSuite::TLS13_CHACHA20_POLY1305_SHA256 => "tls.cipher.TLS13_CHACHA20_POLY1305_SHA256",
        CipherSuite::TLS13_AES_256_GCM_SHA384 => "tls.cipher.TLS13_AES_256_GCM_SHA384",
        CipherSuite::TLS13_AES_128_GCM_SHA256 => "tls.cipher.TLS13_AES_128_GCM_SHA256",
        _ => "tls.cipher.Unsupported",
    }
}

/// Short label suitable for access logs (e.g. `"TLS_AES_128_GCM_SHA256"`).
///
/// Distinct from [`rustls_ciphersuite_str`] which prefixes with `tls.cipher.`
/// for metric ingestion. Returns `None` for cipher suites Sōzu does not know
/// how to label, so the access log records `tls_cipher` as absent rather
/// than emitting a misleading `"Unsupported"` literal.
pub(crate) fn rustls_ciphersuite_label(cipher: SupportedCipherSuite) -> Option<&'static str> {
    match cipher.suite() {
        CipherSuite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256 => {
            Some("TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256")
        }
        CipherSuite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256 => {
            Some("TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256")
        }
        CipherSuite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256 => {
            Some("TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256")
        }
        CipherSuite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384 => {
            Some("TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384")
        }
        CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256 => {
            Some("TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256")
        }
        CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384 => {
            Some("TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384")
        }
        CipherSuite::TLS13_CHACHA20_POLY1305_SHA256 => Some("TLS13_CHACHA20_POLY1305_SHA256"),
        CipherSuite::TLS13_AES_256_GCM_SHA384 => Some("TLS13_AES_256_GCM_SHA384"),
        CipherSuite::TLS13_AES_128_GCM_SHA256 => Some("TLS13_AES_128_GCM_SHA256"),
        _ => None,
    }
}

pub mod testing {
    use crate::testing::*;

    /// this function is not used, but is available for example and testing purposes
    pub fn start_https_worker(
        config: HttpsListenerConfig,
        channel: ProxyChannel,
        max_buffers: usize,
        buffer_size: usize,
    ) -> anyhow::Result<()> {
        let address = config.address.into();

        let ServerParts {
            event_loop,
            registry,
            sessions,
            pool,
            backends,
            client_scm_socket: _,
            server_scm_socket,
            server_config,
        } = prebuild_server(max_buffers, buffer_size, true)?;

        let token = {
            let mut sessions = sessions.borrow_mut();
            let entry = sessions.slab.vacant_entry();
            let key = entry.key();
            let _ = entry.insert(Rc::new(RefCell::new(ListenSession {
                protocol: Protocol::HTTPSListen,
            })));
            Token(key)
        };

        let mut proxy = HttpsProxy::new(registry, sessions.clone(), pool.clone(), backends.clone());
        proxy
            .add_listener(config, token)
            .with_context(|| "Failed at creating adding the listener")?;
        proxy
            .activate_listener(&address, None)
            .with_context(|| "Failed at creating activating the listener")?;

        let mut server = Server::new(
            event_loop,
            channel,
            server_scm_socket,
            sessions,
            pool,
            backends,
            None,
            Some(proxy),
            None,
            server_config,
            None,
            false,
        )
        .with_context(|| "Failed at creating server")?;

        debug!("{} starting event loop", log_module_context!());
        server.run();
        debug!("{} ending event loop", log_module_context!());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sozu_command::{config::ListenerBuilder, proto::command::SocketAddress};

    use super::*;
    use crate::router::{MethodRule, PathRule, Route, Router, pattern_trie::TrieNode};

    /*
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn size_test() {
      assert_size!(ExpectProxyProtocol<mio::net::TcpStream>, 520);
      assert_size!(TlsHandshake, 240);
      assert_size!(Http<SslStream<mio::net::TcpStream>>, 1232);
      assert_size!(Pipe<SslStream<mio::net::TcpStream>>, 272);
      assert_size!(State, 1240);
      // fails depending on the platform?
      assert_size!(Session, 1672);

      assert_size!(SslStream<mio::net::TcpStream>, 16);
      assert_size!(Ssl, 8);
    }
    */

    #[test]
    fn frontend_from_request_test() {
        let cluster_id1 = "cluster_1".to_owned();
        let cluster_id2 = "cluster_2".to_owned();
        let cluster_id3 = "cluster_3".to_owned();
        let uri1 = "/".to_owned();
        let uri2 = "/yolo".to_owned();
        let uri3 = "/yolo/swag".to_owned();

        let mut fronts = Router::new();
        assert!(fronts.add_tree_rule(
            "lolcatho.st".as_bytes(),
            &PathRule::Prefix(uri1),
            &MethodRule::new(None),
            &Route::ClusterId(cluster_id1.clone())
        ));
        assert!(fronts.add_tree_rule(
            "lolcatho.st".as_bytes(),
            &PathRule::Prefix(uri2),
            &MethodRule::new(None),
            &Route::ClusterId(cluster_id2)
        ));
        assert!(fronts.add_tree_rule(
            "lolcatho.st".as_bytes(),
            &PathRule::Prefix(uri3),
            &MethodRule::new(None),
            &Route::ClusterId(cluster_id3)
        ));
        assert!(fronts.add_tree_rule(
            "other.domain".as_bytes(),
            &PathRule::Prefix("test".to_string()),
            &MethodRule::new(None),
            &Route::ClusterId(cluster_id1)
        ));

        let address = SocketAddress::new_v4(127, 0, 0, 1, 1032);
        let resolver = Arc::new(MutexCertificateResolver::default());

        let crypto_provider = Arc::new(default_provider());

        let server_config = RustlsServerConfig::builder_with_provider(crypto_provider)
            .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
            .expect("could not create rustls config server")
            .with_no_client_auth()
            .with_cert_resolver(resolver.clone());

        let rustls_details = Arc::new(server_config);

        let default_config = ListenerBuilder::new_https(address)
            .to_tls(None)
            .expect("Could not create default HTTPS listener config");

        println!("it doesn't even matter");

        let listener = HttpsListener {
            listener: None,
            address: address.into(),
            fronts,
            rustls_details,
            resolver,
            answers: Rc::new(RefCell::new(
                HttpAnswers::new(&std::collections::BTreeMap::new()).unwrap(),
            )),
            config: default_config,
            token: Token(0),
            active: true,
            tags: BTreeMap::new(),
        };

        println!("TEST {}", line!());
        let frontend1 = listener.frontend_from_request("lolcatho.st", "/", &Method::Get);
        assert_eq!(
            frontend1
                .expect("should find a frontend")
                .cluster_id
                .as_deref(),
            Some("cluster_1")
        );
        println!("TEST {}", line!());
        let frontend2 = listener.frontend_from_request("lolcatho.st", "/test", &Method::Get);
        assert_eq!(
            frontend2
                .expect("should find a frontend")
                .cluster_id
                .as_deref(),
            Some("cluster_1")
        );
        println!("TEST {}", line!());
        let frontend3 = listener.frontend_from_request("lolcatho.st", "/yolo/test", &Method::Get);
        assert_eq!(
            frontend3
                .expect("should find a frontend")
                .cluster_id
                .as_deref(),
            Some("cluster_2")
        );
        println!("TEST {}", line!());
        let frontend4 = listener.frontend_from_request("lolcatho.st", "/yolo/swag", &Method::Get);
        assert_eq!(
            frontend4
                .expect("should find a frontend")
                .cluster_id
                .as_deref(),
            Some("cluster_3")
        );
        println!("TEST {}", line!());
        let frontend5 = listener.frontend_from_request("domain", "/", &Method::Get);
        assert!(frontend5.is_err());
        // assert!(false);
    }

    #[test]
    fn wildcard_certificate_names() {
        let mut trie = TrieNode::root();

        trie.domain_insert("*.services.clever-cloud.com".as_bytes().to_vec(), 1u8);
        trie.domain_insert("*.clever-cloud.com".as_bytes().to_vec(), 2u8);
        trie.domain_insert("services.clever-cloud.com".as_bytes().to_vec(), 0u8);
        trie.domain_insert(
            "abprefix.services.clever-cloud.com".as_bytes().to_vec(),
            3u8,
        );
        trie.domain_insert(
            "cdprefix.services.clever-cloud.com".as_bytes().to_vec(),
            4u8,
        );

        let res = trie.domain_lookup(b"test.services.clever-cloud.com", true);
        println!("query result: {res:?}");

        assert_eq!(
            trie.domain_lookup(b"pgstudio.services.clever-cloud.com", true),
            Some(&("*.services.clever-cloud.com".as_bytes().to_vec(), 1u8))
        );
        assert_eq!(
            trie.domain_lookup(b"test-prefix.services.clever-cloud.com", true),
            Some(&("*.services.clever-cloud.com".as_bytes().to_vec(), 1u8))
        );
    }

    #[test]
    fn wildcard_with_subdomains() {
        let mut trie = TrieNode::root();

        trie.domain_insert("*.test.example.com".as_bytes().to_vec(), 1u8);
        trie.domain_insert("hello.sub.test.example.com".as_bytes().to_vec(), 2u8);

        let res = trie.domain_lookup(b"sub.test.example.com", true);
        println!("query result: {res:?}");

        assert_eq!(
            trie.domain_lookup(b"sub.test.example.com", true),
            Some(&("*.test.example.com".as_bytes().to_vec(), 1u8))
        );
        assert_eq!(
            trie.domain_lookup(b"hello.sub.test.example.com", true),
            Some(&("hello.sub.test.example.com".as_bytes().to_vec(), 2u8))
        );

        // now try in a different order
        let mut trie = TrieNode::root();

        trie.domain_insert("hello.sub.test.example.com".as_bytes().to_vec(), 2u8);
        trie.domain_insert("*.test.example.com".as_bytes().to_vec(), 1u8);

        let res = trie.domain_lookup(b"sub.test.example.com", true);
        println!("query result: {res:?}");

        assert_eq!(
            trie.domain_lookup(b"sub.test.example.com", true),
            Some(&("*.test.example.com".as_bytes().to_vec(), 1u8))
        );
        assert_eq!(
            trie.domain_lookup(b"hello.sub.test.example.com", true),
            Some(&("hello.sub.test.example.com".as_bytes().to_vec(), 2u8))
        );
    }

    #[test]
    fn h2_stream_idle_timeout_inherits_back_timeout() {
        use std::time::Duration;

        let address = SocketAddress::new_v4(127, 0, 0, 1, 1041);
        let build = |back_timeout: u32, explicit: Option<u32>| -> HttpsListener {
            let mut cfg = ListenerBuilder::new_https(address)
                .to_tls(None)
                .expect("default HTTPS listener config");
            cfg.back_timeout = back_timeout;
            cfg.h2_stream_idle_timeout_seconds = explicit;
            HttpsListener::try_new(cfg, Token(0)).expect("build listener")
        };

        // Knob unset: inherit back_timeout when it exceeds the 30s floor.
        assert_eq!(
            build(180, None).get_h2_stream_idle_timeout(),
            Duration::from_secs(180)
        );

        // Knob unset, back_timeout below floor: stay at 30s.
        assert_eq!(
            build(5, None).get_h2_stream_idle_timeout(),
            Duration::from_secs(30)
        );

        // Explicit values win in both directions.
        assert_eq!(
            build(180, Some(10)).get_h2_stream_idle_timeout(),
            Duration::from_secs(10)
        );
        assert_eq!(
            build(5, Some(600)).get_h2_stream_idle_timeout(),
            Duration::from_secs(600)
        );

        // `Some(0)` is clamped to 1s.
        assert_eq!(
            build(180, Some(0)).get_h2_stream_idle_timeout(),
            Duration::from_secs(1)
        );
    }
}
