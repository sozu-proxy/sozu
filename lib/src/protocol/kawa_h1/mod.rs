pub mod answers;
pub mod diagnostics;
pub mod editor;
pub mod parser;

use std::{
    cell::RefCell,
    io::ErrorKind,
    net::{Shutdown, SocketAddr},
    rc::{Rc, Weak},
    time::{Duration, Instant},
};

use mio::{net::TcpStream, Interest, Token};
use rusty_ulid::Ulid;
use sozu_command::{
    config::MAX_LOOP_ITERATIONS,
    logging::EndpointRecord,
    proto::command::{Event, EventKind, ListenerType},
};
// use time::{Duration, Instant};

use crate::{
    backends::{Backend, BackendError},
    pool::{Checkout, Pool},
    protocol::{
        http::{
            answers::DefaultAnswerStream,
            diagnostics::{diagnostic_400_502, diagnostic_413_507},
            editor::HttpContext,
            parser::Method,
        },
        SessionState,
    },
    retry::RetryPolicy,
    router::Route,
    server::{push_event, CONN_RETRIES},
    socket::{stats::socket_rtt, SocketHandler, SocketResult, TransportProtocol},
    sozu_command::{logging::LogContext, ready::Ready},
    timer::TimeoutContainer,
    AcceptError, BackendConnectAction, BackendConnectionError, BackendConnectionStatus,
    L7ListenerHandler, L7Proxy, ListenerHandler, Protocol, ProxySession, Readiness,
    RetrieveClusterError, SessionIsToBeClosed, SessionMetrics, SessionResult, StateResult,
};

/// This macro is defined uniquely in this module to help the tracking of kawa h1
/// issues inside Sōzu
macro_rules! log_context {
    ($self:expr) => {
        format!(
            "KAWA-H1\t{}\tSession(public={}, session={}, frontend={}, readiness={}, backend={}, readiness={})\t >>>",
            $self.context.log_context(),
            $self.context.public_address.to_string(),
            $self.context.session_address.map(|addr| addr.to_string()).unwrap_or_else(|| "<none>".to_string()),
            $self.frontend_token.0,
            $self.frontend_readiness,
            $self.backend_token.map(|token| token.0.to_string()).unwrap_or_else(|| "<none>".to_string()),
            $self.backend_readiness,
        )
    };
}

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
pub enum DefaultAnswer {
    Answer301 {
        location: String,
    },
    Answer400 {
        message: String,
        phase: kawa::ParsingPhaseMarker,
        details: String,
    },
    Answer401 {},
    Answer404 {},
    Answer408 {
        duration: String,
    },
    Answer413 {
        message: String,
        phase: kawa::ParsingPhaseMarker,
        capacity: usize,
    },
    Answer502 {
        message: String,
        phase: kawa::ParsingPhaseMarker,
        details: String,
    },
    Answer503 {
        message: String,
    },
    Answer504 {
        duration: String,
    },
    Answer507 {
        phase: kawa::ParsingPhaseMarker,
        message: String,
        capacity: usize,
    },
}

