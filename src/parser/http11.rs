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

named!(pub http_version<[&[u8];2]>,
       chain!(
        tag!("HTTP/") ~
        major: digit ~
        tag!(".") ~
        minor: digit, || {
            [minor, major] // ToDo do we need it?
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
pub enum HttpState {
  Initial,
  Error(ErrorState),
  HasRequestLine(RRequestLine),
  HasHost(RRequestLine, Host),
  HasLength(RRequestLine, LengthInformation),
  HasHostAndLength(RRequestLine, Host, LengthInformation),
  HeadersParsed(RRequestLine, Host, LengthInformation),
  Proxying(RRequestLine, Host)
  //Proxying(RRequestLine, Host, LengthInformation, BackendToken)
}

impl HttpState {
  pub fn get_host(&self) -> Option<String> {
    match *self {
      HttpState::HasHost(_, ref host) |
      HttpState::Proxying(_, ref host)    => Some(host.clone()),
      _                                   => None
    }
  }

  pub fn get_uri(&self) -> Option<String> {
    match *self {
      HttpState::HasRequestLine(ref rl) |
      HttpState::HasHost(ref rl,_)      |
      HttpState::Proxying(ref rl, _)         => Some(rl.uri.clone()),
      _                                      => None
    }
  }

  pub fn get_request_line(&self) -> Option<RRequestLine> {
    match *self {
      HttpState::HasRequestLine(ref rl) |
      HttpState::HasHost(ref rl,_)      |
      HttpState::Proxying(ref rl, _)         => Some(rl.clone()),
      _                                      => None
    }
  }
}

#[derive(Debug,PartialEq)]
pub enum BufferMove {
  None,
  Advance(usize),
  // start, length
  Delete(usize, usize)
}

pub fn default_response<O>(state: &HttpState, res: IResult<&[u8], O>) -> (BufferMove, HttpState) {
  match res {
    IResult::Error(_)      => (BufferMove::None, HttpState::Error(ErrorState::InvalidHttp)),
    IResult::Incomplete(_) => (BufferMove::None, state.clone()),
    _                      => unreachable!()
  }
}

pub fn validate_request_header(state: HttpState, header: &Header) -> HttpState {
  match header.value() {
    HeaderValue::Host(host) => {
      match state {
        HttpState::HasRequestLine(rl) => HttpState::HasHost(rl, host),
        HttpState::HasLength(rl, l)   => HttpState::HasHostAndLength(rl, host, l),
        _                             => HttpState::Error(ErrorState::InvalidHttp)
      }
    },
    HeaderValue::ContentLength(sz) => {
      match state {
        HttpState::HasRequestLine(rl) => HttpState::HasLength(rl, LengthInformation::Length(sz)),
        HttpState::HasHost(rl, host)  => HttpState::HasHostAndLength(rl, host, LengthInformation::Length(sz)),
        _                             => HttpState::Error(ErrorState::InvalidHttp)
      }
    },
    HeaderValue::Encoding(TransferEncodingValue::Chunked) => {
      match state {
        HttpState::HasRequestLine(rl)            => HttpState::HasLength(rl, LengthInformation::Chunked),
        HttpState::HasHost(rl, host)             => HttpState::HasHostAndLength(rl, host, LengthInformation::Chunked),
        // Transfer-Encoding takes the precedence on Content-Length
        HttpState::HasHostAndLength(rl, host,
           LengthInformation::Length(_))         => HttpState::HasHostAndLength(rl, host, LengthInformation::Chunked),
        HttpState::HasHostAndLength(rl, host, _) => HttpState::Error(ErrorState::InvalidHttp),
        _                                        => HttpState::Error(ErrorState::InvalidHttp)
      }
    },
    HeaderValue::Connection(headers) => {
      state.clone()
    },

    // FIXME: there should be an error for unsupported encoding
    HeaderValue::Encoding(_) => HttpState::Error(ErrorState::InvalidHttp),
    HeaderValue::Other(_,_)  => state.clone(),
    HeaderValue::Error       => HttpState::Error(ErrorState::InvalidHttp)
  }
}

pub fn parse_header(buf: &mut Buffer, state: HttpState) -> IResult<&[u8], HttpState> {
  match message_header(buf.data()) {
    IResult::Incomplete(i)   => IResult::Incomplete(i),
    IResult::Error(e)        => IResult::Error(e),
    IResult::Done(i, header) => IResult::Done(i, validate_request_header(state, &header))
  }
}

pub fn parse_request(state: &HttpState, buf: &[u8]) -> (BufferMove, HttpState) {
  match *state {
    HttpState::Initial => {
      match request_line(buf) {
        IResult::Done(i, r)    => {
          if let Some(rl) = RRequestLine::from_request_line(r) {
            let s = (BufferMove::Advance(buf.offset(i)), HttpState::HasRequestLine(rl));
            //parse_request(&s.1, buf)
            s
          } else {
            (BufferMove::None, HttpState::Error(ErrorState::InvalidHttp))
          }
        },
        res => default_response(state, res)
      }
    },
    HttpState::HasRequestLine(ref rl) | HttpState::HasHost(ref rl, _) => {
      match headers(buf) {
        IResult::Done(i, v)    => {
          let mut current_state = state.clone();
          for header in &v {
            current_state = validate_request_header(current_state, header);
          }

          (BufferMove::Advance(buf.offset(i)),current_state)
        },
        res => default_response(state, res)
      }
    },
    HttpState::HasHostAndLength(ref rl, _, _) => {
      match headers(buf) {
        IResult::Done(i, v)    => {
          let mut current_state = state.clone();
          for header in &v {
            current_state = validate_request_header(current_state, header);
          }

          (BufferMove::Advance(buf.offset(i)),current_state)
        },
        res => default_response(state, res)
      }
    },
    _ => {
      println!("unimplemented state: {:?}", state);
      (BufferMove::None, HttpState::Error(ErrorState::InvalidHttp))
    }
  }
}

pub fn parse_until_stop(state: &HttpState, buf: &[u8]) -> (usize, HttpState) {
  let mut current_state = state.clone();
  let mut position = 0usize;
  loop {
    println!("pos[{}]: {:?}", position, current_state);
    let (mv, new_state) = parse_request(&current_state, &buf[position..]);
    println!("mv: {:?}", mv);
    current_state = new_state;
    if let BufferMove::Advance(sz) = mv {
      assert!(sz != 0, "buffer move should not be 0");
      position+=sz;
    } else {
      break;
    }
  }
  (position, current_state)
}

#[cfg(test)]
mod tests {
  use super::*;
  use nom::IResult::*;
  use nom::HexDisplay;
  use nom::Err::*;
  use network::buffer::Buffer;


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

  use std::str;
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
      let initial = HttpState::Initial;

      //let result = parse_request(&initial, input);
      let result = parse_until_stop(&initial, input);
      println!("result: {:?}", result);
      assert_eq!(
        result,
        (109, HttpState::HasHostAndLength(
          RRequestLine { method: String::from("GET"), uri: String::from("/index.html"), version: String::from("11") },
          String::from("localhost:8888"),
          LengthInformation::Length(200)
        ))
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
      let initial = HttpState::Initial;

      //let result = parse_request(&initial, input);
      let result = parse_until_stop(&initial, input);
      println!("result: {:?}", result);
      assert_eq!(
        result,
        (116, HttpState::HasHostAndLength(
          RRequestLine { method: String::from("GET"), uri: String::from("/index.html"), version: String::from("11") },
          String::from("localhost:8888"),
          LengthInformation::Chunked
        ))
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
      let initial = HttpState::Initial;

      let result = parse_until_stop(&initial, input);
      println!("result: {:?}", result);
      assert_eq!(result, (130, HttpState::Error(ErrorState::InvalidHttp)));
  }

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
      let initial = HttpState::Initial;

      //let result = parse_request(&initial, input);
      let result = parse_until_stop(&initial, input);
      println!("result: {:?}", result);
      assert_eq!(
        result,
        (136, HttpState::HasHostAndLength(
          RRequestLine { method: String::from("GET"), uri: String::from("/index.html"), version: String::from("11") },
          String::from("localhost:8888"),
          LengthInformation::Chunked
        ))
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
      let start_state = HttpState::Initial;

      let next_state = handle_request_partial(&start_state, &mut buf, 0);

      println!("next state: {:?}", next_state);
      println!("remaining: {}", from_utf8(buf.data()).unwrap());
      assert!(false);
  }
  */
}
