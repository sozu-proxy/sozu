//! H1 request/response header editor.
//!
//! Captures method/authority/path on parse, rewrites hop-by-hop and
//! forwarding headers (`X-Forwarded-*`, `Forwarded`, `Connection`,
//! WebSocket-upgrade signalling, optional `traceparent`), and surfaces the
//! canonical `LogContext` used by the access-log envelope. Acts as the
//! Kawa `ParserCallbacks` implementation for the H1 mux path.

use std::{
    borrow::Cow,
    io::Write as _,
    net::{IpAddr, SocketAddr},
    rc::Rc,
    str::from_utf8,
    sync::Arc,
};

use rusty_ulid::Ulid;
use sozu_command_lib::logging::{CachedTags, LogContext};

use crate::metrics::names;
use crate::{
    Protocol, RetrieveClusterError,
    pool::Checkout,
    protocol::{
        http::{
            GenericHttpStream,
            parser::{Method, compare_no_case},
        },
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
    // Pre: the ids are hex (they come from a parsed traceparent or our own
    // hex `random_id`). Hex digits are inherently CR/LF-free, so this also
    // upholds the anti-injection guarantee for the value we splice into the
    // `traceparent` header. Inline (not via `is_crlf_free`) so the release
    // build under `--features opentelemetry` still compiles (HARD RULE 2).
    debug_assert!(
        trace_id.iter().all(u8::is_ascii_hexdigit) && parent_id.iter().all(u8::is_ascii_hexdigit),
        "traceparent ids must be hex (CR/LF-free header value)"
    );
    let mut buf = [0; 55];
    buf[..3].copy_from_slice(b"00-");
    buf[3..35].copy_from_slice(trace_id);
    buf[35] = b'-';
    buf[36..52].copy_from_slice(parent_id);
    buf[52..55].copy_from_slice(b"-01");
    // Post: the value is exactly the `00-<trace>-<parent>-01` shape with the
    // dash separators at the fixed offsets the parser expects.
    debug_assert!(
        buf[2] == b'-' && buf[35] == b'-' && buf[52] == b'-',
        "traceparent must carry dash separators at fixed offsets"
    );
    buf
}

/// `true` when `bytes` contains no CR or LF — the anti-injection
/// invariant for any header value Sōzu serialises onto the wire. A value
/// carrying a raw CR/LF could split one header into two (request/response
/// smuggling, CWE-93/CWE-113).
///
/// Compiled in EVERY profile (HARD RULE 2): it is referenced inside plain
/// `debug_assert!` calls whose arguments still compile with
/// `debug_assertions` OFF, so a `#[cfg(debug_assertions)]` gate would break
/// the release build with E0425. In release it is only reached from
/// optimised-out `debug_assert!` bodies, so the optimiser drops it — hence
/// `#[allow(dead_code)]` to keep `-D warnings` clean.
#[allow(dead_code)]
fn is_crlf_free(bytes: &[u8]) -> bool {
    !bytes.iter().any(|b| *b == b'\r' || *b == b'\n')
}

/// Write the ";for=..;by=.." portion of a Forwarded header into `buf`
/// without heap-allocating a `String`.
///
/// RFC 7239 §6 requires IPv6 literals to be enclosed in square brackets
/// inside a quoted string (the `IP-literal` production is `"[" IPv6address "]"`).
/// Emitting a bare IPv6 like `for="2001:db8::1:8080"` is ambiguous: the
/// trailing `:1:8080` could parse as `::1` + port `8080` or as the literal
/// `:1:8080` with no port. Matches HAProxy's behaviour
/// (`_7239_print_ip6` in `src/http_ext.c`), which is the de-facto reference
/// implementation of RFC 7239 in the proxy world. See sozu issue #1254.
fn write_forwarded_for_by(buf: &mut Vec<u8>, peer_ip: IpAddr, peer_port: u16, public_ip: IpAddr) {
    let before = buf.len();
    buf.extend_from_slice(b";for=\"");
    write_ip_literal(buf, peer_ip);
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
            write_ip_literal(buf, public_ip);
            buf.push(b'"');
        }
    }
    // Post: the fragment grew the buffer and the whole appended span is
    // CR/LF-free — it is spliced verbatim into a `Forwarded` header value,
    // so a stray CR/LF would let an attacker-influenced peer address split
    // the header (CWE-93). `peer_ip`/`public_ip` are `IpAddr` and the port
    // is a `u16`, so no untrusted bytes reach here, but the guard pins the
    // anti-injection contract against future edits to this fragment.
    debug_assert!(
        buf.len() > before,
        "the Forwarded ;for=;by= fragment must emit bytes"
    );
    debug_assert!(
        is_crlf_free(&buf[before..]),
        "the Forwarded fragment must be CR/LF-free (anti-injection)"
    );
}

/// Write an `IP-literal` per RFC 3986 §3.2.2 / RFC 7239 §6: IPv4 verbatim,
/// IPv6 wrapped in `[...]`. Caller is responsible for any surrounding quotes
/// required by the embedding syntax (RFC 7239 mandates them for IPv6).
fn write_ip_literal(buf: &mut Vec<u8>, ip: IpAddr) {
    let before = buf.len();
    match ip {
        IpAddr::V4(_) => {
            let _ = write!(buf, "{ip}");
        }
        IpAddr::V6(_) => {
            buf.push(b'[');
            let _ = write!(buf, "{ip}");
            buf.push(b']');
        }
    }
    // Post: a non-empty literal was appended (every IpAddr renders to at
    // least one byte) and the emitted bytes carry no CR/LF — an IpAddr can
    // only render `[0-9a-fA-F:.]` plus the brackets we add, so this is a
    // belt-and-suspenders guard on the value we splice into a header.
    debug_assert!(buf.len() > before, "write_ip_literal must emit bytes");
    debug_assert!(
        is_crlf_free(&buf[before..]),
        "an IP literal spliced into a header must be CR/LF-free"
    );
    // IPv6 is bracketed on both ends; IPv4 is bare (RFC 3986 §3.2.2).
    debug_assert_eq!(
        matches!(ip, IpAddr::V6(_)),
        buf[before] == b'[' && buf[buf.len() - 1] == b']',
        "IPv6 literals are bracketed, IPv4 literals are not"
    );
}

/// Write ", proto=<proto>;for=..;by=.." (the suffix appended to an existing Forwarded value).
fn write_forwarded_suffix(
    buf: &mut Vec<u8>,
    proto: &str,
    peer_ip: IpAddr,
    peer_port: u16,
    public_ip: IpAddr,
) {
    // Pre: `proto` is the proxy-chosen scheme label ("http"/"https"), never
    // attacker-controlled — assert it carries no CR/LF before it is spliced
    // into the `Forwarded` value (anti-injection, CWE-93).
    debug_assert!(
        is_crlf_free(proto.as_bytes()),
        "the proto label spliced into Forwarded must be CR/LF-free"
    );
    let before = buf.len();
    buf.extend_from_slice(b", proto=");
    buf.extend_from_slice(proto.as_bytes());
    write_forwarded_for_by(buf, peer_ip, peer_port, public_ip);
    // Post: the suffix grew the buffer, begins with the `, proto=`
    // separator (so it appends cleanly to an existing value), and the whole
    // appended span is CR/LF-free.
    debug_assert!(
        buf.len() > before + b", proto=".len(),
        "the Forwarded suffix must emit the separator plus a value"
    );
    debug_assert!(
        buf[before..].starts_with(b", proto="),
        "the Forwarded suffix must start with the `, proto=` separator"
    );
    debug_assert!(
        is_crlf_free(&buf[before..]),
        "the Forwarded suffix must be CR/LF-free (anti-injection)"
    );
}

/// The separator a hop is appended to a client-supplied chain with. A
/// rendered hop starts with it, and a synthesised header shares the same
/// rendering from just past it (`kawa::Store::Shared`'s start offset).
const HOP_SEPARATOR: &[u8] = b", ";

/// The forwarding values of one connection, rendered once from the inputs
/// they depend on and shared by every request that reuses those inputs.
///
/// Neither value depends on the request: the peer and public addresses and
/// the protocol are connection-scoped, so the second and later requests of
/// a keep-alive connection forward them without a heap operation
/// (`kawa::Store::Shared` bumps a reference count). A client-supplied
/// `X-Forwarded-For` or `Forwarded` chain is request-scoped and is never
/// kept here: the rendered hop is appended to a copy of it.
/// `HttpContext::forwarding_hop` renders the values again whenever an input
/// differs from the ones they were rendered from.
#[derive(Debug)]
pub(crate) struct ForwardingHop {
    protocol: Protocol,
    public_address: SocketAddr,
    session_address: Option<SocketAddr>,
    /// The `X-Forwarded-Port` value: the public port.
    port: Rc<[u8]>,
    /// `, <peer ip>`: the hop appended to a client `X-Forwarded-For`; past
    /// `HOP_SEPARATOR`, the value of a synthesised `X-Forwarded-For` and
    /// of `X-Real-IP`. `None` without a peer address.
    x_forwarded_for: Option<Rc<[u8]>>,
    /// `, proto=<proto>;for="<peer>";by=<public>`: the element appended to a
    /// client `Forwarded`; past `HOP_SEPARATOR`, the value of a
    /// synthesised `Forwarded`. `None` without a peer address.
    forwarded: Option<Rc<[u8]>>,
}

