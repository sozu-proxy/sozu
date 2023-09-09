use std::{collections::HashMap, str::from_utf8_unchecked};

use rusty_ulid::Ulid;
use sozu_command::ready::Ready;

use crate::{
    println_,
    protocol::mux::{
        converter, debug_kawa,
        parser::{self, error_code_to_str, Frame, FrameHeader, FrameType, H2Error},
        pkawa, serializer, update_readiness_after_read, update_readiness_after_write,
        BackendStatus, Context, Endpoint, GenericHttpStream, GlobalStreamId, MuxResult, Position,
        StreamId, StreamState, set_default_answer, forcefully_terminate_answer,
    },
    socket::SocketHandler,
    Readiness,
};

#[inline(always)]
fn error_nom_to_h2(error: nom::Err<parser::Error>) -> MuxResult {
    match error {
        nom::Err::Error(parser::Error {
            error: parser::InnerError::H2(e),
            ..
        }) => return MuxResult::CloseSession(e),
        _ => return MuxResult::CloseSession(H2Error::ProtocolError),
    }
}

#[derive(Debug)]
pub enum H2State {
    ClientPreface,
    ClientSettings,
    ServerSettings,
    Header,
    Frame(FrameHeader),
    Error,
}

#[derive(Debug)]
pub struct H2Settings {
    pub settings_header_table_size: u32,
    pub settings_enable_push: bool,
    pub settings_max_concurrent_streams: u32,
    pub settings_initial_window_size: u32,
    pub settings_max_frame_size: u32,
    pub settings_max_header_list_size: u32,
}

impl Default for H2Settings {
    fn default() -> Self {
        Self {
            settings_header_table_size: 4096,
            settings_enable_push: true,
            settings_max_concurrent_streams: u32::MAX,
            settings_initial_window_size: (1 << 16) - 1,
            settings_max_frame_size: 1 << 14,
            settings_max_header_list_size: u32::MAX,
        }
    }
}

struct Prioriser {}

pub struct ConnectionH2<Front: SocketHandler> {
    pub decoder: hpack::Decoder<'static>,
    pub encoder: hpack::Encoder<'static>,
    pub expect_read: Option<(H2StreamId, usize)>,
    pub expect_write: Option<H2StreamId>,
    pub position: Position,
    pub readiness: Readiness,
    pub local_settings: H2Settings,
    pub peer_settings: H2Settings,
    pub socket: Front,
    pub state: H2State,
    pub last_stream_id: StreamId,
    pub streams: HashMap<StreamId, GlobalStreamId>,
    pub zero: GenericHttpStream,
    pub window: u32,
}
impl<Front: SocketHandler> std::fmt::Debug for ConnectionH2<Front> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionH2")
            .field("expect", &self.expect_read)
            .field("position", &self.position)
            .field("readiness", &self.readiness)
            .field("local_settings", &self.local_settings)
            .field("peer_settings", &self.peer_settings)
            .field("socket", &self.socket.socket_ref())
            .field("state", &self.state)
            .field("streams", &self.streams)
            .field("zero", &self.zero.storage.meter(20))
            .field("window", &self.window)
            .finish()
    }
}

#[derive(Debug, Clone, Copy)]
pub enum H2StreamId {
    Zero,
    Other(StreamId, GlobalStreamId),
}

