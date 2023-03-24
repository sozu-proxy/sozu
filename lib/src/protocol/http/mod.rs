pub mod answers;
pub mod cookies;
pub mod parser;

use std::{
    cell::RefCell,
    cmp::min,
    io::ErrorKind,
    net::{IpAddr, Shutdown, SocketAddr},
    rc::{Rc, Weak},
    str::from_utf8_unchecked,
};

use anyhow::{bail, Context};
use mio::{net::TcpStream, *};
use rusty_ulid::Ulid;
use sozu_command::response::Event;
use time::{Duration, Instant};

use crate::{
    buffer_queue::BufferQueue,
    pool::Pool,
    protocol::SessionResult,
    retry::RetryPolicy,
    server::{push_event, CONN_RETRIES},
    socket::{SocketHandler, SocketResult, TransportProtocol},
    sozu_command::ready::Ready,
    timer::TimeoutContainer,
    util::UnwrapLog,
    Backend, BackendConnectAction, BackendConnectionStatus, L7ListenerHandler, L7Proxy,
    ListenerHandler, LogDuration, ProxySession, SessionIsToBeClosed,
    {Protocol, Readiness, SessionMetrics, StateResult}, router::Route,
};

use self::parser::{
    compare_no_case, hostname_and_port, parse_request_until_stop, parse_response_until_stop, Chunk,
    Continue, Method, RequestLine, RequestState, ResponseState, StatusLine,
};

use super::SessionState;

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
    added_request_header: Option<AddedRequestHeader>,
    added_response_header: String,
    pub backend: Option<Rc<RefCell<Backend>>>,
    pub backend_buffer: Option<BufferQueue>,
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
    closing: bool,
    pub cluster_id: Option<String>,
    connection_attempts: u8,
    pub frontend_buffer: Option<BufferQueue>,
    pub frontend_readiness: Readiness,
    pub frontend_socket: Front,
    frontend_token: Token,
    keepalive_count: usize,
    listener: Rc<RefCell<L>>,
    pool: Weak<RefCell<Pool>>,
    protocol: Protocol,
    public_address: SocketAddr,
    pub request_id: Ulid,
    pub request_state: Option<RequestState>,
    request_header_end: Option<usize>,
    response_state: Option<ResponseState>,
    response_header_end: Option<usize>,
    status: SessionStatus,
    pub session_address: Option<SocketAddr>,
    sticky_name: String,
    sticky_session: Option<StickySession>,
}

impl<Front: SocketHandler, L: ListenerHandler + L7ListenerHandler> Http<Front, L> {
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
        let mut session = Http {
            added_request_header: None,
            added_response_header: String::from(""),
            answers,
            backend_buffer: None,
            backend_connection_status: BackendConnectionStatus::NotConnected,
            backend_id: None,
            backend_readiness: Readiness::new(),
            backend_socket: None,
            backend_stop: None,
            backend_token: None,
            backend: None,
            closing: false,
            cluster_id: None,
            configured_backend_timeout,
            configured_connect_timeout,
            configured_frontend_timeout,
            connection_attempts: 0,
            container_backend_timeout: TimeoutContainer::new_empty(configured_connect_timeout),
            container_frontend_timeout,
            frontend_buffer: None,
            frontend_readiness: Readiness::new(),
            frontend_socket,
            frontend_token,
            keepalive_count: 0,
            listener,
            pool,
            protocol,
            public_address,
            request_header_end: None,
            request_id,
            request_state: Some(RequestState::Initial),
            response_header_end: None,
            response_state: Some(ResponseState::Initial),
            session_address,
            status: SessionStatus::Normal,
            sticky_name,
            sticky_session: None,
        };

