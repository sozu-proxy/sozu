//! End-to-end tests for `CloseSessions` on a live worker.
//!
//! The worker selects the sessions holding a backend connection to the
//! cluster, or to one of its backends, and orders each to close the way a
//! soft stop does (`Server::close_sessions`, `lib/src/server.rs`): an idle
//! HTTP session and a TCP session close at once, an HTTP session with a
//! request in flight answers it with `Connection: close` first. The reply
//! counts both, and a session the filter does not select is left alone.

use std::{
    io::{ErrorKind, Read},
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, CloseSessions, ClosedSessions, ListenerType, ResponseStatus,
        request::RequestType, response_content::ContentType,
    },
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

/// Send `CloseSessions` and return the worker's `ClosedSessions`, or `None`
/// when it failed or answered something else.
fn close_sessions(
    worker: &mut Worker,
    backend_id: Option<&str>,
    address: Option<SocketAddr>,
) -> Option<ClosedSessions> {
    worker.send_proxy_request_type(RequestType::CloseSessions(CloseSessions {
        cluster_id: "cluster_0".to_owned(),
        backend_id: backend_id.map(ToOwned::to_owned),
        address: address.map(Into::into),
    }));
    let last = worker.command_id.last.to_owned();
    loop {
        let response = worker.read_proxy_response()?;
        if response.id != last {
            continue;
        }
        if response.status != ResponseStatus::Ok as i32 {
            return None;
        }
        return match response.content?.content_type? {
            ContentType::ClosedSessions(closed) => Some(closed),
            _ => None,
        };
    }
}

/// Read from `stream` until the proxy closes it or `ANSWER_BUDGET` elapses.
/// `true` when the proxy closed it (EOF or reset) without sending a byte.
fn closed_by_proxy(stream: &mut TcpStream) -> bool {
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .expect("could not set a read timeout");
    let started = Instant::now();
    let mut buf = [0u8; 64];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return true,
            Ok(n) => {
                println!("the proxy sent {n} byte(s) instead of closing");
                return false;
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if started.elapsed() >= ANSWER_BUDGET {
                    println!("the connection is still open after {ANSWER_BUDGET:?}");
                    return false;
                }
            }
            Err(error) => {
                println!("the connection broke: {error}");
                return true;
            }
        }
    }
}

/// One request answered on `client`, through `backend`'s first connection.
fn exchange(client: &mut Client, backend: &mut SyncBackend) -> bool {
    client.send();
    backend.receive(0);
    backend.send(0);
    client
        .receive_response(ANSWER_BUDGET)
        .is_some_and(|response| response.starts_with("HTTP/1.1 200"))
}

// =========================================================================
// HTTP: an idle keep-alive session closes at once, unless filtered out
// =========================================================================

fn try_close_idle_http_session() -> State {
    let front_address = create_local_address();
    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "CLOSE-SESSIONS-IDLE",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );
    let mut backend = backends.pop().expect("setup_sync_test returns one backend");
    let back_address = backend.address;
    backend.connect();

    let mut client = Client::new(
        "IDLE",
        front_address,
        http_request("GET", "/api", "ping", "localhost"),
    );
    client.connect();
    client.send();
    backend.accept(0);
    backend.receive(0);
    backend.send(0);
    if !client
        .receive_response(ANSWER_BUDGET)
        .is_some_and(|response| response.starts_with("HTTP/1.1 200"))
    {
        println!("the premise failed, the backend did not answer");
        stop(worker);
        return State::Undecided;
    }

    // Another backend of the cluster, and another address: neither selects
    // the session, which keeps serving requests.
    let other_backend = close_sessions(&mut worker, Some("cluster_0-1"), None);
    let other_address = close_sessions(&mut worker, None, Some(create_local_address()));
    let spared = exchange(&mut client, &mut backend);
    println!(
        "other backend: {other_backend:?}, other address: {other_address:?}, spared: {spared}"
    );

    // Its own backend, named by id and address together, selects it.
    let own = close_sessions(&mut worker, Some("cluster_0-0"), Some(back_address));
    let stream = client.stream.as_mut().expect("the client is connected");
    let closed = closed_by_proxy(stream);
    println!("own backend: {own:?}, closed: {closed}");

    let stopped = stop(worker);
    let none = Some(ClosedSessions {
        closed: 0,
        draining: 0,
    });
    let one_closed = Some(ClosedSessions {
        closed: 1,
        draining: 0,
    });
    if other_backend == none
        && other_address == none
        && spared
        && own == one_closed
        && closed
        && stopped
    {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_close_sessions_closes_an_idle_http_session() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "CloseSessions: an idle keep-alive session of the backend closes at once, \
             a filter naming another backend spares it",
            try_close_idle_http_session,
        ),
        State::Success,
    );
}

