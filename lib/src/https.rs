use std::{
    cell::RefCell,
    collections::{hash_map::Entry, BTreeMap, HashMap},
    io::ErrorKind,
    net::{Shutdown, SocketAddr as StdSocketAddr},
    os::unix::{io::AsRawFd, net::UnixStream},
    rc::{Rc, Weak},
    str::{from_utf8, from_utf8_unchecked},
    sync::{Arc, Mutex},
};

use anyhow::Context;
use foreign_types_shared::{ForeignType, ForeignTypeRef};
use mio::{
    net::{TcpListener as MioTcpListener, TcpStream as MioTcpStream},
    unix::SourceFd,
    Interest, Poll, Registry, Token,
};
use openssl::{
    dh::Dh,
    error::ErrorStack,
    nid,
    pkey::{PKey, Private},
    ssl::{
        self, select_next_proto, AlpnError, NameType, SniError, Ssl, SslAlert, SslContext,
        SslContextBuilder, SslMethod, SslOptions, SslRef, SslSessionCacheMode, SslVersion,
    },
    x509::X509,
};
use rustls::{
    cipher_suite::{
        TLS13_AES_128_GCM_SHA256, TLS13_AES_256_GCM_SHA384, TLS13_CHACHA20_POLY1305_SHA256,
        TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256, TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256, TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
        TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384, TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
    },
    CipherSuite, ProtocolVersion, ServerConfig, ServerConnection, SupportedCipherSuite,
};
use rusty_ulid::Ulid;
use slab::Slab;
use time::{Duration, Instant};

use sozu_command::{
    certificate::Fingerprint,
    config::{DEFAULT_CIPHER_SUITES, DEFAULT_OPENSSL_CIPHER_LIST, DEFAULT_RUSTLS_CIPHER_LIST},
    logging,
    proto::command::{
        request::RequestType, response_content::ContentType, AddCertificate, CertificateSummary,
        CertificatesByAddress, Cluster, HttpsListenerConfig, ListOfCertificatesByAddress,
        ListenerType, RemoveCertificate, RemoveListener, ReplaceCertificate, RequestHttpFrontend,
        ResponseContent, TlsVersion,
    },
    ready::Ready,
    request::WorkerRequest,
    response::{HttpFrontend, WorkerResponse},
    scm_socket::ScmSocket,
    state::ClusterId,
};

use crate::{
    backends::BackendMap,
    pool::Pool,
    protocol::{
        h2::Http2,
        http::{
            answers::HttpAnswers,
            parser::{hostname_and_port, Method},
        },
        proxy_protocol::expect::ExpectProxyProtocol,
        Http, OpensslHandshake, Pipe, RustlsHandshake, SessionState,
    },
    router::{Route, Router},
    server::{ListenSession, ListenToken, ProxyChannel, Server, SessionManager, SessionToken},
    socket::{server_bind, FrontOpenssl, FrontRustls},
    timer::TimeoutContainer,
    tls_backends::{
        CertificateResolver, GenericCertificateResolver, MutexWrappedCertificateResolver,
        ParsedCertificateAndKey,
    },
    util::UnwrapLog,
    AcceptError, CachedTags, FrontendFromRequestError, L7ListenerHandler, L7Proxy, ListenerError,
    ListenerHandler, Protocol, ProxyConfiguration, ProxyError, ProxySession, SessionIsToBeClosed,
    SessionMetrics, SessionResult, StateMachineBuilder, StateResult, GLOBAL_PROVIDER,
};

// const SERVER_PROTOS: &[&str] = &["http/1.1", "h2"];
const SERVER_PROTOS_RUSTLS: &[&str] = &["http/1.1"];
const SERVER_PROTOS_OPENSSL: &[u8] = b"\x08http/1.1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsCluster {
    cluster_id: String,
    hostname: String,
    path_begin: String,
}

StateMachineBuilder! {
    /// The various Stages of an HTTPS connection:
    ///
    /// - optional (ExpectProxyProtocol)
    /// - TLS handshake
    /// - HTTP or HTTP2
    /// - WebSocket (passthrough), only from HTTP
    enum HttpsStateMachine impl SessionState {
        OpensslExpect(ExpectProxyProtocol<MioTcpStream>, Ssl),
        OpensslHandshake(OpensslHandshake),
        OpensslHttp(Http<FrontOpenssl, HttpsListener>),
        OpensslWebSocket(Pipe<FrontOpenssl, HttpsListener>),
        RustlsExpect(ExpectProxyProtocol<MioTcpStream>, ServerConnection),
        RustlsHandshake(RustlsHandshake),
        RustlsHttp(Http<FrontRustls, HttpsListener>),
        RustlsWebSocket(Pipe<FrontRustls, HttpsListener>),
        RustlsHttp2(Http2<FrontRustls>) -> todo!("H2"),
    }
}

pub enum AlpnProtocols {
    H2,
    Http11,
}

pub struct HttpsSession {
    answers: Rc<RefCell<HttpAnswers>>,
    configured_backend_timeout: Duration,
    configured_connect_timeout: Duration,
    configured_frontend_timeout: Duration,
    frontend_token: Token,
    has_been_closed: bool,
    last_event: Instant,
    listener: Rc<RefCell<HttpsListener>>,
    metrics: SessionMetrics,
    peer_address: Option<StdSocketAddr>,
    pool: Weak<RefCell<Pool>>,
    proxy: Rc<RefCell<HttpsProxy>>,
    public_address: StdSocketAddr,
    state: HttpsStateMachine,
    sticky_name: String,
}

