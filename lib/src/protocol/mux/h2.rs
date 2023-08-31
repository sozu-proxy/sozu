use std::{collections::HashMap, str::from_utf8_unchecked};

use rusty_ulid::Ulid;
use sozu_command::ready::Ready;

use crate::{
    protocol::mux::{
        converter,
        parser::{self, error_code_to_str, Frame, FrameHeader, FrameType},
        pkawa, serializer, Context, GlobalStreamId, MuxResult, Position, StreamId,
    },
    socket::SocketHandler,
    Readiness,
};

use super::{BackendStatus, Endpoint, GenericHttpStream};

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

pub struct ConnectionH2<Front: SocketHandler> {
    pub decoder: hpack::Decoder<'static>,
    pub encoder: hpack::Encoder<'static>,
    pub expect: Option<(H2StreamId, usize)>,
    pub position: Position,
    pub readiness: Readiness,
    pub settings: H2Settings,
    pub socket: Front,
    pub state: H2State,
    pub streams: HashMap<StreamId, GlobalStreamId>,
    pub zero: GenericHttpStream,
    pub window: u32,
}
impl<Front: SocketHandler> std::fmt::Debug for ConnectionH2<Front> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionH2")
            .field("expect", &self.expect)
            .field("position", &self.position)
            .field("readiness", &self.readiness)
            .field("settings", &self.settings)
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
    Global(GlobalStreamId),
}

