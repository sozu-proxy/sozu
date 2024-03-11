//! ## What this library does
//!
//! This library provides tools to build and start HTTP, HTTPS and TCP reverse proxies.
//!
//! The proxies handles network polling, HTTP parsing, TLS in a fast single threaded event
//! loop.
//!
//! Each proxy is designed to receive configuration changes at runtime instead of
//! reloading from a file regularly. The event loop runs in its own thread
//! and receives commands through a message queue.
//!
//! ## Difference with the crate `sozu`
//!
//! To create several workers and manage them all at once (which is the most common way to
//! use Sōzu), the crate `sozu` is more indicated than using the lib directly.
//!
//! The crate `sozu` provides a binary called the main process.
//! The main process uses `sozu_lib` to start and manage workers.
//! Each worker can handle HTTP, HTTPS and TCP traffic.
//! The main process receives synchronizes the state of all workers, using UNIX sockets
//! and custom channels to communicate with them.
//! The main process itself is is configurable with a file, and has a CLI.
//!
//! ## How to use this library directly
//!
//! This documentation here explains how to write a binary that will start a single Sōzu
//! worker and give it orders. The method has two steps:
//!
//! 1. Starts a Sōzu worker in a distinct thread
//! 2. sends instructions to the worker on a UNIX socket via a Sōzu channel
//!
//! ### How to start a Sōzu worker
//!
//! Before creating an HTTP proxy, we first need to create an HTTP listener.
//! The listener is an abstraction around a TCP socket provided by the kernel.
//! We need the `sozu_command_lib` to build a listener.
//!
//! ```
//! use sozu_command_lib::{config::ListenerBuilder, proto::command::SocketAddress};
//!
//! let address = SocketAddress::new_v4(127,0,0,1,8080);
//! let http_listener = ListenerBuilder::new_http(address)
//!     .to_http(None)
//!     .expect("Could not create HTTP listener");
//! ```
//!
//! The `http_listener` is of the type `HttpListenerConfig`, that we can be sent to the worker
//! to start the proxy.
//!
//! Then create a pair of channels to communicate with the proxy.
//! The channel is a wrapper around a unix socket.
//!
//! ```ignore
//! use sozu_command_lib::{
//!     channel::Channel,
//!     proto::command::{WorkerRequest, WorkerResponse},
//! };
//!
//! let (mut command_channel, proxy_channel): (
//!     Channel<WorkerRequest, WorkerResponse>,
//!     Channel<WorkerResponse, WorkerRequest>,
//! ) = Channel::generate(1000, 10000).expect("should create a channel");
//!```
//!
//! Here, the `command_channel` end is blocking, it sends `WorkerRequest`s and receives
//! `WorkerResponses`, while the `proxy_channel` end is non-blocking, and the types are reversed.
//! Writing the types here isn't even necessary thanks to the compiler,
//! but it brings the point accross.
//!
//! You can now launch the worker in a separate thread, providing the HTTP listener config,
//! the proxy end of the channel, and your custom number of buffers and their size:
//!
//! ```ignore
//! use std::thread;
//!
//! let worker_thread_join_handle = thread::spawn(move || {
//!     let max_buffers = 500;
//!     let buffer_size = 16384;
//!     sozu_lib::http::testing::start_http_worker(http_listener, proxy_channel, max_buffers, buffer_size);
//! });
//! ```
//!
//! ### Send orders
//!
//! Once the thread is launched, the proxy worker will start its event loop and handle
//! events on the listening interface and port specified when building the HTTP Listener.
//! Since no frontends or backends were specified for the proxy, it will receive
//! the connections, parse the requests, then send a default (but configurable)
//! answer.
//!
//! Before defining a frontend and backends, we need to define a cluster, which describes
//! a routing configuration. A cluster contains:
//!
//! - one frontend
//! - one or several backends
//! - routing rules
//!
//! A cluster is identified by its `cluster_id`, which will be used to define frontends
//! and backends later on.
//!
//! ```
//! use sozu_command_lib::proto::command::{Cluster, LoadBalancingAlgorithms};
//!
//! let cluster = Cluster {
//!     cluster_id: "my-cluster".to_string(),
//!     sticky_session: false,
//!     https_redirect: false,
//!     load_balancing: LoadBalancingAlgorithms::RoundRobin as i32,
//!     answer_503: Some("A custom forbidden message".to_string()),
//!     ..Default::default()
//! };
//! ```
//!
//! The defaults are sensible, so we could define only the `cluster_id`.
//!
//! We can now define a frontend. A frontend is a way to recognize a request and match
//! it to a `cluster_id`, depending on the hostname and the beginning of the URL path.
//! The `address` field must match the one of the HTTP listener we defined before:
//!
//! ```
//! use std::collections::BTreeMap;
//!
//!  use sozu_command_lib::proto::command::{PathRule, RequestHttpFrontend, RulePosition, SocketAddress};
//!
//! let http_front = RequestHttpFrontend {
//!     cluster_id: Some("my-cluster".to_string()),
//!     address: SocketAddress::new_v4(127,0,0,1,8080),
//!     hostname: "example.com".to_string(),
//!     path: PathRule::prefix(String::from("/")),
//!     position: RulePosition::Pre.into(),
//!     tags: BTreeMap::from([
//!         ("owner".to_owned(), "John".to_owned()),
//!         ("id".to_owned(), "my-own-http-front".to_owned()),
//!     ]),
//!     ..Default::default()
//! };
//! ```
//!
//! The `tags` are keys and values that will appear in the access logs,
//! which can come in handy.
//!
//! Now let's define a backend.
//! A backend is an instance of a backend application we want to route traffic to.
//! The `address` field must match the IP and port of the backend server.
//!
//! ```
//! use sozu_command_lib::proto::command::{AddBackend, LoadBalancingParams, SocketAddress};
//!
//! let http_backend = AddBackend {
//!     cluster_id: "my-cluster".to_string(),
//!     backend_id: "test-backend".to_string(),
//!     address: SocketAddress::new_v4(127,0,0,1,8000),
//!     load_balancing_parameters: Some(LoadBalancingParams::default()),
//!     ..Default::default()
//! };
//! ```
//!
//! A cluster can have multiple backend servers, and they can be added or
//! removed while the proxy is running. If a backend is removed from the configuration
//! while the proxy is handling a request to that server, it will finish that
//! request and stop sending new traffic to that server.
//!
//!
//! Now we can use the other end of the channel to send all these requests to the worker,
//! using the WorkerRequest type:
//!
//! ```ignore
//! use sozu_command_lib::{
//!     proto::command::{Request, request::RequestType, WorkerRequest},
//! };
//!
//! command_channel
//!     .write_message(&WorkerRequest {
//!         id: String::from("add-the-cluster"),
//!         content: RequestType::AddCluster(cluster).into(),
//!     })
//!     .expect("Could not send AddHttpFrontend request");
//!
//! command_channel
//!     .write_message(&WorkerRequest {
//!         id: String::from("add-the-frontend"),
//!         content: RequestType::AddHttpFrontend(http_front).into(),
//!     })
//!     .expect("Could not send AddHttpFrontend request");
//!
//! command_channel
//!     .write_message(&WorkerRequest {
//!         id: String::from("add-the-backend"),
//!         content: RequestType::AddBackend(http_backend).into(),
//!     })
//!     .expect("Could not send AddBackend request");
//!
//! println!("HTTP -> {:?}", command_channel.read_message());
//! println!("HTTP -> {:?}", command_channel.read_message());
//! println!("HTTP -> {:?}", command_channel.read_message());
//! ```
//!
//!
//! The event loop of the worker will process these instructions and add them to
//! its state, and the worker will send back an acknowledgement
//! message.
//!
//! Now we can let the worker thread run in the background:
//!
//! ```ignore
//! let _ = worker_thread_join_handle.join();
//! ```
//!
//! Here is the complete example for reference, it matches the `examples/http.rs` example:
//!
//! ```
//! #[macro_use]
//! extern crate sozu_command_lib;
//!
//! use std::{collections::BTreeMap, env, io::stdout, thread};
//!
//! use anyhow::Context;
//! use sozu_command_lib::{
//!     channel::Channel,
//!     config::ListenerBuilder,
//!     logging::setup_default_logging,
//!     proto::command::{
//!         request::RequestType, AddBackend, Cluster, LoadBalancingAlgorithms, LoadBalancingParams,
//!         PathRule, Request, RequestHttpFrontend, RulePosition, SocketAddress,WorkerRequest,
//!     },
//! };
//!
//! fn main() -> anyhow::Result<()> {
//!     setup_default_logging(true, "info", "EXAMPLE");
//!
//!     info!("starting up");
//!
//!     let http_listener = ListenerBuilder::new_http(SocketAddress::new_v4(127,0,0,1,8080))
//!         .to_http(None)
//!         .expect("Could not create HTTP listener");
//!
//!     let (mut command_channel, proxy_channel) =
//!         Channel::generate(1000, 10000).with_context(|| "should create a channel")?;
//!
//!     let worker_thread_join_handle = thread::spawn(move || {
//!         let max_buffers = 500;
//!         let buffer_size = 16384;
//!         sozu_lib::http::testing::start_http_worker(http_listener, proxy_channel, max_buffers, buffer_size)
//!             .expect("The worker could not be started, or shut down");
//!     });
//!
//!     let cluster = Cluster {
//!         cluster_id: "my-cluster".to_string(),
//!         sticky_session: false,
//!         https_redirect: false,
//!         load_balancing: LoadBalancingAlgorithms::RoundRobin as i32,
//!         answer_503: Some("A custom forbidden message".to_string()),
//!         ..Default::default()
//!     };
//!
//!     let http_front = RequestHttpFrontend {
//!         cluster_id: Some("my-cluster".to_string()),
//!         address: SocketAddress::new_v4(127,0,0,1,8080),
//!         hostname: "example.com".to_string(),
//!         path: PathRule::prefix(String::from("/")),
//!         position: RulePosition::Pre.into(),
//!         tags: BTreeMap::from([
//!             ("owner".to_owned(), "John".to_owned()),
//!             ("id".to_owned(), "my-own-http-front".to_owned()),
//!         ]),
//!         ..Default::default()
//!     };
//!     let http_backend = AddBackend {
//!         cluster_id: "my-cluster".to_string(),
//!         backend_id: "test-backend".to_string(),
//!         address: SocketAddress::new_v4(127,0,0,1,8000),
//!         load_balancing_parameters: Some(LoadBalancingParams::default()),
//!         ..Default::default()
//!     };
//!
//!     command_channel
//!         .write_message(&WorkerRequest {
//!             id: String::from("add-the-cluster"),
//!             content: RequestType::AddCluster(cluster).into(),
//!         })
//!         .expect("Could not send AddHttpFrontend request");
//!
//!     command_channel
//!         .write_message(&WorkerRequest {
//!             id: String::from("add-the-frontend"),
//!             content: RequestType::AddHttpFrontend(http_front).into(),
//!         })
//!         .expect("Could not send AddHttpFrontend request");
//!
//!     command_channel
//!         .write_message(&WorkerRequest {
//!             id: String::from("add-the-backend"),
//!             content: RequestType::AddBackend(http_backend).into(),
//!         })
//!         .expect("Could not send AddBackend request");
//!
//!     println!("HTTP -> {:?}", command_channel.read_message());
//!     println!("HTTP -> {:?}", command_channel.read_message());
//!
//!     // uncomment to let it run in the background
//!     // let _ = worker_thread_join_handle.join();
//!     info!("good bye");
//!     Ok(())
//! }
//! ```

