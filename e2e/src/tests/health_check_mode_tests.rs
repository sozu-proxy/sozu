//! End-to-end tests for the health-check probe modes and accepted statuses
//! (sozu-proxy/sozu#1801).
//!
//! - `TCP` mode passes as soon as the TCP handshake completes: a blackholed
//!   backend (accept queue full, the handshake never completes) and a refused
//!   one are marked down, while backends that accept the connection but
//!   answer 500 or 404 stay up.
//! - `HTTP` mode with `accepted_statuses` judges the status against the list:
//!   `200-499` keeps a backend answering 404 on the probe path up.
//! - A legacy configuration (no mode, no list) keeps its meaning: any 2xx,
//!   so the same 404 backend is marked down.
//!
//! - Removing the health check (`RemoveHealthCheck`) puts a backend a probe
//!   had marked down back in rotation (sozu-proxy/sozu#1811).
//!
//! The mode switch itself is guarded by
//! `test_health_check_tcp_mode_keeps_500_and_404_backends_up`: an HTTP probe
//! marks those backends down, so it fails if `TCP` ever falls back to HTTP.
//! The blackholed/refused test passes under either mode — both fail an HTTP
//! probe too — and guards the connect verdict instead: a refused connection
//! reported as established makes it fail.
//!
//! The verdicts are read from the worker's `health_check.*` counters
//! (`HealthChecker::record_check_result`, `lib/src/health_check.rs`), which
//! count every probe result and every UP/DOWN transition. The counters are
//! thread-local to the worker, so each test reads only its own probes.

use std::{
    io::{ErrorKind, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    os::fd::AsRawFd,
    thread,
    time::{Duration, Instant},
};

use sozu_command_lib::proto::command::{
    HealthCheckConfig, HealthCheckMode, HttpStatusRange, QueryMetricsOptions, SetHealthCheck,
    filtered_metrics, request::RequestType, response_content::ContentType,
};
use sozu_lib::metrics::names::health_check::{DOWN, FAILURE, SUCCESS};

use crate::{
    http_utils::http_request,
    mock::{
        aggregator::SimpleAggregator, async_backend::BackendHandle as AsyncBackend, client::Client,
    },
    sozu::worker::Worker,
    tests::{
        State, repeat_until_error_or, setup_async_test,
        tests::{create_local_address, create_unbound_local_address},
    },
};

/// Upper bound for the counters to reach their target. Probes run every
/// second with a one-second timeout, so a blackholed backend is judged within
/// about two seconds; the margin absorbs a loaded CI host.
const VERDICT_BUDGET: Duration = Duration::from_secs(15);

/// One-second cycles, one-second timeout, one result per transition: the
/// fastest configuration the validation accepts.
fn fast_health_check(mode: HealthCheckMode) -> HealthCheckConfig {
    HealthCheckConfig {
        uri: "/livez".to_owned(),
        interval: 1,
        timeout: 1,
        healthy_threshold: 1,
        unhealthy_threshold: 1,
        expected_status: 0,
        mode: mode as i32,
        accepted_statuses: Vec::new(),
    }
}

/// A listener nobody accepts on, whose accept queue is full, so the kernel
/// drops every further SYN and a `connect(2)` to it hangs until its timeout.
///
/// `listen(2)` with a backlog of 0 still lets Linux queue one connection; the
/// constructor fills the queue with connections it keeps open and stops at the
/// first one that times out, which is the proof the address is blackholed.
struct Blackhole {
    address: SocketAddr,
    _listener: TcpListener,
    _queued: Vec<TcpStream>,
}

impl Blackhole {
    fn new() -> Self {
        // With `tcp_abort_on_overflow = 1` a full accept queue answers RST,
        // which is a refused connection and not a blackhole.
        if let Ok(value) = std::fs::read_to_string("/proc/sys/net/ipv4/tcp_abort_on_overflow") {
            assert_eq!(
                value.trim(),
                "0",
                "net.ipv4.tcp_abort_on_overflow must be 0 to blackhole a listener"
            );
        }
        let listener = TcpListener::bind("127.0.0.1:0").expect("could not bind a listener");
        // SAFETY: `listen(2)` on a socket this function owns, already
        // listening; it only shrinks the backlog.
        let result = unsafe { libc::listen(listener.as_raw_fd(), 0) };
        assert_eq!(result, 0, "listen(fd, 0) failed");
        let address = listener.local_addr().expect("listener has an address");
        let mut queued = Vec::new();
        loop {
            match TcpStream::connect_timeout(&address, Duration::from_millis(500)) {
                Ok(stream) => {
                    queued.push(stream);
                    assert!(
                        queued.len() < 16,
                        "the accept queue of {address} never filled"
                    );
                }
                Err(error)
                    if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) =>
                {
                    break;
                }
                Err(error) => panic!("unexpected error while filling {address}: {error}"),
            }
        }
        Self {
            address,
            _listener: listener,
            _queued: queued,
        }
    }
}

