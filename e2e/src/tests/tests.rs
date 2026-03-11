use std::{
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
        HealthCheckConfig, ListenerType, RemoveBackend, RequestHttpFrontend, ResponseStatus,
        SetHealthCheck, SocketAddress, TlsVersion, request::RequestType,
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
        https_client::{build_https_client, resolve_request},
        sync_backend::Backend as SyncBackend,
    },
    sozu::worker::Worker,
    tests::{
        State, provide_port, repeat_until_error_or, setup_async_test, setup_sync_test, setup_test,
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

    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker("810-PANIC", config, &listeners, state);

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

    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker(test_name, config, &listeners, state);

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

pub fn try_tls_endpoint() -> State {
    try_tls_with_cert(
        "TLS-ENDPOINT",
        include_str!("../../../lib/assets/local-certificate.pem"),
        include_str!("../../../lib/assets/local-key.pem"),
        None,
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

pub fn test_upgrade() -> State {
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
    match client.receive() {
        Some(msg) => println!("response: {msg}"),
        None => return State::Fail,
    }

    client.send();
    backend.receive(0);
    let mut new_worker = worker.upgrade("NEW_WORKER");
    thread::sleep(Duration::from_millis(100));
    backend.send(0);
    match client.receive() {
        Some(msg) => println!("response: {msg}"),
        None => return State::Fail,
    }
    client.connect();
    client.send();
    println!("ACCEPTING...");
    backend.accept(1);
    backend.receive(1);
    backend.send(1);
    match client.receive() {
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

    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker("BEHAVE-WORKER", config, &listeners, state);

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

    info!("server closes, expecting 503");
    // TODO: what if the client continue to use the closed stream
    client.connect();
    client.send();
    backend.accept(0);
    let request = backend.receive(0);
    backend.close(0);

    let response = client.receive();
    println!("request: {request:?}");
    println!("response: {response:?}");
    assert_eq!(response, Some(immutable_answer(503)));
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

fn try_https_redirect() -> State {
    let front_address: SocketAddr = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker("BEHAVE-WORKER", config, &listeners, state);

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

    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker("WLD_CRD", config, &listeners, state);
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

#[test]
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

#[test]
fn test_http_behaviors() {
    assert_eq!(
        repeat_until_error_or(10, "HTTP stack", try_http_behaviors),
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
    let backend_down = poll_until(front_address, HEALTH_CHECK_LONG_SETTLE, |resp| {
        match resp {
            Some(r) => r.contains("503"),
            None => true,
        }
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
    let (mut worker, _) =
        setup_async_test("HC-FOPEN", config, listeners, state, front_address, 0, false);

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
    let (mut worker, _) =
        setup_async_test("HC-INVAL", config, listeners, state, front_address, 0, false);

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

    if all_ok {
        State::Success
    } else {
        State::Fail
    }
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
    let (mut worker, _) =
        setup_async_test("HC-STATUS", config, listeners, state, front_address, 0, false);

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
