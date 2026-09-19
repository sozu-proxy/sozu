//! End-to-end coverage for the PROXY-protocol v2 `LOCAL` command.
//!
//! The v2 wire format does NOT tie `LOCAL` (`0x20`) to the `AF_UNSPEC`
//! family — only HAProxy's own emitter pairs the two. A crafted peer may send
//! `LOCAL` with family `0x11` and a fully populated 12-byte `AF_INET` block,
//! and `parse_addr_v2` switches on the family nibble alone, so before the fix
//! that forged block reached every consumer of `HeaderV2::addr`. All of them
//! attribute `ProxyAddr::source()` to the client, so a peer able to reach a
//! listener with `expect_proxy = true` could forge the address Sōzu records in
//! its access logs, injects as `X-Real-IP`, and counts against
//! `max_connections_per_ip` — and so evade that cap by picking a fresh address
//! per connection.
//!
//! `parse_v2_header` now yields `ProxyAddr::AfUnspec` for every `LOCAL`
//! header, and the six consumers do not resolve `AfUnspec` alike. The two
//! outcomes below are both operator-visible:
//!
//! * a TCP session falls back to the front socket's `peer_addr`, so it
//!   proceeds attributed to the REAL peer — pinned here through the
//!   per-(cluster, source-IP) cap, the one place that attribution is
//!   observable from outside the process;
//! * `HttpSession::upgrade_expect` needs both a source and a destination,
//!   `AfUnspec` supplies neither, so the refused upgrade becomes
//!   `SessionIsToBeClosed` and the session is closed at the expect stage,
//!   before any request is read.
//!
//! Both tests carry a `PROXY` control with the SAME address block, so a green
//! run proves the fix secures PROXY support rather than disabling it.
//!
//! The TCP worker setup mirrors `cluster_ip_limit_tests::try_tcp_graceful_close_on_limit`
//! and the HTTP one `tests::setup_x_real_ip_test`.
//!
//! ## Test list
//! 1. [`test_ppv2_local_cannot_forge_the_tcp_source_address`] — forged `LOCAL`
//!    source IPs all collapse onto `127.0.0.1`, so the per-IP cap still bites
//! 2. [`test_ppv2_local_closes_an_expect_proxy_http_session`] — a forged
//!    `LOCAL` closes the HTTP session; the same block under `PROXY` is honoured

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

use sozu_command_lib::{
    config::{FileConfig, ListenerBuilder},
    proto::command::{
        ActivateListener, Cluster, ListenerType, ProxyProtocolConfig, RequestTcpFrontend,
        request::RequestType,
    },
    scm_socket::Listeners,
    state::ConfigState,
};
use sozu_lib::protocol::proxy_protocol::header::{Command, HeaderV2};

use crate::{
    http_utils::http_ok_response,
    mock::sync_backend::Backend as SyncBackend,
    port_registry::{attach_reserved_http_listener, attach_reserved_tcp_listener},
    sozu::worker::Worker,
    tests::{State, repeat_until_error_or},
};

use super::tests::create_local_address;

/// The forged address block: a routable-looking source the real peer
/// (`127.0.0.1`) does not own. `HeaderV2::new` derives the family from the
/// addresses regardless of the command, so `Command::Local` here serialises to
/// exactly the shape the fix is about — `0x20` (v2 + LOCAL) with family `0x11`
/// (AF_INET over STREAM) and a populated 12-byte block.
fn forged_header(command: Command, source: &str, front_address: SocketAddr) -> Vec<u8> {
    let source: SocketAddr = source.parse().expect("could not parse the forged source");
    HeaderV2::new(command, source, front_address).into_bytes()
}

/// Open a raw connection, write `header`, then `payload`, and return the
/// stream so the caller can read the answer (or observe the close).
fn connect_with_header(
    front_address: SocketAddr,
    header: &[u8],
    payload: &[u8],
) -> Option<TcpStream> {
    let stream = TcpStream::connect(front_address).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("could not set read timeout");
    stream
        .set_write_timeout(Some(Duration::from_millis(500)))
        .expect("could not set write timeout");
    let mut stream = stream;
    stream.write_all(header).ok()?;
    stream.write_all(payload).ok()?;
    Some(stream)
}

/// Read whatever is on `stream` before its 500 ms timeout. An empty result
/// means the peer closed without sending anything.
fn read_answer(stream: &mut TcpStream) -> Vec<u8> {
    let mut answer = Vec::new();
    let mut buf = [0u8; 4096];
    for _ in 0..4 {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                answer.extend_from_slice(&buf[..n]);
                break;
            }
            Err(_) => continue,
        }
    }
    answer
}

