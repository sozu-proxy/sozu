use mio::net::*;
use mio::*;
use rustls::{NoClientAuth, ProtocolVersion, ServerConfig, ServerSession, ALL_CIPHERSUITES};
use slab::Slab;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::os::unix::io::AsRawFd;
use std::rc::Rc;
use std::str::from_utf8_unchecked;
use std::sync::Arc;
use time::Duration;

use crate::sozu_command::logging;
use crate::sozu_command::proxy::{
    AddCertificate, CertificateFingerprint, Cluster, HttpFrontend, HttpsListener, ProxyRequest,
    ProxyRequestData, ProxyResponse, ProxyResponseData, ProxyResponseStatus, Query, QueryAnswer,
    QueryAnswerCertificate, QueryCertificateType, RemoveCertificate, Route, TlsVersion,
};
use crate::sozu_command::scm_socket::ScmSocket;

use crate::backends::BackendMap;
use crate::pool::Pool;
use crate::protocol::http::{
    answers::HttpAnswers,
    parser::{hostname_and_port, Method},
};
use crate::router::Router;
use crate::server::{
    ListenSession, ListenToken, ProxyChannel, Server, SessionManager, SessionToken,
};
use crate::socket::server_bind;
use crate::tls::{
    CertificateResolver, GenericCertificateResolverError, MutexWrappedCertificateResolver,
    ParsedCertificateAndKey,
};
use crate::util::UnwrapLog;
use crate::{AcceptError, ClusterId, Protocol, ProxyConfiguration, ProxySession};

use super::session::Session;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsApp {
    pub cluster_id: String,
    pub hostname: String,
    pub path_begin: String,
}

pub type HostName = String;
pub type PathBegin = String;

#[derive(thiserror::Error, Debug)]
pub enum ListenerError {
    #[error("failed to acquired the lock, {0}")]
    LockError(String),
    #[error("failed to handle certificate operation, got resolver error, {0}")]
    ResolverError(GenericCertificateResolverError),
}

pub struct Listener {
    listener: Option<TcpListener>,
    address: SocketAddr,
    fronts: Router,
    answers: Rc<RefCell<HttpAnswers>>,
    pub config: HttpsListener,
    ssl_config: Arc<ServerConfig>,
    resolver: Arc<MutexWrappedCertificateResolver>,
    pub token: Token,
    active: bool,
}

impl CertificateResolver for Listener {
    type Error = ListenerError;

    fn get_certificate(
        &self,
        fingerprint: &CertificateFingerprint,
    ) -> Option<ParsedCertificateAndKey> {
        let resolver = self
            .resolver
            .0
            .lock()
            .map_err(|err| ListenerError::LockError(err.to_string()))
            .ok()?;

        resolver.get_certificate(fingerprint)
    }

    fn add_certificate(
        &mut self,
        opts: &AddCertificate,
    ) -> Result<CertificateFingerprint, Self::Error> {
        let mut resolver = self
            .resolver
            .0
            .lock()
            .map_err(|err| ListenerError::LockError(err.to_string()))?;

        resolver
            .add_certificate(opts)
            .map_err(|err| ListenerError::ResolverError(err))
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

impl Listener {
    pub fn new(config: HttpsListener, token: Token) -> Listener {
        let mut server_config = ServerConfig::new(NoClientAuth::new());
        server_config.versions = config
            .versions
            .iter()
            .map(|version| match version {
                TlsVersion::SSLv2 => ProtocolVersion::SSLv2,
                TlsVersion::SSLv3 => ProtocolVersion::SSLv3,
                TlsVersion::TLSv1_0 => ProtocolVersion::TLSv1_0,
                TlsVersion::TLSv1_1 => ProtocolVersion::TLSv1_1,
                TlsVersion::TLSv1_2 => ProtocolVersion::TLSv1_2,
                TlsVersion::TLSv1_3 => ProtocolVersion::TLSv1_3,
            })
            .collect();

        let resolver = Arc::new(MutexWrappedCertificateResolver::new());
        server_config.cert_resolver = resolver.to_owned();

        //FIXME: we should have another way than indexes in ALL_CIPHERSUITES,
        //but rustls does not export the static SupportedCipherSuite instances yet
        if !config.rustls_cipher_list.is_empty() {
            let mut ciphers = Vec::new();
            for cipher in config.rustls_cipher_list.iter() {
                match cipher.as_str() {
                    "TLS13_CHACHA20_POLY1305_SHA256" => ciphers.push(ALL_CIPHERSUITES[0]),
                    "TLS13_AES_256_GCM_SHA384" => ciphers.push(ALL_CIPHERSUITES[1]),
                    "TLS13_AES_128_GCM_SHA256" => ciphers.push(ALL_CIPHERSUITES[2]),
                    "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256" => {
                        ciphers.push(ALL_CIPHERSUITES[3])
                    }
                    "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256" => {
                        ciphers.push(ALL_CIPHERSUITES[4])
                    }
                    "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384" => ciphers.push(ALL_CIPHERSUITES[5]),
                    "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256" => ciphers.push(ALL_CIPHERSUITES[6]),
                    "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384" => ciphers.push(ALL_CIPHERSUITES[7]),
                    "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256" => ciphers.push(ALL_CIPHERSUITES[8]),
                    s => error!("unknown cipher: {:?}", s),
                }
            }
            server_config.ciphersuites = ciphers;
        }

        Listener {
            address: config.address.clone(),
            fronts: Router::new(),
            answers: Rc::new(RefCell::new(HttpAnswers::new(
                &config.answer_404,
                &config.answer_503,
            ))),
            ssl_config: Arc::new(server_config),
            listener: None,
            config,
            resolver,
            token,
            active: false,
        }
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
            server_bind(&self.config.address)
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
                error!("error registering listen socket: {:?}", e);
            }
        } else {
            return None;
        }

        self.listener = listener;
        self.active = true;
        Some(self.token)
    }

