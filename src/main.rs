#[macro_use] extern crate nom;
extern crate mio;
extern crate time;
extern crate libc;
extern crate amqp;
extern crate env_logger;
extern crate rustc_serialize;
extern crate rand;
extern crate openssl;

mod bus;
mod network;
mod parser;
mod messages;

use std::sync::mpsc::{channel};
use std::thread;
use messages::Topic;
use bus::Message;

fn main() {
  let bus_tx = bus::start_bus();
  let (sender, _) = channel::<network::http::ServerMessage>();
  let (tx, jg) = network::http::start_listener("127.0.0.1:8080".parse().unwrap(), 10, 500, sender);


  let (http_proxy_conf_input,http_proxy_conf_listener) = channel();
  let _ = bus_tx.send(Message::Subscribe(Topic::HttpProxyConfig, http_proxy_conf_input));
  if let Ok(Message::SubscribeOk) = http_proxy_conf_listener.recv() {
    println!("Subscribed to http_proxy_conf commands");

    network::amqp::init_rabbitmq(bus_tx);
    println!("Subscribed to http_proxy_conf commands");

    thread::spawn(move || {
      loop {
        if let Ok(Message::Msg(command)) = http_proxy_conf_listener.recv() {
          if let Err(e) = tx.send(network::http::HttpProxyOrder::Command(command)) {
            println!("Error sending HttpProxyOrder: {:?}", e);
          }
        }
      }
    });
  }

  let _ = jg.join();
  println!("good bye");
}

