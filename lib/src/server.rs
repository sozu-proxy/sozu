//! event loop management
use std::{
    cell::RefCell,
    collections::{HashSet, VecDeque},
    convert::TryFrom,
    os::unix::io::{AsRawFd, FromRawFd},
    rc::Rc,
};

use anyhow::Context;
use mio::{
    net::{TcpListener as MioTcpListener, TcpStream},
    Events, Interest, Poll, Token,
};
use slab::Slab;
use sozu_command::{
    channel::Channel,
    config::Config,
    proto::command::{
        request::RequestType, response_content::ContentType, ActivateListener, AddBackend,
        CertificatesWithFingerprints, Cluster, ClusterHashes, ClusterInformations,
        DeactivateListener, Event, HttpListenerConfig, HttpsListenerConfig, ListenerType,
        LoadBalancingAlgorithms, LoadMetric, MetricsConfiguration, RemoveBackend, ResponseStatus,
        TcpListenerConfig as CommandTcpListener,
    },
    ready::Ready,
    request::WorkerRequest,
    response::{MessageId, WorkerResponse},
    scm_socket::{Listeners, ScmSocket},
    state::ConfigState,
};
use time::{Duration, Instant};

use crate::{
    backends::BackendMap, features::FEATURES, http, https, metrics::METRICS, pool::Pool, tcp,
    timer::Timer, AcceptError, Backend, Protocol, ProxyConfiguration, ProxySession,
    SessionIsToBeClosed,
};

// Number of retries to perform on a server after a connection failure
pub const CONN_RETRIES: u8 = 3;

pub type ProxyChannel = Channel<WorkerResponse, WorkerRequest>;

thread_local! {
  pub static QUEUE: RefCell<VecDeque<WorkerResponse>> = RefCell::new(VecDeque::new());
}

thread_local! {
  pub static TIMER: RefCell<Timer<Token>> = RefCell::new(Timer::default());
}

pub fn push_queue(message: WorkerResponse) {
    QUEUE.with(|queue| {
        (*queue.borrow_mut()).push_back(message);
    });
}

