//! End-to-end adversarial tests for Group 8E — session, socket and
//! connection-lifecycle hardening.
//!
//! Recipes landing in this module follow (one per commit):
//!
//! * FIX-18 (`ba0f177c`) — [`e2e_session_router_connect_failure_no_leak`]:
//!   arm the `new_h2_client` failure-injection hook so `Router::connect`
//!   bails out, and assert sozu returns a non-success status, the backend
//!   is never invoked, and the worker still performs a clean soft-stop.
//! * FIX-19 (`1f84f86e`) — [`e2e_socket_bad_tls_peer_does_not_starve_others`]:
//!   stalled half-written TLS peers must not pin a worker — a well-formed
//!   H2 client on the same listener still completes a request inside a
//!   5-second budget.
//! * FIX-20 (`2e6b0ef0`) — [`e2e_session_upgrade_backend_disconnect_no_panic`]:
//!   an HTTP upgrade whose backend drops the connection before the 101
//!   completes must not panic the worker — graceful error branch in
//!   `upgrade_mux`.
//!
//! The `e2e-hooks` feature is forwarded to `sozu-lib` via
//! `e2e/Cargo.toml` so the hooks compile in.

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    os::fd::AsRawFd,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use hyper::Uri;
use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, AddCertificate, CertificateAndKey, Cluster, ListenerType,
        RequestHttpFrontend, SocketAddress, request::RequestType,
    },
};
use sozu_lib::protocol::mux::connection::test_hooks::__test_force_h2_client_failure;

use crate::{
    mock::{
        client::Client,
        h2_backend::H2Backend,
        https_client::{build_h2_client, resolve_request, resolve_request_timeout},
    },
    port_registry::{bind_std_listener, provide_port},
    sozu::worker::Worker,
    tests::{
        State,
        h2_utils::{setup_h2_test, verify_sozu_alive},
        repeat_until_error_or,
        tests::create_local_address,
    },
};

// ============================================================================
// Helpers
// ============================================================================

/// Give sozu's event loop a tick so asynchronous gauge decrements / session
/// cleanup land before follow-up assertions.
fn pump_worker(_worker: &mut Worker) {
    thread::sleep(Duration::from_millis(200));
}

/// Enable `SO_LINGER(0)` on a raw TCP file descriptor so that closing the
/// socket sends a RST instead of a graceful FIN. `TcpStream::set_linger` is
/// still nightly-only, so we reach for `libc::setsockopt` directly.
fn set_linger_zero(fd: libc::c_int) {
    let linger = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    // Safety: `linger` is a valid `libc::linger`; `fd` must live for the
    // duration of the call. The caller holds the owning handle.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            &linger as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::linger>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        eprintln!(
            "set_linger_zero: setsockopt failed ({})",
            std::io::Error::last_os_error(),
        );
    }
}

// ============================================================================
// FIX-18 — Router::connect rollback on backend failure
// ============================================================================

/// Setup a cluster whose backend speaks H2 so that the
/// `Connection::new_h2_client` path is taken.
fn setup_h2_backend_cluster(name: &str) -> (Worker, H2Backend, u16) {
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
    worker.send_proxy_request_type(RequestType::AddCluster(Cluster {
        http2: Some(true),
        ..Worker::default_cluster("cluster_0")
    }));
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

    let back_address = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));
    let backend = H2Backend::start("H2_BACKEND_FIX18", back_address, "fix18-pong");

    worker.read_to_last();
    thread::sleep(Duration::from_millis(100));
    (worker, backend, front_port)
}

