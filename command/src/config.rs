//! parsing data from the configuration file
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env,
    fs::File,
    io::{self, Read},
    net::SocketAddr,
    ops::Range,
    path::PathBuf,
};

use anyhow::{bail, Context};
use toml;

use crate::{
    certificate::{split_certificate_chain, CertificateAndKey, TlsVersion},
    request::{
        ActivateListener, AddBackend, AddCertificate, Cluster, ListenerType,
        LoadBalancingAlgorithms, LoadBalancingParams, LoadMetric, Request, RequestHttpFrontend,
        RequestTcpFrontend, WorkerRequest,
    },
    response::{
        HttpListenerConfig, HttpsListenerConfig, PathRule, RulePosition, TcpListenerConfig,
    },
};

/// [`DEFAULT_RUSTLS_CIPHER_LIST`] provides all supported cipher suites exported by Rustls TLS
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

// TODO: trickle default values from the config, see #873
pub const DEFAULT_FRONT_TIMEOUT: u32 = 60;
pub const DEFAULT_BACK_TIMEOUT: u32 = 30;
pub const DEFAULT_CONNECT_TIMEOUT: u32 = 3;
pub const DEFAULT_REQUEST_TIMEOUT: u32 = 10;

pub const DEFAULT_STICKY_NAME: &str = "SOZUBALANCEID";

/// An HTTP, HTTPS or TCP listener as parsed from the config
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListenerBuilder {
    pub address: String,
    pub protocol: Option<ListenerProtocol>,
    pub public_address: Option<String>,
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
}

pub fn default_sticky_name() -> String {
    DEFAULT_STICKY_NAME.to_string()
}

impl ListenerBuilder {
    /// starts building an HTTP Listener with default values
    pub fn new_http<S>(address: S) -> ListenerBuilder
    where
        S: ToString,
    {
        Self::new(address, ListenerProtocol::Http)
    }

    /// starts building an HTTPS Listener with default values
    pub fn new_tcp<S>(address: S) -> ListenerBuilder
    where
        S: ToString,
    {
        Self::new(address, ListenerProtocol::Tcp)
    }

    /// starts building a TCP Listener with default values
    pub fn new_https<S>(address: S) -> ListenerBuilder
    where
        S: ToString,
    {
        Self::new(address, ListenerProtocol::Https)
    }

    /// starts building a Listener with default values
    fn new<S>(address: S, protocol: ListenerProtocol) -> ListenerBuilder
    where
        S: ToString,
    {
        ListenerBuilder {
            address: address.to_string(),
            protocol: Some(protocol),
            sticky_name: DEFAULT_STICKY_NAME.to_string(),
            front_timeout: Some(DEFAULT_FRONT_TIMEOUT),
            back_timeout: Some(DEFAULT_BACK_TIMEOUT),
            connect_timeout: Some(DEFAULT_CONNECT_TIMEOUT),
            request_timeout: Some(DEFAULT_REQUEST_TIMEOUT),
            ..Default::default()
        }
    }

