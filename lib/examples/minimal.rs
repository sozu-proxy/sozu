#![allow(unused_variables,unused_must_use)]
#[macro_use] extern crate log;
extern crate env_logger;
extern crate sozu_lib as sozu;
extern crate openssl;
extern crate mio;
extern crate mio_uds;

use std::thread;
use std::sync::mpsc;
use mio_uds::UnixStream;
use sozu::messages;
use sozu::network::{self,ProxyOrder,ServerMessage};
use sozu::network::proxy::Channel;
use sozu::command::CommandChannel;

fn main() {
  env_logger::init().unwrap();
  info!("starting up");

  let config = messages::HttpProxyConfiguration {
    front: "127.0.0.1:8080".parse().unwrap(),
    ..Default::default()
  };

  let (mut command, channel) = CommandChannel::generate(1000, 10000).expect("should create a channel");

  let jg            = thread::spawn(move || {
    network::http::start_listener(String::from("HTTP"), config, channel);
  });

  let http_front = messages::HttpFront {
    app_id:     String::from("test"),
    hostname:   String::from("example.com"),
    path_begin: String::from("/")
  };
  let http_instance = messages::Instance {
    app_id:     String::from("test"),
    ip_address: String::from("127.0.0.1"),
    port:       8000
  };

  command.write_message(&network::ProxyOrder {
    id:      String::from("ID_ABCD"),
    command: messages::Command::AddHttpFront(http_front)
  });

  command.write_message(&network::ProxyOrder {
    id:      String::from("ID_EFGH"),
    command: messages::Command::AddInstance(http_instance)
  });

  println!("HTTP -> {:?}", command.read_message());
  println!("HTTP -> {:?}", command.read_message());

  let _ = jg.join();
  info!("good bye");
}

