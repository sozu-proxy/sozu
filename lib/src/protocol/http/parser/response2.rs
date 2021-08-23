use crate::buffer_queue::BufferQueue;
use crate::protocol::http::StickySession;

use nom::{Err, HexDisplay, IResult, Offset};

use std::convert::From;
use std::str;

use crate::protocol::http::AddedHeader;

use super::{crlf, message_header, BufferMove, Chunk, Connection, LengthInformation, RStatusLine};

use super::super::buffer::HttpBuffer;
use super::header::{self, CopyingSlice, Header, HeaderName, Meta, Slice, StatusLine, Version};

pub type UpgradeProtocol = String;

#[derive(Debug, Clone, PartialEq)]
pub enum ResponseState {
    Initial,
    Error {
        status: Option<StatusLine>,
        headers: Vec<Header>,
        data: Option<Slice>,
        index: usize,
        upgrade: Option<UpgradeProtocol>,
        connection: Option<Connection>,
        length: Option<LengthInformation>,
        chunk: Option<Chunk>,
    },
    // index is how far we have parsed in the buffer
    Parsing {
        status: StatusLine,
        headers: Vec<Header>,
        index: usize,
    },
    ParsingDone {
        status: StatusLine,
        headers: Vec<Header>,
        /// start of body
        data: Slice,
        /// position of end of headers
        index: usize,
    },
    CopyingHeaders {
        status: Option<RStatusLine>,
        headers: Vec<Header>,
        data: Slice,
        index: usize,
        connection: Connection,
        upgrade: Option<UpgradeProtocol>,
        length: Option<LengthInformation>,
        header_slices: Vec<CopyingSlice>,
        is_head: bool,
    },
    Response {
        status: RStatusLine,
        connection: Connection,
    },
    ResponseUpgrade {
        status: RStatusLine,
        connection: Connection,
        upgrade: UpgradeProtocol,
    },
    ResponseWithBody {
        status: RStatusLine,
        connection: Connection,
        length: usize,
    },
    ResponseWithBodyChunks {
        status: RStatusLine,
        connection: Connection,
        chunk: Chunk,
    },
    // the boolean indicates if the backend connection is closed
    ResponseWithBodyCloseDelimited {
        status: RStatusLine,
        connection: Connection,
        back_closed: bool,
    },
}

impl ResponseState {
    pub fn into_error(self) -> ResponseState {
        match self {
            ResponseState::Initial => ResponseState::Error {
                status: None,
                headers: Vec::new(),
                data: None,
                index: 0,
                upgrade: None,
                connection: None,
                length: None,
                chunk: None,
            },
            ResponseState::Parsing {
                status,
                headers,
                index,
            } => ResponseState::Error {
                status: Some(status),
                headers,
                data: None,
                index,
                upgrade: None,
                connection: None,
                length: None,
                chunk: None,
            },
            ResponseState::ParsingDone {
                status,
                headers,
                data,
                index,
            } => ResponseState::Error {
                status: Some(status),
                headers,
                data: Some(data),
                index,
                upgrade: None,
                connection: None,
                length: None,
                chunk: None,
            },
            ResponseState::Response {
                status: _,
                connection,
            } => ResponseState::Error {
                status: None,
                headers: Vec::new(),
                data: None,
                index: 0,
                connection: Some(connection),
                upgrade: None,
                length: None,
                chunk: None,
            },
            ResponseState::ResponseUpgrade {
                status: _,
                connection,
                upgrade,
            } => ResponseState::Error {
                status: None,
                headers: Vec::new(),
                data: None,
                index: 0,
                connection: Some(connection),
                upgrade: Some(upgrade),
                length: None,
                chunk: None,
            },
            ResponseState::ResponseWithBody {
                status: _,
                connection,
                length,
            } => ResponseState::Error {
                status: None,
                headers: Vec::new(),
                data: None,
                index: 0,
                connection: Some(connection),
                upgrade: None,
                length: Some(LengthInformation::Length(length)),
                chunk: None,
            },
            ResponseState::ResponseWithBodyChunks {
                status: _,
                connection,
                chunk,
            } => ResponseState::Error {
                status: None,
                headers: Vec::new(),
                data: None,
                index: 0,
                connection: Some(connection),
                upgrade: None,
                length: None,
                chunk: Some(chunk),
            },
            ResponseState::ResponseWithBodyCloseDelimited {
                status: _,
                connection,
                ..
            } => ResponseState::Error {
                status: None,
                headers: Vec::new(),
                data: None,
                index: 0,
                connection: Some(connection),
                upgrade: None,
                length: None,
                chunk: None,
            },
            err => err,
        }
    }