fn try_e2e_session_router_connect_failure_no_leak() -> State {
    let (mut worker, mut backend, front_port) = setup_h2_backend_cluster("E2E-SESSION-FIX18");

    // Arm the injection: the NEXT call to Connection::new_h2_client inside
    // the worker will return None, driving Router::connect into its
    // MaxBuffers error path.
    let _prev = __test_force_h2_client_failure(true);

    let client = build_h2_client();
    let uri: Uri = format!("https://localhost:{front_port}/api")
        .parse()
        .unwrap();

    // The `new_h2_client` injection is one-shot and the backend side of the
    // mux may keep the stream open while returning a 503 body — hyper's
    // body collection can then hang for the configured 10 s. Shorten the
    // budget: we only care that sozu emits a non-success status quickly.
    let response = resolve_request_timeout(&client, uri, Duration::from_secs(3));
    println!("FIX-18 router leak response: {response:?}");

    // Disarm the hook so the rest of the session (including teardown /
    // soft-stop) can proceed normally.
    let _ = __test_force_h2_client_failure(false);
    pump_worker(&mut worker);

    let sozu_ok = verify_sozu_alive(front_port);
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    let resp_sent = backend.get_responses_sent();
    backend.stop();

    // Acceptance:
    // 1. The forced failure produced a non-success status (503 is typical)
    //    OR the hyper call errored out entirely (also acceptable — both
    //    mean Router::connect rejected the stream).
    // 2. The backend was never asked to serve the forced-fail request
    //    (`resp_sent == 0` — Router::connect aborted before committing any
    //    backend state).
    // 3. Sozu is still reachable on the front listener and performs a
    //    clean soft-stop — the hallmark of "no resource leak".
    let first_failed = response
        .as_ref()
        .map(|(s, _)| !s.is_success())
        .unwrap_or(true);

    if first_failed && resp_sent == 0 && sozu_ok && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn e2e_session_router_connect_failure_no_leak() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "FIX-18: Router::connect rollback on forced backend failure",
            try_e2e_session_router_connect_failure_no_leak,
        ),
        State::Success,
    );
}

// ============================================================================
// FIX-19 — Socket MAX_LOOP_ITERATIONS DoS
// ============================================================================

/// Thread that opens a raw TCP connection, writes a single garbage byte, then
/// sleeps forever. The goal is to keep a session half-created in sozu's event
/// loop (rustls mid-handshake) and ensure this does not pin the worker.
fn spawn_stalled_tls_peer(addr: SocketAddr, stop: Arc<AtomicBool>) -> thread::JoinHandle<()> {
    thread::spawn(move || match TcpStream::connect(addr) {
        Ok(mut sock) => {
            let _ = sock.set_read_timeout(Some(Duration::from_millis(100)));
            let _ = sock.set_write_timeout(Some(Duration::from_millis(100)));
            // Write a single byte of junk — insufficient for rustls to
            // complete its ClientHello, so sozu sees progress but no
            // usable frames.
            let _ = sock.write_all(&[0x16]);
            let _ = sock.flush();

            while !stop.load(Ordering::Relaxed) {
                // Drain anything sozu might send (alerts) so the socket
                // stays open; ignore errors.
                let mut buf = [0u8; 64];
                let _ = sock.read(&mut buf);
                thread::sleep(Duration::from_millis(50));
            }
        }
        Err(e) => {
            eprintln!("stalled TLS peer: connect failed: {e}");
        }
    })
}

fn try_e2e_socket_bad_tls_peer_does_not_starve_others() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_test("E2E-SOCKET-FIX19", 1);
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let mut stallers = Vec::new();
    // 3 concurrent stalled peers should be enough to trigger the old
    // infinite-loop path if the fix is reverted.
    for _ in 0..3 {
        stallers.push(spawn_stalled_tls_peer(front_addr, stop.clone()));
    }

    // Let the stallers register.
    thread::sleep(Duration::from_millis(200));

    // Healthy client: must complete within 5 s.
    let deadline = Instant::now() + Duration::from_secs(5);
    let client = build_h2_client();
    let uri: Uri = format!("https://localhost:{front_port}/api/healthy")
        .parse()
        .unwrap();

    let healthy_result = resolve_request(&client, uri);
    let elapsed = Instant::now();
    let within_budget = elapsed <= deadline;
    println!("FIX-19 healthy result: {healthy_result:?} within_budget={within_budget}");

    // Stop stallers.
    stop.store(true, Ordering::Relaxed);
    for handle in stallers {
        let _ = handle.join();
    }

    let sozu_ok = verify_sozu_alive(front_port);
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    for backend in backends.iter_mut() {
        backend.stop_and_get_aggregator();
    }

    let ok = healthy_result
        .as_ref()
        .map(|(s, body)| s.is_success() && body.contains("pong"))
        .unwrap_or(false);

    if ok && within_budget && sozu_ok && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn e2e_socket_bad_tls_peer_does_not_starve_others() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "FIX-19: stalled TLS peer does not starve a concurrent healthy H2 client",
            try_e2e_socket_bad_tls_peer_does_not_starve_others,
        ),
        State::Success,
    );
}

