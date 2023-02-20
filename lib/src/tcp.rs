use std::{
    cell::RefCell,
    collections::{hash_map::Entry, BTreeMap, HashMap},
    io::{self, ErrorKind},
    net::{Shutdown, SocketAddr},
    os::unix::io::AsRawFd,
    rc::Rc,
};

use anyhow::{bail, Context};
use mio::{
    net::TcpListener as MioTcpListener,
    net::{TcpStream as MioTcpStream, UnixStream},
    unix::SourceFd,
    Interest, Poll, Registry, Token,
};
use rusty_ulid::Ulid;
use slab::Slab;
use time::{Duration, Instant};

use crate::{
    backends::BackendMap,
    pool::{Checkout, Pool},
    protocol::{
        proxy_protocol::{
            expect::ExpectProxyProtocol, relay::RelayProxyProtocol, send::SendProxyProtocol,
        },
        Pipe,
    },
    retry::RetryPolicy,
    server::{
        push_event, ListenSession, ListenToken, ProxyChannel, Server, SessionManager, CONN_RETRIES,
        TIMER,
    },
    socket::server_bind,
    sozu_command::{
        config::ProxyProtocolConfig,
        logging,
        proxy::{
            WorkerEvent, ProxyRequest, ProxyRequestOrder, ProxyResponse, TcpFrontend,
            TcpListenerConfig,
        },
        ready::Ready,
        scm_socket::ScmSocket,
        state::ClusterId,
    },
    timer::TimeoutContainer,
    AcceptError, Backend, BackendConnectAction, BackendConnectionStatus, ListenerHandler, Protocol,
    ProxyConfiguration, ProxySession, Readiness, SessionIsToBeClosed, SessionMetrics,
    SessionResult, StateMachineBuilder, StateResult,
};

StateMachineBuilder! {
    /// The various Stages of a TCP connection:
    ///
    /// 1. optional (ExpectProxyProtocol | SendProxyProtocol | RelayProxyProtocol)
    /// 2. Pipe
    enum TcpStateMachine {
        Pipe(Pipe<MioTcpStream, TcpListener>),
        SendProxyProtocol(SendProxyProtocol<MioTcpStream>),
        RelayProxyProtocol(RelayProxyProtocol<MioTcpStream>),
        ExpectProxyProtocol(ExpectProxyProtocol<MioTcpStream>),
    }
}

pub struct TcpSession {
    backend_buffer: Option<Checkout>,
    backend_connected: BackendConnectionStatus,
    backend_id: Option<String>,
    backend_token: Option<Token>,
    backend: Option<Rc<RefCell<Backend>>>,
    cluster_id: Option<String>,
    connection_attempt: u8,
    container_backend_timeout: TimeoutContainer,
    container_frontend_timeout: TimeoutContainer,
    frontend_address: Option<SocketAddr>,
    frontend_buffer: Option<Checkout>,
    frontend_token: Token,
    has_been_closed: SessionIsToBeClosed,
    last_event: Instant,
    listener: Rc<RefCell<TcpListener>>,
    metrics: SessionMetrics,
    proxy: Rc<RefCell<TcpProxy>>,
    request_id: Ulid,
    state: TcpStateMachine,
}

impl TcpSession {
    fn new(
        backend_buffer: Checkout,
        backend_id: Option<String>,
        cluster_id: Option<String>,
        configured_backend_timeout: Duration,
        configured_frontend_timeout: Duration,
        frontend_buffer: Checkout,
        frontend_token: Token,
        listener: Rc<RefCell<TcpListener>>,
        proxy_protocol: Option<ProxyProtocolConfig>,
        proxy: Rc<RefCell<TcpProxy>>,
        socket: MioTcpStream,
        wait_time: Duration,
    ) -> TcpSession {
        let frontend_address = socket.peer_addr().ok();
        let mut frontend_buffer_session = None;
        let mut backend_buffer_session = None;

        let request_id = Ulid::generate();

        let container_frontend_timeout =
            TimeoutContainer::new(configured_frontend_timeout, frontend_token);
        let container_backend_timeout = TimeoutContainer::new_empty(configured_backend_timeout);

        let state = match proxy_protocol {
            Some(ProxyProtocolConfig::RelayHeader) => {
                backend_buffer_session = Some(backend_buffer);
                gauge_add!("protocol.proxy.relay", 1);
                TcpStateMachine::RelayProxyProtocol(RelayProxyProtocol::new(
                    socket,
                    frontend_token,
                    request_id,
                    None,
                    frontend_buffer,
                ))
            }
            Some(ProxyProtocolConfig::ExpectHeader) => {
                frontend_buffer_session = Some(frontend_buffer);
                backend_buffer_session = Some(backend_buffer);
                gauge_add!("protocol.proxy.expect", 1);
                TcpStateMachine::ExpectProxyProtocol(ExpectProxyProtocol::new(
                    container_frontend_timeout.clone(),
                    socket,
                    frontend_token,
                    request_id,
                ))
            }
            Some(ProxyProtocolConfig::SendHeader) => {
                frontend_buffer_session = Some(frontend_buffer);
                backend_buffer_session = Some(backend_buffer);
                gauge_add!("protocol.proxy.send", 1);
                TcpStateMachine::SendProxyProtocol(SendProxyProtocol::new(
                    socket,
                    frontend_token,
                    request_id,
                    None,
                ))
            }
            None => {
                gauge_add!("protocol.tcp", 1);
                let mut pipe = Pipe::new(
                    backend_buffer,
                    backend_id.clone(),
                    None,
                    None,
                    None,
                    None,
                    cluster_id.clone(),
                    frontend_buffer,
                    frontend_token,
                    socket,
                    listener.clone(),
                    Protocol::TCP,
                    request_id,
                    frontend_address,
                    None,
                );
                pipe.set_cluster_id(cluster_id.clone());
                TcpStateMachine::Pipe(pipe)
            }
        };

        let metrics = SessionMetrics::new(Some(wait_time));
        //FIXME: timeout usage

        TcpSession {
            backend_buffer: backend_buffer_session,
            backend_connected: BackendConnectionStatus::NotConnected,
            backend_id,
            backend_token: None,
            backend: None,
            cluster_id,
            connection_attempt: 0,
            container_backend_timeout,
            container_frontend_timeout,
            frontend_address,
            frontend_buffer: frontend_buffer_session,
            frontend_token,
            has_been_closed: false,
            last_event: Instant::now(),
            listener,
            metrics,
            proxy,
            request_id,
            state,
        }
    }

