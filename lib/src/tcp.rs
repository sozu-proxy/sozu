use mio::net::*;
use mio::unix::SourceFd;
use mio::*;
use rusty_ulid::Ulid;
use slab::Slab;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{Shutdown, SocketAddr};
use std::os::unix::io::AsRawFd;
use std::rc::Rc;
use time::{Duration, Instant};

use crate::sozu_command::config::ProxyProtocolConfig;
use crate::sozu_command::logging;
use crate::sozu_command::proxy::TcpListener as TcpListenerConfig;
use crate::sozu_command::proxy::{
    LoadBalancingAlgorithms, ProxyEvent, ProxyRequest, ProxyRequestData, ProxyResponse,
    ProxyResponseStatus,
};
use crate::sozu_command::ready::Ready;
use crate::sozu_command::scm_socket::ScmSocket;

use crate::backends::BackendMap;
use crate::pool::{Checkout, Pool};
use crate::protocol::proxy_protocol::expect::ExpectProxyProtocol;
use crate::protocol::proxy_protocol::relay::RelayProxyProtocol;
use crate::protocol::proxy_protocol::send::SendProxyProtocol;
use crate::protocol::{Pipe, ProtocolResult};
use crate::retry::RetryPolicy;
use crate::server::{
    push_event, ListenSession, ListenToken, ProxyChannel, Server, SessionManager, CONN_RETRIES,
    TIMER,
};
use crate::socket::server_bind;
use crate::timer::TimeoutContainer;
use crate::util::UnwrapLog;
use crate::{
    AcceptError, Backend, BackendConnectAction, BackendConnectionStatus, ClusterId,
    ConnectionError, Protocol, ProxyConfiguration, ProxySession, Readiness, SessionMetrics,
    SessionResult,
};

pub enum UpgradeResult {
    Continue,
    Close,
    ConnectBackend,
}

pub enum State {
    Pipe(Pipe<TcpStream>),
    SendProxyProtocol(SendProxyProtocol<TcpStream>),
    RelayProxyProtocol(RelayProxyProtocol<TcpStream>),
    ExpectProxyProtocol(ExpectProxyProtocol<TcpStream>),
}

pub struct Session {
    backend: Option<Rc<RefCell<Backend>>>,
    frontend_token: Token,
    backend_token: Option<Token>,
    back_connected: BackendConnectionStatus,
    accept_token: Token,
    request_id: Ulid,
    cluster_id: Option<String>,
    backend_id: Option<String>,
    metrics: SessionMetrics,
    protocol: Option<State>,
    front_buf: Option<Checkout>,
    back_buf: Option<Checkout>,
    last_event: Instant,
    connection_attempt: u8,
    frontend_address: Option<SocketAddr>,
    front_timeout: TimeoutContainer,
    back_timeout: TimeoutContainer,
    proxy: Rc<RefCell<Proxy>>,
}

impl Session {
    fn new(
        sock: TcpStream,
        frontend_token: Token,
        accept_token: Token,
        proxy: Rc<RefCell<Proxy>>,
        front_buf: Checkout,
        back_buf: Checkout,
        cluster_id: Option<String>,
        backend_id: Option<String>,
        proxy_protocol: Option<ProxyProtocolConfig>,
        wait_time: Duration,
        front_timeout_duration: Duration,
        backend_timeout_duration: Duration,
    ) -> Session {
        let frontend_address = sock.peer_addr().ok();
        let mut frontend_buffer = None;
        let mut backend_buffer = None;

        let request_id = Ulid::generate();
        let front_timeout = TimeoutContainer::new(front_timeout_duration, frontend_token);
        let back_timeout = TimeoutContainer::new_empty(backend_timeout_duration);

        let protocol = match proxy_protocol {
            Some(ProxyProtocolConfig::RelayHeader) => {
                backend_buffer = Some(back_buf);
                gauge_add!("protocol.proxy.relay", 1);
                Some(State::RelayProxyProtocol(RelayProxyProtocol::new(
                    sock,
                    frontend_token,
                    request_id,
                    None,
                    front_buf,
                )))
            }
            Some(ProxyProtocolConfig::ExpectHeader) => {
                frontend_buffer = Some(front_buf);
                backend_buffer = Some(back_buf);
                gauge_add!("protocol.proxy.expect", 1);
                Some(State::ExpectProxyProtocol(ExpectProxyProtocol::new(
                    sock,
                    frontend_token,
                    request_id,
                )))
            }
            Some(ProxyProtocolConfig::SendHeader) => {
                frontend_buffer = Some(front_buf);
                backend_buffer = Some(back_buf);
                gauge_add!("protocol.proxy.send", 1);
                Some(State::SendProxyProtocol(SendProxyProtocol::new(
                    sock,
                    frontend_token,
                    request_id,
                    None,
                )))
            }
            None => {
                gauge_add!("protocol.tcp", 1);
                let mut pipe = Pipe::new(
                    sock,
                    frontend_token,
                    request_id,
                    cluster_id.clone(),
                    backend_id.clone(),
                    None,
                    None,
                    front_buf,
                    back_buf,
                    frontend_address,
                    Protocol::TCP,
                );
                pipe.set_cluster_id(cluster_id.clone());
                Some(State::Pipe(pipe))
            }
        };

        let metrics = SessionMetrics::new(Some(wait_time));
        //FIXME: timeout usage

        Session {
            backend: None,
            frontend_token,
            backend_token: None,
            back_connected: BackendConnectionStatus::NotConnected,
            accept_token,
            proxy,
            request_id,
            cluster_id,
            backend_id,
            metrics,
            protocol,
            front_buf: frontend_buffer,
            back_buf: backend_buffer,
            last_event: Instant::now(),
            connection_attempt: 0,
            frontend_address,
            front_timeout,
            back_timeout,
        }
    }

