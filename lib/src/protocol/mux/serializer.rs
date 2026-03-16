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
