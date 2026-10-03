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
use sozu_command_lib::{
    logging::{CachedTags, LogContext},
    proto::command::ForwardedHeaders,
};

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

/// Whether a `Connection` field value lists `option` (RFC 9110 §7.6.1): a
/// comma-separated list of case-insensitive tokens with optional whitespace.
pub(crate) fn has_connection_option(value: &[u8], option: &[u8]) -> bool {
    value
        .split(|byte| *byte == b',')
        .any(|listed| compare_no_case(listed.trim_ascii(), option))
}

/// Announce that the connection closes after `message` (RFC 9112 §9.6)
/// without dropping the options it already lists: removing one would turn
/// the field it nominates as hop-by-hop into an end-to-end one (RFC 9110
/// §7.6.1). Every `Connection` line is elided and their options merged into
/// one pushed line ending in a single `close`; only the `keep-alive` the
/// close contradicts, and a `close` already listed, are dropped. With
/// `refuse_upgrade`, the `upgrade` option goes too, with every `Upgrade`
/// field it nominates, so the message requests no protocol switch.
fn merge_connection_options_into_close(message: &mut GenericHttpStream, refuse_upgrade: bool) {
    let buf = message.storage.buffer();
    let mut options = Vec::new();
    for block in &mut message.blocks {
        if let kawa::Block::Header(header) = block
            && !header.is_elided()
        {
            let key = header.key.data(buf);
            if compare_no_case(key, b"connection") {
                for option in header.val.data(buf).split(|byte| *byte == b',') {
                    let option = option.trim_ascii();
                    let dropped = option.is_empty()
                        || compare_no_case(option, b"keep-alive")
                        || compare_no_case(option, b"close")
                        || (refuse_upgrade && compare_no_case(option, b"upgrade"));
                    if !dropped {
                        options.extend_from_slice(option);
                        options.extend_from_slice(b", ");
                    }
                }
                header.elide();
            } else if refuse_upgrade && compare_no_case(key, b"upgrade") {
                header.elide();
            }
        }
    }
    // A lone `close` needs no allocation: the common shutdown case of a
    // message that listed no other option.
    let val = if options.is_empty() {
        kawa::Store::Static(b"close")
    } else {
        options.extend_from_slice(b"close");
        debug_assert!(
            is_crlf_free(&options),
            "the merged Connection options must be CR/LF-free (anti-injection)"
        );
        kawa::Store::from_vec(options)
    };
    message.push_block(kawa::Block::Header(kawa::Pair {
        key: kawa::Store::Static(b"Connection"),
        val,
    }));
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

/// Whether a listener in `mode` adds the `X-Forwarded-*` family
/// (`X-Forwarded-For`, `-Proto`, `-Port`, and `-Host` on a host rewrite).
pub(crate) fn emits_x_forwarded(mode: ForwardedHeaders) -> bool {
    matches!(mode, ForwardedHeaders::Both | ForwardedHeaders::XForwarded)
}

/// Whether a listener in `mode` adds an RFC 7239 `Forwarded` element.
pub(crate) fn emits_rfc7239(mode: ForwardedHeaders) -> bool {
    matches!(mode, ForwardedHeaders::Both | ForwardedHeaders::Rfc7239)
}

/// Whether a listener in `mode` removes a client-supplied
/// `X-Forwarded-For`, `-Proto`, `-Port` or `-Host`: only `rfc7239` does, so
/// that the backend sees one forwarding family, whose last element Sōzu
/// wrote (RFC 7239 §8.1: the header fields a client sends cannot be
/// trusted).
pub(crate) fn strips_x_forwarded(mode: ForwardedHeaders) -> bool {
    mode == ForwardedHeaders::Rfc7239
}

/// Whether `key` names one of the `X-Forwarded-*` headers the editor
/// manages: the ones `strips_x_forwarded` removes.
fn is_managed_x_forwarded(key: &[u8]) -> bool {
    compare_no_case(key, b"X-Forwarded-For")
        || compare_no_case(key, b"X-Forwarded-Proto")
        || compare_no_case(key, b"X-Forwarded-Port")
        || compare_no_case(key, b"X-Forwarded-Host")
}

/// Whether `byte` is an RFC 9110 §5.6.2 `tchar`.
fn is_tchar(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

/// Whether `value` is a well-formed RFC 7239 §4 `Forwarded` field value:
///
/// ```text
/// Forwarded         = 1#forwarded-element
/// forwarded-element = [ forwarded-pair ] *( ";" [ forwarded-pair ] )
/// forwarded-pair    = token "=" value
/// value             = token / quoted-string
/// ```
///
/// with `token` and `quoted-string` from RFC 9110 §5.6.2 / §5.6.4. As RFC
/// 9110 §5.6.1 asks of a list recipient, empty list elements and optional
/// whitespace around `,` are accepted; whitespace is also tolerated around
/// `;`. An empty value is an empty list. What matters is that a parser reads
/// the value as a sequence of complete elements, so the element Sōzu appends
/// after a `, ` is read as one of its own: an unclosed quoted-string would
/// swallow it.
fn is_valid_forwarded(value: &[u8]) -> bool {
    let skip_ows = |i: &mut usize| {
        while *i < value.len() && matches!(value[*i], b' ' | b'\t') {
            *i += 1;
        }
    };
    let mut i = 0;
    loop {
        // One forwarded-element: optional pairs separated by `;`.
        loop {
            skip_ows(&mut i);
            if i < value.len() && is_tchar(value[i]) {
                // token "="
                while i < value.len() && is_tchar(value[i]) {
                    i += 1;
                }
                if value.get(i) != Some(&b'=') {
                    return false;
                }
                i += 1;
                // value = token / quoted-string
                match value.get(i) {
                    Some(b'"') => {
                        i += 1;
                        loop {
                            match value.get(i) {
                                None => return false,
                                Some(b'"') => {
                                    i += 1;
                                    break;
                                }
                                Some(b'\\') => match value.get(i + 1) {
                                    Some(&escaped)
                                        if escaped == b'\t'
                                            || escaped == b' '
                                            || (0x21..=0x7e).contains(&escaped)
                                            || escaped >= 0x80 =>
                                    {
                                        i += 2
                                    }
                                    _ => return false,
                                },
                                Some(&byte)
                                    if byte == b'\t'
                                        || byte == b' '
                                        || byte == 0x21
                                        || (0x23..=0x7e).contains(&byte)
                                        || byte >= 0x80 =>
                                {
                                    i += 1
                                }
                                Some(_) => return false,
                            }
                        }
                    }
                    Some(&byte) if is_tchar(byte) => {
                        while i < value.len() && is_tchar(value[i]) {
                            i += 1;
                        }
                    }
                    _ => return false,
                }
            }
            skip_ows(&mut i);
            if value.get(i) == Some(&b';') {
                i += 1;
            } else {
                break;
            }
        }
        match value.get(i) {
            None => return true,
            Some(b',') => i += 1,
            Some(_) => return false,
        }
    }
}

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
    /// set to false if Kawa finds a "Connection" header with a "close" value in the response,
    /// or if the response is a non-persistent HTTP/1.0 one (RFC 9112 §9.3)
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
    /// Configured cluster lifetime that owns this request's labelled metrics.
    /// Captured after routing so successive requests on one H1 keep-alive
    /// connection may belong to successive lifetimes of the same cluster id.
    pub(crate) cluster_metrics_incarnation: Option<crate::metrics::ClusterMetricsIncarnation>,
    /// The client affinity key `Router::plan_connect`
    /// (`lib/src/protocol/mux/router.rs`) derived for this request when its
    /// cluster selects with `HRW` or `MAGLEV`: the hash of the cluster's
    /// `affinity_header` / `affinity_cookie` value, else of the client source
    /// IP. `None` under every other policy. Stored next to `cluster_id` for the
    /// same reason: a replay finds the request's header blocks drained and
    /// reuses the key the first attempt derived.
    pub affinity_key: Option<u64>,
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
    /// Which forwarding header family `on_request_headers` adds to the
    /// request. Mirrors `HttpListenerConfig::forwarded_headers` /
    /// `HttpsListenerConfig::forwarded_headers`. Set from the mux `Context`
    /// at stream creation (see `Context::create_stream`); listener-scoped
    /// and never reset across keep-alive requests. Independent of
    /// `elide_x_real_ip` and `send_x_real_ip`: `X-Real-IP` belongs to
    /// neither family. `Both`, the default, is the historical behaviour.
    pub forwarded_headers: ForwardedHeaders,
    /// Most trailer fields an H1 chunked request may carry, elided ones
    /// included; one more makes `HttpContext::filter_request_trailers`
    /// reject the request. Mirrors the listener's `h2_max_header_fields`,
    /// the bound `pkawa::handle_trailer` (`lib/src/protocol/mux/pkawa.rs`)
    /// applies to an H2 trailer block, so both frontends admit the same
    /// number of trailer fields. Set from the mux `Context` at stream
    /// creation (see `Context::create_stream`); listener-scoped and never
    /// reset across keep-alive requests.
    pub max_trailer_fields: u32,
    /// Trailer fields of the current H1 request counted so far by
    /// `HttpContext::filter_request_trailers`. Request-scoped: `reset`
    /// zeroes it, so each pipelined request has its own budget.
    pub trailer_fields: u32,
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
    /// The routed cluster has backends, but none could be selected for this
    /// request — every one failing its health check or backing off after
    /// connection failures — so Sōzu answered 503 without contacting one.
    /// That is a backend outage, not a routing miss: the H2 RST caps count
    /// such a stream as routed to a backend (`routed_to_a_backend`,
    /// `lib/src/protocol/mux/h2.rs`). Request-scoped: `reset` clears it.
    pub backends_unavailable: bool,
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

/// Whether `kawa` carries a non-elided `Content-Length` whose value is not
/// `1*DIGIT` (RFC 9110 §8.6).
///
/// kawa 0.7.1 read the value with `usize::from_str`, which also accepts one
/// leading `+`, and left the field line in place with its original
/// spelling (CleverCloud/kawa#25). kawa >= 0.7.2 checks `1*DIGIT` itself
/// before the header callback (CleverCloud/kawa#26), so under the locked
/// kawa this helper never finds a non-digit value on a parsed message: it
/// is defense in depth against a kawa regression. Only non-elided lines are
/// judged, because they are the ones the H1 serializer forwards: kawa elides
/// a second line equal to the first, and every Content-Length beside a
/// Transfer-Encoding.
fn has_non_digit_content_length(kawa: &GenericHttpStream) -> bool {
    let buf = kawa.storage.buffer();
    kawa.blocks.iter().any(|block| match block {
        kawa::Block::Header(header)
            if !header.is_elided() && compare_no_case(header.key.data(buf), b"content-length") =>
        {
            let val = header.val.data(buf);
            val.is_empty() || !val.iter().all(u8::is_ascii_digit)
        }
        _ => false,
    })
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

/// Header names a request trailer section may never carry past Sōzu, in
/// lower case. This is the single list shared by both frontends:
/// `pkawa::handle_trailer` (`lib/src/protocol/mux/pkawa.rs`) drops them from
/// an H2 trailer HEADERS frame, and [`HttpContext::filter_request_trailers`]
/// from an H1 chunked trailer section. Both also drop
/// [`TRAILER_FORBIDDEN_FIELDS`] from a request trailer section.
///
/// RFC 9110 §6.5.1 forbids trailers from carrying fields that affect message
/// routing or request semantics. Each name here is client attribution that
/// Sōzu handles on the header block only: `HttpContext::on_request_headers`
/// replaces, elides or preserves `X-Real-IP` and `X-Request-Id`, and
/// synthesises, extends, removes or passes through `X-Forwarded-For`,
/// `Forwarded`, `X-Forwarded-Proto`, `X-Forwarded-Port` and
/// `X-Forwarded-Host` according to the listener's `forwarded_headers` mode
/// (`apply_request_rewrites_and_headers` in
/// `lib/src/protocol/mux/router.rs` also injects `X-Forwarded-Host` on a
/// host rewrite). A trailer copy would bypass that handling
/// and hand a forged value to any backend that merges trailers into its
/// header view (sozu-proxy/sozu#1689), so they are dropped unconditionally.
pub const TRAILER_SPOOF_VECTOR_HEADERS: [&[u8]; 7] = [
    b"x-real-ip",
    b"x-forwarded-for",
    b"forwarded",
    b"x-request-id",
    b"x-forwarded-proto",
    b"x-forwarded-port",
    b"x-forwarded-host",
];

/// Returns true if `name`, in any case, is one of
/// [`TRAILER_SPOOF_VECTOR_HEADERS`].
pub fn is_trailer_spoof_vector(name: &[u8]) -> bool {
    TRAILER_SPOOF_VECTOR_HEADERS
        .iter()
        .any(|spoofed| compare_no_case(name, spoofed))
}

/// Header names RFC 9110 §6.5.1 keeps out of a trailer section, in lower
/// case: fields "whose evaluation is necessary prior to receiving the
/// content, such as those that describe message framing, routing,
/// authentication, request modifiers, response controls, or content
/// format", plus the connection-specific fields of RFC 9110 §7.6.1.
///
/// RFC 9110 names only the categories. The concrete names are the ones its
/// predecessor, RFC 7230 §4.1.2, gave as examples for each category, read
/// on the request side:
///
/// - framing: `Content-Length`, `Transfer-Encoding`;
/// - routing: `Host`;
/// - request modifiers, "controls and conditionals in Section 5 of
///   \[RFC7231\]": `Cache-Control`, `Expect`, `Max-Forwards`, `Pragma`,
///   `Range`, `TE` (RFC 7231 §5.1) and `If-Match`, `If-None-Match`,
///   `If-Modified-Since`, `If-Unmodified-Since`, `If-Range` (RFC 7231 §5.2);
/// - authentication, "see \[RFC7235\] and \[RFC6265\]": `Authorization`,
///   `Proxy-Authorization`, `Cookie`;
/// - how to process the content: `Content-Encoding`, `Content-Type`,
///   `Content-Range`, `Trailer`.
///
/// Response control data (RFC 7231 §7.1) describes a response, and has no
/// request-side meaning to protect. The connection-specific fields
/// `Connection`, `Keep-Alive`, `Proxy-Connection` and `Upgrade` are added
/// because RFC 9110 §7.6.1 has an intermediary remove them before
/// forwarding, trailer or not, and a trailer copy never reaches the
/// header-block handling that does so. `TE` and `Transfer-Encoding` are in
/// that §7.6.1 list too.
///
/// RFC 7230 §4.1.2 says why a recipient must not act on them: "A recipient
/// MUST ignore (or consider as an error) any fields that are forbidden to be
/// sent in a trailer, since processing them as if they were present in the
/// header section might bypass external security filters."
/// [`HttpContext::filter_request_trailers`] ignores them by eliding them:
/// RFC 9112 §7.1.2 lets a recipient "selectively retain or discard the
/// received trailer fields". `pkawa::handle_trailer`
/// (`lib/src/protocol/mux/pkawa.rs`) elides them from an H2 request trailer
/// block the same way (sozu-proxy/sozu#1714), once `classify_invalid_h2_header`
/// has refused the connection-specific ones as RFC 9113 §8.2.2 requires.
pub const TRAILER_FORBIDDEN_FIELDS: [&[u8]; 25] = [
    // framing
    b"content-length",
    b"transfer-encoding",
    // routing
    b"host",
    // request modifiers: controls
    b"cache-control",
    b"expect",
    b"max-forwards",
    b"pragma",
    b"range",
    b"te",
    // request modifiers: conditionals
    b"if-match",
    b"if-none-match",
    b"if-modified-since",
    b"if-unmodified-since",
    b"if-range",
    // authentication
    b"authorization",
    b"proxy-authorization",
    b"cookie",
    // how to process the content
    b"content-encoding",
    b"content-type",
    b"content-range",
    b"trailer",
    // connection-specific
    b"connection",
    b"keep-alive",
    b"proxy-connection",
    b"upgrade",
];

/// Returns true if `name`, in any case, is one of
/// [`TRAILER_FORBIDDEN_FIELDS`].
pub fn is_trailer_forbidden_field(name: &[u8]) -> bool {
    TRAILER_FORBIDDEN_FIELDS
        .iter()
        .any(|forbidden| compare_no_case(name, forbidden))
}

/// What [`HttpContext::filter_request_trailers`] did to the trailer fields
/// the last parse appended.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TrailerFilterOutcome {
    /// Fields elided because they are in [`TRAILER_SPOOF_VECTOR_HEADERS`].
    pub spoof_vectors: usize,
    /// Fields elided because they are in [`TRAILER_FORBIDDEN_FIELDS`].
    pub forbidden: usize,
    /// The trailer section went over `HttpContext::max_trailer_fields`, and
    /// the request was marked in error.
    pub over_limit: bool,
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
            cluster_metrics_incarnation: None,
            affinity_key: None,

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
            forwarded_headers: ForwardedHeaders::Both,
            max_trailer_fields: crate::protocol::mux::H2FloodConfig::default().max_header_fields(),
            trailer_fields: 0,
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
            backends_unavailable: false,
        }
    }

    pub(crate) fn set_cluster_metrics_incarnation(
        &mut self,
        incarnation: Option<crate::metrics::ClusterMetricsIncarnation>,
    ) {
        self.cluster_metrics_incarnation = incarnation;
    }

    pub(crate) fn cluster_metrics_incarnation(
        &self,
    ) -> Option<crate::metrics::ClusterMetricsIncarnation> {
        self.cluster_metrics_incarnation
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
        // WHICH CLAUSE ACTUALLY FIRES, against the kawa this workspace pins (`^0.7.2`,
        // locked at 0.7.2; its Transfer-Encoding handling is 0.7.1's). Read from kawa's
        // `process_headers` (its `src/protocol/h1/parser/mod.rs`), not measured here: kawa resolves the
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

        // RFC 9110 §8.6: `Content-Length = 1*DIGIT`, and a sender MUST NOT
        // forward a message whose Content-Length does not match it. kawa
        // 0.7.1 framed `Content-Length: +5` as 5 bytes and left the line to be
        // forwarded as sent: a backend that refuses or re-reads `+5` takes the
        // body for the start of the next request — never routed, never
        // Basic-auth checked (CWE-444, sozu#1652). RFC 9112 §6.3 rule 5 makes
        // it an unrecoverable framing error: 400. kawa >= 0.7.2 refuses every
        // non-digit value before calling back (CleverCloud/kawa#26), so this
        // clause no longer fires on a request kawa accepted: it is defense in
        // depth against a kawa regression — keep it, and do not describe it
        // as closing a live hole. A list of equal values
        // (`5, 5`), which RFC 9110 §8.6 lets a recipient either collapse or
        // reject, is rejected — kawa already refuses it. `005` is `1*DIGIT`
        // and is forwarded as sent, as the H2 path forwards it. Mirrors
        // `RejectReason::DuplicateCl` on the H2 side
        // (`pkawa::write_regular_header`), which is why this cannot fire for
        // an H2 request.
        if has_non_digit_content_length(request) {
            incr!(names::http::FRONTEND_CONTENT_LENGTH_INVALID);
            warn!(
                "{} rejecting request: Content-Length is not 1*DIGIT (possible request smuggling)",
                self.log_context()
            );
            request
                .parsing_phase
                .error("Content-Length is not 1*DIGIT".into());
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

        // A request with neither Content-Length nor Transfer-Encoding has no
        // body, whatever its method or version (RFC 9112 §6.3 rule 7);
        // close-delimited framing (rule 8) is for responses only. kawa 0.7.1
        // did not make that distinction: `kawa::h1::parse` mapped
        // `BodySize::Empty` to `ParsingPhase::Body` for both kinds, and that
        // arm takes every byte left in the buffer. A request pipelined behind
        // this one would then be forwarded raw as its "body" to this
        // request's backend: never routed, never Basic-auth checked, without
        // `Sozu-Id` or `X-Forwarded-*` (CWE-444, sozu#1650). End it here
        // instead, so the flags block kawa pushes right after this callback
        // carries `end_stream`, and the next request stays unparsed for its
        // own turn.
        //
        // kawa >= 0.7.2 applies rule 7 itself (CleverCloud/kawa#27): it maps
        // `BodySize::Empty` to `ParsingPhase::Terminated` for a request before
        // calling back, so this branch no longer fires on the H1 path. It is
        // defense in depth against a kawa regression — keep it.
        //
        // The `ParsingPhase::Body` conjunct is what confines this to kawa's
        // H1 parser, which set that phase before calling back (kawa 0.7.1).
        // `pkawa::handle_header` calls this same callback for an H2 request
        // while its `body_size` is still `Empty` and its phase still the
        // initial one, then frames a request whose DATA follows as chunked
        // unless the callback terminated it: `Empty` alone would drop that
        // body. Pinned by `an_h2_request_is_left_for_pkawa_to_frame`.
        if request.parsing_phase == kawa::ParsingPhase::Body
            && request.body_size == kawa::BodySize::Empty
        {
            request.parsing_phase = kawa::ParsingPhase::Terminated;
        }

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
        // - set keep_alive_frontend to false if Connection lists "close"
        //   (RFC 9110 §7.6.1, RFC 9112 §9.6), as on the response side,
        //   shutting down or not
        // - update value of X-Forwarded-Proto
        // - update value of X-Forwarded-Port
        // - store X-Forwarded-For
        // - store Forwarded
        // - store User-Agent
        let forwarded_headers = self.forwarded_headers;
        let emits_x_forwarded = emits_x_forwarded(forwarded_headers);
        let emits_rfc7239 = emits_rfc7239(forwarded_headers);
        let strips_x_forwarded = strips_x_forwarded(forwarded_headers);
        // A mode that strips the client X-Forwarded-* family never adds it.
        debug_assert!(
            !(strips_x_forwarded && emits_x_forwarded),
            "a mode that strips X-Forwarded-* must not emit it"
        );
        let mut x_for = None;
        let mut forwarded = None;
        let mut has_x_port = false;
        let mut has_x_proto = false;
        let mut has_x_request_id = false;
        #[cfg(feature = "opentelemetry")]
        let mut traceparent: Option<&mut kawa::Pair> = None;
        #[cfg(feature = "opentelemetry")]
        let mut tracestate: Option<&mut kawa::Pair> = None;
        for block in &mut request.blocks {
            match block {
                kawa::Block::Header(header) if !header.is_elided() => {
                    let key = header.key.data(buf);
                    if compare_no_case(key, b"connection") {
                        let val = header.val.data(buf);
                        self.keep_alive_frontend &= !has_connection_option(val, b"close");
                    } else if strips_x_forwarded && is_managed_x_forwarded(key) {
                        // `rfc7239`: a client X-Forwarded-For, -Proto,
                        // -Port or -Host is removed, not trusted. The
                        // X-Forwarded-For value is still recorded for the
                        // access log, which keeps what the client attested.
                        if compare_no_case(key, b"X-Forwarded-For") {
                            self.xff_chain = header
                                .val
                                .data_opt(buf)
                                .and_then(|data| from_utf8(data).ok())
                                .map(ToOwned::to_owned);
                        }
                        header.elide();
                        // Post: the client value never reaches the backend.
                        debug_assert!(
                            header.is_elided(),
                            "a client X-Forwarded-* header must be elided in rfc7239 mode"
                        );
                    } else if !emits_x_forwarded
                        && (compare_no_case(key, b"X-Forwarded-Proto")
                            || compare_no_case(key, b"X-Forwarded-Port"))
                    {
                        // `none`: Sōzu takes no stance on a client
                        // X-Forwarded-Proto or -Port — it neither trusts nor
                        // replaces them, so they pass through as sent.
                        debug_assert!(
                            forwarded_headers == ForwardedHeaders::None,
                            "only `none` passes a client X-Forwarded-Proto/-Port through"
                        );
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
                        // also set) is appended after this loop. Trailer
                        // fields bypass this callback; they are covered by
                        // `TRAILER_SPOOF_VECTOR_HEADERS`, dropped from H2
                        // trailer HEADERS frames by `pkawa::handle_trailer`
                        // and from H1 chunked trailers by
                        // `HttpContext::filter_request_trailers`.
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
                        if emits_rfc7239
                            && !is_valid_forwarded(header.val.data_opt(buf).unwrap_or_default())
                        {
                            // A malformed client chain cannot be extended:
                            // an unclosed quoted-string would swallow the
                            // element appended after it. RFC 7239 §4 lets a
                            // proxy remove the field; the element Sōzu adds
                            // then goes to an earlier well-formed line or a
                            // synthesised one, so it is always parseable
                            // and last. The counter is the operator-visible
                            // trace of the removal; the log stays at debug.
                            incr!(names::http::FORWARDED_MALFORMED_ELIDED);
                            debug!(
                                "{} eliding a malformed client Forwarded header",
                                self.log_context()
                            );
                            header.elide();
                            debug_assert!(
                                header.is_elided(),
                                "a malformed client Forwarded must be elided"
                            );
                        } else {
                            forwarded = Some(header);
                        }
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
            // one exact-size copy (`extended_chain`), never kept. A mode
            // that does not emit a family leaves its client chain as sent
            // (`rfc7239` elided the X-Forwarded-For lines in the walk above).
            if emits_x_forwarded && let Some(header) = x_for {
                header.val = extended_chain(header.val.data(buf), &x_forwarded_for_hop);
            }
            if emits_rfc7239 && let Some(header) = &mut forwarded {
                header.val = extended_chain(header.val.data(buf), &forwarded_hop);
            }

            if emits_x_forwarded && !has_x_for {
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
            if emits_rfc7239 && !has_forwarded {
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
            // Forwarded synthesis behaviour above. `X-Real-IP` belongs to
            // neither forwarding family, so `forwarded_headers` never gates
            // it: the hop is rendered in every mode. It shares the
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

        if emits_x_forwarded && !has_x_port {
            request.push_block(kawa::Block::Header(kawa::Pair {
                key: kawa::Store::Static(b"X-Forwarded-Port"),
                val: kawa::Store::Shared(port_hop, 0),
            }));
        }
        if emits_x_forwarded && !has_x_proto {
            request.push_block(kawa::Block::Header(kawa::Pair {
                key: kawa::Store::Static(b"X-Forwarded-Proto"),
                val: kawa::Store::Static(proto.as_bytes()),
            }));
        }
        // A shutting-down Sōzu closes the client connection after this
        // response (`ConnectionH1::writable`, `lib/src/protocol/mux/h1.rs`),
        // so the request announces the close too, keeping the other options
        // the client listed. It refuses a protocol upgrade: `writable` tests
        // `closing` before it handles a 101, so an upgrade would switch the
        // client to a connection that closes at once. Dropping the `upgrade`
        // option with its `Upgrade` field lets the backend answer in
        // HTTP/1.1, which it always may (RFC 9110 §7.8), and the client
        // retry the upgrade on a new connection.
        if self.closing {
            merge_connection_options_into_close(request, true);
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
        // Postcondition: `rfc7239` forwards no X-Forwarded-* header the
        // editor manages, client-supplied or synthesised.
        debug_assert!(
            !strips_x_forwarded
                || request.blocks.iter().all(|block| match block {
                    kawa::Block::Header(header) if !header.is_elided() =>
                        !is_managed_x_forwarded(header.key.data(request.storage.buffer())),
                    _ => true,
                }),
            "rfc7239 mode must forward no X-Forwarded-For/-Proto/-Port/-Host"
        );
    }

    /// Callback for response:
    ///
    /// - edit headers (connection, set-cookie, sozu-id)
    /// - forward an HTTP/1.0 response as HTTP/1.1 (RFC 9110 §6.2)
    /// - save information:
    ///   - status code
    ///   - reason
    ///   - back keep-alive
    fn on_response_headers(&mut self, response: &mut GenericHttpStream) {
        // Like the request path, response editing only adds or elides — pin
        // the entry count so the postcondition can assert "blocks only grow".
        let blocks_at_entry = response.blocks.len();

        // The response mirror of the request's Content-Length clause: a
        // proxy that receives an invalid Content-Length MUST discard the
        // response and answer 502 (RFC 9112 §6.3 rule 5). Failing the parse
        // here does that: `ConnectionH1::readable` ends the backend stream,
        // and `shared::end_stream_decision` answers the client 502 since no
        // byte of the response was consumed. Checked before the 204/304/1xx
        // override of `body_size`, which leaves the field line in place.
        // kawa >= 0.7.2 already fails such a response before calling back
        // (CleverCloud/kawa#26) — the same 502 through the same path — so
        // this clause is defense in depth against a kawa regression.
        if has_non_digit_content_length(response) {
            incr!(names::http::BACKEND_CONTENT_LENGTH_INVALID);
            warn!(
                "{} rejecting response: Content-Length is not 1*DIGIT",
                self.log_context()
            );
            response
                .parsing_phase
                .error("Content-Length is not 1*DIGIT".into());
            return;
        }

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

        let (is_http10, is_interim) = match &response.detached.status_line {
            kawa::StatusLine::Response { version, code, .. } => (
                matches!(version, kawa::Version::V10),
                (100..200).contains(code),
            ),
            _ => (false, false),
        };

        // If found:
        // - set keep_alive_backend to false if Connection lists "close",
        //   shutting down or not
        // - note a "close" and a "keep-alive" option, and Transfer-Encoding
        //   (HTTP/1.0)
        let mut announces_close = false;
        let mut asks_keep_alive = false;
        let mut has_transfer_encoding = false;
        for block in &mut response.blocks {
            match block {
                kawa::Block::Header(header) if !header.is_elided() => {
                    let key = header.key.data(buf);
                    if compare_no_case(key, b"connection") {
                        let val = header.val.data(buf);
                        let is_close = has_connection_option(val, b"close");
                        announces_close |= is_close;
                        self.keep_alive_backend &= !is_close;
                        asks_keep_alive |= has_connection_option(val, b"keep-alive");
                    } else if compare_no_case(key, b"transfer-encoding") {
                        has_transfer_encoding = true;
                    }
                }
                _ => {}
            }
        }

        // RFC 9110 §6.2: Sōzu answers in its own version, HTTP/1.1, whatever
        // the backend spoke. kawa serialises `Version::V10` as `HTTP/1.0` for
        // an H1 client (the H2 converter ignores the version), so only an
        // HTTP/1.0 response needs rewriting.
        //
        // HTTP/1.0 is not persistent unless the response asks to be with a
        // `keep-alive` connection option (RFC 9112 §9.3); a close-delimited
        // body (RFC 9112 §6.3 rule 8) ends with the close whatever it asked,
        // and `Transfer-Encoding` in an HTTP/1.0 message is faulty framing
        // that closes the connection after it (RFC 9112 §6.1). Forwarded as
        // HTTP/1.1 the client would read it as persistent, so a
        // non-persistent one clears `keep_alive_backend`, which lets the
        // backend EOF end a close-delimited body
        // (`ConnectionH1::terminate_close_delimited`,
        // `lib/src/protocol/mux/h1.rs`) and closes an H1 client connection
        // after the response (`ConnectionH1::writable`), and announces that
        // close (RFC 9112 §9.6).
        //
        // A shutting-down Sōzu closes the client connection after the
        // response too (`ConnectionH1::writable` tests `closing` first), in
        // HTTP/1.1 as in HTTP/1.0, and announces it the same way. Either
        // close merges the response's `Connection` options into one line
        // ending in `close`, keeping every field they nominate hop-by-hop
        // (RFC 9110 §7.6.1) and dropping only the `keep-alive` the close
        // contradicts. The H2 converter drops that line with every
        // connection-specific header.
        //
        // An interim 1xx says nothing about the connection: its persistence
        // belongs to the final response, which runs this callback again, and
        // a 101 must keep its `Connection: Upgrade` (RFC 9110 §7.8).
        if is_http10 && !is_interim {
            let close_delimited = response.parsing_phase == kawa::ParsingPhase::Body
                && response.body_size == kawa::BodySize::Empty;
            if !asks_keep_alive || close_delimited || has_transfer_encoding {
                self.keep_alive_backend = false;
            }
        }
        if !is_interim && (self.closing || (is_http10 && !self.keep_alive_backend)) {
            merge_connection_options_into_close(response, false);
            announces_close = true;
        }
        if is_http10 {
            if let kawa::StatusLine::Response { version, .. } = &mut response.detached.status_line {
                *version = kawa::Version::V11;
            }
            // Post: the backend's version never reaches the client, and a
            // response the backend will not keep announces the close.
            debug_assert!(
                matches!(
                    response.detached.status_line,
                    kawa::StatusLine::Response {
                        version: kawa::Version::V11,
                        ..
                    }
                ),
                "an HTTP/1.0 response must be forwarded as HTTP/1.1"
            );
            debug_assert!(
                self.keep_alive_backend || announces_close,
                "a non-persistent HTTP/1.0 response must carry Connection: close"
            );
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

    /// Police the trailer fields that the last `kawa::h1::parse` appended
    /// to the trailer section of a chunked H1 request.
    ///
    /// - A field in [`TRAILER_SPOOF_VECTOR_HEADERS`] (client attribution,
    ///   sozu-proxy/sozu#1689) or in [`TRAILER_FORBIDDEN_FIELDS`] (framing,
    ///   routing, request modifiers, authentication, content processing and
    ///   connection-specific fields, RFC 9110 §6.5.1, sozu-proxy/sozu#1701)
    ///   is elided, and counted in `http.trailer.spoof_vector_elided` or
    ///   `http.trailer.forbidden_field_elided`.
    /// - Every field, elided or not, counts against `max_trailer_fields`,
    ///   as `pkawa::handle_trailer` (`lib/src/protocol/mux/pkawa.rs`) counts
    ///   an H2 trailer block. The field that goes over the limit marks the
    ///   request in error, so the caller's existing parse-error path answers
    ///   400 (or cuts a response that already started), and
    ///   `http.trailer.field_limit_exceeded` is incremented.
    ///
    /// kawa's H1 parser has no trailer callback: its `ParsingPhase::Trailers`
    /// arm pushes each trailer field as a `Block::Header` after the `Flags`
    /// block that carries `end_body` (the last chunk), then closes the
    /// section with a `Flags` block carrying `end_header` and `end_stream`.
    /// Call this after each `kawa::h1::parse` of a frontend request, with
    /// `first_new_block` set to `kawa.blocks.len()` read just before that
    /// parse. It walks back from the end of the queue over the closing block
    /// and the trailer fields, and stops at the first other block (the
    /// `end_body` marker) or at `first_new_block`. It never reaches the
    /// header block, which ends with its own `end_header` marker before the
    /// chunks.
    ///
    /// Stopping at `first_new_block` keeps the total walk linear in the
    /// number of trailer fields: every field queued before that parse was
    /// already examined, and counted in `trailer_fields`, by the call that
    /// followed the parse which queued it, so a client that trickles one
    /// trailer line per segment while the backend is not writable does not
    /// make each call re-walk the whole section. Parsing only appends
    /// blocks, and `prepare` only drains them from the front between parses,
    /// so the fields past `first_new_block` are exactly the new ones even
    /// when earlier trailer fields were already forwarded. That is also why
    /// the count lives in `trailer_fields` rather than in the queue.
    ///
    /// An elided field is skipped by kawa's H1 converter and by the H2 one,
    /// so the last chunk and the closing empty line are still written: the
    /// chunk framing stays valid even when every trailer field is dropped.
    /// A request that is not chunked, or has not reached its trailer
    /// section, returns before touching a block, so the walk costs nothing
    /// and allocates nothing on the common path.
    ///
    /// Only requests are filtered: a response trailer travels towards the
    /// client, which does not take client attribution from it.
    pub fn filter_request_trailers(
        &mut self,
        kawa: &mut GenericHttpStream,
        first_new_block: usize,
    ) -> TrailerFilterOutcome {
        let mut outcome = TrailerFilterOutcome::default();
        if !matches!(kawa.kind, kawa::Kind::Request)
            || kawa.body_size != kawa::BodySize::Chunked
            || !matches!(
                kawa.parsing_phase,
                kawa::ParsingPhase::Trailers | kawa::ParsingPhase::Terminated
            )
        {
            return outcome;
        }
        // Pre: the caller read the length before a parse, which only appends.
        debug_assert!(
            first_new_block <= kawa.blocks.len(),
            "first_new_block must be a block count read before the parse"
        );
        // Pre: the count never passes the limit, the field that would have
        // passed it turned the request into an error, which stops parsing.
        debug_assert!(
            self.trailer_fields <= self.max_trailer_fields,
            "an admitted trailer section fits max_trailer_fields"
        );
        let first_new_block = first_new_block.min(kawa.blocks.len());
        let buf = kawa.storage.buffer();
        let mut fields: u32 = 0;
        for block in kawa.blocks.range_mut(first_new_block..).rev() {
            match block {
                kawa::Block::Flags(kawa::Flags {
                    end_body: false,
                    end_chunk: false,
                    end_header: true,
                    end_stream: true,
                }) => {}
                kawa::Block::Header(pair) => {
                    fields = fields.saturating_add(1);
                    if pair.is_elided() {
                        continue;
                    }
                    let key = pair.key.data(buf);
                    if is_trailer_spoof_vector(key) {
                        pair.elide();
                        outcome.spoof_vectors += 1;
                        incr!(names::http::TRAILER_SPOOF_VECTOR_ELIDED);
                    } else if is_trailer_forbidden_field(key) {
                        pair.elide();
                        outcome.forbidden += 1;
                        incr!(names::http::TRAILER_FORBIDDEN_FIELD_ELIDED);
                    }
                    // Post: no spoof-vector or forbidden field survives.
                    debug_assert!(
                        pair.is_elided()
                            || !(is_trailer_spoof_vector(pair.key.data(buf))
                                || is_trailer_forbidden_field(pair.key.data(buf))),
                        "a spoof-vector or forbidden trailer field must be elided"
                    );
                }
                _ => break,
            }
        }
        self.trailer_fields = self.trailer_fields.saturating_add(fields);
        if self.trailer_fields > self.max_trailer_fields {
            outcome.over_limit = true;
            incr!(names::http::TRAILER_FIELD_LIMIT_EXCEEDED);
            kawa.parsing_phase
                .error("trailer section exceeds max_trailer_fields".into());
        }
        // Post: only blocks appended by the last parse are touched, and a
        // section over the limit never leaves the request forwardable.
        debug_assert!(
            outcome.spoof_vectors + outcome.forbidden <= kawa.blocks.len() - first_new_block,
            "only blocks appended by the last parse are elided"
        );
        debug_assert!(
            !outcome.over_limit || kawa.is_error(),
            "a trailer section over the limit marks the request in error"
        );
        outcome
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
        let forwarded_headers_before = self.forwarded_headers;
        let max_trailer_fields_before = self.max_trailer_fields;
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
        // The sticky-session answer is request-scoped (#1822):
        // `backend_from_request` writes it only when the request's frontend
        // sticks, and `on_response_headers` answers with a `Set-Cookie` for
        // any value left here, so keeping it would hand a request to a
        // frontend that does not stick the previous request's cookie.
        self.sticky_session = None;
        self.affinity_key = None;
        self.cluster_metrics_incarnation = None;
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
        self.trailer_fields = 0;
        self.backends_unavailable = false;
        // Note: tls_server_name, tls_version, tls_cipher, tls_alpn,
        // strict_sni_binding, elide_x_real_ip, send_x_real_ip,
        // forwarded_headers and max_trailer_fields are
        // connection-scoped — set once at handshake completion and reused
        // across every keep-alive request, so reset() intentionally leaves
        // them in place. So is forwarding_hop, rendered from connection-
        // scoped inputs only.

        // Post: request-scoped state is fully cleared (a stale value here
        // would leak across pipelined requests on the same connection).
        debug_assert!(
            self.method.is_none()
                && self.sticky_session.is_none()
                && self.authority.is_none()
                && self.path.is_none()
                && self.status.is_none()
                && self.x_request_id.is_none()
                && self.headers_response.is_empty()
                && self.trailer_fields == 0
                && !self.backends_unavailable,
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
                && self.forwarded_headers == forwarded_headers_before
                && self.max_trailer_fields == max_trailer_fields_before
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
        // `sticky_session` is request-scoped, not connection state (#1822):
        // `reset_clears_the_sticky_session_answer_of_the_previous_request`
        // pins that it is cleared.

        let original_id = ctx.id;
        let original_protocol = ctx.protocol;
        let original_public_address = ctx.public_address;

        let next_id = Ulid::generate();
        ctx.reset(next_id);

        // Connection-level state is preserved
        assert!(ctx.closing);
        assert_eq!(ctx.cluster_id.as_deref(), Some("cluster-1"));
        assert_eq!(ctx.backend_id.as_deref(), Some("backend-1"));
        // The request id is request-scoped: a keep-alive connection's next
        // request must not inherit the previous one's.
        assert_ne!(ctx.id, original_id);
        assert_eq!(ctx.id, next_id);
        assert_eq!(ctx.protocol, original_protocol);
        assert_eq!(ctx.public_address, original_public_address);
    }

    /// The response a context's editor builds for `bytes`, serialised.
    fn edited_response(ctx: &mut HttpContext, bytes: &[u8]) -> String {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let mut kawa: GenericHttpStream = kawa::Kawa::new(
            kawa::Kind::Response,
            kawa::Buffer::new(
                pool.checkout()
                    .expect("the test pool must hand out a buffer"),
            ),
        );
        kawa.storage.space()[..bytes.len()].copy_from_slice(bytes);
        kawa.storage.fill(bytes.len());
        kawa::h1::parse(&mut kawa, ctx);
        assert!(!kawa.is_error(), "premise: the response must parse");
        serialized_request(&mut kawa)
    }

    /// #1822: the sticky-session cookie `backend_from_request` chose for one
    /// request of a keep-alive connection is not the next request's. That
    /// call writes `sticky_session` only when the request's frontend sticks,
    /// so a request to a frontend that does not stick, following one to a
    /// frontend that does, used to answer with the previous request's
    /// `Set-Cookie`, naming a backend of another cluster that frontend
    /// never asked for and cannot use.
    ///
    /// TO SEE THIS RED: drop `self.sticky_session = None;` from
    /// `HttpContext::reset`.
    #[test]
    fn reset_clears_the_sticky_session_answer_of_the_previous_request() {
        const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nb";
        let mut ctx = make_context();
        // Request A, to a sticky frontend whose client sent no cookie: the
        // router answers with the chosen backend's sticky id.
        ctx.sticky_session = Some("sticky-a".to_owned());
        let first = edited_response(&mut ctx, RESPONSE);
        assert!(
            first.contains("Set-Cookie: SERVERID=sticky-a; Path=/\r\n"),
            "premise: the sticky request is answered with its cookie, got {first:?}"
        );

        // Request B, on the same connection, to a frontend that does not
        // stick: the router leaves `sticky_session` as `reset` left it.
        ctx.reset(Ulid::generate());
        assert_eq!(
            ctx.sticky_session, None,
            "reset must clear the previous request's sticky-session answer"
        );
        let second = edited_response(&mut ctx, RESPONSE);
        assert!(
            !second.to_ascii_lowercase().contains("set-cookie"),
            "a request to a frontend that does not stick must not carry the \
             previous request's cookie, got {second:?}"
        );
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

    // ── Content-Length field value: 1*DIGIT ────────────────────────────

    /// Parse `bytes` as a `kind` message through the real parser, and so
    /// through `on_request_headers` / `on_response_headers`, on a fresh
    /// context — the way `ConnectionH1::readable` drives it.
    fn parse_framed(
        pool: &mut crate::pool::Pool,
        kind: kawa::Kind,
        bytes: &[u8],
    ) -> GenericHttpStream {
        let mut kawa: GenericHttpStream = kawa::Kawa::new(
            kind,
            kawa::Buffer::new(
                pool.checkout()
                    .expect("the test pool must hand out a buffer"),
            ),
        );
        kawa.storage.space()[..bytes.len()].copy_from_slice(bytes);
        kawa.storage.fill(bytes.len());
        kawa::h1::parse(&mut kawa, &mut make_context());
        kawa
    }

    /// RFC 9110 §8.6: `Content-Length = 1*DIGIT`, and a sender MUST NOT
    /// forward a message whose Content-Length does not match that grammar.
    /// kawa 0.7.1 read the value with `usize::from_str`, which also accepts
    /// one leading `+`: `Content-Length: +5` framed a 5-byte body and was
    /// forwarded verbatim, handing any backend that refuses or re-reads
    /// that spelling a body it would take for the next request (CWE-444).
    ///
    /// kawa >= 0.7.2 refuses every row here itself (`Invalid Content-Length
    /// field value`, CleverCloud/kawa#26) before `on_request_headers` runs;
    /// under kawa 0.7.1 the `+` rows reached it. The test pins the outcome —
    /// the request is refused — whichever layer refuses it, and that the
    /// Sōzu clause neither relaxes nor depends on kawa's refusal.
    ///
    /// TO SEE THIS RED: deleting the Content-Length clause of
    /// `on_request_headers` alone no longer turns it red, since kawa >= 0.7.2
    /// refuses every row first. Either see the clause's own predicate fail in
    /// `the_content_length_helper_judges_every_non_digit_value`, or pin
    /// kawa 0.7.1 (`kawa = { version = "=0.7.1", default-features = false }`
    /// in the root `Cargo.toml`, then `cargo update -p kawa --precise 0.7.1`)
    /// and delete the clause: the `plus`, `plus-zero` and
    /// `plus-then-canonical` rows then parse clean. Under that pin the
    /// `canonical-then-plus` assertion fails even with the clause present:
    /// that refusal is kawa's alone, since kawa 0.7.1 elided the `+5` line.
    #[test]
    fn a_request_content_length_that_is_not_only_digits_is_rejected() {
        let cases: [(&str, &[u8]); 13] = [
            ("plus", b"+5"),
            ("plus-zero", b"+0"),
            ("minus-zero", b"-0"),
            ("plus-alone", b"+"),
            ("list-of-equal-values", b"5, 5"),
            ("list-without-space", b"5,5"),
            ("inner-space", b"5 5"),
            ("hexadecimal", b"0x5"),
            ("decimal-point", b"5.0"),
            ("overflow", b"99999999999999999999999"),
            ("arabic-indic-digit", "\u{0665}".as_bytes()),
            ("fullwidth-digit", "\u{FF15}".as_bytes()),
            ("empty", b""),
        ];
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        for (label, value) in cases {
            let request = [
                &b"POST /api HTTP/1.1\r\nHost: example.com\r\nContent-Length: "[..],
                value,
                b"\r\n\r\nHello",
            ]
            .concat();
            let kawa = parse_framed(&mut pool, kawa::Kind::Request, &request);
            assert!(
                kawa.is_error(),
                "{label}: a Content-Length that is not 1*DIGIT must be refused, got {:?} with {:?}",
                kawa.parsing_phase,
                kawa.body_size
            );
        }

        // kawa elides a second Content-Length equal to the first, so the
        // FIRST line is the one that survives to be forwarded — and so the
        // one the guard judges.
        let request = b"POST /api HTTP/1.1\r\nHost: example.com\r\nContent-Length: +5\r\nContent-Length: 5\r\n\r\nHello";
        let kawa = parse_framed(&mut pool, kawa::Kind::Request, request);
        assert!(
            kawa.is_error(),
            "plus-then-canonical: the surviving `+5` must be refused, got {:?}",
            kawa.parsing_phase
        );
        // Hand the pool's only buffer back for the next parse.
        drop(kawa);

        // The mirror order. kawa 0.7.1 parsed the second line as 5, elided
        // it as equal to the first and forwarded only `5`. kawa >= 0.7.2
        // judges every line against `1*DIGIT` before comparing it, so the
        // message carries an invalid Content-Length field line and is
        // refused (RFC 9112 §6.3 rule 5) — the same verdict as the order
        // above, no longer one that depends on which line comes first.
        let request = b"POST /api HTTP/1.1\r\nHost: example.com\r\nContent-Length: 5\r\nContent-Length: +5\r\n\r\nHello";
        let kawa = parse_framed(&mut pool, kawa::Kind::Request, request);
        assert!(
            kawa.is_error(),
            "canonical-then-plus: a `+5` field line must be refused, got {:?}",
            kawa.parsing_phase
        );
    }

    /// `has_non_digit_content_length` on its own, over hand-built header
    /// blocks. kawa refuses `-0`, `0x5`, `5 5` and the empty value before
    /// `on_request_headers` runs — and, since 0.7.2, `+5` too — so the
    /// parse-driven tests above prove kawa's refusal, not the helper's: a
    /// helper narrowed to a leading `+`, or not called at all, would still
    /// pass them. This test is the one that pins the helper's own `1*DIGIT`
    /// contract, and that an elided line is not judged.
    ///
    /// TO SEE THIS RED, narrow the helper's predicate to
    /// `val.first() == Some(&b'+')`: the `-0`, `0x5`, `5 5` and empty rows
    /// then read `false`.
    #[test]
    fn the_content_length_helper_judges_every_non_digit_value() {
        fn content_length(val: &'static [u8], elided: bool) -> GenericHttpStream {
            let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
            let mut kawa: GenericHttpStream = kawa::Kawa::new(
                kawa::Kind::Request,
                kawa::Buffer::new(
                    pool.checkout()
                        .expect("the test pool must hand out a buffer"),
                ),
            );
            let mut pair = kawa::Pair {
                key: kawa::Store::Static(b"Content-Length"),
                val: kawa::Store::Static(val),
            };
            if elided {
                pair.elide();
            }
            kawa.push_block(kawa::Block::Header(pair));
            kawa
        }
        let cases: [(&str, &'static [u8], bool, bool); 7] = [
            ("minus-zero", b"-0", false, true),
            ("hexadecimal", b"0x5", false, true),
            ("inner-space", b"5 5", false, true),
            ("empty", b"", false, true),
            ("leading-zeros", b"005", false, false),
            ("zero", b"0", false, false),
            ("elided-plus", b"+5", true, false),
        ];
        for (label, val, elided, expected) in cases {
            assert_eq!(
                has_non_digit_content_length(&content_length(val, elided)),
                expected,
                "{label}"
            );
        }
    }

    /// Non-regression for the Content-Length clause of `on_request_headers`:
    /// it judges only the Content-Length that is forwarded.
    ///
    /// - `005` matches `1*DIGIT`: it is legal and forwarded as sent, exactly
    ///   as the H2 path (`pkawa::write_regular_header`) forwards it.
    /// - `+5` beside `Transfer-Encoding: chunked`: kawa elided the
    ///   Content-Length (RFC 9110 §6.3 — Transfer-Encoding overrides it)
    ///   without reading its value, so nothing non-canonical is forwarded.
    ///
    /// `5` then `+5` is no longer a row here: kawa 0.7.1 elided the second
    /// line as equal to the first and forwarded `5`, while kawa >= 0.7.2
    /// refuses the `+5` line itself, so that request is now pinned as
    /// refused in `a_request_content_length_that_is_not_only_digits_is_rejected`.
    #[test]
    fn a_forwarded_content_length_is_only_digits() {
        let cases: [(&str, &[u8], &str); 3] = [
            (
                "canonical",
                b"POST /api HTTP/1.1\r\nHost: example.com\r\nContent-Length: 5\r\n\r\nHello",
                "Content-Length: 5\r\n",
            ),
            (
                "leading-zeros",
                b"POST /api HTTP/1.1\r\nHost: example.com\r\nContent-Length: 005\r\n\r\nHello",
                "Content-Length: 005\r\n",
            ),
            (
                "plus-beside-chunked",
                b"POST /api HTTP/1.1\r\nHost: example.com\r\nContent-Length: +5\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nHello\r\n0\r\n\r\n",
                "Transfer-Encoding: chunked\r\n",
            ),
        ];
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        for (label, request, expected) in cases {
            let mut kawa = parse_framed(&mut pool, kawa::Kind::Request, request);
            assert!(
                !kawa.is_error(),
                "{label}: must be accepted, got {:?}",
                kawa.parsing_phase
            );
            let forwarded = serialized_request(&mut kawa);
            assert!(
                forwarded.contains(expected),
                "{label}: expected {expected:?} in {forwarded:?}"
            );
            assert!(
                !forwarded.contains('+'),
                "{label}: a signed Content-Length must never be forwarded: {forwarded:?}"
            );
            assert_eq!(
                forwarded.matches("Content-Length").count(),
                usize::from(label != "plus-beside-chunked"),
                "{label}: exactly the framing Content-Length is forwarded: {forwarded:?}"
            );
        }
    }

    /// The response mirror: a backend's `Content-Length: +5` fails the
    /// response parse, which the mux answers with a 502 before any byte of
    /// it reaches the client (`shared::end_stream_decision`).
    ///
    /// kawa >= 0.7.2 already fails such a response itself
    /// (CleverCloud/kawa#26), before `on_response_headers` runs.
    ///
    /// TO SEE THIS RED: deleting the Content-Length clause of
    /// `on_response_headers` alone no longer turns it red. Pin
    /// kawa 0.7.1 (`kawa = { version = "=0.7.1", default-features = false }`
    /// in the root `Cargo.toml`, then `cargo update -p kawa --precise 0.7.1`)
    /// and delete the clause; the helper's own predicate is pinned by
    /// `the_content_length_helper_judges_every_non_digit_value`.
    #[test]
    fn a_response_content_length_that_is_not_only_digits_is_rejected() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        for status in ["200 OK", "204 No Content", "304 Not Modified"] {
            let response = format!("HTTP/1.1 {status}\r\nContent-Length: +5\r\n\r\nHello");
            let kawa = parse_framed(&mut pool, kawa::Kind::Response, response.as_bytes());
            assert!(
                kawa.is_error(),
                "{status}: a signed Content-Length must fail the response, got {:?}",
                kawa.parsing_phase
            );
        }
        let kawa = parse_framed(
            &mut pool,
            kawa::Kind::Response,
            b"HTTP/1.1 200 OK\r\nContent-Length: 005\r\n\r\nHello",
        );
        assert!(
            !kawa.is_error(),
            "a 1*DIGIT Content-Length must still be accepted, got {:?}",
            kawa.parsing_phase
        );
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

    // ── forwarding header family (`forwarded_headers`) ─────────────────

    /// A request carrying a client chain of every forwarding header the
    /// editor manages: two `X-Forwarded-For` lines, a `Forwarded` chain,
    /// `X-Forwarded-Proto`, `X-Forwarded-Port` and `X-Forwarded-Host`.
    const CLIENT_CHAIN_REQUEST: &[u8] = b"GET / HTTP/1.1\r\nHost: example.com\r\n\
        X-Forwarded-For: 198.51.100.1\r\n\
        X-Forwarded-For: 192.0.2.7\r\n\
        Forwarded: for=192.0.2.7\r\n\
        X-Forwarded-Proto: https\r\n\
        X-Forwarded-Port: 443\r\n\
        X-Forwarded-Host: public.example\r\n\
        X-Real-IP: 203.0.113.9\r\n\
        X-Request-Id: client-chosen\r\n\r\n";

    /// The request `on_request_headers` forwards for `request` on a
    /// connection from `10.0.0.1:54321` to `127.0.0.1:8080` whose listener
    /// has the forwarding header family `mode` and `send_x_real_ip` on,
    /// with the context left for the caller to inspect.
    fn forwarded_in_mode(mode: ForwardedHeaders, request: &[u8]) -> (String, HttpContext) {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let mut kawa = warm_request_kawa(&mut pool);
        let mut ctx = forwarding_context(
            Protocol::HTTP,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 54321),
            true,
        );
        ctx.forwarded_headers = mode;
        allocations_of_request_parse(&mut ctx, &mut kawa, request);
        (serialized_request(&mut kawa), ctx)
    }

    /// `both`, the default, is the historical behaviour, pinned byte for
    /// byte: a bare request gains the whole `X-Forwarded-*` family and a
    /// `Forwarded` element; a client chain is extended (the last
    /// `X-Forwarded-For` line and the last `Forwarded` line), and a client
    /// `X-Forwarded-Proto`, `-Port` or `-Host` is kept as sent.
    #[test]
    fn forwarded_headers_both_adds_both_families_and_extends_client_chains() {
        let (bare, ctx) = forwarded_in_mode(ForwardedHeaders::Both, BARE_REQUEST);
        let id = ctx.id.to_string();
        let traceparent = synthesised_traceparent_line(&ctx);
        assert_eq!(
            bare,
            format!(
                "GET / HTTP/1.1\r\nHost: example.com\r\n\
                 X-Forwarded-For: 10.0.0.1\r\n\
                 Forwarded: proto=http;for=\"10.0.0.1:54321\";by=127.0.0.1\r\n\
                 X-Real-IP: 10.0.0.1\r\n\
                 {traceparent}\
                 X-Forwarded-Port: 8080\r\n\
                 X-Forwarded-Proto: http\r\n\
                 X-Request-Id: {id}\r\n\
                 Sozu-Id: {id}\r\n\r\n"
            )
        );

        let (chained, ctx) = forwarded_in_mode(ForwardedHeaders::Both, CLIENT_CHAIN_REQUEST);
        let id = ctx.id.to_string();
        let traceparent = synthesised_traceparent_line(&ctx);
        assert_eq!(
            chained,
            format!(
                "GET / HTTP/1.1\r\nHost: example.com\r\n\
                 X-Forwarded-For: 198.51.100.1\r\n\
                 X-Forwarded-For: 192.0.2.7, 10.0.0.1\r\n\
                 Forwarded: for=192.0.2.7, proto=http;for=\"10.0.0.1:54321\";by=127.0.0.1\r\n\
                 X-Forwarded-Proto: https\r\n\
                 X-Forwarded-Port: 443\r\n\
                 X-Forwarded-Host: public.example\r\n\
                 X-Real-IP: 203.0.113.9\r\n\
                 X-Request-Id: client-chosen\r\n\
                 X-Real-IP: 10.0.0.1\r\n\
                 {traceparent}\
                 Sozu-Id: {id}\r\n\r\n"
            )
        );
        assert_eq!(ctx.xff_chain.as_deref(), Some("192.0.2.7"));
    }

    /// `x_forwarded` handles the `X-Forwarded-*` family exactly as `both`
    /// does and adds no `Forwarded`: a client `Forwarded` chain passes
    /// through as sent, neither extended nor removed.
    ///
    /// TO SEE THIS RED: in `HttpContext::on_request_headers`, drop the
    /// `emits_rfc7239` guard on the `Forwarded` extension and synthesis.
    #[test]
    fn forwarded_headers_x_forwarded_adds_only_the_x_forwarded_family() {
        let (bare, ctx) = forwarded_in_mode(ForwardedHeaders::XForwarded, BARE_REQUEST);
        let id = ctx.id.to_string();
        let traceparent = synthesised_traceparent_line(&ctx);
        assert_eq!(
            bare,
            format!(
                "GET / HTTP/1.1\r\nHost: example.com\r\n\
                 X-Forwarded-For: 10.0.0.1\r\n\
                 X-Real-IP: 10.0.0.1\r\n\
                 {traceparent}\
                 X-Forwarded-Port: 8080\r\n\
                 X-Forwarded-Proto: http\r\n\
                 X-Request-Id: {id}\r\n\
                 Sozu-Id: {id}\r\n\r\n"
            )
        );

        let (chained, ctx) = forwarded_in_mode(ForwardedHeaders::XForwarded, CLIENT_CHAIN_REQUEST);
        let id = ctx.id.to_string();
        let traceparent = synthesised_traceparent_line(&ctx);
        assert_eq!(
            chained,
            format!(
                "GET / HTTP/1.1\r\nHost: example.com\r\n\
                 X-Forwarded-For: 198.51.100.1\r\n\
                 X-Forwarded-For: 192.0.2.7, 10.0.0.1\r\n\
                 Forwarded: for=192.0.2.7\r\n\
                 X-Forwarded-Proto: https\r\n\
                 X-Forwarded-Port: 443\r\n\
                 X-Forwarded-Host: public.example\r\n\
                 X-Real-IP: 203.0.113.9\r\n\
                 X-Request-Id: client-chosen\r\n\
                 X-Real-IP: 10.0.0.1\r\n\
                 {traceparent}\
                 Sozu-Id: {id}\r\n\r\n"
            )
        );
        assert_eq!(ctx.xff_chain.as_deref(), Some("192.0.2.7"));
    }

    /// `rfc7239` adds only `Forwarded`: its own element is appended to the
    /// last client `Forwarded` line (RFC 7239 §4) or synthesised, and every
    /// client `X-Forwarded-For`, `-Proto`, `-Port` and `-Host` line is
    /// removed, so the backend sees one forwarding family whose last element
    /// Sōzu wrote. The client `X-Forwarded-For` is still recorded for the
    /// access log, and `X-Real-IP` is untouched by the mode.
    ///
    /// TO SEE THIS RED: in `HttpContext::on_request_headers`, drop the
    /// `strips_x_forwarded` branch that elides the client headers.
    #[test]
    fn forwarded_headers_rfc7239_adds_only_forwarded_and_strips_client_x_forwarded() {
        let (bare, ctx) = forwarded_in_mode(ForwardedHeaders::Rfc7239, BARE_REQUEST);
        let id = ctx.id.to_string();
        let traceparent = synthesised_traceparent_line(&ctx);
        assert_eq!(
            bare,
            format!(
                "GET / HTTP/1.1\r\nHost: example.com\r\n\
                 Forwarded: proto=http;for=\"10.0.0.1:54321\";by=127.0.0.1\r\n\
                 X-Real-IP: 10.0.0.1\r\n\
                 {traceparent}\
                 X-Request-Id: {id}\r\n\
                 Sozu-Id: {id}\r\n\r\n"
            )
        );

        let (chained, ctx) = forwarded_in_mode(ForwardedHeaders::Rfc7239, CLIENT_CHAIN_REQUEST);
        let id = ctx.id.to_string();
        let traceparent = synthesised_traceparent_line(&ctx);
        assert_eq!(
            chained,
            format!(
                "GET / HTTP/1.1\r\nHost: example.com\r\n\
                 Forwarded: for=192.0.2.7, proto=http;for=\"10.0.0.1:54321\";by=127.0.0.1\r\n\
                 X-Real-IP: 203.0.113.9\r\n\
                 X-Request-Id: client-chosen\r\n\
                 X-Real-IP: 10.0.0.1\r\n\
                 {traceparent}\
                 Sozu-Id: {id}\r\n\r\n"
            )
        );
        assert_eq!(ctx.xff_chain.as_deref(), Some("192.0.2.7"));
    }

    /// `none` adds no forwarding header of either family and leaves every
    /// client one as sent, while `X-Real-IP` — governed by `send_x_real_ip`
    /// alone — is still injected.
    ///
    /// TO SEE THIS RED: in `HttpContext::on_request_headers`, drop the
    /// `emits_x_forwarded` guard on the `X-Forwarded-*` synthesis.
    #[test]
    fn forwarded_headers_none_adds_nothing_and_passes_client_headers_through() {
        let (bare, ctx) = forwarded_in_mode(ForwardedHeaders::None, BARE_REQUEST);
        let id = ctx.id.to_string();
        let traceparent = synthesised_traceparent_line(&ctx);
        assert_eq!(
            bare,
            format!(
                "GET / HTTP/1.1\r\nHost: example.com\r\n\
                 X-Real-IP: 10.0.0.1\r\n\
                 {traceparent}\
                 X-Request-Id: {id}\r\n\
                 Sozu-Id: {id}\r\n\r\n"
            )
        );

        let (chained, ctx) = forwarded_in_mode(ForwardedHeaders::None, CLIENT_CHAIN_REQUEST);
        let id = ctx.id.to_string();
        let traceparent = synthesised_traceparent_line(&ctx);
        assert_eq!(
            chained,
            format!(
                "GET / HTTP/1.1\r\nHost: example.com\r\n\
                 X-Forwarded-For: 198.51.100.1\r\n\
                 X-Forwarded-For: 192.0.2.7\r\n\
                 Forwarded: for=192.0.2.7\r\n\
                 X-Forwarded-Proto: https\r\n\
                 X-Forwarded-Port: 443\r\n\
                 X-Forwarded-Host: public.example\r\n\
                 X-Real-IP: 203.0.113.9\r\n\
                 X-Request-Id: client-chosen\r\n\
                 X-Real-IP: 10.0.0.1\r\n\
                 {traceparent}\
                 Sozu-Id: {id}\r\n\r\n"
            )
        );
        assert_eq!(ctx.xff_chain.as_deref(), Some("192.0.2.7"));
    }

    /// A client `Forwarded` value is only extended when it is a well-formed
    /// RFC 7239 §4 list. An unclosed quoted-string would otherwise swallow
    /// Sōzu's element into the client's value
    /// (`for="6.6.6.6, proto=http;for="10.0.0.1:54321";by=127.0.0.1`), and
    /// the backend would read neither the client's element nor Sōzu's.
    /// In `both` and `rfc7239` — the modes that extend the chain — a
    /// malformed line is elided (RFC 7239 §4 lets a proxy remove the field),
    /// so the element Sōzu writes is always well-formed and last; a
    /// well-formed earlier line is extended instead. `x_forwarded` and
    /// `none` do not touch `Forwarded`, so the line passes through as sent.
    ///
    /// TO SEE THIS RED: in `HttpContext::on_request_headers`, drop the
    /// `is_valid_forwarded` check on the client `Forwarded` line.
    #[test]
    fn a_malformed_client_forwarded_is_elided_where_the_chain_is_extended() {
        const MALFORMED: &[u8] = b"GET / HTTP/1.1\r\nHost: example.com\r\n\
            Forwarded: for=\"6.6.6.6\r\n\r\n";
        const VALID_THEN_MALFORMED: &[u8] = b"GET / HTTP/1.1\r\nHost: example.com\r\n\
            Forwarded: for=192.0.2.7\r\n\
            Forwarded: for=\"6.6.6.6\r\n\r\n";
        let forwarded_lines = |out: &str| -> Vec<String> {
            out.split("\r\n")
                .filter_map(|line| line.strip_prefix("Forwarded: "))
                .map(ToOwned::to_owned)
                .collect()
        };
        let hop = "proto=http;for=\"10.0.0.1:54321\";by=127.0.0.1";

        for mode in [ForwardedHeaders::Both, ForwardedHeaders::Rfc7239] {
            let (out, _) = forwarded_in_mode(mode, MALFORMED);
            assert_eq!(
                forwarded_lines(&out),
                vec![hop.to_owned()],
                "{mode:?}: {out}"
            );
            let (out, _) = forwarded_in_mode(mode, VALID_THEN_MALFORMED);
            let lines = forwarded_lines(&out);
            assert_eq!(
                lines,
                vec![format!("for=192.0.2.7, {hop}")],
                "{mode:?}: {out}"
            );
            assert!(
                lines.iter().all(|line| is_valid_forwarded(line.as_bytes())),
                "{mode:?}: the forwarded chain must be well-formed: {lines:?}"
            );
        }
        for mode in [ForwardedHeaders::XForwarded, ForwardedHeaders::None] {
            let (out, _) = forwarded_in_mode(mode, MALFORMED);
            assert_eq!(
                forwarded_lines(&out),
                vec!["for=\"6.6.6.6".to_owned()],
                "{mode:?} passes a client Forwarded through as sent: {out}"
            );
        }
    }

    /// Read a process-local counter by raw key, treating an absent key as 0.
    /// `dump_local_proxy_metrics` does not drain, and `METRICS` is
    /// thread-local, so a test reads its own increments only.
    fn proxy_counter(key: &str) -> i64 {
        use sozu_command::proto::command::filtered_metrics::Inner;
        crate::metrics::METRICS.with(|metrics| {
            metrics
                .borrow_mut()
                .dump_local_proxy_metrics()
                .get(key)
                .and_then(|fm| fm.inner.as_ref())
                .and_then(|inner| match inner {
                    Inner::Count(v) => Some(*v),
                    _ => None,
                })
                .unwrap_or(0)
        })
    }

    /// Each client `Forwarded` line Sōzu removes as malformed is counted
    /// once in `http.forwarded_malformed_elided`, so an operator behind a
    /// non-conforming upstream load balancer sees the chain being dropped
    /// without enabling `debug` logs. A well-formed line, and every line in
    /// `x_forwarded` and `none` (which pass `Forwarded` through), counts
    /// nothing.
    ///
    /// TO SEE THIS RED: in `HttpContext::on_request_headers`, drop the
    /// `incr!(names::http::FORWARDED_MALFORMED_ELIDED)` on the elision path.
    #[test]
    fn a_malformed_client_forwarded_elision_is_counted_once_per_line() {
        const MALFORMED: &[u8] = b"GET / HTTP/1.1\r\nHost: example.com\r\n\
            Forwarded: for=\"6.6.6.6\r\n\r\n";
        const TWO_MALFORMED_AROUND_VALID: &[u8] = b"GET / HTTP/1.1\r\nHost: example.com\r\n\
            Forwarded: for=1.2.3.4:80\r\n\
            Forwarded: for=192.0.2.7\r\n\
            Forwarded: for=2001:db8::1\r\n\r\n";
        let elided = || proxy_counter(names::http::FORWARDED_MALFORMED_ELIDED);

        for mode in [ForwardedHeaders::Both, ForwardedHeaders::Rfc7239] {
            let before = elided();
            forwarded_in_mode(mode, CLIENT_CHAIN_REQUEST);
            assert_eq!(elided(), before, "{mode:?}: a valid line is not counted");

            let before = elided();
            forwarded_in_mode(mode, MALFORMED);
            assert_eq!(elided(), before + 1, "{mode:?}: one malformed line");

            let before = elided();
            forwarded_in_mode(mode, TWO_MALFORMED_AROUND_VALID);
            assert_eq!(
                elided(),
                before + 2,
                "{mode:?}: each removed line counts once, the valid one not"
            );
        }
        for mode in [ForwardedHeaders::XForwarded, ForwardedHeaders::None] {
            let before = elided();
            forwarded_in_mode(mode, MALFORMED);
            forwarded_in_mode(mode, TWO_MALFORMED_AROUND_VALID);
            assert_eq!(
                elided(),
                before,
                "{mode:?} passes a client Forwarded through and counts nothing"
            );
        }
    }

    /// `is_valid_forwarded` accepts the RFC 7239 §4 grammar — a list of
    /// `;`-separated `token=value` pairs, `value` a token or a quoted-string
    /// (RFC 9110 §5.6.4) — and refuses anything that would make a parser
    /// misread the element appended after it.
    #[test]
    fn is_valid_forwarded_follows_the_rfc7239_grammar() {
        for valid in [
            &b""[..],
            b"for=192.0.2.7",
            b"For=\"[2001:db8:cafe::17]:4711\"",
            b"for=192.0.2.60;proto=http;by=203.0.113.43",
            b"for=192.0.2.43, for=198.51.100.17",
            b"for=192.0.2.43,for=\"[2001:db8:cafe::17]\",for=unknown",
            b"for=_gazonk",
            b"for=\"\\\"quoted\\\"\"",
            b"for=a;",
            b"for=a ; by=b",
            b"for=a, , for=b",
            b"proto=http;for=\"10.0.0.1:54321\";by=127.0.0.1",
        ] {
            assert!(
                is_valid_forwarded(valid),
                "{:?} is valid",
                String::from_utf8_lossy(valid)
            );
        }
        for invalid in [
            &b"for=\"6.6.6.6"[..],
            b"for=\"6.6.6.6\\\"",
            b"for=",
            b"=192.0.2.7",
            b"for",
            b"for=a b",
            b"for=a\"b\"",
            b"for=\"a\"b",
            b"for = a",
            b"for=a\x01",
            b"for=\"a\x7f\"",
        ] {
            assert!(
                !is_valid_forwarded(invalid),
                "{:?} is invalid",
                String::from_utf8_lossy(invalid)
            );
        }
    }

    /// The mode is listener-scoped: `reset` carries it to the next request
    /// of a keep-alive connection.
    #[test]
    fn forwarded_headers_survives_reset() {
        let mut ctx = make_context();
        ctx.forwarded_headers = ForwardedHeaders::Rfc7239;
        ctx.reset(Ulid::generate());
        assert_eq!(ctx.forwarded_headers, ForwardedHeaders::Rfc7239);
    }

    // ── request framing: no Content-Length, no Transfer-Encoding ───────

    /// Parse `bytes` as a `kind` message through the real parser and a fresh
    /// context, as `ConnectionH1::readable` does.
    fn parse_message(
        pool: &mut crate::pool::Pool,
        kind: kawa::Kind,
        bytes: &[u8],
    ) -> GenericHttpStream {
        let mut kawa: GenericHttpStream = kawa::Kawa::new(
            kind,
            kawa::Buffer::new(
                pool.checkout()
                    .expect("the test pool must hand out a buffer"),
            ),
        );
        kawa.storage.space()[..bytes.len()].copy_from_slice(bytes);
        kawa.storage.fill(bytes.len());
        kawa::h1::parse(&mut kawa, &mut make_context());
        kawa
    }

    /// The flag block kawa pushes right after the header callback, which
    /// every converter reads to decide whether the message ended.
    fn header_end_flags(kawa: &GenericHttpStream) -> kawa::Flags {
        kawa.blocks
            .iter()
            .find_map(|block| match block {
                kawa::Block::Flags(flags) if flags.end_header => Some(flags.clone()),
                _ => None,
            })
            .expect("a parsed message carries its end-of-headers flags")
    }

    /// RFC 9112 §6.3 rule 7: a request with neither `Content-Length` nor
    /// `Transfer-Encoding` has a zero-length body, whatever its method or
    /// version. kawa 0.7.1 read such a request as close-delimited
    /// (`ParsingPhase::Body` on `BodySize::Empty`, whose arm takes every
    /// byte left in the buffer), so without `on_request_headers` ending it,
    /// the request pipelined behind it became a `Block::Chunk` of its body
    /// and was forwarded raw to the first request's backend (CWE-444).
    /// kawa >= 0.7.2 ends it itself (CleverCloud/kawa#27); the test pins the
    /// outcome, whichever layer ends it.
    ///
    /// TO SEE THIS RED: deleting the `ParsingPhase::Body` +
    /// `BodySize::Empty` branch of `on_request_headers` alone no longer
    /// turns it red, since kawa >= 0.7.2 terminates the request before that
    /// branch is reached. Pin
    /// kawa 0.7.1 (`kawa = { version = "=0.7.1", default-features = false }`
    /// in the root `Cargo.toml`, then `cargo update -p kawa --precise 0.7.1`)
    /// and delete the branch: every case then fails the first assertion,
    /// still in `Body`.
    #[test]
    fn a_request_without_length_ends_after_its_headers() {
        const NEXT: &[u8] = b"GET /next HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let heads: [&[u8]; 7] = [
            b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n",
            b"HEAD / HTTP/1.1\r\nHost: example.com\r\n\r\n",
            b"DELETE /item HTTP/1.1\r\nHost: example.com\r\n\r\n",
            b"POST /form HTTP/1.1\r\nHost: example.com\r\n\r\n",
            b"POST /form HTTP/1.0\r\nHost: example.com\r\n\r\n",
            b"PUT /item HTTP/1.1\r\nHost: example.com\r\nExpect: 100-continue\r\n\r\n",
            b"GET /chat HTTP/1.1\r\nHost: example.com\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n",
        ];
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        for head in heads {
            let label = String::from_utf8_lossy(head.split(|b| *b == b'\r').next().unwrap());
            let bytes = [head, NEXT].concat();
            let kawa = parse_message(&mut pool, kawa::Kind::Request, &bytes);
            assert!(
                kawa.is_terminated(),
                "{label}: must end after its headers, got {:?}",
                kawa.parsing_phase
            );
            let flags = header_end_flags(&kawa);
            assert!(
                flags.end_stream,
                "{label}: the end-of-headers flags must end the stream"
            );
            assert!(
                !kawa
                    .blocks
                    .iter()
                    .any(|block| matches!(block, kawa::Block::Chunk(_))),
                "{label}: no byte may be read as its body"
            );
            assert_eq!(
                kawa.storage.unparsed_data(),
                NEXT,
                "{label}: the pipelined request must stay unparsed for its own turn"
            );
        }
    }

    /// The guard reads a missing length, not a present one: a request that
    /// declares its body keeps it, and a close-delimited RESPONSE (RFC 9112
    /// §6.3 rule 8) keeps reading until the close.
    #[test]
    fn a_framed_request_and_an_unframed_response_keep_their_bodies() {
        let mut pool = crate::pool::Pool::with_capacity(1, 3, 4096);

        let length = parse_message(
            &mut pool,
            kawa::Kind::Request,
            b"POST / HTTP/1.1\r\nHost: example.com\r\nContent-Length: 10\r\n\r\nHello",
        );
        assert_eq!(length.parsing_phase, kawa::ParsingPhase::Body);
        assert_eq!(length.expects, 5, "half of the declared body is still due");

        let chunked = parse_message(
            &mut pool,
            kawa::Kind::Request,
            b"POST / HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nHello\r\n",
        );
        assert!(
            matches!(chunked.parsing_phase, kawa::ParsingPhase::Chunks { .. }),
            "a chunked request waits for its last chunk, got {:?}",
            chunked.parsing_phase
        );

        let response = parse_message(
            &mut pool,
            kawa::Kind::Response,
            b"HTTP/1.1 200 OK\r\n\r\nclose-delimited",
        );
        assert_eq!(
            response.parsing_phase,
            kawa::ParsingPhase::Body,
            "a response without length is delimited by the close"
        );
        assert!(!header_end_flags(&response).end_stream);
        assert!(
            response
                .blocks
                .iter()
                .any(|block| matches!(block, kawa::Block::Chunk(_))),
            "the close-delimited response body is still read"
        );
    }

    /// `pkawa::handle_header` calls the same `on_headers` for an H2 request
    /// BEFORE it resolves the framing: `body_size` is still `Empty` there
    /// for a request whose DATA frames follow, and it upgrades that to
    /// chunked afterwards unless the callback terminated the message. The
    /// guard keys on the `ParsingPhase::Body` kawa's H1 parser set for a
    /// close-delimited request up to kawa 0.7.1, which the H2 path never
    /// sets before the callback, so an H2 request is left for pkawa to frame.
    /// This test calls the callback directly, so it is unaffected by
    /// kawa >= 0.7.2 ending an H1 request itself.
    ///
    /// TO SEE THIS RED, key the guard on `body_size == BodySize::Empty`
    /// alone: this request is then terminated.
    #[test]
    fn an_h2_request_is_left_for_pkawa_to_frame() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let mut kawa: GenericHttpStream = kawa::Kawa::new(
            kawa::Kind::Request,
            kawa::Buffer::new(
                pool.checkout()
                    .expect("the test pool must hand out a buffer"),
            ),
        );
        kawa.detached.status_line = kawa::StatusLine::Request {
            version: kawa::Version::V20,
            method: kawa::Store::Static(b"POST"),
            uri: kawa::Store::Static(b"/upload"),
            authority: kawa::Store::Static(b"example.com"),
            path: kawa::Store::Static(b"/upload"),
        };
        let phase_before = kawa.parsing_phase;
        assert_eq!(kawa.body_size, kawa::BodySize::Empty, "premise");

        kawa::h1::ParserCallbacks::on_headers(&mut make_context(), &mut kawa);

        assert!(!kawa.is_terminated(), "an H2 request body may still follow");
        assert_eq!(kawa.parsing_phase, phase_before);
    }

    /// A backend response parsed by kawa's H1 parser on a context the test
    /// keeps, so it can read what `on_response_headers` recorded.
    fn parse_response(
        pool: &mut crate::pool::Pool,
        bytes: &[u8],
    ) -> (GenericHttpStream, HttpContext) {
        let mut ctx = make_context();
        let mut kawa: GenericHttpStream = kawa::Kawa::new(
            kawa::Kind::Response,
            kawa::Buffer::new(
                pool.checkout()
                    .expect("the test pool must hand out a buffer"),
            ),
        );
        kawa.storage.space()[..bytes.len()].copy_from_slice(bytes);
        kawa.storage.fill(bytes.len());
        kawa::h1::parse(&mut kawa, &mut ctx);
        assert!(!kawa.is_error(), "premise: the response must parse");
        (kawa, ctx)
    }

    /// RFC 9110 §6.2: Sōzu answers its client in its own HTTP version, not
    /// the backend's. An HTTP/1.0 response that asked to persist
    /// (`Connection: keep-alive`, RFC 9112 §9.3) with a `Content-Length` is
    /// forwarded as HTTP/1.1 and keeps both connections.
    ///
    /// TO SEE THIS RED, delete the `Version::V10` branch of
    /// `HttpContext::on_response_headers`: the status line keeps `HTTP/1.0`.
    #[test]
    fn an_http10_keep_alive_response_is_forwarded_as_http11() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let (mut kawa, ctx) = parse_response(
            &mut pool,
            b"HTTP/1.0 200 OK\r\nContent-Length: 5\r\nConnection: keep-alive\r\n\r\nhello",
        );
        assert!(kawa.is_terminated(), "premise: the whole body was parsed");
        assert!(
            ctx.keep_alive_backend,
            "a keep-alive HTTP/1.0 response keeps the backend connection"
        );
        let wire = serialized_request(&mut kawa);
        assert!(
            wire.starts_with("HTTP/1.1 200 OK\r\n"),
            "the status line carries Sōzu's version, got {wire:?}"
        );
        assert!(
            !wire.contains("HTTP/1.0"),
            "the backend's version must not reach the client, got {wire:?}"
        );
        assert!(
            !wire.to_ascii_lowercase().contains("connection: close"),
            "a persistent response must not announce a close, got {wire:?}"
        );
        assert!(
            wire.ends_with("\r\n\r\nhello"),
            "the body follows, got {wire:?}"
        );
    }

    /// An HTTP/1.0 response without `Connection: keep-alive` is not
    /// persistent (RFC 9112 §9.3): the backend closes after it. Forwarded as
    /// HTTP/1.1, the client would take it for persistent, so the response
    /// carries `Connection: close` (RFC 9112 §9.6) and `keep_alive_backend`
    /// is cleared, which closes the client connection after the response
    /// (`ConnectionH1::writable`) and lets the backend EOF end a
    /// close-delimited body (`ConnectionH1::terminate_close_delimited`).
    ///
    /// TO SEE THIS RED, delete the `Version::V10` branch of
    /// `HttpContext::on_response_headers`: the status line keeps `HTTP/1.0`
    /// and `keep_alive_backend` stays set.
    #[test]
    fn an_http10_response_without_keep_alive_is_forwarded_as_http11_with_close() {
        let cases: [(&str, &[u8], &str); 3] = [
            ("close-delimited", b"HTTP/1.0 200 OK\r\n\r\nabcd", "abcd"),
            (
                "content-length",
                b"HTTP/1.0 200 OK\r\nContent-Length: 4\r\n\r\nabcd",
                "abcd",
            ),
            (
                // A close-delimited body ends only with the close (RFC 9112
                // §6.3 rule 8), whatever the backend claimed.
                "keep-alive but close-delimited",
                b"HTTP/1.0 200 OK\r\nConnection: keep-alive\r\n\r\nabcd",
                "abcd",
            ),
        ];
        for (label, bytes, body) in cases {
            let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
            let (mut kawa, ctx) = parse_response(&mut pool, bytes);
            assert!(
                !ctx.keep_alive_backend,
                "{label}: a non-persistent HTTP/1.0 response closes the backend"
            );
            let wire = serialized_request(&mut kawa);
            assert!(
                wire.starts_with("HTTP/1.1 200 OK\r\n"),
                "{label}: the status line carries Sōzu's version, got {wire:?}"
            );
            let (head, forwarded_body) = wire
                .split_once("\r\n\r\n")
                .expect("the response head ends with an empty line");
            let connection: Vec<&str> = head
                .split("\r\n")
                .filter(|line| line.to_ascii_lowercase().starts_with("connection:"))
                .collect();
            assert_eq!(
                connection,
                ["Connection: close"],
                "{label}: exactly one Connection: close announces the close, got {wire:?}"
            );
            assert_eq!(forwarded_body, body, "{label}: the body is forwarded");
        }
    }

    /// The `Connection` lines of a serialised response head.
    fn connection_lines(wire: &str) -> Vec<&str> {
        let (head, _) = wire
            .split_once("\r\n\r\n")
            .expect("the response head ends with an empty line");
        head.split("\r\n")
            .filter(|line| line.to_ascii_lowercase().starts_with("connection:"))
            .collect()
    }

    /// `close` is a connection option like any other (RFC 9110 §7.6.1): a
    /// list that carries it closes the backend connection, whatever else it
    /// lists and in whichever `Connection` line, for HTTP/1.1 as for HTTP/1.0.
    ///
    /// TO SEE THIS RED, compare the whole `Connection` value with `close` in
    /// `HttpContext::on_response_headers` instead of each of its options.
    #[test]
    fn a_close_option_in_a_connection_list_closes_the_backend() {
        let cases: [(&str, &[u8]); 3] = [
            (
                "HTTP/1.0 list",
                b"HTTP/1.0 200 OK\r\nContent-Length: 4\r\nConnection: keep-alive, close\r\n\r\nabcd",
            ),
            (
                "HTTP/1.1 list",
                b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: keep-alive, Close \r\n\r\nabcd",
            ),
            (
                "HTTP/1.1 second line",
                b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: x-custom\r\nConnection: close\r\n\r\nabcd",
            ),
        ];
        for (label, bytes) in cases {
            let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
            let (mut kawa, ctx) = parse_response(&mut pool, bytes);
            assert!(
                !ctx.keep_alive_backend,
                "{label}: a close option closes the backend"
            );
            let wire = serialized_request(&mut kawa);
            assert!(
                connection_lines(&wire)
                    .iter()
                    .any(|line| line.to_ascii_lowercase().contains("close")),
                "{label}: the close is still announced, got {wire:?}"
            );
        }
    }

    /// A client request parsed by kawa's H1 parser on a context the test
    /// keeps, so it can read what `on_request_headers` recorded.
    fn parse_request(
        pool: &mut crate::pool::Pool,
        bytes: &[u8],
    ) -> (GenericHttpStream, HttpContext) {
        let mut ctx = make_context();
        let mut kawa: GenericHttpStream = kawa::Kawa::new(
            kawa::Kind::Request,
            kawa::Buffer::new(
                pool.checkout()
                    .expect("the test pool must hand out a buffer"),
            ),
        );
        kawa.storage.space()[..bytes.len()].copy_from_slice(bytes);
        kawa.storage.fill(bytes.len());
        kawa::h1::parse(&mut kawa, &mut ctx);
        assert!(!kawa.is_error(), "premise: the request must parse");
        (kawa, ctx)
    }

    /// `close` is a connection option of the request too (RFC 9110 §7.6.1),
    /// and a client that sends it asks for the connection to close after the
    /// response (RFC 9112 §9.6): a list that carries it clears
    /// `keep_alive_frontend`, whatever else it lists and in whichever
    /// `Connection` line, as `on_response_headers` already does for the
    /// backend. A token that merely starts with `close` is not the option,
    /// and the forwarded `Connection` lines are left as the client sent them.
    ///
    /// TO SEE THIS RED, compare the whole field value with `close` in
    /// `HttpContext::on_request_headers` instead of matching a list token:
    /// the three list cases keep the client connection.
    #[test]
    fn a_close_option_in_a_request_connection_list_closes_the_client() {
        let cases: [(&str, &[u8], bool); 7] = [
            (
                "single close",
                b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n",
                false,
            ),
            (
                "keep-alive then close",
                b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: keep-alive, close\r\n\r\n",
                false,
            ),
            (
                "close then TE",
                b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close, TE\r\nTE: trailers\r\n\r\n",
                false,
            ),
            (
                "close listed in the second line",
                b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: keep-alive\r\nConnection: TE, Close \r\nTE: trailers\r\n\r\n",
                false,
            ),
            (
                "closed is not close",
                b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: closed\r\n\r\n",
                true,
            ),
            (
                "keep-alive only",
                b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: keep-alive\r\n\r\n",
                true,
            ),
            (
                "no Connection",
                b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n",
                true,
            ),
        ];
        for (label, bytes, keeps) in cases {
            let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
            let (mut kawa, ctx) = parse_request(&mut pool, bytes);
            assert_eq!(
                ctx.keep_alive_frontend, keeps,
                "{label}: keep_alive_frontend"
            );
            assert!(
                ctx.keep_alive_backend,
                "{label}: the request never decides the backend's persistence"
            );
            let sent: Vec<String> = connection_lines(&String::from_utf8_lossy(bytes))
                .iter()
                .map(|line| line.to_ascii_lowercase().replace(' ', ""))
                .collect();
            let wire = serialized_request(&mut kawa);
            let forwarded: Vec<String> = connection_lines(&wire)
                .iter()
                .map(|line| line.to_ascii_lowercase().replace(' ', ""))
                .collect();
            assert_eq!(
                forwarded, sent,
                "{label}: the Connection lines are forwarded as sent, got {wire:?}"
            );
        }
    }

    /// Removing a `Connection` header would turn the fields it nominates as
    /// hop-by-hop into end-to-end ones (RFC 9110 §7.6.1). A non-persistent
    /// HTTP/1.0 response keeps its other options and gains `close`; only
    /// `keep-alive`, which the close contradicts, is dropped.
    ///
    /// TO SEE THIS RED, elide the backend's `Connection` headers and push a
    /// bare `Connection: close` instead of extending the option list.
    #[test]
    fn a_non_persistent_http10_response_keeps_its_connection_options() {
        let cases: [(&str, &[u8], &str); 3] = [
            (
                "nominated field",
                b"HTTP/1.0 200 OK\r\nConnection: x-custom\r\nX-Custom: 1\r\n\r\nabcd",
                "Connection: x-custom, close",
            ),
            (
                "keep-alive dropped",
                b"HTTP/1.0 200 OK\r\nConnection: keep-alive, x-custom\r\n\r\nabcd",
                "Connection: x-custom, close",
            ),
            (
                "two lines merged",
                b"HTTP/1.0 200 OK\r\nConnection: x-one\r\nConnection: x-two\r\n\r\nabcd",
                "Connection: x-one, x-two, close",
            ),
        ];
        for (label, bytes, want) in cases {
            let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
            let (mut kawa, ctx) = parse_response(&mut pool, bytes);
            assert!(!ctx.keep_alive_backend, "{label}: premise, not persistent");
            let wire = serialized_request(&mut kawa);
            assert_eq!(
                connection_lines(&wire),
                [want],
                "{label}: the options survive beside close, got {wire:?}"
            );
        }
    }

    /// RFC 9112 §6.1: an HTTP/1.0 message with `Transfer-Encoding` has
    /// faulty framing and its connection closes after it, `keep-alive` or
    /// not.
    ///
    /// TO SEE THIS RED, drop the `Transfer-Encoding` clause from the HTTP/1.0
    /// persistence test of `HttpContext::on_response_headers`.
    #[test]
    fn a_chunked_http10_response_closes_the_backend() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let (mut kawa, ctx) = parse_response(
            &mut pool,
            b"HTTP/1.0 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n4\r\nabcd\r\n0\r\n\r\n",
        );
        assert!(kawa.is_terminated(), "premise: the chunked body ended");
        assert!(
            !ctx.keep_alive_backend,
            "a chunked HTTP/1.0 response closes the backend"
        );
        let wire = serialized_request(&mut kawa);
        assert_eq!(
            connection_lines(&wire),
            ["Connection: close"],
            "got {wire:?}"
        );
    }

    /// An interim response says nothing about the connection: a 101 from an
    /// HTTP/1.0 backend keeps its `Connection: Upgrade`, without which the
    /// client does not switch protocols (RFC 9110 §7.8).
    ///
    /// TO SEE THIS RED, apply the HTTP/1.0 persistence rewrite of
    /// `HttpContext::on_response_headers` to 1xx responses too.
    #[test]
    fn an_http10_101_keeps_its_connection_upgrade() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let (mut kawa, ctx) = parse_response(
            &mut pool,
            b"HTTP/1.0 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n",
        );
        assert!(
            ctx.keep_alive_backend,
            "a 101 leaves keep_alive_backend alone"
        );
        let wire = serialized_request(&mut kawa);
        assert!(
            wire.starts_with("HTTP/1.1 101 Switching Protocols\r\n"),
            "got {wire:?}"
        );
        assert_eq!(
            connection_lines(&wire),
            ["Connection: Upgrade"],
            "the upgrade must survive, got {wire:?}"
        );
    }

    /// A 100 from an HTTP/1.0 backend leaves the persistence of the final
    /// response to that response: the backend context outlives the interim
    /// response, so a 100 that cleared `keep_alive_backend` would close a
    /// persistent final response.
    ///
    /// TO SEE THIS RED, apply the HTTP/1.0 persistence rewrite of
    /// `HttpContext::on_response_headers` to 1xx responses too.
    #[test]
    fn an_http10_100_leaves_persistence_to_the_final_response() {
        let mut pool = crate::pool::Pool::with_capacity(1, 2, 4096);
        let mut ctx = make_context();
        for (bytes, label) in [
            (&b"HTTP/1.0 100 Continue\r\n\r\n"[..], "100"),
            (
                &b"HTTP/1.0 200 OK\r\nContent-Length: 4\r\nConnection: keep-alive\r\n\r\nabcd"[..],
                "final",
            ),
        ] {
            let mut kawa: GenericHttpStream = kawa::Kawa::new(
                kawa::Kind::Response,
                kawa::Buffer::new(
                    pool.checkout()
                        .expect("the test pool must hand out a buffer"),
                ),
            );
            kawa.storage.space()[..bytes.len()].copy_from_slice(bytes);
            kawa.storage.fill(bytes.len());
            kawa::h1::parse(&mut kawa, &mut ctx);
            assert!(kawa.is_terminated(), "{label}: premise, parsed whole");
            assert!(
                ctx.keep_alive_backend,
                "{label}: a 100 then a keep-alive response keep the backend"
            );
            let wire = serialized_request(&mut kawa);
            assert!(
                connection_lines(&wire)
                    .iter()
                    .all(|line| !line.to_ascii_lowercase().contains("close")),
                "{label}: no close is announced, got {wire:?}"
            );
        }
    }

    /// A message parsed by kawa's H1 parser on a context that is shutting
    /// down (`HttpContext::closing`, set by `Mux::shutting_down_inner`,
    /// `lib/src/protocol/mux/mod.rs`), which the test keeps.
    fn parse_closing(
        pool: &mut crate::pool::Pool,
        kind: kawa::Kind,
        bytes: &[u8],
    ) -> (GenericHttpStream, HttpContext) {
        let mut ctx = make_context();
        ctx.closing = true;
        let mut kawa: GenericHttpStream = kawa::Kawa::new(
            kind,
            kawa::Buffer::new(
                pool.checkout()
                    .expect("the test pool must hand out a buffer"),
            ),
        );
        kawa.storage.space()[..bytes.len()].copy_from_slice(bytes);
        kawa.storage.fill(bytes.len());
        kawa::h1::parse(&mut kawa, &mut ctx);
        assert!(!kawa.is_error(), "premise: the message must parse");
        (kawa, ctx)
    }

    /// The head of a serialised message, lower-cased, one entry per line.
    fn head_lines(wire: &str) -> Vec<String> {
        let (head, _) = wire
            .split_once("\r\n\r\n")
            .expect("the head ends with an empty line");
        head.split("\r\n")
            .map(|line| line.to_ascii_lowercase())
            .collect()
    }

    /// While Sōzu shuts down, the request it forwards announces the close
    /// (RFC 9112 §9.6) without dropping the other options the client listed:
    /// removing one would turn the field it nominates as hop-by-hop into an
    /// end-to-end one (RFC 9110 §7.6.1). The options are merged into one
    /// line ending in a single `close`, `keep-alive` is dropped as the close
    /// contradicts it, and a client `close` still clears
    /// `keep_alive_frontend`.
    ///
    /// TO SEE THIS RED, restore the whole-value overwrite of the `Connection`
    /// field under `self.closing` in `HttpContext::on_request_headers`.
    #[test]
    fn a_shutting_down_request_keeps_its_connection_options() {
        let cases: [(&str, &[u8], &str, bool); 6] = [
            (
                "no Connection",
                b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n",
                "Connection: close",
                true,
            ),
            (
                "nominated field",
                b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: x-custom\r\nX-Custom: 1\r\n\r\n",
                "Connection: x-custom, close",
                true,
            ),
            (
                "keep-alive dropped",
                b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: keep-alive, x-custom\r\n\r\n",
                "Connection: x-custom, close",
                true,
            ),
            (
                "single close not duplicated",
                b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n",
                "Connection: close",
                false,
            ),
            (
                "two lines merged",
                b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: x-one\r\nConnection: TE, Close \r\nTE: trailers\r\n\r\n",
                "Connection: x-one, TE, close",
                false,
            ),
            (
                "HTTP/1.0",
                b"GET / HTTP/1.0\r\nHost: example.com\r\nConnection: keep-alive, x-custom\r\n\r\n",
                "Connection: x-custom, close",
                true,
            ),
        ];
        for (label, bytes, want, keeps) in cases {
            let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
            let (mut kawa, ctx) = parse_closing(&mut pool, kawa::Kind::Request, bytes);
            assert_eq!(
                ctx.keep_alive_frontend, keeps,
                "{label}: a client close is still recorded"
            );
            let wire = serialized_request(&mut kawa);
            assert_eq!(
                connection_lines(&wire),
                [want],
                "{label}: the options survive beside one close, got {wire:?}"
            );
        }
    }

    /// A shutting-down Sōzu refuses a protocol upgrade: it closes the client
    /// connection as soon as the response is written
    /// (`ConnectionH1::writable`, `lib/src/protocol/mux/h1.rs`, tests
    /// `closing` before it handles a 101), so a 101 would hand the client a
    /// switched protocol on a connection that is already closing. The
    /// `upgrade` option is dropped with the `Upgrade` field it nominates
    /// (RFC 9110 §7.6.1), so the backend answers in HTTP/1.1, which it may
    /// always do (RFC 9110 §7.8), and the client retries the upgrade on a
    /// new connection. The other options are kept.
    ///
    /// TO SEE THIS RED, keep the `upgrade` option or the `Upgrade` field in
    /// the shutdown merge of `HttpContext::on_request_headers`.
    #[test]
    fn a_shutting_down_request_does_not_upgrade() {
        let cases: [(&str, &[u8], &str); 2] = [
            (
                "websocket",
                b"GET /chat HTTP/1.1\r\nHost: example.com\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n",
                "Connection: close",
            ),
            (
                "upgrade among other options",
                b"GET /chat HTTP/1.1\r\nHost: example.com\r\nConnection: keep-alive, Upgrade, x-custom\r\nUpgrade: websocket\r\nX-Custom: 1\r\n\r\n",
                "Connection: x-custom, close",
            ),
        ];
        for (label, bytes, want) in cases {
            let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
            let (mut kawa, _ctx) = parse_closing(&mut pool, kawa::Kind::Request, bytes);
            let wire = serialized_request(&mut kawa);
            assert_eq!(
                connection_lines(&wire),
                [want],
                "{label}: the upgrade is not requested, got {wire:?}"
            );
            assert!(
                !head_lines(&wire)
                    .iter()
                    .any(|line| line.starts_with("upgrade:")),
                "{label}: the Upgrade field is not forwarded, got {wire:?}"
            );
        }
    }

    /// While Sōzu shuts down, the final response it forwards announces the
    /// close (RFC 9112 §9.6), in HTTP/1.1 as in HTTP/1.0, and keeps the
    /// options the backend listed (RFC 9110 §7.6.1) apart from the
    /// `keep-alive` the close contradicts. The backend's own `close` is
    /// still recorded in `keep_alive_backend`, so that connection is not
    /// pooled.
    ///
    /// TO SEE THIS RED, restore the whole-value overwrite of the `Connection`
    /// field under `self.closing` in `HttpContext::on_response_headers`.
    #[test]
    fn a_shutting_down_response_keeps_its_connection_options() {
        let cases: [(&str, &[u8], &str, bool); 6] = [
            (
                "no Connection",
                b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nabcd",
                "Connection: close",
                true,
            ),
            (
                "nominated field",
                b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: x-custom\r\nX-Custom: 1\r\n\r\nabcd",
                "Connection: x-custom, close",
                true,
            ),
            (
                "keep-alive dropped",
                b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: keep-alive, x-custom\r\n\r\nabcd",
                "Connection: x-custom, close",
                true,
            ),
            (
                "single close not duplicated",
                b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nabcd",
                "Connection: close",
                false,
            ),
            (
                "two lines merged",
                b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: x-one\r\nConnection: x-two, Close \r\n\r\nabcd",
                "Connection: x-one, x-two, close",
                false,
            ),
            (
                "persistent HTTP/1.0",
                b"HTTP/1.0 200 OK\r\nContent-Length: 4\r\nConnection: keep-alive, x-custom\r\n\r\nabcd",
                "Connection: x-custom, close",
                true,
            ),
        ];
        for (label, bytes, want, keeps) in cases {
            let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
            let (mut kawa, ctx) = parse_closing(&mut pool, kawa::Kind::Response, bytes);
            assert_eq!(
                ctx.keep_alive_backend, keeps,
                "{label}: the backend's close is still recorded"
            );
            let wire = serialized_request(&mut kawa);
            assert!(wire.starts_with("HTTP/1.1 200 OK\r\n"), "got {wire:?}");
            assert_eq!(
                connection_lines(&wire),
                [want],
                "{label}: the options survive beside one close, got {wire:?}"
            );
        }
    }

    /// An interim response says nothing about the connection, shutdown or
    /// not: the final response carries the close, and a 101 keeps its
    /// `Connection: Upgrade` (RFC 9110 §7.8).
    ///
    /// TO SEE THIS RED, apply the shutdown merge of
    /// `HttpContext::on_response_headers` to 1xx responses too.
    #[test]
    fn a_shutting_down_interim_response_is_left_alone() {
        let cases: [(&str, &[u8], &[&str]); 2] = [
            (
                "101",
                b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n",
                &["Connection: Upgrade"],
            ),
            ("100", b"HTTP/1.1 100 Continue\r\n\r\n", &[]),
        ];
        for (label, bytes, want) in cases {
            let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
            let (mut kawa, ctx) = parse_closing(&mut pool, kawa::Kind::Response, bytes);
            assert!(
                ctx.keep_alive_backend,
                "{label}: an interim response leaves keep_alive_backend alone"
            );
            let wire = serialized_request(&mut kawa);
            assert_eq!(
                connection_lines(&wire),
                want,
                "{label}: the interim Connection lines are forwarded as sent, got {wire:?}"
            );
        }
    }

    // ── chunked request trailers: elision and field bound ──────────────

    /// Filter the trailer fields of `kawa` appended after `first_new_block`
    /// with a fresh context, and return how many spoof vectors were elided.
    fn elide_spoof_vectors(kawa: &mut GenericHttpStream, first_new_block: usize) -> usize {
        make_context()
            .filter_request_trailers(kawa, first_new_block)
            .spoof_vectors
    }

    /// Head and body of a chunked request, up to and including the
    /// last-chunk line, so a test can send its trailer section separately.
    const CHUNKED_HEAD: &[u8] = b"POST /upload HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nHello\r\n0\r\n";

    /// A trailer section carrying every spoof-vector name, in mixed case,
    /// with the forged value `6.6.6.6`, plus one legitimate field.
    const SPOOF_TRAILERS: &[u8] = b"X-Forwarded-For: 6.6.6.6\r\nforwarded: for=6.6.6.6\r\nX-REAL-IP: 6.6.6.6\r\nX-Request-Id: 6.6.6.6\r\nX-Forwarded-Proto: 6.6.6.6\r\nx-forwarded-port: 6.6.6.6\r\nX-Forwarded-Host: 6.6.6.6\r\nGrpc-Status: 0\r\n\r\n";

    /// RFC 9110 §6.5.1, sozu-proxy/sozu#1689: an H1 chunked request cannot
    /// carry `X-Forwarded-For`, `Forwarded`, `X-Real-IP` or the rest of
    /// `TRAILER_SPOOF_VECTOR_HEADERS` in its trailer section, whatever the
    /// case of the name, while a legitimate trailer and the chunk framing
    /// survive: last-chunk, the surviving field, then the empty line.
    ///
    /// TO SEE THIS RED: make `HttpContext::filter_request_trailers` return
    /// before its walk.
    #[test]
    fn a_chunked_request_trailer_section_loses_its_spoof_vector_fields() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let bytes = [CHUNKED_HEAD, SPOOF_TRAILERS].concat();
        let mut kawa = parse_message(&mut pool, kawa::Kind::Request, &bytes);
        assert!(kawa.is_terminated(), "the trailer section ends the request");

        assert_eq!(
            elide_spoof_vectors(&mut kawa, 0),
            TRAILER_SPOOF_VECTOR_HEADERS.len(),
            "every spoof-vector trailer field is elided once"
        );
        assert_eq!(
            elide_spoof_vectors(&mut kawa, 0),
            0,
            "a second walk finds nothing left to elide"
        );
        let wire = serialized_request(&mut kawa);
        assert!(
            !wire.contains("6.6.6.6"),
            "no spoofed trailer may reach the backend, got {wire:?}"
        );
        assert!(
            wire.ends_with("\r\n\r\n5\r\nHello\r\n0\r\nGrpc-Status: 0\r\n\r\n"),
            "the legitimate trailer and the chunk framing survive, got {wire:?}"
        );
    }

    /// The trailer section may arrive after the last-chunk line was already
    /// forwarded, so the `end_body` marker is no longer in the block queue
    /// when the trailer fields are parsed. They must still be elided, and a
    /// request whose every trailer field is dropped still ends with the
    /// empty line that closes its (now empty) trailer section.
    #[test]
    fn a_trailer_section_parsed_after_the_last_chunk_was_forwarded_is_filtered() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let mut kawa = parse_message(&mut pool, kawa::Kind::Request, CHUNKED_HEAD);
        assert_eq!(kawa.parsing_phase, kawa::ParsingPhase::Trailers);
        assert_eq!(elide_spoof_vectors(&mut kawa, 0), 0);
        let head = serialized_request(&mut kawa);
        assert!(
            kawa.blocks.is_empty(),
            "the last-chunk marker was forwarded"
        );

        const ONLY_SPOOF: &[u8] = b"X-Forwarded-For: 6.6.6.6\r\nForwarded: for=6.6.6.6\r\n\r\n";
        kawa.storage.space()[..ONLY_SPOOF.len()].copy_from_slice(ONLY_SPOOF);
        kawa.storage.fill(ONLY_SPOOF.len());
        let before = kawa.blocks.len();
        kawa::h1::parse(&mut kawa, &mut make_context());
        assert!(kawa.is_terminated(), "the trailer section ends the request");
        assert_eq!(elide_spoof_vectors(&mut kawa, before), 2);
        let wire = serialized_request(&mut kawa);
        assert!(
            wire.starts_with(&head) && !wire.contains("6.6.6.6"),
            "no spoofed trailer may reach the backend, got {wire:?}"
        );
        assert!(
            wire.ends_with("0\r\n\r\n"),
            "the empty trailer section is still closed, got {wire:?}"
        );
    }

    /// The elision is confined to a chunked request's trailer section: the
    /// header block of a request, chunked or not, and a response trailer
    /// are left to their own paths, and the walk returns before touching a
    /// block.
    #[test]
    fn trailer_elision_leaves_headers_and_responses_alone() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let cases: [(kawa::Kind, &[u8]); 3] = [
            (
                kawa::Kind::Request,
                b"POST / HTTP/1.1\r\nHost: example.com\r\nX-Request-Id: keep\r\nContent-Length: 2\r\n\r\nok",
            ),
            (
                kawa::Kind::Request,
                b"POST / HTTP/1.1\r\nHost: example.com\r\nX-Request-Id: keep\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n",
            ),
            (
                kawa::Kind::Response,
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\nX-Request-Id: keep\r\n\r\n",
            ),
        ];
        for (kind, bytes) in cases {
            let mut kawa = parse_message(&mut pool, kind, bytes);
            assert_eq!(
                elide_spoof_vectors(&mut kawa, 0),
                0,
                "{kind:?} {:?}: nothing to elide",
                kawa.parsing_phase
            );
            let wire = serialized_request(&mut kawa);
            assert!(
                wire.contains("X-Request-Id: keep\r\n"),
                "{kind:?}: the field survives, got {wire:?}"
            );
        }
    }

    /// Append `bytes` to `kawa`, parse them, and filter the trailer fields
    /// that parse appended with `context`, as `ConnectionH1::readable` does.
    fn feed_and_filter(
        context: &mut HttpContext,
        kawa: &mut GenericHttpStream,
        bytes: &[u8],
    ) -> TrailerFilterOutcome {
        kawa.storage.space()[..bytes.len()].copy_from_slice(bytes);
        kawa.storage.fill(bytes.len());
        let before = kawa.blocks.len();
        kawa::h1::parse(kawa, &mut make_context());
        context.filter_request_trailers(kawa, before)
    }

    /// [`feed_and_filter`] with a fresh context, returning how many spoof
    /// vectors were elided.
    fn feed_and_elide(kawa: &mut GenericHttpStream, bytes: &[u8]) -> usize {
        feed_and_filter(&mut make_context(), kawa, bytes).spoof_vectors
    }

    /// The trailer section itself may be split across reads, and the first
    /// part forwarded before the second arrives: `X-Forwarded-For` comes in
    /// one segment and is drained towards the backend, `Forwarded` in the
    /// next. Both must be elided, and the legitimate field and the closing
    /// empty line of the section survive.
    ///
    /// TO SEE THIS RED: make `HttpContext::filter_request_trailers` return
    /// before its walk.
    #[test]
    fn a_trailer_section_split_across_reads_is_filtered_in_each_part() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let mut kawa = parse_message(&mut pool, kawa::Kind::Request, CHUNKED_HEAD);
        assert_eq!(kawa.parsing_phase, kawa::ParsingPhase::Trailers);

        assert_eq!(
            feed_and_elide(&mut kawa, b"X-Forwarded-For: 6.6.6.6\r\n"),
            1,
            "the first trailer part is filtered on its own"
        );
        assert_eq!(kawa.parsing_phase, kawa::ParsingPhase::Trailers);
        let first = serialized_request(&mut kawa);
        assert!(kawa.blocks.is_empty(), "the first part was forwarded");

        assert_eq!(
            feed_and_elide(
                &mut kawa,
                b"Forwarded: for=6.6.6.6\r\nGrpc-Status: 0\r\n\r\n"
            ),
            1,
            "the second trailer part is filtered on its own"
        );
        assert!(kawa.is_terminated(), "the trailer section ends the request");
        let wire = serialized_request(&mut kawa);
        assert!(
            wire.starts_with(&first) && !wire.contains("6.6.6.6"),
            "no spoofed trailer may reach the backend, got {wire:?}"
        );
        assert!(
            wire.ends_with("0\r\nGrpc-Status: 0\r\n\r\n"),
            "the legitimate trailer and the closing empty line survive, got {wire:?}"
        );
    }

    /// Each call walks only the blocks the last parse appended, so a client
    /// trickling one trailer line per segment while the backend is not
    /// writable costs a linear walk in total, not a quadratic one. A field
    /// queued before the parse is left for the call that followed the parse
    /// which queued it: here it was deliberately never filtered, and the
    /// bounded walk must not reach it.
    ///
    /// TO SEE THIS RED: ignore `first_new_block` in
    /// `HttpContext::filter_request_trailers` and walk the whole queue
    /// (`range_mut(0..)`); the call then also elides the older field.
    #[test]
    fn a_trailer_walk_stops_at_the_blocks_queued_before_the_parse() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let head = [CHUNKED_HEAD, b"X-Forwarded-For: 6.6.6.6\r\n"].concat();
        let mut kawa = parse_message(&mut pool, kawa::Kind::Request, &head);
        assert_eq!(kawa.parsing_phase, kawa::ParsingPhase::Trailers);
        let queued = kawa.blocks.len();

        // Trickle the rest of the section one line per read, never
        // draining: each call must see exactly the one new field.
        for line in [
            &b"Forwarded: for=6.6.6.6\r\n"[..],
            b"X-Real-IP: 6.6.6.6\r\n",
            b"Grpc-Status: 0\r\n",
        ] {
            let spoofed = usize::from(!line.starts_with(b"Grpc"));
            assert_eq!(
                feed_and_elide(&mut kawa, line),
                spoofed,
                "only the field this read appended is examined"
            );
        }
        assert_eq!(feed_and_elide(&mut kawa, b"\r\n"), 0);
        assert!(kawa.is_terminated(), "the trailer section ends the request");

        let buf = kawa.storage.buffer();
        let older = kawa
            .blocks
            .range(..queued)
            .filter_map(|block| match block {
                kawa::Block::Header(pair) if !pair.is_elided() => Some(pair.key.data(buf)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            older
                .iter()
                .any(|key| key.eq_ignore_ascii_case(b"X-Forwarded-For")),
            "a block queued before the parse is outside the walk, got {older:?}"
        );
    }

    /// A trailer section carrying one field of each RFC 9110 §6.5.1
    /// category in mixed case, with the forged value `6.6.6.6`, plus two
    /// legitimate fields that must survive.
    const FORBIDDEN_TRAILERS: &[u8] = b"Content-Length: 6666\r\nTRANSFER-ENCODING: 6.6.6.6\r\nHost: 6.6.6.6\r\nExpect: 6.6.6.6\r\nIf-Match: 6.6.6.6\r\nauthorization: 6.6.6.6\r\nCookie: 6.6.6.6\r\nContent-Type: 6.6.6.6\r\nTrailer: 6.6.6.6\r\nConnection: 6.6.6.6\r\nGrpc-Status: 0\r\nX-Checksum: abc\r\n\r\n";

    /// RFC 9110 §6.5.1, sozu-proxy/sozu#1701: an H1 chunked request cannot
    /// carry a framing, routing, request-modifier, authentication, content
    /// processing or connection-specific field in its trailer section,
    /// whatever the case of the name; legitimate trailers and the chunk
    /// framing survive.
    ///
    /// TO SEE THIS RED: drop the `is_trailer_forbidden_field` branch of
    /// `HttpContext::filter_request_trailers`.
    #[test]
    fn a_chunked_request_trailer_section_loses_its_forbidden_fields() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let bytes = [CHUNKED_HEAD, FORBIDDEN_TRAILERS].concat();
        let mut kawa = parse_message(&mut pool, kawa::Kind::Request, &bytes);
        assert!(kawa.is_terminated(), "the trailer section ends the request");

        let mut context = make_context();
        let outcome = context.filter_request_trailers(&mut kawa, 0);
        assert_eq!(
            outcome,
            TrailerFilterOutcome {
                spoof_vectors: 0,
                forbidden: 10,
                over_limit: false,
            },
            "every forbidden trailer field is elided once"
        );
        assert_eq!(context.trailer_fields, 12, "every field is counted");
        assert!(!kawa.is_error(), "a section within the bound is admitted");
        let wire = serialized_request(&mut kawa);
        // The forged `Content-Length` is matched on its whole field line: a
        // bare `6666` also occurs in the random `traceparent` and request ids.
        assert!(
            !wire.contains("6.6.6.6")
                && !wire
                    .to_ascii_lowercase()
                    .contains("\r\ncontent-length: 6666\r\n"),
            "no forbidden trailer may reach the backend, got {wire:?}"
        );
        assert!(
            wire.ends_with("\r\n\r\n5\r\nHello\r\n0\r\nGrpc-Status: 0\r\nX-Checksum: abc\r\n\r\n"),
            "the legitimate trailers and the chunk framing survive, got {wire:?}"
        );
    }

    /// Every name of `TRAILER_FORBIDDEN_FIELDS` is recognised in upper case
    /// too, and no spoof vector is double-listed in it, so each elided field
    /// lands in exactly one metric.
    #[test]
    fn trailer_forbidden_fields_are_matched_without_case_and_disjoint_from_spoof_vectors() {
        for name in TRAILER_FORBIDDEN_FIELDS {
            assert!(
                is_trailer_forbidden_field(&name.to_ascii_uppercase()),
                "{:?} is matched in upper case",
                String::from_utf8_lossy(name)
            );
            assert!(
                !is_trailer_spoof_vector(name),
                "{:?} is in both lists",
                String::from_utf8_lossy(name)
            );
        }
        for legit in [
            &b"grpc-status"[..],
            b"grpc-message",
            b"x-checksum",
            b"digest",
        ] {
            assert!(!is_trailer_forbidden_field(legit));
        }
    }

    /// Build a trailer section of `count` distinct legitimate fields.
    fn numbered_trailers(count: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        for i in 0..count {
            bytes.extend_from_slice(format!("X-T{i}: {i}\r\n").as_bytes());
        }
        bytes.extend_from_slice(b"\r\n");
        bytes
    }

    /// sozu-proxy/sozu#1701: a trailer section may carry up to
    /// `max_trailer_fields` fields, the bound `h2_max_header_fields` puts on
    /// an H2 trailer block; one more marks the request in error, so the
    /// caller answers 400 before any of it is forwarded. Elided fields count
    /// too, as they do on the H2 path.
    ///
    /// TO SEE THIS RED: drop the `max_trailer_fields` comparison of
    /// `HttpContext::filter_request_trailers`.
    #[test]
    fn a_trailer_section_over_the_field_bound_is_an_error() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        for (count, over) in [(4, false), (5, true)] {
            let bytes = [CHUNKED_HEAD, &numbered_trailers(count)].concat();
            let mut kawa = parse_message(&mut pool, kawa::Kind::Request, &bytes);
            assert!(kawa.is_terminated());
            let mut context = make_context();
            context.max_trailer_fields = 4;
            let outcome = context.filter_request_trailers(&mut kawa, 0);
            assert_eq!(
                outcome.over_limit, over,
                "{count} fields against a bound of 4"
            );
            assert_eq!(kawa.is_error(), over, "{count} fields against a bound of 4");
        }

        // An elided field still counts.
        let bytes = [
            CHUNKED_HEAD,
            b"Host: a\r\nHost: b\r\nX-Forwarded-For: c\r\n\r\n".as_slice(),
        ]
        .concat();
        let mut kawa = parse_message(&mut pool, kawa::Kind::Request, &bytes);
        let mut context = make_context();
        context.max_trailer_fields = 2;
        let outcome = context.filter_request_trailers(&mut kawa, 0);
        assert!(outcome.over_limit && kawa.is_error(), "elided fields count");
    }

    /// The bound covers the whole trailer section, not one read: a client
    /// trickling one field per segment, each forwarded before the next
    /// arrives, is still refused once the running count passes the bound,
    /// and `reset` gives the next pipelined request a budget of its own.
    ///
    /// TO SEE THIS RED: count only the fields of the current walk (compare
    /// `fields` instead of `self.trailer_fields` to the bound).
    #[test]
    fn the_trailer_field_bound_spans_reads_and_resets_per_request() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 4096);
        let mut kawa = parse_message(&mut pool, kawa::Kind::Request, CHUNKED_HEAD);
        let mut context = make_context();
        context.max_trailer_fields = 2;
        assert_eq!(
            context.filter_request_trailers(&mut kawa, 0),
            Default::default()
        );
        for (i, line) in [&b"X-A: 1\r\n"[..], b"X-B: 2\r\n"].into_iter().enumerate() {
            let outcome = feed_and_filter(&mut context, &mut kawa, line);
            assert!(!outcome.over_limit && !kawa.is_error(), "field {i} fits");
            serialized_request(&mut kawa);
            assert!(kawa.blocks.is_empty(), "field {i} was forwarded");
        }
        let outcome = feed_and_filter(&mut context, &mut kawa, b"X-C: 3\r\n");
        assert!(
            outcome.over_limit && kawa.is_error(),
            "the third field goes over"
        );

        context.reset(Ulid::generate());
        assert_eq!(context.trailer_fields, 0, "reset clears the running count");
        assert_eq!(context.max_trailer_fields, 2, "reset keeps the bound");
    }
}
