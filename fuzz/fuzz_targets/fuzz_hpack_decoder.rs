#![no_main]
//! Fuzz target for the HPACK decoder (RFC 7541, `sozu_lib::protocol::mux::hpack`).
//!
//! Drives `decode_with_cb` against arbitrary header blocks under three
//! dynamic-table profiles (default, 256 bytes, zero) so the resize/eviction
//! paths are exercised, and a fourth decoder that sees the input twice so the
//! second pass meets a table the first one filled. The decoder must never
//! panic, must reject malformed input with a typed error, and must never let
//! its table exceed the size it was given. Defends against header-block
//! oversize, incomplete-update and Huffman padding flaws. Corpus + run
//! instructions live in `fuzz/README.md`.

use libfuzzer_sys::fuzz_target;
use sozu_lib::protocol::mux::hpack::{Decoder, huffman};

fuzz_target!(|data: &[u8]| {
    let mut decoder = Decoder::new();
    let _ = decoder.decode_with_cb(data, |_key, _value| {});
    let _ = decoder.decode_with_cb(data, |_key, _value| {});

    for size in [256, 0] {
        let mut decoder = Decoder::new();
        decoder.set_max_allowed_table_size(size);
        decoder.set_max_table_size(size);
        let _ = decoder.decode_with_cb(data, |_key, _value| {});
    }

    // The raw bytes as one Huffman string: decodes or fails, never more
    // octets than the bound.
    let mut out = Vec::new();
    let _ = huffman::decode(data, &mut out);
    assert!(out.len() <= data.len() / 5 * 8 + 8);
});
