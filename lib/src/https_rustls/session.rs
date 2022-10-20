use std::{
    cell::RefCell,
    io::{ErrorKind, Read},
    net::{Shutdown, SocketAddr},
    os::unix::prelude::AsRawFd,
    rc::{Rc, Weak},
    str::from_utf8_unchecked,
};

use mio::{net::*, unix::SourceFd, *};
use rustls::{CipherSuite, ProtocolVersion, ServerConnection, SupportedCipherSuite};
use rusty_ulid::Ulid;
use time::{Duration, Instant};

use crate::{
    buffer_queue::BufferQueue,
    https_rustls::configuration::{Listener, Proxy},
    pool::Pool,
    protocol::{
        http::{
            answers::HttpAnswers,
            parser::{hostname_and_port, Method, RequestLine, RequestState},
            DefaultAnswerStatus,
        },
        proxy_protocol::expect::ExpectProxyProtocol,
        rustls::TlsHandshake,
        Http, Pipe, ProtocolResult, StickySession,
    },
    retry::RetryPolicy,
    server::{push_event, CONN_RETRIES},
    socket::FrontRustls,
    sozu_command::{
        proxy::{ProxyEvent, Route},
        ready::Ready,
    },
    timer::TimeoutContainer,
    util::UnwrapLog,
    {
        Backend, BackendConnectAction, BackendConnectionStatus, ConnectionError, Protocol,
        ProxySession, Readiness, SessionMetrics, SessionResult,
    },
};

pub enum State {
    Expect(ExpectProxyProtocol<TcpStream>, ServerConnection),
    Handshake(TlsHandshake),
    Http(Http<FrontRustls, Listener>),
    WebSocket(Pipe<FrontRustls, Listener>),
}

pub struct Session {
    pub frontend_token: Token,
    pub backend: Option<Rc<RefCell<Backend>>>,
    pub back_connected: BackendConnectionStatus,
    protocol: Option<State>,
    pub public_address: SocketAddr,
    pool: Weak<RefCell<Pool>>,
    proxy: Rc<RefCell<Proxy>>,
    pub metrics: SessionMetrics,
    pub cluster_id: Option<String>,
    sticky_name: String,
    last_event: Instant,
    pub listener_token: Token,
    pub connection_attempt: u8,
    peer_address: Option<SocketAddr>,
    answers: Rc<RefCell<HttpAnswers>>,
    front_timeout: TimeoutContainer,
    frontend_timeout_duration: Duration,
    backend_timeout_duration: Duration,
    pub listener: Rc<RefCell<Listener>>,
}

