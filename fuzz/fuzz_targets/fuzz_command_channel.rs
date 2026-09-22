#![no_main]
//! Fuzz target for the command channel's length-delimited IPC framing
//! (`sozu_command_lib::channel::Channel`, `command/src/channel.rs`).
//!
//! Drives the sans-io parsing core -- `Channel::try_read_delimited_message`
//! -- through its only entry point reachable from outside the crate: the
//! public, purely in-memory `read_message()`, called after bytes are placed
//! directly into the public `front_buf` field. It also drives the sans-io
//! framing core on the write side, `write_delimited_message`, which only
//! ever touches `back_buf`. Neither entry point performs a socket read or
//! write on the fuzzed data path -- the `MioUnixStream` pair
//! `Channel::generate_nonblocking` requires exists only to satisfy the
//! field's type; its file descriptors are never read from or written to
//! here. This mirrors the boundary the house sans-io pattern already draws
//! in `channel.rs` itself: `readable()`/`writable()` are the I/O shell,
//! `try_read_delimited_message()`/`write_delimited_message()` are the pure
//! core.
//!
//! Grammar (big-endian `Reader` over the fuzz input, mirrors
//! `fuzz_udp_flow.rs` / `fuzz_tcp_clienthello.rs`):
//! 1. A "reader" `Channel<WorkerResponse, WorkerRequest>` with a small,
//!    fuzz-chosen `buffer_size` / `max_buffer_size` (so `MessageTooLarge`
//!    and `BufferFull` are cheaply reachable), and a "writer"
//!    `Channel<WorkerRequest, WorkerResponse>` with a fixed, generous cap
//!    (so it always succeeds and produces a real, valid framed
//!    `WorkerRequest` on demand) -- the same `Channel<WorkerRequest,
//!    WorkerResponse>` / `Channel<WorkerResponse, WorkerRequest>` pairing
//!    convention `bin/src/command/server.rs`'s tests use for the master /
//!    worker sides.
//! 2. A bounded (<= 512 iterations) step loop that, each step, either:
//!    - encodes a real `WorkerRequest` via the writer's
//!      `write_delimited_message` and feeds the resulting wire bytes into
//!      the reader whole or split across 2-3 arbitrary chunk boundaries;
//!    - encodes a real frame and immediately appends raw garbage bytes
//!      taken directly from the fuzz input before feeding it;
//!    - feeds a raw adversarial chunk taken directly from the remaining
//!      fuzz bytes (so libFuzzer's coverage-guided mutation shapes the
//!      wire bytes the parser actually sees);
//!    - crafts an explicit little-endian `usize` delimiter -- drawn from a
//!      pool biased toward the interesting boundaries (`0`, `1`,
//!      `delimiter_size() - 1`, `delimiter_size()`, `delimiter_size() + 1`,
//!      `max_buffer_size`, `max_buffer_size + 1`, `usize::MAX`, and a
//!      fuzz-chosen arbitrary value) -- followed by a fuzz-chosen amount of
//!      trailing bytes, which may be shorter or longer than the declared
//!      length.
//!    After every feed, `read_message()` is drained (up to 8 times, so
//!    several frames queued by one feed all get exercised) until it stops
//!    making progress.
//!
//! Invariants asserted beyond "never panic" (see `check_read`):
//! - a single `read_message()` call never *increases* the front buffer's
//!   pending data (it can only consume, or leave it unchanged);
//! - a successful decode (`Ok(_)`) consumes EXACTLY the declared frame
//!   length (delimiter + payload) -- peeked independently before the call
//!   with the same `delimiter_size()` / little-endian decode the
//!   production parser uses. This half is partly tautological: a defect
//!   *inside* `delimiter_size()` or the `from_le_bytes` decode itself would
//!   be invisible to it, since the peek and the parser share the same
//!   primitives. The byte-accounting half below does not share that
//!   blind spot;
//! - `MessageLengthUnderDelimiter` -- the defect `5af7daea` fixed -- always
//!   drops exactly `delimiter_size()` bytes, so the channel can resync on
//!   the peer's next frame (the guarantee the fix's own doc comment and the
//!   `command_channel_security_tests.rs` e2e regression both name);
//! - `MessageTooLarge` -- the defect `7b8dce97` (LISA-011) fixed -- never
//!   consumes any bytes AND never grows `front_buf`'s capacity: the oversize
//!   guard must fire before any allocation pressure, not after;
//! - `NothingRead` (insufficient data yet) never consumes pending bytes;
//! - a global "byte conservation" property tying all of the above together:
//!   at every step, `front_buf.available_data() == total_fed -
//!   total_consumed`, where `total_fed` is every byte this harness ever
//!   placed into `front_buf` and `total_consumed` is the sum of the exact
//!   per-call deltas above -- both counted independently by the harness
//!   itself, not read back from the code under test -- so no byte is ever
//!   silently created or dropped outside an accounted-for `consume()`,
//!   including one hidden by a `delimiter_size()`/decode defect the
//!   previous bullet's peek could not see;
//! - a round trip: `write_delimited_message` followed by feeding the whole
//!   result into a fresh, generously-capped reader decodes back to a
//!   `WorkerRequest` whose `id` matches what was encoded.
//!
//! This does not depend on `debug_assert!` to find anything: `cargo-fuzz`
//! unconditionally builds with `-Cdebug-assertions` regardless of profile,
//! which is why a reintroduced defect here surfaces as the
//! `debug_assert!` at `command/src/channel.rs:589` rather than the raw
//! slice-index panic at `command/src/channel.rs:607`
//! (`Rx::decode(&buffer[delimiter_size()..message_len])`; see
//! `fuzz/README.md`'s `5af7daea` regression-seed entry for the
//! from-scratch `--release`-without-assertions confirmation that reaches
//! the latter instead). The underlying gap is a reachable-input DoS in
//! every build profile either way: the slice-range check is a
//! language-level memory-safety invariant, not a debug-only one.
//!
//! Not covered:
//! [`2c6832b9`](https://github.com/sozu-proxy/sozu/commit/2c6832b95bff8f8c12b408f4710ba51d9d26b8c3)
//! is an allocation-*amortization* change (an inner `min(..., needed)` cap
//! removed from the back-buffer doubling loop, same final ceiling either
//! way) with no effect on decode correctness or byte accounting, so no
//! external oracle here would distinguish pre- from post-fix. It remains
//! covered by `back_buffer_grows_with_doubling_on_write` in
//! `command/src/channel.rs`'s own unit tests.
//! [`7299c285`](https://github.com/sozu-proxy/sozu/commit/7299c285f12e733de8c835283358d74c07581995)
//! is NOT the same kind of gap: per its own commit message it "Closes #1050
//! where the channel silently stopped reading when the buffer was full" --
//! a real stuck-read correctness defect in `Channel::readable()`'s
//! socket-read loop, which used to give up instead of growing. This target
//! cannot reach it, but not because it was harmless: `readable()` is the
//! socket I/O shell the sans-io boundary above deliberately keeps this
//! target out of. `buffer_grows_with_doubling_strategy` only unit-tests
//! `grow_size()` in isolation, not `readable()`'s use of it, so that defect
//! class currently has no dedicated regression coverage at all -- whether
//! the fuzz target should grow a second, socket-facing harness to reach
//! `readable()` is an open question this target does not answer.
//!
//! Corpus + run instructions live in `fuzz/README.md`.

