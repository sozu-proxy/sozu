use std::{cell::RefCell, io::Write, rc::Rc};

use mio::{net::TcpStream, Token};
use nom::{Err, Offset};
use rusty_ulid::Ulid;

use crate::{
    pool::Checkout,
    protocol::{
        pipe::{Pipe, WebSocketContext},
        proxy_protocol::parser::parse_v2_header,
    },
    socket::{SocketHandler, SocketResult},
    sozu_command::ready::Ready,
    tcp::TcpListener,
    Protocol, Readiness, SessionMetrics, SessionResult,
};

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
        }
    }

    pub fn readable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        let (sz, res) = self.frontend.socket_read(self.frontend_buffer.space());
        debug!(
            "FRONT proxy protocol [{:?}]: read {} bytes and res={:?}",
            self.frontend_token, sz, res
        );

        if sz > 0 {
            self.frontend_buffer.fill(sz);

            count!("bytes_in", sz as i64);
            metrics.bin += sz;

            if res == SocketResult::Error {
                error!(
                    "[{:?}] front socket error, closing the connection",
                    self.frontend_token
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
                Ok((rest, _)) => {
                    self.frontend_readiness.interest.remove(Ready::READABLE);
                    self.backend_readiness.interest.insert(Ready::WRITABLE);
                    self.frontend_buffer.data().offset(rest)
                }
                Err(Err::Incomplete(_)) => return SessionResult::Continue,
                Err(e) => {
                    error!("[{:?}] error parsing the proxy protocol header(error={:?}), closing the connection",
            self.frontend_token, e);
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
        debug!("Writing proxy protocol header");

        if let Some(ref mut socket) = self.backend {
            if let Some(ref header_size) = self.header_size {
                loop {
                    match socket.write(self.frontend_buffer.data()) {
                        Ok(sz) => {
                            self.cursor_header += sz;

                            metrics.backend_bout += sz;
                            self.frontend_buffer.consume(sz);

                            if self.cursor_header >= *header_size {
                                info!("Proxy protocol sent, upgrading");
                                return SessionResult::Upgrade;
                            }
                        }
                        Err(e) => {
                            incr!("proxy_protocol.errors");
                            self.frontend_readiness.reset();
                            self.backend_readiness.reset();
                            debug!("PROXY PROTOCOL {}", e);
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
        let addr = self.front_socket().peer_addr().ok();

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
