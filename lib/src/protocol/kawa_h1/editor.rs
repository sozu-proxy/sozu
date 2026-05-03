//! H1 request/response header editor.
//!
//! Captures method/authority/path on parse, rewrites hop-by-hop and
//! forwarding headers (`X-Forwarded-*`, `Forwarded`, `Connection`,
//! WebSocket-upgrade signalling, optional `traceparent`), and surfaces the
//! canonical `LogContext` used by the access-log envelope. Acts as the
//! Kawa `ParserCallbacks` implementation for the H1 mux path.

use std::{
    io::Write as _,
    net::{IpAddr, SocketAddr},
    str::from_utf8,
};

use rusty_ulid::Ulid;
use sozu_command_lib::logging::LogContext;

use crate::{
    Protocol, RetrieveClusterError,
    pool::Checkout,
    protocol::{
        http::{GenericHttpStream, Method, parser::compare_no_case},
        pipe::WebSocketContext,
    },
};

#[cfg(feature = "opentelemetry")]
fn parse_traceparent(val: &kawa::Store, buf: &[u8]) -> Option<([u8; 32], [u8; 16])> {
    let val = val.data(buf);
    let (version, val) = parse_hex::<2>(val)?;
    if version.as_slice() != b"00" {
        return None;
    }
    let val = skip_separator(val)?;
    let (trace_id, val) = parse_hex::<32>(val)?;
    let val = skip_separator(val)?;
    let (parent_id, val) = parse_hex::<16>(val)?;
    let val = skip_separator(val)?;
    let (_, val) = parse_hex::<2>(val)?;
    val.is_empty().then_some((trace_id, parent_id))
}

#[cfg(feature = "opentelemetry")]
fn parse_hex<const N: usize>(buf: &[u8]) -> Option<([u8; N], &[u8])> {
    let val: [u8; N] = buf.get(..N)?.try_into().unwrap();
    val.iter()
        .all(|c| c.is_ascii_hexdigit())
        .then_some((val, &buf[N..]))
}

#[cfg(feature = "opentelemetry")]
fn skip_separator(buf: &[u8]) -> Option<&[u8]> {
    buf.first().filter(|b| **b == b'-').map(|_| &buf[1..])
}

#[cfg(feature = "opentelemetry")]
fn random_id<const N: usize>() -> [u8; N] {
    use rand::RngExt;
    const CHARSET: &[u8] = b"0123456789abcdef";
    let mut rng = rand::rng();
    let mut buf = [0; N];
    buf.fill_with(|| {
        let n = rng.random_range(0..CHARSET.len());
        CHARSET[n]
    });
    buf
}

#[cfg(feature = "opentelemetry")]
fn build_traceparent(trace_id: &[u8; 32], parent_id: &[u8; 16]) -> [u8; 55] {
    let mut buf = [0; 55];
    buf[..3].copy_from_slice(b"00-");
    buf[3..35].copy_from_slice(trace_id);
    buf[35] = b'-';
    buf[36..52].copy_from_slice(parent_id);
    buf[52..55].copy_from_slice(b"-01");
    buf
}

/// Write the ";for=..;by=.." portion of a Forwarded header into `buf`
/// without heap-allocating a `String`.
fn write_forwarded_for_by(buf: &mut Vec<u8>, peer_ip: IpAddr, peer_port: u16, public_ip: IpAddr) {
    buf.extend_from_slice(b";for=\"");
    let _ = write!(buf, "{peer_ip}");
    buf.push(b':');
    let mut port_buf = itoa::Buffer::new();
    buf.extend_from_slice(port_buf.format(peer_port).as_bytes());
    buf.extend_from_slice(b"\";by=");
    match public_ip {
        IpAddr::V4(_) => {
            let _ = write!(buf, "{public_ip}");
        }
        IpAddr::V6(_) => {
            buf.push(b'"');
            let _ = write!(buf, "{public_ip}");
            buf.push(b'"');
        }
    }
}

/// Write ", proto=<proto>;for=..;by=.." (the suffix appended to an existing Forwarded value).
fn write_forwarded_suffix(
    buf: &mut Vec<u8>,
    proto: &str,
    peer_ip: IpAddr,
    peer_port: u16,
    public_ip: IpAddr,
) {
    buf.extend_from_slice(b", proto=");
    buf.extend_from_slice(proto.as_bytes());
    write_forwarded_for_by(buf, peer_ip, peer_port, public_ip);
}

/// This is the container used to store and use information about the session from within a Kawa parser callback
#[derive(Debug)]
pub struct HttpContext {
    // ========== Write only
    /// set to false if Kawa finds a "Connection" header with a "close" value in the response
    pub keep_alive_backend: bool,
    /// set to false if Kawa finds a "Connection" header with a "close" value in the request
    pub keep_alive_frontend: bool,
    /// the value of the sticky session cookie in the request
    pub sticky_session_found: Option<String>,
    // ---------- Status Line
    /// the value of the method in the request line
    pub method: Option<Method>,
    /// the value of the authority of the request (in the request line of "Host" header)
    pub authority: Option<String>,
    /// the value of the path in the request line
    pub path: Option<String>,
    /// the value of the status code in the response line
    pub status: Option<u16>,
    /// the value of the reason in the response line
    pub reason: Option<String>,
    // ---------- Additional optional data
    pub user_agent: Option<String>,
    /// Value of the `x-request-id` header observed (if propagated from the
    /// client/upstream LB) or generated (from `self.id`). Universal correlation
    /// header — populated unconditionally by `on_request_headers` so the access
    /// log can record the exact value forwarded to the backend.
    pub x_request_id: Option<String>,
    /// Verbatim value of the client-supplied `X-Forwarded-For` header as
    /// observed before Sōzu appended its own hop. Captured here, not at
    /// request edit time, so the access log records the upstream-attested
    /// chain even when Sōzu also appends its own peer to the forwarded
    /// header. `None` if the request had no `X-Forwarded-For` header.
    pub xff_chain: Option<String>,

    #[cfg(feature = "opentelemetry")]
    pub otel: Option<sozu_command::logging::OpenTelemetry>,

