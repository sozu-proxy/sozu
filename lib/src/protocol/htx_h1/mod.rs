pub mod answers;
pub mod editor;
pub mod parser;

use std::{
    cell::RefCell,
    io::ErrorKind,
    net::{Shutdown, SocketAddr},
    rc::{Rc, Weak},
    str::from_utf8_unchecked,
};

use anyhow::{bail, Context};
use htx::{self, Htx};
use mio::{net::TcpStream, *};
use rusty_ulid::Ulid;
use sozu_command::proto::command::{Event, EventKind};
use time::{Duration, Instant};

use crate::{
    pool::{Checkout, Pool},
    protocol::{
        http::{editor::HttpContext, parser::Method},
        SessionState,
    },
    retry::RetryPolicy,
    router::Route,
    server::{push_event, CONN_RETRIES},
    socket::{SocketHandler, SocketResult, TransportProtocol},
    sozu_command::ready::Ready,
    timer::TimeoutContainer,
    Backend, BackendConnectAction, BackendConnectionStatus, L7ListenerHandler, L7Proxy,
    ListenerHandler, LogDuration, Protocol, ProxySession, Readiness, SessionIsToBeClosed,
    SessionMetrics, SessionResult, StateResult,
};

type SozuHtx = Htx<Checkout>;

impl htx::AsBuffer for Checkout {
    fn as_buffer(&self) -> &[u8] {
        self.inner.extra()
    }
    fn as_mut_buffer(&mut self) -> &mut [u8] {
        self.inner.extra_mut()
    }
}

#[derive(Clone)]
pub struct StickySession {
    pub sticky_id: String,
}

impl StickySession {
    pub fn new(backend_id: String) -> StickySession {
        StickySession {
            sticky_id: backend_id,
        }
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
    connection_attempts: u8,
    pub frontend_readiness: Readiness,
    pub frontend_socket: Front,
    frontend_token: Token,
    keepalive_count: usize,
    listener: Rc<RefCell<L>>,
    pub htx_request: SozuHtx,
    pub htx_response: SozuHtx,
    status: SessionStatus,
    pub context: HttpContext,
}

impl<Front: SocketHandler, L: ListenerHandler + L7ListenerHandler> Http<Front, L> {
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
    ) -> Http<Front, L> {
        let (front_buffer, back_buffer) = match pool.upgrade() {
            Some(pool) => {
                let a = pool.borrow_mut().checkout();
                let b = pool.borrow_mut().checkout();
                match (a, b) {
                    (Some(front_buffer), Some(back_buffer)) => (front_buffer, back_buffer),
                    _ => panic!(),
                }
            }
            None => panic!(),
        };
        Http {
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
            frontend_readiness: Readiness::new(),
            frontend_socket,
            frontend_token,
            keepalive_count: 0,
            listener,
            htx_request: SozuHtx::new(htx::Kind::Request, htx::HtxBuffer::new(front_buffer)),
            htx_response: SozuHtx::new(htx::Kind::Response, htx::HtxBuffer::new(back_buffer)),
            status: SessionStatus::Normal,
            context: HttpContext {
                closing: false,
                id: request_id,
                keep_alive: false,
                protocol,
                public_address,
                session_address,
                sticky_name,
                sticky_session: None,
            },
        }
    }

