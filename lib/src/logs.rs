use std::{fmt, net::SocketAddr};

use rusty_ulid::Ulid;
use time::Duration;

use crate::{protocol::http::parser::Method, SessionMetrics};

pub struct LogContext<'a> {
    pub request_id: Ulid,
    pub cluster_id: Option<&'a str>,
    pub backend_id: Option<&'a str>,
}

impl fmt::Display for LogContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "{} {} {} \t",
            self.request_id,
            self.cluster_id.unwrap_or("-"),
            self.backend_id.unwrap_or("-")
        )
    }
}

pub trait AsStr {
    fn as_str_or(&self, default: &'static str) -> &str;
}
pub trait AsString {
    fn as_str_or(&self, default: &'static str) -> String;
}

impl AsStr for Option<&str> {
    fn as_str_or(&self, default: &'static str) -> &str {
        match self {
            None => default,
            Some(s) => s,
        }
    }
}
impl AsString for Option<SocketAddr> {
    fn as_str_or(&self, default: &'static str) -> String {
        match self {
            None => default.to_string(),
            Some(s) => s.to_string(),
        }
    }
}
impl AsString for Option<u16> {
    fn as_str_or(&self, default: &'static str) -> String {
        match self {
            None => default.to_string(),
            Some(s) => s.to_string(),
        }
    }
}
impl AsString for Option<&Method> {
    fn as_str_or(&self, default: &'static str) -> String {
        match self {
            None => default.to_string(),
            Some(s) => s.to_string(),
        }
    }
}

pub struct LogDuration(pub Option<Duration>);

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

pub enum Endpoint<'a> {
    Http {
        method: Option<&'a Method>,
        authority: Option<&'a str>,
        path: Option<&'a str>,
        status: Option<u16>,
        reason: Option<&'a str>,
    },
    Tcp {
        context: Option<&'a str>,
    },
}

impl fmt::Display for Endpoint<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Endpoint::Http {
                authority,
                method,
                path,
                status,
                ..
            } => write!(
                f,
                "{} {} {} -> {}",
                authority.as_str_or("-"),
                method.as_str_or("-"),
                path.as_str_or("-"),
                status.as_str_or("-"),
            ),
            Endpoint::Tcp { context } => write!(f, "{}", context.as_str_or("-")),
        }
    }
}

pub struct RequestRecord<'a> {
    pub error: Option<&'a str>,
    pub context: LogContext<'a>,
    pub session_address: Option<SocketAddr>,
    pub backend_address: Option<SocketAddr>,
    pub protocol: &'a str,
    pub endpoint: Endpoint<'a>,
    pub tags: Option<&'a str>,
    pub client_rtt: Option<Duration>,
    pub server_rtt: Option<Duration>,
    pub metrics: &'a SessionMetrics,
    pub user_agent: Option<&'a str>,
}

impl RequestRecord<'_> {
    pub fn log(&self) {
        let context = &self.context;
        let cluster_id = context.cluster_id;
        let tags = self.tags;

        let protocol = self.protocol;
        let session_address = self.session_address;
        let backend_address = self.backend_address;
        let endpoint = &self.endpoint;
        let user_agent = &self.user_agent;

        let metrics = self.metrics;
        // let backend_response_time = metrics.backend_response_time();
        // let backend_connection_time = metrics.backend_connection_time();
        // let backend_bin = metrics.backend_bin;
        // let backend_bout = metrics.backend_bout;
        let response_time = metrics.response_time();
        let service_time = metrics.service_time();
        // let wait_time = metrics.wait_time;
        let client_rtt = self.client_rtt;
        let server_rtt = self.server_rtt;

        if let Some(cluster_id) = cluster_id {
            time!(
                "response_time",
                cluster_id,
                response_time.whole_milliseconds()
            );
            time!(
                "service_time",
                cluster_id,
                service_time.whole_milliseconds()
            );
        }
        time!("response_time", response_time.whole_milliseconds());
        time!("service_time", service_time.whole_milliseconds());

        if let Some(backend_id) = metrics.backend_id.as_ref() {
            if let Some(backend_response_time) = metrics.backend_response_time() {
                record_backend_metrics!(
                    cluster_id.as_str_or("-"),
                    backend_id,
                    backend_response_time.whole_milliseconds(),
                    metrics.backend_connection_time(),
                    metrics.backend_bin,
                    metrics.backend_bout
                );
            }
        }

        match self.error {
            None => {
                info_access!(
                    "{}{} -> {} \t{}/{}/{}/{} \t{} -> {} \t {}{} {} {}",
                    context,
                    session_address.as_str_or("X"),
                    backend_address.as_str_or("X"),
                    LogDuration(Some(response_time)),
                    LogDuration(Some(service_time)),
                    LogDuration(client_rtt),
                    LogDuration(server_rtt),
                    metrics.bin,
                    metrics.bout,
                    match user_agent {
                        Some(_) => tags.as_str_or(""),
                        None => tags.as_str_or("-"),
                    },
                    match tags {
                        Some(tags) if !tags.is_empty() => user_agent
                            .map(|ua| format!(", user-agent={ua}"))
                            .unwrap_or_default(),
                        Some(_) | None => user_agent
                            .map(|ua| format!("user-agent={ua}"))
                            .unwrap_or_default(),
                    },
                    protocol,
                    endpoint
                );
                incr!(
                    "access_logs.count",
                    self.context.cluster_id,
                    self.context.backend_id
                );
            }
            Some(message) => error_access!(
                "{}{} -> {} \t{}/{}/{}/{} \t{} -> {} \t {}{} {} {} | {}",
                context,
                session_address.as_str_or("X"),
                backend_address.as_str_or("X"),
                LogDuration(Some(response_time)),
                LogDuration(Some(service_time)),
                LogDuration(client_rtt),
                LogDuration(server_rtt),
                metrics.bin,
                metrics.bout,
                match user_agent {
                    Some(_) => tags.as_str_or(""),
                    None => tags.as_str_or("-"),
                },
                match tags {
                    Some(tags) if !tags.is_empty() => user_agent
                        .map(|ua| format!(", user-agent={ua}"))
                        .unwrap_or_default(),
                    Some(_) | None => user_agent
                        .map(|ua| format!("user-agent={ua}"))
                        .unwrap_or_default(),
                },
                protocol,
                endpoint,
                message
            ),
        }
    }
}
