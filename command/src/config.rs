//! parsing data from the configuration file
use std::fs::File;
use std::path::PathBuf;
use std::str::FromStr;
use std::net::ToSocketAddrs;
use std::collections::HashMap;
use std::io::{self,Error,ErrorKind,Read};
use certificate::{calculate_fingerprint,split_certificate_chain};
use toml;

use messages::Application;
use messages::{CertFingerprint,CertificateAndKey,Order,HttpFront,HttpsFront,TcpFront,Instance,
  HttpProxyConfiguration,HttpsProxyConfiguration,AddCertificate};

use data::{ConfigCommand,ConfigMessage,PROTOCOL_VERSION};

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
pub struct ProxyConfig {
  pub address:                   String,
  pub public_address:            Option<String>,
  pub port:                      u16,
  pub answer_404:                Option<String>,
  pub answer_503:                Option<String>,
  pub cipher_list:               Option<String>,
  pub default_name:              Option<String>,
  pub default_app_id:            Option<String>,
  pub default_certificate:       Option<String>,
  pub default_certificate_chain: Option<String>,
  pub default_key:               Option<String>,
  pub tls_versions:              Option<Vec<String>>,
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
        front: addr,
        public_address: public_address,
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

    let tls_proxy_configuration = match address.parse() {
      Ok(addr) => Some(addr),
      Err(err) => {
        error!("Couldn't parse address of TLS proxy: {}", err);
        None
      }
    };

    let versions = match self.tls_versions {
      None    => vec!(String::from("TLSv1.2")),
      Some(ref v) => {
        for version in v.iter() {
          match version.as_str() {
            "SSLv2" | "SSLv3" | "TLSv1" | "TLSv1.1" | "TLSv1.2" => (),
            s => error!("unrecognized TLS version: {}", s)
          };
        }

        v.clone()
      }
    };

