use cookie_factory::{GenError, gen, bytes::{be_u8, be_u24, be_u32}, sequence::tuple};
use super::parser::{FrameHeader,FrameType};

pub fn gen_frame_header<'a, 'b>(x: (&'a mut [u8], usize), frame: &'b FrameHeader) -> Result<(&'a mut [u8], usize), GenError> {
  let serializer = tuple((
    be_u24(frame.payload_len),
    be_u8(serialize_frame_type(&frame.frame_type)),
    be_u8(frame.flags),
    be_u32(frame.stream_id)
  ));

  gen(serializer, x.0).map(|(buf, sz)| (buf, sz as usize))
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
