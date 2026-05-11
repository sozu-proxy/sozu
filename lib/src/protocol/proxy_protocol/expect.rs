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
    frontend_buffer: [u8; 232],
    pub frontend_readiness: Readiness,
    pub frontend_token: Token,
    pub frontend: Front,
    header_len: HeaderLen,
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
            frontend_buffer: [0; 232],
            frontend_readiness: Readiness {
                interest: Ready::READABLE | Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            frontend_token,
            frontend,
            header_len: HeaderLen::V4,
            index: 0,
            request_id,
        }
    }

    pub fn readable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        let total_len = match self.header_len {
            HeaderLen::V4 => 28,
            HeaderLen::V6 => 52,
            HeaderLen::Unix => 232,
        };

        let (sz, socket_result) = self
            .frontend
            .socket_read(&mut self.frontend_buffer[self.index..total_len]);
        trace!(
            "{} read {} bytes and res={:?}, total_len = {}",
            log_context!(self),
            sz,
            socket_result,
            total_len
        );

        if sz > 0 {
            self.index += sz;

            count!(names::backend::BYTES_IN, sz as i64);
            metrics.bin += sz;

            if self.index == self.frontend_buffer.len() {
                self.frontend_readiness.interest.remove(Ready::READABLE);
            }
        } else {
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
                trace!(
                    "{} got expect header: {:?}, rest.len() = {}",
                    log_context!(self),
                    header,
                    rest.len()
                );
                self.addresses = Some(header.addr);
                SessionResult::Upgrade
            }
            Err(Err::Incomplete(_)) => {
                match self.header_len {
                    HeaderLen::V4 => {
                        if self.index == 28 {
                            self.header_len = HeaderLen::V6;
                        }
                    }
                    HeaderLen::V6 => {
                        if self.index == 52 {
                            self.header_len = HeaderLen::Unix;
                        }
                    }
                    HeaderLen::Unix => {
                        if self.index == 232 {
                            error!(
                                "{} proxy protocol header exceeds maximum size (232 bytes), closing",
                                log_context!(self)
                            );
                            incr!(names::proxy_protocol::ERRORS);
                            self.frontend_readiness.reset();
                            return SessionResult::Close;
                        }
                    }
                };
                SessionResult::Continue
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
        // Falls back to `peer_addr` when the header carried `Command::Local`
        // (no encapsulated addresses) or when the parser ran with
        // `AddressFamily::Unspec`.
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

        if self.frontend_readiness.event.is_hup() {
            return SessionResult::Close;
        }

        while counter < MAX_LOOP_ITERATIONS {
            let frontend_interest = self.frontend_readiness.filter_interest();

            trace!(
                "{} {:?} -> None",
                log_context!(self),
                self.frontend_readiness
            );

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

            counter += 1;
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

    // Flow diagram of the test below
    //                [connect]   [send proxy protocol]
    //upfront proxy  ----------------------X
    //              /     |           |
    //  sozu     ---------v-----------v----X
    #[test]
    fn middleware_should_receive_proxy_protocol_header_from_an_upfront_middleware() {
        setup_test_logger!();
        let middleware_addr: SocketAddr = "127.0.0.1:3500".parse().expect("parse address error");
        let barrier = Arc::new(Barrier::new(2));

        let upfront = start_upfront_middleware(middleware_addr, barrier.clone());
        start_middleware(middleware_addr, barrier);

        upfront.join().expect("should join");
    }

    // Accept connection from an upfront proxy and expect to read a proxy protocol header in this stream.
    fn start_middleware(middleware_addr: SocketAddr, barrier: Arc<Barrier>) {
        let upfront_middleware_conn_listener = TcpListener::bind(middleware_addr)
            .expect("could not accept upfront middleware connection");
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
    }

    // Connect to the next middleware and send a proxy protocol header
    fn start_upfront_middleware(
        next_middleware_addr: SocketAddr,
        barrier: Arc<Barrier>,
    ) -> JoinHandle<()> {
        thread::spawn(move || {
            let src_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(125, 25, 10, 1)), 8080);
            let dst_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 4, 5, 8)), 4200);
            let proxy_protocol = HeaderV2::new(Command::Local, src_addr, dst_addr).into_bytes();

            barrier.wait();
            match StdTcpStream::connect(next_middleware_addr) {
                Ok(mut stream) => {
                    stream.write_all(&proxy_protocol).unwrap();
                }
                Err(e) => panic!("could not connect to the next middleware: {e}"),
            };
        })
    }
}
