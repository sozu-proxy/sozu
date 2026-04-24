//! HTTP/1.1 mock backend that emits the response body in small, explicitly
//! flushed segments — reproducing the wire cadence of PHP/Apache responses
//! (`mod_deflate`, `flush()`, `SendfileMaxPipeSize`). Used by the H2
//! large-asset repro suite to characterise the cross-connection edge-triggered
//! `WRITABLE` wake-gap (see `lib/src/protocol/mux/h1.rs:341-346, 351-357` and
//! memory entry `project_sozu_h1_missing_signal_pending_write.md`).
//!
//! Divergence from [`CloseDelimitedBackend`] in `h2_tests.rs:3237-3327`:
//! that backend batches all writes (no `flush()` between chunks, no
//! `TCP_NODELAY`) because its role is to probe HUP regression, not write
//! fragmentation. This backend is the opposite: `TCP_NODELAY` + per-chunk
//! `flush()` + `thread::sleep(inter_chunk_delay)` explicitly forces distinct
//! TCP segments so sozu observes multiple `readable()` passes from the
//! backend, each one repeating the `interest.insert(Ready::WRITABLE)`
//! pattern on the frontend endpoint. Without the sleep, the kernel may
//! coalesce the writes despite `TCP_NODELAY`; the sleep is what guarantees
//! a segment is emitted before the next write queues.
//!
//! This backend stays alive across requests (HTTP/1.1 keep-alive by default
//! on the backend path) until [`ChunkedFlushH1Backend::stop`] is called, so
//! the response framing is driven by `Content-Length` / `Transfer-Encoding:
//! chunked`, not by EOF. The [`ChunkedFlushConfig::truncate_at_byte`] knob
//! is the sole exception: when set, the backend drops the TCP connection
//! partway through the body (before the `0\r\n\r\n` terminator on chunked
//! responses) so the H2 side must emit RST_STREAM rather than a silent
//! END_STREAM.
//!
//! The response body and headers can be overridden for customer-shape
//! reproductions:
//!
//! * [`ChunkedFlushConfig::body`] — `Some(Arc<Vec<u8>>)` switches the chunk
//!   source from `b'Z'` fill to a caller-supplied buffer served verbatim
//!   (cursor advanced by [`ChunkedFlushConfig::chunk_size`] per iteration).
//!   The buffer length wins over [`ChunkedFlushConfig::body_size`] when set.
//! * [`ChunkedFlushConfig::extra_response_headers`] — additional response
//!   header lines appended after `Content-Type` / framing header (e.g.
//!   `"Content-Encoding: gzip"`).
//! * [`ChunkedFlushConfig::content_type`] — overrides the default
//!   `application/octet-stream`.
//!
//! The customer-shape repro suite (`test_h2_large_gzipped_chunked_drains_fully`
//! in `e2e/src/tests/h2_correctness_tests.rs`) uses all three together to
//! serve a gzipped deterministic payload whose sha256 the test asserts.

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

/// HTTP/1.1 transfer-encoding used by the mock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferEncoding {
    /// Response advertises `Content-Length: N`; the backend writes exactly
    /// `N` bytes of body without chunk framing.
    ContentLength,
    /// Response advertises `Transfer-Encoding: chunked`; each chunk is
    /// prefixed with `<hex_len>\r\n`, suffixed with `\r\n`, and terminated
    /// with `0\r\n\r\n` (unless `truncate_at_byte` fires first).
    Chunked,
}

