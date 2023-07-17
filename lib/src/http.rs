use std::{
    cell::RefCell,
    collections::{hash_map::Entry, BTreeMap, HashMap},
    io::ErrorKind,
    net::{Shutdown, SocketAddr},
    os::unix::io::{AsRawFd, IntoRawFd},
    rc::{Rc, Weak},
    str::from_utf8_unchecked,
};

use anyhow::{bail, Context};
use mio::{net::*, unix::SourceFd, *};
use rusty_ulid::Ulid;
use slab::Slab;
use time::{Duration, Instant};

use sozu_command::{
    logging,
    proto::command::{
        request::RequestType, Cluster, HttpListenerConfig, ListenerType, RemoveListener,
        RequestHttpFrontend,
    },
    ready::Ready,
    request::WorkerRequest,
    response::{HttpFrontend, WorkerResponse},
    scm_socket::{Listeners, ScmSocket},
    state::ClusterId,
};

use crate::{
    protocol::SessionState, router::Router, timer::TimeoutContainer, util::UnwrapLog, CachedTags,
    L7ListenerHandler, L7Proxy, ListenerHandler, SessionIsToBeClosed, SessionResult,
    StateMachineBuilder,
};

use super::{
    backends::BackendMap,
    pool::Pool,
    protocol::{
        http::{
            answers::HttpAnswers,
            parser::{hostname_and_port, Method},
        },
        proxy_protocol::expect::ExpectProxyProtocol,
        {Http, Pipe},
    },
    router::Route,
    server::{ListenSession, ListenToken, ProxyChannel, Server, SessionManager},
    socket::server_bind,
    AcceptError, Protocol, ProxyConfiguration, ProxySession, Readiness, SessionMetrics,
    StateResult,
};

#[derive(PartialEq, Eq)]
pub enum SessionStatus {
    Normal,
    DefaultAnswer,
}

StateMachineBuilder! {
    /// The various Stages of an HTTP connection:
    ///
    /// 1. optional (ExpectProxyProtocol)
    /// 2. HTTP
    /// 3. WebSocket (passthrough)
    enum HttpStateMachine impl SessionState {
        Expect(ExpectProxyProtocol<TcpStream>),
        Http(Http<TcpStream, HttpListener>),
        WebSocket(Pipe<TcpStream, HttpListener>),
    }
}

/// HTTP Session to insert in the SessionManager
///
/// 1 session <=> 1 HTTP connection (client to sozu)
pub struct HttpSession {
    answers: Rc<RefCell<HttpAnswers>>,
    configured_backend_timeout: Duration,
    configured_connect_timeout: Duration,
    configured_frontend_timeout: Duration,
    frontend_token: Token,
    last_event: Instant,
    listener: Rc<RefCell<HttpListener>>,
    metrics: SessionMetrics,
    pool: Weak<RefCell<Pool>>,
    proxy: Rc<RefCell<HttpProxy>>,
    state: HttpStateMachine,
    sticky_name: String,
    has_been_closed: bool,
}

