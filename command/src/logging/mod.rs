//! Sōzu logs, optimized for performance
//!
//! Instead of relying on well-known logging or tracing solutions,
//! Sōzu has its own logging stack that prioritizes CPU performance

pub mod access_logs;
pub mod display;
#[macro_use]
pub mod logs;

pub use crate::logging::access_logs::*;
pub use crate::logging::logs::*;

#[derive(thiserror::Error, Debug)]
pub enum LogError {
    #[error("invalid log target {0}: {1}")]
    InvalidLogTarget(String, String),
    #[error("could not connect to TCP socket {0}: {1}")]
    TcpConnect(String, std::io::Error),
}
