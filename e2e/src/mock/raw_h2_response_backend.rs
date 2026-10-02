//! Raw-byte HTTP/2 response mock backend.
//!
//! [`H2Backend`](super::h2_backend::H2Backend) sits on top of hyper, which
//! sanitises outgoing responses (refuses invalid `:status`, coerces header
//! names to lowercase, etc.). That is exactly what several adversarial
//! recipes need to bypass — notably FIX-2, which verifies that sozu rejects
//! an upstream `:status` that is not exactly three ASCII digits.
//!
//! This backend accepts cleartext H2 connections, completes the preface +
//! SETTINGS handshake, reads one HEADERS frame, and replies with a
//! hand-crafted HEADERS frame whose `:status` field carries caller-
//! supplied bytes (e.g. `"abc"`, `"20"`, `"+200"`, `"1234"`). Additional
//! header pairs can be appended verbatim via
//! [`RawH2ResponseBackend::push_header`]. Interim (1xx) heads can precede
//! it ([`RawH2ResponseBackend::push_interim`]), and a body, a trailer
//! section or an empty DATA frame can end the stream. In second-stream mode
//! ([`RawH2ResponseBackend::set_serve_second_stream`]) the end of stream 1
//! waits for sozu to open another stream on the same connection, which is
//! answered too. Body DATA frames wait for sozu's flow-control windows
//! (SETTINGS and WINDOW_UPDATE), so a body may exceed the initial 65 535
//! bytes; a window that stays closed for 2 s panics the backend thread.
//!
//! The header block is emitted using HPACK's "literal header field without
//! indexing — new name" form (`0x00` opcode) so the backend does not need a
//! full HPACK encoder — each name/value pair is serialised as:
//!
//! ```text
//! 0x00 <name-len:7bit> <name-bytes> <val-len:7bit> <val-bytes>
//! ```
//!
//! Callers therefore keep every byte they want on the wire, including
//! byte sequences a real HPACK encoder would reject.

use std::{
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    runtime::Runtime,
};

use crate::port_registry::bind_tokio_listener;

/// Response configuration used by the backend thread.
#[derive(Clone)]
struct RawResponse {
    /// Bytes placed in the `:status` pseudo-header. Arbitrary — the
    /// backend does not validate or interpret these bytes.
    status: Vec<u8>,
    /// Extra `(name, value)` header pairs appended after `:status`, in
    /// the order they were pushed.
    extra_headers: Vec<(Vec<u8>, Vec<u8>)>,
    /// Optional response body. When `Some`, HEADERS is sent without
    /// END_STREAM, followed (after [`Self::body_delay`]) by one or more
    /// DATA frames ≤ 16384 bytes each, last flagged END_STREAM.
    body: Option<Vec<u8>>,
    /// Delay between HEADERS and the first DATA frame. Only consulted
    /// when [`Self::body`] is `Some`.
    body_delay: Duration,
    /// Optional trailer fields. When `Some`, HEADERS is sent without
    /// END_STREAM, the body DATA frames (if any) keep END_STREAM clear,
    /// and a trailer HEADERS frame carrying these pairs, possibly none,
    /// ends the stream.
    trailers: Option<Vec<(Vec<u8>, Vec<u8>)>>,
    /// `:status` values of interim (1xx) header sections sent before the
    /// final one, each in its own HEADERS frame without END_STREAM.
    interim: Vec<Vec<u8>>,
    /// When `true` and neither [`Self::body`] nor [`Self::trailers`] is
    /// set, HEADERS is sent without END_STREAM and an empty DATA frame
    /// flagged END_STREAM ends the stream.
    empty_data_end: bool,
    /// When `true`, the end of stream 1 (its DATA and trailer frames) is
    /// held back until sozu opens another stream on the same connection;
    /// that stream is then answered `200` with the body `pong`.
    serve_second_stream: bool,
    /// When `true`, the SETTINGS, HEADERS, DATA and trailer frames of a
    /// response leave in one `write_all`, so sozu reads them together;
    /// [`Self::body_delay`] is then not consulted.
    single_write: bool,
}

impl Default for RawResponse {
    fn default() -> Self {
        Self {
            status: b"200".to_vec(),
            extra_headers: Vec::new(),
            body: None,
            body_delay: Duration::ZERO,
            trailers: None,
            interim: Vec::new(),
            empty_data_end: false,
            serve_second_stream: false,
            single_write: false,
        }
    }
}

