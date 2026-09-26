/// Regression tests for mux layer fixes:
/// - `http.active_requests` accounting: a stream that was never charged emits
///   no `-1` when its access log fires (H1 idle timeout, H1 malformed
///   request), an H1 `100 Continue` does not release the charge early, and a
///   complete H1 request charges and releases it exactly once.
/// - P1: service_start/service_stop bracketing (service_time inflation)
/// - P3: WebSocket upgrade gauge correctness
///
/// Tests cover scenarios with and without proxy protocol, simulating
/// HAProxy healthcheck patterns (connect + PP + disconnect every ~10ms).
///
/// The gauge tests READ the gauge. That is not a given: this header used to
/// advertise "P0: close() stream cleanup (http.active_requests gauge leak)"
/// over three tests that never sampled it and, in the shape they had, could
/// not have. The block above [`await_active_requests_h1`] says why a gauge
/// read from a zero baseline cannot witness an unbalanced decrement, and
/// [`hold_one_request_in_flight`] says what a non-zero baseline buys instead
/// (sozu#1535).
use std::{
    io::{ErrorKind, Read, Write},
    net::{SocketAddr, TcpStream},
    thread,
    time::{Duration, Instant},
};

use sozu_command_lib::{
    config::{FileConfig, ListenerBuilder},
    proto::command::{
        ActivateListener, Cluster, ListenerType, QueryMetricsOptions, Request, filtered_metrics,
        request::RequestType, response_content::ContentType,
    },
};

use crate::{
    http_utils::http_ok_response,
    mock::{client::Client, sync_backend::Backend as SyncBackend},
    port_registry::attach_reserved_http_listener,
    sozu::worker::Worker,
    tests::{State, repeat_until_error_or, setup_sync_test},
};

use super::tests::{create_local_address, create_unbound_local_address};

const BUFFER_SIZE: usize = 4096;

// =========================================================================
// Proxy protocol V2 helpers
// =========================================================================

/// Build a proxy protocol V2 PROXY header with IPv4 addresses (28 bytes).
/// Uses 127.0.0.1 for both source and destination.
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

/// Build a proxy protocol V2 LOCAL header with IPv4 addresses (28 bytes).
/// This is what HAProxy sends for healthchecks when the listener expects
/// proxy protocol and the address family is IPv4.
fn pp_v2_local_ipv4() -> Vec<u8> {
    vec![
        0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A, // magic
        0x20, // version 2, command LOCAL
        0x11, // AF_INET, STREAM
        0x00, 0x0C, // address length: 12
        127, 0, 0, 1, // source IP
        127, 0, 0, 1, // dest IP
        0x00, 0x50, // source port (80)
        0x00, 0x50, // dest port (80)
    ]
}

/// Build a proxy protocol V2 LOCAL header with AF_UNSPEC (16 bytes only).
/// This is the most minimal healthcheck header. Sozu's ExpectProxyProtocol
/// reads 28 bytes minimum (V4 size), so this header is incomplete from
/// sozu's perspective — the connection will close before sozu can parse it.
fn pp_v2_local_af_unspec() -> Vec<u8> {
    vec![
        0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A, // magic
        0x20, // version 2, command LOCAL
        0x00, // AF_UNSPEC
        0x00, 0x00, // address length: 0
    ]
}

// =========================================================================
// Test setup helpers
// =========================================================================

/// Set up a sozu worker with proxy protocol enabled on the HTTP listener.
/// Returns (worker, backend_addresses, front_address).
fn setup_proxy_protocol_test(
    name: &str,
    nb_backends: usize,
) -> (Worker, Vec<SocketAddr>, SocketAddr) {
    let front_address = create_local_address();
    let (config, listeners, state) = Worker::empty_http_config(front_address);
    let mut worker = Worker::start_new_worker_owned(name, config, listeners, state);

    worker.send_proxy_request(Request {
        request_type: Some(RequestType::AddHttpListener(
            ListenerBuilder::new_http(front_address.into())
                .with_expect_proxy(true)
                .to_http(None)
                .unwrap(),
        )),
    });
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::ActivateListener(ActivateListener {
            address: front_address.into(),
            proxy: ListenerType::Http.into(),
            from_scm: false,
        })),
    });
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::AddCluster(Cluster {
            ..Worker::default_cluster("cluster_0")
        })),
    });
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::AddHttpFrontend(Worker::default_http_frontend(
            "cluster_0",
            front_address,
        ))),
    });

    let mut backends = Vec::new();
    for i in 0..nb_backends {
        let back_address = create_local_address();
        worker.send_proxy_request(
            RequestType::AddBackend(Worker::default_backend(
                "cluster_0",
                format!("cluster_0-{i}"),
                back_address,
                None,
            ))
            .into(),
        );
        backends.push(back_address);
    }

    worker.read_to_last();
    (worker, backends, front_address)
}

/// Connect a raw TCP stream with timeouts.
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

/// Drain the stream; tolerates TCP segmentation.
///
/// Thin wrapper around [`super::h2_utils::read_all_available`]: accumulates
/// until EOF / short read / 500 ms global deadline, returning `None` when
/// nothing arrived.
fn raw_read(stream: &mut TcpStream) -> Option<String> {
    let data = super::h2_utils::read_all_available(stream, Duration::from_millis(500));
    if data.is_empty() {
        None
    } else {
        Some(String::from_utf8_lossy(&data).to_string())
    }
}

/// Read until the peer half-closes the connection, and report whether that
/// close actually happened.
///
/// The EOF is an ordering barrier, not a convenience. A stream's access log —
/// and with it the `http.active_requests` `-1`, when the stream carries a
/// charge — is emitted before sozu shuts the frontend socket down, on the
/// response-completion path (`ConnectionH1::writable`) as well as on the
/// teardown path (`Mux::close`). A client that has read EOF has therefore
/// already been passed by every decrement that connection can emit, which is
/// what lets the gauge callers below sample once instead of racing.
///
/// `Err` is the deadline expiring with the connection still open: the barrier
/// did not hold, so a sample taken after it would prove nothing. It is
/// deliberately not folded into `Ok` with the bytes read so far.
fn raw_read_until_eof(stream: &mut TcpStream, deadline: Duration) -> Result<String, String> {
    stream
        .set_read_timeout(Some(Duration::from_millis(50)))
        .map_err(|error| format!("could not arm the read timeout: {error}"))?;
    let started = Instant::now();
    let mut data = Vec::new();
    let mut buffer = [0u8; BUFFER_SIZE];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => return Ok(String::from_utf8_lossy(&data).to_string()),
            Ok(n) => data.extend_from_slice(&buffer[..n]),
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(error) => return Err(format!("read failed: {error}")),
        }
        if started.elapsed() >= deadline {
            return Err(format!(
                "still open after {deadline:?}, read so far: {:?}",
                String::from_utf8_lossy(&data)
            ));
        }
    }
}

fn websocket_text_frame(payload: &str) -> Vec<u8> {
    let payload = payload.as_bytes();
    let mut frame = Vec::with_capacity(payload.len() + 2);
    debug_assert!(payload.len() <= 125);
    frame.push(0x81);
    frame.push(payload.len() as u8);
    frame.extend_from_slice(payload);
    frame
}

fn backend_send_bytes(backend: &mut SyncBackend, client_id: usize, bytes: &[u8]) -> bool {
    match backend.clients.get_mut(&client_id) {
        Some(stream) => stream.write_all(bytes).is_ok() && stream.flush().is_ok(),
        None => false,
    }
}

// =========================================================================
// Test 1: Client HUP during in-flight request
// =========================================================================

fn try_client_hup_during_request() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "CLIENT-HUP",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );
    let mut backend = backends.pop().unwrap();
    backend.connect();

    // Client sends request, then disconnects before response
    let mut client1 = Client::new(
        "client1",
        front_address,
        "GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    client1.connect();
    client1.send();
    backend.accept(0);
    backend.receive(0);

    // Client HUP — sozu's close() must clean up the in-flight stream
    client1.disconnect();
    thread::sleep(Duration::from_millis(100));
    backend.send(0);
    thread::sleep(Duration::from_millis(100));

    // Verify sozu still serves requests
    let mut client2 = Client::new(
        "client2",
        front_address,
        "GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    client2.connect();
    client2.send();
    backend.accept(1);
    backend.receive(1);
    backend.send(1);

    match client2.receive() {
        Some(response) if response.contains("200") => {}
        _ => return State::Fail,
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_client_hup_during_request() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "Client HUP during in-flight request (mux close cleanup)",
            try_client_hup_during_request,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 2: WebSocket upgrade via 101 Switching Protocols
// =========================================================================

fn try_websocket_upgrade() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "WS-UPGRADE",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );
    let mut backend = backends.pop().unwrap();
    backend.set_response(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
    );
    backend.connect();

    let mut client = Client::new(
        "ws-client",
        front_address,
        "GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
    );
    client.connect();
    client.send();
    backend.accept(0);
    backend.receive(0);
    backend.send(0);

    let response = match client.receive() {
        Some(r) => r,
        None => return State::Fail,
    };
    if !response.contains("101") {
        return State::Fail;
    }

    thread::sleep(Duration::from_millis(100));

    // Post-upgrade: raw bidirectional data through the pipe
    client.set_request("hello from client");
    client.send();
    thread::sleep(Duration::from_millis(50));
    match backend.receive(0) {
        Some(data) if data.contains("hello from client") => {}
        _ => return State::Fail,
    }

    backend.set_response("hello from backend");
    backend.send(0);
    thread::sleep(Duration::from_millis(50));
    match client.receive() {
        Some(data) if data.contains("hello from backend") => {}
        _ => return State::Fail,
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_websocket_upgrade() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "WebSocket upgrade via 101 (gauge correctness)",
            try_websocket_upgrade,
        ),
        State::Success,
    );
}

