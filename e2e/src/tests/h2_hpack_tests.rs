//! HPACK (RFC 7541) exercised through a running Sōzu, over TLS + ALPN h2.
//!
//! The client side encodes its requests and decodes Sōzu's responses with the
//! same sans-io codec Sōzu runs, `sozu_lib::protocol::mux::hpack`, and keeps
//! one encoder and one decoder per connection — as a real client does — so a
//! dynamic table that drifts on either side shows up as a request Sōzu cannot
//! route or a response block the client cannot decode.
//!
//! Scenarios:
//!
//! - many indexed fields in one block;
//! - the dynamic table reused across the requests of one connection, with
//!   every block split into HEADERS + CONTINUATION fragments;
//! - `SETTINGS_HEADER_TABLE_SIZE` changed by the client, and size updates
//!   opening the client's own blocks;
//! - `SETTINGS_HEADER_TABLE_SIZE` lowered then raised before one block, which
//!   Sōzu signals as the smallest size, then the last;
//! - Huffman-coded and raw strings, including a Huffman `:authority` and
//!   `:path` that Sōzu must decode to route the request;
//! - header lists at the size limit: a list past what Sōzu accepts refuses
//!   the stream, and the connection's decoder stays in step;
//! - invalid blocks: each one is a connection error of type
//!   COMPRESSION_ERROR (RFC 9113 §4.3), sent as a GOAWAY.

