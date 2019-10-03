use std::collections::{HashMap, VecDeque};
use hpack::Decoder;
use std::str::from_utf8;

use super::state::{FrameResult, OutputFrame};
use super::parser;

#[derive(Clone,Debug,PartialEq)]
pub enum St {
  Init,
  ClientPrefaceReceived,
  ServerPrefaceSent,
}

#[derive(Clone,Debug,PartialEq)]
pub struct Stream {
  pub id: u32,
  pub state: StreamState,
  pub output: VecDeque<OutputFrame>,
  pub inbound_headers: HashMap<Vec<u8>, Vec<u8>>,
}

#[derive(Clone,Copy,Debug,PartialEq)]
pub enum StreamState {
  Idle,
  ReservedLocal,
  ReservedRemote,
  Open,
  HalfClosedLocal,
  HalfClosedRemote,
  Closed,
}

impl Stream {
  pub fn new(id: u32) -> Stream {
    info!("new stream with id {}", id);

    Stream {
      id,
      state: StreamState::Idle,
      output: VecDeque::new(),
      inbound_headers: HashMap::new(),
    }
  }

  pub fn handle(&mut self, frame: &parser::Frame) -> FrameResult {
    match self.state {
      StreamState::Idle => {
        match frame {
          parser::Frame::Headers(h) => {
            let mut decoder = Decoder::new();
            match decoder.decode(h.header_block_fragment) {
              Err(e) => {
                error!("error decoding headers: {:?}", e);
                FrameResult::Close
              },
              Ok(mut h) => {
                let mut has_authority = false;
                let mut has_path = false;

                self.inbound_headers.extend(
                  h.drain(..).map(|(k, v)| {
                    if &k == b"authority" {
                      has_authority = true;
                    }

                    if &k == b"path" {
                      has_path = true;
                    }

                    info!("{} -> {}",
                      from_utf8(&k).unwrap(), from_utf8(&v).unwrap());
                    (k, v)
                  }));

                self.state = StreamState::Open;
                info!("stream[{}] state is now {:?}", self.id, self.state);
                info!("headers: {:?}", self.inbound_headers);

                if self.inbound_headers.contains_key(&b":authority"[..]) &&
                  self.inbound_headers.contains_key(&b":path"[..]) {
                  info!("will send connect_to_backend");
                  FrameResult::ConnectBackend(self.id)
                } else {
                  FrameResult::Continue
                }
              }
            }
          },
          frame => {
            panic!("unknown frame for now: {:?}", frame);
          }
        }
      },
      s => {
        unimplemented!("stream[{}] state {:?} not implemented", self.id, self.state);
      }
    }
  }
}
