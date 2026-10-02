//! End-to-end coverage for backend failover on connect failure
//! (sozu-proxy/sozu#1800).
//!
//! - A backend that never accepts the connection (blackholed: its accept
//!   queue is full, so the kernel drops the SYN) times out on
//!   `connect_timeout`. The request must go to another backend instead of
//!   being answered `504`, and the failure must be counted so the load
//!   balancer stops picking that backend. Covered for an HTTP/1.1 and an
//!   HTTP/2 frontend.
//! - Refused backends interleaved with healthy ones under round robin: no
//!   request may run out of attempts while a healthy backend exists.

use std::{
    io::{ErrorKind, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    os::fd::AsRawFd,
    time::{Duration, Instant},
};

use sozu_command_lib::{
    config::{FileConfig, ListenerBuilder},
    proto::command::{
        ActivateListener, AddCertificate, CertificateAndKey, Cluster, ListenerType,
        QueryMetricsOptions, RequestHttpFrontend, ResponseStatus, ServerConfig, SocketAddress,
        WorkerResponse, filtered_metrics, request::RequestType, response_content::ContentType,
    },
    scm_socket::Listeners,
    state::ConfigState,
};

use crate::{
    BUFFER_SIZE,
    mock::{
        aggregator::SimpleAggregator,
        async_backend::BackendHandle as AsyncBackend,
        h2_backend::H2Backend,
        https_client::{build_h2_client, resolve_request_timeout},
    },
    port_registry::{attach_reserved_http_listener, attach_reserved_https_listener},
    sozu::worker::Worker,
    tests::{State, repeat_until_error_or},
};

use super::tests::{create_local_address, create_unbound_local_address};

/// Connect timeout given to every listener here, in seconds. Short, so a
/// blackholed dial costs a test one second rather than the default three.
const CONNECT_TIMEOUT: u32 = 1;

/// How long a test client waits for one complete response. Covers the
/// worst failover a request can go through: one connect timeout per
/// blackholed backend of the cluster.
const RESPONSE_DEADLINE: Duration = Duration::from_secs(10);

/// What a backend of a test cluster does with a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Behaviour {
    /// Accepts and answers `200` over HTTP/1.1.
    Healthy,
    /// Accepts and answers `200` over cleartext HTTP/2, for a cluster with
    /// `http2 = true`.
    HealthyH2,
    /// Accepts nothing and answers nothing: the SYN is dropped.
    Blackholed,
    /// Nothing listens: the connection is refused.
    Refused,
}

/// A listener nobody accepts on, whose accept queue is full, so the kernel
/// drops every further SYN and a `connect(2)` to it hangs until its timeout.
///
/// `listen(2)` with a backlog of 0 still lets Linux queue one connection; the
/// constructor fills the queue with connections it keeps open and stops at the
/// first one that times out, which is the proof the address is blackholed.
pub(crate) struct Blackhole {
    pub address: SocketAddr,
    _listener: TcpListener,
    _queued: Vec<TcpStream>,
}

