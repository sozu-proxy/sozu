use std::{
    cell::RefCell,
    io::{ErrorKind, Write},
    rc::Rc,
};

use mio::{net::TcpStream, Token};
use rusty_ulid::Ulid;

use crate::{
    pool::Checkout,
    protocol::{
        pipe::{Pipe, WebSocketContext},
        proxy_protocol::header::{Command, HeaderV2, ProxyProtocolHeader},
    },
    socket::SocketHandler,
    sozu_command::ready::Ready,
    tcp::TcpListener,
    BackendConnectionStatus, Protocol, Readiness, SessionMetrics, SessionResult,
};

pub struct SendProxyProtocol<Front: SocketHandler> {
    cursor_header: usize,
    pub backend_readiness: Readiness,
    pub backend_token: Option<Token>,
    pub backend: Option<TcpStream>,
    pub frontend_readiness: Readiness,
    pub frontend_token: Token,
    pub frontend: Front,
    pub header: Option<Vec<u8>>,
    pub request_id: Ulid,
}

impl<Front: SocketHandler> SendProxyProtocol<Front> {
    /// Instantiate a new SendProxyProtocol SessionState with:
    /// - frontend_interest: HUP | ERROR
    /// - frontend_event: EMPTY
    /// - backend_interest: HUP | ERROR
    /// - backend_event: EMPTY
    pub fn new(
        frontend: Front,
        frontend_token: Token,
        request_id: Ulid,
        backend: Option<TcpStream>,
    ) -> Self {
        SendProxyProtocol {
            header: None,
            frontend,
            request_id,
            backend,
            frontend_token,
            backend_token: None,
            frontend_readiness: Readiness {
                interest: Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            backend_readiness: Readiness {
                interest: Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            cursor_header: 0,
        }
    }

    // The header is send immediately at once upon the connection is establish
    // and prepended before any data.
    pub fn back_writable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        debug!("Trying to write proxy protocol header");

        // Generate the proxy protocol header if not already exist.
        if self.header.is_none() {
            if let Ok(local_addr) = self.front_socket().local_addr() {
                if let Ok(frontend_addr) = self.front_socket().peer_addr() {
                    self.header = Some(
                        ProxyProtocolHeader::V2(HeaderV2::new(
                            Command::Proxy,
                            frontend_addr,
                            local_addr,
                        ))
                        .into_bytes(),
                    );
                } else {
                    return SessionResult::Close;
                }
            };
        }

        if let Some(ref mut socket) = self.backend {
            if let Some(ref mut header) = self.header {
                loop {
                    match socket.write(&header[self.cursor_header..]) {
                        Ok(sz) => {
                            self.cursor_header += sz;
                            metrics.backend_bout += sz;

                            if self.cursor_header == header.len() {
                                debug!("Proxy protocol sent, upgrading");
                                return SessionResult::Upgrade;
                            }
                        }
                        Err(e) => match e.kind() {
                            ErrorKind::WouldBlock => {
                                self.backend_readiness.event.remove(Ready::WRITABLE);
                                return SessionResult::Continue;
                            }
                            e => {
                                incr!("proxy_protocol.errors");
                                debug!("send proxy protocol write error {:?}", e);
                                return SessionResult::Close;
                            }
                        },
                    }
                }
            }
        }

        error!("started Send proxy protocol with no header or backend socket");
        SessionResult::Close
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

    pub fn set_back_connected(&mut self, status: BackendConnectionStatus) {
        if status == BackendConnectionStatus::Connected {
            self.backend_readiness.interest.insert(Ready::WRITABLE);
        }
    }

    pub fn into_pipe(
        mut self,
        front_buf: Checkout,
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
            front_buf,
            self.frontend_token,
            self.frontend,
            listener,
            Protocol::TCP,
            self.request_id,
            addr,
            WebSocketContext::Tcp,
        );

        pipe.frontend_readiness = self.frontend_readiness;
        pipe.backend_readiness = self.backend_readiness;

        pipe.frontend_readiness.interest.insert(Ready::READABLE);
        pipe.backend_readiness.interest.insert(Ready::READABLE);

        if let Some(back_token) = self.backend_token {
            pipe.set_back_token(back_token);
        }

        pipe
    }
}

#[cfg(test)]
mod send_test {
    use std::{
        io::Read,
        net::{SocketAddr, TcpListener as StdTcpListener, TcpStream as StdTcpStream},
        os::unix::io::{FromRawFd, IntoRawFd},
        sync::{Arc, Barrier},
        thread::{self, JoinHandle},
    };

