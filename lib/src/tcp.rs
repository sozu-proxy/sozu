use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, hash_map::Entry},
    io::ErrorKind,
    net::{Shutdown, SocketAddr},
    rc::Rc,
    time::{Duration, Instant},
};

use mio::{
    Interest, Registry, Token,
    net::{TcpListener as MioTcpListener, TcpStream as MioTcpStream},
};
use rusty_ulid::Ulid;
use socket2::SockRef;
use sozu_command::{
    ObjectKind,
    config::{
        DEFAULT_SNI_PREREAD_MAX_BYTES, DEFAULT_SNI_PREREAD_TIMEOUT, MAX_LOOP_ITERATIONS,
        MIN_SNI_PREREAD_MAX_BYTES, validate_sni_pattern,
    },
    listener_key::ListenerKey,
    logging::{EndpointRecord, LogContext, ansi_palette},
    proto::command::request::RequestType,
};

use crate::metrics::names;
use crate::router::pattern_trie::{InsertResult, TrieNode};
use crate::{
    AcceptError, BackendConnectAction, BackendConnectionError, BackendConnectionStatus, CachedTags,
    ListenerError, ListenerHandler, Protocol, ProxyConfiguration, ProxyError, ProxySession,
    Readiness, SessionIsToBeClosed, SessionMetrics, SessionResult, StateMachineBuilder,
    backends::{Backend, BackendMap},
    load_balancing::affinity_key_from_ip,
    pool::{Checkout, Pool},
    protocol::mux::router::cluster_reads_affinity_key,
    protocol::{
        Pipe,
        pipe::WebSocketContext,
        proxy_protocol::{
            expect::ExpectProxyProtocol, relay::RelayProxyProtocol, send::SendProxyProtocol,
        },
        tcp_preread::{AlpnMatcher, PrereadConfig, shell::SniPreread},
    },
    retry::RetryPolicy,
    server::{ListenToken, SessionManager, push_event},
    socket::{server_bind, stats::socket_rtt},
    sozu_command::{
        proto::command::{
            Event, EventKind, ProxyProtocolConfig, RequestTcpFrontend, TcpListenerConfig,
            UpdateTcpListenerConfig, WorkerRequest, WorkerResponse,
        },
        ready::Ready,
        state::ClusterId,
    },
    timer::TimeoutContainer,
};

StateMachineBuilder! {
    /// The various Stages of a TCP connection:
    ///
    /// 1. optional SniPreread (SNI-routed listeners only, sozu-proxy/sozu#1279)
    /// 2. optional (ExpectProxyProtocol | SendProxyProtocol | RelayProxyProtocol)
    /// 3. Pipe
    enum TcpStateMachine {
        Pipe(Pipe<MioTcpStream, TcpListener>),
        SendProxyProtocol(SendProxyProtocol<MioTcpStream>),
        RelayProxyProtocol(RelayProxyProtocol<MioTcpStream>),
        ExpectProxyProtocol(ExpectProxyProtocol<MioTcpStream>),
        SniPreread(SniPreread<MioTcpStream>),
    }
}

/// This macro is defined uniquely in this module to help the tracking of kawa h1
/// issues inside Sōzu. Colored output uses the unified log-context scheme:
/// bold bright-white protocol label, light-grey `Session` keyword, gray keys
/// and bright-white values.
macro_rules! log_context {
    ($self:expr) => {{
        let (open, reset, grey, gray, white) = ansi_palette();
        format!(
            "{gray}{ctx}{reset}\t{open}TCP{reset}\t{grey}Session{reset}({gray}frontend{reset}={white}{frontend}{reset}, {gray}backend{reset}={white}{backend}{reset})\t >>>",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            ctx = $self.log_context(),
            frontend = $self.frontend_token.0,
            backend = $self
                .backend_token
                .map(|token| token.0.to_string())
                .unwrap_or_else(|| "<none>".to_string()),
        )
    }};
}

/// Module-level prefix for log lines emitted from this file when no
/// [`TcpSession`] is in scope. Produces a bold bright-white `TCP` label
/// (uniform with the per-session `log_context!`) when the logger is in
/// colored mode. Used by [`TcpProxy`] callbacks (notify, accept,
/// create_session, soft_stop, hard_stop, status) and the `testing`
/// helper module which own a listener/token map but have no
/// `frontend_token` of their own.
macro_rules! log_module_context {
    () => {{
        let (open, reset, _, _, _) = sozu_command::logging::ansi_palette();
        format!("{open}TCP{reset}\t >>>", open = open, reset = reset)
    }};
}

pub struct TcpSession {
    backend_buffer: Option<Checkout>,
    backend_connected: BackendConnectionStatus,
    backend_id: Option<String>,
    /// Address of the backend `connect_to_backend` last dialed, handed to the
    /// `Pipe` for its access log (`Pipe::set_backend_address`), so the log
    /// never asks the socket with `getpeername(2)`.
    backend_address: Option<SocketAddr>,
    backend_token: Option<Token>,
    backend: Option<Rc<RefCell<Backend>>>,
    cluster_id: Option<ClusterId>,
    /// Configured lifetime of `cluster_id` captured when this session routes.
    /// Kept on the session rather than the public `SessionMetrics` value so
    /// delayed TCP and Pipe emissions cannot target a replacement cluster.
    cluster_metrics_incarnation: Option<crate::metrics::ClusterMetricsIncarnation>,
    configured_backend_timeout: Duration,
    connection_attempt: u8,
    container_backend_timeout: TimeoutContainer,
    container_frontend_timeout: TimeoutContainer,
    frontend_address: Option<SocketAddr>,
    frontend_buffer: Option<Checkout>,
    frontend_token: Token,
    has_been_closed: SessionIsToBeClosed,
    last_event: Instant,
    listener: Rc<RefCell<TcpListener>>,
    metrics: SessionMetrics,
    proxy: Rc<RefCell<TcpProxy>>,
    request_id: Ulid,
    state: TcpStateMachine,
    /// `true` once `connect_to_backend` has accounted this session
    /// against the per-(cluster, source-IP) connection counter. Drives
    /// the symmetric `untrack_all_cluster_ip` call in `close`. The flag
    /// is per-session, not per-attempt: a TCP session has at most one
    /// `(cluster, ip)` slot, so the SessionManager-side idempotency
    /// already covers retries — this flag exists only to short-circuit
    /// the close path's untrack when the feature is disabled or no
    /// admit ever ran.
    cluster_ip_tracked: bool,
    /// SNI-preread routing result (sozu-proxy/sozu#1279), captured once by
    /// `upgrade_sni_preread` for every `proxy_protocol` case and consumed
    /// (`Option::take`) at the point the session actually reaches `Pipe`:
    /// immediately in `build_pipe_from_preread` for
    /// `Expect`/`Relay`/`None`, or one `ready()` cycle later in
    /// `upgrade_send` for `SendHeader` (which transitions through
    /// `SendProxyProtocol` first). `None` for every non-SNI-routed session.
    routed_sni: Option<String>,
    /// Paired with `routed_sni`: the client's first ALPN offer, mapped to a
    /// known `&'static str` label (`"h2"` / `"http/1.1"`) for the access
    /// log, or `None` if the client offered nothing recognized. Sōzu never
    /// negotiates ALPN itself on the TCP passthrough path -- the backend
    /// terminates TLS -- so this is informational (the client's
    /// preference), not a negotiated value.
    routed_alpn_label: Option<&'static str>,
    /// Canonical access-log tags key for the MATCHED SNI/ALPN frontend
    /// (`sni_tags_key`), rebuilt in `upgrade_sni_preread` from the route
    /// decision's `matched_sni_pattern` + `matched_alpn`. `None` for every
    /// non-SNI-routed session, whose tags stay keyed by the bare listener
    /// address exactly as before SNI routing existed. Unlike `routed_sni`
    /// it is never consumed: `log_request` reads it for the session-level
    /// access log, and the `Pipe` receives a clone at upgrade time for the
    /// post-upgrade log.
    tags_key: Option<String>,
}

impl TcpSession {
    #[allow(clippy::too_many_arguments)]
    fn new(
        backend_buffer: Checkout,
        backend_id: Option<String>,
        cluster_id: Option<ClusterId>,
        configured_backend_timeout: Duration,
        configured_connect_timeout: Duration,
        configured_frontend_timeout: Duration,
        frontend_buffer: Checkout,
        frontend_token: Token,
        listener: Rc<RefCell<TcpListener>>,
        proxy_protocol: Option<ProxyProtocolConfig>,
        proxy: Rc<RefCell<TcpProxy>>,
        socket: MioTcpStream,
        peer: SocketAddr,
        wait_time: Duration,
    ) -> TcpSession {
        // The address `accept(2)` returned with `socket`.
        let frontend_address = Some(peer);
        let mut frontend_buffer_session = None;
        let mut backend_buffer_session = None;

        let request_id = Ulid::generate();

        let container_frontend_timeout =
            TimeoutContainer::new(configured_frontend_timeout, frontend_token);
        let container_backend_timeout = TimeoutContainer::new_empty(configured_connect_timeout);
        let cluster_metrics_incarnation = cluster_id
            .as_deref()
            .and_then(crate::metrics::cluster_incarnation);

        let state = match proxy_protocol {
            Some(ProxyProtocolConfig::RelayHeader) => {
                backend_buffer_session = Some(backend_buffer);
                gauge_add!(names::protocol::PROXY_RELAY, 1);
                TcpStateMachine::RelayProxyProtocol(RelayProxyProtocol::new(
                    socket,
                    frontend_token,
                    request_id,
                    None,
                    frontend_buffer,
                ))
            }
            Some(ProxyProtocolConfig::ExpectHeader) => {
                frontend_buffer_session = Some(frontend_buffer);
                backend_buffer_session = Some(backend_buffer);
                gauge_add!(names::protocol::PROXY_EXPECT, 1);
                TcpStateMachine::ExpectProxyProtocol(ExpectProxyProtocol::new(
                    container_frontend_timeout.clone(),
                    socket,
                    frontend_token,
                    request_id,
                ))
            }
            Some(ProxyProtocolConfig::SendHeader) => {
                frontend_buffer_session = Some(frontend_buffer);
                backend_buffer_session = Some(backend_buffer);
                gauge_add!(names::protocol::PROXY_SEND, 1);
                TcpStateMachine::SendProxyProtocol(SendProxyProtocol::new(
                    socket,
                    frontend_token,
                    request_id,
                    None,
                ))
            }
            None => {
                gauge_add!(names::protocol::TCP, 1);
                let mut pipe = Pipe::new(
                    backend_buffer,
                    backend_id.clone(),
                    None,
                    None,
                    None,
                    None,
                    cluster_id.clone(),
                    frontend_buffer,
                    frontend_token,
                    socket,
                    listener.clone(),
                    Protocol::TCP,
                    request_id,
                    request_id,
                    frontend_address,
                    WebSocketContext::Tcp,
                );
                pipe.set_cluster_id(cluster_id.clone());
                pipe.set_cluster_metrics_incarnation(cluster_metrics_incarnation);
                TcpStateMachine::Pipe(pipe)
            }
        };

        let metrics = SessionMetrics::new(Some(wait_time));
        //FIXME: timeout usage

        TcpSession {
            backend_buffer: backend_buffer_session,
            backend_connected: BackendConnectionStatus::NotConnected,
            backend_id,
            backend_address: None,
            backend_token: None,
            backend: None,
            cluster_id,
            cluster_metrics_incarnation,
            configured_backend_timeout,
            connection_attempt: 0,
            container_backend_timeout,
            container_frontend_timeout,
            frontend_address,
            frontend_buffer: frontend_buffer_session,
            frontend_token,
            has_been_closed: false,
            last_event: Instant::now(),
            listener,
            metrics,
            proxy,
            request_id,
            state,
            cluster_ip_tracked: false,
            routed_sni: None,
            routed_alpn_label: None,
            tags_key: None,
        }
    }

    /// Construct a session that starts in [`TcpStateMachine::SniPreread`]
    /// instead of resolving a `proxy_protocol` up front -- the cluster (and
    /// therefore the per-cluster `proxy_protocol`) is only known once
    /// [`crate::protocol::tcp_preread::SniPrereadCore`] decides a route.
    /// Mirrors [`Self::new`]'s tail; kept as a separate constructor rather
    /// than folding a synthetic sentinel into `proxy_protocol:
    /// Option<ProxyProtocolConfig>` (a proto-generated enum this crate does
    /// not own).
    #[allow(clippy::too_many_arguments)]
    fn new_sni_preread(
        backend_buffer: Checkout,
        configured_backend_timeout: Duration,
        configured_connect_timeout: Duration,
        frontend_buffer: Checkout,
        frontend_token: Token,
        listener: Rc<RefCell<TcpListener>>,
        proxy: Rc<RefCell<TcpProxy>>,
        socket: MioTcpStream,
        peer: SocketAddr,
        wait_time: Duration,
        preread_timeout: Duration,
        effective_max_bytes: usize,
    ) -> TcpSession {
        // The address `accept(2)` returned with `socket`.
        let frontend_address = Some(peer);
        let request_id = Ulid::generate();

        // Armed with the SHORT preread timeout directly (not the listener's
        // configured front_timeout) -- `upgrade_sni_preread` restores the
        // configured duration on the SAME container once routed, so there is
        // exactly one `TimeoutContainer` for the frontend token throughout,
        // never a diverging clone (a clone independently rearmed to a
        // shorter duration would strand `TcpSession::readable`'s own
        // unconditional `reset()` on a since-cancelled timer-wheel entry).
        let container_frontend_timeout = TimeoutContainer::new(preread_timeout, frontend_token);
        let container_backend_timeout = TimeoutContainer::new_empty(configured_connect_timeout);

        let state = TcpStateMachine::SniPreread(SniPreread::new(
            socket,
            frontend_token,
            request_id,
            frontend_buffer,
            effective_max_bytes,
        ));

        // Enter the `SniPreread` state: +1 the active gauge exactly once, and
        // unconditionally, so every one of the two `-1` decrements has a
        // matching increment. The gauge is decremented on precisely one of the
        // two mutually-exclusive exits: the "upgrade" exit in
        // `upgrade_sni_preread` (which first transitions `self.state` away from
        // `SniPreread`, so `close()` cannot re-decrement), and the
        // "reject"/"teardown" exit in `close()`'s `StateMarker::SniPreread`
        // arm. A session therefore nets to 0 and never underflows.
        gauge_add!(names::tcp::sni_preread::ACTIVE, 1);

        let metrics = SessionMetrics::new(Some(wait_time));

        TcpSession {
            backend_buffer: Some(backend_buffer),
            backend_connected: BackendConnectionStatus::NotConnected,
            backend_id: None,
            backend_address: None,
            backend_token: None,
            backend: None,
            cluster_id: None,
            cluster_metrics_incarnation: None,
            configured_backend_timeout,
            connection_attempt: 0,
            container_backend_timeout,
            container_frontend_timeout,
            frontend_address,
            frontend_buffer: None,
            frontend_token,
            has_been_closed: false,
            last_event: Instant::now(),
            listener,
            metrics,
            proxy,
            request_id,
            state,
            cluster_ip_tracked: false,
            routed_sni: None,
            routed_alpn_label: None,
            tags_key: None,
        }
    }

    /// Source-IP for per-(cluster, source-IP) accounting.
    ///
    /// Prefer the parsed PROXY-v2 source from whichever upgrade phase is
    /// in flight, then the post-upgrade `Pipe.session_address`, finally
    /// the raw TCP `peer_addr` captured at session creation. The
    /// `Pipe::session_address` itself is already PROXY-v2-aware after
    /// `expect.rs::into_pipe` and `relay.rs::into_pipe`.
    fn effective_session_address(&self) -> Option<SocketAddr> {
        match &self.state {
            TcpStateMachine::Pipe(pipe) => pipe.get_session_address(),
            TcpStateMachine::ExpectProxyProtocol(epp) => {
                epp.addresses.as_ref().and_then(|pa| pa.source())
            }
            TcpStateMachine::RelayProxyProtocol(rpp) => {
                rpp.addresses.as_ref().and_then(|pa| pa.source())
            }
            TcpStateMachine::SniPreread(preread) => preread.outcome().and_then(|o| o.proxy_source),
            TcpStateMachine::SendProxyProtocol(_) | TcpStateMachine::FailedUpgrade(_) => None,
        }
        .or(self.frontend_address)
    }

    fn log_request(&self) {
        let listener = self.listener.borrow();
        let context = self.log_context();
        self.metrics
            .register_end_of_session_for_incarnation(&context, self.cluster_metrics_incarnation);
        // SNI-routed sessions carry the matched front's own tags key
        // (`sni_tags_key`, stashed by `upgrade_sni_preread`); everything
        // else keeps the historical bare-address key.
        let address_key = TcpFrontendTagsKey::Address(*listener.get_addr()).to_string();
        let tags_key = self.tags_key.as_deref().unwrap_or(&address_key);
        info_access!(
            on_failure: { incr!(names::access_logs::UNSENT) },
            message: None,
            context,
            session_address: self.frontend_address,
            backend_address: None,
            protocol: "TCP",
            endpoint: EndpointRecord::Tcp,
            tags: listener.get_tags(tags_key),
            client_rtt: socket_rtt(self.state.front_socket()),
            server_rtt: None,
            user_agent: None,
            x_request_id: None,
            // Sōzu never terminates TLS on the TCP path (the frontend is a
            // raw `MioTcpStream`), so no negotiated version/cipher exists.
            // A preread SNI/ALPN (SNI-routed listeners) is stamped on the
            // `Pipe`'s own access log via `set_tls_metadata` at upgrade
            // time; this pre-Pipe log site emits `None` for all four TLS
            // fields and the parsed XFF chain.
            tls_version: None,
            tls_cipher: None,
            tls_sni: None,
            tls_alpn: None,
            xff_chain: None,
            service_time: self.metrics.service_time(),
            response_time: self.metrics.backend_response_time(),
            request_time: self.metrics.request_time(),
            start_time_ns: self.metrics.start_wall_ns(),
            bytes_in: self.metrics.bin,
            bytes_out: self.metrics.bout,
            otel: None,
        );
    }

    fn front_hup(&mut self) -> SessionResult {
        let listener = self.listener.borrow();
        match &mut self.state {
            TcpStateMachine::Pipe(pipe) => pipe.frontend_hup(&mut self.metrics),
            // No access log here, mirroring `readable()`'s own error paths
            // for the other pre-Pipe states (none of them call
            // `log_request()` either): the shell itself decides silent vs.
            // metered based on whether any bytes were ever received.
            TcpStateMachine::SniPreread(preread) => {
                let cfg = listener.preread_config(preread.effective_max_bytes());
                preread.on_front_closed(&cfg);
                SessionResult::Close
            }
            _ => {
                self.log_request();
                SessionResult::Close
            }
        }
    }

    fn back_hup(&mut self) -> SessionResult {
        // `SniPreread` falls into the wildcard catch-all below (unconditional
        // close + access log), same as Send/Relay/Expect: a backend HUP
        // while still prereading is an ordinary connect-time failure with no
        // preread-specific accounting to do (the core only ever reasons
        // about frontend bytes).
        match &mut self.state {
            TcpStateMachine::Pipe(pipe) => pipe.backend_hup(&mut self.metrics),
            _ => {
                self.log_request();
                SessionResult::Close
            }
        }
    }

