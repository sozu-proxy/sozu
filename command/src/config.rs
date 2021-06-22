//! parsing data from the configuration file
use std::{error, fmt, env};
use std::fs::File;
use std::path::PathBuf;
use std::str::FromStr;
use std::iter::repeat;
use std::net::SocketAddr;
use std::collections::{HashMap,HashSet};
use std::io::{self,Error,ErrorKind,Read};

use crate::certificate::split_certificate_chain;
use toml;

use crate::proxy::{CertificateAndKey,ProxyRequestData,HttpFront,TcpFront,Backend,
  HttpListener,HttpsListener,TcpListener,AddCertificate,TlsProvider,LoadBalancingParams,
  LoadMetric, Application, TlsVersion,ActivateListener,ListenerType};

use crate::command::{CommandRequestData,CommandRequest,PROTOCOL_VERSION};


#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listener {
  pub address:            SocketAddr,
  pub protocol:           FileListenerProtocolConfig,
  pub public_address:     Option<SocketAddr>,
  pub answer_404:         Option<String>,
  pub answer_503:         Option<String>,
  pub cipher_list:        Option<String>,
  pub rustls_cipher_list: Option<Vec<String>>,
  pub tls_versions:       Option<Vec<TlsVersion>>,
  pub expect_proxy:       Option<bool>,
  #[serde(default = "default_sticky_name")]
  pub sticky_name:        String,
  pub certificate:        Option<String>,
  pub certificate_chain:  Option<String>,
  pub key:                Option<String>,
  pub front_timeout:      Option<u32>,
  pub back_timeout:       Option<u32>,
  pub connect_timeout:    Option<u32>,
}

fn default_sticky_name() -> String {
  String::from("SOZUBALANCEID")
}

impl Listener {
  pub fn new(address: SocketAddr, protocol: FileListenerProtocolConfig) -> Listener {
    Listener {
      address,
      protocol,
      public_address:     None,
      answer_404:         None,
      answer_503:         None,
      cipher_list:        None,
      rustls_cipher_list: None,
      tls_versions:       None,
      expect_proxy:       None,
      sticky_name:        String::from("SOZUBALANCEID"),
      certificate:        None,
      certificate_chain:  None,
      key:                None,
      front_timeout:      None,
      back_timeout:       None,
      connect_timeout:    None,
    }
  }

  pub fn to_http(&self, front_timeout: Option<u32>, back_timeout: Option<u32>, connect_timeout: Option<u32>) -> Option<HttpListener> {
    if self.protocol != FileListenerProtocolConfig::Http {
      error!("cannot convert listener to HTTP");
      return None;
    }

    /*FIXME
    let mut address = self.address.clone();
    address.push(':');
    address.push_str(&self.port.to_string());

    let http_proxy_configuration = match address.parse() {
      Ok(addr) => Some(addr),
      Err(err) => {
        error!("Couldn't parse address of HTTP proxy: {}", err);
        None
      }
    };
    */
    let http_proxy_configuration = Some(self.address);

    http_proxy_configuration.map(|addr| {
      let mut configuration = HttpListener {
        front:          addr,
        public_address: self.public_address,
        expect_proxy:   self.expect_proxy.unwrap_or(false),
        sticky_name:    self.sticky_name.clone(),
        front_timeout: self.front_timeout.or(front_timeout).unwrap_or(60),
        back_timeout: self.back_timeout.or(back_timeout).unwrap_or(30),
        connect_timeout: self.connect_timeout.or(connect_timeout).unwrap_or(3),
        ..Default::default()
      };

      //FIXME: error messages if file not found?
      let mut answer_404 = String::new();
      if self.answer_404.as_ref().and_then(|path| File::open(path).ok())
        .and_then(|mut file| file.read_to_string(&mut answer_404).ok()).is_some() {
        configuration.answer_404 = answer_404;
      } else {
        configuration.answer_404 = String::from(include_str!("../assets/404.html"));
      }
      let mut answer_503 = String::new();
      if self.answer_503.as_ref().and_then(|path| File::open(path).ok())
        .and_then(|mut file| file.read_to_string(&mut answer_503).ok()).is_some() {
        configuration.answer_503 = answer_503;
      } else {
        configuration.answer_503 = String::from(include_str!("../assets/503.html"));
      }
      configuration
    })
  }

