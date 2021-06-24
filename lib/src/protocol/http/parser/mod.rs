#![allow(dead_code)]
use super::cookies::{RequestCookie, parse_request_cookies};

use nom::{
  IResult,Offset,Err,Needed,
  error::{Error, ErrorKind},
  character::{
    is_alphanumeric, is_space,
    streaming::{char, one_of},
    complete::digit1 as digit_complete
  },
  bytes::{
    self,
    streaming::{tag, take, take_while, take_while1},
    complete::{take_while1 as take_while1_complete}
  },
  sequence::{preceded, terminated, tuple},
  combinator::{opt, map_res}
};

use std::{fmt,str};
use std::str::from_utf8;
use std::convert::From;
use std::collections::HashSet;

mod request;
mod response;
#[cfg(test)]
mod tests;

pub use self::request::*;
pub use self::response::*;

pub fn compare_no_case(left: &[u8], right: &[u8]) -> bool {
  if left.len() != right.len() {
    return false;
  }

  left.iter().zip(right).all(|(a, b)| match (*a, *b) {
    (0..=64, 0..=64) | (91..=96, 91..=96) | (123..=255, 123..=255) => a == b,
    (65..=90, 65..=90) | (97..=122, 97..=122) | (65..=90, 97..=122) | (97..=122, 65..=90) => *a | 0b00_10_00_00 == *b | 0b00_10_00_00,
    _ => false
  })
}

// Primitives
fn is_token_char(i: u8) -> bool {
  is_alphanumeric(i) ||
  b"!#$%&'*+-.^_`|~".contains(&i)
}

fn token(i:&[u8]) -> IResult<&[u8], &[u8]> {
  take_while(is_token_char)(i)
}

fn is_status_token_char(i: u8) -> bool {
  i >= 32 && i != 127
}

fn status_token(i:&[u8]) -> IResult<&[u8], &[u8]> {
  take_while(is_status_token_char)(i)
}

fn is_ws(i: u8) -> bool {
  i == b' ' && i == b'\t'
}

fn sp(i:&[u8]) -> IResult<&[u8], char> {
  char(' ')(i)
}

fn crlf(i:&[u8]) -> IResult<&[u8], &[u8]> {
  tag("\r\n")(i)
}

fn is_vchar(i: u8) -> bool {
  i > 32 && i <= 126
}

// allows ISO-8859-1 characters in header values
// this is allowed in RFC 2616 but not in rfc7230
// cf https://github.com/sozu-proxy/sozu/issues/479
#[cfg(feature = "tolerant-http1-parser")]
fn is_header_value_char(i: u8) -> bool {
  i == 9 || (i >= 32 && i <= 126) || i >= 160
}

#[cfg(not(feature = "tolerant-http1-parser"))]
fn is_header_value_char(i: u8) -> bool {
  i == 9 || (i >= 32 && i <= 126)
}

fn vchar_1(i:&[u8]) -> IResult<&[u8], &[u8]> {
  take_while(is_vchar)(i)
}

#[derive(PartialEq,Debug,Clone)]
pub enum Method {
  Get,
  Post,
  Head,
  Options,
  Put,
  Delete,
  Trace,
  Connect,
  Custom(String),
}

impl Method {
  pub fn new(s: &[u8]) -> Method {
    if compare_no_case(&s, b"GET") {
      Method::Get
    } else if compare_no_case(&s, b"POST") {
      Method::Post
    } else if compare_no_case(&s, b"HEAD") {
      Method::Head
    } else if compare_no_case(&s, b"OPTIONS") {
      Method::Options
    } else if compare_no_case(&s, b"PUT") {
      Method::Put
    } else if compare_no_case(&s, b"DELETE") {
      Method::Delete
    } else if compare_no_case(&s, b"TRACE") {
      Method::Trace
    } else if compare_no_case(&s, b"CONNECT") {
      Method::Connect
    } else {
      Method::Custom(String::from(unsafe { str::from_utf8_unchecked(s) }))
    }
  }
}

