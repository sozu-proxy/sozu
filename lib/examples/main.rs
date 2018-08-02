#![allow(unused_variables,unused_must_use)]
#[macro_use] extern crate sozu_lib as sozu;
#[macro_use] extern crate sozu_command_lib as sozu_command;
extern crate time;
extern crate hex;

use std::thread;
use std::env;
use std::io::stdout;
use sozu::network;
use sozu_command::logging::{Logger,LoggerBackend};
use sozu_command::messages;
use sozu_command::messages::LoadBalancingParams;
use sozu_command::channel::Channel;

fn main() {
  if env::var("RUST_LOG").is_ok() {
   Logger::init("EXAMPLE".to_string(), &env::var("RUST_LOG").expect("could not get the RUST_LOG env var"), LoggerBackend::Stdout(stdout()), None);
  } else {
   Logger::init("EXAMPLE".to_string(), "info", LoggerBackend::Stdout(stdout()), None);
  }

  info!("MAIN\tstarting up");

  network::metrics::setup(&"127.0.0.1:8125".parse().unwrap(), "main", false, None);
  gauge!("sozu.TEST", 42);

  let config = messages::HttpListener {
    front: "127.0.0.1:8080".parse().expect("could not parse address"),
    ..Default::default()
  };

  let (mut command, channel) = Channel::generate(1000, 10000).expect("should create a channel");
  let jg = thread::spawn(move || {
    let max_buffers = 500;
    let buffer_size = 16384;
    network::http::start(config, channel, max_buffers, buffer_size);
  });

  let http_front = messages::HttpFront {
    app_id:     String::from("app_1"),
    address:    "127.0.0.1:8080".parse().unwrap(),
    hostname:   String::from("lolcatho.st"),
    path_begin: String::from("/")
  };

  let http_backend = messages::Backend {
    app_id:      String::from("app_1"),
    backend_id:  String::from("app_1-0"),
    sticky_id:   None,
    address:     "127.0.0.1:1026".parse().unwrap(),
    load_balancing_parameters: Some(LoadBalancingParams::default()),
    backup:      None,
  };

  command.write_message(&messages::OrderMessage {
    id:    String::from("ID_ABCD"),
    order: messages::Order::AddHttpFront(http_front)
  });

  command.write_message(&messages::OrderMessage {
    id:    String::from("ID_EFGH"),
    order: messages::Order::AddBackend(http_backend)
  });

  info!("MAIN\tHTTP -> {:?}", command.read_message());
  info!("MAIN\tHTTP -> {:?}", command.read_message());


  let config = messages::HttpsListener {
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
    network::https_rustls::configuration::start(config, channel2, max_buffers, buffer_size);
  });

  let cert1 = include_str!("../assets/certificate.pem");
  let key1  = include_str!("../assets/key.pem");

  let certificate_and_key = messages::CertificateAndKey {
    certificate:       String::from(cert1),
    key:               String::from(key1),
    certificate_chain: vec!()
  };
  command2.write_message(&messages::OrderMessage {
    id:    String::from("ID_IJKL1"),
    order: messages::Order::AddCertificate(messages::AddCertificate{
      front: "127.0.0.1:8443".parse().unwrap(),
      certificate: certificate_and_key,
      names: Vec::new(),
    })
  });

  let tls_front = messages::HttpsFront {
    app_id:      String::from("app_1"),
    address:     "127.0.0.1:8443".parse().unwrap(),
    hostname:    String::from("lolcatho.st"),
    path_begin:  String::from("/"),
    fingerprint: messages::CertFingerprint(hex::FromHex::from_hex("AB2618B674E15243FD02A5618C66509E4840BA60E7D64CEBEC84CDBFECEEE0C5").unwrap()),
  };

  command2.write_message(&messages::OrderMessage {
    id:    String::from("ID_IJKL2"),
    order: messages::Order::AddHttpsFront(tls_front)
  });
  let tls_backend = messages::Backend {
    app_id:      String::from("app_1"),
    backend_id:  String::from("app_1-0"),
    sticky_id:   None,
    address:     "127.0.0.1:1026".parse().unwrap(),
    load_balancing_parameters: Some(LoadBalancingParams::default()),
    backup:      None,
  };

  command2.write_message(&messages::OrderMessage {
    id:    String::from("ID_MNOP"),
    order: messages::Order::AddBackend(tls_backend)
  });

  let cert2 = include_str!("../assets/cert_test.pem");
  let key2  = include_str!("../assets/key_test.pem");

  let certificate_and_key2 = messages::CertificateAndKey {
    certificate: String::from(cert2),
    key: String::from(key2),
    certificate_chain: vec!()
  };

  command2.write_message(&messages::OrderMessage {
    id:    String::from("ID_QRST1"),
    order: messages::Order::AddCertificate(messages::AddCertificate {
      front: "127.0.0.1:8443".parse().unwrap(),
      certificate: certificate_and_key2,
      names: Vec::new(),
    })
  });

  let tls_front2 = messages::HttpsFront {
    app_id:      String::from("app_2"),
    address:     "127.0.0.1:8443".parse().unwrap(),
    hostname:    String::from("test.local"),
    path_begin:  String::from("/"),
    fingerprint: messages::CertFingerprint(hex::FromHex::from_hex("7E8EBF9AD0645AB755A2E51EB3734B91D4ACACEF1F28AD9D96D9385487FAE6E6").unwrap()),
  };

  command2.write_message(&messages::OrderMessage {
    id:    String::from("ID_QRST2"),
    order: messages::Order::AddHttpsFront(tls_front2)
  });

  let tls_backend2 = messages::Backend {
    app_id:      String::from("app_2"),
    backend_id:  String::from("app_2-0"),
    sticky_id:   None,
    address:     "127.0.0.1:1026".parse().unwrap(),
    load_balancing_parameters: Some(LoadBalancingParams::default()),
    backup:      None,
  };

  command2.write_message(&messages::OrderMessage {
    id:    String::from("ID_UVWX"),
    order: messages::Order::AddBackend(tls_backend2)
  });

  info!("MAIN\tTLS -> {:?}", command2.read_message());
  info!("MAIN\tTLS -> {:?}", command2.read_message());
  info!("MAIN\tTLS -> {:?}", command2.read_message());
  info!("MAIN\tTLS -> {:?}", command2.read_message());

  let _ = jg.join();
  info!("MAIN\tgood bye");
}