  pub fn to_tls(&self, front_timeout: Option<u32>, back_timeout: Option<u32>, connect_timeout: Option<u32>) -> Option<HttpsListener> {
    if self.protocol != FileListenerProtocolConfig::Https {
      error!("cannot convert listener to HTTPS");
      return None;
    }

    let cipher_list:String = self.cipher_list.clone().unwrap_or_else(||
      String::from(
        "ECDHE-ECDSA-CHACHA20-POLY1305:ECDHE-RSA-CHACHA20-POLY1305:\
        ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256:\
        ECDHE-ECDSA-AES256-GCM-SHA384:ECDHE-RSA-AES256-GCM-SHA384:\
        DHE-RSA-AES128-GCM-SHA256:DHE-RSA-AES256-GCM-SHA384:\
        ECDHE-ECDSA-AES128-SHA256:ECDHE-RSA-AES128-SHA256:\
        ECDHE-ECDSA-AES128-SHA:ECDHE-RSA-AES256-SHA384:\
        ECDHE-RSA-AES128-SHA:ECDHE-ECDSA-AES256-SHA384:\
        ECDHE-ECDSA-AES256-SHA:ECDHE-RSA-AES256-SHA:\
        DHE-RSA-AES128-SHA256:DHE-RSA-AES128-SHA:DHE-RSA-AES256-SHA256:\
        DHE-RSA-AES256-SHA:ECDHE-ECDSA-DES-CBC3-SHA:\
        ECDHE-RSA-DES-CBC3-SHA:EDH-RSA-DES-CBC3-SHA:\
        AES128-GCM-SHA256:AES256-GCM-SHA384:AES128-SHA256:\
        AES256-SHA256:AES128-SHA:AES256-SHA:DES-CBC3-SHA:!DSS"));

    let supported_ciphersuites: HashSet<&str> = ["TLS13_CHACHA20_POLY1305_SHA256", "TLS13_AES_256_GCM_SHA384",
     "TLS13_AES_128_GCM_SHA256", "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256",
     "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256",
     "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384",
     "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256",
     "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384",
     "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256"].iter().cloned().collect();

    if let Some(ref list) = self.rustls_cipher_list {
      for cipher in list.iter() {
        if !supported_ciphersuites.contains(cipher.as_str()) {
          error!("unknown rustls ciphersuite: {}", cipher);
        }
      }
    }

    let rustls_cipher_list = self.rustls_cipher_list.clone().unwrap_or_default();

    //FIXME
    let tls_proxy_configuration = Some(self.address);

    let versions = match self.tls_versions {
      None    => vec!(TlsVersion::TLSv1_2, TlsVersion::TLSv1_3),
      Some(ref v) => v.clone(),
    };

    let key = self.key.as_ref().and_then(|path| Config::load_file(&path).map_err(|e| {
      error!("cannot load key at path '{}': {:?}", path, e);
      e
    }).ok());
    let certificate = self.certificate.as_ref().and_then(|path| Config::load_file(&path).map_err(|e| {
      error!("cannot load certificate at path '{}': {:?}", path, e);
      e
    }).ok());
    let certificate_chain = self.certificate_chain.as_ref().and_then(|path| Config::load_file(&path).map_err(|e| {
      error!("cannot load certificate chain at path '{}': {:?}", path, e);
      e
    }).ok())
      .map(split_certificate_chain)
      .unwrap_or_else(Vec::new);

    let expect_proxy = self.expect_proxy.unwrap_or(false);


    tls_proxy_configuration.map(|addr| {
      let mut configuration = HttpsListener {
        front:           addr,
        sticky_name:     self.sticky_name.clone(),
        public_address:  self.public_address,
        cipher_list,
        versions,
        expect_proxy,
        rustls_cipher_list,
        key,
        certificate,
        certificate_chain,
        front_timeout: self.front_timeout.or(front_timeout).unwrap_or(60),
        back_timeout: self.back_timeout.or(back_timeout).unwrap_or(30),
        connect_timeout: self.connect_timeout.or(connect_timeout).unwrap_or(3),
        ..Default::default()
      };

      let mut answer_404 = String::new();
      if self.answer_404.as_ref().and_then(|path| File::open(path).ok())
        .and_then(|mut file| file.read_to_string(&mut answer_404).ok()).is_some() {
        configuration.answer_404 = answer_404;
      } else {
        configuration.answer_404 = String::from(include_str!("../assets/404.html"));
      }
      let mut answer_503 = String::new();
      if self.answer_503.as_ref().and_then(|path| File::open(path).ok())
        .and_then(|mut file| file.read_to_string(&mut answer_503).ok()).is_some() {
        configuration.answer_503 = answer_503;
      } else {
        configuration.answer_503 = String::from(include_str!("../assets/503.html"));
      }
      if let Some(cipher_list) = self.cipher_list.as_ref() {
        configuration.cipher_list = cipher_list.clone();
      }

      configuration
    })
  }

  pub fn to_tcp(&self, front_timeout: Option<u32>, back_timeout: Option<u32>, connect_timeout: Option<u32>) -> Option<TcpListener> {
    /*let mut address = self.address.clone();
    address.push(':');
    address.push_str(&self.port.to_string());
    */

    /*let addr_parsed = match address.parse() {
      Ok(addr) => Some(addr),
      Err(err) => {
        error!("Couldn't parse address of HTTP proxy: {}", err);
        None
      }
    };
    */
    let addr_parsed = Some(self.address);

    addr_parsed.map(|addr| {
      TcpListener {
        front:          addr,
        public_address: self.public_address,
        expect_proxy:   self.expect_proxy.unwrap_or(false),
        front_timeout: self.front_timeout.or(front_timeout).unwrap_or(60),
        back_timeout: self.back_timeout.or(back_timeout).unwrap_or(30),
        connect_timeout: self.connect_timeout.or(connect_timeout).unwrap_or(3),
      }
    })

  }
}


#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsConfig {
  pub address:        SocketAddr,
  #[serde(default)]
  pub tagged_metrics: bool,
  #[serde(default)]
  pub prefix:         Option<String>,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[serde(deny_unknown_fields)]
