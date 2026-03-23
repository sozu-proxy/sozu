use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
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
    thread: Option<thread::JoinHandle<()>>,
}

impl H2Backend {
    pub fn start(name: impl Into<String>, address: SocketAddr, body: impl Into<String>) -> Self {
        let name = name.into();
        let body: Bytes = Bytes::from(body.into());
        let stop = Arc::new(AtomicBool::new(false));
        let requests_received = Arc::new(AtomicUsize::new(0));
        let responses_sent = Arc::new(AtomicUsize::new(0));

        let stop_clone = stop.clone();
        let req_count = requests_received.clone();
        let resp_count = responses_sent.clone();
        let thread_name = name.clone();

        let thread = thread::spawn(move || {
            let rt = Runtime::new().expect("could not create tokio runtime");
            rt.block_on(async move {
                let listener = bind_tokio_listener(address, "h2 backend");

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
                    let name = thread_name.clone();

                    tokio::spawn(async move {
                        let io = TokioIo::new(stream);
                        let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                            let body = body.clone();
                            let req_count = req_count.clone();
                            let resp_count = resp_count.clone();
                            let name = name.clone();
                            async move {
                                req_count.fetch_add(1, Ordering::Relaxed);
                                println!(
                                    "{name}: received {} {} (h2 backend)",
                                    req.method(),
                                    req.uri()
                                );

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

        Self {
            name,
            stop,
            requests_received,
            responses_sent,
            thread: Some(thread),
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            // Give the thread a moment to see the stop signal
            thread::sleep(std::time::Duration::from_millis(100));
            drop(thread);
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
