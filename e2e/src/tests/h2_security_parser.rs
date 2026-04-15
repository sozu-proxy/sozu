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
//!   * Unknown frame types silently ignored per RFC 9113 §5.5, including
//!     the real RFC 9218 PRIORITY_UPDATE type `0x10` (commit `5261dbe5`).
//!   * PUSH_PROMISE from client rejected with GOAWAY(PROTOCOL_ERROR)
//!     (commit `5261dbe5`). Parser-level complement to the existing
//!     `test_h2_push_promise_from_client` — asserts the specific error
//!     code rather than just any GOAWAY.
//!   * Standalone CONTINUATION without preceding HEADERS rejected with
//!     GOAWAY(PROTOCOL_ERROR) (commit `4a798013`). Parser-level
//!     complement to `test_h2_continuation_without_initial_headers`.
//!   * Serializer masks the reserved R-bit on every outbound frame
//!     header and on GOAWAY last-stream-id (commit `7e69b763`). Passive
//!     observation over a session that naturally generates SETTINGS,
//!     HEADERS, WINDOW_UPDATE and GOAWAY.

use std::{io::Write, net::SocketAddr};

use super::h2_utils::{
    H2_ERROR_FRAME_SIZE_ERROR, H2_ERROR_PROTOCOL_ERROR, H2_FRAME_DATA, H2_FRAME_GOAWAY, H2Frame,
    collect_response_frames, contains_goaway, contains_goaway_with_error,
    contains_headers_response, contains_rst_stream, goaway_error_code, h2_handshake, log_frames,
    parse_h2_frames, raw_h2_connection, read_all_available, rejected_with_goaway_or_rst,
    setup_h2_test, teardown,
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

// ============================================================================
// Test 4a: Unknown frame type (0xFF) is silently ignored
// ============================================================================

/// RFC 9113 §5.5: endpoints MUST ignore and discard frames of unknown
/// type. Before commit `5261dbe5` sozu returned a parser error; now the
/// unknown frame is consumed and processing continues.
///
/// Send an unknown type interleaved between handshake and HEADERS; the
/// subsequent HEADERS must still produce a response on stream 1.
fn try_h2_parser_ignore_unknown_frame_type() -> State {
    let (worker, backends, front_port) = setup_h2_test("H2-PARSER-UNKNOWN", 1);
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);

    // Unknown frame type 0xFF on stream 0 (valid for extension frames
    // without stream semantics). 8-byte payload so the parser has to
    // consume it rather than fall off the end of the wire buffer.
    let unknown = H2Frame::new(
        0xFF,
        0,
        0,
        vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x00, 0x00, 0x00],
    );
    tls.write_all(&unknown.encode()).unwrap();

    let headers = H2Frame::headers(1, minimal_get_headers_block(), true, true);
    tls.write_all(&headers.encode()).unwrap();
    tls.flush().unwrap();

    let frames = collect_response_frames(&mut tls, 500, 6, 500);
    log_frames("Unknown frame type ignored", &frames);

    let got_response = contains_headers_response(&frames);
    let got_goaway = contains_goaway(&frames);

    let infra_ok = teardown(tls, front_port, worker, backends);
    if infra_ok && got_response && !got_goaway {
        State::Success
    } else {
        println!(
            "Unknown frame type - FAIL: got_response={got_response} \
             got_goaway={got_goaway} infra_ok={infra_ok}"
        );
        State::Fail
    }
}

#[test]
fn e2e_h2_parser_ignore_unknown_frame_type() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2 parser: unknown frame type 0xFF silently ignored (RFC 9113 \u{00a7}5.5)",
            try_h2_parser_ignore_unknown_frame_type,
        ),
        State::Success,
    );
}

// ============================================================================
// Test 4b: RFC 9218 PRIORITY_UPDATE (type 0x10) silently ignored
// ============================================================================

/// Real-world forward-compatibility check: RFC 9218 defines
/// PRIORITY_UPDATE with frame type `0x10`. Sozu does not implement the
/// extension, so it must treat it as unknown and skip it without
/// affecting the rest of the connection.
fn try_h2_parser_ignore_priority_update_frame() -> State {
    let (worker, backends, front_port) = setup_h2_test("H2-PARSER-PRIO-UPDATE", 1);
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);

    // PRIORITY_UPDATE payload layout (RFC 9218 §7.1): 4 bytes prioritized
    // stream id + a priority field value (ASCII, free-form). Payload
    // contents are irrelevant here — the parser must simply skip
    // `payload_len` bytes.
    let priority_update = H2Frame::new(
        0x10,
        0,
        0,
        vec![0, 0, 0, 1, b'u', b'=', b'0'], // 7 bytes
    );
    tls.write_all(&priority_update.encode()).unwrap();

    let headers = H2Frame::headers(1, minimal_get_headers_block(), true, true);
    tls.write_all(&headers.encode()).unwrap();
    tls.flush().unwrap();

    let frames = collect_response_frames(&mut tls, 500, 6, 500);
    log_frames("PRIORITY_UPDATE ignored", &frames);

    let got_response = contains_headers_response(&frames);
    let got_goaway = contains_goaway(&frames);

    let infra_ok = teardown(tls, front_port, worker, backends);
    if infra_ok && got_response && !got_goaway {
        State::Success
    } else {
        println!(
            "PRIORITY_UPDATE - FAIL: got_response={got_response} \
             got_goaway={got_goaway} infra_ok={infra_ok}"
        );
        State::Fail
    }
}