impl Blackhole {
    pub(crate) fn new() -> Self {
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

/// Everything a test cluster holds alive while the test runs.
pub(crate) struct Backends {
    _healthy: Vec<AsyncBackend<SimpleAggregator>>,
    _healthy_h2: Vec<H2Backend>,
    _blackholes: Vec<Blackhole>,
}

/// Stop the HTTP/1.1 backends with the cluster. Their thread polls a
/// non-blocking listener in a loop that only a stop message ends: a dropped
/// handle leaves it spinning for the rest of the test process, and two dozen
/// of them starved every timing-sensitive test that ran after this module on
/// a small CI runner (`test_h2_rapid_reset_triggers_goaway`). `H2Backend`
/// stops itself on drop; a `Blackhole` owns no thread.
impl Drop for Backends {
    fn drop(&mut self) {
        for backend in &mut self._healthy {
            backend.stop_and_get_aggregator();
        }
    }
}

/// Which frontend a test talks to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Frontend {
    /// Plain HTTP, spoken as HTTP/1.1.
    Http1,
    /// HTTPS with ALPN `h2`, spoken as HTTP/2.
    Http2,
}

/// Start a worker with one listener of the `frontend` kind on
/// `front_address`, its `connect_timeout` set to [`CONNECT_TIMEOUT`].
pub(crate) fn start_worker(
    name: &str,
    config: ServerConfig,
    frontend: Frontend,
    front_address: SocketAddr,
) -> Worker {
    start_worker_with_front_timeout(name, config, frontend, front_address, None)
}

/// [`start_worker`], with the listener's `front_timeout` set when given.
pub(crate) fn start_worker_with_front_timeout(
    name: &str,
    config: ServerConfig,
    frontend: Frontend,
    front_address: SocketAddr,
    front_timeout: Option<u32>,
) -> Worker {
    let mut listeners = Listeners::default();
    match frontend {
        Frontend::Http1 => attach_reserved_http_listener(&mut listeners, front_address),
        Frontend::Http2 => attach_reserved_https_listener(&mut listeners, front_address),
    }
    let mut worker = Worker::start_new_worker_owned(name, config, listeners, ConfigState::new());
    match frontend {
        Frontend::Http1 => {
            worker.send_proxy_request_type(RequestType::AddHttpListener(
                ListenerBuilder::new_http(front_address.into())
                    .with_connect_timeout(Some(CONNECT_TIMEOUT))
                    .with_front_timeout(front_timeout)
                    .to_http(None)
                    .unwrap(),
            ));
            worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
                interface: None,
                address: front_address.into(),
                proxy: ListenerType::Http.into(),
                from_scm: false,
            }));
        }
        Frontend::Http2 => {
            let address = SocketAddress::from(front_address);
            worker.send_proxy_request_type(RequestType::AddHttpsListener(
                ListenerBuilder::new_https(address.clone())
                    .with_connect_timeout(Some(CONNECT_TIMEOUT))
                    .with_front_timeout(front_timeout)
                    .to_tls(None)
                    .unwrap(),
            ));
            worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
                interface: None,
                address: address.clone(),
                proxy: ListenerType::Https.into(),
                from_scm: false,
            }));
            worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
                address,
                certificate: CertificateAndKey {
                    certificate: String::from(include_str!(
                        "../../../lib/assets/local-certificate.pem"
                    )),
                    key: String::from(include_str!("../../../lib/assets/local-key.pem")),
                    certificate_chain: vec![],
                    versions: vec![],
                    names: vec![],
                },
                expired_at: None,
            }));
        }
    }
    worker.read_to_last();
    worker
}

/// Add `cluster` with one `localhost` frontend for the path prefix `path` and
/// one backend per entry of `behaviours`, in that order. Backend ids are
/// `<cluster_id>-<index>`.
pub(crate) fn add_cluster(
    worker: &mut Worker,
    frontend: Frontend,
    front_address: SocketAddr,
    cluster: Cluster,
    path: &str,
    behaviours: &[Behaviour],
) -> Backends {
    let cluster_id = cluster.cluster_id.clone();
    worker.send_proxy_request_type(RequestType::AddCluster(cluster));
    let http_frontend = RequestHttpFrontend {
        path: sozu_command_lib::proto::command::PathRule::prefix(path.to_owned()),
        ..Worker::default_http_frontend(cluster_id.clone(), front_address)
    };
    worker.send_proxy_request_type(match frontend {
        Frontend::Http1 => RequestType::AddHttpFrontend(http_frontend),
        Frontend::Http2 => RequestType::AddHttpsFrontend(http_frontend),
    });

    let mut healthy = Vec::new();
    let mut healthy_h2 = Vec::new();
    let mut blackholes = Vec::new();
    for (index, behaviour) in behaviours.iter().enumerate() {
        let backend_id = format!("{cluster_id}-{index}");
        let address = match behaviour {
            Behaviour::Healthy => {
                let address = create_local_address();
                healthy.push(AsyncBackend::spawn_detached_backend(
                    backend_id.clone(),
                    address,
                    SimpleAggregator::default(),
                    AsyncBackend::http_handler(format!("pong-{backend_id}")),
                ));
                address
            }
            Behaviour::HealthyH2 => {
                let address = create_local_address();
                healthy_h2.push(H2Backend::start(
                    backend_id.clone(),
                    address,
                    format!("pong-{backend_id}"),
                ));
                address
            }
            Behaviour::Blackholed => {
                let blackhole = Blackhole::new();
                let address = blackhole.address;
                blackholes.push(blackhole);
                address
            }
            Behaviour::Refused => create_unbound_local_address(),
        };
        worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
            cluster_id.clone(),
            backend_id.clone(),
            address,
            None,
        )));
    }
    worker.read_to_last();
    Backends {
        _healthy: healthy,
        _healthy_h2: healthy_h2,
        _blackholes: blackholes,
    }
}

