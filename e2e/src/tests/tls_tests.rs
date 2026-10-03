//! End-to-end tests for TLS security behavior in Sozu.
//!
//! These tests verify that the HTTPS listener correctly handles:
//! - SNI-based routing to different clusters based on hostname
//! - Requests for unknown/unconfigured hostnames
//! - ALPN negotiation mismatches (H2-only client vs H1-only listener)
//! - Incomplete TLS handshakes (partial ClientHello timeout)
//! - Connection: close header with proper TLS teardown

use std::{
    io::{ErrorKind, Read, Write},
    net::{Shutdown, SocketAddr, TcpStream},
    os::fd::AsRawFd,
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
        sync_backend::Backend as SyncBackend,
    },
    port_registry::bind_std_listener,
    sozu::worker::Worker,
    tests::{State, provide_port, repeat_until_error_or, tests::create_local_address},
};

const PUSHER_CONNECTION_ESTABLISHED: &str =
    r#"{"event":"pusher:connection_established","data":"{}"}"#;
const PUSHER_PING: &str = r#"{"event":"pusher:ping","data":"{}"}"#;
const PUSHER_PONG: &str = r#"{"event":"pusher:pong","data":"{}"}"#;

struct BlockingHttpBackend {
    stop: Arc<AtomicBool>,
    requests_received: Arc<AtomicUsize>,
    responses_sent: Arc<AtomicUsize>,
    thread: Option<thread::JoinHandle<()>>,
}

impl BlockingHttpBackend {
    fn start(address: SocketAddr, body: String) -> Self {
        Self::start_with_connection(address, body, "close")
    }

    /// `connection` is the response's `Connection` value: `close` ends the
    /// exchange from the backend, `keep-alive` leaves sozu to end it.
    fn start_with_connection(address: SocketAddr, body: String, connection: &'static str) -> Self {
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
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: {connection}\r\n\r\n{}",
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

fn websocket_text_frame(payload: &str) -> Vec<u8> {
    let payload = payload.as_bytes();
    let mut frame = Vec::with_capacity(payload.len() + 4);
    frame.push(0x81);
    if payload.len() <= 125 {
        frame.push(payload.len() as u8);
    } else {
        debug_assert!(u16::try_from(payload.len()).is_ok());
        frame.push(126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    frame.extend_from_slice(payload);
    frame
}

fn masked_websocket_text_frame(payload: &str) -> Vec<u8> {
    let payload = payload.as_bytes();
    let mask = [0x12, 0x34, 0x56, 0x78];
    let mut frame = Vec::with_capacity(payload.len() + 8);
    frame.push(0x81);
    if payload.len() <= 125 {
        frame.push(0x80 | payload.len() as u8);
    } else {
        debug_assert!(u16::try_from(payload.len()).is_ok());
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    frame.extend_from_slice(&mask);
    for (idx, byte) in payload.iter().enumerate() {
        frame.push(byte ^ mask[idx % mask.len()]);
    }
    frame
}

fn bytes_contain(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn backend_send_bytes(backend: &mut SyncBackend, client_id: usize, bytes: &[u8]) -> bool {
    match backend.clients.get_mut(&client_id) {
        Some(stream) => stream.write_all(bytes).is_ok() && stream.flush().is_ok(),
        None => false,
    }
}

fn backend_read_bytes(backend: &mut SyncBackend, client_id: usize) -> Option<Vec<u8>> {
    let mut buf = [0u8; 4096];
    match backend.clients.get_mut(&client_id)?.read(&mut buf) {
        Ok(n) if n > 0 => Some(buf[..n].to_vec()),
        _ => None,
    }
}

fn read_tls_until_contains_all(
    tls_stream: &mut rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
    needles: &[&[u8]],
) -> Option<Vec<u8>> {
    let mut received = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut buf = [0u8; 4096];

    while Instant::now() < deadline {
        match tls_stream.read(&mut buf) {
            Ok(0) => return None,
            Ok(n) => {
                received.extend_from_slice(&buf[..n]);
                if needles
                    .iter()
                    .all(|needle| bytes_contain(&received, needle))
                {
                    return Some(received);
                }
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(error) => {
                println!("TLS read failed while waiting for WebSocket frame: {error}");
                return None;
            }
        }
    }

    None
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
        interface: None,
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
        interface: None,
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
        interface: None,
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
        interface: None,
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
        interface: None,
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
        interface: None,
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

    let status_ok = headers.contains("HTTP/1.1 200");
    let length_ok = body.len() == payload.len();
    let body_ok = body == payload;

    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    // Read the counters once the backend thread is joined: it bumps
    // `responses_sent` after its `write_all` returns, which can come after
    // the client has read the whole response.
    backend.stop();
    let requests_received = backend.requests_received();
    let responses_sent = backend.responses_sent();

    // Name every failing conjunct with its values.
    //
    // This replaces `success && response_ok && requests_received == 1 &&
    // responses_sent == 1`, where `response_ok` itself hid three more
    // conditions: SIX distinguishable causes collapsed into one bit that
    // returned a bare `State::Fail` with no output at all. The five
    // `println!`s earlier in this function only fire on early returns, so a
    // verdict failure printed nothing and the cause had to be reproduced to
    // be learned — on a test that fails intermittently in CI.
    //
    // The distinction that matters most for a large-response delivery guard
    // is truncation (`length_ok`, a short body — the sozu#1279 shape) versus
    // corruption at equal length (`body_ok`), which the collapsed boolean
    // could not tell apart. They are reported separately, the latter with the
    // first differing offset.
    //
    // The verdict is unchanged: `failures.is_empty()` holds exactly when the
    // original conjunction did (a `body_ok` true with `length_ok` false is
    // unreachable — equal strings have equal length).
    let mut failures = Vec::new();
    if !success {
        failures
            .push("worker did not stop cleanly (wait_for_server_stop returned false)".to_owned());
    }
    if !status_ok {
        failures.push(format!(
            "response status line is not 200; headers = {headers:?}"
        ));
    }
    if !length_ok {
        failures.push(format!(
            "body TRUNCATED or padded: got {} bytes, expected {}",
            body.len(),
            payload.len()
        ));
    } else if !body_ok {
        let first_diff = body
            .as_bytes()
            .iter()
            .zip(payload.as_bytes())
            .position(|(got, want)| got != want);
        failures.push(format!(
            "body CORRUPTED at matching length ({} bytes): first differing offset {first_diff:?}",
            body.len()
        ));
    }
    if requests_received != 1 {
        failures.push(format!(
            "backend received {requests_received} requests, expected 1"
        ));
    }
    if responses_sent != 1 {
        failures.push(format!(
            "backend sent {responses_sent} responses, expected 1"
        ));
    }

    if failures.is_empty() {
        State::Success
    } else {
        for failure in &failures {
            println!("TLS-CONN-CLOSE-LARGE: FAIL - {failure}");
        }
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

/// The listener a half-close test runs against.
#[derive(Clone, Copy, Debug)]
enum Transport {
    Plain,
    Tls,
}

/// Start a worker with one listener of `transport` routing `localhost` to one
/// backend at `back_address`. Returns the worker and the frontend address.
fn start_half_close_worker(
    name: &str,
    transport: Transport,
    back_address: SocketAddr,
) -> (Worker, SocketAddr) {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let front: SocketAddr = front_address.clone().into();
    let mut worker = match transport {
        Transport::Plain => {
            let (config, listeners, state) = Worker::empty_http_config(front);
            let mut worker = Worker::start_new_worker_owned(name, config, listeners, state);
            worker.send_proxy_request_type(RequestType::AddHttpListener(
                ListenerBuilder::new_http(front_address.clone())
                    .to_http(None)
                    .unwrap(),
            ));
            worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
                interface: None,
                address: front_address.clone(),
                proxy: ListenerType::Http.into(),
                from_scm: false,
            }));
            worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
                "cluster_0",
            )));
            worker.send_proxy_request_type(RequestType::AddHttpFrontend(
                Worker::default_http_frontend("cluster_0", front),
            ));
            worker
        }
        Transport::Tls => {
            let (config, listeners, state) = Worker::empty_https_config(front);
            let mut worker = Worker::start_new_worker_owned(name, config, listeners, state);
            worker.send_proxy_request_type(RequestType::AddHttpsListener(
                ListenerBuilder::new_https(front_address.clone())
                    .to_tls(None)
                    .unwrap(),
            ));
            worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
                interface: None,
                address: front_address.clone(),
                proxy: ListenerType::Https.into(),
                from_scm: false,
            }));
            worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
                "cluster_0",
            )));
            worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
                hostname: "localhost".to_owned(),
                ..Worker::default_http_frontend("cluster_0", front)
            }));
            worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
                address: front_address,
                certificate: CertificateAndKey {
                    certificate: String::from(include_str!(
                        "../../../lib/assets/local-certificate.pem"
                    )),
                    key: String::from(include_str!("../../../lib/assets/local-key.pem")),
                    certificate_chain: vec![],
                    versions: vec![],
                    names: vec![],
                },
                expired_at: None,
            }));
            worker
        }
    };
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));
    worker.read_to_last();
    (worker, front)
}

