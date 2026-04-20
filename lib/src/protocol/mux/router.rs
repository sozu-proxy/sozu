//! Backend routing and connection reuse for the mux layer.
//!
//! [`Router`] owns the map of token -> backend [`Connection`] and centralises
//! the logic for picking (or opening) the right backend for an incoming
//! request. The H2 reuse strategy prefers the least-loaded non-draining
//! connection of the target cluster; H1 falls back to keep-alive reuse.

use std::{cell::RefCell, collections::HashMap, rc::Rc, time::Duration};

use mio::{Interest, Token, net::TcpStream};
use sozu_command::{logging::is_logger_colored, proto::command::ListenerType};

use super::{
    BackendStatus, Connection, Context, DebugEvent, GlobalStreamId, Position, StreamState,
};
use crate::{
    BackendConnectionError, L7ListenerHandler, L7Proxy, ListenerHandler, ProxySession, Readiness,
    RetrieveClusterError,
    backends::{Backend, BackendError},
    protocol::http::editor::HttpContext,
    router::Route,
    server::CONN_RETRIES,
    socket::SessionTcpStream,
    timer::TimeoutContainer,
};

/// Module-level prefix used on every log line emitted from the router. The
/// router has no direct view of a frontend session so a single `MUX-ROUTER`
/// label is used, colored bold bright-white (uniform across every protocol)
/// when the logger supports ANSI.
macro_rules! log_module_context {
    () => {{
        let colored = is_logger_colored();
        let (open, reset) = if colored {
            ("\x1b[1;97m", "\x1b[0m")
        } else {
            ("", "")
        };
        format!("{open}MUX-ROUTER{reset}\t >>>", open = open, reset = reset)
    }};
}

#[derive(Debug)]
pub struct Router {
    pub backends: HashMap<Token, Connection<SessionTcpStream>>,
    pub configured_backend_timeout: Duration,
    pub configured_connect_timeout: Duration,
    /// Fallback readiness used when a backend token is missing from the map.
    /// This prevents panicking in the Endpoint trait methods that return references.
    pub(super) fallback_readiness: Readiness,
}

impl Router {
    pub fn new(configured_backend_timeout: Duration, configured_connect_timeout: Duration) -> Self {
        Self {
            backends: HashMap::new(),
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
    ) -> Result<(), BackendConnectionError> {
        let stream = &mut context.streams[stream_id];
        // when reused, a stream should be detached from its old connection, if not we could end
        // with concurrent connections on a single endpoint
        if !matches!(stream.state, StreamState::Link) {
            error!(
                "{} stream {} expected to be in Link state, got {:?}",
                log_module_context!(),
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
            return Err(BackendConnectionError::MaxConnectionRetries(
                stream.context.cluster_id.clone(),
            ));
        }
        stream.attempts += 1;

        let stream_context = &mut stream.context;
        let cluster_id = self
            .route_from_request(stream_context, &context.listener)
            .map_err(BackendConnectionError::RetrieveClusterError)?;
        stream_context.cluster_id = Some(cluster_id.to_owned());

        let (frontend_should_stick, frontend_should_redirect_https, h2) = proxy
            .borrow()
            .clusters()
            .get(&cluster_id)
            .map(|cluster| {
                (
                    cluster.sticky_session,
                    cluster.https_redirect,
                    cluster.http2.unwrap_or(false),
                )
            })
            .unwrap_or((false, false, false));

        if frontend_should_redirect_https && matches!(proxy.borrow().kind(), ListenerType::Http) {
            return Err(BackendConnectionError::RetrieveClusterError(
                RetrieveClusterError::HttpsRedirect,
            ));
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
                        log_module_context!()
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
                        let stream_count = h2c.streams.len();
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
                            log_module_context!()
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
            "{} connect: {} (stick={}, h2={}) -> (reuse={:?})",
            log_module_context!(),
            cluster_id,
            frontend_should_stick,
            h2,
            reuse_token
        );

        if let Some(token) = reuse_token {
            trace!(
                "{} reused backend: {:#?}",
                log_module_context!(),
                self.backends.get(&token)
            );
            // Link backend to stream for the reused connection path. We check
            // that the backend can accept a new stream before committing any
            // per-stream state.
            let Some(backend_conn) = self.backends.get_mut(&token) else {
                error!(
                    "{} reused backend token {:?} missing from backends map",
                    log_module_context!(),
                    token
                );
                return Err(BackendConnectionError::MaxSessionsMemory);
            };
            if !backend_conn.start_stream(stream_id, context) {
                error!(
                    "{} Backend rejected stream start (max concurrent streams reached)",
                    log_module_context!()
                );
                return Err(BackendConnectionError::MaxSessionsMemory);
            }
            // For reused backends: set context fields and metrics lifecycle
            if let Some(backend_conn) = self.backends.get(&token) {
                if let Position::Client(_, backend_ref, _) = backend_conn.position() {
                    let backend = backend_ref.borrow();
                    let stream = &mut context.streams[stream_id];
                    stream.context.backend_id = Some(backend.backend_id.to_owned());
                    stream.context.backend_address = Some(backend.address);
                    stream.metrics.backend_id = Some(backend.backend_id.to_owned());
                    stream.metrics.backend_start();
                    stream.metrics.backend_connected();
                }
            }
            context.link_stream(stream_id, token);
            return Ok(());
        }

        // New-backend path: fall through.
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
                    log_module_context!(),
                    socket,
                    e
                );
            }

            let socket = SessionTcpStream::new(socket, context.session_ulid);

            // Build an un-armed timeout: we can't call `TimeoutContainer::new`
            // yet because that requires the slab token, and we only allocate
            // the token on the happy path. `.set(token)` below arms it.
            let timeout_container = TimeoutContainer::new_empty(self.configured_connect_timeout);
            let flood_config = context.listener.borrow().get_h2_flood_config();
            let connection_config = context.listener.borrow().get_h2_connection_config();
            let stream_idle_timeout = context.listener.borrow().get_h2_stream_idle_timeout();
            let backend_id_for_gauge = backend.borrow().backend_id.to_owned();
            let mut connection = if h2 {
                match Connection::new_h2_client(
                    context.session_ulid,
                    socket,
                    cluster_id.to_owned(),
                    backend,
                    context.pool.clone(),
                    timeout_container,
                    flood_config,
                    connection_config,
                    stream_idle_timeout,
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
                    timeout_container,
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
                    log_module_context!()
                );
                // `connection` (socket + timeout_container) drops here.
                return Err(BackendConnectionError::MaxSessionsMemory);
            }

            // --- Happy path: commit side-effects in one atomic-ish block ---
            let stream = &mut context.streams[stream_id];
            stream.metrics.backend_start();
            stream.metrics.backend_id = stream.context.backend_id.to_owned();
            gauge_add!("backend.connections", 1);
            gauge_add!(
                "connections_per_backend",
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
                    error!(
                        "{} error registering back socket: {:?}",
                        log_module_context!(),
                        e
                    );
                }
            }