/// A backend answering every request with `status_line` and no body.
fn spawn_status_backend(
    name: &str,
    address: SocketAddr,
    status_line: &'static str,
) -> AsyncBackend<SimpleAggregator> {
    AsyncBackend::spawn_detached_backend(
        name,
        address,
        SimpleAggregator {
            requests_received: 0,
            responses_sent: 0,
        },
        Box::new(move |mut stream: &TcpStream, _name: &str, mut aggregator| {
            let mut buf = [0u8; 4096];
            match stream.read(&mut buf) {
                Ok(n) if n > 0 => {}
                _ => return aggregator,
            }
            aggregator.requests_received += 1;
            let response =
                format!("HTTP/1.1 {status_line}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            if stream.write_all(response.as_bytes()).is_ok() {
                aggregator.responses_sent += 1;
            }
            aggregator
        }),
    )
}

/// Read the worker's `health_check.*` counters as `(success, failure, down)`;
/// an absent counter reads as 0.
///
/// The health checker queues `HealthCheckUnhealthy` / `NoAvailableBackends`
/// events on the command channel as soon as a backend goes down, so the
/// responses are read up to the one answering this query rather than
/// trusting the next message to be it.
fn query_counters(worker: &mut Worker) -> (i64, i64, i64) {
    worker.send_proxy_request_type(RequestType::QueryMetrics(QueryMetricsOptions {
        list: false,
        cluster_ids: vec![],
        backend_ids: vec![],
        metric_names: vec![SUCCESS.to_owned(), FAILURE.to_owned(), DOWN.to_owned()],
        no_clusters: true,
        workers: false,
    }));
    let response = loop {
        let response = worker
            .read_proxy_response()
            .expect("the worker answers the metrics query");
        if response.id == worker.command_id.last {
            break response;
        }
    };
    let Some(ContentType::WorkerMetrics(metrics)) =
        response.content.and_then(|content| content.content_type)
    else {
        panic!("the metrics query must answer with worker metrics");
    };
    let count = |name: &str| match metrics.proxy.get(name).and_then(|m| m.inner.as_ref()) {
        Some(filtered_metrics::Inner::Count(value)) => *value,
        _ => 0,
    };
    (count(SUCCESS), count(FAILURE), count(DOWN))
}

/// Poll the worker's counters until `done` holds or the budget runs out;
/// returns the last `(success, failure, down)` sample.
fn wait_for_counters(
    worker: &mut Worker,
    mut done: impl FnMut(i64, i64, i64) -> bool,
) -> (i64, i64, i64) {
    let deadline = Instant::now() + VERDICT_BUDGET;
    loop {
        let sample = query_counters(worker);
        if done(sample.0, sample.1, sample.2) || Instant::now() >= deadline {
            return sample;
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn set_health_check(worker: &mut Worker, config: HealthCheckConfig) {
    worker.send_proxy_request_type(RequestType::SetHealthCheck(SetHealthCheck {
        cluster_id: "cluster_0".to_owned(),
        config,
    }));
    worker.read_to_last();
}

fn add_backend(worker: &mut Worker, backend_id: &str, address: SocketAddr) {
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        backend_id,
        address,
        None,
    )));
    worker.read_to_last();
}

fn stop(mut worker: Worker) {
    worker.soft_stop();
    worker.wait_for_server_stop();
}

// ---------------------------------------------------------------------------
// TCP mode
// ---------------------------------------------------------------------------

/// A blackholed backend and a refused one both fail the connect-only probe
/// and are marked down: the blackhole by the probe timeout, the refused one
/// by the socket error.
fn try_tcp_mode_marks_blackholed_and_refused_backends_down() -> State {
    let front_address = create_local_address();
    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, _) = setup_async_test(
        "HC-TCP-DOWN",
        config,
        listeners,
        state,
        front_address,
        0,
        false,
    );

    let blackhole = Blackhole::new();
    add_backend(&mut worker, "cluster_0-blackhole", blackhole.address);
    add_backend(
        &mut worker,
        "cluster_0-refused",
        create_unbound_local_address(),
    );
    set_health_check(&mut worker, fast_health_check(HealthCheckMode::Tcp));

    let (success, failure, down) = wait_for_counters(&mut worker, |_, _, down| down >= 2);
    stop(worker);

    if down >= 2 && success == 0 {
        State::Success
    } else {
        println!(
            "expected both backends DOWN and no passing probe, got success={success} \
             failure={failure} down={down}"
        );
        State::Fail
    }
}