/// Send one `GET` over a fresh HTTP/1.1 connection and return the status
/// code, or `None` when no complete status line arrived in time.
pub(crate) fn h1_get(front_address: SocketAddr, path: &str) -> Option<u16> {
    let mut stream = TcpStream::connect(front_address).ok()?;
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(50)))
        .ok()?;
    let started = Instant::now();
    let mut data = Vec::new();
    let mut buffer = [0u8; BUFFER_SIZE];
    while started.elapsed() < RESPONSE_DEADLINE {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => data.extend_from_slice(&buffer[..n]),
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => break,
        }
    }
    let response = String::from_utf8_lossy(&data);
    response
        .strip_prefix("HTTP/1.1 ")
        .and_then(|rest| rest.get(..3))
        .and_then(|code| code.parse().ok())
}

/// Send `count` sequential requests for `path` and return their status codes
/// (`0` for a request that got no answer). The HTTP/2 requests share one
/// client connection, so they are streams of one session.
pub(crate) fn send_requests(
    frontend: Frontend,
    front_address: SocketAddr,
    path: &str,
    count: usize,
) -> Vec<u16> {
    match frontend {
        Frontend::Http1 => (0..count)
            .map(|_| h1_get(front_address, path).unwrap_or(0))
            .collect(),
        Frontend::Http2 => {
            let client = build_h2_client();
            let uri: hyper::Uri = format!("https://localhost:{}{path}", front_address.port())
                .parse()
                .expect("valid uri");
            (0..count)
                .map(|_| {
                    resolve_request_timeout(&client, uri.clone(), RESPONSE_DEADLINE)
                        .map_or(0, |(status, _)| status.as_u16())
                })
                .collect()
        }
    }
}

/// The worker's answer to the request just sent. A worker also writes the
/// events it emits to the command channel — a backend these tests make fail
/// is reported `BACKEND_DOWN` once its retry policy gives up on it — and one
/// can arrive before the answer, so they are skipped.
pub(crate) fn read_answer(worker: &mut Worker) -> WorkerResponse {
    loop {
        let response = worker.read_proxy_response().expect("the worker answers");
        if !matches!(
            response
                .content
                .as_ref()
                .and_then(|content| content.content_type.as_ref()),
            Some(ContentType::Event(_))
        ) {
            return response;
        }
    }
}

/// The `backend.connections.error` count of `cluster_id`: the backend
/// connections that failed to establish, refused or timed out.
pub(crate) fn cluster_connection_errors(worker: &mut Worker, cluster_id: &str) -> i64 {
    worker.send_proxy_request_type(RequestType::QueryMetrics(QueryMetricsOptions {
        list: false,
        cluster_ids: vec![cluster_id.to_owned()],
        backend_ids: vec![],
        metric_names: vec!["backend.connections.error".to_owned()],
        no_clusters: false,
        workers: false,
    }));
    let response = read_answer(worker);
    assert_eq!(response.status, ResponseStatus::Ok as i32, "{response:?}");
    let Some(ContentType::WorkerMetrics(metrics)) =
        response.content.and_then(|content| content.content_type)
    else {
        return 0;
    };
    match metrics
        .clusters
        .get(cluster_id)
        .and_then(|cluster| cluster.cluster.get("backend.connections.error"))
        .and_then(|metric| metric.inner.as_ref())
    {
        Some(filtered_metrics::Inner::Count(count)) => *count,
        _ => 0,
    }
}

