use std::{cmp::min, collections::HashMap};

use rusty_ulid::Ulid;
use sozu_command::ready::Ready;

use crate::{
    println_,
    protocol::mux::{
        converter, debug_kawa, forcefully_terminate_answer,
        parser::{
            self, error_code_to_str, Frame, FrameHeader, FrameType, H2Error, Headers, ParserError,
            ParserErrorKind, WindowUpdate,
        },
        pkawa, serializer, set_default_answer, update_readiness_after_read,
        update_readiness_after_write, BackendStatus, Context, Endpoint, GenericHttpStream,
        GlobalStreamId, MuxResult, Position, StreamId, StreamState,
    },
    socket::SocketHandler,
    timer::TimeoutContainer,
    L7ListenerHandler, ListenerHandler, Readiness,
};

#[inline(always)]
fn error_nom_to_h2(error: nom::Err<parser::ParserError>) -> H2Error {
    match error {
        nom::Err::Error(parser::ParserError {
            kind: parser::ParserErrorKind::H2(e),
            ..
        }) => return e,
        nom::Err::Failure(parser::ParserError {
            kind: parser::ParserErrorKind::H2(e),
            ..
        }) => return e,
        _ => return H2Error::ProtocolError,
    }
}

#[derive(Debug)]
pub enum H2State {
    ClientPreface,
    ClientSettings,
    ServerSettings,
    Header,
    Frame(FrameHeader),
    ContinuationHeader(Headers),
    ContinuationFrame(Headers),
    GoAway,
    Error,
    Discard,
}

#[derive(Debug)]
pub struct H2Settings {
    pub settings_header_table_size: u32,
    pub settings_enable_push: bool,
    pub settings_max_concurrent_streams: u32,
    pub settings_initial_window_size: u32,
    pub settings_max_frame_size: u32,
    pub settings_max_header_list_size: u32,
    /// RFC 8441
    pub settings_enable_connect_protocol: bool,
    /// RFC 9218
    pub settings_no_rfc7540_priorities: bool,
}

impl Default for H2Settings {
    fn default() -> Self {
        Self {
            settings_header_table_size: 4096,
            settings_enable_push: false,
            settings_max_concurrent_streams: 100,
            settings_initial_window_size: (1 << 16) - 1,
            settings_max_frame_size: 1 << 14,
            settings_max_header_list_size: u32::MAX,
            settings_enable_connect_protocol: false,
            settings_no_rfc7540_priorities: true,
        }
    }
}

pub struct Prioriser {}

impl Prioriser {
    pub fn new() -> Self {
        Self {}
    }
    pub fn push_priority(&mut self, stream_id: StreamId, priority: parser::PriorityPart) -> bool {
        println_!("PRIORITY REQUEST FOR {stream_id}: {priority:?}");
        match priority {
            parser::PriorityPart::Rfc7540 {
                stream_dependency,
                weight,
            } => {
                if stream_dependency.stream_id == stream_id {
                    println_!("STREAM CAN'T DEPEND ON ITSELF");
                    true
                } else {
                    false
                }
            }
            parser::PriorityPart::Rfc9218 {
                urgency,
                incremental,
            } => false,
        }
    }
}

pub struct ConnectionH2<Front: SocketHandler> {
    pub decoder: hpack::Decoder<'static>,
    pub encoder: hpack::Encoder<'static>,
    pub expect_read: Option<(H2StreamId, usize)>,
    pub expect_write: Option<H2StreamId>,
    pub last_stream_id: StreamId,
    pub local_settings: H2Settings,
    pub peer_settings: H2Settings,
    pub position: Position,
    pub prioriser: Prioriser,
    pub readiness: Readiness,
    pub socket: Front,
    pub state: H2State,
    pub streams: HashMap<StreamId, GlobalStreamId>,
    pub timeout_container: TimeoutContainer,
    pub window: i32,
    pub zero: GenericHttpStream,
}
impl<Front: SocketHandler> std::fmt::Debug for ConnectionH2<Front> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionH2")
            .field("position", &self.position)
            .field("state", &self.state)
            .field("expect", &self.expect_read)
            .field("readiness", &self.readiness)
            .field("local_settings", &self.local_settings)
            .field("peer_settings", &self.peer_settings)
            .field("socket", &self.socket.socket_ref())
            .field("streams", &self.streams)
            .field("zero", &self.zero.storage.meter(20))
            .field("window", &self.window)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H2StreamId {
    Zero,
    Other(StreamId, GlobalStreamId),
}