use libfuzzer_sys::fuzz_target;
use sozu_command_lib::{
    channel::{Channel, ChannelError, delimiter_size},
    proto::command::{WorkerRequest, WorkerResponse},
};

/// The "reader" side under test: reads `WorkerRequest`s, writes (unused)
/// `WorkerResponse`s.
type ReaderChannel = Channel<WorkerResponse, WorkerRequest>;
/// The "writer" side used only to produce real, valid wire frames: writes
/// `WorkerRequest`s, reads (unused) `WorkerResponse`s. Same pairing
/// convention as `bin/src/command/server.rs`'s `main_side` / `worker_side`.
type WriterChannel = Channel<WorkerRequest, WorkerResponse>;

/// Generous, fixed cap for the writer channel so `write_delimited_message`
/// always succeeds for the (short, `Reader::chunk`-bounded) ids this target
/// encodes -- the reader's cap is the one under adversarial test, not the
/// writer's.
const WRITER_BUFFER_SIZE: u64 = 64;
const WRITER_MAX_BUFFER_SIZE: u64 = 1 << 16;

/// A tiny big-endian byte reader over the fuzz input, mirroring
/// `fuzz_udp_flow.rs` / `fuzz_tcp_clienthello.rs`. Every getter returns a
/// default (0 / empty) when the input is exhausted, so the grammar degrades
/// gracefully on short inputs instead of branching on length everywhere.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Reader { data, pos: 0 }
    }

    fn is_empty(&self) -> bool {
        self.pos >= self.data.len()
    }

    fn u8(&mut self) -> u8 {
        let b = self.data.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        b
    }

    fn u16(&mut self) -> u16 {
        u16::from_be_bytes([self.u8(), self.u8()])
    }

    fn u32(&mut self) -> u32 {
        u32::from_be_bytes([self.u8(), self.u8(), self.u8(), self.u8()])
    }

    /// Read a length-prefixed slice (1-byte length, capped at the remaining
    /// input) taken DIRECTLY from the fuzz bytes -- not synthesized -- so
    /// the wire content fed to the channel is whatever libFuzzer's
    /// coverage-guided mutation puts there.
    fn chunk(&mut self) -> &'a [u8] {
        let len = self.u8() as usize;
        let start = self.pos.min(self.data.len());
        let end = (start + len).min(self.data.len());
        self.pos = end;
        &self.data[start..end]
    }
}

