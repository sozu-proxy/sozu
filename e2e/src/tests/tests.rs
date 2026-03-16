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
        ListenerType, RemoveBackend, RequestHttpFrontend, SocketAddress, request::RequestType,
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
    tests::{State, provide_port, repeat_until_error_or, setup_async_test, setup_sync_test},
};

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

pub fn try_tls_endpoint() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let back_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker("TLS-ENDPOINT", config, &listeners, state);

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

    let hostname = "localhost".to_string();
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: hostname.to_owned(),
        ..Worker::default_http_frontend("cluster_0", front_address.clone().into())
    }));

    let certificate_and_key = CertificateAndKey {
        certificate: String::from(include_str!("../../../lib/assets/local-certificate.pem")),
        key: String::from(include_str!("../../../lib/assets/local-key.pem")),
        certificate_chain: vec![], // in config.toml the certificate chain would be the same as the certificate
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
    match client.receive() {
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
    match client.receive() {
        Some(msg) => println!("in-flight response after upgrade: {msg}"),
        None => return State::Fail,
    }

    // Verify new worker accepts new connections
    client.connect();
    client.send();
    backend.accept(1);
    backend.receive(1);
    backend.send(1);
    match client.receive() {
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
    match client.receive() {
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
    match new_client.receive() {
        Some(msg) => println!("new client response from new worker: {msg}"),
        None => return State::Fail,
    }

    // Send multiple requests on keep-alive to verify routing is stable
    for i in 0..3 {
        new_client.send();
        backend.receive(1);
        backend.send(1);
        match new_client.receive() {
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
    match another_client.receive() {
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
        match client.receive() {
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
        match client.receive() {
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
    match clients[0].receive() {
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

    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker(name, config, &listeners, state);

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

    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker(name, config, &listeners, state);

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
/// This test exercises 146 RFC 7540/9113 conformance scenarios using h2spec.
/// It is ignored by default because it requires the h2spec binary in PATH.
///
/// Install h2spec:
///   go install github.com/summerwind/h2spec/cmd/h2spec@latest
///
/// Run this test explicitly:
///   cargo test -p sozu-e2e -- --ignored test_h2spec_conformance --nocapture
#[test]
#[ignore]
fn test_h2spec_conformance() {
    use std::process::Command;

    // Verify h2spec is available
    let h2spec_version = Command::new("h2spec")
        .arg("--version")
        .output()
        .expect("h2spec binary not found in PATH. Install with: go install github.com/summerwind/h2spec/cmd/h2spec@latest");
    assert!(
        h2spec_version.status.success(),
        "h2spec --version failed: {}",
        String::from_utf8_lossy(&h2spec_version.stderr)
    );
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

    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker(name, config, &listeners, state);

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
