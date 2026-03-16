use std::cmp::min;

use kawa::{
    AsBuffer, Block, BlockConverter, Chunk, Flags, Kawa, Pair, ParsingErrorKind, ParsingPhase,
    StatusLine, Store,
};

use crate::protocol::{
    http::parser::compare_no_case,
    mux::{
        StreamId,
        parser::{self, FrameHeader, FrameType, H2Error},
        pkawa::is_connection_specific_header,
        serializer::{gen_frame_header, gen_rst_stream},
    },
};

pub struct H2BlockConverter<'a> {
    pub max_frame_size: usize,
    pub window: i32,
    pub stream_id: StreamId,
    pub encoder: &'a mut loona_hpack::Encoder<'static>,
    pub out: Vec<u8>,
    pub scheme: &'static [u8],
    /// Reusable buffer for lowercasing header keys, avoiding per-header allocation.
    pub lowercase_buf: Vec<u8>,
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
                    error!("failed to serialize RST_STREAM frame: {:?}", e);
                    return;
                }
                kawa.push_out(Store::from_slice(&frame));
            }
            ParsingPhase::Error { .. } => {
                let mut frame =
                    [0; parser::FRAME_HEADER_SIZE + parser::RST_STREAM_PAYLOAD_SIZE as usize];
                if let Err(e) = gen_rst_stream(&mut frame, self.stream_id, H2Error::InternalError) {
                    error!("failed to serialize RST_STREAM frame: {:?}", e);
                    return;
                }
                kawa.push_out(Store::from_slice(&frame));
            }
            _ => {}
        }
    }
    fn call(&mut self, block: Block, kawa: &mut Kawa<T>) -> bool {
        let buffer = kawa.storage.buffer();
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
                        error!("HPACK encoding of :method pseudo-header failed: {:?}", e);
                        return false;
                    }
                    if let Err(e) = self
                        .encoder
                        .encode_header_into((b":authority", authority.data(buffer)), &mut self.out)
                    {
                        error!("HPACK encoding of :authority pseudo-header failed: {:?}", e);
                        return false;
                    }
                    if let Err(e) = self
                        .encoder
                        .encode_header_into((b":path", path.data(buffer)), &mut self.out)
                    {
                        error!("HPACK encoding of :path pseudo-header failed: {:?}", e);
                        return false;
                    }
                    if let Err(e) = self
                        .encoder
                        .encode_header_into((b":scheme", self.scheme), &mut self.out)
                    {
                        error!("HPACK encoding of :scheme pseudo-header failed: {:?}", e);
                        return false;
                    }
                }
                StatusLine::Response { status, .. } => {
                    if let Err(e) = self
                        .encoder
                        .encode_header_into((b":status", status.data(buffer)), &mut self.out)
                    {
                        error!("HPACK encoding of :status pseudo-header failed: {:?}", e);
                        return false;
                    }
                }
                StatusLine::Unknown => {
                    error!("status line must be Request or Response before H2 conversion");
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
                    let cookie = [cookie.key.data(buffer), b"=", cookie.val.data(buffer)].concat();
                    if let Err(e) = self
                        .encoder
                        .encode_header_into((b"cookie", &cookie), &mut self.out)
                    {
                        error!("HPACK encoding of cookie header failed: {:?}", e);
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
                    if is_connection_specific_header(key)
                        || compare_no_case(key, b"host")
                        || compare_no_case(key, b"http2-settings")
                        || (compare_no_case(key, b"te") && !compare_no_case(val, b"trailers"))
                        || compare_no_case(key, b"trailer")
                    {
                        return true;
                    }
                }
                self.lowercase_buf.clear();
                self.lowercase_buf.extend_from_slice(key.data(buffer));
                self.lowercase_buf.make_ascii_lowercase();
                // RFC 9113 §8.2: reject header names with control chars or high bytes
                if self.lowercase_buf.iter().any(|&b| b <= 0x20 || b >= 0x7f) {
                    error!("H1->H2 header name contains invalid characters, skipping");
                    return true; // skip this header, continue with next
                }
                if let Err(e) = self
                    .encoder
                    .encode_header_into((&self.lowercase_buf, val.data(buffer)), &mut self.out)
                {
                    error!("HPACK encoding of header failed: {:?}", e);
                    return false;
                }
            }
            Block::ChunkHeader(_) => {
                // this converter doesn't align H1 chunks on H2 data frames
            }
            Block::Chunk(Chunk { data }) => {
                let mut header = [0; parser::FRAME_HEADER_SIZE];
                let payload_len = data.len();
                let (data, payload_len, can_continue) = if self.window >= payload_len as i32
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
                        self.max_frame_size < self.window as usize,
                    )
                } else {
                    // the window can't take any more bytes, return the chunk to the blocks
                    trace!(
                        "H2 flow control stall: stream={} connection_window={} pending_bytes={}",
                        self.stream_id,
                        self.window,
                        data.len()
                    );
                    incr!("h2.flow_control_stall");
                    kawa.blocks.push_front(Block::Chunk(Chunk { data }));
                    return false;
                };
                self.window -= payload_len as i32;
                if let Err(e) = gen_frame_header(
                    &mut header,
                    &FrameHeader {
                        payload_len,
                        frame_type: FrameType::Data,
                        flags: 0,
                        stream_id: self.stream_id,
                    },
                ) {
                    error!("failed to serialize DATA frame header: {:?}", e);
                    return false;
                }
                kawa.push_out(Store::from_slice(&header));
                kawa.push_out(data);
                // kawa.push_delimiter();
                return can_continue;
            }
            Block::Flags(Flags {
                end_header,
                end_stream,
                ..
            }) => {
                let sent_end_stream = if end_header {
                    let payload = std::mem::take(&mut self.out);
                    let mut header = [0; parser::FRAME_HEADER_SIZE];
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
                                error!("failed to serialize HEADERS frame header: {:?}", e);
                                return false;
                            }
                        } else if let Err(e) = gen_frame_header(
                            &mut header,
                            &FrameHeader {
                                payload_len: chunk.len() as u32,
                                frame_type: FrameType::Continuation,
                                flags,
                                stream_id: self.stream_id,
                            },
                        ) {
                            error!("failed to serialize CONTINUATION frame header: {:?}", e);
                            return false;
                        }
                        kawa.push_out(Store::from_slice(&header));
                        kawa.push_out(Store::from_slice(chunk));
                    }
                    n_chunks > 0
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
                        error!("failed to serialize empty DATA frame header: {:?}", e);
                        return false;
                    }
                    kawa.push_out(Store::from_slice(&header));
                }
            }
        }
        true
    }
    fn finalize(&mut self, _kawa: &mut Kawa<T>) {
        if !self.out.is_empty() {
            error!(
                "H2BlockConverter finalize: out buffer not empty ({} bytes remaining), clearing",
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
}