impl ForwardingHop {
    /// Render the forwarding values of `protocol`, labelled `proto`, for
    /// the given public and peer addresses.
    fn render(
        protocol: Protocol,
        proto: &str,
        public_address: SocketAddr,
        session_address: Option<SocketAddr>,
    ) -> Self {
        let mut port_buf = itoa::Buffer::new();
        let port = Rc::from(port_buf.format(public_address.port()).as_bytes());
        let (x_forwarded_for, forwarded) = match session_address {
            Some(peer) => {
                // One scratch renders both values, each copied out once at
                // its exact size.
                let mut scratch = Vec::with_capacity(128);
                scratch.extend_from_slice(HOP_SEPARATOR);
                let _ = write!(scratch, "{}", peer.ip());
                let x_forwarded_for = Rc::<[u8]>::from(&scratch[..]);
                scratch.clear();
                write_forwarded_suffix(
                    &mut scratch,
                    proto,
                    peer.ip(),
                    peer.port(),
                    public_address.ip(),
                );
                (Some(x_forwarded_for), Some(Rc::<[u8]>::from(&scratch[..])))
            }
            None => (None, None),
        };
        let hop = Self {
            protocol,
            public_address,
            session_address,
            port,
            x_forwarded_for,
            forwarded,
        };
        // Post: both peer values exist exactly when there is a peer, each
        // starts with the separator and carries a value past it, and every
        // rendered byte is CR/LF-free — they are spliced verbatim into
        // header values (anti-injection, CWE-93).
        debug_assert!(
            hop.x_forwarded_for.is_some() == session_address.is_some()
                && hop.forwarded.is_some() == session_address.is_some(),
            "the peer values are rendered exactly when there is a peer"
        );
        debug_assert!(
            [&hop.x_forwarded_for, &hop.forwarded]
                .into_iter()
                .flatten()
                .all(|value| value.len() > HOP_SEPARATOR.len()
                    && value.starts_with(HOP_SEPARATOR)
                    && is_crlf_free(value)),
            "a rendered hop is the separator followed by a CR/LF-free value"
        );
        debug_assert!(
            !hop.port.is_empty() && hop.port.iter().all(u8::is_ascii_digit),
            "the X-Forwarded-Port value is the rendered public port"
        );
        hop
    }

    /// Whether these values were rendered from exactly these inputs.
    fn renders(
        &self,
        protocol: Protocol,
        public_address: SocketAddr,
        session_address: Option<SocketAddr>,
    ) -> bool {
        self.protocol == protocol
            && self.public_address == public_address
            && self.session_address == session_address
    }
}

/// A synthesised header value: `hop` past its `HOP_SEPARATOR`, shared.
fn synthesised_from_hop(hop: &Rc<[u8]>) -> kawa::Store {
    debug_assert!(
        hop.starts_with(HOP_SEPARATOR),
        "a rendered hop starts with the separator"
    );
    kawa::Store::Shared(hop.clone(), HOP_SEPARATOR.len() as u32)
}

/// A client-supplied chain `client` extended with `hop`, in one allocation
/// of the exact size of the result.
fn extended_chain(client: &[u8], hop: &[u8]) -> kawa::Store {
    let mut value = Vec::with_capacity(client.len() + hop.len());
    value.extend_from_slice(client);
    value.extend_from_slice(hop);
    // Post: the client chain is kept as a prefix, our CR/LF-free hop is
    // appended after it (anti-injection), and the buffer is full, so
    // `kawa::Store::from_vec` keeps it without reallocating.
    debug_assert!(
        value.starts_with(client) && value.len() > client.len(),
        "a client chain is only ever extended"
    );
    debug_assert!(
        is_crlf_free(&value[client.len()..]) && value.len() == value.capacity(),
        "the appended hop is CR/LF-free and the value exactly sized"
    );
    kawa::Store::from_vec(value)
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
    /// the value of the reason in the response line: the `'static` phrase
    /// RFC 9110 §15 registers for the status code when the backend sent
    /// exactly that phrase, a copy otherwise (`standard_reason`)
    pub reason: Option<Cow<'static, str>>,
    // ---------- Additional optional data
    pub user_agent: Option<String>,
    /// Value of the `x-request-id` header observed (if propagated from the
    /// client/upstream LB) or generated (from `self.id`). Universal correlation
    /// header — populated unconditionally by `on_request_headers` so the access
    /// log can record the exact value forwarded to the backend. `Rc` because
    /// a generated value is the one rendering of `self.id` that the forwarded
    /// `X-Request-Id` and both correlation headers share
    /// (`kawa::Store::Shared`).
    pub x_request_id: Option<Rc<str>>,
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
    /// Request ULID: the value of the correlation header, named "Sozu-Id" by
    /// default, that Kawa should write (request and response), of a generated
    /// `X-Request-Id` and of `%REQUEST_ID`. Request-scoped: `reset` replaces
    /// it for each keep-alive request, unlike [`Self::session_id`].
    pub id: Ulid,
    pub backend_id: Option<Rc<str>>,
    pub cluster_id: Option<sozu_command_lib::state::ClusterId>,
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
    /// Snapshot of the SAN set of the certificate Sōzu actually served at
    /// the TLS handshake. Captured once in `https.rs::upgrade_handshake`
    /// from the resolver and frozen for the connection lifetime so H2
    /// stream coalescing (RFC 7540 §9.1.1 / RFC 9113 §9.1.1) accepts any
    /// `:authority` covered by the certificate, with RFC 6125 §6.4.3
    /// wildcard handling. `None` for plaintext listeners or when SNI was
    /// absent (router falls back to the legacy exact-match predicate).
    /// `Some(empty)` when the default cert was served — every
    /// `:authority` is rejected. `Arc` so the snapshot is shared across
    /// every per-stream `HttpContext` without re-allocation.
    pub tls_cert_names: Option<Arc<Vec<String>>>,
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
    /// response. Defaults to `"Sozu-Id"` via [`crate::L7ListenerHandler::get_sozu_id_header`].
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
    /// answer. Computed in `Router::plan_connect` when the per-(cluster,
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
    /// Access-log tags of the frontend rule that matched this request,
    /// stashed by the routing layer from `RouteResult::tags` (see
    /// `mux/router.rs::route_from_request`) and read back by
    /// `mux/stream.rs::generate_access_log`.
    ///
    /// The matched frontend is the only correct owner of these tags: the
    /// operator configures them per frontend RULE (`*.example.com`,
    /// `/foo.*/.example.com`, a Pre/Post string), while the request only
    /// ever carries a concrete authority. Resolving them here — rather
    /// than by looking the authority up in the listener's
    /// `BTreeMap<String, CachedTags>` — is what makes a wildcard, regex,
    /// ported or differently-cased frontend log its tags at all
    /// (sozu#1379). `Rc` because the same `CachedTags` is shared by every
    /// stream routed to that frontend, and because it must outlive a
    /// frontend removed mid-request.
    ///
    /// `None` until the request is routed, and for a request that matched
    /// no frontend at all.
    pub tags: Option<Rc<CachedTags>>,
    /// The forwarding values rendered for this connection, reused by every
    /// request whose inputs match (`HttpContext::forwarding_hop`).
    /// Connection-scoped: `reset` keeps it. `None` until the first request
    /// is edited.
    pub(crate) forwarding_hop: Option<ForwardingHop>,
    /// Stable, structured discriminator surfaced as the access-log
    /// `message` field when the session terminates on a timeout. Set by
    /// the `MuxState::timeout` handler
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

/// A ULID in its canonical 26-character Crockford base-32 form, rendered on
/// the stack: `Ulid`'s `Display` allocates a `String` to render it.
fn render_ulid(id: Ulid) -> [u8; 26] {
    const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let value = u128::from(id);
    let mut rendered = [0u8; 26];
    // 26 digits of 5 bits carry 130 bits: the first digit holds the top 3
    // bits of the 128-bit value, every later one the next 5.
    for (index, digit) in rendered.iter_mut().enumerate() {
        let shift = 5 * (25 - index);
        *digit = CROCKFORD[((value >> shift) & 0x1f) as usize];
    }
    // Post: every digit is a Crockford base-32 character, so the rendering
    // is ASCII and CR/LF-free wherever it is spliced into a header.
    debug_assert!(
        rendered.iter().all(|digit| CROCKFORD.contains(digit)),
        "a rendered ULID must only carry Crockford base-32 digits"
    );
    rendered
}

/// The reason phrase RFC 9110 §15 registers for `code`, plus 429 from
/// RFC 6585 §4, or `None` for a code it registers none for.
///
/// `on_response_headers` captures a backend's reason for the access log by
/// reference to this phrase when the two are byte-identical, and copies it
/// otherwise: an older name (`Payload Too Large`), a different case or a
/// custom phrase is recorded verbatim, never normalised.
fn standard_reason(code: u16) -> Option<&'static str> {
    let phrase = match code {
        100 => "Continue",
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        203 => "Non-Authoritative Information",
        204 => "No Content",
        205 => "Reset Content",
        206 => "Partial Content",
        300 => "Multiple Choices",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        305 => "Use Proxy",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        407 => "Proxy Authentication Required",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Content Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        416 => "Range Not Satisfiable",
        417 => "Expectation Failed",
        421 => "Misdirected Request",
        422 => "Unprocessable Content",
        426 => "Upgrade Required",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        _ => return None,
    };
    // Post: a registered phrase is non-empty and CR/LF-free; it is only ever
    // compared with, and logged in place of, identical wire bytes.
    debug_assert!(
        !phrase.is_empty() && is_crlf_free(phrase.as_bytes()),
        "a registered reason phrase is a non-empty single-line token"
    );
    Some(phrase)
}

