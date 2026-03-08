//! Non-blocking HTTP health checks for backends
//!
//! Health checks run within the single-threaded mio event loop using non-blocking TCP
//! connections. Each check cycle is triggered by a timer. For each backend with a
//! configured health check, we open a non-blocking TCP connection, send a minimal
//! HTTP/1.1 GET request, parse the status line from the response, and update the
//! backend's health state accordingly.

use std::{
    cell::RefCell,
    collections::HashMap,
    io::{Read, Write},
    net::SocketAddr,
    rc::Rc,
    time::{Duration, Instant},
};

use mio::net::TcpStream;
use sozu_command::{
    proto::command::{Event, EventKind, HealthCheckConfig},
    state::ClusterId,
};

use crate::{backends::BackendMap, server::push_event};

type PendingChecks = Vec<(ClusterId, HealthCheckConfig, Vec<(String, SocketAddr)>)>;

/// Tracks an in-flight health check connection
#[derive(Debug)]
struct InFlightCheck {
    stream: TcpStream,
    cluster_id: ClusterId,
    backend_id: String,
    address: SocketAddr,
    started_at: Instant,
    timeout: Duration,
    request_sent: bool,
    response_buf: Vec<u8>,
    config: HealthCheckConfig,
}

/// Manages health checks across all clusters and backends
#[derive(Debug)]
pub struct HealthChecker {
    in_flight: Vec<InFlightCheck>,
    last_check_time: HashMap<ClusterId, Instant>,
}

impl Default for HealthChecker {
    fn default() -> Self {
        Self::new()
    }
}

impl HealthChecker {
    pub fn new() -> Self {
        HealthChecker {
            in_flight: Vec::new(),
            last_check_time: HashMap::new(),
        }
    }

    /// Called on each event loop iteration. Initiates new health checks when intervals
    /// have elapsed, and progresses in-flight checks.
    pub fn poll(&mut self, backends: &Rc<RefCell<BackendMap>>) {
        self.initiate_checks(backends);
        self.progress_checks(backends);
    }

    fn initiate_checks(&mut self, backends: &Rc<RefCell<BackendMap>>) {
        let backend_map = backends.borrow();
        let now = Instant::now();

        let mut to_check: PendingChecks = Vec::new();

        for (cluster_id, config) in &backend_map.health_check_configs {
            let interval = Duration::from_secs(u64::from(config.interval));
            let should_check = match self.last_check_time.get(cluster_id) {
                Some(last) => now.duration_since(*last) >= interval,
                None => true,
            };

            if !should_check {
                continue;
            }

            if let Some(backend_list) = backend_map.backends.get(cluster_id) {
                let backends_to_check: Vec<(String, SocketAddr)> = backend_list
                    .backends
                    .iter()
                    .filter(|b| {
                        let b = b.borrow();
                        b.status == crate::backends::BackendStatus::Normal
                            && !self.in_flight.iter().any(|f| {
                                f.cluster_id == *cluster_id && f.backend_id == b.backend_id
                            })
                    })
                    .map(|b| {
                        let b = b.borrow();
                        (b.backend_id.to_owned(), b.address)
                    })
                    .collect();

                if !backends_to_check.is_empty() {
                    to_check.push((cluster_id.to_owned(), config.to_owned(), backends_to_check));
                }
            }
        }

        drop(backend_map);

        for (cluster_id, config, backends_to_check) in to_check {
            self.last_check_time.insert(cluster_id.to_owned(), now);

            for (backend_id, address) in backends_to_check {
                match TcpStream::connect(address) {
                    Ok(stream) => {
                        trace!(
                            "health check: initiated connection to {} ({}) for cluster {}",
                            backend_id, address, cluster_id
                        );
                        self.in_flight.push(InFlightCheck {
                            stream,
                            cluster_id: cluster_id.to_owned(),
                            backend_id,
                            address,
                            started_at: now,
                            timeout: Duration::from_secs(u64::from(config.timeout)),
                            request_sent: false,
                            response_buf: Vec::with_capacity(256),
                            config: config.to_owned(),
                        });
                    }
                    Err(e) => {
                        debug!(
                            "health check: failed to connect to {} ({}) for cluster {}: {}",
                            backend_id, address, cluster_id, e
                        );
                        Self::record_check_result(
                            backends,
                            &cluster_id,
                            &backend_id,
                            address,
                            false,
                            &config,
                        );
                    }
                }
            }
        }
    }