    fn log_context(&self) -> LogContext<'_> {
        LogContext {
            session_id: self.request_id,
            request_id: Some(self.request_id),
            cluster_id: self.cluster_id.as_deref(),
            backend_id: self.backend_id.as_deref(),
        }
    }

    fn readable(&mut self) -> SessionResult {
        // The absolute SNI-preread deadline (armed once, at session
        // creation) must stand while undecided -- see
        // `frontend_timeout_resets_on_readable`'s doc.
        if frontend_timeout_resets_on_readable(&self.state)
            && !self.container_frontend_timeout.reset()
        {
            error!(
                "{} Could not reset frontend timeout on readable",
                log_context!(self)
            );
        }
        if self.backend_connected == BackendConnectionStatus::Connected
            && !self.container_backend_timeout.reset()
        {
            error!(
                "{} Could not reset backend timeout on readable",
                log_context!(self)
            );
        }
        let listener = self.listener.borrow();
        let result = match &mut self.state {
            TcpStateMachine::Pipe(pipe) => pipe.readable(&mut self.metrics),
            TcpStateMachine::RelayProxyProtocol(pp) => pp.readable(&mut self.metrics),
            TcpStateMachine::ExpectProxyProtocol(pp) => pp.readable(&mut self.metrics),
            TcpStateMachine::SendProxyProtocol(_) => SessionResult::Continue,
            TcpStateMachine::SniPreread(preread) => {
                let cfg = listener.preread_config(preread.effective_max_bytes());
                preread.readable(&mut self.metrics, &cfg)
            }
            TcpStateMachine::FailedUpgrade(_) => unreachable!(),
        };
        drop(listener);

        // Sync `cluster_id` the moment SNI preread lands a route, so
        // `connect_to_backend`'s cluster source (`self.cluster_id.clone().or_else(...)`)
        // sees it without waiting for a second dispatch.
        if let TcpStateMachine::SniPreread(preread) = &self.state
            && self.cluster_id.is_none()
            && let Some(outcome) = preread.outcome()
        {
            self.cluster_id = Some(outcome.cluster.clone());
            self.cluster_metrics_incarnation =
                crate::metrics::cluster_incarnation(outcome.cluster.as_ref());
            // Restore the listener's configured `front_timeout` THE MOMENT
            // routing succeeds, not only once the backend connect completes
            // (previously done only in `upgrade_sni_preread`, which can run
            // one or more `ready()` cycles later): a slow-but-legitimate
            // backend connect must be bounded by
            // `front_timeout`/`connect_timeout`, never by the short
            // `sni_preread_timeout` that only makes sense while a route
            // decision is still pending (sozu-proxy/sozu#1290). This is
            // also the point from which
            // `frontend_timeout_resets_on_readable` starts resetting this
            // container again on every future `readable()`.
            self.container_frontend_timeout
                .set_duration(Duration::from_secs(
                    self.listener.borrow().config.front_timeout as u64,
                ));
        }

        result
    }

    fn writable(&mut self) -> SessionResult {
        match &mut self.state {
            TcpStateMachine::Pipe(pipe) => pipe.writable(&mut self.metrics),
            _ => SessionResult::Continue,
        }
    }

    fn back_readable(&mut self) -> SessionResult {
        if !self.container_frontend_timeout.reset() {
            error!(
                "{} Could not reset frontend timeout on back_readable",
                log_context!(self)
            );
        }
        if !self.container_backend_timeout.reset() {
            error!(
                "{} Could not reset backend timeout on back_readable",
                log_context!(self)
            );
        }

        match &mut self.state {
            TcpStateMachine::Pipe(pipe) => pipe.backend_readable(&mut self.metrics),
            _ => SessionResult::Continue,
        }
    }

    fn back_writable(&mut self) -> SessionResult {
        match &mut self.state {
            TcpStateMachine::Pipe(pipe) => pipe.backend_writable(&mut self.metrics),
            TcpStateMachine::RelayProxyProtocol(pp) => pp.back_writable(&mut self.metrics),
            TcpStateMachine::SendProxyProtocol(pp) => pp.back_writable(&mut self.metrics),
            // The FIRST backend-writable event while routed drives the
            // upgrade out of `SniPreread` -- see
            // `SniPreread::back_writable`'s doc and `upgrade_sni_preread`.
            TcpStateMachine::SniPreread(preread) => preread.back_writable(),
            TcpStateMachine::ExpectProxyProtocol(_) => SessionResult::Continue,
            TcpStateMachine::FailedUpgrade(_) => {
                unreachable!()
            }
        }
    }

    fn back_socket_mut(&mut self) -> Option<&mut MioTcpStream> {
        match &mut self.state {
            TcpStateMachine::Pipe(pipe) => pipe.back_socket_mut(),
            TcpStateMachine::SendProxyProtocol(pp) => pp.back_socket_mut(),
            TcpStateMachine::RelayProxyProtocol(pp) => pp.back_socket_mut(),
            TcpStateMachine::SniPreread(preread) => preread.back_socket_mut(),
            TcpStateMachine::ExpectProxyProtocol(_) => None,
            TcpStateMachine::FailedUpgrade(_) => unreachable!(),
        }
    }

    pub fn upgrade(&mut self) -> SessionIsToBeClosed {
        let new_state = match self.state.take() {
            TcpStateMachine::SendProxyProtocol(spp) => self.upgrade_send(spp),
            TcpStateMachine::RelayProxyProtocol(rpp) => self.upgrade_relay(rpp),
            TcpStateMachine::ExpectProxyProtocol(epp) => self.upgrade_expect(epp),
            TcpStateMachine::SniPreread(preread) => self.upgrade_sni_preread(preread),
            TcpStateMachine::Pipe(_) => None,
            TcpStateMachine::FailedUpgrade(_) => todo!(),
        };

        match new_state {
            Some(state) => {
                self.state = state;
                false
            } // The state stays FailedUpgrade, but the Session should be closed right after

            None => true,
        }
    }

    fn upgrade_send(
        &mut self,
        send_proxy_protocol: SendProxyProtocol<MioTcpStream>,
    ) -> Option<TcpStateMachine> {
        if self.backend_buffer.is_some() && self.frontend_buffer.is_some() {
            let mut pipe = send_proxy_protocol.into_pipe(
                self.frontend_buffer.take().unwrap(),
                self.backend_buffer.take().unwrap(),
                self.listener.clone(),
            );

            // `SendProxyProtocol::into_pipe` overwrites the whole readiness
            // (clobbering the backend-writable interest `Pipe::new` armed for a
            // non-empty inherited frontend buffer) and re-inserts only
            // READABLE. That is harmless for a legacy `SendHeader` session
            // (empty accumulator) but strands the coalesced payload tail
            // carried in from `SniPreread`'s `SendHeader` branch -- the
            // sozu-proxy/sozu#1279 close-before-flush truncation, where bytes
            // already queued for the backend were dropped instead of flushed.
            // Re-run the inherited-write arm by feeding the pipe's
            // own current events back through `restore_readiness_events`; it is
            // additive (never clears interest) and a no-op when the buffers are
            // empty, so the legacy path is unaffected.
            let frontend_event = pipe.frontend_readiness.event;
            let backend_event = pipe.backend_readiness.event;
            pipe.restore_readiness_events(frontend_event, backend_event);

            pipe.set_cluster_id(self.cluster_id.clone());
            pipe.set_cluster_metrics_incarnation(self.cluster_metrics_incarnation);
            pipe.set_backend_address(self.backend_address);
            // Only `Some` when this `SendProxyProtocol` was itself reached
            // via `upgrade_sni_preread`'s `SendHeader` branch (sozu-proxy/sozu#1279)
            // -- a legacy, non-SNI-routed `SendHeader` cluster never
            // populates these fields, so this is a no-op for it.
            if let Some(sni) = self.routed_sni.take() {
                pipe.set_tls_metadata(None, None, Some(sni), self.routed_alpn_label.take());
            }
            // `None` for legacy sessions (keeps the bare-address tags
            // lookup); the matched front's composed key for SNI-routed ones.
            pipe.set_tags_key(self.tags_key.clone());
            gauge_add!(names::protocol::PROXY_SEND, -1);
            gauge_add!(names::protocol::TCP, 1);
            return Some(TcpStateMachine::Pipe(pipe));
        }

        error!(
            "{} Missing the frontend or backend buffer queue, we can't switch to a pipe",
            log_context!(self)
        );
        None
    }

    fn upgrade_relay(&mut self, rpp: RelayProxyProtocol<MioTcpStream>) -> Option<TcpStateMachine> {
        if self.backend_buffer.is_some() {
            let mut pipe =
                rpp.into_pipe(self.backend_buffer.take().unwrap(), self.listener.clone());
            pipe.set_cluster_id(self.cluster_id.clone());
            pipe.set_cluster_metrics_incarnation(self.cluster_metrics_incarnation);
            pipe.set_backend_address(self.backend_address);
            gauge_add!(names::protocol::PROXY_RELAY, -1);
            gauge_add!(names::protocol::TCP, 1);
            return Some(TcpStateMachine::Pipe(pipe));
        }

        error!(
            "{} Missing the backend buffer queue, we can't switch to a pipe",
            log_context!(self)
        );
        None
    }

    fn upgrade_expect(
        &mut self,
        epp: ExpectProxyProtocol<MioTcpStream>,
    ) -> Option<TcpStateMachine> {
        if self.frontend_buffer.is_some() && self.backend_buffer.is_some() {
            let mut pipe = epp.into_pipe(
                self.frontend_buffer.take().unwrap(),
                self.backend_buffer.take().unwrap(),
                None,
                None,
                self.listener.clone(),
            );

            pipe.set_cluster_id(self.cluster_id.clone());
            pipe.set_cluster_metrics_incarnation(self.cluster_metrics_incarnation);
            pipe.set_backend_address(self.backend_address);
            gauge_add!(names::protocol::PROXY_EXPECT, -1);
            gauge_add!(names::protocol::TCP, 1);
            return Some(TcpStateMachine::Pipe(pipe));
        }

        error!(
            "{} Missing the backend buffer queue, we can't switch to a pipe",
            log_context!(self)
        );
        None
    }

    /// Dispatch out of [`TcpStateMachine::SniPreread`] once its backend has
    /// connected, by the ROUTED cluster's `proxy_protocol` config:
    ///
    /// - `Some(SendHeader)` -> `SendProxyProtocol` synthesizes its OWN PPv2
    ///   header for the backend, so any inbound PPv2 prefix this listener's
    ///   `expect_proxy` preread already parsed (`content_offset` bytes) is
    ///   dropped from the accumulator first: the wire order is `[synth
    ///   PPv2][ClientHello...]`, never both headers back to back.
    /// - `Some(ExpectHeader)` -> the inbound PPv2 prefix is consumed the
    ///   same way (Sōzu terminates it locally; the backend never sees a
    ///   PROXY header at all), then straight into `Pipe`.
    /// - `Some(RelayHeader)` -> NO consume: the already-parsed inbound
    ///   header bytes ARE the header this backend expects, replayed
    ///   verbatim ahead of the ClientHello.
    /// - `None` -> also consumed: a listener with `expect_proxy` but a
    ///   `None`-proxy_protocol cluster still must not leak the stray
    ///   inbound PPv2 prefix onto a backend that expects none -- the
    ///   preread parsed those bytes for routing only, and a backend with
    ///   no PROXY-protocol contract would read them as part of the TLS
    ///   stream.
    ///
    /// `tcp.sni_preread.duration` is recorded on EVERY exit from this
    /// function, including the four defensive early returns below: once
    /// `preread.outcome()`/`preread`'s `SniPreread` value is consumed by the
    /// `TcpStateMachine::FailedUpgrade`/`Pipe` transition its caller drives,
    /// `close()`'s `StateMarker::SniPreread` arm can no longer reach a
    /// `SniPreread` to read `started_at()` from, so recording later is not an
    /// option. `tcp.sni_preread.active`, by contrast, is decremented exactly
    /// once, on the "upgrade" exit named by the gauge's `-1 on every exit`
    /// contract; the "reject"/"teardown" exits are each other's counterpart
    /// in `SniPreread::handle_output` (metric only) and `TcpSession::close`'s
    /// `StateMarker::SniPreread` arm (gauge).
    /// Shared abort path for `upgrade_sni_preread`'s early-return guards:
    /// logs `reason` through the same envelope as the rest of this module,
    /// then records `tcp.sni_preread.duration` -- see the long comment on
    /// `upgrade_sni_preread` for why that metric must fire on every exit --
    /// before returning `None`.
    fn abort_sni_preread_upgrade(
        &self,
        preread: &SniPreread<MioTcpStream>,
        reason: &str,
    ) -> Option<TcpStateMachine> {
        error!("{} {}", log_context!(self), reason);
        time!(
            names::tcp::sni_preread::DURATION,
            preread.started_at().elapsed().as_millis() as i64
        );
        None
    }

    fn upgrade_sni_preread(
        &mut self,
        mut preread: SniPreread<MioTcpStream>,
    ) -> Option<TcpStateMachine> {
        // Every early return below (a route decision missing, or the
        // backend socket/token/buffer not yet wired) must happen BEFORE the
        // `tcp.sni_preread.active` gauge is touched: `close()`'s
        // `StateMarker::SniPreread` arm runs unconditionally whenever
        // `self.state` is still (or, via `FailedUpgrade`, was last)
        // `SniPreread` -- decrementing here AND there for the same session
        // would underflow the gauge on this (defensive, should-never-happen)
        // failure path. The gauge is deferred to `close()` on these paths,
        // but the duration is NOT: it is recorded right before each `return
        // None` below, symmetric with the success path's `time!` call.
        let Some(outcome) = preread.outcome().cloned() else {
            return self.abort_sni_preread_upgrade(
                &preread,
                "upgrade_sni_preread called before a route decision",
            );
        };
        let Some(backend_socket) = preread.backend.take() else {
            return self.abort_sni_preread_upgrade(
                &preread,
                "SNI preread upgrade with no backend socket set",
            );
        };
        let Some(backend_token) = preread.backend_token else {
            return self.abort_sni_preread_upgrade(
                &preread,
                "SNI preread upgrade with no backend token set",
            );
        };
        let Some(back_buffer) = self.backend_buffer.take() else {
            return self.abort_sni_preread_upgrade(
                &preread,
                "SNI preread upgrade with no backend buffer queued",
            );
        };

        gauge_add!(names::tcp::sni_preread::ACTIVE, -1);
        time!(
            names::tcp::sni_preread::DURATION,
            preread.started_at().elapsed().as_millis() as i64
        );

        self.cluster_id = Some(outcome.cluster.clone());
        self.cluster_metrics_incarnation =
            crate::metrics::cluster_incarnation(outcome.cluster.as_ref());
        // `container_frontend_timeout` is NOT restored here anymore: by the
        // time this runs, `TcpSession::readable`'s route-capture block has
        // already restored it to the listener's configured `front_timeout`
        // the moment the route decision first became visible (potentially
        // one or more `ready()` cycles before this upgrade, while the
        // backend was still connecting) -- see that block's doc
        // (sozu-proxy/sozu#1290). Restoring it again here
        // would just re-arm the same duration a second time.
        // Access-log tagging: stash the routed SNI/ALPN for
        // whichever of the four `proxy_protocol` branches below eventually
        // reaches `Pipe` -- immediately via `build_pipe_from_preread` for
        // `Expect`/`Relay`/`None`, or one `ready()` cycle later via
        // `upgrade_send` for `SendHeader` (see that method and the
        // `routed_sni` field doc).
        self.routed_sni = Some(outcome.sni.clone());
        self.routed_alpn_label = known_alpn_label(&outcome.alpn);
        // Rebuild the MATCHED front's tags key from the route decision's
        // identity (`matched_sni_pattern` is the trie key — the configured
        // pattern, not the client's concrete SNI — and `matched_alpn` the
        // winning matcher), so the access log emits the tags of the front
        // that actually routed this session, not whichever front was added
        // last. Must compose the same key `add_tcp_front` stored — see
        // `sni_tags_key`'s canonical-form doc.
        self.tags_key = Some(sni_tags_key(
            self.listener.borrow().get_addr(),
            &outcome.matched_sni_pattern,
            &alpn_matcher_protocols(&outcome.matched_alpn),
        ));

        let proxy_protocol = self
            .proxy
            .borrow()
            .configs
            .get(&outcome.cluster)
            .and_then(|c| c.proxy_protocol);

        let frontend_event = preread.frontend_readiness.event;
        let backend_event = preread.backend_readiness.event;
        let mut frontend_buffer = preread.frontend_buffer;
        let frontend = preread.frontend;
        let frontend_token = preread.frontend_token;
        let request_id = preread.request_id;

        match proxy_protocol {
            Some(ProxyProtocolConfig::SendHeader) => {
                frontend_buffer.consume(outcome.content_offset);
                self.frontend_buffer = Some(frontend_buffer);
                self.backend_buffer = Some(back_buffer);
                gauge_add!(names::protocol::PROXY_SEND, 1);
                let mut spp = SendProxyProtocol::new(
                    frontend,
                    frontend_token,
                    request_id,
                    Some(backend_socket),
                );
                spp.frontend_readiness.event = frontend_event;
                spp.backend_readiness.event = backend_event;
                spp.set_back_token(backend_token);
                spp.set_back_connected(BackendConnectionStatus::Connected);
                Some(TcpStateMachine::SendProxyProtocol(spp))
            }
            Some(ProxyProtocolConfig::ExpectHeader) | None => {
                frontend_buffer.consume(outcome.content_offset);
                Some(self.build_pipe_from_preread(
                    back_buffer,
                    frontend_buffer,
                    frontend,
                    frontend_token,
                    frontend_event,
                    backend_event,
                    backend_socket,
                    backend_token,
                    request_id,
                    outcome.proxy_source,
                    outcome.cluster,
                ))
            }
            Some(ProxyProtocolConfig::RelayHeader) => Some(self.build_pipe_from_preread(
                back_buffer,
                frontend_buffer,
                frontend,
                frontend_token,
                frontend_event,
                backend_event,
                backend_socket,
                backend_token,
                request_id,
                outcome.proxy_source,
                outcome.cluster,
            )),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn build_pipe_from_preread(
        &mut self,
        back_buffer: Checkout,
        frontend_buffer: Checkout,
        frontend: MioTcpStream,
        frontend_token: Token,
        frontend_event: Ready,
        backend_event: Ready,
        backend_socket: MioTcpStream,
        backend_token: Token,
        request_id: Ulid,
        proxy_source: Option<SocketAddr>,
        cluster_id: ClusterId,
    ) -> TcpStateMachine {
        let addr = proxy_source.or(self.frontend_address);
        let mut pipe = Pipe::new(
            back_buffer,
            self.backend_id.clone(),
            Some(backend_socket),
            None,
            None,
            None,
            Some(cluster_id),
            frontend_buffer,
            frontend_token,
            frontend,
            self.listener.clone(),
            Protocol::TCP,
            request_id,
            request_id,
            addr,
            WebSocketContext::Tcp,
        );
        pipe.set_cluster_metrics_incarnation(self.cluster_metrics_incarnation);
        // `Pipe::new` armed backend-writable for the inherited frontend
        // accumulator (the ClientHello + any coalesced payload) via
        // `arm_inherited_buffer_writes`. Restore the preread's readiness
        // events through `restore_readiness_events` rather than a bare
        // `pipe.frontend_readiness.event = …` / `pipe.backend_readiness.event
        // = …` pair: it sets both `.event`s and THEN re-runs the inherited
        // arm, so the synthetic backend-writable event survives even in the
        // case where the restored `backend_event` does not itself carry
        // WRITABLE (the byte-for-byte drain of the accumulator must not depend
        // on that). The eventual flush-on-close of that accumulator is
        // guaranteed by `Pipe::readable`'s half-close drain (sozu-proxy/sozu#1279).
        pipe.restore_readiness_events(frontend_event, backend_event);
        pipe.set_back_token(backend_token);
        pipe.set_backend_address(self.backend_address);
        // Access-log tagging: reaching `Pipe` straight from
        // `SniPreread` (Expect/Relay/None) -- unlike `SendHeader`, which
        // detours through `SendProxyProtocol` first (see `upgrade_send`).
        if let Some(sni) = self.routed_sni.take() {
            pipe.set_tls_metadata(None, None, Some(sni), self.routed_alpn_label.take());
        }
        // The matched front's composed tags key (always `Some` here — this
        // is only reachable from `upgrade_sni_preread`, which just set it).
        pipe.set_tags_key(self.tags_key.clone());
        gauge_add!(names::protocol::TCP, 1);
        TcpStateMachine::Pipe(pipe)
    }

    fn front_readiness(&mut self) -> &mut Readiness {
        match &mut self.state {
            TcpStateMachine::Pipe(pipe) => &mut pipe.frontend_readiness,
            TcpStateMachine::SendProxyProtocol(pp) => &mut pp.frontend_readiness,
            TcpStateMachine::RelayProxyProtocol(pp) => &mut pp.frontend_readiness,
            TcpStateMachine::ExpectProxyProtocol(pp) => &mut pp.frontend_readiness,
            TcpStateMachine::SniPreread(preread) => &mut preread.frontend_readiness,
            TcpStateMachine::FailedUpgrade(_) => unreachable!(),
        }
    }

    fn back_readiness(&mut self) -> Option<&mut Readiness> {
        match &mut self.state {
            TcpStateMachine::Pipe(pipe) => Some(&mut pipe.backend_readiness),
            TcpStateMachine::SendProxyProtocol(pp) => Some(&mut pp.backend_readiness),
            TcpStateMachine::RelayProxyProtocol(pp) => Some(&mut pp.backend_readiness),
            TcpStateMachine::SniPreread(preread) => Some(&mut preread.backend_readiness),
            TcpStateMachine::ExpectProxyProtocol(_) => None,
            TcpStateMachine::FailedUpgrade(_) => unreachable!(),
        }
    }

    fn set_back_socket(&mut self, socket: MioTcpStream) {
        match &mut self.state {
            TcpStateMachine::Pipe(pipe) => pipe.set_back_socket(socket),
            TcpStateMachine::SendProxyProtocol(pp) => pp.set_back_socket(socket),
            TcpStateMachine::RelayProxyProtocol(pp) => pp.set_back_socket(socket),
            TcpStateMachine::SniPreread(preread) => preread.set_back_socket(socket),
            TcpStateMachine::ExpectProxyProtocol(_) => {
                error!(
                    "{} We should not set the back socket for the expect proxy protocol",
                    log_context!(self)
                );
                panic!(
                    "{} We should not set the back socket for the expect proxy protocol",
                    log_context!(self)
                );
            }
            TcpStateMachine::FailedUpgrade(_) => unreachable!(),
        }
    }

    fn set_back_token(&mut self, token: Token) {
        // The frontend must own a token distinct from the backend's: the two
        // index different slab slots, so wiring the same token to both would
        // alias two sessions onto one slot.
        debug_assert_ne!(
            token, self.frontend_token,
            "backend token must differ from the frontend token"
        );
        self.backend_token = Some(token);

        match &mut self.state {
            TcpStateMachine::Pipe(pipe) => pipe.set_back_token(token),
            TcpStateMachine::SendProxyProtocol(pp) => pp.set_back_token(token),
            TcpStateMachine::SniPreread(preread) => preread.set_back_token(token),
            TcpStateMachine::RelayProxyProtocol(pp) => pp.set_back_token(token),
            TcpStateMachine::ExpectProxyProtocol(_) => self.backend_token = Some(token),
            TcpStateMachine::FailedUpgrade(_) => unreachable!(),
        }

        // Postcondition: the session now owns exactly the token it was asked
        // to register — every arm above (including the Expect arm, which only
        // stores the session-side token) leaves `backend_token == Some(token)`.
        debug_assert_eq!(
            self.backend_token,
            Some(token),
            "set_back_token must leave the session owning the registered token"
        );
    }

    fn set_backend_id(&mut self, id: String) {
        self.backend_id = Some(id.clone());
        if let TcpStateMachine::Pipe(pipe) = &mut self.state {
            pipe.set_backend_id(Some(id));
        }
    }

    /// Record the dialed backend's address; a session that is not a `Pipe`
    /// yet hands it over at upgrade time instead.
    fn set_backend_address(&mut self, address: SocketAddr) {
        self.backend_address = Some(address);
        if let TcpStateMachine::Pipe(pipe) = &mut self.state {
            pipe.set_backend_address(Some(address));
        }
    }

    fn back_connected(&self) -> BackendConnectionStatus {
        self.backend_connected
    }

    fn set_back_connected(&mut self, status: BackendConnectionStatus) {
        let last = self.backend_connected;
        // Transitioning INTO `Connected` bumps the backend-connection gauge by
        // exactly +1. Doing so from an already-`Connected` state would
        // double-count (gauge drift that only `close_backend`'s single -1
        // would later reconcile, leaving the gauge permanently +1). The
        // promotion always comes from a `Connecting` (the normal handshake
        // completion in `ready_inner`) — never from `Connected` itself.
        debug_assert!(
            status != BackendConnectionStatus::Connected
                || last != BackendConnectionStatus::Connected,
            "set_back_connected(Connected) must not run on an already-Connected backend (gauge would double-count)"
        );
        self.backend_connected = status;

        // Postcondition: the requested status is now in effect.
        debug_assert_eq!(
            self.backend_connected, status,
            "set_back_connected must record the requested status"
        );

        if status == BackendConnectionStatus::Connected {
            gauge_add!(names::backend::CONNECTIONS, 1);
            gauge_add!(
                names::backend::CONNECTIONS_PER_BACKEND,
                1,
                self.cluster_id.as_deref(),
                self.metrics.backend_id.as_deref(),
                self.cluster_metrics_incarnation
            );

            // the back timeout was of connect_timeout duration before,
            // now that we're connected, move to backend_timeout duration
            self.container_backend_timeout
                .set_duration(self.configured_backend_timeout);
            self.container_frontend_timeout.reset();

            if let TcpStateMachine::SendProxyProtocol(spp) = &mut self.state {
                spp.set_back_connected(BackendConnectionStatus::Connected);
            }

            if let Some(backend) = self.backend.as_ref() {
                let mut backend = backend.borrow_mut();

                if backend.retry_policy.is_down() {
                    incr!(
                        names::backend::UP,
                        self.cluster_id.as_deref(),
                        self.metrics.backend_id.as_deref(),
                        self.cluster_metrics_incarnation
                    );
                    gauge!(
                        names::backend::AVAILABLE,
                        1,
                        self.cluster_id.as_deref(),
                        self.metrics.backend_id.as_deref(),
                        self.cluster_metrics_incarnation
                    );
                    info!(
                        "{} backend server {} at {} is up",
                        log_context!(self),
                        backend.backend_id,
                        backend.address
                    );
                    push_event(Event {
                        kind: EventKind::BackendUp as i32,
                        backend_id: Some(backend.backend_id.to_owned()),
                        address: Some(backend.address.into()),
                        cluster_id: None,
                        metric_detail: None,
                    });
                }

                if let BackendConnectionStatus::Connecting(start) = last {
                    let now = Instant::now();
                    backend.set_connection_time(now - start, now);
                }

                //successful connection, rest failure counter
                backend.failures = 0;
                backend.retry_policy.succeed(Instant::now());
            }
        }
    }

    fn remove_backend(&mut self) {
        if let Some(backend) = self.backend.take() {
            (*backend.borrow_mut()).dec_connections();
        }

        self.backend_token = None;

        // Postcondition: the backend handle and its token are torn down
        // together — neither may outlive the other (a dangling token would
        // leave a stale slab reference; a dangling handle would over-count
        // backend connections).
        debug_assert!(
            self.backend.is_none(),
            "remove_backend must release the backend handle"
        );
        debug_assert!(
            self.backend_token.is_none(),
            "remove_backend must clear the backend token"
        );
    }

    fn fail_backend_connection(&mut self) {
        if let Some(backend) = self.backend.as_ref() {
            let backend = &mut *backend.borrow_mut();
            backend.failures += 1;

            let already_unavailable = backend.retry_policy.is_down();
            // The TCP proxy is not simulated: it keeps the wall clock and the
            // thread-local generator at its own edge (#1684).
            backend.retry_policy.fail(Instant::now(), &mut rand::rng());
            incr!(
                names::backend::CONNECTIONS_ERROR,
                self.cluster_id.as_deref(),
                self.metrics.backend_id.as_deref(),
                self.cluster_metrics_incarnation
            );
            if !already_unavailable && backend.retry_policy.is_down() {
                error!(
                    "{} backend server {} at {} is down",
                    log_context!(self),
                    backend.backend_id,
                    backend.address
                );
                incr!(
                    names::backend::DOWN,
                    self.cluster_id.as_deref(),
                    self.metrics.backend_id.as_deref(),
                    self.cluster_metrics_incarnation
                );
                gauge!(
                    names::backend::AVAILABLE,
                    0,
                    self.cluster_id.as_deref(),
                    self.metrics.backend_id.as_deref(),
                    self.cluster_metrics_incarnation
                );

                push_event(Event {
                    kind: EventKind::BackendDown as i32,
                    backend_id: Some(backend.backend_id.to_owned()),
                    address: Some(backend.address.into()),
                    cluster_id: None,
                    metric_detail: None,
                });
            }
        }
    }

    pub fn test_back_socket(&mut self) -> SessionIsToBeClosed {
        match self.back_socket_mut() {
            Some(ref mut s) => {
                let mut tmp = [0u8; 1];
                let res = s.peek(&mut tmp[..]);

                match res {
                    // if the socket is half open, it will report 0 bytes read (EOF)
                    Ok(0) => false,
                    Ok(_) => true,
                    Err(e) => matches!(e.kind(), std::io::ErrorKind::WouldBlock),
                }
            }
            None => false,
        }
    }

    pub fn cancel_timeouts(&mut self) {
        self.container_frontend_timeout.cancel();
        self.container_backend_timeout.cancel();
    }

    /// Full cross-field invariant sweep for the TCP session state machine.
    ///
    /// Run as a run-to-completion postcondition at the END of `ready()` (the
    /// only public entry point that drives the front/back token + readiness
    /// state machine). These are OUR-logic invariants — never reachable from
    /// hostile traffic — so a violation is a bug in Sōzu, not a malformed
    /// peer. Compiled out in release.
    #[cfg(debug_assertions)]
    fn check_invariants(&self) {
        // Connection-attempt budget: every retry path increments
        // `connection_attempt` and `connect_to_backend` refuses once the
        // counter reaches the cluster's `max_connection_attempts`, so the
        // value never exceeds the largest budget that can be configured (and
        // resets to 0 on success). The live budget itself is no bound here:
        // `SetMaxConnectionAttempts` or an `AddCluster` may lower it while a
        // session is retrying.
        debug_assert!(
            u32::from(self.connection_attempt)
                <= sozu_command::config::MAX_CONNECTION_ATTEMPTS_UPPER_BOUND,
            "connection_attempt ({}) must never exceed the largest attempt budget",
            self.connection_attempt,
        );

        // Token ownership: a fully-connected backend always owns a backend
        // token (set by `set_back_token` during `connect_to_backend`, before
        // the status can ever flip to `Connected`). The `Connecting` phase is
        // deliberately excluded: there is a transient window inside
        // `connect_to_backend` where the status is `Connecting` but the token
        // has not been wired yet — that window never spans a `ready()`
        // boundary, so the postcondition still holds here.
        if self.backend_connected == BackendConnectionStatus::Connected {
            debug_assert!(
                self.backend_token.is_some(),
                "a Connected backend must own a backend token"
            );
        }

        // A live backend handle implies the matching token is present: the
        // two are wired together in `connect_to_backend` and torn down
        // together in `remove_backend` (which clears the token) — they must
        // never drift apart. (For the pure-TCP proxy `backend` is currently
        // always `None`, so this is a guard against a future regression that
        // starts populating it without the token.)
        if self.backend.is_some() {
            debug_assert!(
                self.backend_token.is_some(),
                "a live backend handle must have a backend token"
            );
        }

        // Once the session has been closed it is terminal: the backend has
        // been released and the per-(cluster, source-IP) slot untracked.
        if self.has_been_closed {
            debug_assert!(
                self.backend.is_none(),
                "a closed session must have released its backend handle"
            );
            debug_assert!(
                !self.cluster_ip_tracked,
                "a closed session must have untracked its (cluster, source-IP) slot"
            );
        }
    }

    /// Attempt a fresh backend connect, exactly like `ready_inner`'s
    /// top-of-function gate -- but callable a second time from inside the
    /// dispatch loop. A `SniPreread` session's route decision can complete
    /// INSIDE `readable()`'s own dispatch (the SAME `ready_inner` call), and
    /// without a second attempt right after that dispatch the session would
    /// stall until an unrelated readiness event re-entered `ready_inner`.
    ///
    /// A no-op whenever `back_connected() != NotConnected` (already
    /// attempted, or backend already up), the state is a NOT-YET-ROUTED
    /// `SniPreread` (the cluster -- and therefore the backend to dial -- is
    /// unknown until `SniPrereadCore` decides), or the state is still
    /// `ExpectProxyProtocol` (see below), so this changes nothing for any
    /// other pre-existing state/path.
    ///
    /// `ExpectProxyProtocol` has NO backend side at all: `back_readiness`
    /// returns `None` for it and `set_back_socket` panics outright ("We
    /// should not set the back socket for the expect proxy protocol"). The
    /// backend is dialed from the `Pipe` that `upgrade_expect` installs once
    /// the inbound PROXY header has been parsed -- the same `ready()` pass
    /// re-enters `ready_inner` after the upgrade, so nothing is deferred
    /// beyond the header. Without this arm the top-of-`ready_inner` connect
    /// gate fired on the FIRST readiness event of a freshly accepted socket
    /// (the WRITABLE epoll reports before the client has sent a byte) and
    /// drove `connect_to_backend` straight into that panic, killing the
    /// worker and every other session on it.
    fn attempt_backend_connect_if_needed(
        &mut self,
        session: &Rc<RefCell<dyn ProxySession>>,
    ) -> Option<SessionResult> {
        if self.back_connected() != BackendConnectionStatus::NotConnected {
            return None;
        }
        if matches!(&self.state, TcpStateMachine::SniPreread(preread) if !preread.is_routed()) {
            return None;
        }
        if matches!(&self.state, TcpStateMachine::ExpectProxyProtocol(_)) {
            return None;
        }

        let connection_result = self.connect_to_backend(session.clone());
        if let Err(err) = &connection_result {
            match err {
                // Already logged at warn! + metered at the retry-budget
                // gate in connect_to_backend; avoid double-emission.
                BackendConnectionError::MaxConnectionRetries(_) => trace!(
                    "{} Error connecting to backend: {}",
                    log_context!(self),
                    err
                ),
                _ => warn!(
                    "{} Error connecting to backend: {}",
                    log_context!(self),
                    err
                ),
            }
        }
        handle_connection_result(connection_result)
    }

    fn ready_inner(&mut self, session: Rc<RefCell<dyn ProxySession>>) -> SessionResult {
        let mut counter = 0;

        let back_connected = self.back_connected();
        if back_connected.is_connecting() {
            // A `Connecting` backend always carries a backend readiness:
            // `connect_to_backend` is the only writer of that status, and it
            // never runs for the two states `back_readiness` has none for
            // (`ExpectProxyProtocol`, guarded in
            // `attempt_backend_connect_if_needed`; `FailedUpgrade`, which
            // `close()` short-circuits at its `state.failed()` gate). Read it
            // once instead of unwrapping twice, and treat the impossible
            // `None` as the broken invariant it is: a backend handshake with
            // no readiness slot can never complete, so close rather than
            // panic the worker.
            let Some(back_event) = self.back_readiness().map(|readiness| readiness.event) else {
                error!(
                    "{} backend is connecting but the session state carries no backend readiness, closing",
                    log_context!(self)
                );
                return SessionResult::Close;
            };
            if back_event.is_hup() && !self.test_back_socket() {
                //retry connecting the backend
                debug!(
                    "{} error connecting to backend, trying again",
                    log_context!(self)
                );
                self.connection_attempt += 1;
                self.fail_backend_connection();

                // trigger a backend reconnection
                self.close_backend();
                let connection_result = self.connect_to_backend(session.clone());
                if let Err(err) = &connection_result {
                    match err {
                        // Already logged at warn! + metered at the retry-budget
                        // gate in connect_to_backend; avoid double-emission.
                        BackendConnectionError::MaxConnectionRetries(_) => trace!(
                            "{} Error connecting to backend: {}",
                            log_context!(self),
                            err
                        ),
                        _ => warn!(
                            "{} Error connecting to backend: {}",
                            log_context!(self),
                            err
                        ),
                    }
                }

                if let Some(state_result) = handle_connection_result(connection_result) {
                    return state_result;
                }
            } else if back_event != Ready::EMPTY {
                self.connection_attempt = 0;
                self.set_back_connected(BackendConnectionStatus::Connected);
            }
        } else if back_connected == BackendConnectionStatus::NotConnected
            && let Some(state_result) = self.attempt_backend_connect_if_needed(&session)
        {
            return state_result;
        }

        // A client that half-closes right behind its PROXY-v2 header and
        // payload can deliver both as ONE `READABLE | HUP` event (Linux
        // coalesces `EPOLLIN | EPOLLRDHUP`). `front_hup` closes every
        // pre-`Pipe` state (`SniPreread` without an access log), so
        // honouring HUP first would drop a complete, already-queued header
        // (sozu-proxy/sozu#1823).
        // Drain the expect state's readable bytes first: a parsed header
        // returns `Upgrade` and the recursive `ready()` hands the still-set
        // HUP to `Pipe::frontend_hup`, which keeps the session alive while
        // request bytes remain in the kernel or the pipe's buffers. An empty
        // or truncated header
        // ends with READABLE cleared (`readable` removes it on every
        // zero-byte read) or a `Close`, so a dead client still closes,
        // through `readable`'s own zero-byte arm or the HUP check below.
        if let TcpStateMachine::ExpectProxyProtocol(_) = self.state {
            while (self.front_readiness().interest & self.front_readiness().event).is_readable()
                && self.front_readiness().event.is_hup()
                && counter < MAX_LOOP_ITERATIONS
            {
                let session_result = self.readable();
                if session_result != SessionResult::Continue {
                    return session_result;
                }
                counter += 1;
            }
        }

        if self.front_readiness().event.is_hup() {
            let session_result = self.front_hup();
            if session_result != SessionResult::Continue {
                return session_result;
            }
            // `front_hup` drained in-flight request bytes and wants the
            // session kept alive (`Pipe::frontend_hup`'s in-flight branch):
            // the client already sent FIN, so under edge-triggered epoll no
            // further frontend event will ever arrive -- returning here
            // would stall the session forever waiting for a wake-up that
            // never comes. Clear the now-consumed HUP bit and fall through
            // into the loop below so `readable` (drains the kernel tail) and
            // `back_writable` (flushes `frontend_buffer`) can run
            // synchronously in this same pass, exactly how a backend HUP is
            // already handled inside the loop.
            self.front_readiness().event.remove(Ready::HUP);
        }

        while counter < MAX_LOOP_ITERATIONS {
            let front_interest = self.front_readiness().interest & self.front_readiness().event;
            let back_interest = self
                .back_readiness()
                .map(|r| r.interest & r.event)
                .unwrap_or(Ready::EMPTY);

            trace!(
                "{} Frontend interest({:?}) and backend interest({:?})",
                log_context!(self),
                front_interest,
                back_interest
            );

            if front_interest == Ready::EMPTY && back_interest == Ready::EMPTY {
                break;
            }

            if self
                .back_readiness()
                .map(|r| r.event.is_hup())
                .unwrap_or(false)
                && self.front_readiness().interest.is_writable()
                && !self.front_readiness().event.is_writable()
            {
                break;
            }

            if front_interest.is_readable() {
                let session_result = self.readable();
                if session_result != SessionResult::Continue {
                    return session_result;
                }
                // A `SniPreread` route decision can complete INSIDE this
                // very `readable()` call; without a second attempt here the
                // session would stall until an unrelated readiness event
                // re-entered `ready_inner` to reach the top-of-function
                // connect gate. A no-op for every other state/backend
                // status (see `attempt_backend_connect_if_needed`'s guard).
                if let Some(state_result) = self.attempt_backend_connect_if_needed(&session) {
                    return state_result;
                }
            }

            if back_interest.is_writable() {
                let session_result = self.back_writable();
                if session_result != SessionResult::Continue {
                    return session_result;
                }
            }

            if back_interest.is_readable() {
                let session_result = self.back_readable();
                if session_result != SessionResult::Continue {
                    return session_result;
                }
            }

            if front_interest.is_writable() {
                let session_result = self.writable();
                if session_result != SessionResult::Continue {
                    return session_result;
                }
            }

            if back_interest.is_hup() {
                let session_result = self.back_hup();
                if session_result != SessionResult::Continue {
                    return session_result;
                }
            }

            if front_interest.is_error() {
                error!(
                    "{} Frontend socket error, disconnecting",
                    log_context!(self)
                );
                self.front_readiness().interest = Ready::EMPTY;
                if let Some(r) = self.back_readiness() {
                    r.interest = Ready::EMPTY;
                }

                return SessionResult::Close;
            }

            if back_interest.is_error() && self.back_hup() == SessionResult::Close {
                self.front_readiness().interest = Ready::EMPTY;
                if let Some(r) = self.back_readiness() {
                    r.interest = Ready::EMPTY;
                }

                error!("{} backend socket error, disconnecting", log_context!(self));
                return SessionResult::Close;
            }

            counter += 1;
        }

        if counter >= MAX_LOOP_ITERATIONS {
            error!(
                "{} Handling session went through {} iterations, there's a probable infinite loop bug, closing the connection",
                log_context!(self),
                MAX_LOOP_ITERATIONS
            );

            incr!(names::tcp::INFINITE_LOOP_ERROR);

            let front_interest = self.front_readiness().interest & self.front_readiness().event;
            let back_interest = self
                .back_readiness()
                .map(|r| r.interest & r.event)
                .unwrap_or(Ready::EMPTY);

            let back = self.back_readiness().cloned();

            error!(
                "{} readiness: front {:?} / back {:?} | front: {:?} | back: {:?} ",
                log_context!(self),
                self.front_readiness(),
                back,
                front_interest,
                back_interest
            );

            self.print_session();

            return SessionResult::Close;
        }

        SessionResult::Continue
    }

    /// TCP session closes its backend on its own, without defering this task to the state
    fn close_backend(&mut self) {
        // No `EPOLL_CTL_DEL` for the backend socket: its last `close(2)`
        // takes it out of the epoll set, and it comes before the next
        // `epoll_wait`, so the freed slab token cannot receive a stale event.
        // From `close()`, the server drops the session right after. From
        // `ready_inner`'s retry, `connect_to_backend` either replaces the
        // socket (`set_back_socket` drops the old one) or fails, and every
        // failure closes the session (`handle_connection_result`). No session
        // socket is ever duplicated (no `dup`, `try_clone` or worker fork).
        if let Some(token) = self.backend_token
            && self.back_socket_mut().is_some()
        {
            self.proxy
                .borrow()
                .sessions
                .borrow_mut()
                .slab
                .try_remove(token.0);
        }
        self.remove_backend();

        let back_connected = self.back_connected();
        if back_connected != BackendConnectionStatus::NotConnected {
            if let Some(r) = self.back_readiness() {
                r.event = Ready::EMPTY;
            }

            let log_context = log_context!(self);
            if let Some(sock) = self.back_socket_mut() {
                // TCP-only backend in the pure-TCP proxy: no outbound TLS
                // buffer to truncate, so `Shutdown::Both` is the right call.
                // If the TCP listener ever gains an inline TLS upgrade,
                // switch to `Shutdown::Write` here.
                if let Err(e) = sock.shutdown(Shutdown::Both)
                    && e.kind() != ErrorKind::NotConnected
                {
                    error!(
                        "{} Error closing back socket({:?}): {:?}",
                        log_context, sock, e
                    );
                }
            }
        }

        // The -1 here pairs with the +1 in `set_back_connected(Connected)`:
        // we decrement the gauge exactly once, iff this session had actually
        // reached `Connected`. A `Connecting`/`NotConnected` backend never
        // bumped the gauge, so it must not decrement it either — that
        // asymmetry would underflow the gauge (a correctness bug, never a
        // rounding issue).
        if back_connected == BackendConnectionStatus::Connected {
            gauge_add!(names::backend::CONNECTIONS, -1);
            gauge_add!(
                names::backend::CONNECTIONS_PER_BACKEND,
                -1,
                self.cluster_id.as_deref(),
                self.metrics.backend_id.as_deref(),
                self.cluster_metrics_incarnation
            );
        }

        self.set_back_connected(BackendConnectionStatus::NotConnected);

        // Postcondition: the backend is fully torn down — `remove_backend`
        // cleared the token/handle above and the status is now `NotConnected`,
        // so a subsequent `connect_to_backend` starts from a clean slate.
        debug_assert_eq!(
            self.backend_connected,
            BackendConnectionStatus::NotConnected,
            "close_backend must leave the backend NotConnected"
        );
        debug_assert!(
            self.backend_token.is_none(),
            "close_backend must clear the backend token"
        );
    }

    fn connect_to_backend(
        &mut self,
        session_rc: Rc<RefCell<dyn ProxySession>>,
    ) -> Result<BackendConnectAction, BackendConnectionError> {
        // Precondition: the retry budget can sit AT the ceiling (the gate
        // below converts that into `MaxConnectionRetries`) but the increment
        // in `ready_inner` must never have pushed it past the largest budget
        // that can be configured.
        debug_assert!(
            u32::from(self.connection_attempt)
                <= sozu_command::config::MAX_CONNECTION_ATTEMPTS_UPPER_BOUND,
            "connection_attempt ({}) overflowed the largest attempt budget before the retry gate",
            self.connection_attempt,
        );

        // Prefer the SNI-routed cluster (set by `TcpSession::readable` once
        // `SniPrereadCore` decides) over the listener's legacy no-SNI
        // catch-all -- a listener never configures both (sozu-proxy/sozu#1279),
        // but this order is also simply correct for the routed case, where
        // `listener.cluster_id` is `None`.
        let cluster_id = self
            .cluster_id
            .clone()
            .or_else(|| self.listener.borrow().cluster_id.clone())
            .ok_or(BackendConnectionError::NotFound(ObjectKind::TcpCluster))?;

        self.cluster_id = Some(cluster_id.clone());
        if self.cluster_metrics_incarnation.is_none() {
            self.cluster_metrics_incarnation =
                crate::metrics::cluster_incarnation(cluster_id.as_ref());
        }

        // The cluster's own budget when it sets one, the worker's otherwise
        // (sozu-proxy/sozu#1800).
        let max_attempts = {
            let proxy = self.proxy.borrow();
            let cluster_attempts = proxy
                .configs
                .get(&cluster_id)
                .and_then(|c| c.max_connection_attempts);
            proxy
                .sessions
                .borrow()
                .effective_max_connection_attempts(cluster_attempts)
        };
        if u32::from(self.connection_attempt) >= max_attempts {
            incr!(
                names::backend::CONNECT_RETRIES_EXHAUSTED,
                self.cluster_id.as_deref(),
                self.metrics.backend_id.as_deref(),
                self.cluster_metrics_incarnation
            );
            warn!(
                "{} Max connection attempt reached ({})",
                log_context!(self),
                self.connection_attempt
            );
            return Err(BackendConnectionError::MaxConnectionRetries(Some(
                cluster_id,
            )));
        }

        if self.proxy.borrow().sessions.borrow().at_capacity() {
            return Err(BackendConnectionError::MaxSessionsMemory);
        }

        // Per-(cluster, source-IP) connection limit gate (TCP). The
        // source IP comes from `effective_session_address`, which folds
        // a parsed PROXY-v2 source over the raw `peer_addr`. The mux's
        // Router does the same gate for HTTP/HTTPS sessions; here it
        // runs for raw TCP. Rejection produces a graceful TCP FIN via
        // `BackendConnectionError::TooManyConnectionsPerIp` →
        // `handle_connection_result` → `SessionResult::Close` — TCP has
        // no HTTP envelope to carry a 429 / `Retry-After`.
        let (cluster_max_connections_per_ip, cluster_max_connections_per_subnet, keyed) = self
            .proxy
            .borrow()
            .configs
            .get(&cluster_id)
            .map(|c| {
                (
                    c.max_connections_per_ip,
                    c.max_connections_per_subnet,
                    c.reads_affinity_key,
                )
            })
            .unwrap_or((None, None, false));
        if let Some(ip) = self.effective_session_address().map(|sa| sa.ip()) {
            let sessions_rc = self.proxy.borrow().sessions.clone();
            // Both caps, through the one combined gate the mux uses too,
            // so raw TCP and HTTP/HTTPS can never disagree about what
            // "admitted" means.
            let at_limit = sessions_rc.borrow().cluster_connection_at_limit(
                self.frontend_token,
                &cluster_id,
                &ip,
                cluster_max_connections_per_ip,
                cluster_max_connections_per_subnet,
            );
            if at_limit {
                debug!(
                    "{} per-(cluster, source-IP/subnet) limit hit for cluster {} from {}",
                    log_context!(self),
                    cluster_id,
                    ip
                );
                return Err(BackendConnectionError::TooManyConnectionsPerIp { cluster_id });
            }
            sessions_rc.borrow_mut().track_cluster_connection(
                self.frontend_token,
                cluster_id.clone(),
                ip,
                cluster_max_connections_per_subnet,
            );
            self.cluster_ip_tracked = true;
        }

        // `HRW` and `MAGLEV` pin a TCP client on its source IP, and shuffle
        // sharding ranks its shard by it — the PROXY-v2 source when the
        // listener expects one, as for the gate above. A TCP session has no
        // request to read a header or cookie from, so the source IP is the
        // only key. Any other cluster ignores a key, so none is derived.
        let affinity_key = if keyed {
            self.effective_session_address()
                .map(|address| affinity_key_from_ip(address.ip()))
        } else {
            None
        };
        let (backend, mut stream) = self
            .proxy
            .borrow()
            .backends
            .borrow_mut()
            .backend_from_cluster_id(&cluster_id, affinity_key, Instant::now())
            .map_err(BackendConnectionError::Backend)?;

        if let Err(e) = stream.set_nodelay(true) {
            error!(
                "{} Error setting nodelay on back socket({:?}): {:?}",
                log_context!(self),
                stream,
                e
            );
        }
        // Keep the selected backend's handle, as the mux keeps its own in
        // its `BackendRegistry`: `fail_backend_connection` and
        // `set_back_connected` charge it (failure counter, retry policy,
        // UP/DOWN transitions), and `remove_backend` releases the connection
        // `Backend::try_connect` counted on it.
        self.backend = Some(Rc::clone(&backend));
        self.backend_connected = BackendConnectionStatus::Connecting(Instant::now());

        let back_token = {
            let proxy = self.proxy.borrow();
            let mut s = proxy.sessions.borrow_mut();
            let entry = s.slab.vacant_entry();
            let back_token = Token(entry.key());
            let _entry = entry.insert(session_rc.clone());
            back_token
        };

        if let Err(e) = self.proxy.borrow().registry.register(
            &mut stream,
            back_token,
            Interest::READABLE | Interest::WRITABLE,
        ) {
            error!(
                "{} Error registering back socket({:?}): {:?}",
                log_context!(self),
                stream,
                e
            );
        }

        self.container_backend_timeout.set(back_token);

        self.set_back_token(back_token);
        self.set_back_socket(stream);

        self.metrics.backend_id = Some(Rc::from(backend.borrow().backend_id.as_str()));
        self.metrics.backend_start();
        self.set_backend_id(backend.borrow().backend_id.clone());
        self.set_backend_address(backend.borrow().address);

        // Postcondition of a successful New connect: the session is wired to
        // its freshly-registered backend token and the status reflects an
        // in-flight handshake (`Connecting`). The promotion to `Connected`
        // happens later in `ready_inner` once the socket signals writable.
        debug_assert!(
            self.backend_token.is_some(),
            "a New backend connection must own its backend token"
        );
        debug_assert!(
            self.backend_connected.is_connecting(),
            "a New backend connection must be in the Connecting state"
        );

        Ok(BackendConnectAction::New)
    }
}

impl ProxySession for TcpSession {
    fn close(&mut self) {
        if self.has_been_closed {
            return;
        }

        // Past the idempotency guard the session is closing for the first
        // time: every gauge-restore / untrack below must run exactly once, so
        // re-entry on an already-closed session would double-decrement.
        debug_assert!(
            !self.has_been_closed,
            "close() body must only run on a not-yet-closed session"
        );

        // TODO: the state should handle the timeouts
        trace!("{} Closing TCP session", log_context!(self));
        self.metrics.service_stop();

        // Drain the per-(cluster, source-IP) accounting before any
        // early-return path below. The fail / non-fail close branches
        // both count, and the SessionManager-side untrack is idempotent
        // (no-op when the slot was never tracked) so this is safe even
        // when `cluster_ip_tracked` is false.
        if self.cluster_ip_tracked {
            self.proxy
                .borrow()
                .sessions
                .borrow_mut()
                .untrack_all_cluster_ip(self.frontend_token);
            self.cluster_ip_tracked = false;
        }

        // Restore gauges. `SniPreread` is the "reject"/"teardown" half of
        // `tcp.sni_preread.active`'s "-1 on every exit" contract -- the
        // "upgrade" exit already decremented it in `upgrade_sni_preread`,
        // which also transitions `self.state` away from `SniPreread` before
        // `close()` can ever observe that marker again. A session that
        // reaches `close()` still marked `SniPreread` (directly, or via
        // `FailedUpgrade(SniPreread)` if `upgrade_sni_preread` itself failed)
        // therefore never had its gauge/duration accounted for yet.
        match self.state.marker() {
            StateMarker::Pipe => gauge_add!(names::protocol::TCP, -1),
            StateMarker::SendProxyProtocol => gauge_add!(names::protocol::PROXY_SEND, -1),
            StateMarker::RelayProxyProtocol => gauge_add!(names::protocol::PROXY_RELAY, -1),
            StateMarker::ExpectProxyProtocol => gauge_add!(names::protocol::PROXY_EXPECT, -1),
            StateMarker::SniPreread => {
                gauge_add!(names::tcp::sni_preread::ACTIVE, -1);
                if let TcpStateMachine::SniPreread(preread) = &self.state {
                    time!(
                        names::tcp::sni_preread::DURATION,
                        preread.started_at().elapsed().as_millis() as i64
                    );
                }
            }
        }

        if self.state.failed() {
            match self.state.marker() {
                StateMarker::Pipe => incr!(names::tcp::UPGRADE_PIPE_FAILED),
                StateMarker::SendProxyProtocol => incr!(names::tcp::UPGRADE_SEND_FAILED),
                StateMarker::RelayProxyProtocol => incr!(names::tcp::UPGRADE_RELAY_FAILED),
                StateMarker::ExpectProxyProtocol => incr!(names::tcp::UPGRADE_EXPECT_FAILED),
                StateMarker::SniPreread => incr!(names::tcp::UPGRADE_SNI_PREREAD_FAILED),
            }
            return;
        }

        self.cancel_timeouts();

        let front_socket = self.state.front_socket();
        // TCP listener is plaintext at this layer — `Shutdown::Both` does not
        // truncate any TLS write buffer, so the canonical anti-pattern
        // (forces a TCP RST on the read direction, dropping in-flight bytes)
        // does not apply. Move to `Shutdown::Write` if a TLS upgrade ever
        // wraps this listener.
        if let Err(e) = front_socket.shutdown(Shutdown::Both) {
            // error 107 NotConnected can happen when was never fully connected, or was already disconnected due to error
            if e.kind() != ErrorKind::NotConnected {
                error!(
                    "{} Error shutting down front socket({:?}): {:?}",
                    log_context!(self),
                    front_socket,
                    e
                );
            }
        }

        // Free the frontend slot. No `EPOLL_CTL_DEL`: the server drops the
        // session right after `close()`, and that last `close(2)` of the
        // front socket takes it out of the epoll set (see `close_backend`).
        self.proxy
            .borrow()
            .sessions
            .borrow_mut()
            .slab
            .try_remove(self.frontend_token.0);

        self.close_backend();
        self.has_been_closed = true;

        // Postcondition of the normal close path: the session is terminal and
        // every accounting slot has been released — `close_backend` cleared
        // the backend token, and the per-(cluster, source-IP) untrack above
        // reset the flag. The idempotency guard now short-circuits any repeat.
        debug_assert!(self.has_been_closed, "close() must mark the session closed");
        debug_assert!(
            self.backend_token.is_none(),
            "close() must leave no dangling backend token"
        );
        debug_assert!(
            !self.cluster_ip_tracked,
            "close() must untrack the (cluster, source-IP) slot"
        );
    }

    fn timeout(&mut self, token: Token) -> SessionIsToBeClosed {
        // The frontend and backend slots are distinct tokens, so the two
        // dispatch arms below are mutually exclusive — a single token can
        // never match both. (Obsolete tokens matching neither are tolerated
        // and fall through to the `false` arm.)
        debug_assert!(
            self.backend_token != Some(self.frontend_token),
            "frontend and backend tokens must never collide"
        );
        if self.frontend_token == token {
            self.container_frontend_timeout.triggered();
            // The preread deadline firing always closes the session either
            // way (matches every other state's front-timeout behavior).
            // Route-aware: only feed `Input::Timeout` into the core while
            // still UNDECIDED, for its `tcp.sni_preread.rejected.fragmented`
            // metric + log side effect. Once a route has already latched
            // (backend connect still pending), this same front-timeout
            // firing is a plain "connect/upgrade took too long" close, NOT a
            // fresh preread verdict -- re-feeding `Input::Timeout` into an
            // already-decided core would just replay the SAME latched
            // `Output::Routed` through `SniPreread::handle_output`'s
            // `Routed` arm a SECOND time: double-incrementing
            // `tcp.sni_preread.routed` in release, and tripping its
            // `debug_assert!(self.outcome.is_none(), ...)` in debug
            // (sozu-proxy/sozu#1290).
            if let TcpStateMachine::SniPreread(preread) = &mut self.state {
                if preread.is_routed() {
                    debug!(
                        "{} frontend timeout while a routed SNI-preread session was still \
                         waiting on its backend connect",
                        log_context!(self)
                    );
                } else {
                    let listener = self.listener.borrow();
                    let cfg = listener.preread_config(preread.effective_max_bytes());
                    preread.on_timeout(&cfg);
                }
            }
            return true;
        }
        if self.backend_token == Some(token) {
            self.container_backend_timeout.triggered();
            return true;
        }
        // invalid token, obsolete timeout triggered
        false
    }

    fn protocol(&self) -> Protocol {
        Protocol::TCP
    }

    fn update_readiness(&mut self, token: Token, events: Ready) {
        trace!(
            "{} token {:?} got event {}",
            log_context!(self),
            token,
            super::ready_to_string(events)
        );

        self.last_event = Instant::now();
        self.metrics.wait_start();

        if self.frontend_token == token {
            self.front_readiness().event = self.front_readiness().event | events;
        } else if self.backend_token == Some(token)
            && let Some(r) = self.back_readiness()
        {
            r.event |= events;
        }
    }

    fn ready(&mut self, session: Rc<RefCell<dyn ProxySession>>) -> SessionIsToBeClosed {
        self.metrics.service_start();

        let session_result = self.ready_inner(session.clone());

        let to_bo_closed = match session_result {
            SessionResult::Close => true,
            SessionResult::Continue => false,
            SessionResult::Upgrade => match self.upgrade() {
                false => self.ready(session),
                true => true,
            },
        };

        self.metrics.service_stop();

        // Run-to-completion postcondition: the front/back token + readiness
        // state machine must satisfy its cross-field invariants after every
        // `ready()` pass. Cfg-guarded so the call (and `check_invariants`
        // itself) is absent from release builds.
        #[cfg(debug_assertions)]
        self.check_invariants();

        to_bo_closed
    }

    fn shutting_down(&mut self) -> SessionIsToBeClosed {
        true
    }

    fn last_event(&self) -> Instant {
        self.last_event
    }

    fn print_session(&self) {
        let state: String = match &self.state {
            TcpStateMachine::ExpectProxyProtocol(_) => String::from("Expect"),
            TcpStateMachine::SendProxyProtocol(_) => String::from("Send"),
            TcpStateMachine::RelayProxyProtocol(_) => String::from("Relay"),
            TcpStateMachine::Pipe(_) => String::from("TCP"),
            TcpStateMachine::SniPreread(_) => String::from("SniPreread"),
            TcpStateMachine::FailedUpgrade(marker) => format!("FailedUpgrade({marker:?})"),
        };

        let front_readiness = match &self.state {
            TcpStateMachine::ExpectProxyProtocol(expect) => Some(&expect.frontend_readiness),
            TcpStateMachine::SendProxyProtocol(send) => Some(&send.frontend_readiness),
            TcpStateMachine::RelayProxyProtocol(relay) => Some(&relay.frontend_readiness),
            TcpStateMachine::Pipe(pipe) => Some(&pipe.frontend_readiness),
            TcpStateMachine::SniPreread(preread) => Some(&preread.frontend_readiness),
            TcpStateMachine::FailedUpgrade(_) => None,
        };

        let back_readiness = match &self.state {
            TcpStateMachine::SendProxyProtocol(send) => Some(&send.backend_readiness),
            TcpStateMachine::RelayProxyProtocol(relay) => Some(&relay.backend_readiness),
            TcpStateMachine::Pipe(pipe) => Some(&pipe.backend_readiness),
            TcpStateMachine::SniPreread(preread) => Some(&preread.backend_readiness),
            TcpStateMachine::ExpectProxyProtocol(_) => None,
            TcpStateMachine::FailedUpgrade(_) => None,
        };

        error!(
            "\
{} Session ({:?})
\tFrontend:
\t\ttoken: {:?}\treadiness: {:?}
\tBackend:
\t\ttoken: {:?}\treadiness: {:?}\tstatus: {:?}\tcluster id: {:?}",
            log_context!(self),
            state,
            self.frontend_token,
            front_readiness,
            self.backend_token,
            back_readiness,
            self.backend_connected,
            self.cluster_id
        );
        error!("Metrics: {:?}", self.metrics);
    }

    fn frontend_token(&self) -> Token {
        self.frontend_token
    }
}

pub struct TcpListener {
    active: SessionIsToBeClosed,
    address: SocketAddr,
    cluster_id: Option<ClusterId>,
    config: TcpListenerConfig,
    listener: Option<MioTcpListener>,
    /// A socket this listener holds but has NOT registered.
    ///
    /// `activate()` parks an inherited socket here when `Registry::register`
    /// fails, instead of letting the `?` drop and close a descriptor
    /// `Listeners::get_*` has already removed from the SCM table (sozu#1342).
    /// The next activation reuses it rather than binding a second one.
    ///
    /// Deliberately NOT `listener`: every other consumer reads that field's `Some` as
    /// "registered and live" — `soft_stop` and `hard_stop` deregister it and
    /// report the failure, `give_back_listener` hands it back as an activated
    /// socket, `accept` accepts on it. A parked socket is none of those things,
    /// and putting it there made a failed registration answer `ENOENT` from
    /// `deregister` and fail the whole soft stop.
    parked_listener: Option<MioTcpListener>,
    /// Set by `activate()`, cleared by the first `accept()` that answers
    /// `WouldBlock`: while set, `accept()` sets `TCP_NODELAY` on each socket
    /// it returns, because a connection queued before `activate()` set the
    /// flag on the listener did not inherit it. See the field of the same
    /// name on `HttpListener` (`lib/src/http.rs`).
    nodelay_backlog: bool,
    /// SNI -> `(AlpnMatcher, ClusterId)` route table (sozu-proxy/sozu#1279).
    /// Populated by `add_tcp_front`/`remove_tcp_front` from
    /// `RequestTcpFrontend.sni`/`.alpn`; empty for a listener whose fronts
    /// are all no-SNI (the legacy `cluster_id` catch-all). A listener never
    /// mixes both (enforced at config load, `command/src/config.rs`), but
    /// `create_session`'s routing gate stays defensive and checks both.
    sni_routes: TrieNode<Vec<(AlpnMatcher, ClusterId)>>,
    tags: BTreeMap<String, CachedTags>,
    token: Token,
}

impl ListenerHandler for TcpListener {
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
        Protocol::TCP
    }

    fn public_address(&self) -> SocketAddr {
        self.config
            .public_address
            .map(|addr| addr.into())
            .unwrap_or(self.address)
    }
}