pub(crate) fn stop(worker: Worker) {
    let mut worker = worker;
    worker.soft_stop();
    worker.wait_for_server_stop();
}

// ── Connect timeout ─────────────────────────────────────────────────────────

/// 3 healthy and 3 blackholed backends, interleaved, round robin. Every
/// request must answer `200`: a dial that times out goes to another backend.
/// Each blackholed backend must then show a counted connect failure, which is
/// what takes it out of the selection.
fn try_connect_timeout_fails_over(frontend: Frontend) -> State {
    let front_address = create_local_address();
    let mut worker = start_worker(
        "CONNECT-TIMEOUT-FAILOVER",
        Worker::into_config(FileConfig::default()),
        frontend,
        front_address,
    );
    let _backends = add_cluster(
        &mut worker,
        frontend,
        front_address,
        Worker::default_cluster("cluster_0"),
        "/",
        &[
            Behaviour::Healthy,
            Behaviour::Blackholed,
            Behaviour::Healthy,
            Behaviour::Blackholed,
            Behaviour::Healthy,
            Behaviour::Blackholed,
        ],
    );

    let statuses = send_requests(frontend, front_address, "/api", 24);
    println!("{frontend:?} statuses: {statuses:?}");
    let errors = cluster_connection_errors(&mut worker, "cluster_0");
    println!("{frontend:?} backend.connections.error: {errors}");
    stop(worker);

    if statuses.iter().any(|status| *status != 200) {
        println!("a request was not failed over to a healthy backend");
        return State::Fail;
    }
    // Every connect failure feeds the backend's retry policy, whose back-off
    // is what keeps the load balancer off it afterwards. Before
    // sozu-proxy/sozu#1800 a connect timeout was never counted, so this stayed
    // at 0 and the blackholed backends kept their full share of requests.
    // Round robin over six backends sends the first six requests to each of
    // them once, so each of the three blackholed backends fails at least once.
    if errors < 3 {
        println!(
            "only {errors} connect timeouts were counted as backend connect failures, \
             expected at least one per blackholed backend"
        );
        return State::Fail;
    }
    State::Success
}

#[test]
fn test_connect_timeout_fails_over_h1() {
    assert_eq!(
        repeat_until_error_or(
            1,
            "H1: a backend connect timeout fails the request over to a healthy backend",
            || try_connect_timeout_fails_over(Frontend::Http1),
        ),
        State::Success
    );
}

#[test]
fn test_connect_timeout_fails_over_h2() {
    assert_eq!(
        repeat_until_error_or(
            1,
            "H2: a backend connect timeout fails the stream over to a healthy backend",
            || try_connect_timeout_fails_over(Frontend::Http2),
        ),
        State::Success
    );
}

/// The blackhole fixture itself: a connect to it must time out, or every
/// test above would be measuring a refused connection instead.
#[test]
fn test_blackholed_listener_times_out() {
    let blackhole = Blackhole::new();
    let error = TcpStream::connect_timeout(&blackhole.address, Duration::from_secs(1))
        .expect_err("a connect to a full accept queue must not complete");
    assert!(
        matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock),
        "expected a timeout, got {error:?}"
    );
}

// ── Refused connections ─────────────────────────────────────────────────────

