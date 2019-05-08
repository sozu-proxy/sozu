use std::io::{self,ErrorKind,Read,Write};
use std::net::SocketAddr;
use std::os::unix::io::AsRawFd;
use mio::tcp::{TcpListener,TcpStream};
use rustls::{ServerSession, Session};
use net2::TcpBuilder;
use net2::unix::UnixTcpBuilderExt;
#[cfg(feature = "use-openssl")]
use openssl::ssl::{ErrorCode, SslStream};

#[derive(Debug,PartialEq,Copy,Clone)]
pub enum SocketResult {
  Continue,
  Closed,
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
          ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe => {
            return (size, SocketResult::Closed)
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
          ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe => {
            return (size, SocketResult::Closed)
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
              error!("SOCKET-TLS\treadable TLS socket SSL error: {:?}", e);
              return (size, SocketResult::Error)
            },
            ErrorCode::SYSCALL    => {
              error!("SOCKET-TLS\treadable TLS socket syscall error: {:?}", e);
              return (size, SocketResult::Error)
            },
            ErrorCode::ZERO_RETURN => {
              return (size, SocketResult::Closed)
            },
            _ => {
              error!("SOCKET-TLS\treadable TLS socket error={:?}", e);
              return (size, SocketResult::Error)
            }
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
              error!("SOCKET-TLS\twritable TLS socket SSL error: {:?}", e);
              return (size, SocketResult::Error)
            },
            ErrorCode::SYSCALL        => {
              error!("SOCKET-TLS\twritable TLS socket syscall error: {:?}", e);
              return (size, SocketResult::Error)
            },
            ErrorCode::ZERO_RETURN => {
              return (size, SocketResult::Closed)
            },
            _ => {
              error!("SOCKET-TLS\twritable TLS socket error={:?}", e);
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
    let mut size      = 0usize;
    let mut can_read  = true;
    let mut is_error  = false;
    let mut is_closed = false;

    loop {
      if size == buf.len() {
        break;
      }

      if !can_read | is_error | is_closed {
        break;
      }

      match self.session.read_tls(&mut self.stream) {
        Ok(0)  => {
          can_read  = false;
          is_closed = true;
        },
        Ok(sz) => {},
        Err(e) => match e.kind() {
          ErrorKind::WouldBlock => {
            can_read = false;
          },
          ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe => {
            is_closed = true;
          },
          _ => {
            error!("could not read TLS stream from socket: {:?}", e);
            is_error = true;
            break;
           }
        }
      }

      if let Err(e) = self.session.process_new_packets() {
        error!("could not process read TLS packets: {:?}", e);
        is_error = true;
        break;
      }

      while !self.session.wants_read() {
        match self.session.read(&mut buf[size..]) {
          Ok(sz) => size += sz,
          Err(e) => match e.kind() {
            ErrorKind::WouldBlock => {
              break;
            },
            ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe => {
              is_closed = true;
              break;
            },
            _ => {
              error!("could not read data from TLS stream: {:?}", e);
              is_error = true;
              break;
            }
          }
        }
      }
    }

    if is_error {
      (size, SocketResult::Error)
    } else if is_closed {
      (size, SocketResult::Closed)
    } else if !can_read {
      (size, SocketResult::WouldBlock)
    } else {
      (size, SocketResult::Continue)
    }
  }

  fn socket_write(&mut self,  buf: &[u8]) -> (usize, SocketResult) {
    let mut buffered_size = 0usize;
    let mut sent_size     = 0usize;
    let mut can_write     = true;
    let mut is_error      = false;
    let mut is_closed     = false;

    loop {
      if buffered_size == buf.len() {
        break;
      }

      if !can_write | is_error | is_closed {
        break;
      }

      match self.session.write(&buf[buffered_size..]) {
        Ok(0)  => {
          break;
        },
        Ok(sz) => {
          buffered_size += sz;
        },
        Err(e) => match e.kind() {
          ErrorKind::WouldBlock => {
            // we don't need to do anything, the session will return false in wants_write?
            //error!("rustls socket_write wouldblock");
          },
          ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe => {
            //FIXME: this should probably not happen here
            is_closed = true;
            break;
          },
          _ => {
            error!("could not write data to TLS stream: {:?}", e);
            is_error = true;
            break;
          }
        }
      }

      loop {
        match self.session.write_tls(&mut self.stream) {
          Ok(0)  => {
            //can_write = false;
            break;
          },
          Ok(sz) => {
            sent_size += sz;
          },
          Err(e) => match e.kind() {
            ErrorKind::WouldBlock => can_write = false,
            ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe => {
              is_closed = true;
              break;
            },
            _ => {
              error!("could not write TLS stream to socket: {:?}", e);
              is_error = true;
              break;
            }
          }
        }
      }
    }

    if is_error {
      (buffered_size, SocketResult::Error)
    } else if is_closed {
      (buffered_size, SocketResult::Closed)
    } else if !can_write {
      (buffered_size, SocketResult::WouldBlock)
    } else {
      (buffered_size, SocketResult::Continue)
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


pub fn server_unbind(listener: &TcpListener) -> io::Result<()> {
  match unsafe { libc::close(listener.as_raw_fd()) } {
    0 => Ok(()),
    _ => Err(io::Error::last_os_error())
  }
}
