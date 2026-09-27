//! Tests of the HPACK module alone, with no socket and no mux: RFC 7541
//! Appendix C, round trips, malformed blocks, fragmented blocks, table size
//! changes, allocation criteria, and a seeded simulation of an encoder and a
//! decoder exchanging long random header sequences.

use std::borrow::Cow;

use quickcheck::{QuickCheck, TestResult};

use super::{
    Decoder, DecoderError, Encoder, HuffmanError, decode_integer, encode_integer,
    encoder::Representation,
    huffman::{self, CODES},
    table::{DynamicTable, STATIC_TABLE},
};
use crate::test_allocations::allocations;

type Field = (Vec<u8>, Vec<u8>);

/// One example of RFC 7541 Appendix C: the block, the header list it
/// decodes to, and the dynamic table after it, newest entry first.
struct Vector {
    block: &'static str,
    headers: &'static [(&'static [u8], &'static [u8])],
    table: &'static [(&'static [u8], &'static [u8])],
    table_size: usize,
}

/// RFC 7541 Appendix C.2.1.
const C_2_1: Vector = Vector {
    block: "400a637573746f6d2d6b65790d637573746f6d2d686561646572",
    headers: &[(b"custom-key", b"custom-header")],
    table: &[(b"custom-key", b"custom-header")],
    table_size: 55,
};

/// RFC 7541 Appendix C.2.2.
const C_2_2: Vector = Vector {
    block: "040c2f73616d706c652f70617468",
    headers: &[(b":path", b"/sample/path")],
    table: &[],
    table_size: 0,
};

/// RFC 7541 Appendix C.2.3.
const C_2_3: Vector = Vector {
    block: "100870617373776f726406736563726574",
    headers: &[(b"password", b"secret")],
    table: &[],
    table_size: 0,
};

/// RFC 7541 Appendix C.2.4.
const C_2_4: Vector = Vector {
    block: "82",
    headers: &[(b":method", b"GET")],
    table: &[],
    table_size: 0,
};

/// RFC 7541 Appendix C.3.1.
const C_3_1: Vector = Vector {
    block: "828684410f7777772e6578616d706c652e636f6d",
    headers: &[
        (b":method", b"GET"),
        (b":scheme", b"http"),
        (b":path", b"/"),
        (b":authority", b"www.example.com"),
    ],
    table: &[(b":authority", b"www.example.com")],
    table_size: 57,
};

/// RFC 7541 Appendix C.3.2.
const C_3_2: Vector = Vector {
    block: "828684be58086e6f2d6361636865",
    headers: &[
        (b":method", b"GET"),
        (b":scheme", b"http"),
        (b":path", b"/"),
        (b":authority", b"www.example.com"),
        (b"cache-control", b"no-cache"),
    ],
    table: &[
        (b"cache-control", b"no-cache"),
        (b":authority", b"www.example.com"),
    ],
    table_size: 110,
};

/// RFC 7541 Appendix C.3.3.
const C_3_3: Vector = Vector {
    block: "828785bf400a637573746f6d2d6b65790c637573746f6d2d76616c7565",
    headers: &[
        (b":method", b"GET"),
        (b":scheme", b"https"),
        (b":path", b"/index.html"),
        (b":authority", b"www.example.com"),
        (b"custom-key", b"custom-value"),
    ],
    table: &[
        (b"custom-key", b"custom-value"),
        (b"cache-control", b"no-cache"),
        (b":authority", b"www.example.com"),
    ],
    table_size: 164,
};

/// RFC 7541 Appendix C.4.1.
const C_4_1: Vector = Vector {
    block: "828684418cf1e3c2e5f23a6ba0ab90f4ff",
    headers: &[
        (b":method", b"GET"),
        (b":scheme", b"http"),
        (b":path", b"/"),
        (b":authority", b"www.example.com"),
    ],
    table: &[(b":authority", b"www.example.com")],
    table_size: 57,
};

/// RFC 7541 Appendix C.4.2.
const C_4_2: Vector = Vector {
    block: "828684be5886a8eb10649cbf",
    headers: &[
        (b":method", b"GET"),
        (b":scheme", b"http"),
        (b":path", b"/"),
        (b":authority", b"www.example.com"),
        (b"cache-control", b"no-cache"),
    ],
    table: &[
        (b"cache-control", b"no-cache"),
        (b":authority", b"www.example.com"),
    ],
    table_size: 110,
};

/// RFC 7541 Appendix C.4.3.
const C_4_3: Vector = Vector {
    block: "828785bf408825a849e95ba97d7f8925a849e95bb8e8b4bf",
    headers: &[
        (b":method", b"GET"),
        (b":scheme", b"https"),
        (b":path", b"/index.html"),
        (b":authority", b"www.example.com"),
        (b"custom-key", b"custom-value"),
    ],
    table: &[
        (b"custom-key", b"custom-value"),
        (b"cache-control", b"no-cache"),
        (b":authority", b"www.example.com"),
    ],
    table_size: 164,
};

/// RFC 7541 Appendix C.5.1.
const C_5_1: Vector = Vector {
    block: "4803333032580770726976617465611d4d6f6e2c203231204f637420323031332032303a31333a323120474d546e1768747470733a2f2f7777772e6578616d706c652e636f6d",
    headers: &[
        (b":status", b"302"),
        (b"cache-control", b"private"),
        (b"date", b"Mon, 21 Oct 2013 20:13:21 GMT"),
        (b"location", b"https://www.example.com"),
    ],
    table: &[
        (b"location", b"https://www.example.com"),
        (b"date", b"Mon, 21 Oct 2013 20:13:21 GMT"),
        (b"cache-control", b"private"),
        (b":status", b"302"),
    ],
    table_size: 222,
};

