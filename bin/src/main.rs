#![feature(proc_macro)]
#[macro_use] extern crate nom;
#[macro_use] extern crate log;
#[macro_use] extern crate clap;
#[macro_use] extern crate serde_derive;
extern crate env_logger;
extern crate mio;
extern crate mio_uds;
extern crate toml;
extern crate serde;
extern crate serde_json;
extern crate time;
extern crate libc;
extern crate slab;
extern crate sozu_lib as sozu;

mod config;
mod command;
mod state;

use std::net::{UdpSocket,ToSocketAddrs};
use std::sync::mpsc::{channel};
use std::collections::HashMap;
use std::thread::{self,JoinHandle};
use sozu::network::{self,ProxyOrder};
use sozu::network::metrics::{METRICS,ProxyMetrics};
use log::{LogRecord,LogLevelFilter,LogLevel};
use env_logger::LogBuilder;
use clap::{App,Arg};
use mio::channel;

use command::Listener;
use command::data::ListenerType;

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

  let matches = App::new("sozu")
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
    let jh_opt: Option<JoinHandle<()>> = None;

    for (ref tag, ref ls) in config.listeners {
      let jh_opt = match ls.listener_type {
        ListenerType::HTTP => {
          //FIXME: make safer
          if let Some(conf) = ls.to_http() {
            let mut http_listeners = Vec::new();
            for index in 1..ls.worker_count.unwrap_or(1) {
              let (sender, receiver) = channel::<network::ServerMessage>();
              let (tx, rx) = channel::channel::<ProxyOrder>();
              let config = conf.clone();
              let t = format!("{}-{}", tag, index);
              thread::spawn(move || {
                network::http::start_listener(t, config, sender, rx);
              });
              let l =  Listener::new(tag.clone(), index as u8, ls.listener_type, ls.address.clone(), ls.port, tx, receiver);
              http_listeners.push(l);
            }
            let (sender, receiver) = channel::<network::ServerMessage>();
            let (tx, rx) = channel::channel::<ProxyOrder>();
            let t = format!("{}-{}", tag, 0);
            //FIXME: keep this to get a join guard
            let jg = thread::spawn(move || {
              network::http::start_listener(t, conf, sender, rx);
            });
            let l =  Listener::new(tag.clone(), 0, ls.listener_type, ls.address.clone(), ls.port, tx, receiver);
            http_listeners.push(l);
            listeners.insert(tag.clone(), http_listeners);
            Some(jg)
          } else {
            None
          }
        },
        ListenerType::HTTPS => {
          if let Some(conf) = ls.to_tls() {
            let mut tls_listeners = Vec::new();
            for index in 1..ls.worker_count.unwrap_or(1) {
              let (sender, receiver) = channel::<network::ServerMessage>();
              let (tx, rx) = channel::channel::<ProxyOrder>();
              let config = conf.clone();
              let t = format!("{}-{}", tag, index);
              thread::spawn(move || {
                network::tls::start_listener(t, config, sender, rx);
              });
              let l =  Listener::new(tag.clone(), index as u8, ls.listener_type, ls.address.clone(), ls.port, tx, receiver);
              tls_listeners.push(l);
            }
            let (sender, receiver) = channel::<network::ServerMessage>();
            let (tx, rx) = channel::channel::<ProxyOrder>();
              let t = format!("{}-{}", tag, 0);
            //FIXME: keep this to get a join guard
            let jg = thread::spawn(move || {
              network::tls::start_listener(t, conf, sender, rx);
            });
            let l =  Listener::new(tag.clone(), 0, ls.listener_type, ls.address.clone(), ls.port, tx, receiver);
            tls_listeners.push(l);
            listeners.insert(tag.clone(), tls_listeners);
            Some(jg)
          } else {
            None
          }
        },
        _ => unimplemented!()
      };
    };

    let buffer_size     = config.command_buffer_size.unwrap_or(10000);
    let max_buffer_size = config.max_command_buffer_size.unwrap_or(buffer_size * 2);
    command::start(config.command_socket, listeners, config.saved_state, buffer_size, max_buffer_size);
    //FIXME: really join on all threads?
    if let Some(jh) = jh_opt {
      let _ = jh.join();
      info!("good bye");
    }
  } else {
    println!("could not load configuration file at '{}', stopping", config_file);
  }
}

