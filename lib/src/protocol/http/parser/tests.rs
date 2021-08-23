use super::*;
use nom::{error::ErrorKind, Err, HexDisplay};
#[cfg(test)]
use pretty_assertions::assert_eq;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use super::super::buffer::http_buf_with_capacity;
#[cfg(test)]
use crate::protocol::http::AddedHeader;

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
        version: Version::V11,
    };

    assert_eq!(result, Ok((&[][..], expected)));
}

#[test]
fn header_test() {
    let input = b"Accept: */*\r\n";
    let result = message_header(input);
    let expected = Header {
        name: b"Accept",
        value: b"*/*",
    };

    assert_eq!(result, Ok((&b""[..], expected)))
}

#[test]
#[cfg(not(feature = "tolerant-http1-parser"))]
fn header_iso_8859_1_test() {
    let input = "Test: Aéo\r\n";
    let result = message_header(input.as_bytes());

    assert_eq!(
        result,
        Err(Err::Error(error_position!(
            "éo\r\n".as_bytes(),
            ErrorKind::Tag
        )))
    );
}

#[test]
#[cfg(feature = "tolerant-http1-parser")]
fn header_iso_8859_1_test() {
    let input = "Test: Aéo\r\n";
    let result = message_header(input.as_bytes());
    let expected = Header {
        name: b"Test",
        value: "Aéo".as_bytes(),
    };

    assert_eq!(result, Ok((&b""[..], expected)))
}

