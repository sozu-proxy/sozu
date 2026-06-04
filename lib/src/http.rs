use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, hash_map::Entry},
    io::ErrorKind,
    net::{Shutdown, SocketAddr},
    os::unix::io::AsRawFd,
    rc::{Rc, Weak},
    str::from_utf8_unchecked,
    time::{Duration, Instant},
};

use mio::{
    Interest, Registry, Token,
    net::{TcpListener as MioTcpListener, TcpStream},
    unix::SourceFd,
};
use rusty_ulid::Ulid;
use sozu_command::{
    logging::CachedTags,
    proto::command::{
        Cluster, HttpListenerConfig, ListenerType, RemoveListener, RequestHttpFrontend,
        UpdateHttpListenerConfig, WorkerRequest, WorkerResponse, request::RequestType,
    },
    ready::Ready,
    response::HttpFrontend,
    state::{ClusterId, validate_h2_flood_knobs_http, validate_sozu_id_header},
};

use crate::metrics::names;
use crate::{
    AcceptError, FrontendFromRequestError, L7ListenerHandler, L7Proxy, ListenerError,
    ListenerHandler, Protocol, ProxyConfiguration, ProxyError, ProxySession, SessionIsToBeClosed,
    SessionMetrics, SessionResult, StateMachineBuilder, StateResult,
    backends::BackendMap,
    pool::Pool,
    protocol::{
        Pipe, SessionState,
        http::{
            answers::HttpAnswers,
            parser::{Method, hostname_and_port},
        },
        mux::{self, Mux, MuxClear},
        proxy_protocol::expect::ExpectProxyProtocol,
    },
    router::{RouteResult, Router},
    server::{ListenToken, SessionManager},
    socket::server_bind,
    timer::TimeoutContainer,
};

#[derive(PartialEq, Eq)]
pub enum SessionStatus {
    Normal,
    DefaultAnswer,
}

StateMachineBuilder! {
    /// The various Stages of an HTTP connection:
    ///
    /// 1. optional (ExpectProxyProtocol)
    /// 2. HTTP (via Mux in H1 mode)
    /// 3. WebSocket (passthrough)
    enum HttpStateMachine impl SessionState {
        Expect(ExpectProxyProtocol<TcpStream>),
        Mux(MuxClear),
        WebSocket(Pipe<crate::socket::SessionTcpStream, HttpListener>),
    }
}

/// Module-level prefix for log lines emitted from this file when no session
/// is in scope. Produces a bold bright-white `HTTP` label in colored mode.
/// Used by [`HttpProxy`] / [`HttpListener`] callbacks (notify, add_cluster,
/// add_*_frontend, accept, soft_stop, hard_stop, etc.) which own a token map
/// keyed by listener and have no `frontend_token` of their own.
macro_rules! log_module_context {
    () => {{
        let (open, reset, _, _, _) = sozu_command::logging::ansi_palette();
        format!("{open}HTTP{reset}\t >>>", open = open, reset = reset)
    }};
}

/// Per-session prefix for log lines emitted with an [`HttpSession`] in
/// scope. Renders the canonical `\tHTTP\tSession(...)\t >>>` envelope from
/// the session's `frontend_token` (mirrors the bracket convention used by
/// `MUX-*`, `RUSTLS`, `PIPE`). Operators can grep-correlate against the
/// token id across log lines for the same H1 connection.
macro_rules! log_context {
    ($self:expr) => {{
        let (open, reset, grey, gray, white) = sozu_command::logging::ansi_palette();
        format!(
            "{open}HTTP{reset}\t{grey}Session{reset}({gray}frontend{reset}={white}{frontend}{reset})\t >>>",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            frontend = $self.frontend_token.0,
        )
    }};
}

/// HTTP Session to insert in the SessionManager
///
/// 1 session <=> 1 HTTP connection (client to sozu)
pub struct HttpSession {
    configured_backend_timeout: Duration,
    configured_connect_timeout: Duration,
    configured_frontend_timeout: Duration,
    frontend_token: Token,
    last_event: Instant,
    listener: Rc<RefCell<HttpListener>>,
    metrics: SessionMetrics,
    pool: Weak<RefCell<Pool>>,
    proxy: Rc<RefCell<HttpProxy>>,
    state: HttpStateMachine,
    has_been_closed: bool,
}

