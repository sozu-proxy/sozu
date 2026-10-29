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
//! - A probe `port` moves the probe off the data port, in both modes: a
//!   backend serving on its data port but closed on the probe port is marked
//!   down, and one refusing its data port but answering on the probe port
//!   stays up, with the HTTP `Host` naming the probed address.
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
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    os::fd::AsRawFd,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use sozu_command_lib::{
    proto::command::{
        HealthCheckConfig, HealthCheckHeader, HealthCheckHeaderValue, HealthCheckMode,
        HttpStatusRange, ProtectedHealthCheckOrder, ProtectedHealthCheckPhase, QueryMetricsOptions,
        ResponseStatus, SetHealthCheck, SetProtectedHealthCheck, WorkerResponse, filtered_metrics,
        request::RequestType, response_content::ContentType,
    },
    secret::{FingerprintKey, ProtectedHealthCheck},
};
use sozu_lib::{
    metrics::names::health_check::{DOWN, FAILURE, SUCCESS, UP},
    protocol::mux::hpack::{Decoder, Encoder},
};

use crate::{
    http_utils::http_request,
    mock::{
        aggregator::SimpleAggregator, async_backend::BackendHandle as AsyncBackend, client::Client,
        h2_backend::H2Backend,
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
        port: None,
        headers: Vec::new(),
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

/// Accept one probe connection within the suite's health-check budget.
fn accept_probe(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + VERDICT_BUDGET;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .expect("could not bound probe reads");
                return stream;
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                panic!("no health-check connection arrived within {VERDICT_BUDGET:?}");
            }
            Err(error) => panic!("could not accept health-check connection: {error}"),
        }
    }
}

/// Read one complete HTTP probe request, bounded by the stream timeout.
fn read_probe_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => panic!("health-check connection closed before its HTTP request completed"),
            Ok(n) => {
                request.extend_from_slice(&buf[..n]);
                assert!(
                    request.len() <= 4096,
                    "health-check request exceeded the test's 4096-byte bound"
                );
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    return request;
                }
            }
            Err(error) => panic!("could not read health-check request: {error}"),
        }
    }
}

/// Read the worker's `health_check.*` counters as `(success, failure, down)`;
/// an absent counter reads as 0.
fn query_counters(worker: &mut Worker) -> (i64, i64, i64) {
    let [success, failure, down, _] = query_health_counters(worker);
    (success, failure, down)
}

