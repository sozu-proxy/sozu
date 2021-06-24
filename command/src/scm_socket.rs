use nix::sys::socket;
use nix::sys::uio;
use nix::Result as NixResult;
use nix::cmsg_space;
use std::iter::repeat;
use std::net::SocketAddr;
use std::str::from_utf8;
use std::os::unix::net;
use std::os::unix::io::{RawFd, FromRawFd, IntoRawFd};
use serde_json;
use mio::net::TcpListener;

pub const MAX_FDS_OUT: usize = 200;
pub const MAX_BYTES_OUT: usize = 4096;

#[derive(Clone,Debug,Serialize,Deserialize)]
pub struct ScmSocket {
  pub fd: RawFd,
  pub blocking: bool,
}

impl ScmSocket {
  pub fn new(fd: RawFd) -> ScmSocket {
    unsafe {
      let stream = net::UnixStream::from_raw_fd(fd);
      let _ = stream.set_nonblocking(false).map_err(|e| {
        error!("could not change blocking status for stream: {:?}", e);
      });
      let _fd = stream.into_raw_fd();
    }

    ScmSocket {
      fd,
      blocking: true,
    }
  }

  pub fn raw_fd(&self) -> i32 {
    self.fd
  }

  pub fn set_blocking(&self, blocking: bool) {
    unsafe {
      let stream = net::UnixStream::from_raw_fd(self.fd);
      let _ = stream.set_nonblocking(!blocking).map_err(|e| {
        error!("could not change blocking status for stream: {:?}", e);
      });
      let _fd = stream.into_raw_fd();
    }
  }

  pub fn send_listeners(&self, listeners: &Listeners) -> NixResult<()> {
    let listeners_count = ListenersCount {
      http: listeners.http.iter().map(|t| t.0).collect(),
      tls:  listeners.tls.iter().map(|t| t.0).collect(),
      tcp:  listeners.tcp.iter().map(|t| t.0).collect(),
    };

    let message = serde_json::to_string(&listeners_count).map(|s| s.into_bytes()).unwrap_or_else(|_|Vec::new());

    let mut v: Vec<RawFd> = Vec::new();

    v.extend(listeners.http.iter().map(|t| t.1));
    v.extend(listeners.tls.iter().map(|t| t.1));
    v.extend(listeners.tcp.iter().map(|t| t.1));

    self.send_msg(&message, &v)
  }

  pub fn receive_listeners(&self) -> Option<Listeners> {
    let mut buf = Vec::with_capacity(MAX_BYTES_OUT);
    buf.extend(repeat(0).take(MAX_BYTES_OUT));

    let mut received_fds: [RawFd; MAX_FDS_OUT] = [0; MAX_FDS_OUT];

    match self.rcv_msg(&mut buf, &mut received_fds) {
      Err(e) => {
        error!("could not receive listeners (from fd {}): {:?}", self.fd, e);
        None
      },
      Ok((sz, fds_len)) => {
        //println!("{} received :{:?}", self.fd, (sz, fds_len));
        match from_utf8(&buf[..sz]) {
          Ok(s) => match serde_json::from_str::<ListenersCount>(s) {
            Err(e) => {
              error!("could not parse listeners list (from fd {}): {:?}", self.fd, e);
              None
            },
            Ok(mut listeners_count) => {
              let mut index = 0;
              let len = listeners_count.http.len();
              let mut http = Vec::new();
              http.extend(listeners_count.http.drain(..)
                         .zip((&received_fds[index..index+len]).iter().cloned()));

              index += len;
              let len = listeners_count.tls.len();
              let mut tls = Vec::new();
              tls.extend(listeners_count.tls.drain(..)
                         .zip((&received_fds[index..index+len]).iter().cloned()));

              index += len;
              let mut tcp = Vec::new();
              tcp.extend(listeners_count.tcp.drain(..)
                         .zip((&received_fds[index..fds_len]).iter().cloned()));

              Some(Listeners { http, tls, tcp })
            }
          }
          Err(e) => {
            error!("could not parse listeners list (from fd {}): {:?}", self.fd, e);
            None
          }
        }
      }
    }
  }

