use std::{cmp::min, collections::HashMap};

use rusty_ulid::Ulid;
use sozu_command::ready::Ready;

use crate::{
    L7ListenerHandler, ListenerHandler, Protocol, Readiness,
    protocol::mux::{
        BackendStatus, Context, DebugEvent, Endpoint, GenericHttpStream, GlobalStreamId, MuxResult,
        Position, StreamId, StreamState, converter, forcefully_terminate_answer,
        parser::{
            self, Frame, FrameHeader, FrameType, H2Error, Headers, ParserError, ParserErrorKind,
            WindowUpdate, error_code_to_str,
        },
        pkawa, serializer, set_default_answer, update_readiness_after_read,
        update_readiness_after_write,
    },
    socket::SocketHandler,
    timer::TimeoutContainer,
};

#[inline(always)]
fn error_nom_to_h2(error: nom::Err<parser::ParserError>) -> H2Error {
    match error {
        nom::Err::Error(parser::ParserError {
            kind: parser::ParserErrorKind::H2(e),
            ..
        }) => e,
        nom::Err::Failure(parser::ParserError {
            kind: parser::ParserErrorKind::H2(e),
            ..
        }) => e,
        _ => H2Error::ProtocolError,
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

#[derive(Default)]
pub struct Prioriser {}

impl Prioriser {
    pub fn push_priority(&mut self, stream_id: StreamId, priority: parser::PriorityPart) -> bool {
        trace!("PRIORITY REQUEST FOR {}: {:?}", stream_id, priority);
        match priority {
            parser::PriorityPart::Rfc7540 {
                stream_dependency,
                weight: _,
            } => {
                if stream_dependency.stream_id == stream_id {
                    error!("STREAM CAN'T DEPEND ON ITSELF");
                    true
                } else {
                    false
                }
            }
            parser::PriorityPart::Rfc9218 {
                urgency: _,
                incremental: _,
            } => false,
        }
    }
}

pub struct ConnectionH2<Front: SocketHandler> {
    pub decoder: loona_hpack::Decoder<'static>,
    pub encoder: loona_hpack::Encoder<'static>,
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
    /// Outgoing flow control window: how many DATA bytes we can still send.
    /// Decremented when we send DATA, incremented when we receive WINDOW_UPDATE.
    pub window: i32,
    /// Accumulated connection-level DATA bytes received since last WINDOW_UPDATE sent.
    /// When this exceeds the threshold, we send a connection-level WINDOW_UPDATE.
    pub received_bytes_since_update: u32,
    /// Queued WINDOW_UPDATE frames to send: Vec<(stream_id, increment)>.
    /// stream_id=0 means connection-level update. Entries are coalesced per stream_id.
    pub pending_window_updates: Vec<(u32, u32)>,
    /// Highest stream ID accepted from the peer (used for GoAway last_stream_id).
    pub highest_peer_stream_id: StreamId,
    /// Reusable buffer for HPACK-encoded headers in the H2 block converter.
    pub converter_buf: Vec<u8>,
    /// RFC 9113 §6.8: when true, this connection is draining — no new streams accepted.
    /// Set when we receive or send a GoAway frame. The connection closes once all
    /// active streams complete (or are reset).
    pub draining: bool,
    /// RFC 9113 §6.8: the last_stream_id from a received GoAway frame.
    /// Streams with ID > this value were not processed by the peer and should be retried.
    pub peer_last_stream_id: Option<StreamId>,
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
    fn expect_header(&mut self) {
        self.state = H2State::Header;
        self.expect_read = Some((H2StreamId::Zero, 9));
    }
    pub fn readable<E, L>(&mut self, context: &mut Context<L>, endpoint: E) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        self.timeout_container.reset();
        let (stream_id, kawa) = if let Some((stream_id, amount)) = self.expect_read {
            let (kawa, did) = match stream_id {
                H2StreamId::Zero => (&mut self.zero, usize::MAX),
                H2StreamId::Other(_, global_stream_id) => (
                    context.streams[global_stream_id]
                        .split(&self.position)
                        .rbuffer,
                    global_stream_id,
                ),
            };
            trace!("{:?}({:?}, {})", self.state, stream_id, amount);
            if amount > 0 {
                if amount > kawa.storage.available_space() {
                    self.readiness.interest.remove(Ready::READABLE);
                    return MuxResult::Continue;
                }
                let (size, status) = self.socket.socket_read(&mut kawa.storage.space()[..amount]);
                context.debug.push(DebugEvent::I3(0, did, size));
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
                } else if size == amount {
                    self.expect_read = None;
                } else {
                    self.expect_read = Some((stream_id, amount - size));
                    if let (H2State::ClientPreface, Position::Server) =
                        (&self.state, &self.position)
                    {
                        let i = kawa.storage.data();
                        if !b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".starts_with(i) {
                            debug!("EARLY INVALID PREFACE: {:?}", i);
                            return self.force_disconnect();
                        }
                    }
                    return MuxResult::Continue;
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
            | (H2State::ClientSettings, Position::Client(..)) => {
                error!(
                    "Unexpected combination: (Readable, {:?}, {:?})",
                    self.state, self.position
                );
                return self.force_disconnect();
            }
            (H2State::Discard, _) => {
                let _i = kawa.storage.data();
                trace!("DISCARDING: {:?}", _i);
                kawa.storage.clear();
                self.expect_header();
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
                    Err(error) => {
                        error!("Could not serialize SettingsFrame: {:?}", error);
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
                trace!("  header: {:?}", i);
                match parser::frame_header(i, self.local_settings.settings_max_frame_size) {
                    Ok((_, header)) => {
                        trace!("{:#?}", header);
                        kawa.storage.clear();
                        let stream_id = header.stream_id;
                        let read_stream = if stream_id == 0 {
                            H2StreamId::Zero
                        } else if let Some(global_stream_id) = self.streams.get(&stream_id) {
                            let allowed_on_half_closed = header.frame_type
                                == FrameType::WindowUpdate
                                || header.frame_type == FrameType::Priority
                                || header.frame_type == FrameType::RstStream;
                            let stream = &context.streams[*global_stream_id];
                            // Use the position-aware end_of_stream flag:
                            // - Server reads from front (client requests)
                            // - Client reads from back (backend responses)
                            let received_eos = if self.position.is_server() {
                                stream.front_received_end_of_stream
                            } else {
                                stream.back_received_end_of_stream
                            };
                            trace!(
                                "REQUESTING EXISTING STREAM {}: {}/{:?}",
                                stream_id, received_eos, stream.state
                            );
                            if !allowed_on_half_closed && (received_eos || !stream.state.is_open())
                            {
                                error!(
                                    "CANNOT RECEIVE {:?} ON THIS STREAM {:?}",
                                    header.frame_type, stream.state
                                );
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
                                if context.active_len()
                                    >= self.local_settings.settings_max_concurrent_streams as usize
                                {
                                    error!(
                                        "MAX CONCURRENT STREAMS: {} {} {}",
                                        self.local_settings.settings_max_concurrent_streams,
                                        context.active_len(),
                                        context.streams.len()
                                    );
                                    return self.goaway(H2Error::RefusedStream);
                                }
                                match self.create_stream(stream_id, context) {
                                    Some(_) => {}
                                    None => {
                                        error!("COULD NOT CREATE NEW STREAM");
                                        return self.goaway(H2Error::InternalError);
                                    }
                                }
                            } else if header.frame_type != FrameType::Priority {
                                if header.frame_type == FrameType::Data
                                    && header.payload_len == 0
                                    && header.flags == 1
                                {
                                    // Empty DATA with END_STREAM on closed stream — safe to ignore
                                    self.expect_header();
                                    return MuxResult::Continue;
                                }
                                // Distinguish closed vs idle: if stream_id <=
                                // highest_peer_stream_id, the stream existed and is
                                // now closed; otherwise it was never opened (idle).
                                let is_closed_stream =
                                    header.stream_id <= self.highest_peer_stream_id;
                                if is_closed_stream
                                    && matches!(
                                        header.frame_type,
                                        FrameType::RstStream | FrameType::WindowUpdate
                                    )
                                {
                                    // RFC 9113 §5.1: RST_STREAM and WINDOW_UPDATE on a
                                    // closed stream can arrive due to race conditions
                                    // and SHOULD be ignored. Treat as stream 0 so the
                                    // frame body is parsed and discarded normally.
                                    debug!(
                                        "Ignoring {:?} on closed stream {}",
                                        header.frame_type, header.stream_id
                                    );
                                } else {
                                    error!(
                                        "CANNOT RECEIVE {:?} FRAME ON IDLE/CLOSED STREAMS",
                                        header.frame_type
                                    );
                                    return self.goaway(H2Error::ProtocolError);
                                }
                            }
                            H2StreamId::Zero
                        };
                        trace!("{} {:?} {:#?}", header.stream_id, stream_id, self.streams);
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
                        error!("COULD NOT PARSE FRAME HEADER");
                        return self.goaway(error);
                    }
                };
            }
            (H2State::ContinuationHeader(headers), _) => {
                let i = kawa.storage.unparsed_data();
                trace!("  continuation header: {:?}", i);
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
                        kawa.storage.end -= 9;
                        if stream_id != headers.stream_id {
                            error!(
                                "CONTINUATION stream_id {} does not match HEADERS stream_id {}",
                                stream_id, headers.stream_id
                            );
                            return self.goaway(H2Error::ProtocolError);
                        }
                        self.expect_read = Some((H2StreamId::Zero, payload_len as usize));
                        let mut headers = headers.clone();
                        headers.end_headers = flags & 0x4 != 0;
                        headers.header_block_fragment.len += payload_len;
                        self.state = H2State::ContinuationFrame(headers);
                    }
                    Err(error) => {
                        let error = error_nom_to_h2(error);
                        error!("COULD NOT PARSE CONTINUATION HEADER");
                        return self.goaway(error);
                    }
                    other => {
                        error!("UNEXPECTED {:?} WHILE PARSING CONTINUATION HEADER", other);
                        return self.goaway(H2Error::ProtocolError);
                    }
                };
            }
            (H2State::Frame(header), _) => {
                let i = kawa.storage.unparsed_data();
                trace!("  data: {:?}", i);
                let frame = match parser::frame_body(i, header) {
                    Ok((_, frame)) => frame,
                    Err(error) => {
                        let error = error_nom_to_h2(error);
                        error!("COULD NOT PARSE FRAME BODY");
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
                self.expect_header();
                return self.handle_frame(frame, context, endpoint);
            }
            (H2State::ContinuationFrame(headers), _) => {
                kawa.storage.head = kawa.storage.end;
                let i = kawa.storage.data();
                trace!("  data: {:?}", i);
                let headers = headers.clone();
                self.expect_header();
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
        self.timeout_container.reset();
        if let Some(H2StreamId::Zero) = self.expect_write {
            let kawa = &mut self.zero;
            while !kawa.storage.is_empty() {
                let (size, status) = self.socket.socket_write(kawa.storage.data());
                context.debug.push(DebugEvent::I2(1, size));
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

        // Send pending WINDOW_UPDATE frames before processing stream data.
        // Write them inline to avoid extra event loop iterations that could
        // cause response data to be sent before validating subsequent frames.
        if !self.pending_window_updates.is_empty() && self.expect_write.is_none() {
            let kawa = &mut self.zero;
            kawa.storage.clear();
            let buf = kawa.storage.space();
            let mut offset = 0;
            for (stream_id, increment) in self.pending_window_updates.drain(..) {
                if increment == 0 {
                    continue;
                }
                if let Ok((_, size)) =
                    serializer::gen_window_update(&mut buf[offset..], stream_id, increment)
                {
                    offset += size;
                }
            }
            if offset > 0 {
                kawa.storage.fill(offset);
                // Flush WINDOW_UPDATE frames directly without returning
                while !kawa.storage.is_empty() {
                    let (size, status) = self.socket.socket_write(kawa.storage.data());
                    kawa.storage.consume(size);
                    if update_readiness_after_write(size, status, &mut self.readiness) {
                        self.expect_write = Some(H2StreamId::Zero);
                        return MuxResult::Continue;
                    }
                }
            }
        }

        match (&self.state, &self.position) {
            (H2State::Error, _)
            | (H2State::Discard, _)
            | (H2State::ClientPreface, Position::Server)
            | (H2State::ClientSettings, Position::Server)
            | (H2State::ServerSettings, Position::Client(..)) => {
                error!(
                    "Unexpected combination: (Writable, {:?}, {:?})",
                    self.state, self.position
                );
                self.force_disconnect()
            }
            (H2State::GoAway, _) => self.force_disconnect(),
            (H2State::ClientPreface, Position::Client(..)) => {
                trace!("Preparing preface and settings");
                let pri = serializer::H2_PRI.as_bytes();
                let kawa = &mut self.zero;

                kawa.storage.space()[0..pri.len()].copy_from_slice(pri);
                kawa.storage.fill(pri.len());
                match serializer::gen_settings(kawa.storage.space(), &self.local_settings) {
                    Ok((_, size)) => kawa.storage.fill(size),
                    Err(error) => {
                        error!("Could not serialize SettingsFrame: {:?}", error);
                        return self.force_disconnect();
                    }
                };

                self.state = H2State::ClientSettings;
                self.expect_write = Some(H2StreamId::Zero);
                MuxResult::Continue
            }
            (H2State::ClientSettings, Position::Client(..)) => {
                trace!("Sent preface and settings");
                self.state = H2State::ServerSettings;
                self.expect_read = Some((H2StreamId::Zero, 9));
                self.readiness.interest.remove(Ready::WRITABLE);
                MuxResult::Continue
            }
            (H2State::ServerSettings, Position::Server) => {
                self.expect_header();
                self.readiness.interest.remove(Ready::WRITABLE);
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
                    let _stream_state = stream.state;
                    let parts = stream.split(&self.position);
                    let kawa = parts.wbuffer;
                    while !kawa.out.is_empty() {
                        let bufs = kawa.as_io_slice();
                        let (size, status) = self.socket.socket_write_vectored(&bufs);
                        context
                            .debug
                            .push(DebugEvent::I3(2, global_stream_id, size));
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
                                context.debug.push(DebugEvent::I2(4, global_stream_id));
                                trace!("Recycle stream: {}", global_stream_id);
                                incr!("http.e2e.h2");
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

                let scheme: &'static [u8] =
                    if context.listener.borrow().protocol() == Protocol::HTTPS {
                        b"https"
                    } else {
                        b"http"
                    };
                let mut converter_buf = std::mem::take(&mut self.converter_buf);
                converter_buf.clear();
                let mut converter = converter::H2BlockConverter {
                    max_frame_size: self.peer_settings.settings_max_frame_size as usize,
                    window: 0,
                    stream_id: 0,
                    encoder: &mut self.encoder,
                    out: converter_buf,
                    scheme,
                };
                let mut priorities = self.streams.keys().collect::<Vec<_>>();
                priorities.sort();

                trace!("PRIORITIES: {:?}", priorities);
                let mut socket_write = false;
                'outer: for stream_id in priorities {
                    let Some(&global_stream_id) = self.streams.get(stream_id) else {
                        error!(
                            "stream_id {} from sorted keys missing in streams map",
                            stream_id
                        );
                        continue;
                    };
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
                    }
                    context.debug.push(DebugEvent::S(
                        *stream_id,
                        global_stream_id,
                        kawa.parsing_phase,
                        kawa.blocks.len(),
                        kawa.out.len(),
                    ));
                    while !kawa.out.is_empty() {
                        socket_write = true;
                        let bufs = kawa.as_io_slice();
                        let (size, status) = self.socket.socket_write_vectored(&bufs);
                        context
                            .debug
                            .push(DebugEvent::I3(3, global_stream_id, size));
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
                                // Handle 1xx, this code should probably be merged with the h2 SplitFrame case and h1 nominal case
                                // to avoid code duplication

                                // mark stream as reusable
                                context.debug.push(DebugEvent::I2(5, global_stream_id));
                                if context.debug.is_interesting() {
                                    warn!("{:?}", context.debug.events);
                                    context.debug.set_interesting(false);
                                }
                                trace!("Recycle1 stream: {}", global_stream_id);
                                incr!("http.e2e.h2");
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
                    if self.streams.remove(&stream_id).is_none() {
                        error!("dead stream_id {} missing from streams map", stream_id);
                    }
                }

                // Reclaim the converter's HPACK buffer for reuse
                self.converter_buf = converter.out;

                // RFC 9113 §6.8: if draining and all streams have completed, close
                if self.draining && self.streams.is_empty() {
                    return self.goaway(H2Error::NoError);
                }

                if self.socket.socket_wants_write() {
                    if !socket_write {
                        self.socket.socket_write(&[]);
                    }
                } else if self.expect_write.is_none() {
                    // We wrote everything
                    context.debug.push(DebugEvent::Str(format!(
                        "Wrote everything: {:?}",
                        self.streams
                    )));
                    self.readiness.interest.remove(Ready::WRITABLE);
                }
                MuxResult::Continue
            }
        }
    }

    /// Queue a WINDOW_UPDATE, coalescing with any existing entry for the same stream_id.
    /// RFC 9113 §6.9.1: window size increment MUST be 1..2^31-1 (0x7FFFFFFF).
    fn queue_window_update(&mut self, stream_id: u32, increment: u32) {
        let max_increment = i32::MAX as u32;
        if let Some(entry) = self
            .pending_window_updates
            .iter_mut()
            .find(|(sid, _)| *sid == stream_id)
        {
            entry.1 = entry.1.saturating_add(increment).min(max_increment);
        } else {
            self.pending_window_updates
                .push((stream_id, increment.min(max_increment)));
        }
    }

    pub fn goaway(&mut self, error: H2Error) -> MuxResult {
        self.state = H2State::Error;
        self.draining = true;
        self.expect_read = None;
        let kawa = &mut self.zero;
        kawa.storage.clear();
        error!("GOAWAY: {:?}", error);

        // RFC 9113 §6.8: last_stream_id is the highest peer-initiated stream we processed
        match serializer::gen_goaway(kawa.storage.space(), self.highest_peer_stream_id, error) {
            Ok((_, size)) => {
                kawa.storage.fill(size);
                self.state = H2State::GoAway;
                self.expect_write = Some(H2StreamId::Zero);
                self.readiness.interest = Ready::WRITABLE | Ready::HUP | Ready::ERROR;
                MuxResult::Continue
            }
            Err(error) => {
                error!("Could not serialize GoAwayFrame: {:?}", error);
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
        // RFC 9113 §6.8: reject new streams on a draining connection
        if self.draining {
            error!("Rejecting new stream {} on draining connection", stream_id);
            return None;
        }
        // Track the highest peer-initiated stream ID for GoAway frames
        // before any early return, so GoAway always reports the correct last stream.
        if stream_id > self.highest_peer_stream_id {
            self.highest_peer_stream_id = stream_id;
        }
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
        trace!("{:#?}", frame);
        match frame {
            Frame::Data(data) => {
                let Some(global_stream_id) = self.streams.get(&data.stream_id).copied() else {
                    // the stream was terminated while data was expected,
                    // probably due to automatic answer for invalid/unauthorized access
                    // ignore this frame
                    return MuxResult::Continue;
                };
                let mut slice = data.payload;
                let stream = &mut context.streams[global_stream_id];
                let payload_len = slice.len();

                // Extract declared content-length and update position-aware data counter
                let (data_received, declared_length) = {
                    let parts = stream.split(&self.position);
                    *parts.data_received += payload_len;
                    let total = *parts.data_received;
                    let declared = match parts.rbuffer.body_size {
                        kawa::BodySize::Length(n) => Some(n),
                        _ => None,
                    };
                    (total, declared)
                };

                // RFC 9113 §8.1.1: if Content-Length is present, total DATA payload
                // must not exceed the declared length (check on every frame)
                if let Some(expected) = declared_length {
                    if data_received > expected {
                        error!(
                            "Content-Length mismatch: received {} > declared {}",
                            data_received, expected
                        );
                        return self.reset_stream(
                            global_stream_id,
                            context,
                            endpoint,
                            H2Error::ProtocolError,
                        );
                    }
                }

                let stream = &mut context.streams[global_stream_id];
                let parts = stream.split(&self.position);
                let kawa = parts.rbuffer;
                match self.position {
                    Position::Client(..) => parts.metrics.backend_bin += payload_len,
                    Position::Server => parts.metrics.bin += payload_len,
                }
                slice.start += kawa.storage.head as u32;
                kawa.storage.head += payload_len;
                kawa.push_block(kawa::Block::Chunk(kawa::Chunk {
                    data: kawa::Store::Slice(slice),
                }));

                // RFC 9113 §6.9: Update flow control after consuming DATA.
                // Track bytes received and queue WINDOW_UPDATE when threshold reached.
                let payload_u32 = payload_len as u32;
                let initial_window = self.local_settings.settings_initial_window_size;
                let threshold = initial_window / 2;

                // Connection-level flow control
                self.received_bytes_since_update += payload_u32;
                if self.received_bytes_since_update >= threshold {
                    let increment = self.received_bytes_since_update;
                    self.queue_window_update(0, increment);
                    self.received_bytes_since_update = 0;
                }

                // Stream-level flow control (only if stream is still open)
                if !data.end_stream {
                    self.queue_window_update(data.stream_id, payload_u32);
                }

                // If we have pending updates, ensure we get a writable event
                if !self.pending_window_updates.is_empty() {
                    self.readiness.interest.insert(Ready::WRITABLE);
                }

                if data.end_stream {
                    // RFC 9113 §8.1.1: on end_stream, total DATA must equal Content-Length
                    if let Some(expected) = declared_length {
                        if data_received != expected {
                            error!(
                                "Content-Length mismatch: received {} != declared {}",
                                data_received, expected
                            );
                            return self.reset_stream(
                                global_stream_id,
                                context,
                                endpoint,
                                H2Error::ProtocolError,
                            );
                        }
                    }
                    kawa.push_block(kawa::Block::Flags(kawa::Flags {
                        end_body: true,
                        end_chunk: false,
                        end_header: false,
                        end_stream: true,
                    }));
                    kawa.parsing_phase = kawa::ParsingPhase::Terminated;
                    if self.position.is_server() {
                        stream.front_received_end_of_stream = true;
                    } else {
                        stream.back_received_end_of_stream = true;
                    }
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
                    debug!("FRAGMENT: {:?}", self.zero.storage.data());
                    self.state = H2State::ContinuationHeader(headers);
                    return MuxResult::Continue;
                }
                // can this fail?
                let stream_id = headers.stream_id;
                let Some(global_stream_id) = self.streams.get(&stream_id).copied() else {
                    error!("Handling Headers frame with no attached stream {:#?}", self);
                    incr!("h2.headers_no_stream.error");
                    return self.force_disconnect();
                };

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
                        error!("GOT GLOBAL ERROR WHILE PROCESSING HEADERS");
                        return self.goaway(error);
                    } else {
                        return self.reset_stream(global_stream_id, context, endpoint, error);
                    }
                }
                if headers.end_stream {
                    if self.position.is_server() {
                        stream.front_received_end_of_stream = true;
                    } else {
                        stream.back_received_end_of_stream = true;
                    }
                }
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
            Frame::PushPromise(_push_promise) => match self.position {
                Position::Client(..) => {
                    // RFC 9113 §8.4: Server push is deprecated. Sozu never sends
                    // SETTINGS_ENABLE_PUSH=1, so receiving PUSH_PROMISE is a protocol error.
                    error!("Received PUSH_PROMISE but server push is not supported");
                    return self.goaway(H2Error::ProtocolError);
                }
                Position::Server => {
                    // Clients must never send PUSH_PROMISE (RFC 9113 §8.4)
                    error!("Received PUSH_PROMISE from client");
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
                        error!("INVALID PRIORITY RECEIVED ON INVALID STREAM");
                        return self.goaway(H2Error::ProtocolError);
                    }
                }
            }
            Frame::RstStream(rst_stream) => {
                debug!(
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
                    match setting.identifier {
                        1 => {
                            self.peer_settings.settings_header_table_size = v;
                            // Propagate peer's table size to our HPACK encoder
                            self.encoder.set_max_table_size(v as usize);
                        },
                        2 => { self.peer_settings.settings_enable_push = v == 1;                is_error |= v > 1 },
                        3 => { self.peer_settings.settings_max_concurrent_streams = v },
                        4 => { is_error |= self.update_initial_window_size(v, context) },
                        5 => { self.peer_settings.settings_max_frame_size = v;                  is_error |= !(1u32<<14..1u32<<24).contains(&v) },
                        6 => { self.peer_settings.settings_max_header_list_size = v },
                        8 => { self.peer_settings.settings_enable_connect_protocol = v == 1;    is_error |= v > 1 },
                        9 => { self.peer_settings.settings_no_rfc7540_priorities = v == 1;      is_error |= v > 1 },
                        other => warn!("Unknown setting_id: {}, we MUST ignore this", other),
                    };
                    if is_error {
                        error!("INVALID SETTING");
                        return self.goaway(H2Error::ProtocolError);
                    }
                }

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
                    Err(error) => {
                        error!("Could not serialize PingFrame: {:?}", error);
                        return self.force_disconnect();
                    }
                };
                self.readiness.interest.insert(Ready::WRITABLE);
                self.readiness.interest.remove(Ready::READABLE);
                self.expect_write = Some(H2StreamId::Zero);
            }
            Frame::GoAway(goaway) => {
                error!(
                    "Received GOAWAY: last_stream_id={}, error={}",
                    goaway.last_stream_id,
                    error_code_to_str(goaway.error_code)
                );
                // RFC 9113 §6.8: begin graceful drain.
                self.draining = true;
                self.peer_last_stream_id = Some(goaway.last_stream_id);

                // Streams with ID > last_stream_id were NOT processed by the peer.
                // Mark them for retry (StreamState::Link) so they can be retried
                // on a new connection.
                let mut retry_streams = Vec::new();
                for (&stream_id, &global_stream_id) in &self.streams {
                    if stream_id > goaway.last_stream_id {
                        retry_streams.push((stream_id, global_stream_id));
                    }
                }
                for (stream_id, global_stream_id) in &retry_streams {
                    let stream = &mut context.streams[*global_stream_id];
                    if let StreamState::Linked(token) = stream.state {
                        endpoint.end_stream(token, *global_stream_id, context);
                    }
                    let stream = &mut context.streams[*global_stream_id];
                    stream.state = StreamState::Link;
                    self.streams.remove(stream_id);
                }

                // If no active streams remain, close immediately
                if self.streams.is_empty() {
                    return self.goaway(H2Error::NoError);
                }

                // Otherwise, let remaining streams (ID <= last_stream_id) complete.
                // The connection will be closed when all streams finish.
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
                        error!("INVALID WINDOW INCREMENT");
                        return self.goaway(H2Error::FlowControlError);
                    }
                } else if let Some(global_stream_id) = self.streams.get(&stream_id) {
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
                    trace!(
                        "Ignoring window update on closed stream {}: {}",
                        stream_id, increment
                    );
                };
            }
            Frame::Continuation(_) => {
                error!("CONTINUATION frames are handled inline during header parsing");
                return self.goaway(H2Error::ProtocolError);
            }
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
        let mut open_window = false;
        for stream in context.streams.iter_mut() {
            // RFC 9113 §6.9.2: changes to SETTINGS_INITIAL_WINDOW_SIZE can cause
            // stream windows to exceed 2^31-1, which is a flow control error.
            match stream.window.checked_add(delta) {
                Some(new_window) => {
                    open_window |= stream.window <= 0 && new_window > 0;
                    stream.window = new_window;
                }
                None => return true,
            }
        }
        trace!(
            "UPDATE INIT WINDOW: {} {} {:?}",
            delta, open_window, self.readiness
        );
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
            Position::Client(_, _, BackendStatus::KeepAlive) => {
                error!("H2 connections do not use KeepAlive backend status");
                return;
            }
            Position::Client(..) => {}
            Position::Server => {
                trace!("H2 SENDING CLOSE NOTIFY");
                self.socket.socket_close();
                let _ = self.socket.socket_write_vectored(&[]);
                return;
            }
        }
        // reconnection is handled by the server for each stream separately
        for global_stream_id in self.streams.values() {
            trace!("end stream: {}", global_stream_id);
            if let StreamState::Linked(token) = context.streams[*global_stream_id].state {
                endpoint.end_stream(token, *global_stream_id, context);
            }
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
        trace!("reset H2 stream {}: {:#?}", stream_id, stream.context);
        let old_state = std::mem::replace(&mut stream.state, StreamState::Unlinked);
        forcefully_terminate_answer(stream, &mut self.readiness, error);
        if let StreamState::Linked(token) = old_state {
            endpoint.end_stream(token, stream_id, context);
        }
        MuxResult::Continue
    }

    pub fn end_stream<L>(&mut self, stream_gid: GlobalStreamId, context: &mut Context<L>)
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        let stream_context = &mut context.streams[stream_gid].context;
        trace!("end H2 stream {}: {:#?}", stream_gid, stream_context);
        match self.position {
            Position::Client(..) => {
                for (stream_id, global_stream_id) in &self.streams {
                    if *global_stream_id == stream_gid {
                        let id = *stream_id;
                        // Only send RST_STREAM if the stream hasn't fully completed.
                        // If both request and response are terminated, the stream is
                        // already in "closed" state (RFC 9113 §5.1) — sending RST_STREAM
                        // on a closed stream would be a protocol error that could cause
                        // the H2 peer to close the entire connection.
                        let stream = &context.streams[stream_gid];
                        let fully_completed =
                            stream.back_received_end_of_stream && stream.front.is_terminated();
                        if !fully_completed {
                            let kawa = &mut self.zero;
                            let mut frame = [0; 13];
                            if let Ok((_, _size)) =
                                serializer::gen_rst_stream(&mut frame, id, H2Error::Cancel)
                            {
                                let buf = kawa.storage.space();
                                if buf.len() >= frame.len() {
                                    buf[..frame.len()].copy_from_slice(&frame);
                                    kawa.storage.fill(frame.len());
                                    self.readiness.interest.insert(Ready::WRITABLE);
                                }
                            }
                        }
                        self.streams.remove(&id);
                        return;
                    }
                }
                error!(
                    "end_stream called for unknown global_stream_id {}",
                    stream_gid
                );
            }
            Position::Server => {
                let answers_rc = context.listener.borrow().get_answers().clone();
                let stream = &mut context.streams[stream_gid];
                match (stream.front.consumed, stream.back.is_main_phase()) {
                    (_, true) => {
                        // front might not have been consumed (in case of PushPromise)
                        // we have a "forwardable" answer from the back
                        // if the answer is not terminated we send an RstStream to properly clean the stream
                        // if it is terminated, we finish the transfer, the backend is not necessary anymore
                        if !stream.back.is_terminated() {
                            context
                                .debug
                                .push(DebugEvent::Str(format!("Close unterminated {stream_gid}")));
                            warn!("CLOSING H2 UNTERMINATED STREAM {} {:?}", stream_gid, stream);
                            forcefully_terminate_answer(
                                stream,
                                &mut self.readiness,
                                H2Error::InternalError,
                            );
                        } else {
                            context
                                .debug
                                .push(DebugEvent::Str(format!("Close terminated {stream_gid}")));
                            warn!("CLOSING H2 TERMINATED STREAM {} {:?}", stream_gid, stream);
                            stream.state = StreamState::Unlinked;
                            self.readiness.interest.insert(Ready::WRITABLE);
                        }
                        context.debug.set_interesting(true);
                    }
                    (true, false) => {
                        // we do not have an answer, but the request has already been partially consumed
                        // so we can't retry, send a 502 bad gateway instead
                        // note: it might be possible to send a RstStream with an adequate error code
                        context.debug.push(DebugEvent::Str(format!(
                            "Can't retry, send 502 on {stream_gid}"
                        )));
                        let answers = answers_rc.borrow();
                        set_default_answer(stream, &mut self.readiness, 502, &answers);
                    }
                    (false, false) => {
                        // we do not have an answer, but the request is untouched so we can retry
                        debug!("H2 RECONNECT");
                        context
                            .debug
                            .push(DebugEvent::Str(format!("Retry {stream_gid}")));
                        stream.state = StreamState::Link
                    }
                }
            }
        }
    }

    pub fn start_stream<L>(&mut self, stream: GlobalStreamId, _context: &mut Context<L>) -> bool
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        // RFC 9113 §6.8: reject new streams on a draining connection
        if self.draining {
            error!(
                "Cannot open new stream on draining connection (stream {})",
                stream
            );
            return false;
        }
        // RFC 9113 §5.1.2: respect peer's max concurrent streams limit
        if self.streams.len() >= self.peer_settings.settings_max_concurrent_streams as usize {
            error!(
                "Cannot open new stream: active={} >= peer max_concurrent_streams={}",
                self.streams.len(),
                self.peer_settings.settings_max_concurrent_streams
            );
            return false;
        }
        trace!("start new H2 stream {} {:?}", stream, self.readiness);
        let stream_id = self.new_stream_id();
        self.streams.insert(stream_id, stream);
        self.readiness.interest.insert(Ready::WRITABLE);
        true
    }
}
