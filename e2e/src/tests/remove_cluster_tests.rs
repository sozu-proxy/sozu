//! End-to-end tests for `RemoveCluster` on a live worker.
//!
//! Removing a cluster removes, in the same order, the frontends and backends
//! that name it (`Server::notify_proxys`, `lib/src/server.rs`): a request for
//! its hostname stops routing, a new connection on its TCP listener is closed
//! instead of forwarded, and a session established before the removal drains
//! on the backend connection it already holds.

use std::{
    io::{ErrorKind, Read},
    net::{SocketAddr, TcpStream},
    time::Duration,
};

use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{ActivateListener, ListenerType, request::RequestType},
};

use crate::{
    http_utils::http_request,
    mock::{client::Client, sync_backend::Backend as SyncBackend},
    sozu::worker::Worker,
    tests::{State, repeat_until_error_or, setup_sync_test},
};

use super::tests::create_local_address;

/// How long a client waits for the answer of a request or for its connection
/// to close.
const ANSWER_BUDGET: Duration = Duration::from_secs(1);

fn stop(worker: Worker) -> bool {
    let mut worker = worker;
    worker.soft_stop();
    worker.wait_for_server_stop()
}

// =========================================================================
// HTTP: the removed cluster's hostname stops routing
// =========================================================================

fn try_remove_cluster_stops_http_routing() -> State {
    let front_address = create_local_address();
    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "REMOVE-CLUSTER-HTTP",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );
    let mut backend = backends.pop().expect("setup_sync_test returns one backend");
    backend.connect();

    // Premise: before the removal, the hostname routes to the backend, and
    // the client keeps its connection alive.
    let mut before = Client::new(
        "BEFORE",
        front_address,
        http_request("GET", "/api", "ping", "localhost"),
    );
    before.connect();
    before.send();
    backend.accept(0);
    backend.receive(0);
    backend.send(0);
    match before.receive_response(ANSWER_BUDGET) {
        Some(response) if response.starts_with("HTTP/1.1 200") => {}
        other => {
            println!("the premise failed, the backend did not answer: {other:?}");
            stop(worker);
            return State::Undecided;
        }
    }

    worker.send_proxy_request_type(RequestType::RemoveCluster("cluster_0".to_owned()));
    worker.read_to_last();

    // A new connection: nothing routes the hostname any more.
    let mut after = Client::new(
        "AFTER",
        front_address,
        http_request("GET", "/api", "ping", "localhost"),
    );
    after.connect();
    after.send();
    let fresh = after.receive_until_eof(ANSWER_BUDGET);
    println!("new connection after RemoveCluster: {fresh:?}");

    // The connection kept alive across the removal routes its next request
    // afresh, and is answered the same way instead of reaching the backend.
    before.send();
    let kept_alive = before.receive_until_eof(ANSWER_BUDGET);
    println!("kept-alive connection after RemoveCluster: {kept_alive:?}");

    let stopped = stop(worker);
    let not_found = |answer: &Option<String>| {
        answer
            .as_deref()
            .is_some_and(|a| a.starts_with("HTTP/1.1 404"))
    };
    if not_found(&fresh) && not_found(&kept_alive) && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_remove_cluster_stops_http_routing() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "RemoveCluster: the removed cluster's hostname answers 404 on new and kept-alive connections",
            try_remove_cluster_stops_http_routing,
        ),
        State::Success,
    );
}

// =========================================================================
// TCP: the listener stops routing, an established session drains
// =========================================================================

/// Read from `stream` until it closes or `ANSWER_BUDGET` elapses. `true` when
/// the proxy closed it (EOF or reset) without sending a byte.
fn closed_without_data(stream: &mut TcpStream) -> bool {
    stream
        .set_read_timeout(Some(ANSWER_BUDGET))
        .expect("could not set a read timeout");
    let mut buf = [0u8; 64];
    match stream.read(&mut buf) {
        Ok(0) => true,
        Ok(n) => {
            println!("the proxy sent {n} byte(s) instead of closing");
            false
        }
        Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
            println!("the connection is still open after {ANSWER_BUDGET:?}");
            false
        }
        Err(error) => {
            println!("the connection broke: {error}");
            true
        }
    }
}

fn try_remove_cluster_stops_tcp_routing() -> State {
    let front_address: SocketAddr = create_local_address();
    let back_address: SocketAddr = create_local_address();
    let (config, listeners, state) = Worker::empty_tcp_config(front_address);
    let mut worker = Worker::start_new_worker_owned("REMOVE-CLUSTER-TCP", config, listeners, state);

    worker.send_proxy_request_type(RequestType::AddTcpListener(
        ListenerBuilder::new_tcp(front_address.into())
            .to_tcp(None)
            .expect("could not build the TCP listener config"),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        interface: None,
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

    let mut backend = SyncBackend::new("BACKEND", back_address, "pong");
    backend.connect();

    // Premise: an established session forwards bytes both ways.
    let mut established = Client::new("ESTABLISHED", front_address, "ping");
    established.connect();
    established.send();
    backend.accept(0);
    let premise = backend.receive(0).as_deref() == Some("ping") && {
        backend.send(0);
        established.receive().as_deref() == Some("pong")
    };
    if !premise {
        println!("the premise failed, the session did not forward");
        stop(worker);
        return State::Undecided;
    }

    worker.send_proxy_request_type(RequestType::RemoveCluster("cluster_0".to_owned()));
    worker.read_to_last();

    // The session established before the removal drains on the backend
    // connection it already holds.
    established.send();
    let drained = backend.receive(0).as_deref() == Some("ping") && {
        backend.send(0);
        established.receive().as_deref() == Some("pong")
    };
    println!("established session after RemoveCluster drained: {drained}");

    // A new connection on the listener is no longer forwarded.
    let mut fresh = TcpStream::connect(front_address).expect("could not connect to the listener");
    let refused = closed_without_data(&mut fresh);
    println!("new connection after RemoveCluster closed: {refused}");

    let stopped = stop(worker);
    if drained && refused && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_remove_cluster_stops_tcp_routing() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "RemoveCluster: the TCP listener closes new connections, an established session drains",
            try_remove_cluster_stops_tcp_routing,
        ),
        State::Success,
    );
}
