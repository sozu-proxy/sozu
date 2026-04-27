use std::{
    collections::BTreeMap, fmt, fmt::Formatter, mem::ManuallyDrop, net::SocketAddr, time::Duration,
};

use rusty_ulid::Ulid;

use crate::{
    logging::{LogLevel, Rfc3339Time},
    proto::command::{
        HttpEndpoint, OpenTelemetry as ProtobufOpenTelemetry, ProtobufAccessLog, ProtobufEndpoint,
        TcpEndpoint, protobuf_endpoint,
    },
};

/// This uses unsafe to creates a "fake" owner of the underlying data.
/// Beware that for the compiler it is as legitimate as the original owner.
/// So you have to elide one of them (with std::mem::forget or ManuallyDrop)
/// before it is drop to avoid a double free.
///
/// This trait works on &T and Option<&T> types
///
/// After performance review, it seems not any more efficient than calling `clone()`,
/// probably because the cache of malloc is so well optimized these days.
trait DuplicateOwnership {
    type Target;
    /// Don't forget to use std::mem::forget or ManuallyDrop over one of your owners
    unsafe fn duplicate(self) -> Self::Target;
}

impl<T> DuplicateOwnership for &T {
    type Target = T;
    unsafe fn duplicate(self) -> T {
        // SAFETY: `std::ptr::read` duplicates the bit-pattern of the
        // pointee without copying. The result aliases the original value's
        // ownership view, so the caller MUST ensure exactly one of the two
        // owners is wrapped in `ManuallyDrop` or `mem::forget`'d before
        // either is dropped (cf. the type-level guidance at line 26).
        unsafe { std::ptr::read(self as *const T) }
    }
}
impl<'a, T> DuplicateOwnership for Option<&'a T>
where
    T: ?Sized,
    &'a T: DuplicateOwnership,
{
    type Target = Option<<&'a T as DuplicateOwnership>::Target>;
    unsafe fn duplicate(self) -> Self::Target {
        // SAFETY: delegates to the inner `&T::duplicate` impl above. The
        // same double-free obligation propagates to the caller (cf. the
        // type-level guidance at line 26).
        unsafe { self.map(|t| t.duplicate()) }
    }
}
impl DuplicateOwnership for &str {
    type Target = String;
    unsafe fn duplicate(self) -> Self::Target {
        // SAFETY: `String::from_raw_parts` reuses the underlying allocation
        // owned by `self` without copying, so the result aliases that
        // ownership view. Caller MUST ensure exactly one owner is wrapped
        // in `ManuallyDrop` or `mem::forget`'d to avoid double-free
        // (cf. the type-level guidance at line 26). The bytes are valid
        // UTF-8 because they came from a `&str`.
        unsafe { String::from_raw_parts(self.as_ptr() as *mut _, self.len(), self.len()) }
    }
}
impl<T> DuplicateOwnership for &[T] {
    type Target = Vec<T>;
    unsafe fn duplicate(self) -> Self::Target {
        // SAFETY: `Vec::from_raw_parts` reuses the underlying allocation
        // owned by `self` without copying, so the result aliases that
        // ownership view. Caller MUST ensure exactly one owner is wrapped
        // in `ManuallyDrop` or `mem::forget`'d to avoid double-free
        // (cf. the type-level guidance at line 26).
        unsafe { Vec::from_raw_parts(self.as_ptr() as *mut _, self.len(), self.len()) }
    }
}

pub struct LogMessage<'a>(pub Option<&'a str>);
pub struct LogDuration(pub Option<Duration>);

/// Prefix block attached to every log line. Rendered as
/// `[<session_id> <request_id_or_-> <cluster_id_or_-> <backend_id_or_->]`
/// so operators can grep a full TCP/TLS session (`session_id`) or drill
/// into one HTTP exchange (`request_id`).
///
/// - `session_id` is generated once per mio-accepted socket and survives
///   protocol upgrades (expect-proxy, TLS handshake, H1↔H2 multiplexing).
/// - `request_id` is per-request: distinct for each H2 stream, and
///   regenerated on each H1 keep-alive exchange.
#[derive(Debug)]
pub struct LogContext<'a> {
    pub session_id: Ulid,
    pub request_id: Option<Ulid>,
    pub cluster_id: Option<&'a str>,
    pub backend_id: Option<&'a str>,
}