/// Raw-byte H2 response backend — see module docs.
pub struct RawH2ResponseBackend {
    stop: Arc<AtomicBool>,
    #[allow(dead_code)]
    connections_received: Arc<AtomicUsize>,
    /// GOAWAY frames read from sozu in [`RawResponse::serve_second_stream`]
    /// mode.
    goaways_received: Arc<AtomicUsize>,
    /// RST_STREAM frames read from sozu in the same mode.
    resets_received: Arc<AtomicUsize>,
    response: Arc<Mutex<RawResponse>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl RawH2ResponseBackend {
    /// Start a new backend bound to `address`. The default response is
    /// `:status: 200` with no extra headers — equivalent to a minimal
    /// hyper response. Override via [`Self::set_status`] /
    /// [`Self::push_header`] before driving traffic at the listener.
    pub fn new(address: SocketAddr) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let connections_received = Arc::new(AtomicUsize::new(0));
        let goaways_received = Arc::new(AtomicUsize::new(0));
        let resets_received = Arc::new(AtomicUsize::new(0));
        let response = Arc::new(Mutex::new(RawResponse::default()));

        let stop_thread = stop.clone();
        let conn_thread = connections_received.clone();
        let goaways_thread = goaways_received.clone();
        let resets_thread = resets_received.clone();
        let response_thread = response.clone();

        let thread = thread::spawn(move || {
            let rt = Runtime::new().expect("could not create tokio runtime");
            rt.block_on(async move {
                let listener = bind_tokio_listener(address, "raw h2 response backend");
                loop {
                    if stop_thread.load(Ordering::Relaxed) {
                        break;
                    }
                    let accept =
                        tokio::time::timeout(Duration::from_millis(50), listener.accept()).await;
                    let (mut stream, _) = match accept {
                        Ok(Ok(s)) => s,
                        _ => continue,
                    };
                    conn_thread.fetch_add(1, Ordering::Relaxed);

                    // Consume preface + SETTINGS + HEADERS; we do not parse
                    // the incoming frames, a bounded read is enough for the
                    // adversarial recipes.
                    let mut buf = vec![0u8; 4096];
                    let read =
                        tokio::time::timeout(Duration::from_millis(500), stream.read(&mut buf))
                            .await;
                    // What follows the client preface: sozu's frames, read
                    // for its flow-control windows and its second stream.
                    let mut peer = Peer {
                        pending: match read {
                            Ok(Ok(n)) => buf[..n].get(24..).unwrap_or_default().to_vec(),
                            _ => Vec::new(),
                        },
                        counters: [&goaways_thread, &resets_thread],
                        initial_window: DEFAULT_WINDOW,
                        connection_window: DEFAULT_WINDOW,
                        stream_window: DEFAULT_WINDOW,
                    };
                    peer.read_frames(&mut stream, Duration::ZERO, Until::Timeout)
                        .await;

                    // Snapshot the configured response and build the reply.
                    let response_snapshot = response_thread.lock().unwrap().clone();
                    let mut out = Vec::new();
                    // SETTINGS (empty, non-ACK).
                    out.extend_from_slice(&[0, 0, 0, 0x04, 0, 0, 0, 0, 0]);
                    // SETTINGS ACK (acknowledges sozu's settings).
                    out.extend_from_slice(&[0, 0, 0, 0x04, 0x01, 0, 0, 0, 0]);

                    // Interim HEADERS frames, END_HEADERS only.
                    for status in &response_snapshot.interim {
                        let mut block = Vec::new();
                        encode_literal(&mut block, b":status", status);
                        push_frame(&mut out, 0x01, 0x04, 1, &block);
                    }

                    // HEADERS frame carrying the crafted :status + any
                    // extra headers. END_HEADERS always; END_STREAM only
                    // when there's no body (body = None preserves the
                    // original single-frame behaviour).
                    let header_block = encode_header_block(&response_snapshot);
                    let len = header_block.len();
                    assert!(
                        len < (1 << 24),
                        "raw h2 response header block larger than 24-bit payload_len"
                    );
                    let has_trailers = response_snapshot.trailers.is_some();
                    let empty_data_end = response_snapshot.empty_data_end
                        && response_snapshot.body.is_none()
                        && !has_trailers;
                    let has_body =
                        response_snapshot.body.is_some() || has_trailers || empty_data_end;
                    let headers_flags: u8 = if has_body { 0x04 } else { 0x04 | 0x01 };
                    out.push((len >> 16) as u8);
                    out.push((len >> 8) as u8);
                    out.push(len as u8);
                    out.push(0x01); // HEADERS
                    out.push(headers_flags);
                    out.extend_from_slice(&1u32.to_be_bytes());
                    out.extend_from_slice(&header_block);

                    let single_write = response_snapshot.single_write;
                    if !single_write {
                        let _ = stream.write_all(&out).await;
                        let _ = stream.flush().await;
                        out.clear();
                    }

                    // Hold the end of stream 1 back until sozu opens another
                    // stream on this connection.
                    let second_stream = if response_snapshot.serve_second_stream {
                        peer.read_frames(&mut stream, Duration::from_secs(2), Until::SecondStream)
                            .await
                    } else {
                        None
                    };

                    // HEADERS-then-delay-then-DATA mode: split body into
                    // ≤ 16384-byte DATA frames (default max_frame_size),
                    // last carrying END_STREAM. The delay between HEADERS
                    // and the first DATA surfaces the H2-backend →
                    // H2-frontend peer-rearm gap (sozu's reader must be
                    // woken by `signal_pending_write` after DATA arrives,
                    // not by the initial HEADERS natural-writable).
                    if let Some(body) = response_snapshot.body.as_ref() {
                        if !single_write && !response_snapshot.body_delay.is_zero() {
                            tokio::time::sleep(response_snapshot.body_delay).await;
                        }
                        const MAX_FRAME: usize = 16384;
                        let mut emitted = 0usize;
                        while emitted < body.len() {
                            let end = (emitted + MAX_FRAME).min(body.len());
                            let chunk = &body[emitted..end];
                            let is_last = end == body.len();
                            let chunk_len = chunk.len();
                            let mut frame = Vec::with_capacity(9 + chunk_len);
                            frame.push((chunk_len >> 16) as u8);
                            frame.push((chunk_len >> 8) as u8);
                            frame.push(chunk_len as u8);
                            frame.push(0x00); // DATA
                            frame.push(if is_last && !has_trailers { 0x01 } else { 0x00 });
                            frame.extend_from_slice(&1u32.to_be_bytes());
                            frame.extend_from_slice(chunk);
                            if single_write {
                                out.extend_from_slice(&frame);
                            } else {
                                // Respect sozu's flow-control windows (RFC
                                // 9113 §6.9), waiting for its WINDOW_UPDATE.
                                // A window sozu never opens is a test
                                // failure, not a reason to overrun it.
                                while peer.window() < chunk_len as i64 {
                                    assert!(
                                        peer.read_frames(
                                            &mut stream,
                                            Duration::from_secs(2),
                                            Until::WindowUpdate,
                                        )
                                        .await
                                        .is_some(),
                                        "sozu sent no WINDOW_UPDATE within 2 s for a {chunk_len}-byte \
                                         DATA frame (window {})",
                                        peer.window()
                                    );
                                }
                                peer.connection_window -= chunk_len as i64;
                                peer.stream_window -= chunk_len as i64;
                                let _ = stream.write_all(&frame).await;
                                let _ = stream.flush().await;
                            }
                            emitted = end;
                        }
                    }

                    // Trailer HEADERS frame: END_HEADERS | END_STREAM.
                    if let Some(trailers) = response_snapshot.trailers.as_ref() {
                        let mut block = Vec::new();
                        for (name, value) in trailers {
                            encode_literal(&mut block, name, value);
                        }
                        let len = block.len();
                        let mut frame = Vec::with_capacity(9 + len);
                        frame.push((len >> 16) as u8);
                        frame.push((len >> 8) as u8);
                        frame.push(len as u8);
                        frame.push(0x01); // HEADERS
                        frame.push(0x04 | 0x01);
                        frame.extend_from_slice(&1u32.to_be_bytes());
                        frame.extend_from_slice(&block);
                        out.extend_from_slice(&frame);
                    }
                    if !out.is_empty() {
                        let _ = stream.write_all(&out).await;
                        let _ = stream.flush().await;
                    }

                    if empty_data_end {
                        let mut frame = Vec::new();
                        push_frame(&mut frame, 0x00, 0x01, 1, &[]);
                        let _ = stream.write_all(&frame).await;
                        let _ = stream.flush().await;
                    }

                    if let Some(stream_id) = second_stream {
                        let mut block = Vec::new();
                        encode_literal(&mut block, b":status", b"200");
                        encode_literal(&mut block, b"content-length", b"4");
                        let mut frames = Vec::new();
                        push_frame(&mut frames, 0x01, 0x04, stream_id, &block);
                        push_frame(&mut frames, 0x00, 0x01, stream_id, b"pong");
                        let _ = stream.write_all(&frames).await;
                        let _ = stream.flush().await;
                    }
                    if response_snapshot.serve_second_stream {
                        // Count what sozu answers to the end of stream 1.
                        peer.read_frames(&mut stream, Duration::from_millis(300), Until::Timeout)
                            .await;
                    }

                    // Give sozu time to consume the response before we FIN.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    drop(stream);
                }
            });
        });

        Self {
            stop,
            connections_received,
            goaways_received,
            resets_received,
            response,
            thread: Some(thread),
        }
    }