    use mio::net::{TcpListener, TcpStream};
    use rusty_ulid::Ulid;

    use super::{
        super::parser::parse_v2_header, BackendConnectionStatus, ErrorKind, SendProxyProtocol,
        SessionMetrics, SessionResult, Token,
    };

    #[test]
    fn it_should_send_a_proxy_protocol_header_to_the_upstream_backend() {
        setup_test_logger!();
        let addr_client: SocketAddr = "127.0.0.1:6666".parse().expect("parse address error");
        let addr_backend: SocketAddr = "127.0.0.1:2001".parse().expect("parse address error");
        let barrier = Arc::new(Barrier::new(3));
        let end_barrier = Arc::new(Barrier::new(2));

        start_client(addr_client, barrier.clone(), end_barrier.clone());
        let backend = start_backend(addr_backend, barrier.clone(), end_barrier);
        start_middleware(addr_client, addr_backend, barrier);

        backend
            .join()
            .expect("Couldn't join on the associated backend");
    }

    // Get connection from the session and connect to the backend
    // When connections are establish we send the proxy protocol header
    fn start_middleware(addr_client: SocketAddr, addr_backend: SocketAddr, barrier: Arc<Barrier>) {
        let listener = TcpListener::bind(addr_client).expect("could not accept session connection");

        let client_stream;
        barrier.wait();

        loop {
            if let Ok((stream, _addr)) = listener.accept() {
                client_stream = stream;
                break;
            }
        }

        // connect in blocking first, then convert to a mio socket
        let backend_stream =
            StdTcpStream::connect(addr_backend).expect("could not connect to the backend");
        let fd = backend_stream.into_raw_fd();
        let backend_stream = unsafe { TcpStream::from_raw_fd(fd) };

        let mut send_pp = SendProxyProtocol::new(
            client_stream,
            Token(0),
            Ulid::generate(),
            Some(backend_stream),
        );
        let mut session_metrics = SessionMetrics::new(None);

        send_pp.set_back_connected(BackendConnectionStatus::Connected);

        loop {
            let result = send_pp.back_writable(&mut session_metrics);
            if result == SessionResult::Upgrade {
                break;
            }

            if result != SessionResult::Continue {
                panic!("state machine error: result = {result:?}");
            }
        }
    }

    // Only connect to the middleware
    fn start_client(addr: SocketAddr, barrier: Arc<Barrier>, end_barrier: Arc<Barrier>) {
        thread::spawn(move || {
            barrier.wait();

            let _stream = StdTcpStream::connect(addr).unwrap();

            end_barrier.wait();
        });
    }

    // Get connection from the middleware read on the socket stream.
    // We check if we receive a valid proxy protocol header
    fn start_backend(
        addr: SocketAddr,
        barrier: Arc<Barrier>,
        end_barrier: Arc<Barrier>,
    ) -> JoinHandle<()> {
        let listener = StdTcpListener::bind(addr).expect("could not start backend");

        thread::spawn(move || {
            barrier.wait();

            let mut buf: [u8; 28] = [0; 28];
            let (mut conn, _) = listener
                .accept()
                .expect("could not accept connection from light middleware");
            println!("backend got a connection from the middleware");

            let mut index = 0usize;
            loop {
                if index >= 28 {
                    break;
                }

                match conn.read(&mut buf[index..]) {
                    Err(e) => match e.kind() {
                        ErrorKind::WouldBlock => continue,
                        e => {
                            end_barrier.wait();
                            panic!("read error: {e:?}");
                        }
                    },
                    Ok(sz) => {
                        println!("backend read {sz} bytes");
                        index += sz;
                    }
                }
            }

            match parse_v2_header(&buf) {
                Ok((_, _)) => println!("complete header received"),
                err => {
                    end_barrier.wait();
                    panic!("incorrect proxy protocol header received: {err:?}");
                }
            };

            end_barrier.wait();
        })
    }
}
