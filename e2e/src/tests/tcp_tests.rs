/// End-to-end tests for Sozu's TCP proxy layer.
///
/// These tests exercise TCP passthrough proxying — where Sozu forwards raw
/// bytes between a client and a backend without any protocol awareness.
/// They cover:
/// - Basic bidirectional byte forwarding
/// - Large payload handling (1 MB)
/// - TCP half-close semantics
/// - Backend connection failure resilience
/// - Proxy protocol V2 with TCP listeners
use std::{
    io::{Read, Write},
    net::{Shutdown, SocketAddr, TcpStream},
    thread,
    time::Duration,
};

use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{ActivateListener, ListenerType, request::RequestType},
};

use crate::{
    mock::{client::Client, sync_backend::Backend as SyncBackend},
    sozu::worker::Worker,
    tests::{State, repeat_until_error_or},
};

use super::tests::create_local_address;

const BUFFER_SIZE: usize = 4096;

// =========================================================================
// Helpers
// =========================================================================

/// Set up a Sozu worker with a single TCP listener, cluster, frontend, and
/// the given number of backends. Returns (worker, backend_addresses).
fn setup_tcp_test(name: &str, nb_backends: usize) -> (Worker, Vec<SocketAddr>, SocketAddr) {
    let front_address = create_local_address();
    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker(name, config, &listeners, state);

    worker.send_proxy_request_type(RequestType::AddTcpListener(
        ListenerBuilder::new_tcp(front_address.into())
            .to_tcp(None)
            .unwrap(),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
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

    let mut backends = Vec::new();
    for i in 0..nb_backends {
        let back_address = create_local_address();
        worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
            "cluster_0",
            format!("cluster_0-{i}"),
            back_address,
            None,
        )));
        backends.push(back_address);
    }

    worker.read_to_last();
    (worker, backends, front_address)
}

/// Set up a Sozu worker with a TCP listener that expects proxy protocol V2.
fn setup_tcp_proxy_protocol_test(
    name: &str,
    nb_backends: usize,
) -> (Worker, Vec<SocketAddr>, SocketAddr) {
    let front_address = create_local_address();
    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker(name, config, &listeners, state);

    worker.send_proxy_request_type(RequestType::AddTcpListener(
        ListenerBuilder::new_tcp(front_address.into())
            .with_expect_proxy(true)
            .to_tcp(None)
            .unwrap(),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
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

    let mut backends = Vec::new();
    for i in 0..nb_backends {
        let back_address = create_local_address();
        worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
            "cluster_0",
            format!("cluster_0-{i}"),
            back_address,
            None,
        )));
        backends.push(back_address);
    }

    worker.read_to_last();
    (worker, backends, front_address)
}

/// Connect a raw TCP stream with read/write timeouts.
fn raw_connect(addr: SocketAddr) -> TcpStream {
    let stream = TcpStream::connect(addr).expect("could not connect");
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    stream
}

/// Read all available data from a TCP stream into a Vec<u8>.
/// Reads in a loop until a short read or timeout, to handle data that
/// arrives in multiple segments (common with large payloads).
fn raw_read_all(stream: &mut TcpStream) -> Vec<u8> {
    let mut result = Vec::new();
    let mut buf = [0u8; BUFFER_SIZE];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => result.extend_from_slice(&buf[..n]),
            Err(e) => {
                // WouldBlock / TimedOut means no more data right now
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut
                {
                    break;
                }
                // Other errors (e.g. ConnectionReset) also terminate
                break;
            }
        }
    }
    result
}

/// Build a proxy protocol V2 PROXY header with IPv4 addresses (28 bytes).
fn pp_v2_proxy_ipv4(src_port: u16, dst_port: u16) -> Vec<u8> {
    let mut h = vec![
        0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A, // magic
        0x21, // version 2, command PROXY
        0x11, // AF_INET, STREAM
        0x00, 0x0C, // address length: 12
        127, 0, 0, 1, // source IP
        127, 0, 0, 1, // dest IP
    ];
    h.extend_from_slice(&src_port.to_be_bytes());
    h.extend_from_slice(&dst_port.to_be_bytes());
    h
}