/// Read the worker's `health_check.*` counters as
/// `[success, failure, down, up]`; an absent counter reads as 0. `down` and
/// `up` count the backend's health-state transitions, `success` and `failure`
/// every probe result.
///
/// The health checker queues `HealthCheckUnhealthy` / `NoAvailableBackends`
/// events on the command channel as soon as a backend goes down, so the
/// responses are read up to the one answering this query rather than
/// trusting the next message to be it.
fn query_health_counters(worker: &mut Worker) -> [i64; 4] {
    worker.send_proxy_request_type(RequestType::QueryMetrics(QueryMetricsOptions {
        list: false,
        cluster_ids: vec![],
        backend_ids: vec![],
        metric_names: vec![
            SUCCESS.to_owned(),
            FAILURE.to_owned(),
            DOWN.to_owned(),
            UP.to_owned(),
        ],
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
    [count(SUCCESS), count(FAILURE), count(DOWN), count(UP)]
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

/// Like [`wait_for_counters`], over `[success, failure, down, up]`.
fn wait_for_health_counters(
    worker: &mut Worker,
    mut done: impl FnMut([i64; 4]) -> bool,
) -> [i64; 4] {
    let deadline = Instant::now() + VERDICT_BUDGET;
    loop {
        let sample = query_health_counters(worker);
        if done(sample) || Instant::now() >= deadline {
            return sample;
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn set_health_check(worker: &mut Worker, config: HealthCheckConfig) -> WorkerResponse {
    worker.send_proxy_request_type(RequestType::SetHealthCheck(SetHealthCheck {
        cluster_id: "cluster_0".to_owned(),
        config,
    }));
    loop {
        let response = worker
            .read_proxy_response()
            .expect("the worker answers SetHealthCheck");
        if response.id == worker.command_id.last {
            return response;
        }
    }
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

/// A probe snapshots the policy at launch. A later `SetHealthCheck` is
/// acknowledged without cancelling or rewriting that probe; only probes
/// launched after the update use the replacement policy.
#[test]
fn in_flight_probe_finishes_with_old_policy_then_future_probe_uses_new_policy() {
    let front_address = create_local_address();
    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, _) = setup_async_test(
        "HC-POLICY-CUTOVER",
        config,
        listeners,
        state,
        front_address,
        0,
        false,
    );

    let accepted_listener =
        TcpListener::bind("127.0.0.1:0").expect("could not bind the accepted-status backend");
    let rejected_listener =
        TcpListener::bind("127.0.0.1:0").expect("could not bind the threshold backend");
    accepted_listener
        .set_nonblocking(true)
        .expect("could not make the accepted-status listener nonblocking");
    rejected_listener
        .set_nonblocking(true)
        .expect("could not make the threshold listener nonblocking");
    let accepted_address = accepted_listener
        .local_addr()
        .expect("accepted-status listener must have an address");
    let rejected_address = rejected_listener
        .local_addr()
        .expect("threshold listener must have an address");
    add_backend(&mut worker, "cluster_0-accepted", accepted_address);
    add_backend(&mut worker, "cluster_0-rejected", rejected_address);

    let old_policy = HealthCheckConfig {
        uri: "/old-policy".to_owned(),
        interval: 30,
        timeout: 10,
        healthy_threshold: 2,
        unhealthy_threshold: 2,
        accepted_statuses: vec![HttpStatusRange {
            start: 404,
            end: 404,
        }],
        ..fast_health_check(HealthCheckMode::Http)
    };
    assert_eq!(
        set_health_check(&mut worker, old_policy).status,
        ResponseStatus::Ok as i32,
        "the initial health-check policy must be acknowledged"
    );

    let mut accepted_probe = accept_probe(&accepted_listener);
    let mut rejected_probe = accept_probe(&rejected_listener);
    for request in [
        read_probe_request(&mut accepted_probe),
        read_probe_request(&mut rejected_probe),
    ] {
        assert!(
            request.starts_with(b"GET /old-policy HTTP/1.1\r\n"),
            "the in-flight probe must have launched in the old HTTP mode with the old URI: {request:?}"
        );
    }

    let mut new_policy = HealthCheckConfig {
        uri: String::new(),
        interval: 30,
        timeout: 5,
        ..fast_health_check(HealthCheckMode::Tcp)
    };
    let update = set_health_check(&mut worker, new_policy.to_owned());
    assert_eq!(
        update.status,
        ResponseStatus::Ok as i32,
        "SetHealthCheck must acknowledge the new policy while old probes are in flight"
    );
    assert_eq!(
        query_counters(&mut worker),
        (0, 0, 0),
        "acknowledging the TCP policy must not complete the old HTTP probes"
    );

    accepted_probe
        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
        .expect("could not finish the old accepted-status probe");
    rejected_probe
        .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n")
        .expect("could not finish the old threshold probe");
    accepted_probe
        .shutdown(Shutdown::Write)
        .expect("could not close the accepted-status response");
    rejected_probe
        .shutdown(Shutdown::Write)
        .expect("could not close the threshold response");

    let old_results = wait_for_counters(&mut worker, |success, failure, _| {
        success >= 1 && failure >= 1
    });
    assert_eq!(
        old_results,
        (1, 1, 0),
        "the old policy must accept 404 and require two failures before marking a backend DOWN"
    );

    drop(rejected_probe);
    drop(rejected_listener);
    new_policy.interval = 1;
    assert_eq!(
        set_health_check(&mut worker, new_policy).status,
        ResponseStatus::Ok as i32,
        "the next TCP probe must be made immediately eligible"
    );
    let mut future_probe = accept_probe(&accepted_listener);
    let future_results = wait_for_counters(&mut worker, |success, failure, down| {
        success >= 2 && failure >= 2 && down >= 1
    });
    assert_eq!(
        future_results.2, 1,
        "the refused future probe must use the new threshold of one failure"
    );
    let mut application_byte = [0u8; 1];
    let application_bytes = future_probe
        .read(&mut application_byte)
        .expect("could not observe the completed TCP-mode probe");
    assert_eq!(
        application_bytes, 0,
        "a future TCP-mode probe must close without sending HTTP bytes"
    );

    stop(worker);
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

// ---------------------------------------------------------------------------
// Probe port
// ---------------------------------------------------------------------------

/// A backend answering 200 when the request's `Host` names `address`, the
/// address it listens on, and 400 otherwise.
fn spawn_host_checking_backend(name: &str, address: SocketAddr) -> AsyncBackend<SimpleAggregator> {
    let host_line = format!("\r\nHost: {address}\r\n");
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
            let host_matches = buf[..n]
                .windows(host_line.len())
                .any(|window| window == host_line.as_bytes());
            let status_line = if host_matches {
                "200 OK"
            } else {
                "400 Bad Request"
            };
            let response =
                format!("HTTP/1.1 {status_line}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            if stream.write_all(response.as_bytes()).is_ok() {
                aggregator.responses_sent += 1;
            }
            aggregator
        }),
    )
}

/// A backend serving 200 on its data port, whose probe `port` is closed, is
/// marked down: the probe dials the probe port, not the data port, which
/// receives no probe request.
fn try_probe_port_closed_marks_serving_backend_down(name: &str, mode: HealthCheckMode) -> State {
    let front_address = create_local_address();
    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, _) = setup_async_test(name, config, listeners, state, front_address, 0, false);

    let data_address = create_local_address();
    let mut backend = spawn_status_backend("HC_DATA", data_address, "200 OK");
    add_backend(&mut worker, "cluster_0-data", data_address);
    let closed_port = create_unbound_local_address().port();
    set_health_check(
        &mut worker,
        HealthCheckConfig {
            port: Some(u32::from(closed_port)),
            ..fast_health_check(mode)
        },
    );

    let (success, failure, down) =
        wait_for_counters(&mut worker, |success, _, down| down >= 1 || success > 0);
    stop(worker);
    let probed_data_port = backend
        .stop_and_get_aggregator()
        .map_or(0, |aggregator| aggregator.requests_received);

    if down >= 1 && success == 0 && probed_data_port == 0 {
        State::Success
    } else {
        println!(
            "expected the backend DOWN through its closed probe port {closed_port}, got \
             success={success} failure={failure} down={down} data-port \
             requests={probed_data_port}"
        );
        State::Fail
    }
}

/// A backend refusing its data port, whose probe `port` answers, stays up.
/// In HTTP mode the probe backend answers 200 only when `Host` names the
/// probed `ip:port`.
fn try_probe_port_answering_keeps_backend_up(name: &str, mode: HealthCheckMode) -> State {
    let front_address = create_local_address();
    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, _) = setup_async_test(name, config, listeners, state, front_address, 0, false);

    let data_address = create_unbound_local_address();
    let probe_address = create_local_address();
    let mut backend = spawn_host_checking_backend("HC_PROBE", probe_address);
    add_backend(&mut worker, "cluster_0-probe", data_address);
    set_health_check(
        &mut worker,
        HealthCheckConfig {
            port: Some(u32::from(probe_address.port())),
            ..fast_health_check(mode)
        },
    );

    // Three cycles: a failing probe would have flipped the backend DOWN
    // (unhealthy_threshold = 1) well before that.
    let (success, failure, down) = wait_for_counters(&mut worker, |success, failure, _| {
        success >= 3 || failure > 0
    });
    stop(worker);
    backend.stop_and_get_aggregator();

    if success >= 3 && failure == 0 && down == 0 {
        State::Success
    } else {
        println!(
            "expected only passing probes on probe port {}, got success={success} \
             failure={failure} down={down}",
            probe_address.port()
        );
        State::Fail
    }
}

#[test]
fn test_health_check_tcp_mode_probe_port_closed_marks_serving_backend_down() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "TCP-mode health check marks a serving backend down when its probe port is closed",
            || try_probe_port_closed_marks_serving_backend_down(
                "HC-TCP-PORT-DOWN",
                HealthCheckMode::Tcp
            ),
        ),
        State::Success
    );
}

