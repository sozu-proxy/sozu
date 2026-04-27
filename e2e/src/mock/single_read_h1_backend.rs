//! HTTP/1.1 mock backend that writes the entire response (headers + body) in
//! a single `write_all` call and then keeps the connection alive for
//! subsequent requests — reproducing the wire shape of a Clever Cloud PaaS
//! app returning a small JSON payload. The point is to make sōzu's H1 parser
//! observe the response in ONE `socket_read` cycle that transitions kawa
//! directly from `Headers` to `Terminated`, which hits the wake-gap where
//! `mux/h1.rs:324` guards the `signal_pending_write()` call inside an
//! `if kawa.is_main_phase()` branch that is FALSE by the end of a single
//! successful parse.
//!
//! Divergence from [`AsyncBackend::http_handler`]: that handler also writes
//! the response in one `write_all`, but the callback returns after the write,
//! so the `TcpStream` is dropped and the peer observes a TCP FIN. A `FIN`
//! marks the connection non-keep-alive for sōzu's H1 reader path (via
//! `status == SocketResult::Closed`), which routes cleanup through the
//! close-delimited path in `mux/h1.rs:385-395`. The customer's real-world
//! Clever Cloud backend keeps the connection alive, so the close-delimited
//! wake never fires — which is exactly the scenario this mock reproduces.
//!
//! Divergence from [`ChunkedFlushH1Backend`]: that backend flushes between
//! headers and each body chunk and serves both `Content-Length` and
//! `Transfer-Encoding: chunked`. This one always writes in a single syscall
//! (to maximise kernel coalescing) and serves `Content-Length` only.

use std::{
    io::{Read, Write},
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crate::port_registry::bind_std_listener;

/// Configuration for [`SingleReadH1Backend::start`].
pub struct SingleReadConfig {
    /// Response body in bytes. Filled with ASCII `Z` so logs are
    /// human-inspectable.
    pub body_size: usize,
    /// Optional `Content-Type` value; defaults to `application/octet-stream`.
    pub content_type: Option<&'static str>,
}

/// Keep-alive HTTP/1.1 backend that serves `body_size` bytes of payload in a
/// single `write_all` syscall per request.
pub struct SingleReadH1Backend {
    stop: Arc<AtomicBool>,
    responses_sent: Arc<AtomicUsize>,
    thread: Option<JoinHandle<()>>,
}

impl SingleReadH1Backend {
    pub fn start(address: SocketAddr, config: SingleReadConfig) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let responses_sent = Arc::new(AtomicUsize::new(0));
        let stop_clone = stop.clone();
        let resp_count = responses_sent.clone();

        let thread = thread::spawn(move || {
            let listener = bind_std_listener(address, "single-read H1 backend");
            listener
                .set_nonblocking(true)
                .expect("could not set nonblocking");

            loop {
                if stop_clone.load(Ordering::Relaxed) {
                    break;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_nodelay(false);
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

                        let body = vec![b'Z'; config.body_size];
                        let content_type =
                            config.content_type.unwrap_or("application/octet-stream");

                        loop {
                            if stop_clone.load(Ordering::Relaxed) {
                                break;
                            }
                            let mut req_buf = [0u8; 4096];
                            match stream.read(&mut req_buf) {
                                Ok(0) => break,
                                Ok(_) => {}
                                Err(_) => break,
                            }

                            let mut payload = Vec::with_capacity(body.len() + 128);
                            payload.extend_from_slice(
                                format!(
                                    "HTTP/1.1 200 OK\r\n\
                                     Content-Type: {ct}\r\n\
                                     Content-Length: {cl}\r\n\
                                     \r\n",
                                    ct = content_type,
                                    cl = body.len()
                                )
                                .as_bytes(),
                            );
                            payload.extend_from_slice(&body);

                            if stream.write_all(&payload).is_err() {
                                break;
                            }
                            resp_count.fetch_add(1, Ordering::Relaxed);
                        }
                        drop(stream);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => {}
                }
            }
        });

        Self {
            stop,
            responses_sent,
            thread: Some(thread),
        }
    }

    pub fn responses_sent(&self) -> usize {
        self.responses_sent.load(Ordering::Relaxed)
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            thread::sleep(Duration::from_millis(100));
            drop(t);
        }
    }
}

impl Drop for SingleReadH1Backend {
    fn drop(&mut self) {
        self.stop();
    }
}
