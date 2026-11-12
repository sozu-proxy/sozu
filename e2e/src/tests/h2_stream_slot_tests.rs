//! End-to-end regression guards for the stream slot a frontend H2 stream
//! reset by Sōzu must give back.
//!
//! `ConnectionH2::reset_stream` (`lib/src/protocol/mux/h2.rs`) answers a
//! stream error the client provoked with RST_STREAM, and its callers then
//! drop the stream's wire id. Before the fix the slot was left
//! `StreamState::Unlinked` off the stream table: nothing recycled it while
//! the connection lived, `Context::create_stream` never reused it, and it
//! pinned its two pool buffers until the session closed. With the default
//! buffer pool (`max_buffers = 1000`) about 499 such resets on one
//! connection emptied a worker's pool, and the worker refused new streams
//! for want of buffers, on that connection and on a new one alike.
//!
//! * [`test_h2_malformed_requests_release_their_stream_slots`] — streams
//!   reset at their HEADERS (`StreamState::Idle`): a malformed `:path`.
//! * [`test_h2_content_length_resets_release_their_stream_slots`] — streams
//!   reset once a backend answered them (`StreamState::Linked`): a request
//!   body shorter than its `content-length`.
//!
//! Each sends more resets on one connection than the pool can hold
//! orphaned slots for, then asks for a valid request on that connection and
//! on a second one, and both must be served.

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

use super::h2_utils::{
    H2_ERROR_PROTOCOL_ERROR, H2_ERROR_REFUSED_STREAM, H2_FLAG_END_STREAM, H2_FRAME_GOAWAY,
    H2_FRAME_HEADERS, H2_FRAME_RST_STREAM, H2Frame, advance_one_frame, decode_status,
    extract_rst_streams, h2_handshake, raw_h2_connection, setup_h2_test, teardown,
};
use crate::tests::{State, repeat_until_error_or};

type TlsStream = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;
type Frame = (u8, u8, u32, Vec<u8>);

/// More resets than a 1000-buffer pool holds orphaned slots for (two
/// buffers each, so about 499 with the connection's own buffer), and few
/// enough that no limit a provoked reset counts against ends the connection
/// first (`ConnectionH2::account_emitted_rst`, `lib/src/protocol/mux/h2.rs`):
/// the emitted RST_STREAM floor, 10 000 by default
/// (`max_rst_stream_emitted_lifetime`); the glitch budget, 2000 per window
/// (`max_glitch_count`), which each provoked reset charges one glitch; and the
/// pending RST_STREAM queue bound, at least `MIN_PENDING_RST_STREAMS` (4000,
/// `lib/src/protocol/mux/h2_control_tx.rs`).
const RESETS: u32 = 600;

/// How long one wait on the frontend may last.
const DEADLINE: Duration = Duration::from_secs(10);

/// Frames read from one connection, with the bytes of a frame not read
/// whole yet.
struct FrameReader {
    carry: Vec<u8>,
    frames: Vec<Frame>,
}

impl FrameReader {
    fn new(tls: &mut TlsStream) -> Self {
        // Each read waits on the socket at most this long, so the loop below
        // waits on the frontend, never on a sleep.
        tls.sock
            .set_read_timeout(Some(Duration::from_millis(250)))
            .expect("set read timeout");
        Self {
            carry: Vec::new(),
            frames: Vec::new(),
        }
    }

    /// Read until `done` holds over every frame read so far, the connection
    /// ends, or [`DEADLINE`] passes. Answers whether `done` holds.
    fn read_until(&mut self, tls: &mut TlsStream, done: impl Fn(&[Frame]) -> bool) -> bool {
        let start = Instant::now();
        let mut buf = vec![0u8; 64 * 1024];
        while !done(&self.frames) {
            if start.elapsed() >= DEADLINE {
                return false;
            }
            match tls.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    self.carry.extend_from_slice(&buf[..n]);
                    while let Some(frame) = advance_one_frame(&mut self.carry) {
                        self.frames.push(frame);
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => break,
            }
        }
        done(&self.frames)
    }
}

/// `:method GET`, `:path /`, `:scheme https`, then `:authority localhost`
/// as a literal without indexing: no HPACK dynamic table state.
fn get_block() -> Vec<u8> {
    let mut block = vec![0x82, 0x84, 0x87, 0x01, 0x09];
    block.extend_from_slice(b"localhost");
    block
}

