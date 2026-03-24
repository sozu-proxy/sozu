#![allow(dead_code)]

use std::{
    io::{Error, ErrorKind},
    os::unix::io::AsRawFd,
    ptr,
};

use libc::{c_int, c_uint, off_t, size_t, ssize_t};

const SPLICE_F_NONBLOCK: c_uint = 2;
unsafe extern "C" {
    //ssize_t splice(int fd_in, loff_t *off_in, int fd_out,
    //                      loff_t *off_out, size_t len, unsigned int flags);
    pub fn splice(
        fd_in: c_int,
        off_in: *const off_t,
        fd_out: c_int,
        off_out: *const off_t,
        len: size_t,
        flags: c_uint,
    ) -> ssize_t;

    //int pipe2(int pipefd[2], int flags);
    pub fn pipe2(pipefd: *mut c_int, flags: c_int) -> c_int;
}

pub type Pipe = [c_int; 2];

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

pub fn splice_in(stream: &dyn AsRawFd, pipe: Pipe) -> Option<usize> {
    unsafe {
        let res = splice(
            stream.as_raw_fd(),
            ptr::null(),
            pipe[1],
            ptr::null(),
            2048,
            SPLICE_F_NONBLOCK,
        );
        if res == -1 {
            let err = Error::last_os_error().kind();
            if err != ErrorKind::WouldBlock {
                error!(
                    "SPLICE\terr transferring from tcp({}) to pipe({}): {:?}",
                    stream.as_raw_fd(),
                    pipe[1],
                    err
                );
            }
            None
        } else {
            //error!("transferred {} bytes from tcp({}) to pipe({})", res, stream.as_raw_fd(), pipe[1]);
            Some(res as usize)
        }
    }
}

pub fn splice_out(pipe: Pipe, stream: &dyn AsRawFd) -> Option<usize> {
    unsafe {
        let res = splice(
            pipe[0],
            ptr::null(),
            stream.as_raw_fd(),
            ptr::null(),
            2048,
            SPLICE_F_NONBLOCK,
        );
        if res == -1 {
            let err = Error::last_os_error().kind();
            if err != ErrorKind::WouldBlock {
                error!(
                    "SPLICE\terr transferring from pipe({}) to tcp({}): {:?}",
                    pipe[0],
                    stream.as_raw_fd(),
                    err
                );
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
    use std::{
        io::{Read, Write},
        net::{SocketAddr, TcpListener, TcpStream},
        os::unix::io::AsRawFd,
        str,
        sync::{Arc, Barrier},
        thread,
        time::Duration,
    };

    use super::*;

    /// Retry a splice_in + splice_out transfer with exponential backoff.
    /// Returns the number of bytes transferred, or panics on timeout.
    fn splice_with_retry(from: &dyn AsRawFd, pipe: Pipe, to: &dyn AsRawFd) -> usize {
        let mut delay = Duration::from_millis(1);
        let max_delay = Duration::from_millis(100);
        let mut elapsed = Duration::ZERO;
        let timeout = Duration::from_secs(5);

        let bytes_in = loop {
            if let Some(n) = splice_in(from, pipe) {
                break n;
            }
            assert!(elapsed < timeout, "splice_in timed out after {elapsed:?}");
            thread::sleep(delay);
            elapsed += delay;
            delay = (delay * 2).min(max_delay);
        };

        delay = Duration::from_millis(1);
        let mut out_elapsed = Duration::ZERO;

        let bytes_out = loop {
            if let Some(n) = splice_out(pipe, to) {
                break n;
            }
            assert!(
                out_elapsed < timeout,
                "splice_out timed out after {out_elapsed:?}"
            );
            thread::sleep(delay);
            out_elapsed += delay;
            delay = (delay * 2).min(max_delay);
        };

        println!("splice transfer: {bytes_in} bytes in, {bytes_out} bytes out");
        bytes_out
    }

    #[test]
    fn zerocopy() {
        thread::scope(|s| {
            let backend_addr = start_server(s);
            let (proxy_addr, barrier) = start_server2(s, backend_addr);

            let mut stream = TcpStream::connect(proxy_addr).expect("could not connect to proxy");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("could not set read timeout");
            stream
                .write_all(b"hello world")
                .expect("could not write to proxy");
            barrier.wait();

            let mut res = [0; 128];
            let sz = stream
                .read(&mut res[..])
                .expect("could not read from stream");
            println!("stream received {:?}", str::from_utf8(&res[..sz]));
            assert_eq!(&res[..sz], &b"hello world"[..]);
        });
    }

    fn start_server<'scope>(scope: &'scope thread::Scope<'scope, '_>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("could not bind echo server socket");
        let addr = listener
            .local_addr()
            .expect("could not get echo server address");

        scope.spawn(move || {
            // Accept a single connection — this test only needs one round-trip
            let mut stream = listener.accept().expect("echo server: accept failed").0;
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("could not set echo read timeout");
            let mut buf = [0; 128];
            loop {
                match stream.read(&mut buf[..]) {
                    Ok(0) => break, // EOF — peer closed connection
                    Ok(sz) => {
                        println!("echo: {:?}", str::from_utf8(&buf[..sz]));
                        stream.write_all(&buf[..sz]).expect("echo write failed");
                    }
                    Err(_) => break, // timeout or error — exit
                }
            }
        });

        addr
    }

    fn start_server2<'scope>(
        scope: &'scope thread::Scope<'scope, '_>,
        backend_addr: SocketAddr,
    ) -> (SocketAddr, Arc<Barrier>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("could not bind proxy socket");
        let proxy_addr = listener.local_addr().expect("could not get proxy address");
        let barrier = Arc::new(Barrier::new(2));
        let barrier_clone = barrier.to_owned();

        scope.spawn(move || {
            barrier_clone.wait();

            // Accept a single connection — this test only needs one round-trip
            let stream = listener.accept().expect("proxy: accept failed").0;
            let backend =
                TcpStream::connect(backend_addr).expect("could not connect to echo backend");
            println!("proxy: got a new client");

            if let (Some(pipe_in), Some(pipe_out)) = (create_pipe(), create_pipe()) {
                // client → backend
                splice_with_retry(&stream, pipe_in, &backend);
                // backend → client
                splice_with_retry(&backend, pipe_out, &stream);
            }
        });

        (proxy_addr, barrier)
    }
}
