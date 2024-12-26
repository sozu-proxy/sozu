#[macro_use]
extern crate sozu_lib as sozu;
#[macro_use]
extern crate sozu_command_lib;

#[cfg(feature = "jemallocator")]
#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

/// the arguments to the sozu command line
pub mod cli;
/// Receives orders from the CLI, transmits to workers
// mod command;
pub mod command;
/// The command line logic
pub mod ctl;
/// Forking & restarting the main process using a more recent executable of Sōzu
mod upgrade;
/// Some unix helper functions
pub mod util;
/// Start and restart the worker UNIX processes
mod worker;