    fn log_request(&self) {
        let frontend = match self.frontend_address {
            None => String::from("-"),
            Some(SocketAddr::V4(addr)) => format!("{addr}"),
            Some(SocketAddr::V6(addr)) => format!("{addr}"),
        };

        let backend_address = self
            .backend
            .as_ref()
            .map(|backend| backend.borrow().address);

        let backend = match backend_address {
            None => String::from("-"),
            Some(SocketAddr::V4(addr)) => format!("{addr}"),
            Some(SocketAddr::V6(addr)) => format!("{addr}"),
        };

        let response_time = self.metrics.response_time().whole_milliseconds();
        let service_time = self.metrics.service_time().whole_milliseconds();
        let cluster_id = self.cluster_id.clone().unwrap_or_else(|| String::from("-"));
        time!("response_time", &cluster_id, response_time);
        time!("response_time", response_time);

        if let Some(backend_id) = self.metrics.backend_id.as_ref() {
            if let Some(backend_response_time) = self.metrics.backend_response_time() {
                record_backend_metrics!(
                    cluster_id,
                    backend_id,
                    backend_response_time.whole_milliseconds(),
                    self.metrics.backend_response_time(),
                    self.metrics.backend_bin,
                    self.metrics.backend_bout
                );
            }
        }

        info!(
            "{}{} -> {}\t{} {} {} {}",
            self.log_context(),
            frontend,
            backend,
            response_time,
            service_time,
            self.metrics.bin,
            self.metrics.bout
        );
    }

    fn front_hup(&mut self) -> StateResult {
        match &mut self.state {
            TcpStateMachine::Pipe(pipe) => pipe.frontend_hup(&mut self.metrics),
            _ => {
                self.log_request();
                StateResult::CloseSession
            }
        }
    }

    fn back_hup(&mut self) -> StateResult {
        match &mut self.state {
            TcpStateMachine::Pipe(pipe) => pipe.backend_hup(&mut self.metrics),
            _ => {
                self.log_request();
                StateResult::CloseSession
            }
        }
    }

    fn log_context(&self) -> String {
        format!(
            "{} {} {}\t",
            self.request_id,
            self.cluster_id.as_deref().unwrap_or("-"),
            self.backend_id.as_deref().unwrap_or("-")
        )
    }

    fn readable(&mut self) -> StateResult {
        if !self.container_frontend_timeout.reset() {
            error!("could not reset front timeout");
        }

        let mut should_upgrade_protocol = SessionResult::Continue;

        let state_result = match &mut self.state {
            TcpStateMachine::Pipe(pipe) => pipe.readable(&mut self.metrics),
            TcpStateMachine::RelayProxyProtocol(pp) => pp.readable(&mut self.metrics),
            TcpStateMachine::ExpectProxyProtocol(pp) => {
                should_upgrade_protocol = pp.readable(&mut self.metrics);
                match should_upgrade_protocol {
                    SessionResult::Upgrade => StateResult::Continue,
                    SessionResult::Continue => StateResult::Continue,
                    SessionResult::Close => StateResult::CloseSession,
                }
            }
            TcpStateMachine::SendProxyProtocol(_) => StateResult::Continue,
            TcpStateMachine::FailedUpgrade(_) => unreachable!(),
        };

        if let SessionResult::Upgrade = should_upgrade_protocol {
            match self.upgrade() {
                false => StateResult::Continue,
                true => StateResult::CloseSession,
            }
        } else {
            state_result
        }
    }

    fn writable(&mut self) -> StateResult {
        match &mut self.state {
            TcpStateMachine::Pipe(pipe) => pipe.writable(&mut self.metrics),
            _ => StateResult::Continue,
        }
    }

    fn back_readable(&mut self) -> StateResult {
        if !self.container_backend_timeout.reset() {
            error!("could not reset back timeout");
        }

        match &mut self.state {
            TcpStateMachine::Pipe(pipe) => pipe.backend_readable(&mut self.metrics),
            _ => StateResult::Continue,
        }
    }

    fn back_writable(&mut self) -> StateResult {
        let mut res = (SessionResult::Continue, StateResult::Continue);

        match &mut self.state {
            TcpStateMachine::Pipe(pipe) => res.1 = pipe.backend_writable(&mut self.metrics),
            TcpStateMachine::RelayProxyProtocol(pp) => {
                res = pp.back_writable(&mut self.metrics);
            }
            TcpStateMachine::SendProxyProtocol(pp) => {
                res = pp.back_writable(&mut self.metrics);
            }
            TcpStateMachine::ExpectProxyProtocol(_) | TcpStateMachine::FailedUpgrade(_) => unreachable!(),
        };

        if let SessionResult::Upgrade = res.0 {
            self.upgrade();
        }

        res.1
    }

    fn back_socket_mut(&mut self) -> Option<&mut MioTcpStream> {
        match &mut self.state {
            TcpStateMachine::Pipe(pipe) => pipe.back_socket_mut(),
            TcpStateMachine::SendProxyProtocol(pp) => pp.back_socket_mut(),
            TcpStateMachine::RelayProxyProtocol(pp) => pp.back_socket_mut(),
            TcpStateMachine::ExpectProxyProtocol(_) => None,
            TcpStateMachine::FailedUpgrade(_) => unreachable!(),
        }
    }

    pub fn upgrade(&mut self) -> SessionIsToBeClosed {
        let new_state = match self.state.take() {
            TcpStateMachine::SendProxyProtocol(spp) => self.upgrade_send(spp),
            TcpStateMachine::RelayProxyProtocol(rpp) => self.upgrade_relay(rpp),
            TcpStateMachine::ExpectProxyProtocol(epp) => self.upgrade_expect(epp),
            TcpStateMachine::Pipe(_) => None,
            TcpStateMachine::FailedUpgrade(_) => todo!(),
        };

        match new_state {
            Some(state) => {
                self.state = state;
                false
            } // The state stays FailedUpgrade, but the Session should be closed right after

            None => true,
        }
    }

    fn upgrade_send(
        &mut self,
        send_proxy_protocol: SendProxyProtocol<MioTcpStream>,
    ) -> Option<TcpStateMachine> {
        if self.backend_buffer.is_some() && self.frontend_buffer.is_some() {
            let mut pipe = send_proxy_protocol.into_pipe(
                self.frontend_buffer.take().unwrap(),
                self.backend_buffer.take().unwrap(),
                self.listener.clone(),
            );
            pipe.set_cluster_id(self.cluster_id.clone());
            gauge_add!("protocol.proxy.send", -1);
            gauge_add!("protocol.tcp", 1);
            return Some(TcpStateMachine::Pipe(pipe));
        }
        error!("Missing the frontend or backend buffer queue, we can't switch to a pipe");
        None
    }