/// RFC 7541 Appendix C.5.2.
const C_5_2: Vector = Vector {
    block: "4803333037c1c0bf",
    headers: &[
        (b":status", b"307"),
        (b"cache-control", b"private"),
        (b"date", b"Mon, 21 Oct 2013 20:13:21 GMT"),
        (b"location", b"https://www.example.com"),
    ],
    table: &[
        (b":status", b"307"),
        (b"location", b"https://www.example.com"),
        (b"date", b"Mon, 21 Oct 2013 20:13:21 GMT"),
        (b"cache-control", b"private"),
    ],
    table_size: 222,
};

/// RFC 7541 Appendix C.5.3.
const C_5_3: Vector = Vector {
    block: "88c1611d4d6f6e2c203231204f637420323031332032303a31333a323220474d54c05a04677a69707738666f6f3d4153444a4b48514b425a584f5157454f50495541585157454f49553b206d61782d6167653d333630303b2076657273696f6e3d31",
    headers: &[
        (b":status", b"200"),
        (b"cache-control", b"private"),
        (b"date", b"Mon, 21 Oct 2013 20:13:22 GMT"),
        (b"location", b"https://www.example.com"),
        (b"content-encoding", b"gzip"),
        (
            b"set-cookie",
            b"foo=ASDJKHQKBZXOQWEOPIUAXQWEOIU; max-age=3600; version=1",
        ),
    ],
    table: &[
        (
            b"set-cookie",
            b"foo=ASDJKHQKBZXOQWEOPIUAXQWEOIU; max-age=3600; version=1",
        ),
        (b"content-encoding", b"gzip"),
        (b"date", b"Mon, 21 Oct 2013 20:13:22 GMT"),
    ],
    table_size: 215,
};

/// RFC 7541 Appendix C.6.1.
const C_6_1: Vector = Vector {
    block: "488264025885aec3771a4b6196d07abe941054d444a8200595040b8166e082a62d1bff6e919d29ad171863c78f0b97c8e9ae82ae43d3",
    headers: &[
        (b":status", b"302"),
        (b"cache-control", b"private"),
        (b"date", b"Mon, 21 Oct 2013 20:13:21 GMT"),
        (b"location", b"https://www.example.com"),
    ],
    table: &[
        (b"location", b"https://www.example.com"),
        (b"date", b"Mon, 21 Oct 2013 20:13:21 GMT"),
        (b"cache-control", b"private"),
        (b":status", b"302"),
    ],
    table_size: 222,
};

/// RFC 7541 Appendix C.6.2.
const C_6_2: Vector = Vector {
    block: "4883640effc1c0bf",
    headers: &[
        (b":status", b"307"),
        (b"cache-control", b"private"),
        (b"date", b"Mon, 21 Oct 2013 20:13:21 GMT"),
        (b"location", b"https://www.example.com"),
    ],
    table: &[
        (b":status", b"307"),
        (b"location", b"https://www.example.com"),
        (b"date", b"Mon, 21 Oct 2013 20:13:21 GMT"),
        (b"cache-control", b"private"),
    ],
    table_size: 222,
};

/// RFC 7541 Appendix C.6.3.
const C_6_3: Vector = Vector {
    block: "88c16196d07abe941054d444a8200595040b8166e084a62d1bffc05a839bd9ab77ad94e7821dd7f2e6c7b335dfdfcd5b3960d5af27087f3672c1ab270fb5291f9587316065c003ed4ee5b1063d5007",
    headers: &[
        (b":status", b"200"),
        (b"cache-control", b"private"),
        (b"date", b"Mon, 21 Oct 2013 20:13:22 GMT"),
        (b"location", b"https://www.example.com"),
        (b"content-encoding", b"gzip"),
        (
            b"set-cookie",
            b"foo=ASDJKHQKBZXOQWEOPIUAXQWEOIU; max-age=3600; version=1",
        ),
    ],
    table: &[
        (
            b"set-cookie",
            b"foo=ASDJKHQKBZXOQWEOPIUAXQWEOIU; max-age=3600; version=1",
        ),
        (b"content-encoding", b"gzip"),
        (b"date", b"Mon, 21 Oct 2013 20:13:22 GMT"),
    ],
    table_size: 215,
};

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).expect("valid hex"))
        .collect()
}

fn decode(decoder: &mut Decoder, block: &[u8]) -> Result<Vec<Field>, DecoderError> {
    let mut fields = Vec::new();
    decoder.decode_with_cb(block, |name, value| {
        fields.push((name.into_owned(), value.into_owned()));
    })?;
    Ok(fields)
}

fn entries(table: &DynamicTable) -> Vec<Field> {
    table
        .iter()
        .map(|(name, value)| (name.to_vec(), value.to_vec()))
        .collect()
}

fn owned(fields: &[(&[u8], &[u8])]) -> Vec<Field> {
    fields
        .iter()
        .map(|(name, value)| (name.to_vec(), value.to_vec()))
        .collect()
}

/// Decodes a sequence of examples on one decoder and checks each header list
/// and each table state.
fn check_decoding(vectors: &[&Vector], table_size: usize) {
    let mut decoder = Decoder::new();
    decoder.set_max_table_size(table_size);
    for vector in vectors {
        let fields = decode(&mut decoder, &hex(vector.block)).expect("an RFC example decodes");
        assert_eq!(fields, owned(vector.headers));
        assert_eq!(entries(decoder.table()), owned(vector.table));
        assert_eq!(decoder.table().size(), vector.table_size);
    }
}

