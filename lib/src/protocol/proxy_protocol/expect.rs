//! Inbound PROXY-v2 expectation state.
//!
//! Reads bytes from the freshly accepted front-end socket until a complete
//! PROXY v2 header has been parsed (`parse_v2_header`), captures the peer
//! address pair, and transitions the session to the configured downstream
//! protocol (typically `Pipe` for TCP listeners). Bounded by
//! `MAX_LOOP_ITERATIONS` to defend against malformed/empty headers.

use std::{cell::RefCell, rc::Rc};

use mio::{net::TcpStream, *};
use nom::{Err, HexDisplay};
use rusty_ulid::Ulid;
use sozu_command::{
    config::MAX_LOOP_ITERATIONS,
    logging::{LogContext, ansi_palette},
};

use super::{header::ProxyAddr, parser::parse_v2_header};
use crate::metrics::names;
use crate::{
    Protocol, Readiness, SessionMetrics, StateResult,
    pool::Checkout,
    protocol::{
        SessionResult, SessionState,
        pipe::{Pipe, WebSocketContext},
    },
    socket::{SocketHandler, SocketResult},
    sozu_command::ready::Ready,
    tcp::TcpListener,
    timer::TimeoutContainer,
};

const V2_FIXED_HEADER_LEN: usize = 16;
const MAX_HEADER_LEN: usize = 232;

/// Module-level prefix used on every log line emitted from this module when
/// no per-session state is in scope. Produces a bold bright-white
/// `PROXY-EXPECT` label (uniform across every protocol) when the logger is in
/// colored mode.
macro_rules! log_module_context {
    () => {{
        let (open, reset, _, _, _) = ansi_palette();
        format!(
            "{open}PROXY-EXPECT{reset}\t >>>",
            open = open,
            reset = reset
        )
    }};
}

/// Per-session prefix for log lines emitted with an
/// [`ExpectProxyProtocol`] in scope. Renders the canonical
/// `[ulid - - -]\tPROXY-EXPECT\tSession(...)\t >>>` envelope so operators can
/// grep these lines alongside `MUX-*`, `RUSTLS`, and `PIPE` traffic for the
/// same session.
macro_rules! log_context {
    ($self:expr) => {{
        let (open, reset, grey, gray, white) = ansi_palette();
        format!(
            "{gray}{ctx}{reset}\t{open}PROXY-EXPECT{reset}\t{grey}Session{reset}({gray}frontend{reset}={white}{frontend}{reset}, {gray}index{reset}={white}{index}{reset}, {gray}readiness{reset}={white}{readiness}{reset})\t >>>",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            ctx = $self.log_context(),
            frontend = $self.frontend_token.0,
            index = $self.index,
            readiness = $self.frontend_readiness,
        )
    }};
}

/// Legacy stage labels retained for library API compatibility.
///
/// `ExpectProxyProtocol` now derives its private read target from the v2
/// header's length field instead of using these stages.
#[derive(Clone, Copy)]
pub enum HeaderLen {
    V4,
    V6,
    Unix,
}

// TODO: should have a backend
pub struct ExpectProxyProtocol<Front: SocketHandler> {
    pub addresses: Option<ProxyAddr>,
    pub container_frontend_timeout: TimeoutContainer,
    frontend_buffer: [u8; MAX_HEADER_LEN],
    pub frontend_readiness: Readiness,
    pub frontend_token: Token,
    pub frontend: Front,
    read_target: usize,
    index: usize,
    pub request_id: Ulid,
}

