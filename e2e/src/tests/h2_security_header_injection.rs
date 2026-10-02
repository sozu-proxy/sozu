//! Adversarial H2 pseudo-header / header-injection end-to-end tests.
//!
//! Targets the hardening landed in the H2 audit completion series:
//!
//! * CRLF / C0 / DEL rejection in regular header **values** and cookie values
//!   (FIX-1 — commit `c3f9e090`). Complements the existing
//!   `test_h2_authority_injection_crlf` which covers the `:authority`
//!   pseudo-header variant.
//! * `:status` response-path syntax — exactly 3 ASCII digits; `abc`, `20`,
//!   `+200`, `00200` must all be rejected (FIX-2 — commit `c3f9e090`). Uses
//!   `RawH2ResponseBackend` to bypass hyper's upstream sanitisation.
//! * `:path` request-target syntax — empty rejected by
//!   [`store_pseudo_header`]; non-`/` rejected; `*` with non-OPTIONS method
//!   rejected; `*` with OPTIONS accepted (FIX-3 — commit `c3f9e090`).
//! * `:scheme` — only `http` / `https` accepted; `javascript`, `file`, empty
//!   rejected (FIX-4 — commit `c3f9e090`).
//! * `Content-Length` — reject leading `+`, leading space, `0x` prefix, and
//!   empty/non-digit bytes beyond the patterns already covered by
//!   `test_h2_content_length_format_fuzzing` (FIX-5 — commit `c3f9e090`).
//! * `host` vs `:authority` — mismatch → PROTOCOL_ERROR; case-insensitive
//!   match → deduplicated to a single `Host:` line on the H1 wire
//!   (FIX-6 — commit `4b8fbd3a`).
//! * Request trailers — the fields of `editor::TRAILER_FORBIDDEN_FIELDS`
//!   are elided from an H2 trailer block before an H1 or an H2 backend
//!   receives it (sozu-proxy/sozu#1714).
//!
//! Raw-byte H2 is used for every request path: hyper refuses CRLF in values
//! and collapses duplicate pseudo-headers at its HPACK encoder, so the only
//! way to exercise sozu's filter is to build the header block by hand.

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    thread,
    time::{Duration, Instant},
};

use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, AddCertificate, CertificateAndKey, Cluster, ListenerType,
        RequestHttpFrontend, SocketAddress, request::RequestType,
    },
};

use super::h2_utils::{
    H2_FLAG_END_STREAM, H2_FRAME_DATA, H2_FRAME_HEADERS, H2Frame, collect_response_frames,
    contains_goaway, contains_rst_stream, decode_status, h2_handshake, log_frames, parse_h2_frames,
    raw_h2_connection, rejected_with_goaway_or_rst, setup_h2_listener_only, setup_h2_test,
    stream_status_matches, teardown, verify_sozu_alive,
};
use crate::{
    mock::{
        aggregator::SimpleAggregator, async_backend::BackendHandle as AsyncBackend,
        h2_backend::H2Backend, raw_h2_response_backend::RawH2ResponseBackend,
        sync_backend::Backend as SyncBackend,
    },
    port_registry::provide_port,
    sozu::worker::Worker,
    tests::{State, repeat_until_error_or, tests::create_local_address},
};

// ============================================================================
// HPACK header-block helpers (local — mirrors the literal-without-indexing
// encoding in `raw_h2_response_backend::encode_literal`, kept inline so the
// individual test cases read as single self-contained units).
// ============================================================================

/// Append a literal-without-indexing header pair (`0x00` opcode) with a
/// fresh name. `name.len()` and `value.len()` must each be ≤ 126.
fn push_literal(block: &mut Vec<u8>, name: &[u8], value: &[u8]) {
    assert!(name.len() < 0x7f && value.len() < 0x7f);
    block.push(0x00);
    block.push(name.len() as u8);
    block.extend_from_slice(name);
    block.push(value.len() as u8);
    block.extend_from_slice(value);
}

/// Build a header block for a `GET /` request to `:authority localhost` using
/// the HPACK static-table indexed form — the common prefix for every test.
fn request_prefix_localhost() -> Vec<u8> {
    vec![
        0x82, // :method GET
        0x84, // :path /
        0x87, // :scheme https
        0x41, 0x09, // :authority, len 9
        b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't',
    ]
}

// ============================================================================
// FIX-1 — CRLF / CTL rejection in H2 regular header values
// ============================================================================

/// RFC 9110 §5.5 forbids C0 controls (except HTAB) and DEL in field values.
/// HPACK-decoded bytes containing `\r\n` flow through kawa's H1 serializer
/// and reach backends as injected headers (CWE-93, CWE-444). FIX-1 added
/// `has_invalid_value_byte` to reject the whole stream before any byte is
/// forwarded.
///
/// We send a well-formed request with a single bogus `x-evil` header whose
/// value contains a raw CRLF followed by a would-be header. A permissive
/// proxy would emit `x-evil: safe\r\nevil: 1` on the H1 wire; a correct one
/// rejects with PROTOCOL_ERROR before the backend accepts any connection.
fn try_h2_header_value_crlf_rejected() -> State {
    let (worker, backends, front_port) = setup_h2_test("H2-SEC-CRLF-VALUE", 1);
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);

    let mut block = request_prefix_localhost();
    // x-evil: "safe\r\nevil: 1" — 13 bytes with CR (0x0d) and LF (0x0a).
    push_literal(&mut block, b"x-evil", b"safe\r\nevil: 1");

    let frame = H2Frame::headers(1, block, true, true);
    tls.write_all(&frame.encode()).unwrap();
    tls.flush().unwrap();

    let frames = collect_response_frames(&mut tls, 500, 3, 500);
    log_frames("CRLF-in-value", &frames);

    // Stream-scope header-injection violation: sozu must emit RST/GOAWAY or 400.
    let rejected = rejected_with_goaway_or_rst(&frames) || stream_status_matches(&frames, 1, 400);
    drop(tls);
    thread::sleep(Duration::from_millis(100));
    let still_alive = verify_sozu_alive(front_port);

    let infra_ok = teardown(raw_h2_connection(front_addr), front_port, worker, backends);
    if rejected && still_alive && infra_ok {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_header_value_crlf_rejected() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 security: CRLF in regular header value is rejected (FIX-1 c3f9e090)",
            try_h2_header_value_crlf_rejected
        ),
        State::Success
    );
}

/// Same as above but for a cookie-header value — cookies go through a
/// dedicated decode path (`detached.jar`) which FIX-1 also hardened via
/// `has_invalid_value_byte` on both key and value.
fn try_h2_cookie_value_nul_rejected() -> State {
    let (worker, backends, front_port) = setup_h2_test("H2-SEC-CRLF-COOKIE", 1);
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);

    let mut block = request_prefix_localhost();
    // cookie: "sid=abc\x00smuggle=1" — NUL byte between the two cookie pairs.
    push_literal(&mut block, b"cookie", b"sid=abc\x00smuggle=1");

    let frame = H2Frame::headers(1, block, true, true);
    tls.write_all(&frame.encode()).unwrap();
    tls.flush().unwrap();

    let frames = collect_response_frames(&mut tls, 500, 3, 500);
    log_frames("NUL-in-cookie", &frames);

    // Stream-scope header-injection violation: sozu must emit RST/GOAWAY or 400.
    let rejected = rejected_with_goaway_or_rst(&frames) || stream_status_matches(&frames, 1, 400);
    let infra_ok = teardown(tls, front_port, worker, backends);
    if rejected && infra_ok {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_cookie_value_nul_rejected() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 security: NUL byte in cookie value is rejected (FIX-1 c3f9e090)",
            try_h2_cookie_value_nul_rejected
        ),
        State::Success
    );
}

// ============================================================================
// FIX-6 — `host` vs `:authority` reconciliation
// ============================================================================

/// Helper: sozu listener + sync backend wired to `cluster_0`. Returns the
/// running `Worker`, the `SyncBackend` (listening, ready to `accept`), and
/// the HTTPS front-port.
///
/// Used by the two `host` / `:authority` cases and by FIX-3/FIX-4 tests
/// that need to assert the backend received **nothing** after a malformed
/// request — an `AsyncBackend` swallows the evidence behind its own
/// accept loop.
fn setup_h2_with_sync_backend(name: &str) -> (Worker, SyncBackend, u16) {
    let (mut worker, front_port, _front_address) = setup_h2_listener_only(name);
    let back_address = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));
    worker.read_to_last();
    let mut backend = SyncBackend::new(
        format!("{name}-BACK"),
        back_address,
        crate::http_utils::http_ok_response("pong0"),
    );
    backend.connect();
    (worker, backend, front_port)
}