    fn upgrade_relay(&mut self, rpp: RelayProxyProtocol<MioTcpStream>) -> Option<TcpStateMachine> {
        if self.backend_buffer.is_some() {
            let mut pipe =
                rpp.into_pipe(self.backend_buffer.take().unwrap(), self.listener.clone());
            pipe.set_cluster_id(self.cluster_id.clone());
            gauge_add!("protocol.proxy.relay", -1);
            gauge_add!("protocol.tcp", 1);
            return Some(TcpStateMachine::Pipe(pipe));
        }
        error!("Missing the backend buffer queue, we can't switch to a pipe");
        None
    }

    fn upgrade_expect(&mut self, epp: ExpectProxyProtocol<MioTcpStream>) -> Option<TcpStateMachine> {
        if self.frontend_buffer.is_some() && self.backend_buffer.is_some() {
            let mut pipe = epp.into_pipe(
                self.frontend_buffer.take().unwrap(),
                self.backend_buffer.take().unwrap(),
                None,
                None,
                self.listener.clone(),
            );
            pipe.set_cluster_id(self.cluster_id.clone());
            gauge_add!("protocol.proxy.expect", -1);
            gauge_add!("protocol.tcp", 1);
            return Some(TcpStateMachine::Pipe(pipe));
        }
        error!("Missing the backend buffer queue, we can't switch to a pipe");
        None
    }

    fn front_readiness(&mut self) -> &mut Readiness {
        self.state.front_readiness()
    }

    fn back_readiness(&mut self) -> Option<&mut Readiness> {
        match &mut self.state {
            TcpStateMachine::Pipe(pipe) => Some(pipe.back_readiness()),
            TcpStateMachine::SendProxyProtocol(pp) => Some(pp.back_readiness()),
            TcpStateMachine::RelayProxyProtocol(pp) => Some(pp.back_readiness()),
            TcpStateMachine::ExpectProxyProtocol(_) => None,
            TcpStateMachine::FailedUpgrade(_) => todo!(),
        }
    }

    fn set_back_socket(&mut self, socket: MioTcpStream) {
        match &mut self.state {
            TcpStateMachine::Pipe(pipe) => pipe.set_back_socket(socket),
            TcpStateMachine::SendProxyProtocol(pp) => pp.set_back_socket(socket),
            TcpStateMachine::RelayProxyProtocol(pp) => pp.set_back_socket(socket),
            TcpStateMachine::ExpectProxyProtocol(_) => {
                panic!("we should not set the back socket for the expect proxy protocol")
            }
            TcpStateMachine::FailedUpgrade(_) => unreachable!(),
        }
    }

    fn set_back_token(&mut self, token: Token) {
        self.backend_token = Some(token);

        match &mut self.state {
            TcpStateMachine::Pipe(pipe) => pipe.set_back_token(token),
            TcpStateMachine::SendProxyProtocol(pp) => pp.set_back_token(token),
            TcpStateMachine::RelayProxyProtocol(pp) => pp.set_back_token(token),
            TcpStateMachine::ExpectProxyProtocol(_) => self.backend_token = Some(token),
            TcpStateMachine::FailedUpgrade(_) => unreachable!(),
        }
    }

    fn set_backend_id(&mut self, id: String) {
        self.backend_id = Some(id.clone());
        if let TcpStateMachine::Pipe(pipe) = &mut self.state {
            pipe.set_backend_id(Some(id));
        }
    }

    fn back_connected(&self) -> BackendConnectionStatus {
        self.backend_connected
    }