#[test]
fn test_health_check_tcp_mode_probe_port_answering_keeps_backend_up() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "TCP-mode health check keeps a backend up when its probe port answers",
            || try_probe_port_answering_keeps_backend_up("HC-TCP-PORT-UP", HealthCheckMode::Tcp),
        ),
        State::Success
    );
}

#[test]
fn test_health_check_http_mode_probe_port_closed_marks_serving_backend_down() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "HTTP-mode health check marks a serving backend down when its probe port is closed",
            || try_probe_port_closed_marks_serving_backend_down(
                "HC-HTTP-PORT-DOWN",
                HealthCheckMode::Http
            ),
        ),
        State::Success
    );
}

#[test]
fn test_health_check_http_mode_probe_port_answering_keeps_backend_up() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "HTTP-mode health check keeps a backend up when its probe port answers for its Host",
            || try_probe_port_answering_keeps_backend_up("HC-HTTP-PORT-UP", HealthCheckMode::Http),
        ),
        State::Success
    );
}

// ---------------------------------------------------------------------------
// Custom probe headers
// ---------------------------------------------------------------------------

/// Synthetic credentials: the exact value the probe backend demands, and a
/// wrong one.
const PROBE_AUTHORIZATION: &str = "Bearer SYNTHETIC-HC-PROBE-TOKEN-5a2c";
const WRONG_AUTHORIZATION: &str = "Bearer SYNTHETIC-HC-WRONG-TOKEN-77d0";

