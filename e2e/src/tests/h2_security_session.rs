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
//!
//! The `e2e-hooks` feature is forwarded to `sozu-lib` via
//! `e2e/Cargo.toml` so the hooks compile in.

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
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
        h2_backend::H2Backend,
        https_client::{build_h2_client, resolve_request, resolve_request_timeout},
    },
    port_registry::provide_port,
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
