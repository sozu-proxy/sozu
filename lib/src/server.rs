//! event loop management
use mio::net::*;
use mio::*;
use slab::Slab;
use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::convert::TryFrom;
use std::net::SocketAddr;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::rc::Rc;
use time::{Duration, Instant};

use crate::sozu_command::channel::Channel;
use crate::sozu_command::config::Config;
use crate::sozu_command::proxy::{
    HttpsListener, ListenerType, MessageId, ProxyEvent, ProxyRequest, ProxyRequestData,
    ProxyResponse, ProxyResponseData, ProxyResponseStatus, Query, QueryAnswer,
    QueryAnswerCertificate, QueryApplicationType, QueryCertificateType, TlsProvider, Topic,
};
use crate::sozu_command::ready::Ready;
use crate::sozu_command::scm_socket::{Listeners, ScmSocket};
use crate::sozu_command::state::{get_application_ids_by_domain, get_certificate, ConfigState};

use crate::backends::BackendMap;
use crate::features::FEATURES;
use crate::metrics::METRICS;
use crate::pool::Pool;
use crate::timer::Timer;
use crate::{http, tcp};
use crate::{AcceptError, Backend, Protocol, ProxyConfiguration, ProxySession};

// Number of retries to perform on a server after a connection failure
pub const CONN_RETRIES: u8 = 3;

pub type ProxyChannel = Channel<ProxyResponse, ProxyRequest>;

thread_local! {
  pub static QUEUE: RefCell<VecDeque<ProxyResponse>> = RefCell::new(VecDeque::new());
}

thread_local! {
  pub static TIMER: RefCell<Timer<Token>> = RefCell::new(Timer::default());
}

pub fn push_queue(message: ProxyResponse) {
    QUEUE.with(|queue| {
        (*queue.borrow_mut()).push_back(message);
    });
}

pub fn push_event(event: ProxyEvent) {
    QUEUE.with(|queue| {
        (*queue.borrow_mut()).push_back(ProxyResponse {
            id: "EVENT".to_string(),
            status: ProxyResponseStatus::Processing,
            data: Some(ProxyResponseData::Event(event)),
        });
    });
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ListenToken(pub usize);
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionToken(pub usize);

impl From<usize> for ListenToken {
    fn from(val: usize) -> ListenToken {
        ListenToken(val)
    }
}

impl From<ListenToken> for usize {
    fn from(val: ListenToken) -> usize {
        val.0
    }
}

impl From<usize> for SessionToken {
    fn from(val: usize) -> SessionToken {
        SessionToken(val)
    }
}

impl From<SessionToken> for usize {
    fn from(val: SessionToken) -> usize {
        val.0
    }
}

pub struct ServerConfig {
    pub max_connections: usize,
    pub front_timeout: u32,
    pub back_timeout: u32,
    pub connect_timeout: u32,
    pub zombie_check_interval: u32,
    pub accept_queue_timeout: u32,
}

impl ServerConfig {
    pub fn from_config(config: &Config) -> ServerConfig {
        ServerConfig {
            max_connections: config.max_connections,
            front_timeout: config.front_timeout,
            back_timeout: config.back_timeout,
            connect_timeout: config.connect_timeout,
            zombie_check_interval: config.zombie_check_interval,
            accept_queue_timeout: config.accept_queue_timeout,
        }
    }

    fn slab_capacity(&self) -> usize {
        10 + 2 * self.max_connections
    }
}

impl Default for ServerConfig {
    fn default() -> ServerConfig {
        ServerConfig {
            max_connections: 10000,
            front_timeout: 60,
            back_timeout: 30,
            connect_timeout: 3,
            zombie_check_interval: 30 * 60,
            accept_queue_timeout: 60,
        }
    }
}

pub struct SessionManager {
    pub max_connections: usize,
    pub nb_connections: usize,
    pub can_accept: bool,
    pub slab: Slab<Rc<RefCell<dyn ProxySession>>>,
}

impl SessionManager {
    pub fn new(
        slab: Slab<Rc<RefCell<dyn ProxySession>>>,
        max_connections: usize,
    ) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(SessionManager {
            max_connections,
            nb_connections: 0,
            can_accept: true,
            slab,
        }))
    }

    pub fn slab_capacity(&self) -> usize {
        10 + 2 * self.max_connections
    }

    pub fn check_limits(&mut self) -> bool {
        if self.nb_connections == self.max_connections {
            error!("max number of session connection reached, flushing the accept queue");
            gauge!("accept_queue.backpressure", 1);
            self.can_accept = false;
            return false;
        }

        if self.slab.len() >= self.slab_capacity() {
            error!("not enough memory to accept another session, flushing the accept queue");
            error!(
                "nb_connections: {}, max_connections: {}",
                self.nb_connections, self.max_connections
            );
            gauge!("accept_queue.backpressure", 1);
            self.can_accept = false;

            return false;
        }

        true
    }

    pub fn to_session(token: Token) -> SessionToken {
        SessionToken(token.0)
    }

    pub fn incr(&mut self) {
        self.nb_connections += 1;
        assert!(self.nb_connections <= self.max_connections);
        gauge!("client.connections", self.nb_connections);
    }

    pub fn close_session(&mut self, token: SessionToken) {
        if self.slab.contains(token.0) {
            let session = self.slab.remove(token.0);
            session.borrow_mut().close();

            assert!(self.nb_connections != 0);
            self.nb_connections -= 1;
            gauge!("client.connections", self.nb_connections);
        }

        // do not be ready to accept right away, wait until we get back to 10% capacity
        if !self.can_accept && self.nb_connections < self.max_connections * 90 / 100 {
            debug!(
                "nb_connections = {}, max_connections = {}, starting to accept again",
                self.nb_connections, self.max_connections
            );
            gauge!("accept_queue.backpressure", 0);
            self.can_accept = true;
        }
    }

    pub fn close_session_tokens(&mut self, tokens: Vec<Token>) {
        for tk in tokens.into_iter() {
            let cl = Self::to_session(tk);
            if self.slab.contains(cl.0) {
                self.slab.remove(cl.0);
            }
        }
    }

    pub fn check_can_accept(&mut self) {
        assert!(self.nb_connections != 0);
        self.nb_connections -= 1;
        gauge!("client.connections", self.nb_connections);

        // do not be ready to accept right away, wait until we get back to 10% capacity
        if !self.can_accept && self.nb_connections < self.max_connections * 90 / 100 {
            debug!(
                "nb_connections = {}, max_connections = {}, starting to accept again",
                self.nb_connections, self.max_connections
            );
            gauge!("accept_queue.backpressure", 0);
            self.can_accept = true;
        }
    }
}

