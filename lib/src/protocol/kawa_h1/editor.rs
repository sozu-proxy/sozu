use crate::{
    pool::Checkout,
    protocol::http::{parser::compare_no_case, GenericHttpStream, Method},
    Protocol,
};
use rusty_ulid::Ulid;
use std::{
    net::{IpAddr, SocketAddr},
    str::{from_utf8, from_utf8_unchecked},
};

use sozu_command_lib::logging::LogContext;

#[cfg(feature = "opentelemetry")]
fn parse_ot_id<const N: usize>(val: &kawa::Store, buf: &[u8]) -> Option<[u8; N]> {
    let trace_id: Option<[u8; N]> = val.data(buf).try_into().ok();
    trace_id.filter(|trace_id| trace_id.iter().all(|c| c.is_ascii_hexdigit()))
}

#[cfg(feature = "opentelemetry")]
fn random_id<const N: usize>() -> [u8; N] {
    use rand::Rng;
    const CHARSET: &[u8] = b"0123456789abcdef";
    let mut rng = rand::thread_rng();
    let mut buf = [0; N];
    buf.fill_with(|| {
        let n = rng.gen_range(0..CHARSET.len());
        CHARSET[n]
    });
    buf
}

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

    #[cfg(feature = "opentelemetry")]
    pub ot_trace_id: [u8; 32],
    #[cfg(feature = "opentelemetry")]
    pub ot_span_id: [u8; 16],
    #[cfg(feature = "opentelemetry")]
    pub ot_parent_span_id: [u8; 16],

    // ========== Read only
    /// signals wether Kawa should write a "Connection" header with a "close" value (request and response)
    pub closing: bool,
    /// the value of the custom header, named "Sozu-Id", that Kawa should write (request and response)
    pub id: Ulid,
    pub backend_id: Option<String>,
    pub cluster_id: Option<String>,
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
    /// Creates a new instance
    pub fn new(
        request_id: Ulid,
        protocol: Protocol,
        public_address: SocketAddr,
        session_address: Option<SocketAddr>,
        sticky_name: String,
    ) -> Self {
        Self {
            id: request_id,
            backend_id: None,
            cluster_id: None,

            closing: false,
            keep_alive_backend: true,
            keep_alive_frontend: true,
            protocol,
            public_address,
            session_address,
            sticky_name,
            sticky_session: None,
            sticky_session_found: None,

            method: None,
            authority: None,
            path: None,
            status: None,
            reason: None,
            user_agent: None,

            #[cfg(feature = "opentelemetry")]
            ot_trace_id: [0; 32],
            #[cfg(feature = "opentelemetry")]
            ot_span_id: [0; 16],
            #[cfg(feature = "opentelemetry")]
            ot_parent_span_id: [0; 16],
        }
    }

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
        let buf = request.storage.mut_buffer();

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

        // if self.method == Some(Method::Get) && request.body_size == kawa::BodySize::Empty {
        //     request.parsing_phase = kawa::ParsingPhase::Terminated;
        // }

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
        let mut has_x_port = false;
        let mut has_x_proto = false;
        let mut has_connection = false;
        #[cfg(feature = "opentelemetry")]
        let mut ot_trace_id = None;
        #[cfg(feature = "opentelemetry")]
        let mut ot_span_id = None;
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
                        has_x_proto = true;
                        // header.val = kawa::Store::Static(proto.as_bytes());
                        incr!("http.trusting.x_proto");
                        let val = header.val.data(buf);
                        if !compare_no_case(val, proto.as_bytes()) {
                            incr!("http.trusting.x_proto.diff");
                            debug!(
                                "Trusting X-Forwarded-Proto for {:?} even though {:?} != {}",
                                self.authority, val, proto
                            );
                        }
                    } else if compare_no_case(key, b"X-Forwarded-Port") {
                        has_x_port = true;
                        // header.val = kawa::Store::from_string(public_port.to_string());
                        incr!("http.trusting.x_port");
                        let val = header.val.data(buf);
                        let expected = public_port.to_string();
                        if !compare_no_case(val, expected.as_bytes()) {
                            incr!("http.trusting.x_port.diff");
                            debug!(
                                "Trusting X-Forwarded-Port for {:?} even though {:?} != {}",
                                self.authority, val, expected
                            );
                        }
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
                    } else {
                        #[cfg(feature = "opentelemetry")]
                        if compare_no_case(key, b"ot-tracer-traceid") {
                            ot_trace_id = Some(header);
                        } else if compare_no_case(key, b"ot-tracer-spanid") {
                            ot_span_id = Some(header);
                        }
                    }
                }
                _ => {}
            }
        }

        #[cfg(feature = "opentelemetry")]
        let (has_trace_id, has_span_id) = {
            // Handle traceid
            self.ot_trace_id = ot_trace_id
                .as_ref()
                .and_then(|hdr| parse_ot_id(&hdr.val, buf))
                .unwrap_or_else(|| random_id());
            // Handle spanid
            self.ot_span_id = random_id();
            // Modify header if present
            if let Some(id) = &mut ot_span_id {
                // Swap spanid for the generated one
                id.val.modify(buf, self.ot_span_id.as_slice());
            }
            // Handle parent spanid
            self.ot_parent_span_id = ot_span_id
                .as_ref()
                .and_then(|id| parse_ot_id::<16>(&id.val, buf))
                .unwrap_or_else(|| random_id());
            // Insert new headers later to avoid borrow issues
            (ot_trace_id.is_some(), ot_span_id.is_some())
        };

        // If session_address is set:
        // - append its ip address to the list of "X-Forwarded-For" if it was found, creates it if not
        // - append "proto=[PROTO];for=[PEER];by=[PUBLIC]" to the list of "Forwarded" if it was found, creates it if not
        if let Some(peer_addr) = self.session_address {
            let peer_ip = peer_addr.ip();
            let peer_port = peer_addr.port();
            let has_x_for = x_for.is_some();
            let has_forwarded = forwarded.is_some();

            if let Some(header) = x_for {
                header.val = kawa::Store::from_string(format!("{}, {peer_ip}", unsafe {
                    from_utf8_unchecked(header.val.data(buf))
                }));
            }
            if let Some(header) = &mut forwarded {
                let value = unsafe { from_utf8_unchecked(header.val.data(buf)) };
                let new_value = match public_ip {
                    IpAddr::V4(_) => {
                        format!(
                            "{value}, proto={proto};for=\"{peer_ip}:{peer_port}\";by={public_ip}"
                        )
                    }
                    IpAddr::V6(_) => {
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
                let value = match public_ip {
                    IpAddr::V4(_) => {
                        format!("proto={proto};for=\"{peer_ip}:{peer_port}\";by={public_ip}")
                    }
                    IpAddr::V6(_) => {
                        format!("proto={proto};for=\"{peer_ip}:{peer_port}\";by=\"{public_ip}\"")
                    }
                };
                request.push_block(kawa::Block::Header(kawa::Pair {
                    key: kawa::Store::Static(b"Forwarded"),
                    val: kawa::Store::from_string(value),
                }));
            }
        }

        #[cfg(feature = "opentelemetry")]
        {
            if !has_trace_id {
                request.push_block(kawa::Block::Header(kawa::Pair {
                    key: kawa::Store::Static(b"ot-tracer-traceid"),
                    val: kawa::Store::from_slice(self.ot_trace_id.as_slice()),
                }));
            }
            if !has_span_id {
                request.push_block(kawa::Block::Header(kawa::Pair {
                    key: kawa::Store::Static(b"ot-tracer-spanid"),
                    val: kawa::Store::from_slice(self.ot_span_id.as_slice()),
                }));
            }
        }

        if !has_x_port {
            request.push_block(kawa::Block::Header(kawa::Pair {
                key: kawa::Store::Static(b"X-Forwarded-Port"),
                val: kawa::Store::from_string(public_port.to_string()),
            }));
        }
        if !has_x_proto {
            request.push_block(kawa::Block::Header(kawa::Pair {
                key: kawa::Store::Static(b"X-Forwarded-Proto"),
                val: kawa::Store::Static(proto.as_bytes()),
            }));
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

        if self.method == Some(Method::Head) {
            response.parsing_phase = kawa::ParsingPhase::Terminated;
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

    pub fn reset(&mut self) {
        self.keep_alive_backend = true;
        self.keep_alive_frontend = true;
        self.sticky_session_found = None;
        self.method = None;
        self.authority = None;
        self.path = None;
        self.status = None;
        self.reason = None;
        self.user_agent = None;
    }

    pub fn log_context(&self) -> LogContext<'_> {
        LogContext {
            request_id: self.id,
            cluster_id: self.cluster_id.as_deref(),
            backend_id: self.backend_id.as_deref(),
        }
    }
}