    /// Replace the `:status` value returned on subsequent accepted
    /// connections. Accepts arbitrary bytes — the harness does not
    /// validate HTTP semantics.
    #[allow(dead_code)]
    pub fn set_status(&self, status: impl Into<Vec<u8>>) {
        self.response.lock().unwrap().status = status.into();
    }

    /// Append an extra header pair after `:status`.
    #[allow(dead_code)]
    pub fn push_header(&self, name: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) {
        self.response
            .lock()
            .unwrap()
            .extra_headers
            .push((name.into(), value.into()));
    }

    /// Configure a response body with an optional delay between HEADERS
    /// and the first DATA frame. When set, the response is emitted as
    /// HEADERS (END_HEADERS, no END_STREAM) → sleep(`delay`) → DATA
    /// frames ≤ 16 KiB each, the last flagged END_STREAM.
    ///
    /// Use this to reproduce the H2-backend → H2-frontend peer-rearm
    /// gap: the initial HEADERS arrives within the natural writable
    /// window, but the DATA frames after the delay only reach the
    /// frontend if `signal_pending_write` is paired with the peer's
    /// `Ready::WRITABLE` rearm.
    #[allow(dead_code)]
    pub fn set_body_with_delay(&self, body: impl Into<Vec<u8>>, delay: Duration) {
        let mut response = self.response.lock().unwrap();
        response.body = Some(body.into());
        response.body_delay = delay;
    }

