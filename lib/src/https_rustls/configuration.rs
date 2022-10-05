use std::{
    cell::RefCell,
    collections::{hash_map::Entry, BTreeMap, HashMap},
    io::ErrorKind,
    net::SocketAddr,
    os::unix::io::AsRawFd,
    rc::Rc,
    str::from_utf8_unchecked,
    sync::Arc,
};

use anyhow::Context;
use mio::{net::*, *};
use rustls::{cipher_suite::*, ServerConfig, ServerConnection};
use slab::Slab;
use time::Duration;

use crate::{
    backends::BackendMap,
    pool::Pool,
    protocol::http::{
        answers::HttpAnswers,
        parser::{hostname_and_port, Method},
    },
    router::Router,
    server::{ListenSession, ListenToken, ProxyChannel, Server, SessionManager, SessionToken},
    socket::server_bind,
    sozu_command::{
        logging,
        proxy::{
            AddCertificate, CertificateFingerprint, Cluster, HttpFrontend, HttpsListener,
            ProxyRequest, ProxyRequestOrder, ProxyResponse, ProxyResponseContent,
            ProxyResponseStatus, Query, QueryAnswer, QueryAnswerCertificate, QueryCertificateType,
            RemoveCertificate, Route, TlsVersion,
        },
        scm_socket::ScmSocket,
    },
    tls::{
        CertificateResolver, GenericCertificateResolverError, MutexWrappedCertificateResolver,
        ParsedCertificateAndKey,
    },
    util::UnwrapLog,
    ListenerHandler, {AcceptError, ClusterId, Protocol, ProxyConfiguration, ProxySession},
};