        session.added_request_header = Some(session.added_request_header(session_address));
        session.added_response_header = session.added_response_header();
        session
    }

    pub fn reset(&mut self) {
        let request_id = Ulid::generate();
        //info!("{} RESET TO {}", self.log_ctx, request_id);
        gauge_add!("http.active_requests", -1);

        self.request_state = Some(RequestState::Initial);
        self.response_state = Some(ResponseState::Initial);
        self.request_header_end = None;
        self.response_header_end = None;
        self.added_request_header = Some(self.added_request_header(self.session_address));
        self.added_response_header = self.added_response_header();

        // if HTTP requests are pipelined, we might still have some data in the front buffer
        if self
            .frontend_buffer
            .as_ref()
            .map(|buf| !buf.empty())
            .unwrap_or(false)
        {
            self.frontend_readiness.event.insert(Ready::readable());
        } else {
            self.frontend_buffer = None;
        }

        self.backend_buffer = None;
        self.request_id = request_id;
        self.keepalive_count += 1;

        if let Some(ref mut b) = self.backend {
            let mut backend = b.borrow_mut();
            backend.active_requests = backend.active_requests.saturating_sub(1);
        }

        // reset the front timeout and cancel the back timeout while we are
        // waiting for a new request
        self.container_frontend_timeout.reset();
        self.container_backend_timeout.cancel();
    }

    fn log_context(&self) -> LogContext {
        LogContext {
            request_id: self.request_id,
            cluster_id: self.cluster_id.as_deref(),
            backend_id: self.backend_id.as_deref(),
        }
    }

    pub fn set_answer(&mut self, answer: DefaultAnswerStatus, buf: Option<Rc<Vec<u8>>>) {
        self.frontend_buffer = None;
        self.backend_buffer = None;

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
        self.frontend_readiness.interest = Ready::writable() | Ready::hup() | Ready::error();
        self.backend_readiness.interest = Ready::hup() | Ready::error();
    }

    fn added_request_header(&self, client_address: Option<SocketAddr>) -> AddedRequestHeader {
        AddedRequestHeader {
            request_id: self.request_id,
            closing: self.closing,
            public_address: self.public_address,
            peer_address: client_address.or_else(|| self.front_socket().peer_addr().ok()),
            protocol: self.protocol,
        }
    }

    pub fn added_response_header(&self) -> String {
        if self.closing {
            format!("Sozu-Id: {}\r\nConnection: close", self.request_id)
        } else {
            format!("Sozu-Id: {}\r\n", self.request_id)
        }
    }

    // TODO: should disappear
    pub fn front_socket(&self) -> &TcpStream {
        self.frontend_socket.socket_ref()
    }

    // TODO: should disappear
    pub fn back_token(&self) -> Vec<Token> {
        self.backend_token.iter().cloned().collect()
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

    pub fn frontend_readiness(&mut self) -> &mut Readiness {
        &mut self.frontend_readiness
    }

    pub fn backend_readiness(&mut self) -> &mut Readiness {
        &mut self.backend_readiness
    }

    pub fn set_backend_timeout(&mut self, dur: Duration) {
        if let Some(token) = self.backend_token.as_ref() {
            self.container_backend_timeout.set_duration(dur);
            self.container_backend_timeout.set(*token);
        }
    }

    fn protocol(&self) -> Protocol {
        self.protocol
    }

    fn must_continue_request(&self) -> bool {
        matches!(
            self.request_state
                .as_ref()
                .and_then(|r| r.get_keep_alive().as_ref().map(|conn| conn.continues)),
            Some(Continue::Expects(_))
        )
    }

    fn must_continue_response(&self) -> Option<usize> {
        if let Some(Continue::Expects(sz)) = self
            .request_state
            .as_ref()
            .and_then(|r| r.get_keep_alive().map(|conn| conn.continues))
        {
            if self
                .response_state
                .as_ref()
                .and_then(|r| r.get_status_line().map(|st| st.status == 100))
                .unwrap_or(false)
            {
                return Some(sz);
            }
        }
        None
    }

    pub fn timeout_status(&self) -> TimeoutStatus {
        match self.request_state.as_ref() {
            Some(RequestState::Request(_, _, _))
            | Some(RequestState::RequestWithBody(_, _, _, _))
            | Some(RequestState::RequestWithBodyChunks(_, _, _, _)) => {
                match self.response_state.as_ref() {
                    Some(ResponseState::Initial) => TimeoutStatus::WaitingForResponse,
                    _ => TimeoutStatus::Response,
                }
            }
            _ => {
                if self.keepalive_count > 0 {
                    TimeoutStatus::WaitingForNewRequest
                } else {
                    TimeoutStatus::Request
                }
            }
        }
    }

    pub fn backend_hup(&mut self) -> StateResult {
        if let Some(ref mut buf) = self.backend_buffer {
            //FIXME: closing the session might not be a good idea if we do keep alive on the front here?
            if buf.output_data_size() == 0 || buf.next_output_data().is_empty() {
                if self.backend_readiness.event.is_readable() {
                    self.backend_readiness.interest.insert(Ready::readable());
                    StateResult::Continue
                } else {
                    if self.response_state == Some(ResponseState::Initial) {
                        self.set_answer(DefaultAnswerStatus::Answer503, None);
                    } else if let Some(ResponseState::ResponseWithBodyCloseDelimited(
                        _,
                        _,
                        ref mut backend_closed,
                    )) = self.response_state.as_mut()
                    {
                        *backend_closed = true;
                        // trick the protocol into calling readable() again
                        self.backend_readiness.event.insert(Ready::readable());
                        return StateResult::Continue;
                    } else {
                        self.set_answer(DefaultAnswerStatus::Answer502, None);
                    }

                    // we're not expecting any more data from the backend
                    self.backend_readiness.interest = Ready::empty();
                    StateResult::Continue
                }
            } else {
                self.frontend_readiness.interest.insert(Ready::writable());
                if self.backend_readiness.event.is_readable() {
                    self.backend_readiness.interest.insert(Ready::readable());
                }
                StateResult::Continue
            }
        } else if self.backend_readiness.event.is_readable()
            && self.backend_readiness.interest.is_readable()
        {
            StateResult::Continue
        } else {
            if self
                .request_state
                .as_ref()
                .map(|r| r.is_proxying())
                .unwrap_or(false)
                && self.response_state == Some(ResponseState::Initial)
            {
                self.set_answer(DefaultAnswerStatus::Answer503, None);
                // we're not expecting any more data from the backend
                self.backend_readiness.interest = Ready::empty();
            }
            StateResult::CloseBackend
        }
    }

    /// Retrieve the response status from the http response state
    pub fn get_response_status(&self) -> Option<&StatusLine> {
        self.response_state
            .as_ref()
            .and_then(|r| r.get_status_line())
    }

    pub fn get_host(&self) -> Option<&str> {
        self.request_state.as_ref().and_then(|r| r.get_host())
    }

    pub fn get_request_line(&self) -> Option<&RequestLine> {
        self.request_state
            .as_ref()
            .and_then(|r| r.get_request_line())
    }

    pub fn get_session_address(&self) -> Option<SocketAddr> {
        self.session_address
            .or_else(|| self.frontend_socket.socket_ref().peer_addr().ok())
    }

    pub fn get_backend_address(&self) -> Option<SocketAddr> {
        self.backend
            .as_ref()
            .map(|b| b.borrow().address)
            .or_else(|| {
                self.backend_socket
                    .as_ref()
                    .and_then(|backend| backend.peer_addr().ok())
            })
    }

    pub fn websocket_context(&self) -> String {
        let host = self.get_host().unwrap_or("-");
        let request_line = self
            .get_request_line()
            .map(|line| (&line.method, line.uri.as_str()))
            .unwrap_or((&Method::Get, "-"));
        let status_line = self
            .get_response_status()
            .map(|line| (line.status, line.reason.as_str()))
            .unwrap_or((0, "-"));

        format!(
            "{} {} {}\t{} {}",
            host, request_line.0, request_line.1, status_line.0, status_line.1
        )
    }

    fn protocol_string(&self) -> &'static str {
        match self.protocol() {
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

    pub fn log_request_success(&self, metrics: &SessionMetrics) {
        let session = SessionAddress(self.get_session_address());
        let backend = SessionAddress(self.get_backend_address());

        let host = OptionalString::new(self.get_host());
        let request_line = OptionalRequest::new(
            self.get_request_line()
                .map(|line| (&line.method, line.uri.as_str())),
        );
        let status_line = OptionalStatus::new(self.get_response_status().map(|line| line.status));

        let response_time = metrics.response_time();
        let service_time = metrics.service_time();
        let _wait_time = metrics.wait_time;

        let cluster_id = OptionalString::new(self.cluster_id.as_deref());
        time!(
            "response_time",
            cluster_id.as_str(),
            response_time.whole_milliseconds()
        );
        time!(
            "service_time",
            cluster_id.as_str(),
            service_time.whole_milliseconds()
        );
        time!("response_time", response_time.whole_milliseconds());
        time!("service_time", service_time.whole_milliseconds());

        if let Some(backend_id) = metrics.backend_id.as_ref() {
            if let Some(backend_response_time) = metrics.backend_response_time() {
                record_backend_metrics!(
                    cluster_id,
                    backend_id,
                    backend_response_time.whole_milliseconds(),
                    metrics.backend_connection_time(),
                    metrics.backend_bin,
                    metrics.backend_bout
                );
            }
        }

        let proto = self.protocol_string();
        let tags = host.inner.and_then(|host| {
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
            "{}{} -> {}\t{} {} {} {}\t{}\t{} {} {}\t{}",
            self.log_context(),
            session,
            backend,
            LogDuration(response_time),
            LogDuration(service_time),
            metrics.bin,
            metrics.bout,
            OptionalString::from(tags.as_ref()),
            proto,
            host,
            status_line,
            request_line
        );
    }

    pub fn log_default_answer_success(&self, metrics: &SessionMetrics) {
        let session = SessionAddress(self.get_session_address());
        let backend = SessionAddress(self.get_backend_address());

        let status_line = match self.status {
            SessionStatus::Normal => OptionalStatus::new(None),
            SessionStatus::DefaultAnswer(answers, _, _) => {
                OptionalStatus::new(Some(answers.into()))
            }
        };

        let host = OptionalString::new(self.get_host());
        let request_line = OptionalRequest::new(
            self.get_request_line()
                .map(|line| (&line.method, line.uri.as_str())),
        );

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

        let proto = self.protocol_string();
        let tags = host.inner.and_then(|host| {
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
            "{}{} -> {}\t{} {} {} {}\t{}\t{} {} {}\t{}",
            self.log_context(),
            session,
            backend,
            LogDuration(response_time),
            LogDuration(service_time),
            metrics.bin,
            metrics.bout,
            OptionalString::from(tags.as_ref()),
            proto,
            host,
            status_line,
            request_line
        );
    }

    pub fn log_request_error(&mut self, metrics: &mut SessionMetrics, message: &str) {
        metrics.service_stop();
        self.frontend_readiness.reset();
        self.backend_readiness.reset();

        let session = SessionAddress(self.get_session_address());
        let backend = SessionAddress(self.get_backend_address());

        let host = OptionalString::new(self.get_host());
        let request_line = OptionalRequest::new(
            self.get_request_line()
                .map(|line| (&line.method, line.uri.as_str())),
        );
        let status_line =
            OptionalStatus::new(self.get_response_status().as_ref().map(|line| line.status));

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
        /*
        let cluster_id = self.cluster_id.clone().unwrap_or(String::from("-"));
        time!("request_time", &cluster_id, response_time);

        if let Some(backend_id) = metrics.backend_id.as_ref() {
          if let Some(backend_response_time) = metrics.backend_response_time() {
            record_backend_metrics!(cluster_id, backend_id, backend_response_time.whole_milliseconds(), metrics.backend_connection_time(), metrics.backend_bin, metrics.backend_bout);
          }
        }*/

        let proto = self.protocol_string();
        let tags = host.inner.and_then(|host| {
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
            "{}{} -> {}\t{} {} {} {}\t{} {} {}\t{} | {} {}",
            self.log_context(),
            session,
            backend,
            LogDuration(response_time),
            LogDuration(service_time),
            metrics.bin,
            metrics.bout,
            proto,
            host,
            request_line,
            status_line,
            message,
            OptionalString::from(tags.as_ref())
        );
    }

    /// Read content from the session
    pub fn readable(&mut self, metrics: &mut SessionMetrics) -> StateResult {
        if !self.container_frontend_timeout.reset() {
            //error!("could not reset front timeout");
        }

        if let SessionStatus::DefaultAnswer(_, _, _) = self.status {
            self.frontend_readiness.interest.insert(Ready::writable());
            self.backend_readiness.interest.remove(Ready::readable());
            self.backend_readiness.interest.remove(Ready::writable());
            return StateResult::Continue;
        }

        assert!(!unwrap_msg!(self.request_state.as_ref()).is_front_error());

        if self.frontend_buffer.is_none() {
            if let Some(pool) = self.pool.upgrade() {
                match pool.borrow_mut().checkout() {
                    Some(buf) => self.frontend_buffer = Some(BufferQueue::with_buffer(buf)),
                    None => {
                        error!("cannot get front buffer from pool, closing");
                        return StateResult::CloseSession;
                    }
                }
            }
        }

        if self
            .frontend_buffer
            .as_ref()
            .unwrap()
            .buffer
            .available_space()
            == 0
        {
            if self.backend_token.is_none() {
                self.set_answer(DefaultAnswerStatus::Answer413, None);
                self.frontend_readiness.interest.remove(Ready::readable());
                self.frontend_readiness.interest.insert(Ready::writable());
            } else {
                self.frontend_readiness.interest.remove(Ready::readable());
                self.backend_readiness.interest.insert(Ready::writable());
            }
            return StateResult::Continue;
        }

        let (size, socket_state) = self
            .frontend_socket
            .socket_read(self.frontend_buffer.as_mut().unwrap().buffer.space());
        debug!("{}\tFRONT: read {} bytes", self.log_context(), size);

        if size > 0 {
            count!("bytes_in", size as i64);
            metrics.bin += size;

            if let Some(frontend_buf) = self.frontend_buffer.as_mut() {
                // synchronize end cursor
                frontend_buf.buffer.fill(size);
                frontend_buf.sliced_input(size);
                if frontend_buf.start_parsing_position > frontend_buf.parsed_position {
                    let to_consume = min(
                        frontend_buf.input_data_size(),
                        frontend_buf.start_parsing_position - frontend_buf.parsed_position,
                    );
                    frontend_buf.consume_parsed_data(to_consume);
                }
            }

            if self
                .frontend_buffer
                .as_ref()
                .unwrap()
                .buffer
                .available_space()
                == 0
            {
                self.frontend_readiness.interest.remove(Ready::readable());
            }
        } else {
            self.frontend_readiness.event.remove(Ready::readable());
        }

        match socket_state {
            SocketResult::Error => {
                //we were in keep alive but the peer closed the connection
                if self.request_state == Some(RequestState::Initial) {
                    // count an error if we were waiting for the first request
                    // otherwise, if we already had one completed request and response,
                    // and are waiting for the next one, we do not count a socket
                    // closing abruptly as an error
                    if self.keepalive_count == 0 {
                        self.frontend_socket.read_error();
                    }

                    metrics.service_stop();
                    self.frontend_readiness.reset();
                    self.backend_readiness.reset();
                } else {
                    self.frontend_socket.read_error();
                    let frontend_readiness = self.frontend_readiness.clone();
                    let backend_readiness = self.backend_readiness.clone();
                    self.log_request_error(
                        metrics,
                        &format!(
                            "front socket error, closing the session. Readiness: {frontend_readiness:?} -> {backend_readiness:?}, read {size} bytes"
                        )
                    );
                }
                return StateResult::CloseSession;
            }
            SocketResult::Closed => {
                //we were in keep alive but the peer closed the connection
                if self.request_state == Some(RequestState::Initial) {
                    // count an error if we were waiting for the first request
                    // otherwise, if we already had one completed request and response,
                    // and are waiting for the next one, we do not count a socket
                    // closing abruptly as an error
                    if self.keepalive_count == 0 {
                        self.frontend_socket.read_error();
                    }

                    metrics.service_stop();
                    self.frontend_readiness.reset();
                    self.backend_readiness.reset();
                } else {
                    self.frontend_socket.read_error();
                    let frontend_readiness = self.frontend_readiness.clone();
                    let backend_readiness = self.backend_readiness.clone();
                    self.log_request_error(
                        metrics,
                        &format!(
                            "front socket was closed, closing the session. Readiness: {frontend_readiness:?} -> {backend_readiness:?}, read {size} bytes"
                        )
                    );
                }
                return StateResult::CloseSession;
            }
            SocketResult::WouldBlock => {
                self.frontend_readiness.event.remove(Ready::readable());
            }
            SocketResult::Continue => {}
        };

        // TODO: split this in readable_parse_with_host and readable_parse_without_host
        self.readable_parse(metrics)
    }

    pub fn readable_parse(&mut self, metrics: &mut SessionMetrics) -> StateResult {
        let is_initial = self.request_state == Some(RequestState::Initial);
        // if there's no host, continue parsing until we find it
        let has_host = self
            .request_state
            .as_ref()
            .map(|r| r.has_host())
            .unwrap_or(false);
        if !has_host {
            let (request_state, header_end) = (
                self.request_state.take().unwrap(),
                self.request_header_end.take(),
            );
            let (request_state, header_end) = parse_request_until_stop(
                request_state,
                header_end,
                self.frontend_buffer.as_mut().unwrap(),
                self.added_request_header.as_ref(),
                &self.sticky_name,
            );

            self.request_state = Some(request_state);
            self.request_header_end = header_end;

            if unwrap_msg!(self.request_state.as_ref()).is_front_error() {
                incr!("http.frontend_parse_errors");

                self.set_answer(DefaultAnswerStatus::Answer400, None);
                gauge_add!("http.active_requests", 1);

                return StateResult::Continue;
            }

            let is_still_initial = self.request_state == Some(RequestState::Initial);
            if is_initial && !is_still_initial {
                gauge_add!("http.active_requests", 1);
                incr!("http.requests");
            }

            if unwrap_msg!(self.request_state.as_ref()).has_host() {
                self.backend_readiness.interest.insert(Ready::writable());
                return StateResult::ConnectBackend;
            } else {
                self.frontend_readiness.interest.insert(Ready::readable());
                return StateResult::Continue;
            }
        }

        self.backend_readiness.interest.insert(Ready::writable());
        match self.request_state {
            Some(RequestState::Request(_, _, _))
            | Some(RequestState::RequestWithBody(_, _, _, _)) => {
                if !self.frontend_buffer.as_ref().unwrap().needs_input() {
                    // stop reading
                    self.frontend_readiness.interest.remove(Ready::readable());
                }

                // if it was the first request, the front timeout duration
                // was set to request_timeout, which is much lower. For future
                // requests on this connection, we can wait a bit more
                self.container_frontend_timeout
                    .set_duration(self.configured_frontend_timeout);

                StateResult::Continue
            }
            Some(RequestState::RequestWithBodyChunks(_, _, _, Chunk::Ended)) => {
                error!(
                    "{}\tfront read should have stopped on chunk ended",
                    self.log_context()
                );
                self.frontend_readiness.interest.remove(Ready::readable());
                StateResult::Continue
            }
            Some(RequestState::RequestWithBodyChunks(_, _, _, Chunk::Error)) => {
                self.log_request_error(metrics, "front read should have stopped on chunk error");
                StateResult::CloseSession
            }
            Some(RequestState::RequestWithBodyChunks(_, _, _, _)) => {
                // if it was the first request, the front timeout duration
                // was set to request_timeout, which is much lower. For future
                // requests on this connection, we can wait a bit more
                self.container_frontend_timeout
                    .set_duration(self.configured_frontend_timeout);

                if !self.frontend_buffer.as_ref().unwrap().needs_input() {
                    let (request_state, header_end) = (
                        self.request_state.take().unwrap(),
                        self.request_header_end.take(),
                    );
                    let (request_state, header_end) = parse_request_until_stop(
                        request_state,
                        header_end,
                        self.frontend_buffer.as_mut().unwrap(),
                        self.added_request_header.as_ref(),
                        &self.sticky_name,
                    );

                    self.request_state = Some(request_state);
                    self.request_header_end = header_end;

                    if unwrap_msg!(self.request_state.as_ref()).is_front_error() {
                        self.log_request_error(
                            metrics,
                            "front chunk parsing error, closing the connection",
                        );
                        return StateResult::CloseSession;
                    }

                    if let Some(RequestState::RequestWithBodyChunks(_, _, _, Chunk::Ended)) =
                        self.request_state
                    {
                        self.frontend_readiness.interest.remove(Ready::readable());
                    }
                }
                self.backend_readiness.interest.insert(Ready::writable());
                StateResult::Continue
            }
            // TODO: properly pattern match
            _ => {
                let (request_state, header_end) = (
                    // avoid this unwrap
                    self.request_state.take().unwrap(),
                    self.request_header_end.take(),
                );
                let (request_state, header_end) = parse_request_until_stop(
                    request_state,
                    header_end,
                    self.frontend_buffer.as_mut().unwrap(),
                    self.added_request_header.as_ref(),
                    &self.sticky_name,
                );

                self.request_state = Some(request_state);
                self.request_header_end = header_end;

                if unwrap_msg!(self.request_state.as_ref()).is_front_error() {
                    self.set_answer(DefaultAnswerStatus::Answer400, None);
                    return StateResult::Continue;
                }

                if let Some(RequestState::Request(_, _, _)) = self.request_state {
                    self.frontend_readiness.interest.remove(Ready::readable());
                }
                self.backend_readiness.interest.insert(Ready::writable());
                StateResult::Continue
            }
        }
    }

    fn writable_default_answer(&mut self, metrics: &mut SessionMetrics) -> StateResult {
        let res = if let SessionStatus::DefaultAnswer(_, ref buf, mut index) = self.status {
            let len = buf.len();

            let mut sz = 0usize;
            let mut res = SocketResult::Continue;
            while res == SocketResult::Continue && index < len {
                let (current_sz, current_res) = self.frontend_socket.socket_write(&buf[index..]);
                res = current_res;
                sz += current_sz;
                index += current_sz;
            }

            count!("bytes_out", sz as i64);
            metrics.bout += sz;

            if res != SocketResult::Continue {
                self.frontend_readiness.event.remove(Ready::writable());
            }

            if index == len {
                metrics.service_stop();
                self.log_default_answer_success(metrics);
                self.frontend_readiness.reset();
                self.backend_readiness.reset();
                return StateResult::CloseSession;
            }

            res
        } else {
            return StateResult::CloseSession;
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

    // Forward content to session
    pub fn writable(&mut self, metrics: &mut SessionMetrics) -> StateResult {
        //handle default answers
        if let SessionStatus::DefaultAnswer(_, _, _) = self.status {
            return self.writable_default_answer(metrics);
        }

        if self.backend_buffer.is_none() {
            error!("no back buffer to write on the front socket");
            return StateResult::CloseSession;
        }

        let output_size = self.backend_buffer.as_ref().unwrap().output_data_size();
        if self
            .backend_buffer
            .as_ref()
            .map(|buf| buf.output_data_size() == 0 || buf.next_output_data().is_empty())
            .unwrap()
        {
            self.backend_readiness.interest.insert(Ready::readable());
            self.frontend_readiness.interest.remove(Ready::writable());
            return StateResult::Continue;
        }

        let mut sz = 0usize;
        let mut res = SocketResult::Continue;
        while res == SocketResult::Continue
            && self.backend_buffer.as_ref().unwrap().output_data_size() > 0
        {
            // no more data in buffer, stop here
            if self
                .backend_buffer
                .as_ref()
                .unwrap()
                .next_output_data()
                .is_empty()
            {
                self.backend_readiness.interest.insert(Ready::readable());
                self.frontend_readiness.interest.remove(Ready::writable());
                count!("bytes_out", sz as i64);
                metrics.bout += sz;
                return StateResult::Continue;
            }
            //let (current_sz, current_res) = self.frontend.socket_write(self.backend_buf.as_ref().unwrap().next_output_data());
            let (current_sz, current_res) = if self.frontend_socket.has_vectored_writes() {
                let bufs = self.backend_buffer.as_ref().unwrap().as_ioslice();
                if bufs.is_empty() {
                    break;
                }
                self.frontend_socket.socket_write_vectored(&bufs)
            } else {
                self.frontend_socket
                    .socket_write(self.backend_buffer.as_ref().unwrap().next_output_data())
            };

            res = current_res;
            self.backend_buffer
                .as_mut()
                .unwrap()
                .consume_output_data(current_sz);
            sz += current_sz;
        }
        count!("bytes_out", sz as i64);
        metrics.bout += sz;

        debug!(
            "{}\tFRONT [{}<-{:?}]: wrote {} bytes of {}, buffer position {} restart position {}",
            self.log_context(),
            self.frontend_token.0,
            self.backend_token,
            sz,
            output_size,
            self.backend_buffer.as_ref().unwrap().buffer_position,
            self.backend_buffer.as_ref().unwrap().start_parsing_position
        );

        match res {
            SocketResult::Error | SocketResult::Closed => {
                self.frontend_socket.write_error();
                self.log_request_error(metrics, "error writing to front socket, closing");
                return StateResult::CloseSession;
            }
            SocketResult::WouldBlock => {
                self.frontend_readiness.event.remove(Ready::writable());
            }
            SocketResult::Continue => {}
        }

        if !self.backend_buffer.as_ref().unwrap().can_restart_parsing() {
            self.backend_readiness.interest.insert(Ready::readable());
            return StateResult::Continue;
        }

        //handle this case separately as its cumbersome to do from the pattern match
        if let Some(sz) = self.must_continue_response() {
            self.frontend_readiness.interest.insert(Ready::readable());
            self.frontend_readiness.interest.remove(Ready::writable());

            if self.frontend_buffer.is_none() {
                if let Some(p) = self.pool.upgrade() {
                    if let Some(buf) = p.borrow_mut().checkout() {
                        self.frontend_buffer = Some(BufferQueue::with_buffer(buf));
                    } else {
                        error!("cannot get front buffer from pool, closing");
                        return StateResult::CloseSession;
                    }
                }
            }

            // we must now copy the body from front to back
            trace!("100-Continue => copying {} of body from front to back", sz);
            if let Some(buf) = self.frontend_buffer.as_mut() {
                buf.slice_output(sz);
                buf.consume_parsed_data(sz);
            }

            self.response_state = Some(ResponseState::Initial);
            self.response_header_end = None;
            self.request_state.as_mut().map(|r| {
                r.get_mut_connection()
                    .map(|conn| conn.continues = Continue::None)
            });

            return StateResult::Continue;
        }

        match self.response_state {
            // FIXME: should only restart parsing if we are using keepalive
            Some(ResponseState::Response(_, _))
            | Some(ResponseState::ResponseWithBody(_, _, _))
            | Some(ResponseState::ResponseWithBodyChunks(_, _, Chunk::Ended)) => {
                let frontend_keep_alive = self
                    .request_state
                    .as_ref()
                    .map(|r| r.should_keep_alive())
                    .unwrap_or(false);
                let backend_keep_alive = self
                    .response_state
                    .as_ref()
                    .map(|r| r.should_keep_alive())
                    .unwrap_or(false);

                save_http_status_metric(self.get_response_status());

                self.log_request_success(metrics);
                metrics.reset();

                if self.closing {
                    debug!("{} closing proxy, no keep alive", self.log_context());
                    self.frontend_readiness.reset();
                    self.backend_readiness.reset();
                    return StateResult::CloseSession;
                }

                //FIXME: we could get smarter about this
                // with no keepalive on backend, we could open a new backend ConnectionError
                // with no keepalive on front but keepalive on backend, we could have
                // a pool of connections
                match (frontend_keep_alive, backend_keep_alive) {
                    (true, true) => {
                        debug!("{} keep alive front/back", self.log_context());
                        self.reset();
                        self.frontend_readiness.interest =
                            Ready::readable() | Ready::hup() | Ready::error();
                        self.backend_readiness.interest = Ready::hup() | Ready::error();

                        StateResult::Continue
                    }
                    (true, false) => {
                        debug!("{} keep alive front", self.log_context());
                        self.reset();
                        self.frontend_readiness.interest =
                            Ready::readable() | Ready::hup() | Ready::error();
                        self.backend_readiness.interest = Ready::hup() | Ready::error();
                        StateResult::CloseBackend
                    }
                    _ => {
                        debug!("{} no keep alive", self.log_context());
                        self.frontend_readiness.reset();
                        self.backend_readiness.reset();
                        StateResult::CloseSession
                    }
                }
            }
            Some(ResponseState::ResponseWithBodyCloseDelimited(_, _, backend_closed)) => {
                self.backend_readiness.interest.insert(Ready::readable());
                if backend_closed {
                    save_http_status_metric(self.get_response_status());
                    self.log_request_success(metrics);

                    StateResult::CloseSession
                } else {
                    StateResult::Continue
                }
            }
            // restart parsing, since there will be other chunks next
            Some(ResponseState::ResponseWithBodyChunks(_, _, _)) => {
                self.backend_readiness.interest.insert(Ready::readable());
                StateResult::Continue
            }
            //we're not done parsing the headers
            Some(ResponseState::HasStatusLine(_, _))
            | Some(ResponseState::HasUpgrade(_, _, _))
            | Some(ResponseState::HasLength(_, _, _)) => {
                self.backend_readiness.interest.insert(Ready::readable());
                StateResult::Continue
            }
            _ => {
                self.frontend_readiness.reset();
                self.backend_readiness.reset();
                StateResult::CloseSession
            }
        }
    }

    // Forward content to cluster
    pub fn backend_writable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        if let SessionStatus::DefaultAnswer(_, _, _) = self.status {
            error!(
                "{}\tsending default answer, should not write to back",
                self.log_context()
            );
            self.backend_readiness.interest.remove(Ready::writable());
            self.frontend_readiness.interest.insert(Ready::writable());
            return SessionResult::Continue;
        }

        if self
            .frontend_buffer
            .as_ref()
            .map(|buf| buf.output_data_size() == 0 || buf.next_output_data().is_empty())
            .unwrap()
        {
            self.frontend_readiness.interest.insert(Ready::readable());
            self.backend_readiness.interest.remove(Ready::writable());
            return SessionResult::Continue;
        }

        let output_size = self.frontend_buffer.as_ref().unwrap().output_data_size();
        if self.backend_socket.is_none() {
            self.log_request_error(metrics, "back socket not found, closing connection");
            return SessionResult::Close;
        }

        let mut sz = 0usize;
        let mut socket_result = SocketResult::Continue;

        {
            let sock = unwrap_msg!(self.backend_socket.as_mut());
            while socket_result == SocketResult::Continue
                && self.frontend_buffer.as_ref().unwrap().output_data_size() > 0
            {
                // no more data in buffer, stop here
                if self
                    .frontend_buffer
                    .as_ref()
                    .unwrap()
                    .next_output_data()
                    .is_empty()
                {
                    self.frontend_readiness.interest.insert(Ready::readable());
                    self.backend_readiness.interest.remove(Ready::writable());
                    metrics.backend_bout += sz;
                    return SessionResult::Continue;
                }
                /*
                let (current_sz, current_res) = sock.socket_write(self.frontend_buf.as_ref().unwrap().next_output_data());
                */
                let bufs = self.frontend_buffer.as_ref().unwrap().as_ioslice();
                if bufs.is_empty() {
                    break;
                }
                let (current_sz, current_res) = sock.socket_write_vectored(&bufs);
                //println!("vectored io returned {:?}", (current_sz, current_res));
                socket_result = current_res;
                self.frontend_buffer
                    .as_mut()
                    .unwrap()
                    .consume_output_data(current_sz);
                sz += current_sz;
            }
        }

        metrics.backend_bout += sz;

        debug!(
            "{}\tBACK [{}->{:?}]: wrote {} bytes of {}",
            self.log_context(),
            self.frontend_token.0,
            self.backend_token,
            sz,
            output_size
        );

        match socket_result {
            // the back socket is not writable anymore, so we can drop
            // the front buffer, no more data can be transmitted.
            // But the socket might still be readable, or if it is
            // closed, we might still have some data in the buffer.
            // As an example, we can get an early response for a large
            // POST request to refuse it and prevent all of the data
            // from being transmitted, awith the backend server closing
            // the socket right after sending the response
            SocketResult::Error | SocketResult::Closed => {
                self.frontend_buffer = None;
                self.frontend_readiness.interest.remove(Ready::readable());
                self.backend_readiness.interest.insert(Ready::readable());
                self.backend_readiness.interest.remove(Ready::writable());
                return SessionResult::Continue;
            }
            SocketResult::WouldBlock => {
                self.backend_readiness.event.remove(Ready::writable());
            }
            SocketResult::Continue => {}
        }

        // FIXME/ should read exactly as much data as needed
        if self.frontend_buffer.as_ref().unwrap().can_restart_parsing() {
            match self.request_state {
                // the entire request was transmitted
                Some(RequestState::Request(_, _, _))
                | Some(RequestState::RequestWithBody(_, _, _, _))
                | Some(RequestState::RequestWithBodyChunks(_, _, _, Chunk::Ended)) => {
                    // return the buffer to the pool
                    // if there's still data in there, keep it for pipelining
                    if self.must_continue_request()
                        && self.frontend_buffer.as_ref().map(|buf| buf.empty()) == Some(true)
                    {
                        self.frontend_buffer = None;
                    }
                    self.frontend_readiness.interest.remove(Ready::readable());
                    self.backend_readiness.interest.insert(Ready::readable());
                    self.backend_readiness.interest.remove(Ready::writable());

                    // cancel the front timeout while we are waiting for the server to answer
                    self.container_frontend_timeout.cancel();
                    if let Some(token) = self.backend_token.as_ref() {
                        self.container_backend_timeout.set(*token);
                    }
                    SessionResult::Continue
                }
                Some(RequestState::RequestWithBodyChunks(_, _, _, Chunk::Initial)) => {
                    if !self.must_continue_request() {
                        self.frontend_readiness.interest.insert(Ready::readable());
                    } else {
                        // wait for the 100 continue response from the backend
                        // keep the front buffer
                        self.frontend_readiness.interest.remove(Ready::readable());
                        self.backend_readiness.interest.insert(Ready::readable());
                        self.backend_readiness.interest.remove(Ready::writable());
                    }
                    SessionResult::Continue
                }
                Some(RequestState::RequestWithBodyChunks(_, _, _, _)) => {
                    self.frontend_readiness.interest.insert(Ready::readable());
                    SessionResult::Continue
                }
                //we're not done parsing the headers
                Some(RequestState::HasRequestLine(_, _))
                | Some(RequestState::HasHost(_, _, _))
                | Some(RequestState::HasLength(_, _, _))
                | Some(RequestState::HasHostAndLength(_, _, _, _)) => {
                    self.frontend_readiness.interest.insert(Ready::readable());
                    SessionResult::Continue
                }
                _ => {
                    self.log_request_error(metrics, "invalid state, closing connection");
                    SessionResult::Close
                }
            }
        } else {
            self.frontend_readiness.interest.insert(Ready::readable());
            self.backend_readiness.interest.insert(Ready::writable());
            SessionResult::Continue
        }
    }

    // Read content from cluster
    pub fn backend_readable(&mut self, metrics: &mut SessionMetrics) -> StateResult {
        if !self.container_backend_timeout.reset() {
            error!(
                "could not reset back timeout {:?}",
                self.configured_backend_timeout
            );
            self.print_state(&format!("{:?}", self.protocol));
        }

        if let SessionStatus::DefaultAnswer(_, _, _) = self.status {
            error!(
                "{}\tsending default answer, should not read from back socket",
                self.log_context()
            );
            self.backend_readiness.interest.remove(Ready::readable());
            return StateResult::Continue;
        }

        if self.backend_buffer.is_none() {
            if let Some(pool) = self.pool.upgrade() {
                match pool.borrow_mut().checkout() {
                    Some(buf) => {
                        self.backend_buffer = Some(BufferQueue::with_buffer(buf));
                    }
                    None => {
                        error!("cannot get back buffer from pool, closing");
                        return StateResult::CloseSession;
                    }
                }
            }
        }

        if self
            .backend_buffer
            .as_ref()
            .unwrap()
            .buffer
            .available_space()
            == 0
        {
            self.backend_readiness.interest.remove(Ready::readable());
            return StateResult::Continue;
        }

        if self.backend_socket.is_none() {
            self.log_request_error(metrics, "back socket not found, closing connection");
            return StateResult::CloseSession;
        }

        let (sz, socket_state) = {
            let sock = unwrap_msg!(self.backend_socket.as_mut());
            sock.socket_read(self.backend_buffer.as_mut().unwrap().buffer.space())
        };

        if let Some(backend_buf) = self.backend_buffer.as_mut() {
            backend_buf.buffer.fill(sz);
            backend_buf.sliced_input(sz);
        };

        metrics.backend_bin += sz;

        debug!(
            "{}\tBACK  [{}<-{:?}]: read {} bytes",
            self.log_context(),
            self.frontend_token.0,
            self.backend_token,
            sz
        );

        if socket_state != SocketResult::Continue || sz == 0 {
            self.backend_readiness.event.remove(Ready::readable());
        }

        if socket_state == SocketResult::Error {
            self.log_request_error(metrics, "back socket read error, closing connection");
            return StateResult::CloseSession;
        }

        // isolate that here because the "ref protocol" and the self.state = " make borrowing conflicts
        if let Some(ResponseState::ResponseUpgrade(_, _, ref protocol)) = self.response_state {
            debug!("got an upgrade state[{}]: {:?}", line!(), protocol);
            if compare_no_case(protocol.as_bytes(), "websocket".as_bytes()) {
                self.container_frontend_timeout.reset();
                self.container_backend_timeout.reset();
                return StateResult::Upgrade;
            } else {
                //FIXME: should we upgrade to a pipe or send an error?
                return StateResult::Continue;
            }
        }

        match self.response_state {
            Some(ResponseState::Response(_, _)) => {
                self.log_request_error(
                    metrics,
                    "should not go back in backend_readable if the whole response was parsed",
                );
                StateResult::CloseSession
            }
            Some(ResponseState::ResponseWithBody(_, _, _)) => {
                self.frontend_readiness.interest.insert(Ready::writable());
                if !self.backend_buffer.as_ref().unwrap().needs_input() {
                    metrics.backend_stop();
                    self.backend_stop = Some(Instant::now());
                    self.backend_readiness.interest.remove(Ready::readable());
                }
                StateResult::Continue
            }
            Some(ResponseState::ResponseWithBodyChunks(_, _, Chunk::Ended)) => {
                use nom::HexDisplay;
                self.backend_readiness.interest.remove(Ready::readable());
                match sz {
                    0 => StateResult::Continue,
                    _ => {
                        error!(
                            "{}\tback read should have stopped on chunk ended\nreq: {:?} res:{:?}\ndata:{}",
                            self.log_context(), self.request_state, self.response_state,
                            self.backend_buffer.as_ref().unwrap().unparsed_data().to_hex(16)
                        );
                        self.log_request_error(
                            metrics,
                            "back read should have stopped on chunk ended",
                        );
                        StateResult::CloseSession
                    }
                }
            }
            Some(ResponseState::ResponseWithBodyChunks(_, _, Chunk::Error)) => {
                self.log_request_error(metrics, "back read should have stopped on chunk error");
                StateResult::CloseSession
            }
            Some(ResponseState::ResponseWithBodyChunks(_, _, _)) => {
                if !self.backend_buffer.as_ref().unwrap().needs_input() {
                    let (response_state, header_end, is_head) = (
                        self.response_state.take().unwrap(),
                        self.response_header_end.take(),
                        self.request_state
                            .as_ref()
                            .map(|request| request.is_head())
                            .unwrap_or(false),
                    );

                    {
                        let sticky_session = self.sticky_session.as_ref().and_then(|session| {
                            if self.should_add_sticky_header(session) {
                                Some(session)
                            } else {
                                None
                            }
                        });

                        let (response_state, header_end) = parse_response_until_stop(
                            response_state,
                            header_end,
                            self.backend_buffer.as_mut().unwrap(),
                            is_head,
                            &self.added_response_header,
                            &self.sticky_name,
                            sticky_session,
                            self.cluster_id.as_deref(),
                        );

                        self.response_state = Some(response_state);
                        self.response_header_end = header_end;
                    }

                    if unwrap_msg!(self.response_state.as_ref()).is_back_error() {
                        self.log_request_error(
                            metrics,
                            "back socket chunk parse error, closing connection",
                        );
                        return StateResult::CloseSession;
                    }

                    if let Some(ResponseState::ResponseWithBodyChunks(_, _, Chunk::Ended)) =
                        self.response_state
                    {
                        metrics.backend_stop();
                        self.backend_stop = Some(Instant::now());
                        self.backend_readiness.interest.remove(Ready::readable());
                    }
                }
                self.frontend_readiness.interest.insert(Ready::writable());
                StateResult::Continue
            }
            Some(ResponseState::ResponseWithBodyCloseDelimited(_, _, _)) => {
                self.frontend_readiness.interest.insert(Ready::writable());
                if sz > 0 {
                    if let Some(buf) = self.backend_buffer.as_mut() {
                        buf.slice_output(sz);
                        buf.consume_parsed_data(sz);
                    }
                }

                if let ResponseState::ResponseWithBodyCloseDelimited(rl, conn, backend_closed) =
                    self.response_state.take().unwrap()
                {
                    if socket_state == SocketResult::Error
                        || socket_state == SocketResult::Closed
                        || sz == 0
                    {
                        self.response_state = Some(ResponseState::ResponseWithBodyCloseDelimited(
                            rl, conn, true,
                        ));

                        // if the back buffer is already empty, we can stop here
                        if self
                            .backend_buffer
                            .as_ref()
                            .map(|buf| {
                                buf.output_data_size() == 0 || buf.next_output_data().is_empty()
                            })
                            .unwrap()
                        {
                            save_http_status_metric(self.get_response_status());
                            self.log_request_success(metrics);
                            return StateResult::CloseSession;
                        }
                    } else {
                        self.response_state = Some(ResponseState::ResponseWithBodyCloseDelimited(
                            rl,
                            conn,
                            backend_closed,
                        ));
                    }
                }

                StateResult::Continue
            }
            Some(ResponseState::Error(_, _, _, _, _)) => panic!(
                "{}\tback read should have stopped on responsestate error",
                self.log_context()
            ),
            // Some(ResponseState::Initial) should be pattern matched
            // None should be pattern matched
            // can this match arm be triggered by something else?
            // TODO: we MUST change this
            _ => {
                let (response_state, header_end, is_head) = (
                    // dangerous, should be pattern matched instead
                    self.response_state.take().unwrap(),
                    self.response_header_end.take(),
                    self.request_state
                        .as_ref()
                        .map(|request| request.is_head())
                        .unwrap_or(false),
                );

                // why open a scope here?
                {
                    let sticky_session = self.sticky_session.as_ref().and_then(|session| {
                        if self.should_add_sticky_header(session) {
                            Some(session)
                        } else {
                            None
                        }
                    });

                    let (response_state2, header_end2) = parse_response_until_stop(
                        response_state,
                        header_end,
                        self.backend_buffer.as_mut().unwrap(),
                        is_head,
                        &self.added_response_header,
                        &self.sticky_name,
                        sticky_session,
                        self.cluster_id.as_deref(),
                    );

                    self.response_state = Some(response_state2);
                    self.response_header_end = header_end2;
                };

                // we may check for 499 with get_status_line
                // here and return (ProtocolResult::Continue, SessionResult::CloseSession)
                // this should close the connection without sending a response to the client

                if unwrap_msg!(self.response_state.as_ref()).is_back_error() {
                    self.set_answer(DefaultAnswerStatus::Answer502, None);
                    return self.writable(metrics);
                }

                if let Some(ResponseState::Response(_, _)) = self.response_state {
                    metrics.backend_stop();
                    self.backend_stop = Some(Instant::now());
                    self.backend_readiness.interest.remove(Ready::readable());
                }

                if let Some(ResponseState::ResponseUpgrade(_, _, ref protocol)) =
                    self.response_state
                {
                    debug!("got an upgrade state[{}]: {:?}", line!(), protocol);
                    if compare_no_case(protocol.as_bytes(), "websocket".as_bytes()) {
                        self.container_frontend_timeout.reset();
                        self.container_backend_timeout.reset();
                        return StateResult::Upgrade;
                    }

                    //FIXME: should we upgrade to a pipe or send an error?
                    return StateResult::Continue;
                }

                self.frontend_readiness.interest.insert(Ready::writable());
                StateResult::Continue
            }
        }
    }

    // Check if the connection already has a sticky session header
    // The connection will have a sticky session header if the client sent one
    // If it's the same as the one we want to set, don't set it.
    // If the connection doesn't have a sticky session header or if it's different
    // from the one we want to set, then it should be set.
    fn should_add_sticky_header(&self, session: &StickySession) -> bool {
        self.request_state
            .as_ref()
            .and_then(|request| request.get_keep_alive())
            .and_then(|conn| conn.sticky_session.as_ref())
            .map(|sticky_client| sticky_client != &session.sticky_id)
            .unwrap_or(true)
    }

    pub fn cancel_backend_timeout(&mut self) {
        self.container_backend_timeout.cancel();
    }
}

impl<Front: SocketHandler, L: ListenerHandler + L7ListenerHandler> Http<Front, L> {
    /*
    TODO: check that the logic is preserved somewhere else
    pub fn remove_backend(&mut self) -> (Option<String>, Option<SocketAddr>) {
        debug!(
            "{}\tPROXY [{} -> {}] CLOSED BACKEND",
            self.log_context(),
            self.frontend_token.0,
            self.backend_token
                .map(|t| format!("{}", t.0))
                .unwrap_or_else(|| "-".to_string())
        );
        let addr: Option<SocketAddr> = self.backend.as_ref().and_then(|sock| sock.peer_addr().ok());
        self.cancel_backend_timeout();
        self.backend = None;
        self.backend_token = None;
        (self.cluster_id.clone(), addr)
    }
    */

    //FIXME: check the token passed as argument
    fn close_backend(&mut self, proxy: Rc<RefCell<dyn L7Proxy>>, metrics: &mut SessionMetrics) {
        debug!(
            "{}\tPROXY [{} -> {}] CLOSED BACKEND",
            self.log_context(),
            self.frontend_token.0,
            self.backend_token
                .map(|t| format!("{}", t.0))
                .unwrap_or_else(|| "-".to_string())
        );

        if let Some(token) = self.backend_token {
            if let Some(socket) = &mut self.backend_socket {
                let proxy = proxy.borrow();
                if let Err(e) = proxy.deregister_socket(socket) {
                    error!("error deregistering back socket({:?}): {:?}", socket, e);
                }
                if let Err(e) = socket.shutdown(Shutdown::Both) {
                    if e.kind() != ErrorKind::NotConnected {
                        error!("error shutting down back socket({:?}): {:?}", socket, e);
                    }
                }

                proxy.remove_session(token);
            }

            if self.backend_connection_status != BackendConnectionStatus::NotConnected {
                self.backend_readiness.event = Ready::empty();
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
        self.backend
            .as_mut()
            .map(|b| b.borrow_mut().active_requests += 1);
        true
    }

    // -> host, path, method
    pub fn extract_route(&self) -> anyhow::Result<(&str, &str, &Method)> {
        let given_host = self.get_host().with_context(|| "No host given")?;

        // redundant
        // we only keep host, but we need the request's hostname in frontend_from_request (calling twice hostname_and_port)
        let (remaining_input, (hostname, port)) = match hostname_and_port(given_host.as_bytes()) {
            Ok(tuple) => tuple,

            Err(parse_error) => {
                // parse_error contains a slice of given_host, which should NOT escape this scope
                bail!("Hostname parsing failed for host {given_host}: {parse_error}");
            }
        };
        if remaining_input != &b""[..] {
            bail!("invalid remaining chars after hostname {}", given_host);
        }

        //FIXME: we should check that the port is right too
        let host = if port == Some(&b"80"[..]) {
            // it is alright to call from_utf8_unchecked,
            // we already verified that there are only ascii
            // chars in there
            unsafe { from_utf8_unchecked(hostname) }
        } else {
            given_host
        };

        let request_line = self
            .request_state
            .as_ref()
            .and_then(|request_state| request_state.get_request_line())
            .with_context(|| "No request line given in the request state")?;

        Ok((host, &request_line.uri, &request_line.method))
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
            .frontend_from_request(host, uri, method);

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
        let sticky_session = self
            .request_state
            .as_ref()
            .and_then(|request_state| request_state.get_sticky_session());

        let (backend, conn) = match self.get_backend_for_sticky_session(
            frontend_should_stick,
            sticky_session,
            cluster_id,
            proxy,
        ) {
            Ok((b, c)) => (b, c),
            Err(e) => {
                self.set_answer(DefaultAnswerStatus::Answer503, None);
                return Err(e)
                    .with_context(|| format!("Could not find a backend for cluster {cluster_id}"));
            }
        };

        if frontend_should_stick {
            let sticky_name = self.listener.borrow().get_sticky_name().to_string();

            self.sticky_session = Some(StickySession::new(
                backend
                    .borrow()
                    .sticky_id
                    .clone()
                    .unwrap_or_else(|| backend.borrow().backend_id.clone()),
            ));

            // stick session to listener (how? why?)
            self.sticky_name = sticky_name;
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
            } else if let Some(token) = self.backend_token {
                self.close_backend(proxy.clone(), metrics);

                //reset the back token here so we can remove it
                //from the slab after backend_from* fails
                self.set_backend_token(token);
            }
        }

        //replacing with a connection to another cluster
        if old_cluster_id.is_some() && old_cluster_id.as_ref() != Some(&cluster_id) {
            if let Some(token) = self.backend_token {
                self.close_backend(proxy.clone(), metrics);

                //reset the back token here so we can remove it
                //from the slab after backend_from* fails
                self.set_backend_token(token);
            }
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

        self.backend_readiness.interest = Ready::writable() | Ready::hup() | Ready::error();

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
                /*
                TODO: rewrite elsewhere
                let not_enough_memory = {
                    let sessions = proxy.borrow().sessions.borrow();
                    sessions.slab.len() >= sessions.slab_capacity()
                };

                if not_enough_memory {
                    error!("not enough memory, cannot connect to backend");
                    self.set_answer(DefaultAnswerStatus::Answer503, None);
                    bail!(format!("Too many connections on cluster {}", cluster_id));
                }
                */

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

                    push_event(Event::BackendUp(
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

                push_event(Event::BackendDown(
                    backend.backend_id.clone(),
                    backend.address,
                ));
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
                self.backend_readiness.interest.insert(Ready::readable());
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
                trace!(
                    "front readable\tinterpreting session order {:?}",
                    state_result
                );

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
                match self.backend_writable(metrics) {
                    SessionResult::Continue => {}
                    SessionResult::Upgrade => return StateResult::Upgrade,
                    SessionResult::Close => return StateResult::CloseSession,
                }
            }

            if backend_interest.is_readable() {
                let state_result = self.backend_readable(metrics);
                if state_result != StateResult::Continue {
                    return state_result;
                }
            }

            if frontend_interest.is_writable() {
                let state_result = self.writable(metrics);
                trace!(
                    "front writable\tinterpreting session order {:?}",
                    state_result
                );

                if state_result != StateResult::Continue {
                    return state_result;
                }
            }

            if backend_interest.is_hup() {
                match self.backend_hup() {
                    StateResult::Continue => {
                        continue;
                    }
                    StateResult::CloseSession | StateResult::CloseBackend => {
                        return self.backend_hup();
                    }
                    _ => {
                        self.backend_readiness.event.remove(Ready::hup());
                        return self.backend_hup();
                    }
                };
            }

            if frontend_interest.is_error() {
                error!(
                    "PROXY session {:?} front error, disconnecting",
                    self.frontend_token
                );

                self.frontend_readiness.interest = Ready::empty();
                self.backend_readiness().interest = Ready::empty();

                return StateResult::CloseSession;
            }

            if backend_interest.is_error() && self.backend_hup() == StateResult::CloseSession {
                self.frontend_readiness.interest = Ready::empty();
                self.backend_readiness.interest = Ready::empty();

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

            self.print_state(&format!("{:?}", self.protocol));

            return StateResult::CloseSession;
        }

        StateResult::Continue
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
                // self.close_backend(proxy, metrics);
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
        if self.request_state != Some(RequestState::Initial) {
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
\t\ttoken: {:?}\treadiness: {:?}\tstate: {:?}\theader_end: {:?}
\tBackend:
\t\ttoken: {:?}\treadiness: {:?}\tstate: {:?}\theader_end: {:?}",
            context,
            self.frontend_token,
            self.frontend_readiness,
            self.request_state,
            self.request_header_end,
            self.backend_token,
            self.backend_readiness,
            self.response_state,
            self.response_header_end
        );
    }

    fn shutting_down(&mut self) -> SessionIsToBeClosed {
        if self
            .request_state
            .as_ref()
            .map(|r| *r == RequestState::Initial)
            .unwrap_or(false)
            && self
                .frontend_buffer
                .as_ref()
                .map(|b| !b.empty())
                .unwrap_or(true)
            && self
                .backend_buffer
                .as_ref()
                .map(|b| !b.empty())
                .unwrap_or(true)
        {
            true
        } else {
            self.closing = true;
            false
        }
    }
}

/// Save the backend http response status code metric
fn save_http_status_metric(rs_status_line: Option<&StatusLine>) {
    if let Some(rs_status_line) = rs_status_line {
        match rs_status_line.status {
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

struct SessionAddress(Option<SocketAddr>);

impl std::fmt::Display for SessionAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            None => write!(f, "X"),
            Some(SocketAddr::V4(addr)) => write!(f, "{addr}"),
            Some(SocketAddr::V6(addr)) => write!(f, "{addr}"),
        }
    }
}

pub struct OptionalString<'a> {
    inner: Option<&'a str>,
}

impl<'a> From<Option<&'a String>> for OptionalString<'a> {
    fn from(o: Option<&'a String>) -> Self {
        Self::new(o.map(|s| s.as_str()))
    }
}

impl<'a> std::fmt::Display for OptionalString<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.inner {
            None => write!(f, "-"),
            Some(s) => write!(f, "{s}"),
        }
    }
}

impl<'a> OptionalString<'a> {
    pub fn new(s: Option<&'a str>) -> Self {
        OptionalString { inner: s }
    }

    pub fn as_str(&self) -> &str {
        match &self.inner {
            None => "-",
            Some(s) => s,
        }
    }
}

struct OptionalRequest<'a> {
    inner: Option<(&'a parser::Method, &'a str)>,
}

impl<'a> OptionalRequest<'a> {
    fn new(inner: Option<(&'a parser::Method, &'a str)>) -> Self {
        OptionalRequest { inner }
    }
}

impl<'a> std::fmt::Display for OptionalRequest<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.inner {
            None => write!(f, "-"),
            Some((s1, s2)) => write!(f, "{s1} {s2}"),
        }
    }
}