#[macro_use]
extern crate sozu_command_lib as sozu_command;
#[cfg(test)]
#[macro_use]
extern crate quickcheck;

#[macro_use]
pub mod util;
#[macro_use]
pub mod metrics;

pub mod backends;
pub mod features;
pub mod http;
pub mod load_balancing;
pub mod pool;
pub mod protocol;
pub mod retry;
pub mod router;
pub mod socket;
pub mod timer;
pub mod tls;

/// unused for now but may be usefull for bypassing sozu on a low level
#[cfg(feature = "splice")]
mod splice;

pub mod server;
pub mod tcp;

pub mod https;

use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    fmt,
    net::SocketAddr,
    rc::Rc,
    str,
};

use backends::BackendError;
use hex::FromHexError;
use mio::{net::TcpStream, Interest, Token};
use protocol::http::parser::Method;
use router::RouterError;
use socket::ServerBindError;
use time::{Duration, Instant};
use tls::CertificateResolverError;

use sozu_command::{
    logging::{CachedTags, LogContext},
    proto::command::{Cluster, ListenerType, RequestHttpFrontend, WorkerRequest, WorkerResponse},
    ready::Ready,
    state::ClusterId,
    AsStr, ObjectKind,
};

use crate::{backends::BackendMap, router::Route};

