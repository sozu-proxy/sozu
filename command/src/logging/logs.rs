use std::{
    cell::RefCell,
    cmp,
    env,
    fmt::Arguments,
    fs::{File, OpenOptions},
    io::{stdout, Error as IoError, ErrorKind as IoErrorKind, Stdout, Write},
    net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket},
    ops::{Deref, DerefMut},
    path::Path,
    str::FromStr,
};

use mio::net::UnixDatagram;
use prost::{encoding::encoded_len_varint, Message};
use rand::{distributions::Alphanumeric, thread_rng, Rng};

use crate::{
    config::Config,
    logging::{LogDuration, LogMessage, RequestRecord},
    proto::command::ProtobufAccessLogFormat,
    AsString,
};

thread_local! {
  pub static LOGGER: RefCell<Logger> = RefCell::new(Logger::new());
}

// TODO: check if this error is critical:
//     could not register compat logger: SetLoggerError(())
// The CompatLogger may need a variable that tells wether it has been initiated already
pub static COMPAT_LOGGER: CompatLogger = CompatLogger;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "lowercase")]
pub enum AccessLogFormat {
    Ascii,
    Protobuf,
}

impl From<&ProtobufAccessLogFormat> for AccessLogFormat {
    fn from(value: &ProtobufAccessLogFormat) -> Self {
        match value {
            ProtobufAccessLogFormat::Ascii => Self::Ascii,
            ProtobufAccessLogFormat::Protobuf => Self::Protobuf,
        }
    }
}

impl From<&Option<AccessLogFormat>> for ProtobufAccessLogFormat {
    fn from(value: &Option<AccessLogFormat>) -> Self {
        match value {
            Some(AccessLogFormat::Ascii) | None => Self::Ascii,
            Some(AccessLogFormat::Protobuf) => Self::Protobuf,
        }
    }
}

pub struct InnerLogger {
    version: u8,
    directives: Vec<LogDirective>,
    backend: LoggerBackend,
    pub colored: bool,
    /// target of the access logs
    access_backend: Option<LoggerBackend>,
    /// how to format the access logs
    access_format: AccessLogFormat,
    access_colored: bool,
    buffer: LoggerBuffer,
}

pub struct Logger {
    inner: InnerLogger,
    /// is displayed in each log, for instance "MAIN" or worker_id
    tag: String,
    /// the pid of the current process (main or worker)
    pid: i32,
    initialized: bool,
}

impl std::ops::Deref for Logger {
    type Target = InnerLogger;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl std::ops::DerefMut for Logger {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl Default for Logger {
    fn default() -> Self {
        Self {
            inner: InnerLogger {
                version: 1, // all call site start with a version of 0
                directives: vec![LogDirective {
                    name: None,
                    level: LogLevelFilter::Error,
                }],
                backend: LoggerBackend::Stdout(stdout()),
                colored: false,
                access_backend: None,
                access_format: AccessLogFormat::Ascii,
                access_colored: false,
                buffer: LoggerBuffer(Vec::with_capacity(4096)),
            },
            tag: "UNINITIALIZED".to_string(),
            pid: 0,
            initialized: false,
        }
    }
}

impl Logger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn init(
        tag: String,
        spec: &str,
        backend: LoggerBackend,
        colored: bool,
        access_backend: Option<LoggerBackend>,
        access_format: Option<AccessLogFormat>,
        access_colored: Option<bool>,
    ) {
        let (directives, _errors) = parse_logging_spec(spec);
        LOGGER.with(|logger| {
            let mut logger = logger.borrow_mut();
            if !logger.initialized {
                logger.set_directives(directives);
                logger.colored = match backend {
                    LoggerBackend::Stdout(_) => colored,
                    _ => false,
                };
                logger.access_colored = match (&access_backend, &backend) {
                    (Some(LoggerBackend::Stdout(_)), _) | (None, LoggerBackend::Stdout(_)) => {
                        access_colored.unwrap_or(colored)
                    }
                    _ => false,
                };
                logger.backend = backend;
                logger.access_backend = access_backend;
                logger.access_format = access_format.unwrap_or(AccessLogFormat::Ascii);
                logger.tag = tag;
                logger.pid = unsafe { libc::getpid() };
                logger.initialized = true;

                let _ = log::set_logger(&COMPAT_LOGGER)
                    .map_err(|e| println!("could not register compat logger: {e:?}"));

                log::set_max_level(log::LevelFilter::Info);
            }
        });
    }

    pub fn set_directives(&mut self, directives: Vec<LogDirective>) {
        self.version += 1;
        if self.version >= LOG_LINE_ENABLED {
            self.version = 0;
        }
        self.directives = directives;
    }

    pub fn split(&mut self) -> (i32, &str, &mut InnerLogger) {
        (self.pid, &self.tag, &mut self.inner)
    }
}

struct LoggerBuffer(Vec<u8>);

impl Deref for LoggerBuffer {
    type Target = Vec<u8>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl DerefMut for LoggerBuffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl LoggerBuffer {
    fn fmt<F: FnOnce(&[u8]) -> Result<usize, IoError>>(
        &mut self,
        args: Arguments,
        flush: F,
    ) -> Result<(), IoError> {
        self.clear();
        self.write_fmt(args)?;
        flush(self.as_slice())?;
        Ok(())
    }
}

fn log_arguments(
    args: Arguments,
    backend: &mut LoggerBackend,
    buffer: &mut LoggerBuffer,
) -> Result<(), IoError> {
    match backend {
        LoggerBackend::Stdout(stdout) => {
            let _ = stdout.write_fmt(args);
            Ok(())
        }
        LoggerBackend::Tcp(socket) => socket.write_fmt(args),
        LoggerBackend::File(file) => file.write_fmt(args),
        LoggerBackend::Unix(socket) => buffer.fmt(args, |bytes| socket.send(bytes)),
        LoggerBackend::Udp(sock, addr) => buffer.fmt(args, |b| sock.send_to(b, *addr)),
    }
}

impl InnerLogger {
    pub fn log(&mut self, args: Arguments) {
        if let Err(e) = log_arguments(args, &mut self.backend, &mut self.buffer) {
            println!("Could not write log to {}: {e:?}", self.backend.as_ref());
        }
    }

