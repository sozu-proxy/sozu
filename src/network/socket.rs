use std::io::{ErrorKind,Read,Write};
use mio::tcp::TcpStream;
use openssl::ssl::SslStream;
use openssl::ssl::error::Error;

#[derive(Debug,PartialEq,Copy,Clone)]
pub enum SocketResult {
  Continue,
  WouldBlock,
  Error
}

pub trait SocketHandler {
  fn socket_read(&mut self,  buf: &mut[u8]) -> (usize, SocketResult);
  fn socket_write(&mut self, buf: &[u8])    -> (usize, SocketResult);
  fn socket_ref(&self) -> &TcpStream;
}

impl SocketHandler for TcpStream {
  fn socket_read(&mut self,  buf: &mut[u8]) -> (usize, SocketResult) {
    let mut size = 0usize;
    loop {
      if size == buf.len() {
        return (size, SocketResult::Continue);
      }
      match self.read(&mut buf[size..]) {
        Ok(0)  => return (size, SocketResult::Continue),
        Ok(sz) => size +=sz,
        Err(e) => match e.kind() {
          ErrorKind::WouldBlock => return (size, SocketResult::WouldBlock),
          ErrorKind::BrokenPipe => {
            debug!("SOCKET\tbroken pipe reading from the socket");
            return (size, SocketResult::Error)
          },
          _ => {
            debug!("SOCKET\tsocket_write not implemented; client err={:?}", e);
            return (size, SocketResult::Error)
          },
        }
      }
    }
  }

  fn socket_write(&mut self,  buf: &[u8]) -> (usize, SocketResult) {
    let mut size = 0usize;
    loop {
      if size == buf.len() {
        return (size, SocketResult::Continue);
      }
      match self.write(&buf[size..]) {
        Ok(0)  => return (size, SocketResult::Continue),
        Ok(sz) => size += sz,
        Err(e) => match e.kind() {
          ErrorKind::WouldBlock => return (size, SocketResult::WouldBlock),
          ErrorKind::BrokenPipe => {
            debug!("SOCKET\tbroken pipe writing to the socket");
            return (size, SocketResult::Error)
          },
          _ => {
            debug!("SOCKET\tsocket_write not implemented; client err={:?}", e);
            return (size, SocketResult::Error)
          },
        }
      }
    }
  }

  fn socket_ref(&self) -> &TcpStream { self }
}

impl SocketHandler for SslStream<TcpStream> {
  fn socket_read(&mut self,  buf: &mut[u8]) -> (usize, SocketResult) {
    let mut size = 0usize;
    loop {
      if size == buf.len() {
        return (size, SocketResult::Continue);
      }
      match self.ssl_read(&mut buf[size..]) {
        Ok(0)  => return (size, SocketResult::Continue),
        Ok(sz) => size += sz,
        Err(Error::WantRead(_))  => return (size, SocketResult::WouldBlock),
        Err(Error::WantWrite(_)) => return (size, SocketResult::WouldBlock),
        Err(Error::Stream(e))    => {
          error!("SOCKET-TLS\treadable TLS client err={:?}", e);
          return (size, SocketResult::Error)
        },
        _ => return (size, SocketResult::Error)
      }
    }
  }

  fn socket_write(&mut self,  buf: &[u8]) -> (usize, SocketResult) {
    let mut size = 0usize;
    loop {
      if size == buf.len() {
        return (size, SocketResult::Continue);
      }
      match self.ssl_write(&buf[size..]) {
        Ok(0)  => return (size, SocketResult::Continue),
        Ok(sz) => size +=sz,
        Err(Error::WantRead(_))  => return (size, SocketResult::WouldBlock),
        Err(Error::WantWrite(_)) => return (size, SocketResult::WouldBlock),
        Err(Error::Stream(e))    => {
          error!("SOCKET-TLS\treadable TLS client err={:?}", e);
          return (size, SocketResult::Error)
        },
        _ => return (size, SocketResult::Error)
      }
    }
  }

  fn socket_ref(&self) -> &TcpStream { self.get_ref() }
}