    fn progress_checks(&mut self, backends: &Rc<RefCell<BackendMap>>) {
        let now = Instant::now();
        let mut completed = Vec::new();

        for (idx, check) in self.in_flight.iter_mut().enumerate() {
            if now.duration_since(check.started_at) > check.timeout {
                debug!(
                    "health check: timeout for {} ({}) in cluster {}",
                    check.backend_id, check.address, check.cluster_id
                );
                completed.push((idx, false));
                continue;
            }

            if !check.request_sent {
                let request = format!(
                    "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                    check.config.uri,
                    check.address.ip()
                );
                match check.stream.write_all(request.as_bytes()) {
                    Ok(()) => {
                        check.request_sent = true;
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        continue;
                    }
                    Err(_e) => {
                        completed.push((idx, false));
                        continue;
                    }
                }
            }

            let mut buf = [0u8; 256];
            match check.stream.read(&mut buf) {
                Ok(0) => {
                    let success = parse_health_response(&check.response_buf, &check.config);
                    completed.push((idx, success));
                }
                Ok(n) => {
                    check.response_buf.extend_from_slice(&buf[..n]);
                    if let Some(success) = try_parse_status_line(&check.response_buf, &check.config)
                    {
                        completed.push((idx, success));
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_e) => {
                    completed.push((idx, false));
                }
            }
        }

        completed.sort_by(|a, b| b.0.cmp(&a.0));
        for (idx, success) in completed {
            let check = self.in_flight.swap_remove(idx);
            Self::record_check_result(
                backends,
                &check.cluster_id,
                &check.backend_id,
                check.address,
                success,
                &check.config,
            );
        }
    }

    fn record_check_result(
        backends: &Rc<RefCell<BackendMap>>,
        cluster_id: &str,
        backend_id: &str,
        address: SocketAddr,
        success: bool,
        config: &HealthCheckConfig,
    ) {
        let mut backend_map = backends.borrow_mut();
        let Some(backend_list) = backend_map.backends.get_mut(cluster_id) else {
            return;
        };

        let Some(backend_ref) = backend_list.find_backend(&address) else {
            return;
        };

        let mut backend = backend_ref.borrow_mut();

        if success {
            let transitioned = backend.health.record_success(config.healthy_threshold);
            if transitioned {
                info!(
                    "backend {} at {} marked UP (health check passed {} consecutive times) for cluster {}",
                    backend_id, address, config.healthy_threshold, cluster_id
                );
                incr!("health_check.up");
                push_event(Event {
                    kind: EventKind::HealthCheckHealthy as i32,
                    cluster_id: Some(cluster_id.to_owned()),
                    backend_id: Some(backend_id.to_owned()),
                    address: Some(address.into()),
                });
            }
            count!("health_check.success", 1);
        } else {
            let transitioned = backend.health.record_failure(config.unhealthy_threshold);
            if transitioned {
                error!(
                    "backend {} at {} marked DOWN (health check failed {} consecutive times) for cluster {}",
                    backend_id, address, config.unhealthy_threshold, cluster_id
                );
                incr!("health_check.down");
                push_event(Event {
                    kind: EventKind::HealthCheckUnhealthy as i32,
                    cluster_id: Some(cluster_id.to_owned()),
                    backend_id: Some(backend_id.to_owned()),
                    address: Some(address.into()),
                });
            }
            count!("health_check.failure", 1);
        }
    }

    pub fn remove_cluster(&mut self, cluster_id: &str) {
        self.last_check_time.remove(cluster_id);
        self.in_flight
            .retain(|check| check.cluster_id != cluster_id);
    }
}

fn try_parse_status_line(buf: &[u8], config: &HealthCheckConfig) -> Option<bool> {
    let response = std::str::from_utf8(buf).ok()?;
    let first_line_end = response.find("\r\n")?;
    let status_line = &response[..first_line_end];

    let parts: Vec<&str> = status_line.splitn(3, ' ').collect();
    if parts.len() < 2 {
        return Some(false);
    }

    let status_code: u32 = parts[1].parse().unwrap_or(0);
    Some(is_status_healthy(status_code, config.expected_status))
}

fn parse_health_response(buf: &[u8], config: &HealthCheckConfig) -> bool {
    try_parse_status_line(buf, config).unwrap_or(false)
}

fn is_status_healthy(actual: u32, expected: u32) -> bool {
    if expected == 0 {
        (200..300).contains(&actual)
    } else {
        actual == expected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::HealthState;

    #[test]
    fn test_is_status_healthy_any_2xx() {
        assert!(is_status_healthy(200, 0));
        assert!(is_status_healthy(204, 0));
        assert!(is_status_healthy(299, 0));
        assert!(!is_status_healthy(301, 0));
        assert!(!is_status_healthy(500, 0));
        assert!(!is_status_healthy(0, 0));
    }

    #[test]
    fn test_is_status_healthy_specific() {
        assert!(is_status_healthy(200, 200));
        assert!(!is_status_healthy(204, 200));
        assert!(!is_status_healthy(500, 200));
    }

    #[test]
    fn test_try_parse_status_line() {
        let config = HealthCheckConfig {
            uri: "/health".to_owned(),
            interval: 10,
            timeout: 5,
            healthy_threshold: 3,
            unhealthy_threshold: 3,
            expected_status: 0,
        };

        let buf = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(try_parse_status_line(buf, &config), Some(true));

        let buf = b"HTTP/1.1 500 Internal Server Error\r\n\r\n";
        assert_eq!(try_parse_status_line(buf, &config), Some(false));

        let buf = b"HTTP/1.1 200";
        assert_eq!(try_parse_status_line(buf, &config), None);
    }

    #[test]
    fn test_health_state_transitions() {
        let mut state = HealthState::default();
        assert!(state.is_healthy());

        assert!(!state.record_failure(3));
        assert!(!state.record_failure(3));
        assert!(state.is_healthy());

        assert!(state.record_failure(3));
        assert!(!state.is_healthy());

        assert!(!state.record_success(3));
        assert!(!state.record_success(3));
        assert!(!state.is_healthy());

        assert!(state.record_success(3));
        assert!(state.is_healthy());
    }
}