            // Arm the connect timeout now that we own a real token.
            connection.timeout_container().set(token);

            self.backends.insert(token, connection);
            token
        };

        context.link_stream(stream_id, token);
        Ok(())
    }

    fn route_from_request<L: ListenerHandler + L7ListenerHandler>(
        &mut self,
        context: &mut HttpContext,
        listener: &Rc<RefCell<L>>,
    ) -> Result<String, RetrieveClusterError> {
        let (host, uri, method) = match context.extract_route() {
            Ok(tuple) => tuple,
            Err(cluster_error) => {
                // we are past kawa parsing if it succeeded this can't fail
                // if the request was malformed it was caught by kawa and we sent a 400
                error!(
                    "{} Malformed request in connect (should be caught at parsing) {:?}: {}",
                    log_module_context!(),
                    context,
                    cluster_error
                );
                return Err(cluster_error);
            }
        };

        // ── TLS SNI ↔ HTTP :authority binding ─────────────────────────────
        // Reject any request whose authority hostname does not exact-match the
        // SNI negotiated at TLS handshake. Without this check, an attacker
        // holding a valid certificate for tenant A could open TLS with SNI=A
        // (Sōzu serves cert A) then send an H2 stream with
        // `:authority=tenantB.example.com` and reach tenant B's backend,
        // crossing the TLS trust boundary (CWE-346 / CWE-444).
        //
        // Plaintext listeners bypass the check (SNI is always `None`).
        // Operators may opt out per-listener via
        // `HttpsListenerConfig::strict_sni_binding = false`; the flag is
        // captured on `HttpContext` at stream creation to avoid a
        // per-request listener borrow.
        if context.strict_sni_binding {
            if let Some(sni) = context.tls_server_name.as_deref() {
                if !authority_matches_sni(host, sni) {
                    incr!("http.sni_authority_mismatch");
                    warn!(
                        "{} rejecting request: TLS SNI {:?} does not match :authority {:?} (request_id={})",
                        log_module_context!(),
                        sni,
                        host,
                        context.id
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
                trace!("{} {}", log_module_context!(), frontend_error);
                return Err(RetrieveClusterError::RetrieveFrontend(frontend_error));
            }
        };

        let cluster_id = match route {
            Route::ClusterId(id) => id,
            Route::Deny => {
                trace!("{} Route::Deny", log_module_context!());
                return Err(RetrieveClusterError::UnauthorizedRoute);
            }
        };

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
                trace!("{} {}", log_module_context!(), backend_error);
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
    let host = match authority.rsplit_once(':') {
        Some((h, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => authority,
    };
    if host.len() != sni_lowercased.len() {
        return false;
    }
    host.as_bytes()
        .iter()
        .zip(sni_lowercased.as_bytes())
        .all(|(a, b)| a.to_ascii_lowercase() == *b)
}

#[cfg(test)]
mod tests {
    use super::authority_matches_sni;

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
