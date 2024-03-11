use std::{cell::RefCell, net::SocketAddr, rc::Rc};

use mio::{net::TcpStream, Token};
use rusty_ulid::Ulid;
use sozu_command::{
    config::MAX_LOOP_ITERATIONS,
    logging::{EndpointRecord, LogContext},
};

use crate::{
    backends::Backend,
    pool::Checkout,
    protocol::{http::parser::Method, SessionState},
    socket::{stats::socket_rtt, SocketHandler, SocketResult, TransportProtocol},
    sozu_command::ready::Ready,
    timer::TimeoutContainer,
    L7Proxy, ListenerHandler, Protocol, Readiness, SessionMetrics, SessionResult, StateResult,
};

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
    request_id: Ulid,
    session_address: Option<SocketAddr>,
    websocket_context: WebSocketContext,
}

impl<Front: SocketHandler, L: ListenerHandler> Pipe<Front, L> {
    /// Instantiate a new Pipe SessionState with:
    /// - frontend_interest: READABLE | WRITABLE | HUP | ERROR
    /// - frontend_event: EMPTY
    /// - backend_interest: READABLE | WRITABLE | HUP | ERROR
    /// - backend_event: EMPTY
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
            request_id,
            session_address,
            websocket_context,
        };

        trace!("created pipe");
        session
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
                error!("{} could not reset front timeout (pipe)", self.request_id);
            }
        }

        if let Some(t) = self.container_backend_timeout.as_mut() {
            if !t.reset() {
                error!("{} could not reset back timeout (pipe)", self.request_id);
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
            message: message,
            context,
            session_address: self.get_session_address(),
            backend_address: self.get_backend_address(),
            protocol: self.protocol_string(),
            endpoint,
            tags: listener.get_tags(&listener.get_addr().to_string()),
            client_rtt: socket_rtt(self.front_socket()),
            server_rtt: self.backend_socket.as_ref().and_then(socket_rtt),
            service_time: metrics.service_time(),
            response_time: metrics.response_time(),
            bytes_in: metrics.bin,
            bytes_out: metrics.bout,
            user_agent: None
        );
    }

    pub fn log_request_success(&self, metrics: &SessionMetrics) {
        self.log_request(metrics, false, None);
    }

    pub fn log_request_error(&self, metrics: &SessionMetrics, message: &str) {
        incr!("pipe.errors");
        error!(
            "{} Could not process request properly got: {}",
            self.log_context(),
            message
        );
        self.print_state(self.protocol_string());
        self.log_request(metrics, true, Some(message));
    }

    pub fn check_connections(&self) -> bool {
        match (self.frontend_status, self.backend_status) {
            //(ConnectionStatus::Normal, ConnectionStatus::Normal) => true,
            //(ConnectionStatus::Normal, ConnectionStatus::ReadOpen) => true,
            (ConnectionStatus::Normal, ConnectionStatus::WriteOpen) => {
                // technically we should keep it open, but we'll assume that if the front
                // is not readable and there is no in flight data front -> back or back -> front,
                // we'll close the session, otherwise it interacts badly with HTTP connections
                // with Connection: close header and no Content-length
                self.frontend_readiness.event.is_readable()
                    || self.frontend_buffer.available_data() > 0
                    || self.backend_buffer.available_data() > 0
            }
            (ConnectionStatus::Normal, ConnectionStatus::Closed) => {
                self.backend_buffer.available_data() > 0
            }

            (ConnectionStatus::WriteOpen, ConnectionStatus::Normal) => {
                // technically we should keep it open, but we'll assume that if the back
                // is not readable and there is no in flight data back -> front or front -> back, we'll close the session
                self.backend_readiness.event.is_readable()
                    || self.backend_buffer.available_data() > 0
                    || self.frontend_buffer.available_data() > 0
            }
            //(ConnectionStatus::WriteOpen, ConnectionStatus::ReadOpen) => true,
            (ConnectionStatus::WriteOpen, ConnectionStatus::WriteOpen) => {
                self.frontend_buffer.available_data() > 0
                    || self.backend_buffer.available_data() > 0
            }
            (ConnectionStatus::WriteOpen, ConnectionStatus::Closed) => {
                self.backend_buffer.available_data() > 0
            }

            //(ConnectionStatus::ReadOpen, ConnectionStatus::Normal) => true,
            (ConnectionStatus::ReadOpen, ConnectionStatus::ReadOpen) => false,
            //(ConnectionStatus::ReadOpen, ConnectionStatus::WriteOpen) => true,
            (ConnectionStatus::ReadOpen, ConnectionStatus::Closed) => false,

            (ConnectionStatus::Closed, ConnectionStatus::Normal) => {
                self.frontend_buffer.available_data() > 0
            }
            (ConnectionStatus::Closed, ConnectionStatus::ReadOpen) => false,
            (ConnectionStatus::Closed, ConnectionStatus::WriteOpen) => {
                self.frontend_buffer.available_data() > 0
            }
            (ConnectionStatus::Closed, ConnectionStatus::Closed) => false,

            _ => true,
        }
    }

    pub fn frontend_hup(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        self.log_request_success(metrics);
        self.frontend_status = ConnectionStatus::Closed;
        SessionResult::Close
    }

    pub fn backend_hup(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        self.backend_status = ConnectionStatus::Closed;
        if self.backend_buffer.available_data() == 0 {
            if self.backend_readiness.event.is_readable() {
                self.backend_readiness.interest.insert(Ready::READABLE);
                error!("Pipe::back_hup: backend connection closed but the kernel still holds some data. readiness: {:?} -> {:?}", self.frontend_readiness, self.backend_readiness);
                SessionResult::Continue
            } else {
                self.log_request_success(metrics);
                SessionResult::Close
            }
        } else {
            self.frontend_readiness.interest.insert(Ready::WRITABLE);
            if self.backend_readiness.event.is_readable() {
                self.backend_readiness.interest.insert(Ready::READABLE);
            }
            SessionResult::Continue
        }
    }

    // Read content from the session
    pub fn readable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        self.reset_timeouts();

        trace!("pipe readable");
        if self.frontend_buffer.available_space() == 0 {
            self.frontend_readiness.interest.remove(Ready::READABLE);
            self.backend_readiness.interest.insert(Ready::WRITABLE);
            return SessionResult::Continue;
        }

        let (sz, res) = self.frontend.socket_read(self.frontend_buffer.space());
        debug!(
            "{}\tFRONT [{:?}]: read {} bytes",
            self.log_context(),
            self.frontend_token,
            sz
        );

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
        trace!("pipe writable");
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
            "{}\tFRONT [{}<-{:?}]: wrote {} bytes of {}",
            self.log_context(),
            self.frontend_token.0,
            self.backend_token,
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
        trace!("pipe back_writable");

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

        metrics.backend_bout += sz;

        if !self.check_connections() {
            self.frontend_readiness.reset();
            self.backend_readiness.reset();
            self.log_request_success(metrics);
            return SessionResult::Close;
        }

        debug!(
            "{}\tBACK [{:?}->{}]: wrote {} bytes of {}",
            self.log_context(),
            self.backend_token,
            self.frontend_token.0,
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
        self.reset_timeouts();

        trace!("pipe back_readable");
        if self.backend_buffer.available_space() == 0 {
            self.backend_readiness.interest.remove(Ready::READABLE);
            return SessionResult::Continue;
        }

        if let Some(ref mut backend) = self.backend_socket {
            let (size, remaining) = backend.socket_read(self.backend_buffer.space());
            self.backend_buffer.fill(size);

            debug!(
                "{}\tBACK  [{}<-{:?}]: read {} bytes",
                self.log_context(),
                self.frontend_token.0,
                self.backend_token,
                size
            );

            if remaining != SocketResult::Continue || size == 0 {
                self.backend_readiness.event.remove(Ready::READABLE);
            }
            if size > 0 {
                self.frontend_readiness.interest.insert(Ready::WRITABLE);
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

    pub fn log_context(&self) -> LogContext {
        LogContext {
            request_id: self.request_id,
            cluster_id: self.cluster_id.as_deref(),
            backend_id: self.backend_id.as_deref(),
        }
    }

    fn log_endpoint(&self) -> EndpointRecord {
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

        let token = self.frontend_token;
        while counter < MAX_LOOP_ITERATIONS {
            let frontend_interest = self.frontend_readiness.filter_interest();
            let backend_interest = self.backend_readiness.filter_interest();

            trace!(
                "PROXY\t{} {:?} {:?} -> {:?}",
                self.log_context(),
                token,
                self.frontend_readiness,
                self.backend_readiness
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
                    "PROXY session {:?} front error, disconnecting",
                    self.frontend_token
                );

                self.frontend_readiness.interest = Ready::EMPTY;
                self.backend_readiness.interest = Ready::EMPTY;

                return SessionResult::Close;
            }

            if backend_interest.is_error() && self.backend_hup(metrics) == SessionResult::Close {
                self.frontend_readiness.interest = Ready::EMPTY;
                self.backend_readiness.interest = Ready::EMPTY;

                error!(
                    "PROXY session {:?} back error, disconnecting",
                    self.frontend_token
                );
                return SessionResult::Close;
            }

            counter += 1;
        }

        if counter >= MAX_LOOP_ITERATIONS {
            error!(
                "PROXY\thandling session {:?} went through {} iterations, there's a probable infinite loop bug, closing the connection",
                self.frontend_token, MAX_LOOP_ITERATIONS
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
            self.log_request_error(metrics, "front socket timeout");
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

            self.log_request_error(metrics, "back socket timeout");
            return StateResult::CloseSession;
        }

        error!("got timeout for an invalid token");
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
{} Session(Pipe)
\tFrontend:
\t\ttoken: {:?}\treadiness: {:?}
\tBackend:
\t\ttoken: {:?}\treadiness: {:?}",
            context,
            self.frontend_token,
            self.frontend_readiness,
            self.backend_token,
            self.backend_readiness
        );
    }
}
