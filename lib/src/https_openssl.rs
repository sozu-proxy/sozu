use std::collections::hash_map::Entry;
#[cfg(feature = "use-openssl")]
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    io::ErrorKind,
    net::{Shutdown, SocketAddr},
    os::unix::io::AsRawFd,
    rc::{Rc, Weak},
    str::from_utf8_unchecked,
    sync::{Arc, Mutex},
};

use anyhow::Context;
use foreign_types_shared::{ForeignType, ForeignTypeRef};
use mio::{net::*, unix::SourceFd, *};
use nom::HexDisplay;
use openssl::{
    dh::Dh,
    error::ErrorStack,
    nid,
    pkey::{PKey, Private},
    ssl::{
        self, select_next_proto, AlpnError, NameType, SniError, Ssl, SslAlert, SslContext,
        SslContextBuilder, SslMethod, SslOptions, SslRef, SslSessionCacheMode, SslStream,
        SslVersion,
    },
    x509::X509,
};
use rusty_ulid::Ulid;
use slab::Slab;
use time::{Duration, Instant};

use crate::{
    backends::BackendMap,
    pool::Pool,
    protocol::{
        h2::Http2,
        http::{
            answers::HttpAnswers,
            parser::{hostname_and_port, Method, RequestLine, RequestState},
            DefaultAnswerStatus,
        },
        openssl::TlsHandshake,
        proxy_protocol::expect::ExpectProxyProtocol,
        Http, Pipe, ProtocolResult, StickySession,
    },
    retry::RetryPolicy,
    router::Router,
    server::{
        push_event, ListenSession, ListenToken, ProxyChannel, Server, SessionManager, SessionToken,
        CONN_RETRIES,
    },
    socket::server_bind,
    sozu_command::{
        logging,
        proxy::{
            CertificateFingerprint, Cluster, HttpFrontend, HttpsListener, ProxyEvent, ProxyRequest,
            ProxyRequestOrder, ProxyResponse, ProxyResponseContent, ProxyResponseStatus, Query,
            QueryAnswer, QueryAnswerCertificate, QueryCertificateType, Route, TlsVersion,
        },
        ready::Ready,
        scm_socket::ScmSocket,
    },
    timer::TimeoutContainer,
    tls::{
        CertificateResolver, GenericCertificateResolver, GenericCertificateResolverError,
        ParsedCertificateAndKey,
    },
    util::UnwrapLog,
    AcceptError, Backend, BackendConnectAction, BackendConnectionStatus, ClusterId,
    ConnectionError, ListenerHandler, Protocol, ProxyConfiguration, ProxySession, Readiness,
    SessionMetrics, SessionResult,
};

//const SERVER_PROTOS: &'static [u8] = b"\x02h2\x08http/1.1";
//const SERVER_PROTOS: &'static [u8] = b"\x08http/1.1\x02h2";
const SERVER_PROTOS: &[u8] = b"\x08http/1.1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsCluster {
    pub cluster_id: String,
    pub hostname: String,
    pub path_begin: String,
}

pub enum State {
    Expect(ExpectProxyProtocol<TcpStream>, Ssl),
    Handshake(TlsHandshake),
    Http(Http<SslStream<TcpStream>, Listener>),
    WebSocket(Pipe<SslStream<TcpStream>, Listener>),
    Http2(Http2<SslStream<TcpStream>>),
}

pub enum AlpnProtocols {
    H2,
    Http11,
}

pub struct Session {
    frontend_token: Token,
    backend: Option<Rc<RefCell<Backend>>>,
    back_connected: BackendConnectionStatus,
    protocol: Option<State>,
    public_address: SocketAddr,
    pool: Weak<RefCell<Pool>>,
    proxy: Rc<RefCell<Proxy>>,
    sticky_name: String,
    metrics: SessionMetrics,
    pub cluster_id: Option<String>,
    last_event: Instant,
    pub listener_token: Token,
    connection_attempt: u8,
    peer_address: Option<SocketAddr>,
    answers: Rc<RefCell<HttpAnswers>>,
    front_timeout: TimeoutContainer,
    frontend_timeout_duration: Duration,
    backend_timeout_duration: Duration,
    listener: Rc<RefCell<Listener>>,
}

