//! H2 frame parser (nom-based).
//!
//! Decodes every RFC 9113 §6 frame type plus RFC 9218 `PRIORITY_UPDATE` and
//! unknown extension frames; rejects malformed framing with the matching
//! `H2Error` so the connection can react with GOAWAY / RST_STREAM. Mostly
//! zero-allocation, with bounded `Vec<u8>` allocations on `priority_update`
//! and a SETTINGS-list cap (cf. `priority_update_frame` and the SETTINGS
//! allocation cap test). Frame-size invariants are enforced via
//! `ensure_frame_size!`.

use std::convert::From;

use kawa::repr::Slice;
use nom::{
    Err, IResult,
    bytes::complete::{tag, take},
    combinator::{complete, map},
    error::{ErrorKind, ParseError},
    multi::many0,
    number::complete::{be_u8, be_u16, be_u24, be_u32},
    sequence::tuple,
};
use sozu_command::logging::ansi_palette;

/// Module-level prefix for nom-based H2 frame parser diagnostics. The parser
/// has no session in scope, so a single `MUX-PARSER` label is used, colored
/// bold bright-white (uniform across every protocol) when the logger supports
/// ANSI.
macro_rules! log_module_context {
    () => {{
        let (open, reset, _, _, _) = ansi_palette();
        format!("{open}MUX-PARSER{reset}\t >>>", open = open, reset = reset)
    }};
}

// ── RFC 9113 Wire Format Constants ──────────────────────────────────────────

/// H2 frame header size in bytes (RFC 9113 §4.1)
pub const FRAME_HEADER_SIZE: usize = 9;

/// Mask to extract 31-bit stream ID, clearing the reserved MSB (RFC 9113 §4.1)
pub const STREAM_ID_MASK: u32 = 0x7FFFFFFF;

// Frame flags (RFC 9113 §6)
/// END_STREAM flag — signals last frame for this stream (§6.1, §6.2)
pub const FLAG_END_STREAM: u8 = 0x1;
/// END_HEADERS flag — signals last header block fragment (§6.2, §6.10)
pub const FLAG_END_HEADERS: u8 = 0x4;
/// PADDED flag — indicates padding is present (§6.1, §6.2)
pub const FLAG_PADDED: u8 = 0x8;
/// PRIORITY flag on HEADERS — stream dependency follows (§6.2)
pub const FLAG_PRIORITY: u8 = 0x20;
/// ACK flag on SETTINGS/PING (§6.5, §6.7)
pub const FLAG_ACK: u8 = 0x1;

// Fixed-size frame payload lengths (RFC 9113)
pub const PRIORITY_PAYLOAD_SIZE: u32 = 5;
pub const RST_STREAM_PAYLOAD_SIZE: u32 = 4;
pub const SETTINGS_ENTRY_SIZE: u32 = 6;
pub const PING_PAYLOAD_SIZE: u32 = 8;
pub const WINDOW_UPDATE_PAYLOAD_SIZE: u32 = 4;
pub const GOAWAY_PAYLOAD_SIZE: u32 = 8;

// SETTINGS identifiers (RFC 9113 §6.5.1, RFC 8441, RFC 9218)
pub const SETTINGS_HEADER_TABLE_SIZE: u16 = 1;
pub const SETTINGS_ENABLE_PUSH: u16 = 2;
pub const SETTINGS_MAX_CONCURRENT_STREAMS: u16 = 3;
pub const SETTINGS_INITIAL_WINDOW_SIZE: u16 = 4;
pub const SETTINGS_MAX_FRAME_SIZE: u16 = 5;
pub const SETTINGS_MAX_HEADER_LIST_SIZE: u16 = 6;
pub const SETTINGS_ENABLE_CONNECT_PROTOCOL: u16 = 8;
pub const SETTINGS_NO_RFC7540_PRIORITIES: u16 = 9;
/// Number of settings entries we send in our SETTINGS frame
pub const SETTINGS_COUNT: u32 = 8;

// ─────────────────────────────────────────────────────────────────────────────

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
    /// RFC 9218 §7.1 — PRIORITY_UPDATE frame (type 0x10). Carries a new
    /// priority signal for a prioritized stream, replacing the deprecated
    /// `Priority` frame at the connection level.
    PriorityUpdate,
    /// Frame of unknown type per RFC 9113 §5.5. Implementations MUST ignore
    /// and discard these frames so future H2 extensions do not break
    /// interoperability. The associated `u8` is the raw type byte for
    /// diagnostics only.
    Unknown(u8),
}

impl std::str::FromStr for H2Error {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "NO_ERROR" => Ok(H2Error::NoError),
            "PROTOCOL_ERROR" => Ok(H2Error::ProtocolError),
            "INTERNAL_ERROR" => Ok(H2Error::InternalError),
            "FLOW_CONTROL_ERROR" => Ok(H2Error::FlowControlError),
            "SETTINGS_TIMEOUT" => Ok(H2Error::SettingsTimeout),
            "STREAM_CLOSED" => Ok(H2Error::StreamClosed),
            "FRAME_SIZE_ERROR" => Ok(H2Error::FrameSizeError),
            "REFUSED_STREAM" => Ok(H2Error::RefusedStream),
            "CANCEL" => Ok(H2Error::Cancel),
            "COMPRESSION_ERROR" => Ok(H2Error::CompressionError),
            "CONNECT_ERROR" => Ok(H2Error::ConnectError),
            "ENHANCE_YOUR_CALM" => Ok(H2Error::EnhanceYourCalm),
            "INADEQUATE_SECURITY" => Ok(H2Error::InadequateSecurity),
            "HTTP_1_1_REQUIRED" => Ok(H2Error::HTTP11Required),
            _ => Err(()),
        }
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
}

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(u32)]
pub enum H2Error {
    NoError = 0x0,
    ProtocolError = 0x1,
    InternalError = 0x2,
    FlowControlError = 0x3,
    SettingsTimeout = 0x4,
    StreamClosed = 0x5,
    FrameSizeError = 0x6,
    RefusedStream = 0x7,
    Cancel = 0x8,
    CompressionError = 0x9,
    ConnectError = 0xa,
    EnhanceYourCalm = 0xb,
    InadequateSecurity = 0xc,
    HTTP11Required = 0xd,
}

impl TryFrom<u32> for H2Error {
    type Error = u32;

    fn try_from(code: u32) -> Result<Self, u32> {
        match code {
            0x0 => Ok(H2Error::NoError),
            0x1 => Ok(H2Error::ProtocolError),
            0x2 => Ok(H2Error::InternalError),
            0x3 => Ok(H2Error::FlowControlError),
            0x4 => Ok(H2Error::SettingsTimeout),
            0x5 => Ok(H2Error::StreamClosed),
            0x6 => Ok(H2Error::FrameSizeError),
            0x7 => Ok(H2Error::RefusedStream),
            0x8 => Ok(H2Error::Cancel),
            0x9 => Ok(H2Error::CompressionError),
            0xa => Ok(H2Error::ConnectError),
            0xb => Ok(H2Error::EnhanceYourCalm),
            0xc => Ok(H2Error::InadequateSecurity),
            0xd => Ok(H2Error::HTTP11Required),
            other => Err(other),
        }
    }
}

impl H2Error {
    /// Returns the RFC 7540 §7 error name as a static string.
    pub const fn as_str(&self) -> &'static str {
        match self {
            H2Error::NoError => "NO_ERROR",
            H2Error::ProtocolError => "PROTOCOL_ERROR",
            H2Error::InternalError => "INTERNAL_ERROR",
            H2Error::FlowControlError => "FLOW_CONTROL_ERROR",
            H2Error::SettingsTimeout => "SETTINGS_TIMEOUT",
            H2Error::StreamClosed => "STREAM_CLOSED",
            H2Error::FrameSizeError => "FRAME_SIZE_ERROR",
            H2Error::RefusedStream => "REFUSED_STREAM",
            H2Error::Cancel => "CANCEL",
            H2Error::CompressionError => "COMPRESSION_ERROR",
            H2Error::ConnectError => "CONNECT_ERROR",
            H2Error::EnhanceYourCalm => "ENHANCE_YOUR_CALM",
            H2Error::InadequateSecurity => "INADEQUATE_SECURITY",
            H2Error::HTTP11Required => "HTTP_1_1_REQUIRED",
        }
    }
}

impl std::fmt::Display for H2Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
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

/// `if !$cond { return FrameSizeError; }` wrapping macro for the seven
/// fixed-size H2 frame validators in `frame_body`. Pure dispatch — the
/// nom Err / ParserError construction stays here so callers read like
/// the spec citation instead of a four-line ceremony.
///
/// `$cond` is the **passes** predicate (e.g. `header.payload_len ==
/// PING_PAYLOAD_SIZE`); the macro converts the negation into the bail.
macro_rules! ensure_frame_size {
    ($input:expr, $cond:expr) => {
        if !($cond) {
            return Err(Err::Failure(ParserError::new_h2(
                $input,
                H2Error::FrameSizeError,
            )));
        }
    };
}