#[test]
fn header_without_space_test() {
    let input = b"Host:localhost\r\n";
    let result = message_header(input);
    let expected = Header {
        name: b"Host",
        value: b"localhost",
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
    let input = b"GET http://example.com:8888/index.html HTTP/1.1\r\n\
          User-Agent: curl/7.43.0\r\n\
          Accept: */*\r\n\
          Content-Length: 200\r\n\
          \r\n";
    let initial = RequestState::Initial;
    let (_pool, mut buf) = http_buf_with_capacity(2048);
    buf.write(&input[..]).unwrap();
    //println!("buffer input: {:?}", buf.input_queue);

    //let result = parse_request(initial, input);
    let (mut state, header_end) =
        parse_request_until_stop(initial, None, &mut buf, None, "SOZUBALANCEID");
    println!("result: {:?}", state);
    println!("input length: {}", input.len());
    state = state.consume(header_end.unwrap(), &mut buf);
    assert_eq!(
        (state, header_end),
        (
            RequestState::RequestWithBody {
                request: RRequestLine {
                    method: Method::Get,
                    uri: String::from("http://example.com:8888/index.html"),
                    version: Version::V11
                },
                connection: Connection::keep_alive(),
                host: String::from("example.com"),
                length: 200,
            },
            Some(110)
        )
    );
    /*
    println!("buffer input: {:?}", buf.input_queue);
    println!("buffer output: {:?}", buf.output_queue);
    assert_eq!(buf.output_queue, vec!(
      OutputElement::Slice(49), OutputElement::Slice(25),
      OutputElement::Slice(13), OutputElement::Slice(21),
      OutputElement::Insert(vec!()), OutputElement::Slice(202)));
    assert_eq!(buf.start_parsing_position, 310);
    */
}

#[test]
fn parse_state_host_in_url_conflict_test() {
    let input = b"GET http://example.com:8888/index.html HTTP/1.1\r\n\
          Host: test.org\r\n\
          User-Agent: curl/7.43.0\r\n\
          Accept: */*\r\n\
          Content-Length: 200\r\n\
          \r\n";
    let initial = RequestState::Initial;
    let (_pool, mut buf) = http_buf_with_capacity(2048);
    buf.write(&input[..]).unwrap();
    //println!("buffer input: {:?}", buf.input_queue);

    //let result = parse_request(initial, input);
    let result = parse_request_until_stop(initial, None, &mut buf, None, "SOZUBALANCEID");
    println!("result: {:?}", result);
    println!("input length: {}", input.len());
    /*
    println!("buffer input: {:?}", buf.input_queue);
    println!("buffer output: {:?}", buf.output_queue);
    assert_eq!(buf.output_queue, vec!(
      OutputElement::Slice(49), OutputElement::Slice(16)));
    assert_eq!(buf.start_parsing_position, 65);
    */
    assert!(result.0.is_front_error());
    if let RequestState::Error { host, .. } = result.0 {
        assert_eq!(host, Some(String::from("example.com")));
    } else {
        panic!("unexpected error: {:?}", result);
    }

    //panic!();
}

#[test]
fn parse_state_content_length_test() {
    let input = b"GET /index.html HTTP/1.1\r\n\
          Host: localhost:8888\r\n\
          User-Agent: curl/7.43.0\r\n\
          Accept: */*\r\n\
          Content-Length: 200\r\n\
          \r\n";
    let initial = RequestState::Initial;
    let (_pool, mut buf) = http_buf_with_capacity(2048);
    buf.write(&input[..]).unwrap();
    //println!("buffer input: {:?}", buf.input_queue);

    //let result = parse_request(initial, input);
    let (mut state, header_end) =
        parse_request_until_stop(initial, None, &mut buf, None, "SOZUBALANCEID");
    println!("result: {:?}", state);
    println!("input length: {}", input.len());
    state = state.consume(header_end.unwrap(), &mut buf);
    assert_eq!(
        (state, header_end),
        (
            RequestState::RequestWithBody {
                request: RRequestLine {
                    method: Method::Get,
                    uri: String::from("/index.html"),
                    version: Version::V11
                },
                connection: Connection::keep_alive(),
                host: String::from("localhost:8888"),
                length: 200,
            },
            Some(109)
        )
    );
    /*
    println!("buffer input: {:?}", buf.input_queue);
    println!("buffer output: {:?}", buf.output_queue);
    assert_eq!(buf.output_queue, vec!(
      OutputElement::Slice(26), OutputElement::Slice(22), OutputElement::Slice(25),
      OutputElement::Slice(13), OutputElement::Slice(21),
      OutputElement::Insert(vec!()), OutputElement::Slice(202)));
    assert_eq!(buf.start_parsing_position, 309);
    */
}

#[test]
fn parse_state_content_length_partial() {
    let input = b"GET /index.html HTTP/1.1\r\n\
          Host: localhost:8888\r\n\
          User-Agent: curl/7.43.0\r\n\
          Accept: */*\r\n\
          Content-Length: 200\r\n\
          \r\n";
    let initial = RequestState::Initial;
    /*
    let initial = RequestState::HasRequestLine(
        RRequestLine {
          method: Method::Get,
          uri: String::from("/index.html"),
          version: Version::V11
        },
        Connection::keep_alive()
      );
    */

    let (_pool, mut buf) = http_buf_with_capacity(2048);
    println!("skipping input:\n{}", (&input[..26]).to_hex(16));
    buf.write(&input[..]).unwrap();
    //println!("unparsed data:\n{}", buf.unparsed_data().to_hex(16));
    /*   println!("buffer output: {:?}", buf.output_queue);
        buf.consume_parsed_data(26);
        buf.slice_output(26);
        println!("unparsed data after consume(26):\n{}", buf.unparsed_data().to_hex(16));
        println!("buffer output: {:?}", buf.output_queue);
    */

    let (mut state, header_end) =
        parse_request_until_stop(initial, None, &mut buf, None, "SOZUBALANCEID");
    //println!("unparsed data after parsing:\n{}", buf.unparsed_data().to_hex(16));
    println!("result: {:?}", state);
    state = state.consume(header_end.unwrap(), &mut buf);
    assert_eq!(
        (state, header_end),
        (
            RequestState::RequestWithBody {
                request: RRequestLine {
                    method: Method::Get,
                    uri: String::from("/index.html"),
                    version: Version::V11
                },
                connection: Connection::keep_alive(),
                host: String::from("localhost:8888"),
                length: 200,
            },
            Some(109)
        )
    );
    /*
    println!("input length: {}", input.len());
    println!("buffer output: {:?}", buf.output_queue);
    assert_eq!(buf.output_queue, vec!(
      OutputElement::Slice(26), OutputElement::Slice(22),
      OutputElement::Slice(25), OutputElement::Slice(13),
      OutputElement::Slice(21), OutputElement::Insert(vec!()),
      OutputElement::Slice(202)));
    assert_eq!(buf.start_parsing_position, 309);
    */
}

#[test]
fn parse_state_chunked_test() {
    let input = b"GET /index.html HTTP/1.1\r\n\
          Host: localhost:8888\r\n\
          User-Agent: curl/7.43.0\r\n\
          Transfer-Encoding: chunked\r\n\
          Accept: */*\r\n\
          \r\n";
    let initial = RequestState::Initial;
    let (_pool, mut buf) = http_buf_with_capacity(2048);
    buf.write(&input[..]).unwrap();

    //let result = parse_request(initial, input);
    let (mut state, header_end) =
        parse_request_until_stop(initial, None, &mut buf, None, "SOZUBALANCEID");
    println!("result: {:?}", state);
    state = state.consume(header_end.unwrap(), &mut buf);
    assert_eq!(
        (state, header_end),
        (
            RequestState::RequestWithBodyChunks {
                request: RRequestLine {
                    method: Method::Get,
                    uri: String::from("/index.html"),
                    version: Version::V11
                },
                connection: Connection::keep_alive(),
                host: String::from("localhost:8888"),
                chunk: Chunk::Initial,
            },
            Some(116)
        )
    );
    //assert_eq!(buf.start_parsing_position, 116);
}

#[test]
fn parse_state_duplicate_content_length_test() {
    let input = b"GET /index.html HTTP/1.1\r\n\
          Host: localhost:8888\r\n\
          User-Agent: curl/7.43.0\r\n\
          Content-Length: 120\r\n\
          Accept: */*\r\n\
          Content-Length: 200\r\n\
          \r\n";
    let initial = RequestState::Initial;

    let (_pool, mut buf) = http_buf_with_capacity(2048);
    buf.write(&input[..]).unwrap();

    let result = parse_request_until_stop(initial, None, &mut buf, None, "SOZUBALANCEID");
    println!("result: {:?}", result);
    //assert_eq!(buf.start_parsing_position, 128);
    assert!(result.0.is_front_error());
    /*assert_eq!(
      result,
      (
        RequestState::Error(Some(
          RRequestLine { method: Method::Get, uri: String::from("/index.html"), version: Version::V11 },
        ),
          Some(Connection::new()), Some(String::from("localhost:8888")),
          Some(LengthInformation::Length(120)), None),
        None
      )
    );*/
}

// if there was a content-length, the chunked transfer encoding takes precedence
#[test]
fn parse_state_content_length_and_chunked_test() {
    let input = b"GET /index.html HTTP/1.1\r\n\
          Host: localhost:8888\r\n\
          User-Agent: curl/7.43.0\r\n\
          Content-Length: 10\r\n\
          Transfer-Encoding: chunked\r\n\
          Accept: */*\r\n\
          \r\n";
    let initial = RequestState::Initial;
    let (_pool, mut buf) = http_buf_with_capacity(2048);
    buf.write(&input[..]).unwrap();

    //let result = parse_request(initial, input);
    let (mut state, header_end) =
        parse_request_until_stop(initial, None, &mut buf, None, "SOZUBALANCEID");
    println!("result: {:?}", state);
    //assert_eq!(buf.start_parsing_position, 136);
    println!("result: {:?}", state);
    state = state.consume(header_end.unwrap(), &mut buf);
    assert_eq!(
        (state, header_end),
        (
            RequestState::RequestWithBodyChunks {
                request: RRequestLine {
                    method: Method::Get,
                    uri: String::from("/index.html"),
                    version: Version::V11
                },
                connection: Connection::keep_alive(),
                host: String::from("localhost:8888"),
                chunk: Chunk::Initial,
            },
            Some(136)
        )
    );
}

#[test]
fn parse_request_without_length() {
    setup_test_logger!();
    let input = b"GET / HTTP/1.1\r\n\
          Host: localhost:8888\r\n\
          Connection: close\r\n\
          \r\n";
    let initial = RequestState::Initial;
    let (_pool, mut buf) = http_buf_with_capacity(2048);
    buf.write(&input[..]).unwrap();

    //let result = parse_request(initial, input);
    let (mut state, header_end) =
        parse_request_until_stop(initial, None, &mut buf, None, "SOZUBALANCEID");
    println!("result: {:?}", state);
    println!("input length: {}", input.len());
    state = state.consume(header_end.unwrap(), &mut buf);
    assert_eq!(
        (state, header_end),
        (
            RequestState::Request {
                request: RRequestLine {
                    method: Method::Get,
                    uri: String::from("/"),
                    version: Version::V11
                },
                connection: Connection::close(),
                host: String::from("localhost:8888"),
            },
            Some(59)
        )
    );
    /*
    println!("buffer output: {:?}", buf.output_queue);
    assert_eq!(buf.output_queue, vec!(
      OutputElement::Slice(16), OutputElement::Slice(22), OutputElement::Delete(19),
      OutputElement::Insert(vec!()), OutputElement::Slice(2)));
    assert_eq!(buf.start_parsing_position, 59);
    */
}

// HTTP 1.0 is connection close by default
#[test]
fn parse_request_http_1_0_connection_close() {
    let input = b"GET / HTTP/1.0\r\n\
          Host: localhost:8888\r\n\
          \r\n";
    let initial = RequestState::Initial;
    let (_pool, mut buf) = http_buf_with_capacity(2048);
    buf.write(&input[..]).unwrap();

    //let result = parse_request(initial, input);
    let (mut state, header_end) =
        parse_request_until_stop(initial, None, &mut buf, None, "SOZUBALANCEID");
    println!("result: {:?}", state);
    //assert_eq!(buf.start_parsing_position, 40);
    state = state.consume(header_end.unwrap(), &mut buf);
    assert!(!state.should_keep_alive());
    assert_eq!(
        (state, header_end),
        (
            RequestState::Request {
                request: RRequestLine {
                    method: Method::Get,
                    uri: String::from("/"),
                    version: Version::V10
                },
                connection: Connection::close(),
                host: String::from("localhost:8888"),
            },
            Some(40)
        )
    );
}

#[test]
fn parse_request_http_1_0_connection_keep_alive() {
    setup_test_logger!();
    let input = b"GET / HTTP/1.0\r\n\
          Host: localhost:8888\r\n\
          Connection: keep-alive\r\n\
          \r\n";
    let initial = RequestState::Initial;
    let (_pool, mut buf) = http_buf_with_capacity(2048);
    buf.write(&input[..]).unwrap();

    //let result = parse_request(initial, input);
    let (mut state, header_end) =
        parse_request_until_stop(initial, None, &mut buf, None, "SOZUBALANCEID");
    println!("result: {:?}", state);
    println!("input length: {}", input.len());
    state = state.consume(header_end.unwrap(), &mut buf);
    assert!(state.should_keep_alive());
    assert_eq!(
        (state, header_end),
        (
            RequestState::Request {
                request: RRequestLine {
                    method: Method::Get,
                    uri: String::from("/"),
                    version: Version::V10
                },
                connection: Connection::keep_alive(),
                host: String::from("localhost:8888"),
            },
            Some(64)
        )
    );

    /*
    println!("buffer output: {:?}", buf.output_queue);
    assert_eq!(buf.output_queue, vec!(
      OutputElement::Slice(16), OutputElement::Slice(22), OutputElement::Delete(24),
      OutputElement::Insert(vec!()), OutputElement::Slice(2)));
    assert_eq!(buf.start_parsing_position, 64);
    */
}

#[test]
fn parse_request_http_1_1_connection_keep_alive() {
    setup_test_logger!();
    let input = b"GET / HTTP/1.1\r\n\
          Host: localhost:8888\r\n\
          \r\n";
    let initial = RequestState::Initial;
    let (_pool, mut buf) = http_buf_with_capacity(2048);
    buf.write(&input[..]).unwrap();

    //let result = parse_request(initial, input);
    let (mut state, header_end) =
        parse_request_until_stop(initial, None, &mut buf, None, "SOZUBALANCEID");
    println!("end buf:\n{}", buf.unparsed_data().to_hex(16));
    println!("result: {:?}", state);
    state = state.consume(header_end.unwrap(), &mut buf);
    assert_eq!(
        (state, header_end),
        (
            RequestState::Request {
                request: RRequestLine {
                    method: Method::Get,
                    uri: String::from("/"),
                    version: Version::V11
                },
                connection: Connection::keep_alive(),
                host: String::from("localhost:8888"),
            },
            Some(40)
        )
    );
    /*
    assert_eq!(buf.output_queue, vec!(
      OutputElement::Slice(16), OutputElement::Slice(22),
      OutputElement::Insert(vec!()), OutputElement::Slice(2)));
    assert_eq!(buf.start_parsing_position, 40);
    assert!(result.0.should_keep_alive());
    */
}

#[test]
fn parse_request_http_1_1_connection_close() {
    setup_test_logger!();
    let input = b"GET / HTTP/1.1\r\n\
          Connection: close\r\n\
          Host: localhost:8888\r\n\
          \r\n";
    let initial = RequestState::Initial;
    let (_pool, mut buf) = http_buf_with_capacity(2048);
    buf.write(&input[..]).unwrap();

    //let result = parse_request(initial, input);
    let (mut state, header_end) =
        parse_request_until_stop(initial, None, &mut buf, None, "SOZUBALANCEID");
    println!("end buf:\n{}", buf.unparsed_data().to_hex(16));
    println!("result: {:?}", state);
    state = state.consume(header_end.unwrap(), &mut buf);
    assert!(!state.should_keep_alive());
    assert_eq!(
        (state, header_end),
        (
            RequestState::Request {
                request: RRequestLine {
                    method: Method::Get,
                    uri: String::from("/"),
                    version: Version::V11
                },
                connection: Connection::close(),
                host: String::from("localhost:8888"),
            },
            Some(59)
        )
    );
    /*
    assert_eq!(buf.output_queue, vec!(
      OutputElement::Slice(16), OutputElement::Delete(19), OutputElement::Slice(22),
      OutputElement::Insert(vec!()), OutputElement::Slice(2)));
    assert_eq!(buf.start_parsing_position, 59);
    */
}

#[test]
fn parse_request_add_header_test() {
    setup_test_logger!();
    let input = b"GET /index.html HTTP/1.1\r\n\
          Host: localhost:8888\r\n\
          User-Agent: curl/7.43.0\r\n\
          Accept: */*\r\n\
          Content-Length: 200\r\n\
          \r\n";
    let initial = RequestState::Initial;
    let (_pool, mut buf) = http_buf_with_capacity(2048);
    buf.write(&input[..]).unwrap();

    let added = Some(AddedHeader {
        request_id: rusty_ulid::Ulid::from(0),
        public_address: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080),
        peer_address: Some(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(192, 168, 0, 2)),
            1234,
        )),
        protocol: crate::Protocol::HTTP,
        closing: false,
    });

    let (mut state, header_end) =
        parse_request_until_stop(initial, None, &mut buf, added.as_ref(), "SOZUBALANCEID");
    println!("result: {:?}", state);
    println!("input length: {}", input.len());
    assert_eq!(header_end.unwrap(), 109);
    // we add some request headers; so we must consume more
    let to_consume = 109 + 173;
    state = state.consume(to_consume, &mut buf);
    assert_eq!(
        (state, header_end),
        (
            RequestState::RequestWithBody {
                request: RRequestLine {
                    method: Method::Get,
                    uri: String::from("/index.html"),
                    version: Version::V11
                },
                connection: Connection::keep_alive(),
                host: String::from("localhost:8888"),
                length: 200,
            },
            Some(109)
        )
    );
    /*
    println!("buffer output: {:?}", buf.output_queue);
    assert_eq!(buf.output_queue, vec!(
      OutputElement::Slice(26), OutputElement::Slice(22), OutputElement::Slice(25),
      OutputElement::Slice(13), OutputElement::Slice(21), OutputElement::Insert(Vec::from(&new_header[..])),
    OutputElement::Slice(202)));
    println!("buf:\n{}", buf.buffer.data().to_hex(16));
    assert_eq!(buf.start_parsing_position, 309);
    */
}