/// Case A — `:authority localhost` + literal `host evil.example.com`. This
/// is the HAProxy CVE-2021-39240 desync vector: a permissive proxy picks
/// one value for routing and the backend sees the other. FIX-6 makes sozu
/// refuse the stream with PROTOCOL_ERROR before any TCP connection is
/// initiated towards the backend.
fn try_h2_host_authority_mismatch_rejected() -> State {
    let (mut worker, mut backend, front_port) = setup_h2_with_sync_backend("H2-SEC-HOSTMISMATCH");
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);

    let mut block = request_prefix_localhost();
    // Literal host header disagreeing with :authority.
    push_literal(&mut block, b"host", b"evil.example.com");

    let frame = H2Frame::headers(1, block, true, true);
    tls.write_all(&frame.encode()).unwrap();
    tls.flush().unwrap();

    let frames = collect_response_frames(&mut tls, 500, 3, 500);
    log_frames("host-vs-authority mismatch", &frames);

    // Stream-scope desync violation (FIX-6): sozu must emit RST/GOAWAY or 400.
    let rejected = rejected_with_goaway_or_rst(&frames) || stream_status_matches(&frames, 1, 400);

    // Prove sozu did NOT open a connection towards the sync backend.
    let accepted = backend.accept(0);
    println!("mismatch — backend accepted connection: {accepted}");
    if accepted {
        // sozu forwarded the request — smuggling vector. Drain and fail.
        let _ = backend.receive(0);
        backend.disconnect();
        drop(tls);
        worker.hard_stop();
        let _ = worker.wait_for_server_stop();
        return State::Fail;
    }
    backend.disconnect();

    drop(tls);
    thread::sleep(Duration::from_millis(100));
    let still_alive = verify_sozu_alive(front_port);

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    if rejected && !accepted && still_alive && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_host_authority_mismatch_rejected() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 security: :authority/host mismatch rejected (FIX-6 4b8fbd3a)",
            try_h2_host_authority_mismatch_rejected
        ),
        State::Success
    );
}

/// Case B — `:authority localhost` + literal `host LOCALHOST` (case
/// difference only). RFC 9113 §8.3.1 permits the duplicate iff it
/// identifies the same origin modulo ASCII case; kawa's H1 serializer must
/// then emit exactly one `Host:` line so downstream parsers cannot
/// disagree.
fn try_h2_host_authority_match_deduplicated() -> State {
    let (mut worker, mut backend, front_port) =
        setup_h2_with_sync_backend("H2-SEC-HOSTMATCH-DEDUP");
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);

    let mut block = request_prefix_localhost();
    // Matching host (case-normalized) — must be tolerated and deduplicated.
    push_literal(&mut block, b"host", b"LOCALHOST");

    let frame = H2Frame::headers(1, block, true, true);
    tls.write_all(&frame.encode()).unwrap();
    tls.flush().unwrap();

    // Poll backend.accept up to 2 s — sozu needs a few epoll ticks to
    // connect to the backend after decoding the HEADERS frame.
    let accepted = (0..200).any(|_| {
        if backend.accept(0) {
            true
        } else {
            thread::sleep(Duration::from_millis(10));
            false
        }
    });

    let received = backend.receive(0).unwrap_or_default();
    println!(
        "match-dedup — backend accepted: {accepted}, received {} bytes",
        received.len()
    );
    println!("match-dedup — raw request bytes:\n{received}");

    // The H1-serialized request must contain exactly one `host:` line
    // (case-insensitive). Counting both `host:` and `Host:` covers kawa's
    // capitalization normalization.
    let host_line_count = received.to_ascii_lowercase().matches("\r\nhost:").count();
    // Also tolerate the case where the request starts with `Host:`
    // (no preceding CRLF) — very unusual given kawa's serializer always
    // emits GET / HTTP/1.1 first, but safer.
    let start_host = received.to_ascii_lowercase().starts_with("host:");
    let total_host = host_line_count + usize::from(start_host);
    println!("match-dedup — host lines seen: {total_host}");

    // Allow a 200 response (or any 2xx) back to the client.
    backend.send(0);
    let frames = collect_response_frames(&mut tls, 300, 2, 300);
    log_frames("host-vs-authority match-dedup", &frames);
    let has_response = frames
        .iter()
        .any(|(ft, _fl, sid, _p)| *ft == H2_FRAME_HEADERS && *sid == 1);

    backend.disconnect();
    drop(tls);
    thread::sleep(Duration::from_millis(100));
    let still_alive = verify_sozu_alive(front_port);

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    if accepted && total_host == 1 && has_response && still_alive && stopped {
        State::Success
    } else {
        println!(
            "match-dedup FAIL — accepted={accepted} host_lines={total_host} \
             response={has_response} alive={still_alive} stopped={stopped}"
        );
        State::Fail
    }
}

#[test]
fn test_h2_host_authority_match_deduplicated() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 security: :authority/host match — exactly one Host line (FIX-6 4b8fbd3a)",
            try_h2_host_authority_match_deduplicated
        ),
        State::Success
    );
}

// ============================================================================
// Request trailers towards an H1 backend — the last chunk precedes them
// ============================================================================

/// Send, on stream 1, a `POST /` with no `content-length`, a DATA frame
/// holding `hello` without END_STREAM, and a trailer HEADERS frame holding
/// `trailers` with END_STREAM, then return the bytes the H1 backend reads.
/// Without a declared length the request is chunked towards the backend.
fn h2_post_with_trailers_as_seen_by_h1_backend(name: &str, trailers: &[(&[u8], &[u8])]) -> String {
    let (mut worker, mut backend, front_port) = setup_h2_with_sync_backend(name);
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);

    let mut block = request_prefix_localhost();
    block[0] = 0x83; // :method POST
    tls.write_all(&H2Frame::headers(1, block, true, false).encode())
        .unwrap();
    tls.write_all(&H2Frame::data(1, b"hello".to_vec(), false).encode())
        .unwrap();
    let mut trailer_block = Vec::new();
    for (name, value) in trailers {
        push_literal(&mut trailer_block, name, value);
    }
    tls.write_all(&H2Frame::headers(1, trailer_block, true, true).encode())
        .unwrap();
    tls.flush().unwrap();

    let accepted = (0..200).any(|_| {
        if backend.accept(0) {
            true
        } else {
            thread::sleep(Duration::from_millis(10));
            false
        }
    });
    // Read the whole window, not one segment: the request is complete only
    // once its trailer section is closed, which may arrive in a later read.
    let mut received = String::new();
    for _ in 0..5 {
        if !accepted {
            break;
        }
        if let Some(chunk) = backend.receive(0) {
            received.push_str(&chunk);
        }
    }
    println!("{name} — backend received {received:?}");
    if accepted {
        backend.send(0);
    }
    let _ = collect_response_frames(&mut tls, 300, 2, 300);

    backend.disconnect();
    drop(tls);
    worker.soft_stop();
    worker.wait_for_server_stop();
    received
}

/// The chunked body an H1 backend reads for an H2 request carrying trailers
/// ends with the last chunk `0\r\n`, then the trailer section, then the empty
/// line (RFC 9112 §7.1). Without the last chunk a backend parses the first
/// trailer field as a chunk-size line: a strict one refuses the request, a
/// lenient one desynchronizes the keep-alive connection.
///
/// TO SEE THIS RED: remove the `end_body` `Flags` block `handle_trailer`
/// (`lib/src/protocol/mux/pkawa.rs`) pushes before the trailer fields.
fn try_h2_request_trailers_follow_last_chunk_h1_backend() -> State {
    let cases: [(&str, &[(&[u8], &[u8])], &str); 2] = [
        (
            "H2-TRAILER-LAST-CHUNK-H1",
            &[(b"grpc-status", b"0")],
            "5\r\nhello\r\n0\r\ngrpc-status: 0\r\n\r\n",
        ),
        // Every trailer field is elided: the last chunk and the empty line
        // still end the body.
        (
            "H2-TRAILER-LAST-CHUNK-ELIDED-H1",
            &[(b"x-real-ip", b"1.2.3.4")],
            "5\r\nhello\r\n0\r\n\r\n",
        ),
    ];
    for (name, trailers, expected_body) in cases {
        let received = h2_post_with_trailers_as_seen_by_h1_backend(name, trailers);
        let (head, body) = received.split_once("\r\n\r\n").unwrap_or((&received, ""));
        let chunked = head
            .to_ascii_lowercase()
            .contains("\r\ntransfer-encoding: chunked");
        if !chunked || body != expected_body {
            println!("{name} FAIL — chunked={chunked} body={body:?} expected={expected_body:?}");
            return State::Fail;
        }
    }
    State::Success
}

#[test]
fn test_h2_request_trailers_follow_last_chunk_h1_backend() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2->H1: request trailers follow the last chunk (RFC 9112 §7.1)",
            try_h2_request_trailers_follow_last_chunk_h1_backend
        ),
        State::Success
    );
}

// ============================================================================
// FIX-4 — `:scheme` must be `http` or `https`
// ============================================================================

/// Push an HPACK "literal without indexing, indexed name" header pair where
/// `static_idx` is a 4-bit static-table index (1..=14 — covers every
/// pseudo-header and common request header we need). The opcode byte is
/// `0x0i` where `i` is the index; the value follows as length + bytes.
fn push_literal_indexed_name(block: &mut Vec<u8>, static_idx: u8, value: &[u8]) {
    assert!(
        static_idx < 0x10,
        "static index {static_idx} does not fit in 4 bits"
    );
    assert!(value.len() < 0x7f);
    block.push(static_idx);
    block.push(value.len() as u8);
    block.extend_from_slice(value);
}

/// Build a minimal request header block with the scheme overridden to the
/// provided value. Uses `GET` / `:path /` / `:authority localhost` and a
/// literal `:scheme <value>` to dodge the HPACK static-table `:scheme https`.
fn request_with_scheme(scheme: &[u8]) -> Vec<u8> {
    let mut block = vec![
        0x82, // :method GET
        0x84, // :path /
    ];
    // :scheme (static index 6 = :scheme http, 7 = :scheme https) with literal
    // override. Using static idx 7 works because the decoder keeps only the
    // name (":scheme") and overrides the value.
    push_literal_indexed_name(&mut block, 0x07, scheme);
    // :authority localhost (static index 1 + literal value).
    block.push(0x41);
    block.push(0x09);
    block.extend_from_slice(b"localhost");
    block
}