impl From<&DefaultAnswer> for u16 {
    fn from(answer: &DefaultAnswer) -> u16 {
        match answer {
            DefaultAnswer::Answer301 { .. } => 301,
            DefaultAnswer::Answer400 { .. } => 400,
            DefaultAnswer::Answer401 { .. } => 401,
            DefaultAnswer::Answer404 { .. } => 404,
            DefaultAnswer::Answer408 { .. } => 408,
            DefaultAnswer::Answer413 { .. } => 413,
            DefaultAnswer::Answer502 { .. } => 502,
            DefaultAnswer::Answer503 { .. } => 503,
            DefaultAnswer::Answer504 { .. } => 504,
            DefaultAnswer::Answer507 { .. } => 507,
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

pub enum ResponseStream {
    BackendAnswer(GenericHttpStream),
    DefaultAnswer(u16, DefaultAnswerStream),
}

/// Http will be contained in State which itself is contained by Session
pub struct Http<Front: SocketHandler, L: ListenerHandler + L7ListenerHandler> {
    answers: Rc<RefCell<answers::HttpAnswers>>,
    pub backend: Option<Rc<RefCell<Backend>>>,
    backend_connection_status: BackendConnectionStatus,
    pub backend_readiness: Readiness,
    pub backend_socket: Option<TcpStream>,
    backend_stop: Option<Instant>,
    pub backend_token: Option<Token>,
    pub container_backend_timeout: TimeoutContainer,
    pub container_frontend_timeout: TimeoutContainer,
    configured_backend_timeout: Duration,
    configured_connect_timeout: Duration,
    configured_frontend_timeout: Duration,
    /// attempts to connect to the backends during the session
    connection_attempts: u8,
    pub frontend_readiness: Readiness,
    pub frontend_socket: Front,
    frontend_token: Token,
    keepalive_count: usize,
    listener: Rc<RefCell<L>>,
    pub request_stream: GenericHttpStream,
    pub response_stream: ResponseStream,
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
            backend_readiness: Readiness::new(),
            backend_socket: None,
            backend_stop: None,
            backend_token: None,
            backend: None,
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
            response_stream: ResponseStream::BackendAnswer(GenericHttpStream::new(
                kawa::Kind::Response,
                kawa::Buffer::new(back_buffer),
            )),
            context: HttpContext {
                id: request_id,
                cluster_id: None,
                backend_id: None,
                backend_address: None,

                closing: false,
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
        trace!("{} ============== reset", log_context!(self));
        let response_stream = match &mut self.response_stream {
            ResponseStream::BackendAnswer(response_stream) => response_stream,
            _ => return,
        };

        self.context.reset();

        self.request_stream.clear();
        response_stream.clear();
        self.keepalive_count += 1;
        gauge_add!("http.active_requests", -1);

        if let Some(backend) = &mut self.backend {
            let mut backend = backend.borrow_mut();
            backend.active_requests = backend.active_requests.saturating_sub(1);
        }

        // reset the front timeout and cancel the back timeout while we are
        // waiting for a new request
        self.container_backend_timeout.cancel();
        self.container_frontend_timeout
            .set_duration(self.configured_frontend_timeout);
        self.frontend_readiness.interest = Ready::READABLE | Ready::HUP | Ready::ERROR;
        self.backend_readiness.interest = Ready::HUP | Ready::ERROR;

        // We are resetting the offset of request and response stream buffers
        // We do have to keep cursor position on the request, if there is data
        // in the request stream to preserve http pipelining.

        // Print the left-over response buffer output to track in which case it
        // may happens
        let response_storage = &mut response_stream.storage;
        if !response_storage.is_empty() {
            warn!(
                "{} Leftover fragment from response: {}",
                log_context!(self),
                parser::view(
                    response_storage.used(),
                    16,
                    &[response_storage.start, response_storage.end,],
                )
            );
        }

        response_storage.clear();
        if !self.request_stream.storage.is_empty() {
            self.frontend_readiness.event.insert(Ready::READABLE);
        } else {
            self.request_stream.storage.clear();
        }
    }

    pub fn readable(&mut self, metrics: &mut SessionMetrics) -> StateResult {
        trace!("{} ============== readable", log_context!(self));
        if !self.container_frontend_timeout.reset() {
            error!(
                "could not reset front timeout {:?}",
                self.configured_frontend_timeout
            );
            self.print_state(self.protocol_string());
        }

        let response_stream = match &mut self.response_stream {
            ResponseStream::BackendAnswer(response_stream) => response_stream,
            ResponseStream::DefaultAnswer(..) => {
                error!(
                    "{} Sending default answer, should not read from frontend socket",
                    log_context!(self)
                );

                self.frontend_readiness.interest.remove(Ready::READABLE);
                self.frontend_readiness.interest.insert(Ready::WRITABLE);
                return StateResult::Continue;
            }
        };

        if self.request_stream.storage.is_full() {
            self.frontend_readiness.interest.remove(Ready::READABLE);
            if self.request_stream.is_main_phase() {
                self.backend_readiness.interest.insert(Ready::WRITABLE);
            } else {
                // client has filled its buffer and we can't empty it
                self.set_answer(DefaultAnswer::Answer413 {
                    capacity: self.request_stream.storage.capacity(),
                    phase: self.request_stream.parsing_phase.marker(),
                    message: diagnostic_413_507(self.request_stream.parsing_phase),
                });
            }
            return StateResult::Continue;
        }

        let (size, socket_state) = self
            .frontend_socket
            .socket_read(self.request_stream.storage.space());

        debug!("{} Read {} bytes", log_context!(self), size);

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

        trace!("{} ============== readable_parse", log_context!(self));
        let was_initial = self.request_stream.is_initial();
        let was_not_proxying = !self.request_stream.is_main_phase();

        kawa::h1::parse(&mut self.request_stream, &mut self.context);
        // kawa::debug_kawa(&self.request_stream);

        if was_initial && !self.request_stream.is_initial() {
            // if it was the first request, the front timeout duration
            // was set to request_timeout, which is much lower. For future
            // requests on this connection, we can wait a bit more
            self.container_frontend_timeout
                .set_duration(self.configured_frontend_timeout);
            gauge_add!("http.active_requests", 1);
            incr!("http.requests");
        }

        if let kawa::ParsingPhase::Error { marker, kind } = self.request_stream.parsing_phase {
            incr!("http.frontend_parse_errors");
            warn!(
                "{} Parsing request error in {:?}: {}",
                log_context!(self),
                marker,
                match kind {
                    kawa::ParsingErrorKind::Consuming { index } => {
                        let kawa = &self.request_stream;
                        parser::view(
                            kawa.storage.used(),
                            16,
                            &[
                                kawa.storage.start,
                                kawa.storage.head,
                                index as usize,
                                kawa.storage.end,
                            ],
                        )
                    }
                    kawa::ParsingErrorKind::Processing { message } => message.to_owned(),
                }
            );
            if response_stream.consumed {
                self.log_request_error(metrics, "Parsing error on the request");
                return StateResult::CloseSession;
            } else {
                let (message, details) = diagnostic_400_502(marker, kind, &self.request_stream);
                self.set_answer(DefaultAnswer::Answer400 {
                    phase: marker,
                    details,
                    message,
                });
                return StateResult::Continue;
            }
        }

        if self.request_stream.is_main_phase() {
            self.backend_readiness.interest.insert(Ready::WRITABLE);
            if was_not_proxying {
                // Sozu tries to connect only once all the headers were gathered and edited
                // this could be improved
                trace!("{} ============== HANDLE CONNECTION!", log_context!(self));
                return StateResult::ConnectBackend;
            }
        }
        if self.request_stream.is_terminated() {
            self.frontend_readiness.interest.remove(Ready::READABLE);
        }

        StateResult::Continue
    }

    pub fn writable(&mut self, metrics: &mut SessionMetrics) -> StateResult {
        trace!("{} ============== writable", log_context!(self));
        let response_stream = match &mut self.response_stream {
            ResponseStream::BackendAnswer(response_stream) => response_stream,
            _ => return self.writable_default_answer(metrics),
        };

        response_stream.prepare(&mut kawa::h1::BlockConverter);

        let bufs = response_stream.as_io_slice();
        if bufs.is_empty() && !self.frontend_socket.socket_wants_write() {
            self.frontend_readiness.interest.remove(Ready::WRITABLE);
            return StateResult::Continue;
        }

        let (size, socket_state) = self.frontend_socket.socket_write_vectored(&bufs);

        debug!("{} Wrote {} bytes", log_context!(self), size);

        if size > 0 {
            response_stream.consume(size);
            count!("bytes_out", size as i64);
            metrics.bout += size;
            self.backend_readiness.interest.insert(Ready::READABLE);
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

        if self.frontend_socket.socket_wants_write() {
            return StateResult::Continue;
        }

        if response_stream.is_terminated() && response_stream.is_completed() {
            if self.context.closing {
                debug!("{} closing proxy, no keep alive", log_context!(self));
                return StateResult::CloseSession;
            }

            match response_stream.detached.status_line {
                kawa::StatusLine::Response { code: 101, .. } => {
                    trace!("{} ============== HANDLE UPGRADE!", log_context!(self));
                    self.log_request_success(metrics);
                    return StateResult::Upgrade;
                }
                kawa::StatusLine::Response { code: 100, .. } => {
                    trace!("{} ============== HANDLE CONTINUE!", log_context!(self));
                    response_stream.clear();
                    self.log_request_success(metrics);
                    return StateResult::Continue;
                }
                kawa::StatusLine::Response { code: 103, .. } => {
                    self.backend_readiness.event.insert(Ready::READABLE);
                    trace!("{} ============== HANDLE EARLY HINT!", log_context!(self));
                    response_stream.clear();
                    self.log_request_success(metrics);
                    return StateResult::Continue;
                }
                _ => (),
            }

            let response_length_known = response_stream.body_size != kawa::BodySize::Empty;
            let request_length_known = self.request_stream.body_size != kawa::BodySize::Empty;
            if !(self.request_stream.is_terminated() && self.request_stream.is_completed())
                && request_length_known
            {
                error!(
                    "{} Response terminated before request, this case is not handled properly yet",
                    log_context!(self)
                );
                incr!("http.early_response_close");
                // FIXME: this will cause problems with pipelining
                // return StateResult::CloseSession;
            }

            // FIXME: we could get smarter about this
            // with no keepalive on backend, we could open a new backend ConnectionError
            // with no keepalive on front but keepalive on backend, we could have
            // a pool of connections
            trace!(
                "{} ============== HANDLE KEEP-ALIVE: {} {} {}",
                log_context!(self),
                self.context.keep_alive_frontend,
                self.context.keep_alive_backend,
                response_length_known
            );

            self.log_request_success(metrics);
            return match (
                self.context.keep_alive_frontend,
                self.context.keep_alive_backend,
                response_length_known,
            ) {
                (true, true, true) => {
                    debug!("{} Keep alive frontend/backend", log_context!(self));
                    metrics.reset();
                    self.reset();
                    StateResult::Continue
                }
                (true, false, true) => {
                    debug!("{} Keep alive frontend", log_context!(self));
                    metrics.reset();
                    self.reset();
                    StateResult::CloseBackend
                }
                _ => {
                    debug!("{} No keep alive", log_context!(self));
                    StateResult::CloseSession
                }
            };
        }
        StateResult::Continue
    }

    fn writable_default_answer(&mut self, metrics: &mut SessionMetrics) -> StateResult {
        trace!(
            "{} ============== writable_default_answer",
            log_context!(self)
        );
        let response_stream = match &mut self.response_stream {
            ResponseStream::DefaultAnswer(_, response_stream) => response_stream,
            _ => return StateResult::CloseSession,
        };
        let bufs = response_stream.as_io_slice();
        let (size, socket_state) = self.frontend_socket.socket_write_vectored(&bufs);

        count!("bytes_out", size as i64);
        metrics.bout += size;
        response_stream.consume(size);

        if size == 0 || socket_state != SocketResult::Continue {
            self.frontend_readiness.event.remove(Ready::WRITABLE);
        }

        if response_stream.is_completed() {
            save_http_status_metric(self.context.status, self.context.log_context());
            self.log_default_answer_success(metrics);
            self.frontend_readiness.reset();
            self.backend_readiness.reset();
            return StateResult::CloseSession;
        }

        if socket_state == SocketResult::Error {
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

    pub fn backend_writable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        trace!("{} ============== backend_writable", log_context!(self));
        if let ResponseStream::DefaultAnswer(..) = self.response_stream {
            error!(
                "{}\tsending default answer, should not write to back",
                log_context!(self)
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
        debug!("{} Wrote {} bytes", log_context!(self), size);

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
        trace!("{} ============== backend_readable", log_context!(self));
        if !self.container_backend_timeout.reset() {
            error!(
                "{} Could not reset back timeout {:?}",
                log_context!(self),
                self.configured_backend_timeout
            );
            self.print_state(self.protocol_string());
        }

        let response_stream = match &mut self.response_stream {
            ResponseStream::BackendAnswer(response_stream) => response_stream,
            _ => {
                error!(
                    "{} Sending default answer, should not read from backend socket",
                    log_context!(self),
                );

                self.backend_readiness.interest.remove(Ready::READABLE);
                self.frontend_readiness.interest.insert(Ready::WRITABLE);
                return SessionResult::Continue;
            }
        };

        let backend_socket = if let Some(backend_socket) = &mut self.backend_socket {
            backend_socket
        } else {
            self.log_request_error(metrics, "back socket not found, closing session");
            return SessionResult::Close;
        };

        if response_stream.storage.is_full() {
            self.backend_readiness.interest.remove(Ready::READABLE);
            if response_stream.is_main_phase() {
                self.frontend_readiness.interest.insert(Ready::WRITABLE);
            } else {
                // server has filled its buffer and we can't empty it
                let capacity = response_stream.storage.capacity();
                let phase = response_stream.parsing_phase.marker();
                let message = diagnostic_413_507(response_stream.parsing_phase);
                self.set_answer(DefaultAnswer::Answer507 {
                    capacity,
                    phase,
                    message,
                });
            }
            return SessionResult::Continue;
        }

        let (size, socket_state) = backend_socket.socket_read(response_stream.storage.space());
        debug!("{} Read {} bytes", log_context!(self), size);

        if size > 0 {
            response_stream.storage.fill(size);
            count!("back_bytes_in", size as i64);
            metrics.backend_bin += size;
            // if self.kawa_response.storage.is_full() {
            //     self.backend_readiness.interest.remove(Ready::READABLE);
            // }

            // In case the response starts while the request is not tagged as "terminated",
            // we place the timeout responsibility on the backend.
            // This can happen when:
            // - the request is malformed and doesn't have length information
            // - kawa fails to detect a properly terminated request (e.g. a GET request with no body and no length)
            // - the response can start before the end of the request (e.g. stream processing like compression)
            self.container_frontend_timeout.cancel();
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

        trace!(
            "{} ============== backend_readable_parse",
            log_context!(self)
        );
        kawa::h1::parse(response_stream, &mut self.context);
        // kawa::debug_kawa(&self.response_stream);

        if let kawa::ParsingPhase::Error { marker, kind } = response_stream.parsing_phase {
            incr!("http.backend_parse_errors");
            warn!(
                "{} Parsing response error in {:?}: {}",
                log_context!(self),
                marker,
                match kind {
                    kawa::ParsingErrorKind::Consuming { index } => {
                        parser::view(
                            response_stream.storage.used(),
                            16,
                            &[
                                response_stream.storage.start,
                                response_stream.storage.head,
                                index as usize,
                                response_stream.storage.end,
                            ],
                        )
                    }
                    kawa::ParsingErrorKind::Processing { message } => message.to_owned(),
                }
            );
            if response_stream.consumed {
                return SessionResult::Close;
            } else {
                let (message, details) = diagnostic_400_502(marker, kind, response_stream);
                self.set_answer(DefaultAnswer::Answer502 {
                    phase: marker,
                    details,
                    message,
                });
                return SessionResult::Continue;
            }
        }

        if response_stream.is_main_phase() {
            self.frontend_readiness.interest.insert(Ready::WRITABLE);
        }
        if response_stream.is_terminated() {
            metrics.backend_stop();
            self.backend_stop = Some(Instant::now());
            self.backend_readiness.interest.remove(Ready::READABLE);
        }
        SessionResult::Continue
    }
}

impl<Front: SocketHandler, L: ListenerHandler + L7ListenerHandler> Http<Front, L> {
    fn log_endpoint(&self) -> EndpointRecord {
        EndpointRecord::Http {
            method: self.context.method.as_deref(),
            authority: self.context.authority.as_deref(),
            path: self.context.path.as_deref(),
            reason: self.context.reason.as_deref(),
            status: self.context.status,
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

    pub fn log_request(&self, metrics: &SessionMetrics, error: bool, message: Option<&str>) {
        let listener = self.listener.borrow();
        let tags = self.context.authority.as_ref().and_then(|host| {
            let hostname = match host.split_once(':') {
                None => host,
                Some((hostname, _)) => hostname,
            };
            listener.get_tags(hostname)
        });

        let context = self.context.log_context();
        metrics.register_end_of_session(&context);

        log_access! {
            error,
            on_failure: { incr!("unsent-access-logs") },
            message: message,
            context,
            session_address: self.get_session_address(),
            backend_address: self.get_backend_address(),
            protocol: self.protocol_string(),
            endpoint: self.log_endpoint(),
            tags,
            client_rtt: socket_rtt(self.front_socket()),
            server_rtt: self.backend_socket.as_ref().and_then(socket_rtt),
            service_time: metrics.service_time(),
            response_time: metrics.backend_response_time(),
            request_time: metrics.request_time(),
            bytes_in: metrics.bin,
            bytes_out: metrics.bout,
            user_agent: self.context.user_agent.as_deref(),
        };
    }

    pub fn log_request_success(&self, metrics: &SessionMetrics) {
        save_http_status_metric(self.context.status, self.context.log_context());
        self.log_request(metrics, false, None);
    }

    pub fn log_default_answer_success(&self, metrics: &SessionMetrics) {
        self.log_request(metrics, false, None);
    }
    pub fn log_request_error(&self, metrics: &mut SessionMetrics, message: &str) {
        incr!("http.errors");
        error!(
            "{} Could not process request properly got: {}",
            log_context!(self),
            message
        );
        self.print_state(self.protocol_string());
        self.log_request(metrics, true, Some(message));
    }

    pub fn set_answer(&mut self, answer: DefaultAnswer) {
        let status = u16::from(&answer);
        if let ResponseStream::DefaultAnswer(old_status, ..) = self.response_stream {
            error!(
                "already set the default answer to {}, trying to set to {}",
                old_status, status
            );
        } else {
            match answer {
                DefaultAnswer::Answer301 { .. } => incr!(
                    "http.301.redirection",
                    self.context.cluster_id.as_deref(),
                    self.context.backend_id.as_deref()
                ),
                DefaultAnswer::Answer400 { .. } => incr!("http.400.errors"),
                DefaultAnswer::Answer401 { .. } => incr!(
                    "http.401.errors",
                    self.context.cluster_id.as_deref(),
                    self.context.backend_id.as_deref()
                ),
                DefaultAnswer::Answer404 { .. } => incr!("http.404.errors"),
                DefaultAnswer::Answer408 { .. } => incr!(
                    "http.408.errors",
                    self.context.cluster_id.as_deref(),
                    self.context.backend_id.as_deref()
                ),
                DefaultAnswer::Answer413 { .. } => incr!(
                    "http.413.errors",
                    self.context.cluster_id.as_deref(),
                    self.context.backend_id.as_deref()
                ),
                DefaultAnswer::Answer502 { .. } => incr!(
                    "http.502.errors",
                    self.context.cluster_id.as_deref(),
                    self.context.backend_id.as_deref()
                ),
                DefaultAnswer::Answer503 { .. } => incr!(
                    "http.503.errors",
                    self.context.cluster_id.as_deref(),
                    self.context.backend_id.as_deref()
                ),
                DefaultAnswer::Answer504 { .. } => incr!(
                    "http.504.errors",
                    self.context.cluster_id.as_deref(),
                    self.context.backend_id.as_deref()
                ),
                DefaultAnswer::Answer507 { .. } => incr!(
                    "http.507.errors",
                    self.context.cluster_id.as_deref(),
                    self.context.backend_id.as_deref()
                ),
            };
        }

        let mut kawa = self.answers.borrow().get(
            answer,
            self.context.id.to_string(),
            self.context.cluster_id.as_deref(),
            self.context.backend_id.as_deref(),
            self.context.get_route(),
        );
        kawa.prepare(&mut kawa::h1::BlockConverter);
        self.context.status = Some(status);
        self.context.reason = None;
        self.context.keep_alive_frontend = false;
        self.response_stream = ResponseStream::DefaultAnswer(status, kawa);
        self.frontend_readiness.interest = Ready::WRITABLE | Ready::HUP | Ready::ERROR;
        self.backend_readiness.interest = Ready::HUP | Ready::ERROR;
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
                if dur > Duration::from_secs(1) {
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
        self.context.cluster_id = Some(cluster_id);
    }

    pub fn set_backend_id(&mut self, backend_id: String) {
        self.context.backend_id = Some(backend_id);
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
    fn close_backend<P: L7Proxy>(&mut self, proxy: Rc<RefCell<P>>, metrics: &mut SessionMetrics) {
        self.container_backend_timeout.cancel();
        debug!(
            "{}\tPROXY [{}->{}] CLOSED BACKEND",
            log_context!(self),
            self.frontend_token.0,
            self.backend_token
                .map(|t| format!("{}", t.0))
                .unwrap_or_else(|| "-".to_string())
        );

        let proxy = proxy.borrow();
        if let Some(socket) = &mut self.backend_socket.take() {
            if let Err(e) = proxy.deregister_socket(socket) {
                error!(
                    "{} Error deregistering back socket({:?}): {:?}",
                    log_context!(self),
                    socket,
                    e
                );
            }
            if let Err(e) = socket.shutdown(Shutdown::Both) {
                if e.kind() != ErrorKind::NotConnected {
                    error!(
                        "{} Error shutting down back socket({:?}): {:?}",
                        log_context!(self),
                        socket,
                        e
                    );
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
                    self.context.cluster_id.as_deref(),
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
            error!(
                "{} Max connection attempt reached ({})",
                log_context!(self),
                self.connection_attempts,
            );

            self.set_answer(DefaultAnswer::Answer503 {
                message: format!(
                    "Max connection attempt reached: {}",
                    self.connection_attempts
                ),
            });
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

    fn cluster_id_from_request<P: L7Proxy>(
        &mut self,
        proxy: Rc<RefCell<P>>,
    ) -> Result<String, RetrieveClusterError> {
        let (host, uri, method) = match self.context.extract_route() {
            Ok(tuple) => tuple,
            Err(cluster_error) => {
                self.set_answer(DefaultAnswer::Answer400 {
                    phase: self.request_stream.parsing_phase.marker(),
                    details: cluster_error.to_string(),
                    message: "Could not extract the route after connection started, this should not happen.".into(),
                });
                return Err(cluster_error);
            }
        };

        let route_result = self
            .listener
            .borrow()
            .frontend_from_request(host, uri, method);

        let route = match route_result {
            Ok(route) => route,
            Err(frontend_error) => {
                self.set_answer(DefaultAnswer::Answer404 {});
                return Err(RetrieveClusterError::RetrieveFrontend(frontend_error));
            }
        };

        let cluster_id = match route {
            Route::Cluster(id) => id,
            Route::Deny => {
                self.set_answer(DefaultAnswer::Answer401 {});
                return Err(RetrieveClusterError::UnauthorizedRoute);
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
            self.set_answer(DefaultAnswer::Answer301 {
                location: format!("https://{host}{uri}"),
            });
            return Err(RetrieveClusterError::UnauthorizedRoute);
        }

        Ok(cluster_id)
    }

    pub fn backend_from_request<P: L7Proxy>(
        &mut self,
        cluster_id: &str,
        frontend_should_stick: bool,
        proxy: Rc<RefCell<P>>,
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
                // some backend errors are actually retryable
                // TODO: maybe retry or return a different default answer
                self.set_answer(DefaultAnswer::Answer503 {
                    message: backend_error.to_string(),
                });
                BackendConnectionError::Backend(backend_error)
            })?;

        if frontend_should_stick {
            // update sticky name in case it changed I guess?
            self.context.sticky_name = self.listener.borrow().sticky_name().to_string();

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

    fn get_backend_for_sticky_session<P: L7Proxy>(
        &self,
        frontend_should_stick: bool,
        sticky_session: Option<&str>,
        cluster_id: &str,
        proxy: Rc<RefCell<P>>,
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

    fn connect_to_backend<P: L7Proxy>(
        &mut self,
        session_rc: Rc<RefCell<dyn ProxySession>>,
        proxy: Rc<RefCell<P>>,
        metrics: &mut SessionMetrics,
    ) -> Result<BackendConnectAction, BackendConnectionError> {
        let old_cluster_id = self.context.cluster_id.clone();
        let old_backend_token = self.backend_token;

        self.check_circuit_breaker()?;

        let cluster_id = self
            .cluster_id_from_request(proxy.clone())
            .map_err(BackendConnectionError::RetrieveClusterError)?;

        trace!(
            "{} Connect_to_backend: {:?} {:?} {:?}",
            log_context!(self),
            self.context.cluster_id,
            cluster_id,
            self.backend_connection_status
        );
        // check if we can reuse the backend connection
        if (self.context.cluster_id.as_ref()) == Some(&cluster_id)
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

        self.context.cluster_id = Some(cluster_id.clone());

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
                "{} Error setting nodelay on backend socket({:?}): {:?}",
                log_context!(self),
                socket,
                e
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
                    error!(
                        "{} Error registering back socket({:?}): {:?}",
                        log_context!(self),
                        socket,
                        e
                    );
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
                    error!(
                        "{} Error registering back socket({:?}): {:?}",
                        log_context!(self),
                        socket,
                        e
                    );
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
                self.context.cluster_id.as_deref(),
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
                        self.context.cluster_id.as_deref(),
                        metrics.backend_id.as_deref()
                    );

                    info!(
                        "backend server {} at {} is up",
                        backend.backend_id, backend.address
                    );

                    push_event(Event {
                        kind: EventKind::BackendUp as i32,
                        backend_id: Some(backend.backend_id.to_owned()),
                        address: Some(backend.address.into()),
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
                self.context.cluster_id.as_deref(),
                metrics.backend_id.as_deref()
            );

            if !already_unavailable && backend.retry_policy.is_down() {
                error!(
                    "{} backend server {} at {} is down",
                    log_context!(self),
                    backend.backend_id,
                    backend.address
                );

                incr!(
                    "backend.down",
                    self.context.cluster_id.as_deref(),
                    metrics.backend_id.as_deref()
                );

                push_event(Event {
                    kind: EventKind::BackendDown as i32,
                    backend_id: Some(backend.backend_id.to_owned()),
                    address: Some(backend.address.into()),
                    cluster_id: None,
                });
            }
        }
    }

    pub fn backend_hup(&mut self, metrics: &mut SessionMetrics) -> StateResult {
        let response_stream = match &mut self.response_stream {
            ResponseStream::BackendAnswer(response_stream) => response_stream,
            _ => return StateResult::CloseBackend,
        };

        // there might still data we can read on the socket
        if self.backend_readiness.event.is_readable()
            && self.backend_readiness.interest.is_readable()
        {
            return StateResult::Continue;
        }

        // the backend finished to answer we can close
        if response_stream.is_terminated() {
            return StateResult::CloseBackend;
        }
        match (
            self.request_stream.is_initial(),
            response_stream.is_initial(),
        ) {
            // backend stopped before response is finished,
            // or maybe it was malformed in the first place (no Content-Length)
            (_, false) => {
                error!(
                    "{} Backend closed before session is over",
                    log_context!(self),
                );

                trace!("{} Backend hang-up, setting the parsing phase of the response stream to terminated, this also takes care of responses that lack length information.", log_context!(self));

                response_stream.parsing_phase = kawa::ParsingPhase::Terminated;

                // check if there is anything left to write
                if response_stream.is_completed() {
                    // we have to close the session now, because writable would short-cut
                    self.log_request_success(metrics);
                    StateResult::CloseSession
                } else {
                    // writable() will be called again and finish the session properly
                    StateResult::CloseBackend
                }
            }
            // probably backend hup between keep alive request, change backend
            (true, true) => {
                trace!(
                    "{} Backend hanged up in between requests",
                    log_context!(self)
                );
                StateResult::CloseBackend
            }
            // the frontend already transmitted data so we can't redirect
            (false, true) => {
                error!(
                    "{}  Frontend transmitted data but the back closed",
                    log_context!(self)
                );

                self.set_answer(DefaultAnswer::Answer503 {
                    message: "Backend closed after consuming part of the request".into(),
                });

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
    fn ready_inner<P: L7Proxy>(
        &mut self,
        session: Rc<RefCell<dyn crate::ProxySession>>,
        proxy: Rc<RefCell<P>>,
        metrics: &mut SessionMetrics,
    ) -> SessionResult {
        let mut counter = 0;

        if self.backend_connection_status.is_connecting()
            && !self.backend_readiness.event.is_empty()
        {
            if self.backend_readiness.event.is_hup() && !self.test_backend_socket() {
                //retry connecting the backend
                error!(
                    "{} Error connecting to backend, trying again, attempt {}",
                    log_context!(self),
                    self.connection_attempts
                );

                self.connection_attempts += 1;
                self.fail_backend_connection(metrics);

                self.backend_connection_status =
                    BackendConnectionStatus::Connecting(Instant::now());

                // trigger a backend reconnection
                self.close_backend(proxy.clone(), metrics);

                let connection_result =
                    self.connect_to_backend(session.clone(), proxy.clone(), metrics);
                if let Err(err) = &connection_result {
                    error!(
                        "{} Error connecting to backend: {}",
                        log_context!(self),
                        err
                    );
                }

                if let Some(session_result) = handle_connection_result(connection_result) {
                    return session_result;
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
            if !self.request_stream.is_initial() {
                self.log_request_error(metrics, "Client disconnected abruptly");
            }
            return SessionResult::Close;
        }

        while counter < MAX_LOOP_ITERATIONS {
            let frontend_interest = self.frontend_readiness.filter_interest();
            let backend_interest = self.backend_readiness.filter_interest();

            trace!(
                "{} Frontend interest({:?}) and backend interest({:?})",
                log_context!(self),
                frontend_interest,
                backend_interest,
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
                    "{} frontend_readable: {:?}",
                    log_context!(self),
                    state_result
                );

                match state_result {
                    StateResult::Continue => {}
                    StateResult::ConnectBackend => {
                        let connection_result =
                            self.connect_to_backend(session.clone(), proxy.clone(), metrics);
                        if let Err(err) = &connection_result {
                            error!(
                                "{} Error connecting to backend: {}",
                                log_context!(self),
                                err
                            );
                        }

                        if let Some(session_result) = handle_connection_result(connection_result) {
                            return session_result;
                        }
                    }
                    StateResult::CloseBackend => unreachable!(),
                    StateResult::CloseSession => return SessionResult::Close,
                    StateResult::Upgrade => return SessionResult::Upgrade,
                }
            }

            if backend_interest.is_writable() {
                let session_result = self.backend_writable(metrics);
                trace!(
                    "{} backend_writable: {:?}",
                    log_context!(self),
                    session_result
                );
                if session_result != SessionResult::Continue {
                    return session_result;
                }
            }

            if backend_interest.is_readable() {
                let session_result = self.backend_readable(metrics);
                trace!(
                    "{} backend_readable: {:?}",
                    log_context!(self),
                    session_result
                );
                if session_result != SessionResult::Continue {
                    return session_result;
                }
            }

            if frontend_interest.is_writable() {
                let state_result = self.writable(metrics);
                trace!(
                    "{} frontend_writable: {:?}",
                    log_context!(self),
                    state_result
                );
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
                    "{} frontend socket error, disconnecting",
                    log_context!(self)
                );

                return SessionResult::Close;
            }

            if backend_interest.is_hup() || backend_interest.is_error() {
                let state_result = self.backend_hup(metrics);

                trace!("{} backend_hup: {:?}", log_context!(self), state_result);
                match state_result {
                    StateResult::Continue => {}
                    StateResult::CloseBackend => self.close_backend(proxy.clone(), metrics),
                    StateResult::CloseSession => return SessionResult::Close,
                    StateResult::ConnectBackend | StateResult::Upgrade => unreachable!(),
                }
            }

            counter += 1;
        }

        if counter >= MAX_LOOP_ITERATIONS {
            error!(
                "{}\tHandling session went through {} iterations, there's a probable infinite loop bug, closing the connection",
                log_context!(self), MAX_LOOP_ITERATIONS
            );

            incr!("http.infinite_loop.error");
            self.print_state(self.protocol_string());

            return SessionResult::Close;
        }

        SessionResult::Continue
    }

    pub fn timeout_status(&self) -> TimeoutStatus {
        if self.request_stream.is_main_phase() {
            match &self.response_stream {
                ResponseStream::BackendAnswer(kawa) if kawa.is_initial() => {
                    TimeoutStatus::WaitingForResponse
                }
                _ => TimeoutStatus::Response,
            }
        } else if self.keepalive_count > 0 {
            TimeoutStatus::WaitingForNewRequest
        } else {
            TimeoutStatus::Request
        }
    }
}

impl<Front: SocketHandler, L: ListenerHandler + L7ListenerHandler> SessionState for Http<Front, L> {
    fn ready<P: L7Proxy>(
        &mut self,
        session: Rc<RefCell<dyn crate::ProxySession>>,
        proxy: Rc<RefCell<P>>,
        metrics: &mut SessionMetrics,
    ) -> SessionResult {
        let session_result = self.ready_inner(session, proxy, metrics);
        if session_result == SessionResult::Upgrade {
            let response_storage = match &mut self.response_stream {
                ResponseStream::BackendAnswer(response_stream) => &mut response_stream.storage,
                _ => return SessionResult::Close,
            };

            // sync the underlying Checkout buffers, if they contain remaining data
            // it will be processed once upgraded to websocket
            self.request_stream.storage.buffer.sync(
                self.request_stream.storage.end,
                self.request_stream.storage.head,
            );
            response_storage
                .buffer
                .sync(response_storage.end, response_storage.head);
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

    fn close<P: L7Proxy>(&mut self, proxy: Rc<RefCell<P>>, metrics: &mut SessionMetrics) {
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
                // we do not have a complete answer
                TimeoutStatus::Request => {
                    self.set_answer(DefaultAnswer::Answer408 {
                        duration: self.container_frontend_timeout.to_string(),
                    });
                    self.writable(metrics)
                }
                // we have a complete answer but the response did not start
                TimeoutStatus::WaitingForResponse => {
                    // this case is ambiguous, as it is the frontend timeout that triggers while we were waiting for response
                    // the timeout responsibility should have switched before
                    self.set_answer(DefaultAnswer::Answer504 {
                        duration: self.container_backend_timeout.to_string(),
                    });
                    self.writable(metrics)
                }
                // we have a complete answer and the start of a response, but the request was not tagged as terminated
                // for now we place responsibility of timeout on the backend in those cases, so we ignore this
                TimeoutStatus::Response => StateResult::Continue,
                // timeout in keep-alive, simply close the connection
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
                    self.set_answer(DefaultAnswer::Answer504 {
                        duration: self.container_backend_timeout.to_string(),
                    });
                    self.writable(metrics)
                }
                TimeoutStatus::WaitingForResponse => {
                    self.set_answer(DefaultAnswer::Answer504 {
                        duration: self.container_backend_timeout.to_string(),
                    });
                    self.writable(metrics)
                }
                TimeoutStatus::Response => {
                    error!(
                        "backend {:?} timeout while receiving response (cluster {:?})",
                        self.context.backend_id, self.context.cluster_id
                    );
                    StateResult::CloseSession
                }
                // in keep-alive, we place responsibility of timeout on the frontend, so we ignore this
                TimeoutStatus::WaitingForNewRequest => StateResult::Continue,
            };
        }

        error!("{} Got timeout for an invalid token", log_context!(self));
        StateResult::CloseSession
    }

    fn cancel_timeouts(&mut self) {
        self.container_backend_timeout.cancel();
        self.container_frontend_timeout.cancel();
    }

    fn print_state(&self, context: &str) {
        error!(
            "\
{} {} Session(Kawa)
\tFrontend:
\t\ttoken: {:?}\treadiness: {:?}\tstate: {:?}
\tBackend:
\t\ttoken: {:?}\treadiness: {:?}",
            log_context!(self),
            context,
            self.frontend_token,
            self.frontend_readiness,
            self.request_stream.parsing_phase,
            self.backend_token,
            self.backend_readiness,
            // self.response_stream.parsing_phase
        );
    }

    fn shutting_down(&mut self) -> SessionIsToBeClosed {
        if self.request_stream.is_initial() && self.request_stream.storage.is_empty()
        // && self.response_stream.storage.is_empty()
        {
            true
        } else {
            self.context.closing = true;
            false
        }
    }
}

fn handle_connection_result(
    connection_result: Result<BackendConnectAction, BackendConnectionError>,
) -> Option<SessionResult> {
    match connection_result {
        // reuse connection or send a default answer, we can continue
        Ok(BackendConnectAction::Reuse) => None,
        Ok(BackendConnectAction::New) | Ok(BackendConnectAction::Replace) => {
            // we must wait for an event
            Some(SessionResult::Continue)
        }
        Err(_) => {
            // All BackendConnectionError already set a default answer
            // the session must continue to serve it
            // - NotFound: not used for http (only tcp)
            // - RetrieveClusterError: 301/400/401/404,
            // - MaxConnectionRetries: 503,
            // - Backend: 503,
            // - MaxSessionsMemory: not checked in connect_to_backend (TODO: check it?)
            None
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