/// Encodes a sequence of examples on one encoder with the representation the
/// RFC used, and checks the exact bytes and each table state.
fn check_encoding(vectors: &[&Vector], table_size: usize, huffman: bool) {
    let mut encoder = Encoder::new();
    encoder.set_max_table_size(table_size);
    for vector in vectors {
        let mut block = Vec::new();
        for &(name, value) in vector.headers {
            encoder.encode_field(
                name,
                value,
                Representation::IncrementalIndexing,
                huffman,
                &mut block,
            );
        }
        assert_eq!(block, hex(vector.block));
        assert_eq!(entries(encoder.table()), owned(vector.table));
        assert_eq!(encoder.table().size(), vector.table_size);
    }
}

#[test]
fn rfc_c1_integers() {
    // C.1.1 to C.1.3.
    for (value, prefix, encoded) in [
        (10, 5, &[0x0a][..]),
        (1337, 5, &[0x1f, 0x9a, 0x0a][..]),
        (42, 8, &[0x2a][..]),
    ] {
        let mut out = Vec::new();
        encode_integer(value, prefix, 0, &mut out);
        assert_eq!(out, encoded);
        assert_eq!(decode_integer(encoded, prefix), Ok((value, encoded.len())));
    }
}

#[test]
fn rfc_c2_representations_decode() {
    for vector in [&C_2_1, &C_2_2, &C_2_3, &C_2_4] {
        check_decoding(&[vector], 4096);
    }
}

#[test]
fn rfc_c2_representations_encode() {
    let cases = [
        (&C_2_1, Representation::IncrementalIndexing),
        (&C_2_2, Representation::WithoutIndexing),
        (&C_2_3, Representation::NeverIndexed),
        (&C_2_4, Representation::IncrementalIndexing),
    ];
    for (vector, representation) in cases {
        let mut encoder = Encoder::new();
        let mut block = Vec::new();
        for &(name, value) in vector.headers {
            encoder.encode_field(name, value, representation, false, &mut block);
        }
        assert_eq!(block, hex(vector.block));
        assert_eq!(entries(encoder.table()), owned(vector.table));
    }
}

#[test]
fn rfc_c3_requests_without_huffman() {
    check_decoding(&[&C_3_1, &C_3_2, &C_3_3], 4096);
    check_encoding(&[&C_3_1, &C_3_2, &C_3_3], 4096, false);
}

#[test]
fn rfc_c4_requests_with_huffman() {
    check_decoding(&[&C_4_1, &C_4_2, &C_4_3], 4096);
    check_encoding(&[&C_4_1, &C_4_2, &C_4_3], 4096, true);
}

#[test]
fn rfc_c5_responses_without_huffman() {
    check_decoding(&[&C_5_1, &C_5_2, &C_5_3], 256);
    check_encoding(&[&C_5_1, &C_5_2, &C_5_3], 256, false);
}

#[test]
fn rfc_c6_responses_with_huffman() {
    check_decoding(&[&C_6_1, &C_6_2, &C_6_3], 256);
    check_encoding(&[&C_6_1, &C_6_2, &C_6_3], 256, true);
}

#[test]
fn static_table_is_appendix_a() {
    assert_eq!(STATIC_TABLE.len(), 61);
    assert_eq!(STATIC_TABLE[0], (&b":authority"[..], &b""[..]));
    assert_eq!(
        STATIC_TABLE[15],
        (&b"accept-encoding"[..], &b"gzip, deflate"[..])
    );
    assert_eq!(STATIC_TABLE[60], (&b"www-authenticate"[..], &b""[..]));
}

#[test]
fn every_huffman_symbol_decodes_alone() {
    for symbol in 0..=255u8 {
        let mut encoded = Vec::new();
        huffman::encode(&[symbol], &mut encoded);
        assert_eq!(encoded.len(), huffman::encoded_len(&[symbol]));
        let mut decoded = Vec::new();
        huffman::decode(&encoded, &mut decoded).expect("a lone symbol decodes");
        assert_eq!(decoded, [symbol], "symbol {symbol}");
    }
    // The code is complete: its Kraft sum is exactly 1.
    let kraft: u64 = CODES.iter().map(|&(_, bits)| 1u64 << (30 - bits)).sum();
    assert_eq!(kraft, 1 << 30);
}

#[test]
fn huffman_rejects_eos_and_bad_padding() {
    let decode = |input: &[u8]| huffman::decode(input, &mut Vec::new());
    // EOS is thirty ones: four octets of ones contain it whole.
    assert_eq!(decode(&[0xff, 0xff, 0xff, 0xff]), Err(HuffmanError::Eos));
    // '0' is 00000; the three zero bits after it are not a prefix of EOS.
    assert_eq!(decode(&[0x00]), Err(HuffmanError::InvalidPadding));
    // Eight ones: all ones, but longer than seven bits.
    assert_eq!(decode(&[0xff]), Err(HuffmanError::PaddingTooLong));
    // 'a' is 00011, then eleven ones of padding.
    assert_eq!(
        decode(&[0b0001_1111, 0b1111_1111]),
        Err(HuffmanError::PaddingTooLong)
    );
    // An incomplete code: the first eight bits of '!' (1111111000), cut short.
    assert_eq!(decode(&[0xfe]), Err(HuffmanError::InvalidPadding));
    // 00011000 00111111 is 'a' (00011), '0' (00000), then six ones of
    // padding: valid.
    let mut out = Vec::new();
    huffman::decode(&[0b0001_1000, 0b0011_1111], &mut out).expect("valid padding");
    assert_eq!(out, b"a0");
    // The empty string is valid.
    assert_eq!(decode(&[]), Ok(()));
}