/// Configuration for [`ChunkedFlushH1Backend::start`].
pub struct ChunkedFlushConfig {
    /// Total body size in bytes (payload only; chunk framing overhead is
    /// added on top for [`TransferEncoding::Chunked`]). Ignored when
    /// [`ChunkedFlushConfig::body`] is `Some(_)` — the buffer's length is
    /// authoritative in that case.
    pub body_size: usize,
    /// Per-chunk payload size; the last chunk may be smaller.
    pub chunk_size: usize,
    /// Time to sleep between chunks. Required >0 when `tcp_nodelay=true` to
    /// defeat kernel write-coalescing.
    pub inter_chunk_delay: Duration,
    /// Framing of the response body.
    pub transfer_encoding: TransferEncoding,
    /// When `true`, `TCP_NODELAY` is set on every accepted stream so each
    /// `write_all` + `flush` pair is emitted as a distinct segment.
    pub tcp_nodelay: bool,
    /// When `Some(N)`, drop the TCP connection after `N` bytes of RAW output
    /// (headers + body + any chunk framing) have been written, WITHOUT
    /// writing the terminating `0\r\n\r\n`. Used to reproduce C3
    /// (chunked-EOF misclassification).
    pub truncate_at_byte: Option<usize>,
    /// When `Some(buf)`, serve those exact bytes verbatim (the backend
    /// walks a cursor through `buf` slice-by-slice using `chunk_size`);
    /// `body_size` is overridden to `buf.len()`. When `None`, fall back to
    /// the default `b'Z'` fill driven by `body_size`.
    pub body: Option<Arc<Vec<u8>>>,
    /// Extra response header lines appended after `Content-Type` and the
    /// framing header (`Transfer-Encoding` / `Content-Length`). Each entry
    /// is one header, given WITHOUT the trailing `\r\n`; e.g.
    /// `"Content-Encoding: gzip"`. Callers must not include the blank-line
    /// terminator — the backend emits it exactly once at the end of the
    /// header block.
    pub extra_response_headers: Vec<String>,
    /// Overrides the default `Content-Type: application/octet-stream`. For
    /// `None`, the legacy default is preserved; for `Some("application/json")`
    /// the backend sends that instead.
    pub content_type: Option<String>,
}

/// Keep-alive HTTP/1.1 backend that serves a configurable body in small,
/// flushed segments.
pub struct ChunkedFlushH1Backend {
    stop: Arc<AtomicBool>,
    responses_sent: Arc<AtomicUsize>,
    thread: Option<JoinHandle<()>>,
}

impl ChunkedFlushH1Backend {
    pub fn start(address: SocketAddr, config: ChunkedFlushConfig) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let responses_sent = Arc::new(AtomicUsize::new(0));
        let stop_clone = stop.clone();
        let resp_count = responses_sent.clone();

