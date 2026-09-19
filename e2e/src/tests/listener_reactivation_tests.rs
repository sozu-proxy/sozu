//! End-to-end tests for a listener's slab-slot lifetime — the
//! `add-listener` -> `deactivate-listener` -> `activate-listener` ->
//! `remove-listener` cycle driven over the command channel against a real
//! worker and real sockets.
//!
//! `Server::ready` dispatches an event only when the session slab still
//! contains its token (`lib/src/server.rs`, the `slab.contains` gate at the
//! top of `ready`). A listener keeps ONE slab slot for its whole lifetime:
//! `deactivate-listener` re-arms the inert `ListenSession` placeholder
//! (`Server::reserve_listen_token`) instead of freeing the slot, and
//! `remove-listener` is the sole release point.
//!
//! Freeing the slot on deactivate made a REACTIVATED listener deaf: the
//! `accept()` that `activate-listener` performs inline drained the backlog,
//! and every later readiness event was dropped because the retained listen
//! token indexed a vacant slab key — while `ActivateListener` had answered
//! `ok`. UDP failed harder: the activation path could not install its
//! `UdpListenerSession` behind the `slab.contains` guard, so the socket was
//! registered with no session behind it and every datagram was dropped.
//!
//! The reactivation tests below therefore send their traffic AFTER the
//! `activate-listener` response has been read, so no backlog can mask a deaf
//! listener. The unit test in `lib/src/server.rs` covers TCP only; these
//! cover all four protocols on real sockets.
//!
//! ## Test list
//! 1. [`test_http_listener_serves_after_reactivation`]  — HTTP deactivate → activate → traffic
//! 2. [`test_https_listener_serves_after_reactivation`] — HTTPS deactivate → activate → traffic
//! 3. [`test_tcp_listener_serves_after_reactivation`]   — TCP deactivate → activate → traffic
//! 4. [`test_udp_listener_serves_after_reactivation`]   — UDP deactivate → activate → traffic
//! 5. [`test_udp_add_remove_cycles_do_not_leak_a_listener`] — three add/remove cycles, no
//!    deactivate: each cycle serves, and each `remove-listener` really stops the datagrams

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    time::Duration,
};

use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, AddCertificate, CertificateAndKey, DeactivateListener, ListenerType,
        RemoveListener, RequestHttpFrontend, ResponseStatus, SocketAddress, request::RequestType,
    },
};

use crate::{
    mock::{
        aggregator::SimpleAggregator,
        async_backend::BackendHandle as AsyncBackend,
        client::Client,
        https_client::{build_https_client, resolve_request},
        udp_backend::UdpBackend,
        udp_client::UdpClient,
    },
    sozu::worker::Worker,
    tests::{State, repeat_until_error_or, setup_async_test},
};

use super::tests::create_local_address;

/// Generous deadline for one round trip on a quiet local loopback, matching
/// `udp_tests::RT`.
const ROUND_TRIP: Duration = Duration::from_millis(1500);

// =========================================================================
// Helpers
// =========================================================================

/// Send `deactivate-listener` then `activate-listener` for `address` and read
/// both worker responses. Returns `true` when both answered `Ok`.
///
/// `to_scm` / `from_scm` are both false: the deactivated socket is dropped
/// rather than handed to a supervisor, and the reactivation binds a fresh one
/// (`server_bind` sets `SO_REUSEADDR` + `SO_REUSEPORT`). The retained listen
/// token is what must survive, not the file descriptor.
fn cycle_listener(worker: &mut Worker, address: &SocketAddress, proxy: ListenerType) -> bool {
    worker.send_proxy_request_type(RequestType::DeactivateListener(DeactivateListener {
        address: address.clone(),
        proxy: proxy.into(),
        to_scm: false,
    }));
    let deactivated = worker
        .read_proxy_response()
        .map(|response| response.status == ResponseStatus::Ok as i32)
        .unwrap_or(false);

    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: address.clone(),
        proxy: proxy.into(),
        from_scm: false,
    }));
    let activated = worker
        .read_proxy_response()
        .map(|response| response.status == ResponseStatus::Ok as i32)
        .unwrap_or(false);

    println!("{proxy:?} cycle: deactivate ok={deactivated}, reactivate ok={activated}");
    deactivated && activated
}

