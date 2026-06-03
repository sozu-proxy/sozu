//! UDP load-balancing I/O shell + mio event-loop wiring (issue #1273).
//!
//! This module is the **impure** half of the UDP datapath: it owns every
//! syscall, the buffer copies, the per-flow connected upstream sockets, the
//! timer arming, the `BackendMap`/health/metrics edges, and the slab/token
//! bookkeeping. It drives the pure sans-io core in
//! [`crate::protocol::udp`](crate::protocol::udp) (the `UdpManager` /
//! `UdpFlow` two-level split) through `ManagerInput` / `Output`.
//!
//! Architecture (mirrors `tcp.rs`, but UDP is **one-listener-many-flows**):
//! - [`UdpListener`] wraps the single `mio::net::UdpSocket` a listener binds.
//!   There is **no accept loop** — a readable event means "datagrams waiting",
//!   not "a new connection".
//! - [`UdpProxy`] holds the listeners, the per-listener [`UdpManager`]s, the
//!   shared `BackendMap`, the session slab and the registry. It does **not**
//!   implement `ProxyConfiguration` / `L7Proxy` (their signatures are
//!   `TcpStream`-bound); the server calls its inherent
//!   [`notify`](UdpProxy::notify) directly.
//! - [`UdpListenerSession`] is the `ProxySession` the server's generic
//!   readiness path drives. One session backs one listener; its
//!   `update_readiness` demuxes by token (listener-token = client recv;
//!   upstream-token = backend recv). It owns the per-flow connected sockets and
//!   the `upstream_token -> FlowId` map.
//!
//! UDP never goes through `accept()` / `create_session()`: a `Protocol::UDP`
//! listener readable event falls through `Server::ready`'s generic arm into
//! `ProxySession::ready`.

use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, VecDeque, hash_map::Entry},
    io::ErrorKind,
    net::SocketAddr,
    os::unix::io::AsRawFd,
    rc::Rc,
    time::{Duration, Instant},
};

use mio::{Interest, Registry, Token, net::UdpSocket, unix::SourceFd};
use sozu_command::{
    logging::ansi_palette,
    proto::command::{
        Cluster, LoadBalancingAlgorithms, LoadMetric, RequestUdpFrontend, UdpAffinityKey,
        UdpListenerConfig, UpdateUdpListenerConfig, WorkerRequest, WorkerResponse,
        request::RequestType,
    },
};

use crate::metrics::names;
use crate::{
    CachedTags, ListenerError, ListenerHandler, Protocol, ProxyError, ProxySession,
    SessionIsToBeClosed,
    backends::BackendMap,
    pool::Pool,
    protocol::udp::{
        CloseReason, ClusterConfig, ConfigEvent, DropReason, FlowId, ManagerInput, MetricEvent,
        Output, UdpManager,
    },
    server::{SessionManager, TIMER},
    socket::{udp_bind, udp_connect},
    sozu_command::{ready::Ready, state::ClusterId},
};

mod health;
pub use health::UdpHealthChecker;

/// Per-session log envelope (tag `UDP`). The colored output uses the unified
/// scheme: bold bright-white protocol label, light-grey keyword, gray keys and
/// bright-white values. Honours the `colored` flag via [`ansi_palette`].
macro_rules! log_context {
    ($self:expr) => {{
        let (open, reset, grey, gray, white) = ansi_palette();
        format!(
            "[- - - -]\t{open}UDP{reset}\t{grey}Listener{reset}({gray}token{reset}={white}{token}{reset}, {gray}address{reset}={white}{address}{reset})\t >>>",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            token = $self.listener_token.0,
            address = $self.address,
        )
    }};
}

/// Module-level prefix for [`UdpProxy`] callbacks (notify, listener add/remove,
/// soft/hard stop) which own a listener/token map but have no per-session
/// token. Produces a bold bright-white `UDP` label in colored mode.
macro_rules! log_module_context {
    () => {{
        let (open, reset, _, _, _) = sozu_command::logging::ansi_palette();
        format!("{open}UDP{reset}\t >>>", open = open, reset = reset)
    }};
}

/// Per-flow log envelope (tag `UDP-FLOW`). Renders the flow's stable id plus
/// client/backend addresses so flow lines stay filterable.
macro_rules! log_flow_context {
    ($flow:expr, $client:expr, $backend:expr) => {{
        let (open, reset, grey, gray, white) = sozu_command::logging::ansi_palette();
        format!(
            "[- - - -]\t{open}UDP-FLOW{reset}\t{grey}Flow{reset}({gray}id{reset}={white}{id}{reset}, {gray}client{reset}={white}{client}{reset}, {gray}backend{reset}={white}{backend:?}{reset})\t >>>",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            id = $flow,
            client = $client,
            backend = $backend,
        )
    }};
}

/// Bound on the per-flow upstream write queue (return path to one backend). A
/// connected upstream socket only buffers one flow's traffic, so a small cap is
/// enough to ride out a transient `EWOULDBLOCK` (kernel send buffer momentarily
/// full) without letting a slow/stalled backend balloon memory. Past the cap we
/// drop the datagram (`udp.datagrams.dropped.wq_full`) — UDP is best-effort and
/// the client retries.
const UPSTREAM_WRITE_QUEUE_CAP: usize = 64;
/// Bound on the per-listener client-return write queue (replies fanned back to
/// many clients through the single listener socket). Sized larger than the
/// per-flow cap because it is shared across every flow's return traffic.
const CLIENT_WRITE_QUEUE_CAP: usize = 256;

/// What `WriteQueue::drain` should do after a single send attempt: keep
/// draining, stop because the socket went `WouldBlock` (rearm WRITABLE), or stop
/// because the send hit a hard error (drop the datagram, keep draining the rest).
enum SendOutcome {
    /// The datagram was written; pop it and continue.
    Sent,
    /// `WouldBlock`: leave the datagram at the front and stop draining.
    WouldBlock,
    /// A hard error (e.g. ECONNREFUSED): drop this datagram + count it, continue.
    Dropped,
}

/// A bounded FIFO of datagrams awaiting a writable socket. The egress fast path
/// (a `send`/`send_to` that succeeds immediately) never touches this; it is only
/// engaged when the kernel send buffer is full (`WouldBlock`). On overflow the
/// oldest-still-pending order is preserved and the *new* datagram is dropped
/// (the queued ones are closer to leaving the socket).
///
/// The queue stores `(SocketAddr, Vec<u8>)`: the upstream variant ignores the
/// address (a connected socket's `send` has an implicit destination) while the
/// client-return variant uses it as the `send_to` destination. Keeping one type
/// for both lets the drain loop and the unit tests stay shared.
struct WriteQueue {
    queue: VecDeque<(SocketAddr, Vec<u8>)>,
    cap: usize,
}

impl WriteQueue {
    fn new(cap: usize) -> Self {
        WriteQueue {
            queue: VecDeque::new(),
            cap,
        }
    }

    fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Current depth. Only used by the unit tests; the runtime drains by the
    /// `is_empty` / `drain` return value, not a length read.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.queue.len()
    }

    /// Enqueue a datagram for later draining. Returns `true` if it was accepted,
    /// `false` if the queue was at capacity (caller drops + counts `wq_full`).
    #[must_use]
    fn push(&mut self, dst: SocketAddr, payload: Vec<u8>) -> bool {
        if self.queue.len() >= self.cap {
            return false;
        }
        self.queue.push_back((dst, payload));
        true
    }

    /// Drain in FIFO order, calling `send` for each datagram. Stops on the first
    /// `WouldBlock` (leaving that datagram queued for the next writable event) or
    /// when empty. Hard-errored datagrams are popped and counted via
    /// `SendOutcome::Dropped`. Returns `true` if the queue is now empty (the
    /// caller can drop WRITABLE interest back to READABLE-only).
    fn drain<F: FnMut(&SocketAddr, &[u8]) -> SendOutcome>(&mut self, mut send: F) -> bool {
        while let Some((dst, payload)) = self.queue.front() {
            match send(dst, payload) {
                SendOutcome::Sent | SendOutcome::Dropped => {
                    self.queue.pop_front();
                }
                SendOutcome::WouldBlock => break,
            }
        }
        self.queue.is_empty()
    }
}

/// One UDP listener: a single `mio::net::UdpSocket` plus its routing config.
/// Unlike a TCP listener there is no accept loop — a readable event is a batch
/// of datagrams the session drains to `WouldBlock`.
pub struct UdpListener {
    active: SessionIsToBeClosed,
    address: SocketAddr,
    cluster_id: Option<String>,
    config: UdpListenerConfig,
    socket: Option<UdpSocket>,
    tags: BTreeMap<String, CachedTags>,
    token: Token,
}

impl ListenerHandler for UdpListener {
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

    fn protocol(&self) -> Protocol {
        Protocol::UDP
    }

    fn public_address(&self) -> SocketAddr {
        self.config
            .public_address
            .map(|addr| addr.into())
            .unwrap_or(self.address)
    }
}

impl UdpListener {
    fn new(config: UdpListenerConfig, token: Token) -> Result<UdpListener, ListenerError> {
        Ok(UdpListener {
            cluster_id: None,
            socket: None,
            token,
            address: config.address.into(),
            config,
            active: false,
            tags: BTreeMap::new(),
        })
    }