/// 3 refused and 3 healthy backends, alternating and starting with a refused
/// one, round robin. A request must never run out of attempts while a healthy
/// backend exists.
///
/// The order is what makes the first request walk onto all three refused
/// backends: each failed backend leaves the candidate set, and the round-robin
/// cursor, advanced on every pick, then lands on the next refused one. Three
/// attempts — the budget before sozu-proxy/sozu#1800 — are spent on them.
fn try_refused_backends_never_exhaust_attempts(frontend: Frontend) -> State {
    let front_address = create_local_address();
    let mut worker = start_worker(
        "REFUSED-FAILOVER",
        Worker::into_config(FileConfig::default()),
        frontend,
        front_address,
    );
    let _backends = add_cluster(
        &mut worker,
        frontend,
        front_address,
        Worker::default_cluster("cluster_0"),
        "/",
        &[
            Behaviour::Refused,
            Behaviour::Healthy,
            Behaviour::Refused,
            Behaviour::Healthy,
            Behaviour::Refused,
            Behaviour::Healthy,
        ],
    );

    let statuses = send_requests(frontend, front_address, "/api", 12);
    println!("{frontend:?} statuses: {statuses:?}");
    stop(worker);

    if statuses.iter().any(|status| *status != 200) {
        println!("a request ran out of attempts while healthy backends existed");
        return State::Fail;
    }
    State::Success
}

#[test]
fn test_refused_backends_never_exhaust_attempts_h1() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "H1: refused backends interleaved with healthy ones never answer 503",
            || try_refused_backends_never_exhaust_attempts(Frontend::Http1),
        ),
        State::Success
    );
}

#[test]
fn test_refused_backends_never_exhaust_attempts_h2() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "H2: refused backends interleaved with healthy ones never answer 503",
            || try_refused_backends_never_exhaust_attempts(Frontend::Http2),
        ),
        State::Success
    );
}

// ── HTTP/2 backends ─────────────────────────────────────────────────────────

/// A cluster with `http2 = true`: 2 healthy HTTP/2 backends, 1 blackholed and
/// 1 refused. A request is encoded for an HTTP/2 backend only once that
/// connection is established, so a dial that failed before any byte was sent
/// leaves it free to go elsewhere: every request must answer `200`, never the
/// `502` a request bound to an HTTP/2 backend gets (`EndStreamAction::
/// SendDefault` in `lib/src/protocol/mux/shared.rs`).
fn try_h2_backend_connect_failure_fails_over(frontend: Frontend) -> State {
    let front_address = create_local_address();
    let mut worker = start_worker(
        "H2-BACKEND-FAILOVER",
        Worker::into_config(FileConfig::default()),
        frontend,
        front_address,
    );
    let _backends = add_cluster(
        &mut worker,
        frontend,
        front_address,
        Cluster {
            http2: Some(true),
            ..Worker::default_cluster("cluster_0")
        },
        "/",
        &[
            Behaviour::HealthyH2,
            Behaviour::Blackholed,
            Behaviour::Refused,
            Behaviour::HealthyH2,
        ],
    );

    let statuses = send_requests(frontend, front_address, "/api", 12);
    println!("{frontend:?} statuses: {statuses:?}");
    stop(worker);

    if statuses.iter().any(|status| *status != 200) {
        println!("a request was not failed over to a healthy HTTP/2 backend");
        return State::Fail;
    }
    State::Success
}

#[test]
fn test_h2_backend_connect_failure_fails_over_h1() {
    assert_eq!(
        repeat_until_error_or(
            1,
            "H1 frontend, H2 backends: a failed dial goes to another backend",
            || try_h2_backend_connect_failure_fails_over(Frontend::Http1),
        ),
        State::Success
    );
}

#[test]
fn test_h2_backend_connect_failure_fails_over_h2() {
    assert_eq!(
        repeat_until_error_or(
            1,
            "H2 frontend, H2 backends: a failed dial goes to another backend",
            || try_h2_backend_connect_failure_fails_over(Frontend::Http2),
        ),
        State::Success
    );
}

// ── Attempt budget ──────────────────────────────────────────────────────────

