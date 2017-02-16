use mio::*;
use mio::timer::Timeout;
use mio_uds::UnixStream;
use std::str::from_utf8;
use nom::IResult;
use serde_json::from_str;

use sozu::channel::Channel;

use super::{ConfigMessage,ConfigMessageAnswer};

#[derive(Debug,PartialEq)]
pub enum ConnReadError {
  Continue,
  ParseError,
  SocketError,
}

pub struct CommandClient {
  pub channel:       Channel<ConfigMessageAnswer,ConfigMessage>,
  pub token:         Option<Token>,
  message_ids:       Vec<String>,
  pub write_timeout: Option<Timeout>,
}

impl CommandClient {
  pub fn new(sock: UnixStream, buffer_size: usize, max_buffer_size: usize) -> CommandClient {
    let channel = Channel::new(sock, buffer_size, max_buffer_size);
    CommandClient {
      channel:         channel,
      token:           None,
      message_ids:     Vec::new(),
      write_timeout:   None,
    }
  }

  pub fn add_message_id(&mut self, id: String) {
    self.message_ids.push(id);
    self.message_ids.sort();
  }

  pub fn has_message_id(&self, id: &String) ->Option<usize> {
    self.message_ids.binary_search(&id).ok()
  }

  pub fn remove_message_id(&mut self, index: usize) {
    self.message_ids.remove(index);
  }

  pub fn write_message(&mut self, message: &ConfigMessageAnswer) -> bool {
    self.channel.write_message(message)
  }

}

pub fn parse(input: &[u8]) -> IResult<&[u8], Vec<ConfigMessage>> {
  many0!(input,
    complete!(terminated!(map_res!(map_res!(is_not!("\0"), from_utf8), from_str), char!('\0')))
  )
}