    /// Bind (or adopt an SCM-passed) socket and register it `READABLE`. The
    /// READABLE registration is what drives `Server::ready` for this listener —
    /// there is no accept path.
    pub fn activate(
        &mut self,
        registry: &Registry,
        udp_socket: Option<UdpSocket>,
    ) -> Result<Token, ProxyError> {
        if self.active {
            return Ok(self.token);
        }

        let mut socket = match udp_socket {
            Some(socket) => socket,
            None => {
                let address: SocketAddr = self.config.address.into();
                udp_bind(address).map_err(|e| ProxyError::BindToSocket(address, e))?
            }
        };

        registry
            .register(&mut socket, self.token, Interest::READABLE)
            .map_err(ProxyError::RegisterListener)?;

        self.socket = Some(socket);
        self.active = true;
        Ok(self.token)
    }

    /// Apply a partial-update patch to this UDP listener's live config. Fields
    /// absent in the patch (`None`) are preserved.
    pub fn update_config(&mut self, patch: &UpdateUdpListenerConfig) {
        if let Some(v) = patch.public_address {
            self.config.public_address = Some(v);
        }
        if let Some(v) = patch.front_timeout {
            self.config.front_timeout = v;
        }
        if let Some(v) = patch.back_timeout {
            self.config.back_timeout = v;
        }
        if let Some(v) = patch.max_rx_datagram_size {
            self.config.max_rx_datagram_size = v;
        }
        if let Some(v) = patch.max_flows {
            self.config.max_flows = v;
        }
    }
}

/// The UDP proxy. Holds the listeners, one `UdpManager` per listener token, the
/// shared `BackendMap`, the session slab and a cloned registry. Does NOT
/// implement `ProxyConfiguration` / `L7Proxy`; the server drives it through the
/// inherent [`notify`](Self::notify) plus the activate/give-back helpers.
pub struct UdpProxy {
    fronts: HashMap<String, Token>,
    backends: Rc<RefCell<BackendMap>>,
    listeners: HashMap<Token, Rc<RefCell<UdpListener>>>,
    /// The built listener session per listener token. The server inserts the
    /// same `Rc` into the slab at the listener token; the proxy keeps a clone so
    /// it can drive per-flow teardown (soft/hard stop) without downcasting the
    /// slab's `dyn ProxySession`. Cleared on listener removal/stop.
    listener_sessions: HashMap<Token, Rc<RefCell<UdpListenerSession>>>,
    /// One sans-io manager per listener token, sharing the listener's lifecycle.
    managers: HashMap<Token, Rc<RefCell<UdpManager>>>,
    /// Cluster routing for each listener token (set by the UDP frontend).
    cluster_for_listener: HashMap<Token, ClusterId>,
    /// Last `AddCluster`-supplied UDP knobs per cluster id. Cached so a frontend
    /// added AFTER its cluster still picks up `responses` / `requests` / PPv2 /
    /// affinity — `AddCluster` and `AddUdpFrontend` arrive in either order, and
    /// neither must clobber the other's contribution to a manager's
    /// `ClusterConfig`.
    cluster_udp_config: HashMap<ClusterId, sozu_command::proto::command::UdpClusterConfig>,
    registry: Registry,
    sessions: Rc<RefCell<SessionManager>>,
    #[allow(dead_code)]
    pool: Rc<RefCell<Pool>>,
    /// Fixed hash seed injected once into every manager. It is deliberately the
    /// same constant on every worker and across restarts so HRW/Maglev affinity
    /// is reproducible cluster-wide — a per-worker random seed would scatter the
    /// same flow key onto a different backend on each worker and reshuffle it on
    /// every restart, defeating the documented affinity-stability contract (see
    /// [`crate::load_balancing::DEFAULT_HASH_SEED`]).
    hash_seed: u64,
    /// Global `max_connections` (from `ServerConfig`). Used to clamp a UDP
    /// listener's auto `max_flows` so a single listener cannot inflate the
    /// shared `SessionManager` slab and starve HTTP/TCP.
    max_connections: usize,
    /// Global `buffer_size` (from `ServerConfig`). Used to clamp a listener's
    /// `max_rx_datagram_size` so a hostile `AddUdpListener` cannot allocate a
    /// multi-GB recv buffer.
    buffer_size: usize,
    /// Endpoint-bound active health prober (TCP probe + hysteresis +
    /// fail-open). Driven from the server event loop via
    /// [`UdpProxy::health_poll`] / [`UdpProxy::health_ready`].
    health: UdpHealthChecker,
}

impl UdpProxy {
    pub fn new(
        registry: Registry,
        sessions: Rc<RefCell<SessionManager>>,
        pool: Rc<RefCell<Pool>>,
        backends: Rc<RefCell<BackendMap>>,
        max_connections: usize,
        buffer_size: usize,
    ) -> UdpProxy {
        // Fixed seed, identical on every worker and across restarts: HRW/Maglev
        // affinity must be reproducible cluster-wide. A per-worker/per-restart
        // random seed (as a `process::id()` + `Instant::now()` mix would be)
        // would route the same flow key to a different backend on each worker
        // and reshuffle it on every restart. Reuse the LB module's canonical
        // affinity seed so UDP and the HTTP/TCP affinity hashers agree.
        let hash_seed = crate::load_balancing::DEFAULT_HASH_SEED;
        UdpProxy {
            backends,
            listeners: HashMap::new(),
            listener_sessions: HashMap::new(),
            managers: HashMap::new(),
            cluster_for_listener: HashMap::new(),
            cluster_udp_config: HashMap::new(),
            fronts: HashMap::new(),
            registry,
            sessions,
            pool,
            hash_seed,
            max_connections,
            buffer_size,
            health: UdpHealthChecker::new(),
        }
    }

    /// Drive the UDP health prober one event-loop step (server calls this once
    /// per iteration, mirroring `HealthChecker::poll`). Non-blocking.
    pub fn health_poll(&mut self) {
        let registry = self.registry.try_clone();
        if let Ok(registry) = registry {
            self.health.poll(&self.backends, &registry);
        }
    }

    /// Record mio readiness for a UDP health-probe socket.
    pub fn health_ready(&mut self, token: Token) {
        self.health.ready(token);
    }

    /// Whether `token` is a UDP health-probe socket this proxy owns.
    pub fn health_owns_token(&self, token: Token) -> bool {
        self.health.owns_token(token)
    }

    pub fn add_listener(
        &mut self,
        config: UdpListenerConfig,
        token: Token,
    ) -> Result<Token, ProxyError> {
        match self.listeners.entry(token) {
            Entry::Vacant(entry) => {
                let mut config = config;
                let max_flows = effective_max_flows(config.max_flows, self.max_connections);
                // Defense in depth: cap the recv-buffer sizing to `buffer_size`
                // so a hostile `AddUdpListener` (`max_rx_datagram_size =
                // u32::MAX`) can't allocate a multi-GB buffer. Write the clamped
                // value back into the stored config so the manager AND the
                // session's `recv_buf` (sized from `config` in `new()`) agree on
                // the same bound.
                let max_rx = clamp_max_rx(config.max_rx_datagram_size as usize, self.buffer_size);
                config.max_rx_datagram_size = max_rx as u32;
                let front = Duration::from_secs(u64::from(config.front_timeout));
                let back = Duration::from_secs(u64::from(config.back_timeout));
                let listener = UdpListener::new(config, token).map_err(ProxyError::AddListener)?;
                entry.insert(Rc::new(RefCell::new(listener)));
                let cluster_cfg = ClusterConfig {
                    front_timeout: front,
                    back_timeout: back,
                    ..Default::default()
                };
                self.managers.insert(
                    token,
                    Rc::new(RefCell::new(UdpManager::new(
                        cluster_cfg,
                        max_flows,
                        max_rx,
                        self.hash_seed,
                    ))),
                );
                Ok(token)
            }
            _ => Err(ProxyError::ListenerAlreadyPresent),
        }
    }

    pub fn remove_listener(&mut self, address: SocketAddr) -> SessionIsToBeClosed {
        let len = self.listeners.len();
        let mut removed_tokens = Vec::new();
        self.listeners.retain(|token, l| {
            if l.borrow().address == address {
                removed_tokens.push(*token);
                false
            } else {
                true
            }
        });
        let now = Instant::now();
        for token in removed_tokens {
            self.cluster_for_listener.remove(&token);
            // Drive per-flow teardown THROUGH the manager (emits FlowEvicted +
            // CloseFlow per flow) BEFORE dropping the manager, so the
            // active-flows gauge is decremented once per flow and the per-flow
            // upstream sockets + slab slots are freed. Removing the manager first
            // would silently leak the gauge by N.
            if let Some(session) = self.listener_sessions.remove(&token) {
                session.borrow_mut().close_all_flows(now);
            }
            self.managers.remove(&token);
        }
        self.listeners.len() < len
    }

    pub fn activate_listener(
        &self,
        addr: &SocketAddr,
        udp_socket: Option<UdpSocket>,
    ) -> Result<Token, ProxyError> {
        let listener = self
            .listeners
            .values()
            .find(|listener| listener.borrow().address == *addr)
            .ok_or(ProxyError::NoListenerFound(*addr))?;

        listener.borrow_mut().activate(&self.registry, udp_socket)
    }

    /// Build the [`UdpListenerSession`] that drives this listener's datagrams.
    /// The server inserts the returned session into the slab **at the listener
    /// token**, replacing the `ListenSession` placeholder, so the generic
    /// readiness path reaches `UdpListenerSession::update_readiness`. Returns
    /// `None` if the listener token is unknown.
    pub fn build_session(&mut self, token: Token) -> Option<Rc<RefCell<UdpListenerSession>>> {
        let listener = self.listeners.get(&token)?.clone();
        let manager = self.managers.get(&token)?.clone();
        let registry = self.registry.try_clone().ok()?;
        let session = Rc::new(RefCell::new(UdpListenerSession::new(
            listener,
            manager,
            self.backends.clone(),
            registry,
            self.sessions.clone(),
            token,
        )));
        // Keep a clone so soft/hard stop can drive per-flow teardown.
        self.listener_sessions.insert(token, session.clone());
        Some(session)
    }

