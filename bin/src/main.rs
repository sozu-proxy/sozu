#[macro_use] extern crate nom;
#[macro_use] extern crate clap;
#[macro_use] extern crate serde_derive;
extern crate mio;
extern crate mio_uds;
extern crate serde;
extern crate serde_json;
extern crate time;
extern crate libc;
extern crate slab;
extern crate rand;
extern crate nix;
#[macro_use] extern crate sozu_lib as sozu;
extern crate sozu_command_lib as sozu_command;

#[cfg(target_os = "linux")]
extern crate num_cpus;
#[cfg(target_os = "linux")]
extern crate procinfo;

mod command;
mod worker;
mod logging;
mod upgrade;

use std::net::{UdpSocket,ToSocketAddrs};
use std::{mem,env};
use sozu::network::metrics::{METRICS,ProxyMetrics};
use sozu_command::config::Config;
use clap::{App,Arg,SubCommand};

#[cfg(target_os = "linux")]
use libc::{cpu_set_t,pid_t};

use command::Worker;
use worker::{begin_worker_process,start_workers};
use upgrade::begin_new_master_process;

fn main() {
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
                                    .arg(Arg::with_name("id").long("id")
                                         .takes_value(true).required(true).help("worker identifier"))
                                    .arg(Arg::with_name("fd").long("fd")
                                         .takes_value(true).required(true).help("IPC file descriptor"))
                                    .arg(Arg::with_name("channel-buffer-size").long("channel-buffer-size")
                                         .takes_value(true).required(false).help("Worker's channel buffer size")))
                        .subcommand(SubCommand::with_name("upgrade")
                                    .about("start a new master process (internal command, should not be used directly)")
                                    .arg(Arg::with_name("fd").long("fd")
                                         .takes_value(true).required(true).help("IPC file descriptor"))
                                    .arg(Arg::with_name("channel-buffer-size").long("channel-buffer-size")
                                         .takes_value(true).required(false).help("Worker's channel buffer size")))
                        .get_matches();

  if let Some(matches) = matches.subcommand_matches("worker") {
    let fd  = matches.value_of("fd").expect("needs a file descriptor")
      .parse::<i32>().expect("the file descriptor must be a number");
    let id  = matches.value_of("id").expect("needs a worker id");
    let buffer_size = matches.value_of("channel-buffer-size")
      .and_then(|size| size.parse::<usize>().ok())
      .unwrap_or(10000);

    begin_worker_process(fd, id, buffer_size);
    return;
  }

  if let Some(matches) = matches.subcommand_matches("upgrade") {
    let fd  = matches.value_of("fd").expect("needs a file descriptor")
      .parse::<i32>().expect("the file descriptor must be a number");
    let buffer_size = matches.value_of("channel-buffer-size")
      .and_then(|size| size.parse::<usize>().ok())
      .unwrap_or(10000);

    begin_new_master_process(fd, buffer_size);
    return;
  }

  let submatches = matches.subcommand_matches("start").expect("unknown subcommand");
  let config_file = submatches.value_of("config").expect("required config file");

  if let Ok(config) = Config::load_from_path(config_file) {
    //FIXME: should have an id for the master too
    logging::setup("MASTER".to_string(), &config.log_level, &config.log_target);
    info!("starting up");

    metrics_set_up!(&config.metrics.address[..], config.metrics.port);
    gauge!("sozu.TEST", 42);

    if check_process_limits(config.clone()) {
      match start_workers(&config) {
        Ok(workers) => {
          info!("created workers: {:?}", workers);

          let handle_process_affinity = match config.handle_process_affinity {
            Some(val) => val,
            None => false
          };

          if cfg!(target_os = "linux") && handle_process_affinity {
            set_workers_affinity(&workers);
          }

          command::start(config, workers);
        },
        Err(e) => error!("Error while creating workers: {}", e)
      }
    }

    info!("master process stopped");
  } else {
    error!("could not load configuration file at '{}', stopping", config_file);
  }
}

/// Set workers process affinity, see man sched_setaffinity
/// Bind each worker (including the master) process to a CPU core.
/// Can bind multiple processes to a CPU core if there are more processes
/// than CPU cores. Only works on Linux.
#[cfg(target_os = "linux")]
fn set_workers_affinity(workers: &Vec<Worker>) {
  let mut cpu_count = 0;
  let max_cpu = num_cpus::get();

  // +1 for the master process that will also be bound to its CPU core
  if (workers.len() + 1) > max_cpu {
    warn!("There are more workers than available CPU cores, \
          multiple workers will be bound to the same CPU core. \
          This may impact performances");
  }

  let master_pid = unsafe { libc::getpid() };
  set_process_affinity(master_pid, cpu_count);
  cpu_count = cpu_count + 1;

  for ref worker in workers {
    if cpu_count >= max_cpu {
      cpu_count = 0;
    }

    set_process_affinity(worker.pid, cpu_count);

    cpu_count = cpu_count + 1;
  }
}

/// Set workers process affinity, see man sched_setaffinity
/// Bind each worker (including the master) process to a CPU core.
/// Can bind multiple processes to a CPU core if there are more processes
/// than CPU cores. Only works on Linux.
#[cfg(not(target_os = "linux"))]
fn set_workers_affinity(_: &Vec<Worker>) {
}

/// Set a specific process to run onto a specific CPU core
#[cfg(target_os = "linux")]
fn set_process_affinity(pid: pid_t, cpu: usize) {
  unsafe {
    let mut cpu_set: cpu_set_t = mem::zeroed();
    let size_cpu_set = mem::size_of::<cpu_set_t>();
    libc::CPU_SET(cpu, &mut cpu_set);
    libc::sched_setaffinity(pid, size_cpu_set, &mut cpu_set);

    debug!("Worker {} bound to CPU core {}", pid, cpu);
  };
}

#[cfg(target_os="linux")]
// We check the hard_limit. The soft_limit can be changed at runtime
// by the process or any user. hard_limit can only be changed by root
fn check_process_limits(config: Config) -> bool {
  let process_limits = procinfo::pid::limits_self()
    .expect("Couldn't read /proc/self/limits to determine max open file descriptors limit");

  // If limit is "unlimited"
  if process_limits.max_open_files.hard.is_none() {
    return true;
  }

  let hard_limit = process_limits.max_open_files.hard.unwrap();
  let http_max_cons = config.http.and_then(|proxy| Some(proxy.max_connections)).unwrap_or(0);
  let https_max_cons = config.https.and_then(|proxy| Some(proxy.max_connections)).unwrap_or(0);

  // check if all proxies are under the hard limit
  if http_max_cons > hard_limit || https_max_cons > hard_limit {
    error!("At least one proxy can't have that much of connections. \
            Current max file descriptor hard limit is: {}", hard_limit);
    return false;
  }

  let total_proxies_connections = http_max_cons + https_max_cons;
  let system_max_fd = procinfo::sys::fs::file_max::file_max()
    .expect("Couldn't read /proc/sys/fs/file-max") as usize;

  if total_proxies_connections > system_max_fd {
    error!("Proxies total max_connections can't be higher than system's file-max limit. \
            Current limit: {}, current value: {}", system_max_fd, total_proxies_connections);
    return false;
  }

  true
}

#[cfg(not(target_os = "linux"))]
fn check_process_limits(_: Config) -> bool {
  true
}