impl HttpSession {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        configured_backend_timeout: Duration,
        configured_connect_timeout: Duration,
        configured_frontend_timeout: Duration,
        configured_request_timeout: Duration,
        expect_proxy: bool,
        listener: Rc<RefCell<HttpListener>>,
        pool: Weak<RefCell<Pool>>,
        proxy: Rc<RefCell<HttpProxy>>,
        public_address: SocketAddr,
        sock: TcpStream,
        token: Token,
        wait_time: Duration,
    ) -> Result<Self, AcceptError> {
        let request_id = Ulid::generate();
        let container_frontend_timeout = TimeoutContainer::new(configured_request_timeout, token);

        let state = if expect_proxy {
            trace!("{} starting in expect proxy state", log_module_context!());
            gauge_add!(names::protocol::PROXY_EXPECT, 1);

            HttpStateMachine::Expect(ExpectProxyProtocol::new(
                container_frontend_timeout,
                sock,
                token,
                request_id,
            ))
        } else {
            gauge_add!(names::protocol::HTTP, 1);
            let session_address = sock.peer_addr().ok();
            let session_ulid = rusty_ulid::Ulid::generate();
            let sock = crate::socket::SessionTcpStream::new(sock, session_ulid, session_address);

            let frontend =
                mux::Connection::new_h1_server(session_ulid, sock, container_frontend_timeout);
            let router = mux::Router::new(configured_backend_timeout, configured_connect_timeout);
            let mut context = mux::Context::new(
                session_ulid,
                pool.clone(),
                listener.clone(),
                session_address,
                public_address,
            );
            context
                .create_stream(request_id, 1 << 16)
                .ok_or(AcceptError::BufferCapacityReached)?;
            HttpStateMachine::Mux(Mux {
                configured_frontend_timeout,
                frontend_token: token,
                frontend,
                router,
                context,
                session_ulid,
            })
        };

        // Invariant: `create_session` allocates `token` from the session slab
        // and registers the frontend socket under it before constructing us.
        // A session whose frontend token aliased the listen-socket token (0)
        // would route listener readiness into a session — a state-machine bug.
        // (`Token(0)` is reserved in real workers; the test harness in this
        // file builds listeners directly without going through `new`.)
        debug_assert_eq!(
            state.marker() as u8,
            if expect_proxy {
                StateMarker::Expect as u8
            } else {
                StateMarker::Mux as u8
            },
            "constructed state must match the expect_proxy branch"
        );
        // The freshly-built state can never be a FailedUpgrade: that marker
        // only appears after a real upgrade attempt drains the state via
        // `take()`. Catch a future refactor that constructs one by accident.
        debug_assert!(
            !state.failed(),
            "a newly created session must not start in FailedUpgrade"
        );

        let metrics = SessionMetrics::new(Some(wait_time));
        let session = HttpSession {
            configured_backend_timeout,
            configured_connect_timeout,
            configured_frontend_timeout,
            frontend_token: token,
            has_been_closed: false,
            last_event: Instant::now(),
            listener,
            metrics,
            pool,
            proxy,
            state,
        };
        debug_assert_eq!(
            session.frontend_token, token,
            "frontend token must be the slab token used for registration"
        );
        #[cfg(debug_assertions)]
        session.check_invariants();
        Ok(session)
    }

    /// Full cross-field invariant sweep for the session state machine, used as
    /// a run-to-completion postcondition. Compiled out in release.
    ///
    /// Strong invariants asserted here:
    /// - the frontend token is always present (slab key is never sentinel);
    /// - once `close()` has run the state has been drained / cancelled, so the
    ///   session is terminal and must not be re-driven (callers gate on the
    ///   returned `SessionIsToBeClosed`);
    /// - the live state marker is one of the three legal H1 stages — the
    ///   `FailedUpgrade` marker is only transient (between `take()` and the
    ///   `close()` that immediately follows a failed upgrade), so it is the
    ///   single tolerated exception on a still-open session.
    #[cfg(debug_assertions)]
    fn check_invariants(&self) {
        let marker = self.state.marker();
        debug_assert!(
            matches!(
                marker,
                StateMarker::Expect | StateMarker::Mux | StateMarker::WebSocket
            ),
            "session marker must be a legal H1 stage (Expect/Mux/WebSocket), got {marker:?}"
        );
        // A failed state is only ever observed transiently between a failed
        // upgrade and the close() that reaps it; if it survives to a postcondition
        // sweep on a not-yet-closed session, a close path was skipped.
        debug_assert!(
            !self.state.failed() || self.has_been_closed,
            "FailedUpgrade state must be reaped by close(), never left live"
        );
    }

    pub fn upgrade(&mut self) -> SessionIsToBeClosed {
        debug!("{} upgrade", log_context!(self));
        // Record the stage we are leaving so we can assert the transition is
        // legal (no stage-skipping) after the handoff resolves. `take()`
        // installs a `FailedUpgrade(marker)` placeholder carrying this same
        // marker, so the session is never left in a half-moved state if an
        // upgrade_* helper bails out.
        let from_marker = self.state.marker();
        let new_state = match self.state.take() {
            HttpStateMachine::Mux(mux) => self.upgrade_mux(mux),
            HttpStateMachine::Expect(expect) => self.upgrade_expect(expect),
            HttpStateMachine::WebSocket(ws) => self.upgrade_websocket(ws),
            HttpStateMachine::FailedUpgrade(_) => {
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
                // Legal transitions only: Expect→Mux, Mux→WebSocket, or the
                // WebSocket self-loop (upgrade_websocket is a no-op guard). Any
                // other pair means a stage was skipped or reversed.
                debug_assert!(
                    matches!(
                        (from_marker, state.marker()),
                        (StateMarker::Expect, StateMarker::Mux)
                            | (StateMarker::Mux, StateMarker::WebSocket)
                            | (StateMarker::WebSocket, StateMarker::WebSocket)
                    ),
                    "illegal protocol-upgrade transition {from_marker:?} -> {:?}",
                    state.marker()
                );
                debug_assert!(
                    !state.failed(),
                    "a successful upgrade must not install a FailedUpgrade state"
                );
                self.state = state;
                #[cfg(debug_assertions)]
                self.check_invariants();
                false
            }
            // The state stays FailedUpgrade, but the Session should be closed right after
            None => {
                // On failure `take()` left a FailedUpgrade placeholder behind;
                // the caller (`ready`) must close the session on the returned
                // `true`, so we do not run the still-open invariant sweep here.
                debug_assert!(
                    self.state.failed(),
                    "a failed upgrade must leave the session in FailedUpgrade"
                );
                true
            }
        }
    }

    fn upgrade_expect(
        &mut self,
        expect: ExpectProxyProtocol<TcpStream>,
    ) -> Option<HttpStateMachine> {
        debug!("{} switching to HTTP", log_context!(self));
        match expect
            .addresses
            .as_ref()
            .map(|add| (add.destination(), add.source()))
        {
            Some((Some(public_address), Some(session_address))) => {
                let session_ulid = rusty_ulid::Ulid::generate();
                let frontend = mux::Connection::new_h1_server(
                    session_ulid,
                    crate::socket::SessionTcpStream::new(
                        expect.frontend,
                        session_ulid,
                        Some(session_address),
                    ),
                    expect.container_frontend_timeout,
                );
                let router = mux::Router::new(
                    self.configured_backend_timeout,
                    self.configured_connect_timeout,
                );
                let mut context = mux::Context::new(
                    session_ulid,
                    self.pool.clone(),
                    self.listener.clone(),
                    Some(session_address),
                    public_address,
                );
                if context.create_stream(expect.request_id, 1 << 16).is_none() {
                    error!(
                        "{} expect upgrade failed: could not create stream",
                        log_context!(self)
                    );
                    return None;
                }
                let mut mux = Mux {
                    configured_frontend_timeout: self.configured_frontend_timeout,
                    frontend_token: self.frontend_token,
                    frontend,
                    router,
                    context,
                    session_ulid,
                };
                mux.frontend.readiness_mut().event = expect.frontend_readiness.event;

                // The Expect→Mux handoff must carry the session's frontend
                // token across unchanged: the slab slot and epoll registration
                // are keyed by it, so a mismatch would misroute readiness.
                debug_assert_eq!(
                    mux.frontend_token, self.frontend_token,
                    "expect upgrade must preserve the frontend token"
                );
                // Fresh Mux: exactly one stream was just created above, and no
                // backend is connected yet (back token is set iff linked).
                debug_assert_eq!(
                    mux.context.streams.len(),
                    1,
                    "a freshly upgraded Mux owns exactly the request stream"
                );

                // Gauge pairing: this connection leaves PROXY_EXPECT and enters
                // HTTP. The two adjustments must stay together so the live-state
                // gauges sum to the session count (no double-count, no leak).
                gauge_add!(names::protocol::PROXY_EXPECT, -1);
                gauge_add!(names::protocol::HTTP, 1);
                Some(HttpStateMachine::Mux(mux))
            }
            _ => {
                debug!(
                    "{} expect upgrade failed: bad header {:?}",
                    log_context!(self),
                    expect.addresses
                );
                None
            }
        }
    }

    fn upgrade_mux(&mut self, mut mux: MuxClear) -> Option<HttpStateMachine> {
        debug!("{} mux switching to ws", log_context!(self));
        let Some(stream) = mux.context.streams.pop() else {
            error!(
                "{} upgrade_mux: no stream attached to the mux session, closing",
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
        // Invariant (back token set iff backend connected): a Linked stream
        // names a backend token that must exist in the router's backend map.
        // We assert the map is non-empty here, then prove membership by the
        // `remove` below — a Linked token absent from the map would be a
        // book-keeping desync between stream state and the backend map.
        debug_assert!(
            mux.router.backends.contains_key(&back_token),
            "a Linked stream's back token must index a connected backend"
        );
        let backends_before = mux.router.backends.len();
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

        // Post-removal book-keeping: the backend is gone from the map and the
        // count dropped by exactly one (the `remove` matched a present key).
        debug_assert!(
            !mux.router.backends.contains_key(&back_token),
            "the upgraded backend must be evicted from the router map"
        );
        debug_assert_eq!(
            mux.router.backends.len(),
            backends_before - 1,
            "removing the backend must drop the backend count by exactly one"
        );

        let ws_context = stream.context.websocket_context();

        container_frontend_timeout.reset();
        container_backend_timeout.reset();

        let backend_id = backend.borrow().backend_id.clone();
        // `Pipe::backend_socket` is typed `Option<TcpStream>` (raw, pre-mux).
        // The mux wraps every backend TCP socket in `SessionTcpStream` so
        // SOCKET-layer errors carry the session ULID; unwrap back to the
        // plain `TcpStream` here to feed Pipe's legacy shape.
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
            Protocol::HTTP,
            stream.context.session_id,
            stream.context.id,
            stream.context.session_address,
            ws_context,
        );

        pipe.frontend_readiness.event = frontend_readiness.event;
        pipe.backend_readiness.event = backend_readiness.event;
        // The WebSocket pipe inherits the live backend connection, so its back
        // token must be set (back token present iff a backend is connected).
        pipe.set_back_token(back_token);
        debug_assert_eq!(
            pipe.back_token(),
            vec![back_token],
            "websocket pipe must carry exactly the upgraded backend token"
        );

        // http.active_requests was already decremented by generate_access_log()
        // in h1.rs when the 101 response was written (before MuxResult::Upgrade).
        //
        // Gauge pairing (Mux→WebSocket): leave HTTP, enter WS, and start
        // accounting one active websocket request. These three must move
        // together so the protocol gauges stay consistent with the session set.
        gauge_add!(names::protocol::HTTP, -1);
        gauge_add!(names::protocol::WS, 1);
        gauge_add!(names::websocket::ACTIVE_REQUESTS, 1);
        Some(HttpStateMachine::WebSocket(pipe))
    }

    fn upgrade_websocket(
        &self,
        ws: Pipe<crate::socket::SessionTcpStream, HttpListener>,
    ) -> Option<HttpStateMachine> {
        // what do we do here?
        error!(
            "{} upgrade called on WS, this should not happen",
            log_context!(self)
        );
        Some(HttpStateMachine::WebSocket(ws))
    }
}

