use std::{
    cell::RefCell,
    collections::{hash_map::Entry, BTreeMap, HashMap},
    io::{self, ErrorKind},
    net::{Shutdown, SocketAddr},
    os::unix::io::AsRawFd,
    rc::Rc,
    sync::{Arc, Mutex},
};

use anyhow::Context;
use mio::{net::*, unix::SourceFd, *};
use openssl::{
    pkey::PKey,
    ssl::{SslContext, SslOptions, SslStream},
    x509::X509,
};
use rustls::{
    cipher_suite::{
        TLS13_AES_128_GCM_SHA256, TLS13_AES_256_GCM_SHA384, TLS13_CHACHA20_POLY1305_SHA256,
        TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256, TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256, TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
        TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384, TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
    },
    ServerConfig, ServerConnection,
};
use rusty_ulid::Ulid;
use slab::Slab;
use sozu_command::proxy::{
    AddCertificate, CertificateFingerprint, RemoveCertificate, TcpTlsConfig, TlsProvider,
    TlsVersion,
};
use time::{Duration, Instant};

use crate::{
    backends::BackendMap,
    https_openssl::{self, ListenerError},
    pool::{Checkout, Pool},
    protocol::{
        self,
        proxy_protocol::{
            expect::ExpectProxyProtocol, relay::RelayProxyProtocol, send::SendProxyProtocol,
        },
        {Pipe, ProtocolResult},
    },
    retry::RetryPolicy,
    server::{
        push_event, ListenSession, ListenToken, ProxyChannel, Server, SessionManager, CONN_RETRIES,
        TIMER,
    },
    socket::{server_bind, FrontRustls},
    sozu_command::{
        config::ProxyProtocolConfig,
        logging,
        proxy::{
            ProxyEvent, ProxyRequest, ProxyRequestOrder, ProxyResponse, TcpFrontend,
            TcpListener as TcpListenerConfig,
        },
        ready::Ready,
        scm_socket::ScmSocket,
    },
    timer::TimeoutContainer,
    tls::{
        CertificateResolver, GenericCertificateResolver, MutexWrappedCertificateResolver,
        ParsedCertificateAndKey,
    },
    // tls::MutexWrappedCertificateResolver,
    util::UnwrapLog,
    AcceptError,
    Backend,
    BackendConnectAction,
    BackendConnectionStatus,
    ClusterId,
    ConnectionError,
    ListenerHandler,
    Protocol,
    ProxyConfiguration,
    ProxySession,
    Readiness,
    SessionMetrics,
    SessionResult,
};

pub enum UpgradeResult {
    Continue,
    Close,
    ConnectBackend,
}

