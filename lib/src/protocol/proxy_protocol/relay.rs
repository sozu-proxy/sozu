//! PROXY-v2 relay state.
//!
//! Reads an inbound PROXY-v2 header (`parse_v2_header`) and forwards the
//! captured bytes verbatim onto a freshly opened backend `TcpStream` before
//! the rest of the byte stream begins. Used when Sōzu sits between two
//! PROXY-aware peers and must preserve the original client identity.

use std::{
    cell::RefCell,
    io::{ErrorKind, Write},
    rc::Rc,
};

use mio::{Token, net::TcpStream};
use nom::{Err, Offset};
use rusty_ulid::Ulid;
use sozu_command::logging::ansi_palette;

use crate::metrics::names;
use crate::{
    Protocol, Readiness, SessionMetrics, SessionResult,
    pool::Checkout,
    protocol::{
        pipe::{Pipe, WebSocketContext},
        proxy_protocol::{header::ProxyAddr, parser::parse_v2_header},
    },
    socket::{SocketHandler, SocketResult},
    sozu_command::ready::Ready,
    tcp::TcpListener,
};

/// Module-level prefix used on every log line emitted from this module when
/// no per-session state is in scope. Produces a bold bright-white
/// `PROXY-RELAY` label (uniform across every protocol) when the logger is in
/// colored mode.
#[allow(unused_macros)]
macro_rules! log_module_context {
    () => {{
        let (open, reset, _, _, _) = ansi_palette();
        format!("{open}PROXY-RELAY{reset}\t >>>", open = open, reset = reset)
    }};
}

/// Per-session prefix for log lines emitted with a [`RelayProxyProtocol`] in
/// scope. Renders the canonical
/// `\tPROXY-RELAY\tSession(...)\t >>>` envelope. The relay state has no
/// `request_id`-keyed [`LogContext`] (the caller-side ulid is not yet bound
/// to a request); the bracket carries the front/back tokens instead.
macro_rules! log_context {
    ($self:expr) => {{
        let (open, reset, grey, gray, white) = ansi_palette();
        format!(
            "{open}PROXY-RELAY{reset}\t{grey}Session{reset}({gray}frontend{reset}={white}{frontend}{reset}, {gray}backend{reset}={white}{backend}{reset}, {gray}front_readiness{reset}={white}{front_readiness}{reset}, {gray}back_readiness{reset}={white}{back_readiness}{reset})\t >>>",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            frontend = $self.frontend_token.0,
            backend = $self.backend_token.map(|t| t.0.to_string()).unwrap_or_else(|| "<none>".to_string()),
            front_readiness = $self.frontend_readiness,
            back_readiness = $self.backend_readiness,
        )
    }};
}

pub struct RelayProxyProtocol<Front: SocketHandler> {
    cursor_header: usize,
    pub backend_readiness: Readiness,
    pub backend_token: Option<Token>,
    pub backend: Option<TcpStream>,
    pub frontend_buffer: Checkout,
    pub frontend_readiness: Readiness,
    pub frontend_token: Token,
    pub frontend: Front,
    pub header_size: Option<usize>,
    pub request_id: Ulid,
    /// Parsed PROXY-v2 address pair captured from the inbound header.
    /// `None` until the parser succeeds, and `Some(ProxyAddr::AfUnspec)` for a
    /// header that declared AF_UNSPEC or carried `Command::Local` — whose
    /// address block `parse_v2_header` discards per the HAProxy PROXY protocol
    /// specification §2.2, whatever that block actually held on the wire. The
    /// pipe phase uses `ProxyAddr::source()` here to attribute the real client
    /// instead of the upstream PROXY-emitter's `peer_addr`.
    pub addresses: Option<ProxyAddr>,
}

