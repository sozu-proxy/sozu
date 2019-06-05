#![allow(unused_variables,unused_must_use)]
extern crate sozu_lib as sozu;
#[macro_use] extern crate sozu_command_lib as sozu_command;
extern crate time;

use std::env;
use std::thread;
use std::io::stdout;
use sozu_command::proxy;
use sozu_command::channel::Channel;
use sozu_command::proxy::{LoadBalancingParams,RulePosition,PathRule};
use sozu_command::logging::{Logger,LoggerBackend};

fn main() {
  if env::var("RUST_LOG").is_ok() {
   Logger::init("EXAMPLE".to_string(), &env::var("RUST_LOG").expect("could not get the RUST_LOG env var"), LoggerBackend::Stdout(stdout()), None);
  } else {
   Logger::init("EXAMPLE".to_string(), "info", LoggerBackend::Stdout(stdout()), None);
  }

  info!("starting up");

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
    app_id:   String::from("test"),
    address:  "127.0.0.1:8080".parse().unwrap(),
    hostname: String::from("example.com"),
    path:     PathRule::Prefix(String::from("/")),
    position: RulePosition::Pre,
  };
  let http_backend = proxy::Backend {
    app_id:                    String::from("test"),
    backend_id:                String::from("test-0"),
    address:                   "127.0.0.1:8000".parse().unwrap(),
    load_balancing_parameters: Some(LoadBalancingParams::default()),
    sticky_id:                 None,
    backup:                    None,
  };

  command.write_message(&proxy::ProxyRequest {
    id:    String::from("ID_ABCD"),
    order: proxy::ProxyRequestData::AddHttpFront(http_front)
  });

  command.write_message(&proxy::ProxyRequest {
    id:    String::from("ID_EFGH"),
    order: proxy::ProxyRequestData::AddBackend(http_backend)
  });

  println!("HTTP -> {:?}", command.read_message());
  println!("HTTP -> {:?}", command.read_message());

  let _ = jg.join();
  info!("good bye");
}

