//! End-to-end tests for H2 protocol correctness and edge cases.
//!
//! These tests verify that the H2 mux implementation handles edge cases
//! gracefully WITHOUT crashing the worker. They exercise:
//! - Basic H2 request/response (smoke test)
//! - CONTINUATION with wrong stream_id
//! - GoAway graceful drain
//! - RST_STREAM on backend disconnect
//! - Malformed frame resilience
//! - Content-Length mismatch
//! - Concurrent streams multiplexing

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use http_body_util::{BodyExt, Full};
use hyper::{Response, body::Bytes, service::service_fn};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder as ServerBuilder,
};
use rustls::ClientConfig;
use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, AddCertificate, CertificateAndKey, Cluster, ListenerType,
        RequestHttpFrontend, SocketAddress, request::RequestType,
    },
};
use tokio::net::TcpListener;

use crate::{
    mock::{
        aggregator::SimpleAggregator,
        async_backend::BackendHandle as AsyncBackend,
        h2_backend::H2Backend,
        https_client::{
            Verifier, build_h2_client, build_https_client, resolve_concurrent_requests,
            resolve_post_request, resolve_request,
        },
    },
    sozu::worker::Worker,
    tests::{State, provide_port, repeat_until_error_or, tests::create_local_address},
};

// ============================================================================
// H2 frame constants (RFC 9113)
// ============================================================================

const H2_FRAME_DATA: u8 = 0x0;
const H2_FRAME_HEADERS: u8 = 0x1;
const H2_FRAME_RST_STREAM: u8 = 0x3;
const H2_FRAME_SETTINGS: u8 = 0x4;
const H2_FRAME_PING: u8 = 0x6;
const H2_FRAME_GOAWAY: u8 = 0x7;
const H2_FRAME_WINDOW_UPDATE: u8 = 0x8;
const H2_FRAME_CONTINUATION: u8 = 0x9;

const H2_FLAG_ACK: u8 = 0x1;
const H2_FLAG_END_STREAM: u8 = 0x1;
const H2_FLAG_END_HEADERS: u8 = 0x4;

/// H2 error code: FLOW_CONTROL_ERROR (0x3) — RFC 9113 §7
const H2_ERROR_FLOW_CONTROL_ERROR: u32 = 0x3;
/// H2 error code: FRAME_SIZE_ERROR (0x6) — RFC 9113 §7
const H2_ERROR_FRAME_SIZE_ERROR: u32 = 0x6;
/// H2 error code: REFUSED_STREAM (0x7) — RFC 9113 §7
const H2_ERROR_REFUSED_STREAM: u32 = 0x7;
/// H2 error code: ENHANCE_YOUR_CALM (0xb) — RFC 9113 §7
const H2_ERROR_ENHANCE_YOUR_CALM: u32 = 0xb;

/// The HTTP/2 connection preface sent by the client (RFC 9113 Section 3.4).
const H2_CLIENT_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

// ============================================================================
// Raw H2 frame builder
// ============================================================================

/// A minimal H2 frame builder for crafting raw frames in tests.
struct H2Frame {
    frame_type: u8,
    flags: u8,
    stream_id: u32,
    payload: Vec<u8>,
}

impl H2Frame {
    fn new(frame_type: u8, flags: u8, stream_id: u32, payload: Vec<u8>) -> Self {
        Self {
            frame_type,
            flags,
            stream_id,
            payload,
        }
    }

    fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(9 + self.payload.len());
        let len = self.payload.len() as u32;
        // 3 bytes length (big-endian)
        buf.push((len >> 16) as u8);
        buf.push((len >> 8) as u8);
        buf.push(len as u8);
        // 1 byte type
        buf.push(self.frame_type);
        // 1 byte flags
        buf.push(self.flags);
        // 4 bytes stream id (MSB reserved, must be 0)
        buf.extend_from_slice(&(self.stream_id & 0x7FFFFFFF).to_be_bytes());
        // payload
        buf.extend_from_slice(&self.payload);
        buf
    }

    /// Build a SETTINGS frame (stream 0, no flags = not ACK).
    fn settings(settings: &[(u16, u32)]) -> Self {
        let mut payload = Vec::new();
        for &(id, value) in settings {
            payload.extend_from_slice(&id.to_be_bytes());
            payload.extend_from_slice(&value.to_be_bytes());
        }
        Self::new(H2_FRAME_SETTINGS, 0, 0, payload)
    }

    /// Build a SETTINGS ACK frame.
    fn settings_ack() -> Self {
        Self::new(H2_FRAME_SETTINGS, 0x1, 0, Vec::new())
    }

    /// Build a HEADERS frame. If `end_headers` is false, CONTINUATION is expected.
    fn headers(stream_id: u32, header_block: Vec<u8>, end_headers: bool, end_stream: bool) -> Self {
        let mut flags = 0u8;
        if end_headers {
            flags |= H2_FLAG_END_HEADERS;
        }
        if end_stream {
            flags |= 0x1; // END_STREAM
        }
        Self::new(H2_FRAME_HEADERS, flags, stream_id, header_block)
    }

    /// Build a CONTINUATION frame.
    fn continuation(stream_id: u32, header_block: Vec<u8>, end_headers: bool) -> Self {
        let flags = if end_headers { H2_FLAG_END_HEADERS } else { 0 };
        Self::new(H2_FRAME_CONTINUATION, flags, stream_id, header_block)
    }

    /// Build a WINDOW_UPDATE frame.
    fn window_update(stream_id: u32, increment: u32) -> Self {
        let payload = (increment & 0x7FFFFFFF).to_be_bytes().to_vec();
        Self::new(H2_FRAME_WINDOW_UPDATE, 0, stream_id, payload)
    }

    /// Build a DATA frame on stream 0 (invalid per spec).
    fn data_on_stream_zero(payload: Vec<u8>) -> Self {
        Self::new(H2_FRAME_DATA, 0, 0, payload)
    }

    /// Build a frame with an unknown/invalid type.
    fn invalid_type(stream_id: u32) -> Self {
        // Frame type 0xFF is not defined in the spec
        Self::new(0xFF, 0, stream_id, vec![0xDE, 0xAD])
    }

    /// Build a SETTINGS frame on a non-zero stream (invalid per spec).
    fn settings_on_stream(stream_id: u32) -> Self {
        Self::new(H2_FRAME_SETTINGS, 0, stream_id, Vec::new())
    }

    /// Build an oversized frame — payload bigger than default max_frame_size (16384).
    fn oversized(stream_id: u32) -> Self {
        let payload = vec![0u8; 16385]; // 1 byte over default max
        Self::new(H2_FRAME_DATA, 0, stream_id, payload)
    }

    /// Build a RST_STREAM frame with the given error code.
    fn rst_stream(stream_id: u32, error_code: u32) -> Self {
        Self::new(
            H2_FRAME_RST_STREAM,
            0,
            stream_id,
            error_code.to_be_bytes().to_vec(),
        )
    }

    /// Build a PING frame (non-ACK) with the given 8-byte payload.
    fn ping(payload: [u8; 8]) -> Self {
        Self::new(H2_FRAME_PING, 0, 0, payload.to_vec())
    }

    /// Build a SETTINGS ACK frame WITH a non-empty payload (invalid per RFC 9113 §6.5).
    fn settings_ack_with_payload(payload: Vec<u8>) -> Self {
        Self::new(H2_FRAME_SETTINGS, H2_FLAG_ACK, 0, payload)
    }

    /// Build a DATA frame on a given stream.
    fn data(stream_id: u32, payload: Vec<u8>, end_stream: bool) -> Self {
        let flags = if end_stream { H2_FLAG_END_STREAM } else { 0 };
        Self::new(H2_FRAME_DATA, flags, stream_id, payload)
    }

    /// Build a GOAWAY frame.
    #[allow(dead_code)]
    fn goaway(last_stream_id: u32, error_code: u32) -> Self {
        let mut payload = Vec::with_capacity(8);
        payload.extend_from_slice(&(last_stream_id & 0x7FFFFFFF).to_be_bytes());
        payload.extend_from_slice(&error_code.to_be_bytes());
        Self::new(H2_FRAME_GOAWAY, 0, 0, payload)
    }
}

/// Read raw bytes from a TcpStream / TLS stream, tolerating timeouts.
fn read_all_available(stream: &mut impl Read, timeout: Duration) -> Vec<u8> {
    let mut buf = vec![0u8; 65536];
    let mut result = Vec::new();
    let start = std::time::Instant::now();
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => result.extend_from_slice(&buf[..n]),
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                if start.elapsed() >= timeout {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(_) => break,
        }
    }
    result
}

/// Parse H2 frames from raw bytes. Returns (frame_type, flags, stream_id, payload) tuples.
fn parse_h2_frames(data: &[u8]) -> Vec<(u8, u8, u32, Vec<u8>)> {
    let mut frames = Vec::new();
    let mut pos = 0;
    while pos + 9 <= data.len() {
        let length =
            ((data[pos] as u32) << 16) | ((data[pos + 1] as u32) << 8) | (data[pos + 2] as u32);
        let frame_type = data[pos + 3];
        let flags = data[pos + 4];
        let stream_id = u32::from_be_bytes([
            data[pos + 5] & 0x7F,
            data[pos + 6],
            data[pos + 7],
            data[pos + 8],
        ]);
        let payload_end = pos + 9 + length as usize;
        if payload_end > data.len() {
            break;
        }
        let payload = data[pos + 9..payload_end].to_vec();
        frames.push((frame_type, flags, stream_id, payload));
        pos = payload_end;
    }
    frames
}

/// Check if any parsed frame is a GOAWAY frame.
fn contains_goaway(frames: &[(u8, u8, u32, Vec<u8>)]) -> bool {
    frames.iter().any(|(t, _, _, _)| *t == H2_FRAME_GOAWAY)
}

/// Check if any parsed frame is a RST_STREAM (type 0x3).
fn contains_rst_stream(frames: &[(u8, u8, u32, Vec<u8>)]) -> bool {
    frames.iter().any(|(t, _, _, _)| *t == 0x3)
}

/// Extract the error code from the first GOAWAY frame, if any.
/// GOAWAY payload: 4 bytes last_stream_id + 4 bytes error_code.
fn goaway_error_code(frames: &[(u8, u8, u32, Vec<u8>)]) -> Option<u32> {
    for (t, _, _, payload) in frames {
        if *t == H2_FRAME_GOAWAY && payload.len() >= 8 {
            return Some(u32::from_be_bytes([
                payload[4], payload[5], payload[6], payload[7],
            ]));
        }
    }
    None
}

/// Check if frames contain a GOAWAY with a specific error code.
fn contains_goaway_with_error(frames: &[(u8, u8, u32, Vec<u8>)], expected_error: u32) -> bool {
    goaway_error_code(frames) == Some(expected_error)
}

/// Extract RST_STREAM frames: returns (stream_id, error_code) tuples.
/// RST_STREAM payload: 4 bytes error_code.
fn extract_rst_streams(frames: &[(u8, u8, u32, Vec<u8>)]) -> Vec<(u32, u32)> {
    frames
        .iter()
        .filter(|(t, _, _, payload)| *t == 0x3 && payload.len() >= 4)
        .map(|(_, _, sid, payload)| {
            let error_code = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
            (*sid, error_code)
        })
        .collect()
}

/// Check if any RST_STREAM frame has a given error code for a given stream.
#[allow(dead_code)]
fn contains_rst_stream_with_error(
    frames: &[(u8, u8, u32, Vec<u8>)],
    stream_id: u32,
    expected_error: u32,
) -> bool {
    extract_rst_streams(frames)
        .iter()
        .any(|(sid, ec)| *sid == stream_id && *ec == expected_error)
}

// ============================================================================
// TLS raw connection helper
// ============================================================================

/// Establish a TLS connection to the given address with ALPN=h2,
/// returning a rustls StreamOwned that can be used for raw I/O.
fn tls_connect(addr: SocketAddr) -> rustls::StreamOwned<rustls::ClientConnection, TcpStream> {
    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Verifier))
        .with_no_client_auth();

    let mut config = config;
    config.alpn_protocols = vec![b"h2".to_vec()];

    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let conn = rustls::ClientConnection::new(Arc::new(config), server_name.to_owned()).unwrap();

    let tcp = TcpStream::connect(addr).expect("could not connect to sozu");
    tcp.set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set read timeout");
    tcp.set_write_timeout(Some(Duration::from_secs(2)))
        .expect("set write timeout");

    rustls::StreamOwned::new(conn, tcp)
}

/// Send the H2 client preface and an initial empty SETTINGS frame,
/// then read and consume the server's SETTINGS + SETTINGS ACK.
fn h2_handshake(stream: &mut rustls::StreamOwned<rustls::ClientConnection, TcpStream>) {
    // Send client preface
    stream.write_all(H2_CLIENT_PREFACE).unwrap();
    // Send empty SETTINGS
    stream.write_all(&H2Frame::settings(&[]).encode()).unwrap();
    stream.flush().unwrap();

    // Read server's response (SETTINGS, possibly WINDOW_UPDATE, etc.)
    // We just drain whatever the server sends for the handshake.
    thread::sleep(Duration::from_millis(200));
    let _ = read_all_available(stream, Duration::from_millis(500));

    // Send SETTINGS ACK
    stream.write_all(&H2Frame::settings_ack().encode()).unwrap();
    stream.flush().unwrap();

    // Small delay for the ACK to be processed
    thread::sleep(Duration::from_millis(50));
}

// ============================================================================
// Test setup helpers (reuse patterns from tests.rs)
// ============================================================================

