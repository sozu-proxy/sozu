use std::{
    io::Write as _,
    net::{IpAddr, SocketAddr},
    str::from_utf8,
};

use rusty_ulid::Ulid;
use sozu_command_lib::logging::LogContext;

use crate::{
    Protocol, RetrieveClusterError,
    pool::Checkout,
    protocol::{
        http::{GenericHttpStream, Method, parser::compare_no_case},
        pipe::WebSocketContext,
    },
};

#[cfg(feature = "opentelemetry")]
fn parse_traceparent(val: &kawa::Store, buf: &[u8]) -> Option<([u8; 32], [u8; 16])> {
    let val = val.data(buf);
    let (version, val) = parse_hex::<2>(val)?;
    if version.as_slice() != b"00" {
        return None;
    }
    let val = skip_separator(val)?;
    let (trace_id, val) = parse_hex::<32>(val)?;
    let val = skip_separator(val)?;
    let (parent_id, val) = parse_hex::<16>(val)?;
    let val = skip_separator(val)?;
    let (_, val) = parse_hex::<2>(val)?;
    val.is_empty().then_some((trace_id, parent_id))
}

#[cfg(feature = "opentelemetry")]
fn parse_hex<const N: usize>(buf: &[u8]) -> Option<([u8; N], &[u8])> {
    let val: [u8; N] = buf.get(..N)?.try_into().unwrap();
    val.iter()
        .all(|c| c.is_ascii_hexdigit())
        .then_some((val, &buf[N..]))
}

#[cfg(feature = "opentelemetry")]
fn skip_separator(buf: &[u8]) -> Option<&[u8]> {
    buf.first().filter(|b| **b == b'-').map(|_| &buf[1..])
}

#[cfg(feature = "opentelemetry")]
fn random_id<const N: usize>() -> [u8; N] {
    use rand::RngExt;
    const CHARSET: &[u8] = b"0123456789abcdef";
    let mut rng = rand::rng();
    let mut buf = [0; N];
    buf.fill_with(|| {
        let n = rng.random_range(0..CHARSET.len());
        CHARSET[n]
    });
    buf
}

#[cfg(feature = "opentelemetry")]
fn build_traceparent(trace_id: &[u8; 32], parent_id: &[u8; 16]) -> [u8; 55] {
    let mut buf = [0; 55];
    buf[..3].copy_from_slice(b"00-");
    buf[3..35].copy_from_slice(trace_id);
    buf[35] = b'-';
    buf[36..52].copy_from_slice(parent_id);
    buf[52..55].copy_from_slice(b"-01");
    buf
}

/// Write the ";for=..;by=.." portion of a Forwarded header into `buf`
/// without heap-allocating a `String`.
fn write_forwarded_for_by(buf: &mut Vec<u8>, peer_ip: IpAddr, peer_port: u16, public_ip: IpAddr) {
    buf.extend_from_slice(b";for=\"");
    let _ = write!(buf, "{peer_ip}");
    buf.push(b':');
    let mut port_buf = itoa::Buffer::new();
    buf.extend_from_slice(port_buf.format(peer_port).as_bytes());
    buf.extend_from_slice(b"\";by=");
    match public_ip {
        IpAddr::V4(_) => {
            let _ = write!(buf, "{public_ip}");
        }
        IpAddr::V6(_) => {
            buf.push(b'"');
            let _ = write!(buf, "{public_ip}");
            buf.push(b'"');
        }
    }
}

