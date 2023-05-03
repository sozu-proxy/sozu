use std::{
    io::stdout,
    net::SocketAddr,
    thread,
    time::{Duration, Instant},
};

use serial_test::serial;

use sozu_command_lib::{
    config::{FileConfig, ListenerBuilder},
    info,
    logging::{Logger, LoggerBackend},
    proto::command::{
        request::RequestType, ActivateListener, AddCertificate, CertificateAndKey, ListenerType,
        RemoveBackend, RequestHttpFrontend,
    },
    state::ConfigState,
};

use crate::{
    http_utils::{default_404_answer, default_503_answer, http_ok_response, http_request},
    mock::{
        aggregator::SimpleAggregator,
        async_backend::BackendHandle as AsyncBackend,
        client::Client,
        https_client::{build_https_client, resolve_request},
        sync_backend::Backend as SyncBackend,
    },
    sozu::worker::Worker,
    tests::{repeat_until_error_or, setup_async_test, setup_sync_test, State},
};

pub fn try_async(nb_backends: usize, nb_clients: usize, nb_requests: usize) -> State {
    let front_address = "127.0.0.1:2001"
        .parse()
        .expect("could not parse front address");

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
    let front_address = "127.0.0.1:2001"
        .parse()
        .expect("could not parse front address");

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
    let front_address = "127.0.0.1:2001"
        .parse()
        .expect("could not parse front address");

    let config = Worker::into_config(FileConfig {
        zombie_check_interval: zombie,
        ..Worker::empty_file_config()
    });
    let listeners = Worker::empty_listeners();
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
    let front_address = "127.0.0.1:2001"
        .parse()
        .expect("could not parse front address");

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
    let front_address: SocketAddr = "127.0.0.1:2001"
        .parse()
        .expect("could not parse front address");
    let back_address = "127.0.0.1:2002"
        .parse::<std::net::SocketAddr>()
        .expect("could not parse back address");

    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker("810-PANIC", config, &listeners, state);

    worker.send_proxy_request_type(RequestType::AddTcpListener(
        ListenerBuilder::new_tcp(front_address)
            .to_tcp(None)
            .unwrap(),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.to_string(),
        proxy: ListenerType::Tcp.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "cluster_0",
        false,
    )));
    worker.send_proxy_request_type(RequestType::AddTcpFrontend(Worker::default_tcp_frontend(
        "cluster_0",
        front_address.to_string(),
    )));

    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address.to_string(),
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

    if success {
        State::Success
    } else {
        State::Fail
    }
}

pub fn try_tls_endpoint() -> State {
    let front_address: SocketAddr = "127.0.0.1:2001"
        .parse()
        .expect("could not parse front address");
    let back_address = "127.0.0.1:2002"
        .parse::<std::net::SocketAddr>()
        .expect("could not parse back address");

    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker("TLS-ENDPOINT", config, &listeners, state);

    worker.send_proxy_request_type(RequestType::AddHttpsListener(
        ListenerBuilder::new_https(front_address)
            .to_tls(None)
            .unwrap(),
    ));

    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.to_string(),
        proxy: ListenerType::Https.into(),
        from_scm: false,
    }));

    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "cluster_0",
        false,
    )));

    let hostname = "localhost".to_string();
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: hostname.to_owned(),
        ..Worker::default_http_frontend("cluster_0", front_address)
    }));

    let certificate_and_key = CertificateAndKey {
        certificate: String::from(include_str!("../../../lib/assets/local-certificate.pem")),
        key: String::from(include_str!("../../../lib/assets/local-key.pem")),
        certificate_chain: vec![],
        versions: vec![],
        names: vec![],
    };
    let add_certificate = AddCertificate {
        address: front_address.to_string(),
        certificate: certificate_and_key,
        expired_at: None,
    };
    worker.send_proxy_request_type(RequestType::AddCertificate(add_certificate));

    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address.to_string(),
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
    let request = client.get(format!("https://{hostname}:2001/api").parse().unwrap());
    if let Some((status, body)) = resolve_request(request) {
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

pub fn test_upgrade() -> State {
    let front_address = "127.0.0.1:2001"
        .parse()
        .expect("could not parse front address");

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
    let front_address = "127.0.0.1:2001"
        .parse()
        .expect("could not parse front address");

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

    if success {
        State::Success
    } else {
        State::Fail
    }
}

fn try_http_behaviors() -> State {
    use sozu_command_lib::log;
    Logger::init(
        "BEHAVE-OUT".to_string(),
        "debug",
        LoggerBackend::Stdout(stdout()),
        None,
    );

    info!("starting up");

    let front_address: SocketAddr = "127.0.0.1:2001"
        .parse()
        .expect("could not parse front address");

    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker("BEHAVE-WORKER", config, &listeners, state);

    worker.send_proxy_request_type(RequestType::AddHttpListener(
        ListenerBuilder::new_http(front_address)
            .to_http(None)
            .unwrap(),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.to_string(),
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
    assert_eq!(response, Some(default_404_answer()));
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
    assert_eq!(response, Some(default_503_answer()));
    assert_eq!(client.receive(), None);

    let back_address = "127.0.0.1:2002"
        .parse::<std::net::SocketAddr>()
        .expect("could not parse back address");
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0".to_string(),
        back_address.to_string(),
        None,
    )));
    worker.read_to_last();

    info!("sending invalid request, expecting 400");
    client.set_request("HELLO\r\n\r\n");
    client.connect();
    client.send();

    let expected_response = String::from(
        "HTTP/1.1 400 Bad Request\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
    );
    let response = client.receive();
    println!("response: {response:?}");
    assert_eq!(response, Some(expected_response));
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

    let expected_response = String::from(
        "HTTP/1.1 502 Bad Gateway\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
    );
    let response = client.receive();
    println!("request: {request:?}");
    println!("response: {response:?}");
    assert_eq!(response, Some(expected_response));
    assert_eq!(client.receive(), None);

    info!("expecting 200");
    worker.send_proxy_request_type(RequestType::RemoveBackend(RemoveBackend {
        cluster_id: String::from("cluster_0"),
        backend_id: String::from("cluster_0-0"),
        address: back_address.to_string(),
    }));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0".to_string(),
        back_address.to_string(),
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
    assert_eq!(response, Some(default_503_answer()));
    assert_eq!(client.receive(), None);

    worker.send_proxy_request_type(RequestType::RemoveBackend(RemoveBackend {
        cluster_id: String::from("cluster_0"),
        backend_id: String::from("cluster_0-0"),
        address: back_address.to_string(),
    }));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0".to_string(),
        back_address.to_string(),
        None,
    )));
    backend.disconnect();
    worker.read_to_last();

    let mut backend = SyncBackend::new(
        "backend",
        back_address,
        "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: WebSocket\r\n\r\nearly",
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
    client.set_request("ping");
    client.send();
    let request = backend.receive(1);
    backend.send(1);
    let response = client.receive().unwrap();
    println!("request: {request:?}");
    println!("response: {response:?}");

    worker.hard_stop();
    worker.wait_for_server_stop();

    info!("good bye");
    State::Success
}