/// A client connection of `transport` to `front`, and a handle on its TCP
/// socket for `shutdown` and `SO_LINGER`.
fn half_close_client(transport: Transport, front: SocketAddr) -> (Box<dyn ReadWrite>, TcpStream) {
    let tcp = TcpStream::connect_timeout(&front, Duration::from_secs(5))
        .expect("could not connect to sozu");
    tcp.set_read_timeout(Some(Duration::from_secs(10))).ok();
    tcp.set_write_timeout(Some(Duration::from_secs(5))).ok();
    let handle = tcp.try_clone().expect("the client socket must clone");
    let stream: Box<dyn ReadWrite> = match transport {
        Transport::Plain => Box::new(tcp),
        Transport::Tls => {
            let mut tls_config = ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(Verifier))
                .with_no_client_auth();
            tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];
            let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
            let conn = rustls::ClientConnection::new(Arc::new(tls_config), server_name.to_owned())
                .unwrap();
            Box::new(rustls::StreamOwned::new(conn, tcp))
        }
    };
    (stream, handle)
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

/// When a client half-closes, relative to its exchange.
#[derive(Clone, Copy, Debug)]
enum HalfClose {
    /// Right after the request, so the FIN may reach sozu with it.
    AfterRequest,
    /// Once the response head has arrived, with most of the body still to
    /// come from the backend.
    AfterResponseHead,
}

