use std::{
    cell::RefCell,
    collections::{hash_map::Entry, BTreeMap, HashMap},
    io::ErrorKind,
    net::{Shutdown, SocketAddr},
    os::unix::io::{AsRawFd, IntoRawFd},
    rc::{Rc, Weak},
    str::from_utf8_unchecked,
};

use anyhow::Context;
use mio::{net::*, unix::SourceFd, *};
use rusty_ulid::Ulid;
use slab::Slab;
use time::{Duration, Instant};

use crate::{
    router::Router,
    sozu_command::{
        logging,
        proxy::{
            Cluster, HttpFrontend, HttpListener, ProxyEvent, ProxyRequest, ProxyRequestOrder,
            ProxyResponse, Route,
        },
        ready::Ready,
        scm_socket::{Listeners, ScmSocket},
    },
    timer::TimeoutContainer,
    util::UnwrapLog,
    ListenerHandler,
};

use super::{
    backends::BackendMap,
    pool::Pool,
    protocol::{
        http::{
            answers::HttpAnswers,
            parser::{hostname_and_port, Method, RequestState},
            DefaultAnswerStatus,
        },
        proxy_protocol::expect::ExpectProxyProtocol,
        {Http, Pipe, ProtocolResult, StickySession},
    },
    retry::RetryPolicy,
    server::{
        push_event, ListenSession, ListenToken, ProxyChannel, Server, SessionManager, CONN_RETRIES,
    },
    socket::server_bind,
    sozu_command::state::ClusterId,
    AcceptError, Backend, BackendConnectAction, BackendConnectionStatus, ConnectionError, Protocol,
    ProxyConfiguration, ProxySession, Readiness, SessionMetrics, SessionResult,
};

#[derive(PartialEq, Eq)]
pub enum SessionStatus {
    Normal,
    DefaultAnswer,
}

pub enum State {
    Expect(ExpectProxyProtocol<TcpStream>),
    Http(Http<TcpStream, Listener>),
    WebSocket(Pipe<TcpStream, Listener>),
}

/// HTTP Session to insert in the SessionManager
///
/// 1 session <=> 1 HTTP connection (client to sozu)
pub struct Session {
    frontend_token: Token,
    backend: Option<Rc<RefCell<Backend>>>,
    back_connected: BackendConnectionStatus,
    protocol: Option<State>,
    proxy: Rc<RefCell<Proxy>>,
    pool: Weak<RefCell<Pool>>,
    metrics: SessionMetrics,
    pub cluster_id: Option<String>,
    sticky_name: String,
    pub listener_token: Token,
    connection_attempt: u8,
    answers: Rc<RefCell<HttpAnswers>>,
    last_event: Instant,
    front_timeout: TimeoutContainer,
    frontend_timeout_duration: Duration,
    backend_timeout_duration: Duration,
    listener: Rc<RefCell<Listener>>,
}

impl Session {
    pub fn new(
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
    ) -> Self {
        let request_id = Ulid::generate();
        let mut front_timeout = TimeoutContainer::new_empty(request_timeout_duration);
        let state = if expect_proxy {
            trace!("starting in expect proxy state");
            gauge_add!("protocol.proxy.expect", 1);
            front_timeout.set(token);

            State::Expect(ExpectProxyProtocol::new(sock, token, request_id))
        } else {
            gauge_add!("protocol.http", 1);
            let session_address = sock.peer_addr().ok();
            let timeout = TimeoutContainer::new(request_timeout_duration, token);
            State::Http(Http::new(
                sock,
                token,
                request_id,
                pool.clone(),
                public_address,
                session_address,
                sticky_name.clone(),
                Protocol::HTTP,
                answers.clone(),
                timeout,
                frontend_timeout_duration,
                backend_timeout_duration,
                listener.clone(),
            ))
        };

        let metrics = SessionMetrics::new(Some(wait_time));
        let mut session = Session {
            backend: None,
            back_connected: BackendConnectionStatus::NotConnected,
            protocol: Some(state),
            proxy,
            frontend_token: token,
            pool,
            metrics,
            cluster_id: None,
            sticky_name,
            last_event: Instant::now(),
            front_timeout,
            listener_token: listener_token,
            connection_attempt: 0,
            answers,
            frontend_timeout_duration,
            backend_timeout_duration,
            listener,
        };

        session.front_readiness().interest = Ready::readable() | Ready::hup() | Ready::error();
        session
    }

    pub fn upgrade(&mut self) -> bool {
        debug!("HTTP::upgrade");
        let protocol = unwrap_msg!(self.protocol.take());

        match protocol {
            State::Http(mut http) => {
                debug!("switching to pipe");
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

                gauge_add!("protocol.http", -1);
                gauge_add!("protocol.ws", 1);
                gauge_add!("http.active_requests", -1);
                gauge_add!("websocket.active_requests", 1);
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
                    Protocol::HTTP,
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
                //pipe.set_cluster_id(self.cluster_id.clone());

                self.protocol = Some(State::WebSocket(pipe));
                true
            }
            State::Expect(expect) => {
                debug!("switching to HTTP");
                match expect
                    .addresses
                    .as_ref()
                    .map(|add| (add.destination(), add.source()))
                {
                    Some((Some(public_address), Some(client_address))) => {
                        let readiness = expect.readiness;
                        let mut http = Http::new(
                            expect.frontend,
                            expect.frontend_token,
                            expect.request_id,
                            self.pool.clone(),
                            public_address,
                            Some(client_address),
                            self.sticky_name.clone(),
                            Protocol::HTTP,
                            self.answers.clone(),
                            self.front_timeout.take(),
                            self.frontend_timeout_duration,
                            self.backend_timeout_duration,
                            self.listener.clone(),
                        );
                        http.front_readiness.event = readiness.event;

                        gauge_add!("protocol.proxy.expect", -1);
                        gauge_add!("protocol.http", 1);
                        self.protocol = Some(State::Http(http));
                        true
                    }
                    _ => {
                        self.protocol = Some(State::Expect(expect));
                        false
                    }
                }
            }
            _ => {
                self.protocol = Some(protocol);
                true
            }
        }
    }

    pub fn set_answer(&mut self, answer: DefaultAnswerStatus, buf: Option<Rc<Vec<u8>>>) {
        if let State::Http(ref mut http) = *unwrap_msg!(self.protocol.as_mut()) {
            http.set_answer(answer, buf);
        }
    }