/// `Server` handles the event loop, the listeners, the sessions and
/// communication with the configuration channel.
///
/// A listener wraps a listen socket, the associated proxying protocols
/// (HTTP, HTTPS and TCP) and the routing configuration for applications.
/// Listeners handle creating sessions from accepted sockets.
///
/// A session manages a "front" socket for a connected client, and all
/// of the associated data (back socket, protocol state machine, buffers,
/// metrics...).
///
/// `Server` gets configuration updates from the channel (domIN/path routes,
/// backend server address...).
///
/// Listeners and sessions are all stored in a slab structure to index them
/// by a [Token], they all have to implement the [ProxySession] trait.
pub struct Server {
    pub poll: Poll,
    shutting_down: Option<MessageId>,
    accept_ready: HashSet<ListenToken>,
    channel: ProxyChannel,
    http: Rc<RefCell<http::Proxy>>,
    https: HttpsProvider,
    tcp: Rc<RefCell<tcp::Proxy>>,
    config_state: ConfigState,
    scm: ScmSocket,
    sessions: Rc<RefCell<SessionManager>>,
    pool: Rc<RefCell<Pool>>,
    backends: Rc<RefCell<BackendMap>>,
    scm_listeners: Option<Listeners>,
    zombie_check_interval: Duration,
    accept_queue: VecDeque<(TcpStream, ListenToken, Protocol, Instant)>,
    accept_queue_timeout: Duration,
    base_sessions_count: usize,
}

impl Server {
    pub fn new_from_config(
        channel: ProxyChannel,
        scm: ScmSocket,
        config: Config,
        config_state: ConfigState,
        expects_initial_status: bool,
    ) -> Self {
        let event_loop = Poll::new().expect("could not create event loop");
        let pool = Rc::new(RefCell::new(Pool::with_capacity(
            config.min_buffers,
            config.max_buffers,
            config.buffer_size,
        )));
        let backends = Rc::new(RefCell::new(BackendMap::new()));
        let server_config = ServerConfig::from_config(&config);

        //FIXME: we will use a few entries for the channel, metrics socket and the listeners
        //FIXME: for HTTP/2, we will have more than 2 entries per session
        let sessions: Rc<RefCell<SessionManager>> = SessionManager::new(
            Slab::with_capacity(server_config.slab_capacity()),
            server_config.max_connections,
        );
        {
            let mut s = sessions.borrow_mut();
            let entry = s.slab.vacant_entry();
            trace!("taking token {:?} for channel", SessionToken(entry.key()));
            entry.insert(Rc::new(RefCell::new(ListenSession {
                protocol: Protocol::Channel,
            })));
        }
        {
            let mut s = sessions.borrow_mut();
            let entry = s.slab.vacant_entry();
            trace!("taking token {:?} for metrics", SessionToken(entry.key()));
            entry.insert(Rc::new(RefCell::new(ListenSession {
                protocol: Protocol::Timer,
            })));
        }
        {
            let mut s = sessions.borrow_mut();
            let entry = s.slab.vacant_entry();
            trace!("taking token {:?} for metrics", SessionToken(entry.key()));
            entry.insert(Rc::new(RefCell::new(ListenSession {
                protocol: Protocol::Metrics,
            })));
        }

        let use_openssl = config.tls_provider == TlsProvider::Openssl;
        let registry = event_loop
            .registry()
            .try_clone()
            .expect("could not clone the mio Registry");
        let https = HttpsProvider::new(
            use_openssl,
            registry,
            sessions.clone(),
            pool.clone(),
            backends.clone(),
        );

        Server::new(
            event_loop,
            channel,
            scm,
            sessions,
            pool,
            backends,
            None,
            Some(https),
            None,
            server_config,
            Some(config_state),
            expects_initial_status,
        )
    }

    pub fn new(
        poll: Poll,
        mut channel: ProxyChannel,
        scm: ScmSocket,
        sessions: Rc<RefCell<SessionManager>>,
        pool: Rc<RefCell<Pool>>,
        backends: Rc<RefCell<BackendMap>>,
        http: Option<http::Proxy>,
        https: Option<HttpsProvider>,
        tcp: Option<tcp::Proxy>,
        server_config: ServerConfig,
        config_state: Option<ConfigState>,
        expects_initial_status: bool,
    ) -> Self {
        FEATURES.with(|_features| {
            // initializing feature flags
        });

        poll.registry()
            .register(
                &mut channel,
                Token(0),
                Interest::READABLE | Interest::WRITABLE,
            )
            .expect("should register the channel");

        METRICS.with(|metrics| {
            if let Some(sock) = (*metrics.borrow_mut()).socket_mut() {
                poll.registry()
                    .register(sock, Token(2), Interest::WRITABLE)
                    .expect("should register the metrics socket");
            }
        });

        let base_sessions_count = sessions.borrow().slab.len();

        let http = Rc::new(RefCell::new(http.unwrap_or_else(|| {
            let registry = poll
                .registry()
                .try_clone()
                .expect("could not clone the mio Registry");
            http::Proxy::new(registry, sessions.clone(), pool.clone(), backends.clone())
        })));
        let https = https.unwrap_or_else(|| {
            let registry = poll
                .registry()
                .try_clone()
                .expect("could not clone the mio Registry");
            HttpsProvider::new(
                false,
                registry,
                sessions.clone(),
                pool.clone(),
                backends.clone(),
            )
        });
        let tcp = Rc::new(RefCell::new(tcp.unwrap_or_else(|| {
            let registry = poll
                .registry()
                .try_clone()
                .expect("could not clone the mio Registry");
            tcp::Proxy::new(registry, sessions.clone(), backends.clone())
        })));

        let mut server = Server {
            poll,
            shutting_down: None,
            accept_ready: HashSet::new(),
            channel,
            http,
            https,
            tcp,
            config_state: ConfigState::new(),
            scm,
            sessions,
            scm_listeners: None,
            pool,
            backends,
            zombie_check_interval: Duration::seconds(i64::from(
                server_config.zombie_check_interval,
            )),
            accept_queue: VecDeque::new(),
            accept_queue_timeout: Duration::seconds(i64::from(server_config.accept_queue_timeout)),
            base_sessions_count,
        };

        // initialize the worker with the state we got from a file
        if let Some(state) = config_state {
            let mut counter = 0usize;

            for order in state.generate_orders() {
                let id = format!("INIT-{}", counter);
                let message = ProxyRequest { id, order };

                trace!("generating initial config order: {:#?}", message);
                server.notify_proxys(message);

                counter += 1;
            }
            // do not send back answers to the initialization messages
            QUEUE.with(|queue| {
                (*queue.borrow_mut()).clear();
            });
        }

        if expects_initial_status {
            // the main process sends a Status message, so we can notify it
            // when the initial state is loaded
            server.channel.set_nonblocking(false);
            let msg = server.channel.read_message();
            debug!("got message: {:?}", msg);
            server.channel.write_message(&ProxyResponse {
                id: msg.unwrap().id,
                status: ProxyResponseStatus::Ok,
                data: None,
            });
            server.channel.set_nonblocking(true);
        }

        info!("will try to receive listeners");
        server.scm.set_blocking(true);
        let listeners = server.scm.receive_listeners();
        server.scm.set_blocking(false);
        info!("received listeners: {:?}", listeners);
        server.scm_listeners = listeners;

        server
    }
}