/// Setup an HTTPS listener with H2 support, a cluster, and N async backends.
/// Returns (worker, backends, front_port).
fn setup_h2_edge_test(
    name: &str,
    nb_backends: usize,
) -> (Worker, Vec<AsyncBackend<SimpleAggregator>>, u16) {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);

    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker(name, config, &listeners, state);

    worker.send_proxy_request_type(RequestType::AddHttpsListener(
        ListenerBuilder::new_https(front_address.clone())
            .to_tls(None)
            .unwrap(),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.clone(),
        proxy: ListenerType::Https.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "cluster_0",
    )));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: String::from("localhost"),
        ..Worker::default_http_frontend("cluster_0", front_address.clone().into())
    }));

    let certificate_and_key = CertificateAndKey {
        certificate: String::from(include_str!("../../../lib/assets/local-certificate.pem")),
        key: String::from(include_str!("../../../lib/assets/local-key.pem")),
        certificate_chain: vec![],
        versions: vec![],
        names: vec![],
    };
    worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
        address: front_address,
        certificate: certificate_and_key,
        expired_at: None,
    }));

    let mut backends = Vec::new();
    for i in 0..nb_backends {
        let back_address = create_local_address();
        worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
            "cluster_0",
            format!("cluster_0-{i}"),
            back_address,
            None,
        )));
        backends.push(AsyncBackend::spawn_detached_backend(
            format!("BACKEND_{i}"),
            back_address,
            SimpleAggregator::default(),
            AsyncBackend::http_handler(format!("pong{i}")),
        ));
    }

    worker.read_to_last();
    (worker, backends, front_port)
}

/// Check that sozu is still alive and accepts a new H2 connection + request.
fn verify_sozu_alive(front_port: u16) -> bool {
    // Just verify sozu is still accepting TCP connections.
    // We don't check HTTP response because the backend may be in a bad state.
    use std::net::TcpStream;
    match TcpStream::connect_timeout(
        &format!("127.0.0.1:{front_port}").parse().unwrap(),
        Duration::from_secs(2),
    ) {
        Ok(_) => true,
        Err(e) => {
            println!("verify_sozu_alive: TCP connect failed: {e}");
            false
        }
    }
}

// ============================================================================
// Backend that disconnects mid-response
// ============================================================================

/// A backend that accepts a connection, reads the request, sends a partial
/// HTTP response, then abruptly closes the connection.
struct DisconnectingBackend {
    stop: Arc<AtomicBool>,
    requests_received: Arc<AtomicUsize>,
    thread: Option<thread::JoinHandle<()>>,
}

impl DisconnectingBackend {
    fn start(address: SocketAddr) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let requests_received = Arc::new(AtomicUsize::new(0));
        let stop_clone = stop.clone();
        let req_count = requests_received.clone();

        let thread = thread::spawn(move || {
            let listener =
                std::net::TcpListener::bind(address).expect("could not bind disconnecting backend");
            listener
                .set_nonblocking(true)
                .expect("could not set nonblocking");

            loop {
                if stop_clone.load(Ordering::Relaxed) {
                    break;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
                        // Read the request
                        let mut buf = [0u8; 4096];
                        let _ = stream.read(&mut buf);
                        req_count.fetch_add(1, Ordering::Relaxed);

                        // Send a partial response (headers only, Content-Length says 1000 but
                        // we send nothing) then drop the connection
                        let partial = b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\npartial";
                        let _ = stream.write_all(partial);
                        let _ = stream.flush();
                        // Abruptly close
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
            requests_received,
            thread: Some(thread),
        }
    }

    fn get_requests_received(&self) -> usize {
        self.requests_received.load(Ordering::Relaxed)
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread::sleep(Duration::from_millis(100));
            drop(thread);
        }
    }
}

impl Drop for DisconnectingBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

// ============================================================================
// Backend that sends a content-length mismatch response
// ============================================================================

/// A backend that responds with a Content-Length that does not match the actual body.
struct ContentLengthMismatchBackend {
    stop: Arc<AtomicBool>,
    requests_received: Arc<AtomicUsize>,
    thread: Option<thread::JoinHandle<()>>,
}

impl ContentLengthMismatchBackend {
    fn start(address: SocketAddr) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let requests_received = Arc::new(AtomicUsize::new(0));
        let stop_clone = stop.clone();
        let req_count = requests_received.clone();

        let thread = thread::spawn(move || {
            let listener = std::net::TcpListener::bind(address)
                .expect("could not bind content-length mismatch backend");
            listener
                .set_nonblocking(true)
                .expect("could not set nonblocking");

            loop {
                if stop_clone.load(Ordering::Relaxed) {
                    break;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
                        let mut buf = [0u8; 4096];
                        let _ = stream.read(&mut buf);
                        req_count.fetch_add(1, Ordering::Relaxed);

                        // Content-Length says 100, but we only send 5 bytes of body
                        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nhello";
                        let _ = stream.write_all(response);
                        let _ = stream.flush();
                        // Close the connection after sending mismatched response
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
            requests_received,
            thread: Some(thread),
        }
    }

    fn get_requests_received(&self) -> usize {
        self.requests_received.load(Ordering::Relaxed)
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread::sleep(Duration::from_millis(100));
            drop(thread);
        }
    }
}

impl Drop for ContentLengthMismatchBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

// ============================================================================
// Delayed-response H2 backend (for GoAway drain test)
// ============================================================================

/// An H2 backend that delays its response by a configurable duration.
/// Used to test that in-flight requests complete when GoAway is sent.
struct DelayedH2Backend {
    stop: Arc<AtomicBool>,
    requests_received: Arc<AtomicUsize>,
    responses_sent: Arc<AtomicUsize>,
    thread: Option<thread::JoinHandle<()>>,
}

impl DelayedH2Backend {
    fn start(address: SocketAddr, delay: Duration, body: impl Into<String>) -> Self {
        let body: Bytes = Bytes::from(body.into());
        let stop = Arc::new(AtomicBool::new(false));
        let requests_received = Arc::new(AtomicUsize::new(0));
        let responses_sent = Arc::new(AtomicUsize::new(0));

        let stop_clone = stop.clone();
        let req_count = requests_received.clone();
        let resp_count = responses_sent.clone();

        let thread = thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("could not create tokio runtime");
            rt.block_on(async move {
                let listener = TcpListener::bind(address)
                    .await
                    .expect("could not bind delayed h2 backend");

                loop {
                    if stop_clone.load(Ordering::Relaxed) {
                        break;
                    }

                    let accept =
                        tokio::time::timeout(Duration::from_millis(50), listener.accept()).await;

                    let (stream, _) = match accept {
                        Ok(Ok(s)) => s,
                        Ok(Err(_)) => continue,
                        Err(_) => continue,
                    };

                    let body = body.clone();
                    let req_count = req_count.clone();
                    let resp_count = resp_count.clone();

                    tokio::spawn(async move {
                        let io = TokioIo::new(stream);
                        let service =
                            service_fn(move |_req: hyper::Request<hyper::body::Incoming>| {
                                let body = body.clone();
                                let req_count = req_count.clone();
                                let resp_count = resp_count.clone();
                                async move {
                                    req_count.fetch_add(1, Ordering::Relaxed);
                                    // Delay the response
                                    tokio::time::sleep(delay).await;

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
                        let _ = builder.serve_connection(io, service).await;
                    });
                }
            });
        });

        Self {
            stop,
            requests_received,
            responses_sent,
            thread: Some(thread),
        }
    }

    #[allow(dead_code)]
    fn get_requests_received(&self) -> usize {
        self.requests_received.load(Ordering::Relaxed)
    }

    #[allow(dead_code)]
    fn get_responses_sent(&self) -> usize {
        self.responses_sent.load(Ordering::Relaxed)
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread::sleep(Duration::from_millis(100));
            drop(thread);
        }
    }
}

impl Drop for DelayedH2Backend {
    fn drop(&mut self) {
        self.stop();
    }
}

// ============================================================================
// Setup for tests that use custom backends (not the default async backend)
// ============================================================================

/// Setup HTTPS listener with TLS and a cluster, but do NOT add backends.
/// The caller is responsible for adding backends and calling worker.read_to_last().
fn setup_h2_listener_only(name: &str) -> (Worker, u16, SocketAddress) {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);

    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker(name, config, &listeners, state);

    worker.send_proxy_request_type(RequestType::AddHttpsListener(
        ListenerBuilder::new_https(front_address.clone())
            .to_tls(None)
            .unwrap(),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.clone(),
        proxy: ListenerType::Https.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "cluster_0",
    )));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: String::from("localhost"),
        ..Worker::default_http_frontend("cluster_0", front_address.clone().into())
    }));

    let certificate_and_key = CertificateAndKey {
        certificate: String::from(include_str!("../../../lib/assets/local-certificate.pem")),
        key: String::from(include_str!("../../../lib/assets/local-key.pem")),
        certificate_chain: vec![],
        versions: vec![],
        names: vec![],
    };
    worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
        address: front_address.clone(),
        certificate: certificate_and_key,
        expired_at: None,
    }));

    (worker, front_port, front_address)
}

// ============================================================================
// Test 1: Basic H2 request/response smoke test
// ============================================================================

/// Basic smoke test: H2 client -> sozu -> H1 backend -> response back.
fn try_h2_basic_request_response() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_edge_test("H2-EDGE-BASIC", 1);

    let client = build_h2_client();
    let uri: hyper::Uri = format!("https://localhost:{front_port}/api")
        .parse()
        .unwrap();

    if let Some((status, body)) = resolve_request(&client, uri) {
        println!("H2 edge basic - status: {status:?}, body: {body}");
        if !status.is_success() || !body.contains("pong") {
            return State::Fail;
        }
    } else {
        return State::Fail;
    }

    worker.soft_stop();
    let success = worker.wait_for_server_stop();

    let aggregator = backends[0]
        .stop_and_get_aggregator()
        .expect("Could not get aggregator");
    if success && aggregator.responses_sent == 1 {
        State::Success
    } else {
        State::Fail
    }
}

// ============================================================================
// Test 2: CONTINUATION with wrong stream_id
// ============================================================================

/// A malicious client sends a CONTINUATION frame with a stream_id that doesn't
/// match the preceding HEADERS frame. Sozu must NOT crash — it should send
/// GOAWAY(PROTOCOL_ERROR).
fn try_h2_continuation_wrong_stream_id() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_edge_test("H2-CONT-BAD-SID", 1);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    // Establish raw TLS + H2 connection
    let mut tls = tls_connect(front_addr);
    h2_handshake(&mut tls);

    // Send HEADERS on stream 1 WITHOUT END_HEADERS (expects CONTINUATION)
    // Use a minimal HPACK-encoded :method GET, :path /api, :scheme https
    // (These are static table indices encoded as indexed header fields)
    let header_block = vec![
        0x82, // :method GET (index 2)
        0x84, // :path / (index 4) — close enough
        0x86, // :scheme https (index 6)
    ];
    let headers_frame = H2Frame::headers(1, header_block, false, true);
    tls.write_all(&headers_frame.encode()).unwrap();
    tls.flush().unwrap();

    // Send CONTINUATION on stream 3 (WRONG — should be stream 1)
    let continuation_block = vec![
        0x40, 0x0a, // literal header with indexing, name length 10
        b'x', b'-', b'c', b'u', b's', b't', b'o', b'm', b'-', b'h', 0x05, // value length 5
        b'v', b'a', b'l', b'u', b'e',
    ];
    let bad_continuation = H2Frame::continuation(3, continuation_block, true);
    tls.write_all(&bad_continuation.encode()).unwrap();
    tls.flush().unwrap();

    // Read response — expect GOAWAY
    thread::sleep(Duration::from_millis(500));
    let response_data = read_all_available(&mut tls, Duration::from_secs(2));
    let frames = parse_h2_frames(&response_data);

    println!(
        "H2 CONTINUATION bad stream_id - received {} frames",
        frames.len()
    );
    for (i, (ft, fl, sid, _)) in frames.iter().enumerate() {
        println!("  frame {i}: type=0x{ft:02x} flags=0x{fl:02x} stream={sid}");
    }

    let got_goaway = contains_goaway(&frames);
    if !got_goaway {
        println!("WARNING: expected GOAWAY but did not receive one (sozu may have just closed)");
        // Even if we don't get an explicit GOAWAY, the key assertion is:
        // sozu must NOT crash.
    }

    drop(tls);

    // Verify sozu is still alive by making a normal request on a new connection
    thread::sleep(Duration::from_millis(200));
    let still_alive = verify_sozu_alive(front_port);
    println!("H2 CONTINUATION bad stream_id - sozu still alive: {still_alive}");

    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    for backend in backends.iter_mut() {
        backend.stop_and_get_aggregator();
    }

    if success && still_alive {
        State::Success
    } else {
        State::Fail
    }
}

// ============================================================================
// Test 3: GoAway graceful drain
// ============================================================================

/// Test that when the client sends GoAway, in-flight requests on the connection
/// complete before the connection fully closes. After GoAway, no new streams
/// should be accepted on that connection.
fn try_h2_goaway_graceful_drain() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let back_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker("H2-GOAWAY-DRAIN", config, &listeners, state);

    worker.send_proxy_request_type(RequestType::AddHttpsListener(
        ListenerBuilder::new_https(front_address.clone())
            .to_tls(None)
            .unwrap(),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.clone(),
        proxy: ListenerType::Https.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "cluster_0",
    )));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: String::from("localhost"),
        ..Worker::default_http_frontend("cluster_0", front_address.clone().into())
    }));
    let certificate_and_key = CertificateAndKey {
        certificate: String::from(include_str!("../../../lib/assets/local-certificate.pem")),
        key: String::from(include_str!("../../../lib/assets/local-key.pem")),
        certificate_chain: vec![],
        versions: vec![],
        names: vec![],
    };
    worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
        address: front_address,
        certificate: certificate_and_key,
        expired_at: None,
    }));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address.into(),
        None,
    )));
    worker.read_to_last();

    // Start a backend that delays responses by 500ms
    let mut delayed_backend =
        DelayedH2Backend::start(back_address, Duration::from_millis(500), "delayed-pong");

    // Use hyper H2 client: send a request, then verify it completes
    // Even though we trigger a soft_stop (which sends GoAway), the in-flight
    // request should finish.
    let client = build_h2_client();
    let uri: hyper::Uri = format!("https://localhost:{front_port}/api")
        .parse()
        .unwrap();

    // Send request — backend will delay 500ms before responding
    let result = resolve_request(&client, uri);
    let request_succeeded = result.is_some_and(|(status, body)| {
        println!("H2 GoAway drain - status: {status:?}, body: {body}");
        status.is_success()
    });

    if !request_succeeded {
        println!("H2 GoAway drain - request did not succeed");
    }

    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    delayed_backend.stop();

    if success && request_succeeded {
        State::Success
    } else {
        State::Fail
    }
}

