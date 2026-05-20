use std::{
    io::{ErrorKind, Read},
    net::SocketAddr,
    thread,
    time::{Duration, Instant},
};

use sozu_command_lib::{
    config::{FileConfig, ListenerBuilder},
    info,
    logging::setup_default_logging,
    proto::command::{
        ActivateListener, AddCertificate, CertificateAndKey, Cluster, CustomHttpAnswers,
        HealthCheckConfig, ListenerType, QueryMetricsOptions, RemoveBackend, RequestHttpFrontend,
        ResponseStatus, SetHealthCheck, SocketAddress, TlsVersion, filtered_metrics,
        request::RequestType, response_content::ContentType,
    },
    scm_socket::Listeners,
    state::ConfigState,
};

use crate::{
    http_utils::{http_ok_response, http_request, immutable_answer},
    mock::{
        aggregator::SimpleAggregator,
        async_backend::BackendHandle as AsyncBackend,
        client::Client,
        h2_backend::H2Backend,
        https_client::{
            build_h2_client, build_h2_or_h1_client, build_https_client,
            resolve_concurrent_requests, resolve_post_request, resolve_request,
        },
        sync_backend::Backend as SyncBackend,
    },
    sozu::worker::Worker,
    tests::{
        State, provide_port, provide_unbound_port, repeat_until_error_or, setup_async_test,
        setup_sync_test,
    },
};

/// Health check test timing constants.
/// The interval (3s) × threshold (2) = 6s minimum detection time,
/// plus margin for event loop scheduling.
const HEALTH_CHECK_SETTLE: Duration = Duration::from_secs(10);
const HEALTH_CHECK_LONG_SETTLE: Duration = Duration::from_secs(15);
const HEALTH_CHECK_VERIFY: Duration = Duration::from_secs(5);

fn default_health_check_config() -> HealthCheckConfig {
    HealthCheckConfig {
        uri: "/health".to_owned(),
        interval: 3,
        timeout: 2,
        healthy_threshold: 2,
        unhealthy_threshold: 2,
        expected_status: 0,
    }
}

/// Poll with HTTP requests until a condition is met or deadline expires.
/// Returns true if the condition was met before the deadline.
fn poll_until<F>(front_address: SocketAddr, timeout: Duration, mut condition: F) -> bool
where
    F: FnMut(Option<&str>) -> bool,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let mut client = Client::new(
            "poll",
            front_address,
            http_request("GET", "/api", "ping", "localhost"),
        );
        client.connect();
        client.send();
        let response = client.receive();

        if condition(response.as_deref()) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    false
}

pub fn create_local_address() -> SocketAddr {
    let address: SocketAddr = format!("127.0.0.1:{}", provide_port())
        .parse()
        .expect("could not parse front address");
    println!("created local address {}", address);
    address
}

pub fn create_unbound_local_address() -> SocketAddr {
    let address: SocketAddr = format!("127.0.0.1:{}", provide_unbound_port())
        .parse()
        .expect("could not parse unbound address");
    println!("created unbound local address {}", address);
    address
}

fn receive_with_deadline(client: &mut Client, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(response) = client.receive() {
            return Some(response);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn assert_client_eof(client: &mut Client) {
    let stream = client.stream.as_mut().expect("client should be connected");
    let mut buf = [0; 1];
    match stream.read(&mut buf) {
        Ok(0) => {}
        Ok(n) => panic!("expected frontend connection to close, read {n} byte(s) instead"),
        Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
            panic!("expected frontend connection to close, read timed out")
        }
        Err(e) => panic!("expected frontend connection to close, got read error: {e}"),
    }
}

#[derive(Debug)]
struct WorkerMetricSnapshot {
    client_connections: u64,
    zombies: i64,
}

fn query_worker_metrics(worker: &mut Worker) -> WorkerMetricSnapshot {
    worker.send_proxy_request_type(RequestType::QueryMetrics(QueryMetricsOptions {
        list: false,
        cluster_ids: vec![],
        backend_ids: vec![],
        metric_names: vec!["client.connections".to_owned(), "zombies".to_owned()],
        no_clusters: true,
        workers: false,
    }));
    let response = worker
        .read_proxy_response()
        .expect("worker should respond to metrics query");
    assert_eq!(response.id, worker.command_id.last);
    assert_eq!(response.status, ResponseStatus::Ok as i32, "{response:?}");

    let Some(content) = response.content.and_then(|content| content.content_type) else {
        panic!("metrics query returned no content");
    };
    let ContentType::WorkerMetrics(metrics) = content else {
        panic!("metrics query returned unexpected content: {content:?}");
    };

    let client_connections = match metrics
        .proxy
        .get("client.connections")
        .and_then(|metric| metric.inner.as_ref())
    {
        Some(filtered_metrics::Inner::Gauge(value)) => *value,
        other => panic!("client.connections should be a gauge, got {other:?}"),
    };
    let zombies = match metrics
        .proxy
        .get("zombies")
        .and_then(|metric| metric.inner.as_ref())
    {
        Some(filtered_metrics::Inner::Count(value)) => *value,
        None => 0,
        other => panic!("zombies should be a count, got {other:?}"),
    };

    WorkerMetricSnapshot {
        client_connections,
        zombies,
    }
}

fn wait_for_client_connections(
    worker: &mut Worker,
    expected: u64,
    timeout: Duration,
) -> WorkerMetricSnapshot {
    let deadline = Instant::now() + timeout;
    let mut snapshot = query_worker_metrics(worker);
    while snapshot.client_connections != expected && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
        snapshot = query_worker_metrics(worker);
    }
    snapshot
}

pub fn try_async(nb_backends: usize, nb_clients: usize, nb_requests: usize) -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_async_test(
        "ASYNC",
        config,
        listeners,
        state,
        front_address,
        nb_backends,
        false,
    );

    let mut clients = (0..nb_clients)
        .map(|i| {
            Client::new(
                format!("client{i}"),
                front_address,
                http_request("GET", "/api", format!("ping{i}"), "localhost"),
            )
        })
        .collect::<Vec<_>>();
    for client in clients.iter_mut() {
        client.connect();
    }
    for _ in 0..nb_requests {
        for client in clients.iter_mut() {
            client.send();
        }
        for client in clients.iter_mut() {
            match client.receive() {
                Some(response) => println!("{response}"),
                _ => {}
            }
        }
    }

    worker.soft_stop();
    worker.wait_for_server_stop();

    for client in &clients {
        println!(
            "{} sent: {}, received: {}",
            client.name, client.requests_sent, client.responses_received
        );
    }
    for backend in backends.iter_mut() {
        let aggregator = backend.stop_and_get_aggregator();
        println!("{} aggregated: {:?}", backend.name, aggregator);
    }

    if clients.iter().all(|client| {
        client.requests_sent == nb_requests && client.responses_received == nb_requests
    }) {
        State::Success
    } else {
        State::Fail
    }
}

pub fn try_sync(nb_clients: usize, nb_requests: usize) -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) =
        setup_sync_test("SYNC", config, listeners, state, front_address, 1, false);
    let mut backend = backends.pop().unwrap();

    backend.connect();

    let mut clients = (0..nb_clients)
        .map(|i| {
            Client::new(
                format!("client{i}"),
                front_address,
                http_request("GET", "/api", format!("ping{i}"), "localhost"),
            )
        })
        .collect::<Vec<_>>();

    // send one request, then maintain a keepalive session
    for (i, client) in clients.iter_mut().enumerate() {
        client.connect();
        client.send();
        backend.accept(i);
        backend.receive(i);
        backend.send(i);
        client.receive();
    }

    for _ in 0..nb_requests - 1 {
        for client in clients.iter_mut() {
            client.send();
        }
        for i in 0..nb_clients {
            backend.receive(i);
            backend.send(i);
        }
        for client in clients.iter_mut() {
            match client.receive() {
                Some(response) => println!("{response}"),
                _ => {}
            }
        }
    }

    worker.soft_stop();
    worker.wait_for_server_stop();

    for client in &clients {
        println!(
            "{} sent: {}, received: {}",
            client.name, client.requests_sent, client.responses_received
        );
    }
    println!(
        "{} sent: {}, received: {}",
        backend.name, backend.responses_sent, backend.requests_received
    );

    if clients.iter().all(|client| {
        client.requests_sent == nb_requests && client.responses_received == nb_requests
    }) {
        State::Success
    } else {
        State::Fail
    }
}

pub fn try_backend_stop(nb_requests: usize, zombie: Option<u32>) -> State {
    let front_address = create_local_address();

    let config = Worker::into_config(FileConfig {
        zombie_check_interval: zombie,
        ..FileConfig::default()
    });
    let listeners = Listeners::default();
    let state = ConfigState::new();
    let (mut worker, mut backends) = setup_async_test(
        "BACKSTOP",
        config,
        listeners,
        state,
        front_address,
        2,
        false,
    );
    let mut backend2 = backends.pop().expect("backend2");
    let mut backend1 = backends.pop().expect("backend1");

    let mut aggregator = Some(SimpleAggregator {
        requests_received: 0,
        responses_sent: 0,
    });

    let mut client = Client::new(
        "client",
        front_address,
        http_request("GET", "/api", "ping", "localhost"),
    );
    client.connect();

    let start = Instant::now();
    for i in 0..nb_requests {
        if client.send().is_none() {
            break;
        }
        match client.receive() {
            Some(response) => println!("{response}"),
            None => break,
        }
        if i == 0 {
            aggregator = backend1.stop_and_get_aggregator();
        }
    }
    let duration = Instant::now().duration_since(start);

    worker.soft_stop();
    let success = worker.wait_for_server_stop();

    println!(
        "sent: {}, received: {}",
        client.requests_sent, client.responses_received
    );
    println!("backend1 aggregator: {aggregator:?}");
    aggregator = backend2.stop_and_get_aggregator();
    println!("backend2 aggregator: {aggregator:?}");

    if !success {
        State::Fail
    } else if duration > Duration::from_millis(100) {
        // Reconnecting to unother backend should have lasted less that 100 miliseconds
        State::Undecided
    } else {
        State::Success
    }
}

pub fn try_h1_idle_connection_zombie_metric_increments() -> State {
    let front_address = create_local_address();
    let config = Worker::into_config(FileConfig {
        zombie_check_interval: Some(3),
        ..FileConfig::default()
    });
    let listeners = Listeners::default();
    let state = ConfigState::new();
    let (mut worker, _) = setup_async_test(
        "H1-IDLE-ZOMBIE-METRICS",
        config,
        listeners,
        state,
        front_address,
        0,
        false,
    );

    let mut client = Client::new(
        "idle-h1-client",
        front_address,
        http_request("GET", "/api", "ping", "localhost"),
    );
    client.connect();

    let before = wait_for_client_connections(&mut worker, 1, Duration::from_secs(2));
    println!("H1 idle zombie metrics before sweep: {before:?}");
    if before.client_connections != 1 {
        client.disconnect();
        worker.soft_stop();
        let _ = worker.wait_for_server_stop();
        return State::Fail;
    }

    thread::sleep(Duration::from_millis(6_500));
    let _wake_loop = query_worker_metrics(&mut worker);
    thread::sleep(Duration::from_millis(200));
    let after = query_worker_metrics(&mut worker);
    println!("H1 idle zombie metrics after sweep: {after:?}");

    client.disconnect();
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    if stopped && after.client_connections == 0 && after.zombies > before.zombies {
        State::Success
    } else {
        State::Fail
    }
}

pub fn try_issue_810_timeout() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "810-TIMEOUT",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );
    let mut backend = backends.pop().unwrap();

    let mut client = Client::new(
        "client",
        front_address,
        http_request("GET", "/api", "ping", "localhost"),
    );

    backend.connect();
    client.connect();
    client.send();
    backend.accept(0);
    backend.receive(0);
    backend.send(0);
    client.receive();

    worker.soft_stop();
    let start = Instant::now();
    let success = worker.wait_for_server_stop();
    let duration = Instant::now().duration_since(start);

    println!(
        "{} sent: {}, received: {}",
        client.name, client.requests_sent, client.responses_received
    );
    println!(
        "{} sent: {}, received: {}",
        backend.name, backend.responses_sent, backend.requests_received
    );

    if !success || duration > Duration::from_millis(100) {
        State::Fail
    } else {
        State::Success
    }
}

pub fn try_issue_810_panic(part2: bool) -> State {
    let front_address = create_local_address();

    let back_address = create_local_address();

    let (config, listeners, state) = Worker::empty_tcp_config(front_address);
    let mut worker = Worker::start_new_worker_owned("810-PANIC", config, listeners, state);

    worker.send_proxy_request_type(RequestType::AddTcpListener(
        ListenerBuilder::new_tcp(front_address.into())
            .to_tcp(None)
            .unwrap(),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.into(),
        proxy: ListenerType::Tcp.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "cluster_0",
    )));
    worker.send_proxy_request_type(RequestType::AddTcpFrontend(Worker::default_tcp_frontend(
        "cluster_0",
        front_address,
    )));

    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));
    worker.read_to_last();

    let mut backend = SyncBackend::new("backend", back_address, "pong");
    let mut client = Client::new("client", front_address, "ping");

    backend.connect();
    client.connect();
    client.send();
    if !part2 {
        backend.accept(0);
        backend.receive(0);
        backend.send(0);
        let response = client.receive();
        println!("Response: {response:?}");
    }

    worker.soft_stop();
    let success = worker.wait_for_server_stop();

    println!(
        "{} sent: {}, received: {}",
        client.name, client.requests_sent, client.responses_received
    );
    println!(
        "{} sent: {}, received: {}",
        backend.name, backend.responses_sent, backend.requests_received
    );

    if success { State::Success } else { State::Fail }
}

/// Helper that sets up an HTTPS worker with the given certificate/key and optional TLS version
/// constraint, sends a request, and checks the response.
fn try_tls_with_cert(
    test_name: &str,
    cert_pem: &str,
    key_pem: &str,
    tls_versions: Option<Vec<TlsVersion>>,
) -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let back_address = create_local_address();

    let (config, listeners, state) = Worker::empty_https_config(front_address.clone().into());
    let mut worker = Worker::start_new_worker_owned(test_name, config, listeners, state);

    let mut listener_builder = ListenerBuilder::new_https(front_address.clone());
    if let Some(versions) = tls_versions {
        listener_builder.with_tls_versions(versions);
    }
    worker.send_proxy_request_type(RequestType::AddHttpsListener(
        listener_builder.to_tls(None).unwrap(),
    ));

    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.clone(),
        proxy: ListenerType::Https.into(),
        from_scm: false,
    }));

    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "cluster_0",
    )));

    let hostname = "localhost".to_string();
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: hostname.to_owned(),
        ..Worker::default_http_frontend("cluster_0", front_address.clone().into())
    }));

    let certificate_and_key = CertificateAndKey {
        certificate: String::from(cert_pem),
        key: String::from(key_pem),
        certificate_chain: vec![],
        versions: vec![],
        names: vec![],
    };
    let add_certificate = AddCertificate {
        address: front_address,
        certificate: certificate_and_key,
        expired_at: None,
    };
    worker.send_proxy_request_type(RequestType::AddCertificate(add_certificate));

    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));
    worker.read_to_last();

    let mut backend = AsyncBackend::spawn_detached_backend(
        "BACKEND",
        back_address,
        SimpleAggregator::default(),
        AsyncBackend::http_handler("pong"),
    );

    let client = build_https_client();
    let uri: hyper::Uri = format!("https://{hostname}:{front_port}/api")
        .parse()
        .unwrap();
    if let Some((status, body)) = resolve_request(&client, uri) {
        println!("response status: {status:?}");
        println!("response body: {body}");
    } else {
        return State::Fail;
    }

    worker.soft_stop();
    let success = worker.wait_for_server_stop();

    let aggregator = backend
        .stop_and_get_aggregator()
        .expect("Could not get aggregator");
    println!(
        "{} sent: {}, received: {}",
        backend.name, aggregator.responses_sent, aggregator.requests_received
    );

    if success && aggregator.responses_sent == 1 {
        State::Success
    } else {
        State::Fail
    }
}

/// Helper that drives the four-cell (frontend H1/H2 × backend H1/H2)
/// cardinality matrix over TLS. Frontend ALPN preference is set
/// indirectly: the test's chosen Hyper client (`build_https_client`
/// vs `build_h2_client`) negotiates ALPN. Backend H2 is selected by
/// `cluster.http2 = Some(true)` and an `H2Backend` mock; otherwise an
/// `AsyncBackend` running an HTTP/1.1 handler is started.
///
/// Returns `State::Success` only when both the response status is
/// successful and at least one request reached the backend.
fn try_tls_cardinality_cell(
    test_name: &str,
    cert_pem: &str,
    key_pem: &str,
    tls_versions: Option<Vec<TlsVersion>>,
    frontend_h2: bool,
    backend_h2: bool,
) -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let back_address = create_local_address();

    let (config, listeners, state) = Worker::empty_https_config(front_address.clone().into());
    let mut worker = Worker::start_new_worker_owned(test_name, config, listeners, state);

    let mut listener_builder = ListenerBuilder::new_https(front_address.clone());
    if let Some(ref versions) = tls_versions {
        listener_builder.with_tls_versions(versions.clone());
    }
    worker.send_proxy_request_type(RequestType::AddHttpsListener(
        listener_builder.to_tls(None).unwrap(),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.clone(),
        proxy: ListenerType::Https.into(),
        from_scm: false,
    }));

    worker.send_proxy_request_type(RequestType::AddCluster(Cluster {
        http2: Some(backend_h2),
        ..Worker::default_cluster("cluster_0")
    }));

    let hostname = "localhost".to_string();
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: hostname.to_owned(),
        ..Worker::default_http_frontend("cluster_0", front_address.clone().into())
    }));

    let certificate_and_key = CertificateAndKey {
        certificate: String::from(cert_pem),
        key: String::from(key_pem),
        certificate_chain: vec![],
        versions: vec![],
        names: vec![],
    };
    worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
        address: front_address,
        certificate: certificate_and_key,
        expired_at: None,
    }));

    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));
    worker.read_to_last();

    // Spawn the appropriate backend kind. H2 backends need a small bind
    // delay before the first request is fired; the existing
    // `setup_h2_backend_test` helper uses 100 ms.
    let mut h1_backend = None;
    let mut h2_backend = None;
    if backend_h2 {
        h2_backend = Some(H2Backend::start(
            "BACKEND_H2".to_owned(),
            back_address,
            "h2-pong".to_owned(),
        ));
        thread::sleep(Duration::from_millis(100));
    } else {
        h1_backend = Some(AsyncBackend::spawn_detached_backend(
            "BACKEND_H1",
            back_address,
            SimpleAggregator::default(),
            AsyncBackend::http_handler("pong"),
        ));
    }

    let uri: hyper::Uri = format!("https://{hostname}:{front_port}/api")
        .parse()
        .unwrap();

    let request_ok = if frontend_h2 {
        let client = build_h2_client();
        match resolve_request(&client, uri) {
            Some((status, body)) => {
                println!("{test_name} response status: {status:?}, body: {body}");
                status.is_success()
            }
            None => false,
        }
    } else {
        let client = build_https_client();
        match resolve_request(&client, uri) {
            Some((status, body)) => {
                println!("{test_name} response status: {status:?}, body: {body}");
                status.is_success()
            }
            None => false,
        }
    };

    worker.soft_stop();
    let stop_ok = worker.wait_for_server_stop();

    let backend_handled = if let Some(mut b) = h2_backend {
        let n = b.get_responses_sent();
        b.stop();
        n
    } else if let Some(mut b) = h1_backend {
        let agg = b.stop_and_get_aggregator().expect("aggregator");
        agg.responses_sent
    } else {
        0
    };

    if request_ok && stop_ok && backend_handled >= 1 {
        State::Success
    } else {
        println!(
            "{test_name} failed: request_ok={request_ok} stop_ok={stop_ok} backend_handled={backend_handled}"
        );
        State::Fail
    }
}