/// One HTTP/1.1 round trip on a BRAND NEW connection. `Connection: close`
/// makes the backend's answer end on EOF, so the read loop terminates on the
/// response rather than on the deadline.
fn http_round_trip(front_address: SocketAddr, label: &str) -> Option<String> {
    let mut client = Client::new(
        format!("HTTP-{label}"),
        front_address,
        "GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    client.connect();
    client.send();
    let response = client.receive_until_eof(Duration::from_secs(3));
    println!("HTTP-{label}: response = {response:?}");
    response
}

/// One raw TCP round trip on a BRAND NEW connection: write `payload`, read
/// whatever the backend echoes back before the deadline.
fn tcp_round_trip(front_address: SocketAddr, payload: &[u8], label: &str) -> Option<Vec<u8>> {
    let mut stream = match TcpStream::connect(front_address) {
        Ok(stream) => stream,
        Err(error) => {
            println!("TCP-{label}: could not connect: {error}");
            return None;
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("could not set read timeout");
    stream
        .set_write_timeout(Some(Duration::from_millis(500)))
        .expect("could not set write timeout");
    if let Err(error) = stream.write_all(payload) {
        println!("TCP-{label}: could not send: {error}");
        return None;
    }

    // Six 500 ms reads: sozu may need several event-loop ticks to route a
    // freshly accepted session to its backend.
    let mut received = Vec::new();
    for _ in 0..6 {
        let mut buf = [0u8; 256];
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                received.extend_from_slice(&buf[..n]);
                break;
            }
            Err(_) => continue,
        }
    }
    println!("TCP-{label}: received {} bytes", received.len());
    if received.is_empty() {
        None
    } else {
        Some(received)
    }
}

/// Boot a worker with one activated UDP listener + cluster + frontend +
/// backend address, exactly like `udp_tests::setup_udp_test` but returning the
/// `SocketAddress` the lifecycle requests need.
fn setup_udp_reactivation_worker(name: &str) -> (Worker, SocketAddr, SocketAddr) {
    let front_address = create_local_address();
    let back_address = create_local_address();
    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker_owned(name, config, listeners, state);

    worker.send_proxy_request_type(RequestType::AddUdpListener(
        ListenerBuilder::new_udp(front_address.into())
            .to_udp(None)
            .expect("could not build udp listener config"),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.into(),
        proxy: ListenerType::Udp.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "udp_cluster_0",
    )));
    worker.send_proxy_request_type(RequestType::AddUdpFrontend(
        sozu_command_lib::proto::command::RequestUdpFrontend {
            cluster_id: "udp_cluster_0".to_owned(),
            address: front_address.into(),
            tags: Default::default(),
        },
    ));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "udp_cluster_0",
        "udp_cluster_0-0",
        back_address,
        None,
    )));
    worker.read_to_last();

    (worker, front_address, back_address)
}

/// Strip the `{name}:` reply tag the mock UDP backend prepends, mirroring
/// `udp_tests::strip_reply_tag`.
fn udp_payload(reply: &[u8]) -> Option<Vec<u8>> {
    let pos = reply.iter().position(|&b| b == b':')?;
    Some(reply[pos + 1..].to_vec())
}

// =========================================================================
// Test 1: an HTTP listener still serves after deactivate -> activate
// =========================================================================

