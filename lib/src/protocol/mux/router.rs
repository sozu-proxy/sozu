//! Backend routing and connection reuse for the mux layer.
//!
//! [`Router`] owns the map of token -> backend [`Connection`] and centralises
//! the logic for picking (or opening) the right backend for an incoming
//! request. The H2 reuse strategy prefers the least-loaded non-draining
//! connection of the target cluster; H1 falls back to keep-alive reuse.
//!
//! ## Determinism (nothing mandates a tie-break order; issue #1338 does)
//!
//! [`Router::backends`] used to be a `HashMap<Token, _>`, and
//! `Router::plan_connect` scans it with a plain `for (token, backend) in
//! &self.backends` loop, in the map's iteration order — seeded per-`HashMap`
//! (`RandomState`), so **which backend served a request was non-deterministic
//! across process restarts**, for the identical set of backend connections and
//! the identical request. That is a strictly wider leak than the wire-order
//! ones `h2_flow_control` and `h2_stream_table` closed: it
//! does not reorder bytes on one connection, it sends them to a different
//! machine.
//!
//! Three separate decisions inside that one loop read the order, and they do
//! not read it the same way:
//!
//! - the H2 least-loaded arm compares with a strict `<`, so the FIRST
//!   connection at the minimum stream count wins — on a tie, map order alone
//!   picks the backend;
//! - the H2 `BackendStatus::Connecting` fallback assigns with neither a
//!   `break` nor an "already chosen" guard, so the LAST matching connecting
//!   connection wins;
//! - the H1 `BackendStatus::KeepAlive` arm assigns and breaks, so the FIRST
//!   matching keep-alive socket wins.
//!
//! The map is now a [`BackendConnections`] (a `BTreeMap` behind one inline
//! slot for the lowest token, #1610), so the scan is the total order on
//! `Token` — fixed, reproducible, and free (its iteration is already sorted;
//! no extra sort step, no injected seed to thread through construction and
//! tests). The three decisions above resolve to the lowest `Token`, the
//! highest `Token` and the lowest `Token` respectively. Pinning is all this
//! does: the last-wins shape of the connecting fallback is preserved, not
//! corrected, because changing which backend is preferred is a routing change
//! and not a determinism one.
//!
//! A `Token` is the slab index the proxy's session manager handed the backend
//! socket at dial time, and it carries no fairness or recency meaning of its
//! own: `slab::Slab::try_remove` pushes the freed index onto the head of its
//! vacant list (`self.next = key`) and `insert` pops that head, so a dial
//! reuses the most recently RELEASED index, not the lowest and not the
//! oldest. The total order is a stable arbitrary label. It does not need to
//! be more than that for the
//! least-loaded arm, and for the reason `h2_stream_table`'s doc had
//! to spell out and `h2_flow_control`'s could not: a tie there does
//! not persist across requests. Attaching the stream increments the winner's
//! `ConnectionH2::stream_count`, so the next request sees it above the minimum
//! and takes the next token — the load counter is itself the fairness cursor
//! `pending_window_updates` had to note the absence of. The connecting
//! fallback has no such counter, but every candidate it ranks is a connection
//! that cannot serve anything yet, and it concentrated every waiting stream on
//! one of them before this change too; the total order fixes WHICH one, it
//! does not change how many.
//!
//! ## Complexity — UNMEASURED beyond the default configuration
//!
//! `n` here is the number of live backend connections on ONE frontend session,
//! not a worker-wide or cluster-wide count. An H2 frontend reaching an H2
//! cluster holds roughly one multiplex slot per cluster it touches. An H2
//! frontend reaching an H1 cluster cannot bundle streams, so it holds up to
//! one socket per concurrent stream — bounded by the `max_concurrent_streams`
//! this proxy advertises, `DEFAULT_MAX_CONCURRENT_STREAMS` (100) by default
//! and clamped by `H2ConnectionConfig::new` to `MAX_SAFE_CONCURRENT_STREAMS`
//! (10_000).
//!
//! What changes complexity class is the point lookup: `get` / `get_mut` /
//! `insert` / `remove` / `contains_key` go from `HashMap`'s amortized `O(1)`
//! to `BTreeMap`'s `O(log n)` (the inline slot adds one comparison), and
//! `Mux::ready` does one `get_mut`
//! per backend event through `EndpointClient`. Every iteration site —
//! `Router::plan_connect`'s scan, `Mux::reschedule`, the two
//! `Mux::ready` sweeps — already walks all `n` and keeps its `O(n)`. At the
//! default ceiling (`n <= 100`) a lookup is ~7 `usize` comparisons against one
//! SipHash-1-3 of a `usize` plus a bucket probe, which is not expected to be
//! measurable; at a listener configured near `MAX_SAFE_CONCURRENT_STREAMS`
//! (`n` up to 10_000, ~14 comparisons) it stops being self-evidently
//! negligible. That expectation has **not been benchmarked**, neither before
//! nor after this change. Closing it requires a before/after run of this
//! repository's `Bombardier bench` CI job (or an equivalent local
//! `bombardier` run) against an H1 cluster behind an H2 frontend at or near
//! that ceiling; no such run has been produced.

use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    net::{IpAddr, SocketAddr},
    rc::Rc,
    time::Duration,
};

use kawa::Block;
use mio::Token;
use sozu_command::{
    logging::ansi_palette,
    proto::command::{
        Cluster, ListenerType, LoadBalancingAlgorithms, RedirectPolicy, RedirectScheme,
    },
    state::ClusterId,
};

#[cfg(debug_assertions)]
use super::DebugEvent;
use super::{
    BackendId, BackendStatus, Connection, Context, GenericHttpStream, GlobalStreamId, Position,
    StreamState,
};
use crate::{
    BackendConnectionError, L7ListenerHandler, ListenerHandler, Readiness, RetrieveClusterError,
    backends::BackendError,
    load_balancing::{affinity_key_from_ip, affinity_key_from_value},
    protocol::http::{
        editor::{HeaderEditMode, HeaderEditSnapshot, HttpContext, emits_x_forwarded},
        parser::compare_no_case,
    },
    router::{HeaderEdit, RouteResult},
    socket::SessionTcpStream,
};

use crate::metrics::names;

/// Module-level prefix used on every log line emitted from the router.
///
/// Two arms:
/// * `log_module_context!()` — zero-arg, legacy `MUX-ROUTER\t >>>` output.
///   Kept for sites without an `HttpContext` in scope. No call site in this
///   module currently uses this arm (every one has an `HttpContext` reachable
///   via [`Context::http_context`] or a direct `&mut HttpContext`
///   parameter), but the arm is retained so the macro name stays stable for
///   future sessionless callers.
/// * `log_module_context!($http_context)` — rich form. `$http_context` must be
///   `&HttpContext` (or coerce to one). Produces the same
///   `[session req cluster backend]` bracket as RUSTLS/PIPE/TCP followed by a
///   `Session(frontend=..., method=..., authority_bytes=...)` block, so router
///   lines are filterable by session ULID or request ULID. `cluster_id` is
///   already carried by the bracket's third slot — not duplicated inside
///   `Session(...)`.
macro_rules! log_module_context {
    () => {{
        let (open, reset, _, _, _) = ansi_palette();
        format!("{open}MUX-ROUTER{reset}\t >>>", open = open, reset = reset)
    }};
    ($http_context:expr) => {{
        let (open, reset, grey, gray, white) = ansi_palette();
        let http_ctx: &HttpContext = &$http_context;
        let ctx = http_ctx.log_context();
        format!(
            "{gray}{ctx}{reset}\t{open}MUX-ROUTER{reset}\t{grey}Session{reset}({gray}frontend{reset}={white}{frontend:?}{reset}, {gray}method{reset}={white}{method:?}{reset}, {gray}authority_bytes{reset}={white}{authority_bytes:?}{reset})\t >>>",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            ctx = ctx,
            frontend = http_ctx.session_address,
            method = http_ctx.method,
            authority_bytes = http_ctx.authority.as_ref().map(String::len),
        )
    }};
}

fn log_coalescing_accepted(context: &HttpContext, authority: &str, sni: &str, matched_name: &str) {
    debug!(
        "{} accepted coalesced authority (authority_bytes={}, sni_bytes={}, matched_san_kind={}, matched_san_bytes={})",
        log_module_context!(context),
        authority.len(),
        sni.len(),
        if matched_name.starts_with("*.") {
            "wildcard"
        } else {
            "exact"
        },
        matched_name.len(),
    );
}

fn log_sni_authority_mismatch(
    context: &HttpContext,
    authority: &str,
    sni: &str,
    certificate_names: Option<&[String]>,
) {
    let certificate_sans_count = certificate_names.map_or(0, <[String]>::len);
    let certificate_sans_bytes = certificate_names
        .into_iter()
        .flatten()
        .map(String::len)
        .fold(0usize, usize::saturating_add);
    warn!(
        "{} rejecting request: TLS cert SANs do not cover authority (authority_bytes={}, sni_bytes={}, certificate_sans_count={}, certificate_sans_bytes={})",
        log_module_context!(context),
        authority.len(),
        sni.len(),
        certificate_sans_count,
        certificate_sans_bytes,
    );
}

/// The cluster-side configuration the routing decision reads, borrowed from
/// the embedder for the length of one decision.
///
/// Question 6 of [#1340](https://github.com/sozu-proxy/sozu/issues/1340) puts
/// routing inside the core; this is how the data that routing needs gets
/// there without the core holding the handle it used to reach through.
/// `L7Proxy::clusters` and `L7Proxy::kind` were two of the three reads
/// `Router::plan_connect` made against `Rc<RefCell<dyn L7Proxy>>`, and both
/// are pure. They arrive as this instead.
///
/// The mechanism is Question 12's, applied a second time rather than
/// reinvented: a view in with a defined staleness, a result out. The
/// staleness here is **one routing decision** — the view is built once per
/// `Router::plan_connect` call and every read inside that call sees the same
/// cluster map.
///
/// That is marginally *stronger* than what it replaces, and deliberately so:
/// the two sites used to take separate `proxy.borrow()`s, one in
/// `Router::plan_connect` and one in `Router::route_from_request`, so a
/// cluster update landing between them would have been half-observed. No
/// such update can interleave — `clusters()` is owned by the worker's
/// command loop and a stream is drained from `pending_links` inside the very
/// `ready()` pass that queued it — so this closes a window that was never
/// open rather than changing a behaviour that was.
///
/// A simulator can build one of these from whatever cluster map it likes,
/// which is what Q6's determinism argument wants and what an
/// `Rc<RefCell<dyn L7Proxy>>` could never give it.
pub(super) struct RoutingView<'a> {
    clusters: &'a HashMap<ClusterId, Cluster>,
    listener_kind: ListenerType,
    /// Whether the backend a pooled connection was dialled to has left the
    /// configuration. `None` treats every backend as current.
    backend_retired: Option<&'a dyn Fn(&BackendId) -> bool>,
    /// The worker's backend connection attempt budget, for a cluster that
    /// sets no `max_connection_attempts` of its own.
    max_connection_attempts: u32,
}

impl<'a> RoutingView<'a> {
    pub(super) fn new(
        clusters: &'a HashMap<ClusterId, Cluster>,
        listener_kind: ListenerType,
    ) -> Self {
        Self {
            clusters,
            listener_kind,
            backend_retired: None,
            max_connection_attempts: sozu_command::config::DEFAULT_MAX_CONNECTION_ATTEMPTS,
        }
    }

    /// Use `attempts` as the backend connection attempt budget of a request
    /// whose cluster sets none: the worker's global value,
    /// `SessionManager::max_connection_attempts` (`lib/src/server.rs`).
    pub(super) fn with_max_connection_attempts(mut self, attempts: u32) -> Self {
        self.max_connection_attempts = attempts;
        self
    }

    /// The backend connection attempt budget of a request routed to
    /// `cluster_id`: the cluster's own `max_connection_attempts` when it sets
    /// one, the worker's otherwise (sozu-proxy/sozu#1800).
    fn max_connection_attempts(&self, cluster_id: Option<&str>) -> u32 {
        cluster_id
            .and_then(|cluster_id| self.cluster(cluster_id))
            .and_then(|cluster| cluster.max_connection_attempts)
            .unwrap_or(self.max_connection_attempts)
    }

    /// Answer the pool-reuse scan's question "is this connection's backend
    /// still configured?" with `retired`.
    ///
    /// A backend connection names its cluster by id, and a cluster id outlives
    /// a `RemoveCluster` followed by an `AddCluster` of the same id, as it
    /// outlives a `RemoveBackend`. Only the embedder can tell whether the
    /// backend behind a [`BackendId`] is still part of the configuration, so
    /// the reuse scan asks it here and never reuses a connection whose backend
    /// is retired: that connection drains the streams it carries and takes no
    /// new one.
    pub(super) fn with_backend_retired(mut self, retired: &'a dyn Fn(&BackendId) -> bool) -> Self {
        self.backend_retired = Some(retired);
        self
    }

    /// Whether `backend` has left the configuration since a connection was
    /// dialled to it.
    fn is_retired(&self, backend: &BackendId) -> bool {
        self.backend_retired.is_some_and(|retired| retired(backend))
    }

    /// The cluster `cluster_id` names, if the worker still holds one.
    fn cluster(&self, cluster_id: &str) -> Option<&Cluster> {
        self.clusters.get(cluster_id)
    }

    /// Whether this listener is plaintext HTTP, which is what both legacy
    /// `https_redirect` sites gate on.
    fn is_http_listener(&self) -> bool {
        matches!(self.listener_kind, ListenerType::Http)
    }
}

/// What [`Router::plan_connect`] answers: either a finished decision, or a
/// step it needs the embedder to perform before it can finish.
///
/// The per-`(cluster, source-IP)` limit gate is the reason this exists. It is
/// not a read — it consults `SessionManager::cluster_ip_at_limit` **and**
/// calls `SessionManager::track_cluster_ip`, which mutates
/// `connections_per_cluster_ip`, worker-global state keyed on
/// `(cluster, ip)` across every session and read again by `lib/src/tcp.rs`'s
/// own gate. A read-only view cannot carry a mutation, so the gate becomes a
/// returned step instead: the core resolves the cluster, hands the embedder
/// what to ask, and the embedder answers.
#[derive(Debug)]
pub(super) enum ConnectStep {
    /// No gate consultation was needed — the stream carries no source IP —
    /// so this is already the decision.
    Decided(ConnectPlan),
    /// The core resolved a cluster and needs the limit consulted, and
    /// updated, before it can go on. Answer with
    /// [`Router::plan_connect_resume`].
    CheckIpLimit(ConnectResume),
}

/// The phase-one results the core needs handed back to finish deciding, plus
/// what the embedder needs to consult the gate.
///
/// Opaque on purpose: the embedder reads the gate inputs through accessors
/// and cannot reach — or forge — the routing results the core parked here.
/// The cluster the gate is keyed on is not among them: `plan_connect` has
/// already stored it in the stream's `HttpContext`, and the embedder borrows
/// it from there rather than the resume carrying a copy of its own (#1583).
/// [`Router::plan_connect_resume`] takes it **by value**, so a resume cannot
/// be replayed; that is `H2Shell::settled` taking its parked target, applied
/// to this seam.
#[derive(Debug)]
pub(super) struct ConnectResume {
    h2: bool,
    frontend_should_stick: bool,
    ip: IpAddr,
    max_connections_per_ip: Option<u64>,
    cluster_retry_after: Option<u32>,
    max_connections_per_subnet: Option<u64>,
}

impl ConnectResume {
    /// The source IP the gate is keyed on — proxy-protocol-aware when
    /// present, falling back to `peer_addr`.
    pub(super) fn ip(&self) -> IpAddr {
        self.ip
    }
    /// The cluster's per-IP connection limit override, if it set one.
    pub(super) fn max_connections_per_ip(&self) -> Option<u64> {
        self.max_connections_per_ip
    }
    /// The cluster's `Retry-After` override, if it set one.
    pub(super) fn cluster_retry_after(&self) -> Option<u32> {
        self.cluster_retry_after
    }
    /// The cluster's per-SUBNET connection limit override, if it set
    /// one. Independent of `max_connections_per_ip`: the embedder
    /// consults both caps and admits only when both allow it.
    pub(super) fn max_connections_per_subnet(&self) -> Option<u64> {
        self.max_connections_per_subnet
    }
}

/// The embedder's answer to [`ConnectStep::CheckIpLimit`].
///
/// `Admitted` carries no payload on purpose: it means the embedder has
/// already *performed* the track, so there is nothing left for the core to
/// decide about it and no binding a later edit could reuse to ask a question
/// this variant has answered — the discipline
/// `H2WritableStateTarget::Flush` set by carrying no `bool`.
#[derive(Debug)]
pub(super) enum IpGateVerdict {
    /// Under the limit, and the embedder has tracked this `(cluster, ip)`.
    Admitted,
    /// At the limit. Carries the resolved `Retry-After` the core stashes on
    /// the stream for the 429 mapping to render or elide.
    AtLimit { retry_after: u32 },
}

/// What [`Router::plan_connect`] decided, for the embedder to fulfil.
///
/// Question 6 of [#1340](https://github.com/sozu-proxy/sozu/issues/1340): the
/// core decides and the embedder performs. This is the "returned request"
/// half of that, in the shape the socket-boundary extraction used four times
/// (`finalize_write` / `H2FinalizeTarget`, `flush_pending_control_frames` /
/// `H2ControlFlushTarget`, `dispatch_writable_state` / `H2WritableStateTarget`,
/// and `force_disconnect`'s inversion): the core answers a step, the caller
/// performs the effect, the caller calls back with the answer —
/// [`Router::commit_dialed`] here.
#[derive(Debug)]
pub(super) enum ConnectPlan {
    /// The stream was attached to a backend connection the router already
    /// held, and nothing is left to perform. Carries no token on purpose: the
    /// attach is complete, so there is no binding a later edit could reuse to
    /// ask a question this variant has already answered — the discipline
    /// `H2WritableStateTarget::Flush` set by carrying no `bool`.
    Attached,
    /// No connection could be reused. The embedder must select a backend,
    /// dial it, build the `Connection`, register it, and hand the result
    /// back to [`Router::commit_dialed`].
    Dial {
        /// The cluster routing resolved for this stream.
        cluster_id: ClusterId,
        /// Whether that cluster's backends speak HTTP/2.
        h2: bool,
        /// Whether the frontend asked for sticky-session affinity.
        frontend_should_stick: bool,
    },
}

/// The backend connections of one session, keyed and iterated by [`Token`].
///
/// A session almost always holds exactly one backend connection, and a
/// `Connection<SessionTcpStream>` is about 1.7 KiB. A `BTreeMap` stores its
/// values inline in 11-slot leaves, so the first insertion into one allocated
/// ~18.8 KiB to hold that single connection (#1610). The lowest-token
/// connection therefore lives inline in `first`, and only the others reach
/// the `BTreeMap`: one backend costs no allocation at all, and every point
/// operation stays `O(log n)` for the multi-backend sessions the module
/// header sizes.
///
/// Invariant: `first` is `None` only when `rest` is empty, and its token is
/// below every token in `rest`. Iteration is `first` then `rest`, which is the
/// total order on `Token` #1338 requires.
///
/// HAProxy needs no container here because a stream has one server
/// connection, reached by pointer (`include/haproxy/stream-t.h`, `scb`); a
/// mux session can hold several, hence the overflow map.
pub struct BackendConnections {
    first: Option<(Token, Connection<SessionTcpStream>)>,
    rest: BTreeMap<Token, Connection<SessionTcpStream>>,
}

impl BackendConnections {
    pub const fn new() -> Self {
        Self {
            first: None,
            rest: BTreeMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        usize::from(self.first.is_some()) + self.rest.len()
    }

    pub fn is_empty(&self) -> bool {
        self.first.is_none()
    }

    pub fn contains_key(&self, token: &Token) -> bool {
        self.get(token).is_some()
    }

    pub fn get(&self, token: &Token) -> Option<&Connection<SessionTcpStream>> {
        match &self.first {
            Some((first, connection)) if first == token => Some(connection),
            _ => self.rest.get(token),
        }
    }

    pub fn get_mut(&mut self, token: &Token) -> Option<&mut Connection<SessionTcpStream>> {
        match &mut self.first {
            Some((first, connection)) if first == token => Some(connection),
            _ => self.rest.get_mut(token),
        }
    }

    /// Insert `connection` under `token`, returning the one it replaces.
    pub fn insert(
        &mut self,
        token: Token,
        connection: Connection<SessionTcpStream>,
    ) -> Option<Connection<SessionTcpStream>> {
        match &mut self.first {
            None => {
                self.first = Some((token, connection));
                None
            }
            Some((first, held)) if *first == token => Some(std::mem::replace(held, connection)),
            Some((first, held)) if token < *first => {
                let displaced_token = std::mem::replace(first, token);
                let displaced = std::mem::replace(held, connection);
                self.rest.insert(displaced_token, displaced)
            }
            Some(_) => self.rest.insert(token, connection),
        }
    }

    /// Remove and return the connection under `token`.
    pub fn remove(&mut self, token: &Token) -> Option<Connection<SessionTcpStream>> {
        match &self.first {
            Some((first, _)) if first == token => {
                let removed = std::mem::replace(&mut self.first, self.rest.pop_first());
                removed.map(|(_, connection)| connection)
            }
            _ => self.rest.remove(token),
        }
    }

    /// The connections in ascending `Token` order.
    pub fn iter(&self) -> BackendConnectionsIter<'_> {
        let split: fn(&BackendEntry) -> (&Token, &Connection<SessionTcpStream>) =
            |(token, connection)| (token, connection);
        self.first.iter().map(split).chain(self.rest.iter())
    }

    /// The connections in ascending `Token` order, mutably.
    pub fn iter_mut(&mut self) -> BackendConnectionsIterMut<'_> {
        let split: fn(&mut BackendEntry) -> (&Token, &mut Connection<SessionTcpStream>) =
            |(token, connection)| (&*token, connection);
        self.first.iter_mut().map(split).chain(self.rest.iter_mut())
    }

    pub fn keys(&self) -> impl Iterator<Item = &Token> {
        self.iter().map(|(token, _)| token)
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut Connection<SessionTcpStream>> {
        self.iter_mut().map(|(_, connection)| connection)
    }
}

type BackendEntry = (Token, Connection<SessionTcpStream>);