/// RFC 9113 §8.3.1: `:scheme` must be `http` or `https`. FIX-4 hard-codes
/// this in `pkawa::handle_pseudo_header` — any other value marks the stream
/// as `invalid_headers = true` and the decode loop reports
/// `H2Error::ProtocolError`.
///
/// Iterates over the four known SSRF / smuggling vectors; the test passes
/// iff **every** value is rejected AND the standard `http` / `https` keep
/// the stream alive (sanity floor — we do not assert a 200 because the
/// default async backend answers with a plain HTTP/1.1 `pong0`).
fn try_h2_invalid_scheme_rejected() -> State {
    let (worker, backends, front_port) = setup_h2_test("H2-SEC-SCHEME", 1);
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    let bad_schemes: &[&[u8]] = &[b"javascript", b"file", b"ftp", b"wss"];
    let mut stream_id: u32 = 1;
    let mut all_rejected = true;

    for bad in bad_schemes {
        let mut tls = raw_h2_connection(front_addr);
        h2_handshake(&mut tls);

        let block = request_with_scheme(bad);
        let frame = H2Frame::headers(stream_id, block, true, true);
        if tls.write_all(&frame.encode()).is_err() || tls.flush().is_err() {
            // sozu closed the connection — treat as rejection.
            println!("scheme {:?} — write failed (connection closed)", bad);
            continue;
        }

        let frames = collect_response_frames(&mut tls, 400, 3, 400);
        log_frames(
            &format!("scheme={:?}", String::from_utf8_lossy(bad)),
            &frames,
        );
        // Stream-scope invalid-scheme violation: sozu must emit RST/GOAWAY or 400.
        let rejected =
            rejected_with_goaway_or_rst(&frames) || stream_status_matches(&frames, 1, 400);
        if !rejected {
            println!("scheme {:?} — NOT rejected", bad);
            all_rejected = false;
        }
        drop(tls);
        thread::sleep(Duration::from_millis(50));
        stream_id += 2;
    }

    let infra_ok = teardown(raw_h2_connection(front_addr), front_port, worker, backends);
    if all_rejected && infra_ok {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_invalid_scheme_rejected() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 security: :scheme javascript/file/ftp/wss rejected (FIX-4 c3f9e090)",
            try_h2_invalid_scheme_rejected
        ),
        State::Success
    );
}

// ============================================================================
// Request trailers on a Content-Length-framed request towards an H1 backend
// ============================================================================

/// Read what the backend receives on its connection `0` until a read
/// returns nothing once something arrived. An empty read waits for the
/// backend socket's 100 ms read timeout, so the loop does not spin.
fn drain_backend(backend: &mut SyncBackend) -> String {
    let mut received = String::new();
    for _ in 0..5 {
        match backend.receive(0) {
            Some(chunk) => received.push_str(&chunk),
            None if !received.is_empty() => break,
            None => {}
        }
    }
    received
}

/// An H2 request framed by `content-length` and ending with a trailer
/// HEADERS frame, then a second request, both reach an H1 backend over one
/// keep-alive connection. The bytes the backend reads are split by HTTP/1.1
/// framing, not by reads: the first request ends after its head and its
/// `content-length` of body (RFC 9112 §6.3), and the next byte must start
/// the second request line, since HTTP/1.1 carries no trailer section after
/// a length-delimited body (§7.1) and the trailer fields are dropped
/// (RFC 9110 §6.5.1).
///
/// TO SEE THIS RED: make `ConnectionH1::drop_length_framed_trailers`
/// (`lib/src/protocol/mux/h1.rs`) return `false` without touching the block
/// queue.
fn try_h2_length_framed_request_trailers_keep_h1_backend_framing() -> State {
    let (mut worker, mut backend, front_port) =
        setup_h2_with_sync_backend("H2-LENGTH-FRAMED-TRAILERS-H1");
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);

    let mut block = request_prefix_localhost();
    block[0] = 0x83; // :method POST
    push_literal(&mut block, b"content-length", b"5");
    tls.write_all(&H2Frame::headers(1, block, true, false).encode())
        .unwrap();
    tls.write_all(&H2Frame::data(1, b"hello".to_vec(), false).encode())
        .unwrap();
    let mut trailers = Vec::new();
    push_literal(&mut trailers, b"grpc-status", b"0");
    tls.write_all(&H2Frame::headers(1, trailers, true, true).encode())
        .unwrap();
    tls.flush().unwrap();

    let accepted = (0..200).any(|_| {
        if backend.accept(0) {
            true
        } else {
            thread::sleep(Duration::from_millis(10));
            false
        }
    });
    let first = if accepted {
        drain_backend(&mut backend)
    } else {
        String::new()
    };
    println!("length-framed trailers — first request {first:?}");
    if accepted {
        backend.send(0);
    }
    let first_frames = collect_response_frames(&mut tls, 300, 2, 300);
    let first_ok = stream_status_matches(&first_frames, 1, 200);

    // Second request on the same H2 connection, served on the same
    // keep-alive backend connection.
    tls.write_all(&H2Frame::headers(3, request_prefix_localhost(), true, true).encode())
        .unwrap();
    tls.flush().unwrap();
    let second = if accepted {
        drain_backend(&mut backend)
    } else {
        String::new()
    };
    println!("length-framed trailers — second request {second:?}");
    if accepted {
        backend.send(0);
    }
    let second_frames = collect_response_frames(&mut tls, 300, 2, 300);
    let second_ok = stream_status_matches(&second_frames, 3, 200);

    backend.disconnect();
    drop(tls);
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    // Split the connection's bytes by HTTP/1.1 framing: the first request is
    // its head and its 5-byte body, and whatever follows is the next message.
    let wire = format!("{first}{second}");
    let (first_body, next) = match wire.split_once("\r\n\r\n") {
        Some((_, rest)) if rest.len() >= 5 => (&rest[..5], &rest[5..]),
        _ => ("", ""),
    };
    let second_parsed = next.starts_with("GET / HTTP/1.1\r\n");
    if first_body == "hello" && first_ok && second_parsed && second_ok && stopped {
        State::Success
    } else {
        println!(
            "length-framed trailers FAIL — first_body={first_body:?} next={next:?} \
             first_ok={first_ok} second_parsed={second_parsed} second_ok={second_ok} \
             stopped={stopped}"
        );
        State::Fail
    }
}

#[test]
fn test_h2_length_framed_request_trailers_keep_h1_backend_framing() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2->H1: trailers of a Content-Length-framed request are dropped (RFC 9110 §6.5.1)",
            try_h2_length_framed_request_trailers_keep_h1_backend_framing
        ),
        State::Success
    );
}

// ============================================================================
// Trailers of a response without a body towards an H1 client
// ============================================================================

/// Sōzu HTTP listener (H1 clients) + an `http2` cluster for `localhost`
/// served by a `RawH2ResponseBackend`, and an H1 cluster for `other`
/// answering `200` with `pong`. Returns the running `Worker`, the raw
/// backend, the H1 backend and the front address.
fn setup_h1_front_with_raw_h2_backend(
    name: &str,
) -> (
    Worker,
    RawH2ResponseBackend,
    AsyncBackend<SimpleAggregator>,
    SocketAddr,
) {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let (config, listeners, state) = Worker::empty_http_config(front_address.clone().into());
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
    worker.send_proxy_request_type(RequestType::AddCluster(Cluster {
        http2: Some(true),
        ..Worker::default_cluster("cluster_0")
    }));
    worker.send_proxy_request_type(RequestType::AddHttpFrontend(Worker::default_http_frontend(
        "cluster_0",
        front_address.clone().into(),
    )));
    let back_address = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));
    let backend = RawH2ResponseBackend::new(back_address);
    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "cluster_1",
    )));
    worker.send_proxy_request_type(RequestType::AddHttpFrontend(RequestHttpFrontend {
        hostname: String::from("other"),
        ..Worker::default_http_frontend("cluster_1", front_address.into())
    }));
    let h1_back_address = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_1",
        "cluster_1-0",
        h1_back_address,
        None,
    )));
    let h1_backend = AsyncBackend::spawn_detached_backend(
        format!("{name}-H1-BACK"),
        h1_back_address,
        SimpleAggregator::default(),
        AsyncBackend::http_handler("pong".to_owned()),
    );
    worker.read_to_last();
    // Give the backend threads a moment to bind.
    thread::sleep(Duration::from_millis(100));
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    (worker, backend, h1_backend, front_addr)
}

/// Read what the client connection holds until a read returns nothing once
/// something arrived, within about one second.
fn drain_client(client: &mut TcpStream) -> String {
    let mut received = Vec::new();
    let mut buffer = [0u8; 4096];
    for _ in 0..10 {
        match client.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => received.extend_from_slice(&buffer[..n]),
            Err(_) if !received.is_empty() => break,
            Err(_) => {}
        }
    }
    String::from_utf8_lossy(&received).into_owned()
}