impl TcpListener {
    /// This listener's identity: its address, plus the network interface its
    /// socket is bound to when it has one.
    pub fn key(&self) -> ListenerKey {
        ListenerKey::new(self.address, self.config.interface.as_deref())
    }

    /// Whether this listener is the one `key` names.
    pub fn is(&self, key: &ListenerKey) -> bool {
        key.matches(&self.address, self.config.interface.as_deref())
    }

    fn new(config: TcpListenerConfig, token: Token) -> Result<TcpListener, ListenerError> {
        Ok(TcpListener {
            cluster_id: None,
            listener: None,
            parked_listener: None,
            nodelay_backlog: false,
            token,
            address: config.address.into(),
            config,
            active: false,
            sni_routes: TrieNode::root(),
            tags: BTreeMap::new(),
        })
    }

    /// Validate that a worker can build this TCP listener configuration WITHOUT
    /// constructing the full listener or binding a socket. TCP listener
    /// construction has no fallible config today (no rustls context, no answer
    /// templates; a bad bind surfaces later as an `ActivateListener` failure),
    /// so this currently always succeeds. It exists for surface parity with the
    /// HTTP/HTTPS validators the main process calls before committing an
    /// `Add*Listener` to `ConfigState` and fanning it out (sozu#1301), and is
    /// the hook for any future TCP-config validation.
    pub fn validate_config(_config: &TcpListenerConfig) -> Result<(), ListenerError> {
        Ok(())
    }