pub enum EndpointRecord<'a> {
    Http {
        method: Option<&'a str>,
        authority: Option<&'a str>,
        path: Option<&'a str>,
        status: Option<u16>,
        reason: Option<&'a str>,
    },
    Tcp,
}

/// used to aggregate tags in a session
#[derive(Debug)]
pub struct CachedTags {
    pub tags: BTreeMap<String, String>,
    pub concatenated: String,
}

impl CachedTags {
    pub fn new(tags: BTreeMap<String, String>) -> Self {
        let concatenated = tags
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", ");
        Self { tags, concatenated }
    }
}

#[derive(Debug)]
pub struct FullTags<'a> {
    pub concatenated: Option<&'a str>,
    pub user_agent: Option<&'a str>,
}

#[derive(Default)]
pub struct OpenTelemetry {
    pub trace_id: [u8; 32],
    pub span_id: [u8; 16],
    pub parent_span_id: Option<[u8; 16]>,
}

impl fmt::Debug for OpenTelemetry {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        // SAFETY: `trace_id` is populated either by `parse_hex` (which
        // verifies every byte is `is_ascii_hexdigit`) or by `random_id`
        // (which fills from `b"0123456789abcdef"`). Both produce ASCII
        // bytes, which are by construction valid single-byte UTF-8.
        let trace_id = unsafe { std::str::from_utf8_unchecked(&self.trace_id) };
        // SAFETY: same provenance as `trace_id` above — `parse_hex` or
        // `random_id` from a hex charset, ASCII guaranteed.
        let span_id = unsafe { std::str::from_utf8_unchecked(&self.span_id) };
        let parent_span_id = self
            .parent_span_id
            .as_ref()
            // SAFETY: same provenance as `trace_id` above — `parse_hex`
            // from a hex charset, ASCII guaranteed.
            .map(|id| unsafe { std::str::from_utf8_unchecked(id) })
            .unwrap_or("-");
        write!(f, "{trace_id} {span_id} {parent_span_id}")
    }
}

/// Intermediate representation of an access log agnostic of the final format.
/// Every field is a reference to avoid capturing ownership (as a logger should).
pub struct RequestRecord<'a> {
    pub message: Option<&'a str>,
    pub context: LogContext<'a>,
    pub session_address: Option<SocketAddr>,
    pub backend_address: Option<SocketAddr>,
    pub protocol: &'a str,
    pub endpoint: EndpointRecord<'a>,
    pub tags: Option<&'a CachedTags>,
    pub client_rtt: Option<Duration>,
    pub server_rtt: Option<Duration>,
    pub user_agent: Option<&'a str>,
    /// Value of the `x-request-id` header forwarded to the backend. Preserved
    /// verbatim when the client supplied one; otherwise derived from the
    /// request ULID (`context.request_id`). Used by downstream observability
    /// pipelines as a universal correlation key.
    pub x_request_id: Option<&'a str>,
    /// Negotiated TLS protocol version short-form (e.g. `"TLSv1.3"`).
    /// Static-string borrow plumbed straight from rustls. `None` for
    /// plaintext listeners or when the version label is unknown.
    pub tls_version: Option<&'static str>,
    /// Negotiated TLS cipher suite short-form (e.g.
    /// `"TLS_AES_128_GCM_SHA256"`). Static-string borrow plumbed straight
    /// from rustls. `None` for plaintext listeners or when the cipher
    /// label is unknown.
    pub tls_cipher: Option<&'static str>,
    /// TLS Server Name Indication sent by the client at handshake (already
    /// pre-lowercased, no port). `None` for plaintext listeners or when the
    /// client omitted the SNI extension.
    pub tls_sni: Option<&'a str>,
    /// Negotiated ALPN protocol (e.g. `"h2"`, `"http/1.1"`). `None` for
    /// plaintext listeners or when no ALPN was negotiated.
    pub tls_alpn: Option<&'static str>,
    /// Verbatim value of the client-supplied `X-Forwarded-For` header as
    /// observed before Sōzu appended its own hop. `None` if the request had
    /// no `X-Forwarded-For` header.
    pub xff_chain: Option<&'a str>,
    pub service_time: Duration,
    /// time from connecting to the backend until the end of the response
    pub response_time: Option<Duration>,
    /// time between first byte of the request and last byte of the response
    pub request_time: Duration,
    pub bytes_in: usize,
    pub bytes_out: usize,
    pub otel: Option<&'a OpenTelemetry>,

    // added by the logger itself
    pub pid: i32,
    pub tag: &'a str,
    pub level: LogLevel,
    pub now: Rfc3339Time,
    pub precise_time: i128,
}

