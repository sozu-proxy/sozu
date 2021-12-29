use std::convert::From;

use nom::{
    bytes::streaming::{tag, take},
    combinator::{complete, map, map_opt},
    error::{ErrorKind, ParseError},
    multi::many0,
    number::streaming::{be_u16, be_u24, be_u32, be_u8},
    sequence::tuple,
    Err, HexDisplay, IResult, Offset,
};

#[derive(Clone, Debug, PartialEq)]
pub struct FrameHeader {
    pub payload_len: u32,
    pub frame_type: FrameType,
    pub flags: u8,
    pub stream_id: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FrameType {
    Data,
    Headers,
    Priority,
    RstStream,
    Settings,
    PushPromise,
    Ping,
    GoAway,
    WindowUpdate,
    Continuation,
}

/*
const NO_ERROR: u32 = 0x0;
const PROTOCOL_ERROR: u32 = 0x1;
const INTERNAL_ERROR: u32 = 0x2;
const FLOW_CONTROL_ERROR: u32 = 0x3;
const SETTINGS_TIMEOUT: u32 = 0x4;
const STREAM_CLOSED: u32 = 0x5;
const FRAME_SIZE_ERROR: u32 = 0x6;
const REFUSED_STREAM: u32 = 0x7;
const CANCEL: u32 = 0x8;
const COMPRESSION_ERROR: u32 = 0x9;
const CONNECT_ERROR: u32 = 0xa;
const ENHANCE_YOUR_CALM: u32 = 0xb;
const INADEQUATE_SECURITY: u32 = 0xc;
const HTTP_1_1_REQUIRED: u32 = 0xd;
*/

#[derive(Clone, Debug, PartialEq)]
pub struct Error<'a> {
    pub input: &'a [u8],
    pub error: InnerError,
}

#[derive(Clone, Debug, PartialEq)]
pub enum InnerError {
    Nom(ErrorKind),
    NoError,
    ProtocolError,
    InternalError,
    FlowControlError,
    SettingsTimeout,
    StreamClosed,
    FrameSizeError,
    RefusedStream,
    Cancel,
    CompressionError,
    ConnectError,
    EnhanceYourCalm,
    InadequateSecurity,
    HTTP11Required,
}

impl<'a> Error<'a> {
    pub fn new(input: &'a [u8], error: InnerError) -> Error<'a> {
        Error { input, error }
    }
}

impl<'a> ParseError<&'a [u8]> for Error<'a> {
    fn from_error_kind(input: &'a [u8], kind: ErrorKind) -> Self {
        Error {
            input,
            error: InnerError::Nom(kind),
        }
    }

    fn append(input: &'a [u8], kind: ErrorKind, other: Self) -> Self {
        Error {
            input,
            error: InnerError::Nom(kind),
        }
    }
}

impl<'a> From<(&'a [u8], ErrorKind)> for Error<'a> {
    fn from((input, kind): (&'a [u8], ErrorKind)) -> Self {
        Error {
            input,
            error: InnerError::Nom(kind),
        }
    }
}

