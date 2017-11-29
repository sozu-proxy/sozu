#![allow(unused_variables,unused_must_use)]
#[macro_use] extern crate sozu_lib as sozu;
extern crate sozu_command_lib as sozu_command;
extern crate time;

use std::env;
use std::thread;
use std::io::stdout;
use sozu::network;
use sozu_command::messages;
use sozu_command::channel::Channel;
use sozu::logging::{Logger,LoggerBackend};

fn main() {
  if env::var("RUST_LOG").is_ok() {
   Logger::init("EXAMPLE".to_string(), &env::var("RUST_LOG").expect("could not get the RUST_LOG env var"), LoggerBackend::Stdout(stdout()));
  } else {
   Logger::init("EXAMPLE".to_string(), "info", LoggerBackend::Stdout(stdout()));
  }

  info!("starting up");

  let config = messages::HttpProxyConfiguration {
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
    app_id:     String::from("test"),
    hostname:   String::from("example.com"),
    path_begin: String::from("/"),
  };
  let http_instance = messages::Instance {
    app_id:      String::from("test"),
    instance_id: String::from("test-0"),
    ip_address:  String::from("127.0.0.1"),
    port:        8000
  };

  command.write_message(&messages::OrderMessage {
    id:    String::from("ID_ABCD"),
    order: messages::Order::AddHttpFront(http_front)
  });

  command.write_message(&messages::OrderMessage {
    id:    String::from("ID_EFGH"),
    order: messages::Order::AddInstance(http_instance)
  });

  println!("HTTP -> {:?}", command.read_message());
  println!("HTTP -> {:?}", command.read_message());

  let _ = jg.join();
  info!("good bye");
}