    /// Build the [`PrereadConfig`] this listener's `SniPreread` sessions
    /// feed to [`crate::protocol::tcp_preread::SniPrereadCore::handle_input`].
    /// `routes`/`inbound_proxy`/`timeout` come straight from the listener
    /// config; `effective_max_bytes` is session-specific (already clamped to
    /// that session's buffer capacity at construction, see `create_session`),
    /// so it is passed in rather than re-derived here.
    fn preread_config(&self, effective_max_bytes: usize) -> PrereadConfig<'_> {
        PrereadConfig {
            routes: &self.sni_routes,
            inbound_proxy: self.config.expect_proxy,
            max_bytes: effective_max_bytes,
            timeout: Duration::from_secs(u64::from(
                self.config
                    .sni_preread_timeout
                    .unwrap_or(DEFAULT_SNI_PREREAD_TIMEOUT),
            )),
            accept_wildcard: true,
        }
    }

    /// Validate an incoming `AddTcpFrontend` against this listener's
    /// CURRENT routing state before any mutation, mirroring
    /// `command/src/config.rs`'s TOML config-load TCP SNI/ALPN invariants
    /// (sozu-proxy/sozu#1279):
    ///
    /// - `alpn` set with no `sni`: the worker's no-SNI catch-all path never
    ///   consults `alpn`, so the protocol list would silently never be
    ///   enforced (mirrors `ConfigError::AlpnWithoutSni`).
    /// - a no-SNI frontend added to a listener that already has SNI-scoped
    ///   routes, or an SNI-scoped frontend added to a listener that already
    ///   has a no-SNI catch-all cluster (mirrors
    ///   `ConfigError::TcpListenerMixesSniAndNoSni`).
    /// - an ALPN protocol, or a catch-all (empty `alpn`), that overlaps an
    ///   existing route already registered for the same `(address, sni)`
    ///   (mirrors `ConfigError::TcpFrontendAlpnOverlap` /
    ///   `TcpFrontendMultipleAlpnCatchAll`).
    ///
    /// Config-load already rejects all of these shapes for requests built
    /// from a TOML file, but `AddTcpFrontend` can also arrive directly over
    /// the command socket, or via `LoadState` replay of a hand-edited or
    /// stale state file, bypassing config.rs entirely -- the worker must
    /// not silently corrupt its own routing table when that happens.
    fn validate_new_tcp_front(&self, front: &RequestTcpFrontend) -> Result<(), ProxyError> {
        let reject = |reason: String| {
            Err(ProxyError::InvalidTcpFrontend {
                address: self.address,
                reason,
            })
        };

        match &front.sni {
            None => {
                if !front.alpn.is_empty() {
                    return reject(format!(
                        "alpn = {:?} set without sni: alpn only matches within an SNI-scoped \
                         preread, so a frontend without sni would silently ignore its alpn list",
                        front.alpn
                    ));
                }
                if !self.sni_routes.is_empty() {
                    return reject(
                        "a no-SNI frontend cannot be added to a listener that already has \
                         SNI-scoped routes"
                            .to_string(),
                    );
                }
            }
            Some(sni) => {
                if self.cluster_id.is_some() {
                    return reject(
                        "an SNI-scoped frontend cannot be added to a listener that already has \
                         a no-SNI catch-all cluster"
                            .to_string(),
                    );
                }

                // SNI SHAPE check: delegate to the SAME validator config-load
                // uses (`sozu_command::config::validate_sni_pattern`) rather
                // than a hand-rolled partial check. A direct `AddTcpFrontend`
                // over the command socket, or a `LoadState` replay, bypasses
                // config.rs entirely, so the worker boundary must enforce
                // the identical rule -- including the checks a bare '/'/'*'
                // scan used to miss: empty string, non-ASCII, and any empty
                // label (leading/trailing/consecutive dots). An unvalidated
                // leading-empty-label pattern like `.example.com` would
                // otherwise reach `insert_sni_route` ->
                // `pattern_trie::insert_recursive`'s RELEASE-mode
                // `assert_ne!(partial_key, &b""[..])` and crash the worker.
                let normalized_sni = match validate_sni_pattern(sni) {
                    Ok(normalized) => normalized,
                    Err(config_error) => {
                        return reject(format!(
                            "sni {sni:?} failed SNI shape validation: {config_error}"
                        ));
                    }
                };

                // Same key as `insert_sni_route`'s own lookup, so this checks
                // against exactly the entries the new route would be appended
                // alongside. This is bookkeeping over the key's OWN node, not
                // the routing lookup (`preread_config` keeps `true` for
                // that), so `accept_wildcard` must make the lookup EXACT for
                // both key shapes:
                //
                // - literal key -> `false`. With `true`, a literal key with
                //   no child yet (e.g. `a.example.com` when only
                //   `*.example.com` exists) falls back to the sibling
                //   wildcard's entries (`pattern_trie.rs`'s `lookup`
                //   wildcard-fallback branch), misattributing the wildcard's
                //   catch-all to the exact key (falsely rejecting a
                //   legitimate exact catch-all) — and symmetrically,
                //   `insert_sni_route`/`remove_sni_route` would corrupt the
                //   WILDCARD's `Vec` instead of touching a distinct
                //   exact-key node.
                // - wildcard key -> `true`. Unlike `lookup_mut` (which
                //   short-circuits `partial_key == b"*"` before consulting
                //   `accept_wildcard`, so insert/remove stay on `false`),
                //   the immutable `lookup` reaches a wildcard entry ONLY
                //   through the fallback branch; with `false` an existing
                //   `*.example.com` entry is invisible here and a duplicate
                //   catch-all / overlapping-ALPN wildcard front bypasses
                //   validation. For a wildcard key, `true` IS the exact
                //   self-lookup: the traversal descends the key's own
                //   literal ancestry, and `insert` never creates a literal
                //   `*` child that could shadow the node's `wildcard` slot.
                //
                // `starts_with(b"*.")` matches every wildcard shape
                // config-load can emit: `command/src/config.rs`'s
                // `validate_sni_pattern` only admits a single leading `*.`
                // label (any other `*` placement is rejected), and a plain
                // hostname key traverses literal children in both `lookup`
                // and `insert_recursive`. A bare `*` key (config-load
                // rejects it; only a bypassing IPC request can carry one)
                // is the known gap: `lookup_mut`'s short-circuit maps it to
                // the wildcard slot while this immutable self-lookup (flag
                // `false`) cannot see an existing entry there, so duplicate
                // bare-`*` fronts are not detected -- sibling keys stay
                // untouched either way.
                //
                // `normalized_sni` (not a fresh `sni.to_ascii_lowercase()`)
                // is used here: `validate_sni_pattern` already returned the
                // canonical lowercased form above, and reusing it keeps this
                // function's notion of "the key" identical to what
                // `insert_sni_route`/`remove_sni_route` compute from the
                // same input.
                let key = normalized_sni.into_bytes();
                let accept_wildcard_for_self_lookup = key.starts_with(b"*.");
                if let Some((_, existing)) = self
                    .sni_routes
                    .domain_lookup(&key, accept_wildcard_for_self_lookup)
                {
                    let new_is_catch_all = front.alpn.is_empty();
                    for (matcher, _cluster_id) in existing {
                        match matcher {
                            AlpnMatcher::Any if new_is_catch_all => {
                                return reject(format!(
                                    "sni {sni:?} already has a catch-all (empty alpn) \
                                     frontend: at most one frontend per (address, sni) may \
                                     omit alpn"
                                ));
                            }
                            AlpnMatcher::OneOf(protocols) => {
                                if let Some(overlap) = front
                                    .alpn
                                    .iter()
                                    .find(|protocol| protocols.contains(protocol.as_bytes()))
                                {
                                    return reject(format!(
                                        "sni {sni:?} already has a frontend matching ALPN \
                                         protocol {overlap:?}: ALPN matchers for the same \
                                         (address, sni) must not overlap"
                                    ));
                                }
                            }
                            AlpnMatcher::Any => {}
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Add one `(AlpnMatcher, ClusterId)` entry to this listener's SNI route
    /// table, appending to the SNI key's existing `Vec` if one is already
    /// present rather than clobbering it (multiple ALPN-scoped fronts can
    /// share the same SNI). `sni` is defensively lowercased: the
    /// SNI-preread core normalizes (lowercase, no trailing dot) before ever
    /// looking a route up, so the table key must already be in that form.
    ///
    /// Returns `Err` when the underlying trie insert reports
    /// [`InsertResult::Failed`] (a malformed key, e.g. an empty label) —
    /// the caller (`TcpProxy::add_tcp_front`) must propagate this rather
    /// than silently keep going with an add that reports success while the
    /// route table never gained a usable entry. With `validate_new_tcp_front`
    /// enforcing the shared SNI shape validator before this ever runs, a
    /// `Failed` result should be unreachable here — the `debug_assert_ne!`
    /// below asserts OUR OWN validated invariant, not raw wire/IPC input,
    /// and is a loud Sōzu-internal-bug canary in debug builds; the `Err`
    /// path is the release-mode graceful fallback if that invariant is ever
    /// violated by a future bug.
    fn insert_sni_route(
        &mut self,
        sni: String,
        alpn: Vec<String>,
        cluster_id: ClusterId,
    ) -> Result<(), ProxyError> {
        let (key, matcher) = route_key_and_matcher(&sni, alpn);
        // `accept_wildcard: false` — see `validate_new_tcp_front`'s comment:
        // an exact key must never fall back to a sibling wildcard's entry,
        // or this push would corrupt the WILDCARD's route `Vec` instead of
        // creating this key's own node.
        match self.sni_routes.domain_lookup_mut(&key, false) {
            Some((_, entries)) => {
                entries.push((matcher, cluster_id));
                Ok(())
            }
            None => {
                let insert_result = self
                    .sni_routes
                    .domain_insert(key, vec![(matcher, cluster_id)]);
                debug_assert_ne!(
                    insert_result,
                    InsertResult::Failed,
                    "insert_sni_route's key must already be validated by validate_new_tcp_front"
                );
                if insert_result == InsertResult::Failed {
                    error!(
                        "{} SNI route insert failed for {:?} despite passing shape \
                         validation -- rejecting to avoid a silently dead route",
                        log_module_context!(),
                        sni
                    );
                    return Err(ProxyError::InvalidTcpFrontend {
                        address: self.address,
                        reason: format!(
                            "internal error: the route table rejected sni {sni:?} despite \
                             passing shape validation"
                        ),
                    });
                }
                Ok(())
            }
        }
    }

    /// Symmetric counterpart to [`Self::insert_sni_route`]: removes the
    /// matching `(AlpnMatcher, ClusterId)` entry, then `domain_remove`s the
    /// SNI key itself once its `Vec` is empty (no stranded empty entries in
    /// the trie).
    fn remove_sni_route(&mut self, sni: String, alpn: Vec<String>, cluster_id: &str) {
        let (key, matcher) = route_key_and_matcher(&sni, alpn);
        // `accept_wildcard: false` — same reasoning as `insert_sni_route`:
        // removing the exact key must never reach into and strip a sibling
        // wildcard's entries.
        if let Some((_, entries)) = self.sni_routes.domain_lookup_mut(&key, false) {
            entries.retain(|(m, c)| !(*m == matcher && **c == *cluster_id));
            if entries.is_empty() {
                self.sni_routes.domain_remove(&key);
            }
        }
    }

    pub fn activate(
        &mut self,
        registry: &Registry,
        tcp_listener: Option<MioTcpListener>,
    ) -> Result<Token, ProxyError> {
        if self.active {
            return Ok(self.token);
        }
        let address: SocketAddr = self.config.address.into();

        // Reuse a socket a previous attempt parked here after a failed
        // registration, rather than binding a second one. Taking it
        // unconditionally is what keeps the two from coexisting: if the caller
        // ALSO handed one over, the parked socket is disposed of deliberately
        // below instead of being silently overwritten.
        let parked = self.parked_listener.take();
        let mut listener = match (tcp_listener, parked) {
            (Some(inherited), Some(parked)) => {
                // Not reachable while `Listeners::get_*` removes the entry it
                // hands over and `scm_listeners` is written once: a parked
                // socket means this address's entry was already consumed, so
                // the caller cannot have another. Neither of those is enforced
                // here, so dispose of the loser explicitly and loudly rather
                // than letting an assignment close it — that silent close is
                // sozu#1342 itself.
                warn!(
                    "{} closing the socket parked on the TCP listener for {}: an \
                     inherited socket arrived for the same address and is the one being \
                     activated",
                    log_module_context!(),
                    address
                );
                drop(parked);
                inherited
            }
            (Some(inherited), None) => inherited,
            (None, Some(parked)) => parked,
            (None, None) => server_bind(address, self.config.interface.as_deref())
                .map_err(|e| ProxyError::BindToSocket(address, e))?,
        };

        // Once per listener, on whichever socket won above — freshly bound,
        // inherited over SCM_RIGHTS, or parked. Every socket accepted from now
        // on inherits it; see `nodelay_backlog` for the ones already queued.
        if let Err(e) = SockRef::from(&listener).set_tcp_nodelay(true) {
            error!(
                "{} error setting nodelay on listen socket({:?}): {:?}",
                log_module_context!(),
                listener,
                e
            );
        }

        let registration = registry
            .register(&mut listener, self.token, Interest::READABLE)
            .map_err(ProxyError::RegisterListener);
        if let Err(error) = registration {
            // Park the socket instead of letting the `?` drop it. `register`
            // is fallible (EEXIST for a descriptor still registered because
            // `notify_deactivate_listener` only logs a failed deregister,
            // EBADF, ENOMEM), and a socket still held in a local is dropped —
            // and closed — on the way out. For an SCM-inherited socket that is
            // the sozu#1342 close in a second place: `Listeners::get_*` has
            // already removed the entry, so nothing else owns the descriptor.
            // It goes to `parked_listener`, never to `listener`, because every other
            // consumer of that field reads `Some` as "registered and live".
            self.parked_listener = Some(listener);
            return Err(error);
        }

        self.listener = Some(listener);
        self.nodelay_backlog = true;
        self.active = true;
        Ok(self.token)
    }

    /// Apply a partial-update patch to this TCP listener's live configuration.
    ///
    /// Fields absent in the patch (i.e. `None`) are preserved unchanged.
    pub fn update_config(&mut self, patch: &UpdateTcpListenerConfig) -> Result<(), ListenerError> {
        if let Some(v) = patch.public_address {
            self.config.public_address = Some(v);
        }
        if let Some(v) = patch.expect_proxy {
            self.config.expect_proxy = v;
        }
        if let Some(v) = patch.front_timeout {
            self.config.front_timeout = v;
        }
        if let Some(v) = patch.back_timeout {
            self.config.back_timeout = v;
        }
        if let Some(v) = patch.connect_timeout {
            self.config.connect_timeout = v;
        }
        Ok(())
    }
}

fn handle_connection_result(
    connection_result: Result<BackendConnectAction, BackendConnectionError>,
) -> Option<SessionResult> {
    match connection_result {
        // reuse connection or send a default answer, we can continue
        Ok(BackendConnectAction::Reuse) => None,
        Ok(BackendConnectAction::New) | Ok(BackendConnectAction::Replace) => {
            // we must wait for an event
            Some(SessionResult::Continue)
        }
        Err(_) => {
            // in case of BackendConnectionError::Backend(BackendError::ConnectionFailures(..))
            // we may want to retry instead of closing
            Some(SessionResult::Close)
        }
    }
}

/// `min(listener.config.sni_preread_max_bytes, frontend_buffer.capacity())`,
/// floored at [`MIN_SNI_PREREAD_MAX_BYTES`] -- the SNI-preread core has no
/// independent backstop of its own (`SniPrereadCore::handle_input` trusts
/// `PrereadConfig::max_bytes` entirely), so the shell must never hand it a
/// cap the checked-out buffer cannot actually hold (the `min`), NOR a cap so
/// small the preread read is zero-length and spins until the loop guard (the
/// `max`). Config-load rejects a sub-floor `sni_preread_max_bytes` loudly
/// (`ConfigError::SniPrereadMaxBytesTooSmall`), but a `0` knob from a direct
/// `sozu listener tcp add` CLI/IPC request, or a stale `LoadState`
/// replay, bypasses that check and reaches the worker -- this floor degrades
/// it to the 5-byte TLS-record-header minimum instead, killing the spin for
/// EVERY config source at the single point of use. `buffer_capacity` is
/// always `>= MIN_SNI_PREREAD_MAX_BYTES` in practice (buffers are KB-sized),
/// so the `min` never fights the `max`.
fn effective_sni_preread_max_bytes(configured: Option<u32>, buffer_capacity: usize) -> usize {
    (configured.unwrap_or(DEFAULT_SNI_PREREAD_MAX_BYTES) as usize)
        .min(buffer_capacity)
        .max(MIN_SNI_PREREAD_MAX_BYTES as usize)
}

/// Whether `TcpSession::readable`'s frontend-timeout reset should fire for
/// the CURRENT state. `false` only while the session is an UNDECIDED
/// `SniPreread`: the preread deadline armed at session creation
/// (`new_sni_preread`) is an ABSOLUTE budget, not a per-fragment idle timer
/// -- resetting it on every `readable()` event would let a client
/// trickling one byte just before each expiry hold the session (and both
/// its checked-out buffers) open far past the configured
/// `sni_preread_timeout` (sozu-proxy/sozu#1290). Every
/// other state -- a `SniPreread` that has already routed, or any state
/// reached after it -- resets normally: once routed, the frontend timeout
/// reverts to being a genuine per-read idle timer (see the `front_timeout`
/// restore in `readable()`'s route-capture block).
fn frontend_timeout_resets_on_readable(state: &TcpStateMachine) -> bool {
    !matches!(
        state,
        TcpStateMachine::SniPreread(preread) if preread.outcome().is_none()
    )
}

/// Access-log ALPN tag for an SNI-routed TCP session: the client's FIRST
/// offered protocol (client preference order, matching
/// `SniPrereadCore::route`'s own routing precedence), mapped to a known
/// `&'static str` label -- the same two labels `https.rs`'s own ALPN
/// negotiation records (`"h2"` / `"http/1.1"`) -- so `tcp.sni_preread`
/// sessions and terminated-TLS sessions chart under the same values.
/// `None` for an empty offer or an unrecognized protocol: Sōzu never
/// terminates TLS on this path, so this is the client's stated preference,
/// not a negotiated outcome.
fn known_alpn_label(offered: &[Vec<u8>]) -> Option<&'static str> {
    match offered.first().map(Vec::as_slice) {
        Some(b"h2") => Some("h2"),
        Some(b"http/1.1") => Some("http/1.1"),
        _ => None,
    }
}

/// Worker-internal, collision-free identity for a TCP frontend's access-log
/// tags entry (sozu-proxy/sozu#1290). ALPN protocol
/// identifiers are opaque RFC 7301 byte strings -- nothing forbids a `,` or
/// `|` inside one -- so a naive `sorted_alpn.join(",")` string key let two
/// legal, DISJOINT fronts collide: a single protocol `"a,b"` and the pair
/// `["a", "b"]` both joined to the identical string `"a,b"`, so the second
/// `add_tcp_front` silently clobbered the first front's tags entry.
///
/// `TcpListener::tags: BTreeMap<String, CachedTags>` and
/// `Pipe::set_tags_key(Option<String>)` (`lib/src/protocol/pipe.rs`) both
/// key on a plain `String` -- crossing either boundary still needs one --
/// so this type does not replace that storage; it is the SINGLE place that
/// composes the string, via [`Self::fmt`]'s length-prefixed per-protocol
/// encoding, so no choice of in-band separator can be confused with a
/// protocol-name boundary the way a bare `join(",")` could.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum TcpFrontendTagsKey {
    /// Legacy no-SNI catch-all front: keyed by the bare listener address,
    /// exactly as before SNI routing existed.
    Address(SocketAddr),
    /// An SNI-scoped front. `alpn` is sorted (and deduped, since a plain
    /// `Vec<String>` cannot enforce uniqueness the way
    /// [`AlpnMatcher::OneOf`]'s `BTreeSet` does) so two constructions from
    /// the same logical protocol set always compare equal regardless of
    /// the caller's original order.
    Sni {
        address: SocketAddr,
        sni: String,
        alpn: Vec<String>,
    },
}

impl TcpFrontendTagsKey {
    /// The identity triple `command/src/state.rs`'s `add_tcp_frontend`
    /// deduplicates on: `sni` is lowercased to match `insert_sni_route`'s
    /// trie key, `alpn` sorted + deduped so `add_tcp_front`'s operator
    /// order and the route-time rebuild's [`AlpnMatcher::OneOf`] `BTreeSet`
    /// order (`alpn_matcher_protocols`) always agree.
    fn sni(address: SocketAddr, sni: &str, alpn: &[String]) -> Self {
        let mut alpn: Vec<String> = alpn.to_vec();
        alpn.sort_unstable();
        alpn.dedup();
        TcpFrontendTagsKey::Sni {
            address,
            sni: sni.to_ascii_lowercase(),
            alpn,
        }
    }
}

impl std::fmt::Display for TcpFrontendTagsKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TcpFrontendTagsKey::Address(address) => write!(f, "{address}"),
            TcpFrontendTagsKey::Sni { address, sni, alpn } => {
                write!(f, "{address}|{sni}|")?;
                for protocol in alpn {
                    // `<len>:<bytes>` per protocol: the length prefix is
                    // CHECKED against the following bytes, never scanned
                    // for, so a `,`/`:` embedded in one protocol name can
                    // never be misread as a boundary between two --
                    // unlike the old bare `join(",")`.
                    write!(f, "{}:{protocol}", protocol.len())?;
                }
                Ok(())
            }
        }
    }
}

/// Canonical access-log tags key for an SNI-scoped TCP frontend: one
/// listener can carry many SNI/ALPN fronts, each with its own `tags`, so
/// keying tags by the bare listener address (the pre-SNI behavior, kept
/// verbatim for no-SNI fronts) would let the LAST added front clobber every
/// sibling and let removing ANY front clear tags for the whole address. See
/// [`TcpFrontendTagsKey`] for the collision-free composition.
fn sni_tags_key(address: &SocketAddr, sni: &str, alpn: &[String]) -> String {
    TcpFrontendTagsKey::sni(*address, sni, alpn).to_string()
}

/// The matched [`AlpnMatcher`]'s protocols as strings, for rebuilding the
/// [`sni_tags_key`] a route decision maps to: `Any` is the empty-`alpn`
/// catch-all front, `OneOf` yields its (BTreeSet-sorted) protocol list.
/// `from_utf8_lossy` is defensive — matcher protocols originate from
/// config/IPC `String`s, never raw network bytes.
fn alpn_matcher_protocols(matcher: &AlpnMatcher) -> Vec<String> {
    match matcher {
        AlpnMatcher::Any => Vec::new(),
        AlpnMatcher::OneOf(set) => set
            .iter()
            .map(|protocol| String::from_utf8_lossy(protocol).into_owned())
            .collect(),
    }
}

/// Shared `(trie key, ALPN matcher)` construction for
/// [`TcpListener::insert_sni_route`] and [`TcpListener::remove_sni_route`]:
/// `sni` lowercased into the trie key, `alpn` collapsed to
/// [`AlpnMatcher::Any`] when empty (catch-all) or [`AlpnMatcher::OneOf`]
/// otherwise. `alpn` is taken by value since neither caller needs it again
/// afterward.
fn route_key_and_matcher(sni: &str, alpn: Vec<String>) -> (Vec<u8>, AlpnMatcher) {
    let key = sni.to_ascii_lowercase().into_bytes();
    let matcher = if alpn.is_empty() {
        AlpnMatcher::Any
    } else {
        AlpnMatcher::OneOf(alpn.into_iter().map(String::into_bytes).collect())
    };
    (key, matcher)
}

#[derive(Debug)]
pub struct ClusterConfiguration {
    proxy_protocol: Option<ProxyProtocolConfig>,
    /// Whether selection reads the client's affinity key: the policy is
    /// `HRW` or `MAGLEV`, or the cluster shards. `TcpSession::connect_to_backend`
    /// derives the key only then.
    reads_affinity_key: bool,
    /// Per-cluster override of the global per-(cluster, source-IP)
    /// connection limit. `None` inherits the global default,
    /// `Some(0)` is explicit "unlimited", `Some(n > 0)` overrides.
    /// Resolved against `SessionManager::effective_max_connections_per_ip`
    /// at admit time in `connect_to_backend`.
    pub max_connections_per_ip: Option<u64>,
    /// Per-cluster override of the global per-(cluster, source-SUBNET)
    /// connection limit. Same three-state semantics as
    /// `max_connections_per_ip` above, resolved against
    /// `SessionManager::effective_max_connections_per_subnet` at admit
    /// time. An independent second cap: both must admit.
    pub max_connections_per_subnet: Option<u64>,
    /// Per-cluster override of the global backend connection attempt
    /// budget, resolved against
    /// `SessionManager::effective_max_connection_attempts` in
    /// `connect_to_backend`. `None` inherits the global value.
    pub max_connection_attempts: Option<u32>,
}

pub struct TcpProxy {
    fronts: HashMap<ClusterId, Token>,
    backends: Rc<RefCell<BackendMap>>,
    listeners: HashMap<Token, Rc<RefCell<TcpListener>>>,
    configs: HashMap<ClusterId, ClusterConfiguration>,
    registry: Registry,
    sessions: Rc<RefCell<SessionManager>>,
    pool: Rc<RefCell<Pool>>,
}

impl TcpProxy {
    pub fn new(
        registry: Registry,
        sessions: Rc<RefCell<SessionManager>>,
        pool: Rc<RefCell<Pool>>,
        backends: Rc<RefCell<BackendMap>>,
    ) -> TcpProxy {
        TcpProxy {
            backends,
            listeners: HashMap::new(),
            configs: HashMap::new(),
            fronts: HashMap::new(),
            registry,
            sessions,
            pool,
        }
    }

    pub fn add_listener(
        &mut self,
        config: TcpListenerConfig,
        token: Token,
    ) -> Result<Token, ProxyError> {
        if self.listeners.contains_key(&token) {
            return Err(ProxyError::ListenerAlreadyPresent);
        }
        let mut tcp_listener = TcpListener::new(config, token).map_err(ProxyError::AddListener)?;
        // TCP frontends name their listener by address alone, so a listener
        // added next to one already on its address (on another interface)
        // takes over the routes that one serves: every listener on an address
        // holds the same ones, whenever it was added (sozu-proxy/sozu#719).
        if let Some(sibling) = self
            .listeners_at(tcp_listener.address)
            .ok()
            .and_then(|listeners| listeners.into_iter().next())
        {
            let sibling = sibling.borrow();
            tcp_listener.cluster_id.clone_from(&sibling.cluster_id);
            tcp_listener.sni_routes = sibling.sni_routes.clone();
            tcp_listener.tags = sibling.tags.clone();
        }
        match self.listeners.entry(token) {
            Entry::Vacant(entry) => {
                entry.insert(Rc::new(RefCell::new(tcp_listener)));
                Ok(token)
            }
            _ => Err(ProxyError::ListenerAlreadyPresent),
        }
    }

    pub fn remove_listener(&mut self, address: &ListenerKey) -> SessionIsToBeClosed {
        let len = self.listeners.len();

        self.listeners.retain(|_, l| !l.borrow().is(address));
        self.listeners.len() < len
    }

    /// What this proxy would do with a socket handed to
    /// [`activate_listener`](Self::activate_listener) for `addr`. See
    /// [`crate::InheritedSocketFate`].
    ///
    /// The event loop is single-threaded, so nothing can change the listener's
    /// `active` flag between this answer and the `activate_listener` call that
    /// acts on it.
    pub fn inherited_socket_fate(&self, addr: &ListenerKey) -> crate::InheritedSocketFate {
        use crate::InheritedSocketFate;
        match self
            .listeners
            .values()
            .find(|listener| listener.borrow().is(addr))
        {
            None => InheritedSocketFate::Unclaimed,
            Some(listener) if listener.borrow().active => InheritedSocketFate::Refused,
            Some(_) => InheritedSocketFate::Adopted,
        }
    }

    pub fn activate_listener(
        &self,
        addr: &ListenerKey,
        tcp_listener: Option<MioTcpListener>,
    ) -> Result<Token, ProxyError> {
        let listener = self
            .listeners
            .values()
            .find(|listener| listener.borrow().is(addr))
            .ok_or(ProxyError::NoListenerFound(addr.clone()))?;

        listener.borrow_mut().activate(&self.registry, tcp_listener)
    }

    /// The slab token reserved for the listener at `address`, if this proxy
    /// holds one.
    ///
    /// A listener owns exactly one slab slot for its whole `AddListener` ->
    /// `RemoveListener` lifetime, and no `remove_listener` implementation
    /// touches the session slab. `Server` therefore reads the token here
    /// BEFORE dropping the listener, so that reserved slot is released exactly
    /// once, at the end of the lifetime.
    pub fn listener_token(&self, address: &ListenerKey) -> Option<Token> {
        self.listeners
            .iter()
            .find(|(_, listener)| listener.borrow().is(address))
            .map(|(token, _)| *token)
    }

    /// The no-SNI cluster the listener at `address` routes to, for tests
    /// that drive the proxy through `Server::notify_proxys` (`lib/src/server.rs`).
    #[cfg(test)]
    pub(crate) fn listener_cluster(&self, address: &ListenerKey) -> Option<ClusterId> {
        let token = self.listener_token(address)?;
        self.listeners[&token].borrow().cluster_id.clone()
    }

    pub fn give_back_listeners(&mut self) -> Vec<(ListenerKey, MioTcpListener)> {
        self.listeners
            .values()
            .filter_map(|listener| {
                let mut owned = listener.borrow_mut();
                if let Some(listener) = owned.listener.take() {
                    // Reset `active` so a subsequent `activate()` re-binds
                    // instead of short-circuiting on the stale flag.
                    owned.active = false;
                    return Some((owned.key(), listener));
                }

                None
            })
            .collect()
    }