impl Server {
    pub fn run(&mut self) {
        //FIXME: make those parameters configurable?
        let mut events = Events::with_capacity(1024);
        let poll_timeout = Some(Duration::milliseconds(1000));
        let max_poll_errors = 10000;
        let mut current_poll_errors = 0;
        let mut last_zombie_check = Instant::now();
        let mut last_sessions_len = self.sessions.borrow().slab.len();
        let mut should_poll_at: Option<Instant> = None;
        let mut last_shutting_down_message = None;

        let mut loop_start = Instant::now();
        loop {
            let now = Instant::now();
            time!("event_loop_time", (now - loop_start).whole_milliseconds());
            loop_start = now;

            if current_poll_errors == max_poll_errors {
                error!(
                    "Something is going very wrong. Last {} poll() calls failed, crashing..",
                    current_poll_errors
                );
                panic!("poll() calls failed {} times in a row", current_poll_errors);
            }

            let timeout = match should_poll_at.as_ref() {
                None => poll_timeout,
                Some(i) => {
                    if *i <= now {
                        poll_timeout
                    } else {
                        let dur = *i - now;
                        match poll_timeout {
                            None => Some(dur),
                            Some(t) => {
                                if t < dur {
                                    Some(t)
                                } else {
                                    Some(dur)
                                }
                            }
                        }
                    }
                }
            };

            if let Err(error) = self.poll.poll(
                &mut events,
                timeout.and_then(|t| std::time::Duration::try_from(t).ok()),
            ) {
                error!("Error while polling events: {:?}", error);
                current_poll_errors += 1;
                continue;
            } else {
                current_poll_errors = 0;
            }
            let after_epoll = Instant::now();
            time!(
                "epoll_time",
                (after_epoll - loop_start).whole_milliseconds()
            );
            loop_start = after_epoll;

            self.send_queue();

            for event in events.iter() {
                if event.token() == Token(0) {
                    if event.is_error() {
                        error!("error reading from command channel");
                        continue;
                    }
                    if event.is_read_closed() || event.is_write_closed() {
                        error!("command channel was closed");
                        continue;
                    }
                    let ready = Ready::from(event);
                    self.channel.handle_events(ready);

                    // loop here because iterations has borrow issues
                    loop {
                        QUEUE.with(|queue| {
                            if !(*queue.borrow()).is_empty() {
                                self.channel.interest.insert(Ready::writable());
                            }
                        });

                        //trace!("WORKER[{}] channel readiness={:?}, interest={:?}, queue={} elements",
                        //  line!(), self.channel.readiness, self.channel.interest, self.queue.len());
                        if self.channel.readiness() == Ready::empty() {
                            break;
                        }

                        if self.channel.readiness().is_readable() {
                            if let Err(e) = self.channel.readable() {
                                error!("error reading from channel: {:?}", e);
                            }

                            loop {
                                let msg = self.channel.read_message();

                                // if the message was too large, we grow the buffer and retry to read if possible
                                if msg.is_none() {
                                    if (self.channel.interest & self.channel.readiness)
                                        .is_readable()
                                    {
                                        if let Err(e) = self.channel.readable() {
                                            error!("error reading from channel: {:?}", e);
                                        }
                                        continue;
                                    } else {
                                        break;
                                    }
                                }

                                let msg = msg.expect("the message should be valid");
                                if let ProxyRequestData::HardStop = msg.order {
                                    let id_msg = msg.id.clone();
                                    self.notify(msg);
                                    self.channel.write_message(&ProxyResponse {
                                        id: id_msg,
                                        status: ProxyResponseStatus::Ok,
                                        data: None,
                                    });
                                    self.channel.run();
                                    return;
                                } else if let ProxyRequestData::SoftStop = msg.order {
                                    self.shutting_down = Some(msg.id.clone());
                                    last_sessions_len = self.sessions.borrow().slab.len();
                                    self.notify(msg);
                                } else if let ProxyRequestData::ReturnListenSockets = msg.order {
                                    info!("received ReturnListenSockets order");
                                    self.return_listen_sockets();
                                } else {
                                    self.notify(msg);
                                }
                            }
                        }

                        QUEUE.with(|queue| {
                            if !(*queue.borrow()).is_empty() {
                                self.channel.interest.insert(Ready::writable());
                            }
                        });

                        self.send_queue();
                    }
                } else if event.token() == Token(1) {
                    while let Some(t) = TIMER.with(|timer| timer.borrow_mut().poll()) {
                        self.timeout(t);
                    }
                } else if event.token() == Token(2) {
                    METRICS.with(|metrics| {
                        (*metrics.borrow_mut()).writable();
                    });
                } else {
                    self.ready(event.token(), Ready::from(event));
                }
            }

            if let Some(t) = should_poll_at.as_ref() {
                if *t <= Instant::now() {
                    while let Some(t) = TIMER.with(|timer| timer.borrow_mut().poll()) {
                        //info!("polled for timeout: {:?}", t);
                        self.timeout(t);
                    }
                }
            }
            self.handle_remaining_readiness();
            self.create_sessions();

            should_poll_at = TIMER.with(|timer| timer.borrow().next_poll_date().map(|i| i.into()));

            let now = Instant::now();
            if now - last_zombie_check > self.zombie_check_interval {
                info!("zombie check");
                last_zombie_check = now;

                clear_ssl_error();

                let mut tokens = HashSet::new();
                let mut frontend_tokens = HashSet::new();

                let mut count = 0;
                let duration = self.zombie_check_interval;
                for (_index, session) in self
                    .sessions
                    .borrow_mut()
                    .slab
                    .iter_mut()
                    .filter(|(_, c)| now - c.borrow().last_event() > duration)
                {
                    let t = session.borrow().tokens();
                    if !frontend_tokens.contains(&t[0]) {
                        session.borrow().print_state();

                        frontend_tokens.insert(t[0]);
                        for tk in t.into_iter() {
                            tokens.insert(tk);
                        }

                        count += 1;
                    }
                }

                for tk in frontend_tokens.iter() {
                    let cl = self.to_session(*tk);
                    self.sessions.borrow_mut().close_session(cl);
                }

                if count > 0 {
                    count!("zombies", count);

                    let mut remaining = 0;
                    for tk in tokens.into_iter() {
                        let cl = self.to_session(tk);
                        let mut sessions = self.sessions.borrow_mut();
                        if sessions.slab.contains(cl.0) {
                            sessions.slab.remove(cl.0);
                            remaining += 1;
                        }
                    }
                    info!(
                        "removing {} zombies ({} remaining tokens after close)",
                        count, remaining
                    );
                }
            }

            //FIXME: what if we got no event in a while
            let now = time::OffsetDateTime::now_utc();
            if now.second() == 0 {
                // regularly clear local metrics to prevent them from taking too much memory
                METRICS.with(|metrics| {
                    (*metrics.borrow_mut()).clear_local(now);
                });
            }

            gauge!("client.connections", self.sessions.borrow().nb_connections);
            gauge!("slab.count", self.sessions.borrow().slab.len());
            METRICS.with(|metrics| {
                (*metrics.borrow_mut()).send_data();
            });

            if self.shutting_down.is_some() {
                let sessions_count = self.sessions.borrow().slab.len();
                for session in self.sessions.borrow_mut().slab.iter_mut() {
                    session.1.borrow_mut().shutting_down();
                }

                let new_sessions_count = self.sessions.borrow().slab.len();

                if new_sessions_count < sessions_count {
                    let now = Instant::now();
                    if let Some(last) = last_shutting_down_message {
                        if (now - last) > Duration::seconds(5) {
                            info!(
                                "closed {} sessions, {} sessions left, base_sessions_count = {}",
                                sessions_count - new_sessions_count,
                                new_sessions_count,
                                self.base_sessions_count
                            );
                            last_shutting_down_message = Some(now);
                        }
                    } else {
                        last_shutting_down_message = Some(now);
                    }
                }

                if new_sessions_count <= self.base_sessions_count {
                    info!("last session stopped, shutting down!");
                    self.channel.run();
                    self.channel.set_blocking(true);
                    self.channel.write_message(&ProxyResponse {
                        id: self
                            .shutting_down
                            .take()
                            .expect("should have shut down correctly"),
                        status: ProxyResponseStatus::Ok,
                        data: None,
                    });
                    return;
                } else if new_sessions_count < last_sessions_len {
                    info!(
                        "shutting down, {} slab elements remaining (base: {})",
                        new_sessions_count - self.base_sessions_count,
                        self.base_sessions_count
                    );
                    last_sessions_len = new_sessions_count;
                }
            }
        }
    }