/// The key every protected order of this process is fingerprinted under: one
/// per process, never one per phase or per call.
fn fixture_key() -> &'static FingerprintKey {
    static KEY: OnceLock<FingerprintKey> = OnceLock::new();
    KEY.get_or_init(|| FingerprintKey::generate().expect("the OS random generator is available"))
}

/// Upper bound of the versions this process issues: far above what the suite
/// needs, so exhausting it is a bug of the suite and not a wrap-around.
const LAST_PROTECTED_VERSION: u64 = 1 << 16;

/// The next protected header version of this process. Strictly increasing, so
/// no worker fixture is ever staged the same version twice, and `0`, which no
/// transaction uses, is never issued.
fn next_protected_version() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let mut version = NEXT.load(Ordering::SeqCst);
    loop {
        let incremented = (version <= LAST_PROTECTED_VERSION)
            .then_some(version + 1)
            .expect("the protected header versions of the suite are exhausted");
        match NEXT.compare_exchange_weak(version, incremented, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return version,
            Err(current) => version = current,
        }
    }
}

/// Send one `ProtectedHealthCheckOrder` and wait for its answer, which must be
/// a positive `ProtectedHealthCheckReply` naming exactly this phase, cluster and
/// version. `prepare` is only carried by the prepare phase.
fn protected_health_check_phase(
    worker: &mut Worker,
    phase: ProtectedHealthCheckPhase,
    cluster_id: &str,
    version: u64,
    prepare: Option<SetProtectedHealthCheck>,
) -> WorkerResponse {
    assert_eq!(
        prepare.is_some(),
        phase == ProtectedHealthCheckPhase::ProtectedHealthCheckPrepare,
        "only the prepare phase carries a health check"
    );
    worker.send_proxy_request_type(RequestType::ProtectedHealthCheckOrder(
        ProtectedHealthCheckOrder {
            phase: phase as i32,
            cluster_id: cluster_id.to_owned(),
            version,
            prepare,
        },
    ));
    let response = loop {
        let response = worker
            .read_proxy_response()
            .expect("the worker answers ProtectedHealthCheckOrder");
        if response.id == worker.command_id.last {
            break response;
        }
    };
    assert_eq!(
        response.status,
        ResponseStatus::Ok as i32,
        "{phase:?} of version {version} was refused: {}",
        response.message
    );
    let Some(ContentType::ProtectedHealthCheckReply(reply)) = response
        .content
        .as_ref()
        .and_then(|content| content.content_type.as_ref())
    else {
        panic!("{phase:?} of version {version} must answer with a ProtectedHealthCheckReply");
    };
    assert_eq!(
        (
            ProtectedHealthCheckPhase::try_from(reply.phase),
            reply.cluster_id.as_str(),
            reply.version
        ),
        (Ok(phase), cluster_id, version),
        "the reply must name the phase, the cluster and the version it acted on"
    );
    response
}