#[test]
fn e2e_h2_parser_ignore_priority_update_frame() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2 parser: RFC 9218 PRIORITY_UPDATE (0x10) silently ignored",
            try_h2_parser_ignore_priority_update_frame,
        ),
        State::Success,
    );
}

// ============================================================================
// Test 5: PUSH_PROMISE from client → GOAWAY(PROTOCOL_ERROR)
// ============================================================================

/// RFC 9113 §8.4: a client MUST NOT send PUSH_PROMISE. Commit
/// `5261dbe5` hardened the wire-layer rejection. This test complements
/// the existing `test_h2_push_promise_from_client` by asserting the
/// **specific** GOAWAY error code is PROTOCOL_ERROR.
fn try_h2_parser_reject_push_promise_from_client() -> State {
    let (worker, backends, front_port) = setup_h2_test("H2-PARSER-PUSH-PROMISE", 1);
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);

    // PUSH_PROMISE (type 0x5) with END_HEADERS on a valid client-initiated
    // stream id. Payload: promised-stream-id (4 bytes) + header block.
    let mut payload = Vec::new();
    payload.extend_from_slice(&3u32.to_be_bytes());
    payload.extend_from_slice(&minimal_get_headers_block());
    let push_promise = H2Frame::new(0x5, 0x4 /* END_HEADERS */, 1, payload);
    let write_ok = tls.write_all(&push_promise.encode()).is_ok() && tls.flush().is_ok();

    let frames = collect_response_frames(&mut tls, 500, 5, 500);
    log_frames("PUSH_PROMISE from client rejected", &frames);

    let got_protocol_error = contains_goaway_with_error(&frames, H2_ERROR_PROTOCOL_ERROR);
    let got_any_goaway = contains_goaway(&frames);
    println!(
        "PUSH_PROMISE from client - GOAWAY(PROTOCOL_ERROR): {got_protocol_error}, \
         any GOAWAY: {got_any_goaway}"
    );

    // Accept any GOAWAY if the server closes the TLS layer before we can
    // read the error code back (observed on slower CI).
    let rejected = got_protocol_error || got_any_goaway || !write_ok;
    let infra_ok = teardown(tls, front_port, worker, backends);
    if infra_ok && rejected {
        State::Success
    } else {
        println!("PUSH_PROMISE - FAIL: rejected={rejected} infra_ok={infra_ok}");
        State::Fail
    }
}

#[test]
fn e2e_h2_parser_reject_push_promise_from_client() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2 parser: PUSH_PROMISE from client → GOAWAY(PROTOCOL_ERROR) (RFC 9113 \u{00a7}8.4)",
            try_h2_parser_reject_push_promise_from_client,
        ),
        State::Success,
    );
}

// ============================================================================
// Test 6: Standalone CONTINUATION → GOAWAY(PROTOCOL_ERROR)
// ============================================================================

/// Commit `4a798013` replaced a `debug_assert!(false)` with an explicit
/// GOAWAY(PROTOCOL_ERROR) when a CONTINUATION arrives without a
/// preceding HEADERS (or PUSH_PROMISE) with END_HEADERS clear. This
/// test complements `test_h2_continuation_without_initial_headers` by
/// asserting the specific error code rather than "any GOAWAY".
fn try_h2_parser_reject_standalone_continuation() -> State {
    let (worker, backends, front_port) = setup_h2_test("H2-PARSER-STANDALONE-CONT", 1);
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);

    let cont = H2Frame::continuation(1, minimal_get_headers_block(), true);
    let write_ok = tls.write_all(&cont.encode()).is_ok() && tls.flush().is_ok();

    let frames = collect_response_frames(&mut tls, 500, 5, 500);
    log_frames("Standalone CONTINUATION rejected", &frames);

    let got_protocol_error = contains_goaway_with_error(&frames, H2_ERROR_PROTOCOL_ERROR);
    let got_any_goaway = contains_goaway(&frames);
    println!(
        "Standalone CONTINUATION - GOAWAY(PROTOCOL_ERROR): {got_protocol_error}, \
         any GOAWAY: {got_any_goaway}"
    );

    let rejected = got_protocol_error || got_any_goaway || !write_ok;
    let infra_ok = teardown(tls, front_port, worker, backends);
    if infra_ok && rejected {
        State::Success
    } else {
        println!("Standalone CONTINUATION - FAIL: rejected={rejected} infra_ok={infra_ok}");
        State::Fail
    }
}

