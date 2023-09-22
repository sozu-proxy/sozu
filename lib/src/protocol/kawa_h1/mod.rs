pub mod answers;
pub mod editor;
pub mod parser;

use std::{
    cell::RefCell,
    io::ErrorKind,
    net::{Shutdown, SocketAddr},
    rc::{Rc, Weak},
};

use kawa;
use mio::{net::TcpStream, Interest, Token};
use rusty_ulid::Ulid;
use time::{Duration, Instant};

use sozu_command::proto::command::{Event, EventKind, ListenerType};

use crate::{
    backends::{Backend, BackendError},
    logs::{Endpoint, LogContext, RequestRecord},
    pool::{Checkout, Pool},
    protocol::{
        http::{editor::HttpContext, parser::Method},
        SessionState,
    },
    retry::RetryPolicy,
    server::{push_event, CONN_RETRIES},
    socket::{stats::socket_rtt, SocketHandler, SocketResult, TransportProtocol},
    sozu_command::ready::Ready,
    timer::TimeoutContainer,
    AcceptError, BackendConnectAction, BackendConnectionError, BackendConnectionStatus,
    L7ListenerHandler, L7Proxy, ListenerHandler, Protocol, ProxySession, Readiness,
    RetrieveClusterError, SessionIsToBeClosed, SessionMetrics, SessionResult, StateResult,
};

/// Generic Http representation using the Kawa crate using the Checkout of Sozu as buffer
type GenericHttpStream = kawa::Kawa<Checkout>;

