#[macro_use] extern crate nom;
#[macro_use] extern crate log;
#[macro_use] extern crate clap;
#[macro_use] extern crate serde_derive;
extern crate env_logger;
extern crate mio;
extern crate mio_uds;
extern crate serde;
extern crate serde_json;
extern crate time;
extern crate libc;
extern crate slab;
extern crate nix;
extern crate sozu_lib as sozu;
extern crate sozu_command_lib as sozu_command;

mod command;
mod worker;

use std::net::{UdpSocket,ToSocketAddrs};
use std::collections::HashMap;
use sozu::network::metrics::{METRICS,ProxyMetrics};
use sozu_command::config::Config;
use log::{LogRecord,LogLevelFilter,LogLevel};
use env_logger::LogBuilder;
use clap::{App,Arg,SubCommand};

use command::Proxy;
use worker::{get_executable_path,begin_worker_process,start_workers};

fn main() {
  println!("got path: {}", unsafe { get_executable_path().to_str().unwrap() });
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
                        .subcommand(SubCommand::with_name("start")
                                    .about("launch the master process")
                                    .arg(Arg::with_name("config")
                                        .short("c")
                                        .long("config")
                                        .value_name("FILE")
                                        .help("Sets a custom config file")
                                        .takes_value(true)
                                        .required(true)))
                        .subcommand(SubCommand::with_name("worker")
                                    .about("start a worker (internal command, should not be used directly)")
                                    .arg(Arg::with_name("tag").long("tag")
                                         .takes_value(true).required(true).help("worker configuration tag"))
                                    .arg(Arg::with_name("id").long("id")
                                         .takes_value(true).required(true).help("worker identifier"))
                                    .arg(Arg::with_name("fd").long("fd")
                                         .takes_value(true).required(true).help("IPC file descriptor"))
                                    .arg(Arg::with_name("channel-buffer-size").long("channel-buffer-size")
                                         .takes_value(true).required(false).help("Worker's channel buffer size")))
                        .get_matches();

  if let Some(matches) = matches.subcommand_matches("worker") {
    let fd  = matches.value_of("fd").expect("needs a file descriptor")
      .parse::<i32>().expect("the file descriptor must be a number");
    let id  = matches.value_of("id").expect("needs a worker id");
    let tag = matches.value_of("tag").expect("needs a configuration tag");
    let buffer_size = matches.value_of("channel-buffer-size")
      .and_then(|size| size.parse::<usize>().ok())
      .unwrap_or(10000);

    begin_worker_process(fd, id, tag, buffer_size, builder);
    return;
  }

  let submatches = matches.subcommand_matches("start").expect("unknown subcommand");
  let config_file = submatches.value_of("config").expect("required config file");

  if let Ok(config) = Config::load_from_path(config_file) {
    if let Some(ref log_level) = config.log_level {
     builder.parse(&log_level);
    }

    builder.init().unwrap();
    info!("starting up");

    let metrics_socket = UdpSocket::bind("0.0.0.0:0").unwrap();
    let metrics_host   = (&config.metrics.address[..], config.metrics.port).to_socket_addrs().unwrap().next().unwrap();
    METRICS.lock().unwrap().set_up_remote(metrics_socket, metrics_host);
    let metrics_guard = ProxyMetrics::run();

    let mut proxies:HashMap<String,Vec<Proxy>> = HashMap::new();

    for (ref tag, ref ls) in &config.proxies {
      if let Some(workers) = start_workers(&tag, ls) {
        proxies.insert(tag.to_string(), workers);
      }
    };

    let buffer_size     = config.command_buffer_size.unwrap_or(10000);
    let max_buffer_size = config.max_command_buffer_size.unwrap_or(buffer_size * 2);
    command::start(config, proxies);
  } else {
    println!("could not load configuration file at '{}', stopping", config_file);
  }
}

