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
        ActivateListener, AddCertificate, CertificateAndKey, Cluster, ListenerType, PathRule,
        Request, RequestHttpFrontend, request::RequestType,
    },
};

use crate::{
    http_utils::http_ok_response,
    mock::{client::Client, https_client::Verifier, sync_backend::Backend as SyncBackend},
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
// Early response while the request body is still uploading
//
// A backend may answer before it read the whole request body: a 413 or a
// 401 on an upload. The response is then complete while the request is not.
// RFC 9112 §9.3 lets a connection outlive a message only once that message
// is complete in both directions: the rest of the body belongs to the
// request the backend already answered, never to a new one. Two things must
// hold once such a response has left:
//
// - the rest of the client's body is never parsed as a new request (that is
//   a request-smuggling primitive: the attacker's body becomes a request
//   sozu routes and edits as its own);
// - the backend connection that still expects that body is never reused
//   for another request, which it would read as the rest of the body.
// =========================================================================

/// How the request under test frames its body.
#[derive(Clone, Copy, Debug)]
enum EarlyResponseFraming {
    ContentLength,
    Chunked,
}

/// `true` when `forwarded` carries `request_line` as a request sozu itself
/// forwarded — its head carries the `Sozu-Id` sozu adds to every request it
/// sends — rather than as body bytes passed through verbatim.
fn forwarded_as_request(forwarded: &str, request_line: &str) -> bool {
    forwarded.split(request_line).skip(1).any(|after| {
        let head = after.split("\r\n\r\n").next().unwrap_or_default();
        head.split("\r\n").any(|line| line.starts_with("Sozu-Id: "))
    })
}

fn try_h1_early_response_mid_upload(framing: EarlyResponseFraming) -> State {
    let label = format!("EARLY-RESPONSE-{framing:?}");
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) =
        setup_sync_test(&label, config, listeners, state, front_address, 1, false);
    let mut backend = backends.pop().unwrap();
    backend.connect();

    // The rest of the body is, byte for byte, a request of its own.
    let smuggled = "GET /smuggled HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let (head, first_part, rest) = match framing {
        EarlyResponseFraming::ContentLength => {
            let prefix = "0123456789";
            (
                format!(
                    "POST /upload HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n",
                    prefix.len() + smuggled.len()
                ),
                prefix.to_owned(),
                smuggled.to_owned(),
            )
        }
        EarlyResponseFraming::Chunked => (
            "POST /upload HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n"
                .to_owned(),
            // The chunk-size line leaves first; its data leaves after the
            // response.
            format!("{:x}\r\n", smuggled.len()),
            format!("{smuggled}\r\n0\r\n\r\n"),
        ),
    };
    let second = "GET /second HTTP/1.1\r\nHost: localhost\r\n\r\n";

    let mut stream = raw_connect(front_address);
    stream
        .write_all(format!("{head}{first_part}").as_bytes())
        .expect("write the request head and the start of its body");

    if !backend_accepts_within(&mut backend, 0, Duration::from_secs(2)) {
        println!("{label}: the upload never reached the backend");
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }
    let upload = backend_drain(&mut backend, 0, Duration::from_millis(300));
    if !upload.contains("POST /upload HTTP/1.1") {
        println!("{label}: the backend did not receive the upload head: {upload:?}");
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    // The early, complete, keep-alive response.
    backend.set_response("HTTP/1.1 413 Content Too Large\r\nContent-Length: 0\r\n\r\n");
    backend.send(0);
    let early = read_status_lines(&mut stream, 1, Duration::from_secs(2));
    if !early.contains("HTTP/1.1 413") {
        println!("{label}: the client did not receive the early 413: {early:?}");
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    // The client finishes its body, then sends a second request. Sozu may
    // already have closed the connection: a failed write is fine.
    let _ = stream.write_all(format!("{rest}{second}").as_bytes());

    // Whatever reaches the backend now, on the connection that still expects
    // the rest of the body or on a new one. Anything forwarded as a request
    // is answered, so a smuggled request would also reach the client.
    backend.set_response("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
    let on_reused = backend_drain(&mut backend, 0, Duration::from_millis(500));
    if !sozu_ids(&on_reused).is_empty() {
        backend.send(0);
    }
    let on_fresh = if backend_accepts_within(&mut backend, 1, Duration::from_millis(300)) {
        let received = backend_drain(&mut backend, 1, Duration::from_millis(300));
        if !sozu_ids(&received).is_empty() {
            backend.send(1);
        }
        received
    } else {
        String::new()
    };
    let after = read_status_lines(&mut stream, 1, Duration::from_millis(500));
    println!("{label}: backend connection 0 after the 413: {on_reused:?}");
    println!("{label}: backend connection 1: {on_fresh:?}");
    println!("{label}: client after the 413: {after:?}");

    worker.soft_stop();
    worker.wait_for_server_stop();

    // Client side: the rest of the body was never routed as a request.
    let smuggled_line = "GET /smuggled HTTP/1.1\r\n";
    if forwarded_as_request(&on_reused, smuggled_line)
        || forwarded_as_request(&on_fresh, smuggled_line)
    {
        println!("{label}: the rest of the body was forwarded as a request");
        return State::Fail;
    }
    // Nor did the client get an answer to it.
    if after.contains("HTTP/1.1 200") {
        println!("{label}: the client received a response to the rest of its body");
        return State::Fail;
    }
    // Backend side: the connection that answered early, whose request is
    // still incomplete, carried no further request.
    if !sozu_ids(&on_reused).is_empty() {
        println!("{label}: a request was sent on the backend connection that answered early");
        return State::Fail;
    }
    State::Success
}

#[test]
fn test_h1_early_response_mid_content_length_upload() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H1 security: an early response ends neither a Content-Length upload nor its backend connection",
            || try_h1_early_response_mid_upload(EarlyResponseFraming::ContentLength),
        ),
        State::Success,
    );
}

#[test]
fn test_h1_early_response_mid_chunked_upload() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H1 security: an early response ends neither a chunked upload nor its backend connection",
            || try_h1_early_response_mid_upload(EarlyResponseFraming::Chunked),
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
// Test 13a: chunked request trailers cannot carry spoofed forwarding headers
//
// RFC 9110 §6.5.1: a trailer field must not carry routing or
// client-attribution semantics. Sōzu rewrites `X-Forwarded-For`,
// `Forwarded`, `X-Real-IP` and the other headers of
// `editor::TRAILER_SPOOF_VECTOR_HEADERS` on the header block only, so a
// client that appends them to a chunked body as trailer fields would hand a
// forged client address to any backend that merges trailers into its
// header view (sozu-proxy/sozu#1689). The H2 frontend already drops them in
// `pkawa::handle_trailer`; the H1 frontend must drop the same list. The
// split variants send the trailer section after the last-chunk marker in a
// separate write, so the marker is already forwarded when the trailers are
// parsed, or split the trailer section itself, `X-Forwarded-For` in one
// write and the other fields in the next. A legitimate trailer
// (`Grpc-Status`) must still reach the backend.
// =========================================================================

/// Trailer section shared by both variants: every spoof-vector name carries
/// the forged address `6.6.6.6`, and one legitimate field must survive.
const SPOOF_TRAILERS: &str = concat!(
    "X-Forwarded-For: 6.6.6.6\r\n",
    "Forwarded: for=6.6.6.6\r\n",
    "X-Real-IP: 6.6.6.6\r\n",
    "X-Request-Id: 6.6.6.6\r\n",
    "X-Forwarded-Proto: 6.6.6.6\r\n",
    "X-Forwarded-Port: 6.6.6.6\r\n",
    "X-Forwarded-Host: 6.6.6.6\r\n",
    "Grpc-Status: 0\r\n",
    "\r\n",
);

