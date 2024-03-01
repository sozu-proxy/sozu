//! Sōzu logs, optimized for performance
//!
//! Instead of relying on well-known logging or tracing solutions,
//! Sōzu has its own logging stack that prioritizes CPU performance
//!
//! The `logs-cache` flag, on of that, saves lookup time by storing
//! the ENABLED status of each log call-site, in a `static mut`.
//! The gain in performance is measurable with a lot of log directives,
//! but mostly negligible, since CPUs are clever enough to recognize such patterns.

pub mod access_logs;
pub mod display;
#[macro_use]
pub mod logs;

pub use crate::logging::access_logs::*;
pub use crate::logging::logs::*;