/// [`BackendConnections::iter`]: the inline connection, then the overflow map.
pub type BackendConnectionsIter<'a> = std::iter::Chain<
    std::iter::Map<
        std::option::Iter<'a, BackendEntry>,
        fn(&'a BackendEntry) -> (&'a Token, &'a Connection<SessionTcpStream>),
    >,
    std::collections::btree_map::Iter<'a, Token, Connection<SessionTcpStream>>,
>;

/// [`BackendConnections::iter_mut`]: the inline connection, then the overflow map.
pub type BackendConnectionsIterMut<'a> = std::iter::Chain<
    std::iter::Map<
        std::option::IterMut<'a, BackendEntry>,
        fn(&'a mut BackendEntry) -> (&'a Token, &'a mut Connection<SessionTcpStream>),
    >,
    std::collections::btree_map::IterMut<'a, Token, Connection<SessionTcpStream>>,
>;

impl<'a> IntoIterator for &'a BackendConnections {
    type Item = (&'a Token, &'a Connection<SessionTcpStream>);
    type IntoIter = BackendConnectionsIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a> IntoIterator for &'a mut BackendConnections {
    type Item = (&'a Token, &'a mut Connection<SessionTcpStream>);
    type IntoIter = BackendConnectionsIterMut<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

impl Default for BackendConnections {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for BackendConnections {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

#[derive(Debug)]
pub struct Router {
    pub backends: BackendConnections,
    pub configured_backend_timeout: Duration,
    pub configured_connect_timeout: Duration,
    /// Fallback readiness used when a backend token is missing from the map.
    /// This prevents panicking in the Endpoint trait methods that return references.
    pub(super) fallback_readiness: Readiness,
    /// Tokens of the backend connections a `Mux::ready` pass found dead,
    /// awaiting their removal from [`Self::backends`] later in that pass.
    ///
    /// Emptied at the start of every sweep and kept for its capacity, so a
    /// session whose backends close one after another allocates it once
    /// rather than once per pass that closes one (#1610).
    pub(super) dead_backends: Vec<Token>,
}

impl Router {
    pub fn new(configured_backend_timeout: Duration, configured_connect_timeout: Duration) -> Self {
        Self {
            backends: BackendConnections::new(),
            configured_backend_timeout,
            configured_connect_timeout,
            fallback_readiness: Readiness::new(),
            dead_backends: Vec::new(),
        }
    }

    /// Decide what this stream needs, without performing any of it.
    ///
    /// Routing, the cluster gate, the per-(cluster, source-IP) limit and the
    /// pool-reuse scan all happen here. Dialling, the slab session and the
    /// epoll registration do not: those are the embedder's, and this answers
    /// a [`ConnectPlan`] asking for them instead of reaching a
    /// `Rc<RefCell<dyn ProxySession>>` to do them itself.
    ///
    /// `session` is gone from this signature entirely — its single use was
    /// `L7Proxy::add_session`, which now happens in `Mux::ready_inner`.
    /// `proxy` remains for the three reads this still makes (`clusters`,
    /// `kind`, `sessions`) and for backend selection, both of which Question
    /// 6's borrowed-view step takes next.
    pub(super) fn plan_connect<L: ListenerHandler + L7ListenerHandler>(
        &mut self,
        stream_id: GlobalStreamId,
        context: &mut Context<L>,
        // The cluster-side configuration this decision reads, borrowed from
        // the embedder for the length of the call.
        view: &RoutingView<'_>,
    ) -> Result<ConnectStep, BackendConnectionError> {
        let stream = &mut context.streams[stream_id];
        // when reused, a stream should be detached from its old connection, if not we could end
        // with concurrent connections on a single endpoint
        if !matches!(stream.state, StreamState::Link) {
            error!(
                "{} stream {} expected to be in Link state, got {:?}",
                log_module_context!(stream.context),
                stream_id,
                stream.state
            );
            return Err(BackendConnectionError::MaxSessionsMemory);
        }
        #[cfg(debug_assertions)]
        context
            .debug
            .push(DebugEvent::Str(stream.context.get_route()));
        // The budget is resolved from the cluster the previous attempt routed
        // to. The first attempt has none yet, and needs none: every budget
        // allows at least one attempt.
        let max_attempts = view.max_connection_attempts(stream.context.cluster_id.as_deref());
        if u32::from(stream.attempts) >= max_attempts {
            incr!(
                names::backend::CONNECT_RETRIES_EXHAUSTED,
                stream.context.cluster_id.as_deref(),
                stream.context.backend_id.as_deref(),
                stream.context.cluster_metrics_incarnation()
            );
            return Err(BackendConnectionError::MaxConnectionRetries(
                stream.context.cluster_id.clone(),
            ));
        }
        stream.attempts += 1;

        // Borrow front mutably (so route_from_request can rewrite the request
        // line authority/path and inject request-side header edits before we
        // forward to the backend) plus context mutably (so it can stash
        // redirect_location / www_authenticate / original_authority /
        // headers_response). We split-borrow manually to keep the rest of
        // `connect` working with `stream_context` aliasing `stream.context`.
        //
        // A REPLAY skips routing entirely. `front.consumed` is the exact
        // discriminator: it is false on every first connect and on the
        // untouched-request `EndStreamAction::Reconnect`, and true only once
        // request bytes have left the front kawa — which is precisely the
        // `EndStreamAction::ReplayOnFreshBackend` case. Routing already ran
        // on the first attempt and every decision it made (cluster, legacy
        // HTTPS redirect, SNI/authority binding, authentication, request-side
        // header edits) is baked into the captured bytes.
        //
        // Re-running it against a front kawa whose blocks are drained cannot
        // reproduce them: the request line and headers it rewrites are no
        // longer in `front.blocks`, and `end_of_headers_index` has no anchor
        // to place injected headers at. Whatever the next `prepare` did emit
        // would land BEHIND the replayed bytes, because `Kawa::prepare`
        // appends to `out` while `Stream::queue_upstream_replay` prepends —
        // a second, partial copy of the request trailing the first on the
        // same connection.
        //
        // So a replay whose routing decision did not survive is refused
        // outright rather than re-routed against that drained kawa: 502 is
        // what the stream would have received before the replay existed.
        //
        // The routed id, a handle on the route table's `ClusterId` and not a
        // copy, moves into `HttpContext::cluster_id`; everything after this
        // borrows or re-counts it, the embedder's gate included (#1583).
        let replaying = stream.front.consumed;
        if !replaying {
            let (front_ref, stream_context_ref) = {
                let stream_split = &mut *stream;
                (&mut stream_split.front, &mut stream_split.context)
            };
            let routed = self
                .route_from_request(stream_context_ref, front_ref, &context.listener, view)
                .map_err(BackendConnectionError::RetrieveClusterError)?;
            // Derived here, where the routed cluster and the request's header
            // blocks are both in hand, and stored beside the cluster id: a
            // replay finds the blocks drained and reuses this key, as it
            // reuses the cluster.
            let context = &stream.context;
            let key = view.cluster(&routed).and_then(|cluster| {
                affinity_key(
                    cluster,
                    &stream.front,
                    context.session_address,
                    StickyCookie {
                        name: &context.sticky_name,
                        value: context.sticky_session_found.as_deref(),
                    },
                )
            });
            stream.context.affinity_key = key;
            stream.context.set_cluster_metrics_incarnation(
                crate::metrics::capture_cluster_metrics(routed.as_ref()),
            );
            stream.context.cluster_id = Some(routed);
        }
        let stream_context = &stream.context;
        // Only a replay can find it unset: routing has just stored it otherwise.
        let Some(cluster_id) = stream_context.cluster_id.as_deref() else {
            return Err(BackendConnectionError::ReplayRefused(
                "no cluster_id survived the first attempt",
            ));
        };

        let (
            frontend_should_stick,
            frontend_should_redirect_https,
            h2,
            cluster_max_connections_per_ip,
            cluster_retry_after,
            cluster_max_connections_per_subnet,
        ) = view
            .cluster(cluster_id)
            .map(|cluster| {
                (
                    cluster.sticky_session,
                    cluster.https_redirect,
                    cluster.http2.unwrap_or(false),
                    cluster.max_connections_per_ip,
                    cluster.retry_after,
                    cluster.max_connections_per_subnet,
                )
            })
            .unwrap_or((false, false, false, None, None, None));

        // A replay carries H1 wire bytes in `front.out` (the H1 write path is
        // the only one that captures), so its cluster must still resolve to
        // H1 or the H2 converter would frame raw H1 text as a DATA payload.
        // It is expected to: `clusters()` is owned by the worker's command
        // loop, and a replay is drained from `pending_links` inside the very
        // `ready()` pass that queued it, so no cluster update should
        // interleave. That argument is not load-bearing here — the cost of it
        // being wrong is silent protocol corruption on the wire, which is not
        // a thing to leave to an assertion that compiles out of every release
        // build. Refuse the replay instead and let the caller answer 502.
        if replaying && h2 {
            return Err(BackendConnectionError::ReplayRefused(
                "the cluster switched to HTTP/2 between attempts",
            ));
        }

        // ── Legacy `cluster.https_redirect` short-circuit ──
        //
        // Resolve the legacy HTTP→HTTPS redirect BEFORE per-(cluster,
        // source-IP) accounting so a redirect-only request never
        // consumes an IP slot. Otherwise a same-IP client iterating an
        // HTTP→HTTPS hop could trip 429 ahead of the 301 even though no
        // backend would have been opened. A duplicate guard that lived
        // here previously (rebase artefact — two identical
        // `if frontend_should_redirect_https && …` blocks back-to-back)
        // is folded into this single early-return.
        // Frontend-scoped `RedirectPolicy::PERMANENT` already returns
        // from `route_from_request` with the same error, so this only
        // handles the legacy cluster-level path that doesn't surface
        // from `route_from_request`.
        if frontend_should_redirect_https && view.is_http_listener() {
            return Err(BackendConnectionError::RetrieveClusterError(
                RetrieveClusterError::HttpsRedirect,
            ));
        }

        // Per-(cluster, source-IP) connection limit gate. Runs AFTER cluster
        // resolution AND legacy redirect emission (so a 401/421/redirect
        // frontend never trips the limit) and BEFORE any backend selection
        // (so a rejection consumes neither a backend pool slot nor a retry
        // budget). The check uses the source IP from the per-stream
        // `HttpContext.session_address`, which is the proxy-protocol-aware
        // client address when present, falling back to `peer_addr`. The
        // limit governs distinct **frontend connections** per
        // `(cluster, ip)`: an H2 session multiplexing N streams to the same
        // cluster from the same IP still consumes a single slot.
        // This is where phase one ends. The gate needs `SessionManager`, which
        // the core does not hold and cannot be handed as a read-only view
        // because consulting it is only half of what happens here — the other
        // half is `track_cluster_ip`, a mutation of worker-global state. So
        // the core stops, says what to ask, and resumes on the answer.
        //
        // A stream with no source address skips the gate entirely, exactly as
        // before, and is decided outright.
        let Some(ip) = stream_context.session_address.map(|sa| sa.ip()) else {
            return self
                .decide_after_gate(stream_id, context, view, h2, frontend_should_stick)
                .map(ConnectStep::Decided);
        };
        Ok(ConnectStep::CheckIpLimit(ConnectResume {
            h2,
            frontend_should_stick,
            ip,
            max_connections_per_ip: cluster_max_connections_per_ip,
            cluster_retry_after,
            max_connections_per_subnet: cluster_max_connections_per_subnet,
        }))
    }

    /// Finish a decision the embedder paused at
    /// [`ConnectStep::CheckIpLimit`].
    ///
    /// Takes `resume` **by value**, so a parked decision cannot be resumed
    /// twice. The return type is [`ConnectPlan`], not [`ConnectStep`], so a
    /// second gate consultation is not merely wrong but unrepresentable.
    pub(super) fn plan_connect_resume<L: ListenerHandler + L7ListenerHandler>(
        &mut self,
        stream_id: GlobalStreamId,
        context: &mut Context<L>,
        view: &RoutingView<'_>,
        resume: ConnectResume,
        verdict: IpGateVerdict,
    ) -> Result<ConnectPlan, BackendConnectionError> {
        if let IpGateVerdict::AtLimit { retry_after } = verdict {
            // Stash the resolved retry value on the stream so the mux's
            // `BackendConnectionError` -> 429 mapping can render (or elide)
            // the `Retry-After` header without re-deriving the override
            // chain. The embedder resolved it; the core only records it.
            context.streams[stream_id].context.retry_after_seconds =
                Some(retry_after).filter(|v| *v > 0);
            return Err(BackendConnectionError::TooManyConnectionsPerIp {
                cluster_id: context.streams[stream_id]
                    .context
                    .cluster_id
                    .clone()
                    .unwrap_or_default(),
            });
        }
        self.decide_after_gate(
            stream_id,
            context,
            view,
            resume.h2,
            resume.frontend_should_stick,
        )
    }

    /// Everything the decision does once the gate has answered: the pool
    /// reuse scan, and the dial request when nothing is reusable.
    ///
    /// One body shared by both the gated and the ungated entry, so the two
    /// cannot drift — the thing a split like this most easily gets wrong.
    ///
    /// The cluster is read from the stream's `HttpContext`, where
    /// `plan_connect` stored the routed id, rather than handed in as a copy.
    fn decide_after_gate<L: ListenerHandler + L7ListenerHandler>(
        &mut self,
        stream_id: GlobalStreamId,
        context: &mut Context<L>,
        view: &RoutingView<'_>,
        h2: bool,
        frontend_should_stick: bool,
    ) -> Result<ConnectPlan, BackendConnectionError> {
        let stream_context = &context.streams[stream_id].context;
        let Some(cluster_id) = stream_context.cluster_id.as_ref() else {
            error!(
                "{} stream {} reached the backend decision without a routed cluster",
                log_module_context!(stream_context),
                stream_id
            );
            return Err(BackendConnectionError::MaxSessionsMemory);
        };

        /*
        H2 connecting strategy (least-loaded):
        - look at every backend connection
        - among connected backends for this cluster, pick the one with the fewest active streams
        - fall back to a connecting backend if no connected one exists
        - if no backend is to reuse, ask the router for a socket to the "next in line" backend

        H1 strategy: reuse the first KeepAlive backend for this cluster.

        Either way, a connection whose backend has left the configuration is
        never reused, even when a cluster of the same id is configured again.
         */

        let mut reuse_token = None;
        let mut best_h2_stream_count = usize::MAX;
        for (token, backend) in &self.backends {
            match (h2, backend.position()) {
                (_, Position::Server) => {
                    error!(
                        "{} Backend connection unexpectedly behaves like a server",
                        log_module_context!(stream_context)
                    );
                    continue;
                }
                (_, Position::Client(_, _, BackendStatus::Disconnecting)) => {}
                // The cluster id matches, the backend no longer does.
                (_, Position::Client(other_cluster_id, backend, _))
                    if other_cluster_id == cluster_id && view.is_retired(backend) => {}

                (true, Position::Client(other_cluster_id, _, BackendStatus::Connected)) => {
                    if other_cluster_id == cluster_id && !backend.is_draining() {
                        // Pick the non-draining H2 connection with the fewest active streams
                        let Connection::H2(h2c) = backend else {
                            continue;
                        };
                        let stream_count = h2c.core.stream_count();
                        if stream_count
                            >= h2c.core.peer_settings.settings_max_concurrent_streams as usize
                        {
                            continue;
                        }
                        if stream_count < best_h2_stream_count {
                            best_h2_stream_count = stream_count;
                            reuse_token = Some(*token);
                        }
                    }
                }
                (true, Position::Client(other_cluster_id, _, BackendStatus::Connecting(_))) => {
                    // Only use a connecting backend if no connected one was found
                    if other_cluster_id == cluster_id
                        && best_h2_stream_count == usize::MAX
                        && matches!(backend, Connection::H2(_))
                    {
                        reuse_token = Some(*token)
                    }
                }
                (true, Position::Client(other_cluster_id, _, BackendStatus::KeepAlive)) => {
                    if other_cluster_id == cluster_id && matches!(backend, Connection::H2(_)) {
                        error!(
                            "{} ConnectionH2 unexpectedly behaves like H1 with KeepAlive",
                            log_module_context!(stream_context)
                        );
                    }
                }

                (false, Position::Client(old_cluster_id, _, BackendStatus::KeepAlive)) => {
                    if old_cluster_id == cluster_id {
                        reuse_token = Some(*token);
                        break;
                    }
                }
                // can't bundle H1 streams together
                (false, Position::Client(_, _, BackendStatus::Connected))
                | (false, Position::Client(_, _, BackendStatus::Connecting(_))) => {}
            }
        }
        trace!(
            "{} connect: (stick={}, h2={}) -> (reuse={:?})",
            log_module_context!(stream_context),
            frontend_should_stick,
            h2,
            reuse_token
        );

        if let Some(token) = reuse_token {
            // Pool reuse: an existing backend connection (H2 multiplex slot or
            // H1 keep-alive socket) is being reattached to this stream. Pair
            // with `backend.pool.miss` below — together they describe the
            // pool's hit/miss ratio. Counted before any commit so the metric
            // is consistent with the trace log.
            incr!(names::backend::POOL_HIT);
            trace!(
                "{} reused backend: {:#?}",
                log_module_context!(stream_context),
                self.backends.get(&token)
            );
            // Link backend to stream for the reused connection path. We check
            // that the backend can accept a new stream before committing any
            // per-stream state.
            let Some(backend_conn) = self.backends.get_mut(&token) else {
                error!(
                    "{} reused backend token {:?} missing from backends map",
                    log_module_context!(stream_context),
                    token
                );
                return Err(BackendConnectionError::MaxSessionsMemory);
            };
            if !backend_conn.start_stream(stream_id, context) {
                // Use `context.http_context(stream_id)` instead of reusing
                // `stream_context`: `start_stream` above takes `&mut
                // context`, which reborrows the slab mutably and ends any
                // outstanding `stream_context` reference. A fresh shared
                // borrow via the accessor is borrow-check clean.
                error!(
                    "{} Backend rejected stream start (max concurrent streams reached)",
                    log_module_context!(context.http_context(stream_id))
                );
                return Err(BackendConnectionError::MaxSessionsMemory);
            }
            // For reused backends: set context fields and metrics lifecycle.
            // Both values are read off the connection's own `BackendId`; the id
            // is an `Rc` clone, not a heap copy (#1579). They are identity,
            // immutable for the life of the registry entry: the answer the
            // registry borrow used to give, without the core holding the handle.
            if let Some(backend_conn) = self.backends.get(&token)
                && let Position::Client(_, backend, _) = backend_conn.position()
            {
                let stream = &mut context.streams[stream_id];
                stream.context.backend_id = Some(Rc::clone(&backend.backend_id));
                stream.context.backend_address = Some(backend.address);
                stream.metrics.backend_id = Some(Rc::clone(&backend.backend_id));
                stream.metrics.backend_start();
                stream.metrics.backend_connected();
            }
            context.link_stream(stream_id, token);
            return Ok(ConnectPlan::Attached);
        }

        // New-backend path: no reusable connection was found (no live H2
        // multiplex slot for this cluster, no H1 keep-alive socket), so a
        // fresh TCP dial and full backend handshake have to follow.
        //
        // Pool miss, counted here rather than after the dial so the count
        // includes attempts that fail at backend selection
        // (`BackendError::NoBackendForCluster`, etc.) — every miss is a slot
        // we did not save. Pair with `backend.pool.hit` above. The dial
        // itself may still fail, in which case `backend.pool.size` is never
        // bumped but the miss is already counted.
        incr!(names::backend::POOL_MISS);

        // Everything past this point was the embedder's work all along: the
        // selection, the `connect(2)`, `set_nodelay`, the slab session, the
        // epoll registration and their rollback. It now reads that way.
        // `Mux::ready_inner` performs it in the same order and calls back
        // into `Router::commit_dialed`.
        //
        // The new connection's `Position::Client` shares the routed id: the
        // plan carries a handle on it and `Mux::attach_dialed` moves it there.
        Ok(ConnectPlan::Dial {
            cluster_id: cluster_id.clone(),
            h2,
            frontend_should_stick,
        })
    }

    /// Take ownership of a backend connection the embedder dialled, built and
    /// registered, and attach `stream_id` to it.
    ///
    /// The reply half of [`ConnectPlan::Dial`]. By the time this runs the
    /// embedder has already proved the connection usable — `start_stream`
    /// returned true and the epoll registration succeeded — so this performs
    /// the two steps that are the router's own: taking the connection into
    /// the backend map under its token, and linking the stream to it.
    ///
    /// Deliberately infallible. Every way the dial could fail is decided
    /// before the caller reaches here, which is what keeps the CWE-400
    /// rollback discipline in one place instead of split across the seam.
    pub(super) fn commit_dialed<L: ListenerHandler + L7ListenerHandler>(
        &mut self,
        stream_id: GlobalStreamId,
        context: &mut Context<L>,
        token: Token,
        connection: Connection<SessionTcpStream>,
    ) {
        // No timer arming here: the connection arms its own connect-timeout
        // DEADLINE at construction and the `Mux` adapter reflects it onto the
        // wheel when `ready()` reschedules, which is the same pass this runs
        // in. Until that reschedule no wheel entry exists — which is what the
        // caller's rollback paths want, since they drop the connection
        // without ever reaching here.
        self.backends.insert(token, connection);
        context.link_stream(stream_id, token);
    }

    fn route_from_request<L: ListenerHandler + L7ListenerHandler>(
        &mut self,
        context: &mut HttpContext,
        front: &mut super::GenericHttpStream,
        listener: &Rc<RefCell<L>>,
        view: &RoutingView<'_>,
    ) -> Result<ClusterId, RetrieveClusterError> {
        let (host, uri, method) = match context.extract_route() {
            Ok(tuple) => tuple,
            Err(cluster_error) => {
                // we are past kawa parsing if it succeeded this can't fail
                // if the request was malformed it was caught by kawa and we sent a 400
                error!(
                    "{} Malformed request in connect (should be caught at parsing) {:?}: {}",
                    log_module_context!(context),
                    context,
                    cluster_error
                );
                return Err(cluster_error);
            }
        };
        // ── TLS cert SAN ↔ HTTP :authority binding ────────────────────────
        // Reject any request whose `:authority` is not covered by a SAN of
        // the certificate Sōzu actually served at the TLS handshake, with
        // RFC 6125 §6.4.3 wildcard handling. Without this binding, an
        // attacker holding a valid certificate for tenant A could open TLS
        // with SNI=A then send an H2 stream with `:authority=tenantB.…` and
        // reach tenant B's backend, crossing the TLS trust boundary
        // (CWE-346 / CWE-444). The H2 spec explicitly allows browsers to
        // coalesce streams onto a connection whenever the server is
        // authoritative for the new origin (RFC 7540 §9.1.1 / RFC 9113
        // §9.1.1), which "authoritative" means "covered by a SAN of the
        // served cert"; rejecting coalesced streams as 421 caused the
        // user-visible bug this predicate fixes (RFC 9110 §15.5.20).
        //
        // Plaintext listeners bypass the check (SNI is always `None`).
        // Connections where SNI was sent but no cert matched (rustls served
        // the default cert) carry `Some(empty)` SAN snapshot, so every
        // authority is rejected — Sōzu is not authoritative for any name.
        // Connections with no SNI fall back to the legacy exact-SNI match
        // predicate (`authority_matches_sni`) for parity with pre-fix
        // behaviour on the pathological "no SNI" case.
        // Operators may opt out per-listener via
        // `HttpsListenerConfig::strict_sni_binding = false`.
        if let Some(sni) = context
            .tls_server_name
            .as_deref()
            .filter(|_| context.strict_sni_binding)
        {
            let matched: Option<&str> = match context.tls_cert_names.as_deref() {
                Some(cert_names) => authority_matched_cert_name(host, cert_names),
                None => {
                    if authority_matches_sni(host, sni) {
                        Some(sni)
                    } else {
                        None
                    }
                }
            };
            match matched {
                Some(matched_name) => {
                    // Real coalescing = matched SAN differs from the SNI's
                    // value after the matcher's port-strip + ASCII case
                    // folding. Same-name requests are the common
                    // non-coalesced path; do not pollute the counter or
                    // logs with them. The ALPN=`h2` gate is a defensive
                    // guard, not load-bearing under current invariants —
                    // every request reaching `route_from_request` on an
                    // HTTPS listener with `tls_cert_names` populated has
                    // already gone through the H2 mux (ALPN=h2 by
                    // construction). Kept explicit so a future routing
                    // refactor that funnels H1 keep-alive through the
                    // same predicate doesn't silently double-count
                    // sequential `Host:` reuse as "coalescing".
                    if !authority_matches_sni(host, sni) && context.tls_alpn == Some("h2") {
                        incr!(names::h2::COALESCING_ACCEPTED);
                        log_coalescing_accepted(context, host, sni, matched_name);
                    }
                }
                None => {
                    incr!(names::http::SNI_AUTHORITY_MISMATCH);
                    log_sni_authority_mismatch(
                        context,
                        host,
                        sni,
                        context.tls_cert_names.as_deref().map(Vec::as_slice),
                    );
                    return Err(RetrieveClusterError::SniAuthorityMismatch {
                        sni: sni.to_owned(),
                        authority: host.to_owned(),
                    });
                }
            }
        }

        let route_result = listener.borrow().frontend_from_request(host, uri, method);

        let route = match route_result {
            Ok(route) => route,
            Err(frontend_error) => {
                trace!("{} {}", log_module_context!(context), frontend_error);
                return Err(RetrieveClusterError::RetrieveFrontend(frontend_error));
            }
        };

        // ── Resolve the routing decision ──────────────────────────────────
        // Snapshot the policy fields we need before consuming `route`, then
        // map each policy outcome to either an early-error variant (which
        // the caller turns into a default answer) or a cluster_id (which
        // proceeds to backend connect).
        let RouteResult {
            cluster_id,
            redirect,
            redirect_scheme,
            redirect_template,
            rewritten_host,
            rewritten_path,
            rewritten_port,
            headers_request,
            headers_response,
            required_auth: frontend_required_auth,
            tags,
        } = route;

        // The matched frontend rule is the only correct owner of the
        // access-log tags, so stash them here — before every early return
        // below, so a redirect, a 401 and a backend-connect failure log
        // them too. The listener's `BTreeMap<String, CachedTags>` cannot
        // do this job: it is written under the frontend rule's hostname
        // and read under the request's authority, two spellings that only
        // coincide for an exact literal frontend (sozu#1379).
        //
        // No `..` in the destructure above, deliberately: that wildcard is
        // exactly how `tags` was dropped on the floor for as long as the
        // bug lived. A future `RouteResult` field now has to be routed
        // here explicitly or the build breaks.
        context.tags = tags;

        // ── HSTS (RFC 6797) snapshot hoist for HTTPS ──────────────────────
        // The response snapshot is built in two passes so HSTS reaches
        // every HTTPS response code (RFC 6797 §8.1 — including
        // proxy-generated 3xx / 401 / 5xx default answers) WITHOUT
        // changing the pre-PR scope of operator-defined `Append`
        // response headers (which only apply on the regular forward
        // path).
        //
        // Pass 1 (here, before any early return): for HTTPS only, copy
        // ONLY the HSTS-class typed edits (`SetIfAbsent | Set`). These
        // need to land on default answers — `set_default_answer_with_retry_after`
        // bypasses the post-forward copy below.
        //
        // Pass 2 (post-forward, end of function): copy EVERY edit
        // (including operator `Append` headers). Runs only on the
        // regular forward path because the early returns short-circuit
        // before reaching it.
        //
        // Plain-HTTP listeners are skipped here per RFC 6797 §7.2 (no
        // STS over plaintext) — defense in depth on top of the
        // TOML-time `ConfigError::HstsOnPlainHttp` and the worker IPC
        // `ProxyError::HstsOnPlainHttp` rejects.
        if matches!(context.protocol, crate::Protocol::HTTPS) {
            snapshot_response_edits(&mut context.headers_response, &headers_response, |e| {
                matches!(e.mode, HeaderEditMode::SetIfAbsent | HeaderEditMode::Set)
            });
        }

        // Look up cluster-side policy knobs once. The values we need are:
        //  - `https_redirect` (legacy) and `https_redirect_port` for the 301 location URL
        //  - `authorized_hashes` and `www_authenticate` for the 401 path
        let (legacy_https_redirect, https_redirect_port, authorized_hashes, www_authenticate) =
            match cluster_id.as_deref() {
                Some(id) => view
                    .cluster(id)
                    .map(|c| {
                        (
                            c.https_redirect,
                            c.https_redirect_port,
                            c.authorized_hashes.clone(),
                            c.www_authenticate.clone(),
                        )
                    })
                    .unwrap_or((false, None, Vec::new(), None)),
                None => (false, None, Vec::new(), None),
            };

        // ── 1. Explicit redirect policies (PERMANENT / FOUND / PERMANENT_REDIRECT) ──
        // Resolved BEFORE the clusterless-deny branch so a frontend that
        // declares `redirect = permanent | found | permanent_redirect`
        // emits the matching 3xx even when no cluster is bound. This is
        // the canonical "moved" shape from the original proposal in
        // #1161 and is the only way to express "this hostname has moved"
        // without standing up a dummy cluster. The block does not read
        // `cluster_id`; per-cluster values (`https_redirect_port`,
        // `www_authenticate`, …) default to safe sentinels at the cluster
        // lookup above when `cluster_id` is `None`, so the reorder is
        // data-flow-safe.
        //
        // Status code mapping (closes #1009):
        //   Permanent          → 301 (RFC 9110 §15.4.2)
        //   Found              → 302 (RFC 9110 §15.4.3) — UA may rewrite POST→GET
        //   PermanentRedirect  → 308 (RFC 9110 §15.4.9) — method MUST be preserved
        let redirect_status = match redirect {
            RedirectPolicy::Permanent => Some(301u16),
            RedirectPolicy::Found => Some(302u16),
            RedirectPolicy::PermanentRedirect => Some(308u16),
            // Forward / Unauthorized are handled by other branches
            // below; keeping them named here forces an exhaustive
            // match so a future RedirectPolicy variant doesn't
            // silently fall through to `None`.
            RedirectPolicy::Forward | RedirectPolicy::Unauthorized => None,
        };
        if let Some(status_code) = redirect_status {
            let scheme = resolve_redirect_scheme(redirect_scheme, context);
            let port = rewritten_port.map(|p| p as u32).or(https_redirect_port);
            // Feed the rewritten host AND path into the `Location` URL
            // when the frontend's RewriteParts populated them. Without
            // this, a `redirect = permanent` frontend with
            // `rewrite_host = "new.example.com"` would serve clients
            // back to the original `Host:` header, defeating the
            // documented `old → new` shape.
            // The host_override path also keeps `:port` stripping
            // intact: `build_redirect_location` removes any `:port` on
            // the override before reapplying `port_suffix`.
            context.redirect_location = Some(build_redirect_location(
                scheme,
                context,
                port,
                rewritten_host.as_deref(),
                rewritten_path.as_deref(),
            ));
            // Stash the frontend's `redirect_template` (when set) so the
            // 3xx default-answer path can render it via
            // `HttpAnswers::render_inline_redirect` instead of the
            // listener / cluster default. Without this stash the field
            // flows into `RouteResult` only to be dropped by the
            // wildcard destructure below, so the operator-supplied
            // template has no observable effect on the rendered
            // redirect.
            context.frontend_redirect_template = redirect_template;
            // Stash the resolved status so the answer engine picks the
            // matching default template (`http.301.redirection` /
            // `http.302.redirection` / `http.308.redirection`).
            context.redirect_status = Some(status_code);
            return Err(RetrieveClusterError::HttpsRedirect);
        }

        // ── 2. Explicit `RedirectPolicy::UNAUTHORIZED` or clusterless deny ─
        // Reached when the frontend either explicitly asks for 401 or has
        // no backing cluster and no `Permanent` redirect to honour. The
        // `Forward + cluster_id == None` combination collapses here so
        // legacy clusterless frontends still emit 401 by default.
        if matches!(redirect, RedirectPolicy::Unauthorized) || cluster_id.is_none() {
            context.www_authenticate = www_authenticate.clone();
            trace!("{} RouteResult::deny", log_module_context!(context));
            return Err(RetrieveClusterError::UnauthorizedRoute);
        }

        let Some(cluster_id) = cluster_id else {
            // Guarded by the clusterless-deny branch immediately above;
            // the `is_none()` arm has already returned `UnauthorizedRoute`
            // by the time control reaches here.
            unreachable!("cluster_id was checked Some above")
        };

        // ── 3. Legacy `cluster.https_redirect` (HTTP-only listeners) ───────
        // The caller (`Router::plan_connect`) emits the actual 301 only on
        // `ListenerType::Http`; gate the URL stash on the same predicate
        // so an HTTPS listener never carries a stale `redirect_location`
        // into a downstream default-answer path.
        if legacy_https_redirect && view.is_http_listener() {
            let port = https_redirect_port;
            context.redirect_location =
                Some(build_redirect_location("https", context, port, None, None));
        }

        // ── 4. Basic auth check (only when `required_auth` was set) ────────
        // The check iterates the full hash list in constant time (see
        // `crate::protocol::mux::auth::check_basic`) so the time spent
        // does not leak which hash matched, or whether any did at all.
        // On failure, stash the cluster's `www_authenticate` realm so the
        // 401 default-answer can render the matching `WWW-Authenticate`
        // header. An empty realm causes the template engine to elide the
        // header entirely (`or_elide_header = true`).
        if frontend_required_auth
            && !crate::protocol::mux::auth::check_basic(front, &authorized_hashes)
        {
            context.www_authenticate = www_authenticate.clone();
            trace!(
                "{} basic-auth check failed; emitting 401",
                log_module_context!(context)
            );
            return Err(RetrieveClusterError::UnauthorizedRoute);
        }

        // ── 5. Request-side mutations on the front kawa ────────────────────
        // From here on the route is a Forward — apply the frontend's
        // rewrite + header policy to the request kawa so the backend
        // wire carries the operator-configured shape.
        apply_request_rewrites_and_headers(
            front,
            context,
            rewritten_host.as_deref(),
            rewritten_path.as_deref(),
            &headers_request,
        );

        // Pass 2 of the response-snapshot copy (see the HSTS hoist
        // above). Runs unconditionally on the regular forward path
        // (the early returns above bypass this site, which keeps the
        // default-answer scope as HSTS-only). Copies EVERY edit so
        // operator-defined `Append` response headers reach
        // backend-served responses on both HTTP and HTTPS listeners,
        // preserving their pre-PR scope.
        snapshot_response_edits(&mut context.headers_response, &headers_response, |_| true);

        Ok(cluster_id)
    }

    /// Pick the backend for an already-routed request and stamp its identity
    /// onto the request's [`HttpContext`].
    ///
    /// Takes no listener handle: every listener-derived value this needs —
    /// `sticky_name` above all — is already on the `HttpContext` that
    /// `Context::create_stream` filled when the request arrived, and reading
    /// the listener again here would be exactly the mid-request reload leak
    /// `LIFECYCLE.md` §2.5 rules out.
    ///
    /// Takes no proxy handle either: the backend set arrives as `selector`, a
    /// capability borrowed for this one call (see [`BackendSelector`]).
    ///
    /// Returns the chosen backend, reserved but not dialled: the caller
    /// connects to [`BackendId::address`] (#1684).
    pub fn backend_from_request(
        &mut self,
        cluster_id: &str,
        frontend_should_stick: bool,
        context: &mut HttpContext,
        selector: &mut dyn BackendSelector,
    ) -> Result<BackendId, BackendConnectionError> {
        // Spelled out rather than folded into the selector's signature: a
        // cookie the client sent to a frontend that does not stick is
        // ignored, exactly as the `(false, Some(_))` arm of the match this
        // replaced ignored it.
        let affinity = if frontend_should_stick {
            Affinity::Sticky(context.sticky_session_found.as_deref())
        } else {
            Affinity::Unpinned
        };
        let selected = selector
            .select(cluster_id, affinity, context.affinity_key)
            .map_err(|backend_error| {
                trace!("{} {}", log_module_context!(context), backend_error);
                BackendConnectionError::Backend(backend_error)
            })?;
        debug_assert_eq!(
            selected.sticky_session.is_some(),
            frontend_should_stick,
            "a selector resolves a sticky cookie exactly when the frontend sticks"
        );

        // `context.sticky_name` is the name `Context::create_stream` captured
        // when this request arrived, and it stays that name to the end of the
        // request. Re-reading the listener here used to hand an in-flight
        // request a cookie name an operator installed after it started — the
        // request would then have been matched on the old name and answered
        // with a `Set-Cookie` under the new one. A reload applies from the
        // next request (`LIFECYCLE.md` §2.5).
        if let Some(sticky_session) = selected.sticky_session {
            context.sticky_session = Some(sticky_session);
        }

        context.backend_id = Some(Rc::clone(&selected.backend.backend_id));
        context.backend_address = Some(selected.backend.address);

        Ok(selected.backend)
    }
}

/// The worker's backend set, as the one capability
/// [`Router::backend_from_request`] needs from it: pick a backend and reserve
/// a connection on it.
///
/// Question 12 of [#1340](https://github.com/sozu-proxy/sozu/issues/1340)
/// asked for a borrowed view of backend load state, "the same shape"
/// `RoutingView` gave cluster configuration. That shape cannot select.
/// `RoutingView` is an immutable borrow of immutable data, and selection
/// mutates: the round-robin cursor, the fail-open latch, `PeakEWMA` under
/// `LoadMetric::ConnectionTime`, the Maglev table and the cluster
/// availability latch. A snapshot of the load counters cannot be taken
/// without allocating either — each backend sits behind its own `RefCell`, so
/// lending the core N of them at once needs a container of N `Ref`s. The shape
/// that fits is the UDP core's `BackendSource` (`lib/src/protocol/udp/mod.rs`),
/// a `&mut dyn` the core calls and the embedder implements.
///
/// # Selection reserves, a failed dial releases
///
/// The selector returns a backend and no socket: the embedder dials
/// [`BackendId::address`] itself, which is what lets a selector exist without
/// an OS socket type (#1684). What keeps the load balancer honest is that
/// selecting **reserves**: the embedder's selector raises the chosen
/// backend's `active_connections` under the same borrow as the choice
/// (`BackendMap::reserve_backend`), so the second stream linking in the same
/// pass weighs the first one's connection before it has been dialled, exactly
/// as it weighed the connection `Backend::try_connect` used to count at the
/// dial. A `connect(2)` that fails releases the reservation through the
/// ledger (`BackendChange::DialFailed`), and LIFECYCLE §9 invariant 14's
/// drain before each selection applies it before anything reads the counters
/// again. A dial that connects but is abandoned before its connection is
/// registered — no buffer for an H2 client, a refused stream start, a failed
/// `register_socket` — releases it through `BackendChange::ConnectionClosed`,
/// recording no failure (#1713). A dial that succeeds keeps the reservation
/// as its connection's count, and the connection's close releases it.
///
/// # Why a trait and not `&mut BackendMap`
///
/// A bare `&mut BackendMap` would be smaller, and it would still leave
/// [`Router::backend_from_request`] handing back an `Rc<RefCell<Backend>>`,
/// because turning that handle into the opaque [`BackendId`] takes the
/// session's `BackendRegistry` (`lib/src/protocol/mux/mod.rs`) as well. The
/// embedder's implementation holds both, so no registry handle reaches this
/// file at all. It is also what lets a simulator stand in for the backend set:
/// such a selector mints its ids with [`BackendId::new`] and needs no socket.
///
/// `Router` has no lifetime parameter, so it cannot keep the `&mut` it is
/// lent; the `Rc<RefCell<dyn L7Proxy>>` this replaced was `'static` and
/// `Clone`, and nothing stopped it being stored.
pub trait BackendSelector {
    /// Select a backend of `cluster_id` and reserve a connection on it.
    ///
    /// Under [`Affinity::Sticky`] the result carries the cookie value to
    /// answer with, read from the chosen backend under the same borrow as the
    /// selection. It is resolved here rather than carried on [`BackendId`]
    /// because `Backend::sticky_id` is not immutable: `BackendList::add_backend`
    /// (`lib/src/backends.rs`) rewrites it in place on the live registry entry
    /// when an existing backend is re-added, and `BackendId` carries only what
    /// cannot change for the life of an entry.
    ///
    /// `key` is the client affinity key `affinity_key` in this module derived
    /// for the request, `None` unless the cluster selects with `HRW` or
    /// `MAGLEV`. It decides where the load balancer sends a request the sticky
    /// cookie did not pin, and nothing else: a cookie naming a live backend
    /// still wins.
    fn select(
        &mut self,
        cluster_id: &str,
        affinity: Affinity<'_>,
        key: Option<u64>,
    ) -> Result<SelectedBackend, BackendError>;
}

/// Whether selection for `cluster` reads a client affinity key: its policy
/// is `HRW` or `MAGLEV`, or it shards (`shard_percent`), which ranks each
/// client's shard by the key under any policy (sozu-proxy/sozu#524). Every
/// other cluster derives no key and pays nothing.
pub(crate) fn cluster_reads_affinity_key(cluster: &Cluster) -> bool {
    cluster.shard_percent.is_some()
        || matches!(
            cluster.load_balancing(),
            LoadBalancingAlgorithms::Hrw | LoadBalancingAlgorithms::Maglev
        )
}

/// The client affinity key of a request routed to `cluster`, for the `HRW`
/// and `MAGLEV` policies to pin the client with and for shuffle sharding to
/// rank its shard by; `None` for every other cluster, which would ignore it
/// ([`cluster_reads_affinity_key`]).
///
/// The key is the hash of the cluster's `affinity_header` value (first
/// occurrence, name matched case-insensitively) or `affinity_cookie` value
/// (name matched case-sensitively, RFC 6265), and, when the request does not
/// carry that header or cookie, or carries it empty, of the client's source
/// IP — `session_address`, the PROXY-protocol source when the listener
/// expects one. With no source address either, there is no key, and the
/// policies fall back to round-robin as they always did.
///
/// A cookie named like the listener's sticky-session cookie is read from the
/// value `HttpContext::on_request_headers` captured, since that cookie is
/// elided from the jar before routing. Values are hashed raw, as sent: a
/// quoted cookie value and its unquoted spelling are two different keys.
///
/// It reads the header blocks and cookie jar where they lie in the request
/// buffer and hashes the bytes in place: no copy, no allocation. The hash is
/// the seeded FNV of `crate::load_balancing`, so every worker and every
/// restart derives the same key for the same client.
///
/// A header or cookie value is chosen by the client, so a client can choose
/// its backend under this option — exactly as it can with a sticky-session
/// cookie. It spreads clients; it does not isolate them from one another.
pub(super) fn affinity_key(
    cluster: &Cluster,
    front: &GenericHttpStream,
    session_address: Option<SocketAddr>,
    sticky: StickyCookie<'_>,
) -> Option<u64> {
    if !cluster_reads_affinity_key(cluster) {
        return None;
    }
    // `validate_affinity_key` (`command/src/config.rs`) refuses a cluster
    // naming both on every `AddCluster` the master accepts; the header is
    // read first should a worker ever receive both.
    let buf = front.storage.buffer();
    let value = if let Some(name) = cluster.affinity_header.as_deref() {
        front.blocks.iter().find_map(|block| match block {
            Block::Header(header)
                if !header.is_elided()
                    && compare_no_case(header.key.data(buf), name.as_bytes()) =>
            {
                Some(header.val.data(buf))
            }
            _ => None,
        })
    } else if let Some(name) = cluster.affinity_cookie.as_deref() {
        if name == sticky.name {
            // `HttpContext::on_request_headers` has already elided this
            // cookie from the jar (Sōzu's own session cookie never reaches a
            // backend) and captured its value, so the key reads the capture.
            sticky.value.map(str::as_bytes)
        } else {
            front.detached.jar.iter().find_map(|cookie| {
                (!cookie.is_elided() && cookie.key.data(buf) == name.as_bytes())
                    .then(|| cookie.val.data(buf))
            })
        }
    } else {
        None
    };
    // Pre: a request value is read only for a cluster that names one.
    debug_assert!(
        value.is_none() || cluster.affinity_header.is_some() || cluster.affinity_cookie.is_some(),
        "a header or cookie value keys only a cluster that configured its name"
    );
    let key = match value.filter(|value| !value.is_empty()) {
        Some(value) => Some(affinity_key_from_value(value)),
        None => session_address.map(|address| affinity_key_from_ip(address.ip())),
    };
    // Post: under a keyed policy the only request left without a key is one
    // that carries no usable value AND has no source address.
    debug_assert!(
        key.is_some() || session_address.is_none(),
        "a request with a source address must always be keyed"
    );
    key
}

/// The listener's sticky-session cookie as the request arrived with it: its
/// name, and the value `HttpContext::on_request_headers` captured before
/// eliding it from the cookie jar. [`affinity_key`] reads it when a cluster's
/// `affinity_cookie` names that same cookie.
#[derive(Clone, Copy, Debug)]
pub(super) struct StickyCookie<'a> {
    pub(super) name: &'a str,
    pub(super) value: Option<&'a str>,
}

/// Whether a request is pinned to a backend by its sticky-session cookie.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Affinity<'a> {
    /// The frontend does not stick: any cookie the client sent is ignored.
    Unpinned,
    /// The frontend sticks. `Some` is the cookie the request carried, tried
    /// first; `None`, or a cookie naming no live backend, falls back to the
    /// cluster's load balancer. Either way the answer carries a cookie.
    Sticky(Option<&'a str>),
}

/// What [`BackendSelector::select`] answers: the chosen backend, whose
/// [`BackendId::address`] the caller dials, and — only under
/// [`Affinity::Sticky`] — the cookie value that pins the client to it.
#[derive(Debug)]
pub struct SelectedBackend {
    pub backend: BackendId,
    pub sticky_session: Option<String>,
}

/// Apply the frontend's request-side rewrite + header policy to the
/// request kawa. Mutations land before backend connect so the backend
/// wire carries the rewritten shape:
///
/// 1. If `rewritten_host` is set, replace the request-line authority
///    with the rewritten value, replace any existing `Host` request
///    header (so H1 backends see the same value the H2 `:authority`
///    would carry), and inject `X-Forwarded-Host` carrying the
///    pre-rewrite authority. The X-Forwarded-Host injection ONLY fires
///    when `rewritten_host` is set — without a rewrite there is no host
///    swap to disclose, and HAProxy's `option forwardfor` style
///    headers (`X-Forwarded-For`, `X-Forwarded-Proto`) still flow from
///    the kawa parser. The pre-rewrite authority is read from
///    `context.authority`, which no rewrite touches, and recorded in
///    `context.original_authority` only here, so a request without a
///    host rewrite copies no authority. `X-Forwarded-Host` belongs to the
///    `X-Forwarded-*` family, so it is injected (and a client one
///    replaced) only when the listener's `forwarded_headers` mode emits
///    that family (`both`, `x_forwarded`); `rfc7239` and `none` convey no
///    pre-rewrite authority, and under `none` a client `X-Forwarded-Host`
///    passes through as sent.
///    Dedup rule: the synthetic Host AND any pre-existing Host header
///    are dropped in the retain pass below before the rewritten Host is
///    appended, so the wire never carries two `Host:` headers.
/// 2. If `rewritten_path` is set, replace both the abstract path
///    (consumed by H2 `:path`) and the request-line URI (consumed by
///    the H1 converter) so cardinality H1↔H1, H1↔H2, H2↔H1, H2↔H2 all
///    propagate the rewritten target.
/// 3. For every `headers_request` edit:
///    - empty `val` → remove every existing header with the matching
///      name from `kawa.blocks` (HAProxy `del-header` parity);
///    - non-empty `val` → append the header before the `end_header`
///      flag block. Set/replace semantics: callers that want to replace
///      a header pass two edits (one delete with empty val, one set
///      with the new value).
fn apply_request_rewrites_and_headers(
    kawa: &mut super::GenericHttpStream,
    context: &mut HttpContext,
    rewritten_host: Option<&str>,
    rewritten_path: Option<&str>,
    headers_request: &[HeaderEdit],
) {
    use kawa::{Block, Pair, Store};

    if rewritten_host.is_none() && rewritten_path.is_none() && headers_request.is_empty() {
        return;
    }

    // `context.authority` is the authority the client sent: the rewrites
    // below mutate the kawa status line and headers, never the context. It
    // is copied only when the host is rewritten, for the X-Forwarded-Host
    // injection, and the copy then moves into `context.original_authority`.
    let original_authority: Option<String> = if rewritten_host.is_some() {
        context.authority.clone()
    } else {
        None
    };

    // ── status-line authority / path rewrites ─────────────────────────
    // The kawa request status line carries both `path` and `uri` —
    // `path` is the abstract path (consumed by the H2 converter to
    // emit `:path`) while `uri` is the request-line URI (consumed by
    // the H1 converter at `kawa::protocol::h1::converter`). Both must
    // be mutated so an H1 frontend forwarding to an H1 backend AND an
    // H2 frontend forwarding to an H1 backend (or vice versa) see the
    // rewritten target on the wire.
    if (rewritten_host.is_some() || rewritten_path.is_some())
        && let kawa::StatusLine::Request {
            authority,
            path,
            uri,
            ..
        } = &mut kawa.detached.status_line
    {
        if let Some(new_host) = rewritten_host {
            *authority = Store::from_string(new_host.to_owned());
        }
        if let Some(new_path) = rewritten_path {
            *path = Store::from_string(new_path.to_owned());
            *uri = Store::from_string(new_path.to_owned());
        }
    }

    // ── single-pass split: deletes vs. sets ───────────────────────────
    // Walk `headers_request` once and separate each edit into either the
    // delete list (empty val) or the insert list (non-empty val). Two
    // passes was wasteful when an operator stacks many `--header` flags;
    // one pass keeps the allocation profile flat.
    let host_lower = b"host";
    let xfh_lower = b"x-forwarded-host";
    let rewriting_host = rewritten_host.is_some();
    // X-Forwarded-Host discloses the rewrite only in a mode that emits the
    // X-Forwarded-* family; `rfc7239` already elided a client one in
    // `HttpContext::on_request_headers`, and `none` leaves it as sent.
    let synthesises_xfh = rewriting_host && emits_x_forwarded(context.forwarded_headers);
    let mut keys_to_drop: Vec<Vec<u8>> = Vec::with_capacity(headers_request.len() + 2);
    let mut to_insert: Vec<Block> = Vec::with_capacity(headers_request.len() + 2);
    // Track whether any operator-supplied edit names Host or
    // X-Forwarded-Host so we always dedup the existing kawa Host header
    // before inserting the operator's value. Without this, an operator
    // who sets `--header request=Host=evil` on a frontend WITHOUT
    // `--rewrite-host` lands TWO `Host:` headers on the backend wire —
    // a request-smuggling primitive on backends that pick last-Host
    // (CWE-444 cousin).
    let mut operator_overrides_host = false;
    let mut operator_overrides_xfh = false;
    for edit in headers_request {
        let key_is_host = edit.key.eq_ignore_ascii_case(host_lower);
        let key_is_xfh = edit.key.eq_ignore_ascii_case(xfh_lower);
        operator_overrides_host |= key_is_host;
        operator_overrides_xfh |= key_is_xfh;
        if edit.val.is_empty() {
            keys_to_drop.push(edit.key.iter().map(u8::to_ascii_lowercase).collect());
        } else {
            to_insert.push(Block::Header(Pair {
                key: Store::from_slice(&edit.key),
                val: Store::from_slice(&edit.val),
            }));
        }
    }
    if rewriting_host || operator_overrides_host {
        keys_to_drop.push(host_lower.to_vec());
    }
    if synthesises_xfh || operator_overrides_xfh {
        keys_to_drop.push(xfh_lower.to_vec());
    }

    // ── delete pass on existing blocks ────────────────────────────────
    let buf_ptr = kawa.storage.buffer();
    if !keys_to_drop.is_empty() {
        // Read `key.data(buf_ptr)` only on non-elided headers — kawa's
        // earlier passes (HPACK decoder, H1 header parser) tag suppressed
        // headers with `Store::Empty` rather than removing them, and
        // calling `.data()` on `Store::Empty` panics in
        // `kawa-0.6.8/src/storage/repr.rs`. Pinning the guard explicitly
        // until kawa changes its policy.
        let buf = buf_ptr;
        kawa.blocks.retain(|block| {
            if let Block::Header(Pair { key, val: _ }) = block {
                if matches!(key, Store::Empty) {
                    return true;
                }
                let key_bytes = key.data(buf);
                // Both `keys_to_drop` and `key_lower` are pre-lowercased,
                // so a byte-equality compare is sufficient — a second
                // ASCII-fold pass via `compare_no_case` would just burn
                // cycles re-folding bytes that are already canonical.
                let key_lower: Vec<u8> = key_bytes.iter().map(u8::to_ascii_lowercase).collect();
                !keys_to_drop
                    .iter()
                    .any(|k| k.as_slice() == key_lower.as_slice())
            } else {
                true
            }
        });
    }

    // ── insertion before the end-of-headers flag ──────────────────────
    // Every header we add (rewritten Host, X-Forwarded-Host,
    // operator-supplied set/append edits) must land before
    // `Block::Flags { end_header: true }` so the converter emits them
    // as part of the request header block. Synthetic Host/X-Forwarded-Host
    // are prepended (they describe the rewrite, not an operator policy).
    let end_header_idx = super::shared::end_of_headers_index(kawa);

    if rewriting_host {
        let mut synth: Vec<Block> = Vec::with_capacity(2);
        if let Some(new_host) = rewritten_host {
            synth.push(Block::Header(Pair {
                key: Store::Static(b"Host"),
                val: Store::from_string(new_host.to_owned()),
            }));
        }
        if synthesises_xfh && let Some(orig) = original_authority.as_deref() {
            synth.push(Block::Header(Pair {
                key: Store::Static(b"X-Forwarded-Host"),
                val: Store::from_string(orig.to_owned()),
            }));
        }
        context.original_authority = original_authority;
        synth.append(&mut to_insert);
        to_insert = synth;
    }
    if !to_insert.is_empty() {
        let insert_at = end_header_idx.unwrap_or(kawa.blocks.len());
        for (offset, block) in to_insert.into_iter().enumerate() {
            kawa.blocks.insert(insert_at + offset, block);
        }
    }
}

/// Copy a per-frontend response-edit slice into the per-stream
/// `HttpContext.headers_response` snapshot, applying `filter` to each
/// edit. The snapshot is cleared before the copy so a second pass on
/// the same context (the HSTS hoist + post-forward pattern in
/// `route_from_request`) overrides any earlier partial copy.
fn snapshot_response_edits<F>(target: &mut Vec<HeaderEditSnapshot>, src: &[HeaderEdit], filter: F)
where
    F: Fn(&HeaderEdit) -> bool,
{
    target.clear();
    for edit in src.iter().filter(|e| filter(e)) {
        target.push(HeaderEditSnapshot {
            key: edit.key.to_vec(),
            val: edit.val.to_vec(),
            mode: edit.mode,
        });
    }
}

/// Resolve the protocol scheme to use when emitting a redirect's `Location`
/// header. Maps the proto enum onto `"http"` / `"https"`, with `USE_SAME`
/// preserving the request's scheme (HTTPS for TLS listeners, HTTP otherwise).
fn resolve_redirect_scheme(scheme: RedirectScheme, context: &HttpContext) -> &'static str {
    match scheme {
        RedirectScheme::UseHttps => "https",
        RedirectScheme::UseHttp => "http",
        RedirectScheme::UseSame => {
            if context.tls_server_name.is_some() {
                "https"
            } else {
                "http"
            }
        }
    }
}

/// Build the `Location` URL for a redirect response. Defaults the port
/// suffix only when the operator provided one or when scheme defaults
/// would mismatch (port 80 on https / 443 on http stays implicit).
///
/// `host_override` and `path_override` carry the frontend's
/// `RewriteParts::run` output for `RedirectPolicy::PERMANENT` flows so
/// the 301 `Location` reflects `rewrite_host` / `rewrite_path` instead
/// of the original `:authority` / `:path`. The legacy
/// `cluster.https_redirect` path passes `None` for both — it has no
/// per-frontend rewrite knobs.
fn build_redirect_location(
    scheme: &str,
    context: &HttpContext,
    port: Option<u32>,
    host_override: Option<&str>,
    path_override: Option<&str>,
) -> String {
    let authority = host_override
        .or(context.authority.as_deref())
        .unwrap_or_default();
    let path = path_override.or(context.path.as_deref()).unwrap_or("/");
    // Strip an existing `:port` from the authority — operators typically
    // configure `https_redirect_port` precisely because the listener's
    // port differs from the redirect target. Bracketed IPv6 literals
    // like `[::1]` survive intact: `rsplit_once(':')` only triggers when
    // the suffix after the final `:` is entirely ASCII digits.
    let host_only = match authority.rsplit_once(':') {
        Some((host, port_part))
            if !port_part.is_empty() && port_part.bytes().all(|b| b.is_ascii_digit()) =>
        {
            host
        }
        _ => authority,
    };
    let port_suffix = match port {
        Some(80) if scheme == "http" => String::new(),
        Some(443) if scheme == "https" => String::new(),
        Some(p) => format!(":{p}"),
        None => String::new(),
    };
    format!("{scheme}://{host_only}{port_suffix}{path}")
}

/// Exact-match test between an HTTP `:authority` / `Host` value and a TLS SNI.
///
/// Matching rules:
///   * The authority is stripped of its optional `:port` suffix. RFC 6066 §3
///     forbids a port in the SNI extension, so the SNI is compared against
///     the host component only.
///   * The comparison is case-insensitive (RFC 9110 §4.2.3 — hosts are
///     case-insensitive). The SNI is assumed to be already lowercased by
///     the caller (see `https.rs::upgrade_handshake`); only the authority
///     side needs on-the-fly `to_ascii_lowercase`.
///   * No wildcard logic: if the operator serves a wildcard certificate,
///     the SNI negotiated by the client is still the specific name that
///     client sent, and the request `:authority` must equal that specific
///     name exactly. This is the tightest possible TLS trust boundary.
///
/// The `:port` suffix is only stripped when the suffix is non-empty and
/// entirely ASCII digits. This keeps bracketed IPv6 literals like `[::1]`
/// intact: `rsplit_once(':')` would otherwise mis-split them.
pub(crate) fn authority_matches_sni(authority: &str, sni_lowercased: &str) -> bool {
    let host = strip_authority_port(authority);
    if host.len() != sni_lowercased.len() {
        return false;
    }
    host.as_bytes()
        .iter()
        .zip(sni_lowercased.as_bytes())
        .all(|(a, b)| a.to_ascii_lowercase() == *b)
}

/// Strip the optional `:port` suffix from an authority value. Bracketed
/// IPv6 literals (`[::1]`, `[::1]:8443`) keep their inner colons intact:
/// the suffix is only stripped when the tail after the last `:` is
/// non-empty and entirely ASCII digits.
fn strip_authority_port(authority: &str) -> &str {
    match authority.rsplit_once(':') {
        Some((h, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => authority,
    }
}

/// RFC 6125 §6.4.3 wildcard-aware match of `:authority` against a SAN set
/// snapshot taken at TLS handshake.
///
/// Returns the matched SAN entry on success so the caller can log it.
///
/// Matching rules:
///   * Port suffix on the authority is stripped (same logic as
///     [`authority_matches_sni`], IPv6-bracket safe).
///   * Compare is ASCII case-insensitive (`:authority` is ASCII per
///     RFC 9113 §8.3.1; SAN entries are stored pre-lowercased by
///     `https.rs::upgrade_handshake`).
///   * `*.suffix` matches exactly one DNS label at the leftmost position
///     and only when that label is non-empty: it does NOT match the apex,
///     does NOT cross dots, and embedded wildcards (`foo.*.example.com`,
///     `*foo.example.com`) are forbidden.
///   * Empty `names` ⇒ `None` (default-cert path — Sōzu is not
///     authoritative for any name).
pub(crate) fn authority_matched_cert_name<'a>(
    authority: &str,
    names: &'a [String],
) -> Option<&'a str> {
    let mut host = strip_authority_port(authority);
    // RFC 1034 §3.1 absolute-form: `example.com.` and `example.com` name
    // the same host. The SAN snapshot already strips trailing dots at
    // `https.rs::upgrade_handshake`, and the SNI side strips them at the
    // same site; strip on the authority side so a client emitting
    // absolute-form `:authority` (or H1 `Host`) does not get a false 421.
    // Only one trailing dot is removed because RFC 1034 forbids multiple
    // trailing dots on a domain literal.
    if let Some(trimmed) = host.strip_suffix('.') {
        host = trimmed;
    }
    if host.is_empty() {
        return None;
    }
    for entry in names {
        if let Some(suffix) = entry.strip_prefix("*.") {
            // RFC 6125 §6.4.3: the wildcard label is the *entire* left-most
            // label. Embedded wildcards (`f*.example.com`, `*f.example.com`)
            // are rejected because we reach this branch only when the entry
            // starts with the exact two bytes `*.`. We still must reject
            // wildcards anywhere else in the entry by requiring no further
            // `*` in `suffix`.
            if suffix.contains('*') {
                continue;
            }
            // Authority has the form `<left-most-label>.<rest>`; the
            // wildcard substitutes for exactly that left-most label, which
            // must be non-empty and contain no dot.
            let Some((leftmost, rest)) = host.split_once('.') else {
                continue;
            };
            if leftmost.is_empty() {
                continue;
            }
            if rest.eq_ignore_ascii_case(suffix) {
                return Some(entry);
            }
            continue;
        }
        if entry.contains('*') {
            // Internal wildcards (`foo.*.example.com`) are not RFC 6125-
            // valid. Skip rather than mis-match.
            continue;
        }
        if host.eq_ignore_ascii_case(entry) {
            return Some(entry);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::BTreeMap, rc::Rc, time::Duration};

    use sozu_command::{
        config::ListenerBuilder,
        proto::command::{
            ForwardedHeaders, PathRule, RequestHttpFrontend, RulePosition, SocketAddress,
        },
    };

    use super::{Router, RoutingView, authority_matches_sni};
    use crate::{
        L7Proxy,
        http::HttpProxy,
        protocol::{
            http::{editor::HttpContext, parser::Method},
            mux::{buffer_source::PoolBufferSource, stream::Stream},
        },
    };

    /// A frontend declared as `*.example.com` and carrying `--tags` must put
    /// those tags on the access-log line of a request whose authority is
    /// `foo.example.com` (sozu#1379).
    ///
    /// The bug this pins: the tag cache was WRITTEN under the frontend
    /// RULE's hostname (`listener.set_tags(front.hostname, …)` in
    /// `lib/src/http.rs` / `lib/src/https.rs`) and READ back under the
    /// REQUEST's authority (`listener.get_tags(hostname)` in
    /// `lib/src/protocol/mux/stream.rs`), against a `BTreeMap<String,
    /// CachedTags>` that does exact-key lookup only. `*.example.com` is a
    /// key `foo.example.com` can never produce, so every access-log line of
    /// every wildcard, regex, ported or differently-cased frontend lost its
    /// tags silently. The fix resolves the tags through the router, which
    /// already knows which frontend rule matched.
    ///
    /// The test drives the production add path (`HttpProxy::add_http_frontend`),
    /// the production routing path (`Router::route_from_request`) and the
    /// production emit path (`Stream::generate_access_log`), and asserts on
    /// the rendered access-log line rather than on any intermediate field,
    /// so it stays valid whatever the resolution mechanism becomes.
    ///
    /// To SEE THIS RED: in `Router::route_from_request`
    /// (`lib/src/protocol/mux/router.rs`), replace the
    /// `context.tags = tags;` stash that follows the `RouteResult`
    /// destructure with `let _ = tags;`. The access log then falls back to
    /// the authority-keyed listener map, which holds only the
    /// `*.example.com` key, and the line is emitted with an empty `[]` tag
    /// field. Dropping `|| front.tags.is_some()` from `has_policy` in
    /// `Router::add_http_front_with_hsts_origin`
    /// (`lib/src/router/mod.rs`) reddens it the same way, one step
    /// earlier: the frontend is then stored as a tagless
    /// `Route::ClusterId` and there is nothing left for the stash to
    /// carry.
    #[test]
    fn a_wildcard_frontends_tags_reach_the_access_log_of_a_concrete_authority() {
        const TAG: &str = "owner=team-wildcard-1379";

        let output = crate::capture_test_logs(|| {
            let port = crate::testing::provide_port();
            let address = SocketAddress::new_v4(127, 0, 0, 1, port);
            let config = ListenerBuilder::new_http(address)
                .to_http(None)
                .expect("test http listener config must build");
            let parts = crate::testing::prebuild_server(10, 16_384, false)
                .expect("test server parts must build");
            let pool = parts.pool.clone();
            let mut proxy =
                HttpProxy::new(parts.registry, parts.sessions, parts.pool, parts.backends);
            let token = mio::Token(0);
            proxy
                .add_listener(config, token)
                .expect("test listener must register");
            proxy
                .add_http_frontend(RequestHttpFrontend {
                    cluster_id: Some("cluster-1379".to_owned()),
                    address,
                    hostname: "*.example.com".to_owned(),
                    path: PathRule::prefix("/".to_owned()),
                    position: RulePosition::Tree.into(),
                    tags: BTreeMap::from([("owner".to_owned(), "team-wildcard-1379".to_owned())]),
                    ..Default::default()
                })
                .expect("the wildcard frontend must register");
            let listener = proxy
                .get_listener(&token)
                .expect("the registered listener must be reachable");
            let proxy: Rc<RefCell<dyn L7Proxy>> = Rc::new(RefCell::new(proxy));

            let mut context = HttpContext::new(
                rusty_ulid::Ulid::generate(),
                rusty_ulid::Ulid::generate(),
                crate::Protocol::HTTP,
                "127.0.0.1:80"
                    .parse()
                    .expect("test public address must parse"),
                Some(
                    "127.0.0.1:12345"
                        .parse()
                        .expect("test session address must parse"),
                ),
                "SOZUBALANCEID".to_owned(),
                "Sozu-Id".to_owned(),
                false,
                false,
            );
            context.authority = Some("foo.example.com".to_owned());
            context.path = Some("/".to_owned());
            context.method = Some(Method::Get);

            let mut stream = Stream::new(
                &mut PoolBufferSource::new(Rc::downgrade(&pool)),
                context,
                crate::protocol::mux::test_support::test_answers(),
                65_535,
            )
            .expect("test stream must check out its buffers");
            let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));
            let (front, stream_context) = {
                let split = &mut stream;
                (&mut split.front, &mut split.context)
            };
            let proxy_ref = proxy.borrow();
            let view = RoutingView::new(proxy_ref.clusters(), proxy_ref.kind());
            router
                .route_from_request(stream_context, front, &listener, &view)
                .expect("the wildcard frontend must route foo.example.com");
            drop(proxy_ref);

            // This test asserts on the access log, not on metrics.
            let _ = stream.generate_access_log(false, None, listener, None, None);
        });

        assert!(
            output.contains(TAG),
            "the access log of a request routed by `*.example.com` must carry that frontend's tags, got: {output}"
        );
    }

    /// `HttpContext::original_authority` records the authority the client
    /// sent only when the frontend rewrites the host, the one case where it
    /// is sent on as `X-Forwarded-Host`; a request without a host rewrite
    /// leaves it `None` instead of paying a copy of the authority (#1589).
    ///
    /// To SEE THIS RED: stamp `context.original_authority` with a copy of
    /// `host` in `Router::route_from_request` before the frontend lookup,
    /// as it was before #1589. The plain request then carries
    /// `Some("plain.example.com")`.
    #[test]
    fn only_a_host_rewrite_records_the_original_authority() {
        let port = crate::testing::provide_port();
        let address = SocketAddress::new_v4(127, 0, 0, 1, port);
        let config = ListenerBuilder::new_http(address)
            .to_http(None)
            .expect("test http listener config must build");
        let parts = crate::testing::prebuild_server(10, 16_384, false)
            .expect("test server parts must build");
        let pool = parts.pool.clone();
        let mut proxy = HttpProxy::new(parts.registry, parts.sessions, parts.pool, parts.backends);
        let token = mio::Token(0);
        proxy
            .add_listener(config, token)
            .expect("test listener must register");
        for (hostname, rewrite_host) in [
            ("rewrite.example.com", Some("internal.example.com")),
            ("plain.example.com", None),
        ] {
            proxy
                .add_http_frontend(RequestHttpFrontend {
                    cluster_id: Some("cluster-1589".to_owned()),
                    address,
                    hostname: hostname.to_owned(),
                    path: PathRule::prefix("/".to_owned()),
                    position: RulePosition::Tree.into(),
                    rewrite_host: rewrite_host.map(str::to_owned),
                    ..Default::default()
                })
                .unwrap_or_else(|error| panic!("{hostname} must register: {error}"));
        }
        let listener = proxy
            .get_listener(&token)
            .expect("the registered listener must be reachable");
        let proxy: Rc<RefCell<dyn L7Proxy>> = Rc::new(RefCell::new(proxy));
        let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));

        for (authority, expected) in [
            ("rewrite.example.com", Some("rewrite.example.com")),
            ("plain.example.com", None),
        ] {
            let mut context = HttpContext::new(
                rusty_ulid::Ulid::generate(),
                rusty_ulid::Ulid::generate(),
                crate::Protocol::HTTP,
                "127.0.0.1:80"
                    .parse()
                    .expect("test public address must parse"),
                None,
                "SOZUBALANCEID".to_owned(),
                "Sozu-Id".to_owned(),
                false,
                false,
            );
            context.authority = Some(authority.to_owned());
            context.path = Some("/".to_owned());
            context.method = Some(Method::Get);
            let mut stream = Stream::new(
                &mut PoolBufferSource::new(Rc::downgrade(&pool)),
                context,
                crate::protocol::mux::test_support::test_answers(),
                65_535,
            )
            .expect("test stream must check out its buffers");
            let split = &mut stream;
            let proxy_ref = proxy.borrow();
            let view = RoutingView::new(proxy_ref.clusters(), proxy_ref.kind());
            router
                .route_from_request(&mut split.context, &mut split.front, &listener, &view)
                .unwrap_or_else(|error| panic!("{authority} must route: {error}"));

            assert_eq!(
                split.context.original_authority.as_deref(),
                expected,
                "{authority}: original_authority is recorded exactly when the host is rewritten",
            );
            let forwarded_host = split.front.blocks.iter().find_map(|block| match block {
                kawa::Block::Header(pair)
                    if pair.key.data(split.front.storage.buffer()) == b"X-Forwarded-Host" =>
                {
                    Some(pair.val.data(split.front.storage.buffer()).to_vec())
                }
                _ => None,
            });
            assert_eq!(
                forwarded_host.as_deref(),
                expected.map(str::as_bytes),
                "{authority}: X-Forwarded-Host carries the authority the client sent, on a rewrite only",
            );
        }
    }

    /// On a host rewrite, `X-Forwarded-Host` follows the listener's
    /// `forwarded_headers` mode: `both` and `x_forwarded` replace a client
    /// one with the authority the client sent, `rfc7239` and `none` inject
    /// none, and `none` leaves a client one as sent. (`rfc7239` elided the
    /// client one earlier, in `HttpContext::on_request_headers`.)
    ///
    /// To SEE THIS RED: in `apply_request_rewrites_and_headers`, set
    /// `synthesises_xfh` to `rewriting_host` alone. `rfc7239` and `none` then
    /// carry `X-Forwarded-Host: rewrite.example.com`.
    #[test]
    fn a_host_rewrite_discloses_x_forwarded_host_only_in_x_forwarded_modes() {
        let port = crate::testing::provide_port();
        let address = SocketAddress::new_v4(127, 0, 0, 1, port);
        let config = ListenerBuilder::new_http(address)
            .to_http(None)
            .expect("test http listener config must build");
        let parts = crate::testing::prebuild_server(10, 16_384, false)
            .expect("test server parts must build");
        let pool = parts.pool.clone();
        let mut proxy = HttpProxy::new(parts.registry, parts.sessions, parts.pool, parts.backends);
        let token = mio::Token(0);
        proxy
            .add_listener(config, token)
            .expect("test listener must register");
        proxy
            .add_http_frontend(RequestHttpFrontend {
                cluster_id: Some("cluster-322".to_owned()),
                address,
                hostname: "rewrite.example.com".to_owned(),
                path: PathRule::prefix("/".to_owned()),
                position: RulePosition::Tree.into(),
                rewrite_host: Some("internal.example.com".to_owned()),
                ..Default::default()
            })
            .expect("the rewriting frontend must register");
        let listener = proxy
            .get_listener(&token)
            .expect("the registered listener must be reachable");
        let proxy: Rc<RefCell<dyn L7Proxy>> = Rc::new(RefCell::new(proxy));
        let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));

        for (mode, client_xfh, expected) in [
            (
                ForwardedHeaders::Both,
                true,
                vec![b"rewrite.example.com".to_vec()],
            ),
            (
                ForwardedHeaders::XForwarded,
                true,
                vec![b"rewrite.example.com".to_vec()],
            ),
            (ForwardedHeaders::Rfc7239, false, vec![]),
            (
                ForwardedHeaders::None,
                true,
                vec![b"client.example".to_vec()],
            ),
        ] {
            let mut context = HttpContext::new(
                rusty_ulid::Ulid::generate(),
                rusty_ulid::Ulid::generate(),
                crate::Protocol::HTTP,
                "127.0.0.1:80"
                    .parse()
                    .expect("test public address must parse"),
                None,
                "SOZUBALANCEID".to_owned(),
                "Sozu-Id".to_owned(),
                false,
                false,
            );
            context.forwarded_headers = mode;
            context.authority = Some("rewrite.example.com".to_owned());
            context.path = Some("/".to_owned());
            context.method = Some(Method::Get);
            let mut stream = Stream::new(
                &mut PoolBufferSource::new(Rc::downgrade(&pool)),
                context,
                crate::protocol::mux::test_support::test_answers(),
                65_535,
            )
            .expect("test stream must check out its buffers");
            if client_xfh {
                stream.front.push_block(kawa::Block::Header(kawa::Pair {
                    key: kawa::Store::Static(b"X-Forwarded-Host"),
                    val: kawa::Store::Static(b"client.example"),
                }));
            }
            let split = &mut stream;
            let proxy_ref = proxy.borrow();
            let view = RoutingView::new(proxy_ref.clusters(), proxy_ref.kind());
            router
                .route_from_request(&mut split.context, &mut split.front, &listener, &view)
                .unwrap_or_else(|error| panic!("{mode:?} must route: {error}"));

            let buffer = split.front.storage.buffer();
            let forwarded_hosts: Vec<Vec<u8>> = split
                .front
                .blocks
                .iter()
                .filter_map(|block| match block {
                    kawa::Block::Header(pair)
                        if !pair.is_elided()
                            && pair
                                .key
                                .data(buffer)
                                .eq_ignore_ascii_case(b"X-Forwarded-Host") =>
                    {
                        Some(pair.val.data(buffer).to_vec())
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(
                forwarded_hosts, expected,
                "{mode:?}: X-Forwarded-Host after a host rewrite"
            );
        }
    }

    // Asserts on `debug!` output: gate on the same condition that compiles it in.
    #[cfg(any(debug_assertions, feature = "logs-debug", feature = "logs-trace"))]
    #[test]
    fn routing_runtime_logs_bound_method_authority_sni_and_certificate_names() {
        use super::{log_coalescing_accepted, log_sni_authority_mismatch};

        const METHOD_SECRET: &str = "MUX_ROUTE_METHOD_SECRET_SENTINEL";
        const AUTHORITY_SECRET: &str = "MUX_ROUTE_AUTHORITY_SECRET_SENTINEL";
        const SNI_SECRET: &str = "MUX_ROUTE_SNI_SECRET_SENTINEL";
        const SAN_SECRET: &str = "MUX_ROUTE_SAN_SECRET_SENTINEL";

        let long_value = |marker: &str| format!("{marker}{}", "x".repeat(4096));
        let method = long_value(METHOD_SECRET);
        let authority = long_value(AUTHORITY_SECRET);
        let sni = long_value(SNI_SECRET);
        let san = long_value(SAN_SECRET);
        let method_len = method.len();
        let authority_len = authority.len();
        let sni_len = sni.len();
        let san_len = san.len();

        let output = crate::capture_test_logs_at_level("debug", move || {
            let mut context = HttpContext::new(
                rusty_ulid::Ulid::generate(),
                rusty_ulid::Ulid::generate(),
                crate::Protocol::HTTPS,
                "127.0.0.1:443"
                    .parse()
                    .expect("test public address must parse"),
                Some(
                    "127.0.0.1:12345"
                        .parse()
                        .expect("test session address must parse"),
                ),
                String::new(),
                String::new(),
                false,
                false,
            );
            context.method = Some(crate::protocol::http::parser::Method::Custom(method));
            context.authority = Some(authority.clone());
            context.tls_cert_names = Some(std::sync::Arc::new(vec![san.clone()]));

            log_coalescing_accepted(&context, &authority, &sni, &san);
            log_sni_authority_mismatch(
                &context,
                &authority,
                &sni,
                context.tls_cert_names.as_deref().map(Vec::as_slice),
            );
        });

        for secret in [METHOD_SECRET, AUTHORITY_SECRET, SNI_SECRET, SAN_SECRET] {
            assert!(
                !output.contains(secret),
                "mux routing runtime log leaked {secret}: {output}"
            );
        }
        for metadata in [
            format!("bytes={method_len}"),
            format!("authority_bytes=Some({authority_len})"),
            format!("authority_bytes={authority_len}"),
            format!("sni_bytes={sni_len}"),
            format!("matched_san_bytes={san_len}"),
            "certificate_sans_count=1".to_owned(),
            format!("certificate_sans_bytes={san_len}"),
        ] {
            assert!(
                output.contains(&metadata),
                "mux routing runtime log omitted {metadata}: {output}"
            );
        }
        assert!(
            output.len() <= 2048,
            "mux routing runtime capture is not bounded: {} bytes",
            output.len()
        );
    }

    #[test]
    fn match_exact() {
        assert!(authority_matches_sni("example.com", "example.com"));
    }

    #[test]
    fn match_different_case() {
        assert!(authority_matches_sni("Example.COM", "example.com"));
    }

    #[test]
    fn match_authority_with_port() {
        assert!(authority_matches_sni("example.com:8443", "example.com"));
    }

    #[test]
    fn reject_different_host() {
        assert!(!authority_matches_sni(
            "tenant-b.example.com",
            "tenant-a.example.com"
        ));
    }

    #[test]
    fn reject_substring_attack() {
        // Length check guards against an authority that is a prefix or
        // suffix of the SNI (or vice versa).
        assert!(!authority_matches_sni("example.co", "example.com"));
        assert!(!authority_matches_sni("example.commons", "example.com"));
    }

    #[test]
    fn reject_wildcard_not_expanded() {
        // Wildcard cert selection happens at the cert-resolver layer; the SNI
        // we see here is the concrete name the client sent. Do not silently
        // accept `*.example.com` as matching `foo.example.com`.
        assert!(!authority_matches_sni("foo.example.com", "*.example.com"));
    }

    #[test]
    fn ipv6_bracketed_literal_with_port() {
        // `[::1]:8443` must still match the SNI `[::1]`; only the trailing
        // `:8443` is a port (all digits → stripped).
        assert!(authority_matches_sni("[::1]:8443", "[::1]"));
    }

    #[test]
    fn ipv6_bracketed_without_port() {
        // The `:` characters inside the brackets must not be mistaken for a
        // port separator: the tail after the last `:` is `1]`, not all
        // digits, so it is NOT stripped and the whole string compares.
        assert!(authority_matches_sni("[::1]", "[::1]"));
    }
}

#[cfg(test)]
mod authority_matched_cert_name_tests {
    use super::authority_matched_cert_name;

    #[test]
    fn cert_name_match_exact_single_san() {
        let names = vec!["example.com".to_owned()];
        assert_eq!(
            authority_matched_cert_name("example.com", &names),
            Some("example.com"),
        );
    }

    #[test]
    fn cert_name_match_wildcard_left_most() {
        let names = vec!["*.cleverapps.io".to_owned()];
        assert_eq!(
            authority_matched_cert_name("staging-3.cleverapps.io", &names),
            Some("*.cleverapps.io"),
        );
    }

    #[test]
    fn cert_name_reject_wildcard_apex() {
        // RFC 6125 §6.4.3: `*.example.com` does NOT cover the apex
        // `example.com` — the wildcard label must consume exactly one
        // non-empty label.
        let names = vec!["*.example.com".to_owned()];
        assert_eq!(authority_matched_cert_name("example.com", &names), None);
    }

    #[test]
    fn cert_name_reject_wildcard_two_labels() {
        // `*.example.com` cannot cross dots: `a.b.example.com` has two
        // labels before `example.com` and must be rejected.
        let names = vec!["*.example.com".to_owned()];
        assert_eq!(authority_matched_cert_name("a.b.example.com", &names), None,);
    }

    #[test]
    fn cert_name_reject_wildcard_not_left_most() {
        // Embedded wildcards (`foo.*.example.com`) are not RFC 6125-valid
        // and must be skipped, not mis-matched.
        let names = vec!["foo.*.example.com".to_owned()];
        assert_eq!(
            authority_matched_cert_name("foo.bar.example.com", &names),
            None,
        );
    }

    #[test]
    fn cert_name_match_case_insensitive() {
        // ASCII case folding only — `:authority` is ASCII per RFC 9113
        // §8.3.1 and the snapshot is pre-lowercased at handshake.
        let names = vec!["EXAMPLE.com".to_owned()];
        assert!(authority_matched_cert_name("Example.COM", &names).is_some());
    }

    #[test]
    fn cert_name_match_with_port() {
        // The port suffix on `:authority` must be stripped before the
        // SAN compare.
        let names = vec!["example.com".to_owned()];
        assert!(authority_matched_cert_name("example.com:8443", &names).is_some());
    }

    #[test]
    fn cert_name_match_absolute_form_trailing_dot() {
        // RFC 1034 §3.1: an absolute-form domain literal carries one
        // trailing dot (`example.com.`) and resolves to the same host as
        // the relative form. The SAN snapshot stores the relative form
        // (https.rs strips the trailing dot at handshake), so the matcher
        // must strip it on the authority side too — otherwise a client
        // emitting an absolute-form `:authority` gets a false 421.
        let names = vec!["example.com".to_owned()];
        assert!(authority_matched_cert_name("example.com.", &names).is_some());
        // And with both port and trailing dot.
        assert!(authority_matched_cert_name("example.com.:8443", &names).is_some());
        // The wildcard branch must also accept the absolute form.
        let wildcard = vec!["*.example.com".to_owned()];
        assert!(authority_matched_cert_name("foo.example.com.", &wildcard).is_some());
    }

    #[test]
    fn cert_name_match_idn_a_label() {
        // IDNA A-labels (xn--…) are ASCII and compare byte-for-byte once
        // the snapshot is lowercased.
        let names = vec!["xn--bcher-kva.example.com".to_owned()];
        assert!(authority_matched_cert_name("xn--bcher-kva.example.com", &names).is_some());
    }

    #[test]
    fn cert_name_reject_empty_names() {
        // Empty snapshot = default cert served = Sōzu is not
        // authoritative for any name; every authority must miss.
        assert_eq!(authority_matched_cert_name("example.com", &[]), None);
    }

    #[test]
    fn cert_name_match_multi_san_one_hit() {
        let names = vec!["foo.com".to_owned(), "*.example.org".to_owned()];
        assert_eq!(
            authority_matched_cert_name("bar.example.org", &names),
            Some("*.example.org"),
        );
    }

    #[test]
    fn cert_name_reject_substring_attack() {
        // `*.example.com` must not match `example.commons` — the suffix
        // after the first label is `commons`, not `example.com`.
        let names = vec!["*.example.com".to_owned()];
        assert_eq!(authority_matched_cert_name("example.commons", &names), None,);
    }

    #[test]
    fn cert_name_ipv6_bracketed_literal_with_port() {
        // The `:` characters inside the brackets must not be mistaken for
        // a port separator: only the trailing `:8443` is stripped, and
        // `[::1]` compares equal to `[::1]`.
        let names = vec!["[::1]".to_owned()];
        assert!(authority_matched_cert_name("[::1]:8443", &names).is_some());
    }
}

/// Backend-selection order: `Router::plan_connect` must resolve a tie the same way
/// on every process, for the identical set of backend connections and the
/// identical request (issue #1338).
///
/// **Why these assert a SPECIFIC token rather than "the same one twice".**
/// `RandomState` is seeded per `HashMap`, not per iteration: two walks of one
/// live `HashMap` agree, so a test that runs the selection twice inside one
/// process and compares passes against a `HashMap` by construction. What does
/// NOT agree is two SEPARATELY CONSTRUCTED maps — `RandomState::default()`
/// bumps a thread-local key on every instantiation — so each round below
/// builds a fresh [`Router`], and therefore a fresh hasher, and asserts the
/// token the total order on `Token` picks.
///
/// That makes the red statistical rather than structural, and the bound is
/// stated so it can be checked rather than trusted: with `n` staged backends
/// and `r` rounds a `HashMap` survives with probability about `n.pow(-r)`,
/// which is `4^-24` here — about one run in 2.8e14. A `BTreeMap` survives
/// with probability 1. Measured on the pre-image, with the three production
/// lines reverted and these guards untouched: three consecutive runs failed
/// all three at round 0 or round 1. The least-loaded guard was handed
/// `Token(23)` and `Token(13)` where the total order gives `Token(7)`, and
/// the last-wins fallback `Token(7)` where it gives `Token(41)`.
///
/// **To SEE THESE RED:** in this module, put `Router::backends` back to
/// `HashMap<Token, Connection<SessionTcpStream>>`, `Router::new`'s initialiser
/// back to `HashMap::new()` and the import back to `collections::HashMap`
/// (the `BackendConnections` type then has no user left; delete it).
/// Reverting the file wholesale deletes these tests instead of reddening them.
#[cfg(test)]
mod backend_selection_order_tests {
    use std::{cell::RefCell, collections::HashMap, net::SocketAddr, rc::Rc, time::Duration};

    use mio::Token;
    use rusty_ulid::Ulid;
    use sozu_command::{
        config::ListenerBuilder,
        proto::command::{
            Cluster, ListenerType, PathRule, RequestHttpFrontend, RulePosition, SocketAddress,
        },
    };

    use super::Router;
    use crate::{
        BackendConnectionError, L7Proxy,
        backends::Backend,
        http::{HttpListener, HttpProxy},
        pool::Pool,
        protocol::{
            http::parser::Method,
            mux::{
                BackendId, BackendRegistry, BackendStatus, Connection, Context, Position,
                StreamState,
                buffer_source::PoolBufferSource,
                h2::H2ConnectionConfig,
                h2_flood_detector::H2FloodConfig,
                router::{ConnectPlan, ConnectStep, IpGateVerdict, RoutingView},
            },
        },
        socket::SessionTcpStream,
    };

    /// Unwrap a step these fixtures can only produce one way.
    ///
    /// Every `Context` here is built with `session_address: None`, so the
    /// per-(cluster, source-IP) gate is skipped and `Router::plan_connect`
    /// must decide outright. A `CheckIpLimit` would mean the gate fired
    /// without a source IP, which is its own bug.
    fn decided(step: ConnectStep) -> ConnectPlan {
        match step {
            ConnectStep::Decided(plan) => plan,
            ConnectStep::CheckIpLimit(_) => panic!(
                "a stream with no session address must not reach the \
                 per-(cluster, source-IP) gate"
            ),
        }
    }

    /// Staged deliberately out of order and non-contiguous, so a green run
    /// cannot be an artefact of insertion order or of a dense key space.
    const STAGED_TOKENS: [usize; 4] = [23, 7, 41, 13];
    const LOWEST_TOKEN: usize = 7;
    const HIGHEST_TOKEN: usize = 41;
    /// See the module-level note for the `n.pow(-r)` bound this feeds.
    const ROUNDS: usize = 24;

    const H2_CLUSTER: &str = "cluster-1338-h2";
    const H1_CLUSTER: &str = "cluster-1338-h1";
    const H2_AUTHORITY: &str = "h2.backend-order.example.com";
    const H1_AUTHORITY: &str = "h1.backend-order.example.com";

    /// Which arm of `Router::plan_connect`'s scan the staged backends land in.
    enum Staged {
        /// `Position::Client(_, _, BackendStatus::Connected)` on an H2
        /// connection — the least-loaded arm, strict `<`, first-at-minimum.
        ConnectedH2,
        /// `BackendStatus::Connecting` on an H2 connection — the fallback
        /// arm, assigned with no `break` and no "already chosen" guard.
        ConnectingH2,
        /// `BackendStatus::KeepAlive` on an H1 connection — assigned with a
        /// `break`, so first-seen.
        KeepAliveH1,
    }

    impl Staged {
        fn cluster(&self) -> &'static str {
            match self {
                Staged::ConnectedH2 | Staged::ConnectingH2 => H2_CLUSTER,
                Staged::KeepAliveH1 => H1_CLUSTER,
            }
        }

        fn authority(&self) -> &'static str {
            match self {
                Staged::ConnectedH2 | Staged::ConnectingH2 => H2_AUTHORITY,
                Staged::KeepAliveH1 => H1_AUTHORITY,
            }
        }
    }

    /// The proxy, listener, clusters, frontends and buffer pool
    /// `Router::plan_connect` needs. Built once and shared by every round:
    /// the order under test belongs to `Router::backends`, which each round
    /// rebuilds from scratch.
    struct RoutingFixture {
        proxy: Rc<RefCell<dyn L7Proxy>>,
        /// The map `proxy` was built over, which a `Mux` receives at
        /// construction.
        backends: Rc<RefCell<crate::backends::BackendMap>>,
        listener: Rc<RefCell<HttpListener>>,
        pool: Rc<RefCell<Pool>>,
    }

    fn routing_fixture() -> RoutingFixture {
        let port = crate::testing::provide_port();
        let address = SocketAddress::new_v4(127, 0, 0, 1, port);
        let config = ListenerBuilder::new_http(address)
            .to_http(None)
            .expect("test http listener config must build");
        let parts =
            crate::testing::prebuild_server(32, 65_536, false).expect("test server must build");
        let pool = parts.pool.clone();
        let backends = Rc::clone(&parts.backends);
        let mut proxy = HttpProxy::new(parts.registry, parts.sessions, parts.pool, parts.backends);
        let listener_token = Token(0);
        proxy
            .add_listener(config, listener_token)
            .expect("test listener must register");
        for (cluster_id, authority, http2) in [
            (H2_CLUSTER, H2_AUTHORITY, Some(true)),
            (H1_CLUSTER, H1_AUTHORITY, None),
        ] {
            proxy
                .add_cluster(Cluster {
                    cluster_id: cluster_id.to_owned(),
                    http2,
                    ..Default::default()
                })
                .expect("the test cluster must register");
            proxy
                .add_http_frontend(RequestHttpFrontend {
                    cluster_id: Some(cluster_id.to_owned()),
                    address,
                    hostname: authority.to_owned(),
                    path: PathRule::prefix("/".to_owned()),
                    position: RulePosition::Tree.into(),
                    ..Default::default()
                })
                .expect("the test frontend must register");
        }
        let listener = proxy
            .get_listener(&listener_token)
            .expect("the registered listener must be reachable");
        RoutingFixture {
            proxy: Rc::new(RefCell::new(proxy)),
            backends,
            listener,
            pool,
        }
    }

    /// One backend connection in the requested arm, plus the accepted peer the
    /// caller must keep alive for the length of the round.
    fn staged_backend(
        pool: &Rc<RefCell<Pool>>,
        staged: &Staged,
        backend_registry: &mut BackendRegistry,
    ) -> (Connection<SessionTcpStream>, std::net::TcpStream) {
        let (socket, peer) = super::super::test_support::connected_socket();
        let backend_address: SocketAddr =
            "127.0.0.1:2".parse().expect("backend address must parse");
        let backend = Rc::new(RefCell::new(Backend::new(
            "test-backend",
            backend_address,
            None,
            None,
            None,
        )));
        // Through the embedder's table, exactly as a real dial does: the
        // handle stops here and the connection carries the opaque id.
        let backend = backend_registry.id_for(&backend);
        let session_ulid = Ulid::generate();
        let socket = SessionTcpStream::new(socket, session_ulid, Some(backend_address));
        let mut connection = match staged {
            Staged::ConnectedH2 | Staged::ConnectingH2 => Connection::new_h2_client(
                session_ulid,
                socket,
                staged.cluster().into(),
                backend,
                &mut PoolBufferSource::new(Rc::downgrade(pool)),
                Duration::from_secs(30),
                H2FloodConfig::default(),
                H2ConnectionConfig::default(),
                Duration::from_secs(30),
                None,
            )
            .expect("the test pool must hand out a buffer for the H2 backend"),
            Staged::KeepAliveH1 => Connection::new_h1_client(
                session_ulid,
                socket,
                staged.cluster().into(),
                backend,
                Duration::from_secs(30),
            ),
        };
        // `new_h*_client` opens in `Connecting`; the other two arms are
        // reached by moving the status the way a completed dial does.
        if let Position::Client(_, _, status) = connection.position_mut() {
            match staged {
                Staged::ConnectedH2 => *status = BackendStatus::Connected,
                Staged::ConnectingH2 => {}
                Staged::KeepAliveH1 => *status = BackendStatus::KeepAlive,
            }
        }
        (connection, peer)
    }

    /// Stage [`STAGED_TOKENS`] into a FRESH [`Router`] — a fresh `RandomState`
    /// in the pre-image — drive one `Router::plan_connect`, and report the token it
    /// attached the stream to.
    fn selected_backend_token(fixture: &RoutingFixture, staged: &Staged) -> Token {
        let pool = &fixture.pool;
        let mut context = Context::new(
            Ulid::generate(),
            Rc::downgrade(pool),
            fixture.listener.clone(),
            // No session address: the per-(cluster, source-IP) gate is
            // skipped, so this never reaches the proxy's session manager.
            None,
            "127.0.0.1:80"
                .parse()
                .expect("test public address must parse"),
        );
        let stream_id = context
            .create_stream(Ulid::generate(), 65_535)
            .expect("the test pool must hand out a stream");
        {
            let stream = &mut context.streams[stream_id];
            stream.state = StreamState::Link;
            stream.context.authority = Some(staged.authority().to_owned());
            stream.context.path = Some("/".to_owned());
            stream.context.method = Some(Method::Get);
        }

        let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));
        let mut backend_registry = BackendRegistry::default();
        let mut peers = Vec::with_capacity(STAGED_TOKENS.len());
        for token in STAGED_TOKENS {
            let (connection, peer) = staged_backend(pool, staged, &mut backend_registry);
            peers.push(peer);
            router.backends.insert(Token(token), connection);
        }

        // Every staged backend is reusable, so the plan must be `Attached`:
        // the router completed the attach itself and asked the embedder for
        // nothing. A `Dial` here would mean the reuse scan missed, which is
        // the failure this harness exists to catch.
        let proxy_ref = fixture.proxy.borrow();
        let view = RoutingView::new(proxy_ref.clusters(), proxy_ref.kind());
        let plan = router
            .plan_connect(stream_id, &mut context, &view)
            .map(decided)
            .expect("the router must reuse one of the staged backends");
        assert!(
            matches!(plan, ConnectPlan::Attached),
            "the staged backends are all reusable, so no dial may be requested: got {plan:?}"
        );

        match context.streams[stream_id].state {
            StreamState::Linked(token) => token,
            other => panic!("connect must link the stream to a backend, got {other:?}"),
        }
    }

    /// Drive one `Router::plan_connect` against a single staged connection of
    /// the `staged` arm, whose backend is retired when `retired` is set, and
    /// return the plan.
    fn plan_against_one_staged(
        fixture: &RoutingFixture,
        staged: &Staged,
        retired: bool,
    ) -> (ConnectPlan, StreamState) {
        let mut context = Context::new(
            Ulid::generate(),
            Rc::downgrade(&fixture.pool),
            fixture.listener.clone(),
            None,
            "127.0.0.1:80"
                .parse()
                .expect("test public address must parse"),
        );
        let stream_id = context
            .create_stream(Ulid::generate(), 65_535)
            .expect("the test pool must hand out a stream");
        {
            let stream = &mut context.streams[stream_id];
            stream.state = StreamState::Link;
            stream.context.authority = Some(staged.authority().to_owned());
            stream.context.path = Some("/".to_owned());
            stream.context.method = Some(Method::Get);
        }
        let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));
        let mut backend_registry = BackendRegistry::default();
        let (connection, _peer) = staged_backend(&fixture.pool, staged, &mut backend_registry);
        if retired {
            // What `BackendList::remove_backend` and `BackendMap::remove_cluster`
            // do to the handle the session's registry still holds.
            let Position::Client(_, backend, _) = connection.position() else {
                unreachable!("a staged backend is a client connection")
            };
            backend_registry
                .handle(backend)
                .expect("the staged backend was interned")
                .borrow_mut()
                .set_closing();
        }
        router.backends.insert(Token(LOWEST_TOKEN), connection);
        let is_retired = |backend: &BackendId| backend_registry.is_retired(backend);
        let proxy_ref = fixture.proxy.borrow();
        let view = RoutingView::new(proxy_ref.clusters(), proxy_ref.kind())
            .with_backend_retired(&is_retired);
        let plan = router
            .plan_connect(stream_id, &mut context, &view)
            .map(decided)
            .expect("routing must resolve the staged cluster");
        (plan, context.streams[stream_id].state)
    }

    /// The pool-reuse scan skips a connection whose backend has left the
    /// configuration, in each of its three reusable arms: H2 `Connected`, H2
    /// `Connecting` and H1 `KeepAlive`. The same connection with a live
    /// backend is reused, so a `Dial` here is the skip and not a scan that
    /// would have missed anyway.
    #[test]
    fn a_retired_backend_connection_is_never_reused() {
        let fixture = routing_fixture();
        for (staged, arm) in [
            (Staged::ConnectedH2, "H2 Connected"),
            (Staged::ConnectingH2, "H2 Connecting"),
            (Staged::KeepAliveH1, "H1 KeepAlive"),
        ] {
            let (plan, state) = plan_against_one_staged(&fixture, &staged, false);
            assert!(
                matches!(plan, ConnectPlan::Attached),
                "premise, {arm}: a connection to a live backend is reused, got {plan:?}"
            );
            assert_eq!(state, StreamState::Linked(Token(LOWEST_TOKEN)));

            let (plan, state) = plan_against_one_staged(&fixture, &staged, true);
            assert!(
                matches!(&plan, ConnectPlan::Dial { cluster_id, .. } if &**cluster_id == staged.cluster()),
                "{arm}: a connection to a retired backend must not be reused, got {plan:?}"
            );
            assert_eq!(
                state,
                StreamState::Link,
                "{arm}: the stream must not be linked to the retired connection"
            );
        }
    }

    /// `Mux::ready_inner` closes a retired connection only once it is idle:
    /// an H1 keep-alive, or a connected H2 connection with no stream. A dial
    /// still in progress is left to its own connect path.
    #[test]
    fn idle_pooled_backend_names_only_streamless_pooled_connections() {
        let fixture = routing_fixture();
        for (staged, arm, idle) in [
            (Staged::ConnectedH2, "H2 Connected", true),
            (Staged::ConnectingH2, "H2 Connecting", false),
            (Staged::KeepAliveH1, "H1 KeepAlive", true),
        ] {
            let mut backend_registry = BackendRegistry::default();
            let (connection, _peer) = staged_backend(&fixture.pool, &staged, &mut backend_registry);
            assert_eq!(
                connection.idle_pooled_backend().is_some(),
                idle,
                "{arm}: idle_pooled_backend must answer {idle}"
            );
        }
    }

    /// An admitted stream must have been **counted** before the decision
    /// resumes — the ordering the gate-as-a-step split has to preserve.
    ///
    /// `SessionManager::track_cluster_ip` mutates
    /// `connections_per_cluster_ip`, which is keyed on `(cluster, ip)`
    /// **across every session on the worker** and read again by
    /// `lib/src/tcp.rs`'s own gate. So an `IpGateVerdict::Admitted` is an
    /// assertion that the slot has already been taken. Resume as `Admitted`
    /// without having tracked and the stream goes through uncounted, and the
    /// next session from the same IP finds a slot that should be gone.
    ///
    /// The test drives the real production sequence — `Router::plan_connect`
    /// → `super::consult_ip_gate` → `Router::plan_connect_resume` — for two
    /// DIFFERENT frontend tokens from the SAME source IP, against a limit of
    /// one. Two tokens, not one: the gate is deliberately idempotent within a
    /// token (an H2 session multiplexing many streams to one cluster holds a
    /// single slot), so a second stream on the same token could never trip
    /// the limit and would prove nothing.
    ///
    /// To SEE THIS RED: delete the `track_cluster_ip` call from
    /// `super::consult_ip_gate`, leaving it to return
    /// `IpGateVerdict::Admitted` without counting. The second token is then
    /// admitted too and the `AtLimit` assertion below fires.
    #[test]
    fn an_admitted_stream_is_counted_before_the_decision_resumes() {
        let fixture = routing_fixture();
        let sessions = fixture.proxy.borrow().sessions();
        let source: SocketAddr = "203.0.113.7:51000"
            .parse()
            .expect("test source address must parse");

        // One connection per (cluster, ip).
        let mut clusters = HashMap::new();
        clusters.insert(
            H2_CLUSTER.into(),
            Cluster {
                cluster_id: H2_CLUSTER.to_owned(),
                max_connections_per_ip: Some(1),
                ..Default::default()
            },
        );
        let view = RoutingView::new(&clusters, ListenerType::Http);

        // Drive one stream all the way through the three-step sequence and
        // report what the gate said and what the decision became.
        let run = |token: Token| {
            let mut context = Context::new(
                Ulid::generate(),
                Rc::downgrade(&fixture.pool),
                fixture.listener.clone(),
                Some(source),
                "127.0.0.1:80"
                    .parse()
                    .expect("test public address must parse"),
            );
            let stream_id = context
                .create_stream(Ulid::generate(), 65_535)
                .expect("the test pool must hand out a stream");
            {
                let stream = &mut context.streams[stream_id];
                stream.state = StreamState::Link;
                stream.context.authority = Some(H2_AUTHORITY.to_owned());
                stream.context.path = Some("/".to_owned());
                stream.context.method = Some(Method::Get);
            }
            let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));
            let step = router
                .plan_connect(stream_id, &mut context, &view)
                .expect("routing must resolve the staged cluster");
            let ConnectStep::CheckIpLimit(resume) = step else {
                panic!("a stream WITH a session address must reach the per-IP gate")
            };
            let verdict = super::super::consult_ip_gate(
                &sessions,
                token,
                context
                    .http_context(stream_id)
                    .cluster_id
                    .as_ref()
                    .expect("plan_connect stamps the routed cluster"),
                &resume,
            );
            let admitted = matches!(verdict, IpGateVerdict::Admitted);
            let outcome =
                router.plan_connect_resume(stream_id, &mut context, &view, resume, verdict);
            let retry_after = context.streams[stream_id].context.retry_after_seconds;
            (admitted, outcome, retry_after)
        };

        let (admitted, outcome, _) = run(Token(1));
        assert!(
            admitted,
            "premise: the first token must be under the limit, or the second proves nothing"
        );
        assert!(
            matches!(outcome, Ok(ConnectPlan::Dial { .. })),
            "an admitted stream must go on to request a dial, got {outcome:?}"
        );

        let (admitted, outcome, retry_after) = run(Token(2));
        assert!(
            !admitted,
            "a SECOND token from the same source IP must find the slot already counted — \
             an Admitted verdict here means the first stream resumed without being tracked"
        );
        assert!(
            matches!(
                outcome,
                Err(BackendConnectionError::TooManyConnectionsPerIp { .. })
            ),
            "the refused stream must surface TooManyConnectionsPerIp, got {outcome:?}"
        );
        assert_eq!(
            retry_after, None,
            "with no cluster or global override the resolved Retry-After is 0, which is \
             stashed as None so the 429 mapping elides the header rather than sending 0"
        );
    }

    /// A source holding a whole subnet defeats the per-(cluster, source-IP)
    /// cap outright: it takes a fresh address for every connection, so the
    /// per-address counter never reaches 2. The per-(cluster,
    /// source-SUBNET) cap is the answer, and this pins that it fires on
    /// DISTINCT addresses drawn from one masked network
    /// (sozu-proxy/sozu#1270).
    ///
    /// Three tokens, three different `203.0.113.x` addresses — every one a
    /// distinct `/32`, all inside `203.0.113.0/24`. The per-IP cap is set
    /// to 10, comfortably above the 1 connection each address holds, so it
    /// cannot be what refuses the fourth: only the subnet counter can.
    /// That is the whole point of the two caps being independent.
    ///
    /// To SEE THIS RED: delete the `|| self.cluster_subnet_at_limit(...)`
    /// disjunct from `SessionManager::cluster_connection_at_limit` in
    /// `lib/src/server.rs`, leaving every other piece of the feature —
    /// config keys, proto fields, masking, tracking — in place. The fourth
    /// address is then admitted and the `AtLimit` assertion below fires.
    #[test]
    fn a_subnet_cap_refuses_a_fresh_address_from_an_already_counted_subnet() {
        let fixture = routing_fixture();
        let sessions = fixture.proxy.borrow().sessions();
        {
            let mut manager = sessions.borrow_mut();
            // /24, so 203.0.113.* all mask to one key.
            manager.subnet_ipv4_prefix = 24;
            manager.max_connections_per_subnet = 3;
            // Deliberately far above what any single address will hold.
            manager.max_connections_per_ip = 10;
        }

        let mut clusters = HashMap::new();
        clusters.insert(
            H2_CLUSTER.into(),
            Cluster {
                cluster_id: H2_CLUSTER.to_owned(),
                ..Default::default()
            },
        );
        let view = RoutingView::new(&clusters, ListenerType::Http);

        // Drive one stream, from its own source address on its own token,
        // through the real three-step production sequence.
        let run = |token: Token, source: SocketAddr| {
            let mut context = Context::new(
                Ulid::generate(),
                Rc::downgrade(&fixture.pool),
                fixture.listener.clone(),
                Some(source),
                "127.0.0.1:80"
                    .parse()
                    .expect("test public address must parse"),
            );
            let stream_id = context
                .create_stream(Ulid::generate(), 65_535)
                .expect("the test pool must hand out a stream");
            {
                let stream = &mut context.streams[stream_id];
                stream.state = StreamState::Link;
                stream.context.authority = Some(H2_AUTHORITY.to_owned());
                stream.context.path = Some("/".to_owned());
                stream.context.method = Some(Method::Get);
            }
            let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));
            let step = router
                .plan_connect(stream_id, &mut context, &view)
                .expect("routing must resolve the staged cluster");
            let ConnectStep::CheckIpLimit(resume) = step else {
                panic!("a stream WITH a session address must reach the connection gate")
            };
            let verdict = super::super::consult_ip_gate(
                &sessions,
                token,
                context
                    .http_context(stream_id)
                    .cluster_id
                    .as_ref()
                    .expect("plan_connect stamps the routed cluster"),
                &resume,
            );
            let admitted = matches!(verdict, IpGateVerdict::Admitted);
            let outcome =
                router.plan_connect_resume(stream_id, &mut context, &view, resume, verdict);
            (admitted, outcome)
        };

        let address = |host: u8| -> SocketAddr {
            format!("203.0.113.{host}:51000")
                .parse()
                .expect("test source address must parse")
        };

        // Premise: three DISTINCT addresses inside one /24 fill the cap.
        for (index, host) in [1u8, 2, 3].into_iter().enumerate() {
            let (admitted, outcome) = run(Token(index + 1), address(host));
            assert!(
                admitted,
                "premise: 203.0.113.{host} is the {} of 3 allowed in the subnet and must be \
                 admitted, or the refusal below proves nothing",
                index + 1
            );
            assert!(
                matches!(outcome, Ok(ConnectPlan::Dial { .. })),
                "an admitted stream must go on to request a dial, got {outcome:?}"
            );
        }

        // Premise: no single address is anywhere near the per-IP cap, so
        // the per-IP counter cannot be what refuses the next one.
        {
            let manager = sessions.borrow();
            for host in [1u8, 2, 3] {
                let ip = address(host).ip();
                assert!(
                    !manager.cluster_ip_at_limit(Token(99), H2_CLUSTER, &ip, None),
                    "premise: 203.0.113.{host} holds 1 of 10 per-IP slots and must be under \
                     its own cap — otherwise the refusal below is the per-IP cap, not the subnet"
                );
            }
        }

        // A FOURTH, never-seen address in the same /24 is refused.
        let (admitted, outcome) = run(Token(4), address(4));
        assert!(
            !admitted,
            "a fresh address from an already-counted /24 must be refused — an Admitted \
             verdict here is the #1270 bypass: a new address per connection defeats the cap"
        );
        assert!(
            matches!(
                outcome,
                Err(BackendConnectionError::TooManyConnectionsPerIp { .. })
            ),
            "the refused stream must surface the too-many-connections error, got {outcome:?}"
        );

        // Negative space: the same fourth address in a DIFFERENT /24 is
        // admitted, so the refusal above is the subnet key and not a
        // blanket cap on new addresses.
        let (admitted, _) = run(
            Token(5),
            "198.51.100.4:51000"
                .parse()
                .expect("test source address must parse"),
        );
        assert!(
            admitted,
            "an address in an UNCOUNTED subnet must still be admitted — otherwise the cap \
             is refusing on something other than the subnet key"
        );
    }

    /// The default must change nothing. With `max_connections_per_subnet`
    /// left at its `0` default, the admission path must behave exactly as
    /// it did before this feature existed — and, because the subnet
    /// tracking is skipped outright rather than merely ignored, it must
    /// also leave no bookkeeping behind.
    ///
    /// The same four addresses in one /24 that the test above refuses are
    /// all admitted here, and the per-IP cap still fires on its own terms
    /// when a single address exceeds it.
    #[test]
    fn an_unset_subnet_cap_leaves_the_admission_path_untouched() {
        let fixture = routing_fixture();
        let sessions = fixture.proxy.borrow().sessions();
        {
            let mut manager = sessions.borrow_mut();
            // A /24 prefix is configured but the cap is OFF: the prefix
            // alone must not gate anything.
            manager.subnet_ipv4_prefix = 24;
            manager.max_connections_per_subnet = 0;
            manager.max_connections_per_ip = 1;
        }

        let mut clusters = HashMap::new();
        clusters.insert(
            H2_CLUSTER.into(),
            Cluster {
                cluster_id: H2_CLUSTER.to_owned(),
                ..Default::default()
            },
        );
        let view = RoutingView::new(&clusters, ListenerType::Http);

        let run = |token: Token, source: SocketAddr| {
            let mut context = Context::new(
                Ulid::generate(),
                Rc::downgrade(&fixture.pool),
                fixture.listener.clone(),
                Some(source),
                "127.0.0.1:80"
                    .parse()
                    .expect("test public address must parse"),
            );
            let stream_id = context
                .create_stream(Ulid::generate(), 65_535)
                .expect("the test pool must hand out a stream");
            {
                let stream = &mut context.streams[stream_id];
                stream.state = StreamState::Link;
                stream.context.authority = Some(H2_AUTHORITY.to_owned());
                stream.context.path = Some("/".to_owned());
                stream.context.method = Some(Method::Get);
            }
            let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));
            let step = router
                .plan_connect(stream_id, &mut context, &view)
                .expect("routing must resolve the staged cluster");
            let ConnectStep::CheckIpLimit(resume) = step else {
                panic!("a stream WITH a session address must reach the connection gate")
            };
            let verdict = super::super::consult_ip_gate(
                &sessions,
                token,
                context
                    .http_context(stream_id)
                    .cluster_id
                    .as_ref()
                    .expect("plan_connect stamps the routed cluster"),
                &resume,
            );
            matches!(verdict, IpGateVerdict::Admitted)
        };

        // Four distinct addresses in one /24 — the exact traffic the test
        // above refuses — are all admitted while the cap is off.
        for (index, host) in [1u8, 2, 3, 4].into_iter().enumerate() {
            let source: SocketAddr = format!("203.0.113.{host}:51000")
                .parse()
                .expect("test source address must parse");
            assert!(
                run(Token(index + 1), source),
                "with max_connections_per_subnet = 0, 203.0.113.{host} must be admitted — \
                 the default must not gate traffic the previous release allowed"
            );
        }

        // The subnet accounting must be genuinely untouched, not merely
        // consulted-and-ignored: a disabled limiter allocates nothing.
        assert!(
            sessions.borrow().subnet_tracking_is_empty(),
            "a disabled subnet limiter must leave both subnet maps empty — a populated one \
             means every deployment that never enables this feature pays for it anyway"
        );

        // And the pre-existing per-IP cap still fires on its own terms:
        // a SECOND token from an address already counted once is refused
        // at a per-IP limit of 1.
        let repeat: SocketAddr = "203.0.113.1:51000"
            .parse()
            .expect("test source address must parse");
        assert!(
            !run(Token(90), repeat),
            "the per-IP cap must still refuse a second connection from the same address — \
             this feature must not have relaxed the limiter that already existed"
        );
    }

    /// The routing decision reads cluster configuration from the view it was
    /// handed, not from the `L7Proxy` handle it still carries.
    ///
    /// Question 6 of [#1340](https://github.com/sozu-proxy/sozu/issues/1340)
    /// puts routing inside the core, and the only thing that makes that worth
    /// anything is that the data routing reads is supplied rather than
    /// fetched — otherwise a simulator still cannot drive the decision. This
    /// hands `Router::plan_connect` a view that DISAGREES with the proxy
    /// about the one cluster knob the plan surfaces, and asserts the plan
    /// follows the view.
    ///
    /// The fixture registers `H2_CLUSTER` with `http2: Some(true)`. The view
    /// below says `Some(false)` for the same cluster id. A `Dial { h2: true }`
    /// would mean the decision went back to `L7Proxy::clusters` behind the
    /// view's back.
    ///
    /// To SEE THIS RED: restore either cluster read to the handle — in
    /// `Router::plan_connect` write `proxy.borrow().clusters().get(&cluster_id)`
    /// in place of `view.cluster(&cluster_id)` — and the assertion below
    /// reports `h2: true`.
    #[test]
    fn the_routing_decision_reads_cluster_config_from_the_view() {
        let fixture = routing_fixture();
        let pool = &fixture.pool;
        let mut context = Context::new(
            Ulid::generate(),
            Rc::downgrade(pool),
            fixture.listener.clone(),
            // No session address: the per-(cluster, source-IP) gate — the one
            // read still taken through the handle — is skipped, so this test
            // isolates the reads that moved.
            None,
            "127.0.0.1:80"
                .parse()
                .expect("test public address must parse"),
        );
        let stream_id = context
            .create_stream(Ulid::generate(), 65_535)
            .expect("the test pool must hand out a stream");
        {
            let stream = &mut context.streams[stream_id];
            stream.state = StreamState::Link;
            stream.context.authority = Some(H2_AUTHORITY.to_owned());
            stream.context.path = Some("/".to_owned());
            stream.context.method = Some(Method::Get);
        }

        // A cluster map that is NOT the proxy's, disagreeing on `http2`.
        let mut clusters = HashMap::new();
        clusters.insert(
            H2_CLUSTER.into(),
            Cluster {
                cluster_id: H2_CLUSTER.to_owned(),
                http2: Some(false),
                ..Default::default()
            },
        );
        assert_eq!(
            fixture
                .proxy
                .borrow()
                .clusters()
                .get(H2_CLUSTER)
                .and_then(|c| c.http2),
            Some(true),
            "premise: the proxy and the view must disagree, or this proves nothing"
        );
        let view = RoutingView::new(&clusters, ListenerType::Http);

        let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));
        let plan = router
            .plan_connect(stream_id, &mut context, &view)
            .map(decided)
            .expect("routing must resolve the staged cluster");

        match plan {
            ConnectPlan::Dial {
                ref cluster_id, h2, ..
            } => {
                assert_eq!(
                    &**cluster_id, H2_CLUSTER,
                    "premise: routing resolved a cluster"
                );
                assert!(
                    !h2,
                    "the plan must carry the view's `http2`, not the proxy's — \
                     `h2: true` means the decision read `L7Proxy::clusters` behind the view"
                );
            }
            ConnectPlan::Attached => {
                panic!("premise: an empty router has nothing to reuse, so it must ask for a dial")
            }
        }
    }

    /// A plan is a decision, not a deed: `ConnectPlan::Dial` must leave the
    /// router and the stream exactly as it found them.
    ///
    /// This is the property Question 6 of
    /// [#1340](https://github.com/sozu-proxy/sozu/issues/1340) is actually
    /// about — the core decides and the embedder performs — and it is the one
    /// a later edit is most likely to erode, by "just" doing one small effect
    /// inline because the data is right there. Asserting on the return value
    /// alone would not catch that: a `Dial` that had already dialled is still
    /// a `Dial`. So this asserts on the state the router and the stream are
    /// left in.
    ///
    /// To SEE THIS RED: perform any effect in `Router::plan_connect`'s
    /// new-backend path before it returns — inserting into `self.backends`,
    /// or calling `context.link_stream(stream_id, token)` — and the
    /// corresponding assertion below fires.
    ///
    /// SCOPE, because the name would otherwise over-read.
    /// `Router::plan_connect` is NOT pure yet. It still performs one registry
    /// mutation: `SessionManager::track_cluster_ip`, reached through
    /// `L7Proxy::sessions` in the per-(cluster, source-IP) limit gate. That is
    /// the mutation sitting *between* cluster resolution and backend
    /// selection that #1340 recorded as Question 6's first wrinkle, and it is
    /// deliberately unchanged here — it moves when the borrowed view lands,
    /// as either a second returned step or part of that view.
    ///
    /// This `Context` is therefore built with `session_address: None`, which
    /// skips the gate entirely, and the assertions below cover the router's
    /// backend map, the stream's link state, the backend-stream reverse index
    /// and the accounting ledger — NOT the session manager. Read the
    /// assertion list rather than the test name.
    #[test]
    fn a_dial_plan_performs_no_effect_of_its_own() {
        let fixture = routing_fixture();
        let pool = &fixture.pool;
        let mut context = Context::new(
            Ulid::generate(),
            Rc::downgrade(pool),
            fixture.listener.clone(),
            // No session address, so the per-(cluster, source-IP) gate is
            // skipped and this never reaches the proxy's session manager.
            None,
            "127.0.0.1:80"
                .parse()
                .expect("test public address must parse"),
        );
        let stream_id = context
            .create_stream(Ulid::generate(), 65_535)
            .expect("the test pool must hand out a stream");
        {
            let stream = &mut context.streams[stream_id];
            stream.state = StreamState::Link;
            stream.context.authority = Some(H2_AUTHORITY.to_owned());
            stream.context.path = Some("/".to_owned());
            stream.context.method = Some(Method::Get);
        }

        // An EMPTY router: nothing to reuse, so routing must fall through to
        // the new-backend path and ask the embedder for a dial.
        let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));
        let proxy_ref = fixture.proxy.borrow();
        let view = RoutingView::new(proxy_ref.clusters(), proxy_ref.kind());
        let plan = router
            .plan_connect(stream_id, &mut context, &view)
            .map(decided)
            .expect("routing must resolve the staged cluster");

        match &plan {
            ConnectPlan::Dial { cluster_id, .. } => assert_eq!(
                &**cluster_id, H2_CLUSTER,
                "the dial request must name the cluster routing resolved"
            ),
            ConnectPlan::Attached => {
                panic!("premise: an empty router has nothing to reuse, so it must ask for a dial")
            }
        }

        assert!(
            router.backends.is_empty(),
            "a plan must not have taken a backend connection into the router: {:?}",
            router.backends.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            context.streams[stream_id].state,
            StreamState::Link,
            "a plan must not have linked the stream — the embedder has not dialled yet"
        );
        assert!(
            context.backend_streams.is_empty(),
            "a plan must not have touched the backend-stream reverse index"
        );
        assert!(
            context.backend_deltas.is_empty(),
            "a plan must not have charged any backend accounting: {:?}",
            context.backend_deltas
        );
    }

    /// The leak with the most user-visible consequence: the H2 least-loaded
    /// arm compares stream counts with a strict `<`, so every staged backend
    /// sitting at the same count leaves the choice entirely to iteration
    /// order — hash order decided which machine served the request.
    #[test]
    fn an_h2_stream_count_tie_always_picks_the_lowest_backend_token() {
        let fixture = routing_fixture();
        for round in 0..ROUNDS {
            let chosen = selected_backend_token(&fixture, &Staged::ConnectedH2);
            assert_eq!(
                chosen,
                Token(LOWEST_TOKEN),
                "round {round}: a stream-count tie must resolve to the lowest token, \
                 not to whatever the map iterated first"
            );
        }
    }

    /// The same scan's fallback arm, and it does NOT bias the same way:
    /// it assigns with neither a `break` nor an "already chosen" guard, so
    /// the LAST matching connecting backend wins. The total order pins that
    /// to the highest token — asserting the lowest here would be red against
    /// both containers.
    #[test]
    fn an_all_connecting_h2_fallback_always_picks_the_highest_backend_token() {
        let fixture = routing_fixture();
        for round in 0..ROUNDS {
            let chosen = selected_backend_token(&fixture, &Staged::ConnectingH2);
            assert_eq!(
                chosen,
                Token(HIGHEST_TOKEN),
                "round {round}: the connecting-backend fallback assigns last-wins, \
                 so the total order must land on the highest token"
            );
        }
    }

    /// The H1 keep-alive arm carries the same first-seen bias as the H2
    /// least-loaded one — `reuse_token = Some(*token); break;` — and it is
    /// live wherever an H2 frontend fans several concurrent streams onto an
    /// H1 cluster: each stream dials its own socket, every socket parks
    /// `KeepAlive` for the same cluster, and the next request picks among
    /// them by map order.
    #[test]
    fn an_h1_keep_alive_reuse_always_picks_the_lowest_backend_token() {
        let fixture = routing_fixture();
        for round in 0..ROUNDS {
            let chosen = selected_backend_token(&fixture, &Staged::KeepAliveH1);
            assert_eq!(
                chosen,
                Token(LOWEST_TOKEN),
                "round {round}: the first matching keep-alive socket wins, so the total \
                 order must land on the lowest token"
            );
        }
    }

    /// #1579: the routing decision for a request that reuses a pooled
    /// backend connection allocates nothing of its own, in steady state.
    ///
    /// The reuse branch of `Router::decide_after_gate` stamps the stream's
    /// `HttpContext` and `SessionMetrics` with the id the connection's
    /// `BackendId` carries. That id is an `Rc<str>`, so the stamp is a
    /// reference-count increment; a `String` copy there costs one heap
    /// allocation per field and per request.
    ///
    /// Measured against a control that performs the same mux bookkeeping the
    /// branch triggers — `Connection::start_stream` then
    /// `Context::link_stream` on the same keep-alive socket — and nothing
    /// else. Each request is released the way production releases it
    /// (`Context::unlink_stream`, then the ledger drained through
    /// `BackendRegistry::apply_all` as a Mux pass drains it), so the control
    /// carries exactly the bookkeeping's costs and the difference is what the
    /// decision adds.
    ///
    /// Both are also held at zero outright (#1583): the reverse-index entry
    /// keeps its emptied `Vec` for the next `link_stream`, and the ledger is
    /// drained in place, so the bookkeeping itself allocates nothing either.
    ///
    /// TO SEE THIS RED: stamp either field with
    /// `Some(backend.backend_id.to_string().into())` instead of the `Rc`
    /// clone; the difference is then two allocations per request per field
    /// (the `String`, then the `Rc<str>` built from it). For the absolute
    /// half, make `remove_backend_stream` drop the emptied entry again, or
    /// make `BackendRegistry::apply_all` iterate `mem::take(deltas)`: each
    /// costs one allocation per request in both measurements.
    #[test]
    fn a_request_on_a_reused_backend_connection_allocates_nothing() {
        use std::hint::black_box;

        use crate::test_allocations::allocations;

        const REQUESTS: usize = 64;
        let backend_token = Token(LOWEST_TOKEN);

        let fixture = routing_fixture();
        let mut context = Context::new(
            Ulid::generate(),
            Rc::downgrade(&fixture.pool),
            fixture.listener.clone(),
            None,
            "127.0.0.1:80"
                .parse()
                .expect("test public address must parse"),
        );
        let stream_id = context
            .create_stream(Ulid::generate(), 65_535)
            .expect("the test pool must hand out a stream");
        context.streams[stream_id].context.method = Some(Method::Get);
        // Where `Router::plan_connect` leaves the routed cluster for the
        // decision to read.
        context.streams[stream_id].context.cluster_id = Some(H1_CLUSTER.into());

        let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));
        let mut backend_registry = BackendRegistry::default();
        let (connection, _peer) =
            staged_backend(&fixture.pool, &Staged::KeepAliveH1, &mut backend_registry);
        router.backends.insert(backend_token, connection);

        // Released as `ConnectionH1::end_stream` releases a terminated
        // response on a keep-alive backend, then the ledger drained as the
        // next Mux pass drains it.
        let backend_registry = &backend_registry;
        let release = |router: &mut Router, context: &mut Context<HttpListener>| {
            assert_eq!(
                context.streams[stream_id].state,
                StreamState::Linked(backend_token)
            );
            context.unlink_stream(stream_id);
            backend_registry.apply_all(&mut context.backend_deltas, &fixture.backends);
            context.streams[stream_id].forget_upstream_replay();
            context.streams[stream_id].state = StreamState::Link;
            let Some(Connection::H1(h1)) = router.backends.get_mut(&backend_token) else {
                unreachable!("the staged backend is an H1 connection")
            };
            h1.stream = None;
            if let Position::Client(_, _, status) = &mut h1.position {
                *status = BackendStatus::KeepAlive;
            }
        };
        // The worker's retired-backend check rides the decision, so it is
        // measured with it.
        let clusters = HashMap::new();
        let retired = |backend: &super::BackendId| backend_registry.is_retired(backend);
        let view = RoutingView::new(&clusters, ListenerType::Http).with_backend_retired(&retired);
        // One request through the routing decision.
        let decision = |router: &mut Router, context: &mut Context<HttpListener>| -> usize {
            let before = allocations();
            let plan =
                router.decide_after_gate(stream_id, black_box(&mut *context), &view, false, false);
            let allocated = allocations() - before;
            assert!(
                matches!(plan, Ok(ConnectPlan::Attached)),
                "the keep-alive socket must be reused, got {plan:?}"
            );
            release(router, context);
            allocated
        };
        // The same socket attached by hand: only the mux bookkeeping.
        let control = |router: &mut Router, context: &mut Context<HttpListener>| -> usize {
            let before = allocations();
            let started = router
                .backends
                .get_mut(&backend_token)
                .expect("the staged backend is registered")
                .start_stream(stream_id, black_box(&mut *context));
            context.link_stream(stream_id, backend_token);
            let allocated = allocations() - before;
            assert!(started, "the keep-alive socket must accept the stream");
            release(router, context);
            allocated
        };

        // Warm-up: metric keys and tables are sized by the first request.
        decision(&mut router, &mut context);
        control(&mut router, &mut context);
        let (mut decided, mut controlled) = (0, 0);
        for _ in 0..REQUESTS {
            decided += decision(&mut router, &mut context);
            controlled += control(&mut router, &mut context);
        }

        decision(&mut router, &mut context);
        let stream = &context.streams[stream_id];
        assert_eq!(stream.context.backend_id.as_deref(), Some("test-backend"));
        assert_eq!(stream.metrics.backend_id.as_deref(), Some("test-backend"));
        assert_eq!(
            decided.saturating_sub(controlled),
            0,
            "{REQUESTS} requests on a reused keep-alive backend made {decided} \
             heap allocations in the routing decision against {controlled} for \
             the mux bookkeeping alone, expected no difference"
        );
        assert_eq!(
            (decided, controlled),
            (0, 0),
            "{REQUESTS} requests on a reused keep-alive backend must allocate \
             nothing in the decision nor in the mux bookkeeping it triggers"
        );
    }

    /// #1583: the whole reuse branch copies no cluster id, gated or not.
    ///
    /// `ClusterId` is a reference-counted `Arc<str>` in sozu-command-lib:
    /// `route_from_request` hands `Router::plan_connect` a handle on the
    /// route table's own id, `plan_connect` moves it into the stream's
    /// `HttpContext`, and every later reader borrows it from there or bumps
    /// its count — the per-(cluster, source-IP) gate's `SessionManager`
    /// bookkeeping included, whose maps are keyed by the same handle. With
    /// the reverse-index entry and the delta ledger keeping their capacity,
    /// the branch allocates nothing at all past the debug build's history.
    ///
    /// Measured end to end — `plan_connect`, then on the gated path
    /// `consult_ip_gate` with its `SessionManager::track_cluster_connection`
    /// and `plan_connect_resume`, then the production release — against a
    /// control made of the one cost this branch does not own and which runs
    /// inside `plan_connect`: the debug-build `DebugEvent::Str` history push,
    /// absent from a release build, where the control is zero. The rest of
    /// `route_from_request` is measured, not controlled: it copies no
    /// authority and the route lookup records its trie segments on the stack
    /// (#1589). Both paths run, because production traffic always carries a
    /// source address and so always takes the gated one, while the fixtures
    /// above never do.
    ///
    /// TO SEE THIS RED: stamp `stream_context.cluster_id` in `plan_connect`
    /// with a copy (`Some(ClusterId::from(&*cluster_id))`) instead of moving
    /// the routed handle in: one allocation per request, on both paths. Hand
    /// `SessionManager::track_cluster_connection` such a copy in
    /// `consult_ip_gate` and the gated path alone pays one more. The same
    /// count comes back if `route_from_request` stamps
    /// `HttpContext::original_authority` with a copy of the authority on a
    /// request without a host rewrite, or if `Router::lookup` hands the trie
    /// a `Vec::with_capacity(16)` again.
    #[test]
    fn a_routed_request_on_a_reused_backend_connection_copies_no_cluster_id() {
        use std::hint::black_box;

        use crate::test_allocations::allocations;

        const REQUESTS: usize = 64;
        let backend_token = Token(LOWEST_TOKEN);
        let fixture = routing_fixture();
        let sessions = fixture.proxy.borrow().sessions();

        for gated in [false, true] {
            let mut context = Context::new(
                Ulid::generate(),
                Rc::downgrade(&fixture.pool),
                fixture.listener.clone(),
                gated.then(|| {
                    "127.0.0.1:4242"
                        .parse()
                        .expect("test session address must parse")
                }),
                "127.0.0.1:80"
                    .parse()
                    .expect("test public address must parse"),
            );
            let stream_id = context
                .create_stream(Ulid::generate(), 65_535)
                .expect("the test pool must hand out a stream");
            {
                let stream = &mut context.streams[stream_id];
                stream.state = StreamState::Link;
                stream.context.authority = Some(H1_AUTHORITY.to_owned());
                stream.context.path = Some("/".to_owned());
                stream.context.method = Some(Method::Get);
            }
            let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));
            let mut backend_registry = BackendRegistry::default();
            let (connection, _peer) =
                staged_backend(&fixture.pool, &Staged::KeepAliveH1, &mut backend_registry);
            router.backends.insert(backend_token, connection);
            let proxy_ref = fixture.proxy.borrow();
            let view = RoutingView::new(proxy_ref.clusters(), proxy_ref.kind());

            // One request through the whole branch, the gate's bookkeeping
            // included, then released as production releases it.
            let branch = |router: &mut Router, context: &mut Context<HttpListener>| -> usize {
                let before = allocations();
                let step = router
                    .plan_connect(stream_id, black_box(&mut *context), &view)
                    .expect("the reused request must be planned");
                let mut allocated = allocations() - before;
                let plan = match step {
                    ConnectStep::Decided(plan) => {
                        assert!(!gated, "a stream with a source address must be gated");
                        plan
                    }
                    ConnectStep::CheckIpLimit(resume) => {
                        assert!(gated, "a stream without a source address must not be gated");
                        let before = allocations();
                        let verdict = super::super::consult_ip_gate(
                            &sessions,
                            Token(0),
                            context
                                .http_context(stream_id)
                                .cluster_id
                                .as_ref()
                                .expect("plan_connect stamps the routed cluster"),
                            &resume,
                        );
                        let plan = router.plan_connect_resume(
                            stream_id,
                            &mut *context,
                            &view,
                            resume,
                            verdict,
                        );
                        allocated += allocations() - before;
                        plan.expect("the gate is unlimited, so the stream is admitted")
                    }
                };
                assert!(
                    matches!(plan, ConnectPlan::Attached),
                    "the keep-alive socket must be reused, got {plan:?}"
                );
                let before = allocations();
                context.unlink_stream(stream_id);
                backend_registry.apply_all(&mut context.backend_deltas, &fixture.backends);
                allocated += allocations() - before;
                let stream = &mut context.streams[stream_id];
                stream.forget_upstream_replay();
                stream.state = StreamState::Link;
                stream.attempts = 0;
                let Some(Connection::H1(h1)) = router.backends.get_mut(&backend_token) else {
                    unreachable!("the staged backend is an H1 connection")
                };
                h1.stream = None;
                if let Position::Client(_, _, status) = &mut h1.position {
                    *status = BackendStatus::KeepAlive;
                }
                allocated
            };
            // The one cost `plan_connect` carries that is not this branch's:
            // the debug history push. Routing hands over a handle on the
            // route table's cluster id, so nothing else may allocate.
            let control = |context: &mut Context<HttpListener>| -> usize {
                let before = allocations();
                #[cfg(debug_assertions)]
                {
                    use crate::protocol::mux::DebugEvent;

                    let route = context.streams[stream_id].context.get_route();
                    context.debug.push(DebugEvent::Str(route));
                }
                #[cfg(not(debug_assertions))]
                let _ = &context;
                allocations() - before
            };

            // Warm-up: metric keys, the reverse index and the ledger are
            // sized by the first request.
            branch(&mut router, &mut context);
            control(&mut context);
            let (mut branched, mut controlled) = (0, 0);
            for _ in 0..REQUESTS {
                branched += branch(&mut router, &mut context);
                controlled += control(&mut context);
            }

            assert_eq!(
                context.http_context(stream_id).cluster_id.as_deref(),
                Some(H1_CLUSTER)
            );
            assert_eq!(
                branched, controlled,
                "gated={gated}: {REQUESTS} requests on a reused keep-alive backend \
                 made {branched} heap allocations through the reuse branch against \
                 {controlled} for the debug history, expected no difference"
            );
        }
    }

    /// #1583: a dial copies no cluster id: the new connection shares the
    /// routed one.
    ///
    /// `ClusterId` is a reference-counted `Arc<str>` in sozu-command-lib, so
    /// the route table, the stream's `HttpContext::cluster_id`, the
    /// `ConnectPlan::Dial` and the new connection's `Position::Client` all
    /// point at the one allocation the route table made when the frontend was
    /// added. `Mux::attach_dialed` moves the plan's handle into the connection.
    ///
    /// TO SEE THIS RED: build the connection in `Mux::attach_dialed` from
    /// `ClusterId::from(&*cluster_id)`, or stamp the plan in
    /// `Router::decide_after_gate` with one; the connection then owns a
    /// second copy at another address.
    #[test]
    fn a_dial_moves_the_planned_cluster_id_into_the_backend_connection() {
        use crate::{Protocol, ProxySession, protocol::mux::Mux, server::ListenSession};

        let fixture = routing_fixture();
        let upstream =
            std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback listener must bind");
        fixture.backends.borrow_mut().add_backend(
            H1_CLUSTER,
            Backend::new(
                "dial-backend",
                upstream
                    .local_addr()
                    .expect("a bound listener has an address"),
                None,
                None,
                None,
            ),
        );
        let mut context = Context::new(
            Ulid::generate(),
            Rc::downgrade(&fixture.pool),
            fixture.listener.clone(),
            None,
            "127.0.0.1:80"
                .parse()
                .expect("test public address must parse"),
        );
        let stream_id = context
            .create_stream(Ulid::generate(), 65_535)
            .expect("the test pool must hand out a stream");
        {
            let stream = &mut context.streams[stream_id];
            stream.state = StreamState::Link;
            stream.context.authority = Some(H1_AUTHORITY.to_owned());
            stream.context.path = Some("/".to_owned());
            stream.context.method = Some(Method::Get);
        }
        let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));
        let mut backend_registry = BackendRegistry::default();

        let plan = {
            let proxy_ref = fixture.proxy.borrow();
            let view = RoutingView::new(proxy_ref.clusters(), proxy_ref.kind());
            router
                .plan_connect(stream_id, &mut context, &view)
                .map(decided)
                .expect("an empty router must ask for a dial")
        };
        let ConnectPlan::Dial {
            cluster_id,
            h2,
            frontend_should_stick,
        } = plan
        else {
            panic!("premise: an empty router has nothing to reuse, so it must ask for a dial")
        };
        let planned = cluster_id.as_ptr();

        let session: Rc<RefCell<dyn ProxySession>> = Rc::new(RefCell::new(ListenSession {
            protocol: Protocol::HTTPListen,
        }));
        Mux::<mio::net::TcpStream, HttpListener>::dial_backend(
            &mut router,
            &mut backend_registry,
            &fixture.backends,
            stream_id,
            &mut context,
            &session,
            &fixture.proxy,
            cluster_id,
            h2,
            frontend_should_stick,
        )
        .expect("the loopback backend must dial");

        let token = match context.streams[stream_id].state {
            StreamState::Linked(token) => token,
            other => panic!("the dial must link the stream, got {other:?}"),
        };
        let Some(Position::Client(owned, _, _)) =
            router.backends.get(&token).map(Connection::position)
        else {
            panic!("the dialled connection must be a client")
        };
        assert_eq!(&**owned, H1_CLUSTER);
        assert_eq!(
            context.http_context(stream_id).cluster_id.as_deref(),
            Some(H1_CLUSTER)
        );
        assert!(
            std::ptr::eq(owned.as_ptr(), planned),
            "the connection must own the id the plan carried, not a copy of it"
        );
        let routed = context
            .http_context(stream_id)
            .cluster_id
            .as_ref()
            .expect("plan_connect stamps the routed cluster");
        assert!(
            std::ptr::eq(owned.as_ptr(), routed.as_ptr()),
            "the connection must share the stream's routed id, not a copy of it"
        );
    }

    /// Dial one fresh stream of `H1_CLUSTER` through `Mux::dial_backend`
    /// against a single backend at `address`, then drain the ledger as the
    /// next pass would. Returns the dial's result and the backend.
    fn dial_one(
        address: std::net::SocketAddr,
    ) -> (Result<(), BackendConnectionError>, Rc<RefCell<Backend>>) {
        dial_with(address, DialFault::None)
    }

    /// A local failure `dial_with` stages between the plan and the dial, each
    /// reaching one exit of `Mux::attach_dialed`, past a successful `connect(2)`.
    enum DialFault {
        /// No fault: the dial runs as it does in production.
        None,
        /// Every buffer of the pool is checked out, so
        /// `Connection::new_h2_client` returns `None`. Dials `H2_CLUSTER`.
        PoolExhausted,
        /// The request already holds output encoded for another connection,
        /// so `ConnectionH2::start_stream` refuses it (#1632). Dials
        /// `H2_CLUSTER`.
        StartRefused,
        /// `L7Proxy::register_socket` fails, as it does under fd pressure.
        /// Dials `H1_CLUSTER`.
        RegisterRefused,
    }

    /// The fixture's proxy, except that registering a socket fails.
    ///
    /// Everything `Mux::dial_backend` calls besides `register_socket` goes to
    /// the real proxy, so the slab session it adds is removed from it again.
    struct RegisterRefuses(Rc<RefCell<dyn L7Proxy>>);

    impl L7Proxy for RegisterRefuses {
        fn kind(&self) -> ListenerType {
            self.0.borrow().kind()
        }
        fn register_socket(
            &self,
            _socket: &mut mio::net::TcpStream,
            _token: Token,
            _interest: mio::Interest,
        ) -> Result<(), std::io::Error> {
            Err(std::io::Error::from_raw_os_error(24)) // EMFILE
        }
        fn add_session(&self, session: Rc<RefCell<dyn crate::ProxySession>>) -> Token {
            self.0.borrow().add_session(session)
        }
        fn remove_session(&self, token: Token) -> bool {
            self.0.borrow().remove_session(token)
        }
        fn clusters(&self) -> &HashMap<sozu_command::state::ClusterId, Cluster> {
            unreachable!("Mux::dial_backend never reads the cluster table")
        }
        fn sessions(&self) -> Rc<RefCell<crate::server::SessionManager>> {
            self.0.borrow().sessions()
        }
    }

    /// [`dial_one`] with `fault` staged before the dial.
    fn dial_with(
        address: std::net::SocketAddr,
        fault: DialFault,
    ) -> (Result<(), BackendConnectionError>, Rc<RefCell<Backend>>) {
        use crate::{Protocol, ProxySession, protocol::mux::Mux, server::ListenSession};

        let (cluster, authority) = match fault {
            DialFault::PoolExhausted | DialFault::StartRefused => (H2_CLUSTER, H2_AUTHORITY),
            DialFault::None | DialFault::RegisterRefused => (H1_CLUSTER, H1_AUTHORITY),
        };
        let fixture = routing_fixture();
        fixture.backends.borrow_mut().add_backend(
            cluster,
            Backend::new("dial-once", address, None, None, None),
        );
        let backend = fixture
            .backends
            .borrow_mut()
            .backends
            .get_mut(cluster)
            .and_then(|list| list.find_backend(&address).cloned())
            .expect("the backend was just added");
        let mut context = Context::new(
            Ulid::generate(),
            Rc::downgrade(&fixture.pool),
            fixture.listener.clone(),
            None,
            "127.0.0.1:80"
                .parse()
                .expect("test public address must parse"),
        );
        let stream_id = context
            .create_stream(Ulid::generate(), 65_535)
            .expect("the test pool must hand out a stream");
        {
            let stream = &mut context.streams[stream_id];
            stream.state = StreamState::Link;
            stream.context.authority = Some(authority.to_owned());
            stream.context.path = Some("/".to_owned());
            stream.context.method = Some(Method::Get);
        }
        let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));
        let mut backend_registry = BackendRegistry::default();
        let plan = {
            let proxy_ref = fixture.proxy.borrow();
            let view = RoutingView::new(proxy_ref.clusters(), proxy_ref.kind());
            router
                .plan_connect(stream_id, &mut context, &view)
                .map(decided)
                .expect("an empty router must ask for a dial")
        };
        let ConnectPlan::Dial {
            cluster_id,
            h2,
            frontend_should_stick,
        } = plan
        else {
            panic!("premise: an empty router has nothing to reuse, so it must ask for a dial")
        };
        assert_eq!(
            h2,
            cluster == H2_CLUSTER,
            "premise: the plan must dial the protocol the cluster declares"
        );
        // Held until the dial returns: dropping a checkout gives it back.
        let mut exhausted = Vec::new();
        let mut proxy = fixture.proxy.clone();
        match fault {
            DialFault::None => {}
            DialFault::PoolExhausted => {
                while let Some(buffer) = fixture.pool.borrow_mut().checkout() {
                    exhausted.push(buffer);
                }
            }
            DialFault::StartRefused => {
                context.streams[stream_id]
                    .front
                    .out
                    .push_back(kawa::OutBlock::Store(kawa::Store::Static(b"encoded")));
            }
            DialFault::RegisterRefused => {
                proxy = Rc::new(RefCell::new(RegisterRefuses(fixture.proxy.clone())));
            }
        }
        let session: Rc<RefCell<dyn ProxySession>> = Rc::new(RefCell::new(ListenSession {
            protocol: Protocol::HTTPListen,
        }));
        let dialed = Mux::<mio::net::TcpStream, HttpListener>::dial_backend(
            &mut router,
            &mut backend_registry,
            &fixture.backends,
            stream_id,
            &mut context,
            &session,
            &proxy,
            cluster_id,
            h2,
            frontend_should_stick,
        );
        drop(exhausted);
        assert!(
            dialed.is_ok() || router.backends.is_empty(),
            "a dial that failed must keep no backend connection, got {dialed:?}"
        );
        backend_registry.apply_all(&mut context.backend_deltas, &fixture.backends);
        (dialed, backend)
    }

    /// #1684: a dial that connects counts its connection once. Selection
    /// reserved it, and the dial keeps that reservation as the connection's
    /// count rather than taking a second one.
    ///
    /// TO SEE THIS RED: call `inc_connections` on the dialled backend in
    /// `Mux::dial_backend` after the connect succeeds, as
    /// `Backend::try_connect` does for the TCP proxy; the backend then holds
    /// two connections for one dial.
    #[test]
    fn a_successful_dial_counts_its_connection_once() {
        let upstream =
            std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback listener must bind");
        let (dialed, backend) = dial_one(
            upstream
                .local_addr()
                .expect("a bound listener has an address"),
        );
        dialed.expect("the loopback backend must dial");
        let backend = backend.borrow();
        assert_eq!(
            backend.active_connections, 1,
            "one dial must hold exactly one connection on its backend"
        );
        assert_eq!(backend.failures, 0, "a successful dial records no failure");
    }

    /// #1684: a `connect(2)` that fails synchronously releases the reservation
    /// selection took and records the failure, once the ledger is drained.
    ///
    /// `255.255.255.255` is the limited broadcast address: connecting a TCP
    /// socket to it fails before any packet leaves (`ENETUNREACH`, or
    /// `EACCES` where a default route exists). The dial is asserted to fail,
    /// so a host where it somehow connected fails this test loudly instead of
    /// passing it vacuously.
    ///
    /// TO SEE THIS RED: return the error from `Mux::dial_backend` without
    /// pushing the `BackendChange::DialFailed` delta; the reservation then
    /// outlives the failed dial.
    #[test]
    fn a_synchronously_failed_dial_releases_its_reservation() {
        let (dialed, backend) = dial_one("255.255.255.255:80".parse().expect("a literal address"));
        assert!(
            matches!(
                dialed,
                Err(BackendConnectionError::Backend(
                    crate::backends::BackendError::ConnectionFailures { failures: 1, .. }
                ))
            ),
            "a connect to the broadcast address must fail synchronously, got {dialed:?}"
        );
        let backend = backend.borrow();
        assert_eq!(
            backend.active_connections, 0,
            "the failed dial must release the reservation selection took"
        );
        assert_eq!(
            backend.failures, 1,
            "the failed dial must count one failure"
        );
    }

    /// #1713: dial one stream with `fault` staged against a listening
    /// backend, and assert the dial failed with `expected` and left the
    /// backend exactly as it found it: the reservation selection took is
    /// released, and no failure is recorded, because the backend did not fail.
    fn assert_a_local_failure_releases_the_reservation(
        fault: DialFault,
        expected: fn(&BackendConnectionError) -> bool,
    ) {
        use crate::retry::{RetryAction, RetryPolicy};

        let upstream =
            std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback listener must bind");
        let (dialed, backend) = dial_with(
            upstream
                .local_addr()
                .expect("a bound listener has an address"),
            fault,
        );
        assert!(
            dialed.as_ref().is_err_and(expected),
            "premise: the staged fault must fail the dial at its exit, got {dialed:?}"
        );
        let backend = backend.borrow();
        assert_eq!(
            backend.active_connections, 0,
            "a dial abandoned after its connect must release the reservation \
             selection took"
        );
        assert_eq!(
            backend.failures, 0,
            "a local failure must not count against the backend"
        );
        assert_eq!(
            backend.retry_policy.can_try(std::time::Instant::now()),
            Some(RetryAction::OKAY),
            "a local failure must not arm the backend's backoff"
        );
    }

    /// #1713: `Connection::new_h2_client` returning `None` (buffer pool
    /// exhausted) releases the reservation.
    ///
    /// TO SEE THIS RED: in `Mux::dial_backend`, return the error of
    /// `Mux::attach_dialed` without pushing the `BackendChange::ConnectionClosed`
    /// delta; the backend then keeps `active_connections == 1`.
    #[test]
    fn an_h2_client_the_pool_cannot_build_releases_its_reservation() {
        assert_a_local_failure_releases_the_reservation(DialFault::PoolExhausted, |error| {
            matches!(error, BackendConnectionError::MaxBuffers)
        });
    }

    /// #1713: a refused `Connection::start_stream` releases the reservation.
    ///
    /// TO SEE THIS RED: as for
    /// [`an_h2_client_the_pool_cannot_build_releases_its_reservation`].
    #[test]
    fn a_refused_stream_start_releases_its_reservation() {
        assert_a_local_failure_releases_the_reservation(DialFault::StartRefused, |error| {
            matches!(error, BackendConnectionError::MaxSessionsMemory)
        });
    }

    /// #1713: the `register_socket` rollback releases the reservation, not
    /// only the gauges, `active_requests` and the slab session.
    ///
    /// TO SEE THIS RED: as for
    /// [`an_h2_client_the_pool_cannot_build_releases_its_reservation`].
    #[test]
    fn a_socket_the_poller_refuses_releases_its_reservation() {
        assert_a_local_failure_releases_the_reservation(DialFault::RegisterRefused, |error| {
            matches!(error, BackendConnectionError::MaxSessionsMemory)
        });
    }

    /// #1610: a session's first backend connection is stored without a heap
    /// allocation, and the connections still iterate in `Token` order however
    /// they were inserted and removed.
    ///
    /// `Router::backends` was a `BTreeMap`, whose first insertion allocates an
    /// 11-slot leaf of ~1.7 KiB connections, ~18.8 KiB, for the one backend a
    /// session almost always has.
    ///
    /// TO SEE THIS RED: in `BackendConnections::insert`, send the `None` arm
    /// to `self.rest.insert(token, connection)` instead of filling `first`
    /// (and let `remove` and `is_empty` read `rest` alone). The first
    /// insertion then makes one allocation.
    #[test]
    fn the_first_backend_connection_of_a_session_allocates_nothing() {
        use crate::test_allocations::allocations;

        let fixture = routing_fixture();
        let mut backend_registry = BackendRegistry::default();
        let mut staged = |router: &mut Router, token: usize| {
            let (connection, peer) =
                staged_backend(&fixture.pool, &Staged::KeepAliveH1, &mut backend_registry);
            let before = allocations();
            let replaced = router.backends.insert(Token(token), connection);
            let allocated = allocations() - before;
            assert!(replaced.is_none(), "Token({token}) was not held yet");
            (allocated, peer)
        };

        let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));
        let (first, _first_peer) = staged(&mut router, LOWEST_TOKEN + 2);
        assert_eq!(
            first, 0,
            "the first backend connection of a session must not allocate"
        );

        // A lower token takes the inline slot, a higher one goes to the map.
        let (_, _lower_peer) = staged(&mut router, LOWEST_TOKEN);
        let (_, _higher_peer) = staged(&mut router, LOWEST_TOKEN + 5);
        let order = |router: &Router| router.backends.keys().map(|t| t.0).collect::<Vec<_>>();
        assert_eq!(
            order(&router),
            [LOWEST_TOKEN, LOWEST_TOKEN + 2, LOWEST_TOKEN + 5]
        );
        assert_eq!(router.backends.len(), 3);

        // Removing the inline connection promotes the lowest remaining one.
        assert!(router.backends.remove(&Token(LOWEST_TOKEN)).is_some());
        assert_eq!(order(&router), [LOWEST_TOKEN + 2, LOWEST_TOKEN + 5]);
        assert!(router.backends.contains_key(&Token(LOWEST_TOKEN + 2)));
        assert!(!router.backends.contains_key(&Token(LOWEST_TOKEN)));
        assert!(router.backends.remove(&Token(LOWEST_TOKEN + 5)).is_some());
        assert!(router.backends.remove(&Token(LOWEST_TOKEN + 2)).is_some());
        assert!(router.backends.is_empty());
    }
}