    pub fn give_back_listener(
        &mut self,
        address: &ListenerKey,
    ) -> Result<(Token, MioTcpListener), ProxyError> {
        let listener = self
            .listeners
            .values()
            .find(|listener| listener.borrow().is(address))
            .ok_or(ProxyError::NoListenerFound(address.clone()))?;

        let mut owned = listener.borrow_mut();

        let taken_listener = owned
            .listener
            .take()
            .ok_or(ProxyError::UnactivatedListener)?;

        // Reset `active` so a subsequent `activate()` re-binds instead of
        // short-circuiting on the stale flag.
        owned.active = false;

        Ok((owned.token, taken_listener))
    }

    /// Apply a partial-update patch to the identified TCP listener.
    pub fn update_listener(&mut self, patch: UpdateTcpListenerConfig) -> Result<(), ProxyError> {
        let address: SocketAddr = patch.address.into();
        let listener = self
            .listeners
            .values()
            .find(|l| l.borrow().address == address)
            .ok_or(ProxyError::NoListenerFound(address.into()))?;
        listener
            .borrow_mut()
            .update_config(&patch)
            .map_err(|listener_error| ProxyError::ListenerActivation {
                address,
                listener_error,
            })
    }

    /// Every listener on `address`, whatever its interface. A TCP frontend
    /// names its listener by address alone, so it applies to all the
    /// listeners sharing that address on different interfaces.
    fn listeners_at(
        &self,
        address: SocketAddr,
    ) -> Result<Vec<Rc<RefCell<TcpListener>>>, ProxyError> {
        let mut listeners: Vec<_> = self
            .listeners
            .iter()
            .filter(|(_, listener)| listener.borrow().address == address)
            .map(|(token, listener)| (*token, listener.clone()))
            .collect();
        // Token order, so a failure is reported the same way on every run.
        listeners.sort_by_key(|(token, _)| *token);
        let listeners: Vec<_> = listeners
            .into_iter()
            .map(|(_, listener)| listener)
            .collect();
        if listeners.is_empty() {
            return Err(ProxyError::NoListenerFound(address.into()));
        }
        Ok(listeners)
    }

    pub fn add_tcp_front(&mut self, front: RequestTcpFrontend) -> Result<(), ProxyError> {
        let address = front.address.into();
        let listeners = self.listeners_at(address)?;

        // Hard-reject a request that would corrupt this listener's SNI/ALPN
        // routing invariants BEFORE any mutation below. Config-load
        // (`command/src/config.rs`, sozu-proxy/sozu#1279) already rejects
        // the same shapes for TOML-sourced requests, but `AddTcpFrontend`
        // can also arrive directly over the command socket, or via
        // `LoadState` replay of a hand-edited/stale state file, bypassing
        // config.rs entirely. Every listener on the address is checked before
        // any of them is touched.
        for listener in &listeners {
            listener.borrow().validate_new_tcp_front(&front)?;
        }

        let cluster_id = ClusterId::from(front.cluster_id);
        for listener in listeners {
            let mut listener = listener.borrow_mut();
            self.fronts.insert(cluster_id.clone(), listener.token);

            match &front.sni {
                Some(sni) => {
                    // Per-frontend tags key: many SNI/ALPN fronts share one
                    // listener, so the bare-address key (kept for no-SNI fronts
                    // below) would clobber siblings — see `sni_tags_key`.
                    listener.set_tags(
                        sni_tags_key(&address, sni, &front.alpn),
                        Some(front.tags.clone()),
                    );
                    listener.insert_sni_route(
                        sni.to_owned(),
                        front.alpn.clone(),
                        cluster_id.clone(),
                    )?;
                }
                None => {
                    listener.set_tags(
                        TcpFrontendTagsKey::Address(address).to_string(),
                        Some(front.tags.clone()),
                    );
                    listener.cluster_id = Some(cluster_id.clone());
                }
            }

            // POST: the mixing invariant must hold after every successful add —
            // `validate_new_tcp_front` is the enforcement point above, this is
            // the cheap live re-check that it actually held.
            debug_assert!(
                listener.cluster_id.is_none() || listener.sni_routes.is_empty(),
                "a TCP listener must never mix a no-SNI catch-all cluster with SNI-scoped routes"
            );
        }

        Ok(())
    }

    pub fn remove_tcp_front(&mut self, front: RequestTcpFrontend) -> Result<(), ProxyError> {
        let address = front.address.into();

        for listener in self.listeners_at(address)? {
            let mut listener = listener.borrow_mut();
            match &front.sni {
                Some(sni) => {
                    // Clear ONLY this front's own tags entry (`sni_tags_key`) —
                    // the pre-SNI bare-address removal here used to strip tags
                    // for every sibling front on the listener.
                    listener.set_tags(sni_tags_key(&address, sni, &front.alpn), None);
                    listener.remove_sni_route(
                        sni.to_owned(),
                        front.alpn.clone(),
                        &front.cluster_id,
                    );
                    self.fronts.remove(front.cluster_id.as_str());
                }
                None => {
                    listener.set_tags(TcpFrontendTagsKey::Address(address).to_string(), None);
                    if let Some(cluster_id) = listener.cluster_id.take() {
                        self.fronts.remove(&cluster_id);
                    }
                }
            }
        }

        Ok(())
    }
}

impl ProxyConfiguration for TcpProxy {
    fn notify(&mut self, message: WorkerRequest) -> WorkerResponse {
        let request_type = match message.content.request_type {
            Some(t) => t,
            None => return WorkerResponse::error(message.id, "Empty request"),
        };
        match request_type {
            RequestType::AddTcpFrontend(front) => {
                if let Err(err) = self.add_tcp_front(front) {
                    return WorkerResponse::error(message.id, err);
                }

                WorkerResponse::ok(message.id)
            }
            RequestType::RemoveTcpFrontend(front) => {
                if let Err(err) = self.remove_tcp_front(front) {
                    return WorkerResponse::error(message.id, err);
                }

                WorkerResponse::ok(message.id)
            }
            RequestType::SoftStop(_) => {
                info!(
                    "{} {} processing soft shutdown",
                    log_module_context!(),
                    message.id
                );
                let listeners = std::mem::take(&mut self.listeners);
                for l in listeners.values() {
                    l.borrow_mut()
                        .listener
                        .take()
                        .map(|mut sock| self.registry.deregister(&mut sock));
                }
                WorkerResponse::processing(message.id)
            }
            RequestType::HardStop(_) => {
                info!("{} {} hard shutdown", log_module_context!(), message.id);
                for (_, l) in std::mem::take(&mut self.listeners) {
                    l.borrow_mut()
                        .listener
                        .take()
                        .map(|mut sock| self.registry.deregister(&mut sock));
                }
                WorkerResponse::ok(message.id)
            }
            RequestType::Status(_) => {
                info!("{} {} status", log_module_context!(), message.id);
                WorkerResponse::ok(message.id)
            }
            RequestType::AddCluster(cluster) => {
                let config = ClusterConfiguration {
                    proxy_protocol: cluster
                        .proxy_protocol
                        .and_then(|n| ProxyProtocolConfig::try_from(n).ok()),
                    reads_affinity_key: cluster_reads_affinity_key(&cluster),
                    max_connections_per_ip: cluster.max_connections_per_ip,
                    max_connections_per_subnet: cluster.max_connections_per_subnet,
                    max_connection_attempts: cluster.max_connection_attempts,
                };
                self.configs.insert(cluster.cluster_id.into(), config);
                WorkerResponse::ok(message.id)
            }
            RequestType::RemoveCluster(cluster_id) => {
                self.configs.remove(cluster_id.as_str());
                WorkerResponse::ok(message.id)
            }
            RequestType::RemoveListener(remove) => {
                if !self.remove_listener(&remove.listener_key()) {
                    WorkerResponse::error(
                        message.id,
                        format!("no TCP listener to remove at address {:?}", remove.address),
                    )
                } else {
                    WorkerResponse::ok(message.id)
                }
            }
            command => {
                debug!(
                    "{} {} unsupported message for TCP proxy, ignoring {:?}",
                    log_module_context!(),
                    message.id,
                    command
                );
                WorkerResponse::error(message.id, "unsupported message")
            }
        }
    }

    fn accept(&mut self, token: ListenToken) -> Result<(MioTcpStream, SocketAddr), AcceptError> {
        let internal_token = Token(token.0);
        if let Some(listener) = self.listeners.get(&internal_token) {
            let mut listener = listener.borrow_mut();
            let accepted = match &listener.listener {
                Some(tcp_listener) => tcp_listener.accept(),
                None => return Err(AcceptError::IoError),
            };
            match accepted {
                Ok((frontend_sock, peer)) => {
                    if listener.nodelay_backlog
                        && let Err(e) = frontend_sock.set_nodelay(true)
                    {
                        error!(
                            "{} error setting nodelay on front socket({:?}): {:?}",
                            log_module_context!(),
                            frontend_sock,
                            e
                        );
                    }
                    Ok((frontend_sock, peer))
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    // The backlog queued before `activate()` is drained.
                    listener.nodelay_backlog = false;
                    Err(AcceptError::WouldBlock)
                }
                Err(e) => {
                    error!("{} accept() IO error: {:?}", log_module_context!(), e);
                    Err(AcceptError::IoError)
                }
            }
        } else {
            Err(AcceptError::IoError)
        }
    }

    fn create_session(
        &mut self,
        mut frontend_sock: MioTcpStream,
        peer: SocketAddr,
        token: ListenToken,
        wait_time: Duration,
        proxy: Rc<RefCell<Self>>,
    ) -> Result<(), AcceptError> {
        let listener_token = Token(token.0);

        let listener = self
            .listeners
            .get(&listener_token)
            .ok_or(AcceptError::IoError)?;

        let owned = listener.borrow();
        let mut pool = self.pool.borrow_mut();

        let (front_buffer, back_buffer) = match (pool.checkout(), pool.checkout()) {
            (Some(fb), Some(bb)) => (fb, bb),
            _ => {
                error!("{} could not get buffers from pool", log_module_context!());
                error!(
                    "{} Buffer capacity has been reached, stopping to accept new connections for now",
                    log_module_context!()
                );
                gauge!(names::accept_queue::BACKPRESSURE, 1);
                self.sessions.borrow_mut().can_accept = false;

                return Err(AcceptError::BufferCapacityReached);
            }
        };

        // A listener may route either by a legacy no-SNI catch-all cluster
        // OR by SNI-scoped routes (never both -- enforced at config load,
        // sozu-proxy/sozu#1279); reject only when NEITHER is configured.
        if owned.cluster_id.is_none() && owned.sni_routes.is_empty() {
            error!(
                "{} listener at address {:?} has no linked cluster",
                log_module_context!(),
                owned.address
            );
            return Err(AcceptError::IoError);
        }

        // No `set_nodelay` here: the socket has `TCP_NODELAY` from its
        // listener, or from `TcpProxy::accept` if it was queued before the
        // listener set it.

        let mut session_manager = self.sessions.borrow_mut();
        let entry = session_manager.slab.vacant_entry();
        let frontend_token = Token(entry.key());

        if let Err(register_error) = self.registry.register(
            &mut frontend_sock,
            frontend_token,
            Interest::READABLE | Interest::WRITABLE,
        ) {
            error!(
                "{} error registering front socket({:?}): {:?}",
                log_module_context!(),
                frontend_sock,
                register_error
            );
            return Err(AcceptError::RegisterError);
        }

        let session = if !owned.sni_routes.is_empty() {
            // Routing decides the cluster post-accept; the effective
            // preread cap can never exceed what the checked-out buffer can
            // actually hold, regardless of the configured knob.
            let effective_max_bytes = effective_sni_preread_max_bytes(
                owned.config.sni_preread_max_bytes,
                front_buffer.capacity(),
            );
            let preread_timeout = Duration::from_secs(u64::from(
                owned
                    .config
                    .sni_preread_timeout
                    .unwrap_or(DEFAULT_SNI_PREREAD_TIMEOUT),
            ));
            TcpSession::new_sni_preread(
                back_buffer,
                Duration::from_secs(owned.config.back_timeout as u64),
                Duration::from_secs(owned.config.connect_timeout as u64),
                front_buffer,
                frontend_token,
                listener.clone(),
                proxy,
                frontend_sock,
                peer,
                wait_time,
                preread_timeout,
                effective_max_bytes,
            )
        } else {
            let proxy_protocol = self
                .configs
                .get(owned.cluster_id.as_ref().unwrap())
                .and_then(|c| c.proxy_protocol);
            TcpSession::new(
                back_buffer,
                None,
                owned.cluster_id.clone(),
                Duration::from_secs(owned.config.back_timeout as u64),
                Duration::from_secs(owned.config.connect_timeout as u64),
                Duration::from_secs(owned.config.front_timeout as u64),
                front_buffer,
                frontend_token,
                listener.clone(),
                proxy_protocol,
                proxy,
                frontend_sock,
                peer,
                wait_time,
            )
        };
        incr!(names::tcp::REQUESTS);

        let session = Rc::new(RefCell::new(session));
        entry.insert(session);

        Ok(())
    }
}

pub mod testing {
    use crate::testing::*;

    /// This is not directly used by Sōzu but is available for example and testing purposes
    pub fn start_tcp_worker(
        config: TcpListenerConfig,
        max_buffers: usize,
        buffer_size: usize,
        channel: ProxyChannel,
    ) -> anyhow::Result<()> {
        let address = config.address.into();

        let ServerParts {
            event_loop,
            registry,
            sessions,
            pool,
            backends,
            client_scm_socket: _,
            server_scm_socket,
            server_config,
        } = prebuild_server(max_buffers, buffer_size, true)?;

        let token = {
            let mut sessions = sessions.borrow_mut();
            let entry = sessions.slab.vacant_entry();
            let key = entry.key();
            let _ = entry.insert(Rc::new(RefCell::new(ListenSession {
                protocol: Protocol::TCPListen,
            })));
            Token(key)
        };

        let mut proxy = TcpProxy::new(registry, sessions.clone(), pool.clone(), backends.clone());
        proxy
            .add_listener(config, token)
            .with_context(|| "Failed at creating adding the listener")?;
        proxy
            .activate_listener(&address, None)
            .with_context(|| "Failed at creating activating the listener")?;

        let mut server = Server::new(
            event_loop,
            channel,
            server_scm_socket,
            sessions,
            pool,
            backends,
            None,
            None,
            Some(proxy),
            server_config,
            None,
            false,
        )
        .with_context(|| "Failed at creating server")?;

        debug!("{} starting event loop", log_module_context!());
        server.run();
        debug!("{} ending event loop", log_module_context!());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::{Shutdown, TcpListener, TcpStream},
        str,
        sync::{
            Arc, Barrier,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::Duration,
    };

    use sozu_command::{
        channel::Channel,
        config::ListenerBuilder,
        proto::command::{
            LoadBalancingParams, RequestTcpFrontend, SocketAddress, SoftStop, WorkerRequest,
            WorkerResponse, request::RequestType,
        },
    };

    use super::testing::start_tcp_worker;
    use crate::testing::*;

    /*
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn size_test() {
      assert_size!(Pipe<mio::net::TcpStream>, 224);
      assert_size!(SendProxyProtocol<mio::net::TcpStream>, 144);
      assert_size!(RelayProxyProtocol<mio::net::TcpStream>, 152);
      assert_size!(ExpectProxyProtocol<mio::net::TcpStream>, 520);
      assert_size!(State, 528);
      // fails depending on the platform?
      //assert_size!(Session, 808);
    }*/

    #[test]
    fn round_trip() {
        setup_test_logger!();
        let barrier = Arc::new(Barrier::new(2));
        let test_finished = Arc::new(AtomicBool::new(false));

        let front_port1 = provide_port();
        let front_port2 = provide_port();

        let backend_port = start_server(barrier.clone(), test_finished.clone());
        let mut command =
            start_proxy(backend_port, front_port1, front_port2).expect("Could not start proxy");
        barrier.wait();

        thread::scope(|_s| {
            let front_addr = format!("127.0.0.1:{front_port1}");

            let mut s1 = TcpStream::connect(&front_addr).expect("could not connect");
            s1.set_read_timeout(Some(Duration::from_secs(5)))
                .expect("could not set read timeout on s1");

            let s3 = TcpStream::connect(&front_addr).expect("could not connect");

            let mut s2 = TcpStream::connect(&front_addr).expect("could not connect");
            s2.set_read_timeout(Some(Duration::from_secs(5)))
                .expect("could not set read timeout on s2");

            s1.write_all(b"hello ").expect("could not write to s1");
            println!("s1 sent");

            s2.write_all(b"pouet pouet").expect("could not write to s2");
            println!("s2 sent");

            let mut res = [0; 128];
            s1.write_all(b"coucou").expect("could not write to s1");

            s3.shutdown(Shutdown::Both).expect("could not shutdown s3");

            let sz2 = s2
                .read(&mut res[..])
                .expect("could not read from socket s2");
            println!("s2 received {:?}", str::from_utf8(&res[..sz2]));
            assert_eq!(&res[..sz2], &b"pouet pouet"[..]);

            // Read in a loop: a single read() on a TCP stream is not
            // guaranteed to return all echoed data if the second write's
            // round trip (client → proxy → backend → proxy → client) is
            // still in flight when we poll.
            let expected = b"hello coucou";
            let mut total = 0;
            while total < expected.len() {
                let sz = s1
                    .read(&mut res[total..])
                    .expect("could not read from socket s1");
                assert!(sz > 0, "connection closed before receiving all data");
                total += sz;
            }
            println!(
                "s1 received again({}): {:?}",
                total,
                str::from_utf8(&res[..total])
            );
            assert_eq!(&res[..total], &expected[..]);

            // Signal the echo server to stop
            test_finished.store(true, Ordering::Relaxed);

            // Send SoftStop to the sozu worker so server.run() exits cleanly
            command
                .write_message(&WorkerRequest {
                    id: "ID_SOFTSTOP".to_owned(),
                    content: RequestType::SoftStop(SoftStop {}).into(),
                })
                .expect("could not send SoftStop to sozu worker");
        });
    }

    /// Start an echo server on an ephemeral port.
    /// Returns the port the server is listening on.
    fn start_server(barrier: Arc<Barrier>, test_finished: Arc<AtomicBool>) -> u16 {
        let listener =
            TcpListener::bind("127.0.0.1:0").expect("could not bind echo server listener");
        let port = listener
            .local_addr()
            .expect("could not get echo server local address")
            .port();

        listener
            .set_nonblocking(true)
            .expect("could not set echo server listener to non-blocking");

        thread::spawn(move || {
            barrier.wait();
            let mut count: u8 = 0;
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let finished = test_finished.clone();
                        thread::spawn(move || {
                            println!("got a new client: {count}");
                            stream
                                .set_read_timeout(Some(Duration::from_secs(2)))
                                .expect("could not set read timeout on echo client");
                            let mut buf = [0; 128];
                            loop {
                                match stream.read(&mut buf[..]) {
                                    Ok(0) => break,
                                    Ok(sz) => {
                                        println!(
                                            "ECHO[{count}] got \"{:?}\"",
                                            str::from_utf8(&buf[..sz])
                                        );
                                        stream
                                            .write_all(&buf[..sz])
                                            .expect("could not echo data back");
                                    }
                                    Err(ref e)
                                        if e.kind() == std::io::ErrorKind::WouldBlock
                                            || e.kind() == std::io::ErrorKind::TimedOut =>
                                    {
                                        if finished.load(Ordering::Relaxed) {
                                            println!("backend server stopping (client handler)");
                                            break;
                                        }
                                    }
                                    Err(_) => break,
                                }
                            }
                        });
                        count = count.wrapping_add(1);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if test_finished.load(Ordering::Relaxed) {
                            println!("backend server stopping (accept loop)");
                            break;
                        }
                        thread::sleep(Duration::from_millis(50));
                    }
                    Err(e) => {
                        println!("connection failed: {e:?}");
                    }
                }
            }
        });

        port
    }

    /// Start a sozu TCP proxy worker with the given backend and frontend ports.
    fn start_proxy(
        backend_port: u16,
        front_port1: u16,
        front_port2: u16,
    ) -> anyhow::Result<Channel<WorkerRequest, WorkerResponse>> {
        let config = ListenerBuilder::new_tcp(SocketAddress::new_v4(127, 0, 0, 1, front_port1))
            .to_tcp(None)
            .expect("could not create listener config");

        let (mut command, channel) =
            Channel::generate(1000, 10000).with_context(|| "should create a channel")?;
        let _jg = thread::spawn(move || {
            setup_test_logger!();
            start_tcp_worker(config, 100, 16384, channel).expect("could not start the tcp server");
        });

        command
            .blocking()
            .expect("could not set command channel to blocking");
        {
            let front = RequestTcpFrontend {
                cluster_id: "yolo".to_owned(),
                address: SocketAddress::new_v4(127, 0, 0, 1, front_port1),
                ..Default::default()
            };
            let backend = sozu_command_lib::response::Backend {
                cluster_id: "yolo".into(),
                backend_id: "yolo-0".to_owned(),
                address: SocketAddress::new_v4(127, 0, 0, 1, backend_port).into(),
                load_balancing_parameters: Some(LoadBalancingParams::default()),
                sticky_id: None,
                backup: None,
            };

            command
                .write_message(&WorkerRequest {
                    id: "ID_YOLO1".to_owned(),
                    content: RequestType::AddTcpFrontend(front).into(),
                })
                .expect("could not send AddTcpFrontend for front1");
            command
                .write_message(&WorkerRequest {
                    id: "ID_YOLO2".to_owned(),
                    content: RequestType::AddBackend(backend.to_add_backend()).into(),
                })
                .expect("could not send AddBackend for front1");
        }
        {
            let front = RequestTcpFrontend {
                cluster_id: "yolo".to_owned(),
                address: SocketAddress::new_v4(127, 0, 0, 1, front_port2),
                ..Default::default()
            };
            let backend = sozu_command::response::Backend {
                cluster_id: "yolo".into(),
                backend_id: "yolo-0".to_owned(),
                address: SocketAddress::new_v4(127, 0, 0, 1, backend_port).into(),
                load_balancing_parameters: Some(LoadBalancingParams::default()),
                sticky_id: None,
                backup: None,
            };
            command
                .write_message(&WorkerRequest {
                    id: "ID_YOLO3".to_owned(),
                    content: RequestType::AddTcpFrontend(front).into(),
                })
                .expect("could not send AddTcpFrontend for front2");
            command
                .write_message(&WorkerRequest {
                    id: "ID_YOLO4".to_owned(),
                    content: RequestType::AddBackend(backend.to_add_backend()).into(),
                })
                .expect("could not send AddBackend for front2");
        }

        for _ in 0..4 {
            println!(
                "read_message: {:?}",
                command
                    .read_message()
                    .with_context(|| "could not read message")?
            );
        }

        Ok(command)
    }
}

/// Unit coverage for the SNI-preread routing shell added for
/// sozu-proxy/sozu#1279: the route-table mutations (`add_tcp_front` /
/// `remove_tcp_front`), the `AlpnMatcher` mapping, the effective preread
/// cap, and the routing gate's data invariants. None of this needs a live
/// socket or event loop -- it is a separate module (rather than nested in
/// the `tests` module above) purely to avoid that module's `use
/// std::net::TcpListener` import shadowing `super::TcpListener` (this
/// crate's listener struct).
#[cfg(test)]
mod sni_routing_tests {
    use sozu_command::{config::ListenerBuilder, proto::command::SocketAddress};

    use super::*;
    use crate::testing::{ServerParts, prebuild_server, provide_port};

    fn test_listener() -> TcpListener {
        let config = ListenerBuilder::new_tcp(SocketAddress::new_v4(127, 0, 0, 1, provide_port()))
            .to_tcp(None)
            .expect("could not build a TcpListenerConfig for the test");
        TcpListener::new(config, Token(0)).expect("could not build a bare TcpListener for the test")
    }

    fn frontend(cluster_id: &str, sni: Option<&str>, alpn: &[&str]) -> RequestTcpFrontend {
        RequestTcpFrontend {
            cluster_id: cluster_id.to_owned(),
            address: SocketAddress::new_v4(127, 0, 0, 1, provide_port()),
            sni: sni.map(str::to_owned),
            alpn: alpn.iter().map(|p| p.to_string()).collect(),
            ..Default::default()
        }
    }

    // ---- effective_sni_preread_max_bytes ------------------------------

    #[test]
    fn effective_max_bytes_falls_back_to_default_when_unconfigured() {
        assert_eq!(
            effective_sni_preread_max_bytes(None, 65536),
            DEFAULT_SNI_PREREAD_MAX_BYTES as usize
        );
    }

    #[test]
    fn effective_max_bytes_is_the_min_of_knob_and_capacity() {
        assert_eq!(effective_sni_preread_max_bytes(Some(8192), 16384), 8192);
        assert_eq!(effective_sni_preread_max_bytes(Some(32768), 16384), 16384);
        assert_eq!(effective_sni_preread_max_bytes(Some(16384), 16384), 16384);
    }

    #[test]
    fn effective_max_bytes_never_below_the_floor() {
        // A `sni_preread_max_bytes = 0` knob reaching the worker from a
        // direct `sozu listener tcp add`/`update` CLI/IPC request (or a stale
        // LoadState replay) bypasses config.rs's loud MIN_SNI_PREREAD_MAX_BYTES
        // load-time reject. Without the floor the shell would issue
        // zero-length preread reads and spin until the loop guard closes each
        // session; the floor degrades a sub-minimum knob to the 5-byte
        // TLS-record-header minimum instead.
        assert_eq!(
            effective_sni_preread_max_bytes(Some(0), 16384),
            MIN_SNI_PREREAD_MAX_BYTES as usize,
            "a 0 knob must degrade to the floor, never 0 (would spin the preread)"
        );
        assert_eq!(
            effective_sni_preread_max_bytes(Some(3), 16384),
            MIN_SNI_PREREAD_MAX_BYTES as usize,
            "any sub-floor knob must be raised to the floor"
        );
        // The floor itself, and anything above it, are respected unchanged.
        assert_eq!(
            effective_sni_preread_max_bytes(Some(MIN_SNI_PREREAD_MAX_BYTES), 16384),
            MIN_SNI_PREREAD_MAX_BYTES as usize
        );
        assert_eq!(
            effective_sni_preread_max_bytes(Some(MIN_SNI_PREREAD_MAX_BYTES + 1), 16384),
            (MIN_SNI_PREREAD_MAX_BYTES + 1) as usize
        );
    }

    // ---- known_alpn_label (access-log tagging) -------------------------

    #[test]
    fn known_alpn_label_picks_the_clients_first_offer() {
        assert_eq!(
            known_alpn_label(&[b"h2".to_vec(), b"http/1.1".to_vec()]),
            Some("h2")
        );
        assert_eq!(
            known_alpn_label(&[b"http/1.1".to_vec(), b"h2".to_vec()]),
            Some("http/1.1"),
            "client preference order must win, not a fixed h2-first priority"
        );
    }

    #[test]
    fn known_alpn_label_is_none_for_empty_or_unrecognized_offers() {
        assert_eq!(known_alpn_label(&[]), None);
        assert_eq!(known_alpn_label(&[b"spdy/1".to_vec()]), None);
    }

    // ---- AlpnMatcher mapping + route-table add/remove symmetry --------

    #[test]
    fn empty_alpn_maps_to_any_non_empty_maps_to_one_of() {
        let mut listener = test_listener();
        listener
            .insert_sni_route("example.com".to_owned(), vec![], "cluster-any".into())
            .expect("insert_sni_route must succeed for a valid test SNI");
        listener
            .insert_sni_route(
                "h2.example.com".to_owned(),
                vec!["h2".to_owned(), "http/1.1".to_owned()],
                "cluster-h2".into(),
            )
            .expect("insert_sni_route must succeed for a valid test SNI");

        let (_, any_entries) = listener
            .sni_routes
            .domain_lookup(b"example.com", true)
            .expect("example.com must be routable");
        assert_eq!(any_entries, &vec![(AlpnMatcher::Any, "cluster-any".into())]);

        let (_, h2_entries) = listener
            .sni_routes
            .domain_lookup(b"h2.example.com", true)
            .expect("h2.example.com must be routable");
        assert_eq!(
            h2_entries,
            &vec![(
                AlpnMatcher::OneOf([b"h2".to_vec(), b"http/1.1".to_vec()].into_iter().collect()),
                "cluster-h2".into()
            )]
        );
    }

    #[test]
    fn insert_sni_route_appends_under_the_same_sni() {
        let mut listener = test_listener();
        listener
            .insert_sni_route(
                "example.com".to_owned(),
                vec!["h2".to_owned()],
                "cluster-h2".into(),
            )
            .expect("insert_sni_route must succeed for a valid test SNI");
        listener
            .insert_sni_route("example.com".to_owned(), vec![], "cluster-default".into())
            .expect("insert_sni_route must succeed for a valid test SNI");

        let (_, entries) = listener
            .sni_routes
            .domain_lookup(b"example.com", true)
            .expect("example.com must be routable");
        assert_eq!(entries.len(), 2, "both fronts must share the SNI's Vec");
    }

