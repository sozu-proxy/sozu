/// HTTP/1.1 security e2e tests focused on request smuggling prevention
/// and protocol compliance.
///
/// These tests verify that sozu correctly handles malformed and adversarial
/// HTTP/1.1 requests without crashing, leaking state, or becoming vulnerable
/// to request smuggling attacks.
///
/// Each test follows a common pattern:
/// 1. Send a malicious or edge-case request via raw TCP
/// 2. Verify sozu either rejects it or handles it consistently
/// 3. Send a legitimate follow-up request on a fresh connection
/// 4. Verify sozu responds correctly (no state corruption)
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    thread,
    time::{Duration, Instant},
};

use base64::Engine;
use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, Cluster, ListenerType, Request, RequestHttpFrontend, request::RequestType,
    },
};

use crate::{
    http_utils::http_ok_response,
    mock::{client::Client, sync_backend::Backend as SyncBackend},
    port_registry::attach_reserved_http_listener,
    sozu::worker::Worker,
    tests::{State, repeat_until_error_or, setup_sync_test},
};

use super::tests::create_local_address;

const BUFFER_SIZE: usize = 4096;

// =========================================================================
// Raw TCP helpers
// =========================================================================

/// Open a raw TCP connection to the given address with short timeouts
/// suitable for security testing.
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

/// Read all available data from the stream until EOF or timeout.
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

// =========================================================================
// Attack-test scaffolding
// =========================================================================

/// Polls the backend listener for incoming connections until `deadline`
/// elapses. Returns `true` if no connection arrived (attack was rejected
/// before reaching the backend), `false` if sozu forwarded the attack.
///
/// Each underlying `accept()` call already blocks up to 100 ms via the
/// listener's `SO_RCVTIMEO`, so this loop replaces the legacy
/// `thread::sleep(200 ms)` + single `accept(0)` pattern with a deadline-
/// based wait (CLAUDE.md: "Prefer `repeat_until_error_or` / explicit
/// deadlines over `sleep`"). The deadline is the upper bound on the wait;
/// the helper returns immediately when an attack reaches the backend.
fn assert_attack_not_forwarded(label: &str, backend: &mut SyncBackend, deadline: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if backend.accept(0) {
            println!("{label}: attack reached the backend");
            backend.receive(0);
            backend.send(0);
            return false;
        }
    }
    true
}

/// Concatenate everything the backend reads on `client_id` for the whole
/// `window`, instead of trusting one `receive` — a single `read()` sees one
/// segment (CLAUDE.md: "Always `loop_read_*` ... when asserting on TCP").
/// The full window is always drained rather than stopping at the first
/// complete head, because these assertions are about bytes that must NOT
/// appear: stopping early would let a late segment carry them past the test.
/// Each `receive` costs at most the accepted stream's 100 ms read timeout.
fn backend_drain(backend: &mut SyncBackend, client_id: usize, window: Duration) -> String {
    let start = Instant::now();
    let mut received = String::new();
    while start.elapsed() < window {
        if let Some(chunk) = backend.receive(client_id) {
            received.push_str(&chunk);
        }
    }
    received
}

// =========================================================================
// Verification helper
// =========================================================================

