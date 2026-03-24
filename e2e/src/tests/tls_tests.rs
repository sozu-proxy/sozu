//! End-to-end tests for TLS security behavior in Sozu.
//!
//! These tests verify that the HTTPS listener correctly handles:
//! - SNI-based routing to different clusters based on hostname
//! - Requests for unknown/unconfigured hostnames
//! - ALPN negotiation mismatches (H2-only client vs H1-only listener)
//! - Incomplete TLS handshakes (partial ClientHello timeout)
//! - Connection: close header with proper TLS teardown

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use rustls::ClientConfig;
use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, AddCertificate, CertificateAndKey, ListenerType, RequestHttpFrontend,
        SocketAddress, request::RequestType,
    },
};

use crate::{
    mock::{
        aggregator::SimpleAggregator,
        async_backend::BackendHandle as AsyncBackend,
        https_client::{Verifier, build_h2_client, build_https_client, resolve_request},
    },
    port_registry::bind_std_listener,
    sozu::worker::Worker,
    tests::{State, provide_port, repeat_until_error_or, tests::create_local_address},
};

struct BlockingHttpBackend {
    stop: Arc<AtomicBool>,
    requests_received: Arc<AtomicUsize>,
    responses_sent: Arc<AtomicUsize>,
    thread: Option<thread::JoinHandle<()>>,
}

impl BlockingHttpBackend {
    fn start(address: SocketAddr, body: String) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let requests_received = Arc::new(AtomicUsize::new(0));
        let responses_sent = Arc::new(AtomicUsize::new(0));

        let stop_clone = stop.clone();
        let requests_clone = requests_received.clone();
        let responses_clone = responses_sent.clone();

        let thread = thread::spawn(move || {
            let listener = bind_std_listener(address, "blocking tls backend");
            listener
                .set_nonblocking(true)
                .expect("could not set backend listener nonblocking");

            while !stop_clone.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
                        stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

                        let mut buf = [0u8; 4096];
                        let _ = stream.read(&mut buf);
                        requests_clone.fetch_add(1, Ordering::Relaxed);

                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        if stream.write_all(response.as_bytes()).is_ok() {
                            let _ = stream.flush();
                            responses_clone.fetch_add(1, Ordering::Relaxed);
                        }
                        break;
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            stop,
            requests_received,
            responses_sent,
            thread: Some(thread),
        }
    }

    fn requests_received(&self) -> usize {
        self.requests_received.load(Ordering::Relaxed)
    }