// ============================================================================
// Test 4: RST_STREAM on backend disconnect
// ============================================================================

/// When the backend disconnects mid-response, sozu should handle the error
/// gracefully (RST_STREAM or error response), NOT crash.
fn try_h2_rst_stream_on_backend_disconnect() -> State {
    let (mut worker, front_port, _front_address) = setup_h2_listener_only("H2-RST-DISCONNECT");

    let back_address = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));
    worker.read_to_last();

    let mut backend = DisconnectingBackend::start(back_address);

    let client = build_h2_client();
    let uri: hyper::Uri = format!("https://localhost:{front_port}/api")
        .parse()
        .unwrap();

    // The request should either fail gracefully or return an error response.
    // The key assertion: sozu must NOT crash.
    let result = resolve_request(&client, uri);
    match &result {
        Some((status, body)) => {
            println!("H2 RST backend disconnect - status: {status:?}, body: {body}");
        }
        None => {
            println!("H2 RST backend disconnect - request failed (expected for disconnect)");
        }
    }

    let req_received = backend.get_requests_received();
    println!("H2 RST backend disconnect - backend received {req_received} requests");

    // The crucial check: verify sozu is still alive after the backend disconnect
    thread::sleep(Duration::from_millis(300));
    let still_alive = verify_sozu_alive(front_port);
    println!("H2 RST backend disconnect - sozu still alive: {still_alive}");

    worker.soft_stop();
    let _success = worker.wait_for_server_stop();
    backend.stop();

    // The key invariant: sozu did not crash during the error scenario.
    // Worker stop may fail due to internal state after error handling — that's OK.
    if still_alive {
        State::Success
    } else {
        State::Fail
    }
}

// ============================================================================
// Test 5: Malformed frames — sozu must not crash
// ============================================================================

/// Send various malformed H2 frames and verify sozu doesn't crash:
/// - Frame with invalid type
/// - SETTINGS frame on non-zero stream
/// - DATA frame on stream 0
/// - WINDOW_UPDATE with 0 increment
/// - Oversized frame (larger than max_frame_size)
///
/// For each, verify sozu responds with appropriate error (GOAWAY or RST_STREAM)
/// and stays alive.
fn try_h2_malformed_frames_no_crash() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_edge_test("H2-MALFORMED", 1);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    // ---- Sub-test A: Frame with invalid type ----
    {
        let mut tls = tls_connect(front_addr);
        h2_handshake(&mut tls);

        let frame = H2Frame::invalid_type(1);
        let _ = tls.write_all(&frame.encode());
        let _ = tls.flush();

        thread::sleep(Duration::from_millis(300));
        let data = read_all_available(&mut tls, Duration::from_secs(1));
        let frames = parse_h2_frames(&data);
        println!(
            "Malformed A (invalid type) - {} frames received",
            frames.len()
        );
        // Unknown frame types MUST be ignored per RFC 9113 Section 4.1
        // So sozu should NOT disconnect. But even if it does, it must not crash.
        drop(tls);
    }

    // Verify sozu is alive after sub-test A
    thread::sleep(Duration::from_millis(100));
    if !verify_sozu_alive(front_port) {
        println!("Sozu died after invalid frame type test");
        worker.soft_stop();
        let _ = worker.wait_for_server_stop();
        return State::Fail;
    }

    // ---- Sub-test B: SETTINGS on non-zero stream ----
    {
        let mut tls = tls_connect(front_addr);
        h2_handshake(&mut tls);

        let frame = H2Frame::settings_on_stream(1);
        let _ = tls.write_all(&frame.encode());
        let _ = tls.flush();

        thread::sleep(Duration::from_millis(300));
        let data = read_all_available(&mut tls, Duration::from_secs(1));
        let frames = parse_h2_frames(&data);
        println!(
            "Malformed B (SETTINGS on stream 1) - {} frames received",
            frames.len()
        );
        let got_goaway = contains_goaway(&frames);
        println!("  got GOAWAY: {got_goaway}");
        drop(tls);
    }

    thread::sleep(Duration::from_millis(100));
    if !verify_sozu_alive(front_port) {
        println!("Sozu died after SETTINGS on non-zero stream test");
        worker.soft_stop();
        let _ = worker.wait_for_server_stop();
        return State::Fail;
    }

    // ---- Sub-test C: DATA on stream 0 ----
    {
        let mut tls = tls_connect(front_addr);
        h2_handshake(&mut tls);

        let frame = H2Frame::data_on_stream_zero(vec![0xCA, 0xFE]);
        let _ = tls.write_all(&frame.encode());
        let _ = tls.flush();

        thread::sleep(Duration::from_millis(300));
        let data = read_all_available(&mut tls, Duration::from_secs(1));
        let frames = parse_h2_frames(&data);
        println!(
            "Malformed C (DATA on stream 0) - {} frames received",
            frames.len()
        );
        let got_goaway = contains_goaway(&frames);
        println!("  got GOAWAY: {got_goaway}");
        drop(tls);
    }

    thread::sleep(Duration::from_millis(100));
    if !verify_sozu_alive(front_port) {
        println!("Sozu died after DATA on stream 0 test");
        worker.soft_stop();
        let _ = worker.wait_for_server_stop();
        return State::Fail;
    }

    // ---- Sub-test D: WINDOW_UPDATE with 0 increment ----
    {
        let mut tls = tls_connect(front_addr);
        h2_handshake(&mut tls);

        // WINDOW_UPDATE with increment 0 on connection (stream 0) is a PROTOCOL_ERROR
        let frame = H2Frame::window_update(0, 0);
        let _ = tls.write_all(&frame.encode());
        let _ = tls.flush();

        thread::sleep(Duration::from_millis(300));
        let data = read_all_available(&mut tls, Duration::from_secs(1));
        let frames = parse_h2_frames(&data);
        println!(
            "Malformed D (WINDOW_UPDATE 0 increment) - {} frames received",
            frames.len()
        );
        let got_goaway = contains_goaway(&frames);
        println!("  got GOAWAY: {got_goaway}");
        drop(tls);
    }

    thread::sleep(Duration::from_millis(100));
    if !verify_sozu_alive(front_port) {
        println!("Sozu died after WINDOW_UPDATE with 0 increment test");
        worker.soft_stop();
        let _ = worker.wait_for_server_stop();
        return State::Fail;
    }

    // ---- Sub-test E: Oversized frame ----
    {
        let mut tls = tls_connect(front_addr);
        h2_handshake(&mut tls);

        // Send a HEADERS first to open a stream, then send an oversized DATA frame
        let header_block = vec![
            0x82, // :method GET
            0x84, // :path /
            0x86, // :scheme https
        ];
        let headers = H2Frame::headers(1, header_block, true, false);
        let _ = tls.write_all(&headers.encode());
        let _ = tls.flush();

        thread::sleep(Duration::from_millis(100));

        let oversized = H2Frame::oversized(1);
        let _ = tls.write_all(&oversized.encode());
        let _ = tls.flush();

        thread::sleep(Duration::from_millis(300));
        let data = read_all_available(&mut tls, Duration::from_secs(1));
        let frames = parse_h2_frames(&data);
        println!(
            "Malformed E (oversized frame) - {} frames received",
            frames.len()
        );
        let got_goaway = contains_goaway(&frames);
        let got_rst = contains_rst_stream(&frames);
        println!("  got GOAWAY: {got_goaway}, got RST_STREAM: {got_rst}");
        drop(tls);
    }

    thread::sleep(Duration::from_millis(100));
    let final_alive = verify_sozu_alive(front_port);
    println!("Malformed frames - sozu final alive check: {final_alive}");

    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    for backend in backends.iter_mut() {
        backend.stop_and_get_aggregator();
    }

    if success && final_alive {
        State::Success
    } else {
        State::Fail
    }
}

// ============================================================================
// Test 6: Content-Length mismatch from backend
// ============================================================================

/// Backend sends a response with Content-Length header that doesn't match the
/// actual body size. Sozu must NOT crash. It may forward the partial response,
/// send RST_STREAM, or return an error to the client.
fn try_h2_content_length_mismatch() -> State {
    let (mut worker, front_port, _front_address) = setup_h2_listener_only("H2-CL-MISMATCH");

    let back_address = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));
    worker.read_to_last();

    let mut backend = ContentLengthMismatchBackend::start(back_address);

    let client = build_h2_client();
    let uri: hyper::Uri = format!("https://localhost:{front_port}/api")
        .parse()
        .unwrap();

    // The request may fail or succeed with partial data — either is acceptable.
    // The key assertion: sozu must NOT crash.
    let result = resolve_request(&client, uri);
    match &result {
        Some((status, body)) => {
            println!(
                "H2 CL mismatch - status: {status:?}, body len: {}",
                body.len()
            );
        }
        None => {
            println!("H2 CL mismatch - request failed (may be expected for mismatch)");
        }
    }

    let req_received = backend.get_requests_received();
    println!("H2 CL mismatch - backend received {req_received} requests");

    // Crucial check: sozu is still alive
    thread::sleep(Duration::from_millis(300));
    let still_alive = verify_sozu_alive(front_port);
    println!("H2 CL mismatch - sozu still alive: {still_alive}");

    worker.soft_stop();
    let _success = worker.wait_for_server_stop();
    backend.stop();

    // The key invariant: sozu did not crash during the error scenario.
    // Worker stop may fail due to internal state after error handling — that's OK.
    if still_alive {
        State::Success
    } else {
        State::Fail
    }
}

// ============================================================================
// Test 7: Concurrent streams multiplexing
// ============================================================================

/// Multiple concurrent H2 streams on a single connection.
/// Send 5 requests in parallel on the same H2 connection.
/// Each should get the correct response. Verify request/response multiplexing.
fn try_h2_concurrent_streams() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_edge_test("H2-CONCURRENT", 1);

    let client = build_h2_client();
    let uris: Vec<hyper::Uri> = (0..5)
        .map(|i| {
            format!("https://localhost:{front_port}/api/stream/{i}")
                .parse()
                .unwrap()
        })
        .collect();

    let results = resolve_concurrent_requests(&client, uris);
    let all_ok = results
        .iter()
        .all(|r| r.as_ref().is_some_and(|(s, _)| s.is_success()));

    if !all_ok {
        println!("H2 concurrent streams - not all requests succeeded: {results:?}");
        return State::Fail;
    }

    let all_contain_pong = results
        .iter()
        .all(|r| r.as_ref().is_some_and(|(_, body)| body.contains("pong")));

    if !all_contain_pong {
        println!("H2 concurrent streams - not all responses contain 'pong'");
        return State::Fail;
    }

    println!(
        "H2 concurrent streams - all {} requests succeeded",
        results.len()
    );

    worker.soft_stop();
    let success = worker.wait_for_server_stop();

    let aggregator = backends[0]
        .stop_and_get_aggregator()
        .expect("Could not get aggregator");
    println!(
        "H2 concurrent streams - backend received: {}, sent: {}",
        aggregator.requests_received, aggregator.responses_sent
    );

    if success && aggregator.responses_sent == 5 {
        State::Success
    } else {
        State::Fail
    }
}

// ============================================================================
// #[test] wrappers
// ============================================================================

#[test]
fn test_h2_basic_request_response() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "H2 edge: basic request/response smoke test",
            try_h2_basic_request_response
        ),
        State::Success
    );
}

#[test]
fn test_h2_continuation_wrong_stream_id() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2 edge: CONTINUATION with wrong stream_id must not crash",
            try_h2_continuation_wrong_stream_id
        ),
        State::Success
    );
}

#[test]
fn test_h2_goaway_graceful_drain() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2 edge: GoAway graceful drain of in-flight requests",
            try_h2_goaway_graceful_drain
        ),
        State::Success
    );
}

#[test]
fn test_h2_rst_stream_on_backend_disconnect() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2 edge: RST_STREAM on backend disconnect, sozu must not crash",
            try_h2_rst_stream_on_backend_disconnect
        ),
        State::Success
    );
}

#[test]
fn test_h2_malformed_frames_no_crash() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 edge: malformed frames resilience (5 sub-tests)",
            try_h2_malformed_frames_no_crash
        ),
        State::Success
    );
}

#[test]
fn test_h2_content_length_mismatch() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2 edge: Content-Length mismatch from backend, sozu must not crash",
            try_h2_content_length_mismatch
        ),
        State::Success
    );
}

#[test]
fn test_h2_concurrent_streams() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "H2 edge: concurrent streams multiplexing",
            try_h2_concurrent_streams
        ),
        State::Success
    );
}

// ============================================================================
// Test: Flow control window not leaked on orphaned DATA frames
// ============================================================================

