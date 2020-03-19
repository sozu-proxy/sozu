use mio::*;
use mio_uds::UnixStream;
use std::str::from_utf8;
use std::collections::VecDeque;
use nom::IResult;
use serde_json::from_str;

use sozu_command::{
    channel::Channel,
    command::PROTOCOL_VERSION
};

use super::{CommandRequest,CommandResponse};

pub struct CommandClient {
  pub channel:       Channel<CommandResponse,CommandRequest>,
  pub token:         Option<Token>,
  pub queue:         VecDeque<CommandResponse>,
}

impl CommandClient {
  pub fn new(sock: UnixStream, buffer_size: usize, max_buffer_size: usize) -> CommandClient {
    let channel = Channel::new(sock, buffer_size, max_buffer_size);
    CommandClient {
      channel:         channel,
      token:           None,
      queue:           VecDeque::new(),
    }
  }

  pub fn push_message(&mut self, message: CommandResponse) {
    self.queue.push_back(message);
    self.channel.interest.insert(Ready::writable());
  }

  pub fn can_handle_events(&self) -> bool {
    self.channel.readiness().is_readable() || (!self.queue.is_empty() && self.channel.readiness().is_writable())
  }
}

pub fn parse(input: &[u8]) -> IResult<&[u8], Vec<CommandRequest>> {
  many0!(input,
    complete!(terminated!(map_res!(map_res!(is_not!("\0"), from_utf8), from_str), char!('\0')))
  )
}

