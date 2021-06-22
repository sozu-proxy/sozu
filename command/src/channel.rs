use mio::net::UnixStream;
use mio::event::Source;
use std::fmt::Debug;
use std::iter::Iterator;
use std::str::from_utf8;
use std::marker::PhantomData;
use std::io::{self,Read,Write,ErrorKind};
use std::os::unix::net;
use std::os::unix::io::{AsRawFd,FromRawFd,IntoRawFd,RawFd};
use std::cmp::min;
use serde_json;
use serde::ser::Serialize;
use serde::de::DeserializeOwned;

use crate::buffer::growable::Buffer;
use crate::ready::Ready;

#[derive(Debug,PartialEq)]
pub enum ConnError {
  Continue,
  ParseError,
  SocketError,
}

pub struct Channel<Tx,Rx> {
  pub sock:        UnixStream,
  front_buf:       Buffer,
  pub back_buf:    Buffer,
  max_buffer_size: usize,
  pub readiness:   Ready,
  pub interest:    Ready,
  blocking:        bool,
  phantom_tx:      PhantomData<Tx>,
  phantom_rx:      PhantomData<Rx>,
}

impl<Tx: Debug+Serialize, Rx: Debug+DeserializeOwned> Channel<Tx,Rx> {
  pub fn from_path(path: &str, buffer_size: usize, max_buffer_size: usize) -> Result<Channel<Tx,Rx>, io::Error> {
    UnixStream::connect(path).map(|stream| Channel::new(stream, buffer_size, max_buffer_size))
  }

  pub fn new(sock: UnixStream, buffer_size: usize, max_buffer_size: usize) -> Channel<Tx,Rx> {
    Channel {
      sock,
      front_buf:       Buffer::with_capacity(buffer_size),
      back_buf:        Buffer::with_capacity(buffer_size),
      max_buffer_size,
      readiness:       Ready::empty(),
      interest:        Ready::readable(),
      blocking:        false,
      phantom_tx:      PhantomData,
      phantom_rx:      PhantomData,
    }
  }

  pub fn into<Tx2: Debug+Serialize, Rx2: Debug+DeserializeOwned>(self) -> Channel<Tx2,Rx2> {
    Channel {
      sock:            self.sock,
      front_buf:       self.front_buf,
      back_buf:        self.back_buf,
      max_buffer_size: self.max_buffer_size,
      readiness:       self.readiness,
      interest:        self.interest,
      blocking:        self.blocking,
      phantom_tx:      PhantomData,
      phantom_rx:      PhantomData,
    }
  }

  pub fn set_nonblocking(&mut self, nonblocking: bool) {
    unsafe {
      let fd = self.sock.as_raw_fd();
      let stream = net::UnixStream::from_raw_fd(fd);
      let _ = stream.set_nonblocking(nonblocking).map_err(|e| {
        error!("could not change blocking status for stream: {:?}", e);
      });
      let _fd = stream.into_raw_fd();
    }
    self.blocking = !nonblocking;
  }

  pub fn set_blocking(&mut self, blocking: bool) {
    self.set_nonblocking(!blocking)
  }

  pub fn fd(&self) -> RawFd {
    self.sock.as_raw_fd()
  }

  pub fn handle_events(&mut self, events: Ready) {
    self.readiness |=  events;
  }

  pub fn readiness(&self) -> Ready {
    self.readiness & self.interest
  }

  pub fn run(&mut self) {
    let interest = self.interest & self.readiness;

    if interest.is_readable() {
      let _ = self.readable().map_err(|e| {
        error!("error reading from channel: {:?}", e);
      });
    }

    if interest.is_writable() {
      let _ = self.writable().map_err(|e| {
        error!("error writing to channel: {:?}", e);
      });
    }
  }

  pub fn readable(&mut self) -> Result<usize,ConnError> {
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
          self.interest  = Ready::empty();
          self.readiness.remove(Ready::readable());
          //error!("read() returned 0 (count={})", count);
          self.readiness.insert(Ready::hup());
          return Err(ConnError::SocketError);
        },
        Err(e) => {
          match e.kind() {
            ErrorKind::WouldBlock => {
              self.readiness.remove(Ready::readable());
              break;
            },
            _ => {
              //log!(log::LogLevel::Error, "UNIX CLIENT[{}] read error (kind: {:?}): {:?}", tok.0, code, e);
              self.interest  = Ready::empty();
              self.readiness = Ready::empty();
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

  pub fn writable(&mut self) -> Result<usize,ConnError> {
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
          //error!("write() returned 0");
          self.interest  = Ready::empty();
          self.readiness.insert(Ready::hup());
          return Err(ConnError::SocketError);
        },
        Ok(r) => {
          count += r;
          self.back_buf.consume(r);
        },
        Err(e) => {
          match e.kind() {
            ErrorKind::WouldBlock => {
              self.readiness.remove(Ready::writable());
              break;
            },
            e => {
              error!("channel write error: {:?}", e);
              self.interest  = Ready::empty();
              self.readiness = Ready::empty();
              return Err(ConnError::SocketError);
            }
          }
        }
      }
    }

    Ok(count)
  }

  pub fn read_message(&mut self) -> Option<Rx> {
    if self.blocking {
      self.read_message_blocking()
    } else {
      self.read_message_nonblocking()
    }
  }