impl fmt::Display for Method {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    match self {
     Method::Get => write!(f, "GET"),
     Method::Post => write!(f, "POST"),
     Method::Head => write!(f, "HEAD"),
     Method::Options => write!(f, "OPTIONS"),
     Method::Put => write!(f, "PUT"),
     Method::Delete => write!(f, "DELETE"),
     Method::Trace => write!(f, "TRACE"),
     Method::Connect => write!(f, "CONNECT"),
     Method::Custom(s)  => write!(f, "{}", s),
    }
  }
}

#[derive(PartialEq,Debug,Clone,Copy)]
pub enum Version {
  V10,
  V11,
}

#[derive(PartialEq,Debug)]
pub struct RequestLine<'a> {
    pub method: &'a [u8],
    pub uri: &'a [u8],
    pub version: Version
}

#[derive(PartialEq,Debug,Clone)]
pub struct RRequestLine {
    pub method: Method,
    pub uri: String,
    pub version: Version
}

impl RRequestLine {
  pub fn from_request_line(r: RequestLine) -> Option<RRequestLine> {
    if let Ok(uri) = str::from_utf8(r.uri) {
      Some(RRequestLine {
        method:  Method::new(r.method),
        uri:     String::from(uri),
        version: r.version
      })
    } else {
      None
    }
  }
}

#[derive(PartialEq,Debug)]
pub struct StatusLine<'a> {
    pub version: Version,
    pub status: &'a [u8],
    pub reason: &'a [u8],
}

#[derive(PartialEq,Debug,Clone)]
pub struct RStatusLine {
    pub version: Version,
    pub status:  u16,
    pub reason:  String,
}

impl RStatusLine {
  pub fn from_status_line(r: StatusLine) -> Option<RStatusLine> {
    if let Ok(status_str) = str::from_utf8(r.status) {
      if let Ok(status) = status_str.parse::<u16>() {
        if let Ok(reason) = str::from_utf8(r.reason) {
          Some(RStatusLine {
            version: r.version,
            status,
            reason:  String::from(reason),
          })
        } else {
          None
        }
      } else {
        None
      }
    } else {
      None
    }
  }
}

fn http_version(i:&[u8]) -> IResult<&[u8], Version> {
  let (i, _) = tag("HTTP/1.")(i)?;
  let (i, minor) = one_of("01")(i)?;

  Ok((i, if minor == '0' {
    Version::V10
  } else {
    Version::V11
  }))
}

fn request_line(i:&[u8]) -> IResult<&[u8], RequestLine> {
  let (i, method) = token(i)?;
  let (i, _) = sp(i)?;
  let (i, uri) = vchar_1(i)?; // ToDo proper URI parsing?
  let (i, _) = sp(i)?;
  let (i, version) = http_version(i)?;
  let (i, _) = crlf(i)?;

  Ok((i, RequestLine {
    method: method,
    uri: uri,
    version: version
  }))
}

fn status_line(i:&[u8]) -> IResult<&[u8], StatusLine> {
  let (i, (version, _, status, _, reason, _)) =
    tuple((http_version, sp, take(3usize), sp, status_token, crlf))(i)?;

  Ok((i, StatusLine {
    version: version,
    status: status,
    reason: reason,
  }))
}

#[derive(PartialEq,Debug)]
pub struct Header<'a> {
    pub name: &'a [u8],
    pub value: &'a [u8]
}

fn message_header(i:&[u8]) -> IResult<&[u8], Header> {
  // ToDo handle folding?
  let (i, (name, _, _, value, _)) =
    tuple((token, tag(":"), take_while(is_space), take_while(is_header_value_char), crlf))(i)?;

  Ok((i, Header {
    name: name,
    value: value
  }))
}

