use std::{
    cell::RefCell,
    collections::{hash_map::Entry, BTreeMap, HashMap},
    io::{ErrorKind, Read},
    net::{Shutdown, SocketAddr as StdSocketAddr},
    os::unix::{io::AsRawFd, net::UnixStream},
    rc::{Rc, Weak},
    str::{from_utf8, from_utf8_unchecked},
    sync::Arc,
};

use anyhow::{bail, Context};
use mio::{
    net::{TcpListener as MioTcpListener, TcpStream as MioTcpStream},
    unix::SourceFd,
    Interest, Poll, Registry, Token,
};
use rustls::{
    cipher_suite::{
        TLS13_AES_128_GCM_SHA256, TLS13_AES_256_GCM_SHA384, TLS13_CHACHA20_POLY1305_SHA256,
        TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256, TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256, TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
        TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384, TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
    },
    CipherSuite, ProtocolVersion, ServerConfig, ServerConnection, SupportedCipherSuite,
};
use rusty_ulid::Ulid;
use slab::Slab;
use sozu_command::{
    config::DEFAULT_CIPHER_SUITES,
    proxy::{RemoveListener, ReplaceCertificate},
};
use time::{Duration, Instant};

use crate::{
    backends::BackendMap,
    buffer_queue::BufferQueue,
    pool::Pool,
    protocol::{
        h2::Http2,
        http::{
            answers::HttpAnswers,
            parser::{hostname_and_port, Method, RequestState},
            DefaultAnswerStatus,
        },
        proxy_protocol::expect::ExpectProxyProtocol,
        rustls::TlsHandshake as RustlsHandshake,
        Http, Pipe, ProtocolResult, StickySession,
    },
    retry::RetryPolicy,
    router::Router,
    server::{
        push_event, ListenSession, ListenToken, ProxyChannel, Server, SessionManager, SessionToken,
        CONN_RETRIES,
    },
    socket::{server_bind, FrontRustls},
    sozu_command::{
        logging,
        proxy::{
            AddCertificate, CertificateFingerprint, Cluster, HttpFrontend, HttpsListener,
            ProxyEvent, ProxyRequest, ProxyRequestOrder, ProxyResponse, ProxyResponseContent,
            ProxyResponseStatus, Query, QueryAnswer, QueryAnswerCertificate, QueryCertificateType,
            RemoveCertificate, Route, TlsVersion,
        },
        ready::Ready,
        scm_socket::ScmSocket,
        state::ClusterId,
    },
    timer::TimeoutContainer,
    tls::{
        CertificateResolver, GenericCertificateResolverError, MutexWrappedCertificateResolver,
        ParsedCertificateAndKey,
    },
    util::UnwrapLog,
    AcceptError, Backend, BackendConnectAction, BackendConnectionStatus, ListenerHandler, Protocol,
    ProxyConfiguration, ProxySession, Readiness, SessionMetrics, SessionResult,
};