    #[test]
    fn remove_sni_route_drops_only_the_matching_entry() {
        let mut listener = test_listener();
        listener
            .insert_sni_route(
                "example.com".to_owned(),
                vec!["h2".to_owned()],
                "cluster-h2".into(),
            )
            .expect("insert_sni_route must succeed for a valid test SNI");
        listener
            .insert_sni_route("example.com".to_owned(), vec![], "cluster-default".into())
            .expect("insert_sni_route must succeed for a valid test SNI");

        listener.remove_sni_route(
            "example.com".to_owned(),
            vec!["h2".to_owned()],
            "cluster-h2",
        );

        let (_, entries) = listener
            .sni_routes
            .domain_lookup(b"example.com", true)
            .expect("example.com must still be routable via the remaining entry");
        assert_eq!(
            entries,
            &vec![(AlpnMatcher::Any, "cluster-default".into())],
            "removing one entry must not disturb the other"
        );
    }

    #[test]
    fn remove_sni_route_empties_the_trie_key_when_the_last_entry_goes() {
        let mut listener = test_listener();
        listener
            .insert_sni_route("example.com".to_owned(), vec![], "cluster-a".into())
            .expect("insert_sni_route must succeed for a valid test SNI");
        assert!(!listener.sni_routes.is_empty());

        listener.remove_sni_route("example.com".to_owned(), vec![], "cluster-a");

        assert!(
            listener.sni_routes.is_empty(),
            "domain_remove must run once the SNI's Vec empties, leaving no stranded key"
        );
        assert!(
            listener
                .sni_routes
                .domain_lookup(b"example.com", true)
                .is_none()
        );
    }

    #[test]
    fn remove_sni_route_on_an_absent_sni_is_a_harmless_no_op() {
        let mut listener = test_listener();
        listener
            .insert_sni_route("example.com".to_owned(), vec![], "cluster-a".into())
            .expect("insert_sni_route must succeed for a valid test SNI");

        // Removing a route for a SNI that was never inserted must not panic
        // and must not disturb the existing route.
        listener.remove_sni_route("other.example.net".to_owned(), vec![], "cluster-a");

        assert!(
            listener
                .sni_routes
                .domain_lookup(b"example.com", true)
                .is_some()
        );
    }

    // ---- exact-key bookkeeping must not fall back to a sibling wildcard
    // (route-table corruption caught in sozu-proxy/sozu#1290 review) ----

    #[test]
    fn insert_sni_route_creates_a_distinct_node_for_an_exact_key_over_a_sibling_wildcard() {
        let mut listener = test_listener();
        // Wildcard catch-all first.
        listener
            .insert_sni_route(
                "*.example.com".to_owned(),
                vec![],
                "cluster-wildcard".into(),
            )
            .expect("insert_sni_route must succeed for a valid test SNI");
        // Exact ALPN-scoped route for one specific subdomain.
        listener
            .insert_sni_route(
                "a.example.com".to_owned(),
                vec!["h2".to_owned()],
                "cluster-a-h2".into(),
            )
            .expect("insert_sni_route must succeed for a valid test SNI");

        // The exact key must have gotten its OWN trie node -- with
        // `accept_wildcard: true` this lookup would instead fall back to
        // (and the insert above would have corrupted) the wildcard's node,
        // since no literal `a` child existed yet at insert time.
        let (_, a_entries) = listener
            .sni_routes
            .domain_lookup(b"a.example.com", false)
            .expect("a.example.com must have a distinct exact-key node");
        assert_eq!(
            a_entries,
            &vec![(
                AlpnMatcher::OneOf([b"h2".to_vec()].into_iter().collect()),
                "cluster-a-h2".into()
            )],
            "the exact key's own Vec must hold only its own entry, not the wildcard's"
        );

        // Any OTHER subdomain must still resolve to ONLY the wildcard, via
        // the same `accept_wildcard: true` lookup the routing path uses
        // (`preread_config`) -- it must never see `a.example.com`'s h2 route.
        let (_, b_entries) = listener
            .sni_routes
            .domain_lookup(b"b.example.com", true)
            .expect("b.example.com must fall back to the wildcard catch-all");
        assert_eq!(
            b_entries,
            &vec![(AlpnMatcher::Any, "cluster-wildcard".into())],
            "an unrelated subdomain must see ONLY the wildcard's entry"
        );
    }

    #[test]
    fn validate_new_tcp_front_accepts_an_exact_catch_all_sibling_of_a_wildcard_catch_all() {
        let mut listener = test_listener();
        listener
            .insert_sni_route(
                "*.example.com".to_owned(),
                vec![],
                "cluster-wildcard".into(),
            )
            .expect("insert_sni_route must succeed for a valid test SNI");

        // An exact catch-all for one subdomain must be accepted: it is a
        // SIBLING of the wildcard's catch-all, not a duplicate of it. With
        // `accept_wildcard: true` this lookup would wrongly find the
        // wildcard's own `AlpnMatcher::Any` entry and reject it as "already
        // has a catch-all".
        let front = frontend("cluster-a", Some("a.example.com"), &[]);
        assert!(
            listener.validate_new_tcp_front(&front).is_ok(),
            "an exact catch-all must be accepted when only a SIBLING wildcard has a catch-all"
        );
    }

    #[test]
    fn validate_new_tcp_front_rejects_a_duplicate_wildcard_catch_all() {
        let mut listener = test_listener();
        listener
            .insert_sni_route(
                "*.example.com".to_owned(),
                vec![],
                "cluster-wildcard".into(),
            )
            .expect("insert_sni_route must succeed for a valid test SNI");

        // A SECOND catch-all for the SAME wildcard key is an ambiguous
        // duplicate and must be rejected. The immutable trie `lookup` has no
        // literal-`*` short-circuit (only `lookup_mut` does), so a plain
        // `accept_wildcard: false` self-lookup never sees the existing
        // wildcard entry and waves the duplicate through -- which
        // `insert_sni_route` (lookup_mut, short-circuit present) would then
        // happily append.
        let front = frontend("cluster-dup", Some("*.example.com"), &[]);
        assert!(
            listener.validate_new_tcp_front(&front).is_err(),
            "a second catch-all on the same wildcard SNI must be rejected as a duplicate"
        );
    }

    #[test]
    fn validate_new_tcp_front_rejects_overlapping_alpn_on_the_same_wildcard() {
        let mut listener = test_listener();
        listener
            .insert_sni_route(
                "*.example.com".to_owned(),
                vec!["h2".to_owned()],
                "cluster-wildcard-h2".into(),
            )
            .expect("insert_sni_route must succeed for a valid test SNI");

        // Same wildcard key, overlapping ALPN protocol: ambiguous, must be
        // rejected (same bypass as the duplicate catch-all above).
        let front = frontend("cluster-dup", Some("*.example.com"), &["h2"]);
        assert!(
            listener.validate_new_tcp_front(&front).is_err(),
            "an overlapping ALPN matcher on the same wildcard SNI must be rejected"
        );
    }

    #[test]
    fn validate_new_tcp_front_accepts_a_disjoint_alpn_addition_on_the_same_wildcard() {
        let mut listener = test_listener();
        listener
            .insert_sni_route(
                "*.example.com".to_owned(),
                vec![],
                "cluster-wildcard".into(),
            )
            .expect("insert_sni_route must succeed for a valid test SNI");

        // A non-overlapping ALPN-scoped addition alongside the wildcard's
        // catch-all stays legal -- the wildcard-aware self-lookup must not
        // over-reject.
        let front = frontend("cluster-h2", Some("*.example.com"), &["h2"]);
        assert!(
            listener.validate_new_tcp_front(&front).is_ok(),
            "a disjoint ALPN addition on the same wildcard SNI must be accepted"
        );
    }

    /// The worker boundary must reject malformed SNI SHAPES that a direct
    /// `AddTcpFrontend` (command socket) or `LoadState` replay could carry
    /// past config.rs's `validate_sni_pattern`: a `/.../` label would be
    /// inserted into the `pattern_trie` as a REGEX route and a misplaced
    /// `*` as an unintended wildcard — silently widening routing.
    #[test]
    fn validate_new_tcp_front_rejects_malformed_sni_shapes() {
        let listener = test_listener();

        for bad in [
            "/[a-z]+/.example.com",
            "foo/bar.example.com",
            "a.*.example.com",
            "*.*.example.com",
            "*",
        ] {
            let front = frontend("cluster-a", Some(bad), &[]);
            assert!(
                listener.validate_new_tcp_front(&front).is_err(),
                "malformed SNI shape {bad:?} must be rejected at the worker boundary"
            );
        }

        // The two documented-legal shapes must still pass.
        for good in ["a.example.com", "*.example.com"] {
            let front = frontend("cluster-a", Some(good), &[]);
            assert!(
                listener.validate_new_tcp_front(&front).is_ok(),
                "legal SNI shape {good:?} must still be accepted"
            );
        }
    }

    /// `validate_new_tcp_front`'s OLD hand-rolled shape check only rejected
    /// `/` and a misplaced `*`, letting an empty label (leading dot,
    /// trailing dot, consecutive dots), an empty string, or a non-ASCII
    /// pattern reach `insert_sni_route` -> `pattern_trie::insert_recursive`,
    /// whose RELEASE-mode `assert_ne!(partial_key, &b""[..])` panics the
    /// worker on a leading-empty-label key like `.example.com`. This is the
    /// worker-boundary counterpart to `config.rs`'s `validate_sni_pattern`
    /// tests: every shape the shared validator rejects at config-load must
    /// also be rejected here, for `AddTcpFrontend`/`LoadState` requests that
    /// bypass config.rs entirely.
    #[test]
    fn add_tcp_front_enforces_the_shared_sni_validator_at_the_worker_boundary() {
        let mut proxy = test_proxy();
        let address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let config = ListenerBuilder::new_tcp(address)
            .to_tcp(None)
            .expect("could not build listener config");
        let token = Token(0);
        proxy
            .add_listener(config, token)
            .expect("could not add listener");

        for bad in [
            ".example.com",     // leading empty label -- the pattern_trie-crashing shape
            "example.com.",     // trailing empty label
            "a..b.example.com", // consecutive dots -- empty middle label
            "",                 // empty pattern
            "exämple.com",      // non-ASCII
        ] {
            let front = frontend("cluster-a", Some(bad), &[]);
            let front = RequestTcpFrontend { address, ..front };
            match proxy.add_tcp_front(front) {
                Err(ProxyError::InvalidTcpFrontend { .. }) => {}
                other => panic!("malformed sni {bad:?} must be rejected, got {other:?}"),
            }

            let listener = proxy
                .listeners
                .get(&token)
                .expect("listener must be present")
                .borrow();
            assert!(
                listener.sni_routes.is_empty(),
                "a rejected sni {bad:?} must leave sni_routes empty"
            );
            assert!(
                listener.cluster_id.is_none(),
                "a rejected sni {bad:?} must leave cluster_id unset"
            );
        }

        // A space IS valid ASCII, is not '*'/'/', and splits into non-empty
        // labels ("exa", "mple.com") -- none of the shared validator's rules
        // (empty pattern, non-ASCII, misplaced '*', empty label) cover "not a
        // valid hostname character" in general, so this shape is ACCEPTED,
        // not rejected. Documenting the validator's actual behavior rather
        // than assuming a hostname-shaped string with a space would be
        // caught too.
        let space_front = frontend("cluster-space", Some("exa mple.com"), &[]);
        let space_front = RequestTcpFrontend {
            address,
            ..space_front
        };
        assert!(
            proxy.add_tcp_front(space_front).is_ok(),
            "the shared SNI validator does not reject an embedded space -- \
             it is valid ASCII with no empty label"
        );
    }

    // ---- per-frontend access-log tags keying (sozu-proxy/sozu#1290) ----

    #[test]
    fn sni_fronts_keep_distinct_tags_under_distinct_keys() {
        let mut proxy = test_proxy();
        let address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let config = ListenerBuilder::new_tcp(address)
            .to_tcp(None)
            .expect("could not build listener config");
        let token = Token(0);
        proxy
            .add_listener(config, token)
            .expect("could not add listener");

        let front_a = RequestTcpFrontend {
            cluster_id: "cluster-a".to_owned(),
            address,
            sni: Some("a.example.com".to_owned()),
            alpn: vec![],
            tags: std::collections::BTreeMap::from([("team".to_owned(), "alpha".to_owned())]),
        };
        let front_b = RequestTcpFrontend {
            cluster_id: "cluster-b".to_owned(),
            address,
            sni: Some("b.example.com".to_owned()),
            alpn: vec!["h2".to_owned()],
            tags: std::collections::BTreeMap::from([("team".to_owned(), "beta".to_owned())]),
        };
        proxy
            .add_tcp_front(front_a)
            .expect("add_tcp_front A must succeed");
        proxy
            .add_tcp_front(front_b)
            .expect("add_tcp_front B must succeed");

        let std_address: SocketAddr = address.into();
        let key_a = sni_tags_key(&std_address, "a.example.com", &[]);
        let key_b = sni_tags_key(&std_address, "b.example.com", &["h2".to_owned()]);

        let listener = proxy
            .listeners
            .get(&token)
            .expect("listener must be present")
            .borrow();
        let tags_a = listener
            .get_tags(&key_a)
            .expect("front A's tags must live under its own composed key");
        assert_eq!(
            tags_a.tags.get("team").map(String::as_str),
            Some("alpha"),
            "front A's tags must survive front B's add, not be clobbered by it"
        );
        let tags_b = listener
            .get_tags(&key_b)
            .expect("front B's tags must live under its own composed key");
        assert_eq!(tags_b.tags.get("team").map(String::as_str), Some("beta"));
    }

    #[test]
    fn removing_one_sni_front_clears_only_its_own_tags() {
        let mut proxy = test_proxy();
        let address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let config = ListenerBuilder::new_tcp(address)
            .to_tcp(None)
            .expect("could not build listener config");
        let token = Token(0);
        proxy
            .add_listener(config, token)
            .expect("could not add listener");

        let front_a = RequestTcpFrontend {
            cluster_id: "cluster-a".to_owned(),
            address,
            sni: Some("a.example.com".to_owned()),
            alpn: vec![],
            tags: std::collections::BTreeMap::from([("team".to_owned(), "alpha".to_owned())]),
        };
        let front_b = RequestTcpFrontend {
            cluster_id: "cluster-b".to_owned(),
            address,
            sni: Some("b.example.com".to_owned()),
            alpn: vec!["h2".to_owned()],
            tags: std::collections::BTreeMap::from([("team".to_owned(), "beta".to_owned())]),
        };
        proxy
            .add_tcp_front(front_a.clone())
            .expect("add_tcp_front A must succeed");
        proxy
            .add_tcp_front(front_b)
            .expect("add_tcp_front B must succeed");

        proxy
            .remove_tcp_front(front_a)
            .expect("remove_tcp_front A must succeed");

        let std_address: SocketAddr = address.into();
        let key_a = sni_tags_key(&std_address, "a.example.com", &[]);
        let key_b = sni_tags_key(&std_address, "b.example.com", &["h2".to_owned()]);

        let listener = proxy
            .listeners
            .get(&token)
            .expect("listener must be present")
            .borrow();
        assert!(
            listener.get_tags(&key_a).is_none(),
            "removing front A must clear its own tags entry"
        );
        assert!(
            listener.get_tags(&key_b).is_some(),
            "removing front A must NOT clear sibling front B's tags"
        );
    }

    /// A session routed to front A must look tags up under front A's key:
    /// the key `add_tcp_front` stores and the key `upgrade_sni_preread`
    /// rebuilds from the route decision (`matched_sni_pattern` +
    /// `matched_alpn`) must be identical, regardless of the operator's
    /// original ALPN order or SNI casing, and must never collide with the
    /// bare-address key used by no-SNI fronts.
    #[test]
    fn route_time_tags_key_rebuild_matches_the_add_time_key() {
        let address: SocketAddr = "127.0.0.1:9000".parse().expect("test address");

        // Wildcard front, operator wrote mixed case + reverse ALPN order.
        let add_time = sni_tags_key(
            &address,
            "*.Example.COM",
            &["http/1.1".to_owned(), "h2".to_owned()],
        );
        let matcher =
            AlpnMatcher::OneOf([b"h2".to_vec(), b"http/1.1".to_vec()].into_iter().collect());
        let route_time = sni_tags_key(
            &address,
            "*.example.com", // matched_sni_pattern: the lowercased trie key
            &alpn_matcher_protocols(&matcher),
        );
        assert_eq!(
            add_time, route_time,
            "add-time and route-time keys must agree for the same front"
        );

        // Catch-all front: empty alpn at add time <-> AlpnMatcher::Any.
        assert_eq!(
            sni_tags_key(&address, "a.example.com", &[]),
            sni_tags_key(
                &address,
                "a.example.com",
                &alpn_matcher_protocols(&AlpnMatcher::Any)
            )
        );

        // A composed key never collides with the bare-address key.
        assert_ne!(add_time, address.to_string());
    }

    /// Regression from the sozu-proxy/sozu#1290 review: ALPN protocol
    /// identifiers are opaque byte strings -- nothing forbids a `,` inside
    /// one -- so a SINGLE protocol `"a,b"` and the DISJOINT pair `["a",
    /// "b"]` are two legal, distinct `AlpnMatcher`s on the same
    /// `(address, sni)`, yet the naive `sorted_alpn.join(",")` key collapses
    /// both to the literal string `"a,b"`.
    #[test]
    fn alpn_sets_differing_only_by_an_embedded_separator_get_distinct_tags_keys() {
        let address: SocketAddr = "127.0.0.1:9001".parse().expect("test address");
        let key_joined = sni_tags_key(&address, "example.com", &["a,b".to_owned()]);
        let key_split = sni_tags_key(&address, "example.com", &["a".to_owned(), "b".to_owned()]);
        assert_ne!(
            key_joined, key_split,
            "a single \"a,b\" protocol must not collide with the disjoint [\"a\", \"b\"] pair"
        );
    }

    /// End-to-end through `TcpProxy`: both fronts are accepted (they are
    /// genuinely disjoint `AlpnMatcher`s, so `validate_new_tcp_front`'s
    /// overlap check does not reject either), each keeps its OWN tags under
    /// its own composed key, and removing one leaves the other's tags
    /// intact.
    #[test]
    fn alpn_sets_differing_only_by_an_embedded_separator_keep_distinct_tags() {
        let mut proxy = test_proxy();
        let address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let config = ListenerBuilder::new_tcp(address)
            .to_tcp(None)
            .expect("could not build listener config");
        let token = Token(0);
        proxy
            .add_listener(config, token)
            .expect("could not add listener");

        let front_joined = RequestTcpFrontend {
            cluster_id: "cluster-joined".to_owned(),
            address,
            sni: Some("example.com".to_owned()),
            alpn: vec!["a,b".to_owned()],
            tags: std::collections::BTreeMap::from([("variant".to_owned(), "joined".to_owned())]),
        };
        let front_split = RequestTcpFrontend {
            cluster_id: "cluster-split".to_owned(),
            address,
            sni: Some("example.com".to_owned()),
            alpn: vec!["a".to_owned(), "b".to_owned()],
            tags: std::collections::BTreeMap::from([("variant".to_owned(), "split".to_owned())]),
        };

        proxy
            .add_tcp_front(front_joined.clone())
            .expect("the single \"a,b\" protocol front must be accepted");
        proxy.add_tcp_front(front_split.clone()).expect(
            "the disjoint [\"a\", \"b\"] front must be accepted -- it is NOT the same ALPN \
                 set as [\"a,b\"]",
        );

        let std_address: SocketAddr = address.into();
        let key_joined = sni_tags_key(&std_address, "example.com", &["a,b".to_owned()]);
        let key_split = sni_tags_key(
            &std_address,
            "example.com",
            &["a".to_owned(), "b".to_owned()],
        );
        assert_ne!(key_joined, key_split);

        {
            let listener = proxy
                .listeners
                .get(&token)
                .expect("listener must be present")
                .borrow();
            assert_eq!(
                listener
                    .get_tags(&key_joined)
                    .and_then(|t| t.tags.get("variant"))
                    .map(String::as_str),
                Some("joined"),
                "the joined front's tags must live under its own composed key"
            );
            assert_eq!(
                listener
                    .get_tags(&key_split)
                    .and_then(|t| t.tags.get("variant"))
                    .map(String::as_str),
                Some("split"),
                "the split front's tags must live under its own DISTINCT composed key"
            );
        }

        proxy
            .remove_tcp_front(front_joined)
            .expect("remove the joined front");

        let listener = proxy
            .listeners
            .get(&token)
            .expect("listener must be present")
            .borrow();
        assert!(
            listener.get_tags(&key_joined).is_none(),
            "removing the joined front must clear its own tags entry"
        );
        assert_eq!(
            listener
                .get_tags(&key_split)
                .and_then(|t| t.tags.get("variant"))
                .map(String::as_str),
            Some("split"),
            "removing the joined front must not disturb its sibling's tags"
        );
    }

    #[test]
    fn remove_sni_route_for_an_absent_exact_key_does_not_strip_a_sibling_wildcards_entry() {
        let mut listener = test_listener();
        listener
            .insert_sni_route(
                "*.example.com".to_owned(),
                vec![],
                "cluster-wildcard".into(),
            )
            .expect("insert_sni_route must succeed for a valid test SNI");

        // "a.example.com" was never inserted as its own route -- only the
        // wildcard catch-all exists. A remove targeting the exact host
        // (e.g. a stale `RemoveTcpFrontend` replayed from a hand-edited
        // `LoadState`) must be a no-op here, not reach into and strip the
        // WILDCARD's own catch-all entry.
        listener.remove_sni_route("a.example.com".to_owned(), vec![], "cluster-wildcard");

        let (_, wildcard_entries) = listener
            .sni_routes
            .domain_lookup(b"b.example.com", true)
            .expect(
                "the wildcard catch-all must survive a remove targeting an unrelated exact key",
            );
        assert_eq!(
            wildcard_entries,
            &vec![(AlpnMatcher::Any, "cluster-wildcard".into())]
        );
    }

    // ---- routing gate data invariant: never both cluster_id AND routes ----

    #[test]
    fn a_no_sni_front_leaves_the_route_table_empty() {
        let mut listener = test_listener();
        listener.cluster_id = Some("legacy-catch-all".into());
        assert!(
            listener.sni_routes.is_empty(),
            "a listener with only a no-SNI front must never populate sni_routes"
        );
    }

    #[test]
    fn an_sni_scoped_front_leaves_cluster_id_unset() {
        let mut listener = test_listener();
        listener
            .insert_sni_route("example.com".to_owned(), vec![], "cluster-a".into())
            .expect("insert_sni_route must succeed for a valid test SNI");
        assert!(
            listener.cluster_id.is_none(),
            "a listener with only SNI-scoped fronts must never populate the legacy cluster_id"
        );
    }

    // ---- end-to-end through TcpProxy::add_tcp_front / remove_tcp_front ----

    fn test_proxy() -> TcpProxy {
        let ServerParts {
            registry,
            sessions,
            pool,
            backends,
            ..
        } = prebuild_server(16, 16384, false).expect("could not prebuild a test server");
        TcpProxy::new(registry, sessions, pool, backends)
    }

    /// Two listeners on one address, one bound to `lo` and one to no
    /// interface, coexist in a worker: each is activated, handed back and
    /// removed under its own (address, interface) key, each socket carries its
    /// own `SO_BINDTODEVICE`, and a frontend — which names its listener by
    /// address alone — reaches both.
    #[cfg(target_os = "linux")]
    #[test]
    fn listeners_sharing_an_address_on_different_interfaces_coexist() {
        use socket2::SockRef;

        let mut proxy = test_proxy();
        let address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let bare = ListenerKey::from(address);
        let on_lo = ListenerKey::new(address, Some("lo"));
        for (token, interface) in [(Token(0), None), (Token(1), Some("lo"))] {
            let config = ListenerBuilder::new_tcp(address)
                .with_interface(interface)
                .to_tcp(None)
                .expect("could not build listener config");
            proxy
                .add_listener(config, token)
                .expect("a listener on another interface must not collide");
        }
        assert_eq!(proxy.listener_token(&bare), Some(Token(0)));
        assert_eq!(proxy.listener_token(&on_lo), Some(Token(1)));
        assert_eq!(
            proxy.listener_token(&ListenerKey::new(address, Some("wg0"))),
            None,
            "an interface nobody added names no listener"
        );

        proxy
            .add_tcp_front(RequestTcpFrontend {
                cluster_id: "cluster-a".to_owned(),
                address,
                ..Default::default()
            })
            .expect("add_tcp_front must succeed");
        for token in [Token(0), Token(1)] {
            assert_eq!(
                proxy.listeners[&token].borrow().cluster_id.as_deref(),
                Some("cluster-a"),
                "the frontend must reach the listener at {token:?}"
            );
        }

        for key in [&bare, &on_lo] {
            if let Err(error) = proxy.activate_listener(key, None) {
                let message = error.to_string();
                if message.contains("Operation not permitted") {
                    eprintln!(
                        "SKIPPED listeners_sharing_an_address_on_different_interfaces_coexist: \
                         this environment may not bind a socket to an interface: {message}"
                    );
                    return;
                }
                panic!("activating {key}: {message}");
            }
        }
        let mut handed_back = proxy.give_back_listeners();
        handed_back.sort_by(|(a, _), (b, _)| a.cmp(b));
        let keys: Vec<&ListenerKey> = handed_back.iter().map(|(key, _)| key).collect();
        assert_eq!(keys, vec![&bare, &on_lo]);
        for (key, socket) in &handed_back {
            assert_eq!(
                SockRef::from(socket)
                    .device()
                    .expect("getsockopt(SO_BINDTODEVICE)"),
                key.interface
                    .as_ref()
                    .map(|interface| interface.as_bytes().to_vec()),
                "the socket handed back for {key} must be bound to its interface"
            );
        }

        proxy.remove_listener(&on_lo);
        assert_eq!(proxy.listener_token(&on_lo), None);
        assert_eq!(
            proxy.listener_token(&bare),
            Some(Token(0)),
            "removing the lo listener must keep the one without interface"
        );
    }

    /// A TCP listener added on an address that already routes to a cluster
    /// takes that route over from the listener already there — the no-SNI
    /// cluster and the SNI routes alike — so it forwards from its first
    /// connection (sozu-proxy/sozu#719).
    #[test]
    fn a_listener_added_next_to_a_sibling_takes_over_its_routes() {
        let mut proxy = test_proxy();
        let catch_all = SocketAddress::new_v4(0, 0, 0, 0, provide_port());
        let by_sni = SocketAddress::new_v4(0, 0, 0, 0, provide_port());
        for (token, address, sni) in [
            (Token(0), catch_all, None),
            (Token(2), by_sni, Some("example.com".to_owned())),
        ] {
            let config = ListenerBuilder::new_tcp(address)
                .to_tcp(None)
                .expect("could not build listener config");
            proxy.add_listener(config, token).expect("add the listener");
            proxy
                .add_tcp_front(RequestTcpFrontend {
                    cluster_id: "cluster-a".to_owned(),
                    address,
                    sni,
                    ..Default::default()
                })
                .expect("add_tcp_front must succeed");
        }
        for (token, address) in [(Token(1), catch_all), (Token(3), by_sni)] {
            let config = ListenerBuilder::new_tcp(address)
                .with_interface(Some("lo"))
                .to_tcp(None)
                .expect("could not build listener config");
            proxy.add_listener(config, token).expect("add the sibling");
        }

        assert_eq!(
            proxy.listeners[&Token(1)].borrow().cluster_id.as_deref(),
            Some("cluster-a"),
            "the listener added after the frontend must route to its cluster"
        );
        let sibling = proxy.listeners[&Token(3)].borrow();
        let (_, entries) = sibling
            .sni_routes
            .domain_lookup(b"example.com", true)
            .expect("the listener added after the SNI frontend must route it");
        assert_eq!(entries, &vec![(AlpnMatcher::Any, "cluster-a".into())]);
    }