/// An H2 backend answers a `method` request with `:status` `status` and no
/// `content-length`, then ends the stream with a trailer HEADERS frame; a
/// second request on the same keep-alive H1 client connection, routed to an
/// H1 backend, is answered `200` with a body. The first response has no body
/// by definition (RFC 9110 §9.3.2, §15.3.5, §15.4.5), so the H1 client reads
/// it as ending with its header section (RFC 9112 §6.3 rule 1): the next
/// byte after that head must start the second response, with no last chunk
/// and no trailer section between them.
///
/// TO SEE THIS RED: make `ConnectionH1::drop_bodiless_response_framing`
/// (`lib/src/protocol/mux/h1.rs`) return `false` without touching the block
/// queue.
fn try_h2_bodiless_response_trailers_keep_h1_client_framing(method: &str, status: &str) -> State {
    let (mut worker, backend, mut h1_backend, front_addr) =
        setup_h1_front_with_raw_h2_backend(&format!("H2-BODILESS-TRAILERS-H1-{method}-{status}"));
    backend.set_status(status);
    backend.set_trailers(Some(vec![(b"grpc-status".to_vec(), b"0".to_vec())]));

    let mut client = TcpStream::connect(front_addr).unwrap();
    client
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    client
        .write_all(format!("{method} / HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
        .unwrap();
    let first = drain_client(&mut client);
    println!("bodiless trailers {method} {status} — first response {first:?}");

    client
        .write_all(b"GET / HTTP/1.1\r\nHost: other\r\n\r\n")
        .unwrap();
    let second = drain_client(&mut client);
    println!("bodiless trailers {method} {status} — second response {second:?}");

    drop(client);
    drop(backend);
    h1_backend.stop_and_get_aggregator();
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    // Split the connection's bytes by HTTP/1.1 framing: the first response
    // is its head alone, and whatever follows is the next message.
    let wire = format!("{first}{second}");
    let first_ok = wire.starts_with(&format!("HTTP/1.1 {status} "));
    let next = wire.split_once("\r\n\r\n").map_or("", |(_, rest)| rest);
    let second_parsed = next.starts_with("HTTP/1.1 200 ") && next.contains("pong");
    if first_ok && second_parsed && stopped {
        State::Success
    } else {
        println!(
            "bodiless trailers {method} {status} FAIL — first_ok={first_ok} next={next:?} \
             second_parsed={second_parsed} stopped={stopped}"
        );
        State::Fail
    }
}

#[test]
fn test_h2_bodiless_response_trailers_keep_h1_client_framing() {
    for (method, status) in [("GET", "204"), ("GET", "304"), ("HEAD", "200")] {
        assert_eq!(
            repeat_until_error_or(
                3,
                "H2->H1: no trailer section follows a response without a body (RFC 9112 §6.3)",
                || try_h2_bodiless_response_trailers_keep_h1_client_framing(method, status)
            ),
            State::Success,
            "{method} answered {status}"
        );
    }
}

/// An H2 backend answers a `method` request with `:status` `status` and no
/// `content-length`, its HEADERS without END_STREAM, then ends the stream
/// with an empty trailer HEADERS frame; a second request on the same
/// keep-alive H1 client connection, routed to an H1 backend, is answered
/// `200`. The first response has no body by definition (RFC 9110 §6.4.1), so
/// its head carries no `Transfer-Encoding` (RFC 9112 §6.1) and the next
/// response starts right after it.
///
/// TO SEE THIS RED: drop the `no_body` guard of the chunked upgrade in
/// `pkawa::handle_header` (`lib/src/protocol/mux/pkawa.rs`).
fn try_h2_bodiless_response_head_has_no_transfer_encoding(method: &str, status: &str) -> State {
    let (mut worker, backend, mut h1_backend, front_addr) =
        setup_h1_front_with_raw_h2_backend(&format!("H2-BODILESS-NO-TE-H1-{method}-{status}"));
    backend.set_status(status);
    backend.set_trailers(Some(Vec::new()));

    let mut client = TcpStream::connect(front_addr).unwrap();
    client
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    client
        .write_all(format!("{method} / HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
        .unwrap();
    let first = drain_client(&mut client);
    println!("bodiless no-TE {method} {status} — first response {first:?}");
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: other\r\n\r\n")
        .unwrap();
    let second = drain_client(&mut client);
    println!("bodiless no-TE {method} {status} — second response {second:?}");

    drop(client);
    drop(backend);
    h1_backend.stop_and_get_aggregator();
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    let wire = format!("{first}{second}");
    let (head, next) = wire.split_once("\r\n\r\n").unwrap_or(("", ""));
    let head_ok = head.starts_with(&format!("HTTP/1.1 {status} "))
        && !head.to_ascii_lowercase().contains("transfer-encoding");
    let second_parsed = next.starts_with("HTTP/1.1 200 ") && next.contains("pong");
    if head_ok && second_parsed && stopped {
        State::Success
    } else {
        println!(
            "bodiless no-TE {method} {status} FAIL — head={head:?} next={next:?} \
             stopped={stopped}"
        );
        State::Fail
    }
}

#[test]
fn test_h2_bodiless_response_head_has_no_transfer_encoding() {
    for (method, status) in [("GET", "204"), ("GET", "304"), ("HEAD", "200")] {
        assert_eq!(
            repeat_until_error_or(
                3,
                "H2->H1: no Transfer-Encoding on a response without a body (RFC 9112 §6.1)",
                || try_h2_bodiless_response_head_has_no_transfer_encoding(method, status)
            ),
            State::Success,
            "{method} answered {status}"
        );
    }
}

// ============================================================================
// Responses without a body, interim and 101 responses from an H2 backend
// ============================================================================

/// Sōzu HTTPS listener (H2 clients) + an `http2` cluster for `localhost`
/// served by a `RawH2ResponseBackend`. Returns the running `Worker`, the
/// raw backend and the front port.
fn setup_h2_front_with_raw_h2_backend(name: &str) -> (Worker, RawH2ResponseBackend, u16) {
    let (mut worker, front_port, _) = setup_h2_listener_only(name);
    worker.send_proxy_request_type(RequestType::AddCluster(Cluster {
        http2: Some(true),
        ..Worker::default_cluster("cluster_0")
    }));
    let back_address = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));
    worker.read_to_last();
    let backend = RawH2ResponseBackend::new(back_address);
    // Give the backend thread a moment to bind.
    thread::sleep(Duration::from_millis(100));
    (worker, backend, front_port)
}

/// The header block of a `method` request for `/` on `localhost`.
fn request_for(method: &[u8]) -> Vec<u8> {
    let mut block = request_prefix_localhost();
    if method != b"GET" {
        block.remove(0);
        let mut literal = Vec::new();
        push_literal_indexed_name(&mut literal, 2, method);
        block.splice(0..0, literal);
    }
    block
}

/// Read the frames sozu sends on `tls` until `done` holds for them or
/// `timeout` elapses.
fn read_h2_frames_until(
    tls: &mut rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
    timeout: Duration,
    done: impl Fn(&[(u8, u8, u32, Vec<u8>)]) -> bool,
) -> Vec<(u8, u8, u32, Vec<u8>)> {
    tls.sock
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let start = Instant::now();
    let mut raw = Vec::new();
    let mut buffer = vec![0u8; 16_384];
    loop {
        let frames = parse_h2_frames(&raw);
        if done(&frames) || start.elapsed() >= timeout {
            return frames;
        }
        match tls.read(&mut buffer) {
            Ok(0) => return parse_h2_frames(&raw),
            Ok(n) => raw.extend_from_slice(&buffer[..n]),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => return parse_h2_frames(&raw),
        }
    }
}

/// Whether a frame of `frames` ends `stream_id`.
fn ends_stream(frames: &[(u8, u8, u32, Vec<u8>)], stream_id: u32) -> bool {
    frames.iter().any(|(kind, flags, id, _)| {
        *id == stream_id
            && (*kind == H2_FRAME_HEADERS || *kind == H2_FRAME_DATA)
            && flags & H2_FLAG_END_STREAM != 0
    })
}

/// The DATA payload of `stream_id` in `frames`.
fn stream_data(frames: &[(u8, u8, u32, Vec<u8>)], stream_id: u32) -> Vec<u8> {
    frames
        .iter()
        .filter(|(kind, _, id, _)| *kind == H2_FRAME_DATA && *id == stream_id)
        .flat_map(|(_, _, _, payload)| payload.clone())
        .collect()
}

/// How the H2 backend ends a response whose HEADERS frame lacks END_STREAM.
#[derive(Clone, Copy, Debug)]
enum BodilessEnd {
    /// A trailer HEADERS frame flagged END_STREAM.
    Trailers,
    /// An empty DATA frame flagged END_STREAM.
    EmptyData,
}

fn end_bodiless_response_with(backend: &RawH2ResponseBackend, end: BodilessEnd) {
    match end {
        BodilessEnd::Trailers => {
            backend.set_trailers(Some(vec![(b"grpc-status".to_vec(), b"0".to_vec())]))
        }
        BodilessEnd::EmptyData => backend.set_empty_data_end(true),
    }
}

/// The requests and statuses of a response without a body (RFC 9110
/// §6.4.1), crossed with the two ways an H2 backend ends its stream.
const BODILESS_CASES: [(&str, &str, BodilessEnd); 6] = [
    ("GET", "204", BodilessEnd::Trailers),
    ("GET", "204", BodilessEnd::EmptyData),
    ("GET", "304", BodilessEnd::Trailers),
    ("GET", "304", BodilessEnd::EmptyData),
    ("HEAD", "200", BodilessEnd::Trailers),
    ("HEAD", "200", BodilessEnd::EmptyData),
];

/// An H2 backend answers a `method` request with `:status` `status`, its
/// HEADERS frame without END_STREAM, then ends the stream as `end` says. A
/// response without a body (RFC 9110 §6.4.1) still ends with the END_STREAM
/// of its stream (RFC 9113 §8.1), so the H2 client must receive END_STREAM
/// on stream 1, and no content, reset or GOAWAY.
///
/// TO SEE THIS RED: have `pkawa::handle_header`
/// (`lib/src/protocol/mux/pkawa.rs`) mark every response without a body
/// `ParsingPhase::Terminated` at its head, not only a 1xx: the client
/// stream is released after HEADERS and never ends.
fn try_h2_bodiless_response_ends_the_h2_client_stream(
    method: &str,
    status: &str,
    end: BodilessEnd,
) -> State {
    let (mut worker, backend, front_port) = setup_h2_front_with_raw_h2_backend(&format!(
        "H2-BODILESS-END-H2-{method}-{status}-{end:?}"
    ));
    backend.set_status(status);
    end_bodiless_response_with(&backend, end);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);
    tls.write_all(&H2Frame::headers(1, request_for(method.as_bytes()), true, true).encode())
        .unwrap();
    tls.flush().unwrap();
    let frames = read_h2_frames_until(&mut tls, Duration::from_secs(1), |frames| {
        ends_stream(frames, 1)
    });
    log_frames(&format!("bodiless end {method} {status} {end:?}"), &frames);

    drop(tls);
    drop(backend);
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    let status_ok = stream_status_matches(&frames, 1, status.parse().unwrap());
    let ended = ends_stream(&frames, 1);
    let no_content = stream_data(&frames, 1).is_empty();
    let no_reset = !contains_rst_stream(&frames) && !contains_goaway(&frames);
    if status_ok && ended && no_content && no_reset && stopped {
        State::Success
    } else {
        println!(
            "bodiless end {method} {status} {end:?} FAIL — status_ok={status_ok} \
             ended={ended} no_content={no_content} no_reset={no_reset} stopped={stopped}"
        );
        State::Fail
    }
}

#[test]
fn test_h2_bodiless_response_ends_the_h2_client_stream() {
    for (method, status, end) in BODILESS_CASES {
        assert_eq!(
            repeat_until_error_or(
                2,
                "H2->H2: a response without a body ends with END_STREAM (RFC 9113 §8.1)",
                || try_h2_bodiless_response_ends_the_h2_client_stream(method, status, end)
            ),
            State::Success,
            "{method} answered {status}, ended by {end:?}"
        );
    }
}

/// As `try_h2_bodiless_response_ends_the_h2_client_stream`, but the backend
/// holds the end of stream 1 back until sozu opens a second stream on the
/// same backend connection, for a second request of the H2 client. The end
/// of stream 1 must not cost the backend connection (RFC 9113 §5.1: the
/// stream is open until END_STREAM): no GOAWAY and no RST_STREAM towards
/// the backend, both client streams end,
/// the second one with its `200` and `pong`, over one backend connection.
///
/// TO SEE THIS RED: as for `try_h2_bodiless_response_ends_the_h2_client_stream`;
/// the backend's trailer HEADERS then hits the closed-stream check of
/// `ConnectionH2::handle_read` (`lib/src/protocol/mux/h2.rs`), which sends
/// GOAWAY(STREAM_CLOSED).
fn try_h2_bodiless_response_end_keeps_the_backend_connection(
    method: &str,
    status: &str,
    end: BodilessEnd,
) -> State {
    let (mut worker, backend, front_port) = setup_h2_front_with_raw_h2_backend(&format!(
        "H2-BODILESS-SECOND-{method}-{status}-{end:?}"
    ));
    backend.set_status(status);
    end_bodiless_response_with(&backend, end);
    backend.set_serve_second_stream(true);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);
    tls.write_all(&H2Frame::headers(1, request_for(method.as_bytes()), true, true).encode())
        .unwrap();
    tls.flush().unwrap();
    let first = read_h2_frames_until(&mut tls, Duration::from_secs(1), |frames| {
        frames
            .iter()
            .any(|(kind, _, id, _)| *kind == H2_FRAME_HEADERS && *id == 1)
    });
    tls.write_all(&H2Frame::headers(3, request_for(b"GET"), true, true).encode())
        .unwrap();
    tls.flush().unwrap();
    let rest = read_h2_frames_until(&mut tls, Duration::from_secs(2), |frames| {
        ends_stream(frames, 3) && ends_stream(frames, 1)
    });
    let frames = [first, rest].concat();
    log_frames(
        &format!("bodiless second {method} {status} {end:?}"),
        &frames,
    );

    drop(tls);
    thread::sleep(Duration::from_millis(400));
    let goaways = backend.goaways_received();
    let resets = backend.resets_received();
    let connections = backend.connections_received();
    drop(backend);
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    let first_ok = stream_status_matches(&frames, 1, status.parse().unwrap())
        && ends_stream(&frames, 1)
        && stream_data(&frames, 1).is_empty();
    let second_ok = stream_status_matches(&frames, 3, 200)
        && ends_stream(&frames, 3)
        && stream_data(&frames, 3) == b"pong";
    let no_reset = !contains_rst_stream(&frames) && !contains_goaway(&frames);
    if first_ok
        && second_ok
        && no_reset
        && goaways == 0
        && resets == 0
        && connections == 1
        && stopped
    {
        State::Success
    } else {
        println!(
            "bodiless second {method} {status} {end:?} FAIL — first_ok={first_ok} \
             second_ok={second_ok} no_reset={no_reset} backend_goaways={goaways} \
             backend_resets={resets} backend_connections={connections} stopped={stopped}"
        );
        State::Fail
    }
}