    fn send_queue(&mut self) {
        if self.channel.readiness.is_writable() {
            QUEUE.with(|q| {
                let mut queue = q.borrow_mut();
                loop {
                    if let Some(msg) = queue.pop_front() {
                        if !self.channel.write_message(&msg) {
                            queue.push_front(msg);
                        }
                    }

                    if self.channel.back_buf.available_data() > 0 {
                        if let Err(e) = self.channel.writable() {
                            error!("error writing to channel: {:?}", e);
                        }
                    }

                    if !self.channel.readiness.is_writable() {
                        break;
                    }

                    if self.channel.back_buf.available_data() == 0 && queue.len() == 0 {
                        break;
                    }
                }
            });
        }
    }

    fn notify(&mut self, message: ProxyRequest) {
        if let ProxyRequestData::Metrics(ref configuration) = message.order {
            //let id = message.id.clone();
            METRICS.with(|metrics| {
                (*metrics.borrow_mut()).configure(configuration);

                push_queue(ProxyResponse {
                    id: message.id.clone(),
                    status: ProxyResponseStatus::Ok,
                    data: None,
                });
            });
            return;
        }

        if let ProxyRequestData::Query(ref query) = message.order {
            match query {
                &Query::ApplicationsHashes => {
                    push_queue(ProxyResponse {
                        id: message.id.clone(),
                        status: ProxyResponseStatus::Ok,
                        data: Some(ProxyResponseData::Query(QueryAnswer::ApplicationsHashes(
                            self.config_state.hash_state(),
                        ))),
                    });
                    return;
                }
                &Query::Applications(ref query_type) => {
                    let answer = match query_type {
                        &QueryApplicationType::ClusterId(ref cluster_id) => {
                            QueryAnswer::Applications(vec![self
                                .config_state
                                .application_state(cluster_id)])
                        }
                        &QueryApplicationType::Domain(ref domain) => {
                            let cluster_ids = get_application_ids_by_domain(
                                &self.config_state,
                                domain.hostname.clone(),
                                domain.path.clone(),
                            );
                            let answer = cluster_ids
                                .iter()
                                .map(|ref cluster_id| {
                                    self.config_state.application_state(cluster_id)
                                })
                                .collect();

                            QueryAnswer::Applications(answer)
                        }
                    };

                    push_queue(ProxyResponse {
                        id: message.id.clone(),
                        status: ProxyResponseStatus::Ok,
                        data: Some(ProxyResponseData::Query(answer)),
                    });
                    return;
                }
                &Query::Certificates(ref q) => {
                    match q {
                        // forward the query to the TLS implementation
                        QueryCertificateType::Domain(_) => {}
                        // forward the query to the TLS implementation
                        QueryCertificateType::All => {}
                        QueryCertificateType::Fingerprint(f) => {
                            push_queue(ProxyResponse {
                                id: message.id.clone(),
                                status: ProxyResponseStatus::Ok,
                                data: Some(ProxyResponseData::Query(QueryAnswer::Certificates(
                                    QueryAnswerCertificate::Fingerprint(get_certificate(
                                        &self.config_state,
                                        &f,
                                    )),
                                ))),
                            });
                            return;
                        }
                    }
                }
                &Query::Metrics(ref q) => {
                    METRICS.with(|metrics| {
                        let data = (*metrics.borrow_mut()).query(q);

                        push_queue(ProxyResponse {
                            id: message.id.clone(),
                            status: ProxyResponseStatus::Ok,
                            data: Some(ProxyResponseData::Query(QueryAnswer::Metrics(data))),
                        });
                    });
                    return;
                }
            }
        }

        self.notify_proxys(message);
    }

