#[macro_use] extern crate nom;
#[macro_use] extern crate serde_derive;
#[macro_use] extern crate lazy_static;
#[macro_use] extern crate structopt_derive;
#[macro_use] extern crate prettytable;
extern crate mio;
extern crate mio_uds;
extern crate serde;
extern crate serde_json;
extern crate time;
extern crate libc;
extern crate slab;
extern crate rand;
extern crate nix;
extern crate tempfile;
extern crate futures;
extern crate regex;
extern crate jemallocator;
extern crate chrono;
extern crate structopt;
extern crate hex;
#[macro_use] extern crate sozu_lib as sozu;
#[macro_use] extern crate sozu_command_lib as sozu_command;

#[cfg(target_os = "linux")]
extern crate num_cpus;
#[cfg(target_os="linux")]
use regex::Regex;

#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

mod command;
mod worker;
mod logging;
mod upgrade;
mod util;
mod cli;
mod ctl;

use std::env;
use std::panic;
use sozu_command::config::Config;
use structopt::StructOpt;

#[cfg(target_os = "linux")]
use libc::{cpu_set_t,pid_t};

use command::Worker;
use worker::{start_workers,get_executable_path};
use sozu::metrics::METRICS;

enum StartupError {
  ConfigurationFileNotSpecified,
  ConfigurationFileLoadError(std::io::Error),
  #[allow(dead_code)]
  TooManyAllowedConnections(String),
  #[allow(dead_code)]
  TooManyAllowedConnectionsForWorker(String),
  WorkersSpawnFail(nix::Error),
  PIDFileNotWritable(String)
}

fn main() {
  register_panic_hook();

  // Init parsing of arguments
  let matches = cli::Sozu::from_args();

  // If we are not, then we want to start sozu
  if let cli::SubCmd::Start = matches.cmd {
    let start = get_config_file_path(&matches)
    .and_then(load_configuration)
    .and_then(|config| {
      util::write_pid_file(&config)
        .map(|()| config)
        .map_err(|err| StartupError::PIDFileNotWritable(err))
    })
    .map(|config| {
      util::setup_logging(&config);
      info!("Starting up");
      util::setup_metrics(&config);

      config
    })
    .and_then(|config| check_process_limits(&config).map(|()| config))
    .and_then(|config| init_workers(&config).map(|workers| (config, workers)))
    .map(|(config, workers)| {
      if config.handle_process_affinity {
        set_workers_affinity(&workers);
      }
      let command_socket_path = config.command_socket_path();
      command::start(config, command_socket_path, workers);
    });

    match start {
      Ok(_) => info!("master process stopped"), // Ok() is only called when the proxy exits
      Err(StartupError::ConfigurationFileNotSpecified) => {
        error!("Configuration file hasn't been specified. Either use -c with the start command \
               or use the SOZU_CONFIG environment variable when building sozu.");
        std::process::exit(1);
      },
      Err(StartupError::ConfigurationFileLoadError(err)) => {
        error!("Invalid configuration file. Error: {:?}", err);
        std::process::exit(1);
      },
      Err(StartupError::TooManyAllowedConnections(err)) | Err(StartupError::TooManyAllowedConnectionsForWorker(err)) => {
        error!("{}", err);
        std::process::exit(1);
      },
      Err(StartupError::WorkersSpawnFail(err)) => {
        error!("At least one worker failed to spawn. Error: {:?}", err);
        std::process::exit(1);
      },
      Err(StartupError::PIDFileNotWritable(err)) => {
        error!("{}", err);
        std::process::exit(1);
      }
    }
  } else if let cli::SubCmd::Worker { fd, scm, configuration_state_fd, id, command_buffer_size, max_command_buffer_size } = matches.cmd {
    let max_command_buffer_size = max_command_buffer_size.unwrap_or(command_buffer_size * 2);
    worker::begin_worker_process(fd, scm, configuration_state_fd, id, command_buffer_size, max_command_buffer_size);
  } else if let cli::SubCmd::Master { fd, upgrade_fd, command_buffer_size, max_command_buffer_size } = matches.cmd {
    let max_command_buffer_size = max_command_buffer_size.unwrap_or(command_buffer_size * 2);
    upgrade::begin_new_master_process(fd, upgrade_fd, command_buffer_size, max_command_buffer_size);
  } else {
    ctl::ctl(matches);
  }
}

fn init_workers(config: &Config) -> Result<Vec<Worker>, StartupError> {
  let path = unsafe { get_executable_path() };
  match start_workers(path, &config) {
    Ok(workers) => {
      info!("created workers: {:?}", workers);
      Ok(workers)
    },
    Err(e) => Err(StartupError::WorkersSpawnFail(e))
  }
}

fn get_config_file_path(matches: &cli::Sozu) -> Result<&str, StartupError> {
  match matches.config.as_ref() {
    Some(config_file) => Ok(config_file.as_str()),
    None => option_env!("SOZU_CONFIG").ok_or(StartupError::ConfigurationFileNotSpecified)
  }
}

fn load_configuration(config_file: &str) -> Result<Config, StartupError> {
  match Config::load_from_path(config_file) {
    Ok(config) => Ok(config),
    Err(e) => Err(StartupError::ConfigurationFileLoadError(e))
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
fn check_process_limits(config: &Config) -> Result<(), StartupError> {
  let process_limits_file = sozu_command::config::Config::load_file("/proc/self/limits")
    .expect("Couldn't read /proc/self/limits to determine max open file descriptors limit");

  let re = Regex::new(r"Max open files\s*\d*\s*(\d*)\s*files").unwrap();

  if let Some(hard_limit) = re.captures(&process_limits_file).and_then(|c| c.get(1))
    .and_then(|m| m.as_str().parse::<usize>().ok()) {
    if config.max_connections > hard_limit {
      let error = format!("At least one worker can't have that many connections. \
              Current max file descriptor hard limit is: {}", hard_limit);
      return Err(StartupError::TooManyAllowedConnectionsForWorker(error));
    }
  }

  let f = sozu_command::config::Config::load_file("/proc/sys/fs/file-max")
    .expect("Couldn't read /proc/sys/fs/file-max");

  let re_max = Regex::new(r"(\d*)").unwrap();

  let system_max_fd = re_max.captures(&f).and_then(|c| c.get(1))
    .and_then(|m| m.as_str().parse::<usize>().ok())
    .expect("Couldn't parse /proc/sys/fs/file-max");

  if config.max_connections > system_max_fd {
    let error = format!("Proxies total max_connections can't be higher than system's file-max limit. \
            Current limit: {}, current value: {}", system_max_fd, config.max_connections);
    return Err(StartupError::TooManyAllowedConnections(error))
  }

  Ok(())
}

#[cfg(not(target_os = "linux"))]
fn check_process_limits(_: &Config) -> Result<(), StartupError> {
  Ok(())
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