    /// Traffic that arrives through an interface goes to the listener bound to
    /// it, not to the listener on the same address bound to none: Linux
    /// prefers the device-bound socket, and the two never share a
    /// `SO_REUSEPORT` group. Here a bare `0.0.0.0:P` and a `lo`-bound
    /// `0.0.0.0:P` both listen, a client connects through `127.0.0.1`, and
    /// only the `lo` listener has a connection to accept.
    #[cfg(target_os = "linux")]
    #[test]
    fn traffic_through_an_interface_reaches_the_listener_bound_to_it() {
        let mut proxy = test_proxy();
        let port = provide_port();
        let address = SocketAddress::new_v4(0, 0, 0, 0, port);
        for (token, interface) in [(Token(0), None), (Token(1), Some("lo"))] {
            let config = ListenerBuilder::new_tcp(address)
                .with_interface(interface)
                .to_tcp(None)
                .expect("could not build listener config");
            proxy.add_listener(config, token).expect("add the listener");
            let key = ListenerKey::new(address, interface);
            if let Err(error) = proxy.activate_listener(&key, None) {
                let message = error.to_string();
                if message.contains("Operation not permitted") {
                    eprintln!(
                        "SKIPPED traffic_through_an_interface_reaches_the_listener_bound_to_it: \
                         this environment may not bind a socket to an interface: {message}"
                    );
                    return;
                }
                panic!("activating {key}: {message}");
            }
        }

        // Several connections: with the binding ignored, both sockets would
        // share one `SO_REUSEPORT` group and each connection would land on
        // either of them.
        const CONNECTIONS: usize = 8;
        let _clients: Vec<_> = (0..CONNECTIONS)
            .map(|_| {
                std::net::TcpStream::connect(("127.0.0.1", port))
                    .expect("connect through the loopback interface")
            })
            .collect();
        // The handshakes are complete once `connect` returns; poll briefly in
        // case an accept queue entry is not visible yet.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut accepted = 0;
        while accepted < CONNECTIONS {
            match proxy.accept(ListenToken(1)) {
                Ok(_) => accepted += 1,
                Err(AcceptError::WouldBlock) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => panic!(
                    "the lo listener must accept every connection, got {accepted} of \
                     {CONNECTIONS}: {error:?}"
                ),
            }
        }
        assert!(
            matches!(proxy.accept(ListenToken(0)), Err(AcceptError::WouldBlock)),
            "the listener bound to no interface must not receive loopback traffic"
        );
    }

    #[test]
    fn add_then_remove_sni_front_round_trips_through_tcp_proxy() {
        let mut proxy = test_proxy();
        let address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let config = ListenerBuilder::new_tcp(address)
            .to_tcp(None)
            .expect("could not build listener config");
        let token = Token(0);
        proxy
            .add_listener(config, token)
            .expect("could not add listener");

        let front = RequestTcpFrontend {
            cluster_id: "cluster-a".to_owned(),
            address,
            sni: Some("Example.COM".to_owned()),
            alpn: vec![],
            ..Default::default()
        };
        proxy
            .add_tcp_front(front.clone())
            .expect("add_tcp_front must succeed");

        {
            let listener = proxy
                .listeners
                .get(&token)
                .expect("listener must be present")
                .borrow();
            assert!(listener.cluster_id.is_none());
            let (_, entries) = listener
                .sni_routes
                // Lowercased at insert time regardless of wire-form casing.
                .domain_lookup(b"example.com", true)
                .expect("example.com must be routable after add_tcp_front");
            assert_eq!(entries, &vec![(AlpnMatcher::Any, "cluster-a".into())]);
        }
        assert_eq!(proxy.fronts.get("cluster-a"), Some(&token));

        proxy
            .remove_tcp_front(front)
            .expect("remove_tcp_front must succeed");

        {
            let listener = proxy
                .listeners
                .get(&token)
                .expect("listener must be present")
                .borrow();
            assert!(
                listener.sni_routes.is_empty(),
                "remove_tcp_front must leave no stranded route"
            );
        }
        assert_eq!(
            proxy.fronts.get("cluster-a"),
            None,
            "remove_tcp_front must undo add_tcp_front's self.fronts bookkeeping"
        );
    }

    #[test]
    fn add_then_remove_legacy_no_sni_front_round_trips() {
        let mut proxy = test_proxy();
        let address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let config = ListenerBuilder::new_tcp(address)
            .to_tcp(None)
            .expect("could not build listener config");
        let token = Token(0);
        proxy
            .add_listener(config, token)
            .expect("could not add listener");

        let front = frontend("cluster-legacy", None, &[]);
        let front = RequestTcpFrontend { address, ..front };
        proxy
            .add_tcp_front(front.clone())
            .expect("add_tcp_front must succeed");

        assert_eq!(
            proxy
                .listeners
                .get(&token)
                .expect("listener must be present")
                .borrow()
                .cluster_id,
            Some("cluster-legacy".into())
        );

        proxy
            .remove_tcp_front(front)
            .expect("remove_tcp_front must succeed");

        assert_eq!(
            proxy
                .listeners
                .get(&token)
                .expect("listener must be present")
                .borrow()
                .cluster_id,
            None
        );
        assert_eq!(proxy.fronts.get("cluster-legacy"), None);
    }

    // ---- add_tcp_front hard-rejects routing-corrupting requests --------
    //
    // Worker-side mirror of `command/src/config.rs`'s TOML config-load
    // invariants (sozu-proxy/sozu#1279 hardening): `AddTcpFrontend` can
    // reach the worker directly over the command socket, or via `LoadState`
    // replay, bypassing config.rs entirely, so `add_tcp_front` must defend
    // itself rather than rely on a debug-only assertion.

    #[test]
    fn add_tcp_front_rejects_alpn_without_sni() {
        let mut proxy = test_proxy();
        let address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let config = ListenerBuilder::new_tcp(address)
            .to_tcp(None)
            .expect("could not build listener config");
        proxy
            .add_listener(config, Token(0))
            .expect("could not add listener");

        let front = frontend("cluster-a", None, &["h2"]);
        let front = RequestTcpFrontend { address, ..front };
        match proxy.add_tcp_front(front) {
            Err(ProxyError::InvalidTcpFrontend { .. }) => {}
            other => panic!("expected InvalidTcpFrontend, got {other:?}"),
        }
    }

    #[test]
    fn add_tcp_front_rejects_no_sni_front_on_listener_with_sni_routes() {
        let mut proxy = test_proxy();
        let address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let config = ListenerBuilder::new_tcp(address)
            .to_tcp(None)
            .expect("could not build listener config");
        proxy
            .add_listener(config, Token(0))
            .expect("could not add listener");

        let sni_front = frontend("cluster-a", Some("example.com"), &[]);
        let sni_front = RequestTcpFrontend {
            address,
            ..sni_front
        };
        proxy
            .add_tcp_front(sni_front)
            .expect("the first, SNI-scoped frontend must be accepted");

        let no_sni_front = frontend("cluster-b", None, &[]);
        let no_sni_front = RequestTcpFrontend {
            address,
            ..no_sni_front
        };
        match proxy.add_tcp_front(no_sni_front) {
            Err(ProxyError::InvalidTcpFrontend { .. }) => {}
            other => panic!("expected InvalidTcpFrontend, got {other:?}"),
        }
    }

    #[test]
    fn add_tcp_front_rejects_sni_front_on_listener_with_no_sni_cluster() {
        let mut proxy = test_proxy();
        let address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let config = ListenerBuilder::new_tcp(address)
            .to_tcp(None)
            .expect("could not build listener config");
        proxy
            .add_listener(config, Token(0))
            .expect("could not add listener");

        let no_sni_front = frontend("cluster-a", None, &[]);
        let no_sni_front = RequestTcpFrontend {
            address,
            ..no_sni_front
        };
        proxy
            .add_tcp_front(no_sni_front)
            .expect("the first, no-SNI frontend must be accepted");

        let sni_front = frontend("cluster-b", Some("example.com"), &[]);
        let sni_front = RequestTcpFrontend {
            address,
            ..sni_front
        };
        match proxy.add_tcp_front(sni_front) {
            Err(ProxyError::InvalidTcpFrontend { .. }) => {}
            other => panic!("expected InvalidTcpFrontend, got {other:?}"),
        }
    }

    #[test]
    fn add_tcp_front_rejects_alpn_overlap_on_same_sni() {
        let mut proxy = test_proxy();
        let address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let config = ListenerBuilder::new_tcp(address)
            .to_tcp(None)
            .expect("could not build listener config");
        proxy
            .add_listener(config, Token(0))
            .expect("could not add listener");

        let first = frontend("cluster-a", Some("example.com"), &["h2"]);
        let first = RequestTcpFrontend { address, ..first };
        proxy
            .add_tcp_front(first)
            .expect("the first frontend must be accepted");

        // Second frontend shares "h2" with the first on the same sni.
        let second = frontend("cluster-b", Some("example.com"), &["h2", "http/1.1"]);
        let second = RequestTcpFrontend { address, ..second };
        match proxy.add_tcp_front(second) {
            Err(ProxyError::InvalidTcpFrontend { .. }) => {}
            other => panic!("expected InvalidTcpFrontend, got {other:?}"),
        }
    }

    #[test]
    fn add_tcp_front_rejects_duplicate_catch_all_on_same_sni() {
        let mut proxy = test_proxy();
        let address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let config = ListenerBuilder::new_tcp(address)
            .to_tcp(None)
            .expect("could not build listener config");
        proxy
            .add_listener(config, Token(0))
            .expect("could not add listener");

        let first = frontend("cluster-a", Some("example.com"), &[]);
        let first = RequestTcpFrontend { address, ..first };
        proxy
            .add_tcp_front(first)
            .expect("the first catch-all frontend must be accepted");

        let second = frontend("cluster-b", Some("example.com"), &[]);
        let second = RequestTcpFrontend { address, ..second };
        match proxy.add_tcp_front(second) {
            Err(ProxyError::InvalidTcpFrontend { .. }) => {}
            other => panic!("expected InvalidTcpFrontend, got {other:?}"),
        }
    }

    /// The valid, intended shape (sozu-proxy/sozu#1279's whole reason for
    /// existing) must still be accepted: disjoint, non-empty `alpn` lists
    /// on the same `(address, sni)`, and a catch-all alongside a
    /// specific-protocol entry.
    #[test]
    fn add_tcp_front_accepts_disjoint_alpn_and_catch_all_on_same_sni() {
        let mut proxy = test_proxy();
        let address = SocketAddress::new_v4(127, 0, 0, 1, provide_port());
        let config = ListenerBuilder::new_tcp(address)
            .to_tcp(None)
            .expect("could not build listener config");
        proxy
            .add_listener(config, Token(0))
            .expect("could not add listener");

        let h2 = frontend("cluster-h2", Some("example.com"), &["h2"]);
        let h2 = RequestTcpFrontend { address, ..h2 };
        proxy
            .add_tcp_front(h2)
            .expect("disjoint alpn frontend must be accepted");

        let http11 = frontend("cluster-http11", Some("example.com"), &["http/1.1"]);
        let http11 = RequestTcpFrontend { address, ..http11 };
        proxy
            .add_tcp_front(http11)
            .expect("second disjoint alpn frontend must be accepted");

        let catch_all = frontend("cluster-default", Some("example.com"), &[]);
        let catch_all = RequestTcpFrontend {
            address,
            ..catch_all
        };
        proxy
            .add_tcp_front(catch_all)
            .expect("a catch-all alongside specific-protocol entries must be accepted");
    }

    // ---- tcp.sni_preread.active gauge accounting ----------------------

    /// Read the current process-local `tcp.sni_preread.active` gauge,
    /// treating an absent key as 0. `dump_local_proxy_metrics` is a
    /// non-draining filter over the proxy `MetricsMap`, so repeated reads are
    /// side-effect free and the key is the raw metric name.
    fn sni_preread_active_gauge() -> i64 {
        use sozu_command::proto::command::filtered_metrics::Inner;
        crate::metrics::METRICS.with(|metrics| {
            metrics
                .borrow_mut()
                .dump_local_proxy_metrics()
                .get(names::tcp::sni_preread::ACTIVE)
                .and_then(|fm| fm.inner.as_ref())
                .and_then(|inner| match inner {
                    Inner::Gauge(v) => Some(*v as i64),
                    _ => None,
                })
                .unwrap_or(0)
        })
    }

    #[test]
    fn entering_sni_preread_increments_the_active_gauge() {
        // Regression guard for the missing-`+1` gauge bug
        // (sozu-proxy/sozu#1279): `new_sni_preread` must bump
        // `tcp.sni_preread.active` by exactly one when a session ENTERS the
        // state, so each of the two `-1` decrements -- the "upgrade" exit in
        // `upgrade_sni_preread` and the "reject"/"teardown" exit in `close()`'s
        // `StateMarker::SniPreread` arm -- has a matching increment. Without
        // this `+1` the first `-1` underflows a fresh-zero gauge (clamped to 0,
        // ERROR-logged), pinning the gauge at 0 and rendering the e2e gauge
        // assertion vacuous.
        //
        // `METRICS` is a thread-local shared across unit tests on the same
        // worker thread, so this asserts the DELTA around one constructor call
        // (robust to any starting value), not an absolute reading. The
        // net-zero-per-session contract spans the full lifecycle (accept ->
        // live backend connect -> upgrade/teardown) and is the behavioural job
        // of the e2e gauge assertion
        // (`test_tcp_sni_per_ip_limiter_rejects_second_then_admits_after_release`
        // in `e2e/src/tests/tcp_sni_tests.rs`), not reproducible at this unit
        // level.
        let ServerParts {
            registry,
            sessions,
            pool,
            backends,
            ..
        } = prebuild_server(16, 16384, false).expect("could not prebuild a test server");

        let proxy = Rc::new(RefCell::new(TcpProxy::new(
            registry,
            sessions,
            pool.clone(),
            backends,
        )));
        let listener = Rc::new(RefCell::new(test_listener()));

        let (front_buffer, back_buffer) = {
            let mut pool = pool.borrow_mut();
            (
                pool.checkout().expect("front buffer checkout must succeed"),
                pool.checkout().expect("back buffer checkout must succeed"),
            )
        };

        // A non-blocking connect to a (likely unused) loopback port returns a
        // real `MioTcpStream` handle immediately, regardless of whether the
        // connection completes; `new_sni_preread` reads nothing from it.
        let peer: SocketAddr = format!("127.0.0.1:{}", provide_port())
            .parse()
            .expect("loopback address must parse");
        let socket = MioTcpStream::connect(peer).expect("mio connect must return a socket handle");

        let before = sni_preread_active_gauge();
        let session = TcpSession::new_sni_preread(
            back_buffer,
            Duration::from_secs(30),
            Duration::from_secs(30),
            front_buffer,
            Token(0),
            listener,
            proxy,
            socket,
            peer,
            Duration::from_millis(0),
            Duration::from_secs(3),
            16384,
        );
        let after = sni_preread_active_gauge();

        // The session is measured while still in `SniPreread`; hold it across
        // the read so no future `Drop` side effect could race the measurement.
        assert!(matches!(session.state, TcpStateMachine::SniPreread(_)));

        assert_eq!(
            after - before,
            1,
            "entering the SniPreread state must increment tcp.sni_preread.active by exactly one"
        );
    }

    // ---- absolute preread deadline + route-aware timeout (sozu-proxy/sozu#1290) ----