#[test]
fn test_health_check_tcp_mode_marks_blackholed_and_refused_backends_down() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "TCP-mode health check marks blackholed and refused backends down",
            try_tcp_mode_marks_blackholed_and_refused_backends_down,
        ),
        State::Success
    );
}

/// Backends that accept the connection but answer 500 and 404 pass the
/// connect-only probe: in TCP mode the application's answer is not judged.
fn try_tcp_mode_keeps_500_and_404_backends_up() -> State {
    let front_address = create_local_address();
    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, _) = setup_async_test(
        "HC-TCP-UP",
        config,
        listeners,
        state,
        front_address,
        0,
        false,
    );

    let address_500 = create_local_address();
    let address_404 = create_local_address();
    let mut backend_500 = spawn_status_backend("HC_500", address_500, "500 Internal Server Error");
    let mut backend_404 = spawn_status_backend("HC_404", address_404, "404 Not Found");
    add_backend(&mut worker, "cluster_0-500", address_500);
    add_backend(&mut worker, "cluster_0-404", address_404);
    set_health_check(&mut worker, fast_health_check(HealthCheckMode::Tcp));

    // Three cycles over both backends: a failing probe would have flipped
    // one of them DOWN (unhealthy_threshold = 1) well before that.
    let (success, failure, down) = wait_for_counters(&mut worker, |success, failure, _| {
        success >= 6 || failure > 0
    });
    stop(worker);
    backend_500.stop_and_get_aggregator();
    backend_404.stop_and_get_aggregator();

    if success >= 6 && failure == 0 && down == 0 {
        State::Success
    } else {
        println!(
            "expected only passing probes, got success={success} failure={failure} down={down}"
        );
        State::Fail
    }
}

#[test]
fn test_health_check_tcp_mode_keeps_500_and_404_backends_up() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "TCP-mode health check keeps 500 and 404 backends up",
            try_tcp_mode_keeps_500_and_404_backends_up,
        ),
        State::Success
    );
}

// ---------------------------------------------------------------------------
// HTTP mode
// ---------------------------------------------------------------------------

/// Probe a backend answering 404 on the probe path with `config`; returns the
/// counters once the verdict is in.
fn probe_404_backend(name: &str, config: HealthCheckConfig) -> (i64, i64, i64) {
    let front_address = create_local_address();
    let (server_config, listeners, state) = Worker::empty_config();
    let (mut worker, _) = setup_async_test(
        name,
        server_config,
        listeners,
        state,
        front_address,
        0,
        false,
    );

    let address = create_local_address();
    let mut backend = spawn_status_backend("HC_404", address, "404 Not Found");
    add_backend(&mut worker, "cluster_0-404", address);
    set_health_check(&mut worker, config);

    let sample = wait_for_counters(&mut worker, |success, failure, down| {
        success >= 3 || (failure > 0 && down > 0)
    });
    stop(worker);
    backend.stop_and_get_aggregator();
    sample
}

/// `accepted_statuses = ["200-499"]` keeps a backend answering 404 on the
/// probe path up.
fn try_http_mode_accepted_statuses_keep_404_backend_up() -> State {
    let config = HealthCheckConfig {
        accepted_statuses: vec![HttpStatusRange {
            start: 200,
            end: 499,
        }],
        ..fast_health_check(HealthCheckMode::Http)
    };
    let (success, failure, down) = probe_404_backend("HC-HTTP-ACCEPTED", config);
    if success >= 3 && failure == 0 && down == 0 {
        State::Success
    } else {
        println!(
            "expected only passing probes, got success={success} failure={failure} down={down}"
        );
        State::Fail
    }
}

#[test]
fn test_health_check_http_mode_accepted_statuses_keep_404_backend_up() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "HTTP-mode health check with accepted 200-499 keeps a 404 backend up",
            try_http_mode_accepted_statuses_keep_404_backend_up,
        ),
        State::Success
    );
}

/// A legacy configuration (HTTP mode, no list, `expected_status = 0`) still
/// accepts only 2xx: the same 404 backend is marked down.
fn try_legacy_config_marks_404_backend_down() -> State {
    let (success, failure, down) =
        probe_404_backend("HC-HTTP-LEGACY", fast_health_check(HealthCheckMode::Http));
    if down >= 1 && success == 0 {
        State::Success
    } else {
        println!(
            "expected the 404 backend DOWN with no passing probe, got success={success} \
             failure={failure} down={down}"
        );
        State::Fail
    }
}