// const SERVER_PROTOS: &[&str] = &["http/1.1", "h2"];
const SERVER_PROTOS: &[&str] = &["http/1.1"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsCluster {
    pub cluster_id: String,
    pub hostname: String,
    pub path_begin: String,
}

pub enum State {
    Expect(ExpectProxyProtocol<MioTcpStream>, ServerConnection),
    Handshake(RustlsHandshake),
    Http(Http<FrontRustls, Listener>),
    WebSocket(Pipe<FrontRustls, Listener>),
    Http2(Http2<FrontRustls>),
    /// Temporary state used to extract and take ownership over the previous state.
    /// It should always be replaced by the next valid state and thus never be encountered.
    Invalid,
}

pub enum AlpnProtocols {
    H2,
    Http11,
}

pub struct Session {
    pub frontend_token: Token,
    pub backend: Option<Rc<RefCell<Backend>>>,
    pub back_connected: BackendConnectionStatus,
    state: State,
    pub public_address: StdSocketAddr,
    pool: Weak<RefCell<Pool>>,
    proxy: Rc<RefCell<Proxy>>,
    pub metrics: SessionMetrics,
    pub cluster_id: Option<String>,
    sticky_name: String,
    last_event: Instant,
    pub listener_token: Token,
    pub connection_attempt: u8,
    peer_address: Option<StdSocketAddr>,
    answers: Rc<RefCell<HttpAnswers>>,
    front_timeout: TimeoutContainer,
    frontend_timeout_duration: Duration,
    backend_timeout_duration: Duration,
    pub listener: Rc<RefCell<Listener>>,
}

impl Session {
    pub fn new(
        rustls_details: ServerConnection,
        sock: MioTcpStream,
        token: Token,
        pool: Weak<RefCell<Pool>>,
        proxy: Rc<RefCell<Proxy>>,
        public_address: StdSocketAddr,
        expect_proxy: bool,
        sticky_name: String,
        answers: Rc<RefCell<HttpAnswers>>,
        listener_token: Token,
        wait_time: Duration,
        frontend_timeout_duration: Duration,
        backend_timeout_duration: Duration,
        request_timeout_duration: Duration,
        listener: Rc<RefCell<Listener>>,
    ) -> Session {
        let peer_address = if expect_proxy {
            // Will be defined later once the expect proxy header has been received and parsed
            None
        } else {
            sock.peer_addr().ok()
        };

        let request_id = Ulid::generate();
        let front_timeout = TimeoutContainer::new(request_timeout_duration, token);

        let state = if expect_proxy {
            trace!("starting in expect proxy state");
            gauge_add!("protocol.proxy.expect", 1);
            State::Expect(
                ExpectProxyProtocol::new(sock, token, request_id),
                rustls_details,
            )
        } else {
            gauge_add!("protocol.tls.handshake", 1);
            State::Handshake(RustlsHandshake::new(rustls_details, sock, request_id))
        };

        let metrics = SessionMetrics::new(Some(wait_time));
        let mut session = Session {
            frontend_token: token,
            backend: None,
            back_connected: BackendConnectionStatus::NotConnected,
            state,
            public_address,
            pool,
            proxy,
            sticky_name,
            metrics,
            cluster_id: None,
            last_event: Instant::now(),
            listener_token,
            connection_attempt: 0,
            peer_address,
            answers,
            front_timeout,
            frontend_timeout_duration,
            backend_timeout_duration,
            listener,
        };

        session.front_readiness().interest = Ready::readable() | Ready::hup() | Ready::error();
        session
    }

    pub fn set_answer(&mut self, answer: DefaultAnswerStatus, buf: Option<Rc<Vec<u8>>>) {
        match &mut self.state {
            State::Invalid => unreachable!(),
            State::Http(http) => {
                http.set_answer(answer, buf);
            }
            _ => (),
        }
    }

    pub fn upgrade(&mut self) -> bool {
        // We swap an Invalid state to take ownership over the current state
        let mut owned_state = State::Invalid;
        std::mem::swap(&mut owned_state, &mut self.state);
        let new_state = match owned_state {
            State::Invalid => unreachable!(),
            State::Expect(expect, ssl) => self.upgrade_expect(expect, ssl),
            State::Handshake(handshake) => self.upgrade_handshake(handshake),
            State::Http(http) => self.upgrade_http(http),
            State::Http2(_) => self.upgrade_http2(),
            State::WebSocket(websocket) => self.upgrade_websocket(websocket),
        };

        match new_state {
            Some(state) => {
                self.state = state;
                true
            }
            None => {
                // The state stays Invalid, but the Session should be closed right after
                false
            }
        }
    }

    fn upgrade_expect(
        &mut self,
        expect: ExpectProxyProtocol<MioTcpStream>,
        ssl: ServerConnection,
    ) -> Option<State> {
        if let Some(ref addresses) = expect.addresses {
            if let (Some(public_address), Some(session_address)) =
                (addresses.destination(), addresses.source())
            {
                self.public_address = public_address;
                self.peer_address = Some(session_address);

                let ExpectProxyProtocol {
                    frontend,
                    readiness,
                    request_id,
                    ..
                } = expect;

                let mut tls = RustlsHandshake::new(ssl, frontend, request_id);
                tls.readiness.event = readiness.event;
                tls.readiness.event.insert(Ready::readable());

                gauge_add!("protocol.proxy.expect", -1);
                gauge_add!("protocol.tls.handshake", 1);
                return Some(State::Handshake(tls));
            }
        }

        // currently, only happens in expect proxy protocol with AF_UNSPEC address
        // error!("failed to upgrade from expect");
        None
    }

    fn upgrade_handshake(&mut self, handshake: RustlsHandshake) -> Option<State> {
        // Add 1st routing phase
        // - get SNI
        // - get ALPN
        // - find corresponding listener
        // - determine next protocol (tcps, https ,http2)

        let front_buf = self.pool.upgrade().and_then(|p| p.borrow_mut().checkout());
        if front_buf.is_none() {
            return None;
        }

        let sni = handshake.session.sni_hostname();
        let alpn = handshake.session.alpn_protocol();
        let alpn = alpn.and_then(|alpn| from_utf8(alpn).ok());
        info!(
            "Successful TLS Handshake with, received: {:?} {:?}",
            sni, alpn
        );

        let alpn = match alpn {
            Some("http/1.1") => AlpnProtocols::Http11,
            Some("h2") => AlpnProtocols::H2,
            _ => todo!(),
        };

        let mut front_buf = front_buf.unwrap();
        if let Some(version) = handshake.session.protocol_version() {
            incr!(rustls_version_str(version));
        };
        if let Some(cipher) = handshake.session.negotiated_cipher_suite() {
            incr!(rustls_ciphersuite_str(cipher));
        };

        let front_stream = FrontRustls {
            stream: handshake.stream,
            session: handshake.session,
        };

        let readiness = handshake.readiness.clone();

        gauge_add!("protocol.tls.handshake", -1);
        match alpn {
            AlpnProtocols::Http11 => {
                let mut http = Http::new(
                    front_stream,
                    self.frontend_token,
                    handshake.request_id,
                    self.pool.clone(),
                    self.public_address,
                    self.peer_address,
                    self.sticky_name.clone(),
                    Protocol::HTTPS,
                    self.answers.clone(),
                    self.front_timeout.take(),
                    self.frontend_timeout_duration,
                    self.backend_timeout_duration,
                    self.listener.clone(),
                );

                let res = http.frontend.session.reader().read(front_buf.space());
                match res {
                    Ok(sz) => {
                        //info!("rustls upgrade: there were {} bytes of plaintext available", sz);
                        front_buf.fill(sz);
                        count!("bytes_in", sz as i64);
                        self.metrics.bin += sz;
                    }
                    Err(e) => {
                        error!("read error: {:?}", e);
                    }
                }

                let sz = front_buf.available_data();
                let mut buf = BufferQueue::with_buffer(front_buf);
                buf.sliced_input(sz);

                http.front_buf = Some(buf);
                http.front_readiness = readiness;
                http.front_readiness.interest = Ready::readable() | Ready::hup() | Ready::error();

                gauge_add!("protocol.https", 1);
                Some(State::Http(http))
            }
            AlpnProtocols::H2 => {
                let mut http = Http2::new(
                    front_stream,
                    self.frontend_token,
                    self.pool.clone(),
                    Some(self.public_address),
                    None,
                    self.sticky_name.clone(),
                );

                http.frontend.readiness = readiness;
                http.frontend.readiness.interest =
                    Ready::readable() | Ready::hup() | Ready::error();

                gauge_add!("protocol.http2", 1);
                Some(State::Http2(http))
            }
        }
    }

    fn upgrade_http(&self, mut http: Http<FrontRustls, Listener>) -> Option<State> {
        debug!("https switching to wss");
        let front_token = self.frontend_token;
        let back_token = unwrap_msg!(http.back_token());
        let ws_context = http.websocket_context();

        let front_buf = match http.front_buf {
            Some(buf) => buf.buffer,
            None => {
                let pool = match self.pool.upgrade() {
                    Some(p) => p,
                    None => return None,
                };

                let buffer = match pool.borrow_mut().checkout() {
                    Some(buf) => buf,
                    None => return None,
                };
                buffer
            }
        };
        let back_buf = match http.back_buf {
            Some(buf) => buf.buffer,
            None => {
                let pool = match self.pool.upgrade() {
                    Some(p) => p,
                    None => return None,
                };

                let buffer = match pool.borrow_mut().checkout() {
                    Some(buf) => buf,
                    None => return None,
                };
                buffer
            }
        };

        let mut pipe = Pipe::new(
            http.frontend,
            front_token,
            http.request_id,
            http.cluster_id,
            http.backend_id,
            Some(ws_context),
            http.backend,
            front_buf,
            back_buf,
            http.session_address,
            Protocol::HTTPS,
            self.listener.clone(),
        );

        pipe.front_readiness.event = http.front_readiness.event;
        pipe.back_readiness.event = http.back_readiness.event;
        http.front_timeout
            .set_duration(self.frontend_timeout_duration);
        http.back_timeout
            .set_duration(self.backend_timeout_duration);
        pipe.front_timeout = Some(http.front_timeout);
        pipe.back_timeout = Some(http.back_timeout);
        pipe.set_back_token(back_token);
        pipe.set_cluster_id(self.cluster_id.clone());

        gauge_add!("protocol.https", -1);
        gauge_add!("protocol.wss", 1);
        gauge_add!("websocket.active_requests", 1);
        gauge_add!("http.active_requests", -1);
        Some(State::WebSocket(pipe))
    }

    fn upgrade_http2(&self) -> Option<State> {
        todo!()
    }

    fn upgrade_websocket(&self, websocket: Pipe<FrontRustls, Listener>) -> Option<State> {
        // what do we do here?
        error!("Upgrade called on WSS, this should not happen");
        Some(State::WebSocket(websocket))
    }

    fn front_hup(&mut self) -> SessionResult {
        match &mut self.state {
            State::Invalid => unreachable!(),
            State::Http(http) => http.front_hup(),
            State::WebSocket(pipe) => pipe.front_hup(&mut self.metrics),
            State::Handshake(_) => SessionResult::CloseSession,
            State::Expect(_, _) => SessionResult::CloseSession,
            State::Http2(_) => todo!(),
        }
    }

    fn back_hup(&mut self) -> SessionResult {
        match &mut self.state {
            State::Invalid => unreachable!(),
            State::Http(http) => http.back_hup(),
            State::WebSocket(pipe) => pipe.back_hup(&mut self.metrics),
            State::Handshake(_) => {
                error!("why a backend HUP event while still in frontend handshake?");
                SessionResult::CloseSession
            }
            State::Expect(_, _) => {
                error!("why a backend HUP event while still in frontend proxy protocol expect?");
                SessionResult::CloseSession
            }
            State::Http2(_) => todo!(),
        }
    }

    fn log_context(&self) -> String {
        match &self.state {
            State::Invalid => unreachable!(),
            State::Http(http) => http.log_context().to_string(),
            _ => "".to_string(),
        }
    }

    fn readable(&mut self) -> SessionResult {
        let (upgrade, session_result) = match &mut self.state {
            State::Invalid => unreachable!(),
            State::Expect(expect, _) => {
                if !self.front_timeout.reset() {
                    error!("could not reset front timeout");
                }
                expect.readable(&mut self.metrics)
            }
            State::Handshake(handshake) => {
                if !self.front_timeout.reset() {
                    error!("could not reset front timeout");
                }
                handshake.readable()
            }
            State::Http(http) => (ProtocolResult::Continue, http.readable(&mut self.metrics)),
            State::WebSocket(pipe) => (ProtocolResult::Continue, pipe.readable(&mut self.metrics)),
            State::Http2(_) => todo!(),
        };

        if upgrade == ProtocolResult::Continue {
            session_result
        } else if self.upgrade() {
            match &mut self.state {
                State::Invalid => unreachable!(),
                State::Http(http) => http.readable(&mut self.metrics),
                State::Http2(_) => todo!(),
                _ => session_result,
            }
        } else {
            SessionResult::CloseSession
        }
    }

    fn writable(&mut self) -> SessionResult {
        let (upgrade, session_result) = match self.state {
            State::Invalid => unreachable!(),
            State::Expect(_, _) => return SessionResult::CloseSession,
            State::Handshake(ref mut handshake) => handshake.writable(),
            State::Http(ref mut http) => {
                (ProtocolResult::Continue, http.writable(&mut self.metrics))
            }
            State::WebSocket(ref mut pipe) => {
                (ProtocolResult::Continue, pipe.writable(&mut self.metrics))
            }
            State::Http2(_) => todo!(),
        };

        if upgrade == ProtocolResult::Continue {
            return session_result;
        }
        if self.upgrade() {
            if (self.front_readiness().event & self.front_readiness().interest).is_writable() {
                return self.writable();
            }
            return SessionResult::Continue;
        }
        SessionResult::CloseSession
    }

    fn back_readable(&mut self) -> SessionResult {
        let (upgrade, session_result) = match self.state {
            State::Invalid => unreachable!(),
            State::Expect(_, _) => return SessionResult::CloseSession,
            State::Http(ref mut http) => http.back_readable(&mut self.metrics),
            State::Handshake(_) => (ProtocolResult::Continue, SessionResult::CloseSession),
            State::WebSocket(ref mut pipe) => (
                ProtocolResult::Continue,
                pipe.back_readable(&mut self.metrics),
            ),
            State::Http2(_) => todo!(),
        };

        if upgrade == ProtocolResult::Continue {
            return session_result;
        }
        if !self.upgrade() {
            return SessionResult::CloseSession;
        }
        match self.state {
            State::Invalid => unreachable!(),
            State::WebSocket(ref mut pipe) => pipe.back_readable(&mut self.metrics),
            _ => session_result,
        }
    }

    fn back_writable(&mut self) -> SessionResult {
        match self.state {
            State::Invalid => unreachable!(),
            State::Expect(_, _) | State::Handshake(_) => SessionResult::CloseSession,
            State::Http(ref mut http) => http.back_writable(&mut self.metrics),
            State::WebSocket(ref mut pipe) => pipe.back_writable(&mut self.metrics),
            State::Http2(_) => todo!(),
        }
    }

    fn front_socket(&self) -> Option<&MioTcpStream> {
        match &self.state {
            State::Invalid => unreachable!(),
            State::Expect(expect, _) => Some(expect.front_socket()),
            State::Handshake(handshake) => Some(&handshake.stream),
            State::Http(http) => Some(http.front_socket()),
            State::WebSocket(pipe) => Some(pipe.front_socket()),
            State::Http2(_) => todo!(),
        }
    }

    fn back_socket(&self) -> Option<&MioTcpStream> {
        match self.state {
            State::Invalid => unreachable!(),
            State::Expect(_, _) => None,
            State::Handshake(_) => None,
            State::Http(ref http) => http.back_socket(),
            State::WebSocket(ref pipe) => pipe.back_socket(),
            State::Http2(_) => todo!(),
        }
    }

    fn back_token(&self) -> Option<Token> {
        match self.state {
            State::Invalid => unreachable!(),
            State::Expect(_, _) => None,
            State::Handshake(_) => None,
            State::Http(ref http) => http.back_token(),
            State::WebSocket(ref pipe) => pipe.back_token(),
            State::Http2(_) => todo!(),
        }
    }

    fn set_back_socket(&mut self, sock: MioTcpStream) {
        let backend = self.backend.clone();
        match self.state {
            State::Invalid => unreachable!(),
            State::Http(ref mut http) => http.set_back_socket(sock, backend),
            _ => {
                // what to do when set_back_socket is called on the other states?
                todo!()
            }
        }
    }

    fn set_back_token(&mut self, token: Token) {
        match self.state {
            State::Invalid => unreachable!(),
            State::Http(ref mut http) => http.set_back_token(token),
            State::Http2(_) => todo!(),
            State::WebSocket(ref mut pipe) => pipe.set_back_token(token),
            _ => {}
        }
    }

    fn back_connected(&self) -> BackendConnectionStatus {
        self.back_connected
    }

    fn set_back_connected(&mut self, connected: BackendConnectionStatus) {
        let last = self.back_connected;
        self.back_connected = connected;

        if connected != BackendConnectionStatus::Connected {
            return;
        }

        gauge_add!("backend.connections", 1);
        gauge_add!(
            "connections_per_backend",
            1,
            self.cluster_id.as_deref(),
            self.metrics.backend_id.as_deref()
        );

        // the back timeout was of connect_timeout duration before,
        // now that we're connected, move to backend_timeout duration
        let timeout = self.backend_timeout_duration;
        match self.state {
            State::Invalid => unreachable!(),
            State::Http(ref mut http) => {
                http.set_back_timeout(timeout);
                http.cancel_backend_timeout();
            }
            _ => {}
        }

        if let Some(backend) = &self.backend {
            let mut backend = backend.borrow_mut();

            if backend.retry_policy.is_down() {
                incr!(
                    "up",
                    self.cluster_id.as_deref(),
                    self.metrics.backend_id.as_deref()
                );

                info!(
                    "backend server {} at {} is up",
                    backend.backend_id, backend.address
                );

                push_event(ProxyEvent::BackendUp(
                    backend.backend_id.clone(),
                    backend.address,
                ));
            }

            if let BackendConnectionStatus::Connecting(start) = last {
                backend.set_connection_time(Instant::now() - start);
            }

            backend.active_requests += 1;
            backend.failures = 0;
            backend.retry_policy.succeed();
        }
    }

    fn metrics(&mut self) -> &mut SessionMetrics {
        &mut self.metrics
    }

    fn remove_backend(&mut self) {
        if let Some(backend) = self.backend.take() {
            match self.state {
                State::Invalid => unreachable!(),
                State::Http(ref mut http) => http.clear_back_token(),
                _ => {}
            }

            backend.borrow_mut().dec_connections();
        }
    }

    fn front_readiness(&mut self) -> &mut Readiness {
        match self.state {
            State::Invalid => unreachable!(),
            State::Expect(ref mut expect, _) => &mut expect.readiness,
            State::Handshake(ref mut handshake) => &mut handshake.readiness,
            State::Http(ref mut http) => http.front_readiness(),
            State::WebSocket(ref mut pipe) => &mut pipe.front_readiness,
            State::Http2(_) => todo!(),
        }
    }

    fn back_readiness(&mut self) -> Option<&mut Readiness> {
        match self.state {
            State::Invalid => unreachable!(),
            State::Http(ref mut http) => Some(http.back_readiness()),
            State::Http2(_) => todo!(),
            State::WebSocket(ref mut pipe) => Some(&mut pipe.back_readiness),
            _ => None,
        }
    }

    fn fail_backend_connection(&mut self) {
        self.backend.as_ref().map(|backend| {
            let mut backend = backend.borrow_mut();
            backend.failures += 1;

            let already_unavailable = backend.retry_policy.is_down();
            backend.retry_policy.fail();
            incr!(
                "connections.error",
                self.cluster_id.as_deref(),
                self.metrics.backend_id.as_deref()
            );
            if !already_unavailable && backend.retry_policy.is_down() {
                error!(
                    "backend server {} at {} is down",
                    backend.backend_id, backend.address
                );
                incr!(
                    "down",
                    self.cluster_id.as_deref(),
                    self.metrics.backend_id.as_deref()
                );

                push_event(ProxyEvent::BackendDown(
                    backend.backend_id.clone(),
                    backend.address,
                ));
            }
        });
    }

    fn reset_connection_attempt(&mut self) {
        self.connection_attempt = 0;
    }

    fn cancel_timeouts(&mut self) {
        self.front_timeout.cancel();

        match self.state {
            State::Invalid => unreachable!(),
            State::Http(ref mut http) => http.cancel_timeouts(),
            State::WebSocket(ref mut pipe) => pipe.cancel_timeouts(),
            _ => {}
        }
    }

    fn cancel_backend_timeout(&mut self) {
        match self.state {
            State::Invalid => unreachable!(),
            State::Http(ref mut http) => http.cancel_backend_timeout(),
            _ => {}
        }
    }

    fn ready_inner(&mut self, session: Rc<RefCell<dyn ProxySession>>) -> SessionResult {
        let mut counter = 0;
        let max_loop_iterations = 100000;

        if self.back_connected().is_connecting()
            && self
                .back_readiness()
                .map(|r| r.event != Ready::empty())
                .unwrap_or(false)
        {
            self.cancel_backend_timeout();

            let back_is_hup = self
                .back_readiness()
                .map(|r| r.event.is_hup())
                .unwrap_or(false);
            let back_is_alive = match self.state {
                State::Invalid => unreachable!(),
                State::Http(ref mut http) => http.test_back_socket(),
                _ => false,
            };
            if back_is_hup && !back_is_alive {
                //retry connecting the backend
                error!(
                    "{} error connecting to backend, trying again",
                    self.log_context()
                );
                self.connection_attempt += 1;
                self.fail_backend_connection();

                self.back_connected = BackendConnectionStatus::Connecting(Instant::now());

                // trigger a backend reconnection
                self.close_backend();
                match self.connect_to_backend(session.clone()) {
                    // reuse connection or send a default answer, we can continue
                    Ok(BackendConnectAction::Reuse) => {}
                    Ok(BackendConnectAction::New) | Ok(BackendConnectAction::Replace) => {
                        // stop here, we must wait for an event
                        return SessionResult::Continue;
                    }
                    Err(connection_error) => {
                        error!("Error connecting to backend: {:#}", connection_error)
                    }
                }
            } else {
                self.metrics().backend_connected();
                self.reset_connection_attempt();
                self.set_back_connected(BackendConnectionStatus::Connected);
            }
        }

        if self.front_readiness().event.is_hup() {
            let order = self.front_hup();
            match order {
                SessionResult::CloseSession => {
                    return order;
                }
                _ => {
                    self.front_readiness().event.remove(Ready::hup());
                    return order;
                }
            }
        }

        let token = self.frontend_token;
        while counter < max_loop_iterations {
            let front_interest = self.front_readiness().interest & self.front_readiness().event;
            let back_interest = self
                .back_readiness()
                .map(|r| r.interest & r.event)
                .unwrap_or(Ready::empty());

            trace!(
                "PROXY\t{} {:?} {:?} -> {:?}",
                self.log_context(),
                token,
                self.front_readiness().clone(),
                self.back_readiness()
            );

            if front_interest == Ready::empty() && back_interest == Ready::empty() {
                break;
            }

            if self
                .back_readiness()
                .map(|r| r.event.is_hup())
                .unwrap_or(false)
                && self.front_readiness().interest.is_writable()
                && !self.front_readiness().event.is_writable()
            {
                break;
            }

            if front_interest.is_readable() {
                let session_result = self.readable();
                trace!(
                    "front readable\tinterpreting session order {:?}",
                    session_result
                );

                match session_result {
                    SessionResult::ConnectBackend => {
                        match self.connect_to_backend(session.clone()) {
                            // reuse connection or send a default answer, we can continue
                            Ok(BackendConnectAction::Reuse) => {}
                            Ok(BackendConnectAction::New) | Ok(BackendConnectAction::Replace) => {
                                // we must wait for an event
                                return SessionResult::Continue;
                            }
                            Err(connection_error) => {
                                error!("Error connecting to backend: {:#}", connection_error)
                            }
                        }
                    }
                    SessionResult::Continue => {}
                    order => return order,
                }
            }

            if back_interest.is_writable() {
                let order = self.back_writable();
                if order != SessionResult::Continue {
                    return order;
                }
            }

            if back_interest.is_readable() {
                let order = self.back_readable();
                if order != SessionResult::Continue {
                    return order;
                }
            }

            if front_interest.is_writable() {
                let order = self.writable();
                trace!("front writable\tinterpreting session order {:?}", order);
                if order != SessionResult::Continue {
                    return order;
                }
            }

            if back_interest.is_hup() {
                let order = self.back_hup();
                match order {
                    SessionResult::CloseSession | SessionResult::CloseBackend => {
                        return order;
                    }
                    SessionResult::Continue => {
                        // FIXME: do we have to fix something?
                        /*self.front_readiness().interest.insert(Ready::writable());
                        if ! self.front_readiness().event.is_writable() {
                          error!("got back socket HUP but there's still data, and front is not writable yet(HTTPS)[{:?} => {:?}]", self.frontend_token, self.back_token());
                          break;
                        }*/
                    }
                    _ => {
                        if let Some(r) = self.back_readiness() {
                            r.event.remove(Ready::hup())
                        }
                        return order;
                    }
                };
            }

            if front_interest.is_error() {
                error!(
                    "PROXY session {:?} front error, disconnecting",
                    self.frontend_token
                );
                self.front_readiness().interest = Ready::empty();
                self.back_readiness().map(|r| r.interest = Ready::empty());
                return SessionResult::CloseSession;
            }

            if back_interest.is_error() && self.back_hup() == SessionResult::CloseSession {
                self.front_readiness().interest = Ready::empty();
                self.back_readiness().map(|r| r.interest = Ready::empty());
                error!(
                    "PROXY session {:?} back error, disconnecting",
                    self.frontend_token
                );
                return SessionResult::CloseSession;
            }

            counter += 1;
        }

        if counter == max_loop_iterations {
            error!("PROXY\thandling session {:?} went through {} iterations, there's a probable infinite loop bug, closing the connection", self.frontend_token, max_loop_iterations);
            incr!("https.infinite_loop.error");

            let front_interest = self.front_readiness().filter_interest();
            let back_interest = self.back_readiness().map(|r| r.interest & r.event);

            let token = self.frontend_token;
            let back = self.back_readiness().cloned();
            error!(
                "PROXY\t{:?} readiness: {:?} -> {:?} | front: {:?} | back: {:?} ",
                token,
                self.front_readiness(),
                back,
                front_interest,
                back_interest
            );
            self.print_state();

            return SessionResult::CloseSession;
        }

        SessionResult::Continue
    }

    fn close_backend(&mut self) {
        if let Some(token) = self.back_token() {
            if let Some(fd) = self.back_socket().map(|stream| stream.as_raw_fd()) {
                let proxy = self.proxy.borrow();
                if let Err(e) = proxy.registry.deregister(&mut SourceFd(&fd)) {
                    error!("1error deregistering socket({:?}): {:?}", fd, e);
                }

                proxy.sessions.borrow_mut().slab.try_remove(token.0);
            }

            self.remove_backend();

            let back_connected = self.back_connected();
            if back_connected != BackendConnectionStatus::NotConnected {
                self.back_readiness().map(|r| r.event = Ready::empty());
                if let Some(sock) = self.back_socket() {
                    if let Err(e) = sock.shutdown(Shutdown::Both) {
                        if e.kind() != ErrorKind::NotConnected {
                            error!("error closing back socket({:?}): {:?}", sock, e);
                        }
                    }
                }
            }

            if back_connected == BackendConnectionStatus::Connected {
                gauge_add!("backend.connections", -1);
                gauge_add!(
                    "connections_per_backend",
                    -1,
                    self.cluster_id.as_deref(),
                    self.metrics.backend_id.as_deref()
                );
            }

            self.set_back_connected(BackendConnectionStatus::NotConnected);

            match self.state {
                State::Invalid => unreachable!(),
                State::Http(ref mut http) => {
                    http.clear_back_token();
                    http.remove_backend();
                }
                _ => {}
            }
        }
    }

    fn check_circuit_breaker(&mut self) -> anyhow::Result<()> {
        if self.connection_attempt >= CONN_RETRIES {
            error!("{} max connection attempt reached", self.log_context());
            self.set_answer(DefaultAnswerStatus::Answer503, None);
            bail!("Maximum connection attempt reached");
        }
        Ok(())
    }

    fn check_backend_connection(&mut self) -> bool {
        let is_valid_backend_socket = match self.state {
            State::Invalid => unreachable!(),
            State::Http(ref http) => http.is_valid_backend_socket(),
            _ => false,
        };

        if is_valid_backend_socket {
            //matched on keepalive
            self.metrics.backend_id = self.backend.as_ref().map(|i| i.borrow().backend_id.clone());
            self.metrics.backend_start();

            let backend = match self.state {
                State::Invalid => unreachable!(),
                State::Http(ref mut http) => http.backend_data.as_mut(),
                _ => None,
            };
            backend.map(|backend| backend.borrow_mut().active_requests += 1);
            return true;
        }
        false
    }

    pub fn extract_route(&self) -> anyhow::Result<(&str, &str, &Method)> {
        let request_state = match self.state {
            State::Invalid => unreachable!(),
            State::Http(ref http) => http.request_state.as_ref(),
            _ => None,
        };

        let given_host = request_state
            .and_then(|request_state| request_state.get_host())
            .with_context(|| "No host given")?;

        let (remaining_input, (hostname, port)) = match hostname_and_port(given_host.as_bytes()) {
            Ok(tuple) => tuple,
            Err(parse_error) => {
                bail!(
                    "Hostname parsing failed for host {}: {}",
                    given_host.clone(),
                    parse_error,
                );
            }
        };

        if remaining_input != &b""[..] {
            bail!("invalid remaining chars after hostname {}", given_host);
        }

        let hostname = unsafe { from_utf8_unchecked(hostname) };

        //FIXME: what if we don't use SNI?
        let servername = match self.state {
            State::Invalid => unreachable!(),
            State::Http(ref http) => http.frontend.session.sni_hostname(),
            _ => None,
        };
        let servername = servername.map(|name| name.to_string());

        if servername.as_deref() != Some(hostname) {
            error!(
                "TLS SNI hostname '{:?}' and Host header '{}' don't match",
                servername, hostname
            );
            /*FIXME: deactivate this check for a temporary test
            unwrap_msg!(session.http()).set_answer(DefaultAnswerStatus::Answer404, None);
            */
            bail!("Host not found: {}", hostname);
        }

        let host = if port == Some(&b"443"[..]) {
            hostname
        } else {
            given_host
        };

        let request_line = request_state
            .as_ref()
            .and_then(|r| r.get_request_line())
            .with_context(|| "No request line given in the request state")?;

        Ok((host, &request_line.uri, &request_line.method))
    }

    pub fn backend_from_request(
        &mut self,
        cluster_id: &str,
        front_should_stick: bool,
    ) -> anyhow::Result<MioTcpStream> {
        let request_state = match self.state {
            State::Invalid => unreachable!(),
            State::Http(ref mut http) => http.request_state.as_ref(),
            _ => None,
        };

        let sticky_session =
            request_state.and_then(|request_state| request_state.get_sticky_session());

        let result = match (front_should_stick, sticky_session) {
            (true, Some(sticky_session)) => self
                .proxy
                .borrow()
                .backends
                .borrow_mut()
                .backend_from_sticky_session(cluster_id, sticky_session)
                .map_err(|e| {
                    debug!(
                        "Couldn't find a backend corresponding to sticky_session {} for cluster {}",
                        sticky_session, cluster_id
                    );
                    e
                }),
            _ => self
                .proxy
                .borrow()
                .backends
                .borrow_mut()
                .backend_from_cluster_id(cluster_id),
        };

        let (backend, conn) = match result {
            Err(e) => {
                self.set_answer(DefaultAnswerStatus::Answer503, None);
                return Err(e).with_context(|| {
                    format!("Could not find a backend for cluster {}", cluster_id)
                });
            }
            Ok((backend, conn)) => (backend, conn),
        };

        if front_should_stick {
            let sticky_name = self.proxy.borrow().listeners[&self.listener_token]
                .borrow()
                .config
                .sticky_name
                .clone();
            let sticky_session = Some(StickySession::new(
                backend
                    .borrow()
                    .sticky_id
                    .clone()
                    .unwrap_or(backend.borrow().backend_id.clone()),
            ));

            match self.state {
                State::Invalid => unreachable!(),
                State::Http(ref mut http) => {
                    http.sticky_session = sticky_session;
                    http.sticky_name = sticky_name;
                }
                _ => {}
            };
        }
        self.metrics.backend_id = Some(backend.borrow().backend_id.clone());
        self.metrics.backend_start();

        match self.state {
            State::Invalid => unreachable!(),
            State::Http(ref mut http) => http.set_backend_id(backend.borrow().backend_id.clone()),
            _ => {}
        }
        self.backend = Some(backend);

        Ok(conn)
    }

    fn cluster_id_from_request(&mut self) -> anyhow::Result<String> {
        let (host, uri, method) = match self.extract_route() {
            Ok(tuple) => tuple,
            Err(e) => {
                self.set_answer(DefaultAnswerStatus::Answer400, None);
                return Err(e).with_context(|| "Could not extract route from request");
            }
        };

        let cluster_id_res = self
            .proxy
            .borrow()
            .listeners
            .get(&self.listener_token)
            .with_context(|| "No listener found for this request")?
            .as_ref()
            .borrow()
            .frontend_from_request(host, uri, method);

        match cluster_id_res {
            Ok(route) => match route {
                Route::ClusterId(cluster_id) => Ok(cluster_id),
                Route::Deny => {
                    self.set_answer(DefaultAnswerStatus::Answer401, None);
                    bail!("Route is unauthorized");
                }
            },
            Err(e) => {
                let no_host_error = format!("Host not found: {}: {:#}", host, e);
                self.set_answer(DefaultAnswerStatus::Answer404, None);
                bail!(no_host_error);
            }
        }
    }

    fn connect_to_backend(
        &mut self,
        session_rc: Rc<RefCell<dyn ProxySession>>,
    ) -> anyhow::Result<BackendConnectAction> {
        let old_cluster_id = match &self.state {
            State::Invalid => unreachable!(),
            State::Http(http) => http.cluster_id.clone(),
            _ => None,
        };
        let old_back_token = self.back_token();

        self.check_circuit_breaker()
            .with_context(|| "Circuit break.")?;

        let requested_cluster_id = self
            .cluster_id_from_request()
            .with_context(|| "Could not get cluster id from request")?;

        let backend_is_connected = self.back_connected == BackendConnectionStatus::Connected;

        if old_cluster_id.as_ref() == Some(&requested_cluster_id) && backend_is_connected {
            let has_backend = self
                .backend
                .as_ref()
                .map(|backend| {
                    let backend = &(*backend.borrow());
                    self.proxy
                        .borrow()
                        .backends
                        .borrow()
                        .has_backend(&requested_cluster_id, backend)
                })
                .unwrap_or(false);

            if has_backend && self.check_backend_connection() {
                return Ok(BackendConnectAction::Reuse);
            } else if let Some(token) = self.back_token() {
                self.close_backend();

                //reset the back token here so we can remove it
                //from the slab after backend_from* fails
                self.set_back_token(token);
            }
        }

        //replacing with a connection to another cluster
        if old_cluster_id.is_some() && old_cluster_id.as_ref() != Some(&requested_cluster_id) {
            if let Some(token) = self.back_token() {
                self.close_backend();

                //reset the back token here so we can remove it
                //from the slab after backend_from* fails
                self.set_back_token(token);
            }
        }

        self.cluster_id = Some(requested_cluster_id.clone());
        match &mut self.state {
            State::Invalid => unreachable!(),
            State::Http(http) => http.cluster_id = Some(requested_cluster_id.clone()),
            _ => {}
        }

        let front_should_stick = self
            .proxy
            .borrow()
            .clusters
            .get(&requested_cluster_id)
            .map(|cluster| cluster.sticky_session)
            .unwrap_or(false);
        let mut socket = self
            .backend_from_request(&requested_cluster_id, front_should_stick)
            .with_context(|| "Could not get TCP stream from backend")?;

        // we still want to use the new socket
        if let Err(e) = socket.set_nodelay(true) {
            error!(
                "error setting nodelay on back socket({:?}): {:?}",
                socket, e
            );
        }
        if let Some(r) = self.back_readiness() {
            r.interest = Ready::writable() | Ready::hup() | Ready::error();
        }

        let connect_timeout = Duration::seconds(i64::from(
            self.proxy
                .borrow()
                .listeners
                .get(&self.listener_token)
                .as_ref()
                .map(|listener| listener.borrow().config.connect_timeout)
                .unwrap(),
        ));

        self.back_connected = BackendConnectionStatus::Connecting(Instant::now());

        let connect_action = match old_back_token {
            Some(back_token) => {
                self.set_back_token(back_token);
                if let Err(e) = self.proxy.borrow().registry.register(
                    &mut socket,
                    back_token,
                    Interest::READABLE | Interest::WRITABLE,
                ) {
                    error!("error registering back socket({:?}): {:?}", socket, e);
                }

                BackendConnectAction::Replace
            }
            None => {
                if self.proxy.borrow().sessions.borrow().slab.len()
                    >= self.proxy.borrow().sessions.borrow().slab_capacity()
                {
                    error!("not enough memory, cannot connect to backend");
                    self.set_answer(DefaultAnswerStatus::Answer503, None);
                    bail!(format!(
                        "Too many connections for cluster {:?}",
                        self.cluster_id
                    ));
                }

                let back_token = {
                    let proxy = self.proxy.borrow();
                    let mut session_manager = proxy.sessions.borrow_mut();
                    let entry = session_manager.slab.vacant_entry();
                    let back_token = Token(entry.key());
                    let _entry = entry.insert(session_rc.clone());
                    back_token
                };

                if let Err(e) = self.proxy.borrow().registry.register(
                    &mut socket,
                    back_token,
                    Interest::READABLE | Interest::WRITABLE,
                ) {
                    error!("error registering back socket({:?}): {:?}", socket, e);
                }

                self.set_back_token(back_token);
                BackendConnectAction::New
            }
        };

        self.set_back_socket(socket);
        match &mut self.state {
            State::Invalid => unreachable!(),
            State::Http(http) => http.set_back_timeout(connect_timeout),
            _ => {}
        }
        Ok(connect_action)
    }
}

impl ProxySession for Session {
    fn close(&mut self) {
        //println!("TLS closing[{:?}] temp->front: {:?}, temp->back: {:?}", self.frontend_token, *self.temp.front_buf, *self.temp.back_buf);
        match &mut self.state {
            State::Invalid => unreachable!(),
            State::Http(http) => http.close(),
            _ => (),
        }

        self.metrics.service_stop();
        self.cancel_timeouts();
        if let Some(front_socket) = self.front_socket() {
            if let Err(e) = front_socket.shutdown(Shutdown::Both) {
                if e.kind() != ErrorKind::NotConnected {
                    error!("error closing front socket({:?}): {:?}", front_socket, e);
                }
            }
        }

        self.close_backend();

        match self.state {
            State::Invalid => unreachable!(),
            State::Http(ref mut http) => {
                //if the state was initial, the connection was already reset
                if http.request_state != Some(RequestState::Initial) {
                    gauge_add!("http.active_requests", -1);

                    if let Some(b) = http.backend_data.as_mut() {
                        let mut backend = b.borrow_mut();
                        backend.active_requests = backend.active_requests.saturating_sub(1);
                    }
                }
            }
            State::WebSocket(_) => {
                if let Some(backend) = self.backend.as_mut() {
                    let mut backend = backend.borrow_mut();
                    backend.active_requests = backend.active_requests.saturating_sub(1);
                }
            }
            State::Expect(_, _) => gauge_add!("protocol.proxy.expect", -1),
            State::Handshake(_) => gauge_add!("protocol.tls.handshake", -1),
            State::Http2(_) => todo!(),
        }

        if let Some(fd) = self.front_socket().map(|stream| stream.as_raw_fd()) {
            let proxy = self.proxy.borrow();
            if let Err(e) = proxy.registry.deregister(&mut SourceFd(&fd)) {
                error!("1error deregistering socket({:?}): {:?}", fd, e);
            }
            proxy
                .sessions
                .borrow_mut()
                .slab
                .try_remove(self.frontend_token.0);
        }
    }

    fn timeout(&mut self, token: Token) {
        let session_result = match self.state {
            State::Invalid => unreachable!(),
            State::Expect(_, _) => {
                if token == self.frontend_token {
                    self.front_timeout.triggered();
                }
                SessionResult::CloseSession
            }
            State::Handshake(_) => {
                if token == self.frontend_token {
                    self.front_timeout.triggered();
                }
                SessionResult::CloseSession
            }
            State::WebSocket(ref mut pipe) => pipe.timeout(token, &mut self.metrics),
            State::Http(ref mut http) => http.timeout(token, &mut self.metrics),
            State::Http2(_) => todo!(),
        };

        if session_result == SessionResult::CloseSession {
            self.close();
        }
    }

    fn protocol(&self) -> Protocol {
        Protocol::HTTPS
    }

    // TODO: rename to update_readiness
    fn process_events(&mut self, token: Token, events: Ready) {
        trace!(
            "token {:?} got event {}",
            token,
            super::ready_to_string(events)
        );
        self.last_event = Instant::now();
        self.metrics.wait_start();

        if self.frontend_token == token {
            self.front_readiness().event = self.front_readiness().event | events;
        } else if self.back_token() == Some(token) {
            self.back_readiness().map(|r| r.event |= events);
        }
    }

    fn ready(&mut self, session: Rc<RefCell<dyn ProxySession>>) {
        self.metrics().service_start();

        match self.ready_inner(session) {
            SessionResult::CloseSession => self.close(),
            SessionResult::CloseBackend => self.close_backend(),
            _ => {}
        }

        self.metrics().service_stop();
    }

    fn shutting_down(&mut self) {
        let session_result = match self.state {
            State::Invalid => unreachable!(),
            State::Http(ref mut http) => http.shutting_down(),
            State::Handshake(_) => SessionResult::Continue,
            _ => SessionResult::CloseSession,
        };

        if session_result == SessionResult::CloseSession {
            self.close();
            self.proxy
                .borrow()
                .sessions
                .borrow_mut()
                .slab
                .try_remove(self.frontend_token.0);
        }
    }

    fn last_event(&self) -> Instant {
        self.last_event
    }

    fn print_state(&self) {
        let protocol: String = match &self.state {
            State::Invalid => unreachable!(),
            State::Expect(_, _) => String::from("Expect"),
            State::Handshake(_) => String::from("Handshake"),
            State::Http(h) => h.print_state("HTTPS"),
            State::Http2(_) => todo!(),
            State::WebSocket(_) => String::from("WSS"),
        };

        let front_readiness = match self.state {
            State::Invalid => unreachable!(),
            State::Expect(ref expect, _) => &expect.readiness,
            State::Handshake(ref handshake) => &handshake.readiness,
            State::Http(ref http) => &http.front_readiness,
            State::Http2(_) => todo!(),
            State::WebSocket(ref pipe) => &pipe.front_readiness,
        };
        let back_readiness = match self.state {
            State::Invalid => unreachable!(),
            State::Http(ref http) => Some(&http.back_readiness),
            State::Http2(_) => todo!(),
            State::WebSocket(ref pipe) => Some(&pipe.back_readiness),
            _ => None,
        };

        error!(
            "zombie session[{:?} => {:?}], state => readiness: {:?} -> {:?}, protocol: {}, cluster_id: {:?}, back_connected: {:?}, metrics: {:?}",
            self.frontend_token, self.back_token(), front_readiness, back_readiness, protocol, self.cluster_id, self.back_connected, self.metrics
        );
    }

    fn tokens(&self) -> Vec<Token> {
        let mut tokens = vec![self.frontend_token];
        if let Some(token) = self.back_token() {
            tokens.push(token)
        }
        tokens
    }
}

pub type HostName = String;
pub type PathBegin = String;

#[derive(thiserror::Error, Debug)]
pub enum ListenerError {
    #[error("failed to acquire the lock, {0}")]
    LockError(String),
    #[error("failed to handle certificate request, got a resolver error, {0}")]
    ResolverError(GenericCertificateResolverError),
    #[error("failed to parse pem, {0}")]
    PemParseError(String),
    #[error("failed to build rustls context, {0}")]
    BuildRustlsError(String),
}

pub struct Listener {
    listener: Option<MioTcpListener>,
    address: StdSocketAddr,
    fronts: Router,
    answers: Rc<RefCell<HttpAnswers>>,
    config: HttpsListener,
    resolver: Arc<MutexWrappedCertificateResolver>,
    // resolver: Arc<Mutex<GenericCertificateResolver>>,
    rustls_details: Arc<ServerConfig>,
    pub token: Token,
    active: bool,
    tags: BTreeMap<String, BTreeMap<String, String>>,
}

impl ListenerHandler for Listener {
    fn get_addr(&self) -> &StdSocketAddr {
        &self.address
    }

    fn get_tags(&self, key: &str) -> Option<&BTreeMap<String, String>> {
        self.tags.get(key)
    }

    fn set_tags(&mut self, key: String, tags: Option<BTreeMap<String, String>>) {
        match tags {
            Some(tags) => self.tags.insert(key, tags),
            None => self.tags.remove(&key),
        };
    }
}

impl CertificateResolver for Listener {
    type Error = ListenerError;

    fn get_certificate(
        &self,
        fingerprint: &CertificateFingerprint,
    ) -> Option<ParsedCertificateAndKey> {
        let resolver = self
            .resolver
            .0
            .lock()
            .map_err(|err| ListenerError::LockError(err.to_string()))
            .ok()?;

        resolver.get_certificate(fingerprint)
    }

    fn add_certificate(
        &mut self,
        opts: &sozu_command_lib::proxy::AddCertificate,
    ) -> Result<CertificateFingerprint, Self::Error> {
        let mut resolver = self
            .resolver
            .0
            .lock()
            .map_err(|err| ListenerError::LockError(err.to_string()))?;

        resolver
            .add_certificate(opts)
            .map_err(ListenerError::ResolverError)
    }

    fn remove_certificate(
        &mut self,
        opts: &sozu_command_lib::proxy::RemoveCertificate,
    ) -> Result<(), Self::Error> {
        let mut resolver = self
            .resolver
            .0
            .lock()
            .map_err(|err| ListenerError::LockError(err.to_string()))?;

        resolver
            .remove_certificate(opts)
            .map_err(ListenerError::ResolverError)
    }
}

impl Listener {
    pub fn try_new(config: HttpsListener, token: Token) -> Result<Listener, ListenerError> {
        let resolver = Arc::new(MutexWrappedCertificateResolver::new());

        let server_config = Arc::new(Self::create_rustls_context(&config, resolver.to_owned())?);

        Ok(Listener {
            listener: None,
            address: config.address,
            resolver,
            rustls_details: server_config,
            active: false,
            fronts: Router::new(),
            answers: Rc::new(RefCell::new(HttpAnswers::new(
                &config.answer_404,
                &config.answer_503,
            ))),
            config,
            token,
            tags: BTreeMap::new(),
        })
    }

    pub fn activate(
        &mut self,
        registry: &Registry,
        tcp_listener: Option<MioTcpListener>,
    ) -> anyhow::Result<Token> {
        if self.active {
            return Ok(self.token);
        }

        let mut listener = match tcp_listener {
            Some(tcp_listener) => tcp_listener,
            None => server_bind(self.config.address)
                .with_context(|| format!("could not create listener {:?}", self.config.address))?,
        };

        registry
            .register(&mut listener, self.token, Interest::READABLE)
            .with_context(|| format!("Could not register listener socket {:?}", listener))?;

        self.listener = Some(listener);
        self.active = true;
        Ok(self.token)
    }

    pub fn create_rustls_context(
        config: &HttpsListener,
        resolver: Arc<MutexWrappedCertificateResolver>,
    ) -> Result<ServerConfig, ListenerError> {
        let cipher_names = if config.cipher_list.is_empty() {
            DEFAULT_CIPHER_SUITES.to_vec()
        } else {
            config
                .cipher_list
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
        };

        #[cfg_attr(rustfmt, rustfmt_skip)]
        let ciphers = cipher_names
            .into_iter()
            .filter_map(|cipher| match cipher {
                "TLS13_CHACHA20_POLY1305_SHA256" => Some(TLS13_CHACHA20_POLY1305_SHA256),
                "TLS13_AES_256_GCM_SHA384" => Some(TLS13_AES_256_GCM_SHA384),
                "TLS13_AES_128_GCM_SHA256" => Some(TLS13_AES_128_GCM_SHA256),
                "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256" => Some(TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256),
                "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256" => Some(TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256),
                "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384" => Some(TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384),
                "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256" => Some(TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256),
                "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384" => Some(TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384),
                "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256" => Some(TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256),
                other_cipher => {
                    error!("unknown cipher: {:?}", other_cipher);
                    None
                }
            })
            .collect::<Vec<_>>();

        let versions = config
            .versions
            .iter()
            .filter_map(|version| match version {
                TlsVersion::TLSv1_2 => Some(&rustls::version::TLS12),
                TlsVersion::TLSv1_3 => Some(&rustls::version::TLS13),
                other_version => {
                    error!("unsupported TLS version: {:?}", other_version);
                    None
                }
            })
            .collect::<Vec<_>>();

        let mut server_config = ServerConfig::builder()
            .with_cipher_suites(&ciphers[..])
            .with_safe_default_kx_groups()
            .with_protocol_versions(&versions[..])
            .map_err(|err| ListenerError::BuildRustlsError(err.to_string()))?
            .with_no_client_auth()
            .with_cert_resolver(resolver);

        let mut protocols = SERVER_PROTOS
            .iter()
            .map(|proto| proto.as_bytes().to_vec())
            .collect::<Vec<_>>();
        server_config.alpn_protocols.append(&mut protocols);

        Ok(server_config)
    }

    pub fn add_https_front(&mut self, tls_front: HttpFrontend) -> anyhow::Result<()> {
        self.fronts
            .add_http_front(&tls_front)
            .with_context(|| "Could not add https frontend")
    }

    pub fn remove_https_front(&mut self, tls_front: HttpFrontend) -> anyhow::Result<()> {
        debug!("removing tls_front {:?}", tls_front);
        self.fronts
            .remove_http_front(&tls_front)
            .with_context(|| "Could not remove https frontend")
    }

    // TODO factor out with http.rs
    pub fn frontend_from_request(
        &self,
        host: &str,
        uri: &str,
        method: &Method,
    ) -> anyhow::Result<Route> {
        let (remaining_input, (hostname, _)) = match hostname_and_port(host.as_bytes()) {
            Ok(tuple) => tuple,
            Err(parse_error) => {
                // parse_error contains a slice of given_host, which should NOT escape this scope
                bail!(
                    "Hostname parsing failed for host {}: {}",
                    host.clone(),
                    parse_error,
                );
            }
        };

        if remaining_input != &b""[..] {
            bail!(
                "frontend_from_request: invalid remaining chars after hostname. Host: {}",
                host
            );
        }

        // it is alright to call from_utf8_unchecked,
        // we already verified that there are only ascii
        // chars in there
        let host = unsafe { from_utf8_unchecked(hostname) };

        self.fronts
            .lookup(host.as_bytes(), uri.as_bytes(), method)
            .with_context(|| "No cluster found")
    }

    fn accept(&mut self) -> Result<MioTcpStream, AcceptError> {
        if let Some(ref sock) = self.listener {
            sock.accept()
                .map_err(|e| match e.kind() {
                    ErrorKind::WouldBlock => AcceptError::WouldBlock,
                    _ => {
                        error!("accept() IO error: {:?}", e);
                        AcceptError::IoError
                    }
                })
                .map(|(sock, _)| sock)
        } else {
            error!("cannot accept connections, no listening socket available");
            Err(AcceptError::IoError)
        }
    }
}

pub struct Proxy {
    listeners: HashMap<Token, Rc<RefCell<Listener>>>,
    clusters: HashMap<ClusterId, Cluster>,
    backends: Rc<RefCell<BackendMap>>,
    pool: Rc<RefCell<Pool>>,
    registry: Registry,
    sessions: Rc<RefCell<SessionManager>>,
}

impl Proxy {
    pub fn new(
        registry: Registry,
        sessions: Rc<RefCell<SessionManager>>,
        pool: Rc<RefCell<Pool>>,
        backends: Rc<RefCell<BackendMap>>,
    ) -> Proxy {
        Proxy {
            listeners: HashMap::new(),
            clusters: HashMap::new(),
            backends,
            pool,
            registry,
            sessions,
        }
    }

    pub fn add_listener(&mut self, config: HttpsListener, token: Token) -> Option<Token> {
        match self.listeners.entry(token) {
            Entry::Vacant(entry) => {
                entry.insert(Rc::new(RefCell::new(
                    Listener::try_new(config, token).ok()?,
                )));
                Some(token)
            }
            _ => None,
        }
    }

    pub fn remove_listener(
        &mut self,
        remove: RemoveListener,
    ) -> anyhow::Result<Option<ProxyResponseContent>> {
        let len = self.listeners.len();

        self.listeners
            .retain(|_, listener| listener.borrow().address != remove.address);

        if !self.listeners.len() < len {
            info!(
                "no HTTPS listener to remove at address {:?}",
                remove.address
            )
        }
        Ok(None)
    }

    pub fn soft_stop(&mut self) -> anyhow::Result<()> {
        let listeners: HashMap<_, _> = self.listeners.drain().collect();
        let mut socket_errors = vec![];
        for (_, l) in listeners.iter() {
            if let Some(mut sock) = l.borrow_mut().listener.take() {
                debug!("Deregistering socket {:?}", sock);
                if let Err(e) = self.registry.deregister(&mut sock) {
                    let error = format!("socket {:?}: {:?}", sock, e);
                    socket_errors.push(error);
                }
            }
        }

        if !socket_errors.is_empty() {
            bail!("Error deregistering listen sockets: {:?}", socket_errors);
        }

        Ok(())
    }

    pub fn hard_stop(&mut self) -> anyhow::Result<()> {
        let mut listeners: HashMap<_, _> = self.listeners.drain().collect();
        let mut socket_errors = vec![];
        for (_, l) in listeners.drain() {
            if let Some(mut sock) = l.borrow_mut().listener.take() {
                debug!("Deregistering socket {:?}", sock);
                if let Err(e) = self.registry.deregister(&mut sock) {
                    let error = format!("socket {:?}: {:?}", sock, e);
                    socket_errors.push(error);
                }
            }
        }

        if !socket_errors.is_empty() {
            bail!("Error deregistering listen sockets: {:?}", socket_errors);
        }

        Ok(())
    }

    pub fn logging(
        &mut self,
        logging_filter: String,
    ) -> anyhow::Result<Option<ProxyResponseContent>> {
        logging::LOGGER.with(|l| {
            let directives = logging::parse_logging_spec(&logging_filter);
            l.borrow_mut().set_directives(directives);
        });
        Ok(None)
    }

    pub fn query_all_certificates(&mut self) -> anyhow::Result<Option<ProxyResponseContent>> {
        let certificates = self
            .listeners
            .values()
            .map(|listener| {
                let owned = listener.borrow();
                let resolver = unwrap_msg!(owned.resolver.0.lock());
                let res = resolver
                    .domains
                    .to_hashmap()
                    .drain()
                    .map(|(k, v)| (String::from_utf8(k).unwrap(), v.0))
                    .collect();

                (owned.address, res)
            })
            .collect::<HashMap<_, _>>();

        info!(
            "got Certificates::All query, answering with {:?}",
            certificates
        );

        Ok(Some(ProxyResponseContent::Query(
            QueryAnswer::Certificates(QueryAnswerCertificate::All(certificates)),
        )))
    }

    pub fn query_certificate_for_domain(
        &mut self,
        domain: String,
    ) -> anyhow::Result<Option<ProxyResponseContent>> {
        let certificates = self
            .listeners
            .values()
            .map(|listener| {
                let owned = listener.borrow();
                let resolver = unwrap_msg!(owned.resolver.0.lock());
                (
                    owned.address,
                    resolver
                        .domain_lookup(domain.as_bytes(), true)
                        .map(|(k, v)| (String::from_utf8(k.to_vec()).unwrap(), v.0.clone())),
                )
            })
            .collect::<HashMap<_, _>>();

        info!(
            "got Certificates::Domain({}) query, answering with {:?}",
            domain, certificates
        );

        Ok(Some(ProxyResponseContent::Query(
            QueryAnswer::Certificates(QueryAnswerCertificate::Domain(certificates)),
        )))
    }

    pub fn activate_listener(
        &mut self,
        addr: &StdSocketAddr,
        tcp_listener: Option<MioTcpListener>,
    ) -> anyhow::Result<Token> {
        let listener = self
            .listeners
            .values()
            .find(|listener| listener.borrow().address == *addr)
            .with_context(|| format!("No listener found for address {}", addr))?;

        listener
            .borrow_mut()
            .activate(&self.registry, tcp_listener)
            .with_context(|| "Failed to activate listener")
    }

    pub fn give_back_listeners(&mut self) -> Vec<(StdSocketAddr, MioTcpListener)> {
        self.listeners
            .values()
            .filter_map(|listener| {
                let mut owned = listener.borrow_mut();
                if let Some(listener) = owned.listener.take() {
                    return Some((owned.address, listener));
                }

                None
            })
            .collect()
    }

    // TODO: return Result with context
    pub fn give_back_listener(
        &mut self,
        address: StdSocketAddr,
    ) -> Option<(Token, MioTcpListener)> {
        self.listeners
            .values()
            .find(|listener| listener.borrow().address == address)
            .and_then(|listener| {
                let mut owned = listener.borrow_mut();

                owned
                    .listener
                    .take()
                    .map(|listener| (owned.token, listener))
            })
    }

    pub fn add_cluster(
        &mut self,
        mut cluster: Cluster,
    ) -> anyhow::Result<Option<ProxyResponseContent>> {
        if let Some(answer_503) = cluster.answer_503.take() {
            for listener in self.listeners.values() {
                listener
                    .borrow()
                    .answers
                    .borrow_mut()
                    .add_custom_answer(&cluster.cluster_id, &answer_503);
            }
        }
        self.clusters.insert(cluster.cluster_id.clone(), cluster);
        Ok(None)
    }

    pub fn remove_cluster(
        &mut self,
        cluster_id: &str,
    ) -> anyhow::Result<Option<ProxyResponseContent>> {
        self.clusters.remove(cluster_id);
        for listener in self.listeners.values() {
            listener
                .borrow()
                .answers
                .borrow_mut()
                .remove_custom_answer(cluster_id);
        }

        Ok(None)
    }

    pub fn add_https_frontend(
        &mut self,
        front: HttpFrontend,
    ) -> anyhow::Result<Option<ProxyResponseContent>> {
        match self
            .listeners
            .values()
            .find(|l| l.borrow().address == front.address)
        {
            Some(listener) => {
                let mut owned = listener.borrow_mut();
                owned.set_tags(front.hostname.to_owned(), front.tags.to_owned());
                owned
                    .add_https_front(front)
                    .with_context(|| "Could not add https frontend")?;
                Ok(None)
            }
            None => bail!("adding front {:?} to unknown listener", front),
        }
    }

    pub fn remove_https_frontend(
        &mut self,
        front: HttpFrontend,
    ) -> anyhow::Result<Option<ProxyResponseContent>> {
        if let Some(listener) = self
            .listeners
            .values()
            .find(|l| l.borrow_mut().address == front.address)
        {
            let mut owned = listener.borrow_mut();
            owned.set_tags(front.hostname.to_owned(), None);
            owned
                .remove_https_front(front)
                .with_context(|| "Could not remove https frontend")?;
        }
        Ok(None)
    }

    pub fn add_certificate(
        &mut self,
        add_certificate: AddCertificate,
    ) -> anyhow::Result<Option<ProxyResponseContent>> {
        match self
            .listeners
            .values()
            .find(|l| l.borrow().address == add_certificate.address)
        {
            Some(listener) => listener
                .borrow_mut()
                .add_certificate(&add_certificate)
                .with_context(|| "Could not add certificate to listener")?,
            None => bail!(
                "adding certificate to unknown listener {}",
                add_certificate.address
            ),
        };

        Ok(None)
    }

    //FIXME: should return an error if certificate still has fronts referencing it
    pub fn remove_certificate(
        &mut self,
        remove_certificate: RemoveCertificate,
    ) -> anyhow::Result<Option<ProxyResponseContent>> {
        match self
            .listeners
            .values()
            .find(|l| l.borrow().address == remove_certificate.address)
        {
            Some(listener) => listener
                .borrow_mut()
                .remove_certificate(&remove_certificate)
                .with_context(|| "Could not remove certificate from listener")?,
            None => bail!(
                "removing certificate from unknown listener {}",
                remove_certificate.address
            ),
        };

        Ok(None)
    }

    //FIXME: should return an error if certificate still has fronts referencing it
    pub fn replace_certificate(
        &mut self,
        replace_certificate: ReplaceCertificate,
    ) -> anyhow::Result<Option<ProxyResponseContent>> {
        match self
            .listeners
            .values()
            .find(|l| l.borrow().address == replace_certificate.address)
        {
            Some(listener) => listener
                .borrow_mut()
                .replace_certificate(&replace_certificate)
                .with_context(|| "Could not replace certificate on listener")?,
            None => bail!(
                "replacing certificate on unknown listener {}",
                replace_certificate.address
            ),
        };

        Ok(None)
    }
}

impl ProxyConfiguration<Session> for Proxy {
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
            .ok_or_else(|| AcceptError::IoError)?;
        if let Err(e) = frontend_sock.set_nodelay(true) {
            error!(
                "error setting nodelay on front socket({:?}): {:?}",
                frontend_sock, e
            );
        }

        let owned = listener.borrow();
        let rustls_details = ServerConnection::new(owned.rustls_details.clone()).map_err(|e| {
            error!("failed to create server session: {:?}", e);
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
                    "error registering front socket({:?}): {:?}",
                    frontend_sock, register_error
                );
                AcceptError::RegisterError
            })?;

        let session = Rc::new(RefCell::new(Session::new(
            rustls_details,
            frontend_sock,
            session_token,
            Rc::downgrade(&self.pool),
            proxy,
            owned.config.public_address.unwrap_or(owned.config.address),
            owned.config.expect_proxy,
            owned.config.sticky_name.clone(),
            owned.answers.clone(),
            Token(token.0),
            wait_time,
            Duration::seconds(owned.config.front_timeout as i64),
            Duration::seconds(owned.config.back_timeout as i64),
            Duration::seconds(owned.config.request_timeout as i64),
            listener.clone(),
        )));
        entry.insert(session);

        session_manager.incr();
        Ok(())
    }

    fn notify(&mut self, request: ProxyRequest) -> ProxyResponse {
        let request_id = request.id.clone();

        let content_result = match request.order {
            ProxyRequestOrder::AddCluster(cluster) => {
                info!("{} add cluster {:?}", request_id, cluster);
                self.add_cluster(cluster.clone())
                    .with_context(|| format!("Could not add cluster {}", cluster.cluster_id))
            }
            ProxyRequestOrder::RemoveCluster { cluster_id } => {
                info!("{} remove cluster {:?}", request_id, cluster_id);
                self.remove_cluster(&cluster_id)
                    .with_context(|| format!("Could not remove cluster {}", cluster_id))
            }
            ProxyRequestOrder::AddHttpsFrontend(front) => {
                info!("{} add https front {:?}", request_id, front);
                self.add_https_frontend(front)
                    .with_context(|| "Could not add https frontend")
            }
            ProxyRequestOrder::RemoveHttpsFrontend(front) => {
                info!("{} remove https front {:?}", request_id, front);
                self.remove_https_frontend(front)
                    .with_context(|| "Could not remove https frontend")
            }
            ProxyRequestOrder::AddCertificate(add_certificate) => {
                info!("{} add certificate: {:?}", request_id, add_certificate);
                self.add_certificate(add_certificate)
                    .with_context(|| "Could not add certificate")
            }
            ProxyRequestOrder::RemoveCertificate(remove_certificate) => {
                info!(
                    "{} remove certificate: {:?}",
                    request_id, remove_certificate
                );
                self.remove_certificate(remove_certificate)
                    .with_context(|| "Could not remove certificate")
            }
            ProxyRequestOrder::ReplaceCertificate(replace_certificate) => {
                info!(
                    "{} replace certificate: {:?}",
                    request_id, replace_certificate
                );
                self.replace_certificate(replace_certificate)
                    .with_context(|| "Could not replace certificate")
            }
            ProxyRequestOrder::RemoveListener(remove) => {
                info!("removing HTTPS listener at address {:?}", remove.address);
                self.remove_listener(remove.clone()).with_context(|| {
                    format!("Could not remove listener at address {:?}", remove.address)
                })
            }
            ProxyRequestOrder::SoftStop => {
                info!("{} processing soft shutdown", request_id);
                match self
                    .soft_stop()
                    .with_context(|| "Could not perform soft stop")
                {
                    Ok(_) => {
                        info!("{} soft stop successful", request_id);
                        return ProxyResponse::processing(request.id);
                    }
                    Err(e) => Err(e),
                }
            }
            ProxyRequestOrder::HardStop => {
                info!("{} processing hard shutdown", request_id);
                match self
                    .hard_stop()
                    .with_context(|| "Could not perform hard stop")
                {
                    Ok(_) => {
                        info!("{} hard stop successful", request_id);
                        return ProxyResponse::processing(request.id);
                    }
                    Err(e) => Err(e),
                }
            }
            ProxyRequestOrder::Status => {
                info!("{} status", request_id);
                Ok(None)
            }
            ProxyRequestOrder::Logging(logging_filter) => {
                info!(
                    "{} changing logging filter to {}",
                    request_id, logging_filter
                );
                self.logging(logging_filter.clone())
                    .with_context(|| format!("Could not set logging level to {}", logging_filter))
            }
            ProxyRequestOrder::Query(Query::Certificates(QueryCertificateType::All)) => {
                info!("{} query all certificates", request_id);
                self.query_all_certificates()
                    .with_context(|| "Could not query all certificates")
            }
            ProxyRequestOrder::Query(Query::Certificates(QueryCertificateType::Domain(domain))) => {
                info!("{} query certificate for domain {}", request_id, domain);
                self.query_certificate_for_domain(domain.clone())
                    .with_context(|| format!("Could not query certificate for domain {}", domain))
            }
            other_order => {
                error!(
                    "{} unsupported order for HTTPS proxy, ignoring {:?}",
                    request.id, other_order
                );
                Err(anyhow::Error::msg("unsupported message"))
            }
        };

        match content_result {
            Ok(content) => {
                info!("{} successful", request_id);
                ProxyResponse {
                    id: request_id,
                    status: ProxyResponseStatus::Ok,
                    content,
                }
            }
            Err(error_message) => {
                error!("{} unsuccessful: {:#}", request_id, error_message);
                ProxyResponse::error(request_id, format!("{:#}", error_message))
            }
        }
    }
}

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
    }
}

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

