//! Adversarial end-to-end tests targeting the H2 frame parser hardenings
//! landed in the Pass 1 security audit. Each test exercises a specific
//! wire-layer invariant and asserts sozu's observable reaction (GOAWAY
//! error code, stream acceptance, or serializer output) rather than
//! re-running the unit-level checks already covered in `parser.rs` /
//! `serializer.rs`.
//!
//! Covered recipes (Section 2, group 8D of `e2e-recipe.md`):
//!
//!   * PADDED+PRIORITY HEADERS underflow regression (commit `1a9cf071`).
//!   * GOAWAY with `payload_len < 8` rejected with FRAME_SIZE_ERROR
//!     (commit `5261dbe5`).
//!   * `strip_padding` off-by-one (commit `5261dbe5`): valid
//!     `pad_length == payload_len - 1` accepted, overflowing variant
//!     rejected.

use std::{io::Write, net::SocketAddr};

use super::h2_utils::{
    H2_ERROR_FRAME_SIZE_ERROR, H2_ERROR_PROTOCOL_ERROR, H2_FRAME_DATA, H2_FRAME_GOAWAY, H2Frame,
    collect_response_frames, contains_goaway, contains_goaway_with_error,
    contains_headers_response, contains_rst_stream, goaway_error_code, h2_handshake, log_frames,
    raw_h2_connection, rejected_with_goaway_or_rst, setup_h2_test, teardown,
};
use crate::tests::{State, repeat_until_error_or};

// ============================================================================
// Test 1: PADDED+PRIORITY HEADERS underflow regression
// ============================================================================

/// Replay the exact 16-byte crash input saved at
/// `fuzz/corpus/fuzz_frame_parser/crash-d7a34a0d-...` which caused an
/// unsigned-subtraction panic in `unpad` before commit `1a9cf071`.
///
/// The payload is a HEADERS frame with flags=0xff (all set, including
/// PADDED and PRIORITY) on a stream whose high bit is set. The declared
/// frame length (6) leaves no room for the 5-byte PRIORITY block plus
/// the pad-length byte, which is exactly the condition that triggered
/// the underflow.
///
/// Post-fix sozu responds with a GOAWAY carrying either FRAME_SIZE_ERROR
/// or PROTOCOL_ERROR, without panicking. The exact code is left flexible
/// because different internal branches reject the malformed frame at
/// different stages of the parser (length check vs. unpad check).
fn try_h2_parser_padded_priority_underflow_regression() -> State {
    let (worker, backends, front_port) = setup_h2_test("H2-PARSER-UNDERFLOW", 1);
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);

    // Exact bytes from the fuzz corpus artifact (16 bytes).
    let crash: [u8; 16] = [
        0x00, 0x00, 0x06, 0x01, 0xff, 0xff, 0xff, 0x00, 0x00, 0x03, 0x00, 0x64, 0x6d, 0x6d, 0x6d,
        0x6d,
    ];
    let write_ok = tls.write_all(&crash).is_ok() && tls.flush().is_ok();

    let frames = collect_response_frames(&mut tls, 500, 5, 500);
    log_frames("PADDED+PRIORITY underflow regression", &frames);

    let err = goaway_error_code(&frames);
    let accepted_codes = [H2_ERROR_FRAME_SIZE_ERROR, H2_ERROR_PROTOCOL_ERROR];
    let got_expected_goaway = err.is_some_and(|e| accepted_codes.contains(&e));

    let rejected = got_expected_goaway || rejected_with_goaway_or_rst(&frames) || !write_ok;

    // Infra check: sozu must not have panicked — `teardown` re-connects
    // via TCP and soft-stops the worker.
    let infra_ok = teardown(tls, front_port, worker, backends);
    if rejected && infra_ok {
        State::Success
    } else {
        println!(
            "PADDED+PRIORITY underflow - FAIL: got_expected_goaway={got_expected_goaway} \
             err={err:?} rejected={rejected} infra_ok={infra_ok}"
        );
        State::Fail
    }
}