//not a space nor a comma
//
// allows ISO-8859-1 characters in header values
// this is allowed in RFC 2616 but not in rfc7230
// cf https://github.com/sozu-proxy/sozu/issues/479
#[cfg(feature = "tolerant-http1-parser")]
fn is_single_header_value_char(i: u8) -> bool {
  (i > 33 && i <= 126 && i != 44) || i >= 160
}

//not a space nor a comma
#[cfg(not(feature = "tolerant-http1-parser"))]
fn is_single_header_value_char(i: u8) -> bool {
  i > 33 && i <= 126 && i != 44
}

fn single_header_value(i:&[u8]) -> IResult<&[u8], &[u8]> {
  take_while1_complete(is_single_header_value_char)(i)
}

// Content-Disposition header, cf https://tools.ietf.org/html/rfc6266#section-4
pub fn content_disposition_header_value(i: &[u8]) -> IResult<&[u8], &[u8]> {
    //println!("header value:{}", String::from_utf8_lossy(i));
    let (i, _) = alt!(i, tag_no_case!("inline") | tag_no_case!("attachment") | token)?;
    let (i, o) = recognize!(i, many0!(content_disposition_parm))?;

    Ok((i, o))
}

pub fn content_disposition_parm(i: &[u8]) -> IResult<&[u8], &[u8]> {
    let (i, _) = opt!(i, take_while!(is_space))?;
    let (i, _) = tag!(i, ";")?;
    let (i, _) = opt!(i, take_while!(is_space))?;

    let (i, o) = recognize!(i, alt!(
            content_disposition_filename_parm |
            content_disposition_filename_star_parm |
            content_disposition_ext_parm1 |
            content_disposition_ext_parm2
        ))?;

    Ok((i, o))
}

pub fn content_disposition_filename_parm(i: &[u8]) -> IResult<&[u8], &[u8]> {
    let (i, _) = tag_no_case!(i, "filename")?;
    let (i, _) = opt!(i, take_while!(is_space))?;
    let (i, _) = tag!(i, "=")?;
    let (i, _) = opt!(i, take_while!(is_space))?;
    let (i, o) = content_disposition_value(i)?;

    //println!("recognized filename: {:?}", from_utf8(o));
    Ok((i, o))
}

pub fn content_disposition_filename_star_parm(i: &[u8]) -> IResult<&[u8], &[u8]> {
    let (i, _) = tag_no_case!(i, "filename*")?;
    let (i, _) = opt!(i, take_while!(is_space))?;
    let (i, _) = tag!(i, "=")?;
    let (i, _) = opt!(i, take_while!(is_space))?;

    content_disposition_ext_value(i)
}

pub fn content_disposition_ext_parm1(i: &[u8]) -> IResult<&[u8], &[u8]> {
    let (i, _) = token(i)?;
    let (i, _) = opt!(i, take_while!(is_space))?;
    let (i, _) = tag!(i, "=")?;
    let (i, _) = opt!(i, take_while!(is_space))?;
    content_disposition_value(i)
}

pub fn content_disposition_ext_parm2(i: &[u8]) -> IResult<&[u8], &[u8]> {
    let (i, _) = token(i)?;
    let (i, _) = tag!(i, "*")?;
    let (i, _) = opt!(i, take_while!(is_space))?;
    let (i, _) = tag!(i, "=")?;
    let (i, _) = opt!(i, take_while!(is_space))?;
    content_disposition_ext_value(i)
}

pub fn content_disposition_value(i: &[u8]) -> IResult<&[u8], &[u8]> {
    //println!("will parse:{}", String::from_utf8_lossy(i));
    let (i, o) = recognize!(i, alt!(
              // FIXME: escaping
              delimited!(char!('"'), is_not!("\""), char!('"')) |
              token
            ))?;

    //println!("parsed:{}", String::from_utf8_lossy(o));
    Ok((i, o))
}