/// Poll `backend.accept(client_id)` until it succeeds or `deadline` elapses.
/// Each underlying `accept()` already blocks up to 100 ms on the listener's
/// `SO_RCVTIMEO`, so this is a deadline-bounded wait rather than a spin, and
/// it returns as soon as a connection lands — the shape `doc/testing.md` §7
/// asks for instead of a fixed `sleep`. Mirrors
/// `h1_security_tests::assert_attack_not_forwarded`.
fn accept_within(backend: &mut SyncBackend, client_id: usize, deadline: Duration) -> bool {
    let started = Instant::now();
    loop {
        if backend.accept(client_id) {
            return true;
        }
        if started.elapsed() >= deadline {
            return false;
        }
    }
}

// =========================================================================
// Test 1: a forged LOCAL header cannot forge the TCP source address
//
// `max_connections_per_ip = 1` on the cluster. Two connections, each carrying
// a forged `LOCAL` header announcing a DIFFERENT source. The TCP gate in
// `TcpSession::connect_to_backend` keys on `effective_session_address`, which
// folds the parsed PROXY-v2 source over the raw `peer_addr` — so with the
// block discarded both connections account to `127.0.0.1` and the second is
// refused with a graceful FIN, exactly like a plain-TCP repeat offender.
// =========================================================================

/// To SEE THIS RED: in `lib/src/protocol/proxy_protocol/parser.rs`, make
/// `parse_v2_header`'s `Command::Local` arm keep the parsed block instead of
/// yielding `ProxyAddr::AfUnspec` (the pre-fix behaviour was a single
/// `HeaderV2 { command, family, addr }` with no `match` on `command`). Each
/// connection then accounts to its own forged IP, the cap never bites, and
/// `second_refused` is false.
fn try_ppv2_local_cannot_forge_the_tcp_source_address() -> State {
    let front_address = create_local_address();
    let back_address = create_local_address();

    let mut config = Worker::into_config(FileConfig::default());
    config.max_connections_per_ip = Some(1);
    let mut listeners = Listeners::default();
    attach_reserved_tcp_listener(&mut listeners, front_address);
    let mut worker =
        Worker::start_new_worker_owned("PPV2-LOCAL-TCP", config, listeners, ConfigState::new());

    worker.send_proxy_request_type(RequestType::AddTcpListener(
        ListenerBuilder::new_tcp(front_address.into())
            .to_tcp(None)
            .expect("could not build the tcp listener config"),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.into(),
        proxy: ListenerType::Tcp.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(Cluster {
        cluster_id: "ppv2_local_tcp".to_owned(),
        proxy_protocol: Some(ProxyProtocolConfig::ExpectHeader as i32),
        max_connections_per_ip: Some(1),
        ..Default::default()
    }));
    worker.send_proxy_request_type(RequestType::AddTcpFrontend(RequestTcpFrontend {
        cluster_id: "ppv2_local_tcp".to_owned(),
        address: front_address.into(),
        ..Default::default()
    }));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "ppv2_local_tcp",
        "ppv2_local_tcp-0",
        back_address,
        None,
    )));
    worker.read_to_last();

    let mut backend = SyncBackend::new("PPV2-BACK", back_address, "PONG\n".to_owned());
    backend.connect();

    // First connection: forged LOCAL announcing 10.0.0.1. It takes the single
    // slot the cap allows — for `127.0.0.1`, never for `10.0.0.1`.
    let first = connect_with_header(
        front_address,
        &forged_header(Command::Local, "10.0.0.1:1111", front_address),
        b"hello-a",
    );
    let Some(mut first) = first else {
        println!("PPV2-LOCAL-TCP: could not open the first connection");
        return State::Fail;
    };
    backend.accept(0);
    backend.receive(0);
    backend.send(0);
    let first_answer = read_answer(&mut first);
    let first_served = first_answer.starts_with(b"PONG");
    println!("PPV2-LOCAL-TCP: first connection served = {first_served}");

    // Second connection: a DIFFERENT forged source. Discarding the block makes
    // it the same `127.0.0.1` as the first, so the cap refuses it with a FIN
    // before the backend is ever dialed.
    let second = connect_with_header(
        front_address,
        &forged_header(Command::Local, "10.0.0.2:2222", front_address),
        b"hello-b",
    );
    let Some(mut second) = second else {
        println!("PPV2-LOCAL-TCP: could not open the second connection");
        return State::Fail;
    };
    let reached_backend = backend.accept(1);
    let second_answer = read_answer(&mut second);
    let second_refused = !reached_backend && second_answer.is_empty();
    println!(
        "PPV2-LOCAL-TCP: second connection reached_backend={reached_backend} answer={} bytes",
        second_answer.len()
    );

    drop(first);
    drop(second);
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    if first_served && second_refused && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_ppv2_local_cannot_forge_the_tcp_source_address() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "PROXY-v2 LOCAL: a forged address block cannot re-attribute a TCP session",
            try_ppv2_local_cannot_forge_the_tcp_source_address,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 2: a forged LOCAL header closes an `expect_proxy` HTTP session
