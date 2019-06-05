#![allow(unused_variables,unused_must_use)]
#[macro_use] extern crate sozu_lib as sozu;
#[macro_use] extern crate sozu_command_lib as sozu_command;
extern crate time;

use std::thread;
use std::env;
use std::io::stdout;
use sozu_command::logging::{Logger,LoggerBackend};
use sozu_command::proxy;
use sozu_command::proxy::{LoadBalancingParams,PathRule, RulePosition};
use sozu_command::channel::Channel;

fn main() {
  if env::var("RUST_LOG").is_ok() {
   Logger::init("EXAMPLE".to_string(), &env::var("RUST_LOG").expect("could not get the RUST_LOG env var"), LoggerBackend::Stdout(stdout()), None);
  } else {
   Logger::init("EXAMPLE".to_string(), "info", LoggerBackend::Stdout(stdout()), None);
  }

  info!("MAIN\tstarting up");

  sozu::metrics::setup(&"127.0.0.1:8125".parse().unwrap(), "main", false, None);
  gauge!("sozu.TEST", 42);

  let config = proxy::HttpListener {
    front: "127.0.0.1:8080".parse().expect("could not parse address"),
    ..Default::default()
  };

  let (mut command, channel) = Channel::generate(1000, 10000).expect("should create a channel");
  let jg = thread::spawn(move || {
    let max_buffers = 500;
    let buffer_size = 16384;
    sozu::http::start(config, channel, max_buffers, buffer_size);
  });

  let http_front = proxy::HttpFront {
    app_id:   String::from("app_1"),
    address:  "127.0.0.1:8080".parse().unwrap(),
    hostname: String::from("lolcatho.st"),
    path:     PathRule::Prefix(String::from("/")),
    position: RulePosition::Tree,
  };

  let http_backend = proxy::Backend {
    app_id:      String::from("app_1"),
    backend_id:  String::from("app_1-0"),
    sticky_id:   None,
    address:     "127.0.0.1:1026".parse().unwrap(),
    load_balancing_parameters: Some(LoadBalancingParams::default()),
    backup:      None,
  };

  command.write_message(&proxy::ProxyRequest {
    id:    String::from("ID_ABCD"),
    order: proxy::ProxyRequestData::AddHttpFront(http_front)
  });

  command.write_message(&proxy::ProxyRequest {
    id:    String::from("ID_EFGH"),
    order: proxy::ProxyRequestData::AddBackend(http_backend)
  });

  info!("MAIN\tHTTP -> {:?}", command.read_message());
  info!("MAIN\tHTTP -> {:?}", command.read_message());


  let config = proxy::HttpsListener {
    front: "127.0.0.1:8443".parse().expect("could not parse address"),
    cipher_list: String::from("ECDHE-ECDSA-CHACHA20-POLY1305:\
    ECDHE-RSA-CHACHA20-POLY1305:ECDHE-ECDSA-AES128-GCM-SHA256:\
    ECDHE-RSA-AES128-GCM-SHA256:ECDHE-ECDSA-AES256-GCM-SHA384:\
    ECDHE-RSA-AES256-GCM-SHA384:DHE-RSA-AES128-GCM-SHA256:\
    DHE-RSA-AES256-GCM-SHA384:ECDHE-ECDSA-AES128-SHA256:\
    ECDHE-RSA-AES128-SHA256:ECDHE-ECDSA-AES128-SHA:\
    ECDHE-RSA-AES256-SHA384:ECDHE-RSA-AES128-SHA:\
    ECDHE-ECDSA-AES256-SHA384:ECDHE-ECDSA-AES256-SHA:\
    ECDHE-RSA-AES256-SHA:DHE-RSA-AES128-SHA256:DHE-RSA-AES128-SHA:\
    DHE-RSA-AES256-SHA256:DHE-RSA-AES256-SHA:ECDHE-ECDSA-DES-CBC3-SHA:\
    ECDHE-RSA-DES-CBC3-SHA:EDH-RSA-DES-CBC3-SHA:AES128-GCM-SHA256:\
    AES256-GCM-SHA384:AES128-SHA256:AES256-SHA256:AES128-SHA:\
    AES256-SHA:DES-CBC3-SHA:!DSS"),

    ..Default::default()
  };

  let (mut command2, channel2) = Channel::generate(1000, 10000).expect("should create a channel");
  let jg2 = thread::spawn(move || {
    let max_buffers = 500;
    let buffer_size = 16384;
    sozu::https_rustls::configuration::start(config, channel2, max_buffers, buffer_size);
  });

  let cert1 = include_str!("../assets/certificate.pem");
  let key1  = include_str!("../assets/key.pem");

  let certificate_and_key = proxy::CertificateAndKey {
    certificate:       String::from(cert1),
    key:               String::from(key1),
    certificate_chain: vec!()
  };
  command2.write_message(&proxy::ProxyRequest {
    id:    String::from("ID_IJKL1"),
    order: proxy::ProxyRequestData::AddCertificate(proxy::AddCertificate{
      front: "127.0.0.1:8443".parse().unwrap(),
      certificate: certificate_and_key,
      names: Vec::new(),
    })
  });

  let tls_front = proxy::HttpFront {
    app_id:   String::from("app_1"),
    address:  "127.0.0.1:8443".parse().unwrap(),
    hostname: String::from("lolcatho.st"),
    path:     PathRule::Prefix(String::from("/")),
    position: RulePosition::Tree,
  };

  command2.write_message(&proxy::ProxyRequest {
    id:    String::from("ID_IJKL2"),
    order: proxy::ProxyRequestData::AddHttpsFront(tls_front)
  });
  let tls_backend = proxy::Backend {
    app_id:      String::from("app_1"),
    backend_id:  String::from("app_1-0"),
    sticky_id:   None,
    address:     "127.0.0.1:1026".parse().unwrap(),
    load_balancing_parameters: Some(LoadBalancingParams::default()),
    backup:      None,
  };

  command2.write_message(&proxy::ProxyRequest {
    id:    String::from("ID_MNOP"),
    order: proxy::ProxyRequestData::AddBackend(tls_backend)
  });

  let cert2 = include_str!("../assets/cert_test.pem");
  let key2  = include_str!("../assets/key_test.pem");

  let certificate_and_key2 = proxy::CertificateAndKey {
    certificate: String::from(cert2),
    key: String::from(key2),
    certificate_chain: vec!()
  };

  command2.write_message(&proxy::ProxyRequest {
    id:    String::from("ID_QRST1"),
    order: proxy::ProxyRequestData::AddCertificate(proxy::AddCertificate {
      front: "127.0.0.1:8443".parse().unwrap(),
      certificate: certificate_and_key2,
      names: Vec::new(),
    })
  });

  let tls_front2 = proxy::HttpFront {
    app_id:   String::from("app_2"),
    address:  "127.0.0.1:8443".parse().unwrap(),
    hostname: String::from("test.local"),
    path:     PathRule::Prefix(String::from("/")),
    position: RulePosition::Tree,
  };

  command2.write_message(&proxy::ProxyRequest {
    id:    String::from("ID_QRST2"),
    order: proxy::ProxyRequestData::AddHttpsFront(tls_front2)
  });

  let tls_backend2 = proxy::Backend {
    app_id:      String::from("app_2"),
    backend_id:  String::from("app_2-0"),
    sticky_id:   None,
    address:     "127.0.0.1:1026".parse().unwrap(),
    load_balancing_parameters: Some(LoadBalancingParams::default()),
    backup:      None,
  };

  command2.write_message(&proxy::ProxyRequest {
    id:    String::from("ID_UVWX"),
    order: proxy::ProxyRequestData::AddBackend(tls_backend2)
  });

  info!("MAIN\tTLS -> {:?}", command2.read_message());
  info!("MAIN\tTLS -> {:?}", command2.read_message());
  info!("MAIN\tTLS -> {:?}", command2.read_message());
  info!("MAIN\tTLS -> {:?}", command2.read_message());

  let _ = jg.join();
  info!("MAIN\tgood bye");
}