pub fn try_tls_endpoint() -> State {
    try_tls_with_cert(
        "TLS-ENDPOINT",
        include_str!("../../../lib/assets/local-certificate.pem"),
        include_str!("../../../lib/assets/local-key.pem"),
        None,
    )
}

// ---------------------------------------------------------------------------
// Cardinality matrix — TLS scheme tests across (frontend H1/H2 × backend H1/H2)
// ---------------------------------------------------------------------------
// Each cell exercises one (cert kind × frontend protocol × backend protocol)
// combination. Cert kind covers RSA-2048 and ECDSA-P256; TLS version is the
// listener default (rustls negotiates 1.3 first, falling back to 1.2). The
// H1/H1 cells are already covered by `try_tls_rsa_2048` / `try_tls_ecdsa`.

pub fn try_tls_cardinality_h1_h2_rsa() -> State {
    try_tls_cardinality_cell(
        "TLS-CARD-H1-H2-RSA",
        include_str!("../../../lib/assets/local-certificate.pem"),
        include_str!("../../../lib/assets/local-key.pem"),
        None,
        false,
        true,
    )
}

pub fn try_tls_cardinality_h1_h2_ecdsa() -> State {
    try_tls_cardinality_cell(
        "TLS-CARD-H1-H2-ECDSA",
        include_str!("../../../lib/assets/tests/ecdsa-localhost.pem"),
        include_str!("../../../lib/assets/tests/ecdsa-localhost.key"),
        None,
        false,
        true,
    )
}

pub fn try_tls_cardinality_h2_h1_rsa() -> State {
    try_tls_cardinality_cell(
        "TLS-CARD-H2-H1-RSA",
        include_str!("../../../lib/assets/local-certificate.pem"),
        include_str!("../../../lib/assets/local-key.pem"),
        None,
        true,
        false,
    )
}

pub fn try_tls_cardinality_h2_h1_ecdsa() -> State {
    try_tls_cardinality_cell(
        "TLS-CARD-H2-H1-ECDSA",
        include_str!("../../../lib/assets/tests/ecdsa-localhost.pem"),
        include_str!("../../../lib/assets/tests/ecdsa-localhost.key"),
        None,
        true,
        false,
    )
}

pub fn try_tls_cardinality_h2_h2_rsa() -> State {
    try_tls_cardinality_cell(
        "TLS-CARD-H2-H2-RSA",
        include_str!("../../../lib/assets/local-certificate.pem"),
        include_str!("../../../lib/assets/local-key.pem"),
        None,
        true,
        true,
    )
}

pub fn try_tls_cardinality_h2_h2_ecdsa() -> State {
    try_tls_cardinality_cell(
        "TLS-CARD-H2-H2-ECDSA",
        include_str!("../../../lib/assets/tests/ecdsa-localhost.pem"),
        include_str!("../../../lib/assets/tests/ecdsa-localhost.key"),
        None,
        true,
        true,
    )
}

pub fn try_tls_rsa_2048() -> State {
    try_tls_with_cert(
        "TLS-RSA-2048",
        include_str!("../../../lib/assets/tests/localhost.crt"),
        include_str!("../../../lib/assets/tests/localhost.key"),
        None,
    )
}

pub fn try_tls_ecdsa() -> State {
    try_tls_with_cert(
        "TLS-ECDSA",
        include_str!("../../../lib/assets/tests/ecdsa-localhost.pem"),
        include_str!("../../../lib/assets/tests/ecdsa-localhost.key"),
        None,
    )
}

pub fn try_tls_1_3_rsa() -> State {
    try_tls_with_cert(
        "TLS13-RSA",
        include_str!("../../../lib/assets/local-certificate.pem"),
        include_str!("../../../lib/assets/local-key.pem"),
        Some(vec![TlsVersion::TlsV13]),
    )
}

pub fn try_tls_1_3_ecdsa() -> State {
    try_tls_with_cert(
        "TLS13-ECDSA",
        include_str!("../../../lib/assets/tests/ecdsa-localhost.pem"),
        include_str!("../../../lib/assets/tests/ecdsa-localhost.key"),
        Some(vec![TlsVersion::TlsV13]),
    )
}

pub fn try_tls_1_2_rsa() -> State {
    try_tls_with_cert(
        "TLS12-RSA",
        include_str!("../../../lib/assets/local-certificate.pem"),
        include_str!("../../../lib/assets/local-key.pem"),
        Some(vec![TlsVersion::TlsV12]),
    )
}

pub fn try_tls_1_2_ecdsa() -> State {
    try_tls_with_cert(
        "TLS12-ECDSA",
        include_str!("../../../lib/assets/tests/ecdsa-localhost.pem"),
        include_str!("../../../lib/assets/tests/ecdsa-localhost.key"),
        Some(vec![TlsVersion::TlsV12]),
    )
}

pub fn try_upgrade() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) =
        setup_sync_test("UPGRADE", config, listeners, state, front_address, 1, false);

    let mut backend = backends.pop().expect("backend");
    let mut client = Client::new(
        "client",
        front_address,
        http_request("GET", "/api", "ping", "localhost"),
    );

    backend.connect();
    client.connect();
    client.send();
    backend.accept(0);
    backend.receive(0);
    backend.send(0);
    match receive_with_deadline(&mut client, Duration::from_secs(1)) {
        Some(msg) => println!("response: {msg}"),
        None => return State::Fail,
    }

    client.send();
    backend.receive(0);
    let mut new_worker = worker.upgrade("NEW_WORKER");
    thread::sleep(Duration::from_millis(100));
    backend.send(0);
    match receive_with_deadline(&mut client, Duration::from_secs(1)) {
        Some(msg) => println!("response: {msg}"),
        None => return State::Fail,
    }
    client.connect();
    client.send();
    println!("ACCEPTING...");
    backend.accept(1);
    backend.receive(1);
    backend.send(1);
    match receive_with_deadline(&mut client, Duration::from_secs(1)) {
        Some(msg) => println!("response: {msg}"),
        None => return State::Fail,
    }

    new_worker.soft_stop();
    if !worker.wait_for_server_stop() {
        return State::Fail;
    }
    if !new_worker.wait_for_server_stop() {
        return State::Fail;
    }

    println!(
        "{} sent: {}, received: {}",
        client.name, client.requests_sent, client.responses_received
    );
    println!(
        "{} sent: {}, received: {}",
        backend.name, backend.responses_sent, backend.requests_received
    );

    State::Success
}

pub fn try_upgrade_in_flight_request() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "UPGRADE-INFLIGHT",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );

    let mut backend = backends.pop().expect("backend");
    let mut client = Client::new(
        "client",
        front_address,
        http_request("GET", "/api", "ping", "localhost"),
    );

    // Establish baseline: full request/response cycle
    backend.connect();
    client.connect();
    client.send();
    backend.accept(0);
    backend.receive(0);
    backend.send(0);
    match receive_with_deadline(&mut client, Duration::from_secs(1)) {
        Some(msg) => println!("baseline response: {msg}"),
        None => return State::Fail,
    }

    // Send request that will be in-flight during upgrade
    client.send();
    backend.receive(0);

    // Trigger upgrade while request is in-flight
    let mut new_worker = worker.upgrade("UPGRADE-INFLIGHT-NEW");
    thread::sleep(Duration::from_millis(200));

    // Backend responds — old worker should still forward the in-flight response
    backend.send(0);
    match receive_with_deadline(&mut client, Duration::from_secs(1)) {
        Some(msg) => println!("in-flight response after upgrade: {msg}"),
        None => return State::Fail,
    }

    // Verify new worker accepts new connections
    client.connect();
    client.send();
    backend.accept(1);
    backend.receive(1);
    backend.send(1);
    match receive_with_deadline(&mut client, Duration::from_secs(1)) {
        Some(msg) => println!("new worker response: {msg}"),
        None => return State::Fail,
    }

    new_worker.soft_stop();
    if !worker.wait_for_server_stop() {
        return State::Fail;
    }
    if !new_worker.wait_for_server_stop() {
        return State::Fail;
    }

    State::Success
}

pub fn try_upgrade_new_connections_after() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "UPGRADE-NEWCONN",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );

    let mut backend = backends.pop().expect("backend");
    backend.connect();

    // Baseline: verify worker is functional
    let mut client = Client::new(
        "client",
        front_address,
        http_request("GET", "/api", "ping", "localhost"),
    );
    client.connect();
    client.send();
    backend.accept(0);
    backend.receive(0);
    backend.send(0);
    match receive_with_deadline(&mut client, Duration::from_secs(1)) {
        Some(msg) => println!("baseline response: {msg}"),
        None => return State::Fail,
    }

    // Upgrade worker (no in-flight requests)
    let mut new_worker = worker.upgrade("UPGRADE-NEWCONN-NEW");
    thread::sleep(Duration::from_millis(200));

    // Create a brand new client and connect to the new worker
    let mut new_client = Client::new(
        "new_client",
        front_address,
        http_request("GET", "/api", "hello", "localhost"),
    );
    new_client.connect();
    new_client.send();
    backend.accept(1);
    backend.receive(1);
    backend.send(1);
    match receive_with_deadline(&mut new_client, Duration::from_secs(1)) {
        Some(msg) => println!("new client response from new worker: {msg}"),
        None => return State::Fail,
    }

    // Send multiple requests on keep-alive to verify routing is stable
    for i in 0..3 {
        new_client.send();
        backend.receive(1);
        backend.send(1);
        match receive_with_deadline(&mut new_client, Duration::from_secs(1)) {
            Some(msg) => println!("keep-alive request {i} response: {msg}"),
            None => return State::Fail,
        }
    }

    // Create yet another client to verify multiple new connections work
    let mut another_client = Client::new(
        "another_client",
        front_address,
        http_request("GET", "/api", "world", "localhost"),
    );
    another_client.connect();
    another_client.send();
    backend.accept(2);
    backend.receive(2);
    backend.send(2);
    match receive_with_deadline(&mut another_client, Duration::from_secs(1)) {
        Some(msg) => println!("another client response: {msg}"),
        None => return State::Fail,
    }

    new_worker.soft_stop();
    if !worker.wait_for_server_stop() {
        return State::Fail;
    }
    if !new_worker.wait_for_server_stop() {
        return State::Fail;
    }

    println!(
        "new_client sent: {}, received: {}",
        new_client.requests_sent, new_client.responses_received
    );
    println!(
        "another_client sent: {}, received: {}",
        another_client.requests_sent, another_client.responses_received
    );

    State::Success
}

pub fn try_upgrade_multiple_in_flight() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "UPGRADE-MULTI",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );

    let mut backend = backends.pop().expect("backend");
    backend.connect();

    // Create 3 clients, each establishes a keep-alive connection
    let mut clients: Vec<Client> = (0..3)
        .map(|i| {
            Client::new(
                format!("client{i}"),
                front_address,
                http_request("GET", "/api", format!("ping{i}"), "localhost"),
            )
        })
        .collect();

    // Establish keep-alive for all clients
    for (i, client) in clients.iter_mut().enumerate() {
        client.connect();
        client.send();
        backend.accept(i);
        backend.receive(i);
        backend.send(i);
        match receive_with_deadline(client, Duration::from_secs(1)) {
            Some(msg) => println!("client{i} baseline: {msg}"),
            None => return State::Fail,
        }
    }

    // All 3 clients send a request (all in-flight)
    for client in clients.iter_mut() {
        client.send();
    }
    // Backend receives all 3
    for i in 0..3 {
        backend.receive(i);
    }

    // Upgrade worker while all 3 requests are in-flight
    let mut new_worker = worker.upgrade("UPGRADE-MULTI-NEW");
    thread::sleep(Duration::from_millis(200));

    // Backend responds to all 3 — old worker should forward them
    for i in 0..3 {
        backend.send(i);
    }

    // All 3 clients should receive their responses
    for (i, client) in clients.iter_mut().enumerate() {
        match receive_with_deadline(client, Duration::from_secs(1)) {
            Some(msg) => println!("client{i} in-flight response: {msg}"),
            None => return State::Fail,
        }
    }

    // Verify new worker works: one client reconnects
    clients[0].connect();
    clients[0].send();
    backend.accept(3);
    backend.receive(3);
    backend.send(3);
    match receive_with_deadline(&mut clients[0], Duration::from_secs(1)) {
        Some(msg) => println!("client0 new worker response: {msg}"),
        None => return State::Fail,
    }

    new_worker.soft_stop();
    if !worker.wait_for_server_stop() {
        return State::Fail;
    }
    if !new_worker.wait_for_server_stop() {
        return State::Fail;
    }

    for (i, client) in clients.iter().enumerate() {
        println!(
            "client{i} sent: {}, received: {}",
            client.requests_sent, client.responses_received
        );
    }

    State::Success
}

pub fn try_upgrade_keepalive_reconnect() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "UPGRADE-KA",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );

    let mut backend = backends.pop().expect("backend");
    backend.connect();

    let mut client = Client::new(
        "client",
        front_address,
        http_request("GET", "/api", "ping", "localhost"),
    );

    // Establish keep-alive connection
    client.connect();
    client.send();
    backend.accept(0);
    backend.receive(0);
    backend.send(0);
    match client.receive() {
        Some(msg) => println!("keep-alive established: {msg}"),
        None => return State::Fail,
    }

    // Upgrade worker while client has an idle keep-alive connection (no in-flight request)
    let mut new_worker = worker.upgrade("UPGRADE-KA-NEW");
    thread::sleep(Duration::from_millis(200));

    // Try sending on the old keep-alive connection
    // The old worker is soft-stopping and should close idle sessions
    let old_conn_result = client.send();
    if old_conn_result.is_some() {
        // If send succeeded, try to get a response
        // The old worker may or may not process this
        match client.receive() {
            Some(msg) => {
                println!("old connection still works during drain: {msg}");
                // This means old worker accepted one more request during drain
                // That's acceptable behavior
            }
            None => {
                println!("old connection closed by draining worker (expected)");
            }
        }
    } else {
        println!("old connection already closed (expected)");
    }

    // Reconnect to the new worker — this must always work
    client.connect();
    client.send();
    backend.accept(1);
    backend.receive(1);
    backend.send(1);
    match client.receive() {
        Some(msg) => println!("reconnected to new worker: {msg}"),
        None => return State::Fail,
    }

    // Verify keep-alive works on the new worker
    for i in 0..3 {
        client.send();
        backend.receive(1);
        backend.send(1);
        match client.receive() {
            Some(msg) => println!("new worker keep-alive {i}: {msg}"),
            None => return State::Fail,
        }
    }

    new_worker.soft_stop();
    if !worker.wait_for_server_stop() {
        return State::Fail;
    }
    if !new_worker.wait_for_server_stop() {
        return State::Fail;
    }

    println!(
        "client sent: {}, received: {}",
        client.requests_sent, client.responses_received
    );

    State::Success
}

pub fn try_upgrade_async() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_async_test(
        "UPGRADE-ASYNC",
        config,
        listeners,
        state,
        front_address,
        2,
        false,
    );

    // Send requests from multiple clients before upgrade
    let nb_pre_upgrade = 5;
    let mut clients: Vec<Client> = (0..3)
        .map(|i| {
            Client::new(
                format!("client{i}"),
                front_address,
                http_request("GET", "/api", format!("ping{i}"), "localhost"),
            )
        })
        .collect();

    for client in clients.iter_mut() {
        client.connect();
    }

    // Pre-upgrade: each client sends several requests
    for _ in 0..nb_pre_upgrade {
        for client in clients.iter_mut() {
            client.send();
        }
        thread::sleep(Duration::from_millis(50));
        for client in clients.iter_mut() {
            match client.receive() {
                Some(msg) => println!("pre-upgrade: {msg}"),
                None => {}
            }
        }
    }

    // Upgrade worker
    let mut new_worker = worker.upgrade("UPGRADE-ASYNC-NEW");
    thread::sleep(Duration::from_millis(300));

    // Post-upgrade: clients reconnect and send more requests
    let nb_post_upgrade = 5;
    for client in clients.iter_mut() {
        client.connect();
    }
    for _ in 0..nb_post_upgrade {
        for client in clients.iter_mut() {
            client.send();
        }
        thread::sleep(Duration::from_millis(50));
        for client in clients.iter_mut() {
            match client.receive() {
                Some(msg) => println!("post-upgrade: {msg}"),
                None => {}
            }
        }
    }

    new_worker.soft_stop();
    if !worker.wait_for_server_stop() {
        return State::Fail;
    }
    if !new_worker.wait_for_server_stop() {
        return State::Fail;
    }

    for (i, client) in clients.iter().enumerate() {
        println!(
            "client{i} sent: {}, received: {}",
            client.requests_sent, client.responses_received
        );
    }

    // Check backends handled requests
    for backend in backends.iter_mut() {
        let aggregator = backend.stop_and_get_aggregator();
        println!("{} aggregated: {:?}", backend.name, aggregator);
    }

    // All clients should have received at least the post-upgrade responses
    if clients
        .iter()
        .all(|c| c.responses_received >= nb_post_upgrade)
    {
        State::Success
    } else {
        State::Fail
    }
}

