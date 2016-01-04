#![allow(unused_imports)]
#![allow(dead_code)]

use network::buffer::Buffer;

use nom::{HexDisplay,IResult};
use nom::IResult::*;
use nom::Err::*;

use nom::{digit,is_alphanumeric};

use std::str;

// Primitives
fn is_token_char(i: u8) -> bool {
  is_alphanumeric(i) ||
  b"!#$%&'*+-.^_`|~".contains(&i)
}
named!(pub token, take_while!(is_token_char));

fn is_ws(i: u8) -> bool {
  i == ' ' as u8 && i == '\t' as u8
}
named!(pub repeated_ws, take_while!(is_ws));
named!(pub obsolete_ws, chain!(repeated_ws?, || { &b" "[..] }));

named!(pub sp<char>, char!(' '));
named!(pub crlf, tag!("\r\n"));

fn is_vchar(i: u8) -> bool {
  i > 32 && i <= 126
}
fn is_vchar_or_ws(i: u8) -> bool {
  is_vchar(i) || is_ws(i)
}

fn is_header_value_char(i: u8) -> bool {
  i >= 32 && i <= 126
}

named!(pub vchar_1, take_while!(is_vchar));
named!(pub vchar_ws_1, take_while!(is_vchar_or_ws));


#[derive(PartialEq,Debug)]
pub struct RequestLine<'a> {
    pub method: &'a [u8],
    pub uri: &'a [u8],
    pub version: [&'a [u8];2]
}

#[derive(PartialEq,Debug,Clone)]
pub struct RRequestLine {
    pub method: String,
    pub uri: String,
    pub version: String
}

