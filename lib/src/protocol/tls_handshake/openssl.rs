use std::{cell::RefCell, net::SocketAddr, rc::Rc};

use mio::{net::*, Token};
use openssl::ssl::{HandshakeError, MidHandshakeSslStream, Ssl, SslStream};
use rusty_ulid::Ulid;

use crate::{
    logs::LogContext, protocol::SessionState, sozu_command::ready::Ready, timer::TimeoutContainer,
    Readiness, SessionMetrics, SessionResult, StateResult,
};

pub enum TlsState {
    Initial,
    Handshake,
    Established,
    Error(HandshakeError<TcpStream>),
}

pub struct TlsHandshake {
    pub container_frontend_timeout: TimeoutContainer,
    pub frontend_readiness: Readiness,
    pub frontend_token: Token,
    pub front: Option<TcpStream>,
    pub request_id: Ulid,
    pub ssl: Option<Ssl>,
    pub stream: Option<SslStream<TcpStream>>,
    mid: Option<MidHandshakeSslStream<TcpStream>>,
    state: TlsState,
    address: Option<SocketAddr>,
}

impl TlsHandshake {
    pub fn new(
        container_frontend_timeout: TimeoutContainer,
        ssl: Ssl,
        sock: TcpStream,
        frontend_token: Token,
        request_id: Ulid,
        address: Option<SocketAddr>,
    ) -> TlsHandshake {
        TlsHandshake {
            front: Some(sock),
            ssl: Some(ssl),
            mid: None,
            stream: None,
            state: TlsState::Initial,
            frontend_readiness: Readiness {
                interest: Ready::READABLE | Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            request_id,
            address,
            container_frontend_timeout,
            frontend_token,
        }
    }

    pub fn readable(&mut self, _metrics: &mut SessionMetrics) -> SessionResult {
        match self.state {
            TlsState::Error(_) => SessionResult::Close,
            TlsState::Initial => {
                let ssl = self
                    .ssl
                    .take()
                    .expect("TlsHandshake should have a Ssl backend"); // do we really want to panic here?
                let sock = self
                    .front
                    .take()
                    .expect("TlsHandshake should have a front socket"); // do we really want to panic here?
                match ssl.accept(sock) {
                    Ok(stream) => {
                        self.stream = Some(stream);
                        self.state = TlsState::Established;
                        SessionResult::Upgrade
                    }
                    Err(HandshakeError::SetupFailure(e)) => {
                        error!(
                            "accept: handshake setup failed (client = {:?}): {:?}",
                            self.address, e
                        );
                        self.state = TlsState::Error(HandshakeError::SetupFailure(e));
                        SessionResult::Close
                    }
                    Err(HandshakeError::Failure(e)) => {
                        {
                            if let Some(error_stack) = e.error().ssl_error() {
                                let errors = error_stack.errors();
                                if errors.len() == 1 {
                                    if errors[0].code() == 0x140A1175 {
                                        incr!("openssl.inappropriate_fallback.error");
                                    } else if errors[0].code() == 0x1408A10B {
                                        incr!("openssl.wrong_version_number.error");
                                    } else if errors[0].code() == 0x140760FC {
                                        incr!("openssl.unknown_protocol.error");
                                    } else if errors[0].code() == 0x1407609C {
                                        //someone tried to connect in plain HTTP to a TLS server
                                        incr!("openssl.http_request.error");
                                    } else if errors[0].code() == 0x1422E0EA {
                                        // SNI error
                                        // self.log_request_error(metrics, &e);
                                    } else {
                                        error!(
                                            "accept: handshake failed (client = {:?}): {} ({:?})",
                                            self.address,
                                            e.error(),
                                            e
                                        );
                                    }
                                } else if errors.len() == 2
                                    && errors[0].code() == 0x1412E0E2
                                    && errors[1].code() == 0x1408A0E3
                                {
                                    incr!("openssl.sni.error");
                                } else {
                                    error!(
                                        "accept: handshake failed (client = {:?}): {} ({:?})",
                                        self.address,
                                        e.error(),
                                        e
                                    );
                                }
                            } else {
                                error!(
                                    "accept: handshake failed (client = {:?}): {} ({:?})",
                                    self.address,
                                    e.error(),
                                    e
                                );
                            }
                        }
                        self.state = TlsState::Error(HandshakeError::Failure(e));
                        SessionResult::Close
                    }
                    Err(HandshakeError::WouldBlock(mid)) => {
                        self.state = TlsState::Handshake;
                        self.mid = Some(mid);
                        self.frontend_readiness.event.remove(Ready::READABLE);
                        SessionResult::Continue
                    }
                }
            }
            TlsState::Handshake => {
                let mid = self
                    .mid
                    .take()
                    .expect("TlsHandshake should have a MidHandshakeSslStream backend"); // do we really want to crash here?
                match mid.handshake() {
                    Ok(stream) => {
                        self.stream = Some(stream);
                        self.state = TlsState::Established;
                        SessionResult::Upgrade
                    }
                    Err(HandshakeError::SetupFailure(e)) => {
                        debug!(
                            "mid handshake setup failed (client = {:?}): {:?}",
                            self.address, e
                        );
                        self.state = TlsState::Error(HandshakeError::SetupFailure(e));
                        SessionResult::Close
                    }
                    Err(HandshakeError::Failure(e)) => {
                        debug!(
                            "mid handshake failed (client = {:?}): {:?}",
                            self.address, e
                        );
                        self.state = TlsState::Error(HandshakeError::Failure(e));
                        SessionResult::Close
                    }
                    Err(HandshakeError::WouldBlock(new_mid)) => {
                        self.state = TlsState::Handshake;
                        self.mid = Some(new_mid);
                        self.frontend_readiness.event.remove(Ready::READABLE);
                        SessionResult::Continue
                    }
                }
            }
            TlsState::Established => SessionResult::Upgrade,
        }
    }

    pub fn log_context(&self) -> LogContext {
        LogContext {
            request_id: self.request_id,
            cluster_id: None,
            backend_id: None,
        }
    }

    pub fn front_socket(&self) -> &TcpStream {
        match self.state {
            TlsState::Initial => self.front.as_ref(),
            TlsState::Handshake => self.mid.as_ref().map(|mid| mid.get_ref()),
            TlsState::Established => self.stream.as_ref().map(|stream| stream.get_ref()),
            TlsState::Error(ref error) => match error {
                &HandshakeError::SetupFailure(_) => self
                    .front
                    .as_ref()
                    .or_else(|| self.mid.as_ref().map(|mid| mid.get_ref())),
                &HandshakeError::Failure(ref mid) | &HandshakeError::WouldBlock(ref mid) => {
                    Some(mid.get_ref())
                }
            },
        }
        .unwrap()
    }

    // pub fn socket_mut(&mut self) -> Option<&mut TcpStream> {
    //     match self.state {
    //         TlsState::Initial => self.front.as_mut(),
    //         TlsState::Handshake => self.mid.as_mut().map(|mid| mid.get_mut()),
    //         TlsState::Established => self.stream.as_mut().map(|stream| stream.get_mut()),
    //         TlsState::Error(HandshakeError::Failure(ref mut mid))
    //         | TlsState::Error(HandshakeError::WouldBlock(ref mut mid)) => Some(mid.get_mut()),
    //         TlsState::Error(HandshakeError::SetupFailure(_)) => self
    //             .front
    //             .as_mut()
    //             .or(self.mid.as_mut().map(|mid| mid.get_mut())),
    //     }
    // }

    // pub fn log_request_error(
    //     &mut self,
    //     metrics: &mut SessionMetrics,
    //     handshake: &MidHandshakeSslStream<TcpStream>,
    // ) {
    //     metrics.service_stop();

    //     let session = match self
    //         .address
    //         .or_else(|| handshake.get_ref().peer_addr().ok())
    //     {
    //         None => String::from("-"),
    //         Some(SocketAddr::V4(addr)) => format!("{}", addr),
    //         Some(SocketAddr::V6(addr)) => format!("{}", addr),
    //     };

    //     let backend = "-";

    //     let response_time = metrics.response_time();
    //     let service_time = metrics.service_time();

    //     let proto = handshake
    //         .ssl()
    //         .version2()
    //         .map(|version| match version {
    //             SslVersion::SSL3 => "HTTPS-SSL3",
    //             SslVersion::TLS1 => "HTTPS-TLS1.0",
    //             SslVersion::TLS1_1 => "HTTPS-TLS1.1",
    //             SslVersion::TLS1_2 => "HTTPS-TLS1.2",
    //             _ => "HTTPS-TLS1.3",
    //         })
    //         .unwrap_or("HTTPS");

    //     let message = handshake
    //         .ssl()
    //         .servername(NameType::HOST_NAME)
    //         .map(|s| format!("unknown server name: {}", s))
    //         .unwrap_or("no SNI".to_string());

    //     error_access!(
    //         "{} - -\t{} -> {}\t{} {} {} {}\t{} | {}",
    //         self.request_id,
    //         session,
    //         backend,
    //         LogDuration(response_time),
    //         LogDuration(service_time),
    //         metrics.bin,
    //         metrics.bout,
    //         proto,
    //         message
    //     );
    // }
}

impl SessionState for TlsHandshake {
    fn ready(
        &mut self,
        _session: Rc<RefCell<dyn crate::ProxySession>>,
        _proxy: Rc<RefCell<dyn crate::L7Proxy>>,
        metrics: &mut SessionMetrics,
    ) -> SessionResult {
        let mut counter = 0;
        let max_loop_iterations = 100000;

        if self.frontend_readiness.event.is_hup() {
            return SessionResult::Close;
        }

        while counter < max_loop_iterations {
            let frontend_interest = self.frontend_readiness.filter_interest();

            trace!(
                "PROXY\t{} {:?} {:?} -> None",
                self.log_context(),
                self.frontend_token,
                self.frontend_readiness
            );

            if frontend_interest.is_empty() {
                break;
            }

            if frontend_interest.is_readable() {
                // let start = std::time::Instant::now();
                let protocol_result = self.readable(metrics);
                // println!(
                //     "OPENSSL_HANDSHAKE {:?} ELAPSED: {:?}",
                //     self.frontend_token,
                //     start.elapsed()
                // );
                if protocol_result != SessionResult::Continue {
                    return protocol_result;
                }
            }

            if frontend_interest.is_error() {
                error!(
                    "PROXY session {:?} front error, disconnecting",
                    self.frontend_token
                );
                self.frontend_readiness.interest = Ready::EMPTY;

                return SessionResult::Close;
            }

            counter += 1;
        }

        if counter == max_loop_iterations {
            error!(
                "PROXY\thandling session {:?} went through {} iterations, there's a probable infinite loop bug, closing the connection",
                self.frontend_token, max_loop_iterations
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
            "Expect state: got timeout for an invalid token: {:?}",
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
