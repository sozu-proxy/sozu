use libc::{c_int,c_uint,size_t};
use libc::types::os::arch::posix88::{off_t,ssize_t};
use mio::tcp::TcpStream;
use std::io::{Error,ErrorKind};
use std::ptr;
use std::os::unix::io::AsRawFd;

const SPLICE_F_NONBLOCK: c_uint = 2;
extern {
  //ssize_t splice(int fd_in, loff_t *off_in, int fd_out,
  //                      loff_t *off_out, size_t len, unsigned int flags);
  pub fn splice(fd_in: c_int, off_in: *const off_t, fd_out: c_int, off_out: *const off_t, len: size_t, flags: c_uint)  -> ssize_t;

  //int pipe2(int pipefd[2], int flags);
  pub fn pipe2(pipefd: *mut c_int, flags: c_int) -> c_int;
}

pub type Pipe = [c_int ; 2];

pub fn create_pipe() -> Option<Pipe> {
  let mut p: Pipe = [0; 2];
  unsafe {
    if pipe2(p.as_mut_ptr(), 0) == 0 {
      Some(p)
    } else {
      None
    }
  }
}

pub fn splice_in(stream: &AsRawFd, pipe: Pipe) -> Option<usize> {
  unsafe {
    let res = splice(stream.as_raw_fd(), ptr::null(), pipe[1], ptr::null(), 2048, SPLICE_F_NONBLOCK);
    if res == -1 {
      let err = Error::last_os_error().kind();
      if err != ErrorKind::WouldBlock {
        error!("SPLICE\terr transferring from tcp({}) to pipe({}): {:?}", stream.as_raw_fd(), pipe[1], err);
      }
      None
    } else {
      //error!("transferred {} bytes from tcp({}) to pipe({})", res, stream.as_raw_fd(), pipe[1]);
      Some(res as usize)
    }
  }
}

pub fn splice_out(pipe: Pipe, stream: &AsRawFd) -> Option<usize> {
  unsafe {
    let res = splice(pipe[0], ptr::null(), stream.as_raw_fd(), ptr::null(), 2048, SPLICE_F_NONBLOCK);
    if res == -1 {
      let err = Error::last_os_error().kind();
      if err != ErrorKind::WouldBlock {
        error!("SPLICE\terr transferring from pipe({}) to tcp({}): {:?}", pipe[0], stream.as_raw_fd(), err);
      }
      None
    } else {
      //error!("transferred {} bytes from pipe({}) to tcp({})", res, pipe[0], stream.as_raw_fd());
      Some(res as usize)
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use libc::{c_int};
  use std::net::{TcpStream,TcpListener};
  use std::thread;
  use std::io::{Read,Write,Error};
  use std::str;
  use std::os::unix::io::{AsRawFd,FromRawFd};
  use std::ptr;
  use std::net::SocketAddr;
  use std::str::FromStr;
  use std::sync::{Arc, Barrier};

  #[test]
  fn zerocopy() {
    let barrier = Arc::new(Barrier::new(2));
    start_server();
    start_server2(barrier.clone());

    let mut stream = TcpStream::connect("127.0.0.1:2121").expect("could not connect tcp socket");
    stream.write(&b"hello world"[..]);
    barrier.wait();

    let mut res = [0; 128];
    let mut sz = stream.read(&mut res[..]).expect("could not read from stream");
    println!("stream received {:?}", str::from_utf8(&res[..sz]));
    assert_eq!(&res[..sz], &b"hello world"[..]);
    //assert!(false);
  }

  fn start_server() {
    let listener = TcpListener::bind("127.0.0.1:4242").expect("could not bind socket");
    fn handle_client(stream: &mut TcpStream, id: u8) {
      let mut buf = [0; 128];
      let response = b" END";
      while let Ok(sz) = stream.read(&mut buf[..]) {
        if sz > 0 {
          println!("[{}] {:?}", id, str::from_utf8(&buf[..sz]));
          stream.write(&buf[..sz]);
        }
      }
    }

    let mut count = 0;
    thread::spawn(move|| {
      for conn in listener.incoming() {
        match conn {
          Ok(mut stream) => {
            thread::spawn(move|| {
              println!("got a new client: {}", count);
              handle_client(&mut stream, count)
            });
          }
          Err(e) => { println!("connection failed"); }
        }
        count += 1;
      }
    });
  }

  fn start_server2(barrier: Arc<Barrier>) {
    let listener = TcpListener::bind("127.0.0.1:2121").expect("could not bind socket");

    fn handle_client(stream: &mut TcpStream, backend: &mut TcpStream, id: u8, barrier: &Arc<Barrier>) {
      let mut buf = [0; 128];
      let response = b" END";
      unsafe {

        if let (Some(pipe_in), Some(pipe_out)) = (create_pipe(), create_pipe()) {
          barrier.wait();
          println!("{:?}", splice_in(stream, pipe_in));
          println!("{:?}", splice_out(pipe_in, backend));
          println!("{:?}", splice_in(backend, pipe_out));
          println!("{:?}", splice_out(pipe_out, stream));

        }
      }
    }

    let mut count = 0;
    thread::spawn(move|| {
      barrier.wait();

      for conn in listener.incoming() {
        match conn {
          Ok(mut stream) => {
            let addr: SocketAddr = FromStr::from_str("127.0.0.1:4242").expect("could not parse address");
            let mut backend  = TcpStream::connect(&addr).expect("could not create tcp stream");
            println!("got a new client: {}", count);
            handle_client(&mut stream, &mut backend, count, &barrier)
          }
          Err(e) => { println!("connection failed"); }
        }
        count += 1;
      }
    });
  }
}