// =========================================================================
// HTTP: a request in flight is answered with `Connection: close` first
// =========================================================================

fn try_close_draining_http_session() -> State {
    let front_address = create_local_address();
    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "CLOSE-SESSIONS-DRAIN",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );
    let mut backend = backends.pop().expect("setup_sync_test returns one backend");
    backend.connect();

    let mut client = Client::new(
        "IN-FLIGHT",
        front_address,
        http_request("GET", "/api", "ping", "localhost"),
    );
    client.connect();
    client.send();
    backend.accept(0);
    if backend.receive(0).is_none() {
        println!("the premise failed, the request did not reach the backend");
        stop(worker);
        return State::Undecided;
    }

    // The backend has not answered yet: the session drains.
    let ordered = close_sessions(&mut worker, None, None);
    backend.send(0);
    let response = client.receive_response(ANSWER_BUDGET);
    let stream = client.stream.as_mut().expect("the client is connected");
    let closed = closed_by_proxy(stream);
    println!("ordered: {ordered:?}, response: {response:?}, closed: {closed}");

    let stopped = stop(worker);
    let answered_with_close = response.as_deref().is_some_and(|response| {
        response.starts_with("HTTP/1.1 200")
            && response
                .to_ascii_lowercase()
                .contains("\r\nconnection: close\r\n")
    });
    if ordered
        == Some(ClosedSessions {
            closed: 0,
            draining: 1,
        })
        && answered_with_close
        && closed
        && stopped
    {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_close_sessions_drains_an_http_request_in_flight() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "CloseSessions: a request in flight is answered with Connection: close, \
             then the session closes",
            try_close_draining_http_session,
        ),
        State::Success,
    );
}

// =========================================================================
// TCP: an established session closes at once
// =========================================================================

fn try_close_tcp_session() -> State {
    let front_address: SocketAddr = create_local_address();
    let back_address: SocketAddr = create_local_address();
    let (config, listeners, state) = Worker::empty_tcp_config(front_address);
    let mut worker = Worker::start_new_worker_owned("CLOSE-SESSIONS-TCP", config, listeners, state);

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

    let mut client = Client::new("ESTABLISHED", front_address, "ping");
    client.connect();
    client.send();
    backend.accept(0);
    let premise = backend.receive(0).as_deref() == Some("ping") && {
        backend.send(0);
        client.receive().as_deref() == Some("pong")
    };
    if !premise {
        println!("the premise failed, the session did not forward");
        stop(worker);
        return State::Undecided;
    }

    let ordered = close_sessions(&mut worker, None, Some(back_address));
    let stream = client.stream.as_mut().expect("the client is connected");
    let closed = closed_by_proxy(stream);
    println!("ordered: {ordered:?}, closed: {closed}");

    let stopped = stop(worker);
    if ordered
        == Some(ClosedSessions {
            closed: 1,
            draining: 0,
        })
        && closed
        && stopped
    {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_close_sessions_closes_a_tcp_session() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "CloseSessions: an established TCP session of the backend closes at once",
            try_close_tcp_session,
        ),
        State::Success,
    );
}