    pub fn add_https_front(&mut self, tls_front: HttpFrontend) -> bool {
        self.fronts.add_http_front(tls_front)
    }

    pub fn remove_https_front(&mut self, tls_front: HttpFrontend) -> bool {
        debug!("removing tls_front {:?}", tls_front);
        self.fronts.remove_http_front(tls_front)
    }

    fn accept(&mut self) -> Result<TcpStream, AcceptError> {
        if let Some(ref listener) = self.listener.as_ref() {
            listener
                .accept()
                .map_err(|e| match e.kind() {
                    ErrorKind::WouldBlock => AcceptError::WouldBlock,
                    _ => {
                        error!("accept() IO error: {:?}", e);
                        AcceptError::IoError
                    }
                })
                .map(|(frontend_sock, _)| frontend_sock)
        } else {
            Err(AcceptError::IoError)
        }
    }

    // ToDo factor out with http.rs
    pub fn frontend_from_request(&self, host: &str, uri: &str, method: &Method) -> Option<Route> {
        let host: &str = if let Ok((i, (hostname, _))) = hostname_and_port(host.as_bytes()) {
            if i != &b""[..] {
                error!("invalid remaining chars after hostname");
                return None;
            }

            // it is alright to call from_utf8_unchecked,
            // we already verified that there are only ascii
            // chars in there
            unsafe { from_utf8_unchecked(hostname) }
        } else {
            error!("hostname parsing failed for: '{}'", host);
            return None;
        };

        self.fronts.lookup(host.as_bytes(), uri.as_bytes(), method)
    }
}

pub struct Proxy {
    pub listeners: HashMap<Token, Listener>,
    pub clusters: HashMap<ClusterId, Cluster>,
    pub backends: Rc<RefCell<BackendMap>>,
    pool: Rc<RefCell<Pool>>,
    pub registry: Registry,
    pub sessions: Rc<RefCell<SessionManager>>,
}

impl Proxy {
    pub fn new(
        registry: Registry,
        sessions: Rc<RefCell<SessionManager>>,
        pool: Rc<RefCell<Pool>>,
        backends: Rc<RefCell<BackendMap>>,
    ) -> Proxy {
        Proxy {
            listeners: HashMap::new(),
            clusters: HashMap::new(),
            backends,
            pool,
            registry,
            sessions,
        }
    }

    pub fn add_listener(&mut self, config: HttpsListener, token: Token) -> Option<Token> {
        if self.listeners.contains_key(&token) {
            None
        } else {
            let listener = Listener::new(config, token);
            self.listeners.insert(listener.token, listener);
            Some(token)
        }
    }

    pub fn remove_listener(&mut self, address: SocketAddr) -> bool {
        let len = self.listeners.len();
        self.listeners.retain(|_, l| l.address != address);

        self.listeners.len() < len
    }