    fn set_back_connected(&mut self, status: BackendConnectionStatus) {
        let last = self.backend_connected;
        self.backend_connected = status;

        if status == BackendConnectionStatus::Connected {
            gauge_add!("backend.connections", 1);
            gauge_add!(
                "connections_per_backend",
                1,
                self.cluster_id.as_deref(),
                self.metrics.backend_id.as_deref()
            );
            if let TcpStateMachine::SendProxyProtocol(spp) = &mut self.state {
                spp.set_back_connected(BackendConnectionStatus::Connected);
            }

            if let Some(backend) = self.backend.as_ref() {
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
                    push_event(WorkerEvent::BackendUp(
                        backend.backend_id.clone(),
                        backend.address,
                    ));
                }

                if let BackendConnectionStatus::Connecting(start) = last {
                    backend.set_connection_time(Instant::now() - start);
                }

                //successful connection, rest failure counter
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
            (*backend.borrow_mut()).dec_connections();
        }

        self.backend_token = None;
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

                push_event(WorkerEvent::BackendDown(
                    backend.backend_id.clone(),
                    backend.address,
                ));
            }
        }
    }

    fn reset_connection_attempt(&mut self) {
        self.connection_attempt = 0;
    }

    pub fn test_back_socket(&mut self) -> SessionIsToBeClosed {
        match self.back_socket_mut() {
            Some(ref mut s) => {
                let mut tmp = [0u8; 1];
                let res = s.peek(&mut tmp[..]);

                match res {
                    // if the socket is half open, it will report 0 bytes read (EOF)
                    Ok(0) => false,
                    Ok(_) => true,
                    Err(e) => matches!(e.kind(), std::io::ErrorKind::WouldBlock),
                }
            }
            None => false,
        }
    }

    pub fn cancel_timeouts(&mut self) {
        self.container_frontend_timeout.cancel();
        self.container_backend_timeout.cancel();
    }

    fn ready_inner(&mut self, session: Rc<RefCell<dyn ProxySession>>) -> StateResult {
        let mut counter = 0;
        let max_loop_iterations = 100000;

        let back_connected = self.back_connected();
        if back_connected.is_connecting() {
            if self.back_readiness().unwrap().event.is_hup() && !self.test_back_socket() {
                //retry connecting the backend
                error!("error connecting to backend, trying again");
                self.connection_attempt += 1;
                self.fail_backend_connection();

                // trigger a backend reconnection
                self.close_backend();
                match self.connect_to_backend(session.clone()) {
                    // reuse connection or send a default answer, we can continue
                    Ok(BackendConnectAction::Reuse) => {}
                    Ok(BackendConnectAction::New) | Ok(BackendConnectAction::Replace) => {
                        // stop here, we must wait for an event
                        return StateResult::Continue;
                    }
                    // TODO: should we return CloseSession here?
                    Err(connection_error) => {
                        error!("Error connecting to backend: {:#}", connection_error)
                    }
                }
            } else if self.back_readiness().unwrap().event != Ready::empty() {
                self.reset_connection_attempt();
                let back_token = self.backend_token.unwrap();
                self.container_backend_timeout.set(back_token);
                // Why is this here? this artificially reset the connection time for no apparent reasons
                // TODO: maybe remove this?
                self.backend_connected = BackendConnectionStatus::Connecting(Instant::now());

                self.set_back_connected(BackendConnectionStatus::Connected);
            }
        } else if back_connected == BackendConnectionStatus::NotConnected {
            match self.connect_to_backend(session.clone()) {
                // reuse connection or send a default answer, we can continue
                Ok(BackendConnectAction::Reuse) => {}
                Ok(BackendConnectAction::New) | Ok(BackendConnectAction::Replace) => {
                    // we must wait for an event
                    return StateResult::Continue;
                }
                Err(connection_error) => {
                    error!("Error connecting to backend: {:#}", connection_error)
                }
            }
        }

        if self.front_readiness().event.is_hup() {
            let order = self.front_hup();
            match order {
                StateResult::CloseSession => {
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
                    StateResult::ConnectBackend => {
                        match self.connect_to_backend(session.clone()) {
                            // reuse connection or send a default answer, we can continue
                            Ok(BackendConnectAction::Reuse) => {}
                            Ok(BackendConnectAction::New) | Ok(BackendConnectAction::Replace) => {
                                // we must wait for an event
                                return StateResult::Continue;
                            }
                            Err(connection_error) => {
                                error!("Error connecting to backend: {:#}", connection_error)
                            }
                        }
                    }
                    StateResult::Continue => {}
                    order => return order,
                }
            }

            if back_interest.is_writable() {
                let order = self.back_writable();
                if order != StateResult::Continue {
                    return order;
                }
            }

            if back_interest.is_readable() {
                let order = self.back_readable();
                if order != StateResult::Continue {
                    return order;
                }
            }

            if front_interest.is_writable() {
                let order = self.writable();
                trace!("front writable\tinterpreting session order {:?}", order);
                if order != StateResult::Continue {
                    return order;
                }
            }

            if back_interest.is_hup() {
                let order = self.back_hup();
                match order {
                    StateResult::CloseSession => {
                        return order;
                    }
                    StateResult::Continue => {}
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

                return StateResult::CloseSession;
            }

            if back_interest.is_error() && self.back_hup() == StateResult::CloseSession {
                self.front_readiness().interest = Ready::empty();
                if let Some(r) = self.back_readiness() {
                    r.interest = Ready::empty();
                }

                error!(
                    "PROXY session {:?} back error, disconnecting",
                    self.frontend_token
                );
                return StateResult::CloseSession;
            }

            counter += 1;
        }

        if counter == max_loop_iterations {
            error!("PROXY\thandling session {:?} went through {} iterations, there's a probable infinite loop bug, closing the connection", self.frontend_token, max_loop_iterations);
            incr!("tcp.infinite_loop.error");

            let front_interest = self.front_readiness().interest & self.front_readiness().event;
            let back_interest = self
                .back_readiness()
                .map(|r| r.interest & r.event)
                .unwrap_or_else(Ready::empty);

            let front_token = self.frontend_token;
            let back = self.back_readiness().cloned();
            error!(
                "PROXY\t{:?} readiness: front {:?} / back {:?} | front: {:?} | back: {:?} ",
                front_token,
                self.front_readiness(),
                back,
                front_interest,
                back_interest
            );
            self.print_session();

            return StateResult::CloseSession;
        }

        StateResult::Continue
    }

    /// TCP session closes its backend on its own, without defering this task to the state
    fn close_backend(&mut self) {
        if let (Some(token), Some(fd)) = (
            self.backend_token,
            self.back_socket_mut().map(|s| s.as_raw_fd()),
        ) {
            let proxy = self.proxy.borrow();
            if let Err(e) = proxy.registry.deregister(&mut SourceFd(&fd)) {
                error!("error deregistering socket({:?}): {:?}", fd, e);
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
    }

    fn connect_to_backend(
        &mut self,
        session_rc: Rc<RefCell<dyn ProxySession>>,
    ) -> anyhow::Result<BackendConnectAction> {
        let cluster_id = match self.listener.borrow().cluster_id.clone() {
            Some(cluster_id) => cluster_id,
            None => {
                error!("no TCP cluster corresponds to that front address");
                bail!("no TCP cluster found.")
            }
        };

        self.cluster_id = Some(cluster_id.clone());

        if self.connection_attempt >= CONN_RETRIES {
            error!("{} max connection attempt reached", self.log_context());
            bail!(format!("Too many connections on cluster {cluster_id}"));
        }

        if self.proxy.borrow().sessions.borrow().slab.len()
            >= self.proxy.borrow().sessions.borrow().slab_capacity()
        {
            bail!("not enough memory, cannot connect to backend");
        }

        let (backend, mut stream) = self
            .proxy
            .borrow()
            .backends
            .borrow_mut()
            .backend_from_cluster_id(&cluster_id)
            .with_context(|| {
                format!("Could not get backend and TCP stream from cluster id {cluster_id}")
            })?;
        /*
        this was the old error matching for backend_from_cluster_id.
        panic! is called in case of mio::net::MioTcpStream::connect() error
        Do we really want to panic ?
        Err(ConnectionError::NoBackendAvailable(c_id)) => {
            Err(ConnectionError::NoBackendAvailable(c_id))
        }
        Err(e) => {
            panic!("tcp connect_to_backend: unexpected error: {:?}", e);
        }
        */

        if let Err(e) = stream.set_nodelay(true) {
            error!(
                "error setting nodelay on back socket({:?}): {:?}",
                stream, e
            );
        }
        self.backend_connected = BackendConnectionStatus::Connecting(Instant::now());

        let back_token = {
            let proxy = self.proxy.borrow();
            let mut s = proxy.sessions.borrow_mut();
            let entry = s.slab.vacant_entry();
            let back_token = Token(entry.key());
            let _entry = entry.insert(session_rc.clone());
            back_token
        };

        if let Err(e) = self.proxy.borrow().registry.register(
            &mut stream,
            back_token,
            Interest::READABLE | Interest::WRITABLE,
        ) {
            error!("error registering back socket({:?}): {:?}", stream, e);
        }

        let connect_timeout_duration =
            Duration::seconds(self.listener.borrow().config.connect_timeout as i64);
        self.container_backend_timeout
            .set_duration(connect_timeout_duration);
        self.container_backend_timeout.set(back_token);

        self.set_back_token(back_token);
        self.set_back_socket(stream);

        self.metrics.backend_id = Some(backend.borrow().backend_id.clone());
        self.metrics.backend_start();
        self.set_backend_id(backend.borrow().backend_id.clone());

        Ok(BackendConnectAction::New)
    }
}

impl ProxySession for TcpSession {
    fn close(&mut self) {
        if self.has_been_closed {
            return;
        }

        // TODO: the state should handle the timeouts
        info!("Closing TCP session");
        self.metrics.service_stop();

        // Restore gauges
        match self.state.marker() {
            StateMarker::Pipe => gauge_add!("protocol.tcp", -1),
            StateMarker::SendProxyProtocol => gauge_add!("protocol.proxy.send", -1),
            StateMarker::RelayProxyProtocol => gauge_add!("protocol.proxy.relay", -1),
            StateMarker::ExpectProxyProtocol => gauge_add!("protocol.proxy.expect", -1),
        }

        if self.state.failed() {
            return;
        }

        self.cancel_timeouts();

        let front_socket = self.state.front_socket();
        if let Err(e) = front_socket.shutdown(Shutdown::Both) {
            error!(
                "error shutting down front socket({:?}): {:?}",
                front_socket, e
            );
        }

        // deregister the frontend and remove it, in a separate scope to drop proxy when done
        {
            let proxy = self.proxy.borrow();
            let fd = front_socket.as_raw_fd();
            if let Err(e) = proxy.registry.deregister(&mut SourceFd(&fd)) {
                error!(
                    "error deregistering front socket({:?}) while closing TCP session: {:?}",
                    fd, e
                );
            }
            proxy
                .sessions
                .borrow_mut()
                .slab
                .try_remove(self.frontend_token.0);
        }

        self.close_backend();
        self.has_been_closed = true;
    }

    fn timeout(&mut self, token: Token) -> SessionIsToBeClosed {
        if self.frontend_token == token {
            let dur = Instant::now() - self.last_event;
            let front_timeout = self.container_frontend_timeout.duration();
            if dur < front_timeout {
                TIMER.with(|timer| {
                    timer.borrow_mut().set_timeout(front_timeout - dur, token);
                });
                return false;
            }
            return true;
        }
        // invalid token, obsolete timeout triggered
        false
    }

    fn protocol(&self) -> Protocol {
        Protocol::TCP
    }

    fn update_readiness(&mut self, token: Token, events: Ready) {
        trace!(
            "token {:?} got event {}",
            token,
            super::ready_to_string(events)
        );
        self.last_event = Instant::now();
        self.metrics.wait_start();

        if self.frontend_token == token {
            self.front_readiness().event = self.front_readiness().event | events;
        } else if self.backend_token == Some(token) {
            if let Some(r) = self.back_readiness() {
                r.event |= events;
            }
        }
    }

    fn ready(&mut self, session: Rc<RefCell<dyn ProxySession>>) -> SessionIsToBeClosed {
        self.metrics().service_start();
        let state_result = self.ready_inner(session);

        let to_bo_closed = match state_result {
            StateResult::CloseBackend => {
                self.close_backend();
                false
            }
            StateResult::CloseSession => true,
            StateResult::Continue => false,
            StateResult::ConnectBackend => unreachable!(),
            StateResult::Upgrade => unreachable!(),
        };

        self.metrics().service_stop();
        to_bo_closed
    }

    fn shutting_down(&mut self) -> SessionIsToBeClosed {
        true
    }

    fn last_event(&self) -> Instant {
        self.last_event
    }

    fn print_session(&self) {
        let state: String = match &self.state {
            TcpStateMachine::ExpectProxyProtocol(_) => String::from("Expect"),
            TcpStateMachine::SendProxyProtocol(_) => String::from("Send"),
            TcpStateMachine::RelayProxyProtocol(_) => String::from("Relay"),
            TcpStateMachine::Pipe(_) => String::from("TCP"),
            TcpStateMachine::FailedUpgrade(marker) => format!("FailedUpgrade({marker:?})"),
        };

        let front_readiness = match &self.state {
            TcpStateMachine::ExpectProxyProtocol(expect) => Some(&expect.frontend_readiness),
            TcpStateMachine::SendProxyProtocol(send) => Some(&send.frontend_readiness),
            TcpStateMachine::RelayProxyProtocol(relay) => Some(&relay.frontend_readiness),
            TcpStateMachine::Pipe(pipe) => Some(&pipe.frontend_readiness),
            TcpStateMachine::FailedUpgrade(_) => None,
        };

        let back_readiness = match &self.state {
            TcpStateMachine::SendProxyProtocol(send) => Some(&send.backend_readiness),
            TcpStateMachine::RelayProxyProtocol(relay) => Some(&relay.backend_readiness),
            TcpStateMachine::Pipe(pipe) => Some(&pipe.backend_readiness),
            TcpStateMachine::ExpectProxyProtocol(_) => None,
            TcpStateMachine::FailedUpgrade(_) => None,
        };

        error!(
            "\
TCP Session ({:?})
\tFrontend:
\t\ttoken: {:?}\treadiness: {:?}
\tBackend:
\t\ttoken: {:?}\treadiness: {:?}\tstatus: {:?}\tcluster id: {:?}",
            state,
            self.frontend_token,
            front_readiness,
            self.backend_token,
            back_readiness,
            self.backend_connected,
            self.cluster_id
        );
        error!("Metrics: {:?}", self.metrics);
    }

    fn frontend_token(&self) -> Token {
        self.frontend_token
    }
}

pub struct TcpListener {
    active: SessionIsToBeClosed,
    address: SocketAddr,
    cluster_id: Option<String>,
    config: TcpListenerConfig,
    listener: Option<MioTcpListener>,
    pool: Rc<RefCell<Pool>>,
    tags: BTreeMap<String, BTreeMap<String, String>>,
    token: Token,
}

impl ListenerHandler for TcpListener {
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

impl TcpListener {
    fn new(config: TcpListenerConfig, pool: Rc<RefCell<Pool>>, token: Token) -> TcpListener {
        TcpListener {
            cluster_id: None,
            listener: None,
            token,
            address: config.address,
            pool,
            config,
            active: false,
            tags: BTreeMap::new(),
        }
    }

    // TODO: return Result with context
    pub fn activate(
        &mut self,
        registry: &Registry,
        tcp_listener: Option<MioTcpListener>,
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
                error!("error registering socket({:?}): {:?}", sock, e);
            }
        } else {
            return None;
        }

        self.listener = listener;
        self.active = true;
        Some(self.token)
    }
}

#[derive(Debug)]
pub struct ClusterConfiguration {
    proxy_protocol: Option<ProxyProtocolConfig>,
    // Uncomment this when implementing new load balancing algorithms
    // load_balancing: LoadBalancingAlgorithms,
}

pub struct TcpProxy {
    fronts: HashMap<String, Token>,
    backends: Rc<RefCell<BackendMap>>,
    listeners: HashMap<Token, Rc<RefCell<TcpListener>>>,
    configs: HashMap<ClusterId, ClusterConfiguration>,
    registry: Registry,
    sessions: Rc<RefCell<SessionManager>>,
}

impl TcpProxy {
    pub fn new(
        registry: Registry,
        sessions: Rc<RefCell<SessionManager>>,
        backends: Rc<RefCell<BackendMap>>,
    ) -> TcpProxy {
        TcpProxy {
            backends,
            listeners: HashMap::new(),
            configs: HashMap::new(),
            fronts: HashMap::new(),
            registry,
            sessions,
        }
    }

    // TODO: return Result with context
    pub fn add_listener(
        &mut self,
        config: TcpListenerConfig,
        pool: Rc<RefCell<Pool>>,
        token: Token,
    ) -> Option<Token> {
        match self.listeners.entry(token) {
            Entry::Vacant(entry) => {
                entry.insert(Rc::new(RefCell::new(TcpListener::new(config, pool, token))));
                Some(token)
            }
            _ => None,
        }
    }

    pub fn remove_listener(&mut self, address: SocketAddr) -> SessionIsToBeClosed {
        let len = self.listeners.len();

        self.listeners.retain(|_, l| l.borrow().address != address);
        self.listeners.len() < len
    }

    // TODO: return Result with context
    pub fn activate_listener(
        &self,
        addr: &SocketAddr,
        tcp_listener: Option<MioTcpListener>,
    ) -> Option<Token> {
        self.listeners
            .values()
            .find(|listener| listener.borrow().address == *addr)
            .and_then(|listener| listener.borrow_mut().activate(&self.registry, tcp_listener))
    }

    pub fn give_back_listeners(&mut self) -> Vec<(SocketAddr, MioTcpListener)> {
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
    pub fn give_back_listener(&mut self, address: SocketAddr) -> Option<(Token, MioTcpListener)> {
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

    pub fn add_tcp_front(&mut self, front: TcpFrontend) -> Result<(), io::Error> {
        let mut listener = match self
            .listeners
            .values()
            .find(|l| l.borrow().address == front.address)
        {
            Some(l) => l.borrow_mut(),
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no such listener for '{}'", front.address),
                ));
            }
        };

        self.fronts
            .insert(front.cluster_id.to_string(), listener.token);

        listener.set_tags(front.address.to_string(), front.tags);
        listener.cluster_id = Some(front.cluster_id.to_string());
        Ok(())
    }

    pub fn remove_tcp_front(&mut self, front: &TcpFrontend) -> Result<(), io::Error> {
        let mut listener = match self
            .listeners
            .values()
            .find(|l| l.borrow().address == front.address)
        {
            Some(l) => l.borrow_mut(),
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no such listener for '{}'", front.address),
                ));
            }
        };

        listener.set_tags(front.address.to_string(), None);
        if let Some(cluster_id) = listener.cluster_id.take() {
            self.fronts.remove(&cluster_id);
        }

        Ok(())
    }
}

