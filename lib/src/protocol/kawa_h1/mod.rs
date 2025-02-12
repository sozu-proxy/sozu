pub mod answers;
pub mod diagnostics;
pub mod editor;
pub mod parser;

use std::{
    cell::RefCell,
    io::ErrorKind,
    mem,
    net::{Shutdown, SocketAddr},
    rc::{Rc, Weak},
    str::from_utf8_unchecked,
    time::{Duration, Instant},
};

use editor::{apply_header_edits, HttpRoute};
use mio::{net::TcpStream, Interest, Token};
use parser::hostname_and_port;
use rusty_ulid::Ulid;
use sozu_command::{
    config::MAX_LOOP_ITERATIONS,
    logging::EndpointRecord,
    proto::command::{Event, EventKind, ListenerType, RedirectPolicy, RedirectScheme},
};

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
        pipe::WebSocketContext,
        SessionState,
    },
    retry::RetryPolicy,
    router::RouteResult,
    server::{push_event, CONN_RETRIES},
    socket::{stats::socket_rtt, SocketHandler, SocketResult, TransportProtocol},
    sozu_command::{logging::LogContext, ready::Ready},
    timer::TimeoutContainer,
    AcceptError, BackendConnectAction, BackendConnectionError, FrontendFromRequestError,
    L7ListenerHandler, L7Proxy, Protocol, ProxySession, Readiness, RetrieveClusterError,
    SessionIsToBeClosed, SessionMetrics, SessionResult, StateResult,
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
            $self.origin.as_ref().map(|origin| origin.token.0.to_string()).unwrap_or_else(|| "<none>".to_string()),
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
    AnswerCustom {
        name: String,
        location: String,
    },
    Answer301 {
        location: String,
    },
    Answer400 {
        message: String,
        phase: kawa::ParsingPhaseMarker,
        successfully_parsed: String,
        partially_parsed: String,
        invalid: String,
    },
    Answer401 {
        www_authenticate: Option<String>,
    },
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
        successfully_parsed: String,
        partially_parsed: String,
        invalid: String,
    },
    Answer503 {
        message: String,
    },
    Answer504 {
        duration: String,
    },
    Answer507 {
        message: String,
        phase: kawa::ParsingPhaseMarker,
        capacity: usize,
    },
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
    DefaultAnswerKA(u16, DefaultAnswerStream, GenericHttpStream),
    Swaping,
}

#[derive(Debug)]
pub struct Origin {
    pub cluster_id: String,
    pub backend_id: String,
    pub backend: Rc<RefCell<Backend>>,
    pub token: Token,
    connected: bool,
    pub socket: TcpStream,
}

impl Origin {
    fn is_connected_to(&self, cluster_id: &str) -> bool {
        self.cluster_id == cluster_id && self.connected
    }
}

/// Http will be contained in State which itself is contained by Session
pub struct Http<Front: SocketHandler, L: L7ListenerHandler> {
    answers: Rc<RefCell<answers::HttpAnswers>>,
    /// The last origin server we tried to communicate with.
    /// It may be connected or connecting. It may be the server serving the current response,
    /// or a server kept alive while a default answer is sent. Its cluster_id might differ from context.cluster_id
    pub origin: Option<Origin>,
    backend_stop: Option<Instant>,
    pub backend_readiness: Readiness,
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

impl<Front: SocketHandler, L: L7ListenerHandler> Http<Front, L> {
    /// Instantiate a new HTTP SessionState with:
    ///
    /// - frontend_interest: READABLE | HUP | ERROR
    /// - frontend_event: EMPTY
    /// - backend_interest: EMPTY
    /// - backend_event: EMPTY
    ///
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
            origin: None,
            backend_stop: None,
            backend_readiness: Readiness::new(),
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
                backend_id: None,
                cluster_id: None,

                closing: false,
                keep_alive_backend: true,
                keep_alive_frontend: true,
                protocol,
                public_address,
                session_address,
                sticky_name,
                sticky_session: None,
                sticky_session_found: None,
                authorization_found: None,
                last_header: None,
                headers_response: Rc::new([]),
                tags: None,