// ============================================================================
// FIX-20 — upgrade_mux graceful error on backend disconnect
// ============================================================================

/// TCP backend that accepts a connection, reads the upgrade request, and then
/// closes the socket before finishing the 101 handshake — hitting the
/// `upgrade_mux` error path that previously panicked.
fn spawn_upgrade_dropping_backend(address: SocketAddr) -> (Arc<AtomicBool>, Arc<AtomicUsize>) {
    let stop = Arc::new(AtomicBool::new(false));
    let connections = Arc::new(AtomicUsize::new(0));
    let stop_clone = stop.clone();
    let conn_count = connections.clone();
    thread::spawn(move || {
        let listener = bind_std_listener(address, "upgrade-drop backend");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        loop {
            if stop_clone.load(Ordering::Relaxed) {
                break;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_millis(200)))
                        .ok();
                    let mut buf = [0u8; 4096];
                    let _ = stream.read(&mut buf);
                    conn_count.fetch_add(1, Ordering::Relaxed);
                    // Close immediately — no 101, no upgrade. Linger(0) so
                    // sozu observes a RST, exercising the `upgrade_mux`
                    // error branch.
                    set_linger_zero(stream.as_raw_fd());
                    drop(stream);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(_) => {}
            }
        }
    });
    (stop, connections)
}

fn try_e2e_session_upgrade_backend_disconnect_no_panic() -> State {
    let front_port = provide_port();
    let front_address: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    let (config, listeners, state) = Worker::empty_http_config(front_address.into());
    let mut worker = Worker::start_new_worker_owned("E2E-SESSION-FIX20", config, listeners, state);

    worker.send_proxy_request_type(RequestType::AddHttpListener(
        ListenerBuilder::new_http(front_address.into())
            .to_http(None)
            .unwrap(),
    ));
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

    let back_address = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));
    worker.read_to_last();

    let (backend_stop, conn_counter) = spawn_upgrade_dropping_backend(back_address);
    thread::sleep(Duration::from_millis(150));

    // Fire several upgrade attempts in parallel — panics are per-session, so
    // we want reasonable coverage of the error path.
    let mut handles = Vec::new();
    for i in 0..5 {
        let addr = front_address;
        handles.push(thread::spawn(move || {
            let req = format!(
                "GET /ws{i} HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
            );
            let mut client = Client::new(format!("fix20-{i}"), addr, req);
            client.connect();
            let _ = client.send();
            let _ = client.receive();
            client.disconnect();
        }));
    }
    for h in handles {
        let _ = h.join();
    }

    thread::sleep(Duration::from_millis(300));
    let sozu_ok = verify_sozu_alive(front_port);
    println!(
        "FIX-20 upgrade: sozu_alive={sozu_ok} backend_connections={}",
        conn_counter.load(Ordering::Relaxed),
    );

    backend_stop.store(true, Ordering::Relaxed);
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    if sozu_ok && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn e2e_session_upgrade_backend_disconnect_no_panic() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "FIX-20: HTTP upgrade with backend disconnect does not panic",
            try_e2e_session_upgrade_backend_disconnect_no_panic,
        ),
        State::Success,
    );
}