// cf https://tools.ietf.org/html/rfc5987
// this is not compliant but should be good enough: we don't need the value, we
// just want to transmit the header
pub fn content_disposition_ext_value(i: &[u8]) -> IResult<&[u8], &[u8]> {
    //println!("will parse ext-value:{}", String::from_utf8_lossy(i));

    let (i, o) = recognize!(i,
        preceded!(
            alt!(tag_no_case!("UTF-8") | tag_no_case!("ISO-8859-1")),
            preceded!(
                delimited!(tag!("'"), is_not!("'"), tag!("'")),
                take_while!(|c| {
                    is_alphanumeric(c) ||
                    "!#$&+-.^_`|~".contains(c as char) ||
                    //FIXME: percent encoding
                    // we should also check that the two next chars are hex
                    c as char== '%'
                })
            )
        ))?;

    Ok((i, o))
}

#[cfg(feature = "tolerant-http1-parser")]
fn is_hostname_char(i: u8) -> bool {
  is_alphanumeric(i) ||
  // the domain name should not start with a hyphen or dot
  // but is it important here, since we will match this to
  // the list of accepted applications?
  // BTW each label between dots has a max of 63 chars,
  // and the whole domain shuld not be larger than 253 chars
  //
  // this tolerant parser also allows underscore, which is wrong
  // in domain names but accepted by some proxies and web servers
  // see https://github.com/sozu-proxy/sozu/issues/480
  b"-._".contains(&i)
}

#[cfg(not(feature = "tolerant-http1-parser"))]
fn is_hostname_char(i: u8) -> bool {
  is_alphanumeric(i) ||
  // the domain name should not start with a hyphen or dot
  // but is it important here, since we will match this to
  // the list of accepted applications?
  // BTW each label between dots has a max of 63 chars,
  // and the whole domain shuld not be larger than 253 chars
  b"-.".contains(&i)
}

// FIXME: convert port to u16 here
pub fn hostname_and_port(i: &[u8]) -> IResult<&[u8], (&[u8], Option<&[u8]>)> {
  let (i, host) = take_while1_complete(is_hostname_char)(i)?;
  let (i, port) = opt(preceded(bytes::complete::tag(":"), digit_complete))(i)?;

  if !i.is_empty() {
    Err(Err::Error(Error::new(i, ErrorKind::Eof)))
  } else {
    Ok((i, (host, port)))
  }
}

pub fn is_hex_digit(chr: u8) -> bool {
  (chr >= 0x30 && chr <= 0x39) ||
  (chr >= 0x41 && chr <= 0x46) ||
  (chr >= 0x61 && chr <= 0x66)
}
pub fn chunk_size(input: &[u8]) -> IResult<&[u8], usize> {
  let (i, s) = map_res(take_while(is_hex_digit), from_utf8)(input)?;
  if i.is_empty() {
    return Err(Err::Incomplete(Needed::Unknown));
  }
  match usize::from_str_radix(s, 16) {
    Ok(sz) => Ok((i, sz)),
    Err(_) => Err(Err::Error(error_position!(input, ::nom::error::ErrorKind::MapRes)))
  }
}

fn chunk_header(i: &[u8]) -> IResult<&[u8], usize> {
  terminated(chunk_size, crlf)(i)
}

fn end_of_chunk_and_header(i: &[u8]) -> IResult<&[u8], usize> {
  preceded(crlf, chunk_header)(i)
}

fn trailer_line(i: &[u8]) -> IResult<&[u8], &[u8]> {
  terminated(take_while1(is_header_value_char), crlf)(i)
}

#[derive(PartialEq,Debug,Clone,Copy)]
pub enum Chunk {
  Initial,
  Copying,
  CopyingLastHeader,
  Ended,
  Error
}

impl Chunk {
  pub fn should_copy(&self) -> bool {
    Chunk::Copying == *self
  }

  pub fn should_parse(&self) -> bool {
    match *self {
      Chunk::Initial | Chunk::Copying | Chunk::CopyingLastHeader => true,
      _                                                          => false
    }
  }

  pub fn has_ended(&self) -> bool {
    *self == Chunk::Ended
  }