use std::{
    io::Write,
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

use sozu_lib::protocol::mux::hpack::{
    Decoder, DecoderError, Encoder, Representation, encode_integer,
};

use super::h2_utils::{
    H2_ERROR_ENHANCE_YOUR_CALM, H2_ERROR_PROTOCOL_ERROR, H2_FLAG_END_HEADERS, H2_FLAG_END_STREAM,
    H2_FRAME_CONTINUATION, H2_FRAME_DATA, H2_FRAME_GOAWAY, H2_FRAME_HEADERS, H2Frame,
    advance_one_frame, contains_rst_stream_with_error, goaway_error_code, h2_handshake, log_frames,
    raw_h2_connection, read_all_available, setup_h2_test, teardown,
};
use crate::tests::{State, repeat_until_error_or};

type Frame = (u8, u8, u32, Vec<u8>);
type Field = (Vec<u8>, Vec<u8>);
type Tls = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;

/// RFC 9113 §7: COMPRESSION_ERROR.
const H2_ERROR_COMPRESSION_ERROR: u32 = 0x9;

/// RFC 9113 §6.5.2 identifier of `SETTINGS_HEADER_TABLE_SIZE`.
const SETTINGS_HEADER_TABLE_SIZE: u16 = 0x1;

/// The `SETTINGS_MAX_HEADER_LIST_SIZE` Sōzu advertises
/// (`MAX_HEADER_LIST_SIZE`, `lib/src/protocol/mux/h2.rs`).
const MAX_HEADER_LIST_SIZE: usize = 65_536;

/// Per-field overhead of RFC 9113 §6.5.2 and RFC 7541 §4.1.
const FIELD_OVERHEAD: usize = 32;

/// The request pseudo-headers every scenario starts with, encoded with the
/// representation and Huffman choice of the caller.
fn request_block(
    encoder: &mut Encoder,
    representation: Representation,
    huffman: bool,
    extra: &[(&[u8], &[u8])],
) -> Vec<u8> {
    let mut block = Vec::new();
    for (name, value) in [
        (&b":method"[..], &b"GET"[..]),
        (b":scheme", b"https"),
        (b":authority", b"localhost"),
        (b":path", b"/"),
    ]
    .into_iter()
    .chain(extra.iter().copied())
    {
        encoder.encode_field(name, value, representation, huffman, &mut block);
    }
    block
}

/// Sends `block` on `stream_id` as a HEADERS frame followed by CONTINUATION
/// frames of at most `fragment` octets each (RFC 9113 §6.10).
fn send_block(tls: &mut Tls, stream_id: u32, block: &[u8], fragment: usize) -> bool {
    let mut chunks = block.chunks(fragment.max(1)).peekable();
    let first = chunks.next().unwrap_or(&[]).to_vec();
    let mut wire = H2Frame::headers(stream_id, first, chunks.peek().is_none(), true).encode();
    while let Some(chunk) = chunks.next() {
        let last = chunks.peek().is_none();
        wire.extend(H2Frame::continuation(stream_id, chunk.to_vec(), last).encode());
    }
    tls.write_all(&wire).is_ok() && tls.flush().is_ok()
}

/// Reads frames until every stream of `streams` has ended, a GOAWAY arrived,
/// or `deadline` elapsed.
fn read_until_done(tls: &mut Tls, streams: &[u32], deadline: Duration) -> Vec<Frame> {
    let start = Instant::now();
    let mut carry = Vec::new();
    let mut frames: Vec<Frame> = Vec::new();
    while start.elapsed() < deadline {
        carry.extend(read_all_available(tls, Duration::from_millis(100)));
        while let Some(frame) = advance_one_frame(&mut carry) {
            frames.push(frame);
        }
        let ended = |sid: &u32| {
            frames.iter().any(|(ft, fl, s, _)| {
                s == sid
                    && (*ft == H2_FRAME_HEADERS || *ft == H2_FRAME_DATA)
                    && fl & H2_FLAG_END_STREAM != 0
            })
        };
        let goaway = frames.iter().any(|(ft, ..)| *ft == H2_FRAME_GOAWAY);
        if goaway || streams.iter().all(ended) {
            break;
        }
    }
    frames
}

/// Decodes every response field block in wire order with one decoder, as a
/// client would, joining CONTINUATION fragments first. Returns each stream's
/// fields in arrival order.
fn decode_responses(
    decoder: &mut Decoder,
    frames: &[Frame],
) -> Result<Vec<(u32, Vec<Field>)>, DecoderError> {
    let mut blocks = Vec::new();
    let mut pending: Option<(u32, Vec<u8>)> = None;
    for (ft, fl, sid, payload) in frames {
        match *ft {
            H2_FRAME_HEADERS => pending = Some((*sid, payload.clone())),
            H2_FRAME_CONTINUATION => {
                if let Some((_, block)) = pending.as_mut() {
                    block.extend_from_slice(payload);
                }
            }
            _ => continue,
        }
        if fl & H2_FLAG_END_HEADERS != 0
            && let Some((sid, block)) = pending.take()
        {
            let mut fields = Vec::new();
            decoder.decode_with_cb(&block, |name, value| {
                fields.push((name.into_owned(), value.into_owned()));
            })?;
            blocks.push((sid, fields));
        }
    }
    Ok(blocks)
}

fn status_of(fields: &[Field]) -> Option<&[u8]> {
    fields
        .iter()
        .find(|(name, _)| name == b":status")
        .map(|(_, value)| value.as_slice())
}

/// Whether every stream of `streams` got a decoded `:status: 200`.
fn all_ok(responses: &[(u32, Vec<Field>)], streams: &[u32]) -> bool {
    streams.iter().all(|sid| {
        responses
            .iter()
            .any(|(s, fields)| s == sid && status_of(fields) == Some(b"200"))
    })
}

fn front_addr(front_port: u16) -> SocketAddr {
    format!("127.0.0.1:{front_port}").parse().unwrap()
}

// ============================================================================
// Many indexed fields
// ============================================================================

/// One block of 4 pseudo-headers and 100 one-octet indexed fields
/// (`accept-encoding: gzip, deflate`, static index 16), under the
/// `h2_max_header_fields` cap of 128.
fn try_h2_hpack_many_indexed_fields() -> State {
    let (worker, backends, front_port) = setup_h2_test("H2-HPACK-INDEXED", 1);
    let mut tls = raw_h2_connection(front_addr(front_port));
    h2_handshake(&mut tls);

    let mut encoder = Encoder::new();
    let mut block = request_block(&mut encoder, Representation::Proxy, false, &[]);
    let before = block.len();
    for _ in 0..100 {
        encoder.encode_header_into((b"accept-encoding", b"gzip, deflate"), &mut block);
    }
    let indexed = block.len() - before == 100;

    let sent = send_block(&mut tls, 1, &block, block.len());
    let frames = read_until_done(&mut tls, &[1], Duration::from_secs(5));
    log_frames("HPACK many indexed", &frames);
    let decoded = decode_responses(&mut Decoder::new(), &frames);
    let ok = matches!(&decoded, Ok(responses) if all_ok(responses, &[1]));

    let infra_ok = teardown(tls, front_port, worker, backends);
    println!("HPACK many indexed - indexed={indexed} sent={sent} ok={ok} decoded={decoded:?}");
    if infra_ok && indexed && sent && ok {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_hpack_many_indexed_fields() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "HPACK: one block of 100 indexed fields is decoded and routed",
            try_h2_hpack_many_indexed_fields,
        ),
        State::Success,
    );
}

