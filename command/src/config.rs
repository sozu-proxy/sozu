//! # Sōzu's configuration
//!
//! This module is responsible for parsing the `config.toml` provided by the flag `--config`
//! when starting Sōzu.
//!
//! Here is the workflow for generating a working config:
//!
//! ```text
//!     config.toml   ->   FileConfig    ->  ConfigBuilder   ->  Config
//! ```
//!
//! `config.toml` is parsed to `FileConfig`, a structure that itself contains a lot of substructures
//! whose names start with `File-` and end with `-Config`, like `FileHttpFrontendConfig` for instance.
//!
//! The instance of `FileConfig` is then passed to a `ConfigBuilder` that populates a final `Config`
//! with listeners and clusters.
//!
//! To illustrate:
//!
//! ```no_run
//! use sozu_command_lib::config::{FileConfig, ConfigBuilder};
//!
//! let file_config = FileConfig::load_from_path("../config.toml")
//!     .expect("Could not load config.toml");
//!
//! let config = ConfigBuilder::new(file_config, "../assets/config.toml")
//!     .into_config()
//!     .expect("Could not build config");
//! ```
//!
//! Note that the path to `config.toml` is used twice: the first time, to parse the file,
//! the second time, to keep the path in the config for later use.
//!
//! However, there is a simpler way that combines all this:
//!
//! ```no_run
//! use sozu_command_lib::config::Config;
//!
//! let config = Config::load_from_path("../assets/config.toml")
//!     .expect("Could not build config from the path");
//! ```
//!
//! ## How values are chosen
//!
//! Values are chosen in this order of priority:
//!
//! 1. values defined in a section of the TOML file, for instance, timeouts for a specific listener
//! 2. values defined globally in the TOML file, like timeouts or buffer size
//! 3. if a variable has not been set in the TOML file, it will be set to a default defined here
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env, fmt,
    fs::{File, create_dir_all, metadata},
    io::{ErrorKind, Read},
    net::SocketAddr,
    ops::Range,
    path::PathBuf,
};

use crate::{
    ObjectKind,
    certificate::split_certificate_chain,
    logging::AccessLogFormat,
    proto::command::{
        ActivateListener, AddBackend, AddCertificate, CertificateAndKey, Cluster,
        CustomHttpAnswers, Header, HeaderPosition, HttpListenerConfig, HttpsListenerConfig,
        ListenerType, LoadBalancingAlgorithms, LoadBalancingParams, LoadMetric, MetricDetail,
        MetricsConfiguration, PathRule, ProtobufAccessLogFormat, ProxyProtocolConfig,
        RedirectPolicy, RedirectScheme, Request, RequestHttpFrontend, RequestTcpFrontend,
        RulePosition, ServerConfig, ServerMetricsConfig, SocketAddress, TcpListenerConfig,
        TlsVersion, WorkerRequest, request::RequestType,
    },
};

/// Authoritative list of default cipher suites for all rustls-based TLS providers.
///
/// These use rustls naming conventions and are supported by all three crypto providers
/// (ring, aws-lc-rs, rustls-openssl). Order follows ANSSI recommendations: AES-256
/// preferred over AES-128, ECDSA preferred over RSA, TLS 1.3 preferred over TLS 1.2.
///
/// See the [documentation](https://docs.rs/rustls/latest/rustls/static.ALL_CIPHER_SUITES.html)
pub const DEFAULT_CIPHER_LIST: [&str; 9] = [
    // TLS 1.3 cipher suites
    "TLS13_AES_256_GCM_SHA384",
    "TLS13_AES_128_GCM_SHA256",
    "TLS13_CHACHA20_POLY1305_SHA256",
    // TLS 1.2 cipher suites
    "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384",
    "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256",
    "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256",
    "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384",
    "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256",
    "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256",
];

pub const DEFAULT_SIGNATURE_ALGORITHMS: [&str; 9] = [
    "ECDSA+SHA256",
    "ECDSA+SHA384",
    "ECDSA+SHA512",
    "RSA+SHA256",
    "RSA+SHA384",
    "RSA+SHA512",
    "RSA-PSS+SHA256",
    "RSA-PSS+SHA384",
    "RSA-PSS+SHA512",
];

pub const DEFAULT_GROUPS_LIST: [&str; 4] = ["X25519MLKEM768", "x25519", "P-256", "P-384"];

/// Default ALPN protocols advertised by HTTPS listeners.
/// Both HTTP/2 and HTTP/1.1 are enabled, allowing clients to negotiate either.
pub const DEFAULT_ALPN_PROTOCOLS: [&str; 2] = ["h2", "http/1.1"];

/// maximum time of inactivity for a frontend socket (60 seconds)
pub const DEFAULT_FRONT_TIMEOUT: u32 = 60;

/// maximum time of inactivity for a backend socket (30 seconds)
pub const DEFAULT_BACK_TIMEOUT: u32 = 30;

/// maximum time to connect to a backend server (3 seconds)
pub const DEFAULT_CONNECT_TIMEOUT: u32 = 3;

/// maximum time to receive a request since the connection started (10 seconds)
pub const DEFAULT_REQUEST_TIMEOUT: u32 = 10;

/// maximum time to wait for a worker to respond, until it is deemed NotAnswering (10 seconds)
pub const DEFAULT_WORKER_TIMEOUT: u32 = 10;

/// a name applied to sticky sessions ("SOZUBALANCEID")
pub const DEFAULT_STICKY_NAME: &str = "SOZUBALANCEID";

/// Interval between checking for zombie sessions, (30 minutes)
pub const DEFAULT_ZOMBIE_CHECK_INTERVAL: u32 = 1_800;

/// timeout to accept connection events in the accept queue (60 seconds)
pub const DEFAULT_ACCEPT_QUEUE_TIMEOUT: u32 = 60;

/// whether to evict least-recently-active sessions when the accept queue is
/// saturated (false). Defaults to false because during a DDoS the existing
/// connections are more likely to be legitimate clients than the queued ones;
/// evicting them would serve the attacker. Enable when overload is dominated
/// by normal traffic spikes rather than attacks.
pub const DEFAULT_EVICT_ON_QUEUE_FULL: bool = false;

/// number of workers, i.e. Sōzu processes that scale horizontally (2)
pub const DEFAULT_WORKER_COUNT: u16 = 2;

/// wether a worker is automatically restarted when it crashes (true)
pub const DEFAULT_WORKER_AUTOMATIC_RESTART: bool = true;

/// wether to save the state automatically (false)
pub const DEFAULT_AUTOMATIC_STATE_SAVE: bool = false;

/// minimum number of buffers (1)
pub const DEFAULT_MIN_BUFFERS: u64 = 1;

/// maximum number of buffers (1 000)
pub const DEFAULT_MAX_BUFFERS: u64 = 1_000;

/// size of the buffers, in bytes (16 KB)
pub const DEFAULT_BUFFER_SIZE: u64 = 16_393;

/// minimum buffer size required when any HTTPS listener advertises H2 ALPN.
///
/// RFC 9113 §6.5.2 caps `SETTINGS_MAX_FRAME_SIZE` at 16 384 bytes by default;
/// the on-wire H2 frame header is a fixed 9 bytes (§4.1), so the kawa storage
/// must be able to hold 16 384 + 9 = 16 393 bytes before forwarding. A smaller
/// `buffer_size` causes the H2 mux to deadlock on full-size frames (no panic,
/// no obvious log) until the session timeout fires. Validated at config-load
/// time in `ConfigBuilder::into_config` so a typo in TOML is rejected at boot,
/// not discovered under traffic.
pub const H2_MIN_BUFFER_SIZE: u64 = 16_393;

/// maximum number of simultaneous connections (10 000)
pub const DEFAULT_MAX_CONNECTIONS: usize = 10_000;

/// size of the buffer for the channels, in bytes. Must be bigger than the size of the data received. (1 MB)
pub const DEFAULT_COMMAND_BUFFER_SIZE: u64 = 1_000_000;

/// maximum size of the buffer for the channels, in bytes. (2 MB)
pub const DEFAULT_MAX_COMMAND_BUFFER_SIZE: u64 = 2_000_000;

/// wether to avoid register cluster metrics in the local drain
pub const DEFAULT_DISABLE_CLUSTER_METRICS: bool = false;

pub const MAX_LOOP_ITERATIONS: usize = 100000;

/// Number of TLS 1.3 tickets to send to a client when establishing a connection.
/// The tickets allow the client to resume a session. This protects the client
/// agains session tracking. Increases the number of getrandom syscalls,
/// with little influence on performance. Defaults to 4.
pub const DEFAULT_SEND_TLS_13_TICKETS: u64 = 4;

/// for both logs and access logs
pub const DEFAULT_LOG_TARGET: &str = "stdout";

/// Default per-(cluster, source-IP) connection limit. `0` means unlimited.
/// Counts are kept per `(cluster_id, source_ip)` so two clusters never
/// share a counter even from the same IP. Per-cluster overrides on the
/// `Cluster` message take precedence.
pub const DEFAULT_MAX_CONNECTIONS_PER_IP: u64 = 0;

/// Default `Retry-After` header value (seconds) on HTTP 429 responses
/// emitted when a per-(cluster, source-IP) connection limit is hit. `0`
/// omits the header — `Retry-After: 0` invites an immediate retry that
/// defeats the limit. TCP rejections do not emit this value (no HTTP
/// envelope), but the field is accepted for symmetry.
pub const DEFAULT_RETRY_AFTER: u32 = 60;

#[derive(Debug)]
pub enum IncompatibilityKind {
    PublicAddress,
    ProxyProtocol,
}

#[derive(Debug)]
pub enum MissingKind {
    Field(String),
    Protocol,
    SavedState,
}

#[derive(thiserror::Error, Debug)]
pub enum ConfigError {
    #[error("env path not found: {0}")]
    Env(String),
    #[error("Could not open file {path_to_open}: {io_error}")]
    FileOpen {
        path_to_open: String,
        io_error: std::io::Error,
    },
    #[error("Could not read file {path_to_read}: {io_error}")]
    FileRead {
        path_to_read: String,
        io_error: std::io::Error,
    },
    #[error(
        "the field {kind:?} of {object:?} with id or address {id} is incompatible with the rest of the options"
    )]
    Incompatible {
        kind: IncompatibilityKind,
        object: ObjectKind,
        id: String,
    },
    #[error("Invalid '{0}' field for a TCP frontend")]
    InvalidFrontendConfig(String),
    #[error("invalid path {0:?}")]
    InvalidPath(PathBuf),
    #[error("listening address {0:?} is already used in the configuration")]
    ListenerAddressAlreadyInUse(SocketAddr),
    #[error("missing {0:?}")]
    Missing(MissingKind),
    #[error("could not get parent directory for file {0}")]
    NoFileParent(String),
    #[error("Could not get the path of the saved state")]
    SaveStatePath(String),
    #[error("Can not determine path to sozu socket: {0}")]
    SocketPathError(String),
    #[error("toml decoding error: {0}")]
    DeserializeToml(String),
    #[error("Can not set this frontend on a {0:?} listener")]
    WrongFrontendProtocol(ListenerProtocol),
    #[error("Can not build a {expected:?} listener from a {found:?} config")]
    WrongListenerProtocol {
        expected: ListenerProtocol,
        found: Option<ListenerProtocol>,
    },
    #[error("Invalid ALPN protocol '{0}'. Valid values: \"h2\", \"http/1.1\"")]
    InvalidAlpnProtocol(String),
    /// `disable_http11 = true` and `alpn_protocols` containing `"http/1.1"`
    /// are mutually exclusive: the proxy advertises `http/1.1` to peers,
    /// then refuses every connection that negotiates
    /// it. The combination is a self-DoS at handshake time. Either drop
    /// `http/1.1` from `alpn_protocols` or unset `disable_http11`.
    #[error(
        "disable_http11 = true is incompatible with alpn_protocols containing \"http/1.1\" \
         on listener {address}. The proxy would advertise http/1.1 then refuse every \
         connection that negotiates it. Drop \"http/1.1\" from alpn_protocols or unset \
         disable_http11."
    )]
    DisableHttp11WithHttp11Alpn { address: String },
    /// `buffer_size` is below the H2 minimum (16 393 bytes) but at least one
    /// HTTPS listener advertises `h2` in its ALPN list. The H2 mux requires
    /// 16 384-byte frame payload + 9-byte header to fit in a single kawa
    /// buffer; smaller values deadlock streams that carry full-size frames.
    /// Either raise `buffer_size` to ≥ 16 393 or remove `h2` from the
    /// affected listeners' `alpn_protocols`.
    #[error(
        "buffer_size = {buffer_size} is below the H2 minimum of {minimum} but \
         {listeners} HTTPS listener(s) advertise H2 ALPN. The H2 mux deadlocks \
         on full-size frames with smaller buffers. Raise buffer_size to >= {minimum} \
         or remove \"h2\" from those listeners' alpn_protocols."
    )]
    BufferSizeTooSmallForH2 {
        buffer_size: u64,
        minimum: u64,
        listeners: usize,
    },
    /// `redirect = "<value>"` on a frontend used a value the parser doesn't
    /// recognise. Accepted values are `forward`, `permanent`, `unauthorized`
    /// (case-insensitive).
    #[error(
        "invalid redirect policy '{0}'. Valid values: \"forward\", \"permanent\", \"unauthorized\""
    )]
    InvalidRedirectPolicy(String),
    /// `redirect_scheme = "<value>"` on a frontend used a value the parser
    /// doesn't recognise. Accepted values are `use-same`, `use-http`,
    /// `use-https` (case-insensitive).
    #[error(
        "invalid redirect scheme '{0}'. Valid values: \"use-same\", \"use-http\", \"use-https\""
    )]
    InvalidRedirectScheme(String),
    /// A `[[clusters.<id>.frontends.headers]]` entry carried an unknown
    /// `position` value. Accepted values are `request`, `response`, `both`
    /// (case-insensitive).
    #[error(
        "invalid header position '{position}' at headers[{index}]. Valid values: \"request\", \"response\", \"both\""
    )]
    InvalidHeaderPosition { index: usize, position: String },
    /// A `[[clusters.<id>.frontends.headers]]` entry contains a forbidden
    /// byte (NUL, CR, LF, or another C0 control) in its key or value.
    /// Accepting these would produce HTTP request/response splitting on
    /// the wire (CWE-113) — the worker's H2 emission path filters them
    /// at runtime, but the H1 path serialises raw, so we reject at
    /// config-load time as a defense in depth.
    #[error(
        "invalid header bytes in {field} at headers[{index}]: control characters \
         (NUL / CR / LF / other C0) are forbidden in header keys and values"
    )]
    InvalidHeaderBytes { index: usize, field: &'static str },
}

