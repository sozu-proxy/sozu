use std::convert::From;

use kawa::repr::Slice;
use nom::{
    bytes::complete::{tag, take},
    combinator::{complete, map},
    error::{ErrorKind, ParseError},
    multi::many0,
    number::complete::{be_u16, be_u24, be_u32, be_u8},
    sequence::tuple,
    Err, IResult,
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

pub fn error_code_to_str(error_code: u32) -> &'static str {
    match error_code {
        NO_ERROR => "NO_ERROR",
        PROTOCOL_ERROR => "PROTOCOL_ERROR",
        INTERNAL_ERROR => "INTERNAL_ERROR",
        FLOW_CONTROL_ERROR => "FLOW_CONTROL_ERROR",
        SETTINGS_TIMEOUT => "SETTINGS_TIMEOUT",
        STREAM_CLOSED => "STREAM_CLOSED",
        FRAME_SIZE_ERROR => "FRAME_SIZE_ERROR",
        REFUSED_STREAM => "REFUSED_STREAM",
        CANCEL => "CANCEL",
        COMPRESSION_ERROR => "COMPRESSION_ERROR",
        CONNECT_ERROR => "CONNECT_ERROR",
        ENHANCE_YOUR_CALM => "ENHANCE_YOUR_CALM",
        INADEQUATE_SECURITY => "INADEQUATE_SECURITY",
        HTTP_1_1_REQUIRED => "HTTP_1_1_REQUIRED",
        _ => "UNKNOWN_ERROR",
    }
}