    /// `frontend_timeout_resets_on_readable` must gate the reset on exactly
    /// one thing: whether the CURRENT state is an undecided `SniPreread`.
    /// Constructed directly (no socket I/O needed) since the predicate is a
    /// pure function of `&TcpStateMachine`.
    #[test]
    fn frontend_timeout_reset_is_gated_on_sni_preread_decision() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 16 * 1024);
        let frontend_buffer = pool.checkout().expect("frontend buffer");
        let socket = MioTcpStream::connect(
            format!("127.0.0.1:{}", provide_port())
                .parse()
                .expect("loopback address must parse"),
        )
        .expect("mio connect must return a socket handle");

        let undecided = TcpStateMachine::SniPreread(SniPreread::new(
            socket,
            Token(0),
            Ulid::generate(),
            frontend_buffer,
            16384,
        ));
        assert!(
            !frontend_timeout_resets_on_readable(&undecided),
            "an undecided SniPreread must not have its absolute deadline reset"
        );

        // Every OTHER state resets normally -- represented here by
        // `ExpectProxyProtocol`, cheaply constructible without driving a
        // real route decision (unlike a ROUTED `SniPreread`, whose
        // `outcome` field has no test-only setter and can only become
        // `Some` by parsing a real ClientHello -- see
        // `frontend_timeout_restored_and_timeout_after_route_is_not_double_counted`
        // below for that scenario end-to-end).
        let socket2 = MioTcpStream::connect(
            format!("127.0.0.1:{}", provide_port())
                .parse()
                .expect("loopback address must parse"),
        )
        .expect("mio connect must return a socket handle");
        let container = crate::timer::TimeoutContainer::new_empty(Duration::from_secs(5));
        let other = TcpStateMachine::ExpectProxyProtocol(ExpectProxyProtocol::new(
            container,
            socket2,
            Token(1),
            Ulid::generate(),
        ));
        assert!(
            frontend_timeout_resets_on_readable(&other),
            "every non-preread-undecided state must keep resetting its frontend timeout"
        );
    }

    /// Minimal single-record TLS ClientHello wire carrying only a
    /// `server_name` extension for `host` -- hand-built rather than reusing
    /// `tcp_preread::parser`'s test-only wire-building helpers (`mod
    /// parser` is private to `tcp_preread`, unreachable from this sibling
    /// module) to drive a REAL route decision through
    /// `TcpSession::readable()` for the regression test below.
    fn minimal_client_hello_wire(host: &str) -> Vec<u8> {
        let mut name_list = vec![0u8]; // name_type = host_name
        name_list.extend_from_slice(&(host.len() as u16).to_be_bytes());
        name_list.extend_from_slice(host.as_bytes());
        let mut sni_ext_data = Vec::new();
        sni_ext_data.extend_from_slice(&(name_list.len() as u16).to_be_bytes());
        sni_ext_data.extend_from_slice(&name_list);
        let mut sni_ext = Vec::new();
        sni_ext.extend_from_slice(&0x0000u16.to_be_bytes()); // server_name extension type
        sni_ext.extend_from_slice(&(sni_ext_data.len() as u16).to_be_bytes());
        sni_ext.extend_from_slice(&sni_ext_data);

        let mut body = Vec::new();
        body.extend_from_slice(&[0x03, 0x03]); // legacy_version
        body.extend_from_slice(&[0u8; 32]); // random
        body.push(0); // session_id: empty
        body.extend_from_slice(&[0x00, 0x02, 0x13, 0x01]); // cipher_suites
        body.push(1); // compression_methods length
        body.push(0); // compression_method: null
        body.extend_from_slice(&(sni_ext.len() as u16).to_be_bytes()); // extensions block length
        body.extend_from_slice(&sni_ext);

        let mut handshake = Vec::new();
        handshake.push(1u8); // msg_type = client_hello
        let hs_len = body.len() as u32;
        handshake.extend_from_slice(&hs_len.to_be_bytes()[1..4]);
        handshake.extend_from_slice(&body);

        let mut record = Vec::new();
        record.push(22u8); // ContentType::handshake
        record.extend_from_slice(&[0x03, 0x03]); // legacy record version
        record.extend_from_slice(&(handshake.len() as u16).to_be_bytes());
        record.extend_from_slice(&handshake);
        record
    }

    /// Read the current process-local `tcp.sni_preread.routed` counter.
    /// Same non-draining-read pattern as `sni_preread_active_gauge`, but
    /// for a `Count` metric instead of a `Gauge`.
    fn sni_preread_routed_count() -> i64 {
        use sozu_command::proto::command::filtered_metrics::Inner;
        crate::metrics::METRICS.with(|metrics| {
            metrics
                .borrow_mut()
                .dump_local_proxy_metrics()
                .get(names::tcp::sni_preread::ROUTED)
                .and_then(|fm| fm.inner.as_ref())
                .and_then(|inner| match inner {
                    Inner::Count(v) => Some(*v),
                    _ => None,
                })
                .unwrap_or(0)
        })
    }

    /// End-to-end regression from the sozu-proxy/sozu#1290 review:
    ///
    /// (a) the moment a real ClientHello routes, `container_frontend_timeout`
    ///     must already carry the listener's configured `front_timeout` --
    ///     not just once `upgrade_sni_preread` eventually runs (which can be
    ///     one or more `ready()` cycles later, after the backend connects);
    /// (b) a front-timeout firing AFTER that route decision (backend connect
    ///     still pending) must not re-feed `Input::Timeout` into the
    ///     already-decided core: pre-fix, doing so replayed the SAME latched
    ///     `Output::Routed` through `SniPreread::handle_output`'s `Routed`
    ///     arm a second time, double-incrementing `tcp.sni_preread.routed`
    ///     (release) and tripping `debug_assert!(self.outcome.is_none(),
    ///     ...)` (debug -- this test runs in a debug build, so pre-fix it
    ///     panics here).
    #[test]
    fn frontend_timeout_restored_and_timeout_after_route_is_not_double_counted() {
        let ServerParts {
            registry,
            sessions,
            pool,
            backends,
            ..
        } = prebuild_server(16, 16384, false).expect("could not prebuild a test server");
        let proxy = Rc::new(RefCell::new(TcpProxy::new(
            registry,
            sessions,
            pool.clone(),
            backends,
        )));

        let mut bare_listener = test_listener();
        bare_listener
            .insert_sni_route("example.com".to_owned(), vec![], "cluster-a".into())
            .expect("insert_sni_route must succeed for a valid test SNI");
        let configured_front_timeout = bare_listener.config.front_timeout;
        let listener = Rc::new(RefCell::new(bare_listener));

        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let addr = std_listener.local_addr().expect("listener local addr");
        let mut client = std::net::TcpStream::connect(addr).expect("connect test client");
        let (server, server_peer) = std_listener.accept().expect("accept test server");
        server.set_nonblocking(true).expect("server nonblocking");

        let (front_buffer, back_buffer) = {
            let mut pool = pool.borrow_mut();
            (
                pool.checkout().expect("front buffer checkout must succeed"),
                pool.checkout().expect("back buffer checkout must succeed"),
            )
        };

        let mut session = TcpSession::new_sni_preread(
            back_buffer,
            Duration::from_secs(30),
            Duration::from_secs(30),
            front_buffer,
            Token(0),
            listener,
            proxy,
            MioTcpStream::from_std(server),
            server_peer,
            Duration::from_millis(0),
            Duration::from_secs(3),
            16384,
        );

        {
            use std::io::Write as _;
            client
                .write_all(&minimal_client_hello_wire("example.com"))
                .expect("write ClientHello");
            client.flush().ok();
        }

        let routed_before = sni_preread_routed_count();
        for _ in 0..10 {
            if session.cluster_id.is_some() {
                break;
            }
            let _ = session.readable();
        }
        assert_eq!(
            session.cluster_id.as_deref(),
            Some("cluster-a"),
            "the session must have routed on a valid ClientHello for a configured SNI"
        );
        assert_eq!(
            sni_preread_routed_count() - routed_before,
            1,
            "routing must count tcp.sni_preread.routed exactly once"
        );

        // (a) front_timeout is restored the moment routing succeeds.
        assert_eq!(
            session.container_frontend_timeout.duration(),
            Duration::from_secs(configured_front_timeout as u64),
            "the frontend timeout must already carry the configured front_timeout right after \
             routing, not only once the backend connects"
        );

        // (b) a timeout firing after the route already latched must not
        // double-count tcp.sni_preread.routed, and (running in a debug
        // build) must not panic on SniPreread::handle_output's
        // `debug_assert!(self.outcome.is_none(), ...)`.
        let _ = session.timeout(Token(0));
        assert_eq!(
            sni_preread_routed_count() - routed_before,
            1,
            "a timeout firing after the route already latched must not double-count \
             tcp.sni_preread.routed"
        );

        drop(client);
    }

    /// A complete PROXY-v2 `PROXY` command header for IPv4, the exact shape
    /// `ExpectProxyProtocol::readable` accepts (28 bytes: 12-byte magic,
    /// version+command, family, address-block length, then the two addresses
    /// and ports).
    fn proxy_protocol_v2_ipv4_header() -> [u8; 28] {
        [
            0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54,
            0x0A, // PROXY-v2 magic
            0x21, // version 2, command PROXY
            0x11, // AF_INET over STREAM
            0x00, 0x0C, // address block length = 12
            127, 0, 0, 1, // source address
            127, 0, 0, 1, // destination address
            0x1F, 0x90, // source port 8080
            0x10, 0x68, // destination port 4200
        ]
    }

    /// A live `ExpectHeader` session plus everything that must outlive it:
    /// dropping either listener would break the connect the tests drive.
    struct ExpectProxyFixture {
        session: Rc<RefCell<TcpSession>>,
        /// The same session, type-erased -- `ready()` takes the handle the
        /// server dispatches with, and `connect_to_backend` inserts a clone
        /// of it under the backend token.
        proxy_session: Rc<RefCell<dyn ProxySession>>,
        frontend_token: Token,
        client: std::net::TcpStream,
        _frontend_listener: std::net::TcpListener,
        _backend_listener: std::net::TcpListener,
    }

    /// Build a session for a TCP cluster configured with `expect_proxy = true`
    /// (`ProxyProtocolConfig::ExpectHeader`), wired to a real bound backend and a real accepted
    /// frontend socket, and registered in the slab exactly like
    /// `create_session` does (the frontend slot is reserved BEFORE the
    /// session exists, so `connect_to_backend`'s backend token lands on a
    /// different key -- `set_back_token` asserts the two differ).
    fn expect_proxy_fixture() -> ExpectProxyFixture {
        let ServerParts {
            registry,
            sessions,
            pool,
            backends,
            ..
        } = prebuild_server(16, 16384, false).expect("could not prebuild a test server");

        let backend_listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind test backend");
        let backend_address = backend_listener
            .local_addr()
            .expect("test backend local addr");
        backends.borrow_mut().add_backend(
            "cluster-expect",
            Backend::new("cluster-expect-1", backend_address, None, None, None),
        );

        let proxy = Rc::new(RefCell::new(TcpProxy::new(
            registry,
            sessions.clone(),
            pool.clone(),
            backends,
        )));
        let listener = Rc::new(RefCell::new(test_listener()));

        let frontend_listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind test frontend listener");
        let frontend_address = frontend_listener
            .local_addr()
            .expect("test frontend local addr");
        let client = std::net::TcpStream::connect(frontend_address).expect("connect test client");
        let (frontend, frontend_peer) = frontend_listener
            .accept()
            .expect("accept the client connection");
        frontend
            .set_nonblocking(true)
            .expect("frontend socket nonblocking");

        let (frontend_buffer, backend_buffer) = {
            let mut pool = pool.borrow_mut();
            (
                pool.checkout().expect("front buffer checkout must succeed"),
                pool.checkout().expect("back buffer checkout must succeed"),
            )
        };

        let frontend_token = {
            let mut session_manager = sessions.borrow_mut();
            let entry = session_manager.slab.vacant_entry();
            let token = Token(entry.key());
            entry.insert(Rc::new(RefCell::new(crate::server::ListenSession {
                protocol: Protocol::TCPListen,
            })));
            token
        };

        let session = Rc::new(RefCell::new(TcpSession::new(
            backend_buffer,
            None,
            Some("cluster-expect".into()),
            Duration::from_secs(30),
            Duration::from_secs(30),
            Duration::from_secs(30),
            frontend_buffer,
            frontend_token,
            listener,
            Some(ProxyProtocolConfig::ExpectHeader),
            proxy,
            MioTcpStream::from_std(frontend),
            frontend_peer,
            Duration::from_millis(0),
        )));
        let proxy_session: Rc<RefCell<dyn ProxySession>> = session.clone();
        sessions.borrow_mut().slab[frontend_token.0] = proxy_session.clone();

        assert!(
            matches!(
                session.borrow().state,
                TcpStateMachine::ExpectProxyProtocol(_)
            ),
            "an ExpectHeader cluster must start its session in the expect state"
        );

        ExpectProxyFixture {
            session,
            proxy_session,
            frontend_token,
            client,
            _frontend_listener: frontend_listener,
            _backend_listener: backend_listener,
        }
    }

    /// A TCP cluster configured with `expect_proxy = true`
    /// (`ProxyProtocolConfig::ExpectHeader`) starts its sessions
    /// in [`TcpStateMachine::ExpectProxyProtocol`], a state with NO backend
    /// side at all: `back_readiness` returns `None` and `set_back_socket`
    /// panics outright ("We should not set the back socket for the expect
    /// proxy protocol"). The backend may only be dialed once the inbound
    /// header has been parsed and `upgrade_expect` has swapped in a `Pipe`.
    ///
    /// `ready_inner`'s top-of-function connect gate, however, fires on the
    /// FIRST readiness event of a freshly accepted socket -- including the
    /// WRITABLE epoll reports for a socket that has sent no byte yet. Without
    /// the `ExpectProxyProtocol` arm of `attempt_backend_connect_if_needed`'s
    /// guard, completing the TCP handshake was enough to drive
    /// `connect_to_backend` into that panic, killing the worker and every
    /// other session on it.
    ///
    /// To SEE THIS RED: delete the `TcpStateMachine::ExpectProxyProtocol(_)`
    /// arm from `attempt_backend_connect_if_needed`'s early-return guard.
    /// The first `ready()` below then panics inside `connect_to_backend` ->
    /// `set_back_socket` before any assertion is reached.
    #[test]
    fn expect_proxy_dials_the_backend_only_after_the_header_is_parsed() {
        use std::io::Write as _;

        let mut fixture = expect_proxy_fixture();
        let (session, proxy_session, frontend_token) = (
            fixture.session.clone(),
            fixture.proxy_session.clone(),
            fixture.frontend_token,
        );

        // The handshake alone: epoll reports the accepted socket WRITABLE
        // before the client has sent a single byte.
        session
            .borrow_mut()
            .update_readiness(frontend_token, Ready::WRITABLE);
        let closed = session.borrow_mut().ready(proxy_session.clone());
        assert!(
            !closed,
            "a session still waiting for its PROXY header must stay open"
        );
        {
            let session = session.borrow();
            assert!(
                matches!(session.state, TcpStateMachine::ExpectProxyProtocol(_)),
                "the session must still be expecting its PROXY header"
            );
            assert_eq!(
                session.back_connected(),
                BackendConnectionStatus::NotConnected,
                "no backend may be dialed while the PROXY header is still unparsed"
            );
            assert!(
                session.backend_token.is_none(),
                "no backend token may be wired while the PROXY header is still unparsed"
            );
        }

        // The header lands: NOW the session upgrades out of the expect state
        // and that very same connect gate is allowed to dial.
        fixture
            .client
            .write_all(&proxy_protocol_v2_ipv4_header())
            .expect("write the PROXY-v2 header");
        fixture.client.flush().ok();
        session
            .borrow_mut()
            .update_readiness(frontend_token, Ready::READABLE);
        let _ = session.borrow_mut().ready(proxy_session.clone());
        {
            let session = session.borrow();
            assert!(
                !matches!(session.state, TcpStateMachine::ExpectProxyProtocol(_)),
                "a parsed PROXY header must move the session out of the expect state"
            );
            assert_ne!(
                session.back_connected(),
                BackendConnectionStatus::NotConnected,
                "the backend must be dialed once the header has been parsed"
            );
            assert!(
                session.backend_token.is_some(),
                "a dialed backend must own its backend token"
            );
        }
    }

    /// Queue `wire` on the fixture's client, half-close its write side, then
    /// observe the accepted frontend socket through a real epoll instance
    /// registered only AFTER both the bytes and the FIN are queued.
    ///
    /// On Linux a `shutdown(SHUT_WR)` behind pending bytes is reported as one
    /// event carrying `EPOLLIN | EPOLLRDHUP`, which `Ready::from(&Event)`
    /// maps to `READABLE | HUP` without `WRITE_CLOSED`: the peer ended its
    /// request stream but still owns a usable response stream. The asserts
    /// pin that coalesced shape, so the tests that call this exercise the
    /// real kernel event rather than a synthetic readiness word.
    #[cfg(target_os = "linux")]
    fn half_close_and_observe_frontend(fixture: &mut ExpectProxyFixture, wire: &[u8]) -> Ready {
        use std::{io::Write as _, net::Shutdown, os::fd::AsRawFd, time::Duration};

        use mio::{Events, Interest, Poll, Token, unix::SourceFd};

        fixture
            .client
            .write_all(wire)
            .expect("write the request stream");
        fixture
            .client
            .shutdown(Shutdown::Write)
            .expect("half-close the client write side");

        let frontend_fd = fixture.session.borrow().state.front_socket().as_raw_fd();
        let mut source = SourceFd(&frontend_fd);
        let mut poll = Poll::new().expect("create frontend poll");
        let observed_token = Token(7);
        poll.registry()
            .register(
                &mut source,
                observed_token,
                Interest::READABLE | Interest::WRITABLE,
            )
            .expect("register accepted frontend socket");
        let mut events = Events::with_capacity(4);
        poll.poll(&mut events, Some(Duration::from_secs(2)))
            .expect("poll accepted frontend socket");
        let ready = events
            .iter()
            .find(|event| event.token() == observed_token)
            .map(Ready::from)
            .expect("the queued request and FIN produce an epoll event");
        assert!(
            ready.is_readable(),
            "the queued FIN must be reported readable: {ready:?}"
        );
        assert!(
            ready.is_hup(),
            "the peer FIN must be reported as HUP: {ready:?}"
        );
        assert!(
            !ready.is_write_closed(),
            "EPOLLRDHUP leaves the response direction open: {ready:?}"
        );
        ready
    }

    /// A real Linux `shutdown(SHUT_WR)` can report the final request bytes
    /// and `EPOLLRDHUP` in the same epoll event (`READABLE | HUP`, see
    /// `half_close_and_observe_frontend`). An `ExpectProxyProtocol` session
    /// must consume the complete PROXY-v2 header before acting on HUP,
    /// upgrade to `Pipe`, drain the payload already queued behind the
    /// header, and forward it to the backend (sozu-proxy/sozu#1823).
    ///
    /// To SEE THIS RED: delete the `ExpectProxyProtocol` drain that
    /// `TcpSession::ready_inner` runs before its frontend-HUP check. HUP is
    /// then handled first, `front_hup`'s wildcard arm closes the expect state
    /// before the already-buffered header is parsed, and the `!closed`
    /// assertion fails.
    #[cfg(target_os = "linux")]
    #[test]
    fn expect_proxy_half_close_drains_payload_to_the_backend() {
        use std::{io::Read as _, time::Duration};

        let mut fixture = expect_proxy_fixture();

        let payload = b"request-after-proxy-header";
        let mut wire = proxy_protocol_v2_ipv4_header().to_vec();
        wire.extend_from_slice(payload);
        let ready = half_close_and_observe_frontend(&mut fixture, &wire);

        fixture
            .session
            .borrow_mut()
            .update_readiness(fixture.frontend_token, ready);
        let closed = fixture
            .session
            .borrow_mut()
            .ready(fixture.proxy_session.clone());
        assert!(
            !closed,
            "READABLE|HUP must parse the header and retain the response direction"
        );
        assert!(
            !matches!(
                fixture.session.borrow().state,
                TcpStateMachine::ExpectProxyProtocol(_)
            ),
            "the complete header must upgrade the expect state"
        );

        let (mut backend, _) = fixture
            ._backend_listener
            .accept()
            .expect("the parsed header triggers the backend dial");
        backend
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set backend read timeout");
        backend
            .set_write_timeout(Some(Duration::from_secs(2)))
            .expect("set backend write timeout");

        let backend_token = fixture
            .session
            .borrow()
            .backend_token
            .expect("the upgraded session owns a backend token");
        // The backend connects and the payload drained behind the header is
        // flushed to it. Whether the pipe then stays open for a response is
        // `Pipe::check_connections`' half-close policy (it keeps a
        // frontend-closed pipe only while request bytes are in flight), the
        // same for every TCP session with or without a PROXY header, so it is
        // deliberately not asserted here.
        fixture
            .session
            .borrow_mut()
            .update_readiness(backend_token, Ready::WRITABLE);
        let _ = fixture
            .session
            .borrow_mut()
            .ready(fixture.proxy_session.clone());

        let mut backend_received = vec![0; payload.len()];
        backend
            .read_exact(&mut backend_received)
            .expect("backend receives the payload queued before FIN");
        assert_eq!(backend_received, payload);
    }

    /// The negative space of the drain above: a bare TCP health check
    /// (connect, zero bytes, FIN) observed as `READABLE | HUP` must still
    /// close at once and never dial the backend. The drain reads EOF with
    /// nothing accumulated, and `ExpectProxyProtocol::readable`'s zero-byte
    /// `SocketResult::Closed` arm closes the session -- the same outcome as
    /// when the FIN arrives in its own event.
    #[cfg(target_os = "linux")]
    #[test]
    fn expect_proxy_zero_byte_half_close_closes_without_dialing() {
        let mut fixture = expect_proxy_fixture();
        let ready = half_close_and_observe_frontend(&mut fixture, &[]);

        fixture
            .session
            .borrow_mut()
            .update_readiness(fixture.frontend_token, ready);
        let closed = fixture
            .session
            .borrow_mut()
            .ready(fixture.proxy_session.clone());
        assert!(
            closed,
            "a zero-byte client half-close must close the session"
        );
        let session = fixture.session.borrow();
        assert_eq!(
            session.back_connected(),
            BackendConnectionStatus::NotConnected,
            "a zero-byte health check must never dial the backend"
        );
        assert!(
            session.backend_token.is_none(),
            "a zero-byte health check must never wire a backend token"
        );
    }

    /// A client that sends only part of a PROXY-v2 header and then FIN can
    /// never complete it: once the drain has consumed every readable byte,
    /// the frontend HUP must still close the session instead of leaving it
    /// to the frontend timeout, and no backend may be dialed.
    #[cfg(target_os = "linux")]
    #[test]
    fn expect_proxy_partial_header_half_close_closes_without_dialing() {
        let mut fixture = expect_proxy_fixture();
        let header = proxy_protocol_v2_ipv4_header();
        let ready = half_close_and_observe_frontend(&mut fixture, &header[..10]);

        fixture
            .session
            .borrow_mut()
            .update_readiness(fixture.frontend_token, ready);
        let closed = fixture
            .session
            .borrow_mut()
            .ready(fixture.proxy_session.clone());
        assert!(
            closed,
            "an incomplete PROXY header followed by FIN must close the session"
        );
        let session = fixture.session.borrow();
        assert_eq!(
            session.back_connected(),
            BackendConnectionStatus::NotConnected,
            "an incomplete PROXY header must never dial the backend"
        );
    }

    /// Structural safety for `ready_inner`'s `Connecting` branch: it reads
    /// the backend readiness, which `back_readiness` has none of for
    /// `ExpectProxyProtocol` (and `unreachable!`s on for `FailedUpgrade`).
    ///
    /// No input sequence reaches this pairing -- `connect_to_backend` is the
    /// only writer of `Connecting` and can no longer run in the expect state
    /// (see the test above), and no transition ever enters
    /// `ExpectProxyProtocol` from another state -- so the state is
    /// synthesized here directly. The branch must degrade to a logged close
    /// rather than take the whole worker down with it: a backend handshake
    /// with no readiness slot can never complete.
    ///
    /// To SEE THIS RED: restore either `self.back_readiness().unwrap()` in
    /// `ready_inner`'s `Connecting` branch -- the `None` this test installs
    /// then panics with "called `Option::unwrap()` on a `None` value".
    #[test]
    fn connecting_backend_without_readiness_closes_instead_of_panicking() {
        let fixture = expect_proxy_fixture();
        let (session, proxy_session) = (fixture.session.clone(), fixture.proxy_session.clone());

        // The invariant break itself: `Connecting` while the state still has
        // no backend side.
        session.borrow_mut().backend_connected =
            BackendConnectionStatus::Connecting(Instant::now());
        assert!(
            session.borrow_mut().back_readiness().is_none(),
            "the expect state must have no backend readiness -- the premise of this test"
        );

        let closed = session.borrow_mut().ready(proxy_session.clone());
        assert!(
            closed,
            "a connecting backend with no readiness slot must close the session"
        );
    }

    /// Drive an `ExpectHeader` fixture through its PROXY header, which
    /// upgrades it to `Pipe` and dials the backend in that same `ready()`.
    fn expect_proxy_fixture_dialed() -> ExpectProxyFixture {
        use std::io::Write as _;

        let mut fixture = expect_proxy_fixture();
        fixture
            .client
            .write_all(&proxy_protocol_v2_ipv4_header())
            .expect("write the PROXY-v2 header");
        fixture
            .session
            .borrow_mut()
            .update_readiness(fixture.frontend_token, Ready::READABLE);
        let proxy_session = fixture.proxy_session.clone();
        let _ = fixture.session.borrow_mut().ready(proxy_session);
        {
            let session = fixture.session.borrow();
            assert!(
                matches!(session.state, TcpStateMachine::Pipe(_)),
                "precondition: the parsed header upgrades the session to Pipe"
            );
            assert!(
                session.backend_token.is_some(),
                "precondition: the Pipe dialed its backend"
            );
        }
        fixture
    }

    /// The `Pipe`'s access log reports the address `connect_to_backend`
    /// dialed, handed over by `TcpSession::set_backend_address`, rather than
    /// a `getpeername(2)` on the backend socket.
    ///
    /// To SEE THIS RED: remove the `self.set_backend_address(...)` call from
    /// `connect_to_backend`. The pipe then reports `None`.
    #[test]
    fn a_dialed_pipe_reports_the_backend_address_it_was_given() {
        let fixture = expect_proxy_fixture_dialed();
        let backend_address = fixture
            ._backend_listener
            .local_addr()
            .expect("test backend local addr");
        let session = fixture.session.borrow();
        let TcpStateMachine::Pipe(pipe) = &session.state else {
            unreachable!("expect_proxy_fixture_dialed checked the Pipe state");
        };
        assert_eq!(
            pipe.get_backend_address(),
            Some(backend_address),
            "the pipe must report the backend address the session dialed"
        );
    }

    /// Closing a TCP session issues no `EPOLL_CTL_DEL` for its front or
    /// back socket: the last `close(2)` of each, when the server drops the
    /// session right after `close()`, takes it out of the epoll set.
    ///
    /// The test reads the kernel's own epoll table (`/proc/self/fdinfo`), so
    /// it tells "never deregistered" from "removed by the close".
    ///
    /// To SEE THIS RED: put back
    /// `let _ = self.proxy.borrow().registry.deregister(&mut mio::unix::SourceFd(&front_socket.as_raw_fd()));`
    /// in `TcpSession::close` (or the matching backend call in
    /// `close_backend`). The first assertion after `close()` then fails.
    #[cfg(target_os = "linux")]
    #[test]
    fn closed_tcp_sessions_leave_their_sockets_to_close() {
        use std::os::fd::AsRawFd as _;

        use crate::protocol::mux::test_support::epoll_watches;

        let fixture = expect_proxy_fixture_dialed();
        let session = fixture.session.clone();
        let frontend_token = fixture.frontend_token;
        let registry = session
            .borrow()
            .proxy
            .borrow()
            .registry
            .try_clone()
            .expect("the test registry must clone");

        // `create_session` registers the front socket; the fixture builds the
        // session directly, so register it the same way here.
        let (front_fd, back_fd) = {
            let mut session = session.borrow_mut();
            let front_fd = session.state.front_socket().as_raw_fd();
            registry
                .register(
                    &mut mio::unix::SourceFd(&front_fd),
                    frontend_token,
                    Interest::READABLE | Interest::WRITABLE,
                )
                .expect("register the front socket");
            let back_fd = session
                .back_socket_mut()
                .expect("the dialed Pipe owns a backend socket")
                .as_raw_fd();
            (front_fd, back_fd)
        };
        assert!(
            epoll_watches(&registry, front_fd) && epoll_watches(&registry, back_fd),
            "precondition: both sockets are in the epoll set"
        );

        // What `Server::shut_down_sessions_by_frontend_tokens` does.
        let removed = session
            .borrow()
            .proxy
            .borrow()
            .sessions
            .borrow_mut()
            .slab
            .remove(frontend_token.0);
        session.borrow_mut().close();
        assert!(
            epoll_watches(&registry, front_fd),
            "TcpSession::close must not deregister a front socket it is about \
             to close: the close removes it, the EPOLL_CTL_DEL is a wasted syscall"
        );
        assert!(
            epoll_watches(&registry, back_fd),
            "TcpSession::close_backend must not deregister a backend socket it \
             is about to close: the close removes it"
        );

        drop(removed);
        drop(session);
        drop(fixture);
        assert!(
            !epoll_watches(&registry, front_fd),
            "dropping the session closes the front socket's only descriptor, \
             which must take it out of the epoll set"
        );
        assert!(
            !epoll_watches(&registry, back_fd),
            "dropping the session closes the backend socket's only descriptor, \
             which must take it out of the epoll set"
        );
    }

    /// A session dialed from a state that is not `Pipe` yet, plus everything
    /// that must outlive it.
    struct DialFixture {
        session: Rc<RefCell<TcpSession>>,
        proxy_session: Rc<RefCell<dyn ProxySession>>,
        frontend_token: Token,
        backend_address: SocketAddr,
        client: std::net::TcpStream,
        _frontend_listener: std::net::TcpListener,
        _backend_listener: std::net::TcpListener,
    }

    /// Build a session for cluster `cluster-dial`, wired to a real bound
    /// backend and a real accepted frontend socket, registered in the slab the
    /// way `expect_proxy_fixture` does. `build` picks the constructor, hence
    /// the starting state; `sni_route` routes `example.com` to the cluster.
    fn dial_fixture(
        sni_route: bool,
        build: impl FnOnce(
            Checkout,
            Checkout,
            Token,
            Rc<RefCell<TcpListener>>,
            Rc<RefCell<TcpProxy>>,
            MioTcpStream,
            SocketAddr,
        ) -> TcpSession,
    ) -> DialFixture {
        let ServerParts {
            registry,
            sessions,
            pool,
            backends,
            ..
        } = prebuild_server(16, 16384, false).expect("could not prebuild a test server");

        let backend_listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind test backend");
        let backend_address = backend_listener
            .local_addr()
            .expect("test backend local addr");
        backends.borrow_mut().add_backend(
            "cluster-dial",
            Backend::new("cluster-dial-1", backend_address, None, None, None),
        );

        let proxy = Rc::new(RefCell::new(TcpProxy::new(
            registry,
            sessions.clone(),
            pool.clone(),
            backends,
        )));
        let mut bare_listener = test_listener();
        if sni_route {
            bare_listener
                .insert_sni_route("example.com".to_owned(), vec![], "cluster-dial".into())
                .expect("insert_sni_route must succeed for a valid test SNI");
        }
        let listener = Rc::new(RefCell::new(bare_listener));

        let frontend_listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind test frontend listener");
        let client = std::net::TcpStream::connect(
            frontend_listener
                .local_addr()
                .expect("test frontend local addr"),
        )
        .expect("connect test client");
        let (frontend, frontend_peer) = frontend_listener
            .accept()
            .expect("accept the client connection");
        frontend
            .set_nonblocking(true)
            .expect("frontend socket nonblocking");

        let (frontend_buffer, backend_buffer) = {
            let mut pool = pool.borrow_mut();
            (
                pool.checkout().expect("front buffer checkout must succeed"),
                pool.checkout().expect("back buffer checkout must succeed"),
            )
        };

        let frontend_token = {
            let mut session_manager = sessions.borrow_mut();
            let entry = session_manager.slab.vacant_entry();
            let token = Token(entry.key());
            entry.insert(Rc::new(RefCell::new(crate::server::ListenSession {
                protocol: Protocol::TCPListen,
            })));
            token
        };

        let session = Rc::new(RefCell::new(build(
            backend_buffer,
            frontend_buffer,
            frontend_token,
            listener,
            proxy,
            MioTcpStream::from_std(frontend),
            frontend_peer,
        )));
        let proxy_session: Rc<RefCell<dyn ProxySession>> = session.clone();
        sessions.borrow_mut().slab[frontend_token.0] = proxy_session.clone();

        DialFixture {
            session,
            proxy_session,
            frontend_token,
            backend_address,
            client,
            _frontend_listener: frontend_listener,
            _backend_listener: backend_listener,
        }
    }

    /// A legacy (non-SNI) session whose cluster uses `proxy_protocol`; its
    /// first `ready()` dials the backend while it is still in its
    /// PROXY-protocol state.
    fn proxy_protocol_session_dialed(proxy_protocol: ProxyProtocolConfig) -> DialFixture {
        let fixture = dial_fixture(
            false,
            |back_buffer, front_buffer, token, listener, proxy, socket, peer| {
                TcpSession::new(
                    back_buffer,
                    None,
                    Some("cluster-dial".into()),
                    Duration::from_secs(30),
                    Duration::from_secs(30),
                    Duration::from_secs(30),
                    front_buffer,
                    token,
                    listener,
                    Some(proxy_protocol),
                    proxy,
                    socket,
                    peer,
                    Duration::from_millis(0),
                )
            },
        );
        fixture
            .session
            .borrow_mut()
            .update_readiness(fixture.frontend_token, Ready::WRITABLE);
        let closed = fixture
            .session
            .borrow_mut()
            .ready(fixture.proxy_session.clone());
        assert!(!closed, "precondition: the dial keeps the session open");
        fixture
    }

    /// Upgrade a dialed, not-yet-`Pipe` session and assert the `Pipe` reports
    /// the backend address the session dialed: the address its access log
    /// carries.
    fn assert_upgraded_pipe_reports_the_dialed_address(fixture: &DialFixture) {
        {
            let session = fixture.session.borrow();
            assert!(
                !matches!(session.state, TcpStateMachine::Pipe(_)),
                "precondition: the backend was dialed before the Pipe existed"
            );
            assert!(
                session.backend_token.is_some(),
                "precondition: the session dialed its backend"
            );
        }
        let closed = fixture.session.borrow_mut().upgrade();
        assert!(!closed, "the upgrade to Pipe must succeed");
        let session = fixture.session.borrow();
        let TcpStateMachine::Pipe(pipe) = &session.state else {
            panic!("the upgrade must install a Pipe");
        };
        assert_eq!(
            pipe.get_backend_address(),
            Some(fixture.backend_address),
            "the Pipe must report the backend address dialed before the upgrade"
        );
    }

    /// `SendProxyProtocol` dials before it becomes a `Pipe`: `upgrade_send`
    /// must hand the dialed address over.
    ///
    /// To SEE THIS RED: make `TcpSession::set_backend_address` record nothing,
    /// or pass `None` to `pipe.set_backend_address` in `upgrade_send`.
    #[test]
    fn a_send_proxy_protocol_pipe_reports_the_dialed_backend_address() {
        let fixture = proxy_protocol_session_dialed(ProxyProtocolConfig::SendHeader);
        assert!(
            matches!(
                fixture.session.borrow().state,
                TcpStateMachine::SendProxyProtocol(_)
            ),
            "precondition: a SendHeader cluster starts in the send state"
        );
        assert_upgraded_pipe_reports_the_dialed_address(&fixture);
    }

    /// `RelayProxyProtocol` dials before it becomes a `Pipe`: `upgrade_relay`
    /// must hand the dialed address over.
    ///
    /// To SEE THIS RED: make `TcpSession::set_backend_address` record nothing,
    /// or pass `None` to `pipe.set_backend_address` in `upgrade_relay`.
    #[test]
    fn a_relay_proxy_protocol_pipe_reports_the_dialed_backend_address() {
        let fixture = proxy_protocol_session_dialed(ProxyProtocolConfig::RelayHeader);
        assert!(
            matches!(
                fixture.session.borrow().state,
                TcpStateMachine::RelayProxyProtocol(_)
            ),
            "precondition: a RelayHeader cluster starts in the relay state"
        );
        assert_upgraded_pipe_reports_the_dialed_address(&fixture);
    }

    /// An SNI-routed session dials from `SniPreread`: `upgrade_sni_preread`
    /// builds the `Pipe` through `build_pipe_from_preread`, which must hand
    /// the dialed address over.
    ///
    /// To SEE THIS RED: make `TcpSession::set_backend_address` record nothing,
    /// or pass `None` to `pipe.set_backend_address` in
    /// `build_pipe_from_preread`.
    #[test]
    fn an_sni_preread_pipe_reports_the_dialed_backend_address() {
        use std::io::Write as _;

        let mut fixture = dial_fixture(
            true,
            |back_buffer, front_buffer, token, listener, proxy, socket, peer| {
                TcpSession::new_sni_preread(
                    back_buffer,
                    Duration::from_secs(30),
                    Duration::from_secs(30),
                    front_buffer,
                    token,
                    listener,
                    proxy,
                    socket,
                    peer,
                    Duration::from_millis(0),
                    Duration::from_secs(3),
                    16384,
                )
            },
        );
        fixture
            .client
            .write_all(&minimal_client_hello_wire("example.com"))
            .expect("write ClientHello");
        fixture.client.flush().ok();
        for _ in 0..10 {
            if fixture.session.borrow().cluster_id.is_some() {
                break;
            }
            let _ = fixture.session.borrow_mut().readable();
        }
        assert_eq!(
            fixture.session.borrow().cluster_id.as_deref(),
            Some("cluster-dial"),
            "precondition: the ClientHello routes the session"
        );
        let closed = fixture
            .session
            .borrow_mut()
            .ready(fixture.proxy_session.clone());
        assert!(!closed, "precondition: the dial keeps the session open");
        assert!(
            matches!(
                fixture.session.borrow().state,
                TcpStateMachine::SniPreread(_)
            ),
            "precondition: the routed session dials while still in SniPreread"
        );
        assert_upgraded_pipe_reports_the_dialed_address(&fixture);
    }
}

/// Coverage for the `RelayProxyProtocol` -> `Pipe` handoff. It lives here
/// rather than in `relay.rs`'s own `relay_test` because `into_pipe` demands an
/// `Rc<RefCell<TcpListener>>` and every field of that struct — plus its
/// constructor — is private to this module, so the relay's test module cannot
/// build one. A separate module (rather than the `tests` module above) avoids
/// that module's `use std::net::TcpListener` shadowing `super::TcpListener`.
#[cfg(test)]
mod relay_upgrade_tests {
    use std::net::TcpListener as StdTcpListener;

    use sozu_command::{config::ListenerBuilder, proto::command::SocketAddress};

    use super::*;
    use crate::{pool::Pool, testing::provide_port};

    fn test_listener() -> TcpListener {
        let config = ListenerBuilder::new_tcp(SocketAddress::new_v4(127, 0, 0, 1, provide_port()))
            .to_tcp(None)
            .expect("could not build a TcpListenerConfig for the test");
        TcpListener::new(config, Token(0)).expect("could not build a bare TcpListener for the test")
    }

    fn connected_pair() -> (std::net::TcpStream, std::net::TcpStream) {
        let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind the test listener");
        let address = listener
            .local_addr()
            .expect("the test listener must expose its address");
        let local = std::net::TcpStream::connect(address).expect("connect the test pair");
        let (peer, _addr) = listener.accept().expect("accept the test pair");
        local
            .set_nonblocking(true)
            .expect("the local end must be non-blocking");
        peer.set_nonblocking(true)
            .expect("the peer end must be non-blocking");
        (local, peer)
    }

    /// `Pipe::new` ends with `arm_inherited_buffer_writes`, which arms the
    /// backend WRITABLE readiness when the inherited `frontend_buffer` is
    /// non-empty. `build_pipe_from_preread` carries a comment stating outright
    /// that a bare `pipe.backend_readiness.event = ..` pair must NOT be used
    /// in its place, because the drain of that buffer must not depend on the
    /// restored event carrying WRITABLE itself — and `upgrade_send` re-runs the
    /// arm for the same reason (sozu-proxy/sozu#1279's close-before-flush
    /// truncation). `RelayProxyProtocol::into_pipe` was the one pipe-entry path
    /// still using the bare pair. That was harmless only while the relay's
    /// buffer was always empty at upgrade time; now that the relay leaves
    /// pipelined client payload in it, this path joins the class those comments
    /// describe, and the surviving wake-up must be by construction rather than
    /// by the accident that `back_writable` happens to be entered with WRITABLE
    /// already set.
    ///
    /// To SEE THIS RED: in `relay.rs::into_pipe`, replace the
    /// `pipe.restore_readiness_events(..)` call with the bare pair
    /// `pipe.frontend_readiness.event = self.frontend_readiness.event;` /
    /// `pipe.backend_readiness.event = self.backend_readiness.event;`.
    #[test]
    fn into_pipe_rearms_the_backend_write_for_pipelined_payload() {
        let listener = Rc::new(RefCell::new(test_listener()));
        let (frontend, _front_peer) = connected_pair();
        let (backend, _backend_peer) = connected_pair();

        let mut pool = Pool::with_capacity(2, 2, 4096);
        let mut frontend_buffer = pool.checkout().expect("frontend buffer");
        let backend_buffer = pool.checkout().expect("backend buffer");
        // What the relay leaves behind once the header is forwarded: the
        // payload the client pipelined into the same read.
        let payload = b"pipelined-payload";
        frontend_buffer.space()[..payload.len()].copy_from_slice(payload);
        frontend_buffer.fill(payload.len());

        let mut relay = RelayProxyProtocol::new(
            MioTcpStream::from_std(frontend),
            Token(0),
            Ulid::generate(),
            Some(MioTcpStream::from_std(backend)),
            frontend_buffer,
        );
        // `into_pipe` never reads this, but a relay only ever upgrades with a
        // fully forwarded header, so leave the state coherent.
        relay.header_size = Some(28);
        relay.set_back_token(Token(1));
        // The case the `build_pipe_from_preread` comment names explicitly: the
        // readiness word restored from the previous state need not itself carry
        // WRITABLE, and the inherited drain must not depend on it.
        relay.backend_readiness.event = Ready::EMPTY;
        relay.frontend_readiness.event = Ready::EMPTY;

        let pipe = relay.into_pipe(backend_buffer, listener);

        assert!(
            pipe.backend_readiness.event.is_writable(),
            "a non-empty inherited frontend buffer must leave a backend WRITABLE event queued, or the pipelined payload is never flushed"
        );
        assert!(
            pipe.backend_readiness.interest.is_writable(),
            "the inherited-write arm must also hold the backend WRITABLE interest"
        );
    }
}
