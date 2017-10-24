use std::io::{self,ErrorKind,Read,Write};
use std::net::{SocketAddr,SocketAddrV4,SocketAddrV6};
use mio::tcp::{TcpListener,TcpStream};
use openssl::ssl::{Error, SslStream};
use net2::TcpBuilder;
use net2::unix::UnixTcpBuilderExt;

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
            error!("SOCKET\tbroken pipe reading from the socket");
            return (size, SocketResult::Error)
          },
          _ => {
            error!("SOCKET\tsocket_read error={:?}", e);
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
            error!("SOCKET\tbroken pipe writing to the socket");
            return (size, SocketResult::Error)
          },
          _ => {
            //FIXME: timeout and other common errors should be sent up
            error!("SOCKET\tsocket_write error={:?}", e);
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
          error!("SOCKET-TLS\treadable TLS socket err={:?}", e);
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
          error!("SOCKET-TLS\twritable TLS socket err={:?}", e);
          return (size, SocketResult::Error)
        },
        e => {
          error!("SOCKET-TLS\twritable TLS socket err={:?}", e);
          return (size, SocketResult::Error)
        }
      }
    }
  }

  fn socket_ref(&self) -> &TcpStream { self.get_ref() }
}

pub fn server_bind(addr: &SocketAddr) -> io::Result<TcpListener> {
  let sock = try!(match *addr {
    SocketAddr::V4(..) => TcpBuilder::new_v4(),
    SocketAddr::V6(..) => TcpBuilder::new_v6(),
  });

  // set so_reuseaddr, but only on unix (mirrors what libstd does)
  if cfg!(unix) {
    try!(sock.reuse_address(true));
  }

  try!(sock.reuse_port(true));

  // bind the socket
  try!(sock.bind(addr));

  // listen
  let listener = try!(sock.listen(1024));
  TcpListener::from_listener(listener, addr)
}