#[test]
fn malformed_blocks_fail_with_their_error() {
    let cases: [(&str, &[u8], DecoderError); 14] = [
        ("indexed 0", &[0x80], DecoderError::InvalidIndex),
        (
            "indexed past both tables",
            &[0xbe],
            DecoderError::InvalidIndex,
        ),
        (
            "literal name index past both tables",
            &[0x7e, 0x00],
            DecoderError::InvalidIndex,
        ),
        (
            "prefix announces continuation octets",
            &[0xff],
            DecoderError::Truncated,
        ),
        (
            "continuation bit set on the last octet",
            &[0xff, 0x80],
            DecoderError::Truncated,
        ),
        (
            "integer longer than five octets",
            &[0xff, 0x80, 0x80, 0x80, 0x80, 0x00],
            DecoderError::IntegerOverflow,
        ),
        (
            "literal without its name string",
            &[0x40],
            DecoderError::Truncated,
        ),
        (
            "string longer than the block",
            &[0x40, 0x05, b'a'],
            DecoderError::Truncated,
        ),
        ("value missing", &[0x04], DecoderError::Truncated),
        (
            "Huffman EOS in a value",
            &[0x04, 0x84, 0xff, 0xff, 0xff, 0xff],
            DecoderError::Huffman(HuffmanError::Eos),
        ),
        (
            "Huffman padding of zeros",
            &[0x04, 0x81, 0x00],
            DecoderError::Huffman(HuffmanError::InvalidPadding),
        ),
        (
            "size update after a field",
            &[0x82, 0x20],
            DecoderError::SizeUpdateNotAtStart,
        ),
        (
            "three leading size updates",
            &[0x20, 0x20, 0x20, 0x82],
            DecoderError::TooManySizeUpdates,
        ),
        (
            "block ends with a size update",
            &[0x20],
            DecoderError::SizeUpdateAtEnd,
        ),
    ];
    for (case, block, expected) in cases {
        let mut decoder = Decoder::new();
        assert_eq!(decode(&mut decoder, block), Err(expected), "{case}");
    }
}

#[test]
fn size_update_is_bounded_by_the_advertised_setting() {
    let mut decoder = Decoder::new();
    decoder.set_max_allowed_table_size(100);
    // 0x3f 0x46 is 31 + 70 = 101.
    assert_eq!(
        decode(&mut decoder, &[0x3f, 0x46, 0x82]),
        Err(DecoderError::TableSizeTooLarge)
    );
    let mut decoder = Decoder::new();
    decoder.set_max_allowed_table_size(100);
    // 0x3f 0x45 is 100: accepted, then two updates is the most §4.2 allows.
    assert_eq!(
        decode(&mut decoder, &[0x20, 0x3f, 0x45, 0x82]),
        Ok(owned(&[(b":method", b"GET")]))
    );
    assert_eq!(decoder.table().max_size(), 100);
}

#[test]
fn a_dynamic_name_is_copied_before_the_insertion_evicts_it() {
    // A table that holds exactly one entry: re-indexing its only entry's
    // name with a new value evicts that entry while inserting.
    let mut decoder = Decoder::new();
    decoder.set_max_table_size(32 + 10 + 3);
    decode(&mut decoder, &hex("400a637573746f6d2d6b657903616263")).expect("insert");
    // 0x7e = literal with indexing, name index 62.
    let fields = decode(&mut decoder, &hex("7e03787978")).expect("re-index");
    assert_eq!(fields, owned(&[(b"custom-key", b"xyx")]));
    assert_eq!(entries(decoder.table()), owned(&[(b"custom-key", b"xyx")]));
}

#[test]
fn an_entry_larger_than_the_table_empties_it() {
    // RFC 7541 §4.4: not an error, the table ends up empty.
    let mut decoder = Decoder::new();
    decoder.set_max_table_size(40);
    decode(&mut decoder, &hex("4001610162")).expect("a 34-octet entry fits");
    assert_eq!(decoder.table().len(), 1);
    let fields = decode(&mut decoder, &hex("400a637573746f6d2d6b657903616263")).expect("decodes");
    assert_eq!(fields, owned(&[(b"custom-key", b"abc")]));
    assert_eq!(decoder.table().len(), 0);
}

/// A block split at any boundary, as HEADERS + CONTINUATION frames carry
/// it, decodes to the same fields once the fragments are joined — which is
/// what `h2_header_reassembly` does before decoding. A fragment alone either
/// decodes or fails cleanly; it never panics.
#[test]
fn a_block_split_at_any_boundary() {
    let mut encoder = Encoder::new();
    let headers: Vec<Field> = (0..12)
        .map(|at| (format!("x-field-{at}").into_bytes(), vec![b'v'; at * 7]))
        .collect();
    let first = encoder.encode(headers.iter().map(|(n, v)| (&n[..], &v[..])));
    let second = encoder.encode(headers.iter().map(|(n, v)| (&n[..], &v[..])));
    let mut reference = Decoder::new();
    let expected = [
        decode(&mut reference, &first).expect("first block"),
        decode(&mut reference, &second).expect("second block"),
    ];
    for split in 0..=first.len() {
        for split2 in [0, second.len() / 3, second.len()] {
            let (a, b) = first.split_at(split);
            let (c, d) = second.split_at(split2);
            let mut joined = a.to_vec();
            joined.extend_from_slice(b);
            let mut decoder = Decoder::new();
            assert_eq!(decode(&mut decoder, &joined).as_ref(), Ok(&expected[0]));
            let mut joined = c.to_vec();
            joined.extend_from_slice(d);
            assert_eq!(decode(&mut decoder, &joined).as_ref(), Ok(&expected[1]));
            let _ = decode(&mut Decoder::new(), a);
        }
    }
}