    pub fn activate_listener(
        &mut self,
        addr: &SocketAddr,
        tcp_listener: Option<TcpListener>,
    ) -> Option<Token> {
        for listener in self.listeners.values_mut() {
            if &listener.address == addr {
                return listener.activate(&self.registry, tcp_listener);
            }
        }
        None
    }

    pub fn give_back_listeners(&mut self) -> Vec<(SocketAddr, TcpListener)> {
        self.listeners
            .values_mut()
            .filter(|l| l.listener.is_some())
            .map(|l| (l.address, l.listener.take().unwrap()))
            .collect()
    }

    pub fn give_back_listener(&mut self, address: SocketAddr) -> Option<(Token, TcpListener)> {
        self.listeners
            .values_mut()
            .find(|l| l.address == address)
            .and_then(|l| l.listener.take().map(|sock| (l.token, sock)))
    }

    pub fn add_cluster(&mut self, mut cluster: Cluster) {
        if let Some(answer_503) = cluster.answer_503.take() {
            for l in self.listeners.values_mut() {
                l.answers
                    .borrow_mut()
                    .add_custom_answer(&cluster.cluster_id, &answer_503);
            }
        }

        self.clusters.insert(cluster.cluster_id.clone(), cluster);
    }

    pub fn remove_cluster(&mut self, cluster_id: &str) {
        self.clusters.remove(cluster_id);

        for l in self.listeners.values_mut() {
            l.answers.borrow_mut().remove_custom_answer(cluster_id);
        }
    }
}

impl ProxyConfiguration<Session> for Proxy {
    fn accept(&mut self, token: ListenToken) -> Result<TcpStream, AcceptError> {
        self.listeners.get_mut(&Token(token.0)).unwrap().accept()
    }

    fn create_session(
        &mut self,
        mut frontend_sock: TcpStream,
        token: ListenToken,
        wait_time: Duration,
        proxy: Rc<RefCell<Self>>,
    ) -> Result<(), AcceptError> {
        if let Some(ref listener) = self.listeners.get(&Token(token.0)) {
            if let Err(e) = frontend_sock.set_nodelay(true) {
                error!(
                    "error setting nodelay on front socket({:?}): {:?}",
                    frontend_sock, e
                );
            }

            let mut s = self.sessions.borrow_mut();
            let entry = s.slab.vacant_entry();
            let session_token = Token(entry.key());

            if let Err(e) = self.registry.register(
                &mut frontend_sock,
                session_token,
                Interest::READABLE | Interest::WRITABLE,
            ) {
                error!(
                    "error registering fron socket({:?}): {:?}",
                    frontend_sock, e
                );
            }

            let session = ServerSession::new(&listener.ssl_config);
            let c = Session::new(
                session,
                frontend_sock,
                session_token,
                Rc::downgrade(&self.pool),
                proxy,
                listener
                    .config
                    .public_address
                    .unwrap_or(listener.config.address),
                listener.config.expect_proxy,
                listener.config.sticky_name.clone(),
                listener.answers.clone(),
                Token(token.0),
                wait_time,
                Duration::seconds(listener.config.front_timeout as i64),
                Duration::seconds(listener.config.back_timeout as i64),
                Duration::seconds(listener.config.request_timeout as i64),
            );

            let session = Rc::new(RefCell::new(c));
            entry.insert(session);

            s.incr();
            Ok(())
        } else {
            //FIXME
            Err(AcceptError::IoError)
        }
    }

