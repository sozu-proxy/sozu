#![no_main]
//! Fuzz target for the HPACK decoder (RFC 7541).
//!
//! Exercises `loona_hpack::Decoder::decode` with arbitrary header block
//! fragments. The decoder must handle any byte sequence without panicking.
//!
//! Run: cargo +nightly fuzz run fuzz_hpack_decoder

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Create a fresh decoder for each input to avoid cross-contamination
    // between fuzzer-generated inputs (dynamic table state)
    let mut decoder = loona_hpack::Decoder::new();

    // Try decoding the input as an HPACK header block fragment.
    // The callback collects decoded headers but we discard them -- the goal
    // is to verify the decoder never panics on arbitrary input.
    let _ = decoder.decode_with_cb(data, |_key, _value| {
        // no-op: we only care that it doesn't panic
    });

    // Also exercise the decoder with a constrained dynamic table size,
    // which exercises different resize/eviction paths.
    let mut small_table_decoder = loona_hpack::Decoder::new();
    small_table_decoder.set_max_table_size(256);
    let _ = small_table_decoder.decode_with_cb(data, |_key, _value| {});

    // Zero-size table: forces all entries to be evicted immediately
    let mut zero_table_decoder = loona_hpack::Decoder::new();
    zero_table_decoder.set_max_table_size(0);
    let _ = zero_table_decoder.decode_with_cb(data, |_key, _value| {});
});