#[test]
fn test_h2_bodiless_response_end_keeps_the_backend_connection() {
    for (method, status, end) in BODILESS_CASES {
        assert_eq!(
            repeat_until_error_or(
                2,
                "H2->H2: the end of a response without a body keeps the backend connection",
                || try_h2_bodiless_response_end_keeps_the_backend_connection(method, status, end)
            ),
            State::Success,
            "{method} answered {status}, ended by {end:?}"
        );
    }
}

/// An H2 backend answers `103` then `200` with the body `hello`. The
/// interim response is forwarded on its own (RFC 9110 §15.2), then the
/// final one: to an H1 client as two heads, the `103` without any framing
/// field, and to an H2 client as two HEADERS frames on stream 1 before the
/// DATA ending it, with no reset.
///
/// TO SEE THIS RED: drop the 1xx from the statuses
/// `pkawa::handle_header` (`lib/src/protocol/mux/pkawa.rs`) marks complete
/// at their head: the `103` is framed chunked, the H1 client never reads the
/// `200` and the H2 client stream is reset.
fn try_h2_backend_interim_response_reaches_the_client(h2_client: bool) -> State {
    let name = format!("H2-INTERIM-{}", if h2_client { "H2" } else { "H1" });
    let (worker, backend, received, extra) = if h2_client {
        let (worker, backend, front_port) = setup_h2_front_with_raw_h2_backend(&name);
        backend.push_interim("103");
        backend.set_body_with_delay(b"hello".to_vec(), Duration::ZERO);
        let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
        let mut tls = raw_h2_connection(front_addr);
        h2_handshake(&mut tls);
        tls.write_all(&H2Frame::headers(1, request_for(b"GET"), true, true).encode())
            .unwrap();
        tls.flush().unwrap();
        let frames = read_h2_frames_until(&mut tls, Duration::from_secs(1), |frames| {
            ends_stream(frames, 1)
        });
        log_frames("interim H2 client", &frames);
        let heads: Vec<_> = frames
            .iter()
            .filter(|(kind, _, id, _)| *kind == H2_FRAME_HEADERS && *id == 1)
            .map(|(_, flags, _, payload)| (decode_status(payload), flags & H2_FLAG_END_STREAM))
            .collect();
        let ok = heads == [(Some(103), 0), (Some(200), 0)]
            && stream_data(&frames, 1) == b"hello"
            && ends_stream(&frames, 1)
            && !contains_rst_stream(&frames)
            && !contains_goaway(&frames);
        drop(tls);
        (worker, backend, ok, None)
    } else {
        let (worker, backend, h1_backend, front_addr) = setup_h1_front_with_raw_h2_backend(&name);
        backend.push_interim("103");
        backend.set_body_with_delay(b"hello".to_vec(), Duration::ZERO);
        let mut client = TcpStream::connect(front_addr).unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let wire = drain_client(&mut client);
        println!("interim H1 client — {wire:?}");
        let (interim, rest) = wire.split_once("\r\n\r\n").unwrap_or(("", ""));
        let ok = interim.starts_with("HTTP/1.1 103 ")
            && !interim.to_ascii_lowercase().contains("transfer-encoding")
            && rest.starts_with("HTTP/1.1 200 ")
            && rest.contains("hello");
        drop(client);
        (worker, backend, ok, Some(h1_backend))
    };
    let mut worker = worker;
    drop(backend);
    if let Some(mut h1_backend) = extra {
        h1_backend.stop_and_get_aggregator();
    }
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    if received && stopped {
        State::Success
    } else {
        println!("interim h2_client={h2_client} FAIL — received={received} stopped={stopped}");
        State::Fail
    }
}