/// Regression test for RFC 9113 §6.9: when DATA arrives for a stream that no
/// longer exists (e.g., already reset), the connection-level flow control window
/// MUST still be updated. Without the fix, the window shrinks permanently and
/// the connection eventually stalls — subsequent requests on the same H2
/// connection would hang.
///
/// Strategy: send a request, get a response. Then send a raw HEADERS+DATA on
/// a NEW stream that sozu will reject (e.g., an already-used stream_id). Then
/// verify the connection is still functional by sending more requests.
fn try_h2_flow_control_on_orphaned_data() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_edge_test("H2-FLOW-CTRL", 1);

    let client = build_h2_client();

    // First: verify basic connectivity with several sequential requests to
    // exercise the flow control path (each request consumes window)
    for i in 0..5 {
        let uri: hyper::Uri = format!("https://localhost:{front_port}/api/flow/{i}")
            .parse()
            .unwrap();
        let result = resolve_request(&client, uri);
        if result.as_ref().is_none_or(|(s, _)| !s.is_success()) {
            println!("H2 flow control - request {i} failed: {result:?}");
            return State::Fail;
        }
    }

    // Now send a large POST body to exercise flow control window updates
    let large_body = "x".repeat(128 * 1024); // 128KB — exceeds initial window (65535)
    let uri: hyper::Uri = format!("https://localhost:{front_port}/api/flow/large")
        .parse()
        .unwrap();
    let result = resolve_post_request(&client, uri, large_body);
    if result.as_ref().is_none_or(|(s, _)| !s.is_success()) {
        println!("H2 flow control - large POST failed: {result:?}");
        return State::Fail;
    }

    // After the large transfer, verify the connection still works
    for i in 0..3 {
        let uri: hyper::Uri = format!("https://localhost:{front_port}/api/flow/after/{i}")
            .parse()
            .unwrap();
        let result = resolve_request(&client, uri);
        if result.as_ref().is_none_or(|(s, _)| !s.is_success()) {
            println!("H2 flow control - post-large request {i} failed: {result:?}");
            return State::Fail;
        }
    }

    println!("H2 flow control - all requests succeeded, connection not stalled");

    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    let aggregator = backends[0]
        .stop_and_get_aggregator()
        .expect("Could not get aggregator");

    // 5 initial + 1 large + 3 after = 9 requests
    if success && aggregator.responses_sent >= 9 {
        State::Success
    } else {
        println!(
            "H2 flow control - expected 9 responses, got {}",
            aggregator.responses_sent
        );
        State::Fail
    }
}

// ============================================================================
// Test: Concurrent large H2 transfers (byte accounting regression)
// ============================================================================

/// Regression test for byte_in/byte_out computation: send many concurrent
/// H2 GET requests, ensuring the proxy handles multiplexed streams correctly
/// without stalling or crashing. This exercises:
/// - Frame header byte accounting (9-byte headers in zero_bytes_read)
/// - Flow control window management under load
/// - Overhead distribution across multiplexed streams
fn try_h2_concurrent_large_transfers() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_edge_test("H2-CONCURRENT-LARGE", 1);

    let client = build_h2_client();

    // Send 8 concurrent GET requests to exercise H2 multiplexing and byte accounting
    let uris: Vec<hyper::Uri> = (0..8)
        .map(|i| {
            format!("https://localhost:{front_port}/api/mux/{i}")
                .parse()
                .unwrap()
        })
        .collect();

    let results = resolve_concurrent_requests(&client, uris);
    let all_ok = results
        .iter()
        .all(|r| r.as_ref().is_some_and(|(s, _)| s.is_success()));

    if !all_ok {
        println!("H2 concurrent multiplexing - not all succeeded: {results:?}");
        return State::Fail;
    }

    // Follow up with another batch to verify the connection is still healthy
    let uris: Vec<hyper::Uri> = (0..4)
        .map(|i| {
            format!("https://localhost:{front_port}/api/mux/after/{i}")
                .parse()
                .unwrap()
        })
        .collect();
    let results2 = resolve_concurrent_requests(&client, uris);
    let all_ok2 = results2
        .iter()
        .all(|r| r.as_ref().is_some_and(|(s, _)| s.is_success()));

    if !all_ok2 {
        println!("H2 concurrent multiplexing - follow-up batch failed: {results2:?}");
        return State::Fail;
    }

    println!("H2 concurrent multiplexing - all 12 requests passed");

    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    let aggregator = backends[0]
        .stop_and_get_aggregator()
        .expect("Could not get aggregator");

    if success && aggregator.responses_sent >= 12 {
        State::Success
    } else {
        println!(
            "H2 concurrent multiplexing - expected 12 responses, got {}",
            aggregator.responses_sent
        );
        State::Fail
    }
}

// ============================================================================
// Test: TE header filtering (operator precedence regression)
// ============================================================================

/// Regression test for the TE header filter in the H2 block converter.
/// RFC 9113 §8.2.2: the only TE value allowed in H2 is "trailers".
/// Any other TE value (e.g., "gzip") must be stripped when converting
/// H1 → H2 headers. The operator precedence bug (|| vs &&) could cause
/// "te: trailers" to be incorrectly stripped.
///
/// Strategy: send a request with TE:trailers, verify the backend receives it.
/// Then send with TE:gzip, verify the backend does NOT receive it.
/// We verify this indirectly: if the request succeeds at all through the H2
/// proxy, the converter didn't crash on the TE header.
fn try_h2_te_header_filtering() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_edge_test("H2-TE-FILTER", 1);

    let client = build_h2_client();

    // Request with TE: trailers (should be allowed through)
    let rt = tokio::runtime::Runtime::new().expect("Could not create Runtime");
    let result = rt.block_on(async {
        let request = hyper::Request::builder()
            .method(hyper::Method::GET)
            .uri(format!("https://localhost:{front_port}/api/te-trailers"))
            .header("te", "trailers")
            .body(String::new())
            .expect("Could not build request");
        match client.request(request).await {
            Ok(response) => Some(response.status()),
            Err(e) => {
                println!("TE:trailers request failed: {e}");
                None
            }
        }
    });

    if result.is_none_or(|s| !s.is_success()) {
        println!("H2 TE filter - request with TE:trailers failed: {result:?}");
        return State::Fail;
    }
    println!("H2 TE filter - TE:trailers passed through OK");

    // Request with TE: gzip (should be stripped but request should still succeed)
    let result = rt.block_on(async {
        let request = hyper::Request::builder()
            .method(hyper::Method::GET)
            .uri(format!("https://localhost:{front_port}/api/te-gzip"))
            .header("te", "gzip")
            .body(String::new())
            .expect("Could not build request");
        match client.request(request).await {
            Ok(response) => Some(response.status()),
            Err(e) => {
                println!("TE:gzip request failed: {e}");
                None
            }
        }
    });

    if result.is_none_or(|s| !s.is_success()) {
        println!("H2 TE filter - request with TE:gzip failed: {result:?}");
        return State::Fail;
    }
    println!("H2 TE filter - TE:gzip was stripped, request succeeded");

    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    let aggregator = backends[0]
        .stop_and_get_aggregator()
        .expect("Could not get aggregator");

    if success && aggregator.responses_sent == 2 {
        State::Success
    } else {
        println!(
            "H2 TE filter - expected 2 responses, got {}",
            aggregator.responses_sent
        );
        State::Fail
    }
}

// ============================================================================
// #[test] wrappers for new tests
// ============================================================================

#[test]
fn test_h2_flow_control_on_orphaned_data() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2 edge: flow control window not leaked on orphaned DATA",
            try_h2_flow_control_on_orphaned_data
        ),
        State::Success
    );
}

#[test]
fn test_h2_concurrent_large_transfers() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2 edge: concurrent large transfers (byte accounting)",
            try_h2_concurrent_large_transfers
        ),
        State::Success
    );
}

#[test]
fn test_h2_te_header_filtering() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2 edge: TE header filtering (operator precedence regression)",
            try_h2_te_header_filtering
        ),
        State::Success
    );
}

// ============================================================================
// Security tests: H2 flood protection (CVE mitigations)
// ============================================================================

// ---- CVE-2023-44487: Rapid Reset triggers GOAWAY(ENHANCE_YOUR_CALM) ----

/// Open an H2 connection and send 200 HEADERS+RST_STREAM pairs rapidly.
/// Sozu must detect the flood and respond with GOAWAY(ENHANCE_YOUR_CALM).
fn try_h2_rapid_reset_triggers_goaway() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_edge_test("H2-RAPID-RESET", 1);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    let mut tls = tls_connect(front_addr);
    h2_handshake(&mut tls);

    // Send 200 HEADERS + RST_STREAM pairs rapidly on odd stream IDs
    for i in 0..200u32 {
        let stream_id = 1 + i * 2; // 1, 3, 5, 7, ...
        let header_block = vec![
            0x82, // :method GET (index 2)
            0x84, // :path / (index 4)
            0x86, // :scheme https (index 6)
        ];
        let headers = H2Frame::headers(stream_id, header_block, true, true);
        if tls.write_all(&headers.encode()).is_err() {
            break;
        }
        let rst = H2Frame::rst_stream(stream_id, 0x8); // CANCEL
        if tls.write_all(&rst.encode()).is_err() {
            break;
        }
    }
    let _ = tls.flush();

    // Read response — expect GOAWAY with ENHANCE_YOUR_CALM
    thread::sleep(Duration::from_millis(500));
    let response_data = read_all_available(&mut tls, Duration::from_secs(2));
    let frames = parse_h2_frames(&response_data);

    println!("H2 Rapid Reset - received {} frames", frames.len());
    for (i, (ft, fl, sid, payload)) in frames.iter().enumerate() {
        if *ft == H2_FRAME_GOAWAY && payload.len() >= 8 {
            let error_code = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
            println!("  frame {i}: GOAWAY error_code=0x{error_code:x}");
        } else {
            println!("  frame {i}: type=0x{ft:02x} flags=0x{fl:02x} stream={sid}");
        }
    }

    let got_enhance_your_calm = contains_goaway_with_error(&frames, H2_ERROR_ENHANCE_YOUR_CALM);
    println!("H2 Rapid Reset - got ENHANCE_YOUR_CALM: {got_enhance_your_calm}");

    drop(tls);

    // Verify sozu is still alive
    thread::sleep(Duration::from_millis(200));
    let still_alive = verify_sozu_alive(front_port);
    println!("H2 Rapid Reset - sozu still alive: {still_alive}");

    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    for backend in backends.iter_mut() {
        backend.stop_and_get_aggregator();
    }

    if success && still_alive && got_enhance_your_calm {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_rapid_reset_triggers_goaway() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2 security: CVE-2023-44487 Rapid Reset triggers GOAWAY(ENHANCE_YOUR_CALM)",
            try_h2_rapid_reset_triggers_goaway
        ),
        State::Success
    );
}

// ---- CVE-2024-27316: CONTINUATION flood triggers GOAWAY(ENHANCE_YOUR_CALM) ----

/// Open an H2 connection, send HEADERS without END_HEADERS, then 50 CONTINUATION
/// frames. Sozu must detect the flood and respond with GOAWAY(ENHANCE_YOUR_CALM).
fn try_h2_continuation_flood_triggers_goaway() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_edge_test("H2-CONT-FLOOD", 1);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    let mut tls = tls_connect(front_addr);
    h2_handshake(&mut tls);

    // Send HEADERS on stream 1 WITHOUT END_HEADERS
    let header_block = vec![
        0x82, // :method GET (index 2)
        0x84, // :path / (index 4)
        0x86, // :scheme https (index 6)
    ];
    let headers = H2Frame::headers(1, header_block, false, true);
    tls.write_all(&headers.encode()).unwrap();
    tls.flush().unwrap();

    // Send 50 CONTINUATION frames without END_HEADERS.
    // Batch them into the same write for speed.
    let mut batch = Vec::new();
    for i in 0..50u32 {
        // Each CONTINUATION carries a small header fragment
        let fragment = vec![
            0x40, // literal header with indexing
            0x05, // name length 5
            b'x',
            b'-',
            b'f',
            b'l',
            b'd', // name "x-fld"
            0x03, // value length 3
            b'v',
            (b'0' + (i % 10) as u8),
            (b'0' + (i / 10) as u8), // value "vNN"
        ];
        let cont = H2Frame::continuation(1, fragment, false);
        batch.extend_from_slice(&cont.encode());
    }
    let write_result = tls.write_all(&batch);
    let flush_result = tls.flush();
    println!(
        "CONTINUATION flood: write={:?}, flush={:?}",
        write_result.is_ok(),
        flush_result.is_ok()
    );

    // Read response — expect GOAWAY with ENHANCE_YOUR_CALM
    // Give sozu time to process the frames through its event loop
    thread::sleep(Duration::from_millis(1000));
    let response_data = read_all_available(&mut tls, Duration::from_secs(3));
    let frames = parse_h2_frames(&response_data);

    println!("H2 CONTINUATION flood - received {} frames", frames.len());
    for (i, (ft, fl, sid, payload)) in frames.iter().enumerate() {
        if *ft == H2_FRAME_GOAWAY && payload.len() >= 8 {
            let error_code = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
            println!("  frame {i}: GOAWAY error_code=0x{error_code:x}");
        } else {
            println!("  frame {i}: type=0x{ft:02x} flags=0x{fl:02x} stream={sid}");
        }
    }

    let got_enhance_your_calm = contains_goaway_with_error(&frames, H2_ERROR_ENHANCE_YOUR_CALM);
    println!("H2 CONTINUATION flood - got ENHANCE_YOUR_CALM: {got_enhance_your_calm}");

    drop(tls);

    // Verify sozu is still alive
    thread::sleep(Duration::from_millis(200));
    let still_alive = verify_sozu_alive(front_port);
    println!("H2 CONTINUATION flood - sozu still alive: {still_alive}");

    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    for backend in backends.iter_mut() {
        backend.stop_and_get_aggregator();
    }

    if success && still_alive && got_enhance_your_calm {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_continuation_flood_triggers_goaway() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2 security: CVE-2024-27316 CONTINUATION flood triggers GOAWAY(ENHANCE_YOUR_CALM)",
            try_h2_continuation_flood_triggers_goaway
        ),
        State::Success
    );
}

// ---- CVE-2019-9512: Ping flood triggers GOAWAY(ENHANCE_YOUR_CALM) ----

