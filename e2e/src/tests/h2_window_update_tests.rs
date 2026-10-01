//! How often Sōzu returns flow-control credit while it receives a large
//! body, in both H2 positions (RFC 9113 §6.9).
//!
//! - `Position::Server`: a client uploads [`TRANSFER`] bytes to Sōzu, which
//!   forwards them to an H2 backend; the client counts the WINDOW_UPDATE
//!   frames Sōzu sends it.
//! - `Position::Client`: an H2 backend answers with [`TRANSFER`] bytes;
//!   the backend counts the WINDOW_UPDATE frames Sōzu sends it.
//!
//! Every sender here honours the windows Sōzu grants it, so each transfer
//! completing proves that the credit Sōzu returns never stalls a stream. The
//! connection-level bound is derived from the policy, not measured: the
//! one-shot enlargement, then one stream-0 WINDOW_UPDATE per half of the
//! advertised connection window received. Stream-level credit is still
//! returned per DATA frame; those frames are counted and printed only.

use std::{
    io::{ErrorKind, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    thread,
    time::{Duration, Instant},
};

use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, AddCertificate, CertificateAndKey, ListenerType, RequestHttpFrontend,
        SocketAddress, request::RequestType,
    },
};

use crate::{
    port_registry::bind_std_listener,
    sozu::worker::Worker,
    tests::{h2_utils::*, provide_port, tests::create_local_address},
};

/// Body size moved through Sōzu in each direction.
const TRANSFER: usize = 64 * 1024 * 1024;
/// Sōzu's default `h2_initial_connection_window`.
const CONNECTION_WINDOW: usize = 16 * 1024 * 1024;
/// RFC 9113 §6.5.2 default, what Sōzu advertises as its stream window.
const DEFAULT_STREAM_WINDOW: u32 = 65_535;
/// RFC 9113 §6.9.1: the largest window a sender may hold.
const MAX_WINDOW: u32 = 0x7FFF_FFFF;
/// Largest DATA payload Sōzu accepts (its `SETTINGS_MAX_FRAME_SIZE`).
const MAX_FRAME: usize = 16_384;
/// Whole-transfer deadline.
const DEADLINE: Duration = Duration::from_secs(120);

/// One end of a raw H2 connection that honours the peer's flow control and
/// records every WINDOW_UPDATE the peer sends.
struct Peer<S: Read + Write> {
    io: S,
    carry: Vec<u8>,
    /// Our connection send window, granted by the peer.
    conn_window: i64,
    /// Our send window on the one stream these tests use.
    stream_window: i64,
    /// The peer's `SETTINGS_INITIAL_WINDOW_SIZE`, once seen.
    peer_stream_window: Option<u32>,
    /// Increments of the stream-0 WINDOW_UPDATEs received.
    conn_updates: Vec<u32>,
    /// Increments of the stream-level WINDOW_UPDATEs received.
    stream_updates: Vec<u32>,
    /// DATA octets received on any stream.
    data_received: usize,
    /// Whether END_STREAM arrived on a frame other than WINDOW_UPDATE.
    end_stream: bool,
    /// Whether a HEADERS frame arrived.
    headers: bool,
    /// Stream of the last HEADERS frame received.
    headers_stream: u32,
    /// Unexpected frames: RST_STREAM or GOAWAY.
    errors: Vec<(u8, u32, Vec<u8>)>,
}

impl<S: Read + Write> Peer<S> {
    fn new(io: S) -> Self {
        Self {
            io,
            carry: Vec::new(),
            conn_window: 65_535,
            stream_window: i64::from(DEFAULT_STREAM_WINDOW),
            peer_stream_window: None,
            conn_updates: Vec::new(),
            stream_updates: Vec::new(),
            data_received: 0,
            end_stream: false,
            headers: false,
            headers_stream: 0,
            errors: Vec::new(),
        }
    }

    fn send(&mut self, frame: H2Frame) {
        self.io.write_all(&frame.encode()).expect("write frame");
    }

    /// Advertise the largest windows RFC 9113 allows, so this end never
    /// has to return credit itself.
    fn send_open_settings(&mut self) {
        self.send(H2Frame::settings(&[(0x4, MAX_WINDOW)]));
        self.send(H2Frame::window_update(0, MAX_WINDOW - 65_535));
        self.io.flush().expect("flush settings");
    }