    /// End subsequent responses with a trailer HEADERS frame carrying
    /// `trailers` (possibly none), or with END_STREAM on the last HEADERS
    /// or DATA frame when `None`.
    pub fn set_trailers(&self, trailers: Option<Vec<(Vec<u8>, Vec<u8>)>>) {
        self.response.lock().unwrap().trailers = trailers;
    }

    /// Send an interim (1xx) header section with `:status` `status`
    /// before the final one of subsequent responses.
    pub fn push_interim(&self, status: impl Into<Vec<u8>>) {
        self.response.lock().unwrap().interim.push(status.into());
    }

    /// End subsequent responses that set neither a body nor trailers with
    /// an empty DATA frame flagged END_STREAM, after a HEADERS frame
    /// without it, when `true`.
    pub fn set_empty_data_end(&self, empty_data_end: bool) {
        self.response.lock().unwrap().empty_data_end = empty_data_end;
    }

    /// Hold the end of stream 1 back until sozu opens a second stream on
    /// the same connection, then answer that stream `200` with the body
    /// `pong`, and count the GOAWAY and RST_STREAM frames sozu sends, when
    /// `true`.
    pub fn set_serve_second_stream(&self, serve_second_stream: bool) {
        self.response.lock().unwrap().serve_second_stream = serve_second_stream;
    }

    /// GOAWAY frames read from sozu in second-stream mode.
    pub fn goaways_received(&self) -> usize {
        self.goaways_received.load(Ordering::Relaxed)
    }

    /// RST_STREAM frames read from sozu in second-stream mode.
    pub fn resets_received(&self) -> usize {
        self.resets_received.load(Ordering::Relaxed)
    }

    /// Send every frame of subsequent responses in one `write_all` when
    /// `true`, so they reach sozu in one read, or frame by frame when
    /// `false` (the default).
    pub fn set_single_write(&self, single_write: bool) {
        self.response.lock().unwrap().single_write = single_write;
    }

    #[allow(dead_code)]
    pub fn connections_received(&self) -> usize {
        self.connections_received.load(Ordering::Relaxed)
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            // Join, do not detach: dropping the JoinHandle would leak
            // the thread (along with its listening socket) past the
            // test's teardown and reintroduce the port-binding flake
            // class that motivated `port_registry` in the first place.
            // The thread polls `stop` every ≤500 ms inside its accept
            // loop; we cap the wait conservatively in case it races
            // a long blocking syscall, and surface a panic payload.
            if let Err(err) = t.join() {
                eprintln!("raw h2 response backend thread panicked during shutdown: {err:?}");
            }
        }
    }
}

impl Drop for RawH2ResponseBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

/// HPACK "literal header field without indexing — new name" encoding of
/// each (name, value) pair. Both lengths are written as 7-bit HPACK
/// integers (no Huffman coding) — sufficient for names / values under 127
/// bytes, which covers every test case.
fn encode_header_block(response: &RawResponse) -> Vec<u8> {
    let mut out = Vec::new();
    encode_literal(&mut out, b":status", &response.status);
    for (name, value) in &response.extra_headers {
        encode_literal(&mut out, name, value);
    }
    out
}

