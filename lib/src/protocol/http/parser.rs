use sozu_command::buffer::Buffer;
use buffer_queue::BufferQueue;
use protocol::StickySession;
use super::cookies::{RequestCookie, parse_request_cookies};
use features::FEATURES;

use nom::{HexDisplay,IResult,Offset};

use nom::{AsChar, character::{is_alphanumeric, is_space}};

use url::Url;

use std::{fmt,str};
use std::convert::From;
use std::collections::HashSet;

pub fn compare_no_case(left: &[u8], right: &[u8]) -> bool {
  if left.len() != right.len() {
    return false;
  }

  left.iter().zip(right).all(|(a, b)| match (*a, *b) {
    (0...64, 0...64) | (91...96, 91...96) | (123...255, 123...255) => a == b,
    (65...90, 65...90) | (97...122, 97...122) | (65...90, 97...122) | (97...122, 65...90) => *a | 0b00_10_00_00 == *b | 0b00_10_00_00,
    _ => false
  })
}

// Primitives
fn is_token_char(i: u8) -> bool {
  is_alphanumeric(i) ||
  b"!#$%&'*+-.^_`|~".contains(&i)
}
named!(pub token, take_while!(is_token_char));

fn is_status_token_char(i: u8) -> bool {
  i >= 32 && i != 127
}

named!(pub status_token, take_while!(is_status_token_char));
named!(pub sp<char>, char!(' '));
named!(pub crlf, tag!("\r\n"));

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

named!(pub vchar_1, take_while!(is_vchar));
named!(digit_complete, take_while1_complete!(|item:u8| item.is_dec_digit()));

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

named!(pub http_version<Version>,
do_parse!(
  tag!("HTTP/") >>
  tag!("1.") >>
  minor: one_of!("01") >> (
    if minor == '0' {
      Version::V10
    } else {
      Version::V11
    }
  )
)
);

named!(pub request_line<RequestLine>,
do_parse!(
  method: token >>
  sp >>
  uri: vchar_1 >> // ToDo proper URI parsing?
  sp >>
  version: http_version >>
  crlf >> (
    RequestLine {
      method: method,
      uri: uri,
      version: version
    }
  )
)
);

named!(pub status_line<StatusLine>,
do_parse!(
  version: http_version >>
           sp           >>
  status:  take!(3)     >>
           sp           >>
  reason:  status_token >>
           crlf         >>
  (StatusLine {
    version: version,
    status: status,
    reason: reason,
  })
)
);

#[derive(PartialEq,Debug)]
pub struct Header<'a> {
    pub name: &'a [u8],
    pub value: &'a [u8]
}

named!(pub message_header<Header>,
do_parse!(
  name: token >>
  tag!(":")   >>
  opt!(take_while!(is_space))    >>
  value: take_while!(is_header_value_char) >> // ToDo handle folding?
  crlf >> (
    Header {
      name: name,
      value: value
    }
  )
)
);

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

named!(pub single_header_value, take_while1_complete!(is_single_header_value_char));

pub fn comma_separated_header_values(input:&[u8]) -> Option<Vec<&[u8]>> {
  let res: IResult<&[u8], Vec<&[u8]>> =
    separated_list!(input,
      delimited!(
        opt!(complete!(sp)),
        complete!(char!(',')),
        opt!(sp)
      ),
      single_header_value
    );
  if let Ok((_,o)) = res {
    Some(o)
  } else {
    None
  }
}

named!(pub headers< Vec<Header> >, terminated!(many0!(message_header), opt!(crlf)));

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

named!(pub hostname_and_port<(&[u8],Option<&[u8]>)>,
  terminated!(
    pair!(
      take_while1_complete!(is_hostname_char),
      opt!(complete!(preceded!(
        tag!(":"),
        digit_complete
      )))
    ),
    empty!()
  )
);

use std::str::from_utf8;
use nom::{Err,Needed};

pub fn is_hex_digit(chr: u8) -> bool {
  (chr >= 0x30 && chr <= 0x39) ||
  (chr >= 0x41 && chr <= 0x46) ||
  (chr >= 0x61 && chr <= 0x66)
}
pub fn chunk_size(input: &[u8]) -> IResult<&[u8], usize> {
  let (i, s) = try_parse!(input, map_res!(take_while!(is_hex_digit), from_utf8));
  if i.is_empty() {
    return Err(Err::Incomplete(Needed::Unknown));
  }
  match usize::from_str_radix(s, 16) {
    Ok(sz) => Ok((i, sz)),
    Err(_) => Err(Err::Error(error_position!(input, ::nom::error::ErrorKind::MapRes)))
  }
}

named!(pub chunk_header<usize>, terminated!(chunk_size, crlf));
named!(pub end_of_chunk_and_header<usize>, preceded!(crlf, chunk_header));

