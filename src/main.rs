#[macro_use] extern crate nom;
#[macro_use] extern crate log;
extern crate mio;
extern crate bytes;
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
  env_logger::init().unwrap();
  info!("starting up");

  let bus_tx = bus::start_bus();
  let (sender, _) = channel::<network::ServerMessage>();
  let (tx, jg) = network::http::start_listener("127.0.0.1:8080".parse().unwrap(), 10, 500, sender);

  let (http_proxy_conf_input,http_proxy_conf_listener) = channel();
  let _ = bus_tx.send(Message::Subscribe(Topic::HttpProxyConfig, http_proxy_conf_input));
  /*let config_tx = tx.clone();
  if let Ok(Message::SubscribeOk) = http_proxy_conf_listener.recv() {
  info!("Subscribed to http_proxy_conf commands");

    network::amqp::init_rabbitmq(bus_tx);
    info!("Subscribed to http_proxy_conf commands");

    thread::spawn(move || {
      loop {
        if let Ok(Message::Msg(command)) = http_proxy_conf_listener.recv() {
          if let Err(e) = config_tx.send(network::http::HttpProxyOrder::Command(command)) {
            info!("Error sending HttpProxyOrder: {:?}", e);
          }
        }
      }
    });
  }
  */

  let http_front = messages::HttpFront { app_id: String::from("app_1"), hostname: String::from("lolcatho.st:8080"), path_begin: String::from("/"), port: 8080 };
  let http_instance = messages::Instance { app_id: String::from("app_1"), ip_address: String::from("127.0.0.1"), port: 1026 };
  tx.send(network::http::HttpProxyOrder::Command(messages::Command::AddHttpFront(http_front)));
  tx.send(network::http::HttpProxyOrder::Command(messages::Command::AddInstance(http_instance)));

  let (sender2, _) = channel::<network::ServerMessage>();
  let (tx2, jg2) = network::tls::start_listener("127.0.0.1:8443".parse().unwrap(), 10, 500, sender2);
  let tls_front = messages::HttpFront { app_id: String::from("app_1"), hostname: String::from("lolcatho.st:8443"), path_begin: String::from("/"), port: 8443 };
  tx2.send(network::tls::HttpProxyOrder::Command(messages::Command::AddHttpFront(tls_front)));
  let tls_instance = messages::Instance { app_id: String::from("app_1"), ip_address: String::from("127.0.0.1"), port: 1026 };
  tx2.send(network::tls::HttpProxyOrder::Command(messages::Command::AddInstance(tls_instance)));

  let _ = jg.join();
  info!("good bye");
}