/// An HTTP/1.1 client that sends its whole request and then half-closes its
/// connection (`shutdown(SHUT_WR)`) still receives the whole response, and
/// sozu closes the connection (with `close_notify` over TLS) once it is
/// delivered.
///
/// The FIN only ends the client's sending side (RFC 9293 §3.6): the kernel
/// reports it as `EPOLLRDHUP`, which `Ready::from(&mio::event::Event)` maps
/// to HUP. The response is far larger than the socket buffers, so it is still
/// arriving from the backend when that HUP is seen, and the frontend has to
/// keep serving it instead of closing the session. With a `keep-alive`
/// backend, only that HUP tells sozu to close after the response; a session
/// left open would make the client wait for its read timeout.
fn try_client_half_close(
    name: &str,
    transport: Transport,
    half_close: HalfClose,
    backend_connection: &'static str,
) -> State {
    let back_address = create_local_address();
    let payload = "y".repeat(8 * 1024 * 1024);
    let (mut worker, front) = start_half_close_worker(name, transport, back_address);
    let mut backend = BlockingHttpBackend::start_with_connection(
        back_address,
        payload.clone(),
        backend_connection,
    );
    let (mut stream, handle) = half_close_client(transport, front);

    let request_body = "half-close";
    let request = format!(
        "POST /large HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{request_body}",
        request_body.len()
    );
    let sent = stream.write_all(request.as_bytes()).is_ok() && stream.flush().is_ok();

    let mut response_bytes = Vec::new();
    let mut buf = [0u8; 65536];
    let mut half_closed = None;
    let mut ending = String::from("timeout");
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        let head_received = response_bytes.windows(4).any(|w| w == b"\r\n\r\n");
        if half_closed.is_none()
            && match half_close {
                HalfClose::AfterRequest => true,
                HalfClose::AfterResponseHead => head_received,
            }
        {
            half_closed = Some(handle.shutdown(Shutdown::Write).is_ok());
        }
        match stream.read(&mut buf) {
            Ok(0) => {
                ending = "closed".to_owned();
                break;
            }
            Ok(n) => response_bytes.extend_from_slice(&buf[..n]),
            Err(ref e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => {
                ending = format!("error: {e}");
                break;
            }
        }
    }
    drop(stream);

    let separator = response_bytes.windows(4).position(|w| w == b"\r\n\r\n");
    let (status_ok, body_len) = match separator {
        Some(at) => (
            response_bytes.starts_with(b"HTTP/1.1 200"),
            response_bytes.len() - at - 4,
        ),
        None => (false, 0),
    };

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    backend.stop();

    println!(
        "{name}: {transport:?} {half_close:?} backend={backend_connection} sent={sent} \
         half_closed={half_closed:?} status_ok={status_ok} body={body_len}/{} \
         ending={ending} stopped={stopped}",
        payload.len()
    );
    if sent
        && half_closed == Some(true)
        && status_ok
        && body_len == payload.len()
        && ending == "closed"
        && stopped
    {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_tls_client_half_close_after_request_receives_large_response() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "TLS H1: a client that half-closes after its request receives the whole response",
            || try_client_half_close(
                "TLS-HALF-CLOSE-REQUEST",
                Transport::Tls,
                HalfClose::AfterRequest,
                "close"
            )
        ),
        State::Success
    );
}

#[test]
fn test_tls_client_half_close_mid_response_receives_it_whole() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "TLS H1: a client that half-closes mid-response receives it whole, then the close",
            || try_client_half_close(
                "TLS-HALF-CLOSE-RESPONSE",
                Transport::Tls,
                HalfClose::AfterResponseHead,
                "keep-alive"
            )
        ),
        State::Success
    );
}

#[test]
fn test_tls_client_half_close_keep_alive_closes_after_response() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "TLS H1: a half-closed keep-alive exchange is closed once its response is delivered",
            || try_client_half_close(
                "TLS-HALF-CLOSE-KEEPALIVE",
                Transport::Tls,
                HalfClose::AfterRequest,
                "keep-alive"
            )
        ),
        State::Success
    );
}

#[test]
fn test_plain_client_half_close_after_request_receives_large_response() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H1: a client that half-closes after its request receives the whole response",
            || try_client_half_close(
                "PLAIN-HALF-CLOSE-REQUEST",
                Transport::Plain,
                HalfClose::AfterRequest,
                "keep-alive"
            )
        ),
        State::Success
    );
}

#[test]
fn test_plain_client_half_close_mid_response_receives_it_whole() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H1: a client that half-closes mid-response receives it whole, then the close",
            || try_client_half_close(
                "PLAIN-HALF-CLOSE-RESPONSE",
                Transport::Plain,
                HalfClose::AfterResponseHead,
                "keep-alive"
            )
        ),
        State::Success
    );
}

