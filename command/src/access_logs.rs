use std::{collections::BTreeMap, mem::ManuallyDrop, net::SocketAddr};

use rusty_ulid::Ulid;
use time::Duration;

use crate::proto::command::{ProtobufAccessLog, ProtobufEndpoint, TcpEndpoint, Uint128};

/// This uses unsafe to creates a "fake" owner of the underlying data.
/// Beware that for the compiler it is as legitimate as the original owner.
/// So you have to elide one of them (with std::mem::forget or ManuallyDrop)
/// before it is drop to avoid a double free.
///
/// This trait works on &T and Option<&T> types
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
    &'a T: DuplicateOwnership + 'a,
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

pub fn prepare_user_agent(user_agent: &str) -> String {
    let mut user_agent = user_agent.replace(' ', "_");
    let mut ua_bytes = std::mem::take(&mut user_agent).into_bytes();
    if let Some(last) = ua_bytes.last_mut() {
        if *last == b',' {
            *last = b'!'
        }
    }
    unsafe { String::from_utf8_unchecked(ua_bytes) }
}

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
    Tcp {
        context: Option<&'a str>,
    },
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

/// Intermediate representation of an access log agnostic of the final format.
/// Every field is a reference to avoid capturing ownership (as a logger should).
pub struct RequestRecord<'a> {
    pub error: &'a Option<&'a str>,
    pub context: &'a LogContext<'a>,
    pub session_address: &'a Option<SocketAddr>,
    pub backend_address: &'a Option<SocketAddr>,
    pub protocol: &'a str,
    pub endpoint: &'a EndpointRecord<'a>,
    pub tags: &'a Option<&'a CachedTags>,
    pub client_rtt: &'a Option<Duration>,
    pub server_rtt: &'a Option<Duration>,
    pub user_agent: &'a Option<String>,
    pub service_time: &'a Duration,
    pub response_time: &'a Duration,
    pub bytes_in: &'a usize,
    pub bytes_out: &'a usize,
}

impl RequestRecord<'_> {
    /// Converts the RequestRecord in its protobuf representation.
    /// Prost needs ownership over all the fields but we don't want to take it from the user
    /// or clone them, so we use the unsafe DuplicateOwnership.
    pub unsafe fn into_protobuf_access_log(
        self,
        time: i128,
        tag: &str,
    ) -> ManuallyDrop<ProtobufAccessLog> {
        let (low, high) = self.context.request_id.into();
        let request_id = Uint128 { low, high };
        let time: Uint128 = time.into();

        let endpoint = match self.endpoint {
            EndpointRecord::Http {
                method,
                authority,
                path,
                status,
                reason,
            } => crate::proto::command::protobuf_endpoint::Inner::Http(
                crate::proto::command::HttpEndpoint {
                    method: method.duplicate().duplicate(),
                    authority: authority.duplicate().duplicate(),
                    path: path.duplicate().duplicate(),
                    status: status.map(|s| s as u32),
                    reason: reason.duplicate().duplicate(),
                },
            ),
            EndpointRecord::Tcp { context } => {
                crate::proto::command::protobuf_endpoint::Inner::Tcp(TcpEndpoint {
                    context: context.duplicate().duplicate(),
                })
            }
        };

        ManuallyDrop::new(ProtobufAccessLog {
            backend_address: self.backend_address.map(Into::into),
            backend_id: self.context.backend_id.duplicate(),
            bytes_in: *self.bytes_in as u64,
            bytes_out: *self.bytes_out as u64,
            client_rtt: self.client_rtt.map(|t| t.whole_microseconds() as u64),
            cluster_id: self.context.cluster_id.duplicate(),
            endpoint: ProtobufEndpoint {
                inner: Some(endpoint),
            },
            error: self.error.duplicate().duplicate(),
            protocol: self.protocol.duplicate(),
            request_id,
            response_time: self.response_time.whole_microseconds() as u64,
            server_rtt: self.server_rtt.map(|t| t.whole_microseconds() as u64),
            service_time: self.service_time.whole_microseconds() as u64,
            session_address: self.session_address.map(Into::into),
            tags: self
                .tags
                .map(|tags| tags.tags.duplicate())
                .unwrap_or_default(),
            user_agent: self.user_agent.duplicate(),
            tag: tag.duplicate(),
            time: time.duplicate(),
        })
    }
}
