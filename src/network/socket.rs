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
    match self.read(buf) {
      Ok(0)  => (0, SocketResult::Continue),
      Ok(sz) => (sz, SocketResult::Continue),
      Err(e) => match e.kind() {
        ErrorKind::WouldBlock => (0, SocketResult::WouldBlock),
        ErrorKind::BrokenPipe => {
          debug!("broken pipe reading from the socket");
          (0, SocketResult::Error)
        },
        _ => {
          debug!("not implemented; client err={:?}", e);
          (0, SocketResult::Error)
        },
      }
    }
  }

  fn socket_write(&mut self,  buf: &[u8]) -> (usize, SocketResult) {
    match self.write(buf) {
      Ok(0)  => (0, SocketResult::Continue),
      Ok(sz) => (sz, SocketResult::Continue),
      Err(e) => match e.kind() {
        ErrorKind::WouldBlock => (0, SocketResult::WouldBlock),
        ErrorKind::BrokenPipe => {
          debug!("broken pipe writing to the socket");
          (0, SocketResult::Error)
        },
        _ => {
          debug!("not implemented; client err={:?}", e);
          (0, SocketResult::Error)
        },
      }
    }
  }

  fn socket_ref(&self) -> &TcpStream { self }
}

impl SocketHandler for NonblockingSslStream<TcpStream> {
  fn socket_read(&mut self,  buf: &mut[u8]) -> (usize, SocketResult) {
    match self.read(buf) {
      Ok(0)  => (0, SocketResult::Continue),
      Ok(sz) => (sz, SocketResult::Continue),
      Err(NonblockingSslError::WantRead)    => (0, SocketResult::WouldBlock),
      Err(NonblockingSslError::WantWrite)   => (0, SocketResult::WouldBlock),
      Err(NonblockingSslError::SslError(e)) => {
         error!("readable TLS client err={:?}", e);
         (0, SocketResult::Error)
      }
    }
  }

  fn socket_write(&mut self,  buf: &[u8]) -> (usize, SocketResult) {
    match self.write(buf) {
      Ok(0)  => (0, SocketResult::Continue),
      Ok(sz) => (sz, SocketResult::Continue),
      Err(NonblockingSslError::WantRead)    => (0, SocketResult::WouldBlock),
      Err(NonblockingSslError::WantWrite)   => (0, SocketResult::WouldBlock),
      Err(NonblockingSslError::SslError(e)) => {
         error!("readable TLS client err={:?}", e);
         (0, SocketResult::Error)
      }
    }
  }

  fn socket_ref(&self) -> &TcpStream { self.get_ref() }
}