named!(pub trailer_line, terminated!(take_while1!(is_header_value_char), crlf));

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
    } else if compare_no_case(self.name, b"forwarded")   ||
        compare_no_case(self.name, b"x-forwarded-for")   ||
        compare_no_case(self.name, b"x-forwarded-proto") ||
        compare_no_case(self.name, b"x-forwarded-port") {
      HeaderValue::Forwarded
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
      let mut b = (compare_no_case(&self.name, b"connection") && !compare_no_case(&self.value, b"upgrade")) ||
      compare_no_case(&self.name, b"sozu-id")           ||
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

      if !FEATURES.with(|features| features.borrow().get("forwarded-fix").map(|f| f.is_true()).unwrap_or(false)) {
        b |= compare_no_case(&self.name, b"forwarded")         ||
             compare_no_case(&self.name, b"x-forwarded-for")   ||
             compare_no_case(&self.name, b"x-forwarded-proto") ||
             compare_no_case(&self.name, b"x-forwarded-port");
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
        // we calculate how much chars there is between the : and the first cookie
        let res: IResult<_,_> = take_while!(&buf[header_length..buf.len()], is_space);
        let length_until_value = match res {
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

pub enum ForwardedProtocol {
  HTTP,
  HTTPS
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
  Forwarded,
  ExpectContinue,
  /*
  Forwarded(Vec<&'a[u8]>),
  XForwardedFor(Vec<&'a[u8]>),
  XForwardedProto(ForwardedProtocol),
  XForwardedPort(u16),
  */
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


#[derive(Debug,Clone,PartialEq)]
pub struct Connection {
  pub keep_alive:     Option<bool>,
  pub has_upgrade:    bool,
  pub upgrade:        Option<String>,
  pub to_delete:      Option<HashSet<Vec<u8>>>,
  pub continues:      Continue,
  pub sticky_session: Option<String>,
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
    }
  }

  pub fn close() -> Connection {
    Connection {
      keep_alive:     Some(false),
      has_upgrade:    false,
      upgrade:        None,
      continues:      Continue::None,
      to_delete:      None,
      sticky_session: None
    }
  }
}

#[derive(Debug,Clone,PartialEq)]
pub enum RequestState {
  Initial,
  Error(Option<RRequestLine>, Option<Connection>, Option<Host>, Option<LengthInformation>, Option<Chunk>),
  HasRequestLine(RRequestLine, Connection),
  HasHost(RRequestLine, Connection, Host),
  HasLength(RRequestLine, Connection, LengthInformation),
  HasHostAndLength(RRequestLine, Connection, Host, LengthInformation),
  Request(RRequestLine, Connection, Host),
  RequestWithBody(RRequestLine, Connection, Host, usize),
  RequestWithBodyChunks(RRequestLine, Connection, Host, Chunk),
}

impl RequestState {
  pub fn into_error(self) -> RequestState {
    match self {
      RequestState::Initial => RequestState::Error(None, None, None, None, None),
      RequestState::HasRequestLine(rl, conn) => RequestState::Error(Some(rl), Some(conn), None, None, None),
      RequestState::HasHost(rl, conn, host)  => RequestState::Error(Some(rl), Some(conn), Some(host), None, None),
      RequestState::HasHostAndLength(rl, conn, host, len)  => RequestState::Error(Some(rl), Some(conn), Some(host), Some(len), None),
      RequestState::Request(rl, conn, host)  => RequestState::Error(Some(rl), Some(conn), Some(host), None, None),
      RequestState::RequestWithBody(rl, conn, host, len) => RequestState::Error(Some(rl), Some(conn), Some(host), Some(LengthInformation::Length(len)), None),
      RequestState::RequestWithBodyChunks(rl, conn, host, chunk) => RequestState::Error(Some(rl), Some(conn), Some(host), None, Some(chunk)),
      err => err,
    }
  }

  pub fn is_front_error(&self) -> bool {
    if let RequestState::Error(_,_,_,_,_) = self {
      true
    } else {
      false
    }
  }

  pub fn get_sticky_session(&self) -> Option<&str> {
    self.get_keep_alive().and_then(|con| con.sticky_session.as_ref()).map(|s| s.as_str())
  }

  pub fn has_host(&self) -> bool {
    match *self {
      RequestState::HasHost(_, _, _)            |
      RequestState::HasHostAndLength(_, _, _, _)|
      RequestState::Request(_, _, _)            |
      RequestState::RequestWithBody(_, _, _, _) |
      RequestState::RequestWithBodyChunks(_, _, _, _) => true,
      _                                               => false
    }
  }

  pub fn is_proxying(&self) -> bool {
    match *self {
      RequestState::Request(_, _, _)            |
      RequestState::RequestWithBody(_, _, _, _) |
      RequestState::RequestWithBodyChunks(_, _, _, _)  => true,
      _                                                => false
    }
  }

  pub fn is_head(&self) -> bool {
    match *self {
      RequestState::Request(ref rl, _, _)            |
      RequestState::RequestWithBody(ref rl, _, _, _) |
      RequestState::RequestWithBodyChunks(ref rl, _, _, _) => {
        rl.method == Method::Head
      },
      _                                                => false
    }
  }

  pub fn get_host(&self) -> Option<&str> {
    match *self {
      RequestState::HasHost(_, _, ref host)             |
      RequestState::HasHostAndLength(_, _, ref host, _) |
      RequestState::Request(_, _, ref host)             |
      RequestState::RequestWithBody(_, _, ref host, _)  |
      RequestState::RequestWithBodyChunks(_, _, ref host, _) => Some(host.as_str()),
      RequestState::Error(_, _, ref host, _, _)              => host.as_ref().map(|s| s.as_str()),
      _                                                      => None
    }
  }

  pub fn get_uri(&self) -> Option<String> {
    match *self {
      RequestState::HasRequestLine(ref rl, _)         |
      RequestState::HasHost(ref rl, _, _)             |
      RequestState::HasHostAndLength(ref rl, _, _, _) |
      RequestState::Request(ref rl , _, _)            |
      RequestState::RequestWithBody(ref rl, _, _, _)  |
      RequestState::RequestWithBodyChunks(ref rl, _, _, _) => Some(rl.uri.clone()),
      RequestState::Error(ref rl, _, _, _, _)              => rl.as_ref().map(|r| r.uri.clone()),
      _                                                    => None
    }
  }

  pub fn get_request_line(&self) -> Option<&RRequestLine> {
    match *self {
      RequestState::HasRequestLine(ref rl, _)         |
      RequestState::HasHost(ref rl, _, _)             |
      RequestState::HasHostAndLength(ref rl, _, _, _) |
      RequestState::Request(ref rl, _, _)             |
      RequestState::RequestWithBody(ref rl, _, _, _)  |
      RequestState::RequestWithBodyChunks(ref rl, _, _, _) => Some(rl),
      RequestState::Error(ref rl, _, _, _, _) => rl.as_ref(),
      _ => None
    }
  }

  pub fn get_keep_alive(&self) -> Option<&Connection> {
    match *self {
      RequestState::HasRequestLine(_, ref conn)         |
      RequestState::HasHost(_, ref conn, _)             |
      RequestState::HasLength(_, ref conn, _)           |
      RequestState::HasHostAndLength(_, ref conn, _, _) |
      RequestState::Request(_, ref conn, _)             |
      RequestState::RequestWithBody(_, ref conn, _, _)  |
      RequestState::RequestWithBodyChunks(_, ref conn, _, _) => Some(conn),
      RequestState::Error(_, ref conn, _, _, _) => conn.as_ref(),
      _ => None
    }
  }

  pub fn get_mut_connection(&mut self) -> Option<&mut Connection> {
    match *self {
      RequestState::HasRequestLine(_, ref mut conn)         |
      RequestState::HasHost(_, ref mut conn, _)             |
      RequestState::HasLength(_, ref mut conn, _)           |
      RequestState::HasHostAndLength(_, ref mut conn, _, _) |
      RequestState::Request(_, ref mut conn, _)             |
      RequestState::RequestWithBody(_, ref mut conn, _, _)  |
      RequestState::RequestWithBodyChunks(_, ref mut conn, _, _) => Some(conn),
      _                                                      => None
    }

  }

  pub fn should_copy(&self, position: usize) -> Option<usize> {
    match *self {
      RequestState::RequestWithBody(_, _, _, l) => Some(position + l),
      RequestState::Request(_, _, _)            => Some(position),
      _                                         => None
    }
  }

  pub fn should_keep_alive(&self) -> bool {
    //FIXME: should not clone here
    let rl =  self.get_request_line();
    let version = rl.as_ref().map(|rl| rl.version);
    let conn = self.get_keep_alive();
    match (version, conn.map(|c| c.keep_alive)) {
      (_, Some(Some(true)))   => true,
      (_, Some(Some(false)))  => false,
      (Some(Version::V10), _) => false,
      (Some(Version::V11), _) => true,
      (_, _)                  => false,
    }
  }

  pub fn should_chunk(&self) -> bool {
    if let  RequestState::RequestWithBodyChunks(_, _, _, _) = *self {
      true
    } else {
      false
    }
  }
}

pub type UpgradeProtocol = String;

#[derive(Debug,Clone,PartialEq)]
pub enum ResponseState {
  Initial,
  Error(Option<RStatusLine>, Option<Connection>, Option<UpgradeProtocol>, Option<LengthInformation>, Option<Chunk>),
  HasStatusLine(RStatusLine, Connection),
  HasUpgrade(RStatusLine, Connection, UpgradeProtocol),
  HasLength(RStatusLine, Connection, LengthInformation),
  Response(RStatusLine, Connection),
  ResponseUpgrade(RStatusLine, Connection, UpgradeProtocol),
  ResponseWithBody(RStatusLine, Connection, usize),
  ResponseWithBodyChunks(RStatusLine, Connection, Chunk),
  // the boolean indicates if the backend connection is closed
  ResponseWithBodyCloseDelimited(RStatusLine, Connection, bool),
}

impl ResponseState {
  pub fn into_error(self) -> ResponseState {
    match self {
      ResponseState::Initial => ResponseState::Error(None, None, None, None, None),
      ResponseState::HasStatusLine(sl, conn) => ResponseState::Error(Some(sl), Some(conn), None, None, None),
      ResponseState::HasLength(sl, conn, length) => ResponseState::Error(Some(sl), Some(conn), None, Some(length), None),
      ResponseState::HasUpgrade(sl, conn, upgrade) => ResponseState::Error(Some(sl), Some(conn), Some(upgrade), None, None),
      ResponseState::Response(sl, conn) => ResponseState::Error(Some(sl), Some(conn), None, None, None),
      ResponseState::ResponseUpgrade(sl, conn, upgrade) => ResponseState::Error(Some(sl), Some(conn), Some(upgrade), None, None),
      ResponseState::ResponseWithBody(sl, conn, len) => ResponseState::Error(Some(sl), Some(conn), None, Some(LengthInformation::Length(len)), None),
      ResponseState::ResponseWithBodyChunks(sl, conn, chunk) => ResponseState::Error(Some(sl), Some(conn), None, None, Some(chunk)),
      ResponseState::ResponseWithBodyCloseDelimited(sl, conn, _) => ResponseState::Error(Some(sl), Some(conn), None, None, None),
      err => err
    }
  }

  pub fn is_proxying(&self) -> bool {
    match *self {
        ResponseState::Response(_, _)
      | ResponseState::ResponseWithBody(_, _, _)
      | ResponseState::ResponseWithBodyChunks(_, _, _)
      | ResponseState::ResponseWithBodyCloseDelimited(_, _, _)
        => true,
      _ => false
    }
  }

  pub fn is_back_error(&self) -> bool {
    if let ResponseState::Error(_,_,_,_,_) = self {
      true
    } else {
      false
    }
  }

  pub fn get_status_line(&self) -> Option<&RStatusLine> {
    match *self {
      ResponseState::HasStatusLine(ref sl, _)             |
      ResponseState::HasLength(ref sl, _, _)              |
      ResponseState::HasUpgrade(ref sl, _, _)             |
      ResponseState::Response(ref sl, _)                  |
      ResponseState::ResponseUpgrade(ref sl, _, _)        |
      ResponseState::ResponseWithBody(ref sl, _, _)       |
      ResponseState::ResponseWithBodyCloseDelimited(ref sl, _, _) |
      ResponseState::ResponseWithBodyChunks(ref sl, _, _) => Some(sl),
      ResponseState::Error(ref sl, _, _, _, _)            => sl.as_ref(),
      _                                                   => None
    }
  }

  pub fn get_keep_alive(&self) -> Option<Connection> {
    match *self {
      ResponseState::HasStatusLine(_, ref conn)             |
      ResponseState::HasLength(_, ref conn, _)              |
      ResponseState::HasUpgrade(_, ref conn, _)             |
      ResponseState::Response(_, ref conn)                  |
      ResponseState::ResponseUpgrade(_, ref conn, _)        |
      ResponseState::ResponseWithBody(_, ref conn, _)       |
      ResponseState::ResponseWithBodyCloseDelimited(_, ref conn, _) |
      ResponseState::ResponseWithBodyChunks(_, ref conn, _) => Some(conn.clone()),
      ResponseState::Error(_, ref conn, _, _, _)            => conn.clone(),
      _                                                     => None
    }
  }

  pub fn get_mut_connection(&mut self) -> Option<&mut Connection> {
    match *self {
      ResponseState::HasStatusLine(_, ref mut conn)             |
      ResponseState::HasLength(_, ref mut conn, _)              |
      ResponseState::HasUpgrade(_, ref mut conn, _)             |
      ResponseState::Response(_, ref mut conn)                  |
      ResponseState::ResponseUpgrade(_, ref mut conn, _)        |
      ResponseState::ResponseWithBody(_, ref mut conn, _)       |
      ResponseState::ResponseWithBodyCloseDelimited(_, ref mut conn, _) |
      ResponseState::ResponseWithBodyChunks(_, ref mut conn, _) => Some(conn),
      ResponseState::Error(_, ref mut conn, _, _, _)            => conn.as_mut(),
      _                                                     => None
    }
  }

  pub fn should_copy(&self, position: usize) -> Option<usize> {
    match *self {
      ResponseState::ResponseWithBody(_, _, l) => Some(position + l),
      ResponseState::Response(_, _)            => Some(position),
      _                                        => None
    }
  }

  pub fn should_keep_alive(&self) -> bool {
    //FIXME: should not clone here
    let sl      = self.get_status_line();
    let version = sl.as_ref().map(|sl| sl.version);
    let conn    = self.get_keep_alive();
    match (version, conn.map(|c| c.keep_alive)) {
      (_, Some(Some(true)))   => true,
      (_, Some(Some(false)))  => false,
      (Some(Version::V10), _) => false,
      (Some(Version::V11), _) => true,
      (_, _)                  => false,
    }
  }

  pub fn should_chunk(&self) -> bool {
    if let  ResponseState::ResponseWithBodyChunks(_, _, _) = *self {
      true
    } else {
      false
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

pub fn default_request_result<O>(state: RequestState, res: IResult<&[u8], O>) -> (BufferMove, RequestState) {
  match res {
    Err(Err::Error(_)) | Err(Err::Failure(_)) => (BufferMove::None, state.into_error()),
    Err(Err::Incomplete(_)) => (BufferMove::None, state),
    _                      => unreachable!()
  }
}

pub fn validate_request_header(mut state: RequestState, header: &Header, sticky_name: &str) -> RequestState {
  match header.value() {
    HeaderValue::Host(host) => {
      match state {
        RequestState::HasRequestLine(rl, conn) => RequestState::HasHost(rl, conn, host),
        RequestState::HasLength(rl, conn, l)   => RequestState::HasHostAndLength(rl, conn, host, l),
        s                                      => s.into_error()
      }
    },
    HeaderValue::ContentLength(sz) => {
      match state {
        RequestState::HasRequestLine(rl, conn) => RequestState::HasLength(rl, conn, LengthInformation::Length(sz)),
        RequestState::HasHost(rl, conn, host)  => RequestState::HasHostAndLength(rl, conn, host, LengthInformation::Length(sz)),
        s                                      => s.into_error()
      }
    },
    HeaderValue::Encoding(TransferEncodingValue::Chunked) => {
      match state {
        RequestState::HasRequestLine(rl, conn)            => RequestState::HasLength(rl, conn, LengthInformation::Chunked),
        RequestState::HasHost(rl, conn, host)             => RequestState::HasHostAndLength(rl, conn, host, LengthInformation::Chunked),
        // Transfer-Encoding takes the precedence on Content-Length
        RequestState::HasHostAndLength(rl, conn, host,
           LengthInformation::Length(_))         => RequestState::HasHostAndLength(rl, conn, host, LengthInformation::Chunked),
        s                                        => s.into_error()
      }
    },
    // FIXME: for now, we don't remember if we cancel indications from a previous Connection Header
    HeaderValue::Connection(c) => {
      if state.get_mut_connection().map(|conn| {
        if c.has_close {
          conn.keep_alive = Some(false);
        }
        if c.has_keep_alive {
          conn.keep_alive = Some(true);
        }
        if c.has_upgrade {
          conn.has_upgrade = true;
        }
      }).is_some() {
        state
      } else {
        state.into_error()
      }
    },
    HeaderValue::ExpectContinue => {
      if state.get_mut_connection().map(|conn| {
        conn.continues = Continue::Expects(0);
      }).is_some() {
        state
      } else {
        state.into_error()
      }
    }

    /*
    HeaderValue::Forwarded(_)  => RequestState::Error(ErrorState::InvalidHttp),
    HeaderValue::XForwardedFor(_) => RequestState::Error(ErrorState::InvalidHttp),
    HeaderValue::XForwardedProto(_) => RequestState::Error(ErrorState::InvalidHttp),
    HeaderValue::XForwardedPort(_) => RequestState::Error(ErrorState::InvalidHttp),
    */
    // FIXME: there should be an error for unsupported encoding
    HeaderValue::Encoding(_) => state.into_error(),
    HeaderValue::Forwarded   => state,
    HeaderValue::Other(_,_)  => state,
    //FIXME: for now, we don't look at what is asked in upgrade since the backend is the one deciding
    HeaderValue::Upgrade(s)  => {
      let mut st = state;
      st.get_mut_connection().map(|conn| conn.upgrade = Some(str::from_utf8(s).expect("should be ascii").to_string()));
      st
    },
    HeaderValue::Cookie(cookies) => {
      let sticky_session_header = cookies.into_iter().find(|ref cookie| &cookie.name[..] == sticky_name.as_bytes());
      if let Some(sticky_session) = sticky_session_header {
        let mut st = state;
        st.get_mut_connection().map(|conn| conn.sticky_session = str::from_utf8(sticky_session.value).map(|s| s.to_string()).ok());

        return st;
      }

      state
    },
    HeaderValue::Error       => state.into_error()
  }
}

pub fn parse_header<'a>(buf: &'a mut Buffer, state: RequestState, sticky_name: &str) -> IResult<&'a [u8], RequestState> {
  match message_header(buf.data()) {
    Ok((i, header)) => Ok((i, validate_request_header(state, &header, sticky_name))),
    Err(e) => Err(e),
  }
}

pub fn parse_request(state: RequestState, buf: &[u8], sticky_name: &str) -> (BufferMove, RequestState) {
  match state {
    RequestState::Initial => {
      match request_line(buf) {
        Ok((i, r))    => {
          if let Some(rl) = RRequestLine::from_request_line(r) {

            let conn = Connection::new();
            //FIXME: what if it's not absolute path or complete URL, but an authority with CONNECT?
            if rl.uri.len() > 0 && rl.uri.as_bytes()[0] != b'/' {
              if let Some(host) = Url::parse(&rl.uri).ok().and_then(|u| u.host_str().map(|s| s.to_string())) {
                (BufferMove::Advance(buf.offset(i)), RequestState::HasHost(rl, conn, host))
              } else {
                (BufferMove::None, (RequestState::Initial).into_error())
              }
            } else {
              /*let conn = if rl.version == "11" {
                Connection::keep_alive()
              } else {
                Connection::close()
              };
              */
              (BufferMove::Advance(buf.offset(i)), RequestState::HasRequestLine(rl, conn))
            }
          } else {
            (BufferMove::None, (RequestState::Initial).into_error())
          }
        },
        res => default_request_result(state, res)
      }
    },
    RequestState::HasRequestLine(rl, conn) => {
      match message_header(buf) {
        Ok((i, header)) => {
          let mv = if header.should_delete(&conn, sticky_name) {
            BufferMove::Delete(buf.offset(i))
          } else if header.must_mutate() {
            BufferMove::Multiple(header.mutate_header(buf, buf.offset(i), sticky_name))
          } else {
            BufferMove::Advance(buf.offset(i))
          };
          (mv, validate_request_header(RequestState::HasRequestLine(rl, conn), &header, sticky_name))
        },
        res => default_request_result(RequestState::HasRequestLine(rl, conn), res)
      }
    },
    RequestState::HasHost(rl, conn, h) => {
      match message_header(buf) {
        Ok((i, header)) => {
          let mv = if header.should_delete(&conn, sticky_name) {
            BufferMove::Delete(buf.offset(i))
          } else if header.must_mutate() {
            BufferMove::Multiple(header.mutate_header(buf, buf.offset(i), sticky_name))
          } else {
            BufferMove::Advance(buf.offset(i))
          };
          (mv, validate_request_header(RequestState::HasHost(rl, conn, h), &header, sticky_name))
        },
        Err(Err::Incomplete(_)) => (BufferMove::None, RequestState::HasHost(rl, conn, h)),
        Err(_) => {
          match crlf(buf) {
            Ok((i, _)) => {
              (BufferMove::Advance(buf.offset(i)), RequestState::Request(rl, conn, h))
            },
            res => {
              //error!("PARSER\tHasHost could not parse header for input:\n{}\n", buf.to_hex(16));
              default_request_result(RequestState::HasHost(rl, conn, h), res)
            }
          }
        }
      }
    },
    RequestState::HasLength(rl, conn, l) => {
      match message_header(buf) {
        Ok((i, header)) => {
          let mv = if header.should_delete(&conn, sticky_name) {
            BufferMove::Delete(buf.offset(i))
          } else if header.must_mutate() {
            BufferMove::Multiple(header.mutate_header(buf, buf.offset(i), sticky_name))
          } else {
            BufferMove::Advance(buf.offset(i))
          };
          (mv, validate_request_header(RequestState::HasLength(rl, conn, l), &header, sticky_name))
        },
        res => default_request_result(RequestState::HasLength(rl, conn, l), res)
      }
    },
    RequestState::HasHostAndLength(rl, conn, h, l) => {
      match message_header(buf) {
        Ok((i, header)) => {
          let mv = if header.should_delete(&conn, sticky_name) {
            BufferMove::Delete(buf.offset(i))
          } else if header.must_mutate() {
            BufferMove::Multiple(header.mutate_header(buf, buf.offset(i), sticky_name))
          } else {
            BufferMove::Advance(buf.offset(i))
          };
          (mv, validate_request_header(RequestState::HasHostAndLength(rl, conn, h, l), &header, sticky_name))
        },
        Err(Err::Incomplete(_)) => (BufferMove::None, RequestState::HasHostAndLength(rl, conn, h, l)),
        Err(_) => {
          match crlf(buf) {
            Ok((i, _)) => {
              debug!("PARSER\theaders parsed, stopping");
                match l {
                  LengthInformation::Chunked    => (BufferMove::Advance(buf.offset(i)), RequestState::RequestWithBodyChunks(rl, conn, h, Chunk::Initial)),
                  LengthInformation::Length(sz) => (BufferMove::Advance(buf.offset(i)), RequestState::RequestWithBody(rl, conn, h, sz)),
                }
            },
            res => {
              error!("PARSER\tHasHostAndLength could not parse header for input:\n{}\n", buf.to_hex(16));
              default_request_result(RequestState::HasHostAndLength(rl, conn, h, l), res)
            }
          }
        }
      }
    },
    RequestState::RequestWithBodyChunks(rl, conn, h, ch) => {
      let (advance, chunk_state) = ch.parse(buf);
      //FIXME: should handle Chunk::Error here
      (advance, RequestState::RequestWithBodyChunks(rl, conn, h, chunk_state))
    },
    _ => {
      error!("PARSER\tunimplemented state: {:?}", state);
      (BufferMove::None, state.into_error())
    }
  }
}

pub fn default_response_result<O>(state: ResponseState, res: IResult<&[u8], O>) -> (BufferMove, ResponseState) {
  match res {
    Err(Err::Error(_)) | Err(Err::Failure(_)) => (BufferMove::None, state.into_error()),
    Err(Err::Incomplete(_)) => (BufferMove::None, state),
    _                      => unreachable!()
  }
}

pub fn validate_response_header(mut state: ResponseState, header: &Header, is_head: bool) -> ResponseState {
  match header.value() {
    HeaderValue::ContentLength(sz) => {
      match state {
        // if the request has a HEAD method, we don't count the content length
        // FIXME: what happens if multiple content lengths appear?
        ResponseState::HasStatusLine(sl, conn) => if is_head {
          ResponseState::HasStatusLine(sl, conn)
        } else {
          ResponseState::HasLength(sl, conn, LengthInformation::Length(sz))
        },
        s                                      => s.into_error(),
      }
    },
    HeaderValue::Encoding(TransferEncodingValue::Chunked) => {
      match state {
        ResponseState::HasStatusLine(sl, conn) => if is_head {
          ResponseState::HasStatusLine(sl, conn)
        } else {
          ResponseState::HasLength(sl, conn, LengthInformation::Chunked)
        },
        s                                      => s.into_error(),
      }
    },
    // FIXME: for now, we don't remember if we cancel indications from a previous Connection Header
    HeaderValue::Connection(c) => {
      if state.get_mut_connection().map(|conn| {
        if c.has_close {
          conn.keep_alive = Some(false);
        }
        if c.has_keep_alive {
          conn.keep_alive = Some(true);
        }
        if c.has_upgrade {
          conn.has_upgrade = true;
        }
      }).is_some() {
        if let ResponseState::HasUpgrade(rl, conn, proto) = state {
          if conn.has_upgrade {
            ResponseState::HasUpgrade(rl, conn, proto)
          } else {
            ResponseState::Error(Some(rl), Some(conn), Some(proto), None, None)
          }
        } else {
          state
        }
      } else {
        state.into_error()
      }
    },
    HeaderValue::Upgrade(protocol) => {
      let proto = str::from_utf8(protocol).expect("the parsed protocol should be a valid utf8 string").to_string();
      trace!("parsed a protocol: {:?}", proto);
      trace!("state is {:?}", state);
      match state {
        ResponseState::HasStatusLine(sl, mut conn) => {
          conn.upgrade = Some(proto.clone());
          ResponseState::HasUpgrade(sl, conn, proto)
        },
        s                                       => s.into_error(),
      }
    }

    // FIXME: there should be an error for unsupported encoding
    HeaderValue::Encoding(_) => state.into_error(),
    HeaderValue::Host(_)     => state.into_error(),
    /*
    HeaderValue::Forwarded(_)  => ResponseState::Error(ErrorState::InvalidHttp),
    HeaderValue::XForwardedFor(_) => ResponseState::Error(ErrorState::InvalidHttp),
    HeaderValue::XForwardedProto(_) => ResponseState::Error(ErrorState::InvalidHttp),
    HeaderValue::XForwardedPort(_) => ResponseState::Error(ErrorState::InvalidHttp),
    */
    HeaderValue::Forwarded   => state,
    HeaderValue::Other(_,_)  => state,
    HeaderValue::ExpectContinue => {
      // we should not get that one from the server
      state.into_error()
    },
    HeaderValue::Cookie(_)   => state,
    HeaderValue::Error       => state.into_error()
  }
}

pub fn parse_response(state: ResponseState, buf: &[u8], is_head: bool, sticky_name: &str) -> (BufferMove, ResponseState) {
  match state {
    ResponseState::Initial => {
      match status_line(buf) {
        Ok((i, r))    => {
          if let Some(rl) = RStatusLine::from_status_line(r) {
            let conn = Connection::new();
            /*let conn = if rl.version == "11" {
              Connection::keep_alive()
            } else {
              Connection::close()
            };
            */
            (BufferMove::Advance(buf.offset(i)), ResponseState::HasStatusLine(rl, conn))
          } else {
            (BufferMove::None, ResponseState::Error(None, None, None, None, None))
          }
        },
        res => default_response_result(state, res)
      }
    },
    ResponseState::HasStatusLine(sl, conn) => {
      match message_header(buf) {
        Ok((i, header)) => {
          let mv = if header.should_delete(&conn, sticky_name) {
            BufferMove::Delete(buf.offset(i))
          } else {
            BufferMove::Advance(buf.offset(i))
          };
          (mv, validate_response_header(ResponseState::HasStatusLine(sl, conn), &header, is_head))
        },
        Err(Err::Incomplete(_)) => (BufferMove::None, ResponseState::HasStatusLine(sl, conn)),
        Err(_)      => {
          match crlf(buf) {
            Ok((i, _)) => {
              debug!("PARSER\theaders parsed, stopping");
              // no content
              if is_head ||
                // all 1xx responses
                sl.status / 100  == 1 || sl.status == 204 || sl.status == 304 {
                (BufferMove::Advance(buf.offset(i)), ResponseState::Response(sl, conn))
              } else {
                // no length information, so we'll assume that the response ends when the connection is closed
                (BufferMove::Advance(buf.offset(i)), ResponseState::ResponseWithBodyCloseDelimited(sl, conn, false))
              }
            },
            res => {
              error!("PARSER\tHasResponseLine could not parse header for input:\n{}\n", buf.to_hex(16));
              default_response_result(ResponseState::HasStatusLine(sl, conn), res)
            }
          }
        }
      }
    },
    ResponseState::HasLength(sl, conn, length) => {
      match message_header(buf) {
        Ok((i, header)) => {
          let mv = if header.should_delete(&conn, sticky_name) {
            BufferMove::Delete(buf.offset(i))
          } else {
            BufferMove::Advance(buf.offset(i))
          };
          (mv,  validate_response_header(ResponseState::HasLength(sl, conn, length), &header, is_head))
        },
        Err(Err::Incomplete(_)) => (BufferMove::None, ResponseState::HasLength(sl, conn, length)),
        Err(_)      => {
          match crlf(buf) {
            Ok((i, _)) => {
              debug!("PARSER\theaders parsed, stopping");
                match length {
                  LengthInformation::Chunked    => (BufferMove::Advance(buf.offset(i)), ResponseState::ResponseWithBodyChunks(sl, conn, Chunk::Initial)),
                  LengthInformation::Length(sz) => (BufferMove::Advance(buf.offset(i)), ResponseState::ResponseWithBody(sl, conn, sz)),
                }
            },
            res => {
              error!("PARSER\tHasResponseLine could not parse header for input:\n{}\n", buf.to_hex(16));
              default_response_result(ResponseState::HasLength(sl, conn, length), res)
            }
          }
        }
      }
    },
    ResponseState::HasUpgrade(sl, conn, protocol) => {
      match message_header(buf) {
        Ok((i, header)) => {
          let mv = if header.should_delete(&conn, sticky_name) {
            BufferMove::Delete(buf.offset(i))
          } else {
            BufferMove::Advance(buf.offset(i))
          };
          (mv, validate_response_header(ResponseState::HasUpgrade(sl, conn, protocol), &header, is_head))
        },
        Err(Err::Incomplete(_)) => (BufferMove::None, ResponseState::HasUpgrade(sl, conn, protocol)),
        Err(_)      => {
          match crlf(buf) {
            Ok((i, _)) => {
              debug!("PARSER\theaders parsed, stopping");
              (BufferMove::Advance(buf.offset(i)), ResponseState::ResponseUpgrade(sl, conn, protocol))
            },
            res => {
              error!("PARSER\tHasResponseLine could not parse header for input:\n{}\n", buf.to_hex(16));
              default_response_result(ResponseState::HasUpgrade(sl, conn, protocol), res)
            }
          }
        }
      }
    },
    ResponseState::ResponseWithBodyChunks(rl, conn, ch) => {
      let (advance, chunk_state) = ch.parse(buf);
      (advance, ResponseState::ResponseWithBodyChunks(rl, conn, chunk_state))
    },
    ResponseState::ResponseWithBodyCloseDelimited(rl, conn, b) => {
      (BufferMove::Advance(buf.len()), ResponseState::ResponseWithBodyCloseDelimited(rl, conn, b))
    },
    _ => {
      error!("PARSER\tunimplemented state: {:?}", state);
      (BufferMove::None, state.into_error())
    }
  }
}

pub fn parse_request_until_stop(mut current_state: RequestState, mut header_end: Option<usize>,
  buf: &mut BufferQueue, added_req_header: &str, sticky_name: &str)
  -> (RequestState, Option<usize>) {
  loop {
    let (mv, new_state) = parse_request(current_state, buf.unparsed_data(), sticky_name);
    //println!("PARSER\t{}\tinput:\n{}\nmv: {:?}, new state: {:?}\n", request_id, &buf.unparsed_data().to_hex(16), mv, new_state);
    //trace!("PARSER\t{}\tinput:\n{}\nmv: {:?}, new state: {:?}\n", request_id, &buf.unparsed_data().to_hex(16), mv, new_state);
    //trace!("PARSER\t{}\tmv: {:?}, new state: {:?}\n", request_id, mv, new_state);
    current_state = new_state;

    match mv {
      BufferMove::Advance(sz) => {
        assert!(sz != 0, "buffer move should not be 0");
        //FIXME: what if we advance past the buffer's end? Splice?
        buf.consume_parsed_data(sz);
        if header_end.is_none() {
          match current_state {
            RequestState::Request(_,_,_) |
            RequestState::RequestWithBodyChunks(_,_,_,Chunk::Initial) => {
              //println!("FOUND HEADER END (advance):{}", buf.start_parsing_position);
              header_end = Some(buf.start_parsing_position);
              buf.insert_output(Vec::from(added_req_header.as_bytes()));
              buf.slice_output(sz);
            },
            RequestState::RequestWithBody(_,ref mut conn,_,content_length) => {
              header_end = Some(buf.start_parsing_position);
              buf.insert_output(Vec::from(added_req_header.as_bytes()));

              // If we got "Expects: 100-continue", the body will be sent later
              if conn.continues == Continue::None {
                buf.slice_output(sz+content_length);
                buf.consume_parsed_data(content_length);
              } else {
                buf.slice_output(sz);
                conn.continues = Continue::Expects(content_length);
              }
            },
            _ => {
              buf.slice_output(sz);
            }
          }
        } else {
          buf.slice_output(sz);
        }
      },
      BufferMove::Delete(length) => {
        buf.consume_parsed_data(length);
        if header_end.is_none() {
          match current_state {
            RequestState::Request(_,_,_) |
            RequestState::RequestWithBodyChunks(_,_,_,_) => {
              //println!("FOUND HEADER END (delete):{}", buf.start_parsing_position);
              header_end = Some(buf.start_parsing_position);
              buf.insert_output(Vec::from(added_req_header.as_bytes()));
              buf.delete_output(length);
            },
            RequestState::RequestWithBody(_,_,_,content_length) => {
              header_end = Some(buf.start_parsing_position);
              buf.insert_output(Vec::from(added_req_header.as_bytes()));
              buf.delete_output(length);

              buf.slice_output(content_length);
              buf.consume_parsed_data(content_length);
            },
            _ => {
              buf.delete_output(length);
            }
          }
        } else {
          buf.delete_output(length);
        }
      },
      BufferMove::Multiple(buffer_moves) => {
        for buffer_move in buffer_moves {
          match buffer_move {
            BufferMove::Advance(length) => {
              buf.consume_parsed_data(length);
              buf.slice_output(length);
            },
            BufferMove::Delete(length) => {
              buf.consume_parsed_data(length);
              buf.delete_output(length);
            },
            e => {
              error!("BufferMove {:?} isn't implemented", e);
              unimplemented!();
            }
          }
        }
      }
      _ => break
    }

    match current_state {
      RequestState::Error(_,_,_,_,_) => {
        incr!("http1.parser.request.error");
        break;
      },
      RequestState::Request(_,_,_) | RequestState::RequestWithBody(_,_,_,_) |
        RequestState::RequestWithBodyChunks(_,_,_,Chunk::Ended) => break,
      _ => ()
    }
  }

  (current_state, header_end)
}

pub fn parse_response_until_stop(mut current_state: ResponseState, mut header_end: Option<usize>,
    buf: &mut BufferQueue, is_head: bool, added_res_header: &str,
    sticky_name: &str, sticky_session: Option<&StickySession>)
  -> (ResponseState, Option<usize>) {
  loop {
    //trace!("PARSER\t{}\tpos[{}]: {:?}", request_id, position, current_state);
    let (mv, new_state) = parse_response(current_state, buf.unparsed_data(), is_head, sticky_name);
    //trace!("PARSER\tinput:\n{}\nmv: {:?}, new state: {:?}\n", buf.unparsed_data().to_hex(16), mv, new_state);
    //trace!("PARSER\t{}\tmv: {:?}, new state: {:?}\n", request_id, mv, new_state);
    current_state = new_state;

    match mv {
      BufferMove::Advance(sz) => {
        assert!(sz != 0, "buffer move should not be 0");

        // header_end is some if we already parsed the headers
        if header_end.is_none() {
          match current_state {
            ResponseState::Response(_,_) |
            ResponseState::ResponseUpgrade(_,_,_) |
            ResponseState::ResponseWithBodyChunks(_,_,_) => {
              buf.insert_output(Vec::from(added_res_header.as_bytes()));
              add_sticky_session_to_response(buf, sticky_name, sticky_session);

              buf.consume_parsed_data(sz);
              header_end = Some(buf.start_parsing_position);

              buf.slice_output(sz);
            },
            ResponseState::ResponseWithBody(_,_,content_length) => {
              buf.insert_output(Vec::from(added_res_header.as_bytes()));
              add_sticky_session_to_response(buf, sticky_name, sticky_session);

              buf.consume_parsed_data(sz);
              header_end = Some(buf.start_parsing_position);

              buf.slice_output(sz+content_length);
              buf.consume_parsed_data(content_length);
            },
            ResponseState::ResponseWithBodyCloseDelimited(_,ref conn, _) => {
              buf.insert_output(Vec::from(added_res_header.as_bytes()));
              add_sticky_session_to_response(buf, sticky_name, sticky_session);

              // special case: some servers send responses with no body,
              // no content length, and Connection: close
              // since we deleted the Connection header, we'll add a new one
              if conn.keep_alive == Some(false) {
                buf.insert_output(Vec::from(&b"Connection: close\r\n"[..]));
              }

              buf.consume_parsed_data(sz);
              header_end = Some(buf.start_parsing_position);

              buf.slice_output(sz);

              let len = buf.available_input_data();
              buf.consume_parsed_data(len);
              buf.slice_output(len);
            },
            _ => {
              buf.consume_parsed_data(sz);
              buf.slice_output(sz);
            }
          }
        } else {
          buf.consume_parsed_data(sz);
          buf.slice_output(sz);
        }
        //FIXME: if we add a slice here, we will get a first large slice, then a long list of buffer size slices added by the slice_input function
      },
      BufferMove::Delete(length) => {
        buf.consume_parsed_data(length);
        if header_end.is_none() {
          match current_state {
            ResponseState::Response(_,_) |
            ResponseState::ResponseUpgrade(_,_,_) |
            ResponseState::ResponseWithBodyChunks(_,_,_) => {
              //println!("FOUND HEADER END (delete):{}", buf.start_parsing_position);
              header_end = Some(buf.start_parsing_position);
              buf.insert_output(Vec::from(added_res_header.as_bytes()));
              add_sticky_session_to_response(buf, sticky_name, sticky_session);

              buf.delete_output(length);
            },
            ResponseState::ResponseWithBody(_,_,content_length) => {
              header_end = Some(buf.start_parsing_position);
              buf.insert_output(Vec::from(added_res_header.as_bytes()));
              buf.delete_output(length);

              add_sticky_session_to_response(buf, sticky_name, sticky_session);

              buf.slice_output(content_length);
              buf.consume_parsed_data(content_length);
            },
            _ => {
              buf.delete_output(length);
            }
          }
        } else {
          buf.delete_output(length);
        }
      },
      _ => break
    }

    match current_state {
      ResponseState::Error(_,_,_,_,_) => {
        incr!("http1.parser.response.error");
        break;
      }
      ResponseState::Response(_,_) | ResponseState::ResponseWithBody(_,_,_) |
        ResponseState::ResponseUpgrade(_,_,_) |
        ResponseState::ResponseWithBodyChunks(_,_,Chunk::Ended) |
        ResponseState::ResponseWithBodyCloseDelimited(_,_,_) => break,
      _ => ()
    }
    //println!("move: {:?}, new state: {:?}, input_queue {:?}, output_queue: {:?}", mv, current_state, buf.input_queue, buf.output_queue);
  }

  //println!("end state: {:?}, input_queue {:?}, output_queue: {:?}", current_state, buf.input_queue, buf.output_queue);
  (current_state, header_end)
}

fn add_sticky_session_to_response(buf: &mut BufferQueue,
  sticky_name: &str, sticky_session: Option<&StickySession>) {
  if let Some(ref sticky_backend) = sticky_session {
    let sticky_cookie = format!("Set-Cookie: {}={}; Path=/\r\n", sticky_name, sticky_backend.sticky_id);
    buf.insert_output(Vec::from(sticky_cookie.as_bytes()));
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use nom::{Err,error::ErrorKind,HexDisplay};
  use buffer_queue::{OutputElement,buf_with_capacity};
  use std::io::Write;

  /*
  #[test]
  #[cfg(target_pointer_width = "64")]
  fn size_test() {
    assert_size!(RequestState, 240);
    assert_size!(ResponseState, 208);
    assert_size!(StickySession, 24);
  }
  */

  #[test]
  fn request_line_test() {
      let input = b"GET /index.html HTTP/1.1\r\n";
      let result = request_line(input);
      let expected = RequestLine {
        method: b"GET",
        uri: b"/index.html",
        version: Version::V11
      };

      assert_eq!(result, Ok((&[][..], expected)));
  }

  #[test]
  fn header_test() {
      let input = b"Accept: */*\r\n";
      let result = message_header(input);
      let expected = Header {
        name: b"Accept",
        value: b"*/*"
      };

      assert_eq!(result, Ok((&b""[..], expected)))
  }

  #[test]
  #[cfg(not(feature = "tolerant-http1-parser"))]
  fn header_iso_8859_1_test() {
      let input = "Test: Aéo\r\n";
      let result = message_header(input.as_bytes());

      assert_eq!(result, Err(Err::Error(error_position!("éo\r\n".as_bytes(), ErrorKind::Tag))));
  }

  #[test]
  #[cfg(feature = "tolerant-http1-parser")]
  fn header_iso_8859_1_test() {
      let input = "Test: Aéo\r\n";
      let result = message_header(input.as_bytes());
      let expected = Header {
        name: b"Test",
        value: "Aéo".as_bytes()
      };

      assert_eq!(result, Ok((&b""[..], expected)))
  }

  #[test]
  fn header_without_space_test() {
      let input = b"Host:localhost\r\n";
      let result = message_header(input);
      let expected = Header {
        name: b"Host",
        value: b"localhost"
      };

      assert_eq!(result, Ok((&b""[..], expected)))
  }

  #[test]
  fn header_user_agent() {
      let input = b"User-Agent: Mozilla/5.0 (Macintosh; Intel Mac OS X 10.10; rv:44.0) Gecko/20100101 Firefox/44.0\r\n";

      let result = message_header(input);
      assert_eq!(
        result,
        Ok((&b""[..], Header {
          name: b"User-Agent",
          value: b"Mozilla/5.0 (Macintosh; Intel Mac OS X 10.10; rv:44.0) Gecko/20100101 Firefox/44.0"
        }))
      );
  }

  #[test]
  fn parse_state_host_in_url_test() {
      let input =
          b"GET http://example.com:8888/index.html HTTP/1.1\r\n\
            User-Agent: curl/7.43.0\r\n\
            Accept: */*\r\n\
            Content-Length: 200\r\n\
            \r\n";
      let initial = RequestState::Initial;
      let (pool, mut buf) = buf_with_capacity(2048);
      buf.write(&input[..]).unwrap();
      println!("buffer input: {:?}", buf.input_queue);

      //let result = parse_request(initial, input);
      let result = parse_request_until_stop(initial, None, &mut buf, "", "SOZUBALANCEID");
      println!("result: {:?}", result);
      println!("input length: {}", input.len());
      println!("buffer input: {:?}", buf.input_queue);
      println!("buffer output: {:?}", buf.output_queue);
      assert_eq!(buf.output_queue, vec!(
        OutputElement::Slice(49), OutputElement::Slice(25),
        OutputElement::Slice(13), OutputElement::Slice(21),
        OutputElement::Insert(vec!()), OutputElement::Slice(202)));
      assert_eq!(buf.start_parsing_position, 310);
      assert_eq!(
        result,
        (
          RequestState::RequestWithBody(
            RRequestLine { method: Method::Get, uri: String::from("http://example.com:8888/index.html"), version: Version::V11 },
            Connection::new(),
            String::from("example.com"),
            200
          ),
          Some(110)
        )
      );
  }

  #[test]
  fn parse_state_host_in_url_conflict_test() {
      let input =
          b"GET http://example.com:8888/index.html HTTP/1.1\r\n\
            Host: test.org\r\n\
            User-Agent: curl/7.43.0\r\n\
            Accept: */*\r\n\
            Content-Length: 200\r\n\
            \r\n";
      let initial = RequestState::Initial;
      let (pool, mut buf) = buf_with_capacity(2048);
      buf.write(&input[..]).unwrap();
      println!("buffer input: {:?}", buf.input_queue);

      //let result = parse_request(initial, input);
      let result = parse_request_until_stop(initial, None, &mut buf, "", "SOZUBALANCEID");
      println!("result: {:?}", result);
      println!("input length: {}", input.len());
      println!("buffer input: {:?}", buf.input_queue);
      println!("buffer output: {:?}", buf.output_queue);
      assert_eq!(buf.output_queue, vec!(
        OutputElement::Slice(49), OutputElement::Slice(16)));
      assert_eq!(buf.start_parsing_position, 65);
      assert_eq!(
        result,
        (
          RequestState::Error(Some(
            RRequestLine { method: Method::Get, uri: String::from("http://example.com:8888/index.html"), version: Version::V11 },
          ),
            Some(Connection::new()), Some(String::from("example.com")), None, None),
          None
        )
      );
  }

  #[test]
  fn parse_state_content_length_test() {
      let input =
          b"GET /index.html HTTP/1.1\r\n\
            Host: localhost:8888\r\n\
            User-Agent: curl/7.43.0\r\n\
            Accept: */*\r\n\
            Content-Length: 200\r\n\
            \r\n";
      let initial = RequestState::Initial;
      let (pool, mut buf) = buf_with_capacity(2048);
      buf.write(&input[..]).unwrap();
      println!("buffer input: {:?}", buf.input_queue);

      //let result = parse_request(initial, input);
      let result = parse_request_until_stop(initial, None, &mut buf, "", "SOZUBALANCEID");
      println!("result: {:?}", result);
      println!("input length: {}", input.len());
      println!("buffer input: {:?}", buf.input_queue);
      println!("buffer output: {:?}", buf.output_queue);
      assert_eq!(buf.output_queue, vec!(
        OutputElement::Slice(26), OutputElement::Slice(22), OutputElement::Slice(25),
        OutputElement::Slice(13), OutputElement::Slice(21),
        OutputElement::Insert(vec!()), OutputElement::Slice(202)));
      assert_eq!(buf.start_parsing_position, 309);
      assert_eq!(
        result,
        (
          RequestState::RequestWithBody(
            RRequestLine { method: Method::Get, uri: String::from("/index.html"), version: Version::V11 },
            Connection::new(),
            String::from("localhost:8888"),
            200
          ),
          Some(109)
        )
      );
  }

  #[test]
  fn parse_state_content_length_partial() {
      let input =
          b"GET /index.html HTTP/1.1\r\n\
            Host: localhost:8888\r\n\
            User-Agent: curl/7.43.0\r\n\
            Accept: */*\r\n\
            Content-Length: 200\r\n\
            \r\n";
      let initial = RequestState::HasRequestLine(
          RRequestLine {
            method: Method::Get,
            uri: String::from("/index.html"),
            version: Version::V11
          },
          Connection::keep_alive()
        );

      let (pool, mut buf) = buf_with_capacity(2048);
      println!("skipping input:\n{}", (&input[..26]).to_hex(16));
      buf.write(&input[..]).unwrap();
      println!("unparsed data:\n{}", buf.unparsed_data().to_hex(16));
      println!("buffer output: {:?}", buf.output_queue);
      buf.consume_parsed_data(26);
      buf.slice_output(26);
      println!("unparsed data after consume(26):\n{}", buf.unparsed_data().to_hex(16));
      println!("buffer output: {:?}", buf.output_queue);

      let result = parse_request_until_stop(initial, None, &mut buf, "", "SOZUBALANCEID");
      println!("unparsed data after parsing:\n{}", buf.unparsed_data().to_hex(16));
      println!("result: {:?}", result);
      println!("input length: {}", input.len());
      println!("buffer output: {:?}", buf.output_queue);
      assert_eq!(buf.output_queue, vec!(
        OutputElement::Slice(26), OutputElement::Slice(22),
        OutputElement::Slice(25), OutputElement::Slice(13),
        OutputElement::Slice(21), OutputElement::Insert(vec!()),
        OutputElement::Slice(202)));
      assert_eq!(buf.start_parsing_position, 309);
      assert_eq!(
        result,
        (
          RequestState::RequestWithBody(
            RRequestLine { method: Method::Get, uri: String::from("/index.html"), version: Version::V11 },
            Connection::keep_alive(),
            String::from("localhost:8888"),
            200
          ),
          Some(109)
        )
      );
  }

  #[test]
  fn parse_state_chunked_test() {
      let input =
          b"GET /index.html HTTP/1.1\r\n\
            Host: localhost:8888\r\n\
            User-Agent: curl/7.43.0\r\n\
            Transfer-Encoding: chunked\r\n\
            Accept: */*\r\n\
            \r\n";
      let initial = RequestState::Initial;
      let (pool, mut buf) = buf_with_capacity(2048);
      buf.write(&input[..]).unwrap();

      //let result = parse_request(initial, input);
      let result = parse_request_until_stop(initial, None, &mut buf, "", "SOZUBALANCEID");
      println!("result: {:?}", result);
      assert_eq!(buf.start_parsing_position, 116);
      assert_eq!(
        result,
        (
          RequestState::RequestWithBodyChunks(
            RRequestLine { method: Method::Get, uri: String::from("/index.html"), version: Version::V11 },
            Connection::new(),
            String::from("localhost:8888"),
            Chunk::Initial
          ),
          Some(116)
        )
      );
  }

  #[test]
  fn parse_state_duplicate_content_length_test() {
      let input =
          b"GET /index.html HTTP/1.1\r\n\
            Host: localhost:8888\r\n\
            User-Agent: curl/7.43.0\r\n\
            Content-Length: 120\r\n\
            Accept: */*\r\n\
            Content-Length: 200\r\n\
            \r\n";
      let initial = RequestState::Initial;

      let (pool, mut buf) = buf_with_capacity(2048);
      buf.write(&input[..]).unwrap();

      let result = parse_request_until_stop(initial, None, &mut buf, "", "SOZUBALANCEID");
      println!("result: {:?}", result);
      assert_eq!(buf.start_parsing_position, 128);
      assert_eq!(
        result,
        (
          RequestState::Error(Some(
            RRequestLine { method: Method::Get, uri: String::from("/index.html"), version: Version::V11 },
          ),
            Some(Connection::new()), Some(String::from("localhost:8888")),
            Some(LengthInformation::Length(120)), None),
          None
        )
      );
  }

  // if there was a content-length, the chunked transfer encoding takes precedence
  #[test]
  fn parse_state_content_length_and_chunked_test() {
      let input =
          b"GET /index.html HTTP/1.1\r\n\
            Host: localhost:8888\r\n\
            User-Agent: curl/7.43.0\r\n\
            Content-Length: 10\r\n\
            Transfer-Encoding: chunked\r\n\
            Accept: */*\r\n\
            \r\n";
      let initial = RequestState::Initial;
      let (pool, mut buf) = buf_with_capacity(2048);
      buf.write(&input[..]).unwrap();

      //let result = parse_request(initial, input);
      let result = parse_request_until_stop(initial, None, &mut buf, "", "SOZUBALANCEID");
      println!("result: {:?}", result);
      assert_eq!(buf.start_parsing_position, 136);
      assert_eq!(
        result,
        (
          RequestState::RequestWithBodyChunks(
            RRequestLine { method: Method::Get, uri: String::from("/index.html"), version: Version::V11 },
            Connection::new(),
            String::from("localhost:8888"),
            Chunk::Initial
          ),
          Some(136)
        )
      );
  }

  #[test]
  fn parse_request_without_length() {
      setup_test_logger!();
      let input =
          b"GET / HTTP/1.1\r\n\
            Host: localhost:8888\r\n\
            Connection: close\r\n\
            \r\n";
      let initial = RequestState::Initial;
      let (pool, mut buf) = buf_with_capacity(2048);
      buf.write(&input[..]).unwrap();

      //let result = parse_request(initial, input);
      let result = parse_request_until_stop(initial, None, &mut buf, "", "SOZUBALANCEID");
      println!("result: {:?}", result);
      println!("input length: {}", input.len());
      println!("buffer output: {:?}", buf.output_queue);
      assert_eq!(buf.output_queue, vec!(
        OutputElement::Slice(16), OutputElement::Slice(22), OutputElement::Delete(19),
        OutputElement::Insert(vec!()), OutputElement::Slice(2)));
      assert_eq!(buf.start_parsing_position, 59);
      assert_eq!(
        result,
        (
          RequestState::Request(
            RRequestLine { method: Method::Get, uri: String::from("/"), version: Version::V11 },
            Connection::close(),
            String::from("localhost:8888")
          ),
          Some(59)
        )
      );
  }

  // HTTP 1.0 is connection close by default
  #[test]
  fn parse_request_http_1_0_connection_close() {
      let input =
          b"GET / HTTP/1.0\r\n\
            Host: localhost:8888\r\n\
            \r\n";
      let initial = RequestState::Initial;
      let (pool, mut buf) = buf_with_capacity(2048);
      buf.write(&input[..]).unwrap();

      //let result = parse_request(initial, input);
      let result = parse_request_until_stop(initial, None, &mut buf, "", "SOZUBALANCEID");
      println!("result: {:?}", result);
      assert_eq!(buf.start_parsing_position, 40);
      assert_eq!(
        result,
        (
          RequestState::Request(
            RRequestLine { method: Method::Get, uri: String::from("/"), version: Version::V10 },
            Connection::new(),
            String::from("localhost:8888")
          ),
          Some(40)
        )
      );
      assert!(!result.0.should_keep_alive());
  }

  #[test]
  fn parse_request_http_1_0_connection_keep_alive() {
    setup_test_logger!();
      let input =
          b"GET / HTTP/1.0\r\n\
            Host: localhost:8888\r\n\
            Connection: keep-alive\r\n\
            \r\n";
      let initial = RequestState::Initial;
      let (pool, mut buf) = buf_with_capacity(2048);
      buf.write(&input[..]).unwrap();

      //let result = parse_request(initial, input);
      let result = parse_request_until_stop(initial, None, &mut buf, "", "SOZUBALANCEID");
      println!("result: {:?}", result);
      println!("input length: {}", input.len());
      println!("buffer output: {:?}", buf.output_queue);
      assert_eq!(buf.output_queue, vec!(
        OutputElement::Slice(16), OutputElement::Slice(22), OutputElement::Delete(24),
        OutputElement::Insert(vec!()), OutputElement::Slice(2)));
      assert_eq!(buf.start_parsing_position, 64);
      assert_eq!(
        result,
        (
          RequestState::Request(
            RRequestLine { method: Method::Get, uri: String::from("/"), version: Version::V10 },
            Connection::keep_alive(),
            String::from("localhost:8888")
          ),
          Some(64)
        )
      );
      assert!(result.0.should_keep_alive());
  }

  #[test]
  fn parse_request_http_1_1_connection_keep_alive() {
      setup_test_logger!();
      let input =
          b"GET / HTTP/1.1\r\n\
            Host: localhost:8888\r\n\
            \r\n";
      let initial = RequestState::Initial;
      let (pool, mut buf) = buf_with_capacity(2048);
      buf.write(&input[..]).unwrap();

      //let result = parse_request(initial, input);
      let result = parse_request_until_stop(initial, None, &mut buf, "", "SOZUBALANCEID");
      println!("end buf:\n{}", buf.buffer.data().to_hex(16));
      println!("result: {:?}", result);
      assert_eq!(buf.output_queue, vec!(
        OutputElement::Slice(16), OutputElement::Slice(22),
        OutputElement::Insert(vec!()), OutputElement::Slice(2)));
      assert_eq!(buf.start_parsing_position, 40);
      assert_eq!(
        result,
        (
          RequestState::Request(
            RRequestLine { method: Method::Get, uri: String::from("/"), version: Version::V11 },
            Connection::new(),
            String::from("localhost:8888")
          ),
          Some(40)
        )
      );
      assert!(result.0.should_keep_alive());
  }

  #[test]
  fn parse_request_http_1_1_connection_close() {
      setup_test_logger!();
      let input =
          b"GET / HTTP/1.1\r\n\
            Connection: close\r\n\
            Host: localhost:8888\r\n\
            \r\n";
      let initial = RequestState::Initial;
      let (pool, mut buf) = buf_with_capacity(2048);
      buf.write(&input[..]).unwrap();

      //let result = parse_request(initial, input);
      let result = parse_request_until_stop(initial, None, &mut buf, "", "SOZUBALANCEID");
      println!("end buf:\n{}", buf.buffer.data().to_hex(16));
      println!("result: {:?}", result);
      assert_eq!(buf.output_queue, vec!(
        OutputElement::Slice(16), OutputElement::Delete(19), OutputElement::Slice(22),
        OutputElement::Insert(vec!()), OutputElement::Slice(2)));
      assert_eq!(buf.start_parsing_position, 59);
      assert_eq!(
        result,
        (
          RequestState::Request(
            RRequestLine { method: Method::Get, uri: String::from("/"), version: Version::V11 },
            Connection::close(),
            String::from("localhost:8888")
          ),
          Some(59)
        )
      );
      assert!(!result.0.should_keep_alive());
  }

  #[test]
  fn parse_request_add_header_test() {
      setup_test_logger!();
      let input =
          b"GET /index.html HTTP/1.1\r\n\
            Host: localhost:8888\r\n\
            User-Agent: curl/7.43.0\r\n\
            Accept: */*\r\n\
            Content-Length: 200\r\n\
            \r\n";
      let initial = RequestState::Initial;
      let (pool, mut buf) = buf_with_capacity(2048);
      buf.write(&input[..]).unwrap();

      let new_header = b"Sozu-Id: 123456789\r\n";
      let result = parse_request_until_stop(initial, None, &mut buf, "Sozu-Id: 123456789\r\n", "SOZUBALANCEID");
      println!("result: {:?}", result);
      println!("input length: {}", input.len());
      println!("buffer output: {:?}", buf.output_queue);
      assert_eq!(buf.output_queue, vec!(
        OutputElement::Slice(26), OutputElement::Slice(22), OutputElement::Slice(25),
        OutputElement::Slice(13), OutputElement::Slice(21), OutputElement::Insert(Vec::from(&new_header[..])),
      OutputElement::Slice(202)));
      println!("buf:\n{}", buf.buffer.data().to_hex(16));
      assert_eq!(buf.start_parsing_position, 309);
      assert_eq!(
        result,
        (
          RequestState::RequestWithBody(
            RRequestLine { method: Method::Get, uri: String::from("/index.html"), version: Version::V11 },
            Connection::new(),
            String::from("localhost:8888"),
            200
          ),
          Some(109)
        )
      );
  }

  #[test]
  fn parse_request_delete_forwarded_headers() {
      setup_test_logger!();
      let input =
          b"GET /index.html HTTP/1.1\r\n\
            Host: localhost:8888\r\n\
            Forwarded: proto:https;for=27.0.0.1:1234;by:proxy\r\n\
            X-forwarded-Proto: https\r\n\
            X-Forwarded-For: 127.0.0.1\r\n\
            X-Forwarded-Port: 1234\r\n\
            \r\n";
      let initial = RequestState::Initial;
      let (pool, mut buf) = buf_with_capacity(2048);
      buf.write(&input[..]).unwrap();

      let new_header = b"Sozu-Id: 123456789\r\n";
      let result = parse_request_until_stop(initial, None, &mut buf, "Sozu-Id: 123456789\r\n", "SOZUBALANCEID");
      println!("result: {:?}", result);
      println!("input length: {}", input.len());
      println!("buffer output: {:?}", buf.output_queue);
      assert_eq!(buf.output_queue, vec!(
        OutputElement::Slice(26), OutputElement::Slice(22),
        // Forwarded
        OutputElement::Delete(51),
        // X-Forwarded-Proto
        OutputElement::Delete(26),
        // X-Forwarded-For
        OutputElement::Delete(28),
        // X-Forwarded-Port
        OutputElement::Delete(24),
        OutputElement::Insert(Vec::from(&new_header[..])),
      OutputElement::Slice(2)));
      println!("buf:\n{}", buf.buffer.data().to_hex(16));
      assert_eq!(buf.start_parsing_position, 179);
      assert_eq!(
        result,
        (
          RequestState::Request(
            RRequestLine { method: Method::Get, uri: String::from("/index.html"), version: Version::V11 },
            Connection::new(),
            String::from("localhost:8888"),
          ),
          Some(179)
        )
      );
  }

  #[test]
  fn parse_chunk() {
    let input =
      b"4\r\n\
      Wiki\r\n\
      5\r\n\
      pedia\r\n\
      e\r\n \
      in\r\n\r\nchunks.\r\n\
      0\r\n\
      \r\n";

    let initial = Chunk::Initial;

    let res = initial.parse(&input[..]);
    println!("result: {:?}", res);
    assert_eq!(
      res,
      (BufferMove::Advance(43), Chunk::Ended)
    );
  }

  #[test]
  fn parse_chunk_partial() {
    let input =
      b"4\r\n\
      Wiki\r\n\
      5\r\n\
      pedia\r\n\
      e\r\n \
      in\r\n\r\nchunks.\r\n\
      0\r\n\
      \r\n";

    let initial = Chunk::Initial;

    println!("parsing input:\n{}", (&input[..12]).to_hex(16));
    let res = initial.parse(&input[..12]);
    println!("result: {:?}", res);
    assert_eq!(
      res,
      (BufferMove::Advance(17), Chunk::Copying)
    );

    println!("consuming input:\n{}", (&input[..17]).to_hex(16));
    println!("parsing input:\n{}", (&input[17..]).to_hex(16));
    let res2 = res.1.parse(&input[17..]);
    assert_eq!(
      res2,
      (BufferMove::Advance(26), Chunk::Ended)
    );
  }

  #[test]
  fn parse_requests_and_chunks_test() {
      let input =
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
            \r\n";
      let initial = RequestState::Initial;
      let (pool, mut buf) = buf_with_capacity(2048);
      buf.write(&input[..]).unwrap();

      //let result = parse_request(initial, input);
      let result = parse_request_until_stop(initial, None, &mut buf, "", "SOZUBALANCEID");
      println!("result: {:?}", result);
      assert_eq!(buf.start_parsing_position, 160);
      assert_eq!(
        result,
        (
          RequestState::RequestWithBodyChunks(
            RRequestLine { method: Method::Post, uri: String::from("/index.html"), version: Version::V11 },
            Connection::new(),
            String::from("localhost:8888"),
            Chunk::Ended
          ),
          Some(117)
        )
      );
  }

  #[test]
  fn parse_requests_and_chunks_partial_test() {
      let input =
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
            \r\n";
      let initial = RequestState::Initial;
      let (pool, mut buf) = buf_with_capacity(2048);
      buf.write(&input[..125]).unwrap();
      println!("parsing\n{}", buf.buffer.data().to_hex(16));

      let result = parse_request_until_stop(initial, None, &mut buf, "", "SOZUBALANCEID");
      println!("result({}): {:?}", line!(), result);
      assert_eq!(buf.start_parsing_position, 124);
      assert_eq!(
        result,
        (
          RequestState::RequestWithBodyChunks(
            RRequestLine { method: Method::Post, uri: String::from("/index.html"), version: Version::V11 },
            Connection::new(),
            String::from("localhost:8888"),
            Chunk::Copying
          ),
          Some(117)
        )
      );

      //buf.consume(124);
      buf.write(&input[125..140]).unwrap();
      println!("parsing\n{}", buf.buffer.data().to_hex(16));

      let result = parse_request_until_stop(result.0, result.1, &mut buf, "", "SOZUBALANCEID");
      println!("result({}): {:?}", line!(), result);
      assert_eq!(buf.start_parsing_position, 153);
      assert_eq!(
        result,
        (
          RequestState::RequestWithBodyChunks(
            RRequestLine { method: Method::Post, uri: String::from("/index.html"), version: Version::V11 },
            Connection::new(),
            String::from("localhost:8888"),
            Chunk::Copying
          ),
          Some(117)
        )
      );

      buf.write(&input[153..]).unwrap();
      println!("parsing\n{}", buf.buffer.data().to_hex(16));
      let result = parse_request_until_stop(result.0, result.1, &mut buf, "", "SOZUBALANCEID");
      println!("result({}): {:?}", line!(), result);
      assert_eq!(buf.start_parsing_position, 160);
      assert_eq!(
        result,
        (
          RequestState::RequestWithBodyChunks(
            RRequestLine { method: Method::Post, uri: String::from("/index.html"), version: Version::V11 },
            Connection::new(),
            String::from("localhost:8888"),
            Chunk::Ended
          ),
          Some(117)
        )
      );
  }

  #[test]
  fn parse_response_and_chunks_partial_test() {
      let input =
          b"HTTP/1.1 200 OK\r\n\
            Server: ABCD\r\n\
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
            \r\n";
      let initial = ResponseState::Initial;
      let (pool, mut buf) = buf_with_capacity(2048);
      buf.write(&input[..78]).unwrap();
      println!("parsing\n{}", buf.buffer.data().to_hex(16));

      let result = parse_response_until_stop(initial, None, &mut buf, false, "", "SOZUBALANCEID", None);
      println!("result({}): {:?}", line!(), result);
      assert_eq!(buf.start_parsing_position, 81);
      assert_eq!(
        result,
        (
          ResponseState::ResponseWithBodyChunks(
            RStatusLine { version: Version::V11, status: 200, reason: String::from("OK") },
            Connection::new(),
            Chunk::Copying
          ),
          Some(74)
        )
      );

      //buf.consume(78);
      buf.write(&input[81..100]).unwrap();
      println!("parsing\n{}", buf.buffer.data().to_hex(16));

      let result = parse_response_until_stop(result.0, result.1, &mut buf, false, "", "SOZUBALANCEID", None);
      println!("result({}): {:?}", line!(), result);
      assert_eq!(buf.start_parsing_position, 110);
      assert_eq!(
        result,
        (
          ResponseState::ResponseWithBodyChunks(
            RStatusLine { version: Version::V11, status: 200, reason: String::from("OK") },
            Connection::new(),
            Chunk::Copying
          ),
          Some(74)
        )
      );

      //buf.consume(19);
      println!("remaining:\n{}", &input[110..].to_hex(16));
      buf.write(&input[110..116]).unwrap();
      println!("parsing\n{}", buf.buffer.data().to_hex(16));
      let result = parse_response_until_stop(result.0, result.1, &mut buf, false, "", "SOZUBALANCEID", None);
      println!("result({}): {:?}", line!(), result);
      assert_eq!(buf.start_parsing_position, 115);
      assert_eq!(
        result,
        (
          ResponseState::ResponseWithBodyChunks(
            RStatusLine { version: Version::V11, status: 200, reason: String::from("OK") },
            Connection::new(),
            Chunk::CopyingLastHeader
          ),
          Some(74)
        )
      );

      //buf.consume(5);
      buf.write(&input[116..]).unwrap();
      println!("parsing\n{}", buf.buffer.data().to_hex(16));
      let result = parse_response_until_stop(result.0, result.1, &mut buf, false, "", "SOZUBALANCEID", None);
      println!("result({}): {:?}", line!(), result);
      assert_eq!(buf.start_parsing_position, 117);
      assert_eq!(
        result,
        (
          ResponseState::ResponseWithBodyChunks(
            RStatusLine { version: Version::V11, status: 200, reason: String::from("OK") },
            Connection::new(),
            Chunk::Ended
          ),
          Some(74)
        )
      );
  }

  #[test]
  fn parse_incomplete_chunk_header_test() {
      setup_test_logger!();
      let input =
          b"HTTP/1.1 200 OK\r\n\
            Server: ABCD\r\n\
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
            \r\n";
      let initial = ResponseState::HasLength(
        RStatusLine { version: Version::V11, status: 200, reason: String::from("OK") },
        Connection::keep_alive(),
        LengthInformation::Chunked
      );
      let (pool, mut buf) = buf_with_capacity(2048);

      buf.write(&input[..74]).unwrap();
      buf.consume_parsed_data(72);
      //println!("parsing\n{}", buf.buffer.data().to_hex(16));
      let result = parse_response_until_stop(initial, None, &mut buf, false, "", "SOZUBALANCEID", None);
      println!("result: {:?}", result);
      println!("input length: {}", input.len());
      println!("initial input:\n{}", &input[..72].to_hex(8));
      println!("buffer output: {:?}", buf.output_queue);
      assert_eq!(buf.output_queue, vec!(OutputElement::Insert(vec!()), OutputElement::Slice(2)));
      assert_eq!(buf.start_parsing_position, 74);
      assert_eq!(
        result,
        (
          ResponseState::ResponseWithBodyChunks(
            RStatusLine { version: Version::V11, status: 200, reason: String::from("OK") },
            Connection::keep_alive(),
            Chunk::Initial
          ),
          Some(74)
        )
      );

      // we got the chunk header, but not the chunk content
      buf.write(&input[74..77]).unwrap();
      println!("parsing\n{}", buf.buffer.data().to_hex(16));
      let result = parse_response_until_stop(result.0, result.1, &mut buf, false, "", "SOZUBALANCEID", None);
      println!("result: {:?}", result);
      assert_eq!(buf.start_parsing_position, 81);
      assert_eq!(
        result,
        (
          ResponseState::ResponseWithBodyChunks(
            RStatusLine { version: Version::V11, status: 200, reason: String::from("OK") },
            Connection::keep_alive(),
            Chunk::Copying
          ),
          Some(74)
        )
      );


      //buf.consume(5);

      // the external code copied the chunk content directly, starting at next chunk end
      buf.write(&input[81..115]).unwrap();
      println!("parsing\n{}", buf.buffer.data().to_hex(16));
      let result = parse_response_until_stop(result.0, result.1, &mut buf, false, "", "SOZUBALANCEID", None);
      println!("result({}): {:?}", line!(), result);
      assert_eq!(buf.start_parsing_position, 115);
      assert_eq!(
        result,
        (
          ResponseState::ResponseWithBodyChunks(
            RStatusLine { version: Version::V11, status: 200, reason: String::from("OK") },
            Connection::keep_alive(),
            Chunk::CopyingLastHeader
          ),
          Some(74)
        )
      );
      buf.write(&input[115..]).unwrap();
      println!("parsing\n{}", &input[115..].to_hex(16));
      let result = parse_response_until_stop(result.0, result.1, &mut buf, false, "", "SOZUBALANCEID", None);
      println!("result({}): {:?}", line!(), result);
      assert_eq!(buf.start_parsing_position, 117);
      assert_eq!(
        result,
        (
          ResponseState::ResponseWithBodyChunks(
            RStatusLine { version: Version::V11, status: 200, reason: String::from("OK") },
            Connection::keep_alive(),
            Chunk::Ended
          ),
          Some(74)
        )
      );
  }

  #[test]
  fn parse_response_302() {
    let input =
        b"HTTP/1.1 302 Found\r\n\
          Cache-Control: no-cache\r\n\
          Content-length: 0\r\n\
          Location: https://www.clever-cloud.com\r\n\
          Connection: close\r\n\
          \r\n";
    let initial = ResponseState::Initial;
    let (pool, mut buf) = buf_with_capacity(2048);
    buf.write(&input[..]).unwrap();

    let new_header = b"Sozu-Id: 123456789\r\n";
    let result = parse_response_until_stop(initial, None, &mut buf, false, "Sozu-Id: 123456789\r\n", "SOZUBALANCEID", None);
    println!("result: {:?}", result);
    println!("buf:\n{}", buf.buffer.data().to_hex(16));
    println!("input length: {}", input.len());
    println!("initial input:\n{}", &input[..72].to_hex(8));
    println!("buffer output: {:?}", buf.output_queue);
    assert_eq!(buf.output_queue, vec!(
        OutputElement::Slice(20),
        OutputElement::Slice(25),
        OutputElement::Slice(19),
        OutputElement::Slice(40),
        OutputElement::Delete(19),
        OutputElement::Insert(Vec::from(&new_header[..])),
        OutputElement::Slice(2)));
    assert_eq!(buf.start_parsing_position, 125);
    assert_eq!(
      result,
      (
        ResponseState::ResponseWithBody(
          RStatusLine { version: Version::V11, status: 302, reason: String::from("Found") },
          Connection::close(),
          0
        ),
        Some(125)
      )
    );
  }

  #[test]
  fn parse_response_303() {
    let input =
        b"HTTP/1.1 303 See Other\r\n\
          Cache-Control: no-cache\r\n\
          Content-length: 0\r\n\
          Location: https://www.clever-cloud.com\r\n\
          Connection: close\r\n\
          \r\n";
    let initial = ResponseState::Initial;
    let (pool, mut buf) = buf_with_capacity(2048);
    buf.write(&input[..]).unwrap();

    let new_header = b"Sozu-Id: 123456789\r\n";
    let result = parse_response_until_stop(initial, None, &mut buf, false, "Sozu-Id: 123456789\r\n", "SOZUBALANCEID", None);
    println!("result: {:?}", result);
    println!("buf:\n{}", buf.buffer.data().to_hex(16));
    println!("input length: {}", input.len());
    println!("buffer output: {:?}", buf.output_queue);
    assert_eq!(buf.output_queue, vec!(
      OutputElement::Slice(24), OutputElement::Slice(25),
      OutputElement::Slice(19), OutputElement::Slice(40),
      OutputElement::Delete(19), OutputElement::Insert(Vec::from(&new_header[..])),
      OutputElement::Slice(2)));
    assert_eq!(buf.start_parsing_position, 129);
    assert_eq!(
      result,
      (
        ResponseState::ResponseWithBody(
          RStatusLine { version: Version::V11, status: 303, reason: String::from("See Other") },
          Connection::close(),
          0
        ),
        Some(129)
      )
    );
  }

  #[test]
  fn parse_response_304() {
      let input =
          b"HTTP/1.1 304 Not Modified\r\n\
            Connection: keep-alive\r\n\
            ETag: hello\r\n\
            \r\n";
      let initial = ResponseState::Initial;
      let is_head = true;
      let (pool, mut buf) = buf_with_capacity(2048);
      buf.write(&input[..]).unwrap();
      println!("buffer input: {:?}", buf.input_queue);

      //let result = parse_request(initial, input);
      let result = parse_response_until_stop(initial, None, &mut buf, is_head, "", "SOZUBALANCEID", None);
      println!("result: {:?}", result);
      println!("input length: {}", input.len());
      println!("buffer input: {:?}", buf.input_queue);
      println!("buffer output: {:?}", buf.output_queue);
      assert_eq!(buf.output_queue, vec!(
        OutputElement::Slice(27), OutputElement::Delete(24), OutputElement::Slice(13),
        OutputElement::Insert(vec!()), OutputElement::Slice(2)));
      assert_eq!(buf.start_parsing_position, 66);
      assert_eq!(
        result,
        (
          ResponseState::Response(
            RStatusLine { version: Version::V11, status: 304, reason: String::from("Not Modified") },
            Connection {
              keep_alive:  Some(true),
              has_upgrade: false,
              upgrade:     None,
              continues:   Continue::None,
              to_delete:   None,
              sticky_session: None,
            },
          ),
          Some(66)
        )
      );
  }

  #[test]
  fn hostname_parsing_test() {
    assert_eq!(
      hostname_and_port(&b"rust-test.cleverapps.io"[..]),
      Ok((&b""[..], (&b"rust-test.cleverapps.io"[..], None)))
    );

    assert_eq!(
      hostname_and_port(&b"localhost"[..]),
      Ok((&b""[..], (&b"localhost"[..], None)))
    );

    assert_eq!(
      hostname_and_port(&b"example.com:8080"[..]),
      Ok((&b""[..], (&b"example.com"[..], Some(&b"8080"[..]))))
    );
  }


  #[test]
  #[cfg(not(feature = "tolerant-http1-parser"))]
  fn hostname_parsing_underscore_test() {
    assert_eq!(
      hostname_and_port(&b"test_example.com"[..]),
       Err(Err::Error(error_position!(&b"_example.com"[..], ErrorKind::Eof)))
    );
  }

  #[test]
  #[cfg(feature = "tolerant-http1-parser")]
  fn hostname_parsing_underscore_test() {
    assert_eq!(
      hostname_and_port(&b"test_example.com"[..]),
      Ok((&b""[..], (&b"test_example.com"[..], None)))
    );
  }

  #[test]
  fn parse_state_head_with_content_length_test() {
      let input =
          b"HTTP/1.1 200 Ok\r\n\
            Content-Length: 200\r\n\
            \r\n";
      let initial = ResponseState::Initial;
      let is_head = true;
      let (pool, mut buf) = buf_with_capacity(2048);
      buf.write(&input[..]).unwrap();
      println!("buffer input: {:?}", buf.input_queue);

      //let result = parse_request(initial, input);
      let result = parse_response_until_stop(initial, None, &mut buf, is_head, "", "SOZUBALANCEID", None);
      println!("result: {:?}", result);
      println!("input length: {}", input.len());
      println!("buffer input: {:?}", buf.input_queue);
      println!("buffer output: {:?}", buf.output_queue);
      assert_eq!(buf.output_queue, vec!(
        OutputElement::Slice(17), OutputElement::Slice(21),
        OutputElement::Insert(vec!()), OutputElement::Slice(2)));
      assert_eq!(buf.start_parsing_position, 40);
      assert_eq!(
        result,
        (
          ResponseState::Response(
            RStatusLine { version: Version::V11, status: 200, reason: String::from("Ok") },
            Connection::new()
          ),
          Some(40)
        )
      );
  }

  #[test]
  fn parse_connection_upgrade_test() {
      let input =
          b"GET /index.html HTTP/1.1\r\n\
            Host: localhost:8888\r\n\
            User-Agent: curl/7.43.0\r\n\
            Accept: */*\r\n\
            Upgrade: WebSocket\r\n\
            Connection: keep-alive, Upgrade\r\n\
            \r\n";
      let initial = RequestState::Initial;
      let (pool, mut buf) = buf_with_capacity(2048);
      buf.write(&input[..]).unwrap();
      println!("buffer input: {:?}", buf.input_queue);

      //let result = parse_request(initial, input);
      let result = parse_request_until_stop(initial, None, &mut buf, "", "SOZUBALANCEID");
      println!("result: {:?}", result);
      println!("input length: {}", input.len());
      println!("buffer input: {:?}", buf.input_queue);
      println!("buffer output: {:?}", buf.output_queue);
      assert_eq!(buf.output_queue, vec!(
        OutputElement::Slice(26), OutputElement::Slice(22), OutputElement::Slice(25),
        OutputElement::Slice(13), OutputElement::Slice(20), OutputElement::Slice(33),
        OutputElement::Insert(vec!()), OutputElement::Slice(2)));
      assert_eq!(buf.start_parsing_position, 141);
      assert_eq!(
        result,
        (
          RequestState::Request(
            RRequestLine { method: Method::Get, uri: String::from("/index.html"), version: Version::V11 },
            Connection {
              keep_alive:  Some(true),
              has_upgrade: true,
              upgrade:     Some("WebSocket".to_string()),
              continues:   Continue::None,
              to_delete:   None,
              sticky_session: None
            },
            String::from("localhost:8888"),
          ),
          Some(141)
        )
      );
  }

  #[test]
  fn header_cookies_must_mutate() {
    let header = Header {
      name: b"Cookie",
      value: b"FOO=BAR"
    };

    assert!(header.must_mutate());
  }

  #[test]
  fn header_cookies_no_sticky() {
    let header_line1 = b"Cookie: FOO=BAR\r\n";
    let header_line2 = b"Cookie:FOO=BAR; BAR=FOO;SOZU=SOZU\r\n";
    let header_line3 = b"Cookie: FOO=BAR; BAR=FOO\r\n";

    let header1 = match message_header(header_line1) {
      Ok((_, header)) => header,
      _ => panic!()
    };

    let header2 = match message_header(header_line2) {
      Ok((_, header)) => header,
      _ => panic!()
    };

    let header3 = match message_header(header_line3) {
      Ok((_, header)) => header,
      _ => panic!()
    };

    let moves1 = header1.remove_sticky_cookie_in_request(header_line1, header_line1.len(), "SOZUBALANCEID");
    let moves2 = header2.remove_sticky_cookie_in_request(header_line2, header_line2.len(), "SOZUBALANCEID");
    let moves3 = header3.remove_sticky_cookie_in_request(header_line3, header_line3.len(), "SOZUBALANCEID");
    let expected1 = vec![BufferMove::Advance(header_line1.len())];
    let expected2 = vec![BufferMove::Advance(header_line2.len())];
    let expected3 = vec![BufferMove::Advance(header_line3.len())];

    assert_eq!(moves1, expected1);
    assert_eq!(moves2, expected2);
    assert_eq!(moves3, expected3);
  }

  #[test]
  fn header_cookies_sticky_only_cookie() {
    let header_line1 = b"Cookie: SOZUBALANCEID=0\r\n";
    let header_line2 = b"Cookie: SOZUBALANCEID=0;  \r\n";

    let header1 = match message_header(header_line1) {
      Ok((_, header)) => header,
      _ => panic!()
    };

    let header2 = match message_header(header_line2) {
      Ok((_, header)) => header,
      _ => panic!()
    };

    let moves1 = header1.remove_sticky_cookie_in_request(header_line1, header_line1.len(), "SOZUBALANCEID");
    let moves2 = header2.remove_sticky_cookie_in_request(header_line2, header_line2.len(), "SOZUBALANCEID");
    let expected1 = vec![BufferMove::Delete(header_line1.len())];
    let expected2 = vec![BufferMove::Delete(header_line2.len())];

    assert_eq!(moves1, expected1);
    assert_eq!(moves2, expected2);
  }

  #[test]
  fn header_cookies_sticky_start() {
    let header_line1 = b"Cookie:SOZUBALANCEID=0;FOO=BAR\r\n";
    let header_line2 = b"Cookie: SOZUBALANCEID=0;  FOO=BAR\r\n";
    let header_line3 = b"Cookie: SOZUBALANCEID=0; FOO=BAR\r\n";

    let header1 = match message_header(header_line1) {
      Ok((_, header)) => header,
      _ => panic!()
    };

    let header2 = match message_header(header_line2) {
      Ok((_, header)) => header,
      _ => panic!()
    };

    let header3 = match message_header(header_line3) {
      Ok((_, header)) => header,
      _ => panic!()
    };

    let moves1 = header1.remove_sticky_cookie_in_request(header_line1, header_line1.len(), "SOZUBALANCEID");
    let moves2 = header2.remove_sticky_cookie_in_request(header_line2, header_line2.len(), "SOZUBALANCEID");
    let moves3 = header3.remove_sticky_cookie_in_request(header_line3, header_line3.len(), "SOZUBALANCEID");
    let expected1 = vec![BufferMove::Advance(7), BufferMove::Delete(16), BufferMove::Advance(7), BufferMove::Advance(2)];
    let expected2 = vec![BufferMove::Advance(8), BufferMove::Delete(18), BufferMove::Advance(7), BufferMove::Advance(2)];
    let expected3 = vec![BufferMove::Advance(8), BufferMove::Delete(17), BufferMove::Advance(7), BufferMove::Advance(2)];

    assert_eq!(moves1, expected1);
    assert_eq!(moves2, expected2);
    assert_eq!(moves3, expected3);
  }

  #[test]
  fn header_cookies_sticky_middle() {
    let header_line1 = b"Cookie: BAR=FOO; SOZUBALANCEID=0;FOO=BAR\r\n";
    let header_line2 = b"Cookie:BAR=FOO;SOZUBALANCEID=0;  FOO=BAR\r\n";
    let header_line3 = b"Cookie: BAR=FOO; SOZUBALANCEID=0; FOO=BAR\r\n";

    let header1 = match message_header(header_line1) {
      Ok((_, header)) => header,
      _ => panic!()
    };

    let header2 = match message_header(header_line2) {
      Ok((_, header)) => header,
      _ => panic!()
    };

    let header3 = match message_header(header_line3) {
      Ok((_, header)) => header,
      _ => panic!()
    };

    let moves1 = header1.remove_sticky_cookie_in_request(header_line1, header_line1.len(), "SOZUBALANCEID");
    let moves2 = header2.remove_sticky_cookie_in_request(header_line2, header_line2.len(), "SOZUBALANCEID");
    let moves3 = header3.remove_sticky_cookie_in_request(header_line3, header_line3.len(), "SOZUBALANCEID");
    let expected1 = vec![BufferMove::Advance(8), BufferMove::Advance(9), BufferMove::Delete(16), BufferMove::Advance(7), BufferMove::Advance(2)];
    let expected2 = vec![BufferMove::Advance(7), BufferMove::Advance(8), BufferMove::Delete(18), BufferMove::Advance(7), BufferMove::Advance(2)];
    let expected3 = vec![BufferMove::Advance(8), BufferMove::Advance(9), BufferMove::Delete(17), BufferMove::Advance(7), BufferMove::Advance(2)];

    assert_eq!(moves1, expected1);
    assert_eq!(moves2, expected2);
    assert_eq!(moves3, expected3);
  }

  #[test]
  fn header_cookies_sticky_end() {
    let header_line1 = b"Cookie: BAR=FOO;  SOZUBALANCEID=0\r\n";
    let header_line2 = b"Cookie:BAR=FOO;SOZUBALANCEID=0;  \r\n";
    let header_line3 = b"Cookie: BAR=FOO; SOZUBALANCEID=0  \r\n";
    let header_line4 = b"Cookie: BAR=FOO; SOZUBALANCEID=0\r\n";

    let header1 = match message_header(header_line1) {
      Ok((_, header)) => header,
      _ => panic!()
    };

    let header2 = match message_header(header_line2) {
      Ok((_, header)) => header,
      _ => panic!()
    };

    let header3 = match message_header(header_line3) {
      Ok((_, header)) => header,
      _ => panic!()
    };

    let header4 = match message_header(header_line4) {
      Ok((_, header)) => header,
      _ => panic!()
    };

    let moves1 = header1.remove_sticky_cookie_in_request(header_line1, header_line1.len(), "SOZUBALANCEID");
    let moves2 = header2.remove_sticky_cookie_in_request(header_line2, header_line2.len(), "SOZUBALANCEID");
    let moves3 = header3.remove_sticky_cookie_in_request(header_line3, header_line3.len(), "SOZUBALANCEID");
    let moves4 = header4.remove_sticky_cookie_in_request(header_line4, header_line4.len(), "SOZUBALANCEID");
    let expected1 = vec![BufferMove::Advance(8), BufferMove::Advance(7), BufferMove::Delete(3), BufferMove::Delete(15), BufferMove::Advance(2)];
    let expected2 = vec![BufferMove::Advance(7), BufferMove::Advance(7), BufferMove::Delete(1), BufferMove::Delete(18), BufferMove::Advance(2)];
    let expected3 = vec![BufferMove::Advance(8), BufferMove::Advance(7), BufferMove::Delete(2), BufferMove::Delete(17), BufferMove::Advance(2)];
    let expected4 = vec![BufferMove::Advance(8), BufferMove::Advance(7), BufferMove::Delete(2), BufferMove::Delete(15), BufferMove::Advance(2)];

    assert_eq!(moves1, expected1);
    assert_eq!(moves2, expected2);
    assert_eq!(moves3, expected3);
    assert_eq!(moves4, expected4);
  }
}

#[cfg(all(feature = "unstable", test))]
mod bench {
  use super::*;
  use test::Bencher;
  use buffer_queue::BufferQueue;
  use std::io::Write;

  #[bench]
  fn req_bench(b: &mut Bencher) {
    let data = b"GET /reddit-init.en-us.O1zuMqOOQvY.js HTTP/1.1\r\n\
                 Host: www.redditstatic.com\r\n\
                 User-Agent: Mozilla/5.0 (Macintosh; Intel Mac OS X 10.8; rv:15.0) Gecko/20100101 Firefox/15.0.1\r\n\
                 Accept: */*\r\n\
                 Accept-Language: en-us,en;q=0.5\r\n\
                 Accept-Encoding: gzip, deflate\r\n\
                 Connection: keep-alive\r\n\
                 Referer: http://www.reddit.com/\r\n\r\n";

    let mut buf = BufferQueue::with_capacity(data.len());

    buf.write(&data[..]).unwrap();
    let res1 = parse_request_until_stop(RequestState::Initial, None, &mut buf, "", "");
    println!("res: {:?}", res1);

    b.bytes = data.len() as u64;
    b.iter(||{
      buf.input_queue.clear();
      buf.output_queue.clear();
      buf.parsed_position = 0;
      buf.start_parsing_position = 0;
      buf.sliced_input(data.len());

      let initial = RequestState::Initial;
      let res2 = parse_request_until_stop(initial, None, &mut buf, "", "");
      assert_eq!(res1, res2);
    });
  }

  #[bench]
  fn parse_req_bench(b: &mut Bencher) {
    let data = b"GET /reddit-init.en-us.O1zuMqOOQvY.js HTTP/1.1\r\n\
                 Host: www.redditstatic.com\r\n\
                 User-Agent: Mozilla/5.0 (Macintosh; Intel Mac OS X 10.8; rv:15.0) Gecko/20100101 Firefox/15.0.1\r\n\
                 Accept: */*\r\n\
                 Accept-Language: en-us,en;q=0.5\r\n\
                 Accept-Encoding: gzip, deflate\r\n\
                 Connection: keep-alive\r\n\
                 Referer: http://www.reddit.com/\r\n\r\n";

    b.bytes = data.len() as u64;
    b.iter(||{
      let mut current_state = RequestState::Initial;
      let mut position      = 0;
      loop {
        let test_position = position;
        let (mv, new_state) = parse_request(current_state, &data[test_position..], "");
        current_state = new_state;

        if let BufferMove::Delete(end) = mv {
          position += end;
        }
        match mv {
          BufferMove::Advance(sz) => {
            position+=sz;
          },
          BufferMove::Delete(_) => {},
          _ => break
        }

        match current_state {
          RequestState::Request(_,_,_) | RequestState::RequestWithBody(_,_,_,_) |
            RequestState::Error(_,_,_,_,_) | RequestState::RequestWithBodyChunks(_,_,_,Chunk::Ended) => break,
          _ => ()
        }

        if position >= data.len() { break }
        //println!("pos: {}, len: {}, state: {:?}, remaining:\n{}", position, data.len(), current_state, (&data[position..]).to_hex(16));
      }
    });
  }
}
