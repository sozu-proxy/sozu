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
    let (_pool, mut buf) = buf_with_capacity(2048);
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
    let (_pool, mut buf) = buf_with_capacity(2048);
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
    let (_pool, mut buf) = buf_with_capacity(2048);
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

    let (_pool, mut buf) = buf_with_capacity(2048);
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
    let (_pool, mut buf) = buf_with_capacity(2048);
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

    let (_pool, mut buf) = buf_with_capacity(2048);
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
    let (_pool, mut buf) = buf_with_capacity(2048);
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
    let (_pool, mut buf) = buf_with_capacity(2048);
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
    let (_pool, mut buf) = buf_with_capacity(2048);
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
    let (_pool, mut buf) = buf_with_capacity(2048);
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
    let (_pool, mut buf) = buf_with_capacity(2048);
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
    let (_pool, mut buf) = buf_with_capacity(2048);
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
    let (_pool, mut buf) = buf_with_capacity(2048);
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
    let (_pool, mut buf) = buf_with_capacity(2048);
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
    let (_pool, mut buf) = buf_with_capacity(2048);
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
    let (_pool, mut buf) = buf_with_capacity(2048);
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
    let (_pool, mut buf) = buf_with_capacity(2048);
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
    let (_pool, mut buf) = buf_with_capacity(2048);

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
  let (_pool, mut buf) = buf_with_capacity(2048);
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
  let (_pool, mut buf) = buf_with_capacity(2048);
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
    let (_pool, mut buf) = buf_with_capacity(2048);
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
    let (_pool, mut buf) = buf_with_capacity(2048);
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
    let (_pool, mut buf) = buf_with_capacity(2048);
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