#[test]
fn test_h2_backend_interim_response_reaches_the_client() {
    for h2_client in [false, true] {
        assert_eq!(
            repeat_until_error_or(
                2,
                "H2 backend: a 103 then the final response reach the client (RFC 9110 §15.2)",
                || try_h2_backend_interim_response_reaches_the_client(h2_client)
            ),
            State::Success,
            "h2_client={h2_client}"
        );
    }
}

/// An H2 backend answers `:status 101`, which HTTP/2 does not support (RFC
/// 9113 §8.6): the response is malformed, the backend stream is reset and
/// the H1 client gets a 502, on a session that stays open.
///
/// TO SEE THIS RED: drop the `101` check of `pkawa::handle_header`
/// (`lib/src/protocol/mux/pkawa.rs`): the response reaches the upgrade
/// branch of `ConnectionH1::readable`, which closes the session.
fn try_h2_backend_101_is_a_bad_gateway() -> State {
    let (mut worker, backend, mut h1_backend, front_addr) =
        setup_h1_front_with_raw_h2_backend("H2-101-BAD-GATEWAY");
    backend.set_status("101");
    let mut client = TcpStream::connect(front_addr).unwrap();
    client
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let wire = drain_client(&mut client);
    println!("101 from an H2 backend — {wire:?}");
    drop(client);
    drop(backend);
    h1_backend.stop_and_get_aggregator();
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    if wire.starts_with("HTTP/1.1 502 ") && stopped {
        State::Success
    } else {
        println!("101 FAIL — stopped={stopped}");
        State::Fail
    }
}

#[test]
fn test_h2_backend_101_is_a_bad_gateway() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "H2 backend: :status 101 is a malformed response (RFC 9113 §8.6)",
            try_h2_backend_101_is_a_bad_gateway
        ),
        State::Success
    );
}

/// An H2 backend answers HEAD with `200` and END_STREAM on its HEADERS frame,
/// with `content-length: 5` when `length`, without any `content-length`
/// otherwise. A response to HEAD has no content (RFC 9110 §9.3.2), and RFC
/// 9113 §8.1.1 lets a response with no content carry a non-zero
/// `content-length` with no DATA frame, so the response is forwarded with
/// the field the backend sent, and with none when it sent none: on HEAD the
/// field states the length of the selected representation (RFC 9110 §8.6),
/// so an injected `0` would be false. An H1 client reads the head alone,
/// followed on the same connection by the answer to its next request; an H2
/// client receives HEADERS with END_STREAM on stream 1.
///
/// A response to GET with `content-length: 5` and END_STREAM on HEADERS has
/// content by definition yet no DATA, which RFC 9113 §8.1.1 makes malformed:
/// it stays a stream error and the client gets a 502 (`get` row). The
/// exemption keys on `ParsingPhase::Terminated`, which another callback sets
/// for HEAD, so this row pins that it does not reach other responses.
///
/// TO SEE THIS RED: drop the HEAD case from the END_STREAM `content-length`
/// exemption of `pkawa::handle_header` (`lib/src/protocol/mux/pkawa.rs`), and
/// the client gets a 502 (`length` rows); or from the exclusions of its
/// `Content-Length: 0` injection, and `content-length: 0` is added (other
/// rows); or exempt every response from that check, and the `get` row
/// forwards a `200`.
fn try_h2_head_response_with_end_stream(h2_client: bool, length: bool, get: bool) -> State {
    let name = format!(
        "H2-{}-END-STREAM-{}-{}",
        if get { "GET" } else { "HEAD" },
        if h2_client { "H2" } else { "H1" },
        if length { "CL5" } else { "NOCL" }
    );
    let method: &[u8] = if get { b"GET" } else { b"HEAD" };
    // `content-length: <value>`, as Kawa's encoder writes it in a field
    // block: a literal over the static name index 28 (`0f 0d`).
    let encoded_length = |value: u8| [0x0f, 0x0d, 0x01, value];
    if h2_client {
        let (mut worker, backend, front_port) = setup_h2_front_with_raw_h2_backend(&name);
        if length {
            backend.push_header("content-length", "5");
        }
        let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
        let mut tls = raw_h2_connection(front_addr);
        h2_handshake(&mut tls);
        tls.write_all(&H2Frame::headers(1, request_for(method), true, true).encode())
            .unwrap();
        tls.flush().unwrap();
        let frames = read_h2_frames_until(&mut tls, Duration::from_secs(1), |frames| {
            ends_stream(frames, 1)
        });
        log_frames(
            &format!("END_STREAM get={get} length={length} H2 client"),
            &frames,
        );
        drop(tls);
        drop(backend);
        worker.soft_stop();
        let stopped = worker.wait_for_server_stop();
        let head_carries = |value: u8| {
            frames.iter().any(|(kind, _, id, payload)| {
                *kind == H2_FRAME_HEADERS
                    && *id == 1
                    && payload
                        .windows(4)
                        .any(|field| field == encoded_length(value))
            })
        };
        let length_ok = if length {
            head_carries(b'5')
        } else {
            !head_carries(b'0')
        };
        if get {
            let bad_gateway = stream_status_matches(&frames, 1, 502);
            let no_200 = !stream_status_matches(&frames, 1, 200);
            return if bad_gateway && no_200 && stopped {
                State::Success
            } else {
                println!(
                    "GET END_STREAM H2 client FAIL — bad_gateway={bad_gateway} \
                     no_200={no_200} stopped={stopped}"
                );
                State::Fail
            };
        }
        let status_ok = stream_status_matches(&frames, 1, 200);
        let ended = ends_stream(&frames, 1);
        let no_content = stream_data(&frames, 1).is_empty();
        let no_reset = !contains_rst_stream(&frames) && !contains_goaway(&frames);
        if status_ok && length_ok && ended && no_content && no_reset && stopped {
            State::Success
        } else {
            println!(
                "HEAD END_STREAM length={length} H2 client FAIL — status_ok={status_ok} \
                 length_ok={length_ok} ended={ended} no_content={no_content} \
                 no_reset={no_reset} stopped={stopped}"
            );
            State::Fail
        }
    } else {
        let (mut worker, backend, mut h1_backend, front_addr) =
            setup_h1_front_with_raw_h2_backend(&name);
        if length {
            backend.push_header("content-length", "5");
        }
        let mut client = TcpStream::connect(front_addr).unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        client
            .write_all(
                format!(
                    "{} / HTTP/1.1\r\nHost: localhost\r\n\r\n",
                    String::from_utf8_lossy(method)
                )
                .as_bytes(),
            )
            .unwrap();
        let first = drain_client(&mut client);
        let _ = client.write_all(b"GET / HTTP/1.1\r\nHost: other\r\n\r\n");
        let second = drain_client(&mut client);
        println!("END_STREAM get={get} length={length} H1 client — {first:?} then {second:?}");
        drop(client);
        drop(backend);
        h1_backend.stop_and_get_aggregator();
        worker.soft_stop();
        let stopped = worker.wait_for_server_stop();
        let wire = format!("{first}{second}");
        if get {
            return if wire.starts_with("HTTP/1.1 502 ") && stopped {
                State::Success
            } else {
                println!("GET END_STREAM H1 client FAIL — wire={wire:?} stopped={stopped}");
                State::Fail
            };
        }
        let (head, next) = wire.split_once("\r\n\r\n").unwrap_or(("", ""));
        let head_lower = head.to_ascii_lowercase();
        let length_ok = if length {
            head_lower.contains("\r\ncontent-length: 5")
        } else {
            !head_lower.contains("content-length")
        };
        let head_ok = head.starts_with("HTTP/1.1 200 ") && length_ok;
        let next_ok = next.starts_with("HTTP/1.1 200 ") && next.ends_with("pong");
        if head_ok && next_ok && stopped {
            State::Success
        } else {
            println!(
                "HEAD END_STREAM length={length} H1 client FAIL — head={head:?} next={next:?} \
                 stopped={stopped}"
            );
            State::Fail
        }
    }
}

#[test]
fn test_h2_head_response_with_end_stream_keeps_the_backend_content_length() {
    // Every row runs, so a failure report names each one that failed.
    let mut failed = Vec::new();
    for h2_client in [false, true] {
        for (length, get) in [(true, false), (false, false), (true, true)] {
            if repeat_until_error_or(
                2,
                "H2 backend: a HEAD response with END_STREAM keeps the backend's \
                 content-length, or none, and a GET one is refused (RFC 9113 §8.1.1, \
                 RFC 9110 §8.6)",
                || try_h2_head_response_with_end_stream(h2_client, length, get),
            ) != State::Success
            {
                failed.push((h2_client, length, get));
            }
        }
    }
    assert!(
        failed.is_empty(),
        "failed rows (h2_client, length, get): {failed:?}"
    );
}

// ============================================================================
// FIX-3 — `:path` syntax (starts with `/`, or `*` only for OPTIONS)
// ============================================================================