  pub fn read_message_nonblocking(&mut self) -> Option<Rx> {
    if let Some(pos) = self.front_buf.data().iter().position(|&x| x == 0) {
      let mut res = None;

      if let Ok(s) = from_utf8(&self.front_buf.data()[..pos]) {
        match serde_json::from_str(s) {
          Ok(message) => res = Some(message),
          Err(e) => error!("could not parse message (error={:?}), ignoring:\n{}", e, s),
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

  pub fn read_message_blocking(&mut self) -> Option<Rx> {
    loop {
      if let Some(pos) = self.front_buf.data().iter().position(|&x| x == 0) {
        let mut res = None;

        if let Ok(s) = from_utf8(&self.front_buf.data()[..pos]) {
          match serde_json::from_str(s) {
            Ok(message) => res = Some(message),
            Err(e) => error!("could not parse message (error={:?}), ignoring:\n{}", e, s),
          }
        } else {
          error!("invalid utf-8 encoding in command message, ignoring");
        }

        self.front_buf.consume(pos+1);
        return res;
      } else {
        if self.front_buf.available_space() == 0 {
          if self.front_buf.capacity() == self.max_buffer_size {
            error!("command buffer full, cannot grow more, ignoring");
            return None;
          } else {
            let new_size = min(self.front_buf.capacity()+5000, self.max_buffer_size);
            self.front_buf.grow(new_size);
          }
        }

        match self.sock.read(self.front_buf.space()) {
          Ok(0) => {
              return None;
          },
          Err(_) => { return None; },
          Ok(r) => {
            self.front_buf.fill(r);
          },
        };
      }
    }
  }

  pub fn write_message(&mut self, message: &Tx) -> bool {
    if self.blocking {
      self.write_message_blocking(message)
    } else {
      self.write_message_nonblocking(message)
    }
  }

  pub fn write_message_nonblocking(&mut self, message: &Tx) -> bool {
    let message = &serde_json::to_string(message).map(|s| s.into_bytes()).unwrap_or_else(|_| Vec::new());

    let msg_len = message.len() + 1;
    if msg_len > self.back_buf.available_space() {
      self.back_buf.shift();
    }

    if msg_len > self.back_buf.available_space() {
      if msg_len - self.back_buf.available_space() + self.back_buf.capacity() > self.max_buffer_size {
        error!("message is too large to write to back buffer. Consider increasing proxy channel buffer size, current value is {}", self.back_buf.capacity());
        return false;
      }

      let new_len = msg_len - self.back_buf.available_space() + self.back_buf.capacity();
      self.back_buf.grow( new_len );
    }

    if let Err(e) = self.back_buf.write(message) {
      error!("channel could not write to back buffer: {:?}", e);
      return false;
    }

    if let Err(e) = self.back_buf.write(&b"\0"[..]) {
      error!("channel could not write to back buffer: {:?}", e);
      return false;
    }

    self.interest.insert(Ready::writable());

    true
  }

  pub fn write_message_blocking(&mut self, message: &Tx) -> bool {
    let message = &serde_json::to_string(message).map(|s| s.into_bytes()).unwrap_or_else(|_| Vec::new());

    let msg_len = message.len() + 1;
    if msg_len > self.back_buf.available_space() {
      self.back_buf.shift();
    }

    if msg_len > self.back_buf.available_space() {
      if msg_len - self.back_buf.available_space() + self.back_buf.capacity() > self.max_buffer_size {
        error!("message is too large to write to back buffer. Consider increasing proxy channel buffer size, current value is {}", self.back_buf.capacity());
        return false;
      }

      let new_len = msg_len - self.back_buf.available_space() + self.back_buf.capacity();
      self.back_buf.grow( new_len );
    }

    if let Err(e) = self.back_buf.write(message) {
      error!("channel could not write to back buffer: {:?}", e);
      return false;
    }

    if let Err(e) = self.back_buf.write(&b"\0"[..]) {
      error!("channel could not write to back buffer: {:?}", e);
      return false;
    }

    loop {
      let size = self.back_buf.available_data();
      if size == 0 {
        break;
      }

      match self.sock.write(self.back_buf.data()) {
        Ok(0) => {
            return false;
        },
        Ok(r) => {
          self.back_buf.consume(r);
        },
        Err(_) => {
          return true;
        }
      }
    }
    true
  }
}

impl<Tx: Debug+DeserializeOwned+Serialize, Rx: Debug+DeserializeOwned+Serialize> Channel<Tx,Rx> {
  pub fn generate(buffer_size: usize, max_buffer_size: usize) -> io::Result<(Channel<Tx,Rx>, Channel<Rx,Tx>)> {
    let     (command,proxy) = UnixStream::pair()?;
    let     proxy_channel   = Channel::new(proxy, buffer_size, max_buffer_size);
    let mut command_channel = Channel::new(command, buffer_size, max_buffer_size);
    command_channel.set_nonblocking(false);
    Ok((command_channel, proxy_channel))
  }

  pub fn generate_nonblocking(buffer_size: usize, max_buffer_size: usize) -> io::Result<(Channel<Tx,Rx>, Channel<Rx,Tx>)> {
    let (command,proxy) = UnixStream::pair()?;
    let proxy_channel   = Channel::new(proxy, buffer_size, max_buffer_size);
    let command_channel = Channel::new(command, buffer_size, max_buffer_size);
    Ok((command_channel, proxy_channel))
  }
}

impl<Tx: Debug+Serialize, Rx: Debug+DeserializeOwned> Iterator for Channel<Tx,Rx> {
  type Item = Rx;
  fn next(&mut self) -> Option<Self::Item> {
    self.read_message()
  }
}

use mio::{Interest, Registry, Token};
impl<Tx,Rx> Source for Channel<Tx,Rx> {
  fn register(&mut self,
              registry: &Registry,
              token: Token,
              interests: Interest)
              -> io::Result<()> {
    self.sock.register(registry, token, interests)
  }

  fn reregister(&mut self,
                registry: &Registry,
                token: Token,
                interests: Interest)
                -> io::Result<()> {
    self.sock.reregister(registry, token, interests)
  }

  fn deregister(&mut self, registry: &Registry) -> io::Result<()> {
    self.sock.deregister(registry)
  }
}