  pub fn is_error(&self) -> bool {
    *self == Chunk::Error
  }

  // FIXME: probably inefficient, since we don't parse again until the previous chunk was sent
  // it should be possible to parse the next header from a specific position like parse_*_until_stop
  // and return the biggest copying size
  pub fn parse_one(&self, buf: &[u8]) -> (usize, Chunk) {
    match *self {
      // we parse the first header, and advance the position to the end of chunk
      Chunk::Initial => {
        match chunk_header(buf) {
          Ok((i, sz)) => {
            if sz == 0 {
              // size of header + 0 data
              (buf.offset(i), Chunk::CopyingLastHeader)
            } else {
              // size of header + size of data
              (buf.offset(i) + sz, Chunk::Copying)
            }
          },
          Err(Err::Incomplete(_)) => (0, Chunk::Initial),
          Err(_)     => (0, Chunk::Error)
        }
      },
      // we parse a crlf then a header, and advance the position to the end of chunk
      Chunk::Copying => {
        match end_of_chunk_and_header(buf) {
          Ok((i, sz_str)) => {
            let sz = usize::from(sz_str);
            if sz == 0 {
              // data to copy + size of header + 0 data
              (buf.offset(i), Chunk::CopyingLastHeader)
            } else {
              // data to copy + size of header + size of next chunk
              (buf.offset(i)+sz, Chunk::Copying)
            }
          },
          Err(Err::Incomplete(_)) => (0, Chunk::Copying),
          Err(_) => (0, Chunk::Error)
        }
      },
      // we parse a crlf then stop
      Chunk::CopyingLastHeader => {
        match crlf(buf) {
          Ok((i, _)) => {
            (buf.offset(i), Chunk::Ended)
          },
          Err(Err::Incomplete(_)) => (0, Chunk::CopyingLastHeader),
          Err(_) => (0, Chunk::Error)
        }
      },
      _ => { (0, Chunk::Error) }
    }
  }

  //pub fn parse
  pub fn parse(&self, buf: &[u8]) -> (BufferMove, Chunk) {
    let mut current_state = *self;
    let mut position      = 0;
    let length            = buf.len();
    loop {
      let (mv, new_state) = current_state.parse_one(&buf[position..]);
      current_state = new_state;
      position += mv;
      if mv == 0 {
        break;
      }

      match current_state {
        Chunk::Ended | Chunk::Error => {
          break;
        },
        _ => {}
      }

      if position >= length {
        break;
      }

    }

    match position {
      0  => (BufferMove::None, current_state),
      sz => (BufferMove::Advance(sz), current_state)
    }
  }
}

#[derive(PartialEq,Debug)]
pub enum TransferEncodingValue {
  Chunked,
  Compress,
  Deflate,
  Gzip,
  Identity,
  Unknown
}

#[derive(PartialEq,Debug)]
pub struct ConnectionValue {
  pub has_close: bool,
  pub has_keep_alive: bool,
  pub has_upgrade: bool,
  pub to_delete: Option<HashSet<Vec<u8>>>,
}

#[derive(PartialEq,Debug)]
pub enum HeaderResult<T> {
  Value(T),
  None,
  Error
}