#[test]
fn parse_request_delete_forwarded_headers() {
    setup_test_logger!();
    let input = b"GET /index.html HTTP/1.1\r\n\
          Host: localhost:8888\r\n\
          Forwarded: proto:https;for=10.0.0.2:1234;by:1.2.3.4\r\n\
          X-forwarded-Proto: https\r\n\
          X-Forwarded-For: 10.0.0.2\r\n\
          X-Forwarded-Port: 1234\r\n\
          \r\n";
    let initial = RequestState::Initial;
    let (_pool, mut buf) = http_buf_with_capacity(2048);
    buf.write(&input[..]).unwrap();

    let added = Some(AddedHeader {
        request_id: rusty_ulid::Ulid::from(0),
        public_address: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080),
        peer_address: Some(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(192, 168, 0, 2)),
            1234,
        )),
        protocol: crate::Protocol::HTTP,
        closing: false,
    });

    let (mut state, header_end) =
        parse_request_until_stop(initial, None, &mut buf, added.as_ref(), "SOZUBALANCEID");
    println!("result: {:?}", state);
    println!("input length: {}", input.len());
    assert_eq!(header_end.unwrap(), 180);
    // we add some request headers; so we must consume more
    let to_consume = 180 + 176;
    state = state.consume(to_consume, &mut buf);
    assert_eq!(
        (state, header_end),
        (
            RequestState::Request {
                request: RRequestLine {
                    method: Method::Get,
                    uri: String::from("/index.html"),
                    version: Version::V11
                },
                connection: Connection {
                    keep_alive: Some(true),
                    has_upgrade: false,
                    upgrade: None,
                    continues: Continue::None,
                    to_delete: None,
                    sticky_session: None,
                    forwarded: ForwardedHeaders {
                        x_proto: true,
                        x_host: false,
                        x_port: true,
                        x_for: Some("10.0.0.2".to_string()),
                        forwarded: Some("proto:https;for=10.0.0.2:1234;by:1.2.3.4".to_string()),
                    },
                },
                host: String::from("localhost:8888"),
            },
            Some(180)
        )
    );
    /*
    println!("buffer output: {:?}", buf.output_queue);

    let new_header = b"X-Forwarded-For: 10.0.0.2, 192.168.0.2\r\n\
    Forwarded: proto:https;for=10.0.0.2:1234;by:1.2.3.4, proto=http;for=192.168.0.2:1234;by=127.0.0.1\r\n\
    Sozu-Id: 00000000000000000000000000\r\n";

    assert_eq!(buf.output_queue, vec!(
      OutputElement::Slice(26), OutputElement::Slice(22),
      // Forwarded
      OutputElement::Delete(53),
      // X-Forwarded-Proto
      OutputElement::Slice(26),
      // X-Forwarded-For
      OutputElement::Delete(27),
      // X-Forwarded-Port
      OutputElement::Slice(24),
      OutputElement::Insert(Vec::from(&new_header[..])),
    OutputElement::Slice(2)));
    println!("buf:\n{}", buf.buffer.data().to_hex(16));
    assert_eq!(buf.start_parsing_position, 179);
    */
}