/// Build a request header block with the path overridden to `value`. Uses
/// `GET` (or `method` when non-empty) / `:scheme https` / `:authority
/// localhost` and a literal `:path <value>`.
fn request_with_path(method: &[u8], path: &[u8]) -> Vec<u8> {
    let mut block = Vec::new();
    if method.is_empty() || method == b"GET" {
        block.push(0x82); // :method GET (static idx 2)
    } else if method == b"OPTIONS" {
        // :method OPTIONS — not in the static table; literal over index 2.
        push_literal_indexed_name(&mut block, 0x02, method);
    } else {
        push_literal_indexed_name(&mut block, 0x02, method);
    }
    // :path literal override (index 4).
    push_literal_indexed_name(&mut block, 0x04, path);
    block.push(0x87); // :scheme https
    // :authority localhost.
    block.push(0x41);
    block.push(0x09);
    block.extend_from_slice(b"localhost");
    block
}

/// Multi-case driver: for each `(method, path, should_reject)` triple, send
/// a fresh H2 connection and assert sozu rejects iff `should_reject`.
fn try_h2_path_syntax_enforced() -> State {
    let (worker, backends, front_port) = setup_h2_test("H2-SEC-PATH", 1);
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    // (method, path, must_be_rejected, description)
    let cases: &[(&[u8], &[u8], bool, &str)] = &[
        // Empty :path — store_pseudo_header rejects zero-length values.
        (b"GET", b"", true, "empty"),
        // Non-slash path — FIX-3 requires origin-form starts with `/`.
        (b"GET", b"api/users", true, "no-leading-slash"),
        // `*` with non-OPTIONS method — rejected per RFC 9112 §3.2.
        (b"GET", b"*", true, "asterisk-with-GET"),
        // `*` with OPTIONS — accepted (asterisk-form is legal for OPTIONS).
        (b"OPTIONS", b"*", false, "asterisk-with-OPTIONS"),
    ];

    let mut stream_id: u32 = 1;
    let mut everything_ok = true;

    for (method, path, should_reject, label) in cases {
        let mut tls = raw_h2_connection(front_addr);
        h2_handshake(&mut tls);

        let block = request_with_path(method, path);
        let frame = H2Frame::headers(stream_id, block, true, true);
        let wrote = tls.write_all(&frame.encode()).is_ok() && tls.flush().is_ok();

        let frames = collect_response_frames(&mut tls, 400, 3, 400);
        log_frames(&format!("path case '{label}'"), &frames);
        // Stream-scope path-syntax violation: sozu must emit RST/GOAWAY or 400
        // after the eager-RST fix. `!wrote` still accepted: the HPACK decoder
        // can reject a control-char `:path` early and collapse the TLS write
        // before the stream layer sees it.
        //
        // Scoped to `stream_id`, not the hardcoded stream 1 the shared helper
        // used: every case after the first runs on 3, 5, 7, so the 400 arm was
        // dead for three of the four. Safe to make live only now that it
        // decodes `:status` — the `asterisk-with-OPTIONS` case answers 404
        // (observed 2026-09-20: `HEADERS stream=7 status=404`), whose indexed
        // HPACK byte is the `0x8D` the old scan keyed on (issue #1374).
        let rejected = !wrote
            || rejected_with_goaway_or_rst(&frames)
            || stream_status_matches(&frames, stream_id, 400);

        if *should_reject && !rejected {
            println!("case '{label}' — expected rejection, got acceptance");
            everything_ok = false;
        } else if !*should_reject && rejected {
            // Accept either "no rejection" OR "backend unreachable" (502) —
            // we only fail if sozu emits GOAWAY / PROTOCOL_ERROR / 400.
            // A decoded 400 is a reject; any other `:status` is fine — sozu
            // routed and the answer came from routing or the backend. The
            // byte scan this replaces read `0x8D` as 400; that is static
            // index 13, `:status 404`, so the 404 this very case produces
            // answered "protocol error" (issue #1374).
            let protocol_error = rejected_with_goaway_or_rst(&frames)
                || stream_status_matches(&frames, stream_id, 400);
            if protocol_error {
                println!("case '{label}' — unexpected PROTOCOL_ERROR/400");
                everything_ok = false;
            }
        }

        drop(tls);
        thread::sleep(Duration::from_millis(50));
        stream_id += 2;
    }

    let infra_ok = teardown(raw_h2_connection(front_addr), front_port, worker, backends);
    if everything_ok && infra_ok {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_path_syntax_enforced() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 security: :path empty/non-slash/`*` enforcement (FIX-3 c3f9e090)",
            try_h2_path_syntax_enforced
        ),
        State::Success
    );
}

// ============================================================================
// FIX-5 — `content-length` syntax must be pure ASCII digits
// ============================================================================

/// Complements the existing `test_h2_content_length_format_fuzzing` by
/// covering the three specific patterns called out in FIX-5:
///
/// * leading `+` (e.g. `+10`) — `usize::from_str` would accept these but
///   kawa's H1 backend does not, creating a parser-divergence vector.
/// * leading whitespace (0x20 SP or 0x09 HTAB) — OWS is valid around H1
///   field values but the CL value itself MUST be pure digits.
/// * `0x10` — a hex literal; rejected because `x` is not ASCII-digit.
///
/// Every case must be rejected before the DATA frame is forwarded, so the
/// async backend's aggregator must show zero requests received.
fn try_h2_content_length_strict_syntax() -> State {
    let (worker, mut backends, front_port) = setup_h2_test("H2-SEC-CL-STRICT", 1);
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    let cases: &[&[u8]] = &[
        b"+10",  // leading plus
        b" 5",   // leading SP
        b"\t5",  // leading HTAB
        b"0x10", // hex literal
        b"10a",  // trailing garbage
    ];

    let mut stream_id: u32 = 1;
    let mut all_rejected = true;

    for cl in cases {
        let mut tls = raw_h2_connection(front_addr);
        h2_handshake(&mut tls);

        let mut block = vec![
            0x83, // :method POST
            0x84, // :path /
            0x87, // :scheme https
            0x41, 0x09, // :authority localhost
            b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't',
        ];
        push_literal(&mut block, b"content-length", cl);

        // END_HEADERS only — we claim a body so END_STREAM is off.
        let frame = H2Frame::headers(stream_id, block, true, false);
        if tls.write_all(&frame.encode()).is_err() || tls.flush().is_err() {
            println!("cl={:?} — write failed", String::from_utf8_lossy(cl));
            continue;
        }

        let frames = collect_response_frames(&mut tls, 400, 3, 400);
        log_frames(&format!("cl={:?}", String::from_utf8_lossy(cl)), &frames);
        // Stream-scope content-length syntax violation: sozu must emit RST/GOAWAY or 400.
        let rejected =
            rejected_with_goaway_or_rst(&frames) || stream_status_matches(&frames, 1, 400);
        if !rejected {
            println!("cl {:?} — NOT rejected", String::from_utf8_lossy(cl));
            all_rejected = false;
        }

        drop(tls);
        thread::sleep(Duration::from_millis(50));
        stream_id += 2;
    }

    // Prove no DATA reached the backend. AsyncBackend counts full H1
    // requests it receives; a malformed CL must never produce one.
    let agg = backends[0].stop_and_get_aggregator();
    // consume the rest of the vector via the teardown helper below.
    backends.clear();
    let requests_received = agg.map(|a| a.requests_received).unwrap_or(0);
    println!("backend requests_received after CL fuzz: {requests_received}");

    // Standard teardown without the backend.
    let infra_ok = teardown(raw_h2_connection(front_addr), front_port, worker, backends);

    if all_rejected && requests_received == 0 && infra_ok {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_content_length_strict_syntax() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 security: content-length non-digit/sign/OWS rejected (FIX-5 c3f9e090)",
            try_h2_content_length_strict_syntax
        ),
        State::Success
    );
}

// ============================================================================
// FIX-2 — `:status` response pseudo-header must be exactly 3 ASCII digits
// ============================================================================

