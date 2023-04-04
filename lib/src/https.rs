use std::{
    cell::RefCell,
    collections::{hash_map::Entry, BTreeMap, HashMap},
    io::{ErrorKind, Read},
    net::{Shutdown, SocketAddr as StdSocketAddr},
    os::unix::{io::AsRawFd, net::UnixStream},
    rc::{Rc, Weak},
    str::{from_utf8, from_utf8_unchecked},
    sync::Arc,
};

use anyhow::{bail, Context};
use mio::{
    net::{TcpListener as MioTcpListener, TcpStream as MioTcpStream},
    unix::SourceFd,
    Interest, Poll, Registry, Token,
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
use sozu_command::{
    certificate::CertificateSummary, config::DEFAULT_CIPHER_SUITES, request::RequestHttpFrontend,
};
use time::{Duration, Instant};

use crate::{
    backends::BackendMap,
    buffer_queue::BufferQueue,
    pool::Pool,
    protocol::{
        h2::Http2,
        http::{
            answers::HttpAnswers,
            parser::{hostname_and_port, Method},
        },
        proxy_protocol::expect::ExpectProxyProtocol,
        rustls::TlsHandshake,
        Http, Pipe, SessionState,
    },
    router::{Route, Router},
    server::{ListenSession, ListenToken, ProxyChannel, Server, SessionManager, SessionToken},
    socket::{server_bind, FrontRustls},
    sozu_command::{
        certificate::{Fingerprint, TlsVersion},
        logging,
        ready::Ready,
        request::{
            AddCertificate, Cluster, RemoveCertificate, RemoveListener, ReplaceCertificate,
            Request, WorkerRequest,
        },
        response::{HttpFrontend, HttpsListenerConfig, ResponseContent, WorkerResponse},
        scm_socket::ScmSocket,
        state::ClusterId,
    },
    timer::TimeoutContainer,
    tls::{
        CertificateResolver, GenericCertificateResolverError, MutexWrappedCertificateResolver,
        ParsedCertificateAndKey,
    },
    util::UnwrapLog,
    AcceptError, L7ListenerHandler, L7Proxy, ListenerHandler, Protocol, ProxyConfiguration,
    ProxySession, Readiness, SessionIsToBeClosed, SessionMetrics, SessionResult,
    StateMachineBuilder, StateResult,
};

// const SERVER_PROTOS: &[&str] = &["http/1.1", "h2"];
const SERVER_PROTOS: &[&str] = &["http/1.1"];

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
        Expect(ExpectProxyProtocol<MioTcpStream>, ServerConnection),
        Handshake(TlsHandshake),
        Http(Http<FrontRustls, HttpsListener>),
        WebSocket(Pipe<FrontRustls, HttpsListener>),
        Http2(Http2<FrontRustls>) -> todo!("H2"),
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
        rustls_details: ServerConnection,
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

        let state = if expect_proxy {
            trace!("starting in expect proxy state");
            gauge_add!("protocol.proxy.expect", 1);
            HttpsStateMachine::Expect(
                ExpectProxyProtocol::new(container_frontend_timeout, sock, token, request_id),
                rustls_details,
            )
        } else {
            gauge_add!("protocol.tls.handshake", 1);
            HttpsStateMachine::Handshake(TlsHandshake::new(
                container_frontend_timeout,
                rustls_details,
                sock,
                token,
                request_id,
            ))
        };

        let metrics = SessionMetrics::new(Some(wait_time));
        let mut session = HttpsSession {
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
        };

        session.state.front_readiness().interest =
            Ready::readable() | Ready::hup() | Ready::error();
        session
    }

    pub fn upgrade(&mut self) -> SessionIsToBeClosed {
        debug!("HTTP::upgrade");
        let new_state = match self.state.take() {
            HttpsStateMachine::Expect(expect, ssl) => self.upgrade_expect(expect, ssl),
            HttpsStateMachine::Handshake(handshake) => self.upgrade_handshake(handshake),
            HttpsStateMachine::Http(http) => self.upgrade_http(http),
            HttpsStateMachine::Http2(_) => self.upgrade_http2(),
            HttpsStateMachine::WebSocket(wss) => self.upgrade_websocket(wss),
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
        expect: ExpectProxyProtocol<MioTcpStream>,
        ssl: ServerConnection,
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

                let mut handshake = TlsHandshake::new(
                    container_frontend_timeout,
                    ssl,
                    frontend,
                    self.frontend_token,
                    request_id,
                );
                handshake.frontend_readiness.event = readiness.event;
                handshake.frontend_readiness.event.insert(Ready::readable());

                gauge_add!("protocol.proxy.expect", -1);
                gauge_add!("protocol.tls.handshake", 1);
                return Some(HttpsStateMachine::Handshake(handshake));
            }
        }

        // currently, only happens in expect proxy protocol with AF_UNSPEC address
        // error!("failed to upgrade from expect");
        None
    }

    fn upgrade_handshake(&mut self, handshake: TlsHandshake) -> Option<HttpsStateMachine> {
        // Add 1st routing phase
        // - get SNI
        // - get ALPN
        // - find corresponding listener
        // - determine next protocol (tcps, https ,http2)

        let front_buf = self.pool.upgrade().and_then(|p| p.borrow_mut().checkout());
        front_buf.as_ref()?;

        let sni = handshake.session.sni_hostname();
        let alpn = handshake.session.alpn_protocol();
        let alpn = alpn.and_then(|alpn| from_utf8(alpn).ok());
        info!(
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

        let mut front_buf = front_buf.unwrap();
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

        let readiness = handshake.frontend_readiness.clone();

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
                );

                match http
                    .frontend_socket
                    .session
                    .reader()
                    .read(front_buf.space())
                {
                    Ok(sz) => {
                        //info!("rustls upgrade: there were {} bytes of plaintext available", sz);
                        front_buf.fill(sz);
                        count!("bytes_in", sz as i64);
                        self.metrics.bin += sz;
                    }
                    Err(e) => {
                        error!("read error: {:?}", e);
                    }
                }

                let size = front_buf.available_data();
                let mut buf = BufferQueue::with_buffer(front_buf);
                buf.sliced_input(size);

                http.frontend_buffer = Some(buf);
                http.frontend_readiness = readiness;
                http.frontend_readiness.interest =
                    Ready::readable() | Ready::hup() | Ready::error();

                gauge_add!("protocol.https", 1);
                Some(HttpsStateMachine::Http(http))
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

                http.frontend.readiness = readiness;
                http.frontend.readiness.interest =
                    Ready::readable() | Ready::hup() | Ready::error();

                gauge_add!("protocol.http2", 1);
                Some(HttpsStateMachine::Http2(http))
            }
        }
    }

    fn upgrade_http(&self, http: Http<FrontRustls, HttpsListener>) -> Option<HttpsStateMachine> {
        debug!("https switching to wss");
        let front_token = self.frontend_token;
        let back_token = unwrap_msg!(http.backend_token);
        let ws_context = http.websocket_context();

        let front_buf = match http.frontend_buffer {
            Some(buf) => buf.buffer,
            None => {
                let pool = match self.pool.upgrade() {
                    Some(p) => p,
                    None => return None,
                };

                let buffer = match pool.borrow_mut().checkout() {
                    Some(buf) => buf,
                    None => return None,
                };
                buffer
            }
        };
        let back_buf = match http.backend_buffer {
            Some(buf) => buf.buffer,
            None => {
                let pool = match self.pool.upgrade() {
                    Some(p) => p,
                    None => return None,
                };

                let buffer = match pool.borrow_mut().checkout() {
                    Some(buf) => buf,
                    None => return None,
                };
                buffer
            }
        };

        // TODO: is this necessary? Do we need to reset the timeouts?
        // http.container_frontend_timeout.reset();
        // http.container_backend_timeout.reset();

        let mut pipe = Pipe::new(
            back_buf,
            http.backend_id,
            http.backend_socket,
            http.backend,
            Some(http.container_backend_timeout),
            Some(http.container_frontend_timeout),
            http.cluster_id.clone(),
            front_buf,
            front_token,
            http.frontend_socket,
            self.listener.clone(),
            Protocol::HTTPS,
            http.request_id,
            http.session_address,
            Some(ws_context),
        );

        pipe.frontend_readiness.event = http.frontend_readiness.event;
        pipe.backend_readiness.event = http.backend_readiness.event;
        pipe.set_back_token(back_token);
        pipe.set_cluster_id(http.cluster_id.clone());

        gauge_add!("protocol.https", -1);
        gauge_add!("protocol.wss", 1);
        gauge_add!("http.active_requests", -1);
        gauge_add!("websocket.active_requests", 1);
        Some(HttpsStateMachine::WebSocket(pipe))
    }

    fn upgrade_http2(&self) -> Option<HttpsStateMachine> {
        todo!()
    }

    fn upgrade_websocket(
        &self,
        wss: Pipe<FrontRustls, HttpsListener>,
    ) -> Option<HttpsStateMachine> {
        // what do we do here?
        error!("Upgrade called on WSS, this should not happen");
        Some(HttpsStateMachine::WebSocket(wss))
    }
}