    pub fn give_back_listeners(&mut self) -> Vec<(SocketAddr, UdpSocket)> {
        self.listeners
            .values()
            .filter_map(|listener| {
                let mut owned = listener.borrow_mut();
                if let Some(socket) = owned.socket.take() {
                    owned.active = false;
                    return Some((owned.address, socket));
                }
                None
            })
            .collect()
    }

    pub fn give_back_listener(
        &mut self,
        address: SocketAddr,
    ) -> Result<(Token, UdpSocket), ProxyError> {
        let listener = self
            .listeners
            .values()
            .find(|listener| listener.borrow().address == address)
            .ok_or(ProxyError::NoListenerFound(address))?;

        let (token, taken) = {
            let mut owned = listener.borrow_mut();
            let taken = owned.socket.take().ok_or(ProxyError::UnactivatedListener)?;
            owned.active = false;
            (owned.token, taken)
        };
        // Deactivating removes the listener from service: tear down its active
        // flows THROUGH the manager so the per-flow upstream slab slots + fds
        // don't dangle and the active-flows gauge is decremented once per flow
        // (the manager is RETAINED here — not removed from `self.managers` — so
        // `close_all` also resets its flow table, keeping manager and shell
        // consistent for a later reactivate).
        if let Some(session) = self.listener_sessions.remove(&token) {
            session.borrow_mut().close_all_flows(Instant::now());
        }
        Ok((token, taken))
    }

    pub fn update_listener(&mut self, patch: UpdateUdpListenerConfig) -> Result<(), ProxyError> {
        let address: SocketAddr = patch.address.into();
        let listener = self
            .listeners
            .values()
            .find(|l| l.borrow().address == address)
            .ok_or(ProxyError::NoListenerFound(address))?;
        {
            let mut l = listener.borrow_mut();
            l.update_config(&patch);
            // Clamp the stored rx size in place (defense in depth): keep the
            // listener config, the manager, and the session's `recv_buf` agreeing
            // on the same `buffer_size`-bounded value.
            l.config.max_rx_datagram_size =
                clamp_max_rx(l.config.max_rx_datagram_size as usize, self.buffer_size) as u32;
        }

        // Reflect the timeout / cap / rx-size changes into the manager for new
        // flows. Existing flows keep their captured config (stable contract).
        if let Some(token) = self
            .listeners
            .iter()
            .find(|(_, l)| l.borrow().address == address)
            .map(|(t, _)| *t)
            && let Some(mgr) = self.managers.get(&token)
        {
            let now = Instant::now();
            let (cfg, max_flows, max_rx) = {
                let l = listener.borrow();
                (
                    self.cluster_config_for(&l, token),
                    effective_max_flows(l.config.max_flows, self.max_connections),
                    // Defense in depth: clamp the raised rx size to `buffer_size`
                    // so a hostile `UpdateUdpListener` can't grow the recv buffer
                    // past the global cap.
                    clamp_max_rx(l.config.max_rx_datagram_size as usize, self.buffer_size),
                )
            };
            {
                let mut m = mgr.borrow_mut();
                m.handle_input(ManagerInput::Config(ConfigEvent::SetCluster(cfg)), now);
                m.handle_input(
                    ManagerInput::Config(ConfigEvent::SetMaxFlows(max_flows)),
                    now,
                );
                m.handle_input(
                    ManagerInput::Config(ConfigEvent::SetMaxRxDatagramSize(max_rx)),
                    now,
                );
            }
            // Resize the live session's recv scratch to match the (clamped) new
            // rx size. Without this, a config that RAISES `max_rx_datagram_size`
            // would leave `recv_buf` at its old (smaller) length: `recv_from`
            // would kernel-truncate datagrams between the old and new size while
            // the manager's `len > max_rx` check (now the larger value) passes
            // them, forwarding them truncated. The `+ 1` mirrors `new()`: it lets
            // the manager observe `len == max_rx + 1 > max_rx` and drop an
            // oversized datagram (`DropReason::Truncated`) instead of silently
            // truncating it. Existing flows keep their captured config; only the
            // shared recv buffer tracks the live rx size.
            if let Some(session) = self.listener_sessions.get(&token) {
                session.borrow_mut().resize_recv_buf(max_rx);
            }
        }
        Ok(())
    }

    pub fn add_udp_front(&mut self, front: RequestUdpFrontend) -> Result<(), ProxyError> {
        let address = front.address.into();
        let token = {
            let mut listener = self
                .listeners
                .values()
                .find(|l| l.borrow().address == address)
                .ok_or(ProxyError::NoListenerFound(address))?
                .borrow_mut();
            self.fronts
                .insert(front.cluster_id.to_string(), listener.token);
            listener.set_tags(address.to_string(), Some(front.tags));
            listener.cluster_id = Some(front.cluster_id.clone());
            listener.token
        };
        self.cluster_for_listener
            .insert(token, front.cluster_id.clone());

        // Commit the cluster routing into the manager so admitted flows know
        // which cluster to `SelectBackend` against.
        if let Some(mgr) = self.managers.get(&token) {
            let listener = self.listeners.get(&token).unwrap();
            let cfg = {
                let l = listener.borrow();
                self.cluster_config_for(&l, token)
            };
            mgr.borrow_mut().handle_input(
                ManagerInput::Config(ConfigEvent::SetCluster(cfg)),
                Instant::now(),
            );
        }
        Ok(())
    }

    pub fn remove_udp_front(&mut self, front: RequestUdpFrontend) -> Result<(), ProxyError> {
        let address = front.address.into();
        let token = {
            let mut listener = match self
                .listeners
                .values()
                .find(|l| l.borrow().address == address)
            {
                Some(l) => l.borrow_mut(),
                None => return Err(ProxyError::NoListenerFound(address)),
            };
            listener.set_tags(address.to_string(), None);
            if let Some(cluster_id) = listener.cluster_id.take() {
                self.fronts.remove(&cluster_id);
            }
            listener.token
        };
        self.cluster_for_listener.remove(&token);
        // Drop the routing in the manager — new datagrams now have no backend.
        if let Some(mgr) = self.managers.get(&token) {
            mgr.borrow_mut().handle_input(
                ManagerInput::Config(ConfigEvent::SetCluster(ClusterConfig::default())),
                Instant::now(),
            );
        }
        Ok(())
    }

    /// Build a [`ClusterConfig`] for a listener from its current frontend
    /// cluster routing + timeouts. The per-cluster knobs (responses/requests/
    /// PPv2/affinity) are populated by [`apply_cluster`](Self::apply_cluster)
    /// when an `AddCluster` carries a `udp` block; absent that they stay at the
    /// proto defaults.
    fn cluster_config_for(&self, listener: &UdpListener, _token: Token) -> ClusterConfig {
        let cluster = listener.cluster_id.clone().unwrap_or_default();
        let mut cfg = ClusterConfig {
            cluster: cluster.clone(),
            front_timeout: Duration::from_secs(u64::from(listener.config.front_timeout)),
            back_timeout: Duration::from_secs(u64::from(listener.config.back_timeout)),
            ..Default::default()
        };
        // Fold in the cluster's cached UDP knobs (set by a prior or later
        // `AddCluster`) so the order of `AddCluster` vs `AddUdpFrontend` does not
        // matter — both rebuilds of a manager's `ClusterConfig` converge on the
        // same per-cluster contract.
        if let Some(udp) = self.cluster_udp_config.get(&cluster) {
            apply_udp_knobs(&mut cfg, udp);
        }
        cfg
    }

    /// Apply an `AddCluster` to every listener routing to it: fold the UDP
    /// cluster knobs (affinity / responses / requests / PPv2) into the
    /// manager's `ClusterConfig`, and rebuild the LB policy (HRW/Maglev table)
    /// via the shared `BackendMap`.
    fn apply_cluster(&mut self, cluster: &Cluster) {
        // 1. LB policy (HRW/Maglev rebuild happens inside the BackendMap).
        self.backends
            .borrow_mut()
            .set_load_balancing_policy_for_cluster(
                &cluster.cluster_id,
                LoadBalancingAlgorithms::try_from(cluster.load_balancing).unwrap_or_default(),
                cluster
                    .load_metric
                    .and_then(|n| LoadMetric::try_from(n).ok()),
            );

        // 1b. Health settings from the cluster's `udp.health` block, if any.
        // Mode HEALTH_OFF / no health block disables probing for this cluster.
        let health_settings = cluster.udp.as_ref().and_then(|udp| {
            udp.health.as_ref().and_then(|h| {
                let mode = h
                    .mode
                    .and_then(|m| sozu_command::proto::command::UdpHealthMode::try_from(m).ok());
                match mode {
                    Some(sozu_command::proto::command::UdpHealthMode::HealthOff) => None,
                    // TCP_PROBE (default when a health block is present) and
                    // UDP_PROBE both schedule the TCP-probe state machine; the
                    // app UDP-probe payload rides along for the secondary check.
                    _ => Some(health::UdpHealthSettings::from_proto(h)),
                }
            })
        });
        self.health
            .set_cluster(&cluster.cluster_id, health_settings, &self.registry);

        // 1c. Cache the UDP knobs so a frontend added in EITHER order picks them
        // up via `cluster_config_for`. An `AddCluster` without a `udp` block
        // clears the cache (back to proto defaults).
        match &cluster.udp {
            Some(udp) => {
                self.cluster_udp_config
                    .insert(cluster.cluster_id.clone(), udp.clone());
            }
            None => {
                self.cluster_udp_config.remove(&cluster.cluster_id);
            }
        }

        // 2. Per-cluster UDP knobs into the managers routing to this cluster.
        // `cluster_config_for` now folds the cached knobs in, so the rebuild is
        // identical regardless of whether the frontend or the cluster came
        // first.
        let now = Instant::now();
        let tokens: Vec<Token> = self
            .cluster_for_listener
            .iter()
            .filter(|(_, c)| **c == cluster.cluster_id)
            .map(|(t, _)| *t)
            .collect();
        for token in tokens {
            let Some(listener) = self.listeners.get(&token) else {
                continue;
            };
            let cfg = {
                let l = listener.borrow();
                self.cluster_config_for(&l, token)
            };
            if let Some(mgr) = self.managers.get(&token) {
                mgr.borrow_mut()
                    .handle_input(ManagerInput::Config(ConfigEvent::SetCluster(cfg)), now);
            }
        }
    }

