use libc;
use std::str::FromStr;
use std::cmp::{self,Ord};
use std::sync::{Mutex, Once, ONCE_INIT};
use std::cell::RefCell;
use std::fmt::{Arguments,write};
use std::io::{stdout,Stdout,Write};
use std::ascii::AsciiExt;

lazy_static! {
  pub static ref LOGGER: Mutex<Logger> = Mutex::new(Logger::new());
  pub static ref PID:    i32 = unsafe { libc::getpid() };
}


pub struct Logger {
  directives: Vec<LogDirective>,
}

impl Logger {
  pub fn new() -> Logger {
    Logger {
      directives: vec!(LogDirective {
        name:  None,
        level: LogLevelFilter::Error,
      })
    }
  }

  pub fn init(spec: &str) {
    let directives = parse_logging_spec(spec);
    LOGGER.lock().unwrap().set_directives(directives);
  }

  pub fn log<'a>(&mut self, meta: &LogMetadata, args: Arguments) {
    if self.enabled(meta) {
      stdout().write_fmt(args);
      stdout().write(b"\n");
    }
  }

  pub fn set_directives(&mut self, directives: Vec<LogDirective>) {
    self.directives = directives;
  }

  fn enabled(&self, meta: &LogMetadata) -> bool {
    // Search for the longest match, the vector is assumed to be pre-sorted.
    for directive in self.directives.iter().rev() {
      match directive.name {
        Some(ref name) if !meta.target.starts_with(&**name) => {},
        Some(..) | None => {
          return meta.level <= directive.level
        }
      }
    }
    false
  }
}

#[repr(usize)]
#[derive(Copy, Eq, Debug)]
pub enum LogLevel {
    /// The "error" level.
    ///
    /// Designates very serious errors.
    Error = 1, // This way these line up with the discriminants for LogLevelFilter below
    /// The "warn" level.
    ///
    /// Designates hazardous situations.
    Warn,
    /// The "info" level.
    ///
    /// Designates useful information.
    Info,
    /// The "debug" level.
    ///
    /// Designates lower priority information.
    Debug,
    /// The "trace" level.
    ///
    /// Designates very low priority, often extremely verbose, information.
    Trace,
}

static LOG_LEVEL_NAMES: [&'static str; 6] = ["OFF", "ERROR", "WARN", "INFO",
                                             "DEBUG", "TRACE"];

impl Clone for LogLevel {
    #[inline]
    fn clone(&self) -> LogLevel {
        *self
    }
}

impl PartialEq for LogLevel {
    #[inline]
    fn eq(&self, other: &LogLevel) -> bool {
        *self as usize == *other as usize
    }
}

impl PartialEq<LogLevelFilter> for LogLevel {
    #[inline]
    fn eq(&self, other: &LogLevelFilter) -> bool {
        *self as usize == *other as usize
    }
}

impl PartialOrd for LogLevel {
    #[inline]
    fn partial_cmp(&self, other: &LogLevel) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialOrd<LogLevelFilter> for LogLevel {
    #[inline]
    fn partial_cmp(&self, other: &LogLevelFilter) -> Option<cmp::Ordering> {
        Some((*self as usize).cmp(&(*other as usize)))
    }
}

impl Ord for LogLevel {
    #[inline]
    fn cmp(&self, other: &LogLevel) -> cmp::Ordering {
        (*self as usize).cmp(&(*other as usize))
    }
}

impl LogLevel {
    fn from_usize(u: usize) -> Option<LogLevel> {
        match u {
            1 => Some(LogLevel::Error),
            2 => Some(LogLevel::Warn),
            3 => Some(LogLevel::Info),
            4 => Some(LogLevel::Debug),
            5 => Some(LogLevel::Trace),
            _ => None
        }
    }

    /// Returns the most verbose logging level.
    #[inline]
    pub fn max() -> LogLevel {
        LogLevel::Trace
    }