/// Set `config` on the worker with `headers` as its ordered `(name, value)`
/// probe headers, through the protocol a worker actually speaks: prepare,
/// commit, release. The values reach the worker the one way they can, in the
/// prepare order built from the in-memory protected primitives and
/// fingerprinted under [`fixture_key`], never through `config`. An empty
/// `headers` is a protected clear: the cluster keeps its health check and
/// loses its headers. Returns the answer to the release.
fn set_protected_health_check(
    worker: &mut Worker,
    mut config: HealthCheckConfig,
    headers: &[(&str, &str)],
) -> WorkerResponse {
    const CLUSTER: &str = "cluster_0";
    config.headers = headers
        .iter()
        .map(|(name, _)| HealthCheckHeader {
            name: (*name).to_owned(),
            fingerprint: String::new(),
        })
        .collect();
    let mut order = SetProtectedHealthCheck {
        cluster_id: CLUSTER.to_owned(),
        config,
        values: headers
            .iter()
            .map(|(name, value)| HealthCheckHeaderValue {
                name: (*name).to_owned(),
                value: value.as_bytes().to_vec(),
            })
            .collect(),
    };
    let protected = ProtectedHealthCheck::from_ingress(fixture_key(), &mut order)
        .expect("the synthetic headers are valid");
    let version = next_protected_version();
    let phase = |worker: &mut Worker, phase, prepare| {
        protected_health_check_phase(worker, phase, CLUSTER, version, prepare)
    };
    phase(
        worker,
        ProtectedHealthCheckPhase::ProtectedHealthCheckPrepare,
        Some(protected.to_order()),
    );
    phase(
        worker,
        ProtectedHealthCheckPhase::ProtectedHealthCheckCommit,
        None,
    );
    phase(
        worker,
        ProtectedHealthCheckPhase::ProtectedHealthCheckRelease,
        None,
    )
}

/// Set `config` on the worker with `headers` as its ordered `(name, value)`
/// probe headers: a protected transaction when there is any, the ordinary
/// `SetHealthCheck` when there is none.
fn set_health_check_with_headers(
    worker: &mut Worker,
    config: HealthCheckConfig,
    headers: &[(&str, &str)],
) -> WorkerResponse {
    if headers.is_empty() {
        return set_health_check(worker, config);
    }
    set_protected_health_check(worker, config, headers)
}

/// The `Authorization` value of each request a probe backend received, `None`
/// for a request without one.
type SeenAuthorizations = Arc<Mutex<Vec<Option<Vec<u8>>>>>;

/// A probe backend answering 200 to the exact `PROBE_AUTHORIZATION` and 401
/// to any other request, recording the `Authorization` it saw: HTTP/1.1, or
/// cleartext HTTP/2 when `h2c`.
enum AuthorizationServer {
    H1(AsyncBackend<SimpleAggregator>, SeenAuthorizations),
    H2(H2Backend),
}

impl AuthorizationServer {
    fn start(h2c: bool, address: SocketAddr) -> Self {
        if h2c {
            return Self::H2(H2Backend::start_requiring_authorization(
                "HC_AUTH_H2",
                address,
                "ok",
                PROBE_AUTHORIZATION,
            ));
        }
        let seen = SeenAuthorizations::default();
        let recorded = Arc::clone(&seen);
        let backend = AsyncBackend::spawn_detached_backend(
            "HC_AUTH_H1",
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
                let authorization = buf[..n]
                    .split(|byte| *byte == b'\n')
                    .find_map(|line| line.strip_prefix(b"Authorization: "))
                    .map(|value| value.strip_suffix(b"\r").unwrap_or(value).to_vec());
                let status_line =
                    if authorization.as_deref() == Some(PROBE_AUTHORIZATION.as_bytes()) {
                        "200 OK"
                    } else {
                        "401 Unauthorized"
                    };
                recorded
                    .lock()
                    .expect("the authorization log is not poisoned")
                    .push(authorization);
                let response = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                if stream.write_all(response.as_bytes()).is_ok() {
                    aggregator.responses_sent += 1;
                }
                aggregator
            }),
        );
        Self::H1(backend, seen)
    }

    fn seen(&self) -> Vec<Option<Vec<u8>>> {
        match self {
            Self::H1(_, seen) => seen
                .lock()
                .expect("the authorization log is not poisoned")
                .clone(),
            Self::H2(backend) => backend
                .recorded_requests()
                .into_iter()
                .map(|request| {
                    request
                        .headers
                        .into_iter()
                        .find(|(name, _)| name == "authorization")
                        .map(|(_, value)| value)
                })
                .collect(),
        }
    }

    /// Wait until a request satisfying `wanted` was seen.
    fn wait_until_seen(&self, wanted: impl Fn(&Option<Vec<u8>>) -> bool) -> bool {
        let deadline = Instant::now() + VERDICT_BUDGET;
        while Instant::now() < deadline {
            if self.seen().iter().any(&wanted) {
                return true;
            }
            thread::sleep(Duration::from_millis(100));
        }
        false
    }

    fn stop(self) {
        match self {
            Self::H1(mut backend, _) => {
                backend.stop_and_get_aggregator();
            }
            Self::H2(mut backend) => backend.stop(),
        }
    }
}

