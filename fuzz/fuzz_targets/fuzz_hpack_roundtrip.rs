#![no_main]
//! Round-trip fuzz target for the HPACK encoder and decoder of
//! `sozu_lib::protocol::mux::hpack` (RFC 7541).
//!
//! The input is read as a script: each field takes a control octet (the
//! representation, the Huffman flag, and whether a table size update opens a
//! new block), a name length, a value length, then the bytes. One encoder
//! writes the blocks and one decoder reads them; every decoded list must equal
//! the list sent, which only holds while both dynamic tables stay identical.
//! Corpus + run instructions live in `fuzz/README.md`.

use libfuzzer_sys::fuzz_target;
use sozu_lib::protocol::mux::hpack::{Decoder, Encoder, Representation, encode_integer, huffman};

fuzz_target!(|data: &[u8]| {
    let mut encoder = Encoder::new();
    let mut decoder = Decoder::new();
    decoder.set_max_allowed_table_size(4096);
    let mut block = Vec::new();
    let mut sent: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut rest = data;
    while let [control, name_len, value_len, tail @ ..] = rest {
        let (name_len, value_len) = (usize::from(*name_len), usize::from(*value_len));
        if tail.len() < name_len + value_len {
            break;
        }
        let (name, tail) = tail.split_at(name_len);
        let (value, tail) = tail.split_at(value_len);
        rest = tail;
        if control & 0x80 != 0 {
            flush(&mut decoder, &mut block, &mut sent);
            let size = usize::from(control & 0x3f) * 64;
            encoder.set_max_table_size(size);
            encode_integer(size, 5, 0x20, &mut block);
        }
        let representation = match control & 0x03 {
            0 => Representation::Proxy,
            1 => Representation::IncrementalIndexing,
            2 => Representation::WithoutIndexing,
            _ => Representation::NeverIndexed,
        };
        encoder.encode_field(name, value, representation, control & 0x04 != 0, &mut block);
        sent.push((name.to_vec(), value.to_vec()));
    }
    flush(&mut decoder, &mut block, &mut sent);

    let mut encoded = Vec::new();
    huffman::encode(data, &mut encoded);
    assert_eq!(encoded.len(), huffman::encoded_len(data));
    let mut decoded = Vec::new();
    huffman::decode(&encoded, &mut decoded).expect("a Huffman encoding decodes");
    assert_eq!(decoded, data);
});

fn flush(decoder: &mut Decoder, block: &mut Vec<u8>, sent: &mut Vec<(Vec<u8>, Vec<u8>)>) {
    if block.is_empty() {
        return;
    }
    let mut received = Vec::new();
    let status = decoder.decode_with_cb(block, |name, value| {
        received.push((name.into_owned(), value.into_owned()));
    });
    // A block holding only size updates is rejected by design (RFC 7541 §4.2
    // pairs an update with the block it opens).
    if sent.is_empty() {
        assert!(status.is_err());
    } else {
        assert_eq!(status, Ok(()));
        assert_eq!(&received, sent);
    }
    block.clear();
    sent.clear();
}