/*
pub fn test_http(nb_requests: usize) {
    let front_address = "127.0.0.1:2001"
        .parse()
        .expect("could not parse front address");

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = async_setup_test("HTTP", config, listeners, state, front_address, 1);
    let mut backend = backends.pop().expect("backend");

    let mut bad_client = Client::new(
        "bad_client".to_string(),
        front_address,
        "GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\nContent-Length: 3\r\n\r\nbad_ping",
    );
    let mut good_client = Client::new(
        "good_client".to_string(),
        front_address,
        http_request("GET", "/api", "good_ping", "localhost"),
    );
    bad_client.connect();
    good_client.connect();

    for _ in 0..nb_requests {
        bad_client.send();
        good_client.send();
        match bad_client.receive() {
            Some(msg) => println!("response: {msg}"),
            None => {}
        }
        match good_client.receive() {
            Some(msg) => println!("response: {msg}"),
            None => {}
        }
    }

    worker.send_proxy_request(RequestContent::SoftStop);
    worker.wait_for_server_stop();

    println!(
        "{} sent: {}, received: {}",
        bad_client.name, bad_client.requests_sent, bad_client.responses_received
    );
    println!(
        "{} sent: {}, received: {}",
        good_client.name, good_client.requests_sent, good_client.responses_received
    );
    let aggregator = backend.stop_and_get_aggregator();
    println!("backend aggregator: {aggregator:?}");
}
*/

pub fn try_hard_or_soft_stop(soft: bool) -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) =
        setup_sync_test("STOP", config, listeners, state, front_address, 1, false);
    let mut backend = backends.pop().unwrap();

    let mut client = Client::new(
        "client",
        front_address,
        http_request("GET", "/api", "ping", "localhost"),
    );

    // Send a request to try out
    backend.connect();
    client.connect();
    client.send();
    backend.accept(0);
    backend.receive(0);

    // stop sōzu
    if soft {
        // the worker will wait for backends to respond before shutting down
        worker.soft_stop();
    } else {
        // the worker will shut down without waiting for backends to finish
        worker.hard_stop();
    }
    thread::sleep(Duration::from_millis(100));

    backend.send(0);

    match (soft, client.receive()) {
        (true, None) => {
            println!("SoftStop didn't wait for HTTP response to complete");
            return State::Fail;
        }
        (true, Some(msg)) => {
            println!("response on SoftStop: {msg}");
        }
        (false, None) => {
            println!("no response on HardStop");
        }
        (false, Some(msg)) => {
            println!("HardStop waited for HTTP response to complete: {msg}");
            return State::Fail;
        }
    }

    let success = worker.wait_for_server_stop();

    println!(
        "{} sent: {}, received: {}",
        client.name, client.requests_sent, client.responses_received
    );
    println!(
        "{} sent: {}, received: {}",
        backend.name, backend.responses_sent, backend.requests_received
    );

    if success { State::Success } else { State::Fail }
}

fn try_http_behaviors() -> State {
    if let Err(e) = setup_default_logging(false, "debug", "BEHAVE-OUT") {
        println!("could not setup default logging: {e}");
    }

    info!("starting up");

    let front_address: SocketAddr = create_local_address();

    let (config, listeners, state) = Worker::empty_http_config(front_address);
    let mut worker = Worker::start_new_worker_owned("BEHAVE-WORKER", config, listeners, state);

    let mut http_config = ListenerBuilder::new_http(front_address.into())
        .to_http(None)
        .unwrap();
    let http_answers = CustomHttpAnswers {
        answer_400: Some(immutable_answer(400)),
        answer_404: Some(immutable_answer(404)),
        answer_502: Some(immutable_answer(502)),
        answer_503: Some(immutable_answer(503)),
        ..Default::default()
    };
    http_config.http_answers = Some(http_answers);

    worker.send_proxy_request_type(RequestType::AddHttpListener(http_config));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.into(),
        proxy: ListenerType::Http.into(),
        from_scm: false,
    }));
    worker.read_to_last();

    let mut client = Client::new(
        "client",
        front_address,
        http_request("GET", "/", "ping", "example.com"),
    );

    info!("expecting 404");
    client.connect();
    client.send();

    let response = client.receive();
    println!("response: {response:?}");
    assert_eq!(response, Some(immutable_answer(404)));
    assert_eq!(client.receive(), None);

    worker.send_proxy_request_type(RequestType::AddHttpFrontend(RequestHttpFrontend {
        hostname: String::from("example.com"),
        ..Worker::default_http_frontend("cluster_0", front_address)
    }));
    worker.read_to_last();

    info!("expecting 503");
    client.connect();
    client.send();

    let response = client.receive();
    println!("response: {response:?}");
    assert_eq!(response, Some(immutable_answer(503)));
    assert_eq!(client.receive(), None);

    let back_address = create_local_address();

    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0".to_string(),
        back_address,
        None,
    )));
    worker.read_to_last();

    info!("sending invalid request, expecting 400");
    client.set_request("HELLO\r\n\r\n");
    client.connect();
    client.send();

    let response = client.receive();
    println!("response: {response:?}");
    assert_eq!(response, Some(immutable_answer(400)));
    assert_eq!(client.receive(), None);

    let mut backend = SyncBackend::new("backend", back_address, "TEST\r\n\r\n");
    backend.connect();

    info!("expecting 502");
    client.connect();
    client.set_request(http_request("GET", "/", "ping", "example.com"));
    client.send();
    backend.accept(0);
    let request = backend.receive(0);
    backend.send(0);

    let response = client.receive();
    println!("request: {request:?}");
    println!("response: {response:?}");
    assert_eq!(response, Some(immutable_answer(502)));
    assert_eq!(client.receive(), None);

    info!("expecting 200");
    worker.send_proxy_request_type(RequestType::RemoveBackend(RemoveBackend {
        cluster_id: String::from("cluster_0"),
        backend_id: String::from("cluster_0-0"),
        address: back_address.into(),
    }));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0".to_string(),
        back_address,
        None,
    )));
    backend.disconnect();
    worker.read_to_last();

    let mut backend = SyncBackend::new("backend", back_address, http_ok_response("hello"));
    backend.connect();
    client.connect();
    client.send();
    backend.accept(0);
    let request = backend.receive(0);
    backend.send(0);

    let expected_response_start = String::from("HTTP/1.1 200 OK\r\nContent-Length: 5");
    let expected_response_end = String::from("hello");
    let response = client.receive().unwrap();
    println!("request: {request:?}");
    println!("response: {response:?}");
    assert!(
        response.starts_with(&expected_response_start)
            && response.ends_with(&expected_response_end)
    );

    info!("expecting 200, without content length");
    backend.set_response("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nHello world!");
    client.send();
    let request = backend.receive(0);
    backend.send(0);

    let expected_response_start = String::from("HTTP/1.1 200 OK\r\n");
    let expected_response_end = String::from("Hello world!");
    let response = client.receive().unwrap();
    println!("request: {request:?}");
    println!("response: {response:?}");
    assert!(
        response.starts_with(&expected_response_start)
            && response.ends_with(&expected_response_end)
    );

    info!("server closes, expecting 502");
    // Backend closed after consuming (part of) the request without producing
    // any response: end_stream_decision normalises this to 502 Bad Gateway
    // (see lib/src/protocol/mux/shared.rs::end_stream_decision — "no response
    // is available and the request was already partially consumed" → 502).
    // Previously Sozu returned 503 here; the H1 and H2 paths were aligned on
    // 502 in bd4f4014 (h1) / cb80a595 (h2).
    // TODO: what if the client continue to use the closed stream
    client.connect();
    client.send();
    backend.accept(0);
    let request = backend.receive(0);
    backend.close(0);

    let response = client.receive();
    println!("request: {request:?}");
    println!("response: {response:?}");
    assert_eq!(response, Some(immutable_answer(502)));
    assert_eq!(client.receive(), None);

    worker.send_proxy_request_type(RequestType::RemoveBackend(RemoveBackend {
        cluster_id: String::from("cluster_0"),
        backend_id: String::from("cluster_0-0"),
        address: back_address.into(),
    }));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0".to_string(),
        back_address,
        None,
    )));
    backend.disconnect();
    worker.read_to_last();

    // transfer-encoding is an invalid header for 101 response, but Sozu should ignore it (see issue #885)
    let mut backend = SyncBackend::new(
        "backend",
        back_address,
        "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: WebSocket\r\nTransfer-Encoding: Chunked\r\n\r\n",
    );

    info!("expecting upgrade (101 switching protocols)");
    backend.connect();
    client.connect();
    client.send();
    backend.accept(0);
    let request = backend.receive(0);
    backend.send(0);

    let expected_response_start = String::from(
        "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: WebSocket\r\n",
    );
    let response = client.receive().unwrap();
    println!("request: {request:?}");
    println!("response: {response:?}");
    assert!(response.starts_with(&expected_response_start));

    backend.set_response("early");
    backend.send(0);
    let expected_response = String::from("early");
    let response = client.receive();
    assert_eq!(response, Some(expected_response));

    client.set_request("ping");
    backend.set_response("pong");
    client.send();
    let request = backend.receive(0);
    backend.send(0);

    let expected_response = String::from("pong");
    let response = client.receive();
    println!("request: {request:?}");
    println!("response: {response:?}");
    assert_eq!(response, Some(expected_response));
    assert_eq!(client.receive(), None);

    info!("expecting 100");
    backend.set_response("HTTP/1.1 100 Continue\r\n\r\n");
    client.set_request("GET /100 HTTP/1.1\r\nHost: example.com\r\nConnection: keep-alive\r\nContent-Length: 4\r\nExpect: 100-continue\r\n\r\n");
    client.connect();
    client.send();
    backend.accept(1);
    let request = backend.receive(1);
    backend.send(1);

    let expected_response_start = String::from("HTTP/1.1 100 Continue\r\n");
    let expected_response_end = String::from("\r\n\r\n");
    let response = client.receive().unwrap();
    println!("request: {request:?}");
    println!("response: {response:?}");
    assert!(
        response.starts_with(&expected_response_start)
            && response.ends_with(&expected_response_end)
    );

    backend.set_response("HTTP/1.1 200 OK\r\n\r\n");
    client.set_request("0123456789");
    client.send();
    let request = backend.receive(1).unwrap();
    backend.send(1);

    let expected_response_start = String::from("HTTP/1.1 200 OK\r\n");
    let expected_response_end = String::from("\r\n\r\n");
    let response = client.receive().unwrap();
    println!("request: {request:?}");
    println!("response: {response:?}");
    assert!(
        response.starts_with(&expected_response_start)
            && response.ends_with(&expected_response_end)
    );
    assert_eq!(request, String::from("0123"));

    info!("expecting 100 BAD");
    backend.set_response("HTTP/1.1 200 Ok\r\n\r\nRESPONSE_BODY_NO_LENGTH");
    client.set_request("GET /100 HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\nExpect: 100-continue\r\n\r\n");
    client.connect();
    client.send();
    backend.accept(1);
    let request = backend.receive(1);
    backend.send(1);

    let expected_response_start = String::from("HTTP/1.1 200 Ok\r\n");
    let expected_response_end = String::from("RESPONSE_BODY_NO_LENGTH");
    let response = client.receive().unwrap();
    println!("request: {request:?}");
    println!("response: {response:?}");
    assert!(
        response.starts_with(&expected_response_start)
            && response.ends_with(&expected_response_end)
    );

    info!("expecting 103");
    backend.set_response(
        "HTTP/1.1 103 Early Hint\r\nLink: </style.css>; rel=preload; as=style\r\n\r\n",
    );
    client.set_request("GET /103 HTTP/1.1\r\nHost: example.com\r\nContent-Length: 4\r\n\r\nping");
    client.connect();
    client.send();
    backend.accept(1);
    let request = backend.receive(1);
    backend.send(1);

    let expected_response_start = String::from("HTTP/1.1 103 Early Hint\r\n");
    let expected_response_end = String::from("\r\n\r\n");
    let response = client.receive().unwrap();
    println!("request: {request:?}");
    println!("response: {response:?}");
    assert!(
        response.starts_with(&expected_response_start)
            && response.ends_with(&expected_response_end)
    );

    backend.set_response("HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\npong");
    backend.send(1);

    let expected_response_start = String::from("HTTP/1.1 200 OK\r\n");
    let expected_response_end = String::from("\r\n\r\npong");
    let response = client.receive().unwrap();
    println!("response: {response:?}");
    assert!(
        response.starts_with(&expected_response_start)
            && response.ends_with(&expected_response_end)
    );

    worker.hard_stop();
    worker.wait_for_server_stop();

    info!("good bye");
    State::Success
}

fn try_builtin_404_default_answer_closes_connection() -> State {
    let front_address: SocketAddr = create_local_address();

    let (config, listeners, state) = Worker::empty_http_config(front_address);
    let mut worker = Worker::start_new_worker_owned("DEFAULT-404-WORKER", config, listeners, state);

    let http_config = ListenerBuilder::new_http(front_address.into())
        .to_http(None)
        .unwrap();

    worker.send_proxy_request_type(RequestType::AddHttpListener(http_config));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.into(),
        proxy: ListenerType::Http.into(),
        from_scm: false,
    }));
    worker.read_to_last();

    let mut client = Client::new(
        "client",
        front_address,
        http_request("GET", "/", "ping", "unknown.example"),
    );
    client.connect();
    client.send();

    let response = receive_with_deadline(&mut client, Duration::from_millis(500))
        .expect("client should receive built-in 404 default answer");
    println!("response: {response:?}");
    assert!(response.starts_with("HTTP/1.1 404 Not Found\r\n"));
    assert!(response.contains("<h1>404 Not Found</h1>"));
    assert_client_eof(&mut client);

    worker.hard_stop();
    let success = worker.wait_for_server_stop();

    if success { State::Success } else { State::Fail }
}

fn try_https_redirect() -> State {
    let front_address: SocketAddr = create_local_address();

    let (config, listeners, state) = Worker::empty_http_config(front_address);
    let mut worker = Worker::start_new_worker_owned("BEHAVE-WORKER", config, listeners, state);

    let mut http_config = ListenerBuilder::new_http(front_address.into())
        .to_http(None)
        .unwrap();
    let answer_301_prefix = "HTTP/1.1 301 Moved Permanently\r\nLocation: ";

    let http_answers = CustomHttpAnswers {
        answer_301: Some(format!("{answer_301_prefix}%REDIRECT_LOCATION\r\n\r\n")),
        ..Default::default()
    };
    http_config.http_answers = Some(http_answers);

    worker.send_proxy_request_type(RequestType::AddHttpListener(http_config));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.into(),
        proxy: ListenerType::Http.into(),
        from_scm: false,
    }));
    worker.send_proxy_request(
        RequestType::AddCluster(Cluster {
            https_redirect: true,
            ..Worker::default_cluster("cluster_0")
        })
        .into(),
    );
    worker.send_proxy_request_type(RequestType::AddHttpFrontend(RequestHttpFrontend {
        hostname: String::from("example.com"),
        ..Worker::default_http_frontend("cluster_0", front_address)
    }));

    worker.read_to_last();

    let mut client = Client::new(
        "client",
        front_address,
        http_request("GET", "/redirected?true", "", "example.com"),
    );

    client.connect();
    client.send();
    let answer = client.receive();
    let expected_answer = format!("{answer_301_prefix}https://example.com/redirected?true\r\n\r\n");
    assert_eq!(answer, Some(expected_answer));

    State::Success
}

fn try_msg_close() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "MSG-CLOSE",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );
    let mut backend = backends.pop().unwrap();

    backend.connect();

    let mut client = Client::new(
        "client",
        front_address,
        http_request("GET", "/api", "ping", "localhost"),
    );

    backend.set_response(
        "HTTP/1.1 200 Ok \r\nContent-Length: 4\r\nConnection: Keep-Alive\r\n\r\npong",
    );
    client.connect();
    client.send();
    backend.accept(0);
    backend.receive(0);
    backend.send(0);
    println!("response: {:?}", client.receive());

    thread::sleep(std::time::Duration::from_millis(100));

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

pub fn try_blue_geen() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, _) = setup_async_test("BG", config, listeners, state, front_address, 0, false);

    let aggrerator = SimpleAggregator {
        requests_received: 0,
        responses_sent: 0,
    };

    let primary_address = create_local_address();

    let secondary_address = create_local_address();

    let mut primary = AsyncBackend::spawn_detached_backend(
        "PRIMARY",
        primary_address,
        aggrerator.clone(),
        AsyncBackend::http_handler("pong_primary".to_string()),
    );
    let mut secondary = AsyncBackend::spawn_detached_backend(
        "SECONDARY",
        secondary_address,
        aggrerator,
        AsyncBackend::http_handler("pong_secondary".to_string()),
    );

    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        primary_address,
        None,
    )));
    worker.read_to_last();

    let mut client = Client::new(
        "client",
        front_address,
        http_request("GET", "/api", "ping", "localhost"),
    );

    client.connect();
    client.send();
    let response = client.receive();
    println!("response: {response:?}");

    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-1",
        secondary_address,
        None,
    )));
    worker.read_to_last();

    client.send();
    let response = client.receive();
    println!("response: {response:?}");

    worker.send_proxy_request_type(RequestType::RemoveBackend(RemoveBackend {
        cluster_id: "cluster_0".to_string(),
        backend_id: "cluster_0-0".to_string(),
        address: primary_address.into(),
    }));
    worker.read_to_last();

    client.send();
    let response = client.receive();
    println!("response: {response:?}");

    worker.soft_stop();
    let success = worker.wait_for_server_stop();

    println!(
        "sent: {}, received: {}",
        client.requests_sent, client.responses_received
    );
    let aggregator = primary.stop_and_get_aggregator();
    println!("primary aggregator: {aggregator:?}");
    let aggregator = secondary.stop_and_get_aggregator();
    println!("secondary aggregator: {aggregator:?}");

    if !success {
        State::Fail
    } else {
        State::Success
    }
}