impl RRequestLine {
  pub fn from_request_line(r: RequestLine) -> Option<RRequestLine> {
    if let Ok(method) = str::from_utf8(r.method) {
      if let Ok(uri) = str::from_utf8(r.uri) {
        if let Ok(version1) = str::from_utf8(r.version[0]) {
          if let Ok(version2) = str::from_utf8(r.version[1]) {
            Some(RRequestLine {
              method:  String::from(method),
              uri:     String::from(uri),
              version: String::from(version1) + version2
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
    } else {
      None
    }
  }
}

#[derive(PartialEq,Debug)]
pub struct StatusLine<'a> {
    pub version: [&'a [u8];2],
    pub status: &'a [u8],
    pub reason: &'a [u8],
}

#[derive(PartialEq,Debug,Clone)]
pub struct RStatusLine {
    pub version: String,
    pub status:  u8,
    pub reason:  String,
}

impl RStatusLine {
  pub fn from_status_line(r: StatusLine) -> Option<RStatusLine> {
    if let Ok(status_str) = str::from_utf8(r.status) {
      if let Ok(status) = status_str.parse::<u8>() {
        if let Ok(reason) = str::from_utf8(r.reason) {
          if let Ok(version1) = str::from_utf8(r.version[0]) {
            if let Ok(version2) = str::from_utf8(r.version[1]) {
              Some(RStatusLine {
                version: String::from(version1) + version2,
                status:  status,
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
      } else {
        None
      }
    } else {
      None
    }
  }
}

named!(pub http_version<[&[u8];2]>,
       chain!(
        tag!("HTTP/") ~
        major: digit ~
        tag!(".") ~
        minor: digit, || {
            [major, minor] // ToDo do we need it?
        }
       )
);

named!(pub request_line<RequestLine>,
       chain!(
        method: token ~
        sp ~
        uri: vchar_1 ~ // ToDo proper URI parsing?
        sp ~
        version: http_version ~
        crlf, || {
            RequestLine {
              method: method,
              uri: uri,
              version: version
            }
        }
       )
);

named!(pub status_line<StatusLine>,
  chain!(
    version: http_version ~
             sp           ~
    status:  take!(3)     ~
             sp           ~
    reason:  token        ~
             crlf         ,
    || {
      StatusLine {
        version: version,
        status: status,
        reason: reason,
      }
    }
  )
);

#[derive(PartialEq,Debug)]
pub struct Header<'a> {
    pub name: &'a [u8],
    pub value: &'a [u8]
}

named!(pub message_header<Header>,
       chain!(
         name: token ~
         tag!(":") ~
         sp ~ // ToDo make it optional
         value: take_while!(is_header_value_char) ~ // ToDo handle folding?
         crlf, || {
           Header {
            name: name,
            value: value
           }
         }
       )
);

pub fn comma_separated_header_value(input:&[u8]) -> Option<Vec<&[u8]>> {
  let res: IResult<&[u8], Vec<&[u8]>> = separated_list!(input, char!(','), vchar_1);
  if let IResult::Done(_,o) = res {
    Some(o)
  } else {
    None
  }
}

named!(pub headers< Vec<Header> >, terminated!(many0!(message_header), opt!(crlf)));

#[derive(PartialEq,Debug)]
pub struct Request<'a> {
    pub request_line: RequestLine<'a>,
    pub headers: Vec<Header<'a>>
}

named!(pub request_head<Request>,
       chain!(
        rl: request_line ~
        hs: many0!(message_header) ~
        crlf, || {
          Request {
            request_line: rl,
            headers: hs
          }
        }
       )
);

#[derive(PartialEq,Debug)]
pub struct Response<'a> {
    pub status_line: StatusLine<'a>,
    pub headers: Vec<Header<'a>>
}

named!(pub response_head<Response>,
       chain!(
        sl: status_line ~
        hs: many0!(message_header) ~
        crlf, || {
          Response {
            status_line: sl,
            headers: hs
          }
        }
       )
);

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
pub enum HeaderResult<T> {
  Value(T),
  None,
  Error
}

impl<'a> Header<'a> {
  pub fn value(&self) -> HeaderValue {
    match self.name {
      b"Host" => {
        if let Some(s) = str::from_utf8(self.value).map(|s| String::from(s)).ok() {
          HeaderValue::Host(s)
        } else {
          HeaderValue::Error
        }
      },
      b"Content-Length" => {
        if let Ok(l) = str::from_utf8(self.value) {
          if let Some(length) = l.parse().ok() {
             return HeaderValue::ContentLength(length)
          }
        }
        HeaderValue::Error
      },
      b"Transfer-Encoding" => {
        match self.value {
          b"chunked"  => HeaderValue::Encoding(TransferEncodingValue::Chunked),
          b"compress" => HeaderValue::Encoding(TransferEncodingValue::Compress),
          b"deflate"  => HeaderValue::Encoding(TransferEncodingValue::Deflate),
          b"gzip"     => HeaderValue::Encoding(TransferEncodingValue::Gzip),
          b"identity" => HeaderValue::Encoding(TransferEncodingValue::Identity),
          _           => HeaderValue::Encoding(TransferEncodingValue::Unknown)
        }
      },
      b"Connection" => {
        match comma_separated_header_value(self.value) {
          Some(tokens) => HeaderValue::Connection(tokens),
          None         => HeaderValue::Error
        }
      }
      _ => HeaderValue::Other(self.name, self.value)
    }
  }
}

pub enum HeaderValue<'a> {
  Host(String),
  ContentLength(usize),
  Encoding(TransferEncodingValue),
  Connection(Vec<&'a [u8]>),
  Other(&'a[u8],&'a[u8]),
  Error
}

pub type Host = String;

#[derive(Debug,Clone,PartialEq)]
pub enum ErrorState {
  InvalidHttp,
  MissingHost
}

#[derive(Debug,Clone,PartialEq)]
pub enum LengthInformation {
  Length(usize),
  Chunked,
  Compressed
}

#[derive(Debug,Clone,PartialEq)]
pub enum Connection {
  KeepAlive,
  Close
}

#[derive(Debug,Clone,PartialEq)]
pub enum RequestState {
  Initial,
  Error(ErrorState),
  HasRequestLine(RRequestLine, Connection),
  HasHost(RRequestLine, Connection, Host),
  HasLength(RRequestLine, Connection, LengthInformation),
  HasHostAndLength(RRequestLine, Connection, Host, LengthInformation),
  Request(RRequestLine, Connection, Host),
  RequestWithBody(RRequestLine, Connection, Host, LengthInformation),
  Proxying(RRequestLine, Connection, Host)
  //Proxying(RRequestLine, Host, LengthInformation, BackendToken)
}

impl RequestState {
  pub fn has_host(&self) -> bool {
    match *self {
      RequestState::HasHost(_, _, _)            |
      RequestState::Request(_, _, _)            |
      RequestState::RequestWithBody(_, _, _, _) |
      RequestState::Proxying(_, _, _)           => true,
      _                                      => false
    }
  }

  pub fn is_proxying(&self) -> bool {
    match *self {
      RequestState::Request(_, _, _) | RequestState::RequestWithBody(_, _, _, _) => true,
      _                                                                          => false
    }
  }

  pub fn get_host(&self) -> Option<String> {
    match *self {
      RequestState::HasHost(_, _, ref host)            |
      RequestState::Request(_, _, ref host)            |
      RequestState::RequestWithBody(_, _, ref host, _) |
      RequestState::Proxying(_, _, ref host)    => Some(host.clone()),
      _                                      => None
    }
  }

  pub fn get_uri(&self) -> Option<String> {
    match *self {
      RequestState::HasRequestLine(ref rl, _)        |
      RequestState::HasHost(ref rl, _, _)            |
      RequestState::Request(ref rl , _, _)           |
      RequestState::RequestWithBody(ref rl, _, _, _) |
      RequestState::Proxying(ref rl, _, _)           => Some(rl.uri.clone()),
      _                                           => None
    }
  }

  pub fn get_request_line(&self) -> Option<RRequestLine> {
    match *self {
      RequestState::HasRequestLine(ref rl, _)        |
      RequestState::HasHost(ref rl, _, _)            |
      RequestState::Request(ref rl, _, _)            |
      RequestState::RequestWithBody(ref rl, _, _, _) |
      RequestState::Proxying(ref rl, _, _)           => Some(rl.clone()),
      _                                           => None
    }
  }

  pub fn get_keep_alive(&self) -> Option<Connection> {
    match *self {
      RequestState::HasRequestLine(_, ref conn)         |
      RequestState::HasHost(_, ref conn, _)             |
      RequestState::HasLength(_, ref conn, _)           |
      RequestState::HasHostAndLength(_, ref conn, _, _) |
      RequestState::Request(_, ref conn, _)             |
      RequestState::RequestWithBody(_, ref conn, _, _)  |
      RequestState::Proxying(_, ref conn, _)            => Some(conn.clone()),
      _                                              => None
    }
  }

  pub fn should_copy(&self, position: usize) -> Option<usize> {
    match *self {
      RequestState::RequestWithBody(_, _, _, LengthInformation::Length(l)) => Some(position + l),
      RequestState::Request(_, _, _)                                       => Some(position),
      _                                                                 => None
    }
  }

  pub fn should_keep_alive(&self) -> bool {
    match self.get_keep_alive() {
      Some(Connection::KeepAlive) => true,
      Some(Connection::Close)     => false,
      None                        => false
    }
  }
}

#[derive(Debug,Clone,PartialEq)]
pub enum ResponseState {
  Initial,
  Error(ErrorState),
  HasStatusLine(RStatusLine, Connection),
  HasLength(RStatusLine, Connection, LengthInformation),
  Response(RStatusLine, Connection),
  ResponseWithBody(RStatusLine, Connection, LengthInformation),
  Proxying(RStatusLine, Connection)
  //Proxying(RRequestLine, Host, LengthInformation, BackendToken)
}

impl ResponseState {
  pub fn is_proxying(&self) -> bool {
    match *self {
      ResponseState::Response(_, _) | ResponseState::ResponseWithBody(_, _, _) => true,
      _                                                                        => false
    }
  }

  pub fn get_status_line(&self) -> Option<RStatusLine> {
    match *self {
      ResponseState::HasStatusLine(ref sl, _)       |
      ResponseState::HasLength(ref sl, _, _)        |
      ResponseState::Response(ref sl, _)            |
      ResponseState::ResponseWithBody(ref sl, _, _) |
      ResponseState::Proxying(ref sl, _)            => Some(sl.clone()),
      _                                             => None
    }
  }

  pub fn get_keep_alive(&self) -> Option<Connection> {
    match *self {
      ResponseState::HasStatusLine(_, ref conn)       |
      ResponseState::HasLength(_, ref conn, _)        |
      ResponseState::Response(_, ref conn)            |
      ResponseState::ResponseWithBody(_, ref conn, _) |
      ResponseState::Proxying(_, ref conn)            => Some(conn.clone()),
      _                                               => None
    }
  }

  pub fn should_copy(&self, position: usize) -> Option<usize> {
    match *self {
      ResponseState::ResponseWithBody(_, _, LengthInformation::Length(l)) => Some(position + l),
      ResponseState::Response(_, _)                                       => Some(position),
      _                                                                   => None
    }
  }

  pub fn should_keep_alive(&self) -> bool {
    match self.get_keep_alive() {
      Some(Connection::KeepAlive) => true,
      Some(Connection::Close)     => false,
      None                        => false
    }
  }
}

#[derive(Debug,PartialEq)]
pub struct HttpState {
  pub req_position: usize,
  pub res_position: usize,
  pub request:      RequestState,
  pub response:     ResponseState
}

impl HttpState {
  pub fn new() -> HttpState {
    HttpState {
      req_position: 0,
      res_position: 0,
      request:      RequestState::Initial,
      response:     ResponseState::Initial
    }
  }

  pub fn reset(&mut self) {
    self.req_position = 0;
    self.res_position = 0;
    self.request      = RequestState::Initial;
    self.response     = ResponseState::Initial;
  }

  pub fn has_host(&self) -> bool {
    self.request.has_host()
  }

  pub fn is_front_error(&self) -> bool {
    if let RequestState::Error(_) = self.request {
      true
    } else {
      false
    }
  }

  pub fn is_front_proxying(&self) -> bool {
    self.request.is_proxying()
  }

  pub fn get_host(&self) -> Option<String> {
    self.request.get_host()
  }

  pub fn get_uri(&self) -> Option<String> {
    self.request.get_uri()
  }

  pub fn get_request_line(&self) -> Option<RRequestLine> {
    self.request.get_request_line()
  }

  pub fn get_front_keep_alive(&self) -> Option<Connection> {
    self.request.get_keep_alive()
  }

  pub fn front_should_copy(&self) -> Option<usize> {
    self.request.should_copy(self.req_position)
  }

  pub fn front_should_keep_alive(&self) -> bool {
    self.request.should_keep_alive()
  }

  pub fn is_back_error(&self) -> bool {
    if let ResponseState::Error(_) = self.response {
      true
    } else {
      false
    }
  }

  pub fn is_back_proxying(&self) -> bool {
    self.response.is_proxying()
  }

  pub fn get_status_line(&self) -> Option<RStatusLine> {
    self.response.get_status_line()
  }

  pub fn get_back_keep_alive(&self) -> Option<Connection> {
    self.response.get_keep_alive()
  }

  pub fn back_should_copy(&self) -> Option<usize> {
    self.response.should_copy(self.res_position)
  }

  pub fn back_should_keep_alive(&self) -> bool {
    self.response.should_keep_alive()
  }
}

#[derive(Debug,PartialEq)]
pub enum BufferMove {
  None,
  Advance(usize),
  // start, length
  Delete(usize, usize)
}

pub fn default_request_result<O>(state: &RequestState, res: IResult<&[u8], O>) -> (BufferMove, RequestState) {
  match res {
    IResult::Error(_)      => (BufferMove::None, RequestState::Error(ErrorState::InvalidHttp)),
    IResult::Incomplete(_) => (BufferMove::None, state.clone()),
    _                      => unreachable!()
  }
}

pub fn validate_request_header(state: RequestState, header: &Header) -> RequestState {
  match header.value() {
    HeaderValue::Host(host) => {
      match state {
        RequestState::HasRequestLine(rl, conn) => RequestState::HasHost(rl, conn, host),
        RequestState::HasLength(rl, conn, l)   => RequestState::HasHostAndLength(rl, conn, host, l),
        _                                   => RequestState::Error(ErrorState::InvalidHttp)
      }
    },
    HeaderValue::ContentLength(sz) => {
      match state {
        RequestState::HasRequestLine(rl, conn) => RequestState::HasLength(rl, conn, LengthInformation::Length(sz)),
        RequestState::HasHost(rl, conn, host)  => RequestState::HasHostAndLength(rl, conn, host, LengthInformation::Length(sz)),
        _                                   => RequestState::Error(ErrorState::InvalidHttp)
      }
    },
    HeaderValue::Encoding(TransferEncodingValue::Chunked) => {
      match state {
        RequestState::HasRequestLine(rl, conn)            => RequestState::HasLength(rl, conn, LengthInformation::Chunked),
        RequestState::HasHost(rl, conn, host)             => RequestState::HasHostAndLength(rl, conn, host, LengthInformation::Chunked),
        // Transfer-Encoding takes the precedence on Content-Length
        RequestState::HasHostAndLength(rl, conn, host,
           LengthInformation::Length(_))         => RequestState::HasHostAndLength(rl, conn, host, LengthInformation::Chunked),
        RequestState::HasHostAndLength(_, _, _, _) => RequestState::Error(ErrorState::InvalidHttp),
        _                                        => RequestState::Error(ErrorState::InvalidHttp)
      }
    },
    // FIXME: for now, we don't remember if we cancel indiations from a previous Connection Header
    HeaderValue::Connection(c) => {
      let mut conn = state.get_keep_alive().unwrap_or(Connection::KeepAlive);
      for value in c {
      println!("GOT Connection header: {:?}", str::from_utf8(value).unwrap());
        match value {
          b"close"      => conn = Connection::Close,
          b"keep-alive" => conn = Connection::KeepAlive,
          _             => {}
        }
      }
      match state {
        RequestState::HasRequestLine(rl, _)                 => RequestState::HasRequestLine(rl, conn),
        RequestState::HasHost(rl, _, host)                  => RequestState::HasHost(rl, conn, host),
        RequestState::HasLength(rl, _, length)              => RequestState::HasLength(rl, conn, length),
        RequestState::HasHostAndLength(rl, _, host, length) => RequestState::HasHostAndLength(rl, conn, host, length),
        RequestState::Request(rl, _, host)                  => RequestState::Request(rl, conn, host),
        RequestState::RequestWithBody(rl, _, host, length)  => RequestState::RequestWithBody(rl, conn, host, length),
        RequestState::Proxying(rl, _, host)                 => RequestState::Proxying(rl, conn, host),
        _                                                => RequestState::Error(ErrorState::InvalidHttp)
      }
    },

    // FIXME: there should be an error for unsupported encoding
    HeaderValue::Encoding(_) => RequestState::Error(ErrorState::InvalidHttp),
    HeaderValue::Other(_,_)  => state.clone(),
    HeaderValue::Error       => RequestState::Error(ErrorState::InvalidHttp)
  }
}

pub fn parse_header(buf: &mut Buffer, state: RequestState) -> IResult<&[u8], RequestState> {
  match message_header(buf.data()) {
    IResult::Incomplete(i)   => IResult::Incomplete(i),
    IResult::Error(e)        => IResult::Error(e),
    IResult::Done(i, header) => IResult::Done(i, validate_request_header(state, &header))
  }
}

pub fn parse_request(state: &RequestState, buf: &[u8]) -> (BufferMove, RequestState) {
  match *state {
    RequestState::Initial => {
      match request_line(buf) {
        IResult::Done(i, r)    => {
          if let Some(rl) = RRequestLine::from_request_line(r) {
            let conn = if rl.version == "11" {
              Connection::KeepAlive
            } else {
              Connection::Close
            };
            let s = (BufferMove::Advance(buf.offset(i)), RequestState::HasRequestLine(rl, conn));
            s
          } else {
            (BufferMove::None, RequestState::Error(ErrorState::InvalidHttp))
          }
        },
        res => default_request_result(state, res)
      }
    },
    RequestState::HasRequestLine(_, _) => {
      match message_header(buf) {
        IResult::Done(i, header) => {
          (BufferMove::Advance(buf.offset(i)), validate_request_header(state.clone(), &header))
        },
        res => default_request_result(state, res)
      }
    },
    RequestState::HasHost(ref rl, ref conn, ref h) => {
      match message_header(buf) {
        IResult::Done(i, header) => {
          (BufferMove::Advance(buf.offset(i)), validate_request_header(state.clone(), &header))
        },
        IResult::Incomplete(_) => (BufferMove::None, state.clone()),
        IResult::Error(_)      => {
          match crlf(buf) {
            IResult::Done(i, _) => {
              println!("headers parsed, stopping");
              (BufferMove::Advance(buf.offset(i)), RequestState::Request(rl.clone(), conn.clone(), h.clone()))
            },
            res => {
              println!("HasHost could not parse header for input:\n{}\n", buf.to_hex(8));
              default_request_result(state, res)
            }
          }
        }
      }
    },
    RequestState::HasHostAndLength(ref rl, ref conn, ref h, ref l) => {
      match message_header(buf) {
        IResult::Done(i, header) => {
          (BufferMove::Advance(buf.offset(i)), validate_request_header(state.clone(), &header))
        },
        IResult::Incomplete(_) => (BufferMove::None, state.clone()),
        IResult::Error(_)      => {
          match crlf(buf) {
            IResult::Done(i, _) => {
              println!("headers parsed, stopping");
              (BufferMove::Advance(buf.offset(i)),
               RequestState::RequestWithBody(rl.clone(), conn.clone(), h.clone(), l.clone())
              )
            },
            res => {
              println!("HasHostAndLength could not parse header for input:\n{}\n", buf.to_hex(8));
              default_request_result(state, res)
            }
          }
        }
      }
    },
    _ => {
      println!("unimplemented state: {:?}", state);
      (BufferMove::None, RequestState::Error(ErrorState::InvalidHttp))
    }
  }
}

pub fn default_response_result<O>(state: &ResponseState, res: IResult<&[u8], O>) -> (BufferMove, ResponseState) {
  match res {
    IResult::Error(_)      => (BufferMove::None, ResponseState::Error(ErrorState::InvalidHttp)),
    IResult::Incomplete(_) => (BufferMove::None, state.clone()),
    _                      => unreachable!()
  }
}

pub fn validate_response_header(state: ResponseState, header: &Header) -> ResponseState {
  match header.value() {
    HeaderValue::ContentLength(sz) => {
      match state {
        ResponseState::HasStatusLine(sl, conn) => ResponseState::HasLength(sl, conn, LengthInformation::Length(sz)),
        _                                      => ResponseState::Error(ErrorState::InvalidHttp)
      }
    },
    HeaderValue::Encoding(TransferEncodingValue::Chunked) => {
      match state {
        ResponseState::HasStatusLine(rl, conn) => ResponseState::HasLength(rl, conn, LengthInformation::Chunked),
        _                                      => ResponseState::Error(ErrorState::InvalidHttp)
      }
    },
    // FIXME: for now, we don't remember if we cancel indiations from a previous Connection Header
    HeaderValue::Connection(c) => {
      let mut conn = state.get_keep_alive().unwrap_or(Connection::KeepAlive);
      for value in c {
      println!("GOT Connection header: {:?}", str::from_utf8(value).unwrap());
        match value {
          b"close"      => conn = Connection::Close,
          b"keep-alive" => conn = Connection::KeepAlive,
          _             => {}
        }
      }
      match state {
        ResponseState::HasStatusLine(rl, _)     => ResponseState::HasStatusLine(rl, conn),
        ResponseState::HasLength(rl, _, length) => ResponseState::HasLength(rl, conn, length),
        _                                       => ResponseState::Error(ErrorState::InvalidHttp)
      }
    },

    // FIXME: there should be an error for unsupported encoding
    HeaderValue::Encoding(_) => ResponseState::Error(ErrorState::InvalidHttp),
    HeaderValue::Host(_)     => ResponseState::Error(ErrorState::InvalidHttp),
    HeaderValue::Other(_,_)  => state.clone(),
    HeaderValue::Error       => ResponseState::Error(ErrorState::InvalidHttp)
  }
}

pub fn parse_response(state: &ResponseState, buf: &[u8]) -> (BufferMove, ResponseState) {
  match *state {
    ResponseState::Initial => {
      match status_line(buf) {
        IResult::Done(i, r)    => {
          if let Some(rl) = RStatusLine::from_status_line(r) {
            let conn = if rl.version == "11" {
              Connection::KeepAlive
            } else {
              Connection::Close
            };
            let s = (BufferMove::Advance(buf.offset(i)), ResponseState::HasStatusLine(rl, conn));
            s
          } else {
            (BufferMove::None, ResponseState::Error(ErrorState::InvalidHttp))
          }
        },
        res => default_response_result(state, res)
      }
    },
    ResponseState::HasStatusLine(ref sl, ref conn) => {
      match message_header(buf) {
        IResult::Done(i, header) => {
          (BufferMove::Advance(buf.offset(i)), validate_response_header(state.clone(), &header))
        },
        IResult::Incomplete(_) => (BufferMove::None, state.clone()),
        IResult::Error(_)      => {
          match crlf(buf) {
            IResult::Done(i, _) => {
              println!("headers parsed, stopping");
              (BufferMove::Advance(buf.offset(i)), ResponseState::Response(sl.clone(), conn.clone()))
            },
            res => {
              println!("HasResponseLine could not parse header for input:\n{}\n", buf.to_hex(8));
              default_response_result(state, res)
            }
          }
        }
      }
    },
    ResponseState::HasLength(ref sl, ref conn, ref length) => {
      match message_header(buf) {
        IResult::Done(i, header) => {
          (BufferMove::Advance(buf.offset(i)), validate_response_header(state.clone(), &header))
        },
        IResult::Incomplete(_) => (BufferMove::None, state.clone()),
        IResult::Error(_)      => {
          match crlf(buf) {
            IResult::Done(i, _) => {
              println!("headers parsed, stopping");
              (BufferMove::Advance(buf.offset(i)), ResponseState::ResponseWithBody(sl.clone(), conn.clone(), length.clone()))
            },
            res => {
              println!("HasResponseLine could not parse header for input:\n{}\n", buf.to_hex(8));
              default_response_result(state, res)
            }
          }
        }
      }
    },
    _ => {
      println!("unimplemented state: {:?}", state);
      (BufferMove::None, ResponseState::Error(ErrorState::InvalidHttp))
    }
  }
}

pub fn parse_request_until_stop(rs: &HttpState, buf: &mut Buffer) -> HttpState {
  let mut current_state = rs.request.clone();
  let mut position      = rs.req_position;
  //let (mut position, mut current_state) = state;
  loop {
    //println!("pos[{}]: {:?}", position, current_state);
    let (mv, new_state) = parse_request(&current_state, &buf.data()[position..]);
    //println!("input:\n{}\nmv: {:?}, new state: {:?}\n", (&buf.data()[position..]).to_hex(8), mv, new_state);
    //println!("mv: {:?}, new state: {:?}\n", mv, new_state);
    current_state = new_state;
    if let BufferMove::Advance(sz) = mv {
      assert!(sz != 0, "buffer move should not be 0");
      position+=sz;
    } else {
      break;
    }
    match current_state {
      RequestState::Request(_,_,_) | RequestState::RequestWithBody(_,_,_,_) |
        RequestState::Error(_) => break,
      _ => ()
    }
  }
  HttpState {
    req_position: position,
    res_position: rs.res_position,
    request:  current_state,
    response: rs.response.clone()
  }
}

pub fn parse_response_until_stop(rs: &HttpState, buf: &mut Buffer) -> HttpState {
  let mut current_state = rs.response.clone();
  let mut position      = rs.res_position;
  loop {
    //println!("pos[{}]: {:?}", position, current_state);
    let (mv, new_state) = parse_response(&current_state, &buf.data()[position..]);
    //println!("input:\n{}\nmv: {:?}, new state: {:?}\n", (&buf.data()[position..]).to_hex(8), mv, new_state);
    //println!("mv: {:?}, new state: {:?}\n", mv, new_state);
    current_state = new_state;
    if let BufferMove::Advance(sz) = mv {
      assert!(sz != 0, "buffer move should not be 0");
      position+=sz;
    } else {
      break;
    }
    match current_state {
      ResponseState::Response(_,_) | ResponseState::ResponseWithBody(_,_,_) |
        ResponseState::Error(_) => break,
      _ => ()
    }
  }
  HttpState {
    req_position: rs.req_position,
    res_position: position,
    request:  rs.request.clone(),
    response: current_state
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use nom::IResult::*;
  use nom::HexDisplay;
  use nom::Err::*;
  use network::buffer::Buffer;
  use std::str;
  use std::io::Write;


  #[test]
  fn request_line_test() {
      let input = b"GET /index.html HTTP/1.1\r\n";
      let result = request_line(input);
      let expected = RequestLine {
        method: b"GET",
        uri: b"/index.html",
        version: [b"1", b"1"]
      };

      assert_eq!(result, Done(&[][..], expected));
  }

  #[test]
  fn header_test() {
      let input = b"Accept: */*\r\n";
      let result = message_header(input);
      let expected = Header {
        name: b"Accept",
        value: b"*/*"
      };

      assert_eq!(result, Done(&b""[..], expected))
  }

  #[test]
  fn request_head_test() {
      let input =
          b"GET /index.html HTTP/1.1\r\n\
            Host: localhost:8888\r\n\
            User-Agent: curl/7.43.0\r\n\
            Accept: */*\r\n\
            \r\n";
      let result = request_head(input);
      let expected = Request {
        request_line: RequestLine {
        method: b"GET",
        uri: b"/index.html",
        version: [b"1", b"1"]
      },
        headers: vec!(
          Header {
            name: b"Host",
            value: b"localhost:8888"
          },
          Header {
            name: b"User-Agent",
            value: b"curl/7.43.0"
          },
          Header {
            name: b"Accept",
            value: b"*/*"
          }
        )
      };

      assert_eq!(result, Done(&b""[..], expected))
  }

  #[test]
  fn content_length_test() {
      let input =
          b"GET /index.html HTTP/1.1\r\n\
            Host: localhost:8888\r\n\
            User-Agent: curl/7.43.0\r\n\
            Accept: */*\r\n\
            Content-Length: 200\r\n\
            \r\n";
      let result = request_head(input);
      let expected = Request {
        request_line: RequestLine {
        method: b"GET",
        uri: b"/index.html",
        version: [b"1", b"1"]
      },
        headers: vec!(
          Header {
            name: b"Host",
            value: b"localhost:8888"
          },
          Header {
            name: b"User-Agent",
            value: b"curl/7.43.0"
          },
          Header {
            name: b"Accept",
            value: b"*/*"
          },
          Header {
            name: b"Content-Length",
            value: b"200"
          }
        )
      };

      assert_eq!(result, Done(&b""[..], expected));
  }

  #[test]
  fn header_user_agent() {
      let input = b"User-Agent: Mozilla/5.0 (Macintosh; Intel Mac OS X 10.10; rv:44.0) Gecko/20100101 Firefox/44.0\r\n";

      let result = message_header(input);
      assert_eq!(
        result,
        Done(&b""[..], Header {
          name: b"User-Agent",
          value: b"Mozilla/5.0 (Macintosh; Intel Mac OS X 10.10; rv:44.0) Gecko/20100101 Firefox/44.0"
        })
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
      let initial = HttpState::new();
      let mut buf = Buffer::with_capacity(2048);
      buf.write(&input[..]);

      //let result = parse_request(&initial, input);
      let result = parse_request_until_stop(&initial, &mut buf);
      println!("result: {:?}", result);
      assert_eq!(
        result,
        HttpState {
          req_position: 109,
          res_position: 0,
          request: RequestState::RequestWithBody(
            RRequestLine { method: String::from("GET"), uri: String::from("/index.html"), version: String::from("11") },
            Connection::KeepAlive,
            String::from("localhost:8888"),
            LengthInformation::Length(200)
          ),
          response: ResponseState::Initial
        }
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
      let initial = HttpState {
        req_position: 26,
        res_position: 0,
        request:  RequestState::HasRequestLine(
          RRequestLine {
            method: String::from("GET"),
            uri: String::from("/index.html"),
            version: String::from("11")
          },
          Connection::KeepAlive
        ),
        response: ResponseState::Initial
      };

      let mut buf = Buffer::with_capacity(2048);
      buf.write(&input[..]);

      //let result = parse_request(&initial, input);
      let result = parse_request_until_stop(&initial, &mut buf);
      println!("result: {:?}", result);
      assert_eq!(
        result,
        HttpState {
          req_position: 109,
          res_position: 0,
          request:    RequestState::RequestWithBody(
            RRequestLine { method: String::from("GET"), uri: String::from("/index.html"), version: String::from("11") },
            Connection::KeepAlive,
            String::from("localhost:8888"),
            LengthInformation::Length(200)
          ),
          response: ResponseState::Initial
        }
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
      let initial = HttpState::new();
      let mut buf = Buffer::with_capacity(2048);
      buf.write(&input[..]);

      //let result = parse_request(&initial, input);
      let result = parse_request_until_stop(&initial, &mut buf);
      println!("result: {:?}", result);
      assert_eq!(
        result,
        HttpState {
          req_position: 116,
          res_position: 0,
          request:    RequestState::RequestWithBody(
            RRequestLine { method: String::from("GET"), uri: String::from("/index.html"), version: String::from("11") },
            Connection::KeepAlive,
            String::from("localhost:8888"),
            LengthInformation::Chunked
          ),
          response: ResponseState::Initial
        }
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
      let initial = HttpState::new();
      let mut buf = Buffer::with_capacity(2048);
      buf.write(&input[..]);

      let result = parse_request_until_stop(&initial, &mut buf);
      println!("result: {:?}", result);
      assert_eq!(
        result,
        HttpState {
          req_position: 128,
          res_position: 0,
          request: RequestState::Error(ErrorState::InvalidHttp),
          response: ResponseState::Initial
        }
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
      let initial = HttpState::new();
      let mut buf = Buffer::with_capacity(2048);
      buf.write(&input[..]);

      //let result = parse_request(&initial, input);
      let result = parse_request_until_stop(&initial, &mut buf);
      println!("result: {:?}", result);
      assert_eq!(
        result,
        HttpState {
          req_position: 136,
          res_position: 0,
          request:  RequestState::RequestWithBody(
            RRequestLine { method: String::from("GET"), uri: String::from("/index.html"), version: String::from("11") },
            Connection::KeepAlive,
            String::from("localhost:8888"),
            LengthInformation::Chunked
          ),
          response: ResponseState::Initial
        }
      );
  }

  #[test]
  fn parse_request_without_length() {
      let input =
          b"GET / HTTP/1.1\r\n\
            Host: localhost:8888\r\n\
            Connection: close\r\n\
            \r\n";
      let initial = HttpState::new();
      let mut buf = Buffer::with_capacity(2048);
      buf.write(&input[..]);

      //let result = parse_request(&initial, input);
      let result = parse_request_until_stop(&initial, &mut buf);
      println!("result: {:?}", result);
      assert_eq!(
        result,
        HttpState {
          req_position: 59,
          res_position: 0,
          request:  RequestState::Request(
            RRequestLine { method: String::from("GET"), uri: String::from("/"), version: String::from("11") },
            Connection::Close,
            String::from("localhost:8888")
          ),
          response: ResponseState::Initial
        }
      );
  }

  // HTTP 1.0 is connection close by default
  #[test]
  fn parse_request_http_1_0_connection_close() {
      let input =
          b"GET / HTTP/1.0\r\n\
            Host: localhost:8888\r\n\
            \r\n";
      let initial = HttpState::new();
      let mut buf = Buffer::with_capacity(2048);
      buf.write(&input[..]);

      //let result = parse_request(&initial, input);
      let result = parse_request_until_stop(&initial, &mut buf);
      println!("result: {:?}", result);
      assert_eq!(
        result,
        HttpState {
          req_position: 40,
          res_position: 0,
          request:  RequestState::Request(
            RRequestLine { method: String::from("GET"), uri: String::from("/"), version: String::from("10") },
            Connection::Close,
            String::from("localhost:8888")
          ),
          response: ResponseState::Initial
        }
      );
  }

  #[test]
  fn parse_request_http_1_0_connection_keep_alive() {
      let input =
          b"GET / HTTP/1.0\r\n\
            Host: localhost:8888\r\n\
            Connection: keep-alive\r\n\
            \r\n";
      let initial = HttpState::new();
      let mut buf = Buffer::with_capacity(2048);
      buf.write(&input[..]);

      //let result = parse_request(&initial, input);
      let result = parse_request_until_stop(&initial, &mut buf);
      println!("result: {:?}", result);
      assert_eq!(
        result,
        HttpState {
          req_position: 64,
          res_position: 0,
          request:  RequestState::Request(
            RRequestLine { method: String::from("GET"), uri: String::from("/"), version: String::from("10") },
            Connection::KeepAlive,
            String::from("localhost:8888")
          ),
          response: ResponseState::Initial
        }
      );
  }

  #[test]
  fn parse_request_http_1_1_connection_close() {
      let input =
          b"GET / HTTP/1.1\r\n\
            Connection: close\r\n\
            Host: localhost:8888\r\n\
            \r\n";
      let initial = HttpState::new();
      let mut buf = Buffer::with_capacity(2048);
      buf.write(&input[..]);

      //let result = parse_request(&initial, input);
      let result = parse_request_until_stop(&initial, &mut buf);
      println!("result: {:?}", result);
      assert_eq!(
        result,
        HttpState {
          req_position: 59,
          res_position: 0,
          request:  RequestState::Request(
            RRequestLine { method: String::from("GET"), uri: String::from("/"), version: String::from("11") },
            Connection::Close,
            String::from("localhost:8888")
          ),
          response: ResponseState::Initial
        }
      );
  }
  /*
  use std::str::from_utf8;
  use std::io::Write;

  #[test]
  fn handle_request_line_test() {
     let input =
          b"GET /index.html HTTP/1.1\r\n\
            Host: localhost:8888\r\n\
            User-Agent: curl/7.43.0\r\n\
            Content-Length: 8\r\n\
            \r\nabcdefgj";
      let mut buf = Buffer::with_capacity(2048);
      buf.write(&input[..]);
      let start_state = RequestState::Initial;

      let next_state = handle_request_partial(&start_state, &mut buf, 0);

      println!("next state: {:?}", next_state);
      println!("remaining: {}", from_utf8(buf.data()).unwrap());
      assert!(false);
  }
  */
}
