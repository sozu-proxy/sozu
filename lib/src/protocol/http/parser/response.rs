use buffer_queue::BufferQueue;
use protocol::http::StickySession;

use nom::{HexDisplay,IResult,Offset,Err};

use std::str::{self, from_utf8};
use std::convert::From;

use super::{BufferMove, LengthInformation, RStatusLine, Connection, Chunk, HeaderValue, TransferEncodingValue,
  Version, Header, message_header, status_line, crlf,};


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