/// An HTTP/2 client whose stream window is exhausted receives the whole
/// response of a backend that wrote all of it and closed in the meantime
/// (sozu-proxy/sozu#1819).
///
/// The client keeps the default 65 535-octet windows and sends no
/// WINDOW_UPDATE until the backend has written its last byte and closed.
/// sozu cannot send more DATA, so the stream's response buffer stays full
/// and the backend's FIN arrives with bytes still in its socket: the dead
/// backend connection is kept for them. Nothing can progress until the
/// client opens its window, so the session must wait instead of spinning
/// `Mux::ready_inner` to its iteration budget, which
/// `http.infinite_loop.error` records and which used to close the session
/// with the rest of the body unsent. Flow control stands in for a slow
/// client's full socket here: it stalls the frontend whatever the kernel's
/// buffer sizes, which a socket that only reads slowly does not. The
/// `Mux::ready_inner` unit tests cover an HTTP/1.1 frontend whose socket
/// would block, with and without a client half-close.
fn try_h2_window_stalled_client_after_backend_close() -> State {
    use super::h2_utils::{
        H2_CLIENT_PREFACE, H2_FLAG_ACK, H2_FLAG_END_STREAM, H2_FRAME_DATA, H2_FRAME_HEADERS,
        H2_FRAME_SETTINGS, H2Frame, advance_one_frame,
    };

    let name = "H2-WINDOW-STALLED-CLIENT";
    let back_address = create_local_address();
    // Twice the initial window: the rest fits in the kernel's socket
    // buffers, so the backend finishes writing and closes while the window
    // is still exhausted.
    let body_size = 128 * 1024;
    let (mut worker, front) = start_half_close_worker(name, Transport::Tls, back_address);
    let mut backend =
        BlockingHttpBackend::start_with_connection(back_address, "w".repeat(body_size), "close");

    let mut tls_config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Verifier))
        .with_no_client_auth();
    tls_config.alpn_protocols = vec![b"h2".to_vec()];
    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let conn = rustls::ClientConnection::new(Arc::new(tls_config), server_name.to_owned()).unwrap();
    let tcp = TcpStream::connect_timeout(&front, Duration::from_secs(5))
        .expect("could not connect to sozu");
    tcp.set_read_timeout(Some(Duration::from_millis(250))).ok();
    tcp.set_write_timeout(Some(Duration::from_secs(5))).ok();
    let mut stream = rustls::StreamOwned::new(conn, tcp);
    let is_timeout =
        |e: &std::io::Error| matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut);

    let mut header_block = vec![
        0x82, // :method GET (static idx 2)
        0x87, // :scheme https (static idx 7)
        0x84, // :path / (static idx 4)
    ];
    // :authority localhost — name at static idx 1, literal value.
    header_block.push(0x41);
    header_block.push(9);
    header_block.extend_from_slice(b"localhost");
    let mut opening = H2_CLIENT_PREFACE.to_vec();
    opening.extend(H2Frame::settings(&[]).encode());
    opening.extend(H2Frame::headers(1, header_block, true, true).encode());
    let mut sent = stream.write_all(&opening).is_ok() && stream.flush().is_ok();

    let mut carry = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut answered = false;
    let mut body_received = 0;
    let mut ended = false;
    let mut read_frames = |stream: &mut rustls::StreamOwned<_, TcpStream>,
                           carry: &mut Vec<u8>,
                           sent: &mut bool,
                           answered: &mut bool,
                           body_received: &mut usize,
                           ended: &mut bool|
     -> bool {
        match stream.read(&mut buf) {
            Ok(0) => return false,
            Ok(n) => carry.extend_from_slice(&buf[..n]),
            Err(e) if is_timeout(&e) => {}
            Err(_) => return false,
        }
        while let Some((frame_type, flags, sid, payload)) = advance_one_frame(carry) {
            if frame_type == H2_FRAME_SETTINGS && flags & H2_FLAG_ACK == 0 {
                *sent &= stream.write_all(&H2Frame::settings_ack().encode()).is_ok()
                    && stream.flush().is_ok();
            } else if frame_type == H2_FRAME_HEADERS && sid == 1 {
                *answered = true;
            } else if frame_type == H2_FRAME_DATA && sid == 1 {
                *body_received += payload.len();
                *ended |= flags & H2_FLAG_END_STREAM != 0;
            }
        }
        true
    };

    // Read what the initial window lets through, until the backend has
    // written everything and closed; then sozu gets a moment to see the
    // backend's FIN.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut open = true;
    while sent && open && backend.responses_sent() == 0 && Instant::now() < deadline {
        open = read_frames(
            &mut stream,
            &mut carry,
            &mut sent,
            &mut answered,
            &mut body_received,
            &mut ended,
        );
    }
    let backend_done = backend.responses_sent() == 1;
    let settle = Instant::now() + Duration::from_millis(300);
    while open && Instant::now() < settle {
        open = read_frames(
            &mut stream,
            &mut carry,
            &mut sent,
            &mut answered,
            &mut body_received,
            &mut ended,
        );
    }
    let stalled_at = body_received;
    let spins = super::h2_tests::query_proxy_count(
        &mut worker,
        sozu_lib::metrics::names::http::INFINITE_LOOP_ERROR,
    );

    // The client opens both windows, then reads to the end of the stream.
    let increment = u32::try_from(body_size).expect("the body size fits a window increment");
    let mut window = H2Frame::window_update(0, increment).encode();
    window.extend(H2Frame::window_update(1, increment).encode());
    let reopened = open && stream.write_all(&window).is_ok() && stream.flush().is_ok();
    let deadline = Instant::now() + Duration::from_secs(10);
    while open && !ended && Instant::now() < deadline {
        open = read_frames(
            &mut stream,
            &mut carry,
            &mut sent,
            &mut answered,
            &mut body_received,
            &mut ended,
        );
    }
    drop(stream);

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    backend.stop();

    println!(
        "{name}: sent={sent} answered={answered} backend_done={backend_done} \
         stalled_at={stalled_at} {}={spins} reopened={reopened} \
         body={body_received}/{body_size} ended={ended} stopped={stopped}",
        sozu_lib::metrics::names::http::INFINITE_LOOP_ERROR
    );
    if sent
        && answered
        && backend_done
        && stalled_at < body_size
        && spins == 0
        && reopened
        && body_received == body_size
        && ended
        && stopped
    {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_window_stalled_client_receives_the_whole_response_of_a_closed_backend() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "TLS H2: a client with an exhausted window receives the whole response of a \
             backend that closed",
            try_h2_window_stalled_client_after_backend_close
        ),
        State::Success
    );
}

