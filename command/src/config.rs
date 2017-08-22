//! parsing data from the configuration file
use std::fs::File;
use std::str::FromStr;
use std::net::ToSocketAddrs;
use std::collections::HashMap;
use std::io::{self,Error,ErrorKind,Read};
use certificate::{calculate_fingerprint,split_certificate_chain};
use openssl::ssl;
use toml;

use messages::{CertFingerprint,CertificateAndKey,Order,HttpFront,HttpsFront,Instance,HttpProxyConfiguration,HttpsProxyConfiguration};

use data::{ConfigCommand,ConfigMessage,PROTOCOL_VERSION};

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
pub struct ProxyConfig {
  pub address:                   String,
  pub public_address:            Option<String>,
  pub port:                      u16,
  pub max_connections:           usize,
  pub buffer_size:               usize,
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
        max_connections: self.max_connections,
        buffer_size: self.buffer_size,
        ..Default::default()
      };

      //FIXME: error messages if file not found?
      let mut answer_404 = String::new();
      if self.answer_404.as_ref().and_then(|path| File::open(path).ok())
        .and_then(|mut file| file.read_to_string(&mut answer_404).ok()).is_some() {
        configuration.answer_404 = answer_404;
      }
      let mut answer_503 = String::new();
      if self.answer_503.as_ref().and_then(|path| File::open(path).ok())
        .and_then(|mut file| file.read_to_string(&mut answer_503).ok()).is_some() {
        configuration.answer_503 = answer_503;
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

    let mut default_options = ssl::SSL_OP_CIPHER_SERVER_PREFERENCE | ssl::SSL_OP_NO_COMPRESSION | ssl::SSL_OP_NO_TICKET;
    let mut versions = ssl::SSL_OP_NO_SSLV2 |
      ssl::SSL_OP_NO_SSLV3 | ssl::SSL_OP_NO_TLSV1 |
      ssl::SSL_OP_NO_TLSV1_1 | ssl::SSL_OP_NO_TLSV1_2;

    if let Some(ref versions_list) = self.tls_versions {
      for version in versions_list {
        match version.as_str() {
          "SSLv2"   => versions.remove(ssl::SSL_OP_NO_SSLV2),
          "SSLv3"   => versions.remove(ssl::SSL_OP_NO_SSLV3),
          "TLSv1"   => versions.remove(ssl::SSL_OP_NO_TLSV1),
          "TLSv1.1" => versions.remove(ssl::SSL_OP_NO_TLSV1_1),
          "TLSv1.2" => versions.remove(ssl::SSL_OP_NO_TLSV1_2),
          s         => error!("unrecognized TLS version: {}", s)
        };
      }
    } else {
      versions = ssl::SSL_OP_NO_SSLV2 | ssl::SSL_OP_NO_SSLV3 | ssl::SSL_OP_NO_TLSV1;
    }

    default_options.insert(versions);
    trace!("parsed tls options: {:?}", default_options);

    tls_proxy_configuration.map(|addr| {
      let mut configuration = HttpsProxyConfiguration {
        front:           addr,
        public_address:  public_address,
        max_connections: self.max_connections,
        buffer_size:     self.buffer_size,
        cipher_list:     cipher_list,
        options:         default_options.bits(),
        ..Default::default()
      };

      //FIXME: error messages if file not found?
      let mut answer_404 = String::new();
      if self.answer_404.as_ref().and_then(|path| File::open(path).ok())
        .and_then(|mut file| file.read_to_string(&mut answer_404).ok()).is_some() {
        configuration.answer_404 = answer_404;
      } else {
        error!("error loading default 404 answer file: {:?}", self.answer_404);
      }
      let mut answer_503 = String::new();
      if self.answer_503.as_ref().and_then(|path| File::open(path).ok())
        .and_then(|mut file| file.read_to_string(&mut answer_503).ok()).is_some() {
        configuration.answer_503 = answer_503;
      } else {
        error!("error loading default 503 answer file: {:?}", self.answer_503);
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
pub struct MetricsConfig {
  pub address: String,
  pub port:    u16,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
pub struct AppConfig {
  pub hostname:          String,
  pub path_begin:        Option<String>,
  pub certificate:       Option<String>,
  pub key:               Option<String>,
  pub certificate_chain: Option<String>,
  pub backends:          Vec<String>,
  pub sticky_session:    Option<bool>,
}

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct Config {
  pub command_socket:          String,
  pub command_buffer_size:     Option<usize>,
  pub max_command_buffer_size: Option<usize>,
  pub channel_buffer_size:     Option<usize>,
  pub saved_state:             Option<String>,
  pub log_level:               Option<String>,
  pub log_target:              Option<String>,
  pub worker_count:            Option<u16>,
  pub metrics:                 Option<MetricsConfig>,
  pub http:                    Option<ProxyConfig>,
  pub https:                   Option<ProxyConfig>,
  pub applications:            HashMap<String, AppConfig>,
  pub handle_process_affinity: Option<bool>
}

impl Config {
  pub fn load_from_path(path: &str) -> io::Result<Config> {
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

  pub fn generate_config_messages(&self) -> Vec<ConfigMessage> {
    let mut v = Vec::new();
    let mut count = 0u8;
    for (ref id, ref app) in &self.applications {

      let path_begin = app.path_begin.as_ref().unwrap_or(&String::new()).clone();

      //FIXME: TCP should be handled as well
      let key_opt         = app.key.as_ref().and_then(|path| Config::load_file(&path).ok());
      let certificate_opt = app.certificate.as_ref().and_then(|path| Config::load_file(&path).ok());
      let chain_opt       = app.certificate_chain.as_ref().and_then(|path| Config::load_file(&path).ok())
        .map(split_certificate_chain);

      //create the front both for HTTP and HTTPS if possible
      let order = Order::AddHttpFront(HttpFront {
        app_id:         id.to_string(),
        hostname:       app.hostname.clone(),
        path_begin:     path_begin.clone(),
        sticky_session: app.sticky_session.unwrap_or(false),
      });
      v.push(ConfigMessage {
        id:       format!("CONFIG-{}", count),
        version:  PROTOCOL_VERSION,
        proxy_id: None,
        data:     ConfigCommand::ProxyConfiguration(order),
      });
      count += 1;

      if key_opt.is_some() || certificate_opt.is_some()  || chain_opt.is_some() {

        if key_opt.is_none() {
          error!("cannot read the key at {:?}", app.key);
          continue;
        }
        if certificate_opt.is_none() {
          error!("cannot read the certificate at {:?}", app.certificate);
          continue;
        }
        if chain_opt.is_none() {
          error!("cannot read the certificate chain at {:?}", app.certificate_chain);
          continue;
        }
        let certificate = certificate_opt.unwrap();
        let fingerprint = match calculate_fingerprint(&certificate.as_bytes()[..]) {
          Ok(f)  => CertFingerprint(f),
          Err(e) => {
            error!("cannot obtain the certificate's fingerprint: {:?}", e);
            continue;
          }
        };

        let certificate_order = Order::AddCertificate(CertificateAndKey {
          key:               key_opt.unwrap(),
          certificate:       certificate,
          certificate_chain: chain_opt.unwrap(),
        });
        v.push(ConfigMessage {
          id:       format!("CONFIG-{}", count),
          version:  PROTOCOL_VERSION,
          proxy_id: None,
          data:     ConfigCommand::ProxyConfiguration(certificate_order),
        });
        count += 1;
        let front_order = Order::AddHttpsFront(HttpsFront {
          app_id:         id.to_string(),
          hostname:       app.hostname.clone(),
          path_begin:     path_begin,
          fingerprint:    fingerprint,
          sticky_session: app.sticky_session.unwrap_or(false),
        });
        v.push(ConfigMessage {
          id:       format!("CONFIG-{}", count),
          version:  PROTOCOL_VERSION,
          proxy_id: None,
          data:     ConfigCommand::ProxyConfiguration(front_order),
        });
        count += 1;
      }

      for address_str in app.backends.iter() {
        if let Ok(address_list) = address_str.to_socket_addrs() {
          for address in address_list {
            let ip   = format!("{}", address.ip());
            let port = address.port();

            let backend_order = Order::AddInstance(Instance {
              app_id:     id.to_string(),
              ip_address: ip,
              port:       port
            });

            v.push(ConfigMessage {
              id:       format!("CONFIG-{}", count),
              version:  PROTOCOL_VERSION,
              proxy_id: None,
              data:     ConfigCommand::ProxyConfiguration(backend_order),
            });

            count += 1;
          }
        } else {
          error!("could not parse address: {}", address_str);
        }
      }
    }

    v
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
  use std::collections::HashMap;
  use toml::to_string;

  #[test]
  fn serialize() {
    let http = ProxyConfig {
      address: String::from("127.0.0.1"),
      port: 8080,
      max_connections: 500,
      buffer_size: 16384,
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
      max_connections: 500,
      buffer_size: 16384,
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
    let config = Config {
      command_socket: String::from("./command_folder/sock"),
      saved_state: None,
      worker_count: Some(2),
      handle_process_affinity: None,
      channel_buffer_size: Some(10000),
      command_buffer_size: None,
      max_command_buffer_size: None,
      log_level:  None,
      log_target: None,
      metrics: Some(MetricsConfig {
        address: String::from("192.168.59.103"),
        port:    8125,
      }),
      http:  Some(http),
      https: Some(https),
      applications: HashMap::new(),
    };

    println!("config: {:?}", to_string(&config));
    let encoded = to_string(&config).unwrap();
    println!("conf:\n{}", encoded);
  }
}
