use libc;
use std::fs::File;
use std::str::FromStr;
use std::cell::RefCell;
use std::cmp::{self,Ord};
use std::fmt::{Arguments,format};
use std::io::{stdout,Stdout,Write};
use std::net::{SocketAddr,UdpSocket};
use std::net::TcpStream;
use mio::net::UnixDatagram;

thread_local! {
  pub static LOGGER: RefCell<Logger> = RefCell::new(Logger::new());
  pub static TAG:    String          = LOGGER.with(|logger| (*logger.borrow()).tag.clone());
}

pub static COMPAT_LOGGER: CompatLogger = CompatLogger;

pub struct Logger {
  pub directives:     Vec<LogDirective>,
  pub backend:        LoggerBackend,
  pub access_backend: Option<LoggerBackend>,
  pub tag:            String,
  pub pid:            i32,
  pub initialized:    bool,
}

impl Logger {
  pub fn new() -> Logger {
    Logger {
      directives: vec!(LogDirective {
        name:  None,
        level: LogLevelFilter::Error,
      }),
      backend:        LoggerBackend::Stdout(stdout()),
      access_backend: None,
      tag:            "SOZU".to_string(),
      pid:            0,
      initialized:    false,
    }
  }

  pub fn init(tag: String, spec: &str, backend: LoggerBackend, access_backend: Option<LoggerBackend>) {
    let directives = parse_logging_spec(spec);
    LOGGER.with(|l| {
      let logger = &mut (*l.borrow_mut());
      if !logger.initialized {
        logger.set_directives(directives);
        logger.backend        = backend;
        logger.access_backend = access_backend;
        logger.tag            = tag;
        logger.pid            = unsafe { libc::getpid() };
        logger.initialized    = true;

        let _ = log::set_logger(&COMPAT_LOGGER).map_err(|e| println!("could not register compat logger: {:?}", e));
        log::set_max_level(log::LevelFilter::Info);
      }
    });

  }

  pub fn log(&mut self, meta: &Metadata, args: Arguments) {
    if self.enabled(meta) {
      match self.backend {
        LoggerBackend::Stdout(ref mut stdout) => {
          let _ = stdout.write_fmt(args);
        },
        //FIXME: should have a buffer to write to instead of allocating a string
        LoggerBackend::Unix(ref mut socket) => {
          let _ = socket.send(format(args).as_bytes()).map_err(|e| {
            println!("cannot write logs to Unix socket: {:?}", e);
          });
        },
        //FIXME: should have a buffer to write to instead of allocating a string
        LoggerBackend::Udp(ref mut socket, ref address) => {
          let _ = socket.send_to(format(args).as_bytes(), address).map_err(|e| {
            println!("cannot write logs to UDP socket: {:?}", e);
          });
        }
        LoggerBackend::Tcp(ref mut socket) => {
          let _ = socket.write_fmt(args).map_err(|e| {
            println!("cannot write logs to TCP socket: {:?}", e);
          });
        },
        LoggerBackend::File(ref mut file) => {
          let _ = file.write_fmt(args).map_err(|e| {
            println!("cannot write logs to file: {:?}", e);
          });
        },
      }
    }
  }

  pub fn log_access(&mut self, meta: &Metadata, args: Arguments) {
    if self.enabled(meta) {
      let backend = self.access_backend.as_mut().unwrap_or(&mut self.backend);
      match *backend {
        LoggerBackend::Stdout(ref mut stdout) => {
          let _ = stdout.write_fmt(args);
        },
        //FIXME: should have a buffer to write to instead of allocating a string
        LoggerBackend::Unix(ref mut socket) => {
          let _ = socket.send(format(args).as_bytes()).map_err(|e| {
            println!("cannot write logs to Unix socket: {:?}", e);
          });
        },
        //FIXME: should have a buffer to write to instead of allocating a string
        LoggerBackend::Udp(ref mut socket, ref address) => {
          let _ = socket.send_to(format(args).as_bytes(), address).map_err(|e| {
            println!("cannot write logs to UDP socket: {:?}", e);
          });
        }
        LoggerBackend::Tcp(ref mut socket) => {
          let _ = socket.write_fmt(args).map_err(|e| {
            println!("cannot write logs to TCP socket: {:?}", e);
          });
        },
        LoggerBackend::File(ref mut file) => {
          let _ = file.write_fmt(args).map_err(|e| {
            println!("cannot write logs to file: {:?}", e);
          });
        },
      }
    }
  }

