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

// the default value for the 404 error, as provided in the command lib,
// used as default for listeners
pub fn default_404_answer() -> String {
    String::from(include_str!("../../../command/assets/404.html"))
}

// the default value for the 503 error, as provided in the command lib,
// used as default for listeners
pub fn default_503_answer() -> String {
    String::from(include_str!("../../../command/assets/503.html"))
}
