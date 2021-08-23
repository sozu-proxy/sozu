#![allow(unused)]
use super::super::buffer::HttpBuffer;
use crate::sozu_command::buffer::fixed::Buffer;

use nom::{Err, HexDisplay, IResult, Offset};

use url::Url;

use std::convert::From;
use std::str;

use super::header::{
    self, request_line, CopyingSlice, Header, HeaderName, Meta, RequestLine, Slice, Version,
};
use super::{
    crlf, message_header, BufferMove, Chunk, Connection, Continue, HeaderValue, Host,
    LengthInformation, Method, RRequestLine, TransferEncodingValue,
};
use crate::protocol::http::cookies::parse_request_cookies;
use crate::protocol::http::AddedHeader;

#[derive(Debug, Clone, PartialEq)]
pub enum RequestState {
    Initial,
    Error {
        request: Option<RequestLine>,
        headers: Vec<Header>,
        data: Option<Slice>,
        index: usize,
        host: Option<Host>,
        connection: Option<Connection>,
        length: Option<LengthInformation>,
        chunk: Option<Chunk>,
    },
    // index is how far we have parsed in the buffer
    Parsing {
        request: RequestLine,
        headers: Vec<Header>,
        index: usize,
    },
    ParsingDone {
        request: RequestLine,
        headers: Vec<Header>,
        /// start of body
        data: Slice,
        /// position of end of headers
        index: usize,
    },
    CopyingHeaders {
        request: Option<RRequestLine>,
        headers: Vec<Header>,
        data: Slice,
        index: usize,
        connection: Connection,
        host: Host,
        length: Option<LengthInformation>,
        header_slices: Vec<CopyingSlice>,
    },
    Request {
        request: RRequestLine,
        connection: Connection,
        host: Host,
    },
    RequestWithBody {
        request: RRequestLine,
        connection: Connection,
        host: Host,
        length: usize,
    },
    RequestWithBodyChunks {
        request: RRequestLine,
        connection: Connection,
        host: Host,
        chunk: Chunk,
    },
}

impl RequestState {
    pub fn into_error(self) -> RequestState {
        match self {
            RequestState::Initial => RequestState::Error {
                request: None,
                headers: Vec::new(),
                data: None,
                index: 0,
                host: None,
                connection: None,
                length: None,
                chunk: None,
            },
            RequestState::Parsing {
                request,
                headers,
                index,
            } => RequestState::Error {
                request: Some(request),
                headers,
                data: None,
                index,
                host: None,
                connection: None,
                length: None,
                chunk: None,
            },
            RequestState::ParsingDone {
                request,
                headers,
                data,
                index,
            } => RequestState::Error {
                request: Some(request),
                headers,
                data: Some(data),
                index,
                host: None,
                connection: None,
                length: None,
                chunk: None,
            },
            RequestState::Request {
                connection, host, ..
            } => RequestState::Error {
                request: None,
                headers: Vec::new(),
                data: None,
                index: 0,
                host: Some(host),
                connection: Some(connection),
                length: None,
                chunk: None,
            },
            RequestState::RequestWithBody {
                connection,
                host,
                length,
                ..
            } => RequestState::Error {
                request: None,
                headers: Vec::new(),
                data: None,
                index: 0,
                host: Some(host),
                connection: Some(connection),
                length: Some(LengthInformation::Length(length)),
                chunk: None,
            },
            RequestState::RequestWithBodyChunks {
                connection,
                host,
                chunk,
                ..
            } => RequestState::Error {
                request: None,
                headers: Vec::new(),
                data: None,
                index: 0,
                host: Some(host),
                connection: Some(connection),
                length: Some(LengthInformation::Chunked),
                chunk: Some(chunk),
            },
            err => err,
        }
    }

    pub fn is_front_error(&self) -> bool {
        if let RequestState::Error { .. } = self {
            true
        } else {
            false
        }
    }

    pub fn get_sticky_session(&self) -> Option<&str> {
        self.get_keep_alive()
            .and_then(|con| con.sticky_session.as_ref())
            .map(|s| s.as_str())
    }

    pub fn has_host(&self) -> bool {
        match *self {
            RequestState::CopyingHeaders { .. }
            | RequestState::Request { .. }
            | RequestState::RequestWithBody { .. }
            | RequestState::RequestWithBodyChunks { .. } => true,
            _ => false,
        }
    }