// ============================================================================
// Dynamic table reused across requests, blocks split in fragments
// ============================================================================

/// Ten requests on one connection carry the same five custom fields. The
/// first block inserts them into both dynamic tables; every later block
/// names them with one-octet indexed references. Each block is split into a
/// HEADERS frame and CONTINUATION frames at a different boundary. Sōzu's
/// responses are decoded with one decoder for the whole connection, so its
/// own dynamic table must stay in step too.
fn try_h2_hpack_dynamic_table_reuse() -> State {
    let (worker, backends, front_port) = setup_h2_test("H2-HPACK-REUSE", 1);
    let mut tls = raw_h2_connection(front_addr(front_port));
    h2_handshake(&mut tls);

    let extra: [(&[u8], &[u8]); 5] = [
        (b"x-hpack-a", b"first custom value"),
        (b"x-hpack-b", b"second custom value"),
        (b"x-hpack-c", b"third"),
        (b"x-hpack-d", b"fourth custom value, longer than the others"),
        (b"x-hpack-e", b"fifth"),
    ];
    let mut encoder = Encoder::new();
    let streams: Vec<u32> = (0..10).map(|n| 1 + 2 * n).collect();
    let mut sizes = Vec::new();
    let mut sent = true;
    for (n, sid) in streams.iter().enumerate() {
        let block = request_block(&mut encoder, Representation::Proxy, false, &extra);
        sizes.push(block.len());
        // At most nine frames per block, under the CONTINUATION flood cap of
        // 20 (`H2FloodDetector`), each block cut at a different boundary.
        sent &= send_block(&mut tls, *sid, &block, block.len() / 8 + 1 + n);
    }
    // After the first block, the five custom fields cost one octet each.
    let reused = sizes[1..].iter().all(|&size| size < sizes[0] / 4);

    let frames = read_until_done(&mut tls, &streams, Duration::from_secs(8));
    log_frames("HPACK reuse", &frames);
    let decoded = decode_responses(&mut Decoder::new(), &frames);
    let ok = matches!(&decoded, Ok(responses) if all_ok(responses, &streams));

    let infra_ok = teardown(tls, front_port, worker, backends);
    println!("HPACK reuse - sizes={sizes:?} reused={reused} sent={sent} ok={ok}");
    if let Err(error) = &decoded {
        println!("HPACK reuse - response decoding failed: {error:?}");
    }
    if infra_ok && reused && sent && ok {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_hpack_dynamic_table_reuse() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "HPACK: the dynamic table is reused across requests in fragmented blocks",
            try_h2_hpack_dynamic_table_reuse,
        ),
        State::Success,
    );
}

// ============================================================================
// SETTINGS_HEADER_TABLE_SIZE changes
// ============================================================================

/// The client lowers `SETTINGS_HEADER_TABLE_SIZE` to 128, then to 0: Sōzu's
/// next response block after each change must open with a dynamic table size
/// update carrying the new size (RFC 7541 §4.2, §6.3), and a decoder bound
/// to that size must decode every response. The client's own blocks open
/// with size updates too — two of them, the most §4.2 allows — shrinking
/// then growing the table Sōzu decodes with.
fn try_h2_hpack_table_size_changes() -> State {
    let (worker, backends, front_port) = setup_h2_test("H2-HPACK-TABLE-SIZE", 1);
    let mut tls = raw_h2_connection(front_addr(front_port));
    h2_handshake(&mut tls);

    let mut encoder = Encoder::new();
    let mut decoder = Decoder::new();
    let mut ok = true;
    let mut sid = 1;
    for size in [128u32, 0] {
        let settings = H2Frame::settings(&[(SETTINGS_HEADER_TABLE_SIZE, size)]);
        ok &= tls.write_all(&settings.encode()).is_ok() && tls.flush().is_ok();
        decoder.set_max_allowed_table_size(size as usize);

        let mut block = Vec::new();
        // Shrink the table Sōzu decodes with to 0, then set it to 1024.
        encoder.set_max_table_size(0);
        encode_integer(0, 5, 0x20, &mut block);
        encoder.set_max_table_size(1024);
        encode_integer(1024, 5, 0x20, &mut block);
        block.extend(request_block(
            &mut encoder,
            Representation::IncrementalIndexing,
            false,
            &[(b"x-hpack-size", b"after a size update")],
        ));
        ok &= send_block(&mut tls, sid, &block, block.len());

        let frames = read_until_done(&mut tls, &[sid], Duration::from_secs(5));
        log_frames("HPACK table size", &frames);
        let first_block = frames
            .iter()
            .find(|(ft, _, s, _)| *ft == H2_FRAME_HEADERS && *s == sid)
            .map(|(.., payload)| payload.clone())
            .unwrap_or_default();
        let mut announced = Vec::new();
        encode_integer(size as usize, 5, 0x20, &mut announced);
        let opens_with_update = first_block.starts_with(&announced);
        let decoded = decode_responses(&mut decoder, &frames);
        let routed = matches!(&decoded, Ok(responses) if all_ok(responses, &[sid]));
        println!(
            "HPACK table size {size} - opens_with_update={opens_with_update} routed={routed} \
             decoded={decoded:?}"
        );
        ok &= opens_with_update && routed;
        sid += 2;
    }

    let infra_ok = teardown(tls, front_port, worker, backends);
    if infra_ok && ok {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_hpack_table_size_changes() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "HPACK: SETTINGS_HEADER_TABLE_SIZE changes are signalled and honoured",
            try_h2_hpack_table_size_changes,
        ),
        State::Success,
    );
}

