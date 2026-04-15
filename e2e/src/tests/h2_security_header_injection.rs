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
//!
//! Raw-byte H2 is used for every request path: hyper refuses CRLF in values
//! and collapses duplicate pseudo-headers at its HPACK encoder, so the only
//! way to exercise sozu's filter is to build the header block by hand.

use std::{io::Write, net::SocketAddr, thread, time::Duration};

use sozu_command_lib::proto::command::{AddBackend, RequestHttpFrontend, request::RequestType};

use super::h2_utils::{
    H2_FRAME_HEADERS, H2Frame, collect_response_frames, contains_headers_response, h2_handshake,
    log_frames, raw_h2_connection, rejected_with_goaway_or_rst, setup_h2_listener_only,
    setup_h2_test, teardown, verify_sozu_alive,
};
use crate::{
    mock::{
        aggregator::SimpleAggregator, async_backend::BackendHandle as AsyncBackend,
        raw_h2_response_backend::RawH2ResponseBackend, sync_backend::Backend as SyncBackend,
    },
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

/// Check whether a HEADERS response on stream 1 carries `:status 400` using
/// the HPACK static-table encoding (`0x8D` = static index 13 = `:status 400`).
fn contains_400_response(frames: &[(u8, u8, u32, Vec<u8>)]) -> bool {
    frames.iter().any(|(ft, _fl, sid, payload)| {
        *ft == H2_FRAME_HEADERS && *sid == 1 && payload.contains(&0x8D)
    })
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

    let rejected = rejected_with_goaway_or_rst(&frames) || contains_400_response(&frames);
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

    let rejected = rejected_with_goaway_or_rst(&frames) || contains_400_response(&frames);
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