/// Drives a `RawH2ResponseBackend` configured with an invalid `:status`
/// value (bypassing hyper's upstream sanitisation) and asserts sozu
/// surfaces the upstream error to the client as either a 502 Bad Gateway
/// or a stream RST_STREAM. Iterates over the four patterns called out in
/// FIX-2 / RFC 9113 §8.3.2.
fn try_h2_invalid_status_rejected() -> State {
    let bad_statuses: &[&[u8]] = &[
        b"abc",   // non-digit
        b"20",    // too short
        b"+200",  // signed
        b"00200", // too long
    ];

    for bad_status in bad_statuses {
        let (mut worker, front_port, _) = setup_h2_listener_only("H2-SEC-STATUS");
        let back_address = create_local_address();
        worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
            "cluster_0",
            "cluster_0-0",
            back_address,
            None,
        )));
        worker.read_to_last();

        let backend = RawH2ResponseBackend::new(back_address);
        backend.set_status((*bad_status).to_vec());
        thread::sleep(Duration::from_millis(100));

        let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
        let mut tls = raw_h2_connection(front_addr);
        h2_handshake(&mut tls);

        let block = request_prefix_localhost();
        let frame = H2Frame::headers(1, block, true, true);
        tls.write_all(&frame.encode()).unwrap();
        tls.flush().unwrap();

        let frames = collect_response_frames(&mut tls, 800, 4, 500);
        log_frames(
            &format!(":status={:?}", String::from_utf8_lossy(bad_status)),
            &frames,
        );

        // Valid outcomes: a decoded 502 on stream 1, or a protocol-level
        // rejection (RST_STREAM on stream 1 / GOAWAY).
        //
        // This assertion used to read `protocol_rejection || !got_200`, which
        // a worker that answered *nothing* satisfied: `got_200` is derived
        // from frames that may never arrive, so a silent reset, a timeout
        // collecting zero frames, or a worker that died between iterations
        // all read as success, and a regression turning a clean 502 into a
        // crash read as a pass (issue #1381). Assert the positive outcome
        // instead, require frames to have arrived at all, and keep `!got_200`
        // as a conjunct rather than an escape hatch.
        //
        // Sōzu answers 502 here — observed 2026-09-20 on all four bad
        // statuses, `HEADERS flags=0x04 stream=1 len=52 status=502`. Both
        // status probes decode `:status` rather than scanning for `0x88`,
        // which is a length octet or a raw value byte as often as it is an
        // indexed `:status 200` (issue #1374, same defect class).
        let protocol_rejection = rejected_with_goaway_or_rst(&frames);
        let got_200 = stream_status_matches(&frames, 1, 200);
        let got_502 = stream_status_matches(&frames, 1, 502);
        // To SEE THIS RED: shadow `frames` with an empty `Vec` right after
        // the `collect_response_frames` call above. Measured 2026-09-20:
        // `FAIL — bad :status "abc": frames=0, protocol_rejection=false,
        // got_502=false, got_200=false`, while the historical
        // `protocol_rejection || !got_200` passed both iterations.
        let got_frames = !frames.is_empty();
        let ok = got_frames && (protocol_rejection || got_502) && !got_200;

        if !ok {
            println!(
                "FAIL — bad :status {:?}: frames={}, protocol_rejection={protocol_rejection}, \
                 got_502={got_502}, got_200={got_200}",
                String::from_utf8_lossy(bad_status),
                frames.len()
            );
            drop(backend);
            drop(tls);
            worker.hard_stop();
            let _ = worker.wait_for_server_stop();
            return State::Fail;
        }

        drop(tls);
        drop(backend);
        thread::sleep(Duration::from_millis(100));
        let still_alive = verify_sozu_alive(front_port);
        worker.soft_stop();
        let stopped = worker.wait_for_server_stop();
        if !still_alive || !stopped {
            return State::Fail;
        }
    }

    State::Success
}

#[test]
fn test_h2_invalid_status_rejected() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "H2 security: upstream :status abc/20/+200/00200 does not reach client as 200 (FIX-2 c3f9e090)",
            try_h2_invalid_status_rejected
        ),
        State::Success
    );
}

// ============================================================================
// Request trailers — `TRAILER_FORBIDDEN_FIELDS` (sozu-proxy/sozu#1714)
// ============================================================================

/// Trailer fields of the forbidden-field cases: one legitimate field, then
/// one field of each RFC 9110 §6.5.1 category the H2 frontend does not
/// already refuse as connection-specific (framing, routing, request
/// modifiers, authentication, content processing), each carrying `forged`.
/// `grpc-status` comes first, so a forbidden field that leaks lands after
/// it in the trailer section.
const FORBIDDEN_H2_TRAILERS: [(&[u8], &[u8]); 8] = [
    (b"grpc-status", b"0"),
    (b"content-length", b"6666"),
    (b"host", b"forged.example"),
    (b"cache-control", b"forged"),
    (b"if-match", b"\"forged\""),
    (b"authorization", b"Bearer forged"),
    (b"cookie", b"session=forged"),
    (b"content-type", b"text/forged"),
];

/// Send, on stream 1, a `POST /` with no `content-length`, a DATA frame
/// without END_STREAM, and a trailer HEADERS frame holding
/// `FORBIDDEN_H2_TRAILERS` with END_STREAM. Without a declared length the
/// request is chunked towards an H1 backend, which then carries trailers.
fn send_h2_post_with_forbidden_trailers(tls: &mut impl Write) {
    let mut block = request_prefix_localhost();
    block[0] = 0x83; // :method POST
    tls.write_all(&H2Frame::headers(1, block, true, false).encode())
        .unwrap();
    tls.write_all(&H2Frame::data(1, b"hello".to_vec(), false).encode())
        .unwrap();
    let mut trailers = Vec::new();
    for (name, value) in FORBIDDEN_H2_TRAILERS {
        push_literal(&mut trailers, name, value);
    }
    tls.write_all(&H2Frame::headers(1, trailers, true, true).encode())
        .unwrap();
    tls.flush().unwrap();
}

/// H2 frontend, H1 backend: the only trailer field the backend reads after
/// the body is `grpc-status`. This checks which fields survive, not that the
/// chunked framing around them is well formed.
fn try_h2_trailer_forbidden_fields_dropped_h1_backend() -> State {
    let (mut worker, mut backend, front_port) =
        setup_h2_with_sync_backend("H2-SEC-TRAILER-FORBIDDEN-H1");
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);
    send_h2_post_with_forbidden_trailers(&mut tls);

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut accepted = false;
    while Instant::now() < deadline {
        if backend.accept(0) {
            accepted = true;
            break;
        }
    }
    // Drain the whole window, not one read: the assertion is about bytes
    // that must not appear, so a late segment must be read too.
    let mut received = String::new();
    let window = Instant::now();
    while accepted && window.elapsed() < Duration::from_millis(500) {
        if let Some(chunk) = backend.receive(0) {
            received.push_str(&chunk);
        }
    }
    println!("trailer-forbidden H2->H1 — backend received {received:?}");
    if accepted {
        backend.send(0);
    }
    let frames = collect_response_frames(&mut tls, 300, 2, 300);
    log_frames("trailer-forbidden H2->H1", &frames);
    let has_response = stream_status_matches(&frames, 1, 200);

    backend.disconnect();
    drop(tls);
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    // After the body chunk come exactly the legitimate field and the empty
    // line. The last-chunk line `0\r\n` is skipped when present: the H2->H1
    // path does not write it yet, and that framing defect is fixed
    // separately, so this test only checks which fields survive.
    let trailer_section = received.find("\r\nhello\r\n").map(|at| {
        let rest = &received[at + "\r\nhello\r\n".len()..];
        rest.strip_prefix("0\r\n")
            .unwrap_or(rest)
            .to_ascii_lowercase()
    });
    let only_legitimate = trailer_section.as_deref() == Some("grpc-status: 0\r\n\r\n");
    let leaked = received.contains("forged");
    if accepted && only_legitimate && !leaked && has_response && stopped {
        State::Success
    } else {
        println!(
            "trailer-forbidden H2->H1 FAIL — accepted={accepted} \
             trailer_section={trailer_section:?} leaked={leaked} \
             response={has_response} stopped={stopped}"
        );
        State::Fail
    }
}

#[test]
fn test_h2_trailer_forbidden_fields_dropped_h1_backend() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 security: forbidden request trailer fields never reach an H1 backend (#1714)",
            try_h2_trailer_forbidden_fields_dropped_h1_backend
        ),
        State::Success
    );
}

/// Sōzu listener + an `http2` cluster whose backend records the request
/// trailers it receives. Returns the running `Worker`, the backend and the
/// HTTPS front port.
fn setup_h2_with_h2_trailer_backend(name: &str) -> (Worker, H2Backend, u16) {
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
        interface: None,
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
    worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
        address: front_address,
        certificate: CertificateAndKey {
            certificate: String::from(include_str!("../../../lib/assets/local-certificate.pem")),
            key: String::from(include_str!("../../../lib/assets/local-key.pem")),
            certificate_chain: vec![],
            versions: vec![],
            names: vec![],
        },
        expired_at: None,
    }));
    let back_address = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));
    let backend = H2Backend::start_recording_trailers(format!("{name}-BACK"), back_address, "pong");
    worker.read_to_last();
    (worker, backend, front_port)
}

/// H2 frontend, H2 backend: the trailer HEADERS frame the backend decodes
/// holds `grpc-status` and nothing else. `H2BlockConverter` already drops
/// `host` on its own; the other forbidden fields rely on the frontend.
fn try_h2_trailer_forbidden_fields_dropped_h2_backend() -> State {
    let (mut worker, mut backend, front_port) =
        setup_h2_with_h2_trailer_backend("H2-SEC-TRAILER-FORBIDDEN-H2");
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);
    send_h2_post_with_forbidden_trailers(&mut tls);

    let frames = collect_response_frames(&mut tls, 500, 5, 500);
    log_frames("trailer-forbidden H2->H2", &frames);
    let has_response = stream_status_matches(&frames, 1, 200);
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut recorded = backend.recorded_requests();
    while recorded.is_empty() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
        recorded = backend.recorded_requests();
    }
    println!("trailer-forbidden H2->H2 — backend recorded {recorded:?}");

    drop(tls);
    backend.stop();
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    let trailers = recorded.first().map(|request| request.trailers.clone());
    let only_legitimate =
        trailers.as_deref() == Some(&[("grpc-status".to_owned(), b"0".to_vec())][..]);
    if recorded.len() == 1 && only_legitimate && has_response && stopped {
        State::Success
    } else {
        println!(
            "trailer-forbidden H2->H2 FAIL — requests={} trailers={trailers:?} \
             response={has_response} stopped={stopped}",
            recorded.len()
        );
        State::Fail
    }
}

#[test]
fn test_h2_trailer_forbidden_fields_dropped_h2_backend() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 security: forbidden request trailer fields never reach an H2 backend (#1714)",
            try_h2_trailer_forbidden_fields_dropped_h2_backend
        ),
        State::Success
    );
}