impl ProxySession for HttpSession {
    fn close(&mut self) {
        if self.has_been_closed {
            return;
        }
        // Reaching here means we are about to do the one-and-only teardown.
        debug_assert!(
            !self.has_been_closed,
            "close past the guard must run on a not-yet-closed session"
        );

        trace!("{} closing HTTP session", log_context!(self));
        self.metrics.service_stop();

        // Restore gauges
        match self.state.marker() {
            StateMarker::Expect => gauge_add!(names::protocol::PROXY_EXPECT, -1),
            StateMarker::Mux => gauge_add!(names::protocol::HTTP, -1),
            StateMarker::WebSocket => {
                gauge_add!(names::protocol::WS, -1);
                gauge_add!(names::websocket::ACTIVE_REQUESTS, -1);
            }
        }

        if self.state.failed() {
            match self.state.marker() {
                StateMarker::Expect => incr!(names::http::UPGRADE_EXPECT_FAILED),
                StateMarker::Mux => incr!(names::http::UPGRADE_MUX_FAILED),
                StateMarker::WebSocket => incr!(names::http::UPGRADE_WS_FAILED),
            }
            // FailedUpgrade means the socket was consumed by a failed upgrade
            // attempt, so we can only close the state (no-op) and remove the
            // session — cancel_timeouts / front_socket are unreachable.
            self.state.close(self.proxy.clone(), &mut self.metrics);
            self.proxy.borrow().remove_session(self.frontend_token);
            self.has_been_closed = true;
            debug_assert!(
                self.has_been_closed,
                "failed-upgrade close path must mark the session closed"
            );
            return;
        }

        self.state.cancel_timeouts();
        // defer backend closing to the state
        self.state.close(self.proxy.clone(), &mut self.metrics);

        let front_socket = self.state.front_socket();
        // invariant: write-only shutdown — Shutdown::Both on a TLS frontend
        // discards the receive buffer and elicits TCP RST, truncating the
        // already-queued response. Canonical write-up: `lib/src/https.rs:650-655`.
        // Backend sockets follow the same discipline for symmetry.
        if let Err(e) = front_socket.shutdown(Shutdown::Write) {
            // error 107 NotConnected can happen when was never fully connected, or was already disconnected due to error
            if e.kind() != ErrorKind::NotConnected {
                error!(
                    "{} error shutting down front socket({:?}): {:?}",
                    log_context!(self),
                    front_socket,
                    e
                )
            }
        }

        // deregister the frontend and remove it
        let proxy = self.proxy.borrow();
        let fd = front_socket.as_raw_fd();
        if let Err(e) = proxy.registry.deregister(&mut SourceFd(&fd)) {
            error!(
                "{} error deregistering front socket({:?}) while closing HTTP session: {:?}",
                log_context!(self),
                fd,
                e
            );
        }
        proxy.remove_session(self.frontend_token);

        self.has_been_closed = true;
        debug_assert!(
            self.has_been_closed,
            "close must leave the session marked closed (idempotency latch)"
        );
    }

    fn timeout(&mut self, token: Token) -> SessionIsToBeClosed {
        let state_result = self.state.timeout(token, &mut self.metrics);
        state_result == StateResult::CloseSession
    }

    fn protocol(&self) -> Protocol {
        Protocol::HTTP
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

        self.metrics.service_stop();
        // Run-to-completion postcondition: a session that is being kept alive
        // must still satisfy its cross-field invariants. When `to_be_closed`
        // is true the state may be a transient FailedUpgrade or already torn
        // down, so the sweep only applies to the still-open path.
        #[cfg(debug_assertions)]
        if !to_be_closed {
            self.check_invariants();
        }
        to_be_closed
    }

    fn shutting_down(&mut self) -> SessionIsToBeClosed {
        self.state.shutting_down()
    }

    fn last_event(&self) -> Instant {
        self.last_event
    }

    fn print_session(&self) {
        self.state.print_state("HTTP");
        error!("{} Metrics: {:?}", log_context!(self), self.metrics);
    }

    fn frontend_token(&self) -> Token {
        self.frontend_token
    }
}

pub type Hostname = String;

/// Cleartext HTTP/1.x listener.
///
/// # HTTP/2 over cleartext (h2c) is NOT supported
///
/// RFC 7540 §3.2 specified an `Upgrade: h2c` mechanism to negotiate HTTP/2
/// over a cleartext TCP connection, with a companion prior-knowledge
/// variant in §3.4. Both paths are intentionally absent from this listener:
///
/// - No `Upgrade: h2c` handler: the HTTP/1.1 state machine forwards
///   `Upgrade` headers to the backend but never responds `101 Switching
///   Protocols` with an HTTP/2 connection preface.
/// - No prior-knowledge detection: the listener does not sniff the
///   24-byte `PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n` magic string; a client
///   that opens a TCP connection and immediately sends the preface will
///   be interpreted as a malformed HTTP/1 request and rejected with 400.
///
/// RFC 9113 (the current HTTP/2 RFC, obsoleting 7540) formally deprecates
/// the `Upgrade: h2c` mechanism. Clients that want HTTP/2 MUST use the
/// TLS ALPN path (`HttpsListener`, selector `h2`) instead. This is
/// consistent with the industry consensus (nginx, envoy, cloudflare) and
/// removes an entire class of cleartext-preface smuggling primitives.
pub struct HttpListener {
    active: bool,
    address: SocketAddr,
    answers: Rc<RefCell<HttpAnswers>>,
    config: HttpListenerConfig,
    fronts: Router,
    listener: Option<MioTcpListener>,
    tags: BTreeMap<String, CachedTags>,
    token: Token,
}