#[test]
fn table_size_changes_between_blocks() {
    let mut encoder = Encoder::new();
    let mut decoder = Decoder::new();
    decoder.set_max_allowed_table_size(4096);
    let fields: Vec<Field> = (0..40)
        .map(|at| {
            (
                format!("x-{at}").into_bytes(),
                format!("value-{at}").into_bytes(),
            )
        })
        .collect();
    for (round, size) in [4096usize, 0, 64, 4096, 200, 1000, 0, 4096]
        .into_iter()
        .enumerate()
    {
        let mut block = Vec::new();
        encoder.set_max_table_size(size);
        encode_integer(size, 5, 0x20, &mut block);
        for (name, value) in fields.iter().skip(round).take(20) {
            encoder.encode_header_into((name, value), &mut block);
        }
        let decoded = decode(&mut decoder, &block).expect("the block decodes");
        assert_eq!(decoded, fields[round..round + 20].to_vec());
        assert_eq!(entries(encoder.table()), entries(decoder.table()));
        assert!(decoder.table().size() <= size);
    }
}

/// The size updates `Encoder::encode_size_updates_into` should open a block
/// with, built with the integer encoder rather than spelled as octets.
fn size_updates(sizes: &[usize]) -> Vec<u8> {
    let mut expected = Vec::new();
    for &size in sizes {
        encode_integer(size, 5, 0x20, &mut expected);
    }
    expected
}

/// Encodes `fields` into a block opened by the recorded size updates, the
/// way `H2BlockConverter` opens its first block after a SETTINGS change,
/// decodes it, and checks both tables hold the same entries.
fn exchange_after_size_changes(
    encoder: &mut Encoder,
    decoder: &mut Decoder,
    last_size: usize,
    fields: &[Field],
) -> Vec<u8> {
    let mut block = Vec::new();
    encoder.encode_size_updates_into(last_size, &mut block);
    for (name, value) in fields {
        encoder.encode_header_into((name, value), &mut block);
    }
    let decoded = decode(decoder, &block).expect("the block decodes");
    assert_eq!(&decoded, fields);
    assert_eq!(
        entries(encoder.table()),
        entries(decoder.table()),
        "the dynamic tables diverged"
    );
    block
}

/// Fields whose names are in no table, so the proxy policy inserts them.
fn dynamic_fields(tag: &str) -> Vec<Field> {
    (0..4)
        .map(|at| {
            (
                format!("x-{tag}-{at}").into_bytes(),
                format!("value-{at}").into_bytes(),
            )
        })
        .collect()
}

/// Issue #1622, RFC 7541 §4.2: the peer sends `SETTINGS_HEADER_TABLE_SIZE` 0
/// then 4096 between two blocks. The encoder emptied its table at 0, so the
/// next block must open with 0, then 4096; announcing 4096 alone leaves the
/// peer's decoder holding the entries of the first block, which the encoder
/// then inserts again.
///
/// To SEE THIS RED: in `Encoder::encode_size_updates_into`, drop the branch
/// that appends `pending.smallest`. The table comparison in
/// `exchange_after_size_changes` fails ("the dynamic tables diverged"): the
/// decoder holds eight entries, the encoder four.
#[test]
fn a_size_lowered_then_raised_between_blocks_signals_both() {
    let mut encoder = Encoder::new();
    let mut decoder = Decoder::new();
    decoder.set_max_allowed_table_size(4096);
    let fields = dynamic_fields("sized");
    let mut block = Vec::new();
    for (name, value) in &fields {
        encoder.encode_header_into((name, value), &mut block);
    }
    assert_eq!(decode(&mut decoder, &block).as_deref(), Ok(&fields[..]));
    assert_eq!(entries(decoder.table()).len(), fields.len());

    encoder.change_max_table_size(0);
    encoder.change_max_table_size(4096);
    let block = exchange_after_size_changes(&mut encoder, &mut decoder, 4096, &fields);
    assert!(
        block.starts_with(&size_updates(&[0, 4096])),
        "the block opens with the smallest size, then the last: {block:02x?}"
    );
    assert_eq!(entries(decoder.table()).len(), fields.len());

    // Nothing stays recorded: the next change is signalled alone.
    encoder.change_max_table_size(2048);
    let block = exchange_after_size_changes(&mut encoder, &mut decoder, 2048, &fields);
    assert!(block.starts_with(&size_updates(&[2048])));
    assert!(!block[size_updates(&[2048]).len()..].starts_with(&[0x20]));
}

/// One change between two blocks, and changes that never go below the last
/// size, open the block with that size alone — the bytes sozu sent before
/// issue #1622.
#[test]
fn a_size_never_lowered_below_the_last_signals_it_alone() {
    for changes in [&[1024][..], &[4096], &[4096, 4096], &[512, 256, 256]] {
        let mut encoder = Encoder::new();
        let mut decoder = Decoder::new();
        decoder.set_max_allowed_table_size(4096);
        let fields = dynamic_fields("once");
        exchange_after_size_changes(&mut encoder, &mut decoder, 4096, &fields);
        for &size in changes {
            encoder.change_max_table_size(size);
        }
        let last = *changes.last().expect("one change at least");
        let mut block = Vec::new();
        assert_eq!(encoder.encode_size_updates_into(last, &mut block), 1);
        assert_eq!(block, size_updates(&[last]), "changes {changes:?}");
        for (name, value) in &fields {
            encoder.encode_header_into((name, value), &mut block);
        }
        assert_eq!(decode(&mut decoder, &block).as_deref(), Ok(&fields[..]));
        assert_eq!(entries(encoder.table()), entries(decoder.table()));
    }
}

