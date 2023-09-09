use cookie_factory::{
    bytes::{be_u16, be_u24, be_u32, be_u8},
    combinator::slice,
    gen,
    sequence::tuple,
    GenError,
};

use super::{
    h2::H2Settings,
    parser::{FrameHeader, FrameType, H2Error},
};

pub const H2_PRI: &str = "PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
pub const SETTINGS_ACKNOWLEDGEMENT: [u8; 9] = [0, 0, 0, 4, 1, 0, 0, 0, 0];
pub const PING_ACKNOWLEDGEMENT_HEADER: [u8; 9] = [0, 0, 0, 6, 1, 0, 0, 0, 0];

pub fn gen_frame_header<'a, 'b>(
    buf: &'a mut [u8],
    frame: &'b FrameHeader,
) -> Result<(&'a mut [u8], usize), GenError> {
    let serializer = tuple((
        be_u24(frame.payload_len),
        be_u8(serialize_frame_type(&frame.frame_type)),
        be_u8(frame.flags),
        be_u32(frame.stream_id),
    ));

    gen(serializer, buf).map(|(buf, size)| (buf, size as usize))
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

// pub fn gen_settings_acknoledgement<'a>(buf: &'a mut [u8]) {
//     for (i, b) in SETTINGS_ACKNOWLEDGEMENT.iter().enumerate() {
//         buf[i] = *b;
//     }
// }

pub fn gen_ping_acknolegment<'a>(
    buf: &'a mut [u8],
    payload: &[u8],
) -> Result<(&'a mut [u8], usize), GenError> {
    gen(
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
            payload_len: 6 * 6,
            frame_type: FrameType::Settings,
            flags: 0,
            stream_id: 0,
        },
    )
    .and_then(|(buf, old_size)| {
        gen(
            tuple((
                be_u16(1),
                be_u32(settings.settings_header_table_size),
                be_u16(2),
                be_u32(settings.settings_enable_push as u32),
                be_u16(3),
                be_u32(settings.settings_max_concurrent_streams),
                be_u16(4),
                be_u32(settings.settings_initial_window_size),
                be_u16(5),
                be_u32(settings.settings_max_frame_size),
                be_u16(6),
                be_u32(settings.settings_max_header_list_size),
            )),
            buf,
        )
        .map(|(buf, size)| (buf, (old_size + size as usize)))
    })
}

pub fn gen_rst_stream<'a>(
    buf: &'a mut [u8],
    stream_id: u32,
    error_code: H2Error,
) -> Result<(&'a mut [u8], usize), GenError> {
    gen_frame_header(
        buf,
        &FrameHeader {
            payload_len: 4,
            frame_type: FrameType::RstStream,
            flags: 0,
            stream_id,
        },
    )
    .and_then(|(buf, old_size)| {
        gen(be_u32(error_code as u32), buf).map(|(buf, size)| (buf, (old_size + size as usize)))
    })
}
