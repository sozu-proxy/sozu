use cookie_factory::{
    GenError,
    bytes::{be_u8, be_u16, be_u24, be_u32},
    combinator::slice,
    r#gen,
    sequence::tuple,
};

use crate::protocol::mux::{
    h2::H2Settings,
    parser::{self, FrameHeader, FrameType, H2Error},
};

pub const H2_PRI: &str = "PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
pub const SETTINGS_ACKNOWLEDGEMENT: [u8; 9] = [0, 0, 0, 4, 1, 0, 0, 0, 0];
pub const PING_ACKNOWLEDGEMENT_HEADER: [u8; 9] = [0, 0, 8, 6, 1, 0, 0, 0, 0];

pub fn gen_frame_header<'a>(
    buf: &'a mut [u8],
    frame: &FrameHeader,
) -> Result<(&'a mut [u8], usize), GenError> {
    let serializer = tuple((
        be_u24(frame.payload_len),
        be_u8(serialize_frame_type(&frame.frame_type)),
        be_u8(frame.flags),
        be_u32(frame.stream_id),
    ));

    r#gen(serializer, buf).map(|(buf, size)| (buf, size as usize))
}

pub fn serialize_frame_type(f: &FrameType) -> u8 {
    match *f {
        FrameType::Data => 0,
        FrameType::Headers => 1,
        FrameType::Priority => 2,
        FrameType::RstStream => 3,
        FrameType::Settings => 4,
        FrameType::PushPromise => 5,
        FrameType::Ping => 6,
        FrameType::GoAway => 7,
        FrameType::WindowUpdate => 8,
        FrameType::Continuation => 9,
    }
}

pub fn gen_ping_acknowledgement<'a>(
    buf: &'a mut [u8],
    payload: &[u8],
) -> Result<(&'a mut [u8], usize), GenError> {
    r#gen(
        tuple((slice(PING_ACKNOWLEDGEMENT_HEADER), slice(payload))),
        buf,
    )
    .map(|(buf, size)| (buf, size as usize))
}

pub fn gen_settings<'a>(
    buf: &'a mut [u8],
    settings: &H2Settings,
) -> Result<(&'a mut [u8], usize), GenError> {
    gen_frame_header(
        buf,
        &FrameHeader {
            payload_len: parser::SETTINGS_ENTRY_SIZE * parser::SETTINGS_COUNT,
            frame_type: FrameType::Settings,
            flags: 0,
            stream_id: 0,
        },
    )
    .and_then(|(buf, old_size)| {
        r#gen(
            tuple((
                be_u16(parser::SETTINGS_HEADER_TABLE_SIZE),
                be_u32(settings.settings_header_table_size),
                be_u16(parser::SETTINGS_ENABLE_PUSH),
                be_u32(settings.settings_enable_push as u32),
                be_u16(parser::SETTINGS_MAX_CONCURRENT_STREAMS),
                be_u32(settings.settings_max_concurrent_streams),
                be_u16(parser::SETTINGS_INITIAL_WINDOW_SIZE),
                be_u32(settings.settings_initial_window_size),
                be_u16(parser::SETTINGS_MAX_FRAME_SIZE),
                be_u32(settings.settings_max_frame_size),
                be_u16(parser::SETTINGS_MAX_HEADER_LIST_SIZE),
                be_u32(settings.settings_max_header_list_size),
                be_u16(parser::SETTINGS_ENABLE_CONNECT_PROTOCOL),
                be_u32(settings.settings_enable_connect_protocol as u32),
                be_u16(parser::SETTINGS_NO_RFC7540_PRIORITIES),
                be_u32(settings.settings_no_rfc7540_priorities as u32),
            )),
            buf,
        )
        .map(|(buf, size)| (buf, (old_size + size as usize)))
    })
}

pub fn gen_rst_stream(
    buf: &mut [u8],
    stream_id: u32,
    error_code: H2Error,
) -> Result<(&mut [u8], usize), GenError> {
    gen_frame_header(
        buf,
        &FrameHeader {
            payload_len: parser::RST_STREAM_PAYLOAD_SIZE,
            frame_type: FrameType::RstStream,
            flags: 0,
            stream_id,
        },
    )
    .and_then(|(buf, old_size)| {
        r#gen(be_u32(error_code as u32), buf).map(|(buf, size)| (buf, (old_size + size as usize)))
    })
}

/// Serialize a WINDOW_UPDATE frame (RFC 9113 §6.9).
/// Payload is 4 bytes: 1 reserved bit + 31-bit window size increment.
pub fn gen_window_update(
    buf: &mut [u8],
    stream_id: u32,
    increment: u32,
) -> Result<(&mut [u8], usize), GenError> {
    gen_frame_header(
        buf,
        &FrameHeader {
            payload_len: parser::WINDOW_UPDATE_PAYLOAD_SIZE,
            frame_type: FrameType::WindowUpdate,
            flags: 0,
            stream_id,
        },
    )
    .and_then(|(buf, old_size)| {
        r#gen(be_u32(increment & parser::STREAM_ID_MASK), buf)
            .map(|(buf, size)| (buf, (old_size + size as usize)))
    })
}