/// When the smallest size is the last one (4096, then 0, then 64, then 0),
/// one update carries both.
#[test]
fn a_smallest_size_equal_to_the_last_is_signalled_once() {
    let mut encoder = Encoder::new();
    let mut decoder = Decoder::new();
    decoder.set_max_allowed_table_size(4096);
    exchange_after_size_changes(&mut encoder, &mut decoder, 4096, &dynamic_fields("eq"));
    for size in [4096, 0, 64, 0] {
        encoder.change_max_table_size(size);
    }
    let mut block = Vec::new();
    assert_eq!(encoder.encode_size_updates_into(0, &mut block), 1);
    assert_eq!(block, size_updates(&[0]));
    exchange_after_size_changes(&mut encoder, &mut decoder, 0, &dynamic_fields("zero"));
    assert!(entries(decoder.table()).is_empty());
}

/// Issue #1622 on the path the H2 connection takes, with only the calls
/// `ConnectionH2::handle_settings_frame` and `ConnectionH2::write_streams`
/// (`lib/src/protocol/mux/h2.rs`) make: `HpackState::set_encoder_max_table_size`
/// per SETTINGS, the last size mirrored into an `H2ConverterPass`, and one
/// `H2BlockConverter` encoding the next response block.
///
/// To SEE THIS RED: restore `HpackState::set_encoder_max_table_size` to
/// `Encoder::set_max_table_size` and `emit_pending_size_update_if_new_block`
/// to the single `encode_integer` it made. The block opens with 4096 alone
/// and the decoder keeps the first response's entry.
#[test]
fn the_h2_converter_signals_the_smallest_table_size_first() {
    use kawa::{Block, BlockConverter, Buffer, Kawa, Kind, Pair, SliceBuffer, StatusLine, Store};

    use crate::protocol::mux::{
        buffer_source::PoolBufferSource, converter::H2ConverterPass, hpack_state::HpackState,
    };

    let mut source = PoolBufferSource::new(std::rc::Weak::new());
    let mut hpack = HpackState::new(&mut source, 4096).expect("scratch buffers");
    let mut decoder = Decoder::new();
    decoder.set_max_allowed_table_size(4096);

    let mut respond = |hpack: &mut HpackState, pending: Option<u32>, name: &'static [u8]| {
        let mut pass = H2ConverterPass::new(
            16384,
            b"https",
            false,
            hpack.take_converter_buf(),
            hpack.take_lowercase_buf(),
            hpack.take_cookie_buf(),
            pending,
        );
        let mut storage = vec![0u8; 1024];
        let mut kawa = Kawa::new(Kind::Response, Buffer::new(SliceBuffer(&mut storage)));
        kawa.detached.status_line = StatusLine::Response {
            version: kawa::Version::V20,
            code: 200,
            status: Store::Static(b"200"),
            reason: Store::Static(b"OK"),
        };
        let mut converter = pass.converter(hpack.encoder_mut(), 1, 65535, false, 0);
        assert!(converter.call(Block::StatusLine, &mut kawa));
        assert!(converter.call(
            Block::Header(Pair {
                key: Store::Static(name),
                val: Store::Static(b"sozu"),
            }),
            &mut kawa,
        ));
        let block = converter.out.clone();
        pass.reclaim(converter, &mut Vec::new());
        assert_eq!(pass.size_update_emitted(), pending.is_some());
        let (out, lowercase, cookie) = pass.into_buffers();
        hpack.put_converter_buf(out);
        hpack.put_lowercase_buf(lowercase);
        hpack.put_cookie_buf(cookie);
        let decoded = decode(&mut decoder, &block).expect("the response block decodes");
        assert_eq!(decoded[1], (name.to_vec(), b"sozu".to_vec()));
        (block, entries(decoder.table()))
    };

    let (_, peer_table) = respond(&mut hpack, None, b"x-first");
    assert_eq!(peer_table.len(), 1, "the first response fills the table");

    // Two SETTINGS before the next block, as `handle_settings_frame` applies
    // them: the connection's mirror ends up holding the last size only.
    hpack.set_encoder_max_table_size(0);
    hpack.set_encoder_max_table_size(4096);
    let (block, peer_table) = respond(&mut hpack, Some(4096), b"x-second");
    assert!(
        block.starts_with(&size_updates(&[0, 4096])),
        "the block opens with 0, then 4096: {block:02x?}"
    );
    assert_eq!(
        peer_table,
        entries(hpack.encoder_mut().table()),
        "the peer's decoder holds what our encoder holds"
    );
    assert_eq!(peer_table.len(), 1);
}

/// The default policy keeps the bytes `loona-hpack` sent: a field found whole
/// is indexed, a name found is a literal without indexing, and only a field
/// with a new name enters the dynamic table.
#[test]
fn the_proxy_policy_is_unchanged() {
    let mut encoder = Encoder::new();
    let mut out = Vec::new();
    encoder.encode_header_into((b":method", b"GET"), &mut out);
    assert_eq!(out, [0x82]);
    out.clear();
    encoder.encode_header_into((b":method", b"PUT"), &mut out);
    assert_eq!(out, [0x03, 0x03, b'P', b'U', b'T']);
    out.clear();
    encoder.encode_header_into((b":status", b"201"), &mut out);
    assert_eq!(out, [0x0e, 0x03, b'2', b'0', b'1']);
    out.clear();
    encoder.encode_header_into((b"custom-key", b"custom-value"), &mut out);
    assert_eq!(out[0], 0x40);
    assert_eq!(encoder.table().len(), 1);
    out.clear();
    encoder.encode_header_into((b"custom-key", b"custom-value"), &mut out);
    assert_eq!(out, [0x80 | 62]);
    out.clear();
    encoder.encode_header_into((b"custom-key", b"other"), &mut out);
    assert_eq!(out, [0x0f, 62 - 15, 0x05, b'o', b't', b'h', b'e', b'r']);
    assert_eq!(encoder.table().len(), 1);
}