#[test]
fn e2e_h2_parser_reject_padded_priority_underflow() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2 parser: PADDED+PRIORITY HEADERS underflow regression (fuzz d7a34a0d)",
            try_h2_parser_padded_priority_underflow_regression,
        ),
        State::Success,
    );
}

// ============================================================================
// Test 2: GOAWAY with < 8-byte payload → FRAME_SIZE_ERROR
// ============================================================================

/// RFC 9113 §6.8 mandates at least 8 payload bytes for GOAWAY
/// (4 last-stream-id + 4 error-code). Commit `5261dbe5` added the
/// length check; this test sends a 7-byte GOAWAY and asserts sozu
/// responds with its own GOAWAY(FRAME_SIZE_ERROR) and drops the
/// connection.
fn try_h2_parser_reject_short_goaway() -> State {
    let (worker, backends, front_port) = setup_h2_test("H2-PARSER-SHORT-GOAWAY", 1);
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);

    // 7-byte GOAWAY payload: one byte short of the mandatory 8.
    let short_goaway = H2Frame::new(H2_FRAME_GOAWAY, 0, 0, vec![0u8; 7]);
    let write_ok = tls.write_all(&short_goaway.encode()).is_ok() && tls.flush().is_ok();

    let frames = collect_response_frames(&mut tls, 500, 5, 500);
    log_frames("Short GOAWAY", &frames);

    let got_frame_size_goaway = contains_goaway_with_error(&frames, H2_ERROR_FRAME_SIZE_ERROR);
    println!("Short GOAWAY - GOAWAY(FRAME_SIZE_ERROR): {got_frame_size_goaway}");

    let infra_ok = teardown(tls, front_port, worker, backends);
    if infra_ok && (got_frame_size_goaway || !write_ok) {
        State::Success
    } else {
        println!(
            "Short GOAWAY - FAIL: got_frame_size_goaway={got_frame_size_goaway} \
             write_ok={write_ok} infra_ok={infra_ok}"
        );
        State::Fail
    }
}

#[test]
fn e2e_h2_parser_reject_short_goaway() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2 parser: GOAWAY payload < 8 bytes rejected with FRAME_SIZE_ERROR (RFC 9113 \u{00a7}6.8)",
            try_h2_parser_reject_short_goaway,
        ),
        State::Success,
    );
}

// ============================================================================
// Helper constants and header-block builder shared by the padding tests below
// ============================================================================

/// Minimal header block for `GET / HTTP/2` against `localhost`, HPACK-encoded
/// via static-table indices.
fn minimal_get_headers_block() -> Vec<u8> {
    vec![
        0x82, // :method GET       (static index 2)
        0x84, // :path /           (static index 4)
        0x87, // :scheme https     (static index 7)
        0x41, 0x09, // :authority (name idx 1), value length 9
        b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't',
    ]
}

/// H2 flag constant for PADDED (DATA/HEADERS). Not exported by `h2_utils`.
const H2_FLAG_PADDED: u8 = 0x08;

// ============================================================================
// Test 3a: strip_padding — pad_length == payload_len - 1 accepted (all-padding)
// ============================================================================

