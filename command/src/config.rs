//! parsing data from the configuration file
use std::{error, fmt};
use std::fs::File;
use std::path::PathBuf;
use std::str::FromStr;
use std::iter::repeat;
use std::net::SocketAddr;
use std::collections::{HashMap,HashSet};
use std::io::{self,Error,ErrorKind,Read};

use certificate::{calculate_fingerprint,split_certificate_chain};
use toml;

use messages::{CertFingerprint,CertificateAndKey,Order,HttpFront,HttpsFront,TcpFront,Backend,
  HttpProxyConfiguration,HttpsProxyConfiguration,AddCertificate,TlsProvider,LoadBalancingParams,
  Application, TlsVersion};

use data::{ConfigCommand,ConfigMessage,PROTOCOL_VERSION};

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
  pub address:                   String,
  pub public_address:            Option<String>,
  pub port:                      u16,
  pub answer_404:                Option<String>,
  pub answer_503:                Option<String>,
  pub cipher_list:               Option<String>,
  pub rustls_cipher_list:        Option<Vec<String>>,
  pub default_name:              Option<String>,
  pub default_app_id:            Option<String>,
  pub default_certificate:       Option<String>,
  pub default_certificate_chain: Option<String>,
  pub default_key:               Option<String>,
  pub tls_versions:              Option<Vec<TlsVersion>>,
  pub tls_provider:              Option<TlsProvider>,
  pub expect_proxy:              Option<bool>,
  #[serde(default = "default_sticky_name")]
  pub sticky_name:               String,
}

fn default_sticky_name() -> String {
  String::from("SOZUBALANCEID")
}

