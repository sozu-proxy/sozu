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
mod worker;

use std::net::{UdpSocket,ToSocketAddrs};
use std::collections::HashMap;
use std::thread::JoinHandle;
use sozu::network::metrics::{METRICS,ProxyMetrics};
use log::{LogRecord,LogLevelFilter,LogLevel};
use env_logger::LogBuilder;
use clap::{App,Arg,SubCommand};

use command::Listener;
use worker::start_workers;

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
                        .subcommand(SubCommand::with_name("worker")
                                    .about("start a worker (internal command, should not be used directly)")
                                    .arg(Arg::with_name("fd").long("fd")
                                         .takes_value(true).required(true).help("IPC file descriptor")))
                        .get_matches();

  let config_file = matches.value_of("config").unwrap();

  if let Ok(config) = config::Config::load_from_path(config_file) {
    if let Some(matches) = matches.subcommand_matches("worker") {
      let fd = matches.value_of("fd").expect("needs a file descriptor")
        .parse::<i32>().expect("the file descriptor must be a number");
      println!("will start a worker with file descriptor: {}", fd);
      return;
    }

    if let Some(log_level) = config.log_level {
     builder.parse(&log_level);
    }

    builder.init().unwrap();
    info!("starting up");

    let metrics_socket = UdpSocket::bind("0.0.0.0:0").unwrap();
    let metrics_host   = (&config.metrics.address[..], config.metrics.port).to_socket_addrs().unwrap().next().unwrap();
    METRICS.lock().unwrap().set_up_remote(metrics_socket, metrics_host);
    let metrics_guard = ProxyMetrics::run();

    let mut listeners:HashMap<String,Vec<Listener>> = HashMap::new();
    let mut jh_opt: Option<JoinHandle<()>> = None;

    for (ref tag, ref ls) in config.listeners {
      if let Some((jg, workers)) = start_workers(&tag, ls) {
        listeners.insert(tag.clone(), workers);
        jh_opt = Some(jg);
      }
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