/// Copy `chunk` into `front_buf`'s free tail, growing it first -- mirroring
/// the doubling strategy `Channel::readable()` uses, but driven here
/// directly since the socket is bypassed -- so a short chunk is never
/// silently truncated by insufficient space alone. A chunk that still does
/// not fit once `front_buf` is at `max_buffer_size` is truncated to
/// whatever space remains, exactly as a real short `read()` off the wire
/// would be. Returns the number of bytes actually placed (the authoritative
/// "fed" count, straight from `Buffer::fill`'s own return value).
fn feed(channel: &mut ReaderChannel, max_buffer_size: usize, chunk: &[u8]) -> usize {
    if chunk.is_empty() {
        return 0;
    }
    while channel.front_buf.available_space() < chunk.len()
        && channel.front_buf.capacity() < max_buffer_size
    {
        let capacity = channel.front_buf.capacity();
        let next = capacity
            .saturating_mul(2)
            .max(capacity + 1)
            .min(max_buffer_size);
        if !channel.front_buf.grow(next) {
            break;
        }
    }
    let space = channel.front_buf.available_space();
    let n = chunk.len().min(space);
    if n == 0 {
        return 0;
    }
    channel.front_buf.space()[..n].copy_from_slice(&chunk[..n]);
    channel.front_buf.fill(n)
}

/// Encode one real `WorkerRequest { id, content: Request::default() }` via
/// the production `write_delimited_message` on the scratch writer channel,
/// then read the resulting wire bytes straight back out of its `back_buf`
/// -- never calling `prost::Message` ourselves. Returns the framed bytes
/// (delimiter + payload) and drains the writer's `back_buf` so the next
/// call starts clean. Returns the framed bytes' `id` alongside them so
/// callers can check the write/read round trip.
fn build_valid_frame(writer: &mut WriterChannel, id_bytes: &[u8]) -> (Vec<u8>, String) {
    let id = String::from_utf8_lossy(id_bytes).into_owned();
    let msg = WorkerRequest {
        id: id.clone(),
        content: Default::default(),
    };
    if writer.write_delimited_message(&msg).is_err() {
        // Should not happen given WRITER_MAX_BUFFER_SIZE and Reader::chunk's
        // 255-byte cap, but handled rather than assumed away.
        let pending = writer.back_buf.available_data();
        writer.back_buf.consume(pending);
        return (Vec::new(), id);
    }
    let framed = writer.back_buf.data().to_vec();
    writer.back_buf.consume(framed.len());
    (framed, id)
}