    pub fn notify_proxys(&mut self, message: ProxyRequest) {
        self.config_state.handle_order(&message.order);

        match message {
            ProxyRequest {
                order: ProxyRequestData::AddCluster(ref cluster),
                ..
            } => {
                self.backends
                    .borrow_mut()
                    .set_load_balancing_policy_for_app(
                        &cluster.cluster_id,
                        cluster.load_balancing,
                        cluster.load_metric.clone(),
                    );
                //not returning because the message must still be handled by each proxy
            }
            ProxyRequest {
                ref id,
                order: ProxyRequestData::AddBackend(ref backend),
            } => {
                let new_backend = Backend::new(
                    &backend.backend_id,
                    backend.address,
                    backend.sticky_id.clone(),
                    backend.load_balancing_parameters.clone(),
                    backend.backup,
                );
                self.backends
                    .borrow_mut()
                    .add_backend(&backend.cluster_id, new_backend);

                let answer = ProxyResponse {
                    id: id.to_string(),
                    status: ProxyResponseStatus::Ok,
                    data: None,
                };
                push_queue(answer);
                return;
            }
            ProxyRequest {
                ref id,
                order: ProxyRequestData::RemoveBackend(ref backend),
            } => {
                self.backends
                    .borrow_mut()
                    .remove_backend(&backend.cluster_id, &backend.address);

                let answer = ProxyResponse {
                    id: id.to_string(),
                    status: ProxyResponseStatus::Ok,
                    data: None,
                };
                push_queue(answer);
                return;
            }
            _ => {}
        };

        let topics = message.order.get_topics();

        if topics.contains(&Topic::HttpProxyConfig) {
            match message {
                // special case for AddHttpListener because we need to register a listener
                ProxyRequest {
                    ref id,
                    order: ProxyRequestData::AddHttpListener(ref listener),
                } => {
                    debug!("{} add http listener {:?}", id, listener);

                    if self.sessions.borrow().slab.len() >= self.sessions.borrow().slab_capacity() {
                        push_queue(ProxyResponse {
                            id: id.to_string(),
                            status: ProxyResponseStatus::Error(String::from(
                                "session list is full, cannot add a listener",
                            )),
                            data: None,
                        });
                        return;
                    }

                    let mut s = self.sessions.borrow_mut();
                    let entry = s.slab.vacant_entry();
                    let token = Token(entry.key());

                    let status = if self
                        .http
                        .borrow_mut()
                        .add_listener(listener.clone(), token)
                        .is_some()
                    {
                        entry.insert(Rc::new(RefCell::new(ListenSession {
                            protocol: Protocol::HTTPListen,
                        })));
                        self.base_sessions_count += 1;
                        ProxyResponseStatus::Ok
                    } else {
                        error!("Couldn't add HTTP listener");
                        ProxyResponseStatus::Error(String::from("cannot add HTTP listener"))
                    };

                    let answer = ProxyResponse {
                        id: id.to_string(),
                        status,
                        data: None,
                    };
                    push_queue(answer);
                }
                ProxyRequest {
                    ref id,
                    order: ProxyRequestData::RemoveListener(ref remove),
                } => {
                    if remove.proxy == ListenerType::HTTP {
                        debug!("{} remove http listener {:?}", id, remove);
                        self.base_sessions_count -= 1;
                        push_queue(self.http.borrow_mut().notify(ProxyRequest {
                            id: id.to_string(),
                            order: ProxyRequestData::RemoveListener(remove.clone()),
                        }));
                    }
                }
                ProxyRequest {
                    ref id,
                    order: ProxyRequestData::ActivateListener(ref activate),
                } => {
                    if activate.proxy == ListenerType::HTTP {
                        debug!("{} activate http listener {:?}", id, activate);
                        let listener = self
                            .scm_listeners
                            .as_mut()
                            .and_then(|s| s.get_http(&activate.address))
                            .map(|fd| unsafe { TcpListener::from_raw_fd(fd) });
                        let res = self
                            .http
                            .borrow_mut()
                            .activate_listener(&activate.address, listener);
                        let status = match res {
                            Some(token) => {
                                self.accept(ListenToken(token.0), Protocol::HTTPListen);
                                ProxyResponseStatus::Ok
                            }
                            None => {
                                error!("Couldn't activate HTTP listener");
                                ProxyResponseStatus::Error(String::from(
                                    "cannot activate HTTP listener",
                                ))
                            }
                        };

                        let answer = ProxyResponse {
                            id: id.to_string(),
                            status,
                            data: None,
                        };
                        push_queue(answer);
                    }
                }
                ProxyRequest {
                    ref id,
                    order: ProxyRequestData::DeactivateListener(ref deactivate),
                } => {
                    if deactivate.proxy == ListenerType::HTTP {
                        debug!("{} deactivate http listener {:?}", id, deactivate);
                        let status = match self
                            .http
                            .borrow_mut()
                            .give_back_listener(deactivate.address)
                        {
                            Some((token, mut listener)) => {
                                if let Err(e) = self.poll.registry().deregister(&mut listener) {
                                    error!(
                                        "error deregistering HTTP listen socket({:?}): {:?}",
                                        deactivate, e
                                    );
                                }
                                let mut sessions = self.sessions.borrow_mut();
                                if sessions.slab.contains(token.0) {
                                    sessions.slab.remove(token.0);
                                    info!("removed listen token {:?}", token);
                                }

                                if deactivate.to_scm {
                                    self.scm.set_blocking(false);
                                    let listeners = Listeners {
                                        http: vec![(deactivate.address, listener.as_raw_fd())],
                                        tls: vec![],
                                        tcp: vec![],
                                    };
                                    info!("sending HTTP listener: {:?}", listeners);
                                    let res = self.scm.send_listeners(&listeners);

                                    self.scm.set_blocking(true);

                                    info!("sent HTTP listener: {:?}", res);
                                }
                                ProxyResponseStatus::Ok
                            }
                            None => {
                                error!(
                                    "Couldn't deactivate HTTP listener at address {:?}",
                                    deactivate.address
                                );
                                ProxyResponseStatus::Error(format!(
                                    "cannot deactivate HTTP listener at address {:?}",
                                    deactivate.address
                                ))
                            }
                        };

                        let answer = ProxyResponse {
                            id: id.to_string(),
                            status,
                            data: None,
                        };
                        push_queue(answer);
                    }
                }
                ref m => push_queue(self.http.borrow_mut().notify(m.clone())),
            }
        }
        if topics.contains(&Topic::HttpsProxyConfig) {
            match message {
                // special case for AddHttpListener because we need to register a listener
                ProxyRequest {
                    ref id,
                    order: ProxyRequestData::AddHttpsListener(ref listener),
                } => {
                    debug!("{} add https listener {:?}", id, listener);

                    if self.sessions.borrow().slab.len() >= self.sessions.borrow().slab_capacity() {
                        push_queue(ProxyResponse {
                            id: id.to_string(),
                            status: ProxyResponseStatus::Error(String::from(
                                "session list is full, cannot add a listener",
                            )),
                            data: None,
                        });
                        return;
                    }

                    let mut s = self.sessions.borrow_mut();
                    let entry = s.slab.vacant_entry();
                    let token = Token(entry.key());

                    let status = if self.https.add_listener(listener.clone(), token).is_some() {
                        entry.insert(Rc::new(RefCell::new(ListenSession {
                            protocol: Protocol::HTTPSListen,
                        })));
                        self.base_sessions_count += 1;
                        ProxyResponseStatus::Ok
                    } else {
                        error!("Couldn't add HTTPS listener");
                        ProxyResponseStatus::Error(String::from("cannot add HTTPS listener"))
                    };

                    let answer = ProxyResponse {
                        id: id.to_string(),
                        status,
                        data: None,
                    };
                    push_queue(answer);
                }
                ProxyRequest {
                    ref id,
                    order: ProxyRequestData::RemoveListener(ref remove),
                } => {
                    if remove.proxy == ListenerType::HTTPS {
                        debug!("{} remove https listener {:?}", id, remove);
                        self.base_sessions_count -= 1;
                        push_queue(self.https.notify(ProxyRequest {
                            id: id.to_string(),
                            order: ProxyRequestData::RemoveListener(remove.clone()),
                        }));
                    }
                }
                ProxyRequest {
                    ref id,
                    order: ProxyRequestData::ActivateListener(ref activate),
                } => {
                    if activate.proxy == ListenerType::HTTPS {
                        debug!("{} activate https listener {:?}", id, activate);
                        let listener = self
                            .scm_listeners
                            .as_mut()
                            .and_then(|s| s.get_https(&activate.address))
                            .map(|fd| unsafe { TcpListener::from_raw_fd(fd) });
                        let res = self.https.activate_listener(&activate.address, listener);
                        let status = match res {
                            Some(token) => {
                                self.accept(ListenToken(token.0), Protocol::HTTPSListen);
                                ProxyResponseStatus::Ok
                            }
                            None => {
                                error!("Couldn't activate HTTPS listener");
                                ProxyResponseStatus::Error(String::from(
                                    "cannot activate HTTPS listener",
                                ))
                            }
                        };

                        let answer = ProxyResponse {
                            id: id.to_string(),
                            status,
                            data: None,
                        };
                        push_queue(answer);
                    }
                }
                ProxyRequest {
                    ref id,
                    order: ProxyRequestData::DeactivateListener(ref deactivate),
                } => {
                    if deactivate.proxy == ListenerType::HTTPS {
                        debug!("{} deactivate https listener {:?}", id, deactivate);
                        let status = match self.https.give_back_listener(deactivate.address) {
                            Some((token, mut listener)) => {
                                if let Err(e) = self.poll.registry().deregister(&mut listener) {
                                    error!(
                                        "error deregistering HTTPS listen socket({:?}): {:?}",
                                        deactivate, e
                                    );
                                }
                                if self.sessions.borrow().slab.contains(token.0) {
                                    self.sessions.borrow_mut().slab.remove(token.0);
                                    info!("removed listen token {:?}", token);
                                }

                                if deactivate.to_scm {
                                    self.scm.set_blocking(false);
                                    let listeners = Listeners {
                                        http: vec![],
                                        tls: vec![(deactivate.address, listener.as_raw_fd())],
                                        tcp: vec![],
                                    };
                                    info!("sending HTTPS listener: {:?}", listeners);
                                    let res = self.scm.send_listeners(&listeners);

                                    self.scm.set_blocking(true);

                                    info!("sent HTTPS listener: {:?}", res);
                                }
                                ProxyResponseStatus::Ok
                            }
                            None => {
                                error!(
                                    "Couldn't deactivate HTTPS listener at address {:?}",
                                    deactivate.address
                                );
                                ProxyResponseStatus::Error(format!(
                                    "cannot deactivate HTTPS listener at address {:?}",
                                    deactivate.address
                                ))
                            }
                        };

                        let answer = ProxyResponse {
                            id: id.to_string(),
                            status,
                            data: None,
                        };
                        push_queue(answer);
                    }
                }
                ref m => push_queue(self.https.notify(m.clone())),
            }
        }
        if topics.contains(&Topic::TcpProxyConfig) {
            match message {
                // special case for AddTcpFront because we need to register a listener
                ProxyRequest {
                    id,
                    order: ProxyRequestData::AddTcpListener(listener),
                } => {
                    debug!("{} add tcp listener {:?}", id, listener);

                    if self.sessions.borrow().slab.len() >= self.sessions.borrow().slab_capacity() {
                        push_queue(ProxyResponse {
                            id,
                            status: ProxyResponseStatus::Error(String::from(
                                "session list is full, cannot add a listener",
                            )),
                            data: None,
                        });
                        return;
                    }

                    let mut s = self.sessions.borrow_mut();
                    let entry = s.slab.vacant_entry();
                    let token = Token(entry.key());

                    let status = if self
                        .tcp
                        .borrow_mut()
                        .add_listener(listener.clone(), self.pool.clone(), token)
                        .is_some()
                    {
                        entry.insert(Rc::new(RefCell::new(ListenSession {
                            protocol: Protocol::TCPListen,
                        })));
                        self.base_sessions_count += 1;
                        ProxyResponseStatus::Ok
                    } else {
                        error!("Couldn't add TCP listener");
                        ProxyResponseStatus::Error(String::from("cannot add TCP listener"))
                    };
                    let answer = ProxyResponse {
                        id,
                        status,
                        data: None,
                    };
                    push_queue(answer);
                }
                ProxyRequest {
                    ref id,
                    order: ProxyRequestData::RemoveListener(ref remove),
                } => {
                    if remove.proxy == ListenerType::TCP {
                        debug!("{} remove tcp listener {:?}", id, remove);
                        self.base_sessions_count -= 1;
                        push_queue(self.tcp.borrow_mut().notify(ProxyRequest {
                            id: id.to_string(),
                            order: ProxyRequestData::RemoveListener(remove.clone()),
                        }));
                    }
                }
                ProxyRequest {
                    ref id,
                    order: ProxyRequestData::ActivateListener(ref activate),
                } => {
                    if activate.proxy == ListenerType::TCP {
                        debug!("{} activate tcp listener {:?}", id, activate);
                        let listener = self
                            .scm_listeners
                            .as_mut()
                            .and_then(|s| s.get_tcp(&activate.address))
                            .map(|fd| unsafe { TcpListener::from_raw_fd(fd) });
                        let res = self
                            .tcp
                            .borrow_mut()
                            .activate_listener(&activate.address, listener);
                        let status = match res {
                            Some(token) => {
                                self.accept(ListenToken(token.0), Protocol::TCPListen);
                                ProxyResponseStatus::Ok
                            }
                            None => {
                                error!("Couldn't activate TCP listener");
                                ProxyResponseStatus::Error(String::from(
                                    "cannot activate TCP listener",
                                ))
                            }
                        };

                        let answer = ProxyResponse {
                            id: id.to_string(),
                            status,
                            data: None,
                        };
                        push_queue(answer);
                    }
                }
                ProxyRequest {
                    ref id,
                    order: ProxyRequestData::DeactivateListener(ref deactivate),
                } => {
                    if deactivate.proxy == ListenerType::TCP {
                        debug!("{} deactivate tcp listener {:?}", id, deactivate);
                        let status =
                            match self.tcp.borrow_mut().give_back_listener(deactivate.address) {
                                Some((token, mut listener)) => {
                                    if let Err(e) = self.poll.registry().deregister(&mut listener) {
                                        error!(
                                            "error deregistering TCP listen socket({:?}): {:?}",
                                            deactivate, e
                                        );
                                    }
                                    if self.sessions.borrow().slab.contains(token.0) {
                                        self.sessions.borrow_mut().slab.remove(token.0);
                                        info!("removed listen token {:?}", token);
                                    }

                                    if deactivate.to_scm {
                                        self.scm.set_blocking(false);
                                        let listeners = Listeners {
                                            http: vec![],
                                            tls: vec![],
                                            tcp: vec![(deactivate.address, listener.as_raw_fd())],
                                        };
                                        info!("sending TCP listener: {:?}", listeners);
                                        let res = self.scm.send_listeners(&listeners);

                                        self.scm.set_blocking(true);

                                        info!("sent TCP listener: {:?}", res);
                                    }
                                    ProxyResponseStatus::Ok
                                }
                                None => {
                                    error!(
                                        "Couldn't deactivate TCP listener at address {:?}",
                                        deactivate.address
                                    );
                                    ProxyResponseStatus::Error(format!(
                                        "cannot deactivate TCP listener at address {:?}",
                                        deactivate.address
                                    ))
                                }
                            };

                        let answer = ProxyResponse {
                            id: id.to_string(),
                            status,
                            data: None,
                        };
                        push_queue(answer);
                    }
                }
                m => push_queue(self.tcp.borrow_mut().notify(m)),
            }
        }
    }

