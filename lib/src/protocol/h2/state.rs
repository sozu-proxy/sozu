use super::{parser, serializer};
use nom::Offset;
use std::collections::{HashMap,VecDeque};
use mio::Ready;
use mio::unix::UnixReady;

use super::stream;

#[derive(Clone,Debug,PartialEq)]
pub struct OutputFrame {
  header: parser::FrameHeader,
  payload: Option<Vec<u8>>,
}

#[derive(Clone,Debug,PartialEq)]
pub enum FrameResult {
  Close,
  Continue,
  //parameter is the stream id
  ConnectBackend(u32),
}

#[derive(Clone,Debug,PartialEq)]
pub enum St {
  Init,
  ClientPrefaceReceived,
  ServerPrefaceSent,
}

#[derive(Clone,Debug,PartialEq)]
pub struct State {
  pub output: VecDeque<OutputFrame>,
  pub state: St,
  pub interest: UnixReady,
  //FIXME: make it configurable,
  pub max_frame_size: u32,
  pub streams: HashMap<u32, stream::Stream>,
}

impl State {
  pub fn new() -> State {
    State {
      output: VecDeque::new(),
      state: St::Init,
      interest: UnixReady::from(Ready::readable()) | UnixReady::hup() | UnixReady::error(),
      max_frame_size: 16384,
      streams: HashMap::new(),
    }
  }

  pub fn parse<'a>(&mut self, mut input: &'a [u8]) -> (usize, Result<parser::Frame<'a>, ()>) {
    let mut consumed = 0usize;

    if self.state == St::Init {
      match parser::preface(input) {
        Err(e) => {
          error!("parser::preface error: {:?}", e);
          return (0, Err(()));
        },
        Ok((i, _)) => {
          consumed += input.offset(i);
          self.state = St::ClientPrefaceReceived;
          input = i;
        }
      }
    }


    match parser::frame(input, self.max_frame_size) {
      Err(e) => {
        error!("parser::frame error: {:?}", e);
        return (consumed, Err(()));
      },
      Ok((i, frame)) => {
        consumed += input.offset(i);
        (consumed, Ok(frame))
      }
    }
  }

  pub fn handle(&mut self, frame: &parser::Frame) -> FrameResult {
    let stream_id = frame.stream_id();
    if stream_id != 0 {
      return self.stream_handle(stream_id, frame);
    }

    match self.state {
      St::Init => FrameResult::Continue,
      St::ClientPrefaceReceived => {
        match frame {
          parser::Frame::Settings(s) => {
            let server_settings = OutputFrame {
              header: parser::FrameHeader {
                payload_len: 0,
                frame_type: parser::FrameType::Settings,
                //FIXME: setting 1 for ACK?
                flags: 1,
                stream_id: 0,
              },
              payload: None,
            };

            self.output.push_back(server_settings);
            self.state = St::ServerPrefaceSent;
            self.interest.insert(UnixReady::from(Ready::writable()));
            FrameResult::Continue
          },
          f => {
            unimplemented!("invalid frame: {:?}, should send back an error", f);
          }
        }
      },
      St::ServerPrefaceSent => {
        match frame {
          frame => {
            panic!("unknown frame for now: {:?}", frame);
          }
        }

      }
    }
  }

  pub fn parse_and_handle<'a>(&mut self, mut input: &'a [u8]) -> (usize, FrameResult) {
    let (sz, res) = self.parse(input);
    match res {
      Err(e) => {
        error!("error parsing frame: {:?}", e);
        (sz, FrameResult::Close)
      },
      Ok(frame) => {
        info!("parsed frame: {:?}", frame);
        (sz, self.handle(&frame))
      }
    }
  }

  pub fn gen(&mut self, mut output: &mut [u8]) -> Result<usize, ()> {
    if let Some(frame) = self.output.pop_front() {
      match serializer::gen_frame_header((output, 0), &frame.header) {
        Err(e) => {
          panic!("error serializing: {:?}", e);
        },
        Ok((sl, index)) => {
          Ok(index)
        }
      }
    } else {
      self.interest.remove(Ready::writable());
      Ok(0)
    }
  }

  pub fn stream_handle(&mut self, stream_id: u32, frame: &parser::Frame) -> FrameResult {
    assert!(stream_id != 0);

    self.streams.entry(stream_id).or_insert(stream::Stream::new(stream_id)).handle(frame)
  }
}
