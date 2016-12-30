#![allow(unused_variables,unused_must_use)]
#[macro_use] extern crate log;
extern crate env_logger;
extern crate sozu_lib as sozu;
extern crate openssl;
extern crate time;
extern crate libc;
extern crate mio;
extern crate mio_uds;

use std::net::{UdpSocket,ToSocketAddrs};
use std::sync::mpsc::{channel};
use std::thread;
use std::env;
use mio_uds::UnixStream;
use sozu::network;
use sozu::messages;
use sozu::network::{ProxyOrder,ServerMessage};
use sozu::network::metrics::{METRICS,ProxyMetrics};
use sozu::network::proxy::Channel;
use sozu::command::CommandChannel;
use openssl::ssl;
use log::{LogRecord,LogLevelFilter,LogLevel};
use env_logger::LogBuilder;
use mio::channel;

fn main() {
  //env_logger::init().unwrap();
  let pid = unsafe { libc::getpid() };
  let format = move |record: &LogRecord| {
    match record.level() {
    LogLevel::Debug | LogLevel::Trace => format!("{}\t{}\t{}\t{}\t{}\t|\t{}",
      time::now_utc().rfc3339(), time::precise_time_ns(), pid,
      record.level(), record.args(), record.location().module_path()),
    _ => format!("{}\t{}\t{}\t{}\t{}",
      time::now_utc().rfc3339(), time::precise_time_ns(), pid,
      record.level(), record.args())

    }
  };

  let mut builder = LogBuilder::new();
  builder.format(format).filter(None, LogLevelFilter::Info);

  if env::var("RUST_LOG").is_ok() {
   builder.parse(&env::var("RUST_LOG").unwrap());
  }

  builder.init().unwrap();

  info!("MAIN\tstarting up");
  let metrics_socket = UdpSocket::bind("0.0.0.0:0").unwrap();
  let metrics_host   = ("192.168.59.103", 8125).to_socket_addrs().unwrap().next().unwrap();
  METRICS.lock().unwrap().set_up_remote(metrics_socket, metrics_host);
  let metrics_guard = ProxyMetrics::run();
  METRICS.lock().unwrap().gauge("TEST", 42);

  let config = messages::HttpProxyConfiguration {
    front: "127.0.0.1:8080".parse().unwrap(),
    max_connections: 500,
    buffer_size: 16384,
    ..Default::default()
  };

  let (mut command, channel) = CommandChannel::generate(1000, 10000).expect("should create a channel");
  let jg = thread::spawn(move || {
    network::http::start_listener(String::from("HTTP"), config, channel);
  });

  let http_front = messages::HttpFront { app_id: String::from("app_1"), hostname: String::from("lolcatho.st:8080"), path_begin: String::from("/") };
  let http_instance = messages::Instance { app_id: String::from("app_1"), ip_address: String::from("127.0.0.1"), port: 1026 };
  command.write_message(network::ProxyOrder { id: String::from("ID_ABCD"), command: messages::Command::AddHttpFront(http_front) });
  command.write_message(network::ProxyOrder { id: String::from("ID_EFGH"), command: messages::Command::AddInstance(http_instance) });
  info!("MAIN\tHTTP -> {:?}", command.read_message());
  info!("MAIN\tHTTP -> {:?}", command.read_message());


  let config = messages::TlsProxyConfiguration {
    front: "127.0.0.1:8443".parse().unwrap(),
    max_connections: 500,
    buffer_size: 16384,
    options: (ssl::SSL_OP_CIPHER_SERVER_PREFERENCE | ssl::SSL_OP_NO_COMPRESSION |
               ssl::SSL_OP_NO_TICKET | ssl::SSL_OP_NO_SSLV2 |
               ssl::SSL_OP_NO_SSLV3 | ssl::SSL_OP_NO_TLSV1).bits(),
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

  let (mut command2, channel2) = CommandChannel::generate(1000, 10000).expect("should create a channel");
  let jg2 = thread::spawn(move || {
    network::tls::start_listener(String::from("TLS"), config, channel2);
  });

  let cert1 = include_str!("../../assets/certificate.pem");
  let key1  = include_str!("../../assets/key.pem");

  let tls_front = messages::TlsFront { app_id: String::from("app_1"), hostname: String::from("lolcatho.st"), path_begin: String::from("/"), certificate: String::from(cert1), key: String::from(key1), certificate_chain: vec!() };
  command2.write_message(network::ProxyOrder { id: String::from("ID_IJKL"), command: messages::Command::AddTlsFront(tls_front) });
  let tls_instance = messages::Instance { app_id: String::from("app_1"), ip_address: String::from("127.0.0.1"), port: 1026 };
  command2.write_message(network::ProxyOrder { id: String::from("ID_MNOP"), command: messages::Command::AddInstance(tls_instance) });

  let cert2 = include_str!("../../assets/cert_test.pem");
  let key2  = include_str!("../../assets/key_test.pem");

  let tls_front2 = messages::TlsFront { app_id: String::from("app_2"), hostname: String::from("test.local"), path_begin: String::from("/"), certificate: String::from(cert2), key: String::from(key2), certificate_chain: vec!() };
  command2.write_message(network::ProxyOrder { id: String::from("ID_QRST"), command: messages::Command::AddTlsFront(tls_front2) });
  let tls_instance2 = messages::Instance { app_id: String::from("app_2"), ip_address: String::from("127.0.0.1"), port: 1026 };
  command2.write_message(network::ProxyOrder { id: String::from("ID_UVWX"), command: messages::Command::AddInstance(tls_instance2) });

  info!("MAIN\tTLS -> {:?}", command2.read_message());
  info!("MAIN\tTLS -> {:?}", command2.read_message());
  info!("MAIN\tTLS -> {:?}", command2.read_message());
  info!("MAIN\tTLS -> {:?}", command2.read_message());

  let _ = jg.join();
  info!("MAIN\tgood bye");
}