/// To SEE THIS RED: in `lib/src/server.rs`, in `notify_deactivate_listener`'s
/// HTTP arm, replace `self.reserve_listen_token(token, Protocol::HTTPListen);`
/// with `self.sessions.borrow_mut().slab.remove(token.0);` — the reactivated
/// listener's readiness event is then dropped by `Server::ready`, it accepts
/// nothing, and `after` is `None`.
fn try_http_listener_serves_after_reactivation() -> State {
    let front_address = create_local_address();
    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_async_test(
        "HTTP-REACTIVATE",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );
    let listener_address: SocketAddress = front_address.into();

    let before = http_round_trip(front_address, "before");
    let served_before = before
        .as_deref()
        .map(|response| response.starts_with("HTTP/1.1 200"))
        .unwrap_or(false);

    let cycled = cycle_listener(&mut worker, &listener_address, ListenerType::Http);

    let after = http_round_trip(front_address, "after");
    let served_after = after
        .as_deref()
        .map(|response| response.starts_with("HTTP/1.1 200"))
        .unwrap_or(false);

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    for backend in backends.iter_mut() {
        backend.stop_and_get_aggregator();
    }

    println!(
        "HTTP reactivation: before={served_before} cycled={cycled} after={served_after} stopped={stopped}"
    );
    if served_before && cycled && served_after && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_http_listener_serves_after_reactivation() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "listener lifecycle: an HTTP listener still serves traffic after deactivate → activate",
            try_http_listener_serves_after_reactivation,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 2: an HTTPS listener still serves after deactivate -> activate
// =========================================================================

/// To SEE THIS RED: in `lib/src/server.rs`, in `notify_deactivate_listener`'s
/// HTTPS arm, replace `self.reserve_listen_token(token, Protocol::HTTPSListen);`
/// with `self.sessions.borrow_mut().slab.remove(token.0);` — the reactivated
/// listener's readiness event is then dropped by `Server::ready`, it accepts
/// nothing, and `after` is `None`.
fn try_https_listener_serves_after_reactivation() -> State {
    let front_address = create_local_address();
    let back_address = create_local_address();
    let listener_address: SocketAddress = front_address.into();

    let (config, listeners, state) = Worker::empty_https_config(front_address);
    let mut worker = Worker::start_new_worker_owned("HTTPS-REACTIVATE", config, listeners, state);

    worker.send_proxy_request_type(RequestType::AddHttpsListener(
        ListenerBuilder::new_https(listener_address.clone())
            .to_tls(None)
            .expect("could not build https listener config"),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: listener_address.clone(),
        proxy: ListenerType::Https.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "cluster_0",
    )));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: "localhost".to_owned(),
        ..Worker::default_http_frontend("cluster_0", front_address)
    }));
    worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
        address: listener_address.clone(),
        certificate: CertificateAndKey {
            certificate: String::from(include_str!("../../../lib/assets/local-certificate.pem")),
            key: String::from(include_str!("../../../lib/assets/local-key.pem")),
            certificate_chain: vec![],
            versions: vec![],
            names: vec![],
        },
        expired_at: None,
    }));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));
    worker.read_to_last();

    let mut backend = AsyncBackend::spawn_detached_backend(
        "BACKEND_0",
        back_address,
        SimpleAggregator::default(),
        AsyncBackend::http_handler("pong"),
    );

    let uri: hyper::Uri = format!("https://localhost:{}/api", front_address.port())
        .parse()
        .expect("could not parse the https uri");

    let before = resolve_request(&build_https_client(), uri.clone());
    let served_before = before
        .as_ref()
        .map(|(status, _)| status.as_u16() == 200)
        .unwrap_or(false);
    println!("HTTPS reactivation: before = {before:?}");

    let cycled = cycle_listener(&mut worker, &listener_address, ListenerType::Https);

    let after = resolve_request(&build_https_client(), uri);
    let served_after = after
        .as_ref()
        .map(|(status, _)| status.as_u16() == 200)
        .unwrap_or(false);
    println!("HTTPS reactivation: after = {after:?}");

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    backend.stop_and_get_aggregator();

    println!(
        "HTTPS reactivation: before={served_before} cycled={cycled} after={served_after} stopped={stopped}"
    );
    if served_before && cycled && served_after && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_https_listener_serves_after_reactivation() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "listener lifecycle: an HTTPS listener still serves traffic after deactivate → activate",
            try_https_listener_serves_after_reactivation,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 3: a TCP listener still serves after deactivate -> activate