    /// Inherent dispatch entry point — the server calls this directly (UDP does
    /// not implement `ProxyConfiguration`). Handles UDP frontends, cluster
    /// config, listener removal, and stop. No accept / create_session.
    pub fn notify(&mut self, message: WorkerRequest) -> WorkerResponse {
        let request_type = match message.content.request_type {
            Some(t) => t,
            None => return WorkerResponse::error(message.id, "Empty request"),
        };
        match request_type {
            RequestType::AddUdpFrontend(front) => match self.add_udp_front(front) {
                Ok(()) => WorkerResponse::ok(message.id),
                Err(err) => WorkerResponse::error(message.id, err),
            },
            RequestType::RemoveUdpFrontend(front) => match self.remove_udp_front(front) {
                Ok(()) => WorkerResponse::ok(message.id),
                Err(err) => WorkerResponse::error(message.id, err),
            },
            RequestType::AddCluster(cluster) => {
                self.apply_cluster(&cluster);
                WorkerResponse::ok(message.id)
            }
            RequestType::RemoveCluster(cluster_id) => {
                let tokens: Vec<Token> = self
                    .cluster_for_listener
                    .iter()
                    .filter(|(_, c)| **c == cluster_id)
                    .map(|(t, _)| *t)
                    .collect();
                for token in tokens {
                    if let Some(mgr) = self.managers.get(&token) {
                        mgr.borrow_mut().handle_input(
                            ManagerInput::Config(ConfigEvent::SetCluster(ClusterConfig::default())),
                            Instant::now(),
                        );
                    }
                }
                self.cluster_udp_config.remove(&cluster_id);
                self.health.remove_cluster(&cluster_id, &self.registry);
                WorkerResponse::ok(message.id)
            }
            RequestType::SoftStop(_) => {
                info!(
                    "{} {} processing soft shutdown",
                    log_module_context!(),
                    message.id
                );
                // Drain: admit no new flows. Then actively tear down existing
                // flows so the worker reaches `base_sessions_count` and exits
                // promptly instead of waiting out each flow's idle timeout. UDP
                // has no half-sent response to preserve, so an immediate flow
                // teardown on soft-stop is the right graceful behavior (a stray
                // in-flight reply may be lost, which is acceptable for a
                // best-effort datagram proxy).
                let now = Instant::now();
                for mgr in self.managers.values() {
                    mgr.borrow_mut()
                        .handle_input(ManagerInput::Config(ConfigEvent::Drain), now);
                }
                // Drive teardown through each manager (FlowEvicted + CloseFlow
                // per flow) so the active-flows gauge balances to zero.
                for session in self.listener_sessions.values() {
                    session.borrow_mut().close_all_flows(now);
                }
                self.listener_sessions.clear();
                let listeners: HashMap<_, _> = self.listeners.drain().collect();
                for (_, l) in listeners.iter() {
                    l.borrow_mut()
                        .socket
                        .take()
                        .map(|mut sock| self.registry.deregister(&mut sock));
                }
                WorkerResponse::processing(message.id)
            }
            RequestType::HardStop(_) => {
                info!("{} {} hard shutdown", log_module_context!(), message.id);
                let now = Instant::now();
                // Drive teardown through each manager (FlowEvicted + CloseFlow
                // per flow) so the active-flows gauge balances to zero before the
                // managers are dropped below.
                for session in self.listener_sessions.values() {
                    session.borrow_mut().close_all_flows(now);
                }
                self.listener_sessions.clear();
                let mut listeners: HashMap<_, _> = self.listeners.drain().collect();
                for (_, l) in listeners.drain() {
                    l.borrow_mut()
                        .socket
                        .take()
                        .map(|mut sock| self.registry.deregister(&mut sock));
                }
                self.managers.clear();
                WorkerResponse::ok(message.id)
            }
            RequestType::Status(_) => {
                info!("{} {} status", log_module_context!(), message.id);
                WorkerResponse::ok(message.id)
            }
            RequestType::RemoveListener(remove) => {
                if !self.remove_listener(remove.address.into()) {
                    WorkerResponse::error(
                        message.id,
                        format!("no UDP listener to remove at address {:?}", remove.address),
                    )
                } else {
                    WorkerResponse::ok(message.id)
                }
            }
            command => {
                debug!(
                    "{} {} unsupported message for UDP proxy, ignoring {:?}",
                    log_module_context!(),
                    message.id,
                    command
                );
                WorkerResponse::error(message.id, "unsupported message")
            }
        }
    }
}

/// Fold a proto [`UdpClusterConfig`](sozu_command::proto::command::UdpClusterConfig)
/// into a [`ClusterConfig`], applying the proto defaults for absent fields.
/// Single source of truth so `apply_cluster` (live push) and
/// `cluster_config_for` (frontend add / rebuild) never diverge.
fn apply_udp_knobs(cfg: &mut ClusterConfig, udp: &sozu_command::proto::command::UdpClusterConfig) {
    cfg.affinity_with_port = matches!(
        udp.affinity_key
            .and_then(|k| UdpAffinityKey::try_from(k).ok()),
        Some(UdpAffinityKey::SourceIpPort)
    );
    cfg.responses = udp.responses.unwrap_or(0);
    cfg.requests = udp.requests.unwrap_or(0);
    cfg.send_proxy_protocol = udp.send_proxy_protocol.unwrap_or(false);
    cfg.proxy_protocol_every_datagram = udp.proxy_protocol_every_datagram.unwrap_or(false);
}

/// Fallback auto `max_flows` when `RLIMIT_NOFILE` can't be read.
const DEFAULT_AUTO_MAX_FLOWS: usize = 1024;

/// `max_flows == 0` means "auto": ~70% of the soft `RLIMIT_NOFILE`, so the fd
/// budget adapts to the host without hand-tuning (one fd + one slab slot per
/// flow). Falls back to a conservative constant when the limit can't be read.
///
/// The auto derivation is clamped to `slab_headroom` (the global
/// `max_connections`): every admitted flow consumes one shared `SessionManager`
/// slab slot, so an unclamped ~70%-of-RLIMIT value on a host with a very large
/// fd limit could try to inflate the slab to hundreds of thousands of entries
/// and starve HTTP/TCP. An *explicitly configured* `max_flows` is honoured as-is
/// (the operator opted in); only the auto value is capped. A `slab_headroom` of
/// 0 (no connection budget configured) disables the clamp.
fn effective_max_flows(configured: u32, slab_headroom: usize) -> usize {
    if configured != 0 {
        return configured as usize;
    }
    let auto = {
        #[cfg(unix)]
        {
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: `getrlimit` writes a fully-initialised `rlimit` into
            // `limit`; we read the result only on success (`== 0`).
            let ret = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) };
            if ret == 0 && limit.rlim_cur > 0 {
                let soft = limit.rlim_cur;
                ((soft.saturating_mul(7)) / 10).max(1) as usize
            } else {
                DEFAULT_AUTO_MAX_FLOWS
            }
        }
        #[cfg(not(unix))]
        {
            DEFAULT_AUTO_MAX_FLOWS
        }
    };
    if slab_headroom == 0 {
        auto
    } else {
        auto.min(slab_headroom).max(1)
    }
}

/// Clamp a configured `max_rx_datagram_size` to the global `buffer_size`
/// (defense in depth): the per-session `recv_buf` is sized `max_rx + 1`, so a
/// raw worker `AddUdpListener`/`UpdateUdpListener` carrying e.g.
/// `max_rx_datagram_size = u32::MAX` must NOT be able to allocate a ~4 GB
/// buffer. A `buffer_size` of 0 (unset) leaves the value untouched.
fn clamp_max_rx(configured: usize, buffer_size: usize) -> usize {
    if buffer_size == 0 {
        configured
    } else {
        configured.min(buffer_size)
    }
}