  pub fn compat_log(&mut self, meta: &log::Metadata, args: Arguments) {
    if self.compat_enabled(meta) {
      match self.backend {
        LoggerBackend::Stdout(ref mut stdout) => {
          let _ = stdout.write_fmt(args);
        },
        //FIXME: should have a buffer to write to instead of allocating a string
        LoggerBackend::Unix(ref mut socket) => {
          let _= socket.send(format(args).as_bytes()).map_err(|e| {
            println!("cannot write logs to Unix socket: {:?}", e);
          });
        },
        //FIXME: should have a buffer to write to instead of allocating a string
        LoggerBackend::Udp(ref mut socket, ref address) => {
          let _ = socket.send_to(format(args).as_bytes(), address).map_err(|e| {
            println!("cannot write logs to UDP socket: {:?}", e);
          });
        }
        LoggerBackend::Tcp(ref mut socket) => {
          let _ = socket.write_fmt(args).map_err(|e| {
            println!("cannot write logs to TCP socket: {:?}", e);
          });
        },
        LoggerBackend::File(ref mut file) => {
          let _ = file.write_fmt(args).map_err(|e| {
            println!("cannot write logs to file: {:?}", e);
          });
        },
      }
    }
  }

  pub fn set_directives(&mut self, directives: Vec<LogDirective>) {
    self.directives = directives;
  }