impl HttpsSession {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        answers: Rc<RefCell<HttpAnswers>>,
        configured_backend_timeout: Duration,
        configured_connect_timeout: Duration,
        configured_frontend_timeout: Duration,
        configured_request_timeout: Duration,
        expect_proxy: bool,
        listener: Rc<RefCell<HttpsListener>>,
        pool: Weak<RefCell<Pool>>,
        proxy: Rc<RefCell<HttpsProxy>>,
        public_address: StdSocketAddr,
        tls_details: TlsDetails,
        sock: MioTcpStream,
        sticky_name: String,
        token: Token,
        wait_time: Duration,
    ) -> HttpsSession {
        let peer_address = if expect_proxy {
            // Will be defined later once the expect proxy header has been received and parsed
            None
        } else {
            sock.peer_addr().ok()
        };

        let request_id = Ulid::generate();
        let container_frontend_timeout = TimeoutContainer::new(configured_request_timeout, token);

        let state = match tls_details {
            TlsDetails::Rustls(tls_details) => {
                if expect_proxy {
                    trace!("starting in expect proxy state");
                    gauge_add!("protocol.proxy.expect", 1);
                    HttpsStateMachine::RustlsExpect(
                        ExpectProxyProtocol::new(
                            container_frontend_timeout,
                            sock,
                            token,
                            request_id,
                        ),
                        tls_details,
                    )
                } else {
                    gauge_add!("protocol.tls.handshake", 1);
                    HttpsStateMachine::RustlsHandshake(RustlsHandshake::new(
                        container_frontend_timeout,
                        tls_details,
                        sock,
                        token,
                        request_id,
                    ))
                }
            }
            TlsDetails::Openssl(openssl_details) => {
                if expect_proxy {
                    trace!("starting in expect proxy state");
                    gauge_add!("protocol.proxy.expect", 1);
                    HttpsStateMachine::OpensslExpect(
                        ExpectProxyProtocol::new(
                            container_frontend_timeout,
                            sock,
                            token,
                            request_id,
                        ),
                        openssl_details,
                    )
                } else {
                    gauge_add!("protocol.tls.handshake", 1);
                    HttpsStateMachine::OpensslHandshake(OpensslHandshake::new(
                        container_frontend_timeout,
                        openssl_details,
                        sock,
                        token,
                        request_id,
                        None,
                    ))
                }
            }
        };

        let metrics = SessionMetrics::new(Some(wait_time));
        HttpsSession {
            answers,
            configured_backend_timeout,
            configured_connect_timeout,
            configured_frontend_timeout,
            frontend_token: token,
            has_been_closed: false,
            last_event: Instant::now(),
            listener,
            metrics,
            peer_address,
            pool,
            proxy,
            public_address,
            state,
            sticky_name,
        }
    }

    pub fn upgrade(&mut self) -> SessionIsToBeClosed {
        debug!("HTTP::upgrade");
        let new_state = match self.state.take() {
            HttpsStateMachine::RustlsExpect(expect, ssl) => {
                self.upgrade_expect(expect, TlsDetails::Rustls(ssl))
            }
            HttpsStateMachine::RustlsHandshake(handshake) => {
                self.upgrade_rustls_handshake(handshake)
            }
            HttpsStateMachine::RustlsHttp(http) => self.upgrade_rustls_http(http),
            HttpsStateMachine::RustlsWebSocket(wss) => self.upgrade_rustls_websocket(wss),

            HttpsStateMachine::OpensslExpect(expect, ssl) => {
                self.upgrade_expect(expect, TlsDetails::Openssl(ssl))
            }
            HttpsStateMachine::OpensslHandshake(handshake) => {
                self.upgrade_openssl_handshake(handshake)
            }
            HttpsStateMachine::OpensslHttp(http) => self.upgrade_openssl_http(http),
            HttpsStateMachine::OpensslWebSocket(wss) => self.upgrade_openssl_websocket(wss),

            HttpsStateMachine::RustlsHttp2(_) => unreachable!(),
            HttpsStateMachine::FailedUpgrade(_) => unreachable!(),
        };

        match new_state {
            Some(state) => {
                self.state = state;
                false
            }
            // The state stays FailedUpgrade, but the Session should be closed right after
            None => true,
        }
    }

    fn upgrade_expect(
        &mut self,
        mut expect: ExpectProxyProtocol<MioTcpStream>,
        tls_details: TlsDetails,
    ) -> Option<HttpsStateMachine> {
        if let Some(ref addresses) = expect.addresses {
            if let (Some(public_address), Some(session_address)) =
                (addresses.destination(), addresses.source())
            {
                self.public_address = public_address;
                self.peer_address = Some(session_address);

                let ExpectProxyProtocol {
                    container_frontend_timeout,
                    frontend,
                    frontend_readiness: readiness,
                    request_id,
                    ..
                } = expect;

                let handshake = match tls_details {
                    TlsDetails::Rustls(tls_details) => {
                        let mut handshake = RustlsHandshake::new(
                            container_frontend_timeout,
                            tls_details,
                            frontend,
                            self.frontend_token,
                            request_id,
                        );
                        handshake.frontend_readiness.event = readiness.event;
                        // Can we remove this? If not why?
                        // Add e2e test for proto-proxy upgrades
                        handshake.frontend_readiness.event.insert(Ready::READABLE);
                        HttpsStateMachine::RustlsHandshake(handshake)
                    }
                    TlsDetails::Openssl(openssl_details) => {
                        let mut handshake = OpensslHandshake::new(
                            container_frontend_timeout,
                            openssl_details,
                            frontend,
                            self.frontend_token,
                            request_id,
                            None,
                        );
                        handshake.frontend_readiness.event = readiness.event;
                        // Can we remove this? If not why?
                        // Add e2e test for proto-proxy upgrades
                        handshake.frontend_readiness.event.insert(Ready::READABLE);
                        HttpsStateMachine::OpensslHandshake(handshake)
                    }
                };

                gauge_add!("protocol.proxy.expect", -1);
                gauge_add!("protocol.tls.handshake", 1);
                return Some(handshake);
            }
        }

        // currently, only happens in expect proxy protocol with AF_UNSPEC address
        if !expect.container_frontend_timeout.cancel() {
            error!("failed to cancel request timeout on expect upgrade phase for 'expect proxy protocol with AF_UNSPEC address'");
        }

        None
    }

    fn upgrade_rustls_handshake(
        &mut self,
        handshake: RustlsHandshake,
    ) -> Option<HttpsStateMachine> {
        // Add 1st routing phase
        // - get SNI
        // - get ALPN
        // - find corresponding listener
        // - determine next protocol (tcps, https ,http2)

        let sni = handshake.session.server_name();
        let alpn = handshake.session.alpn_protocol();
        let alpn = alpn.and_then(|alpn| from_utf8(alpn).ok());
        debug!(
            "Successful TLS Handshake with, received: {:?} {:?}",
            sni, alpn
        );

        let alpn = match alpn {
            Some("http/1.1") => AlpnProtocols::Http11,
            Some("h2") => AlpnProtocols::H2,
            Some(other) => {
                error!("Unsupported ALPN protocol: {}", other);
                return None;
            }
            // Some client don't fill in the ALPN protocol, in this case we default to Http/1.1
            None => AlpnProtocols::Http11,
        };

        if let Some(version) = handshake.session.protocol_version() {
            incr!(rustls_version_str(version));
        };
        if let Some(cipher) = handshake.session.negotiated_cipher_suite() {
            incr!(rustls_ciphersuite_str(cipher));
        };

        let front_stream = FrontRustls {
            stream: handshake.stream,
            session: handshake.session,
        };

        gauge_add!("protocol.tls.handshake", -1);
        match alpn {
            AlpnProtocols::Http11 => {
                let mut http = Http::new(
                    self.answers.clone(),
                    self.configured_backend_timeout,
                    self.configured_connect_timeout,
                    self.configured_frontend_timeout,
                    handshake.container_frontend_timeout,
                    front_stream,
                    self.frontend_token,
                    self.listener.clone(),
                    self.pool.clone(),
                    Protocol::HTTPS,
                    self.public_address,
                    handshake.request_id,
                    self.peer_address,
                    self.sticky_name.clone(),
                )
                .ok()?;

                http.frontend_readiness.event = handshake.frontend_readiness.event;

                gauge_add!("protocol.https", 1);
                Some(HttpsStateMachine::RustlsHttp(http))
            }
            AlpnProtocols::H2 => {
                let mut http = Http2::new(
                    front_stream,
                    self.frontend_token,
                    self.pool.clone(),
                    Some(self.public_address),
                    None,
                    self.sticky_name.clone(),
                );

                http.frontend.readiness.event = handshake.frontend_readiness.event;

                gauge_add!("protocol.http2", 1);
                Some(HttpsStateMachine::RustlsHttp2(http))
            }
        }
    }

    fn upgrade_openssl_handshake(
        &mut self,
        handshake: OpensslHandshake,
    ) -> Option<HttpsStateMachine> {
        handshake.stream.as_ref().map(|s| {
            let ssl = s.ssl();
            if let Some(version) = ssl.version2() {
                incr!(openssl_version_str(version))
            }
            ssl.current_cipher().map(|c| incr!(c.name()));
        });

        let selected_protocol = {
            let s = handshake
                .stream
                .as_ref()
                .and_then(|s| s.ssl().selected_alpn_protocol());
            trace!("selected: {}", unsafe {
                from_utf8_unchecked(s.unwrap_or(&b""[..]))
            });

            match s {
                Some(b"h2") => AlpnProtocols::H2,
                Some(b"http/1.1") | None => AlpnProtocols::Http11,
                Some(s) => {
                    error!("unknown alpn protocol: {:?}", s);
                    return None;
                }
            }
        };

        gauge_add!("protocol.tls.handshake", -1);
        match selected_protocol {
            AlpnProtocols::H2 => {
                unreachable!()
            }
            AlpnProtocols::Http11 => {
                let mut http = Http::new(
                    self.answers.clone(),
                    self.configured_backend_timeout,
                    self.configured_connect_timeout,
                    self.configured_frontend_timeout,
                    handshake.container_frontend_timeout,
                    unwrap_msg!(handshake.stream),
                    self.frontend_token,
                    self.listener.clone(),
                    self.pool.clone(),
                    Protocol::HTTPS,
                    self.public_address,
                    handshake.request_id,
                    self.peer_address,
                    self.sticky_name.clone(),
                )
                .ok()?;

                http.frontend_readiness.event = handshake.frontend_readiness.event;

                // let backend_timeout_duration = self.backend_timeout_duration;
                // let mut http = Http::new(
                //     unwrap_msg!(handshake.stream),
                //     self.frontend_token,
                //     handshake.request_id,
                //     pool,
                //     self.public_address,
                //     self.peer_address,
                //     self.sticky_name.clone(),
                //     Protocol::HTTPS,
                //     self.answers.clone(),
                //     self.front_timeout.take(),
                //     self.frontend_timeout_duration,
                //     backend_timeout_duration,
                //     self.listener.clone(),
                // );

                // http.front_readiness = readiness;
                // http.front_readiness.interest = Ready::readable() | Ready::hup() | Ready::error();

                gauge_add!("protocol.https", 1);
                Some(HttpsStateMachine::OpensslHttp(http))
            }
        }
    }

    fn upgrade_rustls_http(
        &self,
        http: Http<FrontRustls, HttpsListener>,
    ) -> Option<HttpsStateMachine> {
        debug!("https switching to wss");
        let front_token = self.frontend_token;
        let back_token = unwrap_msg!(http.backend_token);
        let ws_context = http.websocket_context();

        let mut container_frontend_timeout = http.container_frontend_timeout;
        let mut container_backend_timeout = http.container_backend_timeout;
        container_frontend_timeout.reset();
        container_backend_timeout.reset();

        let mut pipe = Pipe::new(
            http.response_stream.storage.buffer,
            http.backend_id,
            http.backend_socket,
            http.backend,
            Some(container_backend_timeout),
            Some(container_frontend_timeout),
            http.cluster_id,
            http.request_stream.storage.buffer,
            front_token,
            http.frontend_socket,
            self.listener.clone(),
            Protocol::HTTP,
            http.context.id,
            http.context.session_address,
            Some(ws_context),
        );

        pipe.frontend_readiness.event = http.frontend_readiness.event;
        pipe.backend_readiness.event = http.backend_readiness.event;
        pipe.set_back_token(back_token);

        gauge_add!("protocol.https", -1);
        gauge_add!("protocol.wss", 1);
        gauge_add!("http.active_requests", -1);
        gauge_add!("websocket.active_requests", 1);
        Some(HttpsStateMachine::RustlsWebSocket(pipe))
    }

    fn upgrade_openssl_http(
        &self,
        http: Http<FrontOpenssl, HttpsListener>,
    ) -> Option<HttpsStateMachine> {
        debug!("https switching to wss");
        let front_token = self.frontend_token;
        let back_token = unwrap_msg!(http.backend_token);
        let ws_context = http.websocket_context();

        let mut container_frontend_timeout = http.container_frontend_timeout;
        let mut container_backend_timeout = http.container_backend_timeout;
        container_frontend_timeout.reset();
        container_backend_timeout.reset();

        let mut pipe = Pipe::new(
            http.response_stream.storage.buffer,
            http.backend_id,
            http.backend_socket,
            http.backend,
            Some(container_backend_timeout),
            Some(container_frontend_timeout),
            http.cluster_id,
            http.request_stream.storage.buffer,
            front_token,
            http.frontend_socket,
            self.listener.clone(),
            Protocol::HTTP,
            http.context.id,
            http.context.session_address,
            Some(ws_context),
        );

        pipe.frontend_readiness.event = http.frontend_readiness.event;
        pipe.backend_readiness.event = http.backend_readiness.event;
        pipe.set_back_token(back_token);

        gauge_add!("protocol.https", -1);
        gauge_add!("protocol.wss", 1);
        gauge_add!("http.active_requests", -1);
        gauge_add!("websocket.active_requests", 1);
        Some(HttpsStateMachine::OpensslWebSocket(pipe))
    }

    fn upgrade_rustls_websocket(
        &self,
        wss: Pipe<FrontRustls, HttpsListener>,
    ) -> Option<HttpsStateMachine> {
        // what do we do here?
        error!("Upgrade called on WSS, this should not happen");
        Some(HttpsStateMachine::RustlsWebSocket(wss))
    }
    fn upgrade_openssl_websocket(
        &self,
        wss: Pipe<FrontOpenssl, HttpsListener>,
    ) -> Option<HttpsStateMachine> {
        // what do we do here?
        error!("Upgrade called on WSS, this should not happen");
        Some(HttpsStateMachine::OpensslWebSocket(wss))
    }
}

