//! A lightweight, fast, hot-reloadable manager of reverse proxy workers.
//!
//! This crate provides a binary that takes a configuration file and
//! launches a main process and proxy workers that all share the same state.
//!
//! ```
//! sozu --config config.toml start
//! ```
//!
//! The state is reloadable during runtime:
//! the main process can receive requests via a UNIX socket,
//! in order to add listeners, frontends, backends etc.
//!
//! The `sozu` binary works as a CLI to send requests to the main process via the UNIX socket.
//! For instance:
//!
//! ```
//! sozu --config config.toml listener http add --address 127.0.0.1:8080
//! ```
//!
//!
//! The requests sent to Sōzu are defined in protobuf in the `sozu_command_lib` crate,
//! which means other programs can use the protobuf definition and send roquests
//! to Sōzu via its UNIX socket.

#[macro_use]
extern crate sozu_lib as sozu;
#[macro_use]
extern crate sozu_command_lib;
#[cfg(target_os = "linux")]
extern crate num_cpus;

#[cfg(feature = "jemallocator")]
#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

/// the arguments to the sozu command line
mod cli;
/// Receives orders from the CLI, transmits to workers
// mod command;
mod command;
/// The command line logic
mod ctl;
/// Forking & restarting the main process using a more recent executable of Sōzu
mod upgrade;
/// Some unix helper functions
pub mod util;
/// Start and restart the worker UNIX processes
mod worker;

use std::panic;

use sozu::metrics::METRICS;

use cli::Args;
use command::{begin_main_process, StartError};
use ctl::CtlError;
use upgrade::UpgradeError;
use worker::WorkerError;

#[derive(thiserror::Error, Debug)]
enum MainError {
    #[error("failed to start Sōzu: {0}")]
    StartMain(StartError),
    #[error("failed to start new worker: {0}")]
    BeginWorker(WorkerError),
    #[error("failed to start new main process: {0}")]
    BeginNewMain(UpgradeError),
    #[error("{0}")]
    Cli(CtlError),
    #[error("paw io error: {0}")]
    Io(std::io::Error),
}

impl From<std::io::Error> for MainError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[paw::main]
fn main(args: Args) -> Result<(), MainError> {
    register_panic_hook();

    let result = match args.cmd {
        cli::SubCmd::Start => begin_main_process(&args).map_err(MainError::StartMain),
        // this is used only by the CLI when upgrading
        cli::SubCmd::Worker {
            fd: worker_to_main_channel_fd,
            scm: worker_to_main_scm_fd,
            configuration_state_fd,
            id,
            command_buffer_size,
            max_command_buffer_size,
        } => {
            let max_command_buffer_size =
                max_command_buffer_size.unwrap_or(command_buffer_size * 2);
            worker::begin_worker_process(
                worker_to_main_channel_fd,
                worker_to_main_scm_fd,
                configuration_state_fd,
                id,
                command_buffer_size,
                max_command_buffer_size,
            )
            .map_err(MainError::BeginWorker)
        }
        // this is used only by the CLI when upgrading
        cli::SubCmd::Main {
            fd,
            upgrade_fd,
            command_buffer_size,
            max_command_buffer_size,
        } => {
            let max_command_buffer_size =
                max_command_buffer_size.unwrap_or(command_buffer_size * 2);
            upgrade::begin_new_main_process(
                fd,
                upgrade_fd,
                command_buffer_size,
                max_command_buffer_size,
            )
            .map_err(MainError::BeginNewMain)
        }
        _ => ctl::ctl(args).map_err(MainError::Cli),
    };

    if let Err(main_error) = &result {
        eprintln!("\n{main_error}\n");
    }
    result
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
