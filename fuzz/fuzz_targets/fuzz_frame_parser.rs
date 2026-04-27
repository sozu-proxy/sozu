#![no_main]
//! Fuzz target for the H2 frame parser (RFC 9113 §6).
//!
//! Exercises `preface`, `frame_header`, and `frame_body` against arbitrary
//! input; the parser must reject malformed framing through `H2Error` /
//! `nom::Err` and never panic. Defends against the length-confusion family
//! of CVEs that motivated `ensure_frame_size!`. Corpus + run instructions
//! live in `fuzz/README.md`.

use libfuzzer_sys::fuzz_target;
use sozu_lib::protocol::mux::parser;

fuzz_target!(|data: &[u8]| {
    // Try parsing as H2 client connection preface (24 bytes)
    let _ = parser::preface(data);

    // Try parsing a frame header with default max_frame_size (16384)
    if data.len() >= 9 {
        let max_frame_size = 16384;
        if let Ok((remaining, header)) = parser::frame_header(data, max_frame_size) {
            // If the header parsed successfully, try parsing the frame body
            let _ = parser::frame_body(remaining, &header);
        }

        // Also try with a larger max_frame_size to exercise different code paths
        let _ = parser::frame_header(data, 16_777_215);
    }
});
