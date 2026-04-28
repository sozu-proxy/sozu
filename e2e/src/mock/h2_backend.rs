use std::{
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
};

use http_body_util::Full;
use hyper::{Request, Response, body::Bytes, service::service_fn};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder as ServerBuilder,
};
use tokio::runtime::Runtime;

use crate::port_registry::bind_tokio_listener;

/// One row of the H2 backend's request log. Cardinality tests use this
/// to assert deep equality on what reached the wire after Sōzu applied
/// frontend rewrites + header edits — counters alone do not catch a
/// rewrite bug that lands the request on the right path with the wrong
/// authority, etc.
///
/// Header values are stored as `Vec<u8>` so a regression that forwards a
/// non-UTF-8 byte (a corrupted upstream header, a smuggled control char)
/// surfaces in the assertion diff rather than being silently lossily
/// downgraded to an empty string by `HeaderValue::to_str`.
#[derive(Debug, Clone)]
pub struct RecordedH2Request {
    pub method: String,
    pub authority: String,
    pub path: String,
    pub headers: Vec<(String, Vec<u8>)>,
}

/// An HTTP/2 mock backend that accepts cleartext H2 connections (h2c).
///
/// Sozu connects to backends over plain TCP and speaks HTTP/2 directly
/// (no TLS on the backend side). This backend uses hyper's server builder
/// with HTTP/2 support to handle those connections.
pub struct H2Backend {
    pub name: String,
    stop: Arc<AtomicBool>,
    pub requests_received: Arc<AtomicUsize>,
    pub responses_sent: Arc<AtomicUsize>,
    requests_log: Arc<Mutex<Vec<RecordedH2Request>>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl H2Backend {
    pub fn start(name: impl Into<String>, address: SocketAddr, body: impl Into<String>) -> Self {
        let name = name.into();
        let body: Bytes = Bytes::from(body.into());
        let stop = Arc::new(AtomicBool::new(false));
        let requests_received = Arc::new(AtomicUsize::new(0));
        let responses_sent = Arc::new(AtomicUsize::new(0));
        let requests_log: Arc<Mutex<Vec<RecordedH2Request>>> = Arc::new(Mutex::new(Vec::new()));

        let stop_clone = stop.clone();
        let req_count = requests_received.clone();
        let resp_count = responses_sent.clone();
        let req_log = requests_log.clone();
        let thread_name = name.clone();

        // Synchronous readiness handshake: the spawned tokio thread must
        // signal it has bound the listener AND entered the accept loop
        // before `H2Backend::start` returns. Without this, the test
        // request can race the accept-loop spin-up on slow CI runners
        // (observed on `Test (false, beta)` of run 24916520793 where
        // FIX-18 / FIX-22 sessions saw a 503 from sozu because the
        // backend wasn't yet draining its accept queue when sozu opened
        // the outbound TCP connection — sozu's H2 handshake then timed
        // out, the cluster marked the backend down for a window, and
        // the test returned the wrong status code).
        let (ready_tx, ready_rx) = mpsc::channel::<()>();

        let thread = thread::spawn(move || {
            let rt = Runtime::new().expect("could not create tokio runtime");
            rt.block_on(async move {
                let listener = bind_tokio_listener(address, "h2 backend");
                // Notify the caller now that we are about to enter the
                // accept loop. After this point a TCP SYN to `address`
                // will be drained by `listener.accept()` within ≤50 ms.
                let _ = ready_tx.send(());

                loop {
                    if stop_clone.load(Ordering::Relaxed) {
                        break;
                    }

                    let accept = tokio::time::timeout(
                        std::time::Duration::from_millis(50),
                        listener.accept(),
                    )
                    .await;

                    let (stream, _peer) = match accept {
                        Ok(Ok(s)) => s,
                        Ok(Err(e)) => {
                            eprintln!("{thread_name}: accept error: {e}");
                            continue;
                        }
                        Err(_) => continue, // timeout, check stop flag
                    };

                    let body = body.clone();
                    let req_count = req_count.clone();
                    let resp_count = resp_count.clone();
                    let req_log = req_log.clone();
                    let name = thread_name.clone();

                    tokio::spawn(async move {
                        let io = TokioIo::new(stream);
                        let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                            let body = body.clone();
                            let req_count = req_count.clone();
                            let resp_count = resp_count.clone();
                            let req_log = req_log.clone();
                            let name = name.clone();
                            async move {
                                req_count.fetch_add(1, Ordering::Relaxed);
                                println!(
                                    "{name}: received {} {} (h2 backend)",
                                    req.method(),
                                    req.uri()
                                );
                                // Snapshot the request shape so cardinality
                                // tests can assert on the rewritten authority
                                // / path / forwarded headers that reached the
                                // backend wire.
                                let recorded = RecordedH2Request {
                                    method: req.method().as_str().to_owned(),
                                    authority: req
                                        .uri()
                                        .authority()
                                        .map(|a| a.as_str().to_owned())
                                        .unwrap_or_default(),
                                    path: req.uri().path().to_owned(),
                                    headers: req
                                        .headers()
                                        .iter()
                                        .map(|(k, v)| {
                                            (k.as_str().to_owned(), v.as_bytes().to_vec())
                                        })
                                        .collect(),
                                };
                                if let Ok(mut log) = req_log.lock() {
                                    log.push(recorded);
                                }

                                let response = Response::builder()
                                    .status(200)
                                    .header("content-type", "text/plain")
                                    .body(Full::new(body))
                                    .unwrap();

                                resp_count.fetch_add(1, Ordering::Relaxed);
                                Ok::<_, hyper::Error>(response)
                            }
                        });

                        let builder = ServerBuilder::new(TokioExecutor::new());
                        if let Err(e) = builder.serve_connection(io, service).await {
                            eprintln!("h2 backend connection error: {e}");
                        }
                    });
                }
            });
        });

        // Block until the spawned thread has bound the listener and is
        // about to call accept(). Cap at 5 s so a misconfigured runtime
        // does not hang the test forever — that would manifest as a
        // panic instead of the silent 503 race we are trying to close.
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("H2Backend: accept loop did not signal readiness within 5 s");

        Self {
            name,
            stop,
            requests_received,
            responses_sent,
            requests_log,
            thread: Some(thread),
        }
    }

    /// Snapshot of every request the backend received since [`start`].
    /// Cardinality tests use this to assert on the authority/path/headers
    /// the backend actually saw on the wire.
    pub fn recorded_requests(&self) -> Vec<RecordedH2Request> {
        self.requests_log
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            // Join the spawned thread so its tokio runtime drops in
            // lockstep with the test exit. Detaching it (the previous
            // `drop(thread)`) leaves the runtime — and its threadpool —
            // alive across subsequent tests in the same process, which
            // cumulatively starves cargo's default thread budget on
            // contended CI and amplifies otherwise-unrelated flakes.
            // The accept loop polls the stop flag every 50 ms (see the
            // timeout inside `start`) so this join completes within
            // ~100 ms of the stop signal.
            if let Err(payload) = thread.join() {
                eprintln!("H2Backend: spawned thread panicked: {payload:?}");
            }
        }
    }

    pub fn get_requests_received(&self) -> usize {
        self.requests_received.load(Ordering::Relaxed)
    }

    pub fn get_responses_sent(&self) -> usize {
        self.responses_sent.load(Ordering::Relaxed)
    }
}

impl Drop for H2Backend {
    fn drop(&mut self) {
        self.stop();
    }
}
