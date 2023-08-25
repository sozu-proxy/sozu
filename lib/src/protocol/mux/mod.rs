use std::{
    cell::RefCell,
    collections::HashMap,
    io::Write,
    net::SocketAddr,
    rc::{Rc, Weak},
};

use mio::{net::TcpStream, Interest, Token};
use rusty_ulid::Ulid;
use sozu_command::ready::Ready;

mod h1;
mod h2;
mod parser;
mod pkawa;
mod serializer;

use crate::{
    backends::{Backend, BackendError},
    https::HttpsListener,
    pool::{Checkout, Pool},
    protocol::{
        http::editor::HttpContext,
        mux::{h1::ConnectionH1, h2::ConnectionH2},
        SessionState,
    },
    router::Route,
    socket::{FrontRustls, SocketHandler},
    BackendConnectionError, L7ListenerHandler, L7Proxy, ProxySession, Readiness,
    RetrieveClusterError, SessionMetrics, SessionResult, StateResult,
};

use self::h2::{H2Settings, H2State, H2StreamId};

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

pub trait UpdateReadiness {
    fn readiness(&self, token: Token) -> &Readiness;
    fn readiness_mut(&mut self, token: Token) -> &mut Readiness;
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
    pub fn new_h1_client(front_stream: Front, stream_id: GlobalStreamId) -> Connection<Front> {
        Connection::H1(ConnectionH1 {
            socket: front_stream,
            position: Position::Client,
            readiness: Readiness {
                interest: Ready::WRITABLE | Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            stream: stream_id,
        })
    }

    pub fn new_h2_server(
        front_stream: Front,
        pool: Weak<RefCell<Pool>>,
    ) -> Option<Connection<Front>> {
        let buffer = pool
            .upgrade()
            .and_then(|pool| pool.borrow_mut().checkout())?;
        Some(Connection::H2(ConnectionH2 {
            socket: front_stream,
            position: Position::Server,
            readiness: Readiness {
                interest: Ready::READABLE | Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            streams: HashMap::new(),
            state: H2State::ClientPreface,
            expect: Some((H2StreamId::Zero, 24 + 9)),
            settings: H2Settings::default(),
            zero: kawa::Kawa::new(kawa::Kind::Request, kawa::Buffer::new(buffer)),
            window: 1 << 16,
        }))
    }
    pub fn new_h2_client(
        front_stream: Front,
        pool: Weak<RefCell<Pool>>,
    ) -> Option<Connection<Front>> {
        let buffer = pool
            .upgrade()
            .and_then(|pool| pool.borrow_mut().checkout())?;
        Some(Connection::H2(ConnectionH2 {
            socket: front_stream,
            position: Position::Client,
            readiness: Readiness {
                interest: Ready::WRITABLE | Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            streams: HashMap::new(),
            state: H2State::ClientPreface,
            expect: None,
            settings: H2Settings::default(),
            zero: kawa::Kawa::new(kawa::Kind::Request, kawa::Buffer::new(buffer)),
            window: 1 << 16,
        }))
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
    pub fn socket(&self) -> &TcpStream {
        match self {
            Connection::H1(c) => c.socket.socket_ref(),
            Connection::H2(c) => c.socket.socket_ref(),
        }
    }
    fn readable<E>(&mut self, context: &mut Context, endpoint: E) -> MuxResult
    where
        E: UpdateReadiness,
    {
        match self {
            Connection::H1(c) => c.readable(context, endpoint),
            Connection::H2(c) => c.readable(context, endpoint),
        }
    }
    fn writable<E>(&mut self, context: &mut Context, endpoint: E) -> MuxResult
    where
        E: UpdateReadiness,
    {
        match self {
            Connection::H1(c) => c.writable(context, endpoint),
            Connection::H2(c) => c.writable(context, endpoint),
        }
    }
}

struct EndpointServer<'a>(&'a mut Connection<FrontRustls>);
struct EndpointClient<'a>(&'a mut Router);

// note: EndpointServer are used by client Connection, they do not know the frontend Token
// they will use the Stream's Token which is their backend token
impl<'a> UpdateReadiness for EndpointServer<'a> {
    fn readiness(&self, _token: Token) -> &Readiness {
        self.0.readiness()
    }
    fn readiness_mut(&mut self, _token: Token) -> &mut Readiness {
        self.0.readiness_mut()
    }
}
impl<'a> UpdateReadiness for EndpointClient<'a> {
    fn readiness(&self, token: Token) -> &Readiness {
        self.0.backends.get(&token).unwrap().readiness()
    }
    fn readiness_mut(&mut self, token: Token) -> &mut Readiness {
        self.0.backends.get_mut(&token).unwrap().readiness_mut()
    }
}

// enum Stream {
//     Idle {
//         window: i32,
//     },
//     Open {
//         window: i32,
//         token: Token,
//         front: GenericHttpStream,
//         back: GenericHttpStream,
//         context: HttpContext,
//     },
//     Reserved {
//         window: i32,
//         token: Token,
//         position: Position,
//         buffer: GenericHttpStream,
//         context: HttpContext,
//     },
//     HalfClosed {
//         window: i32,
//         token: Token,
//         position: Position,
//         buffer: GenericHttpStream,
//         context: HttpContext,
//     },
//     Closed,
// }

pub struct Stream {
    // pub request_id: Ulid,
    pub window: i32,
    pub token: Option<Token>,
    front: GenericHttpStream,
    back: GenericHttpStream,
    pub context: HttpContext,
}

/// This struct allows to mutably borrow the read and write buffers (dependant on the position)
/// as well as the context of a Stream at the same time
pub struct StreamParts<'a> {
    pub rbuffer: &'a mut GenericHttpStream,
    pub wbuffer: &'a mut GenericHttpStream,
    pub context: &'a mut HttpContext,
}

impl Stream {
    pub fn new(pool: Weak<RefCell<Pool>>, request_id: Ulid, window: u32) -> Option<Self> {
        let (front_buffer, back_buffer) = match pool.upgrade() {
            Some(pool) => {
                let mut pool = pool.borrow_mut();
                match (pool.checkout(), pool.checkout()) {
                    (Some(front_buffer), Some(back_buffer)) => (front_buffer, back_buffer),
                    _ => return None,
                }
            }
            None => return None,
        };
        Some(Self {
            window: window as i32,
            token: None,
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
    pub fn split(&mut self, position: Position) -> StreamParts<'_> {
        match position {
            Position::Client => StreamParts {
                rbuffer: &mut self.back,
                wbuffer: &mut self.front,
                context: &mut self.context,
            },
            Position::Server => StreamParts {
                rbuffer: &mut self.front,
                wbuffer: &mut self.back,
                context: &mut self.context,
            },
        }
    }
    pub fn rbuffer(&mut self, position: Position) -> &mut GenericHttpStream {
        match position {
            Position::Client => &mut self.back,
            Position::Server => &mut self.front,
        }
    }
    pub fn wbuffer(&mut self, position: Position) -> &mut GenericHttpStream {
        match position {
            Position::Client => &mut self.front,
            Position::Server => &mut self.back,
        }
    }
}

pub struct Context {
    pub streams: Vec<Stream>,
    pub pool: Weak<RefCell<Pool>>,
    pub decoder: hpack::Decoder<'static>,
}

impl Context {
    pub fn create_stream(&mut self, request_id: Ulid, window: u32) -> Option<GlobalStreamId> {
        self.streams
            .push(Stream::new(self.pool.clone(), request_id, window)?);
        Some(self.streams.len() - 1)
    }

    pub fn new(pool: Weak<RefCell<Pool>>) -> Context {
        Self {
            streams: Vec::new(),
            pool,
            decoder: hpack::Decoder::new(),
        }
    }
}

pub struct Router {
    pub listener: Rc<RefCell<HttpsListener>>,
    pub backends: HashMap<Token, Connection<TcpStream>>,
}

impl Router {
    fn connect(
        &mut self,
        stream_id: GlobalStreamId,
        context: &mut Context,
        session: Rc<RefCell<dyn ProxySession>>,
        proxy: Rc<RefCell<dyn L7Proxy>>,
        metrics: &mut SessionMetrics,
    ) -> Result<(), BackendConnectionError> {
        let stream = &mut context.streams[stream_id];
        let context = &mut stream.context;
        // we should get if the route is H2 or not here
        // for now we assume it's H1
        let cluster_id = self
            .cluster_id_from_request(context, proxy.clone())
            .map_err(BackendConnectionError::RetrieveClusterError)?;
        println!("{cluster_id}!!!!!");

        let frontend_should_stick = proxy
            .borrow()
            .clusters()
            .get(&cluster_id)
            .map(|cluster| cluster.sticky_session)
            .unwrap_or(false);

        let mut socket = self.backend_from_request(
            &cluster_id,
            frontend_should_stick,
            context,
            proxy.clone(),
            metrics,
        )?;

        if let Err(e) = socket.set_nodelay(true) {
            error!(
                "error setting nodelay on back socket({:?}): {:?}",
                socket, e
            );
        }
        // self.backend_readiness.interest = Ready::WRITABLE | Ready::HUP | Ready::ERROR;
        // self.backend_connection_status = BackendConnectionStatus::Connecting(Instant::now());

        let backend_token = proxy.borrow().add_session(session);

        if let Err(e) = proxy.borrow().register_socket(
            &mut socket,
            backend_token,
            Interest::READABLE | Interest::WRITABLE,
        ) {
            error!("error registering back socket({:?}): {:?}", socket, e);
        }

        stream.token.insert(backend_token);
        self.backends
            .insert(backend_token, Connection::new_h1_client(socket, stream_id));
        Ok(())
    }

    fn cluster_id_from_request(
        &mut self,
        context: &mut HttpContext,
        _proxy: Rc<RefCell<dyn L7Proxy>>,
    ) -> Result<String, RetrieveClusterError> {
        let (host, uri, method) = match context.extract_route() {
            Ok(tuple) => tuple,
            Err(cluster_error) => {
                panic!("{}", cluster_error);
                // self.set_answer(DefaultAnswerStatus::Answer400, None);
                // return Err(cluster_error);
            }
        };

        let route_result = self
            .listener
            .borrow()
            .frontend_from_request(host, uri, method);

        let route = match route_result {
            Ok(route) => route,
            Err(frontend_error) => {
                panic!("{}", frontend_error);
                // self.set_answer(DefaultAnswerStatus::Answer404, None);
                // return Err(RetrieveClusterError::RetrieveFrontend(frontend_error));
            }
        };

        let cluster_id = match route {
            Route::Cluster { id, .. } => id,
            Route::Deny => {
                panic!("Route::Deny");
                // self.set_answer(DefaultAnswerStatus::Answer401, None);
                // return Err(RetrieveClusterError::UnauthorizedRoute);
            }
        };

        Ok(cluster_id)
    }

    pub fn backend_from_request(
        &mut self,
        cluster_id: &str,
        frontend_should_stick: bool,
        context: &mut HttpContext,
        proxy: Rc<RefCell<dyn L7Proxy>>,
        _metrics: &mut SessionMetrics,
    ) -> Result<TcpStream, BackendConnectionError> {
        let (backend, conn) = self
            .get_backend_for_sticky_session(
                cluster_id,
                frontend_should_stick,
                context.sticky_session_found.as_deref(),
                proxy,
            )
            .map_err(|backend_error| {
                panic!("{backend_error}")
                // self.set_answer(DefaultAnswerStatus::Answer503, None);
                // BackendConnectionError::Backend(backend_error)
            })?;

        if frontend_should_stick {
            // update sticky name in case it changed I guess?
            context.sticky_name = self.listener.borrow().get_sticky_name().to_string();

            context.sticky_session = Some(
                backend
                    .borrow()
                    .sticky_id
                    .clone()
                    .unwrap_or_else(|| backend.borrow().backend_id.clone()),
            );
        }

        // metrics.backend_id = Some(backend.borrow().backend_id.clone());
        // metrics.backend_start();
        // self.set_backend_id(backend.borrow().backend_id.clone());
        // self.backend = Some(backend);

        Ok(conn)
    }

    fn get_backend_for_sticky_session(
        &self,
        cluster_id: &str,
        frontend_should_stick: bool,
        sticky_session: Option<&str>,
        proxy: Rc<RefCell<dyn L7Proxy>>,
    ) -> Result<(Rc<RefCell<Backend>>, TcpStream), BackendError> {
        match (frontend_should_stick, sticky_session) {
            (true, Some(sticky_session)) => proxy
                .borrow()
                .backends()
                .borrow_mut()
                .backend_from_sticky_session(cluster_id, sticky_session),
            _ => proxy
                .borrow()
                .backends()
                .borrow_mut()
                .backend_from_cluster_id(cluster_id),
        }
    }
}

pub struct Mux {
    pub frontend_token: Token,
    pub frontend: Connection<FrontRustls>,
    pub router: Router,
    pub public_address: SocketAddr,
    pub peer_address: Option<SocketAddr>,
    pub sticky_name: String,
    pub context: Context,
}

impl Mux {
    pub fn front_socket(&self) -> &TcpStream {
        self.frontend.socket()
    }
}

impl SessionState for Mux {
    fn ready(
        &mut self,
        session: Rc<RefCell<dyn ProxySession>>,
        proxy: Rc<RefCell<dyn L7Proxy>>,
        metrics: &mut SessionMetrics,
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
                match self
                    .frontend
                    .readable(context, EndpointClient(&mut self.router))
                {
                    MuxResult::Continue => (),
                    MuxResult::CloseSession => return SessionResult::Close,
                    MuxResult::Close(_) => todo!(),
                    MuxResult::Connect(stream_id) => {
                        match self.router.connect(
                            stream_id,
                            context,
                            session.clone(),
                            proxy.clone(),
                            metrics,
                        ) {
                            Ok(_) => (),
                            Err(error) => {
                                println!("{error}");
                            }
                        }
                    }
                }
                dirty = true;
            }

            for (_, backend) in self.router.backends.iter_mut() {
                if backend.readiness().filter_interest().is_writable() {
                    match backend.writable(context, EndpointServer(&mut self.frontend)) {
                        MuxResult::Continue => (),
                        MuxResult::CloseSession => return SessionResult::Close,
                        MuxResult::Close(_) => todo!(),
                        MuxResult::Connect(_) => unreachable!(),
                    }
                    dirty = true;
                }

                if backend.readiness().filter_interest().is_readable() {
                    match backend.readable(context, EndpointServer(&mut self.frontend)) {
                        MuxResult::Continue => (),
                        MuxResult::CloseSession => return SessionResult::Close,
                        MuxResult::Close(_) => todo!(),
                        MuxResult::Connect(_) => unreachable!(),
                    }
                    dirty = true;
                }
            }

            if self.frontend.readiness().filter_interest().is_writable() {
                match self
                    .frontend
                    .writable(context, EndpointClient(&mut self.router))
                {
                    MuxResult::Continue => (),
                    MuxResult::CloseSession => return SessionResult::Close,
                    MuxResult::Close(_) => todo!(),
                    MuxResult::Connect(_) => unreachable!(),
                }
                dirty = true;
            }

            for backend in self.router.backends.values() {
                if backend.readiness().filter_interest().is_hup()
                    || backend.readiness().filter_interest().is_error()
                {
                    println!("{:?} {:?}", backend.readiness(), backend.socket());
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
        } else if let Some(c) = self.router.backends.get_mut(&token) {
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
        for stream in &mut self.context.streams {
            for kawa in [&mut stream.front, &mut stream.back] {
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
}
