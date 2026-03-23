//! Reusable H2 test utilities for crafting raw frames, establishing TLS
//! connections, performing handshakes, and asserting protocol-level behavior.
//!
//! This module extracts shared infrastructure from `h2_tests.rs` and
//! `h2_security_tests.rs` so that all H2 e2e tests can use a single set
//! of well-tested helpers.

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    sync::Arc,
    thread,
    time::Duration,
};

use rustls::ClientConfig;
use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, AddCertificate, CertificateAndKey, ListenerType, RequestHttpFrontend,
        SocketAddress, request::RequestType,
    },
};

use crate::{
    mock::{
        aggregator::SimpleAggregator, async_backend::BackendHandle as AsyncBackend,
        https_client::Verifier,
    },
    sozu::worker::Worker,
    tests::{provide_port, tests::create_local_address},
};

// ============================================================================
// H2 frame type constants (RFC 9113 Section 6)
// ============================================================================

/// DATA frame type (0x0) -- RFC 9113 Section 6.1
pub(crate) const H2_FRAME_DATA: u8 = 0x0;
/// HEADERS frame type (0x1) -- RFC 9113 Section 6.2
pub(crate) const H2_FRAME_HEADERS: u8 = 0x1;
/// RST_STREAM frame type (0x3) -- RFC 9113 Section 6.4
pub(crate) const H2_FRAME_RST_STREAM: u8 = 0x3;
/// SETTINGS frame type (0x4) -- RFC 9113 Section 6.5
pub(crate) const H2_FRAME_SETTINGS: u8 = 0x4;
/// PING frame type (0x6) -- RFC 9113 Section 6.7
pub(crate) const H2_FRAME_PING: u8 = 0x6;
/// GOAWAY frame type (0x7) -- RFC 9113 Section 6.8
pub(crate) const H2_FRAME_GOAWAY: u8 = 0x7;
/// WINDOW_UPDATE frame type (0x8) -- RFC 9113 Section 6.9
pub(crate) const H2_FRAME_WINDOW_UPDATE: u8 = 0x8;
/// CONTINUATION frame type (0x9) -- RFC 9113 Section 6.10
pub(crate) const H2_FRAME_CONTINUATION: u8 = 0x9;

// ============================================================================
// H2 flag constants (RFC 9113)
// ============================================================================

/// ACK flag (0x1) -- used for SETTINGS and PING
pub(crate) const H2_FLAG_ACK: u8 = 0x1;
/// END_STREAM flag (0x1) -- used for DATA and HEADERS
pub(crate) const H2_FLAG_END_STREAM: u8 = 0x1;
/// END_HEADERS flag (0x4) -- used for HEADERS and CONTINUATION
pub(crate) const H2_FLAG_END_HEADERS: u8 = 0x4;

// ============================================================================
// H2 error codes (RFC 9113 Section 7)
// ============================================================================

/// PROTOCOL_ERROR (0x1)
pub(crate) const H2_ERROR_PROTOCOL_ERROR: u32 = 0x1;
/// FLOW_CONTROL_ERROR (0x3)
pub(crate) const H2_ERROR_FLOW_CONTROL_ERROR: u32 = 0x3;
/// FRAME_SIZE_ERROR (0x6)
pub(crate) const H2_ERROR_FRAME_SIZE_ERROR: u32 = 0x6;
/// REFUSED_STREAM (0x7)
pub(crate) const H2_ERROR_REFUSED_STREAM: u32 = 0x7;
/// ENHANCE_YOUR_CALM (0xb)
pub(crate) const H2_ERROR_ENHANCE_YOUR_CALM: u32 = 0xb;

/// The HTTP/2 connection preface sent by the client (RFC 9113 Section 3.4).
pub(crate) const H2_CLIENT_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

// ============================================================================
// H2 SETTINGS identifiers (RFC 9113 Section 6.5.2)
// ============================================================================

