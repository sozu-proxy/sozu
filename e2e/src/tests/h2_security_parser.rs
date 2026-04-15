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

use std::{io::Write, net::SocketAddr};

use super::h2_utils::{
    H2_ERROR_FRAME_SIZE_ERROR, H2_ERROR_PROTOCOL_ERROR, collect_response_frames,
    goaway_error_code, h2_handshake, log_frames, raw_h2_connection,
    rejected_with_goaway_or_rst, setup_h2_test, teardown,
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
