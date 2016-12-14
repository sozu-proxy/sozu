#![allow(unused_variables,unused_must_use)]
#[macro_use] extern crate log;
extern crate env_logger;
extern crate sozu_lib as sozu;
extern crate openssl;
extern crate mio;

use std::thread;
use std::sync::mpsc;
use sozu::messages;
use sozu::network::{self,ProxyOrder};

fn main() {
  env_logger::init().unwrap();
  info!("starting up");

  let config = messages::HttpProxyConfiguration {
    front: "127.0.0.1:8080".parse().unwrap(),
    ..Default::default()
  };

  let (tx, rx)      = mio::channel::channel::<ProxyOrder>();
  let (sender, rec) = mpsc::channel::<network::ServerMessage>();

  let jg            = thread::spawn(move || {
    network::http::start_listener(String::from("HTTP"), config, sender, rx);
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

  tx.send(network::ProxyOrder {
    id:      String::from("ID_ABCD"),
    command: messages::Command::AddHttpFront(http_front)
  });

  tx.send(network::ProxyOrder {
    id:      String::from("ID_EFGH"),
    command: messages::Command::AddInstance(http_instance)
  });

  println!("HTTP -> {:?}", rec.recv().unwrap());
  println!("HTTP -> {:?}", rec.recv().unwrap());

  let _ = jg.join();
  info!("good bye");
}