/// Four refused backends and a healthy one, in the order under which the
/// first request of a fresh cluster reaches the healthy backend on exactly
/// its fifth attempt under round robin: each refused backend leaves the
/// candidate set once it fails, and the cursor advances on every pick, so the
/// picks are backends 0, 2, 4, 1 and then 3, the healthy one.
const FIFTH_ATTEMPT: [Behaviour; 5] = [
    Behaviour::Refused,
    Behaviour::Refused,
    Behaviour::Refused,
    Behaviour::Healthy,
    Behaviour::Refused,
];

/// Add a fresh cluster laid out as [`FIFTH_ATTEMPT`] behind the path prefix
/// `/<cluster_id>`, with `max_connection_attempts` as its own budget, and
/// return the status of its first request.
fn first_request_of_fifth_attempt_cluster(
    worker: &mut Worker,
    front_address: SocketAddr,
    cluster_id: &str,
    max_connection_attempts: Option<u32>,
) -> (u16, Backends) {
    let path = format!("/{cluster_id}");
    let backends = add_cluster(
        worker,
        Frontend::Http1,
        front_address,
        Cluster {
            max_connection_attempts,
            ..Worker::default_cluster(cluster_id)
        },
        &path,
        &FIFTH_ATTEMPT,
    );
    let status = h1_get(front_address, &path).unwrap_or(0);
    println!("{cluster_id} (max_connection_attempts {max_connection_attempts:?}): {status}");
    (status, backends)
}

/// The default budget is five attempts: a request reaching its healthy
/// backend on the fifth attempt is answered `200`, and one more refused
/// backend than the budget allows (a cluster budget of four) answers `503`.
#[test]
fn test_default_connection_attempt_budget_is_five() {
    let front_address = create_local_address();
    let mut worker = start_worker(
        "ATTEMPTS-DEFAULT",
        Worker::into_config(FileConfig::default()),
        Frontend::Http1,
        front_address,
    );
    let (default_budget, _a) =
        first_request_of_fifth_attempt_cluster(&mut worker, front_address, "default", None);
    let (four, _b) =
        first_request_of_fifth_attempt_cluster(&mut worker, front_address, "four", Some(4));
    stop(worker);
    assert_eq!(
        default_budget, 200,
        "five attempts reach the healthy backend"
    );
    assert_eq!(
        four, 503,
        "four attempts run out before the healthy backend"
    );
}

/// A cluster's own budget wins over the global one, and a cluster that sets
/// none uses the global one. The global budget here is four.
#[test]
fn test_cluster_connection_attempt_budget_overrides_global() {
    let front_address = create_local_address();
    let mut config = Worker::into_config(FileConfig::default());
    config.max_connection_attempts = Some(4);
    let mut worker = start_worker("ATTEMPTS-OVERRIDE", config, Frontend::Http1, front_address);
    let (inherited, _a) =
        first_request_of_fifth_attempt_cluster(&mut worker, front_address, "inherits", None);
    let (overridden, _b) =
        first_request_of_fifth_attempt_cluster(&mut worker, front_address, "overrides", Some(5));
    stop(worker);
    assert_eq!(inherited, 503, "the global budget of four applies");
    assert_eq!(overridden, 200, "the cluster budget of five wins");
}

