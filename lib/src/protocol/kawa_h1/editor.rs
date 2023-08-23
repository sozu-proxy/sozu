use std::{
    net::{IpAddr, SocketAddr},
    str::{from_utf8, from_utf8_unchecked},
};

use rusty_ulid::Ulid;

use crate::{
    pool::Checkout,
    protocol::http::{parser::compare_no_case, GenericHttpStream, Method},
    Protocol, RetrieveClusterError,
};

/// This is the container used to store and use information about the session from within a Kawa parser callback
#[derive(Debug)]
pub struct HttpContext {
    // ========== Write only
    /// set to false if Kawa finds a "Connection" header with a "close" value in the response
    pub keep_alive_backend: bool,
    /// set to false if Kawa finds a "Connection" header with a "close" value in the request
    pub keep_alive_frontend: bool,
    /// the value of the sticky session cookie in the request
    pub sticky_session_found: Option<String>,
    // ---------- Status Line
    /// the value of the method in the request line
    pub method: Option<Method>,
    /// the value of the authority of the request (in the request line of "Host" header)
    pub authority: Option<String>,
    /// the value of the path in the request line
    pub path: Option<String>,
    /// the value of the status code in the response line
    pub status: Option<u16>,
    /// the value of the reason in the response line
    pub reason: Option<String>,
    // ---------- Additional optional data
    pub user_agent: Option<String>,

    // ========== Read only
    /// signals wether Kawa should write a "Connection" header with a "close" value (request and response)
    pub closing: bool,
    /// the value of the custom header, named "Sozu-Id", that Kawa should write (request and response)
    pub id: Ulid,
    /// the value of the protocol Kawa should write in the Forwarded headers of the request
    pub protocol: Protocol,
    /// the value of the public address Kawa should write in the Forwarded headers of the request
    pub public_address: SocketAddr,
    /// the value of the session address Kawa should write in the Forwarded headers of the request
    pub session_address: Option<SocketAddr>,
    /// the name of the cookie Kawa should read from the request to get the sticky session
    pub sticky_name: String,
    /// the sticky session that should be used
    /// used to create a "Set-Cookie" header in the response in case it differs from sticky_session_found
    pub sticky_session: Option<String>,
}

impl kawa::h1::ParserCallbacks<Checkout> for HttpContext {
    fn on_headers(&mut self, stream: &mut GenericHttpStream) {
        match stream.kind {
            kawa::Kind::Request => self.on_request_headers(stream),
            kawa::Kind::Response => self.on_response_headers(stream),
        }
    }
}