/// Issue #1622: the client lowers `SETTINGS_HEADER_TABLE_SIZE` to 0, then
/// raises it back to 4096, in two SETTINGS frames sent before its request.
/// Sōzu's response block must open with two size updates, the smallest size
/// reached (0), then the last (4096) — RFC 7541 §4.2 — and a second response
/// on the connection must still decode. Before the fix the block opened with
/// 4096 alone.
fn try_h2_hpack_table_size_lowered_then_raised() -> State {
    let (worker, backends, front_port) = setup_h2_test("H2-HPACK-TABLE-SIZE-MIN", 1);
    let mut tls = raw_h2_connection(front_addr(front_port));
    h2_handshake(&mut tls);

    let mut encoder = Encoder::new();
    let mut decoder = Decoder::new();
    decoder.set_max_allowed_table_size(4096);
    let mut ok = true;
    for size in [0u32, 4096] {
        let settings = H2Frame::settings(&[(SETTINGS_HEADER_TABLE_SIZE, size)]);
        ok &= tls.write_all(&settings.encode()).is_ok() && tls.flush().is_ok();
    }

    let mut expected_prefix = Vec::new();
    encode_integer(0, 5, 0x20, &mut expected_prefix);
    encode_integer(4096, 5, 0x20, &mut expected_prefix);
    for (sid, expects_updates) in [(1u32, true), (3, false)] {
        let block = request_block(&mut encoder, Representation::Proxy, false, &[]);
        ok &= send_block(&mut tls, sid, &block, block.len());
        let frames = read_until_done(&mut tls, &[sid], Duration::from_secs(5));
        log_frames("HPACK table size lowered then raised", &frames);
        let first_block = frames
            .iter()
            .find(|(ft, _, s, _)| *ft == H2_FRAME_HEADERS && *s == sid)
            .map(|(.., payload)| payload.clone())
            .unwrap_or_default();
        let opens_with_updates = first_block.starts_with(&expected_prefix);
        let opens_with_any_update = first_block.first().is_some_and(|b| b & 0xe0 == 0x20);
        let decoded = decode_responses(&mut decoder, &frames);
        let routed = matches!(&decoded, Ok(responses) if all_ok(responses, &[sid]));
        println!(
            "HPACK table size 0 then 4096, stream {sid} - opens_with_updates={opens_with_updates} \
             opens_with_any_update={opens_with_any_update} routed={routed} decoded={decoded:?}"
        );
        ok &= routed
            && if expects_updates {
                opens_with_updates
            } else {
                !opens_with_any_update
            };
    }

    let infra_ok = teardown(tls, front_port, worker, backends);
    if infra_ok && ok {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_hpack_table_size_lowered_then_raised() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "HPACK: a table size lowered then raised is signalled as both sizes",
            try_h2_hpack_table_size_lowered_then_raised,
        ),
        State::Success,
    );
}

// ============================================================================
// Huffman and raw strings
// ============================================================================