/// [`Router::backend_from_request`] against the embedder's real selector.
///
/// Question 12 of [#1340](https://github.com/sozu-proxy/sozu/issues/1340)
/// replaced the proxy handle this function took with a backend-set capability,
/// now a [`BackendSelector`] (#1684); it had no test before that change. Each
/// fixture owns its own `BackendMap` and `BackendRegistry`, so nothing here
/// depends on a proxy, a listener or a session — and, since selection no
/// longer dials, on any socket.
#[cfg(test)]
mod backend_selector_tests {
    use std::{
        cell::RefCell,
        net::SocketAddr,
        time::{Duration, Instant},
    };

    use sozu_command::proto::command::{LoadBalancingAlgorithms, LoadMetric};

    use super::Router;
    use crate::{
        backends::{Backend, BackendMap},
        protocol::{
            http::editor::HttpContext,
            mux::{BackendChange, BackendDelta, BackendId, BackendRegistry, RegistrySelector},
        },
        retry::{RetryAction, RetryPolicy},
    };

    const CLUSTER: &str = "cluster-1340-q12";
    const STICKY_B: &str = "sticky-b";

    /// Two backends under `policy` on connections. The addresses are never
    /// dialled: selection reserves, and only a caller of the selector connects.
    /// `LeastLoaded` is the default because round-robin would alternate
    /// whatever the counters said, and a test that passes without reading the
    /// load observes nothing.
    struct Fixture {
        backends: RefCell<BackendMap>,
        registry: BackendRegistry,
        router: Router,
        now: Instant,
    }