    pub fn http(&self) -> Option<&Http<TcpStream, Listener>> {
        self.protocol.as_ref().and_then(|protocol| match protocol {
            State::Http(ref http) => Some(http),
            _ => None,
        })
    }

    pub fn http_mut(&mut self) -> Option<&mut Http<TcpStream, Listener>> {
        self.protocol.as_mut().and_then(|protocol| match protocol {
            State::Http(ref mut http) => Some(http),
            _ => None,
        })
    }

    fn front_hup(&mut self) -> SessionResult {
        match *unwrap_msg!(self.protocol.as_mut()) {
            State::Http(ref mut http) => http.front_hup(),
            State::WebSocket(ref mut pipe) => pipe.front_hup(&mut self.metrics),
            _ => SessionResult::CloseSession,
        }
    }

    fn back_hup(&mut self) -> SessionResult {
        match *unwrap_msg!(self.protocol.as_mut()) {
            State::Http(ref mut http) => http.back_hup(),
            State::WebSocket(ref mut pipe) => pipe.back_hup(&mut self.metrics),
            _ => SessionResult::CloseSession,
        }
    }

    fn log_context(&self) -> String {
        match *unwrap_msg!(self.protocol.as_ref()) {
            State::Http(ref http) => {
                if let Some(ref cluster_id) = http.cluster_id {
                    format!("{}\t{}\t", http.request_id, cluster_id)
                } else {
                    format!("{}\t-\t", http.request_id)
                }
            }
            _ => "".to_string(),
        }
    }

    /// Read content from the frontend
    fn readable(&mut self) -> SessionResult {
        let (upgrade, result) = match *unwrap_msg!(self.protocol.as_mut()) {
            State::Expect(ref mut expect) => {
                if !self.front_timeout.reset() {
                    error!("could not reset front timeout (HTTP upgrading from expect)");
                }
                expect.readable(&mut self.metrics)
            }
            State::Http(ref mut http) => {
                (ProtocolResult::Continue, http.readable(&mut self.metrics))
            }
            State::WebSocket(ref mut pipe) => {
                (ProtocolResult::Continue, pipe.readable(&mut self.metrics))
            }
        };

        if upgrade == ProtocolResult::Continue {
            return result;
        }

        if self.upgrade() {
            return match *unwrap_msg!(self.protocol.as_mut()) {
                State::Http(ref mut http) => http.readable(&mut self.metrics),
                _ => result,
            };
        }

        // currently, only happens in expect proxy protocol with AF_UNSPEC address
        //error!("failed protocol upgrade");
        SessionResult::CloseSession
    }

    // Forward content to the frontend
    fn writable(&mut self) -> SessionResult {
        match *unwrap_msg!(self.protocol.as_mut()) {
            State::Http(ref mut http) => http.writable(&mut self.metrics),
            State::WebSocket(ref mut pipe) => pipe.writable(&mut self.metrics),
            State::Expect(_) => SessionResult::CloseSession,
        }
    }

    // Forward content to cluster
    fn back_writable(&mut self) -> SessionResult {
        match *unwrap_msg!(self.protocol.as_mut()) {
            State::Http(ref mut http) => http.back_writable(&mut self.metrics),
            State::WebSocket(ref mut pipe) => pipe.back_writable(&mut self.metrics),
            State::Expect(_) => SessionResult::CloseSession,
        }
    }

    // Read content from cluster
    fn back_readable(&mut self) -> SessionResult {
        let (upgrade, result) = match *unwrap_msg!(self.protocol.as_mut()) {
            State::Http(ref mut http) => http.back_readable(&mut self.metrics),
            State::WebSocket(ref mut pipe) => (
                ProtocolResult::Continue,
                pipe.back_readable(&mut self.metrics),
            ),
            State::Expect(_) => return SessionResult::CloseSession,
        };

        if upgrade == ProtocolResult::Continue {
            return result;
        }

        if self.upgrade() {
            return match *unwrap_msg!(self.protocol.as_mut()) {
                State::WebSocket(ref mut pipe) => pipe.back_readable(&mut self.metrics),
                _ => result,
            };
        }

        error!("failed protocol upgrade");
        SessionResult::CloseSession
    }

    fn front_socket(&self) -> &TcpStream {
        match *unwrap_msg!(self.protocol.as_ref()) {
            State::Http(ref http) => http.front_socket(),
            State::WebSocket(ref pipe) => pipe.front_socket(),
            State::Expect(ref expect) => expect.front_socket(),
        }
    }

    fn back_socket_mut(&mut self) -> Option<&mut TcpStream> {
        match *unwrap_msg!(self.protocol.as_mut()) {
            State::Http(ref mut http) => http.back_socket_mut(),
            State::WebSocket(ref mut pipe) => pipe.back_socket_mut(),
            State::Expect(_) => None,
        }
    }

    fn back_token(&self) -> Option<Token> {
        match *unwrap_msg!(self.protocol.as_ref()) {
            State::Http(ref http) => http.back_token(),
            State::WebSocket(ref pipe) => pipe.back_token(),
            State::Expect(_) => None,
        }
    }

    fn set_back_socket(&mut self, socket: TcpStream) {
        match *unwrap_msg!(self.protocol.as_mut()) {
            State::Http(ref mut http) => http.set_back_socket(socket, self.backend.clone()),
            // not passing it here since we should already have a connection available
            State::WebSocket(_) => {}
            State::Expect(_) => {}
        }
    }

