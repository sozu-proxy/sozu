use std::{collections::BTreeMap, mem::ManuallyDrop, net::SocketAddr};

use rusty_ulid::Ulid;
use time::Duration;

use crate::{
    logging::{LogLevel, Rfc3339Time},
    proto::command::{
        protobuf_endpoint, HttpEndpoint, ProtobufAccessLog, ProtobufEndpoint, TcpEndpoint,
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
        std::ptr::read(self as *const T)
    }
}
impl<'a, T> DuplicateOwnership for Option<&'a T>
where
    T: ?Sized,
    &'a T: DuplicateOwnership,
{
    type Target = Option<<&'a T as DuplicateOwnership>::Target>;
    unsafe fn duplicate(self) -> Self::Target {
        self.map(|t| t.duplicate())
    }
}
impl DuplicateOwnership for &str {
    type Target = String;
    unsafe fn duplicate(self) -> Self::Target {
        String::from_raw_parts(self.as_ptr() as *mut _, self.len(), self.len())
    }
}
impl<T> DuplicateOwnership for &[T] {
    type Target = Vec<T>;
    unsafe fn duplicate(self) -> Self::Target {
        Vec::from_raw_parts(self.as_ptr() as *mut _, self.len(), self.len())
    }
}

pub struct LogMessage<'a>(pub Option<&'a str>);
pub struct LogDuration(pub Option<Duration>);

#[derive(Debug)]
pub struct LogContext<'a> {
    pub request_id: Ulid,
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
    pub service_time: Duration,
    pub response_time: Duration,
    pub bytes_in: usize,
    pub bytes_out: usize,

    // added by the logger itself
    pub pid: i32,
    pub tag: &'a str,
    pub level: LogLevel,
    pub now: Rfc3339Time,
    pub precise_time: i128,
}

impl RequestRecord<'_> {
    pub fn full_tags(&self) -> FullTags {
        FullTags {
            concatenated: self.tags.as_ref().map(|t| t.concatenated.as_str()),
            user_agent: self.user_agent,
        }
    }

    /// Converts the RequestRecord in its protobuf representation.
    /// Prost needs ownership over all the fields but we don't want to take it from the user
    /// or clone them, so we use the unsafe DuplicateOwnership.
    pub fn into_binary_access_log(self) -> ManuallyDrop<ProtobufAccessLog> {
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
                client_rtt: self.client_rtt.map(|t| t.whole_microseconds() as u64),
                cluster_id: self.context.cluster_id.duplicate(),
                endpoint: ProtobufEndpoint {
                    inner: Some(endpoint),
                },
                message: self.message.duplicate(),
                protocol: self.protocol.duplicate(),
                request_id: self.context.request_id.into(),
                response_time: self.response_time.whole_microseconds() as u64,
                server_rtt: self.server_rtt.map(|t| t.whole_microseconds() as u64),
                service_time: self.service_time.whole_microseconds() as u64,
                session_address: self.session_address.map(Into::into),
                tags: self
                    .tags
                    .map(|tags| tags.tags.duplicate())
                    .unwrap_or_default(),
                user_agent: self.user_agent.duplicate(),
                tag: self.tag.duplicate(),
                time: self.precise_time.into(),
            })
        }
    }
}