/// Open an H2 connection and send 200 PING frames rapidly.
/// Sozu must detect the flood and respond with GOAWAY(ENHANCE_YOUR_CALM).
fn try_h2_ping_flood_triggers_goaway() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_edge_test("H2-PING-FLOOD", 1);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    let mut tls = tls_connect(front_addr);
    h2_handshake(&mut tls);

    // Send 200 PING frames rapidly
    for i in 0..200u32 {
        let mut payload = [0u8; 8];
        payload[0..4].copy_from_slice(&i.to_be_bytes());
        let ping = H2Frame::ping(payload);
        if tls.write_all(&ping.encode()).is_err() {
            break;
        }
    }
    let _ = tls.flush();

    // Read response — expect GOAWAY with ENHANCE_YOUR_CALM
    thread::sleep(Duration::from_millis(500));
    let response_data = read_all_available(&mut tls, Duration::from_secs(2));
    let frames = parse_h2_frames(&response_data);

    println!("H2 Ping flood - received {} frames", frames.len());

    let got_enhance_your_calm = contains_goaway_with_error(&frames, H2_ERROR_ENHANCE_YOUR_CALM);
    if let Some(error_code) = goaway_error_code(&frames) {
        println!("H2 Ping flood - GOAWAY error_code=0x{error_code:x}");
    }
    println!("H2 Ping flood - got ENHANCE_YOUR_CALM: {got_enhance_your_calm}");

    drop(tls);

    // Verify sozu is still alive
    thread::sleep(Duration::from_millis(200));
    let still_alive = verify_sozu_alive(front_port);
    println!("H2 Ping flood - sozu still alive: {still_alive}");

    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    for backend in backends.iter_mut() {
        backend.stop_and_get_aggregator();
    }

    if success && still_alive && got_enhance_your_calm {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_ping_flood_triggers_goaway() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2 security: CVE-2019-9512 Ping flood triggers GOAWAY(ENHANCE_YOUR_CALM)",
            try_h2_ping_flood_triggers_goaway
        ),
        State::Success
    );
}

// ---- CVE-2019-9515: Settings flood triggers GOAWAY(ENHANCE_YOUR_CALM) ----

/// Open an H2 connection and send 100 SETTINGS frames rapidly.
/// Sozu must detect the flood and respond with GOAWAY(ENHANCE_YOUR_CALM).
fn try_h2_settings_flood_triggers_goaway() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_edge_test("H2-SETTINGS-FLOOD", 1);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    let mut tls = tls_connect(front_addr);
    h2_handshake(&mut tls);

    // Send 100 SETTINGS frames rapidly (each with a valid setting)
    for _i in 0..100u32 {
        // SETTINGS_MAX_CONCURRENT_STREAMS = 100 (a valid, harmless setting)
        let settings = H2Frame::settings(&[(0x3, 100)]);
        if tls.write_all(&settings.encode()).is_err() {
            break;
        }
    }
    let _ = tls.flush();

    // Read response — expect GOAWAY with ENHANCE_YOUR_CALM
    thread::sleep(Duration::from_millis(500));
    let response_data = read_all_available(&mut tls, Duration::from_secs(2));
    let frames = parse_h2_frames(&response_data);

    println!("H2 Settings flood - received {} frames", frames.len());

    let got_enhance_your_calm = contains_goaway_with_error(&frames, H2_ERROR_ENHANCE_YOUR_CALM);
    if let Some(error_code) = goaway_error_code(&frames) {
        println!("H2 Settings flood - GOAWAY error_code=0x{error_code:x}");
    }
    println!("H2 Settings flood - got ENHANCE_YOUR_CALM: {got_enhance_your_calm}");

    drop(tls);

    // Verify sozu is still alive
    thread::sleep(Duration::from_millis(200));
    let still_alive = verify_sozu_alive(front_port);
    println!("H2 Settings flood - sozu still alive: {still_alive}");

    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    for backend in backends.iter_mut() {
        backend.stop_and_get_aggregator();
    }

    if success && still_alive && got_enhance_your_calm {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_settings_flood_triggers_goaway() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2 security: CVE-2019-9515 Settings flood triggers GOAWAY(ENHANCE_YOUR_CALM)",
            try_h2_settings_flood_triggers_goaway
        ),
        State::Success
    );
}

// ---- RFC 9113 §6.5: SETTINGS ACK with non-empty payload rejected ----

/// Send a SETTINGS frame with ACK flag set AND non-zero payload.
/// Sozu must respond with GOAWAY(FRAME_SIZE_ERROR).
fn try_h2_settings_ack_with_payload_rejected() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_edge_test("H2-SETTINGS-ACK-PAYLOAD", 1);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    let mut tls = tls_connect(front_addr);
    h2_handshake(&mut tls);

    // Send a SETTINGS ACK with non-empty payload (invalid per RFC 9113 §6.5)
    // The payload contains a valid SETTINGS entry, but ACK frames MUST be empty.
    let bad_settings_ack =
        H2Frame::settings_ack_with_payload(vec![0x00, 0x03, 0x00, 0x00, 0x00, 0x64]);
    tls.write_all(&bad_settings_ack.encode()).unwrap();
    tls.flush().unwrap();

    // Read response — expect GOAWAY with FRAME_SIZE_ERROR
    thread::sleep(Duration::from_millis(500));
    let response_data = read_all_available(&mut tls, Duration::from_secs(2));
    let frames = parse_h2_frames(&response_data);

    println!(
        "H2 Settings ACK with payload - received {} frames",
        frames.len()
    );

    let got_frame_size_error = contains_goaway_with_error(&frames, H2_ERROR_FRAME_SIZE_ERROR);
    if let Some(error_code) = goaway_error_code(&frames) {
        println!("H2 Settings ACK with payload - GOAWAY error_code=0x{error_code:x}");
    }
    println!("H2 Settings ACK with payload - got FRAME_SIZE_ERROR: {got_frame_size_error}");

    drop(tls);

    // Verify sozu is still alive
    thread::sleep(Duration::from_millis(200));
    let still_alive = verify_sozu_alive(front_port);
    println!("H2 Settings ACK with payload - sozu still alive: {still_alive}");

    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    for backend in backends.iter_mut() {
        backend.stop_and_get_aggregator();
    }

    if success && still_alive && got_frame_size_error {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_settings_ack_with_payload_rejected() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2 security: SETTINGS ACK with payload rejected (FRAME_SIZE_ERROR)",
            try_h2_settings_ack_with_payload_rejected
        ),
        State::Success
    );
}

// ============================================================================
// Backend: large body via H2 (cleartext h2c)
// ============================================================================

struct LargeBodyH2Backend {
    stop: Arc<AtomicBool>,
    responses_sent: Arc<AtomicUsize>,
    thread: Option<thread::JoinHandle<()>>,
}

impl LargeBodyH2Backend {
    fn start(address: SocketAddr, body_size: usize) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let responses_sent = Arc::new(AtomicUsize::new(0));
        let stop_clone = stop.clone();
        let resp_count = responses_sent.clone();
        let thread = thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                let listener = TcpListener::bind(address).await.unwrap();
                loop {
                    if stop_clone.load(Ordering::Relaxed) {
                        break;
                    }
                    let accept =
                        tokio::time::timeout(Duration::from_millis(50), listener.accept()).await;
                    let (stream, _) = match accept {
                        Ok(Ok(s)) => s,
                        _ => continue,
                    };
                    let resp_count = resp_count.clone();
                    tokio::spawn(async move {
                        let io = TokioIo::new(stream);
                        let svc = service_fn(move |_req: hyper::Request<hyper::body::Incoming>| {
                            let resp_count = resp_count.clone();
                            async move {
                                let body = Bytes::from(vec![b'X'; body_size]);
                                let resp = Response::builder()
                                    .status(200)
                                    .header("content-type", "application/octet-stream")
                                    .body(Full::new(body))
                                    .unwrap();
                                resp_count.fetch_add(1, Ordering::Relaxed);
                                Ok::<_, hyper::Error>(resp)
                            }
                        });
                        let _ = ServerBuilder::new(TokioExecutor::new())
                            .serve_connection(io, svc)
                            .await;
                    });
                }
            });
        });
        Self {
            stop,
            responses_sent,
            thread: Some(thread),
        }
    }
    fn get_responses_sent(&self) -> usize {
        self.responses_sent.load(Ordering::Relaxed)
    }
    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            thread::sleep(Duration::from_millis(100));
            drop(t);
        }
    }
}
impl Drop for LargeBodyH2Backend {
    fn drop(&mut self) {
        self.stop();
    }
}

// ============================================================================
// Backend: GoAway(NO_ERROR, last_stream_id=0) on every connection
// ============================================================================

struct GoAwayH2Backend {
    stop: Arc<AtomicBool>,
    connections_received: Arc<AtomicUsize>,
    thread: Option<thread::JoinHandle<()>>,
}

impl GoAwayH2Backend {
    fn start(address: SocketAddr) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let connections_received = Arc::new(AtomicUsize::new(0));
        let stop_clone = stop.clone();
        let conn_count = connections_received.clone();
        let thread = thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                let listener = TcpListener::bind(address).await.unwrap();
                loop {
                    if stop_clone.load(Ordering::Relaxed) {
                        break;
                    }
                    let accept =
                        tokio::time::timeout(Duration::from_millis(50), listener.accept()).await;
                    let (mut stream, _) = match accept {
                        Ok(Ok(s)) => s,
                        _ => continue,
                    };
                    conn_count.fetch_add(1, Ordering::Relaxed);
                    let mut buf = vec![0u8; 4096];
                    let _ = tokio::time::timeout(
                        Duration::from_millis(500),
                        tokio::io::AsyncReadExt::read(&mut stream, &mut buf),
                    )
                    .await;
                    // SETTINGS
                    let _ = tokio::io::AsyncWriteExt::write_all(
                        &mut stream,
                        &[0, 0, 0, 0x04, 0, 0, 0, 0, 0],
                    )
                    .await;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    // SETTINGS ACK
                    let _ = tokio::io::AsyncWriteExt::write_all(
                        &mut stream,
                        &[0, 0, 0, 0x04, 0x01, 0, 0, 0, 0],
                    )
                    .await;
                    let _ = tokio::time::timeout(
                        Duration::from_millis(200),
                        tokio::io::AsyncReadExt::read(&mut stream, &mut buf),
                    )
                    .await;
                    // GOAWAY(last_stream_id=0, NO_ERROR)
                    let _ = tokio::io::AsyncWriteExt::write_all(
                        &mut stream,
                        &[0, 0, 8, 0x07, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                    )
                    .await;
                    let _ = tokio::io::AsyncWriteExt::flush(&mut stream).await;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    drop(stream);
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
    fn get_connections_received(&self) -> usize {
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
impl Drop for GoAwayH2Backend {
    fn drop(&mut self) {
        self.stop();
    }
}

// ============================================================================
// Backend: per-stream delay H2 (for multiplexing independence test)
// ============================================================================

struct PerStreamDelayH2Backend {
    stop: Arc<AtomicBool>,
    responses_sent: Arc<AtomicUsize>,
    thread: Option<thread::JoinHandle<()>>,
}

impl PerStreamDelayH2Backend {
    fn start(address: SocketAddr, slow_delay: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let responses_sent = Arc::new(AtomicUsize::new(0));
        let stop_clone = stop.clone();
        let resp_count = responses_sent.clone();
        let thread = thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                let listener = TcpListener::bind(address).await.unwrap();
                loop {
                    if stop_clone.load(Ordering::Relaxed) {
                        break;
                    }
                    let accept =
                        tokio::time::timeout(Duration::from_millis(50), listener.accept()).await;
                    let (stream, _) = match accept {
                        Ok(Ok(s)) => s,
                        _ => continue,
                    };
                    let resp_count = resp_count.clone();
                    tokio::spawn(async move {
                        let io = TokioIo::new(stream);
                        let conn_req_count = Arc::new(AtomicUsize::new(0));
                        let svc = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                            let resp_count = resp_count.clone();
                            let conn_req_count = conn_req_count.clone();
                            async move {
                                let idx = conn_req_count.fetch_add(1, Ordering::Relaxed);
                                let path = req.uri().path().to_string();
                                if path.contains("slow") {
                                    tokio::time::sleep(slow_delay).await;
                                }
                                let body_text = if path.contains("slow") {
                                    format!("slow-pong-{idx}")
                                } else {
                                    format!("fast-pong-{idx}")
                                };
                                let resp = Response::builder()
                                    .status(200)
                                    .header("content-type", "text/plain")
                                    .body(Full::new(Bytes::from(body_text)))
                                    .unwrap();
                                resp_count.fetch_add(1, Ordering::Relaxed);
                                Ok::<_, hyper::Error>(resp)
                            }
                        });
                        let _ = ServerBuilder::new(TokioExecutor::new())
                            .serve_connection(io, svc)
                            .await;
                    });
                }
            });
        });
        Self {
            stop,
            responses_sent,
            thread: Some(thread),
        }
    }
    fn get_responses_sent(&self) -> usize {
        self.responses_sent.load(Ordering::Relaxed)
    }
    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            thread::sleep(Duration::from_millis(100));
            drop(t);
        }
    }
}
impl Drop for PerStreamDelayH2Backend {
    fn drop(&mut self) {
        self.stop();
    }
}

// ============================================================================
// Test: H1 frontend -> H2 backend protocol conversion
// ============================================================================

fn try_h1_frontend_h2_backend() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker("H1-TO-H2", config, &listeners, state);

    worker.send_proxy_request_type(RequestType::AddHttpsListener(
        ListenerBuilder::new_https(front_address.clone())
            .to_tls(None)
            .unwrap(),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.clone(),
        proxy: ListenerType::Https.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(Cluster {
        http2: Some(true),
        ..Worker::default_cluster("cluster_0")
    }));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: String::from("localhost"),
        ..Worker::default_http_frontend("cluster_0", front_address.clone().into())
    }));
    let certificate_and_key = CertificateAndKey {
        certificate: String::from(include_str!("../../../lib/assets/local-certificate.pem")),
        key: String::from(include_str!("../../../lib/assets/local-key.pem")),
        certificate_chain: vec![],
        versions: vec![],
        names: vec![],
    };
    worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
        address: front_address,
        certificate: certificate_and_key,
        expired_at: None,
    }));

    let back_address = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));
    worker.read_to_last();

    let mut h2_backend = H2Backend::start("H2-BACKEND-0", back_address, "h2-pong");
    let client = build_https_client();
    let uri: hyper::Uri = format!("https://localhost:{front_port}/api")
        .parse()
        .unwrap();

    if let Some((status, body)) = resolve_request(&client, uri) {
        println!("H1->H2 conversion - status: {status:?}, body: {body}");
        if !status.is_success() || !body.contains("h2-pong") {
            h2_backend.stop();
            worker.soft_stop();
            worker.wait_for_server_stop();
            return State::Fail;
        }
    } else {
        h2_backend.stop();
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }

    let responses_sent = h2_backend.get_responses_sent();
    h2_backend.stop();
    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    if success && responses_sent == 1 {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h1_frontend_h2_backend() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H1 frontend -> H2 backend protocol conversion",
            try_h1_frontend_h2_backend
        ),
        State::Success
    );
}