fn try_msg_close() -> State {
    let front_address = "127.0.0.1:2001"
        .parse()
        .expect("could not parse front address");

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
    let front_address = "127.0.0.1:2001"
        .parse()
        .expect("could not parse front address");

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, _) = setup_async_test("BG", config, listeners, state, front_address, 0, false);

    let aggrerator = SimpleAggregator {
        requests_received: 0,
        responses_sent: 0,
    };

    let primary_address = "127.0.0.1:2002"
        .parse()
        .expect("could not parse back address");
    let secondary_address = "127.0.0.1:2003"
        .parse()
        .expect("could not parse back address");

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
        primary_address.to_string(),
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
        secondary_address.to_string(),
        None,
    )));
    worker.read_to_last();

    client.send();
    let response = client.receive();
    println!("response: {response:?}");

    worker.send_proxy_request_type(RequestType::RemoveBackend(RemoveBackend {
        cluster_id: "cluster_0".to_string(),
        backend_id: "cluster_0-0".to_string(),
        address: primary_address.to_string(),
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
    use sozu_command_lib::log;
    Logger::init(
        "KA-OUT".to_string(),
        "debug",
        LoggerBackend::Stdout(stdout()),
        None,
    );

    let front_address = "127.0.0.1:2001"
        .parse()
        .expect("could not parse front address");

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
        format!("client"),
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
    let front_address = "127.0.0.1:2001"
        .parse()
        .expect("could not parse front address");

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) =
        setup_sync_test("STICK", config, listeners, state, front_address, 2, true);

    let mut backend2 = backends.pop().unwrap();
    let mut backend1 = backends.pop().unwrap();
    let mut client = Client::new(
        format!("client"),
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
    assert!(response
        .unwrap()
        .starts_with("HTTP/1.1 200 OK\r\nContent-Length: 5\r\nSozu-Id:"));

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

#[serial]
#[test]
fn test_sync() {
    assert_eq!(try_sync(10, 100), State::Success);
}

#[serial]
#[test]
fn test_async() {
    assert_eq!(try_async(3, 10, 100), State::Success);
}

#[serial]
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

#[serial]
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
#[serial]
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
#[serial]
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
#[serial]
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

#[serial]
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

#[serial]
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

#[serial]
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

#[serial]
#[test]
fn test_http_behaviors() {
    assert_eq!(
        repeat_until_error_or(10, "HTTP stack", try_http_behaviors),
        State::Success
    );
}

#[serial]
#[test]
fn test_msg_close() {
    assert_eq!(
        repeat_until_error_or(100, "HTTP error on close", try_msg_close),
        State::Success
    );
}

#[serial]
#[test]
fn test_blue_green() {
    assert_eq!(
        repeat_until_error_or(10, "Blue green switch", try_blue_geen),
        State::Success
    );
}

#[serial]
#[test]
fn test_keep_alive() {
    assert_eq!(
        repeat_until_error_or(10, "Keep alive combinations", try_keep_alive),
        State::Success
    );
}

#[serial]
#[test]
fn test_stick() {
    assert_eq!(
        repeat_until_error_or(10, "Sticky session", try_stick),
        State::Success
    );
}