#[test]
fn test_health_check_legacy_config_marks_404_backend_down() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "legacy HTTP health check still marks a 404 backend down",
            try_legacy_config_marks_404_backend_down,
        ),
        State::Success
    );
}

// ---------------------------------------------------------------------------
// Removing the health check
// ---------------------------------------------------------------------------

/// A backend answering `body` with 200 on every path except the probe path
/// `/livez`, where it answers 404 when `fail_probe` is set: it serves real
/// traffic while failing an HTTP probe.
fn spawn_body_backend(
    name: &str,
    address: SocketAddr,
    body: &'static str,
    fail_probe: bool,
) -> AsyncBackend<SimpleAggregator> {
    AsyncBackend::spawn_detached_backend(
        name,
        address,
        SimpleAggregator {
            requests_received: 0,
            responses_sent: 0,
        },
        Box::new(move |mut stream: &TcpStream, _name: &str, mut aggregator| {
            let mut buf = [0u8; 4096];
            let n = match stream.read(&mut buf) {
                Ok(n) if n > 0 => n,
                _ => return aggregator,
            };
            aggregator.requests_received += 1;
            let response = if fail_probe && buf[..n].starts_with(b"GET /livez ") {
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_owned()
            } else {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            };
            if stream.write_all(response.as_bytes()).is_ok() {
                aggregator.responses_sent += 1;
            }
            aggregator
        }),
    )
}

/// The body of one request through the frontend, if any.
fn request_through(front_address: SocketAddr) -> Option<String> {
    let mut client = Client::new(
        "client",
        front_address,
        http_request("GET", "/api", "ping", "localhost"),
    );
    client.connect();
    client.send();
    client.receive()
}

/// Poll the frontend until a response contains `body` or the budget runs out.
fn reaches(front_address: SocketAddr, body: &str) -> bool {
    let deadline = Instant::now() + VERDICT_BUDGET;
    while Instant::now() < deadline {
        if request_through(front_address).is_some_and(|response| response.contains(body)) {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

/// A backend a probe marked down serves traffic again once the health check is
/// removed: `RemoveHealthCheck` stops probing, so nothing would ever mark it
/// up again, and the removal must reset its health.
///
/// Two backends are needed: with every backend down the load balancer fails
/// open and routes to them anyway, which would hide a stale DOWN.
fn try_remove_health_check_restores_down_backend() -> State {
    let front_address = create_local_address();
    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, _) = setup_async_test(
        "HC-REMOVE-RESET",
        config,
        listeners,
        state,
        front_address,
        0,
        false,
    );

    let address_down = create_local_address();
    let address_up = create_local_address();
    let mut backend_down = spawn_body_backend("HC_DOWN", address_down, "pong-down", true);
    let mut backend_up = spawn_body_backend("HC_UP", address_up, "pong-up", false);
    add_backend(&mut worker, "cluster_0-down", address_down);
    add_backend(&mut worker, "cluster_0-up", address_up);
    set_health_check(&mut worker, fast_health_check(HealthCheckMode::Http));

    // Precondition: the probe marked exactly the failing backend down, and
    // the load balancer now routes only to the other one.
    let (success, failure, down) =
        wait_for_counters(&mut worker, |success, _, down| down >= 1 && success >= 1);
    let excluded = down == 1
        && (0..10).all(|_| {
            request_through(front_address).is_some_and(|response| response.contains("pong-up"))
        });

    let restored = if excluded {
        worker.send_proxy_request_type(RequestType::RemoveHealthCheck("cluster_0".to_owned()));
        worker.read_to_last();
        reaches(front_address, "pong-down")
    } else {
        false
    };

    stop(worker);
    backend_down.stop_and_get_aggregator();
    backend_up.stop_and_get_aggregator();

    if !excluded {
        println!(
            "precondition: expected the failing backend alone DOWN and excluded, got \
             success={success} failure={failure} down={down}"
        );
        State::Fail
    } else if !restored {
        println!("the backend marked DOWN never served traffic after RemoveHealthCheck");
        State::Fail
    } else {
        State::Success
    }
}

#[test]
fn test_remove_health_check_restores_down_backend() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "RemoveHealthCheck puts a backend marked down back in rotation",
            try_remove_health_check_restores_down_backend,
        ),
        State::Success
    );
}