pub enum ProxyProtocolConfig {
  ExpectHeader,
  SendHeader,
  RelayHeader,
}
#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileAppFrontendConfig {
  pub address:           SocketAddr,
  pub hostname:          Option<String>,
  pub path_begin:        Option<String>,
  pub certificate:       Option<String>,
  pub key:               Option<String>,
  pub certificate_chain: Option<String>,
  #[serde(default)]
  pub tls_versions:      Vec<TlsVersion>,
}

impl FileAppFrontendConfig {
  pub fn to_tcp_front(&self) -> Result<TcpFrontendConfig, String> {
    if self.hostname.is_some() {
      return Err(String::from("invalid 'hostname' field for TCP frontend"));
    }
    if self.path_begin.is_some() {
      return Err(String::from("invalid 'path_begin' field for TCP frontend"));
    }
    if self.certificate.is_some() {
      return Err(String::from("invalid 'certificate' field for TCP frontend"));
    }
    if self.hostname.is_some() {
      return Err(String::from("invalid 'key' field for TCP frontend"));
    }
    if self.certificate_chain.is_some() {
      return Err(String::from("invalid 'certificate_chain' field for TCP frontend"));
    }

    Ok(TcpFrontendConfig {
      address: self.address,
    })
  }

  pub fn to_http_front(&self, _app_id: &str) -> Result<HttpFrontendConfig, String> {
    if self.hostname.is_none() {
      return Err(String::from("HTTP frontend should have a 'hostname' field"));
    }

    let key_opt         = self.key.as_ref().and_then(|path| Config::load_file(&path).map_err(|e| {
      error!("cannot load key at path '{}': {:?}", path, e);
      e
    }).ok());
    let certificate_opt = self.certificate.as_ref().and_then(|path| Config::load_file(&path).map_err(|e| {
      error!("cannot load certificate at path '{}': {:?}", path, e);
      e
    }).ok());
    let chain_opt       = self.certificate_chain.as_ref().and_then(|path| Config::load_file(&path).map_err(|e| {
      error!("cannot load certificate chain at path '{}': {:?}", path, e);
      e
    }).ok())
      .map(split_certificate_chain);

    Ok(HttpFrontendConfig {
      address:           self.address,
      hostname:          self.hostname.clone().unwrap(),
      path_begin:        self.path_begin.clone().unwrap_or_default(),
      certificate:       certificate_opt,
      key:               key_opt,
      certificate_chain: chain_opt,
      tls_versions:      self.tls_versions.clone(),
    })
  }
}

#[derive(Debug,Clone,Copy,PartialEq,Eq,Hash,Serialize,Deserialize)]
#[serde(deny_unknown_fields, rename_all="lowercase")]
pub enum FileListenerProtocolConfig {
  Http,
  Https,
  Tcp,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
#[serde(deny_unknown_fields, rename_all="lowercase")]
pub enum FileAppProtocolConfig {
  Http,
  Tcp,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileAppConfig {
  pub frontends: Vec<FileAppFrontendConfig>,
  pub backends: Vec<BackendConfig>,
  pub protocol: FileAppProtocolConfig,
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

#[derive(Debug,Copy,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadBalancingAlgorithms {
  RoundRobin,
  Random,
  LeastLoaded,
  PowerOfTwo,
}

impl Default for LoadBalancingAlgorithms {
  fn default() -> Self {
    LoadBalancingAlgorithms::RoundRobin
  }
}

#[derive(Debug)]
pub struct ParseErrorLoadBalancing;

impl fmt::Display for ParseErrorLoadBalancing {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Cannot find the load balancing policy asked")
    }
}

impl error::Error for ParseErrorLoadBalancing {
    fn description(&self) -> &str {
        "Cannot find the load balancing policy asked"
    }

    fn cause(&self) -> Option<&dyn error::Error> {
        None
    }
}

impl FromStr for LoadBalancingAlgorithms {
  type Err = ParseErrorLoadBalancing;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    match s {
      "roundrobin" => Ok(LoadBalancingAlgorithms::RoundRobin),
      "random" => Ok(LoadBalancingAlgorithms::Random),
      _ => Err(ParseErrorLoadBalancing{}),
    }
  }
}


#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendConfig {
  pub address: SocketAddr,
  pub weight: Option<u8>,
  pub sticky_id: Option<String>,
  pub backup: Option<bool>,
  pub backend_id: Option<String>,
}

