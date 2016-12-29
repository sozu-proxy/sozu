use mio::Ready;
use mio_uds::UnixStream;
use std::str::from_utf8;
use std::marker::PhantomData;
use std::io::{self,Read,Write,ErrorKind};
use std::cmp::min;
use log;
use serde_json;
use serde::ser::Serialize;
use serde::de::Deserialize;

use network::buffer::Buffer;

#[derive(Debug,PartialEq)]
pub enum ConnError {
  Continue,
  ParseError,
  SocketError,
}

pub struct CommandClient<Tx,Rx> {
  pub sock:        UnixStream,
  front_buf:       Buffer,
  back_buf:        Buffer,
  max_buffer_size: usize,
  pub readiness:   Ready,
  pub interest:    Ready,
  phantom_tx:      PhantomData<Tx>,
  phantom_rx:      PhantomData<Rx>,
}

impl<Tx: Serialize, Rx: Deserialize> CommandClient<Tx,Rx> {
  pub fn new(sock: UnixStream, buffer_size: usize, max_buffer_size: usize) -> CommandClient<Tx,Rx> {
    CommandClient {
      sock:            sock,
      front_buf:       Buffer::with_capacity(buffer_size),
      back_buf:        Buffer::with_capacity(buffer_size),
      max_buffer_size: max_buffer_size,
      readiness:       Ready::none(),
      interest:        Ready::readable(),
      phantom_tx:      PhantomData,
      phantom_rx:      PhantomData,
    }
  }

  fn handle_events(&mut self, events: Ready) {
    self.readiness = self.readiness | events;
  }

  fn run(&mut self) {
    let interest = self.interest & self.readiness;

    if interest.is_readable() {
      self.readable();
    }

    if interest.is_writable() {
      self.writable();
    }
  }

  fn readable(&mut self) -> Result<usize,ConnError> {
    if !(self.interest & self.readiness).is_readable() {
      return Err(ConnError::Continue);
    }

    let mut count = 0usize;
    loop {
      let size = self.front_buf.available_space();
      if size == 0 {
        self.interest.remove(Ready::readable());
        break;
      }

      match self.sock.read(self.front_buf.space()) {
        Ok(0) => {
          break;
        },
        Err(e) => {
          match e.kind() {
            ErrorKind::WouldBlock => {
              self.readiness.remove(Ready::readable());
              break;
            },
            code => {
              //log!(log::LogLevel::Error, "UNIX CLIENT[{}] read error (kind: {:?}): {:?}", tok.0, code, e);
              self.interest  = Ready::none();
              self.readiness = Ready::none();
              return Err(ConnError::SocketError);
            }
          }
        },
        Ok(r) => {
          count += r;
          self.front_buf.fill(r);
          //debug!("UNIX CLIENT[{}] sent {} bytes: {:?}", tok.0, r, from_utf8(self.buf.data()));
        },
      };
    }

    Ok(count)
  }

  fn writable(&mut self) -> Result<usize,ConnError> {
    if !(self.interest & self.readiness).is_writable() {
      return Err(ConnError::Continue);
    }

    let mut count = 0usize;
    loop {
      let size = self.back_buf.available_data();
      if size == 0 {
        self.interest.remove(Ready::writable());
        break;
      }

      match self.sock.write(self.back_buf.data()) {
        Ok(0) => {
          break;
        },
        Ok(r) => {
          count += r;
          self.back_buf.consume(r);
        },
        Err(e) => {
          match e.kind() {
            ErrorKind::WouldBlock => {
              self.interest.remove(Ready::writable());
              break;
            },
            code => {
              self.interest  = Ready::none();
              self.readiness = Ready::none();
              return Err(ConnError::SocketError);
            }
          }
        }
      }
    }

    Ok(count)
  }

  fn read_message(&mut self) -> Option<Rx> {
    let mut offset = 0usize;
    let mut grow   = false;

    if let Some(pos) = self.front_buf.data().iter().position(|&x| x == 0) {
      let mut res = None;

      if let Ok(s) = from_utf8(&self.front_buf.data()[..pos]) {
        if let Ok(message) = serde_json::from_str(s) {
          res = Some(message);
        } else {
          error!("could not parse message, ignoring:\n{}", s);
        }
      } else {
        error!("invalid utf-8 encoding in command message, ignoring");
      }

      self.front_buf.consume(pos+1);
      res
    } else {
      if self.front_buf.available_space() == 0 {
        if self.front_buf.capacity() == self.max_buffer_size {
          error!("command buffer full, cannot grow more, ignoring");
        } else {
          let new_size = min(self.front_buf.capacity()+5000, self.max_buffer_size);
          self.front_buf.grow(new_size);
        }
      }

      self.interest.insert(Ready::readable());
      None
    }
  }

  pub fn write_message(&mut self, message: Tx) -> bool {
    let message = &serde_json::to_string(&message).map(|s| s.into_bytes()).unwrap_or(vec!());

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
}