/// Four requests on one connection: every string Huffman-coded or none,
/// indexed incrementally or never indexed. `:authority` and `:path` are
/// coded too, so a Huffman string Sōzu decoded wrongly would not route.
fn try_h2_hpack_huffman_and_raw() -> State {
    let (worker, backends, front_port) = setup_h2_test("H2-HPACK-HUFFMAN", 1);
    let mut tls = raw_h2_connection(front_addr(front_port));
    h2_handshake(&mut tls);

    let mut encoder = Encoder::new();
    let mut sent = true;
    let cases = [
        (Representation::IncrementalIndexing, true),
        (Representation::IncrementalIndexing, false),
        (Representation::NeverIndexed, true),
        (Representation::WithoutIndexing, false),
    ];
    let streams = [1, 3, 5, 7];
    for ((representation, huffman), sid) in cases.into_iter().zip(streams) {
        let block = request_block(
            &mut encoder,
            representation,
            huffman,
            &[
                (b"user-agent", b"sozu-e2e/hpack (Huffman-coded or raw)"),
                (b"x-hpack-bytes", b"~!@#$%^&*()_+{}|:<>?`-=[];',./"),
            ],
        );
        sent &= send_block(&mut tls, sid, &block, block.len());
    }

    let frames = read_until_done(&mut tls, &streams, Duration::from_secs(5));
    log_frames("HPACK Huffman", &frames);
    let decoded = decode_responses(&mut Decoder::new(), &frames);
    let ok = matches!(&decoded, Ok(responses) if all_ok(responses, &streams));

    let infra_ok = teardown(tls, front_port, worker, backends);
    println!("HPACK Huffman - sent={sent} ok={ok} decoded={decoded:?}");
    if infra_ok && sent && ok {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_hpack_huffman_and_raw() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "HPACK: Huffman-coded and raw strings are decoded and routed",
            try_h2_hpack_huffman_and_raw,
        ),
        State::Success,
    );
}

// ============================================================================
// Header lists at the size limit
// ============================================================================

/// Stream 1 carries a header list one field past `MAX_HEADER_LIST_SIZE`: a
/// 4000-octet value inserted into the dynamic table, then referenced by
/// one-octet indexed fields. Sōzu refuses the stream with a RST_STREAM and
/// keeps the connection. Which limit trips first is the stream buffer: its
/// 16 KiB fill after four decoded copies, `decode_headers_with_budget`
/// (`lib/src/protocol/mux/pkawa.rs`) marks the headers invalid and stops
/// counting, so the RST carries PROTOCOL_ERROR rather than the
/// ENHANCE_YOUR_CALM of the 64 KiB budget — measured, and accepted here
/// either way. Sōzu must still decode the whole block, because the block
/// also inserted `:authority: localhost` into its dynamic table. Stream 3
/// then names that `:authority` by its dynamic index only, with a
/// 6000-octet Huffman-coded value: it routes only if Sōzu's decoder stayed
/// in step.
fn try_h2_hpack_header_list_at_the_limit() -> State {
    let (worker, backends, front_port) = setup_h2_test("H2-HPACK-LIMIT", 1);
    let mut tls = raw_h2_connection(front_addr(front_port));
    h2_handshake(&mut tls);

    let mut encoder = Encoder::new();
    let big = vec![b'b'; 4000];
    let mut block = Vec::new();
    for (name, value) in [
        (&b":method"[..], &b"GET"[..]),
        (b":scheme", b"https"),
        (b":path", b"/"),
    ] {
        encoder.encode_header_into((name, value), &mut block);
    }
    encoder.encode_field(
        b":authority",
        b"localhost",
        Representation::IncrementalIndexing,
        false,
        &mut block,
    );
    let mut list_size: usize = [(7, 3), (7, 5), (5, 1), (10, 9)]
        .iter()
        .map(|(name, value)| name + value + FIELD_OVERHEAD)
        .sum();
    let big_field = b"x-big".len() + big.len() + FIELD_OVERHEAD;
    while list_size <= MAX_HEADER_LIST_SIZE {
        encoder.encode_header_into((b"x-big", &big), &mut block);
        list_size += big_field;
    }
    let over_by = list_size - MAX_HEADER_LIST_SIZE;
    let sent_over = send_block(&mut tls, 1, &block, 16_000);
    let frames = read_until_done(&mut tls, &[1], Duration::from_secs(5));
    log_frames("HPACK over the limit", &frames);
    let refused = contains_rst_stream_with_error(&frames, 1, H2_ERROR_ENHANCE_YOUR_CALM)
        || contains_rst_stream_with_error(&frames, 1, H2_ERROR_PROTOCOL_ERROR);
    let kept = goaway_error_code(&frames).is_none();

    let huge = vec![b'a'; 6000];
    let mut block = Vec::new();
    for (name, value) in [
        (&b":method"[..], &b"GET"[..]),
        (b":scheme", b"https"),
        (b":authority", b"localhost"),
        (b":path", b"/"),
    ] {
        encoder.encode_header_into((name, value), &mut block);
    }
    let authority_indexed = block.contains(&0xbf) || block.contains(&0xbe);
    encoder.encode_field(
        b"x-huge",
        &huge,
        Representation::WithoutIndexing,
        true,
        &mut block,
    );
    let sent_next = send_block(&mut tls, 3, &block, block.len());
    let frames = read_until_done(&mut tls, &[3], Duration::from_secs(5));
    log_frames("HPACK after the limit", &frames);
    let mut decoder = Decoder::new();
    let decoded = decode_responses(&mut decoder, &frames);
    let routed = matches!(&decoded, Ok(responses) if all_ok(responses, &[3]));

    let infra_ok = teardown(tls, front_port, worker, backends);
    println!(
        "HPACK limit - over_by={over_by} sent_over={sent_over} refused={refused} kept={kept} \
         authority_indexed={authority_indexed} sent_next={sent_next} routed={routed}"
    );
    if infra_ok && sent_over && refused && kept && authority_indexed && sent_next && routed {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_hpack_header_list_at_the_limit() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "HPACK: a header list past the limit is refused and the decoder stays in step",
            try_h2_hpack_header_list_at_the_limit,
        ),
        State::Success,
    );
}