// https://httpwg.org/specs/rfc7540.html#rfc.section.4.1
//
// Codex G12 invariant (production caller contract):
// `max_frame_size` MUST be the *negotiated* `settings_max_frame_size` from
// the peer's latest accepted SETTINGS frame (see `H2Settings` on
// `ConnectionH2`). It MUST NOT be `DEFAULT_MAX_FRAME_SIZE` — that RFC
// default (16 KiB) is only correct at connection-preface time, before the
// peer has had a chance to raise the limit. All four production callers
// in `lib/src/protocol/mux/h2.rs` (lines 1444, 1704, 1935, 1994) thread
// `self.local_settings.settings_max_frame_size` correctly. Tests may use
// `DEFAULT_MAX_FRAME_SIZE` freely because they construct isolated frame
// buffers that never flow through a real peer handshake.
pub fn frame_header(input: &[u8], max_frame_size: u32) -> IResult<&[u8], FrameHeader, ParserError> {
    let (i, payload_len) = be_u24(input)?;
    if payload_len > max_frame_size {
        return Err(Err::Failure(ParserError::new_h2(
            i,
            H2Error::FrameSizeError,
        )));
    }

    let (i, t) = be_u8(i)?;
    let frame_type = convert_frame_type(t);
    let (i, flags) = be_u8(i)?;
    let (i, stream_id) = be_u32(i)?;
    let stream_id = stream_id & STREAM_ID_MASK;

    // RFC 9113 §5.5: unknown frame types MUST be silently discarded. Skip
    // stream-id parity validation for them — the spec places no constraints on
    // the stream_id of extension frames, and the h2 state machine simply drops
    // the payload bytes.
    let valid_stream_id = match frame_type {
        FrameType::Data
        | FrameType::Headers
        | FrameType::Priority
        | FrameType::RstStream
        | FrameType::PushPromise
        | FrameType::Continuation => stream_id != 0,
        // RFC 9218 §7.1: PRIORITY_UPDATE is a connection-scoped signal
        // (the *prioritized* stream ID lives in the payload).
        FrameType::Settings | FrameType::Ping | FrameType::GoAway | FrameType::PriorityUpdate => {
            stream_id == 0
        }
        FrameType::WindowUpdate | FrameType::Unknown(_) => true,
    };
    if !valid_stream_id {
        error!("{} invalid stream_id: {}", log_module_context!(), stream_id);
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

/// Map a raw H2 frame type byte to its [`FrameType`] variant.
///
/// Unknown types are mapped to [`FrameType::Unknown`] so the caller can skip
/// the payload per RFC 9113 §5.5 ("Implementations MUST ignore and discard
/// frames of unknown type").
fn convert_frame_type(t: u8) -> FrameType {
    trace!("{} got frame type: {}", log_module_context!(), t);
    match t {
        0 => FrameType::Data,
        1 => FrameType::Headers,
        2 => FrameType::Priority,
        3 => FrameType::RstStream,
        4 => FrameType::Settings,
        5 => FrameType::PushPromise,
        6 => FrameType::Ping,
        7 => FrameType::GoAway,
        8 => FrameType::WindowUpdate,
        9 => FrameType::Continuation,
        // RFC 9218 §7.1 PRIORITY_UPDATE
        0x10 => FrameType::PriorityUpdate,
        other => FrameType::Unknown(other),
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
    /// RFC 9218 §7.1 PRIORITY_UPDATE — connection-scoped signal that
    /// re-prioritizes a specific stream. Payload carries the prioritized
    /// stream ID and the verbatim priority field value (structured field).
    PriorityUpdate(PriorityUpdate),
    /// Unknown frame type (RFC 9113 §5.5) — payload already consumed, the
    /// state machine MUST ignore it.
    Unknown(u8),
}

/// RFC 9218 §7.1 PRIORITY_UPDATE frame payload.
#[derive(Clone, Debug, PartialEq)]
pub struct PriorityUpdate {
    /// Identifier of the stream being re-prioritized (31-bit, reserved high
    /// bit masked off). `0` MUST be treated as a connection error
    /// (`PROTOCOL_ERROR`) by the handler.
    pub prioritized_stream_id: u32,
    /// Verbatim priority field value (SF-Item / ASCII). The handler passes
    /// this through the same `parse_rfc9218_priority` helper used for the
    /// `priority` request header.
    pub priority_field_value: Vec<u8>,
}

pub fn frame_body<'a>(
    i: &'a [u8],
    header: &FrameHeader,
) -> IResult<&'a [u8], Frame, ParserError<'a>> {
    let f = match header.frame_type {
        FrameType::Data => data_frame(i, header)?,
        FrameType::Headers => headers_frame(i, header)?,
        FrameType::Priority => {
            ensure_frame_size!(i, header.payload_len == PRIORITY_PAYLOAD_SIZE);
            priority_frame(i, header)?
        }
        FrameType::RstStream => {
            ensure_frame_size!(i, header.payload_len == RST_STREAM_PAYLOAD_SIZE);
            rst_stream_frame(i, header)?
        }
        FrameType::PushPromise => push_promise_frame(i, header)?,
        FrameType::Continuation => continuation_frame(i, header)?,
        FrameType::Settings => {
            // RFC 9113 §6.5: SETTINGS ACK with non-zero payload is FRAME_SIZE_ERROR
            ensure_frame_size!(
                i,
                !(header.flags & FLAG_ACK != 0 && header.payload_len != 0)
            );
            ensure_frame_size!(i, header.payload_len % SETTINGS_ENTRY_SIZE == 0);
            settings_frame(i, header)?
        }
        FrameType::Ping => {
            ensure_frame_size!(i, header.payload_len == PING_PAYLOAD_SIZE);
            ping_frame(i, header)?
        }
        FrameType::GoAway => {
            // RFC 9113 §6.8: GOAWAY payload is at least 8 bytes
            // (last-stream-id + error-code). Additional debug data may follow.
            ensure_frame_size!(i, header.payload_len >= GOAWAY_PAYLOAD_SIZE);
            goaway_frame(i, header)?
        }
        FrameType::WindowUpdate => {
            ensure_frame_size!(i, header.payload_len == WINDOW_UPDATE_PAYLOAD_SIZE);
            window_update_frame(i, header)?
        }
        // RFC 9218 §7.1: PRIORITY_UPDATE payload must be ≥ 4 bytes.
        FrameType::PriorityUpdate => priority_update_frame(i, header)?,
        // RFC 9113 §5.5: silently consume unknown frame payloads.
        FrameType::Unknown(_) => unknown_frame(i, header)?,
    };

    Ok(f)
}

#[derive(Clone, Debug)]
pub struct Data {
    pub stream_id: u32,
    pub payload: Slice,
    pub end_stream: bool,
}

/// Parse the padding prefix from a frame payload per RFC 9113 §6.1.
///
/// If `FLAG_PADDED` is set in `flags`, reads the 1-byte pad length and validates
/// it does not exceed the remaining data. Returns the content slice (after the
/// pad-length byte) and the number of padding bytes to trim from the end.
/// Returns `ProtocolError` when the pad length exceeds available data.
///
/// RFC 9113 §6.1: "The Pad Length field MUST be less than the length of the
/// frame payload". With a frame payload of length `N`, the valid pad_length
/// range is `0..=N-1`. After reading the 1-byte pad-length field, the
/// remaining slice has length `N-1`, so pad_length may be at most `N-1`,
/// i.e. `pad_length <= i.len()`. Only values strictly greater than `i.len()`
/// are invalid.
fn strip_padding<'a>(
    i: &'a [u8],
    flags: u8,
    error_input: &'a [u8],
) -> IResult<&'a [u8], u8, ParserError<'a>> {
    let (i, pad_length) = if flags & FLAG_PADDED != 0 {
        let (i, pad_length) = be_u8(i)?;
        (i, pad_length)
    } else {
        (i, 0)
    };

    if (pad_length as usize) > i.len() {
        return Err(Err::Failure(ParserError::new_h2(
            error_input,
            H2Error::ProtocolError,
        )));
    }

    Ok((i, pad_length))
}

/// Remove `pad_length` bytes of trailing padding from `i`, returning only
/// the content portion. Returns `ProtocolError` if the remaining input is
/// shorter than the declared padding — this is reachable in `headers_frame`
/// when PADDED and PRIORITY are both set and the 5-byte priority payload
/// leaves fewer bytes than `pad_length`.
fn unpad<'a>(
    i: &'a [u8],
    pad_length: u8,
    error_input: &'a [u8],
) -> Result<&'a [u8], Err<ParserError<'a>>> {
    let content_len = i
        .len()
        .checked_sub(pad_length as usize)
        .ok_or_else(|| Err::Failure(ParserError::new_h2(error_input, H2Error::ProtocolError)))?;
    Ok(&i[..content_len])
}

