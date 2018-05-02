use std::io::{self,ErrorKind,Read,Write};
use std::net::{SocketAddr,SocketAddrV4,SocketAddrV6};
use mio::tcp::{TcpListener,TcpStream};
use rustls::{ServerSession, Session};
use net2::TcpBuilder;
use net2::unix::UnixTcpBuilderExt;
#[cfg(feature = "use-openssl")]
use openssl::ssl::{Error, ErrorCode, SslStream};

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

#[cfg(feature = "use-openssl")]
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
        Err(e) => {
          match e.code() {
            ErrorCode::WANT_READ  => return (size, SocketResult::WouldBlock),
            ErrorCode::WANT_WRITE => return (size, SocketResult::WouldBlock),
            ErrorCode::SSL        => {
              error!("SOCKET-TLS\treadable TLS socket err");
              return (size, SocketResult::Error)
            },
            _ => return (size, SocketResult::Error)
          }
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
      match self.ssl_write(&buf[size..]) {
        Ok(0)  => return (size, SocketResult::Continue),
        Ok(sz) => size +=sz,
        Err(e) => {
          match e.code() {
            ErrorCode::WANT_READ  => return (size, SocketResult::WouldBlock),
            ErrorCode::WANT_WRITE => return (size, SocketResult::WouldBlock),
            ErrorCode::SSL        => {
              error!("SOCKET-TLS\twritable TLS socket err");
              return (size, SocketResult::Error)
            },
            err => {
              error!("SOCKET-TLS\twritable TLS socket err={:?}", err);
              return (size, SocketResult::Error)
            }
          }
        }
      }
    }
  }

  fn socket_ref(&self) -> &TcpStream { self.get_ref() }
}

pub struct FrontRustls {
  pub stream:  TcpStream,
  pub session: ServerSession,
}

impl SocketHandler for FrontRustls {
  fn socket_read(&mut self,  buf: &mut[u8]) -> (usize, SocketResult) {
    let mut size = 0usize;
    let mut can_read = true;
    let mut can_work = true;


    while can_work {
      if size == buf.len() {
        return (size, SocketResult::Continue);
      }

      match self.session.read_tls(&mut self.stream) {
        Ok(0)  => can_read = false,
        Ok(sz) => {},
        Err(e) => match e.kind() {
          ErrorKind::WouldBlock => {
            can_read = false;
          },
          _ => {
            error!("could not read TLS stream from socket: {:?}", e);
            return (size, SocketResult::Error)
           }
        }
      }

      if let Err(e) = self.session.process_new_packets() {
        error!("could not process read TLS packets: {:?}", e);
        return (size, SocketResult::Error);
      }

      match self.session.read(&mut buf[size..]) {
        Ok(0)  => if !can_read {
          return (size, SocketResult::Continue)
        },
        Ok(sz) => size += sz,
        Err(e) => match e.kind() {
          ErrorKind::WouldBlock => {
            if !can_read {
              return (size, SocketResult::WouldBlock);
            }
          },
           _ => {
             error!("could not read data from TLS stream: {:?}", e);
            return (size, SocketResult::Error)
           }
        }

      }

      can_work = self.session.wants_read() && can_read;
    }

    if can_read {
    (size, SocketResult::Continue)
    } else {
    (size, SocketResult::WouldBlock)
    }
  }

  fn socket_write(&mut self,  buf: &[u8]) -> (usize, SocketResult) {
    let mut sent_size = 0usize;
    let mut buffered_size = 0usize;
    let mut can_write = true;

    match self.session.write(buf) {
      Ok(0)  => {
      },
      Ok(sz) => {
        buffered_size += sz;
      },
      Err(e) => match e.kind() {
        ErrorKind::WouldBlock => {
          // we don't need to do anything, the session will return false in wants_write?
          error!("rustls socket_write wouldblock");
        },
        _ => {
          error!("could not write data to TLS stream: {:?}", e);
          return (buffered_size, SocketResult::Error)
        }
      }
    }

    while self.session.wants_write() && can_write {
      if sent_size == buf.len() {
        return (buffered_size, SocketResult::Continue);
      }

      if sent_size == buffered_size && buffered_size < buf.len() {
        match self.session.write(&buf[buffered_size..]) {
          Ok(0)  => {
          },
          Ok(sz) => {
            buffered_size += sz;
          },
          Err(e) => match e.kind() {
            ErrorKind::WouldBlock => {
              // we don't need to do anything, the session will return false in wants_write?
              error!("rustls socket_write wouldblock");
            },
             _ => {
               error!("could not write data to TLS stream: {:?}", e);
              return (buffered_size, SocketResult::Error)
             }
          }
        }

      }


      match self.session.write_tls(&mut self.stream) {
        Ok(0)  => {
          can_write = false;
        },
        Ok(sz) => {
          sent_size += sz;
        },
        Err(e) => match e.kind() {
          ErrorKind::WouldBlock => can_write = false,
          _ => {
            error!("could not write TLS stream to socket: {:?}", e);
            return (buffered_size, SocketResult::Error)
          }
        }
      }

    }

    if can_write {
      (buffered_size, SocketResult::Continue)
    } else {
      (buffered_size, SocketResult::WouldBlock)
    }
  }

  fn socket_ref(&self) -> &TcpStream { &self.stream }
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
  // FIXME: make the backlog configurable?
  let listener = try!(sock.listen(1024));
  TcpListener::from_std(listener)
}

