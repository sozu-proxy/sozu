//! H2 kawa-to-wire converter.
//!
//! Owns the per-connection [`loona_hpack::Encoder`] and emits HEADERS / DATA
//! frames on top of `Stream`-owned Kawa buffers (zero-copy slices into stream
//! storage). HEADERS payloads exceeding `max_frame_size` are split into a
//! HEADERS prefix plus CONTINUATION frames per RFC 9113 §6.10. Final frame
//! serialization is delegated to `serializer.rs`.

use std::cmp::min;

use kawa::{
    AsBuffer, Block, BlockConverter, Chunk, Flags, Kawa, Pair, ParsingErrorKind, ParsingPhase,
    StatusLine, Store,
};
use sozu_command::logging::ansi_palette;

use crate::protocol::{
    http::parser::compare_no_case,
    mux::{
        StreamId,
        h2::MAX_HEADER_LIST_SIZE,
        parser::{self, FrameHeader, FrameType, H2Error},
        pkawa::is_connection_specific_header,
        serializer::{gen_frame_header, gen_rst_stream},
    },
};

/// Module-level prefix used on every log line emitted from the H2 kawa
/// converter. Free-standing converter functions have no session in scope so
/// the single `MUX-CONV` label is used, colored bold bright-white (uniform
/// across every protocol) when the logger supports ANSI.
macro_rules! log_module_context {
    () => {{
        let (open, reset, _, _, _) = ansi_palette();
        format!("{open}MUX-CONV{reset}\t >>>", open = open, reset = reset)
    }};
}

pub struct H2BlockConverter<'a> {
    pub max_frame_size: usize,
    pub window: i32,
    pub stream_id: StreamId,
    pub encoder: &'a mut loona_hpack::Encoder<'static>,
    pub out: Vec<u8>,
    pub scheme: &'static [u8],
    /// Reusable buffer for lowercasing header keys, avoiding per-header allocation.
    pub lowercase_buf: Vec<u8>,
    /// Reusable buffer for assembling cookie values, avoiding per-cookie allocation.
    pub cookie_buf: Vec<u8>,
    /// `true` when the owning [`ConnectionH2`] is a backend client
    /// (`Position::Client`) — i.e. we are writing toward the upstream
    /// backend. Used to scope the `backend.flow_control.paused` metric so
    /// we only count stalls in the proxy → backend direction. Captured at
    /// converter construction (h2.rs) to avoid passing position through
    /// every stall site.
    pub position_is_client: bool,
    /// RFC 9218 §4 round-robin: when `true`, the converter emits at most one
    /// DATA frame per `kawa.prepare()` call for the current stream. The
    /// scheduler in [`super::h2::ConnectionH2::write_streams`] sets this flag
    /// before processing streams with the `incremental=true` priority hint so
    /// that same-urgency incremental streams interleave their DATA frames
    /// fairly rather than draining one stream before moving to the next.
    pub incremental_mode: bool,
    /// Number of incremental peers in the current same-urgency bucket
    /// (populated by the scheduler once per write pass). When `<= 1` the
    /// yield-after-one-DATA behaviour is a no-op anti-pattern: there is no
    /// peer to interleave with, so `kawa.prepare` should continue draining
    /// the stream in the same pass. `0` when the converter is used outside
    /// the scheduler (unit tests, resume path).
    pub incremental_peer_count: usize,
    /// Pending HPACK dynamic table-size update signal (RFC 7541 §4.2, §6.3).
    ///
    /// `Some(new_size)` when the peer's latest SETTINGS frame adjusted
    /// `SETTINGS_HEADER_TABLE_SIZE` and we have not yet signalled the new
    /// capacity to the decoder via a dynamic-table-size-update HPACK field
    /// at the start of our next header block. Set by
    /// [`super::h2::ConnectionH2`] at converter construction.
    ///
    /// [`emit_pending_size_update_if_new_block`] consumes this on the first
    /// `Block::StatusLine` / `Block::Header` of each pass; the containing
    /// `ConnectionH2` clears its own mirror only after confirming the signal
    /// reached the wire (see `size_update_emitted` below).
    pub pending_table_size_update: Option<u32>,
    /// `true` once [`emit_pending_size_update_if_new_block`] has actually
    /// written a size-update prefix into `self.out` during this write pass.
    /// The caller in `ConnectionH2::write_streams` reads this flag to know
    /// whether it is safe to clear its own mirror of the pending state.
    pub size_update_emitted: bool,
    /// `true` when [`Self::check_header_capacity`] tripped the
    /// `MAX_HEADER_LIST_SIZE` budget mid-encoding. Set during `call()`,
    /// committed in [`Self::finalize`] which flips `kawa.parsing_phase`
    /// to `Error{Processing(InternalError)}` (so the next prepare
    /// cycle's `initialize` synthesises an RST_STREAM frame) and drains
    /// any remaining `kawa.blocks` so we do not emit HEADERS/DATA frames
    /// after the RST. Defer-then-commit is required: `call()` borrows
    /// `kawa.storage.buffer()` for the whole body, which forecloses the
    /// `&mut kawa.parsing_phase` write that the abort needs.
    pub pending_oversized_abort: bool,
}

impl H2BlockConverter<'_> {
    /// RFC 7541 §4.2 / §6.3: emit a dynamic-table-size-update HPACK field at
    /// the start of the next header block whenever the peer adjusted
    /// `SETTINGS_HEADER_TABLE_SIZE`. Called at the top of every header-
    /// producing `Block::*` arm. Only fires once per block (guarded by
    /// `self.out.is_empty()`) and only when [`Self::pending_table_size_update`]
    /// is `Some(_)`.
    ///
    /// Encoding per RFC 7541 §5.1 + §6.3: prefix bits `001`, 5-bit prefix
    /// integer carrying the new maximum table size.
    fn emit_pending_size_update_if_new_block(&mut self) {
        if !self.out.is_empty() {
            return;
        }
        if let Some(new_size) = self.pending_table_size_update.take() {
            if let Err(e) =
                loona_hpack::encoder::encode_integer_into(new_size as usize, 5, 0x20, &mut self.out)
            {
                error!(
                    "{} HPACK encoding of dynamic-table-size-update signal failed: {:?}",
                    log_module_context!(),
                    e
                );
                // Leave `size_update_emitted` false so the caller retries
                // on the next write pass. `pending_table_size_update` has
                // already been `.take()`n — restore it so we don't drop it.
                self.pending_table_size_update = Some(new_size);
                return;
            }
            self.size_update_emitted = true;
        }
    }

    /// Guard the HPACK output buffer against [`MAX_HEADER_LIST_SIZE`] and
    /// flag the converter for an over-budget abort if the limit is
    /// reached.
    ///
    /// Returning `false` from a `BlockConverter::call` only breaks the
    /// `kawa.prepare` loop — by itself it does NOT signal any error to
    /// the peer, and a partial header block left in `self.out` would
    /// emit as a truncated HEADERS frame on the next flush, wedging the
    /// stream. To turn over-budget injection into a clean failure we
    /// (1) clear the partial block and (2) raise `pending_oversized_abort`,
    /// which [`Self::finalize`] commits by flipping `kawa.parsing_phase`
    /// to `Error{Processing(InternalError)}` and draining any remaining
    /// blocks. The next `prepare` cycle's `initialize` chokepoint at
    /// lines 173-208 then synthesises a typed `RST_STREAM(InternalError)`
    /// frame on the wire.
    ///
    /// Defer-then-commit is necessary because `call` already holds an
    /// immutable borrow of `kawa.storage.buffer()` for the duration of
    /// the body; `finalize` runs after that borrow is released.
    fn check_header_capacity(&mut self) -> bool {
        if self.out.len() > MAX_HEADER_LIST_SIZE {
            error!(
                "{} HPACK output buffer ({} bytes) exceeds MAX_HEADER_LIST_SIZE ({}), aborting header block",
                log_module_context!(),
                self.out.len(),
                MAX_HEADER_LIST_SIZE
            );
            self.out.clear();
            self.pending_oversized_abort = true;
            return false;
        }
        true
    }
}