    /// Read what is available (short socket timeout) and apply every
    /// complete frame.
    fn pump(&mut self) {
        let mut buf = [0u8; 65_536];
        match self.io.read(&mut buf) {
            Ok(0) => panic!("peer closed the connection"),
            Ok(n) => self.carry.extend_from_slice(&buf[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => panic!("read failed: {e}"),
        }
        while let Some((kind, flags, stream_id, payload)) = advance_one_frame(&mut self.carry) {
            match kind {
                H2_FRAME_WINDOW_UPDATE => {
                    let increment =
                        u32::from_be_bytes([payload[0] & 0x7F, payload[1], payload[2], payload[3]]);
                    if stream_id == 0 {
                        self.conn_updates.push(increment);
                        self.conn_window += i64::from(increment);
                    } else {
                        self.stream_updates.push(increment);
                        self.stream_window += i64::from(increment);
                    }
                    assert!(
                        self.conn_window <= i64::from(MAX_WINDOW),
                        "connection window overflow"
                    );
                    assert!(
                        self.stream_window <= i64::from(MAX_WINDOW),
                        "stream window overflow"
                    );
                }
                H2_FRAME_SETTINGS if flags & H2_FLAG_ACK == 0 => {
                    for entry in payload.chunks_exact(6) {
                        if u16::from_be_bytes([entry[0], entry[1]]) == 0x4 {
                            let value =
                                u32::from_be_bytes([entry[2], entry[3], entry[4], entry[5]]);
                            let previous = self.peer_stream_window.unwrap_or(DEFAULT_STREAM_WINDOW);
                            self.stream_window += i64::from(value) - i64::from(previous);
                            self.peer_stream_window = Some(value);
                        }
                    }
                    self.peer_stream_window.get_or_insert(DEFAULT_STREAM_WINDOW);
                    self.send(H2Frame::settings_ack());
                    self.io.flush().expect("flush settings ack");
                }
                H2_FRAME_DATA => {
                    self.data_received += payload.len();
                    self.end_stream |= flags & H2_FLAG_END_STREAM != 0;
                }
                H2_FRAME_HEADERS => {
                    self.headers = true;
                    self.headers_stream = stream_id;
                    self.end_stream |= flags & H2_FLAG_END_STREAM != 0;
                }
                H2_FRAME_RST_STREAM | H2_FRAME_GOAWAY => {
                    self.errors.push((kind, stream_id, payload));
                }
                _ => {}
            }
        }
        assert!(self.errors.is_empty(), "peer reset: {:?}", self.errors);
    }

    fn pump_until(&mut self, what: &str, done: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + DEADLINE;
        while !done(self) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            self.pump();
        }
    }

    /// Send `len` body octets on `stream_id`, never past the granted windows.
    fn send_body(&mut self, stream_id: u32, len: usize) {
        self.pump_until("the peer SETTINGS", |peer| {
            peer.peer_stream_window.is_some()
        });
        let chunk = vec![0x61u8; MAX_FRAME];
        let deadline = Instant::now() + DEADLINE;
        let mut sent = 0;
        while sent < len {
            let room = self.conn_window.min(self.stream_window).max(0) as usize;
            let size = room.min(MAX_FRAME).min(len - sent);
            if size == 0 {
                assert!(
                    Instant::now() < deadline,
                    "flow control stalled after {sent} of {len} octets"
                );
                self.io.flush().expect("flush body");
                self.pump();
                continue;
            }
            let last = sent + size == len;
            self.send(H2Frame::data(stream_id, chunk[..size].to_vec(), last));
            self.conn_window -= size as i64;
            self.stream_window -= size as i64;
            sent += size;
        }
        self.io.flush().expect("flush body");
    }

    /// Check the stream-0 WINDOW_UPDATEs received while `TRANSFER` octets
    /// were sent: each grant after the enlargement returns at least half of
    /// the connection window, and grants never exceed what was sent.
    fn assert_window_update_policy(&self, position: &str) {
        let max_conn_updates = 1 + TRANSFER / (CONNECTION_WINDOW / 2);
        println!(
            "{position}: {} connection WINDOW_UPDATE (max {max_conn_updates}), {} stream WINDOW_UPDATE",
            self.conn_updates.len(),
            self.stream_updates.len(),
        );
        assert!(
            self.conn_updates.len() <= max_conn_updates,
            "{position}: {} connection WINDOW_UPDATE frames for {TRANSFER} octets, expected at most {max_conn_updates}",
            self.conn_updates.len()
        );
    }
}

/// HPACK block: `:method POST`, `:path /`, `:scheme https`,
/// `:authority localhost`.
fn post_headers() -> Vec<u8> {
    let mut block = vec![0x83, 0x84, 0x87, 0x41, 0x09];
    block.extend_from_slice(b"localhost");
    block
}

/// HPACK block: `:method GET`, `:path /`, `:scheme https`,
/// `:authority localhost`.
fn get_headers() -> Vec<u8> {
    let mut block = vec![0x82, 0x84, 0x87, 0x41, 0x09];
    block.extend_from_slice(b"localhost");
    block
}

/// HPACK block: `:status 200`.
const STATUS_200: u8 = 0x88;

/// HTTPS listener whose cluster speaks H2 to `back_address`.
fn setup_h2_backend_cluster(name: &str, back_address: SocketAddr) -> (Worker, u16) {
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
        interface: None,
        address: front_address.clone(),
        proxy: ListenerType::Https.into(),
        from_scm: false,
    }));
    let mut cluster = Worker::default_cluster("cluster_0");
    cluster.http2 = Some(true);
    worker.send_proxy_request_type(RequestType::AddCluster(cluster));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: String::from("localhost"),
        ..Worker::default_http_frontend("cluster_0", front_address.clone().into())
    }));
    worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
        address: front_address,
        certificate: CertificateAndKey {
            certificate: String::from(include_str!("../../../lib/assets/local-certificate.pem")),
            key: String::from(include_str!("../../../lib/assets/local-key.pem")),
            certificate_chain: vec![],
            versions: vec![],
            names: vec![],
        },
        expired_at: None,
    }));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0".to_owned(),
        back_address,
        None,
    )));
    worker.read_to_last();
    (worker, front_port)
}