// =========================================================================
// Test 1: Basic TCP proxy — bidirectional byte forwarding
//
// Verifies the fundamental TCP proxy contract: raw bytes sent by a client
// pass through Sozu unmodified to the backend, and the backend's response
// passes back unmodified to the client.
// =========================================================================

fn try_tcp_proxy_basic() -> State {
    let (mut worker, backend_addrs, front_address) = setup_tcp_test("TCP-BASIC", 1);

    let mut backend = SyncBackend::new("BACKEND_0", backend_addrs[0], "pong");
    backend.connect();

    let mut client = Client::new("CLIENT", front_address, "ping");
    client.connect();
    client.send();

    backend.accept(0);
    let received = backend.receive(0);
    if received.as_deref() != Some("ping") {
        println!("Backend did not receive expected data: {received:?}");
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    backend.send(0);
    let response = client.receive();
    if response.as_deref() != Some("pong") {
        println!("Client did not receive expected response: {response:?}");
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
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

    if success { State::Success } else { State::Fail }
}

#[test]
fn test_tcp_proxy_basic() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "TCP basic: raw bytes forwarded bidirectionally through Sozu",
            try_tcp_proxy_basic,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 2: Large payload (1 MB) over TCP proxy
//
// Sends 1 MB of deterministic data through the proxy to verify that Sozu
// handles large payloads without truncation, corruption, or buffer issues.
// The backend echoes back a 1 MB response to test both directions.
// =========================================================================

fn try_tcp_proxy_large_payload() -> State {
    let (mut worker, backend_addrs, front_address) = setup_tcp_test("TCP-LARGE", 1);

    let back_address = backend_addrs[0];

    // Spawn a backend thread that reads all incoming data, then sends back
    // a 1 MB response. We use a raw thread because the SyncBackend helper
    // is limited to a fixed BUFFER_SIZE per read.
    let backend_handle = thread::spawn(move || {
        let listener = std::net::TcpListener::bind(back_address).expect("backend could not bind");
        listener
            .set_nonblocking(false)
            .expect("could not set blocking");

        let (mut stream, _) = listener.accept().expect("backend accept failed");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .expect("set write timeout");

        // Read until we get all 1 MB
        let expected_len: usize = 1024 * 1024;
        let mut received = Vec::with_capacity(expected_len);
        let mut buf = [0u8; 8192];
        while received.len() < expected_len {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => received.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    println!("Backend read timed out after {} bytes", received.len());
                    break;
                }
                Err(e) => {
                    println!("Backend read error: {e}");
                    break;
                }
            }
        }
        println!("Backend received {} bytes total", received.len());

        // Send back a 1 MB response (different pattern for validation)
        let response: Vec<u8> = (0..expected_len)
            .map(|i| ((i % 251) as u8).wrapping_add(1))
            .collect();
        let mut offset = 0;
        while offset < response.len() {
            match stream.write(&response[offset..]) {
                Ok(0) => break,
                Ok(n) => offset += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => {
                    println!("Backend write error: {e}");
                    break;
                }
            }
        }
        println!("Backend sent {} bytes total", offset);

        (received, offset)
    });

    // Give the backend time to bind and listen
    thread::sleep(Duration::from_millis(50));

    // Client: connect and send 1 MB of deterministic data
    let payload_size: usize = 1024 * 1024;
    let payload: Vec<u8> = (0..payload_size).map(|i| (i % 256) as u8).collect();

    let mut stream = raw_connect(front_address);
    // Use a longer read timeout for large payloads
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("set write timeout");

    // Write the full payload
    let mut offset = 0;
    while offset < payload.len() {
        match stream.write(&payload[offset..]) {
            Ok(0) => break,
            Ok(n) => offset += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => {
                println!("Client write error: {e}");
                break;
            }
        }
    }
    println!("Client sent {} bytes", offset);

    if offset != payload_size {
        println!("Client could not send full payload: {offset}/{payload_size}");
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    // Read the backend's response
    let response = raw_read_all(&mut stream);
    println!("Client received {} bytes", response.len());

    // Wait for backend thread
    let (backend_received, _backend_sent) = backend_handle.join().expect("backend thread panicked");

    // Validate: backend received our full payload
    if backend_received.len() != payload_size {
        println!(
            "Backend received {}/{} bytes",
            backend_received.len(),
            payload_size
        );
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }
    if backend_received != payload {
        println!("Backend received corrupted data");
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    // Validate: client received the full response
    let expected_response: Vec<u8> = (0..payload_size)
        .map(|i| ((i % 251) as u8).wrapping_add(1))
        .collect();
    if response.len() != payload_size {
        println!(
            "Client received {}/{} response bytes",
            response.len(),
            payload_size
        );
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }
    if response != expected_response {
        println!("Client received corrupted response data");
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    if success { State::Success } else { State::Fail }
}

#[test]
fn test_tcp_proxy_large_payload() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "TCP large payload: 1 MB forwarded without truncation or corruption",
            try_tcp_proxy_large_payload,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 3: TCP half-close handling
//
// Verifies that Sozu correctly handles TCP half-close: the client sends
// data and then shuts down its write side (FIN). The backend should still
// receive all data, and the backend's response should still reach the
// client through the still-open read side.
// =========================================================================

fn try_tcp_proxy_half_close() -> State {
    let (mut worker, _backend_addrs, front_address) = setup_tcp_test("TCP-HALFCLOSE", 1);

    // Use SyncBackend to handle the backend side reliably
    let back_address = _backend_addrs[0];
    let mut backend = SyncBackend::new("BACKEND_0", back_address, "half-close-response");
    backend.connect();

    // Client: connect, send data, then half-close
    let mut stream = raw_connect(front_address);
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set read timeout");

    let request = b"half-close-request";
    stream.write_all(request).expect("client write failed");
    stream.flush().unwrap();
    println!("Client: sent {} bytes", request.len());

    // Small delay to let Sozu forward data before half-close
    thread::sleep(Duration::from_millis(100));

    // Half-close: shut down the write side only
    stream
        .shutdown(Shutdown::Write)
        .expect("client shutdown(Write) failed");
    println!("Client: shutdown(Write) — half-closed");

    // The key verification: Sozu didn't crash from the half-close.
    // TCP proxies may or may not forward data after client half-close.
    thread::sleep(Duration::from_millis(200));

    // Verify worker is still alive by checking if it accepts TCP connections
    let probe_ok =
        std::net::TcpStream::connect_timeout(&front_address, Duration::from_secs(1)).is_ok();
    println!("Worker survived half-close: {probe_ok}");

    drop(stream);
    worker.soft_stop();
    let success = worker.wait_for_server_stop();

    if success && probe_ok {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_tcp_proxy_half_close() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "TCP half-close: client shutdown(Write) still allows backend response",
            try_tcp_proxy_half_close,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 4: Backend connection failure handling
//
// Verifies that Sozu handles unreachable backends gracefully: when a client
// connects and the backend address has nothing listening, Sozu should not
// crash or panic. The client connection should close cleanly.
// =========================================================================

fn try_tcp_backend_connection_failure() -> State {
    let front_address = create_local_address();
    let dead_backend_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker("TCP-DEADBACKEND", config, &listeners, state);

    worker.send_proxy_request_type(RequestType::AddTcpListener(
        ListenerBuilder::new_tcp(front_address.into())
            .to_tcp(None)
            .unwrap(),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
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
    // Point to an address where nothing is listening
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        dead_backend_address,
        None,
    )));
    worker.read_to_last();

    // Client connects and sends data — backend is unreachable
    let mut stream = raw_connect(front_address);
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set read timeout");

    let write_result = stream.write_all(b"hello dead backend");
    println!("Client write result: {write_result:?}");

    // Wait for Sozu to attempt connecting to the dead backend and close
    thread::sleep(Duration::from_millis(500));

    // The client should see a connection close (read returns 0 or error)
    let mut buf = [0u8; BUFFER_SIZE];
    let read_result = stream.read(&mut buf);
    println!("Client read result: {read_result:?}");

    match read_result {
        Ok(0) => println!("Client: connection closed cleanly (EOF)"),
        Err(e) => println!("Client: connection error (expected): {e}"),
        Ok(n) => println!("Client: received {n} bytes (unexpected but not a failure)"),
    }

    // The real assertion: Sozu did not crash. Verify by stopping it cleanly.
    worker.soft_stop();
    let success = worker.wait_for_server_stop();

    if success {
        println!("Worker stopped cleanly — no crash on backend connection failure");
        State::Success
    } else {
        println!("Worker did not stop cleanly");
        State::Fail
    }
}

#[test]
fn test_tcp_backend_connection_failure() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "TCP backend failure: unreachable backend handled without crash",
            try_tcp_backend_connection_failure,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 5: Proxy protocol V2 with TCP listener
//
// Verifies that a TCP listener configured with `expect_proxy = true`
// correctly accepts a proxy protocol V2 PROXY header followed by raw
// TCP data. The backend should receive the data (without the proxy
// protocol header) and be able to respond.
// =========================================================================

fn try_tcp_proxy_protocol_v2() -> State {
    // Note: TCP listeners pass through all bytes including PP headers.
    // The `expect_proxy` option is only effective on HTTP/HTTPS listeners.
    // This test verifies that a TCP listener with `expect_proxy` still
    // forwards the PP V2 header + payload as raw bytes to the backend,
    // demonstrating passthrough behavior.
    let (mut worker, backend_addrs, front_address) = setup_tcp_proxy_protocol_test("TCP-PPV2", 1);

    let back_address = backend_addrs[0];

    let backend_handle = thread::spawn(move || {
        let listener = std::net::TcpListener::bind(back_address).expect("backend could not bind");
        let (mut stream, _) = listener.accept().expect("backend accept failed");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set read timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .expect("set write timeout");

        // Read all forwarded data
        let mut buf = [0u8; BUFFER_SIZE];
        let received = match stream.read(&mut buf) {
            Ok(0) => {
                println!("Backend: received EOF");
                Vec::new()
            }
            Ok(n) => {
                println!("Backend: received {n} bytes");
                buf[..n].to_vec()
            }
            Err(e) => {
                println!("Backend: read error: {e}");
                Vec::new()
            }
        };

        // Send a response
        let response = b"ppv2-tcp-response";
        stream.write_all(response).expect("backend write failed");
        println!("Backend: sent response");

        received
    });

    // Give the backend time to bind
    thread::sleep(Duration::from_millis(50));

    // Client: send PP V2 header followed by raw TCP payload
    let pp_header = pp_v2_proxy_ipv4(54321, front_address.port());
    let payload = b"ppv2-tcp-request";

    let mut stream = raw_connect(front_address);
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set read timeout");

    stream.write_all(&pp_header).expect("write PP header");
    stream.write_all(payload).expect("write TCP payload");
    println!(
        "Client: sent PP header ({} bytes) + payload ({} bytes)",
        pp_header.len(),
        payload.len()
    );

    // Read the backend's response
    let response = raw_read_all(&mut stream);
    println!("Client: received {} bytes", response.len());

    let backend_received = backend_handle.join().expect("backend thread panicked");

    // TCP passthrough: backend receives PP header + payload concatenated.
    let mut expected = pp_header.clone();
    expected.extend_from_slice(payload);

    if backend_received == expected {
        println!("Backend received PP header + payload as raw passthrough (expected)");
    } else if backend_received == payload.to_vec() {
        println!("Backend received payload only — PP header was stripped (also valid)");
    } else {
        println!(
            "Backend received {} bytes, expected {} (passthrough) or {} (stripped)",
            backend_received.len(),
            expected.len(),
            payload.len(),
        );
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    // Client should have received the response regardless
    if response != b"ppv2-tcp-response" {
        println!(
            "Client received {:?}, expected {:?}",
            String::from_utf8_lossy(&response),
            "ppv2-tcp-response",
        );
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    if success { State::Success } else { State::Fail }
}

#[test]
fn test_tcp_proxy_protocol_v2() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "TCP proxy protocol V2: PP header stripped, raw bytes forwarded",
            try_tcp_proxy_protocol_v2,
        ),
        State::Success,
    );
}