    pub fn is_proxying(&self) -> bool {
        match *self {
            ResponseState::Response { .. }
            | ResponseState::ResponseWithBody { .. }
            | ResponseState::ResponseWithBodyChunks { .. }
            | ResponseState::ResponseWithBodyCloseDelimited { .. } => true,
            _ => false,
        }
    }

    pub fn is_back_error(&self) -> bool {
        if let ResponseState::Error { .. } = self {
            true
        } else {
            false
        }
    }

    pub fn get_status_line(&self) -> Option<&RStatusLine> {
        match self {
            ResponseState::Response { status, .. }
            | ResponseState::ResponseUpgrade { status, .. }
            | ResponseState::ResponseWithBody { status, .. }
            | ResponseState::ResponseWithBodyCloseDelimited { status, .. }
            | ResponseState::ResponseWithBodyChunks { status, .. } => Some(status),
            ResponseState::Error { status: _, .. } => None,
            _ => None,
        }
    }

    pub fn get_keep_alive(&self) -> Option<Connection> {
        match self {
            ResponseState::Response { connection, .. }
            | ResponseState::ResponseUpgrade { connection, .. }
            | ResponseState::ResponseWithBody { connection, .. }
            | ResponseState::ResponseWithBodyCloseDelimited { connection, .. }
            | ResponseState::ResponseWithBodyChunks { connection, .. } => Some(connection.clone()),
            ResponseState::Error { connection, .. } => connection.clone(),
            _ => None,
        }
    }

    pub fn get_mut_connection(&mut self) -> Option<&mut Connection> {
        match self {
            ResponseState::Response { connection, .. }
            | ResponseState::ResponseUpgrade { connection, .. }
            | ResponseState::ResponseWithBody { connection, .. }
            | ResponseState::ResponseWithBodyCloseDelimited { connection, .. }
            | ResponseState::ResponseWithBodyChunks { connection, .. } => Some(connection),
            ResponseState::Error { connection, .. } => connection.as_mut(),
            _ => None,
        }
    }

    pub fn should_copy(&self, position: usize) -> Option<usize> {
        match *self {
            ResponseState::ResponseWithBody { length, .. } => Some(position + length),
            ResponseState::Response { .. } => Some(position),
            _ => None,
        }
    }

    pub fn should_keep_alive(&self) -> bool {
        //FIXME: should not clone here
        let sl = self.get_status_line();
        let version = sl.as_ref().map(|sl| sl.version);
        let conn = self.get_keep_alive();
        match (version, conn.map(|c| c.keep_alive)) {
            (_, Some(Some(true))) => true,
            (_, Some(Some(false))) => false,
            (Some(super::Version::V10), _) => false,
            (Some(super::Version::V11), _) => true,
            (_, _) => false,
        }
    }

    pub fn should_chunk(&self) -> bool {
        if let ResponseState::ResponseWithBodyChunks { .. } = *self {
            true
        } else {
            false
        }
    }