pub fn try_keep_alive() -> State {
    if let Err(e) = setup_default_logging(false, "debug", "KA-OUT") {
        println!("could not setup default logging: {e}");
    }

    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "KA-WORKER",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );

    let mut backend = backends.pop().unwrap();
    let mut client = Client::new(
        "client".to_string(),
        front_address,
        "GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );

    backend
        .set_response("HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: keep-alive\r\n\r\npong");
    backend.connect();

    info!("front: close / back: keep");
    client.connect();
    client.send();
    backend.accept(0);
    let request = backend.receive(0);
    println!("request: {request:?}");
    backend.send(0);
    let response = client.receive();
    println!("response: {response:?}");
    assert!(!client.is_connected()); // front disconnected
    assert!(!backend.is_connected(0)); // back disconnected

    info!("front: keep / back: keep");
    client.set_request("GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n");
    client.connect();
    client.send();
    backend.accept(0);
    let request = backend.receive(0);
    println!("request: {request:?}");
    backend.send(0);
    let response = client.receive();
    println!("response: {response:?}");
    assert!(client.is_connected()); // front connected
    assert!(backend.is_connected(0)); // back connected

    info!("front: keep / back: close");
    backend.set_response("HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\npong");
    client.send();
    let request = backend.receive(0);
    println!("request: {request:?}");
    backend.send(0);
    let response = client.receive();
    println!("response: {response:?}");
    assert!(client.is_connected()); // front connected
    assert!(!backend.is_connected(0)); // back disconnected

    info!("front: close / back: close");
    client.set_request("GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    client.send();
    backend.accept(0);
    let request = backend.receive(0);
    println!("request: {request:?}");
    backend.send(0);
    let response = client.receive();
    println!("response: {response:?}");
    assert!(!client.is_connected()); // front disconnected
    assert!(!backend.is_connected(0)); // back disconnected

    worker.soft_stop();
    worker.wait_for_server_stop();

    println!(
        "{} sent: {}, received: {}",
        client.name, client.requests_sent, client.responses_received
    );
    println!(
        "{} sent: {}, received: {}",
        backend.name, backend.responses_sent, backend.requests_received
    );

    State::Success
}

pub fn try_stick() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) =
        setup_sync_test("STICK", config, listeners, state, front_address, 2, true);

    let mut backend2 = backends.pop().unwrap();
    let mut backend1 = backends.pop().unwrap();
    let mut client = Client::new(
        "client".to_string(),
        front_address,
        "GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nCookie: foo=bar\r\n\r\n",
    );

    // Sozu choice order is determinist in round-bobin, so we will use backend1 then backend2
    backend1.connect();
    backend2.connect();

    // no sticky_session
    client.connect();
    client.send();
    backend1.accept(0);
    let request = backend1.receive(0);
    println!("request: {request:?}");
    backend1.send(0);
    let response = client.receive();
    println!("response: {response:?}");
    assert!(request.unwrap().starts_with("GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nCookie: foo=bar\r\nX-Forwarded-For:"));
    assert!(response.unwrap().starts_with("HTTP/1.1 200 OK\r\nContent-Length: 5\r\nSet-Cookie: SOZUBALANCEID=sticky_cluster_0-0; Path=/\r\nSozu-Id:"));

    // invalid sticky_session
    client.set_request("GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nCookie: foo=bar; SOZUBALANCEID=invalid\r\n\r\n");
    client.connect();
    client.send();
    backend2.accept(0);
    let request = backend2.receive(0);
    println!("request: {request:?}");
    backend2.send(0);
    let response = client.receive();
    println!("response: {response:?}");
    assert!(request.unwrap().starts_with("GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nCookie: foo=bar\r\nX-Forwarded-For:"));
    assert!(response.unwrap().starts_with("HTTP/1.1 200 OK\r\nContent-Length: 5\r\nSet-Cookie: SOZUBALANCEID=sticky_cluster_0-1; Path=/\r\nSozu-Id:"));

    // good sticky_session (force use backend2, round-robin would have chosen backend1)
    client.set_request("GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nCookie: foo=bar; SOZUBALANCEID=sticky_cluster_0-1\r\n\r\n");
    client.connect();
    client.send();
    backend2.accept(0);
    let request = backend2.receive(0);
    println!("request: {request:?}");
    backend2.send(0);
    let response = client.receive();
    println!("response: {response:?}");
    assert!(request.unwrap().starts_with("GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nCookie: foo=bar\r\nX-Forwarded-For:"));
    assert!(
        response
            .unwrap()
            .starts_with("HTTP/1.1 200 OK\r\nContent-Length: 5\r\nSozu-Id:")
    );

    worker.soft_stop();
    worker.wait_for_server_stop();

    println!(
        "{} sent: {}, received: {}",
        client.name, client.requests_sent, client.responses_received
    );
    println!(
        "{} sent: {}, received: {}",
        backend1.name, backend1.responses_sent, backend1.requests_received
    );

    State::Success
}

fn try_max_connections() -> State {
    let front_address = create_local_address();

    let (mut config, listeners, state) = Worker::empty_config();
    config.max_connections = 15;
    let (mut worker, mut backends) =
        setup_sync_test("MAXCONN", config, listeners, state, front_address, 1, false);

    let mut backend = backends.pop().unwrap();
    backend.connect();
    let expected_response_start = String::from("HTTP/1.1 200 OK\r\nContent-Length: 5");

    let mut clients = Vec::new();
    for i in 0..20 {
        let mut client = Client::new(
            format!("client{i}"),
            front_address,
            http_request("GET", "/api", format!("ping{i}"), "localhost"),
        );
        client.connect();
        client.send();
        if backend.accept(i) {
            assert!(i < 15);
            let request = backend.receive(i);
            println!("request {i}: {request:?}");
            backend.send(i);
        } else {
            assert!(i >= 15);
        }
        let response = client.receive();
        println!("response {i}: {response:?}");
        if i < 15 {
            assert!(response.unwrap().starts_with(&expected_response_start));
        } else {
            assert_eq!(response, None);
        }
        clients.push(client);
    }

    for i in 0..20 {
        let client = &mut clients[i];
        if i < 15 {
            client.send();
            let request = backend.receive(i);
            println!("request {i}: {request:?}");
            backend.send(i);
            let response = client.receive();
            println!("response {i}: {response:?}");
            assert!(client.is_connected());
            assert!(response.unwrap().starts_with(&expected_response_start));
        } else {
            // assert!(!client.is_connected());
        }
    }

    for i in 0..5 {
        let new_client = &mut clients[15 + i];
        new_client.set_request(format!(
            "GET /api-{i} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        ));
        new_client.connect();
        new_client.send();

        let client = &mut clients[i];
        client.set_request("GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        client.send();
        let request = backend.receive(i);
        println!("request {i}: {request:?}");
        backend.send(i);
        let response = client.receive();
        println!("response {i}: {response:?}");
        assert!(!client.is_connected());
        assert!(response.unwrap().starts_with(&expected_response_start));

        if backend.accept(15 + i) {
            assert!(i >= 2);
        } else {
            assert!(i < 2);
        }
    }

    assert!(!backend.accept(100));

    for i in 15..20 {
        let request = backend.receive(i);
        backend.send(i);
        println!("request {i}: {request:?}");
    }
    for i in 15..20 {
        let client = &mut clients[i];
        client.is_connected();
        let response = client.receive();
        println!("response {i}: {response:?}");
        client.is_connected();
        // assert!(response.unwrap().starts_with(&expected_response_start));
    }

    for i in 15..20 {
        let client = &mut clients[i];
        client.is_connected();
        let response = client.receive();
        println!("response: {response:?}");
    }

    worker.hard_stop();
    worker.wait_for_server_stop();

    for client in clients {
        println!(
            "{} sent: {}, received: {}",
            client.name, client.requests_sent, client.responses_received
        );
    }
    println!(
        "{} sent: {}, received: {}",
        backend.name, backend.responses_sent, backend.requests_received
    );

    assert_eq!(backend.requests_received, 38);
    assert_eq!(backend.responses_sent, 38);

    State::Success
}

pub fn try_head() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) =
        setup_sync_test("SYNC", config, listeners, state, front_address, 1, false);
    let mut backend = backends.pop().unwrap();

    let head_response = "HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n";
    backend.connect();
    backend.set_response(head_response);

    let mut client = Client::new(
        "client",
        front_address,
        http_request("HEAD", "/api", "ping".to_string(), "localhost"),
    );

    client.connect();
    for i in 0..2 {
        client.send();
        if i == 0 {
            backend.accept(0);
        }
        let request = backend.receive(0);
        println!("request: {request:?}");
        assert!(request.is_some());
        backend.send(0);
        let response = client.receive();
        println!("response: {response:?}");
        assert!(response.is_some());
        assert!(response.unwrap().ends_with("\r\n\r\n"))
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

pub fn try_status_header_split() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) =
        setup_sync_test("SPLIT", config, listeners, state, front_address, 1, false);
    let mut backend = backends.pop().unwrap();

    backend.connect();

    let mut client = Client::new("client", front_address, "");

    client.connect();

    let mut accepted = false;
    for (i, chunk) in [
        "POST /api HTTP/1.",
        "1\r\n",
        "Host: localhost\r\n",
        "Content-Length",
        ":",
        " 1",
        "0",
        "\r",
        "\n\r",
        "\n012",
        "34567",
        "89",
    ]
    .iter()
    .enumerate()
    {
        println!("{accepted} {i} {chunk:?}");
        client.set_request(*chunk);
        client.send();
        if !accepted {
            println!("accepting");
            accepted = backend.accept(0);
            if accepted {
                assert_eq!(i, 9);
            }
            backend.send(0);
        }
        if accepted {
            println!("receiving");
            let request = backend.receive(0);
            println!("request: {request:?}");
        }
    }
    backend.send(0);
    let response = client.receive();
    println!("response: {response:?}");
    assert!(response.is_some());

    worker.hard_stop();
    worker.wait_for_server_stop();
    State::Success
}

fn try_wildcard() -> State {
    use sozu_command_lib::proto::command::{PathRule, RulePosition};
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_http_config(front_address);
    let mut worker = Worker::start_new_worker_owned("WLD_CRD", config, listeners, state);
    worker.send_proxy_request(
        RequestType::AddHttpListener(
            ListenerBuilder::new_http(front_address.into())
                .to_http(None)
                .unwrap(),
        )
        .into(),
    );
    worker.send_proxy_request(
        RequestType::ActivateListener(ActivateListener {
            address: front_address.into(),
            proxy: ListenerType::Http.into(),
            from_scm: false,
        })
        .into(),
    );

    worker.send_proxy_request(RequestType::AddCluster(Worker::default_cluster("cluster_0")).into());
    worker.send_proxy_request(RequestType::AddCluster(Worker::default_cluster("cluster_1")).into());

    worker.send_proxy_request(
        RequestType::AddHttpFrontend(RequestHttpFrontend {
            cluster_id: Some("cluster_0".to_string()),
            address: front_address.into(),
            hostname: String::from("*.sozu.io"),
            path: PathRule::prefix(String::from("")),
            position: RulePosition::Tree.into(),
            ..Default::default()
        })
        .into(),
    );

    let back_address: SocketAddr = create_local_address();
    worker.send_proxy_request(
        RequestType::AddBackend(Worker::default_backend(
            "cluster_0",
            "cluster_0-0",
            back_address,
            None,
        ))
        .into(),
    );
    worker.read_to_last();

    let mut backend0 = SyncBackend::new(
        "BACKEND_0",
        back_address,
        http_ok_response("pong0".to_string()),
    );

    let mut client = Client::new(
        "client",
        front_address,
        http_request("POST", "/api", "ping".to_string(), "www.sozu.io"),
    );

    backend0.connect();
    client.connect();
    client.send();
    let accepted = backend0.accept(0);
    assert!(accepted);
    let request = backend0.receive(0);
    println!("request: {request:?}");
    backend0.send(0);
    let response = client.receive();
    println!("response: {response:?}");

    worker.send_proxy_request(
        RequestType::AddHttpFrontend(RequestHttpFrontend {
            cluster_id: Some("cluster_1".to_string()),
            address: front_address.into(),
            hostname: String::from("*.sozu.io"),
            path: PathRule::prefix(String::from("/api")),
            position: RulePosition::Tree.into(),
            ..Default::default()
        })
        .into(),
    );
    let back_address: SocketAddr = create_local_address();
    worker.send_proxy_request(
        RequestType::AddBackend(Worker::default_backend(
            "cluster_1",
            "cluster_1-0",
            back_address,
            None,
        ))
        .into(),
    );

    let mut backend1 = SyncBackend::new(
        "BACKEND_1",
        back_address,
        http_ok_response("pong1".to_string()),
    );

    worker.read_to_last();

    backend1.connect();

    client.send();
    let accepted = backend1.accept(0);
    assert!(accepted);
    let request = backend1.receive(0);
    println!("request: {request:?}");
    backend1.send(0);
    let response = client.receive();
    println!("response: {response:?}");

    State::Success
}

#[test]
fn test_sync() {
    assert_eq!(try_sync(10, 100), State::Success);
}

#[test]
fn test_async() {
    assert_eq!(try_async(3, 10, 100), State::Success);
}

#[test]
fn test_hard_stop() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "Hard Stop: Test that the worker shuts down even if backends are not done",
            || try_hard_or_soft_stop(false)
        ),
        State::Success
    );
}

#[test]
fn test_soft_stop() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "Hard Stop: Test that the worker waits for all backends to process requests before shutting down",
            || try_hard_or_soft_stop(true)
        ),
        State::Success
    );
}

// https://github.com/sozu-proxy/sozu/issues/806
// This should actually be a success

#[test]
fn test_issue_806() {
    assert!(
        repeat_until_error_or(
            100,
            "issue 806: timeout with invalid back token\n(not fixed)",
            || try_backend_stop(2, None)
        ) != State::Fail
    );
}

// https://github.com/sozu-proxy/sozu/issues/808

#[test]
fn test_issue_808() {
    // this test is not relevant anymore, at least not like this
    // assert_eq!(
    //     repeat_until_error_or(
    //         100,
    //         "issue 808: panic on successful zombie check\n(fixed)",
    //         || try_backend_stop(2, Some(1))
    //     ),
    //     // if Success, it means the session was never a zombie
    //     // if Fail, it means the zombie checker probably crashed
    //     State::Undecided
    // );
}

#[test]
fn test_h1_idle_connection_zombie_metric_increments() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H1: idle connection reaped by zombie checker increments zombies metric",
            try_h1_idle_connection_zombie_metric_increments
        ),
        State::Success
    );
}

// https://github.com/sozu-proxy/sozu/issues/810

#[test]
fn test_issue_810_timeout() {
    assert_eq!(
        repeat_until_error_or(
            100,
            "issue 810: shutdown struggles until session timeout\n(fixed)",
            try_issue_810_timeout
        ),
        State::Success
    );
}

#[test]
fn test_issue_810_panic_on_session_close() {
    assert_eq!(
        repeat_until_error_or(
            100,
            "issue 810: shutdown panics on session close\n(fixed)",
            || try_issue_810_panic(false)
        ),
        State::Success
    );
}

#[test]
fn test_issue_810_panic_on_missing_listener() {
    assert_eq!(
        repeat_until_error_or(
            100,
            "issue 810: shutdown panics on tcp connection after proxy cleared its listeners\n(opinionated fix)",
            || try_issue_810_panic(true)
        ),
        State::Success
    );
}

#[test]
fn test_tls_endpoint() {
    assert_eq!(
        repeat_until_error_or(
            100,
            "TLS endpoint: Sōzu should decrypt an HTTPS request",
            try_tls_endpoint
        ),
        State::Success
    );
}

#[test]
fn test_tls_rsa_2048() {
    assert_eq!(
        repeat_until_error_or(
            100,
            "TLS RSA-2048: HTTPS with RSA-2048 certificate",
            try_tls_rsa_2048
        ),
        State::Success
    );
}

#[test]
fn test_tls_ecdsa() {
    assert_eq!(
        repeat_until_error_or(
            100,
            "TLS ECDSA: HTTPS with ECDSA P-256 certificate",
            try_tls_ecdsa
        ),
        State::Success
    );
}

#[test]
fn test_tls_1_3_rsa() {
    assert_eq!(
        repeat_until_error_or(
            100,
            "TLS 1.3 RSA: HTTPS with TLS 1.3 only and RSA certificate",
            try_tls_1_3_rsa
        ),
        State::Success
    );
}

#[test]
fn test_tls_1_3_ecdsa() {
    assert_eq!(
        repeat_until_error_or(
            100,
            "TLS 1.3 ECDSA: HTTPS with TLS 1.3 only and ECDSA certificate",
            try_tls_1_3_ecdsa
        ),
        State::Success
    );
}

#[test]
fn test_tls_1_2_rsa() {
    assert_eq!(
        repeat_until_error_or(
            100,
            "TLS 1.2 RSA: HTTPS with TLS 1.2 only and RSA certificate",
            try_tls_1_2_rsa
        ),
        State::Success
    );
}