impl<Front: SocketHandler> ExpectProxyProtocol<Front> {
    /// Instantiate a new ExpectProxyProtocol SessionState with:
    /// - frontend_interest: READABLE | HUP | ERROR
    /// - frontend_event: EMPTY
    pub fn new(
        container_frontend_timeout: TimeoutContainer,
        frontend: Front,
        frontend_token: Token,
        request_id: Ulid,
    ) -> Self {
        ExpectProxyProtocol {
            addresses: None,
            container_frontend_timeout,
            frontend_buffer: [0; MAX_HEADER_LEN],
            frontend_readiness: Readiness {
                interest: Ready::READABLE | Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            frontend_token,
            frontend,
            read_target: V2_FIXED_HEADER_LEN,
            index: 0,
            request_id,
        }
    }

    pub fn readable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        let total_len = self.read_target;

        // Anti-oversized-header / partial-read invariant: the accumulation
        // cursor never runs past the staging window, and the per-stage target
        // never exceeds the fixed 232-byte buffer (the absolute upper bound on
        // a PROXY-v2 header). A violation here would slice-panic on the read
        // below; the asserts name it as a logic bug.
        debug_assert!(
            self.index <= total_len,
            "read cursor must not exceed the current stage target"
        );
        debug_assert!(
            total_len <= self.frontend_buffer.len(),
            "stage target must fit the fixed proxy-protocol buffer"
        );

        let index_before = self.index;
        let (sz, socket_result) = self
            .frontend
            .socket_read(&mut self.frontend_buffer[self.index..total_len]);
        // The socket may only fill the slice it was handed, so a successful
        // read advances the cursor by at most the remaining stage capacity.
        debug_assert!(
            sz <= total_len - index_before,
            "socket_read cannot return more bytes than the slice it was given"
        );
        trace!(
            "{} read {} bytes and res={:?}, total_len = {}",
            log_context!(self),
            sz,
            socket_result,
            total_len
        );

        if sz > 0 {
            self.index += sz;
            // Partial-read accumulation is strictly monotonic and stays within
            // the bounded buffer: bytes-consumed advances by exactly `sz` and
            // never exceeds the buffer length (anti-oversized-header bound).
            debug_assert_eq!(
                self.index,
                index_before + sz,
                "read cursor advances by exactly the bytes just read"
            );
            debug_assert!(
                self.index <= self.frontend_buffer.len(),
                "accumulated bytes must never exceed the fixed buffer bound"
            );

            count!(names::backend::BYTES_IN, sz as i64);
            metrics.bin += sz;
        } else {
            debug_assert_eq!(
                self.index, index_before,
                "a non-positive read must leave the cursor unchanged"
            );
            self.frontend_readiness.event.remove(Ready::READABLE);
        }

        match socket_result {
            SocketResult::Error => {
                error!(
                    "{} front socket error, closing the connection (read {}, wrote {})",
                    log_context!(self),
                    metrics.bin,
                    metrics.bout
                );
                incr!(names::proxy_protocol::ERRORS);
                self.frontend_readiness.reset();
                return SessionResult::Close;
            }
            SocketResult::WouldBlock => {
                self.frontend_readiness.event.remove(Ready::READABLE);
            }
            SocketResult::Closed => {
                // Socket closed before any proxy-protocol bytes were received.
                // This is the typical HAProxy bare TCP healthcheck pattern
                // (SYN/ACK/FIN without send-proxy). Close immediately instead
                // of waiting for request_timeout (default 10s), which would
                // create zombie sessions consuming nb_connections quota.
                if self.index == 0 {
                    trace!(
                        "{} socket closed with 0 bytes, closing session",
                        log_context!(self)
                    );
                    return SessionResult::Close;
                }
            }
            SocketResult::Continue => {}
        }

        match parse_v2_header(&self.frontend_buffer[..self.index]) {
            Ok((rest, header)) => {
                // Completion postcondition: the parser consumed a prefix of the
                // accumulated bytes, so the unparsed remainder is no larger than
                // what we fed it (a complete header was recognized within the
                // bound).
                debug_assert!(
                    rest.len() <= self.index,
                    "parser remainder cannot exceed the accumulated input"
                );
                debug_assert!(
                    rest.is_empty(),
                    "exact-length reads must leave application payload in the socket"
                );
                trace!(
                    "{} got expect header: {:?}, rest.len() = {}",
                    log_context!(self),
                    header,
                    rest.len()
                );
                self.addresses = Some(header.addr);
                SessionResult::Upgrade
            }
            Err(Err::Incomplete(_)) if self.index < self.read_target => SessionResult::Continue,
            Err(Err::Incomplete(_)) if self.read_target == V2_FIXED_HEADER_LEN => {
                let declared_len =
                    u16::from_be_bytes([self.frontend_buffer[14], self.frontend_buffer[15]])
                        as usize;
                let declared_total = V2_FIXED_HEADER_LEN + declared_len;

                if declared_total > self.frontend_buffer.len() {
                    error!(
                        "{} proxy protocol header declares {} bytes, exceeding maximum size ({} bytes), closing",
                        log_context!(self),
                        declared_total,
                        self.frontend_buffer.len()
                    );
                    incr!(names::proxy_protocol::ERRORS);
                    self.frontend_readiness.reset();
                    return SessionResult::Close;
                }

                if declared_total > V2_FIXED_HEADER_LEN {
                    self.read_target = declared_total;
                    return SessionResult::Continue;
                }

                error!(
                    "{} proxy protocol header is incomplete at its declared length ({} bytes), closing",
                    log_context!(self),
                    declared_total
                );
                incr!(names::proxy_protocol::ERRORS);
                self.frontend_readiness.reset();
                SessionResult::Close
            }
            Err(Err::Incomplete(_)) => {
                error!(
                    "{} proxy protocol header is incomplete at its declared length ({} bytes), closing",
                    log_context!(self),
                    self.read_target
                );
                incr!(names::proxy_protocol::ERRORS);
                self.frontend_readiness.reset();
                SessionResult::Close
            }
            Err(Err::Error(e)) | Err(Err::Failure(e)) => {
                error!(
                    "{} parse error, closing the connection:\n{}",
                    log_context!(self),
                    e.input.to_hex(16)
                );
                incr!(names::proxy_protocol::ERRORS);
                self.frontend_readiness.reset();
                SessionResult::Close
            }
        }
    }