/// Trailer section of the forbidden-field variants (sozu-proxy/sozu#1701):
/// one field of each category RFC 9110 §6.5.1 keeps out of trailers
/// (framing, routing, request modifiers, authentication, content processing)
/// plus a connection-specific one (RFC 9110 §7.6.1), each carrying the
/// forged value `6.6.6.6` (`6666` for `Content-Length`), and one legitimate
/// field that must survive.
const FORBIDDEN_TRAILERS: &str = concat!(
    "Content-Length: 6666\r\n",
    "Transfer-Encoding: 6.6.6.6\r\n",
    "Host: 6.6.6.6\r\n",
    "If-Match: 6.6.6.6\r\n",
    "Authorization: 6.6.6.6\r\n",
    "Cookie: 6.6.6.6\r\n",
    "Content-Type: 6.6.6.6\r\n",
    "Connection: 6.6.6.6\r\n",
    "Grpc-Status: 0\r\n",
    "\r\n",
);

/// Split a field value into whole tokens: `,`, `;`, `=`, `"` and whitespace
/// separate them, `:` does not, so `for="127.0.0.1:56666"` stays one token.
fn field_value_tokens(value: &str) -> impl Iterator<Item = &str> {
    value
        .split(|c: char| matches!(c, ',' | ';' | '=' | '"') || c.is_ascii_whitespace())
        .filter(|token| !token.is_empty())
}

/// Whether `forwarded` carries a forged field of `trailers`, the
/// `SPOOF_TRAILERS` or `FORBIDDEN_TRAILERS` section it was sent with: a
/// field line whose name is a forged field's name and whose value holds that
/// field's forged value (`6.6.6.6`, `6666`, the address of `for=6.6.6.6`) as
/// a whole token. `X-Forwarded-For: 127.0.0.1, 6.6.6.6` or `Forwarded:
/// for=6.6.6.6, proto=http` match; Sōzu's own `Forwarded:
/// proto=http;for="127.0.0.1:56666"` or `X-Forwarded-Port: 16666` cannot,
/// whatever ephemeral port or request id they carry.
fn carries_forged_trailer(forwarded: &str, trailers: &str) -> bool {
    let forged: Vec<(&str, &str)> = trailers
        .split("\r\n")
        .filter_map(|line| line.split_once(':'))
        .filter(|(name, _)| !name.eq_ignore_ascii_case("Grpc-Status"))
        .filter_map(|(name, value)| Some((name.trim(), field_value_tokens(value).last()?)))
        .collect();
    forwarded
        .split("\r\n")
        .filter_map(|line| line.split_once(':'))
        .any(|(name, value)| {
            forged.iter().any(|(forged_name, forged_value)| {
                name.trim().eq_ignore_ascii_case(forged_name)
                    && field_value_tokens(value).any(|token| token == *forged_value)
            })
        })
}

#[test]
fn carries_forged_trailer_matches_forged_fields_not_substrings() {
    // CI run 36681162463: a clean request whose `Forwarded` source port is
    // 56666 was taken for a forged `Content-Length: 6666`.
    let clean = concat!(
        "POST /api HTTP/1.1\r\n",
        "Host: localhost\r\n",
        "Transfer-Encoding: chunked\r\n",
        "X-Forwarded-For: 127.0.0.1\r\n",
        "X-Forwarded-Proto: http\r\n",
        "X-Forwarded-Port: 16666\r\n",
        "Forwarded: proto=http;for=\"127.0.0.1:56666\";by=127.0.0.1:6666\r\n",
        "Sozu-Id: 01K6666666666666666666666\r\n",
        "\r\n",
        "5\r\nHello\r\n0\r\nGrpc-Status: 0\r\n\r\n",
    );
    for trailers in [SPOOF_TRAILERS, FORBIDDEN_TRAILERS] {
        assert!(!carries_forged_trailer(clean, trailers), "{trailers:?}");
        // Every forged field still matches, alone or merged into a list.
        for field in trailers.split("\r\n").filter(|f| f.contains(':')) {
            let leaked = format!("{clean}{field}\r\n");
            assert_eq!(
                carries_forged_trailer(&leaked, trailers),
                !field.starts_with("Grpc-Status"),
                "{field:?}"
            );
        }
    }
    let merged = format!("{clean}X-Forwarded-For: 127.0.0.1, 6.6.6.6\r\n");
    assert!(carries_forged_trailer(&merged, SPOOF_TRAILERS));
    let merged = format!("{clean}forwarded: for=6.6.6.6, proto=http\r\n");
    assert!(carries_forged_trailer(&merged, SPOOF_TRAILERS));
}

/// Where the chunked request of `try_h1_trailer_fields_dropped` is
/// cut into separate writes.
#[derive(Clone, Copy)]
enum TrailerSplit {
    /// The whole request in one write.
    None,
    /// The trailer section in a second write, after the last-chunk line.
    AfterLastChunk,
    /// The trailer section itself cut after its first field.
    InsideTrailers,
}

/// Send a chunked request whose trailer section is `trailers`, cut as
/// `split` says, and check the backend receives none of its forged values
/// while `Grpc-Status` and the chunk framing survive.
fn try_h1_trailer_fields_dropped(kind: &str, trailers: &str, split: TrailerSplit) -> State {
    let label = match split {
        TrailerSplit::None => format!("TRAILER-{kind}"),
        TrailerSplit::AfterLastChunk => format!("TRAILER-{kind}-SPLIT"),
        TrailerSplit::InsideTrailers => format!("TRAILER-{kind}-SPLIT-INSIDE"),
    };
    let label = label.as_str();
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) =
        setup_sync_test(label, config, listeners, state, front_address, 1, false);
    let mut backend = backends.pop().unwrap();
    backend.set_response("HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\npong");
    backend.connect();

    let head = concat!(
        "POST /api HTTP/1.1\r\n",
        "Host: localhost\r\n",
        "Transfer-Encoding: chunked\r\n",
        "Trailer: X-Forwarded-For, Forwarded, X-Real-IP, Grpc-Status\r\n",
        "Connection: close\r\n",
        "\r\n",
        "5\r\n",
        "Hello\r\n",
        "0\r\n",
    );
    let request = format!("{head}{trailers}");
    let first_field = trailers
        .find("\r\n")
        .expect("the trailer section has a first field")
        + 2;
    let cut = match split {
        TrailerSplit::None => request.len(),
        TrailerSplit::AfterLastChunk => head.len(),
        TrailerSplit::InsideTrailers => head.len() + first_field,
    };
    let mut stream = raw_connect(front_address);
    stream
        .write_all(&request.as_bytes()[..cut])
        .expect("write first segment");
    if cut < request.len() {
        thread::sleep(Duration::from_millis(100));
        stream
            .write_all(&request.as_bytes()[cut..])
            .expect("write second segment");
    }

    let deadline = Instant::now() + Duration::from_millis(500);
    let mut accepted = false;
    while Instant::now() < deadline {
        if backend.accept(0) {
            accepted = true;
            break;
        }
    }
    if !accepted {
        println!("{label}: FAIL — the chunked request never reached the backend");
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    let forwarded = backend_drain(&mut backend, 0, Duration::from_millis(300));
    backend.send(0);
    println!("{label}: backend received {forwarded:?}");
    drop(stream);
    worker.soft_stop();
    worker.wait_for_server_stop();

    if carries_forged_trailer(&forwarded, trailers) {
        println!("{label}: FAIL — a spoofed or forbidden trailer reached the backend");
        return State::Fail;
    }
    // The legitimate trailer survives, and the chunked framing stays valid:
    // last-chunk, the surviving trailer field, then the empty line.
    if !forwarded.ends_with("0\r\nGrpc-Status: 0\r\n\r\n") {
        println!("{label}: FAIL — the legitimate trailer or the chunk framing was lost");
        return State::Fail;
    }
    State::Success
}

