pub fn http_ok_response<S: Into<String>>(content: S) -> String {
    let content = content.into();
    let status_line = "HTTP/1.1 200 OK";
    let length = content.len();
    format!("{status_line}\r\nContent-Length: {length}\r\n\r\n{content}")
}

/// Creates an HTTP/1 raw request
pub fn http_request<S1: Into<String>, S2: Into<String>, S3: Into<String>, S4: Into<String>>(
    method: S1,
    uri: S2,
    content: S3,
    host: S4,
) -> String {
    let content = content.into();
    let length = content.len();
    format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: keep-alive\r\nContent-Length: {}\r\n\r\n{}",
        method.into(),
        uri.into(),
        host.into(),
        length,
        content,
    )
}

pub fn immutable_answer(status: u16) -> String {
    match status {
        // 400 => String::from("HTTP/1.1 400 Bad Request\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"),
        400 => String::from("HTTP/1.1 400 Sozu Default Answer\r\nCache-Control: no-cache\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"),
        // 404 => String::from("HTTP/1.1 404 Not Found\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"),
        404 => String::from("HTTP/1.1 404 Sozu Default Answer\r\nCache-Control: no-cache\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"),
        // 502 => String::from("HTTP/1.1 502 Bad Gateway\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"),
        502 => String::from("HTTP/1.1 502 Sozu Default Answer\r\nCache-Control: no-cache\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"),
        // 503 => String::from("HTTP/1.1 503 Service Unavailable\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"),
        503 => String::from("HTTP/1.1 503 Sozu Default Answer\r\nCache-Control: no-cache\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"),
        _ => unimplemented!()
    }
}

// use std::io::Write;
// use kawa;

// /// the default kawa answer for the error code provided, converted to HTTP/1.1
// pub fn default_answer(code: u16) -> String {
//     let mut kawa_answer = kawa::Kawa::new(
//         kawa::Kind::Response,
//         kawa::Buffer::new(kawa::SliceBuffer(&mut [])),
//     );
//     sozu_lib::protocol::mux::fill_default_answer(&mut kawa_answer, code);
//     kawa_answer.prepare(&mut kawa::h1::converter::H1BlockConverter);
//     let out = kawa_answer.as_io_slice();
//     let mut writer = std::io::BufWriter::new(Vec::new());
//     writer.write_vectored(&out).expect("WRITE");
//     let result = unsafe { std::str::from_utf8_unchecked(writer.buffer()) };
//     result.to_string()
// }
