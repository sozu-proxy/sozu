#[macro_use] extern crate nom;
#[macro_use] extern crate clap;
#[macro_use] extern crate serde_derive;
extern crate mio;
extern crate mio_uds;
extern crate serde_json;
extern crate time;
extern crate libc;
extern crate slab;
extern crate rand;
extern crate nix;
extern crate tempfile;
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
mod util;

use std::env;
use std::panic;
use sozu_command::config::Config;
use clap::{App,Arg,SubCommand};

#[cfg(target_os = "linux")]
use libc::{cpu_set_t,pid_t};

use command::Worker;
use worker::{begin_worker_process,start_workers,get_executable_path};
use upgrade::begin_new_master_process;
use sozu::network::metrics::METRICS;

fn main() {
  register_panic_hook();

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
                                        .required(option_env!("SOZU_CONFIG").is_none())))
                        .subcommand(SubCommand::with_name("worker")
                                    .about("start a worker (internal command, should not be used directly)")
                                    .arg(Arg::with_name("id").long("id")
                                         .takes_value(true).required(true).help("worker identifier"))
                                    .arg(Arg::with_name("fd").long("fd")
                                         .takes_value(true).required(true).help("IPC file descriptor"))
                                    .arg(Arg::with_name("scm").long("scm")
                                         .takes_value(true).required(true).help("IPC SCM_RIGHTS file descriptor"))
                                    .arg(Arg::with_name("configuration-state-fd").long("configuration-state-fd")
                                         .takes_value(true).required(true).help("configuration data file descriptor"))
                                    .arg(Arg::with_name("command-buffer-size").long("command-buffer-size")
                                         .takes_value(true).required(true).help("Worker's channel buffer size"))
                                    .arg(Arg::with_name("max-command-buffer-size").long("max-command-buffer-size")
                                         .takes_value(true).required(true).help("Worker's channel max buffer size")))
                        .subcommand(SubCommand::with_name("upgrade")
                                    .about("start a new master process (internal command, should not be used directly)")
                                    .arg(Arg::with_name("fd").long("fd")
                                         .takes_value(true).required(true).help("IPC file descriptor"))
                                    .arg(Arg::with_name("upgrade-fd").long("upgrade-fd")
                                         .takes_value(true).required(true).help("upgrade data file descriptor"))
                                    .arg(Arg::with_name("command-buffer-size").long("command-buffer-size")
                                         .takes_value(true).required(true).help("Master's command buffer size"))
                                    .arg(Arg::with_name("max-command-buffer-size").long("max-command-buffer-size")
                                         .takes_value(true).required(false).help("Master's max command buffer size")))
                        .get_matches();

  if let Some(matches) = matches.subcommand_matches("worker") {
    let fd  = matches.value_of("fd").expect("needs a file descriptor")
      .parse::<i32>().expect("the file descriptor must be a number");
    let scm  = matches.value_of("scm").expect("needs a file descriptor")
      .parse::<i32>().expect("the SCM_RIGHTS file descriptor must be a number");
    let configuration_state_fd  = matches.value_of("configuration-state-fd")
      .expect("needs a configuration state file descriptor")
      .parse::<i32>().expect("the file descriptor must be a number");
    let id  = matches.value_of("id").expect("needs a worker id")
      .parse::<i32>().expect("the worker id must be a number");
    let buffer_size = matches.value_of("command-buffer-size")
      .and_then(|size| size.parse::<usize>().ok())
      .unwrap_or(1_000_000);
    let max_buffer_size = matches.value_of("max-command-buffer-size")
      .and_then(|size| size.parse::<usize>().ok())
      .unwrap_or(buffer_size * 2);

    begin_worker_process(fd, scm, configuration_state_fd, id, buffer_size, max_buffer_size);
    return;
  }

  if let Some(matches) = matches.subcommand_matches("upgrade") {
    let fd  = matches.value_of("fd").expect("needs a file descriptor")
      .parse::<i32>().expect("the file descriptor must be a number");
    let upgrade_fd  = matches.value_of("upgrade-fd").expect("needs an upgrade file descriptor")
      .parse::<i32>().expect("the file descriptor must be a number");
    let buffer_size = matches.value_of("channel-buffer-size")
      .and_then(|size| size.parse::<usize>().ok())
      .unwrap_or(1_000_000);
    let max_buffer_size = matches.value_of("max-command-buffer-size")
      .and_then(|size| size.parse::<usize>().ok())
      .unwrap_or(buffer_size * 2);

    begin_new_master_process(fd, upgrade_fd, buffer_size, max_buffer_size);
    return;
  }

  let submatches = matches.subcommand_matches("start").expect("unknown subcommand");
  let config_file = match submatches.value_of("config"){
                      Some(config_file) => config_file,
                      None => option_env!("SOZU_CONFIG").expect("could not find `SOZU_CONFIG` env var at build"),
                    };

  if let Ok(config) = Config::load_from_path(config_file) {
    //FIXME: should have an id for the master too
    logging::setup("MASTER".to_string(), &config.log_level,
      &config.log_target, config.log_access_target.as_ref().map(|s| s.as_str()));
    info!("starting up");

    if let Some(ref metrics) = config.metrics.as_ref() {
      metrics_set_up!(&metrics.address[..], metrics.port, "MASTER".to_string(), metrics.tagged_metrics);
    }


    // define this here so we can stop before launching workers if necessary
    let command_socket_path = config.command_socket_path();

    if check_process_limits(config.clone()) {
      let path = unsafe { get_executable_path() };
      match start_workers(path, &config) {
        Ok(workers) => {
          info!("created workers: {:?}", workers);

          if cfg!(target_os = "linux") && config.handle_process_affinity {
            set_workers_affinity(&workers);
          }

          command::start(config, command_socket_path, workers);
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
use std::mem;
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

  // check if all proxies are under the hard limit
  if config.max_connections > hard_limit {
    error!("At least one proxy can't have that much of connections. \
            Current max file descriptor hard limit is: {}", hard_limit);
    return false;
  }

  let system_max_fd = procinfo::sys::fs::file_max::file_max()
    .expect("Couldn't read /proc/sys/fs/file-max") as usize;

  if config.max_connections > system_max_fd {
    error!("Proxies total max_connections can't be higher than system's file-max limit. \
            Current limit: {}, current value: {}", system_max_fd, config.max_connections);
    return false;
  }

  true
}

#[cfg(not(target_os = "linux"))]
fn check_process_limits(_: Config) -> bool {
  true
}

fn register_panic_hook() {
  // We save the original panic hook so we can call it later
  // to have the original behavior
  let original_panic_hook = panic::take_hook();

  panic::set_hook(Box::new(move |panic_info| {
    incr!("panic");
    METRICS.with(|metrics| {
      (*metrics.borrow_mut()).send_data();
    });

    (*original_panic_hook)(panic_info)
  }));
}
