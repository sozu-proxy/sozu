#[macro_use] extern crate log;
#[macro_use] extern crate nom;
extern crate env_logger;
extern crate bytes;
extern crate mio;
extern crate rustc_serialize;
extern crate yxorp;
extern crate toml;

mod command;
mod state;

use std::net::{UdpSocket,ToSocketAddrs};
use std::sync::mpsc::{channel};
use std::collections::HashMap;
use yxorp::network;
use yxorp::network::metrics::{METRICS,ProxyMetrics};

fn main() {
  env_logger::init().unwrap();
  info!("starting up");
  let metrics_socket = UdpSocket::bind("0.0.0.0:0").unwrap();
  let metrics_host   = ("192.168.59.103", 8125).to_socket_addrs().unwrap().next().unwrap();
  METRICS.lock().unwrap().set_up_remote(metrics_socket, metrics_host);
  let metrics_guard = ProxyMetrics::run();

  let (sender, _) = channel::<network::ServerMessage>();
  let (tx, jg) = network::http::start_listener("127.0.0.1:8080".parse().unwrap(), 500, sender);

  let (sender2, _) = channel::<network::ServerMessage>();
  let (tx2, jg2) = network::tls::start_listener("127.0.0.1:8443".parse().unwrap(), 500, sender2);

  let mut listeners = HashMap::new();
  listeners.insert(String::from("HTTP"), tx);
  listeners.insert(String::from("TLS"),  tx2);
  command::start(String::from("./command_folder"), listeners);
  let _ = jg.join();
  info!("good bye");
}