    /// Converts the `LogLevel` to the equivalent `LogLevelFilter`.
    #[inline]
    pub fn to_log_level_filter(&self) -> LogLevelFilter {
        LogLevelFilter::from_usize(*self as usize).unwrap()
    }
}

#[repr(usize)]
#[derive(Copy, Eq, Debug)]
pub enum LogLevelFilter {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl Clone for LogLevelFilter {
    #[inline]
    fn clone(&self) -> LogLevelFilter {
        *self
    }
}

impl PartialEq for LogLevelFilter {
    #[inline]
    fn eq(&self, other: &LogLevelFilter) -> bool {
        *self as usize == *other as usize
    }
}

impl PartialEq<LogLevel> for LogLevelFilter {
    #[inline]
    fn eq(&self, other: &LogLevel) -> bool {
        other.eq(self)
    }
}

impl PartialOrd for LogLevelFilter {
    #[inline]
    fn partial_cmp(&self, other: &LogLevelFilter) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialOrd<LogLevel> for LogLevelFilter {
    #[inline]
    fn partial_cmp(&self, other: &LogLevel) -> Option<cmp::Ordering> {
        other.partial_cmp(self).map(|x| x.reverse())
    }
}

impl Ord for LogLevelFilter {
    #[inline]
    fn cmp(&self, other: &LogLevelFilter) -> cmp::Ordering {
        (*self as usize).cmp(&(*other as usize))
    }
}

impl FromStr for LogLevelFilter {
    type Err = ();
    fn from_str(level: &str) -> Result<LogLevelFilter, ()> {
        ok_or(LOG_LEVEL_NAMES.iter()
                    .position(|&name| name.eq_ignore_ascii_case(level))
                    .map(|p| LogLevelFilter::from_usize(p).unwrap()), ())
    }
}

impl LogLevelFilter {
    fn from_usize(u: usize) -> Option<LogLevelFilter> {
        match u {
            0 => Some(LogLevelFilter::Off),
            1 => Some(LogLevelFilter::Error),
            2 => Some(LogLevelFilter::Warn),
            3 => Some(LogLevelFilter::Info),
            4 => Some(LogLevelFilter::Debug),
            5 => Some(LogLevelFilter::Trace),
            _ => None
        }
    }
    /// Returns the most verbose logging level filter.
    #[inline]
    pub fn max() -> LogLevelFilter {
        LogLevelFilter::Trace
    }

