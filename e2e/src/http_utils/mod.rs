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
        400 => String::from("HTTP/1.1 400 Bad Request\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"),
        404 => String::from("HTTP/1.1 404 Not Found\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"),
        502 => String::from("HTTP/1.1 502 Bad Gateway\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"),
        503 => String::from("HTTP/1.1 503 Service Unavailable\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"),
        _ => unimplemented!()
    }
}