// ============================================================================
// Invalid blocks: COMPRESSION_ERROR
// ============================================================================

/// Sends `block` on stream 1 of a fresh connection and expects a GOAWAY with
/// COMPRESSION_ERROR and no response on the stream.
fn expect_compression_error(label: &str, block: Vec<u8>) -> State {
    let (worker, backends, front_port) = setup_h2_test(label, 1);
    let mut tls = raw_h2_connection(front_addr(front_port));
    h2_handshake(&mut tls);

    let sent = send_block(&mut tls, 1, &block, block.len());
    let frames = read_until_done(&mut tls, &[1], Duration::from_secs(5));
    log_frames(label, &frames);
    let error = goaway_error_code(&frames);
    let answered = frames
        .iter()
        .any(|(ft, _, sid, _)| *ft == H2_FRAME_HEADERS && *sid == 1);

    let infra_ok = teardown(tls, front_port, worker, backends);
    println!("{label} - sent={sent} goaway={error:?} answered={answered}");
    if infra_ok && sent && error == Some(H2_ERROR_COMPRESSION_ERROR) && !answered {
        State::Success
    } else {
        State::Fail
    }
}

/// The request pseudo-headers followed by `tail`.
fn valid_prefix_then(tail: &[u8]) -> Vec<u8> {
    let mut block = request_block(&mut Encoder::new(), Representation::Proxy, false, &[]);
    block.extend_from_slice(tail);
    block
}

fn try_h2_hpack_invalid_blocks() -> State {
    let cases: [(&str, Vec<u8>); 6] = [
        (
            "H2-HPACK-EOS",
            // Literal without indexing, name `user-agent` (58), Huffman value
            // of four all-ones octets: EOS inside the string.
            valid_prefix_then(&[0x0f, 58 - 15, 0x84, 0xff, 0xff, 0xff, 0xff]),
        ),
        (
            "H2-HPACK-PADDING",
            // Huffman value 0xff: eight bits of padding.
            valid_prefix_then(&[0x0f, 58 - 15, 0x81, 0xff]),
        ),
        (
            "H2-HPACK-INDEX",
            // Indexed field 62 on an empty dynamic table.
            valid_prefix_then(&[0xbe]),
        ),
        (
            "H2-HPACK-SIZE-UPDATE-LATE",
            // A size update after the first field (RFC 7541 §4.2).
            valid_prefix_then(&[0x20]),
        ),
        (
            "H2-HPACK-SIZE-UPDATE-TOO-LARGE",
            // A size update to 4097, above the 4096 Sōzu advertises.
            {
                let mut block = Vec::new();
                encode_integer(4097, 5, 0x20, &mut block);
                block.extend(valid_prefix_then(&[]));
                block
            },
        ),
        (
            "H2-HPACK-TRUNCATED",
            // A literal whose value length runs past the block.
            valid_prefix_then(&[0x0f, 58 - 15, 0x10, b'a']),
        ),
    ];
    for (label, block) in cases {
        if expect_compression_error(label, block) != State::Success {
            return State::Fail;
        }
    }
    State::Success
}

#[test]
fn test_h2_hpack_invalid_blocks_are_compression_errors() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "HPACK: every invalid block is a GOAWAY with COMPRESSION_ERROR",
            try_h2_hpack_invalid_blocks,
        ),
        State::Success,
    );
}