impl ProxySession for HttpsSession {
    fn close(&mut self) {
        if self.has_been_closed {
            return;
        }

        trace!("Closing HTTPS session");
        self.metrics.service_stop();

        // Restore gauges
        match self.state.marker() {
            StateMarker::RustlsExpect | StateMarker::OpensslExpect => {
                gauge_add!("protocol.proxy.expect", -1)
            }
            StateMarker::RustlsHandshake | StateMarker::OpensslHandshake => {
                gauge_add!("protocol.tls.handshake", -1)
            }
            StateMarker::RustlsHttp | StateMarker::OpensslHttp => gauge_add!("protocol.https", -1),
            StateMarker::RustlsWebSocket | StateMarker::OpensslWebSocket => {
                gauge_add!("protocol.wss", -1);
                gauge_add!("websocket.active_requests", -1);
            }
            StateMarker::RustlsHttp2 => gauge_add!("protocol.http2", -1),
        }

        if self.state.failed() {
            match self.state.marker() {
                StateMarker::RustlsExpect | StateMarker::OpensslExpect => {
                    incr!("https.upgrade.expect.failed")
                }
                StateMarker::RustlsHandshake | StateMarker::OpensslHandshake => {
                    incr!("https.upgrade.handshake.failed")
                }
                StateMarker::RustlsHttp | StateMarker::OpensslHttp => {
                    incr!("https.upgrade.http.failed")
                }
                StateMarker::RustlsWebSocket | StateMarker::OpensslWebSocket => {
                    incr!("https.upgrade.wss.failed")
                }
                StateMarker::RustlsHttp2 => incr!("https.upgrade.http2.failed"),
            }
            return;
        }

        self.state.cancel_timeouts();

        let front_socket = self.state.front_socket();
        if let Err(e) = front_socket.shutdown(Shutdown::Both) {
            // error 107 NotConnected can happen when was never fully connected, or was already disconnected due to error
            if e.kind() != ErrorKind::NotConnected {
                error!(
                    "error shutting down front socket({:?}): {:?}",
                    front_socket, e
                );
            }
        }

        // deregister the frontend and remove it
        let proxy = self.proxy.borrow();
        let fd = front_socket.as_raw_fd();
        if let Err(e) = proxy.registry.deregister(&mut SourceFd(&fd)) {
            error!(
                "error deregistering front socket({:?}) while closing HTTPS session: {:?}",
                fd, e
            );
        }
        proxy.remove_session(self.frontend_token);

        // defer backend closing to the state
        self.state.close(self.proxy.clone(), &mut self.metrics);
        self.has_been_closed = true;
    }

    fn timeout(&mut self, token: Token) -> SessionIsToBeClosed {
        let session_result = self.state.timeout(token, &mut self.metrics);
        session_result == StateResult::CloseSession
    }

    fn protocol(&self) -> Protocol {
        Protocol::HTTPS
    }

    fn update_readiness(&mut self, token: Token, events: Ready) {
        trace!(
            "token {:?} got event {}",
            token,
            super::ready_to_string(events)
        );
        self.last_event = Instant::now();
        self.metrics.wait_start();
        self.state.update_readiness(token, events);
    }

    fn ready(&mut self, session: Rc<RefCell<dyn ProxySession>>) -> SessionIsToBeClosed {
        self.metrics.service_start();

        let session_result =
            self.state
                .ready(session.clone(), self.proxy.clone(), &mut self.metrics);

        let to_be_closed = match session_result {
            SessionResult::Close => true,
            SessionResult::Continue => false,
            SessionResult::Upgrade => match self.upgrade() {
                false => self.ready(session),
                true => true,
            },
        };

        self.metrics.service_stop();
        to_be_closed
    }

    fn shutting_down(&mut self) -> SessionIsToBeClosed {
        self.state.shutting_down()
    }

    fn last_event(&self) -> Instant {
        self.last_event
    }

    fn print_session(&self) {
        self.state.print_state("HTTPS");
        error!("Metrics: {:?}", self.metrics);
    }

    fn frontend_token(&self) -> Token {
        self.frontend_token
    }
}

pub type HostName = String;
pub type PathBegin = String;

#[derive(Clone)]
pub struct TlsProviderDetails {
    rustls_server_config: Arc<ServerConfig>,
    openssl_contexts: Arc<Mutex<HashMap<Fingerprint, SslContext>>>,
    openssl_default_context: SslContext,
    openssl_ssl_options: SslOptions,
}

pub enum TlsDetails {
    Openssl(Ssl),
    Rustls(ServerConnection),
}

pub struct HttpsListener {
    active: bool,
    address: StdSocketAddr,
    answers: Rc<RefCell<HttpAnswers>>,
    config: HttpsListenerConfig,
    fronts: Router,
    listener: Option<MioTcpListener>,
    resolver: Arc<MutexWrappedCertificateResolver>,
    tls_provider_details: TlsProviderDetails,
    tags: BTreeMap<String, CachedTags>,
    token: Token,
}

impl ListenerHandler for HttpsListener {
    fn get_addr(&self) -> &StdSocketAddr {
        &self.address
    }

    fn get_tags(&self, key: &str) -> Option<&CachedTags> {
        self.tags.get(key)
    }

    fn set_tags(&mut self, key: String, tags: Option<BTreeMap<String, String>>) {
        match tags {
            Some(tags) => self.tags.insert(key, CachedTags::new(tags)),
            None => self.tags.remove(&key),
        };
    }
}

impl L7ListenerHandler for HttpsListener {
    fn get_sticky_name(&self) -> &str {
        &self.config.sticky_name
    }

    fn get_connect_timeout(&self) -> u32 {
        self.config.connect_timeout
    }

    fn frontend_from_request(
        &self,
        host: &str,
        uri: &str,
        method: &Method,
    ) -> Result<Route, FrontendFromRequestError> {
        let start = Instant::now();
        let (remaining_input, (hostname, _)) = match hostname_and_port(host.as_bytes()) {
            Ok(tuple) => tuple,
            Err(parse_error) => {
                // parse_error contains a slice of given_host, which should NOT escape this scope
                return Err(FrontendFromRequestError::HostParse {
                    host: host.to_owned(),
                    error: parse_error.to_string(),
                });
            }
        };

        if remaining_input != &b""[..] {
            return Err(FrontendFromRequestError::InvalidCharsAfterHost(
                host.to_owned(),
            ));
        }

        // it is alright to call from_utf8_unchecked,
        // we already verified that there are only ascii
        // chars in there
        let host = unsafe { from_utf8_unchecked(hostname) };

        let route = self
            .fronts
            .lookup(host.as_bytes(), uri.as_bytes(), method)
            .ok_or_else(|| {
                incr!("http.failed_backend_matching");
                FrontendFromRequestError::NoClusterFound
            })?;

        let now = Instant::now();

        if let Route::ClusterId(cluster) = &route {
            time!(
                "frontend_matching_time",
                cluster,
                (now - start).whole_milliseconds()
            );
        }

        Ok(route)
    }
}

impl CertificateResolver for HttpsListener {
    type Error = ListenerError;

    fn get_certificate(&self, fingerprint: &Fingerprint) -> Option<ParsedCertificateAndKey> {
        let resolver = self
            .resolver
            .0
            .lock()
            .map_err(|err| ListenerError::Lock(err.to_string()))
            .ok()?;

        resolver.get_certificate(fingerprint)
    }

    fn add_certificate(&mut self, opts: &AddCertificate) -> Result<Fingerprint, Self::Error> {
        let mut resolver = self
            .resolver
            .0
            .lock()
            .map_err(|err| ListenerError::Lock(err.to_string()))?;

        let fingerprint = resolver
            .add_certificate(opts)
            .map_err(ListenerError::Resolver)?;

        let certificate = X509::from_pem(opts.certificate.certificate.as_bytes())
            .map_err(|err| ListenerError::PemParse(err.to_string()))?;
        let key = PKey::private_key_from_pem(opts.certificate.key.as_bytes())
            .map_err(|err| ListenerError::PemParse(err.to_string()))?;

        let mut chain = vec![];
        for certificate in &opts.certificate.certificate_chain {
            chain.push(
                X509::from_pem(certificate.as_bytes())
                    .map_err(|err| ListenerError::PemParse(err.to_string()))?,
            );
        }

        let versions = if !opts.certificate.versions.is_empty() {
            &opts.certificate.versions
        } else {
            &self.config.versions
        };

        let contexts = &self.tls_provider_details.openssl_contexts;
        let (context, _) = Self::create_openssl_context(
            versions,
            &self.config.cipher_list,
            &self.config.cipher_suites,
            &self.config.signature_algorithms,
            &self.config.groups_list,
            Some(&certificate),
            Some(&key),
            Some(&chain),
            self.resolver.to_owned(),
            contexts.to_owned(),
        )?;

        let mut contexts = contexts
            .lock()
            .map_err(|err| ListenerError::Lock(err.to_string()))?;

        contexts.insert(fingerprint.to_owned(), context);
        Ok(fingerprint)
    }

    fn remove_certificate(&mut self, fingerprint: &Fingerprint) -> Result<(), Self::Error> {
        let mut resolver = self
            .resolver
            .0
            .lock()
            .map_err(|err| ListenerError::Lock(err.to_string()))?;

        resolver
            .remove_certificate(fingerprint)
            .map_err(ListenerError::Resolver)
    }
}

fn setup_curves(ctx: &mut SslContextBuilder) -> Result<(), ErrorStack> {
    use openssl::ec::EcKey;

    let curve = EcKey::from_curve_name(nid::Nid::X9_62_PRIME256V1)?;
    ctx.set_tmp_ecdh(&curve)
}

