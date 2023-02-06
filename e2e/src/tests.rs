use std::{
    io::stdin,
    net::SocketAddr,
    thread,
    time::{Duration, Instant},
};

use serial_test::serial;

use sozu_command_lib::{
    config::{Config, FileConfig},
    proxy::{ActivateListener, ListenerType, ProxyRequestOrder},
    scm_socket::Listeners,
    state::ConfigState,
};

use crate::{
    http_utils::{http_ok_response, http_request},
    mock::{
        aggregator::SimpleAggregator, async_backend::BackendHandle as AsyncBackend, client::Client,
        sync_backend::Backend as SyncBackend,
    },
    sozu::worker::Worker,
};

#[derive(PartialEq, Eq, Debug)]
pub enum State {
    Success,
    Fail,
    Undecided,
}

/// Setup a Sozu worker with
/// - `config`
/// - `listeners`
/// - 1 active HttpListener on `front_address`
/// - 1 cluster ("cluster_0")
/// - 1 HttpFrontend for "cluster_0" on `front_address`
/// - n backends ("cluster_0-{0..n}")
pub fn setup_test(
    config: Config,
    listeners: Listeners,
    state: ConfigState,
    front_address: SocketAddr,
    nb_backends: usize,
) -> (Worker, Vec<SocketAddr>) {
    let mut worker = Worker::start_new_worker("WORKER", config, &listeners, state);

    worker.send_proxy_request(ProxyRequestOrder::AddHttpListener(
        Worker::default_http_listener(front_address),
    ));
    worker.send_proxy_request(ProxyRequestOrder::ActivateListener(ActivateListener {
        address: front_address,
        proxy: ListenerType::HTTP,
        from_scm: false,
    }));
    worker.send_proxy_request(ProxyRequestOrder::AddCluster(Worker::default_cluster(
        "cluster_0",
    )));
    worker.send_proxy_request(ProxyRequestOrder::AddHttpFrontend(
        Worker::default_http_frontend("cluster_0", front_address),
    ));

    let mut backends = Vec::new();
    for i in 0..nb_backends {
        let back_address = format!("127.0.0.1:{}", 2002 + i)
            .parse()
            .expect("could not parse back address");
        worker.send_proxy_request(ProxyRequestOrder::AddBackend(Worker::default_backend(
            "cluster_0",
            format!("cluster_0-{i}"),
            back_address,
        )));
        backends.push(back_address);
    }

    worker.read_to_last();
    (worker, backends)
}

pub fn async_setup_test(
    config: Config,
    listeners: Listeners,
    state: ConfigState,
    front_address: SocketAddr,
    nb_backends: usize,
) -> (Worker, Vec<AsyncBackend<SimpleAggregator>>) {
    let (worker, backends) = setup_test(config, listeners, state, front_address, nb_backends);
    let backends = backends
        .into_iter()
        .enumerate()
        .map(|(i, back_address)| {
            let aggregator = SimpleAggregator {
                requests_received: 0,
                responses_sent: 0,
            };
            AsyncBackend::spawn_detached_backend(
                format!("BACKEND_{i}"),
                back_address,
                aggregator,
                AsyncBackend::http_handler(format!("pong{i}")),
            )
        })
        .collect::<Vec<_>>();
    (worker, backends)
}

pub fn sync_setup_test(
    config: Config,
    listeners: Listeners,
    state: ConfigState,
    front_address: SocketAddr,
    nb_backends: usize,
) -> (Worker, Vec<SyncBackend>) {
    let (worker, backends) = setup_test(config, listeners, state, front_address, nb_backends);
    let backends = backends
        .into_iter()
        .enumerate()
        .map(|(i, back_address)| {
            SyncBackend::new(
                format!("BACKEND_{i}"),
                back_address,
                http_ok_response(format!("pong{i}")),
            )
        })
        .collect::<Vec<_>>();
    (worker, backends)
}