pub fn push_event(event: Event) {
    QUEUE.with(|queue| {
        (*queue.borrow_mut()).push_back(WorkerResponse {
            id: "EVENT".to_string(),
            message: String::new(),
            status: ResponseStatus::Processing,
            content: Some(ContentType::Event(event).into()),
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

    /// The slab is considered at capacity if it contains more sessions than twice max_connections
    pub fn at_capacity(&self) -> bool {
        self.slab.len() >= 10 + 2 * self.max_connections
    }

    /// Check the number of connections against max_connections, and the slab capacity.
    /// Returns false if limits are reached.
    pub fn check_limits(&mut self) -> bool {
        if self.nb_connections >= self.max_connections {
            error!("max number of session connection reached, flushing the accept queue");
            gauge!("accept_queue.backpressure", 1);
            self.can_accept = false;
            return false;
        }

        if self.at_capacity() {
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

    /// Decrements the number of sessions, start accepting new connections
    /// if the capacity limit of 90% has not been reached.
    pub fn decr(&mut self) {
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
/// (HTTP, HTTPS and TCP) and the routing configuration for clusters.
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
    accept_queue_timeout: Duration,
    accept_queue: VecDeque<(TcpStream, ListenToken, Protocol, Instant)>,
    accept_ready: HashSet<ListenToken>,
    backends: Rc<RefCell<BackendMap>>,
    base_sessions_count: usize,
    channel: ProxyChannel,
    config_state: ConfigState,
    current_poll_errors: i32,
    http: Rc<RefCell<http::HttpProxy>>,
    https: Rc<RefCell<https::HttpsProxy>>,
    last_sessions_len: usize,
    last_shutting_down_message: Option<Instant>,
    last_zombie_check: Instant,
    loop_start: Instant,
    max_poll_errors: i32, // TODO: make this configurable? this defaults to 10000 for now
    pub poll: Poll,
    poll_timeout: Option<Duration>, // TODO: make this configurable? this defaults to 1000 milliseconds for now
    pool: Rc<RefCell<Pool>>,
    scm_listeners: Option<Listeners>,
    scm: ScmSocket,
    sessions: Rc<RefCell<SessionManager>>,
    should_poll_at: Option<Instant>,
    shutting_down: Option<MessageId>,
    tcp: Rc<RefCell<tcp::TcpProxy>>,
    zombie_check_interval: Duration,
}

impl Server {
    pub fn try_new_from_config(
        worker_to_main_channel: ProxyChannel,
        worker_to_main_scm: ScmSocket,
        config: Config,
        config_state: ConfigState,
        expects_initial_status: bool,
    ) -> anyhow::Result<Self> {
        let event_loop = Poll::new().with_context(|| "could not create event loop")?;
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

        let registry = event_loop
            .registry()
            .try_clone()
            .with_context(|| "could not clone the mio Registry")?;

        let https =
            https::HttpsProxy::new(registry, sessions.clone(), pool.clone(), backends.clone());

        Server::new(
            event_loop,
            worker_to_main_channel,
            worker_to_main_scm,
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

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        poll: Poll,
        mut channel: ProxyChannel,
        scm: ScmSocket,
        sessions: Rc<RefCell<SessionManager>>,
        pool: Rc<RefCell<Pool>>,
        backends: Rc<RefCell<BackendMap>>,
        http: Option<http::HttpProxy>,
        https: Option<https::HttpsProxy>,
        tcp: Option<tcp::TcpProxy>,
        server_config: ServerConfig,
        config_state: Option<ConfigState>,
        expects_initial_status: bool,
    ) -> anyhow::Result<Self> {
        FEATURES.with(|_features| {
            // initializing feature flags
        });

        poll.registry()
            .register(
                &mut channel,
                Token(0),
                Interest::READABLE | Interest::WRITABLE,
            )
            .with_context(|| "should register the channel")?;

        METRICS.with(|metrics| {
            if let Some(sock) = (*metrics.borrow_mut()).socket_mut() {
                poll.registry()
                    .register(sock, Token(2), Interest::WRITABLE)
                    .expect("should register the metrics socket");
            }
        });

        let base_sessions_count = sessions.borrow().slab.len();

        let http = Rc::new(RefCell::new(match http {
            Some(http) => http,
            None => {
                let registry = poll
                    .registry()
                    .try_clone()
                    .with_context(|| "could not clone the mio Registry")?;
                http::HttpProxy::new(registry, sessions.clone(), pool.clone(), backends.clone())
            }
        }));

        let https = Rc::new(RefCell::new(match https {
            Some(https) => https,
            None => {
                let registry = poll
                    .registry()
                    .try_clone()
                    .with_context(|| "could not clone the mio Registry")?;

                https::HttpsProxy::new(registry, sessions.clone(), pool.clone(), backends.clone())
            }
        }));

        let tcp = Rc::new(RefCell::new(match tcp {
            Some(tcp) => tcp,
            None => {
                let registry = poll
                    .registry()
                    .try_clone()
                    .with_context(|| "could not clone the mio Registry")?;
                tcp::TcpProxy::new(registry, sessions.clone(), backends.clone())
            }
        }));

        let mut server = Server {
            accept_queue_timeout: Duration::seconds(i64::from(server_config.accept_queue_timeout)),
            accept_queue: VecDeque::new(),
            accept_ready: HashSet::new(),
            backends,
            base_sessions_count,
            channel,
            config_state: ConfigState::new(),
            current_poll_errors: 0,
            http,
            https,
            last_sessions_len: 0, // to be reset on server run
            last_shutting_down_message: None,
            last_zombie_check: Instant::now(), // to be reset on server run
            loop_start: Instant::now(),        // to be reset on server run
            max_poll_errors: 10000,            // TODO: make it configurable?
            poll_timeout: Some(Duration::milliseconds(1000)), // TODO: make it configurable?
            poll,
            pool,
            scm_listeners: None,
            scm,
            sessions,
            should_poll_at: None,
            shutting_down: None,
            tcp,
            zombie_check_interval: Duration::seconds(i64::from(
                server_config.zombie_check_interval,
            )),
        };

        // initialize the worker with the state we got from a file
        if let Some(state) = config_state {
            for (counter, request) in state.generate_requests().iter().enumerate() {
                let id = format!("INIT-{counter}");
                let worker_request = WorkerRequest {
                    id,
                    content: request.to_owned(),
                };

                trace!("generating initial config request: {:#?}", worker_request);
                server.notify_proxys(worker_request);
            }

            // do not send back answers to the initialization messages
            QUEUE.with(|queue| {
                (*queue.borrow_mut()).clear();
            });
        }

        if expects_initial_status {
            // the main process sends a Status message, so we can notify it
            // when the initial state is loaded
            server.block_channel();
            let msg = server.channel.read_message();
            debug!("got message: {:?}", msg);

            if let Ok(msg) = msg {
                if let Err(e) = server.channel.write_message(&WorkerResponse::ok(msg.id)) {
                    error!("Could not send an ok to the main process: {}", e);
                }
            }
            server.unblock_channel();
        }

        info!("will try to receive listeners");
        server
            .scm
            .set_blocking(true)
            .with_context(|| "Could not set the scm socket to blocking")?;
        let listeners = server
            .scm
            .receive_listeners()
            .with_context(|| "could not receive listeners from the scm socket")?;
        server
            .scm
            .set_blocking(false)
            .with_context(|| "Could not set the scm socket to unblocking")?;
        info!("received listeners: {:?}", listeners);
        server.scm_listeners = Some(listeners);

        Ok(server)
    }

    /// The server runs in a loop until a shutdown is ordered
    pub fn run(&mut self) {
        let mut events = Events::with_capacity(1024); // TODO: make event capacity configurable?
        self.last_sessions_len = self.sessions.borrow().slab.len();

        self.last_zombie_check = Instant::now();
        self.loop_start = Instant::now();

        loop {
            self.check_for_poll_errors();

            let timeout = self.reset_loop_time_and_get_timeout();

            match self.poll.poll(&mut events, timeout) {
                Ok(_) => self.current_poll_errors = 0,
                Err(error) => {
                    error!("Error while polling events: {:?}", error);
                    self.current_poll_errors += 1;
                    continue;
                }
            }

            let after_epoll = Instant::now();
            time!(
                "epoll_time",
                (after_epoll - self.loop_start).whole_milliseconds()
            );
            self.loop_start = after_epoll;

            self.send_queue();

            for event in events.iter() {
                match event.token() {
                    // this is the command channel
                    Token(0) => {
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

                            // exit the big loop if the message is HardStop
                            if self.read_channel_messages_and_notify() {
                                return;
                            }

                            QUEUE.with(|queue| {
                                if !(*queue.borrow()).is_empty() {
                                    self.channel.interest.insert(Ready::writable());
                                }
                            });

                            self.send_queue();
                        }
                    }
                    // timer tick
                    Token(1) => {
                        while let Some(t) = TIMER.with(|timer| timer.borrow_mut().poll()) {
                            self.timeout(t);
                        }
                    }
                    // metrics socket is writable
                    Token(2) => METRICS.with(|metrics| {
                        (*metrics.borrow_mut()).writable();
                    }),
                    // ListenToken: 1 listener <=> 1 token
                    // ProtocolToken (HTTP/HTTPS/TCP): 1 connection <=> 1 token
                    token => self.ready(token, Ready::from(event)),
                }
            }

            if let Some(t) = self.should_poll_at.as_ref() {
                if *t <= Instant::now() {
                    while let Some(t) = TIMER.with(|timer| timer.borrow_mut().poll()) {
                        //info!("polled for timeout: {:?}", t);
                        self.timeout(t);
                    }
                }
            }
            self.handle_remaining_readiness();
            self.create_sessions();

            self.should_poll_at = TIMER.with(|timer| timer.borrow().next_poll_date());

            self.zombie_check();

            let now = time::OffsetDateTime::now_utc();
            // clear the local metrics drain every plain hour (01:00, 02:00, etc.) to prevent memory overuse
            // TODO: have one-hour-lasting metrics instead
            if now.minute() == 00 && now.second() == 0 {
                METRICS.with(|metrics| {
                    (*metrics.borrow_mut()).clear_local();
                });
            }

            gauge!("client.connections", self.sessions.borrow().nb_connections);
            gauge!("slab.count", self.sessions.borrow().slab.len());
            METRICS.with(|metrics| {
                (*metrics.borrow_mut()).send_data();
            });

            if self.shutting_down.is_some() && self.shut_down_sessions() {
                return;
            }
        }
    }

    fn check_for_poll_errors(&mut self) {
        if self.current_poll_errors >= self.max_poll_errors {
            error!(
                "Something is going very wrong. Last {} poll() calls failed, crashing..",
                self.current_poll_errors
            );
            panic!(
                "poll() calls failed {} times in a row",
                self.current_poll_errors
            );
        }
    }

    fn reset_loop_time_and_get_timeout(&mut self) -> Option<std::time::Duration> {
        let now = Instant::now();
        time!(
            "event_loop_time",
            (now - self.loop_start).whole_milliseconds()
        );

        let timeout = match self.should_poll_at.as_ref() {
            None => self.poll_timeout,
            Some(i) => {
                if *i <= now {
                    self.poll_timeout
                } else {
                    let dur = *i - now;
                    match self.poll_timeout {
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

        self.loop_start = now;
        timeout.and_then(|t| std::time::Duration::try_from(t).ok())
    }

    /// Returns true if hardstop
    fn read_channel_messages_and_notify(&mut self) -> bool {
        if !self.channel.readiness().is_readable() {
            return false;
        }

        if let Err(e) = self.channel.readable() {
            error!("error reading from channel: {:?}", e);
        }

        loop {
            match self.channel.read_message() {
                Ok(request) => match request.content.request_type {
                    Some(RequestType::HardStop(_)) => {
                        let req_id = request.id.clone();
                        self.notify(request);
                        if let Err(e) = self.channel.write_message(&WorkerResponse::ok(req_id)) {
                            error!("Could not send ok response to the main process: {}", e);
                        }
                        if let Err(e) = self.channel.run() {
                            error!("Error while running the server channel: {}", e);
                        }
                        return true;
                    }
                    Some(RequestType::SoftStop(_)) => {
                        self.shutting_down = Some(request.id.clone());
                        self.last_sessions_len = self.sessions.borrow().slab.len();
                        self.notify(request);
                    }
                    Some(RequestType::ReturnListenSockets(_)) => {
                        info!("received ReturnListenSockets order");
                        self.return_listen_sockets();
                    }
                    _ => self.notify(request),
                },
                // Not an error per se, occurs when there is nothing to read
                Err(_) => {
                    // if the message was too large, we grow the buffer and retry to read if possible
                    if (self.channel.interest & self.channel.readiness).is_readable() {
                        if let Err(e) = self.channel.readable() {
                            error!("error reading from channel: {:?}", e);
                        }
                        continue;
                    }
                    break;
                }
            }
        }
        false
    }

    /// Scans all sessions that have been inactive for longer than the configured interval
    fn zombie_check(&mut self) {
        let now = Instant::now();
        if now - self.last_zombie_check < self.zombie_check_interval {
            return;
        }
        info!("zombie check");
        self.last_zombie_check = now;

        let mut zombie_tokens = HashSet::new();

        // find the zombie sessions
        for (_index, session) in self
            .sessions
            .borrow_mut()
            .slab
            .iter_mut()
            .filter(|(_, c)| now - c.borrow().last_event() > self.zombie_check_interval)
        {
            let session_token = session.borrow().frontend_token();
            if !zombie_tokens.contains(&session_token) {
                session.borrow().print_session();
                zombie_tokens.insert(session_token);
            }
        }

        let zombie_count = zombie_tokens.len() as i64;
        count!("zombies", zombie_count);

        let remaining_count = self.shut_down_sessions_by_frontend_tokens(zombie_tokens);
        info!(
            "removing {} zombies ({} remaining entries after close)",
            zombie_count, remaining_count
        );
    }

    /// Calls close on targeted sessions, yields the number of entries in the slab
    /// that were not properly removed
    fn shut_down_sessions_by_frontend_tokens(&self, tokens: HashSet<Token>) -> usize {
        if tokens.is_empty() {
            return 0;
        }

        // close the sessions associated with the tokens
        for token in &tokens {
            if self.sessions.borrow().slab.contains(token.0) {
                let session = { self.sessions.borrow_mut().slab.remove(token.0) };
                session.borrow_mut().close();
                self.sessions.borrow_mut().decr();
            }
        }

        // find the entries of closed sessions in the session manager (they should not be there)
        let mut dangling_entries = HashSet::new();
        for (entry_key, session) in &self.sessions.borrow().slab {
            if tokens.contains(&session.borrow().frontend_token()) {
                dangling_entries.insert(entry_key);
            }
        }

        // remove these from the session manager
        let mut dangling_entries_count = 0;
        for entry_key in dangling_entries {
            let mut sessions = self.sessions.borrow_mut();
            if sessions.slab.contains(entry_key) {
                sessions.slab.remove(entry_key);
                dangling_entries_count += 1;
            }
        }
        dangling_entries_count
    }

    /// Order sessions to shut down, check that they are all down
    fn shut_down_sessions(&mut self) -> bool {
        let sessions_count = self.sessions.borrow().slab.len();
        let mut sessions_to_shut_down = HashSet::new();

        for (_key, session) in &self.sessions.borrow().slab {
            if session.borrow_mut().shutting_down() {
                sessions_to_shut_down.insert(Token(session.borrow().frontend_token().0));
            }
        }
        let _ = self.shut_down_sessions_by_frontend_tokens(sessions_to_shut_down);

        let new_sessions_count = self.sessions.borrow().slab.len();

        if new_sessions_count < sessions_count {
            let now = Instant::now();
            if let Some(last) = self.last_shutting_down_message {
                if (now - last) > Duration::seconds(5) {
                    info!(
                        "closed {} sessions, {} sessions left, base_sessions_count = {}",
                        sessions_count - new_sessions_count,
                        new_sessions_count,
                        self.base_sessions_count
                    );
                }
            }
            self.last_shutting_down_message = Some(now);
        }

        if new_sessions_count <= self.base_sessions_count {
            info!("last session stopped, shutting down!");
            if let Err(e) = self.channel.run() {
                error!("Error while running the server channel: {}", e);
            }
            self.block_channel();
            let id = self
                .shutting_down
                .take()
                .expect("should have shut down correctly"); // panicking here makes sense actually
            let proxy_response = WorkerResponse::ok(id);
            if let Err(e) = self.channel.write_message(&proxy_response) {
                error!("Could not write response to the main process: {}", e);
            }
            return true;
        }

        if new_sessions_count < self.last_sessions_len {
            info!(
                "shutting down, {} slab elements remaining (base: {})",
                new_sessions_count - self.base_sessions_count,
                self.base_sessions_count
            );
            self.last_sessions_len = new_sessions_count;
        }

        false
    }

    fn kill_session(&self, session: Rc<RefCell<dyn ProxySession>>) {
        let token = session.borrow().frontend_token();
        let _ = self.shut_down_sessions_by_frontend_tokens(HashSet::from([token]));
    }

    fn send_queue(&mut self) {
        if self.channel.readiness.is_writable() {
            QUEUE.with(|q| {
                let mut queue = q.borrow_mut();
                loop {
                    if let Some(msg) = queue.pop_front() {
                        if let Err(e) = self.channel.write_message(&msg) {
                            error!("Could not write message {} on the channel: {}", msg, e);
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

    fn notify(&mut self, message: WorkerRequest) {
        if let Some(RequestType::ConfigureMetrics(configuration)) = &message.content.request_type {
            if let Some(metrics_config) = MetricsConfiguration::from_i32(*configuration) {
                METRICS.with(|metrics| {
                    (*metrics.borrow_mut()).configure(&metrics_config);

                    push_queue(WorkerResponse::ok(message.id.clone()));
                });
                return;
            }
        }

        match &message.content.request_type {
            Some(RequestType::QueryClustersHashes(_)) => {
                push_queue(WorkerResponse::ok_with_content(
                    message.id.clone(),
                    ContentType::ClusterHashes(ClusterHashes {
                        map: self.config_state.hash_state(),
                    })
                    .into(),
                ));
                return;
            }
            Some(RequestType::QueryClusterById(cluster_id)) => {
                push_queue(WorkerResponse::ok_with_content(
                    message.id.clone(),
                    ContentType::Clusters(ClusterInformations {
                        vec: vec![self.config_state.cluster_state(cluster_id)],
                    })
                    .into(),
                ));
            }
            Some(RequestType::QueryClustersByDomain(domain)) => {
                let cluster_ids = self
                    .config_state
                    .get_cluster_ids_by_domain(domain.hostname.clone(), domain.path.clone());
                let vec = cluster_ids
                    .iter()
                    .map(|cluster_id| self.config_state.cluster_state(cluster_id))
                    .collect();

                push_queue(WorkerResponse::ok_with_content(
                    message.id.clone(),
                    ContentType::Clusters(ClusterInformations { vec }).into(),
                ));
                return;
            }
            Some(RequestType::QueryCertificateByFingerprint(f)) => {
                let certs = self
                    .config_state
                    .get_certificate_by_fingerprint(f.to_string());
                let response = if certs.len() >= 1 {
                    WorkerResponse::ok_with_content(
                        message.id.clone(),
                        ContentType::CertificatesWithFingerprints(CertificatesWithFingerprints {
                            certs,
                        })
                        .into(),
                    )
                } else {
                    WorkerResponse::error(
                        message.id.clone(),
                        format!("Could not find certificate for fingerprint {}", f),
                    )
                };
                push_queue(response);
                return;
            }
            Some(RequestType::QueryMetrics(query_metrics_options)) => {
                METRICS.with(|metrics| {
                    match (*metrics.borrow_mut()).query(query_metrics_options) {
                        Ok(c) => push_queue(WorkerResponse::ok_with_content(message.id.clone(), c)),
                        Err(e) => error!("Error querying metrics: {:#}", e),
                    }
                });
                return;
            }
            _other_request => {}
        }

        self.notify_proxys(message);
    }

    pub fn notify_proxys(&mut self, request: WorkerRequest) {
        if let Err(e) = self.config_state.dispatch(&request.content) {
            error!("Could not execute order on config state: {:#}", e);
        }

        let req_id = request.id.clone();

        match request.content.request_type {
            Some(RequestType::AddCluster(ref cluster)) => {
                self.add_cluster(cluster);
                //not returning because the message must still be handled by each proxy
            }
            Some(RequestType::AddBackend(ref backend)) => {
                push_queue(self.add_backend(&req_id, backend));
                return;
            }
            Some(RequestType::RemoveBackend(ref remove_backend)) => {
                push_queue(self.remove_backend(&req_id, remove_backend));
                return;
            }
            _ => {}
        };

        let proxy_destinations = request.content.get_destinations();
        if proxy_destinations.to_http_proxy {
            push_queue(self.http.borrow_mut().notify(request.clone()));
        }
        if proxy_destinations.to_https_proxy {
            push_queue(self.https.borrow_mut().notify(request.clone()));
        }
        if proxy_destinations.to_tcp_proxy {
            push_queue(self.tcp.borrow_mut().notify(request.clone()));
        }

        match request.content.request_type {
            // special case for adding listeners, because we need to register a listener
            Some(RequestType::AddHttpListener(ref listener)) => {
                push_queue(self.notify_add_http_listener(&req_id, listener));
            }
            Some(RequestType::AddHttpsListener(ref listener)) => {
                push_queue(self.notify_add_https_listener(&req_id, listener));
            }
            Some(RequestType::AddTcpListener(listener)) => {
                push_queue(self.notify_add_tcp_listener(&req_id, listener));
            }
            Some(RequestType::RemoveListener(ref remove)) => {
                debug!("{} remove {:?} listener {:?}", req_id, remove.proxy, remove);
                self.base_sessions_count -= 1;
                let response = match ListenerType::from_i32(remove.proxy) {
                    Some(ListenerType::Http) => self.http.borrow_mut().notify(request.clone()),
                    Some(ListenerType::Https) => self.https.borrow_mut().notify(request.clone()),
                    Some(ListenerType::Tcp) => self.tcp.borrow_mut().notify(request.clone()),
                    None => WorkerResponse::error(req_id, "Wrong variant ListenerType"),
                };
                push_queue(response);
            }
            Some(RequestType::ActivateListener(ref activate)) => {
                push_queue(self.notify_activate_listener(&req_id, activate));
            }
            Some(RequestType::DeactivateListener(ref deactivate)) => {
                push_queue(self.notify_deactivate_listener(&req_id, deactivate));
            }
            _other_request => {}
        };
    }

    fn add_cluster(&mut self, cluster: &Cluster) {
        self.backends
            .borrow_mut()
            .set_load_balancing_policy_for_cluster(
                &cluster.cluster_id,
                LoadBalancingAlgorithms::from_i32(cluster.load_balancing).unwrap_or_default(),
                cluster.load_metric.and_then(LoadMetric::from_i32),
            );
    }

    fn add_backend(&mut self, req_id: &str, add_backend: &AddBackend) -> WorkerResponse {
        let new_backend = Backend::new(
            &add_backend.backend_id,
            add_backend.address.parse().unwrap(),
            add_backend.sticky_id.clone(),
            add_backend.load_balancing_parameters.clone(),
            add_backend.backup,
        );
        self.backends
            .borrow_mut()
            .add_backend(&add_backend.cluster_id, new_backend);

        WorkerResponse::ok(req_id)
    }

    fn remove_backend(&mut self, req_id: &str, backend: &RemoveBackend) -> WorkerResponse {
        let address = backend.address.parse().unwrap();
        self.backends
            .borrow_mut()
            .remove_backend(&backend.cluster_id, &address);

        WorkerResponse::ok(req_id)
    }

    fn notify_add_http_listener(
        &mut self,
        req_id: &str,
        listener: &HttpListenerConfig,
    ) -> WorkerResponse {
        debug!("{} add http listener {:?}", req_id, listener);

        if self.sessions.borrow().at_capacity() {
            return WorkerResponse::error(
                req_id.to_string(),
                "session list is full, cannot add a listener",
            );
        }

        let mut session_manager = self.sessions.borrow_mut();
        let entry = session_manager.slab.vacant_entry();
        let token = Token(entry.key());

        match self.http.borrow_mut().add_listener(listener.clone(), token) {
            Ok(_token) => {
                entry.insert(Rc::new(RefCell::new(ListenSession {
                    protocol: Protocol::HTTPListen,
                })));
                self.base_sessions_count += 1;
                WorkerResponse::ok(req_id)
            }
            Err(e) => {
                let error = format!("Couldn't add HTTP listener: {e:#}");
                error!("{}", error);
                WorkerResponse::error(req_id, error)
            }
        }
    }

    fn notify_add_https_listener(
        &mut self,
        req_id: &str,
        listener: &HttpsListenerConfig,
    ) -> WorkerResponse {
        debug!("{} add https listener {:?}", req_id, listener);

        if self.sessions.borrow().at_capacity() {
            return WorkerResponse::error(req_id, "session list is full, cannot add a listener");
        }

        let mut session_manager = self.sessions.borrow_mut();
        let entry = session_manager.slab.vacant_entry();
        let token = Token(entry.key());

        match self
            .https
            .borrow_mut()
            .add_listener(listener.clone(), token)
        {
            Some(_token) => {
                entry.insert(Rc::new(RefCell::new(ListenSession {
                    protocol: Protocol::HTTPSListen,
                })));
                self.base_sessions_count += 1;
                WorkerResponse::ok(req_id)
            }
            None => {
                error!("Couldn't add HTTPS listener");
                WorkerResponse::error(req_id, "cannot add HTTPS listener")
            }
        }
    }

    fn notify_add_tcp_listener(
        &mut self,
        req_id: &str,
        listener: CommandTcpListener,
    ) -> WorkerResponse {
        debug!("{} add tcp listener {:?}", req_id, listener);

        if self.sessions.borrow().at_capacity() {
            return WorkerResponse::error(req_id, "session list is full, cannot add a listener");
        }

        let mut session_manager = self.sessions.borrow_mut();
        let entry = session_manager.slab.vacant_entry();
        let token = Token(entry.key());

        match self
            .tcp
            .borrow_mut()
            .add_listener(listener, self.pool.clone(), token)
        {
            Ok(_token) => {
                entry.insert(Rc::new(RefCell::new(ListenSession {
                    protocol: Protocol::TCPListen,
                })));
                self.base_sessions_count += 1;
                WorkerResponse::ok(req_id)
            }
            Err(e) => {
                let error = format!("Couldn't add TCP listener: {e:#}");
                error!("{}", error);
                WorkerResponse::error(req_id, error)
            }
        }
    }

    fn notify_activate_listener(
        &mut self,
        req_id: &str,
        activate: &ActivateListener,
    ) -> WorkerResponse {
        debug!(
            "{} activate {:?} listener {:?}",
            req_id, activate.proxy, activate
        );

        let address: std::net::SocketAddr = match activate.address.parse() {
            Ok(a) => a,
            Err(e) => return WorkerResponse::error(req_id, format!("Wrong socket address: {e}")),
        };

        match ListenerType::from_i32(activate.proxy) {
            Some(ListenerType::Http) => {
                let listener = self
                    .scm_listeners
                    .as_mut()
                    .and_then(|s| s.get_http(&address))
                    .map(|fd| unsafe { MioTcpListener::from_raw_fd(fd) });

                let activated_token = self.http.borrow_mut().activate_listener(&address, listener);
                match activated_token {
                    Ok(token) => {
                        self.accept(ListenToken(token.0), Protocol::HTTPListen);
                        WorkerResponse::ok(req_id)
                    }
                    Err(activate_error) => {
                        error!("Could not activate HTTP listener: {:#}", activate_error);
                        WorkerResponse::error(req_id, format!("{activate_error:#}"))
                    }
                }
            }
            Some(ListenerType::Https) => {
                let listener = self
                    .scm_listeners
                    .as_mut()
                    .and_then(|s| s.get_https(&address))
                    .map(|fd| unsafe { MioTcpListener::from_raw_fd(fd) });

                let activated_token = self
                    .https
                    .borrow_mut()
                    .activate_listener(&address, listener);
                match activated_token {
                    Ok(token) => {
                        self.accept(ListenToken(token.0), Protocol::HTTPSListen);
                        WorkerResponse::ok(req_id)
                    }
                    Err(activate_error) => {
                        error!("Could not activate HTTPS listener: {:#}", activate_error);
                        WorkerResponse::error(req_id, format!("{activate_error:#}"))
                    }
                }
            }
            Some(ListenerType::Tcp) => {
                let listener = self
                    .scm_listeners
                    .as_mut()
                    .and_then(|s| s.get_tcp(&address))
                    .map(|fd| unsafe { MioTcpListener::from_raw_fd(fd) });

                let listener_token = self.tcp.borrow_mut().activate_listener(&address, listener);
                match listener_token {
                    Some(token) => {
                        self.accept(ListenToken(token.0), Protocol::TCPListen);
                        WorkerResponse::ok(req_id)
                    }
                    None => {
                        error!("Could not activate TCP listener");
                        WorkerResponse::error(req_id, "cannot activate TCP listener")
                    }
                }
            }
            None => WorkerResponse::error(req_id, "Wrong variant for ListenerType on request"),
        }
    }

    fn notify_deactivate_listener(
        &mut self,
        req_id: &str,
        deactivate: &DeactivateListener,
    ) -> WorkerResponse {
        debug!(
            "{} deactivate {:?} listener {:?}",
            req_id, deactivate.proxy, deactivate
        );

        let address: std::net::SocketAddr = match deactivate.address.parse() {
            Ok(a) => a,
            Err(e) => return WorkerResponse::error(req_id, format!("Wrong socket address: {e}")),
        };

        match ListenerType::from_i32(deactivate.proxy) {
            Some(ListenerType::Http) => {
                let (token, mut listener) = match self.http.borrow_mut().give_back_listener(address)
                {
                    Some((token, listener)) => (token, listener),
                    None => {
                        error!("Couldn't deactivate HTTP listener at address {:?}", address);
                        return WorkerResponse::error(
                            req_id,
                            format!("cannot deactivate HTTP listener at address {address:?}"),
                        );
                    }
                };

                if let Err(e) = self.poll.registry().deregister(&mut listener) {
                    error!(
                        "error deregistering HTTP listen socket({:?}): {:?}",
                        deactivate, e
                    );
                }

                {
                    let mut sessions = self.sessions.borrow_mut();
                    if sessions.slab.contains(token.0) {
                        sessions.slab.remove(token.0);
                        info!("removed listen token {:?}", token);
                    }
                }

                if deactivate.to_scm {
                    self.unblock_scm_socket();
                    let listeners = Listeners {
                        http: vec![(address, listener.as_raw_fd())],
                        tls: vec![],
                        tcp: vec![],
                    };
                    info!("sending HTTP listener: {:?}", listeners);
                    let res = self.scm.send_listeners(&listeners);

                    self.block_scm_socket();

                    info!("sent HTTP listener: {:?}", res);
                }
                WorkerResponse::ok(req_id)
            }
            Some(ListenerType::Https) => {
                let (token, mut listener) =
                    match self.https.borrow_mut().give_back_listener(address) {
                        Some((token, listener)) => (token, listener),

                        None => {
                            error!(
                                "Couldn't deactivate HTTPS listener at address {:?}",
                                address
                            );
                            return WorkerResponse::error(
                                req_id,
                                format!("cannot deactivate HTTPS listener at address {address:?}"),
                            );
                        }
                    };
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
                    self.unblock_scm_socket();
                    let listeners = Listeners {
                        http: vec![],
                        tls: vec![(address, listener.as_raw_fd())],
                        tcp: vec![],
                    };
                    info!("sending HTTPS listener: {:?}", listeners);
                    let res = self.scm.send_listeners(&listeners);

                    self.block_scm_socket();

                    info!("sent HTTPS listener: {:?}", res);
                }
                WorkerResponse::ok(req_id)
            }
            Some(ListenerType::Tcp) => {
                let (token, mut listener) = match self.tcp.borrow_mut().give_back_listener(address)
                {
                    Some((token, listener)) => (token, listener),
                    None => {
                        error!("Couldn't deactivate TCP listener at address {:?}", address);
                        return WorkerResponse::error(
                            req_id,
                            format!("cannot deactivate TCP listener at address {address:?}"),
                        );
                    }
                };

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
                    self.unblock_scm_socket();
                    let listeners = Listeners {
                        http: vec![],
                        tls: vec![],
                        tcp: vec![(address, listener.as_raw_fd())],
                    };
                    info!("sending TCP listener: {:?}", listeners);
                    let res = self.scm.send_listeners(&listeners);

                    self.block_scm_socket();

                    info!("sent TCP listener: {:?}", res);
                }
                WorkerResponse::ok(req_id)
            }
            None => WorkerResponse::error(req_id, "Wrong variant for ListenerType on request"),
        }
    }

    /// Send all socket addresses and file descriptors of all proxies, via the scm socket
    pub fn return_listen_sockets(&mut self) {
        self.unblock_scm_socket();

        let mut http_listeners = self.http.borrow_mut().give_back_listeners();
        for &mut (_, ref mut sock) in http_listeners.iter_mut() {
            if let Err(e) = self.poll.registry().deregister(sock) {
                error!(
                    "error deregistering HTTP listen socket({:?}): {:?}",
                    sock, e
                );
            }
        }

        let mut https_listeners = self.https.borrow_mut().give_back_listeners();
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

        self.block_scm_socket();

        info!("sent default listeners: {:?}", res);
    }

    fn block_scm_socket(&mut self) {
        if let Err(e) = self.scm.set_blocking(true) {
            error!("Could not block scm socket: {}", e);
        }
    }

    fn unblock_scm_socket(&mut self) {
        if let Err(e) = self.scm.set_blocking(false) {
            error!("Could not unblock scm socket: {}", e);
        }
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
                match self.https.borrow_mut().accept(token) {
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
        while let Some((sock, token, protocol, timestamp)) = self.accept_queue.pop_back() {
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
            //TODO: create_session should return the session and
            // the server should insert it in the the SessionManager
            match protocol {
                Protocol::TCPListen => {
                    let proxy = self.tcp.clone();
                    if self
                        .tcp
                        .borrow_mut()
                        .create_session(sock, token, wait_time, proxy)
                        .is_err()
                    {
                        break;
                    }
                }
                Protocol::HTTPListen => {
                    let proxy = self.http.clone();
                    if self
                        .http
                        .borrow_mut()
                        .create_session(sock, token, wait_time, proxy)
                        .is_err()
                    {
                        break;
                    }
                }
                Protocol::HTTPSListen => {
                    if self
                        .https
                        .borrow_mut()
                        .create_session(sock, token, wait_time, self.https.clone())
                        .is_err()
                    {
                        break;
                    }
                }
                _ => panic!("should not call accept() on a HTTP, HTTPS or TCP session"),
            };
            self.sessions.borrow_mut().incr();
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

            let session = self.sessions.borrow_mut().slab[session_token].clone();
            session.borrow_mut().update_readiness(token, events);
            if session.borrow_mut().ready(session.clone()) {
                self.kill_session(session);
            }
        }
    }

    pub fn timeout(&mut self, token: Token) {
        trace!("PROXY\t{:?} got timeout", token);

        let session_token = token.0;
        if self.sessions.borrow().slab.contains(session_token) {
            let session = self.sessions.borrow_mut().slab[session_token].clone();
            if session.borrow_mut().timeout(token) {
                self.kill_session(session);
            }
        }
    }

    pub fn handle_remaining_readiness(&mut self) {
        // try to accept again after handling all session events,
        // since we might have released a few session slots
        if self.sessions.borrow().can_accept && !self.accept_ready.is_empty() {
            while let Some(token) = self
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
            }
        }
    }
    fn block_channel(&mut self) {
        if let Err(e) = self.channel.blocking() {
            error!("Could not block channel: {}", e);
        }
    }
    fn unblock_channel(&mut self) {
        if let Err(e) = self.channel.nonblocking() {
            error!("Could not block channel: {}", e);
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

    fn print_session(&self) {}

    fn frontend_token(&self) -> Token {
        Token(0)
    }

    fn protocol(&self) -> Protocol {
        self.protocol
    }

    fn ready(&mut self, _session: Rc<RefCell<dyn ProxySession>>) -> SessionIsToBeClosed {
        false
    }

    fn shutting_down(&mut self) -> SessionIsToBeClosed {
        false
    }

    fn update_readiness(&mut self, _token: Token, _events: Ready) {}

    fn close(&mut self) {}

    fn timeout(&mut self, _token: Token) -> SessionIsToBeClosed {
        error!(
            "called ProxySession::timeout(token={:?}, time) on ListenSession {{ protocol: {:?} }}",
            _token, self.protocol
        );
        false
    }
}