impl HttpSession {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        answers: Rc<RefCell<HttpAnswers>>,
        configured_backend_timeout: Duration,
        configured_connect_timeout: Duration,
        configured_frontend_timeout: Duration,
        configured_request_timeout: Duration,
        expect_proxy: bool,
        listener: Rc<RefCell<HttpListener>>,
        pool: Weak<RefCell<Pool>>,
        proxy: Rc<RefCell<HttpProxy>>,
        public_address: SocketAddr,
        sock: TcpStream,
        sticky_name: String,
        token: Token,
        wait_time: Duration,
    ) -> Result<Self, AcceptError> {
        let request_id = Ulid::generate();
        let container_frontend_timeout = TimeoutContainer::new(configured_request_timeout, token);

        let state = if expect_proxy {
            trace!("starting in expect proxy state");
            gauge_add!("protocol.proxy.expect", 1);

            HttpStateMachine::Expect(ExpectProxyProtocol::new(
                container_frontend_timeout,
                sock,
                token,
                request_id,
            ))
        } else {
            gauge_add!("protocol.http", 1);
            let session_address = sock.peer_addr().ok();

            HttpStateMachine::Http(Http::new(
                answers.clone(),
                configured_backend_timeout,
                configured_connect_timeout,
                configured_frontend_timeout,
                container_frontend_timeout,
                sock,
                token,
                listener.clone(),
                pool.clone(),
                Protocol::HTTP,
                public_address,
                request_id,
                session_address,
                sticky_name.clone(),
            )?)
        };

        let metrics = SessionMetrics::new(Some(wait_time));
        Ok(HttpSession {
            answers,
            configured_backend_timeout,
            configured_connect_timeout,
            configured_frontend_timeout,
            frontend_token: token,
            has_been_closed: false,
            last_event: Instant::now(),
            listener,
            metrics,
            pool,
            proxy,
            state,
            sticky_name,
        })
    }

    pub fn upgrade(&mut self) -> SessionIsToBeClosed {
        debug!("HTTP::upgrade");
        let new_state = match self.state.take() {
            HttpStateMachine::Http(http) => self.upgrade_http(http),
            HttpStateMachine::Expect(expect) => self.upgrade_expect(expect),
            HttpStateMachine::WebSocket(ws) => self.upgrade_websocket(ws),
            HttpStateMachine::FailedUpgrade(_) => unreachable!(),
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
        expect: ExpectProxyProtocol<TcpStream>,
    ) -> Option<HttpStateMachine> {
        debug!("switching to HTTP");
        match expect
            .addresses
            .as_ref()
            .map(|add| (add.destination(), add.source()))
        {
            Some((Some(public_address), Some(session_address))) => {
                let mut http = Http::new(
                    self.answers.clone(),
                    self.configured_backend_timeout,
                    self.configured_connect_timeout,
                    self.configured_frontend_timeout,
                    expect.container_frontend_timeout,
                    expect.frontend,
                    expect.frontend_token,
                    self.listener.clone(),
                    self.pool.clone(),
                    Protocol::HTTP,
                    public_address,
                    expect.request_id,
                    Some(session_address),
                    self.sticky_name.clone(),
                )
                .ok()?;
                http.frontend_readiness.event = expect.frontend_readiness.event;

                gauge_add!("protocol.proxy.expect", -1);
                gauge_add!("protocol.http", 1);
                Some(HttpStateMachine::Http(http))
            }
            _ => None,
        }
    }

    fn upgrade_http(&mut self, http: Http<TcpStream, HttpListener>) -> Option<HttpStateMachine> {
        debug!("http switching to ws");
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

        gauge_add!("protocol.http", -1);
        gauge_add!("protocol.ws", 1);
        gauge_add!("http.active_requests", -1);
        gauge_add!("websocket.active_requests", 1);
        Some(HttpStateMachine::WebSocket(pipe))
    }

    fn upgrade_websocket(&self, ws: Pipe<TcpStream, HttpListener>) -> Option<HttpStateMachine> {
        // what do we do here?
        error!("Upgrade called on WS, this should not happen");
        Some(HttpStateMachine::WebSocket(ws))
    }
}

impl ProxySession for HttpSession {
    fn close(&mut self) {
        if self.has_been_closed {
            return;
        }

        debug!("Closing HTTP session");
        self.metrics.service_stop();

        // Restore gauges
        match self.state.marker() {
            StateMarker::Expect => gauge_add!("protocol.proxy.expect", -1),
            StateMarker::Http => gauge_add!("protocol.http", -1),
            StateMarker::WebSocket => {
                gauge_add!("protocol.ws", -1);
                gauge_add!("websocket.active_requests", -1);
            }
        }

        if self.state.failed() {
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
                )
            }
        }

        // deregister the frontend and remove it
        let proxy = self.proxy.borrow();
        let fd = front_socket.as_raw_fd();
        if let Err(e) = proxy.registry.deregister(&mut SourceFd(&fd)) {
            error!(
                "error deregistering front socket({:?}) while closing HTTP session: {:?}",
                fd, e
            );
        }
        proxy.remove_session(self.frontend_token);

        // defer backend closing to the state
        self.state.close(self.proxy.clone(), &mut self.metrics);
        self.has_been_closed = true;
    }

    fn timeout(&mut self, token: Token) -> SessionIsToBeClosed {
        let state_result = self.state.timeout(token, &mut self.metrics);
        state_result == StateResult::CloseSession
    }

    fn protocol(&self) -> Protocol {
        Protocol::HTTP
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
        self.state.print_state("HTTP");
        error!("Metrics: {:?}", self.metrics);
    }

    fn frontend_token(&self) -> Token {
        self.frontend_token
    }
}

pub type Hostname = String;

pub struct HttpListener {
    active: bool,
    address: SocketAddr,
    answers: Rc<RefCell<HttpAnswers>>,
    config: HttpListenerConfig,
    fronts: Router,
    listener: Option<TcpListener>,
    tags: BTreeMap<String, CachedTags>,
    token: Token,
}