/// One `read_message()` call plus its property checks (see the module doc
/// comment for the full list and rationale). Returns whether the call made
/// progress -- decoded a frame or resynced past a bad delimiter -- so the
/// caller knows whether draining further is worthwhile.
fn check_read(reader: &mut ReaderChannel, total_fed: usize, total_consumed: &mut usize) -> bool {
    let available_before = reader.front_buf.available_data();
    let capacity_before = reader.front_buf.capacity();

    let declared_len: Option<usize> = if available_before >= delimiter_size() {
        let data = reader.front_buf.data();
        let mut raw = [0u8; std::mem::size_of::<usize>()];
        raw.copy_from_slice(&data[..delimiter_size()]);
        Some(usize::from_le_bytes(raw))
    } else {
        None
    };

    let outcome = reader.read_message();

    let available_after = reader.front_buf.available_data();
    assert!(
        available_after <= available_before,
        "read_message() must never increase pending front-buffer data: {available_before} -> {available_after}"
    );
    let consumed_this_call = available_before - available_after;

    let progressed = match &outcome {
        Ok(_) => {
            let declared = declared_len
                .expect("a successful decode requires a peeked delimiter to have been present");
            assert_eq!(
                consumed_this_call, declared,
                "successful decode consumed {consumed_this_call} bytes but the declared frame length was {declared}"
            );
            true
        }
        Err(ChannelError::MessageLengthUnderDelimiter {
            message_len,
            delimiter_size: ds,
        }) => {
            assert_eq!(
                consumed_this_call, *ds,
                "MessageLengthUnderDelimiter must drop exactly the {ds}-byte delimiter to resync \
                 (declared message_len={message_len}), got {consumed_this_call}"
            );
            true
        }
        Err(ChannelError::MessageTooLarge { .. }) => {
            assert_eq!(
                consumed_this_call, 0,
                "MessageTooLarge must not consume any bytes"
            );
            assert_eq!(
                reader.front_buf.capacity(),
                capacity_before,
                "MessageTooLarge must reject before growing front_buf's capacity"
            );
            false
        }
        Err(ChannelError::NothingRead) => {
            assert_eq!(
                consumed_this_call, 0,
                "NothingRead (insufficient data) must not consume pending bytes"
            );
            false
        }
        _ => false,
    };

    *total_consumed += consumed_this_call;
    assert!(
        *total_consumed <= total_fed,
        "consumed more bytes ({total_consumed}) than were ever fed ({total_fed})"
    );
    assert_eq!(
        reader.front_buf.available_data(),
        total_fed - *total_consumed,
        "byte conservation violated: available_data disagrees with total_fed - total_consumed"
    );

    progressed
}

/// Drain `read_message()` until it stops making progress, or a bound is hit
/// -- several frames queued by a single feed (a whole valid frame followed
/// by garbage that happens to look like another header, for instance) all
/// get exercised this way, mirroring how a real caller loops
/// (`Channel`'s own `Iterator` impl, or the worker event loop).
fn drain(reader: &mut ReaderChannel, total_fed: usize, total_consumed: &mut usize) {
    for _ in 0..8 {
        if !check_read(reader, total_fed, total_consumed) {
            break;
        }
    }
}

/// Craft an interesting declared length: biased toward the exact boundaries
/// the four historical fixes and the `try_read_delimited_message` guards
/// care about, plus a fuzz-chosen arbitrary value for organic coverage.
fn craft_len(r: &mut Reader, max_buffer_size: usize) -> usize {
    match r.u8() % 9 {
        0 => 0,
        1 => 1,
        2 => delimiter_size().saturating_sub(1),
        3 => delimiter_size(),
        4 => delimiter_size() + 1,
        5 => max_buffer_size,
        6 => max_buffer_size.saturating_add(1),
        7 => usize::MAX,
        _ => r.u32() as usize,
    }
}