    pub fn front_socket(&self) -> &TcpStream {
        self.frontend.socket_ref()
    }

    pub fn into_pipe(
        self,
        front_buf: Checkout,
        back_buf: Checkout,
        backend_socket: Option<TcpStream>,
        backend_token: Option<Token>,
        listener: Rc<RefCell<TcpListener>>,
    ) -> Pipe<Front, TcpListener> {
        // Prefer the source address parsed from the PROXY-v2 header over
        // the TCP `peer_addr` so the pipe phase records the real client
        // — `peer_addr` here is the upstream PROXY-emitter (an LB / edge
        // proxy / health-check probe), not the originating client.
        // Falls back to `peer_addr` whenever `self.addresses` is `AfUnspec`:
        // either the header declared AF_UNSPEC, or it carried
        // `Command::Local`, whose address block `parse_v2_header` discards per
        // the HAProxy PROXY protocol specification §2.2. A `LOCAL` header is
        // NOT required by the wire format to leave that block empty — only
        // HAProxy's own emitter pairs the two — so the discard happens in the
        // parser and is what makes this fallback true.
        let addr = self
            .addresses
            .as_ref()
            .and_then(|pa| pa.source())
            .or_else(|| self.front_socket().peer_addr().ok());

        let mut pipe = Pipe::new(
            back_buf,
            None,
            backend_socket,
            None,
            None,
            Some(self.container_frontend_timeout),
            None,
            front_buf,
            self.frontend_token,
            self.frontend,
            listener,
            Protocol::TCP,
            self.request_id,
            self.request_id,
            addr,
            WebSocketContext::Tcp,
        );

        pipe.frontend_readiness.event = self.frontend_readiness.event;

        if let Some(backend_token) = backend_token {
            pipe.set_back_token(backend_token);
        }

        pipe
    }

    pub fn log_context(&self) -> LogContext<'_> {
        LogContext {
            session_id: self.request_id,
            request_id: None,
            cluster_id: None,
            backend_id: None,
        }
    }
}

