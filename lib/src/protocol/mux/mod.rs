use std::{
    cell::RefCell,
    collections::HashMap,
    io::Write,
    net::SocketAddr,
    rc::{Rc, Weak},
};

use mio::{net::TcpStream, Token};
use rusty_ulid::Ulid;
use sozu_command::ready::Ready;

mod h1;
mod h2;
mod parser;
mod pkawa;
mod serializer;

use crate::{
    https::HttpsListener,
    pool::{Checkout, Pool},
    protocol::{
        http::editor::HttpContext,
        mux::{h1::ConnectionH1, h2::ConnectionH2},
        SessionState,
    },
    socket::{FrontRustls, SocketHandler},
    AcceptError, L7Proxy, ProxySession, Readiness, SessionMetrics, SessionResult, StateResult,
};

use self::h2::{H2Settings, H2State};

/// Generic Http representation using the Kawa crate using the Checkout of Sozu as buffer
type GenericHttpStream = kawa::Kawa<Checkout>;
type StreamId = u32;
type GlobalStreamId = usize;

#[derive(Debug, Clone, Copy)]
pub enum Position {
    Client,
    Server,
}

pub enum MuxResult {
    Continue,
    CloseSession,
    Close(GlobalStreamId),
    Connect(GlobalStreamId),
}

pub enum Connection<Front: SocketHandler> {
    H1(ConnectionH1<Front>),
    H2(ConnectionH2<Front>),
}