// =========================================================================

/// To SEE THIS RED: in `lib/src/server.rs`, in `notify_deactivate_listener`'s
/// TCP arm, replace `self.reserve_listen_token(token, Protocol::TCPListen);`
/// with `self.sessions.borrow_mut().slab.remove(token.0);` — the reactivated
/// listener's readiness event is then dropped by `Server::ready`, it accepts
/// nothing, and `after` is `None`.
fn try_tcp_listener_serves_after_reactivation() -> State {
    let front_address = create_local_address();
    let back_address = create_local_address();
    let listener_address: SocketAddress = front_address.into();

    let (config, listeners, state) = Worker::empty_tcp_config(front_address);
    let mut worker = Worker::start_new_worker_owned("TCP-REACTIVATE", config, listeners, state);

    worker.send_proxy_request_type(RequestType::AddTcpListener(
        ListenerBuilder::new_tcp(listener_address.clone())
            .to_tcp(None)
            .expect("could not build tcp listener config"),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: listener_address.clone(),
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

    let mut backend = AsyncBackend::spawn_detached_backend(
        "BACKEND_0",
        back_address,
        SimpleAggregator::default(),
        AsyncBackend::tcp_handler("pong"),
    );

    let before = tcp_round_trip(front_address, b"ping", "before");
    let served_before = before.as_deref() == Some(b"pong".as_slice());

    let cycled = cycle_listener(&mut worker, &listener_address, ListenerType::Tcp);

    let after = tcp_round_trip(front_address, b"ping", "after");
    let served_after = after.as_deref() == Some(b"pong".as_slice());

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    backend.stop_and_get_aggregator();

    println!(
        "TCP reactivation: before={served_before} cycled={cycled} after={served_after} stopped={stopped}"
    );
    if served_before && cycled && served_after && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_tcp_listener_serves_after_reactivation() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "listener lifecycle: a TCP listener still serves traffic after deactivate → activate",
            try_tcp_listener_serves_after_reactivation,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 4: a UDP listener still serves after deactivate -> activate
//
// The UDP failure mode was the worst of the four: `notify_activate_listener`
// installs the real `UdpListenerSession` at the listen token, and it could not
// once the slot had been freed — the socket was registered with nothing behind
// it and every datagram was dropped while the request answered `ok`.
// =========================================================================

/// To SEE THIS RED: in `lib/src/server.rs`, in `notify_deactivate_listener`'s
/// UDP arm, replace `self.reserve_listen_token(token, Protocol::UDPListen);`
/// with `self.sessions.borrow_mut().slab.remove(token.0);` — the reactivation
/// then cannot install its `UdpListenerSession`, so the activate arm answers
/// an error instead of `ok` (`cycled` turns false) and `after` is `None`.
fn try_udp_listener_serves_after_reactivation() -> State {
    let (mut worker, front_address, back_address) = setup_udp_reactivation_worker("UDP-REACTIVATE");
    let listener_address: SocketAddress = front_address.into();
    let backend = UdpBackend::bind("BK0", back_address, 1).spawn();

    let before = UdpClient::new("BEFORE", front_address).round_trip(b"before", ROUND_TRIP);
    let served_before = before.as_deref().and_then(udp_payload).as_deref() == Some(b"before");

    let cycled = cycle_listener(&mut worker, &listener_address, ListenerType::Udp);

    let after = UdpClient::new("AFTER", front_address).round_trip(b"after", ROUND_TRIP);
    let served_after = after.as_deref().and_then(udp_payload).as_deref() == Some(b"after");

    backend.stop();
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    println!(
        "UDP reactivation: before={served_before} cycled={cycled} after={served_after} stopped={stopped}"
    );
    if served_before && cycled && served_after && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_udp_listener_serves_after_reactivation() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "listener lifecycle: a UDP listener still serves traffic after deactivate → activate",
            try_udp_listener_serves_after_reactivation,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 5: add -> remove cycles release the listener, with no deactivate
//
// `remove-listener` is the sole release point of the slab slot. No proxy's
// `remove_listener` touches the session slab, so before the fix removing a
// listener that had NOT been deactivated first stranded its slot — and for UDP
// the stranded entry held the last `UdpListenerSession` reference, keeping the
// listener's `UdpSocket` open and keeping `Server::ready` dispatching to a
// listener the control plane had removed.
//
// Three cycles at the SAME address: each one must serve while the listener is
// there and must go silent once it is removed, and the worker must still be
// accepting on the last one.
// =========================================================================

/// To SEE THIS RED: in `lib/src/server.rs`, in `notify_proxys`' `RemoveListener`
/// arm, delete the `self.sessions.borrow_mut().slab.try_remove(token.0);` line
/// from the `if let Some(token) = listen_token` block. The removed listener's
/// `UdpListenerSession` then stays in the slab holding the listener's
/// `UdpSocket` open and registered, so `Server::ready` keeps dispatching to a
/// listener the control plane removed: the datagram sent after the first
/// `remove-listener` is still answered and `silenced` is `[false, …]`.
fn try_udp_add_remove_cycles_do_not_leak_a_listener() -> State {
    let front_address = create_local_address();
    let back_address = create_local_address();
    let listener_address: SocketAddress = front_address.into();
    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker_owned("UDP-ADDREMOVE", config, listeners, state);

    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "udp_cluster_0",
    )));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "udp_cluster_0",
        "udp_cluster_0-0",
        back_address,
        None,
    )));
    worker.read_to_last();

    let backend = UdpBackend::bind("BK0", back_address, 1).spawn();

    let mut served = Vec::new();
    let mut silenced = Vec::new();
    for cycle in 0..3 {
        worker.send_proxy_request_type(RequestType::AddUdpListener(
            ListenerBuilder::new_udp(listener_address.clone())
                .to_udp(None)
                .expect("could not build udp listener config"),
        ));
        worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
            address: listener_address.clone(),
            proxy: ListenerType::Udp.into(),
            from_scm: false,
        }));
        worker.send_proxy_request_type(RequestType::AddUdpFrontend(
            sozu_command_lib::proto::command::RequestUdpFrontend {
                cluster_id: "udp_cluster_0".to_owned(),
                address: listener_address.clone(),
                tags: Default::default(),
            },
        ));
        worker.read_to_last();

        let payload = format!("cycle-{cycle}");
        let reply = UdpClient::new(format!("C{cycle}"), front_address)
            .round_trip(payload.as_bytes(), ROUND_TRIP);
        served.push(reply.as_deref().and_then(udp_payload).as_deref() == Some(payload.as_bytes()));

        // No `DeactivateListener`: `RemoveListener` alone must release the
        // listener AND its slab slot.
        worker.send_proxy_request_type(RequestType::RemoveListener(RemoveListener {
            address: listener_address.clone(),
            proxy: ListenerType::Udp.into(),
        }));
        worker.read_to_last();

        let after_remove = UdpClient::new(format!("C{cycle}-GONE"), front_address)
            .round_trip(b"gone", Duration::from_millis(500));
        silenced.push(after_remove.is_none());
    }

    backend.stop();
    // `hard_stop`, not `soft_stop`: a leaked listener session holds the event
    // loop's session count above zero, so a soft stop would hang forever
    // instead of letting the assertions below report the leak.
    worker.hard_stop();
    let stopped = worker.wait_for_server_stop();

    println!("UDP add/remove cycles: served={served:?} silenced={silenced:?} stopped={stopped}");
    if served.iter().all(|ok| *ok) && silenced.iter().all(|ok| *ok) && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_udp_add_remove_cycles_do_not_leak_a_listener() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "listener lifecycle: three UDP add → activate → remove cycles each serve then go silent",
            try_udp_add_remove_cycles_do_not_leak_a_listener,
        ),
        State::Success,
    );
}