    // ========== Read only
    /// signals wether Kawa should write a "Connection" header with a "close" value (request and response)
    pub closing: bool,
    /// Connection/session ULID — stable across all requests multiplexed on this
    /// TCP or TLS connection. Used as the first slot in the legacy log-context
    /// bracket `[session req cluster backend]` and emitted into
    /// `ProtobufAccessLog.session_id`.
    pub session_id: Ulid,
    /// the value of the custom header, named "Sozu-Id", that Kawa should write (request and response)
    pub id: Ulid,
    pub backend_id: Option<String>,
    pub cluster_id: Option<String>,
    /// the value of the protocol Kawa should write in the Forwarded headers of the request
    pub protocol: Protocol,
    /// the value of the public address Kawa should write in the Forwarded headers of the request
    pub public_address: SocketAddr,
    /// the value of the session address Kawa should write in the Forwarded headers of the request
    pub session_address: Option<SocketAddr>,
    /// the name of the cookie Kawa should read from the request to get the sticky session
    pub sticky_name: String,
    /// the sticky session that should be used
    /// used to create a "Set-Cookie" header in the response in case it differs from sticky_session_found
    pub sticky_session: Option<String>,
    /// the address of the backend server
    pub backend_address: Option<SocketAddr>,
    /// The TLS Server Name Indication (SNI) hostname negotiated at handshake.
    ///
    /// Populated for HTTPS listeners when the client sent an SNI extension (see
    /// `https.rs::upgrade_handshake`). Used by the routing layer to enforce the
    /// TLS trust boundary against the HTTP `:authority` / `Host` header — without
    /// this check, an attacker holding a valid certificate for tenant A could
    /// open TLS with SNI=A then send requests with `:authority=tenantB` and
    /// reach tenant B's backend (CWE-346 / CWE-444).
    ///
    /// `None` when the listener is plaintext HTTP or the client omitted SNI.
    /// Stored pre-lowercased and without a port for direct exact-match comparison.
    pub tls_server_name: Option<String>,
    /// Whether the router must reject this request when `tls_server_name`
    /// does not exact-match its authority (CWE-346 / CWE-444). Mirrors
    /// `HttpsListenerConfig::strict_sni_binding`. Set from the mux
    /// `Context` at stream creation time (see `Context::create_stream`).
    /// Plaintext listeners still never hit the check because
    /// `tls_server_name` is `None`.
    pub strict_sni_binding: bool,
    /// When `true`, the request-side block walk in `on_request_headers`
    /// strips any client-supplied `X-Real-IP` header before forwarding
    /// (anti-spoofing). Mirrors `HttpListenerConfig::elide_x_real_ip` /
    /// `HttpsListenerConfig::elide_x_real_ip`. Set from the mux `Context`
    /// at stream creation (see `Context::create_stream`); listener-scoped
    /// and never reset across keep-alive requests. Independent of
    /// `send_x_real_ip`. The same flag is plumbed into
    /// `pkawa::handle_trailer` so trailer HEADERS frames cannot bypass
    /// the elision.
    pub elide_x_real_ip: bool,
    /// When `true`, `on_request_headers` injects a proxy-generated
    /// `X-Real-IP` header carrying `session_address.ip()` (post-PROXY-v2
    /// unwrap, i.e. the original client IP). Mirrors
    /// `HttpListenerConfig::send_x_real_ip` /
    /// `HttpsListenerConfig::send_x_real_ip`. Set from the mux `Context`
    /// at stream creation (see `Context::create_stream`); listener-scoped
    /// and never reset across keep-alive requests. Independent of
    /// `elide_x_real_ip`. When `session_address` is `None` (raw socket
    /// without a peer), no header is appended — identical to the
    /// existing X-Forwarded-For / Forwarded synthesis behaviour.
    pub send_x_real_ip: bool,
    /// Negotiated TLS protocol version as a short label (e.g. `"TLSv1.3"`).
    /// Captured from `rustls_version_label` at handshake completion and
    /// propagated from the mux `Context`. `None` for plaintext listeners.
    pub tls_version: Option<&'static str>,
    /// Negotiated TLS cipher suite as a short label (e.g.
    /// `"TLS_AES_128_GCM_SHA256"`). Captured from `rustls_ciphersuite_label`
    /// at handshake completion and propagated from the mux `Context`. `None`
    /// for plaintext listeners.
    pub tls_cipher: Option<&'static str>,
    /// Negotiated ALPN protocol (e.g. `"h2"`, `"http/1.1"`). Captured from
    /// rustls at handshake completion and propagated from the mux `Context`.
    /// `None` for plaintext listeners or when no ALPN was negotiated.
    pub tls_alpn: Option<&'static str>,
    /// Name of the correlation header Sozu injects into every request and
    /// response. Defaults to `"Sozu-Id"` via [`L7ListenerHandler::get_sozu_id_header`].
    /// Populated at stream creation from the listener config's `sozu_id_header`
    /// knob. Stored as an owned `String` so it survives a listener hot-reload
    /// that changes the value.
    pub sozu_id_header: String,
    /// Resolved `Location` URL stashed by the routing layer when a frontend
    /// triggers a permanent redirect (`RedirectPolicy::PERMANENT` or the
    /// legacy `cluster.https_redirect`). Read by the default-answer 301
    /// path so the response carries the correct target URL — including
    /// optional `cluster.https_redirect_port` and rewrite-template captures.
    /// `None` when the request is not redirecting.
    pub redirect_location: Option<String>,
    /// `WWW-Authenticate` realm stashed by the routing layer when a
    /// frontend rejects an unauthenticated request (`required_auth = true`
    /// without a valid `Authorization: Basic` header, or
    /// `RedirectPolicy::UNAUTHORIZED`). Read by the default-answer 401
    /// path so the response carries the cluster's configured
    /// `www_authenticate` value. `None` falls back to template default
    /// (header is elided when no realm is configured).
    pub www_authenticate: Option<String>,
    /// Original authority captured before a rewrite-host fired; emitted
    /// back to the backend as `X-Forwarded-Host` so the backend can
    /// reconstruct the public URL even though `:authority` / `Host` was
    /// rewritten on the wire. `None` when no host rewrite happened.
    pub original_authority: Option<String>,
    /// Per-frontend response-side header edits (set/replace/delete)
    /// stashed by the routing layer for the emission boundary in
    /// `mux/h1.rs::writable` and `mux/h2.rs::write_streams` to apply
    /// before `kawa.prepare(...)`. Empty when the frontend has no
    /// response-side header policy. An entry with an empty `val`
    /// deletes the header by name (HAProxy `del-header` parity); a
    /// non-empty `val` set/replaces.
    pub headers_response: Vec<HeaderEditSnapshot>,
    /// Resolved `Retry-After` value (seconds) for an HTTP 429 default
    /// answer. Computed in `Router::connect` when the per-(cluster,
    /// source-IP) connection limit is hit, by folding the cluster's
    /// `retry_after` override over the global default. `None` (or
    /// `Some(0)`) tells the answer engine to omit the `Retry-After`
    /// header entirely — `Retry-After: 0` invites an immediate retry
    /// that defeats the limit. Unused for any other status code.
    pub retry_after_seconds: Option<u32>,
    /// Frontend-supplied template body that overrides the listener /
    /// cluster default `http.301.redirection` for a single
    /// `RedirectPolicy::PERMANENT` request. Stashed by the routing layer
    /// from `RouteResult::redirect_template` and consumed by the 301
    /// branch of `mux::answers::set_default_answer_with_retry_after`,
    /// which compiles the body via `HttpAnswers::render_inline_301` and
    /// renders it with the same `(REDIRECT_LOCATION, ROUTE,
    /// REQUEST_ID)` variable schema as the persistent template chain.
    /// `None` falls back to the cluster / listener default. Unused for
    /// any other status code or for the legacy
    /// `cluster.https_redirect = true` path (which never sets it).
    pub frontend_redirect_template: Option<String>,
    /// Resolved redirect status code stashed by the routing layer when
    /// a frontend's `RedirectPolicy` is one of the redirect variants.
    /// 301 = `Permanent`, 302 = `Found`, 308 = `PermanentRedirect`.
    /// `None` falls back to 301 for the legacy
    /// `cluster.https_redirect = true` path. Closes #1009.
    pub redirect_status: Option<u16>,
    /// Stable, structured discriminator surfaced as the access-log
    /// `message` field when the session terminates on a timeout. Set by
    /// the timeout handlers in `kawa_h1::timeout` and `MuxState::timeout`
    /// **before** the default-answer or `forcefully_terminate_answer`
    /// path consumes it. The vocabulary is operator-visible API once
    /// shipped — see the access-log section of `doc/configure.md` for
    /// the full token list (`client_timeout`,
    /// `client_timeout_during_response`, `backend_timeout`,
    /// `backend_response_timeout`). `None` for non-timeout sessions, in
    /// which case the access log emits `message: None` as before.
    pub access_log_message: Option<&'static str>,
}

