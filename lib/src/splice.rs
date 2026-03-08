use std::{
    io::{Error, ErrorKind},
    os::unix::io::AsRawFd,
    ptr,
};

use crate::socket::SocketResult;

/// Default capacity for splice transfers (64 KiB matches the default pipe capacity on Linux).
const SPLICE_PIPE_CAPACITY: usize = 65536;

/// A pair of kernel pipes used for zero-copy splicing between two TCP sockets.
///
/// `in_pipe` carries data from the frontend socket to the backend socket.
/// `out_pipe` carries data from the backend socket to the frontend socket.
///
/// Each pipe is a `[read_end, write_end]` pair. Pending byte counts track how
/// much data is currently sitting inside each kernel pipe buffer.
pub struct SplicePipe {
    pub in_pipe: [libc::c_int; 2],
    pub out_pipe: [libc::c_int; 2],
    pub in_pipe_pending: usize,
    pub out_pipe_pending: usize,
}

impl SplicePipe {
    pub fn new() -> Option<Self> {
        let in_pipe = create_pipe()?;
        let out_pipe = match create_pipe() {
            Some(p) => p,
            None => {
                unsafe {
                    libc::close(in_pipe[0]);
                    libc::close(in_pipe[1]);
                }
                return None;
            }
        };
        Some(SplicePipe {
            in_pipe,
            out_pipe,
            in_pipe_pending: 0,
            out_pipe_pending: 0,
        })
    }
}

impl Drop for SplicePipe {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.in_pipe[0]);
            libc::close(self.in_pipe[1]);
            libc::close(self.out_pipe[0]);
            libc::close(self.out_pipe[1]);
        }
    }
}

fn create_pipe() -> Option<[libc::c_int; 2]> {
    let mut fds: [libc::c_int; 2] = [0; 2];
    let ret = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) };
    if ret == 0 { Some(fds) } else { None }
}

/// Splice data from a socket into the write end of a kernel pipe (zero-copy read).
pub fn splice_in(fd: &dyn AsRawFd, pipe_write_end: libc::c_int) -> (usize, SocketResult) {
    let res = unsafe {
        libc::splice(
            fd.as_raw_fd(),
            ptr::null_mut(),
            pipe_write_end,
            ptr::null_mut(),
            SPLICE_PIPE_CAPACITY,
            libc::SPLICE_F_NONBLOCK | libc::SPLICE_F_MOVE,
        )
    };
    match res {
        -1 => {
            let err = Error::last_os_error();
            match err.kind() {
                ErrorKind::WouldBlock => (0, SocketResult::WouldBlock),
                _ => {
                    error!(
                        "SPLICE\terr splicing from fd({}) to pipe({}): {:?}",
                        fd.as_raw_fd(),
                        pipe_write_end,
                        err
                    );
                    (0, SocketResult::Error)
                }
            }
        }
        0 => (0, SocketResult::Closed),
        n => (n as usize, SocketResult::Continue),
    }
}

/// Splice data from the read end of a kernel pipe into a socket (zero-copy write).
///
/// `len` limits the transfer to at most that many bytes (should match the
/// pending byte count tracked by the caller).
pub fn splice_out(
    pipe_read_end: libc::c_int,
    fd: &dyn AsRawFd,
    len: usize,
) -> (usize, SocketResult) {
    if len == 0 {
        return (0, SocketResult::Continue);
    }
    let res = unsafe {
        libc::splice(
            pipe_read_end,
            ptr::null_mut(),
            fd.as_raw_fd(),
            ptr::null_mut(),
            len,
            libc::SPLICE_F_NONBLOCK | libc::SPLICE_F_MOVE,
        )
    };
    match res {
        -1 => {
            let err = Error::last_os_error();
            match err.kind() {
                ErrorKind::WouldBlock => (0, SocketResult::WouldBlock),
                _ => {
                    error!(
                        "SPLICE\terr splicing from pipe({}) to fd({}): {:?}",
                        pipe_read_end,
                        fd.as_raw_fd(),
                        err
                    );
                    (0, SocketResult::Error)
                }
            }
        }
        0 => (0, SocketResult::Closed),
        n => (n as usize, SocketResult::Continue),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        thread,
    };

    use super::*;

    #[test]
    fn splice_roundtrip() {
        // Start an echo server on an OS-assigned port
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local_addr");

        let handle = thread::spawn(move || {
            let (mut conn, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 128];
            let n = conn.read(&mut buf).expect("server read");
            conn.write_all(&buf[..n]).expect("server write");
        });

        let mut client = TcpStream::connect(addr).expect("connect");
        client.write_all(b"hello splice").expect("client write");
        // Give the server a moment to echo back
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .ok();

        // Create a pipe and splice data through it (client -> pipe -> client is not
        // meaningful in production but exercises the syscall path)
        let pipe = create_pipe().expect("create_pipe");

        // We need two connected sockets to test splice properly.
        // Use a second pair: proxy_front <-> proxy_back going through a pipe.
        let listener2 = TcpListener::bind("127.0.0.1:0").expect("bind2");
        let addr2 = listener2.local_addr().expect("local_addr2");

        let handle2 = thread::spawn(move || {
            let (conn2, _) = listener2.accept().expect("accept2");
            // splice from conn2 into pipe write end
            let (sz, res) = splice_in(&conn2, pipe[1]);
            assert!(sz > 0, "splice_in should have read bytes, got {sz}");
            assert_eq!(res, SocketResult::Continue);

            // splice from pipe read end back out
            let (sz2, res2) = splice_out(pipe[0], &conn2, sz);
            assert_eq!(sz2, sz, "splice_out should write same number of bytes");
            assert_eq!(res2, SocketResult::Continue);

            unsafe {
                libc::close(pipe[0]);
                libc::close(pipe[1]);
            }
        });

        let mut proxy_client = TcpStream::connect(addr2).expect("connect2");
        proxy_client
            .write_all(b"splice test data")
            .expect("proxy write");
        proxy_client
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .ok();

        let mut result = [0u8; 128];
        let n = proxy_client.read(&mut result).expect("proxy read");
        assert_eq!(&result[..n], b"splice test data");

        handle.join().expect("server thread");
        handle2.join().expect("splice thread");
    }
}