fn setup_dh(ctx: &mut SslContextBuilder) -> Result<(), ErrorStack> {
    let dh = Dh::params_from_pem(
        b"
-----BEGIN DH PARAMETERS-----
MIIBCAKCAQEAm7EQIiluUX20jnC3NGhikNVGH7MSlRIgTpkUlCyWrmlJci+Pu54Q
YzOZw3tw4g84+ipTzpdGuUvZxNd4Y4ZUp/XsdZgnWMfV+y9OuyYAKWGPTtEO/HUy
6buhvi5qahIXdxUm0dReFuOTrDOphNjHXggU8Iwey3U54aL+KVqLMAP+Tev8jrQN
A0mv4X8dmdvCv7LEZPVYFJb3Uprwd60ThSl3NKNTMDnwnRtXpIrSZIZcJh5IER7S
/IexUuBsopcTVIcHfLyaECIbMKnZfyrkAJfHqRWEpPOzk1euVw0hw3SkDJREfB21
yD0TrUjkXyjV/zczIYiYSROg9OE5UgYqswIBAg==
-----END DH PARAMETERS-----
",
    )?;
    ctx.set_tmp_dh(&dh)
}

fn get_server_name<'a, 'b>(ssl: &'a SslRef) -> Option<&'b [u8]> {
    unsafe {
        let mut out: *const libc::c_uchar = std::ptr::null();
        let mut outlen: libc::size_t = 0;

        let res = openssl_sys::SSL_client_hello_get0_ext(
            ssl.as_ptr(),
            0, // TLSEXT_TYPE_server_name = 0
            &mut out as *mut _,
            &mut outlen as *mut _,
        );

        if res == 0 {
            return None;
        }

        let sl = std::slice::from_raw_parts(out, outlen);

        Some(sl)
    }
}

fn parse_sni_name_list(i: &[u8]) -> Option<&[u8]> {
    use nom::{multi::length_data, number::complete::be_u16};

    if i.is_empty() {
        return None;
    }

    match length_data::<_, _, (), _>(be_u16)(i).ok() {
        None => None,
        Some((i, o)) => {
            if !i.is_empty() {
                None
            } else {
                Some(o)
            }
        }
    }
}

fn parse_sni_name(i: &[u8]) -> Option<(&[u8], &[u8])> {
    use nom::{
        bytes::complete::tag, multi::length_data, number::complete::be_u16, sequence::preceded,
    };

    if i.is_empty() {
        return None;
    }

    preceded::<_, _, _, (), _, _>(
        tag([0x00]), // SNIType for hostname is 0
        length_data(be_u16),
    )(i)
    .ok()
}

extern "C" {
    pub fn SSL_set_options(ssl: *mut openssl_sys::SSL, op: libc::c_ulong) -> libc::c_ulong;
}

fn ssl_set_options(ssl: &mut SslRef, context: &SslContext) {
    unsafe {
        let options = openssl_sys::SSL_CTX_get_options(context.as_ptr());
        SSL_set_options(ssl.as_ptr(), options);
    }
}

impl HttpsListener {
    pub fn try_new(
        config: HttpsListenerConfig,
        token: Token,
    ) -> Result<HttpsListener, ListenerError> {
        let resolver = Arc::new(MutexWrappedCertificateResolver::new());

        let openssl_contexts = Arc::new(Mutex::new(HashMap::new()));
        let a = Self::create_default_openssl_context(
            &config,
            resolver.to_owned(),
            openssl_contexts.to_owned(),
        );
        let (openssl_default_context, openssl_ssl_options) = match a {
            Ok(t) => t,
            Err(e) => {
                println!("HERE {e:?}");
                return Err(e);
            }
        };
        let rustls_server_config =
            Arc::new(Self::create_rustls_context(&config, resolver.to_owned())?);

        let address = config
            .address
            .parse::<StdSocketAddr>()
            .map_err(|parse_error| ListenerError::SocketParse {
                address: config.address.clone(),
                error: parse_error.to_string(),
            })?;

        Ok(HttpsListener {
            listener: None,
            address,
            resolver,
            tls_provider_details: TlsProviderDetails {
                rustls_server_config,
                openssl_contexts,
                openssl_default_context,
                openssl_ssl_options,
            },
            active: false,
            fronts: Router::new(),
            answers: Rc::new(RefCell::new(HttpAnswers::new(
                &config.answer_404,
                &config.answer_503,
            ))),
            config,
            token,
            tags: BTreeMap::new(),
        })
    }

    pub fn activate(
        &mut self,
        registry: &Registry,
        tcp_listener: Option<MioTcpListener>,
    ) -> Result<Token, ListenerError> {
        if self.active {
            return Ok(self.token);
        }

        let mut listener = match tcp_listener {
            Some(tcp_listener) => tcp_listener,
            None => server_bind(self.config.address.clone()).map_err(|server_bind_error| {
                ListenerError::Activation {
                    address: self.config.address.clone(),
                    error: server_bind_error.to_string(),
                }
            })?,
        };

        registry
            .register(&mut listener, self.token, Interest::READABLE)
            .map_err(ListenerError::SocketRegistration)?;

        self.listener = Some(listener);
        self.active = true;
        Ok(self.token)
    }

    pub fn create_rustls_context(
        config: &HttpsListenerConfig,
        resolver: Arc<MutexWrappedCertificateResolver>,
    ) -> Result<ServerConfig, ListenerError> {
        let cipher_names = if config.cipher_list.is_empty() {
            DEFAULT_RUSTLS_CIPHER_LIST.to_vec()
        } else {
            config
                .cipher_list
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
        };

        #[rustfmt::skip]
        let ciphers = cipher_names
            .into_iter()
            .filter_map(|cipher| match cipher {
                "TLS13_CHACHA20_POLY1305_SHA256" => Some(TLS13_CHACHA20_POLY1305_SHA256),
                "TLS13_AES_256_GCM_SHA384" => Some(TLS13_AES_256_GCM_SHA384),
                "TLS13_AES_128_GCM_SHA256" => Some(TLS13_AES_128_GCM_SHA256),
                "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256" => Some(TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256),
                "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256" => Some(TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256),
                "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384" => Some(TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384),
                "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256" => Some(TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256),
                "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384" => Some(TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384),
                "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256" => Some(TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256),
                other_cipher => {
                    error!("unknown cipher: {:?}", other_cipher);
                    None
                }
            })
            .collect::<Vec<_>>();

        let versions = config
            .versions
            .iter()
            .filter_map(|version| match TlsVersion::try_from(*version) {
                Ok(TlsVersion::TlsV12) => Some(&rustls::version::TLS12),
                Ok(TlsVersion::TlsV13) => Some(&rustls::version::TLS13),
                Ok(other_version) => {
                    error!("unsupported TLS version {:?}", other_version);
                    None
                }
                Err(_) => {
                    error!("unsupported TLS version");
                    None
                }
            })
            .collect::<Vec<_>>();

        let mut server_config = ServerConfig::builder()
            .with_cipher_suites(&ciphers[..])
            .with_safe_default_kx_groups()
            .with_protocol_versions(&versions[..])
            .map_err(|err| ListenerError::BuildRustls(err.to_string()))?
            .with_no_client_auth()
            .with_cert_resolver(resolver);

        let mut protocols = SERVER_PROTOS_RUSTLS
            .iter()
            .map(|proto| proto.as_bytes().to_vec())
            .collect::<Vec<_>>();
        server_config.alpn_protocols.append(&mut protocols);

        Ok(server_config)
    }

    pub fn create_default_openssl_context(
        config: &HttpsListenerConfig,
        resolver: Arc<MutexWrappedCertificateResolver>,
        contexts: Arc<Mutex<HashMap<Fingerprint, SslContext>>>,
    ) -> Result<(SslContext, SslOptions), ListenerError> {
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
                    .map_err(|err| ListenerError::PemParse(err.to_string()))?,
            );
        }

        let certificate =
            X509::from_pem(cert_read).map_err(|err| ListenerError::PemParse(err.to_string()))?;

        let key = PKey::private_key_from_pem(key_read)
            .map_err(|err| ListenerError::PemParse(err.to_string()))?;

        Self::create_openssl_context(
            &config.versions,
            &config.cipher_list,
            &config.cipher_suites,
            &config.signature_algorithms,
            &config.groups_list,
            Some(&certificate),
            Some(&key),
            Some(&chain[..]),
            resolver,
            contexts,
        )
    }

    pub fn create_openssl_context(
        tls_versions: &[i32],
        cipher_list: &Vec<String>,
        cipher_suites: &Vec<String>,
        signature_algorithms: &Vec<String>,
        groups_list: &Vec<String>,
        cert: Option<&X509>,
        key: Option<&PKey<Private>>,
        cert_chain: Option<&[X509]>,
        resolver: Arc<MutexWrappedCertificateResolver>,
        contexts: Arc<Mutex<HashMap<Fingerprint, SslContext>>>,
    ) -> Result<(SslContext, SslOptions), ListenerError> {
        let cipher_list = if cipher_list.is_empty() {
            DEFAULT_OPENSSL_CIPHER_LIST.to_vec()
        } else {
            cipher_list.iter().map(|s| s.as_str()).collect::<Vec<_>>()
        };

        let tls_versions = tls_versions
            .iter()
            .map(|v| TlsVersion::try_from(*v).unwrap())
            .collect::<Vec<_>>();

        let mut context = SslContext::builder(SslMethod::tls())
            .map_err(|err| ListenerError::BuildOpenssl(err.to_string()))?;

        // FIXME: maybe activate ssl::SslMode::RELEASE_BUFFERS to save some memory?
        let mode = ssl::SslMode::ENABLE_PARTIAL_WRITE | ssl::SslMode::ACCEPT_MOVING_WRITE_BUFFER;

        context.set_mode(mode);

        let mut ssl_options = SslOptions::CIPHER_SERVER_PREFERENCE
            | SslOptions::NO_COMPRESSION
            | SslOptions::NO_TICKET;

        let mut versions = SslOptions::NO_SSLV2
            | SslOptions::NO_SSLV3
            | SslOptions::NO_TLSV1
            | SslOptions::NO_TLSV1_1
            | SslOptions::NO_TLSV1_2;

        // TLSv1.3 is not implemented before OpenSSL v1.1.0
        if cfg!(any(ossl11x, ossl30x)) {
            versions |= SslOptions::NO_TLSV1_3;
        }

        for version in tls_versions.iter() {
            match version {
                TlsVersion::SslV2 => versions.remove(SslOptions::NO_SSLV2),
                TlsVersion::SslV3 => versions.remove(SslOptions::NO_SSLV3),
                TlsVersion::TlsV10 => versions.remove(SslOptions::NO_TLSV1),
                TlsVersion::TlsV11 => versions.remove(SslOptions::NO_TLSV1_1),
                TlsVersion::TlsV12 => versions.remove(SslOptions::NO_TLSV1_2),
                TlsVersion::TlsV13 => versions.remove(SslOptions::NO_TLSV1_3),
            };
        }

        ssl_options.insert(versions);
        trace!("parsed tls options: {:?}", ssl_options);

        context.set_options(ssl_options);
        context.set_session_cache_size(1);
        context.set_session_cache_mode(SslSessionCacheMode::OFF);

        if let Err(e) = setup_curves(&mut context) {
            error!("could not setup curves for openssl: {:?}", e);
            return Err(ListenerError::BuildOpenssl(e.to_string()));
        }

        if let Err(e) = setup_dh(&mut context) {
            error!("could not setup DH for openssl: {:?}", e);
            return Err(ListenerError::BuildOpenssl(e.to_string()));
        }

        println!("{cipher_list:?}");
        if let Err(e) = context.set_cipher_list(&cipher_list.join(":")) {
            error!("could not set context cipher list: {:?}", e);
            // verify the cipher_list and tls_provider are compatible
            return Err(ListenerError::BuildOpenssl(e.to_string()));
        }

        if cfg!(any(ossl11x, ossl30x)) {
            if let Err(e) = context.set_ciphersuites(&cipher_suites.join(":")) {
                error!("could not set context cipher suites: {:?}", e);
                return Err(ListenerError::BuildOpenssl(e.to_string()));
            }

            if let Err(e) = context.set_groups_list(&groups_list.join(":")) {
                if !e.errors().is_empty() {
                    error!("could not set context groups list: {:?}", e);
                    return Err(ListenerError::BuildOpenssl(e.to_string()));
                }
            }
        }

        if cfg!(any(ossl102, ossl11x, ossl30x)) {
            if let Err(e) = context.set_sigalgs_list(&signature_algorithms.join(":")) {
                error!("could not set context signature algorithms: {:?}", e);
                return Err(ListenerError::BuildOpenssl(e.to_string()));
            }
        }

        if let Some(cert) = cert {
            if let Err(e) = context.set_certificate(cert) {
                error!("error adding certificate to context: {:?}", e);
                return Err(ListenerError::BuildOpenssl(e.to_string()));
            }
        }

        if let Some(key) = key {
            if let Err(e) = context.set_private_key(key) {
                error!("error adding private key to context: {:?}", e);
                return Err(ListenerError::BuildOpenssl(e.to_string()));
            }
        }

        if let Err(e) = context.set_alpn_protos(SERVER_PROTOS_OPENSSL) {
            error!("could not set ALPN protocols: {:?}", e);
            return Err(ListenerError::BuildOpenssl(e.to_string()));
        }

        context.set_alpn_select_callback(move |_ssl: &mut SslRef, client_protocols: &[u8]| {
            debug!("got protocols list from client: {:?}", unsafe {
                from_utf8_unchecked(client_protocols)
            });
            match select_next_proto(SERVER_PROTOS_OPENSSL, client_protocols) {
                None => Err(AlpnError::ALERT_FATAL),
                Some(selected) => {
                    debug!("selected protocol with ALPN: {:?}", unsafe {
                        from_utf8_unchecked(selected)
                    });
                    Ok(selected)
                }
            }
        });

        if let Some(cert_chain) = cert_chain {
            for cert in cert_chain {
                if let Err(e) = context.add_extra_chain_cert(cert.clone()) {
                    error!("error adding chain certificate to context: {:?}", e);
                    return Err(ListenerError::BuildOpenssl(e.to_string()));
                }
            }
        }

        context.set_client_hello_callback(Self::create_client_hello_callback(resolver, contexts));

        Ok((context.build(), ssl_options))
    }

    #[allow(dead_code)]
    fn create_servername_callback(
        resolver: Arc<Mutex<GenericCertificateResolver>>,
        contexts: Arc<Mutex<HashMap<Fingerprint, SslContext>>>,
    ) -> impl Fn(&mut SslRef, &mut SslAlert) -> Result<(), SniError> + 'static + Sync + Send {
        move |ssl: &mut SslRef, alert: &mut SslAlert| {
            let resolver = unwrap_msg!(resolver.lock());
            let contexts = unwrap_msg!(contexts.lock());

            trace!("ref: {:?}", ssl);
            if let Some(servername) = ssl.servername(NameType::HOST_NAME).map(|s| s.to_string()) {
                debug!("looking for fingerprint for {:?}", servername);
                if let Some(kv) = resolver.domain_lookup(servername.as_bytes(), true) {
                    debug!(
                        "looking for context for {:?} with fingerprint {:?}",
                        servername, kv.1
                    );
                    if let Some(context) = contexts.get(&kv.1) {
                        debug!("found context for {:?}", servername);

                        if let Ok(()) = ssl.set_ssl_context(context) {
                            debug!(
                                "servername is now {:?}",
                                ssl.servername(NameType::HOST_NAME)
                            );

                            return Ok(());
                        } else {
                            error!("could not set context for {:?}", servername);
                        }
                    } else {
                        error!("no context found for {:?}", servername);
                    }
                } else {
                    //error!("unrecognized server name: {}", servername);
                }
            } else {
                //error!("no server name information found");
            }

            incr!("openssl.sni.error");
            *alert = SslAlert::UNRECOGNIZED_NAME;
            Err(SniError::ALERT_FATAL)
        }
    }

    fn create_client_hello_callback(
        resolver: Arc<MutexWrappedCertificateResolver>,
        contexts: Arc<Mutex<HashMap<Fingerprint, SslContext>>>,
    ) -> impl Fn(&mut SslRef, &mut SslAlert) -> Result<ssl::ClientHelloResponse, ErrorStack>
           + 'static
           + Sync
           + Send {
        move |ssl: &mut SslRef, alert: &mut SslAlert| {
            debug!(
                "client hello callback: TLS version = {:?}",
                ssl.version_str()
            );

            let server_name_opt = get_server_name(ssl).and_then(parse_sni_name_list);
            // no SNI extension, use the default context
            if server_name_opt.is_none() {
                *alert = SslAlert::UNRECOGNIZED_NAME;
                return Ok(ssl::ClientHelloResponse::SUCCESS);
            }

            let mut sni_names = server_name_opt.unwrap();
            let resolver = unwrap_msg!(resolver.0.lock());
            let contexts = unwrap_msg!(contexts.lock());

            while !sni_names.is_empty() {
                // debug!("name list:\n{}", sni_names.to_hex(16));
                match parse_sni_name(sni_names) {
                    None => break,
                    Some((i, servername)) => {
                        // debug!("parsed name:\n{}", servername.to_hex(16));
                        sni_names = i;

                        if let Some(kv) = resolver.domain_lookup(servername, true) {
                            debug!(
                                "looking for context for {:?} with fingerprint {:?}",
                                servername, kv.1
                            );
                            if let Some(context) = contexts.get(&kv.1) {
                                debug!("found context for {:?}", servername);

                                if let Ok(()) = ssl.set_ssl_context(context) {
                                    ssl_set_options(ssl, context);

                                    return Ok(ssl::ClientHelloResponse::SUCCESS);
                                } else {
                                    error!("could not set context for {:?}", servername);
                                }
                            } else {
                                error!("no context found for {:?}", servername);
                            }
                        } else {
                            error!(
                                "unrecognized server name: {:?}",
                                std::str::from_utf8(servername)
                            );
                        }
                    }
                }
            }

            *alert = SslAlert::UNRECOGNIZED_NAME;
            Ok::<ssl::ClientHelloResponse, ErrorStack>(ssl::ClientHelloResponse::SUCCESS)
        }
    }

    pub fn add_https_front(&mut self, tls_front: HttpFrontend) -> Result<(), ListenerError> {
        self.fronts
            .add_http_front(&tls_front)
            .map_err(ListenerError::AddFrontend)
    }

    pub fn remove_https_front(&mut self, tls_front: HttpFrontend) -> Result<(), ListenerError> {
        debug!("removing tls_front {:?}", tls_front);
        self.fronts
            .remove_http_front(&tls_front)
            .map_err(ListenerError::RemoveFrontend)
    }

    fn accept(&mut self) -> Result<MioTcpStream, AcceptError> {
        if let Some(ref sock) = self.listener {
            sock.accept()
                .map_err(|e| match e.kind() {
                    ErrorKind::WouldBlock => AcceptError::WouldBlock,
                    _ => {
                        error!("accept() IO error: {:?}", e);
                        AcceptError::IoError
                    }
                })
                .map(|(sock, _)| sock)
        } else {
            error!("cannot accept connections, no listening socket available");
            Err(AcceptError::IoError)
        }
    }
}