impl<Front: SocketHandler> ConnectionH2<Front> {
    pub fn readable<E>(&mut self, context: &mut Context, endpoint: E) -> MuxResult
    where
        E: Endpoint,
    {
        println_!("======= MUX H2 READABLE {:?}", self.position);
        let (stream_id, kawa) = if let Some((stream_id, amount)) = self.expect_read {
            let kawa = match stream_id {
                H2StreamId::Zero => &mut self.zero,
                H2StreamId::Other(stream_id, global_stream_id) => {
                    context.streams[global_stream_id].rbuffer(&self.position)
                }
            };
            println_!("{:?}({stream_id:?}, {amount})", self.state);
            if amount > 0 {
                let (size, status) = self.socket.socket_read(&mut kawa.storage.space()[..amount]);
                kawa.storage.fill(size);
                if update_readiness_after_read(size, status, &mut self.readiness) {
                    return MuxResult::Continue;
                } else {
                    if size == amount {
                        self.expect_read = None;
                    } else {
                        self.expect_read = Some((stream_id, amount - size));
                        return MuxResult::Continue;
                    }
                }
            } else {
                self.expect_read = None;
            }
            (stream_id, kawa)
        } else {
            self.readiness.event.remove(Ready::READABLE);
            return MuxResult::Continue;
        };
        match (&self.state, &self.position) {
            (H2State::Error, _)
            | (H2State::ServerSettings, Position::Server)
            | (H2State::ClientPreface, Position::Client(_))
            | (H2State::ClientSettings, Position::Client(_)) => unreachable!(
                "Unexpected combination: (Writable, {:?}, {:?})",
                self.state, self.position
            ),
            (H2State::ClientPreface, Position::Server) => {
                let i = kawa.storage.data();
                let i = match parser::preface(i) {
                    Ok((i, _)) => i,
                    Err(_) => return MuxResult::CloseSession(H2Error::ProtocolError),
                };
                match parser::frame_header(i) {
                    Ok((
                        _,
                        FrameHeader {
                            payload_len,
                            frame_type: FrameType::Settings,
                            flags: 0,
                            stream_id: 0,
                        },
                    )) => {
                        kawa.storage.clear();
                        self.state = H2State::ClientSettings;
                        self.expect_read = Some((H2StreamId::Zero, payload_len as usize));
                    }
                    _ => return MuxResult::CloseSession(H2Error::ProtocolError),
                };
            }
            (H2State::ClientSettings, Position::Server) => {
                let i = kawa.storage.data();
                let settings = match parser::settings_frame(
                    i,
                    &FrameHeader {
                        payload_len: i.len() as u32,
                        frame_type: FrameType::Settings,
                        flags: 0,
                        stream_id: 0,
                    },
                ) {
                    Ok((_, settings)) => {
                        kawa.storage.clear();
                        settings
                    }
                    Err(_) => return MuxResult::CloseSession(H2Error::ProtocolError),
                };
                let kawa = &mut self.zero;
                match serializer::gen_frame_header(
                    kawa.storage.space(),
                    &FrameHeader {
                        payload_len: 0,
                        frame_type: FrameType::Settings,
                        flags: 0,
                        stream_id: 0,
                    },
                ) {
                    Ok((_, size)) => kawa.storage.fill(size),
                    Err(e) => {
                        println!("could not serialize HeaderFrame: {e:?}");
                        return MuxResult::CloseSession(H2Error::InternalError);
                    }
                };

                self.state = H2State::ServerSettings;
                self.expect_write = Some(H2StreamId::Zero);
                self.handle(settings, context, endpoint);
            }
            (H2State::ServerSettings, Position::Client(_)) => {
                let i = kawa.storage.data();
                match parser::frame_header(i) {
                    Ok((
                        _,
                        header @ FrameHeader {
                            payload_len,
                            frame_type: FrameType::Settings,
                            flags: 0,
                            stream_id: 0,
                        },
                    )) => {
                        kawa.storage.clear();
                        self.expect_read = Some((H2StreamId::Zero, payload_len as usize));
                        self.state = H2State::Frame(header)
                    }
                    _ => return MuxResult::CloseSession(H2Error::ProtocolError),
                };
            }
            (H2State::Header, _) => {
                let i = kawa.storage.data();
                println_!("  header: {i:?}");
                match parser::frame_header(i) {
                    Ok((_, header)) => {
                        println_!("{header:#?}");
                        kawa.storage.clear();
                        let stream_id = header.stream_id;
                        let stream_id = if stream_id == 0
                            || header.frame_type == FrameType::RstStream
                        {
                            H2StreamId::Zero
                        } else {
                            let global_stream_id = if let Some(global_stream_id) =
                                self.streams.get(&stream_id)
                            {
                                *global_stream_id
                            } else {
                                match self.create_stream(stream_id, context) {
                                    Some(global_stream_id) => global_stream_id,
                                    None => return MuxResult::CloseSession(H2Error::InternalError),
                                }
                            };
                            if header.frame_type == FrameType::Data {
                                H2StreamId::Other(stream_id, global_stream_id)
                            } else {
                                H2StreamId::Zero
                            }
                        };
                        println_!("{} {stream_id:?} {:#?}", header.stream_id, self.streams);
                        self.expect_read = Some((stream_id, header.payload_len as usize));
                        self.state = H2State::Frame(header);
                    }
                    Err(e) => return error_nom_to_h2(e),
                };
            }
            (H2State::Frame(header), _) => {
                let i = kawa.storage.data();
                println_!("  data: {i:?}");
                let frame = match parser::frame_body(
                    i,
                    header,
                    self.local_settings.settings_max_frame_size,
                ) {
                    Ok((_, frame)) => frame,
                    Err(e) => return error_nom_to_h2(e),
                };
                if let H2StreamId::Zero = stream_id {
                    kawa.storage.clear();
                }
                let state_result = self.handle(frame, context, endpoint);
                self.state = H2State::Header;
                self.expect_read = Some((H2StreamId::Zero, 9));
                return state_result;
            }
        }
        MuxResult::Continue
    }