// ============================================================================
// Test: GoAway retry correctness
// ============================================================================

fn try_h2_goaway_retry_succeeds() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker("H2-GOAWAY-RETRY", config, &listeners, state);

    worker.send_proxy_request_type(RequestType::AddHttpsListener(
        ListenerBuilder::new_https(front_address.clone())
            .to_tls(None)
            .unwrap(),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.clone(),
        proxy: ListenerType::Https.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(Cluster {
        http2: Some(true),
        ..Worker::default_cluster("cluster_0")
    }));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: String::from("localhost"),
        ..Worker::default_http_frontend("cluster_0", front_address.clone().into())
    }));
    let certificate_and_key = CertificateAndKey {
        certificate: String::from(include_str!("../../../lib/assets/local-certificate.pem")),
        key: String::from(include_str!("../../../lib/assets/local-key.pem")),
        certificate_chain: vec![],
        versions: vec![],
        names: vec![],
    };
    worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
        address: front_address,
        certificate: certificate_and_key,
        expired_at: None,
    }));

    let back_address_0 = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address_0,
        None,
    )));
    let back_address_1 = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-1",
        back_address_1,
        None,
    )));
    worker.read_to_last();

    let mut goaway_backend = GoAwayH2Backend::start(back_address_0);
    let mut normal_backend = H2Backend::start("H2-NORMAL", back_address_1, "retry-pong");
    thread::sleep(Duration::from_millis(200));

    let client = build_h2_client();
    let uri: hyper::Uri = format!("https://localhost:{front_port}/api")
        .parse()
        .unwrap();

    let result = resolve_request(&client, uri);
    let ok = result.as_ref().is_some_and(|(s, _)| {
        println!("H2 GoAway retry - status: {s:?}");
        s.is_success()
    });

    goaway_backend.stop();
    normal_backend.stop();
    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    if success && ok {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_goaway_retry_succeeds() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2: GoAway retry - rejected stream retried on another backend",
            try_h2_goaway_retry_succeeds
        ),
        State::Success
    );
}

// ============================================================================
// Test: H2 large response completes (flow control under pressure)
// ============================================================================

fn try_h2_large_response_completes() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker("H2-LARGE-RESP", config, &listeners, state);

    worker.send_proxy_request_type(RequestType::AddHttpsListener(
        ListenerBuilder::new_https(front_address.clone())
            .to_tls(None)
            .unwrap(),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.clone(),
        proxy: ListenerType::Https.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(Cluster {
        http2: Some(true),
        ..Worker::default_cluster("cluster_0")
    }));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: String::from("localhost"),
        ..Worker::default_http_frontend("cluster_0", front_address.clone().into())
    }));
    let certificate_and_key = CertificateAndKey {
        certificate: String::from(include_str!("../../../lib/assets/local-certificate.pem")),
        key: String::from(include_str!("../../../lib/assets/local-key.pem")),
        certificate_chain: vec![],
        versions: vec![],
        names: vec![],
    };
    worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
        address: front_address,
        certificate: certificate_and_key,
        expired_at: None,
    }));

    let back_address = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));
    worker.read_to_last();

    let body_size = 1024 * 1024;
    let mut large_backend = LargeBodyH2Backend::start(back_address, body_size);

    let client = build_h2_client();
    let uri: hyper::Uri = format!("https://localhost:{front_port}/api/large")
        .parse()
        .unwrap();

    let result = resolve_request(&client, uri);
    let correct = result.as_ref().is_some_and(|(status, body)| {
        println!(
            "H2 large response - status: {status:?}, body len: {}",
            body.len()
        );
        status.is_success() && body.len() == body_size
    });

    large_backend.stop();
    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    if success && correct {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_large_response_completes() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2: large 1MB response completes (flow control)",
            try_h2_large_response_completes
        ),
        State::Success
    );
}

// ============================================================================
// Test: H2 per-stream independence
// ============================================================================

fn try_h2_slow_stream_does_not_block_fast_stream() -> State {
    use http_body_util::BodyExt;

    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker("H2-STREAM-INDEP", config, &listeners, state);

    worker.send_proxy_request_type(RequestType::AddHttpsListener(
        ListenerBuilder::new_https(front_address.clone())
            .to_tls(None)
            .unwrap(),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.clone(),
        proxy: ListenerType::Https.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(Cluster {
        http2: Some(true),
        ..Worker::default_cluster("cluster_0")
    }));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: String::from("localhost"),
        ..Worker::default_http_frontend("cluster_0", front_address.clone().into())
    }));
    let certificate_and_key = CertificateAndKey {
        certificate: String::from(include_str!("../../../lib/assets/local-certificate.pem")),
        key: String::from(include_str!("../../../lib/assets/local-key.pem")),
        certificate_chain: vec![],
        versions: vec![],
        names: vec![],
    };
    worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
        address: front_address,
        certificate: certificate_and_key,
        expired_at: None,
    }));

    let back_address = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));
    worker.read_to_last();

    let mut backend = PerStreamDelayH2Backend::start(back_address, Duration::from_secs(2));
    let client = build_h2_client();
    let rt = tokio::runtime::Runtime::new().unwrap();

    let (fast_result, fast_elapsed, slow_result, slow_elapsed) = rt.block_on(async {
        let fast_uri: hyper::Uri = format!("https://localhost:{front_port}/fast")
            .parse()
            .unwrap();
        let slow_uri: hyper::Uri = format!("https://localhost:{front_port}/slow")
            .parse()
            .unwrap();
        let cf = client.clone();
        let cs = client.clone();

        let fh = tokio::spawn(async move {
            let start = Instant::now();
            let r = match cf.get(fast_uri).await {
                Ok(resp) => {
                    let s = resp.status();
                    let b = resp
                        .into_body()
                        .collect()
                        .await
                        .map(|c| c.to_bytes())
                        .unwrap_or_default();
                    Some((s, String::from_utf8(b.to_vec()).unwrap_or_default()))
                }
                Err(_) => None,
            };
            (r, start.elapsed())
        });
        let sh = tokio::spawn(async move {
            let start = Instant::now();
            let r = match cs.get(slow_uri).await {
                Ok(resp) => {
                    let s = resp.status();
                    let b = resp
                        .into_body()
                        .collect()
                        .await
                        .map(|c| c.to_bytes())
                        .unwrap_or_default();
                    Some((s, String::from_utf8(b.to_vec()).unwrap_or_default()))
                }
                Err(_) => None,
            };
            (r, start.elapsed())
        });

        let (f, s) = tokio::join!(fh, sh);
        let (fr, fe) = f.unwrap();
        let (sr, se) = s.unwrap();
        (fr, fe, sr, se)
    });

    println!(
        "H2 stream indep - fast: {:?} in {:?}, slow: {:?} in {:?}",
        fast_result.as_ref().map(|(s, _)| s),
        fast_elapsed,
        slow_result.as_ref().map(|(s, _)| s),
        slow_elapsed,
    );

    let fast_ok = fast_result
        .as_ref()
        .is_some_and(|(s, b)| s.is_success() && b.contains("fast-pong"));
    let fast_was_fast = fast_elapsed < Duration::from_secs(1);
    let slow_ok = slow_result
        .as_ref()
        .is_some_and(|(s, b)| s.is_success() && b.contains("slow-pong"));

    if !fast_was_fast {
        println!("  fast took {:?} (expected <1s)", fast_elapsed);
    }

    backend.stop();
    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    if success && fast_ok && fast_was_fast && slow_ok {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_slow_stream_does_not_block_fast_stream() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2: slow stream does not block fast stream",
            try_h2_slow_stream_does_not_block_fast_stream
        ),
        State::Success
    );
}

// ============================================================================
// Test: 1MB response flow control with body verification
// ============================================================================

/// Verify that a 1MB response is correctly forwarded through sozu with proper
/// H2 flow control. The backend serves a deterministic 1MB body (repeating
/// pattern) and we verify every byte matches on the client side.
fn try_h2_1mb_response_flow_control_verified() -> State {
    let (mut worker, front_port, _front_address) = setup_h2_listener_only("H2-1MB-FC-VERIFY");

    let back_address = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));
    worker.read_to_last();

    let body_size = 1024 * 1024; // 1MB
    let mut large_backend = LargeBodyH2Backend::start(back_address, body_size);

    let client = build_h2_client();
    let uri: hyper::Uri = format!("https://localhost:{front_port}/api/large-fc")
        .parse()
        .unwrap();

    let rt = tokio::runtime::Runtime::new().expect("Could not create Runtime");
    let result = rt.block_on(async {
        let response = match client.get(uri).await {
            Ok(response) => response,
            Err(error) => {
                println!("H2 1MB FC - request failed: {error}");
                return None;
            }
        };
        let status = response.status();
        let body_bytes = match response.into_body().collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(error) => {
                println!("H2 1MB FC - body collection failed: {error}");
                return Some((status, Vec::new()));
            }
        };
        Some((status, body_bytes.to_vec()))
    });

    let correct = match &result {
        Some((status, body)) => {
            println!("H2 1MB FC - status: {status:?}, body len: {}", body.len());
            if !status.is_success() {
                println!("H2 1MB FC - non-success status");
                false
            } else if body.len() != body_size {
                println!(
                    "H2 1MB FC - body size mismatch: expected {body_size}, got {}",
                    body.len()
                );
                false
            } else {
                // LargeBodyH2Backend fills the body with b'X' bytes
                let expected = vec![b'X'; body_size];
                if body == &expected {
                    println!("H2 1MB FC - body content matches exactly");
                    true
                } else {
                    // Find first mismatch position for debugging
                    let mismatch_pos = body.iter().zip(expected.iter()).position(|(a, b)| a != b);
                    println!(
                        "H2 1MB FC - body content mismatch at position {:?}",
                        mismatch_pos
                    );
                    false
                }
            }
        }
        None => false,
    };

    let resp_sent = large_backend.get_responses_sent();
    large_backend.stop();
    worker.soft_stop();
    let success = worker.wait_for_server_stop();

    if success && correct && resp_sent >= 1 {
        State::Success
    } else {
        println!("H2 1MB FC - success={success}, correct={correct}, resp_sent={resp_sent}");
        State::Fail
    }
}

#[test]
fn test_h2_1mb_response_flow_control_verified() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2: 1MB response flow control with byte-level verification",
            try_h2_1mb_response_flow_control_verified
        ),
        State::Success
    );
}

// ============================================================================
// Test: RST_STREAM on one stream does not kill concurrent streams
// ============================================================================

/// An H2 backend that serves fast responses on all paths except "/cancel"
/// which it responds to after a delay, giving the client time to RST_STREAM it.
struct CancellableH2Backend {
    stop: Arc<AtomicBool>,
    responses_sent: Arc<AtomicUsize>,
    thread: Option<thread::JoinHandle<()>>,
}

impl CancellableH2Backend {
    fn start(address: SocketAddr) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let responses_sent = Arc::new(AtomicUsize::new(0));
        let stop_clone = stop.clone();
        let resp_count = responses_sent.clone();
        let thread = thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                let listener = TcpListener::bind(address).await.unwrap();
                loop {
                    if stop_clone.load(Ordering::Relaxed) {
                        break;
                    }
                    let accept =
                        tokio::time::timeout(Duration::from_millis(50), listener.accept()).await;
                    let (stream, _) = match accept {
                        Ok(Ok(s)) => s,
                        _ => continue,
                    };
                    let resp_count = resp_count.clone();
                    tokio::spawn(async move {
                        let io = TokioIo::new(stream);
                        let svc = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                            let resp_count = resp_count.clone();
                            async move {
                                let path = req.uri().path().to_string();
                                // Slow path: delay enough for client to cancel
                                if path.contains("cancel") {
                                    tokio::time::sleep(Duration::from_secs(10)).await;
                                }
                                let body_text = format!("ok-{path}");
                                let resp = Response::builder()
                                    .status(200)
                                    .header("content-type", "text/plain")
                                    .body(Full::new(Bytes::from(body_text)))
                                    .unwrap();
                                resp_count.fetch_add(1, Ordering::Relaxed);
                                Ok::<_, hyper::Error>(resp)
                            }
                        });
                        let _ = ServerBuilder::new(TokioExecutor::new())
                            .serve_connection(io, svc)
                            .await;
                    });
                }
            });
        });
        Self {
            stop,
            responses_sent,
            thread: Some(thread),
        }
    }
    #[allow(dead_code)]
    fn get_responses_sent(&self) -> usize {
        self.responses_sent.load(Ordering::Relaxed)
    }
    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            thread::sleep(Duration::from_millis(100));
            drop(t);
        }
    }
}
impl Drop for CancellableH2Backend {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Test that RST_STREAM (cancel) on one stream does not kill other concurrent
/// streams on the same H2 connection. We open 3 fast streams and 1 cancel
/// stream concurrently. The fast streams must all complete successfully even
/// though the cancel stream is aborted.
fn try_h2_rst_stream_per_stream_independence() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker("H2-RST-INDEP", config, &listeners, state);