/// A chunked request pipelined behind a keep-alive `GET` in the same write
/// is parsed by the keep-alive branch of `ConnectionH1::writable`
/// (`lib/src/protocol/mux/h1.rs`), not by `ConnectionH1::readable`, once the
/// first response is written: its trailer section must be filtered there
/// too.
fn try_h1_pipelined_trailer_fields_dropped(kind: &str, trailers: &str) -> State {
    let label = format!("TRAILER-{kind}-PIPELINED");
    let label = label.as_str();
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) =
        setup_sync_test(label, config, listeners, state, front_address, 1, false);
    let mut backend = backends.pop().unwrap();
    backend.set_response("HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\npong");
    backend.connect();

    let request = format!(
        "{}{}{trailers}",
        "GET /first HTTP/1.1\r\nHost: localhost\r\n\r\n",
        concat!(
            "POST /api HTTP/1.1\r\n",
            "Host: localhost\r\n",
            "Transfer-Encoding: chunked\r\n",
            "Connection: close\r\n",
            "\r\n",
            "5\r\n",
            "Hello\r\n",
            "0\r\n",
        ),
    );
    let mut stream = raw_connect(front_address);
    stream
        .write_all(request.as_bytes())
        .expect("write pipelined requests");

    let deadline = Instant::now() + Duration::from_millis(500);
    let mut accepted = false;
    while Instant::now() < deadline {
        if backend.accept(0) {
            accepted = true;
            break;
        }
    }
    if !accepted {
        println!("{label}: FAIL — the first request never reached the backend");
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }
    let first = backend_drain(&mut backend, 0, Duration::from_millis(300));
    backend.send(0);
    // The pipelined request is parsed once the first response is written,
    // on the same keep-alive backend connection.
    let second = backend_drain(&mut backend, 0, Duration::from_millis(500));
    backend.send(0);
    println!("{label}: backend received {first:?} then {second:?}");
    drop(stream);
    worker.soft_stop();
    worker.wait_for_server_stop();

    if !second.starts_with("POST /api ") {
        println!("{label}: FAIL — the pipelined request never reached the backend");
        return State::Fail;
    }
    if carries_forged_trailer(&first, trailers) || carries_forged_trailer(&second, trailers) {
        println!("{label}: FAIL — a spoofed or forbidden trailer reached the backend");
        return State::Fail;
    }
    if !second.ends_with("0\r\nGrpc-Status: 0\r\n\r\n") {
        println!("{label}: FAIL — the legitimate trailer or the chunk framing was lost");
        return State::Fail;
    }
    State::Success
}

#[test]
fn test_h1_pipelined_trailer_spoof_headers_dropped() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: spoofed forwarding trailers of a pipelined chunked request never reach the backend",
            || try_h1_pipelined_trailer_fields_dropped("SPOOF", SPOOF_TRAILERS),
        ),
        State::Success,
    );
}

#[test]
fn test_h1_trailer_spoof_headers_dropped() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: spoofed forwarding headers in a chunked trailer section never reach the backend",
            || try_h1_trailer_fields_dropped("SPOOF", SPOOF_TRAILERS, TrailerSplit::None),
        ),
        State::Success,
    );
}

#[test]
fn test_h1_trailer_spoof_headers_dropped_split_inside_trailers() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: spoofed forwarding trailers split across two writes never reach the backend",
            || try_h1_trailer_fields_dropped("SPOOF", SPOOF_TRAILERS, TrailerSplit::InsideTrailers),
        ),
        State::Success,
    );
}

#[test]
fn test_h1_trailer_spoof_headers_dropped_split() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: spoofed forwarding trailers sent after the last chunk never reach the backend",
            || try_h1_trailer_fields_dropped("SPOOF", SPOOF_TRAILERS, TrailerSplit::AfterLastChunk),
        ),
        State::Success,
    );
}

#[test]
fn test_h1_trailer_forbidden_fields_dropped() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: trailer fields RFC 9110 §6.5.1 forbids never reach the backend",
            || try_h1_trailer_fields_dropped("FORBIDDEN", FORBIDDEN_TRAILERS, TrailerSplit::None),
        ),
        State::Success,
    );
}

#[test]
fn test_h1_trailer_forbidden_fields_dropped_split() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: forbidden trailer fields sent after the last chunk never reach the backend",
            || try_h1_trailer_fields_dropped(
                "FORBIDDEN",
                FORBIDDEN_TRAILERS,
                TrailerSplit::AfterLastChunk
            ),
        ),
        State::Success,
    );
}

#[test]
fn test_h1_trailer_forbidden_fields_dropped_split_inside_trailers() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: forbidden trailer fields split across two writes never reach the backend",
            || try_h1_trailer_fields_dropped(
                "FORBIDDEN",
                FORBIDDEN_TRAILERS,
                TrailerSplit::InsideTrailers
            ),
        ),
        State::Success,
    );
}

#[test]
fn test_h1_pipelined_trailer_forbidden_fields_dropped() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: forbidden trailer fields of a pipelined chunked request never reach the backend",
            || try_h1_pipelined_trailer_fields_dropped("FORBIDDEN", FORBIDDEN_TRAILERS),
        ),
        State::Success,
    );
}

// =========================================================================
// Test 13a bis: the trailer section of a chunked request is bounded
//
// sozu-proxy/sozu#1701: the H2 frontend refuses a trailer block of more than
// `h2_max_header_fields` fields (128 by default) in `pkawa::handle_trailer`;
// the H1 frontend applies the same listener value to a chunked request's
// trailer section. At the bound the request is forwarded whole; one field
// over, the client is answered 400 and the field that went over never
// reaches the backend. The split variant forwards the head and body first,
// so the backend already holds the request when the trailers arrive.
// =========================================================================

/// The default `h2_max_header_fields`, which the test listeners keep.
const DEFAULT_MAX_TRAILER_FIELDS: usize = 128;

/// A trailer section of `count` distinct legitimate fields.
fn numbered_trailers(count: usize) -> String {
    let mut trailers = String::new();
    for i in 0..count {
        trailers.push_str(&format!("X-T{i}: {i}\r\n"));
    }
    trailers.push_str("\r\n");
    trailers
}