/// Both budgets change at runtime through the command channel: the global one
/// with `SetMaxConnectionAttempts`, a running cluster's by re-sending its
/// `AddCluster`, which is what `sozu cluster connection-attempts set` does.
#[test]
fn test_connection_attempt_budget_changes_at_runtime() {
    let front_address = create_local_address();
    let mut worker = start_worker(
        "ATTEMPTS-RUNTIME",
        Worker::into_config(FileConfig::default()),
        Frontend::Http1,
        front_address,
    );

    // A budget no request could use is refused, and changes nothing.
    worker.send_proxy_request_type(RequestType::SetMaxConnectionAttempts(0));
    let refused = read_answer(&mut worker);
    assert_eq!(
        refused.status,
        ResponseStatus::Failure as i32,
        "0 is refused"
    );

    worker.send_proxy_request_type(RequestType::SetMaxConnectionAttempts(4));
    let lowered = read_answer(&mut worker);
    assert_eq!(lowered.status, ResponseStatus::Ok as i32);
    let (global_four, _a) =
        first_request_of_fifth_attempt_cluster(&mut worker, front_address, "global-four", None);

    worker.send_proxy_request_type(RequestType::SetMaxConnectionAttempts(5));
    let raised = read_answer(&mut worker);
    assert_eq!(raised.status, ResponseStatus::Ok as i32);

    // A running cluster, first added without a budget of its own, then given
    // one before its first request.
    let path = "/updated";
    let _c = add_cluster(
        &mut worker,
        Frontend::Http1,
        front_address,
        Worker::default_cluster("updated"),
        path,
        &FIFTH_ATTEMPT,
    );
    worker.send_proxy_request_type(RequestType::AddCluster(Cluster {
        max_connection_attempts: Some(4),
        ..Worker::default_cluster("updated")
    }));
    let updated = read_answer(&mut worker);
    assert_eq!(updated.status, ResponseStatus::Ok as i32);
    let cluster_four = h1_get(front_address, path).unwrap_or(0);

    let (global_five, _d) =
        first_request_of_fifth_attempt_cluster(&mut worker, front_address, "global-five", None);
    stop(worker);

    assert_eq!(
        global_four, 503,
        "the global budget lowered to four applies"
    );
    assert_eq!(
        cluster_four, 503,
        "the running cluster's new budget of four applies"
    );
    assert_eq!(
        global_five, 200,
        "the global budget raised back to five applies"
    );
}

// ── Overall deadline ────────────────────────────────────────────────────────

/// Failover is bounded by the listener's `front_timeout`, armed when the
/// request was first linked to a backend, not by `max_connection_attempts`
/// times `connect_timeout`: re-linking a request must not push that deadline
/// out. Six blackholed backends, a budget of 20 attempts and a 3-second
/// `front_timeout` with a 1-second `connect_timeout`: the client must get an
/// answer — the frontend timeout's `504` — within about `front_timeout`, not
/// after six or more connect timeouts.
fn try_failover_is_bounded_by_front_timeout(frontend: Frontend) -> State {
    const FRONT_TIMEOUT: u32 = 3;
    let front_address = create_local_address();
    let mut worker = start_worker_with_front_timeout(
        "FAILOVER-DEADLINE",
        Worker::into_config(FileConfig::default()),
        frontend,
        front_address,
        Some(FRONT_TIMEOUT),
    );
    let _backends = add_cluster(
        &mut worker,
        frontend,
        front_address,
        Cluster {
            max_connection_attempts: Some(20),
            ..Worker::default_cluster("cluster_0")
        },
        "/",
        &[Behaviour::Blackholed; 6],
    );

    let started = Instant::now();
    let statuses = send_requests(frontend, front_address, "/api", 1);
    let elapsed = started.elapsed();
    println!("{frontend:?} statuses: {statuses:?} after {elapsed:?}");
    stop(worker);

    if elapsed > Duration::from_secs(u64::from(FRONT_TIMEOUT) + 2) {
        println!("failover outlived front_timeout ({FRONT_TIMEOUT}s): {elapsed:?}");
        return State::Fail;
    }
    if statuses != [504] {
        println!("expected the frontend timeout's 504, got {statuses:?}");
        return State::Fail;
    }
    State::Success
}

#[test]
fn test_failover_is_bounded_by_front_timeout_h1() {
    assert_eq!(
        repeat_until_error_or(1, "H1: front_timeout bounds the whole failover", || {
            try_failover_is_bounded_by_front_timeout(Frontend::Http1)
        },),
        State::Success
    );
}

#[test]
fn test_failover_is_bounded_by_front_timeout_h2() {
    assert_eq!(
        repeat_until_error_or(1, "H2: front_timeout bounds the whole failover", || {
            try_failover_is_bounded_by_front_timeout(Frontend::Http2)
        },),
        State::Success
    );
}