pub fn gen_goaway(
    buf: &mut [u8],
    last_stream_id: u32,
    error_code: H2Error,
) -> Result<(&mut [u8], usize), GenError> {
    gen_frame_header(
        buf,
        &FrameHeader {
            payload_len: parser::GOAWAY_PAYLOAD_SIZE,
            frame_type: FrameType::GoAway,
            flags: 0,
            stream_id: 0,
        },
    )
    .and_then(|(buf, old_size)| {
        r#gen(
            tuple((be_u32(last_stream_id), be_u32(error_code as u32))),
            buf,
        )
        .map(|(buf, size)| (buf, (old_size + size as usize)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::mux::parser;

    /// Helper: serialize a FrameHeader into a fresh 9-byte buffer and return the bytes written.
    fn serialize_header(header: &parser::FrameHeader) -> ([u8; 9], usize) {
        let mut buf = [0u8; 9];
        let (_, sz) = gen_frame_header(&mut buf[..], header).expect("serialization should succeed");
        (buf, sz)
    }

    /// Helper: parse a FrameHeader from a byte slice.
    fn parse_header(buf: &[u8]) -> parser::FrameHeader {
        let (remaining, header) =
            parser::frame_header(buf, 16_777_215).expect("parsing should succeed");
        assert!(remaining.is_empty(), "all bytes should be consumed");
        header
    }

    #[test]
    fn roundtrip_data_frame_header() {
        let original = parser::FrameHeader {
            payload_len: 100,
            frame_type: parser::FrameType::Data,
            flags: 0x1, // END_STREAM
            stream_id: 1,
        };

        let (buf, sz) = serialize_header(&original);
        assert_eq!(sz, 9, "frame header is always 9 bytes");

        let parsed = parse_header(&buf);
        assert_eq!(parsed, original);
    }

    #[test]
    fn roundtrip_headers_frame_header() {
        let original = parser::FrameHeader {
            payload_len: 256,
            frame_type: parser::FrameType::Headers,
            flags: 0x25, // END_STREAM | END_HEADERS | PRIORITY
            stream_id: 3,
        };

        let (buf, sz) = serialize_header(&original);
        assert_eq!(sz, 9);

        let parsed = parse_header(&buf);
        assert_eq!(parsed, original);
    }

    #[test]
    fn roundtrip_settings_frame_header() {
        let original = parser::FrameHeader {
            payload_len: 36,
            frame_type: parser::FrameType::Settings,
            flags: 0x0,
            stream_id: 0,
        };

        let (buf, sz) = serialize_header(&original);
        assert_eq!(sz, 9);

        let parsed = parse_header(&buf);
        assert_eq!(parsed, original);
    }

    #[test]
    fn roundtrip_settings_ack_header() {
        let original = parser::FrameHeader {
            payload_len: 0,
            frame_type: parser::FrameType::Settings,
            flags: 0x1, // ACK
            stream_id: 0,
        };

        let (buf, sz) = serialize_header(&original);
        assert_eq!(sz, 9);

        let parsed = parse_header(&buf);
        assert_eq!(parsed, original);
    }

    #[test]
    fn roundtrip_rst_stream_header() {
        let original = parser::FrameHeader {
            payload_len: 4,
            frame_type: parser::FrameType::RstStream,
            flags: 0x0,
            stream_id: 7,
        };

        let (buf, sz) = serialize_header(&original);
        assert_eq!(sz, 9);

        let parsed = parse_header(&buf);
        assert_eq!(parsed, original);
    }

    #[test]
    fn roundtrip_window_update_header() {
        let original = parser::FrameHeader {
            payload_len: 4,
            frame_type: parser::FrameType::WindowUpdate,
            flags: 0x0,
            stream_id: 0,
        };

        let (buf, sz) = serialize_header(&original);
        assert_eq!(sz, 9);

        let parsed = parse_header(&buf);
        assert_eq!(parsed, original);
    }

    #[test]
    fn roundtrip_window_update_stream_level() {
        let original = parser::FrameHeader {
            payload_len: 4,
            frame_type: parser::FrameType::WindowUpdate,
            flags: 0x0,
            stream_id: 5,
        };

        let (buf, sz) = serialize_header(&original);
        assert_eq!(sz, 9);

        let parsed = parse_header(&buf);
        assert_eq!(parsed, original);
    }

    #[test]
    fn roundtrip_ping_header() {
        let original = parser::FrameHeader {
            payload_len: 8,
            frame_type: parser::FrameType::Ping,
            flags: 0x1, // ACK
            stream_id: 0,
        };

        let (buf, sz) = serialize_header(&original);
        assert_eq!(sz, 9);

        let parsed = parse_header(&buf);
        assert_eq!(parsed, original);
    }

    #[test]
    fn roundtrip_goaway_header() {
        let original = parser::FrameHeader {
            payload_len: 8,
            frame_type: parser::FrameType::GoAway,
            flags: 0x0,
            stream_id: 0,
        };

        let (buf, sz) = serialize_header(&original);
        assert_eq!(sz, 9);

        let parsed = parse_header(&buf);
        assert_eq!(parsed, original);
    }

    #[test]
    fn roundtrip_continuation_header() {
        let original = parser::FrameHeader {
            payload_len: 128,
            frame_type: parser::FrameType::Continuation,
            flags: 0x4, // END_HEADERS
            stream_id: 9,
        };

        let (buf, sz) = serialize_header(&original);
        assert_eq!(sz, 9);

        let parsed = parse_header(&buf);
        assert_eq!(parsed, original);
    }

    #[test]
    fn roundtrip_all_frame_types() {
        let frame_types = [
            (parser::FrameType::Data, 0u8),
            (parser::FrameType::Headers, 1),
            (parser::FrameType::Priority, 2),
            (parser::FrameType::RstStream, 3),
            (parser::FrameType::Settings, 4),
            (parser::FrameType::PushPromise, 5),
            (parser::FrameType::Ping, 6),
            (parser::FrameType::GoAway, 7),
            (parser::FrameType::WindowUpdate, 8),
            (parser::FrameType::Continuation, 9),
        ];

        for (ft, expected_byte) in &frame_types {
            assert_eq!(
                serialize_frame_type(ft),
                *expected_byte,
                "serialize_frame_type mismatch for {ft:?}"
            );

            // Use stream_id that is valid for the frame type
            let stream_id = match ft {
                parser::FrameType::Settings
                | parser::FrameType::Ping
                | parser::FrameType::GoAway => 0,
                _ => 1,
            };

            let header = parser::FrameHeader {
                payload_len: 0,
                frame_type: ft.to_owned(),
                flags: 0,
                stream_id,
            };

            let (buf, sz) = serialize_header(&header);
            assert_eq!(sz, 9);

            let parsed = parse_header(&buf);
            assert_eq!(parsed.frame_type, *ft, "round-trip failed for {ft:?}");
        }
    }

    #[test]
    fn roundtrip_large_payload_len() {
        // payload_len is a u24 (max 16_777_215)
        // Use a large value that is still within u24 but also within the max_frame_size
        // we pass to parse_header (16_777_215).
        let original = parser::FrameHeader {
            payload_len: 16_777_215,
            frame_type: parser::FrameType::Data,
            flags: 0x0,
            stream_id: 1,
        };

        let (buf, sz) = serialize_header(&original);
        assert_eq!(sz, 9);

        let parsed = parse_header(&buf);
        assert_eq!(parsed, original);
    }

    #[test]
    fn roundtrip_zero_payload_len() {
        let original = parser::FrameHeader {
            payload_len: 0,
            frame_type: parser::FrameType::Data,
            flags: 0x0,
            stream_id: 1,
        };

        let (buf, sz) = serialize_header(&original);
        assert_eq!(sz, 9);

        let parsed = parse_header(&buf);
        assert_eq!(parsed, original);
    }

    #[test]
    fn roundtrip_max_stream_id() {
        let original = parser::FrameHeader {
            payload_len: 4,
            frame_type: parser::FrameType::WindowUpdate,
            flags: 0x0,
            stream_id: 0x7FFF_FFFF,
        };

        let (buf, sz) = serialize_header(&original);
        assert_eq!(sz, 9);

        let parsed = parse_header(&buf);
        assert_eq!(parsed, original);
    }

    #[test]
    fn roundtrip_all_flags_set() {
        let original = parser::FrameHeader {
            payload_len: 50,
            frame_type: parser::FrameType::Headers,
            flags: 0xFF,
            stream_id: 1,
        };

        let (buf, sz) = serialize_header(&original);
        assert_eq!(sz, 9);

        let parsed = parse_header(&buf);
        assert_eq!(parsed, original);
    }

    #[test]
    fn buffer_too_small_for_frame_header() {
        let header = parser::FrameHeader {
            payload_len: 10,
            frame_type: parser::FrameType::Data,
            flags: 0,
            stream_id: 1,
        };

        let mut buf = [0u8; 8]; // 8 bytes, need 9
        let result = gen_frame_header(&mut buf[..], &header);
        assert!(result.is_err(), "should fail with buffer too small");
    }

    #[test]
    fn serialized_bytes_match_expected_layout() {
        // Verify the exact byte layout matches RFC 7540 section 4.1
        let header = parser::FrameHeader {
            payload_len: 0x000102,                   // 258
            frame_type: parser::FrameType::Settings, // type = 4
            flags: 0xAB,
            stream_id: 0x0304_0506,
        };

        let (buf, _) = serialize_header(&header);
        // Length: 3 bytes big-endian
        assert_eq!(buf[0], 0x00);
        assert_eq!(buf[1], 0x01);
        assert_eq!(buf[2], 0x02);
        // Type: 1 byte
        assert_eq!(buf[3], 0x04); // Settings
        // Flags: 1 byte
        assert_eq!(buf[4], 0xAB);
        // Stream ID: 4 bytes big-endian
        assert_eq!(buf[5], 0x03);
        assert_eq!(buf[6], 0x04);
        assert_eq!(buf[7], 0x05);
        assert_eq!(buf[8], 0x06);
    }
}