impl ListenerHandler for HttpListener {
    fn get_addr(&self) -> &SocketAddr {
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
        Protocol::HTTP
    }

    fn public_address(&self) -> SocketAddr {
        self.config
            .public_address
            .map(|addr| addr.into())
            .unwrap_or(self.address)
    }
}

impl L7ListenerHandler for HttpListener {
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

    // redundant, already called once in extract_route
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

        /*if port == Some(&b"80"[..]) {
        // it is alright to call from_utf8_unchecked,
        // we already verified that there are only ascii
        // chars in there
          unsafe { from_utf8_unchecked(hostname) }
        } else {
          host
        }
        */
        // SAFETY: `hostname` was just produced by `hostname_and_port` (see
        // `lib/src/protocol/kawa_h1/parser.rs:133`), which only accepts
        // bytes matching `is_hostname_char` (alphanumeric, `-`, `.`, plus
        // `_` under the tolerant-http1-parser feature). All accepted
        // bytes are ASCII (≤ 0x7F), so the slice is valid single-byte UTF-8.
        let host = unsafe { from_utf8_unchecked(hostname) };

        let route = self.fronts.lookup(host, uri, method).map_err(|e| {
            incr!(names::http::FAILED_BACKEND_MATCHING);
            FrontendFromRequestError::NoClusterFound(e)
        })?;

        let now = Instant::now();

        if let Some(cluster) = route.cluster_id.as_deref() {
            time!(
                names::event_loop::FRONTEND_MATCHING_TIME,
                cluster,
                (now - start).as_millis()
            );
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
            max_header_fields: self
                .config
                .h2_max_header_fields
                .unwrap_or(defaults.max_header_fields),
        }
    }

    fn get_h2_connection_config(&self) -> crate::protocol::mux::H2ConnectionConfig {
        crate::protocol::mux::H2ConnectionConfig::from_optional(
            self.config.h2_initial_connection_window,
            self.config.h2_max_concurrent_streams,
            self.config.h2_stream_shrink_ratio,
        )
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

    fn get_elide_x_real_ip(&self) -> bool {
        self.config.elide_x_real_ip.unwrap_or(false)
    }

    fn get_send_x_real_ip(&self) -> bool {
        self.config.send_x_real_ip.unwrap_or(false)
    }
}

pub struct HttpProxy {
    backends: Rc<RefCell<BackendMap>>,
    clusters: HashMap<ClusterId, Cluster>,
    listeners: HashMap<Token, Rc<RefCell<HttpListener>>>,
    pool: Rc<RefCell<Pool>>,
    registry: Registry,
    sessions: Rc<RefCell<SessionManager>>,
}

impl HttpProxy {
    pub fn new(
        registry: Registry,
        sessions: Rc<RefCell<SessionManager>>,
        pool: Rc<RefCell<Pool>>,
        backends: Rc<RefCell<BackendMap>>,
    ) -> HttpProxy {
        HttpProxy {
            backends,
            clusters: HashMap::new(),
            listeners: HashMap::new(),
            pool,
            registry,
            sessions,
        }
    }

    pub fn add_listener(
        &mut self,
        config: HttpListenerConfig,
        token: Token,
    ) -> Result<Token, ProxyError> {
        match self.listeners.entry(token) {
            Entry::Vacant(entry) => {
                let http_listener =
                    HttpListener::new(config, token).map_err(ProxyError::AddListener)?;
                entry.insert(Rc::new(RefCell::new(http_listener)));
                Ok(token)
            }
            _ => Err(ProxyError::ListenerAlreadyPresent),
        }
    }

    pub fn get_listener(&self, token: &Token) -> Option<Rc<RefCell<HttpListener>>> {
        self.listeners.get(token).cloned()
    }

    pub fn remove_listener(&mut self, remove: RemoveListener) -> Result<(), ProxyError> {
        let len = self.listeners.len();
        let remove_address = remove.address.into();
        self.listeners
            .retain(|_, l| l.borrow().address != remove_address);

        if !self.listeners.len() < len {
            info!(
                "{} no HTTP listener to remove at address {:?}",
                log_module_context!(),
                remove_address
            );
        }
        Ok(())
    }