impl ProxySession for HttpsSession {
    fn close(&mut self) {
        if self.has_been_closed {
            return;
        }

        info!("Closing HTTPS session");
        self.metrics.service_stop();

        // Restore gauges
        match self.state.marker() {
            StateMarker::Expect => gauge_add!("protocol.proxy.expect", -1),
            StateMarker::Handshake => gauge_add!("protocol.tls.handshake", -1),
            StateMarker::Http => gauge_add!("protocol.https", -1),
            StateMarker::WebSocket => {
                gauge_add!("protocol.wss", -1);
                gauge_add!("websocket.active_requests", -1);
            }
            StateMarker::Http2 => gauge_add!("protocol.http2", -1),
        }

        if self.state.failed() {
            return;
        }

        self.state.cancel_timeouts();

        let front_socket = self.state.front_socket();
        if let Err(e) = front_socket.shutdown(Shutdown::Both) {
            error!(
                "error shutting down front socket({:?}): {:?}",
                front_socket, e
            );
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

#[derive(thiserror::Error, Debug)]
pub enum ListenerError {
    #[error("failed to acquire the lock, {0}")]
    LockError(String),
    #[error("failed to handle certificate request, got a resolver error, {0}")]
    ResolverError(GenericCertificateResolverError),
    #[error("failed to parse pem, {0}")]
    PemParseError(String),
    #[error("failed to build rustls context, {0}")]
    BuildRustlsError(String),
    #[error("Wrong socket address")]
    WrongSocketAddress,
}

pub struct HttpsListener {
    active: bool,
    address: StdSocketAddr,
    answers: Rc<RefCell<HttpAnswers>>,
    config: HttpsListenerConfig,
    fronts: Router,
    listener: Option<MioTcpListener>,
    resolver: Arc<MutexWrappedCertificateResolver>,
    rustls_details: Arc<ServerConfig>,
    tags: BTreeMap<String, BTreeMap<String, String>>,
    token: Token,
}

impl ListenerHandler for HttpsListener {
    fn get_addr(&self) -> &StdSocketAddr {
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
    ) -> anyhow::Result<Route> {
        let (remaining_input, (hostname, _)) = match hostname_and_port(host.as_bytes()) {
            Ok(tuple) => tuple,
            Err(parse_error) => {
                // parse_error contains a slice of given_host, which should NOT escape this scope
                bail!("Hostname parsing failed for host {host}: {parse_error}");
            }
        };

        if remaining_input != &b""[..] {
            bail!("frontend_from_request: invalid remaining chars after hostname. Host: {host}");
        }

        // it is alright to call from_utf8_unchecked,
        // we already verified that there are only ascii
        // chars in there
        let host = unsafe { from_utf8_unchecked(hostname) };

        self.fronts
            .lookup(host.as_bytes(), uri.as_bytes(), method)
            .with_context(|| "No cluster found")
    }
}

impl CertificateResolver for HttpsListener {
    type Error = ListenerError;

    fn get_certificate(&self, fingerprint: &Fingerprint) -> Option<ParsedCertificateAndKey> {
        let resolver = self
            .resolver
            .0
            .lock()
            .map_err(|err| ListenerError::LockError(err.to_string()))
            .ok()?;

        resolver.get_certificate(fingerprint)
    }

    fn add_certificate(&mut self, opts: &AddCertificate) -> Result<Fingerprint, Self::Error> {
        let mut resolver = self
            .resolver
            .0
            .lock()
            .map_err(|err| ListenerError::LockError(err.to_string()))?;

        resolver
            .add_certificate(opts)
            .map_err(ListenerError::ResolverError)
    }

    fn remove_certificate(&mut self, opts: &RemoveCertificate) -> Result<(), Self::Error> {
        let mut resolver = self
            .resolver
            .0
            .lock()
            .map_err(|err| ListenerError::LockError(err.to_string()))?;

        resolver
            .remove_certificate(opts)
            .map_err(ListenerError::ResolverError)
    }
}

impl HttpsListener {
    pub fn try_new(
        config: HttpsListenerConfig,
        token: Token,
    ) -> Result<HttpsListener, ListenerError> {
        let resolver = Arc::new(MutexWrappedCertificateResolver::new());

        let server_config = Arc::new(Self::create_rustls_context(&config, resolver.to_owned())?);

        let address: StdSocketAddr = config
            .address
            .parse()
            .map_err(|_| ListenerError::WrongSocketAddress)?;

        Ok(HttpsListener {
            listener: None,
            address,
            resolver,
            rustls_details: server_config,
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
    ) -> anyhow::Result<Token> {
        if self.active {
            return Ok(self.token);
        }

        let mut listener = match tcp_listener {
            Some(tcp_listener) => tcp_listener,
            None => server_bind(self.config.address.clone())
                .with_context(|| format!("could not create listener {:?}", self.config.address))?,
        };

        registry
            .register(&mut listener, self.token, Interest::READABLE)
            .with_context(|| format!("Could not register listener socket {listener:?}"))?;

        self.listener = Some(listener);
        self.active = true;
        Ok(self.token)
    }

    pub fn create_rustls_context(
        config: &HttpsListenerConfig,
        resolver: Arc<MutexWrappedCertificateResolver>,
    ) -> Result<ServerConfig, ListenerError> {
        let cipher_names = if config.cipher_list.is_empty() {
            DEFAULT_CIPHER_SUITES.to_vec()
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
            .filter_map(|version| match version {
                TlsVersion::TLSv1_2 => Some(&rustls::version::TLS12),
                TlsVersion::TLSv1_3 => Some(&rustls::version::TLS13),
                other_version => {
                    error!("unsupported TLS version: {:?}", other_version);
                    None
                }
            })
            .collect::<Vec<_>>();

        let mut server_config = ServerConfig::builder()
            .with_cipher_suites(&ciphers[..])
            .with_safe_default_kx_groups()
            .with_protocol_versions(&versions[..])
            .map_err(|err| ListenerError::BuildRustlsError(err.to_string()))?
            .with_no_client_auth()
            .with_cert_resolver(resolver);

        let mut protocols = SERVER_PROTOS
            .iter()
            .map(|proto| proto.as_bytes().to_vec())
            .collect::<Vec<_>>();
        server_config.alpn_protocols.append(&mut protocols);

        Ok(server_config)
    }

    pub fn add_https_front(&mut self, tls_front: HttpFrontend) -> anyhow::Result<()> {
        self.fronts
            .add_http_front(&tls_front)
            .with_context(|| "Could not add https frontend")
    }

    pub fn remove_https_front(&mut self, tls_front: HttpFrontend) -> anyhow::Result<()> {
        debug!("removing tls_front {:?}", tls_front);
        self.fronts
            .remove_http_front(&tls_front)
            .with_context(|| "Could not remove https frontend")
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
    ) -> anyhow::Result<Option<ResponseContent>> {
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

    pub fn soft_stop(&mut self) -> anyhow::Result<()> {
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
            bail!("Error deregistering listen sockets: {:?}", socket_errors);
        }

        Ok(())
    }

    pub fn hard_stop(&mut self) -> anyhow::Result<()> {
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
            bail!("Error deregistering listen sockets: {:?}", socket_errors);
        }

        Ok(())
    }

    pub fn logging(&mut self, logging_filter: String) -> anyhow::Result<Option<ResponseContent>> {
        logging::LOGGER.with(|l| {
            let directives = logging::parse_logging_spec(&logging_filter);
            l.borrow_mut().set_directives(directives);
        });
        Ok(None)
    }

    pub fn query_all_certificates(&mut self) -> anyhow::Result<Option<ResponseContent>> {
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
                        fingerprint,
                    })
                    .collect();

                (owned.address, certificate_summaries)
            })
            .collect::<HashMap<_, _>>();

        info!(
            "got Certificates::All query, answering with {:?}",
            certificates
        );

        Ok(Some(ResponseContent::Certificates(certificates)))
    }

    pub fn query_certificate_for_domain(
        &mut self,
        domain: String,
    ) -> anyhow::Result<Option<ResponseContent>> {
        let certificates = self
            .listeners
            .values()
            .map(|listener| {
                let owned = listener.borrow();
                let resolver = unwrap_msg!(owned.resolver.0.lock());
                let mut certificate_summary = vec![];

                resolver
                    .domain_lookup(domain.as_bytes(), true)
                    .map(|(k, fingerprint)| {
                        certificate_summary.push(CertificateSummary {
                            domain: String::from_utf8(k.to_vec()).unwrap(),
                            fingerprint: fingerprint.clone(),
                        });
                    });
                (owned.address, certificate_summary)
            })
            .collect::<HashMap<_, _>>();

        info!(
            "got Certificates::Domain({}) query, answering with {:?}",
            domain, certificates
        );

        Ok(Some(ResponseContent::Certificates(certificates)))
    }

    pub fn activate_listener(
        &mut self,
        addr: &StdSocketAddr,
        tcp_listener: Option<MioTcpListener>,
    ) -> anyhow::Result<Token> {
        let listener = self
            .listeners
            .values()
            .find(|listener| listener.borrow().address == *addr)
            .with_context(|| format!("No listener found for address {addr}"))?;

        listener
            .borrow_mut()
            .activate(&self.registry, tcp_listener)
            .with_context(|| "Failed to activate listener")
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

    // TODO: return Result with context
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

    pub fn add_cluster(&mut self, mut cluster: Cluster) -> anyhow::Result<Option<ResponseContent>> {
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

    pub fn remove_cluster(&mut self, cluster_id: &str) -> anyhow::Result<Option<ResponseContent>> {
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
    ) -> anyhow::Result<Option<ResponseContent>> {
        let front = front.to_frontend()?;

        match self
            .listeners
            .values()
            .find(|l| l.borrow().address == front.address)
        {
            Some(listener) => {
                let mut owned = listener.borrow_mut();
                owned.set_tags(front.hostname.to_owned(), front.tags.to_owned());
                owned
                    .add_https_front(front)
                    .with_context(|| "Could not add https frontend")?;
                Ok(None)
            }
            None => bail!("adding front {:?} to unknown listener", front),
        }
    }

    pub fn remove_https_frontend(
        &mut self,
        front: RequestHttpFrontend,
    ) -> anyhow::Result<Option<ResponseContent>> {
        let front = front.to_frontend()?;

        if let Some(listener) = self
            .listeners
            .values()
            .find(|l| l.borrow_mut().address == front.address)
        {
            let mut owned = listener.borrow_mut();
            owned.set_tags(front.hostname.to_owned(), None);
            owned
                .remove_https_front(front)
                .with_context(|| "Could not remove https frontend")?;
        }
        Ok(None)
    }

    pub fn add_certificate(
        &mut self,
        add_certificate: AddCertificate,
    ) -> anyhow::Result<Option<ResponseContent>> {
        let address = add_certificate.address.parse()?;
        match self
            .listeners
            .values()
            .find(|l| l.borrow().address == address)
        {
            Some(listener) => listener
                .borrow_mut()
                .add_certificate(&add_certificate)
                .with_context(|| "Could not add certificate to listener")?,
            None => bail!(
                "adding certificate to unknown listener {}",
                add_certificate.address
            ),
        };

        Ok(None)
    }

    //FIXME: should return an error if certificate still has fronts referencing it
    pub fn remove_certificate(
        &mut self,
        remove_certificate: RemoveCertificate,
    ) -> anyhow::Result<Option<ResponseContent>> {
        let address = remove_certificate.address.parse()?;
        match self
            .listeners
            .values()
            .find(|l| l.borrow().address == address)
        {
            Some(listener) => listener
                .borrow_mut()
                .remove_certificate(&remove_certificate)
                .with_context(|| "Could not remove certificate from listener")?,
            None => bail!(
                "removing certificate from unknown listener {}",
                remove_certificate.address
            ),
        };

        Ok(None)
    }

    //FIXME: should return an error if certificate still has fronts referencing it
    pub fn replace_certificate(
        &mut self,
        replace_certificate: ReplaceCertificate,
    ) -> anyhow::Result<Option<ResponseContent>> {
        let address = replace_certificate.address.parse()?;
        match self
            .listeners
            .values()
            .find(|l| l.borrow().address == address)
        {
            Some(listener) => listener
                .borrow_mut()
                .replace_certificate(&replace_certificate)
                .with_context(|| "Could not replace certificate on listener")?,
            None => bail!(
                "replacing certificate on unknown listener {}",
                replace_certificate.address
            ),
        };

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
        let rustls_details = ServerConnection::new(owned.rustls_details.clone()).map_err(|e| {
            error!("failed to create server session: {:?}", e);
            AcceptError::IoError
        })?;

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
            rustls_details,
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

        let content_result = match request.content {
            Request::AddCluster(cluster) => {
                info!("{} add cluster {:?}", request_id, cluster);
                self.add_cluster(cluster.clone())
                    .with_context(|| format!("Could not add cluster {}", cluster.cluster_id))
            }
            Request::RemoveCluster { cluster_id } => {
                info!("{} remove cluster {:?}", request_id, cluster_id);
                self.remove_cluster(&cluster_id)
                    .with_context(|| format!("Could not remove cluster {cluster_id}"))
            }
            Request::AddHttpsFrontend(front) => {
                info!("{} add https front {:?}", request_id, front);
                self.add_https_frontend(front)
                    .with_context(|| "Could not add https frontend")
            }
            Request::RemoveHttpsFrontend(front) => {
                info!("{} remove https front {:?}", request_id, front);
                self.remove_https_frontend(front)
                    .with_context(|| "Could not remove https frontend")
            }
            Request::AddCertificate(add_certificate) => {
                info!("{} add certificate: {:?}", request_id, add_certificate);
                self.add_certificate(add_certificate)
                    .with_context(|| "Could not add certificate")
            }
            Request::RemoveCertificate(remove_certificate) => {
                info!(
                    "{} remove certificate: {:?}",
                    request_id, remove_certificate
                );
                self.remove_certificate(remove_certificate)
                    .with_context(|| "Could not remove certificate")
            }
            Request::ReplaceCertificate(replace_certificate) => {
                info!(
                    "{} replace certificate: {:?}",
                    request_id, replace_certificate
                );
                self.replace_certificate(replace_certificate)
                    .with_context(|| "Could not replace certificate")
            }
            Request::RemoveListener(remove) => {
                info!("removing HTTPS listener at address {:?}", remove.address);
                self.remove_listener(remove.clone()).with_context(|| {
                    format!("Could not remove listener at address {:?}", remove.address)
                })
            }
            Request::SoftStop => {
                info!("{} processing soft shutdown", request_id);
                match self
                    .soft_stop()
                    .with_context(|| "Could not perform soft stop")
                {
                    Ok(_) => {
                        info!("{} soft stop successful", request_id);
                        return WorkerResponse::processing(request.id);
                    }
                    Err(e) => Err(e),
                }
            }
            Request::HardStop => {
                info!("{} processing hard shutdown", request_id);
                match self
                    .hard_stop()
                    .with_context(|| "Could not perform hard stop")
                {
                    Ok(_) => {
                        info!("{} hard stop successful", request_id);
                        return WorkerResponse::processing(request.id);
                    }
                    Err(e) => Err(e),
                }
            }
            Request::Status => {
                info!("{} status", request_id);
                Ok(None)
            }
            Request::Logging(logging_filter) => {
                info!(
                    "{} changing logging filter to {}",
                    request_id, logging_filter
                );
                self.logging(logging_filter.clone())
                    .with_context(|| format!("Could not set logging level to {logging_filter}"))
            }
            Request::QueryAllCertificates => {
                info!("{} query all certificates", request_id);
                self.query_all_certificates()
                    .with_context(|| "Could not query all certificates")
            }
            Request::QueryCertificatesByDomain(domain) => {
                info!("{} query certificate for domain {}", request_id, domain);
                self.query_certificate_for_domain(domain.clone())
                    .with_context(|| format!("Could not query certificate for domain {domain}"))
            }
            other_request => {
                error!(
                    "{} unsupported request for HTTPS proxy, ignoring {:?}",
                    request.id, other_request
                );
                Err(anyhow::Error::msg("unsupported message"))
            }
        };

        match content_result {
            Ok(content) => {
                info!("{} successful", request_id);
                match content {
                    Some(content) => WorkerResponse::ok_with_content(request_id, content),
                    None => WorkerResponse::ok(request_id),
                }
            }
            Err(error_message) => {
                error!("{} unsuccessful: {:#}", request_id, error_message);
                WorkerResponse::error(request_id, format!("{error_message:#}"))
            }
        }
    }
}
impl L7Proxy for HttpsProxy {
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
    }
}

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
    fn frontend_from_request_test() {
        let cluster_id1 = "cluster_1".to_owned();
        let cluster_id2 = "cluster_2".to_owned();
        let cluster_id3 = "cluster_3".to_owned();
        let uri1 = "/".to_owned();
        let uri2 = "/yolo".to_owned();
        let uri3 = "/yolo/swag".to_owned();

        let mut fronts = Router::new();
        assert!(fronts.add_tree_rule(
            "lolcatho.st".as_bytes(),
            &PathRule::Prefix(uri1),
            &MethodRule::new(None),
            &Route::ClusterId(cluster_id1.clone())
        ));
        assert!(fronts.add_tree_rule(
            "lolcatho.st".as_bytes(),
            &PathRule::Prefix(uri2),
            &MethodRule::new(None),
            &Route::ClusterId(cluster_id2)
        ));
        assert!(fronts.add_tree_rule(
            "lolcatho.st".as_bytes(),
            &PathRule::Prefix(uri3),
            &MethodRule::new(None),
            &Route::ClusterId(cluster_id3)
        ));
        assert!(fronts.add_tree_rule(
            "other.domain".as_bytes(),
            &PathRule::Prefix("test".to_string()),
            &MethodRule::new(None),
            &Route::ClusterId(cluster_id1)
        ));

        let address: StdSocketAddr = FromStr::from_str("127.0.0.1:1032")
            .expect("test address 127.0.0.1:1032 should be parsed");
        let resolver = Arc::new(MutexWrappedCertificateResolver::new());

        let server_config = ServerConfig::builder()
            .with_safe_default_cipher_suites()
            .with_safe_default_kx_groups()
            .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
            .map_err(|err| ListenerError::BuildRustlsError(err.to_string()))
            .expect("could not create Rustls server config")
            .with_no_client_auth()
            .with_cert_resolver(resolver.clone());

        let rustls_details = Arc::new(server_config);

        let default_config = ListenerBuilder::new_https(address)
            .to_tls()
            .expect("Could not create default HTTPS listener config");

        let listener = HttpsListener {
            listener: None,
            address,
            fronts,
            rustls_details,
            resolver,
            answers: Rc::new(RefCell::new(HttpAnswers::new(
                "HTTP/1.1 404 Not Found\r\n\r\n",
                "HTTP/1.1 503 Service Unavailable\r\n\r\n",
            ))),
            config: default_config,
            token: Token(0),
            active: true,
            tags: BTreeMap::new(),
        };

        println!("TEST {}", line!());
        let frontend1 = listener.frontend_from_request("lolcatho.st", "/", &Method::Get);
        assert_eq!(
            frontend1.expect("should find a frontend"),
            Route::ClusterId("cluster_1".to_string())
        );
        println!("TEST {}", line!());
        let frontend2 = listener.frontend_from_request("lolcatho.st", "/test", &Method::Get);
        assert_eq!(
            frontend2.expect("should find a frontend"),
            Route::ClusterId("cluster_1".to_string())
        );
        println!("TEST {}", line!());
        let frontend3 = listener.frontend_from_request("lolcatho.st", "/yolo/test", &Method::Get);
        assert_eq!(
            frontend3.expect("should find a frontend"),
            Route::ClusterId("cluster_2".to_string())
        );
        println!("TEST {}", line!());
        let frontend4 = listener.frontend_from_request("lolcatho.st", "/yolo/swag", &Method::Get);
        assert_eq!(
            frontend4.expect("should find a frontend"),
            Route::ClusterId("cluster_3".to_string())
        );
        println!("TEST {}", line!());
        let frontend5 = listener.frontend_from_request("domain", "/", &Method::Get);
        assert!(frontend5.is_err());
        // assert!(false);
    }

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