    fn notify(&mut self, message: ProxyRequest) -> ProxyResponse {
        //info!("{} notified", message);
        match message.order {
            ProxyRequestData::AddCluster(cluster) => {
                debug!("{} add cluster {:?}", message.id, cluster);
                self.add_cluster(cluster);
                ProxyResponse {
                    id: message.id,
                    status: ProxyResponseStatus::Ok,
                    data: None,
                }
            }
            ProxyRequestData::RemoveCluster { cluster_id } => {
                debug!("{} remove cluster {:?}", message.id, cluster_id);
                self.remove_cluster(&cluster_id);
                ProxyResponse {
                    id: message.id,
                    status: ProxyResponseStatus::Ok,
                    data: None,
                }
            }
            ProxyRequestData::AddHttpsFrontend(front) => {
                //info!("HTTPS\t{} add front {:?}", id, front);
                if let Some(listener) = self
                    .listeners
                    .values_mut()
                    .find(|l| l.address == front.address)
                {
                    listener.add_https_front(front);
                    ProxyResponse {
                        id: message.id,
                        status: ProxyResponseStatus::Ok,
                        data: None,
                    }
                } else {
                    panic!("unknown listener: {:?}", front.address)
                }
            }
            ProxyRequestData::RemoveHttpsFrontend(front) => {
                //info!("HTTPS\t{} remove front {:?}", id, front);
                if let Some(listener) = self
                    .listeners
                    .values_mut()
                    .find(|l| l.address == front.address)
                {
                    listener.remove_https_front(front);
                    ProxyResponse {
                        id: message.id,
                        status: ProxyResponseStatus::Ok,
                        data: None,
                    }
                } else {
                    panic!("unknown listener: {:?}", front.address)
                }
            }
            ProxyRequestData::AddCertificate(add_certificate) => {
                if let Some(listener) = self
                    .listeners
                    .values_mut()
                    .find(|l| l.address == add_certificate.address)
                {
                    match listener.add_certificate(&add_certificate) {
                        Ok(_) => ProxyResponse {
                            id: message.id,
                            status: ProxyResponseStatus::Ok,
                            data: None,
                        },
                        Err(err) => ProxyResponse {
                            id: message.id,
                            status: ProxyResponseStatus::Error(err.to_string()),
                            data: None,
                        },
                    }
                } else {
                    error!("replacing certificate to unknown listener");
                    ProxyResponse {
                        id: message.id,
                        status: ProxyResponseStatus::Error(String::from("unsupported message")),
                        data: None,
                    }
                }
            }
            ProxyRequestData::RemoveCertificate(remove_certificate) => {
                //FIXME: should return an error if certificate still has fronts referencing it
                if let Some(listener) = self
                    .listeners
                    .values_mut()
                    .find(|l| l.address == remove_certificate.address)
                {
                    match listener.remove_certificate(&remove_certificate) {
                        Ok(_) => ProxyResponse {
                            id: message.id,
                            status: ProxyResponseStatus::Ok,
                            data: None,
                        },
                        Err(err) => ProxyResponse {
                            id: message.id,
                            status: ProxyResponseStatus::Error(err.to_string()),
                            data: None,
                        },
                    }
                } else {
                    error!("replacing certificate to unknown listener");
                    ProxyResponse {
                        id: message.id,
                        status: ProxyResponseStatus::Error(String::from("unsupported message")),
                        data: None,
                    }
                }
            }
            ProxyRequestData::ReplaceCertificate(replace_certificate) => {
                //FIXME: should return an error if certificate still has fronts referencing it
                if let Some(listener) = self
                    .listeners
                    .values_mut()
                    .find(|l| l.address == replace_certificate.address)
                {
                    match listener.replace_certificate(&replace_certificate) {
                        Ok(_) => ProxyResponse {
                            id: message.id,
                            status: ProxyResponseStatus::Ok,
                            data: None,
                        },
                        Err(err) => ProxyResponse {
                            id: message.id,
                            status: ProxyResponseStatus::Error(err.to_string()),
                            data: None,
                        },
                    }
                } else {
                    error!("replacing certificate to unknown listener");
                    ProxyResponse {
                        id: message.id,
                        status: ProxyResponseStatus::Error(String::from("unsupported message")),
                        data: None,
                    }
                }
            }
            ProxyRequestData::RemoveListener(remove) => {
                debug!("removing HTTPS listener at address: {:?}", remove.address);
                if !self.remove_listener(remove.address) {
                    ProxyResponse {
                        id: message.id,
                        status: ProxyResponseStatus::Error(format!(
                            "no HTTPS listener to remove at address {:?}",
                            remove.address
                        )),
                        data: None,
                    }
                } else {
                    ProxyResponse {
                        id: message.id,
                        status: ProxyResponseStatus::Ok,
                        data: None,
                    }
                }
            }
            ProxyRequestData::SoftStop => {
                info!("{} processing soft shutdown", message.id);
                let mut listeners: HashMap<_, _> = self.listeners.drain().collect();
                for (_, l) in listeners.iter_mut() {
                    l.listener.take().map(|mut sock| {
                        if let Err(e) = self.registry.deregister(&mut sock) {
                            error!("error deregistering listen socket: {:?}", e);
                        }
                    });
                }
                ProxyResponse {
                    id: message.id,
                    status: ProxyResponseStatus::Processing,
                    data: None,
                }
            }
            ProxyRequestData::HardStop => {
                info!("{} hard shutdown", message.id);
                let mut listeners: HashMap<_, _> = self.listeners.drain().collect();
                for (_, mut l) in listeners.drain() {
                    l.listener.take().map(|mut sock| {
                        if let Err(e) = self.registry.deregister(&mut sock) {
                            error!("error deregistering listen socket: {:?}", e);
                        }
                    });
                }
                ProxyResponse {
                    id: message.id,
                    status: ProxyResponseStatus::Processing,
                    data: None,
                }
            }
            ProxyRequestData::Status => {
                debug!("{} status", message.id);
                ProxyResponse {
                    id: message.id,
                    status: ProxyResponseStatus::Ok,
                    data: None,
                }
            }
            ProxyRequestData::Logging(logging_filter) => {
                debug!(
                    "{} changing logging filter to {}",
                    message.id, logging_filter
                );
                logging::LOGGER.with(|l| {
                    let directives = logging::parse_logging_spec(&logging_filter);
                    l.borrow_mut().set_directives(directives);
                });
                ProxyResponse {
                    id: message.id,
                    status: ProxyResponseStatus::Ok,
                    data: None,
                }
            }
            ProxyRequestData::Query(Query::Certificates(QueryCertificateType::All)) => {
                let res = self
                    .listeners
                    .iter()
                    .map(|(_addr, listener)| {
                        let mut domains =
                            (&unwrap_msg!(listener.resolver.0.lock()).domains).to_hashmap();
                        let res = domains
                            .drain()
                            .map(|(k, v)| (String::from_utf8(k).unwrap(), v.0))
                            .collect();

                        (listener.address, res)
                    })
                    .collect::<HashMap<_, _>>();

                ProxyResponse {
                    id: message.id,
                    status: ProxyResponseStatus::Ok,
                    data: Some(ProxyResponseData::Query(QueryAnswer::Certificates(
                        QueryAnswerCertificate::All(res),
                    ))),
                }
            }
            ProxyRequestData::Query(Query::Certificates(QueryCertificateType::Domain(d))) => {
                let res = self
                    .listeners
                    .iter()
                    .map(|(_addr, listener)| {
                        let resolver = &unwrap_msg!(listener.resolver.0.lock());
                        (
                            listener.address,
                            resolver.domain_lookup(d.as_bytes(), true).map(|(k, v)| {
                                (String::from_utf8(k.to_vec()).unwrap(), v.0.clone())
                            }),
                        )
                    })
                    .collect::<HashMap<_, _>>();

                ProxyResponse {
                    id: message.id,
                    status: ProxyResponseStatus::Ok,
                    data: Some(ProxyResponseData::Query(QueryAnswer::Certificates(
                        QueryAnswerCertificate::Domain(res),
                    ))),
                }
            }
            command => {
                error!(
                    "{} unsupported message for rustls proxy, ignoring {:?}",
                    message.id, command
                );
                ProxyResponse {
                    id: message.id,
                    status: ProxyResponseStatus::Error(String::from("unsupported message")),
                    data: None,
                }
            }
        }
    }
}

use crate::server::HttpsProvider;
pub fn start(config: HttpsListener, channel: ProxyChannel, max_buffers: usize, buffer_size: usize) {
    use crate::server;

    let event_loop = Poll::new().expect("could not create event loop");

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

    let address = config.address.clone();
    let sessions = SessionManager::new(sessions, max_buffers);
    let registry = event_loop.registry().try_clone().unwrap();
    let mut configuration = Proxy::new(registry, sessions.clone(), pool.clone(), backends.clone());
    if configuration.add_listener(config, token).is_some()
        && configuration.activate_listener(&address, None).is_some()
    {
        let (scm_server, _scm_client) = UnixStream::pair().unwrap();
        let mut server_config: server::ServerConfig = Default::default();
        server_config.max_connections = max_buffers;
        let mut server = Server::new(
            event_loop,
            channel,
            ScmSocket::new(scm_server.as_raw_fd()),
            sessions,
            pool,
            backends,
            None,
            Some(HttpsProvider::Rustls(Rc::new(RefCell::new(configuration)))),
            None,
            server_config,
            None,
            false,
        );

        info!("starting event loop");
        server.run();
        info!("ending event loop");
    }
}