impl<'a> Header<'a> {
  pub fn value(&self) -> HeaderValue {
    if compare_no_case(self.name, b"host") {
      //FIXME: UTF8 conversion should be unchecked here, since we already checked the tokens?
      if let Some(s) = str::from_utf8(self.value).map(String::from).ok() {
        HeaderValue::Host(s)
      } else {
        HeaderValue::Error
      }
    } else if compare_no_case(self.name, b"content-length") {
      if let Ok(l) = str::from_utf8(self.value) {
        if let Some(length) = l.parse().ok() {
           return HeaderValue::ContentLength(length)
        }
      }
      HeaderValue::Error
    } else if compare_no_case(self.name, b"transfer-encoding") {
      if compare_no_case(&self.value, b"chunked") {
        HeaderValue::Encoding(TransferEncodingValue::Chunked)
      } else if compare_no_case(&self.value, b"compress") {
        HeaderValue::Encoding(TransferEncodingValue::Compress)
      } else if compare_no_case(&self.value, b"deflate") {
        HeaderValue::Encoding(TransferEncodingValue::Deflate)
      } else if compare_no_case(&self.value, b"gzip") {
        HeaderValue::Encoding(TransferEncodingValue::Gzip)
      } else if compare_no_case(&self.value, b"identity") {
        HeaderValue::Encoding(TransferEncodingValue::Identity)
      } else {
        HeaderValue::Encoding(TransferEncodingValue::Unknown)
      }
    } else if compare_no_case(self.name, b"connection") {
      let mut has_close = false;
      let mut has_upgrade = false;
      let mut has_keep_alive = false;
      let mut to_delete = None;

      match single_header_value(self.value) {
        Ok((mut input, first)) => {
          if compare_no_case(first, b"upgrade") {
            has_upgrade = true;
          } else if compare_no_case(first, b"close") {
            has_close = true;
          } else if compare_no_case(first, b"keep-alive") {
            has_keep_alive = true;
          } else {
            if to_delete.is_none() {
              to_delete = Some(HashSet::new());
            }

            to_delete.as_mut().map(|h| h.insert(Vec::from(first)));
          }

          while input.len() != 0 {
            match do_parse!(input,
              opt!(complete!(sp)) >>
              complete!(char!(',')) >>
              opt!(sp) >>
              v: single_header_value >> (v)
            ) {
              Ok((i, v)) => {
                if compare_no_case(v, b"upgrade") {
                  has_upgrade = true;
                } else if compare_no_case(v, b"close") {
                  has_close = true;
                } else if compare_no_case(v, b"keep-alive") {
                  has_keep_alive = true;
                } else {
                  if to_delete.is_none() {
                    to_delete = Some(HashSet::new());
                  }

                  to_delete.as_mut().map(|h| h.insert(Vec::from(v)));
                }

                input = i;
              },
              Err(_) => {
                return HeaderValue::Error;
              }
            }
          }
          let r = ConnectionValue {
            has_close, has_keep_alive, has_upgrade, to_delete
          };
          //println!("returning: {:?}", r);
          HeaderValue::Connection(r)
        },
        Err(_) => HeaderValue::Error
      }
    } else if compare_no_case(self.name, b"upgrade") {
      HeaderValue::Upgrade(self.value)
    } else if compare_no_case(self.name, b"forwarded") {
      HeaderValue::Forwarded(self.value)
    } else if compare_no_case(self.name, b"x-forwarded-for") {
      HeaderValue::XForwardedFor(self.value)
    } else if compare_no_case(self.name, b"x-forwarded-proto") {
      //FIXME: should parse the header properly
      HeaderValue::XForwardedProto
    } else if compare_no_case(self.name, b"x-forwarded-port") {
      //FIXME: should parse the header properly
      HeaderValue::XForwardedPort
    } else if compare_no_case(self.name, b"expect") {
      if compare_no_case(self.value, b"100-continue") {
        HeaderValue::ExpectContinue
      } else {
        HeaderValue::Error
      }
    } else if compare_no_case(self.name, b"cookie") {
      match parse_request_cookies(self.value) {
        Some(cookies) => HeaderValue::Cookie(cookies),
        None          => HeaderValue::Error
      }
    } else {
      HeaderValue::Other(self.name, self.value)
    }
  }

