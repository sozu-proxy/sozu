//! Linux zero-copy data transfer between two TCP sockets via the
//! `splice(2)` and `pipe2(2)` syscalls.
//!
//! [`SplicePipe`] owns a pair of kernel pipes that carry data between a
//! frontend socket and a backend socket without round-tripping payload
//! through user space. Its `Drop` impl closes all four pipe fds.
//!
//! The pipe capacity matches the default Linux pipe buffer size
//! (64 KiB). Both ends are created with `O_NONBLOCK` so [`splice_in`] /
//! [`splice_out`] never block the event loop, and `O_CLOEXEC` so the
//! fds are not inherited across `exec()` boundaries (master/worker
//! hot-upgrade safety).

use std::{
    io::{Error, ErrorKind},
    os::unix::io::AsRawFd,
    ptr,
};

use crate::socket::SocketResult;

/// Default kernel-pipe capacity (64 KiB), matching the Linux default
/// for an unprivileged pipe. Sized so a single `splice(2)` call can
/// move a full pipe in one syscall.
pub const SPLICE_PIPE_CAPACITY: usize = 65_536;

/// A pair of kernel pipes used to carry zero-copy traffic between a
/// frontend socket and a backend socket.
///
/// `in_pipe` carries data from the frontend toward the backend.
/// `out_pipe` carries data from the backend toward the frontend.
///
/// Each pipe is `[read_end, write_end]`. `*_pipe_pending` tracks how
/// many bytes have been spliced into the pipe but not yet drained to
/// the destination socket — this is the "in flight in kernel" signal
/// that `Pipe::check_connections` consumes to keep half-closed sessions
/// alive while the kernel still owns the data.
pub struct SplicePipe {
    pub in_pipe: [libc::c_int; 2],
    pub out_pipe: [libc::c_int; 2],
    pub in_pipe_pending: usize,
    pub out_pipe_pending: usize,
}

impl SplicePipe {
    /// Allocate two `pipe2(O_NONBLOCK | O_CLOEXEC)` pairs. Returns
    /// `None` if either allocation fails (typically RLIMIT_NOFILE
    /// pressure); the caller falls back to the buffered path.
    pub fn new() -> Option<Self> {
        let in_pipe = create_pipe()?;
        let out_pipe = match create_pipe() {
            Some(p) => p,
            None => {
                // SAFETY: `in_pipe` was just successfully created by
                // `create_pipe`; both fds are owned by this stack frame
                // and not yet handed to anyone else. Closing them here
                // before returning `None` prevents the leak.
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
        // SAFETY: All four fds were created by `create_pipe` in
        // `SplicePipe::new`, are exclusively owned by this struct, and
        // are about to go out of scope. The worker event loop is
        // single-threaded, so no `splice(2)` call is in flight against
        // these fds when Drop runs.
        unsafe {
            libc::close(self.in_pipe[0]);
            libc::close(self.in_pipe[1]);
            libc::close(self.out_pipe[0]);
            libc::close(self.out_pipe[1]);
        }
    }
}

/// Allocate one `pipe2(O_NONBLOCK | O_CLOEXEC)` pair.
fn create_pipe() -> Option<[libc::c_int; 2]> {
    let mut fds: [libc::c_int; 2] = [0; 2];
    // SAFETY: `fds.as_mut_ptr()` is a valid, correctly-aligned writable
    // pointer to two contiguous `c_int`s, matching pipe2's
    // `int pipefd[2]` parameter. pipe2 writes the two new fds into the
    // array and does not retain the pointer after it returns.
    let ret = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) };
    if ret == 0 { Some(fds) } else { None }
}

/// Splice up to `SPLICE_PIPE_CAPACITY` bytes from `fd` into the write
/// end of a kernel pipe. Returns `(bytes_moved, status)`.
///
/// `SocketResult::WouldBlock` means the source has no more data right
/// now; `SocketResult::Closed` means the source sent EOF. The caller
/// is responsible for incrementing the matching `*_pipe_pending`.
pub fn splice_in(fd: &dyn AsRawFd, pipe_write_end: libc::c_int) -> (usize, SocketResult) {
    // SAFETY: `fd.as_raw_fd()` borrows the descriptor from its owner;
    // the `&dyn AsRawFd` keeps the owner alive for the duration of the
    // syscall. `pipe_write_end` is the write end of a pipe owned by the
    // caller (typically a `SplicePipe`). Both offset pointers are null
    // (sequential, no offset). The kernel does not retain any pointer
    // after `splice` returns.
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

/// Splice up to `len` bytes from the read end of a kernel pipe into
/// `fd`. Returns `(bytes_moved, status)`.
///
/// `len` should match the caller's `*_pipe_pending` so we never ask
/// the kernel for more bytes than the pipe contains. The caller is
/// responsible for decrementing the matching `*_pipe_pending` by
/// `bytes_moved`.
pub fn splice_out(
    pipe_read_end: libc::c_int,
    fd: &dyn AsRawFd,
    len: usize,
) -> (usize, SocketResult) {
    if len == 0 {
        return (0, SocketResult::Continue);
    }
    // SAFETY: `pipe_read_end` is the read end of a pipe owned by the
    // caller. `fd.as_raw_fd()` borrows the destination descriptor from
    // its owner; the `&dyn AsRawFd` keeps that owner alive for the
    // duration of the syscall. Both offset pointers are null
    // (sequential). The kernel does not retain any pointer after
    // `splice` returns.
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
        time::Duration,
    };

    use super::*;

    /// Round-trip a payload through a kernel pipe and assert the byte
    /// count is preserved. Exercises `create_pipe`, `splice_in`, and
    /// `splice_out` end-to-end with two real sockets.
    #[test]
    fn splice_roundtrip() {
        let proxy_listener = TcpListener::bind("127.0.0.1:0").expect("bind proxy");
        let proxy_addr = proxy_listener.local_addr().expect("local_addr");

        let pipe = create_pipe().expect("create_pipe");

        let pipe_thread = thread::spawn(move || {
            let (conn, _) = proxy_listener.accept().expect("accept");
            conn.set_read_timeout(Some(Duration::from_secs(2))).ok();

            // Pull bytes off the wire into the kernel pipe. The client
            // side may not have written yet, so retry on WouldBlock
            // with a short cap.
            let mut moved = 0usize;
            for _ in 0..50 {
                let (sz, status) = splice_in(&conn, pipe[1]);
                if sz > 0 {
                    moved = sz;
                    assert_eq!(status, SocketResult::Continue);
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            assert!(moved > 0, "splice_in moved 0 bytes");

            // Drain the pipe back into the same socket.
            let (sz_out, status_out) = splice_out(pipe[0], &conn, moved);
            assert_eq!(sz_out, moved, "splice_out byte count mismatch");
            assert_eq!(status_out, SocketResult::Continue);

            // SAFETY: pipe is locally owned and going out of scope.
            unsafe {
                libc::close(pipe[0]);
                libc::close(pipe[1]);
            }
        });

        let mut client = TcpStream::connect(proxy_addr).expect("connect");
        client.set_read_timeout(Some(Duration::from_secs(2))).ok();
        let payload = b"splice test data";
        client.write_all(payload).expect("client write");

        let mut buf = [0u8; 128];
        let n = client.read(&mut buf).expect("client read");
        assert_eq!(&buf[..n], payload);

        pipe_thread.join().expect("pipe thread");
    }
}