pub fn preface(i: &[u8]) -> IResult<&[u8], &[u8]> {
    tag(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")(i)
}

// https://httpwg.org/specs/rfc7540.html#rfc.section.4.1
/*named!(pub frame_header<FrameHeader>,
  do_parse!(
    payload_len: dbg_dmp!(be_u24) >>
    frame_type: map_opt!(be_u8, convert_frame_type) >>
    flags: dbg_dmp!(be_u8) >>
    stream_id: dbg_dmp!(verify!(be_u32, |id| {
      match frame_type {

      }
    }) >>
    (FrameHeader { payload_len, frame_type, flags, stream_id })
  )
);
  */

pub fn frame_header(input: &[u8]) -> IResult<&[u8], FrameHeader, Error> {
    let (i1, payload_len) = be_u24(input)?;
    let (i2, frame_type) = map_opt(be_u8, convert_frame_type)(i1)?;
    let (i3, flags) = be_u8(i2)?;
    let (i4, stream_id) = be_u32(i3)?;

    Ok((
        i4,
        FrameHeader {
            payload_len,
            frame_type,
            flags,
            stream_id,
        },
    ))
}

fn convert_frame_type(t: u8) -> Option<FrameType> {
    info!("got frame type: {}", t);
    match t {
        0 => Some(FrameType::Data),
        1 => Some(FrameType::Headers),
        2 => Some(FrameType::Priority),
        3 => Some(FrameType::RstStream),
        4 => Some(FrameType::Settings),
        5 => Some(FrameType::PushPromise),
        6 => Some(FrameType::Ping),
        7 => Some(FrameType::GoAway),
        8 => Some(FrameType::WindowUpdate),
        9 => Some(FrameType::Continuation),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Frame<'a> {
    Data(Data<'a>),
    Headers(Headers<'a>),
    Priority,
    RstStream(RstStream),
    Settings(Settings),
    PushPromise,
    Ping(Ping),
    GoAway,
    WindowUpdate(WindowUpdate),
    Continuation,
}

impl<'a> Frame<'a> {
    pub fn is_stream_specific(&self) -> bool {
        match self {
            Frame::Data(_)
            | Frame::Headers(_)
            | Frame::Priority
            | Frame::RstStream(_)
            | Frame::PushPromise
            | Frame::Continuation => true,
            Frame::Settings(_) | Frame::Ping(_) | Frame::GoAway => false,
            Frame::WindowUpdate(w) => w.stream_id != 0,
        }
    }

    pub fn stream_id(&self) -> u32 {
        match self {
            Frame::Data(d) => d.stream_id,
            Frame::Headers(h) => h.stream_id,
            Frame::Priority => unimplemented!(),
            Frame::RstStream(r) => r.stream_id,
            Frame::PushPromise => unimplemented!(),
            Frame::Continuation => unimplemented!(),
            Frame::Settings(_) | Frame::Ping(_) | Frame::GoAway => 0,
            Frame::WindowUpdate(w) => w.stream_id,
        }
    }
}

pub fn frame<'a>(input: &'a [u8], max_frame_size: u32) -> IResult<&'a [u8], Frame<'a>, Error<'a>> {
    let (i, header) = frame_header(input)?;

    info!("got frame header: {:?}", header);

    if header.payload_len > max_frame_size {
        return Err(Err::Failure(Error::new(input, InnerError::FrameSizeError)));
    }

    let valid_stream_id = match header.frame_type {
        FrameType::Data
        | FrameType::Headers
        | FrameType::Priority
        | FrameType::RstStream
        | FrameType::PushPromise
        | FrameType::Continuation => header.stream_id != 0,
        FrameType::Settings | FrameType::Ping | FrameType::GoAway => header.stream_id == 0,
        FrameType::WindowUpdate => true,
    };

    if !valid_stream_id {
        return Err(Err::Failure(Error::new(input, InnerError::ProtocolError)));
    }

    let f = match header.frame_type {
        FrameType::Data => data_frame(i, &header)?,
        FrameType::Headers => headers_frame(i, &header)?,
        FrameType::Priority => {
            if header.payload_len != 5 {
                return Err(Err::Failure(Error::new(input, InnerError::FrameSizeError)));
            }
            unimplemented!();
        }
        FrameType::RstStream => {
            if header.payload_len != 4 {
                return Err(Err::Failure(Error::new(input, InnerError::FrameSizeError)));
            }
            rst_stream_frame(i, &header)?
        }
        FrameType::PushPromise => {
            unimplemented!();
        }
        FrameType::Continuation => {
            unimplemented!();
        }
        FrameType::Settings => {
            if header.payload_len % 6 != 0 {
                return Err(Err::Failure(Error::new(input, InnerError::FrameSizeError)));
            }
            settings_frame(i, &header)?
        }
        FrameType::Ping => {
            if header.payload_len != 8 {
                return Err(Err::Failure(Error::new(input, InnerError::FrameSizeError)));
            }
            ping_frame(i, &header)?
        }
        FrameType::GoAway => {
            unimplemented!();
        }
        FrameType::WindowUpdate => {
            if header.payload_len != 4 {
                return Err(Err::Failure(Error::new(input, InnerError::FrameSizeError)));
            }
            window_update_frame(i, &header)?
        }
    };

    Ok(f)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Data<'a> {
    pub stream_id: u32,
    pub payload: &'a [u8],
    pub end_stream: bool,
}

pub fn data_frame<'a, 'b>(
    input: &'a [u8],
    header: &'b FrameHeader,
) -> IResult<&'a [u8], Frame<'a>, Error<'a>> {
    let (remaining, i) = take(header.payload_len)(input)?;

    let (i1, pad_length) = if header.flags & 0x8 != 0 {
        let (i, pad_length) = be_u8(i)?;
        (i, Some(pad_length))
    } else {
        (i, None)
    };

    if pad_length.is_some() && i1.len() <= pad_length.unwrap() as usize {
        return Err(Err::Failure(Error::new(input, InnerError::ProtocolError)));
    }

    let (_, payload) = take(i1.len() - pad_length.unwrap_or(0) as usize)(i1)?;

    Ok((
        remaining,
        Frame::Data(Data {
            stream_id: header.stream_id,
            payload,
            end_stream: header.flags & 0x1 != 0,
        }),
    ))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Headers<'a> {
    pub stream_id: u32,
    pub stream_dependency: Option<StreamDependency>,
    pub weight: Option<u8>,
    pub header_block_fragment: &'a [u8],
    pub end_stream: bool,
    pub end_headers: bool,
    pub priority: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StreamDependency {
    pub exclusive: bool,
    pub stream_id: u32,
}

pub fn headers_frame<'a, 'b>(
    input: &'a [u8],
    header: &'b FrameHeader,
) -> IResult<&'a [u8], Frame<'a>, Error<'a>> {
    let (remaining, i) = take(header.payload_len)(input)?;

    let (i1, pad_length) = if header.flags & 0x8 != 0 {
        let (i, pad_length) = be_u8(i)?;
        (i, Some(pad_length))
    } else {
        (i, None)
    };

    let (i2, stream_dependency) = if header.flags & 0x20 != 0 {
        let (i, stream) = map(be_u32, |i| StreamDependency {
            exclusive: i & 0x8000 != 0,
            stream_id: i & 0x7FFF,
        })(i1)?;
        (i, Some(stream))
    } else {
        (i1, None)
    };

    let (i3, weight) = if header.flags & 0x20 != 0 {
        let (i, weight) = be_u8(i2)?;
        (i, Some(weight))
    } else {
        (i2, None)
    };

    if pad_length.is_some() && i3.len() <= pad_length.unwrap() as usize {
        return Err(Err::Failure(Error::new(input, InnerError::ProtocolError)));
    }

    let (_, header_block_fragment) = take(i3.len() - pad_length.unwrap_or(0) as usize)(i3)?;

    Ok((
        remaining,
        Frame::Headers(Headers {
            stream_id: header.stream_id,
            stream_dependency,
            weight,
            header_block_fragment,
            end_stream: header.flags & 0x1 != 0,
            end_headers: header.flags & 0x4 != 0,
            priority: header.flags & 0x20 != 0,
        }),
    ))
}

#[derive(Clone, Debug, PartialEq)]
pub struct RstStream {
    pub stream_id: u32,
    pub error_code: u32,
}

pub fn rst_stream_frame<'a, 'b>(
    input: &'a [u8],
    header: &'b FrameHeader,
) -> IResult<&'a [u8], Frame<'a>, Error<'a>> {
    let (i, error_code) = be_u32(input)?;
    Ok((
        i,
        Frame::RstStream(RstStream {
            stream_id: header.stream_id,
            error_code,
        }),
    ))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub settings: Vec<Setting>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Setting {
    pub identifier: u16,
    pub value: u32,
}

pub fn settings_frame<'a, 'b>(
    input: &'a [u8],
    header: &'b FrameHeader,
) -> IResult<&'a [u8], Frame<'a>, Error<'a>> {
    let (i, data) = take(header.payload_len)(input)?;

    let (_, settings) = many0(map(
        complete(tuple((be_u16, be_u32))),
        |(identifier, value)| Setting { identifier, value },
    ))(data)?;

    Ok((i, Frame::Settings(Settings { settings })))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Ping {
    pub payload: [u8; 8],
}

pub fn ping_frame<'a, 'b>(
    input: &'a [u8],
    header: &'b FrameHeader,
) -> IResult<&'a [u8], Frame<'a>, Error<'a>> {
    let (i, data) = take(8usize)(input)?;

    let mut p = Ping { payload: [0; 8] };

    for i in 0..8 {
        p.payload[i] = data[i];
    }

    Ok((i, Frame::Ping(p)))
}

#[derive(Clone, Debug, PartialEq)]
pub struct WindowUpdate {
    pub stream_id: u32,
    pub increment: u32,
}

pub fn window_update_frame<'a, 'b>(
    input: &'a [u8],
    header: &'b FrameHeader,
) -> IResult<&'a [u8], Frame<'a>, Error<'a>> {
    let (i, increment) = be_u32(input)?;
    let increment = increment & 0x7FFF;

    //FIXME: if stream id is 0, trat it as connection error?
    if increment == 0 {
        return Err(Err::Failure(Error::new(input, InnerError::ProtocolError)));
    }

    Ok((
        i,
        Frame::WindowUpdate(WindowUpdate {
            stream_id: header.stream_id,
            increment,
        }),
    ))
}

#[macro_export]
macro_rules! map_err(
  (__impl $i:expr, $submac:ident!( $($args:tt)* ), $g:expr) => (
    ($submac!($i, $($args)*)).map_err(|e| {
      $g(e)
    })
  );
);