/// An HTTP, HTTPS or TCP listener as parsed from the `Listeners` section in the toml
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListenerBuilder {
    pub address: SocketAddr,
    pub protocol: Option<ListenerProtocol>,
    pub public_address: Option<SocketAddr>,
    pub answer_301: Option<String>,
    pub answer_400: Option<String>,
    pub answer_401: Option<String>,
    pub answer_404: Option<String>,
    pub answer_408: Option<String>,
    pub answer_413: Option<String>,
    /// RFC 9110 §15.5.20 — returned when the request's `:authority` / `Host`
    /// host does not match the TLS SNI negotiated for this connection.
    pub answer_421: Option<String>,
    pub answer_502: Option<String>,
    pub answer_503: Option<String>,
    pub answer_504: Option<String>,
    pub answer_507: Option<String>,
    /// RFC 6585 §4 — emitted when a request would have reached a backend
    /// but the per-(cluster, source-IP) connection limit is full. Honoured
    /// like the other deprecated `answer_NNN` fields: copies into the
    /// listener-level `answers` map at the matching status.
    pub answer_429: Option<String>,
    pub tls_versions: Option<Vec<TlsVersion>>,
    pub cipher_list: Option<Vec<String>>,
    pub cipher_suites: Option<Vec<String>>,
    pub groups_list: Option<Vec<String>>,
    pub expect_proxy: Option<bool>,
    #[serde(default = "default_sticky_name")]
    pub sticky_name: String,
    pub certificate: Option<String>,
    pub certificate_chain: Option<String>,
    pub key: Option<String>,
    /// maximum time of inactivity for a frontend socket
    pub front_timeout: Option<u32>,
    /// maximum time of inactivity for a backend socket
    pub back_timeout: Option<u32>,
    /// maximum time to connect to a backend server
    pub connect_timeout: Option<u32>,
    /// maximum time to receive a request since the connection started
    pub request_timeout: Option<u32>,
    /// A [Config] to pull defaults from
    pub config: Option<Config>,
    /// Number of TLS 1.3 tickets to send to a client when establishing a connection.
    /// The ticket allow the client to resume a session. This protects the client
    /// agains session tracking. Defaults to 4.
    pub send_tls13_tickets: Option<u64>,
    /// ALPN protocols to advertise during TLS handshake, in order of preference.
    /// Valid values: "h2", "http/1.1". Defaults to ["h2", "http/1.1"].
    pub alpn_protocols: Option<Vec<String>>,
    /// H2 flood detection: max RST_STREAM frames per second window (CVE-2023-44487, CVE-2019-9514)
    pub h2_max_rst_stream_per_window: Option<u32>,
    /// H2 flood detection: max PING frames per second window (CVE-2019-9512)
    pub h2_max_ping_per_window: Option<u32>,
    /// H2 flood detection: max SETTINGS frames per second window (CVE-2019-9515)
    pub h2_max_settings_per_window: Option<u32>,
    /// H2 flood detection: max empty DATA frames per second window (CVE-2019-9518)
    pub h2_max_empty_data_per_window: Option<u32>,
    /// H2 flood detection: max connection-level (stream 0) WINDOW_UPDATE
    /// frames per sliding window. Caps non-zero stream-0 WINDOW_UPDATE floods
    /// that would otherwise stay under the generic glitch counter. Default: 100.
    pub h2_max_window_update_stream0_per_window: Option<u32>,
    /// Name of the correlation header Sozu injects into every request and
    /// response. Default: `Sozu-Id`. Operators can rebrand (e.g. `X-Edge-Id`)
    /// without touching code.
    pub sozu_id_header: Option<String>,
    /// H2 flood detection: max CONTINUATION frames per header block (CVE-2024-27316)
    pub h2_max_continuation_frames: Option<u32>,
    /// H2 flood detection: max accumulated protocol anomalies before ENHANCE_YOUR_CALM
    pub h2_max_glitch_count: Option<u32>,
    /// H2 connection-level receive window size in bytes (RFC 9113 §6.9.2). Default: 1048576 (1MB).
    pub h2_initial_connection_window: Option<u32>,
    /// Maximum concurrent H2 streams (SETTINGS_MAX_CONCURRENT_STREAMS). Default: 100.
    pub h2_max_concurrent_streams: Option<u32>,
    /// Shrink threshold ratio for recycled stream slots. Default: 2.
    pub h2_stream_shrink_ratio: Option<u32>,
    /// H2 flood detection: absolute lifetime cap on RST_STREAM frames
    /// received on a single connection (CVE-2023-44487). Default: 10000.
    pub h2_max_rst_stream_lifetime: Option<u64>,
    /// H2 flood detection: lifetime cap on "abusive" (pre-response-start)
    /// RST_STREAM frames (Rapid Reset signature, CVE-2023-44487). Default: 50.
    pub h2_max_rst_stream_abusive_lifetime: Option<u64>,
    /// H2 flood detection: absolute lifetime cap on **server-emitted**
    /// RST_STREAM frames (CVE-2025-8671 "MadeYouReset"). Only non-`NoError`
    /// resets count — graceful cancels are exempt. Default: 500.
    pub h2_max_rst_stream_emitted_lifetime: Option<u64>,
    /// H2 flood detection: maximum accumulated HPACK-decoded header list
    /// size per request (SETTINGS_MAX_HEADER_LIST_SIZE, RFC 9113 §6.5.2).
    /// Default: 65536.
    pub h2_max_header_list_size: Option<u32>,
    /// Maximum HPACK dynamic table size (SETTINGS_HEADER_TABLE_SIZE) accepted
    /// from the peer. Caps the value the peer advertises in SETTINGS frames to
    /// prevent unbounded HPACK encoder memory growth. Default: 65536.
    pub h2_max_header_table_size: Option<u32>,
    /// Per-stream idle timeout, in seconds. An open H2 stream that makes no
    /// forward progress for this duration is cancelled (RST_STREAM / CANCEL)
    /// to defend against slow-multiplex Slowloris. Default: 30.
    pub h2_stream_idle_timeout_seconds: Option<u32>,
    /// Maximum wall-clock seconds to wait for in-flight H2 streams after
    /// `GOAWAY(NO_ERROR)` has been sent during soft-stop. Once the deadline
    /// elapses the connection is forcibly closed with a final GOAWAY. Set to
    /// `0` to wait for streams to finish (no forced close). Default: 5.
    pub h2_graceful_shutdown_deadline_seconds: Option<u32>,
    /// When true, every HTTP request served on this listener must have its
    /// `:authority` / `Host` host exact-match the TLS SNI negotiated at
    /// handshake (CWE-346 / CWE-444). Applies to HTTPS listeners only;
    /// plaintext HTTP listeners never have an SNI to compare against.
    /// Default: true.
    pub strict_sni_binding: Option<bool>,
    /// When true, this HTTPS listener only accepts HTTP/2 connections;
    /// clients that do not negotiate `h2` via TLS ALPN (including those
    /// that omit ALPN entirely) are dropped at handshake instead of
    /// silently downgrading to HTTP/1.1. Default: false.
    pub disable_http11: Option<bool>,
    /// When true, any client-supplied `X-Real-IP` header is stripped from
    /// requests before forwarding (anti-spoofing). Independently combinable
    /// with `send_x_real_ip`. Default: false.
    pub elide_x_real_ip: Option<bool>,
    /// When true, a proxy-generated `X-Real-IP` header carrying the
    /// connection peer IP (post-PROXY-v2 unwrap, i.e. the original client
    /// IP) is appended to every forwarded request. Independently combinable
    /// with `elide_x_real_ip`. Default: false.
    pub send_x_real_ip: Option<bool>,
    /// Per-status HTTP answer templates at listener scope — the **global
    /// default** that fires whenever no cluster-level override matches.
    /// Map key is the HTTP status code (e.g. `"503"`); map value is
    /// either a filesystem path or an `inline:<body>` literal, see
    /// [`resolve_answer_source`]. Loaded into
    /// [`HttpListenerConfig::answers`] / [`HttpsListenerConfig::answers`]
    /// at build time via [`load_answers`].
    ///
    /// Cluster-level [`FileClusterConfig::answers`] entries override the
    /// matching status here for requests routed to that cluster.
    ///
    /// The deprecated per-status `answer_NNN` fields are still honoured
    /// for backwards compatibility but are equivalent to a one-line entry
    /// in this map; new configs should prefer `[listeners.answers]`.
    pub answers: Option<BTreeMap<String, String>>,
}

pub fn default_sticky_name() -> String {
    DEFAULT_STICKY_NAME.to_string()
}

impl ListenerBuilder {
    /// starts building an HTTP Listener with config values for timeouts,
    /// or defaults if no config is provided
    pub fn new_http(address: SocketAddress) -> ListenerBuilder {
        Self::new(address, ListenerProtocol::Http)
    }

    /// starts building an HTTPS Listener with config values for timeouts,
    /// or defaults if no config is provided
    pub fn new_tcp(address: SocketAddress) -> ListenerBuilder {
        Self::new(address, ListenerProtocol::Tcp)
    }

    /// starts building a TCP Listener with config values for timeouts,
    /// or defaults if no config is provided
    pub fn new_https(address: SocketAddress) -> ListenerBuilder {
        Self::new(address, ListenerProtocol::Https)
    }

    /// starts building a Listener
    fn new(address: SocketAddress, protocol: ListenerProtocol) -> ListenerBuilder {
        ListenerBuilder {
            address: address.into(),
            answer_301: None,
            answer_401: None,
            answer_400: None,
            answer_404: None,
            answer_408: None,
            answer_413: None,
            answer_421: None,
            answer_502: None,
            answer_503: None,
            answer_504: None,
            answer_507: None,
            answer_429: None,
            back_timeout: None,
            certificate_chain: None,
            certificate: None,
            cipher_list: None,
            cipher_suites: None,
            groups_list: None,
            config: None,
            connect_timeout: None,
            expect_proxy: None,
            front_timeout: None,
            key: None,
            protocol: Some(protocol),
            public_address: None,
            request_timeout: None,
            send_tls13_tickets: None,
            sticky_name: DEFAULT_STICKY_NAME.to_string(),
            tls_versions: None,
            alpn_protocols: None,
            h2_max_rst_stream_per_window: None,
            h2_max_ping_per_window: None,
            h2_max_settings_per_window: None,
            h2_max_empty_data_per_window: None,
            h2_max_window_update_stream0_per_window: None,
            sozu_id_header: None,
            h2_max_continuation_frames: None,
            h2_max_glitch_count: None,
            h2_initial_connection_window: None,
            h2_max_concurrent_streams: None,
            h2_stream_shrink_ratio: None,
            h2_max_rst_stream_lifetime: None,
            h2_max_rst_stream_abusive_lifetime: None,
            h2_max_rst_stream_emitted_lifetime: None,
            h2_max_header_list_size: None,
            h2_max_header_table_size: None,
            h2_stream_idle_timeout_seconds: None,
            h2_graceful_shutdown_deadline_seconds: None,
            strict_sni_binding: None,
            disable_http11: None,
            elide_x_real_ip: None,
            send_x_real_ip: None,
            answers: None,
        }
    }

    pub fn with_public_address(&mut self, public_address: Option<SocketAddr>) -> &mut Self {
        if let Some(address) = public_address {
            self.public_address = Some(address);
        }
        self
    }

    pub fn with_answer_404_path<S>(&mut self, answer_404_path: Option<S>) -> &mut Self
    where
        S: ToString,
    {
        if let Some(path) = answer_404_path {
            self.answer_404 = Some(path.to_string());
        }
        self
    }

    pub fn with_answer_503_path<S>(&mut self, answer_503_path: Option<S>) -> &mut Self
    where
        S: ToString,
    {
        if let Some(path) = answer_503_path {
            self.answer_503 = Some(path.to_string());
        }
        self
    }

    pub fn with_tls_versions(&mut self, tls_versions: Vec<TlsVersion>) -> &mut Self {
        self.tls_versions = Some(tls_versions);
        self
    }

    pub fn with_cipher_list(&mut self, cipher_list: Option<Vec<String>>) -> &mut Self {
        self.cipher_list = cipher_list;
        self
    }

    pub fn with_cipher_suites(&mut self, cipher_suites: Option<Vec<String>>) -> &mut Self {
        self.cipher_suites = cipher_suites;
        self
    }

    pub fn with_alpn_protocols(&mut self, alpn_protocols: Option<Vec<String>>) -> &mut Self {
        self.alpn_protocols = alpn_protocols;
        self
    }

    /// When true, strip any client-supplied `X-Real-IP` header from
    /// forwarded requests (anti-spoofing). Default: false.
    pub fn with_elide_x_real_ip(&mut self, elide_x_real_ip: bool) -> &mut Self {
        self.elide_x_real_ip = Some(elide_x_real_ip);
        self
    }

    /// When true, append a proxy-generated `X-Real-IP` header carrying the
    /// connection peer IP (post-PROXY-v2 unwrap) to every forwarded request.
    /// Default: false.
    pub fn with_send_x_real_ip(&mut self, send_x_real_ip: bool) -> &mut Self {
        self.send_x_real_ip = Some(send_x_real_ip);
        self
    }

    pub fn with_expect_proxy(&mut self, expect_proxy: bool) -> &mut Self {
        self.expect_proxy = Some(expect_proxy);
        self
    }

    pub fn with_sticky_name<S>(&mut self, sticky_name: Option<S>) -> &mut Self
    where
        S: ToString,
    {
        if let Some(name) = sticky_name {
            self.sticky_name = name.to_string();
        }
        self
    }

    pub fn with_certificate<S>(&mut self, certificate: S) -> &mut Self
    where
        S: ToString,
    {
        self.certificate = Some(certificate.to_string());
        self
    }

    pub fn with_certificate_chain(&mut self, certificate_chain: String) -> &mut Self {
        self.certificate = Some(certificate_chain);
        self
    }

    pub fn with_key<S>(&mut self, key: String) -> &mut Self
    where
        S: ToString,
    {
        self.key = Some(key);
        self
    }

    pub fn with_front_timeout(&mut self, front_timeout: Option<u32>) -> &mut Self {
        self.front_timeout = front_timeout;
        self
    }

    pub fn with_back_timeout(&mut self, back_timeout: Option<u32>) -> &mut Self {
        self.back_timeout = back_timeout;
        self
    }

    pub fn with_connect_timeout(&mut self, connect_timeout: Option<u32>) -> &mut Self {
        self.connect_timeout = connect_timeout;
        self
    }

    pub fn with_request_timeout(&mut self, request_timeout: Option<u32>) -> &mut Self {
        self.request_timeout = request_timeout;
        self
    }

    /// Register a single per-status answer template file path on this
    /// listener. The path is read off disk into the resulting listener's
    /// `answers` map at build time via [`load_answers`]. Repeated calls
    /// with the same status code overwrite the prior entry.
    pub fn with_answer<S, P>(&mut self, code: S, path: P) -> &mut Self
    where
        S: ToString,
        P: ToString,
    {
        self.answers
            .get_or_insert_with(BTreeMap::new)
            .insert(code.to_string(), path.to_string());
        self
    }

    /// Replace the listener-scope answer-template path map. See
    /// [`Self::with_answer`].
    pub fn with_answers(&mut self, answers: BTreeMap<String, String>) -> &mut Self {
        self.answers = Some(answers);
        self
    }

    /// Get the custom HTTP answers from the file system using the provided paths
    fn get_http_answers(&self) -> Result<Option<CustomHttpAnswers>, ConfigError> {
        let http_answers = CustomHttpAnswers {
            answer_301: read_http_answer_file(&self.answer_301)?,
            answer_400: read_http_answer_file(&self.answer_400)?,
            answer_401: read_http_answer_file(&self.answer_401)?,
            answer_404: read_http_answer_file(&self.answer_404)?,
            answer_408: read_http_answer_file(&self.answer_408)?,
            answer_413: read_http_answer_file(&self.answer_413)?,
            answer_421: read_http_answer_file(&self.answer_421)?,
            answer_502: read_http_answer_file(&self.answer_502)?,
            answer_503: read_http_answer_file(&self.answer_503)?,
            answer_504: read_http_answer_file(&self.answer_504)?,
            answer_507: read_http_answer_file(&self.answer_507)?,
            answer_429: read_http_answer_file(&self.answer_429)?,
        };
        Ok(Some(http_answers))
    }

    /// Build the proto-side `answers` map for this listener.
    ///
    /// Merges, in order:
    /// 1. legacy per-status `answer_NNN` fields (if set), so legacy state
    ///    files round-trip into the new shape;
    /// 2. the explicit `[listeners.answers]` map (loaded via [`load_answers`]),
    ///    so new entries take precedence over legacy ones.
    fn get_listener_answers(&self) -> Result<BTreeMap<String, String>, ConfigError> {
        let mut out = BTreeMap::new();

        // Pull bodies from the legacy per-status fields first so the new map
        // takes precedence on collision. Empty bodies are skipped to keep the
        // proto map minimal.
        macro_rules! merge_legacy {
            ($code:literal, $field:ident) => {
                if let Some(body) = read_http_answer_file(&self.$field)? {
                    out.insert($code.to_owned(), body);
                }
            };
        }
        merge_legacy!("301", answer_301);
        merge_legacy!("400", answer_400);
        merge_legacy!("401", answer_401);
        merge_legacy!("404", answer_404);
        merge_legacy!("408", answer_408);
        merge_legacy!("413", answer_413);
        merge_legacy!("421", answer_421);
        merge_legacy!("502", answer_502);
        merge_legacy!("503", answer_503);
        merge_legacy!("504", answer_504);
        merge_legacy!("507", answer_507);
        merge_legacy!("429", answer_429);

        if let Some(map) = &self.answers {
            let loaded = load_answers(map)?;
            out.extend(loaded);
        }
        Ok(out)
    }

    /// Assign the timeouts of the config to this listener, only if timeouts did not exist
    fn assign_config_timeouts(&mut self, config: &Config) {
        self.front_timeout = Some(self.front_timeout.unwrap_or(config.front_timeout));
        self.back_timeout = Some(self.back_timeout.unwrap_or(config.back_timeout));
        self.connect_timeout = Some(self.connect_timeout.unwrap_or(config.connect_timeout));
        self.request_timeout = Some(self.request_timeout.unwrap_or(config.request_timeout));
    }