impl HttpContext {
    /// Callback for request:
    ///
    /// - edit headers (connection, forwarded, sticky cookie, sozu-id)
    /// - save information:
    ///   - method
    ///   - authority
    ///   - path
    ///   - front keep-alive
    ///   - sticky cookie
    ///   - user-agent
    fn on_request_headers(&mut self, request: &mut GenericHttpStream) {
        let buf = &mut request.storage.mut_buffer();

        // Captures the request line
        if let kawa::StatusLine::Request {
            method,
            authority,
            path,
            ..
        } = &request.detached.status_line
        {
            self.method = method.data_opt(buf).map(Method::new);
            self.authority = authority
                .data_opt(buf)
                .and_then(|data| from_utf8(data).ok())
                .map(ToOwned::to_owned);
            self.path = path
                .data_opt(buf)
                .and_then(|data| from_utf8(data).ok())
                .map(ToOwned::to_owned);
        }

        let public_ip = self.public_address.ip();
        let public_port = self.public_address.port();
        let proto = match self.protocol {
            Protocol::HTTP => "http",
            Protocol::HTTPS => "https",
            _ => unreachable!(),
        };

        // Find and remove the sticky_name cookie
        // if found its value is stored in sticky_session_found
        for cookie in &mut request.detached.jar {
            let key = cookie.key.data(buf);
            if key == self.sticky_name.as_bytes() {
                let val = cookie.val.data(buf);
                self.sticky_session_found = from_utf8(val).ok().map(|val| val.to_string());
                cookie.elide();
            }
        }

        // If found:
        // - set Connection to "close" if closing is set
        // - set keep_alive_frontend to false if Connection is "close"
        // - update value of X-Forwarded-Proto
        // - update value of X-Forwarded-Port
        // - store X-Forwarded-For
        // - store Forwarded
        // - store User-Agent
        let mut x_for = None;
        let mut forwarded = None;
        let mut has_connection = false;
        for block in &mut request.blocks {
            match block {
                kawa::Block::Header(header) if !header.is_elided() => {
                    let key = header.key.data(buf);
                    if compare_no_case(key, b"connection") {
                        has_connection = true;
                        if self.closing {
                            header.val = kawa::Store::Static(b"close");
                        } else {
                            let val = header.val.data(buf);
                            self.keep_alive_frontend &= !compare_no_case(val, b"close");
                        }
                    } else if compare_no_case(key, b"X-Forwarded-Proto") {
                        header.val = kawa::Store::Static(proto.as_bytes());
                    } else if compare_no_case(key, b"X-Forwarded-Port") {
                        header.val = kawa::Store::from_string(public_port.to_string());
                    } else if compare_no_case(key, b"X-Forwarded-For") {
                        x_for = Some(header);
                    } else if compare_no_case(key, b"Forwarded") {
                        forwarded = Some(header);
                    } else if compare_no_case(key, b"User-Agent") {
                        self.user_agent = header
                            .val
                            .data_opt(buf)
                            .and_then(|data| from_utf8(data).ok())
                            .map(ToOwned::to_owned);
                    }
                }
                _ => {}
            }
        }

        // If session_address is set:
        // - append its ip address to the list of "X-Forwarded-For" if it was found, creates it if not
        // - append "proto=[PROTO];for=[PEER];by=[PUBLIC]" to the list of "Forwarded" if it was found, creates it if not
        if let Some(peer_addr) = self.session_address {
            let peer_ip = peer_addr.ip();
            let peer_port = peer_addr.port();
            let has_x_for = x_for.is_some();
            let has_forwarded = forwarded.is_some();

            if let Some(header) = x_for {
                header.val = kawa::Store::from_string(format!(
                    "{}, {}",
                    unsafe { from_utf8_unchecked(header.val.data(buf)) },
                    peer_ip
                ));
            }
            if let Some(header) = &mut forwarded {
                let value = unsafe { from_utf8_unchecked(header.val.data(buf)) };
                let new_value = match (peer_ip, public_ip) {
                    (IpAddr::V4(_), IpAddr::V4(_)) => {
                        format!("{value}, proto={proto};for={peer_ip}:{peer_port};by={public_ip}")
                    }
                    (IpAddr::V4(_), IpAddr::V6(_)) => {
                        format!(
                            "{value}, proto={proto};for={peer_ip}:{peer_port};by=\"{public_ip}\""
                        )
                    }
                    (IpAddr::V6(_), IpAddr::V4(_)) => {
                        format!(
                            "{value}, proto={proto};for=\"{peer_ip}:{peer_port}\";by={public_ip}"
                        )
                    }
                    (IpAddr::V6(_), IpAddr::V6(_)) => {
                        format!(
                        "{value}, proto={proto};for=\"{peer_ip}:{peer_port}\";by=\"{public_ip}\""
                    )
                    }
                };
                header.val = kawa::Store::from_string(new_value);
            }

            if !has_x_for {
                request.push_block(kawa::Block::Header(kawa::Pair {
                    key: kawa::Store::Static(b"X-Forwarded-For"),
                    val: kawa::Store::from_string(peer_ip.to_string()),
                }));
            }
            if !has_forwarded {
                let value = match (peer_ip, public_ip) {
                    (IpAddr::V4(_), IpAddr::V4(_)) => {
                        format!("proto={proto};for={peer_ip}:{peer_port};by={public_ip}")
                    }
                    (IpAddr::V4(_), IpAddr::V6(_)) => {
                        format!("proto={proto};for={peer_ip}:{peer_port};by=\"{public_ip}")
                    }
                    (IpAddr::V6(_), IpAddr::V4(_)) => {
                        format!("proto={proto};for=\"{peer_ip}:{peer_port}\";by={public_ip}")
                    }
                    (IpAddr::V6(_), IpAddr::V6(_)) => {
                        format!("proto={proto};for=\"{peer_ip}:{peer_port}\";by=\"{public_ip}\"")
                    }
                };
                request.push_block(kawa::Block::Header(kawa::Pair {
                    key: kawa::Store::Static(b"Forwarded"),
                    val: kawa::Store::from_string(value),
                }));
            }
        }

        // Create a "Connection" header in case it was not found and closing it set
        if !has_connection && self.closing {
            request.push_block(kawa::Block::Header(kawa::Pair {
                key: kawa::Store::Static(b"Connection"),
                val: kawa::Store::Static(b"close"),
            }));
        }

        // Create a custom "Sozu-Id" header
        request.push_block(kawa::Block::Header(kawa::Pair {
            key: kawa::Store::Static(b"Sozu-Id"),
            val: kawa::Store::from_string(self.id.to_string()),
        }));
    }

