use std::{cell::RefCell, io::ErrorKind, net::SocketAddr, rc::Rc};

use mio::{Token, net::TcpStream};
use rustls::ServerConnection;
use rusty_ulid::Ulid;
use sozu_command::{config::MAX_LOOP_ITERATIONS, logging::LogContext};

use crate::{
    Readiness, Ready, SessionMetrics, SessionResult, StateResult, protocol::SessionState,
    timer::TimeoutContainer,
};

/// This macro is defined uniquely in this module to help the tracking of tls
/// issues inside Sōzu
macro_rules! log_context {
    ($self:expr) => {
        format!(
            "RUSTLS\t{}\tSession(sni={:?}, source={:?}, frontend={}, readiness={})\t >>>",
            $self.log_context(),
            $self
                .session
                .server_name()
                .map(|addr| addr.to_string())
                .unwrap_or_else(|| "<none>".to_string()),
            $self
                .peer_address
                .map(|addr| addr.to_string())
                .unwrap_or_else(|| "<none>".to_string()),
            $self.frontend_token.0,
            $self.frontend_readiness
        )
    };
}

pub enum TlsState {
    Initial,
    Handshake,
    Established,
    Error,
}

pub struct TlsHandshake {
    pub container_frontend_timeout: TimeoutContainer,
    pub frontend_readiness: Readiness,
    frontend_token: Token,
    pub peer_address: Option<SocketAddr>,
    pub request_id: Ulid,
    pub session: ServerConnection,
    pub stream: TcpStream,
}

impl TlsHandshake {
    /// Instantiate a new TlsHandshake SessionState with:
    ///
    /// - frontend_interest: READABLE | HUP | ERROR
    /// - frontend_event: EMPTY
    ///
    /// Remember to set the events from the previous State!
    pub fn new(
        container_frontend_timeout: TimeoutContainer,
        session: ServerConnection,
        stream: TcpStream,
        frontend_token: Token,
        request_id: Ulid,
        peer_address: Option<SocketAddr>,
    ) -> TlsHandshake {
        TlsHandshake {
            container_frontend_timeout,
            frontend_readiness: Readiness {
                interest: Ready::READABLE | Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            frontend_token,
            peer_address,
            request_id,
            session,
            stream,
        }
    }

    pub fn readable(&mut self) -> SessionResult {
        let mut can_read = true;

        loop {
            let mut can_work = false;

            if self.session.wants_read() && can_read {
                can_work = true;

                match self.session.read_tls(&mut self.stream) {
                    Ok(0) => {
                        error!("{} Connection closed during handshake", log_context!(self));
                        return SessionResult::Close;
                    }
                    Ok(_) => {}
                    Err(e) => match e.kind() {
                        ErrorKind::WouldBlock => {
                            self.frontend_readiness.event.remove(Ready::READABLE);
                            can_read = false
                        }
                        _ => {
                            error!(
                                "{} Could not perform handshake: {:?}",
                                log_context!(self),
                                e
                            );
                            return SessionResult::Close;
                        }
                    },
                }

                if let Err(e) = self.session.process_new_packets() {
                    error!(
                        "{} Could not perform handshake: {:?}",
                        log_context!(self),
                        e
                    );
                    return SessionResult::Close;
                }
            }

            if !can_work {
                break;
            }
        }

        if !self.session.wants_read() {
            self.frontend_readiness.interest.remove(Ready::READABLE);
        }

        if self.session.wants_write() {
            self.frontend_readiness.interest.insert(Ready::WRITABLE);
        }

        if self.session.is_handshaking() {
            SessionResult::Continue
        } else {
            // handshake might be finished, but we still have something to send
            if self.session.wants_write() {
                SessionResult::Continue
            } else {
                self.frontend_readiness.interest.insert(Ready::READABLE);
                self.frontend_readiness.event.insert(Ready::READABLE);
                self.frontend_readiness.interest.insert(Ready::WRITABLE);
                SessionResult::Upgrade
            }
        }
    }

    pub fn writable(&mut self) -> SessionResult {
        let mut can_write = true;

        loop {
            let mut can_work = false;

            if self.session.wants_write() && can_write {
                can_work = true;

                match self.session.write_tls(&mut self.stream) {
                    Ok(_) => {}
                    Err(e) => match e.kind() {
                        ErrorKind::WouldBlock => {
                            self.frontend_readiness.event.remove(Ready::WRITABLE);
                            can_write = false
                        }
                        _ => {
                            error!(
                                "{} Could not perform handshake: {:?}",
                                log_context!(self),
                                e
                            );
                            return SessionResult::Close;
                        }
                    },
                }

                if let Err(e) = self.session.process_new_packets() {
                    error!(
                        "{} Could not perform handshake: {:?}",
                        log_context!(self),
                        e
                    );
                    return SessionResult::Close;
                }
            }

            if !can_work {
                break;
            }
        }

        if !self.session.wants_write() {
            self.frontend_readiness.interest.remove(Ready::WRITABLE);
        }

        if self.session.wants_read() {
            self.frontend_readiness.interest.insert(Ready::READABLE);
        }

        if self.session.is_handshaking() {
            SessionResult::Continue
        } else if self.session.wants_read() {
            self.frontend_readiness.interest.insert(Ready::READABLE);
            SessionResult::Upgrade
        } else {
            self.frontend_readiness.interest.insert(Ready::WRITABLE);
            self.frontend_readiness.interest.insert(Ready::READABLE);
            SessionResult::Upgrade
        }
    }

    pub fn log_context(&self) -> LogContext<'_> {
        LogContext {
            request_id: self.request_id,
            cluster_id: None,
            backend_id: None,
        }
    }