impl Session {
    pub fn new(
        ssl: Ssl,
        sock: TcpStream,
        token: Token,
        pool: Weak<RefCell<Pool>>,
        proxy: Rc<RefCell<Proxy>>,
        public_address: SocketAddr,
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

        let protocol = if expect_proxy {
            trace!("starting in expect proxy state");
            gauge_add!("protocol.proxy.expect", 1);
            Some(State::Expect(
                ExpectProxyProtocol::new(sock, token, request_id),
                ssl,
            ))
        } else {
            gauge_add!("protocol.tls.handshake", 1);
            Some(State::Handshake(TlsHandshake::new(
                ssl,
                sock,
                request_id,
                peer_address,
            )))
        };

        let metrics = SessionMetrics::new(Some(wait_time));
        let mut session = Session {
            frontend_token: token,
            backend: None,
            back_connected: BackendConnectionStatus::NotConnected,
            protocol,
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

    pub fn http(&self) -> Option<&Http<SslStream<TcpStream>, Listener>> {
        self.protocol.as_ref().and_then(|protocol| {
            if let &State::Http(ref http) = protocol {
                Some(http)
            } else {
                None
            }
        })
    }

    pub fn http_mut(&mut self) -> Option<&mut Http<SslStream<TcpStream>, Listener>> {
        self.protocol.as_mut().and_then(|protocol| {
            if let &mut State::Http(ref mut http) = protocol {
                Some(http)
            } else {
                None
            }
        })
    }

    pub fn set_answer(&mut self, answer: DefaultAnswerStatus, buf: Option<Rc<Vec<u8>>>) {
        self.protocol.as_mut().map(|protocol| {
            if let &mut State::Http(ref mut http) = protocol {
                http.set_answer(answer, buf);
            }
        });
    }

    pub fn upgrade(&mut self) -> bool {
        let protocol = unwrap_msg!(self.protocol.take());

        if let State::Expect(expect, ssl) = protocol {
            debug!("switching to TLS handshake");
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
                    let mut tls = TlsHandshake::new(ssl, frontend, request_id, self.peer_address);
                    tls.readiness.event = readiness.event;

                    gauge_add!("protocol.proxy.expect", -1);
                    gauge_add!("protocol.tls.handshake", 1);
                    self.protocol = Some(State::Handshake(tls));
                    return true;
                }
            }

            // currently, only happens in expect proxy protocol with AF_UNSPEC address
            //error!("failed to upgrade from expect");
            self.protocol = Some(State::Expect(expect, ssl));
            false
        } else if let State::Handshake(handshake) = protocol {
            let pool = self.pool.clone();
            let readiness = handshake.readiness.clone();

            handshake.stream.as_ref().map(|s| {
                let ssl = s.ssl();
                if let Some(version) = ssl.version2() {
                    incr!(version_str(version))
                }
                ssl.current_cipher().map(|c| incr!(c.name()));
            });

            let selected_protocol = {
                let s = handshake
                    .stream
                    .as_ref()
                    .and_then(|s| s.ssl().selected_alpn_protocol());
                trace!("selected: {}", unsafe {
                    from_utf8_unchecked(s.unwrap_or(&b""[..]))
                });

                match s {
                    Some(b"h2") => AlpnProtocols::H2,
                    Some(b"http/1.1") | None => AlpnProtocols::Http11,
                    Some(s) => {
                        error!("unknown alpn protocol: {:?}", s);
                        return false;
                    }
                }
            };

            match selected_protocol {
                AlpnProtocols::H2 => {
                    info!("got h2");
                    let mut http = Http2::new(
                        unwrap_msg!(handshake.stream),
                        self.frontend_token,
                        pool,
                        Some(self.public_address),
                        None,
                        self.sticky_name.clone(),
                        Protocol::HTTPS,
                    );

                    http.frontend.readiness = readiness;
                    http.frontend.readiness.interest =
                        Ready::readable() | Ready::hup() | Ready::error();

                    gauge_add!("protocol.tls.handshake", -1);
                    gauge_add!("protocol.http2s", 1);

                    self.protocol = Some(State::Http2(http));
                    true
                }
                AlpnProtocols::Http11 => {
                    let backend_timeout_duration = self.backend_timeout_duration;
                    let mut http = Http::new(
                        unwrap_msg!(handshake.stream),
                        self.frontend_token,
                        handshake.request_id,
                        pool,
                        self.public_address,
                        self.peer_address,
                        self.sticky_name.clone(),
                        Protocol::HTTPS,
                        self.answers.clone(),
                        self.front_timeout.take(),
                        self.frontend_timeout_duration,
                        backend_timeout_duration,
                        self.listener.clone(),
                    );

                    http.front_readiness = readiness;
                    http.front_readiness.interest =
                        Ready::readable() | Ready::hup() | Ready::error();

                    gauge_add!("protocol.tls.handshake", -1);
                    gauge_add!("protocol.https", 1);

                    self.protocol = Some(State::Http(http));
                    true
                }
            }
        } else if let State::Http(mut http) = protocol {
            debug!("https switching to wss");
            let front_token = self.frontend_token;
            let back_token = unwrap_msg!(http.back_token());
            let ws_context = http.websocket_context();

            let front_buf = match http.front_buf {
                Some(buf) => buf.buffer,
                None => {
                    if let Some(p) = self.pool.upgrade() {
                        if let Some(buf) = p.borrow_mut().checkout() {
                            buf
                        } else {
                            return false;
                        }
                    } else {
                        return false;
                    }
                }
            };
            let back_buf = match http.back_buf {
                Some(buf) => buf.buffer,
                None => {
                    if let Some(p) = self.pool.upgrade() {
                        if let Some(buf) = p.borrow_mut().checkout() {
                            buf
                        } else {
                            return false;
                        }
                    } else {
                        return false;
                    }
                }
            };

            gauge_add!("protocol.https", -1);
            gauge_add!("http.active_requests", -1);
            gauge_add!("websocket.active_requests", 1);
            gauge_add!("protocol.wss", 1);

            let mut pipe = Pipe::new(
                http.frontend,
                front_token,
                http.request_id,
                http.cluster_id,
                http.backend_id,
                Some(ws_context),
                Some(unwrap_msg!(http.backend)),
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

            self.protocol = Some(State::WebSocket(pipe));
            true
        } else {
            self.protocol = Some(protocol);
            true
        }
    }

    fn front_hup(&mut self) -> SessionResult {
        match *unwrap_msg!(self.protocol.as_mut()) {
            State::Http(ref mut http) => http.front_hup(),
            State::WebSocket(ref mut pipe) => pipe.front_hup(&mut self.metrics),
            State::Http2(ref mut http) => http.front_hup(),
            State::Handshake(_) => SessionResult::CloseSession,
            State::Expect(_, _) => SessionResult::CloseSession,
        }
    }

    fn back_hup(&mut self) -> SessionResult {
        match *unwrap_msg!(self.protocol.as_mut()) {
            State::Http(ref mut http) => http.back_hup(),
            State::WebSocket(ref mut pipe) => pipe.back_hup(&mut self.metrics),
            State::Http2(ref mut http) => http.back_hup(),
            State::Handshake(_) => {
                error!("why a backend HUP event while still in frontend handshake?");
                SessionResult::CloseSession
            }
            State::Expect(_, _) => {
                error!("why a backend HUP event while still in frontend proxy protocol expect?");
                SessionResult::CloseSession
            }
        }
    }

    fn log_context(&self) -> String {
        if let &State::Http(ref http) = unwrap_msg!(self.protocol.as_ref()) {
            http.log_context().to_string()
        } else {
            "".to_string()
        }
    }

    fn readable(&mut self) -> SessionResult {
        let (upgrade, result) = match *unwrap_msg!(self.protocol.as_mut()) {
            State::Expect(ref mut expect, _) => {
                if !self.front_timeout.reset() {
                    error!("could not reset front timeout (HTTPS OpenSSL upgrading from Expect)");
                }
                expect.readable(&mut self.metrics)
            }
            State::Handshake(ref mut handshake) => {
                if !self.front_timeout.reset() {
                    error!(
                        "could not reset front timeout (HTTPS OpenSSL upgrading from Handshake)"
                    );
                }
                handshake.readable(&mut self.metrics)
            }
            State::Http(ref mut http) => {
                (ProtocolResult::Continue, http.readable(&mut self.metrics))
            }
            State::Http2(ref mut http) => {
                (ProtocolResult::Continue, http.readable(&mut self.metrics))
            }
            State::WebSocket(ref mut pipe) => {
                (ProtocolResult::Continue, pipe.readable(&mut self.metrics))
            }
        };

        if upgrade == ProtocolResult::Continue {
            result
        } else if self.upgrade() {
            match *unwrap_msg!(self.protocol.as_mut()) {
                State::Http(ref mut http) => http.readable(&mut self.metrics),
                State::Http2(ref mut http) => http.readable(&mut self.metrics),
                _ => result,
            }
        } else {
            SessionResult::CloseSession
        }
    }

    fn writable(&mut self) -> SessionResult {
        match *unwrap_msg!(self.protocol.as_mut()) {
            State::Expect(_, _) => SessionResult::CloseSession,
            State::Handshake(_) => SessionResult::CloseSession,
            State::Http(ref mut http) => http.writable(&mut self.metrics),
            State::Http2(ref mut http) => http.writable(&mut self.metrics),
            State::WebSocket(ref mut pipe) => pipe.writable(&mut self.metrics),
        }
    }

    fn back_readable(&mut self) -> SessionResult {
        let (upgrade, result) = match *unwrap_msg!(self.protocol.as_mut()) {
            State::Expect(_, _) => (ProtocolResult::Continue, SessionResult::CloseSession),
            State::Http(ref mut http) => http.back_readable(&mut self.metrics),
            State::Http2(ref mut http) => (
                ProtocolResult::Continue,
                http.back_readable(&mut self.metrics),
            ),
            State::Handshake(_) => (ProtocolResult::Continue, SessionResult::CloseSession),
            State::WebSocket(ref mut pipe) => (
                ProtocolResult::Continue,
                pipe.back_readable(&mut self.metrics),
            ),
        };

        if upgrade == ProtocolResult::Continue {
            result
        } else if self.upgrade() {
            match *unwrap_msg!(self.protocol.as_mut()) {
                State::WebSocket(ref mut pipe) => pipe.back_readable(&mut self.metrics),
                _ => result,
            }
        } else {
            SessionResult::CloseSession
        }
    }

    fn back_writable(&mut self) -> SessionResult {
        match *unwrap_msg!(self.protocol.as_mut()) {
            State::Expect(_, _) => SessionResult::CloseSession,
            State::Handshake(_) => SessionResult::CloseSession,
            State::Http(ref mut http) => http.back_writable(&mut self.metrics),
            State::Http2(ref mut http) => http.back_writable(&mut self.metrics),
            State::WebSocket(ref mut pipe) => pipe.back_writable(&mut self.metrics),
        }
    }

    fn front_socket_mut(&mut self) -> Option<&mut TcpStream> {
        match *unwrap_msg!(self.protocol.as_mut()) {
            State::Expect(ref mut expect, _) => Some(expect.front_socket_mut()),
            State::Handshake(ref mut handshake) => handshake.socket_mut(),
            State::Http(ref mut http) => Some(http.front_socket_mut()),
            State::Http2(ref mut http) => Some(http.front_socket_mut()),
            State::WebSocket(ref mut pipe) => Some(pipe.front_socket_mut()),
        }
    }

    fn back_socket_mut(&mut self) -> Option<&mut TcpStream> {
        match *unwrap_msg!(self.protocol.as_mut()) {
            State::Expect(_, _) => None,
            State::Handshake(_) => None,
            State::Http(ref mut http) => http.back_socket_mut(),
            State::Http2(ref mut http) => http.back_socket_mut(),
            State::WebSocket(ref mut pipe) => pipe.back_socket_mut(),
        }
    }

    fn back_token(&self) -> Option<Token> {
        match unwrap_msg!(self.protocol.as_ref()) {
            &State::Expect(_, _) => None,
            &State::Handshake(_) => None,
            &State::Http(ref http) => http.back_token(),
            &State::Http2(ref http) => http.back_token(),
            &State::WebSocket(ref pipe) => pipe.back_token(),
        }
    }

    fn set_back_socket(&mut self, sock: TcpStream) {
        let backend = self.backend.clone();
        unwrap_msg!(self.http_mut()).set_back_socket(sock, backend)
    }

    fn set_back_token(&mut self, token: Token) {
        match *unwrap_msg!(self.protocol.as_mut()) {
            State::Http(ref mut http) => http.set_back_token(token),
            State::Http2(ref mut http) => http.set_back_token(token),
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

        if connected == BackendConnectionStatus::Connected {
            gauge_add!("backend.connections", 1);
            gauge_add!(
                "connections_per_backend",
                1,
                self.cluster_id.as_deref(),
                self.metrics.backend_id.as_deref()
            );

            // the back timeout was of connect_timeout duration before,
            // now that we're connected, move to backend_timeout duration
            let t = self.backend_timeout_duration;
            self.http_mut().map(|h| {
                h.set_back_timeout(t);
                h.cancel_backend_timeout();
            });

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
    }

    fn metrics(&mut self) -> &mut SessionMetrics {
        &mut self.metrics
    }

    fn remove_backend(&mut self) {
        if let Some(backend) = self.backend.take() {
            if let Some(h) = self.http_mut() {
                h.clear_back_token()
            }

            backend.borrow_mut().dec_connections();
        }
    }

    fn front_readiness(&mut self) -> &mut Readiness {
        let r = match *unwrap_msg!(self.protocol.as_mut()) {
            State::Expect(ref mut expect, _) => &mut expect.readiness,
            State::Handshake(ref mut handshake) => &mut handshake.readiness,
            State::Http(ref mut http) => http.front_readiness(),
            State::Http2(ref mut http) => http.front_readiness(),
            State::WebSocket(ref mut pipe) => &mut pipe.front_readiness,
        };
        //info!("current readiness: {:?}", r);
        r
    }

    fn back_readiness(&mut self) -> Option<&mut Readiness> {
        let r = match *unwrap_msg!(self.protocol.as_mut()) {
            State::Http(ref mut http) => Some(http.back_readiness()),
            State::Http2(ref mut http) => Some(http.back_readiness()),
            State::WebSocket(ref mut pipe) => Some(&mut pipe.back_readiness),
            _ => None,
        };
        //info!("current readiness: {:?}", r);
        r
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

        match *unwrap_msg!(self.protocol.as_mut()) {
            State::Http(ref mut http) => http.cancel_timeouts(),
            State::WebSocket(ref mut pipe) => pipe.cancel_timeouts(),
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
            if let Some(h) = self.http_mut() {
                h.cancel_backend_timeout()
            }

            if self
                .back_readiness()
                .map(|r| r.event.is_hup())
                .unwrap_or(false)
                && !self
                    .http_mut()
                    .map(|h| h.test_back_socket())
                    .unwrap_or(false)
            {
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
                    Ok(BackendConnectAction::Reuse) | Err(_) => {}
                    // New or Replace: stop here, we must wait for an event
                    _ => return SessionResult::Continue,
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
                let order = self.readable();
                trace!("front readable\tinterpreting session order {:?}", order);

                match order {
                    SessionResult::ConnectBackend => {
                        match self.connect_to_backend(session.clone()) {
                            // reuse connection or send a default answer, we can continue
                            Ok(BackendConnectAction::Reuse) | Err(_) => {}
                            // New or Replace: stop here, we must wait for an event
                            _ => return SessionResult::Continue,
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
                        /*self.front_readiness().interest.insert(Ready::writable());
                        if ! self.front_readiness().event.is_writable() {
                          error!("got back socket HUP but there's still data, and front is not writable yet(openssl)[{:?} => {:?}]", self.frontend_token, self.back_token());
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
            incr!("https_openssl.infinite_loop.error");

            let front_interest = self.front_readiness().interest & self.front_readiness().event;
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
            if let Some(fd) = self.back_socket_mut().map(|s| s.as_raw_fd()) {
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
                if let Some(sock) = self.back_socket_mut() {
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

            self.http_mut().map(|h| {
                h.clear_back_token();
                h.remove_backend();
            });
        }
    }

    fn check_circuit_breaker(&mut self) -> Result<(), ConnectionError> {
        if self.connection_attempt == CONN_RETRIES {
            error!("{} max connection attempt reached", self.log_context());
            self.set_answer(DefaultAnswerStatus::Answer503, None);
            Err(ConnectionError::NoBackendAvailable)
        } else {
            Ok(())
        }
    }

    fn check_backend_connection(&mut self) -> bool {
        let is_valid_backend_socket = self
            .http_mut()
            .map(|h| h.is_valid_backend_socket())
            .unwrap_or(false);

        if is_valid_backend_socket {
            //matched on keepalive
            self.metrics.backend_id = self.backend.as_ref().map(|i| i.borrow().backend_id.clone());
            self.metrics.backend_start();
            self.http_mut().map(|h| {
                h.backend_data
                    .as_mut()
                    .map(|b| b.borrow_mut().active_requests += 1)
            });

            true
        } else {
            false
        }
    }

    pub fn extract_route(&self) -> Result<(&str, &str, &Method), ConnectionError> {
        let h = self
            .http()
            .and_then(|http| http.request_state.as_ref())
            .and_then(|s| s.get_host())
            .ok_or(ConnectionError::NoHostGiven)?;

        let host: &str = if let Ok((i, (hostname, port))) = hostname_and_port(h.as_bytes()) {
            if i != &b""[..] {
                error!(
                    "connect_to_backend: invalid remaining chars after hostname. Host: {}",
                    h
                );
                return Err(ConnectionError::InvalidHost);
            }

            // it is alright to call from_utf8_unchecked,
            // we already verified that there are only ascii
            // chars in there
            let hostname_str = unsafe { from_utf8_unchecked(hostname) };

            //FIXME: what if we don't use SNI?
            let servername: Option<String> = self
                .http()
                .and_then(|h| h.frontend.ssl().servername(NameType::HOST_NAME))
                .map(|s| s.to_string());

            if servername.as_deref() != Some(hostname_str) {
                error!(
                    "TLS SNI hostname '{:?}' and Host header '{}' don't match",
                    servername, hostname_str
                );
                /*FIXME: deactivate this check for a temporary test
                unwrap_msg!(session.http()).set_answer(DefaultAnswerStatus::Answer404, None);
                return Err(ConnectionError::HostNotFound);
                */
            }

            //FIXME: we should check that the port is right too

            if port == Some(&b"443"[..]) {
                hostname_str
            } else {
                h
            }
        } else {
            error!("hostname parsing failed");
            return Err(ConnectionError::InvalidHost);
        };

        let rl: &RequestLine = self
            .http()
            .and_then(|http| http.request_state.as_ref())
            .and_then(|r| r.get_request_line())
            .ok_or(ConnectionError::NoRequestLineGiven)?;

        Ok((host, &rl.uri, &rl.method))
    }

    pub fn backend_from_request(
        &mut self,
        cluster_id: &str,
        front_should_stick: bool,
    ) -> Result<TcpStream, ConnectionError> {
        let sticky_session = self
            .http()
            .and_then(|http| http.request_state.as_ref())
            .and_then(|r| r.get_sticky_session());

        let res = match (front_should_stick, sticky_session) {
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

        match res {
            Err(e) => {
                self.set_answer(DefaultAnswerStatus::Answer503, None);
                Err(e)
            }
            Ok((backend, conn)) => {
                if front_should_stick {
                    let sticky_name = self.proxy.borrow().listeners[&self.listener_token]
                        .borrow()
                        .config
                        .sticky_name
                        .clone();
                    self.http_mut().map(|http| {
                        http.sticky_session = Some(StickySession::new(
                            backend
                                .borrow()
                                .sticky_id
                                .clone()
                                .unwrap_or(backend.borrow().backend_id.clone()),
                        ));
                        http.sticky_name = sticky_name.to_string();
                    });
                }
                self.metrics.backend_id = Some(backend.borrow().backend_id.clone());
                self.metrics.backend_start();
                if let Some(http) = self.http_mut() {
                    http.set_backend_id(backend.borrow().backend_id.clone());
                }
                self.backend = Some(backend);

                Ok(conn)
            }
        }
    }

    fn cluster_id_from_request(&mut self) -> Result<String, ConnectionError> {
        let (host, uri, method) = match self.extract_route() {
            Ok(t) => t,
            Err(e) => {
                self.set_answer(DefaultAnswerStatus::Answer400, None);
                return Err(e);
            }
        };

        let route_res = self
            .proxy
            .borrow()
            .listeners
            .get(&self.listener_token)
            .as_ref()
            .and_then(|l| l.borrow().frontend_from_request(host, uri, method));
        match route_res {
            Some(Route::ClusterId(cluster_id)) => Ok(cluster_id),
            Some(Route::Deny) => {
                self.set_answer(DefaultAnswerStatus::Answer401, None);
                Err(ConnectionError::Unauthorized)
            }
            None => {
                self.set_answer(DefaultAnswerStatus::Answer404, None);
                Err(ConnectionError::HostNotFound)
            }
        }
    }

    fn connect_to_backend(
        &mut self,
        session_rc: Rc<RefCell<dyn ProxySession>>,
    ) -> Result<BackendConnectAction, ConnectionError> {
        let old_cluster_id = self.http().and_then(|http| http.cluster_id.clone());
        let old_back_token = self.back_token();

        self.check_circuit_breaker()?;

        let cluster_id = self.cluster_id_from_request()?;

        if (self.http().and_then(|h| h.cluster_id.as_ref()) == Some(&cluster_id))
            && self.back_connected == BackendConnectionStatus::Connected
        {
            let has_backend = self
                .backend
                .as_ref()
                .map(|backend| {
                    let backend = &(*backend.borrow());
                    self.proxy
                        .borrow()
                        .backends
                        .borrow()
                        .has_backend(&cluster_id, backend)
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
        if old_cluster_id.is_some() && old_cluster_id.as_ref() != Some(&cluster_id) {
            if let Some(token) = self.back_token() {
                self.close_backend();

                //reset the back token here so we can remove it
                //from the slab after backend_from* fails
                self.set_back_token(token);
            }
        }

        self.cluster_id = Some(cluster_id.clone());
        if let Some(http) = self.http_mut() {
            http.cluster_id = Some(cluster_id.clone());
        }

        let front_should_stick = self
            .proxy
            .borrow()
            .clusters
            .get(&cluster_id)
            .map(|cluster| cluster.sticky_session)
            .unwrap_or(false);
        let mut socket = self.backend_from_request(&cluster_id, front_should_stick)?;

        if let Err(e) = socket.set_nodelay(true) {
            error!(
                "error setting nodelay on back socket({:?}): {:?}",
                socket, e
            );
        }
        if let Some(r) = self.back_readiness() {
            r.interest = Ready::writable() | Ready::hup() | Ready::error();
        }

        let connect_timeout = time::Duration::seconds(i64::from(
            self.proxy
                .borrow()
                .listeners
                .get(&self.listener_token)
                .as_ref()
                .map(|l| l.borrow().config.connect_timeout)
                .unwrap(),
        ));

        self.back_connected = BackendConnectionStatus::Connecting(Instant::now());
        if let Some(back_token) = old_back_token {
            self.set_back_token(back_token);
            if let Err(e) = self.proxy.borrow().registry.register(
                &mut socket,
                back_token,
                Interest::READABLE | Interest::WRITABLE,
            ) {
                error!("error registering back socket({:?}): {:?}", socket, e);
            }

            self.set_back_socket(socket);
            if let Some(h) = self.http_mut() {
                h.set_back_timeout(connect_timeout)
            }
            Ok(BackendConnectAction::Replace)
        } else {
            if self.proxy.borrow().sessions.borrow().slab.len()
                >= self.proxy.borrow().sessions.borrow().slab_capacity()
            {
                error!("not enough memory, cannot connect to backend");
                self.set_answer(DefaultAnswerStatus::Answer503, None);
                return Err(ConnectionError::TooManyConnections);
            }

            let back_token = {
                let proxy = self.proxy.borrow();
                let mut s = proxy.sessions.borrow_mut();
                let entry = s.slab.vacant_entry();
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

            self.set_back_socket(socket);
            self.set_back_token(back_token);
            if let Some(h) = self.http_mut() {
                h.set_back_timeout(connect_timeout)
            }
            Ok(BackendConnectAction::New)
        }
    }
}

impl ProxySession for Session {
    fn close(&mut self) {
        //println!("TLS closing[{:?}] temp->front: {:?}, temp->back: {:?}", self.frontend_token, *self.temp.front_buf, *self.temp.back_buf);
        if let Some(http) = self.http_mut() {
            http.close()
        }
        self.metrics.service_stop();
        self.cancel_timeouts();
        if let Some(front_socket) = self.front_socket_mut() {
            if let Err(e) = front_socket.shutdown(Shutdown::Both) {
                if e.kind() != ErrorKind::NotConnected {
                    error!("error closing front socket({:?}): {:?}", front_socket, e);
                }
            }
        }

        self.close_backend();

        if let Some(State::Http(ref mut http)) = self.protocol {
            //if the state was initial, the connection was already reset
            if http.request_state != Some(RequestState::Initial) {
                gauge_add!("http.active_requests", -1);

                if let Some(b) = http.backend_data.as_mut() {
                    let mut backend = b.borrow_mut();
                    backend.active_requests = backend.active_requests.saturating_sub(1);
                }
            }
        }

        if let Some(State::WebSocket(_)) = self.protocol {
            if let Some(b) = self.backend.as_mut() {
                let mut backend = b.borrow_mut();
                backend.active_requests = backend.active_requests.saturating_sub(1);
            }
        }

        match self.protocol {
            Some(State::Expect(_, _)) => gauge_add!("protocol.proxy.expect", -1),
            Some(State::Handshake(_)) => gauge_add!("protocol.tls.handshake", -1),
            Some(State::Http(_)) => gauge_add!("protocol.https", -1),
            Some(State::Http2(_)) => gauge_add!("protocol.http2s", -1),
            Some(State::WebSocket(_)) => gauge_add!("protocol.wss", -1),
            None => {}
        }

        if let Some(fd) = self.front_socket_mut().map(|s| s.as_raw_fd()) {
            let proxy = self.proxy.borrow();
            if let Err(e) = proxy.registry.deregister(&mut SourceFd(&fd)) {
                error!("1error deregistering socket({:?}): {:?}", fd, e);
            }
        }
    }

    fn timeout(&mut self, token: Token) {
        let res = match *unwrap_msg!(self.protocol.as_mut()) {
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
            //FIXME: not implemented yet
            State::Http2(_) => SessionResult::CloseSession,
            State::Http(ref mut http) => http.timeout(token, &mut self.metrics),
        };

        if res == SessionResult::CloseSession {
            self.close();
        }
    }

    fn protocol(&self) -> Protocol {
        Protocol::HTTPS
    }

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
        let res = self.ready_inner(session);

        if res == SessionResult::CloseSession {
            self.close();
        } else if let SessionResult::CloseBackend = res {
            self.close_backend();
        }

        self.metrics().service_stop();
    }

    fn shutting_down(&mut self) {
        let res = match &mut self.protocol {
            Some(State::Http(h)) => h.shutting_down(),
            Some(State::Handshake(_)) => SessionResult::Continue,
            _ => SessionResult::CloseSession,
        };

        if res == SessionResult::CloseSession {
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
        let p: String = match &self.protocol {
            Some(State::Expect(_, _)) => String::from("Expect"),
            Some(State::Handshake(_)) => String::from("Handshake"),
            Some(State::Http(h)) => h.print_state("HTTPS"),
            Some(State::Http2(h)) => format!("HTTP2S: {:?}", h.state),
            Some(State::WebSocket(_)) => String::from("WSS"),
            None => String::from("None"),
        };

        let rf = match *unwrap_msg!(self.protocol.as_ref()) {
            State::Expect(ref expect, _) => &expect.readiness,
            State::Handshake(ref handshake) => &handshake.readiness,
            State::Http(ref http) => &http.front_readiness,
            State::Http2(ref http) => &http.frontend.readiness,
            State::WebSocket(ref pipe) => &pipe.front_readiness,
        };
        let rb = match *unwrap_msg!(self.protocol.as_ref()) {
            State::Http(ref http) => Some(&http.back_readiness),
            State::Http2(ref http) => Some(&http.back_readiness),
            State::WebSocket(ref pipe) => Some(&pipe.back_readiness),
            _ => None,
        };

        error!("zombie session[{:?} => {:?}], state => readiness: {:?} -> {:?}, protocol: {}, cluster_id: {:?}, back_connected: {:?}, metrics: {:?}",
      self.frontend_token, self.back_token(), rf, rb, p, self.cluster_id, self.back_connected, self.metrics);
    }

    fn tokens(&self) -> Vec<Token> {
        let mut v = vec![self.frontend_token];
        if let Some(tk) = self.back_token() {
            v.push(tk)
        }

        v
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
    #[error("failed to build openssl context, {0}")]
    BuildOpenSslError(String),
}

pub struct Listener {
    listener: Option<TcpListener>,
    address: SocketAddr,
    fronts: Router,
    resolver: Arc<Mutex<GenericCertificateResolver>>,
    contexts: Arc<Mutex<HashMap<CertificateFingerprint, SslContext>>>,
    default_context: SslContext,
    answers: Rc<RefCell<HttpAnswers>>,
    config: HttpsListener,
    _ssl_options: SslOptions,
    pub token: Token,
    active: bool,
    tags: BTreeMap<String, BTreeMap<String, String>>,
}

impl ListenerHandler for Listener {
    fn get_addr(&self) -> &SocketAddr {
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
            .lock()
            .map_err(|err| ListenerError::LockError(err.to_string()))
            .ok()?;

        resolver.get_certificate(fingerprint)
    }

    fn add_certificate(
        &mut self,
        opts: &sozu_command_lib::proxy::AddCertificate,
    ) -> Result<CertificateFingerprint, Self::Error> {
        let fingerprint = {
            let mut resolver = self
                .resolver
                .lock()
                .map_err(|err| ListenerError::LockError(err.to_string()))?;

            resolver
                .add_certificate(opts)
                .map_err(ListenerError::ResolverError)?
        };

        let certificate = X509::from_pem(opts.certificate.certificate.as_bytes())
            .map_err(|err| ListenerError::PemParseError(err.to_string()))?;
        let key = PKey::private_key_from_pem(opts.certificate.key.as_bytes())
            .map_err(|err| ListenerError::PemParseError(err.to_string()))?;

        let mut chain = vec![];
        for certificate in &opts.certificate.certificate_chain {
            chain.push(
                X509::from_pem(certificate.as_bytes())
                    .map_err(|err| ListenerError::PemParseError(err.to_string()))?,
            );
        }

        let versions = if !opts.certificate.versions.is_empty() {
            &opts.certificate.versions
        } else {
            &self.config.versions
        };

        let (context, _) = Self::create_context(
            versions,
            &self.config.cipher_list,
            &certificate,
            &key,
            &chain,
            self.resolver.to_owned(),
            self.contexts.to_owned(),
        )?;

        let mut contexts = self
            .contexts
            .lock()
            .map_err(|err| ListenerError::LockError(err.to_string()))?;

        contexts.insert(fingerprint.to_owned(), context);

        Ok(fingerprint)
    }

    fn remove_certificate(
        &mut self,
        opts: &sozu_command_lib::proxy::RemoveCertificate,
    ) -> Result<(), Self::Error> {
        {
            let mut resolver = self
                .resolver
                .lock()
                .map_err(|err| ListenerError::LockError(err.to_string()))?;

            resolver
                .remove_certificate(opts)
                .map_err(ListenerError::ResolverError)?;
        }

        let mut contexts = self
            .contexts
            .lock()
            .map_err(|err| ListenerError::LockError(err.to_string()))?;

        contexts.remove(&opts.fingerprint);

        Ok(())
    }
}

impl Listener {
    pub fn try_new(config: HttpsListener, token: Token) -> Result<Listener, ListenerError> {
        let resolver = Arc::new(Mutex::new(GenericCertificateResolver::new()));
        let contexts = Arc::new(Mutex::new(HashMap::new()));

        let (default_context, ssl_options): (SslContext, SslOptions) =
            Self::create_default_context(&config, resolver.to_owned(), contexts.to_owned())?;

        Ok(Listener {
            listener: None,
            address: config.address,
            default_context,
            resolver,
            contexts,
            answers: Rc::new(RefCell::new(HttpAnswers::new(
                &config.answer_404,
                &config.answer_503,
            ))),
            active: false,
            fronts: Router::new(),
            config,
            _ssl_options: ssl_options,
            token,
            tags: BTreeMap::new(),
        })
    }

    pub fn activate(
        &mut self,
        registry: &Registry,
        tcp_listener: Option<TcpListener>,
    ) -> Option<Token> {
        if self.active {
            return Some(self.token);
        }

        let mut listener = tcp_listener.or_else(|| {
            server_bind(self.config.address)
                .map_err(|e| {
                    error!(
                        "could not create listener {:?}: {:?}",
                        self.config.address, e
                    );
                })
                .ok()
        });

        if let Some(ref mut sock) = listener {
            if let Err(e) = registry.register(sock, self.token, Interest::READABLE) {
                error!("cannot register listen socket({:?}): {:?}", sock, e);
            }
        } else {
            return None;
        }

        self.listener = listener;
        self.active = true;
        Some(self.token)
    }

    pub fn create_default_context(
        config: &HttpsListener,
        resolver: Arc<Mutex<GenericCertificateResolver>>,
        contexts: Arc<Mutex<HashMap<CertificateFingerprint, SslContext>>>,
    ) -> Result<(SslContext, SslOptions), ListenerError> {
        let cert_read = config
            .certificate
            .as_ref()
            .map(|c| c.as_bytes())
            .unwrap_or_else(|| include_bytes!("../assets/certificate.pem"));

        let key_read = config
            .key
            .as_ref()
            .map(|c| c.as_bytes())
            .unwrap_or_else(|| include_bytes!("../assets/key.pem"));

        let mut chain = vec![];
        for certificate in &config.certificate_chain {
            chain.push(
                X509::from_pem(certificate.as_bytes())
                    .map_err(|err| ListenerError::PemParseError(err.to_string()))?,
            );
        }

        let certificate = X509::from_pem(cert_read)
            .map_err(|err| ListenerError::PemParseError(err.to_string()))?;

        let key = PKey::private_key_from_pem(key_read)
            .map_err(|err| ListenerError::PemParseError(err.to_string()))?;

        Self::create_context(
            &config.versions,
            &config.cipher_list,
            &certificate,
            &key,
            &chain[..],
            resolver,
            contexts,
        )
    }

    pub fn create_context(
        tls_versions: &[TlsVersion],
        cipher_list: &str,
        cert: &X509,
        key: &PKey<Private>,
        cert_chain: &[X509],
        resolver: Arc<Mutex<GenericCertificateResolver>>,
        contexts: Arc<Mutex<HashMap<CertificateFingerprint, SslContext>>>,
    ) -> Result<(SslContext, SslOptions), ListenerError> {
        let mut context = SslContext::builder(SslMethod::tls())
            .map_err(|err| ListenerError::BuildOpenSslError(err.to_string()))?;

        let mut mode = ssl::SslMode::ENABLE_PARTIAL_WRITE;
        mode.insert(ssl::SslMode::ACCEPT_MOVING_WRITE_BUFFER);
        //FIXME: maybe activate ssl::SslMode::RELEASE_BUFFERS to save some memory?
        context.set_mode(mode);

        let mut ssl_options = ssl::SslOptions::CIPHER_SERVER_PREFERENCE
            | ssl::SslOptions::NO_COMPRESSION
            | ssl::SslOptions::NO_TICKET;
        let mut versions = ssl::SslOptions::NO_SSLV2 | ssl::SslOptions::NO_SSLV3 |
      ssl::SslOptions::NO_TLSV1 | ssl::SslOptions::NO_TLSV1_1 |
      ssl::SslOptions::NO_TLSV1_2
      //FIXME: the value will not be available if openssl-sys does not find an OpenSSL v1.1
      /*| ssl::SslOptions::NO_TLSV1_3*/;

        for version in tls_versions.iter() {
            match version {
                TlsVersion::SSLv2 => versions.remove(ssl::SslOptions::NO_SSLV2),
                TlsVersion::SSLv3 => versions.remove(ssl::SslOptions::NO_SSLV3),
                TlsVersion::TLSv1_0 => versions.remove(ssl::SslOptions::NO_TLSV1),
                TlsVersion::TLSv1_1 => versions.remove(ssl::SslOptions::NO_TLSV1_1),
                TlsVersion::TLSv1_2 => versions.remove(ssl::SslOptions::NO_TLSV1_2),
                //TlsVersion::TLSv1_3 => versions.remove(ssl::SslOptions::NO_TLSV1_3),
                TlsVersion::TLSv1_3 => {} //s         => error!("unrecognized TLS version: {:?}", s)
            };
        }

        ssl_options.insert(versions);
        trace!("parsed tls options: {:?}", ssl_options);

        context.set_options(ssl_options);
        context.set_session_cache_size(1);
        context.set_session_cache_mode(SslSessionCacheMode::OFF);

        if let Err(e) = setup_curves(&mut context) {
            error!("could not setup curves for openssl: {:?}", e);
            return Err(ListenerError::BuildOpenSslError(e.to_string()));
        }

        if let Err(e) = setup_dh(&mut context) {
            error!("could not setup DH for openssl: {:?}", e);
            return Err(ListenerError::BuildOpenSslError(e.to_string()));
        }

        if let Err(e) = context.set_cipher_list(cipher_list) {
            error!("could not set context cipher list: {:?}", e);
            return Err(ListenerError::BuildOpenSslError(e.to_string()));
        }

        if let Err(e) = context.set_certificate(cert) {
            error!("error adding certificate to context: {:?}", e);
            return Err(ListenerError::BuildOpenSslError(e.to_string()));
        }
        if let Err(e) = context.set_private_key(key) {
            error!("error adding private key to context: {:?}", e);
            return Err(ListenerError::BuildOpenSslError(e.to_string()));
        }
        if let Err(e) = context.set_alpn_protos(SERVER_PROTOS) {
            error!("could not set ALPN protocols: {:?}", e);
            return Err(ListenerError::BuildOpenSslError(e.to_string()));
        }

        context.set_alpn_select_callback(move |_ssl: &mut SslRef, client_protocols: &[u8]| {
            debug!("got protocols list from client: {:?}", unsafe {
                from_utf8_unchecked(client_protocols)
            });
            match select_next_proto(SERVER_PROTOS, client_protocols) {
                None => Err(AlpnError::ALERT_FATAL),
                Some(selected) => {
                    debug!("selected protocol with ALPN: {:?}", unsafe {
                        from_utf8_unchecked(selected)
                    });
                    Ok(selected)
                }
            }
        });

        for cert in cert_chain {
            if let Err(e) = context.add_extra_chain_cert(cert.clone()) {
                error!("error adding chain certificate to context: {:?}", e);
                return Err(ListenerError::BuildOpenSslError(e.to_string()));
            }
        }

        context.set_client_hello_callback(Self::create_client_hello_callback(resolver, contexts));

        Ok((context.build(), ssl_options))
    }

    #[allow(dead_code)]
    fn create_servername_callback(
        resolver: Arc<Mutex<GenericCertificateResolver>>,
        contexts: Arc<Mutex<HashMap<CertificateFingerprint, SslContext>>>,
    ) -> impl Fn(&mut SslRef, &mut SslAlert) -> Result<(), SniError> + 'static + Sync + Send {
        move |ssl: &mut SslRef, alert: &mut SslAlert| {
            let resolver = unwrap_msg!(resolver.lock());
            let contexts = unwrap_msg!(contexts.lock());

            trace!("ref: {:?}", ssl);
            if let Some(servername) = ssl.servername(NameType::HOST_NAME).map(|s| s.to_string()) {
                debug!("looking for fingerprint for {:?}", servername);
                if let Some(kv) = resolver.domain_lookup(servername.as_bytes(), true) {
                    debug!(
                        "looking for context for {:?} with fingerprint {:?}",
                        servername, kv.1
                    );
                    if let Some(context) = contexts.get(&kv.1) {
                        debug!("found context for {:?}", servername);

                        if let Ok(()) = ssl.set_ssl_context(context) {
                            debug!(
                                "servername is now {:?}",
                                ssl.servername(NameType::HOST_NAME)
                            );

                            return Ok(());
                        } else {
                            error!("could not set context for {:?}", servername);
                        }
                    } else {
                        error!("no context found for {:?}", servername);
                    }
                } else {
                    //error!("unrecognized server name: {}", servername);
                }
            } else {
                //error!("no server name information found");
            }

            incr!("openssl.sni.error");
            *alert = SslAlert::UNRECOGNIZED_NAME;
            Err(SniError::ALERT_FATAL)
        }
    }

    fn create_client_hello_callback(
        resolver: Arc<Mutex<GenericCertificateResolver>>,
        contexts: Arc<Mutex<HashMap<CertificateFingerprint, SslContext>>>,
    ) -> impl Fn(&mut SslRef, &mut SslAlert) -> Result<ssl::ClientHelloResponse, ErrorStack>
           + 'static
           + Sync
           + Send {
        move |ssl: &mut SslRef, alert: &mut SslAlert| {
            debug!(
                "client hello callback: TLS version = {:?}",
                ssl.version_str()
            );

            let server_name_opt = get_server_name(ssl).and_then(parse_sni_name_list);
            // no SNI extension, use the default context
            if server_name_opt.is_none() {
                *alert = SslAlert::UNRECOGNIZED_NAME;
                return Ok(ssl::ClientHelloResponse::SUCCESS);
            }

            let mut sni_names = server_name_opt.unwrap();
            let resolver = unwrap_msg!(resolver.lock());
            let contexts = unwrap_msg!(contexts.lock());

            while !sni_names.is_empty() {
                debug!("name list:\n{}", sni_names.to_hex(16));
                match parse_sni_name(sni_names) {
                    None => break,
                    Some((i, servername)) => {
                        debug!("parsed name:\n{}", servername.to_hex(16));
                        sni_names = i;

                        if let Some(kv) = resolver.domain_lookup(servername, true) {
                            debug!(
                                "looking for context for {:?} with fingerprint {:?}",
                                servername, kv.1
                            );
                            if let Some(context) = contexts.get(&kv.1) {
                                debug!("found context for {:?}", servername);

                                if let Ok(()) = ssl.set_ssl_context(context) {
                                    ssl_set_options(ssl, context);

                                    return Ok(ssl::ClientHelloResponse::SUCCESS);
                                } else {
                                    error!("could not set context for {:?}", servername);
                                }
                            } else {
                                error!("no context found for {:?}", servername);
                            }
                        } else {
                            error!(
                                "unrecognized server name: {:?}",
                                std::str::from_utf8(servername)
                            );
                        }
                    }
                }
            }

            *alert = SslAlert::UNRECOGNIZED_NAME;
            Ok::<ssl::ClientHelloResponse, ErrorStack>(ssl::ClientHelloResponse::SUCCESS)
        }
    }

    pub fn add_https_front(&mut self, tls_front: HttpFrontend) -> bool {
        self.fronts.add_http_front(tls_front)
    }

    pub fn remove_https_front(&mut self, tls_front: HttpFrontend) -> bool {
        debug!("removing tls_front {:?}", tls_front);
        self.fronts.remove_http_front(tls_front)
    }

    // ToDo factor out with http.rs
    pub fn frontend_from_request(&self, host: &str, uri: &str, method: &Method) -> Option<Route> {
        let host: &str = if let Ok((i, (hostname, _))) = hostname_and_port(host.as_bytes()) {
            if i != &b""[..] {
                error!(
                    "frontend_from_request: invalid remaining chars after hostname. Host: {}",
                    host
                );
                return None;
            }

            // it is alright to call from_utf8_unchecked,
            // we already verified that there are only ascii
            // chars in there
            unsafe { from_utf8_unchecked(hostname) }
        } else {
            error!("hostname parsing failed for: '{}'", host);
            return None;
        };

        self.fronts.lookup(host.as_bytes(), uri.as_bytes(), method)
    }

    fn accept(&mut self) -> Result<TcpStream, AcceptError> {
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

    pub fn remove_listener(&mut self, address: SocketAddr) -> bool {
        let len = self.listeners.len();

        self.listeners.retain(|_, l| l.borrow().address != address);
        self.listeners.len() < len
    }

    pub fn activate_listener(
        &mut self,
        addr: &SocketAddr,
        tcp_listener: Option<TcpListener>,
    ) -> Option<Token> {
        self.listeners
            .values()
            .find(|listener| listener.borrow().address == *addr)
            .and_then(|listener| listener.borrow_mut().activate(&self.registry, tcp_listener))
    }

    pub fn give_back_listeners(&mut self) -> Vec<(SocketAddr, TcpListener)> {
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

    pub fn give_back_listener(&mut self, address: SocketAddr) -> Option<(Token, TcpListener)> {
        self.listeners
            .values()
            .find(|listener| listener.borrow().address == address)
            .map(|listener| {
                let mut owned = listener.borrow_mut();

                owned
                    .listener
                    .take()
                    .map(|listener| (owned.token, listener))
            })
            .flatten()
    }

    pub fn add_cluster(&mut self, mut cluster: Cluster) {
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
    }

    pub fn remove_cluster(&mut self, cluster_id: &str) {
        self.clusters.remove(cluster_id);
        for listener in self.listeners.values() {
            listener
                .borrow()
                .answers
                .borrow_mut()
                .remove_custom_answer(cluster_id);
        }
    }
}

impl ProxyConfiguration<Session> for Proxy {
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
        token: ListenToken,
        wait_time: Duration,
        proxy: Rc<RefCell<Self>>,
    ) -> Result<(), AcceptError> {
        if let Some(listener) = self.listeners.get(&Token(token.0)) {
            if let Err(e) = frontend_sock.set_nodelay(true) {
                error!(
                    "error setting nodelay on front socket({:?}): {:?}",
                    frontend_sock, e
                );
            }

            let owned = listener.borrow();
            if let Ok(ssl) = Ssl::new(&owned.default_context) {
                let mut s = self.sessions.borrow_mut();
                let entry = s.slab.vacant_entry();
                let session_token = Token(entry.key());

                if let Err(e) = self.registry.register(
                    &mut frontend_sock,
                    session_token,
                    Interest::READABLE | Interest::WRITABLE,
                ) {
                    error!(
                        "error registering front socket({:?}): {:?}",
                        frontend_sock, e
                    );
                }

                let c = Session::new(
                    ssl,
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
                );

                let session = Rc::new(RefCell::new(c));
                entry.insert(session);

                s.incr();
                Ok(())
            } else {
                error!("could not create ssl context");
                Err(AcceptError::IoError)
            }
        } else {
            Err(AcceptError::IoError)
        }
    }

    fn notify(&mut self, message: ProxyRequest) -> ProxyResponse {
        //trace!("{} notified", message);
        match message.order {
            ProxyRequestOrder::AddCluster(cluster) => {
                debug!("{} add cluster {:?}", message.id, cluster);
                self.add_cluster(cluster);
                ProxyResponse::ok(message.id)
            }
            ProxyRequestOrder::RemoveCluster { cluster_id } => {
                debug!("{} remove cluster {:?}", message.id, cluster_id);
                self.remove_cluster(&cluster_id);
                ProxyResponse::ok(message.id)
            }
            ProxyRequestOrder::AddHttpsFrontend(front) => {
                //info!("HTTPS\t{} add front {:?}", id, front);
                if let Some(listener) = self
                    .listeners
                    .values()
                    .find(|l| l.borrow().address == front.address)
                {
                    let mut owned = listener.borrow_mut();
                    owned.set_tags(front.hostname.to_owned(), front.tags.to_owned());
                    owned.add_https_front(front);
                    ProxyResponse::ok(message.id)
                } else {
                    ProxyResponse::error(
                        message.id,
                        format!("adding front {:?} to unknown listener", front),
                    )
                }
            }
            ProxyRequestOrder::RemoveHttpsFrontend(front) => {
                //info!("HTTPS\t{} remove front {:?}", id, front);
                if let Some(listener) = self
                    .listeners
                    .values()
                    .find(|l| l.borrow_mut().address == front.address)
                {
                    let mut owned = listener.borrow_mut();
                    owned.set_tags(front.hostname.to_owned(), None);
                    owned.remove_https_front(front);
                    ProxyResponse::ok(message.id)
                } else {
                    ProxyResponse::error(
                        message.id,
                        format!("No listener found for the frontend {:?}", front),
                    )
                }
            }
            ProxyRequestOrder::AddCertificate(add_certificate) => {
                if let Some(listener) = self
                    .listeners
                    .values()
                    .find(|l| l.borrow().address == add_certificate.address)
                {
                    //info!("HTTPS\t{} add certificate: {:?}", id, certificate_and_key);
                    match listener.borrow_mut().add_certificate(&add_certificate) {
                        Ok(_) => ProxyResponse::ok(message.id),
                        Err(err) => ProxyResponse::error(message.id, err),
                    }
                } else {
                    error!("adding certificate to unknown listener");
                    ProxyResponse::error(message.id, "unsupported message")
                }
            }
            ProxyRequestOrder::RemoveCertificate(remove_certificate) => {
                if let Some(listener) = self
                    .listeners
                    .values()
                    .find(|l| l.borrow().address == remove_certificate.address)
                {
                    match listener
                        .borrow_mut()
                        .remove_certificate(&remove_certificate)
                    {
                        Ok(_) => ProxyResponse::ok(message.id),
                        Err(err) => ProxyResponse::error(message.id, err),
                    }
                } else {
                    error!("removing certificate to unknown listener");
                    ProxyResponse::error(message.id, "unsupported message")
                }
            }
            ProxyRequestOrder::ReplaceCertificate(replace) => {
                if let Some(listener) = self
                    .listeners
                    .values_mut()
                    .find(|l| l.borrow().address == replace.address)
                {
                    match listener.borrow_mut().replace_certificate(&replace) {
                        Ok(_) => ProxyResponse::ok(message.id),
                        Err(err) => ProxyResponse::error(message.id, err),
                    }
                } else {
                    error!("replacing certificate to unknown listener");
                    ProxyResponse::error(message.id, "unsupported message")
                }
            }
            ProxyRequestOrder::RemoveListener(remove) => {
                debug!("removing HTTPS listener at address {:?}", remove.address);
                if !self.remove_listener(remove.address) {
                    ProxyResponse::error(
                        message.id,
                        format!(
                            "no HTTPS listener to remove at address {:?}",
                            remove.address
                        ),
                    )
                } else {
                    ProxyResponse::ok(message.id)
                }
            }
            ProxyRequestOrder::SoftStop => {
                info!("{} processing soft shutdown", message.id);
                let listeners: HashMap<_, _> = self.listeners.drain().collect();
                for (_, listener) in listeners.iter() {
                    listener.borrow_mut().listener.take().map(|mut sock| {
                        if let Err(e) = self.registry.deregister(&mut sock) {
                            error!("error deregistering listen socket({:?}): {:?}", sock, e);
                        }
                    });
                }
                ProxyResponse::processing(message.id)
            }
            ProxyRequestOrder::HardStop => {
                info!("{} hard shutdown", message.id);
                let listeners: HashMap<_, _> = self.listeners.drain().collect();
                for (_, listener) in listeners.iter() {
                    listener.borrow_mut().listener.take().map(|mut sock| {
                        if let Err(e) = self.registry.deregister(&mut sock) {
                            error!("error dereginstering listen socket({:?}): {:?}", sock, e);
                        }
                    });
                }
                ProxyResponse::processing(message.id)
            }
            ProxyRequestOrder::Status => {
                debug!("{} status", message.id);
                ProxyResponse::ok(message.id)
            }
            ProxyRequestOrder::Logging(logging_filter) => {
                debug!(
                    "{} changing logging filter to {}",
                    message.id, logging_filter
                );
                logging::LOGGER.with(|l| {
                    let directives = logging::parse_logging_spec(&logging_filter);
                    l.borrow_mut().set_directives(directives);
                });
                ProxyResponse::processing(message.id)
            }
            ProxyRequestOrder::Query(Query::Certificates(QueryCertificateType::All)) => {
                let res = self
                    .listeners
                    .values()
                    .map(|listener| {
                        let owned = listener.borrow();
                        let resolver = unwrap_msg!(owned.resolver.lock());
                        let res = resolver
                            .domains
                            .to_hashmap()
                            .drain()
                            .map(|(k, v)| (String::from_utf8(k).unwrap(), v.0))
                            .collect();

                        (owned.address, res)
                    })
                    .collect::<HashMap<_, _>>();

                info!("got Certificates::All query, answering with {:?}", res);

                ProxyResponse {
                    id: message.id,
                    status: ProxyResponseStatus::Ok,
                    content: Some(ProxyResponseContent::Query(QueryAnswer::Certificates(
                        QueryAnswerCertificate::All(res),
                    ))),
                }
            }
            ProxyRequestOrder::Query(Query::Certificates(QueryCertificateType::Domain(d))) => {
                let res = self
                    .listeners
                    .values()
                    .map(|listener| {
                        let owned = listener.borrow();
                        let resolver = unwrap_msg!(owned.resolver.lock());
                        (
                            owned.address,
                            resolver.domain_lookup(d.as_bytes(), true).map(|(k, v)| {
                                (String::from_utf8(k.to_vec()).unwrap(), v.0.clone())
                            }),
                        )
                    })
                    .collect::<HashMap<_, _>>();

                info!(
                    "got Certificates::Domain({}) query, answering with {:?}",
                    d, res
                );

                ProxyResponse {
                    id: message.id,
                    status: ProxyResponseStatus::Ok,
                    content: Some(ProxyResponseContent::Query(QueryAnswer::Certificates(
                        QueryAnswerCertificate::Domain(res),
                    ))),
                }
            }
            command => {
                error!(
                    "{} unsupported message for OpenSSL proxy, ignoring {:?}",
                    message.id, command
                );
                ProxyResponse::error(message.id, "unsupported message")
            }
        }
    }
}

#[cfg(ossl101)]
pub fn setup_curves(ctx: &mut SslContextBuilder) -> Result<(), ErrorStack> {
    use openssl::ec::EcKey;
    use openssl::nid;

    let curve = EcKey::from_curve_name(nid::X9_62_PRIME256V1)?;
    ctx.set_tmp_ecdh(&curve)
}

#[cfg(ossl102)]
fn setup_curves(ctx: &mut SslContextBuilder) -> Result<(), ErrorStack> {
    match Dh::get_2048_256() {
        Ok(dh) => ctx.set_tmp_dh(&dh),
        Err(e) => return Err(e),
    };
    ctx.set_ecdh_auto(true)
}

#[cfg(ossl11x)]
fn setup_curves(ctx: &mut SslContextBuilder) -> Result<(), ErrorStack> {
    use openssl::ec::EcKey;

    let curve = EcKey::from_curve_name(nid::Nid::X9_62_PRIME256V1)?;
    ctx.set_tmp_ecdh(&curve)
}

#[cfg(all(not(ossl101), not(ossl102), not(ossl11x)))]
fn setup_curves(_: &mut SslContextBuilder) -> Result<(), ErrorStack> {
    compile_error!("unsupported openssl version, please open an issue");
    Ok(())
}

fn setup_dh(ctx: &mut SslContextBuilder) -> Result<(), ErrorStack> {
    let dh = Dh::params_from_pem(
        b"
-----BEGIN DH PARAMETERS-----
MIIBCAKCAQEAm7EQIiluUX20jnC3NGhikNVGH7MSlRIgTpkUlCyWrmlJci+Pu54Q
YzOZw3tw4g84+ipTzpdGuUvZxNd4Y4ZUp/XsdZgnWMfV+y9OuyYAKWGPTtEO/HUy
6buhvi5qahIXdxUm0dReFuOTrDOphNjHXggU8Iwey3U54aL+KVqLMAP+Tev8jrQN
A0mv4X8dmdvCv7LEZPVYFJb3Uprwd60ThSl3NKNTMDnwnRtXpIrSZIZcJh5IER7S
/IexUuBsopcTVIcHfLyaECIbMKnZfyrkAJfHqRWEpPOzk1euVw0hw3SkDJREfB21
yD0TrUjkXyjV/zczIYiYSROg9OE5UgYqswIBAg==
-----END DH PARAMETERS-----
",
    )?;
    ctx.set_tmp_dh(&dh)
}

use crate::server::HttpsProvider;
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
    let mut configuration = Proxy::new(registry, sessions.clone(), pool.clone(), backends.clone());
    let address = config.address;
    if configuration.add_listener(config, token).is_some()
        && configuration.activate_listener(&address, None).is_some()
    {
        let (scm_server, _scm_client) =
            UnixStream::pair().with_context(|| "Failed at creating scm stream sockets")?;
        let mut server_config: server::ServerConfig = Default::default();
        server_config.max_connections = max_buffers;
        let mut server = Server::new(
            event_loop,
            channel,
            ScmSocket::new(scm_server.as_raw_fd()),
            sessions,
            pool,
            backends,
            None,
            Some(HttpsProvider::Openssl(Rc::new(RefCell::new(configuration)))),
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
    extern crate tiny_http;
    use super::*;
    use crate::router::{trie::TrieNode, MethodRule, PathRule, Router};
    use crate::sozu_command::proxy::Route;
    use openssl::ssl::{SslContext, SslMethod};
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::rc::Rc;
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};

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
            PathRule::Prefix(uri1),
            MethodRule::new(None),
            Route::ClusterId(cluster_id1.clone())
        ));
        assert!(fronts.add_tree_rule(
            "lolcatho.st".as_bytes(),
            PathRule::Prefix(uri2),
            MethodRule::new(None),
            Route::ClusterId(cluster_id2)
        ));
        assert!(fronts.add_tree_rule(
            "lolcatho.st".as_bytes(),
            PathRule::Prefix(uri3),
            MethodRule::new(None),
            Route::ClusterId(cluster_id3)
        ));
        assert!(fronts.add_tree_rule(
            "other.domain".as_bytes(),
            PathRule::Prefix("test".to_string()),
            MethodRule::new(None),
            Route::ClusterId(cluster_id1)
        ));

        let context =
            SslContext::builder(SslMethod::tls()).expect("could not create a SslContextBuilder");

        let address: SocketAddr = FromStr::from_str("127.0.0.1:1032")
            .expect("test address 127.0.0.1:1032 should be parsed");
        let listener = Listener {
            listener: None,
            address,
            fronts,
            default_context: context.build(),
            resolver: Arc::new(Mutex::new(GenericCertificateResolver::new())),
            contexts: Arc::new(Mutex::new(HashMap::new())),
            answers: Rc::new(RefCell::new(HttpAnswers::new(
                "HTTP/1.1 404 Not Found\r\n\r\n",
                "HTTP/1.1 503 Service Unavailable\r\n\r\n",
            ))),
            config: Default::default(),
            _ssl_options: ssl::SslOptions::CIPHER_SERVER_PREFERENCE
                | ssl::SslOptions::NO_COMPRESSION
                | ssl::SslOptions::NO_TICKET
                | ssl::SslOptions::NO_SSLV2
                | ssl::SslOptions::NO_SSLV3
                | ssl::SslOptions::NO_TLSV1
                | ssl::SslOptions::NO_TLSV1_1,
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
        assert_eq!(frontend5, None);
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

fn version_str(version: SslVersion) -> &'static str {
    match version {
        SslVersion::SSL3 => "tls.version.SSLv3",
        SslVersion::TLS1 => "tls.version.TLSv1_0",
        SslVersion::TLS1_1 => "tls.version.TLSv1_1",
        SslVersion::TLS1_2 => "tls.version.TLSv1_2",
        //SslVersion::TLS1_3 => "tls.version.TLSv1_3",
        _ => "tls.version.TLSv1_3",
        //_ => "tls.version.Unknown",
    }
}

extern "C" {
    pub fn SSL_set_options(ssl: *mut openssl_sys::SSL, op: libc::c_ulong) -> libc::c_ulong;
}

fn get_server_name<'a, 'b>(ssl: &'a SslRef) -> Option<&'b [u8]> {
    unsafe {
        let mut out: *const libc::c_uchar = std::ptr::null();
        let mut outlen: libc::size_t = 0;

        let res = openssl_sys::SSL_client_hello_get0_ext(
            ssl.as_ptr(),
            0, // TLSEXT_TYPE_server_name = 0
            &mut out as *mut _,
            &mut outlen as *mut _,
        );

        if res == 0 {
            return None;
        }

        let sl = std::slice::from_raw_parts(out, outlen);

        Some(sl)
    }
}

fn ssl_set_options(ssl: &mut SslRef, context: &SslContext) {
    unsafe {
        let options = openssl_sys::SSL_CTX_get_options(context.as_ptr());
        SSL_set_options(ssl.as_ptr(), options);
    }
}

fn parse_sni_name_list(i: &[u8]) -> Option<&[u8]> {
    use nom::{multi::length_data, number::complete::be_u16};

    if i.is_empty() {
        return None;
    }

    match length_data::<_, _, (), _>(be_u16)(i).ok() {
        None => None,
        Some((i, o)) => {
            if !i.is_empty() {
                None
            } else {
                Some(o)
            }
        }
    }
}

fn parse_sni_name(i: &[u8]) -> Option<(&[u8], &[u8])> {
    use nom::{
        bytes::complete::tag, multi::length_data, number::complete::be_u16, sequence::preceded,
    };

    if i.is_empty() {
        return None;
    }

    preceded::<_, _, _, (), _, _>(
        tag([0x00]), // SNIType for hostname is 0
        length_data(be_u16),
    )(i)
    .ok()
}