/// The store of a correlation-header name: the default `Sozu-Id` — what
/// `L7ListenerHandler::get_sozu_id_header` (`lib/src/lib.rs`) and its
/// `HttpListener` and `HttpsListener` implementations answer when the
/// listener does not rename it — is a `'static` literal and costs nothing; a
/// renamed header is copied.
fn correlation_header_name(name: &str) -> kawa::Store {
    const DEFAULT: &str = "Sozu-Id";
    if name == DEFAULT {
        kawa::Store::Static(DEFAULT.as_bytes())
    } else {
        kawa::Store::from_slice(name.as_bytes())
    }
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
            tls_cert_names: None,
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
            tags: None,
            forwarding_hop: None,
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
        // Editing never drops a block — it only elides in place (key set to
        // Empty, length preserved) or pushes new headers. Snapshot the count
        // so the postcondition can pin "blocks only grow" for the whole edit.
        let blocks_at_entry = request.blocks.len();

        // Defense-in-depth against CL.TE request smuggling (CWE-444, RFC 9110 §7.6 /
        // RFC 9112 §6.1; reopen of #726). kawa leaves every Transfer-Encoding field line
        // in place, so we reason about ALL surviving TE headers rather than its aggregate
        // `body_size`. Reject:
        //   * more than one non-elided TE header  (RFC 9112 §6.1: chunked must be applied
        //     once and be the final coding; several field lines cannot be safely
        //     reconciled here, and forwarding them all hands the backend the combining
        //     problem we just declined to solve), or
        //   * a TE header whose field value does not end in `chunked`, or
        //   * a TE header present while kawa did not adopt chunked framing.
        //
        // WHICH CLAUSE ACTUALLY FIRES, against the kawa this workspace pins (`^0.7.1`,
        // locked at 0.7.1). Read from kawa 0.7.1's `process_headers`
        // (its `src/protocol/h1/parser/mod.rs`), not measured here: kawa resolves the
        // combined Transfer-Encoding BEFORE this callback and, for a REQUEST whose
        // combined final coding is not chunked, errors the parse and returns without
        // calling `callbacks.on_headers` at all (RFC 9112 §6.3). It judges the LAST TE
        // line, per RFC 9110 §5.3 combining, so an earlier `chunked` line can no longer
        // latch chunked framing on its own — that latch was a kawa 0.7.0 shape.
        //   * the COUNT clause is the one that fires on traffic kawa accepted:
        //     `Transfer-Encoding: identity` followed by `Transfer-Encoding: chunked`
        //     combines to a chunked-final coding, so kawa parses it clean, and forwarding
        //     both lines is what we refuse.
        //   * the other two cannot fire on a request kawa accepted: a surviving TE line is
        //     chunked-final (or kawa already refused the request), and `body_size` is then
        //     Chunked. `chunked, gzip` never reaches this code — kawa refused it. They are
        //     defense in depth against a kawa regression. Keep them; do not describe them
        //     as closing a live hole.
        //
        // OWS IS NOT AMBIGUITY, and this guard must not treat it as one. kawa >=0.7.1
        // trims OWS from every field value at parse time (`trim_ows` in its
        // `parser/primitives.rs`, RFC 9112 §5), so `header.val` here already reads
        // `chunked` for `Transfer-Encoding: chunked\t`: the suffix check passes,
        // `body_size` is Chunked, the request is ACCEPTED — and it reaches the backend
        // spelled `chunked`, with the Content-Length elided. Measured:
        // `test_h1_te_ows_forwarded_canonically` (`e2e/src/tests/h1_security_tests.rs`)
        // sends exactly that request and pins the forwarded bytes. An obfuscated coding
        // over a valid chunked body is legal traffic; refusing it would refuse legal
        // traffic.
        //
        // kawa 0.7.0 is what the suffix check was written against: it framed on the
        // trimmed reading but forwarded the field line verbatim, so a backend that did not
        // itself trim saw neither a coding it recognized nor a Content-Length, and read
        // our chunked body as a pipelined request — a TE.TE desync. 0.7.1 closed that at
        // the source. Do not reintroduce a trailing-OWS rejection here to compensate.
        //
        // The `trailing-tab` / `trailing-space` rows of that module's `TE_SMUGGLING_CASES`
        // still answer 400, for their invalid chunked BODY (`Hello` is not a chunk) and
        // not through this predicate — as that table's own doc comment says. Their 400 is
        // not evidence that this guard rejects an obfuscated coding.
        let (te_count, te_all_suffix_chunked) = {
            const CHUNKED: &[u8] = b"chunked";
            let buf0 = request.storage.buffer();
            request
                .blocks
                .iter()
                .filter_map(|block| match block {
                    kawa::Block::Header(header)
                        if !header.is_elided()
                            && compare_no_case(header.key.data(buf0), b"transfer-encoding") =>
                    {
                        Some(header.val.data(buf0))
                    }
                    _ => None,
                })
                .fold((0usize, true), |(count, all_chunked), val| {
                    let suffix_chunked = val.len() >= CHUNKED.len()
                        && compare_no_case(&val[val.len() - CHUNKED.len()..], CHUNKED);
                    (count + 1, all_chunked && suffix_chunked)
                })
        };
        if te_count > 1
            || (te_count == 1
                && (!te_all_suffix_chunked || request.body_size != kawa::BodySize::Chunked))
        {
            incr!(names::http::FRONTEND_TE_SMUGGLING);
            warn!(
                "{} rejecting request: ambiguous Transfer-Encoding framing (possible CL.TE request smuggling)",
                self.log_context()
            );
            request
                .parsing_phase
                .error("Transfer-Encoding conflicts with message framing".into());
            return;
        }

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

        let public_port = self.public_address.port();
        let proto = match self.protocol {
            Protocol::HTTP => "http",
            Protocol::HTTPS => "https",
            _ => unreachable!(),
        };

        // `proto` is the proxy-resolved scheme label, sourced from the
        // listener protocol (never from request bytes). The match above
        // already rejects anything but HTTP/HTTPS; pin that it is one of the
        // two CR/LF-free literals we splice into forwarding headers.
        debug_assert!(
            proto == "http" || proto == "https",
            "proto must be the http/https scheme label"
        );

        // Find and remove the sticky_name cookie
        // if found its value is stored in sticky_session_found
        for cookie in &mut request.detached.jar {
            let key = cookie.key.data(buf);
            if key == self.sticky_name.as_bytes() {
                let val = cookie.val.data(buf);
                self.sticky_session_found = from_utf8(val).ok().map(ToOwned::to_owned);
                cookie.elide();
                // Post: the matched sticky cookie is gone from the forwarded
                // jar so the backend never sees Sōzu's own session cookie.
                debug_assert!(
                    cookie.is_elided(),
                    "the matched sticky cookie must be elided after capture"
                );
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
                        incr!(names::http::TRUSTING_X_PROTO);
                        let val = header.val.data(buf);
                        if !compare_no_case(val, proto.as_bytes()) {
                            incr!(names::http::TRUSTING_X_PROTO_DIFF);
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
                        incr!(names::http::TRUSTING_X_PORT);
                        let val = header.val.data(buf);
                        let mut port_buf = itoa::Buffer::new();
                        let expected = port_buf.format(public_port);
                        if !compare_no_case(val, expected.as_bytes()) {
                            incr!(names::http::TRUSTING_X_PORT_DIFF);
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
                        debug_assert!(
                            self.elide_x_real_ip,
                            "X-Real-IP is only elided when anti-spoofing is enabled"
                        );
                        header.elide();
                        // Post: the spoofable client value is stripped before
                        // forwarding (anti-spoofing invariant, CWE-348).
                        debug_assert!(
                            header.is_elided(),
                            "client X-Real-IP must be elided when elide_x_real_ip is set"
                        );
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
                            .map(Rc::from);
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

        // The connection's forwarding values, rendered by its first request
        // and shared by the next ones: a reference-count bump each, never a
        // copy (`HttpContext::forwarding_hop`).
        let (port_hop, x_forwarded_for_hop, forwarded_hop) = {
            let hop = self.forwarding_hop(proto);
            (
                hop.port.clone(),
                hop.x_forwarded_for.clone(),
                hop.forwarded.clone(),
            )
        };

        // If session_address is set:
        // - append its ip address to the list of "X-Forwarded-For" if it was found, creates it if not
        // - append "proto=[PROTO];for=[PEER];by=[PUBLIC]" to the list of "Forwarded" if it was found, creates it if not
        if let (Some(x_forwarded_for_hop), Some(forwarded_hop)) =
            (x_forwarded_for_hop, forwarded_hop)
        {
            let has_x_for = x_for.is_some();
            let has_forwarded = forwarded.is_some();

            // A client-supplied chain is request-scoped: it is extended into
            // one exact-size copy (`extended_chain`), never kept.
            if let Some(header) = x_for {
                header.val = extended_chain(header.val.data(buf), &x_forwarded_for_hop);
            }
            if let Some(header) = &mut forwarded {
                header.val = extended_chain(header.val.data(buf), &forwarded_hop);
            }

            if !has_x_for {
                let blocks_before = request.blocks.len();
                request.push_block(kawa::Block::Header(kawa::Pair {
                    key: kawa::Store::Static(b"X-Forwarded-For"),
                    val: synthesised_from_hop(&x_forwarded_for_hop),
                }));
                debug_assert_eq!(
                    request.blocks.len(),
                    blocks_before + 1,
                    "creating X-Forwarded-For must push exactly one block"
                );
            }
            if !has_forwarded {
                let blocks_before = request.blocks.len();
                request.push_block(kawa::Block::Header(kawa::Pair {
                    key: kawa::Store::Static(b"Forwarded"),
                    val: synthesised_from_hop(&forwarded_hop),
                }));
                debug_assert_eq!(
                    request.blocks.len(),
                    blocks_before + 1,
                    "creating Forwarded must push exactly one block"
                );
            }

            // Inject a proxy-generated `X-Real-IP` header carrying the
            // peer IP (post-PROXY-v2 unwrap, so the original client IP
            // even when the upstream presented PROXY-v2). Folded into the
            // peer arm so missing peers (raw socket, no PROXY-v2) skip the
            // injection silently — identical to the X-Forwarded-For /
            // Forwarded synthesis behaviour above. It shares the
            // X-Forwarded-For rendering of the peer. Any client-supplied
            // `X-Real-IP` was either elided in the block walk (if
            // `elide_x_real_ip` is on) or passes through; this header is
            // appended last so order in the resulting block list is
            // deterministic for tests.
            if self.send_x_real_ip {
                let blocks_before = request.blocks.len();
                request.push_block(kawa::Block::Header(kawa::Pair {
                    key: kawa::Store::Static(b"X-Real-IP"),
                    val: synthesised_from_hop(&x_forwarded_for_hop),
                }));
                debug_assert_eq!(
                    request.blocks.len(),
                    blocks_before + 1,
                    "injecting X-Real-IP must push exactly one block"
                );
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
            request.push_block(kawa::Block::Header(kawa::Pair {
                key: kawa::Store::Static(b"X-Forwarded-Port"),
                val: kawa::Store::Shared(port_hop, 0),
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
            incr!(names::http::X_REQUEST_ID_PROPAGATED);
        } else {
            // The one rendering of `self.id` this request allocates: the
            // forwarded header, the access log and both correlation headers
            // share it (`Self::shared_id`).
            let rendered = render_ulid(self.id);
            let value: Rc<str> = Rc::from(from_utf8(&rendered).expect("a rendered ULID is ASCII"));
            // The generated id is a ULID rendering — Crockford base-32, so
            // CR/LF-free by construction.
            debug_assert!(
                is_crlf_free(value.as_bytes()),
                "the generated X-Request-Id must be CR/LF-free"
            );
            request.push_block(kawa::Block::Header(kawa::Pair {
                key: kawa::Store::Static(b"X-Request-Id"),
                val: kawa::Store::Shared(Rc::<[u8]>::from(value.clone()), 0),
            }));
            self.x_request_id = Some(value);
            incr!(names::http::X_REQUEST_ID_GENERATED);
        }
        // Either branch leaves the forwarded value recorded for the access
        // log so it matches exactly what the backend receives.
        debug_assert!(
            self.x_request_id.is_some(),
            "on_request_headers must record the forwarded X-Request-Id"
        );

        // Create a custom correlation header (defaults to "Sozu-Id", can be
        // renamed via the `sozu_id_header` listener config knob).
        let blocks_before_sozu_id = request.blocks.len();
        request.push_block(kawa::Block::Header(kawa::Pair {
            key: correlation_header_name(&self.sozu_id_header),
            val: kawa::Store::Shared(self.shared_id(), 0),
        }));
        debug_assert_eq!(
            request.blocks.len(),
            blocks_before_sozu_id + 1,
            "the Sozu-Id correlation header must be pushed exactly once"
        );

        // Postcondition: the whole edit only ever added or elided headers —
        // the block count is monotonically non-decreasing (a removed header
        // is elided in place, never popped), so the backend never loses a
        // header it should have seen.
        debug_assert!(
            request.blocks.len() >= blocks_at_entry,
            "header editing must never drop a block from the request"
        );
    }

    /// Callback for response:
    ///
    /// - edit headers (connection, set-cookie, sozu-id)
    /// - save information:
    ///   - status code
    ///   - reason
    ///   - back keep-alive
    fn on_response_headers(&mut self, response: &mut GenericHttpStream) {
        // Like the request path, response editing only adds or elides — pin
        // the entry count so the postcondition can assert "blocks only grow".
        let blocks_at_entry = response.blocks.len();

        let buf = &mut response.storage.mut_buffer();

        // Captures the response line
        if let kawa::StatusLine::Response { code, reason, .. } = &response.detached.status_line {
            self.status = Some(*code);
            // The standard phrase for the code is borrowed, anything else
            // copied: the access log records the wire bytes either way.
            self.reason = reason
                .data_opt(buf)
                .and_then(|data| from_utf8(data).ok())
                .map(|data| match standard_reason(*code) {
                    Some(phrase) if phrase == data => Cow::Borrowed(phrase),
                    _ => Cow::Owned(data.to_owned()),
                });
            // Post: the captured reason is the wire reason, byte for byte,
            // whichever representation carries it.
            debug_assert_eq!(
                self.reason.as_deref().map(str::as_bytes),
                reason.data_opt(buf).filter(|data| from_utf8(data).is_ok()),
                "the captured reason must be the backend's own bytes"
            );
            debug_assert!(
                !matches!(&self.reason, Some(Cow::Owned(owned))
                    if standard_reason(*code) == Some(owned.as_str())),
                "a standard reason must be borrowed, never copied"
            );
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
        if let Some(sticky_session) = &self.sticky_session
            && self.sticky_session != self.sticky_session_found
        {
            let blocks_before = response.blocks.len();
            let mut cookie_buf =
                Vec::with_capacity(self.sticky_name.len() + 1 + sticky_session.len() + 8);
            cookie_buf.extend_from_slice(self.sticky_name.as_bytes());
            cookie_buf.push(b'=');
            cookie_buf.extend_from_slice(sticky_session.as_bytes());
            cookie_buf.extend_from_slice(b"; Path=/");
            // The cookie value is `name=session; Path=/`, built from the
            // proxy-controlled sticky name + session id — assert it is
            // well-formed (contains the `=` separator, ends with the
            // attribute suffix) and CR/LF-free so it cannot inject an
            // extra Set-Cookie / split the response (CWE-113).
            debug_assert!(
                cookie_buf.contains(&b'='),
                "the Set-Cookie value must carry the name=value separator"
            );
            debug_assert!(
                cookie_buf.ends_with(b"; Path=/"),
                "the Set-Cookie value must end with the Path attribute"
            );
            debug_assert!(
                is_crlf_free(&cookie_buf),
                "the synthesised Set-Cookie value must be CR/LF-free (anti-injection)"
            );
            response.push_block(kawa::Block::Header(kawa::Pair {
                key: kawa::Store::Static(b"Set-Cookie"),
                val: kawa::Store::from_vec(cookie_buf),
            }));
            debug_assert_eq!(
                response.blocks.len(),
                blocks_before + 1,
                "synthesising Set-Cookie must push exactly one block"
            );
        }

        // Create a custom correlation header (defaults to "Sozu-Id", can be
        // renamed via the `sozu_id_header` listener config knob).
        let blocks_before_sozu_id = response.blocks.len();
        response.push_block(kawa::Block::Header(kawa::Pair {
            key: correlation_header_name(&self.sozu_id_header),
            val: kawa::Store::Shared(self.shared_id(), 0),
        }));
        debug_assert_eq!(
            response.blocks.len(),
            blocks_before_sozu_id + 1,
            "the Sozu-Id correlation header must be pushed exactly once"
        );

        // Postcondition: response editing only added or elided headers, so
        // the block count never decreased.
        debug_assert!(
            response.blocks.len() >= blocks_at_entry,
            "header editing must never drop a block from the response"
        );
    }

    /// The connection's forwarding values for the current inputs, rendered
    /// only when the cached ones were rendered from different inputs — the
    /// first request of a connection, or an input changed since.
    fn forwarding_hop(&mut self, proto: &str) -> &ForwardingHop {
        let (protocol, public_address, session_address) =
            (self.protocol, self.public_address, self.session_address);
        let cached = self
            .forwarding_hop
            .as_ref()
            .is_some_and(|hop| hop.renders(protocol, public_address, session_address));
        if !cached {
            self.forwarding_hop = Some(ForwardingHop::render(
                protocol,
                proto,
                public_address,
                session_address,
            ));
        }
        let hop = self
            .forwarding_hop
            .as_ref()
            .expect("the forwarding values were just rendered or reused");
        // Post: the values handed out were rendered from the current inputs.
        debug_assert!(
            hop.renders(protocol, public_address, session_address),
            "the forwarding values must match the current inputs"
        );
        hop
    }

    /// `self.id` rendered, as a shared value for the correlation header.
    ///
    /// When the request carried no `X-Request-Id`, `on_request_headers`
    /// generated one from `self.id` and kept it in `self.x_request_id`: the
    /// same 26 bytes, shared with a reference-count bump. A client-supplied
    /// `X-Request-Id` is a different value, and `self.id` is then rendered
    /// into a copy of its own, because the correlation header always carries
    /// Sōzu's id, whatever the client sent.
    fn shared_id(&self) -> Rc<[u8]> {
        let rendered = render_ulid(self.id);
        let shared = match &self.x_request_id {
            Some(generated) if generated.as_bytes() == rendered => {
                Rc::<[u8]>::from(generated.clone())
            }
            _ => Rc::from(&rendered[..]),
        };
        debug_assert_eq!(
            &*shared, &rendered,
            "the correlation header must carry this request's own id"
        );
        shared
    }

    /// Prepare this context for the next request of a keep-alive connection,
    /// which `request_id` identifies.
    ///
    /// The request id is request-scoped like the rest of what this clears:
    /// `Sozu-Id`, a generated `X-Request-Id`, `%REQUEST_ID` and the access
    /// log's `request_id` all read `self.id`, so keeping it would give every
    /// request of the connection the first one's id. The caller mints it —
    /// `ConnectionH1` from `Context::next_request_id`
    /// (`lib/src/protocol/mux/mod.rs`), the same source that numbers H2
    /// streams — because this struct owns neither a clock nor an RNG.
    pub fn reset(&mut self, request_id: Ulid) {
        // Snapshot the connection-scoped identity + TLS/listener fields that
        // reset() must NOT touch (set once at handshake, reused across every
        // keep-alive request). Cheap to copy — all `Copy`. Read only inside
        // the postcondition `debug_assert!`s below, so dead code in release.
        let session_id_before = self.session_id;
        let id_before = self.id;
        debug_assert_ne!(
            request_id, id_before,
            "the next request of a connection needs an id of its own"
        );
        let strict_sni_before = self.strict_sni_binding;
        let elide_before = self.elide_x_real_ip;
        let send_before = self.send_x_real_ip;
        let tls_version_before = self.tls_version;
        let tls_cipher_before = self.tls_cipher;
        let tls_alpn_before = self.tls_alpn;
        let forwarding_hop_before = self
            .forwarding_hop
            .as_ref()
            .map(|hop| Rc::as_ptr(&hop.port));

        self.id = request_id;
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
        // them in place. So is forwarding_hop, rendered from connection-
        // scoped inputs only.

        // Post: request-scoped state is fully cleared (a stale value here
        // would leak across pipelined requests on the same connection).
        debug_assert!(
            self.method.is_none()
                && self.authority.is_none()
                && self.path.is_none()
                && self.status.is_none()
                && self.x_request_id.is_none()
                && self.headers_response.is_empty(),
            "reset() must clear all request-scoped state"
        );
        debug_assert!(
            self.keep_alive_backend && self.keep_alive_frontend,
            "reset() must restore keep-alive to its optimistic default"
        );
        // Post: connection-scoped identity + TLS/listener knobs are untouched.
        debug_assert_eq!(
            self.session_id, session_id_before,
            "reset() must preserve the connection session id"
        );
        debug_assert!(
            self.id == request_id && self.id != id_before,
            "reset() must give the next request its own id"
        );
        debug_assert!(
            self.strict_sni_binding == strict_sni_before
                && self.elide_x_real_ip == elide_before
                && self.send_x_real_ip == send_before
                && self.tls_version == tls_version_before
                && self.tls_cipher == tls_cipher_before
                && self.tls_alpn == tls_alpn_before,
            "reset() must preserve connection-scoped TLS/listener knobs"
        );
        debug_assert_eq!(
            self.forwarding_hop
                .as_ref()
                .map(|hop| Rc::as_ptr(&hop.port)),
            forwarding_hop_before,
            "reset() must keep the connection's forwarding values"
        );
    }

    pub fn extract_route(&self) -> Result<(&str, &str, &Method), RetrieveClusterError> {
        let given_method = self.method.as_ref().ok_or(RetrieveClusterError::NoMethod)?;
        let given_authority = self
            .authority
            .as_deref()
            .ok_or(RetrieveClusterError::NoHost)?;
        let given_path = self.path.as_deref().ok_or(RetrieveClusterError::NoPath)?;

        // Post: the triple is returned in (authority, path, method) order —
        // pin it against a future field-swap regression that would route to
        // the wrong cluster. (Both fields can be empty strings on the wire,
        // so we assert the mapping, not non-emptiness.)
        debug_assert!(
            std::ptr::eq(given_authority, self.authority.as_deref().unwrap())
                && std::ptr::eq(given_path, self.path.as_deref().unwrap()),
            "extract_route must return (authority, path, method) in order"
        );
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
            reason: self.reason.as_deref().map(ToOwned::to_owned),
            status: self.status,
        }
    }

    pub fn log_context(&self) -> LogContext<'_> {
        let ctx = LogContext {
            session_id: self.session_id,
            request_id: Some(self.id),
            cluster_id: self.cluster_id.as_deref(),
            backend_id: self.backend_id.as_deref(),
        };
        // The access-log bracket `[session req cluster backend]` is keyed on
        // session + request id; both must always be present so the log line
        // is correlatable. cluster/backend are legitimately absent before
        // routing, so they are not asserted here.
        debug_assert!(
            ctx.request_id.is_some(),
            "log_context must always carry the request id for correlation"
        );
        ctx
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
        ctx.reason = Some(Cow::Owned("Fine".to_owned()));
        ctx.user_agent = Some("curl/7.81".to_owned());
        ctx.x_request_id = Some(Rc::from("client-xrid-123"));
        ctx.xff_chain = Some("203.0.113.5, 198.51.100.10".to_owned());
        ctx.redirect_location = Some("https://example.com/".to_owned());
        ctx.www_authenticate = Some("Basic realm=\"sozu\"".to_owned());
        ctx.original_authority = Some("old.example.com".to_owned());
        ctx.headers_response.push(HeaderEditSnapshot {
            key: b"X-Cache".to_vec(),
            val: b"HIT".to_vec(),
            mode: HeaderEditMode::Append,
        });

        ctx.reset(Ulid::generate());

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

        ctx.reset(Ulid::generate());

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
        ctx.cluster_id = Some("cluster-1".into());
        ctx.backend_id = Some("backend-1".into());
        ctx.sticky_session = Some("session-abc".to_owned());

        let original_id = ctx.id;
        let original_protocol = ctx.protocol;
        let original_public_address = ctx.public_address;

        let next_id = Ulid::generate();
        ctx.reset(next_id);

        // Connection-level state is preserved
        assert!(ctx.closing);
        assert_eq!(ctx.cluster_id.as_deref(), Some("cluster-1"));
        assert_eq!(ctx.backend_id.as_deref(), Some("backend-1"));
        assert_eq!(ctx.sticky_session.as_deref(), Some("session-abc"));
        // The request id is request-scoped: a keep-alive connection's next
        // request must not inherit the previous one's.
        assert_ne!(ctx.id, original_id);
        assert_eq!(ctx.id, next_id);
        assert_eq!(ctx.protocol, original_protocol);
        assert_eq!(ctx.public_address, original_public_address);
    }

    // ── write_forwarded_for_by (RFC 7239 §6 IP-literal bracketing) ──────
    //
    // Locks in the IPv6 bracket+quote contract that matches HAProxy
    // (`_7239_print_ip6` in `src/http_ext.c`) and prevents regression to
    // the ambiguous `for="2001:db8::1:8080"` form flagged in issue #1254.

    use std::net::Ipv6Addr;

    fn render_for_by(peer: SocketAddr, public_ip: IpAddr) -> String {
        let mut buf = Vec::new();
        write_forwarded_for_by(&mut buf, peer.ip(), peer.port(), public_ip);
        String::from_utf8(buf).expect("forwarded fragment must be ASCII")
    }

    fn render_suffix(proto: &str, peer: SocketAddr, public_ip: IpAddr) -> String {
        let mut buf = Vec::new();
        write_forwarded_suffix(&mut buf, proto, peer.ip(), peer.port(), public_ip);
        String::from_utf8(buf).expect("forwarded fragment must be ASCII")
    }

    #[test]
    fn test_forwarded_ipv4_peer_ipv4_by_unchanged() {
        // IPv4 must keep the pre-fix wire format: unquoted `by=`, bare IPv4 in
        // `for=`. Sōzu emits `for=` quoted unconditionally (HAProxy emits it
        // quoted only when a port is attached; we always attach a port).
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 54321);
        let public_ip = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 1));
        assert_eq!(
            render_for_by(peer, public_ip),
            r#";for="203.0.113.7:54321";by=198.51.100.1"#,
        );
    }

    #[test]
    fn test_forwarded_ipv6_peer_brackets_disambiguate_port() {
        // Regression for issue #1254: without brackets, `for="2001:db8::1:8080"`
        // is ambiguous (could parse as `::1` + port `8080` or `:1:8080` literal).
        let peer = SocketAddr::new(IpAddr::V6("2001:db8::1".parse::<Ipv6Addr>().unwrap()), 8080);
        let public_ip = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 1));
        assert_eq!(
            render_for_by(peer, public_ip),
            r#";for="[2001:db8::1]:8080";by=198.51.100.1"#,
        );
    }

    #[test]
    fn test_forwarded_ipv6_public_address_bracketed_and_quoted() {
        // `by=` carries no port but RFC 7239 §6 still requires the IP-literal
        // brackets for IPv6, and the value must be quoted because the literal
        // contains `:`.
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 54321);
        let public_ip = IpAddr::V6("2001:db8::2".parse::<Ipv6Addr>().unwrap());
        assert_eq!(
            render_for_by(peer, public_ip),
            r#";for="203.0.113.7:54321";by="[2001:db8::2]""#,
        );
    }

    #[test]
    fn test_forwarded_ipv6_peer_and_public_both_bracketed() {
        let peer = SocketAddr::new(IpAddr::V6("2001:db8::1".parse::<Ipv6Addr>().unwrap()), 8080);
        let public_ip = IpAddr::V6("2001:db8::2".parse::<Ipv6Addr>().unwrap());
        assert_eq!(
            render_for_by(peer, public_ip),
            r#";for="[2001:db8::1]:8080";by="[2001:db8::2]""#,
        );
    }

    #[test]
    fn test_forwarded_suffix_prepends_proto_and_brackets_ipv6() {
        // The suffix form is what we append when an inbound `Forwarded`
        // header already exists. Same bracketing contract must hold.
        let peer = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 443);
        let public_ip = IpAddr::V6("fe80::1".parse::<Ipv6Addr>().unwrap());
        assert_eq!(
            render_suffix("https", peer, public_ip),
            r#", proto=https;for="[::1]:443";by="[fe80::1]""#,
        );
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

    // ── header-editing allocations ─────────────────────────────────────

    /// A bare keep-alive request: no forwarding header, no `X-Request-Id`,
    /// no `User-Agent`, so every header the editor adds is synthesised.
    const BARE_REQUEST: &[u8] = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";

    /// Parse `request` through the real parser, and so through
    /// `on_request_headers`, into `kawa`, and return the heap allocations
    /// the parse made.
    ///
    /// `kawa` is cleared first, the way the H1 keep-alive path clears a
    /// stream's front kawa between two requests, so a caller that parses on
    /// the same kawa twice measures the second parse without the growth of
    /// kawa's own block queue.
    fn allocations_of_request_parse(
        ctx: &mut HttpContext,
        kawa: &mut GenericHttpStream,
        request: &[u8],
    ) -> usize {
        use crate::test_allocations::allocations;
        kawa.clear();
        kawa.storage.clear();
        kawa.storage.space()[..request.len()].copy_from_slice(request);
        kawa.storage.fill(request.len());
        let before = allocations();
        kawa::h1::parse(kawa, ctx);
        let parsed = allocations() - before;
        assert!(kawa.is_main_phase(), "premise: the request must parse");
        parsed
    }

    /// A kawa on its own pool buffer, its block queue warmed by one parse on
    /// a throwaway context so later measurements see only the editor.
    fn warm_request_kawa(pool: &mut crate::pool::Pool) -> GenericHttpStream {
        let mut kawa: GenericHttpStream = kawa::Kawa::new(
            kawa::Kind::Request,
            kawa::Buffer::new(
                pool.checkout()
                    .expect("the test pool must hand out a buffer"),
            ),
        );
        allocations_of_request_parse(&mut make_context(), &mut kawa, BARE_REQUEST);
        kawa
    }

    /// The bytes `kawa` would send upstream, serialised the way
    /// `ConnectionH1::writable` serialises them.
    fn serialized_request(kawa: &mut GenericHttpStream) -> String {
        kawa.prepare(&mut kawa::h1::BlockConverter);
        let buffer = kawa.storage.buffer();
        let bytes = kawa
            .out
            .iter()
            .flat_map(|block| match block {
                kawa::OutBlock::Store(store) => store.data(buffer).to_vec(),
                kawa::OutBlock::Delimiter => Vec::new(),
            })
            .collect::<Vec<u8>>();
        String::from_utf8(bytes).expect("the serialised request must be UTF-8")
    }

    /// The `traceparent` line `on_request_headers` synthesises for a request
    /// that carried none, rendered from the ids it recorded on `ctx.otel`;
    /// empty without the `opentelemetry` feature, which adds no header.
    fn synthesised_traceparent_line(ctx: &HttpContext) -> String {
        #[cfg(feature = "opentelemetry")]
        {
            let otel = ctx
                .otel
                .as_ref()
                .expect("on_request_headers records the trace context it forwards");
            let value = build_traceparent(&otel.trace_id, &otel.span_id);
            format!(
                "traceparent: {}\r\n",
                from_utf8(&value).expect("a traceparent is ASCII hex")
            )
        }
        #[cfg(not(feature = "opentelemetry"))]
        {
            let _ = ctx;
            String::new()
        }
    }

    /// Heap operations of the `traceparent` header `on_request_headers`
    /// synthesises under the `opentelemetry` feature: its value is built on
    /// the stack (`build_traceparent`) and copied once
    /// (`kawa::Store::from_slice`), the same one exact copy as every other
    /// synthesised header. Without the feature there is no such header.
    const SYNTHESISED_TRACEPARENT: usize = if cfg!(feature = "opentelemetry") {
        1
    } else {
        0
    };

    /// The first request of a connection: every forwarding header is
    /// synthesised from the connection's forwarding values, which one
    /// scratch renders and each copies out once; the request id is rendered
    /// once. The next requests reuse the values
    /// (`keep_alive_requests_reuse_the_connection_forwarding_hop`).
    ///
    /// The seven: `authority` and `path` captured for routing and the access
    /// log (2), the scratch (1), the `X-Forwarded-For` hop, the `Forwarded`
    /// element and the `X-Forwarded-Port` value (3), and the one rendering of
    /// the request id that `X-Request-Id`, the access log and the `Sozu-Id`
    /// value share (1). The default `Sozu-Id` name is a `'static` literal
    /// (0). Under the `opentelemetry` feature, the synthesised `traceparent`
    /// adds its one copy ([`SYNTHESISED_TRACEPARENT`]).
    ///
    /// TO SEE THIS RED, either of:
    /// - in `on_request_headers`, allocate a scratch for every request again
    ///   to extend the client chains in, as the recipe of
    ///   `a_client_chain_costs_one_exact_copy_per_extended_header` does.
    ///   Measured: `left: 8, right: 7`.
    /// - render the id per header again: `kawa::Store::from_string(
    ///   self.id.to_string())` for the `Sozu-Id` value, `from_string(
    ///   self.sozu_id_header.clone())` for its name, and a `self.id
    ///   .to_string()` copied for `X-Request-Id`. Measured: `left: 10,
    ///   right: 7`.
    #[test]
    fn a_bare_request_costs_one_scratch_and_one_copy_per_forwarding_header() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let mut kawa = warm_request_kawa(&mut pool);
        let mut ctx = make_context();

        let first = allocations_of_request_parse(&mut ctx, &mut kawa, BARE_REQUEST);

        assert_eq!(
            first,
            7 + SYNTHESISED_TRACEPARENT,
            "a bare request's header editing must cost one scratch, one exact \
             copy per synthesised forwarding header and one rendering of the \
             request id"
        );
    }

    /// Parse `response` through the real parser, and so through
    /// `on_response_headers`, into a warm response kawa; return the heap
    /// allocations the parse made and the serialised response.
    fn response_parse(ctx: &mut HttpContext, response: &[u8]) -> (usize, String) {
        use crate::test_allocations::allocations;
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let mut kawa: GenericHttpStream = kawa::Kawa::new(
            kawa::Kind::Response,
            kawa::Buffer::new(
                pool.checkout()
                    .expect("the test pool must hand out a buffer"),
            ),
        );
        let mut measured = 0;
        // The first parse warms kawa's block queue on a throwaway context.
        for context in [&mut make_context(), ctx] {
            kawa.clear();
            kawa.storage.clear();
            kawa.storage.space()[..response.len()].copy_from_slice(response);
            kawa.storage.fill(response.len());
            let before = allocations();
            kawa::h1::parse(&mut kawa, context);
            measured = allocations() - before;
            assert!(kawa.is_main_phase(), "premise: the response must parse");
        }
        kawa.prepare(&mut kawa::h1::BlockConverter);
        let buffer = kawa.storage.buffer();
        let bytes = kawa
            .out
            .iter()
            .flat_map(|block| match block {
                kawa::OutBlock::Store(store) => store.data(buffer).to_vec(),
                kawa::OutBlock::Delimiter => Vec::new(),
            })
            .collect::<Vec<u8>>();
        (
            measured,
            String::from_utf8(bytes).expect("the serialised response must be UTF-8"),
        )
    }

    /// The response's `Sozu-Id` shares the rendering the request generated
    /// for `X-Request-Id`, and still carries Sōzu's own id when the client
    /// chose its `X-Request-Id`.
    ///
    /// Nothing is left to allocate on the generated path: the standard `OK`
    /// reason is captured for the access log as a `'static` phrase
    /// (`standard_reason`).
    ///
    /// TO SEE THIS RED, either of:
    /// - in `on_response_headers`, push the correlation header as `key:
    ///   kawa::Store::from_string(self.sozu_id_header.clone())` and `val:
    ///   kawa::Store::from_string(self.id.to_string())`. Measured: `left: 2,
    ///   right: 0`.
    /// - capture every reason as a copy again, `.map(|reason|
    ///   Cow::Owned(reason.to_owned()))`. Measured: `left: 1, right: 0`.
    #[test]
    fn a_response_shares_the_request_id_rendering() {
        const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let mut kawa = warm_request_kawa(&mut pool);

        let mut generated = make_context();
        allocations_of_request_parse(&mut generated, &mut kawa, BARE_REQUEST);
        let (allocations, response) = response_parse(&mut generated, RESPONSE);
        let id = generated.id.to_string();
        assert_eq!(
            response,
            format!("HTTP/1.1 200 OK\r\nContent-Length: 0\r\nSozu-Id: {id}\r\n\r\n")
        );
        assert_eq!(
            allocations, 0,
            "the response must share the id rendering the request generated \
             and borrow its standard reason"
        );

        let mut client_chosen = make_context();
        allocations_of_request_parse(
            &mut client_chosen,
            &mut kawa,
            b"GET / HTTP/1.1\r\nHost: example.com\r\nX-Request-Id: client-chosen\r\n\r\n",
        );
        let (_, response) = response_parse(&mut client_chosen, RESPONSE);
        assert_eq!(
            response,
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nSozu-Id: {}\r\n\r\n",
                client_chosen.id
            ),
            "the correlation header carries Sōzu's id, not the client's"
        );
    }

    /// The reason of a response is captured for the access log as the
    /// `'static` phrase RFC 9110 §15 registers for its code when the backend
    /// sent exactly that phrase, and as a verbatim copy otherwise — a
    /// different phrase, a different case, an older name or an unregistered
    /// code. The forwarded bytes come from kawa's status line either way and
    /// stay byte-exact across the keep-alive requests of a connection.
    ///
    /// TO SEE THIS RED: capture every reason as a copy again, `.map(|reason|
    /// Cow::Owned(reason.to_owned()))` in `on_response_headers`. Measured:
    /// `left: 1, right: 0` on the first, standard, response.
    #[test]
    fn a_response_reason_is_borrowed_when_standard_and_copied_otherwise() {
        let cases: [(&[u8], &str, usize); 8] = [
            (b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n", "OK", 0),
            (b"HTTP/1.1 200 Fine\r\nContent-Length: 0\r\n\r\n", "Fine", 1),
            (b"HTTP/1.1 200 ok\r\nContent-Length: 0\r\n\r\n", "ok", 1),
            (
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
                "Not Found",
                0,
            ),
            (
                b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\n\r\n",
                "Payload Too Large",
                1,
            ),
            (
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
                "Service Unavailable",
                0,
            ),
            (
                b"HTTP/1.1 599 Network Connect Timeout Error\r\nContent-Length: 0\r\n\r\n",
                "Network Connect Timeout Error",
                1,
            ),
            (
                b"HTTP/1.1 302 Found\r\nContent-Length: 0\r\n\r\n",
                "Found",
                0,
            ),
        ];
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let mut kawa = warm_request_kawa(&mut pool);
        let mut ctx = make_context();

        for (response, reason, cost) in cases {
            allocations_of_request_parse(&mut ctx, &mut kawa, BARE_REQUEST);
            let (allocations, serialized) = response_parse(&mut ctx, response);
            let raw = from_utf8(response).expect("the response literal is ASCII");
            let headers_end = raw.len() - "\r\n".len();
            assert_eq!(
                serialized,
                format!("{}Sozu-Id: {}\r\n\r\n", &raw[..headers_end], ctx.id),
                "the forwarded response of {raw:?} is byte-exact"
            );
            assert_eq!(
                ctx.reason.as_deref(),
                Some(reason),
                "the access log records the reason of {raw:?} verbatim"
            );
            assert_eq!(
                allocations, cost,
                "capturing the reason of {raw:?} costs {cost} allocation(s)"
            );
            ctx.reset(Ulid::generate());
        }
    }

    /// `render_ulid` agrees with `Ulid`'s own `Display` at both ends of the
    /// 128-bit range and on generated ids.
    #[test]
    fn render_ulid_matches_the_display_rendering() {
        let ids = [
            Ulid::from(0u128),
            Ulid::from(u128::MAX),
            Ulid::from(1u128 << 127),
            Ulid::generate(),
            Ulid::generate(),
            Ulid::generate(),
        ];
        for id in ids {
            assert_eq!(
                from_utf8(&render_ulid(id)).expect("a rendered ULID is ASCII"),
                id.to_string()
            );
        }
        assert_eq!(render_ulid(Ulid::from(u128::MAX))[0], b'7');
    }

    /// Byte-exact output of the header editing, for the synthesised headers
    /// and for the chains a client supplied, on the first request of a
    /// connection and on the next ones after `reset`: each request carries
    /// its own id, never the one of the request before it.
    #[test]
    fn header_editing_output_is_byte_exact_across_keep_alive_requests() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let mut kawa = warm_request_kawa(&mut pool);
        let mut ctx = HttpContext::new(
            Ulid::generate(),
            Ulid::generate(),
            Protocol::HTTP,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080),
            Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
                54321,
            )),
            "SERVERID".to_owned(),
            "X-Edge-Id".to_owned(),
            false,
            true,
        );
        let mut seen = Vec::new();

        for request in 0..3 {
            let id = ctx.id.to_string();
            assert!(
                !seen.contains(&id),
                "request {request} of the connection reuses the id of an earlier one: {id}"
            );
            seen.push(id.clone());
            allocations_of_request_parse(&mut ctx, &mut kawa, BARE_REQUEST);
            let traceparent = synthesised_traceparent_line(&ctx);
            assert_eq!(
                serialized_request(&mut kawa),
                format!(
                    "GET / HTTP/1.1\r\nHost: example.com\r\n\
                     X-Forwarded-For: 10.0.0.1\r\n\
                     Forwarded: proto=http;for=\"10.0.0.1:54321\";by=127.0.0.1\r\n\
                     X-Real-IP: 10.0.0.1\r\n\
                     {traceparent}\
                     X-Forwarded-Port: 8080\r\n\
                     X-Forwarded-Proto: http\r\n\
                     X-Request-Id: {id}\r\n\
                     X-Edge-Id: {id}\r\n\r\n"
                ),
                "request {request} of the connection"
            );
            assert_eq!(ctx.x_request_id.as_deref(), Some(id.as_str()));
            ctx.reset(Ulid::generate());
        }

        let id = ctx.id.to_string();
        assert!(
            !seen.contains(&id),
            "the request with a client X-Request-Id reuses an earlier id: {id}"
        );
        allocations_of_request_parse(
            &mut ctx,
            &mut kawa,
            b"GET / HTTP/1.1\r\nHost: example.com\r\n\
              X-Forwarded-For: 192.0.2.7\r\n\
              Forwarded: for=192.0.2.7\r\n\
              X-Request-Id: client-chosen\r\n\r\n",
        );
        let traceparent = synthesised_traceparent_line(&ctx);
        assert_eq!(
            serialized_request(&mut kawa),
            format!(
                "GET / HTTP/1.1\r\nHost: example.com\r\n\
                 X-Forwarded-For: 192.0.2.7, 10.0.0.1\r\n\
                 Forwarded: for=192.0.2.7, proto=http;for=\"10.0.0.1:54321\";by=127.0.0.1\r\n\
                 X-Request-Id: client-chosen\r\n\
                 X-Real-IP: 10.0.0.1\r\n\
                 {traceparent}\
                 X-Forwarded-Port: 8080\r\n\
                 X-Forwarded-Proto: http\r\n\
                 X-Edge-Id: {id}\r\n\r\n"
            ),
            "client chains are extended, a client X-Request-Id is kept verbatim"
        );
        assert_eq!(ctx.x_request_id.as_deref(), Some("client-chosen"));
    }

    // ── connection-scoped forwarding hop ───────────────────────────────

    /// A context whose forwarding inputs the caller chooses, with the
    /// default correlation header name.
    fn forwarding_context(
        protocol: Protocol,
        public_address: SocketAddr,
        session_address: SocketAddr,
        send_x_real_ip: bool,
    ) -> HttpContext {
        HttpContext::new(
            Ulid::generate(),
            Ulid::generate(),
            protocol,
            public_address,
            Some(session_address),
            "SERVERID".to_owned(),
            "Sozu-Id".to_owned(),
            false,
            send_x_real_ip,
        )
    }

    /// The second and later requests of a keep-alive connection reuse the
    /// forwarding values the first one rendered: `X-Forwarded-For`,
    /// `Forwarded`, `X-Real-IP` and `X-Forwarded-Port` then cost no heap
    /// operation at all.
    ///
    /// The three left: `authority` and `path` captured for routing and the
    /// access log (2), and the one rendering of the request id (1). Under
    /// the `opentelemetry` feature, the synthesised `traceparent` adds its
    /// one copy ([`SYNTHESISED_TRACEPARENT`]).
    ///
    /// TO SEE THIS RED: in `HttpContext::forwarding_hop`, rebuild the hop on
    /// every call instead of returning the cached one whose inputs match.
    /// Measured: `left: 7, right: 3` (the scratch and the three values).
    #[test]
    fn keep_alive_requests_reuse_the_connection_forwarding_hop() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let mut kawa = warm_request_kawa(&mut pool);
        let mut ctx = forwarding_context(
            Protocol::HTTP,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 54321),
            true,
        );

        let first = allocations_of_request_parse(&mut ctx, &mut kawa, BARE_REQUEST);
        assert_eq!(
            first,
            7 + SYNTHESISED_TRACEPARENT,
            "the first request renders the connection's forwarding values once, \
             X-Real-IP sharing the X-Forwarded-For rendering"
        );
        for request in 1..4 {
            ctx.reset(Ulid::generate());
            let later = allocations_of_request_parse(&mut ctx, &mut kawa, BARE_REQUEST);
            assert_eq!(
                later,
                3 + SYNTHESISED_TRACEPARENT,
                "request {request} of the connection must reuse the forwarding values"
            );
        }
    }

    /// A client-supplied chain is extended with one exact-size copy per
    /// header, the appended hop coming from the connection's rendering.
    ///
    /// The six on a later request: `authority` and `path` (2), the
    /// `xff_chain` snapshot for the access log (1), the extended
    /// `X-Forwarded-For` and `Forwarded` (2), and the request id (1).
    ///
    /// TO SEE THIS RED: in `on_request_headers`, extend the client chains in
    /// a `Vec::with_capacity(128)` scratch shared by both again, each copied
    /// out with `kawa::Store::from_slice`. Measured: `left: 7, right: 6`, the
    /// scratch.
    #[test]
    fn a_client_chain_costs_one_exact_copy_per_extended_header() {
        const CLIENT_CHAINS: &[u8] = b"GET / HTTP/1.1\r\nHost: example.com\r\n\
              X-Forwarded-For: 192.0.2.7\r\n\
              Forwarded: for=192.0.2.7\r\n\r\n";
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let mut kawa = warm_request_kawa(&mut pool);
        let mut ctx = make_context();

        allocations_of_request_parse(&mut ctx, &mut kawa, BARE_REQUEST);
        ctx.reset(Ulid::generate());
        let later = allocations_of_request_parse(&mut ctx, &mut kawa, CLIENT_CHAINS);
        assert_eq!(
            later,
            6 + SYNTHESISED_TRACEPARENT,
            "a client chain costs one exact copy per extended header"
        );
    }

    /// The bytes forwarded on a keep-alive connection that alternates bare
    /// requests with requests carrying their own chains: a client chain is
    /// extended, never kept for the next request, and the IPv6 literals
    /// keep their RFC 7239 §6 brackets and quotes.
    #[test]
    fn a_client_chain_never_leaks_into_the_next_request() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let mut kawa = warm_request_kawa(&mut pool);
        let mut ctx = forwarding_context(
            Protocol::HTTPS,
            SocketAddr::new(
                IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)),
                443,
            ),
            SocketAddr::new(
                IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 1, 2)),
                50000,
            ),
            true,
        );
        let bare = |id: &str, traceparent: &str| {
            format!(
                "GET / HTTP/1.1\r\nHost: example.com\r\n\
                 X-Forwarded-For: 2001:db8::1:2\r\n\
                 Forwarded: proto=https;for=\"[2001:db8::1:2]:50000\";by=\"[2001:db8::1]\"\r\n\
                 X-Real-IP: 2001:db8::1:2\r\n\
                 {traceparent}\
                 X-Forwarded-Port: 443\r\n\
                 X-Forwarded-Proto: https\r\n\
                 X-Request-Id: {id}\r\n\
                 Sozu-Id: {id}\r\n\r\n"
            )
        };

        for round in 0..2 {
            let id = ctx.id.to_string();
            allocations_of_request_parse(&mut ctx, &mut kawa, BARE_REQUEST);
            let traceparent = synthesised_traceparent_line(&ctx);
            assert_eq!(
                serialized_request(&mut kawa),
                bare(&id, &traceparent),
                "bare request of round {round}"
            );
            assert_eq!(ctx.xff_chain, None);
            ctx.reset(Ulid::generate());

            let id = ctx.id.to_string();
            allocations_of_request_parse(
                &mut ctx,
                &mut kawa,
                b"GET / HTTP/1.1\r\nHost: example.com\r\n\
                  X-Forwarded-For: 192.0.2.7, 198.51.100.3\r\n\
                  Forwarded: for=192.0.2.7;proto=http\r\n\r\n",
            );
            let traceparent = synthesised_traceparent_line(&ctx);
            assert_eq!(
                serialized_request(&mut kawa),
                format!(
                    "GET / HTTP/1.1\r\nHost: example.com\r\n\
                     X-Forwarded-For: 192.0.2.7, 198.51.100.3, 2001:db8::1:2\r\n\
                     Forwarded: for=192.0.2.7;proto=http, \
                     proto=https;for=\"[2001:db8::1:2]:50000\";by=\"[2001:db8::1]\"\r\n\
                     X-Real-IP: 2001:db8::1:2\r\n\
                     {traceparent}\
                     X-Forwarded-Port: 443\r\n\
                     X-Forwarded-Proto: https\r\n\
                     X-Request-Id: {id}\r\n\
                     Sozu-Id: {id}\r\n\r\n"
                ),
                "request with client chains of round {round}"
            );
            assert_eq!(ctx.xff_chain.as_deref(), Some("192.0.2.7, 198.51.100.3"));
            ctx.reset(Ulid::generate());
        }
    }

    /// Changing an input of the forwarding values between two requests —
    /// the peer, the public address or the protocol — renders them again:
    /// a value is reused only for the inputs it was rendered from.
    #[test]
    fn a_changed_forwarding_input_renders_the_hop_again() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let mut kawa = warm_request_kawa(&mut pool);
        let mut ctx = forwarding_context(
            Protocol::HTTP,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 54321),
            true,
        );
        let expected = |ctx: &HttpContext, peer: &str, forwarded: &str, port: u16, proto: &str| {
            let id = ctx.id.to_string();
            let traceparent = synthesised_traceparent_line(ctx);
            format!(
                "GET / HTTP/1.1\r\nHost: example.com\r\n\
                 X-Forwarded-For: {peer}\r\n\
                 Forwarded: {forwarded}\r\n\
                 X-Real-IP: {peer}\r\n\
                 {traceparent}\
                 X-Forwarded-Port: {port}\r\n\
                 X-Forwarded-Proto: {proto}\r\n\
                 X-Request-Id: {id}\r\n\
                 Sozu-Id: {id}\r\n\r\n"
            )
        };

        allocations_of_request_parse(&mut ctx, &mut kawa, BARE_REQUEST);
        assert_eq!(
            serialized_request(&mut kawa),
            expected(
                &ctx,
                "10.0.0.1",
                "proto=http;for=\"10.0.0.1:54321\";by=127.0.0.1",
                8080,
                "http"
            )
        );

        ctx.reset(Ulid::generate());
        ctx.session_address = Some(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)),
            40000,
        ));
        allocations_of_request_parse(&mut ctx, &mut kawa, BARE_REQUEST);
        assert_eq!(
            serialized_request(&mut kawa),
            expected(
                &ctx,
                "10.0.0.2",
                "proto=http;for=\"10.0.0.2:40000\";by=127.0.0.1",
                8080,
                "http"
            ),
            "a new peer is rendered"
        );

        ctx.reset(Ulid::generate());
        ctx.public_address = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 8443);
        allocations_of_request_parse(&mut ctx, &mut kawa, BARE_REQUEST);
        assert_eq!(
            serialized_request(&mut kawa),
            expected(
                &ctx,
                "10.0.0.2",
                "proto=http;for=\"10.0.0.2:40000\";by=192.0.2.1",
                8443,
                "http"
            ),
            "a new public address is rendered"
        );

        // The protocol alone changes: the addresses are the ones the cached
        // values were rendered from, so only the protocol can invalidate
        // them — a stale `Forwarded` would still say `proto=http`.
        ctx.reset(Ulid::generate());
        ctx.protocol = Protocol::HTTPS;
        allocations_of_request_parse(&mut ctx, &mut kawa, BARE_REQUEST);
        assert_eq!(
            serialized_request(&mut kawa),
            expected(
                &ctx,
                "10.0.0.2",
                "proto=https;for=\"10.0.0.2:40000\";by=192.0.2.1",
                8443,
                "https"
            ),
            "a new protocol is rendered"
        );
    }
}