/// A backend whose probe port demands an `Authorization`, away from its
/// (closed) data port, is marked DOWN while the probe sends none or a wrong
/// one, and UP once it sends the exact value. The verdict is read from the
/// worker's health counters: a request through the frontend would not tell,
/// since an all-down cluster fails open.
fn authorization_gates_health_state(name: &str, h2c: bool) {
    let front_address = create_local_address();
    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, _) = setup_async_test(name, config, listeners, state, front_address, 0, false);
    if h2c {
        let mut cluster = Worker::default_cluster("cluster_0");
        cluster.http2 = Some(true);
        worker.send_proxy_request_type(RequestType::AddCluster(cluster));
        worker.read_to_last();
    }

    let data_address = create_unbound_local_address();
    let probe_address = create_local_address();
    assert_ne!(data_address.port(), probe_address.port());
    let server = AuthorizationServer::start(h2c, probe_address);
    add_backend(&mut worker, "cluster_0-auth", data_address);
    let probe_config = || HealthCheckConfig {
        port: Some(u32::from(probe_address.port())),
        ..fast_health_check(HealthCheckMode::Http)
    };

    // No header: 401, the backend goes DOWN, never UP.
    assert_eq!(
        set_health_check_with_headers(&mut worker, probe_config(), &[]).status,
        ResponseStatus::Ok as i32
    );
    let [success, failure, down, up] =
        wait_for_health_counters(&mut worker, |[_, failure, down, _]| {
            failure >= 1 && down >= 1
        });
    assert_eq!(
        (success, down, up),
        (0, 1, 0),
        "a probe without the credential must fail: failure={failure}"
    );
    assert!(
        server.seen().iter().all(Option::is_none),
        "no probe carried an Authorization yet: {:?}",
        server.seen()
    );

    // A wrong value is refused too: the backend stays DOWN, and the server
    // saw that very value.
    assert_eq!(
        set_health_check_with_headers(
            &mut worker,
            probe_config(),
            &[("Authorization", WRONG_AUTHORIZATION)]
        )
        .status,
        ResponseStatus::Ok as i32
    );
    assert!(
        server.wait_until_seen(|value| value.as_deref() == Some(WRONG_AUTHORIZATION.as_bytes())),
        "a probe must carry the configured value: {:?}",
        server.seen()
    );
    let failures_before = query_health_counters(&mut worker)[1];
    let [success, failure, down, up] =
        wait_for_health_counters(&mut worker, |[_, failure, ..]| failure > failures_before);
    assert_eq!(
        (success, down, up),
        (0, 1, 0),
        "a wrong credential must keep the backend DOWN: failure={failure}"
    );

    // The exact value: 200, and the backend comes back UP.
    assert_eq!(
        set_health_check_with_headers(
            &mut worker,
            probe_config(),
            &[("Authorization", PROBE_AUTHORIZATION)]
        )
        .status,
        ResponseStatus::Ok as i32
    );
    let [success, _, down, up] =
        wait_for_health_counters(&mut worker, |[success, _, _, up]| success >= 1 && up >= 1);
    assert_eq!(
        (success >= 1, down, up),
        (true, 1, 1),
        "the exact credential must mark the backend UP (success={success})"
    );
    assert!(
        server
            .seen()
            .contains(&Some(PROBE_AUTHORIZATION.as_bytes().to_vec())),
        "the exact credential reached the backend: {:?}",
        server.seen()
    );

    stop(worker);
    server.stop();
}