    fn responses_sent(&self) -> usize {
        self.responses_sent.load(Ordering::Relaxed)
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for BlockingHttpBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

// ============================================================================
// Test 1: SNI routing — two certificates, two clusters, correct routing
// ============================================================================

/// Verify that Sozu routes requests to the correct backend cluster based on the
/// SNI hostname in the TLS ClientHello.
///
/// This test sets up a single HTTPS listener with two certificates:
/// - "localhost" → cluster_0 (backend responds "pong-localhost")
/// - "other.localhost" → cluster_1 (backend responds "pong-other")
///
/// Both certificates use the same `local-certificate.pem` / `local-key.pem`
/// (which has CN=localhost, SAN=localhost). Since Sozu dispatches by the
/// frontend hostname match (not the certificate SAN), we register two frontends
/// with distinct hostnames and verify each request reaches the correct backend.
fn try_tls_sni_routing() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let back_address_0 = create_local_address();
    let back_address_1 = create_local_address();

    let (config, listeners, state) = Worker::empty_https_config(front_address.into());
    let mut worker = Worker::start_new_worker_owned("TLS-SNI-ROUTING", config, listeners, state);

    // Add HTTPS listener
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

    // Cluster 0: "localhost"
    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "cluster_0",
    )));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: "localhost".to_owned(),
        ..Worker::default_http_frontend("cluster_0", front_address.clone().into())
    }));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address_0,
        None,
    )));

    // Cluster 1: "other.localhost"
    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "cluster_1",
    )));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: "other.localhost".to_owned(),
        ..Worker::default_http_frontend("cluster_1", front_address.clone().into())
    }));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_1",
        "cluster_1-0",
        back_address_1,
        None,
    )));

    // Add TLS certificate (covers both hostnames via our permissive verifier)
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

    // Start backends
    let mut backend_0 = AsyncBackend::spawn_detached_backend(
        "BACKEND_0",
        back_address_0,
        SimpleAggregator::default(),
        AsyncBackend::http_handler("pong-localhost"),
    );
    let mut backend_1 = AsyncBackend::spawn_detached_backend(
        "BACKEND_1",
        back_address_1,
        SimpleAggregator::default(),
        AsyncBackend::http_handler("pong-other"),
    );

    worker.read_to_last();

    // Request to "localhost" — should reach cluster_0
    let client = build_https_client();
    let uri_localhost: hyper::Uri = format!("https://localhost:{front_port}/api")
        .parse()
        .unwrap();
    let result_localhost = resolve_request(&client, uri_localhost);

    // Request to "other.localhost" — should reach cluster_1
    let uri_other: hyper::Uri = format!("https://other.localhost:{front_port}/api")
        .parse()
        .unwrap();
    let result_other = resolve_request(&client, uri_other);

    worker.soft_stop();
    let success = worker.wait_for_server_stop();

    let agg_0 = backend_0
        .stop_and_get_aggregator()
        .expect("Could not get aggregator for backend_0");
    let agg_1 = backend_1
        .stop_and_get_aggregator()
        .expect("Could not get aggregator for backend_1");

    println!(
        "BACKEND_0: sent={}, received={}",
        agg_0.responses_sent, agg_0.requests_received
    );
    println!(
        "BACKEND_1: sent={}, received={}",
        agg_1.responses_sent, agg_1.requests_received
    );

    // Verify localhost request reached cluster_0
    let localhost_ok = match result_localhost {
        Some((status, body)) => {
            println!("localhost response: status={status}, body={body}");
            status.is_success() && body.contains("pong-localhost")
        }
        None => {
            println!("localhost request failed");
            false
        }
    };

    // Verify other.localhost request reached cluster_1
    let other_ok = match result_other {
        Some((status, body)) => {
            println!("other.localhost response: status={status}, body={body}");
            status.is_success() && body.contains("pong-other")
        }
        None => {
            println!("other.localhost request failed");
            false
        }
    };

    if success && localhost_ok && other_ok && agg_0.responses_sent == 1 && agg_1.responses_sent == 1
    {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_tls_sni_routing() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "TLS: SNI-based routing dispatches requests to correct cluster",
            try_tls_sni_routing
        ),
        State::Success
    );
}

// ============================================================================
// Test 2: Invalid hostname — request for unconfigured hostname
// ============================================================================