impl ProxyConfig {
  pub fn to_http(&self) -> Option<HttpProxyConfiguration> {
    let mut address = self.address.clone();
    address.push(':');
    address.push_str(&self.port.to_string());

    let public_address = self.public_address.as_ref().and_then(|addr| FromStr::from_str(&addr).ok());
    let http_proxy_configuration = match address.parse() {
      Ok(addr) => Some(addr),
      Err(err) => {
        error!("Couldn't parse address of HTTP proxy: {}", err);
        None
      }
    };

    http_proxy_configuration.map(|addr| {
      let mut configuration = HttpProxyConfiguration {
        front:          addr,
        public_address: public_address,
        expect_proxy:   self.expect_proxy.unwrap_or(false),
        sticky_name:    self.sticky_name.clone(),
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

  pub fn to_tls(&self) -> Option<HttpsProxyConfiguration> {
    let mut address = self.address.clone();
    address.push(':');
    address.push_str(&self.port.to_string());

    let public_address     = self.public_address.as_ref().and_then(|addr| FromStr::from_str(&addr).ok());
    let cipher_list:String = self.cipher_list.clone().unwrap_or(
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

    let rustls_cipher_list = self.rustls_cipher_list.clone();

    let tls_proxy_configuration = match address.parse() {
      Ok(addr) => Some(addr),
      Err(err) => {
        error!("Couldn't parse address of TLS proxy: {}", err);
        None
      }
    };

    let versions = match self.tls_versions {
      None    => vec!(TlsVersion::TLSv1_2),
      Some(ref v) => v.clone(),
    };

    let expect_proxy = self.expect_proxy.unwrap_or(false);
    let tls_provider = self.tls_provider.unwrap_or(if cfg!(use_openssl) {
      TlsProvider::Openssl
    } else {
      TlsProvider::Rustls
    });


    tls_proxy_configuration.map(|addr| {
      let mut configuration = HttpsProxyConfiguration {
        front:           addr,
        public_address:  public_address,
        cipher_list:     cipher_list,
        versions:        versions,
        expect_proxy:    expect_proxy,
        sticky_name:     self.sticky_name.clone(),
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
      let mut default_cert: Vec<u8> = vec!();
      if self.default_certificate.as_ref().and_then(|path| File::open(path).ok())
        .and_then(|mut file| file.read_to_end(&mut default_cert).ok()).is_some() {
        configuration.default_certificate = Some(default_cert);
      }

      configuration.default_app_id            = self.default_app_id.clone();
      configuration.default_certificate_chain = self.default_certificate_chain.clone();

      let mut default_key: Vec<u8> = vec!();
      if self.default_key.as_ref().and_then(|path| File::open(path).ok())
        .and_then(|mut file| file.read_to_end(&mut default_key).ok()).is_some() {
        configuration.default_key = Some(default_key);
      }

      configuration.tls_provider = tls_provider;

      configuration

    })
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TcpProxyConfig {
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsConfig {
  pub address:        String,
  pub port:           u16,
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
pub struct FileAppConfig {
  pub ip_address:        Option<String>,
  pub port:              Option<u16>,
  pub hostname:          Option<String>,
  pub path_begin:        Option<String>,
  pub certificate:       Option<String>,
  pub key:               Option<String>,
  pub certificate_chain: Option<String>,
  pub backends:          Vec<BackendConfig>,
  pub sticky_session:    Option<bool>,
  pub https_redirect:    Option<bool>,
  #[serde(default)]
  pub send_proxy:        Option<bool>,
  #[serde(default)]
  pub expect_proxy:      Option<bool>,
  #[serde(default)]
  pub load_balancing_policy: LoadBalancingAlgorithms,
}

#[derive(Debug,Copy,Clone,PartialEq,Eq,Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoadBalancingAlgorithms {
  RoundRobin,
  Random,
  LeastConnections,
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

    fn cause(&self) -> Option<&error::Error> {
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
}

impl FileAppConfig {
  pub fn to_app_config(self, app_id: &str) -> Result<AppConfig, String> {
    let send_proxy = self.send_proxy.unwrap_or(false);
    let expect_proxy = self.expect_proxy.unwrap_or(false);

    if self.hostname.is_none() && self.path_begin.is_none() && self.certificate.is_none() &&
      self.key.is_none() && self.certificate_chain.is_none() && self.sticky_session.is_none() {
      let proxy_protocol = match (send_proxy, expect_proxy) {
        (true, true)  => Some(ProxyProtocolConfig::RelayHeader),
        (true, false) => Some(ProxyProtocolConfig::SendHeader),
        (false, true) => Some(ProxyProtocolConfig::ExpectHeader),
        _             => None,
      };

      match (self.ip_address, self.port) {
        (Some(ip), Some(port)) => {
          Ok(AppConfig::Tcp(TcpAppConfig {
            app_id:         app_id.to_string(),
            ip_address:     ip,
            port:           port,
            backends:       self.backends,
            proxy_protocol,
            load_balancing_policy: self.load_balancing_policy,
          }))
        },
        (None, Some(_)) => Err(String::from("missing IP address for TCP application")),
        (Some(_), None) => Err(String::from("missing port for TCP application")),
        (None, None) => Err(String::from("missing IP address port for TCP application")),
      }
    } else {
      if self.expect_proxy.is_some() {
        return Err(String::from("invalid `expect_proxy` field in HTTP application"));
      }

      if self.hostname.is_none() {
        return Err(String::from("missing hostname for HTTP application"));
      }

      let path_begin     = self.path_begin.unwrap_or(String::new());
      let sticky_session = self.sticky_session.unwrap_or(false);
      let https_redirect = self.https_redirect.unwrap_or(false);

      let key_opt         = self.key.as_ref().and_then(|path| Config::load_file(&path).ok());
      let certificate_opt = self.certificate.as_ref().and_then(|path| Config::load_file(&path).ok());
      let chain_opt       = self.certificate_chain.as_ref().and_then(|path| Config::load_file(&path).ok())
        .map(split_certificate_chain);

      if key_opt.is_some() || certificate_opt.is_some()  || chain_opt.is_some() {

        if key_opt.is_none() {
          return Err(format!("cannot read the key at {:?}", self.key));
        }
        if certificate_opt.is_none() {
          return Err(format!("cannot read the certificate at {:?}", self.certificate));
        }
      }

      Ok(AppConfig::Http(HttpAppConfig {
        app_id:            app_id.to_string(),
        hostname:          self.hostname.unwrap(),
        path_begin:        path_begin,
        certificate:       certificate_opt,
        key:               key_opt,
        certificate_chain: chain_opt,
        backends:          self.backends,
        sticky_session:    sticky_session,
        https_redirect:    https_redirect,
        load_balancing_policy: self.load_balancing_policy,
      }))
    }
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpAppConfig {
  pub app_id:            String,
  pub hostname:          String,
  pub path_begin:        String,
  pub certificate:       Option<String>,
  pub key:               Option<String>,
  pub certificate_chain: Option<Vec<String>>,
  pub backends:          Vec<BackendConfig>,
  pub sticky_session:    bool,
  pub https_redirect:    bool,
  pub load_balancing_policy: LoadBalancingAlgorithms,
}

impl HttpAppConfig {
  pub fn generate_orders(&self) -> Vec<Order> {
    let mut v = Vec::new();

    v.push(Order::AddApplication(Application {
      app_id: self.app_id.clone(),
      sticky_session: self.sticky_session.clone(),
      https_redirect: self.https_redirect.clone(),
      proxy_protocol: None,
      load_balancing_policy: self.load_balancing_policy,
    }));

    //create the front both for HTTP and HTTPS if possible
    v.push(Order::AddHttpFront(HttpFront {
      app_id:     self.app_id.clone(),
      hostname:   self.hostname.clone(),
      path_begin: self.path_begin.clone(),
    }));


    if self.key.is_some() && self.certificate.is_some() {

      v.push(Order::AddCertificate(AddCertificate{
        certificate: CertificateAndKey {
          key:               self.key.clone().unwrap(),
          certificate:       self.certificate.clone().unwrap(),
          certificate_chain: self.certificate_chain.clone().unwrap_or(vec!()),
        },
        names: vec!(self.hostname.clone()),
      }));

      if let Some(f) = calculate_fingerprint(&self.certificate.as_ref().unwrap().as_bytes()[..]) {
        v.push(Order::AddHttpsFront(HttpsFront {
          app_id:         self.app_id.clone(),
          hostname:       self.hostname.clone(),
          path_begin:     self.path_begin.clone(),
          fingerprint:    CertFingerprint(f),
        }));
      } else {
        error!("cannot obtain the certificate's fingerprint");
      }
    }

    let mut backend_count = 0usize;
    for backend in self.backends.iter() {
        let ip   = format!("{}", backend.address.ip());
        let port = backend.address.port();

        let load_balancing_parameters = Some(LoadBalancingParams {
          weight: backend.weight.unwrap_or(100),
        });

        v.push(Order::AddBackend(Backend {
          app_id:     self.app_id.clone(),
          backend_id:  format!("{}-{}", self.app_id, backend_count),
          ip_address: ip,
          port:       port,
          load_balancing_parameters,
          sticky_id:  backend.sticky_id.clone(),
        }));

        backend_count += 1;
    }

    v
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
pub struct TcpAppConfig {
  pub app_id:            String,
  pub ip_address:        String,
  pub port:              u16,
  pub backends:          Vec<BackendConfig>,
  #[serde(default)]
  pub proxy_protocol:    Option<ProxyProtocolConfig>,
  pub load_balancing_policy: LoadBalancingAlgorithms,
}

impl TcpAppConfig {
  pub fn generate_orders(&self) -> Vec<Order> {
    let mut v = Vec::new();

    v.push(Order::AddApplication(Application {
      app_id: self.app_id.clone(),
      sticky_session: false,
      https_redirect: false,
      proxy_protocol: self.proxy_protocol.clone(),
      load_balancing_policy: self.load_balancing_policy,
    }));

    v.push(Order::AddTcpFront(TcpFront {
      app_id:         self.app_id.clone(),
      ip_address:     self.ip_address.clone(),
      port:           self.port,
    }));

    let mut backend_count = 0usize;
    for backend in self.backends.iter() {
      let ip   = format!("{}", backend.address.ip());
      let port = backend.address.port();

      let load_balancing_parameters = Some(LoadBalancingParams {
        weight: backend.weight.unwrap_or(100),
      });

      v.push(Order::AddBackend(Backend {
        app_id:     self.app_id.clone(),
        backend_id: format!("{}-{}", self.app_id, backend_count),
        ip_address: ip,
        port:       port,
        load_balancing_parameters,
        sticky_id:  backend.sticky_id.clone(),
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
  pub fn generate_orders(&self) -> Vec<Order> {
    match self {
      &AppConfig::Http(ref http) => http.generate_orders(),
      &AppConfig::Tcp(ref tcp)   => tcp.generate_orders(),
    }
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct FileConfig {
  pub command_socket:           String,
  pub command_buffer_size:      Option<usize>,
  pub max_command_buffer_size:  Option<usize>,
  pub max_connections:          usize,
  pub max_buffers:              usize,
  pub buffer_size:              usize,
  pub saved_state:              Option<String>,
  pub log_level:                Option<String>,
  pub log_target:               Option<String>,
  #[serde(default)]
  pub log_access_target:        Option<String>,
  pub worker_count:             Option<u16>,
  pub worker_automatic_restart: Option<bool>,
  pub metrics:                  Option<MetricsConfig>,
  pub http:                     Option<ProxyConfig>,
  pub https:                    Option<ProxyConfig>,
  pub tcp:                      Option<TcpProxyConfig>,
  pub applications:             Option<HashMap<String, FileAppConfig>>,
  pub handle_process_affinity:  Option<bool>,
  pub ctl_command_timeout:      Option<u64>,
  pub pid_file_path:            Option<String>,
}


impl FileConfig {
  pub fn load_from_path(path: &str) -> io::Result<FileConfig> {
    let data = try!(Config::load_file(path));

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
        let mut reserved_address: HashSet<(String,u16)> = HashSet::new();
        if let Some(addr) = config.http.as_ref().map(|h| (h.address.clone(), h.port)) {
          reserved_address.insert(addr);
        }

        if let Some(addr) = config.https.as_ref().map(|h| (h.address.clone(), h.port)) {
          if reserved_address.contains(&addr) {
            println!("listening address ( {}:{} ) is already used in the configuration", addr.0, addr.1);
            return Err(Error::new(
              ErrorKind::InvalidData,
              format!("listening address ( {}:{} ) is already used in the configuration", addr.0, addr.1)));
          } else {
            reserved_address.insert(addr);
          }
        }

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

        Ok(config)
      },
    }
  }

  pub fn into(self, config_path: &str) -> Config {
    let mut applications = HashMap::new();

    if let Some(mut apps) = self.applications {
      for (id, app) in apps.drain() {
        match app.to_app_config(id.as_str()) {
          Ok(app_config) => { applications.insert(id, app_config); },
          Err(s)         => {
            panic!("error parsing application configuration for {}: {}", id, s);
          },
        }
      }
    }

    Config {
      config_path:    config_path.to_string(),
      command_socket: self.command_socket,
      command_buffer_size: self.command_buffer_size.unwrap_or(1_000_000),
      max_command_buffer_size: self.max_command_buffer_size.unwrap_or( self.command_buffer_size.unwrap_or(1_000_000) * 2),
      max_connections: self.max_connections,
      max_buffers: self.max_buffers,
      buffer_size: self.buffer_size,
      saved_state: self.saved_state,
      log_level: self.log_level.unwrap_or(String::from("info")),
      log_target: self.log_target.unwrap_or(String::from("stdout")),
      log_access_target: self.log_access_target,
      worker_count: self.worker_count.unwrap_or(2),
      worker_automatic_restart: self.worker_automatic_restart.unwrap_or(true),
      metrics: self.metrics,
      http: self.http,
      https: self.https,
      tcp: self.tcp,
      applications: applications,
      handle_process_affinity: self.handle_process_affinity.unwrap_or(false),
      ctl_command_timeout: self.ctl_command_timeout.unwrap_or(1_000),
      pid_file_path: self.pid_file_path
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
  pub max_buffers:              usize,
  pub buffer_size:              usize,
  pub saved_state:              Option<String>,
  pub log_level:                String,
  pub log_target:               String,
  #[serde(default)]
  pub log_access_target:        Option<String>,
  pub worker_count:             u16,
  pub worker_automatic_restart: bool,
  pub metrics:                  Option<MetricsConfig>,
  pub http:                     Option<ProxyConfig>,
  pub https:                    Option<ProxyConfig>,
  pub tcp:                      Option<TcpProxyConfig>,
  pub applications:             HashMap<String, AppConfig>,
  pub handle_process_affinity:  bool,
  pub ctl_command_timeout:      u64,
  pub pid_file_path:            Option<String>,
}

impl Config {
  pub fn load_from_path(path: &str) -> io::Result<Config> {
    FileConfig::load_from_path(path).map(|config| config.into(path))
  }

  pub fn generate_config_messages(&self) -> Vec<ConfigMessage> {
    let mut v = Vec::new();
    let mut count = 0u8;

    for ref app in self.applications.values() {
      let mut orders = app.generate_orders();
      for order in orders.drain(..) {
        v.push(ConfigMessage {
          id:       format!("CONFIG-{}", count),
          version:  PROTOCOL_VERSION,
          proxy_id: None,
          data:     ConfigCommand::ProxyConfiguration(order),
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
    let mut f = try!(File::open(path));
    let mut data = String::new();
    try!(f.read_to_string(&mut data));
    Ok(data)
  }

  pub fn load_file_bytes(path: &str) -> io::Result<Vec<u8>> {
    let mut f = try!(File::open(path));
    let mut data = Vec::new();
    try!(f.read_to_end(&mut data));
    Ok(data)
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
    let http = ProxyConfig {
      address: String::from("127.0.0.1"),
      port: 8080,
      answer_404: Some(String::from("404.html")),
      answer_503: None,
      public_address: None,
      tls_versions: None,
      cipher_list: None,
      rustls_cipher_list: None,
      default_app_id: None,
      default_certificate: None,
      default_certificate_chain: None,
      default_key: None,
      default_name: None,
      tls_provider: None,
      expect_proxy: None,
      sticky_name: "SOZUBALANCEID".to_string(),
    };
    println!("http: {:?}", to_string(&http));
    let https = ProxyConfig {
      address: String::from("127.0.0.1"),
      port: 8080,
      answer_404: Some(String::from("404.html")),
      answer_503: None,
      public_address: None,
      tls_versions: None,
      cipher_list: None,
      rustls_cipher_list: None,
      default_app_id: None,
      default_certificate: None,
      default_certificate_chain: None,
      default_key: None,
      default_name: None,
      tls_provider: None,
      expect_proxy: None,
      sticky_name: "SOZUBALANCEID".to_string(),
    };
    println!("https: {:?}", to_string(&https));
    let config = FileConfig {
      command_socket: String::from("./command_folder/sock"),
      saved_state: None,
      worker_count: Some(2),
      worker_automatic_restart: Some(true),
      handle_process_affinity: None,
      command_buffer_size: None,
      max_connections: 500,
      max_buffers: 500,
      buffer_size: 16384,
      max_command_buffer_size: None,
      log_level:  None,
      log_target: None,
      log_access_target: None,
      metrics: Some(MetricsConfig {
        address: String::from("192.168.59.103"),
        port:    8125,
        tagged_metrics: false,
        prefix: Some(String::from("sozu-metrics")),
      }),
      http:  Some(http),
      https: Some(https),
      tcp:   None,
      applications: None,
      ctl_command_timeout: None,
      pid_file_path: None
    };

    println!("config: {:?}", to_string(&config));
    let encoded = to_string(&config).unwrap();
    println!("conf:\n{}", encoded);
  }
}