    pub fn with_public_address<S>(&mut self, public_address: Option<S>) -> &mut Self
    where
        S: ToString,
    {
        if let Some(address) = public_address {
            self.public_address = Some(address.to_string());
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

    pub fn parse_address(&self) -> anyhow::Result<SocketAddr> {
        self.address.parse().with_context(|| "wrong socket address")
    }

    pub fn parse_public_address(&self) -> anyhow::Result<Option<SocketAddr>> {
        match &self.public_address {
            Some(a) => {
                let parsed = a
                    .parse::<SocketAddr>()
                    .with_context(|| "wrong socket address")?;
                Ok(Some(parsed))
            }
            None => Ok(None),
        }
    }

    /// build an HTTP listener using defaults if the values were not provided
    pub fn to_http(&mut self) -> anyhow::Result<HttpListenerConfig> {
        if self.protocol != Some(ListenerProtocol::Http) {
            bail!(format!(
                "Can not build an HTTP listener from a {:?} config",
                self.protocol
            ));
        }

        let (answer_404, answer_503) = self
            .get_404_503_answers()
            .with_context(|| "Could not get 404 and 503 answers from file system")?;

        let _address = self
            .parse_address()
            .with_context(|| "wrong socket address")?;

        let _public_address = self
            .parse_public_address()
            .with_context(|| "wrong public address")?;

        let configuration = HttpListenerConfig {
            address: self.address.clone(),
            public_address: self.public_address.clone(),
            expect_proxy: self.expect_proxy.unwrap_or(false),
            sticky_name: self.sticky_name.clone(),
            front_timeout: self.front_timeout.unwrap_or(DEFAULT_FRONT_TIMEOUT),
            back_timeout: self.back_timeout.unwrap_or(DEFAULT_BACK_TIMEOUT),
            connect_timeout: self.connect_timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT),
            request_timeout: self.request_timeout.unwrap_or(DEFAULT_REQUEST_TIMEOUT),
            answer_404,
            answer_503,
        };

        Ok(configuration)
    }

    /// build an HTTPS listener using defaults if the values were not provided
    pub fn to_tls(&self) -> anyhow::Result<HttpsListenerConfig> {
        if self.protocol != Some(ListenerProtocol::Https) {
            bail!(format!(
                "Can not build an HTTPS listener from a {:?} config",
                self.protocol
            ));
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
            None => vec![TlsVersion::TLSv1_2, TlsVersion::TLSv1_3],
            Some(ref v) => v.clone(),
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
            .unwrap_or_else(Vec::new);

        let (answer_404, answer_503) = self
            .get_404_503_answers()
            .with_context(|| "Could not get 404 and 503 answers from file system")?;

        let _address = self
            .parse_address()
            .with_context(|| "wrong socket address")?;

        let _public_address = self
            .parse_public_address()
            .with_context(|| "wrong public address")?;

        let https_listener_config = HttpsListenerConfig {
            address: self.address.clone(),
            sticky_name: self.sticky_name.clone(),
            public_address: self.public_address.clone(),
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
        };

        Ok(https_listener_config)
    }

    /// build a TCP listener using defaults if the values were not provided
    pub fn to_tcp(&self) -> anyhow::Result<TcpListenerConfig> {
        if self.protocol != Some(ListenerProtocol::Tcp) {
            bail!(format!(
                "Can not build a TCP listener from a {:?} config",
                self.protocol
            ));
        }

        let _address = self
            .parse_address()
            .with_context(|| "wrong socket address")?;

        let _public_address = self
            .parse_public_address()
            .with_context(|| "wrong public address")?;

        Ok(TcpListenerConfig {
            address: self.address.clone(),
            public_address: self.public_address.clone(),
            expect_proxy: self.expect_proxy.unwrap_or(false),
            front_timeout: self.front_timeout.unwrap_or(DEFAULT_FRONT_TIMEOUT),
            back_timeout: self.back_timeout.unwrap_or(DEFAULT_BACK_TIMEOUT),
            connect_timeout: self.connect_timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT),
            active: false,
        })
    }

    /// Get the 404 and 503 answers from the file system using the provided paths,
    /// if none, defaults to HTML files in the sozu assets
    fn get_404_503_answers(&self) -> anyhow::Result<(String, String)> {
        let answer_404 = match &self.answer_404 {
            Some(a_404_path) => {
                let mut a_404 = String::new();
                let mut file = File::open(a_404_path).with_context(|| {
                    format!(
                        "Could not open 404 answer file on path {}, current dir {:?}",
                        a_404_path,
                        std::env::current_dir().ok()
                    )
                })?;

                file.read_to_string(&mut a_404).with_context(|| {
                    format!(
                        "Could not read 404 answer file on path {}, current dir {:?}",
                        a_404_path,
                        std::env::current_dir().ok()
                    )
                })?;
                a_404
            }
            None => String::from(include_str!("../assets/404.html")),
        };

        let answer_503 = match &self.answer_503 {
            Some(a_503_path) => {
                let mut a_503 = String::new();
                let mut file = File::open(a_503_path).with_context(|| {
                    format!("Could not open 503 answer file on path {a_503_path}")
                })?;

                file.read_to_string(&mut a_503).with_context(|| {
                    format!("Could not read 503 answer file on path {a_503_path}")
                })?;
                a_503
            }
            None => String::from(include_str!("../assets/503.html")),
        };
        Ok((answer_404, answer_503))
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
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[serde(deny_unknown_fields)]
pub enum ProxyProtocolConfig {
    ExpectHeader,
    SendHeader,
    RelayHeader,
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
    pub fn to_tcp_front(&self) -> anyhow::Result<TcpFrontendConfig> {
        if self.hostname.is_some() {
            bail!("invalid 'hostname' field for TCP frontend");
        }
        if self.path.is_some() {
            bail!("invalid 'path_prefix' field for TCP frontend");
        }
        if self.certificate.is_some() {
            bail!("invalid 'certificate' field for TCP frontend");
        }
        if self.hostname.is_some() {
            bail!("invalid 'key' field for TCP frontend");
        }
        if self.certificate_chain.is_some() {
            bail!("invalid 'certificate_chain' field for TCP frontend",);
        }

        Ok(TcpFrontendConfig {
            address: self.address,
            tags: self.tags.clone(),
        })
    }

    pub fn to_http_front(&self, _cluster_id: &str) -> anyhow::Result<HttpFrontendConfig> {
        let hostname = match &self.hostname {
            Some(hostname) => hostname.to_owned(),
            None => bail!("HTTP frontend should have a 'hostname' field"),
        };

        let key_opt = match self.key.as_ref() {
            None => None,
            Some(path) => {
                let key = Config::load_file(path)
                    .with_context(|| format!("cannot load key at path '{path}'"))?;
                Some(key)
            }
        };

        let certificate_opt = match self.certificate.as_ref() {
            None => None,
            Some(path) => {
                let certificate = Config::load_file(path)
                    .with_context(|| format!("cannot load certificate at path '{path}'"))?;
                Some(certificate)
            }
        };

        let chain_opt = match self.certificate_chain.as_ref() {
            None => None,
            Some(path) => {
                let certificate_chain = Config::load_file(path)
                    .with_context(|| format!("cannot load certificate chain at path {path}"))?;
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
            certificate_chain: chain_opt,
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
    ) -> anyhow::Result<ClusterConfig> {
        match self.protocol {
            FileClusterProtocolConfig::Tcp => {
                let mut has_expect_proxy = None;
                let mut frontends = Vec::new();
                for f in self.frontends {
                    if expect_proxy.contains(&f.address) {
                        match has_expect_proxy {
                            Some(true) => {},
                            Some(false) => bail!(format!(
                                "all the listeners for cluster {cluster_id} should have the same expect_proxy option"
                            )),
                            None => has_expect_proxy = Some(true),
                        }
                    } else {
                        match has_expect_proxy {
                            Some(false) => {},
                            Some(true) => bail!(format!(
                                "all the listeners for cluster {cluster_id} should have the same expect_proxy option"
                            )),
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
                    let http_frontend = frontend
                        .to_http_front(cluster_id)
                        .with_context(|| "Could not convert frontend config to http frontend")?;
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

        if self.key.is_some() && self.certificate.is_some() {
            v.push(Request::AddCertificate(AddCertificate {
                address: self.address.to_string(),
                certificate: CertificateAndKey {
                    key: self.key.clone().unwrap(),
                    certificate: self.certificate.clone().unwrap(),
                    certificate_chain: self.certificate_chain.clone().unwrap_or_default(),
                    versions: self.tls_versions.clone(),
                },
                names: vec![self.hostname.clone()],
                expired_at: None,
            }));

            v.push(Request::AddHttpsFrontend(RequestHttpFrontend {
                cluster_id: Some(cluster_id.to_string()),
                address: self.address.to_string(),
                hostname: self.hostname.clone(),
                path: self.path.clone(),
                method: self.method.clone(),
                position: self.position,
                tags: self.tags.clone(),
            }));
        } else {
            //create the front both for HTTP and HTTPS if possible
            v.push(Request::AddHttpFrontend(RequestHttpFrontend {
                cluster_id: Some(cluster_id.to_string()),
                address: self.address.to_string(),
                hostname: self.hostname.clone(),
                path: self.path.clone(),
                method: self.method.clone(),
                position: self.position,
                tags: self.tags.clone(),
            }));
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
    pub fn generate_requests(&self) -> anyhow::Result<Vec<Request>> {
        let mut v = vec![Request::AddCluster(Cluster {
            cluster_id: self.cluster_id.clone(),
            sticky_session: self.sticky_session,
            https_redirect: self.https_redirect,
            proxy_protocol: None,
            load_balancing: self.load_balancing,
            answer_503: self.answer_503.clone(),
            load_metric: self.load_metric,
        })];

        for frontend in &self.frontends {
            let mut orders = frontend.generate_requests(&self.cluster_id);
            v.append(&mut orders);
        }

        for (backend_count, backend) in self.backends.iter().enumerate() {
            let load_balancing_parameters = Some(LoadBalancingParams {
                weight: backend.weight.unwrap_or(100),
            });

            v.push(Request::AddBackend(AddBackend {
                cluster_id: self.cluster_id.clone(),
                backend_id: backend.backend_id.clone().unwrap_or_else(|| {
                    format!("{}-{}-{}", self.cluster_id, backend_count, backend.address)
                }),
                address: backend.address.to_string(),
                load_balancing_parameters,
                sticky_id: backend.sticky_id.clone(),
                backup: backend.backup,
            }));
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
    pub fn generate_requests(&self) -> anyhow::Result<Vec<Request>> {
        let mut v = vec![Request::AddCluster(Cluster {
            cluster_id: self.cluster_id.clone(),
            sticky_session: false,
            https_redirect: false,
            proxy_protocol: self.proxy_protocol.clone(),
            load_balancing: self.load_balancing,
            load_metric: self.load_metric,
            answer_503: None,
        })];

        for frontend in &self.frontends {
            v.push(Request::AddTcpFrontend(RequestTcpFrontend {
                cluster_id: self.cluster_id.clone(),
                address: frontend.address.to_string(),
                tags: frontend.tags.clone(),
            }));
        }

        for (backend_count, backend) in self.backends.iter().enumerate() {
            let load_balancing_parameters = Some(LoadBalancingParams {
                weight: backend.weight.unwrap_or(100),
            });

            v.push(Request::AddBackend(AddBackend {
                cluster_id: self.cluster_id.clone(),
                backend_id: backend.backend_id.clone().unwrap_or_else(|| {
                    format!("{}-{}-{}", self.cluster_id, backend_count, backend.address)
                }),
                address: backend.address.to_string(),
                load_balancing_parameters,
                sticky_id: backend.sticky_id.clone(),
                backup: backend.backup,
            }));
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
    pub fn generate_requests(&self) -> anyhow::Result<Vec<Request>> {
        match *self {
            ClusterConfig::Http(ref http) => http.generate_requests(),
            ClusterConfig::Tcp(ref tcp) => tcp.generate_requests(),
        }
    }
}

/// Built from the TOML config provided by the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default, Deserialize)]
pub struct FileConfig {
    pub command_socket: Option<String>,
    pub command_buffer_size: Option<usize>,
    pub max_command_buffer_size: Option<usize>,
    pub max_connections: Option<usize>,
    pub min_buffers: Option<usize>,
    pub max_buffers: Option<usize>,
    pub buffer_size: Option<usize>,
    pub saved_state: Option<String>,
    #[serde(default)]
    pub automatic_state_save: Option<bool>,
    pub log_level: Option<String>,
    pub log_target: Option<String>,
    #[serde(default)]
    pub log_access_target: Option<String>,
    pub worker_count: Option<u16>,
    pub worker_automatic_restart: Option<bool>,
    pub metrics: Option<MetricsConfig>,
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
}

impl FileConfig {
    pub fn load_from_path(path: &str) -> anyhow::Result<FileConfig> {
        let data = Config::load_file(path)?;

        let config: FileConfig = match toml::from_str(&data) {
            Ok(config) => config,
            Err(e) => {
                display_toml_error(&data, &e);
                bail!(format!("toml decoding error: {e}"));
            }
        };

        let mut reserved_address: HashSet<SocketAddr> = HashSet::new();

        if let Some(listeners) = config.listeners.as_ref() {
            for listener in listeners.iter() {
                if reserved_address.contains(&listener.parse_address()?) {
                    bail!(format!(
                        "listening address {:?} is already used in the configuration",
                        listener.address
                    ));
                }
                reserved_address.insert(listener.parse_address()?);
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

pub struct ConfigBuilder {
    file: FileConfig,
    known_addresses: HashMap<SocketAddr, ListenerProtocol>,
    expect_proxy_addresses: HashSet<SocketAddr>,
    built: Config,
}

impl ConfigBuilder {
    pub fn new(file_config: FileConfig) -> Self {
        Self {
            file: file_config,
            known_addresses: HashMap::new(),
            expect_proxy_addresses: HashSet::new(),
            built: Config::default(),
        }
    }

    fn push_tls_listener(&mut self, listener: ListenerBuilder) -> anyhow::Result<()> {
        let listener = listener
            .to_tls()
            .with_context(|| "Cannot convert listener to TLS")?;
        self.built.https_listeners.push(listener);
        Ok(())
    }

    fn push_http_listener(&mut self, mut listener: ListenerBuilder) -> anyhow::Result<()> {
        let listener = listener
            .to_http()
            .with_context(|| "Cannot convert listener to HTTP")?;
        self.built.http_listeners.push(listener);
        Ok(())
    }

    fn push_tcp_listener(&mut self, listener: ListenerBuilder) -> anyhow::Result<()> {
        let listener = listener
            .to_tcp()
            .with_context(|| "Cannot convert listener to TCP")?;
        self.built.tcp_listeners.push(listener);
        Ok(())
    }

    fn populate_listeners(&mut self, listeners: Vec<ListenerBuilder>) -> anyhow::Result<()> {
        for listener in listeners.iter() {
            let address = listener.parse_address()?;
            if self.known_addresses.contains_key(&address) {
                bail!(format!(
                    "there's already a listener for address {:?}",
                    listener.address
                ));
            }

            let protocol = listener
                .protocol
                .with_context(|| "No protocol defined for this listener")?;

            self.known_addresses.insert(address, protocol);
            if listener.expect_proxy == Some(true) {
                self.expect_proxy_addresses.insert(address);
            }

            if listener.public_address.is_some() && listener.expect_proxy == Some(true) {
                bail!(format!(
                        "the listener on {} has incompatible options: it cannot use the expect proxy protocol and have a public_address field at the same time",
                        &listener.address
                    ));
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
    ) -> anyhow::Result<()> {
        for (id, file_cluster_config) in file_cluster_configs.drain() {
            let mut cluster_config = file_cluster_config
                .to_cluster_config(id.as_str(), &self.expect_proxy_addresses)
                .with_context(|| format!("error parsing cluster configuration for cluster {id}"))?;

            match cluster_config {
                ClusterConfig::Http(ref mut http) => {
                    for frontend in http.frontends.iter_mut() {
                        match self.known_addresses.get(&frontend.address) {
                            Some(ListenerProtocol::Tcp) => {
                                bail!("cannot set up a HTTP or HTTPS frontend on a TCP listener");
                            }
                            Some(ListenerProtocol::Http) => {
                                if frontend.certificate.is_some() {
                                    bail!("cannot set up a HTTPS frontend on a HTTP listener");
                                }
                            }
                            Some(ListenerProtocol::Https) => {
                                if frontend.certificate.is_none() {
                                    if let Some(https_listener) =
                                        self.built.https_listeners.iter().find(|listener| {
                                            listener.address == frontend.address.to_string()
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
                                        println!("known addresses: {:#?}", self.known_addresses);
                                        println!("frontend: {frontend:#?}");
                                        bail!("cannot set up a HTTP frontend on a HTTPS listener");
                                    }
                                }
                            }
                            None => {
                                // create a default listener for that front
                                let file_listener_protocol = if frontend.certificate.is_some() {
                                    self.push_tls_listener(ListenerBuilder::new(
                                        frontend.address.to_string(),
                                        ListenerProtocol::Https,
                                    ))?;

                                    ListenerProtocol::Https
                                } else {
                                    self.push_http_listener(ListenerBuilder::new(
                                        frontend.address.to_string(),
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
                                bail!("cannot set up a TCP frontend on a HTTP listener");
                            }
                            Some(ListenerProtocol::Tcp) => {}
                            None => {
                                // create a default listener for that front
                                self.push_tcp_listener(ListenerBuilder::new(
                                    frontend.address.to_string(),
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

    pub fn into_config(&mut self, config_path: &str) -> anyhow::Result<Config> {
        if let Some(listeners) = &self.file.listeners {
            self.populate_listeners(listeners.clone())?;
        }

        if let Some(file_cluster_configs) = &self.file.clusters {
            self.populate_clusters(file_cluster_configs.clone())?;
        }

        let command_socket_path = self.file.command_socket.clone().unwrap_or({
            let mut path = env::current_dir().with_context(|| "env path not found")?;
            path.push("sozu.sock");
            let verified_path = path
                .to_str()
                .with_context(|| "command socket path not valid")?;
            verified_path.to_owned()
        });

        if let (None, Some(true)) = (&self.file.saved_state, &self.file.automatic_state_save) {
            bail!("cannot activate automatic state save if the 'saved_state` option is not set");
        }

        Ok(Config {
            config_path: config_path.to_string(),
            command_socket: command_socket_path,
            command_buffer_size: self.file.command_buffer_size.unwrap_or(1_000_000),
            max_command_buffer_size: self
                .file
                .max_command_buffer_size
                .unwrap_or(self.file.command_buffer_size.unwrap_or(1_000_000) * 2),
            max_connections: self.file.max_connections.unwrap_or(10000),
            min_buffers: std::cmp::min(
                self.file.min_buffers.unwrap_or(1),
                self.file.max_buffers.unwrap_or(1000),
            ),
            max_buffers: self.file.max_buffers.unwrap_or(1000),
            buffer_size: self.file.buffer_size.unwrap_or(16393),
            saved_state: self.file.saved_state.clone(),
            automatic_state_save: self.file.automatic_state_save.unwrap_or(false),
            log_level: self
                .file
                .log_level
                .clone()
                .unwrap_or_else(|| String::from("info")),
            log_target: self
                .file
                .log_target
                .clone()
                .unwrap_or_else(|| String::from("stdout")),
            log_access_target: self.file.log_access_target.clone(),
            worker_count: self.file.worker_count.unwrap_or(2),
            worker_automatic_restart: self.file.worker_automatic_restart.unwrap_or(true),
            metrics: self.file.metrics.clone(),
            http_listeners: self.built.http_listeners.clone(),
            https_listeners: self.built.https_listeners.clone(),
            tcp_listeners: self.built.tcp_listeners.clone(),
            clusters: self.built.clusters.clone(),
            handle_process_affinity: self.file.handle_process_affinity.unwrap_or(false),
            ctl_command_timeout: self.file.ctl_command_timeout.unwrap_or(1_000),
            pid_file_path: self.file.pid_file_path.clone(),
            activate_listeners: self.file.activate_listeners.unwrap_or(true),
            front_timeout: self.file.front_timeout.unwrap_or(DEFAULT_FRONT_TIMEOUT),
            back_timeout: self.file.front_timeout.unwrap_or(DEFAULT_BACK_TIMEOUT),
            connect_timeout: self.file.front_timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT),
            //defaults to 30mn
            zombie_check_interval: self.file.zombie_check_interval.unwrap_or(30 * 60),
            accept_queue_timeout: self.file.accept_queue_timeout.unwrap_or(60),
        })
    }
}

/// Sōzu config as parsed from the TOML config, used to create requests
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default, Deserialize)]
pub struct Config {
    pub config_path: String,
    pub command_socket: String,
    pub command_buffer_size: usize,
    pub max_command_buffer_size: usize,
    pub max_connections: usize,
    pub min_buffers: usize,
    pub max_buffers: usize,
    pub buffer_size: usize,
    pub saved_state: Option<String>,
    #[serde(default)]
    pub automatic_state_save: bool,
    pub log_level: String,
    pub log_target: String,
    #[serde(default)]
    pub log_access_target: Option<String>,
    pub worker_count: u16,
    pub worker_automatic_restart: bool,
    pub metrics: Option<MetricsConfig>,
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

//defaults to 30mn
fn default_zombie_check_interval() -> u32 {
    30 * 60
}

fn default_accept_queue_timeout() -> u32 {
    60
}

impl Config {
    pub fn load_from_path(path: &str) -> anyhow::Result<Config> {
        let file_config =
            FileConfig::load_from_path(path).with_context(|| "Could not load the config file")?;

        let mut config_builder = ConfigBuilder::new(file_config);

        let mut config = config_builder
            .into_config(path)
            .with_context(|| "Could not build config from file")?;

        // replace saved_state with a verified path
        config.saved_state = config
            .saved_state_path()
            .with_context(|| "Invalid saved_state in the config. Check your config file")?;

        Ok(config)
    }

    pub fn generate_config_messages(&self) -> anyhow::Result<Vec<WorkerRequest>> {
        let mut v = Vec::new();
        let mut count = 0u8;

        for listener in &self.http_listeners {
            v.push(WorkerRequest {
                id: format!("CONFIG-{count}"),
                content: Request::AddHttpListener(listener.clone()),
            });
            count += 1;
        }

        for listener in &self.https_listeners {
            v.push(WorkerRequest {
                id: format!("CONFIG-{count}"),
                content: Request::AddHttpsListener(listener.clone()),
            });
            count += 1;
        }

        for listener in &self.tcp_listeners {
            v.push(WorkerRequest {
                id: format!("CONFIG-{count}"),
                content: Request::AddTcpListener(listener.clone()),
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
                    content: Request::ActivateListener(ActivateListener {
                        address: listener.address.clone(),
                        proxy: ListenerType::HTTP,
                        from_scm: false,
                    }),
                });
                count += 1;
            }

            for listener in &self.https_listeners {
                v.push(WorkerRequest {
                    id: format!("CONFIG-{count}"),
                    content: Request::ActivateListener(ActivateListener {
                        address: listener.address.clone(),
                        proxy: ListenerType::HTTPS,
                        from_scm: false,
                    }),
                });
                count += 1;
            }

            for listener in &self.tcp_listeners {
                v.push(WorkerRequest {
                    id: format!("CONFIG-{count}"),
                    content: Request::ActivateListener(ActivateListener {
                        address: listener.address.clone(),
                        proxy: ListenerType::TCP,
                        from_scm: false,
                    }),
                });
                count += 1;
            }
        }

        Ok(v)
    }

    pub fn command_socket_path(&self) -> anyhow::Result<String> {
        let config_path_buf = PathBuf::from(self.config_path.clone());
        let mut config_folder = match config_path_buf.parent() {
            Some(path) => path.to_path_buf(),
            None => bail!("could not get parent folder of configuration file"),
        };

        let socket_path = PathBuf::from(self.command_socket.clone());
        let mut parent = match socket_path.parent() {
            None => config_folder,
            Some(path) => {
                config_folder.push(path);
                config_folder.canonicalize().with_context(|| {
                    format!("could not get command socket folder path: {path:?}")
                })?
            }
        };

        let path = match socket_path.file_name() {
            None => bail!("could not get command socket file name"),
            Some(f) => {
                parent.push(f);
                parent
            }
        };

        path.to_str()
            .map(|s| s.to_string())
            .with_context(|| "could not parse command socket path")
    }

    fn saved_state_path(&self) -> anyhow::Result<Option<String>> {
        let path = match self.saved_state.as_ref() {
            Some(path) => path,
            None => return Ok(None),
        };

        debug!("saved_stated path in the config: {}", path);

        let config_path_buf = PathBuf::from(self.config_path.clone());
        debug!("Config path buffer: {:?}", config_path_buf);

        let config_folder = config_path_buf
            .parent()
            .with_context(|| "could not get parent folder of configuration file")?;

        debug!("Config folder: {:?}", config_folder);

        let mut saved_state_path_raw = config_folder.to_path_buf();

        saved_state_path_raw.push(path);
        debug!(
            "Looking for saved state on the path {:?}",
            saved_state_path_raw
        );

        saved_state_path_raw.canonicalize().with_context(|| {
            format!("could not get saved state path from config file input {path:?}")
        })?;

        let stringified_path = saved_state_path_raw
            .to_str()
            .ok_or_else(|| anyhow::Error::msg("Invalid character format, expected UTF8"))?
            .to_string();

        Ok(Some(stringified_path))
    }

    pub fn load_file(path: &str) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    pub fn load_file_bytes(path: &str) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }
}

pub fn display_toml_error(file: &str, error: &toml::de::Error) {
    println!("error parsing the configuration file '{file}': {error}");
    if let Some(Range { start, end }) = error.span() {
        print!("error parsing the configuration file '{file}' at position: {start}, {end}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use toml::to_string;

    #[test]
    fn serialize() {
        let http = ListenerBuilder::new("127.0.0.1:8080", ListenerProtocol::Http)
            .with_answer_404_path(Some("404.html"))
            .to_owned();
        println!("http: {:?}", to_string(&http));

        let https = ListenerBuilder::new("127.0.0.1:8443", ListenerProtocol::Https)
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