  pub fn send_msg(&self, bytes: &[u8], fds: &[RawFd]) -> NixResult<()> {
    let iov = [uio::IoVec::from_slice(bytes)];
    let flags = if self.blocking {
      socket::MsgFlags::empty()
    } else {
      socket::MsgFlags::MSG_DONTWAIT
    };

    if !fds.is_empty() {
      let cmsgs = [socket::ControlMessage::ScmRights(fds)];
      //println!("{} send with data", self.fd);
      socket::sendmsg(self.fd, &iov, &cmsgs, flags, None)?;
    } else {
      //println!("{} send empty", self.fd);
      socket::sendmsg(self.fd, &iov, &[], flags, None)?;
    };
    Ok(())
  }

  pub fn rcv_msg(&self, buffer: &mut [u8], fds: &mut [RawFd]) -> NixResult<(usize, usize)> {
    let mut cmsg = cmsg_space!([RawFd; MAX_FDS_OUT]);
    let iov = [uio::IoVec::from_mut_slice(buffer)];

    let flags = if self.blocking {
      socket::MsgFlags::empty()
    } else {
      socket::MsgFlags::MSG_DONTWAIT
    };

    //let msg = socket::recvmsg(self.fd, &iov[..], Some(&mut cmsg), socket::MSG_DONTWAIT)?;
    let msg = socket::recvmsg(self.fd, &iov[..], Some(&mut cmsg), flags)?;

    let mut fd_count = 0;
    let received_fds = msg.cmsgs()
      .filter_map(|cmsg| {
        if let socket::ControlMessageOwned::ScmRights(s) = cmsg {
          Some(s)
        } else {
          None
        }
      }).flatten();
    for (fd, place) in received_fds.zip(fds.iter_mut()) {
      fd_count += 1;
      *place = fd;
    }
    Ok((msg.bytes, fd_count))
  }
}

#[derive(Clone,Debug,Serialize,Deserialize)]
pub struct Listeners {
  pub http: Vec<(SocketAddr, RawFd)>,
  pub tls:  Vec<(SocketAddr, RawFd)>,
  pub tcp:  Vec<(SocketAddr, RawFd)>,
}

#[derive(Clone,Debug,Serialize,Deserialize)]
pub struct ListenersCount {
  pub http: Vec<SocketAddr>,
  pub tls:  Vec<SocketAddr>,
  pub tcp:  Vec<SocketAddr>,
}

impl Listeners {
  pub fn get_http(&mut self, addr: &SocketAddr) -> Option<RawFd> {
    if let Some(pos) = self.http.iter().position(|(front, _)| front == addr) {
      Some(self.http.remove(pos).1)
    } else {
      None
    }
  }

  pub fn get_https(&mut self, addr: &SocketAddr) -> Option<RawFd> {
    if let Some(pos) = self.tls.iter().position(|(front, _)| front == addr) {
      Some(self.tls.remove(pos).1)
    } else {
      None
    }
  }

  pub fn get_tcp(&mut self, addr: &SocketAddr) -> Option<RawFd> {
    if let Some(pos) = self.tcp.iter().position(|(front, _)| front == addr) {
      Some(self.tcp.remove(pos).1)
    } else {
      None
    }
  }

  pub fn close(&self) {
    for (_, ref fd) in &self.http {
      unsafe {
        let _ = TcpListener::from_raw_fd(*fd);
      }
    }

    for (_, ref fd) in &self.tls {
      unsafe {
        let _ = TcpListener::from_raw_fd(*fd);
      }
    }

    for (_, ref fd) in &self.tcp {
      unsafe {
        let _ = TcpListener::from_raw_fd(*fd);
      }
    }
  }
}