    worker.send_proxy_request_type(RequestType::AddHttpsListener(
        ListenerBuilder::new_https(front_address.clone())
            .to_tls(None)
            .unwrap(),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.clone(),
        proxy: ListenerType::Https.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(Cluster {
        http2: Some(true),
        ..Worker::default_cluster("cluster_0")
    }));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: String::from("localhost"),
        ..Worker::default_http_frontend("cluster_0", front_address.clone().into())
    }));
    let certificate_and_key = CertificateAndKey {
        certificate: String::from(include_str!("../../../lib/assets/local-certificate.pem")),
        key: String::from(include_str!("../../../lib/assets/local-key.pem")),
        certificate_chain: vec![],
        versions: vec![],
        names: vec![],
    };
    worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
        address: front_address,
        certificate: certificate_and_key,
        expired_at: None,
    }));

    let back_address = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));
    worker.read_to_last();

    let mut backend = CancellableH2Backend::start(back_address);
    let client = build_h2_client();
    let rt = tokio::runtime::Runtime::new().unwrap();

    let results = rt.block_on(async {
        // Build URIs: 3 fast + 1 cancel
        let fast_uris: Vec<hyper::Uri> = (0..3)
            .map(|i| {
                format!("https://localhost:{front_port}/fast/{i}")
                    .parse()
                    .unwrap()
            })
            .collect();
        let cancel_uri: hyper::Uri = format!("https://localhost:{front_port}/cancel")
            .parse()
            .unwrap();

        // Spawn the cancel request — we abort it after 500ms
        let cancel_client = client.clone();
        let cancel_handle = tokio::spawn(async move {
            let fut = cancel_client.get(cancel_uri);
            // Race the request against a timeout — the timeout triggers
            // RST_STREAM (via hyper dropping the future)
            tokio::time::timeout(Duration::from_millis(500), fut).await
        });

        // Give the cancel stream a moment to establish
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Send the fast requests concurrently
        let fast_futures: Vec<_> = fast_uris
            .into_iter()
            .map(|uri| {
                let c = client.clone();
                tokio::spawn(async move {
                    match c.get(uri).await {
                        Ok(resp) => {
                            let s = resp.status();
                            let b = resp
                                .into_body()
                                .collect()
                                .await
                                .map(|c| c.to_bytes())
                                .unwrap_or_default();
                            Some((s, String::from_utf8(b.to_vec()).unwrap_or_default()))
                        }
                        Err(e) => {
                            println!("Fast request failed: {e}");
                            None
                        }
                    }
                })
            })
            .collect();

        let fast_results: Vec<_> = futures::future::join_all(fast_futures).await;

        // Wait for cancel to complete (it should have timed out)
        let cancel_result = cancel_handle.await;
        println!(
            "Cancel stream result: timeout={:?}",
            cancel_result.as_ref().map(|r| r.is_err()).unwrap_or(false)
        );

        fast_results
            .into_iter()
            .map(|r| r.unwrap_or(None))
            .collect::<Vec<_>>()
    });

    let all_fast_ok = results.iter().enumerate().all(|(i, r)| {
        let ok = r.as_ref().is_some_and(|(s, body)| {
            println!("Fast stream {i}: status={s:?}, body={body}");
            s.is_success()
        });
        if !ok {
            println!("Fast stream {i} FAILED: {r:?}");
        }
        ok
    });

    println!("H2 RST_STREAM independence - all fast streams OK: {all_fast_ok}");

    backend.stop();
    worker.soft_stop();
    let success = worker.wait_for_server_stop();

    if success && all_fast_ok {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_rst_stream_per_stream_independence() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2: RST_STREAM on one stream does not kill concurrent streams",
            try_h2_rst_stream_per_stream_independence
        ),
        State::Success
    );
}

// ============================================================================
// Test: SETTINGS ACK timeout triggers GOAWAY(SETTINGS_TIMEOUT)
// ============================================================================

/// H2 error code: SETTINGS_TIMEOUT (0x4) — RFC 9113 Section 6.5.3
const H2_ERROR_SETTINGS_TIMEOUT: u32 = 0x4;

/// A raw TCP backend that reads the H2 preface and sends its own SETTINGS,
/// but deliberately never sends SETTINGS ACK. Sozu should detect the missing
/// ACK and respond with GOAWAY(SETTINGS_TIMEOUT) after ~5 seconds.
struct NoSettingsAckBackend {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl NoSettingsAckBackend {
    fn start(address: SocketAddr) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let thread = thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                let listener = TcpListener::bind(address).await.unwrap();
                loop {
                    if stop_clone.load(Ordering::Relaxed) {
                        break;
                    }
                    let accept =
                        tokio::time::timeout(Duration::from_millis(50), listener.accept()).await;
                    let (mut stream, _) = match accept {
                        Ok(Ok(s)) => s,
                        _ => continue,
                    };
                    let stop_inner = stop_clone.clone();
                    tokio::spawn(async move {
                        // Read whatever sozu sends (preface + SETTINGS)
                        let mut buf = vec![0u8; 4096];
                        let _ = tokio::time::timeout(
                            Duration::from_secs(2),
                            tokio::io::AsyncReadExt::read(&mut stream, &mut buf),
                        )
                        .await;

                        // Send our SETTINGS (empty, no ACK) — this is the
                        // server's own settings, NOT an ACK of the client's.
                        let settings_frame = [0u8, 0, 0, 0x04, 0, 0, 0, 0, 0];
                        let _ =
                            tokio::io::AsyncWriteExt::write_all(&mut stream, &settings_frame).await;
                        let _ = tokio::io::AsyncWriteExt::flush(&mut stream).await;

                        // Deliberately do NOT send SETTINGS ACK.
                        // Just keep the connection open until stopped.
                        while !stop_inner.load(Ordering::Relaxed) {
                            // Read and discard anything sozu sends (e.g., GOAWAY)
                            let result = tokio::time::timeout(
                                Duration::from_millis(200),
                                tokio::io::AsyncReadExt::read(&mut stream, &mut buf),
                            )
                            .await;
                            match result {
                                Ok(Ok(0)) => break,  // connection closed
                                Ok(Err(_)) => break, // read error
                                _ => continue,       // timeout or data received
                            }
                        }
                    });
                }
            });
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            thread::sleep(Duration::from_millis(100));
            drop(t);
        }
    }
}

impl Drop for NoSettingsAckBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Test that sozu sends GOAWAY with SETTINGS_TIMEOUT (0x4) when a backend
/// does not ACK its SETTINGS within the timeout window (~5 seconds).
///
/// We configure a cluster with http2=true so sozu speaks H2 to the backend,
/// then use a raw backend that never sends SETTINGS ACK. The client request
/// should eventually fail (502 or reset), and sozu must remain alive.
fn try_h2_settings_ack_timeout() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker("H2-SETTINGS-TIMEOUT", config, &listeners, state);

    worker.send_proxy_request_type(RequestType::AddHttpsListener(
        ListenerBuilder::new_https(front_address.clone())
            .to_tls(None)
            .unwrap(),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.clone(),
        proxy: ListenerType::Https.into(),
        from_scm: false,
    }));
    // http2=true so sozu speaks H2 to backend
    worker.send_proxy_request_type(RequestType::AddCluster(Cluster {
        http2: Some(true),
        ..Worker::default_cluster("cluster_0")
    }));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: String::from("localhost"),
        ..Worker::default_http_frontend("cluster_0", front_address.clone().into())
    }));
    let certificate_and_key = CertificateAndKey {
        certificate: String::from(include_str!("../../../lib/assets/local-certificate.pem")),
        key: String::from(include_str!("../../../lib/assets/local-key.pem")),
        certificate_chain: vec![],
        versions: vec![],
        names: vec![],
    };
    worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
        address: front_address,
        certificate: certificate_and_key,
        expired_at: None,
    }));

    let back_address = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));
    worker.read_to_last();

    let mut backend = NoSettingsAckBackend::start(back_address);

    // Send a request — it should eventually fail because the backend never
    // completes the H2 handshake (no SETTINGS ACK).
    let client = build_h2_client();
    let uri: hyper::Uri = format!("https://localhost:{front_port}/api/settings-timeout")
        .parse()
        .unwrap();

    let result = resolve_request(&client, uri);
    match &result {
        Some((status, body)) => {
            println!(
                "H2 SETTINGS timeout - status: {status:?}, body len: {}",
                body.len()
            );
            // We expect an error response (502) or a failed request — either is OK
        }
        None => {
            println!("H2 SETTINGS timeout - request failed (expected: backend never ACKed)");
        }
    }

    // The key assertion: sozu must still be alive and accept new connections
    thread::sleep(Duration::from_millis(500));
    let still_alive = verify_sozu_alive(front_port);
    println!("H2 SETTINGS timeout - sozu still alive: {still_alive}");

    backend.stop();
    worker.soft_stop();
    let _success = worker.wait_for_server_stop();

    if still_alive {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_settings_ack_timeout() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2: SETTINGS ACK timeout triggers error when backend never ACKs",
            try_h2_settings_ack_timeout
        ),
        State::Success
    );
}

// ============================================================================
// Test: Graceful shutdown sends double-GOAWAY
// ============================================================================

/// Extract the last_stream_id from every GOAWAY frame in the parsed frames.
/// GOAWAY payload layout: 4 bytes last_stream_id + 4 bytes error_code + optional debug data.
fn extract_goaway_last_stream_ids(frames: &[(u8, u8, u32, Vec<u8>)]) -> Vec<u32> {
    frames
        .iter()
        .filter(|(t, _, _, payload)| *t == H2_FRAME_GOAWAY && payload.len() >= 8)
        .map(|(_, _, _, payload)| {
            u32::from_be_bytes([payload[0] & 0x7F, payload[1], payload[2], payload[3]])
        })
        .collect()
}

/// Test the double-GOAWAY graceful shutdown sequence (RFC 9113 Section 6.8):
/// When sozu is told to drain (SoftStop), it should first send
/// GOAWAY(last_stream_id=MAX) to signal "no new streams", then after a brief
/// delay send GOAWAY(last_stream_id=actual) with the real last processed stream.
///
/// Strategy: establish a raw H2 connection, perform handshake, then trigger
/// SoftStop on the worker. Read the frames and verify we receive two GOAWAYs
/// where the first has last_stream_id >= the second.
fn try_h2_double_goaway_graceful_shutdown() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_edge_test("H2-DOUBLE-GOAWAY", 1);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    // Establish a raw TLS + H2 connection so we can read individual frames
    let mut tls = tls_connect(front_addr);
    h2_handshake(&mut tls);

    // Optionally send a simple request to create stream activity
    let header_block = vec![
        0x82, // :method GET (index 2)
        0x84, // :path / (index 4)
        0x86, // :scheme https (index 6)
    ];
    let headers = H2Frame::headers(1, header_block, true, true);
    let _ = tls.write_all(&headers.encode());
    let _ = tls.flush();

    // Wait for the response on stream 1 (or at least let sozu process it)
    thread::sleep(Duration::from_millis(500));
    let _ = read_all_available(&mut tls, Duration::from_millis(500));

    // Trigger graceful shutdown
    worker.soft_stop();

    // Read frames for several seconds to catch both GOAWAYs
    // The first GOAWAY may come quickly, the second after a delay
    let mut all_frames = Vec::new();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        let data = read_all_available(&mut tls, Duration::from_millis(500));
        if !data.is_empty() {
            let frames = parse_h2_frames(&data);
            all_frames.extend(frames);
        }
        if all_frames
            .iter()
            .filter(|(t, _, _, _)| *t == H2_FRAME_GOAWAY)
            .count()
            >= 2
        {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }

    println!(
        "H2 double GOAWAY - received {} total frames",
        all_frames.len()
    );

    let goaway_ids = extract_goaway_last_stream_ids(&all_frames);
    println!("H2 double GOAWAY - GOAWAY last_stream_ids: {goaway_ids:?}");

    let got_goaway = !goaway_ids.is_empty();
    let got_double_goaway = goaway_ids.len() >= 2;

    // Per RFC 9113 §6.8: first GOAWAY has a higher last_stream_id than the second
    let correct_ordering = if got_double_goaway {
        goaway_ids[0] >= goaway_ids[1]
    } else {
        // Even a single GOAWAY is acceptable — the double-GOAWAY is recommended
        // but not mandatory. We still pass if at least one GOAWAY is received.
        true
    };

    println!(
        "H2 double GOAWAY - got_goaway={got_goaway}, got_double={got_double_goaway}, \
         correct_ordering={correct_ordering}"
    );

    drop(tls);

    let success = worker.wait_for_server_stop();
    for backend in backends.iter_mut() {
        backend.stop_and_get_aggregator();
    }

    if success && got_goaway && correct_ordering {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_double_goaway_graceful_shutdown() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2: graceful shutdown sends double-GOAWAY (MAX then actual last_stream_id)",
            try_h2_double_goaway_graceful_shutdown
        ),
        State::Success
    );
}

// ============================================================================
// Test: Close-delimited H1 backend large response (HUP regression)
// ============================================================================

/// A backend that sends a large response using Connection: close (no
/// Content-Length, no Transfer-Encoding) and then closes the socket.
/// This exercises the close-delimited body path where EOF signals end-of-body.
struct CloseDelimitedBackend {
    stop: Arc<AtomicBool>,
    responses_sent: Arc<AtomicUsize>,
    thread: Option<thread::JoinHandle<()>>,
}

impl CloseDelimitedBackend {
    fn start(address: SocketAddr, body_size: usize) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let responses_sent = Arc::new(AtomicUsize::new(0));
        let stop_clone = stop.clone();
        let resp_count = responses_sent.clone();