impl<Front: SocketHandler> SessionState for ExpectProxyProtocol<Front> {
    fn ready(
        &mut self,
        _session: Rc<RefCell<dyn crate::ProxySession>>,
        _proxy: Rc<RefCell<dyn crate::L7Proxy>>,
        metrics: &mut SessionMetrics,
    ) -> SessionResult {
        let mut counter = 0;

        while counter < MAX_LOOP_ITERATIONS {
            let frontend_interest = self.frontend_readiness.filter_interest();

            trace!(
                "{} {:?} -> None",
                log_context!(self),
                self.frontend_readiness
            );

            // A client half-close can arrive in the same `READABLE | HUP`
            // event as the header it follows (Linux coalesces
            // `EPOLLIN | EPOLLRDHUP`), so HUP closes only once every readable
            // byte has been consumed: a complete header still upgrades and
            // hands HUP on to the next stage (sozu-proxy/sozu#1823).
            // `readable` clears READABLE on every zero-byte read, so an empty
            // or truncated header reaches this close on the next turn.
            if self.frontend_readiness.event.is_hup() && !frontend_interest.is_readable() {
                return SessionResult::Close;
            }

            if frontend_interest.is_empty() {
                break;
            }

            if frontend_interest.is_readable() {
                let session_result = self.readable(metrics);
                if session_result != SessionResult::Continue {
                    return session_result;
                }
            }

            if frontend_interest.is_error() {
                error!("{} front error, disconnecting", log_context!(self));
                self.frontend_readiness.interest = Ready::EMPTY;

                return SessionResult::Close;
            }

            let counter_before = counter;
            counter += 1;
            // The readiness loop is bounded by MAX_LOOP_ITERATIONS; the counter
            // advances by exactly one per turn and stays within the cap, so the
            // loop cannot spin unbounded on a stuck readiness state.
            debug_assert_eq!(counter, counter_before + 1, "loop counter advances by one");
            debug_assert!(
                counter <= MAX_LOOP_ITERATIONS,
                "loop counter must stay within the iteration cap"
            );
        }

        if counter >= MAX_LOOP_ITERATIONS {
            error!(
                "{} handling session went through {} iterations, there's a probable infinite loop bug, closing the connection",
                log_context!(self),
                MAX_LOOP_ITERATIONS
            );
            incr!(names::http::INFINITE_LOOP_ERROR);

            self.print_state("");

            return SessionResult::Close;
        }

        SessionResult::Continue
    }

    fn update_readiness(&mut self, token: Token, events: Ready) {
        if self.frontend_token == token {
            self.frontend_readiness.event |= events;
        }
    }

    fn timeout(&mut self, token: Token, _metrics: &mut SessionMetrics) -> StateResult {
        if self.frontend_token == token {
            self.container_frontend_timeout.triggered();
            return StateResult::CloseSession;
        }

        error!(
            "{} got timeout for an invalid token: {:?}",
            log_module_context!(),
            token
        );
        StateResult::CloseSession
    }

    fn cancel_timeouts(&mut self) {
        self.container_frontend_timeout.cancel();
    }

    fn print_state(&self, context: &str) {
        error!(
            "{} {} Session(Expect)\n\tFrontend:\n\t\ttoken: {:?}\treadiness: {:?}",
            log_context!(self),
            context,
            self.frontend_token,
            self.frontend_readiness
        );
    }
}