impl<Front: SocketHandler> Connection<Front> {
    pub fn new_h1_server(front_stream: Front) -> Connection<Front> {
        Connection::H1(ConnectionH1 {
            socket: front_stream,
            position: Position::Server,
            readiness: Readiness {
                interest: Ready::READABLE | Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            stream: 0,
        })
    }
    pub fn new_h1_client(front_stream: Front) -> Connection<Front> {
        Connection::H1(ConnectionH1 {
            socket: front_stream,
            position: Position::Client,
            readiness: Readiness {
                interest: Ready::WRITABLE | Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            stream: 0,
        })
    }

    pub fn new_h2_server(front_stream: Front) -> Connection<Front> {
        Connection::H2(ConnectionH2 {
            socket: front_stream,
            position: Position::Server,
            readiness: Readiness {
                interest: Ready::READABLE | Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            streams: HashMap::from([(0, 0)]),
            state: H2State::ClientPreface,
            expect: Some((0, 24 + 9)),
            settings: H2Settings::default(),
        })
    }
    pub fn new_h2_client(front_stream: Front) -> Connection<Front> {
        Connection::H2(ConnectionH2 {
            socket: front_stream,
            position: Position::Client,
            readiness: Readiness {
                interest: Ready::WRITABLE | Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            streams: HashMap::from([(0, 0)]),
            state: H2State::ClientPreface,
            expect: None,
            settings: H2Settings::default(),
        })
    }

    pub fn readiness(&self) -> &Readiness {
        match self {
            Connection::H1(c) => &c.readiness,
            Connection::H2(c) => &c.readiness,
        }
    }
    pub fn readiness_mut(&mut self) -> &mut Readiness {
        match self {
            Connection::H1(c) => &mut c.readiness,
            Connection::H2(c) => &mut c.readiness,
        }
    }
    fn readable(&mut self, context: &mut Context) -> MuxResult {
        match self {
            Connection::H1(c) => c.readable(context),
            Connection::H2(c) => c.readable(context),
        }
    }
    fn writable(&mut self, context: &mut Context) -> MuxResult {
        match self {
            Connection::H1(c) => c.writable(context),
            Connection::H2(c) => c.writable(context),
        }
    }
}

pub struct Stream {
    // pub request_id: Ulid,
    pub window: i32,
    front: GenericHttpStream,
    back: GenericHttpStream,
    pub context: HttpContext,
}

impl Stream {
    pub fn front(&mut self, position: Position) -> &mut GenericHttpStream {
        match position {
            Position::Client => &mut self.back,
            Position::Server => &mut self.front,
        }
    }
    pub fn back(&mut self, position: Position) -> &mut GenericHttpStream {
        match position {
            Position::Client => &mut self.front,
            Position::Server => &mut self.back,
        }
    }
}

pub struct Streams {
    zero: Stream,
    others: Vec<Stream>,
}

impl Streams {
    pub fn get(&mut self, stream_id: GlobalStreamId) -> &mut Stream {
        if stream_id == 0 {
            &mut self.zero
        } else {
            &mut self.others[stream_id - 1]
        }
    }
}

pub struct Context {
    pub streams: Streams,
    pub pool: Weak<RefCell<Pool>>,
    pub decoder: hpack::Decoder<'static>,
}

impl Context {
    pub fn new_stream(
        pool: Weak<RefCell<Pool>>,
        request_id: Ulid,
        window: u32,
    ) -> Result<Stream, AcceptError> {
        let (front_buffer, back_buffer) = match pool.upgrade() {
            Some(pool) => {
                let mut pool = pool.borrow_mut();
                match (pool.checkout(), pool.checkout()) {
                    (Some(front_buffer), Some(back_buffer)) => (front_buffer, back_buffer),
                    _ => return Err(AcceptError::BufferCapacityReached),
                }
            }
            None => return Err(AcceptError::BufferCapacityReached),
        };
        Ok(Stream {
            window: window as i32,
            front: GenericHttpStream::new(kawa::Kind::Request, kawa::Buffer::new(front_buffer)),
            back: GenericHttpStream::new(kawa::Kind::Response, kawa::Buffer::new(back_buffer)),
            context: HttpContext {
                keep_alive_backend: false,
                keep_alive_frontend: false,
                sticky_session_found: None,
                method: None,
                authority: None,
                path: None,
                status: None,
                reason: None,
                user_agent: None,
                closing: false,
                id: request_id,
                protocol: crate::Protocol::HTTPS,
                public_address: "0.0.0.0:80".parse().unwrap(),
                session_address: None,
                sticky_name: "SOZUBALANCEID".to_owned(),
                sticky_session: None,
            },
        })
    }

    pub fn create_stream(
        &mut self,
        request_id: Ulid,
        window: u32,
    ) -> Result<GlobalStreamId, AcceptError> {
        self.streams
            .others
            .push(Self::new_stream(self.pool.clone(), request_id, window)?);
        Ok(self.streams.others.len())
    }

    pub fn new(
        pool: Weak<RefCell<Pool>>,
        request_id: Ulid,
        window: u32,
    ) -> Result<Context, AcceptError> {
        Ok(Self {
            streams: Streams {
                zero: Context::new_stream(pool.clone(), request_id, window)?,
                others: Vec::new(),
            },
            pool,
            decoder: hpack::Decoder::new(),
        })
    }
}

pub struct Mux {
    pub frontend_token: Token,
    pub frontend: Connection<FrontRustls>,
    pub backends: HashMap<Token, Connection<TcpStream>>,
    pub listener: Rc<RefCell<HttpsListener>>,
    pub public_address: SocketAddr,
    pub peer_address: Option<SocketAddr>,
    pub sticky_name: String,
    pub context: Context,
}

impl Mux {
    pub fn front_socket(&self) -> &TcpStream {
        match &self.frontend {
            Connection::H1(c) => &c.socket.stream,
            Connection::H2(c) => &c.socket.stream,
        }
    }
}

impl SessionState for Mux {
    fn ready(
        &mut self,
        _session: Rc<RefCell<dyn ProxySession>>,
        _proxy: Rc<RefCell<dyn L7Proxy>>,
        _metrics: &mut SessionMetrics,
    ) -> SessionResult {
        let mut counter = 0;
        let max_loop_iterations = 100000;

        if self.frontend.readiness().event.is_hup() {
            return SessionResult::Close;
        }

        let context = &mut self.context;
        while counter < max_loop_iterations {
            let mut dirty = false;

            if self.frontend.readiness().filter_interest().is_readable() {
                match self.frontend.readable(context) {
                    MuxResult::Continue => (),
                    MuxResult::CloseSession => return SessionResult::Close,
                    MuxResult::Close(_) => todo!(),
                    MuxResult::Connect(_) => todo!(),
                }
                dirty = true;
            }

            for (_, backend) in self.backends.iter_mut() {
                if backend.readiness().filter_interest().is_writable() {
                    match backend.writable(context) {
                        MuxResult::Continue => (),
                        MuxResult::CloseSession => return SessionResult::Close,
                        MuxResult::Close(_) => todo!(),
                        MuxResult::Connect(_) => unreachable!(),
                        }
                    dirty = true;
                }

                if backend.readiness().filter_interest().is_readable() {
                    match backend.readable(context) {
                        MuxResult::Continue => (),
                        MuxResult::CloseSession => return SessionResult::Close,
                        MuxResult::Close(_) => todo!(),
                        MuxResult::Connect(_) => unreachable!(),
                        }
                    dirty = true;
                }
            }

            if self.frontend.readiness().filter_interest().is_writable() {
                match self.frontend.writable(context) {
                    MuxResult::Continue => (),
                    MuxResult::CloseSession => return SessionResult::Close,
                    MuxResult::Close(_) => todo!(),
                    MuxResult::Connect(_) => unreachable!(),
                }
                dirty = true;
            }

            for backend in self.backends.values() {
                if backend.readiness().filter_interest().is_hup()
                    || backend.readiness().filter_interest().is_error()
                {
                    return SessionResult::Close;
                }
            }

            if !dirty {
                break;
            }

            counter += 1;
        }

        if counter == max_loop_iterations {
            incr!("http.infinite_loop.error");
            return SessionResult::Close;
        }

        SessionResult::Continue
    }

    fn update_readiness(&mut self, token: Token, events: sozu_command::ready::Ready) {
        if token == self.frontend_token {
            self.frontend.readiness_mut().event |= events;
        } else if let Some(c) = self.backends.get_mut(&token) {
            c.readiness_mut().event |= events;
        }
    }

    fn timeout(&mut self, token: Token, _metrics: &mut SessionMetrics) -> StateResult {
        println!("MuxState::timeout({token:?})");
        StateResult::CloseSession
    }

    fn cancel_timeouts(&mut self) {
        println!("MuxState::cancel_timeouts");
    }

    fn print_state(&self, context: &str) {
        error!(
            "\
{} Session(Mux)
\tFrontend:
\t\ttoken: {:?}\treadiness: {:?}",
            context,
            self.frontend_token,
            self.frontend.readiness()
        );
    }
    fn close(&mut self, _proxy: Rc<RefCell<dyn L7Proxy>>, _metrics: &mut SessionMetrics) {
        let s = match &mut self.frontend {
            Connection::H1(c) => &mut c.socket,
            Connection::H2(c) => &mut c.socket,
        };
        let mut b = [0; 1024];
        let (size, status) = s.socket_read(&mut b);
        println!("{size} {status:?} {:?}", &b[..size]);
        for stream in &mut self.context.streams.others {
            let kawa = &mut stream.front;
            kawa::debug_kawa(kawa);
            kawa.prepare(&mut kawa::h1::BlockConverter);
            let out = kawa.as_io_slice();
            let mut writer = std::io::BufWriter::new(Vec::new());
            let amount = writer.write_vectored(&out).unwrap();
            println!("amount: {amount}\n{}", unsafe {
                std::str::from_utf8_unchecked(writer.buffer())
            });
        }
    }
}