impl<Front: SocketHandler> RelayProxyProtocol<Front> {
    /// Instantiate a new RelayProxyProtocol SessionState with:
    /// - frontend_interest: READABLE | HUP | ERROR
    /// - frontend_event: EMPTY
    pub fn new(
        frontend: Front,
        frontend_token: Token,
        request_id: Ulid,
        backend: Option<TcpStream>,
        front_buf: Checkout,
    ) -> Self {
        RelayProxyProtocol {
            backend_readiness: Readiness {
                interest: Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            backend_token: None,
            backend,
            cursor_header: 0,
            frontend_buffer: front_buf,
            frontend_readiness: Readiness {
                interest: Ready::READABLE | Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            frontend_token,
            frontend,
            header_size: None,
            request_id,
            addresses: None,
        }
    }

    pub fn readable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        let space_before = self.frontend_buffer.available_space();
        let (sz, res) = self.frontend.socket_read(self.frontend_buffer.space());
        debug!("{} read {} bytes and res={:?}", log_context!(self), sz, res);
        // The socket can only write into the free space it was handed.
        debug_assert!(
            sz <= space_before,
            "socket_read cannot return more bytes than the available space"
        );

        if sz > 0 {
            let data_before = self.frontend_buffer.available_data();
            self.frontend_buffer.fill(sz);
            // `fill` admits the freshly read bytes into the readable window:
            // available data grows by exactly `sz` (the read fit, asserted
            // above, so `fill` does not clamp).
            debug_assert_eq!(
                self.frontend_buffer.available_data(),
                data_before + sz,
                "fill must expose exactly the bytes just read"
            );

            count!(names::backend::BYTES_IN, sz as i64);
            metrics.bin += sz;

            if res == SocketResult::Error {
                error!(
                    "{} front socket error, closing the connection",
                    log_context!(self)
                );
                incr!(names::proxy_protocol::ERRORS);
                self.frontend_readiness.reset();
                self.backend_readiness.reset();
                return SessionResult::Close;
            }

            if res == SocketResult::WouldBlock {
                self.frontend_readiness.event.remove(Ready::READABLE);
            }

            let data_len = self.frontend_buffer.available_data();
            let read_sz = match parse_v2_header(self.frontend_buffer.data()) {
                Ok((rest, header)) => {
                    self.frontend_readiness.interest.remove(Ready::READABLE);
                    self.backend_readiness.interest.insert(Ready::WRITABLE);
                    // Capture the parsed addresses so the pipe phase can
                    // attribute traffic to the real client (see `into_pipe`).
                    // The header BYTES are a separate matter: they stay in
                    // `frontend_buffer` untouched, and `back_writable` relays
                    // that buffered prefix verbatim. Nothing here
                    // re-serializes `addresses`, so consuming the header at
                    // this point would destroy the only copy of it.
                    self.addresses = Some(header.addr);
                    let consumed = self.frontend_buffer.data().offset(rest);
                    // The header length is the prefix the parser consumed; it
                    // is a real (non-empty) prefix of the buffered data and can
                    // never exceed what was buffered.
                    debug_assert!(
                        consumed <= data_len,
                        "parsed header length cannot exceed the buffered bytes"
                    );
                    debug_assert!(consumed > 0, "a recognized v2 header is non-empty");
                    consumed
                }
                Err(Err::Incomplete(_)) => return SessionResult::Continue,
                Err(e) => {
                    error!(
                        "{} error parsing the proxy protocol header (error={:?}), closing the connection",
                        log_context!(self),
                        e
                    );
                    return SessionResult::Close;
                }
            };

            self.header_size = Some(read_sz);
            // Deliberately no `consume` here. The first `read_sz` buffered
            // bytes ARE the header `back_writable` has to relay, and anything
            // the client pipelined behind them belongs to the pipe phase:
            // `into_pipe` hands this very `Checkout` to `Pipe::new` and then
            // calls `restore_readiness_events`, which re-runs
            // `arm_inherited_buffer_writes` AFTER restoring this state's event
            // words -- that re-run, not the one inside `Pipe::new`, is what
            // actually leaves the backend WRITABLE readiness armed for a
            // non-empty buffer. The buffer is drained from the front, one
            // forwarded byte at a time, by `back_writable`.
            return SessionResult::Continue;
        }

        // Reached only when the read returned nothing. `tcp_socket_read`
        // answers `(0, SocketResult::Continue)` the moment the slice it is
        // handed is empty (`socket.rs`), which is what `space()` yields once
        // the buffer is full -- a client that declares a large `len` and then
        // stalls gets there. Leaving READABLE set in both readiness words
        // makes `tcp.rs::ready_inner` call this again at once with no space to
        // read into, burning `MAX_LOOP_ITERATIONS` before closing the session.
        // Drop the event and wait for a real edge, exactly as
        // `expect.rs::readable` does on a non-positive read.
        debug_assert_eq!(
            self.frontend_buffer.available_space(),
            space_before,
            "a zero-length read must leave the buffer untouched"
        );
        self.frontend_readiness.event.remove(Ready::READABLE);
        SessionResult::Continue
    }

    // The header is send immediately at once upon the connection is establish
    // and prepended before any data.
    pub fn back_writable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        debug!("{} writing proxy protocol header", log_context!(self));

        if let Some(ref mut socket) = self.backend
            && let Some(header_size) = self.header_size
        {
            // Termination: every iteration either returns, breaks, or advances
            // `cursor_header` by at least one byte towards `header_size`, so
            // the loop runs at most `header_size` times. `MAX_LOOP_ITERATIONS`
            // bounds the OUTER dispatch loop in `tcp.rs::ready_inner` and would
            // not bound this one, so the variant has to hold here.
            loop {
                // `readable` consumes nothing, and this loop only drains the
                // buffer from the front, so the unsent header tail is always
                // the first `header_size - cursor_header` buffered bytes.
                // Everything behind it is client payload pipelined into the
                // same read: it belongs to the pipe phase and is deliberately
                // NOT written here.
                debug_assert!(
                    self.cursor_header <= header_size,
                    "the forwarding cursor must stay within the parsed header"
                );
                let remaining = header_size.saturating_sub(self.cursor_header);
                let buffered = self.frontend_buffer.data();
                // Clamped rather than asserted outright. `remaining <=
                // buffered.len()` does hold for every state `readable` can
                // produce, but `a_zero_length_write_yields_instead_of_spinning`
                // deliberately violates it -- it drains the buffer behind this
                // function's back to prove the `Ok(0)` arm below still yields --
                // so a bare `debug_assert!` on it would fire in that test
                // instead of letting it exercise the guard. Assert the
                // invariant for the states that must satisfy it, and let the
                // clamp carry the degenerate one.
                debug_assert!(
                    buffered.is_empty() || remaining <= buffered.len(),
                    "a partially consumed header must still be buffered in full"
                );
                let offered = remaining.min(buffered.len());

                match socket.write(&buffered[..offered]) {
                    Ok(0) => {
                        // A zero-length write moves no byte: `cursor_header`
                        // would not advance, the `cursor_header >= header_size`
                        // exit would never be reached, and the only other exit
                        // is the `Err` arm below -- so looping here spins the
                        // worker at 100% CPU with its event loop starved, and
                        // every co-resident session with it. Answer it the way
                        // `send.rs::back_writable` answers `WouldBlock` and
                        // `expect.rs::readable` answers a zero-length read:
                        // drop the WRITABLE readiness and wait for the next
                        // epoll edge.
                        debug!(
                            "{} the backend accepted no byte, waiting for the next writable event",
                            log_context!(self)
                        );
                        self.backend_readiness.event.remove(Ready::WRITABLE);
                        break;
                    }
                    Ok(sz) => {
                        // A socket write reports at most the bytes it was
                        // offered, so the forwarded count never exceeds the
                        // unsent header tail.
                        debug_assert!(
                            sz <= offered,
                            "socket.write cannot send more than the bytes it was offered"
                        );
                        let cursor_before = self.cursor_header;
                        self.cursor_header += sz;
                        // The forwarding cursor is strictly monotonic and
                        // tracks exactly the bytes emitted this write.
                        debug_assert_eq!(
                            self.cursor_header,
                            cursor_before + sz,
                            "header cursor advances by exactly the bytes written"
                        );

                        count!(names::backend::BACK_BYTES_OUT, sz as i64);
                        metrics.backend_bout += sz;
                        self.frontend_buffer.consume(sz);

                        if self.cursor_header >= header_size {
                            info!("{} proxy protocol sent, upgrading", log_context!(self));
                            return SessionResult::Upgrade;
                        }
                    }
                    Err(e) => match e.kind() {
                        ErrorKind::WouldBlock => {
                            // The backend's send queue is full. A v2 header is
                            // `16 + len` with `len: u16`, so it can be far
                            // larger than that queue: this is ordinary
                            // backpressure, not a failure. Park the session the
                            // way `send.rs::back_writable` does -- clear the
                            // stale WRITABLE event, KEEP the interest -- so the
                            // next epoll edge resumes the half-written header.
                            // Resetting both readiness words, which this arm
                            // did for every error kind, means no later edge is
                            // ever acted on: the session then dies at the
                            // frontend timeout with a truncated PROXY header
                            // already delivered to the backend.
                            debug!(
                                "{} backend send queue full, waiting for the next writable event",
                                log_context!(self)
                            );
                            self.backend_readiness.event.remove(Ready::WRITABLE);
                            break;
                        }
                        ErrorKind::Interrupted => {
                            // A signal landed mid-write. Nothing about the
                            // socket changed, so edge-triggered epoll owes no
                            // new edge and dropping the readiness here would
                            // strand the session. Leave both words untouched
                            // and break: `tcp.rs::ready_inner` still sees
                            // WRITABLE in `interest & event` and re-enters
                            // immediately, so the retry is bounded by
                            // `MAX_LOOP_ITERATIONS` rather than looping
                            // unbounded in here. `udp.rs` retries the same way.
                            debug!("{} write interrupted, retrying", log_context!(self));
                            break;
                        }
                        _ => {
                            incr!(names::proxy_protocol::ERRORS);
                            self.frontend_readiness.reset();
                            self.backend_readiness.reset();
                            debug!("{} write error: {}", log_context!(self), e);
                            break;
                        }
                    },
                }
            }
        }
        SessionResult::Continue
    }

