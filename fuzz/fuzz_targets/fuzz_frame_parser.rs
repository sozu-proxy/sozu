#![no_main]
//! Fuzz target for the H2 frame parser.
//!
//! Exercises `frame_header`, `frame_body`, and `preface` with arbitrary input.
//! The parser must handle any byte sequence without panicking.
//!
//! Run: cargo +nightly fuzz run fuzz_frame_parser

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