//
// `HttpSession::upgrade_expect` needs a source AND a destination; `AfUnspec`
// supplies neither, so the upgrade is refused and the session closes before a
// request is read. The `PROXY` control carries the SAME block and must still
// be honoured — the listener injects it as `X-Real-IP`, which is precisely
// what a `LOCAL` header must NOT be able to forge.
// =========================================================================

/// To SEE THIS RED: in `lib/src/protocol/proxy_protocol/parser.rs`, make
/// `parse_v2_header`'s `Command::Local` arm keep the parsed block instead of
/// yielding `ProxyAddr::AfUnspec`. The forged session then upgrades, reaches
/// the backend carrying `X-Real-IP: 10.0.0.42`, and both `local_refused` and
/// `local_never_reached_backend` turn false.
fn try_ppv2_local_closes_an_expect_proxy_http_session() -> State {
    let front_address = create_local_address();
    let back_address = create_local_address();

    let (config, mut listeners, state) = Worker::empty_config();
    attach_reserved_http_listener(&mut listeners, front_address);
    let mut worker = Worker::start_new_worker_owned("PPV2-LOCAL-HTTP", config, listeners, state);

    let http_listener = {
        let mut builder = ListenerBuilder::new_http(front_address.into());
        builder.with_expect_proxy(true);
        builder.with_send_x_real_ip(true);
        builder
            .to_http(None)
            .expect("could not build the http listener config")
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

    let mut backend = SyncBackend::new("BACKEND_0", back_address, http_ok_response("pong"));
    backend.connect();

    let request = "GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";

    // The forged LOCAL session must be closed at the expect stage.
    let forged = connect_with_header(
        front_address,
        &forged_header(Command::Local, "10.0.0.42:12345", front_address),
        request.as_bytes(),
    );
    let Some(mut forged) = forged else {
        println!("PPV2-LOCAL-HTTP: could not open the forged connection");
        return State::Fail;
    };
    let local_reached_backend = accept_within(&mut backend, 0, Duration::from_millis(500));
    // Serve the forged session if it DID get through. Without this the client
    // reads nothing either way — once because the session was closed at the
    // expect stage, once merely because the test left the backend silent — and
    // `local_refused` would assert nothing. Answering makes it load-bearing:
    // with the block honoured, the client reads a 200.
    if local_reached_backend {
        backend.receive(0);
        backend.send(0);
    }
    let local_never_reached_backend = !local_reached_backend;
    let local_answer = read_answer(&mut forged);
    let local_refused = local_answer.is_empty();
    println!(
        "PPV2-LOCAL-HTTP: LOCAL reached_backend={local_reached_backend} answer={:?}",
        String::from_utf8_lossy(&local_answer)
    );

    // Control: the SAME block under `PROXY` is still honoured end to end.
    let honoured = connect_with_header(
        front_address,
        &forged_header(Command::Proxy, "10.0.0.42:12345", front_address),
        request.as_bytes(),
    );
    let Some(mut honoured) = honoured else {
        println!("PPV2-LOCAL-HTTP: could not open the PROXY control connection");
        return State::Fail;
    };
    let proxy_reached_backend = accept_within(&mut backend, 1, Duration::from_secs(2));
    let backend_saw = backend.receive(1).unwrap_or_default();
    backend.send(1);
    let proxy_answer = read_answer(&mut honoured);
    let proxy_served = proxy_answer.starts_with(b"HTTP/1.1 200");
    let proxy_attributed = backend_saw.to_lowercase().contains("x-real-ip: 10.0.0.42");
    println!(
        "PPV2-LOCAL-HTTP: PROXY reached_backend={proxy_reached_backend} served={proxy_served} attributed={proxy_attributed}"
    );

    drop(forged);
    drop(honoured);
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    if local_refused
        && local_never_reached_backend
        && proxy_reached_backend
        && proxy_served
        && proxy_attributed
        && stopped
    {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_ppv2_local_closes_an_expect_proxy_http_session() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "PROXY-v2 LOCAL: a forged address block closes an expect_proxy HTTP session while PROXY still works",
            try_ppv2_local_closes_an_expect_proxy_http_session,
        ),
        State::Success,
    );
}