pub struct HttpsProxy {
    listeners: HashMap<Token, Rc<RefCell<HttpsListener>>>,
    clusters: HashMap<ClusterId, Cluster>,
    backends: Rc<RefCell<BackendMap>>,
    pool: Rc<RefCell<Pool>>,
    registry: Registry,
    sessions: Rc<RefCell<SessionManager>>,
}

impl HttpsProxy {
    pub fn new(
        registry: Registry,
        sessions: Rc<RefCell<SessionManager>>,
        pool: Rc<RefCell<Pool>>,
        backends: Rc<RefCell<BackendMap>>,
    ) -> HttpsProxy {
        HttpsProxy {
            listeners: HashMap::new(),
            clusters: HashMap::new(),
            backends,
            pool,
            registry,
            sessions,
        }
    }

    pub fn add_listener(&mut self, config: HttpsListenerConfig, token: Token) -> Option<Token> {
        match self.listeners.entry(token) {
            Entry::Vacant(entry) => {
                entry.insert(Rc::new(RefCell::new(
                    HttpsListener::try_new(config, token).ok()?,
                )));
                Some(token)
            }
            _ => None,
        }
    }

    pub fn remove_listener(
        &mut self,
        remove: RemoveListener,
    ) -> Result<Option<ResponseContent>, ProxyError> {
        let len = self.listeners.len();

        self.listeners
            .retain(|_, listener| listener.borrow().address.to_string() != remove.address);

        if !self.listeners.len() < len {
            info!(
                "no HTTPS listener to remove at address {:?}",
                remove.address
            )
        }
        Ok(None)
    }