fn roundtrip(fields: Vec<Field>, huffman: bool, representation: Representation) -> TestResult {
    let mut encoder = Encoder::new();
    let mut decoder = Decoder::new();
    let mut block = Vec::new();
    for (name, value) in &fields {
        encoder.encode_field(name, value, representation, huffman, &mut block);
    }
    match decode(&mut decoder, &block) {
        Ok(decoded) if decoded == fields => {}
        other => return TestResult::error(format!("{fields:?} decoded as {other:?}")),
    }
    if entries(encoder.table()) != entries(decoder.table()) {
        return TestResult::error("the two tables diverged");
    }
    TestResult::passed()
}

#[test]
fn quickcheck_roundtrip_every_representation() {
    fn property(fields: Vec<Field>, huffman: bool, pick: u8) -> TestResult {
        let representation = match pick % 4 {
            0 => Representation::Proxy,
            1 => Representation::IncrementalIndexing,
            2 => Representation::WithoutIndexing,
            _ => Representation::NeverIndexed,
        };
        roundtrip(fields, huffman, representation)
    }
    QuickCheck::new()
        .tests(2000)
        .quickcheck(property as fn(Vec<Field>, bool, u8) -> TestResult);
}

#[test]
fn quickcheck_huffman_roundtrip_and_arbitrary_input() {
    fn property(input: Vec<u8>) -> bool {
        let mut encoded = Vec::new();
        huffman::encode(&input, &mut encoded);
        let mut decoded = Vec::new();
        let roundtrip = huffman::decode(&encoded, &mut decoded).is_ok() && decoded == input;
        // Arbitrary octets decode or fail; the output never exceeds the bound.
        let mut out = Vec::new();
        let _ = huffman::decode(&input, &mut out);
        roundtrip && out.len() <= huffman::max_decoded_len(input.len())
    }
    QuickCheck::new()
        .tests(5000)
        .quickcheck(property as fn(Vec<u8>) -> bool);
}

#[test]
fn quickcheck_arbitrary_blocks_never_panic() {
    fn property(block: Vec<u8>, size: u16) -> bool {
        let mut decoder = Decoder::new();
        decoder.set_max_allowed_table_size(usize::from(size));
        decoder.set_max_table_size(usize::from(size));
        let _ = decoder.decode_with_cb(&block, |_, _| {});
        decoder.table().size() <= usize::from(size)
    }
    QuickCheck::new()
        .tests(5000)
        .quickcheck(property as fn(Vec<u8>, u16) -> bool);
}

/// Criterion: Huffman decoding allocates nothing — the machine is static and
/// the output goes to the caller's buffer.
///
/// TO SEE THIS RED: the same count against `loona_hpack::Decoder`, which
/// builds a `HashMap` of 257 entries per Huffman string, was 64 allocations
/// for this one string before this module replaced it.
#[test]
fn huffman_decoding_allocates_nothing() {
    let encoded = hex("f1e3c2e5f23a6ba0ab90f4ff");
    let mut out = Vec::with_capacity(huffman::max_decoded_len(encoded.len()));
    let before = allocations();
    huffman::decode(&encoded, &mut out).expect("C.4.1 value");
    assert_eq!(allocations() - before, 0);
    assert_eq!(out, b"www.example.com");

    let mut decoder = Decoder::new();
    // Literal without indexing, :authority, Huffman "www.example.com".
    let block = hex("018cf1e3c2e5f23a6ba0ab90f4ff");
    decode(&mut decoder, &block).expect("warm-up");
    let mut length = 0;
    let before = allocations();
    decoder
        .decode_with_cb(&block, |name, value| length += name.len() + value.len())
        .expect("decodes");
    assert_eq!(allocations() - before, 0);
    assert_eq!(length, b":authority".len() + b"www.example.com".len());
}

/// Criterion: a steady-state header block allocates nothing outside what the
/// callback stores. The C.4 cycle indexes, inserts, evicts and Huffman-decodes.
///
/// TO SEE THIS RED: the same cycle against `loona_hpack::Decoder` allocated
/// 257 times per cycle before this module replaced it.
#[test]
fn a_steady_state_block_allocates_nothing() {
    let blocks = [hex(C_4_1.block), hex(C_4_2.block), hex(C_4_3.block)];
    let mut decoder = Decoder::new();
    decoder.set_max_table_size(256);
    for _ in 0..64 {
        for block in &blocks {
            decoder.decode_with_cb(block, |_, _| {}).expect("warm-up");
        }
    }
    let mut fields = 0;
    let before = allocations();
    for _ in 0..16 {
        for block in &blocks {
            decoder
                .decode_with_cb(block, |_, _| fields += 1)
                .expect("steady state");
        }
    }
    assert_eq!(allocations() - before, 0);
    assert_eq!(fields, 16 * (4 + 5 + 5));

    let mut encoder = Encoder::new();
    let headers: Vec<(&[u8], &[u8])> = C_4_3.headers.to_vec();
    let mut out = Vec::with_capacity(512);
    for _ in 0..8 {
        out.clear();
        encoder.encode_into(headers.iter().copied(), &mut out);
    }
    let before = allocations();
    out.clear();
    encoder.encode_into(headers.iter().copied(), &mut out);
    assert_eq!(allocations() - before, 0);
}

