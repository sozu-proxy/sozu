#![allow(unused_variables,unused_must_use)]
#[macro_use] extern crate log;
extern crate env_logger;
extern crate yxorp;
extern crate openssl;
extern crate mio;

use std::thread;
use std::sync::mpsc;
use yxorp::messages;
use yxorp::network::{self,ProxyOrder};

fn main() {
  env_logger::init().unwrap();
  info!("starting up");

  let config = messages::HttpProxyConfiguration {
    front: "127.0.0.1:8080".parse().unwrap(),
    ..Default::default()
  };

  let poll          = mio::Poll::new().unwrap();
  let (tx, rx)      = mio::channel::channel::<ProxyOrder>();
  let (sender, rec) = mpsc::channel::<network::ServerMessage>();

  let jg            = thread::spawn(move || {
    network::http::start_listener(config, sender, poll, rx);
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

  tx.send(network::ProxyOrder::Command(
    String::from("ID_ABCD"),
    messages::Command::AddHttpFront(http_front)
  ));

  tx.send(network::ProxyOrder::Command(
    String::from("ID_EFGH"),
    messages::Command::AddInstance(http_instance)
  ));

  println!("HTTP -> {:?}", rec.recv().unwrap());
  println!("HTTP -> {:?}", rec.recv().unwrap());

  let _ = jg.join();
  info!("good bye");
}