impl ListenerHandler for HttpListener {
    fn get_addr(&self) -> &SocketAddr {
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

impl L7ListenerHandler for HttpListener {
    fn get_sticky_name(&self) -> &str {
        &self.config.sticky_name
    }

    fn get_connect_timeout(&self) -> u32 {
        self.config.connect_timeout
    }

    // redundant, already called once in extract_route
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

        /*if port == Some(&b"80"[..]) {
        // it is alright to call from_utf8_unchecked,
        // we already verified that there are only ascii
        // chars in there
          unsafe { from_utf8_unchecked(hostname) }
        } else {
          host
        }
        */
        let host = unsafe { from_utf8_unchecked(hostname) };

        self.fronts
            .lookup(host.as_bytes(), uri.as_bytes(), method)
            .with_context(|| "No cluster found")
    }
}

pub struct HttpProxy {
    backends: Rc<RefCell<BackendMap>>,
    clusters: HashMap<ClusterId, Cluster>,
    listeners: HashMap<Token, Rc<RefCell<HttpListener>>>,
    pool: Rc<RefCell<Pool>>,
    registry: Registry,
    sessions: Rc<RefCell<SessionManager>>,
}

impl HttpProxy {
    pub fn new(
        registry: Registry,
        sessions: Rc<RefCell<SessionManager>>,
        pool: Rc<RefCell<Pool>>,
        backends: Rc<RefCell<BackendMap>>,
    ) -> HttpProxy {
        HttpProxy {
            backends,
            clusters: HashMap::new(),
            listeners: HashMap::new(),
            pool,
            registry,
            sessions,
        }
    }

    pub fn add_listener(
        &mut self,
        config: HttpListenerConfig,
        token: Token,
    ) -> anyhow::Result<Token> {
        match self.listeners.entry(token) {
            Entry::Vacant(entry) => {
                let http_listener = HttpListener::new(config, token)
                    .with_context(|| "Could not create http listener")?;
                entry.insert(Rc::new(RefCell::new(http_listener)));
                Ok(token)
            }
            _ => bail!("There is already a listener for this token"),
        }
    }

    pub fn get_listener(&self, token: &Token) -> Option<Rc<RefCell<HttpListener>>> {
        self.listeners.get(token).map(Clone::clone)
    }

    pub fn remove_listener(&mut self, remove: RemoveListener) -> anyhow::Result<()> {
        let len = self.listeners.len();
        self.listeners
            .retain(|_, l| l.borrow().address.to_string() != remove.address);

        if !self.listeners.len() < len {
            info!("no HTTP listener to remove at address {:?}", remove.address);
        }
        Ok(())
    }