impl ProxyConfiguration for TcpProxy {
    fn notify(&mut self, message: ProxyRequest) -> ProxyResponse {
        match message.order {
            ProxyRequestOrder::AddTcpFrontend(front) => {
                if let Err(err) = self.add_tcp_front(front) {
                    return ProxyResponse::error(message.id, err);
                }

                ProxyResponse::ok(message.id)
            }
            ProxyRequestOrder::RemoveTcpFrontend(front) => {
                if let Err(err) = self.remove_tcp_front(&front) {
                    return ProxyResponse::error(message.id, err);
                }

                ProxyResponse::ok(message.id)
            }
            ProxyRequestOrder::SoftStop => {
                info!("{} processing soft shutdown", message.id);
                let listeners: HashMap<_, _> = self.listeners.drain().collect();
                for (_, l) in listeners.iter() {
                    l.borrow_mut()
                        .listener
                        .take()
                        .map(|mut sock| self.registry.deregister(&mut sock));
                }
                ProxyResponse::processing(message.id)
            }
            ProxyRequestOrder::HardStop => {
                info!("{} hard shutdown", message.id);
                let mut listeners: HashMap<_, _> = self.listeners.drain().collect();
                for (_, l) in listeners.drain() {
                    l.borrow_mut()
                        .listener
                        .take()
                        .map(|mut sock| self.registry.deregister(&mut sock));
                }
                ProxyResponse::ok(message.id)
            }
            ProxyRequestOrder::Status => {
                info!("{} status", message.id);
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
            ProxyRequestOrder::AddCluster(cluster) => {
                let config = ClusterConfiguration {
                    proxy_protocol: cluster.proxy_protocol,
                    //load_balancing: cluster.load_balancing,
                };
                self.configs.insert(cluster.cluster_id, config);
                ProxyResponse::ok(message.id)
            }
            ProxyRequestOrder::RemoveCluster { cluster_id } => {
                self.configs.remove(&cluster_id);
                ProxyResponse::ok(message.id)
            }
            ProxyRequestOrder::RemoveListener(remove) => {
                if !self.remove_listener(remove.address) {
                    ProxyResponse::error(
                        message.id,
                        format!("no TCP listener to remove at address {:?}", remove.address),
                    )
                } else {
                    ProxyResponse::ok(message.id)
                }
            }
            command => {
                error!(
                    "{} unsupported message for TCP proxy, ignoring {:?}",
                    message.id, command
                );
                ProxyResponse::error(message.id, "unsupported message")
            }
        }
    }

    fn accept(&mut self, token: ListenToken) -> Result<MioTcpStream, AcceptError> {
        let internal_token = Token(token.0);
        if let Some(listener) = self.listeners.get(&internal_token) {
            if let Some(tcp_listener) = &listener.borrow().listener {
                tcp_listener
                    .accept()
                    .map(|(frontend_sock, _)| frontend_sock)
                    .map_err(|e| match e.kind() {
                        ErrorKind::WouldBlock => AcceptError::WouldBlock,
                        _ => {
                            error!("accept() IO error: {:?}", e);
                            AcceptError::IoError
                        }
                    })
            } else {
                Err(AcceptError::IoError)
            }
        } else {
            Err(AcceptError::IoError)
        }
    }

    fn create_session(
        &mut self,
        mut frontend_sock: MioTcpStream,
        token: ListenToken,
        wait_time: Duration,
        proxy: Rc<RefCell<Self>>,
    ) -> Result<(), AcceptError> {
        let listener_token = Token(token.0);

        let listener = self
            .listeners
            .get(&listener_token)
            .ok_or(AcceptError::IoError)?;

        let owned = listener.borrow();
        let mut pool = owned.pool.borrow_mut();

        let (front_buffer, back_buffer) = match (pool.checkout(), pool.checkout()) {
            (Some(fb), Some(bb)) => (fb, bb),
            _ => {
                error!("could not get buffers from pool");
                error!("max number of session connection reached, flushing the accept queue");
                gauge!("accept_queue.backpressure", 1);
                self.sessions.borrow_mut().can_accept = false;

                return Err(AcceptError::TooManySessions);
            }
        };

        if owned.cluster_id.is_none() {
            error!(
                "listener at address {:?} has no linked cluster",
                owned.address
            );
            return Err(AcceptError::IoError);
        }

        let proxy_protocol = self
            .configs
            .get(owned.cluster_id.as_ref().unwrap())
            .and_then(|c| c.proxy_protocol.clone());

        if let Err(e) = frontend_sock.set_nodelay(true) {
            error!(
                "error setting nodelay on front socket({:?}): {:?}",
                frontend_sock, e
            );
        }

        let mut session_manager = self.sessions.borrow_mut();
        let entry = session_manager.slab.vacant_entry();
        let frontend_token = Token(entry.key());

        if let Err(register_error) = self.registry.register(
            &mut frontend_sock,
            frontend_token,
            Interest::READABLE | Interest::WRITABLE,
        ) {
            error!(
                "error registering front socket({:?}): {:?}",
                frontend_sock, register_error
            );
            return Err(AcceptError::RegisterError);
        }

        let session = TcpSession::new(
            back_buffer,
            None,
            owned.cluster_id.clone(),
            Duration::seconds(owned.config.back_timeout as i64),
            Duration::seconds(owned.config.front_timeout as i64),
            front_buffer,
            frontend_token,
            listener.clone(),
            proxy_protocol,
            proxy,
            frontend_sock,
            wait_time,
        );
        incr!("tcp.requests");

        let session = Rc::new(RefCell::new(session));
        entry.insert(session);

        Ok(())
    }
}

/// This is not directly used by Sōzu but is available for example and testing purposes
pub fn start_tcp_worker(
    config: TcpListenerConfig,
    max_buffers: usize,
    buffer_size: usize,
    channel: ProxyChannel,
) -> anyhow::Result<()> {
    use crate::server;

    let poll = Poll::new().with_context(|| "could not create event loop")?;
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

    let sessions = SessionManager::new(sessions, max_buffers);
    let address = config.address;
    let registry = poll
        .registry()
        .try_clone()
        .with_context(|| "Failed at creating a registry")?;
    let mut configuration = TcpProxy::new(registry, sessions.clone(), backends.clone());
    let _ = configuration.add_listener(config, pool.clone(), token);
    let _ = configuration.activate_listener(&address, None);
    let (scm_server, _scm_client) =
        UnixStream::pair().with_context(|| "Failed at creating scm stream sockets")?;
    let scm_socket =
        ScmSocket::new(scm_server.as_raw_fd()).with_context(|| "Could not create scm socket")?;
    let server_config = server::ServerConfig {
        max_connections: max_buffers,
        ..Default::default()
    };

    let mut server = Server::new(
        poll,
        channel,
        scm_socket,
        sessions,
        pool,
        backends,
        None,
        None,
        Some(configuration),
        server_config,
        None,
        false,
    )
    .with_context(|| "Could not create tcp server")?;

    info!("starting event loop");
    server.run();
    info!("ending event loop");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sozu_command::channel::Channel;
    use crate::sozu_command::proxy::{self, LoadBalancingParams, TcpFrontend};
    use crate::sozu_command::scm_socket::Listeners;
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::os::unix::io::IntoRawFd;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier};
    use std::{str, thread};
    static TEST_FINISHED: AtomicBool = AtomicBool::new(false);