  pub fn should_delete(&self, conn: &Connection, sticky_name: &str) -> bool {
    //FIXME: we should delete this header anyway, and add a Connection: Upgrade if we detected an upgrade
    if compare_no_case(&self.name, b"connection") {
      match single_header_value(self.value) {
        Ok((mut input, first)) => {
          if compare_no_case(first, b"upgrade") {
            false
          } else {
            while input.len() != 0 {
              match do_parse!(input,
                opt!(complete!(sp)) >>
                complete!(char!(',')) >>
                opt!(sp) >>
                v: single_header_value >> (v)
              ) {
                Ok((i, v)) => {
                  if compare_no_case(v, b"upgrade") {
                    return false;
                  }
                  input = i;
                },
                Err(_) => {
                  return true;
                }
              }
            }
            true

          }
        },
        Err(_) => true
      }
    } else if compare_no_case(&self.name, b"set-cookie") {
      self.value.starts_with(sticky_name.as_bytes())
    } else {
      let b = (compare_no_case(&self.name, b"connection") &&
                   !compare_no_case(&self.value, b"upgrade")) ||
                    compare_no_case(&self.name, b"sozu-id")   ||
      {
        let mut res = false;
        if let Some(ref to_delete) = conn.to_delete {
          for ref header_value in to_delete {
            if compare_no_case(&self.value, &header_value) {
              res = true;
              break;
            }
          }
        }

        res
      };

      if compare_no_case(&self.name, b"x-forwarded-proto") ||
          compare_no_case(&self.name, b"x-forwarded-host") ||
          compare_no_case(&self.name, b"x-forwarded-port") {

        return false;
      }

      if compare_no_case(&self.name, b"x-forwarded-for") ||
          compare_no_case(&self.name, b"forwarded") {
              return true;
      }

      b
    }
  }

  pub fn must_mutate(&self) -> bool {
    compare_no_case(&self.name, b"cookie")
  }

  pub fn mutate_header(&self, buf: &[u8], offset: usize, sticky_name: &str) -> Vec<BufferMove> {
    if compare_no_case(&self.name, b"cookie") {
      self.remove_sticky_cookie_in_request(buf, offset, sticky_name)
    } else {
      vec![BufferMove::Advance(offset)]
    }
  }

  pub fn remove_sticky_cookie_in_request(&self, buf: &[u8], offset: usize, sticky_name: &str) -> Vec<BufferMove> {
    if let Some(cookies) = parse_request_cookies(self.value) {
      // if we don't find the cookie, don't go further
      if let Some(sozu_balance_position) = cookies.iter().position(|cookie| &cookie.name[..] == sticky_name.as_bytes()) {
        // If we have only one cookie and that's the one, then we drop the whole header
        if cookies.len() == 1 {
          return vec![BufferMove::Delete(offset)];
        }
        // we want to advance the buffer for the header's name
        // +1 is to count ":"
        let header_length = self.name.len() + 1;

        fn take_space(i: &[u8]) -> IResult<&[u8], &[u8]> {
          take_while(is_space)(i)
        }
        // we calculate how much chars there is between the : and the first cookie
        let length_until_value = match take_space(&buf[header_length..buf.len()]) {
          Ok((_, spaces)) => spaces,
          Err(_) => {
            // if there is not enough data or an error, we completely remove the header.
            return vec![BufferMove::Advance(offset)];
          }
        };

        // Our iterator over the cookies
        let mut iter = cookies.iter();
        // Our return value
        let mut moves = Vec::new();
        // The current number of cookie parsed
        let mut current_cookie = 0;
        // If the cookie SOZUBALANCEID is the last of the cookie chain
        let sozu_balance_is_last = (sozu_balance_position + 1) == cookies.len();

        moves.push(BufferMove::Advance(header_length + length_until_value.len()));

        loop {
          match iter.next() {
            Some(cookie) => {
              let cookie_length = cookie.get_full_length();
              // We already know the position of the cookie in the chain, so we avoid
              // a string comparision and directly check against where we are in the cookies
              if current_cookie == sozu_balance_position {
                moves.push(BufferMove::Delete(cookie_length));
              } else if sozu_balance_is_last {
                // if sozublanceid is the last element, we want to delete the "; " chars from
                // the before last cookie
                if (current_cookie + 1) == sozu_balance_position {
                  let spaces = cookie.spaces.len();
                  // This one is obvious but I prefer to name the value
                  let semicolon = 1;
                  moves.push(BufferMove::Advance(cookie_length - spaces - semicolon));
                  // We directly do the Delete here to avoid keeping context of 'did the cookie
                  // before had spaces ?'
                  moves.push(BufferMove::Delete(semicolon + spaces));
                } else {
                  moves.push(BufferMove::Advance(cookie_length));
                }
              } else {
                moves.push(BufferMove::Advance(cookie_length));
              }

              current_cookie += 1;
            },
            None => {
              moves.push(BufferMove::Advance(2)); // advance of 2 for the header's \r\n
              return moves;
            }
          }
        }
      }
    }

    vec![BufferMove::Advance(offset)]
  }
}