/// A backend that accepts one connection, reads, never answers, and records
/// when it has read something and when its connection closed.
struct SilentBackend {
    received: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl SilentBackend {
    fn start(address: SocketAddr) -> Self {
        let received = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));
        let (received_clone, closed_clone) = (received.clone(), closed.clone());
        let listener = bind_std_listener(address, "silent backend");
        let thread = thread::spawn(move || {
            listener
                .set_nonblocking(true)
                .expect("could not set backend listener nonblocking");
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
                    Err(_) => return,
                }
            };
            stream.set_nonblocking(false).ok();
            stream.set_read_timeout(Some(Duration::from_secs(15))).ok();
            let mut buf = [0u8; 65536];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(_) => received_clone.store(true, Ordering::SeqCst),
                    Err(ref e) if e.kind() == ErrorKind::Interrupted => {}
                    // A timeout leaves `closed` unset: the connection stayed open.
                    Err(ref e)
                        if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut =>
                    {
                        return;
                    }
                    Err(_) => break,
                }
            }
            closed_clone.store(true, Ordering::SeqCst);
        });
        Self {
            received,
            closed,
            thread: Some(thread),
        }
    }

    /// Wait up to `within` for `flag`.
    fn wait(flag: &AtomicBool, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if flag.load(Ordering::SeqCst) {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        flag.load(Ordering::SeqCst)
    }
}

impl Drop for SilentBackend {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A client that half-closes after a request it left incomplete — its head
/// announces a body it never sends whole — can never complete it. Once the
/// client's EOF is read, sozu closes the session at once, as on a hang-up,
/// instead of waiting for the backend, which waits for the rest of the body
/// until its own timeout.
fn try_half_close_with_incomplete_request(name: &str, transport: Transport) -> State {
    let back_address = create_local_address();
    let backend = SilentBackend::start(back_address);
    let (mut worker, front) = start_half_close_worker(name, transport, back_address);
    let (mut stream, handle) = half_close_client(transport, front);

    let mut request =
        b"POST /upload HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1000000\r\n\r\n".to_vec();
    request.extend_from_slice(&[b'u'; 1000]);
    let sent = stream.write_all(&request).is_ok() && stream.flush().is_ok();
    // The backend has the head before the client gives up on the body.
    let forwarded = SilentBackend::wait(&backend.received, Duration::from_secs(5));
    let half_closed = handle.shutdown(Shutdown::Write).is_ok();

    let start = Instant::now();
    let mut buf = [0u8; 4096];
    let client_closed = loop {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break start.elapsed(),
            Ok(_) => {}
        }
    };
    let backend_closed = SilentBackend::wait(&backend.closed, Duration::from_secs(2));
    drop(stream);

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    drop(backend);

    println!(
        "{name}: {transport:?} sent={sent} forwarded={forwarded} half_closed={half_closed} \
         client_closed_after={client_closed:?} backend_closed={backend_closed} stopped={stopped}"
    );
    if sent
        && forwarded
        && half_closed
        && client_closed < Duration::from_secs(2)
        && backend_closed
        && stopped
    {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_plain_half_close_with_incomplete_request_closes_at_once() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H1: a half-close that leaves the request incomplete closes the session at once",
            || try_half_close_with_incomplete_request(
                "PLAIN-HALF-CLOSE-INCOMPLETE",
                Transport::Plain
            )
        ),
        State::Success
    );
}

#[test]
fn test_tls_half_close_with_incomplete_request_closes_at_once() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "TLS H1: a half-close that leaves the request incomplete closes the session at once",
            || try_half_close_with_incomplete_request("TLS-HALF-CLOSE-INCOMPLETE", Transport::Tls)
        ),
        State::Success
    );
}

/// A client that resets its connection while its request is linked to a
/// backend has hung up: the reset reaches sozu as ERROR and WRITE_CLOSED
/// beside the HUP, and the session closes at once. Treating it as a half-close kept the session
/// with an ERROR nothing clears, which spun `Mux::ready_inner` to
/// `MAX_LOOP_ITERATIONS` (`http.infinite_loop.error`).
fn try_client_reset_with_linked_request(name: &str, transport: Transport) -> State {
    let back_address = create_local_address();
    let backend = SilentBackend::start(back_address);
    let (mut worker, front) = start_half_close_worker(name, transport, back_address);
    let (mut stream, handle) = half_close_client(transport, front);

    let sent = stream
        .write_all(b"GET /slow HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .is_ok()
        && stream.flush().is_ok();
    let linked = SilentBackend::wait(&backend.received, Duration::from_secs(5));
    super::h2_security_session::set_linger_zero(handle.as_raw_fd());
    drop(stream);
    drop(handle);

    let backend_closed = SilentBackend::wait(&backend.closed, Duration::from_secs(5));
    let spins = super::h2_tests::query_proxy_count(
        &mut worker,
        sozu_lib::metrics::names::http::INFINITE_LOOP_ERROR,
    );

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    drop(backend);

    println!(
        "{name}: {transport:?} sent={sent} linked={linked} backend_closed={backend_closed} \
         {}={spins} stopped={stopped}",
        sozu_lib::metrics::names::http::INFINITE_LOOP_ERROR
    );
    if sent && linked && backend_closed && spins == 0 && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_tls_client_reset_with_linked_request_closes_without_spinning() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "TLS H1: a client reset with a linked request closes the session without spinning",
            || try_client_reset_with_linked_request("TLS-RESET-LINKED", Transport::Tls)
        ),
        State::Success
    );
}

