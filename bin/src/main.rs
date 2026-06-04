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

use sozu_lib::metrics::names;

// FreeBSD/NetBSD libc malloc is already jemalloc; the bundled `jemallocator`
// dep is filtered out of the build graph in `bin/Cargo.toml`. The
// `libc_jemalloc` cfg is emitted by `bin/build.rs` for those targets; without
// the cfg here, rustc would expand the binding to an unresolved crate.
#[cfg(all(feature = "jemallocator", not(libc_jemalloc)))]
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

use cli::Args;
use command::{StartError, begin_main_process};
use ctl::CtlError;
use sozu::metrics::METRICS;
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
                resolve_max_command_buffer_size(command_buffer_size, max_command_buffer_size);
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
                resolve_max_command_buffer_size(command_buffer_size, max_command_buffer_size);
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

/// Resolve the effective `max_command_buffer_size` for an internal `worker` /
/// `main` re-exec subcommand. When the caller (always the `sozu` CLI during
/// an upgrade — these subcommands are never operator-facing) omits an explicit
/// max, it defaults to twice the base command buffer.
///
/// The default branch carries a hard logic invariant we assert here:
/// `max >= base`, because doubling never shrinks a `u64`. We deliberately do
/// NOT assert the relation across the explicit-`Some` branch: that value is
/// caller/config-supplied input (`Config::max_command_buffer_size`, which
/// defaults independently of `command_buffer_size`), so a `max < base` there
/// is a misconfiguration to surface as a runtime error downstream, not a logic
/// bug to panic on. The default uses the same `command_buffer_size * 2`
/// expression as before (behaviour-identical); the assertion only observes it.
fn resolve_max_command_buffer_size(command_buffer_size: u64, explicit_max: Option<u64>) -> u64 {
    match explicit_max {
        Some(max) => max,
        None => {
            let default_max = command_buffer_size * 2;
            // INVARIANT: the defaulted max is at least the base buffer, so an
            // IPC frame that fits the base can always grow into the max. This
            // is the load-bearing relation the framing layer relies on; it is
            // guaranteed for the default but NOT for an explicit caller value.
            debug_assert!(
                default_max >= command_buffer_size,
                "defaulted max_command_buffer_size must be >= command_buffer_size"
            );
            default_max
        }
    }
}

fn register_panic_hook() {
    // We save the original panic hook so we can call it later
    // to have the original behavior
    let original_panic_hook = panic::take_hook();

    panic::set_hook(Box::new(move |panic_info| {
        incr!(names::misc::PANIC);
        METRICS.with(|metrics| {
            (*metrics.borrow_mut()).send_data();
        });

        (*original_panic_hook)(panic_info)
    }));
}