const SETTINGS_HEADER_TABLE_SIZE: u16 = 0x1;
const SETTINGS_MAX_CONCURRENT_STREAMS: u16 = 0x3;
const SETTINGS_INITIAL_WINDOW_SIZE: u16 = 0x4;
const SETTINGS_MAX_FRAME_SIZE: u16 = 0x5;
const SETTINGS_MAX_HEADER_LIST_SIZE: u16 = 0x6;

// ============================================================================
// Raw H2 frame builder
// ============================================================================

/// A minimal H2 frame builder for crafting raw frames in tests.
pub(crate) struct H2Frame {
    pub(crate) frame_type: u8,
    pub(crate) flags: u8,
    pub(crate) stream_id: u32,
    pub(crate) payload: Vec<u8>,
}

impl H2Frame {
    pub(crate) fn new(frame_type: u8, flags: u8, stream_id: u32, payload: Vec<u8>) -> Self {
        Self {
            frame_type,
            flags,
            stream_id,
            payload,
        }
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
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
    pub(crate) fn settings(settings: &[(u16, u32)]) -> Self {
        let mut payload = Vec::new();
        for &(id, value) in settings {
            payload.extend_from_slice(&id.to_be_bytes());
            payload.extend_from_slice(&value.to_be_bytes());
        }
        Self::new(H2_FRAME_SETTINGS, 0, 0, payload)
    }

    /// Build a SETTINGS ACK frame.
    pub(crate) fn settings_ack() -> Self {
        Self::new(H2_FRAME_SETTINGS, 0x1, 0, Vec::new())
    }

    /// Build a HEADERS frame. If `end_headers` is false, CONTINUATION is expected.
    pub(crate) fn headers(
        stream_id: u32,
        header_block: Vec<u8>,
        end_headers: bool,
        end_stream: bool,
    ) -> Self {
        let mut flags = 0u8;
        if end_headers {
            flags |= H2_FLAG_END_HEADERS;
        }
        if end_stream {
            flags |= H2_FLAG_END_STREAM;
        }
        Self::new(H2_FRAME_HEADERS, flags, stream_id, header_block)
    }

    /// Build a CONTINUATION frame.
    pub(crate) fn continuation(stream_id: u32, header_block: Vec<u8>, end_headers: bool) -> Self {
        let flags = if end_headers { H2_FLAG_END_HEADERS } else { 0 };
        Self::new(H2_FRAME_CONTINUATION, flags, stream_id, header_block)
    }

    /// Build a WINDOW_UPDATE frame.
    pub(crate) fn window_update(stream_id: u32, increment: u32) -> Self {
        let payload = (increment & 0x7FFFFFFF).to_be_bytes().to_vec();
        Self::new(H2_FRAME_WINDOW_UPDATE, 0, stream_id, payload)
    }

    /// Build a DATA frame on stream 0 (invalid per spec).
    pub(crate) fn data_on_stream_zero(payload: Vec<u8>) -> Self {
        Self::new(H2_FRAME_DATA, 0, 0, payload)
    }

    /// Build a frame with an unknown/invalid type.
    pub(crate) fn invalid_type(stream_id: u32) -> Self {
        // Frame type 0xFF is not defined in the spec
        Self::new(0xFF, 0, stream_id, vec![0xDE, 0xAD])
    }

    /// Build a SETTINGS frame on a non-zero stream (invalid per spec).
    pub(crate) fn settings_on_stream(stream_id: u32) -> Self {
        Self::new(H2_FRAME_SETTINGS, 0, stream_id, Vec::new())
    }

    /// Build an oversized frame -- payload bigger than default max_frame_size (16384).
    pub(crate) fn oversized(stream_id: u32) -> Self {
        let payload = vec![0u8; 16385]; // 1 byte over default max
        Self::new(H2_FRAME_DATA, 0, stream_id, payload)
    }

    /// Build a RST_STREAM frame with the given error code.
    pub(crate) fn rst_stream(stream_id: u32, error_code: u32) -> Self {
        Self::new(
            H2_FRAME_RST_STREAM,
            0,
            stream_id,
            error_code.to_be_bytes().to_vec(),
        )
    }

    /// Build a PING frame (non-ACK) with the given 8-byte payload.
    pub(crate) fn ping(payload: [u8; 8]) -> Self {
        Self::new(H2_FRAME_PING, 0, 0, payload.to_vec())
    }

    /// Build a SETTINGS ACK frame WITH a non-empty payload (invalid per RFC 9113 Section 6.5).
    pub(crate) fn settings_ack_with_payload(payload: Vec<u8>) -> Self {
        Self::new(H2_FRAME_SETTINGS, H2_FLAG_ACK, 0, payload)
    }

    /// Build a DATA frame on a given stream.
    pub(crate) fn data(stream_id: u32, payload: Vec<u8>, end_stream: bool) -> Self {
        let flags = if end_stream { H2_FLAG_END_STREAM } else { 0 };
        Self::new(H2_FRAME_DATA, flags, stream_id, payload)
    }

    /// Build a GOAWAY frame.
    #[allow(dead_code)]
    pub(crate) fn goaway(last_stream_id: u32, error_code: u32) -> Self {
        let mut payload = Vec::with_capacity(8);
        payload.extend_from_slice(&(last_stream_id & 0x7FFFFFFF).to_be_bytes());
        payload.extend_from_slice(&error_code.to_be_bytes());
        Self::new(H2_FRAME_GOAWAY, 0, 0, payload)
    }
}

// ============================================================================
// Frame I/O helpers
// ============================================================================

/// Read raw bytes from a stream, tolerating timeouts.
pub(crate) fn read_all_available(stream: &mut impl Read, timeout: Duration) -> Vec<u8> {
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
pub(crate) fn parse_h2_frames(data: &[u8]) -> Vec<(u8, u8, u32, Vec<u8>)> {
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

// ============================================================================
// Frame inspection helpers
// ============================================================================

/// Check if any parsed frame is a GOAWAY frame.
pub(crate) fn contains_goaway(frames: &[(u8, u8, u32, Vec<u8>)]) -> bool {
    frames.iter().any(|(t, _, _, _)| *t == H2_FRAME_GOAWAY)
}

/// Check if any parsed frame is a RST_STREAM (type 0x3).
pub(crate) fn contains_rst_stream(frames: &[(u8, u8, u32, Vec<u8>)]) -> bool {
    frames.iter().any(|(t, _, _, _)| *t == H2_FRAME_RST_STREAM)
}

/// Extract the error code from the first GOAWAY frame, if any.
/// GOAWAY payload: 4 bytes last_stream_id + 4 bytes error_code.
pub(crate) fn goaway_error_code(frames: &[(u8, u8, u32, Vec<u8>)]) -> Option<u32> {
    for (t, _, _, payload) in frames {
        if *t == H2_FRAME_GOAWAY && payload.len() >= 8 {
            return Some(u32::from_be_bytes([
                payload[4], payload[5], payload[6], payload[7],
            ]));
        }
    }
    None
}

/// Extract the last_stream_id from the first GOAWAY frame, if any.
pub(crate) fn goaway_last_stream_id(frames: &[(u8, u8, u32, Vec<u8>)]) -> Option<u32> {
    for (t, _, _, payload) in frames {
        if *t == H2_FRAME_GOAWAY && payload.len() >= 8 {
            return Some(u32::from_be_bytes([
                payload[0] & 0x7F,
                payload[1],
                payload[2],
                payload[3],
            ]));
        }
    }
    None
}

/// Check if frames contain a GOAWAY with a specific error code.
pub(crate) fn contains_goaway_with_error(
    frames: &[(u8, u8, u32, Vec<u8>)],
    expected_error: u32,
) -> bool {
    goaway_error_code(frames) == Some(expected_error)
}

/// Extract RST_STREAM frames: returns (stream_id, error_code) tuples.
/// RST_STREAM payload: 4 bytes error_code.
pub(crate) fn extract_rst_streams(frames: &[(u8, u8, u32, Vec<u8>)]) -> Vec<(u32, u32)> {
    frames
        .iter()
        .filter(|(t, _, _, payload)| *t == H2_FRAME_RST_STREAM && payload.len() >= 4)
        .map(|(_, _, sid, payload)| {
            let error_code = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
            (*sid, error_code)
        })
        .collect()
}

/// Check if any RST_STREAM frame has a given error code for a given stream.
#[allow(dead_code)]
pub(crate) fn contains_rst_stream_with_error(
    frames: &[(u8, u8, u32, Vec<u8>)],
    stream_id: u32,
    expected_error: u32,
) -> bool {
    extract_rst_streams(frames)
        .iter()
        .any(|(sid, ec)| *sid == stream_id && *ec == expected_error)
}

// ============================================================================
// TLS connection helper (T-4)
// ============================================================================

/// Create a raw TLS connection with H2 ALPN to the given address.
///
/// Returns a `rustls::StreamOwned` that can be used for raw frame I/O.
/// The connection uses a permissive certificate verifier (accepts self-signed)
/// and sets 2-second read/write timeouts.
pub(crate) fn raw_h2_connection(
    addr: SocketAddr,
) -> rustls::StreamOwned<rustls::ClientConnection, TcpStream> {
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

// ============================================================================
// H2 server settings (T-3)
// ============================================================================

/// Parsed SETTINGS values received from sozu during the H2 handshake.
#[derive(Debug, Default)]
pub(crate) struct H2ServerSettings {
    pub max_concurrent_streams: Option<u32>,
    pub initial_window_size: Option<u32>,
    pub max_frame_size: Option<u32>,
    pub header_table_size: Option<u32>,
    pub max_header_list_size: Option<u32>,
}

/// Parse SETTINGS key-value pairs from a SETTINGS frame payload.
fn parse_settings_payload(payload: &[u8]) -> H2ServerSettings {
    let mut settings = H2ServerSettings::default();
    let mut pos = 0;
    while pos + 6 <= payload.len() {
        let id = u16::from_be_bytes([payload[pos], payload[pos + 1]]);
        let value = u32::from_be_bytes([
            payload[pos + 2],
            payload[pos + 3],
            payload[pos + 4],
            payload[pos + 5],
        ]);
        match id {
            SETTINGS_HEADER_TABLE_SIZE => settings.header_table_size = Some(value),
            SETTINGS_MAX_CONCURRENT_STREAMS => settings.max_concurrent_streams = Some(value),
            SETTINGS_INITIAL_WINDOW_SIZE => settings.initial_window_size = Some(value),
            SETTINGS_MAX_FRAME_SIZE => settings.max_frame_size = Some(value),
            SETTINGS_MAX_HEADER_LIST_SIZE => settings.max_header_list_size = Some(value),
            _ => {} // Unknown settings are ignored per spec
        }
        pos += 6;
    }
    settings
}

// ============================================================================
// H2 handshake (T-3)
// ============================================================================

/// Send the H2 client preface and an initial empty SETTINGS frame, then
/// read and parse the server's SETTINGS. Send SETTINGS ACK and return
/// the parsed server settings.
///
/// Tests that do not need the settings can simply ignore the return value.
pub(crate) fn h2_handshake(
    stream: &mut rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
) -> H2ServerSettings {
    // Send client preface
    stream.write_all(H2_CLIENT_PREFACE).unwrap();
    // Send empty SETTINGS
    stream.write_all(&H2Frame::settings(&[]).encode()).unwrap();
    stream.flush().unwrap();

    // Read server's response (SETTINGS, possibly WINDOW_UPDATE, etc.)
    thread::sleep(Duration::from_millis(200));
    let data = read_all_available(stream, Duration::from_millis(500));

    // Parse frames and extract the first non-ACK SETTINGS frame
    let frames = parse_h2_frames(&data);
    let mut server_settings = H2ServerSettings::default();
    for (ft, fl, _sid, payload) in &frames {
        if *ft == H2_FRAME_SETTINGS && (*fl & H2_FLAG_ACK) == 0 {
            server_settings = parse_settings_payload(payload);
            break;
        }
    }

    // Send SETTINGS ACK
    stream.write_all(&H2Frame::settings_ack().encode()).unwrap();
    stream.flush().unwrap();

    // Small delay for the ACK to be processed
    thread::sleep(Duration::from_millis(50));

    server_settings
}

// ============================================================================
// Test setup helpers (T-1)
// ============================================================================

/// Setup an HTTPS listener with H2 support, a cluster, TLS certificate, and
/// N async backends. Returns (worker, backends, front_port).
///
/// This is the standard H2 test setup that replaces 60-100 lines of
/// boilerplate in each test function.
pub(crate) fn setup_h2_test(
    name: &str,
    nb_backends: usize,
) -> (Worker, Vec<AsyncBackend<SimpleAggregator>>, u16) {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);

    let (config, listeners, state) = Worker::empty_https_config(front_address.clone().into());
    let mut worker = Worker::start_new_worker_owned(name, config, listeners, state);

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

/// Setup HTTPS listener with TLS and a cluster, but do NOT add backends.
/// The caller is responsible for adding backends and calling `worker.read_to_last()`.
pub(crate) fn setup_h2_listener_only(name: &str) -> (Worker, u16, SocketAddress) {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);

    let (config, listeners, state) = Worker::empty_https_config(front_address.clone().into());
    let mut worker = Worker::start_new_worker_owned(name, config, listeners, state);

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

/// Check that sozu is still alive and accepts a new TCP connection.
pub(crate) fn verify_sozu_alive(front_port: u16) -> bool {
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

/// Collect response frames from a TLS stream with multiple read attempts.
/// More reliable than a single read because sozu may need multiple event-loop
/// ticks to process input and emit a response.
pub(crate) fn collect_response_frames(
    tls: &mut impl Read,
    initial_delay_ms: u64,
    attempts: usize,
    read_timeout_ms: u64,
) -> Vec<(u8, u8, u32, Vec<u8>)> {
    thread::sleep(Duration::from_millis(initial_delay_ms));
    let mut all_data = Vec::new();
    for _ in 0..attempts {
        let chunk = read_all_available(tls, Duration::from_millis(read_timeout_ms));
        if !chunk.is_empty() {
            all_data.extend_from_slice(&chunk);
        }
        if all_data.is_empty() {
            thread::sleep(Duration::from_millis(100));
        }
    }
    parse_h2_frames(&all_data)
}

/// Log parsed frames for debugging.
pub(crate) fn log_frames(test_name: &str, frames: &[(u8, u8, u32, Vec<u8>)]) {
    println!("{test_name} - received {} frames", frames.len());
    for (i, (ft, fl, sid, payload)) in frames.iter().enumerate() {
        if *ft == H2_FRAME_GOAWAY && payload.len() >= 8 {
            let error_code = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
            println!("  frame {i}: GOAWAY error_code=0x{error_code:x}");
        } else if *ft == H2_FRAME_RST_STREAM && payload.len() >= 4 {
            let error_code = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
            println!("  frame {i}: RST_STREAM stream={sid} error_code=0x{error_code:x}");
        } else {
            println!(
                "  frame {i}: type=0x{ft:02x} flags=0x{fl:02x} stream={sid} len={}",
                payload.len()
            );
        }
    }
}

/// Standard test teardown: drop TLS, verify sozu is alive, soft-stop worker.
/// Returns true if both sozu-alive check and worker stop succeeded.
pub(crate) fn teardown<T>(
    tls: T,
    front_port: u16,
    mut worker: Worker,
    mut backends: Vec<AsyncBackend<SimpleAggregator>>,
) -> bool {
    drop(tls);
    thread::sleep(Duration::from_millis(200));
    let still_alive = verify_sozu_alive(front_port);
    println!("  sozu still alive: {still_alive}");

    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    for backend in backends.iter_mut() {
        backend.stop_and_get_aggregator();
    }
    success && still_alive
}

// ============================================================================
// Assertion helpers (T-5)
// ============================================================================

/// Assert that the response frames contain a GOAWAY with the specified error code.
///
/// Panics with a descriptive message if no matching GOAWAY is found.
pub(crate) fn assert_goaway_received(frames: &[(u8, u8, u32, Vec<u8>)], expected_error: u32) {
    let actual = goaway_error_code(frames);
    assert!(
        actual == Some(expected_error),
        "Expected GOAWAY with error code 0x{expected_error:x}, got {:?} (frames: {} total)",
        actual.map(|e| format!("0x{e:x}")),
        frames.len()
    );
}

/// Assert that the response frames contain a GOAWAY with a specific error code
/// and last_stream_id.
pub(crate) fn assert_goaway_with_last_stream(
    frames: &[(u8, u8, u32, Vec<u8>)],
    expected_error: u32,
    expected_last_stream: u32,
) {
    let actual_error = goaway_error_code(frames);
    let actual_last_stream = goaway_last_stream_id(frames);
    assert!(
        actual_error == Some(expected_error) && actual_last_stream == Some(expected_last_stream),
        "Expected GOAWAY(error=0x{expected_error:x}, last_stream={expected_last_stream}), \
         got error={:?} last_stream={:?}",
        actual_error.map(|e| format!("0x{e:x}")),
        actual_last_stream
    );
}

/// Assert that a RST_STREAM was received for a specific stream with a specific error.
///
/// Panics with a descriptive message if no matching RST_STREAM is found.
pub(crate) fn assert_rst_stream_received(
    frames: &[(u8, u8, u32, Vec<u8>)],
    stream_id: u32,
    expected_error: u32,
) {
    let rst_streams = extract_rst_streams(frames);
    let found = rst_streams
        .iter()
        .any(|(sid, ec)| *sid == stream_id && *ec == expected_error);
    assert!(
        found,
        "Expected RST_STREAM on stream {stream_id} with error 0x{expected_error:x}, \
         got RST_STREAMs: {rst_streams:?}"
    );
}

/// Assert that SETTINGS ACK was received.
///
/// Panics if no SETTINGS frame with the ACK flag is found.
pub(crate) fn assert_settings_ack_received(frames: &[(u8, u8, u32, Vec<u8>)]) {
    let found = frames
        .iter()
        .any(|(ft, fl, _, _)| *ft == H2_FRAME_SETTINGS && (*fl & H2_FLAG_ACK) != 0);
    assert!(
        found,
        "Expected SETTINGS ACK frame, but none found among {} frames",
        frames.len()
    );
}

/// Check if the response contains a GOAWAY or RST_STREAM indicating a
/// protocol-level rejection.
pub(crate) fn rejected_with_goaway_or_rst(frames: &[(u8, u8, u32, Vec<u8>)]) -> bool {
    contains_goaway(frames) || contains_rst_stream(frames)
}

/// Check if frames contain a HEADERS response (type 0x1) on any stream.
pub(crate) fn contains_headers_response(frames: &[(u8, u8, u32, Vec<u8>)]) -> bool {
    frames.iter().any(|(t, _, _, _)| *t == H2_FRAME_HEADERS)
}

/// Check if frames contain a DATA frame on any stream.
#[allow(dead_code)]
pub(crate) fn contains_data_frame(frames: &[(u8, u8, u32, Vec<u8>)]) -> bool {
    frames.iter().any(|(t, _, _, _)| *t == H2_FRAME_DATA)
}