impl RequestRecord<'_> {
    pub fn full_tags(&self) -> FullTags<'_> {
        FullTags {
            concatenated: self.tags.as_ref().map(|t| t.concatenated.as_str()),
            user_agent: self.user_agent,
        }
    }

    /// Converts the RequestRecord in its protobuf representation.
    /// Prost needs ownership over all the fields but we don't want to take it from the user
    /// or clone them, so we use the unsafe DuplicateOwnership.
    pub fn into_binary_access_log(self) -> ManuallyDrop<ProtobufAccessLog> {
        // SAFETY: Each `.duplicate()` call below borrows ownership without
        // copying. The whole protobuf is wrapped in `ManuallyDrop` so its
        // destructor never runs — the original `RequestRecord` references
        // remain the sole owners. `std::str::from_utf8_unchecked` on `otel`
        // fields is sound because `trace_id` / `span_id` are ASCII hex
        // (see the `OpenTelemetry` Debug impl above for the same proof).
        unsafe {
            let endpoint = match self.endpoint {
                EndpointRecord::Http {
                    method,
                    authority,
                    path,
                    status,
                    reason,
                } => protobuf_endpoint::Inner::Http(HttpEndpoint {
                    method: method.duplicate(),
                    authority: authority.duplicate(),
                    path: path.duplicate(),
                    status: status.map(|s| s as u32),
                    reason: reason.duplicate(),
                }),
                EndpointRecord::Tcp => protobuf_endpoint::Inner::Tcp(TcpEndpoint {}),
            };

            ManuallyDrop::new(ProtobufAccessLog {
                backend_address: self.backend_address.map(Into::into),
                backend_id: self.context.backend_id.duplicate(),
                bytes_in: self.bytes_in as u64,
                bytes_out: self.bytes_out as u64,
                client_rtt: self.client_rtt.map(|t| t.as_micros() as u64),
                cluster_id: self.context.cluster_id.duplicate(),
                endpoint: ProtobufEndpoint {
                    inner: Some(endpoint),
                },
                message: self.message.duplicate(),
                protocol: self.protocol.duplicate(),
                request_id: self
                    .context
                    .request_id
                    .unwrap_or(self.context.session_id)
                    .into(),
                session_id: Some(self.context.session_id.into()),
                response_time: self.response_time.map(|t| t.as_micros() as u64),
                server_rtt: self.server_rtt.map(|t| t.as_micros() as u64),
                service_time: self.service_time.as_micros() as u64,
                session_address: self.session_address.map(Into::into),
                tags: self
                    .tags
                    .map(|tags| tags.tags.duplicate())
                    .unwrap_or_default(),
                user_agent: self.user_agent.duplicate(),
                x_request_id: self.x_request_id.duplicate(),
                tls_version: self.tls_version.duplicate(),
                tls_cipher: self.tls_cipher.duplicate(),
                tls_sni: self.tls_sni.duplicate(),
                tls_alpn: self.tls_alpn.duplicate(),
                xff_chain: self.xff_chain.duplicate(),
                tag: self.tag.duplicate(),
                time: self.precise_time.into(),
                request_time: Some(self.request_time.as_micros() as u64),
                otel: self.otel.map(|otel| ProtobufOpenTelemetry {
                    trace_id: std::str::from_utf8_unchecked(&otel.trace_id).duplicate(),
                    span_id: std::str::from_utf8_unchecked(&otel.span_id).duplicate(),
                    parent_span_id: otel
                        .parent_span_id
                        .as_ref()
                        .map(|id| std::str::from_utf8_unchecked(id).duplicate()),
                }),
            })
        }
    }
}