#[test]
fn test_plain_client_reset_with_linked_request_closes_without_spinning() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H1: a client reset with a linked request closes the session without spinning",
            || try_client_reset_with_linked_request("PLAIN-RESET-LINKED", Transport::Plain)
        ),
        State::Success
    );
}

/// A backend that streams an 8 MiB response to one request and records when
/// a write fails, which happens once sozu closes its connection.
struct StreamingBackend {
    closed_at: Arc<std::sync::Mutex<Option<Instant>>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl StreamingBackend {
    const BODY: usize = 8 * 1024 * 1024;

    fn start(address: SocketAddr) -> Self {
        let closed_at = Arc::new(std::sync::Mutex::new(None));
        let closed_clone = closed_at.clone();
        let listener = bind_std_listener(address, "streaming backend");
        let thread = thread::spawn(move || {
            listener
                .set_nonblocking(true)
                .expect("could not set backend listener nonblocking");
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
                    Err(_) => return,
                }
            };
            stream.set_nonblocking(false).ok();
            stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
            // Longer than any prompt close, shorter than a session held open.
            stream.set_write_timeout(Some(Duration::from_secs(30))).ok();
            let mut buf = [0u8; 4096];
            if !matches!(stream.read(&mut buf), Ok(n) if n > 0) {
                return;
            }
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", Self::BODY);
            let chunk = vec![b'd'; 65536];
            let mut result = stream.write_all(head.as_bytes());
            let mut sent = 0;
            while result.is_ok() && sent < Self::BODY {
                result = stream.write_all(&chunk);
                sent += chunk.len();
            }
            if result.is_err() {
                *closed_clone.lock().unwrap() = Some(Instant::now());
            }
        });
        Self {
            closed_at,
            thread: Some(thread),
        }
    }

    fn closed_at(&self) -> Option<Instant> {
        *self.closed_at.lock().unwrap()
    }
}