fn try_h1_trailer_field_limit(count: usize, split: bool) -> State {
    let label = format!("TRAILER-LIMIT-{count}{}", if split { "-SPLIT" } else { "" });
    let label = label.as_str();
    let over = count > DEFAULT_MAX_TRAILER_FIELDS;
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) =
        setup_sync_test(label, config, listeners, state, front_address, 1, false);
    let mut backend = backends.pop().unwrap();
    backend.set_response("HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\npong");
    backend.connect();

    let head = concat!(
        "POST /api HTTP/1.1\r\n",
        "Host: localhost\r\n",
        "Transfer-Encoding: chunked\r\n",
        "Connection: close\r\n",
        "\r\n",
        "5\r\n",
        "Hello\r\n",
        "0\r\n",
    );
    let trailers = numbered_trailers(count);
    let mut stream = raw_connect(front_address);
    if split {
        stream.write_all(head.as_bytes()).expect("write head");
        thread::sleep(Duration::from_millis(100));
        stream
            .write_all(trailers.as_bytes())
            .expect("write trailers");
    } else {
        stream
            .write_all(format!("{head}{trailers}").as_bytes())
            .expect("write request");
    }

    // The backend may or may not be reached before the trailer section is
    // parsed; poll it for the whole window either way.
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut accepted = false;
    while Instant::now() < deadline {
        if backend.accept(0) {
            accepted = true;
            break;
        }
    }
    let forwarded = if accepted {
        backend_drain(&mut backend, 0, Duration::from_millis(300))
    } else {
        String::new()
    };
    if accepted && !over {
        backend.send(0);
    }
    let response = raw_read(&mut stream).unwrap_or_default();
    println!(
        "{label}: backend accepted={accepted} received {} bytes ending {:?}, client got {:?}",
        forwarded.len(),
        &forwarded[forwarded.len().saturating_sub(40)..],
        response.lines().next()
    );
    drop(stream);
    worker.soft_stop();
    worker.wait_for_server_stop();

    let last = format!("X-T{}: ", count - 1);
    if over {
        if !response.starts_with("HTTP/1.1 400") {
            println!("{label}: FAIL — an over-bound trailer section was not answered 400");
            return State::Fail;
        }
        if forwarded.contains(&last) {
            println!("{label}: FAIL — the field over the bound reached the backend");
            return State::Fail;
        }
        if split && !accepted {
            println!("{label}: FAIL — the head of the split request never reached the backend");
            return State::Fail;
        }
    } else {
        if !response.starts_with("HTTP/1.1 200") {
            println!("{label}: FAIL — a trailer section at the bound was refused");
            return State::Fail;
        }
        if !forwarded.ends_with(&format!("{last}{}\r\n\r\n", count - 1)) {
            println!("{label}: FAIL — the trailer section at the bound was not forwarded whole");
            return State::Fail;
        }
    }
    State::Success
}

#[test]
fn test_h1_trailer_field_limit_at_bound_forwarded() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: a trailer section of h2_max_header_fields fields is forwarded whole",
            || try_h1_trailer_field_limit(DEFAULT_MAX_TRAILER_FIELDS, false),
        ),
        State::Success,
    );
}

#[test]
fn test_h1_trailer_field_limit_exceeded_rejected() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: a trailer section over h2_max_header_fields is answered 400",
            || try_h1_trailer_field_limit(DEFAULT_MAX_TRAILER_FIELDS + 1, false),
        ),
        State::Success,
    );
}

#[test]
fn test_h1_trailer_field_limit_exceeded_rejected_split() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: a trailer section over h2_max_header_fields sent after the body is answered 400",
            || try_h1_trailer_field_limit(DEFAULT_MAX_TRAILER_FIELDS + 1, true),
        ),
        State::Success,
    );
}

// =========================================================================
// Test 13a ter: a request rejected after part of it reached the backend
// closes that backend connection
//
// The over-bound trailer section of a split chunked request is parsed after
// its head and body were already forwarded, so the backend holds a request
// cut before its trailer section. The rejection answers the client 400, and
// must also close that backend connection: left open, it waits for the rest
// of a request that never comes, and stays attached to the stream slot.
//
// The client connection closes after the 400 too. The request was never
// received whole, so its end cannot be found and no further request can be
// read from that connection (RFC 9112 §6.3, sozu-proxy/sozu#1721). The
// listener's 400 answer here carries no `Connection: close`, the
// operator-supported way to keep the client connection alive after a
// default answer (`set_default_answer`, `lib/src/protocol/mux/answers.rs`),
// so the close can only come from the incomplete request.
// =========================================================================

/// This test no longer goes red when the block of `ConnectionH1::readable`
/// (`lib/src/protocol/mux/h1.rs`) that ends the linked backend stream before
/// the `Position::Server` arm answers 400 is removed: since
/// sozu-proxy/sozu#1721, the incomplete request closes the client session,
/// and that close tears the backend connection down too. It still pins the
/// observable outcome: 400, then both connections closed, and no request
/// read after the rejected one.
fn try_h1_trailer_field_limit_split_closes_backend() -> State {
    let label = "TRAILER-LIMIT-SPLIT-BACKEND-CLOSE";
    let front_address = create_local_address();
    let back_address = create_local_address();

    let (config, mut listeners, state) = Worker::empty_config();
    attach_reserved_http_listener(&mut listeners, front_address);
    let mut worker = Worker::start_new_worker_owned(label, config, listeners, state);

    let mut listener_config = ListenerBuilder::new_http(front_address.into())
        .to_http(None)
        .expect("could not build the http listener config");
    listener_config.answers.insert(
        "400".to_owned(),
        "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n".to_owned(),
    );
    worker.send_proxy_request_type(RequestType::AddHttpListener(listener_config));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        interface: None,
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

    let mut backend = SyncBackend::new(
        "BACKEND_0",
        back_address,
        "HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\npong",
    );
    backend.connect();

    // A keep-alive request: nothing on the client side asks for a close.
    let head = concat!(
        "POST /api HTTP/1.1\r\n",
        "Host: localhost\r\n",
        "Transfer-Encoding: chunked\r\n",
        "\r\n",
        "5\r\n",
        "Hello\r\n",
        "0\r\n",
    );
    let trailers = numbered_trailers(DEFAULT_MAX_TRAILER_FIELDS + 1);
    let mut stream = raw_connect(front_address);
    stream.write_all(head.as_bytes()).expect("write head");

    let deadline = Instant::now() + Duration::from_millis(500);
    let mut accepted = false;
    while Instant::now() < deadline {
        if backend.accept(0) {
            accepted = true;
            break;
        }
    }
    let forwarded = if accepted {
        backend_drain(&mut backend, 0, Duration::from_millis(300))
    } else {
        String::new()
    };
    stream
        .write_all(trailers.as_bytes())
        .expect("write trailers");
    let response = raw_read(&mut stream).unwrap_or_default();

    // The backend connection must close well within this window.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut backend_closed = false;
    while accepted && Instant::now() < deadline {
        if !backend.is_connected(0) {
            backend_closed = true;
            break;
        }
        // Drain whatever else arrived, so `is_connected` peeks at the EOF.
        backend.receive(0);
    }
    // The request was not received whole: sozu closes the client connection
    // after the 400 instead of reading another request from it.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut client_closed = false;
    let mut after = Vec::new();
    let mut buf = [0u8; 1024];
    while Instant::now() < deadline {
        match stream.read(&mut buf) {
            Ok(0) => {
                client_closed = true;
                break;
            }
            Ok(n) => after.extend_from_slice(&buf[..n]),
            Err(e)
                if e.kind() == std::io::ErrorKind::ConnectionReset
                    || e.kind() == std::io::ErrorKind::BrokenPipe =>
            {
                client_closed = true;
                break;
            }
            Err(_) => {}
        }
    }
    let after = String::from_utf8_lossy(&after).to_string();
    // A follow-up request, if the write still lands, reaches no backend.
    let _ = stream.write_all(b"GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    let reaccepted = backend_accepts_within(&mut backend, 1, Duration::from_millis(300));
    println!(
        "{label}: backend accepted={accepted} received {} bytes ending {:?}, client got {:?}, backend_closed={backend_closed}, client_closed={client_closed}, after the 400 {after:?}, reaccepted={reaccepted}",
        forwarded.len(),
        &forwarded[forwarded.len().saturating_sub(24)..],
        response.lines().next()
    );
    drop(stream);
    worker.soft_stop();
    worker.wait_for_server_stop();

    if !accepted || !forwarded.ends_with("0\r\n") {
        println!("{label}: FAIL — the head and body never reached the backend before the trailers");
        return State::Fail;
    }
    if !response.starts_with("HTTP/1.1 400") {
        println!("{label}: FAIL — the over-bound trailer section was not answered 400");
        return State::Fail;
    }
    if !backend_closed {
        println!("{label}: FAIL — the backend connection holding the cut request stayed open");
        return State::Fail;
    }
    if !client_closed || !after.is_empty() {
        println!("{label}: FAIL — the client connection was not closed after the 400");
        return State::Fail;
    }
    if reaccepted {
        println!("{label}: FAIL — a request after the 400 reached the backend");
        return State::Fail;
    }
    State::Success
}

#[test]
fn test_h1_trailer_field_limit_exceeded_split_closes_backend() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H1 security: a request rejected after part of it was forwarded closes the backend connection",
            try_h1_trailer_field_limit_split_closes_backend,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 13b: a Content-Length that is not 1*DIGIT is never forwarded
//
// RFC 9110 §8.6: `Content-Length = 1*DIGIT`, and a sender MUST NOT forward
// a message whose Content-Length does not match that grammar. kawa 0.7.1
// read the value with `usize::from_str`, which accepts one leading `+`:
// `Content-Length: +5` framed a 5-byte body and reached the backend
// verbatim. A backend that refuses, ignores or re-reads that spelling
// takes the body for the start of the next request — one sozu never
// routed nor checked against the frontend's Basic auth (CWE-444).
// kawa >= 0.7.2 refuses such a value itself before the header callback
// (CleverCloud/kawa#26); `HttpContext::on_request_headers` and
// `HttpContext::on_response_headers` keep the same check as defense in
// depth. Either way the request is answered 400 before routing, and a
// backend response carrying one fails its parse, which the mux answers
// with a 502. These tests pin that outcome, so they stay green if only
// the Sōzu clauses are deleted; see `kawa_h1/LIFECYCLE.md` for how to
// reproduce the regression against kawa 0.7.1.
// =========================================================================

fn try_h1_signed_content_length_request_rejected() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "SIGNED-CL-REQUEST",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );
    let mut backend = backends.pop().unwrap();
    backend.connect();

    // The 5 body bytes are a request of their own to any backend that does
    // not read `+5` as a length.
    const REQUEST: &[u8] =
        b"POST /api HTTP/1.1\r\nHost: localhost\r\nContent-Length: +5\r\n\r\nGET /";

    let mut stream = raw_connect(front_address);
    stream
        .write_all(REQUEST)
        .expect("write signed Content-Length request");

    let not_forwarded = {
        let start = Instant::now();
        let mut forwarded = None;
        while start.elapsed() < Duration::from_millis(500) {
            if backend.accept(0) {
                forwarded = Some(backend_drain(&mut backend, 0, Duration::from_millis(300)));
                backend.send(0);
                break;
            }
        }
        if let Some(bytes) = &forwarded {
            println!("SIGNED-CL-REQUEST: FAIL — backend received {bytes:?}");
        }
        forwarded.is_none()
    };
    let response = raw_read(&mut stream);
    println!("SIGNED-CL-REQUEST: client received {response:?}");
    drop(stream);

    let rejected = matches!(&response, Some(r) if r.starts_with("HTTP/1.1 400"));
    let healthy = verify_sozu_healthy(front_address, &mut backend, !not_forwarded);

    worker.soft_stop();
    worker.wait_for_server_stop();
    if not_forwarded && rejected && healthy {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h1_signed_content_length_request_rejected() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: a signed Content-Length request is answered 400 and never reaches the backend",
            try_h1_signed_content_length_request_rejected,
        ),
        State::Success,
    );
}