struct OptionalStatus {
    inner: Option<u16>,
}

impl OptionalStatus {
    fn new(inner: Option<u16>) -> Self {
        OptionalStatus { inner }
    }
}

impl std::fmt::Display for OptionalStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match &self.inner {
            None => write!(f, "-"),
            Some(code) => write!(f, "{code}"),
        }
    }
}

pub struct AddedRequestHeader {
    pub request_id: Ulid,
    pub public_address: SocketAddr,
    pub peer_address: Option<SocketAddr>,
    pub protocol: Protocol,
    pub closing: bool,
}

impl AddedRequestHeader {
    pub fn added_request_header(&self, headers: &parser::ForwardedHeaders) -> String {
        use std::fmt::Write;

        //FIXME: should update the Connection header directly if present
        let closing_header = if self.closing {
            "Connection: close\r\n"
        } else {
            ""
        };

        let frontend_ip = self.public_address.ip();
        let frontend_port = self.public_address.port();
        let proto = match self.protocol {
            Protocol::HTTP => "http",
            Protocol::HTTPS => "https",
            _ => unreachable!(),
        };

        let mut s = String::new();

        if !headers.x_proto {
            if let Err(e) = write!(&mut s, "X-Forwarded-Proto: {proto}\r\n") {
                error!("could not append request header: {:?}", e);
            }
        }

        if !headers.x_port {
            if let Err(e) = write!(&mut s, "X-Forwarded-Port: {frontend_port}\r\n") {
                error!("could not append request header: {:?}", e);
            }
        }

        if let Some(peer_addr) = self.peer_address {
            let peer_ip = peer_addr.ip();
            let peer_port = peer_addr.port();
            match &headers.x_for {
                None => {
                    if let Err(e) = write!(&mut s, "X-Forwarded-For: {peer_ip}\r\n") {
                        error!("could not append request header: {:?}", e);
                    }
                }
                Some(value) => {
                    if let Err(e) = write!(&mut s, "X-Forwarded-For: {value}, {peer_ip}\r\n") {
                        error!("could not append request header: {:?}", e);
                    }
                }
            }

            match &headers.forwarded {
                None => {
                    if let Err(e) = write!(&mut s, "Forwarded: ") {
                        error!("could not append request header: {:?}", e);
                    }
                }
                Some(value) => {
                    if let Err(e) = write!(&mut s, "Forwarded: {value}, ") {
                        error!("could not append request header: {:?}", e);
                    }
                }
            }

            match (peer_ip, peer_port, frontend_ip) {
                (IpAddr::V4(_), peer_port, IpAddr::V4(_)) => {
                    if let Err(e) = write!(
                        &mut s,
                        "proto={proto};for={peer_ip}:{peer_port};by={frontend_ip}\r\n"
                    ) {
                        error!("could not append request header: {:?}", e);
                    }
                }
                (IpAddr::V4(_), peer_port, IpAddr::V6(_)) => {
                    if let Err(e) = write!(
                        &mut s,
                        "proto={proto};for={peer_ip}:{peer_port};by=\"{frontend_ip}\"\r\n"
                    ) {
                        error!("could not append request header: {:?}", e);
                    }
                }
                (IpAddr::V6(_), peer_port, IpAddr::V4(_)) => {
                    if let Err(e) = write!(
                        &mut s,
                        "proto={proto};for=\"{peer_ip}:{peer_port}\";by={frontend_ip}\r\n"
                    ) {
                        error!("could not append request header: {:?}", e);
                    }
                }
                (IpAddr::V6(_), peer_port, IpAddr::V6(_)) => {
                    if let Err(e) = write!(
                        &mut s,
                        "proto={proto};for=\"{peer_ip}:{peer_port}\";by=\"{frontend_ip}\"\r\n"
                    ) {
                        error!("could not append request header: {:?}", e);
                    }
                }
            };
        }

        if let Err(e) = write!(&mut s, "Sozu-Id: {}\r\n{}", self.request_id, closing_header) {
            error!("could not append request header: {:?}", e);
        }

        s
    }
}

/*
#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  #[cfg(target_pointer_width = "64")]
  fn size_test() {
    assert_size!(SessionStatus, 24);
    assert_size!(String, 24);
    assert_size!(Option<String>, 24);
    assert_size!(DefaultAnswerStatus, 1);
    assert_size!(Readiness, 16);
    assert_size!(Option<BufferQueue>, 88);
    assert_size!(Option<SocketAddr>, 32);
    assert_size!(Option<RequestState>, 288);
    assert_size!(Option<ResponseState>, 256);
    assert_size!(Option<AddedRequestHeader>, 88);
  }
}
*/