impl<T: AsBuffer> BlockConverter<T> for H2BlockConverter<'_> {
    fn initialize(&mut self, kawa: &mut Kawa<T>) {
        // This is very ugly... we may add a h2 variant in kawa::ParsingErrorKind
        match kawa.parsing_phase {
            ParsingPhase::Error {
                kind: ParsingErrorKind::Processing { message },
                ..
            } => {
                let error = message.parse::<H2Error>().unwrap_or(H2Error::InternalError);
                let mut frame =
                    [0; parser::FRAME_HEADER_SIZE + parser::RST_STREAM_PAYLOAD_SIZE as usize];
                if let Err(e) = gen_rst_stream(&mut frame, self.stream_id, error) {
                    error!(
                        "{} failed to serialize RST_STREAM frame: {:?}",
                        log_module_context!(),
                        e
                    );
                    return;
                }
                kawa.push_out(Store::from_slice(&frame));
            }
            ParsingPhase::Error { .. } => {
                let mut frame =
                    [0; parser::FRAME_HEADER_SIZE + parser::RST_STREAM_PAYLOAD_SIZE as usize];
                if let Err(e) = gen_rst_stream(&mut frame, self.stream_id, H2Error::InternalError) {
                    error!(
                        "{} failed to serialize RST_STREAM frame: {:?}",
                        log_module_context!(),
                        e
                    );
                    return;
                }
                kawa.push_out(Store::from_slice(&frame));
            }
            _ => {}
        }
    }
    fn call(&mut self, block: Block, kawa: &mut Kawa<T>) -> bool {
        // Once `kawa.parsing_phase` is `Error`, `initialize` has already
        // synthesised a RST_STREAM frame for this prepare cycle. Drop any
        // remaining blocks instead of trying to encode HEADERS/DATA frames
        // that would land on the wire after the RST and break the stream
        // post-condition.
        if matches!(kawa.parsing_phase, ParsingPhase::Error { .. }) {
            return false;
        }
        let buffer = kawa.storage.buffer();
        // RFC 7541 §6.3: when the peer reduced SETTINGS_HEADER_TABLE_SIZE
        // (or changed it in any direction), the very first header block we
        // emit afterwards MUST begin with a dynamic-table-size-update
        // signal. `emit_pending_size_update_if_new_block` no-ops on every
        // block past the first of a header group (guard: `self.out.is_empty`)
        // and on DATA / flag blocks (no header output to prepend).
        if matches!(block, Block::StatusLine | Block::Header(_)) {
            self.emit_pending_size_update_if_new_block();
        }
        match block {
            Block::StatusLine => match kawa.detached.status_line.pop() {
                StatusLine::Request {
                    method,
                    authority,
                    path,
                    ..
                } => {
                    if let Err(e) = self
                        .encoder
                        .encode_header_into((b":method", method.data(buffer)), &mut self.out)
                    {
                        error!(
                            "{} HPACK encoding of :method pseudo-header failed: {:?}",
                            log_module_context!(),
                            e
                        );
                        return false;
                    }
                    if !self.check_header_capacity() {
                        return false;
                    }
                    if let Err(e) = self
                        .encoder
                        .encode_header_into((b":authority", authority.data(buffer)), &mut self.out)
                    {
                        error!(
                            "{} HPACK encoding of :authority pseudo-header failed: {:?}",
                            log_module_context!(),
                            e
                        );
                        return false;
                    }
                    if !self.check_header_capacity() {
                        return false;
                    }
                    if let Err(e) = self
                        .encoder
                        .encode_header_into((b":path", path.data(buffer)), &mut self.out)
                    {
                        error!(
                            "{} HPACK encoding of :path pseudo-header failed: {:?}",
                            log_module_context!(),
                            e
                        );
                        return false;
                    }
                    if !self.check_header_capacity() {
                        return false;
                    }
                    if let Err(e) = self
                        .encoder
                        .encode_header_into((b":scheme", self.scheme), &mut self.out)
                    {
                        error!(
                            "{} HPACK encoding of :scheme pseudo-header failed: {:?}",
                            log_module_context!(),
                            e
                        );
                        return false;
                    }
                    if !self.check_header_capacity() {
                        return false;
                    }
                }
                StatusLine::Response { status, .. } => {
                    if let Err(e) = self
                        .encoder
                        .encode_header_into((b":status", status.data(buffer)), &mut self.out)
                    {
                        error!(
                            "{} HPACK encoding of :status pseudo-header failed: {:?}",
                            log_module_context!(),
                            e
                        );
                        return false;
                    }
                    if !self.check_header_capacity() {
                        return false;
                    }
                }
                StatusLine::Unknown => {
                    error!(
                        "{} status line must be Request or Response before H2 conversion",
                        log_module_context!()
                    );
                    return false;
                }
            },
            Block::Cookies => {
                if kawa.detached.jar.is_empty() {
                    return true;
                }
                for cookie in kawa
                    .detached
                    .jar
                    .drain(..)
                    .filter(|cookie| !cookie.is_elided())
                {
                    self.cookie_buf.clear();
                    self.cookie_buf.extend_from_slice(cookie.key.data(buffer));
                    self.cookie_buf.push(b'=');
                    self.cookie_buf.extend_from_slice(cookie.val.data(buffer));
                    if let Err(e) = self
                        .encoder
                        .encode_header_into((b"cookie", &self.cookie_buf), &mut self.out)
                    {
                        error!(
                            "{} HPACK encoding of cookie header failed: {:?}",
                            log_module_context!(),
                            e
                        );
                        return false;
                    }
                    if !self.check_header_capacity() {
                        return false;
                    }
                }
            }
            Block::Header(Pair {
                key: Store::Empty, ..
            }) => {
                // elided header
            }
            Block::Header(Pair { key, val }) => {
                {
                    let key = key.data(buffer);
                    let val = val.data(buffer);
                    let skip = match key.first() {
                        Some(b'c' | b'C' | b'p' | b'P' | b'u' | b'U' | b'k' | b'K') => {
                            is_connection_specific_header(key)
                        }
                        Some(b'h' | b'H') => {
                            compare_no_case(key, b"host") || compare_no_case(key, b"http2-settings")
                        }
                        Some(b't' | b'T') => {
                            is_connection_specific_header(key)
                                || compare_no_case(key, b"trailer")
                                || (compare_no_case(key, b"te")
                                    && !compare_no_case(val, b"trailers"))
                        }
                        _ => false,
                    };
                    if skip {
                        return true;
                    }
                }
                self.lowercase_buf.clear();
                self.lowercase_buf.extend_from_slice(key.data(buffer));
                self.lowercase_buf.make_ascii_lowercase();
                // RFC 9113 §8.2: reject header names with control chars or high bytes
                if self.lowercase_buf.iter().any(|&b| b <= 0x20 || b >= 0x7f) {
                    error!(
                        "{} H1->H2 header name contains invalid characters, skipping",
                        log_module_context!()
                    );
                    return true; // skip this header, continue with next
                }
                // RFC 9113 §8.2.1 / RFC 9110 §5.5: reject header values with
                // NUL, CR, LF, or other C0 controls. Without this, a compromised
                // backend can inject bytes that poison the HPACK dynamic table.
                let val_bytes = val.data(buffer);
                if val_bytes
                    .iter()
                    .any(|&b| matches!(b, 0x00..=0x08 | 0x0A..=0x1F | 0x7F))
                {
                    error!(
                        "{} H1->H2 header value contains invalid characters, skipping",
                        log_module_context!()
                    );
                    return true;
                }
                if let Err(e) = self
                    .encoder
                    .encode_header_into((&self.lowercase_buf, val.data(buffer)), &mut self.out)
                {
                    error!(
                        "{} HPACK encoding of header failed: {:?}",
                        log_module_context!(),
                        e
                    );
                    return false;
                }
                if !self.check_header_capacity() {
                    return false;
                }
            }
            Block::ChunkHeader(_) => {
                // this converter doesn't align H1 chunks on H2 data frames
            }
            Block::Chunk(Chunk { data }) => {
                let mut header = [0; parser::FRAME_HEADER_SIZE];
                let payload_len = data.len();
                let payload_fits_window =
                    i32::try_from(payload_len).is_ok_and(|pl| self.window >= pl);
                let (data, payload_len, can_continue) = if payload_fits_window
                    && self.max_frame_size >= payload_len
                {
                    // the window is wide enough to send the entire chunk
                    (data, payload_len as u32, true)
                } else if self.window > 0 {
                    // we split the chunk to fit in the window
                    let payload_len = min(self.max_frame_size, self.window.max(0) as usize);
                    let (before, after) = data.split(payload_len);
                    if !after.is_empty() {
                        kawa.blocks.push_front(Block::Chunk(Chunk { data: after }));
                    }
                    (
                        before,
                        payload_len as u32,
                        self.max_frame_size < self.window.max(0) as usize,
                    )
                } else {
                    // the window can't take any more bytes, return the chunk to the blocks
                    trace!(
                        "{} H2 flow control stall: stream={} connection_window={} pending_bytes={}",
                        log_module_context!(),
                        self.stream_id,
                        self.window,
                        data.len()
                    );
                    incr!("h2.flow_control_stall");
                    if self.position_is_client {
                        // Direction-scoped counterpart: the proxy → backend
                        // write was paused because the backend's HTTP/2
                        // receive window is empty. There is no symmetric
                        // "resumed" emission — the next writable cycle just
                        // succeeds when the backend acknowledges window
                        // updates, so the stall metric is the only signal
                        // available without plumbing an explicit boundary
                        // through `flush_stream_out`.
                        incr!("backend.flow_control.paused");
                    }
                    kawa.blocks.push_front(Block::Chunk(Chunk { data }));
                    return false;
                };
                self.window -= i32::try_from(payload_len).unwrap_or(i32::MAX);
                if let Err(e) = gen_frame_header(
                    &mut header,
                    &FrameHeader {
                        payload_len,
                        frame_type: FrameType::Data,
                        flags: 0,
                        stream_id: self.stream_id,
                    },
                ) {
                    error!(
                        "{} failed to serialize DATA frame header: {:?}",
                        log_module_context!(),
                        e
                    );
                    return false;
                }
                kawa.push_out(Store::from_slice(&header));
                kawa.push_out(data);
                incr!("h2.frames.tx.data");
                // kawa.push_delimiter();
                // RFC 9218 §4: incremental streams yield to the scheduler
                // after every DATA frame so same-urgency incremental peers
                // can interleave. Non-incremental streams drain sequentially
                // (current behaviour) by honouring `can_continue`.
                //
                // Solo-bucket guard: the yield only matters when there is
                // at least one other incremental peer in the same urgency
                // bucket. With <= 1 incremental peer, yielding strands the
                // stream because `finalize_write` then strips
                // `Ready::WRITABLE` (no `expect_write` is set on a clean
                // yield), and edge-triggered epoll will not re-fire. See
                // `test_h2_solo_incremental_drains_fully` and the h2.rs
                // scheduler wiring that populates `incremental_peer_count`.
                //
                // Tier 3c close-race guard (LIFECYCLE §9 invariant 19): if
                // the next block is the closing `Block::Flags` with
                // `end_stream=true`, suppress the yield unconditionally.
                // Yielding between the last DATA and the closing Flags
                // strands the END_STREAM marker in `kawa.blocks` — the
                // invariant-16 finalize_write guard catches the wake-up
                // loss, but yielding one frame later is still cheaper than
                // paying the round-trip through the event loop for a
                // single END_STREAM frame (often an empty DATA frame with
                // the END_STREAM flag set, 9 bytes of overhead).
                let next_closes_stream = matches!(
                    kawa.blocks.front(),
                    Some(Block::Flags(Flags {
                        end_stream: true,
                        ..
                    }))
                );
                let yield_after_data =
                    self.incremental_mode && self.incremental_peer_count > 1 && !next_closes_stream;
                return can_continue && !yield_after_data;
            }
            Block::Flags(Flags {
                end_header,
                end_stream,
                ..
            }) => {
                let sent_end_stream = if end_header {
                    let payload = std::mem::take(&mut self.out);
                    let mut header = [0; parser::FRAME_HEADER_SIZE];
                    if payload.is_empty() {
                        false
                    } else if payload.len() <= self.max_frame_size {
                        let mut flags = parser::FLAG_END_HEADERS;
                        if end_stream {
                            flags |= parser::FLAG_END_STREAM;
                        }
                        if let Err(e) = gen_frame_header(
                            &mut header,
                            &FrameHeader {
                                payload_len: payload.len() as u32,
                                frame_type: FrameType::Headers,
                                flags,
                                stream_id: self.stream_id,
                            },
                        ) {
                            error!(
                                "{} failed to serialize HEADERS frame header: {:?}",
                                log_module_context!(),
                                e
                            );
                            return false;
                        }
                        kawa.push_out(Store::from_slice(&header));
                        kawa.push_out(Store::from_vec(payload));
                        incr!("h2.frames.tx.headers");
                        true
                    } else {
                        let chunks = payload.chunks(self.max_frame_size);
                        let n_chunks = chunks.len();
                        for (i, chunk) in chunks.enumerate() {
                            let mut flags = 0u8;
                            if i == 0 && end_stream {
                                flags |= parser::FLAG_END_STREAM;
                            }
                            if i + 1 == n_chunks {
                                flags |= parser::FLAG_END_HEADERS;
                            }
                            if i == 0 {
                                if let Err(e) = gen_frame_header(
                                    &mut header,
                                    &FrameHeader {
                                        payload_len: chunk.len() as u32,
                                        frame_type: FrameType::Headers,
                                        flags,
                                        stream_id: self.stream_id,
                                    },
                                ) {
                                    error!(
                                        "{} failed to serialize HEADERS frame header: {:?}",
                                        log_module_context!(),
                                        e
                                    );
                                    return false;
                                }
                                incr!("h2.frames.tx.headers");
                            } else if let Err(e) = gen_frame_header(
                                &mut header,
                                &FrameHeader {
                                    payload_len: chunk.len() as u32,
                                    frame_type: FrameType::Continuation,
                                    flags,
                                    stream_id: self.stream_id,
                                },
                            ) {
                                error!(
                                    "{} failed to serialize CONTINUATION frame header: {:?}",
                                    log_module_context!(),
                                    e
                                );
                                return false;
                            } else {
                                incr!("h2.frames.tx.continuation");
                            }
                            kawa.push_out(Store::from_slice(&header));
                            kawa.push_out(Store::from_slice(chunk));
                        }
                        true
                    }
                } else {
                    false
                };
                if end_stream && !sent_end_stream {
                    let mut header = [0; parser::FRAME_HEADER_SIZE];
                    if let Err(e) = gen_frame_header(
                        &mut header,
                        &FrameHeader {
                            payload_len: 0,
                            frame_type: FrameType::Data,
                            flags: parser::FLAG_END_STREAM,
                            stream_id: self.stream_id,
                        },
                    ) {
                        error!(
                            "{} failed to serialize empty DATA frame header: {:?}",
                            log_module_context!(),
                            e
                        );
                        return false;
                    }
                    kawa.push_out(Store::from_slice(&header));
                    incr!("h2.frames.tx.data");
                }
            }
        }
        true
    }
    fn finalize(&mut self, kawa: &mut Kawa<T>) {
        if self.pending_oversized_abort {
            self.pending_oversized_abort = false;
            // Push the RST_STREAM frame to `kawa.out` IN THIS PASS so the
            // very next `flush_stream_out` call drains it onto the wire.
            //
            // Why we cannot rely on the next prepare cycle's `initialize`:
            // `write_streams` retires a stream the moment
            // `kawa.is_error() && kawa.is_completed()`. By clearing
            // `kawa.blocks` (required to suppress post-RST HEADERS/DATA
            // frames) we trip `is_completed()` immediately, which would
            // otherwise allow the stream to be recycled with no RST on
            // the wire — a spec violation and a hung peer (see Codex P1
            // on commit 358e6e20). Emitting the frame here keeps the
            // single-pass invariant: prepare → out has RST_STREAM →
            // flush ships it → retirement check.
            let mut frame =
                [0u8; parser::FRAME_HEADER_SIZE + parser::RST_STREAM_PAYLOAD_SIZE as usize];
            if let Err(e) = gen_rst_stream(&mut frame, self.stream_id, H2Error::InternalError) {
                error!(
                    "{} failed to serialize over-budget RST_STREAM frame: {:?}",
                    log_module_context!(),
                    e
                );
            } else {
                kawa.push_out(Store::from_slice(&frame));
            }
            // Preserve the original parsing_phase marker so logs and the
            // ParsingPhaseMarker telemetry stay coherent (e.g. Headers vs
            // Body) instead of getting clobbered to a synthetic value.
            let marker = kawa.parsing_phase.marker();
            kawa.parsing_phase = ParsingPhase::Error {
                marker,
                kind: ParsingErrorKind::Processing {
                    message: H2Error::InternalError.as_str(),
                },
            };
            // Drop any remaining blocks — the stream is dead. Without this
            // a follow-on prepare pass (e.g. on the back-pressure return
            // path) would still try to encode headers/data after our RST.
            kawa.blocks.clear();
            self.out.clear();
            incr!("h2.headers.rejected.budget_overrun");
            return;
        }
        if !self.out.is_empty() {
            error!(
                "{} H2BlockConverter finalize: out buffer not empty ({} bytes remaining), clearing",
                log_module_context!(),
                self.out.len()
            );
            self.out.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kawa::{Buffer, Kind, SliceBuffer};

    /// Create a fresh H2BlockConverter with default settings for testing.
    fn test_converter<'a>(encoder: &'a mut loona_hpack::Encoder<'static>) -> H2BlockConverter<'a> {
        H2BlockConverter {
            max_frame_size: 16384,
            window: 65535,
            stream_id: 1,
            encoder,
            out: Vec::new(),
            scheme: b"https",
            lowercase_buf: Vec::new(),
            cookie_buf: Vec::new(),
            // Tests exercise the converter in isolation; the
            // backend.flow_control.paused metric is direction-scoped and
            // off by default in unit tests.
            position_is_client: false,
            // RFC 9218 round-robin is scheduler-driven; unit tests that
            // exercise single-stream conversions leave it off.
            incremental_mode: false,
            // Default 0 means "no scheduler context"; unit tests that do
            // not exercise the solo-bucket guard get the safe default.
            incremental_peer_count: 0,
            // RFC 7541 §6.3: no pending SETTINGS_HEADER_TABLE_SIZE change
            // in the default test converter. Tests that exercise the
            // dynamic-table-size-update path set this explicitly.
            pending_table_size_update: None,
            size_update_emitted: false,
            pending_oversized_abort: false,
        }
    }

    /// Create a Kawa response stream with a buffer for testing.
    fn make_kawa(buf: &mut [u8], kind: Kind) -> Kawa<SliceBuffer<'_>> {
        Kawa::new(kind, Buffer::new(SliceBuffer(buf)))
    }

    // ── Header filtering ─────────────────────────────────────────────────

    #[test]
    fn test_converter_filters_connection_header() {
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        // "connection: close" should be filtered (return true = continue, no output)
        let result = conv.call(
            Block::Header(Pair {
                key: Store::Static(b"connection"),
                val: Store::Static(b"close"),
            }),
            &mut kawa,
        );
        assert!(result);
        assert!(conv.out.is_empty(), "connection header should be filtered");
    }

    #[test]
    fn test_converter_filters_upgrade_header() {
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        let result = conv.call(
            Block::Header(Pair {
                key: Store::Static(b"upgrade"),
                val: Store::Static(b"h2c"),
            }),
            &mut kawa,
        );
        assert!(result);
        assert!(conv.out.is_empty(), "upgrade header should be filtered");
    }

    #[test]
    fn test_converter_filters_transfer_encoding_header() {
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        let result = conv.call(
            Block::Header(Pair {
                key: Store::Static(b"transfer-encoding"),
                val: Store::Static(b"chunked"),
            }),
            &mut kawa,
        );
        assert!(result);
        assert!(
            conv.out.is_empty(),
            "transfer-encoding header should be filtered"
        );
    }

    #[test]
    fn test_converter_filters_keep_alive_header() {
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        let result = conv.call(
            Block::Header(Pair {
                key: Store::Static(b"keep-alive"),
                val: Store::Static(b"timeout=5"),
            }),
            &mut kawa,
        );
        assert!(result);
        assert!(conv.out.is_empty(), "keep-alive header should be filtered");
    }

    #[test]
    fn test_converter_filters_host_header() {
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        let result = conv.call(
            Block::Header(Pair {
                key: Store::Static(b"host"),
                val: Store::Static(b"example.com"),
            }),
            &mut kawa,
        );
        assert!(result);
        assert!(conv.out.is_empty(), "host header should be filtered");
    }

    #[test]
    fn test_converter_filters_http2_settings_header() {
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        let result = conv.call(
            Block::Header(Pair {
                key: Store::Static(b"http2-settings"),
                val: Store::Static(b"some-settings"),
            }),
            &mut kawa,
        );
        assert!(result);
        assert!(
            conv.out.is_empty(),
            "http2-settings header should be filtered"
        );
    }

    #[test]
    fn test_converter_filters_te_non_trailers() {
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        // TE: gzip should be filtered
        let result = conv.call(
            Block::Header(Pair {
                key: Store::Static(b"te"),
                val: Store::Static(b"gzip"),
            }),
            &mut kawa,
        );
        assert!(result);
        assert!(
            conv.out.is_empty(),
            "te: gzip header should be filtered in H2"
        );
    }

    #[test]
    fn test_converter_keeps_te_trailers() {
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        // TE: trailers should be kept
        let result = conv.call(
            Block::Header(Pair {
                key: Store::Static(b"te"),
                val: Store::Static(b"trailers"),
            }),
            &mut kawa,
        );
        assert!(result);
        assert!(
            !conv.out.is_empty(),
            "te: trailers header should NOT be filtered"
        );
    }

    #[test]
    fn test_converter_filters_trailer_header() {
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        let result = conv.call(
            Block::Header(Pair {
                key: Store::Static(b"trailer"),
                val: Store::Static(b"grpc-status"),
            }),
            &mut kawa,
        );
        assert!(result);
        assert!(conv.out.is_empty(), "trailer header should be filtered");
    }

    #[test]
    fn test_converter_keeps_valid_header() {
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        let result = conv.call(
            Block::Header(Pair {
                key: Store::Static(b"content-type"),
                val: Store::Static(b"text/html"),
            }),
            &mut kawa,
        );
        assert!(result);
        assert!(!conv.out.is_empty(), "valid header should be HPACK encoded");
    }

    // ── Header lowercasing ───────────────────────────────────────────────

    #[test]
    fn test_converter_lowercases_header_name() {
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        // Write the key/val into the kawa buffer so Store::Slice works
        std::io::Write::write_all(&mut kawa.storage, b"Content-Type").unwrap();
        std::io::Write::write_all(&mut kawa.storage, b"text/html").unwrap();

        let result = conv.call(
            Block::Header(Pair {
                key: Store::Static(b"Content-Type"),
                val: Store::Static(b"text/html"),
            }),
            &mut kawa,
        );
        assert!(result);
        // The lowercase_buf should contain the lowercased name
        assert_eq!(&conv.lowercase_buf, b"content-type");
    }

    // ── HPACK dynamic-table-size-update (RFC 7541 §4.2 / §6.3) ────────────

    #[test]
    fn test_converter_emits_pending_size_update_on_first_header_block() {
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        conv.pending_table_size_update = Some(256);

        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf, Kind::Response);
        kawa.detached.status_line = StatusLine::Response {
            version: kawa::Version::V20,
            code: 200,
            status: Store::Static(b"200"),
            reason: Store::Static(b"OK"),
        };

        let result = conv.call(Block::StatusLine, &mut kawa);
        assert!(result, "StatusLine encoding must succeed");
        assert!(
            conv.size_update_emitted,
            "emit flag must flip once the signal bytes are written"
        );
        assert!(
            conv.pending_table_size_update.is_none(),
            "pending value must be cleared after emission"
        );
        // First byte of out MUST be an HPACK dynamic-table-size-update
        // directive: prefix bits `001` (0x20..0x3F range). 256 > 30 so the
        // encoding uses the multi-byte form: `00111111` = 0x3F then a
        // varint-continued encoding of (256 - 31) = 225. 225 = 0xE1 in one
        // byte since 225 < 128 is false → 0xE1 has high bit set, encoded
        // as 0xE1 - 128 = 0x61 continuation then 0x01 final. Rather than
        // spell the exact bytes, assert only the prefix pattern.
        assert!(!conv.out.is_empty(), "out must contain the prefix bytes");
        assert_eq!(
            conv.out[0] & 0b1110_0000,
            0b0010_0000,
            "first byte must have HPACK size-update prefix `001xxxxx`, got {:#010b}",
            conv.out[0]
        );
    }

    #[test]
    fn test_converter_skips_size_update_when_no_pending() {
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        // pending_table_size_update is None by default.

        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf, Kind::Response);
        kawa.detached.status_line = StatusLine::Response {
            version: kawa::Version::V20,
            code: 200,
            status: Store::Static(b"200"),
            reason: Store::Static(b"OK"),
        };

        let result = conv.call(Block::StatusLine, &mut kawa);
        assert!(result);
        assert!(
            !conv.size_update_emitted,
            "emit flag must stay false when nothing was pending"
        );
        // Without a size-update, the first byte should be a literal/indexed
        // header directive (0x00..=0x1F, 0x40..=0x7F, or 0x80..=0xFF),
        // NEVER the 0b001xxxxx size-update pattern.
        assert!(
            !conv.out.is_empty(),
            "out must contain the :status encoding"
        );
        assert_ne!(
            conv.out[0] & 0b1110_0000,
            0b0010_0000,
            "first byte MUST NOT be a size-update directive when nothing pending"
        );
    }

    #[test]
    fn test_converter_emits_size_update_only_once_per_block() {
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        conv.pending_table_size_update = Some(128);

        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf, Kind::Response);
        kawa.detached.status_line = StatusLine::Response {
            version: kawa::Version::V20,
            code: 200,
            status: Store::Static(b"200"),
            reason: Store::Static(b"OK"),
        };

        // First block: StatusLine — emits the size update.
        let r1 = conv.call(Block::StatusLine, &mut kawa);
        assert!(r1);
        let size_after_status_line = conv.out.len();
        assert!(conv.size_update_emitted);

        // Second block: a regular header in the SAME block — must NOT
        // re-emit the size-update (guard: `self.out.is_empty()` is now
        // false). The output buffer should grow by the header's encoded
        // length, not by the size-update length.
        let r2 = conv.call(
            Block::Header(Pair {
                key: Store::Static(b"content-type"),
                val: Store::Static(b"text/plain"),
            }),
            &mut kawa,
        );
        assert!(r2);
        assert!(
            conv.out.len() > size_after_status_line,
            "second header must have been appended"
        );
        // `size_update_emitted` stays true (we fired once); pending stays
        // cleared.
        assert!(conv.size_update_emitted);
        assert!(conv.pending_table_size_update.is_none());
    }

    // ── Elided headers ──────────────────────────────────────────────────

    #[test]
    fn test_converter_skips_elided_header() {
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        let result = conv.call(
            Block::Header(Pair {
                key: Store::Empty,
                val: Store::Static(b"some-value"),
            }),
            &mut kawa,
        );
        assert!(result);
        assert!(
            conv.out.is_empty(),
            "elided header should produce no output"
        );
    }

    // ── Invalid header name characters ──────────────────────────────────

    #[test]
    fn test_converter_skips_header_with_control_chars() {
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        // Header name with a null byte (control char <= 0x20)
        let result = conv.call(
            Block::Header(Pair {
                key: Store::Static(b"bad\x00header"),
                val: Store::Static(b"value"),
            }),
            &mut kawa,
        );
        assert!(result, "should return true (skip and continue)");
        assert!(
            conv.out.is_empty(),
            "header with control chars should be skipped"
        );
    }

    #[test]
    fn test_converter_skips_header_with_high_bytes() {
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        // Header name with byte >= 0x7f
        let result = conv.call(
            Block::Header(Pair {
                key: Store::Static(b"bad\x80header"),
                val: Store::Static(b"value"),
            }),
            &mut kawa,
        );
        assert!(result, "should return true (skip and continue)");
        assert!(
            conv.out.is_empty(),
            "header with high bytes should be skipped"
        );
    }

    // ── Flow control during DATA emission ────────────────────────────────

    #[test]
    fn test_converter_data_within_window() {
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        conv.window = 1000;
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        let data = Store::Static(b"hello world");
        let result = conv.call(Block::Chunk(Chunk { data }), &mut kawa);
        assert!(result, "data within window should succeed");
        // Window should be decremented by data length
        assert_eq!(conv.window, 1000 - 11); // "hello world" = 11 bytes
        // kawa.out should contain a DATA frame header + data
        assert!(!kawa.out.is_empty());
    }

    #[test]
    fn test_converter_data_stalls_on_zero_window() {
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        conv.window = 0;
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        let data = Store::Static(b"hello");
        let result = conv.call(Block::Chunk(Chunk { data }), &mut kawa);
        assert!(!result, "should stall when window is zero");
        // Data should be pushed back to blocks
        assert!(!kawa.blocks.is_empty());
    }

    #[test]
    fn test_converter_data_stalls_on_negative_window() {
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        conv.window = -100;
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        let data = Store::Static(b"hello");
        let result = conv.call(Block::Chunk(Chunk { data }), &mut kawa);
        assert!(!result, "should stall when window is negative");
        assert!(!kawa.blocks.is_empty());
    }

    #[test]
    fn test_converter_data_splits_for_small_window() {
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        conv.window = 3; // smaller than data
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        let data = Store::Static(b"hello world");
        let _sent = conv.call(Block::Chunk(Chunk { data }), &mut kawa);
        // Should have sent 3 bytes (or returned false) with remainder in blocks
        assert!(conv.window <= 0);
        // The remaining bytes should be pushed back to kawa.blocks
        assert!(!kawa.blocks.is_empty());
    }

    // ── END_STREAM flag management ──────────────────────────────────────

    #[test]
    fn test_converter_end_stream_on_headers() {
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        // First encode a header to populate the out buffer
        conv.call(
            Block::Header(Pair {
                key: Store::Static(b"content-type"),
                val: Store::Static(b"text/html"),
            }),
            &mut kawa,
        );

        // Now emit Flags with end_header=true and end_stream=true
        let result = conv.call(
            Block::Flags(Flags {
                end_body: true,
                end_chunk: false,
                end_header: true,
                end_stream: true,
            }),
            &mut kawa,
        );
        assert!(result);
        // Should have produced a HEADERS frame with END_STREAM flag
        assert!(!kawa.out.is_empty());
    }

    #[test]
    fn test_converter_end_stream_without_headers_emits_empty_data() {
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        // end_stream=true but end_header=false and no header data
        let result = conv.call(
            Block::Flags(Flags {
                end_body: true,
                end_chunk: false,
                end_header: false,
                end_stream: true,
            }),
            &mut kawa,
        );
        assert!(result);
        // Should emit an empty DATA frame with END_STREAM flag
        assert!(!kawa.out.is_empty());
    }

    // ── Finalize ────────────────────────────────────────────────────────

    #[test]
    fn test_converter_finalize_clears_remaining_buffer() {
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        // Add a header but don't emit Flags (so out buffer is non-empty)
        conv.call(
            Block::Header(Pair {
                key: Store::Static(b"x-test"),
                val: Store::Static(b"value"),
            }),
            &mut kawa,
        );
        assert!(!conv.out.is_empty());

        conv.finalize(&mut kawa);
        assert!(conv.out.is_empty(), "finalize should clear the out buffer");
    }

    #[test]
    fn test_converter_finalize_noop_when_empty() {
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        // Nothing in out buffer
        conv.finalize(&mut kawa);
        assert!(conv.out.is_empty());
    }

    // ── ChunkHeader ─────────────────────────────────────────────────────

    #[test]
    fn test_converter_chunk_header_noop() {
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        let result = conv.call(
            Block::ChunkHeader(kawa::ChunkHeader {
                length: Store::Static(b"a"),
            }),
            &mut kawa,
        );
        assert!(result, "ChunkHeader should be a no-op and return true");
        assert!(kawa.out.is_empty());
    }

    // ── RFC 9218 incremental yield: solo-bucket guard ───────────────────
    //
    // The converter yields after every DATA frame when
    // `incremental_mode == true`, so same-urgency incremental peers can
    // interleave. But when there is <= 1 incremental peer in the bucket
    // (`incremental_peer_count <= 1`), the yield is a no-op anti-pattern
    // that strands the stream — `finalize_write` then strips
    // `Ready::WRITABLE` without setting `expect_write`, and edge-triggered
    // epoll never re-fires. These tests exercise the three boundary
    // transitions.

    #[test]
    fn test_converter_incremental_solo_does_not_yield() {
        // peer_count = 1 means this stream is alone in the incremental
        // bucket. The yield MUST be skipped so the stream drains in the
        // same pass. Returning `true` signals `kawa.prepare` to continue.
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        conv.incremental_mode = true;
        conv.incremental_peer_count = 1;
        conv.window = 4096;
        let mut buf = vec![0u8; 8192];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        let data = Store::Static(b"hello world");
        let cont = conv.call(Block::Chunk(Chunk { data }), &mut kawa);
        assert!(
            cont,
            "solo incremental stream must not yield after a DATA frame \
             (would strand the stream under finalize_write WRITABLE withdrawal)"
        );
    }

    #[test]
    fn test_converter_incremental_pair_yields() {
        // peer_count = 2 is the smallest bucket where interleaving has any
        // effect. Must yield to allow the peer to run.
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        conv.incremental_mode = true;
        conv.incremental_peer_count = 2;
        conv.window = 4096;
        let mut buf = vec![0u8; 8192];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        let data = Store::Static(b"hello world");
        let cont = conv.call(Block::Chunk(Chunk { data }), &mut kawa);
        assert!(
            !cont,
            "incremental stream with a peer must yield after a DATA frame \
             so the peer can interleave (RFC 9218 §4)"
        );
    }

    #[test]
    fn test_converter_incremental_trio_yields() {
        // peer_count = 3 is the canonical multi-peer case. Yield is
        // required so the scheduler's round-robin cursor advances.
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        conv.incremental_mode = true;
        conv.incremental_peer_count = 3;
        conv.window = 4096;
        let mut buf = vec![0u8; 8192];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        let data = Store::Static(b"hello world");
        let cont = conv.call(Block::Chunk(Chunk { data }), &mut kawa);
        assert!(
            !cont,
            "incremental stream with two peers must yield after a DATA frame"
        );
    }

    #[test]
    fn test_converter_non_incremental_never_yields_regardless_of_peer_count() {
        // When `incremental_mode == false`, the guard is skipped entirely:
        // draining is sequential regardless of what `incremental_peer_count`
        // says. The scheduler is responsible for setting the pair
        // consistently, but the converter must not misbehave if they
        // disagree (defence-in-depth).
        for peers in [0usize, 1, 2, 5] {
            let mut encoder = loona_hpack::Encoder::new();
            let mut conv = test_converter(&mut encoder);
            conv.incremental_mode = false;
            conv.incremental_peer_count = peers;
            conv.window = 4096;
            let mut buf = vec![0u8; 8192];
            let mut kawa = make_kawa(&mut buf, Kind::Response);

            let data = Store::Static(b"hello world");
            let cont = conv.call(Block::Chunk(Chunk { data }), &mut kawa);
            assert!(
                cont,
                "non-incremental stream must drain sequentially regardless \
                 of incremental_peer_count (= {peers})"
            );
        }
    }

    // ── LIFECYCLE §9 invariant 19: DATA/trailer close-race suppression ────

    #[test]
    fn test_converter_suppresses_yield_before_closing_end_stream_flags() {
        // Two incremental peers normally trigger a yield after DATA. But
        // if the next queued block is a closing `Block::Flags` (end_stream
        // = true), yielding would strand END_STREAM. Tier 3c:
        // converter must drain the closing Flags in the same pass.
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        conv.incremental_mode = true;
        conv.incremental_peer_count = 3;
        conv.window = 4096;
        let mut buf = vec![0u8; 8192];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        // Pre-queue the closing Flags so the converter sees it when
        // peeking `kawa.blocks.front()` inside the DATA arm.
        kawa.blocks.push_back(Block::Flags(Flags {
            end_body: true,
            end_chunk: false,
            end_header: false,
            end_stream: true,
        }));

        let data = Store::Static(b"hello world");
        let cont = conv.call(Block::Chunk(Chunk { data }), &mut kawa);
        assert!(
            cont,
            "yield must be suppressed when the next block is a closing Flags \
             (end_stream = true) — otherwise END_STREAM strands in kawa.blocks"
        );
    }

    #[test]
    fn test_converter_yields_before_trailing_flags_without_end_stream() {
        // Flags blocks without `end_stream=true` (e.g. inter-chunk
        // `end_chunk` markers the H1 parser emits mid-body) must NOT
        // suppress the yield — they do not terminate the stream.
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        conv.incremental_mode = true;
        conv.incremental_peer_count = 3;
        conv.window = 4096;
        let mut buf = vec![0u8; 8192];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        kawa.blocks.push_back(Block::Flags(Flags {
            end_body: false,
            end_chunk: true,
            end_header: false,
            end_stream: false,
        }));

        let data = Store::Static(b"chunked body");
        let cont = conv.call(Block::Chunk(Chunk { data }), &mut kawa);
        assert!(
            !cont,
            "yield still applies when next Flags is a non-terminal marker \
             (end_chunk without end_stream)"
        );
    }

    #[test]
    fn test_converter_yields_before_chunk_block() {
        // Sanity: the close-race guard only triggers on Block::Flags —
        // a trailing Block::Chunk does NOT suppress the yield.
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        conv.incremental_mode = true;
        conv.incremental_peer_count = 3;
        conv.window = 4096;
        let mut buf = vec![0u8; 8192];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        kawa.blocks.push_back(Block::Chunk(Chunk {
            data: Store::Static(b"next chunk"),
        }));

        let data = Store::Static(b"hello world");
        let cont = conv.call(Block::Chunk(Chunk { data }), &mut kawa);
        assert!(
            !cont,
            "back-to-back DATA must still yield — incremental rotation depends \
             on the converter deferring the next DATA"
        );
    }

    // ── HPACK budget over-run ────────────────────────────────────────────

    /// `check_header_capacity` setting `pending_oversized_abort` should
    /// stop the encoding pass cleanly: `self.out` is dropped (no truncated
    /// HEADERS frame escapes), the converter raises the abort flag, and
    /// `finalize` commits by flipping `kawa.parsing_phase` to
    /// `Error{Processing(InternalError)}` so the next prepare cycle's
    /// `initialize` synthesises a typed RST_STREAM frame.
    #[test]
    fn test_converter_aborts_on_header_budget_overrun() {
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        // Pre-populate `self.out` past `MAX_HEADER_LIST_SIZE` so that the
        // very next capacity check trips the abort. We use a Status:200
        // pseudo-header to drive the encode path through the Response arm
        // of `Block::StatusLine`.
        conv.out = vec![0u8; super::MAX_HEADER_LIST_SIZE + 1];
        kawa.detached.status_line = StatusLine::Response {
            version: kawa::Version::V20,
            code: 200,
            status: Store::Static(b"200"),
            reason: Store::Empty,
        };
        let cont = conv.call(Block::StatusLine, &mut kawa);
        assert!(!cont, "overflow must break the prepare loop");
        assert!(
            conv.pending_oversized_abort,
            "abort flag must be raised when MAX_HEADER_LIST_SIZE is exceeded"
        );
        assert!(
            conv.out.is_empty(),
            "the partial HPACK block must not escape as a truncated HEADERS frame"
        );

        // Subsequent calls within the SAME prepare cycle should be no-ops
        // (the prepare loop already broke; we only assert the early-out
        // works in case a caller ever invokes call() manually after a
        // false return).
        kawa.parsing_phase = ParsingPhase::Error {
            marker: kawa::ParsingPhaseMarker::Headers,
            kind: ParsingErrorKind::Processing { message: "test" },
        };
        let cont = conv.call(
            Block::Header(Pair {
                key: Store::Static(b"x-after-rst"),
                val: Store::Static(b"1"),
            }),
            &mut kawa,
        );
        assert!(
            !cont,
            "post-error call() must short-circuit so we do not append frames after RST_STREAM"
        );
    }

    /// `finalize` commits the abort in the SAME prepare pass:
    /// - `kawa.out` carries one RST_STREAM(InternalError) frame so the
    ///   following `flush_stream_out` ships it before the retirement
    ///   gate retires the stream;
    /// - `kawa.parsing_phase` becomes `Error{Processing(InternalError)}`;
    /// - `kawa.blocks` is cleared so no follow-on HEADERS/DATA leak;
    /// - the abort flag is reset so the converter is reusable.
    #[test]
    fn test_converter_finalize_commits_oversized_abort() {
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        conv.stream_id = 7;
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf, Kind::Response);

        conv.pending_oversized_abort = true;
        kawa.blocks.push_back(Block::Header(Pair {
            key: Store::Static(b"x-leftover"),
            val: Store::Static(b"1"),
        }));
        kawa.parsing_phase = ParsingPhase::Headers;

        BlockConverter::finalize(&mut conv, &mut kawa);

        // RST_STREAM frame shape: 9-byte header + 4-byte payload = 13 bytes.
        let mut bytes = Vec::new();
        for store in &kawa.out {
            if let kawa::OutBlock::Store(s) = store {
                bytes.extend_from_slice(s.data(kawa.storage.buffer()));
            }
        }
        assert_eq!(
            bytes.len(),
            parser::FRAME_HEADER_SIZE + parser::RST_STREAM_PAYLOAD_SIZE as usize,
            "finalize must push the RST_STREAM frame to kawa.out in the same pass"
        );
        assert_eq!(bytes[3], 0x03, "frame type byte must be RST_STREAM");
        assert_eq!(
            u32::from_be_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]) & 0x7fff_ffff,
            7,
            "stream_id must match converter.stream_id"
        );
        assert_eq!(
            u32::from_be_bytes([bytes[9], bytes[10], bytes[11], bytes[12]]),
            H2Error::InternalError as u32,
            "error code must be InternalError"
        );
        match kawa.parsing_phase {
            ParsingPhase::Error {
                marker,
                kind: ParsingErrorKind::Processing { message },
            } => {
                assert_eq!(marker, kawa::ParsingPhaseMarker::Headers);
                assert_eq!(message, H2Error::InternalError.as_str());
            }
            other => panic!("expected Error{{Processing(InternalError)}}, got {other:?}"),
        }
        assert!(
            kawa.blocks.is_empty(),
            "remaining blocks must be drained so they do not emit after RST_STREAM"
        );
        assert!(!conv.pending_oversized_abort, "abort flag must reset");
    }

    /// After `finalize` flipped `kawa.parsing_phase` to `Error{Processing}`,
    /// the next prepare cycle's `initialize` must synthesise a single
    /// RST_STREAM(InternalError) frame and push it onto `kawa.out`.
    #[test]
    fn test_converter_initialize_emits_rst_stream_on_error_phase() {
        let mut encoder = loona_hpack::Encoder::new();
        let mut conv = test_converter(&mut encoder);
        conv.stream_id = 5;
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf, Kind::Response);
        kawa.parsing_phase = ParsingPhase::Error {
            marker: kawa::ParsingPhaseMarker::Headers,
            kind: ParsingErrorKind::Processing {
                message: H2Error::InternalError.as_str(),
            },
        };

        BlockConverter::initialize(&mut conv, &mut kawa);

        // RST_STREAM frame: 9-byte header + 4-byte payload (error code).
        let mut bytes = Vec::new();
        for store in &kawa.out {
            if let kawa::OutBlock::Store(s) = store {
                bytes.extend_from_slice(s.data(kawa.storage.buffer()));
            }
        }
        assert_eq!(
            bytes.len(),
            parser::FRAME_HEADER_SIZE + parser::RST_STREAM_PAYLOAD_SIZE as usize,
            "exactly one RST_STREAM frame must be emitted"
        );
        // Payload type = 0x03 (RST_STREAM), stream_id = 5, error code = 2 (InternalError).
        assert_eq!(bytes[3], 0x03, "frame type must be RST_STREAM");
        assert_eq!(
            u32::from_be_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]) & 0x7fff_ffff,
            5,
            "stream id must match"
        );
        assert_eq!(
            u32::from_be_bytes([bytes[9], bytes[10], bytes[11], bytes[12]]),
            H2Error::InternalError as u32,
            "error code must be InternalError"
        );
    }
}