/// The same request with `:path /a#b` as a literal without indexing: a
/// fragment in `:path` makes it malformed (RFC 9113 §8.3.1, RFC 9112
/// §3.2), a stream error answered with RST_STREAM(PROTOCOL_ERROR).
fn malformed_get_block() -> Vec<u8> {
    let mut block = vec![0x82, 0x04, 0x04];
    block.extend_from_slice(b"/a#b");
    block.extend_from_slice(&[0x87, 0x01, 0x09]);
    block.extend_from_slice(b"localhost");
    block
}

/// `:method POST`, `:path /`, `:scheme https`, `:authority localhost` and
/// `content-length: 5`, every literal without indexing.
fn post_with_length_five_block() -> Vec<u8> {
    let mut block = vec![0x83, 0x84, 0x87, 0x01, 0x09];
    block.extend_from_slice(b"localhost");
    block.extend_from_slice(&[0x0f, 0x0d, 0x01, b'5']);
    block
}

/// The `:status` of the first HEADERS frame on `stream_id`.
fn status_on(frames: &[Frame], stream_id: u32) -> Option<u16> {
    frames
        .iter()
        .find(|(kind, _, id, _)| *kind == H2_FRAME_HEADERS && *id == stream_id)
        .and_then(|(_, _, _, payload)| decode_status(payload))
}

/// Whether `stream_id` got its response HEADERS or a RST_STREAM, or the
/// connection a GOAWAY.
fn answered(frames: &[Frame], stream_id: u32) -> bool {
    frames.iter().any(|(kind, _, id, _)| {
        *kind == H2_FRAME_GOAWAY
            || (*id == stream_id && (*kind == H2_FRAME_HEADERS || *kind == H2_FRAME_RST_STREAM))
    })
}

/// Send a GET on `stream_id` and answer its `:status`, or `None` when the
/// stream was reset or the connection ended first.
fn get_status(tls: &mut TlsStream, reader: &mut FrameReader, stream_id: u32) -> Option<u16> {
    tls.write_all(&H2Frame::headers(stream_id, get_block(), true, true).encode())
        .ok()?;
    tls.flush().ok()?;
    reader.read_until(tls, |frames| answered(frames, stream_id));
    status_on(&reader.frames, stream_id)
}

/// A valid GET on a second connection, on stream 1.
fn second_connection_status(front_addr: SocketAddr) -> Option<u16> {
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);
    let mut reader = FrameReader::new(&mut tls);
    get_status(&mut tls, &mut reader, 1)
}

/// RST_STREAM error codes on the streams `1, 3, .., 2 * RESETS - 1`, by
/// code: `(protocol_error, refused_stream, other)`, and the GOAWAY count.
fn reset_census(frames: &[Frame]) -> ((usize, usize, usize), usize) {
    let mut codes = (0, 0, 0);
    for (stream_id, code) in extract_rst_streams(frames) {
        if stream_id % 2 == 0 || stream_id >= 2 * RESETS {
            continue;
        }
        match code {
            H2_ERROR_PROTOCOL_ERROR => codes.0 += 1,
            H2_ERROR_REFUSED_STREAM => codes.1 += 1,
            _ => codes.2 += 1,
        }
    }
    let goaways = frames
        .iter()
        .filter(|(kind, _, _, _)| *kind == H2_FRAME_GOAWAY)
        .count();
    (codes, goaways)
}

// ============================================================================
// Streams reset at their HEADERS
// ============================================================================

/// [`RESETS`] malformed requests on one connection, all sent at once: each
/// is reset with PROTOCOL_ERROR while its slot is still `Idle`, and none
/// may be refused for want of buffers. A valid request is then served on
/// that connection and on a second one.
fn try_h2_malformed_requests_release_their_stream_slots() -> State {
    let (worker, backends, front_port) = setup_h2_test("H2-SLOT-RECYCLE-MALFORMED", 1);
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);
    let mut reader = FrameReader::new(&mut tls);

    let mut burst = Vec::new();
    for i in 0..RESETS {
        burst.extend(H2Frame::headers(2 * i + 1, malformed_get_block(), true, true).encode());
    }
    tls.write_all(&burst).expect("the burst must be written");
    tls.flush().expect("the burst must be flushed");
    let all_reset = reader.read_until(&mut tls, |frames| {
        let ((protocol, refused, other), goaways) = reset_census(frames);
        protocol + refused + other == RESETS as usize || goaways > 0
    });
    let ((protocol, refused, other), goaways) = reset_census(&reader.frames);

    let first_status = get_status(&mut tls, &mut reader, 2 * RESETS + 1);
    let second_status = second_connection_status(front_addr);

    println!(
        "malformed requests: all_reset={all_reset} protocol_error={protocol} \
         refused_stream={refused} other={other} goaways={goaways} \
         first_connection_status={first_status:?} second_connection_status={second_status:?}"
    );
    let infra_ok = teardown(tls, front_port, worker, backends);
    if infra_ok
        && all_reset
        && (protocol, refused, other, goaways) == (RESETS as usize, 0, 0, 0)
        && first_status == Some(200)
        && second_status == Some(200)
    {
        State::Success
    } else {
        println!("malformed requests - FAIL: infra_ok={infra_ok}");
        State::Fail
    }
}