/// Write ", proto=<proto>;for=..;by=.." (the suffix appended to an existing Forwarded value).
fn write_forwarded_suffix(
    buf: &mut Vec<u8>,
    proto: &str,
    peer_ip: IpAddr,
    peer_port: u16,
    public_ip: IpAddr,
) {
    buf.extend_from_slice(b", proto=");
    buf.extend_from_slice(proto.as_bytes());
    write_forwarded_for_by(buf, peer_ip, peer_port, public_ip);
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
    pub otel: Option<sozu_command::logging::OpenTelemetry>,

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
    /// the address of the backend server
    pub backend_address: Option<SocketAddr>,
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
            otel: Default::default(),

            backend_address: None,
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
                self.sticky_session_found = from_utf8(val).ok().map(ToOwned::to_owned);
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
        let mut traceparent: Option<&mut kawa::Pair> = None;
        #[cfg(feature = "opentelemetry")]
        let mut tracestate: Option<&mut kawa::Pair> = None;
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
                        let mut port_buf = itoa::Buffer::new();
                        let expected = port_buf.format(public_port);
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
                        if compare_no_case(key, b"traceparent") {
                            if let Some(hdr) = traceparent {
                                hdr.elide();
                            }
                            traceparent = Some(header);
                        } else if compare_no_case(key, b"tracestate") {
                            if let Some(hdr) = tracestate {
                                hdr.elide();
                            }
                            tracestate = Some(header);
                        }
                    }
                }
                _ => {}
            }
        }

        #[cfg(feature = "opentelemetry")]
        let (otel, has_traceparent) = {
            let mut otel = sozu_command_lib::logging::OpenTelemetry::default();
            let tp = traceparent
                .as_ref()
                .and_then(|hdr| parse_traceparent(&hdr.val, buf))
                .map(|(trace_id, parent_id)| (trace_id, Some(parent_id)));
            // Remove tracestate if no traceparent is present
            if let (None, Some(tracestate)) = (tp, tracestate) {
                tracestate.elide();
            }
            let (trace_id, parent_id) = tp.unwrap_or_else(|| (random_id(), None));
            otel.trace_id = trace_id;
            otel.parent_span_id = parent_id;
            otel.span_id = random_id();
            // Modify header if present
            if let Some(id) = &mut traceparent {
                let new_val = build_traceparent(&otel.trace_id, &otel.span_id);
                id.val.modify(buf, &new_val);
            }
            (otel, traceparent.is_some())
        };

        // If session_address is set:
        // - append its ip address to the list of "X-Forwarded-For" if it was found, creates it if not
        // - append "proto=[PROTO];for=[PEER];by=[PUBLIC]" to the list of "Forwarded" if it was found, creates it if not
        if let Some(peer_addr) = self.session_address {
            let peer_ip = peer_addr.ip();
            let peer_port = peer_addr.port();
            let has_x_for = x_for.is_some();
            let has_forwarded = forwarded.is_some();

            // Buffer for building header values — ownership is transferred to Store
            // via `take`, so each header gets its own allocation.
            let mut hdr_buf = Vec::with_capacity(128);

            if let Some(header) = x_for {
                hdr_buf.extend_from_slice(header.val.data(buf));
                let _ = write!(hdr_buf, ", {peer_ip}");
                header.val = kawa::Store::from_vec(std::mem::take(&mut hdr_buf));
            }
            if let Some(header) = &mut forwarded {
                hdr_buf.extend_from_slice(header.val.data(buf));
                write_forwarded_suffix(&mut hdr_buf, proto, peer_ip, peer_port, public_ip);
                header.val = kawa::Store::from_vec(std::mem::take(&mut hdr_buf));
            }

            if !has_x_for {
                let _ = write!(hdr_buf, "{peer_ip}");
                request.push_block(kawa::Block::Header(kawa::Pair {
                    key: kawa::Store::Static(b"X-Forwarded-For"),
                    val: kawa::Store::from_vec(std::mem::take(&mut hdr_buf)),
                }));
            }
            if !has_forwarded {
                hdr_buf.extend_from_slice(b"proto=");
                hdr_buf.extend_from_slice(proto.as_bytes());
                write_forwarded_for_by(&mut hdr_buf, peer_ip, peer_port, public_ip);
                request.push_block(kawa::Block::Header(kawa::Pair {
                    key: kawa::Store::Static(b"Forwarded"),
                    val: kawa::Store::from_vec(std::mem::take(&mut hdr_buf)),
                }));
            }
        }

        #[cfg(feature = "opentelemetry")]
        {
            if !has_traceparent {
                let val = build_traceparent(&otel.trace_id, &otel.span_id);
                request.push_block(kawa::Block::Header(kawa::Pair {
                    key: kawa::Store::Static(b"traceparent"),
                    val: kawa::Store::from_slice(&val),
                }));
            }
            self.otel = Some(otel);
        }

        if !has_x_port {
            let mut port_buf = itoa::Buffer::new();
            let port_str = port_buf.format(public_port);
            request.push_block(kawa::Block::Header(kawa::Pair {
                key: kawa::Store::Static(b"X-Forwarded-Port"),
                val: kawa::Store::from_slice(port_str.as_bytes()),
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
                let mut cookie_buf =
                    Vec::with_capacity(self.sticky_name.len() + 1 + sticky_session.len() + 8);
                cookie_buf.extend_from_slice(self.sticky_name.as_bytes());
                cookie_buf.push(b'=');
                cookie_buf.extend_from_slice(sticky_session.as_bytes());
                cookie_buf.extend_from_slice(b"; Path=/");
                response.push_block(kawa::Block::Header(kawa::Pair {
                    key: kawa::Store::Static(b"Set-Cookie"),
                    val: kawa::Store::from_vec(cookie_buf),
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

    pub fn extract_route(&self) -> Result<(&str, &str, &Method), RetrieveClusterError> {
        let given_method = self.method.as_ref().ok_or(RetrieveClusterError::NoMethod)?;
        let given_authority = self
            .authority
            .as_deref()
            .ok_or(RetrieveClusterError::NoHost)?;
        let given_path = self.path.as_deref().ok_or(RetrieveClusterError::NoPath)?;

        Ok((given_authority, given_path, given_method))
    }

    pub fn get_route(&self) -> String {
        if let Some(method) = &self.method {
            if let Some(authority) = &self.authority {
                if let Some(path) = &self.path {
                    return format!("{method} {authority}{path}");
                }
                return format!("{method} {authority}");
            }
            return format!("{method}");
        }
        String::new()
    }

    pub fn websocket_context(&self) -> WebSocketContext {
        WebSocketContext::Http {
            method: self.method.clone(),
            authority: self.authority.clone(),
            path: self.path.clone(),
            reason: self.reason.clone(),
            status: self.status,
        }
    }

    pub fn log_context(&self) -> LogContext<'_> {
        LogContext {
            request_id: self.id,
            cluster_id: self.cluster_id.as_deref(),
            backend_id: self.backend_id.as_deref(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    /// Helper to create a minimal HttpContext for testing.
    fn make_context() -> HttpContext {
        HttpContext::new(
            Ulid::generate(),
            Protocol::HTTP,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080),
            Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
                54321,
            )),
            "SERVERID".to_owned(),
        )
    }

    // ── extract_route ──────────────────────────────────────────────────

    #[test]
    fn test_extract_route_all_present() {
        let mut ctx = make_context();
        ctx.method = Some(Method::Get);
        ctx.authority = Some("example.com".to_owned());
        ctx.path = Some("/index.html".to_owned());

        let (authority, path, method) = ctx.extract_route().unwrap();
        assert_eq!(authority, "example.com");
        assert_eq!(path, "/index.html");
        assert_eq!(method, &Method::Get);
    }

    #[test]
    fn test_extract_route_no_method() {
        let mut ctx = make_context();
        ctx.authority = Some("example.com".to_owned());
        ctx.path = Some("/".to_owned());

        let err = ctx.extract_route().unwrap_err();
        assert!(matches!(err, RetrieveClusterError::NoMethod));
    }

    #[test]
    fn test_extract_route_no_host() {
        let mut ctx = make_context();
        ctx.method = Some(Method::Get);
        ctx.path = Some("/".to_owned());

        let err = ctx.extract_route().unwrap_err();
        assert!(matches!(err, RetrieveClusterError::NoHost));
    }

    #[test]
    fn test_extract_route_no_path() {
        let mut ctx = make_context();
        ctx.method = Some(Method::Get);
        ctx.authority = Some("example.com".to_owned());

        let err = ctx.extract_route().unwrap_err();
        assert!(matches!(err, RetrieveClusterError::NoPath));
    }

    // ── get_route ──────────────────────────────────────────────────────

    #[test]
    fn test_get_route_all_present() {
        let mut ctx = make_context();
        ctx.method = Some(Method::Get);
        ctx.authority = Some("example.com".to_owned());
        ctx.path = Some("/api/v1".to_owned());

        assert_eq!(ctx.get_route(), "GET example.com/api/v1");
    }

    #[test]
    fn test_get_route_method_and_authority_only() {
        let mut ctx = make_context();
        ctx.method = Some(Method::Post);
        ctx.authority = Some("example.com".to_owned());

        assert_eq!(ctx.get_route(), "POST example.com");
    }

    #[test]
    fn test_get_route_method_only() {
        let mut ctx = make_context();
        ctx.method = Some(Method::Delete);

        assert_eq!(ctx.get_route(), "DELETE");
    }

    #[test]
    fn test_get_route_empty() {
        let ctx = make_context();
        assert_eq!(ctx.get_route(), "");
    }

    // ── reset ──────────────────────────────────────────────────────────

    #[test]
    fn test_reset_clears_request_response_state() {
        let mut ctx = make_context();
        ctx.keep_alive_backend = false;
        ctx.keep_alive_frontend = false;
        ctx.sticky_session_found = Some("abc123".to_owned());
        ctx.method = Some(Method::Post);
        ctx.authority = Some("example.com".to_owned());
        ctx.path = Some("/upload".to_owned());
        ctx.status = Some(200);
        ctx.reason = Some("OK".to_owned());
        ctx.user_agent = Some("curl/7.81".to_owned());

        ctx.reset();

        assert!(ctx.keep_alive_backend);
        assert!(ctx.keep_alive_frontend);
        assert!(ctx.sticky_session_found.is_none());
        assert!(ctx.method.is_none());
        assert!(ctx.authority.is_none());
        assert!(ctx.path.is_none());
        assert!(ctx.status.is_none());
        assert!(ctx.reason.is_none());
        assert!(ctx.user_agent.is_none());
    }

    #[test]
    fn test_reset_preserves_connection_state() {
        let mut ctx = make_context();
        ctx.closing = true;
        ctx.cluster_id = Some("cluster-1".to_owned());
        ctx.backend_id = Some("backend-1".to_owned());
        ctx.sticky_session = Some("session-abc".to_owned());

        let original_id = ctx.id;
        let original_protocol = ctx.protocol;
        let original_public_address = ctx.public_address;

        ctx.reset();

        // Connection-level state is preserved
        assert!(ctx.closing);
        assert_eq!(ctx.cluster_id.as_deref(), Some("cluster-1"));
        assert_eq!(ctx.backend_id.as_deref(), Some("backend-1"));
        assert_eq!(ctx.sticky_session.as_deref(), Some("session-abc"));
        assert_eq!(ctx.id, original_id);
        assert_eq!(ctx.protocol, original_protocol);
        assert_eq!(ctx.public_address, original_public_address);
    }

    // ── traceparent / opentelemetry helpers ─────────────────────────────

    #[cfg(feature = "opentelemetry")]
    mod otel {
        use super::super::*;

        #[test]
        fn test_parse_hex_valid() {
            let (val, rest) = parse_hex::<4>(b"abcd1234").unwrap();
            assert_eq!(&val, b"abcd");
            assert_eq!(rest, b"1234");
        }

        #[test]
        fn test_parse_hex_exact_length() {
            let (val, rest) = parse_hex::<8>(b"01234567").unwrap();
            assert_eq!(&val, b"01234567");
            assert!(rest.is_empty());
        }

        #[test]
        fn test_parse_hex_too_short() {
            assert!(parse_hex::<4>(b"ab").is_none());
        }

        #[test]
        fn test_parse_hex_rejects_non_hex() {
            assert!(parse_hex::<4>(b"ghij").is_none());
        }

        #[test]
        fn test_parse_hex_rejects_uppercase_is_ok() {
            // Uppercase hex digits are valid
            let (val, _) = parse_hex::<4>(b"ABCD").unwrap();
            assert_eq!(&val, b"ABCD");
        }

        #[test]
        fn test_skip_separator_valid() {
            let rest = skip_separator(b"-hello").unwrap();
            assert_eq!(rest, b"hello");
        }

        #[test]
        fn test_skip_separator_wrong_char() {
            assert!(skip_separator(b"+hello").is_none());
        }

        #[test]
        fn test_skip_separator_empty() {
            assert!(skip_separator(b"").is_none());
        }

        #[test]
        fn test_build_traceparent_format() {
            let trace_id: [u8; 32] = *b"4bf92f3577b34da6a3ce929d0e0e4736";
            let parent_id: [u8; 16] = *b"00f067aa0ba902b7";

            let result = build_traceparent(&trace_id, &parent_id);
            assert_eq!(
                &result,
                b"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
            );
        }

        #[test]
        fn test_build_traceparent_length() {
            let trace_id = [b'a'; 32];
            let parent_id = [b'b'; 16];
            let result = build_traceparent(&trace_id, &parent_id);
            // Format: "00-" (3) + trace_id (32) + "-" (1) + parent_id (16) + "-01" (3) = 55
            assert_eq!(result.len(), 55);
        }

        #[test]
        fn test_parse_traceparent_valid() {
            let input = b"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
            let store = kawa::Store::Static(input);
            let (trace_id, parent_id) = parse_traceparent(&store, input).unwrap();
            assert_eq!(&trace_id, b"4bf92f3577b34da6a3ce929d0e0e4736");
            assert_eq!(&parent_id, b"00f067aa0ba902b7");
        }

        #[test]
        fn test_parse_traceparent_sampled_flag_zero() {
            let input = b"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00";
            let store = kawa::Store::Static(input);
            let result = parse_traceparent(&store, input);
            assert!(result.is_some());
        }

        #[test]
        fn test_parse_traceparent_wrong_version() {
            let input = b"01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
            let store = kawa::Store::Static(input);
            assert!(parse_traceparent(&store, input).is_none());
        }

        #[test]
        fn test_parse_traceparent_too_short() {
            let input = b"00-4bf9";
            let store = kawa::Store::Static(input);
            assert!(parse_traceparent(&store, input).is_none());
        }

        #[test]
        fn test_parse_traceparent_trailing_data() {
            // Extra characters after the trace-flags should cause rejection
            let input = b"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra";
            let store = kawa::Store::Static(input);
            assert!(parse_traceparent(&store, input).is_none());
        }

        #[test]
        fn test_parse_traceparent_missing_separator() {
            let input = b"004bf92f3577b34da6a3ce929d0e0e473600f067aa0ba902b701";
            let store = kawa::Store::Static(input);
            assert!(parse_traceparent(&store, input).is_none());
        }

        #[test]
        fn test_parse_build_roundtrip() {
            let trace_id: [u8; 32] = *b"4bf92f3577b34da6a3ce929d0e0e4736";
            let parent_id: [u8; 16] = *b"00f067aa0ba902b7";

            // Build a traceparent from known IDs
            let built = build_traceparent(&trace_id, &parent_id);

            // Verify that the built value matches the expected static string,
            // then parse that static string back to confirm roundtrip.
            let expected: &[u8] =
                b"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
            assert_eq!(&built[..], expected);

            let store = kawa::Store::Static(expected);
            let (parsed_trace_id, parsed_parent_id) =
                parse_traceparent(&store, expected).unwrap();

            assert_eq!(parsed_trace_id, trace_id);
            assert_eq!(parsed_parent_id, parent_id);
        }
    }
}