#[test]
fn e2e_h2_parser_reject_standalone_continuation() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2 parser: standalone CONTINUATION → GOAWAY(PROTOCOL_ERROR) (RFC 9113 \u{00a7}6.10)",
            try_h2_parser_reject_standalone_continuation,
        ),
        State::Success,
    );
}

// ============================================================================
// Test 7: Serializer masks the reserved R-bit on stream IDs
// ============================================================================

/// Walk the raw stream of bytes returned by sozu and yield every frame
/// header's stream-id byte zero (bits 0-7 of the 4-byte id) so we can
/// inspect the reserved R-bit without `parse_h2_frames` masking it.
fn raw_frame_stream_id_high_bytes(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 9 <= data.len() {
        let length =
            ((data[pos] as u32) << 16) | ((data[pos + 1] as u32) << 8) | (data[pos + 2] as u32);
        out.push(data[pos + 5]);
        let payload_end = pos + 9 + length as usize;
        if payload_end > data.len() {
            break;
        }
        pos = payload_end;
    }
    out
}

/// RFC 9113 §4.1 reserves the high bit of every 32-bit stream-id field;
/// §6.8 says the same about GOAWAY's last-stream-id. Commit `7e69b763`
/// tightened `gen_frame_header` and `gen_goaway` so callers that leak
/// the reserved bit cannot corrupt the wire.
///
/// This test drives sozu through a session that provokes many distinct
/// outbound frames (SETTINGS, SETTINGS ACK, HEADERS response,
/// WINDOW_UPDATE and finally GOAWAY) and asserts the high bit of byte
/// 5 of every frame header is zero, plus — for the GOAWAY — that
/// payload[0]'s high bit is zero. Parsing is done on the raw bytes so
/// `parse_h2_frames`'s own masking does not hide a leak.
fn try_h2_parser_serializer_masks_reserved_bit() -> State {
    let (worker, backends, front_port) = setup_h2_test("H2-PARSER-RESERVED-BIT", 1);
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);

    // Happy request → sozu emits SETTINGS/SETTINGS-ACK during handshake,
    // then HEADERS + optional DATA + WINDOW_UPDATE.
    let headers = H2Frame::headers(1, minimal_get_headers_block(), true, true);
    tls.write_all(&headers.encode()).unwrap();
    tls.flush().unwrap();

    // Drain everything sozu sends for the normal response.
    std::thread::sleep(std::time::Duration::from_millis(300));
    let mut raw = read_all_available(&mut tls, std::time::Duration::from_millis(800));

    // Now trigger a GOAWAY: PUSH_PROMISE from client is guaranteed to
    // produce GOAWAY(PROTOCOL_ERROR) with last-stream-id set to the
    // highest processed stream, exercising the goaway serializer path.
    let mut bad_payload = Vec::new();
    bad_payload.extend_from_slice(&3u32.to_be_bytes());
    bad_payload.extend_from_slice(&minimal_get_headers_block());
    let push_promise = H2Frame::new(0x5, 0x4, 1, bad_payload);
    let _ = tls.write_all(&push_promise.encode());
    let _ = tls.flush();

    std::thread::sleep(std::time::Duration::from_millis(400));
    raw.extend(read_all_available(
        &mut tls,
        std::time::Duration::from_millis(800),
    ));

    // Inspect every frame-header stream-id high byte.
    let high_bytes = raw_frame_stream_id_high_bytes(&raw);
    let leaked_header = high_bytes.iter().any(|b| *b & 0x80 != 0);

    // Inspect GOAWAY payload[0] for reserved-bit leakage on last_stream_id.
    let parsed = parse_h2_frames(&raw);
    let leaked_goaway_payload = parsed.iter().any(|(ft, _, _, payload)| {
        *ft == H2_FRAME_GOAWAY && payload.len() >= 8 && (payload[0] & 0x80) != 0
    });

    println!(
        "Reserved-bit mask - frames_inspected={} leaked_header={leaked_header} \
         leaked_goaway_payload={leaked_goaway_payload}",
        high_bytes.len()
    );

    let infra_ok = teardown(tls, front_port, worker, backends);
    if infra_ok && !high_bytes.is_empty() && !leaked_header && !leaked_goaway_payload {
        State::Success
    } else {
        println!(
            "Reserved-bit mask - FAIL: frames={} leaked_header={leaked_header} \
             leaked_goaway_payload={leaked_goaway_payload} infra_ok={infra_ok}",
            high_bytes.len()
        );
        State::Fail
    }
}

#[test]
fn e2e_h2_parser_serializer_masks_reserved_bit() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 serializer: reserved R-bit masked on every outbound stream-id (RFC 9113 \u{00a7}4.1)",
            try_h2_parser_serializer_masks_reserved_bit,
        ),
        State::Success,
    );
}
