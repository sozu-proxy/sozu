use std::io::{ErrorKind,Read,Write};
use mio::tcp::TcpStream;
use openssl::ssl::NonblockingSslStream;
use openssl::ssl::error::{NonblockingSslError,SslError};

#[derive(Debug)]
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
            debug!("broken pipe reading from the socket");
            return (size, SocketResult::Error)
          },
          _ => {
            debug!("socket_write not implemented; client err={:?}", e);
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
            debug!("broken pipe writing to the socket");
            return (size, SocketResult::Error)
          },
          _ => {
            debug!("socket_write not implemented; client err={:?}", e);
            return (size, SocketResult::Error)
          },
        }
      }
    }
  }

  fn socket_ref(&self) -> &TcpStream { self }
}

impl SocketHandler for NonblockingSslStream<TcpStream> {
  fn socket_read(&mut self,  buf: &mut[u8]) -> (usize, SocketResult) {
    let mut size = 0usize;
    loop {
      if size == buf.len() {
        return (size, SocketResult::Continue);
      }
      match self.read(&mut buf[size..]) {
        Ok(0)  => return (size, SocketResult::Continue),
        Ok(sz) => size += sz,
        Err(NonblockingSslError::WantRead)    => return (size, SocketResult::WouldBlock),
        Err(NonblockingSslError::WantWrite)   => return (size, SocketResult::WouldBlock),
        Err(NonblockingSslError::SslError(e)) => {
          error!("readable TLS client err={:?}", e);
          return (size, SocketResult::Error)
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
        Ok(sz) => size +=sz,
        Err(NonblockingSslError::WantRead)    => return (size, SocketResult::WouldBlock),
        Err(NonblockingSslError::WantWrite)   => return (size, SocketResult::WouldBlock),
        Err(NonblockingSslError::SslError(e)) => {
          error!("readable TLS client err={:?}", e);
          return (size, SocketResult::Error)
        }
      }
    }
  }

  fn socket_ref(&self) -> &TcpStream { self.get_ref() }
}