                route: HttpRoute {
                    method: None,
                    authority: None,
                    path: None,
                },
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

        self.context.id = Ulid::generate();
        self.context.reset();

        self.request_stream.clear();
        response_stream.clear();
        self.keepalive_count += 1;
        gauge_add!("http.active_requests", -1);

        if let Some(origin) = &self.origin {
            if origin.connected {
                let mut backend = origin.backend.borrow_mut();
                backend.active_requests = backend.active_requests.saturating_sub(1);
            }
        }

        // reset the front timeout and cancel the back timeout while we are
        // waiting for a new request
        self.container_backend_timeout.cancel();
        self.container_frontend_timeout
            .set_duration(self.configured_frontend_timeout);
        self.frontend_readiness.interest = Ready::READABLE | Ready::HUP | Ready::ERROR;
        self.backend_readiness.interest = Ready::HUP | Ready::ERROR;

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

        // We are resetting the offset of request and response stream buffers
        // We do have to keep cursor position on the request, if there is data
        // in the request stream to preserve http pipelining.
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
            _ => {
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
                let (message, successfully_parsed, partially_parsed, invalid) =
                    diagnostic_400_502(marker, kind, &self.request_stream);
                self.set_answer(DefaultAnswer::Answer400 {
                    message,
                    phase: marker,
                    successfully_parsed,
                    partially_parsed,
                    invalid,
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
            // do not shortcut, response might have been terminated without anything more to send
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
                self.log_request_success(metrics);
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
            ResponseStream::DefaultAnswer(_, response_stream)
            | ResponseStream::DefaultAnswerKA(_, response_stream, _) => response_stream,
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
            self.log_request_success(metrics);
            return match mem::replace(&mut self.response_stream, ResponseStream::Swaping) {
                ResponseStream::DefaultAnswerKA(_, _, kawa_back) => {
                    self.response_stream = ResponseStream::BackendAnswer(kawa_back);
                    metrics.reset();
                    self.reset();
                    return StateResult::Continue;
                }
                _ => StateResult::CloseSession,
            };
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

        let backend_socket = if let Some(origin) = &mut self.origin {
            &mut origin.socket
        } else {
            self.log_request_error(metrics, "backend socket not found, closing session");
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

        let backend_socket = if let Some(origin) = &mut self.origin {
            &mut origin.socket
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
                // backend_socket.read_error();
                incr!("tcp.read.error");
                self.log_request_error(
                    metrics,
                    &format!(
                        "back socket {socket_state:?}, closing session. Readiness: {:?} -> {:?}, read {size} bytes",
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
                let (message, successfully_parsed, partially_parsed, invalid) =
                    diagnostic_400_502(marker, kind, response_stream);
                self.set_answer(DefaultAnswer::Answer502 {
                    message,
                    phase: marker,
                    successfully_parsed,
                    partially_parsed,
                    invalid,
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

impl<Front: SocketHandler, L: L7ListenerHandler> Http<Front, L> {
    fn log_endpoint(&self) -> EndpointRecord {
        EndpointRecord::Http {
            method: self.context.route.method.as_deref(),
            authority: self.context.route.authority.as_deref(),
            path: self.context.route.path.as_deref(),
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
        self.origin
            .as_ref()
            .map(|origin| origin.backend.borrow().address)
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
    pub fn websocket_context(&self) -> WebSocketContext {
        WebSocketContext::Http {
            method: self.context.route.method.clone(),
            authority: self.context.route.authority.clone(),
            path: self.context.route.path.clone(),
            reason: self.context.reason.clone(),
            status: self.context.status,
        }
    }

    pub fn log_request(&self, metrics: &SessionMetrics, error: bool, message: Option<&str>) {
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
            tags: self.context.tags.as_deref(),
            client_rtt: socket_rtt(self.front_socket()),
            server_rtt: self.origin.as_ref().and_then(|origin| socket_rtt(&origin.socket)),
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
        match answer {
            DefaultAnswer::AnswerCustom { .. } => incr!(
                "http.custom_asnwers",
                self.context.cluster_id.as_deref(),
                self.context.backend_id.as_deref()
            ),
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
        }
        let (status, keep_alive, mut kawa) = self.answers.borrow().get(
            answer,
            self.context.id.to_string(),
            self.context.cluster_id.as_deref(),
            self.context.backend_id.as_deref(),
            self.get_route(),
        );
        if let ResponseStream::DefaultAnswer(old_status, ..) = self.response_stream {
            error!(
                "already set the default answer to {}, trying to set to {}",
                old_status, status
            );
        };
        kawa.prepare(&mut kawa::h1::BlockConverter);
        self.context.status = Some(status);
        self.context.reason = None;
        if keep_alive {
            match mem::replace(&mut self.response_stream, ResponseStream::Swaping) {
                ResponseStream::BackendAnswer(back_kawa) => {
                    self.response_stream = ResponseStream::DefaultAnswerKA(status, kawa, back_kawa);
                }
                _ => unreachable!(),
            }
        } else {
            self.response_stream = ResponseStream::DefaultAnswer(status, kawa);
        }
        self.frontend_readiness.interest = Ready::WRITABLE | Ready::HUP | Ready::ERROR;
        self.backend_readiness.interest = Ready::HUP | Ready::ERROR;
    }

    pub fn test_backend_socket(&self) -> bool {
        match &self.origin {
            Some(origin) => {
                let mut tmp = [0u8; 1];
                let res = origin.socket.peek(&mut tmp[..]);

                match res {
                    // if the socket is half open, it will report 0 bytes read (EOF)
                    Ok(0) => false,
                    Ok(_) => true,
                    Err(e) => matches!(e.kind(), ErrorKind::WouldBlock),
                }
            }
            None => false,
        }
    }

    pub fn is_valid_backend_socket(&self) -> bool {
        // if socket was last used in the last second, test it
        match &self.backend_stop {
            Some(stop_instant) => {
                stop_instant.elapsed() < Duration::from_secs(1) || self.test_backend_socket()
            }
            None => self.test_backend_socket(),
        }
    }

    pub fn set_backend_timeout(&mut self, token: Token, dur: Duration) {
        self.container_backend_timeout.set_duration(dur);
        self.container_backend_timeout.set(token);
    }

    pub fn front_socket(&self) -> &TcpStream {
        self.frontend_socket.socket_ref()
    }

    fn close_backend(
        &mut self,
        proxy: Rc<RefCell<dyn L7Proxy>>,
        reuse_entry: bool,
    ) -> Option<Token> {
        self.container_backend_timeout.cancel();

        let proxy = proxy.borrow();
        if let Some(mut origin) = self.origin.take() {
            debug!(
                "{}\tPROXY [{}->{}] CLOSE BACKEND {:?}",
                log_context!(self),
                self.frontend_token.0,
                format!("{}", origin.token.0),
                origin
            );

            if let Err(e) = proxy.deregister_socket(&mut origin.socket) {
                error!(
                    "{} Error deregistering back socket({:?}): {:?}",
                    log_context!(self),
                    origin.socket,
                    e
                );
            }
            if let Err(e) = origin.socket.shutdown(Shutdown::Both) {
                if e.kind() != ErrorKind::NotConnected {
                    error!(
                        "{} Error shutting down back socket({:?}): {:?}",
                        log_context!(self),
                        origin.socket,
                        e
                    );
                }
            }
            self.backend_readiness.event = Ready::EMPTY;
            if origin.connected {
                gauge_add!("backend.connections", -1);
                gauge_add!(
                    "connections_per_backend",
                    -1,
                    Some(&origin.cluster_id),
                    Some(&origin.backend_id)
                );
            }
            origin.backend.borrow_mut().dec_connections();

            if !reuse_entry {
                proxy.remove_session(origin.token);
            }
            Some(origin.token)
        } else {
            None
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

    fn check_backend_connection(&self, metrics: &mut SessionMetrics) -> bool {
        let is_valid_backend_socket = self.is_valid_backend_socket();

        if !is_valid_backend_socket {
            return false;
        }

        metrics.backend_start();
        if let Some(origin) = &self.origin {
            origin.backend.borrow_mut().active_requests += 1;
        }
        true
    }

    pub fn get_route(&self) -> String {
        if let Some(method) = &self.context.route.method {
            if let Some(authority) = &self.context.route.authority {
                if let Some(path) = &self.context.route.path {
                    return format!("{method} {authority}{path}");
                }
                return format!("{method} {authority}");
            }
            return format!("{method}");
        }
        String::new()
    }

    fn cluster_id_from_request(
        &mut self,
        proxy: Rc<RefCell<dyn L7Proxy>>,
    ) -> Result<String, RetrieveClusterError> {
        let (host, path, method) = match self.context.route.extract() {
            Ok(tuple) => tuple,
            Err(cluster_error) => {
                self.set_answer(DefaultAnswer::Answer400 {
                    message: "Could not extract the route after connection started, this should not happen.".into(),
                    phase: kawa::ParsingPhaseMarker::StatusLine,
                    successfully_parsed: "null".into(),
                    partially_parsed: "null".into(),
                    invalid: "null".into(),
                });
                return Err(cluster_error);
            }
        };

        let (host, port) = match hostname_and_port(host.as_bytes()) {
            Ok((b"", (hostname, port))) => (unsafe { from_utf8_unchecked(hostname) }, port),
            Ok(_) => {
                let host = host.to_owned();
                self.set_answer(DefaultAnswer::Answer400 {
                    message: "Invalid characters after hostname, this should not happen.".into(),
                    phase: kawa::ParsingPhaseMarker::StatusLine,
                    successfully_parsed: "null".into(),
                    partially_parsed: "null".into(),
                    invalid: "null".into(),
                });
                return Err(RetrieveClusterError::RetrieveFrontend(
                    FrontendFromRequestError::InvalidCharsAfterHost(host),
                ));
            }
            Err(parse_error) => {
                let host = host.to_owned();
                let error = parse_error.to_string();
                self.set_answer(DefaultAnswer::Answer400 {
                    message: "Could not parse port from hostname, this should not happen.".into(),
                    phase: kawa::ParsingPhaseMarker::StatusLine,
                    successfully_parsed: "null".into(),
                    partially_parsed: "null".into(),
                    invalid: "null".into(),
                });
                return Err(RetrieveClusterError::RetrieveFrontend(
                    FrontendFromRequestError::HostParse { host, error },
                ));
            }
        };

        let start = Instant::now();
        let route_result = self
            .listener
            .borrow()
            .frontend_from_request(host, path, method);

        let RouteResult {
            cluster_id,
            required_auth,
            redirect,
            redirect_scheme,
            redirect_template,
            rewritten_host,
            rewritten_path,
            rewritten_port,
            headers_request,
            headers_response,
            tags,
        } = match route_result {
            Ok(route) => route,
            Err(frontend_error) => {
                self.set_answer(DefaultAnswer::Answer404 {});
                return Err(RetrieveClusterError::RetrieveFrontend(frontend_error));
            }
        };
        self.context.cluster_id = cluster_id;
        self.context.headers_response = headers_response;
        self.context.tags = tags;
        let cluster_id = &self.context.cluster_id;

        if let Some(cluster_id) = cluster_id {
            time!(
                "frontend_matching_time",
                cluster_id,
                start.elapsed().as_millis()
            );
        }

        let body = if let Some(position) = self.context.last_header {
            self.request_stream.blocks.split_off(position)
        } else {
            unreachable!();
        };

        match &mut self.request_stream.detached.status_line {
            kawa::StatusLine::Request { authority, uri, .. } => {
                let buf = self.request_stream.storage.mut_buffer();
                if let Some(new) = &rewritten_host {
                    authority.modify(buf, new.as_bytes());
                    self.request_stream
                        .blocks
                        .push_back(kawa::Block::Header(kawa::Pair {
                            key: kawa::Store::Static(b"X-Forwarded-Host"),
                            val: kawa::Store::from_slice(host.as_bytes()),
                        }));
                }
                if let Some(new) = &rewritten_path {
                    uri.modify(buf, new.as_bytes());
                }
            }
            _ => unreachable!(),
        }

        let host = rewritten_host.as_deref().unwrap_or(host);
        let path = rewritten_path.as_deref().unwrap_or(path);
        let port = rewritten_port.map_or_else(
            || {
                port.map_or(String::new(), |port| {
                    format!(":{}", unsafe { from_utf8_unchecked(port) })
                })
            },
            |port| format!(":{port}"),
        );
        let is_https = matches!(proxy.borrow().kind(), ListenerType::Https);
        let proto = match (redirect_scheme, is_https) {
            (RedirectScheme::UseHttp, _) | (RedirectScheme::UseSame, false) => "http",
            (RedirectScheme::UseHttps, _) | (RedirectScheme::UseSame, true) => "https",
        };

        let (authorized, www_authenticate, https_redirect, https_redirect_port) =
            match (&cluster_id, redirect, &redirect_template, required_auth) {
                // unauthorized frontends
                (_, RedirectPolicy::Unauthorized, _, _) => (false, None, false, None),
                // forward frontends with no target (no cluster nor template)
                (None, RedirectPolicy::Forward, None, _) => (false, None, false, None),
                // clusterless frontend with auth (unsupported)
                (None, _, _, true) => (false, None, false, None),
                // clusterless frontends
                (None, _, _, false) => (true, None, false, None),
                // "attached" frontends
                (Some(cluster_id), _, _, _) => {
                    proxy.borrow().clusters().get(cluster_id).map_or(
                        (true, None, false, None), // cluster not found, consider authorized?
                        |cluster| {
                            let authorized =
                                match (required_auth, &self.context.authorization_found) {
                                    // auth not required
                                    (false, _) => true,
                                    // no auth found
                                    (true, None) => false,
                                    // validation
                                    (true, Some(hash)) => {
                                        println!("{hash:?}");
                                        cluster.authorized_hashes.contains(hash)
                                    }
                                };
                            (
                                authorized,
                                cluster.www_authenticate.clone(),
                                cluster.https_redirect,
                                cluster.https_redirect_port,
                            )
                        },
                    )
                }
            };

        match (cluster_id, redirect, redirect_template, authorized) {
            (_, RedirectPolicy::Permanent, _, true) => {
                let location = format!("{proto}://{host}{port}{path}");
                self.set_answer(DefaultAnswer::Answer301 { location });
                Err(RetrieveClusterError::Redirected)
            }
            (_, RedirectPolicy::Forward, Some(name), true) => {
                let location = format!("{proto}://{host}{port}{path}");
                self.set_answer(DefaultAnswer::AnswerCustom { name, location });
                Err(RetrieveClusterError::Redirected)
            }
            (Some(cluster_id), RedirectPolicy::Forward, None, true) => {
                if !is_https && https_redirect {
                    let port = rewritten_port
                        .or_else(|| https_redirect_port.map(|port| port as u16))
                        .map_or(String::new(), |port| format!(":{port}"));
                    let location = format!("https://{host}{port}{path}");
                    self.set_answer(DefaultAnswer::Answer301 { location });
                    return Err(RetrieveClusterError::Redirected);
                }
                apply_header_edits(&mut self.request_stream, headers_request);
                self.request_stream.blocks.extend(body);
                Ok(cluster_id.clone())
            }
            _ => {
                self.set_answer(DefaultAnswer::Answer401 { www_authenticate });
                Err(RetrieveClusterError::UnauthorizedRoute)
            }
        }
    }

    pub fn backend_from_request(
        &mut self,
        cluster_id: &str,
        frontend_should_stick: bool,
        proxy: Rc<RefCell<dyn L7Proxy>>,
    ) -> Result<(Rc<RefCell<Backend>>, TcpStream), BackendConnectionError> {
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
            self.context.sticky_name = self.listener.borrow().get_sticky_name().to_string();

            self.context.sticky_session = Some(
                backend
                    .borrow()
                    .sticky_id
                    .clone()
                    .unwrap_or_else(|| backend.borrow().backend_id.clone()),
            );
        }

        Ok((backend, conn))
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
        self.check_circuit_breaker()?;

        let cluster_id = if self.connection_attempts == 0 {
            // cluster_id is determined from the triplet (method, authority, path)
            // which doesn't ever change for a request
            // WARNING: cluster_id_from_request is NOT idempotent, but connect_to_backend SHOULD be
            self.cluster_id_from_request(proxy.clone())
                .map_err(BackendConnectionError::RetrieveClusterError)?
        } else if let Some(cluster_id) = self.context.cluster_id.take() {
            cluster_id
        } else {
            unreachable!();
        };

        // update response cluster producer
        self.context.cluster_id = Some(cluster_id.clone());

        trace!(
            "{} Connect_to_backend: {:?}",
            log_context!(self),
            cluster_id,
        );

        // check if we can reuse the backend connection
        if let Some(origin) = &self.origin {
            if origin.is_connected_to(&cluster_id) {
                let has_backend = proxy
                    .borrow()
                    .backends()
                    .borrow()
                    .has_backend(&cluster_id, &origin.backend.borrow());

                if has_backend && self.check_backend_connection(metrics) {
                    // update response backend producer
                    self.context.backend_id = Some(origin.backend.borrow().backend_id.clone());
                    return Ok(BackendConnectAction::Reuse);
                }
            }
        }

        // close old backend (if it exists), replace with a connection to another cluster
        let reusable_token = self.close_backend(proxy.clone(), true);

        let frontend_should_stick = proxy
            .borrow()
            .clusters()
            .get(&cluster_id)
            .map(|cluster| cluster.sticky_session)
            .unwrap_or(false);

        let (backend, mut socket) =
            self.backend_from_request(&cluster_id, frontend_should_stick, proxy.clone())?;
        metrics.backend_start();
        if let Err(e) = socket.set_nodelay(true) {
            error!(
                "{} Error setting nodelay on backend socket({:?}): {:?}",
                log_context!(self),
                socket,
                e
            );
        }

        self.backend_readiness.interest = Ready::WRITABLE | Ready::HUP | Ready::ERROR;
        let backend_id = backend.borrow().backend_id.clone();
        // update response backend producer
        self.context.backend_id = Some(backend_id.clone());

        let (token, action) = if let Some(token) = reusable_token {
            (token, BackendConnectAction::Replace)
        } else {
            (
                proxy.borrow().add_session(session_rc),
                BackendConnectAction::New,
            )
        };

        if let Err(e) = proxy.borrow().register_socket(
            &mut socket,
            token,
            Interest::READABLE | Interest::WRITABLE,
        ) {
            error!(
                "{} Error registering backend socket({:?}): {:?}",
                log_context!(self),
                socket,
                e
            );
        }

        self.origin = Some(Origin {
            cluster_id,
            backend_id,
            backend,
            token,
            connected: false,
            socket,
        });
        self.set_backend_timeout(token, self.configured_connect_timeout);

        Ok(action)
    }

    fn set_backend_connected(&mut self, metrics: &mut SessionMetrics) {
        if let Some(Origin {
            connected: connected @ false,
            token,
            backend,
            backend_id,
            cluster_id,
            ..
        }) = &mut self.origin
        {
            *connected = true;

            gauge_add!("backend.connections", 1);
            gauge_add!(
                "connections_per_backend",
                1,
                Some(cluster_id),
                Some(backend_id)
            );

            {
                let mut backend = backend.borrow_mut();
                if backend.retry_policy.is_down() {
                    incr!("backend.up", Some(cluster_id), Some(backend_id));
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
                if let Some(time) = metrics.backend_connection_time() {
                    backend.set_connection_time(time);
                }
                //successful connection, reset failure counter
                backend.failures = 0;
                backend.active_requests += 1;
                backend.retry_policy.succeed();
            }

            // the back timeout was of connect_timeout duration before,
            // now that we're connected, move to backend_timeout duration
            let token = *token;
            self.set_backend_timeout(token, self.configured_backend_timeout);
            // if we are not waiting for the backend response, its timeout is concelled
            // it should be set when the request has been entirely transmitted
            if !self.backend_readiness.interest.is_readable() {
                self.container_backend_timeout.cancel();
            }
        }
    }

    fn fail_backend_connection(&mut self) {
        if let Some(Origin {
            connected: false,
            backend,
            backend_id,
            cluster_id,
            ..
        }) = &self.origin
        {
            let mut backend = backend.borrow_mut();
            backend.failures += 1;

            let already_unavailable = backend.retry_policy.is_down();
            backend.retry_policy.fail();
            incr!(
                "backend.connections.error",
                Some(cluster_id),
                Some(backend_id)
            );

            if !already_unavailable && backend.retry_policy.is_down() {
                error!(
                    "{} backend server {} at {} is down",
                    log_context!(self),
                    backend.backend_id,
                    backend.address
                );

                incr!("backend.down", Some(cluster_id), Some(backend_id));

                push_event(Event {
                    kind: EventKind::BackendDown as i32,
                    backend_id: Some(backend.backend_id.to_owned()),
                    address: Some(backend.address.into()),
                    cluster_id: None,
                });
            }
        }
    }

    pub fn backend_hup(&mut self, _metrics: &mut SessionMetrics) -> StateResult {
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

                // writable() will be called again and finish the session properly
                // for this reason, writable must not short cut
                self.frontend_readiness.interest.insert(Ready::WRITABLE);
                StateResult::Continue
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
    fn ready_inner(
        &mut self,
        session: Rc<RefCell<dyn crate::ProxySession>>,
        proxy: Rc<RefCell<dyn L7Proxy>>,
        metrics: &mut SessionMetrics,
    ) -> SessionResult {
        let mut counter = 0;

        if self
            .origin
            .as_ref()
            .map(|origin| !origin.connected)
            .unwrap_or(false)
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
                self.fail_backend_connection();

                // trigger a backend reconnection
                self.close_backend(proxy.clone(), false);

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
                self.set_backend_connected(metrics);
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
                    StateResult::CloseBackend => {
                        self.close_backend(proxy.clone(), false);
                    }
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
                    StateResult::CloseBackend => {
                        self.close_backend(proxy.clone(), false);
                    }
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

impl<Front: SocketHandler, L: L7ListenerHandler> SessionState for Http<Front, L> {
    fn ready(
        &mut self,
        session: Rc<RefCell<dyn crate::ProxySession>>,
        proxy: Rc<RefCell<dyn L7Proxy>>,
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
        } else if self
            .origin
            .as_ref()
            .map(|origin| origin.token == token)
            .unwrap_or(false)
        {
            self.backend_readiness.event |= events;
        }
    }

    fn close(&mut self, proxy: Rc<RefCell<dyn L7Proxy>>, _metrics: &mut SessionMetrics) {
        //if the state was initial, the connection was already reset
        if !self.request_stream.is_initial() {
            gauge_add!("http.active_requests", -1);

            if let Some(origin) = &self.origin {
                if origin.connected {
                    let mut backend = origin.backend.borrow_mut();
                    backend.active_requests = backend.active_requests.saturating_sub(1);
                }
            }
        }

        self.close_backend(proxy, false);
        self.frontend_socket.socket_close();
        let _ = self.frontend_socket.socket_write_vectored(&[]);
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

        if self
            .origin
            .as_ref()
            .map(|origin| origin.token == token)
            .unwrap_or(false)
        {
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
        let kawa_back = match &self.response_stream {
            ResponseStream::BackendAnswer(kawa) => format!("{:?}", kawa.parsing_phase),
            ResponseStream::DefaultAnswer(status, ..)
            | ResponseStream::DefaultAnswerKA(status, ..) => format!("DefaulAnswer({status})"),
            ResponseStream::Swaping => unreachable!(),
        };
        error!(
            "{} {} kawa_front={:?}, kawa_back={}",
            log_context!(self),
            context,
            self.request_stream.parsing_phase,
            kawa_back
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