/// The `ProxySession` backing one UDP listener. The server's generic readiness
/// path drives this (UDP is not in the listen-accept arm). It owns the per-flow
/// connected upstream sockets and demuxes events by token.
pub struct UdpListenerSession {
    /// The listener this session serves.
    listener: Rc<RefCell<UdpListener>>,
    /// The sans-io manager for this listener.
    manager: Rc<RefCell<UdpManager>>,
    /// Shared backend map for LB selection.
    backends: Rc<RefCell<BackendMap>>,
    /// Cloned mio registry for per-flow socket (de)registration.
    registry: Registry,
    /// Slab the per-flow upstream tokens are inserted into (the same `Rc` the
    /// listener-session is registered under: multi-token pattern).
    sessions: Rc<RefCell<SessionManager>>,
    /// The listener's own token (client recv + timer key).
    listener_token: Token,
    /// Listener address (for logging).
    address: SocketAddr,
    /// Per-flow connected upstream sockets, keyed by their slab token.
    upstream_sockets: HashMap<Token, UdpSocket>,
    /// Per-flow bounded egress queues for the forward path, keyed by the
    /// upstream token. A datagram lands here only when the connected upstream
    /// socket returns `WouldBlock`; the socket is then reregistered
    /// `READABLE | WRITABLE` and drained on the next writable event. Dropped
    /// together with the socket on flow close (no leak, gauge stays correct).
    upstream_write_queues: HashMap<Token, WriteQueue>,
    /// Bounded egress queue for the client-return path (replies fanned back
    /// through the single listener socket via `send_to`). Engaged only when the
    /// listener socket returns `WouldBlock`; the listener is then reregistered
    /// `READABLE | WRITABLE` and drained on its next writable event.
    client_write_queue: WriteQueue,
    /// `upstream_token -> FlowId` for NAT-return demux.
    upstream_to_flow: HashMap<Token, FlowId>,
    /// `FlowId -> upstream_token` to tear down on close.
    flow_to_upstream: HashMap<FlowId, Token>,
    /// `FlowId -> admission Instant` for `udp.flow.duration` on close.
    flow_started: HashMap<FlowId, Instant>,
    /// `FlowId -> (client, backend)` for the close access log.
    flow_endpoints: HashMap<FlowId, (SocketAddr, Option<SocketAddr>)>,
    /// The client source whose datagram is currently being drained. Lets
    /// `on_send_to_backend` resolve the right per-flow upstream socket for an
    /// already-established flow (where no `OpenUpstream` precedes the send).
    /// `None` while draining backend-side or timer-driven outputs.
    in_flight_client: Option<SocketAddr>,
    /// The flow whose upstream was just opened in this drain pass — covers the
    /// new-flow path where `OpenUpstream{flow}` immediately precedes the first
    /// `SendToBackend`.
    in_flight_flow: Option<FlowId>,
    /// Shadow of the manager's flow table: normalised client key → `FlowId`.
    /// Lets the shell resolve the owning flow for a `SendToBackend` on an
    /// established flow from the in-flight client source. Kept in lockstep with
    /// `OpenUpstream` / `CloseFlow`.
    client_key_to_flow: HashMap<SocketAddr, FlowId>,
    /// Reusable recv scratch buffer, sized to `max_rx_datagram_size`.
    recv_buf: Vec<u8>,
    /// The currently-armed `TIMER` handle, so a re-arm cancels the previous
    /// deadline instead of leaking timer-slab entries (the manager only emits a
    /// fresh `ArmTimer` when the deadline actually changes).
    timer_handle: Option<crate::timer::Timeout>,
}