    /// Callback for response:
    ///
    /// - edit headers (connection, set-cookie, sozu-id)
    /// - save information:
    ///   - status code
    ///   - reason
    ///   - back keep-alive
    fn on_response_headers(&mut self, response: &mut GenericHttpStream) {
        let buf = &mut response.storage.mut_buffer();

        // Captures the response line
        if let kawa::StatusLine::Response { code, reason, .. } = &response.detached.status_line {
            self.status = Some(*code);
            self.reason = reason
                .data_opt(buf)
                .and_then(|data| from_utf8(data).ok())
                .map(ToOwned::to_owned);
        }

        // If found:
        // - set Connection to "close" if closing is set
        // - set keep_alive_backend to false if Connection is "close"
        for block in &mut response.blocks {
            match block {
                kawa::Block::Header(header) if !header.is_elided() => {
                    let key = header.key.data(buf);
                    if compare_no_case(key, b"connection") {
                        if self.closing {
                            header.val = kawa::Store::Static(b"close");
                        } else {
                            let val = header.val.data(buf);
                            self.keep_alive_backend &= !compare_no_case(val, b"close");
                        }
                    }
                }
                _ => {}
            }
        }

        // If the sticky_session is set and differs from the one found in the request
        // create a "Set-Cookie" header to update the sticky_name value
        if let Some(sticky_session) = &self.sticky_session {
            if self.sticky_session != self.sticky_session_found {
                response.push_block(kawa::Block::Header(kawa::Pair {
                    key: kawa::Store::Static(b"Set-Cookie"),
                    val: kawa::Store::from_string(format!(
                        "{}={}; Path=/",
                        self.sticky_name, sticky_session
                    )),
                }));
            }
        }

        // Create a custom "Sozu-Id" header
        response.push_block(kawa::Block::Header(kawa::Pair {
            key: kawa::Store::Static(b"Sozu-Id"),
            val: kawa::Store::from_string(self.id.to_string()),
        }));
    }

    // -> host, path, method
    pub fn extract_route(&self) -> Result<(&str, &str, &Method), RetrieveClusterError> {
        let given_method = self.method.as_ref().ok_or(RetrieveClusterError::NoMethod)?;
        let given_authority = self
            .authority
            .as_deref()
            .ok_or(RetrieveClusterError::NoHost)?;
        let given_path = self.path.as_deref().ok_or(RetrieveClusterError::NoPath)?;

        Ok((given_authority, given_path, given_method))
    }
}