    /*
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn size_test() {
      assert_size!(Pipe<mio::net::TcpStream>, 224);
      assert_size!(SendProxyProtocol<mio::net::TcpStream>, 144);
      assert_size!(RelayProxyProtocol<mio::net::TcpStream>, 152);
      assert_size!(ExpectProxyProtocol<mio::net::TcpStream>, 520);
      assert_size!(State, 528);
      // fails depending on the platform?
      //assert_size!(Session, 808);
    }*/

    #[test]
    fn mi() {
        setup_test_logger!();
        let barrier = Arc::new(Barrier::new(2));
        start_server(barrier.clone());
        let _tx = start_proxy().expect("Could not start proxy");
        barrier.wait();

        let mut s1 = TcpStream::connect("127.0.0.1:1234").expect("could not parse address");
        let s3 = TcpStream::connect("127.0.0.1:1234").expect("could not parse address");
        let mut s2 = TcpStream::connect("127.0.0.1:1234").expect("could not parse address");

        s1.write(&b"hello "[..])
            .map_err(|e| {
                TEST_FINISHED.store(true, Ordering::Relaxed);
                e
            })
            .unwrap();
        println!("s1 sent");

        s2.write(&b"pouet pouet"[..])
            .map_err(|e| {
                TEST_FINISHED.store(true, Ordering::Relaxed);
                e
            })
            .unwrap();

        println!("s2 sent");

        let mut res = [0; 128];
        s1.write(&b"coucou"[..])
            .map_err(|e| {
                TEST_FINISHED.store(true, Ordering::Relaxed);
                e
            })
            .unwrap();

        s3.shutdown(Shutdown::Both).unwrap();
        let sz2 = s2
            .read(&mut res[..])
            .map_err(|e| {
                TEST_FINISHED.store(true, Ordering::Relaxed);
                e
            })
            .expect("could not read from socket");
        println!("s2 received {:?}", str::from_utf8(&res[..sz2]));
        assert_eq!(&res[..sz2], &b"pouet pouet"[..]);

        let sz1 = s1
            .read(&mut res[..])
            .map_err(|e| {
                TEST_FINISHED.store(true, Ordering::Relaxed);
                e
            })
            .expect("could not read from socket");
        println!(
            "s1 received again({}): {:?}",
            sz1,
            str::from_utf8(&res[..sz1])
        );
        assert_eq!(&res[..sz1], &b"hello coucou"[..]);
        TEST_FINISHED.store(true, Ordering::Relaxed);
    }