    /// write an access log to the proper logging target
    ///
    /// Protobuf access logs are written with a prost length delimiter before, and 2 empty bytes after
    pub fn log_access(&mut self, log: RequestRecord) {
        let backend = self.access_backend.as_mut().unwrap_or(&mut self.backend);

        let io_result = match self.access_format {
            AccessLogFormat::Protobuf => {
                let binary_log = log.into_binary_access_log();
                let log_length = binary_log.encoded_len();
                let total_length = log_length + encoded_len_varint(log_length as u64);
                self.buffer.clear();
                let current_capacity = self.buffer.capacity();
                if current_capacity < total_length {
                    self.buffer.reserve(total_length - current_capacity);
                }

                if let Err(e) = binary_log.encode_length_delimited(&mut self.buffer.0) {
                    Err(IoError::new(IoErrorKind::InvalidData, e))
                } else {
                    self.buffer.extend_from_slice(&[0, 0]); // add two empty bytes after each protobuf access log
                    let bytes = &self.buffer;
                    match backend {
                        LoggerBackend::Stdout(stdout) => {
                            let _ = stdout.write(bytes);
                            return;
                        }
                        LoggerBackend::Tcp(socket) => socket.write(bytes),
                        LoggerBackend::File(file) => file.write(bytes),
                        LoggerBackend::Unix(socket) => socket.send(bytes),
                        LoggerBackend::Udp(socket, address) => socket.send_to(bytes, *address),
                    }
                    .map(|_| ())
                }
            }
            AccessLogFormat::Ascii => crate::_prompt_log! {
                logger: |args| log_arguments(args, backend, &mut self.buffer),
                is_access: true,
                condition: self.access_colored,
                prompt: [
                    log.now,
                    log.precise_time,
                    log.pid,
                    log.level,
                    log.tag,
                ],
                standard: {
                    formats: ["{} {} {} {}/{}/{}/{} {} {} [{}] {} {}{}\n"],
                    args: [
                        log.context,
                        log.session_address.as_string_or("-"),
                        log.backend_address.as_string_or("-"),
                        LogDuration(Some(log.response_time)),
                        LogDuration(Some(log.service_time)),
                        LogDuration(log.client_rtt),
                        LogDuration(log.server_rtt),
                        log.bytes_in,
                        log.bytes_out,
                        log.full_tags(),
                        log.protocol,
                        log.endpoint,
                        LogMessage(log.message),
                    ]
                },
                colored: {
                    formats: ["\x1b[;1m{}\x1b[m {} {} {}/{}/{}/{} {} {} \x1b[2m[{}] \x1b[;1m{} {:#}\x1b[m{}\n"],
                    args: @,
                }
            },
        };

        if let Err(e) = io_result {
            println!("Could not write access log to {}: {e:?}", backend.as_ref());
        }
    }