    fn fixture_with(policy: LoadBalancingAlgorithms) -> Fixture {
        let now = Instant::now();
        let address = |port: u16| SocketAddr::from(([127, 0, 0, 1], port));
        let mut backends = BackendMap::with_seed(0x1684);
        backends.set_load_balancing_policy_for_cluster(
            CLUSTER,
            policy,
            Some(LoadMetric::Connections),
        );
        backends.add_backend(
            CLUSTER,
            Backend::new_at("backend-a", address(9501), None, None, None, now),
        );
        backends.add_backend(
            CLUSTER,
            Backend::new_at(
                "backend-b",
                address(9502),
                Some(STICKY_B.to_owned()),
                None,
                None,
                now,
            ),
        );
        Fixture {
            backends: RefCell::new(backends),
            registry: BackendRegistry::default(),
            router: Router::new(Duration::from_secs(10), Duration::from_secs(10)),
            now,
        }
    }

    fn fixture() -> Fixture {
        fixture_with(LoadBalancingAlgorithms::LeastLoaded)
    }

    fn request(cookie: Option<&str>) -> HttpContext {
        let mut context = HttpContext::new(
            rusty_ulid::Ulid::generate(),
            rusty_ulid::Ulid::generate(),
            crate::Protocol::HTTP,
            "127.0.0.1:80"
                .parse()
                .expect("test public address must parse"),
            None,
            "SOZUBALANCEID".to_owned(),
            "Sozu-Id".to_owned(),
            false,
            false,
        );
        context.sticky_session_found = cookie.map(ToOwned::to_owned);
        context
    }