        let thread = thread::spawn(move || {
            let listener = bind_std_listener(address, "chunked-flush H1 backend");
            listener
                .set_nonblocking(true)
                .expect("could not set nonblocking");

            loop {
                if stop_clone.load(Ordering::Relaxed) {
                    break;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if config.tcp_nodelay {
                            let _ = stream.set_nodelay(true);
                        }
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                        // Read and discard the request headers (single-shot
                        // read is sufficient: the largest test request is a
                        // plain GET that fits in one syscall).
                        let mut buf = [0u8; 4096];
                        let _ = stream.read(&mut buf);

                        let mut bytes_written: usize = 0;
                        let mut truncated = false;

                        // Effective body length: when a caller-supplied
                        // buffer is present, its length wins over
                        // `body_size` — we serve the buffer verbatim.
                        let effective_body_size = match &config.body {
                            Some(buf) => buf.len(),
                            None => config.body_size,
                        };

                        // Build the response header block as a list of
                        // lines and join with `\r\n`, then append the
                        // single blank-line terminator. Keeps the framing
                        // clean when `extra_response_headers` is populated.
                        let content_type = config
                            .content_type
                            .as_deref()
                            .unwrap_or("application/octet-stream");
                        let mut header_lines: Vec<String> = vec![
                            String::from("HTTP/1.1 200 OK"),
                            format!("Content-Type: {content_type}"),
                        ];
                        match config.transfer_encoding {
                            TransferEncoding::ContentLength => {
                                header_lines.push(format!("Content-Length: {effective_body_size}"))
                            }
                            TransferEncoding::Chunked => {
                                header_lines.push(String::from("Transfer-Encoding: chunked"))
                            }
                        }
                        for extra in &config.extra_response_headers {
                            header_lines.push(extra.clone());
                        }
                        let mut headers = header_lines.join("\r\n");
                        headers.push_str("\r\n\r\n");
                        if stream.write_all(headers.as_bytes()).is_err() {
                            continue;
                        }
                        let _ = stream.flush();
                        bytes_written += headers.len();

                        if Self::should_truncate(&config, bytes_written) {
                            drop(stream);
                            continue;
                        }

                        // Body phase. When `config.body` is `Some`, walk a
                        // cursor through the caller-supplied buffer so the
                        // wire content is deterministic (e.g. a gzipped
                        // customer payload whose sha256 the test asserts
                        // against). Otherwise emit `b'Z'` fill so the wire
                        // stays human-inspectable in logs.
                        let fill = if config.body.is_none() {
                            vec![b'Z'; config.chunk_size.max(1)]
                        } else {
                            Vec::new()
                        };
                        let chunk_size = config.chunk_size.max(1);
                        let mut pos: usize = 0;
                        let mut remaining = effective_body_size;
                        let mut write_failed = false;
                        while remaining > 0 {
                            let to_write = remaining.min(chunk_size);
                            let piece: &[u8] = match &config.body {
                                Some(buf) => &buf[pos..pos + to_write],
                                None => &fill[..to_write],
                            };
                            match config.transfer_encoding {
                                TransferEncoding::ContentLength => {
                                    if stream.write_all(piece).is_err() {
                                        write_failed = true;
                                        break;
                                    }
                                    bytes_written += to_write;
                                }
                                TransferEncoding::Chunked => {
                                    let header = format!("{to_write:x}\r\n");
                                    if stream.write_all(header.as_bytes()).is_err() {
                                        write_failed = true;
                                        break;
                                    }
                                    bytes_written += header.len();
                                    if stream.write_all(piece).is_err() {
                                        write_failed = true;
                                        break;
                                    }
                                    bytes_written += to_write;
                                    if stream.write_all(b"\r\n").is_err() {
                                        write_failed = true;
                                        break;
                                    }
                                    bytes_written += 2;
                                }
                            }
                            // `flush()` forces rustls-independent flush on
                            // TCP stream. Combined with `TCP_NODELAY` and the
                            // sleep below, this fragments the wire.
                            if stream.flush().is_err() {
                                write_failed = true;
                                break;
                            }

                            if Self::should_truncate(&config, bytes_written) {
                                truncated = true;
                                break;
                            }

                            pos += to_write;
                            remaining -= to_write;
                            if remaining > 0 && !config.inter_chunk_delay.is_zero() {
                                thread::sleep(config.inter_chunk_delay);
                            }
                        }

                        if !write_failed && !truncated {
                            if let TransferEncoding::Chunked = config.transfer_encoding {
                                let _ = stream.write_all(b"0\r\n\r\n");
                                let _ = stream.flush();
                            }
                            resp_count.fetch_add(1, Ordering::Relaxed);
                            // Keep the connection alive so sozu's H1 parser
                            // frames the response by Content-Length /
                            // chunked terminator, not by EOF. Wait for the
                            // peer to close or for the test to stop.
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                            let mut drain = [0u8; 256];
                            loop {
                                if stop_clone.load(Ordering::Relaxed) {
                                    break;
                                }
                                match stream.read(&mut drain) {
                                    Ok(0) => break,
                                    Ok(_) => continue,
                                    Err(_) => break,
                                }
                            }
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

    fn should_truncate(config: &ChunkedFlushConfig, bytes_written: usize) -> bool {
        matches!(config.truncate_at_byte, Some(limit) if bytes_written >= limit)
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

impl Drop for ChunkedFlushH1Backend {
    fn drop(&mut self) {
        self.stop();
    }
}