#[test]
fn parse_chunk() {
    let input = b"4\r\n\
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
        (BufferMove::Advance(43), Chunk::CopyingLastHeader(43, true))
    );
}

#[test]
fn parse_chunk_partial() {
    let input = b"4\r\n\
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
    assert_eq!(res, (BufferMove::Advance(17), Chunk::Copying(17)));

    println!("consuming input:\n{}", (&input[..17]).to_hex(16));
    println!("parsing input:\n{}", (&input[17..]).to_hex(16));
    let res2 = res.1.parse(&input[17..]);
    assert_eq!(
        res2,
        (BufferMove::Advance(26), Chunk::CopyingLastHeader(43, true))
    );
}

#[test]
fn parse_requests_and_chunks_test() {
    let input = b"POST /index.html HTTP/1.1\r\n\
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
    let (_pool, mut buf) = http_buf_with_capacity(2048);
    buf.write(&input[..]).unwrap();

    //let result = parse_request(initial, input);
    let (mut state, header_end) =
        parse_request_until_stop(initial, None, &mut buf, None, "SOZUBALANCEID");
    println!("result: {:?}", state);
    //assert_eq!(buf.start_parsing_position, 160);
    state = state.consume(header_end.unwrap(), &mut buf);
    assert_eq!(
        (&state, header_end),
        (
            &RequestState::RequestWithBodyChunks {
                request: RRequestLine {
                    method: Method::Post,
                    uri: String::from("/index.html"),
                    version: Version::V11
                },
                connection: Connection::keep_alive(),
                host: String::from("localhost:8888"),
                chunk: Chunk::Initial,
            },
            Some(117)
        )
    );

    //buf.consume_parsed_data(header_end.unwrap());

    loop {
        match state {
            RequestState::RequestWithBodyChunks { chunk, .. } => match chunk {
                Chunk::Initial | Chunk::Copying(_) | Chunk::CopyingLastHeader(_, _) => {
                    let (st, advance) = super::request2::parse_chunks(state, &mut buf);

                    state = st;
                    if let Some(sz) = advance {
                        println!(
                            "parsed: {:?}",
                            std::str::from_utf8(&(buf.unparsed_data())[..sz])
                        );
                        //buf.consume_parsed_data(sz);
                        state = state.consume(sz, &mut buf);
                    } else {
                        break;
                    }
                }
                Chunk::Error => {
                    panic!();
                }
                Chunk::Ended => {
                    break;
                }
            },
            st => {
                panic!("unexpected state: {:?}", st);
            }
        }
    }

    assert_eq!(
        state,
        RequestState::RequestWithBodyChunks {
            request: RRequestLine {
                method: Method::Post,
                uri: String::from("/index.html"),
                version: Version::V11
            },
            connection: Connection::keep_alive(),
            host: String::from("localhost:8888"),
            chunk: Chunk::Ended,
        },
    );
}

#[test]
fn parse_requests_and_chunks_partial_test() {
    let input = b"POST /index.html HTTP/1.1\r\n\
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
    let (_pool, mut buf) = http_buf_with_capacity(2048);
    buf.write(&input[..125]).unwrap();
    println!(
        "[{}]parsing request\n{}",
        line!(),
        buf.unparsed_data().to_hex(16)
    );

    let (mut state, header_end) =
        parse_request_until_stop(initial, None, &mut buf, None, "SOZUBALANCEID");
    println!("result({}): {:?}", line!(), state);
    state = state.consume(header_end.unwrap(), &mut buf);
    //assert_eq!(buf.start_parsing_position, 124);
    assert_eq!(
        (&state, header_end),
        (
            &RequestState::RequestWithBodyChunks {
                request: RRequestLine {
                    method: Method::Post,
                    uri: String::from("/index.html"),
                    version: Version::V11
                },
                connection: Connection::keep_alive(),
                host: String::from("localhost:8888"),
                chunk: Chunk::Initial,
            },
            Some(117)
        )
    );

    println!(
        "[{}]parsing first chunks\n{}",
        line!(),
        buf.unparsed_data().to_hex(16)
    );
    println!(
        "buf: available_data(: {}, parsed position: {}, buffer position: {}",
        buf.unparsed_data().len(),
        buf.parsed_position,
        buf.buffer_position
    );

    loop {
        match state {
            RequestState::RequestWithBodyChunks { chunk, .. } => match chunk {
                Chunk::Initial | Chunk::Copying(_) | Chunk::CopyingLastHeader(_, _) => {
                    let (st, advance) = super::request2::parse_chunks(state, &mut buf);

                    state = st;
                    if let Some(sz) = advance {
                        let index = std::cmp::min(sz, buf.unparsed_data().len());
                        println!(
                            "parsed: {:?}",
                            std::str::from_utf8(&(buf.unparsed_data())[..index])
                        );
                        state = state.consume(index, &mut buf);
                    } else {
                        break;
                    }
                }
                Chunk::Error => {
                    panic!();
                }
                Chunk::Ended => {
                    break;
                }
            },
            st => {
                panic!("unexpected state: {:?}", st);
            }
        }
    }

    assert_eq!(
        state,
        RequestState::RequestWithBodyChunks {
            request: RRequestLine {
                method: Method::Post,
                uri: String::from("/index.html"),
                version: Version::V11
            },
            connection: Connection::keep_alive(),
            host: String::from("localhost:8888"),
            chunk: Chunk::Copying(0),
        },
    );

    println!("state is now {:?}", state);
    //buf.consume(124);
    buf.write(&input[125..140]).unwrap();

    let next_len = state.next_slice(buf.unparsed_data()).len();
    println!("next_len: {}", next_len);
    if next_len != 0 {
        state = state.consume(next_len, &mut buf);
    }

    println!("state is now {:?}", state);

    /*
    println!("[{}]parsing next chunks\n{}", line!(), buf.unparsed_data().to_hex(16));
    println!("buf: available_data(: {}, parsed position: {}, buffer position: {}",
      buf.buffer.available_data(), buf.parsed_position, buf.buffer_position);

    loop {
      match state {
        RequestState::RequestWithBodyChunks{chunk, ..} => match chunk {
          Chunk::Initial | Chunk::Copying(_) | Chunk::CopyingLastHeader(_,_) => {
            let (st, advance) = super::request2::parse_chunks(state, &mut buf);

            state = st;
            if let Some(sz) = advance {
              let index = std::cmp::min(sz, buf.unparsed_data().len());
              println!("parsed: {:?}",
                std::str::from_utf8(&(buf.unparsed_data())[..index]));
              buf.consume_parsed_data(index);
              if state.next_slice(buf.unparsed_data()).len() == 0 {
                break;
              }
            } else {
              break;
            }
          },
          Chunk::Error => {
            panic!();
          },
          Chunk::Ended => {
            break;
          }
        }protocol::http::parser::tests::parse_chunkprotocol::http::parser::tests::parse_chunk

    assert_eq!(
      state,
      RequestState::RequestWithBodyChunks {
        request: RRequestLine { method: Method::Post, uri: String::from("/index.html"), version: Version::V11 },
        connection: Connection::keep_alive(),
        host: String::from("localhost:8888"),
        chunk: Chunk::Copying(36),
    },
    );*/

    buf.write(&input[140..]).unwrap();

    let next_len = state.next_slice(buf.unparsed_data()).len();
    println!("next_len: {}", next_len);
    if next_len != 0 {
        state = state.consume(next_len, &mut buf);
    }
    println!("state is now {:?}", state);

    println!(
        "[{}]parsing end\n{}",
        line!(),
        buf.unparsed_data().to_hex(16)
    );
    println!(
        "buf: available_data(: {}, parsed position: {}, buffer position: {}",
        buf.unparsed_data().len(),
        buf.parsed_position,
        buf.buffer_position
    );

    loop {
        match state {
            RequestState::RequestWithBodyChunks { chunk, .. } => match chunk {
                Chunk::Initial | Chunk::Copying(_) | Chunk::CopyingLastHeader(_, _) => {
                    let (st, advance) = super::request2::parse_chunks(state, &mut buf);

                    state = st;
                    if let Some(sz) = advance {
                        let index = std::cmp::min(sz, buf.unparsed_data().len());
                        println!(
                            "parsed: {:?}",
                            std::str::from_utf8(&(buf.unparsed_data())[..index])
                        );
                        state = state.consume(index, &mut buf);
                    } else {
                        break;
                    }
                }
                Chunk::Error => {
                    panic!();
                }
                Chunk::Ended => {
                    break;
                }
            },
            st => {
                panic!("unexpected state: {:?}", st);
            }
        }
    }

    //let result = parse_request_until_stop(result.0, result.1, &mut buf, None, "SOZUBALANCEID");
    //println!("result({}): {:?}", line!(), result);
    //assert_eq!(buf.start_parsing_position, 160);
    assert_eq!(
        state,
        RequestState::RequestWithBodyChunks {
            request: RRequestLine {
                method: Method::Post,
                uri: String::from("/index.html"),
                version: Version::V11
            },
            connection: Connection::keep_alive(),
            host: String::from("localhost:8888"),
            chunk: Chunk::Ended,
        },
    );
}