    impl Fixture {
        fn select(&mut self, should_stick: bool, context: &mut HttpContext) -> BackendId {
            let mut backends = self.backends.borrow_mut();
            let mut selector = RegistrySelector {
                backends: &mut backends,
                registry: &mut self.registry,
                now: self.now,
                exclude: &[],
            };
            let backend = self
                .router
                .backend_from_request(CLUSTER, should_stick, context, &mut selector)
                .expect("a cluster with two available backends must select");
            assert_eq!(
                context.backend_id.as_deref(),
                Some(&*backend.backend_id),
                "the request must be stamped with the backend it was reserved on"
            );
            backend
        }

        fn backend(&self, id: &BackendId) -> std::cell::Ref<'_, Backend> {
            self.registry
                .handle(id)
                .expect("an id this fixture minted must resolve")
                .borrow()
        }

        fn active_connections(&self, backend: &BackendId) -> usize {
            self.backend(backend).active_connections
        }
    }

    /// Two streams linking in one pass must spread under `LeastLoaded`, and
    /// no dial happens in between.
    ///
    /// The second selection reads `active_connections`, which only the first
    /// selection's reservation can have raised: nothing here connects, and no
    /// delta drain runs. So this holds only if selecting reserves under the
    /// same borrow as the choice (#1684). A selector that stopped reserving —
    /// counting the connection at the dial instead — would send both streams
    /// to `backend-a`.
    ///
    /// TO SEE THIS RED: make `Backend::reserve_connection` return `Ok(())`
    /// without calling `inc_connections`.
    #[test]
    fn a_second_selection_observes_the_first_selections_reservation() {
        let mut fixture = fixture();

        let first = fixture.select(false, &mut request(None));
        let second = fixture.select(false, &mut request(None));

        assert_eq!(
            &*first.backend_id, "backend-a",
            "with both backends idle the first minimum wins"
        );
        assert_ne!(
            first.backend_id,
            second.backend_id,
            "the second selection landed on the first one's backend: the first \
             selection did not reserve its connection \
             (first={} active={}, second={} active={})",
            first.backend_id,
            fixture.active_connections(&first),
            second.backend_id,
            fixture.active_connections(&second),
        );
        assert_eq!(
            (
                fixture.active_connections(&first),
                fixture.active_connections(&second)
            ),
            (1, 1),
            "each selection must reserve exactly one connection on the backend it chose"
        );
    }

    /// #1684: power-of-two-choices weighs every reservation too.
    ///
    /// With two backends the sample is the whole set, so each selection must
    /// take the one with fewer reserved connections, and a tie resolves by
    /// coin flip. Over any run of selections the two counts therefore never
    /// drift more than one apart, and together they count every selection.
    ///
    /// TO SEE THIS RED: make `Backend::reserve_connection` return `Ok(())`
    /// without calling `inc_connections`; nothing is reserved and the total
    /// stays at zero.
    #[test]
    fn power_of_two_weighs_every_reservation() {
        const SELECTIONS: usize = 64;
        let mut fixture = fixture_with(LoadBalancingAlgorithms::PowerOfTwo);
        let first = fixture.select(false, &mut request(None));
        let mut ids = vec![first];
        for selections in 2..=SELECTIONS {
            let before: Vec<usize> = ids
                .iter()
                .map(|id| fixture.active_connections(id))
                .collect();
            let picked = fixture.select(false, &mut request(None));
            if !ids.iter().any(|id| id.backend_id == picked.backend_id) {
                ids.push(picked.clone());
            }
            let loads: Vec<usize> = ids
                .iter()
                .map(|id| fixture.active_connections(id))
                .collect();
            assert_eq!(
                loads.iter().sum::<usize>(),
                selections,
                "{selections} selections must hold {selections} reservations, got {loads:?}"
            );
            if ids.len() == 2 {
                let (low, high) = (loads[0].min(loads[1]), loads[0].max(loads[1]));
                assert!(
                    high - low <= 1,
                    "power-of-two let the reservations drift apart: {loads:?} (before {before:?})"
                );
            }
        }
        assert_eq!(ids.len(), 2, "both backends must be used");
    }

    /// #1684: a dial whose `connect(2)` fails releases its reservation and
    /// records the failure, once the ledger is drained.
    ///
    /// TO SEE THIS RED: drop `self.dec_connections()` from
    /// `Backend::release_failed_dial`; the reservation then outlives the
    /// failed dial.
    #[test]
    fn a_failed_dial_releases_its_reservation_and_records_the_failure() {
        let mut fixture = fixture();
        let backend = fixture.select(false, &mut request(None));
        assert_eq!(
            fixture.active_connections(&backend),
            1,
            "selection reserves"
        );

        let mut deltas = vec![BackendDelta {
            slot: backend.slot(),
            change: BackendChange::DialFailed(fixture.now),
        }];
        fixture.registry.apply_all(&mut deltas, &fixture.backends);

        assert!(deltas.is_empty(), "the drain empties the ledger");
        let released = fixture.backend(&backend);
        assert_eq!(
            released.active_connections, 0,
            "a failed dial must release the reservation selection took"
        );
        assert_eq!(released.failures, 1, "a failed dial must count one failure");
        assert_eq!(
            released.retry_policy.can_try(fixture.now),
            Some(RetryAction::WAIT),
            "a failed dial must arm the backoff at the instant it failed"
        );
    }

    /// A cookie pins the request only when its frontend sticks.
    ///
    /// The call this replaced matched on `(frontend_should_stick, cookie)`
    /// and sent `(false, Some(_))` to the load balancer; the selector now takes
    /// an [`super::Affinity`] built by the caller, so that arm is the one a
    /// signature change could flip without a compile error.
    #[test]
    fn a_cookie_pins_only_a_frontend_that_sticks() {
        let mut unpinned = fixture();
        let mut context = request(Some(STICKY_B));
        let backend = unpinned.select(false, &mut context);
        assert_eq!(
            &*backend.backend_id, "backend-a",
            "a frontend that does not stick must ignore the client's cookie"
        );
        assert_eq!(
            context.sticky_session, None,
            "a frontend that does not stick must not answer with a cookie"
        );

        let mut pinned = fixture();
        let mut context = request(Some(STICKY_B));
        let backend = pinned.select(true, &mut context);
        assert_eq!(
            &*backend.backend_id, "backend-b",
            "a sticky frontend must follow the cookie to its backend"
        );
        assert_eq!(context.sticky_session.as_deref(), Some(STICKY_B));
        assert_eq!(
            pinned.active_connections(&backend),
            1,
            "a sticky selection reserves like any other"
        );

        let mut fallback = fixture();
        let mut context = request(None);
        let backend = fallback.select(true, &mut context);
        assert_eq!(
            context.sticky_session.as_deref(),
            Some(&*backend.backend_id),
            "a sticky frontend with no cookie answers with the chosen \
             backend's id when it has no sticky id of its own"
        );
    }

    /// [`fixture`] under `HRW`, the policy that reads the affinity key.
    fn hrw_fixture() -> Fixture {
        fixture_with(LoadBalancingAlgorithms::Hrw)
    }

    /// A request whose `plan_connect` derived `key`.
    fn keyed_request(cookie: Option<&str>, key: u64) -> HttpContext {
        let mut context = request(cookie);
        context.affinity_key = Some(key);
        context
    }

    /// #524: the key a request carries reaches the load balancer, so `HRW`
    /// sends one key to one backend selection after selection.
    ///
    /// TO SEE THIS RED: pass `None` instead of `context.affinity_key` to
    /// `BackendSelector::select` in `Router::backend_from_request`;
    /// `HRW` then round-robins and the second selection of each key lands on
    /// the other backend.
    #[test]
    fn a_keyed_request_stays_on_one_backend_under_hrw() {
        let mut fixture = hrw_fixture();
        for key in 0..16u64 {
            let first = fixture.select(false, &mut keyed_request(None, key));
            for _ in 0..3 {
                let again = fixture.select(false, &mut keyed_request(None, key));
                assert_eq!(
                    again.backend_id, first.backend_id,
                    "key {key} must stay on one backend under HRW"
                );
            }
        }
    }

    /// #524: a sticky cookie naming a live backend wins over the key; the key
    /// only decides where a request the cookie did not pin lands.
    #[test]
    fn a_sticky_cookie_wins_over_the_affinity_key() {
        let mut fixture = hrw_fixture();
        let key = (0..64u64)
            .find(|&key| {
                &*fixture
                    .select(false, &mut keyed_request(None, key))
                    .backend_id
                    == "backend-a"
            })
            .expect("HRW sends some of 64 keys to each of two backends");

        let mut context = keyed_request(Some(STICKY_B), key);
        let backend = fixture.select(true, &mut context);
        assert_eq!(
            &*backend.backend_id, "backend-b",
            "the cookie must pin the request whatever its key says"
        );

        let mut context = keyed_request(None, key);
        let backend = fixture.select(true, &mut context);
        assert_eq!(
            &*backend.backend_id, "backend-a",
            "without a cookie a sticky frontend follows the key"
        );
    }

    /// #1579: stamping a selected backend onto the request allocates nothing.
    ///
    /// `Router::backend_from_request` runs once per backend dial and writes
    /// the chosen backend's id into `HttpContext::backend_id`. The id arrives
    /// as the `Rc<str>` of the selected `BackendId`, so the stamp is a
    /// reference-count increment. The selector is a stub that always answers
    /// one id, so the count covers the router's own work and not the load
    /// balancer. It needs no socket: a selector does not dial (#1684).
    ///
    /// TO SEE THIS RED: stamp `Some(selected.backend.backend_id.to_string().into())`
    /// instead of the `Rc` clone; the assertion then reports two allocations
    /// per dial.
    #[test]
    fn stamping_a_dialled_backend_allocates_nothing() {
        use std::hint::black_box;

        use super::{Affinity, BackendSelector, SelectedBackend};
        use crate::{backends::BackendError, test_allocations::allocations};

        const DIALS: usize = 64;

        /// Always answers `backend`, minted as a simulator would mint it.
        struct StubSelector {
            backend: BackendId,
        }

        impl BackendSelector for StubSelector {
            fn select(
                &mut self,
                _cluster_id: &str,
                _affinity: Affinity<'_>,
                _key: Option<u64>,
            ) -> Result<SelectedBackend, BackendError> {
                Ok(SelectedBackend {
                    backend: self.backend.clone(),
                    sticky_session: None,
                })
            }
        }

        let mut selector = StubSelector {
            backend: BackendId::new(
                0,
                std::rc::Rc::from("stamped-backend"),
                SocketAddr::from(([127, 0, 0, 1], 9503)),
            ),
        };
        let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));
        let mut context = request(None);

        // Warm-up: the first stamp fills the slot the later ones overwrite.
        drop(
            router
                .backend_from_request(CLUSTER, false, &mut context, &mut selector)
                .expect("the stub selector always selects"),
        );
        let mut allocated = 0;
        for _ in 0..DIALS {
            let before = allocations();
            let selected = router.backend_from_request(
                black_box(CLUSTER),
                false,
                black_box(&mut context),
                &mut selector,
            );
            allocated += allocations() - before;
            drop(selected.expect("the stub selector always selects"));
        }

        assert_eq!(context.backend_id.as_deref(), Some("stamped-backend"));
        assert_eq!(
            allocated, 0,
            "{DIALS} dials made {allocated} heap allocations stamping the \
             backend onto the request, expected none"
        );
    }
}