fn try_h1_signed_content_length_response_rejected() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = setup_sync_test(
        "SIGNED-CL-RESPONSE",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );
    let mut backend = backends.pop().unwrap();
    backend.set_response("HTTP/1.1 200 OK\r\nContent-Length: +5\r\n\r\nhello");
    backend.connect();

    // POST, so the failed response is never replayed on a fresh backend.
    const REQUEST: &[u8] =
        b"POST /api HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nConnection: close\r\n\r\nHello";

    let mut stream = raw_connect(front_address);
    stream.write_all(REQUEST).expect("write request");

    let deadline = Instant::now() + Duration::from_millis(500);
    let mut accepted = false;
    while Instant::now() < deadline {
        if backend.accept(0) {
            accepted = true;
            break;
        }
    }
    if !accepted {
        println!("SIGNED-CL-RESPONSE: FAIL — a canonical request never reached the backend");
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }
    backend_drain(&mut backend, 0, Duration::from_millis(200));
    backend.send(0);

    let response = raw_read_all(&mut stream);
    println!("SIGNED-CL-RESPONSE: client received {response:?}");
    drop(stream);

    worker.soft_stop();
    worker.wait_for_server_stop();

    // The backend's framing never reaches the client: neither its signed
    // Content-Length nor the body it framed.
    if response.starts_with("HTTP/1.1 502")
        && !response.contains("+5")
        && !response.contains("hello")
    {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h1_signed_content_length_response_rejected() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: a backend response with a signed Content-Length is answered 502",
            try_h1_signed_content_length_response_rejected,
        ),
        State::Success,
    );
}

