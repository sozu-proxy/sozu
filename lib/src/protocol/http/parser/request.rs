
use sozu_command::buffer::fixed::Buffer;
use buffer_queue::BufferQueue;

use nom::{HexDisplay,IResult,Offset,Err};

use url::Url;

use std::str;
use std::convert::From;

use super::{BufferMove, LengthInformation, RRequestLine, Connection, Chunk, Host, HeaderValue, TransferEncodingValue,
  Method, Version, Continue, Header, message_header, request_line, crlf,};

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

  pub fn get_uri(&self) -> Option<&str> {
    match *self {
      RequestState::HasRequestLine(ref rl, _)         |
      RequestState::HasHost(ref rl, _, _)             |
      RequestState::HasHostAndLength(ref rl, _, _, _) |
      RequestState::Request(ref rl , _, _)            |
      RequestState::RequestWithBody(ref rl, _, _, _)  |
      RequestState::RequestWithBodyChunks(ref rl, _, _, _) => Some(rl.uri.as_str()),
      RequestState::Error(ref rl, _, _, _, _)              => rl.as_ref().map(|r| r.uri.as_str()),
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
      RequestState::Error(ref rl, _, _, _, _)              => rl.as_ref(),
      _                                                    => None
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
      RequestState::Error(_, ref conn, _, _, _)       => conn.as_ref(),
      _                                                      => None
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