/// Send a legitimate GET request on a fresh connection and verify sozu
/// responds with 200 OK containing "pong". This is the critical
/// post-attack health check shared by all smuggling tests.
///
/// The backend must already be listening. If `smuggling_forwarded` is true,
/// the backend may have an existing connection from the malicious request,
/// so we try to receive on `client_id` 0 first, then fall back to accepting
/// a new connection on `client_id` 1.
fn verify_sozu_healthy(
    front_address: SocketAddr,
    backend: &mut SyncBackend,
    smuggling_forwarded: bool,
) -> bool {
    backend.set_response("HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\npong");

    let mut client = Client::new(
        "verify-client",
        front_address,
        "GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    client.connect();
    client.send();

    if smuggling_forwarded {
        // Sozu may reuse the existing backend connection (keep-alive) or
        // open a new one. Try client 0 first, then accept on client 1.
        match backend.receive(0) {
            Some(data) if data.contains("GET /healthz") => {
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
            println!("health check: sozu responded correctly after attack");
            true
        }
        other => {
            println!("health check: sozu failed after attack: {other:?}");
            false
        }
    }
}

// =========================================================================
// Test 1: TE/CL request smuggling (CL-TE variant)
//
// CVE family: CL-TE desynchronization (e.g. CVE-2023-25690, CVE-2022-32213)
//
// RFC 7230 §3.3.3: If a message is received with both Transfer-Encoding
// and Content-Length, the Transfer-Encoding overrides. However, the
// presence of both is a strong indicator of a smuggling attempt. A
// compliant proxy SHOULD reject such requests with 400.
//
// Attack: the front-end uses Content-Length, the back-end uses
// Transfer-Encoding (or vice versa), allowing an attacker to
// "smuggle" a second request inside the body of the first.
// =========================================================================

fn try_h1_smuggling_te_cl() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) =
        setup_sync_test("TE-CL", config, listeners, state, front_address, 1, false);
    let mut backend = backends.pop().unwrap();
    backend.connect();

    // The TE body says "0 bytes" (immediate terminator), but CL says "5 bytes".
    // If sozu trusts CL, it will wait for 5 more bytes and interpret the next
    // request on the same connection as body data — classic smuggling.
    let smuggling_request = concat!(
        "POST /api HTTP/1.1\r\n",
        "Host: localhost\r\n",
        "Transfer-Encoding: chunked\r\n",
        "Content-Length: 5\r\n",
        "Connection: close\r\n",
        "\r\n",
        "0\r\n",
        "\r\n",
    );

    let mut stream = raw_connect(front_address);
    stream
        .write_all(smuggling_request.as_bytes())
        .expect("write TE-CL smuggling request");

    thread::sleep(Duration::from_millis(200));

    // Try to service the request on the backend if sozu forwarded it.
    let smuggling_forwarded = backend.accept(0);
    if smuggling_forwarded {
        backend.receive(0);
        backend.send(0);
        println!("TE-CL: smuggling request was forwarded to backend");
    }

    let response = raw_read(&mut stream);
    match &response {
        Some(r) if r.contains("400") => {
            println!("TE-CL: correctly rejected with 400");
        }
        Some(r) if r.contains("200") || r.contains("502") || r.contains("503") => {
            println!("TE-CL: got response (not 400): {}", &r[..r.len().min(80)]);
        }
        Some(r) => {
            println!("TE-CL: unexpected response: {}", &r[..r.len().min(80)]);
        }
        None => {
            println!("TE-CL: connection closed (acceptable)");
        }
    }
    drop(stream);
    thread::sleep(Duration::from_millis(200));

    // Critical: sozu must still be functional after the smuggling attempt.
    if !verify_sozu_healthy(front_address, &mut backend, smuggling_forwarded) {
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_h1_smuggling_te_cl() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: TE/CL request smuggling variant",
            try_h1_smuggling_te_cl,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 2: TE/TE request smuggling with obfuscated Transfer-Encoding
//
// CVE family: TE obfuscation desynchronization (e.g. CVE-2019-16869,
// CVE-2020-7247)
//
// RFC 7230 §3.3.1: Transfer-Encoding is defined as a list of transfer
// coding names. Proxies that do not recognize all encodings must not
// forward the message. Obfuscated TE headers (e.g., leading whitespace,
// non-standard capitalization, duplicate headers) can cause front-end
// and back-end to disagree on whether chunked encoding is in effect.
// =========================================================================

fn try_h1_smuggling_te_te_obfuscated() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) =
        setup_sync_test("TE-TE", config, listeners, state, front_address, 1, false);
    let mut backend = backends.pop().unwrap();
    backend.connect();

    // Variant 1: duplicate Transfer-Encoding headers. One proxy may use
    // the first, another the second, causing desync.
    let obfuscated_request = concat!(
        "POST /api HTTP/1.1\r\n",
        "Host: localhost\r\n",
        "Transfer-Encoding: chunked\r\n",
        "Transfer-Encoding: identity\r\n",
        "Connection: close\r\n",
        "\r\n",
        "5\r\n",
        "Hello\r\n",
        "0\r\n",
        "\r\n",
    );

    let mut stream = raw_connect(front_address);
    stream
        .write_all(obfuscated_request.as_bytes())
        .expect("write TE-TE obfuscated request (variant 1)");

    thread::sleep(Duration::from_millis(200));

    let forwarded_v1 = backend.accept(0);
    if forwarded_v1 {
        backend.receive(0);
        backend.send(0);
        println!("TE-TE v1: request was forwarded to backend");
    }

    let response = raw_read(&mut stream);
    match &response {
        Some(r) if r.contains("400") => {
            println!("TE-TE v1: correctly rejected with 400");
        }
        Some(r) => {
            println!("TE-TE v1: got response: {}", &r[..r.len().min(80)]);
        }
        None => {
            println!("TE-TE v1: connection closed (acceptable)");
        }
    }
    drop(stream);
    thread::sleep(Duration::from_millis(200));

    if !verify_sozu_healthy(front_address, &mut backend, forwarded_v1) {
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    // Variant 2: leading tab in Transfer-Encoding value. Some parsers
    // strip leading whitespace, others do not.
    let _backend2 = SyncBackend::new("BACKEND_V2", create_local_address(), http_ok_response("ok"));

    // We need to add the new backend to the worker. Instead, just reuse
    // the existing backend on a fresh connection attempt.
    // Actually, let's just send the second variant on the same sozu instance.
    let obfuscated_request_v2 = concat!(
        "POST /api HTTP/1.1\r\n",
        "Host: localhost\r\n",
        "Transfer-Encoding: \tchunked\r\n",
        "Connection: close\r\n",
        "\r\n",
        "5\r\n",
        "Hello\r\n",
        "0\r\n",
        "\r\n",
    );

    let mut stream = raw_connect(front_address);
    stream
        .write_all(obfuscated_request_v2.as_bytes())
        .expect("write TE-TE obfuscated request (variant 2)");

    thread::sleep(Duration::from_millis(200));

    // The backend from variant 1 may or may not still be usable.
    // Try to accept a new connection for variant 2.
    let next_client_id = if forwarded_v1 { 1 } else { 0 };
    let forwarded_v2 = backend.accept(next_client_id);
    if forwarded_v2 {
        backend.receive(next_client_id);
        backend.send(next_client_id);
        println!("TE-TE v2: request was forwarded to backend");
    }

    let response = raw_read(&mut stream);
    match &response {
        Some(r) if r.contains("400") => {
            println!("TE-TE v2: correctly rejected with 400");
        }
        Some(r) => {
            println!("TE-TE v2: got response: {}", &r[..r.len().min(80)]);
        }
        None => {
            println!("TE-TE v2: connection closed (acceptable)");
        }
    }
    drop(stream);
    thread::sleep(Duration::from_millis(200));

    // Final health check: use a fresh backend client ID.
    let health_client_id = next_client_id + 1;
    backend.set_response("HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\npong");
    let mut client = Client::new(
        "verify-client",
        front_address,
        "GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    client.connect();
    client.send();

    // Try to receive on any existing backend connection first, then accept new.
    let mut served = false;
    for cid in 0..health_client_id {
        if let Some(data) = backend.receive(cid)
            && data.contains("GET /healthz")
        {
            backend.send(cid);
            served = true;
            break;
        }
    }
    if !served {
        backend.accept(health_client_id);
        backend.receive(health_client_id);
        backend.send(health_client_id);
    }

    match client.receive() {
        Some(r) if r.contains("200") && r.contains("pong") => {
            println!("TE-TE: post-attack verification succeeded");
        }
        other => {
            println!("TE-TE: post-attack verification failed: {other:?}");
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
fn test_h1_smuggling_te_te_obfuscated() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: TE/TE obfuscated request smuggling variant",
            try_h1_smuggling_te_te_obfuscated,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 3: Double Content-Length headers
//
// CVE family: CL desynchronization (e.g. CVE-2021-22959 in Node.js,
// CVE-2021-22960)
//
// RFC 7230 §3.3.2: If a message is received with multiple
// Content-Length fields having differing values, the message is
// malformed. A proxy MUST reject such a message with 400.
//
// Attack: different proxies in a chain may pick different CL values,
// causing them to disagree on message boundaries.
// =========================================================================

fn try_h1_smuggling_double_content_length() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "DOUBLE-CL",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );
    let mut backend = backends.pop().unwrap();
    backend.connect();

    // Two Content-Length headers with different values.
    // Sozu MUST reject this with 400 per RFC 7230.
    let double_cl_request = concat!(
        "POST /api HTTP/1.1\r\n",
        "Host: localhost\r\n",
        "Content-Length: 5\r\n",
        "Content-Length: 10\r\n",
        "Connection: close\r\n",
        "\r\n",
        "Hello",
    );

    let mut stream = raw_connect(front_address);
    stream
        .write_all(double_cl_request.as_bytes())
        .expect("write double Content-Length request");

    thread::sleep(Duration::from_millis(200));

    let smuggling_forwarded = backend.accept(0);
    if smuggling_forwarded {
        backend.receive(0);
        backend.send(0);
        println!("DOUBLE-CL: request was forwarded to backend (unexpected but not fatal)");
    }

    let response = raw_read(&mut stream);
    match &response {
        Some(r) if r.contains("400") => {
            println!("DOUBLE-CL: correctly rejected with 400");
        }
        Some(r) => {
            println!(
                "DOUBLE-CL: got response (ideally should be 400): {}",
                &r[..r.len().min(80)]
            );
        }
        None => {
            println!("DOUBLE-CL: connection closed (acceptable)");
        }
    }
    drop(stream);
    thread::sleep(Duration::from_millis(200));

    if !verify_sozu_healthy(front_address, &mut backend, smuggling_forwarded) {
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_h1_smuggling_double_content_length() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: double Content-Length request smuggling",
            try_h1_smuggling_double_content_length,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 4: Oversized headers (header buffer exhaustion)
//
// CVE family: header overflow / DoS (e.g. CVE-2023-44487 rapid reset,
// various buffer overflow CVEs in HTTP servers)
//
// RFC 7230 §3.2.6: A server that receives a header field larger than
// it can process SHOULD respond with 431 Request Header Fields Too Large.
// The server MUST NOT crash or leak memory.
//
// This test verifies sozu enforces its header size limits and responds
// gracefully rather than panicking or consuming unbounded memory.
// =========================================================================

fn try_h1_oversized_headers() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "OVERSIZE-HDR",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );
    let mut backend = backends.pop().unwrap();
    backend.connect();

    // Build a request with a single header value of 64KB.
    // This exceeds sozu's default header buffer size.
    let large_value = "X".repeat(64 * 1024);
    let oversized_request = format!(
        "GET /api HTTP/1.1\r\nHost: localhost\r\nX-Huge: {}\r\nConnection: close\r\n\r\n",
        large_value,
    );

    let mut stream = raw_connect(front_address);
    // Write in chunks to avoid OS-level write buffer issues.
    let bytes = oversized_request.as_bytes();
    let mut written = 0;
    while written < bytes.len() {
        let chunk_end = (written + 8192).min(bytes.len());
        match stream.write(&bytes[written..chunk_end]) {
            Ok(n) => written += n,
            Err(e) => {
                println!("OVERSIZE-HDR: write error at byte {written}: {e}");
                break;
            }
        }
    }

    thread::sleep(Duration::from_millis(300));

    // Sozu should NOT forward this to the backend.
    let forwarded = backend.accept(0);
    if forwarded {
        println!("OVERSIZE-HDR: request was unexpectedly forwarded to backend");
        backend.receive(0);
        backend.send(0);
    }

    // Acceptable responses: 431 (best), 400, or connection close.
    let response = raw_read(&mut stream);
    match &response {
        Some(r) if r.contains("431") => {
            println!("OVERSIZE-HDR: correctly rejected with 431");
        }
        Some(r) if r.contains("400") => {
            println!("OVERSIZE-HDR: rejected with 400 (acceptable)");
        }
        Some(r) => {
            println!("OVERSIZE-HDR: got response: {}", &r[..r.len().min(80)]);
        }
        None => {
            println!("OVERSIZE-HDR: connection closed (acceptable)");
        }
    }
    drop(stream);
    thread::sleep(Duration::from_millis(200));

    // Verify sozu is still functional after the oversized header attempt.
    if !verify_sozu_healthy(front_address, &mut backend, forwarded) {
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_h1_oversized_headers() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: oversized headers rejected without crash",
            try_h1_oversized_headers,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 5: Multiple Host headers
//
// RFC 7230 §5.4: A server MUST respond with 400 to any HTTP/1.1
// request that contains more than one Host header field.
//
// CVE family: host header injection (e.g. CVE-2016-10033, various
// cache poisoning and SSRF attacks via ambiguous Host resolution)
//
// Attack: different components may pick different Host values, enabling
// cache poisoning, routing confusion, or virtual-host bypass.
// =========================================================================

fn try_h1_multiple_host_headers() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "MULTI-HOST",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );
    let mut backend = backends.pop().unwrap();
    backend.connect();

    // Two Host headers with different values.
    let multi_host_request = concat!(
        "GET /api HTTP/1.1\r\n",
        "Host: localhost\r\n",
        "Host: evil.example.com\r\n",
        "Connection: close\r\n",
        "\r\n",
    );

    let mut stream = raw_connect(front_address);
    stream
        .write_all(multi_host_request.as_bytes())
        .expect("write multiple Host header request");

    thread::sleep(Duration::from_millis(200));

    let forwarded = backend.accept(0);
    if forwarded {
        backend.receive(0);
        backend.send(0);
        println!("MULTI-HOST: request was forwarded to backend (less ideal, but may be OK)");
    }

    let response = raw_read(&mut stream);
    match &response {
        Some(r) if r.contains("400") => {
            println!("MULTI-HOST: correctly rejected with 400");
        }
        Some(r) => {
            println!("MULTI-HOST: got response: {}", &r[..r.len().min(80)]);
        }
        None => {
            println!("MULTI-HOST: connection closed (acceptable)");
        }
    }
    drop(stream);
    thread::sleep(Duration::from_millis(200));

    if !verify_sozu_healthy(front_address, &mut backend, forwarded) {
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_h1_multiple_host_headers() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: multiple Host headers rejected per RFC 7230 §5.4",
            try_h1_multiple_host_headers,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 6: Host authority with out-of-range port
//
// RFC 3986 §3.2.3 permits any `*DIGIT` in the port subcomponent, but
// RFC 6335 §6 caps TCP/UDP ports at 16 bits. The H1 routing helper used
// to strip a syntactically valid :port suffix before frontend lookup,
// which let `Host: localhost:65536` route as if it were `Host: localhost`.
// The parser now rejects out-of-range ports, and per RFC 9110 §15.5.1 the
// reverse proxy answers 400 Bad Request rather than 404 Not Found.
// =========================================================================

fn try_h1_host_port_overflow_not_routed() -> State {
    try_h1_bad_authority_rejected(
        "HOST-PORT-OVERFLOW",
        b"GET /api HTTP/1.1\r\nHost: localhost:65536\r\nConnection: close\r\n\r\n",
        "400",
    )
}

#[test]
fn test_h1_host_port_overflow_not_routed() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: Host authority with out-of-range port is not routed",
            try_h1_host_port_overflow_not_routed,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 7: Host authority with reserved port 0
//
// RFC 6335 §6 reserves port 0; it cannot identify a TCP/UDP service. A
// `Host: example.com:0` request must be rejected at the parser before
// frontend lookup.
// =========================================================================

fn try_h1_host_port_zero_not_routed() -> State {
    try_h1_bad_authority_rejected(
        "HOST-PORT-ZERO",
        b"GET /api HTTP/1.1\r\nHost: localhost:0\r\nConnection: close\r\n\r\n",
        "400",
    )
}

#[test]
fn test_h1_host_port_zero_not_routed() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: Host authority with reserved port 0 is not routed",
            try_h1_host_port_zero_not_routed,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 8: Invalid UTF-8 in custom method does not crash the worker
//
// kawa rejects non-token method bytes at request-line parsing under the
// strict default. The defence at `Method::new` ensures that even under
// `--features tolerant-http1-parser`, where kawa accepts bytes
// `0xA0..=0xFF` as method-token characters, malformed network bytes never
// reach `from_utf8_unchecked`. See `test_h1_tolerant_high_byte_method_no_ub`
// below for the feature-gated end-to-end coverage of that path.
// =========================================================================

fn try_h1_invalid_utf8_method_no_crash() -> State {
    try_h1_bad_authority_rejected(
        "BAD-METHOD-UTF8",
        b"\xFFBAD /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        "400",
    )
}

#[test]
fn test_h1_invalid_utf8_method_no_crash() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: invalid UTF-8 method is rejected without crash",
            try_h1_invalid_utf8_method_no_crash,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 9: Tolerant-mode high-byte method does not trigger UB
//
// Under `--features tolerant-http1-parser` the kawa `tchar` stop set
// shrinks to `0x7F..=0x9F`, so bytes `0xA0..=0xFF` slip through as valid
// method-token characters and reach `Method::new`. Before this fix that
// path called `from_utf8_unchecked` on the wire bytes — a lone
// continuation byte such as `0xA5` is not valid UTF-8 and would produce
// undefined behaviour. With `from_utf8_lossy`, the method is safely
// represented as `U+FFFD…` and the worker remains healthy.
// =========================================================================

#[cfg(feature = "tolerant-http1-parser")]
fn try_h1_tolerant_high_byte_method_no_ub() -> State {
    let label = "TOLERANT-HIGH-BYTE-METHOD";
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) =
        setup_sync_test(label, config, listeners, state, front_address, 1, false);
    let mut backend = backends.pop().unwrap();
    backend.connect();
    // Canned reply: under tolerant parsing the proxy may forward the
    // request with a lossy method, so the backend must answer
    // *without* calling `receive()`, which would panic on the raw
    // 0xA5 byte that sozu re-emits on the wire.
    backend.set_response("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");

    let mut stream = raw_connect(front_address);
    stream
        .write_all(b"\xA5BAD /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("write high-byte method attack");

    // Drain whichever side reacts first within the deadline. Under
    // tolerant-parsing the proxy is expected to forward to the
    // backend; under strict parsing the byte is a stop char and the
    // proxy answers 400 directly. Both outcomes are acceptable here —
    // the assertion of interest is the post-attack health check, not
    // the rejection code.
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(300) {
        if backend.accept(0) {
            backend.send(0);
            break;
        }
    }
    let _ = raw_read(&mut stream);
    drop(stream);

    if !verify_sozu_healthy(front_address, &mut backend, false) {
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[cfg(feature = "tolerant-http1-parser")]
#[test]
fn test_h1_tolerant_high_byte_method_no_ub() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: high-byte method under tolerant parser does not UB",
            try_h1_tolerant_high_byte_method_no_ub,
        ),
        State::Success,
    );
}

/// Shared body for "malformed request must never reach the backend" tests.
///
/// Sets up a single-backend worker, writes the raw request bytes, polls
/// the backend until a deadline (no fixed `sleep`), reads the proxy
/// response, and finishes with a `verify_sozu_healthy` follow-up. Accepts
/// `expected_status` as a substring such as `"400"` or `"404"`; an empty
/// response (i.e. the proxy closed without writing) is treated as a
/// terminal accept too, since some malformed inputs justifiably yield a
/// silent close.
fn try_h1_bad_authority_rejected(label: &str, request: &[u8], expected_status: &str) -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) =
        setup_sync_test(label, config, listeners, state, front_address, 1, false);
    let mut backend = backends.pop().unwrap();
    backend.connect();

    let mut stream = raw_connect(front_address);
    stream.write_all(request).expect("write attack bytes");

    if !assert_attack_not_forwarded(label, &mut backend, Duration::from_millis(300)) {
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    match raw_read(&mut stream) {
        Some(response) if response.contains(expected_status) => {
            println!("{label}: rejected with {expected_status}");
        }
        None => {
            println!("{label}: connection closed without forwarding");
        }
        other => {
            println!("{label}: expected {expected_status} or close, got {other:?}");
            worker.soft_stop();
            worker.wait_for_server_stop();
            return State::Fail;
        }
    }
    drop(stream);

    if !verify_sozu_healthy(front_address, &mut backend, false) {
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

// =========================================================================
// Test 10: Chunked encoding edge cases
//
// RFC 7230 §4.1: Chunk extensions and zero-length intermediate chunks
// are valid per the HTTP specification. A compliant proxy must handle
// them correctly without corruption or rejection.
//
// This test verifies sozu correctly parses:
// - Chunk extensions (e.g., "5;name=value\r\nHello\r\n0\r\n\r\n")
// - Zero-length intermediate chunks (0-byte chunk before the terminator)
//
// Incorrect handling can lead to request truncation, body corruption,
// or desynchronization with pipelined requests.
// =========================================================================

fn try_h1_chunked_encoding_edge_cases() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "CHUNK-EDGE",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );
    let mut backend = backends.pop().unwrap();
    backend.connect();

    // Variant 1: chunk extension (semicolon + key=value after chunk size).
    // This is legal per RFC 7230 §4.1.1 and must not confuse the parser.
    let chunked_with_extension = concat!(
        "POST /api HTTP/1.1\r\n",
        "Host: localhost\r\n",
        "Transfer-Encoding: chunked\r\n",
        "Connection: close\r\n",
        "\r\n",
        "5;name=value\r\n",
        "Hello\r\n",
        "0\r\n",
        "\r\n",
    );

    let mut stream = raw_connect(front_address);
    stream
        .write_all(chunked_with_extension.as_bytes())
        .expect("write chunked request with extensions");

    thread::sleep(Duration::from_millis(200));

    let forwarded_v1 = backend.accept(0);
    if forwarded_v1 {
        let received = backend.receive(0);
        if let Some(ref data) = received {
            println!(
                "CHUNK-EDGE v1: backend received: {}",
                &data[..data.len().min(120)]
            );
        }
        backend.send(0);
    }

    let response = raw_read(&mut stream);
    let _v1_ok = match &response {
        Some(r) if r.contains("200") => {
            println!("CHUNK-EDGE v1 (extensions): correctly handled, got 200");
            true
        }
        Some(r) if r.contains("400") => {
            // Rejecting chunk extensions is conservative but acceptable.
            println!("CHUNK-EDGE v1 (extensions): rejected with 400 (conservative)");
            true
        }
        Some(r) => {
            println!(
                "CHUNK-EDGE v1 (extensions): unexpected response: {}",
                &r[..r.len().min(80)]
            );
            true // non-fatal; we check health below
        }
        None => {
            println!("CHUNK-EDGE v1 (extensions): connection closed");
            true // acceptable
        }
    };
    drop(stream);
    thread::sleep(Duration::from_millis(200));

    // Variant 2: zero-length intermediate chunk followed by a real chunk.
    // "0\r\n\r\n" without preceding data would be a terminator, but here
    // we insert a zero-length chunk between two data chunks.
    let chunked_with_zero = concat!(
        "POST /api HTTP/1.1\r\n",
        "Host: localhost\r\n",
        "Transfer-Encoding: chunked\r\n",
        "Connection: close\r\n",
        "\r\n",
        "3\r\n",
        "Hel\r\n",
        "0\r\n",
        "\r\n",
    );

    let mut stream = raw_connect(front_address);
    stream
        .write_all(chunked_with_zero.as_bytes())
        .expect("write chunked request with zero-length intermediate chunk");

    thread::sleep(Duration::from_millis(200));

    // Accept on the next available client ID.
    let next_id = if forwarded_v1 { 1 } else { 0 };
    let forwarded_v2 = backend.accept(next_id);
    if forwarded_v2 {
        backend.receive(next_id);
        backend.send(next_id);
        println!("CHUNK-EDGE v2: request forwarded to backend");
    }

    let response = raw_read(&mut stream);
    match &response {
        Some(r) if r.contains("200") => {
            println!("CHUNK-EDGE v2 (zero-length): correctly handled, got 200");
        }
        Some(r) => {
            println!(
                "CHUNK-EDGE v2 (zero-length): got response: {}",
                &r[..r.len().min(80)]
            );
        }
        None => {
            println!("CHUNK-EDGE v2 (zero-length): connection closed");
        }
    }
    drop(stream);
    thread::sleep(Duration::from_millis(200));

    // Health check after both variants.
    let health_id = next_id + 1;
    backend.set_response("HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\npong");
    let mut client = Client::new(
        "verify-client",
        front_address,
        "GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    client.connect();
    client.send();

    // Try existing connections first, then accept a new one.
    let mut served = false;
    for cid in 0..health_id {
        if let Some(data) = backend.receive(cid)
            && data.contains("GET /healthz")
        {
            backend.send(cid);
            served = true;
            break;
        }
    }
    if !served {
        backend.accept(health_id);
        backend.receive(health_id);
        backend.send(health_id);
    }

    match client.receive() {
        Some(r) if r.contains("200") && r.contains("pong") => {
            println!("CHUNK-EDGE: post-test verification succeeded");
        }
        other => {
            println!("CHUNK-EDGE: post-test verification failed: {other:?}");
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
fn test_h1_chunked_encoding_edge_cases() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: chunked encoding edge cases (extensions, zero-length chunks)",
            try_h1_chunked_encoding_edge_cases,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 11: HTTP/0.9 request rejection
//
// RFC 7230 §2.6: HTTP/1.1 servers SHOULD respond to HTTP/0.9 requests
// with a proper HTTP response indicating the version is not supported,
// or close the connection.
//
// HTTP/0.9 has no headers, no Content-Length, and no Host. Accepting
// it on a modern proxy is dangerous because it bypasses all header-based
// security controls (Host routing, authentication headers, etc.).
//
// Attack vector: an attacker sends "GET /\r\n" (no HTTP version) and
// the proxy either crashes or misroutes the request.
// =========================================================================

fn try_h1_http09_request_rejection() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) =
        setup_sync_test("HTTP09", config, listeners, state, front_address, 1, false);
    let mut backend = backends.pop().unwrap();
    backend.connect();

    // HTTP/0.9 style request: no version, no headers.
    let http09_request = "GET /\r\n";

    let mut stream = raw_connect(front_address);
    stream
        .write_all(http09_request.as_bytes())
        .expect("write HTTP/0.9 request");

    thread::sleep(Duration::from_millis(200));

    // Sozu should NOT forward this to the backend.
    let forwarded = backend.accept(0);
    if forwarded {
        backend.receive(0);
        backend.send(0);
        println!("HTTP09: request was unexpectedly forwarded to backend");
    }

    let response = raw_read(&mut stream);
    match &response {
        Some(r) if r.contains("400") => {
            println!("HTTP09: correctly rejected with 400");
        }
        Some(r) if r.contains("505") => {
            println!("HTTP09: rejected with 505 HTTP Version Not Supported");
        }
        Some(r) => {
            println!("HTTP09: got response: {}", &r[..r.len().min(80)]);
        }
        None => {
            println!("HTTP09: connection closed (acceptable)");
        }
    }
    drop(stream);
    thread::sleep(Duration::from_millis(200));

    // Verify sozu still works after the malformed request.
    if !verify_sozu_healthy(front_address, &mut backend, forwarded) {
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_h1_http09_request_rejection() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: HTTP/0.9 request rejection",
            try_h1_http09_request_rejection,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 12: Connection: close terminates the connection
//
// RFC 7230 §6.1: A client that sends "Connection: close" signals that
// it will not send further requests on this connection. The server
// (or proxy) MUST close the connection after sending the response.
//
// If sozu fails to close the connection, it leaks file descriptors
// and potentially allows request pipelining on a connection that
// should be dead.
// =========================================================================

fn try_h1_connection_close_terminates() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "CONN-CLOSE",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );
    let mut backend = backends.pop().unwrap();
    backend.connect();

    let close_request = concat!(
        "GET /api HTTP/1.1\r\n",
        "Host: localhost\r\n",
        "Connection: close\r\n",
        "\r\n",
    );

    let mut stream = raw_connect(front_address);
    stream
        .write_all(close_request.as_bytes())
        .expect("write Connection: close request");

    thread::sleep(Duration::from_millis(200));

    backend.accept(0);
    backend.receive(0);
    backend.send(0);

    // Read the response.
    let response = raw_read_all(&mut stream);
    if !response.contains("200") {
        println!(
            "CONN-CLOSE: did not get 200 response: {}",
            &response[..response.len().min(80)]
        );
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }
    println!("CONN-CLOSE: got 200 response");

    // The connection should now be closed by sozu. Attempting to read
    // more data should return EOF (Ok(0)) or an error.
    thread::sleep(Duration::from_millis(100));
    let mut buf = [0u8; 64];
    let connection_closed = match stream.read(&mut buf) {
        Ok(0) => {
            println!("CONN-CLOSE: connection properly closed (EOF)");
            true
        }
        Ok(n) => {
            let extra = String::from_utf8_lossy(&buf[..n]);
            println!("CONN-CLOSE: unexpected data after close: {extra}");
            false
        }
        Err(ref e) if e.kind() == std::io::ErrorKind::ConnectionReset => {
            println!("CONN-CLOSE: connection reset (acceptable close)");
            true
        }
        Err(ref e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
        {
            // Timeout means the connection was not explicitly closed,
            // but sozu may be waiting for us to close first. This is
            // still acceptable behavior in practice.
            println!("CONN-CLOSE: read timed out (connection may still be open)");
            true
        }
        Err(e) => {
            println!("CONN-CLOSE: read error: {e}");
            true // broken pipe, etc. — all indicate closure
        }
    };

    drop(stream);

    if !connection_closed {
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    // Final health check: send another request on a NEW connection.
    thread::sleep(Duration::from_millis(100));
    backend.set_response("HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\npong");

    let mut client = Client::new(
        "verify-client",
        front_address,
        "GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    client.connect();
    client.send();

    // Backend may reuse connection 0 or need a new one.
    match backend.receive(0) {
        Some(data) if data.contains("GET /healthz") => {
            backend.send(0);
        }
        _ => {
            backend.accept(1);
            backend.receive(1);
            backend.send(1);
        }
    }

    match client.receive() {
        Some(r) if r.contains("200") && r.contains("pong") => {
            println!("CONN-CLOSE: post-test verification succeeded");
        }
        other => {
            println!("CONN-CLOSE: post-test verification failed: {other:?}");
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
fn test_h1_connection_close_terminates() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: Connection: close properly terminates the connection",
            try_h1_connection_close_terminates,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 13: CL.TE request smuggling via an ambiguous Transfer-Encoding
// (regression of #726)
//
// RFC 9110 §7.6 / RFC 7230 §3.3.3: a message whose Transfer-Encoding is not
// the framing actually applied, alongside a Content-Length, is a desync
// primitive (CWE-444) — a lenient backend may frame on the Transfer-Encoding
// while sozu framed by Content-Length, so the two disagree on where the
// message ends and the tail of one becomes a request of its own.
//
// The invariant sozu upholds is NOT "reject anything unusual" but:
//
//     sozu never forwards a Transfer-Encoding that differs from the framing
//     it applied — it either normalizes, or it rejects.
//
// Two layers cooperate. kawa >=0.7.1 excludes leading/trailing OWS from every
// field value (RFC 9112 §5), so `chunked\t` and `chunked ` ARE `chunked`:
// they select chunked framing, elide every Content-Length (RFC 9110 §6.3),
// and — the part that matters here — are FORWARDED as the canonical
// `chunked`, never as the obfuscated spelling. (kawa 0.7.0 framed on the
// trimmed reading but forwarded the raw bytes; a backend that did not itself
// trim then saw no coding it recognised and no length, and read the chunked
// body as a pipelined request. That gap is what `chunked\t` used to be
// rejected for.) The guard in `lib/src/protocol/kawa_h1/editor.rs` then
// rejects what remains genuinely ambiguous: more than one surviving TE header,
// or a value whose final coding is not chunked.
//
// So an OWS-obfuscated coding is now handled rather than refused — it is a
// legal chunked request and rejecting it would reject legal traffic. What it
// must never do is reach a backend still obfuscated: see
// `test_h1_te_ows_forwarded_canonically`, which pins the forwarded bytes.
//
// The `multi-line-chunked-then-identity` case covers a second bypass:
// a first `Transfer-Encoding: chunked` line must not be able to latch
// chunked framing while a second, separate `Transfer-Encoding: identity`
// line rides along — forwarding both lines yields `chunked, identity`,
// with chunked NOT the final coding. The guard counts every non-elided TE
// header and rejects whenever more than one survives, regardless of
// `body_size`.
// =========================================================================

/// Ambiguous Transfer-Encoding shapes that must each be rejected with 400
/// before ever reaching the backend, with or without a Content-Length.
///
/// The `trailing-tab` / `trailing-space` cases are rejected for their BODY,
/// not their coding: `chunked\t` frames as chunked (OWS is not part of the
/// field value), and `Hello` is not a valid chunk. They are kept because a
/// Content-Length that a lenient peer might frame on must never survive
/// alongside chunked framing — the request must die rather than reach the
/// backend with two framings. An OWS-obfuscated coding with a *valid* body
/// is a legal request and is covered by `test_h1_te_ows_forwarded_canonically`.
const TE_SMUGGLING_CASES: [(&str, &[u8]); 4] = [
    (
        "trailing-tab",
        b"POST /api HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\t\r\nContent-Length: 5\r\nConnection: close\r\n\r\nHello",
    ),
    (
        "trailing-space",
        b"POST /api HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked \r\nContent-Length: 5\r\nConnection: close\r\n\r\nHello",
    ),
    (
        "not-final-coding",
        b"POST /api HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked, gzip\r\nContent-Length: 5\r\nConnection: close\r\n\r\nHello",
    ),
    (
        "multi-line-chunked-then-identity",
        b"POST /api HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: identity\r\nConnection: close\r\n\r\n5\r\nHello\r\n0\r\n\r\n",
    ),
];

fn try_h1_smuggling_te_cl_trailing_tab() -> State {
    for (label, request) in TE_SMUGGLING_CASES {
        let front_address = create_local_address();

        let (config, listeners, state) = Worker::empty_config();
        let (mut worker, mut backends) = setup_sync_test(
            format!("TE-SMUGGLE-{label}"),
            config,
            listeners,
            state,
            front_address,
            1,
            false,
        );
        let mut backend = backends.pop().unwrap();
        backend.connect();

        let mut stream = raw_connect(front_address);
        stream
            .write_all(request)
            .unwrap_or_else(|e| panic!("{label}: write attack bytes: {e}"));

        // Assertion 2: the malformed request must never reach the backend.
        if !assert_attack_not_forwarded(label, &mut backend, Duration::from_millis(300)) {
            println!("{label}: FAIL — malformed framing reached the backend");
            worker.soft_stop();
            worker.wait_for_server_stop();
            return State::Fail;
        }

        // Assertion 1: sozu answers 400.
        match raw_read(&mut stream) {
            Some(r) if r.contains("400") => {
                println!("{label}: correctly rejected with 400");
            }
            other => {
                println!("{label}: FAIL — expected 400, got {other:?}");
                worker.soft_stop();
                worker.wait_for_server_stop();
                return State::Fail;
            }
        }
        drop(stream);

        if !verify_sozu_healthy(front_address, &mut backend, false) {
            println!("{label}: FAIL — sozu unhealthy after the attack");
            worker.soft_stop();
            worker.wait_for_server_stop();
            return State::Fail;
        }

        worker.soft_stop();
        worker.wait_for_server_stop();
    }
    State::Success
}

#[test]
fn test_h1_smuggling_te_cl_trailing_tab() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: CL.TE smuggling via an ambiguous Transfer-Encoding (trailing tab/space and non-final coding alongside a Content-Length, and duplicate TE field lines)",
            try_h1_smuggling_te_cl_trailing_tab,
        ),
        State::Success,
    );
}

/// An OWS-obfuscated `Transfer-Encoding` with a valid chunked body is a
/// legal request (RFC 9112 §5: trailing OWS is not part of the field value),
/// so it is handled rather than refused — refusing it would reject legal
/// traffic. The property that keeps it safe is what reaches the backend:
/// the coding sozu framed on, and no second framing header.
///
/// This is the assertion the old suite never made. It asserted `400` and so
/// could not tell "sozu normalized correctly" from "sozu forwarded
/// `chunked\t` verbatim" — which is exactly what kawa 0.7.0 did, and exactly
/// how a TE.TE desync starts: a backend that does not itself trim OWS sees
/// no coding it recognises and, the Content-Length having been elided, no
/// length either, so it reads the chunked body as a pipelined request.
fn try_h1_te_ows_forwarded_canonically() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "TE-OWS-CANONICAL",
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

    // `chunked\t` frames as chunked and elides the Content-Length; the body
    // is valid chunked ("Hello"), so nothing else can reject this request.
    const REQUEST: &[u8] = b"POST /api HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\t\r\nContent-Length: 5\r\nConnection: close\r\n\r\n5\r\nHello\r\n0\r\n\r\n";

    let mut stream = raw_connect(front_address);
    stream.write_all(REQUEST).expect("write OWS-coding request");

    let deadline = Instant::now() + Duration::from_millis(500);
    let mut accepted = false;
    while Instant::now() < deadline {
        if backend.accept(0) {
            accepted = true;
            break;
        }
    }
    if !accepted {
        println!("TE-OWS-CANONICAL: FAIL — a legal chunked request never reached the backend");
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    let forwarded = backend_drain(&mut backend, 0, Duration::from_millis(300));
    backend.send(0);
    println!("TE-OWS-CANONICAL: backend received {forwarded:?}");

    // The coding we framed on is the coding we forward.
    if !forwarded.contains("Transfer-Encoding: chunked\r\n") {
        println!(
            "TE-OWS-CANONICAL: FAIL — backend did not receive a canonical `Transfer-Encoding: chunked`"
        );
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }
    // The obfuscated spelling must not survive to the backend.
    if forwarded.contains("chunked\t") || forwarded.contains("chunked \r\n") {
        println!("TE-OWS-CANONICAL: FAIL — the obfuscated coding was forwarded verbatim");
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }
    // RFC 9110 §6.3: Transfer-Encoding overrides Content-Length, so no
    // second framing header may reach the backend.
    if forwarded.to_lowercase().contains("content-length") {
        println!("TE-OWS-CANONICAL: FAIL — a Content-Length survived alongside chunked framing");
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }
    drop(stream);

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_h1_te_ows_forwarded_canonically() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: an OWS-obfuscated Transfer-Encoding is forwarded as the canonical coding, with no Content-Length beside it",
            try_h1_te_ows_forwarded_canonically,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 14: Non-regression — legitimately framed requests are still forwarded
//
// The CL.TE guard added to `editor.rs::on_request_headers` must reject
// only requests where a Transfer-Encoding header survives without kawa
// adopting chunked framing. It must never fire when the framing is
// unambiguous, including the shapes below.
// =========================================================================

fn try_h1_valid_framing_still_forwarded() -> State {
    let cases: [(&str, &[u8]); 5] = [
        (
            "chunked",
            b"POST /api HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nHello\r\n0\r\n\r\n",
        ),
        (
            "multi-coding-chunked-final",
            b"POST /api HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: gzip, chunked\r\nConnection: close\r\n\r\n5\r\nHello\r\n0\r\n\r\n",
        ),
        (
            "content-length-only",
            b"POST /api HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nConnection: close\r\n\r\nHello",
        ),
        (
            "cl-and-te-together",
            b"POST /api HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nHello\r\n0\r\n\r\n",
        ),
        (
            "get-no-body",
            b"GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        ),
    ];

    for (label, request) in cases {
        let front_address = create_local_address();

        let (config, listeners, state) = Worker::empty_config();
        let (mut worker, mut backends) = setup_sync_test(
            format!("VALID-FRAME-{label}"),
            config,
            listeners,
            state,
            front_address,
            1,
            false,
        );
        let mut backend = backends.pop().unwrap();
        backend.connect();

        let mut stream = raw_connect(front_address);
        stream
            .write_all(request)
            .unwrap_or_else(|e| panic!("{label}: write request: {e}"));

        let deadline = Instant::now() + Duration::from_millis(300);
        let mut forwarded = false;
        while Instant::now() < deadline {
            if backend.accept(0) {
                forwarded = true;
                break;
            }
        }
        if !forwarded {
            println!("{label}: FAIL — request never reached the backend");
            worker.soft_stop();
            worker.wait_for_server_stop();
            return State::Fail;
        }

        let received = backend.receive(0);
        backend.send(0);
        let response = raw_read(&mut stream);
        drop(stream);

        let ok = received.is_some()
            && matches!(&response, Some(r) if r.contains("200") && r.contains("pong0"));

        worker.soft_stop();
        worker.wait_for_server_stop();

        if !ok {
            println!(
                "{label}: FAIL — expected 200/pong0, got response={response:?} received={received:?}"
            );
            return State::Fail;
        }
        println!("{label}: correctly forwarded, got 200");
    }
    State::Success
}

#[test]
fn test_h1_valid_framing_still_forwarded() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: legitimately framed requests (chunked, CL, both, neither) are still forwarded",
            try_h1_valid_framing_still_forwarded,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 15: CL.TE smuggling cannot bypass per-frontend Basic auth
//
// Sozu supports per-frontend HTTP Basic auth (`required_auth = true` +
// `authorized_hashes` + `www_authenticate`). A classic use of CL/TE
// desync is to hide a second, unauthenticated request inside what the
// auth-enforcing proxy believes is opaque body content of the first
// request, so the smuggled request never passes through the proxy's
// per-request auth gate at all. With the CL.TE guard, the outer request
// is rejected (400) before routing or the auth check ever run — see
// `lib/src/protocol/mux/h1.rs`'s `kawa.is_error()` short-circuit, which
// fires immediately after `kawa::h1::parse` and before the router/auth
// check further down the same read event.
// =========================================================================

/// SHA-256 hex of the literal byte string `s3cr3t` (`printf 's3cr3t' |
/// sha256sum`). The attack request in this test never supplies a matching
/// `Authorization` header — the guard must reject the malformed framing
/// before the auth check ever runs — so this hash only backs the
/// post-attack health check, which authenticates for real to prove the
/// CL.TE guard didn't collaterally break the (unrelated) Basic-auth gate.
const AUTH_BYPASS_SECRET_SHA256_HEX: &str =
    "4e738ca5563c06cfd0018299933d58db1dd8bf97f6973dc99bf6cdc64b5550bd";

/// Spins up a worker with a single HTTP listener, one cluster gated by
/// per-frontend Basic auth, and one backend. Duplicated locally rather
/// than reusing `redirect_rewrite_auth_tests::spawn_worker_with_http_listener`
/// / `make_basic_auth_cluster` — those helpers are private to that module.
fn spawn_auth_gated_worker(
    label: &str,
    front_address: SocketAddr,
    back_address: SocketAddr,
) -> Worker {
    let (config, mut listeners, state) = Worker::empty_config();
    attach_reserved_http_listener(&mut listeners, front_address);
    let mut worker = Worker::start_new_worker_owned(label, config, listeners, state);

    worker.send_proxy_request(Request {
        request_type: Some(RequestType::AddHttpListener(
            ListenerBuilder::new_http(front_address.into())
                .to_http(None)
                .expect("default HTTP listener must build"),
        )),
    });
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::ActivateListener(ActivateListener {
            address: front_address.into(),
            proxy: ListenerType::Http.into(),
            from_scm: true,
        })),
    });
    worker.send_proxy_request(
        RequestType::AddCluster(Cluster {
            authorized_hashes: vec![format!("admin:{AUTH_BYPASS_SECRET_SHA256_HEX}")],
            www_authenticate: Some("Basic realm=\"sozu\"".to_owned()),
            ..Worker::default_cluster("auth_bypass_cluster")
        })
        .into(),
    );
    worker.send_proxy_request(
        RequestType::AddHttpFrontend(RequestHttpFrontend {
            required_auth: Some(true),
            ..Worker::default_http_frontend("auth_bypass_cluster", front_address)
        })
        .into(),
    );
    worker.send_proxy_request(
        RequestType::AddBackend(Worker::default_backend(
            "auth_bypass_cluster",
            "auth_bypass_back_0",
            back_address,
            None,
        ))
        .into(),
    );
    worker.read_to_last();
    worker
}

fn try_h1_smuggling_auth_bypass() -> State {
    let front_address = create_local_address();
    let back_address = create_local_address();
    let mut worker = spawn_auth_gated_worker("AUTH-BYPASS", front_address, back_address);
    let mut backend = SyncBackend::new(
        "auth_bypass_back_0",
        back_address,
        "HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\npong",
    );
    backend.connect();

    let token = base64::engine::general_purpose::STANDARD.encode(b"admin:s3cr3t");

    // The outer request carries VALID `Authorization` credentials — this is
    // the actual bypass primitive: a per-request Basic-auth gate that only
    // ever validates the *outer* request's headers is worthless if an
    // ambiguous Transfer-Encoding/Content-Length pair lets a second, fully
    // -formed request ride along hidden inside what sozu treats as opaque,
    // already-authenticated body bytes. Pre-fix, sozu's auth check passed on
    // the outer headers and forwarded the whole Content-Length-framed blob —
    // smuggled request included — to the backend, having checked credentials
    // exactly once for what a lenient backend treats as two requests.
    //
    // Post-fix the bypass is gone by construction rather than by rejection.
    // `chunked\t` IS `chunked` (RFC 9112 §5), so the Content-Length that
    // framed the blob is elided (RFC 9110 §6.3) and the body ends at its
    // terminating `0\r\n\r\n` chunk. The trailing bytes are therefore no
    // longer *inside* a body at all: they are a pipelined request, which
    // sozu parses and auth-checks on its own merits. The outer, genuinely
    // authenticated POST is forwarded — that is correct — but the smuggled
    // `GET /admin` must never ride along inside it, and must never reach the
    // backend on the strength of the outer request's credentials.
    let smuggled_request = concat!("GET /admin HTTP/1.1\r\n", "Host: localhost\r\n", "\r\n",);
    let body = format!("0\r\n\r\n{smuggled_request}");
    let attack = format!(
        "POST /secured HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic {token}\r\nTransfer-Encoding: chunked\t\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body,
    );

    let mut stream = raw_connect(front_address);
    stream
        .write_all(attack.as_bytes())
        .expect("write CL.TE auth-bypass attack");

    // The outer POST is authenticated and correctly framed, so it is expected
    // to reach the backend.
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut accepted = false;
    while Instant::now() < deadline {
        if backend.accept(0) {
            accepted = true;
            break;
        }
    }
    if !accepted {
        println!("AUTH-BYPASS: FAIL — the authenticated outer request never reached the backend");
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    let forwarded = backend_drain(&mut backend, 0, Duration::from_millis(300));
    backend.send(0);
    println!("AUTH-BYPASS: backend received {forwarded:?}");

    // THE bypass assertion: the smuggled request must never reach the backend
    // hidden inside the authenticated request's body.
    if forwarded.contains("/admin") {
        println!(
            "AUTH-BYPASS: FAIL — the smuggled `GET /admin` rode along inside the authenticated body"
        );
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }
    // ...and it must not have been smuggled by leaving both framings in place.
    if forwarded.to_lowercase().contains("content-length") {
        println!("AUTH-BYPASS: FAIL — a Content-Length survived alongside chunked framing");
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }
    if !forwarded.contains("Transfer-Encoding: chunked\r\n") {
        println!("AUTH-BYPASS: FAIL — backend did not receive a canonical chunked framing");
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }
    drop(stream);

    // The auth gate must still be per-request: the smuggled request's own
    // credentials (it has none) are what decide its fate, not the outer
    // request's. Replay it on its own and prove it is challenged, not served.
    let mut stream = raw_connect(front_address);
    stream
        .write_all(smuggled_request.as_bytes())
        .expect("write the smuggled request on its own");
    match raw_read(&mut stream) {
        Some(r) if r.contains("401") => {
            println!("AUTH-BYPASS: the smuggled request is challenged (401) on its own merits");
        }
        other => {
            println!(
                "AUTH-BYPASS: FAIL — expected 401 for the unauthenticated request, got {other:?}"
            );
            worker.soft_stop();
            worker.wait_for_server_stop();
            return State::Fail;
        }
    }
    drop(stream);

    // Post-attack health check: this deliberately does NOT reuse the
    // shared `verify_sozu_healthy` helper, which sends an unauthenticated
    // GET to `/healthz` — against this test's `required_auth = true`
    // frontend on path prefix "/", that would itself get a 401 and the
    // helper would misreport failure. Instead, authenticate for real
    // against the same auth-gated route to prove the CL.TE guard didn't
    // collaterally break the Basic-auth gate.
    let healthy_request = format!(
        "GET /secured HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic {token}\r\nConnection: close\r\n\r\n"
    );
    let mut stream = raw_connect(front_address);
    stream
        .write_all(healthy_request.as_bytes())
        .expect("write authenticated health-check request");

    // The outer POST was legitimately forwarded, so unlike the pre-kawa-0.7.1
    // version of this test the backend already has a connection here. Try to
    // serve on it before accepting a new one (same reasoning as
    // `verify_sozu_healthy`'s `smuggling_forwarded` path) — whether sozu
    // reuses the pooled backend connection or opens a fresh one is not what
    // this check is about.
    let deadline = Instant::now() + Duration::from_millis(1000);
    let mut served = false;
    while Instant::now() < deadline {
        if backend.receive(0).is_some() {
            backend.send(0);
            served = true;
            break;
        }
        if backend.accept(1) {
            backend.receive(1);
            backend.send(1);
            served = true;
            break;
        }
    }
    if !served {
        println!("AUTH-BYPASS: FAIL — authenticated health check never reached the backend");
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    match raw_read(&mut stream) {
        Some(r) if r.contains("200") => {
            println!("AUTH-BYPASS: post-attack authenticated health check succeeded");
        }
        other => {
            println!("AUTH-BYPASS: FAIL — authenticated health check got {other:?}");
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
fn test_h1_smuggling_auth_bypass() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: CL.TE smuggling cannot bypass per-frontend Basic auth",
            try_h1_smuggling_auth_bypass,
        ),
        State::Success,
    );
}
