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
//! `Router::connect` scans it with a plain `for (token, backend) in
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
//! The map is now a `BTreeMap`, so the scan is the total order on `Token` —
//! fixed, reproducible, and free (`BTreeMap` iteration is already sorted; no
//! extra sort step, no injected seed to thread through construction and
//! tests). The three decisions above resolve to the lowest `Token`, the
//! highest `Token` and the lowest `Token` respectively. Pinning is all this
//! does: the last-wins shape of the connecting fallback is preserved, not
//! corrected, because changing which backend is preferred is a routing change
//! and not a determinism one.
//!
//! A `Token` is the slab index the proxy's session manager handed the backend
//! socket at dial time, and the slab hands out its lowest free index, so the
//! order carries no fairness meaning of its own. It does not need one for the
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
//! to `BTreeMap`'s `O(log n)`, and `Mux::ready` does one `get_mut`
//! per backend event through `EndpointClient`. Every iteration site —
//! `Router::connect`'s scan, `Mux::reschedule`, the two
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

use std::{cell::RefCell, collections::BTreeMap, rc::Rc, time::Duration};

use mio::{Interest, Token, net::TcpStream};
use sozu_command::{
    logging::ansi_palette,
    proto::command::{ListenerType, RedirectPolicy, RedirectScheme},
};