    /// Converts `self` to the equivalent `LogLevel`.
    ///
    /// Returns `None` if `self` is `LogLevelFilter::Off`.
    #[inline]
    pub fn to_log_level(&self) -> Option<LogLevel> {
        LogLevel::from_usize(*self as usize)
    }
}

/// Metadata about a log message.
pub struct LogMetadata {
  pub level:  LogLevel,
  pub target: &'static str,
}

pub struct LogDirective {
    name:  Option<String>,
    level: LogLevelFilter,
}

fn ok_or<T, E>(t: Option<T>, e: E) -> Result<T, E> {
    match t {
        Some(t) => Ok(t),
        None => Err(e),
    }
}

pub fn parse_logging_spec(spec: &str) -> Vec<LogDirective> {
    let mut dirs = Vec::new();

    let mut parts = spec.split('/');
    let mods = parts.next();
    let filter = parts.next();
    if parts.next().is_some() {
        println!("warning: invalid logging spec '{}', \
                 ignoring it (too many '/'s)", spec);
        return dirs;
    }
    mods.map(|m| { for s in m.split(',') {
        if s.len() == 0 { continue }
        let mut parts = s.split('=');
        let (log_level, name) = match (parts.next(), parts.next().map(|s| s.trim()), parts.next()) {
            (Some(part0), None, None) => {
                // if the single argument is a log-level string or number,
                // treat that as a global fallback
                match part0.parse() {
                    Ok(num) => (num, None),
                    Err(_) => (LogLevelFilter::max(), Some(part0)),
                }
            }
            (Some(part0), Some(""), None) => (LogLevelFilter::max(), Some(part0)),
            (Some(part0), Some(part1), None) => {
                match part1.parse() {
                    Ok(num) => (num, Some(part0)),
                    _ => {
                        println!("warning: invalid logging spec '{}', \
                                 ignoring it", part1);
                        continue
                    }
                }
            },
            _ => {
                println!("warning: invalid logging spec '{}', \
                         ignoring it", s);
                continue
            }
        };
        dirs.push(LogDirective {
            name: name.map(|s| s.to_string()),
            level: log_level,
        });
    }});

    return dirs;
}

#[macro_export]
macro_rules! log {
    (target: $target:expr, $lvl:expr, $($arg:tt)+) => ({
      use $crate::logging::LOGGER;
      static _META: $crate::logging::LogMetadata = $crate::logging::LogMetadata {
          level:  $lvl,
          target: module_path!(),
      };
      LOGGER.lock().unwrap().log(&_META, format_args!($($arg)+));
    });
    ($lvl:expr, $($arg:tt)+) => (log!(target: module_path!(), $lvl, $($arg)+));
}

#[macro_export]
macro_rules! error {
    ($format:expr, $($arg:tt)*) => {
        log!($crate::logging::LogLevel::Error, concat!("{}\t{}\t{}\t{}\t", $format),
          ::time::now_utc().rfc3339(), ::time::precise_time_ns(), *$crate::logging::PID,
          "ERROR", $($arg)*);
    };
    ($format:expr) => {
        log!($crate::logging::LogLevel::Error, concat!("{}\t{}\t{}\t{}\t", $format),
          ::time::now_utc().rfc3339(), ::time::precise_time_ns(), *$crate::logging::PID,
          "ERROR");
    };
}

#[macro_export]
macro_rules! warn {
    ($format:expr, $($arg:tt)*) => {
        use time;
        use logging::PID;
        log!($crate::logging::LogLevel::Warn, concat!("{}\t{}\t{}\t{}\t", $format),
          ::time::now_utc().rfc3339(), ::time::precise_time_ns(), *$crate::logging::PID,
          "WARN", $($arg)*);
    };
    ($format:expr) => {
        log!($crate::logging::LogLevel::Warn, concat!("{}\t{}\t{}\t{}\t", $format),
          ::time::now_utc().rfc3339(), ::time::precise_time_ns(), *$crate::logging::PID,
          "WARN");
    }
}

#[macro_export]
macro_rules! info {
    ($format:expr, $($arg:tt)*) => {
        log!($crate::logging::LogLevel::Info, concat!("{}\t{}\t{}\t{}\t", $format),
          ::time::now_utc().rfc3339(), ::time::precise_time_ns(), *$crate::logging::PID,
          "INFO", $($arg)*);
    };
    ($format:expr) => {
        log!($crate::logging::LogLevel::Info, concat!("{}\t{}\t{}\t{}\t", $format),
          ::time::now_utc().rfc3339(), ::time::precise_time_ns(), *$crate::logging::PID,
          "INFO");
    }
}

#[macro_export]
macro_rules! debug {
    ($format:expr, $($arg:tt)*) => {
        log!($crate::logging::LogLevel::Debug, concat!("{}\t{}\t{}\t{}\t{}\t", $format),
          ::time::now_utc().rfc3339(), ::time::precise_time_ns(), *$crate::logging::PID,
          "DEBUG", module_path!(), $($arg)*);
    };
    ($format:expr) => {
        log!($crate::logging::LogLevel::Debug, concat!("{}\t{}\t{}\t{}\t{}\t", $format),
          ::time::now_utc().rfc3339(), ::time::precise_time_ns(), *$crate::logging::PID,
          "DEBUG", module_path!());
    }
}

#[macro_export]
macro_rules! trace {
    ($format:expr, $($arg:tt)*) => (
        log!($crate::logging::LogLevel::Trace, concat!("{}\t{}\t{}\t{}\t{}\t", $format),
          ::time::now_utc().rfc3339(), ::time::precise_time_ns(), *$crate::logging::PID,
          "TRACE", module_path!(), $($arg)*);
    );
    ($format:expr) => (
        log!($crate::logging::LogLevel::Trace, concat!("{}\t{}\t{}\t{}\t{}\t", $format),
          ::time::now_utc().rfc3339(), ::time::precise_time_ns(), *$crate::logging::PID,
          "TRACE", module_path!());
    )
}