pub fn try_async(nb_backends: usize, nb_clients: usize, nb_requests: usize) -> State {
    let front_address = "127.0.0.1:2001"
        .parse()
        .expect("could not parse front address");

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) =
        async_setup_test(config, listeners, state, front_address, nb_backends);

    let mut clients = (0..nb_clients)
        .map(|i| {
            Client::new(
                format!("client{i}"),
                front_address,
                http_request("GET", "/api", format!("ping{i}")),
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

    worker.send_proxy_request(ProxyRequestOrder::SoftStop);
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
    let (mut worker, mut backends) = sync_setup_test(config, listeners, state, front_address, 1);
    let mut backend = backends.pop().unwrap();

    backend.connect();

    let mut clients = (0..nb_clients)
        .map(|i| {
            Client::new(
                format!("client{i}"),
                front_address,
                http_request("GET", "/api", format!("ping{i}")),
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

    worker.send_proxy_request(ProxyRequestOrder::SoftStop);
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
    let (mut worker, mut backends) = async_setup_test(config, listeners, state, front_address, 2);
    let mut backend2 = backends.pop().expect("backend2");
    let mut backend1 = backends.pop().expect("backend1");

    let mut aggregator = Some(SimpleAggregator {
        requests_received: 0,
        responses_sent: 0,
    });

    let mut client = Client::new("client", front_address, http_request("GET", "/api", "ping"));
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

    worker.send_proxy_request(ProxyRequestOrder::SoftStop);
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
    let (mut worker, mut backends) = sync_setup_test(config, listeners, state, front_address, 1);
    let mut backend = backends.pop().unwrap();

    let mut client = Client::new("client", front_address, http_request("GET", "/api", "ping"));

    backend.connect();
    client.connect();
    client.send();
    backend.accept(0);
    backend.receive(0);
    backend.send(0);
    client.receive();

    worker.send_proxy_request(ProxyRequestOrder::SoftStop);
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
    let front_address = "127.0.0.1:2001"
        .parse()
        .expect("could not parse front address");
    let back_address = "127.0.0.1:2002"
        .to_string()
        .parse()
        .expect("could not parse back address");

    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker("WORKER", config, &listeners, state);

    worker.send_proxy_request(ProxyRequestOrder::AddTcpListener(
        Worker::default_tcp_listener(front_address),
    ));
    worker.send_proxy_request(ProxyRequestOrder::ActivateListener(ActivateListener {
        address: front_address,
        proxy: ListenerType::TCP,
        from_scm: false,
    }));
    worker.send_proxy_request(ProxyRequestOrder::AddCluster(Worker::default_cluster(
        "cluster_0",
    )));
    worker.send_proxy_request(ProxyRequestOrder::AddTcpFrontend(
        Worker::default_tcp_frontend("cluster_0", front_address),
    ));

    worker.send_proxy_request(ProxyRequestOrder::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
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

    worker.send_proxy_request(ProxyRequestOrder::SoftStop);
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

pub fn try_issue_810_panic_variant() -> State {
    let front_address = "127.0.0.1:2001"
        .parse()
        .expect("could not parse front address");

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = sync_setup_test(config, listeners, state, front_address, 1);

    let mut backend = backends.pop().expect("backend");
    let mut client = Client::new("client", front_address, http_request("GET", "/api", "ping"));

    backend.connect();
    client.connect();
    client.send();

    worker.send_proxy_request(ProxyRequestOrder::SoftStop);
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

pub fn test_upgrade() -> State {
    let front_address = "127.0.0.1:2001"
        .parse()
        .expect("could not parse front address");

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = sync_setup_test(config, listeners, state, front_address, 1);

    let mut backend = backends.pop().expect("backend");
    let mut client = Client::new("client", front_address, http_request("GET", "/api", "ping"));

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

    new_worker.send_proxy_request(ProxyRequestOrder::SoftStop);
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

pub fn test_http(nb_requests: usize) {
    let front_address = "127.0.0.1:2001"
        .parse()
        .expect("could not parse front address");

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = async_setup_test(config, listeners, state, front_address, 1);
    let mut backend = backends.pop().expect("backend");

    let mut bad_client = Client::new(
        "bad_client".to_string(),
        front_address,
        "GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\nContent-Length: 3\r\n\r\nbad_ping",
    );
    let mut good_client = Client::new(
        "good_client".to_string(),
        front_address,
        http_request("GET", "/api", "good_ping"),
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

    worker.send_proxy_request(ProxyRequestOrder::SoftStop);
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

pub fn try_hard_or_soft_stop(soft: bool) -> State {
    let front_address = "127.0.0.1:2001"
        .parse()
        .expect("could not parse front address");

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = sync_setup_test(config, listeners, state, front_address, 1);
    let mut backend = backends.pop().unwrap();

    let mut client = Client::new("client", front_address, http_request("GET", "/api", "ping"));

    // Send a request to try out
    backend.connect();
    client.connect();
    client.send();
    backend.accept(0);
    backend.receive(0);

    // stop sōzu
    if soft {
        // the worker will wait for backends to respond before shutting down
        worker.send_proxy_request(ProxyRequestOrder::SoftStop);
    } else {
        // the worker will shut down without waiting for backends to finish
        worker.send_proxy_request(ProxyRequestOrder::HardStop);
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

/*
pub fn test_hard_vs_soft_stop() -> State {
    let state = _test_hard_vs_soft_stop(true);
    if state != State::Success {
        return state;
    }
    _test_hard_vs_soft_stop(false)
}
*/

pub fn wait_input<S: Into<String>>(s: S) {
    println!("==================================================================");
    println!("{}", s.into());
    println!("==================================================================");
    let mut buf = String::new();
    stdin().read_line(&mut buf).expect("bad input");
}

pub fn repeat_until_error_or<F>(times: usize, test_description: &str, test: F) -> State
where
    F: Fn() -> State + Sized,
{
    println!("{test_description}");
    for i in 1..=times {
        let state = test();
        match state {
            State::Success => {}
            State::Fail => {
                println!("------------------------------------------------------------------");
                println!("Test not successful after: {i} iterations");
                return State::Fail;
            }
            State::Undecided => {
                println!("------------------------------------------------------------------");
                println!("Test interupted after: {i} iterations");
                return State::Undecided;
            }
        }
    }
    println!("------------------------------------------------------------------");
    println!("Test successful after: {times} iterations");
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
            1000,
            "issue 806: timeout with invalid back token\n(not fixed)",
            || try_backend_stop(2, None)
        ) != State::Fail
    );
}

// https://github.com/sozu-proxy/sozu/issues/808
#[serial]
#[test]
fn test_issue_808() {
    assert_eq!(
        repeat_until_error_or(
            1000,
            "issue 808: panic on successful zombie check\n(fixed)",
            || try_backend_stop(2, Some(1))
        ),
        // if Success, it means the session was never a zombie
        // if Fail, it means the zombie checker probably crashed
        State::Undecided
    );
}

// https://github.com/sozu-proxy/sozu/issues/810
#[serial]
#[test]
fn test_issue_810_timeout() {
    assert_eq!(
        repeat_until_error_or(
            1000,
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
            1000,
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
                1000,
                "issue 810: shutdown panics on tcp connection after proxy cleared its listeners\n(opinionated fix)",
                || try_issue_810_panic(false)
            ),
            State::Success
        );
}

#[serial]
#[test]
fn test_issue_810_panic_variant() {
    assert_eq!(
            repeat_until_error_or(
                2,
                "issue 810: shutdown panics on http connection accept after proxy cleared its listeners",
                try_issue_810_panic_variant
            ),
            State::Success
        );
}
