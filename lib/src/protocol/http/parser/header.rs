#![allow(unused)]
// new parsing ideas
// remove the HasHost, HasLength etc options,
// since they are not used anymore
//
// the entire request headers should fit in the input buffer
// which will be oftenr around 16393 bytes
//
// each element of the headers will be defined by
// its offset from the beginning of the stream,
// and its length
// the offset will be store on 32 bytes, and
// the length: on 16 bytes:
// - the request or status line and headers shall
// not be larger than 4GB (but we could have a
// buffer larger than 65kB)
// - the length of a single element is stored on 16 bytes,
// so we have a hard limit on URL, header name and header
// size of 65kB (but we will make it configurable)
//
// a header will be composed of a slice for the name,
// and one for the value
// we will need a kind of Cow semantics:
// - we might want to rewrite a header's value
// - we might want to rewrite it in place if there's enough room
// - otherwise we might have to write it to allocate a new header or write to the end of the buffer
//
// this will represent the request or status line and the headers list,
// and will be used by the router and various filters or plugins.
// since it refers to the buffer's data, once the data has been flushed
// to the output socket, this structure will be removed (and any data
// we want to keep should be allocated elsewhere)
//
// we must reserve space at the end of the buffer for rewriting. Haproxy has
// the tune.maxrewrite option, with the default value of half the buffer, and
// the recommended value of 1024 bytes
//
// other assumptions:
// - we do not start modifying headers until after we've seen the header end. This means that if
// there is not enough room to rewrite a header, we might write at the end of the buffer, and that
// can be after a part of the body, so here is the process:
//  - parse and accumulate Header structs, until HeaderEnd then the Data part
//  - handle and modify headers, possibly adding headers to the end
//  - start writing headers to the next socket, until header end. For each header, we can produce
//  slices of the input buffer. If possible, coalesce the slices
//  - reset the positions of the input buffer to the start and end of the Data part. If we wrote
//  headers after that part, we will be able to reclaim that space for more data
//

use crate::protocol::http::parser::RStatusLine;

use super::{compare_no_case, is_header_value_char, status_token};
use super::{crlf, one_of, sp, tag, token, vchar_1};
use nom::Offset;
use nom::{
    bytes::streaming::{take, take_while},
    character::streaming::digit1,
    combinator::{recognize, verify},
    sequence::tuple,
    IResult,
};
use std::str::from_utf8;

// slice into a buffer
//
// we use 32 bits since w'll use those for headers,
// and headers will
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Slice {
    pub start: u32,
    pub len: u16,
    pub meta: Meta,
}

impl Slice {
    // data MUST be a subset of buffer
    pub fn new(buffer: &[u8], data: &[u8], meta: Meta) -> Slice {
        let offset = buffer.offset(data);
        assert!(
            offset <= u32::MAX as usize,
            "header slices should not start at more than 4GB from its beginning"
        );
        assert!(
            data.len() <= u16::MAX as usize,
            "header slices should not be larger than 65536 bytes"
        );

        Slice {
            start: offset as u32,
            len: data.len() as u16,
            meta,
        }
    }

    /// returns the underlying data for the slice
    /// this allows us to manipulate and store slices independently from the buffer
    pub fn data<'a>(&self, buffer: &'a [u8]) -> Option<&'a [u8]> {
        let start = self.start as usize;
        let end = start + self.len as usize;

        if start <= buffer.len() && end <= buffer.len() {
            Some(&buffer[start..end])
        } else {
            None
        }
    }

    /// changes the content of the slice in the original buffer, if there is enough room
    pub fn modify(&mut self, buffer: &mut crate::pool::Checkout, new_value: &[u8]) -> bool {
        // the new slice is smaller than this one: rewrite in place and reduce the size
        if new_value.len() <= self.len as usize {
            /*
            let new_start = self.start as usize + self.len as usize - new_value.len();
            (buffer.data_mut()[new_start..new_start+new_value.len()]).copy_from_slice(new_value);

            self.start = new_start as u32;
            self.len = new_value.len() as u16;
            */
            (buffer.data_mut()[self.start as usize..self.start as usize + new_value.len()])
                .copy_from_slice(new_value);

            //self.start = new_start as u32;
            self.len = new_value.len() as u16;
            true
        // the new slice is larger than this one: write at the end of the buffer
        } else if new_value.len() <= buffer.available_space() {
            let current_offset = buffer.available_data();
            (buffer.space()[..new_value.len()]).copy_from_slice(new_value);
            buffer.fill(new_value.len());

            self.start = current_offset as u32;
            self.len = new_value.len() as u16;
            true
        } else {
            false
        }
    }

    pub fn consume(self, sz: usize) -> (usize, Option<Slice>) {
        if sz >= self.len as usize {
            (sz - self.len as usize, None)
        } else {
            let Slice { start, len, meta } = self;
            (
                0,
                Some(Slice {
                    start: start + (sz as u32),
                    len: len - (sz as u16),
                    meta,
                }),
            )
        }
    }

    pub fn len(&self) -> usize {
        self.len.into()
    }
}