#[test]
fn parse_response_and_chunks_partial_test() {
    let input = b"HTTP/1.1 200 OK\r\n\
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
    let (_pool, mut buf) = http_buf_with_capacity(2048);
    buf.write(&input[..78]).unwrap();
    println!("parsing\n{}", buf.unparsed_data().to_hex(16));

    let (mut state, header_end) = parse_response_until_stop(
        initial,
        None,
        &mut buf,
        false,
        None,
        "SOZUBALANCEID",
        None,
        None,
    );
    println!("result({}): {:?}", line!(), state);
    assert_eq!(header_end, Some(74));
    state = state.consume(header_end.unwrap(), &mut buf);
    /*println!("result({}): {:?}", line!(), result);
    assert_eq!(buf.start_parsing_position, 81);*/
    assert_eq!(
        state,
        ResponseState::ResponseWithBodyChunks {
            status: RStatusLine {
                version: Version::V11,
                status: 200,
                reason: String::from("OK")
            },
            connection: Connection::keep_alive(),
            chunk: Chunk::Initial,
        }
    );

    buf.write(&input[78..81]).unwrap();
    let (mut state, advanced) = parse_response_until_stop(
        state,
        header_end,
        &mut buf,
        false,
        None,
        "SOZUBALANCEID",
        None,
        None,
    );
    println!("result({}): {:?}", line!(), state);
    assert_eq!(74 + advanced.as_ref().unwrap(), 81);
    state = state.consume(advanced.unwrap(), &mut buf);
    assert_eq!(
        state,
        ResponseState::ResponseWithBodyChunks {
            status: RStatusLine {
                version: Version::V11,
                status: 200,
                reason: String::from("OK")
            },
            connection: Connection::keep_alive(),
            chunk: Chunk::Copying(0),
        }
    );

    //buf.consume(78);
    buf.write(&input[81..100]).unwrap();
    println!("parsing\n{}", buf.unparsed_data().to_hex(16));

    let (mut state, advanced) = parse_response_until_stop(
        state,
        header_end,
        &mut buf,
        false,
        None,
        "SOZUBALANCEID",
        None,
        None,
    );
    println!("result({}): {:?}", line!(), state);
    assert_eq!(81 + advanced.as_ref().unwrap(), 110);

    /*println!("result({}): {:?}", line!(), result);
    assert_eq!(buf.start_parsing_position, 110);*/
    assert_eq!(
        state,
        ResponseState::ResponseWithBodyChunks {
            status: RStatusLine {
                version: Version::V11,
                status: 200,
                reason: String::from("OK")
            },
            connection: Connection::keep_alive(),
            chunk: Chunk::Copying(29),
        }
    );
    state = state.consume(19, &mut buf);

    //buf.consume(19);
    println!("remaining:\n{}", buf.unparsed_data().to_hex(16));
    buf.write(&input[110..116]).unwrap();
    println!("parsing\n{}", buf.unparsed_data().to_hex(16));
    let (mut state, header_end) = parse_response_until_stop(
        state,
        header_end,
        &mut buf,
        false,
        None,
        "SOZUBALANCEID",
        None,
        None,
    );
    println!("result({}): {:?}", line!(), state);
    assert_eq!(header_end, Some(5));

    /*println!("result({}): {:?}", line!(), result);
    assert_eq!(buf.start_parsing_position, 115);*/
    assert_eq!(
        state,
        ResponseState::ResponseWithBodyChunks {
            status: RStatusLine {
                version: Version::V11,
                status: 200,
                reason: String::from("OK")
            },
            connection: Connection::keep_alive(),
            chunk: Chunk::CopyingLastHeader(15, false),
        }
    );

    state = state.consume(5, &mut buf);
    //buf.consume(5);
    buf.write(&input[116..]).unwrap();
    println!("parsing\n{}", buf.unparsed_data().to_hex(16));
    let (mut state, header_end) = parse_response_until_stop(
        state,
        header_end,
        &mut buf,
        false,
        None,
        "SOZUBALANCEID",
        None,
        None,
    );
    println!("result({}): {:?}", line!(), state);
    assert_eq!(header_end, Some(2));
    state = state.consume(12, &mut buf);
    /*println!("result({}): {:?}", line!(), result);
    assert_eq!(buf.start_parsing_position, 117);*/
    assert_eq!(
        state,
        ResponseState::ResponseWithBodyChunks {
            status: RStatusLine {
                version: Version::V11,
                status: 200,
                reason: String::from("OK")
            },
            connection: Connection::keep_alive(),
            chunk: Chunk::Ended,
        }
    );
}