    pub fn reset(&mut self) {
        println!("==============reset");
        self.context.id = Ulid::generate();
        gauge_add!("http.active_requests", -1);

        self.htx_request.clear();
        self.htx_response.clear();
        // self.added_request_header = Some(self.added_request_header(self.session_address));
        // self.added_response_header = self.added_response_header();

        self.keepalive_count += 1;

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
        println!("==============readable");
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

        if self.htx_request.storage.is_full() {
            self.frontend_readiness.interest.remove(Ready::READABLE);
            if self.htx_request.is_main_phase() {
                self.backend_readiness.interest.insert(Ready::WRITABLE);
            } else {
                // client has filled its HtxBuffer and we can't empty it
                self.set_answer(DefaultAnswerStatus::Answer413, None);
            }
            return StateResult::Continue;
        }

        let (size, socket_state) = self
            .frontend_socket
            .socket_read(self.htx_request.storage.space());
        debug!(
            "{}\tFRONT [{}->{:?}]: read {} bytes",
            self.log_context(),
            self.frontend_token.0,
            self.backend_token,
            size
        );

        if size > 0 {
            self.htx_request.storage.fill(size);
            count!("bytes_in", size as i64);
            metrics.bin += size;
            // if self.htx_request.storage.is_full() {
            //     self.frontend_readiness.interest.remove(Ready::READABLE);
            // }
        } else {
            self.frontend_readiness.event.remove(Ready::READABLE);
        }

        match socket_state {
            SocketResult::Error | SocketResult::Closed => {
                if self.htx_request.is_initial() {
                    // count an error if we were waiting for the first request
                    // otherwise, if we already had one completed request and response,
                    // and are waiting for the next one, we do not count a socket
                    // closing abruptly as an error
                    if self.keepalive_count == 0 {
                        self.frontend_socket.read_error();
                    }
                    metrics.service_stop();
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
        println!("==============readable_parse");
        let was_initial = self.htx_request.is_initial();

        // let mut context = self.request_context();
        htx::h1::parse(&mut self.htx_request, &mut self.context);
        htx::debug_htx(&self.htx_request);

        if was_initial && !self.htx_request.is_initial() {
            gauge_add!("http.active_requests", 1);
            incr!("http.requests");
        }

        if self.htx_request.is_error() {
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

        if self.htx_request.is_main_phase() {
            self.backend_readiness.interest.insert(Ready::WRITABLE);
            // if it was the first request, the front timeout duration
            // was set to request_timeout, which is much lower. For future
            // requests on this connection, we can wait a bit more
            self.container_frontend_timeout
                .set_duration(self.configured_frontend_timeout);
        }
        if self.htx_request.is_terminated() {
            self.frontend_readiness.interest.remove(Ready::READABLE);
        }

        if was_initial {
            if let htx::StatusLine::Request { .. } = self.htx_request.detached.status_line {
                println!("============== HANDLE CONNECTION!");
                return StateResult::ConnectBackend;
            }
        }
        StateResult::Continue
    }

    pub fn writable(&mut self, metrics: &mut SessionMetrics) -> StateResult {
        println!("==============writable");
        //handle default answers
        if let SessionStatus::DefaultAnswer(_, _, _) = self.status {
            return self.writable_default_answer(metrics);
        }

        self.htx_response.prepare(&mut htx::h1::BlockConverter);

        let bufs = self.htx_response.as_io_slice();
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
            self.htx_response.consume(size);
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

        if self.htx_response.is_terminated() && self.htx_response.is_completed() {
            save_http_status_metric(self.get_response_line().0);

            self.log_request_success(metrics);
            metrics.reset();

            if self.context.closing {
                debug!("{} closing proxy, no keep alive", self.log_context());
                return StateResult::CloseSession;
            }

            match self.htx_response.detached.status_line {
                htx::StatusLine::Response { code: 101, .. } => {
                    println!("============== HANDLE UPGRADE!");
                    return StateResult::Upgrade;
                }
                htx::StatusLine::Response { code: 100, .. } => {
                    println!("============== HANDLE CONTINUE!");
                    self.htx_response.clear();
                    return StateResult::Continue;
                }
                _ => (),
            }

            let frontend_keep_alive = true;
            let backend_keep_alive = true;

            // FIXME: we could get smarter about this
            // with no keepalive on backend, we could open a new backend ConnectionError
            // with no keepalive on front but keepalive on backend, we could have
            // a pool of connections
            return match (frontend_keep_alive, backend_keep_alive) {
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
        println!("==============backend_writable");
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

        self.htx_request.prepare(&mut htx::h1::BlockConverter);

        let bufs = self.htx_request.as_io_slice();
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
            self.htx_request.consume(size);
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

        if self.htx_request.is_terminated() && self.htx_request.is_completed() {
            self.backend_readiness.interest.remove(Ready::WRITABLE);

            // cancel the front timeout while we are waiting for the server to answer
            self.container_frontend_timeout.cancel();
            if let Some(token) = self.backend_token {
                self.container_backend_timeout.set(token);
            }
        }
        SessionResult::Continue
    }

    // Read content from cluster
    pub fn backend_readable(&mut self, metrics: &mut SessionMetrics) -> StateResult {
        println!("==============backend_readable");
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
            return StateResult::Continue;
        }

        let backend_socket = if let Some(backend_socket) = &mut self.backend_socket {
            backend_socket
        } else {
            self.log_request_error(metrics, "back socket not found, closing session");
            return StateResult::CloseSession;
        };

        if self.htx_response.storage.is_full() {
            self.backend_readiness.interest.remove(Ready::READABLE);
            if self.htx_response.is_main_phase() {
                self.frontend_readiness.interest.insert(Ready::WRITABLE);
            } else {
                // server has filled its HtxBuffer and we can't empty it
                // FIXME: what error code should we use?
                self.set_answer(DefaultAnswerStatus::Answer502, None);
            }
            return StateResult::Continue;
        }

        let (size, socket_state) = backend_socket.socket_read(self.htx_response.storage.space());
        debug!(
            "{}\tBACK  [{}<-{:?}]: read {} bytes",
            "ctx", // self.log_context(),
            self.frontend_token.0,
            self.backend_token,
            size
        );

        if size > 0 {
            self.htx_response.storage.fill(size);
            count!("back_bytes_in", size as i64);
            metrics.backend_bin += size;
            // if self.htx_response.storage.is_full() {
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
                return StateResult::CloseSession;
            }
            SocketResult::WouldBlock | SocketResult::Closed => {
                self.backend_readiness.event.remove(Ready::READABLE);
            }
            SocketResult::Continue => {}
        }

        self.backend_readable_parse(metrics)
    }

    pub fn backend_readable_parse(&mut self, metrics: &mut SessionMetrics) -> StateResult {
        println!("==============backend_readable_parse");
        let was_initial = self.htx_response.is_initial();

        // let mut context = self.response_context();
        htx::h1::parse(&mut self.htx_response, &mut self.context);
        htx::debug_htx(&self.htx_response);

        if self.htx_response.is_error() {
            incr!("http.backend_parse_errors");
            // was_initial is maybe too restrictive
            // from what I understand the only condition should be:
            // "did we already sent byte?"
            if was_initial {
                self.set_answer(DefaultAnswerStatus::Answer502, None);
                return StateResult::Continue;
            } else {
                return StateResult::CloseSession;
            }
        }

        if self.htx_response.is_main_phase() {
            self.frontend_readiness.interest.insert(Ready::WRITABLE);
        }
        if self.htx_response.is_terminated() {
            metrics.backend_stop();
            self.backend_stop = Some(Instant::now());
            self.backend_readiness.interest.remove(Ready::READABLE);
        }
        StateResult::Continue
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

    /// (code, reason)
    pub fn get_response_line(&self) -> (Option<u16>, Option<&str>) {
        if let htx::StatusLine::Response { code, reason, .. } =
            &self.htx_request.detached.status_line
        {
            let buffer = self.htx_response.storage.buffer();
            let reason = reason
                .data_opt(buffer)
                .map(|data| unsafe { from_utf8_unchecked(data) });
            (Some(*code), reason)
        } else {
            (None, None)
        }
    }

    /// (method, authority, path, uri)
    pub fn get_request_line(&self) -> (Option<Method>, Option<&str>, Option<&str>, Option<&str>) {
        if let htx::StatusLine::Request {
            method,
            authority,
            path,
            uri,
            ..
        } = &self.htx_request.detached.status_line
        {
            let buffer = self.htx_request.storage.buffer();
            let method = method.data_opt(buffer).map(Method::new);
            let authority = authority
                .data_opt(buffer)
                .map(|data| unsafe { from_utf8_unchecked(data) });
            let path = path
                .data_opt(buffer)
                .map(|data| unsafe { from_utf8_unchecked(data) });
            let uri = uri
                .data_opt(buffer)
                .map(|data| unsafe { from_utf8_unchecked(data) });
            (method, authority, path, uri)
        } else {
            (None, None, None, None)
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

    pub fn websocket_context(&self) -> String {
        let (method, authority, path, _) = self.get_request_line();
        let (status, reason) = self.get_response_line();
        format!(
            "{} {} {}\t{} {}",
            authority.as_str(),
            method.unwrap_or(Method::Get),
            path.as_str(),
            status.as_string(),
            reason.as_str()
        )
    }

    pub fn log_request_success(&self, metrics: &SessionMetrics) {
        let session = self.get_session_address().as_string();
        let backend = self.get_backend_address().as_string();

        let (method, authority, path, _) = self.get_request_line();
        let (status, _) = self.get_response_line();

        let response_time = metrics.response_time();
        let service_time = metrics.service_time();
        let _wait_time = metrics.wait_time;

        if let Some(cluster_id) = &self.cluster_id {
            time!(
                "response_time",
                cluster_id,
                response_time.whole_milliseconds()
            );
            time!(
                "service_time",
                cluster_id,
                service_time.whole_milliseconds()
            );
        }
        time!("response_time", response_time.whole_milliseconds());
        time!("service_time", service_time.whole_milliseconds());

        if let Some(backend_id) = metrics.backend_id.as_ref() {
            if let Some(backend_response_time) = metrics.backend_response_time() {
                record_backend_metrics!(
                    self.cluster_id.as_str(),
                    backend_id,
                    backend_response_time.whole_milliseconds(),
                    metrics.backend_connection_time(),
                    metrics.backend_bin,
                    metrics.backend_bout
                );
            }
        }

        let tags = authority.and_then(|host| {
            let hostname = match host.split_once(':') {
                None => host,
                Some((hn, _)) => hn,
            };

            self.listener.borrow().get_tags(hostname).map(|tags| {
                tags.iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
        });

        info_access!(
            "{}{} -> {}\t{} {} {} {}\t{}\t{} {} {} {}\t{}",
            self.log_context(),
            session,
            backend,
            LogDuration(response_time),
            LogDuration(service_time),
            metrics.bin,
            metrics.bout,
            tags.as_str(),
            self.protocol_string(),
            authority.as_str(),
            method.unwrap_or(Method::Get),
            path.as_str(),
            status.as_string()
        );
    }

    pub fn log_default_answer_success(&self, metrics: &SessionMetrics) {
        let session = self.get_session_address().as_string();
        let backend = self.get_backend_address().as_string();

        let (method, authority, path, _) = self.get_request_line();
        let status = match self.status {
            SessionStatus::Normal => "-".to_string(),
            SessionStatus::DefaultAnswer(answers, ..) => {
                let status: u16 = answers.into();
                status.to_string()
            }
        };

        let response_time = metrics.response_time();
        let service_time = metrics.service_time();

        if let Some(cluster_id) = &self.cluster_id {
            time!(
                "response_time",
                cluster_id,
                response_time.whole_milliseconds()
            );
            time!(
                "service_time",
                cluster_id,
                service_time.whole_milliseconds()
            );
        }
        time!("response_time", response_time.whole_milliseconds());
        time!("service_time", service_time.whole_milliseconds());
        incr!("http.errors");

        let tags = authority.and_then(|host| {
            let hostname = match host.split_once(':') {
                None => host,
                Some((hn, _)) => hn,
            };

            self.listener.borrow().get_tags(hostname).map(|tags| {
                tags.iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
        });

        info_access!(
            "{}{} -> {}\t{} {} {} {}\t{}\t{} {} {} {}\t{}",
            self.log_context(),
            session,
            backend,
            LogDuration(response_time),
            LogDuration(service_time),
            metrics.bin,
            metrics.bout,
            tags.as_str(),
            self.protocol_string(),
            authority.as_str(),
            method.unwrap_or(Method::Get),
            path.as_str(),
            status
        );
    }

    pub fn log_request_error(&mut self, metrics: &mut SessionMetrics, message: &str) {
        metrics.service_stop();
        self.frontend_readiness.reset();
        self.backend_readiness.reset();

        let session = self.get_session_address().as_string();
        let backend = self.get_backend_address().as_string();

        let (method, authority, path, _) = self.get_request_line();
        let (status, _) = self.get_response_line();

        let response_time = metrics.response_time();
        let service_time = metrics.service_time();

        if let Some(cluster_id) = &self.cluster_id {
            time!(
                "response_time",
                cluster_id,
                response_time.whole_milliseconds()
            );
            time!(
                "service_time",
                cluster_id,
                service_time.whole_milliseconds()
            );
        }
        time!("response_time", response_time.whole_milliseconds());
        time!("service_time", service_time.whole_milliseconds());
        incr!("http.errors");

        let tags = authority.and_then(|host| {
            let hostname = match host.split_once(':') {
                None => host,
                Some((hn, _)) => hn,
            };

            self.listener.borrow().get_tags(hostname).map(|tags| {
                tags.iter()
                    .map(|(k, v)| format!("\"{k}={v}\""))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
        });

        error_access!(
            "{}{} -> {}\t{} {} {} {}\t{}\t{} {} {} {}\t{} | {}",
            self.log_context(),
            session,
            backend,
            LogDuration(response_time),
            LogDuration(service_time),
            metrics.bin,
            metrics.bout,
            tags.as_str(),
            self.protocol_string(),
            authority.as_str(),
            method.unwrap_or(Method::Get),
            path.as_str(),
            status.as_string(),
            message
        );
    }

    pub fn set_answer(&mut self, answer: DefaultAnswerStatus, buf: Option<Rc<Vec<u8>>>) {
        if let SessionStatus::DefaultAnswer(status, _, _) = self.status {
            error!(
                "already set the default answer to {:?}, trying to set to {:?}",
                status, answer
            );
        } else {
            match answer {
                DefaultAnswerStatus::Answer301 => incr!("http.301.redirection"),
                DefaultAnswerStatus::Answer400 => incr!("http.400.errors"),
                DefaultAnswerStatus::Answer401 => incr!("http.401.errors"),
                DefaultAnswerStatus::Answer404 => incr!("http.404.errors"),
                DefaultAnswerStatus::Answer408 => incr!("http.408.errors"),
                DefaultAnswerStatus::Answer413 => incr!("http.413.errors"),
                DefaultAnswerStatus::Answer502 => incr!("http.502.errors"),
                DefaultAnswerStatus::Answer503 => incr!("http.503.errors"),
                DefaultAnswerStatus::Answer504 => incr!("http.504.errors"),
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
                    metrics.service_stop();
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
        // if socket was not used in the last second, test it
        if self
            .backend_stop
            .as_ref()
            .map(|t| {
                let now = Instant::now();
                let dur = now - *t;

                dur > Duration::seconds(1)
            })
            .unwrap_or(true)
        {
            return self.test_backend_socket();
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

    pub fn cancel_backend_timeout(&mut self) {
        self.container_backend_timeout.cancel();
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
    /// IF the backend_token is set. I don't think this is a good idea, but it is a quick fix
    fn close_backend(&mut self, proxy: Rc<RefCell<dyn L7Proxy>>, metrics: &mut SessionMetrics) {
        debug!(
            "{}\tPROXY [{}->{}] CLOSED BACKEND",
            self.log_context(),
            self.frontend_token.0,
            self.backend_token
                .map(|t| format!("{}", t.0))
                .unwrap_or_else(|| "-".to_string())
        );

        let proxy = proxy.borrow();
        if let Some(socket) = &mut self.backend_socket {
            if let Err(e) = proxy.deregister_socket(socket) {
                error!("error deregistering back socket({:?}): {:?}", socket, e);
            }
            if let Err(e) = socket.shutdown(Shutdown::Both) {
                if e.kind() != ErrorKind::NotConnected {
                    error!("error shutting down back socket({:?}): {:?}", socket, e);
                }
            }
        }

        if let Some(token) = self.backend_token {
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
                self.clear_backend_token();
                backend.borrow_mut().dec_connections();
                self.cancel_backend_timeout();
            }
        }
    }

    fn check_circuit_breaker(&mut self) -> anyhow::Result<()> {
        if self.connection_attempts >= CONN_RETRIES {
            error!("{} max connection attempt reached", self.log_context());
            self.set_answer(DefaultAnswerStatus::Answer503, None);
            bail!("Maximum connection attempt reached");
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
    pub fn extract_route(&self) -> anyhow::Result<(&str, &str, Method)> {
        let request_line = self.get_request_line();
        let given_method = request_line.0.with_context(|| "No method given")?;
        let given_host = request_line.1.with_context(|| "No host given")?;
        let given_path = request_line.2.with_context(|| "No path given")?;

        Ok((given_host, given_path, given_method))
    }

    fn cluster_id_from_request(
        &mut self,
        proxy: Rc<RefCell<dyn L7Proxy>>,
    ) -> anyhow::Result<String> {
        let (host, uri, method) = match self.extract_route() {
            Ok(tuple) => tuple,
            Err(e) => {
                self.set_answer(DefaultAnswerStatus::Answer400, None);
                return Err(e).with_context(|| "Could not extract route from request");
            }
        };

        let cluster_id_res = self
            .listener
            .borrow()
            .frontend_from_request(host, uri, &method);

        let cluster_id = match cluster_id_res {
            Ok(route) => match route {
                Route::ClusterId(cluster_id) => cluster_id,
                Route::Deny => {
                    self.set_answer(DefaultAnswerStatus::Answer401, None);
                    bail!("Unauthorized route");
                }
            },
            Err(e) => {
                let no_host_error = format!("Host not found: {host}: {e:#}");
                self.set_answer(DefaultAnswerStatus::Answer404, None);
                bail!(no_host_error);
            }
        };

        let frontend_should_redirect_https = proxy
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
            bail!("Route is unauthorized");
        }

        Ok(cluster_id)
    }

    pub fn backend_from_request(
        &mut self,
        cluster_id: &str,
        frontend_should_stick: bool,
        proxy: Rc<RefCell<dyn L7Proxy>>,
        metrics: &mut SessionMetrics,
    ) -> anyhow::Result<TcpStream> {
        // TODO: sticky session...
        let sticky_session = None;

        let (backend, conn) = match self.get_backend_for_sticky_session(
            frontend_should_stick,
            sticky_session,
            cluster_id,
            proxy,
        ) {
            Ok(tuple) => tuple,
            Err(e) => {
                self.set_answer(DefaultAnswerStatus::Answer503, None);
                return Err(e)
                    .with_context(|| format!("Could not find a backend for cluster {cluster_id}"));
            }
        };

        if frontend_should_stick {
            let sticky_name = self.listener.borrow().get_sticky_name().to_string();

            self.context.sticky_session = Some(StickySession::new(
                backend
                    .borrow()
                    .sticky_id
                    .clone()
                    .unwrap_or_else(|| backend.borrow().backend_id.clone()),
            ));

            // stick session to listener (how? why?)
            self.context.sticky_name = sticky_name;
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
    ) -> anyhow::Result<(Rc<RefCell<Backend>>, TcpStream)> {
        match (frontend_should_stick, sticky_session) {
            (true, Some(sticky_session)) => proxy
                .borrow()
                .backends()
                .borrow_mut()
                .backend_from_sticky_session(cluster_id, sticky_session)
                .with_context(|| {
                    format!(
                        "Couldn't find a backend corresponding to sticky_session {sticky_session} for cluster {cluster_id}"
                    )
                }),
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
    ) -> anyhow::Result<BackendConnectAction> {
        let old_cluster_id = self.cluster_id.clone();
        let old_backend_token = self.backend_token;

        self.check_circuit_breaker()
            .with_context(|| "Circuit broke")?;

        let cluster_id = self
            .cluster_id_from_request(proxy.clone())
            .with_context(|| "Could not get cluster id from request")?;

        println!(
            "connect_to_backend: {:?} {:?} {:?}",
            self.cluster_id, cluster_id, self.backend_connection_status
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
            self.cancel_backend_timeout();

            if let Some(backend) = &self.backend {
                let mut backend = backend.borrow_mut();

                if backend.retry_policy.is_down() {
                    incr!(
                        "up",
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
                "connections.error",
                self.cluster_id.as_deref(),
                metrics.backend_id.as_deref()
            );
            if !already_unavailable && backend.retry_policy.is_down() {
                error!(
                    "backend server {} at {} is down",
                    backend.backend_id, backend.address
                );
                incr!(
                    "down",
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
        if self.backend_readiness.event.is_readable()
            && self.backend_readiness.interest.is_readable()
        {
            return StateResult::Continue;
        }
        match (
            self.htx_request.is_initial(),
            self.htx_response.is_initial(),
        ) {
            // the backend started to answer so we close
            (_, false) => StateResult::CloseSession,
            // probably backend hup between keep alive request, change change backend
            (true, true) => StateResult::CloseBackend,
            // the frontend already transmitted data so we can't redirect
            (false, true) => {
                self.set_answer(DefaultAnswerStatus::Answer503, None);
                self.backend_readiness.interest = Ready::EMPTY;
                StateResult::Continue
            }
        }
    }

    fn ready_inner(
        &mut self,
        session: Rc<RefCell<dyn crate::ProxySession>>,
        proxy: Rc<RefCell<dyn L7Proxy>>,
        metrics: &mut SessionMetrics,
    ) -> StateResult {
        let mut counter = 0;
        let max_loop_iterations = 100000;

        if self.backend_connection_status.is_connecting()
            && !self.backend_readiness.event.is_empty()
        {
            self.cancel_backend_timeout();

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
                        return StateResult::Continue;
                    }
                    Err(connection_error) => {
                        error!("Error connecting to backend: {:#}", connection_error)
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
            return StateResult::CloseSession;
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
                                return StateResult::Continue;
                            }
                            Err(connection_error) => {
                                error!("Error connecting to backend: {:#}", connection_error)
                            }
                        }
                    }
                    other => return other,
                }
            }

            if backend_interest.is_writable() {
                let session_result = self.backend_writable(metrics);
                trace!("backend_writable: {:?}", session_result);
                match session_result {
                    SessionResult::Continue => {}
                    SessionResult::Upgrade => return StateResult::Upgrade,
                    SessionResult::Close => return StateResult::CloseSession,
                }
            }

            if backend_interest.is_readable() {
                let state_result = self.backend_readable(metrics);
                trace!("backend_readable: {:?}", state_result);
                if state_result != StateResult::Continue {
                    return state_result;
                }
            }

            if frontend_interest.is_writable() {
                let state_result = self.writable(metrics);
                trace!("frontend_writable: {:?}", state_result);
                if state_result != StateResult::Continue {
                    return state_result;
                }
            }

            if backend_interest.is_hup() {
                let state_result = self.backend_hup();
                trace!("backend_hup: {:?}", state_result);
                match state_result {
                    StateResult::Continue => {}
                    other => return other,
                };
            }

            if frontend_interest.is_error() {
                error!(
                    "PROXY session {:?} front error, disconnecting",
                    self.frontend_token
                );

                self.frontend_readiness.interest = Ready::EMPTY;
                self.backend_readiness.interest = Ready::EMPTY;

                return StateResult::CloseSession;
            }

            if backend_interest.is_error() && self.backend_hup() == StateResult::CloseSession {
                self.frontend_readiness.interest = Ready::EMPTY;
                self.backend_readiness.interest = Ready::EMPTY;

                error!(
                    "PROXY session {:?} back error, disconnecting",
                    self.frontend_token
                );
                return StateResult::CloseSession;
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

            return StateResult::CloseSession;
        }

        StateResult::Continue
    }

    pub fn timeout_status(&self) -> TimeoutStatus {
        if self.htx_request.is_main_phase() {
            if self.htx_response.is_initial() {
                TimeoutStatus::WaitingForResponse
            } else {
                TimeoutStatus::Response
            }
        } else {
            if self.keepalive_count > 0 {
                TimeoutStatus::WaitingForNewRequest
            } else {
                TimeoutStatus::Request
            }
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
        match self.ready_inner(session, proxy.clone(), metrics) {
            StateResult::CloseSession => SessionResult::Close,
            StateResult::Upgrade => SessionResult::Upgrade,
            StateResult::CloseBackend => {
                // this is redundant
                self.close_backend(proxy, metrics);
                SessionResult::Continue
            }
            StateResult::Continue => SessionResult::Continue,
            StateResult::ConnectBackend => unreachable!(),
        }
    }

    fn update_readiness(&mut self, token: Token, events: Ready) {
        if self.frontend_token == token {
            self.frontend_readiness.event |= events;
        } else if self.backend_token == Some(token) {
            self.backend_readiness.event |= events;
        }
    }

    // TODO: State should take responsibility for closing sockets and token entries?
    // how about no
    fn close(&mut self, proxy: Rc<RefCell<dyn L7Proxy>>, metrics: &mut SessionMetrics) {
        self.close_backend(proxy, metrics);

        //if the state was initial, the connection was already reset
        if !self.htx_request.is_initial() {
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
            self.htx_request.parsing_phase,
            self.backend_token,
            self.backend_readiness,
            self.htx_response.parsing_phase
        );
    }

    fn shutting_down(&mut self) -> SessionIsToBeClosed {
        if self.htx_request.is_initial()
            && self.htx_request.storage.is_empty()
            && self.htx_response.storage.is_empty()
        {
            true
        } else {
            self.context.closing = true;
            false
        }
    }
}

/// Save the backend http response status code metric
fn save_http_status_metric(status: Option<u16>) {
    if let Some(status) = status {
        match status {
            100..=199 => {
                incr!("http.status.1xx");
            }
            200..=299 => {
                incr!("http.status.2xx");
            }
            300..=399 => {
                incr!("http.status.3xx");
            }
            400..=499 => {
                incr!("http.status.4xx");
            }
            500..=599 => {
                incr!("http.status.5xx");
            }
            _ => {
                incr!("http.status.other");
            } // http responses with other codes (protocol error)
        }
    }
}

pub struct LogContext<'a> {
    pub request_id: Ulid,
    pub cluster_id: Option<&'a str>,
    pub backend_id: Option<&'a str>,
}

impl<'a> std::fmt::Display for LogContext<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {} {}\t",
            self.request_id,
            self.cluster_id.unwrap_or("-"),
            self.backend_id.unwrap_or("-")
        )
    }
}

pub trait AsStr {
    fn as_str(&self) -> &str;
}
pub trait AsString {
    fn as_string(&self) -> String;
}

impl AsStr for Option<String> {
    fn as_str(&self) -> &str {
        match &self {
            None => "-",
            Some(s) => s,
        }
    }
}

impl AsStr for Option<&str> {
    fn as_str(&self) -> &str {
        match &self {
            None => "-",
            Some(s) => s,
        }
    }
}

impl AsString for Option<u16> {
    fn as_string(&self) -> String {
        match &self {
            None => "-".to_string(),
            Some(s) => s.to_string(),
        }
    }
}

impl AsString for Option<SocketAddr> {
    fn as_string(&self) -> String {
        match &self {
            None => "X".to_string(),
            Some(s) => s.to_string(),
        }
    }
}
