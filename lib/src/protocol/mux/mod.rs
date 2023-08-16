use std::{
    cell::RefCell,
    collections::HashMap,
    net::SocketAddr,
    rc::{Rc, Weak},
};

use mio::{net::TcpStream, Token};
use rusty_ulid::Ulid;
use sozu_command::ready::Ready;

mod parser;
mod serializer;

use crate::{
    https::HttpsListener,
    pool::{Checkout, Pool},
    protocol::SessionState,
    socket::{FrontRustls, SocketHandler, SocketResult},
    AcceptError, L7Proxy, ProxySession, Readiness, SessionMetrics, SessionResult, StateResult,
};

/// Generic Http representation using the Kawa crate using the Checkout of Sozu as buffer
type GenericHttpStream = kawa::Kawa<Checkout>;
type StreamId = usize;
type GlobalStreamId = usize;

#[derive(Debug, Clone, Copy)]
pub enum Position {
    Client,
    Server,
}

pub struct ConnectionH1<Front: SocketHandler> {
    pub socket: Front,
    pub position: Position,
    pub readiness: Readiness,
    pub stream: GlobalStreamId,
}

#[derive(Debug)]
pub enum H2State {
    ClientPreface,
    ClientSettings,
    ServerSettings,
    Header,
    Frame,
    Error,
}
pub struct ConnectionH2<Front: SocketHandler> {
    pub socket: Front,
    pub position: Position,
    pub readiness: Readiness,
    pub state: H2State,
    pub streams: HashMap<StreamId, GlobalStreamId>,
    pub expect: Option<(GlobalStreamId, usize)>,
    // context_hpack: HpackContext,
    // settings: SettiongsH2,
}
pub struct Stream {
    pub request_id: Ulid,
    pub front: GenericHttpStream,
    pub back: GenericHttpStream,
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

pub enum Connection<Front: SocketHandler> {
    H1(ConnectionH1<Front>),
    H2(ConnectionH2<Front>),
}
impl<Front: SocketHandler> Connection<Front> {
    fn readiness(&self) -> &Readiness {
        match self {
            Connection::H1(c) => &c.readiness,
            Connection::H2(c) => &c.readiness,
        }
    }
    fn readiness_mut(&mut self) -> &mut Readiness {
        match self {
            Connection::H1(c) => &mut c.readiness,
            Connection::H2(c) => &mut c.readiness,
        }
    }
    fn readable(&mut self, streams: &mut [Stream]) {
        match self {
            Connection::H1(c) => c.readable(streams),
            Connection::H2(c) => c.readable(streams),
        }
    }
    fn writable(&mut self, streams: &mut [Stream]) {
        match self {
            Connection::H1(c) => c.writable(streams),
            Connection::H2(c) => c.writable(streams),
        }
    }
}

pub struct Mux {
    pub frontend_token: Token,
    pub frontend: Connection<FrontRustls>,
    pub backends: HashMap<Token, Connection<TcpStream>>,
    pub streams: Vec<Stream>,
    pub listener: Rc<RefCell<HttpsListener>>,
    pub pool: Weak<RefCell<Pool>>,
    pub public_address: SocketAddr,
    pub peer_address: Option<SocketAddr>,
    pub sticky_name: String,
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