#[test]
fn parse_incomplete_chunk_header_test() {
    setup_test_logger!();
    let input = b"HTTP/1.1 200 OK\r\n\
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
    let initial = ResponseState::Initial; /*ResponseState::HasLength{
                                            status: RStatusLine { version: Version::V11, status: 200, reason: String::from("OK") },
                                            connection: Connection::keep_alive(),
                                            length: LengthInformation::Chunked,
                                          };*/
    let (_pool, mut buf) = http_buf_with_capacity(2048);

    buf.write(&input[..74]).unwrap();
    //buf.consume_parsed_data(72);
    println!("parsing\n{}", buf.unparsed_data().to_hex(16));
    let (mut state, header_end) = parse_response_until_stop(
        initial,
        None,
        &mut buf,
        false,
        None,
        "SOZUBALANCEID",
        None,
        None,
    );
    println!("result({}): {:?}", line!(), state);
    assert_eq!(header_end, Some(74));
    state = state.consume(header_end.unwrap(), &mut buf);

    /*println!("result: {:?}", result);
    println!("input length: {}", input.len());
    println!("initial input:\n{}", &input[..72].to_hex(8));
    println!("buffer output: {:?}", buf.output_queue);
    assert_eq!(buf.output_queue, vec!(OutputElement::Insert(vec!()), OutputElement::Slice(2)));
    assert_eq!(buf.start_parsing_position, 74);*/
    assert_eq!(
        state,
        ResponseState::ResponseWithBodyChunks {
            status: RStatusLine {
                version: Version::V11,
                status: 200,
                reason: String::from("OK")
            },
            connection: Connection::keep_alive(),
            chunk: Chunk::Initial,
        },
    );

    // we got the chunk header, but not the chunk content
    buf.write(&input[74..77]).unwrap();
    println!("parsing\n{}", buf.unparsed_data().to_hex(16));
    let (mut state, advance) = parse_response_until_stop(
        state,
        header_end,
        &mut buf,
        false,
        None,
        "SOZUBALANCEID",
        None,
        None,
    );
    println!("result({}): {:?}", line!(), state);
    assert_eq!(advance, Some(7));

    /*println!("result: {:?}", result);
    assert_eq!(buf.start_parsing_position, 81);*/
    assert_eq!(
        state,
        ResponseState::ResponseWithBodyChunks {
            status: RStatusLine {
                version: Version::V11,
                status: 200,
                reason: String::from("OK")
            },
            connection: Connection::keep_alive(),
            chunk: Chunk::Copying(7),
        }
    );
    buf.write(&input[77..81]).unwrap();
    state = state.consume(advance.unwrap(), &mut buf);

    println!("STATE IS NOW: {:?}", state);

    // the external code copied the chunk content directly, starting at next chunk end
    buf.write(&input[81..115]).unwrap();
    println!("parsing\n{}", buf.unparsed_data().to_hex(16));
    let (mut state, header_end) = parse_response_until_stop(
        state,
        header_end,
        &mut buf,
        false,
        None,
        "SOZUBALANCEID",
        None,
        None,
    );
    println!("result({}): {:?}", line!(), state);
    assert_eq!(header_end, Some(34));

    /*println!("result({}): {:?}", line!(), result);
    assert_eq!(buf.start_parsing_position, 115);*/
    assert_eq!(
        state,
        ResponseState::ResponseWithBodyChunks {
            status: RStatusLine {
                version: Version::V11,
                status: 200,
                reason: String::from("OK")
            },
            connection: Connection::keep_alive(),
            chunk: Chunk::CopyingLastHeader(34, false),
        }
    );

    state = state.consume(header_end.unwrap(), &mut buf);

    buf.write(&input[115..]).unwrap();
    println!("parsing\n{}", buf.unparsed_data().to_hex(16));
    let (mut state, header_end) = parse_response_until_stop(
        state,
        header_end,
        &mut buf,
        false,
        None,
        "SOZUBALANCEID",
        None,
        None,
    );
    println!("result({}): {:?}", line!(), state);
    assert_eq!(header_end, Some(2));
    state = state.consume(header_end.unwrap(), &mut buf);
    /*println!("result({}): {:?}", line!(), result);
    assert_eq!(buf.start_parsing_position, 117);*/
    assert_eq!(
        state,
        ResponseState::ResponseWithBodyChunks {
            status: RStatusLine {
                version: Version::V11,
                status: 200,
                reason: String::from("OK")
            },
            connection: Connection::keep_alive(),
            chunk: Chunk::Ended,
        }
    );
}

#[test]
fn parse_response_302() {
    let input = b"HTTP/1.1 302 Found\r\n\
        Cache-Control: no-cache\r\n\
        Content-length: 0\r\n\
        Location: https://www.clever-cloud.com\r\n\
        Connection: close\r\n\
        \r\n";
    let initial = ResponseState::Initial;
    let (_pool, mut buf) = http_buf_with_capacity(2048);
    buf.write(&input[..]).unwrap();

    let added = Some(AddedHeader {
        request_id: rusty_ulid::Ulid::from(0),
        public_address: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080),
        peer_address: Some(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(192, 168, 0, 2)),
            1234,
        )),
        protocol: crate::Protocol::HTTP,
        closing: false,
    });
    let (mut state, header_end) = parse_response_until_stop(
        initial,
        None,
        &mut buf,
        false,
        added.as_ref(),
        "SOZUBALANCEID",
        None,
        None,
    );
    println!("result({}): {:?}", line!(), state);
    assert_eq!(header_end, Some(125));
    assert_eq!(header_end.unwrap(), 125);
    // we add some request headers; so we must consume more
    let to_consume = 125 + 37;
    state = state.consume(to_consume, &mut buf);
    /*let (mut state, header_end) = parse_request_until_stop(initial, None, &mut buf, None, "SOZUBALANCEID");
      println!("result({}): {:?}", line!(), state);
      state = state.consume(header_end.unwrap(), &mut buf);
    println!("result: {:?}", result);
    println!("buf:\n{}", buf.buffer.data().to_hex(16));
    println!("input length: {}", input.len());
    println!("initial input:\n{}", &input[..72].to_hex(8));*/
    //println!("buffer output: {:?}", buf.output_queue);
    /*assert_eq!(buf.output_queue, vec!(
        OutputElement::Slice(20),
        OutputElement::Slice(25),
        OutputElement::Slice(19),
        OutputElement::Slice(40),
        OutputElement::Delete(19),
        OutputElement::Insert(Vec::from(&new_header[..])),
        OutputElement::Slice(2)));
    assert_eq!(buf.start_parsing_position, 125);*/

    assert_eq!(
        state,
        ResponseState::ResponseWithBody {
            status: RStatusLine {
                version: Version::V11,
                status: 302,
                reason: String::from("Found")
            },
            connection: Connection::close(),
            length: 0,
        },
    );
}