    pub fn is_proxying(&self) -> bool {
        match *self {
            RequestState::CopyingHeaders { .. }
            | RequestState::Request { .. }
            | RequestState::RequestWithBody { .. }
            | RequestState::RequestWithBodyChunks { .. } => true,
            _ => false,
        }
    }

    pub fn is_head(&self) -> bool {
        match *self {
            RequestState::CopyingHeaders { ref request, .. } => request
                .as_ref()
                .map(|r| r.method == Method::Head)
                .unwrap_or(false),
            RequestState::Request { ref request, .. }
            | RequestState::RequestWithBody { ref request, .. }
            | RequestState::RequestWithBodyChunks { ref request, .. } => {
                request.method == Method::Head
            }
            _ => false,
        }
    }

    pub fn get_host(&self) -> Option<&str> {
        match *self {
            RequestState::CopyingHeaders { ref host, .. }
            | RequestState::Request { ref host, .. }
            | RequestState::RequestWithBody { ref host, .. }
            | RequestState::RequestWithBodyChunks { ref host, .. } => Some(host.as_str()),
            RequestState::Error { ref host, .. } => host.as_ref().map(|s| s.as_str()),
            _ => None,
        }
    }

    pub fn get_uri(&self) -> Option<&str> {
        match *self {
            RequestState::CopyingHeaders { ref request, .. } => {
                request.as_ref().map(|r| r.uri.as_str())
            }
            RequestState::Request { ref request, .. }
            | RequestState::RequestWithBody { ref request, .. }
            | RequestState::RequestWithBodyChunks { ref request, .. } => Some(request.uri.as_str()),
            //FIXME
            //RequestState::Error{ rl, .. }              => rl.as_ref().map(|r| r.uri.as_str()),
            _ => None,
        }
    }

    pub fn get_request_line(&self) -> Option<&RRequestLine> {
        match *self {
            RequestState::CopyingHeaders { ref request, .. } => request.as_ref(),
            RequestState::Request { ref request, .. }
            | RequestState::RequestWithBody { ref request, .. }
            | RequestState::RequestWithBodyChunks { ref request, .. } => Some(request),
            //FIXME
            //RequestState::Error{ rl, .. }           => rl.as_ref(),
            _ => None,
        }
    }

    pub fn get_keep_alive(&self) -> Option<&Connection> {
        match *self {
            RequestState::CopyingHeaders { ref connection, .. }
            | RequestState::Request { ref connection, .. }
            | RequestState::RequestWithBody { ref connection, .. }
            | RequestState::RequestWithBodyChunks { ref connection, .. } => Some(connection),
            RequestState::Error { ref connection, .. } => connection.as_ref(),
            _ => None,
        }
    }

    pub fn get_mut_connection(&mut self) -> Option<&mut Connection> {
        match *self {
            RequestState::CopyingHeaders {
                ref mut connection, ..
            }
            | RequestState::Request {
                ref mut connection, ..
            }
            | RequestState::RequestWithBody {
                ref mut connection, ..
            }
            | RequestState::RequestWithBodyChunks {
                ref mut connection, ..
            } => Some(connection),
            _ => None,
        }
    }

    pub fn should_copy(&self, position: usize) -> Option<usize> {
        match *self {
            RequestState::RequestWithBody { length, .. } => Some(position + length),
            RequestState::Request { .. } => Some(position),
            _ => None,
        }
    }

    pub fn should_keep_alive(&self) -> bool {
        use super::Version;

        //FIXME: should not clone here
        let rl = self.get_request_line();
        let version = rl.as_ref().map(|rl| rl.version);
        let conn = self.get_keep_alive();
        match (version, conn.map(|c| c.keep_alive)) {
            (_, Some(Some(true))) => true,
            (_, Some(Some(false))) => false,
            (Some(Version::V10), _) => false,
            (Some(Version::V11), _) => true,
            (_, _) => false,
        }
    }