#[test]
fn test_health_check_http_authorization_header_gates_health_state_h1() {
    authorization_gates_health_state("HC-AUTH-H1", false);
}

#[test]
fn test_health_check_http_authorization_header_gates_health_state_h2c() {
    authorization_gates_health_state("HC-AUTH-H2C", true);
}

/// The custom fields of one probe, in wire order: HTTP/1.1 field lines, or the
/// HPACK-decoded fields of the HEADERS frame, minus the probe-owned ones.
fn read_probe_fields(stream: &mut TcpStream, h2c: bool) -> Vec<(String, Vec<u8>)> {
    if !h2c {
        let request = read_probe_request(stream);
        let text = String::from_utf8(request).expect("the probe is UTF-8");
        let mut fields = Vec::new();
        for line in text.split("\r\n").skip(1).filter(|line| !line.is_empty()) {
            let (name, value) = line.split_once(": ").expect("a field line");
            if !["Host", "Connection"].contains(&name) {
                fields.push((name.to_owned(), value.as_bytes().to_vec()));
            }
        }
        return fields;
    }
    const PREFACE_AND_SETTINGS: usize = 24 + 9;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    let block = loop {
        if buf.len() >= PREFACE_AND_SETTINGS + 9 {
            let frame = &buf[PREFACE_AND_SETTINGS..];
            let len = (usize::from(frame[0]) << 16)
                | (usize::from(frame[1]) << 8)
                | usize::from(frame[2]);
            assert_eq!(
                frame[3], 0x01,
                "the probe's first frame after SETTINGS is HEADERS"
            );
            if frame.len() >= 9 + len {
                break frame[9..9 + len].to_vec();
            }
        }
        match stream.read(&mut chunk) {
            Ok(0) => panic!("h2c probe closed before its HEADERS frame completed"),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(error) => panic!("could not read the h2c probe: {error}"),
        }
    };
    let mut fields = Vec::new();
    Decoder::new()
        .decode_with_cb(&block, |name, value| {
            if !name.starts_with(b":") {
                fields.push((
                    String::from_utf8(name.to_vec()).expect("an ASCII name"),
                    value.to_vec(),
                ));
            }
        })
        .expect("the probe HPACK block decodes");
    fields
}

/// Answer a probe read by [`read_probe_fields`] with a bare 200 and close.
fn finish_probe(mut stream: TcpStream, h2c: bool) {
    let response = if h2c {
        let mut block = Vec::new();
        Encoder::new().encode_into([(b":status".as_slice(), b"200".as_slice())], &mut block);
        let mut frames = vec![0, 0, 0, 0x04, 0, 0, 0, 0, 0];
        let len = block.len();
        frames.extend_from_slice(&[(len >> 16) as u8, (len >> 8) as u8, len as u8, 0x01, 0x05]);
        frames.extend_from_slice(&1u32.to_be_bytes());
        frames.extend_from_slice(&block);
        frames
    } else {
        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
    };
    stream
        .write_all(&response)
        .expect("could not answer the probe");
    stream
        .shutdown(Shutdown::Write)
        .expect("could not close the probe response");
}