/// How `apply_response_header_edits` should interpret a per-edit value.
///
/// The implicit empty-`val` Append → delete encoding is still supported
/// (so legacy operator-supplied `[[...frontends.headers]]` entries work
/// unchanged); the explicit modes give typed policies finer control:
///
/// - `Append`: drop the legacy delete shortcut for empty `val`, and
///   append every other entry before the end-of-headers flag.
/// - `SetIfAbsent`: skip the insert when `kawa.blocks` already carries
///   a non-elided header with the same name (case-insensitive). HSTS
///   uses this by default to preserve a backend-supplied
///   `Strict-Transport-Security` (RFC 6797 §6.1 single-header
///   requirement).
/// - `Set`: delete every existing header with the matching name, then
///   insert the new entry. Use when the operator wants their typed
///   policy to override any backend-supplied value (the
///   `force_replace_backend = true` HSTS shape, for example).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HeaderEditMode {
    /// Append the header before the end-of-headers flag. Empty `val`
    /// is interpreted as a delete (legacy behaviour preserved).
    #[default]
    Append,
    /// Skip the insert if `kawa.blocks` already contains a non-elided
    /// header whose name matches `key` case-insensitively. Otherwise
    /// behave like `Append`.
    SetIfAbsent,
    /// Delete every existing header with the matching name, then
    /// insert the new value. Equivalent to two operator-defined edits
    /// (delete + append) but safer to express as one typed entry.
    Set,
}

/// Owned snapshot of a per-frontend header edit, captured at routing
/// time so the emission boundary can apply set/replace/delete without
/// touching the routing layer's `Rc<[HeaderEdit]>` slices.
///
/// `mode` chooses between explicit Append/Delete/SetIfAbsent semantics.
/// For backwards compatibility `mode = Append` paired with an empty
/// `val` is still treated as a delete by `apply_response_header_edits`
/// (HAProxy `del-header` parity), so callers that have not yet migrated
/// to the explicit `HeaderEditMode::Delete` keep working unchanged.
#[derive(Debug, Clone)]
pub struct HeaderEditSnapshot {
    pub key: Vec<u8>,
    pub val: Vec<u8>,
    pub mode: HeaderEditMode,
}

impl kawa::h1::ParserCallbacks<Checkout> for HttpContext {
    fn on_headers(&mut self, stream: &mut GenericHttpStream) {
        match stream.kind {
            kawa::Kind::Request => self.on_request_headers(stream),
            kawa::Kind::Response => self.on_response_headers(stream),
        }
    }
}