pub fn str_to_error_code(str: &str) -> H2Error {
    match str {
        "NO_ERROR" => H2Error::NoError,
        "PROTOCOL_ERROR" => H2Error::ProtocolError,
        "INTERNAL_ERROR" => H2Error::InternalError,
        "FLOW_CONTROL_ERROR" => H2Error::FlowControlError,
        "SETTINGS_TIMEOUT" => H2Error::SettingsTimeout,
        "STREAM_CLOSED" => H2Error::StreamClosed,
        "FRAME_SIZE_ERROR" => H2Error::FrameSizeError,
        "REFUSED_STREAM" => H2Error::RefusedStream,
        "CANCEL" => H2Error::Cancel,
        "COMPRESSION_ERROR" => H2Error::CompressionError,
        "CONNECT_ERROR" => H2Error::ConnectError,
        "ENHANCE_YOUR_CALM" => H2Error::EnhanceYourCalm,
        "INADEQUATE_SECURITY" => H2Error::InadequateSecurity,
        "HTTP_1_1_REQUIRED" => H2Error::HTTP11Required,
        _ => H2Error::InternalError,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParserError<'a> {
    pub input: &'a [u8],
    pub kind: ParserErrorKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ParserErrorKind {
    Nom(ErrorKind),
    H2(H2Error),
    UnknownFrame(u32),
}

#[derive(Clone, Debug, PartialEq)]
pub enum H2Error {
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

impl<'a> ParserError<'a> {
    pub fn new(input: &'a [u8], error: ParserErrorKind) -> ParserError<'a> {
        ParserError { input, kind: error }
    }
    pub fn new_h2(input: &'a [u8], error: H2Error) -> ParserError<'a> {
        ParserError {
            input,
            kind: ParserErrorKind::H2(error),
        }
    }
}

impl<'a> ParseError<&'a [u8]> for ParserError<'a> {
    fn from_error_kind(input: &'a [u8], kind: ErrorKind) -> Self {
        ParserError {
            input,
            kind: ParserErrorKind::Nom(kind),
        }
    }

    fn append(input: &'a [u8], kind: ErrorKind, _other: Self) -> Self {
        ParserError {
            input,
            kind: ParserErrorKind::Nom(kind),
        }
    }
}

impl<'a> From<(&'a [u8], ErrorKind)> for ParserError<'a> {
    fn from((input, kind): (&'a [u8], ErrorKind)) -> Self {
        ParserError {
            input,
            kind: ParserErrorKind::Nom(kind),
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

pub fn frame_header(input: &[u8], max_frame_size: u32) -> IResult<&[u8], FrameHeader, ParserError> {
    let (i, payload_len) = be_u24(input)?;
    if payload_len > max_frame_size {
        return Err(Err::Failure(ParserError::new_h2(
            i,
            H2Error::FrameSizeError,
        )));
    }

    let (i, t) = be_u8(i)?;
    let Some(frame_type) = convert_frame_type(t) else {
        return Err(Err::Failure(ParserError::new(
            i,
            ParserErrorKind::UnknownFrame(payload_len),
        )));
    };
    let (i, flags) = be_u8(i)?;
    let (i, stream_id) = be_u32(i)?;
    let stream_id = stream_id & 0x7FFFFFFF;

    let valid_stream_id = match frame_type {
        FrameType::Data
        | FrameType::Headers
        | FrameType::Priority
        | FrameType::RstStream
        | FrameType::PushPromise
        | FrameType::Continuation => stream_id != 0,
        FrameType::Settings | FrameType::Ping | FrameType::GoAway => stream_id == 0,
        FrameType::WindowUpdate => true,
    };
    if !valid_stream_id {
        error!("invalid stream_id: {}", stream_id);
        return Err(Err::Failure(ParserError::new_h2(i, H2Error::ProtocolError)));
    }

    Ok((
        i,
        FrameHeader {
            payload_len,
            frame_type,
            flags,
            stream_id,
        },
    ))
}

fn convert_frame_type(t: u8) -> Option<FrameType> {
    debug!("got frame type: {}", t);
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

#[derive(Clone, Debug)]
pub enum Frame {
    Data(Data),
    Headers(Headers),
    Priority(Priority),
    RstStream(RstStream),
    Settings(Settings),
    PushPromise(PushPromise),
    Ping(Ping),
    GoAway(GoAway),
    WindowUpdate(WindowUpdate),
    Continuation(Continuation),
}

impl Frame {
    pub fn is_stream_specific(&self) -> bool {
        match self {
            Frame::Data(_)
            | Frame::Headers(_)
            | Frame::Priority(_)
            | Frame::RstStream(_)
            | Frame::PushPromise(_)
            | Frame::Continuation(_) => true,
            Frame::Settings(_) | Frame::Ping(_) | Frame::GoAway(_) => false,
            Frame::WindowUpdate(w) => w.stream_id != 0,
        }
    }

    pub fn stream_id(&self) -> u32 {
        match self {
            Frame::Data(d) => d.stream_id,
            Frame::Headers(h) => h.stream_id,
            Frame::Priority(p) => p.stream_id,
            Frame::RstStream(r) => r.stream_id,
            Frame::PushPromise(p) => p.stream_id,
            Frame::Continuation(c) => c.stream_id,
            Frame::Settings(_) | Frame::Ping(_) | Frame::GoAway(_) => 0,
            Frame::WindowUpdate(w) => w.stream_id,
        }
    }
}

pub fn frame_body<'a>(
    i: &'a [u8],
    header: &FrameHeader,
) -> IResult<&'a [u8], Frame, ParserError<'a>> {
    let f = match header.frame_type {
        FrameType::Data => data_frame(i, header)?,
        FrameType::Headers => headers_frame(i, header)?,
        FrameType::Priority => {
            if header.payload_len != 5 {
                return Err(Err::Failure(ParserError::new_h2(
                    i,
                    H2Error::FrameSizeError,
                )));
            }
            priority_frame(i, header)?
        }
        FrameType::RstStream => {
            if header.payload_len != 4 {
                return Err(Err::Failure(ParserError::new_h2(
                    i,
                    H2Error::FrameSizeError,
                )));
            }
            rst_stream_frame(i, header)?
        }
        FrameType::PushPromise => push_promise_frame(i, header)?,
        FrameType::Continuation => continuation_frame(i, header)?,
        FrameType::Settings => {
            if header.payload_len % 6 != 0 {
                return Err(Err::Failure(ParserError::new_h2(
                    i,
                    H2Error::FrameSizeError,
                )));
            }
            settings_frame(i, header)?
        }
        FrameType::Ping => {
            if header.payload_len != 8 {
                return Err(Err::Failure(ParserError::new_h2(
                    i,
                    H2Error::FrameSizeError,
                )));
            }
            ping_frame(i, header)?
        }
        FrameType::GoAway => goaway_frame(i, header)?,
        FrameType::WindowUpdate => {
            if header.payload_len != 4 {
                return Err(Err::Failure(ParserError::new_h2(
                    i,
                    H2Error::FrameSizeError,
                )));
            }
            window_update_frame(i, header)?
        }
    };

    Ok(f)
}

#[derive(Clone, Debug)]
pub struct Data {
    pub stream_id: u32,
    pub payload: Slice,
    pub end_stream: bool,
}

pub fn data_frame<'a>(
    input: &'a [u8],
    header: &FrameHeader,
) -> IResult<&'a [u8], Frame, ParserError<'a>> {
    let (remaining, i) = take(header.payload_len)(input)?;

    let (i, pad_length) = if header.flags & 0x8 != 0 {
        let (i, pad_length) = be_u8(i)?;
        (i, Some(pad_length))
    } else {
        (i, None)
    };

    if pad_length.is_some() && i.len() <= pad_length.unwrap() as usize {
        return Err(Err::Failure(ParserError::new_h2(
            input,
            H2Error::ProtocolError,
        )));
    }

    let (_, payload) = take(i.len() - pad_length.unwrap_or(0) as usize)(i)?;

    Ok((
        remaining,
        Frame::Data(Data {
            stream_id: header.stream_id,
            payload: Slice::new(input, payload),
            end_stream: header.flags & 0x1 != 0,
        }),
    ))
}

#[derive(Clone, Debug)]
pub struct Headers {
    pub stream_id: u32,
    pub priority: Option<PriorityPart>,
    pub header_block_fragment: Slice,
    // pub header_block_fragment: &'a [u8],
    pub end_stream: bool,
    pub end_headers: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StreamDependency {
    pub exclusive: bool,
    pub stream_id: u32,
}

fn stream_dependency(i: &[u8]) -> IResult<&[u8], StreamDependency, ParserError<'_>> {
    let (i, stream) = map(be_u32, |i| StreamDependency {
        exclusive: i & 0x8000 != 0,
        stream_id: i & 0x7FFFFFFF,
    })(i)?;
    Ok((i, stream))
}

pub fn headers_frame<'a>(
    input: &'a [u8],
    header: &FrameHeader,
) -> IResult<&'a [u8], Frame, ParserError<'a>> {
    let (remaining, i) = take(header.payload_len)(input)?;

    let (i, pad_length) = if header.flags & 0x8 != 0 {
        let (i, pad_length) = be_u8(i)?;
        (i, Some(pad_length))
    } else {
        (i, None)
    };

    let (i, priority) = if header.flags & 0x20 != 0 {
        let (i, stream_dependency) = stream_dependency(i)?;
        let (i, weight) = be_u8(i)?;
        (
            i,
            Some(PriorityPart::Rfc7540 {
                stream_dependency,
                weight,
            }),
        )
    } else {
        (i, None)
    };

    if pad_length.is_some() && i.len() <= pad_length.unwrap() as usize {
        return Err(Err::Failure(ParserError::new_h2(
            input,
            H2Error::ProtocolError,
        )));
    }

    let (_, header_block_fragment) = take(i.len() - pad_length.unwrap_or(0) as usize)(i)?;

    Ok((
        remaining,
        Frame::Headers(Headers {
            stream_id: header.stream_id,
            priority,
            header_block_fragment: Slice::new(input, header_block_fragment),
            end_stream: header.flags & 0x1 != 0,
            end_headers: header.flags & 0x4 != 0,
        }),
    ))
}

#[derive(Clone, Debug, PartialEq)]
pub enum PriorityPart {
    Rfc7540 {
        stream_dependency: StreamDependency,
        weight: u8,
    },
    Rfc9218 {
        urgency: u8, // should be between 0 and 7 inclusive
        incremental: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Priority {
    pub stream_id: u32,
    pub inner: PriorityPart,
}

pub fn priority_frame<'a>(
    input: &'a [u8],
    header: &FrameHeader,
) -> IResult<&'a [u8], Frame, ParserError<'a>> {
    let (i, stream_dependency) = stream_dependency(input)?;
    let (i, weight) = be_u8(i)?;
    Ok((
        i,
        Frame::Priority(Priority {
            stream_id: header.stream_id,
            inner: PriorityPart::Rfc7540 {
                stream_dependency,
                weight,
            },
        }),
    ))
}

#[derive(Clone, Debug, PartialEq)]
pub struct RstStream {
    pub stream_id: u32,
    pub error_code: u32,
}

pub fn rst_stream_frame<'a>(
    input: &'a [u8],
    header: &FrameHeader,
) -> IResult<&'a [u8], Frame, ParserError<'a>> {
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
    pub ack: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Setting {
    pub identifier: u16,
    pub value: u32,
}

pub fn settings_frame<'a>(
    input: &'a [u8],
    header: &FrameHeader,
) -> IResult<&'a [u8], Frame, ParserError<'a>> {
    let (i, data) = take(header.payload_len)(input)?;

    let (_, settings) = many0(map(
        complete(tuple((be_u16, be_u32))),
        |(identifier, value)| Setting { identifier, value },
    ))(data)?;

    Ok((
        i,
        Frame::Settings(Settings {
            settings,
            ack: header.flags & 0x1 != 0,
        }),
    ))
}

#[derive(Clone, Debug)]
pub struct PushPromise {
    pub stream_id: u32,
    pub promised_stream_id: u32,
    pub header_block_fragment: Slice,
    pub end_headers: bool,
}

pub fn push_promise_frame<'a>(
    input: &'a [u8],
    header: &FrameHeader,
) -> IResult<&'a [u8], Frame, ParserError<'a>> {
    let (remaining, i) = take(header.payload_len)(input)?;

    let (i, pad_length) = if header.flags & 0x8 != 0 {
        let (i, pad_length) = be_u8(i)?;
        (i, Some(pad_length))
    } else {
        (i, None)
    };

    if pad_length.is_some() && i.len() <= pad_length.unwrap() as usize {
        return Err(Err::Failure(ParserError::new_h2(
            input,
            H2Error::ProtocolError,
        )));
    }

    let (i, promised_stream_id) = be_u32(i)?;
    let (_, header_block_fragment) = take(i.len() - pad_length.unwrap_or(0) as usize)(i)?;

    Ok((
        remaining,
        Frame::PushPromise(PushPromise {
            stream_id: header.stream_id,
            promised_stream_id,
            header_block_fragment: Slice::new(input, header_block_fragment),
            end_headers: header.flags & 0x4 != 0,
        }),
    ))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Ping {
    pub payload: [u8; 8],
    pub ack: bool,
}

pub fn ping_frame<'a>(
    input: &'a [u8],
    header: &FrameHeader,
) -> IResult<&'a [u8], Frame, ParserError<'a>> {
    let (i, data) = take(8usize)(input)?;

    let mut p = Ping {
        payload: [0; 8],
        ack: header.flags & 1 != 0,
    };
    p.payload[..8].copy_from_slice(&data[..8]);

    Ok((i, Frame::Ping(p)))
}

#[derive(Clone, Debug)]
pub struct GoAway {
    pub last_stream_id: u32,
    pub error_code: u32,
    pub additional_debug_data: Slice,
}

pub fn goaway_frame<'a>(
    input: &'a [u8],
    header: &FrameHeader,
) -> IResult<&'a [u8], Frame, ParserError<'a>> {
    let (remaining, i) = take(header.payload_len)(input)?;
    let (i, last_stream_id) = be_u32(i)?;
    let (additional_debug_data, error_code) = be_u32(i)?;
    Ok((
        remaining,
        Frame::GoAway(GoAway {
            last_stream_id,
            error_code,
            additional_debug_data: Slice::new(input, additional_debug_data),
        }),
    ))
}

#[derive(Clone, Debug, PartialEq)]
pub struct WindowUpdate {
    pub stream_id: u32,
    pub increment: u32,
}

pub fn window_update_frame<'a>(
    input: &'a [u8],
    header: &FrameHeader,
) -> IResult<&'a [u8], Frame, ParserError<'a>> {
    let (i, increment) = be_u32(input)?;
    let increment = increment & 0x7FFFFFFF;

    //FIXME: if stream id is 0, trat it as connection error?
    if increment == 0 {
        return Err(Err::Failure(ParserError::new_h2(
            input,
            H2Error::ProtocolError,
        )));
    }

    Ok((
        i,
        Frame::WindowUpdate(WindowUpdate {
            stream_id: header.stream_id,
            increment,
        }),
    ))
}

#[derive(Clone, Debug)]
pub struct Continuation {
    pub stream_id: u32,
    pub header_block_fragment: Slice,
    pub end_headers: bool,
}

pub fn continuation_frame<'a>(
    input: &'a [u8],
    header: &FrameHeader,
) -> IResult<&'a [u8], Frame, ParserError<'a>> {
    let (i, header_block_fragment) = take(header.payload_len)(input)?;
    Ok((
        i,
        Frame::Continuation(Continuation {
            stream_id: header.stream_id,
            header_block_fragment: Slice::new(input, header_block_fragment),
            end_headers: header.flags & 0x4 != 0,
        }),
    ))
}