    fn set_back_token(&mut self, token: Token) {
        match *unwrap_msg!(self.protocol.as_mut()) {
            State::Http(ref mut http) => http.set_back_token(token),
            State::WebSocket(ref mut pipe) => pipe.set_back_token(token),
            State::Expect(_) => {}
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

                //successful connection, reset failure counter
                backend.failures = 0;
                backend.active_requests += 1;
                backend.retry_policy.succeed();
            }
        }
    }

    fn metrics(&mut self) -> &mut SessionMetrics {
        &mut self.metrics
    }

    fn remove_backend(&mut self) {
        /*debug!("{}\tPROXY [{} -> {}] CLOSED BACKEND",
          self.http().map(|h| h.log_ctx.clone()).unwrap_or_else(|| "".to_string()), self.frontend_token.0,
          self.back_token().map(|t| format!("{}", t.0)).unwrap_or_else(|| "-".to_string()));
        */

        if let Some(backend) = self.backend.take() {
            if let Some(h) = self.http_mut() {
                h.clear_back_token();
            }

            backend.borrow_mut().dec_connections();
        }
    }

    fn front_readiness(&mut self) -> &mut Readiness {
        match *unwrap_msg!(self.protocol.as_mut()) {
            State::Http(ref mut http) => &mut http.front_readiness,
            State::WebSocket(ref mut pipe) => &mut pipe.front_readiness,
            State::Expect(ref mut expect) => &mut expect.readiness,
        }
    }

    fn back_readiness(&mut self) -> Option<&mut Readiness> {
        match *unwrap_msg!(self.protocol.as_mut()) {
            State::Http(ref mut http) => Some(&mut http.back_readiness),
            State::WebSocket(ref mut pipe) => Some(&mut pipe.back_readiness),
            _ => None,
        }
    }

    fn fail_backend_connection(&mut self) {
        if let Some(backend) = &self.backend {
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
        }
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
                h.cancel_backend_timeout();
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
                    Ok(BackendConnectAction::Reuse) => {}
                    Ok(BackendConnectAction::New) | Ok(BackendConnectAction::Replace) => {
                        // we must wait for an event
                        return SessionResult::Continue;
                    }
                    Err(connection_error) => error!("{}", connection_error),
                }
            } else {
                self.metrics().backend_connected();
                self.reset_connection_attempt();
                self.set_back_connected(BackendConnectionStatus::Connected);
                // we might get an early response from the backend, so we want to look
                // at readable events
                if let Some(r) = self.back_readiness() {
                    r.interest.insert(Ready::readable());
                }
            }
        }

        if self.front_readiness().event.is_hup() {
            let order = self.front_hup();
            if order != SessionResult::CloseSession {
                self.front_readiness().event.remove(Ready::hup());
            }
            return order;
        }

        let token = self.frontend_token;
        while counter < max_loop_iterations {
            let front_interest = self.front_readiness().filter_interest();
            let back_interest = self
                .back_readiness()
                .map(|r| r.interest & r.event)
                .unwrap_or_else(Ready::empty);

            trace!(
                "PROXY\t{} {:?} {:?} -> {:?}",
                self.log_context(),
                token,
                self.front_readiness().clone(),
                self.back_readiness()
            );

            if front_interest.is_empty() && back_interest.is_empty() {
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
                            Err(connection_error) => error!("{}", connection_error),
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
                        continue;
                    }
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
            incr!("http.infinite_loop.error");

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

    //FIXME: check the token passed as argument
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
                            error!("error shutting down back socket({:?}): {:?}", sock, e);
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
            }
        }
    }

    fn check_circuit_breaker(&mut self) -> Result<(), ConnectionError> {
        if self.connection_attempt >= CONN_RETRIES {
            error!("{} max connection attempt reached", self.log_context());
            self.set_answer(DefaultAnswerStatus::Answer503, None);
            return Err(ConnectionError::NoBackendAvailable(
                self.cluster_id.to_owned(),
            ));
        }
        Ok(())
    }

    fn check_backend_connection(&mut self) -> bool {
        let is_valid_backend_socket = self
            .http_mut()
            .map(|h| h.is_valid_backend_socket())
            .unwrap_or(false);

        if !is_valid_backend_socket {
            return false;
        }

        //matched on keepalive
        self.metrics.backend_id = self.backend.as_ref().map(|i| i.borrow().backend_id.clone());

        self.metrics.backend_start();
        self.http_mut().map(|h| {
            h.backend_data
                .as_mut()
                .map(|b| b.borrow_mut().active_requests += 1)
        });

        true
    }

    // -> host, path, method
    pub fn extract_route(&self) -> Result<(&str, &str, &Method), ConnectionError> {
        let host = self
            .http()
            .and_then(|http| http.request_state.as_ref())
            .and_then(|request_state| request_state.get_host())
            .ok_or(ConnectionError::NoHostGiven)?;

        // redundant
        // we only keep host, but we need the request's hostname in frontend_from_request (calling twice hostname_and_port)
        let host: &str = match hostname_and_port(host.as_bytes()) {
            Ok((input, (hostname, port))) => {
                if input != &b""[..] {
                    return Err(ConnectionError::InvalidHost {
                        hostname: host.to_owned(),
                        message: "connect_to_backend: invalid remaining chars after hostname"
                            .to_owned(),
                    });
                }

                //FIXME: we should check that the port is right too

                if port == Some(&b"80"[..]) {
                    // it is alright to call from_utf8_unchecked,
                    // we already verified that there are only ascii
                    // chars in there
                    unsafe { from_utf8_unchecked(hostname) }
                } else {
                    host
                }
            }
            Err(parse_error) => {
                return Err(ConnectionError::InvalidHost {
                    hostname: host.to_owned(),
                    message: format!("Hostname parsing failed: {}", parse_error),
                });
            }
        };

        let request_line = self
            .http()
            .and_then(|http| http.request_state.as_ref())
            .and_then(|request_state| request_state.get_request_line())
            .ok_or(ConnectionError::NoRequestLineGiven)?;

        Ok((host, &request_line.uri, &request_line.method))
    }

    fn cluster_id_from_request(&mut self) -> Result<String, ConnectionError> {
        let (host, uri, method) = match self.extract_route() {
            Ok((h, u, m)) => (h, u, m),
            Err(e) => {
                self.set_answer(DefaultAnswerStatus::Answer400, None);
                return Err(e);
            }
        };

        let cluster_id_res = self
            .proxy
            .borrow()
            .listeners
            .get(&self.listener_token)
            .as_ref()
            .and_then(|listener| listener.borrow().frontend_from_request(host, uri, method));

        let cluster_id = match cluster_id_res {
            Some(Route::ClusterId(cluster_id)) => cluster_id,
            Some(Route::Deny) => {
                self.set_answer(DefaultAnswerStatus::Answer401, None);
                return Err(ConnectionError::Unauthorized);
            }
            None => {
                let no_host_error = ConnectionError::HostNotFound(host.to_owned());
                self.set_answer(DefaultAnswerStatus::Answer404, None);
                return Err(no_host_error);
            }
        };

        let front_should_redirect_https = self
            .proxy
            .borrow()
            .clusters
            .get(&cluster_id)
            .map(|cluster| cluster.https_redirect)
            .unwrap_or(false);

        if front_should_redirect_https {
            let answer = format!("HTTP/1.1 301 Moved Permanently\r\nContent-Length: 0\r\nLocation: https://{}{}\r\n\r\n", host, uri);
            self.set_answer(
                DefaultAnswerStatus::Answer301,
                Some(Rc::new(answer.into_bytes())),
            );
            return Err(ConnectionError::HttpsRedirect(cluster_id));
        }

        Ok(cluster_id)
    }

    pub fn backend_from_request(
        &mut self,
        cluster_id: &str,
        front_should_stick: bool,
    ) -> Result<TcpStream, ConnectionError> {
        let sticky_session = self
            .http()
            .and_then(|http| http.request_state.as_ref())
            .and_then(|request_state| request_state.get_sticky_session());

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
            Ok((b, c)) => (b, c),
            Err(e) => {
                self.set_answer(DefaultAnswerStatus::Answer503, None);
                return Err(e);
            }
        };

        let listener_token = self.listener_token;
        if front_should_stick {
            let sticky_name = &self.proxy.borrow().listeners[&listener_token]
                .borrow()
                .config
                .sticky_name
                .to_string();

            if let Some(http) = self.http_mut() {
                // stick backend to session
                http.sticky_session = Some(StickySession::new(
                    backend
                        .borrow()
                        .sticky_id
                        .clone()
                        .unwrap_or_else(|| backend.borrow().backend_id.clone()),
                ));

                // stick session to listener (how? why?)
                http.sticky_name = sticky_name.to_string();
            }
        }

        self.metrics.backend_id = Some(backend.borrow().backend_id.clone());
        self.metrics.backend_start();
        if let Some(http) = self.http_mut() {
            http.set_backend_id(backend.borrow().backend_id.clone());
        }

        self.backend = Some(backend);
        Ok(conn)
    }

    fn connect_to_backend(
        &mut self,
        session_rc: Rc<RefCell<dyn ProxySession>>,
    ) -> Result<BackendConnectAction, ConnectionError> {
        let old_cluster_id = self.http().and_then(|http| http.cluster_id.clone());
        let old_back_token = self.back_token();

        self.check_circuit_breaker()?;

        let cluster_id = self.cluster_id_from_request()?;

        // check if we can reuse the backend connection
        if (self.http().and_then(|h| h.cluster_id.as_ref()) == Some(&cluster_id))
            && self.back_connected == BackendConnectionStatus::Connected
        {
            let has_backend = self
                .backend
                .as_ref()
                .map(|backend| {
                    let backend = backend.borrow();
                    self.proxy
                        .borrow()
                        .backends
                        .borrow()
                        .has_backend(&cluster_id, &backend)
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
                .map(|listener| listener.borrow().config.connect_timeout)
                .unwrap(),
        ));

        self.back_connected = BackendConnectionStatus::Connecting(Instant::now());

        match old_back_token {
            Some(back_token) => {
                self.set_back_token(back_token);
                if let Err(e) = self.proxy.borrow().registry.register(
                    &mut socket,
                    back_token,
                    Interest::READABLE | Interest::WRITABLE,
                ) {
                    error!("error registering back socket({:?}): {:?}", socket, e);
                }

                self.set_back_socket(socket);
                if let Some(http) = self.http_mut() {
                    http.set_back_timeout(connect_timeout);
                }

                Ok(BackendConnectAction::Replace)
            }
            None => {
                let not_enough_memory = {
                    let proxy = self.proxy.borrow();
                    let sessions = proxy.sessions.borrow();

                    sessions.slab.len() >= sessions.slab_capacity()
                };

                if not_enough_memory {
                    error!("not enough memory, cannot connect to backend");
                    self.set_answer(DefaultAnswerStatus::Answer503, None);
                    return Err(ConnectionError::TooManyConnections(
                        self.cluster_id.to_owned(),
                    ));
                }

                let back_token = {
                    let proxy = self.proxy.borrow();
                    let mut s = proxy.sessions.borrow_mut();
                    let entry = s.slab.vacant_entry();
                    let back_token = Token(entry.key());
                    let _entry = entry.insert(session_rc);
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
                if let Some(http) = self.http_mut() {
                    http.set_back_timeout(connect_timeout);
                }

                Ok(BackendConnectAction::New)
            }
        }
    }
}

impl ProxySession for Session {
    fn close(&mut self) {
        self.metrics.service_stop();
        self.cancel_timeouts();
        if let Err(e) = self.front_socket().shutdown(Shutdown::Both) {
            if e.kind() != ErrorKind::NotConnected {
                error!(
                    "error shutting down front socket({:?}): {:?}",
                    self.front_socket(),
                    e
                );
            }
        }

        self.close_backend();

        match self.protocol {
            Some(State::Http(ref mut http)) => {
                //if the state was initial, the connection was already reset
                if http.request_state != Some(RequestState::Initial) {
                    gauge_add!("http.active_requests", -1);

                    if let Some(b) = http.backend_data.as_mut() {
                        let mut backend = b.borrow_mut();
                        backend.active_requests = backend.active_requests.saturating_sub(1);
                    }
                }
                gauge_add!("protocol.http", -1)
            }
            Some(State::WebSocket(_)) => {
                if let Some(b) = &self.backend {
                    let mut backend = b.borrow_mut();
                    backend.active_requests = backend.active_requests.saturating_sub(1);
                }
                gauge_add!("protocol.ws", -1)
            }
            Some(State::Expect(_)) => gauge_add!("protocol.proxy.expect", -1),
            None => {}
        }

        let fd = self.front_socket().as_raw_fd();
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

    fn timeout(&mut self, token: Token) {
        let res = match *unwrap_msg!(self.protocol.as_mut()) {
            State::Expect(_) => {
                if token == self.frontend_token {
                    self.front_timeout.triggered();
                }
                SessionResult::CloseSession
            }
            State::Http(ref mut http) => http.timeout(token, &mut self.metrics),
            State::WebSocket(ref mut pipe) => pipe.timeout(token, &mut self.metrics),
        };

        if res == SessionResult::CloseSession {
            self.close();
        }
    }

    fn protocol(&self) -> Protocol {
        Protocol::HTTP
    }

    /// this seems to update session readiness but it does not process events
    fn process_events(&mut self, token: Token, events: Ready) {
        trace!(
            "token {:?} got event {}",
            token,
            super::ready_to_string(events)
        );
        self.last_event = Instant::now();
        self.metrics.wait_start();

        if self.frontend_token == token {
            self.front_readiness().event |= events;
        } else if self.back_token() == Some(token) {
            if let Some(readiness) = self.back_readiness() {
                readiness.event |= events;
            }
        }
    }

    fn ready(&mut self, session: Rc<RefCell<dyn ProxySession>>) {
        self.metrics().service_start();

        match self.ready_inner(session) {
            SessionResult::CloseSession => self.close(),
            SessionResult::CloseBackend => self.close_backend(),
            _ => (),
        }

        self.metrics().service_stop();
    }

    fn shutting_down(&mut self) {
        let res = match &mut self.protocol {
            Some(State::Http(h)) => h.shutting_down(),
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
            Some(State::Expect(_)) => String::from("Expect"),
            Some(State::Http(h)) => h.print_state("HTTP"),
            Some(State::WebSocket(_)) => String::from("WS"),
            None => String::from("None"),
        };

        let rf = match *unwrap_msg!(self.protocol.as_ref()) {
            State::Expect(ref expect) => &expect.readiness,
            State::Http(ref http) => &http.front_readiness,
            State::WebSocket(ref pipe) => &pipe.front_readiness,
        };
        let rb = match *unwrap_msg!(self.protocol.as_ref()) {
            State::Http(ref http) => Some(&http.back_readiness),
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

pub type Hostname = String;

pub struct Listener {
    listener: Option<TcpListener>,
    pub address: SocketAddr,
    fronts: Router,
    answers: Rc<RefCell<HttpAnswers>>,
    config: HttpListener,
    pub token: Token,
    pub active: bool,
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

pub struct Proxy {
    listeners: HashMap<Token, Rc<RefCell<Listener>>>,
    backends: Rc<RefCell<BackendMap>>,
    clusters: HashMap<ClusterId, Cluster>,
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

    pub fn add_listener(&mut self, config: HttpListener, token: Token) -> Option<Token> {
        match self.listeners.entry(token) {
            Entry::Vacant(entry) => {
                entry.insert(Rc::new(RefCell::new(Listener::new(config, token))));
                Some(token)
            }
            _ => None,
        }
    }

    pub fn get_listener(&self, token: &Token) -> Option<Rc<RefCell<Listener>>> {
        self.listeners.get(token).map(Clone::clone)
    }

    pub fn remove_listener(&mut self, address: SocketAddr) -> bool {
        let len = self.listeners.len();

        self.listeners.retain(|_, l| l.borrow().address != address);
        self.listeners.len() < len
    }

    pub fn activate_listener(
        &self,
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
            .iter()
            .filter_map(|(_, listener)| {
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
            .and_then(|listener| {
                let mut owned = listener.borrow_mut();

                owned
                    .listener
                    .take()
                    .map(|listener| (owned.token, listener))
            })
    }

    pub fn add_cluster(&mut self, cluster: Cluster) {
        if let Some(answer_503) = &cluster.answer_503 {
            for listener in self.listeners.values() {
                listener
                    .borrow()
                    .answers
                    .borrow_mut()
                    .add_custom_answer(&cluster.cluster_id, answer_503);
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

impl Listener {
    pub fn new(config: HttpListener, token: Token) -> Listener {
        Listener {
            listener: None,
            address: config.address,
            fronts: Router::new(),
            answers: Rc::new(RefCell::new(HttpAnswers::new(
                &config.answer_404,
                &config.answer_503,
            ))),
            config,
            token,
            active: false,
            tags: BTreeMap::new(),
        }
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
                error!("error registering listener socket({:?}): {:?}", sock, e);
            }
        } else {
            return None;
        }

        self.listener = listener;
        self.active = true;
        Some(self.token)
    }

    // TODO: return Result with context
    pub fn add_http_front(&mut self, http_front: HttpFrontend) -> Result<(), String> {
        if self.fronts.add_http_front(http_front) {
            Ok(())
        } else {
            Err(String::from("could not add HTTP front"))
        }
    }

    // TODO: return Result with context
    pub fn remove_http_front(&mut self, http_front: HttpFrontend) -> Result<(), String> {
        debug!("removing http_front {:?}", http_front);
        //FIXME: proper error reporting
        if !self.fronts.remove_http_front(http_front) {
            return Err(String::from("could not remove HTTP front"));
        }
        Ok(())
    }

    // TODO: return Result with context
    pub fn frontend_from_request(&self, host: &str, uri: &str, method: &Method) -> Option<Route> {
        // redundant
        // already called once in extract_route
        let host: &str = if let Ok((i, (hostname, _))) = hostname_and_port(host.as_bytes()) {
            if i != &b""[..] {
                error!(
                    "frontend_from_request: invalid remaining chars after hostname. Host: {}",
                    host
                );
                return None;
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

impl ProxyConfiguration<Session> for Proxy {
    fn notify(&mut self, message: ProxyRequest) -> ProxyResponse {
        // ToDo temporary
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
            ProxyRequestOrder::AddHttpFrontend(front) => {
                debug!("{} add front {:?}", message.id, front);
                if let Some(listener) = self
                    .listeners
                    .values()
                    .find(|l| l.borrow().address == front.address)
                {
                    let mut owned = listener.borrow_mut();

                    let hostname = front.hostname.to_owned();
                    let tags = front.tags.to_owned();

                    match owned.add_http_front(front) {
                        Ok(_) => {
                            owned.set_tags(hostname, tags);

                            ProxyResponse::ok(message.id)
                        }
                        Err(err) => ProxyResponse::error(message.id, err),
                    }
                } else {
                    ProxyResponse::error(
                        message.id,
                        format!("no HTTP listener found for front: {:?}", front),
                    )

                    // let (listener, tokens) = Listener::new(HttpListener::default(), event_loop,
                    //  self.pool.clone(), None, token: Token) -> (Listener,HashSet<Token>
                }
            }
            ProxyRequestOrder::RemoveHttpFrontend(front) => {
                debug!("{} front {:?}", message.id, front);
                if let Some(listener) = self
                    .listeners
                    .values()
                    .find(|l| l.borrow().address == front.address)
                {
                    let mut owned = listener.borrow_mut();

                    let hostname = front.hostname.to_owned();

                    match owned.remove_http_front(front) {
                        Ok(_) => {
                            owned.set_tags(hostname, None);
                            ProxyResponse::ok(message.id)
                        }
                        Err(err) => ProxyResponse::error(message.id, err),
                    }
                } else {
                    ProxyResponse::error(
                        message.id,
                        "trying to remove front from non existing listener",
                    )
                }
            }
            ProxyRequestOrder::RemoveListener(remove) => {
                debug!("removing HTTP listener at address {:?}", remove.address);
                if !self.remove_listener(remove.address) {
                    ProxyResponse::error(
                        message.id,
                        format!("no HTTP listener to remove at address {:?}", remove.address),
                    )
                } else {
                    ProxyResponse::ok(message.id)
                }
            }
            ProxyRequestOrder::SoftStop => {
                info!("{} processing soft shutdown", message.id);
                let listeners: HashMap<_, _> = self.listeners.drain().collect();
                for (_, l) in listeners.iter() {
                    if let Some(mut sock) = l.borrow_mut().listener.take() {
                        if let Err(e) = self.registry.deregister(&mut sock) {
                            error!("error deregistering listen socket({:?}): {:?}", sock, e);
                        }
                    }
                }
                ProxyResponse::processing(message.id)
            }
            ProxyRequestOrder::HardStop => {
                info!("{} hard shutdown", message.id);
                let mut listeners: HashMap<_, _> = self.listeners.drain().collect();
                for (_, l) in listeners.drain() {
                    if let Some(mut sock) = l.borrow_mut().listener.take() {
                        if let Err(e) = self.registry.deregister(&mut sock) {
                            error!("error deregistering listen socket({:?}): {:?}", sock, e);
                        }
                    }
                }
                ProxyResponse::processing(message.id)
            }
            ProxyRequestOrder::Status => {
                debug!("{} status", message.id);
                ProxyResponse::ok(message.id)
            }
            ProxyRequestOrder::Logging(logging_filter) => {
                info!(
                    "{} changing logging filter to {}",
                    message.id, logging_filter
                );
                logging::LOGGER.with(|l| {
                    let directives = logging::parse_logging_spec(&logging_filter);
                    l.borrow_mut().set_directives(directives);
                });
                ProxyResponse::ok(message.id)
            }
            command => {
                debug!(
                    "{} unsupported message for HTTP proxy, ignoring: {:?}",
                    message.id, command
                );
                ProxyResponse::error(message.id, "unsupported message")
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
            .map(Clone::clone)
            .ok_or_else(|| AcceptError::IoError)?;

        if let Err(e) = frontend_sock.set_nodelay(true) {
            error!(
                "error setting nodelay on front socket({:?}): {:?}",
                frontend_sock, e
            );
        }
        let mut session_manager = self.sessions.borrow_mut();
        let session_entry = session_manager.slab.vacant_entry();
        let session_token = Token(session_entry.key());
        let owned = listener.borrow();

        if let Err(register_error) = self.registry.register(
            &mut frontend_sock,
            session_token,
            Interest::READABLE | Interest::WRITABLE,
        ) {
            error!(
                "error registering listen socket({:?}): {:?}",
                frontend_sock, register_error
            );
            return Err(AcceptError::RegisterError);
        }

        let session = Session::new(
            frontend_sock,
            session_token,
            Rc::downgrade(&self.pool),
            proxy,
            owned.config.public_address.unwrap_or(owned.config.address),
            owned.config.expect_proxy,
            owned.config.sticky_name.clone(),
            owned.answers.clone(),
            owned.token,
            wait_time,
            Duration::seconds(owned.config.front_timeout as i64),
            Duration::seconds(owned.config.back_timeout as i64),
            Duration::seconds(owned.config.request_timeout as i64),
            listener.clone(),
        );

        let session = Rc::new(RefCell::new(session));
        session_entry.insert(session);

        session_manager.incr();
        Ok(())
    }
}

/// This is not directly used by Sōzu but is available for example and testing purposes
pub fn start(
    config: HttpListener,
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
        info!("taking token {:?} for channel", entry.key());
        entry.insert(Rc::new(RefCell::new(ListenSession {
            protocol: Protocol::HTTPListen,
        })));
    }
    {
        let entry = sessions.vacant_entry();
        info!("taking token {:?} for timer", entry.key());
        entry.insert(Rc::new(RefCell::new(ListenSession {
            protocol: Protocol::HTTPListen,
        })));
    }
    {
        let entry = sessions.vacant_entry();
        info!("taking token {:?} for metrics", entry.key());
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

    let address = config.address;
    let sessions = SessionManager::new(sessions, max_buffers);
    let registry = event_loop
        .registry()
        .try_clone()
        .with_context(|| "Failed at creating a registry")?;
    let mut proxy = Proxy::new(registry, sessions.clone(), pool.clone(), backends.clone());
    let _ = proxy.add_listener(config, token);
    let _ = proxy.activate_listener(&address, None);
    let (scm_server, scm_client) =
        UnixStream::pair().with_context(|| "Failed at creating scm stream sockets")?;
    let client_scm_socket =
        ScmSocket::new(scm_client.into_raw_fd()).with_context(|| "Could not create scm socket")?;
    let server_scm_socket =
        ScmSocket::new(scm_server.as_raw_fd()).with_context(|| "Could not create scm socket")?;

    if let Err(e) = client_scm_socket.send_listeners(&Listeners {
        http: Vec::new(),
        tls: Vec::new(),
        tcp: Vec::new(),
    }) {
        error!("error sending empty listeners: {:?}", e);
    }

    let server_config = server::ServerConfig {
        max_connections: max_buffers,
        ..Default::default()
    };

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

    println!("starting event loop");
    server.run();
    println!("ending event loop");
    Ok(())
}

#[cfg(test)]
mod tests {
    extern crate tiny_http;
    use super::*;
    use crate::sozu_command::channel::Channel;
    use crate::sozu_command::proxy::{
        Backend, HttpFrontend, HttpListener, LoadBalancingAlgorithms, LoadBalancingParams,
        PathRule, ProxyRequest, ProxyRequestOrder, Route, RulePosition,
    };
    use std::io::{Read, Write};
    use std::net::SocketAddr;
    use std::net::TcpStream;
    use std::str::FromStr;
    use std::sync::{Arc, Barrier};
    use std::time::Duration;
    use std::{str, thread};

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
    fn mi() {
        setup_test_logger!();
        let barrier = Arc::new(Barrier::new(2));
        start_server(1025, barrier.clone());
        barrier.wait();

        let address: SocketAddr =
            FromStr::from_str("127.0.0.1:1024").expect("could not parse address");
        let config = HttpListener {
            address,
            ..Default::default()
        };

        let (mut command, channel) =
            Channel::generate(1000, 10000).expect("should create a channel");
        let _jg = thread::spawn(move || {
            setup_test_logger!();
            start(config, channel, 10, 16384).expect("could not start the http server");
        });

        let front = HttpFrontend {
            route: Route::ClusterId(String::from("cluster_1")),
            address: "127.0.0.1:1024".parse().unwrap(),
            hostname: String::from("localhost"),
            path: PathRule::Prefix(String::from("/")),
            method: None,
            position: RulePosition::Tree,
            tags: None,
        };
        command
            .write_message(&ProxyRequest {
                id: String::from("ID_ABCD"),
                order: ProxyRequestOrder::AddHttpFrontend(front),
            })
            .unwrap();
        let backend = Backend {
            cluster_id: String::from("cluster_1"),
            backend_id: String::from("cluster_1-0"),
            address: "127.0.0.1:1025".parse().unwrap(),
            load_balancing_parameters: Some(LoadBalancingParams::default()),
            sticky_id: None,
            backup: None,
        };
        command
            .write_message(&ProxyRequest {
                id: String::from("ID_EFGH"),
                order: ProxyRequestOrder::AddBackend(backend),
            })
            .unwrap();

        println!("test received: {:?}", command.read_message());
        println!("test received: {:?}", command.read_message());

        let mut client = TcpStream::connect(("127.0.0.1", 1024)).expect("could not parse address");

        // 5 seconds of timeout
        client.set_read_timeout(Some(Duration::new(1, 0))).unwrap();
        let w = client
            .write(&b"GET / HTTP/1.1\r\nHost: localhost:1024\r\nConnection: Close\r\n\r\n"[..]);
        println!("http client write: {:?}", w);

        barrier.wait();
        let mut buffer = [0; 4096];
        let mut index = 0;

        loop {
            assert!(index <= 191);
            if index == 191 {
                break;
            }

            let r = client.read(&mut buffer[index..]);
            println!("http client read: {:?}", r);
            match r {
                Err(e) => assert!(false, "client request should not fail. Error: {:?}", e),
                Ok(sz) => {
                    index += sz;
                }
            }
        }
        println!(
            "Response: {}",
            str::from_utf8(&buffer[..index]).expect("could not make string from buffer")
        );
    }

    #[test]
    fn keep_alive() {
        setup_test_logger!();
        let barrier = Arc::new(Barrier::new(2));
        start_server(1028, barrier.clone());
        barrier.wait();

        let address: SocketAddr =
            FromStr::from_str("127.0.0.1:1031").expect("could not parse address");
        let config = HttpListener {
            address,
            ..Default::default()
        };

        let (mut command, channel) =
            Channel::generate(1000, 10000).expect("should create a channel");

        let _jg = thread::spawn(move || {
            setup_test_logger!();
            start(config, channel, 10, 16384).expect("could not start the http server");
        });

        let front = HttpFrontend {
            route: Route::ClusterId(String::from("cluster_1")),
            address: "127.0.0.1:1031".parse().unwrap(),
            hostname: String::from("localhost"),
            path: PathRule::Prefix(String::from("/")),
            method: None,
            position: RulePosition::Tree,
            tags: None,
        };
        command
            .write_message(&ProxyRequest {
                id: String::from("ID_ABCD"),
                order: ProxyRequestOrder::AddHttpFrontend(front),
            })
            .unwrap();
        let backend = Backend {
            cluster_id: String::from("cluster_1"),
            backend_id: String::from("cluster_1-0"),
            address: "127.0.0.1:1028".parse().unwrap(),
            load_balancing_parameters: Some(LoadBalancingParams::default()),
            sticky_id: None,
            backup: None,
        };
        command
            .write_message(&ProxyRequest {
                id: String::from("ID_EFGH"),
                order: ProxyRequestOrder::AddBackend(backend),
            })
            .unwrap();

        println!("test received: {:?}", command.read_message());
        println!("test received: {:?}", command.read_message());

        let mut client = TcpStream::connect(("127.0.0.1", 1031)).expect("could not parse address");
        // 5 seconds of timeout
        client.set_read_timeout(Some(Duration::new(5, 0))).unwrap();

        let w = client
            .write(&b"GET / HTTP/1.1\r\nHost: localhost:1031\r\n\r\n"[..])
            .unwrap();
        println!("http client write: {:?}", w);
        barrier.wait();

        let mut buffer = [0; 4096];
        let mut index = 0;

        loop {
            assert!(index <= 191);
            if index == 191 {
                break;
            }

            let r = client.read(&mut buffer[index..]);
            println!("http client read: {:?}", r);
            match r {
                Err(e) => assert!(false, "client request should not fail. Error: {:?}", e),
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
        let w2 = client.write(&b"GET / HTTP/1.1\r\nHost: localhost:1031\r\n\r\n"[..]);
        println!("http client write: {:?}", w2);
        barrier.wait();

        let mut buffer2 = [0; 4096];
        let mut index = 0;

        loop {
            assert!(index <= 191);
            if index == 191 {
                break;
            }

            let r2 = client.read(&mut buffer2[index..]);
            println!("http client read: {:?}", r2);
            match r2 {
                Err(e) => assert!(false, "client request should not fail. Error: {:?}", e),
                Ok(sz) => {
                    index += sz;
                }
            }
        }
        println!(
            "Response: {}",
            str::from_utf8(&buffer2[..index]).expect("could not make string from buffer")
        );
    }

    #[test]
    fn https_redirect() {
        setup_test_logger!();
        let address: SocketAddr =
            FromStr::from_str("127.0.0.1:1041").expect("could not parse address");
        let config = HttpListener {
            address,
            ..Default::default()
        };

        let (mut command, channel) =
            Channel::generate(1000, 10000).expect("should create a channel");
        let _jg = thread::spawn(move || {
            setup_test_logger!();
            start(config, channel, 10, 16384).expect("could not start the http server");
        });

        let cluster = Cluster {
            cluster_id: String::from("cluster_1"),
            sticky_session: false,
            https_redirect: true,
            proxy_protocol: None,
            load_balancing: LoadBalancingAlgorithms::default(),
            load_metric: None,
            answer_503: None,
        };
        command
            .write_message(&ProxyRequest {
                id: String::from("ID_ABCD"),
                order: ProxyRequestOrder::AddCluster(cluster),
            })
            .unwrap();
        let front = HttpFrontend {
            route: Route::ClusterId(String::from("cluster_1")),
            address: "127.0.0.1:1041".parse().unwrap(),
            hostname: String::from("localhost"),
            path: PathRule::Prefix(String::from("/")),
            method: None,
            position: RulePosition::Tree,
            tags: None,
        };
        command
            .write_message(&ProxyRequest {
                id: String::from("ID_EFGH"),
                order: ProxyRequestOrder::AddHttpFrontend(front),
            })
            .unwrap();
        let backend = Backend {
            cluster_id: String::from("cluster_1"),
            backend_id: String::from("cluster_1-0"),
            address: "127.0.0.1:1040".parse().unwrap(),
            load_balancing_parameters: Some(LoadBalancingParams::default()),
            sticky_id: None,
            backup: None,
        };
        command
            .write_message(&ProxyRequest {
                id: String::from("ID_IJKL"),
                order: ProxyRequestOrder::AddBackend(backend),
            })
            .unwrap();

        println!("test received: {:?}", command.read_message());
        println!("test received: {:?}", command.read_message());
        println!("test received: {:?}", command.read_message());

        let mut client = TcpStream::connect(("127.0.0.1", 1041)).expect("could not parse address");
        // 5 seconds of timeout
        client.set_read_timeout(Some(Duration::new(5, 0))).unwrap();

        let w = client.write(
            &b"GET /redirected?true HTTP/1.1\r\nHost: localhost\r\nConnection: Close\r\n\r\n"[..],
        );
        println!("http client write: {:?}", w);

        let expected_answer = "HTTP/1.1 301 Moved Permanently\r\nContent-Length: 0\r\nLocation: https://localhost/redirected?true\r\n\r\n";
        let mut buffer = [0; 4096];
        let mut index = 0;
        loop {
            assert!(index <= expected_answer.len());
            if index == expected_answer.len() {
                break;
            }

            let r = client.read(&mut buffer[index..]);
            println!("http client read: {:?}", r);
            match r {
                Err(e) => assert!(false, "client request should not fail. Error: {:?}", e),
                Ok(sz) => {
                    index += sz;
                }
            }
        }

        let answer = str::from_utf8(&buffer[..index]).expect("could not make string from buffer");
        println!("Response: {}", answer);
        assert_eq!(answer, expected_answer);
    }

    use self::tiny_http::{Response, Server};

    fn start_server(port: u16, barrier: Arc<Barrier>) {
        thread::spawn(move || {
            setup_test_logger!();
            let server =
                Server::http(&format!("127.0.0.1:{}", port)).expect("could not create server");
            info!("starting web server in port {}", port);
            barrier.wait();

            for request in server.incoming_requests() {
                info!(
                    "backend web server got request -> method: {:?}, url: {:?}, headers: {:?}",
                    request.method(),
                    request.url(),
                    request.headers()
                );

                let response = Response::from_string("hello world");
                request.respond(response).unwrap();
                info!("backend web server sent response");
                barrier.wait();
                info!("server session stopped");
            }

            println!("server on port {} closed", port);
        });
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
        fronts.add_http_front(HttpFrontend {
            route: Route::ClusterId(cluster_id1),
            address: "0.0.0.0:80".parse().unwrap(),
            hostname: "lolcatho.st".to_owned(),
            path: PathRule::Prefix(uri1),
            method: None,
            position: RulePosition::Tree,
            tags: None,
        });
        fronts.add_http_front(HttpFrontend {
            route: Route::ClusterId(cluster_id2),
            address: "0.0.0.0:80".parse().unwrap(),
            hostname: "lolcatho.st".to_owned(),
            path: PathRule::Prefix(uri2),
            method: None,
            position: RulePosition::Tree,
            tags: None,
        });
        fronts.add_http_front(HttpFrontend {
            route: Route::ClusterId(cluster_id3),
            address: "0.0.0.0:80".parse().unwrap(),
            hostname: "lolcatho.st".to_owned(),
            path: PathRule::Prefix(uri3),
            method: None,
            position: RulePosition::Tree,
            tags: None,
        });
        fronts.add_http_front(HttpFrontend {
            route: Route::ClusterId("cluster_1".to_owned()),
            address: "0.0.0.0:80".parse().unwrap(),
            hostname: "other.domain".to_owned(),
            path: PathRule::Prefix("/test".to_owned()),
            method: None,
            position: RulePosition::Tree,
            tags: None,
        });

        let address: SocketAddr =
            FromStr::from_str("127.0.0.1:1030").expect("could not parse address");
        let listener = Listener {
            listener: None,
            address,
            fronts,
            answers: Rc::new(RefCell::new(HttpAnswers::new(
                "HTTP/1.1 404 Not Found\r\n\r\n",
                "HTTP/1.1 503 Service Unavailable\r\n\r\n",
            ))),
            config: Default::default(),
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
            frontend1.expect("should find frontend"),
            Route::ClusterId("cluster_1".to_string())
        );
        assert_eq!(
            frontend2.expect("should find frontend"),
            Route::ClusterId("cluster_1".to_string())
        );
        assert_eq!(
            frontend3.expect("should find frontend"),
            Route::ClusterId("cluster_2".to_string())
        );
        assert_eq!(
            frontend4.expect("should find frontend"),
            Route::ClusterId("cluster_3".to_string())
        );
        assert_eq!(frontend5, None);
    }
}