    /// build an HTTP listener with config timeouts, using defaults if no config is provided
    pub fn to_http(&mut self, config: Option<&Config>) -> Result<HttpListenerConfig, ConfigError> {
        if self.protocol != Some(ListenerProtocol::Http) {
            return Err(ConfigError::WrongListenerProtocol {
                expected: ListenerProtocol::Http,
                found: self.protocol.to_owned(),
            });
        }

        if let Some(config) = config {
            self.assign_config_timeouts(config);
        }

        let http_answers = self.get_http_answers()?;
        let answers = self.get_listener_answers()?;

        let configuration = HttpListenerConfig {
            address: self.address.into(),
            public_address: self.public_address.map(|a| a.into()),
            expect_proxy: self.expect_proxy.unwrap_or(false),
            sticky_name: self.sticky_name.clone(),
            front_timeout: self.front_timeout.unwrap_or(DEFAULT_FRONT_TIMEOUT),
            back_timeout: self.back_timeout.unwrap_or(DEFAULT_BACK_TIMEOUT),
            connect_timeout: self.connect_timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT),
            request_timeout: self.request_timeout.unwrap_or(DEFAULT_REQUEST_TIMEOUT),
            http_answers,
            answers,
            h2_max_rst_stream_per_window: self.h2_max_rst_stream_per_window,
            h2_max_ping_per_window: self.h2_max_ping_per_window,
            h2_max_settings_per_window: self.h2_max_settings_per_window,
            h2_max_empty_data_per_window: self.h2_max_empty_data_per_window,
            h2_max_window_update_stream0_per_window: self.h2_max_window_update_stream0_per_window,
            h2_max_continuation_frames: self.h2_max_continuation_frames,
            h2_max_glitch_count: self.h2_max_glitch_count,
            h2_initial_connection_window: self.h2_initial_connection_window,
            h2_max_concurrent_streams: self.h2_max_concurrent_streams,
            h2_stream_shrink_ratio: self.h2_stream_shrink_ratio,
            h2_max_rst_stream_lifetime: self.h2_max_rst_stream_lifetime,
            h2_max_rst_stream_abusive_lifetime: self.h2_max_rst_stream_abusive_lifetime,
            h2_max_rst_stream_emitted_lifetime: self.h2_max_rst_stream_emitted_lifetime,
            h2_max_header_list_size: self.h2_max_header_list_size,
            h2_max_header_table_size: self.h2_max_header_table_size,
            h2_stream_idle_timeout_seconds: self.h2_stream_idle_timeout_seconds,
            h2_graceful_shutdown_deadline_seconds: self.h2_graceful_shutdown_deadline_seconds,
            sozu_id_header: self.sozu_id_header.clone(),
            elide_x_real_ip: Some(self.elide_x_real_ip.unwrap_or(false)),
            send_x_real_ip: Some(self.send_x_real_ip.unwrap_or(false)),
            ..Default::default()
        };

        Ok(configuration)
    }

    /// build an HTTPS listener using defaults if no config or values were provided upstream
    pub fn to_tls(&mut self, config: Option<&Config>) -> Result<HttpsListenerConfig, ConfigError> {
        if self.protocol != Some(ListenerProtocol::Https) {
            return Err(ConfigError::WrongListenerProtocol {
                expected: ListenerProtocol::Https,
                found: self.protocol.to_owned(),
            });
        }

        let default_cipher_list = DEFAULT_CIPHER_LIST.into_iter().map(String::from).collect();

        let cipher_list = self.cipher_list.clone().unwrap_or(default_cipher_list);

        let cipher_suites = self
            .cipher_suites
            .clone()
            .unwrap_or_else(|| DEFAULT_CIPHER_LIST.into_iter().map(String::from).collect());

        let signature_algorithms: Vec<String> = DEFAULT_SIGNATURE_ALGORITHMS
            .into_iter()
            .map(String::from)
            .collect();

        let groups_list = self
            .groups_list
            .clone()
            .unwrap_or_else(|| DEFAULT_GROUPS_LIST.into_iter().map(String::from).collect());

        let alpn_protocols: Vec<String> = match &self.alpn_protocols {
            Some(protos) if !protos.is_empty() => {
                for proto in protos {
                    match proto.as_str() {
                        "h2" | "http/1.1" => {}
                        other => return Err(ConfigError::InvalidAlpnProtocol(other.to_owned())),
                    }
                }
                // disable_http11 + http/1.1 ALPN is a self-DoS — every
                // connection negotiates http/1.1 then is
                // immediately refused at `https.rs::upgrade_handshake`.
                // Reject the combination at config load.
                if self.disable_http11.unwrap_or(false) && protos.iter().any(|p| p == "http/1.1") {
                    return Err(ConfigError::DisableHttp11WithHttp11Alpn {
                        address: self.address.to_string(),
                    });
                }
                if !protos.iter().any(|p| p == "http/1.1") {
                    warn!(
                        "ALPN protocols do not include 'http/1.1'. Clients without H2 support will fail TLS negotiation."
                    );
                }
                // Deduplicate while preserving order
                let mut seen = std::collections::HashSet::new();
                protos
                    .iter()
                    .filter(|p| seen.insert(p.as_str()))
                    .cloned()
                    .collect()
            }
            _ => {
                // Same self-DoS check on the default ALPN list (which
                // contains "http/1.1") — `disable_http11 = true` with the
                // implicit default ALPN must also be rejected.
                if self.disable_http11.unwrap_or(false)
                    && DEFAULT_ALPN_PROTOCOLS.contains(&"http/1.1")
                {
                    return Err(ConfigError::DisableHttp11WithHttp11Alpn {
                        address: self.address.to_string(),
                    });
                }
                DEFAULT_ALPN_PROTOCOLS
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            }
        };

        let versions = match self.tls_versions {
            None => vec![TlsVersion::TlsV12 as i32, TlsVersion::TlsV13 as i32],
            Some(ref v) => v.iter().map(|v| *v as i32).collect(),
        };

        let key = self.key.as_ref().and_then(|path| {
            Config::load_file(path)
                .map_err(|e| {
                    error!("cannot load key at path '{}': {:?}", path, e);
                    e
                })
                .ok()
        });
        let certificate = self.certificate.as_ref().and_then(|path| {
            Config::load_file(path)
                .map_err(|e| {
                    error!("cannot load certificate at path '{}': {:?}", path, e);
                    e
                })
                .ok()
        });
        let certificate_chain = self
            .certificate_chain
            .as_ref()
            .and_then(|path| {
                Config::load_file(path)
                    .map_err(|e| {
                        error!("cannot load certificate chain at path '{}': {:?}", path, e);
                        e
                    })
                    .ok()
            })
            .map(split_certificate_chain)
            .unwrap_or_default();

        let http_answers = self.get_http_answers()?;
        let answers = self.get_listener_answers()?;

        if let Some(config) = config {
            self.assign_config_timeouts(config);
        }

        let https_listener_config = HttpsListenerConfig {
            address: self.address.into(),
            sticky_name: self.sticky_name.clone(),
            public_address: self.public_address.map(|a| a.into()),
            cipher_list,
            versions,
            expect_proxy: self.expect_proxy.unwrap_or(false),
            key,
            certificate,
            certificate_chain,
            front_timeout: self.front_timeout.unwrap_or(DEFAULT_FRONT_TIMEOUT),
            back_timeout: self.back_timeout.unwrap_or(DEFAULT_BACK_TIMEOUT),
            connect_timeout: self.connect_timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT),
            request_timeout: self.request_timeout.unwrap_or(DEFAULT_REQUEST_TIMEOUT),
            cipher_suites,
            signature_algorithms,
            groups_list,
            active: false,
            send_tls13_tickets: self
                .send_tls13_tickets
                .unwrap_or(DEFAULT_SEND_TLS_13_TICKETS),
            http_answers,
            answers,
            alpn_protocols,
            h2_max_rst_stream_per_window: self.h2_max_rst_stream_per_window,
            h2_max_ping_per_window: self.h2_max_ping_per_window,
            h2_max_settings_per_window: self.h2_max_settings_per_window,
            h2_max_empty_data_per_window: self.h2_max_empty_data_per_window,
            h2_max_window_update_stream0_per_window: self.h2_max_window_update_stream0_per_window,
            h2_max_continuation_frames: self.h2_max_continuation_frames,
            h2_max_glitch_count: self.h2_max_glitch_count,
            h2_initial_connection_window: self.h2_initial_connection_window,
            h2_max_concurrent_streams: self.h2_max_concurrent_streams,
            h2_stream_shrink_ratio: self.h2_stream_shrink_ratio,
            h2_max_rst_stream_lifetime: self.h2_max_rst_stream_lifetime,
            h2_max_rst_stream_abusive_lifetime: self.h2_max_rst_stream_abusive_lifetime,
            h2_max_rst_stream_emitted_lifetime: self.h2_max_rst_stream_emitted_lifetime,
            h2_max_header_list_size: self.h2_max_header_list_size,
            h2_max_header_table_size: self.h2_max_header_table_size,
            strict_sni_binding: self.strict_sni_binding,
            disable_http11: self.disable_http11,
            h2_stream_idle_timeout_seconds: self.h2_stream_idle_timeout_seconds,
            h2_graceful_shutdown_deadline_seconds: self.h2_graceful_shutdown_deadline_seconds,
            sozu_id_header: self.sozu_id_header.clone(),
            elide_x_real_ip: Some(self.elide_x_real_ip.unwrap_or(false)),
            send_x_real_ip: Some(self.send_x_real_ip.unwrap_or(false)),
        };

        Ok(https_listener_config)
    }

    /// build an HTTPS listener using defaults if no config or values were provided upstream
    pub fn to_tcp(&mut self, config: Option<&Config>) -> Result<TcpListenerConfig, ConfigError> {
        if self.protocol != Some(ListenerProtocol::Tcp) {
            return Err(ConfigError::WrongListenerProtocol {
                expected: ListenerProtocol::Tcp,
                found: self.protocol.to_owned(),
            });
        }

        if let Some(config) = config {
            self.assign_config_timeouts(config);
        }

        Ok(TcpListenerConfig {
            address: self.address.into(),
            public_address: self.public_address.map(|a| a.into()),
            expect_proxy: self.expect_proxy.unwrap_or(false),
            front_timeout: self.front_timeout.unwrap_or(DEFAULT_FRONT_TIMEOUT),
            back_timeout: self.back_timeout.unwrap_or(DEFAULT_BACK_TIMEOUT),
            connect_timeout: self.connect_timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT),
            active: false,
        })
    }
}

/// read a custom HTTP answer from a file
fn read_http_answer_file(path: &Option<String>) -> Result<Option<String>, ConfigError> {
    match path {
        Some(path) => {
            let mut content = String::new();
            let mut file = File::open(path).map_err(|io_error| ConfigError::FileOpen {
                path_to_open: path.to_owned(),
                io_error,
            })?;

            file.read_to_string(&mut content)
                .map_err(|io_error| ConfigError::FileRead {
                    path_to_read: path.to_owned(),
                    io_error,
                })?;

            Ok(Some(content))
        }
        None => Ok(None),
    }
}

/// Resolve a single `answers` map entry into the literal template body
/// the proto layer expects.
///
/// The same resolution rule applies to entries at every layer:
/// * **Listener-level** `[listeners.<id>.answers]` — the global default
///   that fires whenever no more specific override matches.
/// * **Cluster-level** `[clusters.<id>.answers]` — overrides the
///   listener-level default for the matching status code on requests
///   routed to that cluster.
///
/// Two source forms are accepted:
/// * **Filesystem path** — the value starts with the `file://` URI
///   scheme. Everything after the prefix is treated as a path; the
///   path is opened and read into a string. Mirrors the on-disk
///   loading the per-status [`read_http_answer_file`] helper performs
///   for the deprecated `answer_301`..`answer_507` fields.
/// * **Inline literal** (default) — anything else. The value is taken
///   verbatim as the template body, including an empty string (a
///   0-byte response payload, typical with `Connection: close` and no
///   headers). The bare-string default keeps the common case — a
///   short canned response — typing-light; operators who need a file
///   say so explicitly with `file://`.
pub fn resolve_answer_source(value: &str) -> Result<String, ConfigError> {
    if let Some(path) = value.strip_prefix("file://") {
        let mut content = String::new();
        let mut file = File::open(path).map_err(|io_error| ConfigError::FileOpen {
            path_to_open: path.to_owned(),
            io_error,
        })?;
        file.read_to_string(&mut content)
            .map_err(|io_error| ConfigError::FileRead {
                path_to_read: path.to_owned(),
                io_error,
            })?;
        return Ok(content);
    }
    Ok(value.to_owned())
}

/// Load every per-status template referenced by `answers`.
///
/// `answers` maps an HTTP status code (e.g. `"503"`) to either a
/// filesystem path or an `inline:<body>` literal — see
/// [`resolve_answer_source`] for the resolution rules. Each entry is
/// resolved into a body string and inserted into the returned map
/// under the same key, ready to be assigned to the proto-level
/// `answers` field on a [`HttpListenerConfig`] / [`HttpsListenerConfig`]
/// / [`Cluster`]. Empty values are skipped (treated as "preserve
/// current") so the caller can use them as a no-op stub in example
/// configs.
///
/// Errors map to the existing `ConfigError::FileOpen` /
/// `ConfigError::FileRead` variants so the operator gets the same
/// diagnostics whether the path comes from this map or from the
/// deprecated per-status `answer_301`..`answer_507` fields.
pub fn load_answers(
    answers: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, ConfigError> {
    let mut out = BTreeMap::new();
    for (code, value) in answers {
        if value.is_empty() {
            continue;
        }
        out.insert(code.to_owned(), resolve_answer_source(value)?);
    }
    Ok(out)
}

/// Cardinality knob for metrics labels in the StatsD network drain.
///
/// Mirrors HAProxy's `process|frontend|backend|server` extra-counters opt-in.
/// Operators choose the lowest level that satisfies their dashboards so that
/// the keyspace stays bounded. Each level is a SUPERSET of the previous one:
///
/// - `process` — proxy-only counters (no listener, cluster, or backend label).
/// - `frontend` — adds per-listener (frontend) breakdown.
/// - `cluster` — adds per-cluster aggregation. **Default** (preserves the
///   pre-knob behaviour).
/// - `backend` — adds per-backend aggregation (cluster + backend, highest
///   cardinality).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MetricDetailLevel {
    Process,
    Frontend,
    Cluster,
    Backend,
}

impl Default for MetricDetailLevel {
    fn default() -> Self {
        // Preserve the historical (pre-knob) behaviour: cluster-scoped
        // metrics are emitted by default.
        Self::Cluster
    }
}

impl From<MetricDetailLevel> for MetricDetail {
    fn from(level: MetricDetailLevel) -> Self {
        match level {
            MetricDetailLevel::Process => MetricDetail::DetailProcess,
            MetricDetailLevel::Frontend => MetricDetail::DetailFrontend,
            MetricDetailLevel::Cluster => MetricDetail::DetailCluster,
            MetricDetailLevel::Backend => MetricDetail::DetailBackend,
        }
    }
}