    pub fn as_ioslice<'a>(&'a self, buffer: &'a [u8]) -> Vec<std::io::IoSlice<'a>> {
        let mut v = Vec::new();

        match *self {
            ResponseState::CopyingHeaders {
                ref header_slices, ..
            } => {
                for h in header_slices.iter() {
                    match h {
                        CopyingSlice::Static(s) => v.push(std::io::IoSlice::new(*s)),
                        CopyingSlice::Slice(s) => match s.data(buffer) {
                            Some(data) => v.push(std::io::IoSlice::new(data)),
                            None => break,
                        },
                        CopyingSlice::Vec(data, index) => {
                            v.push(std::io::IoSlice::new(&data[*index..]))
                        }
                    }
                }
            }
            ResponseState::ResponseWithBody { length, .. } => {
                let sz = std::cmp::min(length, buffer.len());
                v.push(std::io::IoSlice::new(&buffer[..sz]));
            }
            ResponseState::ResponseWithBodyChunks { chunk, .. } => match chunk {
                Chunk::Initial => {}
                Chunk::Copying(length) => {
                    let sz = std::cmp::min(length, buffer.len());
                    v.push(std::io::IoSlice::new(&buffer[..sz]));
                }
                Chunk::CopyingLastHeader(length, _) => {
                    let sz = std::cmp::min(length, buffer.len());
                    v.push(std::io::IoSlice::new(&buffer[..sz]));
                }
                Chunk::Ended => {}
                Chunk::Error => {}
            },
            ResponseState::ResponseWithBodyCloseDelimited { .. } => {
                v.push(std::io::IoSlice::new(buffer));
            }
            ResponseState::Response { .. } | ResponseState::ResponseUpgrade { .. } => {}
            ResponseState::Initial
            | ResponseState::Error { .. }
            | ResponseState::Parsing { .. }
            | ResponseState::ParsingDone { .. } => {}
        }
        v
    }

    pub fn next_slice<'a>(&'a self, buffer: &'a [u8]) -> &'a [u8] {
        match *self {
            ResponseState::CopyingHeaders {
                ref header_slices, ..
            } => header_slices
                .get(0)
                .and_then(|h| match h {
                    CopyingSlice::Static(s) => Some(*s),
                    CopyingSlice::Slice(s) => s.data(buffer),
                    CopyingSlice::Vec(v, index) => Some(&v[*index..]),
                })
                .unwrap_or(&b""[..]),
            ResponseState::Response { .. } | ResponseState::ResponseUpgrade { .. } => &b""[..],
            ResponseState::ResponseWithBody { length, .. } => {
                let sz = std::cmp::min(length, buffer.len());

                &buffer[..sz]
            }
            ResponseState::ResponseWithBodyChunks { chunk, .. } => match chunk {
                Chunk::Initial => &buffer[..0],
                Chunk::Copying(length) => {
                    let sz = std::cmp::min(length, buffer.len());
                    &buffer[..sz]
                }
                Chunk::CopyingLastHeader(length, _) => {
                    let sz = std::cmp::min(length, buffer.len());
                    &buffer[..sz]
                }
                Chunk::Ended => &buffer[..0],
                Chunk::Error => &buffer[..0],
            },
            ResponseState::ResponseWithBodyCloseDelimited { .. } => buffer,
            ResponseState::Initial
            | ResponseState::Error { .. }
            | ResponseState::Parsing { .. }
            | ResponseState::ParsingDone { .. } => &buffer[..0],
        }
    }