/// A header set change applies to the next probe only: a probe whose
/// connection is still being established when the set is replaced sends the
/// headers it was launched with, never the replacement; later probes send the
/// replacement, then none.
///
/// The first probe dials a backend whose accept queue is full, so its
/// request is not written until the test drains the queue, after both
/// replacements were acknowledged: a probe that read the live configuration
/// when it writes, not when it is launched, would send the last set.
fn in_flight_probe_keeps_its_headers_then_later_probes_use_the_new_set(name: &str, h2c: bool) {
    let front_address = create_local_address();
    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, _) = setup_async_test(name, config, listeners, state, front_address, 0, false);
    if h2c {
        let mut cluster = Worker::default_cluster("cluster_0");
        cluster.http2 = Some(true);
        worker.send_proxy_request_type(RequestType::AddCluster(cluster));
        worker.read_to_last();
    }
    // HTTP/2 names are lowercase on the wire.
    let wire = |name: &str| {
        if h2c {
            name.to_ascii_lowercase()
        } else {
            name.to_owned()
        }
    };
    let fields = |headers: &[(&str, &str)]| -> Vec<(String, Vec<u8>)> {
        headers
            .iter()
            .map(|(name, value)| (wire(name), value.as_bytes().to_vec()))
            .collect()
    };

    let first = [
        ("Authorization", "Bearer SYNTHETIC-HC-FIRST-1f00"),
        ("X-Zeta", "z"),
        ("x-Alpha", "a"),
    ];
    let second = [
        ("X-Second", "2"),
        ("Authorization", "Bearer SYNTHETIC-HC-SECOND-8e11"),
    ];

    let blackhole = Blackhole::new();
    let queued = blackhole._queued.len();
    add_backend(&mut worker, "cluster_0-snapshot", blackhole.address);
    // Probes are judged by a 200; a long timeout keeps the blackholed one alive.
    let with_interval = |interval: u32| HealthCheckConfig {
        interval,
        timeout: 10,
        ..fast_health_check(HealthCheckMode::Http)
    };

    // Launches the probe: its SYN is dropped by the full accept queue.
    assert_eq!(
        set_health_check_with_headers(&mut worker, with_interval(30), &first).status,
        ResponseStatus::Ok as i32
    );
    assert_eq!(
        set_health_check_with_headers(&mut worker, with_interval(1), &second).status,
        ResponseStatus::Ok as i32,
        "SetHealthCheck must acknowledge the new headers while a probe is launching"
    );
    assert_eq!(
        query_counters(&mut worker),
        (0, 0, 0),
        "replacing the headers must not complete the probe"
    );

    // Free the queue: the probe's SYN retransmission completes the handshake.
    blackhole
        ._listener
        .set_nonblocking(true)
        .expect("could not make the listener nonblocking");
    for _ in 0..queued {
        accept_probe(&blackhole._listener);
    }
    let mut probe = accept_probe(&blackhole._listener);
    assert_eq!(
        read_probe_fields(&mut probe, h2c),
        fields(&first),
        "the probe launched before the replacement keeps the headers it was launched with"
    );
    finish_probe(probe, h2c);
    let results = wait_for_counters(&mut worker, |success, _, _| success >= 1);
    assert_eq!(results, (1, 0, 0), "the first probe passes");

    // The next probe, launched after the replacement, sends the new set; it
    // is held in flight while the set is removed.
    let mut probe = accept_probe(&blackhole._listener);
    assert_eq!(
        read_probe_fields(&mut probe, h2c),
        fields(&second),
        "the next probe sends the replacement headers, in order"
    );
    assert_eq!(
        set_protected_health_check(&mut worker, with_interval(1), &[]).status,
        ResponseStatus::Ok as i32
    );
    assert_eq!(
        query_counters(&mut worker),
        (1, 0, 0),
        "removing the headers must not complete the probe in flight"
    );
    finish_probe(probe, h2c);
    wait_for_counters(&mut worker, |success, _, _| success >= 2);

    let mut probe = accept_probe(&blackhole._listener);
    assert_eq!(
        read_probe_fields(&mut probe, h2c),
        Vec::<(String, Vec<u8>)>::new(),
        "once removed, a probe sends no custom header"
    );
    finish_probe(probe, h2c);
    let results = wait_for_counters(&mut worker, |success, _, _| success >= 3);
    assert!(
        results.0 >= 3 && results.1 == 0 && results.2 == 0,
        "every probe passed: {results:?}"
    );

    stop(worker);
}

#[test]
fn test_health_check_in_flight_probe_keeps_its_headers_h1() {
    in_flight_probe_keeps_its_headers_then_later_probes_use_the_new_set("HC-HDR-CUTOVER-H1", false);
}

#[test]
fn test_health_check_in_flight_probe_keeps_its_headers_h2c() {
    in_flight_probe_keeps_its_headers_then_later_probes_use_the_new_set("HC-HDR-CUTOVER-H2C", true);
}