    pub fn activate_listener(
        &self,
        addr: &SocketAddr,
        tcp_listener: Option<TcpListener>,
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

    pub fn give_back_listeners(&mut self) -> Vec<(SocketAddr, TcpListener)> {
        self.listeners
            .iter()
            .filter_map(|(_, listener)| {
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

    pub fn add_cluster(&mut self, cluster: Cluster) -> anyhow::Result<()> {
        if let Some(answer_503) = &cluster.answer_503 {
            for listener in self.listeners.values() {
                listener
                    .borrow()
                    .answers
                    .borrow_mut()
                    .add_custom_answer(&cluster.cluster_id, answer_503);
            }
        }
        self.clusters.insert(cluster.cluster_id.clone(), cluster);
        Ok(())
    }

    pub fn remove_cluster(&mut self, cluster_id: &str) -> anyhow::Result<()> {
        self.clusters.remove(cluster_id);

        for listener in self.listeners.values() {
            listener
                .borrow()
                .answers
                .borrow_mut()
                .remove_custom_answer(cluster_id);
        }
        Ok(())
    }

    pub fn add_http_frontend(&mut self, front: RequestHttpFrontend) -> anyhow::Result<()> {
        let front = front.to_frontend()?;

        match self
            .listeners
            .values()
            .find(|l| l.borrow().address == front.address)
        {
            Some(listener) => {
                let mut owned = listener.borrow_mut();

                let hostname = front.hostname.to_owned();
                let tags = front.tags.to_owned();

                match owned.add_http_front(front) {
                    Ok(_) => {
                        owned.set_tags(hostname, tags);
                        Ok(())
                    }
                    Err(err) => Err(anyhow::Error::msg(err)),
                }
            }
            None => bail!("no HTTP listener found for address: {}", front.address),
        }
    }

    pub fn remove_http_frontend(&mut self, front: RequestHttpFrontend) -> anyhow::Result<()> {
        let front = front.to_frontend()?;

        if let Some(listener) = self
            .listeners
            .values()
            .find(|l| l.borrow().address == front.address)
        {
            let mut owned = listener.borrow_mut();
            let hostname = front.hostname.to_owned();

            match owned.remove_http_front(front) {
                Ok(_) => owned.set_tags(hostname, None),
                Err(err) => return Err(anyhow::Error::msg(err)),
            }
        }
        Ok(())
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

    pub fn logging(&mut self, logging_filter: String) -> anyhow::Result<()> {
        logging::LOGGER.with(|l| {
            let directives = logging::parse_logging_spec(&logging_filter);
            l.borrow_mut().set_directives(directives);
        });
        Ok(())
    }
}

impl HttpListener {
    pub fn new(config: HttpListenerConfig, token: Token) -> anyhow::Result<HttpListener> {
        let address = config
            .address
            .parse()
            .with_context(|| "wrong socket address")?;
        Ok(HttpListener {
            active: false,
            address,
            answers: Rc::new(RefCell::new(HttpAnswers::new(
                &config.answer_404,
                &config.answer_503,
            ))),
            config,
            fronts: Router::new(),
            listener: None,
            tags: BTreeMap::new(),
            token,
        })
    }

    pub fn activate(
        &mut self,
        registry: &Registry,
        tcp_listener: Option<TcpListener>,
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

    pub fn add_http_front(&mut self, http_front: HttpFrontend) -> anyhow::Result<()> {
        self.fronts
            .add_http_front(&http_front)
            .with_context(|| format!("Could not add http frontend {http_front:?}"))
    }

    pub fn remove_http_front(&mut self, http_front: HttpFrontend) -> anyhow::Result<()> {
        debug!("removing http_front {:?}", http_front);
        self.fronts
            .remove_http_front(&http_front)
            .with_context(|| format!("Could not remove http frontend {http_front:?}"))
    }

    fn accept(&mut self) -> Result<TcpStream, AcceptError> {
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

impl ProxyConfiguration for HttpProxy {
    fn notify(&mut self, request: WorkerRequest) -> WorkerResponse {
        let request_id = request.id.clone();

        let result = match request.content.request_type {
            Some(RequestType::AddCluster(cluster)) => {
                debug!("{} add cluster {:?}", request.id, cluster);
                self.add_cluster(cluster.clone())
                    .with_context(|| format!("Could not add cluster {}", cluster.cluster_id))
            }
            Some(RequestType::RemoveCluster(cluster_id)) => {
                debug!("{} remove cluster {:?}", request_id, cluster_id);
                self.remove_cluster(&cluster_id)
                    .with_context(|| format!("Could not remove cluster {cluster_id}"))
            }
            Some(RequestType::AddHttpFrontend(front)) => {
                debug!("{} add front {:?}", request_id, front);
                self.add_http_frontend(front)
                    .with_context(|| "Could not add http frontend")
            }
            Some(RequestType::RemoveHttpFrontend(front)) => {
                debug!("{} remove front {:?}", request_id, front);
                self.remove_http_frontend(front)
                    .with_context(|| "Could not remove http frontend")
            }
            Some(RequestType::RemoveListener(remove)) => {
                debug!("removing HTTP listener at address {:?}", remove.address);
                self.remove_listener(remove.clone()).with_context(|| {
                    format!("Could not remove listener at address {:?}", remove.address)
                })
            }
            Some(RequestType::SoftStop(_)) => {
                debug!("{} processing soft shutdown", request_id);
                match self
                    .soft_stop()
                    .with_context(|| "Could not perform soft stop")
                {
                    Ok(()) => {
                        info!("{} soft stop successful", request_id);
                        return WorkerResponse::processing(request.id);
                    }
                    Err(e) => Err(e),
                }
            }
            Some(RequestType::HardStop(_)) => {
                debug!("{} processing hard shutdown", request_id);
                match self
                    .hard_stop()
                    .with_context(|| "Could not perform hard stop")
                {
                    Ok(()) => {
                        info!("{} hard stop successful", request_id);
                        return WorkerResponse::processing(request.id);
                    }
                    Err(e) => Err(e),
                }
            }
            Some(RequestType::Status(_)) => {
                debug!("{} status", request_id);
                Ok(())
            }
            Some(RequestType::Logging(logging_filter)) => {
                debug!(
                    "{} changing logging filter to {}",
                    request_id, logging_filter
                );
                self.logging(logging_filter.clone())
                    .with_context(|| format!("Could not set logging level to {logging_filter}"))
            }
            other_command => {
                debug!(
                    "{} unsupported message for HTTP proxy, ignoring: {:?}",
                    request.id, other_command
                );
                Err(anyhow::Error::msg("unsupported message"))
            }
        };

        match result {
            Ok(()) => {
                debug!("{} successful", request_id);
                WorkerResponse::ok(request_id)
            }
            Err(error_message) => {
                debug!("{} unsuccessful: {:#}", request_id, error_message);
                WorkerResponse::error(request_id, format!("{error_message:#}"))
            }
        }
    }

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
        listener_token: ListenToken,
        wait_time: Duration,
        proxy: Rc<RefCell<Self>>,
    ) -> Result<(), AcceptError> {
        let listener = self
            .listeners
            .get(&Token(listener_token.0))
            .map(Clone::clone)
            .ok_or(AcceptError::IoError)?;

        if let Err(e) = frontend_sock.set_nodelay(true) {
            error!(
                "error setting nodelay on front socket({:?}): {:?}",
                frontend_sock, e
            );
        }
        let mut session_manager = self.sessions.borrow_mut();
        let session_entry = session_manager.slab.vacant_entry();
        let session_token = Token(session_entry.key());
        let owned = listener.borrow();

        if let Err(register_error) = self.registry.register(
            &mut frontend_sock,
            session_token,
            Interest::READABLE | Interest::WRITABLE,
        ) {
            error!(
                "error registering listen socket({:?}): {:?}",
                frontend_sock, register_error
            );
            return Err(AcceptError::RegisterError);
        }

        let public_address: SocketAddr = owned
            .config
            .public_address
            .clone()
            .unwrap_or(owned.config.address.clone())
            .parse()
            .map_err(|_| AcceptError::WrongSocketAddress)?;

        let session = HttpSession::new(
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
            frontend_sock,
            owned.config.sticky_name.clone(),
            session_token,
            wait_time,
        )?;

        let session = Rc::new(RefCell::new(session));
        session_entry.insert(session);

        Ok(())
    }
}

impl L7Proxy for HttpProxy {
    fn kind(&self) -> ListenerType {
        ListenerType::Http
    }

    fn register_socket(
        &self,
        source: &mut TcpStream,
        token: Token,
        interest: Interest,
    ) -> Result<(), std::io::Error> {
        self.registry.register(source, token, interest)
    }

    fn deregister_socket(&self, tcp_stream: &mut TcpStream) -> Result<(), std::io::Error> {
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

/// This is starts an HTTP worker with an HTTP listener config.
/// It activates the Listener automatically.
pub fn start_http_worker(
    config: HttpListenerConfig,
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

    let address = config.address.clone();
    let sessions = SessionManager::new(sessions, max_buffers);
    let registry = event_loop
        .registry()
        .try_clone()
        .with_context(|| "Failed at creating a registry")?;
    let mut proxy = HttpProxy::new(registry, sessions.clone(), pool.clone(), backends.clone());
    let _ = proxy.add_listener(config, token);
    let _ = proxy.activate_listener(
        &address
            .parse()
            .with_context(|| "Could not parse socket address")?,
        None,
    );
    let (scm_server, scm_client) =
        UnixStream::pair().with_context(|| "Failed at creating scm stream sockets")?;
    let client_scm_socket =
        ScmSocket::new(scm_client.into_raw_fd()).with_context(|| "Could not create scm socket")?;
    let server_scm_socket =
        ScmSocket::new(scm_server.as_raw_fd()).with_context(|| "Could not create scm socket")?;

    if let Err(e) = client_scm_socket.send_listeners(&Listeners {
        http: Vec::new(),
        tls: Vec::new(),
        tcp: Vec::new(),
    }) {
        error!("error sending empty listeners: {:?}", e);
    }

    let server_config = server::ServerConfig {
        max_connections: max_buffers,
        ..Default::default()
    };

    let mut server = Server::new(
        event_loop,
        channel,
        server_scm_socket,
        sessions,
        pool,
        backends,
        Some(proxy),
        None,
        None,
        server_config,
        None,
        false,
    )
    .with_context(|| "Failed at creating server")?;

    debug!("starting event loop");
    server.run();
    debug!("ending event loop");
    Ok(())
}

#[cfg(test)]
mod tests {
    extern crate tiny_http;

    use super::*;
    use crate::sozu_command::{
        channel::Channel,
        config::ListenerBuilder,
        proto::command::{LoadBalancingAlgorithms, LoadBalancingParams, PathRule, RulePosition},
        request::WorkerRequest,
        response::{Backend, HttpFrontend},
    };

    use std::{
        io::{Read, Write},
        net::{SocketAddr, TcpStream},
        str,
        str::FromStr,
        sync::{Arc, Barrier},
        thread,
        time::Duration,
    };

    /*
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn size_test() {
      assert_size!(ExpectProxyProtocol<mio::net::TcpStream>, 520);
      assert_size!(Http<mio::net::TcpStream>, 1232);
      assert_size!(Pipe<mio::net::TcpStream>, 272);
      assert_size!(State, 1240);
      // fails depending on the platform?
      assert_size!(Session, 1592);
    }
    */

    #[test]
    fn round_trip() {
        setup_test_logger!();
        let barrier = Arc::new(Barrier::new(2));
        start_server(1025, barrier.clone());
        barrier.wait();

        let config = ListenerBuilder::new_http("127.0.0.1:1024")
            .to_http(None)
            .expect("could not create listener config");

        let (mut command, channel) =
            Channel::generate(1000, 10000).expect("should create a channel");
        let _jg = thread::spawn(move || {
            setup_test_logger!();
            start_http_worker(config, channel, 10, 16384).expect("could not start the http server");
        });

        let front = RequestHttpFrontend {
            cluster_id: Some(String::from("cluster_1")),
            address: "127.0.0.1:1024".to_string(),
            hostname: String::from("localhost"),
            path: PathRule::prefix(String::from("/")),
            ..Default::default()
        };
        command
            .write_message(&WorkerRequest {
                id: String::from("ID_ABCD"),
                content: RequestType::AddHttpFrontend(front).into(),
            })
            .unwrap();
        let backend = Backend {
            cluster_id: String::from("cluster_1"),
            backend_id: String::from("cluster_1-0"),
            address: "127.0.0.1:1025".parse().unwrap(),
            load_balancing_parameters: Some(LoadBalancingParams::default()),
            sticky_id: None,
            backup: None,
        };
        command
            .write_message(&WorkerRequest {
                id: String::from("ID_EFGH"),
                content: RequestType::AddBackend(backend.to_add_backend()).into(),
            })
            .unwrap();

        println!("test received: {:?}", command.read_message());
        println!("test received: {:?}", command.read_message());

        let mut client = TcpStream::connect(("127.0.0.1", 1024)).expect("could not parse address");

        // 5 seconds of timeout
        client.set_read_timeout(Some(Duration::new(1, 0))).unwrap();
        let w = client
            .write(&b"GET / HTTP/1.1\r\nHost: localhost:1024\r\nConnection: Close\r\n\r\n"[..]);
        println!("http client write: {w:?}");

        barrier.wait();
        let mut buffer = [0; 4096];
        let mut index = 0;

        loop {
            assert!(index <= 191);
            if index == 191 {
                break;
            }

            let r = client.read(&mut buffer[index..]);
            println!("http client read: {r:?}");
            match r {
                Err(e) => assert!(false, "client request should not fail. Error: {e:?}"),
                Ok(sz) => {
                    index += sz;
                }
            }
        }
        println!(
            "Response: {}",
            str::from_utf8(&buffer[..index]).expect("could not make string from buffer")
        );
    }

    #[test]
    fn keep_alive() {
        setup_test_logger!();
        let barrier = Arc::new(Barrier::new(2));
        start_server(1028, barrier.clone());
        barrier.wait();

        let config = ListenerBuilder::new_http("127.0.0.1:1031")
            .to_http(None)
            .expect("could not create listener config");

        let (mut command, channel) =
            Channel::generate(1000, 10000).expect("should create a channel");

        let _jg = thread::spawn(move || {
            setup_test_logger!();
            start_http_worker(config, channel, 10, 16384).expect("could not start the http server");
        });

        let front = RequestHttpFrontend {
            address: "127.0.0.1:1031".to_string(),
            hostname: String::from("localhost"),
            path: PathRule::prefix(String::from("/")),
            cluster_id: Some(String::from("cluster_1")),
            ..Default::default()
        };
        command
            .write_message(&WorkerRequest {
                id: String::from("ID_ABCD"),
                content: RequestType::AddHttpFrontend(front).into(),
            })
            .unwrap();
        let backend = Backend {
            address: "127.0.0.1:1028".parse().unwrap(),
            backend_id: String::from("cluster_1-0"),
            backup: None,
            cluster_id: String::from("cluster_1"),
            load_balancing_parameters: Some(LoadBalancingParams::default()),
            sticky_id: None,
        };
        command
            .write_message(&WorkerRequest {
                id: String::from("ID_EFGH"),
                content: RequestType::AddBackend(backend.to_add_backend()).into(),
            })
            .unwrap();

        println!("test received: {:?}", command.read_message());
        println!("test received: {:?}", command.read_message());

        let mut client = TcpStream::connect(("127.0.0.1", 1031)).expect("could not parse address");
        // 5 seconds of timeout
        client.set_read_timeout(Some(Duration::new(5, 0))).unwrap();

        let w = client
            .write(&b"GET / HTTP/1.1\r\nHost: localhost:1031\r\n\r\n"[..])
            .unwrap();
        println!("http client write: {w:?}");
        barrier.wait();

        let mut buffer = [0; 4096];
        let mut index = 0;

        loop {
            assert!(index <= 191);
            if index == 191 {
                break;
            }

            let r = client.read(&mut buffer[index..]);
            println!("http client read: {r:?}");
            match r {
                Err(e) => assert!(false, "client request should not fail. Error: {e:?}"),
                Ok(sz) => {
                    index += sz;
                }
            }
        }

        println!(
            "Response: {}",
            str::from_utf8(&buffer[..index]).expect("could not make string from buffer")
        );

        println!("first request ended, will send second one");
        let w2 = client.write(&b"GET / HTTP/1.1\r\nHost: localhost:1031\r\n\r\n"[..]);
        println!("http client write: {w2:?}");
        barrier.wait();

        let mut buffer2 = [0; 4096];
        let mut index = 0;

        loop {
            assert!(index <= 191);
            if index == 191 {
                break;
            }

            let r2 = client.read(&mut buffer2[index..]);
            println!("http client read: {r2:?}");
            match r2 {
                Err(e) => assert!(false, "client request should not fail. Error: {e:?}"),
                Ok(sz) => {
                    index += sz;
                }
            }
        }
        println!(
            "Response: {}",
            str::from_utf8(&buffer2[..index]).expect("could not make string from buffer")
        );
    }

    #[test]
    fn https_redirect() {
        setup_test_logger!();

        let config = ListenerBuilder::new_http("127.0.0.1:1041")
            .to_http(None)
            .expect("could not create listener config");

        let (mut command, channel) =
            Channel::generate(1000, 10000).expect("should create a channel");
        let _jg = thread::spawn(move || {
            setup_test_logger!();
            start_http_worker(config, channel, 10, 16384).expect("could not start the http server");
        });

        let cluster = Cluster {
            cluster_id: String::from("cluster_1"),
            https_redirect: true,
            load_balancing: LoadBalancingAlgorithms::default() as i32,
            sticky_session: false,
            ..Default::default()
        };
        command
            .write_message(&WorkerRequest {
                id: String::from("ID_ABCD"),
                content: RequestType::AddCluster(cluster).into(),
            })
            .unwrap();
        let front = RequestHttpFrontend {
            address: "127.0.0.1:1041".to_string(),
            hostname: String::from("localhost"),
            path: PathRule::prefix(String::from("/")),
            cluster_id: Some(String::from("cluster_1")),
            ..Default::default()
        };
        command
            .write_message(&WorkerRequest {
                id: String::from("ID_EFGH"),
                content: RequestType::AddHttpFrontend(front).into(),
            })
            .unwrap();
        let backend = Backend {
            address: "127.0.0.1:1040".parse().unwrap(),
            backend_id: String::from("cluster_1-0"),
            backup: None,
            cluster_id: String::from("cluster_1"),
            load_balancing_parameters: Some(LoadBalancingParams::default()),
            sticky_id: None,
        };
        command
            .write_message(&WorkerRequest {
                id: String::from("ID_IJKL"),
                content: RequestType::AddBackend(backend.to_add_backend()).into(),
            })
            .unwrap();

        println!("test received: {:?}", command.read_message());
        println!("test received: {:?}", command.read_message());
        println!("test received: {:?}", command.read_message());

        let mut client = TcpStream::connect(("127.0.0.1", 1041)).expect("could not parse address");
        // 5 seconds of timeout
        client.set_read_timeout(Some(Duration::new(5, 0))).unwrap();

        let w = client.write(
            &b"GET /redirected?true HTTP/1.1\r\nHost: localhost\r\nConnection: Close\r\n\r\n"[..],
        );
        println!("http client write: {w:?}");

        let expected_answer = "HTTP/1.1 301 Moved Permanently\r\nContent-Length: 0\r\nLocation: https://localhost/redirected?true\r\n\r\n";
        let mut buffer = [0; 4096];
        let mut index = 0;
        loop {
            assert!(index <= expected_answer.len());
            if index == expected_answer.len() {
                break;
            }

            let r = client.read(&mut buffer[index..]);
            println!("http client read: {r:?}");
            match r {
                Err(e) => assert!(false, "Failed to read client stream. Error: {e:?}"),
                Ok(sz) => {
                    index += sz;
                }
            }
        }

        let answer = str::from_utf8(&buffer[..index]).expect("could not make string from buffer");
        println!("Response: {answer}");
        assert_eq!(answer, expected_answer);
    }

    use self::tiny_http::{Response, Server};

    fn start_server(port: u16, barrier: Arc<Barrier>) {
        thread::spawn(move || {
            setup_test_logger!();
            let server =
                Server::http(&format!("127.0.0.1:{port}")).expect("could not create server");
            info!("starting web server in port {}", port);
            barrier.wait();

            for request in server.incoming_requests() {
                info!(
                    "backend web server got request -> method: {:?}, url: {:?}, headers: {:?}",
                    request.method(),
                    request.url(),
                    request.headers()
                );

                let response = Response::from_string("hello world");
                request.respond(response).unwrap();
                info!("backend web server sent response");
                barrier.wait();
                info!("server session stopped");
            }

            println!("server on port {port} closed");
        });
    }

    #[test]
    fn frontend_from_request_test() {
        let cluster_id1 = "cluster_1".to_owned();
        let cluster_id2 = "cluster_2".to_owned();
        let cluster_id3 = "cluster_3".to_owned();
        let uri1 = "/".to_owned();
        let uri2 = "/yolo".to_owned();
        let uri3 = "/yolo/swag".to_owned();

        let mut fronts = Router::new();
        fronts
            .add_http_front(&HttpFrontend {
                address: "0.0.0.0:80".parse().unwrap(),
                hostname: "lolcatho.st".to_owned(),
                method: None,
                path: PathRule::prefix(uri1),
                position: RulePosition::Tree,
                cluster_id: Some(cluster_id1),
                tags: None,
            })
            .expect("Could not add http frontend");
        fronts
            .add_http_front(&HttpFrontend {
                address: "0.0.0.0:80".parse().unwrap(),
                hostname: "lolcatho.st".to_owned(),
                method: None,
                path: PathRule::prefix(uri2),
                position: RulePosition::Tree,
                cluster_id: Some(cluster_id2),
                tags: None,
            })
            .expect("Could not add http frontend");
        fronts
            .add_http_front(&HttpFrontend {
                address: "0.0.0.0:80".parse().unwrap(),
                hostname: "lolcatho.st".to_owned(),
                method: None,
                path: PathRule::prefix(uri3),
                position: RulePosition::Tree,
                cluster_id: Some(cluster_id3),
                tags: None,
            })
            .expect("Could not add http frontend");
        fronts
            .add_http_front(&HttpFrontend {
                address: "0.0.0.0:80".parse().unwrap(),
                hostname: "other.domain".to_owned(),
                method: None,
                path: PathRule::prefix("/test".to_owned()),
                position: RulePosition::Tree,
                cluster_id: Some("cluster_1".to_owned()),
                tags: None,
            })
            .expect("Could not add http frontend");

        let address: SocketAddr =
            FromStr::from_str("127.0.0.1:1030").expect("could not parse address");

        let default_config = ListenerBuilder::new_http(address)
            .to_http(None)
            .expect("Could not create default HTTP listener config");

        let listener = HttpListener {
            listener: None,
            address,
            fronts,
            answers: Rc::new(RefCell::new(HttpAnswers::new(
                "HTTP/1.1 404 Not Found\r\n\r\n",
                "HTTP/1.1 503 Service Unavailable\r\n\r\n",
            ))),
            config: default_config,
            token: Token(0),
            active: true,
            tags: BTreeMap::new(),
        };

        let frontend1 = listener.frontend_from_request("lolcatho.st", "/", &Method::Get);
        let frontend2 = listener.frontend_from_request("lolcatho.st", "/test", &Method::Get);
        let frontend3 = listener.frontend_from_request("lolcatho.st", "/yolo/test", &Method::Get);
        let frontend4 = listener.frontend_from_request("lolcatho.st", "/yolo/swag", &Method::Get);
        let frontend5 = listener.frontend_from_request("domain", "/", &Method::Get);
        assert_eq!(
            frontend1.expect("should find frontend"),
            Route::ClusterId("cluster_1".to_string())
        );
        assert_eq!(
            frontend2.expect("should find frontend"),
            Route::ClusterId("cluster_1".to_string())
        );
        assert_eq!(
            frontend3.expect("should find frontend"),
            Route::ClusterId("cluster_2".to_string())
        );
        assert_eq!(
            frontend4.expect("should find frontend"),
            Route::ClusterId("cluster_3".to_string())
        );
        assert!(frontend5.is_err());
    }
}