/// Anything that can be registered in mio (subscribe to kernel events)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    HTTP,
    HTTPS,
    TCP,
    HTTPListen,
    HTTPSListen,
    TCPListen,
    Channel,
    Metrics,
    Timer,
}

/// trait that must be implemented by listeners and client sessions
pub trait ProxySession {
    /// indicates the protocol associated with the session
    ///
    /// this is used to distinguish sessions from listenrs, channels, metrics
    /// and timers
    fn protocol(&self) -> Protocol;
    /// if a session received an event or can still execute, the event loop will
    /// call this method. Its result indicates if it can still execute, needs to
    /// connect to a backend server, close the session
    fn ready(&mut self, session: Rc<RefCell<dyn ProxySession>>) -> SessionIsToBeClosed;
    /// if the event loop got an event for a token associated with the session,
    /// it will call this method on the session
    fn update_readiness(&mut self, token: Token, events: Ready);
    /// close a session, frontend and backend sockets,
    /// remove the entries from the session manager slab
    fn close(&mut self);
    /// if a timeout associated with the session triggers, the event loop will
    /// call this method with the timeout's token
    fn timeout(&mut self, t: Token) -> SessionIsToBeClosed;
    /// last time the session got an event
    fn last_event(&self) -> Instant;
    /// display the session's internal state (for debugging purpose)
    fn print_session(&self);
    /// get the token associated with the frontend
    fn frontend_token(&self) -> Token;
    /// tell the session it has to shut down if possible
    ///
    /// if the session handles HTTP requests, it will not close until the response
    /// is completely sent back to the client
    fn shutting_down(&mut self) -> SessionIsToBeClosed;
}

#[macro_export]
macro_rules! branch {
    (if $($value:ident)? == $expected:ident { $($then:tt)* } else { $($else:tt)* }) => {
        macro_rules! expect {
            ($expected) => {$($then)*};
            ($a:ident) => {$($else)*};
            () => {$($else)*}
        }
        expect!($($value)?);
    };
    (if $($value:ident)? == $expected:ident { $($then:tt)* } ) => {
        macro_rules! expect {
            ($expected) => {$($then)*};
        }
        expect!($($value)?);
    };
}

#[macro_export]
macro_rules! fallback {
    ({} $($default:tt)*) => {
        $($default)*
    };
    ({$($value:tt)+} $($default:tt)*) => {
        $($value)+
    };
}