    pub fn soft_stop(&mut self) -> Result<(), ProxyError> {
        let listeners: HashMap<_, _> = self.listeners.drain().collect();
        let mut socket_errors = vec![];
        for (_, l) in listeners.iter() {
            if let Some(mut sock) = l.borrow_mut().listener.take() {
                debug!("Deregistering socket {:?}", sock);
                if let Err(e) = self.registry.deregister(&mut sock) {
                    let error = format!("socket {sock:?}: {e:?}");
                    socket_errors.push(error);
                }
            }
        }

        if !socket_errors.is_empty() {
            return Err(ProxyError::SoftStop {
                proxy_protocol: "HTTPS".to_string(),
                error: format!("Error deregistering listen sockets: {:?}", socket_errors),
            });
        }

        Ok(())
    }

    pub fn hard_stop(&mut self) -> Result<(), ProxyError> {
        let mut listeners: HashMap<_, _> = self.listeners.drain().collect();
        let mut socket_errors = vec![];
        for (_, l) in listeners.drain() {
            if let Some(mut sock) = l.borrow_mut().listener.take() {
                debug!("Deregistering socket {:?}", sock);
                if let Err(e) = self.registry.deregister(&mut sock) {
                    let error = format!("socket {sock:?}: {e:?}");
                    socket_errors.push(error);
                }
            }
        }

        if !socket_errors.is_empty() {
            return Err(ProxyError::HardStop {
                proxy_protocol: "HTTPS".to_string(),
                error: format!("Error deregistering listen sockets: {:?}", socket_errors),
            });
        }

        Ok(())
    }

    pub fn logging(
        &mut self,
        logging_filter: String,
    ) -> Result<Option<ResponseContent>, ProxyError> {
        logging::LOGGER.with(|l| {
            let directives = logging::parse_logging_spec(&logging_filter);
            l.borrow_mut().set_directives(directives);
        });
        Ok(None)
    }

    pub fn query_all_certificates(&mut self) -> Result<Option<ResponseContent>, ProxyError> {
        let certificates = self
            .listeners
            .values()
            .map(|listener| {
                let owned = listener.borrow();
                let resolver = unwrap_msg!(owned.resolver.0.lock());
                let certificate_summaries = resolver
                    .domains
                    .to_hashmap()
                    .drain()
                    .map(|(k, fingerprint)| CertificateSummary {
                        domain: String::from_utf8(k).unwrap(),
                        fingerprint: fingerprint.to_string(),
                    })
                    .collect();

                CertificatesByAddress {
                    address: owned.address.to_string(),
                    certificate_summaries,
                }
            })
            .collect();

        info!(
            "got Certificates::All query, answering with {:?}",
            certificates
        );

        Ok(Some(
            ContentType::CertificatesByAddress(ListOfCertificatesByAddress { certificates }).into(),
        ))
    }

    pub fn query_certificate_for_domain(
        &mut self,
        domain: String,
    ) -> Result<Option<ResponseContent>, ProxyError> {
        let certificates = self
            .listeners
            .values()
            .map(|listener| {
                let owned = listener.borrow();
                let resolver = unwrap_msg!(owned.resolver.0.lock());
                let mut certificate_summaries = vec![];

                if let Some((k, fingerprint)) = resolver.domain_lookup(domain.as_bytes(), true) {
                    certificate_summaries.push(CertificateSummary {
                        domain: String::from_utf8(k.to_vec()).unwrap(),
                        fingerprint: fingerprint.to_string(),
                    });
                }
                CertificatesByAddress {
                    address: owned.address.to_string(),
                    certificate_summaries,
                }
            })
            .collect();

        info!(
            "got Certificates::Domain({}) query, answering with {:?}",
            domain, certificates
        );

        Ok(Some(
            ContentType::CertificatesByAddress(ListOfCertificatesByAddress { certificates }).into(),
        ))
    }

    pub fn activate_listener(
        &mut self,
        addr: &StdSocketAddr,
        tcp_listener: Option<MioTcpListener>,
    ) -> Result<Token, ProxyError> {
        let listener = self
            .listeners
            .values()
            .find(|listener| listener.borrow().address == *addr)
            .ok_or(ProxyError::NoListenerFound(addr.to_owned()))?;

        listener
            .borrow_mut()
            .activate(&self.registry, tcp_listener)
            .map_err(|listener_error| ProxyError::ListenerActivation {
                address: *addr,
                listener_error,
            })
    }