    fn log_request(&self) {
        let frontend = match self.frontend_address {
            None => String::from("-"),
            Some(SocketAddr::V4(addr)) => format!("{}", addr),
            Some(SocketAddr::V6(addr)) => format!("{}", addr),
        };

        let backend_address = self
            .backend
            .as_ref()
            .map(|backend| (*backend.borrow_mut()).address);

        let backend = match backend_address {
            None => String::from("-"),
            Some(SocketAddr::V4(addr)) => format!("{}", addr),
            Some(SocketAddr::V6(addr)) => format!("{}", addr),
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

    fn front_hup(&mut self) -> SessionResult {
        match self.protocol {
            Some(State::Pipe(ref mut pipe)) => pipe.front_hup(&mut self.metrics),
            _ => {
                self.log_request();
                SessionResult::CloseSession
            }
        }
    }

    fn back_hup(&mut self) -> SessionResult {
        match self.protocol {
            Some(State::Pipe(ref mut pipe)) => pipe.back_hup(&mut self.metrics),
            _ => {
                self.log_request();
                SessionResult::CloseSession
            }
        }
    }

    fn log_context(&self) -> String {
        format!(
            "{} {} {}\t",
            self.request_id,
            self.cluster_id.as_ref().map(|s| s.as_str()).unwrap_or(&"-"),
            self.backend_id.as_ref().map(|s| s.as_str()).unwrap_or(&"-")
        )
    }

    fn readable(&mut self) -> SessionResult {
        if !self.front_timeout.reset() {
            error!("could not reset front timeout");
        }

        let mut should_upgrade_protocol = ProtocolResult::Continue;

        let res = match self.protocol {
            Some(State::Pipe(ref mut pipe)) => pipe.readable(&mut self.metrics),
            Some(State::RelayProxyProtocol(ref mut pp)) => pp.readable(&mut self.metrics),
            Some(State::ExpectProxyProtocol(ref mut pp)) => {
                let res = pp.readable(&mut self.metrics);
                should_upgrade_protocol = res.0;
                res.1
            }
            _ => SessionResult::Continue,
        };

        if let ProtocolResult::Upgrade = should_upgrade_protocol {
            match self.upgrade() {
                UpgradeResult::Continue => SessionResult::Continue,
                UpgradeResult::Close => SessionResult::CloseSession,
                UpgradeResult::ConnectBackend => SessionResult::ConnectBackend,
            }
        } else {
            res
        }
    }

    fn writable(&mut self) -> SessionResult {
        match self.protocol {
            Some(State::Pipe(ref mut pipe)) => pipe.writable(&mut self.metrics),
            _ => SessionResult::Continue,
        }
    }

    fn back_readable(&mut self) -> SessionResult {
        if !self.back_timeout.reset() {
            error!("could not reset back timeout");
        }

        match self.protocol {
            Some(State::Pipe(ref mut pipe)) => pipe.back_readable(&mut self.metrics),
            _ => SessionResult::Continue,
        }
    }

    fn back_writable(&mut self) -> SessionResult {
        let mut res = (ProtocolResult::Continue, SessionResult::Continue);

        match self.protocol {
            Some(State::Pipe(ref mut pipe)) => res.1 = pipe.back_writable(&mut self.metrics),
            Some(State::RelayProxyProtocol(ref mut pp)) => {
                res = pp.back_writable(&mut self.metrics);
            }
            Some(State::SendProxyProtocol(ref mut pp)) => {
                res = pp.back_writable(&mut self.metrics);
            }
            _ => unreachable!(),
        };

        if let ProtocolResult::Upgrade = res.0 {
            self.upgrade();
        }

        res.1
    }

    fn front_socket(&self) -> &TcpStream {
        match self.protocol {
            Some(State::Pipe(ref pipe)) => pipe.front_socket(),
            Some(State::SendProxyProtocol(ref pp)) => pp.front_socket(),
            Some(State::RelayProxyProtocol(ref pp)) => pp.front_socket(),
            Some(State::ExpectProxyProtocol(ref pp)) => pp.front_socket(),
            _ => unreachable!(),
        }
    }

    fn front_socket_mut(&mut self) -> &mut TcpStream {
        match self.protocol {
            Some(State::Pipe(ref mut pipe)) => pipe.front_socket_mut(),
            Some(State::SendProxyProtocol(ref mut pp)) => pp.front_socket_mut(),
            Some(State::RelayProxyProtocol(ref mut pp)) => pp.front_socket_mut(),
            Some(State::ExpectProxyProtocol(ref mut pp)) => pp.front_socket_mut(),
            _ => unreachable!(),
        }
    }

    fn back_socket_mut(&mut self) -> Option<&mut TcpStream> {
        match self.protocol {
            Some(State::Pipe(ref mut pipe)) => pipe.back_socket_mut(),
            Some(State::SendProxyProtocol(ref mut pp)) => pp.back_socket_mut(),
            Some(State::RelayProxyProtocol(ref mut pp)) => pp.back_socket_mut(),
            Some(State::ExpectProxyProtocol(_)) => None,
            _ => unreachable!(),
        }
    }

    pub fn upgrade(&mut self) -> UpgradeResult {
        let protocol = self.protocol.take();

        if let Some(State::SendProxyProtocol(pp)) = protocol {
            if self.back_buf.is_some() && self.front_buf.is_some() {
                let mut pipe = pp.into_pipe(
                    self.front_buf.take().unwrap(),
                    self.back_buf.take().unwrap(),
                );
                pipe.set_cluster_id(self.cluster_id.clone());
                self.protocol = Some(State::Pipe(pipe));
                gauge_add!("protocol.proxy.send", -1);
                gauge_add!("protocol.tcp", 1);
                UpgradeResult::Continue
            } else {
                error!("Missing the frontend or backend buffer queue, we can't switch to a pipe");
                UpgradeResult::Close
            }
        } else if let Some(State::RelayProxyProtocol(pp)) = protocol {
            if self.back_buf.is_some() {
                let mut pipe = pp.into_pipe(self.back_buf.take().unwrap());
                pipe.set_cluster_id(self.cluster_id.clone());
                self.protocol = Some(State::Pipe(pipe));
                gauge_add!("protocol.proxy.relay", -1);
                gauge_add!("protocol.tcp", 1);
                UpgradeResult::Continue
            } else {
                error!("Missing the backend buffer queue, we can't switch to a pipe");
                UpgradeResult::Close
            }
        } else if let Some(State::ExpectProxyProtocol(pp)) = protocol {
            if self.front_buf.is_some() && self.back_buf.is_some() {
                let mut pipe = pp.into_pipe(
                    self.front_buf.take().unwrap(),
                    self.back_buf.take().unwrap(),
                    None,
                    None,
                );
                pipe.set_cluster_id(self.cluster_id.clone());
                self.protocol = Some(State::Pipe(pipe));
                gauge_add!("protocol.proxy.expect", -1);
                gauge_add!("protocol.tcp", 1);
                UpgradeResult::ConnectBackend
            } else {
                error!("Missing the backend buffer queue, we can't switch to a pipe");
                UpgradeResult::Close
            }
        } else {
            UpgradeResult::Close
        }
    }

    fn front_readiness(&mut self) -> &mut Readiness {
        match self.protocol {
            Some(State::Pipe(ref mut pipe)) => pipe.front_readiness(),
            Some(State::SendProxyProtocol(ref mut pp)) => pp.front_readiness(),
            Some(State::RelayProxyProtocol(ref mut pp)) => pp.front_readiness(),
            Some(State::ExpectProxyProtocol(ref mut pp)) => pp.readiness(),
            _ => unreachable!(),
        }
    }

    fn back_readiness(&mut self) -> Option<&mut Readiness> {
        match self.protocol {
            Some(State::Pipe(ref mut pipe)) => Some(pipe.back_readiness()),
            Some(State::SendProxyProtocol(ref mut pp)) => Some(pp.back_readiness()),
            Some(State::RelayProxyProtocol(ref mut pp)) => Some(pp.back_readiness()),
            _ => None,
        }
    }

    fn back_token(&self) -> Option<Token> {
        match self.protocol {
            Some(State::Pipe(ref pipe)) => pipe.back_token(),
            Some(State::SendProxyProtocol(ref pp)) => pp.back_token(),
            Some(State::RelayProxyProtocol(ref pp)) => pp.back_token(),
            Some(State::ExpectProxyProtocol(_)) => None,
            _ => unreachable!(),
        }
    }

    fn set_back_socket(&mut self, socket: TcpStream) {
        match self.protocol {
            Some(State::Pipe(ref mut pipe)) => pipe.set_back_socket(socket),
            Some(State::SendProxyProtocol(ref mut pp)) => pp.set_back_socket(socket),
            Some(State::RelayProxyProtocol(ref mut pp)) => pp.set_back_socket(socket),
            Some(State::ExpectProxyProtocol(_)) => {
                panic!("we should not set the back socket for the expect proxy protocol")
            }
            _ => unreachable!(),
        }
    }

    fn set_back_token(&mut self, token: Token) {
        self.backend_token = Some(token);

        match self.protocol {
            Some(State::Pipe(ref mut pipe)) => pipe.set_back_token(token),
            Some(State::SendProxyProtocol(ref mut pp)) => pp.set_back_token(token),
            Some(State::RelayProxyProtocol(ref mut pp)) => pp.set_back_token(token),
            Some(State::ExpectProxyProtocol(_)) => self.backend_token = Some(token),
            _ => unreachable!(),
        }
    }

    fn set_backend_id(&mut self, id: String) {
        self.backend_id = Some(id.clone());
        match self.protocol {
            Some(State::Pipe(ref mut pipe)) => pipe.set_backend_id(Some(id)),
            //FIXME: do other cases
            _ => {}
        }
    }

    fn back_connected(&self) -> BackendConnectionStatus {
        self.back_connected
    }

    fn set_back_connected(&mut self, status: BackendConnectionStatus) {
        let last = self.back_connected.clone();
        self.back_connected = status;

        if status == BackendConnectionStatus::Connected {
            gauge_add!("backend.connections", 1);
            gauge_add!(
                "connections_per_backend",
                1,
                self.cluster_id.as_ref().map(|s| s.as_str()),
                self.metrics.backend_id.as_ref().map(|s| s.as_str())
            );
            if let Some(State::SendProxyProtocol(ref mut pp)) = self.protocol {
                pp.set_back_connected(BackendConnectionStatus::Connected);
            }

            self.backend.as_ref().map(|backend| {
                let ref mut backend = *backend.borrow_mut();

                if backend.retry_policy.is_down() {
                    incr!(
                        "up",
                        self.cluster_id.as_ref().map(|s| s.as_str()),
                        self.metrics.backend_id.as_ref().map(|s| s.as_str())
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

                //successful connection, rest failure counter
                backend.failures = 0;
                backend.retry_policy.succeed();
            });
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
        self.backend.as_ref().map(|backend| {
            let ref mut backend = *backend.borrow_mut();
            backend.failures += 1;

            let already_unavailable = backend.retry_policy.is_down();
            backend.retry_policy.fail();
            incr!(
                "connections.error",
                self.cluster_id.as_ref().map(|s| s.as_str()),
                self.metrics.backend_id.as_ref().map(|s| s.as_str())
            );
            if !already_unavailable && backend.retry_policy.is_down() {
                error!(
                    "backend server {} at {} is down",
                    backend.backend_id, backend.address
                );
                incr!(
                    "down",
                    self.cluster_id.as_ref().map(|s| s.as_str()),
                    self.metrics.backend_id.as_ref().map(|s| s.as_str())
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

    pub fn test_back_socket(&mut self) -> bool {
        match self.back_socket_mut() {
            Some(ref mut s) => {
                let mut tmp = [0u8; 1];
                let res = s.peek(&mut tmp[..]);

                match res {
                    // if the socket is half open, it will report 0 bytes read (EOF)
                    Ok(0) => false,
                    Ok(_) => true,
                    Err(e) => match e.kind() {
                        std::io::ErrorKind::WouldBlock => true,
                        _ => false,
                    },
                }
            }
            None => false,
        }
    }

    pub fn cancel_timeouts(&mut self) {
        self.front_timeout.cancel();
        self.back_timeout.cancel();
    }

    fn ready_inner(&mut self, session: Rc<RefCell<dyn ProxySession>>) -> SessionResult {
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
                    Ok(BackendConnectAction::Reuse) | Err(_) => {}
                    // New or Replace: stop here, we must wait for an event
                    _ => return SessionResult::Continue,
                }
            } else if self.back_readiness().unwrap().event != Ready::empty() {
                self.reset_connection_attempt();
                let back_token = self.backend_token.unwrap();
                self.back_timeout.set(back_token);
                self.back_connected = BackendConnectionStatus::Connecting(Instant::now());

                self.set_back_connected(BackendConnectionStatus::Connected);
            }
        } else if back_connected == BackendConnectionStatus::NotConnected {
            match self.connect_to_backend(session.clone()) {
                // reuse connection or error we can continue
                Ok(BackendConnectAction::Reuse) | Err(_) => {}
                // New or Replace: stop here, we must wait for an event
                _ => return SessionResult::Continue,
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
                    SessionResult::CloseSession => {
                        return order;
                    }
                    SessionResult::Continue => {}
                    _ => {
                        self.back_readiness().map(|r| r.event.remove(Ready::hup()));
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

            if back_interest.is_error() {
                if self.back_hup() == SessionResult::CloseSession {
                    self.front_readiness().interest = Ready::empty();
                    self.back_readiness().map(|r| r.interest = Ready::empty());
                    error!(
                        "PROXY session {:?} back error, disconnecting",
                        self.frontend_token
                    );
                    return SessionResult::CloseSession;
                }
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
                .unwrap_or(Ready::empty());

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
            self.print_state();

            return SessionResult::CloseSession;
        }

        SessionResult::Continue
    }

    fn close_backend(&mut self) {
        if let (Some(token), Some(fd)) = (
            self.back_token(),
            self.back_socket_mut().map(|s| s.as_raw_fd()),
        ) {
            let proxy = self.proxy.borrow_mut();
            if let Err(e) = proxy.registry.deregister(&mut SourceFd(&fd)) {
                error!("error deregistering socket({:?}): {:?}", fd, e);
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
                        error!("error closing back socket({:?}): {:?}", sock, e);
                    }
                }
            }
        }

        if back_connected == BackendConnectionStatus::Connected {
            gauge_add!("backend.connections", -1);
            gauge_add!(
                "connections_per_backend",
                -1,
                self.cluster_id.as_ref().map(|s| s.as_str()),
                self.metrics.backend_id.as_ref().map(|s| s.as_str())
            );
        }

        self.set_back_connected(BackendConnectionStatus::NotConnected);
    }

    fn connect_to_backend(
        &mut self,
        session_rc: Rc<RefCell<dyn ProxySession>>,
    ) -> Result<BackendConnectAction, ConnectionError> {
        if self.proxy.borrow().listeners[&self.accept_token]
            .cluster_id
            .is_none()
        {
            error!("no TCP application corresponds to that front address");
            return Err(ConnectionError::HostNotFound);
        }

        let cluster_id = self.proxy.borrow().listeners[&self.accept_token]
            .cluster_id
            .clone();
        self.cluster_id = cluster_id.clone();
        let cluster_id = cluster_id.unwrap();

        if self.connection_attempt == CONN_RETRIES {
            error!("{} max connection attempt reached", self.log_context());
            return Err(ConnectionError::NoBackendAvailable);
        }

        if self.proxy.borrow().sessions.borrow().slab.len()
            >= self.proxy.borrow().sessions.borrow().slab_capacity()
        {
            error!("not enough memory, cannot connect to backend");
            return Err(ConnectionError::TooManyConnections);
        }

        let conn = self
            .proxy
            .borrow()
            .backends
            .borrow_mut()
            .backend_from_cluster_id(&cluster_id);
        match conn {
            Ok((backend, mut stream)) => {
                if let Err(e) = stream.set_nodelay(true) {
                    error!(
                        "error setting nodelay on back socket({:?}): {:?}",
                        stream, e
                    );
                }
                self.back_connected = BackendConnectionStatus::Connecting(Instant::now());

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
                    error!("error registering back socket({:?}): {:?}", stream, e);
                }

                let connect_timeout_duration = Duration::seconds(
                    self.proxy.borrow().listeners[&self.accept_token]
                        .config
                        .connect_timeout as i64,
                );
                self.back_timeout.set_duration(connect_timeout_duration);
                self.back_timeout.set(back_token);

                self.set_back_token(back_token);
                self.set_back_socket(stream);

                self.metrics.backend_id = Some(backend.borrow().backend_id.clone());
                self.metrics.backend_start();
                self.set_backend_id(backend.borrow().backend_id.clone());

                Ok(BackendConnectAction::New)
            }
            Err(ConnectionError::NoBackendAvailable) => Err(ConnectionError::NoBackendAvailable),
            Err(e) => {
                panic!("tcp connect_to_backend: unexpected error: {:?}", e);
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
                    "error shutting down front socket({:?}): {:?}",
                    self.front_socket(),
                    e
                );
            }
        }

        self.close_backend();

        match self.protocol {
            Some(State::Pipe(_)) => gauge_add!("protocol.tcp", -1),
            Some(State::SendProxyProtocol(_)) => gauge_add!("protocol.proxy.send", -1),
            Some(State::RelayProxyProtocol(_)) => gauge_add!("protocol.proxy.relay", -1),
            Some(State::ExpectProxyProtocol(_)) => gauge_add!("protocol.proxy.expect", -1),
            None => {}
        }

        let fd = self.front_socket().as_raw_fd();
        let proxy = self.proxy.borrow_mut();
        if let Err(e) = proxy.registry.deregister(&mut SourceFd(&fd)) {
            error!("1error deregistering socket({:?}): {:?}", fd, e);
        }
    }

    fn timeout(&mut self, token: Token) {
        if self.frontend_token == token {
            let dur = Instant::now() - self.last_event;
            let front_timeout = self.front_timeout.duration();
            if dur < front_timeout {
                TIMER.with(|timer| {
                    timer.borrow_mut().set_timeout(front_timeout - dur, token);
                });
            } else {
                self.close();
            }
        } else {
            // invalid token, obsolete timeout triggered
        }
    }

    fn protocol(&self) -> Protocol {
        Protocol::TCP
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
        } else if self.backend_token == Some(token) {
            self.back_readiness().map(|r| r.event = r.event | events);
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
        self.close()
    }

    fn last_event(&self) -> Instant {
        self.last_event
    }

    fn print_state(&self) {
        let p: String = match &self.protocol {
            Some(State::ExpectProxyProtocol(_)) => String::from("Expect"),
            Some(State::SendProxyProtocol(_)) => String::from("Send"),
            Some(State::RelayProxyProtocol(_)) => String::from("Relay"),
            Some(State::Pipe(_)) => String::from("TCP"),
            None => String::from("None"),
        };

        let rf = match *unwrap_msg!(self.protocol.as_ref()) {
            State::ExpectProxyProtocol(ref expect) => &expect.readiness,
            State::SendProxyProtocol(ref send) => &send.front_readiness,
            State::RelayProxyProtocol(ref relay) => &relay.front_readiness,
            State::Pipe(ref pipe) => &pipe.front_readiness,
        };

        let rb = match *unwrap_msg!(self.protocol.as_ref()) {
            State::SendProxyProtocol(ref send) => Some(&send.back_readiness),
            State::RelayProxyProtocol(ref relay) => Some(&relay.back_readiness),
            State::Pipe(ref pipe) => Some(&pipe.back_readiness),
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

pub struct Listener {
    cluster_id: Option<String>,
    listener: Option<TcpListener>,
    token: Token,
    address: SocketAddr,
    pool: Rc<RefCell<Pool>>,
    config: TcpListenerConfig,
    active: bool,
}

impl Listener {
    fn new(config: TcpListenerConfig, pool: Rc<RefCell<Pool>>, token: Token) -> Listener {
        Listener {
            cluster_id: None,
            listener: None,
            token,
            address: config.address,
            pool,
            config,
            active: false,
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
            server_bind(&self.config.address)
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
                error!("error registering socket({:?}): {:?}", sock, e);
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
    load_balancing: LoadBalancingAlgorithms,
}

pub struct Proxy {
    fronts: HashMap<String, Token>,
    backends: Rc<RefCell<BackendMap>>,
    listeners: HashMap<Token, Listener>,
    configs: HashMap<ClusterId, ClusterConfiguration>,
    registry: Registry,
    sessions: Rc<RefCell<SessionManager>>,
}

impl Proxy {
    pub fn new(
        registry: Registry,
        sessions: Rc<RefCell<SessionManager>>,
        backends: Rc<RefCell<BackendMap>>,
    ) -> Proxy {
        Proxy {
            backends,
            listeners: HashMap::new(),
            configs: HashMap::new(),
            fronts: HashMap::new(),
            registry,
            sessions,
        }
    }

    pub fn add_listener(
        &mut self,
        config: TcpListenerConfig,
        pool: Rc<RefCell<Pool>>,
        token: Token,
    ) -> Option<Token> {
        if self.listeners.contains_key(&token) {
            None
        } else {
            let listener = Listener::new(config, pool, token);
            self.listeners.insert(listener.token, listener);
            Some(token)
        }
    }

    pub fn remove_listener(&mut self, address: SocketAddr) -> bool {
        let len = self.listeners.len();
        self.listeners.retain(|_, l| l.address != address);

        self.listeners.len() < len
    }

    pub fn activate_listener(
        &mut self,
        addr: &SocketAddr,
        tcp_listener: Option<TcpListener>,
    ) -> Option<Token> {
        for listener in self.listeners.values_mut() {
            if &listener.address == addr {
                return listener.activate(&mut self.registry, tcp_listener);
            }
        }
        None
    }

    pub fn give_back_listeners(&mut self) -> Vec<(SocketAddr, TcpListener)> {
        self.listeners
            .values_mut()
            .filter(|app_listener| app_listener.listener.is_some())
            .map(|app_listener| (app_listener.address, app_listener.listener.take().unwrap()))
            .collect()
    }

    pub fn give_back_listener(&mut self, address: SocketAddr) -> Option<(Token, TcpListener)> {
        self.listeners
            .values_mut()
            .find(|l| l.address == address)
            .and_then(|l| l.listener.take().map(|sock| (l.token, sock)))
    }

    pub fn add_tcp_front(&mut self, cluster_id: &str, address: &SocketAddr) -> bool {
        if let Some(listener) = self.listeners.values_mut().find(|l| l.address == *address) {
            self.fronts.insert(cluster_id.to_string(), listener.token);
            //info!("add_tcp_front: fronts are now: {:?}", self.fronts);
            listener.cluster_id = Some(cluster_id.to_string());
            true
        } else {
            false
        }
    }

    pub fn remove_tcp_front(&mut self, address: SocketAddr) -> bool {
        if let Some(listener) = self.listeners.values_mut().find(|l| l.address == address) {
            if let Some(cluster_id) = listener.cluster_id.take() {
                self.fronts.remove(&cluster_id);
            }
            true
        } else {
            false
        }
    }
}

impl ProxyConfiguration<Session> for Proxy {
    fn notify(&mut self, message: ProxyRequest) -> ProxyResponse {
        match message.order {
            ProxyRequestData::AddTcpFrontend(front) => {
                let _ = self.add_tcp_front(&front.cluster_id, &front.address);
                ProxyResponse {
                    id: message.id,
                    status: ProxyResponseStatus::Ok,
                    data: None,
                }
            }
            ProxyRequestData::RemoveTcpFrontend(front) => {
                let _ = self.remove_tcp_front(front.address);
                ProxyResponse {
                    id: message.id,
                    status: ProxyResponseStatus::Ok,
                    data: None,
                }
            }
            ProxyRequestData::SoftStop => {
                info!("{} processing soft shutdown", message.id);
                let mut listeners: HashMap<_, _> = self.listeners.drain().collect();
                for (_, l) in listeners.iter_mut() {
                    l.listener
                        .take()
                        .map(|mut sock| self.registry.deregister(&mut sock));
                }
                ProxyResponse {
                    id: message.id,
                    status: ProxyResponseStatus::Processing,
                    data: None,
                }
            }
            ProxyRequestData::HardStop => {
                info!("{} hard shutdown", message.id);
                let mut listeners: HashMap<_, _> = self.listeners.drain().collect();
                for (_, mut l) in listeners.drain() {
                    l.listener
                        .take()
                        .map(|mut sock| self.registry.deregister(&mut sock));
                }
                ProxyResponse {
                    id: message.id,
                    status: ProxyResponseStatus::Ok,
                    data: None,
                }
            }
            ProxyRequestData::Status => {
                info!("{} status", message.id);
                ProxyResponse {
                    id: message.id,
                    status: ProxyResponseStatus::Ok,
                    data: None,
                }
            }
            ProxyRequestData::Logging(logging_filter) => {
                info!(
                    "{} changing logging filter to {}",
                    message.id, logging_filter
                );
                logging::LOGGER.with(|l| {
                    let directives = logging::parse_logging_spec(&logging_filter);
                    l.borrow_mut().set_directives(directives);
                });
                ProxyResponse {
                    id: message.id,
                    status: ProxyResponseStatus::Ok,
                    data: None,
                }
            }
            ProxyRequestData::AddCluster(cluster) => {
                let config = ClusterConfiguration {
                    proxy_protocol: cluster.proxy_protocol,
                    load_balancing: cluster.load_balancing,
                };
                self.configs.insert(cluster.cluster_id.clone(), config);

                ProxyResponse {
                    id: message.id,
                    status: ProxyResponseStatus::Ok,
                    data: None,
                }
            }
            ProxyRequestData::RemoveCluster { cluster_id } => {
                self.configs.remove(&cluster_id);
                ProxyResponse {
                    id: message.id,
                    status: ProxyResponseStatus::Ok,
                    data: None,
                }
            }
            ProxyRequestData::RemoveListener(remove) => {
                if !self.remove_listener(remove.address) {
                    ProxyResponse {
                        id: message.id,
                        status: ProxyResponseStatus::Error(format!(
                            "no TCP listener to remove at address {:?}",
                            remove.address
                        )),
                        data: None,
                    }
                } else {
                    ProxyResponse {
                        id: message.id,
                        status: ProxyResponseStatus::Ok,
                        data: None,
                    }
                }
            }
            command => {
                error!(
                    "{} unsupported message for TCP proxy, ignoring {:?}",
                    message.id, command
                );
                ProxyResponse {
                    id: message.id,
                    status: ProxyResponseStatus::Error(String::from("unsupported message")),
                    data: None,
                }
            }
        }
    }

    fn accept(&mut self, token: ListenToken) -> Result<TcpStream, AcceptError> {
        let internal_token = Token(token.0);
        if let Some(listener) = self.listeners.get_mut(&internal_token) {
            if let Some(ref tcp_listener) = listener.listener.as_ref() {
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
        frontend_sock: TcpStream,
        token: ListenToken,
        wait_time: Duration,
        proxy: Rc<RefCell<Self>>,
    ) -> Result<(), AcceptError> {
        let internal_token = Token(token.0);

        if let Some(listener) = self.listeners.get_mut(&internal_token) {
            let mut p = (*listener.pool).borrow_mut();

            if let (Some(front_buf), Some(back_buf)) = (p.checkout(), p.checkout()) {
                if listener.cluster_id.is_none() {
                    error!(
                        "listener at address {:?} has no linked application",
                        listener.address
                    );
                    return Err(AcceptError::IoError);
                }

                let proxy_protocol = self
                    .configs
                    .get(listener.cluster_id.as_ref().unwrap())
                    .and_then(|c| c.proxy_protocol.clone());

                if let Err(e) = frontend_sock.set_nodelay(true) {
                    error!(
                        "error setting nodelay on front socket({:?}): {:?}",
                        frontend_sock, e
                    );
                }

                let mut s = self.sessions.borrow_mut();
                let entry = s.slab.vacant_entry();
                let session_token = Token(entry.key());

                let mut c = Session::new(
                    frontend_sock,
                    session_token,
                    internal_token,
                    proxy,
                    front_buf,
                    back_buf,
                    listener.cluster_id.clone(),
                    None,
                    proxy_protocol.clone(),
                    wait_time,
                    Duration::seconds(listener.config.front_timeout as i64),
                    Duration::seconds(listener.config.back_timeout as i64),
                );
                incr!("tcp.requests");

                if let Err(e) = self.registry.register(
                    c.front_socket_mut(),
                    session_token,
                    Interest::READABLE | Interest::WRITABLE,
                ) {
                    error!(
                        "error registering front socket({:?}): {:?}",
                        c.front_socket(),
                        e
                    );
                }

                let session = Rc::new(RefCell::new(c));
                entry.insert(session);

                s.incr();

                Ok(())
            } else {
                error!("could not get buffers from pool");
                error!("max number of session connection reached, flushing the accept queue");
                gauge!("accept_queue.backpressure", 1);
                self.sessions.borrow_mut().can_accept = false;

                Err(AcceptError::TooManySessions)
            }
        } else {
            Err(AcceptError::IoError)
        }
    }
}

pub fn start(
    config: TcpListenerConfig,
    max_buffers: usize,
    buffer_size: usize,
    channel: ProxyChannel,
) {
    use crate::server;

    let poll = Poll::new().expect("could not create event loop");
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
    let registry = poll.registry().try_clone().unwrap();
    let mut configuration = Proxy::new(registry, sessions.clone(), backends.clone());
    let _ = configuration.add_listener(config, pool.clone(), token);
    let _ = configuration.activate_listener(&address, None);
    let (scm_server, _scm_client) = UnixStream::pair().unwrap();

    let mut server_config: server::ServerConfig = Default::default();
    server_config.max_connections = max_buffers;
    let mut server = Server::new(
        poll,
        channel,
        ScmSocket::new(scm_server.as_raw_fd()),
        sessions,
        pool,
        backends,
        None,
        None,
        Some(configuration),
        server_config,
        None,
        false,
    );

    info!("starting event loop");
    server.run();
    info!("ending event loop");
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
        let _tx = start_proxy();
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
                            println!("got a new client: {}", count);
                            handle_client(&mut stream, count)
                        });
                    }
                    Err(e) => {
                        println!("connection failed: {:?}", e);
                    }
                }
                count += 1;
            }
        });
    }

    pub fn start_proxy() -> Channel<ProxyRequest, ProxyResponse> {
        use crate::server;

        info!("listen for connections");
        let (mut command, channel) =
            Channel::generate(1000, 10000).expect("should create a channel");
        thread::spawn(move || {
            setup_test_logger!();
            info!("starting event loop");
            let poll = Poll::new().expect("could not create event loop");
            let max_buffers = 100;
            let buffer_size = 16384;
            let pool = Rc::new(RefCell::new(Pool::with_capacity(
                1,
                2 * max_buffers,
                buffer_size,
            )));
            let backends = Rc::new(RefCell::new(BackendMap::new()));

            let mut sessions: Slab<Rc<RefCell<dyn ProxySession>>> =
                Slab::with_capacity(max_buffers);
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

            let sessions = SessionManager::new(sessions, max_buffers);
            let registry = poll.registry().try_clone().unwrap();
            let mut configuration = Proxy::new(registry, sessions.clone(), backends.clone());
            let listener_config = TcpListenerConfig {
                address: "127.0.0.1:1234".parse().unwrap(),
                public_address: None,
                expect_proxy: false,
                front_timeout: 60,
                back_timeout: 30,
                connect_timeout: 3,
            };

            {
                let address = listener_config.address.clone();
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
            let scm = ScmSocket::new(scm_client.into_raw_fd());
            let _ = scm
                .send_listeners(&Listeners {
                    http: Vec::new(),
                    tls: Vec::new(),
                    tcp: Vec::new(),
                })
                .unwrap();

            let mut server_config: server::ServerConfig = Default::default();
            server_config.max_connections = max_buffers;
            let mut s = Server::new(
                poll,
                channel,
                ScmSocket::new(scm_server.into_raw_fd()),
                sessions,
                pool,
                backends,
                None,
                None,
                Some(configuration),
                server_config,
                None,
                false,
            );
            info!("will run");
            s.run();
            info!("ending event loop");
        });

        command.set_blocking(true);
        {
            let front = TcpFrontend {
                cluster_id: String::from("yolo"),
                address: "127.0.0.1:1234".parse().unwrap(),
            };
            let backend = proxy::Backend {
                cluster_id: String::from("yolo"),
                backend_id: String::from("yolo-0"),
                address: "127.0.0.1:5678".parse().unwrap(),
                load_balancing_parameters: Some(LoadBalancingParams::default()),
                sticky_id: None,
                backup: None,
            };

            command.write_message(&ProxyRequest {
                id: String::from("ID_YOLO1"),
                order: ProxyRequestData::AddTcpFrontend(front),
            });
            command.write_message(&ProxyRequest {
                id: String::from("ID_YOLO2"),
                order: ProxyRequestData::AddBackend(backend),
            });
        }
        {
            let front = TcpFrontend {
                cluster_id: String::from("yolo"),
                address: "127.0.0.1:1235".parse().unwrap(),
            };
            let backend = proxy::Backend {
                cluster_id: String::from("yolo"),
                backend_id: String::from("yolo-0"),
                address: "127.0.0.1:5678".parse().unwrap(),
                load_balancing_parameters: Some(LoadBalancingParams::default()),
                sticky_id: None,
                backup: None,
            };
            command.write_message(&ProxyRequest {
                id: String::from("ID_YOLO3"),
                order: ProxyRequestData::AddTcpFrontend(front),
            });
            command.write_message(&ProxyRequest {
                id: String::from("ID_YOLO4"),
                order: ProxyRequestData::AddBackend(backend),
            });
        }

        println!("read_message: {:?}", command.read_message().unwrap());
        println!("read_message: {:?}", command.read_message().unwrap());
        println!("read_message: {:?}", command.read_message().unwrap());
        println!("read_message: {:?}", command.read_message().unwrap());

        command
    }
}