    pub fn return_listen_sockets(&mut self) {
        self.scm.set_blocking(false);

        let mut http_listeners = self.http.borrow_mut().give_back_listeners();
        for &mut (_, ref mut sock) in http_listeners.iter_mut() {
            if let Err(e) = self.poll.registry().deregister(sock) {
                error!(
                    "error deregistering HTTP listen socket({:?}): {:?}",
                    sock, e
                );
            }
        }

        let mut https_listeners = self.https.give_back_listeners();
        for &mut (_, ref mut sock) in https_listeners.iter_mut() {
            if let Err(e) = self.poll.registry().deregister(sock) {
                error!(
                    "error deregistering HTTPS listen socket({:?}): {:?}",
                    sock, e
                );
            }
        }

        let mut tcp_listeners = self.tcp.borrow_mut().give_back_listeners();
        for &mut (_, ref mut sock) in tcp_listeners.iter_mut() {
            if let Err(e) = self.poll.registry().deregister(sock) {
                error!("error deregistering TCP listen socket({:?}): {:?}", sock, e);
            }
        }

        // use as_raw_fd because the listeners should be dropped after sending them
        let listeners = Listeners {
            http: http_listeners
                .iter()
                .map(|(addr, listener)| (*addr, listener.as_raw_fd()))
                .collect(),
            tls: https_listeners
                .iter()
                .map(|(addr, listener)| (*addr, listener.as_raw_fd()))
                .collect(),
            tcp: tcp_listeners
                .iter()
                .map(|(addr, listener)| (*addr, listener.as_raw_fd()))
                .collect(),
        };
        info!("sending default listeners: {:?}", listeners);
        let res = self.scm.send_listeners(&listeners);

        self.scm.set_blocking(true);

        info!("sent default listeners: {:?}", res);
    }

