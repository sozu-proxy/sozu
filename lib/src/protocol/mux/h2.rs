use std::collections::HashMap;

use kawa::h1::ParserCallbacks;
use rusty_ulid::Ulid;
use sozu_command::ready::Ready;

use crate::{
    protocol::mux::{
        parser::{self, error_code_to_str, FrameHeader},
        pkawa, serializer, Context, GlobalStreamId, Position, StreamId,
    },
    socket::SocketHandler,
    Readiness,
};

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
    settings_header_table_size: u32,
    settings_enable_push: bool,
    settings_max_concurrent_streams: u32,
    settings_initial_window_size: u32,
    settings_max_frame_size: u32,
    settings_max_header_list_size: u32,
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
    // pub decoder: hpack::Decoder<'static>,
    pub expect: Option<(GlobalStreamId, usize)>,
    pub position: Position,
    pub readiness: Readiness,
    pub settings: H2Settings,
    pub socket: Front,
    pub state: H2State,
    pub streams: HashMap<StreamId, GlobalStreamId>,
}

impl<Front: SocketHandler> ConnectionH2<Front> {
    pub fn readable(&mut self, context: &mut Context) {
        println!("======= MUX H2 READABLE");
        let kawa = if let Some((stream_id, amount)) = self.expect {
            let kawa = context.streams.get(stream_id).front(self.position);
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
                    Ok((_, settings)) => {
                        kawa.storage.clear();
                        self.handle(settings, context);
                    }
                    Err(e) => panic!("{e:?}"),
                }
                let kawa = &mut context.streams.zero.back;
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
                        let stream_id = if let Some(stream_id) = self.streams.get(&header.stream_id)
                        {
                            *stream_id
                        } else {
                            self.create_stream(header.stream_id, context)
                        };
                        let stream_id = if header.frame_type == parser::FrameType::Data {
                            stream_id
                        } else {
                            0
                        };
                        println!("{} {} {:#?}", header.stream_id, stream_id, self.streams);
                        self.expect = Some((stream_id as usize, header.payload_len as usize));
                        self.state = H2State::Frame(header);
                    }
                    Err(e) => panic!("{e:?}"),
                };
            }
            (H2State::Frame(header), Position::Server) => {
                let i = kawa.storage.data();
                println!("  data: {i:?}");
                match parser::frame_body(i, header, self.settings.settings_max_frame_size) {
                    Ok((_, frame)) => {
                        kawa.storage.clear();
                        self.handle(frame, context);
                    }
                    Err(e) => panic!("{e:?}"),
                }
                self.state = H2State::Header;
                self.expect = Some((0, 9));
            }
            _ => unreachable!(),
        }
    }

    pub fn writable(&mut self, context: &mut Context) {
        println!("======= MUX H2 WRITABLE");
        match (&self.state, &self.position) {
            (H2State::ClientPreface, Position::Client) => todo!("Send PRI + client Settings"),
            (H2State::ClientPreface, Position::Server) => unreachable!(),
            (H2State::ServerSettings, Position::Client) => unreachable!(),
            (H2State::ServerSettings, Position::Server) => {
                let kawa = &mut context.streams.zero.back;
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
    }

    pub fn create_stream(&mut self, stream_id: StreamId, context: &mut Context) -> GlobalStreamId {
        match context.create_stream(Ulid::generate(), self.settings.settings_initial_window_size) {
            Ok(global_stream_id) => {
                self.streams.insert(stream_id, global_stream_id);
                global_stream_id
            }
            Err(e) => panic!("{e:?}"),
        }
    }

    fn handle(&mut self, frame: parser::Frame, context: &mut Context) {
        println!("{frame:?}");
        match frame {
            parser::Frame::Data(_) => todo!(),
            parser::Frame::Headers(headers) => {
                // if !headers.end_headers {
                //     self.state = H2State::Continuation
                // }
                let global_stream_id = self.streams.get(&headers.stream_id).unwrap();
                let kawa = context.streams.zero.front(self.position);
                let buffer = headers.header_block_fragment.data(kawa.storage.buffer());
                let stream = &mut context.streams.others[*global_stream_id - 1];
                let kawa = &mut stream.front;
                pkawa::handle_header(kawa, buffer, &mut context.decoder);
                stream.context.on_headers(kawa);
            }
            parser::Frame::Priority(priority) => (),
            parser::Frame::RstStream(_) => todo!(),
            parser::Frame::Settings(settings) => {
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
            }
            parser::Frame::PushPromise(_) => todo!(),
            parser::Frame::Ping(_) => todo!(),
            parser::Frame::GoAway(goaway) => panic!("{}", error_code_to_str(goaway.error_code)),
            parser::Frame::WindowUpdate(update) => {
                let global_stream_id = *self.streams.get(&update.stream_id).unwrap();
                context.streams.get(global_stream_id).window += update.increment as i32;
            }
            parser::Frame::Continuation(_) => todo!(),
        }
    }
}