impl HttpContext {
    /// Creates a new instance
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: Ulid,
        request_id: Ulid,
        protocol: Protocol,
        public_address: SocketAddr,
        session_address: Option<SocketAddr>,
        sticky_name: String,
        sozu_id_header: String,
        elide_x_real_ip: bool,
        send_x_real_ip: bool,
    ) -> Self {
        Self {
            session_id,
            id: request_id,
            backend_id: None,
            cluster_id: None,

            closing: false,
            keep_alive_backend: true,
            keep_alive_frontend: true,
            protocol,
            public_address,
            session_address,
            sticky_name,
            sticky_session: None,
            sticky_session_found: None,

            method: None,
            authority: None,
            path: None,
            status: None,
            reason: None,
            user_agent: None,
            x_request_id: None,
            xff_chain: None,

            #[cfg(feature = "opentelemetry")]
            otel: Default::default(),

            backend_address: None,
            tls_server_name: None,
            strict_sni_binding: true,
            elide_x_real_ip,
            send_x_real_ip,
            tls_version: None,
            tls_cipher: None,
            tls_alpn: None,
            sozu_id_header,
            redirect_location: None,
            www_authenticate: None,
            original_authority: None,
            headers_response: Vec::new(),
            retry_after_seconds: None,
            frontend_redirect_template: None,
            redirect_status: None,
            access_log_message: None,
        }
    }

    /// Callback for request:
    ///
    /// - edit headers (connection, forwarded, sticky cookie, sozu-id,
    ///   x-request-id, x-real-ip)
    /// - save information:
    ///   - method
    ///   - authority
    ///   - path
    ///   - front keep-alive
    ///   - sticky cookie
    ///   - user-agent
    ///   - x-request-id (preserved if present, else derived from `self.id`)
    fn on_request_headers(&mut self, request: &mut GenericHttpStream) {
        let buf = request.storage.mut_buffer();

        // Captures the request line
        if let kawa::StatusLine::Request {
            method,
            authority,
            path,
            ..
        } = &request.detached.status_line
        {
            self.method = method.data_opt(buf).map(Method::new);
            self.authority = authority
                .data_opt(buf)
                .and_then(|data| from_utf8(data).ok())
                .map(ToOwned::to_owned);
            self.path = path
                .data_opt(buf)
                .and_then(|data| from_utf8(data).ok())
                .map(ToOwned::to_owned);
        }

        // if self.method == Some(Method::Get) && request.body_size == kawa::BodySize::Empty {
        //     request.parsing_phase = kawa::ParsingPhase::Terminated;
        // }

        let public_ip = self.public_address.ip();
        let public_port = self.public_address.port();
        let proto = match self.protocol {
            Protocol::HTTP => "http",
            Protocol::HTTPS => "https",
            _ => unreachable!(),
        };

        // Find and remove the sticky_name cookie
        // if found its value is stored in sticky_session_found
        for cookie in &mut request.detached.jar {
            let key = cookie.key.data(buf);
            if key == self.sticky_name.as_bytes() {
                let val = cookie.val.data(buf);
                self.sticky_session_found = from_utf8(val).ok().map(ToOwned::to_owned);
                cookie.elide();
            }
        }

        // If found:
        // - set Connection to "close" if closing is set
        // - set keep_alive_frontend to false if Connection is "close"
        // - update value of X-Forwarded-Proto
        // - update value of X-Forwarded-Port
        // - store X-Forwarded-For
        // - store Forwarded
        // - store User-Agent
        let mut x_for = None;
        let mut forwarded = None;
        let mut has_x_port = false;
        let mut has_x_proto = false;
        let mut has_x_request_id = false;
        let mut has_connection = false;
        #[cfg(feature = "opentelemetry")]
        let mut traceparent: Option<&mut kawa::Pair> = None;
        #[cfg(feature = "opentelemetry")]
        let mut tracestate: Option<&mut kawa::Pair> = None;
        for block in &mut request.blocks {
            match block {
                kawa::Block::Header(header) if !header.is_elided() => {
                    let key = header.key.data(buf);
                    if compare_no_case(key, b"connection") {
                        has_connection = true;
                        if self.closing {
                            header.val = kawa::Store::Static(b"close");
                        } else {
                            let val = header.val.data(buf);
                            self.keep_alive_frontend &= !compare_no_case(val, b"close");
                        }
                    } else if compare_no_case(key, b"X-Forwarded-Proto") {
                        has_x_proto = true;
                        // header.val = kawa::Store::Static(proto.as_bytes());
                        incr!("http.trusting.x_proto");
                        let val = header.val.data(buf);
                        if !compare_no_case(val, proto.as_bytes()) {
                            incr!("http.trusting.x_proto.diff");
                            debug!(
                                "{} Trusting X-Forwarded-Proto for {:?} even though {:?} != {}",
                                self.log_context(),
                                self.authority,
                                val,
                                proto
                            );
                        }
                    } else if compare_no_case(key, b"X-Forwarded-Port") {
                        has_x_port = true;
                        // header.val = kawa::Store::from_string(public_port.to_string());
                        incr!("http.trusting.x_port");
                        let val = header.val.data(buf);
                        let mut port_buf = itoa::Buffer::new();
                        let expected = port_buf.format(public_port);
                        if !compare_no_case(val, expected.as_bytes()) {
                            incr!("http.trusting.x_port.diff");
                            debug!(
                                "{} Trusting X-Forwarded-Port for {:?} even though {:?} != {}",
                                self.log_context(),
                                self.authority,
                                val,
                                expected
                            );
                        }
                    } else if compare_no_case(key, b"X-Forwarded-For") {
                        // Snapshot the upstream-attested chain before we
                        // potentially append our own peer below — the access
                        // log records the value the client/upstream LB
                        // forwarded, not the rewritten value Sōzu emits.
                        self.xff_chain = header
                            .val
                            .data_opt(buf)
                            .and_then(|data| from_utf8(data).ok())
                            .map(ToOwned::to_owned);
                        x_for = Some(header);
                    } else if compare_no_case(key, b"X-Real-IP") && self.elide_x_real_ip {
                        // Anti-spoofing: a client cannot supply its own
                        // `X-Real-IP` and have it reach the backend. The
                        // proxy-injected value (when `send_x_real_ip` is
                        // also set) is appended after this loop. H2 trailer
                        // HEADERS frames bypass this callback; they are
                        // covered by the matching elision in
                        // `pkawa::handle_trailer`.
                        header.elide();
                    } else if compare_no_case(key, b"Forwarded") {
                        forwarded = Some(header);
                    } else if compare_no_case(key, b"User-Agent") {
                        self.user_agent = header
                            .val
                            .data_opt(buf)
                            .and_then(|data| from_utf8(data).ok())
                            .map(ToOwned::to_owned);
                    } else if compare_no_case(key, b"X-Request-Id") {
                        // RFC: not standardized, but the de-facto correlation
                        // header used by Envoy/HAProxy/most LBs. Preserve the
                        // client-supplied value verbatim — overwriting it
                        // breaks end-to-end request tracing.
                        has_x_request_id = true;
                        self.x_request_id = header
                            .val
                            .data_opt(buf)
                            .and_then(|data| from_utf8(data).ok())
                            .map(ToOwned::to_owned);
                    } else {
                        #[cfg(feature = "opentelemetry")]
                        if compare_no_case(key, b"traceparent") {
                            if let Some(hdr) = traceparent {
                                hdr.elide();
                            }
                            traceparent = Some(header);
                        } else if compare_no_case(key, b"tracestate") {
                            if let Some(hdr) = tracestate {
                                hdr.elide();
                            }
                            tracestate = Some(header);
                        }
                    }
                }
                _ => {}
            }
        }

        #[cfg(feature = "opentelemetry")]
        let (otel, has_traceparent) = {
            let mut otel = sozu_command_lib::logging::OpenTelemetry::default();
            let tp = traceparent
                .as_ref()
                .and_then(|hdr| parse_traceparent(&hdr.val, buf))
                .map(|(trace_id, parent_id)| (trace_id, Some(parent_id)));
            // Remove tracestate if no traceparent is present
            if let (None, Some(tracestate)) = (tp, tracestate) {
                tracestate.elide();
            }
            let (trace_id, parent_id) = tp.unwrap_or_else(|| (random_id(), None));
            otel.trace_id = trace_id;
            otel.parent_span_id = parent_id;
            otel.span_id = random_id();
            // Modify header if present
            if let Some(id) = &mut traceparent {
                let new_val = build_traceparent(&otel.trace_id, &otel.span_id);
                id.val.modify(buf, &new_val);
            }
            (otel, traceparent.is_some())
        };

        // If session_address is set:
        // - append its ip address to the list of "X-Forwarded-For" if it was found, creates it if not
        // - append "proto=[PROTO];for=[PEER];by=[PUBLIC]" to the list of "Forwarded" if it was found, creates it if not
        if let Some(peer_addr) = self.session_address {
            let peer_ip = peer_addr.ip();
            let peer_port = peer_addr.port();
            let has_x_for = x_for.is_some();
            let has_forwarded = forwarded.is_some();

            // Buffer for building header values — ownership is transferred to Store
            // via `take`, so each header gets its own allocation.
            let mut hdr_buf = Vec::with_capacity(128);

            if let Some(header) = x_for {
                hdr_buf.extend_from_slice(header.val.data(buf));
                let _ = write!(hdr_buf, ", {peer_ip}");
                header.val = kawa::Store::from_vec(std::mem::take(&mut hdr_buf));
            }
            if let Some(header) = &mut forwarded {
                hdr_buf.extend_from_slice(header.val.data(buf));
                write_forwarded_suffix(&mut hdr_buf, proto, peer_ip, peer_port, public_ip);
                header.val = kawa::Store::from_vec(std::mem::take(&mut hdr_buf));
            }

            if !has_x_for {
                let _ = write!(hdr_buf, "{peer_ip}");
                request.push_block(kawa::Block::Header(kawa::Pair {
                    key: kawa::Store::Static(b"X-Forwarded-For"),
                    val: kawa::Store::from_vec(std::mem::take(&mut hdr_buf)),
                }));
            }
            if !has_forwarded {
                hdr_buf.extend_from_slice(b"proto=");
                hdr_buf.extend_from_slice(proto.as_bytes());
                write_forwarded_for_by(&mut hdr_buf, peer_ip, peer_port, public_ip);
                request.push_block(kawa::Block::Header(kawa::Pair {
                    key: kawa::Store::Static(b"Forwarded"),
                    val: kawa::Store::from_vec(std::mem::take(&mut hdr_buf)),
                }));
            }

            // Inject a proxy-generated `X-Real-IP` header carrying the
            // peer IP (post-PROXY-v2 unwrap, so the original client IP
            // even when the upstream presented PROXY-v2). Folded into the
            // `if let Some(peer_addr)` arm so missing peers (raw socket,
            // no PROXY-v2) skip the injection silently — identical to the
            // X-Forwarded-For / Forwarded synthesis behaviour above. Any
            // client-supplied `X-Real-IP` was either elided in the block
            // walk (if `elide_x_real_ip` is on) or passes through; this
            // header is appended last so order in the resulting block
            // list is deterministic for tests.
            if self.send_x_real_ip {
                let _ = write!(hdr_buf, "{peer_ip}");
                request.push_block(kawa::Block::Header(kawa::Pair {
                    key: kawa::Store::Static(b"X-Real-IP"),
                    val: kawa::Store::from_vec(std::mem::take(&mut hdr_buf)),
                }));
            }
        }

        #[cfg(feature = "opentelemetry")]
        {
            if !has_traceparent {
                let val = build_traceparent(&otel.trace_id, &otel.span_id);
                request.push_block(kawa::Block::Header(kawa::Pair {
                    key: kawa::Store::Static(b"traceparent"),
                    val: kawa::Store::from_slice(&val),
                }));
            }
            self.otel = Some(otel);
        }

        if !has_x_port {
            let mut port_buf = itoa::Buffer::new();
            let port_str = port_buf.format(public_port);
            request.push_block(kawa::Block::Header(kawa::Pair {
                key: kawa::Store::Static(b"X-Forwarded-Port"),
                val: kawa::Store::from_slice(port_str.as_bytes()),
            }));
        }
        if !has_x_proto {
            request.push_block(kawa::Block::Header(kawa::Pair {
                key: kawa::Store::Static(b"X-Forwarded-Proto"),
                val: kawa::Store::Static(proto.as_bytes()),
            }));
        }
        // Create a "Connection" header in case it was not found and closing it set
        if !has_connection && self.closing {
            request.push_block(kawa::Block::Header(kawa::Pair {
                key: kawa::Store::Static(b"Connection"),
                val: kawa::Store::Static(b"close"),
            }));
        }
        // Inject "X-Request-Id" derived from the request ULID when the client
        // (or upstream LB) did not already supply one. When already present,
        // the header is left untouched in the block list — preserving the
        // client-supplied value end-to-end is the whole point of this header.
        // Either way, `self.x_request_id` is populated so the access log
        // records the exact value forwarded to the backend.
        if has_x_request_id {
            incr!("http.x_request_id.propagated");
        } else {
            let value = self.id.to_string();
            request.push_block(kawa::Block::Header(kawa::Pair {
                key: kawa::Store::Static(b"X-Request-Id"),
                val: kawa::Store::from_string(value.clone()),
            }));
            self.x_request_id = Some(value);
            incr!("http.x_request_id.generated");
        }

        // Create a custom correlation header (defaults to "Sozu-Id", can be
        // renamed via the `sozu_id_header` listener config knob).
        request.push_block(kawa::Block::Header(kawa::Pair {
            key: kawa::Store::from_string(self.sozu_id_header.clone()),
            val: kawa::Store::from_string(self.id.to_string()),
        }));
    }

    /// Callback for response:
    ///
    /// - edit headers (connection, set-cookie, sozu-id)
    /// - save information:
    ///   - status code
    ///   - reason
    ///   - back keep-alive
    fn on_response_headers(&mut self, response: &mut GenericHttpStream) {
        let buf = &mut response.storage.mut_buffer();

        // Captures the response line
        if let kawa::StatusLine::Response { code, reason, .. } = &response.detached.status_line {
            self.status = Some(*code);
            self.reason = reason
                .data_opt(buf)
                .and_then(|data| from_utf8(data).ok())
                .map(ToOwned::to_owned);
        }

        if self.method == Some(Method::Head) {
            response.parsing_phase = kawa::ParsingPhase::Terminated;
        }

        // If found:
        // - set Connection to "close" if closing is set
        // - set keep_alive_backend to false if Connection is "close"
        for block in &mut response.blocks {
            match block {
                kawa::Block::Header(header) if !header.is_elided() => {
                    let key = header.key.data(buf);
                    if compare_no_case(key, b"connection") {
                        if self.closing {
                            header.val = kawa::Store::Static(b"close");
                        } else {
                            let val = header.val.data(buf);
                            self.keep_alive_backend &= !compare_no_case(val, b"close");
                        }
                    }
                }
                _ => {}
            }
        }

        // If the sticky_session is set and differs from the one found in the request
        // create a "Set-Cookie" header to update the sticky_name value
        if let Some(sticky_session) = &self.sticky_session {
            if self.sticky_session != self.sticky_session_found {
                let mut cookie_buf =
                    Vec::with_capacity(self.sticky_name.len() + 1 + sticky_session.len() + 8);
                cookie_buf.extend_from_slice(self.sticky_name.as_bytes());
                cookie_buf.push(b'=');
                cookie_buf.extend_from_slice(sticky_session.as_bytes());
                cookie_buf.extend_from_slice(b"; Path=/");
                response.push_block(kawa::Block::Header(kawa::Pair {
                    key: kawa::Store::Static(b"Set-Cookie"),
                    val: kawa::Store::from_vec(cookie_buf),
                }));
            }
        }

        // Create a custom correlation header (defaults to "Sozu-Id", can be
        // renamed via the `sozu_id_header` listener config knob).
        response.push_block(kawa::Block::Header(kawa::Pair {
            key: kawa::Store::from_string(self.sozu_id_header.clone()),
            val: kawa::Store::from_string(self.id.to_string()),
        }));
    }

    pub fn reset(&mut self) {
        self.keep_alive_backend = true;
        self.keep_alive_frontend = true;
        self.sticky_session_found = None;
        self.method = None;
        self.authority = None;
        self.path = None;
        self.status = None;
        self.reason = None;
        self.user_agent = None;
        self.x_request_id = None;
        self.xff_chain = None;
        self.redirect_location = None;
        self.www_authenticate = None;
        self.original_authority = None;
        self.headers_response.clear();
        // Note: tls_server_name, tls_version, tls_cipher, tls_alpn,
        // strict_sni_binding, elide_x_real_ip, send_x_real_ip are
        // connection-scoped — set once at handshake completion and reused
        // across every keep-alive request, so reset() intentionally leaves
        // them in place.
    }

    pub fn extract_route(&self) -> Result<(&str, &str, &Method), RetrieveClusterError> {
        let given_method = self.method.as_ref().ok_or(RetrieveClusterError::NoMethod)?;
        let given_authority = self
            .authority
            .as_deref()
            .ok_or(RetrieveClusterError::NoHost)?;
        let given_path = self.path.as_deref().ok_or(RetrieveClusterError::NoPath)?;

        Ok((given_authority, given_path, given_method))
    }

    pub fn get_route(&self) -> String {
        if let Some(method) = &self.method {
            if let Some(authority) = &self.authority {
                if let Some(path) = &self.path {
                    return format!("{method} {authority}{path}");
                }
                return format!("{method} {authority}");
            }
            return format!("{method}");
        }
        String::new()
    }

    pub fn websocket_context(&self) -> WebSocketContext {
        WebSocketContext::Http {
            method: self.method.clone(),
            authority: self.authority.clone(),
            path: self.path.clone(),
            reason: self.reason.clone(),
            status: self.status,
        }
    }

    pub fn log_context(&self) -> LogContext<'_> {
        LogContext {
            session_id: self.session_id,
            request_id: Some(self.id),
            cluster_id: self.cluster_id.as_deref(),
            backend_id: self.backend_id.as_deref(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    /// Helper to create a minimal HttpContext for testing.
    fn make_context() -> HttpContext {
        HttpContext::new(
            Ulid::generate(),
            Ulid::generate(),
            Protocol::HTTP,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080),
            Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
                54321,
            )),
            "SERVERID".to_owned(),
            "Sozu-Id".to_owned(),
            false,
            false,
        )
    }

    // ── sozu_id_header ──────────────────────────────────────────────────

    #[test]
    fn test_sozu_id_header_default_name_stored_on_context() {
        // The make_context helper uses the documented default "Sozu-Id" to
        // match the trait default on `L7ListenerHandler::get_sozu_id_header`.
        let ctx = make_context();
        assert_eq!(ctx.sozu_id_header, "Sozu-Id");
    }

    #[test]
    fn test_sozu_id_header_custom_name_stored_on_context() {
        // Operator-provided rename is carried verbatim onto the HttpContext.
        let ctx = HttpContext::new(
            Ulid::generate(),
            Ulid::generate(),
            Protocol::HTTP,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080),
            None,
            "SERVERID".to_owned(),
            "X-Edge-Id".to_owned(),
            false,
            false,
        );
        assert_eq!(ctx.sozu_id_header, "X-Edge-Id");
    }

    // ── extract_route ──────────────────────────────────────────────────

    #[test]
    fn test_extract_route_all_present() {
        let mut ctx = make_context();
        ctx.method = Some(Method::Get);
        ctx.authority = Some("example.com".to_owned());
        ctx.path = Some("/index.html".to_owned());

        let (authority, path, method) = ctx.extract_route().unwrap();
        assert_eq!(authority, "example.com");
        assert_eq!(path, "/index.html");
        assert_eq!(method, &Method::Get);
    }

    #[test]
    fn test_extract_route_no_method() {
        let mut ctx = make_context();
        ctx.authority = Some("example.com".to_owned());
        ctx.path = Some("/".to_owned());

        let err = ctx.extract_route().unwrap_err();
        assert!(matches!(err, RetrieveClusterError::NoMethod));
    }

    #[test]
    fn test_extract_route_no_host() {
        let mut ctx = make_context();
        ctx.method = Some(Method::Get);
        ctx.path = Some("/".to_owned());

        let err = ctx.extract_route().unwrap_err();
        assert!(matches!(err, RetrieveClusterError::NoHost));
    }

    #[test]
    fn test_extract_route_no_path() {
        let mut ctx = make_context();
        ctx.method = Some(Method::Get);
        ctx.authority = Some("example.com".to_owned());

        let err = ctx.extract_route().unwrap_err();
        assert!(matches!(err, RetrieveClusterError::NoPath));
    }

    // ── get_route ──────────────────────────────────────────────────────

    #[test]
    fn test_get_route_all_present() {
        let mut ctx = make_context();
        ctx.method = Some(Method::Get);
        ctx.authority = Some("example.com".to_owned());
        ctx.path = Some("/api/v1".to_owned());

        assert_eq!(ctx.get_route(), "GET example.com/api/v1");
    }

    #[test]
    fn test_get_route_method_and_authority_only() {
        let mut ctx = make_context();
        ctx.method = Some(Method::Post);
        ctx.authority = Some("example.com".to_owned());

        assert_eq!(ctx.get_route(), "POST example.com");
    }

    #[test]
    fn test_get_route_method_only() {
        let mut ctx = make_context();
        ctx.method = Some(Method::Delete);

        assert_eq!(ctx.get_route(), "DELETE");
    }

    #[test]
    fn test_get_route_empty() {
        let ctx = make_context();
        assert_eq!(ctx.get_route(), "");
    }

    // ── reset ──────────────────────────────────────────────────────────

    #[test]
    fn test_reset_clears_request_response_state() {
        let mut ctx = make_context();
        ctx.keep_alive_backend = false;
        ctx.keep_alive_frontend = false;
        ctx.sticky_session_found = Some("abc123".to_owned());
        ctx.method = Some(Method::Post);
        ctx.authority = Some("example.com".to_owned());
        ctx.path = Some("/upload".to_owned());
        ctx.status = Some(200);
        ctx.reason = Some("OK".to_owned());
        ctx.user_agent = Some("curl/7.81".to_owned());
        ctx.x_request_id = Some("client-xrid-123".to_owned());
        ctx.xff_chain = Some("203.0.113.5, 198.51.100.10".to_owned());
        ctx.redirect_location = Some("https://example.com/".to_owned());
        ctx.www_authenticate = Some("Basic realm=\"sozu\"".to_owned());
        ctx.original_authority = Some("old.example.com".to_owned());
        ctx.headers_response.push(HeaderEditSnapshot {
            key: b"X-Cache".to_vec(),
            val: b"HIT".to_vec(),
            mode: HeaderEditMode::Append,
        });

        ctx.reset();

        assert!(ctx.keep_alive_backend);
        assert!(ctx.keep_alive_frontend);
        assert!(ctx.sticky_session_found.is_none());
        assert!(ctx.method.is_none());
        assert!(ctx.authority.is_none());
        assert!(ctx.path.is_none());
        assert!(ctx.status.is_none());
        assert!(ctx.reason.is_none());
        assert!(ctx.user_agent.is_none());
        assert!(ctx.x_request_id.is_none());
        assert!(ctx.xff_chain.is_none());
        // The four stash slots written by the routing layer must clear
        // between pipelined H1 requests; otherwise a future code path that
        // emits a 301 / 401 default-answer without re-routing would
        // inherit a stale Location / WWW-Authenticate from a prior request,
        // or the backend would receive a stale X-Forwarded-Host or a stale
        // response-side header edit.
        assert!(ctx.redirect_location.is_none());
        assert!(ctx.www_authenticate.is_none());
        assert!(ctx.original_authority.is_none());
        assert!(ctx.headers_response.is_empty());
    }

    #[test]
    fn test_reset_preserves_tls_metadata() {
        // TLS metadata is connection-scoped (set once at handshake, reused
        // across every keep-alive request) — reset() must leave it intact
        // so the access log of the second request still carries it.
        let mut ctx = make_context();
        ctx.tls_server_name = Some("example.com".to_owned());
        ctx.tls_version = Some("TLSv1.3");
        ctx.tls_cipher = Some("TLS_AES_128_GCM_SHA256");
        ctx.tls_alpn = Some("h2");
        ctx.strict_sni_binding = false;

        ctx.reset();

        assert_eq!(ctx.tls_server_name.as_deref(), Some("example.com"));
        assert_eq!(ctx.tls_version, Some("TLSv1.3"));
        assert_eq!(ctx.tls_cipher, Some("TLS_AES_128_GCM_SHA256"));
        assert_eq!(ctx.tls_alpn, Some("h2"));
        assert!(!ctx.strict_sni_binding);
    }

    #[test]
    fn test_reset_preserves_connection_state() {
        let mut ctx = make_context();
        ctx.closing = true;
        ctx.cluster_id = Some("cluster-1".to_owned());
        ctx.backend_id = Some("backend-1".to_owned());
        ctx.sticky_session = Some("session-abc".to_owned());

        let original_id = ctx.id;
        let original_protocol = ctx.protocol;
        let original_public_address = ctx.public_address;

        ctx.reset();

        // Connection-level state is preserved
        assert!(ctx.closing);
        assert_eq!(ctx.cluster_id.as_deref(), Some("cluster-1"));
        assert_eq!(ctx.backend_id.as_deref(), Some("backend-1"));
        assert_eq!(ctx.sticky_session.as_deref(), Some("session-abc"));
        assert_eq!(ctx.id, original_id);
        assert_eq!(ctx.protocol, original_protocol);
        assert_eq!(ctx.public_address, original_public_address);
    }

    // ── traceparent / opentelemetry helpers ─────────────────────────────

    #[cfg(feature = "opentelemetry")]
    mod otel {
        use super::super::*;

        #[test]
        fn test_parse_hex_valid() {
            let (val, rest) = parse_hex::<4>(b"abcd1234").unwrap();
            assert_eq!(&val, b"abcd");
            assert_eq!(rest, b"1234");
        }

        #[test]
        fn test_parse_hex_exact_length() {
            let (val, rest) = parse_hex::<8>(b"01234567").unwrap();
            assert_eq!(&val, b"01234567");
            assert!(rest.is_empty());
        }

        #[test]
        fn test_parse_hex_too_short() {
            assert!(parse_hex::<4>(b"ab").is_none());
        }

        #[test]
        fn test_parse_hex_rejects_non_hex() {
            assert!(parse_hex::<4>(b"ghij").is_none());
        }

        #[test]
        fn test_parse_hex_rejects_uppercase_is_ok() {
            // Uppercase hex digits are valid
            let (val, _) = parse_hex::<4>(b"ABCD").unwrap();
            assert_eq!(&val, b"ABCD");
        }

        #[test]
        fn test_skip_separator_valid() {
            let rest = skip_separator(b"-hello").unwrap();
            assert_eq!(rest, b"hello");
        }

        #[test]
        fn test_skip_separator_wrong_char() {
            assert!(skip_separator(b"+hello").is_none());
        }

        #[test]
        fn test_skip_separator_empty() {
            assert!(skip_separator(b"").is_none());
        }

        #[test]
        fn test_build_traceparent_format() {
            let trace_id: [u8; 32] = *b"4bf92f3577b34da6a3ce929d0e0e4736";
            let parent_id: [u8; 16] = *b"00f067aa0ba902b7";

            let result = build_traceparent(&trace_id, &parent_id);
            assert_eq!(
                &result,
                b"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
            );
        }

        #[test]
        fn test_build_traceparent_length() {
            let trace_id = [b'a'; 32];
            let parent_id = [b'b'; 16];
            let result = build_traceparent(&trace_id, &parent_id);
            // Format: "00-" (3) + trace_id (32) + "-" (1) + parent_id (16) + "-01" (3) = 55
            assert_eq!(result.len(), 55);
        }

        #[test]
        fn test_parse_traceparent_valid() {
            let input = b"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
            let store = kawa::Store::Static(input);
            let (trace_id, parent_id) = parse_traceparent(&store, input).unwrap();
            assert_eq!(&trace_id, b"4bf92f3577b34da6a3ce929d0e0e4736");
            assert_eq!(&parent_id, b"00f067aa0ba902b7");
        }

        #[test]
        fn test_parse_traceparent_sampled_flag_zero() {
            let input = b"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00";
            let store = kawa::Store::Static(input);
            let result = parse_traceparent(&store, input);
            assert!(result.is_some());
        }

        #[test]
        fn test_parse_traceparent_wrong_version() {
            let input = b"01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
            let store = kawa::Store::Static(input);
            assert!(parse_traceparent(&store, input).is_none());
        }

        #[test]
        fn test_parse_traceparent_too_short() {
            let input = b"00-4bf9";
            let store = kawa::Store::Static(input);
            assert!(parse_traceparent(&store, input).is_none());
        }

        #[test]
        fn test_parse_traceparent_trailing_data() {
            // Extra characters after the trace-flags should cause rejection
            let input = b"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra";
            let store = kawa::Store::Static(input);
            assert!(parse_traceparent(&store, input).is_none());
        }

        #[test]
        fn test_parse_traceparent_missing_separator() {
            let input = b"004bf92f3577b34da6a3ce929d0e0e473600f067aa0ba902b701";
            let store = kawa::Store::Static(input);
            assert!(parse_traceparent(&store, input).is_none());
        }

        #[test]
        fn test_parse_build_roundtrip() {
            let trace_id: [u8; 32] = *b"4bf92f3577b34da6a3ce929d0e0e4736";
            let parent_id: [u8; 16] = *b"00f067aa0ba902b7";

            // Build a traceparent from known IDs
            let built = build_traceparent(&trace_id, &parent_id);

            // Verify that the built value matches the expected static string,
            // then parse that static string back to confirm roundtrip.
            let expected: &[u8] = b"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
            assert_eq!(&built[..], expected);

            let store = kawa::Store::Static(expected);
            let (parsed_trace_id, parsed_parent_id) = parse_traceparent(&store, expected).unwrap();

            assert_eq!(parsed_trace_id, trace_id);
            assert_eq!(parsed_parent_id, parent_id);
        }
    }
}
