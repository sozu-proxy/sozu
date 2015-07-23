use libc::{c_int,c_uint,size_t};
use libc::types::os::arch::posix88::{off_t,ssize_t};

extern {
  //ssize_t splice(int fd_in, loff_t *off_in, int fd_out,
  //                      loff_t *off_out, size_t len, unsigned int flags);
  pub fn splice(fd_in: c_int, off_in: *const off_t, fd_out: c_int, off_out: *const off_t, len: size_t, flags: c_uint)  -> ssize_t;

  //int pipe2(int pipefd[2], int flags);
  pub fn pipe2(pipefd: *mut c_int, flags: c_int) -> c_int;
}

#[cfg(test)]
mod tests {
  use super::*;
  use libc::{c_int};
  use std::net::{TcpStream,TcpListener};
  use std::thread;
  use std::io::{Read,Write,Error};
  use std::str;
  use std::os::unix::io::AsRawFd;
  use std::ptr;

  #[test]
  fn zerocopy() {
    start_server();
    start_server2();
    let mut stream = TcpStream::connect("127.0.0.1:2121").unwrap();
    stream.write(&b"hello world"[..]);
    let mut res = [0; 128];
    let mut sz = stream.read(&mut res[..]).unwrap();
    println!("stream received {:?}", str::from_utf8(&res[..sz]));
    assert_eq!(&res[..sz], &b"hello world"[..]);
    //assert!(false);
  }

  #[allow(unused_mut, unused_must_use, unused_variables)]
  fn start_server() {
    let listener = TcpListener::bind("127.0.0.1:4242").unwrap();
    fn handle_client(stream: &mut TcpStream, id: u8) {
      let mut buf = [0; 128];
      let response = b" END";
      while let Ok(sz) = stream.read(&mut buf[..]) {
        if sz > 0 {
          println!("[{}] {:?}", id, str::from_utf8(&buf[..sz]));
          stream.write(&buf[..sz]);
          //thread::sleep_ms(200);
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

  #[allow(unused_mut, unused_must_use, unused_variables)]
  fn start_server2() {
    let listener = TcpListener::bind("127.0.0.1:2121").unwrap();

    fn handle_client(stream: &mut TcpStream, backend: &mut TcpStream, id: u8) {
      let mut buf = [0; 128];
      let response = b" END";
      unsafe {

        let mut pipefd_in = [0 as c_int; 2];
        let mut pipefd_out = [0 as c_int; 2];
        let res = pipe2(pipefd_in.as_mut_ptr(), 0);
        println!("pipe2 in returned {}", res);
        let res = pipe2(pipefd_out.as_mut_ptr(), 0);
        println!("pipe2 out returned {}", res);
        thread::sleep_ms(200);
        //pub fn splice(fd_in: c_int, off_in: *mut off_t, fd_out: c_int, off_out: *mut off_t, len: size_t, flags: c_uint)  -> ssize_t;
        let mut res = splice(stream.as_raw_fd(), ptr::null(), pipefd_in[1], ptr::null(), 2048, 0);
        println!("transferred {} bytes from front({}) to pipe_in({}) | err: {:?}", stream.as_raw_fd(), pipefd_in[1],
          res, Error::last_os_error().kind());
        res = splice(pipefd_in[0], ptr::null(), backend.as_raw_fd(), ptr::null(), 2048, 0);
        println!("transferred {} bytes from pipe_in  to back", res);
        res = splice(backend.as_raw_fd(), ptr::null(), pipefd_out[1], ptr::null(), 2048, 0);
        println!("transferred {} bytes from back to pipe_out", res);
        res = splice(pipefd_out[0], ptr::null(), stream.as_raw_fd(), ptr::null(), 2048, 0);
        println!("transferred {} bytes from pipe_out to front", res);
      }
      /*while let Ok(sz) = stream.read(&mut buf[..]) {
        if sz > 0 {
          //println!("[{}] {:?}", id, str::from_utf8(&buf[..sz]));
          backend.write(&buf[..sz]);
          //thread::sleep_ms(200);
          let mut buf2 = [0; 128];
          if let Ok(sz2) = backend.read(&mut buf2[..]) {
            stream.write(&buf2[..sz2]);
          }
        }
      }*/
    }

    let mut count = 0;
    thread::spawn(move|| {
      for conn in listener.incoming() {
        match conn {
          Ok(mut stream) => {
            thread::spawn(move|| {
              let mut backend  = TcpStream::connect("127.0.0.1:4242").unwrap();
              println!("got a new client: {}", count);
              handle_client(&mut stream, &mut backend, count)
            });
          }
          Err(e) => { println!("connection failed"); }
        }
        count += 1;
      }
    });
  }
}