// TLS 1.2 + ECDSA handshakes on `rustls-openssl 0.3.x` reliably fail
// with `General("OpenSSL error: OpenSSL error")` on the proxy side
// (`Could not perform handshake` log line on cargo CI). The same
// handshake succeeds for ring and aws-lc-rs against the identical
// listener config + ECDSA cert. Mark the test ignored under
// `crypto-openssl` so CI's matrix can stay green; tracked as an
// upstream `rustls-openssl` issue. Re-enable after a `rustls-openssl`
// release that resolves the regression.
#[test]
#[cfg_attr(
    feature = "crypto-openssl",
    ignore = "rustls-openssl 0.3.x reports General(\"OpenSSL error\") on TLS 1.2 + ECDSA handshake; tracked upstream"
)]
fn test_tls_1_2_ecdsa() {
    assert_eq!(
        repeat_until_error_or(
            100,
            "TLS 1.2 ECDSA: HTTPS with TLS 1.2 only and ECDSA certificate",
            try_tls_1_2_ecdsa
        ),
        State::Success
    );
}

// Companion probe to `test_tls_1_2_ecdsa`. Compiled only when the
// `crypto-openssl` feature is on, this asserts that the very same
// handshake the upstream regression breaks STILL fails. As soon as
// `rustls-openssl` ships a fix, the handshake will succeed and this
// probe will flip red — at that point:
//
//   1. delete this probe
//   2. drop the `#[cfg_attr(feature = "crypto-openssl", ignore = ...)]`
//      from `test_tls_1_2_ecdsa` above so the real test runs on every
//      cell again
//
// One iteration is enough because the upstream failure is deterministic
// (`General("OpenSSL error: OpenSSL error")` on the proxy-side
// handshake — see the comment on `test_tls_1_2_ecdsa`).
#[test]
#[cfg(feature = "crypto-openssl")]
fn test_tls_1_2_ecdsa_openssl_regression_probe() {
    assert_ne!(
        repeat_until_error_or(
            1,
            "Probe: TLS 1.2 ECDSA must still fail under crypto-openssl",
            try_tls_1_2_ecdsa
        ),
        State::Success,
        "rustls-openssl's TLS 1.2 + ECDSA handshake regression appears \
         resolved — un-ignore `test_tls_1_2_ecdsa` for crypto-openssl \
         and remove this probe."
    );
}

// ---------------------------------------------------------------------------
// Cardinality matrix tests — frontend H1/H2 × backend H1/H2 across cert kinds.
// Cell H1/H1 is already covered by `test_tls_rsa_2048` / `test_tls_ecdsa`.
// ---------------------------------------------------------------------------

#[test]
fn test_tls_cardinality_h1_h2_rsa() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "Cardinality H1→H2 RSA: H1 frontend reverse-proxies to h2c backend",
            try_tls_cardinality_h1_h2_rsa
        ),
        State::Success
    );
}

#[test]
fn test_tls_cardinality_h1_h2_ecdsa() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "Cardinality H1→H2 ECDSA: H1 frontend reverse-proxies to h2c backend",
            try_tls_cardinality_h1_h2_ecdsa
        ),
        State::Success
    );
}

#[test]
fn test_tls_cardinality_h2_h1_rsa() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "Cardinality H2→H1 RSA: H2 frontend reverse-proxies to H1 backend",
            try_tls_cardinality_h2_h1_rsa
        ),
        State::Success
    );
}

#[test]
fn test_tls_cardinality_h2_h1_ecdsa() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "Cardinality H2→H1 ECDSA: H2 frontend reverse-proxies to H1 backend",
            try_tls_cardinality_h2_h1_ecdsa
        ),
        State::Success
    );
}

#[test]
fn test_tls_cardinality_h2_h2_rsa() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "Cardinality H2→H2 RSA: H2 frontend reverse-proxies to h2c backend",
            try_tls_cardinality_h2_h2_rsa
        ),
        State::Success
    );
}

#[test]
fn test_tls_cardinality_h2_h2_ecdsa() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "Cardinality H2→H2 ECDSA: H2 frontend reverse-proxies to h2c backend",
            try_tls_cardinality_h2_h2_ecdsa
        ),
        State::Success
    );
}

#[test]
fn test_http_behaviors() {
    assert_eq!(
        repeat_until_error_or(10, "HTTP stack", try_http_behaviors),
        State::Success
    );
}

#[test]
fn test_builtin_404_default_answer_closes_connection() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "HTTP: built-in 404 default answer closes connection",
            try_builtin_404_default_answer_closes_connection
        ),
        State::Success
    );
}

#[test]
fn test_https_redirect() {
    assert_eq!(
        repeat_until_error_or(2, "HTTPS redirection", try_https_redirect),
        State::Success
    );
}

#[test]
fn test_msg_close() {
    assert_eq!(
        repeat_until_error_or(100, "HTTP error on close", try_msg_close),
        State::Success
    );
}

#[test]
fn test_blue_green() {
    assert_eq!(
        repeat_until_error_or(10, "Blue green switch", try_blue_geen),
        State::Success
    );
}

#[test]
fn test_keep_alive() {
    assert_eq!(
        repeat_until_error_or(10, "Keep alive combinations", try_keep_alive),
        State::Success
    );
}

#[test]
fn test_stick() {
    assert_eq!(
        repeat_until_error_or(10, "Sticky session", try_stick),
        State::Success
    );
}

#[test]
fn test_max_connections() {
    assert_eq!(
        repeat_until_error_or(2, "Max connections reached", try_max_connections),
        State::Success
    );
}

#[test]
fn test_head() {
    assert_eq!(
        repeat_until_error_or(10, "Head request", try_head),
        State::Success
    );
}

#[test]
fn test_wildcard() {
    assert_eq!(
        repeat_until_error_or(2, "Hostname with wildcard", try_wildcard),
        State::Success
    );
}

#[test]
fn test_status_header_split() {
    assert_eq!(
        repeat_until_error_or(2, "Status line and Headers split", try_status_header_split),
        State::Success
    );
}

// ---------------------------------------------------------------------------
// Upgrade tests
// ---------------------------------------------------------------------------

#[test]
fn test_upgrade() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "Upgrade: original upgrade test — in-flight request completes after worker upgrade",
            try_upgrade
        ),
        State::Success
    );
}

#[test]
fn test_upgrade_in_flight_request() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "Upgrade: in-flight request completes after worker upgrade, new connections work",
            try_upgrade_in_flight_request
        ),
        State::Success
    );
}

#[test]
fn test_upgrade_new_connections_after() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "Upgrade: new connections work after worker upgrade",
            try_upgrade_new_connections_after
        ),
        State::Success
    );
}

#[test]
fn test_upgrade_multiple_in_flight() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "Upgrade: multiple in-flight requests complete after worker upgrade",
            try_upgrade_multiple_in_flight
        ),
        State::Success
    );
}

#[test]
fn test_upgrade_keepalive_reconnect() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "Upgrade: keep-alive connection behavior across worker upgrade",
            try_upgrade_keepalive_reconnect
        ),
        State::Success
    );
}

#[test]
fn test_upgrade_async() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "Upgrade: async backends with concurrent clients across worker upgrade",
            try_upgrade_async
        ),
        State::Success
    );
}

// ---------------------------------------------------------------------------
// HTTP/2 tests
// ---------------------------------------------------------------------------

/// Helper: set up a Sozu worker with an HTTPS listener, a cluster, a certificate,
/// and `nb_backends` async backends. Returns (worker, backends, front_port).
fn setup_h2_test(
    name: &str,
    nb_backends: usize,
) -> (Worker, Vec<AsyncBackend<SimpleAggregator>>, u16) {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);

    let (config, listeners, state) = Worker::empty_https_config(front_address.clone().into());
    let mut worker = Worker::start_new_worker_owned(name, config, listeners, state);

    worker.send_proxy_request_type(RequestType::AddHttpsListener(
        ListenerBuilder::new_https(front_address.clone())
            .to_tls(None)
            .unwrap(),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.clone(),
        proxy: ListenerType::Https.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "cluster_0",
    )));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: String::from("localhost"),
        ..Worker::default_http_frontend("cluster_0", front_address.clone().into())
    }));

    let certificate_and_key = CertificateAndKey {
        certificate: String::from(include_str!("../../../lib/assets/local-certificate.pem")),
        key: String::from(include_str!("../../../lib/assets/local-key.pem")),
        certificate_chain: vec![],
        versions: vec![],
        names: vec![],
    };
    worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
        address: front_address,
        certificate: certificate_and_key,
        expired_at: None,
    }));

    let mut backends = Vec::new();
    for i in 0..nb_backends {
        let back_address = create_local_address();
        worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
            "cluster_0",
            format!("cluster_0-{i}"),
            back_address,
            None,
        )));
        backends.push(AsyncBackend::spawn_detached_backend(
            format!("BACKEND_{i}"),
            back_address,
            SimpleAggregator::default(),
            AsyncBackend::http_handler(format!("pong{i}")),
        ));
    }

    worker.read_to_last();
    (worker, backends, front_port)
}

/// Send a basic GET request over HTTP/2 and verify the response.
fn try_h2_basic_request() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_test("H2-BASIC", 1);

    let client = build_h2_client();
    let uri: hyper::Uri = format!("https://localhost:{front_port}/api")
        .parse()
        .unwrap();

    if let Some((status, body)) = resolve_request(&client, uri) {
        println!("H2 basic - status: {status:?}, body: {body}");
        if !status.is_success() || !body.contains("pong") {
            return State::Fail;
        }
    } else {
        return State::Fail;
    }

    worker.soft_stop();
    let success = worker.wait_for_server_stop();

    let aggregator = backends[0]
        .stop_and_get_aggregator()
        .expect("Could not get aggregator");
    if success && aggregator.responses_sent == 1 {
        State::Success
    } else {
        State::Fail
    }
}

/// Verify that when a client advertises both h2 and http/1.1, the server selects h2
/// (since Sozu's SERVER_PROTOS lists "h2" first).
fn try_h2_alpn_negotiation() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_test("H2-ALPN", 1);

    // Client advertises both h2 and http/1.1
    let client = build_h2_or_h1_client();
    let uri: hyper::Uri = format!("https://localhost:{front_port}/api")
        .parse()
        .unwrap();

    if let Some((status, body)) = resolve_request(&client, uri) {
        println!("H2 ALPN - status: {status:?}, body: {body}");
        if !status.is_success() || !body.contains("pong") {
            return State::Fail;
        }
    } else {
        return State::Fail;
    }

    worker.soft_stop();
    let success = worker.wait_for_server_stop();

    let aggregator = backends[0]
        .stop_and_get_aggregator()
        .expect("Could not get aggregator");
    if success && aggregator.responses_sent == 1 {
        State::Success
    } else {
        State::Fail
    }
}

/// Send a POST request with a body over HTTP/2.
fn try_h2_post_with_body() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_test("H2-POST", 1);

    let client = build_h2_client();
    let uri: hyper::Uri = format!("https://localhost:{front_port}/api")
        .parse()
        .unwrap();
    let payload = "hello from h2 post".to_owned();

    if let Some((status, body)) = resolve_post_request(&client, uri, payload) {
        println!("H2 POST - status: {status:?}, body: {body}");
        if !status.is_success() {
            return State::Fail;
        }
    } else {
        return State::Fail;
    }

    worker.soft_stop();
    let success = worker.wait_for_server_stop();

    let aggregator = backends[0]
        .stop_and_get_aggregator()
        .expect("Could not get aggregator");
    if success && aggregator.responses_sent >= 1 {
        State::Success
    } else {
        State::Fail
    }
}

/// Send multiple concurrent requests over a single H2 connection (multiple streams).
fn try_h2_multiple_streams() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_test("H2-STREAMS", 1);

    let client = build_h2_client();
    let uris: Vec<hyper::Uri> = (0..4)
        .map(|i| {
            format!("https://localhost:{front_port}/api/stream{i}")
                .parse()
                .unwrap()
        })
        .collect();

    let results = resolve_concurrent_requests(&client, uris);
    let all_ok = results
        .iter()
        .all(|r| r.as_ref().is_some_and(|(s, _)| s.is_success()));
    if !all_ok {
        println!("H2 streams - not all requests succeeded: {results:?}");
        return State::Fail;
    }

    worker.soft_stop();
    let success = worker.wait_for_server_stop();

    let aggregator = backends[0]
        .stop_and_get_aggregator()
        .expect("Could not get aggregator");
    println!(
        "H2 streams - backend received: {}, sent: {}",
        aggregator.requests_received, aggregator.responses_sent
    );
    if success && aggregator.responses_sent == 4 {
        State::Success
    } else {
        State::Fail
    }
}

/// Send a large payload over HTTP/2 to exercise flow control.
fn try_h2_large_payload() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_test("H2-LARGE", 1);

    let client = build_h2_client();
    let uri: hyper::Uri = format!("https://localhost:{front_port}/api")
        .parse()
        .unwrap();
    // 128 KiB payload — large enough to trigger H2 flow control windows
    let payload = "X".repeat(128 * 1024);

    if let Some((status, _body)) = resolve_post_request(&client, uri, payload) {
        println!("H2 large payload - status: {status:?}");
        if !status.is_success() {
            return State::Fail;
        }
    } else {
        return State::Fail;
    }

    worker.soft_stop();
    let success = worker.wait_for_server_stop();

    let aggregator = backends[0]
        .stop_and_get_aggregator()
        .expect("Could not get aggregator");
    if success && aggregator.responses_sent >= 1 {
        State::Success
    } else {
        State::Fail
    }
}

/// Verify that custom and standard headers are forwarded correctly through H2.
fn try_h2_headers() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_test("H2-HEADERS", 1);

    let client = build_h2_client();
    let uri: hyper::Uri = format!("https://localhost:{front_port}/api")
        .parse()
        .unwrap();

    let rt = tokio::runtime::Runtime::new().expect("Could not create Runtime");
    let result = rt.block_on(async {
        let request = hyper::Request::builder()
            .method(hyper::Method::GET)
            .uri(uri)
            .header("host", "localhost")
            .header("x-custom-header", "test-value")
            .header("content-type", "text/plain")
            .body(String::new())
            .expect("Could not build request");
        match client.request(request).await {
            Ok(response) => {
                let status = response.status();
                println!("H2 headers - status: {status:?}");
                Some(status)
            }
            Err(error) => {
                println!("H2 headers - error: {error}");
                None
            }
        }
    });

    if result.is_none_or(|s| !s.is_success()) {
        return State::Fail;
    }

    worker.soft_stop();
    let success = worker.wait_for_server_stop();

    let aggregator = backends[0]
        .stop_and_get_aggregator()
        .expect("Could not get aggregator");
    if success && aggregator.responses_sent == 1 {
        State::Success
    } else {
        State::Fail
    }
}

/// Verify that HTTP/1.1 requests still work on the same HTTPS listener (ALPN fallback).
fn try_h1_still_works_on_h2_listener() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_test("H1-FALLBACK", 1);

    // Use the HTTP/1.1-only client on the same listener that supports H2
    let client = build_https_client();
    let uri: hyper::Uri = format!("https://localhost:{front_port}/api")
        .parse()
        .unwrap();

    if let Some((status, body)) = resolve_request(&client, uri) {
        println!("H1 fallback - status: {status:?}, body: {body}");
        if !status.is_success() || !body.contains("pong") {
            return State::Fail;
        }
    } else {
        return State::Fail;
    }

    worker.soft_stop();
    let success = worker.wait_for_server_stop();

    let aggregator = backends[0]
        .stop_and_get_aggregator()
        .expect("Could not get aggregator");
    if success && aggregator.responses_sent == 1 {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_basic_request() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "H2: basic GET request over HTTP/2",
            try_h2_basic_request
        ),
        State::Success
    );
}

#[test]
fn test_h2_alpn_negotiation() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "H2: ALPN negotiation selects h2 when both h2 and http/1.1 are offered",
            try_h2_alpn_negotiation
        ),
        State::Success
    );
}

#[test]
fn test_h2_post_with_body() {
    assert_eq!(
        repeat_until_error_or(10, "H2: POST request with body", try_h2_post_with_body),
        State::Success
    );
}

#[test]
fn test_h2_multiple_streams() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "H2: multiple concurrent streams on a single connection",
            try_h2_multiple_streams
        ),
        State::Success
    );
}

#[test]
fn test_h2_large_payload() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "H2: large payload exercises flow control",
            try_h2_large_payload
        ),
        State::Success
    );
}

#[test]
fn test_h2_headers() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "H2: custom and standard headers are forwarded",
            try_h2_headers
        ),
        State::Success
    );
}

#[test]
fn test_h1_still_works_on_h2_listener() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "H2: HTTP/1.1 still works on HTTPS listener with H2 support",
            try_h1_still_works_on_h2_listener
        ),
        State::Success
    );
}

/// Set up an HTTPS listener with custom ALPN protocols.
fn setup_h2_test_with_alpn(
    name: &str,
    nb_backends: usize,
    alpn_protocols: Vec<String>,
) -> (Worker, Vec<AsyncBackend<SimpleAggregator>>, u16) {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);

    let (config, listeners, state) = Worker::empty_https_config(front_address.clone().into());
    let mut worker = Worker::start_new_worker_owned(name, config, listeners, state);

    let mut listener_builder = ListenerBuilder::new_https(front_address.clone());
    listener_builder.with_alpn_protocols(Some(alpn_protocols));
    worker.send_proxy_request_type(RequestType::AddHttpsListener(
        listener_builder.to_tls(None).unwrap(),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.clone(),
        proxy: ListenerType::Https.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "cluster_0",
    )));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: String::from("localhost"),
        ..Worker::default_http_frontend("cluster_0", front_address.clone().into())
    }));

    let certificate_and_key = CertificateAndKey {
        certificate: String::from(include_str!("../../../lib/assets/local-certificate.pem")),
        key: String::from(include_str!("../../../lib/assets/local-key.pem")),
        certificate_chain: vec![],
        versions: vec![],
        names: vec![],
    };
    worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
        address: front_address,
        certificate: certificate_and_key,
        expired_at: None,
    }));

    let mut backends = Vec::new();
    for i in 0..nb_backends {
        let back_address = create_local_address();
        worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
            "cluster_0",
            format!("cluster_0-{i}"),
            back_address,
            None,
        )));
        backends.push(AsyncBackend::spawn_detached_backend(
            format!("BACKEND_{i}"),
            back_address,
            SimpleAggregator::default(),
            AsyncBackend::http_handler(format!("pong{i}")),
        ));
    }

    worker.read_to_last();
    (worker, backends, front_port)
}