impl<Front: SocketHandler> ConnectionH2<Front> {
    pub fn readable<E>(&mut self, context: &mut Context, endpoint: E) -> MuxResult
    where
        E: Endpoint,
    {
        println!("======= MUX H2 READABLE");
        let (stream_id, kawa) = if let Some((stream_id, amount)) = self.expect {
            let kawa = match stream_id {
                H2StreamId::Zero => &mut self.zero,
                H2StreamId::Global(stream_id) => context.streams[stream_id].rbuffer(&self.position),
            };
            if amount > 0 {
                let (size, status) = self.socket.socket_read(&mut kawa.storage.space()[..amount]);
                println!(
                    "{:?}({stream_id:?}, {amount}) {size} {status:?}",
                    self.state
                );
                if size > 0 {
                    kawa.storage.fill(size);
                    if size == amount {
                        self.expect = None;
                    } else {
                        self.expect = Some((stream_id, amount - size));
                        return MuxResult::Continue;
                    }
                } else {
                    // We wanted to read (amoun > 0) but there is nothing yet (size == 0)
                    self.readiness.event.remove(Ready::READABLE);
                    return MuxResult::Continue;
                }
            } else {
                self.expect = None;
            }
            (stream_id, kawa)
        } else {
            self.readiness.event.remove(Ready::READABLE);
            return MuxResult::Continue;
        };
        match (&self.state, &self.position) {
            (H2State::ClientPreface, Position::Client(_)) => {
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
                        FrameHeader {
                            payload_len,
                            frame_type: FrameType::Settings,
                            flags: 0,
                            stream_id: 0,
                        },
                    )) => {
                        kawa.storage.clear();
                        self.state = H2State::ClientSettings;
                        self.expect = Some((H2StreamId::Zero, payload_len as usize));
                    }
                    _ => todo!(),
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
                    Err(e) => panic!("{e:?}"),
                };
                self.state = H2State::ServerSettings;
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
                    Err(e) => panic!("could not serialize HeaderFrame: {e:?}"),
                };

                self.handle(settings, context, endpoint);
            }
            (H2State::ServerSettings, Position::Client(_)) => {
                let i = kawa.storage.data();

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
                        self.expect = Some((H2StreamId::Zero, payload_len as usize));
                        self.state = H2State::Frame(FrameHeader {
                            payload_len,
                            frame_type: FrameType::Settings,
                            flags: 0,
                            stream_id: 0,
                        })
                    }
                    _ => todo!(),
                };
            }
            (H2State::ServerSettings, Position::Server) => {
                error!("waiting for ServerPreface to finish writing")
            }
            (H2State::Header, Position::Server) => {
                let i = kawa.storage.data();
                println!("  header: {i:?}");
                match parser::frame_header(i) {
                    Ok((_, header)) => {
                        println!("{header:#?}");
                        kawa.storage.clear();
                        let stream_id = if header.stream_id == 0 {
                            H2StreamId::Zero
                        } else {
                            let stream_id =
                                if let Some(stream_id) = self.streams.get(&header.stream_id) {
                                    *stream_id
                                } else {
                                    self.create_stream(header.stream_id, context)
                                };
                            if header.frame_type == FrameType::Data {
                                H2StreamId::Global(stream_id)
                            } else {
                                H2StreamId::Zero
                            }
                        };
                        println!("{} {stream_id:?} {:#?}", header.stream_id, self.streams);
                        self.expect = Some((stream_id, header.payload_len as usize));
                        self.state = H2State::Frame(header);
                    }
                    Err(e) => panic!("{e:?}"),
                };
            }
            (H2State::Frame(header), _) => {
                let i = kawa.storage.data();
                println!("  data: {i:?}");
                let frame =
                    match parser::frame_body(i, header, self.settings.settings_max_frame_size) {
                        Ok((_, frame)) => frame,
                        Err(e) => panic!("{e:?}"),
                    };
                if let H2StreamId::Zero = stream_id {
                    kawa.storage.clear();
                }
                let state_result = self.handle(frame, context, endpoint);
                self.state = H2State::Header;
                self.expect = Some((H2StreamId::Zero, 9));
                return state_result;
            }
            _ => unreachable!(),
        }
        MuxResult::Continue
    }

    pub fn writable<E>(&mut self, context: &mut Context, mut endpoint: E) -> MuxResult
    where
        E: Endpoint,
    {
        println!("======= MUX H2 WRITABLE");
        match (&self.state, &self.position) {
            (H2State::ClientPreface, Position::Client(_)) => {
                let pri = serializer::H2_PRI.as_bytes();
                let kawa = &mut self.zero;

                kawa.storage.space()[0..pri.len()].copy_from_slice(pri);
                kawa.storage.fill(pri.len());

                self.state = H2State::ClientSettings;
                MuxResult::Continue
            }
            (H2State::ClientPreface, Position::Server) => unreachable!(),
            (H2State::ClientSettings, Position::Client(_)) => {
                let kawa = &mut self.zero;
                match serializer::gen_settings(kawa.storage.space(), &self.settings) {
                    Ok((_, size)) => kawa.storage.fill(size),
                    Err(e) => panic!("{e:?}"),
                };
                let (size, status) = self.socket.socket_write(kawa.storage.data());
                self.state = H2State::ServerSettings;
                self.readiness.interest.remove(Ready::WRITABLE);
                self.readiness.interest.insert(Ready::READABLE);
                kawa.storage.clear();

                self.expect = Some((H2StreamId::Zero, 9));
                MuxResult::Continue
            }
            (H2State::ClientSettings, Position::Server) => unreachable!(),
            (H2State::ServerSettings, Position::Client(_)) => unreachable!(),
            (H2State::ServerSettings, Position::Server) => {
                let kawa = &mut self.zero;
                println!("{:?}", kawa.storage.data());
                let (size, status) = self.socket.socket_write(kawa.storage.data());
                println!("  size: {size}, status: {status:?}");
                kawa.storage.clear();
                self.readiness.interest.remove(Ready::WRITABLE);
                self.readiness.interest.insert(Ready::READABLE);
                self.state = H2State::Header;
                self.expect = Some((H2StreamId::Zero, 9));
                MuxResult::Continue
            }
            (H2State::Error, _) => unreachable!(),
            // Proxying states (Header/Frame)
            (_, _) => {
                let mut converter = converter::H2BlockConverter {
                    stream_id: 0,
                    encoder: &mut self.encoder,
                    out: Vec::new(),
                };
                let mut want_write = false;
                let mut dead_streams = Vec::new();
                for (stream_id, global_stream_id) in &self.streams {
                    let stream = &mut context.streams[*global_stream_id];
                    let kawa = stream.wbuffer(&self.position);
                    if kawa.is_main_phase() {
                        converter.stream_id = *stream_id;
                        kawa.prepare(&mut converter);
                        kawa::debug_kawa(kawa);
                        while !kawa.out.is_empty() {
                            let bufs = kawa.as_io_slice();
                            let (size, status) = self.socket.socket_write_vectored(&bufs);
                            println!("  size: {size}, status: {status:?}");
                            if size > 0 {
                                kawa.consume(size);
                            } else {
                                want_write = true;
                                break;
                            }
                        }
                        if kawa.is_terminated() && kawa.is_completed() {
                            match self.position {
                                Position::Client(_) => {}
                                Position::Server => {
                                    endpoint.end_stream(
                                        stream.token.unwrap(),
                                        *global_stream_id,
                                        context,
                                    );
                                    dead_streams.push(*stream_id);
                                }
                            }
                        }
                    }
                }
                for stream_id in dead_streams {
                    self.streams.remove(&stream_id).unwrap();
                }

                let kawa = &mut self.zero;
                self.socket.socket_write(kawa.storage.data());
                kawa.storage.clear();

                if !want_write {
                    self.readiness.interest.remove(Ready::WRITABLE);
                }
                MuxResult::Continue
            }
        }
    }

    pub fn create_stream(&mut self, stream_id: StreamId, context: &mut Context) -> GlobalStreamId {
        let global_stream_id = context
            .create_stream(Ulid::generate(), self.settings.settings_initial_window_size)
            .unwrap();
        self.streams.insert(stream_id, global_stream_id);
        global_stream_id
    }

    fn handle<E>(&mut self, frame: Frame, context: &mut Context, mut endpoint: E) -> MuxResult
    where
        E: Endpoint,
    {
        println!("{frame:#?}");
        match frame {
            Frame::Data(data) => {
                let mut slice = data.payload;
                let global_stream_id = *self.streams.get(&data.stream_id).unwrap();
                let stream = &mut context.streams[global_stream_id];
                let kawa = stream.rbuffer(&self.position);
                slice.start += kawa.storage.head as u32;
                kawa.storage.head += slice.len();
                let buffer = kawa.storage.buffer();
                let payload = slice.data(buffer);
                println!("{:?}", unsafe { from_utf8_unchecked(payload) });
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
                kawa::debug_kawa(parts.rbuffer);
                match self.position {
                    Position::Client(_) => endpoint
                        .readiness_mut(stream.token.unwrap())
                        .interest
                        .insert(Ready::WRITABLE),
                    Position::Server => return MuxResult::Connect(global_stream_id),
                };
            }
            Frame::PushPromise(push_promise) => match self.position {
                Position::Client(_) => {
                    todo!("if enabled forward the push")
                }
                Position::Server => {
                    println!("A client should not push promises");
                    return MuxResult::CloseSession;
                }
            },
            Frame::Priority(priority) => (),
            Frame::RstStream(rst_stream) => {
                println!(
                    "RstStream({} -> {})",
                    rst_stream.error_code,
                    error_code_to_str(rst_stream.error_code)
                );
                // context.streams.get(priority.stream_id).close()
            }
            Frame::Settings(settings) => {
                if settings.ack {
                    return MuxResult::Continue;
                }
                for setting in settings.settings {
                    match setting.identifier {
                        1 => self.settings.settings_header_table_size = setting.value,
                        2 => self.settings.settings_enable_push = setting.value == 1,
                        3 => self.settings.settings_max_concurrent_streams = setting.value,
                        4 => self.settings.settings_initial_window_size = setting.value,
                        5 => self.settings.settings_max_frame_size = setting.value,
                        6 => self.settings.settings_max_header_list_size = setting.value,
                        other => panic!("setting_id: {other}"),
                    }
                }
                println!("{:#?}", self.settings);

                let kawa = &mut self.zero;
                kawa.storage.space()[0..serializer::SETTINGS_ACKNOWLEDGEMENT.len()]
                    .copy_from_slice(&serializer::SETTINGS_ACKNOWLEDGEMENT);
                kawa.storage
                    .fill(serializer::SETTINGS_ACKNOWLEDGEMENT.len());

                self.readiness.interest.insert(Ready::WRITABLE);
                self.readiness.interest.remove(Ready::READABLE);
            }
            Frame::Ping(_) => todo!(),
            Frame::GoAway(goaway) => {
                println!(
                    "GoAway({} -> {})",
                    goaway.error_code,
                    error_code_to_str(goaway.error_code)
                );
                return MuxResult::CloseSession;
            }
            Frame::WindowUpdate(update) => {
                if update.stream_id == 0 {
                    self.window += update.increment;
                } else {
                    let global_stream_id = *self.streams.get(&update.stream_id).unwrap();
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
            Position::Client(BackendStatus::Connecting(_)) => todo!("reconnect"),
            Position::Client(_) => {}
            Position::Server => unreachable!(),
        }
        for global_stream_id in self.streams.values() {
            endpoint.end_stream(
                context.streams[*global_stream_id].token.unwrap(),
                *global_stream_id,
                context,
            )
        }
    }

    pub fn end_stream(&mut self, stream: usize, context: &mut Context) {
        let stream_context = &mut context.streams[stream].context;
        println!("end H2 stream {stream}: {stream_context:#?}");
        for (stream_id, global_stream_id) in &self.streams {
            if *global_stream_id == stream {
                let id = *stream_id;
                self.streams.remove(&id);
                break;
            }
        }
        todo!()
    }

    pub fn start_stream(&mut self, stream: usize, context: &mut Context) {
        println!("start new H2 stream {stream}");
        todo!()
    }
}