#[cfg(test)]
mod expect_test {
    use std::{
        io::Write,
        net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream as StdTcpStream},
        sync::{Arc, Barrier},
        thread::{self, JoinHandle},
        time::Duration,
    };

    use mio::net::TcpListener;
    use rusty_ulid::Ulid;

    use super::*;
    use crate::protocol::proxy_protocol::header::*;

    /// Address pair the upfront middleware encapsulates in its header. Under
    /// `Command::Local` these are the *forged* values a crafted peer would
    /// send: the wire format lets a `LOCAL` header carry a fully populated
    /// address block.
    fn header_src() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(125, 25, 10, 1)), 8080)
    }

    fn header_dst() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 4, 5, 8)), 4200)
    }

    // Flow diagram of the test below
    //                [connect]   [send proxy protocol]
    //upfront proxy  ----------------------X
    //              /     |           |
    //  sozu     ---------v-----------v----X
    //
    // Drives a full `readable` loop against a real loopback connection and
    // returns the source address `into_pipe` would attribute to the client,
    // i.e. `self.addresses.as_ref().and_then(|pa| pa.source())` -- `None`
    // meaning `into_pipe` falls back to the front socket's `peer_addr`.
    fn attributed_source_for(command: Command) -> Option<SocketAddr> {
        setup_test_logger!();
        let listener = TcpListener::bind("127.0.0.1:0".parse().expect("parse address error"))
            .expect("could not bind the middleware listener");
        let middleware_addr = listener
            .local_addr()
            .expect("the middleware listener must expose its address");
        let barrier = Arc::new(Barrier::new(2));

        let upfront = start_upfront_middleware(middleware_addr, barrier.clone(), command);
        let addresses = start_middleware(listener, barrier);

        upfront.join().expect("should join");

        addresses.as_ref().and_then(ProxyAddr::source)
    }

    #[test]
    fn middleware_should_receive_proxy_protocol_header_from_an_upfront_middleware() {
        assert_eq!(
            attributed_source_for(Command::Proxy),
            Some(header_src()),
            "a PROXY header still carries the real client through to the pipe phase"
        );
    }

    /// The PROXY protocol `LOCAL` command (ver/cmd `0x20`) describes a
    /// connection the upstream proxy originated itself -- typically a health
    /// check -- and the HAProxy PROXY protocol specification §2.2 requires the
    /// receiver to discard its address block. Nothing on the wire pairs
    /// `LOCAL` with `AF_UNSPEC`; only HAProxy's own emitter happens to. The
    /// header written by the upfront middleware below is exactly what
    /// `HeaderV2::new(Command::Local, ..)` emits: ver/cmd `0x20`, family
    /// `0x11`, and a populated 12-byte `AF_INET` block.
    ///
    /// `into_pipe` prefers `self.addresses.source()` over the socket's
    /// `peer_addr`, so before the fix that forged pair became the client
    /// address in the access logs, in `X-Real-IP` and in the
    /// `max_connections_per_ip` counters.
    ///
    /// To SEE THIS RED: in `parse_v2_header`
    /// (`lib/src/protocol/proxy_protocol/parser.rs`), replace the
    /// `Command::Local => ProxyAddr::AfUnspec` arm of the `addr` binding with
    /// `parsed_addr` -- this then yields `Some(125.25.10.1:8080)`.
    #[test]
    fn a_local_command_header_attributes_no_source_address() {
        assert_eq!(
            attributed_source_for(Command::Local),
            None,
            "a LOCAL header must attribute no source, so into_pipe falls back to peer_addr"
        );
    }

    #[test]
    fn oversized_declared_header_is_rejected_after_the_fixed_prelude() {
        setup_test_logger!();
        let listener = TcpListener::bind("127.0.0.1:0".parse().expect("parse address error"))
            .expect("could not bind the middleware listener");
        let middleware_addr = listener
            .local_addr()
            .expect("the middleware listener must expose its address");

        let mut sender = StdTcpStream::connect(middleware_addr).expect("connect sender");
        let mut wire = vec![
            0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A, 0x20, 0x00,
            0x00, 0xD9, // 16 + 217 = 233, one byte beyond capacity
        ];
        wire.extend_from_slice(b"sentinel bytes must remain unread");
        sender.write_all(&wire).expect("write oversized prelude");

        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let frontend = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "accept deadline elapsed"
                    );
                    thread::yield_now();
                }
                Err(error) => panic!("middleware accept failed: {error}"),
            }
        };

        let mut metrics = SessionMetrics::new(None);
        let mut expect = ExpectProxyProtocol::new(
            TimeoutContainer::new(Duration::from_secs(10), Token(0)),
            frontend,
            Token(0),
            Ulid::generate(),
        );

        assert_eq!(expect.readable(&mut metrics), SessionResult::Close);
        assert_eq!(
            metrics.bin, V2_FIXED_HEADER_LEN,
            "the capacity check must happen before a second socket read"
        );

        drop(sender);
    }

    // Accept connection from an upfront proxy and expect to read a proxy protocol header in this stream.
    fn start_middleware(
        upfront_middleware_conn_listener: TcpListener,
        barrier: Arc<Barrier>,
    ) -> Option<ProxyAddr> {
        let session_stream;
        barrier.wait();

        // mio::TcpListener use a nonblocking mode so we have to loop on accept
        loop {
            if let Ok((stream, _addr)) = upfront_middleware_conn_listener.accept() {
                session_stream = stream;
                break;
            }
        }

        let mut session_metrics = SessionMetrics::new(None);
        let container_frontend_timeout = TimeoutContainer::new(Duration::from_secs(10), Token(0));
        let mut expect_pp = ExpectProxyProtocol::new(
            container_frontend_timeout,
            session_stream,
            Token(0),
            Ulid::generate(),
        );

        let mut res = SessionResult::Continue;
        while res == SessionResult::Continue {
            res = expect_pp.readable(&mut session_metrics);
        }

        if res != SessionResult::Upgrade {
            panic!("Should receive a complete proxy protocol header, res = {res:?}");
        };

        expect_pp.addresses
    }

    // Connect to the next middleware and send a proxy protocol header
    fn start_upfront_middleware(
        next_middleware_addr: SocketAddr,
        barrier: Arc<Barrier>,
        command: Command,
    ) -> JoinHandle<()> {
        thread::spawn(move || {
            let proxy_protocol = HeaderV2::new(command, header_src(), header_dst()).into_bytes();

            barrier.wait();
            match StdTcpStream::connect(next_middleware_addr) {
                Ok(mut stream) => {
                    stream.write_all(&proxy_protocol).unwrap();
                }
                Err(e) => panic!("could not connect to the next middleware: {e}"),
            };
        })
    }

    /// `SessionState::ready` (the HTTP and HTTPS listeners' expect stage)
    /// never dials nor registers anything, so the session and proxy handles
    /// it is given are never touched.
    struct UnusedSession;

    impl crate::ProxySession for UnusedSession {
        fn protocol(&self) -> Protocol {
            unreachable!("the expect stage never asks the session")
        }
        fn ready(
            &mut self,
            _session: Rc<RefCell<dyn crate::ProxySession>>,
        ) -> crate::SessionIsToBeClosed {
            unreachable!("the expect stage never asks the session")
        }
        fn update_readiness(&mut self, _token: Token, _events: Ready) {
            unreachable!("the expect stage never asks the session")
        }
        fn close(&mut self) {
            unreachable!("the expect stage never asks the session")
        }
        fn timeout(&mut self, _t: Token) -> crate::SessionIsToBeClosed {
            unreachable!("the expect stage never asks the session")
        }
        fn last_event(&self) -> std::time::Instant {
            unreachable!("the expect stage never asks the session")
        }
        fn print_session(&self) {
            unreachable!("the expect stage never asks the session")
        }
        fn frontend_token(&self) -> Token {
            unreachable!("the expect stage never asks the session")
        }
        fn shutting_down(&mut self) -> crate::SessionIsToBeClosed {
            unreachable!("the expect stage never asks the session")
        }
    }

    struct UnusedProxy;

    impl crate::L7Proxy for UnusedProxy {
        fn kind(&self) -> sozu_command::proto::command::ListenerType {
            unreachable!("the expect stage never asks the proxy")
        }
        fn register_socket(
            &self,
            _socket: &mut TcpStream,
            _token: Token,
            _interest: Interest,
        ) -> Result<(), std::io::Error> {
            unreachable!("the expect stage never asks the proxy")
        }
        fn add_session(&self, _session: Rc<RefCell<dyn crate::ProxySession>>) -> Token {
            unreachable!("the expect stage never asks the proxy")
        }
        fn remove_session(&self, _token: Token) -> bool {
            unreachable!("the expect stage never asks the proxy")
        }
        fn clusters(
            &self,
        ) -> &std::collections::HashMap<
            sozu_command::state::ClusterId,
            sozu_command::proto::command::Cluster,
        > {
            unreachable!("the expect stage never asks the proxy")
        }
        fn sessions(&self) -> Rc<RefCell<crate::server::SessionManager>> {
            unreachable!("the expect stage never asks the proxy")
        }
    }

    /// Write `wire` from a loopback client, half-close the client's write
    /// side, and hand the accepted socket to a fresh `ExpectProxyProtocol`
    /// together with the readiness a real epoll instance reports for it once
    /// both the bytes and the FIN are queued -- on Linux one coalesced
    /// `READABLE | HUP` event (`EPOLLIN | EPOLLRDHUP`). The client is
    /// returned so its still-open read side outlives the call.
    #[cfg(target_os = "linux")]
    fn half_closed_expect(wire: &[u8]) -> (ExpectProxyProtocol<TcpStream>, StdTcpStream) {
        use std::net::{Shutdown, TcpListener as StdTcpListener};

        let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind the expect listener");
        let mut client = StdTcpStream::connect(
            listener
                .local_addr()
                .expect("the expect listener must expose its address"),
        )
        .expect("connect the client");
        let (accepted, _) = listener.accept().expect("accept the client");
        accepted
            .set_nonblocking(true)
            .expect("accepted socket nonblocking");
        client.write_all(wire).expect("write the request stream");
        client
            .shutdown(Shutdown::Write)
            .expect("half-close the client write side");

        let mut frontend = TcpStream::from_std(accepted);
        let mut poll = Poll::new().expect("create poll");
        poll.registry()
            .register(&mut frontend, Token(0), Interest::READABLE)
            .expect("register the accepted socket");
        let mut events = Events::with_capacity(4);
        poll.poll(&mut events, Some(Duration::from_secs(2)))
            .expect("poll the accepted socket");
        let ready = events
            .iter()
            .find(|event| event.token() == Token(0))
            .map(Ready::from)
            .expect("the queued request and FIN produce an epoll event");
        assert!(
            ready.is_readable() && ready.is_hup(),
            "the queued bytes and FIN must coalesce into READABLE|HUP: {ready:?}"
        );
        poll.registry()
            .deregister(&mut frontend)
            .expect("deregister the accepted socket");

        let mut expect = ExpectProxyProtocol::new(
            TimeoutContainer::new(Duration::from_secs(10), Token(0)),
            frontend,
            Token(0),
            Ulid::generate(),
        );
        expect.update_readiness(Token(0), ready);
        (expect, client)
    }

    fn ready_once(expect: &mut ExpectProxyProtocol<TcpStream>) -> SessionResult {
        let session: Rc<RefCell<dyn crate::ProxySession>> = Rc::new(RefCell::new(UnusedSession));
        let proxy: Rc<RefCell<dyn crate::L7Proxy>> = Rc::new(RefCell::new(UnusedProxy));
        let mut metrics = SessionMetrics::new(None);
        expect.ready(session, proxy, &mut metrics)
    }

    /// The HTTP/HTTPS expect stage must parse a complete PROXY-v2 header
    /// that arrives in the same `READABLE | HUP` event as the client's FIN
    /// and upgrade, handing HUP on to the next stage, instead of closing on
    /// HUP before reading (sozu-proxy/sozu#1823).
    ///
    /// To SEE THIS RED: restore the unconditional
    /// `if self.frontend_readiness.event.is_hup() { return SessionResult::Close; }`
    /// before the readiness loop of `SessionState::ready`; the result is then
    /// `Close` and no address is parsed.
    #[cfg(target_os = "linux")]
    #[test]
    fn ready_parses_a_header_coalesced_with_the_client_half_close() {
        let mut wire = HeaderV2::new(Command::Proxy, header_src(), header_dst()).into_bytes();
        wire.extend_from_slice(b"GET / HTTP/1.1\r\n\r\n");
        let (mut expect, _client) = half_closed_expect(&wire);

        assert_eq!(ready_once(&mut expect), SessionResult::Upgrade);
        assert_eq!(
            expect.addresses.as_ref().and_then(ProxyAddr::source),
            Some(header_src()),
            "the coalesced header must be parsed before HUP is honoured"
        );
        assert!(
            expect.frontend_readiness.event.is_hup(),
            "HUP must survive the upgrade for the next stage to honour"
        );
    }

    /// Negative space: a zero-byte half-close (a bare TCP health check) and
    /// a truncated header followed by FIN must both still close once every
    /// readable byte is consumed.
    #[cfg(target_os = "linux")]
    #[test]
    fn ready_closes_a_half_close_without_a_complete_header() {
        let (mut zero_bytes, _client) = half_closed_expect(&[]);
        assert_eq!(ready_once(&mut zero_bytes), SessionResult::Close);

        let header = HeaderV2::new(Command::Proxy, header_src(), header_dst()).into_bytes();
        let (mut truncated, _client) = half_closed_expect(&header[..10]);
        assert_eq!(ready_once(&mut truncated), SessionResult::Close);
        assert_eq!(truncated.index, 10, "every readable byte must be consumed");
    }
}