    pub fn writable<E>(&mut self, context: &mut Context, mut endpoint: E) -> MuxResult
    where
        E: Endpoint,
    {
        println_!("======= MUX H2 WRITABLE {:?}", self.position);
        if let Some(H2StreamId::Zero) = self.expect_write {
            let kawa = &mut self.zero;
            println_!("{:?}", kawa.storage.data());
            while !kawa.storage.is_empty() {
                let (size, status) = self.socket.socket_write(kawa.storage.data());
                kawa.storage.consume(size);
                if update_readiness_after_write(size, status, &mut self.readiness) {
                    return MuxResult::Continue;
                }
            }
            // when H2StreamId::Zero is used to write READABLE is disabled
            // so when we finish the write we enable READABLE again
            self.readiness.interest.insert(Ready::READABLE);
            self.expect_write = None;
        }
        match (&self.state, &self.position) {
            (H2State::Error, _)
            | (H2State::ClientPreface, Position::Server)
            | (H2State::ClientSettings, Position::Server)
            | (H2State::ServerSettings, Position::Client(_)) => unreachable!(
                "Unexpected combination: (Readable, {:?}, {:?})",
                self.state, self.position
            ),
            (H2State::ClientPreface, Position::Client(_)) => {
                println_!("Preparing preface and settings");
                let pri = serializer::H2_PRI.as_bytes();
                let kawa = &mut self.zero;

                kawa.storage.space()[0..pri.len()].copy_from_slice(pri);
                kawa.storage.fill(pri.len());
                match serializer::gen_settings(kawa.storage.space(), &self.local_settings) {
                    Ok((_, size)) => kawa.storage.fill(size),
                    Err(e) => {
                        println!("could not serialize SettingsFrame: {e:?}");
                        return MuxResult::CloseSession(H2Error::InternalError);
                    }
                };

                self.state = H2State::ClientSettings;
                self.expect_write = Some(H2StreamId::Zero);
                MuxResult::Continue
            }
            (H2State::ClientSettings, Position::Client(_)) => {
                println_!("Sent preface and settings");
                self.state = H2State::ServerSettings;
                self.readiness.interest.remove(Ready::WRITABLE);
                self.expect_read = Some((H2StreamId::Zero, 9));
                MuxResult::Continue
            }
            (H2State::ServerSettings, Position::Server) => {
                self.state = H2State::Header;
                self.readiness.interest.remove(Ready::WRITABLE);
                self.expect_read = Some((H2StreamId::Zero, 9));
                MuxResult::Continue
            }
            // Proxying states (Header/Frame)
            (_, _) => {
                let mut dead_streams = Vec::new();

                if let Some(H2StreamId::Other(stream_id, global_stream_id)) = self.expect_write {
                    let stream = &mut context.streams[global_stream_id];
                    let kawa = stream.wbuffer(&self.position);
                    while !kawa.out.is_empty() {
                        let bufs = kawa.as_io_slice();
                        let (size, status) = self.socket.socket_write_vectored(&bufs);
                        kawa.consume(size);
                        if update_readiness_after_write(size, status, &mut self.readiness) {
                            return MuxResult::Continue;
                        }
                    }
                    self.expect_write = None;
                    if (kawa.is_terminated() || kawa.is_error()) && kawa.is_completed() {
                        match self.position {
                            Position::Client(_) => {}
                            Position::Server => {
                                // mark stream as reusable
                                println_!("Recycle stream: {global_stream_id}");
                                let mut state = StreamState::Recycle;
                                std::mem::swap(&mut stream.state, &mut state);
                                if let StreamState::Linked(token) = state {
                                    endpoint.end_stream(token, global_stream_id, context);
                                }
                                dead_streams.push(stream_id);
                            }
                        }
                    }
                }

                let mut converter = converter::H2BlockConverter {
                    stream_id: 0,
                    state: StreamState::Idle,
                    encoder: &mut self.encoder,
                    out: Vec::new(),
                };
                let mut priorities = self.streams.keys().collect::<Vec<_>>();
                priorities.sort();

                println_!("PRIORITIES: {priorities:?}");
                'outer: for stream_id in priorities {
                    let global_stream_id = *self.streams.get(stream_id).unwrap();
                    let stream = &mut context.streams[global_stream_id];
                    converter.state = stream.state;
                    let kawa = stream.wbuffer(&self.position);
                    if kawa.is_main_phase() || kawa.is_error() {
                        converter.stream_id = *stream_id;
                        kawa.prepare(&mut converter);
                        debug_kawa(kawa);
                    }
                    while !kawa.out.is_empty() {
                        let bufs = kawa.as_io_slice();
                        let (size, status) = self.socket.socket_write_vectored(&bufs);
                        kawa.consume(size);
                        if update_readiness_after_write(size, status, &mut self.readiness) {
                            self.expect_write =
                                Some(H2StreamId::Other(*stream_id, global_stream_id));
                            break 'outer;
                        }
                    }
                    if (kawa.is_terminated() || kawa.is_error()) && kawa.is_completed() {
                        match self.position {
                            Position::Client(_) => {}
                            Position::Server => {
                                // mark stream as reusable
                                println_!("Recycle stream: {global_stream_id}");
                                let mut state = StreamState::Recycle;
                                std::mem::swap(&mut stream.state, &mut state);
                                if let StreamState::Linked(token) = state {
                                    endpoint.end_stream(token, global_stream_id, context);
                                }
                                dead_streams.push(*stream_id);
                            }
                        }
                    }
                }
                for stream_id in dead_streams {
                    self.streams.remove(&stream_id).unwrap();
                }

                if self.expect_write.is_none() {
                    // We wrote everything
                    self.readiness.interest.remove(Ready::WRITABLE);
                }
                MuxResult::Continue
            }
        }
    }

    pub fn create_stream(
        &mut self,
        stream_id: StreamId,
        context: &mut Context,
    ) -> Option<GlobalStreamId> {
        let global_stream_id = context.create_stream(
            Ulid::generate(),
            self.peer_settings.settings_initial_window_size,
        )?;
        if (stream_id >> 1) > self.last_stream_id {
            self.last_stream_id = stream_id >> 1;
        }
        self.streams.insert(stream_id, global_stream_id);
        Some(global_stream_id)
    }

    pub fn new_stream_id(&mut self) -> StreamId {
        self.last_stream_id += 2;
        match self.position {
            Position::Client(_) => self.last_stream_id + 1,
            Position::Server => self.last_stream_id,
        }
    }

    fn handle<E>(&mut self, frame: Frame, context: &mut Context, mut endpoint: E) -> MuxResult
    where
        E: Endpoint,
    {
        println_!("{frame:#?}");
        match frame {
            Frame::Data(data) => {
                let mut slice = data.payload;
                let global_stream_id = match self.streams.get(&data.stream_id) {
                    Some(global_stream_id) => *global_stream_id,
                    None => return MuxResult::CloseSession(H2Error::ProtocolError),
                };
                let stream = &mut context.streams[global_stream_id];
                let kawa = stream.rbuffer(&self.position);
                slice.start += kawa.storage.head as u32;
                kawa.storage.head += slice.len();
                let buffer = kawa.storage.buffer();
                let payload = slice.data(buffer);
                println_!("{:?}", unsafe { from_utf8_unchecked(payload) });
                kawa.push_block(kawa::Block::Chunk(kawa::Chunk {
                    data: kawa::Store::Slice(slice),
                }));
                if data.end_stream {
                    kawa.push_block(kawa::Block::Flags(kawa::Flags {
                        end_body: true,
                        end_chunk: false,
                        end_header: false,
                        end_stream: true,
                    }));
                    kawa.parsing_phase = kawa::ParsingPhase::Terminated;
                }
            }
            Frame::Headers(headers) => {
                if !headers.end_headers {
                    todo!();
                    // self.state = H2State::Continuation
                }
                // can this fail?
                let global_stream_id = *self.streams.get(&headers.stream_id).unwrap();
                let kawa = &mut self.zero;
                let buffer = headers.header_block_fragment.data(kawa.storage.buffer());
                let stream = &mut context.streams[global_stream_id];
                let parts = &mut stream.split(&self.position);
                pkawa::handle_header(
                    parts.rbuffer,
                    buffer,
                    headers.end_stream,
                    &mut self.decoder,
                    parts.context,
                );
                debug_kawa(parts.rbuffer);
                match self.position {
                    Position::Client(_) => {
                        let StreamState::Linked(token) = stream.state else { unreachable!() };
                        endpoint
                            .readiness_mut(token)
                            .interest
                            .insert(Ready::WRITABLE)
                    }
                    Position::Server => stream.state = StreamState::Link,
                };
            }
            Frame::PushPromise(push_promise) => match self.position {
                Position::Client(_) => {
                    if self.local_settings.settings_enable_push {
                        todo!("forward the push")
                    } else {
                        return MuxResult::CloseSession(H2Error::ProtocolError);
                    }
                }
                Position::Server => {
                    println_!("A client should not push promises");
                    return MuxResult::CloseSession(H2Error::ProtocolError);
                }
            },
            Frame::Priority(priority) => (),
            Frame::RstStream(rst_stream) => {
                println_!(
                    "RstStream({} -> {})",
                    rst_stream.error_code,
                    error_code_to_str(rst_stream.error_code)
                );
                self.streams.remove(&rst_stream.stream_id);
            }
            Frame::Settings(settings) => {
                if settings.ack {
                    return MuxResult::Continue;
                }
                for setting in settings.settings {
                    match setting.identifier {
                        1 => self.peer_settings.settings_header_table_size = setting.value,
                        2 => self.peer_settings.settings_enable_push = setting.value == 1,
                        3 => self.peer_settings.settings_max_concurrent_streams = setting.value,
                        4 => self.peer_settings.settings_initial_window_size = setting.value,
                        5 => self.peer_settings.settings_max_frame_size = setting.value,
                        6 => self.peer_settings.settings_max_header_list_size = setting.value,
                        other => println!("unknown setting_id: {other}, we MUST ignore this"),
                    }
                }
                println_!("{:#?}", self.peer_settings);

                let kawa = &mut self.zero;
                kawa.storage.space()[0..serializer::SETTINGS_ACKNOWLEDGEMENT.len()]
                    .copy_from_slice(&serializer::SETTINGS_ACKNOWLEDGEMENT);
                kawa.storage
                    .fill(serializer::SETTINGS_ACKNOWLEDGEMENT.len());

                self.readiness.interest.insert(Ready::WRITABLE);
                self.readiness.interest.remove(Ready::READABLE);
                self.expect_write = Some(H2StreamId::Zero);
            }
            Frame::Ping(ping) => {
                let kawa = &mut self.zero;
                match serializer::gen_ping_acknolegment(kawa.storage.space(), &ping.payload) {
                    Ok((_, size)) => kawa.storage.fill(size),
                    Err(e) => {
                        println!("could not serialize PingFrame: {e:?}");
                        return MuxResult::CloseSession(H2Error::InternalError);
                    }
                };
                self.readiness.interest.insert(Ready::WRITABLE);
                self.readiness.interest.remove(Ready::READABLE);
                self.expect_write = Some(H2StreamId::Zero);
            }
            Frame::GoAway(goaway) => {
                println_!(
                    "GoAway({} -> {})",
                    goaway.error_code,
                    error_code_to_str(goaway.error_code)
                );
                todo!();
            }
            Frame::WindowUpdate(update) => {
                if update.stream_id == 0 {
                    self.window += update.increment;
                } else {
                    let global_stream_id = match self.streams.get(&update.stream_id) {
                        Some(global_stream_id) => *global_stream_id,
                        None => return MuxResult::CloseSession(H2Error::ProtocolError),
                    };
                    context.streams[global_stream_id].window += update.increment as i32;
                }
            }
            Frame::Continuation(_) => todo!(),
        }
        MuxResult::Continue
    }

    pub fn close<E>(&mut self, context: &mut Context, mut endpoint: E)
    where
        E: Endpoint,
    {
        match self.position {
            Position::Client(BackendStatus::Connected(_))
            | Position::Client(BackendStatus::Connecting(_)) => {}
            Position::Client(BackendStatus::Disconnecting)
            | Position::Client(BackendStatus::KeepAlive(_)) => unreachable!(),
            Position::Server => unreachable!(),
        }
        // reconnection is handled by the server for each stream separately
        for global_stream_id in self.streams.values() {
            println_!("end stream: {global_stream_id}");
            let StreamState::Linked(token) = context.streams[*global_stream_id].state else { unreachable!() };
            endpoint.end_stream(token, *global_stream_id, context)
        }
    }

    pub fn end_stream(&mut self, stream: GlobalStreamId, context: &mut Context) {
        let stream_context = &mut context.streams[stream].context;
        println_!("end H2 stream {stream}: {stream_context:#?}");
        match self.position {
            Position::Client(_) => {
                for (stream_id, global_stream_id) in &self.streams {
                    if *global_stream_id == stream {
                        let id = *stream_id;
                        self.streams.remove(&id);
                        return;
                    }
                }
                unreachable!()
            }
            Position::Server => {
                let stream = &mut context.streams[stream];
                match (stream.front.consumed, stream.back.is_main_phase()) {
                    (_, true) => {
                        // front might not have been consumed (in case of PushPromise)
                        // we have a "forwardable" answer from the back
                        // if the answer is not terminated we send an RstStream to properly clean the stream
                        // if it is terminated, we finish the transfer, the backend is not necessary anymore
                        if !stream.back.is_terminated() {
                            forcefully_terminate_answer(&mut stream.back, &mut self.readiness);
                        }
                        stream.state = StreamState::Unlinked
                    }
                    (true, false) => {
                        // we do not have an answer, but the request has already been partially consumed
                        // so we can't retry, send a 502 bad gateway instead
                        // note: it might be possible to send a RstStream with an adequate error code
                        set_default_answer(&mut stream.back, &mut self.readiness, 502);
                        stream.state = StreamState::Unlinked;
                        }
                    (false, false) => {
                        // we do not have an answer, but the request is untouched so we can retry
                        println!("H2 RECONNECT");
                        stream.state = StreamState::Link
                    }
                }
            }
        }
    }

    pub fn start_stream(&mut self, stream: GlobalStreamId, context: &mut Context) {
        println_!("start new H2 stream {stream} {:?}", self.readiness);
        let stream_id = self.new_stream_id();
        self.streams.insert(stream_id, stream);
        self.readiness.interest.insert(Ready::WRITABLE);
    }
}