    pub fn front_socket(&self) -> &TcpStream {
        self.frontend.socket_ref()
    }

    pub fn front_socket_mut(&mut self) -> &mut TcpStream {
        self.frontend.socket_mut()
    }

    pub fn back_socket(&self) -> Option<&TcpStream> {
        self.backend.as_ref()
    }

    pub fn back_socket_mut(&mut self) -> Option<&mut TcpStream> {
        self.backend.as_mut()
    }

    pub fn set_back_socket(&mut self, socket: TcpStream) {
        self.backend = Some(socket);
    }

    pub fn back_token(&self) -> Option<Token> {
        self.backend_token
    }

    pub fn set_back_token(&mut self, token: Token) {
        self.backend_token = Some(token);
    }

    pub fn into_pipe(
        mut self,
        back_buf: Checkout,
        listener: Rc<RefCell<TcpListener>>,
    ) -> Pipe<Front, TcpListener> {
        let backend_socket = self.backend.take().unwrap();
        // Same rationale as `ExpectProxyProtocol::into_pipe`: prefer the
        // PROXY-v2 source over the TCP `peer_addr`. In Relay mode the
        // upstream emitter is also the TCP peer, so without this fix
        // the pipe phase records the LB / edge proxy instead of the
        // real client. Falls back whenever `self.addresses` is `AfUnspec` —
        // a header that declared AF_UNSPEC, or one that carried
        // `Command::Local`, whose address block the parser discards per the
        // HAProxy PROXY protocol specification §2.2 even when the wire block
        // was populated.
        let addr = self
            .addresses
            .as_ref()
            .and_then(|pa| pa.source())
            .or_else(|| self.front_socket().peer_addr().ok());

        let mut pipe = Pipe::new(
            back_buf,
            None,
            Some(backend_socket),
            None,
            None,
            None,
            None,
            self.frontend_buffer,
            self.frontend_token,
            self.frontend,
            listener,
            Protocol::TCP,
            self.request_id,
            self.request_id,
            addr,
            WebSocketContext::Tcp,
        );

        // Not a bare `pipe.*_readiness.event = ..` pair. `Pipe::new` has just
        // armed the backend WRITABLE readiness for the inherited
        // `frontend_buffer` (`arm_inherited_buffer_writes`), and assigning the
        // event words over it clobbers that arm. `restore_readiness_events`
        // sets both words and THEN re-runs the arm, so the payload this state
        // deliberately leaves buffered is flushed by construction instead of
        // by the accident that `back_writable` happens to be entered with
        // WRITABLE already set. `tcp.rs`'s `build_pipe_from_preread` and
        // `upgrade_send` say the same, for the same reason
        // (sozu-proxy/sozu#1279's close-before-flush truncation).
        pipe.restore_readiness_events(self.frontend_readiness.event, self.backend_readiness.event);

        if let Some(back_token) = self.backend_token {
            pipe.set_back_token(back_token);
        }

        pipe
    }
}