    pub fn to_session(&self, token: Token) -> SessionToken {
        SessionToken(token.0)
    }

    pub fn from_session(&self, token: SessionToken) -> Token {
        Token(token.0)
    }

    pub fn accept(&mut self, token: ListenToken, protocol: Protocol) {
        match protocol {
            Protocol::TCPListen => loop {
                match self.tcp.borrow_mut().accept(token) {
                    Ok(sock) => self.accept_queue.push_back((
                        sock,
                        token,
                        Protocol::TCPListen,
                        Instant::now(),
                    )),
                    Err(AcceptError::WouldBlock) => {
                        self.accept_ready.remove(&token);
                        break;
                    }
                    Err(other) => {
                        error!("error accepting TCP sockets: {:?}", other);
                        self.accept_ready.remove(&token);
                        break;
                    }
                }
            },
            Protocol::HTTPListen => loop {
                match self.http.borrow_mut().accept(token) {
                    Ok(sock) => self.accept_queue.push_back((
                        sock,
                        token,
                        Protocol::HTTPListen,
                        Instant::now(),
                    )),
                    Err(AcceptError::WouldBlock) => {
                        self.accept_ready.remove(&token);
                        break;
                    }
                    Err(other) => {
                        error!("error accepting HTTP sockets: {:?}", other);
                        self.accept_ready.remove(&token);
                        break;
                    }
                }
            },
            Protocol::HTTPSListen => loop {
                match self.https.accept(token) {
                    Ok(sock) => self.accept_queue.push_back((
                        sock,
                        token,
                        Protocol::HTTPSListen,
                        Instant::now(),
                    )),
                    Err(AcceptError::WouldBlock) => {
                        self.accept_ready.remove(&token);
                        break;
                    }
                    Err(other) => {
                        error!("error accepting HTTPS sockets: {:?}", other);
                        self.accept_ready.remove(&token);
                        break;
                    }
                }
            },
            _ => panic!("should not call accept() on a HTTP, HTTPS or TCP session"),
        }

        gauge!("accept_queue.count", self.accept_queue.len());
    }

    pub fn create_sessions(&mut self) {
        loop {
            if let Some((sock, token, protocol, timestamp)) = self.accept_queue.pop_back() {
                let wait_time = Instant::now() - timestamp;
                time!("accept_queue.wait_time", wait_time.whole_milliseconds());
                if wait_time > self.accept_queue_timeout {
                    incr!("accept_queue.timeout");
                    continue;
                }

                if !self.sessions.borrow_mut().check_limits() {
                    break;
                }

                //FIXME: check the timestamp
                match protocol {
                    Protocol::TCPListen => {
                        let proxy = self.tcp.clone();
                        if !self
                            .tcp
                            .borrow_mut()
                            .create_session(sock, token, wait_time, proxy)
                            .is_ok()
                        {
                            break;
                        }
                    }
                    Protocol::HTTPListen => {
                        let proxy = self.http.clone();
                        if !self
                            .http
                            .borrow_mut()
                            .create_session(sock, token, wait_time, proxy)
                            .is_ok()
                        {
                            break;
                        }
                    }
                    Protocol::HTTPSListen => {
                        let res = match self.https {
                            #[cfg(feature = "use-openssl")]
                            HttpsProvider::Openssl(ref mut openssl) => {
                                let o = openssl.clone();
                                openssl
                                    .borrow_mut()
                                    .create_session(sock, token, wait_time, o)
                            }
                            HttpsProvider::Rustls(ref mut rustls) => {
                                let r = rustls.clone();
                                rustls
                                    .borrow_mut()
                                    .create_session(sock, token, wait_time, r)
                            }
                        };

                        if !res.is_ok() {
                            break;
                        }
                    }
                    _ => panic!("should not call accept() on a HTTP, HTTPS or TCP session"),
                }
            } else {
                break;
            }
        }

        gauge!("accept_queue.count", self.accept_queue.len());
    }

    pub fn ready(&mut self, token: Token, events: Ready) {
        trace!("PROXY\t{:?} got events: {:?}", token, events);

        let session_token = token.0;
        if self.sessions.borrow().slab.contains(session_token) {
            //info!("sessions contains {:?}", session_token);
            let protocol = self.sessions.borrow().slab[session_token]
                .borrow()
                .protocol();
            //info!("protocol: {:?}", protocol);
            match protocol {
                Protocol::HTTPListen | Protocol::HTTPSListen | Protocol::TCPListen => {
                    //info!("PROTOCOL IS LISTEN");
                    if events.is_readable() {
                        self.accept_ready.insert(ListenToken(token.0));
                        if self.sessions.borrow().can_accept {
                            self.accept(ListenToken(token.0), protocol);
                        }
                        return;
                    }

                    if events.is_writable() {
                        error!(
                            "received writable for listener {:?}, this should not happen",
                            token
                        );
                        return;
                    }

                    if events.is_hup() {
                        error!("should not happen: server {:?} closed", token);
                        return;
                    }

                    unreachable!();
                }
                _ => {}
            }

            self.sessions.borrow_mut().slab[session_token]
                .borrow_mut()
                .process_events(token, events);

            let session = self.sessions.borrow_mut().slab[session_token].clone();
            let session2 = session.clone();
            session.borrow_mut().ready(session2);
        }
    }

    pub fn timeout(&mut self, token: Token) {
        trace!("PROXY\t{:?} got timeout", token);

        let session_token = token.0;
        if self.sessions.borrow().slab.contains(session_token) {
            self.sessions.borrow_mut().slab[session_token]
                .borrow_mut()
                .timeout(token);
        }
    }

    pub fn handle_remaining_readiness(&mut self) {
        // try to accept again after handling all session events,
        // since we might have released a few session slots
        if self.sessions.borrow().can_accept && !self.accept_ready.is_empty() {
            loop {
                if let Some(token) = self
                    .accept_ready
                    .iter()
                    .next()
                    .map(|token| ListenToken(token.0))
                {
                    let protocol = self.sessions.borrow().slab[token.0].borrow().protocol();
                    self.accept(token, protocol);
                    if !self.sessions.borrow().can_accept || self.accept_ready.is_empty() {
                        break;
                    }
                } else {
                    // we don't have any more elements to loop over
                    break;
                }
            }
        }
    }
}

pub struct ListenSession {
    pub protocol: Protocol,
}

impl ProxySession for ListenSession {
    fn last_event(&self) -> Instant {
        Instant::now()
    }

    fn print_state(&self) {}

    fn tokens(&self) -> Vec<Token> {
        Vec::new()
    }

    fn protocol(&self) -> Protocol {
        self.protocol
    }

    fn ready(&mut self, _session: Rc<RefCell<dyn ProxySession>>) {}

    fn shutting_down(&mut self) {}

    fn process_events(&mut self, _token: Token, _events: Ready) {}

    fn close(&mut self) {}

