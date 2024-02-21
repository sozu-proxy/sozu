use std::fmt;

use crate::{
    logging::{
        EndpointRecord, FullTags, LogContext, LogDuration, LogLevel, LogMessage, LoggerBackend,
        Rfc3339Time,
    },
    AsStr,
};

impl LogLevel {
    pub const fn as_str(&self, access: bool, colored: bool) -> &'static str {
        match (self, access, colored) {
            (LogLevel::Error, false, false) => "ERROR",
            (LogLevel::Warn, false, false) => "WARN ",
            (LogLevel::Info, false, false) => "INFO ",
            (LogLevel::Debug, false, false) => "DEBUG",
            (LogLevel::Trace, false, false) => "TRACE",

            (LogLevel::Error, false, true) => "\x1b[;31;1mERROR",
            (LogLevel::Warn, false, true) => "\x1b[;33;1mWARN ",
            (LogLevel::Info, false, true) => "\x1b[;32;1mINFO ",
            (LogLevel::Debug, false, true) => "\x1b[;34mDEBUG",
            (LogLevel::Trace, false, true) => "\x1b[;90mTRACE",

            (LogLevel::Error, true, false) => "ERROR-ACCESS",
            (LogLevel::Info, true, false) => "INFO-ACCESS ",
            (_, true, false) => "???",

            (LogLevel::Error, true, true) => "\x1b[;35;1mERROR-ACCESS",
            (LogLevel::Info, true, true) => "\x1b[;35;1mINFO-ACCESS ",
            (_, true, true) => "\x1b[;35;1m???",
        }
    }
}

impl AsRef<str> for LoggerBackend {
    fn as_ref(&self) -> &str {
        match self {
            LoggerBackend::Stdout(_) => "stdout",
            LoggerBackend::Unix(_) => "UNIX socket",
            LoggerBackend::Udp(_, _) => "UDP socket",
            LoggerBackend::Tcp(_) => "TCP socket",
            LoggerBackend::File(_) => "file",
        }
    }
}

impl fmt::Display for Rfc3339Time {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        let t = self.inner;
        write!(
            f,
            "{}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
            t.year(),
            t.month() as u8,
            t.day(),
            t.hour(),
            t.minute(),
            t.second(),
            t.microsecond()
        )
    }
}

impl fmt::Display for LogMessage<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(message) => write!(f, " | {message}"),
            None => Ok(()),
        }
    }
}

impl fmt::Display for LogDuration {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.0 {
            None => write!(f, "-"),
            Some(duration) => {
                let secs = duration.whole_seconds();
                if secs >= 10 {
                    return write!(f, "{secs}s");
                }

                let ms = duration.whole_milliseconds();
                if ms < 10 {
                    let us = duration.whole_microseconds();
                    if us >= 10 {
                        return write!(f, "{us}μs");
                    }

                    let ns = duration.whole_nanoseconds();
                    return write!(f, "{ns}ns");
                }

                write!(f, "{ms}ms")
            }
        }
    }
}

impl fmt::Display for LogContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "[{} {} {}]",
            self.request_id,
            self.cluster_id.unwrap_or("-"),
            self.backend_id.unwrap_or("-")
        )
    }
}

impl fmt::Display for EndpointRecord<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Http {
                method,
                authority,
                path,
                status,
                ..
            } => write!(
                f,
                "{} {} {} {}",
                authority.as_str_or("-"),
                method.as_str_or("-"),
                path.as_str_or("-"),
                display_status(*status, f.alternate()),
            ),
            Self::Tcp => {
                write!(f, "-")
            }
        }
    }
}

fn display_status(status: Option<u16>, pretty: bool) -> String {
    match (status, pretty) {
        (Some(s @ 200..=299), true) => format!("\x1b[32m{s}"),
        (Some(s @ 300..=399), true) => format!("\x1b[34m{s}"),
        (Some(s @ 400..=499), true) => format!("\x1b[33m{s}"),
        (Some(s @ 500..=599), true) => format!("\x1b[31m{s}"),
        (Some(s), _) => s.to_string(),
        (None, _) => "-".to_string(),
    }
}

impl<'a> fmt::Display for FullTags<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.concatenated, self.user_agent) {
            (None, None) => Ok(()),
            (Some(tags), None) => write!(f, "{tags}"),
            (Some(tags), Some(ua)) if !tags.is_empty() => {
                write!(f, "{tags}, user-agent={}", prepare_user_agent(ua))
            }
            (_, Some(ua)) => write!(f, "user-agent={}", prepare_user_agent(ua)),
        }
    }
}

fn prepare_user_agent(user_agent: &str) -> String {
    user_agent
        .replace(' ', "_")
        .replace('[', "{")
        .replace(']', "}")
}