/// this function is not used, but is available for example and testing purposes
pub fn start(
    config: HttpsListener,
    channel: ProxyChannel,
    max_buffers: usize,
    buffer_size: usize,
) -> anyhow::Result<()> {
    use crate::server;

    let event_loop = Poll::new().with_context(|| "could not create event loop")?;

    let pool = Rc::new(RefCell::new(Pool::with_capacity(
        1,
        max_buffers,
        buffer_size,
    )));
    let backends = Rc::new(RefCell::new(BackendMap::new()));

    let mut sessions: Slab<Rc<RefCell<dyn ProxySession>>> = Slab::with_capacity(max_buffers);
    {
        let entry = sessions.vacant_entry();
        info!("taking token {:?} for channel", SessionToken(entry.key()));
        entry.insert(Rc::new(RefCell::new(ListenSession {
            protocol: Protocol::HTTPListen,
        })));
    }
    {
        let entry = sessions.vacant_entry();
        info!("taking token {:?} for timer", SessionToken(entry.key()));
        entry.insert(Rc::new(RefCell::new(ListenSession {
            protocol: Protocol::HTTPListen,
        })));
    }
    {
        let entry = sessions.vacant_entry();
        info!("taking token {:?} for metrics", SessionToken(entry.key()));
        entry.insert(Rc::new(RefCell::new(ListenSession {
            protocol: Protocol::HTTPListen,
        })));
    }

    let token = {
        let entry = sessions.vacant_entry();
        let key = entry.key();
        let _e = entry.insert(Rc::new(RefCell::new(ListenSession {
            protocol: Protocol::HTTPListen,
        })));
        Token(key)
    };

    let sessions = SessionManager::new(sessions, max_buffers);
    let registry = event_loop
        .registry()
        .try_clone()
        .with_context(|| "Failed at creating a registry")?;
    let mut proxy = Proxy::new(registry, sessions.clone(), pool.clone(), backends.clone());
    let address = config.address;
    if proxy.add_listener(config, token).is_some()
        && proxy.activate_listener(&address, None).is_ok()
    {
        let (scm_server, _scm_client) =
            UnixStream::pair().with_context(|| "Failed at creating scm stream sockets")?;
        let mut server_config: server::ServerConfig = Default::default();
        server_config.max_connections = max_buffers;
        let mut server = Server::new(
            event_loop,
            channel,
            ScmSocket::new(scm_server.as_raw_fd()).unwrap(),
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
        .with_context(|| "Failed to create server")?;

        info!("starting event loop");
        server.run();
        info!("ending event loop");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, sync::Arc};

    use crate::router::{trie::TrieNode, MethodRule, PathRule, Router};
    use crate::sozu_command::proxy::Route;

    use super::*;

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

        let address: StdSocketAddr = FromStr::from_str("127.0.0.1:1032")
            .expect("test address 127.0.0.1:1032 should be parsed");
        let resolver = Arc::new(MutexWrappedCertificateResolver::new());

        let server_config = ServerConfig::builder()
            .with_safe_default_cipher_suites()
            .with_safe_default_kx_groups()
            .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
            .map_err(|err| ListenerError::BuildRustlsError(err.to_string()))
            .expect("could not create Rustls server config")
            .with_no_client_auth()
            .with_cert_resolver(resolver.clone());

        let rustls_details = Arc::new(server_config);

        let listener = Listener {
            listener: None,
            address,
            fronts,
            rustls_details,
            resolver,
            answers: Rc::new(RefCell::new(HttpAnswers::new(
                "HTTP/1.1 404 Not Found\r\n\r\n",
                "HTTP/1.1 503 Service Unavailable\r\n\r\n",
            ))),
            config: Default::default(),
            token: Token(0),
            active: true,
            tags: BTreeMap::new(),
        };

        println!("TEST {}", line!());
        let frontend1 = listener.frontend_from_request("lolcatho.st", "/", &Method::Get);
        assert_eq!(
            frontend1.expect("should find a frontend"),
            Route::ClusterId("cluster_1".to_string())
        );
        println!("TEST {}", line!());
        let frontend2 = listener.frontend_from_request("lolcatho.st", "/test", &Method::Get);
        assert_eq!(
            frontend2.expect("should find a frontend"),
            Route::ClusterId("cluster_1".to_string())
        );
        println!("TEST {}", line!());
        let frontend3 = listener.frontend_from_request("lolcatho.st", "/yolo/test", &Method::Get);
        assert_eq!(
            frontend3.expect("should find a frontend"),
            Route::ClusterId("cluster_2".to_string())
        );
        println!("TEST {}", line!());
        let frontend4 = listener.frontend_from_request("lolcatho.st", "/yolo/swag", &Method::Get);
        assert_eq!(
            frontend4.expect("should find a frontend"),
            Route::ClusterId("cluster_3".to_string())
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
        println!("query result: {:?}", res);

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
        println!("query result: {:?}", res);

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
        println!("query result: {:?}", res);

        assert_eq!(
            trie.domain_lookup(b"sub.test.example.com", true),
            Some(&("*.test.example.com".as_bytes().to_vec(), 1u8))
        );
        assert_eq!(
            trie.domain_lookup(b"hello.sub.test.example.com", true),
            Some(&("hello.sub.test.example.com".as_bytes().to_vec(), 2u8))
        );
    }
}