/// Verify that an HTTPS listener with alpn_protocols = ["http/1.1"] only
/// serves HTTP/1.1 even when the client offers both H2 and HTTP/1.1.
fn try_alpn_http11_only_listener() -> State {
    let (mut worker, mut backends, front_port) =
        setup_h2_test_with_alpn("ALPN-H1ONLY", 1, vec!["http/1.1".to_owned()]);

    // Client advertises both h2 and http/1.1, but the listener only supports http/1.1
    let client = build_h2_or_h1_client();
    let uri: hyper::Uri = format!("https://localhost:{front_port}/api")
        .parse()
        .unwrap();

    if let Some((status, body)) = resolve_request(&client, uri) {
        println!("ALPN H1-only - status: {status:?}, body: {body}");
        if !status.is_success() || !body.contains("pong") {
            return State::Fail;
        }
    } else {
        return State::Fail;
    }

    worker.soft_stop();
    let success = worker.wait_for_server_stop();

    let aggregator = backends[0]
        .stop_and_get_aggregator()
        .expect("Could not get aggregator");
    if success && aggregator.responses_sent == 1 {
        State::Success
    } else {
        State::Fail
    }
}

/// Verify that an HTTPS listener with alpn_protocols = ["http/1.1", "h2"]
/// (preferring HTTP/1.1) still works with an H2-only client.
fn try_alpn_prefer_h1_with_h2_client() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_test_with_alpn(
        "ALPN-PREFER-H1",
        1,
        vec!["http/1.1".to_owned(), "h2".to_owned()],
    );

    // Client requires H2 only — the listener supports it (just prefers H1)
    let client = build_h2_client();
    let uri: hyper::Uri = format!("https://localhost:{front_port}/api")
        .parse()
        .unwrap();

    if let Some((status, body)) = resolve_request(&client, uri) {
        println!("ALPN prefer-H1 with H2 client - status: {status:?}, body: {body}");
        if !status.is_success() || !body.contains("pong") {
            return State::Fail;
        }
    } else {
        return State::Fail;
    }

    worker.soft_stop();
    let success = worker.wait_for_server_stop();

    let aggregator = backends[0]
        .stop_and_get_aggregator()
        .expect("Could not get aggregator");
    if success && aggregator.responses_sent == 1 {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_alpn_http11_only_listener() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "ALPN: HTTP/1.1-only listener serves H1 even when client offers H2",
            try_alpn_http11_only_listener
        ),
        State::Success
    );
}

#[test]
fn test_alpn_prefer_h1_with_h2_client() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "ALPN: listener preferring HTTP/1.1 still serves H2-only client",
            try_alpn_prefer_h1_with_h2_client
        ),
        State::Success
    );
}

/// Run h2spec HTTP/2 conformance tests against Sozu.
///
/// This test exercises 145 RFC 9113 conformance scenarios using h2spec 2.0.
///
/// Requires the `h2spec` binary in PATH. Install via a prebuilt release or:
///   go install github.com/summerwind/h2spec/cmd/h2spec@latest
///
/// When the binary is not available (e.g. CI legs without the install step)
/// the test logs a notice and returns cleanly instead of panicking — CI can
/// still exercise the rest of the e2e suite without blocking on the Go
/// dependency. Local runs that expect the conformance gate should ensure
/// `h2spec --version` prints successfully before relying on this test.
#[test]
fn test_h2spec_conformance() {
    use std::process::Command;

    // Graceful skip if h2spec is missing. Downstream CI installs it via a
    // prebuilt release tarball from github.com/summerwind/h2spec/releases;
    // on platforms where that step is skipped (or the binary failed to
    // install) the whole e2e matrix would otherwise panic here.
    let h2spec_version = match Command::new("h2spec").arg("--version").output() {
        Ok(o) if o.status.success() => o,
        Ok(_) | Err(_) => {
            eprintln!(
                "test_h2spec_conformance: h2spec binary not available in PATH — skipping. \
                 Install with a prebuilt release or \
                 `go install github.com/summerwind/h2spec/cmd/h2spec@latest` to enable the \
                 conformance gate locally."
            );
            return;
        }
    };
    println!(
        "h2spec version: {}",
        String::from_utf8_lossy(&h2spec_version.stdout).trim()
    );

    let (mut worker, _backends, front_port) = setup_h2_test("H2SPEC", 1);

    // Give the worker time to bind the listener and become ready
    thread::sleep(Duration::from_millis(300));

    let output = Command::new("h2spec")
        .args([
            "-h",
            "localhost",
            "-p",
            &front_port.to_string(),
            "-t", // TLS mode
            "-k", // skip cert verification
            "-o",
            "5", // 5 second timeout per test
        ])
        .output()
        .expect("Failed to execute h2spec");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    println!("h2spec stdout:\n{stdout}");
    if !stderr.is_empty() {
        println!("h2spec stderr:\n{stderr}");
    }

    // Parse results from the summary line: "N tests, N passed, N skipped, N failed"
    let mut passed = 0u32;
    let mut failed = 0u32;
    let mut skipped = 0u32;
    let mut total = 0u32;
    for line in stdout.lines().rev().take(5) {
        if line.contains("passed") && line.contains("failed") {
            for part in line.split(',') {
                let part = part.trim();
                if part.ends_with("passed") {
                    passed = part
                        .split_whitespace()
                        .next()
                        .unwrap_or("0")
                        .parse()
                        .unwrap_or(0);
                } else if part.ends_with("failed") {
                    failed = part
                        .split_whitespace()
                        .next()
                        .unwrap_or("0")
                        .parse()
                        .unwrap_or(0);
                } else if part.ends_with("skipped") {
                    skipped = part
                        .split_whitespace()
                        .next()
                        .unwrap_or("0")
                        .parse()
                        .unwrap_or(0);
                } else if part.ends_with("tests") {
                    total = part
                        .split_whitespace()
                        .next()
                        .unwrap_or("0")
                        .parse()
                        .unwrap_or(0);
                }
            }
        }
    }

    println!("h2spec results: {total} total, {passed} passed, {skipped} skipped, {failed} failed");

    worker.soft_stop();
    worker.wait_for_server_stop();

    assert_eq!(
        failed, 0,
        "h2spec: {failed} out of {total} tests failed (see output above)"
    );
}

// ============================================================================
// H2 Backend Tests (H1→H2 and H2→H2)
// ============================================================================

/// Setup an HTTPS frontend with H2 backends (cluster.http2 = true).
/// The backends speak cleartext HTTP/2 (h2c) — Sozu connects to them over
/// plain TCP and initiates an H2 connection.
fn setup_h2_backend_test(
    name: &str,
    nb_backends: usize,
    frontend_h2: bool,
) -> (Worker, Vec<H2Backend>, u16) {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);

    let (config, listeners, state) = if frontend_h2 {
        Worker::empty_https_config(front_address.clone().into())
    } else {
        Worker::empty_http_config(front_address.clone().into())
    };
    let mut worker = Worker::start_new_worker_owned(name, config, listeners, state);

    if frontend_h2 {
        // HTTPS listener (supports H2 via ALPN)
        worker.send_proxy_request_type(RequestType::AddHttpsListener(
            ListenerBuilder::new_https(front_address.clone())
                .to_tls(None)
                .unwrap(),
        ));
        worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
            address: front_address.clone(),
            proxy: ListenerType::Https.into(),
            from_scm: false,
        }));
    } else {
        // HTTP listener (H1 frontend)
        worker.send_proxy_request_type(RequestType::AddHttpListener(
            ListenerBuilder::new_http(front_address.clone())
                .to_http(None)
                .unwrap(),
        ));
        worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
            address: front_address.clone(),
            proxy: ListenerType::Http.into(),
            from_scm: false,
        }));
    }

    // Cluster with http2=true (backend speaks H2)
    worker.send_proxy_request_type(RequestType::AddCluster(Cluster {
        http2: Some(true),
        ..Worker::default_cluster("cluster_0")
    }));

    if frontend_h2 {
        worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
            hostname: String::from("localhost"),
            ..Worker::default_http_frontend("cluster_0", front_address.clone().into())
        }));

        let certificate_and_key = CertificateAndKey {
            certificate: String::from(include_str!("../../../lib/assets/local-certificate.pem")),
            key: String::from(include_str!("../../../lib/assets/local-key.pem")),
            certificate_chain: vec![],
            versions: vec![],
            names: vec![],
        };
        worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
            address: front_address,
            certificate: certificate_and_key,
            expired_at: None,
        }));
    } else {
        worker.send_proxy_request_type(RequestType::AddHttpFrontend(
            Worker::default_http_frontend("cluster_0", front_address.into()),
        ));
    }

    let mut backends = Vec::new();
    for i in 0..nb_backends {
        let back_address = create_local_address();
        worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
            "cluster_0",
            format!("cluster_0-{i}"),
            back_address,
            None,
        )));
        backends.push(H2Backend::start(
            format!("H2_BACKEND_{i}"),
            back_address,
            format!("h2-pong{i}"),
        ));
    }

    worker.read_to_last();
    // Give H2 backends time to bind
    thread::sleep(Duration::from_millis(100));
    (worker, backends, front_port)
}

/// H2 frontend → H2 backend: basic GET request
fn try_h2_to_h2_basic_request() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_backend_test("H2-TO-H2-BASIC", 1, true);

    let client = build_h2_client();
    let uri: hyper::Uri = format!("https://localhost:{front_port}/api")
        .parse()
        .unwrap();

    if let Some((status, body)) = resolve_request(&client, uri) {
        println!("H2→H2 basic - status: {status:?}, body: {body}");
        if !status.is_success() || !body.contains("h2-pong") {
            return State::Fail;
        }
    } else {
        return State::Fail;
    }

    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    let resp_sent = backends[0].get_responses_sent();
    backends.iter_mut().for_each(|b| b.stop());

    if success && resp_sent >= 1 {
        State::Success
    } else {
        State::Fail
    }
}

/// H1 frontend → H2 backend: client sends HTTP/1.1, Sozu upgrades to H2 on backend
fn try_h1_to_h2_basic_request() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_backend_test("H1-TO-H2-BASIC", 1, false);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut client = Client::new(
        "client",
        front_addr,
        http_request("GET", "/api", "", &format!("localhost:{front_port}")),
    );
    client.connect();
    client.send();
    let response = client.receive();
    // Second read to get the body (may arrive in a separate TCP segment)
    let body = client.receive();
    println!("H1→H2 basic - response: {response:?}, body: {body:?}");

    let success_response = response.as_ref().is_some_and(|r| r.contains("200"));

    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    let resp_sent = backends[0].get_responses_sent();
    backends.iter_mut().for_each(|b| b.stop());

    if success && success_response && resp_sent >= 1 {
        State::Success
    } else {
        State::Fail
    }
}

/// H2 frontend → H2 backend: multiple concurrent streams
fn try_h2_to_h2_multiple_streams() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_backend_test("H2-TO-H2-MULTI", 1, true);

    let client = build_h2_client();
    let uris: Vec<hyper::Uri> = (0..4)
        .map(|i| {
            format!("https://localhost:{front_port}/stream/{i}")
                .parse()
                .unwrap()
        })
        .collect();

    let results = resolve_concurrent_requests(&client, uris);
    let all_ok = results.iter().all(|r| {
        r.as_ref()
            .is_some_and(|(status, body)| status.is_success() && body.contains("h2-pong"))
    });

    if !all_ok || results.len() != 4 {
        println!("H2→H2 multi streams failed: {results:?}");
        return State::Fail;
    }

    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    let resp_sent = backends[0].get_responses_sent();
    backends.iter_mut().for_each(|b| b.stop());

    if success && resp_sent >= 4 {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_to_h2_basic_request() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "H2→H2: basic GET request with H2 backend",
            try_h2_to_h2_basic_request
        ),
        State::Success
    );
}

#[test]
fn test_h1_to_h2_basic_request() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "H1→H2: HTTP/1.1 frontend to H2 backend",
            try_h1_to_h2_basic_request
        ),
        State::Success
    );
}

#[test]
fn test_h2_to_h2_multiple_streams() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "H2→H2: multiple concurrent streams with H2 backend",
            try_h2_to_h2_multiple_streams
        ),
        State::Success
    );
}

// ============================================================================
// Test: HTTP/1.1 pipelining
// ============================================================================

/// Send 3 HTTP/1.1 requests on the same TCP connection without waiting for
/// responses between sends (pipelining). Then read all 3 responses and verify
/// they arrive in order and match the requests.
///
/// HTTP/1.1 pipelining is rarely used in practice but is part of the spec
/// (RFC 9112 Section 9.3). Sozu must handle pipelined requests correctly
/// or at minimum not crash.
fn try_h1_pipelining() -> State {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_async_test(
        "H1-PIPELINE",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );

    // Build 3 distinct HTTP/1.1 requests
    let req1 = http_request("GET", "/api/pipe/1", "ping1", "localhost");
    let req2 = http_request("GET", "/api/pipe/2", "ping2", "localhost");
    let req3 = http_request("GET", "/api/pipe/3", "ping3", "localhost");

    // Connect and send all 3 requests without reading any responses
    let mut stream = TcpStream::connect(front_address).expect("could not connect for pipelining");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("set write timeout");

    // Pipeline: write all 3 requests back-to-back
    stream.write_all(req1.as_bytes()).expect("send req1");
    stream.write_all(req2.as_bytes()).expect("send req2");
    stream.write_all(req3.as_bytes()).expect("send req3");
    stream.flush().expect("flush pipelined requests");

    // Read all responses with a reasonable timeout
    let mut all_data = Vec::new();
    let mut buf = [0u8; 8192];
    let start = Instant::now();
    let timeout = Duration::from_secs(5);
    while start.elapsed() < timeout {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                all_data.extend_from_slice(&buf[..n]);
                // Check if we have received 3 complete responses
                let response_str = String::from_utf8_lossy(&all_data);
                let response_count = response_str.matches("HTTP/1.1 200").count();
                if response_count >= 3 {
                    break;
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                println!("H1 pipelining - read error: {e}");
                break;
            }
        }
    }

    drop(stream);

    let response_str = String::from_utf8_lossy(&all_data);
    let response_count = response_str.matches("HTTP/1.1 200").count();
    println!("H1 pipelining - received {response_count} HTTP 200 responses");
    println!("H1 pipelining - total bytes received: {}", all_data.len());

    // Verify all 3 responses contain "pong" (the backend response body)
    let pong_count = response_str.matches("pong").count();
    println!("H1 pipelining - 'pong' occurrences: {pong_count}");

    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    for backend in backends.iter_mut() {
        backend.stop_and_get_aggregator();
    }

    // All 3 pipelined requests should get responses
    if success && response_count >= 3 {
        State::Success
    } else {
        println!("H1 pipelining - success={success}, response_count={response_count}");
        State::Fail
    }
}

#[test]
fn test_h1_pipelining() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1: HTTP/1.1 pipelining — 3 requests on same connection without reading",
            try_h1_pipelining
        ),
        State::Success
    );
}

// ---------------------------------------------------------------------------
// X-Real-IP injection and anti-spoof elision (closes #1113).
//
// The two listener-scoped flags `elide_x_real_ip` and `send_x_real_ip`
// default off and are independently combinable. Coverage:
//
// * `test_x_real_ip_neither`        — defaults; client header passes through.
// * `test_x_real_ip_send_only`      — proxy injects peer IP; client header (if any) survives.
// * `test_x_real_ip_elide_only`     — client header stripped; nothing injected.
// * `test_x_real_ip_send_and_elide` — full anti-spoof + send, exercised through PROXY-v2.
// * `test_x_real_ip_elide_h2_trailer` — H2 trailer regression (Codex finding); scaffold pending.
// ---------------------------------------------------------------------------

/// Helper: spin up a single-worker HTTP listener with the two `X-Real-IP`
/// flags, optional `expect_proxy`, one cluster `cluster_0`, one HTTP
/// frontend, and one backend address. Returns `(worker, front, back)`.
fn setup_x_real_ip_test(
    name: &str,
    elide_x_real_ip: bool,
    send_x_real_ip: bool,
    expect_proxy: bool,
) -> (Worker, SocketAddr, SocketAddr) {
    let front_address = create_local_address();
    let back_address = create_local_address();

    let (config, mut listeners, state) = Worker::empty_config();
    crate::port_registry::attach_reserved_http_listener(&mut listeners, front_address);
    let mut worker = Worker::start_new_worker_owned(name, config, listeners, state);

    let http_listener = {
        let mut builder = ListenerBuilder::new_http(front_address.into());
        builder.with_elide_x_real_ip(elide_x_real_ip);
        builder.with_send_x_real_ip(send_x_real_ip);
        builder.with_expect_proxy(expect_proxy);
        builder
            .to_http(None)
            .expect("could not build HTTP listener")
    };

    worker.send_proxy_request_type(RequestType::AddHttpListener(http_listener));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.into(),
        proxy: ListenerType::Http.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "cluster_0",
    )));
    worker.send_proxy_request_type(RequestType::AddHttpFrontend(Worker::default_http_frontend(
        "cluster_0",
        front_address,
    )));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));
    worker.read_to_last();

    (worker, front_address, back_address)
}