#[derive(Debug,Clone,Copy,PartialEq)]
pub enum ForwardedProtocol {
  HTTP,
  HTTPS
}

impl Default for ForwardedProtocol {
    fn default() -> Self {
        ForwardedProtocol::HTTP
    }
}

pub enum HeaderValue<'a> {
  Host(String),
  ContentLength(usize),
  Encoding(TransferEncodingValue),
  //FIXME: are the references in Connection still valid after we delete that part of the headers?
  Connection(ConnectionValue),
  Upgrade(&'a[u8]),
  Cookie(Vec<RequestCookie<'a>>),
  Other(&'a[u8],&'a[u8]),
  ExpectContinue,
  Forwarded(&'a[u8]),
  XForwardedFor(&'a[u8]),
  XForwardedProto,
  XForwardedPort,
  Error
}

pub type Host = String;

#[derive(Debug,Clone,PartialEq)]
pub enum LengthInformation {
  Length(usize),
  Chunked,
  //Compressed
}

#[derive(Debug,Clone,Copy,PartialEq)]
pub enum Continue {
  None,
  Expects(usize),
}
/*
#[derive(Debug,Clone,PartialEq)]
pub enum Connection {
  KeepAlive,
  Close,
  Upgrade,
}
*/

#[derive(Debug,Clone,PartialEq,Default)]
pub struct ForwardedHeaders {
    pub x_proto: bool,
    pub x_host: bool,
    pub x_port: bool,
    pub x_for: Option<String>,
    pub forwarded: Option<String>,
}

#[derive(Debug,Clone,PartialEq)]
pub struct Connection {
  pub keep_alive:     Option<bool>,
  pub has_upgrade:    bool,
  pub upgrade:        Option<String>,
  pub to_delete:      Option<HashSet<Vec<u8>>>,
  pub continues:      Continue,
  pub sticky_session: Option<String>,
  pub forwarded:      ForwardedHeaders,
}

impl Connection {
  pub fn new() -> Connection {
    Connection {
      keep_alive:     None,
      has_upgrade:    false,
      upgrade:        None,
      continues:      Continue::None,
      to_delete:      None,
      sticky_session: None,
      forwarded:      ForwardedHeaders::default(),
    }
  }

  pub fn keep_alive() -> Connection {
    Connection {
      keep_alive:     Some(true),
      has_upgrade:    false,
      upgrade:        None,
      continues:      Continue::None,
      to_delete:      None,
      sticky_session: None,
      forwarded:      ForwardedHeaders::default(),
    }
  }

  pub fn close() -> Connection {
    Connection {
      keep_alive:     Some(false),
      has_upgrade:    false,
      upgrade:        None,
      continues:      Continue::None,
      to_delete:      None,
      sticky_session: None,
      forwarded:      ForwardedHeaders::default(),
    }
  }
}


pub type HeaderEndPosition = Option<usize>;

#[derive(Debug,PartialEq)]
pub enum BufferMove {
  None,
  /// length
  Advance(usize),
  /// length
  Delete(usize),
  /// Vec of BufferMove operations
  Multiple(Vec<BufferMove>)
}