fuzz_target!(|data: &[u8]| {
    let mut r = Reader::new(data);

    // `Channel::new` (and therefore `generate_nonblocking`) never validates
    // `buffer_size <= max_buffer_size` -- every production caller happens to
    // respect it, but nothing enforces it. Violating it makes `front_buf`
    // start out already larger than `max_buffer_size`, which trips
    // `try_read_delimited_message`'s own `debug_assert!` on the very first
    // parse attempt (found while developing this target -- see
    // `fuzz/README.md` §2.5 / this changeset's report for the minimized
    // input and the config-reachable path). That is a real, narrow,
    // construction-time precondition gap, not a wire-framing defect, and
    // outside this target's intended adversarial-input scope (malformed
    // WIRE bytes against a validly-constructed channel) -- so the
    // generator clamps to the same `buffer_size <= max_buffer_size` shape
    // every real caller already uses, the same way `fuzz_udp_flow.rs`
    // bounds `max_flows` away from the degenerate zero case.
    let reader_max_buffer_size = 32 + (r.u16() as u64 % 4096);
    let reader_buffer_size = (8 + (r.u8() as u64 % 64)).min(reader_max_buffer_size);

    let Ok((mut reader, _reader_peer)) =
        ReaderChannel::generate_nonblocking(reader_buffer_size, reader_max_buffer_size)
    else {
        return;
    };
    let Ok((mut writer, _writer_peer)) =
        WriterChannel::generate_nonblocking(WRITER_BUFFER_SIZE, WRITER_MAX_BUFFER_SIZE)
    else {
        return;
    };

    let reader_max_buffer_size = reader_max_buffer_size as usize;
    let mut total_fed = 0usize;
    let mut total_consumed = 0usize;

    // --- Round trip, once up front: encode a real frame, feed it whole
    // into a throwaway reader with a generous cap, and check it decodes
    // back to the same `id`. -----------------------------------------
    {
        let id_bytes = r.chunk();
        let (frame, id) = build_valid_frame(&mut writer, id_bytes);
        if !frame.is_empty() {
            if let Ok((mut roundtrip_reader, _peer)) =
                ReaderChannel::generate_nonblocking(WRITER_BUFFER_SIZE, WRITER_MAX_BUFFER_SIZE)
            {
                feed(
                    &mut roundtrip_reader,
                    WRITER_MAX_BUFFER_SIZE as usize,
                    &frame,
                );
                match roundtrip_reader.read_message() {
                    Ok(decoded) => assert_eq!(
                        decoded.id, id,
                        "round trip: decoded id does not match the encoded id"
                    ),
                    Err(error) => panic!(
                        "round trip: a whole, validly-framed message failed to decode: {error:?}"
                    ),
                }
            }
        }
    }

    // --- Main adversarial step loop -----------------------------------
    let mut steps = 0usize;
    while !r.is_empty() && steps < 512 {
        steps += 1;
        match r.u8() % 6 {
            // Whole valid frame, fed in 1-3 pieces (split across arbitrary
            // boundaries).
            0 | 1 => {
                let id_bytes = r.chunk();
                let (frame, _id) = build_valid_frame(&mut writer, id_bytes);
                if frame.is_empty() {
                    continue;
                }
                let n_pieces = 1 + (r.u8() as usize % 3);
                let mut offset = 0usize;
                for piece in 0..n_pieces {
                    if offset >= frame.len() {
                        break;
                    }
                    let remaining = frame.len() - offset;
                    let take = if piece + 1 == n_pieces {
                        remaining
                    } else {
                        1 + (r.u16() as usize % remaining)
                    };
                    let end = (offset + take).min(frame.len());
                    total_fed += feed(&mut reader, reader_max_buffer_size, &frame[offset..end]);
                    offset = end;
                    drain(&mut reader, total_fed, &mut total_consumed);
                }
            }
            // Valid frame immediately followed by garbage.
            2 => {
                let id_bytes = r.chunk();
                let (mut frame, _id) = build_valid_frame(&mut writer, id_bytes);
                let garbage = r.chunk();
                frame.extend_from_slice(garbage);
                total_fed += feed(&mut reader, reader_max_buffer_size, &frame);
                drain(&mut reader, total_fed, &mut total_consumed);
            }
            // Raw adversarial chunk straight from the fuzz input.
            3 | 4 => {
                let chunk = r.chunk();
                total_fed += feed(&mut reader, reader_max_buffer_size, chunk);
                drain(&mut reader, total_fed, &mut total_consumed);
            }
            // Crafted delimiter (boundary-biased) + trailing bytes that may
            // be shorter or longer than the declared length.
            _ => {
                let declared = craft_len(&mut r, reader_max_buffer_size);
                let trailing = r.chunk();
                let mut raw = Vec::with_capacity(delimiter_size() + trailing.len());
                raw.extend_from_slice(&declared.to_le_bytes());
                raw.extend_from_slice(trailing);
                total_fed += feed(&mut reader, reader_max_buffer_size, &raw);
                drain(&mut reader, total_fed, &mut total_consumed);
            }
        }
    }
});
