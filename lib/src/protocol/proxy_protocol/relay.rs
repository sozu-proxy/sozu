//! PROXY-v2 relay state.
//!
//! Reads an inbound PROXY-v2 header (`parse_v2_header`) and forwards the
//! captured bytes verbatim onto a freshly opened backend `TcpStream` before
//! the rest of the byte stream begins. Used when Sōzu sits between two
//! PROXY-aware peers and must preserve the original client identity.

use std::{cell::RefCell, io::Write, rc::Rc};

use mio::{Token, net::TcpStream};
use nom::{Err, Offset};
use rusty_ulid::Ulid;
use sozu_command::logging::ansi_palette;

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
    /// `None` until the parser succeeds, or for headers that carry
    /// `Command::Local` (no encapsulated addresses). The pipe phase
    /// uses `ProxyAddr::source()` here to attribute the real client
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
        let (sz, res) = self.frontend.socket_read(self.frontend_buffer.space());
        debug!("{} read {} bytes and res={:?}", log_context!(self), sz, res);

        if sz > 0 {
            self.frontend_buffer.fill(sz);

            count!("bytes_in", sz as i64);
            metrics.bin += sz;

            if res == SocketResult::Error {
                error!(
                    "{} front socket error, closing the connection",
                    log_context!(self)
                );
                incr!("proxy_protocol.errors");
                self.frontend_readiness.reset();
                self.backend_readiness.reset();
                return SessionResult::Close;
            }

            if res == SocketResult::WouldBlock {
                self.frontend_readiness.event.remove(Ready::READABLE);
            }

            let read_sz = match parse_v2_header(self.frontend_buffer.data()) {
                Ok((rest, header)) => {
                    self.frontend_readiness.interest.remove(Ready::READABLE);
                    self.backend_readiness.interest.insert(Ready::WRITABLE);
                    // Capture the parsed addresses so the pipe phase can
                    // attribute traffic to the real client (see
                    // `into_pipe`). The header bytes themselves are still
                    // forwarded verbatim onto the backend by
                    // `back_writable`.
                    self.addresses = Some(header.addr);
                    self.frontend_buffer.data().offset(rest)
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
            self.frontend_buffer.consume(sz);
            return SessionResult::Continue;
        }

        SessionResult::Continue
    }

    // The header is send immediately at once upon the connection is establish
    // and prepended before any data.
    pub fn back_writable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        debug!("{} writing proxy protocol header", log_context!(self));

        if let Some(ref mut socket) = self.backend {
            if let Some(ref header_size) = self.header_size {
                loop {
                    match socket.write(self.frontend_buffer.data()) {
                        Ok(sz) => {
                            self.cursor_header += sz;

                            count!("back_bytes_out", sz as i64);
                            metrics.backend_bout += sz;
                            self.frontend_buffer.consume(sz);

                            if self.cursor_header >= *header_size {
                                info!("{} proxy protocol sent, upgrading", log_context!(self));
                                return SessionResult::Upgrade;
                            }
                        }
                        Err(e) => {
                            incr!("proxy_protocol.errors");
                            self.frontend_readiness.reset();
                            self.backend_readiness.reset();
                            debug!("{} write error: {}", log_context!(self), e);
                            break;
                        }
                    }
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
        // real client. Falls back when the header was `Command::Local`
        // (no addresses) or when the parser ran with `AddressFamily::Unspec`.
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

        pipe.frontend_readiness.event = self.frontend_readiness.event;
        pipe.backend_readiness.event = self.backend_readiness.event;

        if let Some(back_token) = self.backend_token {
            pipe.set_back_token(back_token);
        }

        pipe
    }
}
