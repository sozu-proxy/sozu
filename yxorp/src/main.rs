#![feature(custom_derive, plugin, libc)]
#![plugin(serde_macros)]
#[macro_use] extern crate nom;
#[macro_use] extern crate log;
#[macro_use] extern crate clap;
extern crate env_logger;
extern crate mio;
extern crate yxorp;
extern crate toml;
extern crate rustc_serialize;
extern crate serde;
extern crate serde_json;
extern crate time;
extern crate libc;

mod config;
mod command;
mod state;

use std::net::{UdpSocket,ToSocketAddrs};
use std::sync::mpsc::{channel};
use std::collections::HashMap;
use std::thread::JoinHandle;
use std::env;
use yxorp::network;
use yxorp::network::metrics::{METRICS,ProxyMetrics};
use log::{LogRecord,LogLevelFilter,LogLevel};
use env_logger::LogBuilder;
use clap::{App,Arg};

use command::{Listener,ListenerType};

fn main() {
  let pid = unsafe { libc::getpid() };
  let format = move |record: &LogRecord| {
    match record.level() {
    LogLevel::Debug | LogLevel::Trace => format!("{}\t{}\t{}\t{}\t{}\t|\t{}",
      time::now_utc().rfc3339(), time::precise_time_ns(), pid,
      record.level(), record.args(), record.location().module_path()),
    _ => format!("{}\t{}\t{}\t{}\t{}",
      time::now_utc().rfc3339(), time::precise_time_ns(), pid,
      record.level(), record.args())

    }
  };

  let mut builder = LogBuilder::new();
  builder.format(format).filter(None, LogLevelFilter::Info);

  let matches = App::new("I will not name this thing yxorp")
                        .version(crate_version!())
                        .about("hot reconfigurable proxy")
                        .arg(Arg::with_name("config")
                                    .short("c")
                                    .long("config")
                                    .value_name("FILE")
                                    .help("Sets a custom config file")
                                    .takes_value(true)
                                    .required(true))
                        .get_matches();

  let config_file = matches.value_of("config").unwrap();

  if let Ok(config) = config::Config::load_from_path(config_file) {
    if let Some(log_level) = config.log_level {
     builder.parse(&log_level);
    }

    builder.init().unwrap();
    info!("starting up");

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
            network::http::start_listener(addr, ls.max_connections, ls.buffer_size, sender)
          },
          ListenerType::HTTPS => {
            network::tls::start_listener(addr, ls.max_connections, ls.buffer_size, None, sender)
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

    command::start(config.command_socket, listeners, config.saved_state);
    if let Some(jh) = jh_opt {
      let _ = jh.join();
      info!("good bye");
    }
  } else {
    println!("could not load configuration file at '{}', stopping", config_file);
  }
}