/// Append a frame of type `kind` with `flags` on `stream_id` to `buf`.
fn push_frame(buf: &mut Vec<u8>, kind: u8, flags: u8, stream_id: u32, payload: &[u8]) {
    let len = payload.len();
    buf.extend_from_slice(&[(len >> 16) as u8, (len >> 8) as u8, len as u8, kind, flags]);
    buf.extend_from_slice(&stream_id.to_be_bytes());
    buf.extend_from_slice(payload);
}

/// The flow-control window each side starts with (RFC 9113 §6.9.2).
const DEFAULT_WINDOW: i64 = 65_535;

/// What [`Peer::read_frames`] waits for besides its timeout.
enum Until {
    /// Only the timeout.
    Timeout,
    /// A HEADERS frame opening a stream other than 1, whose id is returned.
    SecondStream,
    /// A WINDOW_UPDATE frame; `Some(0)` is returned.
    WindowUpdate,
}

/// What the backend knows of sozu's side of the connection.
struct Peer<'a> {
    /// Bytes read but not yet parsed, starting on a frame boundary.
    pending: Vec<u8>,
    /// GOAWAY and RST_STREAM frames read.
    counters: [&'a AtomicUsize; 2],
    /// `SETTINGS_INITIAL_WINDOW_SIZE` sozu announced.
    initial_window: i64,
    /// What the backend may still send on the connection.
    connection_window: i64,
    /// What the backend may still send on stream 1.
    stream_window: i64,
}

impl Peer<'_> {
    /// What the backend may still send on stream 1.
    fn window(&self) -> i64 {
        self.connection_window.min(self.stream_window)
    }

    /// Read and account for the frames sozu sends until `timeout` elapses or
    /// `until` is met.
    async fn read_frames(
        &mut self,
        stream: &mut tokio::net::TcpStream,
        timeout: Duration,
        until: Until,
    ) -> Option<u32> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut buf = vec![0u8; 4096];
        loop {
            while self.pending.len() >= 9 {
                let pending = &self.pending;
                let len = (usize::from(pending[0]) << 16)
                    | (usize::from(pending[1]) << 8)
                    | usize::from(pending[2]);
                if pending.len() < 9 + len {
                    break;
                }
                let (kind, flags) = (pending[3], pending[4]);
                let stream_id =
                    u32::from_be_bytes([pending[5], pending[6], pending[7], pending[8]])
                        & 0x7fff_ffff;
                let payload = pending[9..9 + len].to_vec();
                self.pending.drain(..9 + len);
                match kind {
                    0x07 => {
                        self.counters[0].fetch_add(1, Ordering::Relaxed);
                    }
                    0x03 => {
                        self.counters[1].fetch_add(1, Ordering::Relaxed);
                    }
                    0x04 if flags & 0x01 == 0 => {
                        for setting in payload.chunks_exact(6) {
                            if u16::from_be_bytes([setting[0], setting[1]]) == 0x4 {
                                let value = i64::from(u32::from_be_bytes([
                                    setting[2], setting[3], setting[4], setting[5],
                                ]));
                                self.stream_window += value - self.initial_window;
                                self.initial_window = value;
                            }
                        }
                    }
                    0x08 if payload.len() == 4 => {
                        let increment = i64::from(
                            u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
                                & 0x7fff_ffff,
                        );
                        match stream_id {
                            0 => self.connection_window += increment,
                            1 => self.stream_window += increment,
                            _ => {}
                        }
                        if matches!(until, Until::WindowUpdate) {
                            return Some(0);
                        }
                    }
                    0x01 if matches!(until, Until::SecondStream) && stream_id != 1 => {
                        return Some(stream_id);
                    }
                    _ => {}
                }
            }
            let read = tokio::time::timeout_at(deadline, stream.read(&mut buf)).await;
            match read {
                Ok(Ok(n)) if n > 0 => self.pending.extend_from_slice(&buf[..n]),
                _ => return None,
            }
        }
    }
}

fn encode_literal(buf: &mut Vec<u8>, name: &[u8], value: &[u8]) {
    assert!(name.len() < 0x7f, "name longer than 126 bytes unsupported");
    assert!(
        value.len() < 0x7f,
        "value longer than 126 bytes unsupported"
    );
    buf.push(0x00); // literal without indexing, new name
    buf.push(name.len() as u8);
    buf.extend_from_slice(name);
    buf.push(value.len() as u8);
    buf.extend_from_slice(value);
}