/// [`affinity_key`]: which request bytes key `HRW` and `MAGLEV`, and that
/// deriving the key costs no allocation (#524).
#[cfg(test)]
mod affinity_key_tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use sozu_command::proto::command::{Cluster, LoadBalancingAlgorithms};

    use super::{StickyCookie, affinity_key};
    use crate::{
        load_balancing::{affinity_key_from_ip, affinity_key_from_value},
        pool::Pool,
        protocol::mux::GenericHttpStream,
    };

    /// A listener sticky cookie the requests below do not carry.
    const NO_STICKY: StickyCookie<'static> = StickyCookie {
        name: "SOZUBALANCEID",
        value: None,
    };

    const CLIENT: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)), 40_000);

    /// A request kawa parsed from `request`, on its own pool buffer.
    fn parsed(pool: &mut Pool, request: &[u8]) -> GenericHttpStream {
        let mut kawa: GenericHttpStream = kawa::Kawa::new(
            kawa::Kind::Request,
            kawa::Buffer::new(
                pool.checkout()
                    .expect("the test pool must hand out a buffer"),
            ),
        );
        kawa.storage.space()[..request.len()].copy_from_slice(request);
        kawa.storage.fill(request.len());
        kawa::h1::parse(&mut kawa, &mut kawa::h1::NoCallbacks);
        assert!(kawa.is_main_phase(), "premise: the request must parse");
        kawa
    }

    fn cluster(
        policy: LoadBalancingAlgorithms,
        header: Option<&str>,
        cookie: Option<&str>,
    ) -> Cluster {
        Cluster {
            cluster_id: "affinity".to_owned(),
            load_balancing: policy as i32,
            affinity_header: header.map(ToOwned::to_owned),
            affinity_cookie: cookie.map(ToOwned::to_owned),
            ..Default::default()
        }
    }

    const REQUEST: &[u8] = b"GET / HTTP/1.1\r\nHost: example.com\r\nx-tenant: acme\r\n\
        Cookie: theme=dark; tenant=globex\r\nX-Empty: \r\n\r\n";

    /// Only `HRW`, `MAGLEV` and a sharded cluster read a key; every other
    /// cluster gets `None`, pays nothing and selects exactly as before.
    ///
    /// TO SEE THIS RED: drop the `shard_percent` arm of
    /// `cluster_reads_affinity_key`; the sharded round-robin cluster then
    /// derives no key and is silently not sharded.
    #[test]
    fn only_hrw_maglev_and_sharded_clusters_derive_a_key() {
        let mut pool = Pool::with_capacity(1, 1, 4096);
        let front = parsed(&mut pool, REQUEST);
        for policy in [
            LoadBalancingAlgorithms::RoundRobin,
            LoadBalancingAlgorithms::Random,
            LoadBalancingAlgorithms::LeastLoaded,
            LoadBalancingAlgorithms::PowerOfTwo,
        ] {
            assert_eq!(
                affinity_key(
                    &cluster(policy, Some("X-Tenant"), None),
                    &front,
                    Some(CLIENT),
                    NO_STICKY
                ),
                None,
                "{policy:?} does not read a key"
            );
        }
        // #524: a sharded cluster reads the key under any policy, to rank
        // the client's shard by.
        let sharded_round_robin = Cluster {
            shard_percent: Some(25),
            ..cluster(LoadBalancingAlgorithms::RoundRobin, None, None)
        };
        assert_eq!(
            affinity_key(&sharded_round_robin, &front, Some(CLIENT), NO_STICKY),
            Some(affinity_key_from_ip(CLIENT.ip())),
            "a sharded cluster derives a key whatever its policy"
        );
        for policy in [
            LoadBalancingAlgorithms::Hrw,
            LoadBalancingAlgorithms::Maglev,
        ] {
            assert_eq!(
                affinity_key(
                    &cluster(policy, None, None),
                    &front,
                    Some(CLIENT),
                    NO_STICKY
                ),
                Some(affinity_key_from_ip(CLIENT.ip())),
                "{policy:?} keys on the source IP by default"
            );
        }
    }

    /// The configured header keys the request, its name matched without
    /// regard to case; a header the request lacks, or carries empty, falls
    /// back to the source IP; with no source address either there is no key.
    #[test]
    fn a_configured_header_keys_the_request_and_the_source_ip_is_the_fallback() {
        let mut pool = Pool::with_capacity(1, 1, 4096);
        let front = parsed(&mut pool, REQUEST);
        let hrw = LoadBalancingAlgorithms::Hrw;
        assert_eq!(
            affinity_key(
                &cluster(hrw, Some("X-Tenant"), None),
                &front,
                Some(CLIENT),
                NO_STICKY
            ),
            Some(affinity_key_from_value(b"acme"))
        );
        assert_eq!(
            affinity_key(
                &cluster(hrw, Some("X-Absent"), None),
                &front,
                Some(CLIENT),
                NO_STICKY
            ),
            Some(affinity_key_from_ip(CLIENT.ip()))
        );
        assert_eq!(
            affinity_key(
                &cluster(hrw, Some("X-Empty"), None),
                &front,
                Some(CLIENT),
                NO_STICKY
            ),
            Some(affinity_key_from_ip(CLIENT.ip())),
            "an empty value keys nothing and falls back to the source IP"
        );
        assert_eq!(
            affinity_key(
                &cluster(hrw, Some("X-Absent"), None),
                &front,
                None,
                NO_STICKY
            ),
            None,
            "no header and no source address: no key, and the policy round-robins"
        );
    }

    /// The configured cookie keys the request, its name matched exactly
    /// (RFC 6265 cookie names are case-sensitive).
    #[test]
    fn a_configured_cookie_keys_the_request() {
        let mut pool = Pool::with_capacity(1, 1, 4096);
        let front = parsed(&mut pool, REQUEST);
        let maglev = LoadBalancingAlgorithms::Maglev;
        assert_eq!(
            affinity_key(
                &cluster(maglev, None, Some("tenant")),
                &front,
                Some(CLIENT),
                NO_STICKY
            ),
            Some(affinity_key_from_value(b"globex"))
        );
        assert_eq!(
            affinity_key(
                &cluster(maglev, None, Some("Tenant")),
                &front,
                Some(CLIENT),
                NO_STICKY
            ),
            Some(affinity_key_from_ip(CLIENT.ip())),
            "a cookie name differing in case is another cookie"
        );
    }

    /// #524: deriving the key allocates nothing: the header blocks and the
    /// cookie jar are read where they lie and hashed in place.
    ///
    /// TO SEE THIS RED: copy the value out before hashing it in
    /// `affinity_key`, e.g. `affinity_key_from_value(&value.to_vec())`; each
    /// derivation then makes one allocation.
    #[test]
    fn deriving_an_affinity_key_allocates_nothing() {
        use std::hint::black_box;

        use crate::test_allocations::allocations;

        const DERIVATIONS: usize = 64;
        let mut pool = Pool::with_capacity(1, 1, 4096);
        let front = parsed(&mut pool, REQUEST);
        let clusters = [
            cluster(LoadBalancingAlgorithms::Hrw, None, None),
            cluster(LoadBalancingAlgorithms::Hrw, Some("X-Tenant"), None),
            cluster(LoadBalancingAlgorithms::Maglev, None, Some("tenant")),
            cluster(LoadBalancingAlgorithms::Maglev, Some("X-Absent"), None),
        ];
        let mut allocated = 0;
        for cluster in &clusters {
            for _ in 0..DERIVATIONS {
                let before = allocations();
                let key = affinity_key(
                    black_box(cluster),
                    black_box(&front),
                    Some(CLIENT),
                    NO_STICKY,
                );
                allocated += allocations() - before;
                assert!(key.is_some());
            }
        }
        assert_eq!(
            allocated,
            0,
            "{} derivations made {allocated} heap allocations, expected none",
            DERIVATIONS * clusters.len()
        );
    }

    /// A cluster keying on the listener's own sticky-session cookie reads the
    /// value captured before that cookie was elided from the jar; without the
    /// capture it would never find the cookie and always fall back to the
    /// source IP.
    ///
    /// TO SEE THIS RED: drop the sticky-name arm of `affinity_key`, so the
    /// lookup reads only the jar.
    #[test]
    fn an_affinity_cookie_named_like_the_sticky_cookie_reads_its_captured_value() {
        let mut pool = Pool::with_capacity(1, 1, 4096);
        // The sticky cookie is already gone from the jar, as
        // `on_request_headers` leaves it.
        let front = parsed(&mut pool, REQUEST);
        let hrw = cluster(LoadBalancingAlgorithms::Hrw, None, Some("SOZUBALANCEID"));
        assert_eq!(
            affinity_key(
                &hrw,
                &front,
                Some(CLIENT),
                StickyCookie {
                    name: "SOZUBALANCEID",
                    value: Some("backend-7"),
                },
            ),
            Some(affinity_key_from_value(b"backend-7"))
        );
        assert_eq!(
            affinity_key(&hrw, &front, Some(CLIENT), NO_STICKY),
            Some(affinity_key_from_ip(CLIENT.ip())),
            "no sticky cookie on the request: the source IP keys it"
        );
    }
}
