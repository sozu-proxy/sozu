use std::{fmt::{self, Write}, str::from_utf8_unchecked};

use nom::{
    bytes::{self, complete::take_while},
    character::{complete::digit1, is_alphanumeric},
    combinator::opt,
    error::{Error, ErrorKind},
    sequence::preceded,
    Err, IResult,
};

pub fn compare_no_case(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }

    left.iter().zip(right).all(|(a, b)| match (*a, *b) {
        (0..=64, 0..=64) | (91..=96, 91..=96) | (123..=255, 123..=255) => a == b,
        (65..=90, 65..=90) | (97..=122, 97..=122) | (65..=90, 97..=122) | (97..=122, 65..=90) => {
            *a | 0b00_10_00_00 == *b | 0b00_10_00_00
        }
        _ => false,
    })
}

#[derive(PartialEq, Eq, Debug, Clone)]
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
        if compare_no_case(s, b"GET") {
            Method::Get
        } else if compare_no_case(s, b"POST") {
            Method::Post
        } else if compare_no_case(s, b"HEAD") {
            Method::Head
        } else if compare_no_case(s, b"OPTIONS") {
            Method::Options
        } else if compare_no_case(s, b"PUT") {
            Method::Put
        } else if compare_no_case(s, b"DELETE") {
            Method::Delete
        } else if compare_no_case(s, b"TRACE") {
            Method::Trace
        } else if compare_no_case(s, b"CONNECT") {
            Method::Connect
        } else {
            Method::Custom(String::from(unsafe { from_utf8_unchecked(s) }))
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
            Method::Custom(s) => write!(f, "{s}"),
        }
    }
}

#[cfg(feature = "tolerant-http1-parser")]
fn is_hostname_char(i: u8) -> bool {
    is_alphanumeric(i) ||
  // the domain name should not start with a hyphen or dot
  // but is it important here, since we will match this to
  // the list of accepted clusters?
  // BTW each label between dots has a max of 63 chars,
  // and the whole domain should not be larger than 253 chars
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
  // the list of accepted clusters?
  // BTW each label between dots has a max of 63 chars,
  // and the whole domain should not be larger than 253 chars
  b"-.".contains(&i)
}

// FIXME: convert port to u16 here
#[allow(clippy::type_complexity)]
pub fn hostname_and_port(i: &[u8]) -> IResult<&[u8], (&[u8], Option<&[u8]>)> {
    let (i, host) = take_while(is_hostname_char)(i)?;
    let (i, port) = opt(preceded(bytes::complete::tag(":"), digit1))(i)?;

    if !i.is_empty() {
        return Err(Err::Error(Error::new(i, ErrorKind::Eof)));
    }
    Ok((i, (host, port)))
}

pub fn view(buf: &[u8], size: usize, points: &[usize]) -> String {
    let mut view = String::new();
    let mut end = 0;
    for (i, point) in points.iter().enumerate() {
        let start = if end + size < *point {
            view.push_str("... ");
            point - size
        } else {
            end
        };
        let stop = if i + 1 < points.len() {
            points[i + 1]
        } else {
            buf.len()
        };
        end = if point + size > stop {
            stop
        } else {
            point + size
        };
        for element in &buf[start..*point] {
            let _ = view.write_fmt(format_args!("{element:02X} "));
        }
        view.push_str("| ");
        for element in &buf[*point..end] {
            let _ = view.write_fmt(format_args!("{element:02X} "));
        }
    }
    if end < buf.len() {
        view.push_str("...")
    }
    view
}