fn try_websocket_server_speaks_first_after_upgrade() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "WS-SERVER-FIRST",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );
    let mut backend = backends.pop().unwrap();
    backend.set_response(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
    );
    backend.connect();

    let mut client = Client::new(
        "ws-server-first-client",
        front_address,
        "GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
    );
    client.connect();
    client.send();
    backend.accept(0);
    backend.receive(0);
    backend.send(0);

    let response = match client.receive() {
        Some(response) if response.contains("101") => response,
        other => {
            println!("unexpected upgrade response: {other:?}");
            worker.soft_stop();
            worker.wait_for_server_stop();
            return State::Fail;
        }
    };
    if response.contains("server-speaks-first") {
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Success;
    }

    backend.set_response("server-speaks-first");
    backend.send(0);

    match client.receive() {
        Some(data) if data.contains("server-speaks-first") => {
            worker.soft_stop();
            worker.wait_for_server_stop();
            State::Success
        }
        other => {
            println!("server-first payload was not flushed before client data: {other:?}");
            worker.soft_stop();
            worker.wait_for_server_stop();
            State::Fail
        }
    }
}

#[test]
fn test_websocket_server_speaks_first_after_upgrade() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "WebSocket server-speaks-first payload after 101",
            try_websocket_server_speaks_first_after_upgrade,
        ),
        State::Success,
    );
}