  fn enabled(&self, meta: &Metadata) -> bool {
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

  fn compat_enabled(&self, meta: &log::Metadata) -> bool {
    // Search for the longest match, the vector is assumed to be pre-sorted.
    for directive in self.directives.iter().rev() {
      match directive.name {
        Some(ref name) if !meta.target().starts_with(&**name) => {},
        Some(..) | None => {
          let lvl: LogLevel = meta.level().into();
          return lvl <= directive.level
        }
      }
    }
    false
  }
}

pub enum LoggerBackend {
  Stdout(Stdout),
  Unix(UnixDatagram),
  Udp(UdpSocket, SocketAddr),
  Tcp(TcpStream),
  File(crate::writer::MultiLineWriter<File>),
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
    pub fn to_log_level_filter(self) -> LogLevelFilter {
        LogLevelFilter::from_usize(self as usize).unwrap()
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
    pub fn to_log_level(self) -> Option<LogLevel> {
        LogLevel::from_usize(self as usize)
    }
}

/// Metadata about a log message.
pub struct Metadata {
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
    let _    = parts.next();
    if parts.next().is_some() {
        println!("warning: invalid logging spec '{}', \
                 ignoring it (too many '/'s)", spec);
        return dirs;
    }
    mods.map(|m| { for s in m.split(',') {
        if s.is_empty() { continue }
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

    dirs
}

#[macro_export]
macro_rules! log {
    (__inner__ $target:expr, $lvl:expr, $format:expr, $level_tag:expr,
     [$($transformed_args:ident),*], [$first_ident:ident $(, $other_idents:ident)*], $first_arg:expr $(, $other_args:expr)*) => ({
      let $first_ident = &$first_arg;
      log!(__inner__ $target, $lvl, $format, $level_tag, [$($transformed_args,)* $first_ident], [$($other_idents),*] $(, $other_args)*);
    });

    (__inner__ $target:expr, $lvl:expr, $format:expr, $level_tag:expr,
     [$($final_args:ident),*], [$($idents:ident),*]) => ({
      static _META: $crate::logging::Metadata = $crate::logging::Metadata {
          level:  $lvl,
          target: module_path!(),
      };
      {
        $crate::logging::TAG.with(|tag| {
          //let tag = t.borrow().tag;
          $crate::logging::LOGGER.with(|l| {
            let pid = l.borrow().pid;

            let (now, precise_time) = $crate::logging::now();
            l.borrow_mut().log(
              &_META,
              format_args!(
                concat!("{} {} {} {} {}\t", $format, '\n'),
                now, precise_time, pid, tag,
                $level_tag $(, $final_args)*)
            );
          })
        });
      }
    });
    ($lvl:expr, $format:expr, $level_tag:expr $(, $args:expr)+) => {
      log!(__inner__ module_path!(), $lvl, $format, $level_tag, [], [a,b,c,d,e,f,g,h,i,j,k,l,m,n,o,p,q,r,s,t,u,v]
                  $(, $args)+)
    };
    ($lvl:expr, $format:expr, $level_tag:expr) => {
      log!(__inner__ module_path!(), $lvl, $format, $level_tag, [], [a,b,c,d,e,f,g,h,i,j,k,l,m,n,o,p,q,r,s,t,u,v])
    };
}

#[macro_export]
macro_rules! log_access {
    (__inner__ $target:expr, $lvl:expr, $format:expr, $level_tag:expr,
     [$($transformed_args:ident),*], [$first_ident:ident $(, $other_idents:ident)*], $first_arg:expr $(, $other_args:expr)*) => ({
      let $first_ident = &$first_arg;
      log_access!(__inner__ $target, $lvl, $format, $level_tag, [$($transformed_args,)* $first_ident], [$($other_idents),*] $(, $other_args)*);
    });

    (__inner__ $target:expr, $lvl:expr, $format:expr, $level_tag:expr,
     [$($final_args:ident),*], [$($idents:ident),*]) => ({
      static _META: $crate::logging::Metadata = $crate::logging::Metadata {
          level:  $lvl,
          target: module_path!(),
      };
      {
        $crate::logging::TAG.with(|tag| {
          //let tag = t.borrow().tag;
          $crate::logging::LOGGER.with(|l| {
            let pid = l.borrow().pid;

            let (now, precise_time) = $crate::logging::now();
            l.borrow_mut().log_access(
              &_META,
              format_args!(
                concat!("{} {} {} {} {}\t", $format, '\n'),
                now, precise_time, pid, tag,
                $level_tag $(, $final_args)*)
            );
          })
        });
      }
    });
    ($lvl:expr, $format:expr, $level_tag:expr $(, $args:expr)+) => {
      log_access!(__inner__ module_path!(), $lvl, $format, $level_tag, [], [a,b,c,d,e,f,g,h,i,j,k,l,m,n,o,p,q,r,s,t,u,v]
                  $(, $args)+)
    };
    ($lvl:expr, $format:expr, $level_tag:expr) => {
      log_access!(__inner__ module_path!(), $lvl, $format, $level_tag, [], [a,b,c,d,e,f,g,h,i,j,k,l,m,n,o,p,q,r,s,t,u,v])
    };
}

#[macro_export]
macro_rules! error {
    ($format:expr, $($arg:tt)*) => {
        log!($crate::logging::LogLevel::Error, $format, "ERROR", $($arg)*);
    };
    ($format:expr) => {
        log!($crate::logging::LogLevel::Error, $format, "ERROR");
    };
}

#[macro_export]
macro_rules! error_access {
    ($format:expr, $($arg:tt)*) => {
        log_access!($crate::logging::LogLevel::Error, $format, "ERROR", $($arg)*);
    };
    ($format:expr) => {
        log_access!($crate::logging::LogLevel::Error, $format, "ERROR");
    };
}

#[macro_export]
macro_rules! warn {
    ($format:expr, $($arg:tt)*) => {
        use time;
        log!($crate::logging::LogLevel::Warn, $format, "WARN", $($arg)*);
    };
    ($format:expr) => {
        log!($crate::logging::LogLevel::Warn, $format, "WARN");
    }
}

#[macro_export]
macro_rules! info {
    ($format:expr, $($arg:tt)*) => {
        log!($crate::logging::LogLevel::Info, $format, "INFO", $($arg)*);
    };
    ($format:expr) => {
        log!($crate::logging::LogLevel::Info, $format, "INFO");
    }
}

#[macro_export]
macro_rules! info_access {
    ($format:expr, $($arg:tt)*) => {
        log_access!($crate::logging::LogLevel::Info, $format, "INFO", $($arg)*);
    };
    ($format:expr) => {
        log_access!($crate::logging::LogLevel::Info, $format, "INFO");
    }
}

#[macro_export]
macro_rules! debug {
    ($format:expr, $($arg:tt)*) => {
        #[cfg(any(debug_assertions, feature = "logs-debug", feature = "logs-trace"))]
        log!($crate::logging::LogLevel::Debug, concat!("{}\t", $format),
          "DEBUG", {module_path!()}, $($arg)*);
    };
    ($format:expr) => {
        #[cfg(any(debug_assertions, feature = "logs-debug", feature = "logs-trace"))]
        log!($crate::logging::LogLevel::Debug, concat!("{}\t", $format),
          "DEBUG", {module_path!()});
    }
}

#[macro_export]
macro_rules! trace {
    ($format:expr, $($arg:tt)*) => (
        #[cfg(any(debug_assertions, feature = "logs-trace"))]
        log!($crate::logging::LogLevel::Trace, concat!("{}\t", $format),
          "TRACE", module_path!(), $($arg)*);
    );
    ($format:expr) => (
        #[cfg(any(debug_assertions, feature = "logs-trace"))]
        log!($crate::logging::LogLevel::Trace, concat!("{}\t", $format),
          "TRACE", module_path!());
    )
}


#[macro_export]
macro_rules! fixme {
    () => {
        log!($crate::logging::LogLevel::Info, "FIXME: {}:{} in {}", "INFO", file!(), line!(), module_path!());
    };
    ($($arg:tt)*) => {
        log!($crate::logging::LogLevel::Info, "FIXME: {}:{} in {}: {}", "INFO", file!(), line!(), module_path!(), $($arg)*);
    };
}

use crate::log;
pub struct CompatLogger;

impl From<log::Level> for LogLevel {
  fn from(lvl: log::Level) -> Self {
    match lvl {
      log::Level::Error => LogLevel::Error,
      log::Level::Warn  => LogLevel::Warn,
      log::Level::Info  => LogLevel::Info,
      log::Level::Debug => LogLevel::Debug,
      log::Level::Trace => LogLevel::Trace,
    }
  }
}

impl log::Log for CompatLogger {
  fn enabled(&self, _: &log::Metadata) -> bool {
    true
  }

  fn log(&self, record: &log::Record) {

    TAG.with(|tag| {
      LOGGER.with(|l| {
        let pid = l.borrow().pid;
        let (now, precise_time) = now();
        l.borrow_mut().compat_log(
          record.metadata(),
          format_args!(
            concat!("{} {} {} {} {}\t{}\n"),
            now, precise_time, pid, tag,
            record.level(), record.args())
        );
      })
    });
  }

  fn flush(&self) {}
}

#[macro_export]
macro_rules! setup_test_logger {
  () => (
    $crate::logging::Logger::init(module_path!().to_string(), "error", $crate::logging::LoggerBackend::Stdout(::std::io::stdout()), None);
  );
}

pub struct Rfc3339Time {
  inner: ::time::OffsetDateTime,
}

impl std::fmt::Display for Rfc3339Time {
  fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
    let t = self.inner;
    write!(f, "{}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
      t.year(), t.month(), t.day(),
      t.hour(), t.minute(), t.second(),
      t.microsecond()
    )
  }
}

pub fn now() -> (Rfc3339Time, i128) {
  let t = time::OffsetDateTime::now_utc();
  (Rfc3339Time { inner: t, }, (t - time::OffsetDateTime::unix_epoch()).whole_nanoseconds())
}