    // argument: how much was written
    // return: how much the buffer should be advanced
    //
    // if we're sending the headers, we do not want to advance
    // the buffer until all have been sent
    // also, if we are deleting a chunk of data, we might return a higher value
    pub fn consume(self, mut consumed: usize, buffer: &mut HttpBuffer) -> Self {
        let c = consumed;
        match self {
            ResponseState::CopyingHeaders {
                status,
                data,
                index,
                connection,
                upgrade,
                length,
                headers,
                mut header_slices,
                is_head,
            } => {
                let mut v = Vec::new();

                let mut it = header_slices.drain(..);
                loop {
                    if let Some(h) = it.next() {
                        match h.consume(consumed) {
                            (remaining, None) => consumed = remaining,
                            (r, Some(slice)) => {
                                consumed = r;
                                v.push(slice);
                                break;
                            }
                        }
                    } else {
                        break;
                    }
                }

                v.extend(it);
                header_slices = v;

                // we should not try to consume more than we wrote
                assert_eq!(consumed, 0);
                println!(
                    "response state consumed {} bytes, remaining slices: {:?}",
                    c, header_slices
                );

                if !header_slices.is_empty() {
                    return ResponseState::CopyingHeaders {
                        status,
                        data,
                        index,
                        connection,
                        upgrade,
                        length,
                        headers,
                        header_slices,
                        is_head,
                    };
                }

                buffer.consume_parsed_data(index);
                let status_line = status.unwrap();

                let state = if is_head ||
                    // all 1xx responses
                    status_line.status / 100  == 1 || status_line.status == 204 || status_line.status == 304
                {
                    ResponseState::Response {
                        status: status_line,
                        connection,
                    }
                } else {
                    match upgrade {
                        Some(upgrade) => ResponseState::ResponseUpgrade {
                            status: status_line,
                            connection,
                            upgrade,
                        },
                        None => match length {
                            // no length info, assuming the response ends at connection close
                            None => ResponseState::ResponseWithBodyCloseDelimited {
                                status: status_line,
                                connection,
                                back_closed: false,
                            },
                            Some(LengthInformation::Length(sz)) => {
                                ResponseState::ResponseWithBody {
                                    status: status_line,
                                    connection,
                                    length: sz,
                                }
                            }
                            Some(LengthInformation::Chunked) => {
                                ResponseState::ResponseWithBodyChunks {
                                    status: status_line,
                                    connection,
                                    chunk: Chunk::Initial,
                                }
                            } //FIXME: missing body close delimited and response upgrade
                        },
                    }
                };

                println!("state is now {:?}", state);
                state
            }
            ResponseState::ResponseWithBody {
                status,
                connection,
                length,
                ..
            } => {
                buffer.consume_parsed_data(consumed);
                println!(
                    "RESPONSE_STATE::ResponseBody::CONSUME({}): new length = {}",
                    consumed,
                    length - consumed
                );
                ResponseState::ResponseWithBody {
                    status,
                    connection,
                    length: length - consumed,
                }
            }
            ResponseState::ResponseWithBodyChunks {
                status,
                connection,
                chunk,
            } => {
                buffer.consume_parsed_data(consumed);

                let c = chunk.clone();
                let chunk = match chunk {
                    Chunk::Copying(sz) => {
                        if sz >= consumed {
                            Chunk::Copying(sz - consumed)
                        } else {
                            Chunk::Error
                        }
                    }
                    Chunk::CopyingLastHeader(sz, found_end) => {
                        if sz == consumed && found_end {
                            Chunk::Ended
                        } else if sz >= consumed {
                            Chunk::CopyingLastHeader(sz - consumed, found_end)
                        } else {
                            Chunk::Error
                        }
                    }
                    _ => Chunk::Error,
                };
                println!(
                    "ResponseWithBodyChunks::consume({}): {:?} => {:?}",
                    consumed, c, chunk
                );
                ResponseState::ResponseWithBodyChunks {
                    status,
                    connection,
                    chunk,
                }
            }
            ResponseState::ResponseWithBodyCloseDelimited { .. } => {
                buffer.consume_parsed_data(consumed);
                self
            }
            ResponseState::Initial
            | ResponseState::Error { .. }
            | ResponseState::Parsing { .. }
            | ResponseState::ParsingDone { .. }
            | ResponseState::Response { .. }
            | ResponseState::ResponseUpgrade { .. } => self,
            //_ => self,
        }
    }

    pub fn can_restart_parsing(&self, _available_data: usize) -> bool {
        let res = match self {
            ResponseState::Response { .. } => true,
            ResponseState::ResponseWithBody { length: 0, .. } => true,
            ResponseState::ResponseWithBodyChunks {
                chunk: Chunk::Ended,
                ..
            } => true,
            _s => false,
        };
        res
    }
}

pub fn default_response_result<O>(state: ResponseState, res: IResult<&[u8], O>) -> ResponseState {
    match res {
        Err(Err::Error(_)) | Err(Err::Failure(_)) => state.into_error(),
        Err(Err::Incomplete(_)) => state,
        _ => unreachable!(),
    }
}