pub enum State {
    HandshakeRustls(protocol::rustls::TlsHandshake),
    HandshakeOpenssl(protocol::openssl::TlsHandshake),
    Pipe(Pipe<TcpStream, Listener>),
    PipeRustls(Pipe<FrontRustls, Listener>),
    PipeOpenssl(Pipe<SslStream<TcpStream>, Listener>),
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
    listener: Rc<RefCell<Listener>>,
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
        listener: Rc<RefCell<Listener>>,
    ) -> Result<Session, AcceptError> {
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
                let owned = listener.borrow();
                match &owned.flavor {
                    TcpFlavor::Rustls(ctx) => {
                        gauge_add!("protocol.tcp.tls", 1);
                        frontend_buffer = Some(front_buf);
                        backend_buffer = Some(back_buf);
                        let request_id = Ulid::generate();
                        let ssl = match ServerConnection::new(ctx.ssl_config.clone()) {
                            Ok(session) => session,
                            Err(e) => {
                                error!("failed to create server session: {:?}", e);
                                return Err(AcceptError::IoError);
                            }
                        };
                        let handshake = protocol::rustls::TlsHandshake::new(ssl, sock, request_id);
                        Some(State::HandshakeRustls(handshake))
                    }
                    TcpFlavor::Openssl(_ctx) => {
                        gauge_add!("protocol.tcp.tls", 1);
                        // frontend_buffer = Some(front_buf);
                        // backend_buffer = Some(back_buf);
                        // let request_id = Ulid::generate();
                        // let handshake = protocol::openssl::TlsHandshake::new();
                        // Some(State::HandshakeOpenssl(handshake))
                        todo!();
                    }
                    TcpFlavor::Plaintext => {
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
                            listener.clone(),
                        );
                        pipe.set_cluster_id(cluster_id.clone());
                        Some(State::Pipe(pipe))
                    }
                }
            }
        };

        let metrics = SessionMetrics::new(Some(wait_time));
        //FIXME: timeout usage

        Ok(Session {
            backend: None,
            frontend_token,
            backend_token: None,
            back_connected: BackendConnectionStatus::NotConnected,
            accept_token,
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
            proxy,
            listener,
        })
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
            .map(|backend| backend.borrow().address);

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
            Some(State::PipeRustls(ref mut pipe)) => pipe.front_hup(&mut self.metrics),
            Some(State::PipeOpenssl(ref mut pipe)) => pipe.front_hup(&mut self.metrics),
            Some(State::HandshakeRustls(_)) => {
                self.log_request();
                SessionResult::CloseSession
            }
            Some(State::HandshakeOpenssl(_)) => {
                self.log_request();
                SessionResult::CloseSession
            }
            _ => {
                self.log_request();
                SessionResult::CloseSession
            }
        }
    }

    fn back_hup(&mut self) -> SessionResult {
        match self.protocol {
            Some(State::Pipe(ref mut pipe)) => pipe.back_hup(&mut self.metrics),
            Some(State::PipeRustls(ref mut pipe)) => pipe.back_hup(&mut self.metrics),
            Some(State::PipeOpenssl(ref mut pipe)) => pipe.back_hup(&mut self.metrics),
            Some(State::HandshakeRustls(_)) => {
                self.log_request();
                SessionResult::CloseSession
            }
            Some(State::HandshakeOpenssl(_)) => {
                self.log_request();
                SessionResult::CloseSession
            }
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
            self.cluster_id.as_deref().unwrap_or("-"),
            self.backend_id.as_deref().unwrap_or("-")
        )
    }

    fn readable(&mut self) -> SessionResult {
        if !self.front_timeout.reset() {
            error!("could not reset front timeout");
        }

        let mut should_upgrade_protocol = ProtocolResult::Continue;

        let res = match self.protocol {
            Some(State::Pipe(ref mut pipe)) => pipe.readable(&mut self.metrics),
            Some(State::PipeRustls(ref mut pipe)) => pipe.readable(&mut self.metrics),
            Some(State::PipeOpenssl(ref mut pipe)) => pipe.readable(&mut self.metrics),
            Some(State::HandshakeRustls(ref mut handshake)) => {
                if !self.front_timeout.reset() {
                    error!("could not reset front timeout");
                }
                let res = handshake.readable();
                should_upgrade_protocol = res.0;
                res.1
            }
            Some(State::HandshakeOpenssl(ref mut handshake)) => {
                if !self.front_timeout.reset() {
                    error!("could not reset front timeout");
                }
                let res = handshake.readable(&mut self.metrics);
                should_upgrade_protocol = res.0;
                res.1
            }
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
            Some(State::PipeRustls(ref mut pipe)) => pipe.writable(&mut self.metrics),
            Some(State::PipeOpenssl(ref mut pipe)) => pipe.writable(&mut self.metrics),
            Some(State::HandshakeRustls(ref mut handshake)) => {
                let (upgrade, session_result) = handshake.writable();
                if upgrade == ProtocolResult::Continue {
                    session_result
                } else {
                    match self.upgrade() {
                        UpgradeResult::Continue => SessionResult::Continue,
                        UpgradeResult::Close => SessionResult::CloseSession,
                        UpgradeResult::ConnectBackend => SessionResult::ConnectBackend,
                    }
                }
            }
            Some(State::HandshakeOpenssl(_)) => SessionResult::CloseSession,
            _ => SessionResult::Continue,
        }
    }

    fn back_readable(&mut self) -> SessionResult {
        if !self.back_timeout.reset() {
            error!("could not reset back timeout");
        }

        match self.protocol {
            Some(State::Pipe(ref mut pipe)) => pipe.back_readable(&mut self.metrics),
            Some(State::PipeRustls(ref mut pipe)) => pipe.back_readable(&mut self.metrics),
            Some(State::PipeOpenssl(ref mut pipe)) => pipe.back_readable(&mut self.metrics),
            Some(State::HandshakeRustls(_)) => SessionResult::CloseSession,
            Some(State::HandshakeOpenssl(_)) => SessionResult::CloseSession,
            _ => SessionResult::Continue,
        }
    }

    fn back_writable(&mut self) -> SessionResult {
        let (upgrade, session_result) = match self.protocol {
            Some(State::Pipe(ref mut pipe)) => (
                ProtocolResult::Continue,
                pipe.back_writable(&mut self.metrics),
            ),
            Some(State::PipeRustls(ref mut pipe)) => (
                ProtocolResult::Continue,
                pipe.back_writable(&mut self.metrics),
            ),
            Some(State::PipeOpenssl(ref mut pipe)) => (
                ProtocolResult::Continue,
                pipe.back_writable(&mut self.metrics),
            ),
            Some(State::HandshakeRustls(_)) => {
                (ProtocolResult::Continue, SessionResult::CloseSession)
            }
            Some(State::HandshakeOpenssl(_)) => {
                (ProtocolResult::Continue, SessionResult::CloseSession)
            }
            Some(State::RelayProxyProtocol(ref mut pp)) => pp.back_writable(&mut self.metrics),
            Some(State::SendProxyProtocol(ref mut pp)) => pp.back_writable(&mut self.metrics),
            _ => unreachable!(),
        };

        if let ProtocolResult::Upgrade = upgrade {
            self.upgrade();
        }

        session_result
    }

    fn front_socket(&self) -> &TcpStream {
        match self.protocol {
            Some(State::Pipe(ref pipe)) => pipe.front_socket(),
            Some(State::PipeRustls(ref pipe)) => pipe.front_socket(),
            Some(State::PipeOpenssl(ref pipe)) => pipe.front_socket(),
            Some(State::HandshakeRustls(ref handshake)) => &handshake.stream,
            // Some(State::HandshakeOpenssl(ref handshake)) => &handshake.front.unwrap(),
            Some(State::SendProxyProtocol(ref pp)) => pp.front_socket(),
            Some(State::RelayProxyProtocol(ref pp)) => pp.front_socket(),
            Some(State::ExpectProxyProtocol(ref pp)) => pp.front_socket(),
            _ => unreachable!(),
        }
    }

    fn front_socket_mut(&mut self) -> &mut TcpStream {
        match self.protocol {
            Some(State::Pipe(ref mut pipe)) => pipe.front_socket_mut(),
            Some(State::PipeRustls(ref mut pipe)) => pipe.front_socket_mut(),
            Some(State::PipeOpenssl(ref mut pipe)) => pipe.front_socket_mut(),
            Some(State::HandshakeRustls(ref mut handshake)) => &mut handshake.stream,
            // Some(State::HandshakeOpenssl(ref mut handshake)) => &mut handshake.front.unwrap(),
            Some(State::SendProxyProtocol(ref mut pp)) => pp.front_socket_mut(),
            Some(State::RelayProxyProtocol(ref mut pp)) => pp.front_socket_mut(),
            Some(State::ExpectProxyProtocol(ref mut pp)) => pp.front_socket_mut(),
            _ => unreachable!(),
        }
    }

    fn back_socket_mut(&mut self) -> Option<&mut TcpStream> {
        match self.protocol {
            Some(State::Pipe(ref mut pipe)) => pipe.back_socket_mut(),
            Some(State::PipeRustls(ref mut pipe)) => pipe.back_socket_mut(),
            Some(State::PipeOpenssl(ref mut pipe)) => pipe.back_socket_mut(),
            Some(State::HandshakeRustls(_)) => None,
            Some(State::HandshakeOpenssl(_)) => None,
            Some(State::SendProxyProtocol(ref mut pp)) => pp.back_socket_mut(),
            Some(State::RelayProxyProtocol(ref mut pp)) => pp.back_socket_mut(),
            Some(State::ExpectProxyProtocol(_)) => None,
            _ => unreachable!(),
        }
    }

    pub fn upgrade(&mut self) -> UpgradeResult {
        let protocol = self.protocol.take();

        match protocol {
            Some(State::HandshakeRustls(handshake)) => {
                // if let Some(version) = handshake.session.protocol_version() {
                //     incr!(version_str(version));
                // };
                // if let Some(cipher) = handshake.session.negotiated_cipher_suite() {
                //     incr!(ciphersuite_str(cipher));
                // };

                let front_stream = FrontRustls {
                    stream: handshake.stream,
                    session: handshake.session,
                };

                let pipe = Pipe::new(
                    front_stream,
                    self.frontend_token,
                    handshake.request_id,
                    self.cluster_id.clone(),
                    self.backend_id.clone(),
                    None,
                    None,
                    self.front_buf.take().unwrap(),
                    self.back_buf.take().unwrap(),
                    self.frontend_address,
                    Protocol::TCP,
                    self.listener.clone(),
                );
                self.protocol = Some(State::PipeRustls(pipe));
                UpgradeResult::ConnectBackend
            }
            Some(State::HandshakeOpenssl(_handshake)) => {
                // let pipe = Pipe::new(
                //     front_stream,
                //     self.frontend_token,
                //     handshake.request_id,
                //     self.cluster_id.clone(),
                //     self.backend_id.clone(),
                //     None,
                //     None,
                //     self.front_buf.take().unwrap(),
                //     self.back_buf.take().unwrap(),
                //     self.frontend_address,
                //     Protocol::TCP,
                //     self.listener.clone(),
                // );
                // self.protocol = Some(State::PipeRustls(pipe));
                UpgradeResult::Continue
            }
            Some(State::SendProxyProtocol(pp)) => {
                if self.back_buf.is_some() && self.front_buf.is_some() {
                    let mut pipe = pp.into_pipe(
                        self.front_buf.take().unwrap(),
                        self.back_buf.take().unwrap(),
                        self.listener.clone(),
                    );
                    pipe.set_cluster_id(self.cluster_id.clone());
                    self.protocol = Some(State::Pipe(pipe));
                    gauge_add!("protocol.proxy.send", -1);
                    gauge_add!("protocol.tcp", 1);
                    return UpgradeResult::Continue;
                }
                error!("Missing the frontend or backend buffer queue, we can't switch to a pipe");
                UpgradeResult::Close
            }
            Some(State::RelayProxyProtocol(pp)) => {
                if self.back_buf.is_some() {
                    let mut pipe =
                        pp.into_pipe(self.back_buf.take().unwrap(), self.listener.clone());
                    pipe.set_cluster_id(self.cluster_id.clone());
                    self.protocol = Some(State::Pipe(pipe));
                    gauge_add!("protocol.proxy.relay", -1);
                    gauge_add!("protocol.tcp", 1);
                    return UpgradeResult::Continue;
                }
                error!("Missing the backend buffer queue, we can't switch to a pipe");
                UpgradeResult::Close
            }
            Some(State::ExpectProxyProtocol(pp)) => {
                if self.front_buf.is_some() && self.back_buf.is_some() {
                    let mut pipe = pp.into_pipe(
                        self.front_buf.take().unwrap(),
                        self.back_buf.take().unwrap(),
                        None,
                        None,
                        self.listener.clone(),
                    );
                    pipe.set_cluster_id(self.cluster_id.clone());
                    self.protocol = Some(State::Pipe(pipe));
                    gauge_add!("protocol.proxy.expect", -1);
                    gauge_add!("protocol.tcp", 1);
                    return UpgradeResult::ConnectBackend;
                }
                error!("Missing the backend buffer queue, we can't switch to a pipe");
                UpgradeResult::Close
            }
            Some(State::Pipe(_)) => UpgradeResult::Close,
            Some(State::PipeRustls(_)) => UpgradeResult::Close,
            Some(State::PipeOpenssl(_)) => UpgradeResult::Close,
            None => UpgradeResult::Close,
        }
    }

    fn front_readiness(&mut self) -> &mut Readiness {
        match self.protocol {
            Some(State::Pipe(ref mut pipe)) => pipe.front_readiness(),
            Some(State::PipeRustls(ref mut pipe)) => pipe.front_readiness(),
            Some(State::PipeOpenssl(ref mut pipe)) => pipe.front_readiness(),
            Some(State::HandshakeRustls(ref mut handshake)) => &mut handshake.readiness,
            Some(State::HandshakeOpenssl(ref mut handshake)) => &mut handshake.readiness,
            Some(State::SendProxyProtocol(ref mut pp)) => pp.front_readiness(),
            Some(State::RelayProxyProtocol(ref mut pp)) => pp.front_readiness(),
            Some(State::ExpectProxyProtocol(ref mut pp)) => pp.readiness(),
            _ => unreachable!(),
        }
    }

    fn back_readiness(&mut self) -> Option<&mut Readiness> {
        match self.protocol {
            Some(State::Pipe(ref mut pipe)) => Some(pipe.back_readiness()),
            Some(State::PipeRustls(ref mut pipe)) => Some(pipe.back_readiness()),
            Some(State::PipeOpenssl(ref mut pipe)) => Some(pipe.back_readiness()),
            Some(State::SendProxyProtocol(ref mut pp)) => Some(pp.back_readiness()),
            Some(State::RelayProxyProtocol(ref mut pp)) => Some(pp.back_readiness()),
            _ => None,
        }
    }

    fn back_token(&self) -> Option<Token> {
        match self.protocol {
            Some(State::Pipe(ref pipe)) => pipe.back_token(),
            Some(State::PipeRustls(ref pipe)) => pipe.back_token(),
            Some(State::PipeOpenssl(ref pipe)) => pipe.back_token(),
            Some(State::HandshakeRustls(_)) => self.backend_token,
            Some(State::HandshakeOpenssl(_)) => self.backend_token,
            Some(State::SendProxyProtocol(ref pp)) => pp.back_token(),
            Some(State::RelayProxyProtocol(ref pp)) => pp.back_token(),
            Some(State::ExpectProxyProtocol(_)) => None,
            _ => unreachable!(),
        }
    }

    fn set_back_socket(&mut self, socket: TcpStream) {
        match self.protocol {
            Some(State::Pipe(ref mut pipe)) => pipe.set_back_socket(socket),
            Some(State::PipeRustls(ref mut pipe)) => pipe.set_back_socket(socket),
            Some(State::PipeOpenssl(ref mut pipe)) => pipe.set_back_socket(socket),
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
            Some(State::PipeRustls(ref mut pipe)) => pipe.set_back_token(token),
            Some(State::PipeOpenssl(ref mut pipe)) => pipe.set_back_token(token),
            Some(State::HandshakeRustls(_)) => {}
            Some(State::HandshakeOpenssl(_)) => {}
            Some(State::SendProxyProtocol(ref mut pp)) => pp.set_back_token(token),
            Some(State::RelayProxyProtocol(ref mut pp)) => pp.set_back_token(token),
            Some(State::ExpectProxyProtocol(_)) => {}
            _ => unreachable!(),
        }
    }

    fn set_backend_id(&mut self, id: String) {
        self.backend_id = Some(id.clone());
        if let Some(State::Pipe(ref mut pipe)) = self.protocol {
            pipe.set_backend_id(Some(id));
        }
    }

    fn back_connected(&self) -> BackendConnectionStatus {
        self.back_connected
    }

    fn set_back_connected(&mut self, status: BackendConnectionStatus) {
        let last = self.back_connected;
        self.back_connected = status;

        if status == BackendConnectionStatus::Connected {
            gauge_add!("backend.connections", 1);
            gauge_add!(
                "connections_per_backend",
                1,
                self.cluster_id.as_deref(),
                self.metrics.backend_id.as_deref()
            );
            if let Some(State::SendProxyProtocol(ref mut pp)) = self.protocol {
                pp.set_back_connected(BackendConnectionStatus::Connected);
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

                push_event(ProxyEvent::BackendDown(
                    backend.backend_id.clone(),
                    backend.address,
                ));
            }
        }
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
                    Err(e) => matches!(e.kind(), io::ErrorKind::WouldBlock),
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
            match self.protocol {
                Some(State::HandshakeRustls(_)) => {}
                Some(State::HandshakeOpenssl(_)) => {}
                _ => {
                    match self.connect_to_backend(session.clone()) {
                        // reuse connection or error we can continue
                        Ok(BackendConnectAction::Reuse) | Err(_) => {}
                        // New or Replace: stop here, we must wait for an event
                        _ => return SessionResult::Continue,
                    }
                }
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

                return SessionResult::CloseSession;
            }

            if back_interest.is_error() && self.back_hup() == SessionResult::CloseSession {
                self.front_readiness().interest = Ready::empty();
                if let Some(r) = self.back_readiness() {
                    r.interest = Ready::empty();
                }

                error!(
                    "PROXY session {:?} back error, disconnecting",
                    self.frontend_token
                );
                return SessionResult::CloseSession;
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
    ) -> Result<BackendConnectAction, ConnectionError> {
        let cluster_id = if let Some(cluster_id) = self
            .proxy
            .borrow()
            .listeners
            .get(&self.accept_token)
            .and_then(|listener| listener.borrow().cluster_id.clone())
        {
            cluster_id
        } else {
            error!("no TCP cluster corresponds to that front address");
            return Err(ConnectionError::HostNotFound);
        };

        self.cluster_id = Some(cluster_id.clone());

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
                        "error setting nodelay on back socket({:?}): {:?}",
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
                    error!("error registering back socket({:?}): {:?}", stream, e);
                }

                let connect_timeout_duration = Duration::seconds(
                    self.proxy.borrow().listeners[&self.accept_token]
                        .borrow()
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
                    "error shutting down front socket({:?}): {:?}",
                    self.front_socket(),
                    e
                );
            }
        }

        self.close_backend();

        match self.protocol {
            Some(State::Pipe(_)) => gauge_add!("protocol.tcp", -1),
            Some(State::PipeRustls(_)) => gauge_add!("protocol.tcp.tls", -1),
            Some(State::PipeOpenssl(_)) => gauge_add!("protocol.tcp.tls", -1),
            Some(State::HandshakeRustls(_)) => gauge_add!("protocol.tcp.handshake", -1),
            Some(State::HandshakeOpenssl(_)) => gauge_add!("protocol.tcp.handshake", -1),
            Some(State::SendProxyProtocol(_)) => gauge_add!("protocol.proxy.send", -1),
            Some(State::RelayProxyProtocol(_)) => gauge_add!("protocol.proxy.relay", -1),
            Some(State::ExpectProxyProtocol(_)) => gauge_add!("protocol.proxy.expect", -1),
            None => {}
        }

        let fd = self.front_socket().as_raw_fd();
        let proxy = self.proxy.borrow();
        if let Err(e) = proxy.registry.deregister(&mut SourceFd(&fd)) {
            error!("1error deregistering socket({:?}): {:?}", fd, e);
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
            if let Some(r) = self.back_readiness() {
                r.event |= events;
            }
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
        self.close();
        self.proxy
            .borrow()
            .sessions
            .borrow_mut()
            .slab
            .try_remove(self.frontend_token.0);
    }

    fn last_event(&self) -> Instant {
        self.last_event
    }

    fn print_state(&self) {
        let p: String = match &self.protocol {
            Some(State::ExpectProxyProtocol(_)) => String::from("Expect"),
            Some(State::SendProxyProtocol(_)) => String::from("Send"),
            Some(State::RelayProxyProtocol(_)) => String::from("Relay"),
            Some(State::HandshakeRustls(_)) => String::from("TCP TLS Handshake"),
            Some(State::HandshakeOpenssl(_)) => String::from("TCP TLS Handshake"),
            Some(State::PipeRustls(_)) => String::from("TCP TLS"),
            Some(State::PipeOpenssl(_)) => String::from("TCP TLS"),
            Some(State::Pipe(_)) => String::from("TCP"),
            None => String::from("None"),
        };

        let rf = match *unwrap_msg!(self.protocol.as_ref()) {
            State::ExpectProxyProtocol(ref expect) => &expect.readiness,
            State::SendProxyProtocol(ref send) => &send.front_readiness,
            State::RelayProxyProtocol(ref relay) => &relay.front_readiness,
            State::HandshakeRustls(ref handshake) => &handshake.readiness,
            State::HandshakeOpenssl(ref handshake) => &handshake.readiness,
            State::PipeRustls(ref pipe) => &pipe.front_readiness,
            State::PipeOpenssl(ref pipe) => &pipe.front_readiness,
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

pub enum TcpFlavor {
    Rustls(TcpRustls),
    Openssl(TcpOpenssl),
    Plaintext,
}

impl TcpFlavor {
    fn has_tls(&self) -> bool {
        match self {
            TcpFlavor::Plaintext => false,
            _ => true,
        }
    }
}

pub enum FlavorError {
    Rustls(rustls::Error),
    Openssl(ListenerError),
}

pub struct TcpRustls {
    pub ssl_config: Arc<ServerConfig>,
    pub resolver: Arc<MutexWrappedCertificateResolver>,
}

pub struct TcpOpenssl {
    pub resolver: Arc<Mutex<GenericCertificateResolver>>,
    pub contexts: Arc<Mutex<HashMap<CertificateFingerprint, SslContext>>>,
    pub default_context: SslContext,
    pub _ssl_options: SslOptions,
}

pub struct Listener {
    cluster_id: Option<String>,
    listener: Option<TcpListener>,
    token: Token,
    address: SocketAddr,
    pool: Rc<RefCell<Pool>>,
    config: TcpListenerConfig,
    flavor: TcpFlavor,
    active: bool,
    tags: BTreeMap<String, BTreeMap<String, String>>,
}

impl ListenerHandler for Listener {
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

impl CertificateResolver for Listener {
    type Error = ListenerError;

    fn get_certificate(
        &self,
        fingerprint: &CertificateFingerprint,
    ) -> Option<ParsedCertificateAndKey> {
        match &self.flavor {
            TcpFlavor::Rustls(TcpRustls { resolver, .. }) => resolver
                .0
                .lock()
                .map_err(|err| ListenerError::LockError(err.to_string()))
                .ok()?
                .get_certificate(fingerprint),
            TcpFlavor::Openssl(TcpOpenssl { .. }) => todo!(),
            TcpFlavor::Plaintext => unreachable!(),
        }
    }

    fn add_certificate(
        &mut self,
        opts: &AddCertificate,
    ) -> Result<CertificateFingerprint, Self::Error> {
        match &self.flavor {
            TcpFlavor::Rustls(TcpRustls { resolver, .. }) => resolver
                .0
                .lock()
                .map_err(|err| ListenerError::LockError(err.to_string()))?
                .add_certificate(opts)
                .map_err(ListenerError::ResolverError),
            TcpFlavor::Openssl(TcpOpenssl { .. }) => todo!(),
            TcpFlavor::Plaintext => unreachable!(),
        }
    }

    fn remove_certificate(&mut self, opts: &RemoveCertificate) -> Result<(), Self::Error> {
        match &self.flavor {
            TcpFlavor::Rustls(TcpRustls { resolver, .. }) => resolver
                .0
                .lock()
                .map_err(|err| ListenerError::LockError(err.to_string()))?
                .remove_certificate(opts)
                .map_err(ListenerError::ResolverError),
            TcpFlavor::Openssl(TcpOpenssl { .. }) => todo!(),
            TcpFlavor::Plaintext => unreachable!(),
        }
    }
}

impl Listener {
    fn new(
        config: TcpListenerConfig,
        pool: Rc<RefCell<Pool>>,
        token: Token,
    ) -> Result<Listener, FlavorError> {
        let flavor = match config.tls {
            Some(ref tls) => match tls.tls_provider {
                TlsProvider::Rustls => {
                    Self::create_rustls_resolver(tls).map_err(|e| FlavorError::Rustls(e))?
                }
                TlsProvider::Openssl => {
                    Self::create_openssl_resolver(tls).map_err(|e| FlavorError::Openssl(e))?
                }
            },
            None => TcpFlavor::Plaintext,
        };

        Ok(Listener {
            cluster_id: None,
            listener: None,
            token,
            address: config.address,
            pool,
            config,
            flavor,
            active: false,
            tags: BTreeMap::new(),
        })
    }

    pub fn create_rustls_resolver(config: &TcpTlsConfig) -> Result<TcpFlavor, rustls::Error> {
        let server_config = ServerConfig::builder();
        let server_config = if !config.cipher_list.is_empty() {
            let mut ciphers = Vec::new();
            for cipher in config.cipher_list.iter() {
                match cipher.as_str() {
                    "TLS13_CHACHA20_POLY1305_SHA256" => {
                        ciphers.push(TLS13_CHACHA20_POLY1305_SHA256)
                    }
                    "TLS13_AES_256_GCM_SHA384" => ciphers.push(TLS13_AES_256_GCM_SHA384),
                    "TLS13_AES_128_GCM_SHA256" => ciphers.push(TLS13_AES_128_GCM_SHA256),
                    "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256" => {
                        ciphers.push(TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256)
                    }
                    "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256" => {
                        ciphers.push(TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256)
                    }
                    "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384" => {
                        ciphers.push(TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384)
                    }
                    "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256" => {
                        ciphers.push(TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256)
                    }
                    "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384" => {
                        ciphers.push(TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384)
                    }
                    "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256" => {
                        ciphers.push(TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256)
                    }
                    s => error!("unknown cipher: {:?}", s),
                }
            }
            server_config.with_cipher_suites(&ciphers[..])
        } else {
            server_config.with_safe_default_cipher_suites()
        };
        let server_config = server_config.with_safe_default_kx_groups();
        let mut versions = Vec::new();
        for version in config.versions.iter() {
            match version {
                TlsVersion::TLSv1_2 => versions.push(&rustls::version::TLS12),
                TlsVersion::TLSv1_3 => versions.push(&rustls::version::TLS13),
                s => error!("unsupported TLS version: {:?}", s),
            }
        }
        let server_config = server_config.with_protocol_versions(&versions[..])?;
        let server_config = server_config.with_no_client_auth();
        let resolver = Arc::new(MutexWrappedCertificateResolver::new());
        let server_config = server_config.with_cert_resolver(resolver.clone());
        Ok(TcpFlavor::Rustls(TcpRustls {
            ssl_config: Arc::new(server_config),
            resolver,
        }))
    }

    pub fn create_openssl_resolver(config: &TcpTlsConfig) -> Result<TcpFlavor, ListenerError> {
        let resolver = Arc::new(Mutex::new(GenericCertificateResolver::new()));
        let contexts = Arc::new(Mutex::new(HashMap::new()));

        let cert_read = config
            .certificate
            .as_ref()
            .map(|c| c.as_bytes())
            .unwrap_or_else(|| include_bytes!("../assets/certificate.pem"));

        let key_read = config
            .key
            .as_ref()
            .map(|c| c.as_bytes())
            .unwrap_or_else(|| include_bytes!("../assets/key.pem"));

        let mut chain = vec![];
        for certificate in &config.certificate_chain {
            chain.push(
                X509::from_pem(certificate.as_bytes())
                    .map_err(|err| ListenerError::PemParseError(err.to_string()))?,
            );
        }

        let certificate = X509::from_pem(cert_read)
            .map_err(|err| ListenerError::PemParseError(err.to_string()))?;

        let key = PKey::private_key_from_pem(key_read)
            .map_err(|err| ListenerError::PemParseError(err.to_string()))?;

        let (context, _ssl_options) = https_openssl::Listener::create_context(
            &config.versions,
            &config.cipher_list,
            &config.cipher_suites,
            &config.signature_algorithms,
            &config.groups_list,
            &certificate,
            &key,
            &chain[..],
            resolver.to_owned(),
            contexts.to_owned(),
        )?;

        Ok(TcpFlavor::Openssl(TcpOpenssl {
            default_context: context,
            resolver,
            contexts,
            _ssl_options,
        }))
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
    // Uncomment this when implementing new load balancing algorythms
    // load_balancing: LoadBalancingAlgorithms,
}

pub struct Proxy {
    fronts: HashMap<String, Token>,
    backends: Rc<RefCell<BackendMap>>,
    listeners: HashMap<Token, Rc<RefCell<Listener>>>,
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
    ) -> Result<Option<Token>, FlavorError> {
        match self.listeners.entry(token) {
            Entry::Vacant(entry) => {
                entry.insert(Rc::new(RefCell::new(Listener::new(config, pool, token)?)));
                Ok(Some(token))
            }
            _ => Ok(None),
        }
    }

    pub fn remove_listener(&mut self, address: SocketAddr) -> bool {
        let len = self.listeners.len();

        self.listeners.retain(|_, l| l.borrow().address != address);
        self.listeners.len() < len
    }

    pub fn activate_listener(
        &self,
        addr: &SocketAddr,
        tcp_listener: Option<TcpListener>,
    ) -> Option<Token> {
        self.listeners
            .values()
            .find(|listener| listener.borrow().address == *addr)
            .and_then(|listener| listener.borrow_mut().activate(&self.registry, tcp_listener))
    }

    pub fn give_back_listeners(&mut self) -> Vec<(SocketAddr, TcpListener)> {
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

    pub fn give_back_listener(&mut self, address: SocketAddr) -> Option<(Token, TcpListener)> {
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

impl ProxyConfiguration<Session> for Proxy {
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
            ProxyRequestOrder::AddCertificate(add_certificate) => {
                if let Some(listener) = self.listeners.values().find(|l| {
                    l.borrow().address == add_certificate.address && l.borrow().flavor.has_tls()
                }) {
                    match listener.borrow_mut().add_certificate(&add_certificate) {
                        Ok(_) => ProxyResponse::ok(message.id),
                        Err(err) => ProxyResponse::error(message.id, err),
                    }
                } else {
                    error!("replacing certificate to unknown listener");
                    ProxyResponse::error(message.id, "unsupported message")
                }
            }
            ProxyRequestOrder::RemoveCertificate(remove_certificate) => {
                //FIXME: should return an error if certificate still has fronts referencing it
                if let Some(listener) = self.listeners.values_mut().find(|l| {
                    l.borrow().address == remove_certificate.address && l.borrow().flavor.has_tls()
                }) {
                    match listener
                        .borrow_mut()
                        .remove_certificate(&remove_certificate)
                    {
                        Ok(_) => ProxyResponse::ok(message.id),
                        Err(err) => ProxyResponse::error(message.id, err),
                    }
                } else {
                    error!("replacing certificate to unknown listener");
                    ProxyResponse::error(message.id, "unsupported message")
                }
            }
            ProxyRequestOrder::ReplaceCertificate(replace_certificate) => {
                //FIXME: should return an error if certificate still has fronts referencing it
                if let Some(listener) = self.listeners.values().find(|l| {
                    l.borrow().address == replace_certificate.address && l.borrow().flavor.has_tls()
                }) {
                    match listener
                        .borrow_mut()
                        .replace_certificate(&replace_certificate)
                    {
                        Ok(_) => ProxyResponse::ok(message.id),
                        Err(err) => ProxyResponse::error(message.id, err),
                    }
                } else {
                    error!("replacing certificate to unknown listener");
                    ProxyResponse::error(message.id, "unsupported message")
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

    fn accept(&mut self, token: ListenToken) -> Result<TcpStream, AcceptError> {
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
        frontend_sock: TcpStream,
        token: ListenToken,
        wait_time: Duration,
        proxy: Rc<RefCell<Self>>,
    ) -> Result<(), AcceptError> {
        let internal_token = Token(token.0);

        if let Some(listener) = self.listeners.get(&internal_token) {
            let owned = listener.borrow();
            let mut p = owned.pool.borrow_mut();

            if let (Some(front_buf), Some(back_buf)) = (p.checkout(), p.checkout()) {
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
                    owned.cluster_id.clone(),
                    None,
                    proxy_protocol,
                    wait_time,
                    Duration::seconds(owned.config.front_timeout as i64),
                    Duration::seconds(owned.config.back_timeout as i64),
                    listener.clone(),
                )?;
                incr!("tcp.requests");

                if let Err(e) = self.registry.register(
                    c.front_socket_mut(),
                    session_token,
                    Interest::READABLE | Interest::WRITABLE,
                ) {
                    error!(
                        "error registering front socket({:?}): {:?}",
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

/// This is not directly used by Sōzu but is available for example and testing purposes
pub fn start(
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
    let mut configuration = Proxy::new(registry, sessions.clone(), backends.clone());
    let _ = configuration.add_listener(config, pool.clone(), token);
    let _ = configuration.activate_listener(&address, None);
    let (scm_server, _scm_client) =
        UnixStream::pair().with_context(|| "Failed at creating scm stream sockets")?;

    let server_config = server::ServerConfig {
        max_connections: max_buffers,
        ..Default::default()
    };

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

    /// used in tests only
    pub fn start_proxy() -> anyhow::Result<Channel<ProxyRequest, ProxyResponse>> {
        use crate::server;

        info!("listen for connections");
        let (mut command, channel) =
            Channel::generate(1000, 10000).with_context(|| "should create a channel")?;

        // this thread should call a start() function that performs the same logic and returns Result<()>
        // any error coming from this start() would be mapped and logged within the tread
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
            let mut configuration = Proxy::new(registry, sessions.clone(), backends.clone());
            let listener_config = TcpListenerConfig {
                address: "127.0.0.1:1234".parse().unwrap(),
                public_address: None,
                expect_proxy: false,
                front_timeout: 60,
                back_timeout: 30,
                connect_timeout: 3,
                tls: None,
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
            let scm = ScmSocket::new(scm_client.into_raw_fd());
            scm.send_listeners(&Listeners {
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
            )
            .expect("Failed at creating the server");
            info!("will run");
            server.run();
            info!("ending event loop");
        });

        command.blocking();
        {
            let front = TcpFrontend {
                cluster_id: String::from("yolo"),
                address: "127.0.0.1:1234"
                    .parse()
                    .with_context(|| "Could not parse address")?,
                hostname: None,
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

            command.write_message(&ProxyRequest {
                id: String::from("ID_YOLO1"),
                order: ProxyRequestOrder::AddTcpFrontend(front),
            });
            command.write_message(&ProxyRequest {
                id: String::from("ID_YOLO2"),
                order: ProxyRequestOrder::AddBackend(backend),
            });
        }
        {
            let front = TcpFrontend {
                cluster_id: String::from("yolo"),
                address: "127.0.0.1:1235"
                    .parse()
                    .with_context(|| "Could not parse address")?,
                hostname: None,
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
            command.write_message(&ProxyRequest {
                id: String::from("ID_YOLO3"),
                order: ProxyRequestOrder::AddTcpFrontend(front),
            });
            command.write_message(&ProxyRequest {
                id: String::from("ID_YOLO4"),
                order: ProxyRequestOrder::AddBackend(backend),
            });
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
