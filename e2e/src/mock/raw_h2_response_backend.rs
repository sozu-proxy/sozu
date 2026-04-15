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
//! [`RawH2ResponseBackend::push_header`].
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
}

impl Default for RawResponse {
    fn default() -> Self {
        Self {
            status: b"200".to_vec(),
            extra_headers: Vec::new(),
        }
    }
}

/// Raw-byte H2 response backend — see module docs.
pub struct RawH2ResponseBackend {
    stop: Arc<AtomicBool>,
    #[allow(dead_code)]
    connections_received: Arc<AtomicUsize>,
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
        let response = Arc::new(Mutex::new(RawResponse::default()));

        let stop_thread = stop.clone();
        let conn_thread = connections_received.clone();
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
                    let _ = tokio::time::timeout(Duration::from_millis(500), stream.read(&mut buf))
                        .await;

                    // Snapshot the configured response and build the reply.
                    let response_snapshot = response_thread.lock().unwrap().clone();
                    let mut out = Vec::new();
                    // SETTINGS (empty, non-ACK).
                    out.extend_from_slice(&[0, 0, 0, 0x04, 0, 0, 0, 0, 0]);
                    // SETTINGS ACK (acknowledges sozu's settings).
                    out.extend_from_slice(&[0, 0, 0, 0x04, 0x01, 0, 0, 0, 0]);

                    // HEADERS frame carrying the crafted :status + any
                    // extra headers, END_HEADERS | END_STREAM on stream 1.
                    let header_block = encode_header_block(&response_snapshot);
                    let len = header_block.len();
                    assert!(
                        len < (1 << 24),
                        "raw h2 response header block larger than 24-bit payload_len"
                    );
                    out.push((len >> 16) as u8);
                    out.push((len >> 8) as u8);
                    out.push(len as u8);
                    out.push(0x01); // HEADERS
                    out.push(0x04 | 0x01); // END_HEADERS | END_STREAM
                    out.extend_from_slice(&1u32.to_be_bytes());
                    out.extend_from_slice(&header_block);

                    let _ = stream.write_all(&out).await;
                    let _ = stream.flush().await;
                    // Give sozu time to consume the response before we FIN.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    drop(stream);
                }
            });
        });

        Self {
            stop,
            connections_received,
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

    #[allow(dead_code)]
    pub fn connections_received(&self) -> usize {
        self.connections_received.load(Ordering::Relaxed)
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            thread::sleep(Duration::from_millis(100));
            drop(t);
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
