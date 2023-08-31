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

mod converter;
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

#[derive(Debug)]
pub enum Position {
    Client(BackendStatus),
    Server,
}

#[derive(Debug)]
pub enum BackendStatus {
    Connecting(String),
    Connected(String),
    KeepAlive(String),
    Disconnecting,
}

pub enum MuxResult {
    Continue,
    CloseSession,
    Close(GlobalStreamId),
    Connect(GlobalStreamId),
}

#[derive(Debug)]
pub enum Connection<Front: SocketHandler> {
    H1(ConnectionH1<Front>),
    H2(ConnectionH2<Front>),
}

pub trait Endpoint {
    fn readiness(&self, token: Token) -> &Readiness;
    fn readiness_mut(&mut self, token: Token) -> &mut Readiness;
    fn end_stream(&mut self, token: Token, stream: GlobalStreamId, context: &mut Context);
    fn start_stream(&mut self, token: Token, stream: GlobalStreamId, context: &mut Context);
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
    pub fn new_h1_client(front_stream: Front, cluster_id: String) -> Connection<Front> {
        Connection::H1(ConnectionH1 {
            socket: front_stream,
            position: Position::Client(BackendStatus::Connecting(cluster_id)),
            readiness: Readiness {
                interest: Ready::WRITABLE | Ready::READABLE | Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            stream: 0,
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
            decoder: hpack::Decoder::new(),
            encoder: hpack::Encoder::new(),
            last_stream_id: 0,
        }))
    }
    pub fn new_h2_client(
        front_stream: Front,
        cluster_id: String,
        pool: Weak<RefCell<Pool>>,
    ) -> Option<Connection<Front>> {
        let buffer = pool
            .upgrade()
            .and_then(|pool| pool.borrow_mut().checkout())?;
        Some(Connection::H2(ConnectionH2 {
            socket: front_stream,
            position: Position::Client(BackendStatus::Connecting(cluster_id)),
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
            decoder: hpack::Decoder::new(),
            encoder: hpack::Encoder::new(),
            last_stream_id: 0,
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
    fn position(&self) -> &Position {
        match self {
            Connection::H1(c) => &c.position,
            Connection::H2(c) => &c.position,
        }
    }
    fn position_mut(&mut self) -> &mut Position {
        match self {
            Connection::H1(c) => &mut c.position,
            Connection::H2(c) => &mut c.position,
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
        E: Endpoint,
    {
        match self {
            Connection::H1(c) => c.readable(context, endpoint),
            Connection::H2(c) => c.readable(context, endpoint),
        }
    }
    fn writable<E>(&mut self, context: &mut Context, endpoint: E) -> MuxResult
    where
        E: Endpoint,
    {
        match self {
            Connection::H1(c) => c.writable(context, endpoint),
            Connection::H2(c) => c.writable(context, endpoint),
        }
    }

    fn close<E>(&mut self, context: &mut Context, endpoint: E)
    where
        E: Endpoint,
    {
        match self {
            Connection::H1(c) => c.close(context, endpoint),
            Connection::H2(c) => c.close(context, endpoint),
        }
    }

    fn end_stream(&mut self, stream: GlobalStreamId, context: &mut Context) {
        match self {
            Connection::H1(c) => c.end_stream(stream, context),
            Connection::H2(c) => c.end_stream(stream, context),
        }
    }

    fn start_stream(&mut self, stream: GlobalStreamId, context: &mut Context) {
        match self {
            Connection::H1(c) => c.start_stream(stream, context),
            Connection::H2(c) => c.start_stream(stream, context),
        }
    }
}

struct EndpointServer<'a>(&'a mut Connection<FrontRustls>);
struct EndpointClient<'a>(&'a mut Router);

// note: EndpointServer are used by client Connection, they do not know the frontend Token
// they will use the Stream's Token which is their backend token
impl<'a> Endpoint for EndpointServer<'a> {
    fn readiness(&self, _token: Token) -> &Readiness {
        self.0.readiness()
    }
    fn readiness_mut(&mut self, _token: Token) -> &mut Readiness {
        self.0.readiness_mut()
    }

    fn end_stream(&mut self, token: Token, stream: GlobalStreamId, context: &mut Context) {
        // this may be used to forward H2<->H2 RstStream
        // or to handle backend hup
        self.0.end_stream(stream, context);
    }

    fn start_stream(&mut self, token: Token, stream: GlobalStreamId, context: &mut Context) {
        // this may be used to forward H2<->H2 PushPromise
        todo!()
    }
}
impl<'a> Endpoint for EndpointClient<'a> {
    fn readiness(&self, token: Token) -> &Readiness {
        self.0.backends.get(&token).unwrap().readiness()
    }
    fn readiness_mut(&mut self, token: Token) -> &mut Readiness {
        self.0.backends.get_mut(&token).unwrap().readiness_mut()
    }

    fn end_stream(&mut self, token: Token, stream: GlobalStreamId, context: &mut Context) {
        self.0
            .backends
            .get_mut(&token)
            .unwrap()
            .end_stream(stream, context);
    }

    fn start_stream(&mut self, token: Token, stream: GlobalStreamId, context: &mut Context) {
        self.0
            .backends
            .get_mut(&token)
            .unwrap()
            .start_stream(stream, context);
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
    pub active: bool,
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

fn temporary_http_context(request_id: Ulid) -> HttpContext {
    HttpContext {
        keep_alive_backend: true,
        keep_alive_frontend: true,
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
    }
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
            active: true,
            window: window as i32,
            token: None,
            front: GenericHttpStream::new(kawa::Kind::Request, kawa::Buffer::new(front_buffer)),
            back: GenericHttpStream::new(kawa::Kind::Response, kawa::Buffer::new(back_buffer)),
            context: temporary_http_context(request_id),
        })
    }
    pub fn split(&mut self, position: &Position) -> StreamParts<'_> {
        match position {
            Position::Client(_) => StreamParts {
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
    pub fn rbuffer(&mut self, position: &Position) -> &mut GenericHttpStream {
        match position {
            Position::Client(_) => &mut self.back,
            Position::Server => &mut self.front,
        }
    }
    pub fn wbuffer(&mut self, position: &Position) -> &mut GenericHttpStream {
        match position {
            Position::Client(_) => &mut self.front,
            Position::Server => &mut self.back,
        }
    }
}

pub struct Context {
    pub streams: Vec<Stream>,
    pub pool: Weak<RefCell<Pool>>,
}

impl Context {
    pub fn create_stream(&mut self, request_id: Ulid, window: u32) -> Option<GlobalStreamId> {
        for (stream_id, stream) in self.streams.iter_mut().enumerate() {
            if !stream.active {
                println!("Reuse stream: {stream_id}");
                stream.window = window as i32;
                stream.context = temporary_http_context(request_id);
                stream.back.clear();
                stream.back.storage.clear();
                stream.front.clear();
                stream.front.storage.clear();
                stream.token = None;
                stream.active = true;
                return Some(stream_id);
            }
        }
        self.streams
            .push(Stream::new(self.pool.clone(), request_id, window)?);
        Some(self.streams.len() - 1)
    }

    pub fn new(pool: Weak<RefCell<Pool>>) -> Context {
        Self {
            streams: Vec::new(),
            pool,
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
        // when reused, a stream should be detached from its old connection, if not we could end
        // with concurrent connections on a single endpoint
        assert!(stream.token.is_none());
        let stream_context = &mut stream.context;
        let (cluster_id, h2) = self
            .route_from_request(stream_context, proxy.clone())
            .map_err(BackendConnectionError::RetrieveClusterError)?;

        let frontend_should_stick = proxy
            .borrow()
            .clusters()
            .get(&cluster_id)
            .map(|cluster| cluster.sticky_session)
            .unwrap_or(false);

        let mut reuse_token = None;
        // let mut priority = 0;
        let mut reuse_connecting = true;
        for (token, backend) in &self.backends {
            match (h2, reuse_connecting, backend.position()) {
                (_, _, Position::Server) => panic!("Backend connection behaves like a server"),
                (_, _, Position::Client(BackendStatus::Disconnecting)) => {}

                (true, _, Position::Client(BackendStatus::Connected(old_cluster_id))) => {
                    if *old_cluster_id == cluster_id {
                        reuse_token = Some(*token);
                        reuse_connecting = false;
                        break;
                    }
                }
                (true, true, Position::Client(BackendStatus::Connecting(old_cluster_id))) => {
                    if *old_cluster_id == cluster_id {
                        reuse_token = Some(*token)
                    }
                }
                (true, false, Position::Client(BackendStatus::Connecting(_))) => {}
                (true, _, Position::Client(BackendStatus::KeepAlive(_))) => {
                    panic!("ConnectionH2 behaves like H1")
                }

                (false, _, Position::Client(BackendStatus::KeepAlive(old_cluster_id))) => {
                    if *old_cluster_id == cluster_id {
                        reuse_token = Some(*token);
                        reuse_connecting = false;
                        break;
                    }
                }
                // can't bundle H1 streams together
                (false, _, Position::Client(BackendStatus::Connected(_)))
                | (false, _, Position::Client(BackendStatus::Connecting(_))) => {}
            }
        }
        println!("connect: {cluster_id} (stick={frontend_should_stick}, h2={h2}) -> (reuse={reuse_token:?})");

        let token = if let Some(token) = reuse_token {
            println!("reused backend: {:#?}", self.backends.get(&token).unwrap());
            token
        } else {
            let mut socket = self.backend_from_request(
                &cluster_id,
                frontend_should_stick,
                stream_context,
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

            let token = proxy.borrow().add_session(session);

            if let Err(e) = proxy.borrow().register_socket(
                &mut socket,
                token,
                Interest::READABLE | Interest::WRITABLE,
            ) {
                error!("error registering back socket({:?}): {:?}", socket, e);
            }

            let connection = if h2 {
                Connection::new_h2_client(socket, cluster_id, context.pool.clone()).unwrap()
            } else {
                Connection::new_h1_client(socket, cluster_id)
            };
            self.backends.insert(token, connection);
            token
        };

        // link stream to backend
        stream.token = Some(token);
        // link backend to stream
        self.backends
            .get_mut(&token)
            .unwrap()
            .start_stream(stream_id, context);
        Ok(())
    }

    fn route_from_request(
        &mut self,
        context: &mut HttpContext,
        _proxy: Rc<RefCell<dyn L7Proxy>>,
    ) -> Result<(String, bool), RetrieveClusterError> {
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
            Route::Cluster { id, h2 } => (id, h2),
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

            let mut dead_backends = Vec::new();
            for (token, backend) in self.router.backends.iter_mut() {
                let readiness = backend.readiness().filter_interest();
                if readiness.is_hup() || readiness.is_error() {
                    println!(
                        "{token:?} -> {:?} {:?}",
                        backend.readiness(),
                        backend.socket()
                    );
                    backend.close(context, EndpointServer(&mut self.frontend));
                    dead_backends.push(*token);
                }
                if readiness.is_writable() {
                    let mut owned_position = Position::Server;
                    let position = backend.position_mut();
                    std::mem::swap(&mut owned_position, position);
                    match owned_position {
                        Position::Client(BackendStatus::Connecting(cluster_id)) => {
                            *position = Position::Client(BackendStatus::Connected(cluster_id));
                        }
                        _ => *position = owned_position,
                    }
                    match backend.writable(context, EndpointServer(&mut self.frontend)) {
                        MuxResult::Continue => (),
                        MuxResult::CloseSession => return SessionResult::Close,
                        MuxResult::Close(_) => todo!(),
                        MuxResult::Connect(_) => unreachable!(),
                    }
                    dirty = true;
                }

                if readiness.is_readable() {
                    match backend.readable(context, EndpointServer(&mut self.frontend)) {
                        MuxResult::Continue => (),
                        MuxResult::CloseSession => return SessionResult::Close,
                        MuxResult::Close(_) => todo!(),
                        MuxResult::Connect(_) => unreachable!(),
                    }
                    dirty = true;
                }
            }
            if !dead_backends.is_empty() {
                for token in &dead_backends {
                    self.router.backends.remove(token);
                }
                println!("FRONTEND: {:#?}", &self.frontend);
                println!("BACKENDS: {:#?}", self.router.backends);
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

    fn update_readiness(&mut self, token: Token, events: Ready) {
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
