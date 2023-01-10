pub fn http_ok_response<S: Into<String>>(content: S) -> String {
    let content = content.into();
    let status_line = "HTTP/1.1 200 OK";
    let length = content.len();
    format!(
        "{}\r\nContent-Length: {}\r\n\r\n{}",
        status_line, length, content
    )
}

/// Creates an HTTP/1 raw request
pub fn http_request<S1: Into<String>, S2: Into<String>, S3: Into<String>>(
    method: S1,
    uri: S2,
    content: S3,
) -> String {
    let content = content.into();
    let length = content.len();
    format!(
        "{} {} HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\nContent-Length: {}\r\n\r\n{}",
        method.into(),
        uri.into(),
        length,
        content,
    )
}