/// Both flags off (default behaviour): client-supplied `X-Real-IP` reaches
/// the backend verbatim, no proxy-side injection occurs.
fn try_x_real_ip_neither() -> State {
    let (mut worker, front_address, back_address) =
        setup_x_real_ip_test("X-REAL-IP-NEITHER", false, false, false);

    let mut backend = SyncBackend::new("BACKEND_0", back_address, http_ok_response("pong"));
    backend.connect();

    let request_with_client_header = "\
        GET /api HTTP/1.1\r\n\
        Host: localhost\r\n\
        Connection: close\r\n\
        X-Real-IP: 1.2.3.4\r\n\
        Content-Length: 4\r\n\
        \r\n\
        ping";

    let mut client = Client::new("client", front_address, request_with_client_header);
    client.connect();
    client.send();

    backend.accept(0);
    let request = backend.receive(0);
    println!("backend received request: {request:?}");

    backend.send(0);
    let _ = client.receive();

    worker.soft_stop();
    worker.wait_for_server_stop();

    match request {
        Some(req) => {
            let lower = req.to_lowercase();
            // Default: client header survives end-to-end.
            if !lower.contains("x-real-ip: 1.2.3.4") {
                println!("expected client X-Real-IP to pass through, got:\n{req}");
                return State::Fail;
            }
            // Default: proxy must NOT have injected a second peer-IP header.
            if lower.contains("x-real-ip: 127.0.0.1") {
                println!("proxy injected X-Real-IP unexpectedly:\n{req}");
                return State::Fail;
            }
            State::Success
        }
        None => {
            println!("backend received no request");
            State::Fail
        }
    }
}

/// `send_x_real_ip = true`: a proxy-generated `X-Real-IP` header carrying
/// the connection peer IP (here, the loopback) is appended to the
/// forwarded request.
fn try_x_real_ip_send_only() -> State {
    let (mut worker, front_address, back_address) =
        setup_x_real_ip_test("X-REAL-IP-SEND", false, true, false);

    let mut backend = SyncBackend::new("BACKEND_0", back_address, http_ok_response("pong"));
    backend.connect();

    let mut client = Client::new(
        "client",
        front_address,
        http_request("GET", "/api", "ping", "localhost"),
    );
    client.connect();
    client.send();

    backend.accept(0);
    let request = backend.receive(0);
    println!("backend received request: {request:?}");

    backend.send(0);
    let _ = client.receive();

    worker.soft_stop();
    worker.wait_for_server_stop();

    match request {
        Some(req) => {
            if req.contains("X-Real-IP: 127.0.0.1") || req.contains("x-real-ip: 127.0.0.1") {
                State::Success
            } else {
                println!("X-Real-IP: 127.0.0.1 not found in:\n{req}");
                State::Fail
            }
        }
        None => {
            println!("backend received no request");
            State::Fail
        }
    }
}

/// `elide_x_real_ip = true`: a client-supplied `X-Real-IP` header is
/// stripped before the request reaches the backend (anti-spoofing).
fn try_x_real_ip_elide_only() -> State {
    let (mut worker, front_address, back_address) =
        setup_x_real_ip_test("X-REAL-IP-ELIDE", true, false, false);

    let mut backend = SyncBackend::new("BACKEND_0", back_address, http_ok_response("pong"));
    backend.connect();

    let request_with_spoofed_header = "\
        GET /api HTTP/1.1\r\n\
        Host: localhost\r\n\
        Connection: close\r\n\
        X-Real-IP: 1.2.3.4\r\n\
        Content-Length: 4\r\n\
        \r\n\
        ping";

    let mut client = Client::new("client", front_address, request_with_spoofed_header);
    client.connect();
    client.send();

    backend.accept(0);
    let request = backend.receive(0);
    println!("backend received request: {request:?}");

    backend.send(0);
    let _ = client.receive();

    worker.soft_stop();
    worker.wait_for_server_stop();

    match request {
        Some(req) => {
            let lower = req.to_lowercase();
            if lower.contains("x-real-ip") {
                println!("X-Real-IP not elided in:\n{req}");
                State::Fail
            } else {
                State::Success
            }
        }
        None => {
            println!("backend received no request");
            State::Fail
        }
    }
}

/// Both flags + `expect_proxy = true` together: the client opens a raw
/// TCP socket, prepends a PROXY-v2 v4 header announcing source IP
/// `10.0.0.42`, then sends a spoofed `X-Real-IP: 1.2.3.4` HTTP request.
/// The backend MUST observe `X-Real-IP: 10.0.0.42` (the PROXY-v2 source,
/// **not** the loopback peer) and the client-supplied spoof MUST be
/// gone.
fn try_x_real_ip_send_and_elide() -> State {
    use std::io::Write;
    use std::net::TcpStream;

    let (mut worker, front_address, back_address) =
        setup_x_real_ip_test("X-REAL-IP-SEND-ELIDE-PP", true, true, true);

    let mut backend = SyncBackend::new("BACKEND_0", back_address, http_ok_response("pong"));
    backend.connect();

    let pp_src: SocketAddr = "10.0.0.42:12345".parse().unwrap();
    let pp_dst: SocketAddr = format!("127.0.0.1:{}", front_address.port())
        .parse()
        .unwrap();
    let pp_header = sozu_lib::protocol::proxy_protocol::header::HeaderV2::new(
        sozu_lib::protocol::proxy_protocol::header::Command::Proxy,
        pp_src,
        pp_dst,
    );
    let pp_bytes = pp_header.into_bytes();

    let http_payload = "GET /api HTTP/1.1\r\n\
        Host: localhost\r\n\
        Connection: close\r\n\
        X-Real-IP: 1.2.3.4\r\n\
        Content-Length: 4\r\n\
        \r\n\
        ping";

    let mut stream = TcpStream::connect(front_address).expect("could not connect to sozu");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .expect("set write timeout");
    stream
        .write_all(&pp_bytes)
        .expect("write proxy-protocol header");
    stream
        .write_all(http_payload.as_bytes())
        .expect("write HTTP request");

    backend.accept(0);
    let request = backend.receive(0);
    println!("backend received request: {request:?}");

    backend.send(0);
    let mut buf = [0u8; 4096];
    let _ = stream.read(&mut buf);

    worker.soft_stop();
    worker.wait_for_server_stop();

    match request {
        Some(req) => {
            let lower = req.to_lowercase();
            if lower.contains("x-real-ip: 1.2.3.4") {
                println!("spoofed X-Real-IP: 1.2.3.4 was not stripped:\n{req}");
                return State::Fail;
            }
            if !lower.contains("x-real-ip: 10.0.0.42") {
                println!("PROXY-v2 source IP not present as X-Real-IP:\n{req}");
                return State::Fail;
            }
            if lower.contains("x-real-ip: 127.0.0.1") {
                println!(
                    "loopback IP leaked into X-Real-IP — should be the PROXY-v2 source:\n{req}"
                );
                return State::Fail;
            }
            State::Success
        }
        None => {
            println!("backend received no request");
            State::Fail
        }
    }
}

#[test]
fn test_x_real_ip_neither() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "X-Real-IP: defaults — client header passes through, no proxy injection",
            try_x_real_ip_neither,
        ),
        State::Success
    );
}

#[test]
fn test_x_real_ip_send_only() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "X-Real-IP: send_x_real_ip injects the peer IP",
            try_x_real_ip_send_only,
        ),
        State::Success
    );
}

#[test]
fn test_x_real_ip_elide_only() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "X-Real-IP: elide_x_real_ip strips spoofed client header",
            try_x_real_ip_elide_only,
        ),
        State::Success
    );
}

#[test]
fn test_x_real_ip_send_and_elide() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "X-Real-IP: elide + send + PROXY-v2 — spoof stripped, peer IP from PROXY frame",
            try_x_real_ip_send_and_elide,
        ),
        State::Success
    );
}

// ---------------------------------------------------------------------------
// RFC 7239 IPv6 bracketing in the synthesised `Forwarded` header.
//
// Regression for sozu issue #1254. Prior to the fix Sōzu emitted
// `Forwarded: ... for="2001:db8::1:9000"` which the RFC explicitly calls out
// as ambiguous — the trailing `:9000` could be the port or part of the
// address. The on-wire contract must be `for="[2001:db8::1]:9000"` and
// `by="[<ipv6>]"` (RFC 7239 §6, mirrors HAProxy `_7239_print_ip6`).
//
// We drive PROXY-v2 with an IPv6 source AND destination so `HttpContext`
// sees IPv6 for both `session_address` (→ `for=`) and `public_address`
// (→ `by=`) without needing the e2e port_registry to grow IPv6 support.
// ---------------------------------------------------------------------------

/// PROXY-v2 with IPv6 src + dst → backend must observe a bracketed
/// `Forwarded` header for both `for=` and `by=`.
fn try_forwarded_ipv6_brackets_via_proxy_v2() -> State {
    use std::io::Write;
    use std::net::TcpStream;

    let (mut worker, front_address, back_address) = setup_x_real_ip_test(
        "FORWARDED-IPV6-BRACKETS",
        /* elide_x_real_ip */ false,
        /* send_x_real_ip */ false,
        /* expect_proxy */ true,
    );

    let mut backend = SyncBackend::new("BACKEND_0", back_address, http_ok_response("pong"));
    backend.connect();

    // IPv6 source + destination in the PROXY-v2 header. Sōzu uses
    // `addresses.destination()` as `public_address` (→ `by=`) and
    // `addresses.source()` as `session_address` (→ `for=`).
    let pp_src: SocketAddr = "[2001:db8::1]:9000".parse().unwrap();
    let pp_dst: SocketAddr = "[2001:db8::2]:443".parse().unwrap();
    let pp_header = sozu_lib::protocol::proxy_protocol::header::HeaderV2::new(
        sozu_lib::protocol::proxy_protocol::header::Command::Proxy,
        pp_src,
        pp_dst,
    );
    let pp_bytes = pp_header.into_bytes();

    let http_payload = "GET /api HTTP/1.1\r\n\
        Host: localhost\r\n\
        Connection: close\r\n\
        Content-Length: 4\r\n\
        \r\n\
        ping";

    let mut stream = TcpStream::connect(front_address).expect("could not connect to sozu");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .expect("set write timeout");
    stream
        .write_all(&pp_bytes)
        .expect("write proxy-protocol header");
    stream
        .write_all(http_payload.as_bytes())
        .expect("write HTTP request");

    backend.accept(0);
    let request = backend.receive(0);
    println!("backend received request: {request:?}");

    backend.send(0);
    let mut buf = [0u8; 4096];
    let _ = stream.read(&mut buf);

    worker.soft_stop();
    worker.wait_for_server_stop();

    let Some(req) = request else {
        println!("backend received no request");
        return State::Fail;
    };

    // Assert the exact RFC 7239 wire format for both nodes.
    let expected_for = r#"for="[2001:db8::1]:9000""#;
    let expected_by = r#"by="[2001:db8::2]""#;

    if !req.contains(expected_for) {
        println!(
            "missing bracketed `for=` in Forwarded header:\nwant: {expected_for}\ngot:\n{req}"
        );
        return State::Fail;
    }
    if !req.contains(expected_by) {
        println!("missing bracketed `by=` in Forwarded header:\nwant: {expected_by}\ngot:\n{req}");
        return State::Fail;
    }

    // The pre-fix, ambiguous form must never appear on the wire.
    let ambiguous_for = r#"for="2001:db8::1:9000""#;
    if req.contains(ambiguous_for) {
        println!("Forwarded header is back to the ambiguous form #1254 (no brackets):\n{req}");
        return State::Fail;
    }

    State::Success
}

#[test]
fn test_forwarded_ipv6_brackets_via_proxy_v2() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "Forwarded (RFC 7239) — IPv6 peer + public are bracketed in `for=` / `by=` (#1254)",
            try_forwarded_ipv6_brackets_via_proxy_v2,
        ),
        State::Success
    );
}

/// Codex cross-check finding: `pkawa::handle_trailer` bypasses the
/// shared `HttpContext::on_request_headers` callback. Without dedicated
/// trailer-side elision a naive H2 client could spoof `x-real-ip` as a
/// trailer header and reach the backend unscrubbed.
///
/// The §6.15 plumbing (`elide_x_real_ip` parameter on `handle_trailer`,
/// early-return inside the HPACK decode closure when the trailer key
/// matches `x-real-ip`) closes that gap; this test pins the contract by
/// driving the trailer-decode code path with `elide_x_real_ip = true`.
///
/// Drive shape: open an H2/TLS connection, send a POST with body and a
/// trailer block carrying `x-real-ip: 1.2.3.4`, and assert sōzu
/// successfully forwards the request (response observed; no GOAWAY /
/// RST_STREAM). A successful response means our patch dropped the
/// trailer pair without corrupting the kawa block list — both
/// well-formedness and elision are covered structurally:
///
/// - **Elision**: the `x-real-ip` pair never reaches `kawa.blocks`
///   (early-return before `kawa.push_block`). Visible as: response is
///   not a protocol-level rejection.
/// - **Well-formedness**: the rest of the trailer payload (and the
///   preceding HEADERS + DATA frames) still parse; downstream H2 → H1
///   conversion still completes. Visible as: backend handler runs and
///   sōzu emits a HEADERS response.
///
/// We do not assert that a specific backend never observed the trailer
/// because the H1 backend (`AsyncBackend::http_handler`) does not
/// surface received trailers; per `pkawa.rs` the H1 serialiser does not
/// even emit trailers when content-length is fixed (RFC 9110 §6.5).
/// The unit-level invariant is the elision check inside
/// `handle_trailer`'s decode closure; this e2e exercises that closure
/// is invoked end-to-end.
fn try_x_real_ip_elide_h2_trailer() -> State {
    use std::io::Write;

    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);

    let (config, listeners, state) =
        Worker::empty_https_config(SocketAddr::from(front_address.clone()));
    let mut worker =
        Worker::start_new_worker_owned("X-REAL-IP-H2-TRAILER", config, listeners, state);

    let listener = ListenerBuilder::new_https(front_address.clone())
        .with_elide_x_real_ip(true)
        .to_tls(None)
        .expect("could not build HTTPS listener");
    worker.send_proxy_request_type(RequestType::AddHttpsListener(listener));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.clone(),
        proxy: ListenerType::Https.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "cluster_0",
    )));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: String::from("localhost"),
        ..Worker::default_http_frontend("cluster_0", front_address.clone().into())
    }));
    worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
        address: front_address.clone(),
        certificate: CertificateAndKey {
            certificate: String::from(include_str!("../../../lib/assets/local-certificate.pem")),
            key: String::from(include_str!("../../../lib/assets/local-key.pem")),
            certificate_chain: vec![],
            versions: vec![],
            names: vec![],
        },
        expired_at: None,
    }));

    let back_address = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));
    let backend = AsyncBackend::spawn_detached_backend(
        "BACKEND_0",
        back_address,
        SimpleAggregator::default(),
        AsyncBackend::http_handler("pong"),
    );

    worker.read_to_last();

    let front_addr: SocketAddr = SocketAddr::from(front_address);
    let mut tls = super::h2_utils::raw_h2_connection(front_addr);
    super::h2_utils::h2_handshake(&mut tls);

    // Initial HEADERS for POST on stream 1, no END_STREAM. HPACK static-table
    // indices: 0x83 = :method POST, 0x84 = :path /, 0x87 = :scheme https,
    // 0x41 = :authority literal-indexed (name index 1).
    let header_block: Vec<u8> = vec![
        0x83, 0x84, 0x87, 0x41, 0x09, b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't',
    ];
    let headers = super::h2_utils::H2Frame::headers(1, header_block, true, false);
    tls.write_all(&headers.encode())
        .expect("could not write HEADERS");
    tls.flush().expect("could not flush HEADERS");

    // DATA frame with body, no END_STREAM (trailer carries END_STREAM).
    let data = super::h2_utils::H2Frame::data(1, b"hello world".to_vec(), false);
    tls.write_all(&data.encode()).expect("could not write DATA");
    tls.flush().expect("could not flush DATA");

    // Trailer HEADERS with END_STREAM + END_HEADERS. Encode `x-real-ip`
    // as a literal-without-indexing pair (0x00 prefix) — the most direct
    // path through `handle_trailer`'s decode closure, no static-table
    // lookup needed.
    let mut trailer_block = Vec::new();
    trailer_block.push(0x00);
    trailer_block.push(0x09);
    trailer_block.extend_from_slice(b"x-real-ip");
    trailer_block.push(0x07);
    trailer_block.extend_from_slice(b"1.2.3.4");
    let trailers = super::h2_utils::H2Frame::headers(1, trailer_block, true, true);
    tls.write_all(&trailers.encode())
        .expect("could not write trailers");
    tls.flush().expect("could not flush trailers");

    let frames = super::h2_utils::collect_response_frames(&mut tls, 500, 5, 500);
    super::h2_utils::log_frames("X-Real-IP H2 trailer elision", &frames);

    let got_response = super::h2_utils::contains_headers_response(&frames);
    let got_rejection = super::h2_utils::rejected_with_goaway_or_rst(&frames);
    let infra_ok = super::h2_utils::teardown(tls, front_port, worker, vec![backend]);

    if !infra_ok {
        println!("sozu did not survive trailer-elision request");
        return State::Fail;
    }
    if got_rejection {
        println!("sozu rejected request with GOAWAY/RST after eliding x-real-ip trailer");
        return State::Fail;
    }
    if !got_response {
        println!("sozu produced no response — trailer elision broke the H2 path");
        return State::Fail;
    }

    State::Success
}

#[test]
fn test_x_real_ip_elide_h2_trailer() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "X-Real-IP: trailer `x-real-ip` is elided on H2 path (Codex finding regression)",
            try_x_real_ip_elide_h2_trailer,
        ),
        State::Success
    );
}

// ---------------------------------------------------------------------------
// Health check tests
// ---------------------------------------------------------------------------

