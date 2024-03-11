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
//! ```ignore
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
//! ```ignore
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
    fs::{create_dir_all, metadata, File},
    io::{ErrorKind, Read},
    net::SocketAddr,
    ops::Range,
    path::PathBuf,
};

use crate::{
    certificate::split_certificate_chain,
    logging::AccessLogFormat,
    proto::command::{
        request::RequestType, ActivateListener, AddBackend, AddCertificate, CertificateAndKey,
        Cluster, HttpListenerConfig, HttpsListenerConfig, ListenerType, LoadBalancingAlgorithms,
        LoadBalancingParams, LoadMetric, MetricsConfiguration, PathRule, ProtobufAccessLogFormat,
        ProxyProtocolConfig, Request, RequestHttpFrontend, RequestTcpFrontend, RulePosition,
        ServerConfig, ServerMetricsConfig, SocketAddress, TcpListenerConfig, TlsVersion,
        WorkerRequest,
    },
    ObjectKind,
};

/// provides all supported cipher suites exported by Rustls TLS
/// provider as it support only strongly secure ones.
///
/// See the [documentation](https://docs.rs/rustls/latest/rustls/static.ALL_CIPHER_SUITES.html)
pub const DEFAULT_RUSTLS_CIPHER_LIST: [&str; 9] = [
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

pub const DEFAULT_CIPHER_SUITES: [&str; 4] = [
    "TLS_AES_256_GCM_SHA384",
    "TLS_AES_128_GCM_SHA256",
    "TLS_AES_128_CCM_SHA256",
    "TLS_CHACHA20_POLY1305_SHA256",
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

pub const DEFAULT_GROUPS_LIST: [&str; 4] = ["P-521", "P-384", "P-256", "x25519"];

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
    #[error("the field {kind:?} of {object:?} with id or address {id} is incompatible with the rest of the options")]
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
}

/// An HTTP, HTTPS or TCP listener as parsed from the `Listeners` section in the toml
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListenerBuilder {
    pub address: SocketAddr,
    pub protocol: Option<ListenerProtocol>,
    pub public_address: Option<SocketAddr>,
    /// path to the 404 html file
    pub answer_404: Option<String>,
    /// path to the 503 html file
    pub answer_503: Option<String>,
    pub tls_versions: Option<Vec<TlsVersion>>,
    pub cipher_list: Option<Vec<String>>,
    pub cipher_suites: Option<Vec<String>>,
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
            protocol: Some(protocol),
            sticky_name: DEFAULT_STICKY_NAME.to_string(),
            public_address: None,
            answer_404: None,
            answer_503: None,
            tls_versions: None,
            cipher_list: None,
            cipher_suites: None,
            expect_proxy: None,
            certificate: None,
            certificate_chain: None,
            key: None,
            front_timeout: None,
            back_timeout: None,
            connect_timeout: None,
            request_timeout: None,
            config: None,
            send_tls13_tickets: None,
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

        let (answer_404, answer_503) = self.get_404_503_answers()?;

        let configuration = HttpListenerConfig {
            address: self.address.into(),
            public_address: self.public_address.map(|a| a.into()),
            expect_proxy: self.expect_proxy.unwrap_or(false),
            sticky_name: self.sticky_name.clone(),
            front_timeout: self.front_timeout.unwrap_or(DEFAULT_FRONT_TIMEOUT),
            back_timeout: self.back_timeout.unwrap_or(DEFAULT_BACK_TIMEOUT),
            connect_timeout: self.connect_timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT),
            request_timeout: self.request_timeout.unwrap_or(DEFAULT_REQUEST_TIMEOUT),
            answer_404,
            answer_503,
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

        let default_cipher_list = DEFAULT_RUSTLS_CIPHER_LIST
            .into_iter()
            .map(String::from)
            .collect();

        let cipher_list = self.cipher_list.clone().unwrap_or(default_cipher_list);

        let default_cipher_suites = DEFAULT_CIPHER_SUITES
            .into_iter()
            .map(String::from)
            .collect();

        let cipher_suites = self.cipher_suites.clone().unwrap_or(default_cipher_suites);

        let signature_algorithms: Vec<String> = DEFAULT_SIGNATURE_ALGORITHMS
            .into_iter()
            .map(String::from)
            .collect();

        let groups_list: Vec<String> = DEFAULT_GROUPS_LIST.into_iter().map(String::from).collect();

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

        let (answer_404, answer_503) = self.get_404_503_answers()?;

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
            answer_404,
            answer_503,
            cipher_suites,
            signature_algorithms,
            groups_list,
            active: false,
            send_tls13_tickets: self
                .send_tls13_tickets
                .unwrap_or(DEFAULT_SEND_TLS_13_TICKETS),
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

    /// Get the 404 and 503 answers from the file system using the provided paths,
    /// if none, defaults to HTML files in the sozu assets
    fn get_404_503_answers(&self) -> Result<(String, String), ConfigError> {
        let answer_404 = match &self.answer_404 {
            Some(a_404_path) => open_and_read_file(a_404_path)?,
            None => String::from(include_str!("../assets/404.html")),
        };

        let answer_503 = match &self.answer_503 {
            Some(a_503_path) => open_and_read_file(a_503_path)?,
            None => String::from(include_str!("../assets/503.html")),
        };
        Ok((answer_404, answer_503))
    }
}

fn open_and_read_file(path: &str) -> Result<String, ConfigError> {
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

    Ok(content)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsConfig {
    pub address: SocketAddr,
    #[serde(default)]
    pub tagged_metrics: bool,
    #[serde(default)]
    pub prefix: Option<String>,
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
                )))
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
        })
    }
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
                                })
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
                                })
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

                Ok(ClusterConfig::Tcp(TcpClusterConfig {
                    cluster_id: cluster_id.to_string(),
                    frontends,
                    backends: self.backends,
                    proxy_protocol,
                    load_balancing: self.load_balancing,
                    load_metric: self.load_metric,
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

                Ok(ClusterConfig::Http(HttpClusterConfig {
                    cluster_id: cluster_id.to_string(),
                    frontends,
                    backends: self.backends,
                    sticky_session: self.sticky_session.unwrap_or(false),
                    https_redirect: self.https_redirect.unwrap_or(false),
                    load_balancing: self.load_balancing,
                    load_metric: self.load_metric,
                    answer_503,
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
}

impl HttpFrontendConfig {
    pub fn generate_requests(&self, cluster_id: &str) -> Vec<Request> {
        let mut v = Vec::new();

        let tags = match self.tags.clone() {
            Some(tags) => tags,
            None => BTreeMap::new(),
        };

        if self.key.is_some() && self.certificate.is_some() {
            v.push(
                RequestType::AddCertificate(AddCertificate {
                    address: self.address.into(),
                    certificate: CertificateAndKey {
                        key: self.key.clone().unwrap(),
                        certificate: self.certificate.clone().unwrap(),
                        certificate_chain: self.certificate_chain.clone().unwrap_or_default(),
                        versions: self.tls_versions.iter().map(|v| *v as i32).collect(),
                        names: vec![self.hostname.clone()],
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
}

impl HttpClusterConfig {
    pub fn generate_requests(&self) -> Result<Vec<Request>, ConfigError> {
        let mut v = vec![RequestType::AddCluster(Cluster {
            cluster_id: self.cluster_id.clone(),
            sticky_session: self.sticky_session,
            https_redirect: self.https_redirect,
            proxy_protocol: None,
            load_balancing: self.load_balancing as i32,
            answer_503: self.answer_503.clone(),
            load_metric: self.load_metric.map(|s| s as i32),
        })
        .into()];

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
}

impl TcpClusterConfig {
    pub fn generate_requests(&self) -> Result<Vec<Request>, ConfigError> {
        let mut v = vec![RequestType::AddCluster(Cluster {
            cluster_id: self.cluster_id.clone(),
            sticky_session: false,
            https_redirect: false,
            proxy_protocol: self.proxy_protocol.map(|s| s as i32),
            load_balancing: self.load_balancing as i32,
            load_metric: self.load_metric.map(|s| s as i32),
            answer_503: None,
        })
        .into()];

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
    pub saved_state: Option<String>,
    #[serde(default)]
    pub automatic_state_save: Option<bool>,
    pub log_level: Option<String>,
    pub log_target: Option<String>,
    #[serde(default)]
    pub log_colored: bool,
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
                                        frontend.certificate = https_listener.certificate.clone();
                                        frontend.certificate_chain =
                                            Some(https_listener.certificate_chain.clone());
                                        frontend.key = https_listener.key.clone();
                                    }
                                    if frontend.certificate.is_none() {
                                        debug!("known addresses: {:#?}", self.known_addresses);
                                        debug!("frontend: {:#?}", frontend);
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
    #[serde(default = "default_request_timeout")]
    pub request_timeout: u32,
    #[serde(default = "default_worker_timeout")]
    pub worker_timeout: u32,
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

fn default_disable_cluster_metrics() -> bool {
    DEFAULT_DISABLE_CLUSTER_METRICS
}

fn default_worker_timeout() -> u32 {
    DEFAULT_WORKER_TIMEOUT
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
                content: RequestType::AddTcpListener(listener.clone()).into(),
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
                        address: listener.address.clone(),
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
                        address: listener.address.clone(),
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
                        address: listener.address.clone(),
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
    /// size of the slab for the Session manager
    pub fn slab_capacity(&self) -> u64 {
        10 + 2 * self.max_connections
    }
}

/// reduce the config to the bare minimum needed by a worker
impl From<&Config> for ServerConfig {
    fn from(config: &Config) -> Self {
        let metrics = config.metrics.clone().map(|m| ServerMetricsConfig {
            address: m.address.to_string(),
            tagged_metrics: m.tagged_metrics,
            prefix: m.prefix,
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
            command_buffer_size: config.command_buffer_size,
            max_command_buffer_size: config.max_command_buffer_size,
            metrics,
            access_log_format: ProtobufAccessLogFormat::from(&config.access_logs_format) as i32,
            log_colored: config.log_colored,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use toml::to_string;

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
}