/// Accept Sōzu's backend connection and consume the client preface.
fn accept_backend(listener: &TcpListener) -> Peer<TcpStream> {
    let (mut socket, _) = listener.accept().expect("accept sozu");
    // Like real peers, and so Nagle never holds back the tail of a frame.
    socket.set_nodelay(true).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut preface = [0u8; 24];
    socket.read_exact(&mut preface).expect("client preface");
    assert_eq!(&preface[..], H2_CLIENT_PREFACE);
    socket
        .set_read_timeout(Some(Duration::from_millis(5)))
        .unwrap();
    Peer::new(socket)
}

fn connect_client(
    front_port: u16,
) -> Peer<rustls::StreamOwned<rustls::ClientConnection, TcpStream>> {
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    tls.sock.set_nodelay(true).unwrap();
    // The TLS handshake completes inside this first write, under the
    // helper's 2 s timeouts; only then switch to short polling reads.
    tls.write_all(H2_CLIENT_PREFACE).unwrap();
    tls.flush().unwrap();
    tls.sock
        .set_read_timeout(Some(Duration::from_millis(5)))
        .unwrap();
    tls.sock
        .set_write_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    Peer::new(tls)
}

fn finish(worker: Worker, front_port: u16, backend: thread::JoinHandle<()>) {
    backend.join().expect("backend thread");
    assert!(
        teardown((), front_port, worker, vec![]),
        "sozu must survive the transfer"
    );
}

/// `Position::Server`: Sōzu receives a 64 MiB upload from the client.
#[test]
fn test_h2_upload_connection_window_updates_are_bounded() {
    let back_address = create_local_address();
    let listener = bind_std_listener(back_address, "raw H2 backend");
    let backend = thread::spawn(move || {
        let mut sozu = accept_backend(&listener);
        sozu.send_open_settings();
        sozu.pump_until("the request body", |peer| peer.end_stream);
        assert_eq!(sozu.data_received, TRANSFER, "backend body size");
        let stream_id = sozu.headers_stream;
        sozu.send(H2Frame::headers(stream_id, vec![STATUS_200], true, true));
        sozu.io.flush().unwrap();
        // Keep the connection up until Sōzu forwards the response.
        thread::sleep(Duration::from_millis(500));
    });
    let (worker, front_port) = setup_h2_backend_cluster("H2-WU-UPLOAD", back_address);

    let mut client = connect_client(front_port);
    // Default windows: this client only receives one HEADERS frame.
    client.send(H2Frame::settings(&[]));
    client.send(H2Frame::headers(1, post_headers(), true, false));
    client.send_body(1, TRANSFER);
    client.pump_until("the response", |peer| peer.headers && peer.end_stream);
    client.assert_window_update_policy("H2 server position");
    drop(client);
    finish(worker, front_port, backend);
}

/// `Position::Client`: Sōzu receives a 64 MiB response from the backend.
#[test]
fn test_h2_download_connection_window_updates_are_bounded() {
    let back_address = create_local_address();
    let listener = bind_std_listener(back_address, "raw H2 backend");
    let backend = thread::spawn(move || {
        let mut sozu = accept_backend(&listener);
        sozu.send(H2Frame::settings(&[]));
        sozu.io.flush().unwrap();
        sozu.pump_until("the request", |peer| peer.headers && peer.end_stream);
        let stream_id = sozu.headers_stream;
        sozu.send(H2Frame::headers(stream_id, vec![STATUS_200], true, false));
        sozu.send_body(stream_id, TRANSFER);
        sozu.assert_window_update_policy("H2 client position");
        // Keep the connection up until the client read the whole body.
        thread::sleep(Duration::from_secs(2));
    });
    let (worker, front_port) = setup_h2_backend_cluster("H2-WU-DOWNLOAD", back_address);

    let mut client = connect_client(front_port);
    client.send_open_settings();
    client.send(H2Frame::headers(1, get_headers(), true, true));
    client.io.flush().unwrap();
    client.pump_until("the response body", |peer| peer.end_stream);
    assert_eq!(client.data_received, TRANSFER, "client body size");
    drop(client);
    finish(worker, front_port, backend);
}