    pub fn front_socket(&self) -> &TcpStream {
        &self.stream
    }
}

impl SessionState for TlsHandshake {
    fn ready(
        &mut self,
        _session: Rc<RefCell<dyn crate::ProxySession>>,
        _proxy: Rc<RefCell<dyn crate::L7Proxy>>,
        _metrics: &mut SessionMetrics,
    ) -> SessionResult {
        let mut counter = 0;

        if self.frontend_readiness.event.is_hup() {
            return SessionResult::Close;
        }

        while counter < MAX_LOOP_ITERATIONS {
            let frontend_interest = self.frontend_readiness.filter_interest();

            trace!("{} Interest({:?})", log_context!(self), frontend_interest);
            if frontend_interest.is_empty() {
                break;
            }

            if frontend_interest.is_readable() {
                let protocol_result = self.readable();
                if protocol_result != SessionResult::Continue {
                    return protocol_result;
                }
            }

            if frontend_interest.is_writable() {
                let protocol_result = self.writable();
                if protocol_result != SessionResult::Continue {
                    return protocol_result;
                }
            }

            if frontend_interest.is_error() {
                error!("{} Front socket error, disconnecting", log_context!(self));
                self.frontend_readiness.interest = Ready::EMPTY;
                return SessionResult::Close;
            }

            counter += 1;
        }

        if counter >= MAX_LOOP_ITERATIONS {
            error!(
                "{}\tHandling session went through {} iterations, there's a probable infinite loop bug, closing the connection",
                log_context!(self),
                MAX_LOOP_ITERATIONS
            );

            incr!("http.infinite_loop.error");
            self.print_state("HTTPS");

            return SessionResult::Close;
        }

        SessionResult::Continue
    }

    fn update_readiness(&mut self, token: Token, events: Ready) {
        if self.frontend_token == token {
            self.frontend_readiness.event |= events;
        }
    }

    fn timeout(&mut self, token: Token, _metrics: &mut SessionMetrics) -> StateResult {
        // relevant timeout is still stored in the Session as front_timeout.
        if self.frontend_token == token {
            self.container_frontend_timeout.triggered();
            return StateResult::CloseSession;
        }

        error!(
            "{}, Expect state: got timeout for an invalid token: {:?}",
            log_context!(self),
            token
        );
        StateResult::CloseSession
    }

    fn cancel_timeouts(&mut self) {
        self.container_frontend_timeout.cancel();
    }

    fn print_state(&self, context: &str) {
        error!(
            "{} Session(Handshake)\n\tFrontend:\n\t\ttoken: {:?}\treadiness: {:?}",
            context, self.frontend_token, self.frontend_readiness
        );
    }
}
