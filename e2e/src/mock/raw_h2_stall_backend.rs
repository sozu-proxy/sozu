//! Raw-byte HTTP/2 "stall" mock backend.
//!
//! Advertises `SETTINGS_INITIAL_WINDOW_SIZE = 0` so sozu's per-stream send
//! window toward this backend is exhausted from the start: any request body sozu
//! forwards is window-blocked and buffered, never delivered. The backend never
//! grants a `WINDOW_UPDATE` and never answers; it only emits periodic
//! connection-level `PING` frames to keep sozu's backend reader active (so the
//! per-stream reaper runs at the top of `readable()` and frees the slot).
//!
//! Used to exercise the `Position::Client` window-stall reap path: a stalled
//! H2-backend upload must be reaped and the client returned a `502` rather than
//! hanging — coverage no other mock provides (`H2Backend` is hyper-managed and
//! auto-drains; `RawH2ResponseBackend` advertises the default 65535 window and
//! answers immediately).

use std::{
    net::SocketAddr,
    sync::{
        Arc,
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

/// Raw-byte H2 stall backend — see module docs.
pub struct RawH2StallBackend {
    stop: Arc<AtomicBool>,
    #[allow(dead_code)]
    connections_received: Arc<AtomicUsize>,
    thread: Option<thread::JoinHandle<()>>,
}

impl RawH2StallBackend {
    pub fn new(address: SocketAddr) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let connections_received = Arc::new(AtomicUsize::new(0));
        let stop_thread = stop.clone();
        let conn_thread = connections_received.clone();

        let thread = thread::spawn(move || {
            let rt = Runtime::new().expect("could not create tokio runtime");
            rt.block_on(async move {
                let listener = bind_tokio_listener(address, "raw h2 stall backend");
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

                    // Server SETTINGS advertising INITIAL_WINDOW_SIZE = 0 (so
                    // sozu's per-stream send window toward us is exhausted from
                    // the first byte of request body), then a SETTINGS ACK for
                    // sozu's settings.
                    let mut handshake = Vec::new();
                    handshake.extend_from_slice(&[
                        0x00, 0x00, 0x06, // length 6
                        0x04, // SETTINGS
                        0x00, // flags
                        0x00, 0x00, 0x00, 0x00, // stream 0
                        0x00, 0x04, // SETTINGS_INITIAL_WINDOW_SIZE
                        0x00, 0x00, 0x00, 0x00, // value 0
                    ]);
                    // SETTINGS ACK
                    handshake
                        .extend_from_slice(&[0x00, 0x00, 0x00, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00]);
                    if stream.write_all(&handshake).await.is_err() {
                        continue;
                    }
                    let _ = stream.flush().await;

                    // Hold the connection open: emit a connection-level PING every
                    // ~150ms (drives sozu's backend reader → the per-stream
                    // reaper) and drain + discard whatever sozu sends. Never grant
                    // a WINDOW_UPDATE, never answer — the upload stays blocked
                    // until sozu reaps the stream.
                    let ping = [
                        0x00, 0x00, 0x08, // length 8
                        0x06, // PING
                        0x00, // flags (not ACK)
                        0x00, 0x00, 0x00, 0x00, // stream 0
                        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, // opaque data
                    ];
                    let mut buf = [0u8; 8192];
                    loop {
                        if stop_thread.load(Ordering::Relaxed) {
                            break;
                        }
                        if stream.write_all(&ping).await.is_err() {
                            break;
                        }
                        let _ = stream.flush().await;
                        // Drain + discard whatever sozu sends; a clean EOF
                        // (Ok(Ok(0))) means the peer closed, anything else (data
                        // or read-timeout) means keep stalling.
                        let read =
                            tokio::time::timeout(Duration::from_millis(150), stream.read(&mut buf))
                                .await;
                        if let Ok(Ok(0)) = read {
                            break;
                        }
                    }
                }
            });
        });

        Self {
            stop,
            connections_received,
            thread: Some(thread),
        }
    }

    #[allow(dead_code)]
    pub fn connections_received(&self) -> usize {
        self.connections_received.load(Ordering::Relaxed)
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            // Join (do not detach) so the listening socket is released before the
            // test's port_registry teardown — see RawH2ResponseBackend::stop.
            if let Err(err) = t.join() {
                eprintln!("raw h2 stall backend thread panicked during shutdown: {err:?}");
            }
        }
    }
}

impl Drop for RawH2StallBackend {
    fn drop(&mut self) {
        self.stop();
    }
}
