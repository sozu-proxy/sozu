#[macro_use] extern crate nom;
#[macro_use] extern crate log;
extern crate env_logger;
extern crate mio;
extern crate rustc_serialize;
extern crate yxorp;
extern crate toml;

mod config;
mod command;
mod state;

use std::net::{UdpSocket,ToSocketAddrs};
use std::sync::mpsc::{channel};
use std::collections::HashMap;
use std::thread::JoinHandle;
use yxorp::network;
use yxorp::network::metrics::{METRICS,ProxyMetrics};

use command::{Listener,ListenerType};

fn main() {
  env_logger::init().unwrap();
  info!("starting up");

  // FIXME: should load configuration from a CLI argument
  if let Ok(config) = config::Config::load_from_path("./config.toml") {
    let metrics_socket = UdpSocket::bind("0.0.0.0:0").unwrap();
    let metrics_host   = (&config.metrics.address[..], config.metrics.port).to_socket_addrs().unwrap().next().unwrap();
    METRICS.lock().unwrap().set_up_remote(metrics_socket, metrics_host);
    let metrics_guard = ProxyMetrics::run();

    let mut listeners = HashMap::new();
    let mut jh_opt: Option<JoinHandle<()>> = None;

    for (ref tag, ref ls) in config.listeners {
      let (sender, receiver) = channel::<network::ServerMessage>();
      let mut address = ls.address.clone();
      address.push(':');
      address.push_str(&ls.port.to_string());

      if let Ok(addr) = address.parse() {
        let (tx, jg) = match ls.listener_type {
          ListenerType::HTTP => {
            network::http::start_listener(addr, ls.max_connections, sender)
          },
          ListenerType::HTTPS => {
            network::tls::start_listener(addr, ls.max_connections, None, sender)
          },
          _ => unimplemented!()
        };
        let l =  Listener::new(tag.clone(), ls.listener_type, ls.address.clone(), ls.port, tx, receiver);
        listeners.insert(tag.clone(), l);
        jh_opt = Some(jg);
      } else {
        error!("could not parse address: {}", address);
      }
    };

    command::start(config.command_socket, listeners);
    if let Some(jh) = jh_opt {
      let _ = jh.join();
      info!("good bye");
    }
  } else {
    println!("could not load configuration, stopping");
  }
}