impl<Front: SocketHandler> ConnectionH2<Front> {
    pub fn readable<E, L>(&mut self, context: &mut Context<L>, endpoint: E) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        println_!("======= MUX H2 READABLE {:?}", self.position);
        self.timeout_container.reset();
        let (stream_id, kawa) = if let Some((stream_id, amount)) = self.expect_read {
            let kawa = match stream_id {
                H2StreamId::Zero => &mut self.zero,
                H2StreamId::Other(stream_id, global_stream_id) => {
                    context.streams[global_stream_id]
                        .split(&self.position)
                        .rbuffer
                }
            };
            println_!("{:?}({stream_id:?}, {amount})", self.state);
            if amount > 0 {
                if amount > kawa.storage.available_space() {
                    self.readiness.interest.remove(Ready::READABLE);
                    return MuxResult::Continue;
                }
                let (size, status) = self.socket.socket_read(&mut kawa.storage.space()[..amount]);
                kawa.storage.fill(size);
                match self.position {
                    Position::Client(..) => {
                        count!("back_bytes_in", size as i64);
                    }
                    Position::Server => {
                        count!("bytes_in", size as i64);
                    }
                }
                if update_readiness_after_read(size, status, &mut self.readiness) {
                    return MuxResult::Continue;
                } else {
                    if size == amount {
                        self.expect_read = None;
                    } else {
                        self.expect_read = Some((stream_id, amount - size));
                        match (&self.state, &self.position) {
                            (H2State::ClientPreface, Position::Server) => {
                                let i = kawa.storage.data();
                                if !b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".starts_with(i) {
                                    println_!("EARLY INVALID PREFACE: {i:?}");
                                    return self.force_disconnect();
                                }
                            }
                            _ => {}
                        }
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
            | (H2State::GoAway, _)
            | (H2State::ServerSettings, Position::Server)
            | (H2State::ClientPreface, Position::Client(..))
            | (H2State::ClientSettings, Position::Client(..)) => unreachable!(
                "Unexpected combination: (Readable, {:?}, {:?})",
                self.state, self.position
            ),
            (H2State::Discard, _) => {
                let i = kawa.storage.data();
                println_!("DISCARDING: {i:?}");
                kawa.storage.clear();
                self.state = H2State::Header;
                self.expect_read = Some((H2StreamId::Zero, 9));
            }
            (H2State::ClientPreface, Position::Server) => {
                let i = kawa.storage.data();
                let i = match parser::preface(i) {
                    Ok((i, _)) => i,
                    Err(_) => return self.force_disconnect(),
                };
                match parser::frame_header(i, self.local_settings.settings_max_frame_size) {
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
                    _ => return self.force_disconnect(),
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
                    Err(_) => return self.force_disconnect(),
                };
                let kawa = &mut self.zero;
                match serializer::gen_settings(kawa.storage.space(), &self.local_settings) {
                    Ok((_, size)) => kawa.storage.fill(size),
                    Err(e) => {
                        println!("could not serialize SettingsFrame: {e:?}");
                        return self.force_disconnect();
                    }
                };

                self.state = H2State::ServerSettings;
                self.expect_write = Some(H2StreamId::Zero);
                return self.handle_frame(settings, context, endpoint);
            }
            (H2State::ServerSettings, Position::Client(..)) => {
                let i = kawa.storage.data();
                match parser::frame_header(i, self.local_settings.settings_max_frame_size) {
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
                    _ => return self.force_disconnect(),
                };
            }
            (H2State::Header, _) => {
                let i = kawa.storage.data();
                println_!("  header: {i:?}");
                match parser::frame_header(i, self.local_settings.settings_max_frame_size) {
                    Ok((_, header)) => {
                        println_!("{header:#?}");
                        kawa.storage.clear();
                        let stream_id = header.stream_id;
                        let read_stream = if stream_id == 0 {
                            H2StreamId::Zero
                        } else if let Some(global_stream_id) = self.streams.get(&stream_id) {
                            let allowed_on_half_closed = header.frame_type
                                == FrameType::WindowUpdate
                                || header.frame_type == FrameType::Priority;
                            let stream = &context.streams[*global_stream_id];
                            println_!(
                                "REQUESTING EXISTING STREAM {stream_id}: {}/{:?}",
                                stream.received_end_of_stream,
                                stream.state
                            );
                            if !allowed_on_half_closed
                                && (stream.received_end_of_stream || !stream.state.is_open())
                            {
                                return self.goaway(H2Error::StreamClosed);
                            }
                            if header.frame_type == FrameType::Data {
                                H2StreamId::Other(stream_id, *global_stream_id)
                            } else {
                                H2StreamId::Zero
                            }
                        } else {
                            if header.frame_type == FrameType::Headers
                                && self.position.is_server()
                                && stream_id % 2 == 1
                                && stream_id >= self.last_stream_id
                            {
                                if context.streams.len()
                                    >= self.local_settings.settings_max_concurrent_streams as usize
                                {
                                    return self.goaway(H2Error::RefusedStream);
                                }
                                match self.create_stream(stream_id, context) {
                                    Some(_) => {}
                                    None => return self.goaway(H2Error::InternalError),
                                }
                            } else if header.frame_type != FrameType::Priority {
                                println_!(
                                    "ONLY HEADERS AND PRIORITY CAN BE RECEIVED ON IDLE/CLOSED STREAMS"
                                );
                                return self.goaway(H2Error::ProtocolError);
                            }
                            H2StreamId::Zero
                        };
                        println_!("{} {stream_id:?} {:#?}", header.stream_id, self.streams);
                        self.expect_read = Some((read_stream, header.payload_len as usize));
                        self.state = H2State::Frame(header);
                    }
                    Err(nom::Err::Failure(ParserError {
                        kind: ParserErrorKind::UnknownFrame(skip),
                        ..
                    })) => {
                        self.expect_read = Some((H2StreamId::Zero, skip as usize));
                        self.state = H2State::Discard;
                    }
                    Err(error) => {
                        let error = error_nom_to_h2(error);
                        return self.goaway(error);
                    }
                };
            }
            (H2State::ContinuationHeader(headers), _) => {
                let i = kawa.storage.unparsed_data();
                println_!("  continuation header: {i:?}");
                match parser::frame_header(i, self.local_settings.settings_max_frame_size) {
                    Ok((
                        _,
                        FrameHeader {
                            payload_len,
                            frame_type: FrameType::Continuation,
                            flags,
                            stream_id,
                        },
                    )) => {
                        // println_!("{header:#?}");
                        kawa.storage.end -= 9;
                        assert_eq!(stream_id, headers.stream_id);
                        self.expect_read = Some((H2StreamId::Zero, payload_len as usize));
                        let mut headers = headers.clone();
                        headers.end_headers = flags & 0x4 != 0;
                        headers.header_block_fragment.len += payload_len;
                        self.state = H2State::ContinuationFrame(headers);
                    }
                    Err(error) => {
                        let error = error_nom_to_h2(error);
                        return self.goaway(error);
                    }
                    _ => return self.goaway(H2Error::ProtocolError),
                };
            }
            (H2State::Frame(header), _) => {
                let i = kawa.storage.unparsed_data();
                println_!("  data: {i:?}");
                let frame = match parser::frame_body(i, header) {
                    Ok((_, frame)) => frame,
                    Err(error) => {
                        let error = error_nom_to_h2(error);
                        return self.goaway(error);
                    }
                };
                if let H2StreamId::Zero = stream_id {
                    if header.frame_type == FrameType::Headers {
                        kawa.storage.head = kawa.storage.end;
                    } else {
                        kawa.storage.end = kawa.storage.head;
                    }
                }
                self.state = H2State::Header;
                self.expect_read = Some((H2StreamId::Zero, 9));
                return self.handle_frame(frame, context, endpoint);
            }
            (H2State::ContinuationFrame(headers), _) => {
                kawa.storage.head = kawa.storage.end;
                let i = kawa.storage.data();
                println_!("  data: {i:?}");
                let headers = headers.clone();
                self.state = H2State::Header;
                self.expect_read = Some((H2StreamId::Zero, 9));
                return self.handle_frame(Frame::Headers(headers), context, endpoint);
            }
        }
        MuxResult::Continue
    }

    pub fn writable<E, L>(&mut self, context: &mut Context<L>, mut endpoint: E) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        println_!("======= MUX H2 WRITABLE {:?}", self.position);
        self.timeout_container.reset();
        if let Some(H2StreamId::Zero) = self.expect_write {
            let kawa = &mut self.zero;
            println_!("{:?}", kawa.storage.data());
            while !kawa.storage.is_empty() {
                let (size, status) = self.socket.socket_write(kawa.storage.data());
                kawa.storage.consume(size);
                match self.position {
                    Position::Client(..) => {
                        count!("back_bytes_out", size as i64);
                    }
                    Position::Server => {
                        count!("bytes_out", size as i64);
                    }
                }
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
            | (H2State::Discard, _)
            | (H2State::ClientPreface, Position::Server)
            | (H2State::ClientSettings, Position::Server)
            | (H2State::ServerSettings, Position::Client(..)) => unreachable!(
                "Unexpected combination: (Writable, {:?}, {:?})",
                self.state, self.position
            ),
            (H2State::GoAway, _) => self.force_disconnect(),
            (H2State::ClientPreface, Position::Client(..)) => {
                println_!("Preparing preface and settings");
                let pri = serializer::H2_PRI.as_bytes();
                let kawa = &mut self.zero;

                kawa.storage.space()[0..pri.len()].copy_from_slice(pri);
                kawa.storage.fill(pri.len());
                match serializer::gen_settings(kawa.storage.space(), &self.local_settings) {
                    Ok((_, size)) => kawa.storage.fill(size),
                    Err(e) => {
                        println!("could not serialize SettingsFrame: {e:?}");
                        return self.force_disconnect();
                    }
                };

                self.state = H2State::ClientSettings;
                self.expect_write = Some(H2StreamId::Zero);
                MuxResult::Continue
            }
            (H2State::ClientSettings, Position::Client(..)) => {
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
            // Proxying states
            (H2State::Header, _)
            | (H2State::Frame(_), _)
            | (H2State::ContinuationFrame(_), _)
            | (H2State::ContinuationHeader(_), _) => {
                let mut dead_streams = Vec::new();

                if let Some(write_stream @ H2StreamId::Other(stream_id, global_stream_id)) =
                    self.expect_write
                {
                    let stream = &mut context.streams[global_stream_id];
                    let parts = stream.split(&self.position);
                    let kawa = parts.wbuffer;
                    while !kawa.out.is_empty() {
                        let bufs = kawa.as_io_slice();
                        let (size, status) = self.socket.socket_write_vectored(&bufs);
                        kawa.consume(size);
                        match self.position {
                            Position::Client(..) => {
                                count!("back_bytes_out", size as i64);
                                parts.metrics.backend_bout += size;
                            }
                            Position::Server => {
                                count!("bytes_out", size as i64);
                                parts.metrics.bout += size;
                            }
                        }
                        if let Some((read_stream, amount)) = self.expect_read {
                            if write_stream == read_stream
                                && kawa.storage.available_space() >= amount
                            {
                                self.readiness.interest.insert(Ready::READABLE);
                            }
                        }
                        if update_readiness_after_write(size, status, &mut self.readiness) {
                            return MuxResult::Continue;
                        }
                    }
                    self.expect_write = None;
                    if (kawa.is_terminated() || kawa.is_error()) && kawa.is_completed() {
                        match self.position {
                            Position::Client(..) => {}
                            Position::Server => {
                                // mark stream as reusable
                                println_!("Recycle stream: {global_stream_id}");
                                // ACCESS LOG
                                stream.generate_access_log(
                                    false,
                                    Some(String::from("H2::SplitFrame")),
                                    context.listener.clone(),
                                );
                                let state =
                                    std::mem::replace(&mut stream.state, StreamState::Recycle);
                                if let StreamState::Linked(token) = state {
                                    endpoint.end_stream(token, global_stream_id, context);
                                }
                                dead_streams.push(stream_id);
                            }
                        }
                    }
                }

                let mut converter = converter::H2BlockConverter {
                    window: 0,
                    stream_id: 0,
                    encoder: &mut self.encoder,
                    out: Vec::new(),
                };
                let mut priorities = self.streams.keys().collect::<Vec<_>>();
                priorities.sort();

                println_!("PRIORITIES: {priorities:?}");
                'outer: for stream_id in priorities {
                    let global_stream_id = *self.streams.get(stream_id).unwrap();
                    let stream = &mut context.streams[global_stream_id];
                    let parts = stream.split(&self.position);
                    let kawa = parts.wbuffer;
                    if kawa.is_main_phase() || kawa.is_error() {
                        let window = min(*parts.window, self.window);
                        converter.window = window;
                        converter.stream_id = *stream_id;
                        kawa.prepare(&mut converter);
                        let consumed = window - converter.window;
                        *parts.window -= consumed;
                        self.window -= consumed;
                        debug_kawa(kawa);
                    }
                    while !kawa.out.is_empty() {
                        let bufs = kawa.as_io_slice();
                        let (size, status) = self.socket.socket_write_vectored(&bufs);
                        kawa.consume(size);
                        match self.position {
                            Position::Client(..) => {
                                count!("back_bytes_out", size as i64);
                                parts.metrics.backend_bout += size;
                            }
                            Position::Server => {
                                count!("bytes_out", size as i64);
                                parts.metrics.bout += size;
                            }
                        }
                        if update_readiness_after_write(size, status, &mut self.readiness) {
                            self.expect_write =
                                Some(H2StreamId::Other(*stream_id, global_stream_id));
                            break 'outer;
                        }
                    }
                    self.expect_write = None;
                    if (kawa.is_terminated() || kawa.is_error()) && kawa.is_completed() {
                        match self.position {
                            Position::Client(..) => {}
                            Position::Server => {
                                // mark stream as reusable
                                println_!("Recycle1 stream: {global_stream_id}");
                                // ACCESS LOG
                                stream.generate_access_log(
                                    false,
                                    Some(String::from("H2::WholeFrame")),
                                    context.listener.clone(),
                                );
                                let state =
                                    std::mem::replace(&mut stream.state, StreamState::Recycle);
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

    pub fn goaway(&mut self, error: H2Error) -> MuxResult {
        self.state = H2State::Error;
        self.expect_read = None;
        let kawa = &mut self.zero;
        kawa.storage.clear();

        match serializer::gen_goaway(kawa.storage.space(), self.last_stream_id, error) {
            Ok((_, size)) => {
                kawa.storage.fill(size);
                self.state = H2State::GoAway;
                self.expect_write = Some(H2StreamId::Zero);
                self.readiness.interest = Ready::WRITABLE | Ready::HUP | Ready::ERROR;
                MuxResult::Continue
            }
            Err(e) => {
                println!("could not serialize GoAwayFrame: {e:?}");
                self.force_disconnect()
            }
        }
    }

    pub fn create_stream<L>(
        &mut self,
        stream_id: StreamId,
        context: &mut Context<L>,
    ) -> Option<GlobalStreamId>
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        let global_stream_id = context.create_stream(
            Ulid::generate(),
            self.peer_settings.settings_initial_window_size,
        )?;
        self.last_stream_id = (stream_id + 2) & !1;
        self.streams.insert(stream_id, global_stream_id);
        Some(global_stream_id)
    }

    pub fn new_stream_id(&mut self) -> StreamId {
        self.last_stream_id += 2;
        match self.position {
            Position::Client(..) => self.last_stream_id - 1,
            Position::Server => self.last_stream_id - 2,
        }
    }

    fn handle_frame<E, L>(
        &mut self,
        frame: Frame,
        context: &mut Context<L>,
        mut endpoint: E,
    ) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        println_!("{frame:#?}");
        match frame {
            Frame::Data(data) => {
                let mut slice = data.payload;
                let global_stream_id = match self.streams.get(&data.stream_id) {
                    Some(global_stream_id) => *global_stream_id,
                    None => panic!("stream error"),
                };
                let stream = &mut context.streams[global_stream_id];
                let parts = stream.split(&self.position);
                let kawa = parts.rbuffer;
                match self.position {
                    Position::Client(..) => parts.metrics.backend_bin += slice.len(),
                    Position::Server => parts.metrics.bin += slice.len(),
                }
                slice.start += kawa.storage.head as u32;
                kawa.storage.head += slice.len();
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
                    stream.received_end_of_stream = true;
                }
                if let StreamState::Linked(token) = stream.state {
                    endpoint
                        .readiness_mut(token)
                        .interest
                        .insert(Ready::WRITABLE)
                }
            }
            Frame::Headers(headers) => {
                if !headers.end_headers {
                    // self.zero.storage.head = self.zero.storage.end;
                    println!("FRAGMENT: {:?}", self.zero.storage.data());
                    self.state = H2State::ContinuationHeader(headers);
                    return MuxResult::Continue;
                }
                // can this fail?
                let stream_id = headers.stream_id;
                let global_stream_id = *self.streams.get(&stream_id).unwrap();

                if let Some(priority) = &headers.priority {
                    if self.prioriser.push_priority(stream_id, priority.clone()) {
                        self.reset_stream(
                            global_stream_id,
                            context,
                            endpoint,
                            H2Error::ProtocolError,
                        );
                        return MuxResult::Continue;
                    }
                }

                let kawa = &mut self.zero;
                let buffer = headers.header_block_fragment.data(kawa.storage.buffer());
                let stream = &mut context.streams[global_stream_id];
                let parts = &mut stream.split(&self.position);
                match self.position {
                    Position::Client(..) => parts.metrics.backend_bin += buffer.len(),
                    Position::Server => parts.metrics.bin += buffer.len(),
                }
                let was_initial = parts.rbuffer.is_initial();
                let status = pkawa::handle_header(
                    &mut self.decoder,
                    &mut self.prioriser,
                    stream_id,
                    parts.rbuffer,
                    buffer,
                    headers.end_stream,
                    parts.context,
                );
                kawa.storage.clear();
                if let Err((error, global)) = status {
                    if global {
                        return self.goaway(error);
                    } else {
                        return self.reset_stream(global_stream_id, context, endpoint, error);
                    }
                }
                debug_kawa(parts.rbuffer);
                stream.received_end_of_stream |= headers.end_stream;
                if let StreamState::Linked(token) = stream.state {
                    endpoint
                        .readiness_mut(token)
                        .interest
                        .insert(Ready::WRITABLE)
                }
                // was_initial prevents trailers from triggering connection
                if was_initial && self.position.is_server() {
                    gauge_add!("http.active_requests", 1);
                    stream.state = StreamState::Link;
                }
            }
            Frame::PushPromise(push_promise) => match self.position {
                Position::Client(..) => {
                    if self.local_settings.settings_enable_push {
                        todo!("forward the push")
                    } else {
                        return self.goaway(H2Error::ProtocolError);
                    }
                }
                Position::Server => {
                    println_!("A client should not push promises");
                    return self.goaway(H2Error::ProtocolError);
                }
            },
            Frame::Priority(priority) => {
                if self
                    .prioriser
                    .push_priority(priority.stream_id, priority.inner)
                {
                    if let Some(global_stream_id) = self.streams.get(&priority.stream_id) {
                        return self.reset_stream(
                            *global_stream_id,
                            context,
                            endpoint,
                            H2Error::ProtocolError,
                        );
                    } else {
                        return self.goaway(H2Error::ProtocolError);
                    }
                }
            }
            Frame::RstStream(rst_stream) => {
                println_!(
                    "RstStream({} -> {})",
                    rst_stream.error_code,
                    error_code_to_str(rst_stream.error_code)
                );
                if let Some(stream_id) = self.streams.remove(&rst_stream.stream_id) {
                    let stream = &mut context.streams[stream_id];
                    if let StreamState::Linked(token) = stream.state {
                        endpoint.end_stream(token, stream_id, context);
                    }
                    let stream = &mut context.streams[stream_id];
                    match self.position {
                        Position::Client(..) => {}
                        Position::Server => {
                            // This is a special case, normally, all stream are terminated by the server
                            // when the last byte of the response is written. Here, the reset is requested
                            // on the server endpoint and immediately terminates, shortcutting the other path
                            // ACCESS LOG
                            stream.generate_access_log(
                                true,
                                Some(String::from("H2::ResetFrame")),
                                context.listener.clone(),
                            );
                            stream.state = StreamState::Recycle;
                        }
                    }
                }
            }
            Frame::Settings(settings) => {
                if settings.ack {
                    return MuxResult::Continue;
                }
                for setting in settings.settings {
                    let v = setting.value;
                    let mut is_error = false;
                    #[rustfmt::skip]
                    let _ = match setting.identifier {
                        1 => { self.peer_settings.settings_header_table_size = v },
                        2 => { self.peer_settings.settings_enable_push = v == 1;                is_error |= v > 1 },
                        3 => { self.peer_settings.settings_max_concurrent_streams = v },
                        4 => { is_error |= self.update_initial_window_size(v, context) },
                        5 => { self.peer_settings.settings_max_frame_size = v;                  is_error |= v >= 1<<24 || v < 1<<14 },
                        6 => { self.peer_settings.settings_max_header_list_size = v },
                        8 => { self.peer_settings.settings_enable_connect_protocol = v == 1;    is_error |= v > 1 },
                        9 => { self.peer_settings.settings_no_rfc7540_priorities = v == 1;      is_error |= v > 1 },
                        other => println!("unknown setting_id: {other}, we MUST ignore this"),
                    };
                    if is_error {
                        return self.goaway(H2Error::ProtocolError);
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
                if ping.ack {
                    return MuxResult::Continue;
                }
                let kawa = &mut self.zero;
                match serializer::gen_ping_acknolegment(kawa.storage.space(), &ping.payload) {
                    Ok((_, size)) => kawa.storage.fill(size),
                    Err(e) => {
                        println!("could not serialize PingFrame: {e:?}");
                        return self.force_disconnect();
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
                // return self.goaway(H2Error::NoError);
            }
            Frame::WindowUpdate(WindowUpdate {
                stream_id,
                increment,
            }) => {
                let increment = increment as i32;
                if stream_id == 0 {
                    if let Some(window) = self.window.checked_add(increment) {
                        if self.window <= 0 && window > 0 {
                            self.readiness.interest.insert(Ready::WRITABLE);
                        }
                        self.window = window;
                    } else {
                        return self.goaway(H2Error::FlowControlError);
                    }
                } else {
                    if let Some(global_stream_id) = self.streams.get(&stream_id) {
                        let stream = &mut context.streams[*global_stream_id];
                        if let Some(window) = stream.window.checked_add(increment) {
                            if stream.window <= 0 && window > 0 {
                                self.readiness.interest.insert(Ready::WRITABLE);
                            }
                            stream.window = window;
                        } else {
                            return self.reset_stream(
                                *global_stream_id,
                                context,
                                endpoint,
                                H2Error::FlowControlError,
                            );
                        }
                    } else {
                        println_!(
                            "Ignoring window update on closed stream {stream_id}: {increment}"
                        );
                    }
                };
            }
            Frame::Continuation(_) => unreachable!(),
        }
        MuxResult::Continue
    }

    fn update_initial_window_size<L>(&mut self, value: u32, context: &mut Context<L>) -> bool
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        if value >= 1 << 31 {
            return true;
        }
        let delta = value as i32 - self.peer_settings.settings_initial_window_size as i32;
        println!(
            "INITIAL_WINDOW_SIZE: {} -> {} => {}",
            self.peer_settings.settings_initial_window_size, value, delta
        );
        let mut open_window = false;
        for (i, stream) in context.streams.iter_mut().enumerate() {
            println!(
                " - stream_{i}: {} -> {}",
                stream.window,
                stream.window + delta
            );
            open_window |= stream.window <= 0 && stream.window + delta > 0;
            stream.window += delta;
        }
        println_!("UPDATE INIT WINDOW: {open_window} {:?}", self.readiness);
        if open_window {
            self.readiness.interest.insert(Ready::WRITABLE);
        }
        self.peer_settings.settings_initial_window_size = value;
        false
    }

    pub fn force_disconnect(&mut self) -> MuxResult {
        self.state = H2State::Error;
        match &mut self.position {
            Position::Client(_, _, status) => {
                *status = BackendStatus::Disconnecting;
                self.readiness.event = Ready::HUP;
                MuxResult::Continue
            }
            Position::Server => MuxResult::CloseSession,
        }
    }

    pub fn close<E, L>(&mut self, context: &mut Context<L>, mut endpoint: E)
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        match self.position {
            Position::Client(_, _, BackendStatus::KeepAlive) => unreachable!(),
            Position::Client(..) => {}
            Position::Server => unreachable!(),
        }
        // reconnection is handled by the server for each stream separately
        for global_stream_id in self.streams.values() {
            println_!("end stream: {global_stream_id}");
            let StreamState::Linked(token) = context.streams[*global_stream_id].state else {
                unreachable!()
            };
            endpoint.end_stream(token, *global_stream_id, context)
        }
    }

    pub fn reset_stream<E, L>(
        &mut self,
        stream_id: GlobalStreamId,
        context: &mut Context<L>,
        mut endpoint: E,
        error: H2Error,
    ) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        let stream = &mut context.streams[stream_id];
        println_!("reset H2 stream {stream_id}: {:#?}", stream.context);
        let old_state = std::mem::replace(&mut stream.state, StreamState::Unlinked);
        forcefully_terminate_answer(stream, &mut self.readiness, error);
        if let StreamState::Linked(token) = old_state {
            endpoint.end_stream(token, stream_id, context);
        }
        MuxResult::Continue
    }

    pub fn end_stream<L>(&mut self, stream: GlobalStreamId, context: &mut Context<L>)
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        let stream_context = &mut context.streams[stream].context;
        println_!("end H2 stream {stream}: {stream_context:#?}");
        match self.position {
            Position::Client(..) => {
                for (stream_id, global_stream_id) in &self.streams {
                    if *global_stream_id == stream {
                        let id = *stream_id;
                        // if the stream is not in a closed state we should probably send an
                        // RST_STREAM frame here. We also need to handle frames coming from
                        // the backend on this stream after it was closed
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
                            forcefully_terminate_answer(
                                stream,
                                &mut self.readiness,
                                H2Error::InternalError,
                            );
                        } else {
                            stream.state = StreamState::Unlinked;
                            self.readiness.interest.insert(Ready::WRITABLE);
                        }
                    }
                    (true, false) => {
                        // we do not have an answer, but the request has already been partially consumed
                        // so we can't retry, send a 502 bad gateway instead
                        // note: it might be possible to send a RstStream with an adequate error code
                        set_default_answer(stream, &mut self.readiness, 502);
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

    pub fn start_stream<L>(&mut self, stream: GlobalStreamId, context: &mut Context<L>)
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        println_!("start new H2 stream {stream} {:?}", self.readiness);
        let stream_id = self.new_stream_id();
        self.streams.insert(stream_id, stream);
        self.readiness.interest.insert(Ready::WRITABLE);
    }
}