    tls_proxy_configuration.map(|addr| {
      let mut configuration = HttpsProxyConfiguration {
        front:           addr,
        public_address:  public_address,
        cipher_list:     cipher_list,
        versions:        versions,
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

      configuration

    })
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
pub struct TcpProxyConfig {
  pub max_listeners : usize,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
pub struct MetricsConfig {
  pub address: String,
  pub port:    u16,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
pub struct FileAppConfig {
  pub ip_address:        Option<String>,
  pub port:              Option<u16>,
  pub hostname:          Option<String>,
  pub path_begin:        Option<String>,
  pub certificate:       Option<String>,
  pub key:               Option<String>,
  pub certificate_chain: Option<String>,
  pub tls_names:         Option<Vec<String>>,
  pub backends:          Vec<String>,
  pub sticky_session:    Option<bool>,
  pub https_redirect:    Option<bool>,
}

impl FileAppConfig {
  pub fn to_app_config(self, app_id: &str) -> Result<AppConfig, String> {
    if self.hostname.is_none() && self.path_begin.is_none() && self.certificate.is_none() &&
      self.key.is_none() && self.certificate_chain.is_none() && self.sticky_session.is_none() {
      match (self.ip_address, self.port) {
        (Some(ip), Some(port)) => {
          Ok(AppConfig::Tcp(TcpAppConfig {
            app_id:     app_id.to_string(),
            ip_address: ip,
            port:       port,
            backends:   self.backends,
          }))
        },
        (None, Some(_)) => Err(String::from("missing IP address for TCP application")),
        (Some(_), None) => Err(String::from("missing port for TCP application")),
        (None, None) => Err(String::from("missing IP address port for TCP application")),
      }
    } else {
      if self.hostname.is_none() {
        return Err(String::from("missing hostname for TCP application"));
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
        tls_names:         self.tls_names,
        backends:          self.backends,
        sticky_session:    sticky_session,
        https_redirect:    https_redirect,
      }))
    }
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
pub struct HttpAppConfig {
  pub app_id:            String,
  pub hostname:          String,
  pub path_begin:        String,
  pub certificate:       Option<String>,
  pub key:               Option<String>,
  pub certificate_chain: Option<Vec<String>>,
  pub tls_names:         Option<Vec<String>>,
  pub backends:          Vec<String>,
  pub sticky_session:    bool,
  pub https_redirect:    bool,
}

impl HttpAppConfig {
  pub fn generate_orders(&self) -> Vec<Order> {
    let mut v = Vec::new();

    v.push(Order::AddApplication(Application {
      app_id: self.app_id.clone(),
      sticky_session: self.sticky_session.clone(),
      https_redirect: self.https_redirect.clone(),
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
        names: self.tls_names.clone().unwrap_or(vec!()),
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
    for address_str in self.backends.iter() {
      if let Ok(address_list) = address_str.to_socket_addrs() {
        for address in address_list {
          let ip   = format!("{}", address.ip());
          let port = address.port();

          v.push(Order::AddInstance(Instance {
            app_id:     self.app_id.clone(),
            instance_id: format!("{}-{}", self.app_id, backend_count),
            ip_address: ip,
            port:       port
          }));

          backend_count += 1;
        }
      } else {
        error!("could not parse address: {}", address_str);
      }
    }

    v
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
pub struct TcpAppConfig {
  pub app_id:            String,
  pub ip_address:        String,
  pub port:              u16,
  pub backends:          Vec<String>,
}

impl TcpAppConfig {
  pub fn generate_orders(&self) -> Vec<Order> {
    let mut v = Vec::new();

    v.push(Order::AddApplication(Application {
      app_id: self.app_id.clone(),
      sticky_session: false,
      https_redirect: false,
    }));

    v.push(Order::AddTcpFront(TcpFront {
      app_id:     self.app_id.clone(),
      ip_address: self.ip_address.clone(),
      port:       self.port,
    }));

    let mut backend_count = 0usize;
    for address_str in self.backends.iter() {
      if let Ok(address_list) = address_str.to_socket_addrs() {
        for address in address_list {
          let ip   = format!("{}", address.ip());
          let port = address.port();

          v.push(Order::AddInstance(Instance {
            app_id:     self.app_id.clone(),
            instance_id: format!("{}-{}", self.app_id, backend_count),
            ip_address: ip,
            port:       port
          }));

          backend_count += 1;
        }
      } else {
        error!("could not parse address: {}", address_str);
      }
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
  pub command_socket:          String,
  pub command_buffer_size:     Option<usize>,
  pub max_command_buffer_size: Option<usize>,
  pub channel_buffer_size:     Option<usize>,
  pub max_connections:         usize,
  pub max_buffers:             usize,
  pub buffer_size:             usize,
  pub saved_state:             Option<String>,
  pub log_level:               Option<String>,
  pub log_target:              Option<String>,
  pub worker_count:            Option<u16>,
  pub metrics:                 Option<MetricsConfig>,
  pub http:                    Option<ProxyConfig>,
  pub https:                   Option<ProxyConfig>,
  pub tcp:                     Option<TcpProxyConfig>,
  pub applications:            Option<HashMap<String, FileAppConfig>>,
  pub handle_process_affinity: Option<bool>,
  pub ctl_commands_timeout:    Option<u64>
}


impl FileConfig {
  pub fn load_from_path(path: &str) -> io::Result<FileConfig> {
    let data = try!(Config::load_file(path));

    match toml::from_str(&data) {
      Ok(config) => Ok(config),
      Err(e)     => {
        println!("decoding error: {:?}", e);
        Err(Error::new(
          ErrorKind::InvalidData,
          format!("decoding error: {:?}", e))
        )
      }
    }
  }

  pub fn into(self, config_path: &str) -> Config {
    let mut applications = HashMap::new();

    if let Some(mut apps) = self.applications {
      for (id, app) in apps.drain() {
        match app.to_app_config(id.as_str()) {
          Ok(app_config) => { applications.insert(id, app_config); },
          Err(s)         => {
            println!("error parsing application configuration for {}: {}", id, s);
          },
        }
      }
    }

    Config {
      config_path:    config_path.to_string(),
      command_socket: self.command_socket,
      command_buffer_size: self.command_buffer_size.unwrap_or(1_000_000),
      max_command_buffer_size: self.max_command_buffer_size.unwrap_or( self.command_buffer_size.unwrap_or(1_000_000) * 2),
      channel_buffer_size: self.channel_buffer_size.unwrap_or(1_000_000),
      max_connections: self.max_connections,
      max_buffers: self.max_buffers,
      buffer_size: self.buffer_size,
      saved_state: self.saved_state,
      log_level: self.log_level.unwrap_or(String::from("info")),
      log_target: self.log_target.unwrap_or(String::from("stdout")),
      worker_count: self.worker_count.unwrap_or(2),
      metrics: self.metrics,
      http: self.http,
      https: self.https,
      tcp: self.tcp,
      applications: applications,
      handle_process_affinity: self.handle_process_affinity.unwrap_or(false),
      ctl_commands_timeout: self.ctl_commands_timeout.unwrap_or(1_000)
    }
  }
}

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct Config {
  pub config_path:             String,
  pub command_socket:          String,
  pub command_buffer_size:     usize,
  pub max_command_buffer_size: usize,
  pub channel_buffer_size:     usize,
  pub max_connections:         usize,
  pub max_buffers:             usize,
  pub buffer_size:             usize,
  pub saved_state:             Option<String>,
  pub log_level:               String,
  pub log_target:              String,
  pub worker_count:            u16,
  pub metrics:                 Option<MetricsConfig>,
  pub http:                    Option<ProxyConfig>,
  pub https:                   Option<ProxyConfig>,
  pub tcp:                     Option<TcpProxyConfig>,
  pub applications:            HashMap<String, AppConfig>,
  pub handle_process_affinity: bool,
  pub ctl_commands_timeout:    u64,
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
      default_app_id: None,
      default_certificate: None,
      default_certificate_chain: None,
      default_key: None,
      default_name: None,
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
      default_app_id: None,
      default_certificate: None,
      default_certificate_chain: None,
      default_key: None,
      default_name: None,
    };
    println!("https: {:?}", to_string(&https));
    let config = FileConfig {
      command_socket: String::from("./command_folder/sock"),
      saved_state: None,
      worker_count: Some(2),
      handle_process_affinity: None,
      channel_buffer_size: Some(10000),
      command_buffer_size: None,
      max_connections: 500,
      max_buffers: 500,
      buffer_size: 16384,
      max_command_buffer_size: None,
      log_level:  None,
      log_target: None,
      metrics: Some(MetricsConfig {
        address: String::from("192.168.59.103"),
        port:    8125,
      }),
      http:  Some(http),
      https: Some(https),
      tcp:   None,
      applications: None,
      ctl_commands_timeout: None
    };

    println!("config: {:?}", to_string(&config));
    let encoded = to_string(&config).unwrap();
    println!("conf:\n{}", encoded);
  }
}