#[macro_export]
macro_rules! StateMachineBuilder {
    (
        ($d:tt)
        $(#[$($state_macros:tt)*])*
        enum $state_name:ident $(impl $trait:ident)?  {
            $($(#[$($variant_macros:tt)*])*
            $variant_name:ident($state:ty$(,$($aux:ty),+)?) $(-> $override:expr)?),+ $(,)?
        }
    ) => {
        /// A summary of the last valid State
        #[derive(Clone, Copy, Debug)]
        pub enum StateMarker {
            $($variant_name,)+
        }

        $(#[$($state_macros)*])*
        pub enum $state_name {
            $(
                $(#[$($variant_macros)*])*
                $variant_name($state$(,$($aux),+)?),
            )+
            /// Informs about upgrade failure, contains a summary the last valid State
            FailedUpgrade(StateMarker),
        }

        macro_rules! _fn_impl {
            ($function:ident(&$d($mut:ident)?, self $d(,$arg_name:ident: $arg_type:ty)*) $d(-> $ret:ty)? $d(| $marker:tt => $fail:expr)?) => {
                fn $function(&$d($mut)? self $d(,$arg_name: $arg_type)*) $d(-> $ret)? {
                    match self {
                        $($state_name::$variant_name(_state, ..) => $crate::fallback!({$($override)?} _state.$function($d($arg_name),*)),)+
                        $state_name::FailedUpgrade($crate::fallback!({$d($marker)?} _)) => $crate::fallback!({$d($fail)?} unreachable!())
                    }
                }
            };
        }

        impl $state_name {
            /// Informs about the last valid State before upgrade failure
            fn marker(&self) -> StateMarker {
                match self {
                    $($state_name::$variant_name(..) => StateMarker::$variant_name,)+
                    $state_name::FailedUpgrade(marker) => *marker,
                }
            }
            /// Returns wether or not the State is FailedUpgrade
            fn failed(&self) -> bool {
                match self {
                    $state_name::FailedUpgrade(_) => true,
                    _ => false,
                }
            }
            /// Gives back an owned version of the State,
            /// leaving a FailedUpgrade in its place.
            /// The FailedUpgrade retains the marker of the previous State.
            fn take(&mut self) -> $state_name {
                let mut owned_state = $state_name::FailedUpgrade(self.marker());
                std::mem::swap(&mut owned_state, self);
                owned_state
            }
            _fn_impl!{front_socket(&, self) -> &mio::net::TcpStream}
        }

        $crate::branch!{
            if $($trait)? == SessionState {
                impl SessionState for $state_name {
                    _fn_impl!{ready(&mut, self, session: Rc<RefCell<dyn ProxySession>>, proxy: Rc<RefCell<dyn L7Proxy>>, metrics: &mut SessionMetrics) -> SessionResult}
                    _fn_impl!{update_readiness(&mut, self, token: Token, events: Ready)}
                    _fn_impl!{timeout(&mut, self, token: Token, metrics: &mut SessionMetrics) -> StateResult}
                    _fn_impl!{cancel_timeouts(&mut, self)}
                    _fn_impl!{print_state(&, self, context: &str) | marker => error!("{} Session(FailedUpgrade({:?}))", context, marker)}
                    _fn_impl!{close(&mut, self, proxy: Rc<RefCell<dyn L7Proxy>>, metrics: &mut SessionMetrics) | _ => {}}
                    _fn_impl!{shutting_down(&mut, self) -> SessionIsToBeClosed | _ => true}
                }
            } else {}
        }
    };
    ($($tt:tt)+) => {
        StateMachineBuilder!{($) $($tt)+}
    }
}

pub trait ListenerHandler {
    fn get_addr(&self) -> &SocketAddr;

    fn get_tags(&self, key: &str) -> Option<&CachedTags>;

    fn get_concatenated_tags(&self, key: &str) -> Option<&str> {
        self.get_tags(key).map(|tags| tags.concatenated.as_str())
    }

    fn set_tags(&mut self, key: String, tags: Option<BTreeMap<String, String>>);
}

#[derive(thiserror::Error, Debug)]
pub enum FrontendFromRequestError {
    #[error("Could not parse hostname from '{host}': {error}")]
    HostParse { host: String, error: String },
    #[error("invalid remaining chars after hostname. Host: {0}")]
    InvalidCharsAfterHost(String),
    #[error("no cluster: {0}")]
    NoClusterFound(RouterError),
}

pub trait L7ListenerHandler {
    fn get_sticky_name(&self) -> &str;

    fn get_connect_timeout(&self) -> u32;

    /// retrieve a frontend by parsing a request's hostname, uri and method
    fn frontend_from_request(
        &self,
        host: &str,
        uri: &str,
        method: &Method,
    ) -> Result<Route, FrontendFromRequestError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendConnectionStatus {
    NotConnected,
    Connecting(Instant),
    Connected,
}

impl BackendConnectionStatus {
    pub fn is_connecting(&self) -> bool {
        matches!(self, BackendConnectionStatus::Connecting(_))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum BackendConnectAction {
    New,
    Reuse,
    Replace,
}

#[derive(thiserror::Error, Debug)]
pub enum BackendConnectionError {
    #[error("Not found: {0:?}")]
    NotFound(ObjectKind),
    #[error("Too many connections on cluster {0:?}")]
    MaxConnectionRetries(Option<String>),
    #[error("the sessions slab has reached maximum capacity")]
    MaxSessionsMemory,
    #[error("error from the backend: {0}")]
    Backend(BackendError),
    #[error("failed to retrieve the cluster: {0}")]
    RetrieveClusterError(RetrieveClusterError),
}

/// used in kawa_h1 module for the Http session state
#[derive(thiserror::Error, Debug)]
pub enum RetrieveClusterError {
    #[error("No method given")]
    NoMethod,
    #[error("No host given")]
    NoHost,
    #[error("No path given")]
    NoPath,
    #[error("unauthorized route")]
    UnauthorizedRoute,
    #[error("{0}")]
    RetrieveFrontend(FrontendFromRequestError),
}

/// Used in sessions
#[derive(Debug, PartialEq, Eq)]
pub enum AcceptError {
    IoError,
    TooManySessions,
    WouldBlock,
    RegisterError,
    WrongSocketAddress,
    BufferCapacityReached,
}

/// returned by the HTTP, HTTPS and TCP listeners
#[derive(thiserror::Error, Debug)]
pub enum ListenerError {
    #[error("failed to handle certificate request, got a resolver error, {0}")]
    Resolver(CertificateResolverError),
    #[error("failed to parse pem, {0}")]
    PemParse(String),
    #[error("failed to build rustls context, {0}")]
    BuildRustls(String),
    #[error("could not activate listener with address {address:?}: {error}")]
    Activation { address: SocketAddr, error: String },
    #[error("Could not register listener socket: {0}")]
    SocketRegistration(std::io::Error),
    #[error("could not add frontend: {0}")]
    AddFrontend(RouterError),
    #[error("could not remove frontend: {0}")]
    RemoveFrontend(RouterError),
}

/// Returned by the HTTP, HTTPS and TCP proxies
#[derive(thiserror::Error, Debug)]
pub enum ProxyError {
    #[error("error while soft stopping {proxy_protocol} proxy: {error}")]
    SoftStop {
        proxy_protocol: String,
        error: String,
    },
    #[error("error while hard stopping {proxy_protocol} proxy: {error}")]
    HardStop {
        proxy_protocol: String,
        error: String,
    },
    #[error("found no listener with address {0:?}")]
    NoListenerFound(SocketAddr),
    #[error("a listener is already present for this token")]
    ListenerAlreadyPresent,
    #[error("could not add listener: {0}")]
    AddListener(ListenerError),
    #[error("failed to activate listener with address {address:?}: {listener_error}")]
    ListenerActivation {
        address: SocketAddr,
        listener_error: ListenerError,
    },
    #[error("can not add frontend {front:?}: {error}")]
    WrongInputFrontend {
        front: RequestHttpFrontend,
        error: String,
    },
    #[error("could not add frontend: {0}")]
    AddFrontend(ListenerError),
    #[error("could not remove frontend: {0}")]
    RemoveFrontend(ListenerError),
    #[error("could not add certificate: {0}")]
    AddCertificate(CertificateResolverError),
    #[error("could not remove certificate: {0}")]
    RemoveCertificate(CertificateResolverError),
    #[error("could not replace certificate: {0}")]
    ReplaceCertificate(CertificateResolverError),
    #[error("wrong certificate fingerprint: {0}")]
    WrongCertificateFingerprint(FromHexError),
    #[error("this request is not supported by the proxy")]
    UnsupportedMessage,
    #[error("failed to acquire the lock, {0}")]
    Lock(String),
    #[error("could not bind to socket {0:?}: {1}")]
    BindToSocket(SocketAddr, ServerBindError),
    #[error("error registering socket of listener: {0}")]
    RegisterListener(std::io::Error),
    #[error("the listener is not activated")]
    UnactivatedListener,
}

use self::server::ListenToken;
pub trait ProxyConfiguration {
    fn notify(&mut self, message: WorkerRequest) -> WorkerResponse;
    fn accept(&mut self, token: ListenToken) -> Result<TcpStream, AcceptError>;
    fn create_session(
        &mut self,
        socket: TcpStream,
        token: ListenToken,
        wait_time: Duration,
        proxy: Rc<RefCell<Self>>,
        // should we insert the tags here?
    ) -> Result<(), AcceptError>;
}

pub trait L7Proxy {
    fn kind(&self) -> ListenerType;

    fn register_socket(
        &self,
        socket: &mut TcpStream,
        token: Token,
        interest: Interest,
    ) -> Result<(), std::io::Error>;

    fn deregister_socket(&self, tcp_stream: &mut TcpStream) -> Result<(), std::io::Error>;

    fn add_session(&self, session: Rc<RefCell<dyn ProxySession>>) -> Token;

    /// Remove the session from the session manager slab.
    /// Returns true if the session was actually there before deletion
    fn remove_session(&self, token: Token) -> bool;

    fn backends(&self) -> Rc<RefCell<BackendMap>>;

    fn clusters(&self) -> &HashMap<ClusterId, Cluster>;
}

#[derive(Debug, PartialEq, Eq)]
pub enum RequiredEvents {
    FrontReadBackNone,
    FrontWriteBackNone,
    FrontReadWriteBackNone,
    FrontNoneBackNone,
    FrontReadBackRead,
    FrontWriteBackRead,
    FrontReadWriteBackRead,
    FrontNoneBackRead,
    FrontReadBackWrite,
    FrontWriteBackWrite,
    FrontReadWriteBackWrite,
    FrontNoneBackWrite,
    FrontReadBackReadWrite,
    FrontWriteBackReadWrite,
    FrontReadWriteBackReadWrite,
    FrontNoneBackReadWrite,
}

impl RequiredEvents {
    pub fn front_readable(&self) -> bool {
        matches!(
            *self,
            RequiredEvents::FrontReadBackNone
                | RequiredEvents::FrontReadWriteBackNone
                | RequiredEvents::FrontReadBackRead
                | RequiredEvents::FrontReadWriteBackRead
                | RequiredEvents::FrontReadBackWrite
                | RequiredEvents::FrontReadWriteBackWrite
                | RequiredEvents::FrontReadBackReadWrite
                | RequiredEvents::FrontReadWriteBackReadWrite
        )
    }

    pub fn front_writable(&self) -> bool {
        matches!(
            *self,
            RequiredEvents::FrontWriteBackNone
                | RequiredEvents::FrontReadWriteBackNone
                | RequiredEvents::FrontWriteBackRead
                | RequiredEvents::FrontReadWriteBackRead
                | RequiredEvents::FrontWriteBackWrite
                | RequiredEvents::FrontReadWriteBackWrite
                | RequiredEvents::FrontWriteBackReadWrite
                | RequiredEvents::FrontReadWriteBackReadWrite
        )
    }

    pub fn back_readable(&self) -> bool {
        matches!(
            *self,
            RequiredEvents::FrontReadBackRead
                | RequiredEvents::FrontWriteBackRead
                | RequiredEvents::FrontReadWriteBackRead
                | RequiredEvents::FrontNoneBackRead
                | RequiredEvents::FrontReadBackReadWrite
                | RequiredEvents::FrontWriteBackReadWrite
                | RequiredEvents::FrontReadWriteBackReadWrite
                | RequiredEvents::FrontNoneBackReadWrite
        )
    }

    pub fn back_writable(&self) -> bool {
        matches!(
            *self,
            RequiredEvents::FrontReadBackWrite
                | RequiredEvents::FrontWriteBackWrite
                | RequiredEvents::FrontReadWriteBackWrite
                | RequiredEvents::FrontNoneBackWrite
                | RequiredEvents::FrontReadBackReadWrite
                | RequiredEvents::FrontWriteBackReadWrite
                | RequiredEvents::FrontReadWriteBackReadWrite
                | RequiredEvents::FrontNoneBackReadWrite
        )
    }
}

/// Signals transitions between states of a given Protocol
#[derive(Debug, PartialEq, Eq)]
pub enum StateResult {
    /// Signals to the Protocol to close its backend
    CloseBackend,
    /// Signals to the parent Session to close itself
    CloseSession,
    /// Signals to the Protocol to connect to backend
    ConnectBackend,
    /// Signals to the Protocol to continue
    Continue,
    /// Signals to the parent Session to upgrade to the next Protocol
    Upgrade,
}

/// Signals transitions between states of a given Session
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionResult {
    /// Signals to the Session to close itself
    Close,
    /// Signals to the Session to continue
    Continue,
    /// Signals to the Session to upgrade its Protocol
    Upgrade,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SocketType {
    Listener,
    FrontClient,
}

type SessionIsToBeClosed = bool;

#[derive(Clone)]
pub struct Readiness {
    /// the current readiness
    pub event: Ready,
    /// the readiness we wish to attain
    pub interest: Ready,
}

impl Default for Readiness {
    fn default() -> Self {
        Self::new()
    }
}

impl Readiness {
    pub const fn new() -> Readiness {
        Readiness {
            event: Ready::EMPTY,
            interest: Ready::EMPTY,
        }
    }

    pub fn reset(&mut self) {
        self.event = Ready::EMPTY;
        self.interest = Ready::EMPTY;
    }

    /// filters the readiness we actually want
    pub fn filter_interest(&self) -> Ready {
        self.event & self.interest
    }
}

pub fn display_ready(s: &mut [u8], readiness: Ready) {
    if readiness.is_readable() {
        s[0] = b'R';
    }
    if readiness.is_writable() {
        s[1] = b'W';
    }
    if readiness.is_error() {
        s[2] = b'E';
    }
    if readiness.is_hup() {
        s[3] = b'H';
    }
}

pub fn ready_to_string(readiness: Ready) -> String {
    let s = &mut [b'-'; 4];
    display_ready(s, readiness);
    String::from_utf8(s.to_vec()).unwrap()
}

impl fmt::Debug for Readiness {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let i = &mut [b'-'; 4];
        let r = &mut [b'-'; 4];
        let mixed = &mut [b'-'; 4];

        display_ready(i, self.interest);
        display_ready(r, self.event);
        display_ready(mixed, self.interest & self.event);

        write!(
            f,
            "Readiness {{ interest: {}, readiness: {}, mixed: {} }}",
            str::from_utf8(i).unwrap(),
            str::from_utf8(r).unwrap(),
            str::from_utf8(mixed).unwrap()
        )
    }
}

#[derive(Clone, Debug)]
pub struct SessionMetrics {
    /// date at which we started handling that request
    pub start: Option<Instant>,
    /// time actually spent handling the request
    pub service_time: Duration,
    /// time spent waiting for its turn
    pub wait_time: Duration,
    /// bytes received by the frontend
    pub bin: usize,
    /// bytes sent by the frontend
    pub bout: usize,

    /// date at which we started working on the request
    pub service_start: Option<Instant>,
    pub wait_start: Instant,

    pub backend_id: Option<String>,
    pub backend_start: Option<Instant>,
    pub backend_connected: Option<Instant>,
    pub backend_stop: Option<Instant>,
    pub backend_bin: usize,
    pub backend_bout: usize,
}

impl SessionMetrics {
    pub fn new(wait_time: Option<Duration>) -> SessionMetrics {
        SessionMetrics {
            start: Some(Instant::now()),
            service_time: Duration::seconds(0),
            wait_time: wait_time.unwrap_or_else(|| Duration::seconds(0)),
            bin: 0,
            bout: 0,
            service_start: None,
            wait_start: Instant::now(),
            backend_id: None,
            backend_start: None,
            backend_connected: None,
            backend_stop: None,
            backend_bin: 0,
            backend_bout: 0,
        }
    }

    pub fn reset(&mut self) {
        self.start = None;
        self.service_time = Duration::seconds(0);
        self.wait_time = Duration::seconds(0);
        self.bin = 0;
        self.bout = 0;
        self.service_start = None;
        self.backend_start = None;
        self.backend_connected = None;
        self.backend_stop = None;
        self.backend_bin = 0;
        self.backend_bout = 0;
    }

    pub fn service_start(&mut self) {
        let now = Instant::now();

        if self.start.is_none() {
            self.start = Some(now);
        }

        self.service_start = Some(now);
        self.wait_time += now - self.wait_start;
    }

    pub fn service_stop(&mut self) {
        if let Some(start) = self.service_start.take() {
            let duration = Instant::now() - start;
            self.service_time += duration;
        }
    }

    pub fn wait_start(&mut self) {
        self.wait_start = Instant::now();
    }

    pub fn service_time(&self) -> Duration {
        match self.service_start {
            Some(start) => {
                let last_duration = Instant::now() - start;
                self.service_time + last_duration
            }
            None => self.service_time,
        }
    }

    pub fn response_time(&self) -> Duration {
        match self.start {
            Some(start) => Instant::now() - start,
            None => Duration::seconds(0),
        }
    }

    pub fn backend_start(&mut self) {
        self.backend_start = Some(Instant::now());
    }

    pub fn backend_connected(&mut self) {
        self.backend_connected = Some(Instant::now());
    }

    pub fn backend_stop(&mut self) {
        self.backend_stop = Some(Instant::now());
    }

    pub fn backend_response_time(&self) -> Option<Duration> {
        match (self.backend_connected, self.backend_stop) {
            (Some(start), Some(end)) => Some(end - start),
            (Some(start), None) => Some(Instant::now() - start),
            _ => None,
        }
    }

    pub fn backend_connection_time(&self) -> Option<Duration> {
        match (self.backend_start, self.backend_connected) {
            (Some(start), Some(end)) => Some(end - start),
            _ => None,
        }
    }

    pub fn register_end_of_session(&self, context: &LogContext) {
        let response_time = self.response_time();
        let service_time = self.service_time();

        if let Some(cluster_id) = context.cluster_id {
            time!(
                "response_time",
                cluster_id,
                response_time.whole_milliseconds()
            );
            time!(
                "service_time",
                cluster_id,
                service_time.whole_milliseconds()
            );
        }
        time!("response_time", response_time.whole_milliseconds());
        time!("service_time", service_time.whole_milliseconds());

        if let Some(backend_id) = self.backend_id.as_ref() {
            if let Some(backend_response_time) = self.backend_response_time() {
                record_backend_metrics!(
                    context.cluster_id.as_str_or("-"),
                    backend_id,
                    backend_response_time.whole_milliseconds(),
                    self.backend_connection_time(),
                    self.backend_bin,
                    self.backend_bout
                );
            }
        }

        incr!("access_logs.count", context.cluster_id, context.backend_id);
    }
}

/// exponentially weighted moving average with high sensibility to latency bursts
///
/// cf Finagle for the original implementation: <https://github.com/twitter/finagle/blob/9cc08d15216497bb03a1cafda96b7266cfbbcff1/finagle-core/src/main/scala/com/twitter/finagle/loadbalancer/PeakEwma.scala>
#[derive(Debug, PartialEq, Clone)]
pub struct PeakEWMA {
    /// decay in nanoseconds
    ///
    /// higher values will make the EWMA decay slowly to 0
    pub decay: f64,
    /// estimated RTT in nanoseconds
    ///
    /// must be set to a high enough default value so that new backends do not
    /// get all the traffic right away
    pub rtt: f64,
    /// last modification
    pub last_event: Instant,
}

impl Default for PeakEWMA {
    fn default() -> Self {
        Self::new()
    }
}

impl PeakEWMA {
    // hardcoded default values for now
    pub fn new() -> Self {
        PeakEWMA {
            // 1s
            decay: 1_000_000_000f64,
            // 50ms
            rtt: 50_000_000f64,
            last_event: Instant::now(),
        }
    }

    pub fn observe(&mut self, rtt: f64) {
        let now = Instant::now();
        let dur = now - self.last_event;

        // if latency is rising, we will immediately raise the cost
        if rtt > self.rtt {
            self.rtt = rtt;
        } else {
            // new_rtt = old_rtt * e^(-elapsed/decay) + observed_rtt * (1 - e^(-elapsed/decay))
            let weight = (-1.0 * dur.whole_nanoseconds() as f64 / self.decay).exp();
            self.rtt = self.rtt * weight + rtt * (1.0 - weight);
        }

        self.last_event = now;
    }

    pub fn get(&mut self, active_requests: usize) -> f64 {
        // decay the current value
        // (we might not have seen a request in a long time)
        self.observe(0.0);

        (active_requests + 1) as f64 * self.rtt
    }
}

pub mod testing {
    pub use std::{cell::RefCell, os::fd::IntoRawFd, rc::Rc};

    pub use anyhow::Context;
    pub use mio::{net::UnixStream, Poll, Registry, Token};
    pub use slab::Slab;
    pub use sozu_command::{
        proto::command::{
            HttpListenerConfig, HttpsListenerConfig, ServerConfig, TcpListenerConfig,
        },
        scm_socket::{Listeners, ScmSocket},
    };

    pub use crate::{
        backends::BackendMap,
        http::HttpProxy,
        https::HttpsProxy,
        pool::Pool,
        server::Server,
        server::{ListenSession, ProxyChannel, SessionManager},
        tcp::TcpProxy,
        Protocol, ProxySession,
    };

    /// Everything needed to create a Server
    pub struct ServerParts {
        pub event_loop: Poll,
        pub registry: Registry,
        pub sessions: Rc<RefCell<SessionManager>>,
        pub pool: Rc<RefCell<Pool>>,
        pub backends: Rc<RefCell<BackendMap>>,
        pub client_scm_socket: ScmSocket,
        pub server_scm_socket: ScmSocket,
        pub server_config: ServerConfig,
    }

    /// Setup a standalone server, for testing purposes
    pub fn prebuild_server(
        max_buffers: usize,
        buffer_size: usize,
        send_scm: bool,
    ) -> anyhow::Result<ServerParts> {
        let event_loop = Poll::new().with_context(|| "Failed at creating event loop")?;
        let backends = Rc::new(RefCell::new(BackendMap::new()));
        let server_config = ServerConfig {
            max_connections: max_buffers as u64,
            ..Default::default()
        };

        let pool = Rc::new(RefCell::new(Pool::with_capacity(
            1,
            max_buffers,
            buffer_size,
        )));

        let mut sessions: Slab<Rc<RefCell<dyn ProxySession>>> = Slab::with_capacity(max_buffers);
        {
            let entry = sessions.vacant_entry();
            info!("taking token {:?} for channel", entry.key());
            entry.insert(Rc::new(RefCell::new(ListenSession {
                protocol: Protocol::Channel,
            })));
        }
        {
            let entry = sessions.vacant_entry();
            info!("taking token {:?} for timer", entry.key());
            entry.insert(Rc::new(RefCell::new(ListenSession {
                protocol: Protocol::Timer,
            })));
        }
        {
            let entry = sessions.vacant_entry();
            info!("taking token {:?} for metrics", entry.key());
            entry.insert(Rc::new(RefCell::new(ListenSession {
                protocol: Protocol::Metrics,
            })));
        }
        let sessions = SessionManager::new(sessions, max_buffers);

        let registry = event_loop
            .registry()
            .try_clone()
            .with_context(|| "Failed at creating a registry")?;

        let (scm_server, scm_client) =
            UnixStream::pair().with_context(|| "Failed at creating scm unix stream")?;
        let client_scm_socket = ScmSocket::new(scm_client.into_raw_fd())
            .with_context(|| "Failed at creating the scm client socket")?;
        let server_scm_socket = ScmSocket::new(scm_server.into_raw_fd())
            .with_context(|| "Failed at creating the scm server socket")?;
        if send_scm {
            client_scm_socket
                .send_listeners(&Listeners::default())
                .with_context(|| "Failed at sending empty listeners")?;
        }

        Ok(ServerParts {
            event_loop,
            registry,
            sessions,
            pool,
            backends,
            client_scm_socket,
            server_scm_socket,
            server_config,
        })
    }
}