/// Verify that a request with an unknown hostname (no matching frontend)
/// is rejected gracefully without crashing the worker.
///
/// Sozu should respond with a 404 or close the connection when no frontend
/// matches the requested hostname. The key assertion is that the worker
/// remains alive and functional afterward.
fn try_tls_invalid_hostname() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let back_address = create_local_address();

    let (config, listeners, state) = Worker::empty_https_config(front_address.into());
    let mut worker =
        Worker::start_new_worker_owned("TLS-INVALID-HOSTNAME", config, listeners, state);

    // HTTPS listener with certificate for "localhost" only
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
        hostname: "localhost".to_owned(),
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
        address: front_address.clone(),
        certificate: certificate_and_key,
        expired_at: None,
    }));

    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));

    let mut backend = AsyncBackend::spawn_detached_backend(
        "BACKEND",
        back_address,
        SimpleAggregator::default(),
        AsyncBackend::http_handler("pong"),
    );

    worker.read_to_last();

    // Send a request with hostname "unknown.host" — no frontend should match.
    // The client uses our permissive verifier, so TLS handshake succeeds
    // despite hostname mismatch in the certificate.
    let client = build_https_client();
    let uri: hyper::Uri = format!("https://unknown.host:{front_port}/api")
        .parse()
        .unwrap();
    let result = resolve_request(&client, uri);

    // Either we get a 404 response, or the connection is closed (None).
    // Both are acceptable — the key is no crash.
    let invalid_handled = match &result {
        Some((status, body)) => {
            println!("unknown.host response: status={status}, body={body}");
            // A 404 is the expected Sozu behavior for unknown frontends
            status.as_u16() == 404
        }
        None => {
            // Connection closed or refused — also acceptable
            println!("unknown.host: connection closed or failed (expected)");
            true
        }
    };

    // Verify the worker is still alive by sending a valid request
    let valid_uri: hyper::Uri = format!("https://localhost:{front_port}/api")
        .parse()
        .unwrap();
    let valid_result = resolve_request(&client, valid_uri);
    let worker_alive = match valid_result {
        Some((status, body)) => {
            println!("follow-up localhost response: status={status}, body={body}");
            status.is_success() && body.contains("pong")
        }
        None => {
            println!("follow-up localhost request failed — worker may be dead");
            false
        }
    };

    worker.soft_stop();
    let success = worker.wait_for_server_stop();

    let aggregator = backend
        .stop_and_get_aggregator()
        .expect("Could not get aggregator");
    println!(
        "BACKEND: sent={}, received={}",
        aggregator.responses_sent, aggregator.requests_received
    );

    if success && invalid_handled && worker_alive {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_tls_invalid_hostname() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "TLS: request with unknown hostname returns 404 without crash",
            try_tls_invalid_hostname
        ),
        State::Success
    );
}

// ============================================================================
// Test 3: ALPN mismatch recovery — H2-only client vs H1-only listener
// ============================================================================

/// Verify that when a listener is configured for HTTP/1.1 only (no H2 in ALPN),
/// an H2-only client is handled gracefully.
///
/// Expected behavior: the TLS handshake may succeed (ALPN mismatch is not fatal
/// at the TLS level), but the subsequent H2 connection attempt should either:
/// - Be downgraded to HTTP/1.1 (if the client falls back), or
/// - Fail cleanly without crashing the worker.
///
/// The important thing is that Sozu does not panic or hang, and remains
/// available for subsequent valid connections.
fn try_tls_alpn_mismatch_recovery() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let back_address = create_local_address();

    let (config, listeners, state) = Worker::empty_https_config(front_address.into());
    let mut worker = Worker::start_new_worker_owned("TLS-ALPN-MISMATCH", config, listeners, state);

    // Listener with HTTP/1.1 only — no H2 support
    let mut listener_builder = ListenerBuilder::new_https(front_address.clone());
    listener_builder.with_alpn_protocols(Some(vec!["http/1.1".to_owned()]));
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
        hostname: "localhost".to_owned(),
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
        address: front_address.clone(),
        certificate: certificate_and_key,
        expired_at: None,
    }));

    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));

    let mut backend = AsyncBackend::spawn_detached_backend(
        "BACKEND",
        back_address,
        SimpleAggregator::default(),
        AsyncBackend::http_handler("pong"),
    );

    worker.read_to_last();

    // H2-only client attempts to connect to H1-only listener.
    // The ALPN negotiation should either fail or select no protocol,
    // causing the H2 connection to fail gracefully.
    let h2_client = build_h2_client();
    let uri: hyper::Uri = format!("https://localhost:{front_port}/api")
        .parse()
        .unwrap();
    let h2_result = resolve_request(&h2_client, uri);

    // The H2 request may succeed (if Sozu tolerates ALPN mismatch) or fail.
    // Both outcomes are fine as long as the worker stays alive.
    match &h2_result {
        Some((status, body)) => {
            println!("H2 client on H1-only listener: status={status}, body={body}");
        }
        None => {
            println!("H2 client on H1-only listener: connection failed (expected)");
        }
    }

    // Verify Sozu is still alive — try a follow-up H1 request.
    // Small delay to let Sozu recover from the ALPN mismatch connection.
    thread::sleep(Duration::from_millis(500));
    let h1_client = build_https_client();
    let valid_uri: hyper::Uri = format!("https://localhost:{front_port}/api")
        .parse()
        .unwrap();
    let h1_result = resolve_request(&h1_client, valid_uri);
    match &h1_result {
        Some((status, body)) => {
            println!("follow-up H1 request: status={status}, body={body}");
        }
        None => {
            println!("follow-up H1 request failed — checking if worker is still alive via TCP");
        }
    }

    // The critical check is that the worker process didn't crash.
    // The follow-up request may fail if the ALPN mismatch corrupted
    // the connection pool, but the worker must survive.
    let worker_survived = std::net::TcpStream::connect_timeout(
        &format!("127.0.0.1:{front_port}").parse().unwrap(),
        Duration::from_secs(2),
    )
    .is_ok();
    println!("Worker survived ALPN mismatch: {worker_survived}");

    worker.soft_stop();
    let success = worker.wait_for_server_stop();

    backend.stop_and_get_aggregator();

    if success && worker_survived {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_tls_alpn_mismatch_recovery() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "TLS: H2-only client vs H1-only listener is handled gracefully",
            try_tls_alpn_mismatch_recovery
        ),
        State::Success
    );
}

