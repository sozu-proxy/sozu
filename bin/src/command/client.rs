use mio::*;
use mio::timer::{Timer,Timeout};
use mio_uds::{UnixListener,UnixStream};
use slab::Slab;
use std::fs;
use std::fmt;
use std::path::PathBuf;
use std::io::{self,Read,Write,ErrorKind};
use std::str::from_utf8;
use std::os::unix::fs::PermissionsExt;
use std::collections::HashMap;
use std::sync::mpsc;
use std::cmp::min;
use std::time::Duration;
use log;
use nom::{IResult,Offset};
use serde_json;
use serde_json::from_str;

use sozu::network::{ProxyOrder,ServerMessage,ServerMessageStatus};
use sozu::network::buffer::Buffer;

use state::{HttpProxy,TlsProxy,ConfigState};

use super::ConfigMessage;

#[derive(Debug,PartialEq)]
pub enum ConnReadError {
  Continue,
  ParseError,
  SocketError,
}

pub struct CommandClient {
  pub sock:        UnixStream,
  buf:         Buffer,
  back_buf:    Buffer,
  pub token:       Option<Token>,
  message_ids: Vec<String>,
  pub write_timeout: Option<Timeout>,
  pub max_buffer_size: usize,
}

impl CommandClient {
  pub fn new(sock: UnixStream, buffer_size: usize, max_buffer_size: usize) -> CommandClient {
    CommandClient {
      sock:            sock,
      buf:             Buffer::with_capacity(buffer_size),
      back_buf:        Buffer::with_capacity(buffer_size),
      token:           None,
      message_ids:     Vec::new(),
      write_timeout:   None,
      max_buffer_size: max_buffer_size
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

  pub fn write_message(&mut self, message: &[u8]) -> bool {
    let msg_len = message.len() + 1;
    if msg_len > self.back_buf.available_space() {
      self.back_buf.shift();
    }

    if msg_len > self.back_buf.available_space() {
      if msg_len - self.back_buf.available_space() + self.back_buf.capacity() > self.max_buffer_size {
        error!("message is too large to write to back buffer");
        return false;
      }

      let new_len = msg_len - self.back_buf.available_space() + self.back_buf.capacity();
      self.back_buf.grow( new_len );
    }

    self.back_buf.write(message);
    self.back_buf.write(&b"\0"[..]);
    true
  }

  pub fn conn_readable(&mut self, tok: Token) -> Result<Vec<ConfigMessage>,ConnReadError>{
    trace!("server conn readable; tok={:?}", tok);
    loop {
      let size = self.buf.available_space();
      if size == 0 { break; }

      match self.sock.read(self.buf.space()) {
        Ok(0) => {
          //self.reregister(event_loop, tok);
          break;
          //return None;
        },
        Err(e) => {
          match e.kind() {
            ErrorKind::WouldBlock => {
              break;
            },
            code => {
              log!(log::LogLevel::Error, "UNIX CLIENT[{}] read error (kind: {:?}): {:?}", tok.0, code, e);
              return Err(ConnReadError::SocketError);
            }
          }
        },
        Ok(r) => {
          self.buf.fill(r);
          debug!("UNIX CLIENT[{}] sent {} bytes: {:?}", tok.0, r, from_utf8(self.buf.data()));
        },
      };
    }

    let mut res: Result<Vec<ConfigMessage>,ConnReadError> = Err(ConnReadError::Continue);
    let mut offset = 0usize;
    let mut grow   = false;
    match parse(self.buf.data()) {
      IResult::Incomplete(_) => {
        if self.buf.available_space() == 0 {
          log!(log::LogLevel::Error, "UNIX CLIENT[{}] buffer full, but not enough data: {:?}", tok.0, from_utf8(self.buf.data()));
          grow = true;
        } else {
          return Ok(Vec::new());
        }
      },
      IResult::Error(e)      => {
        log!(log::LogLevel::Error, "UNIX CLIENT[{}] read error: {:?}", tok.0, e);
        return Err(ConnReadError::ParseError);
      },
      IResult::Done(i, v)    => {
        if self.buf.available_data() == i.len() && v.len() == 0 {
          if self.buf.available_space() == 0 {
            log!(log::LogLevel::Error, "UNIX CLIENT[{}] buffer full, cannot parse", tok.0);
            grow = true;
          } else {
            log!(log::LogLevel::Error, "UNIX CLIENT[{}] parse error", tok.0);
            return Err(ConnReadError::ParseError);
          }
        }
        offset = self.buf.data().offset(i);
        res = Ok(v);
      }
    }

    if grow {
      let new_size = min(self.buf.capacity()+5000, self.max_buffer_size);
      self.buf.grow(new_size);
      Err(ConnReadError::Continue)

    } else {
      debug!("parsed {} bytes, result: {:?}", offset, res);
      self.buf.consume(offset);
      res
    }
  }

  pub fn conn_writable(&mut self, tok: Token) {
    if self.write_timeout.is_some() {
      trace!("server conn writable; tok={:?}; waiting for timeout", tok.0);
      return;
    }

    trace!("server conn writable; tok={:?}", tok);
    loop {
      let size = self.back_buf.available_data();
      if size == 0 { break; }

      match self.sock.write(self.back_buf.data()) {
        Ok(0) => {
          //println!("[{}] setting timeout!", tok.0);
          /*FIXME: timeout
          self.write_timeout = event_loop.timeout_ms(self.token.unwrap().0, 700).ok();
          */
          break;
        },
        Ok(r) => {
          self.back_buf.consume(r);
        },
        Err(e) => {
          match e.kind() {
            ErrorKind::WouldBlock => {
              break;
            },
            code => {
              log!(log::LogLevel::Error,"UNIX CLIENT[{}] write error: (kind: {:?}): {:?}", tok.0, code, e);
              return;
            }
          }
        }
      }
    }

    //let mut interest = Ready::hup();
    //interest.insert(Ready::readable());
    //interest.insert(Ready::writable());
    //event_loop.register(&self.sock, tok, interest, PollOpt::edge() | PollOpt::oneshot());
  }
}

pub fn parse(input: &[u8]) -> IResult<&[u8], Vec<ConfigMessage>> {
  many0!(input,
    complete!(terminated!(map_res!(map_res!(is_not!("\0"), from_utf8), from_str), char!('\0')))
  )
}