#[test]
fn parse_response_303() {
    let input = b"HTTP/1.1 303 See Other\r\n\
        Cache-Control: no-cache\r\n\
        Content-length: 0\r\n\
        Location: https://www.clever-cloud.com\r\n\
        Connection: close\r\n\
        \r\n";
    let initial = ResponseState::Initial;
    let (_pool, mut buf) = http_buf_with_capacity(2048);
    buf.write(&input[..]).unwrap();

    let added = Some(AddedHeader {
        request_id: rusty_ulid::Ulid::from(0),
        public_address: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080),
        peer_address: Some(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(192, 168, 0, 2)),
            1234,
        )),
        protocol: crate::Protocol::HTTP,
        closing: false,
    });
    let (mut state, header_end) = parse_response_until_stop(
        initial,
        None,
        &mut buf,
        false,
        added.as_ref(),
        "SOZUBALANCEID",
        None,
        None,
    );
    println!("result({}): {:?}", line!(), state);
    assert_eq!(header_end.unwrap(), 129);
    // we add some request headers; so we must consume more
    let to_consume = 129 + 37;
    state = state.consume(to_consume, &mut buf);
    /*println!("result: {:?}", result);
    println!("buf:\n{}", buf.buffer.data().to_hex(16));
    println!("input length: {}", input.len());
    println!("buffer output: {:?}", buf.output_queue);
    assert_eq!(buf.output_queue, vec!(
      OutputElement::Slice(24), OutputElement::Slice(25),
      OutputElement::Slice(19), OutputElement::Slice(40),
      OutputElement::Delete(19), OutputElement::Insert(Vec::from(&new_header[..])),
      OutputElement::Slice(2)));
    assert_eq!(buf.start_parsing_position, 129);*/
    assert_eq!(
        state,
        ResponseState::ResponseWithBody {
            status: RStatusLine {
                version: Version::V11,
                status: 303,
                reason: String::from("See Other")
            },
            connection: Connection::close(),
            length: 0,
        }
    );
}

#[test]
fn parse_response_304() {
    let input = b"HTTP/1.1 304 Not Modified\r\n\
          Connection: keep-alive\r\n\
          ETag: hello\r\n\
          \r\n";
    let initial = ResponseState::Initial;
    let is_head = true;
    let (_pool, mut buf) = http_buf_with_capacity(2048);
    buf.write(&input[..]).unwrap();
    //println!("buffer input: {:?}", buf.input_queue);

    //let result = parse_request(initial, input);
    let (mut state, header_end) = parse_response_until_stop(
        initial,
        None,
        &mut buf,
        is_head,
        None,
        "SOZUBALANCEID",
        None,
        None,
    );
    println!("result({}): {:?}", line!(), state);
    assert_eq!(header_end, Some(66));
    state = state.consume(header_end.unwrap(), &mut buf);
    /*println!("result: {:?}", result);
    println!("input length: {}", input.len());
    println!("buffer input: {:?}", buf.input_queue);
    println!("buffer output: {:?}", buf.output_queue);
    assert_eq!(buf.output_queue, vec!(
      OutputElement::Slice(27), OutputElement::Delete(24), OutputElement::Slice(13),
      OutputElement::Insert(vec!()), OutputElement::Slice(2)));
    assert_eq!(buf.start_parsing_position, 66);*/
    assert_eq!(
        state,
        ResponseState::Response {
            status: RStatusLine {
                version: Version::V11,
                status: 304,
                reason: String::from("Not Modified")
            },
            connection: Connection {
                keep_alive: Some(true),
                has_upgrade: false,
                upgrade: None,
                continues: Continue::None,
                to_delete: None,
                sticky_session: None,
                forwarded: ForwardedHeaders::default(),
            },
        }
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
        Err(Err::Error(error_position!(
            &b"_example.com"[..],
            ErrorKind::Eof
        )))
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
    let input = b"HTTP/1.1 200 Ok\r\n\
          Content-Length: 200\r\n\
          \r\n";
    let initial = ResponseState::Initial;
    let is_head = true;
    let (_pool, mut buf) = http_buf_with_capacity(2048);
    buf.write(&input[..]).unwrap();
    //println!("buffer input: {:?}", buf.input_queue);

    //let result = parse_request(initial, input);
    let (mut state, header_end) = parse_response_until_stop(
        initial,
        None,
        &mut buf,
        is_head,
        None,
        "SOZUBALANCEID",
        None,
        None,
    );
    println!("result({}): {:?}", line!(), state);
    assert_eq!(header_end, Some(40));
    state = state.consume(header_end.unwrap(), &mut buf);
    /*println!("result: {:?}", result);
    println!("input length: {}", input.len());
    println!("buffer input: {:?}", buf.input_queue);
    println!("buffer output: {:?}", buf.output_queue);
    assert_eq!(buf.output_queue, vec!(
      OutputElement::Slice(17), OutputElement::Slice(21),
      OutputElement::Insert(vec!()), OutputElement::Slice(2)));
    assert_eq!(buf.start_parsing_position, 40);*/
    assert_eq!(
        state,
        ResponseState::Response {
            status: RStatusLine {
                version: Version::V11,
                status: 200,
                reason: String::from("Ok")
            },
            connection: Connection::keep_alive(),
        }
    );
}

#[test]
fn parse_connection_upgrade_test() {
    let input = b"GET /index.html HTTP/1.1\r\n\
          Host: localhost:8888\r\n\
          User-Agent: curl/7.43.0\r\n\
          Accept: */*\r\n\
          Upgrade: WebSocket\r\n\
          Connection: keep-alive, Upgrade\r\n\
          \r\n";
    let initial = RequestState::Initial;
    let (_pool, mut buf) = http_buf_with_capacity(2048);
    buf.write(&input[..]).unwrap();
    //println!("buffer input: {:?}", buf.input_queue);

    //let result = parse_request(initial, input);
    let (mut state, header_end) =
        parse_request_until_stop(initial, None, &mut buf, None, "SOZUBALANCEID");
    println!("result: {:?}", state);
    println!("input length: {}", input.len());
    state = state.consume(header_end.unwrap(), &mut buf);
    assert_eq!(
        (state, header_end),
        (
            RequestState::Request {
                request: RRequestLine {
                    method: Method::Get,
                    uri: String::from("/index.html"),
                    version: Version::V11
                },
                connection: Connection {
                    keep_alive: Some(true),
                    has_upgrade: true,
                    upgrade: Some("WebSocket".to_string()),
                    continues: Continue::None,
                    to_delete: None,
                    sticky_session: None,
                    forwarded: ForwardedHeaders::default(),
                },
                host: String::from("localhost:8888"),
            },
            Some(141)
        )
    );
    /*
    println!("buffer input: {:?}", buf.input_queue);
    println!("buffer output: {:?}", buf.output_queue);
    assert_eq!(buf.output_queue, vec!(
      OutputElement::Slice(26), OutputElement::Slice(22), OutputElement::Slice(25),
      OutputElement::Slice(13), OutputElement::Slice(20), OutputElement::Slice(33),
      OutputElement::Insert(vec!()), OutputElement::Slice(2)));
    assert_eq!(buf.start_parsing_position, 141);
    */
}

#[test]
fn header_cookies_must_mutate() {
    let header = Header {
        name: b"Cookie",
        value: b"FOO=BAR",
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
        _ => panic!(),
    };

    let header2 = match message_header(header_line2) {
        Ok((_, header)) => header,
        _ => panic!(),
    };

    let header3 = match message_header(header_line3) {
        Ok((_, header)) => header,
        _ => panic!(),
    };

    let moves1 =
        header1.remove_sticky_cookie_in_request(header_line1, header_line1.len(), "SOZUBALANCEID");
    let moves2 =
        header2.remove_sticky_cookie_in_request(header_line2, header_line2.len(), "SOZUBALANCEID");
    let moves3 =
        header3.remove_sticky_cookie_in_request(header_line3, header_line3.len(), "SOZUBALANCEID");
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
        _ => panic!(),
    };

    let header2 = match message_header(header_line2) {
        Ok((_, header)) => header,
        _ => panic!(),
    };

    let moves1 =
        header1.remove_sticky_cookie_in_request(header_line1, header_line1.len(), "SOZUBALANCEID");
    let moves2 =
        header2.remove_sticky_cookie_in_request(header_line2, header_line2.len(), "SOZUBALANCEID");
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
        _ => panic!(),
    };

    let header2 = match message_header(header_line2) {
        Ok((_, header)) => header,
        _ => panic!(),
    };

    let header3 = match message_header(header_line3) {
        Ok((_, header)) => header,
        _ => panic!(),
    };

    let moves1 =
        header1.remove_sticky_cookie_in_request(header_line1, header_line1.len(), "SOZUBALANCEID");
    let moves2 =
        header2.remove_sticky_cookie_in_request(header_line2, header_line2.len(), "SOZUBALANCEID");
    let moves3 =
        header3.remove_sticky_cookie_in_request(header_line3, header_line3.len(), "SOZUBALANCEID");
    let expected1 = vec![
        BufferMove::Advance(7),
        BufferMove::Delete(16),
        BufferMove::Advance(7),
        BufferMove::Advance(2),
    ];
    let expected2 = vec![
        BufferMove::Advance(8),
        BufferMove::Delete(18),
        BufferMove::Advance(7),
        BufferMove::Advance(2),
    ];
    let expected3 = vec![
        BufferMove::Advance(8),
        BufferMove::Delete(17),
        BufferMove::Advance(7),
        BufferMove::Advance(2),
    ];

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
        _ => panic!(),
    };

    let header2 = match message_header(header_line2) {
        Ok((_, header)) => header,
        _ => panic!(),
    };

    let header3 = match message_header(header_line3) {
        Ok((_, header)) => header,
        _ => panic!(),
    };

    let moves1 =
        header1.remove_sticky_cookie_in_request(header_line1, header_line1.len(), "SOZUBALANCEID");
    let moves2 =
        header2.remove_sticky_cookie_in_request(header_line2, header_line2.len(), "SOZUBALANCEID");
    let moves3 =
        header3.remove_sticky_cookie_in_request(header_line3, header_line3.len(), "SOZUBALANCEID");
    let expected1 = vec![
        BufferMove::Advance(8),
        BufferMove::Advance(9),
        BufferMove::Delete(16),
        BufferMove::Advance(7),
        BufferMove::Advance(2),
    ];
    let expected2 = vec![
        BufferMove::Advance(7),
        BufferMove::Advance(8),
        BufferMove::Delete(18),
        BufferMove::Advance(7),
        BufferMove::Advance(2),
    ];
    let expected3 = vec![
        BufferMove::Advance(8),
        BufferMove::Advance(9),
        BufferMove::Delete(17),
        BufferMove::Advance(7),
        BufferMove::Advance(2),
    ];

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
        _ => panic!(),
    };

    let header2 = match message_header(header_line2) {
        Ok((_, header)) => header,
        _ => panic!(),
    };

    let header3 = match message_header(header_line3) {
        Ok((_, header)) => header,
        _ => panic!(),
    };

    let header4 = match message_header(header_line4) {
        Ok((_, header)) => header,
        _ => panic!(),
    };

    let moves1 =
        header1.remove_sticky_cookie_in_request(header_line1, header_line1.len(), "SOZUBALANCEID");
    let moves2 =
        header2.remove_sticky_cookie_in_request(header_line2, header_line2.len(), "SOZUBALANCEID");
    let moves3 =
        header3.remove_sticky_cookie_in_request(header_line3, header_line3.len(), "SOZUBALANCEID");
    let moves4 =
        header4.remove_sticky_cookie_in_request(header_line4, header_line4.len(), "SOZUBALANCEID");
    let expected1 = vec![
        BufferMove::Advance(8),
        BufferMove::Advance(7),
        BufferMove::Delete(3),
        BufferMove::Delete(15),
        BufferMove::Advance(2),
    ];
    let expected2 = vec![
        BufferMove::Advance(7),
        BufferMove::Advance(7),
        BufferMove::Delete(1),
        BufferMove::Delete(18),
        BufferMove::Advance(2),
    ];
    let expected3 = vec![
        BufferMove::Advance(8),
        BufferMove::Advance(7),
        BufferMove::Delete(2),
        BufferMove::Delete(17),
        BufferMove::Advance(2),
    ];
    let expected4 = vec![
        BufferMove::Advance(8),
        BufferMove::Advance(7),
        BufferMove::Delete(2),
        BufferMove::Delete(15),
        BufferMove::Advance(2),
    ];

    assert_eq!(moves1, expected1);
    assert_eq!(moves2, expected2);
    assert_eq!(moves3, expected3);
    assert_eq!(moves4, expected4);
}