//represents the "type" of the slice
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Meta {
    Method(Method),
    Version(Version),
    Uri,
    Status,
    Reason,
    HeaderName(HeaderName),
    HeaderValue,
    HeadersEnd,
    Data,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Method {
    Get,
    Post,
    Head,
    Options,
    Put,
    Delete,
    Trace,
    Connect,
    Custom,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Version {
    V10,
    V11,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum HeaderName {
    Host,
    ContentLength,
    Encoding,
    Connection,
    Upgrade,
    Cookie,
    TransferEncoding,
    Accept,
    Expect,
    Forwarded,
    XForwardedFor,
    XForwardedPort,
    XForwardedProto,
    OtherHeader,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CopyingSlice {
    Slice(Slice),
    Static(&'static [u8]),
    Vec(Vec<u8>, usize),
}

impl CopyingSlice {
    pub fn consume(self, sz: usize) -> (usize, Option<CopyingSlice>) {
        //println!("will consume({}) {:?}", sz, self);
        let res = match self {
            CopyingSlice::Slice(s) => {
                let (remaining, opt) = s.consume(sz);
                (remaining, opt.map(CopyingSlice::Slice))
            }
            CopyingSlice::Static(s) => {
                if sz >= s.len() {
                    (sz - s.len(), None)
                } else {
                    (0, Some(CopyingSlice::Static(&s[sz..])))
                }
            }
            CopyingSlice::Vec(v, index) => {
                if sz >= v.len() - index {
                    (sz - v.len() + index, None)
                } else {
                    (0, Some(CopyingSlice::Vec(v, index + sz)))
                }
            }
        };
        //println!("consumed {:?}", res);
        res
    }

    pub fn vec(v: Vec<u8>) -> Self {
        CopyingSlice::Vec(v, 0)
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Header {
    pub name: Slice,
    pub value: Slice,
}

impl Header {
    pub fn new(buffer: &[u8], name: &[u8], value: &[u8]) -> Self {
        //FIXME: we only accept the : right after the header name
        let inner_name = &name[..name.len()];
        let name_meta = Header::calculate_meta(inner_name);

        Header {
            name: Slice::new(buffer, name, name_meta),
            value: Slice::new(buffer, value, Meta::HeaderValue),
        }
    }

    fn calculate_meta(name: &[u8]) -> Meta {
        if compare_no_case(name, b"host") {
            Meta::HeaderName(HeaderName::Host)
        } else if compare_no_case(name, b"Content-Length") {
            Meta::HeaderName(HeaderName::ContentLength)
        } else if compare_no_case(name, b"Connection") {
            Meta::HeaderName(HeaderName::Connection)
        } else if compare_no_case(name, b"Encoding") {
            Meta::HeaderName(HeaderName::Encoding)
        } else if compare_no_case(name, b"Transfer-Encoding") {
            Meta::HeaderName(HeaderName::TransferEncoding)
        } else if compare_no_case(name, b"Cookie") {
            Meta::HeaderName(HeaderName::Cookie)
        } else if compare_no_case(name, b"Upgrade") {
            Meta::HeaderName(HeaderName::Upgrade)
        } else if compare_no_case(name, b"Accept") {
            Meta::HeaderName(HeaderName::Accept)
        } else if compare_no_case(name, b"Expect") {
            Meta::HeaderName(HeaderName::Expect)
        } else if compare_no_case(name, b"Forwarded") {
            Meta::HeaderName(HeaderName::Forwarded)
        } else if compare_no_case(name, b"X-Forwarded-For") {
            Meta::HeaderName(HeaderName::XForwardedFor)
        } else if compare_no_case(name, b"X-Forwarded-Port") {
            Meta::HeaderName(HeaderName::XForwardedPort)
        } else if compare_no_case(name, b"X-Forwarded-Proto") {
            Meta::HeaderName(HeaderName::XForwardedProto)
        } else {
            Meta::HeaderName(HeaderName::OtherHeader)
        }
    }

    pub fn name<'a>(&self, buffer: &'a [u8]) -> Option<&'a str> {
        self.name
            .data(buffer)
            .map(|s| &s[..s.len()])
            .and_then(|s| std::str::from_utf8(s).ok())
    }

    pub fn value<'a>(&self, buffer: &'a [u8]) -> Option<&'a str> {
        self.value
            .data(buffer)
            .map(|s| &s[..s.len()])
            .and_then(|s| std::str::from_utf8(s).ok())
    }

    pub fn modify(&mut self, buffer: &mut crate::pool::Checkout, new_value: &[u8]) -> bool {
        self.value.modify(buffer, new_value)
    }

    pub fn as_copying_slices(&self, v: &mut Vec<CopyingSlice>) {
        v.push(CopyingSlice::Slice(self.name));
        v.push(CopyingSlice::Static(&b": "[..]));
        v.push(CopyingSlice::Slice(self.value));
        v.push(CopyingSlice::Static(&b"\r\n"[..]));
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct RequestLine {
    pub method: Slice,
    pub version: Slice,
    pub uri: Slice,
}

impl RequestLine {
    pub fn new(buffer: &[u8], method: &[u8], uri: &[u8], version: &[u8]) -> Self {
        let method_meta = if compare_no_case(method, b"GET") {
            Meta::Method(Method::Get)
        } else if compare_no_case(method, b"POST") {
            Meta::Method(Method::Post)
        } else if compare_no_case(method, b"HEAD") {
            Meta::Method(Method::Head)
        } else if compare_no_case(method, b"OPTIONS") {
            Meta::Method(Method::Options)
        } else if compare_no_case(method, b"PUT") {
            Meta::Method(Method::Put)
        } else if compare_no_case(method, b"DELETE") {
            Meta::Method(Method::Delete)
        } else if compare_no_case(method, b"TRACE") {
            Meta::Method(Method::Trace)
        } else if compare_no_case(method, b"CONNECT") {
            Meta::Method(Method::Connect)
        } else {
            Meta::Method(Method::Custom)
        };

        let version_meta = if compare_no_case(version, b"HTTP/1.1") {
            Meta::Version(Version::V11)
        } else {
            //FIXME: older versions?
            Meta::Version(Version::V10)
        };

        RequestLine {
            method: Slice::new(buffer, method, method_meta),
            version: Slice::new(buffer, version, version_meta),
            uri: Slice::new(buffer, uri, Meta::Uri),
        }
    }

    pub fn uri<'a>(&self, buffer: &'a [u8]) -> Option<&'a str> {
        self.uri
            .data(buffer) //.map(|s| &s[..s.len() - 1])
            .and_then(|s| std::str::from_utf8(s).ok())
    }

    pub fn to_rrequest_line(&self, buffer: &[u8]) -> Option<super::RRequestLine> {
        let method = match self.method.meta {
            Meta::Method(Method::Get) => super::Method::Get,
            Meta::Method(Method::Post) => super::Method::Post,
            Meta::Method(Method::Head) => super::Method::Head,
            Meta::Method(Method::Options) => super::Method::Options,
            Meta::Method(Method::Put) => super::Method::Put,
            Meta::Method(Method::Delete) => super::Method::Delete,
            Meta::Method(Method::Trace) => super::Method::Trace,
            Meta::Method(Method::Connect) => super::Method::Connect,
            Meta::Method(Method::Custom) => match self.method.data(buffer) {
                None => return None,
                Some(s) => {
                    super::Method::Custom(String::from(unsafe { std::str::from_utf8_unchecked(s) }))
                }
            },
            _ => return None,
        };

        let version = match self.version.meta {
            Meta::Version(Version::V10) => super::Version::V10,
            Meta::Version(Version::V11) => super::Version::V11,
            _ => return None,
        };

        let uri = match self.uri.data(buffer) {
            None => return None,
            Some(s) => String::from(unsafe { std::str::from_utf8_unchecked(s) }),
        };

        Some(super::RRequestLine {
            method,
            uri,
            version,
        })
    }

    pub fn as_copying_slices(&self, v: &mut Vec<CopyingSlice>) {
        v.push(CopyingSlice::Slice(self.method));
        v.push(CopyingSlice::Static(&b" "[..]));
        v.push(CopyingSlice::Slice(self.uri));
        v.push(CopyingSlice::Static(&b" "[..]));
        v.push(CopyingSlice::Slice(self.version));
        v.push(CopyingSlice::Static(&b"\r\n"[..]));
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    request_line: RequestLine,
    headers: Vec<Header>,
}

pub fn request_line(i: &[u8]) -> IResult<&[u8], (&[u8], &[u8], &[u8])> {
    let (i, method) = token(i)?;
    let (i, _) = sp(i)?;
    let (i, uri) = vchar_1(i)?;
    let (i, _) = sp(i)?;
    let (i, version) = http_version(i)?;
    let (i, _) = crlf(i)?;

    Ok((i, (method, uri, version)))
}

fn http_version(i: &[u8]) -> IResult<&[u8], &[u8]> {
    recognize(tuple((tag("HTTP/1."), one_of("01"))))(i)
}

fn request(input: &[u8]) -> IResult<&[u8], (RequestLine, Vec<Header>)> {
    let i = input;
    let (mut i, (method, uri, version)) = request_line(i)?;

    let rline = RequestLine::new(input, method, uri, version);

    let mut v = Vec::new();
    loop {
        match super::message_header(i) {
            Ok((i2, h)) => {
                let header = Header::new(input, h.name, h.value);
                i = i2;
                v.push(header);
            }
            Err(_) => break,
        }
    }

    let (i, _) = crlf(i)?;

    Ok((i, (rline, v)))
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct StatusLine {
    pub version: Slice,
    pub status: Slice,
    pub reason: Slice,
}

impl StatusLine {
    pub fn new(buffer: &[u8], version: &[u8], status: &[u8], reason: &[u8]) -> Self {
        let version_meta = if compare_no_case(version, b"HTTP/1.1") {
            Meta::Version(Version::V11)
        } else {
            //FIXME: older versions?
            Meta::Version(Version::V10)
        };

        StatusLine {
            version: Slice::new(buffer, version, version_meta),
            status: Slice::new(buffer, status, Meta::Status),
            reason: Slice::new(buffer, reason, Meta::Reason),
        }
    }

    pub fn to_rstatus_line(&self, buffer: &[u8]) -> Option<super::RStatusLine> {
        let version = match self.version.meta {
            Meta::Version(Version::V10) => super::Version::V10,
            Meta::Version(Version::V11) => super::Version::V11,
            _ => return None,
        };

        let status = match self
            .status
            .data(buffer)
            .and_then(|data| from_utf8(data).ok())
            .and_then(|s| s.parse::<u16>().ok())
        {
            None => return None,
            Some(code) => code,
        };

        let reason = match self
            .reason
            .data(buffer)
            .and_then(|data| from_utf8(data).ok())
        {
            None => return None,
            Some(s) => s.to_string(),
        };

        Some(RStatusLine {
            version,
            status,
            reason,
        })
    }

    pub fn as_copying_slices(&self, v: &mut Vec<CopyingSlice>) {
        v.push(CopyingSlice::Slice(self.version));
        v.push(CopyingSlice::Static(&b" "[..]));
        v.push(CopyingSlice::Slice(self.status));
        v.push(CopyingSlice::Static(&b" "[..]));
        v.push(CopyingSlice::Slice(self.reason));
        v.push(CopyingSlice::Static(&b"\r\n"[..]));
    }
}

pub fn status_line(i: &[u8]) -> IResult<&[u8], (&[u8], &[u8], &[u8])> {
    let (i, version) = http_version(i)?;
    let (i, _) = sp(i)?;
    //FIXME: should recognize a number
    let (i, status) = verify(digit1, |s: &[u8]| s.len() == 3)(i)?;
    let (i, _) = sp(i)?;
    let (i, reason) = status_token(i)?;
    let (i, _) = crlf(i)?;

    Ok((i, (version, status, reason)))
}

fn response(input: &[u8]) -> IResult<&[u8], (StatusLine, Vec<Header>)> {
    let i = input;
    let (mut i, (version, status, reason)) = status_line(i)?;

    let sline = StatusLine::new(input, version, status, reason);

    let mut v = Vec::new();
    loop {
        match super::message_header(i) {
            Ok((i2, h)) => {
                let header = Header::new(input, h.name, h.value);
                i = i2;
                v.push(header);
            }
            Err(_) => break,
        }
    }

    let (i, _) = crlf(i)?;

    Ok((i, (sline, v)))
}

#[cfg(test)]
mod tests {
    use super::super::message_header;
    use super::*;
    use nom::HexDisplay;
    use std::io::Write;

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn size_test() {
        println!("size of header_name: {}", std::mem::size_of::<HeaderName>());
        assert_size!(Meta, 2);
        assert_size!(Slice, 8);
        assert_size!(Header, 16);
        //assert_size!(Slice2, 8);
        //panic!()
    }

    #[test]
    fn parse_modify() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 16384);
        let mut buffer = pool.checkout().unwrap();
        buffer.write(b"Connection: keep-alive\r\nContent-length: 12\r\n");
        println!("parsing: Connection: keep-alive\r\nContent-length: 12\r\n");
        let (i, h) = message_header(buffer.data()).unwrap();

        let mut header = Header::new(buffer.data(), h.name, h.value);

        println!("got header: {:?}", header);
        println!("name: {:?}", header.name(buffer.data()));
        println!("value: {:?}", header.value(buffer.data()));

        header.modify(&mut buffer, b"close");
        println!("modified header: {:?}", header);
        println!("name: {:?}", header.name(buffer.data()));
        println!("value: {:?}", header.value(buffer.data()));
        println!("modified buffer\n{}", buffer.data().to_hex(16));

        assert_eq!(
            std::str::from_utf8(buffer.data()).unwrap(),
            "Connection: closealive\r\nContent-length: 12\r\n"
        );
    }

    #[test]
    fn parse_modify_larger() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 16384);
        let mut buffer = pool.checkout().unwrap();
        buffer.write(b"Connection: close\r\nContent-length: 12\r\n");
        println!("parsing: Connection: close\r\nContent-length: 12\r\n");
        let (i, h) = message_header(buffer.data()).unwrap();

        let mut header = Header::new(buffer.data(), h.name, h.value);

        println!("got header: {:?}", header);

        header.modify(&mut buffer, b"keep-alive");
        println!("modified header: {:?}", header);
        println!("modified buffer\n{}", buffer.data().to_hex(16));
        assert_eq!(
            std::str::from_utf8(buffer.data()).unwrap(),
            "Connection: close\r\nContent-length: 12\r\nkeep-alive"
        );
    }

    #[test]
    fn parse_post_request() {
        let mut pool = crate::pool::Pool::with_capacity(1, 1, 16384);
        let mut buffer = pool.checkout().unwrap();
        buffer.write(
            b"POST /index.html HTTP/1.1\r\n\
          Host: localhost:8888\r\n\
          User-Agent: curl/7.43.0\r\n\
          Transfer-Encoding: chunked\r\n\
          Accept: */*\r\n\
          \r\n\
          4\r\n\
          Wiki\r\n\
          5\r\n\
          pedia\r\n\
          e\r\n \
          in\r\n\r\nchunks.\r\n\
          0\r\n\
          \r\n",
        );
        let (i, mut request) = request(buffer.data()).unwrap();
        println!("got request: {:#?}", request);

        println!("uri: {:?}", request.0.uri(buffer.data()));

        for header in request.1.iter_mut() {
            println!(
                "\t{:?} ({:?}) -> {:?}",
                header.name(buffer.data()),
                header.name.meta,
                header.value(buffer.data())
            );
            if header.name.meta == Meta::HeaderName(HeaderName::Host) {
                header.modify(&mut buffer, b"long.internal.server.name.com");
            }
        }

        println!("modified buffer\n{}", buffer.data().to_hex(16));

        assert_eq!(
            std::str::from_utf8(buffer.data()).unwrap(),
            "POST /index.html HTTP/1.1\r\n\
        Host: localhost:8888\r\n\
        User-Agent: curl/7.43.0\r\n\
        Transfer-Encoding: chunked\r\n\
        Accept: */*\r\n\
        \r\n\
        4\r\n\
        Wiki\r\n\
        5\r\n\
        pedia\r\n\
        e\r\n \
        in\r\n\r\nchunks.\r\n\
        0\r\n\
        \r\nlong.internal.server.name.com",
        );
    }
}