pub fn data_frame<'a>(
    input: &'a [u8],
    header: &FrameHeader,
) -> IResult<&'a [u8], Frame, ParserError<'a>> {
    let (remaining, i) = take(header.payload_len)(input)?;

    let (i, pad_length) = strip_padding(i, header.flags, input)?;
    let payload = unpad(i, pad_length, input)?;

    Ok((
        remaining,
        Frame::Data(Data {
            stream_id: header.stream_id,
            payload: Slice::new(input, payload),
            end_stream: header.flags & FLAG_END_STREAM != 0,
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
        exclusive: i & 0x80000000 != 0,
        stream_id: i & STREAM_ID_MASK,
    })(i)?;
    Ok((i, stream))
}

pub fn headers_frame<'a>(
    input: &'a [u8],
    header: &FrameHeader,
) -> IResult<&'a [u8], Frame, ParserError<'a>> {
    let (remaining, i) = take(header.payload_len)(input)?;

    let (i, pad_length) = strip_padding(i, header.flags, input)?;

    let (i, priority) = if header.flags & FLAG_PRIORITY != 0 {
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

    let header_block_fragment = unpad(i, pad_length, input)?;

    Ok((
        remaining,
        Frame::Headers(Headers {
            stream_id: header.stream_id,
            priority,
            header_block_fragment: Slice::new(input, header_block_fragment),
            end_stream: header.flags & FLAG_END_STREAM != 0,
            end_headers: header.flags & FLAG_END_HEADERS != 0,
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
    // Size-check in `frame_header` already rejected mismatches, but use
    // `take(header.payload_len)` to keep the fixed-size parsers structurally
    // symmetric with variable-size ones: if the upstream length invariant is
    // ever weakened we still consume exactly the declared payload and the
    // inner parse fails cleanly instead of reading random trailing bytes.
    let (i, data) = take(header.payload_len)(input)?;
    let (_, (stream_dependency, weight)) = tuple((stream_dependency, be_u8))(data)?;
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
    // `take(header.payload_len)` keeps the fixed-size parsers symmetric with
    // variable-size ones (see `priority_frame`). The framing layer already
    // enforced `payload_len == RST_STREAM_PAYLOAD_SIZE == 4`, so this
    // consumes exactly the 32-bit error-code field.
    let (i, data) = take(header.payload_len)(input)?;
    let (_, error_code) = be_u32(data)?;
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

/// Maximum SETTINGS entries accepted in a single SETTINGS frame.
///
/// RFC 9113 §6.5 defines 7 identifiers and RFC 7540/9218/RFC 8441 add a
/// handful more; a well-formed peer never sends more than a dozen. Capping
/// at 64 leaves generous headroom for future extensions while bounding the
/// per-frame allocation: without this cap a malicious peer could send a
/// MAX_FRAME_SIZE payload (16 MiB) composed entirely of six-byte
/// identifier/value pairs, forcing `settings_frame` to allocate ≈2.8M
/// `Setting` entries per connection (audit Pass 1 Low #7).
pub const MAX_SETTINGS_ENTRIES: usize = 64;

pub fn settings_frame<'a>(
    input: &'a [u8],
    header: &FrameHeader,
) -> IResult<&'a [u8], Frame, ParserError<'a>> {
    // Reject oversized SETTINGS before allocating. An entry is exactly 6
    // bytes (RFC 9113 §6.5.1 — 16-bit identifier + 32-bit value), so the
    // count is bounded by `payload_len / 6`.
    if header.payload_len / SETTINGS_ENTRY_SIZE > MAX_SETTINGS_ENTRIES as u32 {
        return Err(Err::Failure(ParserError::new_h2(
            input,
            H2Error::FrameSizeError,
        )));
    }

    let (i, data) = take(header.payload_len)(input)?;

    let (_, settings) = many0(map(
        complete(tuple((be_u16, be_u32))),
        |(identifier, value)| Setting { identifier, value },
    ))(data)?;

    Ok((
        i,
        Frame::Settings(Settings {
            settings,
            ack: header.flags & FLAG_ACK != 0,
        }),
    ))
}

/// PushPromise is always rejected with PROTOCOL_ERROR at the wire layer.
/// Sozu never announces `SETTINGS_ENABLE_PUSH=1`, and RFC 9113 §8.4 requires
/// that a peer which has not enabled server push treat a received
/// PUSH_PROMISE as a connection error of type PROTOCOL_ERROR. Rejecting in
/// the parser is defence-in-depth: any future refactor of `mux/h2.rs` that
/// forgets to call `handle_push_promise_frame` cannot silently accept push
/// traffic. The struct is retained so the outer match in `h2.rs` still
/// compiles, but `push_promise_frame` never constructs it.
#[derive(Clone, Debug)]
pub struct PushPromise;

pub fn push_promise_frame<'a>(
    input: &'a [u8],
    header: &FrameHeader,
) -> IResult<&'a [u8], Frame, ParserError<'a>> {
    // Consume the payload bytes first so the framing layer does not desync
    // if the caller ever demoted the error, then reject.
    let (_remaining, _payload) = take(header.payload_len)(input)?;
    Err(Err::Failure(ParserError::new_h2(
        input,
        H2Error::ProtocolError,
    )))
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
    // `take(header.payload_len)` (rather than `take(8usize)`) keeps the
    // fixed-size parsers symmetric with variable-size ones. The framing
    // layer already enforced `payload_len == PING_PAYLOAD_SIZE == 8`.
    let (i, data) = take(header.payload_len)(input)?;

    let mut p = Ping {
        payload: [0; 8],
        ack: header.flags & FLAG_ACK != 0,
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
    let (i, raw_last_stream_id) = be_u32(i)?;
    // RFC 9113 §6.8: reserved bit must be masked (same as frame_header stream_id)
    let last_stream_id = raw_last_stream_id & STREAM_ID_MASK;
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
    // Scope input to payload_len like other fixed-size parsers (rst_stream_frame,
    // ping_frame). The caller enforces payload_len == 4; the take() ensures a
    // future relaxation doesn't desynchronize the frame stream.
    let (i, data) = take(header.payload_len)(input)?;
    let (_, increment) = be_u32(data)?;
    let increment = increment & STREAM_ID_MASK;

    // NOTE: zero-increment validation is intentionally NOT performed here.
    // RFC 9113 §6.9 requires different error handling depending on whether the
    // WINDOW_UPDATE targets stream 0 (connection error) or a specific stream
    // (stream error). That distinction requires the stream_id context, which is
    // available in the H2 connection handler (handle_window_update_frame), not
    // in the wire-level parser.

    Ok((
        i,
        Frame::WindowUpdate(WindowUpdate {
            stream_id: header.stream_id,
            increment,
        }),
    ))
}

/// Continuation frames are handled inline during HEADERS parsing. The
/// wire-level parser does not track connection state (whether the previous
/// frame was HEADERS/PUSH_PROMISE with END_HEADERS=0), so standalone
/// CONTINUATION is rejected at the h2 state-machine layer
/// (`handle_header_state`): when a CONTINUATION frame header is observed
/// while the state is `H2State::Header` (i.e. no header block is in
/// progress) we emit GOAWAY(PROTOCOL_ERROR) per RFC 9113 §6.10.
#[derive(Clone, Debug)]
pub struct Continuation;

pub fn continuation_frame<'a>(
    input: &'a [u8],
    header: &FrameHeader,
) -> IResult<&'a [u8], Frame, ParserError<'a>> {
    // Consume the entire frame payload without storing fields
    let (remaining, _) = take(header.payload_len)(input)?;
    Ok((remaining, Frame::Continuation(Continuation)))
}

/// Silently consume the payload of a frame whose type byte is not recognised,
/// per RFC 9113 §5.5 ("Implementations MUST ignore and discard frames of
/// unknown type"). Previously covered RFC 9218 PRIORITY_UPDATE (type 0x10);
/// PRIORITY_UPDATE is now parsed through [`priority_update_frame`].
pub fn unknown_frame<'a>(
    input: &'a [u8],
    header: &FrameHeader,
) -> IResult<&'a [u8], Frame, ParserError<'a>> {
    let (remaining, _payload) = take(header.payload_len)(input)?;
    let raw = match header.frame_type {
        FrameType::Unknown(t) => t,
        _ => 0,
    };
    Ok((remaining, Frame::Unknown(raw)))
}

/// RFC 9218 §7.1 PRIORITY_UPDATE frame parser. Payload layout:
///
/// ```text
/// +-+-------------------------------------------------------------+
/// |R|              Prioritized Stream ID (31)                     |
/// +-+-------------------------------------------------------------+
/// |                    Priority Field Value (*)                 ...
/// +---------------------------------------------------------------+
/// ```
///
/// - 4-byte prioritized stream ID (reserved high bit masked off).
/// - Remainder: verbatim priority field value (ASCII / SF-Item).
///
/// A payload shorter than 4 bytes is `FRAME_SIZE_ERROR` per §7.1. Stream
/// ID value `0` is rejected by the handler (connection-level
/// `PROTOCOL_ERROR`), not here — keep the parser purely structural.
pub fn priority_update_frame<'a>(
    input: &'a [u8],
    header: &FrameHeader,
) -> IResult<&'a [u8], Frame, ParserError<'a>> {
    if header.payload_len < PRIORITY_UPDATE_MIN_PAYLOAD {
        return Err(Err::Failure(ParserError::new_h2(
            input,
            H2Error::FrameSizeError,
        )));
    }
    // RFC 9218 §7.1 priority field is an SF-Item (RFC 8941): ASCII-only,
    // small. Real-world emissions (`u=N, i, foo=...`) are < 30 bytes.
    // Cap at PRIORITY_UPDATE_MAX_VALUE (1024) — the same cap nghttp2 and
    // the `h2` Rust crate apply — so an attacker cannot drive
    // `value_bytes.to_vec()` to allocate ~16 KiB per frame
    // (SETTINGS_MAX_FRAME_SIZE default), turning a structurally-legitimate
    // PRIORITY_UPDATE stream into allocator pressure that the
    // H2FloodDetector glitch budget may not catch quickly.
    let value_len = (header.payload_len - PRIORITY_UPDATE_MIN_PAYLOAD) as usize;
    if value_len > PRIORITY_UPDATE_MAX_VALUE {
        return Err(Err::Failure(ParserError::new_h2(
            input,
            H2Error::ProtocolError,
        )));
    }
    let (remaining, payload) = take(header.payload_len)(input)?;
    let (raw_id_bytes, value_bytes) = payload.split_at(PRIORITY_UPDATE_MIN_PAYLOAD as usize);
    let prioritized_stream_id = u32::from_be_bytes([
        raw_id_bytes[0],
        raw_id_bytes[1],
        raw_id_bytes[2],
        raw_id_bytes[3],
    ]) & STREAM_ID_MASK;
    Ok((
        remaining,
        Frame::PriorityUpdate(PriorityUpdate {
            prioritized_stream_id,
            priority_field_value: value_bytes.to_vec(),
        }),
    ))
}

/// Minimum payload size for a PRIORITY_UPDATE frame (RFC 9218 §7.1):
/// the 4-byte prioritized stream ID. The priority field value is allowed
/// to be empty (defaults to the RFC 9218 §4 defaults).
pub const PRIORITY_UPDATE_MIN_PAYLOAD: u32 = 4;

/// Maximum length of the `priority_field_value` portion of a PRIORITY_UPDATE
/// frame (RFC 9218 §7.1). The field is a Structured Field Item (RFC 8941):
/// ASCII-only, intended for compact dictionary payloads such as `u=3, i`.
/// 1024 bytes mirrors nghttp2's `NGHTTP2_MAX_PRIORITY_VALUE_LEN` and the
/// upper-bound the `h2` Rust crate enforces, an order of magnitude tighter
/// than `SETTINGS_MAX_FRAME_SIZE` — the attacker is denied a per-frame
/// 16 KiB allocation primitive sized in single TCP segments.
pub const PRIORITY_UPDATE_MAX_VALUE: usize = 1024;

#[cfg(test)]
mod tests {
    use super::*;

    /// Default max frame size per RFC 9113 §6.5.2 (2^14 = 16384)
    const DEFAULT_MAX_FRAME_SIZE: u32 = 1 << 14;

    // ---- SETTINGS ACK with non-zero payload (C-2 regression) ----

    /// RFC 9113 §6.5: a SETTINGS frame with the ACK flag AND a non-empty
    /// payload is a FRAME_SIZE_ERROR. The parser now enforces this directly
    /// (moved from the mux layer for defense-in-depth).
    #[test]
    fn test_settings_ack_with_payload_rejected() {
        // SETTINGS ACK (flags=0x01) with 6-byte payload (one setting entry)
        let input = [
            0x00, 0x00, 0x06, // payload_len = 6
            0x04, // type = SETTINGS
            0x01, // flags = ACK
            0x00, 0x00, 0x00, 0x00, // stream_id = 0
            // payload: SETTINGS_MAX_CONCURRENT_STREAMS (0x0003) = 100 (0x00000064)
            0x00, 0x03, 0x00, 0x00, 0x00, 0x64,
        ];

        let (remaining, header) =
            frame_header(&input, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        assert_eq!(header.frame_type, FrameType::Settings);
        assert_eq!(header.flags & FLAG_ACK, FLAG_ACK);
        assert_eq!(header.payload_len, 6);

        let result = frame_body(remaining, &header);
        assert!(
            result.is_err(),
            "SETTINGS ACK with non-empty payload must be rejected"
        );
        match result {
            Err(nom::Err::Failure(e)) => {
                assert_eq!(e.kind, ParserErrorKind::H2(H2Error::FrameSizeError));
            }
            other => panic!("expected Failure(FrameSizeError), got {other:?}"),
        }
    }

    // ---- SETTINGS ACK with empty payload (valid) ----

    /// RFC 9113 §6.5: a SETTINGS ACK with zero-length payload is the only
    /// valid form. The parser must accept it and produce ack=true with no
    /// settings entries.
    #[test]
    fn test_settings_ack_empty_accepted() {
        let input = [
            0x00, 0x00, 0x00, // payload_len = 0
            0x04, // type = SETTINGS
            0x01, // flags = ACK
            0x00, 0x00, 0x00, 0x00, // stream_id = 0
        ];

        let (remaining, header) =
            frame_header(&input, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        assert_eq!(header.frame_type, FrameType::Settings);
        assert_eq!(header.flags & FLAG_ACK, FLAG_ACK);

        let (_, frame) = frame_body(remaining, &header).expect("frame body parses cleanly");
        match frame {
            Frame::Settings(settings) => {
                assert!(settings.ack, "ACK flag must be set");
                assert!(
                    settings.settings.is_empty(),
                    "should have no settings entries"
                );
            }
            other => panic!("expected Frame::Settings, got {other:?}"),
        }
    }

    // ---- WINDOW_UPDATE with max increment (flow control boundary) ----

    /// RFC 9113 §6.9: increment = 2^31-1 (0x7FFFFFFF) is the maximum valid
    /// window size increment. The parser must accept it.
    #[test]
    fn test_window_update_max_increment() {
        let input = [
            0x00, 0x00, 0x04, // payload_len = 4
            0x08, // type = WINDOW_UPDATE
            0x00, // flags = 0
            0x00, 0x00, 0x00, 0x01, // stream_id = 1
            0x7F, 0xFF, 0xFF, 0xFF, // increment = 2^31-1
        ];

        let (remaining, header) =
            frame_header(&input, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        assert_eq!(header.frame_type, FrameType::WindowUpdate);
        assert_eq!(header.stream_id, 1);

        let (_, frame) = frame_body(remaining, &header).expect("frame body parses cleanly");
        match frame {
            Frame::WindowUpdate(wu) => {
                assert_eq!(wu.increment, 0x7FFFFFFF);
                assert_eq!(wu.stream_id, 1);
            }
            other => panic!("expected Frame::WindowUpdate, got {other:?}"),
        }
    }

    // ---- WINDOW_UPDATE with zero increment (parser accepts, handler differentiates) ----

    /// RFC 9113 §6.9: zero-increment WINDOW_UPDATE is now parsed successfully
    /// by the wire parser. The connection vs stream error distinction is handled
    /// in handle_window_update_frame, not in the parser.
    #[test]
    fn test_window_update_zero_increment_parsed() {
        // Connection-level (stream_id = 0) zero increment
        let input = [
            0x00, 0x00, 0x04, // payload_len = 4
            0x08, // type = WINDOW_UPDATE
            0x00, // flags = 0
            0x00, 0x00, 0x00, 0x00, // stream_id = 0
            0x00, 0x00, 0x00, 0x00, // increment = 0
        ];

        let (remaining, header) =
            frame_header(&input, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        assert_eq!(header.frame_type, FrameType::WindowUpdate);

        let (_, frame) = frame_body(remaining, &header).expect("frame body parses cleanly");
        match frame {
            Frame::WindowUpdate(wu) => {
                assert_eq!(wu.stream_id, 0);
                assert_eq!(wu.increment, 0);
            }
            other => panic!("expected Frame::WindowUpdate, got {other:?}"),
        }

        // Stream-level (stream_id = 3) zero increment
        let input2 = [
            0x00, 0x00, 0x04, // payload_len = 4
            0x08, // type = WINDOW_UPDATE
            0x00, // flags = 0
            0x00, 0x00, 0x00, 0x03, // stream_id = 3
            0x00, 0x00, 0x00, 0x00, // increment = 0
        ];

        let (remaining2, header2) =
            frame_header(&input2, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        let (_, frame2) =
            frame_body(remaining2, &header2).expect("second frame body parses cleanly");
        match frame2 {
            Frame::WindowUpdate(wu) => {
                assert_eq!(wu.stream_id, 3);
                assert_eq!(wu.increment, 0);
            }
            other => panic!("expected Frame::WindowUpdate, got {other:?}"),
        }
    }

    // ---- Unknown frame type is silently discarded (RFC 9113 §5.5) ----

    /// RFC 9113 §5.5: "Implementations MUST ignore and discard frames of
    /// unknown type". The parser maps unknown type bytes to
    /// [`FrameType::Unknown`] and consumes the payload without erroring so
    /// H2 extensions (e.g. RFC 9218 PRIORITY_UPDATE, type 0x10) stay
    /// interoperable.
    #[test]
    fn test_unknown_frame_type_is_ignored() {
        let input = [
            0x00, 0x00, 0x04, // payload_len = 4
            0xFF, // type = unknown (255)
            0x00, // flags = 0
            0x00, 0x00, 0x00, 0x00, // stream_id = 0
            0xde, 0xad, 0xbe, 0xef, // arbitrary payload bytes
        ];

        let (remaining, header) = frame_header(&input, DEFAULT_MAX_FRAME_SIZE)
            .expect("unknown frame header must parse cleanly");
        assert!(matches!(header.frame_type, FrameType::Unknown(0xFF)));
        assert_eq!(header.payload_len, 4);

        let (after, frame) =
            frame_body(remaining, &header).expect("unknown frame body must be consumed");
        assert!(after.is_empty(), "payload bytes must be consumed");
        match frame {
            Frame::Unknown(0xFF) => {}
            other => panic!("expected Frame::Unknown(0xFF), got {other:?}"),
        }
    }

    /// RFC 9218 §7.1: PRIORITY_UPDATE with an empty priority field value +
    /// prioritized stream ID = 1 is a well-formed frame. The parser yields
    /// `Frame::PriorityUpdate` with an empty `priority_field_value` — the
    /// handler supplies the RFC 9218 §4 defaults.
    #[test]
    fn test_priority_update_empty_field_parses() {
        let input = [
            0x00, 0x00, 0x04, // payload_len = 4
            0x10, // type = PRIORITY_UPDATE (0x10)
            0x00, // flags = 0
            0x00, 0x00, 0x00, 0x00, // stream_id = 0 (connection-scoped)
            // prioritized stream ID (big-endian, 31-bit with reserved MSB)
            0x00, 0x00, 0x00, 0x01,
        ];

        let (remaining, header) = frame_header(&input, DEFAULT_MAX_FRAME_SIZE)
            .expect("PRIORITY_UPDATE header must parse");
        assert!(matches!(header.frame_type, FrameType::PriorityUpdate));
        let (_, frame) = frame_body(remaining, &header).expect("body must be consumed");
        match frame {
            Frame::PriorityUpdate(PriorityUpdate {
                prioritized_stream_id,
                ref priority_field_value,
            }) => {
                assert_eq!(prioritized_stream_id, 1);
                assert!(priority_field_value.is_empty());
            }
            other => panic!("expected Frame::PriorityUpdate, got {other:?}"),
        }
    }

    /// RFC 9218 §7.1: PRIORITY_UPDATE with a non-empty SF-Item priority
    /// field value. The parser preserves the raw bytes verbatim; the
    /// handler re-uses `parse_rfc9218_priority` to extract `(urgency, i)`.
    #[test]
    fn test_priority_update_with_priority_field_parses() {
        let value = b"u=0, i";
        let mut input = vec![
            0x00,
            0x00,
            0x04 + value.len() as u8, // payload_len = 4 + value
            0x10,                     // type = PRIORITY_UPDATE
            0x00,                     // flags = 0
            0x00,
            0x00,
            0x00,
            0x00, // connection stream
            0x00,
            0x00,
            0x00,
            0x05, // prioritized stream 5
        ];
        input.extend_from_slice(value);

        let (remaining, header) =
            frame_header(&input, DEFAULT_MAX_FRAME_SIZE).expect("header parses");
        assert!(matches!(header.frame_type, FrameType::PriorityUpdate));
        let (_, frame) = frame_body(remaining, &header).expect("body parses");
        match frame {
            Frame::PriorityUpdate(PriorityUpdate {
                prioritized_stream_id,
                ref priority_field_value,
            }) => {
                assert_eq!(prioritized_stream_id, 5);
                assert_eq!(priority_field_value.as_slice(), value);
            }
            other => panic!("expected Frame::PriorityUpdate, got {other:?}"),
        }
    }

    /// RFC 9218 §7.1: PRIORITY_UPDATE MUST carry at least the 4-byte
    /// prioritized stream ID. Shorter payloads are `FRAME_SIZE_ERROR`.
    #[test]
    fn test_priority_update_payload_below_min_is_frame_size_error() {
        let input = [
            0x00, 0x00, 0x03, // payload_len = 3 (one byte short)
            0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let (remaining, header) =
            frame_header(&input, DEFAULT_MAX_FRAME_SIZE).expect("header parses");
        let result = frame_body(remaining, &header);
        match result {
            Err(Err::Failure(e)) => {
                assert_eq!(e.kind, ParserErrorKind::H2(H2Error::FrameSizeError));
            }
            other => panic!("expected FRAME_SIZE_ERROR, got {other:?}"),
        }
    }

    /// RFC 9218 §7.1 + nghttp2/h2-Rust convention: cap the
    /// `priority_field_value` at PRIORITY_UPDATE_MAX_VALUE (1024) so a
    /// peer cannot drive per-frame `Vec<u8>` allocations toward
    /// SETTINGS_MAX_FRAME_SIZE (16 KiB) under cover of a structurally-
    /// legitimate frame type.
    #[test]
    fn test_priority_update_oversized_value_is_protocol_error() {
        // payload_len = 4 (sid) + 1025 (value) = 1029 → just over the cap.
        let payload_len = (PRIORITY_UPDATE_MIN_PAYLOAD as usize) + PRIORITY_UPDATE_MAX_VALUE + 1;
        let mut input = Vec::with_capacity(9 + payload_len);
        // 24-bit length, big-endian.
        input.push(((payload_len >> 16) & 0xff) as u8);
        input.push(((payload_len >> 8) & 0xff) as u8);
        input.push((payload_len & 0xff) as u8);
        input.push(0x10); // type = PRIORITY_UPDATE
        input.push(0x00); // flags
        input.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // stream_id = 0
        input.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]); // prioritized_stream_id = 1
        input.extend(std::iter::repeat_n(b'a', PRIORITY_UPDATE_MAX_VALUE + 1));
        // Use the larger frame size for the header parse so payload_len passes;
        // the value-length cap is enforced by `priority_update_frame` itself.
        let (remaining, header) =
            frame_header(&input, payload_len as u32 + 1).expect("header parses");
        match frame_body(remaining, &header) {
            Err(Err::Failure(e)) => {
                assert_eq!(e.kind, ParserErrorKind::H2(H2Error::ProtocolError));
            }
            other => {
                panic!("expected PROTOCOL_ERROR for oversized PRIORITY_UPDATE value, got {other:?}")
            }
        }
    }

    /// Boundary case: priority value at exactly PRIORITY_UPDATE_MAX_VALUE
    /// MUST be accepted (off-by-one regression guard).
    #[test]
    fn test_priority_update_at_max_value_accepted() {
        let payload_len = (PRIORITY_UPDATE_MIN_PAYLOAD as usize) + PRIORITY_UPDATE_MAX_VALUE;
        let mut input = Vec::with_capacity(9 + payload_len);
        input.push(((payload_len >> 16) & 0xff) as u8);
        input.push(((payload_len >> 8) & 0xff) as u8);
        input.push((payload_len & 0xff) as u8);
        input.push(0x10);
        input.push(0x00);
        input.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        input.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        input.extend(std::iter::repeat_n(b'a', PRIORITY_UPDATE_MAX_VALUE));
        let (remaining, header) =
            frame_header(&input, payload_len as u32 + 1).expect("header parses");
        match frame_body(remaining, &header) {
            Ok((_, Frame::PriorityUpdate(pu))) => {
                assert_eq!(pu.priority_field_value.len(), PRIORITY_UPDATE_MAX_VALUE);
            }
            other => panic!("expected Frame::PriorityUpdate at the cap boundary, got {other:?}"),
        }
    }

    /// RFC 9218 §7.1: PRIORITY_UPDATE MUST be sent on stream 0 (the
    /// connection control stream). Any other stream ID is a connection
    /// `PROTOCOL_ERROR`, rejected at `frame_header` time via the
    /// frame_type ↔ stream_id cross-check.
    #[test]
    fn test_priority_update_on_non_zero_stream_is_protocol_error() {
        let input = [
            0x00, 0x00, 0x04, // payload_len = 4
            0x10, // type = PRIORITY_UPDATE
            0x00, // flags
            0x00, 0x00, 0x00, 0x07, // stream_id = 7 (invalid for PRIORITY_UPDATE)
            0x00, 0x00, 0x00, 0x01, // prioritized_stream_id
        ];
        match frame_header(&input, DEFAULT_MAX_FRAME_SIZE) {
            Err(Err::Failure(e)) => {
                assert_eq!(e.kind, ParserErrorKind::H2(H2Error::ProtocolError));
            }
            other => panic!("expected PROTOCOL_ERROR, got {other:?}"),
        }
    }

    // ---- SETTINGS with odd payload size (not a multiple of 6) ----

    /// RFC 9113 §6.5: a SETTINGS frame payload that is not a multiple of 6
    /// octets MUST be treated as a FRAME_SIZE_ERROR.
    #[test]
    fn test_settings_payload_not_multiple_of_6_rejected() {
        let input = [
            0x00, 0x00, 0x05, // payload_len = 5 (not a multiple of 6)
            0x04, // type = SETTINGS
            0x00, // flags = 0 (not ACK)
            0x00, 0x00, 0x00, 0x00, // stream_id = 0
            0x00, 0x03, 0x00, 0x00, 0x00, // 5 bytes of incomplete setting
        ];

        let (remaining, header) =
            frame_header(&input, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        assert_eq!(header.frame_type, FrameType::Settings);

        let result = frame_body(remaining, &header);
        assert!(
            result.is_err(),
            "odd-size SETTINGS payload must be rejected"
        );
        match result {
            Err(nom::Err::Failure(e)) => {
                assert_eq!(e.kind, ParserErrorKind::H2(H2Error::FrameSizeError));
            }
            other => panic!("expected Failure(FrameSizeError), got {other:?}"),
        }
    }

    /// Audit Pass 1 Low #7: cap the number of SETTINGS entries at 64 to
    /// prevent a peer from forcing a multi-megabyte `Vec<Setting>` allocation
    /// per connection via a MAX_FRAME_SIZE SETTINGS payload full of
    /// identifier/value pairs. The first over-cap pair must be rejected.
    #[test]
    fn test_settings_frame_over_cap_rejected() {
        // 65 entries * 6 bytes = 390 bytes payload — one past MAX_SETTINGS_ENTRIES.
        let n_entries: u16 = (MAX_SETTINGS_ENTRIES as u16) + 1;
        let payload_len = u32::from(n_entries) * SETTINGS_ENTRY_SIZE;
        let mut input = Vec::with_capacity(9 + payload_len as usize);
        input.extend_from_slice(&payload_len.to_be_bytes()[1..]); // 24-bit length
        input.push(0x04); // SETTINGS
        input.push(0x00); // flags
        input.extend_from_slice(&[0, 0, 0, 0]); // stream_id = 0
        for i in 0..n_entries {
            input.extend_from_slice(&i.to_be_bytes()); // identifier
            input.extend_from_slice(&0u32.to_be_bytes()); // value
        }

        let (remaining, header) =
            frame_header(&input, 16_777_215).expect("frame header parses cleanly");
        assert_eq!(header.frame_type, FrameType::Settings);

        let result = frame_body(remaining, &header);
        assert!(
            result.is_err(),
            "SETTINGS frame over MAX_SETTINGS_ENTRIES must be rejected"
        );
        match result {
            Err(nom::Err::Failure(e)) => {
                assert_eq!(e.kind, ParserErrorKind::H2(H2Error::FrameSizeError));
            }
            other => panic!("expected Failure(FrameSizeError), got {other:?}"),
        }
    }

    /// Boundary: exactly MAX_SETTINGS_ENTRIES is still accepted.
    #[test]
    fn test_settings_frame_at_cap_accepted() {
        let n_entries: u16 = MAX_SETTINGS_ENTRIES as u16;
        let payload_len = u32::from(n_entries) * SETTINGS_ENTRY_SIZE;
        let mut input = Vec::with_capacity(9 + payload_len as usize);
        input.extend_from_slice(&payload_len.to_be_bytes()[1..]);
        input.push(0x04);
        input.push(0x00);
        input.extend_from_slice(&[0, 0, 0, 0]);
        for _ in 0..n_entries {
            input.extend_from_slice(&0u16.to_be_bytes());
            input.extend_from_slice(&0u32.to_be_bytes());
        }

        let (remaining, header) =
            frame_header(&input, 16_777_215).expect("frame header parses cleanly");
        let result = frame_body(remaining, &header);
        assert!(result.is_ok(), "exactly MAX_SETTINGS_ENTRIES must parse");
    }

    // ---- RST_STREAM with wrong payload size ----

    /// RFC 9113 §6.4: RST_STREAM payload MUST be exactly 4 octets.
    #[test]
    fn test_rst_stream_wrong_payload_size_rejected() {
        // 8 bytes instead of 4
        let input = [
            0x00, 0x00, 0x08, // payload_len = 8
            0x03, // type = RST_STREAM
            0x00, // flags = 0
            0x00, 0x00, 0x00, 0x01, // stream_id = 1
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 8 bytes
        ];

        let (remaining, header) =
            frame_header(&input, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        assert_eq!(header.frame_type, FrameType::RstStream);

        let result = frame_body(remaining, &header);
        assert!(
            result.is_err(),
            "wrong RST_STREAM payload size must be rejected"
        );
        match result {
            Err(nom::Err::Failure(e)) => {
                assert_eq!(e.kind, ParserErrorKind::H2(H2Error::FrameSizeError));
            }
            other => panic!("expected Failure(FrameSizeError), got {other:?}"),
        }
    }

    // ---- PING with wrong payload size ----

    /// RFC 9113 §6.7: PING payload MUST be exactly 8 octets.
    #[test]
    fn test_ping_wrong_payload_size_rejected() {
        let input = [
            0x00, 0x00, 0x04, // payload_len = 4 (should be 8)
            0x06, // type = PING
            0x00, // flags = 0
            0x00, 0x00, 0x00, 0x00, // stream_id = 0
            0x01, 0x02, 0x03, 0x04, // only 4 bytes
        ];

        let (remaining, header) =
            frame_header(&input, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        assert_eq!(header.frame_type, FrameType::Ping);

        let result = frame_body(remaining, &header);
        assert!(result.is_err(), "wrong PING payload size must be rejected");
        match result {
            Err(nom::Err::Failure(e)) => {
                assert_eq!(e.kind, ParserErrorKind::H2(H2Error::FrameSizeError));
            }
            other => panic!("expected Failure(FrameSizeError), got {other:?}"),
        }
    }

    // ---- Frame exceeding max_frame_size ----

    /// RFC 9113 §4.2: a frame with payload_len > max_frame_size is a
    /// FRAME_SIZE_ERROR at the frame header parsing stage.
    #[test]
    fn test_frame_exceeding_max_frame_size_rejected() {
        // payload_len = 16385 (0x004001), which exceeds the default 16384
        let input = [
            0x00, 0x40, 0x01, // payload_len = 16385
            0x00, // type = DATA
            0x00, // flags = 0
            0x00, 0x00, 0x00, 0x01, // stream_id = 1
        ];

        let result = frame_header(&input, DEFAULT_MAX_FRAME_SIZE);
        assert!(result.is_err(), "oversized frame must be rejected");
        match result {
            Err(nom::Err::Failure(e)) => {
                assert_eq!(e.kind, ParserErrorKind::H2(H2Error::FrameSizeError));
            }
            other => panic!("expected Failure(FrameSizeError), got {other:?}"),
        }
    }

    // ---- SETTINGS on non-zero stream (PROTOCOL_ERROR) ----

    /// RFC 9113 §6.5: SETTINGS frames MUST be associated with stream 0.
    #[test]
    fn test_settings_on_nonzero_stream_rejected() {
        let input = [
            0x00, 0x00, 0x00, // payload_len = 0
            0x04, // type = SETTINGS
            0x00, // flags = 0
            0x00, 0x00, 0x00, 0x01, // stream_id = 1 (invalid!)
        ];

        let result = frame_header(&input, DEFAULT_MAX_FRAME_SIZE);
        assert!(
            result.is_err(),
            "SETTINGS on non-zero stream must be rejected"
        );
        match result {
            Err(nom::Err::Failure(e)) => {
                assert_eq!(e.kind, ParserErrorKind::H2(H2Error::ProtocolError));
            }
            other => panic!("expected Failure(ProtocolError), got {other:?}"),
        }
    }

    // ---- DATA on stream 0 (PROTOCOL_ERROR) ----

    /// RFC 9113 §6.1: DATA frames MUST be associated with a stream.
    #[test]
    fn test_data_on_stream_zero_rejected() {
        let input = [
            0x00, 0x00, 0x02, // payload_len = 2
            0x00, // type = DATA
            0x00, // flags = 0
            0x00, 0x00, 0x00, 0x00, // stream_id = 0 (invalid!)
            0xCA, 0xFE,
        ];

        let result = frame_header(&input, DEFAULT_MAX_FRAME_SIZE);
        assert!(result.is_err(), "DATA on stream 0 must be rejected");
        match result {
            Err(nom::Err::Failure(e)) => {
                assert_eq!(e.kind, ParserErrorKind::H2(H2Error::ProtocolError));
            }
            other => panic!("expected Failure(ProtocolError), got {other:?}"),
        }
    }

    // ---- WINDOW_UPDATE with reserved bit set (must be masked) ----

    /// RFC 9113 §6.9: the reserved bit (MSB) of the window size increment
    /// MUST be ignored. An increment of 0x80000001 should be read as 1.
    #[test]
    fn test_window_update_reserved_bit_masked() {
        let input = [
            0x00, 0x00, 0x04, // payload_len = 4
            0x08, // type = WINDOW_UPDATE
            0x00, // flags = 0
            0x00, 0x00, 0x00, 0x01, // stream_id = 1
            0x80, 0x00, 0x00, 0x01, // increment with reserved bit set = 1
        ];

        let (remaining, header) =
            frame_header(&input, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        let (_, frame) = frame_body(remaining, &header).expect("frame body parses cleanly");
        match frame {
            Frame::WindowUpdate(wu) => {
                assert_eq!(
                    wu.increment, 1,
                    "reserved bit must be masked to yield increment=1"
                );
            }
            other => panic!("expected Frame::WindowUpdate, got {other:?}"),
        }
    }

    // ---- Helper: build a raw H2 frame (header + payload) ----

    /// Build a raw H2 frame header (9 bytes) from explicit fields.
    fn build_frame_header(payload_len: u32, frame_type: u8, flags: u8, stream_id: u32) -> Vec<u8> {
        vec![
            ((payload_len >> 16) & 0xFF) as u8,
            ((payload_len >> 8) & 0xFF) as u8,
            (payload_len & 0xFF) as u8,
            frame_type,
            flags,
            ((stream_id >> 24) & 0xFF) as u8,
            ((stream_id >> 16) & 0xFF) as u8,
            ((stream_id >> 8) & 0xFF) as u8,
            (stream_id & 0xFF) as u8,
        ]
    }

    /// Build a complete raw frame (header + payload).
    fn build_raw_frame(
        payload_len: u32,
        frame_type: u8,
        flags: u8,
        stream_id: u32,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut raw = build_frame_header(payload_len, frame_type, flags, stream_id);
        raw.extend_from_slice(payload);
        raw
    }

    // ---- Connection preface ----

    #[test]
    fn test_preface_valid() {
        let input = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
        let (remaining, matched) = preface(input).expect("should parse preface");
        assert!(remaining.is_empty());
        assert_eq!(matched, input.as_slice());
    }

    #[test]
    fn test_preface_invalid() {
        let input = b"GET / HTTP/1.1\r\n";
        let result = preface(input);
        assert!(result.is_err(), "invalid preface should fail");
    }

    #[test]
    fn test_preface_with_trailing_data() {
        let mut input = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec();
        input.extend_from_slice(b"extra stuff");
        let (remaining, _) = preface(&input).expect("should parse preface");
        assert_eq!(remaining, b"extra stuff");
    }

    // ---- Frame header basic parsing ----

    #[test]
    fn test_frame_header_settings_basic() {
        let raw = build_frame_header(0, 4, 0, 0);
        let (remaining, header) =
            frame_header(&raw, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        assert!(remaining.is_empty());
        assert_eq!(header.payload_len, 0);
        assert_eq!(header.frame_type, FrameType::Settings);
        assert_eq!(header.flags, 0);
        assert_eq!(header.stream_id, 0);
    }

    #[test]
    fn test_frame_header_data_basic() {
        let raw = build_frame_header(100, 0, 1, 1);
        let (_, header) =
            frame_header(&raw, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        assert_eq!(header.payload_len, 100);
        assert_eq!(header.frame_type, FrameType::Data);
        assert_eq!(header.flags, 1);
        assert_eq!(header.stream_id, 1);
    }

    #[test]
    fn test_frame_header_stream_id_reserved_bit_masked() {
        // stream_id with reserved bit set (0x80000001) should be masked to 1.
        // Use a DATA frame (stream-specific) to avoid the stream_id=0 rejection.
        let raw = build_frame_header(5, 0, 0, 0x80000001);
        let (_, header) =
            frame_header(&raw, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        assert_eq!(header.stream_id, 1, "reserved MSB must be masked off");
    }

    // ---- DATA frame parsing ----

    #[test]
    fn test_parse_data_frame_end_stream() {
        let payload = b"hello";
        let raw = build_raw_frame(payload.len() as u32, 0, 0x01, 1, payload);
        let (remaining, header) =
            frame_header(&raw, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        let (_, f) = frame_body(remaining, &header).expect("frame body parses cleanly");
        match f {
            Frame::Data(d) => {
                assert_eq!(d.stream_id, 1);
                assert!(d.end_stream);
            }
            other => panic!("expected Data, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_data_frame_no_end_stream() {
        let payload = b"data";
        let raw = build_raw_frame(payload.len() as u32, 0, 0x00, 3, payload);
        let (remaining, header) =
            frame_header(&raw, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        let (_, f) = frame_body(remaining, &header).expect("frame body parses cleanly");
        match f {
            Frame::Data(d) => {
                assert_eq!(d.stream_id, 3);
                assert!(!d.end_stream);
            }
            other => panic!("expected Data, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_data_frame_with_padding() {
        // PADDED flag (0x08), 2 bytes of padding
        let pad_length: u8 = 2;
        let actual_data = b"hello";
        let total_payload = 1 + actual_data.len() + pad_length as usize;
        let mut payload = Vec::new();
        payload.push(pad_length);
        payload.extend_from_slice(actual_data);
        payload.extend_from_slice(&[0x00; 2]);

        let raw = build_raw_frame(total_payload as u32, 0, 0x08, 1, &payload);
        let (remaining, header) =
            frame_header(&raw, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        let (_, f) = frame_body(remaining, &header).expect("frame body parses cleanly");
        match f {
            Frame::Data(d) => {
                assert_eq!(d.stream_id, 1);
                assert_eq!(d.payload.len, actual_data.len() as u32);
            }
            other => panic!("expected Data, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_data_frame_padding_exceeds_payload() {
        // pad_length claims more padding than remaining bytes
        let mut payload = Vec::new();
        payload.push(10); // pad_length = 10, but only 5 bytes remain
        payload.extend_from_slice(b"hello");

        let raw = build_raw_frame(payload.len() as u32, 0, 0x08, 1, &payload);
        let (remaining, header) =
            frame_header(&raw, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        let result = frame_body(remaining, &header);
        assert!(
            result.is_err(),
            "padding exceeding payload should be a protocol error"
        );
    }

    // ---- HEADERS frame parsing ----

    #[test]
    fn test_parse_headers_frame_basic() {
        let hblock = b"\x82\x86";
        let raw = build_raw_frame(hblock.len() as u32, 1, 0x04, 1, hblock);
        let (remaining, header) =
            frame_header(&raw, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        let (_, f) = frame_body(remaining, &header).expect("frame body parses cleanly");
        match f {
            Frame::Headers(h) => {
                assert_eq!(h.stream_id, 1);
                assert!(!h.end_stream);
                assert!(h.end_headers);
                assert!(h.priority.is_none());
            }
            other => panic!("expected Headers, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_headers_frame_end_stream_and_headers() {
        let hblock = b"\x82";
        let raw = build_raw_frame(hblock.len() as u32, 1, 0x05, 1, hblock);
        let (remaining, header) =
            frame_header(&raw, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        let (_, f) = frame_body(remaining, &header).expect("frame body parses cleanly");
        match f {
            Frame::Headers(h) => {
                assert!(h.end_stream);
                assert!(h.end_headers);
            }
            other => panic!("expected Headers, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_headers_frame_with_priority() {
        let hblock = b"\x82";
        let payload_len = 5 + hblock.len(); // 4 (stream dep) + 1 (weight) + hblock
        let mut payload = Vec::new();
        // Stream dependency: non-exclusive, stream_id=1
        payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        payload.push(15); // weight
        payload.extend_from_slice(hblock);

        let raw = build_raw_frame(payload_len as u32, 1, 0x24, 3, &payload);
        let (remaining, header) =
            frame_header(&raw, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        let (_, f) = frame_body(remaining, &header).expect("frame body parses cleanly");
        match f {
            Frame::Headers(h) => {
                assert_eq!(h.stream_id, 3);
                let priority = h.priority.expect("should have priority");
                match priority {
                    PriorityPart::Rfc7540 {
                        stream_dependency,
                        weight,
                    } => {
                        assert!(!stream_dependency.exclusive);
                        assert_eq!(stream_dependency.stream_id, 1);
                        assert_eq!(weight, 15);
                    }
                    other => panic!("expected Rfc7540, got {other:?}"),
                }
            }
            other => panic!("expected Headers, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_headers_frame_with_exclusive_priority() {
        let hblock = b"\x82\x86";
        let mut payload = Vec::new();
        // Stream dependency: exclusive (bit 31 set), stream_id=5
        let dep = 0x80000005u32;
        payload.extend_from_slice(&dep.to_be_bytes());
        payload.push(255); // weight
        payload.extend_from_slice(hblock);

        let payload_len = payload.len();
        let raw = build_raw_frame(payload_len as u32, 1, 0x24, 3, &payload);
        let (remaining, header) =
            frame_header(&raw, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        let (_, f) = frame_body(remaining, &header).expect("frame body parses cleanly");
        match f {
            Frame::Headers(h) => {
                let priority = h.priority.expect("should have priority");
                match priority {
                    PriorityPart::Rfc7540 {
                        stream_dependency,
                        weight,
                    } => {
                        assert!(stream_dependency.exclusive, "exclusive bit should be set");
                        assert_eq!(stream_dependency.stream_id, 5);
                        assert_eq!(weight, 255);
                    }
                    other => panic!("expected Rfc7540, got {other:?}"),
                }
            }
            other => panic!("expected Headers, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_headers_stream_id_zero_rejected() {
        let raw = build_raw_frame(2, 1, 0x04, 0, b"\x82\x86");
        let result = frame_header(&raw, DEFAULT_MAX_FRAME_SIZE);
        assert!(
            result.is_err(),
            "HEADERS with stream_id=0 should be rejected"
        );
    }

    // ---- RST_STREAM frame parsing ----

    #[test]
    fn test_parse_rst_stream() {
        let error_code = 0x00000008u32; // CANCEL
        let raw = build_raw_frame(4, 3, 0, 1, &error_code.to_be_bytes());
        let (remaining, header) =
            frame_header(&raw, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        let (_, f) = frame_body(remaining, &header).expect("frame body parses cleanly");
        match f {
            Frame::RstStream(rst) => {
                assert_eq!(rst.stream_id, 1);
                assert_eq!(rst.error_code, 0x08);
            }
            other => panic!("expected RstStream, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_rst_stream_stream_id_zero_rejected() {
        let raw = build_raw_frame(4, 3, 0, 0, &[0u8; 4]);
        let result = frame_header(&raw, DEFAULT_MAX_FRAME_SIZE);
        assert!(
            result.is_err(),
            "RST_STREAM with stream_id=0 should be rejected"
        );
    }

    // ---- SETTINGS frame parsing ----

    #[test]
    fn test_parse_settings_frame_with_values() {
        let mut payload = Vec::new();
        // SETTINGS_HEADER_TABLE_SIZE (0x1) = 4096
        payload.extend_from_slice(&0x0001u16.to_be_bytes());
        payload.extend_from_slice(&4096u32.to_be_bytes());
        // SETTINGS_MAX_CONCURRENT_STREAMS (0x3) = 100
        payload.extend_from_slice(&0x0003u16.to_be_bytes());
        payload.extend_from_slice(&100u32.to_be_bytes());
        // SETTINGS_INITIAL_WINDOW_SIZE (0x4) = 65535
        payload.extend_from_slice(&0x0004u16.to_be_bytes());
        payload.extend_from_slice(&65535u32.to_be_bytes());

        let raw = build_raw_frame(payload.len() as u32, 4, 0x0, 0, &payload);
        let (remaining, header) =
            frame_header(&raw, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        let (_, f) = frame_body(remaining, &header).expect("frame body parses cleanly");
        match f {
            Frame::Settings(s) => {
                assert!(!s.ack);
                assert_eq!(s.settings.len(), 3);
                assert_eq!(s.settings[0].identifier, 0x0001);
                assert_eq!(s.settings[0].value, 4096);
                assert_eq!(s.settings[1].identifier, 0x0003);
                assert_eq!(s.settings[1].value, 100);
                assert_eq!(s.settings[2].identifier, 0x0004);
                assert_eq!(s.settings[2].value, 65535);
            }
            other => panic!("expected Settings, got {other:?}"),
        }
    }

    // ---- PING frame parsing ----

    #[test]
    fn test_parse_ping_frame() {
        let ping_payload = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let raw = build_raw_frame(8, 6, 0, 0, &ping_payload);
        let (remaining, header) =
            frame_header(&raw, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        let (_, f) = frame_body(remaining, &header).expect("frame body parses cleanly");
        match f {
            Frame::Ping(p) => {
                assert_eq!(p.payload, ping_payload);
                assert!(!p.ack);
            }
            other => panic!("expected Ping, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_ping_ack_preserves_payload() {
        let ping_payload = [0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE];
        let raw = build_raw_frame(8, 6, 0x01, 0, &ping_payload);
        let (remaining, header) =
            frame_header(&raw, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        let (_, f) = frame_body(remaining, &header).expect("frame body parses cleanly");
        match f {
            Frame::Ping(p) => {
                assert_eq!(
                    p.payload, ping_payload,
                    "PING ACK must echo the exact payload"
                );
                assert!(p.ack);
            }
            other => panic!("expected Ping, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_ping_stream_id_nonzero_rejected() {
        let raw = build_raw_frame(8, 6, 0, 1, &[0u8; 8]);
        let result = frame_header(&raw, DEFAULT_MAX_FRAME_SIZE);
        assert!(result.is_err(), "PING with stream_id!=0 should be rejected");
    }

    // ---- WINDOW_UPDATE frame parsing ----

    #[test]
    fn test_parse_window_update_connection_level() {
        let increment = 1000u32;
        let raw = build_raw_frame(4, 8, 0, 0, &increment.to_be_bytes());
        let (remaining, header) =
            frame_header(&raw, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        let (_, f) = frame_body(remaining, &header).expect("frame body parses cleanly");
        match f {
            Frame::WindowUpdate(w) => {
                assert_eq!(w.stream_id, 0);
                assert_eq!(w.increment, 1000);
            }
            other => panic!("expected WindowUpdate, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_window_update_stream_level() {
        let increment = 65535u32;
        let raw = build_raw_frame(4, 8, 0, 5, &increment.to_be_bytes());
        let (remaining, header) =
            frame_header(&raw, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        let (_, f) = frame_body(remaining, &header).expect("frame body parses cleanly");
        match f {
            Frame::WindowUpdate(w) => {
                assert_eq!(w.stream_id, 5);
                // mux parser uses STREAM_ID_MASK (0x7FFFFFFF), so 65535 & 0x7FFFFFFF = 65535
                assert_eq!(w.increment, 65535);
            }
            other => panic!("expected WindowUpdate, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_window_update_wrong_size() {
        let raw = build_raw_frame(3, 8, 0, 0, &[0u8; 3]);
        let (remaining, header) =
            frame_header(&raw, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        let result = frame_body(remaining, &header);
        assert!(
            result.is_err(),
            "WINDOW_UPDATE with payload != 4 should fail"
        );
        match result {
            Err(Err::Failure(e)) => {
                assert_eq!(e.kind, ParserErrorKind::H2(H2Error::FrameSizeError));
            }
            other => panic!("expected FrameSizeError, got {other:?}"),
        }
    }

    // ---- Frame at max_frame_size boundary ----

    #[test]
    fn test_parse_frame_at_max_frame_size() {
        let payload = vec![0u8; DEFAULT_MAX_FRAME_SIZE as usize];
        let raw = build_raw_frame(DEFAULT_MAX_FRAME_SIZE, 0, 0x0, 1, &payload);
        let (remaining, header) =
            frame_header(&raw, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        let (_, f) = frame_body(remaining, &header).expect("frame body parses cleanly");
        match f {
            Frame::Data(d) => {
                assert_eq!(d.payload.len, DEFAULT_MAX_FRAME_SIZE);
            }
            other => panic!("expected Data, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_frame_with_custom_max_frame_size() {
        let custom_max = 32768u32;
        let payload = vec![0u8; 20000];
        let raw = build_raw_frame(20000, 0, 0x0, 1, &payload);
        let (remaining, header) =
            frame_header(&raw, custom_max).expect("frame header parses with custom max");
        let (_, f) = frame_body(remaining, &header).expect("frame body parses cleanly");
        match f {
            Frame::Data(d) => {
                assert_eq!(d.payload.len, 20000);
            }
            other => panic!("expected Data, got {other:?}"),
        }
    }

    // ---- H2Error conversions ----

    #[test]
    fn test_h2_error_try_from_valid() {
        assert_eq!(H2Error::try_from(0x0), Ok(H2Error::NoError));
        assert_eq!(H2Error::try_from(0x1), Ok(H2Error::ProtocolError));
        assert_eq!(H2Error::try_from(0x6), Ok(H2Error::FrameSizeError));
        assert_eq!(H2Error::try_from(0xd), Ok(H2Error::HTTP11Required));
    }

    #[test]
    fn test_h2_error_try_from_invalid() {
        assert_eq!(H2Error::try_from(0x0e), Err(0x0e));
        assert_eq!(H2Error::try_from(0xFF), Err(0xFF));
    }

    #[test]
    fn test_h2_error_from_str() {
        assert_eq!("NO_ERROR".parse::<H2Error>(), Ok(H2Error::NoError));
        assert_eq!(
            "PROTOCOL_ERROR".parse::<H2Error>(),
            Ok(H2Error::ProtocolError)
        );
        assert_eq!(
            "ENHANCE_YOUR_CALM".parse::<H2Error>(),
            Ok(H2Error::EnhanceYourCalm)
        );
        assert!("INVALID_ERROR".parse::<H2Error>().is_err());
    }

    #[test]
    fn test_h2_error_as_str_roundtrip() {
        let errors = [
            H2Error::NoError,
            H2Error::ProtocolError,
            H2Error::InternalError,
            H2Error::FlowControlError,
            H2Error::SettingsTimeout,
            H2Error::StreamClosed,
            H2Error::FrameSizeError,
            H2Error::RefusedStream,
            H2Error::Cancel,
            H2Error::CompressionError,
            H2Error::ConnectError,
            H2Error::EnhanceYourCalm,
            H2Error::InadequateSecurity,
            H2Error::HTTP11Required,
        ];

        for error in &errors {
            let s = error.as_str();
            let parsed: H2Error = s.parse().unwrap_or_else(|_| panic!("failed to parse {s}"));
            assert_eq!(*error, parsed, "roundtrip failed for {s}");
        }
    }

    // ---- PRIORITY frame parsing ----

    #[test]
    fn test_parse_priority_frame() {
        let mut payload = Vec::new();
        // Stream dependency: non-exclusive, stream_id=1
        payload.extend_from_slice(&0x00000001u32.to_be_bytes());
        payload.push(15); // weight
        assert_eq!(payload.len(), 5);

        let raw = build_raw_frame(5, 2, 0, 3, &payload);
        let (remaining, header) =
            frame_header(&raw, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        let (_, f) = frame_body(remaining, &header).expect("frame body parses cleanly");
        match f {
            Frame::Priority(p) => {
                assert_eq!(p.stream_id, 3);
                match p.inner {
                    PriorityPart::Rfc7540 {
                        stream_dependency,
                        weight,
                    } => {
                        assert!(!stream_dependency.exclusive);
                        assert_eq!(stream_dependency.stream_id, 1);
                        assert_eq!(weight, 15);
                    }
                    other => panic!("expected Rfc7540, got {other:?}"),
                }
            }
            other => panic!("expected Priority, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_priority_wrong_size_rejected() {
        let raw = build_raw_frame(4, 2, 0, 1, &[0u8; 4]);
        let (remaining, header) =
            frame_header(&raw, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        let result = frame_body(remaining, &header);
        assert!(
            result.is_err(),
            "PRIORITY with wrong payload size should fail"
        );
    }

    // A HEADERS frame with PADDED+PRIORITY where the declared pad_length
    // exceeds the bytes left after consuming the 5-byte priority payload
    // must be rejected as PROTOCOL_ERROR instead of panicking on underflow.
    // Regression for fuzz crash d7a34a0d: payload_len=6, pad_length=3 →
    // 6 − 1 (pad_length byte) − 5 (priority) = 0 content bytes, yet the
    // parser tried to strip 3 padding bytes from 0.
    #[test]
    fn test_headers_padded_priority_underflow_rejected() {
        let raw: [u8; 16] = [
            0x00, 0x00, 0x06, 0x01, 0xff, 0xff, 0xff, 0x00, 0x00, 0x03, 0x00, 0x64, 0x6d, 0x6d,
            0x6d, 0x6d,
        ];
        let (remaining, header) =
            frame_header(&raw, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        let result = frame_body(remaining, &header);
        assert!(
            result.is_err(),
            "PADDED+PRIORITY HEADERS with oversized pad_length must error, not panic"
        );
    }

    // ---- strip_padding boundary: pad_length == payload_len - 1 (all padding) ----

    /// RFC 9113 §6.1: "The Pad Length field MUST be less than the length of
    /// the frame payload". With payload_len = N, pad_length up to N-1 is
    /// valid (content may be empty, the final byte is padding). The previous
    /// `i.len() <= pad_length` check rejected the N-1 case; the fix changes
    /// the comparison to `(pad_length as usize) > i.len()`.
    #[test]
    fn test_strip_padding_all_padding_accepted() {
        // DATA frame: payload_len = 2, FLAG_PADDED, pad_length = 1.
        // After consuming the pad-length byte, `i.len() = 1` and
        // `pad_length = 1`, which is exactly the maximum valid value.
        let input = [
            0x00, 0x00, 0x02, // payload_len = 2
            0x00, // type = DATA
            0x08, // flags = PADDED
            0x00, 0x00, 0x00, 0x01, // stream_id = 1
            0x01, // pad_length
            0x00, // padding byte
        ];

        let (remaining, header) =
            frame_header(&input, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        let (_, frame) =
            frame_body(remaining, &header).expect("DATA with all-padding body must parse");
        match frame {
            Frame::Data(data) => {
                assert_eq!(data.stream_id, 1);
                // Content length is 0 (all of the post-pad-length-byte payload is padding).
                assert_eq!(data.payload.len, 0);
            }
            other => panic!("expected Frame::Data, got {other:?}"),
        }
    }

    #[test]
    fn test_strip_padding_pad_length_equals_payload_len_rejected() {
        // payload_len = 1, FLAG_PADDED, pad_length = 1 → invalid
        // (the pad-length byte consumes one byte, leaving i.len()=0 but
        // pad_length wants to strip 1 extra byte of trailing padding).
        let input = [
            0x00, 0x00, 0x01, // payload_len = 1
            0x00, // type = DATA
            0x08, // flags = PADDED
            0x00, 0x00, 0x00, 0x01, // stream_id = 1
            0x01, // pad_length (invalid: exceeds remaining body)
        ];

        let (remaining, header) =
            frame_header(&input, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        let result = frame_body(remaining, &header);
        assert!(
            matches!(
                result,
                Err(nom::Err::Failure(ParserError {
                    kind: ParserErrorKind::H2(H2Error::ProtocolError),
                    ..
                }))
            ),
            "pad_length > remaining body must yield PROTOCOL_ERROR, got {result:?}"
        );
    }

    // ---- GOAWAY payload-length validation ----

    /// RFC 9113 §6.8: a GOAWAY frame payload is at least 8 bytes (4-byte
    /// last-stream-id + 4-byte error-code). Shorter payloads MUST be treated
    /// as FRAME_SIZE_ERROR.
    #[test]
    fn test_goaway_short_payload_rejected() {
        let input = [
            0x00, 0x00, 0x04, // payload_len = 4 (too short)
            0x07, // type = GOAWAY
            0x00, // flags = 0
            0x00, 0x00, 0x00, 0x00, // stream_id = 0
            0x00, 0x00, 0x00, 0x00, // partial payload (only last_stream_id)
        ];

        let (remaining, header) =
            frame_header(&input, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        assert_eq!(header.frame_type, FrameType::GoAway);
        let result = frame_body(remaining, &header);
        assert!(
            matches!(
                result,
                Err(nom::Err::Failure(ParserError {
                    kind: ParserErrorKind::H2(H2Error::FrameSizeError),
                    ..
                }))
            ),
            "GOAWAY with payload_len < 8 must yield FRAME_SIZE_ERROR, got {result:?}"
        );
    }

    #[test]
    fn test_goaway_minimum_payload_accepted() {
        let input = [
            0x00, 0x00, 0x08, // payload_len = 8
            0x07, // type = GOAWAY
            0x00, // flags = 0
            0x00, 0x00, 0x00, 0x00, // stream_id = 0
            0x00, 0x00, 0x00, 0x0a, // last_stream_id = 10
            0x00, 0x00, 0x00, 0x00, // error_code = NO_ERROR
        ];

        let (remaining, header) =
            frame_header(&input, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        let (_, frame) = frame_body(remaining, &header).expect("minimal GOAWAY must parse cleanly");
        match frame {
            Frame::GoAway(goaway) => {
                assert_eq!(goaway.last_stream_id, 10);
                assert_eq!(goaway.error_code, 0);
            }
            other => panic!("expected Frame::GoAway, got {other:?}"),
        }
    }

    // ---- PUSH_PROMISE wire-level rejection ----

    /// RFC 9113 §8.4: a peer that has not enabled server push MUST treat a
    /// received PUSH_PROMISE as a connection error of type PROTOCOL_ERROR.
    /// Sozu never advertises `SETTINGS_ENABLE_PUSH=1`, so we reject
    /// PUSH_PROMISE at the wire layer for defence-in-depth — even if a
    /// future refactor of `mux/h2.rs` forgets the explicit check.
    #[test]
    fn test_push_promise_rejected_at_wire_layer() {
        let input = [
            0x00, 0x00, 0x08, // payload_len = 8
            0x05, // type = PUSH_PROMISE
            0x04, // flags = END_HEADERS
            0x00, 0x00, 0x00, 0x01, // stream_id = 1 (associated stream)
            0x00, 0x00, 0x00, 0x02, // promised stream_id = 2
            0x00, 0x00, 0x00, 0x00, // arbitrary header-block placeholder
        ];

        let (remaining, header) =
            frame_header(&input, DEFAULT_MAX_FRAME_SIZE).expect("frame header parses cleanly");
        assert_eq!(header.frame_type, FrameType::PushPromise);
        let result = frame_body(remaining, &header);
        assert!(
            matches!(
                result,
                Err(nom::Err::Failure(ParserError {
                    kind: ParserErrorKind::H2(H2Error::ProtocolError),
                    ..
                }))
            ),
            "PUSH_PROMISE must be rejected at wire layer with PROTOCOL_ERROR, got {result:?}"
        );
    }
}