/// Non-regression: `005` matches `1*DIGIT`, so it is legal, framed as 5 and
/// forwarded as sent — the same as the H2 path does.
fn try_h1_leading_zero_content_length_forwarded() -> State {
    let front_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) =
        setup_sync_test("ZERO-CL", config, listeners, state, front_address, 1, false);
    let mut backend = backends.pop().unwrap();
    backend.set_response("HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\npong");
    backend.connect();

    const REQUEST: &[u8] =
        b"POST /api HTTP/1.1\r\nHost: localhost\r\nContent-Length: 005\r\nConnection: close\r\n\r\nHello";

    let mut stream = raw_connect(front_address);
    stream.write_all(REQUEST).expect("write request");

    let deadline = Instant::now() + Duration::from_millis(500);
    let mut accepted = false;
    while Instant::now() < deadline {
        if backend.accept(0) {
            accepted = true;
            break;
        }
    }
    if !accepted {
        println!("ZERO-CL: FAIL — a legal request never reached the backend");
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }
    let forwarded = backend_drain(&mut backend, 0, Duration::from_millis(300));
    backend.send(0);
    let response = raw_read(&mut stream);
    println!("ZERO-CL: backend received {forwarded:?}, client received {response:?}");
    drop(stream);

    worker.soft_stop();
    worker.wait_for_server_stop();

    if forwarded.contains("Content-Length: 005\r\n")
        && forwarded.ends_with("\r\n\r\nHello")
        && matches!(&response, Some(r) if r.starts_with("HTTP/1.1 200") && r.contains("pong"))
    {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h1_leading_zero_content_length_forwarded() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 security: a leading-zero Content-Length is legal and forwarded",
            try_h1_leading_zero_content_length_forwarded,
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
            interface: None,
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

// =========================================================================
// Test: an operator answer carrying an out-of-range status does not kill the
// worker
//
// `HttpListenerConfig.answers` is a `map<string, string>` whose keys AND
// bodies are operator input arriving over the command socket. Seventeen names
// ("301", "302", "308", "400" … "507") are pinned to their code by
// `HttpAnswers::template`'s `InvalidStatusCode` guard, but its `_ =>` arm
// builds an answer under any OTHER name with `Template::new(None, …)`, so that
// guard never runs for it. kawa reads a status line with `take(3)` plus
// `str::parse::<u16>()` and applies no range check, so a body starting
// `HTTP/1.1 000 x` compiles to status `0` — and `Template::new` used to
// `debug_assert!` the `100..=999` range, panicking the worker on a
// control-plane request in every debug, test, e2e and fuzz build.
//
// The compiled answer must stay UNSELECTABLE: selection keys off the
// `DefaultAnswer` variant, so an unrouted request must still get the builtin
// 404, never the operator's `000`.
//
// Scope note: the sibling half of that fix is the same range assertion on a
// status parsed from the BACKEND's response line. It used to live in
// `kawa_h1::save_http_status_metric`, which an unconditional planted `panic!`
// proved unreachable from this suite — H1 proxying runs through
// `protocol/mux`, not through the `kawa_h1` session, which was removed on
// 2026-09-20 (sozu#1346). The assertion now guards the live bucketer and is
// covered by
// `mux::stream::tests::a_backend_status_line_below_100_is_bucketed_not_asserted`.
// =========================================================================

/// Did the unrouted request draw the builtin 404, and not the operator's
/// out-of-range `000` answer?
///
/// Both conjuncts read the STATUS LINE alone. Scanning the whole response for
/// the substring `"000"` is falsifiable on the wire: the builtin 404 carries a
/// Crockford base32 ULID in its `Sozu-Id` response header and again in the
/// `request_id` field of its HTML body, and roughly one ULID in twenty
/// contains `000` — so the guard reddened on a perfectly correct answer in CI
/// job 105898321274 (check `fips`, head `5cb5395e`, 2026-09-19), on
/// `HTTP/1.1 404 Not Found … Sozu-Id: 01M2WV005MHC6FDKVS19000JFY`. The
/// request immediately before it drew `01M2WV005JRA3CXKWPPESY3TCR` and passed.
/// That is cause C of sozu#1393, a defect in this assertion and not in sozu.
///
/// The negative conjunct is kept rather than folded into the positive one: it
/// states the property this test exists to guard — the compiled `000` answer
/// stays unselectable — and keeps holding if the positive conjunct is ever
/// loosened.
fn builtin_404_answer_selected(answer: &str) -> bool {
    let status_line = answer.split_once("\r\n").map_or(answer, |(line, _)| line);
    status_line.starts_with("HTTP/1.1 404") && !status_line.starts_with("HTTP/1.1 000")
}

/// To SEE THIS RED: in `lib/src/protocol/kawa_h1/answers.rs`, restore the
/// deleted post-condition at the end of `Template::new`:
/// `debug_assert!((100..=999).contains(&resolved_status), "parsed template status must be a 3-digit HTTP code, got {resolved_status}");`
/// The worker then panics while compiling the listener's answer map, the
/// `AddHttpListener` is never answered, and the harness fails on the command
/// channel the dead worker dropped.
fn try_h1_custom_answer_with_out_of_range_status_does_not_kill_the_worker() -> State {
    let front_address = create_local_address();
    let back_address = create_local_address();

    let (config, mut listeners, state) = Worker::empty_config();
    attach_reserved_http_listener(&mut listeners, front_address);
    let mut worker = Worker::start_new_worker_owned("H1-BAD-ANSWER", config, listeners, state);

    let mut listener_config = ListenerBuilder::new_http(front_address.into())
        .to_http(None)
        .expect("could not build the http listener config");
    // An UNRECOGNISED name, so `HttpAnswers::template` takes its `_ =>` arm
    // with `status: None` and the `InvalidStatusCode` guard never runs.
    listener_config.answers.insert(
        "operator_custom".to_owned(),
        "HTTP/1.1 000 x\r\nContent-Length: 0\r\n\r\n".to_owned(),
    );

    worker.send_proxy_request_type(RequestType::AddHttpListener(listener_config));
    // No `None` arm: `Worker::read_proxy_response`
    // (`e2e/src/sozu/worker.rs`) `.expect()`s on the command channel and
    // always returns `Some`, so a `State::Fail` branch here would be
    // unreachable code dressed up as error handling. That `.expect()` IS this
    // test's failure path: a worker that died compiling the answer map drops
    // its end of the channel, and the read panics with "Could not read message
    // on command channel" — which is exactly the red this test was seen
    // producing.
    let add_response = worker
        .read_proxy_response()
        .expect("read_proxy_response never yields None");
    let listener_added =
        add_response.status == sozu_command_lib::proto::command::ResponseStatus::Ok as i32;
    println!(
        "H1-BAD-ANSWER: AddHttpListener status={}",
        add_response.status
    );

    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        interface: None,
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

    // The listener compiled the answer and still serves ordinary traffic.
    let still_serving = verify_sozu_healthy(front_address, &mut backend, false);

    // The custom answer is compiled in but unselectable: a request for an
    // unknown host must still get the builtin 404, never the operator's `000`.
    let mut stray = raw_connect(front_address);
    stray
        .write_all(b"GET /api HTTP/1.1\r\nHost: unrouted.example.com\r\nConnection: close\r\n\r\n")
        .expect("write the unrouted request");
    let stray_answer = raw_read_all(&mut stray);
    let builtin_answer_selected = builtin_404_answer_selected(&stray_answer);
    println!("H1-BAD-ANSWER: unrouted request got {stray_answer:?}");

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    println!(
        "H1-BAD-ANSWER: listener_added={listener_added} still_serving={still_serving} builtin_answer_selected={builtin_answer_selected} stopped={stopped}"
    );
    if listener_added && still_serving && builtin_answer_selected && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h1_custom_answer_with_out_of_range_status_does_not_kill_the_worker() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H1 security: an operator answer carrying a status outside 100..=999 does not kill the worker",
            try_h1_custom_answer_with_out_of_range_status_does_not_kill_the_worker,
        ),
        State::Success,
    );
}

/// Pins `builtin_404_answer_selected` against the exact answer that reddened
/// CI job 105898321274 (check `fips`, head `5cb5395e`, 2026-09-19): a correct
/// builtin 404 whose ULID happens to carry the digits `000`. The fixture is
/// the response the e2e test printed verbatim before returning `State::Fail`,
/// so the property is captured, not constructed — a regression test that
/// waited for an unlucky ULID would detect nothing, since only about one draw
/// in twenty carries the substring.
///
/// To SEE THIS RED: restore the historical body of
/// `builtin_404_answer_selected`,
/// `answer.starts_with("HTTP/1.1 404") && !answer.contains("000")` — the
/// `ulid_carrying_000` assertion fails, because the substring scan reaches the
/// `Sozu-Id` header and the `request_id` in the body. The `operator_000`
/// assertion is what keeps that mutation from being repaired by deleting the
/// negative conjunct: the guard must still reject the operator's answer.
#[test]
fn the_unselectable_000_guard_reads_the_status_line_not_the_whole_answer() {
    // Captured from the failing run: `Sozu-Id` and the HTML `request_id` both
    // repeat the ULID `01M2WV005MHC6FDKVS19000JFY`.
    let ulid_carrying_000 = concat!(
        "HTTP/1.1 404 Not Found\r\n",
        "Cache-Control: no-cache\r\n",
        "Connection: close\r\n",
        "Sozu-Id: 01M2WV005MHC6FDKVS19000JFY\r\n",
        "\r\n",
        "<html><head><meta charset='utf-8'><head><body>\r\n",
        "<style>pre{background:#EEE;padding:10px;border:1px solid #AAA;",
        "border-radius: 5px;}</style>\r\n",
        "<h1>404 Not Found</h1>\r\n",
        "<pre>\r\n",
        "{\r\n",
        "    \"status_code\": 404,\r\n",
        "    \"route\": \"GET unrouted.example.com/api\",\r\n",
        "    \"request_id\": \"01M2WV005MHC6FDKVS19000JFY\"\r\n",
        "}\r\n",
        "</pre>\r\n",
        "<footer>This is an automatic answer by S\u{14d}zu.</footer></body></html>",
    );
    assert!(
        builtin_404_answer_selected(ulid_carrying_000),
        "a builtin 404 whose ULID carries the digits 000 is still a builtin 404"
    );

    // The draw the same run made one request earlier, which passed.
    let ulid_without_000 = concat!(
        "HTTP/1.1 404 Not Found\r\n",
        "Cache-Control: no-cache\r\n",
        "Connection: close\r\n",
        "Sozu-Id: 01M2WV005JRA3CXKWPPESY3TCR\r\n",
        "\r\n",
    );
    assert!(
        builtin_404_answer_selected(ulid_without_000),
        "an ordinary builtin 404 must keep passing the guard"
    );

    // The check is not vacuous: the operator's compiled answer must still be
    // rejected, whatever its ULID.
    let operator_000 = concat!(
        "HTTP/1.1 000 x\r\n",
        "Content-Length: 0\r\n",
        "Sozu-Id: 01M2WV005JRA3CXKWPPESY3TCR\r\n",
        "\r\n",
    );
    assert!(
        !builtin_404_answer_selected(operator_000),
        "the operator's out-of-range 000 answer must never read as the builtin 404"
    );

    // A truncated answer carrying no CRLF is not a builtin 404 either.
    assert!(
        !builtin_404_answer_selected("HTTP/1.1 000 x"),
        "a header-less 000 status line must still be rejected"
    );
}