impl From<MetricDetail> for MetricDetailLevel {
    /// Reverse of [`From<MetricDetailLevel> for MetricDetail`] — used by the
    /// worker side to convert the protobuf wire enum back into the
    /// configuration enum before passing it to `sozu_lib::metrics::setup`.
    fn from(detail: MetricDetail) -> Self {
        match detail {
            MetricDetail::DetailProcess => MetricDetailLevel::Process,
            MetricDetail::DetailFrontend => MetricDetailLevel::Frontend,
            MetricDetail::DetailCluster => MetricDetailLevel::Cluster,
            MetricDetail::DetailBackend => MetricDetailLevel::Backend,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsConfig {
    pub address: SocketAddr,
    #[serde(default)]
    pub tagged_metrics: bool,
    #[serde(default)]
    pub prefix: Option<String>,
    /// Cardinality knob for label-aware metrics. Defaults to `cluster` to
    /// preserve historical behaviour. See [`MetricDetailLevel`].
    #[serde(default)]
    pub detail: MetricDetailLevel,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[serde(deny_unknown_fields)]
pub enum PathRuleType {
    Prefix,
    Regex,
    Equals,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileClusterFrontendConfig {
    pub address: SocketAddr,
    pub hostname: Option<String>,
    /// creates a path routing rule where the request URL path has to match this
    pub path: Option<String>,
    /// declares whether the path rule is Prefix (default), Regex, or Equals
    pub path_type: Option<PathRuleType>,
    pub method: Option<String>,
    pub certificate: Option<String>,
    pub key: Option<String>,
    pub certificate_chain: Option<String>,
    #[serde(default)]
    pub tls_versions: Vec<TlsVersion>,
    #[serde(default)]
    pub position: RulePosition,
    pub tags: Option<BTreeMap<String, String>>,
    /// Frontend-level redirect policy. Accepted values are `forward`
    /// (default — route to the backend), `permanent` (return 301 with the
    /// computed `Location`), or `unauthorized` (return 401 with
    /// `WWW-Authenticate: Basic realm=…`). Case-insensitive.
    pub redirect: Option<String>,
    /// Scheme used when emitting a permanent redirect's `Location`. Accepted
    /// values are `use-same` (default — preserve request scheme), `use-http`,
    /// `use-https`. Case-insensitive.
    pub redirect_scheme: Option<String>,
    /// Optional template applied to the emitted permanent-redirect response
    /// body. Supports the `%REDIRECT_LOCATION` / `%STATUS_CODE` etc.
    /// variables documented in `doc/configure.md`.
    pub redirect_template: Option<String>,
    /// Rewrite host template. Supports `$HOST[n]` / `$PATH[n]` placeholders
    /// populated from regex captures collected during routing.
    pub rewrite_host: Option<String>,
    /// Rewrite path template. Same grammar as `rewrite_host`.
    pub rewrite_path: Option<String>,
    /// Optional literal port override on the rewritten URL.
    pub rewrite_port: Option<u32>,
    /// When true, requests routed through this frontend must carry a valid
    /// `Authorization: Basic <user:pass>` header whose hash matches one of
    /// the cluster's `authorized_hashes`. Default: false.
    pub required_auth: Option<bool>,
    /// Header mutations applied to requests and/or responses passing through
    /// this frontend. See [`HeaderEditConfig`] for the empty-value-deletes
    /// semantics (HAProxy `del-header` parity).
    pub headers: Option<Vec<HeaderEditConfig>>,
}

/// A single header mutation as serialised under
/// `[[clusters.<id>.frontends.headers]]`. Maps to the proto [`Header`]
/// message at request-build time.
///
/// `position` accepts `request`, `response`, or `both` (case-insensitive).
/// An empty `value` deletes the header by name (HAProxy `del-header` parity).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeaderEditConfig {
    pub position: String,
    pub key: String,
    pub value: String,
}

impl FileClusterFrontendConfig {
    pub fn to_tcp_front(&self) -> Result<TcpFrontendConfig, ConfigError> {
        if self.hostname.is_some() {
            return Err(ConfigError::InvalidFrontendConfig("hostname".to_string()));
        }
        if self.path.is_some() {
            return Err(ConfigError::InvalidFrontendConfig(
                "path_prefix".to_string(),
            ));
        }
        if self.certificate.is_some() {
            return Err(ConfigError::InvalidFrontendConfig(
                "certificate".to_string(),
            ));
        }
        if self.hostname.is_some() {
            return Err(ConfigError::InvalidFrontendConfig("hostname".to_string()));
        }
        if self.certificate_chain.is_some() {
            return Err(ConfigError::InvalidFrontendConfig(
                "certificate_chain".to_string(),
            ));
        }

        Ok(TcpFrontendConfig {
            address: self.address,
            tags: self.tags.clone(),
        })
    }

    pub fn to_http_front(&self, _cluster_id: &str) -> Result<HttpFrontendConfig, ConfigError> {
        let hostname = match &self.hostname {
            Some(hostname) => hostname.to_owned(),
            None => {
                return Err(ConfigError::Missing(MissingKind::Field(
                    "hostname".to_string(),
                )));
            }
        };

        let key_opt = match self.key.as_ref() {
            None => None,
            Some(path) => {
                let key = Config::load_file(path)?;
                Some(key)
            }
        };

        let certificate_opt = match self.certificate.as_ref() {
            None => None,
            Some(path) => {
                let certificate = Config::load_file(path)?;
                Some(certificate)
            }
        };

        let certificate_chain = match self.certificate_chain.as_ref() {
            None => None,
            Some(path) => {
                let certificate_chain = Config::load_file(path)?;
                Some(split_certificate_chain(certificate_chain))
            }
        };

        let path = match (self.path.as_ref(), self.path_type.as_ref()) {
            (None, _) => PathRule::prefix("".to_string()),
            (Some(s), Some(PathRuleType::Prefix)) => PathRule::prefix(s.to_string()),
            (Some(s), Some(PathRuleType::Regex)) => PathRule::regex(s.to_string()),
            (Some(s), Some(PathRuleType::Equals)) => PathRule::equals(s.to_string()),
            (Some(s), None) => PathRule::prefix(s.clone()),
        };

        let redirect = match self.redirect.as_deref() {
            Some(v) => Some(parse_redirect_policy(v)?),
            None => None,
        };
        let redirect_scheme = match self.redirect_scheme.as_deref() {
            Some(v) => Some(parse_redirect_scheme(v)?),
            None => None,
        };

        let headers = match self.headers.as_ref() {
            Some(entries) => {
                let mut out = Vec::with_capacity(entries.len());
                for (index, entry) in entries.iter().enumerate() {
                    out.push(parse_header_edit(index, entry)?);
                }
                out
            }
            None => Vec::new(),
        };

        Ok(HttpFrontendConfig {
            address: self.address,
            hostname,
            certificate: certificate_opt,
            key: key_opt,
            certificate_chain,
            tls_versions: self.tls_versions.clone(),
            position: self.position,
            path,
            method: self.method.clone(),
            tags: self.tags.clone(),
            redirect,
            redirect_scheme,
            redirect_template: self.redirect_template.clone(),
            rewrite_host: self.rewrite_host.clone(),
            rewrite_path: self.rewrite_path.clone(),
            rewrite_port: self.rewrite_port,
            required_auth: self.required_auth,
            headers,
        })
    }
}

/// Parse a `redirect` TOML value (case-insensitive) into the proto enum.
pub(crate) fn parse_redirect_policy(value: &str) -> Result<RedirectPolicy, ConfigError> {
    match value.to_ascii_lowercase().as_str() {
        "forward" => Ok(RedirectPolicy::Forward),
        "permanent" => Ok(RedirectPolicy::Permanent),
        "unauthorized" => Ok(RedirectPolicy::Unauthorized),
        _ => Err(ConfigError::InvalidRedirectPolicy(value.to_owned())),
    }
}

/// Parse a `redirect_scheme` TOML value (case-insensitive) into the proto enum.
pub(crate) fn parse_redirect_scheme(value: &str) -> Result<RedirectScheme, ConfigError> {
    match value.to_ascii_lowercase().as_str() {
        "use-same" | "use_same" => Ok(RedirectScheme::UseSame),
        "use-http" | "use_http" => Ok(RedirectScheme::UseHttp),
        "use-https" | "use_https" => Ok(RedirectScheme::UseHttps),
        _ => Err(ConfigError::InvalidRedirectScheme(value.to_owned())),
    }
}

/// Parse a `[[clusters.<id>.frontends.headers]]` entry into the proto
/// [`Header`] message. `index` is the zero-based position of `entry` in
/// the source array — surfaced into the error so a multi-entry config
/// pinpoints the bad row instead of just naming the unknown position.
/// An empty `value` is the HAProxy `del-header` parity (deletes the
/// header by name); the proto carries the empty string verbatim.
pub(crate) fn parse_header_edit(
    index: usize,
    entry: &HeaderEditConfig,
) -> Result<Header, ConfigError> {
    let position = match entry.position.to_ascii_lowercase().as_str() {
        "request" => HeaderPosition::Request,
        "response" => HeaderPosition::Response,
        "both" => HeaderPosition::Both,
        _ => {
            return Err(ConfigError::InvalidHeaderPosition {
                index,
                position: entry.position.clone(),
            });
        }
    };
    if !header_name_is_valid_token(entry.key.as_bytes()) {
        return Err(ConfigError::InvalidHeaderBytes {
            index,
            field: "key",
        });
    }
    if header_value_contains_forbidden_controls(entry.value.as_bytes()) {
        return Err(ConfigError::InvalidHeaderBytes {
            index,
            field: "value",
        });
    }
    Ok(Header {
        position: position as i32,
        key: entry.key.clone(),
        val: entry.value.clone(),
    })
}

/// Field names follow the RFC 9110 §5.1 `token` grammar: non-empty,
/// composed of `tchar` bytes (alphanumeric plus a closed punctuation
/// list). HTAB and SP are NOT tchar — they belong to field-value
/// grammar and must be rejected in the name. Reusing the more
/// permissive value-side filter would let `Host\t` slip through and
/// produce an invalid header line on the H1 wire (security review
/// LISA-002 follow-up).
pub(crate) fn header_name_is_valid_token(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    bytes.iter().all(|&b| is_tchar(b))
}

/// `tchar = "!" / "#" / "$" / "%" / "&" / "'" / "*" / "+" / "-" / "." /
/// "^" / "_" / "`" / "|" / "~" / DIGIT / ALPHA` per RFC 9110 §5.6.2.
fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
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

/// Reject any byte that would let a header injection escape the value
/// block on the wire (RFC 9110 §5.5 / RFC 9113 §8.2.1):
/// `\0..=\x08`, `\x0A..=\x1F`, and `\x7F` — the entire C0 control set
/// minus horizontal tab `\x09`, which RFC 9110 explicitly permits in
/// field values. Mirrors the runtime filter at
/// `lib/src/protocol/mux/converter.rs::call` so config-load and runtime
/// agree on which header values may travel.
pub(crate) fn header_value_contains_forbidden_controls(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .any(|&b| matches!(b, 0x00..=0x08 | 0x0A..=0x1F | 0x7F))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "lowercase")]
pub enum ListenerProtocol {
    Http,
    Https,
    Tcp,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "lowercase")]
pub enum FileClusterProtocolConfig {
    Http,
    Tcp,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileClusterConfig {
    pub frontends: Vec<FileClusterFrontendConfig>,
    pub backends: Vec<BackendConfig>,
    pub protocol: FileClusterProtocolConfig,
    pub sticky_session: Option<bool>,
    pub https_redirect: Option<bool>,
    #[serde(default)]
    pub send_proxy: Option<bool>,
    #[serde(default)]
    pub load_balancing: LoadBalancingAlgorithms,
    pub answer_503: Option<String>,
    #[serde(default)]
    pub load_metric: Option<LoadMetric>,
    /// Backend-capability hint: `true` when the backend speaks HTTP/2 (h2c or h2+TLS once #1218 lands).
    /// Does NOT gate H2 at the frontend — frontend H2 is ALPN-negotiated independently (see `alpn_protocols`).
    pub http2: Option<bool>,
    /// Per-cluster HTTP answer template overrides keyed by HTTP status
    /// code (e.g. `"503"`). Each value is either a filesystem path or an
    /// `inline:<body>` literal — see [`resolve_answer_source`]. Loaded
    /// into [`Cluster::answers`] at build time via [`load_answers`].
    ///
    /// Layering: an entry here overrides the listener-level
    /// `[listeners.<id>.answers]` default for the matching status on
    /// requests routed to this cluster. The listener-level map is the
    /// global default; the cluster-level map is the per-cluster
    /// override.
    pub answers: Option<BTreeMap<String, String>>,
    /// Optional explicit port to use when building the `Location` header
    /// for an `https_redirect`. When unset, the listener's effective HTTPS
    /// port is used. Lets operators front a non-standard HTTPS port (e.g.
    /// 8443) on the redirect target while keeping `https_redirect = true`.
    pub https_redirect_port: Option<u32>,
    /// Authorized credentials for HTTP basic authentication, formatted as
    /// `username:hex(sha256(password))` (lower-case hex). Empty list
    /// disables auth even when a frontend sets `required_auth = true` —
    /// such requests are rejected with 401.
    pub authorized_hashes: Option<Vec<String>>,
    /// Realm string emitted in `WWW-Authenticate: Basic realm="…"` when
    /// an unauthenticated request is rejected. Treated as an opaque
    /// value (no template substitution).
    pub www_authenticate: Option<String>,
    /// Override the global per-(cluster, source-IP) connection limit for
    /// this cluster. `None` (field absent) inherits the global default
    /// `max_connections_per_ip`. `Some(0)` is explicit "unlimited for
    /// this cluster". `Some(n > 0)` overrides with the cluster-specific
    /// limit. The source IP is taken from the parsed proxy-protocol
    /// header when present, else `peer_addr`.
    pub max_connections_per_ip: Option<u64>,
    /// Override the global `Retry-After` header value (seconds) emitted
    /// on HTTP 429 responses for this cluster. `None` inherits the global
    /// default. `Some(0)` omits the header. TCP clusters carry this
    /// field for shape uniformity but never emit the header (no HTTP
    /// envelope).
    pub retry_after: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendConfig {
    pub address: SocketAddr,
    pub weight: Option<u8>,
    pub sticky_id: Option<String>,
    pub backup: Option<bool>,
    pub backend_id: Option<String>,
}

impl FileClusterConfig {
    pub fn to_cluster_config(
        self,
        cluster_id: &str,
        expect_proxy: &HashSet<SocketAddr>,
    ) -> Result<ClusterConfig, ConfigError> {
        match self.protocol {
            FileClusterProtocolConfig::Tcp => {
                let mut has_expect_proxy = None;
                let mut frontends = Vec::new();
                for f in self.frontends {
                    if expect_proxy.contains(&f.address) {
                        match has_expect_proxy {
                            Some(true) => {}
                            Some(false) => {
                                return Err(ConfigError::Incompatible {
                                    object: ObjectKind::Cluster,
                                    id: cluster_id.to_owned(),
                                    kind: IncompatibilityKind::ProxyProtocol,
                                });
                            }
                            None => has_expect_proxy = Some(true),
                        }
                    } else {
                        match has_expect_proxy {
                            Some(false) => {}
                            Some(true) => {
                                return Err(ConfigError::Incompatible {
                                    object: ObjectKind::Cluster,
                                    id: cluster_id.to_owned(),
                                    kind: IncompatibilityKind::ProxyProtocol,
                                });
                            }
                            None => has_expect_proxy = Some(false),
                        }
                    }
                    let tcp_frontend = f.to_tcp_front()?;
                    frontends.push(tcp_frontend);
                }

                let send_proxy = self.send_proxy.unwrap_or(false);
                let expect_proxy = has_expect_proxy.unwrap_or(false);
                let proxy_protocol = match (send_proxy, expect_proxy) {
                    (true, true) => Some(ProxyProtocolConfig::RelayHeader),
                    (true, false) => Some(ProxyProtocolConfig::SendHeader),
                    (false, true) => Some(ProxyProtocolConfig::ExpectHeader),
                    _ => None,
                };

                let answers = match self.answers.as_ref() {
                    Some(map) => load_answers(map)?,
                    None => BTreeMap::new(),
                };

                Ok(ClusterConfig::Tcp(TcpClusterConfig {
                    cluster_id: cluster_id.to_string(),
                    frontends,
                    backends: self.backends,
                    proxy_protocol,
                    load_balancing: self.load_balancing,
                    load_metric: self.load_metric,
                    answers,
                    https_redirect_port: self.https_redirect_port,
                    authorized_hashes: self.authorized_hashes.unwrap_or_default(),
                    www_authenticate: self.www_authenticate,
                    max_connections_per_ip: self.max_connections_per_ip,
                    retry_after: self.retry_after,
                }))
            }
            FileClusterProtocolConfig::Http => {
                let mut frontends = Vec::new();
                for frontend in self.frontends {
                    let http_frontend = frontend.to_http_front(cluster_id)?;
                    frontends.push(http_frontend);
                }

                let answer_503 = self.answer_503.as_ref().and_then(|path| {
                    Config::load_file(path)
                        .map_err(|e| {
                            error!("cannot load 503 error page at path '{}': {:?}", path, e);
                            e
                        })
                        .ok()
                });

                let answers = match self.answers.as_ref() {
                    Some(map) => load_answers(map)?,
                    None => BTreeMap::new(),
                };

                Ok(ClusterConfig::Http(HttpClusterConfig {
                    cluster_id: cluster_id.to_string(),
                    frontends,
                    backends: self.backends,
                    sticky_session: self.sticky_session.unwrap_or(false),
                    https_redirect: self.https_redirect.unwrap_or(false),
                    load_balancing: self.load_balancing,
                    load_metric: self.load_metric,
                    answer_503,
                    http2: self.http2,
                    answers,
                    https_redirect_port: self.https_redirect_port,
                    authorized_hashes: self.authorized_hashes.unwrap_or_default(),
                    www_authenticate: self.www_authenticate,
                    max_connections_per_ip: self.max_connections_per_ip,
                    retry_after: self.retry_after,
                }))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpFrontendConfig {
    pub address: SocketAddr,
    pub hostname: String,
    pub path: PathRule,
    pub method: Option<String>,
    pub certificate: Option<String>,
    pub key: Option<String>,
    pub certificate_chain: Option<Vec<String>>,
    #[serde(default)]
    pub tls_versions: Vec<TlsVersion>,
    #[serde(default)]
    pub position: RulePosition,
    pub tags: Option<BTreeMap<String, String>>,
    /// Resolved redirect policy. `None` keeps the proto-default `FORWARD`.
    #[serde(default)]
    pub redirect: Option<RedirectPolicy>,
    /// Resolved redirect scheme. `None` keeps the proto-default `USE_SAME`.
    #[serde(default)]
    pub redirect_scheme: Option<RedirectScheme>,
    #[serde(default)]
    pub redirect_template: Option<String>,
    #[serde(default)]
    pub rewrite_host: Option<String>,
    #[serde(default)]
    pub rewrite_path: Option<String>,
    #[serde(default)]
    pub rewrite_port: Option<u32>,
    #[serde(default)]
    pub required_auth: Option<bool>,
    /// Header mutations applied to requests and/or responses passing through
    /// this frontend. Empty by default.
    #[serde(default)]
    pub headers: Vec<Header>,
}

impl HttpFrontendConfig {
    pub fn generate_requests(&self, cluster_id: &str) -> Vec<Request> {
        let mut v = Vec::new();

        let tags = self.tags.clone().unwrap_or_default();

        if self.key.is_some() && self.certificate.is_some() {
            v.push(
                RequestType::AddCertificate(AddCertificate {
                    address: self.address.into(),
                    certificate: CertificateAndKey {
                        key: self.key.clone().unwrap(),
                        certificate: self.certificate.clone().unwrap(),
                        certificate_chain: self.certificate_chain.clone().unwrap_or_default(),
                        versions: self.tls_versions.iter().map(|v| *v as i32).collect(),
                        // This field is used to override the certificate subject and san, we should not set it when
                        // loading the configuration, as we may provide a wildcard certificate for a specific domain.
                        // As a result, we will reject legit traffic for others domains as the certificate resolver will
                        // not load twice the same certificate and then do not register the certificate for others domains.
                        names: vec![],
                    },
                    expired_at: None,
                })
                .into(),
            );

            v.push(
                RequestType::AddHttpsFrontend(RequestHttpFrontend {
                    cluster_id: Some(cluster_id.to_string()),
                    address: self.address.into(),
                    hostname: self.hostname.clone(),
                    path: self.path.clone(),
                    method: self.method.clone(),
                    position: self.position.into(),
                    tags,
                    redirect: self.redirect.map(|r| r as i32),
                    required_auth: self.required_auth,
                    redirect_scheme: self.redirect_scheme.map(|s| s as i32),
                    redirect_template: self.redirect_template.clone(),
                    rewrite_host: self.rewrite_host.clone(),
                    rewrite_path: self.rewrite_path.clone(),
                    rewrite_port: self.rewrite_port,
                    headers: self.headers.clone(),
                })
                .into(),
            );
        } else {
            //create the front both for HTTP and HTTPS if possible
            v.push(
                RequestType::AddHttpFrontend(RequestHttpFrontend {
                    cluster_id: Some(cluster_id.to_string()),
                    address: self.address.into(),
                    hostname: self.hostname.clone(),
                    path: self.path.clone(),
                    method: self.method.clone(),
                    position: self.position.into(),
                    tags,
                    redirect: self.redirect.map(|r| r as i32),
                    required_auth: self.required_auth,
                    redirect_scheme: self.redirect_scheme.map(|s| s as i32),
                    redirect_template: self.redirect_template.clone(),
                    rewrite_host: self.rewrite_host.clone(),
                    rewrite_path: self.rewrite_path.clone(),
                    rewrite_port: self.rewrite_port,
                    headers: self.headers.clone(),
                })
                .into(),
            );
        }

        v
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpClusterConfig {
    pub cluster_id: String,
    pub frontends: Vec<HttpFrontendConfig>,
    pub backends: Vec<BackendConfig>,
    pub sticky_session: bool,
    pub https_redirect: bool,
    pub load_balancing: LoadBalancingAlgorithms,
    pub load_metric: Option<LoadMetric>,
    pub answer_503: Option<String>,
    pub http2: Option<bool>,
    /// Per-status template body map (already loaded from disk). Maps to
    /// the proto [`Cluster::answers`] field.
    #[serde(default)]
    pub answers: BTreeMap<String, String>,
    #[serde(default)]
    pub https_redirect_port: Option<u32>,
    #[serde(default)]
    pub authorized_hashes: Vec<String>,
    #[serde(default)]
    pub www_authenticate: Option<String>,
    /// Per-cluster override of the global `max_connections_per_ip`. See
    /// [`FileClusterConfig::max_connections_per_ip`] for semantics.
    #[serde(default)]
    pub max_connections_per_ip: Option<u64>,
    /// Per-cluster override of the global `retry_after` HTTP-429 header
    /// value (seconds). See [`FileClusterConfig::retry_after`].
    #[serde(default)]
    pub retry_after: Option<u32>,
}

impl HttpClusterConfig {
    pub fn generate_requests(&self) -> Result<Vec<Request>, ConfigError> {
        let mut v = vec![
            RequestType::AddCluster(Cluster {
                cluster_id: self.cluster_id.clone(),
                sticky_session: self.sticky_session,
                https_redirect: self.https_redirect,
                proxy_protocol: None,
                load_balancing: self.load_balancing as i32,
                answer_503: self.answer_503.clone(),
                load_metric: self.load_metric.map(|s| s as i32),
                http2: self.http2,
                answers: self.answers.clone(),
                https_redirect_port: self.https_redirect_port,
                authorized_hashes: self.authorized_hashes.clone(),
                www_authenticate: self.www_authenticate.clone(),
                max_connections_per_ip: self.max_connections_per_ip,
                retry_after: self.retry_after,
            })
            .into(),
        ];

        for frontend in &self.frontends {
            let mut orders = frontend.generate_requests(&self.cluster_id);
            v.append(&mut orders);
        }

        for (backend_count, backend) in self.backends.iter().enumerate() {
            let load_balancing_parameters = Some(LoadBalancingParams {
                weight: backend.weight.unwrap_or(100) as i32,
            });

            v.push(
                RequestType::AddBackend(AddBackend {
                    cluster_id: self.cluster_id.clone(),
                    backend_id: backend.backend_id.clone().unwrap_or_else(|| {
                        format!("{}-{}-{}", self.cluster_id, backend_count, backend.address)
                    }),
                    address: backend.address.into(),
                    load_balancing_parameters,
                    sticky_id: backend.sticky_id.clone(),
                    backup: backend.backup,
                })
                .into(),
            );
        }

        Ok(v)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TcpFrontendConfig {
    pub address: SocketAddr,
    pub tags: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TcpClusterConfig {
    pub cluster_id: String,
    pub frontends: Vec<TcpFrontendConfig>,
    pub backends: Vec<BackendConfig>,
    #[serde(default)]
    pub proxy_protocol: Option<ProxyProtocolConfig>,
    pub load_balancing: LoadBalancingAlgorithms,
    pub load_metric: Option<LoadMetric>,
    /// Per-status template body map (already loaded from disk). Even
    /// though TCP clusters do not emit HTTP responses, the field is
    /// carried for shape uniformity with [`HttpClusterConfig`].
    #[serde(default)]
    pub answers: BTreeMap<String, String>,
    #[serde(default)]
    pub https_redirect_port: Option<u32>,
    #[serde(default)]
    pub authorized_hashes: Vec<String>,
    #[serde(default)]
    pub www_authenticate: Option<String>,
    /// Per-cluster override of the global `max_connections_per_ip`. See
    /// [`FileClusterConfig::max_connections_per_ip`] for semantics.
    #[serde(default)]
    pub max_connections_per_ip: Option<u64>,
    /// Per-cluster override of the global `retry_after`. TCP listeners
    /// never emit `Retry-After`; the field is carried for shape
    /// uniformity with [`HttpClusterConfig`].
    #[serde(default)]
    pub retry_after: Option<u32>,
}

impl TcpClusterConfig {
    pub fn generate_requests(&self) -> Result<Vec<Request>, ConfigError> {
        let mut v = vec![
            RequestType::AddCluster(Cluster {
                cluster_id: self.cluster_id.clone(),
                sticky_session: false,
                https_redirect: false,
                proxy_protocol: self.proxy_protocol.map(|s| s as i32),
                load_balancing: self.load_balancing as i32,
                load_metric: self.load_metric.map(|s| s as i32),
                answer_503: None,
                http2: None,
                answers: self.answers.clone(),
                https_redirect_port: self.https_redirect_port,
                authorized_hashes: self.authorized_hashes.clone(),
                www_authenticate: self.www_authenticate.clone(),
                max_connections_per_ip: self.max_connections_per_ip,
                retry_after: self.retry_after,
            })
            .into(),
        ];

        for frontend in &self.frontends {
            v.push(
                RequestType::AddTcpFrontend(RequestTcpFrontend {
                    cluster_id: self.cluster_id.clone(),
                    address: frontend.address.into(),
                    tags: frontend.tags.clone().unwrap_or(BTreeMap::new()),
                })
                .into(),
            );
        }

        for (backend_count, backend) in self.backends.iter().enumerate() {
            let load_balancing_parameters = Some(LoadBalancingParams {
                weight: backend.weight.unwrap_or(100) as i32,
            });

            v.push(
                RequestType::AddBackend(AddBackend {
                    cluster_id: self.cluster_id.clone(),
                    backend_id: backend.backend_id.clone().unwrap_or_else(|| {
                        format!("{}-{}-{}", self.cluster_id, backend_count, backend.address)
                    }),
                    address: backend.address.into(),
                    load_balancing_parameters,
                    sticky_id: backend.sticky_id.clone(),
                    backup: backend.backup,
                })
                .into(),
            );
        }

        Ok(v)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ClusterConfig {
    Http(HttpClusterConfig),
    Tcp(TcpClusterConfig),
}

impl ClusterConfig {
    pub fn generate_requests(&self) -> Result<Vec<Request>, ConfigError> {
        match *self {
            ClusterConfig::Http(ref http) => http.generate_requests(),
            ClusterConfig::Tcp(ref tcp) => tcp.generate_requests(),
        }
    }
}

/// Parsed from the TOML config provided by the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default, Deserialize)]
pub struct FileConfig {
    pub command_socket: Option<String>,
    pub command_buffer_size: Option<u64>,
    pub max_command_buffer_size: Option<u64>,
    pub max_connections: Option<usize>,
    pub min_buffers: Option<u64>,
    pub max_buffers: Option<u64>,
    pub buffer_size: Option<u64>,
    /// Slab-entries-per-connection multiplier. `None` keeps the compile-time
    /// default of 4. Operator-visible escape hatch for fan-out topologies
    /// that exceed 4 backends per session — clamped to [2, 32] at load.
    #[serde(default)]
    pub slab_entries_per_connection: Option<u64>,
    /// Maximum length, in bytes, of a base64-decoded `Authorization: Basic`
    /// payload accepted by the worker's `mux::auth` module. Caps the
    /// per-failed-auth allocation so a hostile peer cannot force the worker
    /// to decode arbitrarily large tokens. RFC 7617 imposes no upper bound
    /// — defaults to 4096, which is well above the realistic
    /// `username:password` shape. Operators running hardened tenants can
    /// lower this to e.g. 256 or 512 to bound the allocation tighter.
    /// Values >= `buffer_size / 3` emit a warning at config-load time
    /// (the credential cap shouldn't dominate the per-frontend buffer).
    #[serde(default)]
    pub basic_auth_max_credential_bytes: Option<u64>,
    /// Default per-(cluster, source-IP) connection limit. `None` keeps
    /// `0` (unlimited). Each cluster may override via its own
    /// `max_connections_per_ip`. The source IP is taken from the parsed
    /// proxy-protocol header when present, else `peer_addr`. When the
    /// limit is reached, HTTP requests are answered with `429 Too Many
    /// Requests` (with optional `Retry-After`) and TCP sessions are
    /// closed gracefully without dialing the backend.
    #[serde(default)]
    pub max_connections_per_ip: Option<u64>,
    /// Default `Retry-After` header value (seconds) sent on HTTP 429
    /// responses. `Some(0)` or `None` keeping the default `0` omits the
    /// header (rendering `Retry-After: 0` invites an immediate retry that
    /// defeats the limit). Per-cluster overrides apply for HTTP listeners
    /// only. TCP listeners ignore this value (no HTTP envelope).
    #[serde(default)]
    pub retry_after: Option<u32>,
    /// Requested kernel-pipe capacity, in bytes, for each `splice(2)`
    /// zero-copy direction (Linux only, `splice` feature). `None` keeps
    /// the kernel default (64 KiB). Applied via `fcntl(F_SETPIPE_SZ)`;
    /// the kernel rounds up to a page boundary and clamps at
    /// `/proc/sys/fs/pipe-max-size` (default 1 MiB unprivileged). The
    /// realised capacity is read back via `fcntl(F_GETPIPE_SZ)` and
    /// drives the per-call `len` for `splice_in`. Ignored on non-Linux
    /// targets and on builds without the `splice` feature.
    #[serde(default)]
    pub splice_pipe_capacity_bytes: Option<u64>,
    /// Optional UID allowlist for command-socket requests. `None` (default)
    /// preserves historical behaviour: any same-UID local process can
    /// invoke any verb. When set, requests whose `SO_PEERCRED` UID is not
    /// in the list are rejected. Use to restrict mutating verbs to a
    /// specific operator UID even when other same-UID daemons coexist
    /// (CI runners, monitoring).
    #[serde(default)]
    pub command_allowed_uids: Option<Vec<u32>>,
    pub saved_state: Option<String>,
    #[serde(default)]
    pub automatic_state_save: Option<bool>,
    pub log_level: Option<String>,
    pub log_target: Option<String>,
    #[serde(default)]
    pub log_colored: bool,
    /// Dedicated file path for the control-plane audit log. When set, every
    /// emitted `[AUDIT]` / `Command(...)` line is also appended to this file
    /// opened `O_APPEND | O_CREAT` with mode `0o640` (owner read+write,
    /// group read, world nothing) so operators can separate the audit trail
    /// from the main log stream and protect it with group-scoped ACLs /
    /// logrotate. Independent of the standard `log_target`. `None` keeps
    /// audit lines routed only through the standard logger.
    #[serde(default)]
    pub audit_logs_target: Option<String>,
    /// Dedicated file path for a JSON-encoded mirror of the audit log.
    /// One JSON object per line so SIEM pipelines (Wazuh, Elastic, Loki)
    /// ingest without bespoke parsers. Same `O_APPEND | O_CREAT | 0o640`
    /// as `audit_logs_target`. `None` disables the JSON mirror.
    #[serde(default)]
    pub audit_logs_json_target: Option<String>,
    #[serde(default)]
    pub access_logs_target: Option<String>,
    #[serde(default)]
    pub access_logs_format: Option<AccessLogFormat>,
    #[serde(default)]
    pub access_logs_colored: Option<bool>,
    pub worker_count: Option<u16>,
    pub worker_automatic_restart: Option<bool>,
    pub metrics: Option<MetricsConfig>,
    pub disable_cluster_metrics: Option<bool>,
    pub listeners: Option<Vec<ListenerBuilder>>,
    pub clusters: Option<HashMap<String, FileClusterConfig>>,
    pub handle_process_affinity: Option<bool>,
    pub ctl_command_timeout: Option<u64>,
    pub pid_file_path: Option<String>,
    pub activate_listeners: Option<bool>,
    #[serde(default)]
    pub front_timeout: Option<u32>,
    #[serde(default)]
    pub back_timeout: Option<u32>,
    #[serde(default)]
    pub connect_timeout: Option<u32>,
    #[serde(default)]
    pub zombie_check_interval: Option<u32>,
    #[serde(default)]
    pub accept_queue_timeout: Option<u32>,
    #[serde(default)]
    pub evict_on_queue_full: Option<bool>,
    #[serde(default)]
    pub request_timeout: Option<u32>,
    #[serde(default)]
    pub worker_timeout: Option<u32>,
}

impl FileConfig {
    pub fn load_from_path(path: &str) -> Result<FileConfig, ConfigError> {
        let data = Config::load_file(path)?;

        let config: FileConfig = match toml::from_str(&data) {
            Ok(config) => config,
            Err(e) => {
                display_toml_error(&data, &e);
                return Err(ConfigError::DeserializeToml(e.to_string()));
            }
        };

        let mut reserved_address: HashSet<SocketAddr> = HashSet::new();

        if let Some(listeners) = config.listeners.as_ref() {
            for listener in listeners.iter() {
                if reserved_address.contains(&listener.address) {
                    return Err(ConfigError::ListenerAddressAlreadyInUse(listener.address));
                }
                reserved_address.insert(listener.address);
            }
        }

        //FIXME: verify how clusters and listeners share addresses
        /*
        if let Some(ref clusters) = config.clusters {
          for (key, cluster) in clusters.iter() {
            if let (Some(address), Some(port)) = (cluster.ip_address.clone(), cluster.port) {
              let addr = (address, port);
              if reserved_address.contains(&addr) {
                println!("TCP cluster '{}' listening address ( {}:{} ) is already used in the configuration",
                  key, addr.0, addr.1);
                return Err(Error::new(
                  ErrorKind::InvalidData,
                  format!("TCP cluster '{}' listening address ( {}:{} ) is already used in the configuration",
                    key, addr.0, addr.1)));
              } else {
                reserved_address.insert(addr.clone());
              }
            }
          }
        }
        */

        Ok(config)
    }
}

/// A builder that converts [FileConfig] to [Config]
pub struct ConfigBuilder {
    file: FileConfig,
    known_addresses: HashMap<SocketAddr, ListenerProtocol>,
    expect_proxy_addresses: HashSet<SocketAddr>,
    built: Config,
}

impl ConfigBuilder {
    /// starts building a [Config] with values from a [FileConfig], or defaults.
    ///
    /// please provide a config path, usefull for rebuilding the config later.
    pub fn new<S>(file_config: FileConfig, config_path: S) -> Self
    where
        S: ToString,
    {
        let built = Config {
            accept_queue_timeout: file_config
                .accept_queue_timeout
                .unwrap_or(DEFAULT_ACCEPT_QUEUE_TIMEOUT),
            evict_on_queue_full: file_config
                .evict_on_queue_full
                .unwrap_or(DEFAULT_EVICT_ON_QUEUE_FULL),
            activate_listeners: file_config.activate_listeners.unwrap_or(true),
            automatic_state_save: file_config
                .automatic_state_save
                .unwrap_or(DEFAULT_AUTOMATIC_STATE_SAVE),
            back_timeout: file_config.back_timeout.unwrap_or(DEFAULT_BACK_TIMEOUT),
            buffer_size: file_config.buffer_size.unwrap_or(DEFAULT_BUFFER_SIZE),
            command_buffer_size: file_config
                .command_buffer_size
                .unwrap_or(DEFAULT_COMMAND_BUFFER_SIZE),
            config_path: config_path.to_string(),
            connect_timeout: file_config
                .connect_timeout
                .unwrap_or(DEFAULT_CONNECT_TIMEOUT),
            ctl_command_timeout: file_config.ctl_command_timeout.unwrap_or(1_000),
            front_timeout: file_config.front_timeout.unwrap_or(DEFAULT_FRONT_TIMEOUT),
            handle_process_affinity: file_config.handle_process_affinity.unwrap_or(false),
            access_logs_target: file_config.access_logs_target.clone(),
            audit_logs_target: file_config.audit_logs_target.clone(),
            audit_logs_json_target: file_config.audit_logs_json_target.clone(),
            access_logs_format: file_config.access_logs_format.clone(),
            access_logs_colored: file_config.access_logs_colored,
            log_level: file_config
                .log_level
                .clone()
                .unwrap_or_else(|| String::from("info")),
            log_target: file_config
                .log_target
                .clone()
                .unwrap_or_else(|| String::from("stdout")),
            log_colored: file_config.log_colored,
            max_buffers: file_config.max_buffers.unwrap_or(DEFAULT_MAX_BUFFERS),
            max_command_buffer_size: file_config
                .max_command_buffer_size
                .unwrap_or(DEFAULT_MAX_COMMAND_BUFFER_SIZE),
            max_connections: file_config
                .max_connections
                .unwrap_or(DEFAULT_MAX_CONNECTIONS),
            metrics: file_config.metrics.clone(),
            disable_cluster_metrics: file_config
                .disable_cluster_metrics
                .unwrap_or(DEFAULT_DISABLE_CLUSTER_METRICS),
            min_buffers: std::cmp::min(
                file_config.min_buffers.unwrap_or(DEFAULT_MIN_BUFFERS),
                file_config.max_buffers.unwrap_or(DEFAULT_MAX_BUFFERS),
            ),
            pid_file_path: file_config.pid_file_path.clone(),
            request_timeout: file_config
                .request_timeout
                .unwrap_or(DEFAULT_REQUEST_TIMEOUT),
            saved_state: file_config.saved_state.clone(),
            worker_automatic_restart: file_config
                .worker_automatic_restart
                .unwrap_or(DEFAULT_WORKER_AUTOMATIC_RESTART),
            worker_count: file_config.worker_count.unwrap_or(DEFAULT_WORKER_COUNT),
            zombie_check_interval: file_config
                .zombie_check_interval
                .unwrap_or(DEFAULT_ZOMBIE_CHECK_INTERVAL),
            worker_timeout: file_config.worker_timeout.unwrap_or(DEFAULT_WORKER_TIMEOUT),
            slab_entries_per_connection: file_config.slab_entries_per_connection.map(|n| {
                n.clamp(
                    ServerConfig::MIN_SLAB_ENTRIES_PER_CONNECTION,
                    ServerConfig::MAX_SLAB_ENTRIES_PER_CONNECTION,
                )
            }),
            command_allowed_uids: file_config.command_allowed_uids.clone(),
            basic_auth_max_credential_bytes: file_config.basic_auth_max_credential_bytes,
            max_connections_per_ip: file_config
                .max_connections_per_ip
                .unwrap_or(DEFAULT_MAX_CONNECTIONS_PER_IP),
            retry_after: file_config.retry_after.unwrap_or(DEFAULT_RETRY_AFTER),
            splice_pipe_capacity_bytes: file_config.splice_pipe_capacity_bytes,
            ..Default::default()
        };

        Self {
            file: file_config,
            known_addresses: HashMap::new(),
            expect_proxy_addresses: HashSet::new(),
            built,
        }
    }

    fn push_tls_listener(&mut self, mut listener: ListenerBuilder) -> Result<(), ConfigError> {
        let listener = listener.to_tls(Some(&self.built))?;
        self.built.https_listeners.push(listener);
        Ok(())
    }

    fn push_http_listener(&mut self, mut listener: ListenerBuilder) -> Result<(), ConfigError> {
        let listener = listener.to_http(Some(&self.built))?;
        self.built.http_listeners.push(listener);
        Ok(())
    }

    fn push_tcp_listener(&mut self, mut listener: ListenerBuilder) -> Result<(), ConfigError> {
        let listener = listener.to_tcp(Some(&self.built))?;
        self.built.tcp_listeners.push(listener);
        Ok(())
    }

    fn populate_listeners(&mut self, listeners: Vec<ListenerBuilder>) -> Result<(), ConfigError> {
        for listener in listeners.iter() {
            if self.known_addresses.contains_key(&listener.address) {
                return Err(ConfigError::ListenerAddressAlreadyInUse(listener.address));
            }

            let protocol = listener
                .protocol
                .ok_or(ConfigError::Missing(MissingKind::Protocol))?;

            self.known_addresses.insert(listener.address, protocol);
            if listener.expect_proxy == Some(true) {
                self.expect_proxy_addresses.insert(listener.address);
            }

            if listener.public_address.is_some() && listener.expect_proxy == Some(true) {
                return Err(ConfigError::Incompatible {
                    object: ObjectKind::Listener,
                    id: listener.address.to_string(),
                    kind: IncompatibilityKind::PublicAddress,
                });
            }

            match protocol {
                ListenerProtocol::Https => self.push_tls_listener(listener.clone())?,
                ListenerProtocol::Http => self.push_http_listener(listener.clone())?,
                ListenerProtocol::Tcp => self.push_tcp_listener(listener.clone())?,
            }
        }
        Ok(())
    }

    fn populate_clusters(
        &mut self,
        mut file_cluster_configs: HashMap<String, FileClusterConfig>,
    ) -> Result<(), ConfigError> {
        for (id, file_cluster_config) in file_cluster_configs.drain() {
            let mut cluster_config =
                file_cluster_config.to_cluster_config(id.as_str(), &self.expect_proxy_addresses)?;

            match cluster_config {
                ClusterConfig::Http(ref mut http) => {
                    for frontend in http.frontends.iter_mut() {
                        match self.known_addresses.get(&frontend.address) {
                            Some(ListenerProtocol::Tcp) => {
                                return Err(ConfigError::WrongFrontendProtocol(
                                    ListenerProtocol::Tcp,
                                ));
                            }
                            Some(ListenerProtocol::Http) => {
                                if frontend.certificate.is_some() {
                                    return Err(ConfigError::WrongFrontendProtocol(
                                        ListenerProtocol::Http,
                                    ));
                                }
                            }
                            Some(ListenerProtocol::Https) => {
                                if frontend.certificate.is_none() {
                                    if let Some(https_listener) =
                                        self.built.https_listeners.iter().find(|listener| {
                                            listener.address == frontend.address.into()
                                                && listener.certificate.is_some()
                                        })
                                    {
                                        //println!("using listener certificate for {:}", frontend.address);
                                        frontend
                                            .certificate
                                            .clone_from(&https_listener.certificate);
                                        frontend.certificate_chain =
                                            Some(https_listener.certificate_chain.clone());
                                        frontend.key.clone_from(&https_listener.key);
                                    }
                                    if frontend.certificate.is_none() {
                                        debug!("known addresses: {:?}", self.known_addresses);
                                        debug!("frontend: {:?}", frontend);
                                        return Err(ConfigError::WrongFrontendProtocol(
                                            ListenerProtocol::Https,
                                        ));
                                    }
                                }
                            }
                            None => {
                                // create a default listener for that front
                                let file_listener_protocol = if frontend.certificate.is_some() {
                                    self.push_tls_listener(ListenerBuilder::new(
                                        frontend.address.into(),
                                        ListenerProtocol::Https,
                                    ))?;

                                    ListenerProtocol::Https
                                } else {
                                    self.push_http_listener(ListenerBuilder::new(
                                        frontend.address.into(),
                                        ListenerProtocol::Http,
                                    ))?;

                                    ListenerProtocol::Http
                                };
                                self.known_addresses
                                    .insert(frontend.address, file_listener_protocol);
                            }
                        }
                    }
                }
                ClusterConfig::Tcp(ref tcp) => {
                    //FIXME: verify that different TCP clusters do not request the same address
                    for frontend in &tcp.frontends {
                        match self.known_addresses.get(&frontend.address) {
                            Some(ListenerProtocol::Http) | Some(ListenerProtocol::Https) => {
                                return Err(ConfigError::WrongFrontendProtocol(
                                    ListenerProtocol::Http,
                                ));
                            }
                            Some(ListenerProtocol::Tcp) => {}
                            None => {
                                // create a default listener for that front
                                self.push_tcp_listener(ListenerBuilder::new(
                                    frontend.address.into(),
                                    ListenerProtocol::Tcp,
                                ))?;
                                self.known_addresses
                                    .insert(frontend.address, ListenerProtocol::Tcp);
                            }
                        }
                    }
                }
            }

            self.built.clusters.insert(id, cluster_config);
        }
        Ok(())
    }

    /// Builds a [`Config`], populated with listeners and clusters
    pub fn into_config(&mut self) -> Result<Config, ConfigError> {
        if let Some(listeners) = &self.file.listeners {
            self.populate_listeners(listeners.clone())?;
        }

        if let Some(file_cluster_configs) = &self.file.clusters {
            self.populate_clusters(file_cluster_configs.clone())?;
        }

        // RFC 9113 §6.5.2 + §4.1: the H2 mux must accept up to
        // SETTINGS_MAX_FRAME_SIZE (16 384) + 9-byte frame header in a single
        // kawa buffer. If any HTTPS listener advertises "h2" in its ALPN list
        // and the global buffer_size is below H2_MIN_BUFFER_SIZE, the mux
        // deadlocks on full-size DATA / HEADERS / CONTINUATION frames until
        // the session timeout fires. Reject at config load so the failure
        // mode surfaces at boot, not under traffic.
        // Long-form rationale: `lib/src/protocol/mux/LIFECYCLE.md`.
        let h2_listeners = self
            .built
            .https_listeners
            .iter()
            .filter(|l| l.alpn_protocols.iter().any(|p| p == "h2"))
            .count();
        if h2_listeners > 0 && self.built.buffer_size < H2_MIN_BUFFER_SIZE {
            return Err(ConfigError::BufferSizeTooSmallForH2 {
                buffer_size: self.built.buffer_size,
                minimum: H2_MIN_BUFFER_SIZE,
                listeners: h2_listeners,
            });
        }

        // Warn (no hard reject) when the configured Basic-auth credential
        // cap is large enough to dominate the per-frontend buffer. The
        // worker copies a decoded credential into a transient allocation
        // sized by this cap; values >= 33% of `buffer_size` mean a single
        // failed-auth attempt can hold a third of the buffer's worth of
        // bytes, which combined with in-flight request/response framing
        // pushes the buffer toward back-pressure under load. Log only —
        // operators with deliberate threat models may choose this
        // trade-off, but the surprise needs to be visible.
        if let Some(cap) = self.built.basic_auth_max_credential_bytes {
            let third = self.built.buffer_size / 3;
            if cap >= third {
                warn!(
                    "basic_auth_max_credential_bytes = {} is >= buffer_size / 3 ({}); \
                     a hostile peer can pin ~33% of the per-frontend buffer per failed auth \
                     attempt. Consider lowering basic_auth_max_credential_bytes (typical \
                     credentials are <100 bytes) or raising buffer_size.",
                    cap, third
                );
            }
        }

        // The eviction batch is `(max_connections / 100).max(1)` — a 1% ratio
        // by design. Below 100 connections the floor of 1 means each cap
        // event evicts a larger share than 1% of capacity (e.g. 4% at
        // max_connections=25), which can surprise an operator who reads the
        // knob as "1% per round". Warn at config load so the discrepancy is
        // visible at boot, not under traffic.
        if self.built.evict_on_queue_full && self.built.max_connections < 100 {
            let pct = 100usize.div_ceil(self.built.max_connections);
            warn!(
                "evict_on_queue_full enabled with max_connections = {}; the eviction batch \
                 clamps to 1, equivalent to ~{}% of capacity per cap event (the knob is \
                 documented as 1%). Confirm this is intended.",
                self.built.max_connections, pct
            );
        }

        let command_socket_path = self.file.command_socket.clone().unwrap_or({
            let mut path = env::current_dir().map_err(|e| ConfigError::Env(e.to_string()))?;
            path.push("sozu.sock");
            let verified_path = path
                .to_str()
                .ok_or(ConfigError::InvalidPath(path.clone()))?;
            verified_path.to_owned()
        });

        if let (None, Some(true)) = (&self.file.saved_state, &self.file.automatic_state_save) {
            return Err(ConfigError::Missing(MissingKind::SavedState));
        }

        Ok(Config {
            command_socket: command_socket_path,
            ..self.built.clone()
        })
    }
}

/// Sōzu configuration, populated with clusters and listeners.
///
/// This struct is used on startup to generate `WorkerRequest`s
#[derive(Clone, PartialEq, Eq, Serialize, Default, Deserialize)]
pub struct Config {
    pub config_path: String,
    pub command_socket: String,
    pub command_buffer_size: u64,
    pub max_command_buffer_size: u64,
    pub max_connections: usize,
    pub min_buffers: u64,
    pub max_buffers: u64,
    pub buffer_size: u64,
    pub saved_state: Option<String>,
    #[serde(default)]
    pub automatic_state_save: bool,
    pub log_level: String,
    pub log_target: String,
    pub log_colored: bool,
    /// Optional dedicated file path for the control-plane audit log. See
    /// `FileConfig::audit_logs_target` for rationale.
    #[serde(default)]
    pub audit_logs_target: Option<String>,
    /// Optional JSON mirror of the audit log; see
    /// `FileConfig::audit_logs_json_target`.
    #[serde(default)]
    pub audit_logs_json_target: Option<String>,
    #[serde(default)]
    pub access_logs_target: Option<String>,
    pub access_logs_format: Option<AccessLogFormat>,
    pub access_logs_colored: Option<bool>,
    pub worker_count: u16,
    pub worker_automatic_restart: bool,
    pub metrics: Option<MetricsConfig>,
    #[serde(default = "default_disable_cluster_metrics")]
    pub disable_cluster_metrics: bool,
    pub http_listeners: Vec<HttpListenerConfig>,
    pub https_listeners: Vec<HttpsListenerConfig>,
    pub tcp_listeners: Vec<TcpListenerConfig>,
    pub clusters: HashMap<String, ClusterConfig>,
    pub handle_process_affinity: bool,
    pub ctl_command_timeout: u64,
    pub pid_file_path: Option<String>,
    pub activate_listeners: bool,
    #[serde(default = "default_front_timeout")]
    pub front_timeout: u32,
    #[serde(default = "default_back_timeout")]
    pub back_timeout: u32,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout: u32,
    #[serde(default = "default_zombie_check_interval")]
    pub zombie_check_interval: u32,
    #[serde(default = "default_accept_queue_timeout")]
    pub accept_queue_timeout: u32,
    #[serde(default = "default_evict_on_queue_full")]
    pub evict_on_queue_full: bool,
    #[serde(default = "default_request_timeout")]
    pub request_timeout: u32,
    #[serde(default = "default_worker_timeout")]
    pub worker_timeout: u32,
    /// Slab-entries-per-connection multiplier exposed for operators with
    /// fan-out topologies that exceed the default 4 backends per session.
    /// `None` means the default (4) applies; set values are clamped to
    /// [`ServerConfig::MIN_SLAB_ENTRIES_PER_CONNECTION`,
    /// `ServerConfig::MAX_SLAB_ENTRIES_PER_CONNECTION`] = [2, 32]. Slab
    /// capacity is `10 + slab_entries_per_connection * max_connections`.
    #[serde(default)]
    pub slab_entries_per_connection: Option<u64>,
    /// Optional allowlist of UIDs permitted to invoke command-socket
    /// requests. `None` keeps the historical "any same-UID local process"
    /// behaviour. When `Some`, every request whose `SO_PEERCRED` UID is
    /// not in the list is rejected before reaching dispatch.
    #[serde(default)]
    pub command_allowed_uids: Option<Vec<u32>>,
    /// Maximum length, in bytes, of a base64-decoded `Authorization: Basic`
    /// payload accepted by `mux::auth`. `None` keeps the compile-time
    /// default of 4096. Set once on each worker at boot via
    /// [`ServerConfig::basic_auth_max_credential_bytes`].
    #[serde(default)]
    pub basic_auth_max_credential_bytes: Option<u64>,
    /// Default per-(cluster, source-IP) connection limit. `0` means
    /// unlimited. Each cluster may override via its own
    /// `max_connections_per_ip`. Source IP attribution honours the
    /// proxy-protocol header when present.
    #[serde(default = "default_max_connections_per_ip")]
    pub max_connections_per_ip: u64,
    /// Default `Retry-After` header value (seconds) emitted on HTTP 429
    /// responses. `0` omits the header.
    #[serde(default = "default_retry_after")]
    pub retry_after: u32,
    /// Requested kernel-pipe capacity, in bytes, for each `splice(2)`
    /// zero-copy direction. `None` keeps the kernel default of 64 KiB.
    /// Applied via `fcntl(F_SETPIPE_SZ)` per pipe at `SplicePipe::new`;
    /// the kernel rounds up to a page boundary and clamps at
    /// `/proc/sys/fs/pipe-max-size`. Linux-only; ignored on builds
    /// without the `splice` feature.
    #[serde(default)]
    pub splice_pipe_capacity_bytes: Option<u64>,
}

fn default_front_timeout() -> u32 {
    DEFAULT_FRONT_TIMEOUT
}

fn default_back_timeout() -> u32 {
    DEFAULT_BACK_TIMEOUT
}

fn default_connect_timeout() -> u32 {
    DEFAULT_CONNECT_TIMEOUT
}

fn default_request_timeout() -> u32 {
    DEFAULT_REQUEST_TIMEOUT
}

fn default_zombie_check_interval() -> u32 {
    DEFAULT_ZOMBIE_CHECK_INTERVAL
}

fn default_accept_queue_timeout() -> u32 {
    DEFAULT_ACCEPT_QUEUE_TIMEOUT
}

fn default_evict_on_queue_full() -> bool {
    DEFAULT_EVICT_ON_QUEUE_FULL
}

fn default_disable_cluster_metrics() -> bool {
    DEFAULT_DISABLE_CLUSTER_METRICS
}

fn default_worker_timeout() -> u32 {
    DEFAULT_WORKER_TIMEOUT
}

fn default_max_connections_per_ip() -> u64 {
    DEFAULT_MAX_CONNECTIONS_PER_IP
}

fn default_retry_after() -> u32 {
    DEFAULT_RETRY_AFTER
}

impl Config {
    /// Parse a TOML file and build a config out of it
    pub fn load_from_path(path: &str) -> Result<Config, ConfigError> {
        let file_config = FileConfig::load_from_path(path)?;

        let mut config = ConfigBuilder::new(file_config, path).into_config()?;

        // replace saved_state with a verified path
        config.saved_state = config.saved_state_path()?;

        Ok(config)
    }

    /// yields requests intended to recreate a proxy that match the config
    pub fn generate_config_messages(&self) -> Result<Vec<WorkerRequest>, ConfigError> {
        let mut v = Vec::new();
        let mut count = 0u8;

        for listener in &self.http_listeners {
            v.push(WorkerRequest {
                id: format!("CONFIG-{count}"),
                content: RequestType::AddHttpListener(listener.clone()).into(),
            });
            count += 1;
        }

        for listener in &self.https_listeners {
            v.push(WorkerRequest {
                id: format!("CONFIG-{count}"),
                content: RequestType::AddHttpsListener(listener.clone()).into(),
            });
            count += 1;
        }

        for listener in &self.tcp_listeners {
            v.push(WorkerRequest {
                id: format!("CONFIG-{count}"),
                content: RequestType::AddTcpListener(*listener).into(),
            });
            count += 1;
        }

        for cluster in self.clusters.values() {
            let mut orders = cluster.generate_requests()?;
            for content in orders.drain(..) {
                v.push(WorkerRequest {
                    id: format!("CONFIG-{count}"),
                    content,
                });
                count += 1;
            }
        }

        if self.activate_listeners {
            for listener in &self.http_listeners {
                v.push(WorkerRequest {
                    id: format!("CONFIG-{count}"),
                    content: RequestType::ActivateListener(ActivateListener {
                        address: listener.address,
                        proxy: ListenerType::Http.into(),
                        from_scm: false,
                    })
                    .into(),
                });
                count += 1;
            }

            for listener in &self.https_listeners {
                v.push(WorkerRequest {
                    id: format!("CONFIG-{count}"),
                    content: RequestType::ActivateListener(ActivateListener {
                        address: listener.address,
                        proxy: ListenerType::Https.into(),
                        from_scm: false,
                    })
                    .into(),
                });
                count += 1;
            }

            for listener in &self.tcp_listeners {
                v.push(WorkerRequest {
                    id: format!("CONFIG-{count}"),
                    content: RequestType::ActivateListener(ActivateListener {
                        address: listener.address,
                        proxy: ListenerType::Tcp.into(),
                        from_scm: false,
                    })
                    .into(),
                });
                count += 1;
            }
        }

        if self.disable_cluster_metrics {
            v.push(WorkerRequest {
                id: format!("CONFIG-{count}"),
                content: RequestType::ConfigureMetrics(MetricsConfiguration::Disabled.into())
                    .into(),
            });
            // count += 1; // uncomment if code is added below
        }

        Ok(v)
    }

    /// Get the path of the UNIX socket used to communicate with Sōzu
    pub fn command_socket_path(&self) -> Result<String, ConfigError> {
        let config_path_buf = PathBuf::from(self.config_path.clone());
        let mut config_dir = config_path_buf
            .parent()
            .ok_or(ConfigError::NoFileParent(
                config_path_buf.to_string_lossy().to_string(),
            ))?
            .to_path_buf();

        let socket_path = PathBuf::from(self.command_socket.clone());

        let mut socket_parent_dir = match socket_path.parent() {
            // if the socket path is of the form "./sozu.sock",
            // then the parent is the directory where config.toml is situated
            None => config_dir,
            Some(path) => {
                // concatenate the config directory and the relative path of the socket
                config_dir.push(path);
                // canonicalize to remove double dots like /path/to/config/directory/../../path/to/socket/directory/
                config_dir.canonicalize().map_err(|io_error| {
                    ConfigError::SocketPathError(format!(
                        "Could not canonicalize path {config_dir:?}: {io_error}"
                    ))
                })?
            }
        };

        let socket_name = socket_path
            .file_name()
            .ok_or(ConfigError::SocketPathError(format!(
                "could not get command socket file name from {socket_path:?}"
            )))?;

        // concatenate parent directory and socket file name
        socket_parent_dir.push(socket_name);

        let command_socket_path = socket_parent_dir
            .to_str()
            .ok_or(ConfigError::SocketPathError(format!(
                "Invalid socket path {socket_parent_dir:?}"
            )))?
            .to_string();

        Ok(command_socket_path)
    }

    /// Get the path of where the state will be saved
    fn saved_state_path(&self) -> Result<Option<String>, ConfigError> {
        let path = match self.saved_state.as_ref() {
            Some(path) => path,
            None => return Ok(None),
        };

        debug!("saved_stated path in the config: {}", path);
        let config_path = PathBuf::from(self.config_path.clone());

        debug!("Config path buffer: {:?}", config_path);
        let config_dir = config_path
            .parent()
            .ok_or(ConfigError::SaveStatePath(format!(
                "Could get parent directory of config file {config_path:?}"
            )))?;

        debug!("Config folder: {:?}", config_dir);
        if !config_dir.exists() {
            create_dir_all(config_dir).map_err(|io_error| {
                ConfigError::SaveStatePath(format!(
                    "failed to create state parent directory '{config_dir:?}': {io_error}"
                ))
            })?;
        }

        let mut saved_state_path_raw = config_dir.to_path_buf();
        saved_state_path_raw.push(path);
        debug!(
            "Looking for saved state on the path {:?}",
            saved_state_path_raw
        );

        match metadata(path) {
            Err(err) if matches!(err.kind(), ErrorKind::NotFound) => {
                info!("Create an empty state file at '{}'", path);
                File::create(path).map_err(|io_error| {
                    ConfigError::SaveStatePath(format!(
                        "failed to create state file '{path:?}': {io_error}"
                    ))
                })?;
            }
            _ => {}
        }

        saved_state_path_raw.canonicalize().map_err(|io_error| {
            ConfigError::SaveStatePath(format!(
                "could not get saved state path from config file input {path:?}: {io_error}"
            ))
        })?;

        let stringified_path = saved_state_path_raw
            .to_str()
            .ok_or(ConfigError::SaveStatePath(format!(
                "Invalid path {saved_state_path_raw:?}"
            )))?
            .to_string();

        Ok(Some(stringified_path))
    }

    /// read any file to a string
    pub fn load_file(path: &str) -> Result<String, ConfigError> {
        std::fs::read_to_string(path).map_err(|io_error| ConfigError::FileRead {
            path_to_read: path.to_owned(),
            io_error,
        })
    }

    /// read any file to bytes
    pub fn load_file_bytes(path: &str) -> Result<Vec<u8>, ConfigError> {
        std::fs::read(path).map_err(|io_error| ConfigError::FileRead {
            path_to_read: path.to_owned(),
            io_error,
        })
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("config_path", &self.config_path)
            .field("command_socket", &self.command_socket)
            .field("command_buffer_size", &self.command_buffer_size)
            .field("max_command_buffer_size", &self.max_command_buffer_size)
            .field("max_connections", &self.max_connections)
            .field("min_buffers", &self.min_buffers)
            .field("max_buffers", &self.max_buffers)
            .field("buffer_size", &self.buffer_size)
            .field("saved_state", &self.saved_state)
            .field("automatic_state_save", &self.automatic_state_save)
            .field("log_level", &self.log_level)
            .field("log_target", &self.log_target)
            .field("access_logs_target", &self.access_logs_target)
            .field("audit_logs_target", &self.audit_logs_target)
            .field("audit_logs_json_target", &self.audit_logs_json_target)
            .field("access_logs_format", &self.access_logs_format)
            .field("worker_count", &self.worker_count)
            .field("worker_automatic_restart", &self.worker_automatic_restart)
            .field("metrics", &self.metrics)
            .field("disable_cluster_metrics", &self.disable_cluster_metrics)
            .field("handle_process_affinity", &self.handle_process_affinity)
            .field("ctl_command_timeout", &self.ctl_command_timeout)
            .field("pid_file_path", &self.pid_file_path)
            .field("activate_listeners", &self.activate_listeners)
            .field("front_timeout", &self.front_timeout)
            .field("back_timeout", &self.back_timeout)
            .field("connect_timeout", &self.connect_timeout)
            .field("zombie_check_interval", &self.zombie_check_interval)
            .field("accept_queue_timeout", &self.accept_queue_timeout)
            .field("evict_on_queue_full", &self.evict_on_queue_full)
            .field("request_timeout", &self.request_timeout)
            .field("worker_timeout", &self.worker_timeout)
            .finish()
    }
}

fn display_toml_error(file: &str, error: &toml::de::Error) {
    println!("error parsing the configuration file '{file}': {error}");
    if let Some(Range { start, end }) = error.span() {
        print!("error parsing the configuration file '{file}' at position: {start}, {end}");
    }
}

impl ServerConfig {
    /// Default number of slab entries per connection. Set to 4 to accommodate
    /// H2 multiplexing (1 frontend + up to 3 backend connections per
    /// frontend with stream multiplexing). Previous value was 2 for H1-only
    /// operation. Operators with topologies that fan out across more
    /// clusters per session can override via `slab_entries_per_connection`
    /// in the config (clamped to [2, 32]).
    pub const DEFAULT_SLAB_ENTRIES_PER_CONNECTION: u64 = 4;
    /// Lower bound for the runtime knob. Below 2 the slab cannot hold one
    /// frontend + one backend per session.
    pub const MIN_SLAB_ENTRIES_PER_CONNECTION: u64 = 2;
    /// Upper bound for the runtime knob. 32 caps memory blow-up from a
    /// runaway config; 32 backends per frontend covers any sane topology.
    pub const MAX_SLAB_ENTRIES_PER_CONNECTION: u64 = 32;

    /// Effective slab-entries-per-connection. Applies the [MIN, MAX] clamp
    /// and falls back to the default when the proto field is absent or 0.
    pub fn effective_slab_entries_per_connection(&self) -> u64 {
        match self.slab_entries_per_connection {
            Some(0) | None => Self::DEFAULT_SLAB_ENTRIES_PER_CONNECTION,
            Some(n) => n.clamp(
                Self::MIN_SLAB_ENTRIES_PER_CONNECTION,
                Self::MAX_SLAB_ENTRIES_PER_CONNECTION,
            ),
        }
    }

    /// Size of the slab for the Session manager.
    ///
    /// With HTTP/2 multiplexing, each frontend session can have multiple backend
    /// connections (one per cluster), so we allocate
    /// [`Self::effective_slab_entries_per_connection`] entries per connection
    /// instead of the old H1-only multiplier of 2.
    pub fn slab_capacity(&self) -> u64 {
        10 + self.effective_slab_entries_per_connection() * self.max_connections
    }
}

/// reduce the config to the bare minimum needed by a worker
impl From<&Config> for ServerConfig {
    fn from(config: &Config) -> Self {
        let metrics = config.metrics.clone().map(|m| ServerMetricsConfig {
            address: m.address.to_string(),
            tagged_metrics: m.tagged_metrics,
            prefix: m.prefix,
            detail: Some(MetricDetail::from(m.detail) as i32),
        });
        Self {
            max_connections: config.max_connections as u64,
            front_timeout: config.front_timeout,
            back_timeout: config.back_timeout,
            connect_timeout: config.connect_timeout,
            zombie_check_interval: config.zombie_check_interval,
            accept_queue_timeout: config.accept_queue_timeout,
            min_buffers: config.min_buffers,
            max_buffers: config.max_buffers,
            buffer_size: config.buffer_size,
            log_level: config.log_level.clone(),
            log_target: config.log_target.clone(),
            access_logs_target: config.access_logs_target.clone(),
            audit_logs_target: config.audit_logs_target.clone(),
            audit_logs_json_target: config.audit_logs_json_target.clone(),
            command_buffer_size: config.command_buffer_size,
            max_command_buffer_size: config.max_command_buffer_size,
            metrics,
            access_log_format: ProtobufAccessLogFormat::from(&config.access_logs_format) as i32,
            log_colored: config.log_colored,
            slab_entries_per_connection: config.slab_entries_per_connection,
            basic_auth_max_credential_bytes: config.basic_auth_max_credential_bytes,
            evict_on_queue_full: Some(config.evict_on_queue_full),
            max_connections_per_ip: Some(config.max_connections_per_ip),
            retry_after: Some(config.retry_after),
            splice_pipe_capacity_bytes: config.splice_pipe_capacity_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use toml::to_string;

    use super::*;

    #[test]
    fn serialize() {
        let http = ListenerBuilder::new(
            SocketAddress::new_v4(127, 0, 0, 1, 8080),
            ListenerProtocol::Http,
        )
        .with_answer_404_path(Some("404.html"))
        .to_owned();
        println!("http: {:?}", to_string(&http));

        let https = ListenerBuilder::new(
            SocketAddress::new_v4(127, 0, 0, 1, 8443),
            ListenerProtocol::Https,
        )
        .with_answer_404_path(Some("404.html"))
        .to_owned();
        println!("https: {:?}", to_string(&https));

        let listeners = vec![http, https];
        let config = FileConfig {
            command_socket: Some(String::from("./command_folder/sock")),
            worker_count: Some(2),
            worker_automatic_restart: Some(true),
            max_connections: Some(500),
            min_buffers: Some(1),
            max_buffers: Some(500),
            buffer_size: Some(16393),
            metrics: Some(MetricsConfig {
                address: "127.0.0.1:8125".parse().unwrap(),
                tagged_metrics: false,
                prefix: Some(String::from("sozu-metrics")),
                detail: MetricDetailLevel::default(),
            }),
            listeners: Some(listeners),
            ..Default::default()
        };

        println!("config: {:?}", to_string(&config));
        let encoded = to_string(&config).unwrap();
        println!("conf:\n{encoded}");
    }

    #[test]
    fn parse() {
        let path = "assets/config.toml";
        let config = Config::load_from_path(path).unwrap_or_else(|load_error| {
            panic!("Cannot load config from path {path}: {load_error:?}")
        });
        println!("config: {config:#?}");
        //panic!();
    }

    #[test]
    fn multiple_listeners_preserve_per_address_expect_proxy() {
        let toml_content = r#"
            command_socket = "/tmp/sozu_test.sock"
            worker_count = 1

            [[listeners]]
            protocol = "http"
            address = "172.16.20.1:80"
            expect_proxy = true

            [[listeners]]
            protocol = "http"
            address = "10.22.0.1:80"
            expect_proxy = false

            [[listeners]]
            protocol = "https"
            address = "192.168.1.1:443"
            expect_proxy = true

            [[listeners]]
            protocol = "https"
            address = "192.168.2.1:443"
            expect_proxy = false
        "#;

        let file_config: FileConfig =
            toml::from_str(toml_content).expect("Could not parse TOML config");

        let listeners = file_config.listeners.as_ref().expect("No listeners found");
        assert_eq!(listeners.len(), 4);

        let config = ConfigBuilder::new(file_config, "/tmp/test_config.toml")
            .into_config()
            .expect("Could not build config");

        assert_eq!(config.http_listeners.len(), 2);
        assert_eq!(config.https_listeners.len(), 2);

        // HTTP listeners
        let http_proxy = config
            .http_listeners
            .iter()
            .find(|l| SocketAddr::from(l.address) == "172.16.20.1:80".parse().unwrap())
            .expect("Listener on 172.16.20.1:80 not found");
        let http_direct = config
            .http_listeners
            .iter()
            .find(|l| SocketAddr::from(l.address) == "10.22.0.1:80".parse().unwrap())
            .expect("Listener on 10.22.0.1:80 not found");

        assert!(http_proxy.expect_proxy);
        assert!(!http_direct.expect_proxy);

        // HTTPS listeners
        let https_proxy = config
            .https_listeners
            .iter()
            .find(|l| SocketAddr::from(l.address) == "192.168.1.1:443".parse().unwrap())
            .expect("Listener on 192.168.1.1:443 not found");
        let https_direct = config
            .https_listeners
            .iter()
            .find(|l| SocketAddr::from(l.address) == "192.168.2.1:443".parse().unwrap())
            .expect("Listener on 192.168.2.1:443 not found");

        assert!(https_proxy.expect_proxy);
        assert!(!https_direct.expect_proxy);
    }

    #[test]
    fn multiple_listeners_generate_correct_worker_requests() {
        let toml_content = r#"
            command_socket = "/tmp/sozu_test.sock"
            worker_count = 1
            activate_listeners = true

            [[listeners]]
            protocol = "http"
            address = "172.16.20.1:80"
            expect_proxy = true

            [[listeners]]
            protocol = "http"
            address = "10.22.0.1:80"
            expect_proxy = false
        "#;

        let file_config: FileConfig =
            toml::from_str(toml_content).expect("Could not parse TOML config");

        let config = ConfigBuilder::new(file_config, "/tmp/test_config.toml")
            .into_config()
            .expect("Could not build config");

        let messages = config
            .generate_config_messages()
            .expect("Could not generate config messages");

        let add_listener_count = messages
            .iter()
            .filter(|m| {
                matches!(
                    m.content.request_type,
                    Some(RequestType::AddHttpListener(_))
                )
            })
            .count();

        let activate_listener_count = messages
            .iter()
            .filter(|m| {
                matches!(
                    m.content.request_type,
                    Some(RequestType::ActivateListener(ActivateListener {
                        proxy,
                        ..
                    })) if proxy == ListenerType::Http as i32
                )
            })
            .count();

        assert_eq!(add_listener_count, 2);
        assert_eq!(activate_listener_count, 2);
    }

    #[test]
    fn duplicate_listener_address_rejected() {
        let toml_content = r#"
            command_socket = "/tmp/sozu_test.sock"
            worker_count = 1

            [[listeners]]
            protocol = "http"
            address = "0.0.0.0:80"

            [[listeners]]
            protocol = "http"
            address = "0.0.0.0:80"
        "#;

        let file_config: FileConfig =
            toml::from_str(toml_content).expect("Could not parse TOML config");

        let result = ConfigBuilder::new(file_config, "/tmp/test_config.toml").into_config();

        assert!(
            result.is_err(),
            "Should reject duplicate listener addresses"
        );
    }

    #[test]
    fn buffer_size_below_h2_minimum_rejected() {
        // Default ALPN ["h2", "http/1.1"] + buffer_size = 8192 must error.
        let toml_content = r#"
            command_socket = "/tmp/sozu_test.sock"
            worker_count = 1
            buffer_size = 8192

            [[listeners]]
            protocol = "https"
            address = "127.0.0.1:8443"
        "#;
        let file_config: FileConfig =
            toml::from_str(toml_content).expect("Could not parse TOML config");
        let result = ConfigBuilder::new(file_config, "/tmp/test_config.toml").into_config();
        match result {
            Err(ConfigError::BufferSizeTooSmallForH2 {
                buffer_size: 8192,
                minimum: 16_393,
                listeners: 1,
            }) => {}
            other => panic!("expected BufferSizeTooSmallForH2, got {other:?}"),
        }
    }

    #[test]
    fn buffer_size_below_h2_minimum_accepted_when_no_h2_listener() {
        // Drop "h2" from ALPN — buffer_size = 8192 is now valid.
        let toml_content = r#"
            command_socket = "/tmp/sozu_test.sock"
            worker_count = 1
            buffer_size = 8192

            [[listeners]]
            protocol = "https"
            address = "127.0.0.1:8443"
            alpn_protocols = ["http/1.1"]
        "#;
        let file_config: FileConfig =
            toml::from_str(toml_content).expect("Could not parse TOML config");
        let result = ConfigBuilder::new(file_config, "/tmp/test_config.toml").into_config();
        assert!(
            result.is_ok(),
            "non-H2 HTTPS listener with sub-16393 buffer should be accepted: {result:?}"
        );
    }

    #[test]
    fn buffer_size_at_h2_minimum_accepted() {
        let toml_content = r#"
            command_socket = "/tmp/sozu_test.sock"
            worker_count = 1
            buffer_size = 16393

            [[listeners]]
            protocol = "https"
            address = "127.0.0.1:8443"
        "#;
        let file_config: FileConfig =
            toml::from_str(toml_content).expect("Could not parse TOML config");
        let result = ConfigBuilder::new(file_config, "/tmp/test_config.toml").into_config();
        assert!(
            result.is_ok(),
            "buffer_size at the H2 minimum should be accepted: {result:?}"
        );
    }

    #[test]
    fn alpn_protocols_default() {
        let mut builder = ListenerBuilder::new_https(SocketAddress::new_v4(127, 0, 0, 1, 8443));
        let config = builder.to_tls(None).expect("to_tls should succeed");
        assert_eq!(config.alpn_protocols, vec!["h2", "http/1.1"]);
    }

    #[test]
    fn alpn_protocols_custom() {
        let mut builder = ListenerBuilder::new_https(SocketAddress::new_v4(127, 0, 0, 1, 8443));
        builder.with_alpn_protocols(Some(vec!["http/1.1".to_owned()]));
        let config = builder.to_tls(None).expect("to_tls should succeed");
        assert_eq!(config.alpn_protocols, vec!["http/1.1"]);
    }

    #[test]
    fn alpn_protocols_invalid_rejected() {
        let mut builder = ListenerBuilder::new_https(SocketAddress::new_v4(127, 0, 0, 1, 8443));
        builder.with_alpn_protocols(Some(vec!["h3".to_owned()]));
        let result = builder.to_tls(None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("h3"),
            "error should mention the invalid protocol: {err}"
        );
    }

    #[test]
    fn alpn_protocols_empty_uses_default() {
        let mut builder = ListenerBuilder::new_https(SocketAddress::new_v4(127, 0, 0, 1, 8443));
        builder.with_alpn_protocols(Some(vec![]));
        let config = builder.to_tls(None).expect("to_tls should succeed");
        assert_eq!(config.alpn_protocols, vec!["h2", "http/1.1"]);
    }

    #[test]
    fn alpn_protocols_deduplicated() {
        let mut builder = ListenerBuilder::new_https(SocketAddress::new_v4(127, 0, 0, 1, 8443));
        builder.with_alpn_protocols(Some(vec![
            "h2".to_owned(),
            "h2".to_owned(),
            "http/1.1".to_owned(),
        ]));
        let config = builder.to_tls(None).expect("to_tls should succeed");
        assert_eq!(config.alpn_protocols, vec!["h2", "http/1.1"]);
    }

    #[test]
    fn alpn_protocols_order_preserved() {
        let mut builder = ListenerBuilder::new_https(SocketAddress::new_v4(127, 0, 0, 1, 8443));
        builder.with_alpn_protocols(Some(vec!["http/1.1".to_owned(), "h2".to_owned()]));
        let config = builder.to_tls(None).expect("to_tls should succeed");
        assert_eq!(config.alpn_protocols, vec!["http/1.1", "h2"]);
    }

    /// CRLF or NUL in a `[[clusters.<id>.frontends.headers]]` value
    /// would let an operator-supplied config splice arbitrary
    /// header / request lines into the H1 wire on the backend side
    /// (CWE-113). The H2 emission path filters at runtime; we reject
    /// at config-load time as a defense in depth.
    #[test]
    fn parse_header_edit_rejects_crlf_in_value() {
        let entry = HeaderEditConfig {
            position: "request".to_owned(),
            key: "X-Test".to_owned(),
            value: "value\r\nEvil-Header: stolen".to_owned(),
        };
        let err = parse_header_edit(0, &entry).expect_err("CRLF in value must be rejected");
        match err {
            ConfigError::InvalidHeaderBytes { index, field } => {
                assert_eq!(index, 0);
                assert_eq!(field, "value");
            }
            other => panic!("expected InvalidHeaderBytes, got {other:?}"),
        }
    }

    #[test]
    fn parse_header_edit_rejects_lf_in_key() {
        let entry = HeaderEditConfig {
            position: "response".to_owned(),
            key: "X-\nTest".to_owned(),
            value: "ok".to_owned(),
        };
        let err = parse_header_edit(2, &entry).expect_err("LF in key must be rejected");
        match err {
            ConfigError::InvalidHeaderBytes { index, field } => {
                assert_eq!(index, 2);
                assert_eq!(field, "key");
            }
            other => panic!("expected InvalidHeaderBytes, got {other:?}"),
        }
    }

    #[test]
    fn parse_header_edit_rejects_nul() {
        let entry = HeaderEditConfig {
            position: "both".to_owned(),
            key: "X-Test".to_owned(),
            value: "with\0nul".to_owned(),
        };
        assert!(matches!(
            parse_header_edit(0, &entry),
            Err(ConfigError::InvalidHeaderBytes { .. })
        ));
    }

    /// Horizontal tab `\t` (0x09) is permitted in field values per
    /// RFC 9110 §5.5 (folded-header obs-fold parts). The value-side
    /// validator must NOT reject it — otherwise legitimate operator
    /// configs (e.g. `Authorization: Basic\tCREDENTIALS`) become
    /// unusable. The key-side validator IS stricter (token grammar).
    #[test]
    fn parse_header_edit_accepts_tab_in_value() {
        let entry = HeaderEditConfig {
            position: "request".to_owned(),
            key: "X-Test".to_owned(),
            value: "with\ttab".to_owned(),
        };
        let header = parse_header_edit(0, &entry).expect("tab in value must be accepted");
        assert_eq!(header.val, "with\ttab");
    }

    /// Header NAMES follow `token` grammar per RFC 9110 §5.1. HTAB and
    /// SP are NOT tchar; the key-side validator must reject them even
    /// though the value-side validator permits HTAB. Without this,
    /// an operator entry like `key = "Host\t"` would emit `Host\t: …`
    /// on the H1 wire and produce an invalid (but parser-tolerant)
    /// header line that some backends silently accept as `Host:`.
    #[test]
    fn parse_header_edit_rejects_tab_in_key() {
        let entry = HeaderEditConfig {
            position: "request".to_owned(),
            key: "Host\t".to_owned(),
            value: "ok".to_owned(),
        };
        let err = parse_header_edit(0, &entry).expect_err("HTAB in key must be rejected");
        match err {
            ConfigError::InvalidHeaderBytes { field, .. } => assert_eq!(field, "key"),
            other => panic!("expected InvalidHeaderBytes{{field=\"key\"}}, got {other:?}"),
        }
    }

    #[test]
    fn parse_header_edit_rejects_space_in_key() {
        let entry = HeaderEditConfig {
            position: "request".to_owned(),
            key: "X Test".to_owned(),
            value: "ok".to_owned(),
        };
        let err = parse_header_edit(0, &entry).expect_err("SP in key must be rejected");
        assert!(matches!(err, ConfigError::InvalidHeaderBytes { .. }));
    }

    #[test]
    fn parse_header_edit_rejects_empty_key() {
        let entry = HeaderEditConfig {
            position: "request".to_owned(),
            key: String::new(),
            value: "ok".to_owned(),
        };
        let err = parse_header_edit(0, &entry).expect_err("empty key must be rejected");
        assert!(matches!(
            err,
            ConfigError::InvalidHeaderBytes { field: "key", .. }
        ));
    }

    #[test]
    fn parse_header_edit_accepts_clean_value() {
        let entry = HeaderEditConfig {
            position: "request".to_owned(),
            key: "X-Tenant".to_owned(),
            value: "alpha".to_owned(),
        };
        let header = parse_header_edit(0, &entry).expect("clean value must be accepted");
        assert_eq!(header.key, "X-Tenant");
        assert_eq!(header.val, "alpha");
    }

    /// A bare string with no scheme prefix is the inline literal body.
    /// This is the common case — short canned responses inline in TOML
    /// or a `--answer` flag, no disk I/O.
    #[test]
    fn resolve_answer_source_bare_string_is_literal() {
        let body = resolve_answer_source("HTTP/1.1 503 Service Unavailable\r\n\r\nbusy")
            .expect("bare-string source must resolve");
        assert_eq!(body, "HTTP/1.1 503 Service Unavailable\r\n\r\nbusy");
    }

    #[test]
    fn resolve_answer_source_empty_string_is_legitimate() {
        let body = resolve_answer_source("").expect("empty source must resolve");
        assert_eq!(body, "");
    }

    /// `file://` opts into reading the path off disk. A non-existent
    /// path bubbles up as `ConfigError::FileOpen` so the operator gets
    /// the same diagnostics as the existing per-status `answer_NNN`
    /// flow.
    #[test]
    fn resolve_answer_source_file_scheme_missing_file_errors() {
        let err = resolve_answer_source("file:///nonexistent/sozu-test/never.http")
            .expect_err("missing path must error");
        assert!(matches!(err, ConfigError::FileOpen { .. }));
    }

    /// `file://` strips the scheme; an empty path after the scheme is
    /// rejected (empty path on filesystem read).
    #[test]
    fn resolve_answer_source_file_scheme_empty_path_errors() {
        let err = resolve_answer_source("file://").expect_err("empty path must error");
        assert!(matches!(err, ConfigError::FileOpen { .. }));
    }
}