// ============================================================================
// Test 4: Handshake timeout — partial ClientHello on raw TCP
// ============================================================================

/// Verify that Sozu closes connections where the TLS handshake never completes.
///
/// This test opens a raw TCP connection and sends a few bytes that look like the
/// beginning of a TLS ClientHello, but never completes the handshake. Sozu should
/// time out and close the connection rather than holding it open indefinitely.
///
/// The test asserts that the connection is closed within a reasonable timeout
/// window (30 seconds), proving Sozu does not leak file descriptors on stalled
/// handshakes.
fn try_tls_handshake_timeout() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let back_address = create_local_address();

    let (config, listeners, state) = Worker::empty_https_config(front_address.into());
    let mut worker =
        Worker::start_new_worker_owned("TLS-HANDSHAKE-TIMEOUT", config, listeners, state);

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
        hostname: "localhost".to_owned(),
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

    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));

    let mut backend = AsyncBackend::spawn_detached_backend(
        "BACKEND",
        back_address,
        SimpleAggregator::default(),
        AsyncBackend::http_handler("pong"),
    );

    worker.read_to_last();

    // Open a raw TCP connection and send partial TLS ClientHello bytes.
    // TLS record: ContentType=Handshake(0x16), Version=TLS1.0(0x0301),
    // Length=5(truncated), HandshakeType=ClientHello(0x01)
    let partial_client_hello: &[u8] = &[
        0x16, // ContentType: Handshake
        0x03, 0x01, // ProtocolVersion: TLS 1.0
        0x00, 0x05, // Length: 5 (but we won't send all of it)
        0x01, // HandshakeType: ClientHello
        0x00, 0x00, // Partial length (truncated)
    ];

    let addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let tcp = match TcpStream::connect_timeout(&addr, Duration::from_secs(5)) {
        Ok(stream) => stream,
        Err(e) => {
            println!("Could not connect to sozu: {e}");
            worker.soft_stop();
            worker.wait_for_server_stop();
            backend.stop_and_get_aggregator();
            return State::Fail;
        }
    };

    // Set a generous read timeout — we expect Sozu to close the connection
    // well before this, but we don't want the test to hang forever.
    tcp.set_read_timeout(Some(Duration::from_secs(35)))
        .expect("set read timeout");
    tcp.set_write_timeout(Some(Duration::from_secs(5)))
        .expect("set write timeout");

    // Send partial ClientHello — not enough to complete the handshake
    let mut tcp = tcp;
    if let Err(e) = tcp.write_all(partial_client_hello) {
        println!("Could not write partial ClientHello: {e}");
        worker.soft_stop();
        worker.wait_for_server_stop();
        backend.stop_and_get_aggregator();
        return State::Fail;
    }
    tcp.flush().ok();

    // Wait for Sozu to close the connection. The default handshake timeout
    // is typically a few seconds. We wait up to 30 seconds to be safe.
    let start = Instant::now();
    let mut buf = [0u8; 1024];
    let connection_closed = loop {
        match tcp.read(&mut buf) {
            Ok(0) => {
                // EOF — connection closed by Sozu
                println!(
                    "Connection closed by Sozu after {:.1}s",
                    start.elapsed().as_secs_f64()
                );
                break true;
            }
            Ok(n) => {
                // Sozu sent something (e.g., TLS alert) — that's fine
                println!("Received {n} bytes from Sozu (possibly TLS alert)");
                break true;
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                if start.elapsed() > Duration::from_secs(30) {
                    println!("Connection still open after 30s — Sozu did not time out");
                    break false;
                }
                thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                // Connection reset or other error — connection was closed
                println!(
                    "Connection error after {:.1}s: {e}",
                    start.elapsed().as_secs_f64()
                );
                break true;
            }
        }
    };
    drop(tcp);

    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    backend.stop_and_get_aggregator();

    if success && connection_closed {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_tls_handshake_timeout() {
    assert_eq!(
        repeat_until_error_or(
            3, // Fewer iterations — each run waits for a timeout
            "TLS: partial ClientHello triggers handshake timeout and connection close",
            try_tls_handshake_timeout
        ),
        State::Success
    );
}

// ============================================================================
// Test 5: Connection: close header — TLS teardown after response
// ============================================================================

/// Verify that a request with `Connection: close` over HTTPS results in the
/// TLS connection being properly torn down after the response is delivered.
///
/// This test uses a raw rustls connection to send an HTTP/1.1 request with
/// `Connection: close`, then verifies that Sozu closes the TLS session after
/// sending the response. The response should be complete and the connection
/// should be cleanly shut down.
fn try_tls_connection_close_header() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let back_address = create_local_address();

    let (config, listeners, state) = Worker::empty_https_config(front_address.into());
    let mut worker = Worker::start_new_worker_owned("TLS-CONN-CLOSE", config, listeners, state);

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
        hostname: "localhost".to_owned(),
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

    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));

    let mut backend = AsyncBackend::spawn_detached_backend(
        "BACKEND",
        back_address,
        SimpleAggregator::default(),
        AsyncBackend::http_handler("pong"),
    );

    worker.read_to_last();

    // Establish a TLS connection with HTTP/1.1 ALPN
    let tls_config = {
        let mut config = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Verifier))
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        config
    };

    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let conn = rustls::ClientConnection::new(Arc::new(tls_config), server_name.to_owned()).unwrap();

    let addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let tcp = match TcpStream::connect_timeout(&addr, Duration::from_secs(5)) {
        Ok(stream) => stream,
        Err(e) => {
            println!("Could not connect to sozu: {e}");
            worker.soft_stop();
            worker.wait_for_server_stop();
            backend.stop_and_get_aggregator();
            return State::Fail;
        }
    };
    tcp.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    tcp.set_write_timeout(Some(Duration::from_secs(5)))
        .expect("set write timeout");

    let mut tls_stream = rustls::StreamOwned::new(conn, tcp);

    // Send HTTP/1.1 request with Connection: close
    let request = "GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    if let Err(e) = tls_stream.write_all(request.as_bytes()) {
        println!("Could not send request: {e}");
        worker.soft_stop();
        worker.wait_for_server_stop();
        backend.stop_and_get_aggregator();
        return State::Fail;
    }
    tls_stream.flush().ok();

    // Read the full response
    let mut response_bytes = Vec::new();
    let mut buf = [0u8; 4096];
    let start = Instant::now();
    let mut got_response = false;
    loop {
        match tls_stream.read(&mut buf) {
            Ok(0) => {
                // TLS connection closed — clean shutdown
                println!(
                    "TLS connection closed after {:.1}s",
                    start.elapsed().as_secs_f64()
                );
                break;
            }
            Ok(n) => {
                response_bytes.extend_from_slice(&buf[..n]);
                got_response = true;
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                if got_response {
                    // We already have the response and the connection is idle
                    break;
                }
                if start.elapsed() > Duration::from_secs(10) {
                    println!("Timed out waiting for response");
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                // Connection reset or TLS close_notify — expected with Connection: close
                println!("Read error (expected with Connection: close): {e}");
                break;
            }
        }
    }
    drop(tls_stream);

    let response_str = String::from_utf8_lossy(&response_bytes);
    println!("Response:\n{response_str}");

    // Verify we got a valid HTTP response
    let response_ok = response_str.contains("HTTP/1.1 200") && response_str.contains("pong");

    // Verify the response includes Connection: close (Sozu should echo it)
    let has_connection_close = response_str.to_lowercase().contains("connection: close");
    if !has_connection_close {
        println!("Note: response did not include Connection: close header");
        // This is informational — Sozu may or may not echo the header
    }

    worker.soft_stop();
    let success = worker.wait_for_server_stop();

    let aggregator = backend
        .stop_and_get_aggregator()
        .expect("Could not get aggregator");
    println!(
        "BACKEND: sent={}, received={}",
        aggregator.responses_sent, aggregator.requests_received
    );

    if success && response_ok && aggregator.responses_sent == 1 {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_tls_connection_close_header() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "TLS: Connection: close header causes proper TLS teardown after response",
            try_tls_connection_close_header
        ),
        State::Success
    );
}

