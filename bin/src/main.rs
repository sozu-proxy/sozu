extern crate nom;
#[macro_use] extern crate clap;
#[macro_use] extern crate serde_derive;
#[macro_use] extern crate lazy_static;
#[macro_use] extern crate sozu_lib as sozu;
#[macro_use] extern crate sozu_command_lib as sozu_command;

#[cfg(target_os = "linux")]
extern crate num_cpus;
use cli::{upgrade_main, upgrade_worker};
#[cfg(target_os="linux")]
use regex::Regex;

#[cfg(feature = "jemallocator")]
#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

#[macro_use]
mod logging;

mod command;
mod worker;
mod upgrade;
mod cli;
mod util;

use anyhow::{Context, bail};
use std::panic;
use sozu_command::config::Config;
use clap::ArgMatches;

#[cfg(target_os = "linux")]
use libc::{cpu_set_t,pid_t};

use crate::command::Worker;
use crate::worker::{start_workers,get_executable_path};
use sozu::metrics::METRICS;

fn main() -> Result<(), anyhow::Error> {
  register_panic_hook();

  // Init parsing of arguments
  let matches = cli::init();

  // Check if we are upgrading workers
  if let Some(worker_matches) = matches.subcommand_matches("worker") {
    return upgrade_worker(&worker_matches);
  }

  // check if we are upgrading main (overrides worker upgrading)
  if let Some(upgrade_matches) = matches.subcommand_matches("upgrade") {
    return upgrade_main(&upgrade_matches);
  }
            
  // If we are not, then we want to start sozu
  start(&matches)?;

  info!("main process stopped");
  Ok(())
}

fn start(matches: &ArgMatches) -> Result<(), anyhow::Error> {

  let config = load_configuration(get_config_file_path(&matches)?)?;

  util::setup_logging(&config);
  info!("Starting up");
  util::setup_metrics(&config);
  util::write_pid_file(&config)
    .with_context(|| "The PID file is not writeable")?;

  update_process_limits(&config)?;

  let workers = init_workers(&config)?;

  if config.handle_process_affinity {
    set_workers_affinity(&workers);
  }

  let command_socket_path = config.command_socket_path();

  command::start(config, command_socket_path, workers)
    .with_context(|| "Sozu main start failed")
}

fn init_workers(config: &Config) -> Result<Vec<Worker>, anyhow::Error> {
  let path = unsafe {
    get_executable_path().with_context(|| "Failed to get the executable path for the worker")?
  };

  let workers = start_workers(path, &config)
    .with_context(|| "At least one worker failed to spawn")?;
  
  info!("created workers: {:?}", workers);
  Ok(workers)
}

fn get_config_file_path<'a>(matches: &'a ArgMatches<'a>) -> Result<&'a str, anyhow::Error> {
  let start_matches = matches.subcommand_matches("start").with_context(|| "unknown subcommand")?;
  match start_matches.value_of("config") {
    Some(config_file) => Ok(config_file),
    None => {
      option_env!("SOZU_CONFIG")
        .ok_or(
          anyhow::Error::msg(
            "Configuration file hasn't been specified. Either use -c with the start command \
            or use the SOZU_CONFIG environment variable when building sozu."
          )
        )
    }
  }
}

fn load_configuration(config_file: &str) -> Result<Config, anyhow::Error> {
  Config::load_from_path(config_file).with_context(|| "Invalid configuration file.")
}

/// Set workers process affinity, see man sched_setaffinity
/// Bind each worker (including the main) process to a CPU core.
/// Can bind multiple processes to a CPU core if there are more processes
/// than CPU cores. Only works on Linux.
#[cfg(target_os = "linux")]
fn set_workers_affinity(workers: &Vec<Worker>) {
  let mut cpu_count = 0;
  let max_cpu = num_cpus::get();

  // +1 for the main process that will also be bound to its CPU core
  if (workers.len() + 1) > max_cpu {
    warn!("There are more workers than available CPU cores, \
          multiple workers will be bound to the same CPU core. \
          This may impact performances");
  }

  let main_pid = unsafe { libc::getpid() };
  set_process_affinity(main_pid, cpu_count);
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
/// Bind each worker (including the main) process to a CPU core.
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
fn update_process_limits(config: &Config) -> Result<(), anyhow::Error> {
  let wanted_opened_files = (config.max_connections as u64) * 2;

  // Ensure we don't exceed the system maximum capacity
  let f = sozu_command::config::Config::load_file("/proc/sys/fs/file-max")
    .with_context(|| "Couldn't read /proc/sys/fs/file-max")?;
  let re_max = Regex::new(r"(\d*)").unwrap();
  let system_max_fd = re_max.captures(&f).and_then(|c| c.get(1))
    .and_then(|m| m.as_str().parse::<usize>().ok())
    .expect("Couldn't parse /proc/sys/fs/file-max");
  if config.max_connections > system_max_fd {
    bail!(
      format!(
        "Proxies total max_connections can't be higher than system's file-max limit.\nCurrent limit: {}, current value: {}",
        system_max_fd,
        config.max_connections)
      );
  }

  // Get the soft and hard limits for the current process
  let mut limits = libc::rlimit {
    rlim_cur: 0,
    rlim_max: 0,
  };
  unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) };

  // Ensure we don't exceed the hard limit
  if limits.rlim_max < wanted_opened_files {
      bail!(
        format!(
          "at least one worker can't have that many connections. \
          current max file descriptor hard limit is: {}, \
          configured max_connections is {} (the worker needs two file descriptors \
          per client connection)",
          limits.rlim_max,
          config.max_connections
        )
      )
  }

  if limits.rlim_cur < wanted_opened_files && limits.rlim_cur != limits.rlim_max {
    // Try to get twice what we need to be safe, or rlim_max if we exceed that
    limits.rlim_cur = limits.rlim_max.min(wanted_opened_files * 2);
    unsafe {
      libc::setrlimit(libc::RLIMIT_NOFILE, &limits);

      // Refresh the data we have
      libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits);
    }
  }

  // Ensure we don't exceed the new soft limit
  if limits.rlim_cur < wanted_opened_files {
      bail!(
        format!(
          "at least one worker can't have that many connections. \
          current max file descriptor soft limit is: {}, \
          configured max_connections is {} (the worker needs two file descriptors \
          per client connection)",
          limits.rlim_cur,
          config.max_connections
        )
      )
  }

  Ok(())
}

#[cfg(not(target_os = "linux"))]
fn update_process_limits(_: &Config) -> Result<(), StartupError> {
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