pub fn parse_response_until_stop(
    mut state: ResponseState,
    _header_end: Option<usize>,
    buffer: &mut HttpBuffer,
    is_head: bool,
    added_res_header: Option<&AddedHeader>,
    sticky_name: &str,
    _sticky_session: Option<&StickySession>,
    _app_id: Option<&str>,
) -> (ResponseState, Option<usize>) {
    let buf = buffer.unparsed_data();
    println!("will parse:\n{}", buf.to_hex(16));

    loop {
        println!("state: {:?}", state);
        match state {
            ResponseState::Initial => match header::status_line(buf) {
                Ok((i, (version, status, reason))) => {
                    let sline = StatusLine::new(buf, version, status, reason);
                    println!("sline: {:?}", sline);
                    state = ResponseState::Parsing {
                        status: sline,
                        headers: Vec::new(),
                        index: buf.offset(i),
                    };
                }
                Err(Err::Incomplete(_)) => break,
                res => {
                    println!("err: {:?}", res);
                    state = default_response_result(state, res);
                    break;
                }
            },
            ResponseState::Parsing {
                status,
                mut headers,
                index,
            } => {
                //println!("will parse header:\n{}", &buf[index..].to_hex(16));
                match message_header(&buf[index..]) {
                    Ok((i, header)) => {
                        println!("header: {:?}", header);
                        headers.push(Header::new(buf, header.name, header.value));
                        state = ResponseState::Parsing {
                            status,
                            headers,
                            index: buf.offset(i),
                        };
                    }
                    Err(_) => match crlf(&buf[index..]) {
                        Ok((i, _o)) => {
                            state = ResponseState::ParsingDone {
                                status,
                                headers,
                                index: buf.offset(i),
                                data: Slice::new(buf, i, Meta::Data),
                            };
                            println!(
                                "parsing done from\n{}\nremaining ->\n{}\nstate: {:?}",
                                (&buf[index..]).to_hex(16),
                                i.to_hex(16),
                                state
                            );
                            break;
                        }
                        res => {
                            state = default_response_result(
                                ResponseState::Parsing {
                                    status,
                                    headers,
                                    index,
                                },
                                res,
                            );
                            break;
                        }
                    },
                    res => {
                        state = default_response_result(
                            ResponseState::Parsing {
                                status,
                                headers,
                                index,
                            },
                            res,
                        );
                        break;
                    }
                }
            }
            ResponseState::ResponseWithBodyChunks {
                status,
                connection,
                chunk,
            } => {
                let (mv, chunk) = chunk.parse(buf);
                println!("chunk parse returned {:?} {:?}", mv, chunk);
                state = ResponseState::ResponseWithBodyChunks {
                    status,
                    connection,
                    chunk,
                };

                if let BufferMove::Advance(sz) = mv {
                    return (state, Some(sz));
                } else {
                    return (state, None);
                }
            }
            s => panic!(
                "parse_response_until_stop should not be called with this state: {:?}",
                s
            ),
        }
    }

    let header_end = if let ResponseState::ParsingDone { index, .. } = state {
        Some(index)
    } else {
        None
    };

    state = match state {
        ResponseState::ParsingDone {
            status,
            headers,
            index,
            data,
        } => finish_response(
            status,
            headers,
            index,
            data,
            buffer,
            is_head,
            added_res_header,
            sticky_name,
        ),
        s => s,
    };

    (state, header_end)
}

fn add_sticky_session_to_response(
    buf: &mut BufferQueue,
    sticky_name: &str,
    sticky_session: Option<&StickySession>,
) {
    if let Some(ref sticky_backend) = sticky_session {
        let sticky_cookie = format!(
            "Set-Cookie: {}={}; Path=/\r\n",
            sticky_name, sticky_backend.sticky_id
        );
        buf.insert_output(Vec::from(sticky_cookie.as_bytes()));
    }
}