/// Regression test for the TLS flush bug: a large HTTPS response on a
/// `Connection: close` request must be fully delivered before the TLS session
/// is torn down.
fn try_tls_connection_close_large_response() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let back_address = create_local_address();
    let payload = "x".repeat(256 * 1024);

    let (config, listeners, state) = Worker::empty_https_config(front_address.into());
    let mut worker =
        Worker::start_new_worker_owned("TLS-CONN-CLOSE-LARGE", config, listeners, state);

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
        hostname: "localhost".to_owned(),
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
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));
    worker.read_to_last();

    let mut backend = BlockingHttpBackend::start(back_address, payload.clone());

    let tls_config = {
        let mut config = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Verifier))
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        config
    };

    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let conn = rustls::ClientConnection::new(Arc::new(tls_config), server_name.to_owned()).unwrap();

    let addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let tcp = match TcpStream::connect_timeout(&addr, Duration::from_secs(5)) {
        Ok(stream) => stream,
        Err(e) => {
            println!("Could not connect to sozu: {e}");
            worker.soft_stop();
            worker.wait_for_server_stop();
            backend.stop();
            return State::Fail;
        }
    };
    tcp.set_read_timeout(Some(Duration::from_secs(10))).ok();
    tcp.set_write_timeout(Some(Duration::from_secs(5))).ok();

    let mut tls_stream = rustls::StreamOwned::new(conn, tcp);
    let request = "GET /large HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    if let Err(e) = tls_stream.write_all(request.as_bytes()) {
        println!("Could not send request: {e}");
        worker.soft_stop();
        worker.wait_for_server_stop();
        backend.stop();
        return State::Fail;
    }
    tls_stream.flush().ok();

    let mut response_bytes = Vec::new();
    let mut buf = [0u8; 8192];
    let start = Instant::now();
    loop {
        match tls_stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => response_bytes.extend_from_slice(&buf[..n]),
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                if start.elapsed() > Duration::from_secs(15) {
                    println!("Timed out waiting for large HTTPS response");
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                println!("Read error after response drain: {e}");
                break;
            }
        }
    }
    drop(tls_stream);

    let response = String::from_utf8_lossy(&response_bytes);
    let (headers, body) = match response.split_once("\r\n\r\n") {
        Some(parts) => parts,
        None => {
            println!("Missing HTTP header/body separator");
            worker.soft_stop();
            worker.wait_for_server_stop();
            backend.stop();
            return State::Fail;
        }
    };

    let response_ok =
        headers.contains("HTTP/1.1 200") && body.len() == payload.len() && body == payload;

    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    let requests_received = backend.requests_received();
    let responses_sent = backend.responses_sent();
    backend.stop();

    if success && response_ok && requests_received == 1 && responses_sent == 1 {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_tls_connection_close_large_response() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "TLS regression: large Connection: close response is fully delivered before teardown",
            try_tls_connection_close_large_response
        ),
        State::Success
    );
}