#[cfg(debug_assertions)]
use super::DebugEvent;
use super::{BackendStatus, Connection, Context, GlobalStreamId, Position, StreamState};
use crate::{
    BackendConnectionError, L7ListenerHandler, L7Proxy, ListenerHandler, ProxySession, Readiness,
    RetrieveClusterError,
    backends::{Backend, BackendError},
    protocol::http::editor::{HeaderEditMode, HeaderEditSnapshot, HttpContext},
    router::{HeaderEdit, RouteResult},
    server::CONN_RETRIES,
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

#[derive(Debug)]
pub struct Router {
    pub backends: BTreeMap<Token, Connection<SessionTcpStream>>,
    pub configured_backend_timeout: Duration,
    pub configured_connect_timeout: Duration,
    /// Fallback readiness used when a backend token is missing from the map.
    /// This prevents panicking in the Endpoint trait methods that return references.
    pub(super) fallback_readiness: Readiness,
}

impl Router {
    pub fn new(configured_backend_timeout: Duration, configured_connect_timeout: Duration) -> Self {
        Self {
            backends: BTreeMap::new(),
            configured_backend_timeout,
            configured_connect_timeout,
            fallback_readiness: Readiness::new(),
        }
    }

    pub(super) fn connect<L: ListenerHandler + L7ListenerHandler>(
        &mut self,
        stream_id: GlobalStreamId,
        context: &mut Context<L>,
        session: Rc<RefCell<dyn ProxySession>>,
        proxy: Rc<RefCell<dyn L7Proxy>>,
        // Frontend session token, threaded in from `Mux::ready` so the
        // per-(cluster, source-IP) accounting can key on it without
        // re-borrowing `session` — the outer event-loop call chain
        // already holds a mutable borrow of that cell.
        frontend_token: Token,
    ) -> Result<(), BackendConnectionError> {
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
        if stream.attempts >= CONN_RETRIES {
            incr!(
                names::backend::CONNECT_RETRIES_EXHAUSTED,
                stream.context.cluster_id.as_deref(),
                stream.context.backend_id.as_deref()
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
        let replaying = stream.front.consumed;
        let cluster_id = if replaying {
            stream
                .context
                .cluster_id
                .clone()
                .ok_or(BackendConnectionError::ReplayRefused(
                    "no cluster_id survived the first attempt",
                ))?
        } else {
            let (front_ref, stream_context_ref) = {
                let stream_split = &mut *stream;
                (&mut stream_split.front, &mut stream_split.context)
            };
            self.route_from_request(stream_context_ref, front_ref, &context.listener, &proxy)
                .map_err(BackendConnectionError::RetrieveClusterError)?
        };
        let stream_context = &mut stream.context;
        stream_context.cluster_id = Some(cluster_id.to_owned());

        let (
            frontend_should_stick,
            frontend_should_redirect_https,
            h2,
            cluster_max_connections_per_ip,
            cluster_retry_after,
        ) = proxy
            .borrow()
            .clusters()
            .get(&cluster_id)
            .map(|cluster| {
                (
                    cluster.sticky_session,
                    cluster.https_redirect,
                    cluster.http2.unwrap_or(false),
                    cluster.max_connections_per_ip,
                    cluster.retry_after,
                )
            })
            .unwrap_or((false, false, false, None, None));

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
        if frontend_should_redirect_https && matches!(proxy.borrow().kind(), ListenerType::Http) {
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
        let session_ip = stream_context.session_address.map(|sa| sa.ip());
        if let Some(ip) = session_ip {
            // The frontend session is mutably borrowed up the call stack
            // (`HttpSession::ready` -> `state.ready` -> `Mux::ready` ->
            // here), so we cannot reach `session.borrow().frontend_token()`.
            // The token is threaded in by the caller instead.
            let sessions_rc = proxy.borrow().sessions();
            let at_limit = sessions_rc.borrow().cluster_ip_at_limit(
                frontend_token,
                &cluster_id,
                &ip,
                cluster_max_connections_per_ip,
            );
            if at_limit {
                let retry_after = sessions_rc
                    .borrow()
                    .effective_retry_after(cluster_retry_after);
                // Stash the resolved retry value on the stream so the
                // mux's BackendConnectionError → 429 mapping can render
                // (or elide) the `Retry-After` header without
                // re-deriving the override chain.
                stream_context.retry_after_seconds = Some(retry_after).filter(|v| *v > 0);
                return Err(BackendConnectionError::TooManyConnectionsPerIp {
                    cluster_id: cluster_id.to_owned(),
                });
            }
            // Idempotent track — H2 streams to the same `(cluster, ip)`
            // share a single slot in the per-token set. Decrement happens
            // wholesale on session close via `untrack_all_cluster_ip`.
            sessions_rc
                .borrow_mut()
                .track_cluster_ip(frontend_token, cluster_id.clone(), ip);
        }

        /*
        H2 connecting strategy (least-loaded):
        - look at every backend connection
        - among connected backends for this cluster, pick the one with the fewest active streams
        - fall back to a connecting backend if no connected one exists
        - if no backend is to reuse, ask the router for a socket to the "next in line" backend

        H1 strategy: reuse the first KeepAlive backend for this cluster.
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

                (true, Position::Client(other_cluster_id, _, BackendStatus::Connected)) => {
                    if *other_cluster_id == cluster_id && !backend.is_draining() {
                        // Pick the non-draining H2 connection with the fewest active streams
                        let Connection::H2(h2c) = backend else {
                            continue;
                        };
                        let stream_count = h2c.stream_count();
                        if stream_count
                            >= h2c.peer_settings.settings_max_concurrent_streams as usize
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
                    if *other_cluster_id == cluster_id
                        && best_h2_stream_count == usize::MAX
                        && matches!(backend, Connection::H2(_))
                    {
                        reuse_token = Some(*token)
                    }
                }
                (true, Position::Client(other_cluster_id, _, BackendStatus::KeepAlive)) => {
                    if *other_cluster_id == cluster_id && matches!(backend, Connection::H2(_)) {
                        error!(
                            "{} ConnectionH2 unexpectedly behaves like H1 with KeepAlive",
                            log_module_context!(stream_context)
                        );
                    }
                }

                (false, Position::Client(old_cluster_id, _, BackendStatus::KeepAlive)) => {
                    if *old_cluster_id == cluster_id {
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
            // For reused backends: set context fields and metrics lifecycle
            if let Some(backend_conn) = self.backends.get(&token)
                && let Position::Client(_, backend_ref, _) = backend_conn.position()
            {
                let backend = backend_ref.borrow();
                let stream = &mut context.streams[stream_id];
                stream.context.backend_id = Some(backend.backend_id.to_owned());
                stream.context.backend_address = Some(backend.address);
                stream.metrics.backend_id = Some(backend.backend_id.to_owned());
                stream.metrics.backend_start();
                stream.metrics.backend_connected();
            }
            context.link_stream(stream_id, token);
            return Ok(());
        }

        // New-backend path: fall through.
        //
        // Pool miss: no reusable connection was found (no live H2 multiplex
        // slot for this cluster, no H1 keep-alive socket). A fresh TCP dial
        // and full backend handshake will follow. Pair with `backend.pool.hit`
        // above. The metric is incremented BEFORE `backend_from_request` so
        // the count includes attempts that fail at backend selection
        // (BackendError::NoBackendForCluster, etc.) — every miss is a slot
        // we did not save. The dial itself may still fail
        // (BackendConnectionError::*), in which case `backend.pool.size` is
        // never bumped (see the gauge below) but the miss is already counted.
        incr!(names::backend::POOL_MISS);
        let token = {
            //
            // SECURITY (CWE-400): defer every stateful side-effect
            // (backend.connections / connections_per_backend gauges, slab
            // add_session, mio register_socket, self.backends.insert,
            // stream.metrics.backend_start) until AFTER `new_h2_client` AND
            // `start_stream` have both succeeded. If either fails we must
            // return Err without leaking a slab entry, an epoll registration,
            // a gauge counter, or a router-map entry.
            //
            // The TcpStream lives on the stack here and is moved into the
            // Connection by `new_h2_client`/`new_h1_client`; on failure the
            // Connection (or the raw TcpStream, for the pool-exhaustion
            // branch that drops inside `new_h2_client`) is dropped, closing
            // the fd. No token is ever allocated, so there is nothing to
            // roll back.
            let (socket, backend) = self.backend_from_request(
                &cluster_id,
                frontend_should_stick,
                stream_context,
                proxy.clone(),
                &context.listener,
            )?;

            if let Err(e) = socket.set_nodelay(true) {
                error!(
                    "{} error setting nodelay on back socket({:?}): {:?}",
                    log_module_context!(context.http_context(stream_id)),
                    socket,
                    e
                );
            }

            // Cache the backend's configured address so SOCKET log lines
            // fired on ECONNREFUSED (or any failed async `connect()`) can
            // still render `peer=<backend>` — `getpeername(2)` returns
            // ENOTCONN in that state, so the live lookup path would show
            // `peer=None` exactly when the operator needs the backend id.
            let backend_peer = Some(backend.borrow().address);
            let socket = SessionTcpStream::new(socket, context.session_ulid, backend_peer);

            let flood_config = context.listener.borrow().get_h2_flood_config();
            let connection_config = context.listener.borrow().get_h2_connection_config();
            let stream_idle_timeout = context.listener.borrow().get_h2_stream_idle_timeout();
            let graceful_shutdown_deadline = context
                .listener
                .borrow()
                .get_h2_graceful_shutdown_deadline();
            let backend_id_for_gauge = backend.borrow().backend_id.to_owned();
            let mut connection = if h2 {
                match Connection::new_h2_client(
                    context.session_ulid,
                    socket,
                    cluster_id.to_owned(),
                    backend,
                    context.pool.clone(),
                    self.configured_connect_timeout,
                    flood_config,
                    connection_config,
                    stream_idle_timeout,
                    graceful_shutdown_deadline,
                ) {
                    Some(connection) => connection,
                    // pool exhaustion: socket already dropped by new_h2_client,
                    // no side-effects were committed.
                    None => return Err(BackendConnectionError::MaxBuffers),
                }
            } else {
                Connection::new_h1_client(
                    context.session_ulid,
                    socket,
                    cluster_id.to_owned(),
                    backend,
                    self.configured_connect_timeout,
                )
            };

            // Check the backend can accept a new stream BEFORE committing any
            // registry state. `start_stream` increments `active_requests` via
            // `pre_start_stream_client_bookkeeping` and undoes it itself on
            // failure (see `Connection::start_stream`), so dropping the
            // connection on a false return leaves backend accounting clean.
            if !connection.start_stream(stream_id, context) {
                error!(
                    "{} Backend rejected stream start (max concurrent streams reached)",
                    log_module_context!(context.http_context(stream_id))
                );
                // `connection` (socket + pending timeout deadline) drops here; no
                // wheel entry was ever armed for it.
                return Err(BackendConnectionError::MaxSessionsMemory);
            }

            // --- Happy path: commit side-effects in one atomic-ish block ---
            let stream = &mut context.streams[stream_id];
            stream.metrics.backend_start();
            stream.metrics.backend_id = stream.context.backend_id.to_owned();
            gauge_add!(names::backend::CONNECTIONS, 1);
            // `backend.pool.size` mirrors `backend.connections` exactly: one
            // entry per `Router::backends` token. The `-1` partner lives in
            // `connection.rs::pre_close_client_bookkeeping` (graceful close)
            // and `mod.rs::close_backend` (session teardown). Symmetric
            // pairing with both decrement sites is the only defence against
            // the gauge underflow class of bug fixed by ff401b54 / aadb3fa4.
            gauge_add!(names::backend::POOL_SIZE, 1);
            gauge_add!(
                names::backend::CONNECTIONS_PER_BACKEND,
                1,
                Some(&cluster_id),
                Some(&backend_id_for_gauge)
            );

            let token = proxy.borrow().add_session(session);

            {
                let socket_ref = connection.socket_mut();
                if let Err(e) = proxy.borrow().register_socket(
                    socket_ref,
                    token,
                    Interest::READABLE | Interest::WRITABLE,
                ) {
                    // SECURITY (CWE-400): treat mio registration failure as a
                    // hard connect failure. Without this rollback the gauges
                    // (`backend.connections`, `backend.pool.size`,
                    // `connections_per_backend`), the slab session, and the
                    // already-incremented `Backend.active_requests` counter
                    // (bumped in `Connection::start_stream` ->
                    // `pre_start_stream_client_bookkeeping`) all leak until
                    // the connect timeout fires. Under fd pressure
                    // (EMFILE/ENFILE) this can occur in tight bursts and
                    // poison capacity dashboards.
                    error!(
                        "{} error registering back socket: {:?} — rolling back",
                        log_module_context!(context.http_context(stream_id)),
                        e
                    );
                    // Undo the gauge increments committed above.
                    gauge_add!(names::backend::CONNECTIONS, -1);
                    gauge_add!(names::backend::POOL_SIZE, -1);
                    gauge_add!(
                        names::backend::CONNECTIONS_PER_BACKEND,
                        -1,
                        Some(&cluster_id),
                        Some(&backend_id_for_gauge)
                    );
                    // Drop the slab session and the connection. The connection
                    // is local to this scope; dropping it here also closes the
                    // underlying TcpStream and releases the
                    // `Backend.active_requests` increment via the regular
                    // session drop path (`pre_close_client_bookkeeping`).
                    proxy.borrow().remove_session(token);
                    return Err(BackendConnectionError::MaxSessionsMemory);
                }
            }

            // No `set(token)` here any more: the connection arms its own
            // connect-timeout DEADLINE at construction and the `Mux` adapter
            // reflects it onto the wheel when `ready()` reschedules, which is
            // the same pass this runs in. Until that reschedule no wheel entry
            // exists — which is what the rollback paths above want, since they
            // drop `connection` without ever reaching here.
            self.backends.insert(token, connection);
            token
        };

        context.link_stream(stream_id, token);
        Ok(())
    }

    fn route_from_request<L: ListenerHandler + L7ListenerHandler>(
        &mut self,
        context: &mut HttpContext,
        front: &mut super::GenericHttpStream,
        listener: &Rc<RefCell<L>>,
        proxy: &Rc<RefCell<dyn L7Proxy>>,
    ) -> Result<String, RetrieveClusterError> {
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
        // Snapshot the pre-rewrite authority into an owned string so we
        // can later stash it on `context.original_authority` without
        // mutably aliasing the immutable borrow that `host: &str` still
        // holds on `context`.
        let captured_authority = host.to_owned();

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

        // Stash the pre-rewrite authority unconditionally so log lines,
        // access logs, and audit records that fire on ANY downstream
        // path (denial, redirect, basic-auth 401, backend-connect
        // failure, successful forward) carry the value the client
        // actually sent. Capturing inside the rewrite helper alone would
        // lose it on every branch where the rewrite is not applied.
        context.original_authority = Some(captured_authority);

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
                Some(id) => proxy
                    .borrow()
                    .clusters()
                    .get(id)
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
        // The caller (`Router::connect`) emits the actual 301 only on
        // `ListenerType::Http`; gate the URL stash on the same predicate
        // so an HTTPS listener never carries a stale `redirect_location`
        // into a downstream default-answer path.
        if legacy_https_redirect && matches!(proxy.borrow().kind(), ListenerType::Http) {
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

    pub fn backend_from_request<L: ListenerHandler + L7ListenerHandler>(
        &mut self,
        cluster_id: &str,
        frontend_should_stick: bool,
        context: &mut HttpContext,
        proxy: Rc<RefCell<dyn L7Proxy>>,
        listener: &Rc<RefCell<L>>,
    ) -> Result<(TcpStream, Rc<RefCell<Backend>>), BackendConnectionError> {
        let (backend, conn) = self
            .get_backend_for_sticky_session(
                cluster_id,
                frontend_should_stick,
                context.sticky_session_found.as_deref(),
                proxy,
            )
            .map_err(|backend_error| {
                trace!("{} {}", log_module_context!(context), backend_error);
                BackendConnectionError::Backend(backend_error)
            })?;

        if frontend_should_stick {
            // update sticky name in case it changed I guess?
            context.sticky_name = listener.borrow().get_sticky_name().to_string();

            context.sticky_session = Some(
                backend
                    .borrow()
                    .sticky_id
                    .clone()
                    .unwrap_or_else(|| backend.borrow().backend_id.to_owned()),
            );
        }

        context.backend_id = Some(backend.borrow().backend_id.to_owned());
        context.backend_address = Some(backend.borrow().address);

        Ok((conn, backend))
    }

    fn get_backend_for_sticky_session(
        &self,
        cluster_id: &str,
        frontend_should_stick: bool,
        sticky_session: Option<&str>,
        proxy: Rc<RefCell<dyn L7Proxy>>,
    ) -> Result<(Rc<RefCell<Backend>>, TcpStream), BackendError> {
        match (frontend_should_stick, sticky_session) {
            (true, Some(sticky_session)) => proxy
                .borrow()
                .backends()
                .borrow_mut()
                .backend_from_sticky_session(cluster_id, sticky_session),
            _ => proxy
                .borrow()
                .backends()
                .borrow_mut()
                .backend_from_cluster_id(cluster_id),
        }
    }
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
///    the kawa parser. The pre-rewrite authority itself is captured by
///    the caller (`route_from_request`) into `context.original_authority`
///    on every routed request so it survives every downstream code path
///    (audit, deny, redirect, basic-auth 401, backend-connect failure).
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

    // `route_from_request` already captured the pre-rewrite authority
    // into `context.original_authority`. Re-borrow it here for the
    // optional X-Forwarded-Host injection rather than re-parsing the
    // kawa Store. Cloning a short header value (typically `host:port`)
    // is cheaper than another UTF-8 decode of the request-line slice.
    let original_authority: Option<String> = if rewritten_host.is_some() {
        context.original_authority.clone()
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
    if rewriting_host || operator_overrides_xfh {
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
        if let Some(orig) = original_authority.as_deref() {
            synth.push(Block::Header(Pair {
                key: Store::Static(b"X-Forwarded-Host"),
                val: Store::from_string(orig.to_owned()),
            }));
        }
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
        proto::command::{PathRule, RequestHttpFrontend, RulePosition, SocketAddress},
    };

    use super::{
        Router, authority_matches_sni, log_coalescing_accepted, log_sni_authority_mismatch,
    };
    use crate::{
        L7Proxy,
        http::HttpProxy,
        protocol::{
            http::{editor::HttpContext, parser::Method},
            mux::stream::Stream,
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

            let mut stream = Stream::new(Rc::downgrade(&pool), context, 65_535)
                .expect("test stream must check out its buffers");
            let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));
            let (front, stream_context) = {
                let split = &mut stream;
                (&mut split.front, &mut split.context)
            };
            router
                .route_from_request(stream_context, front, &listener, &proxy)
                .expect("the wildcard frontend must route foo.example.com");

            stream.generate_access_log(false, None, listener, None, None);
        });

        assert!(
            output.contains(TAG),
            "the access log of a request routed by `*.example.com` must carry that frontend's tags, got: {output}"
        );
    }

    #[test]
    fn routing_runtime_logs_bound_method_authority_sni_and_certificate_names() {
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

/// Backend-selection order: `Router::connect` must resolve a tie the same way
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
/// token the total order on `Token` picks. Measured against the pre-image at
/// four keys: the minimum came first in 8, 5 and 7 of 24 rounds over three
/// runs, never in all 24.
///
/// That makes the red statistical rather than structural, and the bound is
/// stated so it can be checked rather than trusted: with `n` staged backends
/// and `r` rounds a `HashMap` survives with probability `n.pow(-r)`, which is
/// `4^-24` here — about one run in 2.8e14. A `BTreeMap` survives with
/// probability 1.
///
/// **To SEE THESE RED:** in this module, put `Router::backends` back to
/// `HashMap<Token, Connection<SessionTcpStream>>`, `Router::new`'s initialiser
/// back to `HashMap::new()` and the import back to `collections::HashMap`.
/// Reverting the file wholesale deletes these tests instead of reddening them.
#[cfg(test)]
mod backend_selection_order_tests {
    use std::{cell::RefCell, net::SocketAddr, rc::Rc, time::Duration};

    use mio::Token;
    use rusty_ulid::Ulid;
    use sozu_command::{
        config::ListenerBuilder,
        proto::command::{Cluster, PathRule, RequestHttpFrontend, RulePosition, SocketAddress},
    };

    use super::Router;
    use crate::{
        L7Proxy, ProxySession,
        backends::Backend,
        http::{HttpListener, HttpProxy},
        pool::Pool,
        protocol::{
            http::parser::Method,
            mux::{
                BackendStatus, Connection, Context, Position, StreamState, h2::H2ConnectionConfig,
                h2_flood_detector::H2FloodConfig,
            },
        },
        server::ListenSession,
        socket::SessionTcpStream,
    };

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

    /// Which arm of `Router::connect`'s scan the staged backends land in.
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

    /// The proxy, listener, clusters, frontends and buffer pool the routing
    /// half of `Router::connect` needs. Built once and shared by every round:
    /// the order under test belongs to `Router::backends`, which each round
    /// rebuilds from scratch.
    struct RoutingFixture {
        proxy: Rc<RefCell<dyn L7Proxy>>,
        listener: Rc<RefCell<HttpListener>>,
        pool: Rc<RefCell<Pool>>,
        /// `Router::connect` touches `session` only on the new-dial path
        /// (`L7Proxy::add_session`); every round here returns through the
        /// reuse path, which never reads it.
        session: Rc<RefCell<dyn ProxySession>>,
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
            listener,
            pool,
            session: Rc::new(RefCell::new(ListenSession {
                protocol: crate::Protocol::HTTP,
            })),
        }
    }

    /// One backend connection in the requested arm, plus the accepted peer the
    /// caller must keep alive for the length of the round.
    fn staged_backend(
        pool: &Rc<RefCell<Pool>>,
        staged: &Staged,
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
        let session_ulid = Ulid::generate();
        let socket = SessionTcpStream::new(socket, session_ulid, Some(backend_address));
        let mut connection = match staged {
            Staged::ConnectedH2 | Staged::ConnectingH2 => Connection::new_h2_client(
                session_ulid,
                socket,
                staged.cluster().to_owned(),
                backend,
                Rc::downgrade(pool),
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
                staged.cluster().to_owned(),
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
    /// in the pre-image — drive one `Router::connect`, and report the token it
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
        let mut peers = Vec::with_capacity(STAGED_TOKENS.len());
        for token in STAGED_TOKENS {
            let (connection, peer) = staged_backend(pool, staged);
            peers.push(peer);
            router.backends.insert(Token(token), connection);
        }

        router
            .connect(
                stream_id,
                &mut context,
                fixture.session.clone(),
                fixture.proxy.clone(),
                Token(0),
            )
            .expect("the router must reuse one of the staged backends");

        match context.streams[stream_id].state {
            StreamState::Linked(token) => token,
            other => panic!("connect must link the stream to a backend, got {other:?}"),
        }
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
}