    pub fn give_back_listeners(&mut self) -> Vec<(StdSocketAddr, MioTcpListener)> {
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

    // TODO: return <Result, ProxyError>
    pub fn give_back_listener(
        &mut self,
        address: StdSocketAddr,
    ) -> Option<(Token, MioTcpListener)> {
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

    pub fn add_cluster(
        &mut self,
        mut cluster: Cluster,
    ) -> Result<Option<ResponseContent>, ProxyError> {
        if let Some(answer_503) = cluster.answer_503.take() {
            for listener in self.listeners.values() {
                listener
                    .borrow()
                    .answers
                    .borrow_mut()
                    .add_custom_answer(&cluster.cluster_id, &answer_503);
            }
        }
        self.clusters.insert(cluster.cluster_id.clone(), cluster);
        Ok(None)
    }

    pub fn remove_cluster(
        &mut self,
        cluster_id: &str,
    ) -> Result<Option<ResponseContent>, ProxyError> {
        self.clusters.remove(cluster_id);
        for listener in self.listeners.values() {
            listener
                .borrow()
                .answers
                .borrow_mut()
                .remove_custom_answer(cluster_id);
        }

        Ok(None)
    }

    pub fn add_https_frontend(
        &mut self,
        front: RequestHttpFrontend,
    ) -> Result<Option<ResponseContent>, ProxyError> {
        let front = front.clone().to_frontend().map_err(|request_error| {
            ProxyError::WrongInputFrontend {
                front,
                error: request_error.to_string(),
            }
        })?;

        let mut listener = self
            .listeners
            .values()
            .find(|l| l.borrow().address == front.address)
            .ok_or(ProxyError::NoListenerFound(front.address))?
            .borrow_mut();

        listener.set_tags(front.hostname.to_owned(), front.tags.to_owned());
        listener
            .add_https_front(front)
            .map_err(ProxyError::AddFrontend)?;
        Ok(None)
    }

    pub fn remove_https_frontend(
        &mut self,
        front: RequestHttpFrontend,
    ) -> Result<Option<ResponseContent>, ProxyError> {
        let front = front.clone().to_frontend().map_err(|request_error| {
            ProxyError::WrongInputFrontend {
                front,
                error: request_error.to_string(),
            }
        })?;

        let mut listener = self
            .listeners
            .values()
            .find(|l| l.borrow().address == front.address)
            .ok_or(ProxyError::NoListenerFound(front.address))?
            .borrow_mut();

        listener.set_tags(front.hostname.to_owned(), None);
        listener
            .remove_https_front(front)
            .map_err(ProxyError::RemoveFrontend)?;
        Ok(None)
    }

    pub fn add_certificate(
        &mut self,
        add_certificate: AddCertificate,
    ) -> Result<Option<ResponseContent>, ProxyError> {
        let address = add_certificate
            .address
            .parse::<StdSocketAddr>()
            .map_err(|parse_error| ProxyError::SocketParse {
                address: add_certificate.address.clone(),
                error: parse_error.to_string(),
            })?;

        let listener = self
            .listeners
            .values()
            .find(|l| l.borrow().address == address)
            .ok_or(ProxyError::NoListenerFound(address))?;

        listener
            .borrow_mut()
            .add_certificate(&add_certificate)
            .map_err(ProxyError::AddCertificate)?;

        Ok(None)
    }

    //FIXME: should return an error if certificate still has fronts referencing it
    pub fn remove_certificate(
        &mut self,
        remove_certificate: RemoveCertificate,
    ) -> Result<Option<ResponseContent>, ProxyError> {
        let address = remove_certificate
            .address
            .parse::<StdSocketAddr>()
            .map_err(|parse_error| ProxyError::SocketParse {
                address: remove_certificate.address,
                error: parse_error.to_string(),
            })?;

        let fingerprint = Fingerprint(
            hex::decode(&remove_certificate.fingerprint)
                .map_err(|e| ProxyError::WrongCertificateFingerprint(e.to_string()))?,
        );

        let listener = self
            .listeners
            .values()
            .find(|l| l.borrow().address == address)
            .ok_or(ProxyError::NoListenerFound(address))?;

        listener
            .borrow_mut()
            .remove_certificate(&fingerprint)
            .map_err(ProxyError::AddCertificate)?;

        Ok(None)
    }

    //FIXME: should return an error if certificate still has fronts referencing it
    pub fn replace_certificate(
        &mut self,
        replace_certificate: ReplaceCertificate,
    ) -> Result<Option<ResponseContent>, ProxyError> {
        let address = replace_certificate
            .address
            .parse::<StdSocketAddr>()
            .map_err(|parse_error| ProxyError::SocketParse {
                address: replace_certificate.address.clone(),
                error: parse_error.to_string(),
            })?;

        let listener = self
            .listeners
            .values()
            .find(|l| l.borrow().address == address)
            .ok_or(ProxyError::NoListenerFound(address))?;

        listener
            .borrow_mut()
            .replace_certificate(&replace_certificate)
            .map_err(ProxyError::ReplaceCertificate)?;

        Ok(None)
    }
}

impl ProxyConfiguration for HttpsProxy {
    fn accept(&mut self, token: ListenToken) -> Result<MioTcpStream, AcceptError> {
        match self.listeners.get(&Token(token.0)) {
            Some(listener) => listener.borrow_mut().accept(),
            None => Err(AcceptError::IoError),
        }
    }

    fn create_session(
        &mut self,
        mut frontend_sock: MioTcpStream,
        token: ListenToken,
        wait_time: Duration,
        proxy: Rc<RefCell<Self>>,
    ) -> Result<(), AcceptError> {
        let listener = self
            .listeners
            .get(&Token(token.0))
            .ok_or(AcceptError::IoError)?;
        if let Err(e) = frontend_sock.set_nodelay(true) {
            error!(
                "error setting nodelay on front socket({:?}): {:?}",
                frontend_sock, e
            );
        }

        let owned = listener.borrow();
        let tls_details = match GLOBAL_PROVIDER {
            crate::TlsProvider::Openssl => {
                let ssl = Ssl::new(&owned.tls_provider_details.openssl_default_context).map_err(
                    |ssl_creation_error| {
                        error!("could not create ssl context: {}", ssl_creation_error);
                        AcceptError::IoError
                    },
                )?;
                TlsDetails::Openssl(ssl)
            }
            crate::TlsProvider::Rustls => {
                let session =
                    ServerConnection::new(owned.tls_provider_details.rustls_server_config.clone())
                        .map_err(|e| {
                            error!("failed to create server session: {:?}", e);
                            AcceptError::IoError
                        })?;
                TlsDetails::Rustls(session)
            }
        };

        let mut session_manager = self.sessions.borrow_mut();
        let entry = session_manager.slab.vacant_entry();
        let session_token = Token(entry.key());

        self.registry
            .register(
                &mut frontend_sock,
                session_token,
                Interest::READABLE | Interest::WRITABLE,
            )
            .map_err(|register_error| {
                error!(
                    "error registering front socket({:?}): {:?}",
                    frontend_sock, register_error
                );
                AcceptError::RegisterError
            })?;

        let public_address: StdSocketAddr = owned
            .config
            .public_address
            .clone()
            .unwrap_or(owned.config.address.clone())
            .parse()
            .map_err(|_| AcceptError::WrongSocketAddress)?;

        let session = Rc::new(RefCell::new(HttpsSession::new(
            owned.answers.clone(),
            Duration::seconds(owned.config.back_timeout as i64),
            Duration::seconds(owned.config.connect_timeout as i64),
            Duration::seconds(owned.config.front_timeout as i64),
            Duration::seconds(owned.config.request_timeout as i64),
            owned.config.expect_proxy,
            listener.clone(),
            Rc::downgrade(&self.pool),
            proxy,
            public_address,
            tls_details,
            frontend_sock,
            owned.config.sticky_name.clone(),
            session_token,
            wait_time,
        )));
        entry.insert(session);

        Ok(())
    }

    fn notify(&mut self, request: WorkerRequest) -> WorkerResponse {
        let request_id = request.id.clone();

        let request_type = match request.content.request_type {
            Some(t) => t,
            None => return WorkerResponse::error(request_id, "Empty request"),
        };

        let content_result = match request_type {
            RequestType::AddCluster(cluster) => {
                debug!("{} add cluster {:?}", request_id, cluster);
                self.add_cluster(cluster)
            }
            RequestType::RemoveCluster(cluster_id) => {
                debug!("{} remove cluster {:?}", request_id, cluster_id);
                self.remove_cluster(&cluster_id)
            }
            RequestType::AddHttpsFrontend(front) => {
                debug!("{} add https front {:?}", request_id, front);
                self.add_https_frontend(front)
            }
            RequestType::RemoveHttpsFrontend(front) => {
                debug!("{} remove https front {:?}", request_id, front);
                self.remove_https_frontend(front)
            }
            RequestType::AddCertificate(add_certificate) => {
                debug!("{} add certificate: {:?}", request_id, add_certificate);
                self.add_certificate(add_certificate)
            }
            RequestType::RemoveCertificate(remove_certificate) => {
                debug!(
                    "{} remove certificate: {:?}",
                    request_id, remove_certificate
                );
                self.remove_certificate(remove_certificate)
            }
            RequestType::ReplaceCertificate(replace_certificate) => {
                debug!(
                    "{} replace certificate: {:?}",
                    request_id, replace_certificate
                );
                self.replace_certificate(replace_certificate)
            }
            RequestType::RemoveListener(remove) => {
                debug!("removing HTTPS listener at address {:?}", remove.address);
                self.remove_listener(remove)
            }
            RequestType::SoftStop(_) => {
                debug!("{} processing soft shutdown", request_id);
                match self.soft_stop() {
                    Ok(_) => {
                        info!("{} soft stop successful", request_id);
                        return WorkerResponse::processing(request.id);
                    }
                    Err(e) => Err(e),
                }
            }
            RequestType::HardStop(_) => {
                debug!("{} processing hard shutdown", request_id);
                match self.hard_stop() {
                    Ok(_) => {
                        debug!("{} hard stop successful", request_id);
                        return WorkerResponse::processing(request.id);
                    }
                    Err(e) => Err(e),
                }
            }
            RequestType::Status(_) => {
                debug!("{} status", request_id);
                Ok(None)
            }
            RequestType::Logging(logging_filter) => {
                debug!(
                    "{} changing logging filter to {}",
                    request_id, logging_filter
                );
                self.logging(logging_filter)
            }
            RequestType::QueryCertificatesFromWorkers(filters) => {
                if let Some(domain) = filters.domain {
                    debug!("{} query certificate for domain {}", request_id, domain);
                    self.query_certificate_for_domain(domain)
                } else {
                    debug!("{} query all certificates", request_id);
                    self.query_all_certificates()
                }
            }
            other_request => {
                error!(
                    "{} unsupported request for HTTPS proxy, ignoring {:?}",
                    request.id, other_request
                );
                Err(ProxyError::UnsupportedMessage)
            }
        };

        match content_result {
            Ok(content) => {
                debug!("{} successful", request_id);
                match content {
                    Some(content) => WorkerResponse::ok_with_content(request_id, content),
                    None => WorkerResponse::ok(request_id),
                }
            }
            Err(proxy_error) => {
                debug!("{} unsuccessful: {}", request_id, proxy_error);
                WorkerResponse::error(request_id, proxy_error)
            }
        }
    }
}
impl L7Proxy for HttpsProxy {
    fn kind(&self) -> ListenerType {
        ListenerType::Https
    }

    fn register_socket(
        &self,
        socket: &mut MioTcpStream,
        token: Token,
        interest: Interest,
    ) -> Result<(), std::io::Error> {
        self.registry.register(socket, token, interest)
    }

    fn deregister_socket(&self, tcp_stream: &mut MioTcpStream) -> Result<(), std::io::Error> {
        self.registry.deregister(tcp_stream)
    }

    fn add_session(&self, session: Rc<RefCell<dyn ProxySession>>) -> Token {
        let mut session_manager = self.sessions.borrow_mut();
        let entry = session_manager.slab.vacant_entry();
        let token = Token(entry.key());
        let _entry = entry.insert(session);
        token
    }

    fn remove_session(&self, token: Token) -> bool {
        self.sessions
            .borrow_mut()
            .slab
            .try_remove(token.0)
            .is_some()
    }

    fn backends(&self) -> Rc<RefCell<BackendMap>> {
        self.backends.clone()
    }

    fn clusters(&self) -> &HashMap<ClusterId, Cluster> {
        &self.clusters
    }
}

fn openssl_version_str(version: SslVersion) -> &'static str {
    match version {
        SslVersion::SSL3 => "tls.version.SSLv3",
        SslVersion::TLS1 => "tls.version.TLSv1_0",
        SslVersion::TLS1_1 => "tls.version.TLSv1_1",
        SslVersion::TLS1_2 => "tls.version.TLSv1_2",
        //SslVersion::TLS1_3 => "tls.version.TLSv1_3",
        _ => "tls.version.TLSv1_3",
        //_ => "tls.version.Unknown",
    }
}

/// Used for metrics keeping
fn rustls_version_str(version: ProtocolVersion) -> &'static str {
    match version {
        ProtocolVersion::SSLv2 => "tls.version.SSLv2",
        ProtocolVersion::SSLv3 => "tls.version.SSLv3",
        ProtocolVersion::TLSv1_0 => "tls.version.TLSv1_0",
        ProtocolVersion::TLSv1_1 => "tls.version.TLSv1_1",
        ProtocolVersion::TLSv1_2 => "tls.version.TLSv1_2",
        ProtocolVersion::TLSv1_3 => "tls.version.TLSv1_3",
        ProtocolVersion::DTLSv1_0 => "tls.version.DTLSv1_0",
        ProtocolVersion::DTLSv1_2 => "tls.version.DTLSv1_2",
        ProtocolVersion::DTLSv1_3 => "tls.version.DTLSv1_3",
        ProtocolVersion::Unknown(_) => "tls.version.Unknown",
        _ => "tls.version.unimplemented",
    }
}

/// Used for metrics keeping
fn rustls_ciphersuite_str(cipher: SupportedCipherSuite) -> &'static str {
    match cipher.suite() {
        CipherSuite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256 => {
            "tls.cipher.TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256"
        }
        CipherSuite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256 => {
            "tls.cipher.TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256"
        }
        CipherSuite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256 => {
            "tls.cipher.TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256"
        }
        CipherSuite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384 => {
            "tls.cipher.TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384"
        }
        CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256 => {
            "tls.cipher.TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256"
        }
        CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384 => {
            "tls.cipher.TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384"
        }
        CipherSuite::TLS13_CHACHA20_POLY1305_SHA256 => "tls.cipher.TLS13_CHACHA20_POLY1305_SHA256",
        CipherSuite::TLS13_AES_256_GCM_SHA384 => "tls.cipher.TLS13_AES_256_GCM_SHA384",
        CipherSuite::TLS13_AES_128_GCM_SHA256 => "tls.cipher.TLS13_AES_128_GCM_SHA256",
        _ => "tls.cipher.Unsupported",
    }
}