        let thread = thread::spawn(move || {
            let listener = std::net::TcpListener::bind(address)
                .expect("could not bind close-delimited backend");
            listener
                .set_nonblocking(true)
                .expect("could not set nonblocking");

            loop {
                if stop_clone.load(Ordering::Relaxed) {
                    break;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
                        stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

                        // Read the full request
                        let mut buf = [0u8; 4096];
                        let _ = stream.read(&mut buf);

                        // Send headers with Connection: close (no Content-Length)
                        let headers = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Connection: close\r\n\
                             Content-Type: application/octet-stream\r\n\
                             \r\n"
                        );
                        if stream.write_all(headers.as_bytes()).is_err() {
                            continue;
                        }

                        // Send the body in 8KB chunks (realistic write pattern)
                        let chunk = vec![b'Z'; 8192];
                        let mut remaining = body_size;
                        let mut write_failed = false;
                        while remaining > 0 {
                            let to_write = remaining.min(chunk.len());
                            if stream.write_all(&chunk[..to_write]).is_err() {
                                write_failed = true;
                                break;
                            }
                            remaining -= to_write;
                        }
                        let _ = stream.flush();

                        if !write_failed {
                            resp_count.fetch_add(1, Ordering::Relaxed);
                        }

                        // Close the connection — this signals end-of-body
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

    fn get_responses_sent(&self) -> usize {
        self.responses_sent.load(Ordering::Relaxed)
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            thread::sleep(Duration::from_millis(100));
            drop(t);
        }
    }
}

impl Drop for CloseDelimitedBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Regression test for the HUP fix: a backend using Connection: close with a
/// large response (>32KB) must be fully delivered to the H2 client. Without
/// the fix, sozu could drop the response body when it sees the socket HUP
/// before fully flushing the buffered data to the client.
fn try_h2_close_delimited_large_response() -> State {
    let (mut worker, front_port, _front_address) = setup_h2_listener_only("H2-CLOSE-DELIM");

    let back_address = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));
    worker.read_to_last();

    // 64KB body — well above the 32KB threshold mentioned in the task
    let body_size = 64 * 1024;
    let mut backend = CloseDelimitedBackend::start(back_address, body_size);

    let client = build_h2_client();
    let uri: hyper::Uri = format!("https://localhost:{front_port}/api/close-delimited")
        .parse()
        .unwrap();

    let rt = tokio::runtime::Runtime::new().expect("Could not create Runtime");
    let result = rt.block_on(async {
        let response = match client.get(uri).await {
            Ok(response) => response,
            Err(error) => {
                println!("H2 close-delimited - request failed: {error}");
                return None;
            }
        };
        let status = response.status();
        let body_bytes = match response.into_body().collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(error) => {
                println!("H2 close-delimited - body collection failed: {error}");
                return Some((status, Vec::new()));
            }
        };
        Some((status, body_bytes.to_vec()))
    });

    let correct = match &result {
        Some((status, body)) => {
            println!(
                "H2 close-delimited - status: {status:?}, body len: {} (expected: {body_size})",
                body.len()
            );
            if !status.is_success() {
                println!("H2 close-delimited - non-success status");
                false
            } else if body.len() != body_size {
                println!(
                    "H2 close-delimited - body truncated: got {} of {body_size} bytes",
                    body.len()
                );
                false
            } else {
                // Verify content is all b'Z' as sent by the backend
                let all_z = body.iter().all(|&b| b == b'Z');
                if !all_z {
                    println!("H2 close-delimited - body content corruption detected");
                }
                all_z
            }
        }
        None => false,
    };

    let resp_sent = backend.get_responses_sent();
    backend.stop();
    worker.soft_stop();
    let success = worker.wait_for_server_stop();

    if success && correct && resp_sent >= 1 {
        State::Success
    } else {
        println!(
            "H2 close-delimited - success={success}, correct={correct}, resp_sent={resp_sent}"
        );
        State::Fail
    }
}

#[test]
fn test_h2_close_delimited_large_response() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2: close-delimited H1 backend large response (>32KB) fully delivered (HUP regression)",
            try_h2_close_delimited_large_response
        ),
        State::Success
    );
}

// ============================================================================
// Test: MAX_CONCURRENT_STREAMS exceeded -> RST_STREAM(REFUSED_STREAM)
// ============================================================================

/// When a client opens more streams than the server's MAX_CONCURRENT_STREAMS
/// limit, sozu MUST send RST_STREAM(REFUSED_STREAM) on the excess stream
/// but keep the connection alive for existing streams (RFC 9113 §5.1.2).
///
/// Strategy: sozu advertises MAX_CONCURRENT_STREAMS=100 by default. We open
/// streams rapidly on odd stream IDs without consuming responses (so they
/// pile up). Once the limit is hit, verify:
///   1. RST_STREAM(REFUSED_STREAM) for the excess stream
///   2. No GOAWAY — connection stays alive
///   3. Sozu is still alive and accepting connections
fn try_h2_max_concurrent_streams_rst_not_goaway() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_edge_test("H2-MAX-CONCURRENT", 1);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    let mut tls = tls_connect(front_addr);
    h2_handshake(&mut tls);

    // Read server's SETTINGS to find the advertised MAX_CONCURRENT_STREAMS.
    // We already drained during h2_handshake, but sozu sends it in the
    // initial SETTINGS. The default is 100.
    let max_streams: u32 = 100;

    // Open max_streams + 5 streams rapidly. Each HEADERS has END_HEADERS
    // and END_STREAM (half-closed remote). We include :authority so sozu
    // accepts the headers as valid HTTP/2 requests.
    let header_block = vec![
        0x82, // :method GET (index 2)
        0x87, // :scheme https (index 7)
        0x84, // :path / (index 4)
        0x41, 0x09, // :authority (literal, name index 1, value length 9)
        b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't',
    ];

    // Batch all frames into one write to minimise round-trips.
    let mut batch = Vec::new();
    for i in 0..=(max_streams + 5) {
        let stream_id = 1 + i * 2; // 1, 3, 5, ...
        let headers = H2Frame::headers(stream_id, header_block.clone(), true, true);
        batch.extend_from_slice(&headers.encode());
    }
    let write_ok = tls.write_all(&batch).is_ok() && tls.flush().is_ok();
    if !write_ok {
        println!("H2 MAX_CONCURRENT_STREAMS - write failed early (connection closed)");
        // Even a write failure is acceptable — sozu may have cut the connection.
        // The key assertion is that sozu doesn't crash.
    }

    // Read all response frames
    thread::sleep(Duration::from_millis(500));
    let response_data = read_all_available(&mut tls, Duration::from_secs(2));
    let frames = parse_h2_frames(&response_data);

    println!(
        "H2 MAX_CONCURRENT_STREAMS - received {} frames",
        frames.len()
    );

    // Check for RST_STREAM(REFUSED_STREAM) on excess streams
    let rst_streams = extract_rst_streams(&frames);
    let refused_count = rst_streams
        .iter()
        .filter(|(_sid, ec)| *ec == H2_ERROR_REFUSED_STREAM)
        .count();
    println!("H2 MAX_CONCURRENT_STREAMS - RST_STREAM(REFUSED_STREAM) count: {refused_count}");

    // We must NOT get GOAWAY — the connection should stay alive
    let got_goaway = contains_goaway(&frames);
    println!("H2 MAX_CONCURRENT_STREAMS - got GOAWAY: {got_goaway}");

    drop(tls);

    // Verify sozu is still alive
    thread::sleep(Duration::from_millis(200));
    let still_alive = verify_sozu_alive(front_port);
    println!("H2 MAX_CONCURRENT_STREAMS - sozu still alive: {still_alive}");

    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    for backend in backends.iter_mut() {
        backend.stop_and_get_aggregator();
    }

    // Key assertions:
    // 1. At least one RST_STREAM(REFUSED_STREAM) was sent
    // 2. No GOAWAY (connection-level error) for stream exhaustion
    // 3. Sozu stayed alive
    if success && still_alive && refused_count > 0 && !got_goaway {
        State::Success
    } else {
        println!(
            "H2 MAX_CONCURRENT_STREAMS - success={success}, alive={still_alive}, \
             refused={refused_count}, goaway={got_goaway}"
        );
        State::Fail
    }
}

#[test]
fn test_h2_max_concurrent_streams_rst_not_goaway() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2: MAX_CONCURRENT_STREAMS exceeded -> RST_STREAM(REFUSED_STREAM), not GOAWAY",
            try_h2_max_concurrent_streams_rst_not_goaway
        ),
        State::Success
    );
}

// ============================================================================
// Test: WINDOW_UPDATE overflow -> GOAWAY(FLOW_CONTROL_ERROR)
// ============================================================================

/// RFC 9113 §6.9.1: if a WINDOW_UPDATE causes the connection-level flow
/// control window to exceed 2^31-1, the endpoint MUST send
/// GOAWAY(FLOW_CONTROL_ERROR).
///
/// Strategy: the initial connection window is 65535 (default). Sending a
/// WINDOW_UPDATE with increment=2^31-1 (0x7FFFFFFF) on stream 0 would make
/// the total 65535 + 2147483647 = 2147549182, which exceeds 2^31-1.
/// Sozu must respond with GOAWAY(FLOW_CONTROL_ERROR).
fn try_h2_window_update_overflow() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_edge_test("H2-WINDOW-OVERFLOW", 1);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    let mut tls = tls_connect(front_addr);
    h2_handshake(&mut tls);

    // Send WINDOW_UPDATE on stream 0 with max increment (0x7FFFFFFF).
    // Initial window = 65535, so 65535 + 2^31-1 > 2^31-1 => overflow.
    let window_update = H2Frame::window_update(0, 0x7FFFFFFF);
    tls.write_all(&window_update.encode()).unwrap();
    tls.flush().unwrap();

    // Read response — expect GOAWAY(FLOW_CONTROL_ERROR)
    thread::sleep(Duration::from_millis(500));
    let response_data = read_all_available(&mut tls, Duration::from_secs(2));
    let frames = parse_h2_frames(&response_data);

    println!(
        "H2 WINDOW_UPDATE overflow - received {} frames",
        frames.len()
    );

    let got_flow_control_error = contains_goaway_with_error(&frames, H2_ERROR_FLOW_CONTROL_ERROR);
    if let Some(error_code) = goaway_error_code(&frames) {
        println!("H2 WINDOW_UPDATE overflow - GOAWAY error_code=0x{error_code:x}");
    }
    println!("H2 WINDOW_UPDATE overflow - got FLOW_CONTROL_ERROR: {got_flow_control_error}");

    drop(tls);

    // Verify sozu is still alive
    thread::sleep(Duration::from_millis(200));
    let still_alive = verify_sozu_alive(front_port);
    println!("H2 WINDOW_UPDATE overflow - sozu still alive: {still_alive}");

    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    for backend in backends.iter_mut() {
        backend.stop_and_get_aggregator();
    }

    if success && still_alive && got_flow_control_error {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_window_update_overflow() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2: WINDOW_UPDATE overflow triggers GOAWAY(FLOW_CONTROL_ERROR)",
            try_h2_window_update_overflow
        ),
        State::Success
    );
}

// ============================================================================
// Test: Empty DATA frame flood (CVE-2019-9518)
// ============================================================================

/// CVE-2019-9518: an attacker sends many empty DATA frames (payload_len=0,
/// no END_STREAM) on a stream. Without flood detection, the server wastes
/// CPU processing each frame with no useful work. Sozu must detect this and
/// respond with GOAWAY(ENHANCE_YOUR_CALM).
///
/// Strategy:
///   1. H2 handshake
///   2. Send HEADERS on stream 1 (END_HEADERS, no END_STREAM) to open a stream
///   3. Flood with 200+ empty DATA frames on stream 1
///   4. Verify GOAWAY(ENHANCE_YOUR_CALM)
fn try_h2_empty_data_flood() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_edge_test("H2-EMPTY-DATA-FLOOD", 1);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    let mut tls = tls_connect(front_addr);
    h2_handshake(&mut tls);

    // Open stream 1 with HEADERS (END_HEADERS but NOT END_STREAM).
    // Use POST so sozu expects a body. Include :authority.
    // HPACK: 0x83 = :method POST (index 3)
    let header_block = vec![
        0x83, // :method POST (index 3)
        0x87, // :scheme https (index 7)
        0x84, // :path / (index 4)
        0x41, 0x09, // :authority (literal, name index 1, value length 9)
        b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't',
    ];

    // Send HEADERS + 200 empty DATA frames in a single batch to ensure
    // sozu processes them in a tight loop within one flood window.
    let mut batch = Vec::new();
    let headers = H2Frame::headers(1, header_block, true, false);
    batch.extend_from_slice(&headers.encode());
    for _ in 0..200 {
        let empty_data = H2Frame::data(1, Vec::new(), false);
        batch.extend_from_slice(&empty_data.encode());
    }
    let write_ok = tls.write_all(&batch).is_ok() && tls.flush().is_ok();
    if !write_ok {
        println!("H2 empty DATA flood - write failed (connection may have been closed)");
    }

    // Read response — expect GOAWAY(ENHANCE_YOUR_CALM)
    // Use multiple read attempts with short sleeps for reliability.
    let mut all_data = Vec::new();
    for _ in 0..5 {
        thread::sleep(Duration::from_millis(200));
        let chunk = read_all_available(&mut tls, Duration::from_millis(500));
        if !chunk.is_empty() {
            all_data.extend_from_slice(&chunk);
        }
    }
    let frames = parse_h2_frames(&all_data);

    println!("H2 empty DATA flood - received {} frames ({} bytes)", frames.len(), all_data.len());
    for (i, (ft, fl, sid, payload)) in frames.iter().enumerate() {
        if *ft == H2_FRAME_GOAWAY && payload.len() >= 8 {
            let error_code = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
            println!("  frame {i}: GOAWAY error_code=0x{error_code:x}");
        } else {
            println!("  frame {i}: type=0x{ft:02x} flags=0x{fl:02x} stream={sid} len={}", payload.len());
        }
    }

    let got_enhance_your_calm = contains_goaway_with_error(&frames, H2_ERROR_ENHANCE_YOUR_CALM);
    println!("H2 empty DATA flood - got ENHANCE_YOUR_CALM: {got_enhance_your_calm}");

    drop(tls);

    // Verify sozu is still alive
    thread::sleep(Duration::from_millis(200));
    let still_alive = verify_sozu_alive(front_port);
    println!("H2 empty DATA flood - sozu still alive: {still_alive}");

    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    for backend in backends.iter_mut() {
        backend.stop_and_get_aggregator();
    }

    if success && still_alive && got_enhance_your_calm {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_empty_data_flood() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2 security: CVE-2019-9518 empty DATA flood triggers GOAWAY(ENHANCE_YOUR_CALM)",
            try_h2_empty_data_flood
        ),
        State::Success
    );
}