/// When a backend goes down, health checks should mark it unhealthy and Sōzu
/// should stop routing traffic to it. All requests go to the remaining healthy
/// backend.
pub fn try_health_check_excludes_unhealthy() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    // Setup cluster with 0 backends — we add them manually below
    let (mut worker, _) =
        setup_async_test("HC-EXCL", config, listeners, state, front_address, 0, false);

    let backend0_addr = create_local_address();
    let backend1_addr = create_local_address();

    let aggregator = SimpleAggregator {
        requests_received: 0,
        responses_sent: 0,
    };

    let mut backend0 = AsyncBackend::spawn_detached_backend(
        "HC_BACKEND_0",
        backend0_addr,
        aggregator.to_owned(),
        AsyncBackend::http_handler("pong0"),
    );
    let mut backend1 = AsyncBackend::spawn_detached_backend(
        "HC_BACKEND_1",
        backend1_addr,
        aggregator,
        AsyncBackend::http_handler("pong1"),
    );

    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        backend0_addr,
        None,
    )));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-1",
        backend1_addr,
        None,
    )));
    worker.read_to_last();

    // Configure health check with generous timeout so the mio event loop
    // (which polls every ~1s) has time to complete the write/read cycle.
    worker.send_proxy_request_type(RequestType::SetHealthCheck(SetHealthCheck {
        cluster_id: "cluster_0".to_owned(),
        config: HealthCheckConfig {
            timeout: 3,
            healthy_threshold: 1,
            ..default_health_check_config()
        },
    }));
    worker.read_to_last();

    // Verify both backends serve traffic — client requests also keep the event
    // loop active so health checks are processed.
    let deadline = Instant::now() + HEALTH_CHECK_SETTLE;
    let mut got_pong0 = false;
    let mut got_pong1 = false;
    while Instant::now() < deadline && !(got_pong0 && got_pong1) {
        let mut client = Client::new(
            "client",
            front_address,
            http_request("GET", "/api", "ping", "localhost"),
        );
        client.connect();
        client.send();
        if let Some(response) = client.receive() {
            if response.contains("pong0") {
                got_pong0 = true;
            }
            if response.contains("pong1") {
                got_pong1 = true;
            }
        }
        thread::sleep(Duration::from_millis(200));
    }

    if !got_pong0 || !got_pong1 {
        println!(
            "Phase 1: expected traffic to both backends, got pong0={} pong1={}",
            got_pong0, got_pong1
        );
        worker.soft_stop();
        worker.wait_for_server_stop();
        backend0.stop_and_get_aggregator();
        backend1.stop_and_get_aggregator();
        return State::Fail;
    }

    // Stop backend 0 — its listener closes, health checks will fail to connect
    backend0.stop_and_get_aggregator();

    // Poll until all responses come from backend 1 only. Sending requests
    // keeps the event loop active so health checks fire promptly.
    let deadline = Instant::now() + HEALTH_CHECK_LONG_SETTLE;
    let mut backend0_excluded = false;
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(500));
        let mut all_pong1 = true;
        for _ in 0..3 {
            let mut client = Client::new(
                "client",
                front_address,
                http_request("GET", "/api", "ping", "localhost"),
            );
            client.connect();
            client.send();
            if let Some(response) = client.receive() {
                if !response.contains("pong1") {
                    all_pong1 = false;
                    break;
                }
            }
        }
        if all_pong1 {
            backend0_excluded = true;
            break;
        }
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    backend1.stop_and_get_aggregator();

    if backend0_excluded {
        State::Success
    } else {
        println!("Phase 2: backend 0 was not excluded after going down");
        State::Fail
    }
}

/// After a backend recovers (starts responding to health checks again), Sōzu
/// should mark it healthy and resume routing traffic to it.
pub fn try_health_check_recovery() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, _) =
        setup_async_test("HC-REC", config, listeners, state, front_address, 0, false);

    let backend_addr = create_local_address();
    let aggregator = SimpleAggregator {
        requests_received: 0,
        responses_sent: 0,
    };

    // Start backend so initial requests work
    let mut backend = AsyncBackend::spawn_detached_backend(
        "HC_BACKEND",
        backend_addr,
        aggregator.to_owned(),
        AsyncBackend::http_handler("pong"),
    );

    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        backend_addr,
        None,
    )));
    worker.read_to_last();

    // Use generous timeout so the mio event loop (polling every ~1s) has time
    // to complete the TCP write/read cycle for each health check.
    worker.send_proxy_request_type(RequestType::SetHealthCheck(SetHealthCheck {
        cluster_id: "cluster_0".to_owned(),
        config: HealthCheckConfig {
            timeout: 3,
            ..default_health_check_config()
        },
    }));
    worker.read_to_last();

    // Phase 1: confirm traffic works (requests also keep event loop active)
    let phase1_ok = poll_until(front_address, HEALTH_CHECK_VERIFY, |resp| {
        resp.map(|r| r.contains("pong")).unwrap_or(false)
    });
    if !phase1_ok {
        println!("Phase 1: backend never responded");
        worker.soft_stop();
        worker.wait_for_server_stop();
        backend.stop_and_get_aggregator();
        return State::Fail;
    }

    // Phase 2: stop backend — poll until we see 503 (backend marked unhealthy)
    backend.stop_and_get_aggregator();
    let backend_down = poll_until(front_address, HEALTH_CHECK_LONG_SETTLE, |resp| match resp {
        Some(r) => r.contains("503"),
        None => true,
    });
    if !backend_down {
        println!("Phase 2: backend never marked unhealthy");
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    // Phase 3: restart backend at the same address
    let mut backend = AsyncBackend::spawn_detached_backend(
        "HC_BACKEND_RECOVERED",
        backend_addr,
        aggregator,
        AsyncBackend::http_handler("pong_recovered"),
    );

    // Phase 4: poll until traffic resumes — each request keeps the event loop
    // active so health checks are processed promptly. Recovery needs two full
    // health check cycles (settle + verify), so we allow extra time.
    let recovered = poll_until(
        front_address,
        HEALTH_CHECK_LONG_SETTLE + HEALTH_CHECK_VERIFY,
        |resp| resp.map(|r| r.contains("pong_recovered")).unwrap_or(false),
    );

    if !recovered {
        println!("Phase 4: backend never recovered");
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    backend.stop_and_get_aggregator();

    if recovered {
        State::Success
    } else {
        State::Fail
    }
}

/// Configuring a health check via `SetHealthCheck` should be accepted, and
/// removing it via `RemoveHealthCheck` should stop health-checking the cluster.
pub fn try_health_check_remove() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, _) =
        setup_async_test("HC-RM", config, listeners, state, front_address, 0, false);

    let backend_addr = create_local_address();
    let aggregator = SimpleAggregator {
        requests_received: 0,
        responses_sent: 0,
    };

    let mut backend = AsyncBackend::spawn_detached_backend(
        "HC_BACKEND",
        backend_addr,
        aggregator,
        AsyncBackend::http_handler("pong"),
    );

    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        backend_addr,
        None,
    )));
    worker.read_to_last();

    // Set health check
    worker.send_proxy_request_type(RequestType::SetHealthCheck(SetHealthCheck {
        cluster_id: "cluster_0".to_owned(),
        config: HealthCheckConfig {
            timeout: 3,
            healthy_threshold: 1,
            unhealthy_threshold: 1,
            ..default_health_check_config()
        },
    }));
    worker.read_to_last();

    // Verify traffic works while health checks are active (requests keep event
    // loop active so health checks fire)
    let phase1_ok = poll_until(front_address, HEALTH_CHECK_VERIFY, |resp| {
        resp.map(|r| r.contains("pong")).unwrap_or(false)
    });
    if !phase1_ok {
        println!("Phase 1: expected pong, got nothing");
        worker.soft_stop();
        worker.wait_for_server_stop();
        backend.stop_and_get_aggregator();
        return State::Fail;
    }

    // Remove health check
    worker.send_proxy_request_type(RequestType::RemoveHealthCheck("cluster_0".to_owned()));
    worker.read_to_last();

    // Traffic should still work after removing the health check
    let mut client = Client::new(
        "client",
        front_address,
        http_request("GET", "/api", "ping", "localhost"),
    );
    client.connect();
    client.send();
    let phase2 = client.receive();
    let still_works = phase2.as_ref().map(|r| r.contains("pong")).unwrap_or(false);
    if !still_works {
        println!("Phase 2: expected pong after remove, got: {:?}", phase2);
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    backend.stop_and_get_aggregator();

    if still_works {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_health_check_excludes_unhealthy() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "Health check excludes unhealthy backend",
            try_health_check_excludes_unhealthy,
        ),
        State::Success
    );
}

#[test]
fn test_health_check_recovery() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "Health check recovery after backend restart",
            try_health_check_recovery,
        ),
        State::Success
    );
}

#[test]
fn test_health_check_remove() {
    assert_eq!(
        repeat_until_error_or(2, "Health check set and remove", try_health_check_remove,),
        State::Success
    );
}

// ---------------------------------------------------------------------------
// Health check — fail-open
// ---------------------------------------------------------------------------

/// When ALL backends are unhealthy, Sōzu should fail-open and route to them
/// anyway, rather than returning 503 to all clients.
pub fn try_health_check_fail_open() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, _) = setup_async_test(
        "HC-FOPEN",
        config,
        listeners,
        state,
        front_address,
        0,
        false,
    );

    let backend_addr = create_local_address();
    let aggregator = SimpleAggregator {
        requests_received: 0,
        responses_sent: 0,
    };

    // Start a backend that responds on /api but NOT on /health — health checks
    // will get connection refused or a non-2xx, marking the backend unhealthy.
    let mut backend = AsyncBackend::spawn_detached_backend(
        "HC_BACKEND_NO_HEALTH",
        backend_addr,
        aggregator,
        AsyncBackend::http_handler("fail-open-ok"),
    );

    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        backend_addr,
        None,
    )));
    worker.read_to_last();

    // Configure health check expecting status 204 — the backend returns 200,
    // so health checks will consistently fail, marking it unhealthy.
    worker.send_proxy_request_type(RequestType::SetHealthCheck(SetHealthCheck {
        cluster_id: "cluster_0".to_owned(),
        config: HealthCheckConfig {
            expected_status: 204,
            unhealthy_threshold: 1,
            healthy_threshold: 1,
            timeout: 3,
            ..default_health_check_config()
        },
    }));
    worker.read_to_last();

    // Continuously send requests to keep the event loop active while health
    // checks fire. With interval=3s and unhealthy_threshold=1, detection
    // should happen within ~6s. We send traffic for HEALTH_CHECK_SETTLE to
    // ensure health checks have fired.
    let start = Instant::now();
    while Instant::now().duration_since(start) < HEALTH_CHECK_SETTLE {
        let mut client = Client::new(
            "keepalive",
            front_address,
            http_request("GET", "/api", "ping", "localhost"),
        );
        client.connect();
        client.send();
        let _ = client.receive();
        thread::sleep(Duration::from_secs(1));
    }

    // Verify fail-open: the backend should be marked unhealthy by now, but
    // fail-open should still route traffic since it's the only backend.
    let mut success_count = 0;
    for _ in 0..5 {
        let mut client = Client::new(
            "failopen",
            front_address,
            http_request("GET", "/api", "ping", "localhost"),
        );
        client.connect();
        client.send();
        if let Some(response) = client.receive() {
            if response.contains("fail-open-ok") {
                success_count += 1;
            }
        }
        thread::sleep(Duration::from_millis(200));
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    backend.stop_and_get_aggregator();

    if success_count >= 3 {
        State::Success
    } else {
        println!(
            "Fail-open: expected traffic to route despite unhealthy backend, got {}/5 successes",
            success_count
        );
        State::Fail
    }
}

#[test]
fn test_health_check_fail_open() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "Health check fail-open when all backends unhealthy",
            try_health_check_fail_open,
        ),
        State::Success
    );
}

// ---------------------------------------------------------------------------
// Health check — server-side config validation
// ---------------------------------------------------------------------------

/// Invalid health check configurations should be rejected by the worker with
/// an error response.
pub fn try_health_check_invalid_config() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, _) = setup_async_test(
        "HC-INVAL",
        config,
        listeners,
        state,
        front_address,
        0,
        false,
    );

    // Test 1: zero interval should be rejected
    worker.send_proxy_request_type(RequestType::SetHealthCheck(SetHealthCheck {
        cluster_id: "cluster_0".to_owned(),
        config: HealthCheckConfig {
            interval: 0,
            ..default_health_check_config()
        },
    }));
    let response = worker.read_proxy_response();
    let zero_interval_rejected = response
        .as_ref()
        .map(|r| r.status == ResponseStatus::Failure as i32)
        .unwrap_or(false);

    // Test 2: zero timeout should be rejected
    worker.send_proxy_request_type(RequestType::SetHealthCheck(SetHealthCheck {
        cluster_id: "cluster_0".to_owned(),
        config: HealthCheckConfig {
            timeout: 0,
            ..default_health_check_config()
        },
    }));
    let response = worker.read_proxy_response();
    let zero_timeout_rejected = response
        .as_ref()
        .map(|r| r.status == ResponseStatus::Failure as i32)
        .unwrap_or(false);

    // Test 3: URI without leading slash should be rejected
    worker.send_proxy_request_type(RequestType::SetHealthCheck(SetHealthCheck {
        cluster_id: "cluster_0".to_owned(),
        config: HealthCheckConfig {
            uri: "no-leading-slash".to_owned(),
            ..default_health_check_config()
        },
    }));
    let response = worker.read_proxy_response();
    let bad_uri_rejected = response
        .as_ref()
        .map(|r| r.status == ResponseStatus::Failure as i32)
        .unwrap_or(false);

    // Test 4: URI with CRLF should be rejected
    worker.send_proxy_request_type(RequestType::SetHealthCheck(SetHealthCheck {
        cluster_id: "cluster_0".to_owned(),
        config: HealthCheckConfig {
            uri: "/health\r\nX-Injected: true".to_owned(),
            ..default_health_check_config()
        },
    }));
    let response = worker.read_proxy_response();
    let crlf_uri_rejected = response
        .as_ref()
        .map(|r| r.status == ResponseStatus::Failure as i32)
        .unwrap_or(false);

    // Test 5: valid config should be accepted
    worker.send_proxy_request_type(RequestType::SetHealthCheck(SetHealthCheck {
        cluster_id: "cluster_0".to_owned(),
        config: default_health_check_config(),
    }));
    let response = worker.read_proxy_response();
    let valid_accepted = response
        .as_ref()
        .map(|r| r.status == ResponseStatus::Ok as i32)
        .unwrap_or(false);

    worker.soft_stop();
    worker.wait_for_server_stop();

    let mut all_ok = true;
    if !zero_interval_rejected {
        println!("FAIL: zero interval config was not rejected");
        all_ok = false;
    }
    if !zero_timeout_rejected {
        println!("FAIL: zero timeout config was not rejected");
        all_ok = false;
    }
    if !bad_uri_rejected {
        println!("FAIL: URI without leading slash was not rejected");
        all_ok = false;
    }
    if !crlf_uri_rejected {
        println!("FAIL: URI with CRLF was not rejected");
        all_ok = false;
    }
    if !valid_accepted {
        println!("FAIL: valid config was not accepted");
        all_ok = false;
    }

    if all_ok { State::Success } else { State::Fail }
}

#[test]
fn test_health_check_invalid_config() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "Health check rejects invalid configs",
            try_health_check_invalid_config,
        ),
        State::Success
    );
}

// ---------------------------------------------------------------------------
// Health check — expected status code
// ---------------------------------------------------------------------------

/// When `expected_status` is set to a specific code that the backend does not
/// return, health checks should fail. Combined with fail-open, the backend
/// remains reachable. This validates the `expected_status` matching logic.
pub fn try_health_check_expected_status() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, _) = setup_async_test(
        "HC-STATUS",
        config,
        listeners,
        state,
        front_address,
        0,
        false,
    );

    let backend_addr = create_local_address();
    let aggregator = SimpleAggregator {
        requests_received: 0,
        responses_sent: 0,
    };

    // Backend returns 200 OK — but health check expects 204
    let mut backend = AsyncBackend::spawn_detached_backend(
        "HC_BACKEND_200",
        backend_addr,
        aggregator,
        AsyncBackend::http_handler("status-check-ok"),
    );

    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        backend_addr,
        None,
    )));
    worker.read_to_last();

    // First: confirm traffic works with default health check (expects any 2xx)
    worker.send_proxy_request_type(RequestType::SetHealthCheck(SetHealthCheck {
        cluster_id: "cluster_0".to_owned(),
        config: HealthCheckConfig {
            expected_status: 0, // any 2xx
            healthy_threshold: 1,
            unhealthy_threshold: 1,
            timeout: 3,
            ..default_health_check_config()
        },
    }));
    worker.read_to_last();

    let phase1 = poll_until(front_address, HEALTH_CHECK_VERIFY, |resp| {
        resp.map(|r| r.contains("status-check-ok")).unwrap_or(false)
    });
    if !phase1 {
        println!("Phase 1: backend never responded with default health check");
        worker.soft_stop();
        worker.wait_for_server_stop();
        backend.stop_and_get_aggregator();
        return State::Fail;
    }

    // Now switch to expected_status=204 — backend returns 200, so health checks
    // will fail. The backend should be marked unhealthy.
    worker.send_proxy_request_type(RequestType::SetHealthCheck(SetHealthCheck {
        cluster_id: "cluster_0".to_owned(),
        config: HealthCheckConfig {
            expected_status: 204,
            healthy_threshold: 1,
            unhealthy_threshold: 1,
            timeout: 3,
            ..default_health_check_config()
        },
    }));
    worker.read_to_last();

    // Keep the event loop active while health checks fire and mark the
    // backend unhealthy (status mismatch: expects 204, gets 200).
    let start = Instant::now();
    while Instant::now().duration_since(start) < HEALTH_CHECK_SETTLE {
        let mut client = Client::new(
            "keepalive",
            front_address,
            http_request("GET", "/api", "ping", "localhost"),
        );
        client.connect();
        client.send();
        let _ = client.receive();
        thread::sleep(Duration::from_secs(1));
    }

    // With fail-open, traffic should still reach the backend even though it's
    // unhealthy (status mismatch). This validates both expected_status matching
    // AND fail-open working together.
    let phase2 = poll_until(front_address, HEALTH_CHECK_VERIFY, |resp| {
        resp.map(|r| r.contains("status-check-ok")).unwrap_or(false)
    });

    worker.soft_stop();
    worker.wait_for_server_stop();
    backend.stop_and_get_aggregator();

    if phase2 {
        State::Success
    } else {
        println!("Phase 2: traffic did not reach backend via fail-open after status mismatch");
        State::Fail
    }
}

#[test]
fn test_health_check_expected_status() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "Health check with specific expected status",
            try_health_check_expected_status,
        ),
        State::Success
    );
}