/// A seeded, deterministic simulation: one sozu encoder and one sozu decoder
/// exchange long sequences of random header blocks — random fields drawn from
/// a small pool so the tables hit, random representations, Huffman or not,
/// and random table size changes signalled by size updates. After every
/// block the decoded list equals the sent one and both dynamic tables hold
/// the same entries. `SOZU_HPACK_SIM_SEED` replays one seed.
#[test]
fn simulation_encoder_and_decoder_tables_stay_identical() {
    let seeds: Vec<u64> = match std::env::var("SOZU_HPACK_SIM_SEED") {
        Ok(seed) => vec![seed.parse().expect("SOZU_HPACK_SIM_SEED is a u64")],
        Err(_) => (1..=16).collect(),
    };
    for seed in seeds {
        simulate(seed, 2_000);
    }
}

/// xorshift64*: deterministic across platforms and toolchains.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: u64) -> usize {
        (self.next() % bound) as usize
    }
}

fn simulate(seed: u64, blocks: usize) {
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let mut encoder = Encoder::new();
    let mut decoder = Decoder::new();
    let allowed = 4096;
    decoder.set_max_allowed_table_size(allowed);
    let names: Vec<Vec<u8>> = STATIC_TABLE
        .iter()
        .step_by(5)
        .map(|(name, _)| name.to_vec())
        .chain((0..24).map(|at| format!("x-sim-{at}").into_bytes()))
        .collect();
    let values: Vec<Vec<u8>> = (0..32)
        .map(|at| {
            let length = (at * 13) % 300;
            (0..length)
                .map(|byte| ((byte * 31 + at) % 256) as u8)
                .collect()
        })
        .collect();
    for step in 0..blocks {
        let mut block = Vec::new();
        if rng.below(10) == 0 {
            let size = rng.below(allowed as u64 + 1);
            encoder.set_max_table_size(size);
            encode_integer(size, 5, 0x20, &mut block);
        }
        let mut sent = Vec::new();
        for _ in 0..1 + rng.below(16) {
            let name = &names[rng.below(names.len() as u64)];
            let value = &values[rng.below(values.len() as u64)];
            let representation = match rng.below(4) {
                0 => Representation::Proxy,
                1 => Representation::IncrementalIndexing,
                2 => Representation::WithoutIndexing,
                _ => Representation::NeverIndexed,
            };
            let huffman = rng.below(2) == 0;
            encoder.encode_field(name, value, representation, huffman, &mut block);
            sent.push((name.clone(), value.clone()));
        }
        let mut received = Vec::new();
        let status = decoder.decode_with_cb(&block, |name, value| {
            received.push((name.into_owned(), value.into_owned()));
        });
        assert_eq!(status, Ok(()), "seed {seed} step {step}");
        assert_eq!(received, sent, "seed {seed} step {step}");
        assert_eq!(
            entries(encoder.table()),
            entries(decoder.table()),
            "seed {seed} step {step}: the dynamic tables diverged"
        );
        assert_eq!(encoder.table().size(), decoder.table().size());
    }
}

#[test]
fn the_callback_always_borrows() {
    let mut decoder = Decoder::new();
    decoder
        .decode_with_cb(&hex(C_4_1.block), |name, value| {
            assert!(matches!(name, Cow::Borrowed(_)));
            assert!(matches!(value, Cow::Borrowed(_)));
        })
        .expect("decodes");
}

/// Issue #1627: the encoder's table changes when a block is ENCODED, so a
/// block encoded and then dropped unsent leaves the peer's decoder behind —
/// and not only with an error. Entries are numbered from the newest (RFC 7541
/// §2.3.3): each entry the dropped block inserted moves the older ones one
/// index down in the encoder's table only. A later block naming an older
/// entry then makes the peer read ANOTHER field, silently: here `x-c: 3` is
/// read as `x-a: 1`, and a literal naming `x-c` is read under the name `x-a`.
/// `Encoder::reset_table` repairs it: the next block opens with the size
/// updates 0, then the maximum size, and both tables are empty again.
#[test]
fn a_dropped_block_substitutes_fields_until_the_table_is_reset() {
    let a: Field = (b"x-a".to_vec(), b"1".to_vec());
    let c: Field = (b"x-c".to_vec(), b"3".to_vec());
    let mut encoder = Encoder::new();
    let mut decoder = Decoder::new();
    decoder.set_max_allowed_table_size(4096);

    let sent = encoder.encode([(&a.0[..], &a.1[..]), (&c.0[..], &c.1[..])]);
    assert_eq!(decode(&mut decoder, &sent), Ok(vec![a.clone(), c.clone()]));
    let _dropped = encoder.encode([(&b"x-b"[..], &b"2"[..])]);
    assert_eq!(
        entries(encoder.table()).len(),
        3,
        "the dropped block inserted x-b"
    );

    let block = encoder.encode([(&c.0[..], &c.1[..]), (&c.0[..], &b"other"[..])]);
    assert_eq!(
        decode(&mut decoder, &block),
        Ok(vec![a.clone(), (a.0.clone(), b"other".to_vec())]),
        "the peer reads x-a where x-c was encoded, with no error"
    );

    let before = allocations();
    let max_size = encoder.reset_table();
    assert_eq!(allocations() - before, 0, "the reset allocates nothing");
    assert_eq!(max_size, 4096);
    assert!(entries(encoder.table()).is_empty());
    let block = exchange_after_size_changes(
        &mut encoder,
        &mut decoder,
        max_size,
        std::slice::from_ref(&c),
    );
    assert!(
        block.starts_with(&size_updates(&[0, 4096])),
        "the reset opens the block with 0, then the size: {block:02x?}"
    );
    assert_eq!(
        entries(decoder.table()),
        vec![c],
        "both tables restart empty"
    );
}