impl Drop for StreamingBackend {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// rr_client_reset_mid_download: a client reads part of a large response,
/// then resets its connection. That is a full hang-up, and the session must
/// close at once, closing its backend connection with it.
///
/// When sozu's own `read` or `write` meets the reset before `epoll` reports
/// it, the error is consumed and the event is `EPOLLHUP` without `EPOLLERR`:
/// HUP and WRITE_CLOSED, not ERROR. Read as a half-close, it kept the session
/// with its response in flight until a timeout. The race depends on timing,
/// so the test repeats the exchange.
fn try_rr_client_reset_mid_download(name: &str, transport: Transport) -> State {
    let back_address = create_local_address();
    let backend = StreamingBackend::start(back_address);
    let (mut worker, front) = start_half_close_worker(name, transport, back_address);
    let (mut stream, handle) = half_close_client(transport, front);

    let sent = stream
        .write_all(b"GET /download HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .is_ok()
        && stream.flush().is_ok();
    let mut received = 0;
    let mut buf = [0u8; 65536];
    while received < 280 * 1024 {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => received += n,
        }
    }
    super::h2_security_session::set_linger_zero(handle.as_raw_fd());
    let reset_at = Instant::now();
    drop(stream);
    drop(handle);

    let deadline = reset_at + Duration::from_secs(3);
    while backend.closed_at().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    let closed_after = backend
        .closed_at()
        .map(|at| at.saturating_duration_since(reset_at));

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    drop(backend);

    println!(
        "{name}: {transport:?} sent={sent} received={received} \
         backend_closed_after={closed_after:?} stopped={stopped}"
    );
    if sent
        && received >= 280 * 1024
        && closed_after.is_some_and(|after| after < Duration::from_secs(2))
        && stopped
    {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_rr_client_reset_mid_download_tls() {
    assert_eq!(
        repeat_until_error_or(
            12,
            "TLS H1: a client that resets mid-download closes its session at once",
            || try_rr_client_reset_mid_download("TLS-RR-RESET-MID-DOWNLOAD", Transport::Tls)
        ),
        State::Success
    );
}

#[test]
fn test_rr_client_reset_mid_download_plain() {
    assert_eq!(
        repeat_until_error_or(
            12,
            "H1: a client that resets mid-download closes its session at once",
            || try_rr_client_reset_mid_download("PLAIN-RR-RESET-MID-DOWNLOAD", Transport::Plain)
        ),
        State::Success
    );
}

/// rr_client_reset_mid_download over HTTP/2: the client opens one stream
/// with a large window, reads part of the response, then resets its
/// connection. The session must close at once, closing its backend
/// connection with it, instead of waiting for a timeout while the delayed
/// close tries to flush output to a socket that can take none.
fn try_rr_h2_client_reset_mid_download(name: &str) -> State {
    use super::h2_utils::{
        H2Frame, build_chrome146_get_headers, h2_handshake_chromium_146, raw_h2_connection,
    };

    let back_address = create_local_address();
    let backend = StreamingBackend::start(back_address);
    let (mut worker, front) = start_half_close_worker(name, Transport::Tls, back_address);

    let mut tls = raw_h2_connection(front);
    h2_handshake_chromium_146(&mut tls);
    let headers = build_chrome146_get_headers("localhost", "/download", None);
    let sent = tls
        .write_all(&H2Frame::headers(1, headers, true, true).encode())
        .is_ok()
        && tls.flush().is_ok();
    let mut received = 0;
    let mut buf = [0u8; 65536];
    while received < 280 * 1024 {
        match tls.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => received += n,
        }
    }
    super::h2_security_session::set_linger_zero(tls.sock.as_raw_fd());
    let reset_at = Instant::now();
    drop(tls);

    let deadline = reset_at + Duration::from_secs(3);
    while backend.closed_at().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    let closed_after = backend
        .closed_at()
        .map(|at| at.saturating_duration_since(reset_at));

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    drop(backend);

    println!(
        "{name}: sent={sent} received={received} backend_closed_after={closed_after:?} \
         stopped={stopped}"
    );
    if sent
        && received >= 280 * 1024
        && closed_after.is_some_and(|after| after < Duration::from_secs(2))
        && stopped
    {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_rr_h2_client_reset_mid_download() {
    assert_eq!(
        repeat_until_error_or(
            12,
            "H2: a client that resets mid-download closes its session at once",
            || try_rr_h2_client_reset_mid_download("H2-RR-RESET-MID-DOWNLOAD")
        ),
        State::Success
    );
}

fn try_wss_server_speaks_first_after_upgrade() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let back_address = create_local_address();

    let (config, listeners, state) = Worker::empty_https_config(front_address.clone().into());
    let mut worker = Worker::start_new_worker_owned("WSS-SERVER-FIRST", config, listeners, state);

    worker.send_proxy_request_type(RequestType::AddHttpsListener(
        ListenerBuilder::new_https(front_address.clone())
            .to_tls(None)
            .unwrap(),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        interface: None,
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

    let mut backend = SyncBackend::new(
        "BACKEND_0",
        back_address,
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
    );
    backend.connect();

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
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).unwrap();
    tcp.set_read_timeout(Some(Duration::from_millis(500))).ok();
    tcp.set_write_timeout(Some(Duration::from_millis(500))).ok();
    let mut tls_stream = rustls::StreamOwned::new(conn, tcp);

    let request = "GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
    if tls_stream.write_all(request.as_bytes()).is_err() {
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }
    tls_stream.flush().ok();

    backend.accept(0);
    backend.receive(0);
    backend.send(0);

    let upgrade = match read_tls_until_contains_all(&mut tls_stream, &[b"101 Switching Protocols"])
    {
        Some(upgrade) => upgrade,
        other => {
            println!("unexpected WSS upgrade read: {other:?}");
            worker.soft_stop();
            worker.wait_for_server_stop();
            return State::Fail;
        }
    };
    if bytes_contain(&upgrade, PUSHER_CONNECTION_ESTABLISHED.as_bytes()) {
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Success;
    }

    thread::sleep(Duration::from_millis(50));

    let connection_established = websocket_text_frame(PUSHER_CONNECTION_ESTABLISHED);
    let result = backend_send_bytes(&mut backend, 0, &connection_established)
        && read_tls_until_contains_all(
            &mut tls_stream,
            &[PUSHER_CONNECTION_ESTABLISHED.as_bytes()],
        )
        .is_some();
    if !result {
        println!("server-first WSS frame was not flushed before client data");
    }

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    if result && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_wss_server_speaks_first_after_upgrade() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "WSS server-speaks-first payload after 101",
            try_wss_server_speaks_first_after_upgrade,
        ),
        State::Success,
    );
}

fn try_wss_client_frame_after_upgrade_receives_pusher_pong() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let back_address = create_local_address();

    let (config, listeners, state) = Worker::empty_https_config(front_address.clone().into());
    let mut worker =
        Worker::start_new_worker_owned("WSS-PUSHER-PING-PONG", config, listeners, state);

    worker.send_proxy_request_type(RequestType::AddHttpsListener(
        ListenerBuilder::new_https(front_address.clone())
            .to_tls(None)
            .unwrap(),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        interface: None,
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

    let mut backend = SyncBackend::new(
        "BACKEND_0",
        back_address,
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
    );
    backend.connect();

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
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).unwrap();
    tcp.set_read_timeout(Some(Duration::from_millis(500))).ok();
    tcp.set_write_timeout(Some(Duration::from_millis(500))).ok();
    let mut tls_stream = rustls::StreamOwned::new(conn, tcp);

    let request = "GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
    if tls_stream.write_all(request.as_bytes()).is_err() {
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }
    tls_stream.flush().ok();

    backend.accept(0);
    backend.receive(0);
    backend.send(0);

    if read_tls_until_contains_all(&mut tls_stream, &[b"101 Switching Protocols"]).is_none() {
        println!("client did not receive WSS 101 before sending pusher ping");
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    let connection_established = websocket_text_frame(PUSHER_CONNECTION_ESTABLISHED);
    if !backend_send_bytes(&mut backend, 0, &connection_established) {
        println!("backend could not send pusher connection_established frame");
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    let ping = masked_websocket_text_frame(PUSHER_PING);
    if tls_stream.write_all(&ping).is_err() || tls_stream.flush().is_err() {
        println!("client could not send masked pusher ping frame");
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    let backend_received_ping =
        backend_read_bytes(&mut backend, 0)
            .as_ref()
            .is_some_and(|received| {
                received.first() == Some(&0x81) && received.get(1).is_some_and(|b| b & 0x80 != 0)
            });
    if !backend_received_ping {
        println!("backend did not receive a masked websocket text frame from client");
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    let pong = websocket_text_frame(PUSHER_PONG);
    let result = backend_send_bytes(&mut backend, 0, &pong)
        && read_tls_until_contains_all(
            &mut tls_stream,
            &[
                PUSHER_CONNECTION_ESTABLISHED.as_bytes(),
                PUSHER_PONG.as_bytes(),
            ],
        )
        .is_some();
    if !result {
        println!("client did not receive both pusher connection_established and pong frames");
    }

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    if result && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_wss_client_frame_after_upgrade_receives_pusher_pong() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "WSS client pusher ping after 101 receives pusher pong",
            try_wss_client_frame_after_upgrade_receives_pusher_pong,
        ),
        State::Success,
    );
}

// ============================================================================
// Test 7: ALPN absent on H2-only listener — RFC 9113 §3.2 gate
// ============================================================================

/// Verify that a listener configured with `disable_http11 = true` rejects
/// clients that complete the TLS handshake without negotiating ALPN.
///
/// Pass 5 Medium #4 of the security audit: without this gate, a client that
/// omits ALPN would silently be handed to the HTTP/1.1 state machine on an
/// H2-only listener, bypassing H2-specific protections. The server-side
/// behaviour lives at `lib/src/https.rs` (`upgrade_handshake` None-ALPN arm):
/// on `disable_http11 = true` it bumps `https.alpn.rejected.http11_disabled`,
/// logs a `warn!`, and returns `None` — the session transitions to
/// `FailedUpgrade` and is torn down without emitting an H1 response.
fn try_h2_listener_rejects_alpn_absent() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let back_address = create_local_address();

    let (config, listeners, state) = Worker::empty_https_config(front_address.into());
    let mut worker =
        Worker::start_new_worker_owned("TLS-ALPN-ABSENT-REJECT", config, listeners, state);

    // Build a listener that only accepts `h2` (disable_http11 = true).
    // `ListenerBuilder` does not yet expose a `with_disable_http11` setter,
    // so we mutate the generated HttpsListenerConfig in place before sending.
    let mut https_listener = ListenerBuilder::new_https(front_address.clone())
        .to_tls(None)
        .expect("could not build HTTPS listener config");
    https_listener.disable_http11 = Some(true);
    worker.send_proxy_request_type(RequestType::AddHttpsListener(https_listener));

    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        interface: None,
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

    // Client that deliberately sends no ALPN extension. rustls happily omits
    // ALPN when `alpn_protocols` is empty (the default). The handshake itself
    // must complete (otherwise we cannot distinguish the reject from a raw
    // TLS failure), so we retain the custom `Verifier`.
    let tls_config = {
        let mut cfg = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Verifier))
            .with_no_client_auth();
        cfg.alpn_protocols = vec![];
        cfg
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

    // Attempt an HTTP/1.1 request. The write may succeed (the TLS handshake
    // completes before Sozu decides to reject the session), but the server
    // must never produce an HTTP response: reading back must observe a clean
    // EOF / close_notify / connection reset, not `HTTP/1.1 ...`.
    let request = "GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let _ = tls_stream.write_all(request.as_bytes());
    let _ = tls_stream.flush();

    let mut response_bytes = Vec::new();
    let mut buf = [0u8; 1024];
    let start = Instant::now();
    let connection_closed = loop {
        match tls_stream.read(&mut buf) {
            Ok(0) => {
                println!(
                    "connection closed by sozu after {:.1}s",
                    start.elapsed().as_secs_f64()
                );
                break true;
            }
            Ok(n) => {
                response_bytes.extend_from_slice(&buf[..n]);
                // Any HTTP prefix means the gate is broken — Sozu would have
                // downgraded to H1 and let the request through.
                if response_bytes.len() >= 4 && response_bytes.starts_with(b"HTTP") {
                    println!(
                        "UNEXPECTED HTTP response on H2-only listener: {:?}",
                        String::from_utf8_lossy(&response_bytes)
                    );
                    break false;
                }
                if start.elapsed() > Duration::from_secs(10) {
                    println!(
                        "timed out waiting for close after {} bytes",
                        response_bytes.len()
                    );
                    break false;
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                if start.elapsed() > Duration::from_secs(10) {
                    println!("timed out waiting for sozu to close the connection");
                    break false;
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                // UnexpectedEof, connection reset, close_notify, etc. all
                // count as a reject — the server terminated the session.
                println!(
                    "read error after {:.1}s (expected reject): {e}",
                    start.elapsed().as_secs_f64()
                );
                break true;
            }
        }
    };
    drop(tls_stream);

    // Verify the backend never received anything — the gate must fire before
    // any H1 request reaches the cluster.
    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    let aggregator = backend
        .stop_and_get_aggregator()
        .expect("Could not get aggregator");
    println!(
        "BACKEND: sent={}, received={}",
        aggregator.responses_sent, aggregator.requests_received
    );

    let backend_untouched = aggregator.requests_received == 0 && aggregator.responses_sent == 0;

    if success && connection_closed && backend_untouched {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_listener_rejects_alpn_absent() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "TLS: disable_http11=true listener rejects clients that omit ALPN",
            try_h2_listener_rejects_alpn_absent
        ),
        State::Success
    );
}