use super::session::Session;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsCluster {
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

impl Listener {
    pub fn new(config: HttpsListener, token: Token) -> Result<Listener, rustls::Error> {
        let server_config = ServerConfig::builder();
        let server_config = if !config.rustls_cipher_list.is_empty() {
            let mut ciphers = Vec::new();
            for cipher in config.rustls_cipher_list.iter() {
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

        Ok(Listener {
            address: config.address,
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
            tags: BTreeMap::new(),
        })
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
        if let Some(listener) = self.listener.as_ref() {
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
    pub listeners: HashMap<Token, Rc<RefCell<Listener>>>,
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

    pub fn add_listener(
        &mut self,
        config: HttpsListener,
        token: Token,
    ) -> Result<Option<Token>, rustls::Error> {
        match self.listeners.entry(token) {
            Entry::Vacant(entry) => {
                entry.insert(Rc::new(RefCell::new(Listener::new(config, token)?)));
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

    pub fn add_cluster(&mut self, mut cluster: Cluster) {
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
    }

    pub fn remove_cluster(&mut self, cluster_id: &str) {
        self.clusters.remove(cluster_id);

        for listener in self.listeners.values() {
            listener
                .borrow()
                .answers
                .borrow_mut()
                .remove_custom_answer(cluster_id);
        }
    }
}

impl ProxyConfiguration<Session> for Proxy {
    fn accept(&mut self, token: ListenToken) -> Result<TcpStream, AcceptError> {
        if let Some(listener) = self.listeners.get(&Token(token.0)) {
            listener.borrow_mut().accept()
        } else {
            Err(AcceptError::IoError)
        }
    }

    fn create_session(
        &mut self,
        mut frontend_sock: TcpStream,
        token: ListenToken,
        wait_time: Duration,
        proxy: Rc<RefCell<Self>>,
    ) -> Result<(), AcceptError> {
        let listener = self
            .listeners
            .get(&Token(token.0))
            .ok_or(AcceptError::IoError)?;

        let owned = listener.borrow();

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

        let session = match ServerConnection::new(owned.ssl_config.clone()) {
            Ok(session) => session,
            Err(e) => {
                error!("failed to create server session: {:?}", e);
                return Err(AcceptError::IoError);
            }
        };
        let c = Session::new(
            session,
            frontend_sock,
            session_token,
            Rc::downgrade(&self.pool),
            proxy,
            owned.config.public_address.unwrap_or(owned.config.address),
            owned.config.expect_proxy,
            owned.config.sticky_name.clone(),
            owned.answers.clone(),
            Token(token.0),
            wait_time,
            Duration::seconds(owned.config.front_timeout as i64),
            Duration::seconds(owned.config.back_timeout as i64),
            Duration::seconds(owned.config.request_timeout as i64),
            listener.clone(),
        );

        let session = Rc::new(RefCell::new(c));
        entry.insert(session);

        s.incr();
        Ok(())
    }

    fn notify(&mut self, message: ProxyRequest) -> ProxyResponse {
        //info!("{} notified", message);
        match message.order {
            ProxyRequestOrder::AddCluster(cluster) => {
                debug!("{} add cluster {:?}", message.id, cluster);
                self.add_cluster(cluster);
                ProxyResponse::ok(message.id)
            }
            ProxyRequestOrder::RemoveCluster { cluster_id } => {
                debug!("{} remove cluster {:?}", message.id, cluster_id);
                self.remove_cluster(&cluster_id);
                ProxyResponse::ok(message.id)
            }
            ProxyRequestOrder::AddHttpsFrontend(front) => {
                //info!("HTTPS\t{} add front {:?}", id, front);
                if let Some(listener) = self
                    .listeners
                    .values()
                    .find(|l| l.borrow().address == front.address)
                {
                    let mut owned = listener.borrow_mut();

                    owned.add_https_front(front.clone());
                    owned.set_tags(front.hostname, front.tags);

                    ProxyResponse::ok(message.id)
                } else {
                    ProxyResponse::error(
                        message.id,
                        format!("No listener found for the frontend {:?}", front),
                    )
                }
            }
            ProxyRequestOrder::RemoveHttpsFrontend(front) => {
                //info!("HTTPS\t{} remove front {:?}", id, front);
                if let Some(listener) = self
                    .listeners
                    .values()
                    .find(|l| l.borrow().address == front.address)
                {
                    let mut owned = listener.borrow_mut();

                    owned.remove_https_front(front.clone());
                    owned.set_tags(front.hostname, None);

                    ProxyResponse::ok(message.id)
                } else {
                    ProxyResponse::error(
                        message.id,
                        format!("No listener found for the frontend {:?}", front),
                    )
                }
            }
            ProxyRequestOrder::AddCertificate(add_certificate) => {
                if let Some(listener) = self
                    .listeners
                    .values()
                    .find(|l| l.borrow().address == add_certificate.address)
                {
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
                if let Some(listener) = self
                    .listeners
                    .values_mut()
                    .find(|l| l.borrow().address == remove_certificate.address)
                {
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
                if let Some(listener) = self
                    .listeners
                    .values()
                    .find(|l| l.borrow().address == replace_certificate.address)
                {
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
            ProxyRequestOrder::RemoveListener(remove) => {
                debug!("removing HTTPS listener at address: {:?}", remove.address);
                if !self.remove_listener(remove.address) {
                    ProxyResponse::error(
                        message.id,
                        format!(
                            "no HTTPS listener to remove at address {:?}",
                            remove.address
                        ),
                    )
                } else {
                    ProxyResponse::ok(message.id)
                }
            }
            ProxyRequestOrder::SoftStop => {
                info!("{} processing soft shutdown", message.id);
                let listeners: HashMap<_, _> = self.listeners.drain().collect();
                for (_, l) in listeners.iter() {
                    if let Some(mut sock) = l.borrow_mut().listener.take() {
                        if let Err(e) = self.registry.deregister(&mut sock) {
                            error!("error deregistering listen socket: {:?}", e);
                        }
                    };
                }
                ProxyResponse::processing(message.id)
            }
            ProxyRequestOrder::HardStop => {
                info!("{} hard shutdown", message.id);
                let listeners: HashMap<_, _> = self.listeners.drain().collect();
                for (_, l) in listeners.iter() {
                    if let Some(mut sock) = l.borrow_mut().listener.take() {
                        if let Err(e) = self.registry.deregister(&mut sock) {
                            error!("error deregistering listen socket: {:?}", e);
                        }
                    };
                }
                ProxyResponse::processing(message.id)
            }
            ProxyRequestOrder::Status => {
                debug!("{} status", message.id);
                ProxyResponse::ok(message.id)
            }
            ProxyRequestOrder::Logging(logging_filter) => {
                debug!(
                    "{} changing logging filter to {}",
                    message.id, logging_filter
                );
                logging::LOGGER.with(|l| {
                    let directives = logging::parse_logging_spec(&logging_filter);
                    l.borrow_mut().set_directives(directives);
                });
                ProxyResponse::ok(message.id)
            }
            ProxyRequestOrder::Query(Query::Certificates(QueryCertificateType::All)) => {
                let res = self
                    .listeners
                    .iter()
                    .map(|(_addr, listener)| {
                        let owned = listener.borrow();
                        let mut domains = unwrap_msg!(owned.resolver.0.lock()).domains.to_hashmap();
                        let res = domains
                            .drain()
                            .map(|(k, v)| (String::from_utf8(k).unwrap(), v.0))
                            .collect();

                        (owned.address, res)
                    })
                    .collect::<HashMap<_, _>>();

                ProxyResponse {
                    id: message.id,
                    status: ProxyResponseStatus::Ok,
                    content: Some(ProxyResponseContent::Query(QueryAnswer::Certificates(
                        QueryAnswerCertificate::All(res),
                    ))),
                }
            }
            ProxyRequestOrder::Query(Query::Certificates(QueryCertificateType::Domain(d))) => {
                let res = self
                    .listeners
                    .iter()
                    .map(|(_addr, listener)| {
                        let owned = listener.borrow();
                        let resolver = &unwrap_msg!(owned.resolver.0.lock());
                        (
                            owned.address,
                            resolver.domain_lookup(d.as_bytes(), true).map(|(k, v)| {
                                (String::from_utf8(k.to_vec()).unwrap(), v.0.clone())
                            }),
                        )
                    })
                    .collect::<HashMap<_, _>>();

                ProxyResponse {
                    id: message.id,
                    status: ProxyResponseStatus::Ok,
                    content: Some(ProxyResponseContent::Query(QueryAnswer::Certificates(
                        QueryAnswerCertificate::Domain(res),
                    ))),
                }
            }
            command => {
                error!(
                    "{} unsupported message for rustls proxy, ignoring {:?}",
                    message.id, command
                );
                ProxyResponse::error(message.id, "unsupported message")
            }
        }
    }
}

use crate::server::HttpsProvider;
/// This is not directly used by Sōzu but is available for example and testing purposes
pub fn start(
    config: HttpsListener,
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

    let address = config.address;
    let sessions = SessionManager::new(sessions, max_buffers);
    let registry = event_loop
        .registry()
        .try_clone()
        .with_context(|| "Could not clone the mio registry")?;
    let mut configuration = Proxy::new(registry, sessions.clone(), pool.clone(), backends.clone());
    if configuration
        .add_listener(config, token)
        .with_context(|| "failed to create listener")?
        .is_some()
        && configuration.activate_listener(&address, None).is_some()
    {
        let (scm_server, _scm_client) = UnixStream::pair().unwrap();
        let server_config = server::ServerConfig {
            max_connections: max_buffers,
            ..Default::default()
        };

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
        )
        .with_context(|| "Failed at creating server")?;

        info!("starting event loop");
        server.run();
        info!("ending event loop");
    }
    Ok(())
}