/// this function is not used, but is available for example and testing purposes
pub fn start_https_worker(
    config: HttpsListenerConfig,
    channel: ProxyChannel,
    max_buffers: usize,
    buffer_size: usize,
) -> anyhow::Result<()> {
    use crate::server;

    let event_loop = Poll::new().with_context(|| "could not create event loop")?;

    let pool = Rc::new(RefCell::new(Pool::with_capacity(
        1,
        max_buffers,
        buffer_size,
    )));
    let backends = Rc::new(RefCell::new(BackendMap::new()));

    let mut sessions: Slab<Rc<RefCell<dyn ProxySession>>> = Slab::with_capacity(max_buffers);
    {
        let entry = sessions.vacant_entry();
        info!("taking token {:?} for channel", SessionToken(entry.key()));
        entry.insert(Rc::new(RefCell::new(ListenSession {
            protocol: Protocol::HTTPListen,
        })));
    }
    {
        let entry = sessions.vacant_entry();
        info!("taking token {:?} for timer", SessionToken(entry.key()));
        entry.insert(Rc::new(RefCell::new(ListenSession {
            protocol: Protocol::HTTPListen,
        })));
    }
    {
        let entry = sessions.vacant_entry();
        info!("taking token {:?} for metrics", SessionToken(entry.key()));
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
    let registry = event_loop
        .registry()
        .try_clone()
        .with_context(|| "Failed at creating a registry")?;
    let mut proxy = HttpsProxy::new(registry, sessions.clone(), pool.clone(), backends.clone());
    let address = config.address.clone();
    if proxy.add_listener(config, token).is_some()
        && proxy
            .activate_listener(
                &address
                    .parse()
                    .with_context(|| "Could not parse socket address")?,
                None,
            )
            .is_ok()
    {
        let (scm_server, _scm_client) =
            UnixStream::pair().with_context(|| "Failed at creating scm stream sockets")?;
        let server_config = server::ServerConfig {
            max_connections: max_buffers,
            ..Default::default()
        };
        let mut server = Server::new(
            event_loop,
            channel,
            ScmSocket::new(scm_server.as_raw_fd()).unwrap(),
            sessions,
            pool,
            backends,
            None,
            Some(proxy),
            None,
            server_config,
            None,
            false,
        )
        .with_context(|| "Failed to create server")?;

        info!("starting event loop");
        server.run();
        info!("ending event loop");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, sync::Arc};

    use sozu_command::config::ListenerBuilder;

    use crate::router::{trie::TrieNode, MethodRule, PathRule, Route, Router};

    use super::*;

    /*
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn size_test() {
      assert_size!(ExpectProxyProtocol<mio::net::TcpStream>, 520);
      assert_size!(TlsHandshake, 240);
      assert_size!(Http<SslStream<mio::net::TcpStream>>, 1232);
      assert_size!(Pipe<SslStream<mio::net::TcpStream>>, 272);
      assert_size!(State, 1240);
      // fails depending on the platform?
      assert_size!(Session, 1672);

      assert_size!(SslStream<mio::net::TcpStream>, 16);
      assert_size!(Ssl, 8);
    }
    */

    #[test]
    // fn frontend_from_request_test() {
    //     let cluster_id1 = "cluster_1".to_owned();
    //     let cluster_id2 = "cluster_2".to_owned();
    //     let cluster_id3 = "cluster_3".to_owned();
    //     let uri1 = "/".to_owned();
    //     let uri2 = "/yolo".to_owned();
    //     let uri3 = "/yolo/swag".to_owned();

    //     let mut fronts = Router::new();
    //     assert!(fronts.add_tree_rule(
    //         "lolcatho.st".as_bytes(),
    //         &PathRule::Prefix(uri1),
    //         &MethodRule::new(None),
    //         &Route::ClusterId(cluster_id1.clone())
    //     ));
    //     assert!(fronts.add_tree_rule(
    //         "lolcatho.st".as_bytes(),
    //         &PathRule::Prefix(uri2),
    //         &MethodRule::new(None),
    //         &Route::ClusterId(cluster_id2)
    //     ));
    //     assert!(fronts.add_tree_rule(
    //         "lolcatho.st".as_bytes(),
    //         &PathRule::Prefix(uri3),
    //         &MethodRule::new(None),
    //         &Route::ClusterId(cluster_id3)
    //     ));
    //     assert!(fronts.add_tree_rule(
    //         "other.domain".as_bytes(),
    //         &PathRule::Prefix("test".to_string()),
    //         &MethodRule::new(None),
    //         &Route::ClusterId(cluster_id1)
    //     ));

    //     let address: StdSocketAddr = FromStr::from_str("127.0.0.1:1032")
    //         .expect("test address 127.0.0.1:1032 should be parsed");
    //     let resolver = Arc::new(MutexWrappedCertificateResolver::new());

    //     let server_config = ServerConfig::builder()
    //         .with_safe_default_cipher_suites()
    //         .with_safe_default_kx_groups()
    //         .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
    //         .map_err(|err| ListenerError::BuildRustls(err.to_string()))
    //         .expect("could not create Rustls server config")
    //         .with_no_client_auth()
    //         .with_cert_resolver(resolver.clone());

    //     let rustls_details = Arc::new(server_config);

    //     let default_config = ListenerBuilder::new_https(address)
    //         .to_tls(None)
    //         .expect("Could not create default HTTPS listener config");

    //     let listener = HttpsListener {
    //         listener: None,
    //         address,
    //         fronts,
    //         tls_provider_details: rustls_details,
    //         resolver,
    //         answers: Rc::new(RefCell::new(HttpAnswers::new(
    //             "HTTP/1.1 404 Not Found\r\n\r\n",
    //             "HTTP/1.1 503 Service Unavailable\r\n\r\n",
    //         ))),
    //         config: default_config,
    //         token: Token(0),
    //         active: true,
    //         tags: BTreeMap::new(),
    //     };

    //     println!("TEST {}", line!());
    //     let frontend1 = listener.frontend_from_request("lolcatho.st", "/", &Method::Get);
    //     assert_eq!(
    //         frontend1.expect("should find a frontend"),
    //         Route::ClusterId("cluster_1".to_string())
    //     );
    //     println!("TEST {}", line!());
    //     let frontend2 = listener.frontend_from_request("lolcatho.st", "/test", &Method::Get);
    //     assert_eq!(
    //         frontend2.expect("should find a frontend"),
    //         Route::ClusterId("cluster_1".to_string())
    //     );
    //     println!("TEST {}", line!());
    //     let frontend3 = listener.frontend_from_request("lolcatho.st", "/yolo/test", &Method::Get);
    //     assert_eq!(
    //         frontend3.expect("should find a frontend"),
    //         Route::ClusterId("cluster_2".to_string())
    //     );
    //     println!("TEST {}", line!());
    //     let frontend4 = listener.frontend_from_request("lolcatho.st", "/yolo/swag", &Method::Get);
    //     assert_eq!(
    //         frontend4.expect("should find a frontend"),
    //         Route::ClusterId("cluster_3".to_string())
    //     );
    //     println!("TEST {}", line!());
    //     let frontend5 = listener.frontend_from_request("domain", "/", &Method::Get);
    //     assert!(frontend5.is_err());
    //     // assert!(false);
    // }
    #[test]
    fn wildcard_certificate_names() {
        let mut trie = TrieNode::root();

        trie.domain_insert("*.services.clever-cloud.com".as_bytes().to_vec(), 1u8);
        trie.domain_insert("*.clever-cloud.com".as_bytes().to_vec(), 2u8);
        trie.domain_insert("services.clever-cloud.com".as_bytes().to_vec(), 0u8);
        trie.domain_insert(
            "abprefix.services.clever-cloud.com".as_bytes().to_vec(),
            3u8,
        );
        trie.domain_insert(
            "cdprefix.services.clever-cloud.com".as_bytes().to_vec(),
            4u8,
        );

        let res = trie.domain_lookup(b"test.services.clever-cloud.com", true);
        println!("query result: {res:?}");

        assert_eq!(
            trie.domain_lookup(b"pgstudio.services.clever-cloud.com", true),
            Some(&("*.services.clever-cloud.com".as_bytes().to_vec(), 1u8))
        );
        assert_eq!(
            trie.domain_lookup(b"test-prefix.services.clever-cloud.com", true),
            Some(&("*.services.clever-cloud.com".as_bytes().to_vec(), 1u8))
        );
    }

    #[test]
    fn wildcard_with_subdomains() {
        let mut trie = TrieNode::root();

        trie.domain_insert("*.test.example.com".as_bytes().to_vec(), 1u8);
        trie.domain_insert("hello.sub.test.example.com".as_bytes().to_vec(), 2u8);

        let res = trie.domain_lookup(b"sub.test.example.com", true);
        println!("query result: {res:?}");

        assert_eq!(
            trie.domain_lookup(b"sub.test.example.com", true),
            Some(&("*.test.example.com".as_bytes().to_vec(), 1u8))
        );
        assert_eq!(
            trie.domain_lookup(b"hello.sub.test.example.com", true),
            Some(&("hello.sub.test.example.com".as_bytes().to_vec(), 2u8))
        );

        // now try in a different order
        let mut trie = TrieNode::root();

        trie.domain_insert("hello.sub.test.example.com".as_bytes().to_vec(), 2u8);
        trie.domain_insert("*.test.example.com".as_bytes().to_vec(), 1u8);

        let res = trie.domain_lookup(b"sub.test.example.com", true);
        println!("query result: {res:?}");

        assert_eq!(
            trie.domain_lookup(b"sub.test.example.com", true),
            Some(&("*.test.example.com".as_bytes().to_vec(), 1u8))
        );
        assert_eq!(
            trie.domain_lookup(b"hello.sub.test.example.com", true),
            Some(&("hello.sub.test.example.com".as_bytes().to_vec(), 2u8))
        );
    }
}