impl UdpListenerSession {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        listener: Rc<RefCell<UdpListener>>,
        manager: Rc<RefCell<UdpManager>>,
        backends: Rc<RefCell<BackendMap>>,
        registry: Registry,
        sessions: Rc<RefCell<SessionManager>>,
        listener_token: Token,
    ) -> UdpListenerSession {
        let (address, max_rx) = {
            let l = listener.borrow();
            (l.address, l.config.max_rx_datagram_size as usize)
        };
        UdpListenerSession {
            listener,
            manager,
            backends,
            registry,
            sessions,
            listener_token,
            address,
            upstream_sockets: HashMap::new(),
            upstream_write_queues: HashMap::new(),
            client_write_queue: WriteQueue::new(CLIENT_WRITE_QUEUE_CAP),
            upstream_to_flow: HashMap::new(),
            flow_to_upstream: HashMap::new(),
            flow_started: HashMap::new(),
            flow_endpoints: HashMap::new(),
            in_flight_client: None,
            in_flight_flow: None,
            client_key_to_flow: HashMap::new(),
            // Size the recv scratch to `max_rx + 1`, NOT `max_rx`: a UDP
            // `recv_from` truncates the datagram to the buffer length and
            // silently discards the tail. If the buffer were exactly `max_rx`,
            // an oversized datagram would arrive as a `max_rx`-byte payload —
            // indistinguishable from a legal one — and be forwarded truncated.
            // The extra byte lets the manager observe `len == max_rx + 1 >
            // max_rx` and drop it (`DropReason::Truncated`) instead.
            recv_buf: vec![0u8; max_rx.saturating_add(1).max(1)],
            timer_handle: None,
        }
    }

    /// Resize the recv scratch buffer to `max_rx + 1` (the same `+ 1` sizing as
    /// [`new`](Self::new): the extra byte lets the manager observe an oversized
    /// datagram as `len == max_rx + 1 > max_rx` and drop it as `Truncated`
    /// rather than silently forwarding a kernel-truncated payload). Called from
    /// `UdpProxy::update_listener` when a config push changes the listener's
    /// `max_rx_datagram_size`. Resizing larger zero-fills the new tail; resizing
    /// smaller truncates the (idle, between-datagram) scratch — both are safe
    /// because the buffer holds no live datagram across calls.
    fn resize_recv_buf(&mut self, max_rx: usize) {
        self.recv_buf.resize(max_rx.saturating_add(1).max(1), 0u8);
    }

    /// Normalise a client source the same way the active manager config keys
    /// flows (4-tuple when `affinity_with_port`, else source-IP with port 0).
    fn client_key(&self, src: SocketAddr) -> SocketAddr {
        let with_port = self.manager.borrow().affinity_with_port();
        if with_port {
            src
        } else {
            let mut s = src;
            s.set_port(0);
            s
        }
    }

    /// Drain every datagram waiting on the listener socket into the manager.
    /// Edge-triggered epoll: loop `recv_from` to `WouldBlock`.
    fn ingest_client(&mut self, now: Instant) {
        // Pull the socket out behind a short borrow then operate on it.
        loop {
            let result = {
                let listener = self.listener.borrow();
                let Some(socket) = listener.socket.as_ref() else {
                    return;
                };
                socket.recv_from(&mut self.recv_buf)
            };
            match result {
                Ok((len, src)) => {
                    // Mark the in-flight client so `on_send_to_backend` can
                    // resolve the owning flow's upstream socket for an
                    // already-established flow (no `OpenUpstream` precedes it).
                    self.in_flight_client = Some(src);
                    self.in_flight_flow = None;
                    let len = len.min(self.recv_buf.len());
                    // Borrow the payload via a split so the mutable borrow of
                    // `self.manager` does not alias `self.recv_buf`.
                    let payload: &[u8] = &self.recv_buf[..len];
                    // SAFETY-free: `handle_input` only reads the slice; the
                    // borrow checker is satisfied because `recv_buf` and
                    // `manager` are disjoint fields.
                    let mgr = self.manager.clone();
                    mgr.borrow_mut()
                        .handle_input(ManagerInput::ClientDatagram { src, payload }, now);
                    // Drain outputs after each datagram to bound queue growth.
                    self.drain_outputs(now);
                    self.in_flight_client = None;
                }
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => {
                    debug!(
                        "{} recv_from error on UDP listener: {}",
                        log_context!(self),
                        e
                    );
                    break;
                }
            }
        }
    }

    /// Drain datagrams waiting on one flow's connected upstream socket into the
    /// manager as `BackendDatagram`s.
    fn ingest_upstream(&mut self, upstream_token: Token, now: Instant) {
        let Some(&flow) = self.upstream_to_flow.get(&upstream_token) else {
            return;
        };
        loop {
            let result = {
                let Some(socket) = self.upstream_sockets.get(&upstream_token) else {
                    return;
                };
                socket.recv(&mut self.recv_buf)
            };
            match result {
                Ok(len) => {
                    let len = len.min(self.recv_buf.len());
                    let payload: &[u8] = &self.recv_buf[..len];
                    let mgr = self.manager.clone();
                    mgr.borrow_mut()
                        .handle_input(ManagerInput::BackendDatagram { flow, payload }, now);
                    self.drain_outputs(now);
                }
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => {
                    debug!(
                        "{} recv error on upstream socket: {}",
                        log_context!(self),
                        e
                    );
                    break;
                }
            }
        }
    }

    /// Drain the manager's output queue, acting on each `Output`.
    fn drain_outputs(&mut self, now: Instant) {
        let mgr = self.manager.clone();
        loop {
            let out = mgr.borrow_mut().poll_output();
            let Some(out) = out else { break };
            match out {
                Output::SelectBackend { flow, cluster, key } => {
                    self.on_select_backend(flow, &cluster, key, now)
                }
                Output::OpenUpstream { flow, backend } => self.on_open_upstream(flow, backend, now),
                Output::SendToBackend(transmit) => self.on_send_to_backend(transmit),
                Output::SendToClient(transmit) => self.on_send_to_client(transmit),
                Output::ArmTimer(deadline) => self.arm_timer(deadline, now),
                Output::Metric(ev) => Self::record_metric(ev),
                Output::CloseFlow(flow) => self.on_close_flow(flow),
                Output::Drop(reason) => Self::record_drop(reason),
            }
        }
    }

    fn on_select_backend(&mut self, flow: FlowId, cluster: &str, key: u64, now: Instant) {
        let resolved = self
            .backends
            .borrow_mut()
            .backend_from_cluster_id_with_key(cluster, Some(key));
        match resolved {
            Ok((backend, addr)) => {
                self.manager.borrow_mut().handle_input(
                    ManagerInput::BackendResolved {
                        flow,
                        backend,
                        addr,
                    },
                    now,
                );
            }
            Err(e) => {
                debug!(
                    "{} no backend for cluster {}: {}; aborting flow {}",
                    log_context!(self),
                    cluster,
                    e,
                    flow
                );
                incr!(names::udp::DROPPED_NO_BACKEND);
                // Abort the flow instead of leaving it parked AwaitingBackend:
                // the manager already counted it (FlowCreated, +1 gauge, slab +
                // admission slot), so without this it would squat a `max_flows`
                // slot for the full idle timeout while every later datagram is
                // dropped. `abort_flow` enqueues FlowEvicted + CloseFlow, which
                // the surrounding `drain_outputs` loop processes — freeing the
                // slot immediately and balancing the gauge via FlowEvicted.
                self.manager
                    .borrow_mut()
                    .abort_flow(flow, now, CloseReason::Aborted);
            }
        }
    }

    fn on_open_upstream(&mut self, flow: FlowId, backend: SocketAddr, now: Instant) {
        let mut socket = match udp_connect(backend) {
            Ok(socket) => socket,
            Err(e) => {
                // EMFILE/ENFILE/connect refusal → shed this flow, never panic.
                warn!(
                    "{} could not open upstream socket to {}: {}; shedding flow {}",
                    log_context!(self),
                    backend,
                    e,
                    flow
                );
                incr!(names::udp::FLOWS_SHED);
                // The manager already moved this flow toward Established and
                // counted it; abort it so the `max_flows` slot frees immediately
                // (FlowEvicted balances the gauge) instead of squatting until the
                // idle timeout. Keep the FLOWS_SHED metric above for the
                // EMFILE/refused case. `abort_flow` enqueues FlowEvicted +
                // CloseFlow for the surrounding `drain_outputs` loop.
                self.manager
                    .borrow_mut()
                    .abort_flow(flow, now, CloseReason::Aborted);
                return;
            }
        };
        // Multi-token pattern (template tcp.rs:1029-1053): a fresh slab slot
        // under the SAME listener-session Rc, registered READABLE so its
        // readiness reaches `Server::ready` → demuxed back to this session by
        // `update_readiness`. The flow-table cap (`max_flows`) already bounds
        // how many upstream sockets/slots can exist, so the slab cannot grow
        // unbounded here.
        let upstream_token = {
            let mut s = self.sessions.borrow_mut();
            let listener_session = s.slab[self.listener_token.0].clone();
            let entry = s.slab.vacant_entry();
            let token = Token(entry.key());
            entry.insert(listener_session);
            token
        };
        if let Err(e) = self
            .registry
            .register(&mut socket, upstream_token, Interest::READABLE)
        {
            error!(
                "{} could not register upstream socket: {}",
                log_context!(self),
                e
            );
            self.sessions.borrow_mut().slab.try_remove(upstream_token.0);
            // The flow is Established in the manager but has no usable upstream
            // socket: abort it so its `max_flows` slot frees now (FlowEvicted
            // balances the gauge) rather than squatting until idle timeout.
            self.manager
                .borrow_mut()
                .abort_flow(flow, now, CloseReason::Aborted);
            return;
        }
        self.upstream_sockets.insert(upstream_token, socket);
        self.upstream_to_flow.insert(upstream_token, flow);
        self.flow_to_upstream.insert(flow, upstream_token);
        self.flow_started.insert(flow, Instant::now());
        // The client source for this flow is the one currently in flight.
        let client = self.in_flight_client.unwrap_or(self.address);
        self.flow_endpoints.insert(flow, (client, Some(backend)));
        if let Some(src) = self.in_flight_client {
            let key = self.client_key(src);
            self.client_key_to_flow.insert(key, flow);
        }
        // This flow's first SendToBackend (if any) follows immediately.
        self.in_flight_flow = Some(flow);
    }

    fn on_send_to_backend(&mut self, transmit: crate::protocol::udp::Transmit) {
        // Resolve the owning flow precisely (NOT by `transmit.dst`, which two
        // flows to the same backend would alias — sending on the wrong
        // connected socket misroutes that backend's reply to the wrong client).
        //   * new flow:  `OpenUpstream{flow}` set `in_flight_flow` just before.
        //   * established flow: resolve via the in-flight client source through
        //     the shell-side `client_key -> flow` shadow of the flow table.
        let flow = self.in_flight_flow.or_else(|| {
            self.in_flight_client
                .map(|src| self.client_key(src))
                .and_then(|key| self.client_key_to_flow.get(&key).copied())
        });
        let token = flow.and_then(|f| self.flow_to_upstream.get(&f).copied());
        let Some(token) = token else {
            // No resolved flow / upstream socket: this is not a queue-full drop.
            incr!(names::udp::DROPPED_UNKNOWN_FLOW);
            return;
        };
        let Some(socket) = self.upstream_sockets.get(&token) else {
            // Socket already gone (flow closed mid-drain): unknown-flow, not
            // queue-full.
            incr!(names::udp::DROPPED_UNKNOWN_FLOW);
            return;
        };
        // If a queue is already backed up for this flow, preserve FIFO order —
        // do NOT jump the line with the fast-path `send`. Append (or drop on
        // overflow) and let the WRITABLE drain catch up.
        if let Some(q) = self.upstream_write_queues.get_mut(&token)
            && !q.is_empty()
        {
            if !q.push(transmit.dst, transmit.payload) {
                debug!("{} upstream write queue full, dropping", log_context!(self));
                incr!(names::udp::DROPPED_WQ_FULL);
            }
            return;
        }
        match socket.send(&transmit.payload) {
            Ok(_) => {}
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                // Kernel send buffer full: enqueue (bounded) and arm WRITABLE so
                // the next writable event drains it. Drop + metric only at cap.
                let q = self
                    .upstream_write_queues
                    .entry(token)
                    .or_insert_with(|| WriteQueue::new(UPSTREAM_WRITE_QUEUE_CAP));
                if q.push(transmit.dst, transmit.payload) {
                    self.arm_upstream_writable(token);
                } else {
                    debug!("{} upstream write queue full, dropping", log_context!(self));
                    incr!(names::udp::DROPPED_WQ_FULL);
                }
            }
            Err(e) => {
                debug!("{} upstream send error: {}", log_context!(self), e);
                // Hard send error (e.g. ECONNREFUSED), not a queue-full drop.
                incr!(names::udp::DROPPED_SEND_ERROR);
            }
        }
    }

    /// Reregister a flow's connected upstream socket for `READABLE | WRITABLE`
    /// so a queued forward datagram gets a writable wake (the edge-triggered
    /// analog of `signal_pending_write`). Idempotent enough — mio coalesces a
    /// repeated interest set.
    fn arm_upstream_writable(&mut self, token: Token) {
        if let Some(socket) = self.upstream_sockets.get_mut(&token)
            && let Err(e) =
                self.registry
                    .reregister(socket, token, Interest::READABLE | Interest::WRITABLE)
        {
            debug!(
                "{} could not arm WRITABLE on upstream socket: {}",
                log_context!(self),
                e
            );
        }
    }

    /// Drop a flow's upstream socket back to `READABLE`-only once its write queue
    /// has fully drained, so an empty socket no longer wakes the loop on every
    /// writable edge (a permanently-WRITABLE UDP socket would busy-loop).
    fn disarm_upstream_writable(&mut self, token: Token) {
        if let Some(socket) = self.upstream_sockets.get_mut(&token)
            && let Err(e) = self.registry.reregister(socket, token, Interest::READABLE)
        {
            debug!(
                "{} could not disarm WRITABLE on upstream socket: {}",
                log_context!(self),
                e
            );
        }
    }

    /// Drain a flow's upstream write queue on a writable event. Re-sends in FIFO
    /// order until `WouldBlock` or empty; on empty, drops WRITABLE interest.
    fn drain_upstream_queue(&mut self, token: Token) {
        let Some(mut queue) = self.upstream_write_queues.remove(&token) else {
            return;
        };
        let socket = self.upstream_sockets.get(&token);
        let Some(socket) = socket else {
            // Socket gone (flow closed mid-drain): discard the queue.
            return;
        };
        let emptied = queue.drain(|_dst, payload| match socket.send(payload) {
            Ok(_) => SendOutcome::Sent,
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => SendOutcome::WouldBlock,
            Err(_) => SendOutcome::Dropped,
        });
        if emptied {
            self.disarm_upstream_writable(token);
        } else {
            // Still backed up: put the queue back and keep WRITABLE armed.
            self.upstream_write_queues.insert(token, queue);
        }
    }

    fn on_send_to_client(&mut self, transmit: crate::protocol::udp::Transmit) {
        // Preserve FIFO: if the client-return queue is already backed up, append
        // (or drop on overflow) rather than jumping the line via the fast path.
        if !self.client_write_queue.is_empty() {
            if !self.client_write_queue.push(transmit.dst, transmit.payload) {
                debug!("{} client write queue full, dropping", log_context!(self));
                incr!(names::udp::DROPPED_WQ_FULL);
            }
            return;
        }
        let send_result = {
            let listener = self.listener.borrow();
            let Some(socket) = listener.socket.as_ref() else {
                return;
            };
            socket.send_to(&transmit.payload, transmit.dst)
        };
        match send_result {
            Ok(_) => {}
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                // Listener send buffer full: enqueue (bounded) + arm WRITABLE on
                // the listener so the next writable event drains it.
                if self.client_write_queue.push(transmit.dst, transmit.payload) {
                    self.arm_client_writable();
                } else {
                    debug!("{} client write queue full, dropping", log_context!(self));
                    incr!(names::udp::DROPPED_WQ_FULL);
                }
            }
            Err(e) => {
                debug!("{} client send_to error: {}", log_context!(self), e);
                // Hard send_to error, not a queue-full drop.
                incr!(names::udp::DROPPED_SEND_ERROR);
            }
        }
    }

    /// Reregister the listener socket for `READABLE | WRITABLE` so a queued
    /// client-return datagram gets a writable wake. The listener token routes
    /// the writable event back into `update_readiness`.
    fn arm_client_writable(&mut self) {
        let listener = self.listener.borrow();
        let fd = match listener.socket.as_ref() {
            Some(socket) => socket.as_raw_fd(),
            None => return,
        };
        if let Err(e) = self.registry.reregister(
            &mut SourceFd(&fd),
            self.listener_token,
            Interest::READABLE | Interest::WRITABLE,
        ) {
            debug!(
                "{} could not arm WRITABLE on listener socket: {}",
                log_context!(self),
                e
            );
        }
    }

    /// Drop the listener socket back to `READABLE`-only once the client-return
    /// queue is empty (a permanently-WRITABLE listener would busy-loop).
    fn disarm_client_writable(&mut self) {
        let listener = self.listener.borrow();
        let fd = match listener.socket.as_ref() {
            Some(socket) => socket.as_raw_fd(),
            None => return,
        };
        if let Err(e) =
            self.registry
                .reregister(&mut SourceFd(&fd), self.listener_token, Interest::READABLE)
        {
            debug!(
                "{} could not disarm WRITABLE on listener socket: {}",
                log_context!(self),
                e
            );
        }
    }

    /// Drain the client-return write queue on a listener writable event. Re-sends
    /// in FIFO order until `WouldBlock` or empty; on empty, drops WRITABLE.
    fn drain_client_queue(&mut self) {
        let mut queue = std::mem::replace(&mut self.client_write_queue, WriteQueue::new(0));
        let emptied = {
            let listener = self.listener.borrow();
            let Some(socket) = listener.socket.as_ref() else {
                // Listener socket gone: discard the queue and restore an empty one.
                self.client_write_queue = WriteQueue::new(CLIENT_WRITE_QUEUE_CAP);
                return;
            };
            queue.drain(|dst, payload| match socket.send_to(payload, *dst) {
                Ok(_) => SendOutcome::Sent,
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => SendOutcome::WouldBlock,
                Err(_) => SendOutcome::Dropped,
            })
        };
        // Restore the (possibly still-backed-up) queue; preserve its capacity.
        queue.cap = CLIENT_WRITE_QUEUE_CAP;
        self.client_write_queue = queue;
        if emptied {
            self.disarm_client_writable();
        }
    }

    /// Map the single manager-wide `ArmTimer(deadline)` onto the thread-local
    /// `TIMER`, keyed by the **listener token**: when it fires, the run loop
    /// calls `Server::timeout(listener_token)` → `session.timeout(token)` →
    /// `manager.handle_timeout(now)`.
    fn arm_timer(&mut self, deadline: Instant, now: Instant) {
        let delay = deadline.saturating_duration_since(now);
        TIMER.with(|timer| {
            let mut timer = timer.borrow_mut();
            // Cancel the previous deadline so re-arming does not leak timer-slab
            // entries. The manager only emits a fresh `ArmTimer` on a real
            // deadline change, so cancellations are rare.
            if let Some(old) = self.timer_handle.take() {
                let _ = timer.cancel_timeout(&old);
            }
            self.timer_handle = Some(timer.set_timeout(delay, self.listener_token));
        });
    }

    fn on_close_flow(&mut self, flow: FlowId) {
        if let Some(token) = self.flow_to_upstream.remove(&flow) {
            if let Some(mut socket) = self.upstream_sockets.remove(&token) {
                if let Err(e) = self.registry.deregister(&mut socket) {
                    debug!("{} deregister upstream on close: {}", log_context!(self), e);
                }
            }
            // Drop any queued (un-drained) forward datagrams with the socket so
            // the per-flow queue cannot leak; gauge correctness is preserved
            // because the queue holds bytes, not flow-count state.
            self.upstream_write_queues.remove(&token);
            self.upstream_to_flow.remove(&token);
            self.sessions.borrow_mut().slab.try_remove(token.0);
        }
        if let Some(started) = self.flow_started.remove(&flow) {
            let duration = started.elapsed();
            time!(names::udp::FLOW_DURATION, duration.as_millis());
        }
        let (client, backend) = self
            .flow_endpoints
            .remove(&flow)
            .unwrap_or((self.address, None));
        // Drop the shadow flow-table entry if it still points at this flow.
        let key = self.client_key(client);
        if self.client_key_to_flow.get(&key) == Some(&flow) {
            self.client_key_to_flow.remove(&key);
        }
        info!("{} flow closed", log_flow_context!(flow, client, backend));
    }

    fn record_metric(ev: MetricEvent) {
        match ev {
            MetricEvent::FlowCreated => {
                incr!(names::udp::FLOWS_CREATED);
                gauge_add!(names::udp::ACTIVE_FLOWS, 1);
            }
            MetricEvent::FlowEvicted => {
                incr!(names::udp::FLOWS_EVICTED);
                gauge_add!(names::udp::ACTIVE_FLOWS, -1);
            }
            MetricEvent::FlowShed => {
                incr!(names::udp::FLOWS_SHED);
            }
            MetricEvent::DatagramIn(bytes) => {
                incr!(names::udp::DATAGRAMS_IN);
                count!(names::udp::BYTES_IN, bytes as i64);
            }
            MetricEvent::DatagramOut(bytes) => {
                incr!(names::udp::DATAGRAMS_OUT);
                count!(names::udp::BYTES_OUT, bytes as i64);
            }
            MetricEvent::DatagramDropped(reason) => Self::record_drop(reason),
        }
    }

    fn record_drop(reason: DropReason) {
        incr!(names::udp::DATAGRAMS_DROPPED);
        match reason {
            DropReason::Invalid => incr!(names::udp::DROPPED_INVALID),
            DropReason::Truncated => incr!(names::udp::DROPPED_TRUNCATED),
            DropReason::NoBackend => incr!(names::udp::DROPPED_NO_BACKEND),
            DropReason::Shed => incr!(names::udp::DROPPED_SHED),
            DropReason::UnknownFlow => incr!(names::udp::DROPPED_UNKNOWN_FLOW),
        }
    }

    /// Tear down every active flow on this listener **through the manager**, so
    /// each close emits `FlowEvicted` + `CloseFlow` exactly once and the shell's
    /// normal [`on_close_flow`](Self::on_close_flow) handler frees the upstream
    /// socket + slab slot and decrements `udp.active_flows`. Used on soft/hard
    /// stop, listener remove, and listener deactivate so the worker reaches its
    /// `base_sessions_count` and exits promptly instead of waiting out every
    /// flow's idle timeout — and so the active-flows gauge does not leak by N
    /// (the bug the old direct-teardown path had: it cleared the shell maps
    /// without telling the manager, so `FlowEvicted` never fired).
    ///
    /// On a deactivate where the manager is *retained* (not dropped), this also
    /// resets the manager's flow table to empty, keeping manager and shell
    /// consistent. The listener socket and the listener-session slab slot are
    /// left intact (the listener slot is part of `base_sessions_count` and
    /// reclaimed when the worker exits); only the connectionless per-flow slots
    /// — which are NOT counted in `nb_connections` and so removed without
    /// `decr` — are freed.
    pub fn close_all_flows(&mut self, now: Instant) {
        // Drive teardown through the manager: it emits one `FlowEvicted` +
        // `CloseFlow` per live flow into its output queue. Draining those runs
        // `record_metric(FlowEvicted)` (the single gauge decrement) and
        // `on_close_flow` (frees socket + slab slot + shell maps) per flow.
        self.manager.borrow_mut().close_all(now);
        self.drain_outputs(now);
        // After `close_all`, the manager's flow table is empty and it armed no
        // new timer (`reschedule` emits `ArmTimer` only on a real deadline
        // change, and an empty table has no deadline). Cancel any residual shell
        // timer so it can't fire against a now-flowless listener.
        if let Some(handle) = self.timer_handle.take() {
            TIMER.with(|timer| {
                let _ = timer.borrow_mut().cancel_timeout(&handle);
            });
        }
    }
}

