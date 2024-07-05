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