impl kawa::AsBuffer for Checkout {
    fn as_buffer(&self) -> &[u8] {
        self.inner.extra()
    }
    fn as_mut_buffer(&mut self) -> &mut [u8] {
        self.inner.extra_mut()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStatus {
    Normal,
    /// status, HTTP answer, index in HTTP answer
    DefaultAnswer(DefaultAnswerStatus, Rc<Vec<u8>>, usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultAnswerStatus {
    Answer301,
    Answer400,
    Answer401,
    Answer404,
    Answer408,
    Answer413,
    Answer502,
    Answer503,
    Answer504,
}

#[allow(clippy::from_over_into)]
impl Into<u16> for DefaultAnswerStatus {
    fn into(self) -> u16 {
        match self {
            Self::Answer301 => 301,
            Self::Answer400 => 400,
            Self::Answer401 => 401,
            Self::Answer404 => 404,
            Self::Answer408 => 408,
            Self::Answer413 => 413,
            Self::Answer502 => 502,
            Self::Answer503 => 503,
            Self::Answer504 => 504,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutStatus {
    Request,
    Response,
    WaitingForNewRequest,
    WaitingForResponse,
}

/// Http will be contained in State which itself is contained by Session
pub struct Http<Front: SocketHandler, L: ListenerHandler + L7ListenerHandler> {
    answers: Rc<RefCell<answers::HttpAnswers>>,
    pub backend: Option<Rc<RefCell<Backend>>>,
    backend_connection_status: BackendConnectionStatus,
    pub backend_id: Option<String>,
    pub backend_readiness: Readiness,
    pub backend_socket: Option<TcpStream>,
    backend_stop: Option<Instant>,
    pub backend_token: Option<Token>,
    pub container_backend_timeout: TimeoutContainer,
    pub container_frontend_timeout: TimeoutContainer,
    configured_backend_timeout: Duration,
    configured_connect_timeout: Duration,
    configured_frontend_timeout: Duration,
    pub cluster_id: Option<String>,
    /// attempts to connect to the backends during the session
    connection_attempts: u8,
    pub frontend_readiness: Readiness,
    pub frontend_socket: Front,
    frontend_token: Token,
    keepalive_count: usize,
    listener: Rc<RefCell<L>>,
    pub request_stream: GenericHttpStream,
    pub response_stream: GenericHttpStream,
    status: SessionStatus,
    /// The HTTP context was separated from the State for borrowing reasons.
    /// Calling a kawa parser mutably borrows the State through request_stream or response_stream,
    /// so Http can't be borrowed again to be used in callbacks. HttContext is an independant
    /// subsection of Http that can be mutably borrowed for parser callbacks.
    pub context: HttpContext,
}

impl<Front: SocketHandler, L: ListenerHandler + L7ListenerHandler> Http<Front, L> {
    /// Instantiate a new HTTP SessionState with:
    /// - frontend_interest: READABLE | HUP | ERROR
    /// - frontend_event: EMPTY
    /// - backend_interest: EMPTY
    /// - backend_event: EMPTY
    /// Remember to set the events from the previous State!
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        answers: Rc<RefCell<answers::HttpAnswers>>,
        configured_backend_timeout: Duration,
        configured_connect_timeout: Duration,
        configured_frontend_timeout: Duration,
        container_frontend_timeout: TimeoutContainer,
        frontend_socket: Front,
        frontend_token: Token,
        listener: Rc<RefCell<L>>,
        pool: Weak<RefCell<Pool>>,
        protocol: Protocol,
        public_address: SocketAddr,
        request_id: Ulid,
        session_address: Option<SocketAddr>,
        sticky_name: String,
    ) -> Result<Http<Front, L>, AcceptError> {
        let (front_buffer, back_buffer) = match pool.upgrade() {
            Some(pool) => {
                let mut pool = pool.borrow_mut();
                match (pool.checkout(), pool.checkout()) {
                    (Some(front_buffer), Some(back_buffer)) => (front_buffer, back_buffer),
                    _ => return Err(AcceptError::BufferCapacityReached),
                }
            }
            None => return Err(AcceptError::BufferCapacityReached),
        };
        Ok(Http {
            answers,
            backend_connection_status: BackendConnectionStatus::NotConnected,
            backend_id: None,
            backend_readiness: Readiness::new(),
            backend_socket: None,
            backend_stop: None,
            backend_token: None,
            backend: None,
            cluster_id: None,
            configured_backend_timeout,
            configured_connect_timeout,
            configured_frontend_timeout,
            connection_attempts: 0,
            container_backend_timeout: TimeoutContainer::new_empty(configured_connect_timeout),
            container_frontend_timeout,
            frontend_readiness: Readiness {
                interest: Ready::READABLE | Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            frontend_socket,
            frontend_token,
            keepalive_count: 0,
            listener,
            request_stream: GenericHttpStream::new(
                kawa::Kind::Request,
                kawa::Buffer::new(front_buffer),
            ),
            response_stream: GenericHttpStream::new(
                kawa::Kind::Response,
                kawa::Buffer::new(back_buffer),
            ),
            status: SessionStatus::Normal,
            context: HttpContext {
                closing: false,
                id: request_id,
                keep_alive_backend: true,
                keep_alive_frontend: true,
                protocol,
                public_address,
                session_address,
                sticky_name,
                sticky_session: None,
                sticky_session_found: None,

                method: None,
                authority: None,
                path: None,
                status: None,
                reason: None,
                user_agent: None,
            },
        })
    }

    /// Reset the connection in case of keep-alive to be ready for the next request
    pub fn reset(&mut self) {
        trace!("==============reset");
        self.context.keep_alive_frontend = true;
        self.context.keep_alive_backend = true;
        self.context.sticky_session_found = None;
        self.context.id = Ulid::generate();

        self.request_stream.clear();
        self.response_stream.clear();
        self.keepalive_count += 1;
        gauge_add!("http.active_requests", -1);

        if let Some(backend) = &mut self.backend {
            let mut backend = backend.borrow_mut();
            backend.active_requests = backend.active_requests.saturating_sub(1);
        }

        // reset the front timeout and cancel the back timeout while we are
        // waiting for a new request
        self.container_frontend_timeout.reset();
        self.container_backend_timeout.cancel();
        self.frontend_readiness.interest = Ready::READABLE | Ready::HUP | Ready::ERROR;
        self.backend_readiness.interest = Ready::HUP | Ready::ERROR;
    }

    pub fn readable(&mut self, metrics: &mut SessionMetrics) -> StateResult {
        trace!("==============readable");
        if !self.container_frontend_timeout.reset() {
            error!(
                "could not reset front timeout {:?}",
                self.configured_frontend_timeout
            );
            self.print_state(&format!("{:?}", self.context.protocol));
        }

        if let SessionStatus::DefaultAnswer(_, _, _) = self.status {
            error!(
                "{}\tsending default answer, should not read from front socket",
                self.log_context()
            );
            self.frontend_readiness.interest.remove(Ready::READABLE);
            self.frontend_readiness.interest.insert(Ready::WRITABLE);
            return StateResult::Continue;
        }

        if self.request_stream.storage.is_full() {
            self.frontend_readiness.interest.remove(Ready::READABLE);
            if self.request_stream.is_main_phase() {
                self.backend_readiness.interest.insert(Ready::WRITABLE);
            } else {
                // client has filled its buffer and we can't empty it
                self.set_answer(DefaultAnswerStatus::Answer413, None);
            }
            return StateResult::Continue;
        }

        let (size, socket_state) = self
            .frontend_socket
            .socket_read(self.request_stream.storage.space());
        debug!(
            "{}\tFRONT [{}->{:?}]: read {} bytes",
            self.log_context(),
            self.frontend_token.0,
            self.backend_token,
            size
        );

        if size > 0 {
            self.request_stream.storage.fill(size);
            count!("bytes_in", size as i64);
            metrics.bin += size;
            // if self.kawa_request.storage.is_full() {
            //     self.frontend_readiness.interest.remove(Ready::READABLE);
            // }
        } else {
            self.frontend_readiness.event.remove(Ready::READABLE);
        }

        match socket_state {
            SocketResult::Error | SocketResult::Closed => {
                if self.request_stream.is_initial() {
                    // count an error if we were waiting for the first request
                    // otherwise, if we already had one completed request and response,
                    // and are waiting for the next one, we do not count a socket
                    // closing abruptly as an error
                    if self.keepalive_count == 0 {
                        self.frontend_socket.read_error();
                    }
                } else {
                    self.frontend_socket.read_error();
                    self.log_request_error(
                        metrics,
                        &format!(
                            "front socket {socket_state:?}, closing the session. Readiness: {:?} -> {:?}, read {size} bytes",
                            self.frontend_readiness,
                            self.backend_readiness,
                        )
                    );
                }
                return StateResult::CloseSession;
            }
            SocketResult::WouldBlock => {
                self.frontend_readiness.event.remove(Ready::READABLE);
            }
            SocketResult::Continue => {}
        };

        self.readable_parse(metrics)
    }

    pub fn readable_parse(&mut self, _metrics: &mut SessionMetrics) -> StateResult {
        trace!("==============readable_parse");
        let was_initial = self.request_stream.is_initial();

        kawa::h1::parse(&mut self.request_stream, &mut self.context);
        // kawa::debug_kawa(&self.request_stream);

        if was_initial && !self.request_stream.is_initial() {
            gauge_add!("http.active_requests", 1);
            incr!("http.requests");
        }

        if self.request_stream.is_error() {
            incr!("http.frontend_parse_errors");
            // was_initial is maybe too restrictive
            // from what I understand the only condition should be:
            // "did we already sent byte?"
            if was_initial {
                self.set_answer(DefaultAnswerStatus::Answer400, None);
                // gauge_add!("http.active_requests", 1);
                return StateResult::Continue;
            } else {
                return StateResult::CloseSession;
            }
        }

        if self.request_stream.is_main_phase() {
            self.backend_readiness.interest.insert(Ready::WRITABLE);
            // if it was the first request, the front timeout duration
            // was set to request_timeout, which is much lower. For future
            // requests on this connection, we can wait a bit more
            self.container_frontend_timeout
                .set_duration(self.configured_frontend_timeout);
        }
        if self.request_stream.is_terminated() {
            self.frontend_readiness.interest.remove(Ready::READABLE);
        }

        if was_initial {
            if let kawa::StatusLine::Request { .. } = self.request_stream.detached.status_line {
                trace!("============== HANDLE CONNECTION!");
                return StateResult::ConnectBackend;
            }
        }
        StateResult::Continue
    }

    pub fn writable(&mut self, metrics: &mut SessionMetrics) -> StateResult {
        trace!("==============writable");
        //handle default answers
        if let SessionStatus::DefaultAnswer(_, _, _) = self.status {
            return self.writable_default_answer(metrics);
        }

        self.response_stream.prepare(&mut kawa::h1::BlockConverter);

        let bufs = self.response_stream.as_io_slice();
        if bufs.is_empty() {
            self.frontend_readiness.interest.remove(Ready::WRITABLE);
            return StateResult::Continue;
        }

        let (size, socket_state) = self.frontend_socket.socket_write_vectored(&bufs);
        debug!(
            "{}\tFRONT [{}<-{:?}]: wrote {} bytes",
            self.log_context(),
            self.frontend_token.0,
            self.backend_token,
            size
        );

        if size > 0 {
            self.response_stream.consume(size);
            count!("bytes_out", size as i64);
            metrics.bout += size;
            self.backend_readiness.interest.insert(Ready::READABLE);
        } else {
            self.frontend_readiness.event.remove(Ready::WRITABLE);
        }

        match socket_state {
            SocketResult::Error | SocketResult::Closed => {
                self.frontend_socket.write_error();
                self.log_request_error(
                    metrics,
                    &format!(
                        "front socket {socket_state:?}, closing session.  Readiness: {:?} -> {:?}, read {size} bytes",
                        self.frontend_readiness,
                        self.backend_readiness,
                    ),
                );
                return StateResult::CloseSession;
            }
            SocketResult::WouldBlock => {
                self.frontend_readiness.event.remove(Ready::WRITABLE);
            }
            SocketResult::Continue => {}
        }

        if self.response_stream.is_terminated() && self.response_stream.is_completed() {
            save_http_status_metric(self.context.status, self.log_context());

            self.log_request_success(metrics);
            metrics.reset();

            if self.context.closing {
                debug!("{} closing proxy, no keep alive", self.log_context());
                return StateResult::CloseSession;
            }

            match self.response_stream.detached.status_line {
                kawa::StatusLine::Response { code: 101, .. } => {
                    trace!("============== HANDLE UPGRADE!");
                    return StateResult::Upgrade;
                }
                kawa::StatusLine::Response { code: 100, .. } => {
                    trace!("============== HANDLE CONTINUE!");
                    self.response_stream.clear();
                    return StateResult::Continue;
                }
                kawa::StatusLine::Response { code: 103, .. } => {
                    self.backend_readiness.event.insert(Ready::READABLE);
                    trace!("============== HANDLE EARLY HINT!");
                    self.response_stream.clear();
                    return StateResult::Continue;
                }
                _ => (),
            }

            // FIXME: we could get smarter about this
            // with no keepalive on backend, we could open a new backend ConnectionError
            // with no keepalive on front but keepalive on backend, we could have
            // a pool of connections
            trace!(
                "============== HANDLE KEEP-ALIVE: {} {}",
                self.context.keep_alive_frontend,
                self.context.keep_alive_backend
            );
            return match (
                self.context.keep_alive_frontend,
                self.context.keep_alive_backend,
            ) {
                (true, true) => {
                    debug!("{} keep alive front/back", self.log_context());
                    self.reset();
                    StateResult::Continue
                }
                (true, false) => {
                    debug!("{} keep alive front", self.log_context());
                    self.reset();
                    StateResult::CloseBackend
                }
                _ => {
                    debug!("{} no keep alive", self.log_context());
                    StateResult::CloseSession
                }
            };
        }
        StateResult::Continue
    }

    pub fn backend_writable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        trace!("==============backend_writable");
        if let SessionStatus::DefaultAnswer(_, _, _) = self.status {
            error!(
                "{}\tsending default answer, should not write to back",
                self.log_context()
            );
            self.backend_readiness.interest.remove(Ready::WRITABLE);
            self.frontend_readiness.interest.insert(Ready::WRITABLE);
            return SessionResult::Continue;
        }

        let backend_socket = if let Some(backend_socket) = &mut self.backend_socket {
            backend_socket
        } else {
            self.log_request_error(metrics, "back socket not found, closing session");
            return SessionResult::Close;
        };

        self.request_stream.prepare(&mut kawa::h1::BlockConverter);

        let bufs = self.request_stream.as_io_slice();
        if bufs.is_empty() {
            self.backend_readiness.interest.remove(Ready::WRITABLE);
            return SessionResult::Continue;
        }

        let (size, socket_state) = backend_socket.socket_write_vectored(&bufs);
        debug!(
            "{}\tBACK [{}->{:?}]: wrote {} bytes",
            self.log_context(),
            self.frontend_token.0,
            self.backend_token,
            size
        );

        if size > 0 {
            self.request_stream.consume(size);
            count!("back_bytes_out", size as i64);
            metrics.backend_bout += size;
            self.frontend_readiness.interest.insert(Ready::READABLE);
            self.backend_readiness.interest.insert(Ready::READABLE);
        } else {
            self.backend_readiness.event.remove(Ready::WRITABLE);
        }

        match socket_state {
            // the back socket is not writable anymore, so we can drop
            // the front buffer, no more data can be transmitted.
            // But the socket might still be readable, or if it is
            // closed, we might still have some data in the buffer.
            // As an example, we can get an early response for a large
            // POST request to refuse it and prevent all of the data
            // from being transmitted, with the backend server closing
            // the socket right after sending the response
            // FIXME: shouldn't we test for response_state then?
            SocketResult::Error | SocketResult::Closed => {
                self.frontend_readiness.interest.remove(Ready::READABLE);
                self.backend_readiness.interest.remove(Ready::WRITABLE);
                return SessionResult::Continue;
            }
            SocketResult::WouldBlock => {
                self.backend_readiness.event.remove(Ready::WRITABLE);
            }
            SocketResult::Continue => {}
        }

        if self.request_stream.is_terminated() && self.request_stream.is_completed() {
            self.backend_readiness.interest.remove(Ready::WRITABLE);

            // cancel the front timeout while we are waiting for the server to answer
            self.container_frontend_timeout.cancel();
            self.container_backend_timeout.reset();
        }
        SessionResult::Continue
    }

    // Read content from cluster
    pub fn backend_readable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        trace!("==============backend_readable");
        if !self.container_backend_timeout.reset() {
            error!(
                "could not reset back timeout {:?}",
                self.configured_backend_timeout
            );
            self.print_state(&format!("{:?}", self.context.protocol));
        }

        if let SessionStatus::DefaultAnswer(_, _, _) = self.status {
            error!(
                "{}\tsending default answer, should not read from back socket",
                self.log_context()
            );
            self.backend_readiness.interest.remove(Ready::READABLE);
            self.frontend_readiness.interest.insert(Ready::WRITABLE);
            return SessionResult::Continue;
        }

        let backend_socket = if let Some(backend_socket) = &mut self.backend_socket {
            backend_socket
        } else {
            self.log_request_error(metrics, "back socket not found, closing session");
            return SessionResult::Close;
        };

        if self.response_stream.storage.is_full() {
            self.backend_readiness.interest.remove(Ready::READABLE);
            if self.response_stream.is_main_phase() {
                self.frontend_readiness.interest.insert(Ready::WRITABLE);
            } else {
                // server has filled its buffer and we can't empty it
                // FIXME: what error code should we use?
                self.set_answer(DefaultAnswerStatus::Answer502, None);
            }
            return SessionResult::Continue;
        }

        let (size, socket_state) = backend_socket.socket_read(self.response_stream.storage.space());
        debug!(
            "{}\tBACK  [{}<-{:?}]: read {} bytes",
            "ctx", // self.log_context(),
            self.frontend_token.0,
            self.backend_token,
            size
        );

        if size > 0 {
            self.response_stream.storage.fill(size);
            count!("back_bytes_in", size as i64);
            metrics.backend_bin += size;
            // if self.kawa_response.storage.is_full() {
            //     self.backend_readiness.interest.remove(Ready::READABLE);
            // }
        } else {
            self.backend_readiness.event.remove(Ready::READABLE);
        }

        // TODO: close delimited and backend_hup should be handled better
        match socket_state {
            SocketResult::Error => {
                backend_socket.read_error();
                self.log_request_error(
                    metrics,
                    &format!(
                        "back socket {socket_state:?}, closing session.  Readiness: {:?} -> {:?}, read {size} bytes",
                        self.frontend_readiness,
                        self.backend_readiness,
                    ),
                );
                return SessionResult::Close;
            }
            SocketResult::WouldBlock | SocketResult::Closed => {
                self.backend_readiness.event.remove(Ready::READABLE);
            }
            SocketResult::Continue => {}
        }

        self.backend_readable_parse(metrics)
    }

    pub fn backend_readable_parse(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        trace!("==============backend_readable_parse");
        let was_initial = self.response_stream.is_initial();

        kawa::h1::parse(&mut self.response_stream, &mut self.context);
        // kawa::debug_kawa(&self.response_stream);

        if self.response_stream.is_error() {
            incr!("http.backend_parse_errors");
            // was_initial is maybe too restrictive
            // from what I understand the only condition should be:
            // "did we already sent byte?"
            if was_initial {
                self.set_answer(DefaultAnswerStatus::Answer502, None);
                return SessionResult::Continue;
            } else {
                return SessionResult::Close;
            }
        }

        if self.response_stream.is_main_phase() {
            self.frontend_readiness.interest.insert(Ready::WRITABLE);
        }
        if self.response_stream.is_terminated() {
            metrics.backend_stop();
            self.backend_stop = Some(Instant::now());
            self.backend_readiness.interest.remove(Ready::READABLE);
        }
        SessionResult::Continue
    }
}

impl<Front: SocketHandler, L: ListenerHandler + L7ListenerHandler> Http<Front, L> {
    fn log_context(&self) -> LogContext {
        LogContext {
            request_id: self.context.id,
            cluster_id: self.cluster_id.as_deref(),
            backend_id: self.backend_id.as_deref(),
        }
    }

    pub fn get_session_address(&self) -> Option<SocketAddr> {
        self.context
            .session_address
            .or_else(|| self.frontend_socket.socket_ref().peer_addr().ok())
    }

    pub fn get_backend_address(&self) -> Option<SocketAddr> {
        self.backend
            .as_ref()
            .map(|backend| backend.borrow().address)
            .or_else(|| {
                self.backend_socket
                    .as_ref()
                    .and_then(|backend| backend.peer_addr().ok())
            })
    }

    // The protocol name used in the access logs
    fn protocol_string(&self) -> &'static str {
        match self.context.protocol {
            Protocol::HTTP => "HTTP",
            Protocol::HTTPS => match self.frontend_socket.protocol() {
                TransportProtocol::Ssl2 => "HTTPS-SSL2",
                TransportProtocol::Ssl3 => "HTTPS-SSL3",
                TransportProtocol::Tls1_0 => "HTTPS-TLS1.0",
                TransportProtocol::Tls1_1 => "HTTPS-TLS1.1",
                TransportProtocol::Tls1_2 => "HTTPS-TLS1.2",
                TransportProtocol::Tls1_3 => "HTTPS-TLS1.3",
                _ => unreachable!(),
            },
            _ => unreachable!(),
        }
    }

    /// Format the context of the websocket into a loggable String
    pub fn websocket_context(&self) -> String {
        format!(
            "{}",
            Endpoint::Http {
                method: self.context.method.as_ref(),
                authority: self.context.authority.as_deref(),
                path: self.context.path.as_deref(),
                status: self.context.status,
                reason: self.context.reason.as_deref(),
            }
        )
    }

    pub fn log_request(&self, metrics: &SessionMetrics, message: Option<&str>) {
        let listener = self.listener.borrow();
        let tags = self.context.authority.as_ref().and_then(|host| {
            let hostname = match host.split_once(':') {
                None => host,
                Some((hostname, _)) => hostname,
            };
            listener.get_concatenated_tags(hostname)
        });
        let status = match self.status {
            SessionStatus::Normal => self.context.status,
            SessionStatus::DefaultAnswer(answers, ..) => Some(answers.into()),
        };

        RequestRecord {
            error: message,
            context: self.log_context(),
            session_address: self.get_session_address(),
            backend_address: self.get_backend_address(),
            protocol: self.protocol_string(),
            endpoint: Endpoint::Http {
                method: self.context.method.as_ref(),
                authority: self.context.authority.as_deref(),
                path: self.context.path.as_deref(),
                status,
                reason: self.context.reason.as_deref(),
            },
            tags,
            client_rtt: socket_rtt(self.front_socket()),
            server_rtt: self.backend_socket.as_ref().and_then(socket_rtt),
            metrics,
            user_agent: self.context.user_agent.as_deref(),
        }
        .log();
    }

    pub fn log_request_success(&self, metrics: &SessionMetrics) {
        self.log_request(metrics, None);
    }
    pub fn log_default_answer_success(&self, metrics: &SessionMetrics) {
        self.log_request(metrics, None);
    }
    pub fn log_request_error(&mut self, metrics: &mut SessionMetrics, message: &str) {
        incr!("http.errors");
        self.log_request(metrics, Some(message));
    }

    pub fn set_answer(&mut self, answer: DefaultAnswerStatus, buf: Option<Rc<Vec<u8>>>) {
        if let SessionStatus::DefaultAnswer(status, _, _) = self.status {
            error!(
                "already set the default answer to {:?}, trying to set to {:?}",
                status, answer
            );
        } else {
            match answer {
                DefaultAnswerStatus::Answer301 => incr!(
                    "http.301.redirection",
                    self.cluster_id.as_deref(),
                    self.backend_id.as_deref()
                ),
                DefaultAnswerStatus::Answer400 => incr!("http.400.errors"),
                DefaultAnswerStatus::Answer401 => incr!(
                    "http.401.errors",
                    self.cluster_id.as_deref(),
                    self.backend_id.as_deref()
                ),
                DefaultAnswerStatus::Answer404 => incr!("http.404.errors"),
                DefaultAnswerStatus::Answer408 => incr!(
                    "http.408.errors",
                    self.cluster_id.as_deref(),
                    self.backend_id.as_deref()
                ),
                DefaultAnswerStatus::Answer413 => incr!(
                    "http.413.errors",
                    self.cluster_id.as_deref(),
                    self.backend_id.as_deref()
                ),
                DefaultAnswerStatus::Answer502 => incr!(
                    "http.502.errors",
                    self.cluster_id.as_deref(),
                    self.backend_id.as_deref()
                ),
                DefaultAnswerStatus::Answer503 => incr!(
                    "http.503.errors",
                    self.cluster_id.as_deref(),
                    self.backend_id.as_deref()
                ),
                DefaultAnswerStatus::Answer504 => incr!(
                    "http.504.errors",
                    self.cluster_id.as_deref(),
                    self.backend_id.as_deref()
                ),
            };
        }

        let buf = buf.unwrap_or_else(|| {
            self.answers
                .borrow()
                .get(answer, self.cluster_id.as_deref())
        });
        self.status = SessionStatus::DefaultAnswer(answer, buf, 0);
        self.frontend_readiness.interest = Ready::WRITABLE | Ready::HUP | Ready::ERROR;
        self.backend_readiness.interest = Ready::HUP | Ready::ERROR;
    }

    fn writable_default_answer(&mut self, metrics: &mut SessionMetrics) -> StateResult {
        let res = match self.status {
            SessionStatus::DefaultAnswer(_, ref buf, mut index) => {
                let len = buf.len();

                let mut sz = 0usize;
                let mut res = SocketResult::Continue;
                while res == SocketResult::Continue && index < len {
                    let (current_sz, current_res) =
                        self.frontend_socket.socket_write(&buf[index..]);
                    res = current_res;
                    sz += current_sz;
                    index += current_sz;
                }

                count!("bytes_out", sz as i64);
                metrics.bout += sz;

                if res != SocketResult::Continue {
                    self.frontend_readiness.event.remove(Ready::WRITABLE);
                }

                if index == len {
                    self.log_default_answer_success(metrics);
                    self.frontend_readiness.reset();
                    self.backend_readiness.reset();
                    return StateResult::CloseSession;
                }

                res
            }
            _ => return StateResult::CloseSession,
        };

        if res == SocketResult::Error {
            self.frontend_socket.write_error();
            self.log_request_error(
                metrics,
                "error writing default answer to front socket, closing",
            );
            StateResult::CloseSession
        } else {
            StateResult::Continue
        }
    }

    pub fn test_backend_socket(&self) -> bool {
        match self.backend_socket {
            Some(ref s) => {
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

    pub fn is_valid_backend_socket(&self) -> bool {
        // if socket was last used in the last second, test it
        match self.backend_stop.as_ref() {
            Some(stop_instant) => {
                let now = Instant::now();
                let dur = now - *stop_instant;
                if dur > Duration::seconds(1) {
                    return self.test_backend_socket();
                }
            }
            None => return self.test_backend_socket(),
        }

        true
    }

    pub fn set_backend_socket(&mut self, socket: TcpStream, backend: Option<Rc<RefCell<Backend>>>) {
        self.backend_socket = Some(socket);
        self.backend = backend;
    }

    pub fn set_cluster_id(&mut self, cluster_id: String) {
        self.cluster_id = Some(cluster_id);
    }

    pub fn set_backend_id(&mut self, backend_id: String) {
        self.backend_id = Some(backend_id);
    }

    pub fn set_backend_token(&mut self, token: Token) {
        self.backend_token = Some(token);
    }

    pub fn clear_backend_token(&mut self) {
        self.backend_token = None;
    }

    pub fn set_backend_timeout(&mut self, dur: Duration) {
        if let Some(token) = self.backend_token.as_ref() {
            self.container_backend_timeout.set_duration(dur);
            self.container_backend_timeout.set(*token);
        }
    }

    pub fn front_socket(&self) -> &TcpStream {
        self.frontend_socket.socket_ref()
    }

    /// WARNING: this function removes the backend entry in the session manager
    /// IF the backend_token is set, so that entry can be reused for new backend.
    /// I don't think this is a good idea, but it is a quick fix
    fn close_backend(&mut self, proxy: Rc<RefCell<dyn L7Proxy>>, metrics: &mut SessionMetrics) {
        self.container_backend_timeout.cancel();
        debug!(
            "{}\tPROXY [{}->{}] CLOSED BACKEND",
            self.log_context(),
            self.frontend_token.0,
            self.backend_token
                .map(|t| format!("{}", t.0))
                .unwrap_or_else(|| "-".to_string())
        );

        let proxy = proxy.borrow();
        if let Some(socket) = &mut self.backend_socket.take() {
            if let Err(e) = proxy.deregister_socket(socket) {
                error!("error deregistering back socket({:?}): {:?}", socket, e);
            }
            if let Err(e) = socket.shutdown(Shutdown::Both) {
                if e.kind() != ErrorKind::NotConnected {
                    error!("error shutting down back socket({:?}): {:?}", socket, e);
                }
            }
        }

        if let Some(token) = self.backend_token.take() {
            proxy.remove_session(token);

            if self.backend_connection_status != BackendConnectionStatus::NotConnected {
                self.backend_readiness.event = Ready::EMPTY;
            }

            if self.backend_connection_status == BackendConnectionStatus::Connected {
                gauge_add!("backend.connections", -1);
                gauge_add!(
                    "connections_per_backend",
                    -1,
                    self.cluster_id.as_deref(),
                    metrics.backend_id.as_deref()
                );
            }

            self.set_backend_connected(BackendConnectionStatus::NotConnected, metrics);

            if let Some(backend) = self.backend.take() {
                backend.borrow_mut().dec_connections();
            }
        }
    }

    /// Check the number of connection attempts against authorized connection retries
    fn check_circuit_breaker(&mut self) -> Result<(), BackendConnectionError> {
        if self.connection_attempts >= CONN_RETRIES {
            error!("{} max connection attempt reached", self.log_context());
            self.set_answer(DefaultAnswerStatus::Answer503, None);
            return Err(BackendConnectionError::MaxConnectionRetries(None));
        }
        Ok(())
    }

    fn check_backend_connection(&mut self, metrics: &mut SessionMetrics) -> bool {
        let is_valid_backend_socket = self.is_valid_backend_socket();

        if !is_valid_backend_socket {
            return false;
        }

        //matched on keepalive
        metrics.backend_id = self.backend.as_ref().map(|i| i.borrow().backend_id.clone());

        metrics.backend_start();
        if let Some(b) = self.backend.as_mut() {
            b.borrow_mut().active_requests += 1;
        }
        true
    }

    // -> host, path, method
    pub fn extract_route(&self) -> Result<(&str, &str, &Method), RetrieveClusterError> {
        let given_method = self
            .context
            .method
            .as_ref()
            .ok_or(RetrieveClusterError::NoMethod)?;
        let given_authority = self
            .context
            .authority
            .as_deref()
            .ok_or(RetrieveClusterError::NoHost)?;
        let given_path = self
            .context
            .path
            .as_deref()
            .ok_or(RetrieveClusterError::NoPath)?;

        Ok((given_authority, given_path, given_method))
    }

    fn cluster_id_from_request(
        &mut self,
        proxy: Rc<RefCell<dyn L7Proxy>>,
    ) -> Result<String, RetrieveClusterError> {
        let (host, uri, method) = match self.extract_route() {
            Ok(tuple) => tuple,
            Err(cluster_error) => {
                self.set_answer(DefaultAnswerStatus::Answer400, None);
                return Err(cluster_error);
            }
        };

        let cluster_result = self
            .listener
            .borrow()
            .frontend_from_request(host, uri, method);

        let cluster_id = match cluster_result {
            Ok(route) => route,
            Err(frontend_error) => {
                self.set_answer(DefaultAnswerStatus::Answer404, None);
                return Err(RetrieveClusterError::RetrieveFrontend(frontend_error));
            }
        };

        let frontend_should_redirect_https = matches!(proxy.borrow().kind(), ListenerType::Http)
            && proxy
                .borrow()
                .clusters()
                .get(&cluster_id)
                .map(|cluster| cluster.https_redirect)
                .unwrap_or(false);

        if frontend_should_redirect_https {
            let answer = format!("HTTP/1.1 301 Moved Permanently\r\nContent-Length: 0\r\nLocation: https://{host}{uri}\r\n\r\n");
            self.set_answer(
                DefaultAnswerStatus::Answer301,
                Some(Rc::new(answer.into_bytes())),
            );
            return Err(RetrieveClusterError::UnauthorizedRoute);
        }

        Ok(cluster_id)
    }

    pub fn backend_from_request(
        &mut self,
        cluster_id: &str,
        frontend_should_stick: bool,
        proxy: Rc<RefCell<dyn L7Proxy>>,
        metrics: &mut SessionMetrics,
    ) -> Result<TcpStream, BackendConnectionError> {
        let (backend, conn) = self
            .get_backend_for_sticky_session(
                frontend_should_stick,
                self.context.sticky_session_found.as_deref(),
                cluster_id,
                proxy,
            )
            .map_err(|backend_error| {
                self.set_answer(DefaultAnswerStatus::Answer503, None);
                BackendConnectionError::Backend(backend_error)
            })?;

        if frontend_should_stick {
            // update sticky name in case it changed I guess?
            self.context.sticky_name = self.listener.borrow().get_sticky_name().to_string();

            self.context.sticky_session = Some(
                backend
                    .borrow()
                    .sticky_id
                    .clone()
                    .unwrap_or_else(|| backend.borrow().backend_id.clone()),
            );
        }

        metrics.backend_id = Some(backend.borrow().backend_id.clone());
        metrics.backend_start();
        self.set_backend_id(backend.borrow().backend_id.clone());

        self.backend = Some(backend);
        Ok(conn)
    }

    fn get_backend_for_sticky_session(
        &self,
        frontend_should_stick: bool,
        sticky_session: Option<&str>,
        cluster_id: &str,
        proxy: Rc<RefCell<dyn L7Proxy>>,
    ) -> Result<(Rc<RefCell<Backend>>, TcpStream), BackendError> {
        match (frontend_should_stick, sticky_session) {
            (true, Some(sticky_session)) => proxy
                .borrow()
                .backends()
                .borrow_mut()
                .backend_from_sticky_session(cluster_id, sticky_session),
            _ => proxy
                .borrow()
                .backends()
                .borrow_mut()
                .backend_from_cluster_id(cluster_id),
        }
    }

    fn connect_to_backend(
        &mut self,
        session_rc: Rc<RefCell<dyn ProxySession>>,
        proxy: Rc<RefCell<dyn L7Proxy>>,
        metrics: &mut SessionMetrics,
    ) -> Result<BackendConnectAction, BackendConnectionError> {
        let old_cluster_id = self.cluster_id.clone();
        let old_backend_token = self.backend_token;

        self.check_circuit_breaker()?;

        let cluster_id = self
            .cluster_id_from_request(proxy.clone())
            .map_err(BackendConnectionError::RetrieveClusterError)?;

        trace!(
            "connect_to_backend: {:?} {:?} {:?}",
            self.cluster_id,
            cluster_id,
            self.backend_connection_status
        );
        // check if we can reuse the backend connection
        if (self.cluster_id.as_ref()) == Some(&cluster_id)
            && self.backend_connection_status == BackendConnectionStatus::Connected
        {
            let has_backend = self
                .backend
                .as_ref()
                .map(|backend| {
                    let backend = backend.borrow();
                    proxy
                        .borrow()
                        .backends()
                        .borrow()
                        .has_backend(&cluster_id, &backend)
                })
                .unwrap_or(false);

            if has_backend && self.check_backend_connection(metrics) {
                return Ok(BackendConnectAction::Reuse);
            } else if self.backend_token.take().is_some() {
                self.close_backend(proxy.clone(), metrics);
            }
        }

        //replacing with a connection to another cluster
        if old_cluster_id.is_some()
            && old_cluster_id.as_ref() != Some(&cluster_id)
            && self.backend_token.take().is_some()
        {
            self.close_backend(proxy.clone(), metrics);
        }

        self.cluster_id = Some(cluster_id.clone());

        let frontend_should_stick = proxy
            .borrow()
            .clusters()
            .get(&cluster_id)
            .map(|cluster| cluster.sticky_session)
            .unwrap_or(false);

        let mut socket =
            self.backend_from_request(&cluster_id, frontend_should_stick, proxy.clone(), metrics)?;
        if let Err(e) = socket.set_nodelay(true) {
            error!(
                "error setting nodelay on back socket({:?}): {:?}",
                socket, e
            );
        }

        self.backend_readiness.interest = Ready::WRITABLE | Ready::HUP | Ready::ERROR;
        self.backend_connection_status = BackendConnectionStatus::Connecting(Instant::now());

        match old_backend_token {
            Some(backend_token) => {
                self.set_backend_token(backend_token);
                if let Err(e) = proxy.borrow().register_socket(
                    &mut socket,
                    backend_token,
                    Interest::READABLE | Interest::WRITABLE,
                ) {
                    error!("error registering back socket({:?}): {:?}", socket, e);
                }

                self.set_backend_socket(socket, self.backend.clone());
                self.set_backend_timeout(self.configured_connect_timeout);

                Ok(BackendConnectAction::Replace)
            }
            None => {
                let backend_token = proxy.borrow().add_session(session_rc);

                if let Err(e) = proxy.borrow().register_socket(
                    &mut socket,
                    backend_token,
                    Interest::READABLE | Interest::WRITABLE,
                ) {
                    error!("error registering back socket({:?}): {:?}", socket, e);
                }

                self.set_backend_socket(socket, self.backend.clone());
                self.set_backend_token(backend_token);
                self.set_backend_timeout(self.configured_connect_timeout);

                Ok(BackendConnectAction::New)
            }
        }
    }

    fn set_backend_connected(
        &mut self,
        connected: BackendConnectionStatus,
        metrics: &mut SessionMetrics,
    ) {
        let last = self.backend_connection_status;
        self.backend_connection_status = connected;

        if connected == BackendConnectionStatus::Connected {
            gauge_add!("backend.connections", 1);
            gauge_add!(
                "connections_per_backend",
                1,
                self.cluster_id.as_deref(),
                metrics.backend_id.as_deref()
            );

            // the back timeout was of connect_timeout duration before,
            // now that we're connected, move to backend_timeout duration
            self.set_backend_timeout(self.configured_backend_timeout);
            // if we are not waiting for the backend response, its timeout is concelled
            // it should be set when the request has been entirely transmitted
            if !self.backend_readiness.interest.is_readable() {
                self.container_backend_timeout.cancel();
            }

            if let Some(backend) = &self.backend {
                let mut backend = backend.borrow_mut();

                if backend.retry_policy.is_down() {
                    incr!(
                        "backend.up",
                        self.cluster_id.as_deref(),
                        metrics.backend_id.as_deref()
                    );

                    info!(
                        "backend server {} at {} is up",
                        backend.backend_id, backend.address
                    );

                    push_event(Event {
                        kind: EventKind::BackendUp as i32,
                        backend_id: Some(backend.backend_id.to_owned()),
                        address: Some(backend.address.to_string()),
                        cluster_id: None,
                    });
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

    fn fail_backend_connection(&mut self, metrics: &SessionMetrics) {
        if let Some(backend) = &self.backend {
            let mut backend = backend.borrow_mut();
            backend.failures += 1;

            let already_unavailable = backend.retry_policy.is_down();
            backend.retry_policy.fail();
            incr!(
                "backend.connections.error",
                self.cluster_id.as_deref(),
                metrics.backend_id.as_deref()
            );
            if !already_unavailable && backend.retry_policy.is_down() {
                error!(
                    "backend server {} at {} is down",
                    backend.backend_id, backend.address
                );
                incr!(
                    "backend.down",
                    self.cluster_id.as_deref(),
                    metrics.backend_id.as_deref()
                );

                push_event(Event {
                    kind: EventKind::BackendDown as i32,
                    backend_id: Some(backend.backend_id.to_owned()),
                    address: Some(backend.address.to_string()),
                    cluster_id: None,
                });
            }
        }
    }

    pub fn backend_hup(&mut self) -> StateResult {
        // println!("=====backend_hup");
        if self.backend_readiness.event.is_readable()
            && self.backend_readiness.interest.is_readable()
        {
            // there might still data we can read on the socket
            // println!("=====backend_hup:continue");
            return StateResult::Continue;
        }
        if self.response_stream.is_terminated() {
            // the backend finished to answer we can close
            // println!("=====backend_hup:terminated");
            return StateResult::CloseBackend;
        }
        match (
            self.request_stream.is_initial(),
            self.response_stream.is_initial(),
        ) {
            // the backend started to answer so we close
            (_, false) => {
                // println!("=====backend_hup:close");
                StateResult::CloseSession
            }
            // probably backend hup between keep alive request, change backend
            (true, true) => {
                // println!("=====backend_hup:close_backend");
                StateResult::CloseBackend
            }
            // the frontend already transmitted data so we can't redirect
            (false, true) => {
                // println!("=====backend_hup:503");
                self.set_answer(DefaultAnswerStatus::Answer503, None);
                self.backend_readiness.interest = Ready::EMPTY;
                StateResult::Continue
            }
        }
    }

    /// The main session loop, processing all events triggered by mio since last time
    /// and proxying http traffic. The main flow can be summed up by:
    ///
    /// - if connecting an back has event:
    ///   - if backend hanged up, try again
    ///   - else, set as connected
    /// - while front or back has event:
    ///   - read request on front
    ///   - write request to back
    ///   - read response on back
    ///   - write response to front
    fn ready_inner(
        &mut self,
        session: Rc<RefCell<dyn crate::ProxySession>>,
        proxy: Rc<RefCell<dyn L7Proxy>>,
        metrics: &mut SessionMetrics,
    ) -> SessionResult {
        let mut counter = 0;
        let max_loop_iterations = 100000;

        if self.backend_connection_status.is_connecting()
            && !self.backend_readiness.event.is_empty()
        {
            if self.backend_readiness.event.is_hup() && !self.test_backend_socket() {
                //retry connecting the backend
                error!(
                    "{} error connecting to backend, trying again",
                    self.log_context()
                );

                self.connection_attempts += 1;
                self.fail_backend_connection(metrics);

                self.backend_connection_status =
                    BackendConnectionStatus::Connecting(Instant::now());

                // trigger a backend reconnection
                self.close_backend(proxy.clone(), metrics);
                match self.connect_to_backend(session.clone(), proxy.clone(), metrics) {
                    // reuse connection or send a default answer, we can continue
                    Ok(BackendConnectAction::Reuse) => {}
                    Ok(BackendConnectAction::New) | Ok(BackendConnectAction::Replace) => {
                        // we must wait for an event
                        return SessionResult::Continue;
                    }
                    Err(connection_error) => {
                        error!("Error connecting to backend: {}", connection_error)
                    }
                }
            } else {
                metrics.backend_connected();
                self.connection_attempts = 0;
                self.set_backend_connected(BackendConnectionStatus::Connected, metrics);
                // we might get an early response from the backend, so we want to look
                // at readable events
                self.backend_readiness.interest.insert(Ready::READABLE);
            }
        }

        if self.frontend_readiness.event.is_hup() {
            return SessionResult::Close;
        }

        while counter < max_loop_iterations {
            let frontend_interest = self.frontend_readiness.filter_interest();
            let backend_interest = self.backend_readiness.filter_interest();

            trace!(
                "PROXY\t{} {:?} {:?} -> {:?}",
                self.log_context(),
                self.frontend_token,
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

            if frontend_interest.is_readable() {
                let state_result = self.readable(metrics);
                trace!("frontend_readable: {:?}", state_result);

                match state_result {
                    StateResult::Continue => {}
                    StateResult::ConnectBackend => {
                        match self.connect_to_backend(session.clone(), proxy.clone(), metrics) {
                            // reuse connection or send a default answer, we can continue
                            Ok(BackendConnectAction::Reuse) => {}
                            Ok(BackendConnectAction::New) | Ok(BackendConnectAction::Replace) => {
                                // we must wait for an event
                                return SessionResult::Continue;
                            }
                            Err(connection_error) => {
                                error!("Error connecting to backend: {}", connection_error)
                            }
                        }
                    }
                    StateResult::CloseBackend => unreachable!(),
                    StateResult::CloseSession => return SessionResult::Close,
                    StateResult::Upgrade => return SessionResult::Upgrade,
                }
            }

            if backend_interest.is_writable() {
                let session_result = self.backend_writable(metrics);
                trace!("backend_writable: {:?}", session_result);
                if session_result != SessionResult::Continue {
                    return session_result;
                }
            }

            if backend_interest.is_readable() {
                let session_result = self.backend_readable(metrics);
                trace!("backend_readable: {:?}", session_result);
                if session_result != SessionResult::Continue {
                    return session_result;
                }
            }

            if frontend_interest.is_writable() {
                let state_result = self.writable(metrics);
                trace!("frontend_writable: {:?}", state_result);
                match state_result {
                    StateResult::CloseBackend => self.close_backend(proxy.clone(), metrics),
                    StateResult::CloseSession => return SessionResult::Close,
                    StateResult::Upgrade => return SessionResult::Upgrade,
                    StateResult::Continue => {}
                    StateResult::ConnectBackend => unreachable!(),
                }
            }

            if frontend_interest.is_error() {
                error!(
                    "PROXY session {:?} front error, disconnecting",
                    self.frontend_token
                );
                return SessionResult::Close;
            }

            if backend_interest.is_hup() || backend_interest.is_error() {
                let state_result = self.backend_hup();
                error!("PROXY session {:?} back error", self.frontend_token);
                trace!("backend_hup: {:?}", state_result);
                match state_result {
                    StateResult::Continue => {}
                    StateResult::CloseBackend => self.close_backend(proxy.clone(), metrics),
                    StateResult::CloseSession => return SessionResult::Close,
                    StateResult::ConnectBackend | StateResult::Upgrade => unreachable!(),
                }
            }

            counter += 1;
        }

        if counter == max_loop_iterations {
            error!(
                "PROXY\thandling session {:?} went through {} iterations, there's a probable infinite loop bug, closing the connection",
                self.frontend_token, max_loop_iterations
            );
            incr!("http.infinite_loop.error");

            self.print_state(&format!("{:?}", self.context.protocol));

            return SessionResult::Close;
        }

        SessionResult::Continue
    }

    pub fn timeout_status(&self) -> TimeoutStatus {
        if self.request_stream.is_main_phase() {
            if self.response_stream.is_initial() {
                TimeoutStatus::WaitingForResponse
            } else {
                TimeoutStatus::Response
            }
        } else if self.keepalive_count > 0 {
            TimeoutStatus::WaitingForNewRequest
        } else {
            TimeoutStatus::Request
        }
    }
}

impl<Front: SocketHandler, L: ListenerHandler + L7ListenerHandler> SessionState for Http<Front, L> {
    fn ready(
        &mut self,
        session: Rc<RefCell<dyn crate::ProxySession>>,
        proxy: Rc<RefCell<dyn L7Proxy>>,
        metrics: &mut SessionMetrics,
    ) -> SessionResult {
        let session_result = self.ready_inner(session, proxy, metrics);
        if session_result == SessionResult::Upgrade {
            // sync the underlying Checkout buffers, if they contain remaining data
            // it will be processed once upgraded to websocket
            self.request_stream.storage.buffer.sync(
                self.request_stream.storage.end,
                self.request_stream.storage.head,
            );
            self.response_stream.storage.buffer.sync(
                self.response_stream.storage.end,
                self.response_stream.storage.head,
            );
        }
        session_result
    }

    fn update_readiness(&mut self, token: Token, events: Ready) {
        if self.frontend_token == token {
            self.frontend_readiness.event |= events;
        } else if self.backend_token == Some(token) {
            self.backend_readiness.event |= events;
        }
    }

    fn close(&mut self, proxy: Rc<RefCell<dyn L7Proxy>>, metrics: &mut SessionMetrics) {
        self.close_backend(proxy, metrics);

        //if the state was initial, the connection was already reset
        if !self.request_stream.is_initial() {
            gauge_add!("http.active_requests", -1);

            if let Some(b) = self.backend.as_mut() {
                let mut backend = b.borrow_mut();
                backend.active_requests = backend.active_requests.saturating_sub(1);
            }
        }
    }

    fn timeout(&mut self, token: Token, metrics: &mut SessionMetrics) -> StateResult {
        //info!("got timeout for token: {:?}", token);
        if self.frontend_token == token {
            self.container_frontend_timeout.triggered();
            return match self.timeout_status() {
                TimeoutStatus::Request => {
                    self.set_answer(DefaultAnswerStatus::Answer408, None);
                    self.writable(metrics)
                }
                TimeoutStatus::WaitingForResponse => {
                    self.set_answer(DefaultAnswerStatus::Answer504, None);
                    self.writable(metrics)
                }
                TimeoutStatus::Response => StateResult::CloseSession,
                TimeoutStatus::WaitingForNewRequest => StateResult::CloseSession,
            };
        }

        if self.backend_token == Some(token) {
            //info!("backend timeout triggered for token {:?}", token);
            self.container_backend_timeout.triggered();
            return match self.timeout_status() {
                TimeoutStatus::Request => {
                    error!(
                        "got backend timeout while waiting for a request, this should not happen"
                    );
                    self.set_answer(DefaultAnswerStatus::Answer504, None);
                    self.writable(metrics)
                }
                TimeoutStatus::WaitingForResponse => {
                    self.set_answer(DefaultAnswerStatus::Answer504, None);
                    self.writable(metrics)
                }
                TimeoutStatus::Response => {
                    error!(
                        "backend {:?} timeout while receiving response (cluster {:?})",
                        self.backend_id, self.cluster_id
                    );
                    StateResult::CloseSession
                }
                TimeoutStatus::WaitingForNewRequest => StateResult::Continue,
            };
        }

        error!("got timeout for an invalid token");
        StateResult::CloseSession
    }

    fn cancel_timeouts(&mut self) {
        self.container_backend_timeout.cancel();
        self.container_frontend_timeout.cancel();
    }

    fn print_state(&self, context: &str) {
        error!(
            "\
{} Session(Pipe)
\tFrontend:
\t\ttoken: {:?}\treadiness: {:?}\tstate: {:?}
\tBackend:
\t\ttoken: {:?}\treadiness: {:?}\tstate: {:?}",
            context,
            self.frontend_token,
            self.frontend_readiness,
            self.request_stream.parsing_phase,
            self.backend_token,
            self.backend_readiness,
            self.response_stream.parsing_phase
        );
    }

    fn shutting_down(&mut self) -> SessionIsToBeClosed {
        if self.request_stream.is_initial()
            && self.request_stream.storage.is_empty()
            && self.response_stream.storage.is_empty()
        {
            true
        } else {
            self.context.closing = true;
            false
        }
    }
}

/// Save the HTTP status code of the backend response
fn save_http_status_metric(status: Option<u16>, context: LogContext) {
    if let Some(status) = status {
        match status {
            100..=199 => {
                incr!("http.status.1xx", context.cluster_id, context.backend_id);
            }
            200..=299 => {
                incr!("http.status.2xx", context.cluster_id, context.backend_id);
            }
            300..=399 => {
                incr!("http.status.3xx", context.cluster_id, context.backend_id);
            }
            400..=499 => {
                incr!("http.status.4xx", context.cluster_id, context.backend_id);
            }
            500..=599 => {
                incr!("http.status.5xx", context.cluster_id, context.backend_id);
            }
            _ => {
                // http responses with other codes (protocol error)
                incr!("http.status.other");
            }
        }
    }
}