    pub fn activate_listener(
        &self,
        addr: &SocketAddr,
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

    pub fn give_back_listeners(&mut self) -> Vec<(SocketAddr, MioTcpListener)> {
        self.listeners
            .iter()
            .filter_map(|(_, listener)| {
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
        address: SocketAddr,
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

    /// Apply a partial-update patch to the identified HTTP listener.
    pub fn update_listener(&mut self, patch: UpdateHttpListenerConfig) -> Result<(), ProxyError> {
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

    pub fn add_cluster(&mut self, mut cluster: Cluster) -> Result<(), ProxyError> {
        // Reconcile the legacy single-status `answer_503` field with the
        // new map. The new map wins on collision.
        let mut overrides = cluster.answers.clone();
        if let Some(answer_503) = cluster.answer_503.take() {
            overrides.entry("503".to_owned()).or_insert(answer_503);
        }
        if !overrides.is_empty() {
            for listener in self.listeners.values() {
                listener
                    .borrow()
                    .answers
                    .borrow_mut()
                    .add_cluster_answers(&cluster.cluster_id, &overrides)
                    .map_err(|(name, error)| {
                        ProxyError::AddCluster(ListenerError::TemplateParse(name, error))
                    })?;
            }
        }
        self.clusters.insert(cluster.cluster_id.clone(), cluster);
        Ok(())
    }

    pub fn remove_cluster(&mut self, cluster_id: &str) -> Result<(), ProxyError> {
        self.clusters.remove(cluster_id);

        for listener in self.listeners.values() {
            listener
                .borrow()
                .answers
                .borrow_mut()
                .remove_cluster_answers(cluster_id);
        }
        Ok(())
    }

    pub fn add_http_frontend(&mut self, front: RequestHttpFrontend) -> Result<(), ProxyError> {
        // RFC 6797 §7.2: `Strict-Transport-Security` MUST NOT be sent over
        // plaintext HTTP. Reject any AddHttpFrontend that ships an enabled
        // HSTS policy before it ever touches the routing trie. This is
        // defense in depth on top of the TOML config-load reject in
        // `command/src/config.rs`; sites that build a `RequestHttpFrontend`
        // outside the TOML path (`sozu frontend http add`, programmatic
        // IPC senders) are caught here.
        // Reject ANY hsts field on a plain-HTTP frontend, not just
        // `enabled = true`. There is no listener-default HSTS to inherit
        // on an HTTP listener (the field doesn't exist on
        // `HttpListenerConfig`), so the explicit-disable signal
        // (`enabled = false`) has nothing to suppress on this surface.
        // Carrying any `hsts` field here is a misconfiguration rather
        // than a deliberate choice.
        if front.hsts.is_some() {
            incr!(names::http::HSTS_SUPPRESSED_PLAINTEXT);
            return Err(ProxyError::HstsOnPlainHttp(front.address.into()));
        }

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
        let tags = front.tags.to_owned();

        listener
            .add_http_front(front)
            .map_err(ProxyError::AddFrontend)?;
        listener.set_tags(hostname, tags);
        Ok(())
    }

    pub fn remove_http_frontend(&mut self, front: RequestHttpFrontend) -> Result<(), ProxyError> {
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
            .remove_http_front(front)
            .map_err(ProxyError::RemoveFrontend)?;

        if !listener.fronts.has_hostname(&hostname) {
            listener.set_tags(hostname, None);
        }
        Ok(())
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
                proxy_protocol: "HTTP".to_string(),
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
                proxy_protocol: "HTTP".to_string(),
                error: format!("Error deregistering listen sockets: {socket_errors:?}"),
            });
        }

        Ok(())
    }
}

impl HttpListener {
    pub fn new(config: HttpListenerConfig, token: Token) -> Result<HttpListener, ListenerError> {
        Ok(HttpListener {
            active: false,
            address: config.address.into(),
            answers: Rc::new(RefCell::new({
                // Reconcile the legacy `http_answers` per-status fields
                // with the new template map: the new map wins on
                // collision, the legacy fields fill in any status the
                // operator hasn't yet migrated.
                let mut answers_map = config.answers.clone();
                if let Some(ref legacy) = config.http_answers {
                    crate::protocol::http::answers::merge_legacy_into_map(&mut answers_map, legacy);
                }
                HttpAnswers::new(&answers_map)
                    .map_err(|(name, error)| ListenerError::TemplateParse(name, error))?
            })),
            config,
            fronts: Router::new(),
            listener: None,
            tags: BTreeMap::new(),
            token,
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
        let address: SocketAddr = self.config.address.into();

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

    /// Apply a partial-update patch to this listener's live configuration.
    ///
    /// Fields absent in the patch (i.e. `None`) are preserved unchanged.
    /// If `http_answers` is present only the listener-default templates are
    /// replaced; per-cluster overrides in `cluster_custom_answers` are kept.
    pub fn update_config(&mut self, patch: &UpdateHttpListenerConfig) -> Result<(), ListenerError> {
        // Defense-in-depth validation: main-process ConfigState::dispatch
        // validates before scatter, but a raw protobuf client or state replay
        // may reach the worker without that check. `StateError` lifts into
        // `ListenerError` via `From` so `?` suffices.
        validate_h2_flood_knobs_http(patch)?;
        if let Some(ref hdr) = patch.sozu_id_header {
            validate_sozu_id_header(hdr)?;
        }

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
        if let Some(ref v) = patch.sozu_id_header {
            self.config.sozu_id_header = Some(v.to_owned());
        }
        if let Some(v) = patch.elide_x_real_ip {
            self.config.elide_x_real_ip = Some(v);
        }
        if let Some(v) = patch.send_x_real_ip {
            self.config.send_x_real_ip = Some(v);
        }

        // H2 flood knobs
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
        if let Some(v) = patch.h2_max_header_fields {
            self.config.h2_max_header_fields = Some(v);
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
            // Rebuild the listener-level templates and migrate the existing
            // per-cluster overrides over to the new `HttpAnswers`.
            let mut new_answers = HttpAnswers::new(&answers_map)
                .map_err(|(name, error)| ListenerError::TemplateParse(name, error))?;
            let preserved = std::mem::take(&mut self.answers.borrow_mut().cluster_answers);
            new_answers.cluster_answers = preserved;
            *self.answers.borrow_mut() = new_answers;
        }

        Ok(())
    }

    pub fn add_http_front(&mut self, http_front: HttpFrontend) -> Result<(), ListenerError> {
        self.fronts
            .add_http_front(&http_front)
            .map_err(ListenerError::AddFrontend)
    }

    pub fn remove_http_front(&mut self, http_front: HttpFrontend) -> Result<(), ListenerError> {
        debug!(
            "{} removing http_front {:?}",
            log_module_context!(),
            http_front
        );
        self.fronts
            .remove_http_front(&http_front)
            .map_err(ListenerError::RemoveFrontend)
    }

    fn accept(&mut self) -> Result<TcpStream, AcceptError> {
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

impl ProxyConfiguration for HttpProxy {
    fn notify(&mut self, request: WorkerRequest) -> WorkerResponse {
        let request_id = request.id.clone();

        let result = match request.content.request_type {
            Some(RequestType::AddCluster(cluster)) => {
                debug!(
                    "{} {} add cluster {:?}",
                    log_module_context!(),
                    request.id,
                    cluster
                );
                self.add_cluster(cluster)
            }
            Some(RequestType::RemoveCluster(cluster_id)) => {
                debug!(
                    "{} {} remove cluster {:?}",
                    log_module_context!(),
                    request_id,
                    cluster_id
                );
                self.remove_cluster(&cluster_id)
            }
            Some(RequestType::AddHttpFrontend(front)) => {
                debug!(
                    "{} {} add front {:?}",
                    log_module_context!(),
                    request_id,
                    front
                );
                self.add_http_frontend(front)
            }
            Some(RequestType::RemoveHttpFrontend(front)) => {
                debug!(
                    "{} {} remove front {:?}",
                    log_module_context!(),
                    request_id,
                    front
                );
                self.remove_http_frontend(front)
            }
            Some(RequestType::RemoveListener(remove)) => {
                debug!(
                    "{} removing HTTP listener at address {:?}",
                    log_module_context!(),
                    remove.address
                );
                self.remove_listener(remove)
            }
            Some(RequestType::SoftStop(_)) => {
                debug!(
                    "{} {} processing soft shutdown",
                    log_module_context!(),
                    request_id
                );
                match self.soft_stop() {
                    Ok(()) => {
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
            Some(RequestType::HardStop(_)) => {
                debug!(
                    "{} {} processing hard shutdown",
                    log_module_context!(),
                    request_id
                );
                match self.hard_stop() {
                    Ok(()) => {
                        info!(
                            "{} {} hard stop successful",
                            log_module_context!(),
                            request_id
                        );
                        return WorkerResponse::processing(request.id);
                    }
                    Err(e) => Err(e),
                }
            }
            Some(RequestType::Status(_)) => {
                debug!("{} {} status", log_module_context!(), request_id);
                Ok(())
            }
            other_command => {
                debug!(
                    "{} {} unsupported message for HTTP proxy, ignoring: {:?}",
                    log_module_context!(),
                    request.id,
                    other_command
                );
                Err(ProxyError::UnsupportedMessage)
            }
        };

        match result {
            Ok(()) => {
                debug!("{} {} successful", log_module_context!(), request_id);
                WorkerResponse::ok(request_id)
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

    fn accept(&mut self, token: ListenToken) -> Result<TcpStream, AcceptError> {
        if let Some(listener) = self.listeners.get(&Token(token.0)) {
            listener.borrow_mut().accept()
        } else {
            Err(AcceptError::IoError)
        }
    }

    fn create_session(
        &mut self,
        mut frontend_sock: TcpStream,
        listener_token: ListenToken,
        wait_time: Duration,
        proxy: Rc<RefCell<Self>>,
    ) -> Result<(), AcceptError> {
        let listener = self
            .listeners
            .get(&Token(listener_token.0))
            .cloned()
            .ok_or(AcceptError::IoError)?;

        if let Err(e) = frontend_sock.set_nodelay(true) {
            error!(
                "{} error setting nodelay on front socket({:?}): {:?}",
                log_module_context!(),
                frontend_sock,
                e
            );
        }
        let mut session_manager = self.sessions.borrow_mut();
        let slab_len_before = session_manager.slab.len();
        let session_entry = session_manager.slab.vacant_entry();
        let session_token = Token(session_entry.key());
        // The token handed to the new session, used for epoll registration and
        // every `remove_session(frontend_token)` call, MUST be the very slab
        // key this entry will occupy. A drift here would deregister/free the
        // wrong slot on close.
        debug_assert_eq!(
            session_token.0,
            session_entry.key(),
            "session token must equal the slab vacant-entry key"
        );
        let owned = listener.borrow();

        if let Err(register_error) = self.registry.register(
            &mut frontend_sock,
            session_token,
            Interest::READABLE | Interest::WRITABLE,
        ) {
            error!(
                "{} error registering listen socket({:?}): {:?}",
                log_module_context!(),
                frontend_sock,
                register_error
            );
            return Err(AcceptError::RegisterError);
        }

        let public_address: SocketAddr = match owned.config.public_address {
            Some(pub_addr) => pub_addr.into(),
            None => owned.config.address.into(),
        };

        let session = HttpSession::new(
            Duration::from_secs(owned.config.back_timeout as u64),
            Duration::from_secs(owned.config.connect_timeout as u64),
            Duration::from_secs(owned.config.front_timeout as u64),
            Duration::from_secs(owned.config.request_timeout as u64),
            owned.config.expect_proxy,
            listener.clone(),
            Rc::downgrade(&self.pool),
            proxy,
            public_address,
            frontend_sock,
            session_token,
            wait_time,
        )?;

        // The session's frontend token must be exactly the slab key we are
        // about to fill — the registration above and all later token lookups
        // depend on it.
        debug_assert_eq!(
            session.frontend_token, session_token,
            "session must own the frontend token it was created with"
        );

        let session = Rc::new(RefCell::new(session));
        session_entry.insert(session);
        // Inserting into the previously-vacant entry grows the live session
        // count by exactly one (gauge-like ±1 pairing against close()'s
        // try_remove). `len()` is the count of occupied slots.
        debug_assert_eq!(
            session_manager.slab.len(),
            slab_len_before + 1,
            "creating a session must occupy exactly one new slab slot"
        );

        Ok(())
    }
}

impl L7Proxy for HttpProxy {
    fn kind(&self) -> ListenerType {
        ListenerType::Http
    }

    fn register_socket(
        &self,
        source: &mut TcpStream,
        token: Token,
        interest: Interest,
    ) -> Result<(), std::io::Error> {
        self.registry.register(source, token, interest)
    }

    fn deregister_socket(&self, tcp_stream: &mut TcpStream) -> Result<(), std::io::Error> {
        self.registry.deregister(tcp_stream)
    }

    fn add_session(&self, session: Rc<RefCell<dyn ProxySession>>) -> Token {
        let mut session_manager = self.sessions.borrow_mut();
        let len_before = session_manager.slab.len();
        let entry = session_manager.slab.vacant_entry();
        let token = Token(entry.key());
        let _entry = entry.insert(session);
        // The returned token is the slab key callers use to later remove this
        // session, and the insert occupied exactly one new slot.
        debug_assert_eq!(
            session_manager.slab.len(),
            len_before + 1,
            "add_session must occupy exactly one new slab slot"
        );
        debug_assert!(
            session_manager.slab.contains(token.0),
            "the returned token must index the freshly inserted session"
        );
        token
    }

    fn remove_session(&self, token: Token) -> bool {
        let mut sessions = self.sessions.borrow_mut();
        let was_present = sessions.slab.contains(token.0);
        let len_before = sessions.slab.len();
        // Drain the session's `(cluster, ip)` accounting before the slab
        // slot is freed — once the slot is reused for a new session the
        // token would otherwise alias an unrelated set of entries. No-op
        // when the session never tracked anything (feature disabled, or
        // no request reached `Router::connect`).
        sessions.untrack_all_cluster_ip(token);
        let removed = sessions.slab.try_remove(token.0).is_some();
        // Removal must report exactly whether the slot was occupied, and the
        // live count drops by one iff something was actually removed (±1
        // pairing against add_session / create_session). The slot is gone.
        debug_assert_eq!(
            removed, was_present,
            "try_remove reports presence iff the slot was occupied"
        );
        debug_assert_eq!(
            sessions.slab.len(),
            len_before - removed as usize,
            "slab len drops by exactly one iff a session was removed"
        );
        debug_assert!(
            !sessions.slab.contains(token.0),
            "the slot must be free after remove_session"
        );
        removed
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

pub mod testing {
    use crate::testing::*;

    /// this function is not used, but is available for example and testing purposes
    pub fn start_http_worker(
        config: HttpListenerConfig,
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
                protocol: Protocol::HTTPListen,
            })));
            Token(key)
        };

        let mut proxy = HttpProxy::new(registry, sessions.clone(), pool.clone(), backends.clone());
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
            Some(proxy),
            None,
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
    extern crate tiny_http;

    use std::{
        io::{Read, Write},
        net::TcpStream,
        str,
        sync::{Arc, Barrier},
        thread,
        time::Duration,
    };

    use sozu_command::proto::command::SocketAddress;

    use super::{testing::start_http_worker, *};
    use crate::sozu_command::{
        channel::Channel,
        config::ListenerBuilder,
        proto::command::{
            LoadBalancingParams, PathRule, RulePosition, SoftStop, WorkerRequest,
            request::RequestType,
        },
        response::{Backend, HttpFrontend},
    };

    /*
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn size_test() {
      assert_size!(ExpectProxyProtocol<mio::net::TcpStream>, 520);
      assert_size!(Http<mio::net::TcpStream>, 1232);
      assert_size!(Pipe<mio::net::TcpStream>, 272);
      assert_size!(State, 1240);
      // fails depending on the platform?
      assert_size!(Session, 1592);
    }
    */

    #[test]
    fn round_trip() {
        setup_test_logger!();
        let front_port = crate::testing::provide_port();
        let backend_server = Arc::new(
            tiny_http::Server::http("127.0.0.1:0").expect("could not create tiny_http server"),
        );
        let backend_port = backend_server
            .server_addr()
            .to_ip()
            .expect("tiny_http server should bind to IP address")
            .port();

        let barrier = Arc::new(Barrier::new(2));

        let config = ListenerBuilder::new_http(SocketAddress::new_v4(127, 0, 0, 1, front_port))
            .to_http(None)
            .expect("could not create listener config");

        let (mut command, channel) =
            Channel::generate(1000, 10000).expect("should create a channel");

        thread::scope(|s| {
            let backend_handle = backend_server.clone();
            let barrier_clone = barrier.to_owned();
            s.spawn(move || {
                setup_test_logger!();
                start_server(&backend_handle, barrier_clone);
            });
            barrier.wait();

            s.spawn(move || {
                setup_test_logger!();
                start_http_worker(config, channel, 10, 16384)
                    .expect("could not start the http server");
            });

            let front = RequestHttpFrontend {
                cluster_id: Some("cluster_1".to_owned()),
                address: SocketAddress::new_v4(127, 0, 0, 1, front_port),
                hostname: "localhost".to_owned(),
                path: PathRule::prefix("/".to_owned()),
                ..Default::default()
            };
            command
                .write_message(&WorkerRequest {
                    id: "ID_ABCD".to_owned(),
                    content: RequestType::AddHttpFrontend(front).into(),
                })
                .expect("could not send AddHttpFrontend");
            let backend = Backend {
                cluster_id: "cluster_1".to_owned(),
                backend_id: "cluster_1-0".to_owned(),
                address: SocketAddress::new_v4(127, 0, 0, 1, backend_port).into(),
                load_balancing_parameters: Some(LoadBalancingParams::default()),
                sticky_id: None,
                backup: None,
            };
            command
                .write_message(&WorkerRequest {
                    id: "ID_EFGH".to_owned(),
                    content: RequestType::AddBackend(backend.to_add_backend()).into(),
                })
                .expect("could not send AddBackend");

            println!("test received: {:?}", command.read_message());
            println!("test received: {:?}", command.read_message());

            let mut client =
                TcpStream::connect(("127.0.0.1", front_port)).expect("could not connect to sozu");

            client
                .set_read_timeout(Some(Duration::new(1, 0)))
                .expect("could not set read timeout");
            let request = format!(
                "GET / HTTP/1.1\r\nHost: localhost:{front_port}\r\nConnection: Close\r\n\r\n"
            );
            let w = client.write(request.as_bytes());
            println!("http client write: {w:?}");

            barrier.wait();
            let mut buffer = [0; 4096];
            let mut index = 0;

            // tiny_http responds with exactly 191 bytes for a "hello world" body
            // (headers + body). This is deterministic for a given tiny_http version.
            let expected_len = 191;

            loop {
                assert!(index <= expected_len);
                if index == expected_len {
                    break;
                }

                let r = client.read(&mut buffer[index..]);
                println!("http client read: {r:?}");
                match r {
                    Err(e) => panic!("client request should not fail. Error: {e:?}"),
                    Ok(sz) => {
                        index += sz;
                    }
                }
            }
            println!(
                "Response: {}",
                str::from_utf8(&buffer[..index]).expect("could not make string from buffer")
            );

            // Gracefully stop the sozu worker so the scoped thread can join
            command
                .write_message(&WorkerRequest {
                    id: "ID_STOP".to_owned(),
                    content: RequestType::SoftStop(SoftStop {}).into(),
                })
                .expect("could not send SoftStop");
            // Unblock the backend server so its thread can exit
            backend_server.unblock();
        });
    }

    #[test]
    fn keep_alive() {
        setup_test_logger!();
        let front_port = crate::testing::provide_port();
        let backend_server = Arc::new(
            tiny_http::Server::http("127.0.0.1:0").expect("could not create tiny_http server"),
        );
        let backend_port = backend_server
            .server_addr()
            .to_ip()
            .expect("tiny_http server should bind to IP address")
            .port();

        let barrier = Arc::new(Barrier::new(2));

        let config = ListenerBuilder::new_http(SocketAddress::new_v4(127, 0, 0, 1, front_port))
            .to_http(None)
            .expect("could not create listener config");

        let (mut command, channel) =
            Channel::generate(1000, 10000).expect("should create a channel");

        thread::scope(|s| {
            let backend_handle = backend_server.clone();
            let barrier_clone = barrier.to_owned();
            s.spawn(move || {
                setup_test_logger!();
                start_server(&backend_handle, barrier_clone);
            });
            barrier.wait();

            s.spawn(move || {
                setup_test_logger!();
                start_http_worker(config, channel, 10, 16384)
                    .expect("could not start the http server");
            });

            let front = RequestHttpFrontend {
                address: SocketAddress::new_v4(127, 0, 0, 1, front_port),
                hostname: "localhost".to_owned(),
                path: PathRule::prefix("/".to_owned()),
                cluster_id: Some("cluster_1".to_owned()),
                ..Default::default()
            };
            command
                .write_message(&WorkerRequest {
                    id: "ID_ABCD".to_owned(),
                    content: RequestType::AddHttpFrontend(front).into(),
                })
                .expect("could not send AddHttpFrontend");
            let backend = Backend {
                address: SocketAddress::new_v4(127, 0, 0, 1, backend_port).into(),
                backend_id: "cluster_1-0".to_owned(),
                backup: None,
                cluster_id: "cluster_1".to_owned(),
                load_balancing_parameters: Some(LoadBalancingParams::default()),
                sticky_id: None,
            };
            command
                .write_message(&WorkerRequest {
                    id: "ID_EFGH".to_owned(),
                    content: RequestType::AddBackend(backend.to_add_backend()).into(),
                })
                .expect("could not send AddBackend");

            println!("test received: {:?}", command.read_message());
            println!("test received: {:?}", command.read_message());

            let mut client =
                TcpStream::connect(("127.0.0.1", front_port)).expect("could not connect to sozu");
            client
                .set_read_timeout(Some(Duration::new(5, 0)))
                .expect("could not set read timeout");

            // tiny_http responds with exactly 191 bytes for a "hello world" body
            // (headers + body). This is deterministic for a given tiny_http version.
            let expected_len = 191;

            let request = format!("GET / HTTP/1.1\r\nHost: localhost:{front_port}\r\n\r\n");
            let w = client
                .write(request.as_bytes())
                .expect("could not write first request");
            println!("http client write: {w:?}");
            barrier.wait();

            let mut buffer = [0; 4096];
            let mut index = 0;

            loop {
                assert!(index <= expected_len);
                if index == expected_len {
                    break;
                }

                let r = client.read(&mut buffer[index..]);
                println!("http client read: {r:?}");
                match r {
                    Err(e) => panic!("client request should not fail. Error: {e:?}"),
                    Ok(sz) => {
                        index += sz;
                    }
                }
            }

            println!(
                "Response: {}",
                str::from_utf8(&buffer[..index]).expect("could not make string from buffer")
            );

            println!("first request ended, will send second one");
            let request2 = format!("GET / HTTP/1.1\r\nHost: localhost:{front_port}\r\n\r\n");
            let w2 = client.write(request2.as_bytes());
            println!("http client write: {w2:?}");
            barrier.wait();

            let mut buffer2 = [0; 4096];
            let mut index = 0;

            loop {
                assert!(index <= expected_len);
                if index == expected_len {
                    break;
                }

                let r2 = client.read(&mut buffer2[index..]);
                println!("http client read: {r2:?}");
                match r2 {
                    Err(e) => panic!("client request should not fail. Error: {e:?}"),
                    Ok(sz) => {
                        index += sz;
                    }
                }
            }
            println!(
                "Response: {}",
                str::from_utf8(&buffer2[..index]).expect("could not make string from buffer")
            );

            // Gracefully stop the sozu worker so the scoped thread can join
            command
                .write_message(&WorkerRequest {
                    id: "ID_STOP".to_owned(),
                    content: RequestType::SoftStop(SoftStop {}).into(),
                })
                .expect("could not send SoftStop");
            // Unblock the backend server so its thread can exit
            backend_server.unblock();
        });
    }

    use self::tiny_http::Response;

    fn start_server(server: &tiny_http::Server, barrier: Arc<Barrier>) {
        let addr = server.server_addr();
        info!("starting web server on {:?}", addr);
        barrier.wait();

        for request in server.incoming_requests() {
            info!(
                "backend web server got request -> method: {:?}, url: {:?}, headers: {:?}",
                request.method(),
                request.url(),
                request.headers()
            );

            let response = Response::from_string("hello world");
            request
                .respond(response)
                .expect("could not respond to request");
            info!("backend web server sent response");
            barrier.wait();
            info!("server session stopped");
        }

        println!("server on {addr:?} closed");
    }

    #[test]
    fn frontend_from_request_test() {
        let cluster_id1 = "cluster_1".to_owned();
        let cluster_id2 = "cluster_2".to_owned();
        let cluster_id3 = "cluster_3".to_owned();
        let uri1 = "/".to_owned();
        let uri2 = "/yolo".to_owned();
        let uri3 = "/yolo/swag".to_owned();

        let mut fronts = Router::new();
        fronts
            .add_http_front(&HttpFrontend {
                address: "0.0.0.0:80".parse().unwrap(),
                hostname: "lolcatho.st".to_owned(),
                method: None,
                path: PathRule::prefix(uri1),
                position: RulePosition::Tree,
                cluster_id: Some(cluster_id1),
                tags: None,
                redirect: None,
                redirect_scheme: None,
                redirect_template: None,
                rewrite_host: None,
                rewrite_path: None,
                rewrite_port: None,
                required_auth: None,
                headers: Vec::new(),
                hsts: None,
            })
            .expect("Could not add http frontend");
        fronts
            .add_http_front(&HttpFrontend {
                address: "0.0.0.0:80".parse().unwrap(),
                hostname: "lolcatho.st".to_owned(),
                method: None,
                path: PathRule::prefix(uri2),
                position: RulePosition::Tree,
                cluster_id: Some(cluster_id2),
                tags: None,
                redirect: None,
                redirect_scheme: None,
                redirect_template: None,
                rewrite_host: None,
                rewrite_path: None,
                rewrite_port: None,
                required_auth: None,
                headers: Vec::new(),
                hsts: None,
            })
            .expect("Could not add http frontend");
        fronts
            .add_http_front(&HttpFrontend {
                address: "0.0.0.0:80".parse().unwrap(),
                hostname: "lolcatho.st".to_owned(),
                method: None,
                path: PathRule::prefix(uri3),
                position: RulePosition::Tree,
                cluster_id: Some(cluster_id3),
                tags: None,
                redirect: None,
                redirect_scheme: None,
                redirect_template: None,
                rewrite_host: None,
                rewrite_path: None,
                rewrite_port: None,
                required_auth: None,
                headers: Vec::new(),
                hsts: None,
            })
            .expect("Could not add http frontend");
        fronts
            .add_http_front(&HttpFrontend {
                address: "0.0.0.0:80".parse().unwrap(),
                hostname: "other.domain".to_owned(),
                method: None,
                path: PathRule::prefix("/test".to_owned()),
                position: RulePosition::Tree,
                cluster_id: Some("cluster_1".to_owned()),
                tags: None,
                redirect: None,
                redirect_scheme: None,
                redirect_template: None,
                rewrite_host: None,
                rewrite_path: None,
                rewrite_port: None,
                required_auth: None,
                headers: Vec::new(),
                hsts: None,
            })
            .expect("Could not add http frontend");

        let address = SocketAddress::new_v4(127, 0, 0, 1, 1030);

        let default_config = ListenerBuilder::new_http(address)
            .to_http(None)
            .expect("Could not create default HTTP listener config");

        let listener = HttpListener {
            listener: None,
            address: address.into(),
            fronts,
            answers: Rc::new(RefCell::new(HttpAnswers::new(&BTreeMap::new()).unwrap())),
            config: default_config,
            token: Token(0),
            active: true,
            tags: BTreeMap::new(),
        };

        let frontend1 = listener.frontend_from_request("lolcatho.st", "/", &Method::Get);
        let frontend2 = listener.frontend_from_request("lolcatho.st", "/test", &Method::Get);
        let frontend3 = listener.frontend_from_request("lolcatho.st", "/yolo/test", &Method::Get);
        let frontend4 = listener.frontend_from_request("lolcatho.st", "/yolo/swag", &Method::Get);
        let frontend5 = listener.frontend_from_request("domain", "/", &Method::Get);
        assert_eq!(
            frontend1
                .expect("should find frontend")
                .cluster_id
                .as_deref(),
            Some("cluster_1")
        );
        assert_eq!(
            frontend2
                .expect("should find frontend")
                .cluster_id
                .as_deref(),
            Some("cluster_1")
        );
        assert_eq!(
            frontend3
                .expect("should find frontend")
                .cluster_id
                .as_deref(),
            Some("cluster_2")
        );
        assert_eq!(
            frontend4
                .expect("should find frontend")
                .cluster_id
                .as_deref(),
            Some("cluster_3")
        );
        assert!(frontend5.is_err());
    }

    #[test]
    fn h2_stream_idle_timeout_inherits_back_timeout() {
        let address = SocketAddress::new_v4(127, 0, 0, 1, 1040);
        let build = |back_timeout: u32, explicit: Option<u32>| -> HttpListener {
            let mut cfg = ListenerBuilder::new_http(address)
                .to_http(None)
                .expect("default HTTP listener config");
            cfg.back_timeout = back_timeout;
            cfg.h2_stream_idle_timeout_seconds = explicit;
            HttpListener::new(cfg, Token(0)).expect("build listener")
        };

        // Knob unset: inherit back_timeout when it exceeds the 30s floor.
        assert_eq!(
            build(180, None).get_h2_stream_idle_timeout(),
            Duration::from_secs(180)
        );

        // Knob unset, back_timeout below floor: stay at 30s to preserve the
        // slow-multiplex Slowloris mitigation.
        assert_eq!(
            build(5, None).get_h2_stream_idle_timeout(),
            Duration::from_secs(30)
        );

        // Explicit values win in both directions — including below the floor,
        // so operators under attack can tighten the deadline.
        assert_eq!(
            build(180, Some(10)).get_h2_stream_idle_timeout(),
            Duration::from_secs(10)
        );
        assert_eq!(
            build(5, Some(600)).get_h2_stream_idle_timeout(),
            Duration::from_secs(600)
        );

        // `Some(0)` is clamped to 1s to keep the deadline non-degenerate.
        assert_eq!(
            build(180, Some(0)).get_h2_stream_idle_timeout(),
            Duration::from_secs(1)
        );
    }
}