    pub fn enabled(&self, meta: Metadata) -> bool {
        // Search for the longest match, the vector is assumed to be pre-sorted.
        for directive in self.directives.iter().rev() {
            match &directive.name {
                Some(name) if !meta.target.starts_with(name) => {}
                Some(_) | None => return meta.level <= directive.level,
            }
        }
        false
    }

    pub fn cached_enabled(&self, call_site_state: &mut LogLineCachedState, meta: Metadata) -> bool {
        if call_site_state.version() == self.version {
            call_site_state.enabled()
        } else {
            let enabled = self.enabled(meta);
            call_site_state.set(self.version, enabled);
            enabled
        }
    }

    fn compat_enabled(&self, meta: &log::Metadata) -> bool {
        // Search for the longest match, the vector is assumed to be pre-sorted.
        for directive in self.directives.iter().rev() {
            match &directive.name {
                Some(name) if !meta.target().starts_with(name) => {}
                Some(_) | None => return LogLevel::from(meta.level()) <= directive.level,
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
#[derive(Clone, Copy, Eq, Debug)]
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

static LOG_LEVEL_NAMES: [&str; 6] = ["OFF", "ERROR", "WARN", "INFO", "DEBUG", "TRACE"];

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
            _ => None,
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
#[derive(Clone, Copy, Eq, Debug)]
pub enum LogLevelFilter {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
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
        LOG_LEVEL_NAMES
            .iter()
            .position(|&name| name.eq_ignore_ascii_case(level))
            .map(|p| LogLevelFilter::from_usize(p).unwrap())
            .ok_or(())
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
            _ => None,
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
#[derive(Debug)]
pub struct Metadata {
    pub level: LogLevel,
    pub target: &'static str,
}

#[derive(Debug)]
pub struct LogDirective {
    name: Option<String>,
    level: LogLevelFilter,
}

#[derive(thiserror::Error, Debug)]
pub enum LogSpecParseError {
    #[error("Too many '/'s: {0}")]
    TooManySlashes(String),
    #[error("Too many '='s: {0}")]
    TooManyEquals(String),
    #[error("Invalid log level: {0}")]
    InvalidLogLevel(String),
}

pub fn parse_logging_spec(spec: &str) -> (Vec<LogDirective>, Vec<LogSpecParseError>) {
    let mut dirs = Vec::new();
    let mut errors = Vec::new();

    let mut parts = spec.split('/');
    let mods = parts.next();
    let _ = parts.next();
    if parts.next().is_some() {
        errors.push(LogSpecParseError::TooManySlashes(spec.to_string()));
    }
    if let Some(m) = mods {
        for s in m.split(',') {
            if s.is_empty() {
                continue;
            }
            let mut parts = s.split('=');
            let (log_level, name) =
                match (parts.next(), parts.next().map(|s| s.trim()), parts.next()) {
                    (Some(part0), None, None) => {
                        // if the single argument is a log-level string or number,
                        // treat that as a global fallback
                        match part0.parse() {
                            Ok(num) => (num, None),
                            Err(_) => (LogLevelFilter::max(), Some(part0)),
                        }
                    }
                    (Some(part0), Some(""), None) => (LogLevelFilter::max(), Some(part0)),
                    (Some(part0), Some(part1), None) => match part1.parse() {
                        Ok(num) => (num, Some(part0)),
                        _ => {
                            errors.push(LogSpecParseError::InvalidLogLevel(s.to_string()));
                            continue;
                        }
                    },
                    _ => {
                        errors.push(LogSpecParseError::TooManyEquals(s.to_string()));
                        continue;
                    }
                };
            dirs.push(LogDirective {
                name: name.map(|s| s.to_string()),
                level: log_level,
            });
        }
    }

    for error in &errors {
        println!("{error:?}");
    }
    (dirs, errors)
}

/// start the logger with all logs and access logs on stdout
pub fn setup_default_logging(log_colored: bool, log_level: &str, tag: &str) {
    setup_logging("stdout", log_colored, None, None, None, log_level, tag)
}

/// start the logger from config (takes RUST_LOG into account)
pub fn setup_logging_with_config(config: &Config, tag: &str) {
    setup_logging(
        &config.log_target,
        config.log_colored,
        config.access_logs_target.as_deref(),
        config.access_logs_format.clone(),
        config.access_logs_colored,
        &config.log_level,
        tag,
    )
}

/// start the logger, after:
///
/// - determining logging backends
/// - taking RUST_LOG into account
pub fn setup_logging(
    log_target: &str,
    log_colored: bool,
    access_logs_target: Option<&str>,
    access_logs_format: Option<AccessLogFormat>,
    access_logs_colored: Option<bool>,
    log_level: &str,
    tag: &str,
) {
    let backend = target_to_backend(log_target);
    let access_backend = access_logs_target.map(target_to_backend);

    Logger::init(
        tag.to_string(),
        env::var("RUST_LOG").as_deref().unwrap_or(log_level),
        backend,
        log_colored,
        access_backend,
        access_logs_format,
        access_logs_colored,
    );
}

pub fn target_to_backend(target: &str) -> LoggerBackend {
    if target == "stdout" {
        LoggerBackend::Stdout(stdout())
    } else if let Some(addr) = target.strip_prefix("udp://") {
        match addr.to_socket_addrs() {
            Err(e) => {
                println!("invalid log target configuration ({e:?}): {target}");
                LoggerBackend::Stdout(stdout())
            }
            Ok(mut addrs) => {
                let socket = UdpSocket::bind(("0.0.0.0", 0)).unwrap();
                LoggerBackend::Udp(socket, addrs.next().unwrap())
            }
        }
    } else if let Some(addr) = target.strip_prefix("tcp://") {
        match addr.to_socket_addrs() {
            Err(e) => {
                println!("invalid log target configuration ({e:?}): {target}");
                LoggerBackend::Stdout(stdout())
            }
            Ok(mut addrs) => LoggerBackend::Tcp(TcpStream::connect(addrs.next().unwrap()).unwrap()),
        }
    } else if let Some(addr) = target.strip_prefix("unix://") {
        let path = Path::new(addr);
        if !path.exists() {
            println!("invalid log target configuration: {addr} is not a file");
            LoggerBackend::Stdout(stdout())
        } else {
            let mut dir = env::temp_dir();
            let s: String = thread_rng()
                .sample_iter(&Alphanumeric)
                .take(12)
                .map(|c| c as char)
                .collect();
            dir.push(s);
            let socket = UnixDatagram::bind(dir).unwrap();
            socket.connect(path).unwrap();
            LoggerBackend::Unix(socket)
        }
    } else if let Some(addr) = target.strip_prefix("file://") {
        let path = Path::new(addr);
        match OpenOptions::new().create(true).append(true).open(path) {
            Ok(file) => LoggerBackend::File(crate::writer::MultiLineWriter::new(file)),
            Err(e) => {
                println!(
                    "invalid log target configuration: could not open file at {addr} (error: {e:?})"
                );
                LoggerBackend::Stdout(stdout())
            }
        }
    } else {
        println!("invalid log target configuration: {target}");
        LoggerBackend::Stdout(stdout())
    }
}

#[macro_export]
macro_rules! _prompt_log {
    {
        logger: $logger:expr,
        is_access: $access:expr,
        condition: $cond:expr,
        prompt: [$($p:tt)*],
        standard: {$($std:tt)*}$(,)?
    } => {
        $crate::_prompt_log!{
            logger: $logger,
            is_access: $access,
            condition: $cond,
            prompt: [$($p)*],
            standard: {$($std)*},
            colored: {$($std)*},
        }
    };
    {
        logger: $logger:expr,
        is_access: $access:expr,
        condition: $cond:expr,
        prompt: [$($p:tt)*],
        standard: {
            formats: [$($std_fmt:tt)*],
            args: [$($std_args:expr),*$(,)?]$(,)?
        },
        colored: {
            formats: [$($col_fmt:tt)*],
            args: @$(,)?
        }$(,)?
    } => {
        $crate::_prompt_log!{
            logger: $logger,
            is_access: $access,
            condition: $cond,
            prompt: [$($p)*],
            standard: {
                formats: [$($std_fmt)*],
                args: [$($std_args),*],
            },
            colored: {
                formats: [$($col_fmt)*],
                args: [$($std_args),*],
            },
        }
    };
    {
        logger: $logger:expr,
        is_access: $access:expr,
        condition: $cond:expr,
        prompt: [$now:expr, $precise_time:expr, $pid:expr, $lvl:expr, $tag:expr$(,)?],
        standard: {
            formats: [$($std_fmt:tt)*],
            args: [$($std_args:expr),*$(,)?]$(,)?
        },
        colored: {
            formats: [$($col_fmt:tt)*],
            args: [$($col_args:expr),*$(,)?]$(,)?
        }$(,)?
    } => {
        if $cond {
            $crate::_prompt_log!(@bind [$logger, concat!("{} \x1b[2m{} \x1b[;2;1m{} {} \x1b[0;1m{}\x1b[m\t", $($col_fmt)*)] [$now, $precise_time, $pid, $lvl.as_str($access, true), $tag] $($col_args),*)
        } else {
            $crate::_prompt_log!(@bind [$logger, concat!("{} {} {} {} {}\t", $($std_fmt)*)] [$now, $precise_time, $pid, $lvl.as_str($access, false), $tag] $($std_args),*)
        }
    };
    (@bind [$logger:expr, $fmt:expr] [$($bindings:expr),*] $arg:expr $(, $args:expr)*) => {{
        let binding = &$arg;
        $crate::_prompt_log!(@bind [$logger, $fmt] [$($bindings),* , binding] $($args),*)
    }};
    (@bind [$logger:expr, $fmt:expr] [$($bindings:expr),*]) => {
        $logger(format_args!($fmt, $($bindings),*))
    };
}

#[derive(Clone, Copy, Debug)]
pub struct LogLineCachedState(u8);
const LOG_LINE_ENABLED: u8 = 1 << 7;

impl LogLineCachedState {
    pub const fn new() -> Self {
        Self(0)
    }
    #[inline(always)]
    pub fn version(&self) -> u8 {
        self.0 & !LOG_LINE_ENABLED
    }
    #[inline(always)]
    pub fn enabled(&self) -> bool {
        self.0 & LOG_LINE_ENABLED != 0
    }
    #[inline(always)]
    pub fn set(&mut self, version: u8, enabled: bool) {
        self.0 = version;
        if enabled {
            self.0 |= LOG_LINE_ENABLED
        }
    }
}

#[macro_export]
macro_rules! _log_enabled {
    ($logger:expr, $lvl:expr) => {{
        let logger = $logger.borrow_mut();
        let enable = if cfg!(feature = "logs-cache") {
            static mut LOG_LINE_CACHED_STATE: $crate::logging::LogLineCachedState =
                $crate::logging::LogLineCachedState::new();
            logger.cached_enabled(
                unsafe { &mut LOG_LINE_CACHED_STATE },
                $crate::logging::Metadata {
                    level: $lvl,
                    target: module_path!(),
                },
            )
        } else {
            logger.enabled($crate::logging::Metadata {
                level: $lvl,
                target: module_path!(),
            })
        };
        if !enable {
            return;
        }
        logger
    }};
}

#[macro_export]
macro_rules! _log {
    ($lvl:expr, $format:expr $(, $args:expr)*) => {{
        $crate::logging::LOGGER.with(|logger| {
            let mut logger = $crate::_log_enabled!(logger, $lvl);
            let (pid, tag, inner) = logger.split();
            let (now, precise_time) = $crate::logging::now();

            $crate::_prompt_log!{
                logger: |args| inner.log(args),
                is_access: false,
                condition: inner.colored,
                prompt: [now, precise_time, pid, $lvl, tag],
                standard: {
                    formats: [$format, '\n'],
                    args: [$($args),*]
                }
            };
        })
    }};
}

#[macro_export]
macro_rules! _log_access {
    ($lvl:expr, $($request_record_fields:tt)*) => {{
        $crate::logging::LOGGER.with(|logger| {
            let mut logger = $crate::_log_enabled!(logger, $lvl);
            let (pid, tag, inner) = logger.split();
            let (now, precise_time) = $crate::logging::now();

            inner.log_access(
                $crate::_structured_access_log!([$crate::logging::RequestRecord]
                pid, tag, now, precise_time, level: $lvl, $($request_record_fields)*
            ));
        })
    }};
}

#[macro_export]
macro_rules! _structured_access_log {
    ([$($struct_name:tt)+] $($fields:tt)*) => {{
        $($struct_name)+ {$(
            $fields
        )*}
    }};
}

#[macro_export]
/// dynamically chose between info_access and error_access
macro_rules! log_access {
    ($error:expr, $($request_record_fields:tt)*) => {
        let lvl = if $error {
            $crate::logging::LogLevel::Error
        } else {
            $crate::logging::LogLevel::Info
        };
        _log_access!(lvl, $($request_record_fields)*);
    };
}

/// log a failure concerning an HTTP or TCP request
#[macro_export]
macro_rules! error_access {
    ($($request_record_fields:tt)*) => {
        $crate::_log_access!($crate::logging::LogLevel::Error, $($request_record_fields)*);
    };
}

/// log the success of an HTTP or TCP request
#[macro_export]
macro_rules! info_access {
    ($($request_record_fields:tt)*) => {
        $crate::_log_access!($crate::logging::LogLevel::Info, $($request_record_fields)*);
    };
}

/// log an error with Sōzu's custom log stack
#[macro_export]
macro_rules! error {
    ($format:expr $(, $args:expr)* $(,)?) => {
        $crate::_log!($crate::logging::LogLevel::Error, $format $(, $args)*)
    };
}

/// log a warning with Sōzu’s custom log stack
#[macro_export]
macro_rules! warn {
    ($format:expr $(, $args:expr)* $(,)?) => {
        $crate::_log!($crate::logging::LogLevel::Warn, $format $(, $args)*)
    };
}

/// log an info with Sōzu’s custom log stack
#[macro_export]
macro_rules! info {
    ($format:expr $(, $args:expr)* $(,)?) => {
        $crate::_log!($crate::logging::LogLevel::Info, $format $(, $args)*)
    };
}

/// log a debug with Sōzu’s custom log stack
#[macro_export]
macro_rules! debug {
    ($format:expr $(, $args:expr)* $(,)?) => {{
        #[cfg(any(debug_assertions, feature = "logs-debug", feature = "logs-trace"))]
        $crate::_log!($crate::logging::LogLevel::Debug, concat!("{}\t", $format), module_path!() $(, $args)*);
        #[cfg(not(any(debug_assertions, feature = "logs-trace")))]
        {$( let _ = $args; )*}
    }};
}

/// log a trace with Sōzu’s custom log stack
#[macro_export]
macro_rules! trace {
    ($format:expr $(, $args:expr)* $(,)?) => {{
        #[cfg(any(debug_assertions, feature = "logs-trace"))]
        $crate::_log!($crate::logging::LogLevel::Trace, concat!("{}\t", $format), module_path!() $(, $args)*);
        #[cfg(not(any(debug_assertions, feature = "logs-trace")))]
        {$( let _ = $args; )*}
    }};
}

/// write a log with a "FIXME" prefix on an info level
#[macro_export]
macro_rules! fixme {
    ($(, $args:expr)* $(,)?) => {
        $crate::_log!($crate::logging::LogLevel::Info, "FIXME: {}:{} in {}: {}", file!(), line!(), module_path!() $(, $args)*)
    };
}

pub struct CompatLogger;

impl From<log::Level> for LogLevel {
    fn from(lvl: log::Level) -> Self {
        match lvl {
            log::Level::Error => LogLevel::Error,
            log::Level::Warn => LogLevel::Warn,
            log::Level::Info => LogLevel::Info,
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
        LOGGER.with(|logger| {
            let mut logger = logger.borrow_mut();
            if !logger.compat_enabled(record.metadata()) {
                return;
            }
            let (pid, tag, inner) = logger.split();
            let (now, precise_time) = now();
            crate::_prompt_log! {
                logger: |args| inner.log(args),
                is_access: false,
                condition: inner.colored,
                prompt: [
                    now, precise_time, pid, LogLevel::from(record.level()), tag
                ],
                standard: {
                    formats: ["{}\n"],
                    args: [record.args()]
                }
            };
        })
    }

    fn flush(&self) {}
}

/// start a logger used in test environment
#[macro_export]
macro_rules! setup_test_logger {
    () => {
        $crate::logging::Logger::init(
            module_path!().to_string(),
            "error",
            $crate::logging::LoggerBackend::Stdout(::std::io::stdout()),
            false,
            None,
            None,
            None,
        );
    };
}

pub struct Rfc3339Time {
    pub inner: ::time::OffsetDateTime,
}

pub fn now() -> (Rfc3339Time, i128) {
    let t = time::OffsetDateTime::now_utc();
    (
        Rfc3339Time { inner: t },
        (t - time::OffsetDateTime::UNIX_EPOCH).whole_nanoseconds(),
    )
}