/*
#[test]
fn header_content_disposition() {
  let header1 = b"Content-Disposition: Attachment; filename=example.html\r\n";
  let header2 = b"Content-Disposition: INLINE; FILENAME= \"an example.html\"\r\n";
  let header3 = b"Content-Disposition: attachment;  filename*= UTF-8''%e2%82%ac%20rates\r\n";
  let header4 = b"Content-Disposition: attachment; filename=\"EURO rates\"; filename*=utf-8''%e2%82%ac%20rates\r\n";

  let header5 = b"Content-Disposition: attachment; filename=\"Export _ Main metrics _ December 13, 2020 \x8093 January 11, 2021.csv\"\r\n";

  let res1 = message_header(header1);
  println!("for\n{}\ngot header: {:?}", from_utf8(header1).unwrap(), res1);
  res1.unwrap();

  let res2 = message_header(header2);
  println!("for\n{}\ngot header: {:?}", from_utf8(header2).unwrap(), res2);
  res2.unwrap();


  let res3 = message_header(header3);
  println!("for\n{}\ngot header: {:?}", from_utf8(header3).unwrap(), res3);
  res3.unwrap();

  let res4 = message_header(header4);
  println!("for\n{}\ngot header: {:?}", from_utf8(header4).unwrap(), res4);
  res4.unwrap();

  let res5 = message_header(header5);
  println!("for\n{}\ngot header: {:?}", String::from_utf8_lossy(header5), res5);
  res5.unwrap();
}
*/

#[cfg(all(feature = "unstable", test))]
mod bench {
    use super::*;
    use buffer_queue::BufferQueue;
    use std::io::Write;
    use test::Bencher;

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
        b.iter(|| {
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
        b.iter(|| {
            let mut current_state = RequestState::Initial;
            let mut position = 0;
            loop {
                let test_position = position;
                let (mv, new_state) = parse_request(current_state, &data[test_position..], "");
                current_state = new_state;

                if let BufferMove::Delete(end) = mv {
                    position += end;
                }
                match mv {
                    BufferMove::Advance(sz) => {
                        position += sz;
                    }
                    BufferMove::Delete(_) => {}
                    _ => break,
                }

                match current_state {
                    RequestState::Request(_, _, _)
                    | RequestState::RequestWithBody(_, _, _, _)
                    | RequestState::Error(_, _, _, _, _)
                    | RequestState::RequestWithBodyChunks(_, _, _, Chunk::Ended) => break,
                    _ => (),
                }

                if position >= data.len() {
                    break;
                }
                //println!("pos: {}, len: {}, state: {:?}, remaining:\n{}", position, data.len(), current_state, (&data[position..]).to_hex(16));
            }
        });
    }
}
