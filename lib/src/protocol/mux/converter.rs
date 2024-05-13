use std::str::from_utf8_unchecked;

use kawa::{AsBuffer, Block, BlockConverter, Chunk, Flags, Kawa, Pair, StatusLine, Store};

use crate::protocol::http::parser::compare_no_case;

use super::{
    parser::{FrameHeader, FrameType, H2Error},
    serializer::{gen_frame_header, gen_rst_stream},
    StreamId,
};

pub struct H2BlockConverter<'a> {
    pub stream_id: StreamId,
    pub encoder: &'a mut hpack::Encoder<'static>,
    pub out: Vec<u8>,
}

impl<'a, T: AsBuffer> BlockConverter<T> for H2BlockConverter<'a> {
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
                    self.encoder
                        .encode_header_into((b":method", method.data(buffer)), &mut self.out)
                        .unwrap();
                    self.encoder
                        .encode_header_into((b":authority", authority.data(buffer)), &mut self.out)
                        .unwrap();
                    self.encoder
                        .encode_header_into((b":path", path.data(buffer)), &mut self.out)
                        .unwrap();
                    self.encoder
                        .encode_header_into((b":scheme", b"https"), &mut self.out)
                        .unwrap();
                }
                StatusLine::Response { status, .. } => {
                    self.encoder
                        .encode_header_into((b":status", status.data(buffer)), &mut self.out)
                        .unwrap();
                }
                StatusLine::Unknown => unreachable!(),
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
                    self.encoder
                        .encode_header_into((b"cookie", &cookie), &mut self.out)
                        .unwrap();
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
                    if compare_no_case(key, b"connection")
                        || compare_no_case(key, b"host")
                        || compare_no_case(key, b"http2-settings")
                        || compare_no_case(key, b"keep-alive")
                        || compare_no_case(key, b"proxy-connection")
                        || compare_no_case(key, b"te") && !compare_no_case(val, b"trailers")
                        || compare_no_case(key, b"trailer")
                        || compare_no_case(key, b"transfer-encoding")
                        || compare_no_case(key, b"upgrade")
                    {
                        println!("Elided H2 header: {}", unsafe { from_utf8_unchecked(key) });
                        return true;
                    }
                }
                self.encoder
                    .encode_header_into(
                        (&key.data(buffer).to_ascii_lowercase(), val.data(buffer)),
                        &mut self.out,
                    )
                    .unwrap();
            }
            Block::ChunkHeader(_) => {
                // this converter doesn't align H1 chunks on H2 data frames
            }
            Block::Chunk(Chunk { data }) => {
                let mut header = [0; 9];
                let payload_len = match &data {
                    Store::Empty => 0,
                    Store::Detached(s) | Store::Slice(s) => s.len,
                    Store::Static(s) => s.len() as u32,
                    Store::Alloc(a, i) => a.len() as u32 - i,
                };
                gen_frame_header(
                    &mut header,
                    &FrameHeader {
                        payload_len,
                        frame_type: FrameType::Data,
                        flags: 0,
                        stream_id: self.stream_id,
                    },
                )
                .unwrap();
                kawa.push_out(Store::from_slice(&header));
                kawa.push_out(data);
                kawa.push_delimiter()
            }
            Block::Flags(Flags {
                end_header,
                end_stream,
                ..
            }) => {
                if end_header {
                    let payload = std::mem::replace(&mut self.out, Vec::new());
                    let mut header = [0; 9];
                    let flags = if end_stream { 1 } else { 0 } | if end_header { 4 } else { 0 };
                    gen_frame_header(
                        &mut header,
                        &FrameHeader {
                            payload_len: payload.len() as u32,
                            frame_type: FrameType::Headers,
                            flags,
                            stream_id: self.stream_id,
                        },
                    )
                    .unwrap();
                    kawa.push_out(Store::from_slice(&header));
                    kawa.push_out(Store::Alloc(payload.into_boxed_slice(), 0));
                } else if end_stream {
                    if kawa.is_error() {
                        let mut frame = [0; 13];
                        gen_rst_stream(&mut frame, self.stream_id, H2Error::InternalError).unwrap();
                        kawa.push_out(Store::from_slice(&frame));
                    } else {
                        let mut header = [0; 9];
                        gen_frame_header(
                            &mut header,
                            &FrameHeader {
                                payload_len: 0,
                                frame_type: FrameType::Data,
                                flags: 1,
                                stream_id: self.stream_id,
                            },
                        )
                        .unwrap();
                        kawa.push_out(Store::from_slice(&header));
                    }
                }
                if end_header || end_stream {
                    kawa.push_delimiter()
                }
            }
        }
        true
    }
    fn finalize(&mut self, _kawa: &mut Kawa<T>) {
        assert!(self.out.is_empty());
    }
}