#[test]
fn test_h2_malformed_requests_release_their_stream_slots() {
    assert_eq!(
        repeat_until_error_or(
            1,
            "H2: streams reset at their malformed HEADERS give their slot back",
            try_h2_malformed_requests_release_their_stream_slots,
        ),
        State::Success,
    );
}

// ============================================================================
// Streams reset once a backend answered them
// ============================================================================

/// [`RESETS`] requests on one connection, one at a time: a POST announcing
/// `content-length: 5`, whose head the backend answers with a 200 at once
/// (`AsyncBackend::http_handler` answers its first read), which proves the
/// stream was linked to it; then a 3-byte DATA frame with END_STREAM, a
/// Content-Length mismatch (RFC 9113 §8.1.1) Sōzu resets with
/// PROTOCOL_ERROR. A valid request is then served on that connection and on
/// a second one.
fn try_h2_content_length_resets_release_their_stream_slots() -> State {
    let (worker, backends, front_port) = setup_h2_test("H2-SLOT-RECYCLE-CONTENT-LENGTH", 1);
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);
    let mut reader = FrameReader::new(&mut tls);

    // (index, what the stream got instead) at the first stream that was not
    // answered 200 then reset with PROTOCOL_ERROR.
    let mut first_failure = None;
    for i in 0..RESETS {
        let stream_id = 2 * i + 1;
        let head = H2Frame::headers(stream_id, post_with_length_five_block(), true, false);
        if tls.write_all(&head.encode()).is_err() || tls.flush().is_err() {
            first_failure = Some((i, "the request head could not be written".to_owned()));
            break;
        }
        reader.read_until(&mut tls, |frames| answered(frames, stream_id));
        let status = status_on(&reader.frames, stream_id);
        if status != Some(200) {
            first_failure = Some((i, format!("status {status:?} instead of 200")));
            break;
        }
        let short_body = H2Frame::new(0x0, H2_FLAG_END_STREAM, stream_id, b"abc".to_vec());
        if tls.write_all(&short_body.encode()).is_err() || tls.flush().is_err() {
            first_failure = Some((i, "the short body could not be written".to_owned()));
            break;
        }
        let reset = reader.read_until(&mut tls, |frames| {
            frames.iter().any(|(kind, _, id, _)| {
                *kind == H2_FRAME_GOAWAY || (*kind == H2_FRAME_RST_STREAM && *id == stream_id)
            })
        });
        let codes: Vec<u32> = extract_rst_streams(&reader.frames)
            .into_iter()
            .filter(|(id, _)| *id == stream_id)
            .map(|(_, code)| code)
            .collect();
        if !reset || codes != [H2_ERROR_PROTOCOL_ERROR] {
            first_failure = Some((
                i,
                format!("reset codes {codes:?} instead of PROTOCOL_ERROR"),
            ));
            break;
        }
    }
    let ((protocol, refused, other), goaways) = reset_census(&reader.frames);

    let first_status = get_status(&mut tls, &mut reader, 2 * RESETS + 1);
    let second_status = second_connection_status(front_addr);

    println!(
        "content-length resets: first_failure={first_failure:?} protocol_error={protocol} \
         refused_stream={refused} other={other} goaways={goaways} \
         first_connection_status={first_status:?} second_connection_status={second_status:?}"
    );
    let infra_ok = teardown(tls, front_port, worker, backends);
    if infra_ok
        && first_failure.is_none()
        && (protocol, refused, other, goaways) == (RESETS as usize, 0, 0, 0)
        && first_status == Some(200)
        && second_status == Some(200)
    {
        State::Success
    } else {
        println!("content-length resets - FAIL: infra_ok={infra_ok}");
        State::Fail
    }
}

#[test]
fn test_h2_content_length_resets_release_their_stream_slots() {
    assert_eq!(
        repeat_until_error_or(
            1,
            "H2: streams reset for their Content-Length once linked give their slot back",
            try_h2_content_length_resets_release_their_stream_slots,
        ),
        State::Success,
    );
}