#[cfg(test)]
mod relay_test {
    use std::{
        io::{Read, Write},
        net::{
            IpAddr, Ipv4Addr, SocketAddr, TcpListener as StdTcpListener, TcpStream as StdTcpStream,
        },
        os::unix::io::FromRawFd,
        sync::{
            Arc, Barrier,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    };

    use mio::net::TcpListener;
    use rusty_ulid::Ulid;
    use socket2::SockRef;

    use super::*;
    use crate::{
        pool::Pool,
        protocol::proxy_protocol::header::{Command, HeaderV2},
    };

    /// Address pair the upfront middleware encapsulates. Under
    /// `Command::Local` these are the *forged* values a crafted peer would
    /// send: the wire format lets a `LOCAL` header carry a fully populated
    /// address block.
    fn header_src() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(125, 25, 10, 1)), 8080)
    }

    fn header_dst() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 4, 5, 8)), 4200)
    }

    /// Drives `RelayProxyProtocol::readable` against a real loopback
    /// connection carrying one PROXY-v2 header, and returns the source address
    /// `into_pipe` would attribute to the client, i.e.
    /// `self.addresses.as_ref().and_then(|pa| pa.source())` -- `None` meaning
    /// `into_pipe` falls back to the front socket's `peer_addr`.
    fn attributed_source_for(command: Command) -> Option<SocketAddr> {
        setup_test_logger!();
        let listener = TcpListener::bind("127.0.0.1:0".parse().expect("parse address error"))
            .expect("could not bind the relay listener");
        let relay_addr = listener
            .local_addr()
            .expect("the relay listener must expose its address");
        let barrier = Arc::new(Barrier::new(2));

        let upfront = start_upfront_middleware(relay_addr, barrier.clone(), command);

        barrier.wait();
        let session_stream = loop {
            if let Ok((stream, _addr)) = listener.accept() {
                break stream;
            }
        };

        let mut pool = Pool::with_capacity(1, 2, 16_384);
        let front_buf = pool.checkout().expect("the pool must hand out a buffer");
        let mut relay =
            RelayProxyProtocol::new(session_stream, Token(0), Ulid::generate(), None, front_buf);

        let mut session_metrics = SessionMetrics::new(None);
        // The front socket is non-blocking, so `readable` legitimately returns
        // without progress until the peer's bytes land. The bound is wall-clock
        // rather than an iteration count: a spin count turns a scheduling stall
        // under load into a spurious failure, whereas this only fires if the
        // header genuinely never arrives. It mirrors the 10s frontend timeout
        // the expect state uses.
        let deadline = Instant::now() + Duration::from_secs(10);
        while relay.header_size.is_none() {
            assert_eq!(
                relay.readable(&mut session_metrics),
                SessionResult::Continue,
                "the relay must keep reading until a complete header is parsed"
            );
            assert!(
                Instant::now() < deadline,
                "the relay never parsed a complete PROXY-v2 header within 10s"
            );
        }

        upfront.join().expect("should join");

        relay.addresses.as_ref().and_then(ProxyAddr::source)
    }

    #[test]
    fn a_proxy_command_header_attributes_the_encapsulated_source() {
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
    /// header written below is exactly what `HeaderV2::new(Command::Local, ..)`
    /// emits: ver/cmd `0x20`, family `0x11`, and a populated 12-byte `AF_INET`
    /// block.
    ///
    /// `into_pipe` prefers `self.addresses.source()` over the socket's
    /// `peer_addr`, so before the fix that forged pair became the client
    /// address in the access logs, in `X-Real-IP` and in the
    /// `max_connections_per_ip` counters. The header bytes themselves are
    /// relayed verbatim to the backend either way -- `back_writable` forwards
    /// the buffered prefix, it never re-serializes `addresses`. That claim was
    /// written here before anything tested it, and was in fact false until
    /// `readable` stopped consuming the header; it is now pinned by
    /// `back_writable_forwards_the_header_verbatim`.
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

    // Connect to the relay and send one proxy protocol header.
    fn start_upfront_middleware(
        relay_addr: SocketAddr,
        barrier: Arc<Barrier>,
        command: Command,
    ) -> JoinHandle<()> {
        thread::spawn(move || {
            let proxy_protocol = HeaderV2::new(command, header_src(), header_dst()).into_bytes();

            barrier.wait();
            match StdTcpStream::connect(relay_addr) {
                Ok(mut stream) => {
                    stream.write_all(&proxy_protocol).unwrap();
                }
                Err(e) => panic!("could not connect to the relay: {e}"),
            };
        })
    }

    /// Exactly what a PROXY-aware peer puts on the wire ahead of the payload:
    /// the 28-byte PROXY-v2 `PROXY` header over IPv4.
    fn wire_header() -> Vec<u8> {
        HeaderV2::new(Command::Proxy, header_src(), header_dst()).into_bytes()
    }

    /// How long each stage BEFORE `back_writable` -- the header-parse loop and
    /// the pipelined top-up -- may take. Deliberately well below the
    /// `recv_timeout` every caller passes (5s), so a stage that stalls reports
    /// ITSELF instead of letting the outer timeout expire and blame
    /// `back_writable`. A parse or read defect must never be able to describe
    /// itself as the remote-triggerable worker wedge; this PR's own history is
    /// one such misattribution and the harness must not be able to produce
    /// another.
    const STAGE_DEADLINE: Duration = Duration::from_secs(3);

    /// Everything one relay pass hands back across the worker-thread boundary.
    /// `Checkout` and the mio sockets stay inside the worker; only plain data
    /// crosses.
    #[derive(Debug)]
    struct RelayPass {
        /// What `back_writable` returned.
        result: SessionResult,
        /// The bytes the backend actually received.
        forwarded: Vec<u8>,
        /// What `frontend_buffer` still holds once `back_writable` returned,
        /// i.e. exactly what `into_pipe` hands `Pipe::new`.
        pending_for_pipe: Vec<u8>,
        /// Whether the backend WRITABLE *event* survived the pass.
        backend_writable_event: bool,
    }

    /// Drives one complete relay pass on a worker thread: a real loopback
    /// client writes `client_bytes` at a real frontend socket, `readable`
    /// parses the header, and `back_writable` forwards it onto a real
    /// connected backend.
    ///
    /// Returns `None` when the pass did not come back within `timeout` -- which
    /// is exactly what a spinning `back_writable` looks like from the outside.
    /// A wedged worker thread cannot be cancelled, so it keeps burning its core
    /// until the test binary exits. That is the point: in production that
    /// thread is the whole sozu worker, and every co-resident session starves
    /// with it.
    ///
    /// `starve_frontend_buffer` drains the buffer after the header is parsed,
    /// reproducing -- without depending on `readable` -- the state a
    /// header-consuming read path leaves behind.
    fn run_relay_pass(
        client_bytes: Vec<u8>,
        starve_frontend_buffer: bool,
        timeout: Duration,
    ) -> Option<RelayPass> {
        setup_test_logger!();
        let (sender, receiver) = mpsc::channel();

        thread::spawn(move || {
            let expected_buffered = client_bytes.len();

            // Frontend: a real accepted loopback connection already carrying
            // the client's bytes.
            let front_listener =
                TcpListener::bind("127.0.0.1:0".parse().expect("parse frontend address"))
                    .expect("could not bind the relay listener");
            let front_addr = front_listener
                .local_addr()
                .expect("the relay listener must expose its address");
            let mut client =
                StdTcpStream::connect(front_addr).expect("the client could not reach the relay");
            client
                .write_all(&client_bytes)
                .expect("the client could not write its bytes");
            let frontend = loop {
                if let Ok((stream, _addr)) = front_listener.accept() {
                    break stream;
                }
            };

            // Backend: a real connected TCP pair. The relay owns the writing
            // end; `backend_peer` is what a PROXY-aware backend sees.
            let backend_listener =
                StdTcpListener::bind("127.0.0.1:0").expect("could not bind the backend listener");
            let backend_addr = backend_listener
                .local_addr()
                .expect("the backend listener must expose its address");
            let relay_side =
                StdTcpStream::connect(backend_addr).expect("the relay could not reach the backend");
            let (mut backend_peer, _addr) = backend_listener
                .accept()
                .expect("the backend must accept the relay connection");
            relay_side
                .set_nonblocking(true)
                .expect("the relay's backend socket must be non-blocking");

            let mut pool = Pool::with_capacity(1, 2, 16_384);
            let front_buf = pool.checkout().expect("the pool must hand out a buffer");
            let mut relay = RelayProxyProtocol::new(
                frontend,
                Token(0),
                Ulid::generate(),
                Some(TcpStream::from_std(relay_side)),
                front_buf,
            );
            relay.set_back_token(Token(1));
            // What the event loop hands `back_writable`: a connected backend
            // whose WRITABLE edge has been observed.
            relay.backend_readiness.interest.insert(Ready::WRITABLE);
            relay.backend_readiness.event.insert(Ready::WRITABLE);

            let mut metrics = SessionMetrics::new(None);
            // The front socket is non-blocking, so `readable` legitimately
            // makes no progress until the peer's bytes land. Failures here are
            // SENT rather than asserted: a panic in this thread only drops the
            // channel, and the caller would then see the same `None` a wedged
            // `back_writable` produces and report the wedge.
            let deadline = Instant::now() + STAGE_DEADLINE;
            while relay.header_size.is_none() {
                let result = relay.readable(&mut metrics);
                if result != SessionResult::Continue {
                    let _ = sender.send(Err(format!(
                        "readable returned {result:?} before a complete header was parsed"
                    )));
                    return;
                }
                if Instant::now() >= deadline {
                    let _ = sender.send(Err(format!(
                        "the relay never parsed a complete PROXY-v2 header within {STAGE_DEADLINE:?}"
                    )));
                    return;
                }
            }
            // The client wrote header and payload in a single `write_all`
            // before the relay ever accepted, so loopback delivers both in one
            // read -- but TCP promises no such thing, and a split read would
            // silently downgrade the pipelined case into the header-only one.
            // Bounded top-up; a no-op on loopback.
            //
            // The gate is `metrics.bin` -- what `readable` has pulled off the
            // SOCKET -- and deliberately not `frontend_buffer.available_data()`,
            // which is what survives in the buffer. A mutation to the
            // consumption logic (the very thing these tests exist to catch)
            // drives `available_data()` to zero, so gating on it would stall
            // this loop for its full deadline and redden every test in here
            // WITHOUT EVER REACHING `back_writable` -- a green-looking red that
            // proves nothing. `metrics.bin` only ever counts bytes that arrived.
            while metrics.bin < expected_buffered && Instant::now() < deadline {
                let result = relay.readable(&mut metrics);
                if result != SessionResult::Continue {
                    let _ = sender.send(Err(format!(
                        "readable returned {result:?} while the client's bytes were still in flight"
                    )));
                    return;
                }
            }

            if starve_frontend_buffer {
                let buffered = relay.frontend_buffer.available_data();
                relay.frontend_buffer.consume(buffered);
            }

            let result = relay.back_writable(&mut metrics);

            // Read back whatever reached the backend. A short read timeout is
            // enough: `back_writable` has already returned, so every byte it
            // was ever going to emit is in flight.
            backend_peer
                .set_read_timeout(Some(Duration::from_millis(300)))
                .expect("the backend peer must accept a read timeout");
            let mut forwarded = Vec::new();
            let mut chunk = [0u8; 256];
            loop {
                match backend_peer.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(sz) => forwarded.extend_from_slice(&chunk[..sz]),
                    Err(_) => break,
                }
            }

            let _ = sender.send(Ok(RelayPass {
                result,
                forwarded,
                pending_for_pipe: relay.frontend_buffer.data().to_vec(),
                backend_writable_event: relay.backend_readiness.event.is_writable(),
            }));
        });

        match receiver.recv_timeout(timeout) {
            Ok(Ok(pass)) => Some(pass),
            // A stage BEFORE `back_writable` failed. Surface THAT, so a parse
            // or read defect names itself instead of arriving as a `None` the
            // caller can only read as the wedge.
            Ok(Err(stage)) => panic!("{stage}"),
            // Nothing came back in time: the pass is stuck inside
            // `back_writable`, which is the wedge the callers assert on.
            Err(_) => None,
        }
    }

    /// The wedge. `readable` used to `consume(sz)` everything it had just read
    /// -- the header included -- leaving `back_writable` looping on
    /// `socket.write(&[])`, which a connected non-blocking socket answers
    /// `Ok(0)` forever: `cursor_header += 0` never reaches `header_size`, the
    /// only other exit is the `Err` arm, and the worker spins at 100% CPU with
    /// its event loop starved. `MAX_LOOP_ITERATIONS` bounds the OUTER dispatch
    /// loop in `tcp.rs::ready_inner`, never this inner one, and a single
    /// unauthenticated TCP connection to a `RelayHeader` cluster reaches it.
    ///
    /// To SEE THIS RED: BOTH halves of the fix have to go, because each one
    /// alone now stops the wedge -- that redundancy is the point. In `readable`
    /// re-add `self.frontend_buffer.consume(sz);` immediately after
    /// `self.header_size = Some(read_sz);`, AND delete the `Ok(0)` arm from
    /// `back_writable`'s `match socket.write(..)`. That is the original defect
    /// exactly, and `back_writable` then never returns, failing this test on
    /// its 5s deadline. Re-adding the `consume` on its own leaves this test
    /// GREEN: the empty-buffer write lands in the `Ok(0)` arm, which yields --
    /// see `a_zero_length_write_yields_instead_of_spinning`.
    #[test]
    fn back_writable_returns_instead_of_spinning() {
        assert!(
            run_relay_pass(wire_header(), false, Duration::from_secs(5)).is_some(),
            "back_writable did not return within 5s: this is the remote-triggerable worker wedge"
        );
    }

    /// The correctness half. A `RelayHeader` cluster exists precisely so the
    /// backend receives the client's OWN header, byte for byte --
    /// `back_writable` forwards the buffered prefix, it never re-serializes
    /// `addresses`.
    ///
    /// Nothing covered this before: the other `relay_test` cases stop at
    /// `readable`, and e2e's `try_tcp_sni_relay_proxy_forwards_header_verbatim`
    /// drives the SNI-preread path, which reaches `Pipe` through
    /// `tcp.rs::build_pipe_from_preread` without ever building a
    /// `RelayProxyProtocol`.
    ///
    /// To SEE THIS RED: in `readable`, replace `self.header_size = Some(read_sz);`
    /// with `self.header_size = Some(read_sz - 4);`. The backend then receives a
    /// truncated 24-byte header and the comparison below fails.
    #[test]
    fn back_writable_forwards_the_header_verbatim() {
        let header = wire_header();
        let pass = run_relay_pass(header.clone(), false, Duration::from_secs(5))
            .expect("back_writable must return within 5s");

        assert_eq!(
            pass.result,
            SessionResult::Upgrade,
            "a fully forwarded header must upgrade the session to the pipe phase"
        );
        assert_eq!(
            pass.forwarded, header,
            "the backend must receive the client's PROXY-v2 header byte for byte"
        );
        assert!(
            pass.pending_for_pipe.is_empty(),
            "a header-only client leaves the pipe phase nothing to flush"
        );
    }

    /// A client is free to pipeline payload into the same read as its header.
    /// Those bytes belong to the PIPE phase, not to the relay: `into_pipe`
    /// hands `frontend_buffer` straight to `Pipe::new`, whose
    /// `arm_inherited_buffer_writes` arms the backend WRITABLE readiness
    /// exactly when that buffer is non-empty, and `Pipe::backend_writable`
    /// drains it (covered in `protocol/pipe.rs` by
    /// `backend_writable_drains_inherited_frontend_buffer_before_splice_engages`).
    /// So the relay forwards the header and stops there; dropping the tail
    /// would be silent data loss.
    ///
    /// To SEE THIS RED: in `back_writable`, widen the write back to the whole
    /// buffer -- `let offered = buffered.len();` instead of
    /// `remaining.min(buffered.len())`. The relay then forwards the payload
    /// too and hands the pipe an empty buffer, so the "forwards the header and
    /// nothing more" assertion below fails on the full 75-byte read.
    #[test]
    fn back_writable_leaves_pipelined_payload_for_the_pipe_phase() {
        let header = wire_header();
        let payload = b"GET / HTTP/1.1\r\nHost: pipelined.example.com\r\n\r\n".to_vec();
        let mut client_bytes = header.clone();
        client_bytes.extend_from_slice(&payload);

        let pass = run_relay_pass(client_bytes, false, Duration::from_secs(5))
            .expect("back_writable must return within 5s");

        assert_eq!(
            pass.result,
            SessionResult::Upgrade,
            "a fully forwarded header must upgrade the session to the pipe phase"
        );
        assert_eq!(
            pass.forwarded, header,
            "the relay forwards the header and nothing more"
        );
        assert_eq!(
            pass.pending_for_pipe, payload,
            "pipelined payload must survive in frontend_buffer for the pipe phase"
        );
    }

    /// The zero-write guard itself, independent of `readable`: whatever leaves
    /// the frontend buffer empty while a `header_size` is pending, a
    /// zero-length write must cost one syscall and then yield.
    /// `send.rs::back_writable` answers the same "the backend took nothing"
    /// condition by dropping the WRITABLE readiness and returning, and
    /// `expect.rs::readable` does the same on a zero-length read.
    ///
    /// To SEE THIS RED: delete the `Ok(0)` arm from `back_writable`'s
    /// `match socket.write(..)`, folding it back into `Ok(sz)`.
    /// `cursor_header += 0` then never reaches `header_size` and this test
    /// fails on its 5s deadline.
    #[test]
    fn a_zero_length_write_yields_instead_of_spinning() {
        let pass = run_relay_pass(wire_header(), true, Duration::from_secs(5))
            .expect("back_writable must return within 5s on a zero-length write");

        assert_eq!(
            pass.result,
            SessionResult::Continue,
            "a zero-length write makes no progress, so the session continues instead of upgrading"
        );
        assert!(
            pass.forwarded.is_empty(),
            "an empty buffer has nothing to forward"
        );
        assert!(
            !pass.backend_writable_event,
            "a zero-length write must drop the backend WRITABLE event so the next epoll edge drives the retry"
        );
    }

    /// A real connected loopback TCP pair, both ends non-blocking. When
    /// `socket_buffer` is `Some`, the kernel send/receive queues are shrunk so
    /// a bounded write can actually exhaust them.
    fn connected_pair(socket_buffer: Option<usize>) -> (StdTcpStream, StdTcpStream) {
        let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind the test listener");
        if let Some(size) = socket_buffer {
            // Set before the handshake so the accepted socket inherits it.
            SockRef::from(&listener)
                .set_recv_buffer_size(size)
                .expect("shrink the listener receive queue");
        }
        let address = listener
            .local_addr()
            .expect("the test listener must expose its address");
        let local = StdTcpStream::connect(address).expect("connect the test pair");
        let (peer, _addr) = listener.accept().expect("accept the test pair");
        if let Some(size) = socket_buffer {
            SockRef::from(&local)
                .set_send_buffer_size(size)
                .expect("shrink the local send queue");
            SockRef::from(&peer)
                .set_recv_buffer_size(size)
                .expect("shrink the peer receive queue");
        }
        local
            .set_nonblocking(true)
            .expect("the local end must be non-blocking");
        peer.set_nonblocking(true)
            .expect("the peer end must be non-blocking");
        (local, peer)
    }

    /// `back_writable`'s `Err` arm only went live with the forwarding fix
    /// above: before it, `readable` emptied the buffer, so the loop called
    /// `write(&[])` and got `Ok(0)` forever without ever reaching an error.
    /// Now that the relay really writes, a backpressured backend is reachable
    /// — a v2 header is `16 + len` with `len: u16`, so it can be far larger
    /// than a socket send queue (the 232-byte cap belongs to `expect.rs`'s
    /// fixed array, not here, and this state reads into a pool `Checkout`).
    /// That must park the session on the next epoll edge, not tear it down:
    /// clearing WRITABLE from `interest` as well would mean no later edge is
    /// ever acted on and the half-written PROXY header never completes, so the
    /// session dies at the frontend timeout with a truncated header already on
    /// the backend.
    ///
    /// To SEE THIS RED: delete the `ErrorKind::WouldBlock` arm from
    /// `back_writable`'s `Err(e) => match e.kind()`, leaving the catch-all that
    /// calls `frontend_readiness.reset()` / `backend_readiness.reset()`.
    #[test]
    fn back_writable_parks_the_session_on_a_backpressured_backend() {
        setup_test_logger!();
        let (frontend, _front_peer) = connected_pair(None);
        // 4 KiB queues each way, and the peer never reads, so the 256 KiB
        // header below cannot drain and the write must hit WouldBlock.
        let (relay_side, _backend_peer) = connected_pair(Some(4096));

        let header_size = 256 * 1024;
        let mut pool = Pool::with_capacity(1, 1, header_size);
        let mut front_buf = pool.checkout().expect("the pool must hand out a buffer");
        front_buf.space().fill(b'x');
        front_buf.fill(header_size);

        let mut relay = RelayProxyProtocol::new(
            TcpStream::from_std(frontend),
            Token(0),
            Ulid::generate(),
            Some(TcpStream::from_std(relay_side)),
            front_buf,
        );
        relay.header_size = Some(header_size);
        relay.backend_readiness.interest.insert(Ready::WRITABLE);
        relay.backend_readiness.event.insert(Ready::WRITABLE);

        let mut metrics = SessionMetrics::new(None);
        assert_eq!(
            relay.back_writable(&mut metrics),
            SessionResult::Continue,
            "a backpressured backend parks the session, it does not close it"
        );
        assert!(
            !relay.backend_readiness.event.is_writable(),
            "WouldBlock must clear the stale WRITABLE event so the loop yields"
        );
        assert!(
            relay.backend_readiness.interest.is_writable(),
            "WouldBlock must KEEP the WRITABLE interest, or no later epoll edge is acted on and the half-written header never completes"
        );
        assert!(
            relay.frontend_readiness.interest.is_hup(),
            "a backpressured backend must not tear down the frontend readiness"
        );
        assert!(
            metrics.backend_bout > 0 && metrics.backend_bout < header_size,
            "the write must have made partial progress before blocking: {} of {header_size} bytes",
            metrics.backend_bout
        );
    }

    /// The same missing-guard defect class one function up, in `readable`.
    /// `tcp_socket_read` returns `(0, SocketResult::Continue)` the moment the
    /// slice it is handed is empty (`lib/src/socket.rs`), which is exactly what
    /// `frontend_buffer.space()` yields once the buffer is full. A client that
    /// declares a large `len` and then stops fills the buffer, the streaming
    /// parser answers `Incomplete`, and `readable` returns `Continue` with
    /// READABLE still set in both readiness words — so `ready_inner` calls it
    /// again immediately, with no space to read into. `MAX_LOOP_ITERATIONS`
    /// bounds that OUTER loop, so this is CPU amplification and a spurious
    /// close rather than a wedge, but it is the same guard that was missing
    /// below. `ExpectProxyProtocol::readable`
    /// (`lib/src/protocol/proxy_protocol/expect.rs`) drops READABLE on any
    /// non-positive read; this now matches it.
    ///
    /// To SEE THIS RED: delete the `else` branch `readable` takes when
    /// `sz == 0`.
    #[test]
    fn readable_yields_on_a_zero_length_read() {
        setup_test_logger!();
        let (frontend, mut client) = connected_pair(None);

        // A well-formed v2 prefix that declares 65535 address bytes and then
        // stops: the parser can never complete it, and the filler fills the
        // session buffer exactly.
        let capacity = 64;
        let mut stalled = Vec::with_capacity(capacity);
        stalled.extend_from_slice(&[
            0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
        ]);
        stalled.push(0x21); // version 2, command PROXY
        stalled.push(0x11); // AF_INET over STREAM
        stalled.extend_from_slice(&u16::MAX.to_be_bytes());
        stalled.resize(capacity, b'x');
        client
            .write_all(&stalled)
            .expect("the client must be able to write its stalled header");

        let mut pool = Pool::with_capacity(1, 1, capacity);
        let front_buf = pool.checkout().expect("the pool must hand out a buffer");
        let mut relay = RelayProxyProtocol::new(
            TcpStream::from_std(frontend),
            Token(0),
            Ulid::generate(),
            None,
            front_buf,
        );
        // What the event loop hands `readable`.
        relay.frontend_readiness.event.insert(Ready::READABLE);

        let mut metrics = SessionMetrics::new(None);
        let deadline = Instant::now() + Duration::from_secs(10);
        while relay.frontend_buffer.available_space() > 0 && Instant::now() < deadline {
            assert_eq!(
                relay.readable(&mut metrics),
                SessionResult::Continue,
                "an incomplete header keeps the session reading"
            );
        }
        assert_eq!(
            relay.frontend_buffer.available_data(),
            capacity,
            "the stalled header must fill the whole session buffer"
        );
        assert!(
            relay.header_size.is_none(),
            "the declared 65535-byte address block never arrives, so no header is ever parsed"
        );

        // No space left, so this read is handed an empty slice and returns
        // zero bytes without touching the socket.
        assert_eq!(
            relay.readable(&mut metrics),
            SessionResult::Continue,
            "a zero-length read keeps the session alive"
        );
        assert!(
            !relay.frontend_readiness.event.is_readable(),
            "a zero-length read must clear the READABLE event; leaving it set makes ready_inner call readable again at once, up to MAX_LOOP_ITERATIONS"
        );
    }

    /// `back_writable`'s `Interrupted` arm, exercised for real.
    ///
    /// The arm is DEFENSIVE: it is unreachable in production, because the
    /// backend socket is built by `mio::net::TcpStream::connect` in
    /// `Backend::try_connect` (`lib/src/backends.rs`) and is therefore always
    /// non-blocking, and a non-blocking `send` answers EAGAIN, never EINTR.
    /// Reaching it at all needs a blocking socket, so this test builds one.
    /// The seam is that `mio::net::TcpStream::from_std` only wraps a
    /// descriptor and accepts any socket fd: a `socketpair(AF_UNIX,
    /// SOCK_STREAM)` -- deliberately not a pipe, whose fd answers `send(2)`
    /// with `ENOTSOCK` (os error 88) -- is filled until it refuses another
    /// byte, so the next blocking send PARKS with nothing written. That is
    /// what makes the outcome deterministic: with zero free space a partial
    /// `Ok(n)` is impossible and EINTR is the only way out. SIGUSR1 is
    /// installed WITHOUT `SA_RESTART`, so the parked send returns
    /// `Interrupted` instead of being resumed by the kernel.
    ///
    /// The contract under test: a signal is not a socket state change, so
    /// edge-triggered epoll owes no new edge and BOTH readiness words must
    /// survive untouched. `tcp.rs::ready_inner` then re-enters `back_writable`
    /// on its own `MAX_LOOP_ITERATIONS`-bounded loop, and that re-entry is the
    /// retry -- the shape `udp.rs` uses for EINTR, without an unbounded
    /// `continue` in here.
    ///
    /// To SEE THIS RED: add `self.backend_readiness.event.remove(Ready::WRITABLE);`
    /// to the `ErrorKind::Interrupted` arm, i.e. give it the `WouldBlock`
    /// treatment.
    #[test]
    fn back_writable_keeps_its_readiness_when_a_signal_interrupts_the_write() {
        setup_test_logger!();
        extern "C" fn noop_handler(_signal: libc::c_int) {}

        let (frontend, _front_peer) = connected_pair(None);

        let mut fds = [0 as libc::c_int; 2];
        assert_eq!(
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) },
            0,
            "the socketpair must be created"
        );
        let (write_fd, read_fd) = (fds[0], fds[1]);
        for (fd, option) in [(write_fd, libc::SO_SNDBUF), (read_fd, libc::SO_RCVBUF)] {
            let size: libc::c_int = 4096;
            assert_eq!(
                unsafe {
                    libc::setsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        option,
                        std::ptr::addr_of!(size).cast(),
                        std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                    )
                },
                0,
                "the socketpair buffers must be shrinkable"
            );
        }

        // Fill it until it refuses another byte. `MSG_DONTWAIT` keeps the
        // descriptor itself blocking, which is what the write under test needs.
        let chunk = [b'f'; 4096];
        let mut filled = 0usize;
        loop {
            let sent = unsafe {
                libc::send(
                    write_fd,
                    chunk.as_ptr().cast(),
                    chunk.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if sent < 0 {
                break;
            }
            filled += sent as usize;
            assert!(filled < 8 << 20, "the socketpair never stopped accepting");
        }
        assert!(filled > 0, "the socketpair must have accepted some bytes");

        let header_size = 64;
        let mut pool = Pool::with_capacity(1, 1, header_size);
        let mut front_buf = pool.checkout().expect("the pool must hand out a buffer");
        front_buf.space().fill(b'h');
        front_buf.fill(header_size);

        // SAFETY: `write_fd` is a live socket descriptor this test owns and
        // never touches again. Ownership passes to the stream, whose `Drop`
        // closes it. `from_std` only wraps the descriptor, so the socket stays
        // blocking -- exactly what makes EINTR reachable here.
        let backend = unsafe { StdTcpStream::from_raw_fd(write_fd) };
        let mut relay = RelayProxyProtocol::new(
            TcpStream::from_std(frontend),
            Token(0),
            Ulid::generate(),
            Some(TcpStream::from_std(backend)),
            front_buf,
        );
        relay.header_size = Some(header_size);
        relay.backend_readiness.interest.insert(Ready::WRITABLE);
        relay.backend_readiness.event.insert(Ready::WRITABLE);

        let mut handler: libc::sigaction = unsafe { std::mem::zeroed() };
        handler.sa_sigaction = noop_handler as *const () as usize;
        handler.sa_flags = 0; // deliberately NOT SA_RESTART
        unsafe { libc::sigemptyset(&mut handler.sa_mask) };
        let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::sigaction(libc::SIGUSR1, &handler, &mut previous) },
            0,
            "the SIGUSR1 handler must install"
        );

        // If the signal is somehow missed, drain the peer so the parked send
        // completes: the test then FAILS on its assertions instead of hanging
        // the suite.
        let finished = Arc::new(AtomicBool::new(false));
        let watchdog = {
            let finished = Arc::clone(&finished);
            thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                while !finished.load(Ordering::SeqCst) {
                    if Instant::now() >= deadline {
                        let mut sink = [0u8; 4096];
                        loop {
                            let got = unsafe {
                                libc::recv(
                                    read_fd,
                                    sink.as_mut_ptr().cast(),
                                    sink.len(),
                                    libc::MSG_DONTWAIT,
                                )
                            };
                            if got <= 0 {
                                break;
                            }
                        }
                        return;
                    }
                    thread::sleep(Duration::from_millis(25));
                }
            })
        };

        let target = unsafe { libc::pthread_self() };
        let signaller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(250));
            unsafe { libc::pthread_kill(target, libc::SIGUSR1) };
        });

        let mut metrics = SessionMetrics::new(None);
        let result = relay.back_writable(&mut metrics);

        finished.store(true, Ordering::SeqCst);
        signaller
            .join()
            .expect("the signalling thread must not panic");
        watchdog.join().expect("the watchdog thread must not panic");
        // Restore only AFTER joining the signaller: restoring first lets a late
        // SIGUSR1 reach the default disposition, which kills the test process
        // with signal 10.
        unsafe { libc::sigaction(libc::SIGUSR1, &previous, std::ptr::null_mut()) };
        unsafe { libc::close(read_fd) };

        assert_eq!(
            result,
            SessionResult::Continue,
            "an interrupted write parks the session, it does not close it"
        );
        assert_eq!(
            metrics.backend_bout, 0,
            "the send parked on a full socket, so no byte can have been written"
        );
        assert!(
            relay.backend_readiness.event.is_writable(),
            "Interrupted must LEAVE the WRITABLE event set: the socket did not change state, so edge-triggered epoll owes no new edge and clearing it strands the session"
        );
        assert!(
            relay.backend_readiness.interest.is_writable(),
            "Interrupted must leave the WRITABLE interest set so ready_inner re-enters back_writable"
        );
    }
}