impl Session {
    pub fn new(
        ssl: ServerConnection,
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

        let state = if expect_proxy {
            trace!("starting in expect proxy state");
            gauge_add!("protocol.proxy.expect", 1);
            Some(State::Expect(
                ExpectProxyProtocol::new(sock, token, request_id),
                ssl,
            ))
        } else {
            gauge_add!("protocol.tls.handshake", 1);
            Some(State::Handshake(TlsHandshake::new(ssl, sock, request_id)))
        };

        let metrics = SessionMetrics::new(Some(wait_time));

        let mut session = Session {
            frontend_token: token,
            backend: None,
            back_connected: BackendConnectionStatus::NotConnected,
            protocol: state,
            public_address,
            pool,
            proxy,
            metrics,
            cluster_id: None,
            sticky_name,
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

    pub fn http(&self) -> Option<&Http<FrontRustls, Listener>> {
        self.protocol.as_ref().and_then(|protocol| {
            if let State::Http(ref http) = protocol {
                Some(http)
            } else {
                None
            }
        })
    }

    pub fn http_mut(&mut self) -> Option<&mut Http<FrontRustls, Listener>> {
        self.protocol.as_mut().and_then(|protocol| {
            if let State::Http(ref mut http) = *protocol {
                Some(http)
            } else {
                None
            }
        })
    }

    pub fn set_answer(&mut self, answer: DefaultAnswerStatus, buf: Option<Rc<Vec<u8>>>) {
        if let Some(State::Http(http)) = self.protocol.as_mut() {
            http.set_answer(answer, buf);
        };
    }

    pub fn upgrade(&mut self) -> bool {
        let protocol = unwrap_msg!(self.protocol.take());

        match protocol {
            State::Expect(expect, ssl) => {
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

                        let mut tls = TlsHandshake::new(ssl, frontend, request_id);
                        tls.readiness.event = readiness.event;
                        tls.readiness.event.insert(Ready::readable());

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
            }
            State::Handshake(handshake) => {
                let front_buf = self.pool.upgrade().and_then(|p| p.borrow_mut().checkout());
                if front_buf.is_none() {
                    self.protocol = Some(State::Handshake(handshake));
                    return false;
                }

                let mut front_buf = front_buf.unwrap();
                if let Some(version) = handshake.session.protocol_version() {
                    incr!(version_str(version));
                };
                if let Some(cipher) = handshake.session.negotiated_cipher_suite() {
                    incr!(ciphersuite_str(cipher));
                };

                let front_stream = FrontRustls {
                    stream: handshake.stream,
                    session: handshake.session,
                };

                let readiness = handshake.readiness.clone();
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

                gauge_add!("protocol.tls.handshake", -1);
                gauge_add!("protocol.https", 1);
                http.front_buf = Some(buf);
                http.front_readiness = readiness;
                http.front_readiness.interest = Ready::readable() | Ready::hup() | Ready::error();

                self.protocol = Some(State::Http(http));
                true
            }
            State::Http(mut http) => {
                debug!("https switching to wss");
                let front_token = self.frontend_token;
                let back_token = unwrap_msg!(http.back_token());
                let ws_context = http.websocket_context();

                let front_buf = match http.front_buf {
                    Some(buf) => buf.buffer,
                    None => {
                        let pool = match self.pool.upgrade() {
                            Some(p) => p,
                            None => return false,
                        };

                        let buffer = match pool.borrow_mut().checkout() {
                            Some(buf) => buf,
                            None => return false,
                        };
                        buffer
                    }
                };
                let back_buf = match http.back_buf {
                    Some(buf) => buf.buffer,
                    None => {
                        let pool = match self.pool.upgrade() {
                            Some(p) => p,
                            None => return false,
                        };

                        let buffer = match pool.borrow_mut().checkout() {
                            Some(buf) => buf,
                            None => return false,
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
                self.protocol = Some(State::WebSocket(pipe));
                true
            }
            _ => {
                self.protocol = Some(protocol);
                true
            }
        }
    }

    fn front_hup(&mut self) -> SessionResult {
        match *unwrap_msg!(self.protocol.as_mut()) {
            State::Http(ref mut http) => http.front_hup(),
            State::WebSocket(ref mut pipe) => pipe.front_hup(&mut self.metrics),
            State::Handshake(_) => SessionResult::CloseSession,
            State::Expect(_, _) => SessionResult::CloseSession,
        }
    }

    fn back_hup(&mut self) -> SessionResult {
        match *unwrap_msg!(self.protocol.as_mut()) {
            State::Http(ref mut http) => http.back_hup(),
            State::WebSocket(ref mut pipe) => pipe.back_hup(&mut self.metrics),
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

    pub fn log_context(&self) -> String {
        if let State::Http(ref http) = unwrap_msg!(self.protocol.as_ref()) {
            http.log_context().to_string()
        } else {
            "".to_string()
        }
    }

    fn readable(&mut self) -> SessionResult {
        let (upgrade, result) = match *unwrap_msg!(self.protocol.as_mut()) {
            State::Expect(ref mut expect, _) => {
                if !self.front_timeout.reset() {
                    error!("could not reset front timeout");
                }
                expect.readable(&mut self.metrics)
            }
            State::Handshake(ref mut handshake) => {
                if !self.front_timeout.reset() {
                    error!("could not reset front timeout");
                }
                handshake.readable()
            }
            State::Http(ref mut http) => {
                (ProtocolResult::Continue, http.readable(&mut self.metrics))
            }
            State::WebSocket(ref mut pipe) => {
                (ProtocolResult::Continue, pipe.readable(&mut self.metrics))
            }
        };

        if upgrade == ProtocolResult::Continue {
            result
        } else if self.upgrade() {
            self.readable()
        } else {
            SessionResult::CloseSession
        }
    }

    fn writable(&mut self) -> SessionResult {
        let (upgrade, result) = match *unwrap_msg!(self.protocol.as_mut()) {
            State::Expect(_, _) => return SessionResult::CloseSession,
            State::Handshake(ref mut handshake) => handshake.writable(),
            State::Http(ref mut http) => {
                (ProtocolResult::Continue, http.writable(&mut self.metrics))
            }
            State::WebSocket(ref mut pipe) => {
                (ProtocolResult::Continue, pipe.writable(&mut self.metrics))
            }
        };

        if upgrade == ProtocolResult::Continue {
            result
        } else if self.upgrade() {
            if (self.front_readiness().event & self.front_readiness().interest).is_writable() {
                self.writable()
            } else {
                SessionResult::Continue
            }
        } else {
            SessionResult::CloseSession
        }
    }

    fn back_readable(&mut self) -> SessionResult {
        let (upgrade, result) = match *unwrap_msg!(self.protocol.as_mut()) {
            State::Expect(_, _) => return SessionResult::CloseSession,
            State::Http(ref mut http) => http.back_readable(&mut self.metrics),
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
            State::WebSocket(ref mut pipe) => pipe.back_writable(&mut self.metrics),
        }
    }

    pub fn front_socket(&self) -> &TcpStream {
        match unwrap_msg!(self.protocol.as_ref()) {
            State::Expect(ref expect, _) => expect.front_socket(),
            State::Handshake(ref handshake) => &handshake.stream,
            State::Http(ref http) => http.front_socket(),
            State::WebSocket(ref pipe) => pipe.front_socket(),
        }
    }

    pub fn front_socket_mut(&mut self) -> &mut TcpStream {
        match unwrap_msg!(self.protocol.as_mut()) {
            State::Expect(ref mut expect, _) => expect.front_socket_mut(),
            State::Handshake(ref mut handshake) => &mut handshake.stream,
            State::Http(ref mut http) => http.front_socket_mut(),
            State::WebSocket(ref mut pipe) => pipe.front_socket_mut(),
        }
    }

    pub fn back_socket(&self) -> Option<&TcpStream> {
        match unwrap_msg!(self.protocol.as_ref()) {
            State::Expect(_, _) => None,
            State::Handshake(_) => None,
            State::Http(ref http) => http.back_socket(),
            State::WebSocket(ref pipe) => pipe.back_socket(),
        }
    }

    pub fn back_socket_mut(&mut self) -> Option<&mut TcpStream> {
        match unwrap_msg!(self.protocol.as_mut()) {
            State::Expect(_, _) => None,
            State::Handshake(_) => None,
            State::Http(ref mut http) => http.back_socket_mut(),
            State::WebSocket(ref mut pipe) => pipe.back_socket_mut(),
        }
    }

    pub fn back_token(&self) -> Option<Token> {
        match unwrap_msg!(self.protocol.as_ref()) {
            State::Expect(_, _) => None,
            State::Handshake(_) => None,
            State::Http(ref http) => http.back_token(),
            State::WebSocket(ref pipe) => pipe.back_token(),
        }
    }

    pub fn set_back_socket(&mut self, sock: TcpStream) {
        if let State::Http(ref mut http) = unwrap_msg!(self.protocol.as_mut()) {
            http.set_back_socket(sock, self.backend.clone())
        }
    }

    pub fn set_back_token(&mut self, token: Token) {
        match *unwrap_msg!(self.protocol.as_mut()) {
            State::Http(ref mut http) => http.set_back_token(token),
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
            if let Some(h) = self.http_mut() {
                h.set_back_timeout(t);
                h.cancel_backend_timeout();
            };

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
            };
        }
    }

    fn metrics(&mut self) -> &mut SessionMetrics {
        &mut self.metrics
    }

    fn remove_backend(&mut self) {
        if let Some(backend) = self.backend.take() {
            if let Some(h) = self.http_mut() {
                h.clear_back_token();
            }

            backend.borrow_mut().dec_connections();
        }
    }

    pub fn front_readiness(&mut self) -> &mut Readiness {
        match *unwrap_msg!(self.protocol.as_mut()) {
            State::Expect(ref mut expect, _) => &mut expect.readiness,
            State::Handshake(ref mut handshake) => &mut handshake.readiness,
            State::Http(ref mut http) => http.front_readiness(),
            State::WebSocket(ref mut pipe) => &mut pipe.front_readiness,
        }
    }

    pub fn back_readiness(&mut self) -> Option<&mut Readiness> {
        match *unwrap_msg!(self.protocol.as_mut()) {
            State::Http(ref mut http) => Some(http.back_readiness()),
            State::WebSocket(ref mut pipe) => Some(&mut pipe.back_readiness),
            _ => None,
        }
    }

    fn fail_backend_connection(&mut self) {
        if let Some(backend) = self.backend.as_ref() {
            let backend = &mut *backend.borrow_mut();
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
        };
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
                .unwrap_or_else(|| false)
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
                    .unwrap_or_else(|| false)
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
                .unwrap_or_else(Ready::empty);

            trace!(
                "PROXY\t{} {:?} F:{:?} B:{:?}",
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
                    SessionResult::Continue => {}
                    _ => {
                        if let Some(r) = self.back_readiness() {
                            r.event.remove(Ready::hup());
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
                if let Some(r) = self.back_readiness() {
                    r.interest = Ready::empty();
                }

                return SessionResult::CloseSession;
            }

            if back_interest.is_error() && self.back_hup() == SessionResult::CloseSession {
                self.front_readiness().interest = Ready::empty();
                if let Some(r) = self.back_readiness() {
                    r.interest = Ready::empty();
                }

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
            incr!("https_rustls.infinite_loop.error");

            let front_interest = self.front_readiness().interest & self.front_readiness().event;
            let back_interest = self.back_readiness().map(|r| r.interest & r.event);

            let token = self.frontend_token;
            let back = self.back_readiness().cloned();
            error!(
                "PROXY\t{:?} readiness: front {:?} / back {:?} |front: {:?} | back: {:?} ",
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
                if let Some(r) = self.back_readiness() {
                    r.event = Ready::empty();
                }

                if let Some(sock) = self.back_socket_mut() {
                    if let Err(e) = sock.shutdown(Shutdown::Both) {
                        if e.kind() != ErrorKind::NotConnected {
                            error!("error shutting down backend socket: {:?}", e);
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
            if let Some(h) = self.http_mut() {
                h.clear_back_token();
                h.remove_backend();
            };
        }
    }

    pub fn check_circuit_breaker(&mut self) -> Result<(), ConnectionError> {
        if self.connection_attempt == CONN_RETRIES {
            error!("{} max connection attempt reached", self.log_context());
            self.set_answer(DefaultAnswerStatus::Answer503, None);
            Err(ConnectionError::NoBackendAvailable)
        } else {
            Ok(())
        }
    }

    pub fn check_backend_connection(&mut self) -> bool {
        let is_valid_backend_socket = self
            .http_mut()
            .map(|h| h.is_valid_backend_socket())
            .unwrap_or(false);

        if is_valid_backend_socket {
            //matched on keepalive
            self.metrics.backend_id = self.backend.as_ref().map(|i| i.borrow().backend_id.clone());
            self.metrics.backend_start();
            if let Some(h) = self.http_mut() {
                if let Some(b) = h.backend_data.as_mut() {
                    b.borrow_mut().active_requests += 1;
                }
            };

            true
        } else {
            false
        }
    }

    pub fn extract_route(&self) -> Result<(&str, &str, &Method), ConnectionError> {
        let h = self
            .http()
            .and_then(|h| h.request_state.as_ref())
            .and_then(|r| r.get_host())
            .ok_or(ConnectionError::NoHostGiven)?;

        let host: &str = if let Ok((i, (hostname, port))) = hostname_and_port(h.as_bytes()) {
            if i != &b""[..] {
                error!("invalid remaining chars after hostname");
                return Err(ConnectionError::InvalidHost);
            }

            // it is alright to call from_utf8_unchecked,
            // we already verified that there are only ascii
            // chars in there
            let hostname_str = unsafe { from_utf8_unchecked(hostname) };

            //FIXME: what if we don't use SNI?
            let servername: Option<String> = self
                .http()
                .and_then(|h| h.frontend.session.sni_hostname())
                .map(|s| s.to_string());
            if servername.as_deref() != Some(hostname_str) {
                error!(
                    "TLS SNI hostname '{:?}' and Host header '{}' don't match",
                    servername, hostname_str
                );
                return Err(ConnectionError::HostNotFound);
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
            .and_then(|h| h.request_state.as_ref())
            .and_then(|r| r.get_request_line())
            .ok_or(ConnectionError::NoRequestLineGiven)?;

        Ok((host, &rl.uri, &rl.method))
    }

    pub fn set_backend(
        &mut self,
        backend: Rc<RefCell<Backend>>,
        front_should_stick: bool,
        sticky_name: &str,
    ) {
        if front_should_stick {
            if let Some(http) = self.http_mut() {
                http.sticky_session = Some(StickySession::new(
                    backend
                        .borrow()
                        .sticky_id
                        .clone()
                        .unwrap_or_else(|| backend.borrow().backend_id.clone()),
                ));
                http.sticky_name = sticky_name.to_string();
            };
        }
        self.metrics.backend_id = Some(backend.borrow().backend_id.clone());
        self.metrics.backend_start();

        if let Some(http) = self.http_mut() {
            http.set_backend_id(backend.borrow().backend_id.clone());
        };

        self.backend = Some(backend);
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
                    if let Some(http) = self.http_mut() {
                        http.sticky_session = Some(StickySession::new(
                            backend
                                .borrow()
                                .sticky_id
                                .clone()
                                .unwrap_or_else(|| backend.borrow().backend_id.clone()),
                        ));
                        http.sticky_name = sticky_name;
                    };
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
        let listener_token = self.listener_token;
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
            .get(&listener_token)
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
                    let backend = &*backend.borrow();
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
        };

        let front_should_stick = self
            .proxy
            .borrow()
            .clusters
            .get(&cluster_id)
            .map(|cluster| cluster.sticky_session)
            .unwrap_or(false);
        let mut socket = self.backend_from_request(&cluster_id, front_should_stick)?;

        // we still want to use the new socket
        if let Err(e) = socket.set_nodelay(true) {
            error!("error setting nodelay on back socket: {:?}", e);
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
                error!("error registering back socket: {:?}", e);
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
                error!("error registering back socket: {:?}", e);
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
        //println!("TLS closing[{:?}] temp->front: {:?}, temp->back: {:?}", self.token, *self.temp.front_buf, *self.temp.back_buf);
        if let Some(http) = self.http_mut() {
            http.close()
        }

        self.metrics.service_stop();
        self.cancel_timeouts();
        if let Err(e) = self.front_socket().shutdown(Shutdown::Both) {
            if e.kind() != ErrorKind::NotConnected {
                error!("error closing front socket: {:?}", e);
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
            Some(State::WebSocket(_)) => gauge_add!("protocol.wss", -1),
            None => {}
        }

        let fd = self.front_socket().as_raw_fd();
        let proxy = self.proxy.borrow();
        if let Err(e) = proxy.registry.deregister(&mut SourceFd(&fd)) {
            error!("1error deregistering socket({:?}): {:?}", fd, e);
        }
    }

    fn timeout(&mut self, token: Token) {
        let res = match *unwrap_msg!(self.protocol.as_mut()) {
            State::Http(ref mut http) => http.timeout(token, &mut self.metrics),
            State::WebSocket(ref mut pipe) => pipe.timeout(token, &mut self.metrics),
            _ => {
                if token == self.frontend_token {
                    self.front_timeout.triggered();
                }

                SessionResult::CloseSession
            }
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
            super::super::ready_to_string(events)
        );
        self.last_event = Instant::now();
        self.metrics.wait_start();

        if self.frontend_token == token {
            self.front_readiness().event = self.front_readiness().event | events;
        } else if self.back_token() == Some(token) {
            if let Some(r) = self.back_readiness() {
                r.event |= events;
            }
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
            Some(State::WebSocket(_)) => String::from("WSS"),
            None => String::from("None"),
        };

        let r = match *unwrap_msg!(self.protocol.as_ref()) {
            State::Expect(ref expect, _) => &expect.readiness,
            State::Handshake(ref handshake) => &handshake.readiness,
            State::Http(ref http) => &http.front_readiness,
            State::WebSocket(ref pipe) => &pipe.front_readiness,
        };

        error!("zombie session[{:?} => {:?}], state => readiness: {:?}, protocol: {}, cluster_id: {:?}, back_connected: {:?}, metrics: {:?}",
      self.frontend_token, self.back_token(), r, p, self.cluster_id, self.back_connected, self.metrics);
    }

    fn tokens(&self) -> Vec<Token> {
        let mut v = vec![self.frontend_token];
        if let Some(tk) = self.back_token() {
            v.push(tk)
        }

        v
    }
}

fn version_str(version: ProtocolVersion) -> &'static str {
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

fn ciphersuite_str(cipher: SupportedCipherSuite) -> &'static str {
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

/*
#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  #[cfg(target_pointer_width = "64")]
  fn size_test() {
    // fails depending on the platform?
    //assert_size!(Session, 2488);
    assert_size!(ExpectProxyProtocol<TcpStream>, 520);
    assert_size!(TlsHandshake, 1488);
    assert_size!(Http<FrontRustls>, 2456);
    assert_size!(Pipe<FrontRustls>, 1664);
    assert_size!(State, 2464);

    assert_size!(FrontRustls, 1456);
    assert_size!(ServerConnection, 1440);
  }
}
*/