impl ProxySession for UdpListenerSession {
    fn protocol(&self) -> Protocol {
        Protocol::UDPListen
    }

    fn update_readiness(&mut self, token: Token, events: Ready) {
        // WRITABLE first: a previously-`WouldBlock` socket can now accept queued
        // egress. Drain before reading so the kernel send buffer has room before
        // this pass enqueues more (and so a writable-only event still drains).
        if events.is_writable() {
            if token == self.listener_token {
                self.drain_client_queue();
            } else if self.upstream_to_flow.contains_key(&token) {
                self.drain_upstream_queue(token);
            }
        }
        if !events.is_readable() {
            return;
        }
        let now = Instant::now();
        if token == self.listener_token {
            self.ingest_client(now);
        } else if self.upstream_to_flow.contains_key(&token) {
            self.ingest_upstream(token, now);
        }
    }

    fn ready(&mut self, _session: Rc<RefCell<dyn ProxySession>>) -> SessionIsToBeClosed {
        // All work happens in `update_readiness` (it has the firing token; the
        // generic `ready()` does not). Never close the listener session here —
        // it lives for the listener's lifetime.
        false
    }

    fn timeout(&mut self, token: Token) -> SessionIsToBeClosed {
        if token == self.listener_token {
            let now = Instant::now();
            self.manager.borrow_mut().handle_timeout(now);
            self.drain_outputs(now);
            // Re-arm: the manager emits a fresh ArmTimer via poll_output if a
            // flow is still scheduled (handled inside drain_outputs). Nothing
            // to do here. Never close the listener on a flow timeout.
        }
        false
    }