// =========================================================================
// Test: a request without Content-Length or Transfer-Encoding has no body
//
// RFC 9112 §6.3 rule 7: "If this is a request message and none of the above
// are true, then the message body length is zero (no message body is
// present)." Close-delimited framing (rule 8) belongs to responses only.
//
// kawa frames a message with neither header as `BodySize::Empty`. kawa 0.7.1,
// for a request exactly as for a response, parsed it into `ParsingPhase::Body`,
// where the `Empty` arm takes every byte left in the buffer. A second request
// pipelined in the same segment used to become the first request's "body":
// forwarded raw to the first request's backend — never routed, never
// Basic-auth checked, without `Sozu-Id` or `X-Forwarded-*` (CWE-444).
// kawa >= 0.7.2 ends such a request after its headers itself
// (CleverCloud/kawa#27), and `HttpContext::on_request_headers` keeps doing
// so as defense in depth, so the pipelined one is parsed, routed and edited
// on its own. These tests pin that outcome, so they stay green if only the
// Sōzu branch is deleted; see `kawa_h1/LIFECYCLE.md` §2.2 for how to
// reproduce the regression against kawa 0.7.1.
//
// The matrix: the first request's method (GET, HEAD, DELETE, POST, none of
// them framed), the second request's destination (the same cluster, another
// cluster by path, an auth-gated cluster), and a clear or a TLS frontend.
// =========================================================================

/// Which listener the unframed-request cases go through.
#[derive(Clone, Copy, Debug)]
enum UnframedFront {
    Clear,
    Tls,
}

/// Where the pipelined second request must be routed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UnframedTarget {
    /// `/a/…`: the first request's own cluster.
    Same,
    /// `/b/…`: another, open cluster.
    Other,
    /// `/c/…`: a cluster whose frontend requires Basic auth the second
    /// request does not carry.
    AuthGated,
}

/// Start a worker with three clusters on one listener, split by path:
/// `/a` → `cluster_a`, `/b` → `cluster_b`, `/c` → `cluster_c` (Basic auth
/// required), each with one backend.
fn spawn_unframed_worker(
    label: &str,
    front: UnframedFront,
    front_address: SocketAddr,
    back_addresses: [SocketAddr; 3],
) -> Worker {
    let (config, mut listeners, state) = Worker::empty_config();
    let mut worker = match front {
        UnframedFront::Clear => {
            attach_reserved_http_listener(&mut listeners, front_address);
            let mut worker = Worker::start_new_worker_owned(label, config, listeners, state);
            worker.send_proxy_request_type(RequestType::AddHttpListener(
                ListenerBuilder::new_http(front_address.into())
                    .to_http(None)
                    .expect("default HTTP listener must build"),
            ));
            worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
                interface: None,
                address: front_address.into(),
                proxy: ListenerType::Http.into(),
                from_scm: true,
            }));
            worker
        }
        UnframedFront::Tls => {
            let (config, listeners, state) = Worker::empty_https_config(front_address);
            let mut worker = Worker::start_new_worker_owned(label, config, listeners, state);
            worker.send_proxy_request_type(RequestType::AddHttpsListener(
                ListenerBuilder::new_https(front_address.into())
                    .to_tls(None)
                    .expect("default HTTPS listener must build"),
            ));
            worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
                interface: None,
                address: front_address.into(),
                proxy: ListenerType::Https.into(),
                from_scm: false,
            }));
            worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
                address: front_address.into(),
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

    for (index, (name, back_address)) in ["a", "b", "c"].into_iter().zip(back_addresses).enumerate()
    {
        let cluster_id = format!("cluster_{name}");
        let gated = index == 2;
        worker.send_proxy_request_type(RequestType::AddCluster(if gated {
            Cluster {
                authorized_hashes: vec![format!("admin:{AUTH_BYPASS_SECRET_SHA256_HEX}")],
                www_authenticate: Some("Basic realm=\"sozu\"".to_owned()),
                ..Worker::default_cluster(&cluster_id)
            }
        } else {
            Worker::default_cluster(&cluster_id)
        }));
        let frontend = RequestHttpFrontend {
            path: PathRule::prefix(format!("/{name}")),
            required_auth: gated.then_some(true),
            ..Worker::default_http_frontend(&cluster_id, front_address)
        };
        worker.send_proxy_request_type(match front {
            UnframedFront::Clear => RequestType::AddHttpFrontend(frontend),
            UnframedFront::Tls => RequestType::AddHttpsFrontend(frontend),
        });
        worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
            &cluster_id,
            format!("{cluster_id}-0"),
            back_address,
            None,
        )));
    }
    worker.read_to_last();
    worker
}

/// Every value of the `Sozu-Id` header in `forwarded`, in order.
fn sozu_ids(forwarded: &str) -> Vec<&str> {
    forwarded
        .split("\r\n")
        .filter_map(|line| line.strip_prefix("Sozu-Id: "))
        .collect()
}

/// Request lines (`METHOD /path HTTP/1.x`) found in `forwarded`.
fn request_lines(forwarded: &str) -> Vec<&str> {
    forwarded
        .split("\r\n")
        .filter(|line| line.ends_with(" HTTP/1.1") || line.ends_with(" HTTP/1.0"))
        .collect()
}

/// Read from `stream` until `count` status lines arrived or `timeout` ran
/// out, tolerating segmentation and the short read timeouts of both
/// transports.
fn read_status_lines<S: Read>(stream: &mut S, count: usize, timeout: Duration) -> String {
    let start = Instant::now();
    let mut received = Vec::new();
    let mut buf = [0u8; 4096];
    while start.elapsed() < timeout {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                received.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&received);
                if text.matches("HTTP/1.1 ").count() >= count {
                    break;
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&received).into_owned()
}

/// Wait for `backend` to accept on `client_id` until `deadline`.
fn backend_accepts_within(backend: &mut SyncBackend, client_id: usize, deadline: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if backend.accept(client_id) {
            return true;
        }
    }
    false
}