        let streams = &mut self.streams;
        while counter < max_loop_iterations {
            let mut dirty = false;

            if self.frontend.readiness().filter_interest().is_readable() {
                self.frontend.readable(streams);
                dirty = true;
            }

            for (_, backend) in self.backends.iter_mut() {
                if backend.readiness().filter_interest().is_writable() {
                    backend.writable(streams);
                    dirty = true;
                }

                if backend.readiness().filter_interest().is_readable() {
                    backend.readable(streams);
                    dirty = true;
                }
            }

            if self.frontend.readiness().filter_interest().is_writable() {
                self.frontend.writable(streams);
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

    fn timeout(&mut self, token: Token, metrics: &mut SessionMetrics) -> StateResult {
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
    }
}

impl Mux {
    pub fn front_socket(&self) -> &TcpStream {
        match &self.frontend {
            Connection::H1(c) => &c.socket.stream,
            Connection::H2(c) => &c.socket.stream,
        }
    }

    pub fn create_stream(&mut self, request_id: Ulid) -> Result<GlobalStreamId, AcceptError> {
        let (front_buffer, back_buffer) = match self.pool.upgrade() {
            Some(pool) => {
                let mut pool = pool.borrow_mut();
                match (pool.checkout(), pool.checkout()) {
                    (Some(front_buffer), Some(back_buffer)) => (front_buffer, back_buffer),
                    _ => return Err(AcceptError::BufferCapacityReached),
                }
            }
            None => return Err(AcceptError::BufferCapacityReached),
        };
        self.streams.push(Stream {
            request_id,
            front: GenericHttpStream::new(kawa::Kind::Request, kawa::Buffer::new(front_buffer)),
            back: GenericHttpStream::new(kawa::Kind::Request, kawa::Buffer::new(back_buffer)),
        });
        Ok(self.streams.len() - 1)
    }
}

impl<Front: SocketHandler> ConnectionH2<Front> {
    fn readable(&mut self, streams: &mut [Stream]) {
        println!("======= MUX H2 READABLE");
        let kawa = if let Some((stream_id, amount)) = self.expect {
            let kawa = streams[stream_id].front(self.position);
            let (size, status) = self.socket.socket_read(&mut kawa.storage.space()[..amount]);
            println!("{:?}({stream_id}, {amount}) {size} {status:?}", self.state);
            if size > 0 {
                kawa.storage.fill(size);
                if size == amount {
                    self.expect = None;
                } else {
                    self.expect = Some((stream_id, amount - size));
                    return;
                }
            } else {
                self.readiness.event.remove(Ready::READABLE);
                return;
            }
            kawa
        } else {
            self.readiness.event.remove(Ready::READABLE);
            return;
        };
        match (&self.state, &self.position) {
            (H2State::ClientPreface, Position::Client) => {
                error!("Waiting for ClientPreface to finish writing")
            }
            (H2State::ClientPreface, Position::Server) => {
                let i = kawa.storage.data();
                let i = match parser::preface(i) {
                    Ok((i, _)) => i,
                    Err(e) => panic!("{e:?}"),
                };
                match parser::frame_header(i) {
                    Ok((
                        _,
                        parser::FrameHeader {
                            payload_len,
                            frame_type: parser::FrameType::Settings,
                            flags: 0,
                            stream_id: 0,
                        },
                    )) => {
                        kawa.storage.clear();
                        self.state = H2State::ClientSettings;
                        self.expect = Some((0, payload_len as usize));
                    }
                    _ => todo!(),
                };
            }
            (H2State::ClientSettings, Position::Server) => {
                let i = kawa.storage.data();
                match parser::settings_frame(i, i.len()) {
                    Ok((_, settings)) => println!("{settings:?}"),
                    Err(e) => panic!("{e:?}"),
                }
                kawa.storage.clear();
                let kawa = &mut streams[0].back;
                self.state = H2State::ServerSettings;
                match serializer::gen_frame_header(
                    kawa.storage.space(),
                    &parser::FrameHeader {
                        payload_len: 6 * 2,
                        frame_type: parser::FrameType::Settings,
                        flags: 0,
                        stream_id: 0,
                    },
                ) {
                    Ok((_, size)) => kawa.storage.fill(size),
                    Err(e) => panic!("could not serialize HeaderFrame: {e:?}"),
                };
                // kawa.storage
                //     .write(&[1, 3, 0, 0, 0, 100, 0, 4, 0, 1, 0, 0])
                //     .unwrap();
                match serializer::gen_frame_header(
                    kawa.storage.space(),
                    &parser::FrameHeader {
                        payload_len: 0,
                        frame_type: parser::FrameType::Settings,
                        flags: 1,
                        stream_id: 0,
                    },
                ) {
                    Ok((_, size)) => kawa.storage.fill(size),
                    Err(e) => panic!("could not serialize HeaderFrame: {e:?}"),
                };
                self.readiness.interest.insert(Ready::WRITABLE);
                self.readiness.interest.remove(Ready::READABLE);
            }
            (H2State::ServerSettings, Position::Client) => todo!("Receive server Settings"),
            (H2State::ServerSettings, Position::Server) => {
                error!("waiting for ServerPreface to finish writing")
            }
            (H2State::Header, Position::Server) => {
                let i = kawa.storage.data();
                println!("  header: {i:?}");
                match parser::frame_header(i) {
                    Ok((_, header)) => {
                        println!("{header:?}");
                        kawa.storage.clear();
                        self.state = H2State::Frame;
                        self.expect =
                            Some((header.stream_id as usize, header.payload_len as usize));
                    }
                    Err(e) => panic!("{e:?}"),
                };
            }
            (H2State::Frame, Position::Server) => {
                let i = kawa.storage.data();
                println!("  data: {i:?}");
                kawa.storage.clear();
                self.state = H2State::Header;
                self.expect = Some((0, 9));
            }
            _ => unreachable!(),
        }
    }

    fn writable(&mut self, streams: &mut [Stream]) {
        println!("======= MUX H2 WRITABLE");
        match (&self.state, &self.position) {
            (H2State::ClientPreface, Position::Client) => todo!("Send PRI + client Settings"),
            (H2State::ClientPreface, Position::Server) => unreachable!(),
            (H2State::ServerSettings, Position::Client) => unreachable!(),
            (H2State::ServerSettings, Position::Server) => {
                let stream = &mut streams[0];
                let kawa = &mut stream.back;
                println!("{:?}", kawa.storage.data());
                let (size, status) = self.socket.socket_write(kawa.storage.data());
                println!("  size: {size}, status: {status:?}");
                let size = kawa.storage.available_data();
                kawa.storage.consume(size);
                if kawa.storage.is_empty() {
                    self.readiness.interest.remove(Ready::WRITABLE);
                    self.readiness.interest.insert(Ready::READABLE);
                    self.state = H2State::Header;
                    self.expect = Some((0, 9));
                }
            }
            _ => unreachable!(),
        }
        // for global_stream_id in self.streams.values() {
        //     let stream = &mut streams[*global_stream_id];
        //     let kawa = match self.position {
        //         Position::Client => &mut stream.back,
        //         Position::Server => &mut stream.front,
        //     };
        //     kawa.prepare(&mut kawa::h2::BlockConverter);
        //     let (size, status) = self.socket.socket_write_vectored(&kawa.as_io_slice());
        //     println!("  size: {size}, status: {status:?}");
        //     kawa.consume(size);
        // }
    }
}

impl<Front: SocketHandler> ConnectionH1<Front> {
    fn readable(&mut self, streams: &mut [Stream]) {
        println!("======= MUX H1 READABLE");
        let stream = &mut streams[self.stream];
        let kawa = match self.position {
            Position::Client => &mut stream.front,
            Position::Server => &mut stream.back,
        };
        let (size, status) = self.socket.socket_read(kawa.storage.space());
        println!("  size: {size}, status: {status:?}");
        if size > 0 {
            kawa.storage.fill(size);
        } else {
            self.readiness.event.remove(Ready::READABLE);
        }
        match status {
            SocketResult::Continue => {}
            SocketResult::Closed => todo!(),
            SocketResult::Error => todo!(),
            SocketResult::WouldBlock => self.readiness.event.remove(Ready::READABLE),
        }
        kawa::h1::parse(kawa, &mut kawa::h1::NoCallbacks);
        kawa::debug_kawa(kawa);
        if kawa.is_terminated() {
            self.readiness.interest.remove(Ready::READABLE);
        }
    }
    fn writable(&mut self, streams: &mut [Stream]) {
        println!("======= MUX H1 WRITABLE");
        let stream = &mut streams[self.stream];
        let kawa = match self.position {
            Position::Client => &mut stream.back,
            Position::Server => &mut stream.front,
        };
        kawa.prepare(&mut kawa::h1::BlockConverter);
        let bufs = kawa.as_io_slice();
        if bufs.is_empty() {
            self.readiness.interest.remove(Ready::WRITABLE);
            return;
        }
        let (size, status) = self.socket.socket_write_vectored(&bufs);
        println!("  size: {size}, status: {status:?}");
        if size > 0 {
            kawa.consume(size);
            // self.backend_readiness.interest.insert(Ready::READABLE);
        } else {
            self.readiness.event.remove(Ready::WRITABLE);
        }
    }
}
