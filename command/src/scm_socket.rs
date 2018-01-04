use nix::sys::socket;
use nix::sys::uio;
use nix::Error;
use nix::Result as NixResult;
use std::iter::repeat;
use std::str::from_utf8;
use std::os::unix::io::RawFd;
use serde_json;

pub const MAX_FDS_OUT: usize = 200;
pub const MAX_BYTES_OUT: usize = 4096;

#[derive(Clone,Debug,Serialize,Deserialize)]
pub struct ScmSocket {
  pub fd: RawFd,
}

impl ScmSocket {
  pub fn new(fd: RawFd) -> ScmSocket {
    ScmSocket {
      fd
    }
  }

  pub fn raw_fd(&self) -> i32 {
    self.fd
  }

  pub fn send_listeners(&self, listeners: Listeners) -> NixResult<()> {
    let listeners_count = ListenersCount {
      http: listeners.http.is_some(),
      tls:  listeners.tls.is_some(),
      tcp:  listeners.tcp.len() as u32,
    };

    let message = serde_json::to_string(&listeners_count).map(|s| s.into_bytes()).unwrap_or(vec!());

    let mut v: Vec<RawFd> = Vec::new();
    if let Some(fd) = listeners.http {
      v.push(fd);
    }
    if let Some(fd) = listeners.tls {
      v.push(fd);
    }

    v.extend_from_slice(&listeners.tcp);

    self.send_msg(&message, &v)
  }

  pub fn receive_listeners(&self) -> Listeners {
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
        error!("could not receive listeners: {:?}", e);
        default
      },
      Ok((sz, fds_len)) => {
        match from_utf8(&buf[..sz]) {
          Ok(s) => match serde_json::from_str::<ListenersCount>(s) {
            Err(e) => {
              error!("could not parse listeners list: {:?}", e);
              default
            },
            Ok(listeners_count) => {
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
              tcp.extend_from_slice(&received_fds[index..fds_len]);

              Listeners { http, tls, tcp }
            }
          }
          Err(e) => {
            error!("could not parse listeners list: {:?}", e);
            default
          }
        }
      }
    }
  }

  pub fn send_msg(&self, bytes: &[u8], fds: &[RawFd]) -> NixResult<()> {
    let iov = [uio::IoVec::from_slice(bytes)];
    if fds.len() > 0 {
      let cmsgs = [socket::ControlMessage::ScmRights(fds)];
      socket::sendmsg(self.fd, &iov, &cmsgs, socket::MSG_DONTWAIT, None)?;
    } else {
      socket::sendmsg(self.fd, &iov, &[], socket::MSG_DONTWAIT, None)?;
    };
    Ok(())
  }

  pub fn rcv_msg(&self, buffer: &mut [u8], mut fds: &mut [RawFd]) -> NixResult<(usize, usize)> {
    let mut cmsg = socket::CmsgSpace::<[RawFd; MAX_FDS_OUT]>::new();
    let iov = [uio::IoVec::from_mut_slice(buffer)];

    let msg = socket::recvmsg(self.fd, &iov[..], Some(&mut cmsg), socket::MSG_DONTWAIT)?;

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
  pub tcp:  Vec<RawFd>,
}

#[derive(Clone,Debug,Serialize,Deserialize)]
pub struct ListenersCount {
  pub http: bool,
  pub tls:  bool,
  pub tcp:  u32,
}
