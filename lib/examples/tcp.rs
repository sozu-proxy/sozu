#![allow(unused_variables,unused_must_use)]
extern crate sozu_lib as sozu;
#[macro_use] extern crate sozu_command_lib as sozu_command;
extern crate time;

use std::thread;
use std::io::stdout;
use sozu::network;
use sozu_command::messages;
use sozu_command::messages::{TcpListener, LoadBalancingParams};
use sozu_command::channel::Channel;
use sozu_command::logging::{Logger,LoggerBackend};

fn main() {
  /*
  if env::var("RUST_LOG").is_ok() {
   Logger::init("EXAMPLE".to_string(), &env::var("RUST_LOG").expect("could not get the RUST_LOG env var"), LoggerBackend::Stdout(stdout()));
  } else {
   Logger::init("EXAMPLE".to_string(), "info", LoggerBackend::Stdout(stdout()));
  }
  */
 Logger::init("EXAMPLE".to_string(), "debug", LoggerBackend::Stdout(stdout()), None);

  info!("starting up");

  let (mut command, channel) = Channel::generate(1000, 10000).expect("should create a channel");

  let jg = thread::spawn(move || {
    let max_listeners = 500;
    let max_buffers   = 500;
    let buffer_size   = 16384;
    let listener = TcpListener {
      front: "127.0.0.1:8080".parse().unwrap(),
      public_address: None,
      expect_proxy: false,
    };
    Logger::init("TCP".to_string(), "debug", LoggerBackend::Stdout(stdout()), None);
    network::tcp::start(listener, max_buffers, buffer_size, channel);
  });

  let tcp_front = messages::TcpFront {
    app_id:  String::from("test"),
    address: "127.0.0.1:8080".parse().unwrap(),
  };
  let tcp_backend = messages::Backend {
    app_id:     String::from("test"),
    backend_id: String::from("test-0"),
    address:    "127.0.0.1:1026".parse().unwrap(),
    load_balancing_parameters: Some(LoadBalancingParams::default()),
    sticky_id:   None,
    backup:      None,
  };

  command.write_message(&messages::OrderMessage {
    id:    String::from("ID_ABCD"),
    order: messages::Order::AddTcpFront(tcp_front)
  });

  command.write_message(&messages::OrderMessage {
    id:    String::from("ID_EFGH"),
    order: messages::Order::AddBackend(tcp_backend)
  });

  info!("TCP -> {:?}", command.read_message());
  info!("TCP -> {:?}", command.read_message());

  let _ = jg.join();
  info!("good bye");
}