/// Edge case from RFC 9113 §6.1: a DATA frame whose entire post-pad-length
/// payload is padding (content length zero) is **valid** — `pad_length`
/// must be **less than** the frame payload length, and
/// `pad_length == payload_len - 1` satisfies that.
///
/// Before commit `5261dbe5`, `strip_padding` used `<=` and rejected this
/// case with PROTOCOL_ERROR; after the fix it is accepted.
///
/// Flow: POST / with Content-Length:0, the padded DATA frame carries zero
/// content bytes, END_STREAM=true. Sozu must forward the empty POST and
/// return a 2xx response (backend answers with `pong0`).
fn try_h2_parser_accept_all_padding_data() -> State {
    let (worker, backends, front_port) = setup_h2_test("H2-PARSER-ALL-PADDING", 1);
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);

    // POST / with content-length: 0 + localhost :authority.
    let mut header_block = vec![
        0x83, // :method POST  (static idx 3)
        0x84, // :path /       (static idx 4)
        0x87, // :scheme https (static idx 7)
        0x41, 0x09, // :authority localhost
        b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't',
    ];
    header_block.push(0x00); // literal, new name
    header_block.push(0x0e); // name length = 14
    header_block.extend_from_slice(b"content-length");
    header_block.push(0x01); // value length = 1
    header_block.extend_from_slice(b"0");

    let headers = H2Frame::headers(1, header_block, true, false);
    tls.write_all(&headers.encode()).unwrap();

    // Padded DATA frame: payload = [pad_length=5, 5 × 0x00], total 6 bytes.
    // pad_length == payload_len - 1 == 5 → all padding, zero content. Must be
    // accepted by sozu per RFC 9113 §6.1.
    let payload: Vec<u8> = std::iter::once(5u8)
        .chain(std::iter::repeat_n(0u8, 5))
        .collect();
    assert_eq!(payload.len(), 6);
    let data = H2Frame::new(
        H2_FRAME_DATA,
        H2_FLAG_PADDED | 0x1, /* END_STREAM */
        1,
        payload,
    );
    tls.write_all(&data.encode()).unwrap();
    tls.flush().unwrap();

    let frames = collect_response_frames(&mut tls, 500, 6, 500);
    log_frames("All-padding DATA accepted", &frames);

    let got_response = contains_headers_response(&frames);
    let got_rejection = contains_goaway(&frames) || contains_rst_stream(&frames);

    let infra_ok = teardown(tls, front_port, worker, backends);
    if infra_ok && got_response && !got_rejection {
        State::Success
    } else {
        println!(
            "All-padding DATA - FAIL: got_response={got_response} \
             got_rejection={got_rejection} infra_ok={infra_ok}"
        );
        State::Fail
    }
}

#[test]
fn e2e_h2_parser_accept_all_padding_data() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2 parser: all-padding DATA accepted (pad_length == payload_len - 1)",
            try_h2_parser_accept_all_padding_data,
        ),
        State::Success,
    );
}

// ============================================================================
// Test 3b: strip_padding — pad_length == payload_len rejected
// ============================================================================

/// Companion of test 3a: `pad_length >= payload_len` is the overflow path
/// that would leave negative content bytes. Sozu must reject with
/// PROTOCOL_ERROR (stream or connection scope).
fn try_h2_parser_reject_overflowing_padding() -> State {
    let (worker, backends, front_port) = setup_h2_test("H2-PARSER-OVERFLOW-PAD", 1);
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);

    let headers = H2Frame::headers(1, minimal_get_headers_block(), true, false);
    tls.write_all(&headers.encode()).unwrap();

    // pad_length = payload_len (5) → reserves 5 padding bytes from a 5-byte
    // payload, no room for the pad-length byte itself → overflow.
    let payload = vec![5u8, 0u8, 0u8, 0u8, 0u8];
    let data = H2Frame::new(H2_FRAME_DATA, H2_FLAG_PADDED | 0x1, 1, payload);
    let write_ok = tls.write_all(&data.encode()).is_ok() && tls.flush().is_ok();

    let frames = collect_response_frames(&mut tls, 500, 5, 500);
    log_frames("Overflowing pad_length rejected", &frames);

    let rejected = rejected_with_goaway_or_rst(&frames) || !write_ok;
    let infra_ok = teardown(tls, front_port, worker, backends);
    if infra_ok && rejected {
        State::Success
    } else {
        println!("Overflowing pad_length - FAIL: rejected={rejected} infra_ok={infra_ok}");
        State::Fail
    }
}

#[test]
fn e2e_h2_parser_reject_overflowing_padding() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2 parser: pad_length >= payload_len rejected (strip_padding overflow)",
            try_h2_parser_reject_overflowing_padding,
        ),
        State::Success,
    );
}