    pub fn as_ioslice<'a>(&'a self, buffer: &'a [u8]) -> Vec<std::io::IoSlice<'a>> {
        let mut v = Vec::new();

        match *self {
            RequestState::CopyingHeaders {
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
            RequestState::Request { .. } => {}
            RequestState::RequestWithBody { length, .. } => {
                let sz = std::cmp::min(length, buffer.len());
                v.push(std::io::IoSlice::new(&buffer[..sz]));
            }
            RequestState::RequestWithBodyChunks { chunk, .. } => match chunk {
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
            RequestState::Initial
            | RequestState::Error { .. }
            | RequestState::Parsing { .. }
            | RequestState::ParsingDone { .. } => {}
        }
        v
    }

    pub fn next_slice<'a>(&'a self, buffer: &'a [u8]) -> &'a [u8] {
        match *self {
            RequestState::CopyingHeaders {
                ref header_slices, ..
            } => header_slices
                .get(0)
                .and_then(|h| match h {
                    CopyingSlice::Static(s) => Some(*s),
                    CopyingSlice::Slice(s) => s.data(buffer),
                    CopyingSlice::Vec(v, index) => Some(&v[*index..]),
                })
                .unwrap_or(&b""[..]),
            RequestState::Request { .. } => &b""[..],
            RequestState::RequestWithBody { length, .. } => {
                let sz = std::cmp::min(length, buffer.len());
                &buffer[..sz]
            }
            RequestState::RequestWithBodyChunks { chunk, .. } => match chunk {
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
            RequestState::Initial
            | RequestState::Error { .. }
            | RequestState::Parsing { .. }
            | RequestState::ParsingDone { .. } => &buffer[..0],
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
            RequestState::CopyingHeaders {
                request,
                data,
                index,
                mut connection,
                host,
                length,
                headers,
                mut header_slices,
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
                info!(
                    "consumed {} bytes, remaining slices: {:?}",
                    c, header_slices
                );

                if !header_slices.is_empty() {
                    return RequestState::CopyingHeaders {
                        request,
                        data,
                        index,
                        connection,
                        host,
                        length,
                        headers,
                        header_slices,
                    };
                }

                buffer.consume_parsed_data(index);
                let request_line = request.unwrap();
                let state = match length {
                    None => RequestState::Request {
                        request: request_line,
                        connection,
                        host,
                    },
                    Some(LengthInformation::Length(length)) => {
                        if connection.has_continue() {
                            connection.continues = Continue::Expects(length);
                            RequestState::Request {
                                request: request_line,
                                connection,
                                host,
                            }
                        } else {
                            RequestState::RequestWithBody {
                                request: request_line,
                                connection,
                                host,
                                length,
                            }
                        }
                    }
                    Some(LengthInformation::Chunked) => RequestState::RequestWithBodyChunks {
                        request: request_line,
                        connection,
                        host,
                        chunk: Chunk::Initial,
                    },
                };
                state
            }
            RequestState::RequestWithBody {
                request,
                connection,
                host,
                length,
            } => {
                buffer.consume_parsed_data(consumed);
                RequestState::RequestWithBody {
                    request,
                    connection,
                    host,
                    length: length - consumed,
                }
            }
            RequestState::RequestWithBodyChunks {
                request,
                connection,
                host,
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
                RequestState::RequestWithBodyChunks {
                    request,
                    connection,
                    host,
                    chunk,
                }
            }
            _ => {
                error!("should not have called consume() on state: {:?}", &self);
                self
            }
        }
    }

    pub fn can_restart_parsing(&self, available_data: usize) -> bool {
        match self {
            RequestState::Request { .. }
            | RequestState::RequestWithBody { length: 0, .. }
            | &RequestState::RequestWithBodyChunks {
                chunk: Chunk::Ended,
                ..
            } => true,

            s => false,
        }
    }
}

pub fn default_request_result<O>(state: RequestState, res: IResult<&[u8], O>) -> RequestState {
    match res {
        Err(Err::Error(_)) | Err(Err::Failure(_)) => state.into_error(),
        Err(Err::Incomplete(_)) => state,
        _ => unreachable!(),
    }
}

pub fn parse_request_until_stop(
    mut state: RequestState,
    mut header_end: Option<usize>,
    buffer: &mut HttpBuffer,
    added_req_header: Option<&AddedHeader>,
    sticky_name: &str,
) -> (RequestState, Option<usize>) {
    let buf = buffer.unparsed_data();
    info!("will parse:\n{}", buf.to_hex(16));

    loop {
        info!("state: {:?}", state);
        match state {
            RequestState::Initial => match header::request_line(buf) {
                Ok((i, (method, uri, version))) => {
                    let rline = RequestLine::new(buf, method, uri, version);
                    println!("rline: {:?}", rline);
                    state = RequestState::Parsing {
                        request: rline,
                        headers: Vec::new(),
                        index: buf.offset(i),
                    };
                }
                Err(Err::Incomplete(_)) => break,
                res => {
                    println!("err: {:?}", res);
                    state = default_request_result(state, res);
                    break;
                }
            },
            RequestState::Parsing {
                request,
                mut headers,
                index,
            } => {
                println!("will parse header:\n{}", &buf[index..].to_hex(16));
                match message_header(&buf[index..]) {
                    Ok((i, header)) => {
                        println!("header: {:?}", header);
                        headers.push(Header::new(buf, header.name, header.value));
                        state = RequestState::Parsing {
                            request,
                            headers,
                            index: buf.offset(i),
                        };
                    }
                    Err(_) => match crlf(&buf[index..]) {
                        Ok((i, o)) => {
                            println!(
                                "parsing done from\n{}\nremaining ->\n{}",
                                (&buf[index..]).to_hex(16),
                                i.to_hex(16)
                            );
                            state = RequestState::ParsingDone {
                                request,
                                headers,
                                index: buf.offset(i),
                                data: Slice::new(buf, i, Meta::Data),
                            };
                            break;
                        }
                        res => {
                            state = default_request_result(
                                RequestState::Parsing {
                                    request,
                                    headers,
                                    index,
                                },
                                res,
                            );
                            break;
                        }
                    },
                    res => {
                        state = default_request_result(
                            RequestState::Parsing {
                                request,
                                headers,
                                index,
                            },
                            res,
                        );
                        break;
                    }
                }
            }
            s => panic!(
                "parse_request_until_stop should not be called with this state: {:?}",
                s
            ),
        }
    }

    let header_end = if let RequestState::ParsingDone { index, .. } = state {
        Some(index)
    } else {
        None
    };

    state = match state {
        RequestState::ParsingDone {
            request,
            headers,
            index,
            data,
        } => finish_request(
            request,
            headers,
            index,
            data,
            buffer,
            added_req_header,
            sticky_name,
        ),
        s => s,
    };

    (state, header_end)
}

// this function will try to parse multiple chunks at once
pub fn parse_chunks(
    mut state: RequestState,
    buffer: &mut HttpBuffer,
) -> (RequestState, Option<usize>) {
    match state {
        RequestState::RequestWithBodyChunks {
            request,
            connection,
            host,
            chunk,
        } => {
            let (advance, chunk) = chunk.parse(buffer.unparsed_data());
            match advance {
                BufferMove::Advance(sz) => (
                    RequestState::RequestWithBodyChunks {
                        request,
                        connection,
                        host,
                        chunk,
                    },
                    Some(sz),
                ),
                BufferMove::None => (
                    RequestState::RequestWithBodyChunks {
                        request,
                        connection,
                        host,
                        chunk,
                    },
                    None,
                ),
                _ => panic!(),
            }
        }
        _ => (state, None),
    }
}

fn finish_request(
    request: RequestLine,
    mut headers: Vec<Header>,
    index: usize,
    data: Slice,
    buffer: &mut HttpBuffer,
    added_req_header: Option<&AddedHeader>,
    sticky_name: &str,
) -> RequestState {
    let mut connection = Connection::new();
    let mut length: Option<LengthInformation> = None;
    let request_line = request.to_rrequest_line(buffer.unparsed_data());
    let mut host: Option<String> = request_line
        .as_ref()
        .and_then(|rl| Url::parse(&rl.uri).ok())
        .and_then(|u| u.host_str().map(|s| s.to_string()));

    match request.version.meta {
        Meta::Version(Version::V10) => connection.keep_alive = Some(false),
        Meta::Version(Version::V11) => connection.keep_alive = Some(true),
        _ => {}
    }

    for header in headers.iter() {
        match header.name.meta {
            Meta::HeaderName(HeaderName::Host) => match header.value.data(buffer.unparsed_data()) {
                None => unimplemented!(),
                Some(s) => {
                    if host.is_some() {
                        return RequestState::Error {
                            request: Some(request),
                            headers,
                            data: Some(data),
                            index,
                            host,
                            connection: Some(connection),
                            length,
                            chunk: None,
                        };
                    }

                    host = std::str::from_utf8(s).ok().map(String::from);
                }
            },
            Meta::HeaderName(HeaderName::ContentLength) => {
                match header.value.data(buffer.unparsed_data()) {
                    None => unimplemented!(),
                    Some(s) => match str::from_utf8(s).ok().and_then(|s| s.parse::<usize>().ok()) {
                        None => unimplemented!(),
                        Some(sz) => {
                            if length.is_none() {
                                length = Some(LengthInformation::Length(sz));
                                // we should allow multiple Content-Length headers if they have the same value
                            } else {
                                return RequestState::Error {
                                    request: Some(request),
                                    headers,
                                    data: Some(data),
                                    index,
                                    host,
                                    connection: Some(connection),
                                    length,
                                    chunk: None,
                                };
                            }
                        }
                    },
                }
            }
            Meta::HeaderName(HeaderName::TransferEncoding) => {
                match header.value.data(buffer.unparsed_data()) {
                    None => unimplemented!(),
                    Some(s) => {
                        for value in super::comma_separated_values(s) {
                            // Transfer-Encoding gets the priority over Content-Length
                            if super::compare_no_case(value, b"chunked") {
                                length = Some(LengthInformation::Chunked);
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
            Meta::HeaderName(HeaderName::Expect) => match header.value.data(buffer.unparsed_data())
            {
                None => unimplemented!(),
                Some(s) => {
                    if super::compare_no_case(s, b"100-continue") {
                        connection.continues = Continue::Expects(0);
                    }
                }
            },
            Meta::HeaderName(HeaderName::Forwarded) => {
                match header.value.data(buffer.unparsed_data()) {
                    None => unimplemented!(),
                    Some(s) => {
                        connection.forwarded.forwarded = String::from_utf8(s.to_vec()).ok();
                    }
                }
            }
            Meta::HeaderName(HeaderName::XForwardedFor) => {
                match header.value.data(buffer.unparsed_data()) {
                    None => unimplemented!(),
                    Some(s) => {
                        connection.forwarded.x_for = String::from_utf8(s.to_vec()).ok();
                    }
                }
            }
            Meta::HeaderName(HeaderName::XForwardedPort) => connection.forwarded.x_port = true,
            Meta::HeaderName(HeaderName::XForwardedProto) => connection.forwarded.x_proto = true,
            Meta::HeaderName(HeaderName::Upgrade) => {
                match header.value.data(buffer.unparsed_data()) {
                    None => unimplemented!(),
                    Some(s) => {
                        connection.upgrade = String::from_utf8(s.to_vec()).ok();
                    }
                }
            }
            Meta::HeaderName(HeaderName::Cookie) => match header.value.data(buffer.unparsed_data())
            {
                None => unimplemented!(),
                Some(s) => match parse_request_cookies(s) {
                    None => {
                        return RequestState::Error {
                            request: Some(request),
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
            },
            _ => {}
        };
    }

    if request_line.is_none() || host.is_none() {
        return RequestState::Error {
            request: Some(request),
            headers,
            data: Some(data),
            index,
            host,
            connection: Some(connection),
            length,
            chunk: None,
        };
    }

    let request_line = request_line.unwrap();

    let mut header_slices = Vec::new();
    request.as_copying_slices(&mut header_slices);
    for h in headers.iter() {
        //if !h.should_delete() {
        h.as_copying_slices(&mut header_slices);
        //}
    }

    println!("FIXME: delete some headers");

    if let Some(added) = added_req_header {
        added.as_copying_slices_request(&connection.forwarded, &mut header_slices);
    }

    header_slices.push(CopyingSlice::Static(&b"\r\n"[..]));

    let state = RequestState::CopyingHeaders {
        request: Some(request_line),
        headers,
        data,
        index,
        connection,
        host: host.unwrap(),
        length,
        header_slices,
    };

    println!("result state: {:?}", state);
    state
}