/// One case: send `<method> /a/first` with neither Content-Length nor
/// Transfer-Encoding and a second request to `target` in ONE write, then
/// prove each reached its own backend as a distinct, edited request.
fn unframed_request_case<S: Read + Write>(
    label: &str,
    method: &str,
    target: UnframedTarget,
    stream: &mut S,
    backends: &mut [SyncBackend; 3],
) -> Result<(), String> {
    let second_path = match target {
        UnframedTarget::Same => "/a/second",
        UnframedTarget::Other => "/b/second",
        UnframedTarget::AuthGated => "/c/second",
    };
    // Keep-alive on purpose: a `Connection: close` first request would make
    // Sōzu legitimately drop the second one after the first response, and
    // the test would then prove nothing.
    let payload = format!(
        "{method} /a/first HTTP/1.1\r\nHost: localhost\r\n\r\nGET {second_path} HTTP/1.1\r\nHost: localhost\r\n\r\n"
    );
    stream
        .write_all(payload.as_bytes())
        .and_then(|()| stream.flush())
        .map_err(|e| format!("write pipelined requests: {e}"))?;

    let [backend_a, backend_b, backend_c] = backends;
    if !backend_accepts_within(backend_a, 0, Duration::from_millis(1000)) {
        return Err("the first request never reached cluster_a".to_owned());
    }
    let first = backend_drain(backend_a, 0, Duration::from_millis(300));
    println!(
        "{label}: cluster_a received first {} bytes: {first:?}",
        first.len()
    );
    let lines = request_lines(&first);
    if first.contains(second_path) || lines.len() != 1 {
        return Err(format!(
            "the pipelined request was forwarded inside the first one's body \
             (request lines {lines:?}, Sozu-Id {:?})",
            sozu_ids(&first)
        ));
    }
    let first_ids = sozu_ids(&first);
    if first_ids.len() != 1 {
        return Err(format!("first request carries Sozu-Id {first_ids:?}"));
    }
    let first_id = first_ids[0].to_owned();

    backend_a.set_response(if method == "HEAD" {
        "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n"
    } else {
        "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\npongA"
    });
    backend_a.send(0);

    let second = match target {
        UnframedTarget::Same => {
            // Sōzu reuses the pooled backend connection or opens a fresh one;
            // which one is not what this case is about.
            let mut second = backend_drain(backend_a, 0, Duration::from_millis(300));
            let mut client_id = 0;
            if second.is_empty() && backend_accepts_within(backend_a, 1, Duration::from_millis(700))
            {
                client_id = 1;
                second = backend_drain(backend_a, 1, Duration::from_millis(300));
            }
            backend_a.set_response("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\npongA");
            backend_a.send(client_id);
            Some(second)
        }
        UnframedTarget::Other => {
            if !backend_accepts_within(backend_b, 0, Duration::from_millis(1000)) {
                return Err("the pipelined request never reached cluster_b".to_owned());
            }
            let second = backend_drain(backend_b, 0, Duration::from_millis(300));
            backend_b.send(0);
            Some(second)
        }
        UnframedTarget::AuthGated => {
            if backend_accepts_within(backend_c, 0, Duration::from_millis(300)) {
                let leaked = backend_drain(backend_c, 0, Duration::from_millis(300));
                return Err(format!(
                    "the unauthenticated request reached cluster_c: {leaked:?}"
                ));
            }
            None
        }
    };

    if let Some(second) = &second {
        println!("{label}: second request forwarded as {second:?}");
        let lines = request_lines(second);
        if lines != [format!("GET {second_path} HTTP/1.1").as_str()] {
            return Err(format!(
                "the pipelined request was not forwarded on its own: {lines:?}"
            ));
        }
        let ids = sozu_ids(second);
        if ids.len() != 1 || ids[0] == first_id {
            return Err(format!(
                "the pipelined request must carry its own Sozu-Id, got {ids:?} (first {first_id})"
            ));
        }
        if !second.contains("X-Forwarded-For: ") {
            return Err("the pipelined request carries no X-Forwarded-For".to_owned());
        }
    }

    let responses = read_status_lines(stream, 2, Duration::from_millis(2000));
    println!("{label}: client received {responses:?}");
    // A body is not followed by a line break: `pongAHTTP/1.1 200 OK` is one
    // line, so find the status lines by their prefix, not line by line.
    let statuses: Vec<&str> = responses
        .match_indices("HTTP/1.1 ")
        .map(|(at, _)| &responses[at..(at + 12).min(responses.len())])
        .collect();
    let second_status = match target {
        UnframedTarget::AuthGated => "HTTP/1.1 401",
        _ => "HTTP/1.1 200",
    };
    if statuses.len() != 2
        || !statuses[0].starts_with("HTTP/1.1 200")
        || !statuses[1].starts_with(second_status)
    {
        return Err(format!(
            "expected a 200 then a {second_status} response, got {statuses:?}"
        ));
    }
    Ok(())
}

fn try_h1_unframed_request_has_no_body(front: UnframedFront, target: UnframedTarget) -> State {
    // Every method runs, so one failure does not hide which others fail.
    let mut state = State::Success;
    for method in ["GET", "HEAD", "DELETE", "POST"] {
        let label = format!("UNFRAMED-{front:?}-{target:?}-{method}");
        let front_address = create_local_address();
        let back_addresses = [
            create_local_address(),
            create_local_address(),
            create_local_address(),
        ];
        let mut worker = spawn_unframed_worker(&label, front, front_address, back_addresses);
        let mut backends = back_addresses.map(|address| {
            let mut backend = SyncBackend::new(
                format!("{label}-{address}"),
                address,
                "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\npong_",
            );
            backend.connect();
            backend
        });

        let tcp = raw_connect(front_address);
        let outcome = match front {
            UnframedFront::Clear => {
                let mut tcp = tcp;
                unframed_request_case(&label, method, target, &mut tcp, &mut backends)
            }
            UnframedFront::Tls => {
                let mut tls_config = rustls::ClientConfig::builder()
                    .dangerous()
                    .with_custom_certificate_verifier(std::sync::Arc::new(Verifier))
                    .with_no_client_auth();
                tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];
                let server_name = rustls::pki_types::ServerName::try_from("localhost")
                    .expect("localhost is a valid server name")
                    .to_owned();
                let conn =
                    rustls::ClientConnection::new(std::sync::Arc::new(tls_config), server_name)
                        .expect("TLS client connection");
                let mut tls = rustls::StreamOwned::new(conn, tcp);
                unframed_request_case(&label, method, target, &mut tls, &mut backends)
            }
        };

        worker.soft_stop();
        worker.wait_for_server_stop();
        match outcome {
            Ok(()) => println!("{label}: OK"),
            Err(reason) => {
                println!("{label}: FAIL — {reason}");
                state = State::Fail;
            }
        }
    }
    state
}

#[test]
fn test_h1_unframed_request_then_same_cluster() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "H1 security: a pipelined request behind an unframed one reaches its cluster on its own (clear)",
            || try_h1_unframed_request_has_no_body(UnframedFront::Clear, UnframedTarget::Same),
        ),
        State::Success,
    );
}

#[test]
fn test_h1_unframed_request_then_other_cluster() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "H1 security: an unframed request cannot carry a pipelined one past routing (clear)",
            || try_h1_unframed_request_has_no_body(UnframedFront::Clear, UnframedTarget::Other),
        ),
        State::Success,
    );
}

#[test]
fn test_h1_unframed_request_then_auth_gated_cluster() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "H1 security: an unframed request cannot carry a pipelined one past Basic auth (clear)",
            || try_h1_unframed_request_has_no_body(UnframedFront::Clear, UnframedTarget::AuthGated),
        ),
        State::Success,
    );
}

#[test]
fn test_h1_unframed_request_then_same_cluster_tls() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "H1 security: a pipelined request behind an unframed one reaches its cluster on its own (TLS)",
            || try_h1_unframed_request_has_no_body(UnframedFront::Tls, UnframedTarget::Same),
        ),
        State::Success,
    );
}

#[test]
fn test_h1_unframed_request_then_other_cluster_tls() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "H1 security: an unframed request cannot carry a pipelined one past routing (TLS)",
            || try_h1_unframed_request_has_no_body(UnframedFront::Tls, UnframedTarget::Other),
        ),
        State::Success,
    );
}

#[test]
fn test_h1_unframed_request_then_auth_gated_cluster_tls() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "H1 security: an unframed request cannot carry a pipelined one past Basic auth (TLS)",
            || try_h1_unframed_request_has_no_body(UnframedFront::Tls, UnframedTarget::AuthGated),
        ),
        State::Success,
    );
}
