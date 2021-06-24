use std::io::{self, Error, ErrorKind, Write};
use std::os::unix::io::AsRawFd;
#[cfg(target_env = "musl")]
use std::mem;
use std::net::SocketAddr;
use mio::net::UdpSocket;
use libc::{msghdr, iovec, c_void, c_uint};

pub struct MetricSocket {
  pub addr:   SocketAddr,
  pub socket: UdpSocket,
}


impl Write for MetricSocket {
  fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
    self.socket.send_to(buf, self.addr)
  }

  fn flush(&mut self) -> io::Result<()> {
    Ok(())
  }
}

#[cfg(not(target_os = "linux"))]
pub type MetricsWriter = sozu_command::writer::MultiLineWriter<MetricSocket>;

#[cfg(target_os = "linux")]
pub struct MetricsWriter {
    inner: Option<MetricSocket>,
    buf: Vec<u8>,
    last_newline: usize,
    last_packet_start: usize,
    panicked: bool,
    packet_indexes: Vec<usize>,
}

#[cfg(target_os = "linux")]
impl MetricsWriter {
    pub fn new(inner: MetricSocket) -> MetricsWriter {
        MetricsWriter::with_capacity(16384, inner)
    }

    pub fn with_capacity(capacity: usize, inner: MetricSocket) -> MetricsWriter {
        MetricsWriter {
            inner: Some(inner),
            buf: Vec::with_capacity(capacity),
            panicked: false,
            last_newline: 0,
            last_packet_start: 0,
            packet_indexes: vec![],
        }
    }

    pub fn get_ref(&self) -> &MetricSocket {
        self.inner.as_ref().unwrap()
    }

    pub fn get_mut(&mut self) -> &mut MetricSocket {
        self.inner.as_mut().unwrap()
    }

    fn flush_buf(&mut self, flush_entire_buffer: bool) -> io::Result<()> {
        let mut written = 0;
        let len = if flush_entire_buffer {
            self.buf.len()
        } else {
            self.last_newline + 1
        };

        let mut ret = Ok(());
        while written < len {
            self.panicked = true;

            let mut last_index = written;
            let iovs = self.packet_indexes.iter().map(|index| {
              let iov = iovec {
                iov_base: (self.buf.as_ptr() as usize + last_index) as *mut c_void,
                iov_len: index - last_index
              };
              /*println!("will add the packet ({} bytes): {}",
                *index -  last_index,
                std::str::from_utf8(&self.buf[last_index..*index]).unwrap());
              */
              //println!("will add the packet ({} bytes) with iov: {:?}", *index -  last_index, iov);

              last_index = *index;
              vec![iov]
            }).collect::<Vec<_>>();

            #[cfg(not(target_env = "musl"))]
            let mut messages = iovs.iter().map(|iov| {

              libc::mmsghdr {
                msg_hdr: msghdr {
                  msg_name: std::ptr::null_mut(),
                  msg_namelen: 0,
                  msg_iov: iov.as_ptr() as *mut iovec,
                  msg_iovlen: 1,
                  msg_control: std::ptr::null_mut(),
                  msg_controllen: 0,
                  msg_flags: 0,
                },
                msg_len: 0,
              }
            }).collect::<Vec<_>>();

            #[cfg(target_env = "musl")]
            let mut messages = iovs.iter().map(|iov| {
              let mhdr = {
                // Musl's msghdr has private padding fields,
                // so this is the only way to initialize it.
                let mut mhdr: msghdr = unsafe{mem::uninitialized()};
                mhdr.msg_name = std::ptr::null_mut();
                mhdr.msg_namelen = 0;
                mhdr.msg_iov = iov.as_ptr() as *mut _;
                mhdr.msg_iovlen = 1;
                mhdr.msg_control = std::ptr::null_mut();
                mhdr.msg_controllen = 0;
                mhdr.msg_flags = 0;
                mhdr
              };
              libc::mmsghdr {
                msg_hdr: mhdr,
                msg_len: 0,
              }
            }).collect::<Vec<_>>();

            //println!("created {} packets", messages.len());

            if messages.is_empty() {
              break;
            }

            unsafe {
              let r = libc::sendmmsg(
                self.inner.as_ref().unwrap().socket.as_raw_fd(),
                &mut messages[0] as *mut libc::mmsghdr,
                messages.len() as c_uint,
                0,
              );
              self.panicked = false;

              match r {
                -1 => {
                  //println!("last error: {:?}", io::Error::last_os_error());
                  let e = io::Error::last_os_error();
                  if e.kind() != io::ErrorKind::Interrupted {
                    ret = Err(e);
                    break;
                  }
                },
                sent => {
                  //println!("sent: {:?}", sent);
                  /*for i in 0..messages.len() {
                    println!("message {} wrote {} bytes from {:?}",
                      i, messages[i].msg_len,
                      *messages[i].msg_hdr.msg_iov);
                  }*/

                  let mut currently_written = 0;
                  for i in 0..(sent as usize) {
                    currently_written += messages[i].msg_len as usize;
                  }
                  written += currently_written;
                  //println!("written (packet indexes: {:?}): {}, total {}", self.packet_indexes,
                  //  currently_written, written);
                  //println!("written {}, total {}", currently_written, written);

                  if currently_written == 0 {
                    ret = Err(Error::new(
                        ErrorKind::WriteZero,
                        "failed to write the buffered data",
                    ));
                    break;
                  }

                  if sent as usize == self.packet_indexes.len() {
                    self.packet_indexes.clear();
                    break;
                  } else {
                    for _i in 0..(sent as usize) {
                      let _ = self.packet_indexes.remove(0);
                    }
                  }

                }
              }
            };
        }
        //println!("buf len {} last newline {}, written {}", self.buf.len(), self.last_newline, written);
        if written > 0 {
            //println!("FLUSHED: {}", ::std::str::from_utf8(&self.buf[..written]).unwrap());
            self.buf.drain(..written);
        }

        if flush_entire_buffer {
            self.last_newline = 0;
            self.last_packet_start = 0;
        } else if written > self.last_newline {
            self.last_newline = 0;
            //FIXME
            self.last_packet_start = 0;
        } else {
            self.last_newline -= written;
            //FIXME
            self.last_packet_start = 0;
        }

        ret
    }
}

#[cfg(target_os = "linux")]
impl Write for MetricsWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.buf.len() + buf.len() > self.buf.capacity() {
            self.flush_buf(false)?;
        }
        if buf.len() >= self.buf.capacity() {
            self.panicked = true;
            let r = self.get_mut().write(buf);
            self.panicked = false;
            r
        } else {
            if let Some(i) = memchr::memrchr(b'\n', buf) {
                let newline_index = self.buf.len() + i;

                // we limit UDP payload size to 502 bytes
                if newline_index - self.last_packet_start > 502 {
                  self.last_packet_start = self.last_newline;
                  self.packet_indexes.push(self.last_newline);
                }

                self.last_newline = newline_index;
            };

            self.buf.write(buf)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_buf(true).and_then(|()| self.get_mut().flush())
    }
}

#[cfg(target_os = "linux")]
impl Drop for MetricsWriter {
    fn drop(&mut self) {
        if self.inner.is_some() && !self.panicked {
            // dtors should not panic, so we ignore a failed flush
            let _r = self.flush_buf(true);
        }
    }
}