    fn start_server(barrier: Arc<Barrier>) {
        let listener = TcpListener::bind("127.0.0.1:5678").expect("could not parse address");
        fn handle_client(stream: &mut TcpStream, id: u8) {
            let mut buf = [0; 128];
            let _response = b" END";
            while let Ok(sz) = stream.read(&mut buf[..]) {
                if sz > 0 {
                    println!("ECHO[{}] got \"{:?}\"", id, str::from_utf8(&buf[..sz]));
                    stream.write(&buf[..sz]).unwrap();
                }
                if TEST_FINISHED.load(Ordering::Relaxed) {
                    println!("backend server stopping");
                    break;
                }
            }
        }

        let mut count = 0;
        thread::spawn(move || {
            barrier.wait();
            for conn in listener.incoming() {
                match conn {
                    Ok(mut stream) => {
                        thread::spawn(move || {
                            println!("got a new client: {count}");
                            handle_client(&mut stream, count)
                        });
                    }
                    Err(e) => {
                        println!("connection failed: {e:?}");
                    }
                }
                count += 1;
            }
        });
    }

    /// used in tests only
    pub fn start_proxy() -> anyhow::Result<Channel<ProxyRequest, ProxyResponse>> {
        use crate::server;

        info!("listen for connections");
        let (mut command, channel) =
            Channel::generate(1000, 10000).with_context(|| "should create a channel")?;

        // this thread should call a start() function that performs the same logic and returns Result<()>
        // any error coming from this start() would be mapped and logged within the thread
        thread::spawn(move || {
            setup_test_logger!();
            info!("starting event loop");
            let poll = Poll::new().expect("could not create event loop");
            let max_connections = 100;
            let buffer_size = 16384;
            let pool = Rc::new(RefCell::new(Pool::with_capacity(
                1,
                2 * max_connections,
                buffer_size,
            )));
            let backends = Rc::new(RefCell::new(BackendMap::new()));

            let mut sessions: Slab<Rc<RefCell<dyn ProxySession>>> =
                Slab::with_capacity(max_connections);
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

            let sessions = SessionManager::new(sessions, max_connections);
            let registry = poll.registry().try_clone().unwrap();
            let mut configuration = TcpProxy::new(registry, sessions.clone(), backends.clone());
            let listener_config = TcpListenerConfig {
                address: "127.0.0.1:1234".parse().unwrap(),
                public_address: None,
                expect_proxy: false,
                front_timeout: 60,
                back_timeout: 30,
                connect_timeout: 3,
            };

            {
                let address = listener_config.address;
                let mut s = sessions.borrow_mut();
                let entry = s.slab.vacant_entry();
                let _ =
                    configuration.add_listener(listener_config, pool.clone(), Token(entry.key()));
                let _ = configuration.activate_listener(&address, None);
                entry.insert(Rc::new(RefCell::new(ListenSession {
                    protocol: Protocol::TCPListen,
                })));
            }

            let (scm_server, scm_client) = UnixStream::pair().unwrap();
            let client_scm_socket =
                ScmSocket::new(scm_client.into_raw_fd()).expect("Could not create scm socket");
            let server_scm_socket =
                ScmSocket::new(scm_server.as_raw_fd()).expect("Could not create scm socket");
            client_scm_socket
                .send_listeners(&Listeners {
                    http: Vec::new(),
                    tls: Vec::new(),
                    tcp: Vec::new(),
                })
                .unwrap();

            let server_config = server::ServerConfig {
                max_connections,
                ..Default::default()
            };
            let mut server = Server::new(
                poll,
                channel,
                server_scm_socket,
                sessions,
                pool,
                backends,
                None,
                None,
                Some(configuration),
                server_config,
                None,
                false,
            )
            .expect("Failed at creating the server");
            info!("will run");
            server.run();
            info!("ending event loop");
        });

        command.blocking().unwrap();
        {
            let front = TcpFrontend {
                cluster_id: String::from("yolo"),
                address: "127.0.0.1:1234"
                    .parse()
                    .with_context(|| "Could not parse address")?,
                tags: None,
            };
            let backend = proxy::Backend {
                cluster_id: String::from("yolo"),
                backend_id: String::from("yolo-0"),
                address: "127.0.0.1:5678"
                    .parse()
                    .with_context(|| "Could not parse address")?,
                load_balancing_parameters: Some(LoadBalancingParams::default()),
                sticky_id: None,
                backup: None,
            };

            command
                .write_message(&ProxyRequest {
                    id: String::from("ID_YOLO1"),
                    order: ProxyRequestOrder::AddTcpFrontend(front),
                })
                .unwrap();
            command
                .write_message(&ProxyRequest {
                    id: String::from("ID_YOLO2"),
                    order: ProxyRequestOrder::AddBackend(backend),
                })
                .unwrap();
        }
        {
            let front = TcpFrontend {
                cluster_id: String::from("yolo"),
                address: "127.0.0.1:1235"
                    .parse()
                    .with_context(|| "Could not parse address")?,
                tags: None,
            };
            let backend = proxy::Backend {
                cluster_id: String::from("yolo"),
                backend_id: String::from("yolo-0"),
                address: "127.0.0.1:5678"
                    .parse()
                    .with_context(|| "Could not parse address")?,
                load_balancing_parameters: Some(LoadBalancingParams::default()),
                sticky_id: None,
                backup: None,
            };
            command
                .write_message(&ProxyRequest {
                    id: String::from("ID_YOLO3"),
                    order: ProxyRequestOrder::AddTcpFrontend(front),
                })
                .unwrap();
            command
                .write_message(&ProxyRequest {
                    id: String::from("ID_YOLO4"),
                    order: ProxyRequestOrder::AddBackend(backend),
                })
                .unwrap();
        }

        // not sure why four times
        for _ in 0..4 {
            println!(
                "read_message: {:?}",
                command
                    .read_message()
                    .with_context(|| "could not read message")?
            );
        }

        Ok(command)
    }
}