impl FileAppConfig {
  pub fn to_app_config(self, app_id: &str, expect_proxy: &HashSet<SocketAddr>) -> Result<AppConfig, String> {
    match self.protocol {
      FileAppProtocolConfig::Tcp => {
        let mut has_expect_proxy = None;
        let mut frontends = Vec::new();
        for f in self.frontends {
          if expect_proxy.contains(&f.address) {
            match has_expect_proxy {
              Some(true) => {},
              Some(false) => return Err(format!("all the listeners for application {} should have the same expect_proxy option", app_id)),
              None => has_expect_proxy = Some(true),
            }
          } else {
            match has_expect_proxy {
              Some(false) => {},
              Some(true) => return Err(format!("all the listeners for application {} should have the same expect_proxy option", app_id)),
              None => has_expect_proxy = Some(false),
            }
          }
          match f.to_tcp_front() {
            Ok(frontend) => frontends.push(frontend),
            Err(e) => return Err(e),
          }
        }


        let send_proxy = self.send_proxy.unwrap_or(false);
        let expect_proxy = has_expect_proxy.unwrap_or(false);
        let proxy_protocol = match (send_proxy, expect_proxy) {
          (true, true)  => Some(ProxyProtocolConfig::RelayHeader),
          (true, false) => Some(ProxyProtocolConfig::SendHeader),
          (false, true) => Some(ProxyProtocolConfig::ExpectHeader),
          _             => None,
        };

        Ok(AppConfig::Tcp(TcpAppConfig {
          app_id: app_id.to_string(),
          frontends,
          backends: self.backends,
          proxy_protocol,
          load_balancing: self.load_balancing,
          load_metric: self.load_metric,
        }))
      },
      FileAppProtocolConfig::Http => {
        let mut frontends = Vec::new();
        for f in self.frontends {
          match f.to_http_front(app_id) {
            Ok(frontend) => frontends.push(frontend),
            Err(e) => return Err(e),
          }
        }

        let answer_503 = self.answer_503.as_ref().and_then(|path| Config::load_file(&path).map_err(|e| {
          error!("cannot load 503 error page at path '{}': {:?}", path, e);
          e
        }).ok());

        Ok(AppConfig::Http(HttpAppConfig {
          app_id: app_id.to_string(),
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

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpFrontendConfig {
  pub address:           SocketAddr,
  pub hostname:          String,
  pub path_begin:        String,
  pub certificate:       Option<String>,
  pub key:               Option<String>,
  pub certificate_chain: Option<Vec<String>>,
  #[serde(default)]
  pub tls_versions:      Vec<TlsVersion>,
}

impl HttpFrontendConfig {
  pub fn generate_orders(&self, app_id: &str) -> Vec<ProxyRequestData> {
    let mut v = Vec::new();

    if self.key.is_some() && self.certificate.is_some() {

      v.push(ProxyRequestData::AddCertificate(AddCertificate{
        front: self.address,
        certificate: CertificateAndKey {
          key:               self.key.clone().unwrap(),
          certificate:       self.certificate.clone().unwrap(),
          certificate_chain: self.certificate_chain.clone().unwrap_or_default(),
          versions:          self.tls_versions.clone(),
        },
        names: vec!(self.hostname.clone()),
      }));

      v.push(ProxyRequestData::AddHttpsFront(HttpFront {
        app_id:      app_id.to_string(),
        address:     self.address,
        hostname:    self.hostname.clone(),
        path_begin:  self.path_begin.clone(),
      }));
    } else {
      //create the front both for HTTP and HTTPS if possible
      v.push(ProxyRequestData::AddHttpFront(HttpFront {
        app_id:     app_id.to_string(),
        address:    self.address,
        hostname:   self.hostname.clone(),
        path_begin: self.path_begin.clone(),
      }));
    }

    v
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpAppConfig {
  pub app_id: String,
  pub frontends: Vec<HttpFrontendConfig>,
  pub backends: Vec<BackendConfig>,
  pub sticky_session: bool,
  pub https_redirect: bool,
  pub load_balancing: LoadBalancingAlgorithms,
  pub load_metric: Option<LoadMetric>,
  pub answer_503: Option<String>,
}

impl HttpAppConfig {
  pub fn generate_orders(&self) -> Vec<ProxyRequestData> {
    let mut v = Vec::new();

    v.push(ProxyRequestData::AddApplication(Application {
      app_id: self.app_id.clone(),
      sticky_session: self.sticky_session,
      https_redirect: self.https_redirect,
      proxy_protocol: None,
      load_balancing: self.load_balancing,
      answer_503: self.answer_503.clone(),
      load_metric: self.load_metric.clone(),
    }));

    for frontend in &self.frontends {
      let mut orders = frontend.generate_orders(&self.app_id);
      v.extend(orders.drain(..));
    }

    let mut backend_count = 0usize;
    for backend in &self.backends {
        let load_balancing_parameters = Some(LoadBalancingParams {
          weight: backend.weight.unwrap_or(100),
        });

        v.push(ProxyRequestData::AddBackend(Backend {
          app_id:     self.app_id.clone(),
          backend_id: backend.backend_id.clone().unwrap_or_else(|| format!("{}-{}-{}", self.app_id, backend_count, backend.address)),
          address:    backend.address,
          load_balancing_parameters,
          sticky_id:  backend.sticky_id.clone(),
          backup:     backend.backup,
        }));

        backend_count += 1;
    }

    v
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
pub struct TcpFrontendConfig {
  pub address: SocketAddr,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
pub struct TcpAppConfig {
  pub app_id: String,
  pub frontends: Vec<TcpFrontendConfig>,
  pub backends: Vec<BackendConfig>,
  #[serde(default)]
  pub proxy_protocol: Option<ProxyProtocolConfig>,
  pub load_balancing: LoadBalancingAlgorithms,
  pub load_metric: Option<LoadMetric>,
}

impl TcpAppConfig {
  pub fn generate_orders(&self) -> Vec<ProxyRequestData> {
    let mut v = Vec::new();

    v.push(ProxyRequestData::AddApplication(Application {
      app_id: self.app_id.clone(),
      sticky_session: false,
      https_redirect: false,
      proxy_protocol: self.proxy_protocol.clone(),
      load_balancing: self.load_balancing,
      load_metric: self.load_metric.clone(),
      answer_503: None,
    }));

    for frontend in &self.frontends {
      v.push(ProxyRequestData::AddTcpFront(TcpFront {
        app_id:  self.app_id.clone(),
        address: frontend.address,
      }));
    }

    let mut backend_count = 0usize;
    for backend in &self.backends {
      let load_balancing_parameters = Some(LoadBalancingParams {
        weight: backend.weight.unwrap_or(100),
      });

      v.push(ProxyRequestData::AddBackend(Backend {
        app_id:     self.app_id.clone(),
        backend_id: backend.backend_id.clone().unwrap_or_else(|| format!("{}-{}-{}", self.app_id, backend_count, backend.address)),
        address:    backend.address,
        load_balancing_parameters,
        sticky_id:  backend.sticky_id.clone(),
        backup:     backend.backup,
      }));

      backend_count += 1;
    }

    v
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
pub enum AppConfig {
  Http(HttpAppConfig),
  Tcp(TcpAppConfig),
}

impl AppConfig {
  pub fn generate_orders(&self) -> Vec<ProxyRequestData> {
    match *self {
      AppConfig::Http(ref http) => http.generate_orders(),
      AppConfig::Tcp(ref tcp)   => tcp.generate_orders(),
    }
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct FileConfig {
  pub command_socket:           Option<String>,
  pub command_buffer_size:      Option<usize>,
  pub max_command_buffer_size:  Option<usize>,
  pub max_connections:          Option<usize>,
  pub min_buffers:              Option<usize>,
  pub max_buffers:              Option<usize>,
  pub buffer_size:              Option<usize>,
  pub saved_state:              Option<String>,
  #[serde(default)]
  pub automatic_state_save:     Option<bool>,
  pub log_level:                Option<String>,
  pub log_target:               Option<String>,
  #[serde(default)]
  pub log_access_target:        Option<String>,
  pub worker_count:             Option<u16>,
  pub worker_automatic_restart: Option<bool>,
  pub metrics:                  Option<MetricsConfig>,
  pub listeners:                Option<Vec<Listener>>,
  pub applications:             Option<HashMap<String, FileAppConfig>>,
  pub handle_process_affinity:  Option<bool>,
  pub ctl_command_timeout:      Option<u64>,
  pub pid_file_path:            Option<String>,
  pub tls_provider:             Option<TlsProvider>,
  pub activate_listeners:       Option<bool>,
  #[serde(default)]
  pub front_timeout:            Option<u32>,
  #[serde(default)]
  pub back_timeout:             Option<u32>,
  #[serde(default)]
  pub connect_timeout:          Option<u32>,
  #[serde(default)]
  pub zombie_check_interval:    Option<u32>,
  #[serde(default)]
  pub accept_queue_timeout:     Option<u32>,
}


impl FileConfig {
  pub fn load_from_path(path: &str) -> io::Result<FileConfig> {
    let data = Config::load_file(path)?;

    match toml::from_str(&data) {
      Err(e)     => {
        display_toml_error(&data, &e);
        Err(Error::new(
          ErrorKind::InvalidData,
          format!("decoding error: {}", e))
        )
      }
      Ok(config) => {
        let config: FileConfig = config;
        let mut reserved_address: HashSet<SocketAddr> = HashSet::new();


        if let Some(l) = config.listeners.as_ref() {
          for listener in l.iter() {
            if reserved_address.contains(&listener.address) {
              println!("listening address {:?} is already used in the configuration", listener.address);
              return Err(Error::new(
                ErrorKind::InvalidData,
                format!("listening address {:?} is already used in the configuration", listener.address)));
            } else {
              reserved_address.insert(listener.address);
            }
          }
        }

        //FIXME: verify how apps and listeners share addresses
        /*
        if let Some(ref apps) = config.applications {
          for (key, app) in apps.iter() {
            if let (Some(address), Some(port)) = (app.ip_address.clone(), app.port) {
              let addr = (address, port);
              if reserved_address.contains(&addr) {
                println!("TCP app '{}' listening address ( {}:{} ) is already used in the configuration",
                  key, addr.0, addr.1);
                return Err(Error::new(
                  ErrorKind::InvalidData,
                  format!("TCP app '{}' listening address ( {}:{} ) is already used in the configuration",
                    key, addr.0, addr.1)));
              } else {
                reserved_address.insert(addr.clone());
              }
            }
          }
        }
        */

        Ok(config)
      },
    }
  }

  pub fn into(self, config_path: &str) -> Config {
    let mut applications = HashMap::new();
    let mut http_listeners = Vec::new();
    let mut https_listeners = Vec::new();
    let mut tcp_listeners = Vec::new();
    let mut known_addresses = HashMap::new();
    let mut expect_proxy = HashSet::new();

    if let Some(listeners) = self.listeners {
      for listener in listeners.iter() {
        if known_addresses.contains_key(&listener.address) {
          panic!("there's already a listener for address {:?}", listener.address);
        }

        known_addresses.insert(listener.address, listener.protocol);
        if listener.expect_proxy == Some(true) {
          expect_proxy.insert(listener.address);
        }

        if listener.public_address.is_some() && listener.expect_proxy == Some(true) {
          panic!("the listener on {} has incompatible options: it cannot use the expect proxy protocol and have a public_address field at the same time", &listener.address);
        }

        match listener.protocol {
          FileListenerProtocolConfig::Https => {
            if let Some(l) = listener.to_tls(self.front_timeout.clone(), self.back_timeout.clone(), self.connect_timeout.clone()) {
              https_listeners.push(l);
            } else {
              panic!("invalid listener");
            }
          },
          FileListenerProtocolConfig::Http => {
            if let Some(l) = listener.to_http(self.front_timeout.clone(), self.back_timeout.clone(), self.connect_timeout.clone()) {
              http_listeners.push(l);
            } else {
              panic!("invalid listener");
            }
          },
          FileListenerProtocolConfig::Tcp => {
            if let Some(l) = listener.to_tcp(self.front_timeout.clone(), self.back_timeout.clone(), self.connect_timeout.clone()) {
              tcp_listeners.push(l);
            } else {
              panic!("invalid listener");
            }
          },
        }
      }
    }

    if let Some(mut apps) = self.applications {
      for (id, app) in apps.drain() {
        match app.to_app_config(id.as_str(), &expect_proxy) {
          Ok(app_config) => {
            match app_config {
              AppConfig::Http(ref http) => {
                for frontend in http.frontends.iter() {
                  match known_addresses.get(&frontend.address) {
                    Some(FileListenerProtocolConfig::Tcp) => {
                      panic!("cannot set up a HTTP or HTTPS frontend on a TCP listener");
                    },
                    Some(FileListenerProtocolConfig::Http) => {
                      if frontend.certificate.is_some() {
                        panic!("cannot set up a HTTPS frontend on a HTTP listener");
                      }
                    },
                    Some(FileListenerProtocolConfig::Https) => {
                      if frontend.certificate.is_none() {
                        println!("known addresses: {:#?}", known_addresses);
                        println!("frontend: {:#?}", frontend);
                        panic!("cannot set up a HTTP frontend on a HTTPS listener");
                      }
                    },
                    None => {
                      // create a default listener for that front
                      let p = if frontend.certificate.is_some() {
                        let listener = Listener::new(frontend.address, FileListenerProtocolConfig::Https);
                        https_listeners.push(listener.to_tls(self.front_timeout.clone(), self.back_timeout.clone(), self.connect_timeout.clone()).unwrap());

                        FileListenerProtocolConfig::Https
                      } else {
                        let listener = Listener::new(frontend.address, FileListenerProtocolConfig::Http);
                        http_listeners.push(listener.to_http(self.front_timeout.clone(), self.back_timeout.clone(), self.connect_timeout.clone()).unwrap());

                        FileListenerProtocolConfig::Http
                      };
                      known_addresses.insert(frontend.address, p);
                    },
                  }
                }
              },
              AppConfig::Tcp(ref tcp) => {
                //FIXME: verify that different TCP apps do not request the same address
                for frontend in &tcp.frontends {
                  match known_addresses.get(&frontend.address) {
                    Some(FileListenerProtocolConfig::Http) | Some(FileListenerProtocolConfig::Https) => {
                      panic!("cannot set up a TCP frontend on a HTTP listener");
                    },
                    Some(FileListenerProtocolConfig::Tcp) => {},
                    None => {
                      // create a default listener for that front
                      let listener = Listener::new(frontend.address, FileListenerProtocolConfig::Tcp);
                      tcp_listeners.push(listener.to_tcp(self.front_timeout.clone(), self.back_timeout.clone(), self.connect_timeout.clone()).unwrap());
                      known_addresses.insert(frontend.address, FileListenerProtocolConfig::Tcp);
                    },
                  }
                }
              },
            }

            applications.insert(id, app_config);
          },
          Err(s)         => {
            panic!("error parsing application configuration for {}: {}", id, s);
          },
        }
      }
    }

    let tls_provider = self.tls_provider.unwrap_or(if cfg!(use_openssl) {
      TlsProvider::Openssl
    } else {
      TlsProvider::Rustls
    });

    let command_socket_path = self.command_socket.unwrap_or({
      let mut path = env::current_dir().unwrap();
      path.push("sozu.sock");
      path.to_str().map(|s| s.to_string()).unwrap()
    });

    match (&self.saved_state, &self.automatic_state_save) {
      (None, Some(true)) => panic!("cannot activate automatic state save if the 'saved_state` option is not set"),
      _ => {}
    }

    Config {
      config_path:    config_path.to_string(),
      command_socket: command_socket_path,
      command_buffer_size: self.command_buffer_size.unwrap_or(1_000_000),
      max_command_buffer_size: self.max_command_buffer_size.unwrap_or( self.command_buffer_size.unwrap_or(1_000_000) * 2),
      max_connections: self.max_connections.unwrap_or(10000),
      min_buffers: std::cmp::min(self.min_buffers.unwrap_or(1), self.max_buffers.unwrap_or(1000)),
      max_buffers: self.max_buffers.unwrap_or(1000),
      buffer_size: self.buffer_size.unwrap_or(16384),
      saved_state: self.saved_state,
      automatic_state_save: self.automatic_state_save.unwrap_or(false),
      log_level: self.log_level.unwrap_or_else(|| String::from("info")),
      log_target: self.log_target.unwrap_or_else(|| String::from("stdout")),
      log_access_target: self.log_access_target,
      worker_count: self.worker_count.unwrap_or(2),
      worker_automatic_restart: self.worker_automatic_restart.unwrap_or(true),
      metrics: self.metrics,
      http_listeners,
      https_listeners,
      tcp_listeners,
      applications,
      handle_process_affinity: self.handle_process_affinity.unwrap_or(false),
      ctl_command_timeout: self.ctl_command_timeout.unwrap_or(1_000),
      pid_file_path: self.pid_file_path,
      tls_provider,
      activate_listeners: self.activate_listeners.unwrap_or(true),
      front_timeout: self.front_timeout.unwrap_or(60),
      back_timeout: self.front_timeout.unwrap_or(30),
      connect_timeout: self.front_timeout.unwrap_or(3),
      //defaults to 30mn
      zombie_check_interval: self.zombie_check_interval.unwrap_or(30 * 60),
      accept_queue_timeout: self.accept_queue_timeout.unwrap_or(60),
    }
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct Config {
  pub config_path:              String,
  pub command_socket:           String,
  pub command_buffer_size:      usize,
  pub max_command_buffer_size:  usize,
  pub max_connections:          usize,
  pub min_buffers:              usize,
  pub max_buffers:              usize,
  pub buffer_size:              usize,
  pub saved_state:              Option<String>,
  #[serde(default)]
  pub automatic_state_save:     bool,
  pub log_level:                String,
  pub log_target:               String,
  #[serde(default)]
  pub log_access_target:        Option<String>,
  pub worker_count:             u16,
  pub worker_automatic_restart: bool,
  pub metrics:                  Option<MetricsConfig>,
  pub http_listeners:           Vec<HttpListener>,
  pub https_listeners:          Vec<HttpsListener>,
  pub tcp_listeners:            Vec<TcpListener>,
  pub applications:             HashMap<String, AppConfig>,
  pub handle_process_affinity:  bool,
  pub ctl_command_timeout:      u64,
  pub pid_file_path:            Option<String>,
  pub tls_provider:             TlsProvider,
  pub activate_listeners:       bool,
  #[serde(default = "default_front_timeout")]
  pub front_timeout:            u32,
  #[serde(default = "default_back_timeout")]
  pub back_timeout:            u32,
  #[serde(default = "default_connect_timeout")]
  pub connect_timeout:            u32,
  #[serde(default = "default_zombie_check_interval")]
  pub zombie_check_interval:    u32,
  #[serde(default = "default_accept_queue_timeout")]
  pub accept_queue_timeout:     u32,
}

fn default_front_timeout() -> u32 {
  60
}

fn default_back_timeout() -> u32 {
  30
}

fn default_connect_timeout() -> u32 {
  3
}

//defaults to 30mn
fn default_zombie_check_interval() -> u32 {
  30*60
}

fn default_accept_queue_timeout() -> u32 {
  60
}

impl Config {
  pub fn load_from_path(path: &str) -> io::Result<Config> {
    FileConfig::load_from_path(path).map(|config| config.into(path))
  }

  pub fn generate_config_messages(&self) -> Vec<CommandRequest> {
    let mut v = Vec::new();
    let mut count = 0u8;

    for listener in &self.http_listeners {
      v.push(CommandRequest {
        id:       format!("CONFIG-{}", count),
        version:  PROTOCOL_VERSION,
        worker_id: None,
        data:     CommandRequestData::Proxy(ProxyRequestData::AddHttpListener(listener.clone())),
      });
      count += 1;
    }

    for listener in &self.https_listeners {
      v.push(CommandRequest {
        id:       format!("CONFIG-{}", count),
        version:  PROTOCOL_VERSION,
        worker_id: None,
        data:     CommandRequestData::Proxy(ProxyRequestData::AddHttpsListener(listener.clone())),
      });
      count += 1;
    }

    for listener in &self.tcp_listeners {
      v.push(CommandRequest {
        id:       format!("CONFIG-{}", count),
        version:  PROTOCOL_VERSION,
        worker_id: None,
        data:     CommandRequestData::Proxy(ProxyRequestData::AddTcpListener(listener.clone())),
      });
      count += 1;
    }

    for app in self.applications.values() {
      let mut orders = app.generate_orders();
      for order in orders.drain(..) {
        v.push(CommandRequest {
          id:       format!("CONFIG-{}", count),
          version:  PROTOCOL_VERSION,
          worker_id: None,
          data:     CommandRequestData::Proxy(order),
        });
        count += 1;
      }
    }

    if self.activate_listeners {
      for listener in &self.http_listeners {
        v.push(CommandRequest {
          id:       format!("CONFIG-{}", count),
          version:  PROTOCOL_VERSION,
          worker_id: None,
          data:     CommandRequestData::Proxy(ProxyRequestData::ActivateListener(ActivateListener{
            front:    listener.front,
            proxy:    ListenerType::HTTP,
            from_scm: false,
          })),
        });
        count += 1;
      }

      for listener in &self.https_listeners {
        v.push(CommandRequest {
          id:       format!("CONFIG-{}", count),
          version:  PROTOCOL_VERSION,
          worker_id: None,
          data:     CommandRequestData::Proxy(ProxyRequestData::ActivateListener(ActivateListener{
            front:    listener.front,
            proxy:    ListenerType::HTTPS,
            from_scm: false,
          })),
        });
        count += 1;
      }

      for listener in &self.tcp_listeners {
        v.push(CommandRequest {
          id:       format!("CONFIG-{}", count),
          version:  PROTOCOL_VERSION,
          worker_id: None,
          data:     CommandRequestData::Proxy(ProxyRequestData::ActivateListener(ActivateListener{
            front:    listener.front,
            proxy:    ListenerType::TCP,
            from_scm: false,
          })),
        });
        count += 1;
      }
    }

    v
  }

  pub fn command_socket_path(&self) -> String {
    let config_path_buf = PathBuf::from(self.config_path.clone());
    let mut config_folder = config_path_buf.parent().expect("could not get parent folder of configuration file").to_path_buf();

    let socket_path = PathBuf::from(self.command_socket.clone());
    let mut parent = match socket_path.parent() {
      None => config_folder,
      Some(path) => {
        config_folder.push(path);
        match config_folder.canonicalize() {
          Ok(path) => path,
           Err(e)   => panic!("could not get command socket folder path: {}", e),
        }
      }
    };

    let path = match socket_path.file_name() {
      None => panic!("could not get command socket file name"),
      Some(f) => {
        parent.push(f);
        parent
      }
    };

    path.to_str().map(|s| s.to_string()).expect("could not parse command socket path")
  }

  pub fn saved_state_path(&self) -> Option<String> {
    self.saved_state.as_ref().and_then(|path| {
      let config_path_buf = PathBuf::from(self.config_path.clone());
      let config_folder = config_path_buf.parent().expect("could not get parent folder of configuration file");
      let mut saved_state_path_raw = config_folder.to_path_buf();
      saved_state_path_raw.push(path);
      saved_state_path_raw.canonicalize().map_err(|e| {
        error!("could not get saved state path: {}", e);
      }).ok().and_then(|path| path.to_str().map(|s| s.to_string()))
    })
  }

  pub fn load_file(path: &str) -> io::Result<String> {
    std::fs::read_to_string(path)
  }

  pub fn load_file_bytes(path: &str) -> io::Result<Vec<u8>> {
    std::fs::read(path)
  }
}

pub fn display_toml_error(file: &str, error: &toml::de::Error) {
  println!("error parsing the configuration file: {}", error);
  if let Some((line, column)) = error.line_col() {
    let l_span = line.to_string().len();
    println!("{}| {}", line + 1, file.lines().nth(line).unwrap());
    println!("{}^", repeat(' ').take(l_span + 2 + column).collect::<String>());
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use toml::to_string;

  #[test]
  fn serialize() {
    let http = Listener {
      address: "127.0.0.1:8080".parse().unwrap(),
      protocol: FileListenerProtocolConfig::Http,
      answer_404: Some(String::from("404.html")),
      answer_503: None,
      public_address: None,
      tls_versions: None,
      cipher_list: None,
      rustls_cipher_list: None,
      expect_proxy: None,
      sticky_name: "SOZUBALANCEID".to_string(),
      certificate:        None,
      certificate_chain:  None,
      key:                None,
      front_timeout: None,
      back_timeout: None,
      connect_timeout: None,
    };
    println!("http: {:?}", to_string(&http));
    let https = Listener {
      address: "127.0.0.1:8443".parse().unwrap(),
      protocol: FileListenerProtocolConfig::Https,
      answer_404: Some(String::from("404.html")),
      answer_503: None,
      public_address: None,
      tls_versions: None,
      cipher_list: None,
      rustls_cipher_list: None,
      expect_proxy: None,
      sticky_name: "SOZUBALANCEID".to_string(),
      certificate:        None,
      certificate_chain:  None,
      key:                None,
      front_timeout: None,
      back_timeout: None,
      connect_timeout: None,
    };
    println!("https: {:?}", to_string(&https));

    let mut listeners = Vec::new();
    listeners.push(http);
    listeners.push(https);

    let config = FileConfig {
      command_socket: Some(String::from("./command_folder/sock")),
      saved_state: None,
      automatic_state_save: None,
      worker_count: Some(2),
      worker_automatic_restart: Some(true),
      handle_process_affinity: None,
      command_buffer_size: None,
      max_connections: Some(500),
      min_buffers: Some(1),
      max_buffers: Some(500),
      buffer_size: Some(16384),
      max_command_buffer_size: None,
      log_level:  None,
      log_target: None,
      log_access_target: None,
      metrics: Some(MetricsConfig {
        address: "127.0.0.1:8125".parse().unwrap(),
        tagged_metrics: false,
        prefix: Some(String::from("sozu-metrics")),
      }),
      listeners: Some(listeners),
      applications: None,
      ctl_command_timeout: None,
      pid_file_path: None,
      tls_provider: None,
      activate_listeners: None,
      front_timeout: None,
      back_timeout: None,
      connect_timeout: None,
      zombie_check_interval: None,
      accept_queue_timeout: None,
    };

    println!("config: {:?}", to_string(&config));
    let encoded = to_string(&config).unwrap();
    println!("conf:\n{}", encoded);
  }

  #[test]
  fn parse() {
    let res = Config::load_from_path("assets/config.toml");
    let config = res.unwrap();
    println!("config: {:#?}", config);
    //panic!();
  }
}