    fn close(&mut self) {
        // Tear down any still-live flows THROUGH the manager (FlowEvicted +
        // CloseFlow per flow) so `udp.active_flows` balances to zero even if the
        // server reaps this listener session directly without a prior
        // proxy-driven `close_all_flows`. In the common path
        // (`close_all_flows` already ran) the manager has no live flows, so this
        // is a cheap no-op. Also cancels the residual idle timer.
        self.close_all_flows(Instant::now());
        // Any leftover egress queues / shell maps are dropped here (the per-flow
        // ones were already freed by the manager-driven close above; this only
        // resets the client-return queue, which is not flow-state).
        self.upstream_write_queues.clear();
        self.client_write_queue = WriteQueue::new(CLIENT_WRITE_QUEUE_CAP);
        self.upstream_to_flow.clear();
        self.flow_to_upstream.clear();
        // Deregister + drop the listener socket. Never shutdown(Both) — UDP has
        // no connection to shut down; just deregister + drop the fds.
        let mut listener = self.listener.borrow_mut();
        if let Some(socket) = listener.socket.as_ref() {
            let fd = socket.as_raw_fd();
            let _ = self.registry.deregister(&mut SourceFd(&fd));
        }
        listener.active = false;
    }

    fn last_event(&self) -> Instant {
        // The listener session lives for the listener's lifetime — it is never a
        // zombie even when idle. Mirror `ListenSession::last_event` (which
        // returns `now`) so the `zombie_check` (which has no listener-protocol
        // exclusion) never reaps a quiet UDP listener. Per-flow idle reaping is
        // handled by the manager's timer wheel, not the zombie sweep.
        Instant::now()
    }

    fn print_session(&self) {
        error!(
            "{} UDP listener session: {} active flows, {} upstream sockets",
            log_context!(self),
            self.manager.borrow().flow_count(),
            self.upstream_sockets.len(),
        );
    }

    fn frontend_token(&self) -> Token {
        self.listener_token
    }

    fn shutting_down(&mut self) -> SessionIsToBeClosed {
        // The listener session is a *listener*, not a connection: it was never
        // counted by `SessionManager::incr` (only accepted sessions are), so it
        // MUST NOT be routed through the connection-counted
        // `shut_down_sessions_by_frontend_tokens` path — that calls `decr`,
        // which underflows `nb_connections` and panics (`assert!(nb != 0)` at
        // `server.rs`). This mirrors `ListenSession::shutting_down` (which also
        // returns `false`). Soft-stop draining is owned by
        // `UdpProxy::notify(SoftStop)`: it flips every manager to `Drain` and
        // deregisters the listener sockets, so existing flows reach teardown and
        // no new flow is admitted. The slot is reclaimed when the worker exits.
        false
    }

    fn cluster_id(&self) -> Option<String> {
        self.listener.borrow().cluster_id.clone()
    }
}

#[allow(unused_imports)]
pub(crate) use {log_context, log_flow_context, log_module_context};

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::Cell;

    #[test]
    fn effective_max_flows_explicit_value_is_used() {
        // An explicit value is honoured verbatim, ignoring the slab headroom
        // clamp (the operator opted in).
        assert_eq!(effective_max_flows(42, 0), 42);
        assert_eq!(effective_max_flows(42, 10), 42);
    }

    #[test]
    fn effective_max_flows_auto_is_positive() {
        // 0 = auto: derives ~70% of RLIMIT_NOFILE, always >= 1.
        assert!(effective_max_flows(0, 0) >= 1);
    }

    #[test]
    fn effective_max_flows_auto_is_clamped_to_slab_headroom() {
        // Auto derivation is capped at the slab headroom (max_connections) so a
        // UDP listener can't inflate the shared slab. A headroom of 0 disables
        // the clamp.
        assert!(effective_max_flows(0, 4) <= 4);
        assert!(effective_max_flows(0, 4) >= 1);
    }

    #[test]
    fn clamp_max_rx_respects_buffer_size() {
        // The configured rx size is clamped to buffer_size; an unset (0)
        // buffer_size leaves it untouched.
        assert_eq!(clamp_max_rx(u32::MAX as usize, 16_384), 16_384);
        assert_eq!(clamp_max_rx(1_024, 16_384), 1_024);
        assert_eq!(clamp_max_rx(u32::MAX as usize, 0), u32::MAX as usize);
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    /// A fake socket whose `send` outcome is scripted per call. Lets the
    /// `WriteQueue` state machine be exercised with zero real I/O: we feed a
    /// sequence of `SendOutcome`s and record what was sent in order.
    struct FakeSocket {
        /// Outcomes returned by successive `send` calls (front = next).
        script: RefCell<VecDeque<SendOutcome>>,
        /// Payloads that `Sent`/`Dropped` actually consumed, in order.
        consumed: RefCell<Vec<Vec<u8>>>,
        /// Default outcome once the script is exhausted.
        default: Cell<bool>, // true = Sent, false = WouldBlock
    }

    impl FakeSocket {
        fn new(script: Vec<SendOutcome>, default_sent: bool) -> Self {
            FakeSocket {
                script: RefCell::new(script.into()),
                consumed: RefCell::new(Vec::new()),
                default: Cell::new(default_sent),
            }
        }

        fn send(&self, payload: &[u8]) -> SendOutcome {
            let outcome = self.script.borrow_mut().pop_front().unwrap_or({
                if self.default.get() {
                    SendOutcome::Sent
                } else {
                    SendOutcome::WouldBlock
                }
            });
            if matches!(outcome, SendOutcome::Sent | SendOutcome::Dropped) {
                self.consumed.borrow_mut().push(payload.to_vec());
            }
            outcome
        }
    }

    #[test]
    fn write_queue_push_until_full_then_drops() {
        let mut q = WriteQueue::new(2);
        assert!(q.is_empty());
        assert!(q.push(addr(1), vec![1]));
        assert!(q.push(addr(2), vec![2]));
        assert_eq!(q.len(), 2);
        // At capacity: the third push is rejected (caller drops + counts).
        assert!(!q.push(addr(3), vec![3]));
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn write_queue_drains_in_fifo_order_on_writable() {
        let mut q = WriteQueue::new(8);
        for i in 0..4u8 {
            assert!(q.push(addr(i as u16), vec![i]));
        }
        // All sends succeed: drain empties the queue, FIFO preserved.
        let sock = FakeSocket::new(vec![], true);
        let emptied = q.drain(|_dst, payload| sock.send(payload));
        assert!(emptied);
        assert!(q.is_empty());
        assert_eq!(
            *sock.consumed.borrow(),
            vec![vec![0u8], vec![1u8], vec![2u8], vec![3u8]]
        );
    }

    #[test]
    fn write_queue_stops_on_wouldblock_and_resumes() {
        let mut q = WriteQueue::new(8);
        for i in 0..3u8 {
            assert!(q.push(addr(i as u16), vec![i]));
        }
        // First send ok, then WouldBlock: drain sends one and stops, leaving two.
        let sock = FakeSocket::new(vec![SendOutcome::Sent, SendOutcome::WouldBlock], false);
        let emptied = q.drain(|_dst, payload| sock.send(payload));
        assert!(!emptied);
        assert_eq!(q.len(), 2);
        assert_eq!(*sock.consumed.borrow(), vec![vec![0u8]]);
        // Front is still the second datagram (FIFO preserved across the stall).
        assert_eq!(q.queue.front().unwrap().1, vec![1u8]);
        // Second writable event: now everything goes through.
        let sock2 = FakeSocket::new(vec![], true);
        let emptied2 = q.drain(|_dst, payload| sock2.send(payload));
        assert!(emptied2);
        assert!(q.is_empty());
        assert_eq!(*sock2.consumed.borrow(), vec![vec![1u8], vec![2u8]]);
    }

    #[test]
    fn write_queue_hard_error_drops_one_and_continues() {
        let mut q = WriteQueue::new(8);
        for i in 0..3u8 {
            assert!(q.push(addr(i as u16), vec![i]));
        }
        // Middle datagram hard-errors: it is popped + skipped, the rest proceed.
        let sock = FakeSocket::new(
            vec![SendOutcome::Sent, SendOutcome::Dropped, SendOutcome::Sent],
            true,
        );
        let emptied = q.drain(|_dst, payload| sock.send(payload));
        assert!(emptied);
        assert!(q.is_empty());
        // The dropped datagram (1) was consumed-for-accounting but the queue is
        // empty and the surviving datagrams (0, 2) went out in order.
        assert_eq!(
            *sock.consumed.borrow(),
            vec![vec![0u8], vec![1u8], vec![2u8]]
        );
    }

    #[test]
    fn write_queue_empties_cleanly_when_already_empty() {
        let mut q = WriteQueue::new(4);
        let sock = FakeSocket::new(vec![], true);
        // Draining an empty queue is a no-op that reports emptied = true.
        let emptied = q.drain(|_dst, payload| sock.send(payload));
        assert!(emptied);
        assert!(q.is_empty());
        assert!(sock.consumed.borrow().is_empty());
    }
}
