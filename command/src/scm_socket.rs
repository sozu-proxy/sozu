use nix::sys::socket;
use nix::sys::uio;
use nix::Result as NixResult;
use std::iter::repeat;
use std::str::from_utf8;
use std::os::unix::net;
use std::os::unix::io::{RawFd, FromRawFd, IntoRawFd};
use serde_json;

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

  pub fn send_listeners(&self, listeners: Listeners) -> NixResult<()> {
    let listeners_count = ListenersCount {
      http: listeners.http.is_some(),
      tls:  listeners.tls.is_some(),
      tcp:  listeners.tcp.iter().map(|t| t.0.clone()).collect(),
    };

    let message = serde_json::to_string(&listeners_count).map(|s| s.into_bytes()).unwrap_or(vec!());

    let mut v: Vec<RawFd> = Vec::new();
    if let Some(fd) = listeners.http {
      v.push(fd);
    }
    if let Some(fd) = listeners.tls {
      v.push(fd);
    }

    v.extend(listeners.tcp.iter().map(|t| t.1));

    self.send_msg(&message, &v)
  }

  pub fn receive_listeners(&self) -> Option<Listeners> {
    let mut buf = Vec::with_capacity(MAX_BYTES_OUT);
    buf.extend(repeat(0).take(MAX_BYTES_OUT));

    let mut received_fds: [RawFd; MAX_FDS_OUT] = [0; MAX_FDS_OUT];
    let default = Listeners {
      http: None,
      tls:  None,
      tcp:  Vec::new()
    };

    match self.rcv_msg(&mut buf, &mut received_fds) {
      Err(e) => {
        error!("{} could not receive listeners: {:?}", self.fd, e);
        None
      },
      Ok((sz, fds_len)) => {
        println!("{} received :{:?}", self.fd, (sz, fds_len));
        match from_utf8(&buf[..sz]) {
          Ok(s) => match serde_json::from_str::<ListenersCount>(s) {
            Err(e) => {
              error!("{} could not parse listeners list: {:?}", self.fd, e);
              None
            },
            Ok(mut listeners_count) => {
              let mut index = 0;
              let http = if listeners_count.http {
                index = 1;
                Some(received_fds[0])
              } else {
                None
              };

              let tls = if listeners_count.tls {
                let fd = received_fds[index];
                index += 1;
                Some(fd)
              } else {
                None
              };

              let mut tcp = Vec::new();
              tcp.extend(listeners_count.tcp.drain(..)
                         .zip((&received_fds[index..fds_len]).iter().cloned()));

              Some(Listeners { http, tls, tcp })
            }
          }
          Err(e) => {
            error!("{} could not parse listeners list: {:?}", self.fd, e);
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
      socket::MSG_DONTWAIT
    };

    if fds.len() > 0 {
      let cmsgs = [socket::ControlMessage::ScmRights(fds)];
      //println!("{} send with data", self.fd);
      socket::sendmsg(self.fd, &iov, &cmsgs, flags, None)?;
    } else {
      println!("{} send empty", self.fd);
      socket::sendmsg(self.fd, &iov, &[], flags, None)?;
    };
    Ok(())
  }

  pub fn rcv_msg(&self, buffer: &mut [u8], fds: &mut [RawFd]) -> NixResult<(usize, usize)> {
    let mut cmsg = socket::CmsgSpace::<[RawFd; MAX_FDS_OUT]>::new();
    let iov = [uio::IoVec::from_mut_slice(buffer)];

    let flags = if self.blocking {
      socket::MsgFlags::empty()
    } else {
      socket::MSG_DONTWAIT
    };

    //let msg = socket::recvmsg(self.fd, &iov[..], Some(&mut cmsg), socket::MSG_DONTWAIT)?;
    let msg = socket::recvmsg(self.fd, &iov[..], Some(&mut cmsg), flags)?;

    let mut fd_count = 0;
    let received_fds = msg.cmsgs()
      .flat_map(|cmsg|
                match cmsg {
                  socket::ControlMessage::ScmRights(s) => s,
                  _ => &[]
                }.iter()
               );
    for (fd, place) in received_fds.zip(fds.iter_mut()) {
      fd_count += 1;
      *place = *fd;
    }
    Ok((msg.bytes, fd_count))
  }
}

#[derive(Clone,Debug,Serialize,Deserialize)]
pub struct Listeners {
  pub http: Option<RawFd>,
  pub tls:  Option<RawFd>,
  //app_id, fd
  pub tcp:  Vec<(String, RawFd)>,
}

#[derive(Clone,Debug,Serialize,Deserialize)]
pub struct ListenersCount {
  pub http: bool,
  pub tls:  bool,
  pub tcp:  Vec<String>,
}