    fn timeout(&mut self, _token: Token) {
        error!(
            "called ProxySession::timeout(token={:?}, time) on ListenSession {{ protocol: {:?} }}",
            _token, self.protocol
        );
    }
}

#[cfg(feature = "use-openssl")]
use crate::https_openssl;

use crate::https_rustls;

#[cfg(feature = "use-openssl")]
pub enum HttpsProvider {
    Openssl(Rc<RefCell<https_openssl::Proxy>>),
    Rustls(Rc<RefCell<https_rustls::configuration::Proxy>>),
}

#[cfg(not(feature = "use-openssl"))]
pub enum HttpsProvider {
    Rustls(Rc<RefCell<https_rustls::configuration::Proxy>>),
}

#[cfg(feature = "use-openssl")]
impl HttpsProvider {
    pub fn new(
        use_openssl: bool,
        registry: Registry,
        sessions: Rc<RefCell<SessionManager>>,
        pool: Rc<RefCell<Pool>>,
        backends: Rc<RefCell<BackendMap>>,
    ) -> HttpsProvider {
        if use_openssl {
            HttpsProvider::Openssl(Rc::new(RefCell::new(https_openssl::Proxy::new(
                registry, sessions, pool, backends,
            ))))
        } else {
            HttpsProvider::Rustls(Rc::new(RefCell::new(
                https_rustls::configuration::Proxy::new(registry, sessions, pool, backends),
            )))
        }
    }

    pub fn notify(&mut self, message: ProxyRequest) -> ProxyResponse {
        match self {
            &mut HttpsProvider::Rustls(ref mut rustls) => rustls.borrow_mut().notify(message),
            &mut HttpsProvider::Openssl(ref mut openssl) => openssl.borrow_mut().notify(message),
        }
    }

    pub fn add_listener(&mut self, config: HttpsListener, token: Token) -> Option<Token> {
        match self {
            &mut HttpsProvider::Rustls(ref mut rustls) => {
                rustls.borrow_mut().add_listener(config, token)
            }
            &mut HttpsProvider::Openssl(ref mut openssl) => {
                openssl.borrow_mut().add_listener(config, token)
            }
        }
    }

    pub fn activate_listener(
        &mut self,
        addr: &SocketAddr,
        tcp_listener: Option<TcpListener>,
    ) -> Option<Token> {
        match self {
            &mut HttpsProvider::Rustls(ref mut rustls) => {
                rustls.borrow_mut().activate_listener(addr, tcp_listener)
            }
            &mut HttpsProvider::Openssl(ref mut openssl) => {
                openssl.borrow_mut().activate_listener(addr, tcp_listener)
            }
        }
    }

    pub fn give_back_listeners(&mut self) -> Vec<(SocketAddr, TcpListener)> {
        match self {
            &mut HttpsProvider::Rustls(ref mut rustls) => rustls.borrow_mut().give_back_listeners(),
            &mut HttpsProvider::Openssl(ref mut openssl) => {
                openssl.borrow_mut().give_back_listeners()
            }
        }
    }

    pub fn give_back_listener(&mut self, address: SocketAddr) -> Option<(Token, TcpListener)> {
        match self {
            &mut HttpsProvider::Rustls(ref mut rustls) => {
                rustls.borrow_mut().give_back_listener(address)
            }
            &mut HttpsProvider::Openssl(ref mut openssl) => {
                openssl.borrow_mut().give_back_listener(address)
            }
        }
    }

    pub fn accept(&mut self, token: ListenToken) -> Result<TcpStream, AcceptError> {
        match self {
            &mut HttpsProvider::Rustls(ref mut rustls) => rustls.borrow_mut().accept(token),
            &mut HttpsProvider::Openssl(ref mut openssl) => openssl.borrow_mut().accept(token),
        }
    }

    pub fn create_session(
        &mut self,
        frontend_sock: TcpStream,
        token: ListenToken,
        wait_time: Duration,
        _proxy: Rc<RefCell<Self>>,
    ) -> Result<(), AcceptError> {
        match self {
            &mut HttpsProvider::Rustls(ref mut rustls) => {
                let r = rustls.clone();
                rustls
                    .borrow_mut()
                    .create_session(frontend_sock, token, wait_time, r)
            }
            &mut HttpsProvider::Openssl(ref mut openssl) => {
                let o = openssl.clone();
                openssl
                    .borrow_mut()
                    .create_session(frontend_sock, token, wait_time, o)
            }
        }
    }
}

#[cfg(not(feature = "use-openssl"))]
use crate::https_rustls::session::Session;

#[cfg(not(feature = "use-openssl"))]
impl HttpsProvider {
    pub fn new(
        use_openssl: bool,
        registry: Registry,
        sessions: Rc<RefCell<SessionManager>>,
        pool: Rc<RefCell<Pool>>,
        backends: Rc<RefCell<BackendMap>>,
    ) -> HttpsProvider {
        if use_openssl {
            error!("the openssl provider is not compiled, continuing with the rustls provider");
        }

        let configuration =
            https_rustls::configuration::Proxy::new(registry, sessions, pool, backends);
        HttpsProvider::Rustls(Rc::new(RefCell::new(configuration)))
    }

    pub fn notify(&mut self, message: ProxyRequest) -> ProxyResponse {
        let &mut HttpsProvider::Rustls(ref mut rustls) = self;
        rustls.borrow_mut().notify(message)
    }

    pub fn add_listener(&mut self, config: HttpsListener, token: Token) -> Option<Token> {
        let &mut HttpsProvider::Rustls(ref mut rustls) = self;
        rustls.borrow_mut().add_listener(config, token)
    }

    pub fn activate_listener(
        &mut self,
        addr: &SocketAddr,
        tcp_listener: Option<TcpListener>,
    ) -> Option<Token> {
        let &mut HttpsProvider::Rustls(ref mut rustls) = self;

        rustls.borrow_mut().activate_listener(addr, tcp_listener)
    }

    pub fn give_back_listeners(&mut self) -> Vec<(SocketAddr, TcpListener)> {
        let &mut HttpsProvider::Rustls(ref mut rustls) = self;
        rustls.borrow_mut().give_back_listeners()
    }

    pub fn give_back_listener(&mut self, address: SocketAddr) -> Option<(Token, TcpListener)> {
        let &mut HttpsProvider::Rustls(ref mut rustls) = self;
        rustls.borrow_mut().give_back_listener(address)
    }

    pub fn accept(&mut self, token: ListenToken) -> Result<TcpStream, AcceptError> {
        let &mut HttpsProvider::Rustls(ref mut rustls) = self;
        rustls.borrow_mut().accept(token)
    }

    pub fn create_session(
        &mut self,
        frontend_sock: TcpStream,
        token: ListenToken,
        wait_time: Duration,
        proxy: Rc<RefCell<Self>>,
    ) -> Result<(), AcceptError> {
        let &mut HttpsProvider::Rustls(ref mut rustls) = self;
        let r = rustls.clone();
        rustls
            .borrow_mut()
            .create_session(frontend_sock, token, wait_time, r)
    }
}

#[cfg(feature = "use-openssl")]
fn clear_ssl_error() {
    unsafe { ::openssl_sys::ERR_clear_error() };
}

#[cfg(not(feature = "use-openssl"))]
fn clear_ssl_error() {}