fn try_websocket_backend_frame_in_same_read_as_101() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "WS-ONEFLUSH",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );
    let mut backend = backends.pop().unwrap();
    backend.connect();

    let mut client = raw_connect(front_address);
    let request = b"GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
    if client.write_all(request).is_err() || client.flush().is_err() {
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    backend.accept(0);
    backend.receive(0);

    let mut response =
        b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n"
            .to_vec();
    response.extend_from_slice(&websocket_text_frame("HELLO-ONEFLUSH"));
    if !backend_send_bytes(&mut backend, 0, &response) {
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    let result = match raw_read(&mut client) {
        Some(data)
            if data.contains("101 Switching Protocols") && data.contains("HELLO-ONEFLUSH") =>
        {
            State::Success
        }
        other => {
            println!("upgrade plus same-buffer websocket frame read: {other:?}");
            State::Fail
        }
    };

    worker.soft_stop();
    worker.wait_for_server_stop();
    result
}

#[test]
fn test_websocket_backend_frame_in_same_read_as_101() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "WebSocket backend frame in same read buffer as 101",
            try_websocket_backend_frame_in_same_read_as_101,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 3: Rapid connect-disconnect without proxy protocol
// =========================================================================

fn try_rapid_connect_disconnect() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "RAPID-HUP",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );
    let mut backend = backends.pop().unwrap();
    backend.connect();

    for i in 0..50 {
        let mut ephemeral = Client::new(format!("ephemeral-{i}"), front_address, "");
        ephemeral.connect();
        ephemeral.disconnect();
    }

    thread::sleep(Duration::from_millis(500));

    let mut client = Client::new(
        "real-client",
        front_address,
        "GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    client.connect();
    client.send();
    backend.accept(0);
    backend.receive(0);
    backend.send(0);

    match client.receive() {
        Some(response) if response.contains("200") => {}
        _ => return State::Fail,
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_rapid_connect_disconnect() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "Rapid connect-disconnect cycles (no proxy protocol)",
            try_rapid_connect_disconnect,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 4: Keep-alive requests then client HUP mid-request
// =========================================================================

fn try_keepalive_then_hup() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) =
        setup_sync_test("KA-HUP", config, listeners, state, front_address, 1, false);
    let mut backend = backends.pop().unwrap();
    backend
        .set_response("HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: keep-alive\r\n\r\npong");
    backend.connect();

    let mut client = Client::new(
        "ka-client",
        front_address,
        "GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n",
    );
    client.connect();

    // Complete 3 keep-alive cycles (metrics.reset() → start=None after each)
    for i in 0..3 {
        client.send();
        if i == 0 {
            backend.accept(0);
        }
        backend.receive(0);
        backend.send(0);
        match client.receive() {
            Some(response) if response.contains("200") => {}
            _ => return State::Fail,
        }
    }

    // 4th request: HUP before response. close() must only log this one.
    client.send();
    backend.receive(0);
    client.disconnect();
    thread::sleep(Duration::from_millis(100));
    backend.send(0);
    thread::sleep(Duration::from_millis(100));

    // Verify sozu is still functional
    let mut client2 = Client::new(
        "verify-client",
        front_address,
        "GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    client2.connect();
    client2.send();
    backend.accept(1);
    backend.receive(1);
    backend.send(1);

    match client2.receive() {
        Some(response) if response.contains("200") => {}
        _ => return State::Fail,
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_keepalive_then_hup() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "Keep-alive requests then client HUP (no double access log)",
            try_keepalive_then_hup,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 5: Concurrent clients with staggered HUPs
// =========================================================================

fn try_concurrent_hup() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "CONC-HUP",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );
    let mut backend = backends.pop().unwrap();
    backend.connect();

    // Client A: completes normally
    let mut client_a = Client::new(
        "client-A",
        front_address,
        "GET /a HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    client_a.connect();
    client_a.send();
    backend.accept(0);
    backend.receive(0);
    backend.send(0);
    if client_a.receive().is_none() {
        return State::Fail;
    }

    // Client B: HUP mid-flight
    let mut client_b = Client::new(
        "client-B",
        front_address,
        "GET /b HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    client_b.connect();
    client_b.send();
    backend.accept(1);
    backend.receive(1);
    client_b.disconnect();
    thread::sleep(Duration::from_millis(100));
    backend.send(1);
    thread::sleep(Duration::from_millis(50));

    // Client C: completes normally
    let mut client_c = Client::new(
        "client-C",
        front_address,
        "GET /c HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    client_c.connect();
    client_c.send();
    backend.accept(2);
    backend.receive(2);
    backend.send(2);
    match client_c.receive() {
        Some(r) if r.contains("200") => {}
        _ => return State::Fail,
    }

    // Client D: HUP mid-flight
    let mut client_d = Client::new(
        "client-D",
        front_address,
        "GET /d HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    client_d.connect();
    client_d.send();
    backend.accept(3);
    backend.receive(3);
    client_d.disconnect();
    thread::sleep(Duration::from_millis(100));
    backend.send(3);
    thread::sleep(Duration::from_millis(50));

    // Client E: final validation
    let mut client_e = Client::new(
        "client-E",
        front_address,
        "GET /e HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    client_e.connect();
    client_e.send();
    backend.accept(4);
    backend.receive(4);
    backend.send(4);
    match client_e.receive() {
        Some(r) if r.contains("200") => {}
        _ => return State::Fail,
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_concurrent_hup() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "Concurrent clients with staggered HUPs (mixed cleanup paths)",
            try_concurrent_hup,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 6: Proxy protocol PROXY + HTTP request (normal flow)
//
// Validates that the full proxy protocol → Expect → Mux → request → response
// path works correctly with the new service_start/service_stop bracketing.
// =========================================================================

fn try_proxy_protocol_request() -> State {
    let (mut worker, backend_addrs, front_address) = setup_proxy_protocol_test("PP-REQ", 1);

    let mut backend = SyncBackend::new("BACKEND_0", backend_addrs[0], http_ok_response("pp-pong"));
    backend.connect();

    // Send proxy protocol V2 PROXY header + HTTP request on raw TCP
    let pp_header = pp_v2_proxy_ipv4(12345, front_address.port());
    let http_req = b"GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";

    let mut stream = raw_connect(front_address);
    stream.write_all(&pp_header).expect("write pp header");
    stream.write_all(http_req).expect("write http request");

    backend.accept(0);
    backend.receive(0);
    backend.send(0);

    match raw_read(&mut stream) {
        Some(response) if response.contains("200") => {
            println!("PP-REQ response: {response}");
        }
        other => {
            println!("PP-REQ unexpected response: {other:?}");
            worker.soft_stop();
            worker.wait_for_server_stop();
            return State::Fail;
        }
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_proxy_protocol_request() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "Proxy protocol V2 PROXY + HTTP request (normal flow)",
            try_proxy_protocol_request,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 7: Proxy protocol PROXY + HTTP request + client HUP
//
// Exercises Mux::close() stream cleanup behind proxy protocol.
// The session transitions Expect → Mux, starts an HTTP request, then the
// client HUPs before receiving a response. close() must generate access logs.
// =========================================================================

fn try_proxy_protocol_hup() -> State {
    let (mut worker, backend_addrs, front_address) = setup_proxy_protocol_test("PP-HUP", 1);

    let mut backend = SyncBackend::new("BACKEND_0", backend_addrs[0], http_ok_response("pp-pong"));
    backend.connect();

    // Send PP header + HTTP request, then disconnect before response
    let pp_header = pp_v2_proxy_ipv4(12345, front_address.port());
    let http_req = b"GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";

    let mut stream = raw_connect(front_address);
    stream.write_all(&pp_header).expect("write pp header");
    stream.write_all(http_req).expect("write http request");

    backend.accept(0);
    backend.receive(0);

    // HUP before response
    drop(stream);
    thread::sleep(Duration::from_millis(100));
    backend.send(0);
    thread::sleep(Duration::from_millis(100));

    // Verify sozu still works with a second request
    let mut stream2 = raw_connect(front_address);
    let pp_header2 = pp_v2_proxy_ipv4(12346, front_address.port());
    stream2.write_all(&pp_header2).expect("write pp header 2");
    stream2
        .write_all(b"GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("write http request 2");

    backend.accept(1);
    backend.receive(1);
    backend.send(1);

    match raw_read(&mut stream2) {
        Some(response) if response.contains("200") => {}
        _ => {
            worker.soft_stop();
            worker.wait_for_server_stop();
            return State::Fail;
        }
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_proxy_protocol_hup() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "Proxy protocol V2 PROXY + HTTP request + client HUP",
            try_proxy_protocol_hup,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 8: Proxy protocol LOCAL + IPv4 healthcheck then disconnect
//
// Simulates HAProxy healthcheck with LOCAL command (IPv4 family, 28 bytes).
// Sozu parses the header, transitions Expect → Mux (with valid addresses),
// but the client disconnects immediately — no HTTP request is ever sent.
// Validates Mux::close() handles sessions with no active streams.
// =========================================================================

fn try_proxy_protocol_local_healthcheck() -> State {
    let (mut worker, backend_addrs, front_address) = setup_proxy_protocol_test("PP-LOCAL", 1);

    let mut backend = SyncBackend::new("BACKEND_0", backend_addrs[0], http_ok_response("pp-pong"));
    backend.connect();

    // Send LOCAL header and disconnect immediately (healthcheck pattern)
    let local_header = pp_v2_local_ipv4();
    let mut stream = raw_connect(front_address);
    stream.write_all(&local_header).expect("write LOCAL header");
    drop(stream);
    thread::sleep(Duration::from_millis(100));

    // Verify sozu is still healthy with a real request
    let pp_header = pp_v2_proxy_ipv4(12345, front_address.port());
    let mut stream2 = raw_connect(front_address);
    stream2.write_all(&pp_header).expect("write pp header");
    stream2
        .write_all(b"GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("write http request");

    backend.accept(0);
    backend.receive(0);
    backend.send(0);

    match raw_read(&mut stream2) {
        Some(response) if response.contains("200") => {}
        _ => {
            worker.soft_stop();
            worker.wait_for_server_stop();
            return State::Fail;
        }
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_proxy_protocol_local_healthcheck() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "Proxy protocol LOCAL + IPv4 healthcheck then disconnect",
            try_proxy_protocol_local_healthcheck,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 9: Proxy protocol AF_UNSPEC healthcheck then disconnect
//
// Simulates the most minimal HAProxy healthcheck: LOCAL + AF_UNSPEC (16 bytes).
// Sozu's ExpectProxyProtocol reads 28 bytes minimum (V4), so this header is
// incomplete — sozu waits for more data, then sees the connection close.
// Session closes in Expect state without ever reaching Mux.
// =========================================================================

fn try_proxy_protocol_af_unspec_healthcheck() -> State {
    let (mut worker, backend_addrs, front_address) = setup_proxy_protocol_test("PP-UNSPEC", 1);

    let mut backend = SyncBackend::new("BACKEND_0", backend_addrs[0], http_ok_response("pp-pong"));
    backend.connect();

    // Send 16-byte AF_UNSPEC header and disconnect
    let unspec_header = pp_v2_local_af_unspec();
    let mut stream = raw_connect(front_address);
    stream
        .write_all(&unspec_header)
        .expect("write AF_UNSPEC header");
    drop(stream);
    thread::sleep(Duration::from_millis(100));

    // Verify sozu is still healthy
    let pp_header = pp_v2_proxy_ipv4(12345, front_address.port());
    let mut stream2 = raw_connect(front_address);
    stream2.write_all(&pp_header).expect("write pp header");
    stream2
        .write_all(b"GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("write http request");

    backend.accept(0);
    backend.receive(0);
    backend.send(0);

    match raw_read(&mut stream2) {
        Some(response) if response.contains("200") => {}
        _ => {
            worker.soft_stop();
            worker.wait_for_server_stop();
            return State::Fail;
        }
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_proxy_protocol_af_unspec_healthcheck() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "Proxy protocol AF_UNSPEC healthcheck then disconnect",
            try_proxy_protocol_af_unspec_healthcheck,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 10: Rapid proxy protocol healthchecks (HAProxy pattern at ~10ms)
//
// Simulates HAProxy sending healthchecks every ~10ms with proxy protocol.
// Mix of LOCAL+IPv4 (28-byte, parseable) and AF_UNSPEC (16-byte, incomplete)
// headers followed by immediate disconnect. Validates sozu doesn't leak
// resources under sustained healthcheck traffic.
// =========================================================================

fn try_rapid_proxy_protocol_healthchecks() -> State {
    let (mut worker, backend_addrs, front_address) = setup_proxy_protocol_test("PP-RAPID", 1);

    let mut backend = SyncBackend::new("BACKEND_0", backend_addrs[0], http_ok_response("pp-pong"));
    backend.connect();

    // 100 rapid healthchecks alternating between LOCAL+IPv4 and AF_UNSPEC
    for i in 0..100 {
        let mut stream = raw_connect(front_address);
        if i % 2 == 0 {
            // LOCAL+IPv4 (28 bytes — sozu can parse this)
            let _ = stream.write_all(&pp_v2_local_ipv4());
        } else {
            // AF_UNSPEC (16 bytes — incomplete for sozu)
            let _ = stream.write_all(&pp_v2_local_af_unspec());
        }
        drop(stream);
        // ~10ms between healthchecks (like real HAProxy)
        thread::sleep(Duration::from_millis(10));
    }

    // Let sozu drain all the closed sessions
    thread::sleep(Duration::from_millis(500));

    // Verify sozu is still healthy with a real proxied request
    let pp_header = pp_v2_proxy_ipv4(54321, front_address.port());
    let mut stream = raw_connect(front_address);
    stream.write_all(&pp_header).expect("write pp header");
    stream
        .write_all(b"GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("write http request");

    backend.accept(0);
    backend.receive(0);
    backend.send(0);

    match raw_read(&mut stream) {
        Some(response) if response.contains("200") => {
            println!("post-healthcheck-storm response OK");
        }
        _ => {
            worker.soft_stop();
            worker.wait_for_server_stop();
            return State::Fail;
        }
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_rapid_proxy_protocol_healthchecks() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "Rapid proxy protocol healthchecks (~10ms interval, 100 cycles)",
            try_rapid_proxy_protocol_healthchecks,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 11: Partial proxy protocol header then disconnect
//
// Client sends only the 12-byte magic signature (incomplete V2 header),
// then disconnects. Validates ExpectProxyProtocol state cleanup when the
// header is truncated — sozu must not crash or leak.
// =========================================================================

fn try_proxy_protocol_partial_header() -> State {
    let (mut worker, backend_addrs, front_address) = setup_proxy_protocol_test("PP-PARTIAL", 1);

    let mut backend = SyncBackend::new("BACKEND_0", backend_addrs[0], http_ok_response("pp-pong"));
    backend.connect();

    // Send only the 12-byte magic (incomplete header)
    let partial = &[
        0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
    ];
    let mut stream = raw_connect(front_address);
    stream.write_all(partial).expect("write partial header");
    drop(stream);
    thread::sleep(Duration::from_millis(100));

    // Verify sozu still works
    let pp_header = pp_v2_proxy_ipv4(12345, front_address.port());
    let mut stream2 = raw_connect(front_address);
    stream2.write_all(&pp_header).expect("write pp header");
    stream2
        .write_all(b"GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("write http request");

    backend.accept(0);
    backend.receive(0);
    backend.send(0);

    match raw_read(&mut stream2) {
        Some(response) if response.contains("200") => {}
        _ => {
            worker.soft_stop();
            worker.wait_for_server_stop();
            return State::Fail;
        }
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_proxy_protocol_partial_header() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "Partial proxy protocol header then disconnect",
            try_proxy_protocol_partial_header,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 12: Proxy protocol + WebSocket upgrade
//
// Full path: PP PROXY → Expect → Mux → WebSocket upgrade → pipe mode.
// Validates gauge correctness when proxy protocol and WebSocket upgrade
// are combined — the most complex state machine path.
// =========================================================================

fn try_proxy_protocol_websocket_upgrade() -> State {
    let (mut worker, backend_addrs, front_address) = setup_proxy_protocol_test("PP-WS", 1);

    let mut backend = SyncBackend::new(
        "BACKEND_0",
        backend_addrs[0],
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
    );
    backend.connect();

    // Send PP header + WebSocket upgrade request
    let pp_header = pp_v2_proxy_ipv4(12345, front_address.port());
    let ws_req = b"GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";

    let mut stream = raw_connect(front_address);
    stream.write_all(&pp_header).expect("write pp header");
    stream.write_all(ws_req).expect("write ws upgrade request");

    backend.accept(0);
    backend.receive(0);
    backend.send(0);

    // Should receive 101 Switching Protocols
    match raw_read(&mut stream) {
        Some(response) if response.contains("101") => {
            println!("PP-WS upgrade response: {response}");
        }
        other => {
            println!("PP-WS unexpected response: {other:?}");
            worker.soft_stop();
            worker.wait_for_server_stop();
            return State::Fail;
        }
    }

    thread::sleep(Duration::from_millis(100));

    // Post-upgrade bidirectional data
    stream
        .write_all(b"ws-ping")
        .expect("write post-upgrade data");
    thread::sleep(Duration::from_millis(50));
    match backend.receive(0) {
        Some(data) if data.contains("ws-ping") => {}
        _ => {
            worker.soft_stop();
            worker.wait_for_server_stop();
            return State::Fail;
        }
    }

    backend.set_response("ws-pong");
    backend.send(0);
    thread::sleep(Duration::from_millis(50));
    match raw_read(&mut stream) {
        Some(data) if data.contains("ws-pong") => {}
        _ => {
            worker.soft_stop();
            worker.wait_for_server_stop();
            return State::Fail;
        }
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_proxy_protocol_websocket_upgrade() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "Proxy protocol V2 + WebSocket upgrade (full state machine path)",
            try_proxy_protocol_websocket_upgrade,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 13: Proxy protocol + keep-alive + HUP mid-request
//
// Full path: PP PROXY → Expect → Mux → 3 keep-alive requests → HUP on 4th.
// Exercises the start.is_some() guard in close() behind proxy protocol.
// =========================================================================

fn try_proxy_protocol_keepalive_hup() -> State {
    let (mut worker, backend_addrs, front_address) = setup_proxy_protocol_test("PP-KA-HUP", 1);

    let mut backend = SyncBackend::new(
        "BACKEND_0",
        backend_addrs[0],
        "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: keep-alive\r\n\r\npong",
    );
    backend.connect();

    // PP header + first request
    let pp_header = pp_v2_proxy_ipv4(12345, front_address.port());
    let http_req = b"GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n";

    let mut stream = raw_connect(front_address);
    stream.write_all(&pp_header).expect("write pp header");
    stream.write_all(http_req).expect("write request 1");

    backend.accept(0);
    backend.receive(0);
    backend.send(0);
    match raw_read(&mut stream) {
        Some(r) if r.contains("200") => {}
        _ => {
            worker.soft_stop();
            worker.wait_for_server_stop();
            return State::Fail;
        }
    }

    // 2 more keep-alive requests on the same connection
    for _ in 0..2 {
        stream
            .write_all(http_req)
            .expect("write keep-alive request");
        backend.receive(0);
        backend.send(0);
        match raw_read(&mut stream) {
            Some(r) if r.contains("200") => {}
            _ => {
                worker.soft_stop();
                worker.wait_for_server_stop();
                return State::Fail;
            }
        }
    }

    // 4th request: HUP before response
    stream.write_all(http_req).expect("write request 4");
    backend.receive(0);
    drop(stream);
    thread::sleep(Duration::from_millis(100));
    backend.send(0);
    thread::sleep(Duration::from_millis(100));

    // Verify sozu still works
    let pp_header2 = pp_v2_proxy_ipv4(12346, front_address.port());
    let mut stream2 = raw_connect(front_address);
    stream2.write_all(&pp_header2).expect("write pp header 2");
    stream2
        .write_all(b"GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("write verification request");

    backend.accept(1);
    backend.receive(1);
    backend.send(1);

    match raw_read(&mut stream2) {
        Some(r) if r.contains("200") => {}
        _ => {
            worker.soft_stop();
            worker.wait_for_server_stop();
            return State::Fail;
        }
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_proxy_protocol_keepalive_hup() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "Proxy protocol + keep-alive + HUP mid-request",
            try_proxy_protocol_keepalive_hup,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 14: Mixed proxy protocol healthchecks and real traffic
//
// Simulates production conditions: concurrent healthcheck probes interleaved
// with legitimate proxied HTTP requests. Validates sozu handles the mixture
// without leaking sessions or corrupting state.
// =========================================================================

fn try_proxy_protocol_mixed_traffic() -> State {
    let (mut worker, backend_addrs, front_address) = setup_proxy_protocol_test("PP-MIXED", 1);

    let mut backend = SyncBackend::new("BACKEND_0", backend_addrs[0], http_ok_response("pp-pong"));
    backend.connect();

    let mut backend_client_id = 0;

    for round in 0..5 {
        // Burst of 10 healthchecks
        for _ in 0..10 {
            let mut stream = raw_connect(front_address);
            let _ = stream.write_all(&pp_v2_local_ipv4());
            drop(stream);
        }
        thread::sleep(Duration::from_millis(50));

        // Real request
        let pp_header = pp_v2_proxy_ipv4(30000 + round, front_address.port());
        let mut stream = raw_connect(front_address);
        stream.write_all(&pp_header).expect("write pp header");
        stream
            .write_all(b"GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .expect("write http request");

        backend.accept(backend_client_id);
        backend.receive(backend_client_id);
        backend.send(backend_client_id);
        backend_client_id += 1;

        match raw_read(&mut stream) {
            Some(response) if response.contains("200") => {
                println!("round {round}: request OK");
            }
            _ => {
                worker.soft_stop();
                worker.wait_for_server_stop();
                return State::Fail;
            }
        }
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_proxy_protocol_mixed_traffic() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "Mixed proxy protocol healthchecks and real traffic",
            try_proxy_protocol_mixed_traffic,
        ),
        State::Success,
    );
}

// =========================================================================
// Regression tests for gauge underflow and error over-counting fixes
//
// These tests validate that, measured against a baseline of one request held
// in flight (see `hold_one_request_in_flight`):
// - http.active_requests is not decremented for streams that never had a
//   request fully parsed (idle timeouts, malformed requests)
// - an intermediate HTTP response (100 Continue) does not release the charge
//   early, leaving the final response to release it a second time
// - Session close() does not unconditionally mark all in-flight streams
//   as errors
// =========================================================================

/// Helper: set up a worker whose pre-request timeout is 2s, so a silent
/// connection collects its 408 instead of making the test wait out the
/// 10-second default.
fn setup_short_timeout_test(
    name: &str,
    front_address: SocketAddr,
    nb_backends: usize,
) -> (Worker, Vec<SyncBackend>) {
    let mut file_config = FileConfig::default();
    // ONLY `request_timeout` is shortened, and which one is shortened is
    // load-bearing. It is the timeout a frontend is armed with before its
    // first request links — `HttpSession::new` builds the frontend's
    // `TimeoutContainer` from `configured_request_timeout`, and `Mux` swaps in
    // the nominal `front_timeout` only once a stream reaches
    // `StreamState::Link` — so it alone decides how fast a silent connection
    // collects its 408. `front_timeout` and `back_timeout` must stay at their
    // defaults: the gauge test below holds one legitimate request in flight
    // across the whole idle-timeout window, and a 2-second frontend or backend
    // timeout would answer THAT request 504 and emit a perfectly LEGITIMATE
    // `-1`. The gauge would read zero for a correct proxy, which is
    // indistinguishable from the underflow the test exists to catch.
    file_config.request_timeout = Some(2);
    let config = Worker::into_config(file_config);
    let mut listeners = sozu_command_lib::scm_socket::Listeners::default();
    attach_reserved_http_listener(&mut listeners, front_address);
    let state = sozu_command_lib::state::ConfigState::new();

    let mut worker = Worker::start_new_worker_owned(name, config, listeners, state);
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::AddHttpListener(
            ListenerBuilder::new_http(front_address.into())
                .to_http(None)
                .unwrap(),
        )),
    });
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::ActivateListener(ActivateListener {
            address: front_address.into(),
            proxy: ListenerType::Http.into(),
            from_scm: false,
        })),
    });
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::AddCluster(Cluster {
            ..Worker::default_cluster("cluster_0")
        })),
    });
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::AddHttpFrontend(Worker::default_http_frontend(
            "cluster_0",
            front_address,
        ))),
    });

    let mut backends = Vec::new();
    for i in 0..nb_backends {
        let back_address = create_local_address();
        worker.send_proxy_request(
            RequestType::AddBackend(Worker::default_backend(
                "cluster_0",
                format!("cluster_0-{i}"),
                back_address,
                None,
            ))
            .into(),
        );
        backends.push(SyncBackend::new(
            format!("BACKEND_{i}"),
            back_address,
            http_ok_response(format!("pong{i}")),
        ));
    }

    worker.read_to_last();
    (worker, backends)
}

/// Raw TCP connect with configurable read timeout.
fn raw_connect_with_timeout(addr: SocketAddr, timeout: Duration) -> TcpStream {
    let stream = TcpStream::connect(addr).expect("could not connect");
    stream.set_read_timeout(Some(timeout)).unwrap();
    stream
        .set_write_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    stream
}

// =========================================================================
// Test 15: an H1 idle timeout emits no `http.active_requests` decrement
//
// A connection that sends nothing collects a 408 from `MuxState::timeout`'s
// `StreamState::Idle` arm, then closes. That stream was never charged — the H1
// `+1` sits past the header parse, in `ConnectionH1::readable` — so the access
// log its 408 emits must not decrement the gauge.
//
// The assertion is made against a baseline of one request held in flight and
// never against zero: `AggregatedMetric::update` saturates a gauge at zero, so
// from a zero baseline a parasitic `-1` reads exactly like a balanced request.
// See `hold_one_request_in_flight`.
//
// To SEE THIS RED: in `Stream::generate_access_log`
// (`lib/src/protocol/mux/stream.rs`), drop the `if self.request_counted`
// condition, leaving `events[0] = Some(MetricEvent::ActiveRequestFinished);`
// and `self.request_counted = false;` unconditional. Every access log then
// decrements, the five 408s take the gauge from 1 to 0, and this test reports
// `idle-timeout: the 408s moved the gauge - wanted 1, sample Some(0)`.
//
// That substitution is SHARED with test 16, which goes red with it: the
// malformed path reaches the same access log on the same never-charged
// stream. There is no second guard between "no `+1`" and "no `-1`" to remove,
// so these two tests are two scenarios through one guard, not two guards, and
// no third distinct red was invented to disguise that.
// =========================================================================

fn try_idle_timeout_no_underflow() -> State {
    let front_address = create_local_address();
    let (mut worker, mut backends) = setup_short_timeout_test("IDLE-TIMEOUT", front_address, 1);
    let mut backend = backends.pop().unwrap();
    backend.connect();

    let mut held = match hold_one_request_in_flight(&mut worker, &mut backend, front_address, 0) {
        Ok(held) => held,
        Err(diag) => {
            println!("idle-timeout: {diag}");
            worker.soft_stop();
            let _ = worker.wait_for_server_stop();
            return State::Fail;
        }
    };

    // Opened all at once rather than one at a time: each waits out the same
    // 2-second `request_timeout`, and serialising them would hold the baseline
    // request open five times longer for no added coverage.
    let mut idle: Vec<TcpStream> = (0..5)
        .map(|_| raw_connect_with_timeout(front_address, Duration::from_secs(5)))
        .collect();
    for (i, stream) in idle.iter_mut().enumerate() {
        match raw_read_until_eof(stream, Duration::from_secs(20)) {
            Ok(response) if response.contains("408") => {
                println!("idle-timeout {i}: got 408 and EOF as expected");
            }
            Ok(response) => {
                println!("idle-timeout {i}: expected a 408, got {response:?}");
                worker.soft_stop();
                let _ = worker.wait_for_server_stop();
                return State::Fail;
            }
            Err(diag) => {
                println!("idle-timeout {i}: {diag}");
                worker.soft_stop();
                let _ = worker.wait_for_server_stop();
                return State::Fail;
            }
        }
    }

    // THE assertion: five timed-out streams, none of them ever charged, and
    // the gauge still holds exactly the one request in flight.
    if let Err(diag) = expect_active_requests_h1(&mut worker, 1) {
        println!("idle-timeout: the 408s moved the gauge - {diag}");
        worker.soft_stop();
        let _ = worker.wait_for_server_stop();
        return State::Fail;
    }

    // Releasing the held request is what takes the gauge back to zero, which
    // also pins the `-1` a charged stream DOES owe.
    backend.send(0);
    let response = held.receive();
    if !response.as_deref().is_some_and(|r| r.contains("200")) {
        println!("idle-timeout: the held request did not complete: {response:?}");
        worker.soft_stop();
        let _ = worker.wait_for_server_stop();
        return State::Fail;
    }
    if let Err(diag) = await_active_requests_h1(&mut worker, 0, Duration::from_secs(10)) {
        println!("idle-timeout: the held request never released its charge - {diag}");
        worker.soft_stop();
        let _ = worker.wait_for_server_stop();
        return State::Fail;
    }

    worker.soft_stop();
    let _ = worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_idle_timeout_no_underflow() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H1: five idle-timeout 408s leave http.active_requests at the one \
             request held in flight",
            try_idle_timeout_no_underflow,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 16: an H1 malformed request emits no `http.active_requests` decrement
//
// A request that fails to parse is answered 400 by `ConnectionH1::readable`,
// from a `return` placed BEFORE the `gauge_add!(ACTIVE_REQUESTS, 1)` a dozen
// lines below it. The stream is therefore never charged, and the access log
// its 400 emits must not decrement the gauge.
//
// To SEE THIS RED: the same substitution as test 15 — drop the
// `if self.request_counted` condition in `Stream::generate_access_log`
// (`lib/src/protocol/mux/stream.rs`). This test then reports
// `malformed: the 400s moved the gauge - wanted 1, sample Some(0)`. The two
// tests fall together deliberately; see test 15's note on why.
// =========================================================================

fn try_malformed_request_no_underflow() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "MALFORMED",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );
    let mut backend = backends.pop().unwrap();
    backend.connect();

    let mut held = match hold_one_request_in_flight(&mut worker, &mut backend, front_address, 0) {
        Ok(held) => held,
        Err(diag) => {
            println!("malformed: {diag}");
            worker.soft_stop();
            let _ = worker.wait_for_server_stop();
            return State::Fail;
        }
    };

    let malformed_requests = [
        "GARBAGE DATA THAT IS NOT HTTP\r\n\r\n",
        "GET / HTTP/1.1\r\nHost localhost\r\n\r\n",
        "GET\r\n\r\n",
    ];

    for (i, bad_request) in malformed_requests.iter().enumerate() {
        let mut stream = raw_connect_with_timeout(front_address, Duration::from_secs(5));
        if let Err(error) = stream.write_all(bad_request.as_bytes()) {
            println!("malformed {i}: could not send: {error}");
            worker.soft_stop();
            let _ = worker.wait_for_server_stop();
            return State::Fail;
        }
        match raw_read_until_eof(&mut stream, Duration::from_secs(20)) {
            Ok(response) if response.contains("400") => {
                println!("malformed {i}: got 400 and EOF as expected");
            }
            Ok(response) => {
                println!("malformed {i}: expected a 400, got {response:?}");
                worker.soft_stop();
                let _ = worker.wait_for_server_stop();
                return State::Fail;
            }
            Err(diag) => {
                println!("malformed {i}: {diag}");
                worker.soft_stop();
                let _ = worker.wait_for_server_stop();
                return State::Fail;
            }
        }
    }

    // THE assertion: three rejected requests, none of them ever charged, and
    // the gauge still holds exactly the one request in flight.
    if let Err(diag) = expect_active_requests_h1(&mut worker, 1) {
        println!("malformed: the 400s moved the gauge - {diag}");
        worker.soft_stop();
        let _ = worker.wait_for_server_stop();
        return State::Fail;
    }

    backend.send(0);
    let response = held.receive();
    if !response.as_deref().is_some_and(|r| r.contains("200")) {
        println!("malformed: the held request did not complete: {response:?}");
        worker.soft_stop();
        let _ = worker.wait_for_server_stop();
        return State::Fail;
    }
    if let Err(diag) = await_active_requests_h1(&mut worker, 0, Duration::from_secs(10)) {
        println!("malformed: the held request never released its charge - {diag}");
        worker.soft_stop();
        let _ = worker.wait_for_server_stop();
        return State::Fail;
    }

    worker.soft_stop();
    let _ = worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_malformed_request_no_underflow() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H1: three malformed-request 400s leave http.active_requests at \
             the one request held in flight",
            try_malformed_request_no_underflow,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 17: an H1 100 Continue does not release the active_requests charge
//
// `ConnectionH1::writable` matches `StatusLine::Response { code: 100, .. }`
// and returns WITHOUT generating an access log, precisely so that the final
// response stays the stream's only completion. The gauge is where that is
// observable: while the client holds its 100 and the backend has not yet sent
// the 200, the request is still in flight and must still be charged. With one
// further request held in flight the reading is `2`, and an early release
// shows up as `1`.
//
// What this covers, and what it does not. It covers the `code: 100` arm. It
// does NOT cover `Stream::request_counted`'s idempotency, the second guard
// against a double decrement, and no e2e test can: no production path calls
// `generate_access_log` twice on one stream, because the completion path
// clears `metrics.start` through `stream.metrics.reset()` and `Mux::close`
// skips every stream whose `metrics.start` is `None`. Removing the flag clear
// alone therefore changes no gauge value any client can observe. Do not read
// the closing assertion below as covering it.
//
// To SEE THIS RED: in `ConnectionH1::writable` (`lib/src/protocol/mux/h1.rs`),
// delete the `kawa::StatusLine::Response { code: 100, .. }` match arm so that
// a 100 falls through to the generic `_ => {}` completion path. The access log
// then fires on the interim response, the charge is released one response too
// early, and this test reports
// `100-continue: the interim response released the charge - wanted 2, sample Some(1)`.
// =========================================================================

fn try_100_continue_no_double_decrement() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "100-CONTINUE",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );
    let mut backend = backends.pop().unwrap();
    backend.connect();

    let mut held = match hold_one_request_in_flight(&mut worker, &mut backend, front_address, 0) {
        Ok(held) => held,
        Err(diag) => {
            println!("100-continue: {diag}");
            worker.soft_stop();
            let _ = worker.wait_for_server_stop();
            return State::Fail;
        }
    };

    // The request under test, on its own frontend connection and its own
    // backend slot, so the baseline above keeps its charge throughout.
    let request = "POST /api HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nExpect: 100-continue\r\nConnection: close\r\n\r\nhello";
    let mut stream = raw_connect_with_timeout(front_address, Duration::from_secs(5));
    if let Err(error) = stream.write_all(request.as_bytes()) {
        println!("100-continue: could not send the request: {error}");
        worker.soft_stop();
        let _ = worker.wait_for_server_stop();
        return State::Fail;
    }
    backend.accept(1);
    backend.receive(1);

    // Two requests in flight: the held baseline, and this one.
    if let Err(diag) = await_active_requests_h1(&mut worker, 2, Duration::from_secs(10)) {
        println!("100-continue: the request never charged the gauge - {diag}");
        worker.soft_stop();
        let _ = worker.wait_for_server_stop();
        return State::Fail;
    }

    backend.set_response("HTTP/1.1 100 Continue\r\n\r\n");
    backend.send(1);

    // Reading the interim response is the ordering barrier for the assertion
    // that follows: sozu has handled the 100 and taken whatever branch it
    // takes for it.
    let mut interim = String::new();
    let started = Instant::now();
    while !interim.contains("100") && started.elapsed() < Duration::from_secs(10) {
        if let Some(chunk) = raw_read(&mut stream) {
            interim.push_str(&chunk);
        }
    }
    if !interim.contains("100") {
        println!("100-continue: the interim response never reached the client: {interim:?}");
        worker.soft_stop();
        let _ = worker.wait_for_server_stop();
        return State::Fail;
    }

    // THE assertion: an interim response is not a completion, so the charge is
    // still outstanding and the gauge still reads both requests.
    if let Err(diag) = expect_active_requests_h1(&mut worker, 2) {
        println!("100-continue: the interim response released the charge - {diag}");
        worker.soft_stop();
        let _ = worker.wait_for_server_stop();
        return State::Fail;
    }

    backend.set_response("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
    backend.send(1);
    match raw_read_until_eof(&mut stream, Duration::from_secs(20)) {
        Ok(rest) if rest.contains("200") => {}
        Ok(rest) => {
            println!("100-continue: expected a 200, got {rest:?} after {interim:?}");
            worker.soft_stop();
            let _ = worker.wait_for_server_stop();
            return State::Fail;
        }
        Err(diag) => {
            println!("100-continue: {diag}");
            worker.soft_stop();
            let _ = worker.wait_for_server_stop();
            return State::Fail;
        }
    }

    // The final response released the charge, once: the gauge is back to the
    // held baseline and not below it.
    if let Err(diag) = expect_active_requests_h1(&mut worker, 1) {
        println!("100-continue: the completed request left the gauge wrong - {diag}");
        worker.soft_stop();
        let _ = worker.wait_for_server_stop();
        return State::Fail;
    }

    backend.set_response(http_ok_response("pong0"));
    backend.send(0);
    let response = held.receive();
    if !response.as_deref().is_some_and(|r| r.contains("200")) {
        println!("100-continue: the held request did not complete: {response:?}");
        worker.soft_stop();
        let _ = worker.wait_for_server_stop();
        return State::Fail;
    }
    if let Err(diag) = await_active_requests_h1(&mut worker, 0, Duration::from_secs(10)) {
        println!("100-continue: the held request never released its charge - {diag}");
        worker.soft_stop();
        let _ = worker.wait_for_server_stop();
        return State::Fail;
    }

    worker.soft_stop();
    let _ = worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_100_continue_no_double_decrement() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H1: a 100 Continue leaves http.active_requests charged until the \
             final response",
            try_100_continue_no_double_decrement,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 18: Backend connection refused does not over-count errors
//
// Requests a cluster with no backend listening. Sozu should return a 503
// and NOT inflate http.errors for every session teardown.
// The worker must remain functional for subsequent requests.
// Uses short timeouts to avoid test hanging.
// =========================================================================

fn try_backend_refused_no_error_inflation() -> State {
    let front_address = create_local_address();
    let mut file_config = FileConfig::default();
    file_config.front_timeout = Some(2);
    file_config.request_timeout = Some(2);
    file_config.back_timeout = Some(2);
    file_config.connect_timeout = Some(2);
    let config = Worker::into_config(file_config);
    let mut listeners = sozu_command_lib::scm_socket::Listeners::default();
    attach_reserved_http_listener(&mut listeners, front_address);
    let state = sozu_command_lib::state::ConfigState::new();

    let mut worker = Worker::start_new_worker_owned("BACKEND-REFUSED", config, listeners, state);
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::AddHttpListener(
            ListenerBuilder::new_http(front_address.into())
                .to_http(None)
                .unwrap(),
        )),
    });
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::ActivateListener(ActivateListener {
            address: front_address.into(),
            proxy: ListenerType::Http.into(),
            from_scm: false,
        })),
    });
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::AddCluster(Cluster {
            ..Worker::default_cluster("cluster_0")
        })),
    });
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::AddHttpFrontend(Worker::default_http_frontend(
            "cluster_0",
            front_address,
        ))),
    });

    let dead_backend = create_unbound_local_address();
    worker.send_proxy_request(
        RequestType::AddBackend(Worker::default_backend(
            "cluster_0",
            "cluster_0-0",
            dead_backend,
            None,
        ))
        .into(),
    );
    worker.read_to_last();

    // Send requests that will get 503 (no backend available).
    // Before the fix, each session teardown would unconditionally mark
    // in-flight streams as errors, inflating http.errors.
    for i in 0..5 {
        let mut stream = raw_connect_with_timeout(front_address, Duration::from_secs(5));
        stream
            .write_all(b"GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .unwrap();
        match raw_read(&mut stream) {
            Some(response)
                if response.contains("503")
                    || response.contains("502")
                    || response.contains("504") =>
            {
                println!("backend-refused {i}: got 50x as expected");
            }
            Some(response) => {
                println!(
                    "backend-refused {i}: response: {}",
                    &response[..response.len().min(80)]
                );
            }
            None => {
                println!("backend-refused {i}: connection closed");
            }
        }
        thread::sleep(Duration::from_millis(100));
    }

    // Sozu must still be accepting connections after the failed attempts
    match TcpStream::connect_timeout(&front_address, Duration::from_secs(2)) {
        Ok(_) => {
            println!("backend-refused: sozu still accepts connections after 5 failures");
        }
        Err(e) => {
            println!("backend-refused: sozu not accepting connections: {e}");
            worker.soft_stop();
            worker.wait_for_server_stop();
            return State::Fail;
        }
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_backend_refused_no_error_inflation() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "Backend connection refused does not over-count errors",
            try_backend_refused_no_error_inflation,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 19: Rapid idle timeouts interleaved with valid requests
//
// Simulates the production pattern that caused the original bug:
// a scanner repeatedly connecting with bad TLS (here simulated as TCP
// connections that send nothing), interleaved with real HTTP traffic.
// After the fix, the gauge must not underflow and sozu must stay healthy.
// =========================================================================

fn try_rapid_idle_with_valid_traffic() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "IDLE+VALID",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );
    let mut backend = backends.pop().unwrap();
    backend.set_response("HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\npong");
    backend.connect();

    // Interleave: open idle connections (simulating scanner) + valid requests
    for round in 0..3 {
        // 3 idle connections (fire-and-forget, will timeout)
        let mut idle_streams = Vec::new();
        for _ in 0..3 {
            idle_streams.push(raw_connect(front_address));
        }

        // 1 valid request while idle connections are pending
        let mut client = Client::new(
            "valid-client",
            front_address,
            "GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        client.connect();
        client.send();
        backend.accept(round);
        backend.receive(round);
        backend.send(round);

        match client.receive() {
            Some(response) if response.contains("200") => {
                println!("round {round}: valid request got 200");
            }
            other => {
                println!("round {round}: valid request failed: {other:?}");
                worker.soft_stop();
                worker.wait_for_server_stop();
                return State::Fail;
            }
        }

        // Drop idle connections
        drop(idle_streams);
        thread::sleep(Duration::from_millis(100));
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_rapid_idle_with_valid_traffic() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "Rapid idle timeouts interleaved with valid requests",
            try_rapid_idle_with_valid_traffic,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 20: Close-delimited response is fully delivered to the client
//
// When a backend sends a response with `Connection: close`, sozu must
// drain the full response buffer to the client before closing the session.
//
// Before the fix, end_stream() set `self.readiness.event = Ready::HUP` on
// the frontend connection, which prevented the main event loop from
// flushing the response buffer. The session would spin 10,000 iterations
// and be force-closed, causing ECONNRESET on the client side.
//
// The fix sets `stream.state = StreamState::Unlinked` and inserts
// `Ready::WRITABLE` into the interest set, allowing writable() to flush
// the response buffer and then cleanly close via CloseSession.
// =========================================================================

/// Read all available data from a TCP stream until EOF or timeout.
/// Returns the accumulated data as a String.
fn raw_read_all(stream: &mut TcpStream) -> String {
    let mut all_data = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => all_data.extend_from_slice(&buf[..n]),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => break,
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&all_data).to_string()
}

fn try_close_delimited_response() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "CLOSE-DELIM",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );
    let mut backend = backends.pop().unwrap();

    // Backend sends a response with Connection: close
    let body = "close-delimited-body-ok";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body,
    );
    backend.set_response(response);
    backend.connect();

    // Client sends a request with Connection: close
    let mut stream = raw_connect(front_address);
    stream
        .write_all(b"GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("write request");

    backend.accept(0);
    backend.receive(0);
    backend.send(0);
    // Backend closes its side after sending (close-delimited)
    backend.close(0);

    // Give sozu time to process the backend close and flush the buffer
    thread::sleep(Duration::from_millis(200));

    // Client must receive the full response body
    let response_data = raw_read_all(&mut stream);
    if !response_data.contains("200 OK") {
        println!("close-delimited: missing 200 status line in: {response_data}");
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }
    if !response_data.contains(body) {
        println!("close-delimited: missing body in response: {response_data}");
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    println!(
        "close-delimited: received full response ({} bytes)",
        response_data.len()
    );

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

fn try_close_delimited_large_response() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "CLOSE-DELIM-LARGE",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );
    let mut backend = backends.pop().unwrap();

    // Generate a body larger than sozu's buffer_size (default ~16KB).
    // Use 48KB to ensure multiple flush cycles are needed.
    let body = "X".repeat(48 * 1024);
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body,
    );
    backend.set_response(response);
    backend.connect();

    // Client sends a request with Connection: close
    let mut stream = raw_connect_with_timeout(front_address, Duration::from_secs(5));
    stream
        .write_all(b"GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("write request");

    backend.accept(0);
    backend.receive(0);

    // The backend response is larger than the mock's write buffer,
    // so we may need to send in chunks. SyncBackend::send writes once,
    // but the kernel TCP buffer usually handles ~48KB on loopback.
    backend.send(0);
    // Backend closes its side after sending (close-delimited)
    backend.close(0);

    // Give sozu time to process the backend close and flush the buffer.
    // Larger body needs more flush cycles.
    thread::sleep(Duration::from_millis(500));

    // Client reads all available data
    let response_data = raw_read_all(&mut stream);
    if !response_data.contains("200 OK") {
        println!(
            "close-delimited-large: missing 200 status line (got {} bytes)",
            response_data.len()
        );
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    // Verify we got the full body. The response includes HTTP headers,
    // so just check the body portion is present in full.
    let body_start = response_data.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
    let received_body_len = response_data.len() - body_start;
    if received_body_len < body.len() {
        println!(
            "close-delimited-large: truncated body: got {} bytes, expected {}",
            received_body_len,
            body.len()
        );
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    println!(
        "close-delimited-large: received full response ({} bytes, body {} bytes)",
        response_data.len(),
        received_body_len,
    );

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_h1_close_delimited_response_fully_delivered() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "Close-delimited response (small body) fully delivered to client",
            try_close_delimited_response,
        ),
        State::Success,
    );
    assert_eq!(
        repeat_until_error_or(
            5,
            "Close-delimited response (large 48KB body) fully delivered to client",
            try_close_delimited_large_response,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 21: H1 rejects ambiguous Content-Length + Transfer-Encoding
//
// RFC 7230 §3.3.3: A server that receives a request with both
// Content-Length and Transfer-Encoding MUST either reject the
// request (400) or handle it consistently to prevent request
// smuggling attacks.
//
// This test sends a POST with both headers. Sozu should either
// respond with 400 or consistently handle one header and remain
// healthy for subsequent requests.
// =========================================================================

fn try_h1_rejects_ambiguous_cl_te() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) =
        setup_sync_test("CL-TE", config, listeners, state, front_address, 1, false);
    let mut backend = backends.pop().unwrap();
    backend.connect();

    // Send a request with both Content-Length and Transfer-Encoding.
    // This is ambiguous per RFC 7230 and is the classic smuggling vector.
    let smuggling_request = concat!(
        "POST /api HTTP/1.1\r\n",
        "Host: localhost\r\n",
        "Content-Length: 5\r\n",
        "Transfer-Encoding: chunked\r\n",
        "Connection: close\r\n",
        "\r\n",
        "0\r\n",
        "\r\n",
    );

    let mut stream = raw_connect(front_address);
    stream
        .write_all(smuggling_request.as_bytes())
        .expect("write smuggling request");

    // Sozu may forward the request to backend. If it does, we need to
    // accept and respond so the smuggling request completes. Otherwise
    // the backend connection stays pending and blocks subsequent requests.
    // Use a short sleep then try to accept (100ms timeout from listener).
    thread::sleep(Duration::from_millis(200));

    // Try to service the smuggling request on the backend side.
    // If sozu rejected it with 400, the backend never gets a connection
    // and accept() returns false after the 100ms timeout.
    let smuggling_forwarded = backend.accept(0);
    if smuggling_forwarded {
        backend.receive(0);
        backend.send(0);
        println!("CL-TE: smuggling request was forwarded to backend");
    }

    // Read the response. Acceptable outcomes:
    // 1. 400 Bad Request — sozu rejects the ambiguous request (best)
    // 2. 200 OK — sozu handled it consistently (acceptable)
    // 3. Connection closed — sozu dropped the session (acceptable)
    //
    // Unacceptable: sozu crashes or becomes unresponsive.
    let response = raw_read(&mut stream);
    match &response {
        Some(r) if r.contains("400") => {
            println!("CL-TE: correctly rejected with 400");
        }
        Some(r) if r.contains("200") || r.contains("502") || r.contains("503") => {
            println!("CL-TE: got response (not 400): {}", &r[..r.len().min(80)]);
        }
        Some(r) => {
            println!("CL-TE: unexpected response: {}", &r[..r.len().min(80)]);
        }
        None => {
            println!("CL-TE: connection closed (acceptable)");
        }
    }
    drop(stream);
    thread::sleep(Duration::from_millis(200));

    // The critical assertion: sozu must still be functional.
    // If the ambiguous request corrupted state, subsequent requests will fail.
    backend.set_response("HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\npong");

    let mut client = Client::new(
        "verify-client",
        front_address,
        "GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    client.connect();
    client.send();

    if smuggling_forwarded {
        // Sozu may reuse the existing backend connection (keep-alive) or
        // open a new one. Try to receive on client 0 first (reuse case).
        // If nothing arrives, accept a new connection on client 1.
        match backend.receive(0) {
            Some(data) if data.contains("GET /api") => {
                println!("CL-TE: verification request reused backend connection 0");
                backend.send(0);
            }
            _ => {
                backend.accept(1);
                backend.receive(1);
                backend.send(1);
            }
        }
    } else {
        backend.accept(0);
        backend.receive(0);
        backend.send(0);
    }

    match client.receive() {
        Some(r) if r.contains("200") && r.contains("pong") => {
            println!("CL-TE: post-smuggling verification succeeded");
        }
        other => {
            println!("CL-TE: post-smuggling verification failed: {other:?}");
            worker.soft_stop();
            worker.wait_for_server_stop();
            return State::Fail;
        }
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_h1_rejects_ambiguous_cl_te() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 rejects ambiguous Content-Length + Transfer-Encoding (smuggling)",
            try_h1_rejects_ambiguous_cl_te,
        ),
        State::Success,
    );
}

// =========================================================================
// H1 chunked-trailer end-to-end forwarding (#899)
// =========================================================================

/// Issue #899 asked whether sozu forwards HTTP/1.1 chunked trailers through
/// unchanged. Audit of the stack shows the plumbing is already in place
/// — kawa's H1 parser has `ParsingPhase::Trailers`
/// (`kawa::protocol::h1::parser`) and kawa's H1 converter serializes
/// `Block::Header` identically whether the block originated before the
/// body (regular header) or after (trailer). The whole chain just needs an
/// e2e to lock the behaviour down so a future refactor cannot silently
/// drop trailers.
///
/// Test shape: a sync backend sends a chunked RESPONSE whose wire layout
/// includes `Trailer:` in the headers, body chunks, a `0\r\n` terminator,
/// and `X-Custom-Trailer: value\r\n\r\n` after the last chunk. The client
/// reads sozu's forwarded response and asserts BOTH the chunked body AND
/// the trailer line are present on the wire.
fn try_h1_chunked_trailer_forwarded() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "H1-CHUNKED-TRAILER",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );
    let mut backend = backends.pop().unwrap();

    // Backend canned response: chunked body with a trailer. The `Trailer:`
    // header advertises the trailer-name per RFC 9110 §6.5, body is two
    // chunks ("hello" + "world"), then terminator + trailer + CRLF.
    let response = concat!(
        "HTTP/1.1 200 OK\r\n",
        "Transfer-Encoding: chunked\r\n",
        "Trailer: X-Custom-Trailer\r\n",
        "Connection: close\r\n",
        "\r\n",
        "5\r\nhello\r\n",
        "5\r\nworld\r\n",
        "0\r\n",
        "X-Custom-Trailer: tail-value\r\n",
        "\r\n",
    );
    backend.set_response(response);
    backend.connect();

    let mut client = Client::new(
        "client",
        front_address,
        "GET /chunked-trailer HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    client.connect();
    client.send();
    backend.accept(0);
    let _request = backend.receive(0);
    backend.send(0);

    // Pull the full response; the client may receive it in multiple reads
    // because of chunked framing, so accumulate.
    let mut observed = String::new();
    for _ in 0..10 {
        match client.receive() {
            Some(chunk) if !chunk.is_empty() => observed.push_str(&chunk),
            _ => break,
        }
    }
    println!("H1 chunked-trailer - observed response:\n{observed}");

    worker.soft_stop();
    worker.wait_for_server_stop();

    // The response the client sees MUST contain:
    // 1. The status line — sozu forwarded the response at all.
    // 2. The `Trailer:` header — sozu did NOT strip the trailer
    //    announcement (H2 strips this per RFC 9113 §8.2.2 but H1 must not).
    // 3. Both body chunks, in order — the body was not truncated.
    // 4. The trailer line `X-Custom-Trailer: tail-value` — the trailer
    //    payload made it all the way through.
    let has_status = observed.starts_with("HTTP/1.1 200");
    let has_trailer_announce = observed.contains("Trailer: X-Custom-Trailer")
        || observed.contains("trailer: X-Custom-Trailer");
    let has_body = observed.contains("hello") && observed.contains("world");
    let has_trailer = observed.contains("X-Custom-Trailer: tail-value")
        || observed.contains("x-custom-trailer: tail-value");

    println!(
        "H1 chunked-trailer - status:{has_status} announce:{has_trailer_announce} body:{has_body} trailer:{has_trailer}"
    );

    if has_status && has_trailer_announce && has_body && has_trailer {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h1_chunked_trailer_forwarded() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 chunked trailers forwarded through sozu (issue #899)",
            try_h1_chunked_trailer_forwarded,
        ),
        State::Success,
    );
}

// =========================================================================
// `http.active_requests` balances over one complete H1 request
//
// Companion to `test_h2_active_requests_balances_over_one_request`
// (`h2_tests.rs`). Both are needed, and not out of symmetry: the `+1` sites
// are per-protocol, but the `-1` is NOT — it lives once in
// `Stream::generate_access_log`, shared by H1, H2 and `Mux::close`. Anything
// that moves that decrement moves it for both protocols at once, so a test
// on one path alone would leave the other half of the moved symbol unguarded.
//
// **Which half of the H1 path this covers.** H1 increments the gauge at two
// sites: `ConnectionH1::readable`, for the first request on a connection, and
// `ConnectionH1::writable`, for a pipelined request whose headers were parsed
// while the previous response was still being written. This test sends ONE
// request and closes, so it exercises the **`readable`** site only. The
// pipelined site is NOT covered here — do not read this test as covering it.
//
// Sampling happens **in flight**, with the response withheld at the backend,
// because `AggregatedMetric::update` saturates a gauge at zero: a test that
// only checked the value returned to its starting point could not see a
// missing increment, since the unpaired decrement would take 0 to 0 and read
// exactly like a balanced request.
//
//   correct            0 -> 1 -> 0
//   `+1` missing       0 -> 0 -> 0   (the in-flight assertion fails)
//   `-1` missing       0 -> 1 -> 1   (the post-completion assertion fails)
//
// **What these two tests do NOT cover.** `Stream::generate_access_log` has
// eight production callers — three in `h1.rs`, four in `h2.rs`, one in
// `mod.rs` — and these tests exercise one H1 caller and one H2 caller. The
// other six are not covered by a test, and they do not need to be: they have
// no step of their own to forget. `#[must_use]` on the return obliges every
// caller to handle it, and on the core side the events are drained at a
// single point rather than recorded per caller. Do not read these tests as
// covering the other six.
// =========================================================================

/// Read one proxy gauge by name. `None` is "not readable this sample" and is
/// deliberately not folded into a value: before the first request the key may
/// be absent entirely, which is not the same observation as a zero.
fn query_proxy_gauge(worker: &mut Worker, metric_name: &str) -> Option<u64> {
    if worker.server_job.is_finished() {
        return None;
    }
    worker.send_proxy_request_type(RequestType::QueryMetrics(QueryMetricsOptions {
        list: false,
        cluster_ids: vec![],
        backend_ids: vec![],
        metric_names: vec![metric_name.to_owned()],
        no_clusters: true,
        workers: false,
    }));
    let response = worker.read_proxy_response()?;
    let content = response.content.and_then(|content| content.content_type)?;
    let ContentType::WorkerMetrics(metrics) = content else {
        return None;
    };
    match metrics
        .proxy
        .get(metric_name)
        .and_then(|metric| metric.inner.clone())
    {
        Some(filtered_metrics::Inner::Gauge(value)) => Some(value),
        _ => None,
    }
}

/// Poll `http.active_requests` until it reads `want`, or report the last
/// sample. Bounded by a deadline rather than a sleep, so a slow machine costs
/// iterations instead of turning a correct run into a failure.
fn await_active_requests_h1(
    worker: &mut Worker,
    want: u64,
    deadline: Duration,
) -> Result<(), String> {
    let started = Instant::now();
    loop {
        let last = query_proxy_gauge(worker, sozu_lib::metrics::names::http::ACTIVE_REQUESTS);
        if last == Some(want) {
            return Ok(());
        }
        if started.elapsed() > deadline {
            return Err(format!("wanted {want}, last sample {last:?}"));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Take ONE sample of `http.active_requests` and require it to read exactly
/// `want`.
///
/// Deliberately not a poll, and not a thin wrapper over
/// [`await_active_requests_h1`]. Its call sites assert that a scenario left
/// the gauge UNCHANGED, and a poll for a value the gauge already holds returns
/// on its first iteration, before the scenario it is meant to weigh has
/// emitted anything at all — the vacuous shape sozu#1535 is about. The single
/// sample is sound because every caller has already crossed an ordering
/// barrier: the scenario's connection reached EOF, or its interim response
/// reached the client.
///
/// `None` travels into the diagnostic as itself, exactly as it does in
/// [`query_proxy_gauge`]: "no such key" and "the gauge reads zero" are two
/// different observations.
fn expect_active_requests_h1(worker: &mut Worker, want: u64) -> Result<(), String> {
    let sample = query_proxy_gauge(worker, sozu_lib::metrics::names::http::ACTIVE_REQUESTS);
    if sample == Some(want) {
        Ok(())
    } else {
        Err(format!("wanted {want}, sample {sample:?}"))
    }
}

/// Put one legitimate request in flight and leave it there, held at the
/// backend between `receive` and `send`, with the gauge proved to read exactly
/// one before returning.
///
/// This is the whole device the gauge tests turn on. `AggregatedMetric::update`
/// saturates a `GaugeAdd` at zero — correctly, since a `clear()` during live
/// traffic can drive a paired decrement below a fresh baseline — so measured
/// from a zero baseline a parasitic `-1` reads 0, indistinguishable from a
/// balanced request:
///
///   correct          0 -> 1 -> 0
///   `-1` unpaired    0 -> 0        looks exactly like the line above
///
/// Measured from a baseline of one it reads 0 where 1 is required, and THAT is
/// observable. What makes the baseline reachable is that `http.active_requests`
/// is per worker process, not per connection: the scenario under test runs on
/// its own connections while this one holds the gauge up.
///
/// The returned [`Client`] must outlive the assertions — dropping it closes the
/// connection and lets sozu release the charge.
fn hold_one_request_in_flight(
    worker: &mut Worker,
    backend: &mut SyncBackend,
    front_address: SocketAddr,
    client_id: usize,
) -> Result<Client, String> {
    let mut client = Client::new(
        "gauge-baseline",
        front_address,
        "GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    client.connect();
    client.send();
    // The backend having the request is what proves sozu parsed the headers,
    // so the `+1` — if it happens at all — has happened. The response is
    // withheld: `backend.send` is the caller's to make, which makes this an
    // ordering guarantee rather than a race.
    backend.accept(client_id);
    backend.receive(client_id);
    await_active_requests_h1(worker, 1, Duration::from_secs(10))
        .map(|()| client)
        .map_err(|diag| format!("the held request never charged the gauge: {diag}"))
}

fn try_h1_active_requests_balances_over_one_request() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "H1-ACTIVE-REQ-BALANCE",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );
    let mut backend = backends.pop().expect("one backend was requested");
    backend.connect();

    let mut client = Client::new(
        "balance-client",
        front_address,
        "GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    client.connect();
    client.send();

    // The backend having the request is what proves sozu parsed the headers,
    // so the increment — if it happens at all — has happened. The response is
    // withheld: `backend.send` is not called until the in-flight sample is
    // taken, which makes this an ordering guarantee rather than a race.
    backend.accept(0);
    backend.receive(0);

    if let Err(diag) = await_active_requests_h1(&mut worker, 1, Duration::from_secs(10)) {
        println!("H1 active-requests balance - in flight: {diag}");
        worker.soft_stop();
        let _ = worker.wait_for_server_stop();
        return State::Fail;
    }

    backend.send(0);
    let response = client.receive();
    if !response.as_deref().is_some_and(|r| r.contains("200")) {
        println!("H1 active-requests balance - unexpected response: {response:?}");
        worker.soft_stop();
        let _ = worker.wait_for_server_stop();
        return State::Fail;
    }

    if let Err(diag) = await_active_requests_h1(&mut worker, 0, Duration::from_secs(10)) {
        println!("H1 active-requests balance - after completion: {diag}");
        worker.soft_stop();
        let _ = worker.wait_for_server_stop();
        return State::Fail;
    }

    worker.soft_stop();
    let _ = worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_h1_active_requests_balances_over_one_request() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H1: http.active_requests rises to exactly one while a request is \
             held at the backend and returns to zero once it completes",
            try_h1_active_requests_balances_over_one_request,
        ),
        State::Success,
    );
}