fn finish_response(
    status: StatusLine,
    headers: Vec<Header>,
    index: usize,
    data: Slice,
    buffer: &mut HttpBuffer,
    is_head: bool,
    added_res_header: Option<&AddedHeader>,
    _sticky_name: &str,
) -> ResponseState {
    let mut connection = Connection::new();
    let mut length: Option<LengthInformation> = None;
    let _upgrade: Option<UpgradeProtocol> = None;
    let status_line = status.to_rstatus_line(buffer.unparsed_data());

    match status.version.meta {
        Meta::Version(Version::V10) => connection.keep_alive = Some(false),
        Meta::Version(Version::V11) => connection.keep_alive = Some(true),
        _ => {}
    }

    for header in headers.iter() {
        match header.name.meta {
            Meta::HeaderName(HeaderName::ContentLength) => {
                // when we have a head request, we won't use the length header
                if !is_head {
                    match header.value.data(buffer.unparsed_data()) {
                        None => unimplemented!(),
                        Some(s) => {
                            match str::from_utf8(s).ok().and_then(|s| s.parse::<usize>().ok()) {
                                None => unimplemented!(),
                                Some(sz) => {
                                    if length.is_none() {
                                        length = Some(LengthInformation::Length(sz));
                                        // we should allow multiple Content-Length headers if they have the same value
                                    } else {
                                        return ResponseState::Error {
                                            status: Some(status),
                                            headers,
                                            data: Some(data),
                                            index,
                                            upgrade: None,
                                            connection: Some(connection),
                                            length,
                                            chunk: None,
                                        };
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Meta::HeaderName(HeaderName::TransferEncoding) => {
                match header.value.data(buffer.unparsed_data()) {
                    None => unimplemented!(),
                    Some(s) => {
                        for value in super::comma_separated_values(s) {
                            // when we have a head request, we won't use the length header
                            if !is_head {
                                // Transfer-Encoding gets the priority over Content-Length
                                if super::compare_no_case(value, b"chunked") {
                                    length = Some(LengthInformation::Chunked);
                                }
                            }
                        }
                    }
                }
            }

            Meta::HeaderName(HeaderName::Connection) => {
                match header.value.data(buffer.unparsed_data()) {
                    None => unimplemented!(),
                    Some(s) => {
                        for value in super::comma_separated_values(s) {
                            println!(
                                "connection header contains: {:?}",
                                std::str::from_utf8(value)
                            );
                            if super::compare_no_case(value, b"close") {
                                connection.keep_alive = Some(false);
                                continue;
                            }
                            if super::compare_no_case(value, b"keep-alive") {
                                connection.keep_alive = Some(true);
                                continue;
                            }
                            if super::compare_no_case(value, b"upgrade") {
                                connection.has_upgrade = true;
                                continue;
                            }
                        }
                    }
                }
            }

            Meta::HeaderName(HeaderName::Upgrade) => {
                match header.value.data(buffer.unparsed_data()) {
                    None => unimplemented!(),
                    Some(s) => {
                        connection.upgrade = String::from_utf8(s.to_vec()).ok();
                    }
                }
            }
            /*
            Meta::HeaderName(HeaderName::Cookie) => match header.value.data(buffer.unparsed_data())
            {
                None => unimplemented!(),
                Some(s) => match parse_request_cookies(s) {
                    None => {
                        return RequestState::Error {
                            request: Some(request),quest
                            headers,
                            data: Some(data),
                            index,
                            host,
                            connection: Some(connection),
                            length,
                            chunk: None,
                        }
                    }
                    Some(cookies) => {
                        let sticky_session_header = cookies
                            .into_iter()
                            .find(|cookie| &(cookie.name)[..] == sticky_name.as_bytes());
                        if let Some(sticky_session) = sticky_session_header {
                            connection.sticky_session = str::from_utf8(sticky_session.value)
                                .map(|s| s.to_string())
                                .ok();
                        }
                    }
                },
            },*/
            _ => {}
        };
    }

    let upgrade: Option<String> = None;

    if status_line.is_none() {
        return ResponseState::Error {
            status: Some(status),
            headers,
            data: Some(data),
            index,
            upgrade: None,
            connection: Some(connection),
            length,
            chunk: None,
        };
    }
    let status_line = status_line.unwrap();

    let mut header_slices = Vec::new();
    status.as_copying_slices(&mut header_slices);
    for h in headers.iter() {
        h.as_copying_slices(&mut header_slices);
    }

    /*
    let state = if is_head ||
                    // all 1xx responses
                    status_line.status / 100  == 1 || status_line.status == 204 || status_line.status == 304
                {*/
    println!("fixme: if not head or connection upgrade, set header connection: close because we deleted it");
    println!("FIXME: delete some headers");
    println!("FIXME: add sticky cookie");

    if let Some(added) = added_res_header {
        added.as_copying_slices_response(&mut header_slices);
    }

    header_slices.push(CopyingSlice::Static(&b"\r\n"[..]));

    let state = ResponseState::CopyingHeaders {
        status: Some(status_line),
        headers,
        data,
        index,
        connection,
        upgrade,
        length,
        header_slices,
        is_head,
    };

    println!("result state: {:?}", state);
    state
}
