//! End-to-end behavioral coverage for TCP passthrough SNI+ALPN preread
//! routing (sozu-proxy/sozu#1279). The feature itself (`SniPrereadCore`,
//! `SniPreread` shell, `TcpSession::upgrade_sni_preread`) is unit/sim/fuzz
//! verified already; this module proves the end-to-end behavior through
//! REAL sockets: a raw TCP client speaking real TLS wire bytes, Sōzu's
//! worker process making a live routing decision, and (for the cases that
//! need it) a TLS-TERMINATING mock backend (`mock::tls_backend`) that
//! completes its own handshake -- proving Sōzu never terminates TLS on
//! this path, only reads far enough to route.
//!
//! Coverage, by section (in file order):
//! - SNI routing to the correct backend by cert, mTLS passthrough,
//!   byte-for-byte ClientHello replay.
//! - the full preread reject matrix, plus positive ALPN routing.
//! - PROXY-protocol composition with the SNI preread (expect / send /
//!   relay / none).
//! - a large payload through an SNI-routed session (splice-agnostic).
//! - per-(cluster, source-IP) counter conservation across a
//!   reject-then-route sequence, and the paired differential that proves
//!   the limiter's rejection is attributable to the cap itself (a second
//!   concurrent connection is admitted once the cap is raised, using the
//!   SAME genuinely-held-open first connection).
//!
//! Metric-observability note (read before touching the counter-conservation
//! test): `tcp.sni_preread.*`
//! are process-wide gauges/counters (`lib/src/metrics/names.rs`), queryable
//! from an e2e test via `QueryMetrics { no_clusters: true, .. }`. The
//! per-(cluster, source-IP) admission count (`lib/src/server.rs`'s
//! `connections_per_cluster_ip`) is a plain in-memory `HashMap`, never
//! published as a named metric (deliberately -- a per-IP-keyed gauge would
//! be a cardinality bomb), so THAT half of the counter-conservation test is
//! a behavioral proxy: a
//! `max_connections_per_ip = 1` cluster must still accept its first real
//! connection after N unrelated preread rejects (no leaked slot), and must
//! still reject a second concurrent one (the limiter itself still works).
//! The OTHER half -- active-count conservation -- is now a LIVE
//! metric assertion against the real `tcp.sni_preread.active` gauge (see
//! the fixed-defects note just below).
//!
//! Defects this suite surfaced (all FOUND by these tests, all now FIXED in
//! `lib` on this branch -- PR sozu-proxy/sozu#1290, implementing the #1279
//! SNI-preread feature -- these
//! notes are the durable record of what the tests below now guard against
//! regressing, not open bugs):
//!
//! - `tcp.sni_preread.active` gauge was decremented on both session exits
//!   but never incremented on entry, so it underflowed (clamped to 0) on
//!   every SNI-preread session and its "active count" was permanently 0.
//!   Fixed: `TcpSession::new_sni_preread` now `+1`s the gauge on entry
//!   (`lib/src/tcp.rs`, the `gauge_add!(...ACTIVE, 1)` next to the
//!   `SniPreread` state construction), matched by the two `-1` exits (the
//!   routed upgrade in `upgrade_sni_preread`, and `close()`'s
//!   `StateMarker::SniPreread` arm).
//!   `test_tcp_sni_reject_then_valid_connection_not_limited`
//!   now asserts the gauge increments while a session is mid-preread AND
//!   returns to a clean 0 after a reject-storm + a routed connection + a
//!   limit-rejected connection -- against the pre-fix gauge (clamped to a
//!   permanent 0) the mid-preread `>= 1` assertion cannot pass, so a
//!   re-regression fails loudly.
//!
//! - A legitimate, small ClientHello immediately followed by a large
//!   payload in the SAME TCP send (no gap -- what any real fast client
//!   does) was mishandled on the coalesced-read path: the preread read was
//!   not capped at `effective_max_bytes` (spurious `RejectReason::TooLarge`
//!   before the backend was ever dialed), and bytes already read from the
//!   frontend and queued for the backend were dropped when the frontend
//!   closed before they flushed. Fixed in `lib`: the preread
//!   read is now hard-capped at `effective_max_bytes`
//!   (`lib/src/protocol/tcp_preread/shell.rs`), and `Pipe` now drains
//!   queued front->back bytes instead of closing on every frontend-close
//!   signal -- the EOF (`SocketResult::Closed`) arm of `readable`, the
//!   `frontend_hup` (EPOLLRDHUP) in-flight branch, and `splice_readable`'s
//!   EOF arm for bytes already spliced into the kernel pipe
//!   (`lib/src/protocol/pipe.rs`).
//!   `test_tcp_sni_large_payload_coalesced_with_hello_delivered_intact`
//!   now asserts the connection routes (not rejected) AND the full
//!   1 MiB payload reaches the backend byte-identical; it is the permanent
//!   regression guard for that path.

use std::{
    io::{ErrorKind, Read, Write},
    net::{SocketAddr, TcpStream},
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use rustls::{
    ClientConfig, ClientConnection, StreamOwned,
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, pem::PemObject},
};
use sozu_command_lib::{
    config::{DEFAULT_SNI_PREREAD_MAX_BYTES, DEFAULT_SNI_PREREAD_TIMEOUT, ListenerBuilder},
    proto::command::{
        ActivateListener, Cluster, ListenerType, ProxyProtocolConfig, QueryMetricsOptions,
        ResponseStatus, filtered_metrics, request::RequestType, response_content::ContentType,
    },
};

use crate::{
    mock::{https_client::Verifier, tls_backend},
    port_registry::bind_std_listener,
    sozu::worker::Worker,
    tests::{State, repeat_until_error_or},
};

use super::tests::create_local_address;

// =========================================================================
// TLS fixture certs (e2e/assets/tcp_sni/, see its README.md)
// =========================================================================

const CA_CERT: &[u8] = include_bytes!("../../assets/tcp_sni/ca-cert.pem");
const BACKEND_A_CERT: &[u8] = include_bytes!("../../assets/tcp_sni/backend-a-cert.pem");
const BACKEND_A_KEY: &[u8] = include_bytes!("../../assets/tcp_sni/backend-a-key.pem");
const BACKEND_B_CERT: &[u8] = include_bytes!("../../assets/tcp_sni/backend-b-cert.pem");
const BACKEND_B_KEY: &[u8] = include_bytes!("../../assets/tcp_sni/backend-b-key.pem");
const MTLS_BACKEND_CERT: &[u8] = include_bytes!("../../assets/tcp_sni/mtls-backend-cert.pem");
const MTLS_BACKEND_KEY: &[u8] = include_bytes!("../../assets/tcp_sni/mtls-backend-key.pem");
const MTLS_CLIENT_CERT: &[u8] = include_bytes!("../../assets/tcp_sni/mtls-client-cert.pem");
const MTLS_CLIENT_KEY: &[u8] = include_bytes!("../../assets/tcp_sni/mtls-client-key.pem");

// =========================================================================
// Worker / listener / route setup helpers
// =========================================================================

/// One `RequestTcpFrontend` + its cluster + single backend, as consumed by
/// [`setup_sni_worker`].
struct SniRoute {
    sni: Option<&'static str>,
    alpn: &'static [&'static str],
    cluster_id: &'static str,
    proxy_protocol: Option<ProxyProtocolConfig>,
    backend_address: SocketAddr,
    max_connections_per_ip: Option<u64>,
}

/// Boot a worker with ONE SNI-preread TCP listener at `front_address` plus
/// every route in `routes`, each wired to its own cluster + single
/// backend. `expect_proxy` toggles the listener's inbound PROXY-v2
/// preread (the PROXY-protocol composition tests);
/// `sni_preread_timeout`/`sni_preread_max_bytes` override
/// the preread budget (the timeout/oversized reject cases).
fn setup_sni_worker(
    name: &str,
    front_address: SocketAddr,
    expect_proxy: bool,
    sni_preread_timeout: u32,
    sni_preread_max_bytes: u32,
    routes: &[SniRoute],
) -> Worker {
    let (config, listeners, state) = Worker::empty_tcp_config(front_address);
    let mut worker = Worker::start_new_worker_owned(name, config, listeners, state);

    let mut builder = ListenerBuilder::new_tcp(front_address.into());
    builder.with_expect_proxy(expect_proxy);
    builder.sni_preread_timeout = Some(sni_preread_timeout);
    builder.sni_preread_max_bytes = Some(sni_preread_max_bytes);
    let listener_config = builder
        .to_tcp(None)
        .expect("build SNI-preread TCP listener config");

    worker.send_proxy_request_type(RequestType::AddTcpListener(listener_config));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.into(),
        proxy: ListenerType::Tcp.into(),
        from_scm: false,
    }));

    for route in routes {
        worker.send_proxy_request_type(RequestType::AddCluster(Cluster {
            proxy_protocol: route.proxy_protocol.map(|p| p as i32),
            max_connections_per_ip: route.max_connections_per_ip,
            ..Worker::default_cluster(route.cluster_id)
        }));
        worker.send_proxy_request_type(RequestType::AddTcpFrontend(Worker::sni_tcp_frontend(
            route.cluster_id,
            front_address,
            route.sni,
            route.alpn,
        )));
        worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
            route.cluster_id,
            format!("{}-0", route.cluster_id),
            route.backend_address,
            None,
        )));
    }

    worker.read_to_last();
    worker
}

/// Convenience wrapper for the many reject-matrix cases: a listener
/// with exactly ONE real SNI route (`known.example.com`, never actually
/// reached by these tests). A listener needs at least one route at all --
/// `TcpProxy::create_session` refuses the connection outright when BOTH
/// `cluster_id` (no-SNI catch-all) and `sni_routes` are empty -- so an
/// empty route table would test "no listener configured" rather than
/// "the preread core rejected this ClientHello", which is the thing these
/// tests actually want to exercise.
fn setup_single_sni_route_worker(
    name: &str,
    front_address: SocketAddr,
    sni_preread_timeout: u32,
    sni_preread_max_bytes: u32,
) -> Worker {
    let unreachable_backend = create_local_address();
    setup_sni_worker(
        name,
        front_address,
        false,
        sni_preread_timeout,
        sni_preread_max_bytes,
        &[SniRoute {
            sni: Some("known.example.com"),
            alpn: &[],
            cluster_id: "known_cluster",
            proxy_protocol: None,
            backend_address: unreachable_backend,
            max_connections_per_ip: None,
        }],
    )
}

// =========================================================================
// TLS / raw-byte client + backend helpers
// =========================================================================

/// Permissive `ClientConfig` shared by every in-test TLS client in this
/// module: the backend certs are self-signed test fixtures with no path
/// to a system trust store, and validating them is not what these tests
/// are about (routing/passthrough is) -- reuses the SAME insecure
/// `Verifier` `tls_tests.rs` already relies on.
fn insecure_client_config(alpn: &[&[u8]]) -> ClientConfig {
    let mut config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Verifier))
        .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    config
}

/// Same as [`insecure_client_config`] but presents `cert_pem`/`key_pem` as
/// the client's OWN identity (the mTLS positive-space case).
fn insecure_client_config_with_identity(
    alpn: &[&[u8]],
    cert_pem: &[u8],
    key_pem: &[u8],
) -> ClientConfig {
    let cert = CertificateDer::from_pem_slice(cert_pem).expect("parse client cert PEM");
    let key = PrivateKeyDer::from_pem_slice(key_pem).expect("parse client key PEM");
    let mut config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Verifier))
        .with_client_auth_cert(vec![cert], key)
        .expect("attach client identity cert+key");
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    config
}

/// Produce the raw wire bytes of a REAL TLS ClientHello (record layer
/// included) for `sni` + `alpn`, without ever opening a socket --
/// `rustls::ClientConnection` queues its ClientHello into an internal
/// output buffer synchronously at construction, and `write_tls` drains
/// that buffer. Every reject/composition/replay test below sends these
/// bytes over a raw `TcpStream` it drives directly (never a
/// `rustls::StreamOwned`) because Sōzu NEVER answers as a TLS server on
/// this path -- it either forwards bytes to a backend or silently closes,
/// so there is no ServerHello for a "real" handshake to complete against.
fn build_client_hello(sni: &str, alpn: &[&[u8]]) -> Vec<u8> {
    let config = insecure_client_config(alpn);
    let server_name = ServerName::try_from(sni.to_owned()).expect("valid DNS name for test SNI");
    let mut conn =
        ClientConnection::new(Arc::new(config), server_name).expect("build ClientConnection");
    let mut buf = Vec::new();
    conn.write_tls(&mut buf)
        .expect("write_tls must drain the queued ClientHello");
    assert!(
        !buf.is_empty(),
        "a fresh ClientConnection must queue a non-empty ClientHello"
    );
    buf
}

/// Same as [`build_client_hello`] but with the SNI extension itself
/// suppressed at the protocol level (the "valid ClientHello, no SNI
/// extension" reject case) -- `ServerName` is still syntactically required to
/// construct a `ClientConnection`, even though `enable_sni = false`
/// drops the wire extension.
fn build_client_hello_no_sni(alpn: &[&[u8]]) -> Vec<u8> {
    let mut config = insecure_client_config(alpn);
    config.enable_sni = false;
    let server_name =
        ServerName::try_from("sni-suppressed.example.com".to_owned()).expect("valid DNS name");
    let mut conn =
        ClientConnection::new(Arc::new(config), server_name).expect("build ClientConnection");
    let mut buf = Vec::new();
    conn.write_tls(&mut buf)
        .expect("write_tls must drain the queued ClientHello");
    buf
}

/// Connect a raw TCP stream with short read/write timeouts, for tests
/// that drive their own hand-built bytes rather than a `rustls` stream.
fn raw_connect(addr: SocketAddr) -> TcpStream {
    let stream = TcpStream::connect(addr).expect("could not connect to sozu");
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    stream
}

/// Drain whatever Sōzu sends back until a short read / timeout. A
/// preread reject normally sends nothing at all (silent close), so this
/// is mostly used on routes that DO expect a backend-relayed response.
fn raw_read_all(stream: &mut TcpStream) -> Vec<u8> {
    let mut result = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => result.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => break,
            Err(_) => break,
        }
    }
    result
}

/// The shared shape of every reject assertion in this module: block for up to
/// `budget` waiting for Sōzu to close the connection (EOF or a hard
/// error). Returns `true` iff the close was observed within `budget` --
/// `false` covers BOTH "still open when the budget ran out" (`WouldBlock`
/// / `TimedOut`) and any other non-close outcome. A single blocking
/// `read()` with its timeout set to `budget` is the right primitive here
/// (not a sleep, not a polling loop): the kernel wakes it the instant the
/// peer's FIN/RST arrives, so a close is detected immediately, and a
/// hung connection reliably falls through to the timeout instead.
fn expect_closed_within(stream: &mut TcpStream, budget: Duration) -> bool {
    stream.set_read_timeout(Some(budget)).ok();
    let mut buf = [0u8; 4096];
    match stream.read(&mut buf) {
        Ok(0) => true,
        Err(e) if e.kind() != ErrorKind::WouldBlock && e.kind() != ErrorKind::TimedOut => true,
        _ => false,
    }
}

/// Shared byte-capture loop for [`spawn_raw_capture_backend`] and
/// [`spawn_held_multi_capture_backend`]: read from `stream` until a quiet
/// 300ms gap once at least one byte has arrived, tolerating up to 2s for
/// the FIRST byte to arrive (scheduling jitter under parallel test load).
/// Every caller either reads a response back right away (which would
/// otherwise race a fixed multi-second capture deadline) or ignores the
/// captured bytes entirely, so there is no reason to hold a full deadline
/// hostage once a burst is over.
fn capture_until_quiet(stream: &mut TcpStream) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_millis(300)))
        .ok();
    let mut received = Vec::new();
    let mut buf = [0u8; 8192];
    let first_byte_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => received.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                if !received.is_empty() || Instant::now() >= first_byte_deadline {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    received
}

/// Spawn a thread that accepts ONE raw TCP connection and captures every
/// byte it receives (via [`capture_until_quiet`]; Sōzu forwards raw bytes
/// verbatim on this path -- a "backend" here only needs to prove WHICH
/// bytes reached it, never a TLS handshake). Writes `response` once the
/// client goes quiet, then returns the accumulated bytes -- dropping both
/// the stream and the one-shot listener. This is a TRUE one-shot: it is
/// only suited to scenarios where nothing after the response matters,
/// since the moment this thread returns, the backend is both
/// disconnected AND unreachable (no listener left to accept a follow-up
/// connection). A caller that needs the connection to stay open, or that
/// needs a SECOND connection to reach a live backend, must use
/// [`spawn_held_multi_capture_backend`] instead.
fn spawn_raw_capture_backend(
    address: SocketAddr,
    response: &'static [u8],
) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let listener = bind_std_listener(address, "sni raw capture backend");
        let (mut stream, _) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(error) => {
                println!("sni raw capture backend: accept failed: {error}");
                return Vec::new();
            }
        };
        let received = capture_until_quiet(&mut stream);
        let _ = stream.write_all(response);
        received
    })
}

/// Spawn a backend that accepts up to `max_conns` raw TCP connections (in
/// acceptance order, each numbered by a 0-based `index`) and holds EACH
/// one open behind its own release gate, rather than letting it drop as
/// soon as it has responded.
///
/// Contrast [`spawn_raw_capture_backend`]: that one is a true one-shot,
/// so a caller which then attempts a SECOND connection to the same
/// backend address observes a close that proves nothing about whatever
/// admission logic it meant to exercise -- the fixed listener is simply
/// gone by then (sozu-proxy/sozu#1290).
///
/// Each accepted connection runs on its OWN worker thread: it captures
/// bytes until a quiet gap ([`capture_until_quiet`]), writes
/// `responses[index.min(responses.len() - 1)]`, reports `(index,
/// captured_bytes)` on the shared, returned `Receiver` the instant the
/// response has been flushed -- the caller's synchronization point,
/// proving the end-to-end path is live rather than guessing with a sleep
/// -- then BLOCKS holding the stream open (no FIN) until the caller sends
/// on the matching `Sender` in the returned `Vec` (indexed the same way).
/// Running each accepted connection on its own thread (rather than
/// serially in the acceptor loop) lets a later connection be accepted and
/// served concurrently with an earlier one that is still held open --
/// exactly what proves a raised/unlimited per-IP cap admits a second
/// CONCURRENT connection while the first is still live.
///
/// The acceptor thread's own `JoinHandle` is intentionally NOT returned:
/// if the very regression this backend is built to catch reoccurs (the
/// per-(cluster, IP) limiter never releases a slot), the acceptor's
/// `accept()` for a later index blocks forever. Joining that thread from
/// the test would turn a should-fail assertion into a hung test; the
/// caller's own bookkeeping (via the `Receiver` and the booleans it
/// computes) is what decides pass/fail, not the backend thread reaching
/// completion.
fn spawn_held_multi_capture_backend(
    address: SocketAddr,
    responses: &'static [&'static [u8]],
    max_conns: usize,
) -> (Receiver<(usize, Vec<u8>)>, Vec<Sender<()>>) {
    let (ready_tx, ready_rx) = mpsc::channel::<(usize, Vec<u8>)>();
    let mut release_txs = Vec::with_capacity(max_conns);
    let mut release_rxs = Vec::with_capacity(max_conns);
    for _ in 0..max_conns {
        let (tx, rx) = mpsc::channel::<()>();
        release_txs.push(tx);
        release_rxs.push(rx);
    }
    let mut release_rxs = release_rxs.into_iter();
    thread::spawn(move || {
        let listener = bind_std_listener(address, "sni held multi-capture backend");
        for index in 0..max_conns {
            let (mut stream, _) = match listener.accept() {
                Ok(accepted) => accepted,
                Err(error) => {
                    println!("sni held multi-capture backend: accept {index} failed: {error}");
                    break;
                }
            };
            let ready_tx = ready_tx.clone();
            let response = responses[index.min(responses.len() - 1)];
            let release_rx = release_rxs
                .next()
                .expect("one release channel reserved per connection slot");
            thread::spawn(move || {
                let received = capture_until_quiet(&mut stream);
                let _ = stream.write_all(response);
                let _ = ready_tx.send((index, received));
                // Hold the stream open (no FIN) until released -- proves
                // the (cluster, IP) slot backing this connection stays
                // occupied for as long as the test needs it to.
                let _ = release_rx.recv();
            });
        }
    });
    (ready_rx, release_txs)
}

/// Build a PROXY-protocol V2 PROXY header (IPv4, 28 bytes). Mirrors
/// `tcp_tests.rs`'s private `pp_v2_proxy_ipv4` helper -- duplicated
/// locally since that one is not `pub(crate)`.
fn pp_v2_header(src_port: u16, dst_port: u16) -> Vec<u8> {
    let mut header = vec![
        0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A, // magic
        0x21, // version 2, command PROXY
        0x11, // AF_INET, STREAM
        0x00, 0x0C, // address length: 12
        127, 0, 0, 1, // source IP
        127, 0, 0, 1, // dest IP
    ];
    header.extend_from_slice(&src_port.to_be_bytes());
    header.extend_from_slice(&dst_port.to_be_bytes());
    header
}

/// The 13-byte PROXY-v2 magic + version/command prefix, used to assert a
/// synthesized `SendHeader` starts a captured buffer without depending on
/// the exact addresses `SendProxyProtocol` fills in (it uses the raw TCP
/// peer/local addresses of the connection, not any parsed inbound value).
const PPV2_MAGIC_AND_VERSION: [u8; 13] = [
    0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A, 0x21,
];

/// Query one process-wide (`no_clusters: true`) gauge/counter by exact
/// name via the command channel's `QueryMetrics`, as used by
/// `metrics_lifecycle_tests.rs`'s `cluster_row_present`. Returns `None`
/// when the response is malformed or the metric is absent/not a gauge.
fn query_global_gauge(worker: &mut Worker, metric_name: &str) -> Option<u64> {
    worker.send_proxy_request_type(RequestType::QueryMetrics(QueryMetricsOptions {
        list: false,
        cluster_ids: vec![],
        backend_ids: vec![],
        metric_names: vec![metric_name.to_owned()],
        no_clusters: true,
        workers: false,
    }));
    let response = worker.read_proxy_response()?;
    if response.status != ResponseStatus::Ok as i32 {
        return None;
    }
    let content = response.content.and_then(|c| c.content_type)?;
    let ContentType::WorkerMetrics(metrics) = content else {
        return None;
    };
    metrics.proxy.get(metric_name).and_then(|filtered| {
        filtered.inner.clone().and_then(|inner| match inner {
            filtered_metrics::Inner::Gauge(value) => Some(value),
            _ => None,
        })
    })
}

/// Same as [`query_global_gauge`] but for a `Count`-shaped metric (every
/// `tcp.sni_preread.rejected.*` reason and `tcp.sni_preread.routed`, via
/// `incr!`). Used to CONFIRM (not just infer from the client-visible
/// symptom) whether a connection routed or which `RejectReason` fired --
/// e.g. in `test_tcp_sni_reject_oversized_preread` and
/// `test_tcp_sni_large_payload_coalesced_with_hello_delivered_intact`.
fn query_global_count(worker: &mut Worker, metric_name: &str) -> Option<i64> {
    worker.send_proxy_request_type(RequestType::QueryMetrics(QueryMetricsOptions {
        list: false,
        cluster_ids: vec![],
        backend_ids: vec![],
        metric_names: vec![metric_name.to_owned()],
        no_clusters: true,
        workers: false,
    }));
    let response = worker.read_proxy_response()?;
    if response.status != ResponseStatus::Ok as i32 {
        return None;
    }
    let content = response.content.and_then(|c| c.content_type)?;
    let ContentType::WorkerMetrics(metrics) = content else {
        return None;
    };
    metrics.proxy.get(metric_name).and_then(|filtered| {
        filtered.inner.clone().and_then(|inner| match inner {
            filtered_metrics::Inner::Count(value) => Some(value),
            _ => None,
        })
    })
}

// =========================================================================
// SNI passthrough + mTLS (the canonical case)
// =========================================================================

/// Two TLS-terminating backends, two SNI routes on ONE TCP listener.
/// Asserts: (1) each SNI lands on its OWN backend (proven by the raw
/// bytes the backend captured AND by its response body), and (2) the
/// certificate the CLIENT observed differs between the two connections
/// -- a terminating proxy would present ONE shared front cert regardless
/// of destination, so two DISTINCT backend certs reaching the client is
/// the direct proof Sōzu never terminated TLS on this path.
fn try_tcp_sni_routes_to_correct_backend_by_cert() -> State {
    let front_address = create_local_address();
    let addr_a = create_local_address();
    let addr_b = create_local_address();

    let mut worker = setup_sni_worker(
        "TCP-SNI-ROUTE",
        front_address,
        false,
        DEFAULT_SNI_PREREAD_TIMEOUT,
        DEFAULT_SNI_PREREAD_MAX_BYTES,
        &[
            SniRoute {
                sni: Some("a.example.com"),
                alpn: &[],
                cluster_id: "cluster_a",
                proxy_protocol: None,
                backend_address: addr_a,
                max_connections_per_ip: None,
            },
            SniRoute {
                sni: Some("b.example.com"),
                alpn: &[],
                cluster_id: "cluster_b",
                proxy_protocol: None,
                backend_address: addr_b,
                max_connections_per_ip: None,
            },
        ],
    );

    let handle_a = tls_backend::spawn_accept_one(
        "BACKEND-A",
        addr_a,
        tls_backend::server_config_single_cert(BACKEND_A_CERT, BACKEND_A_KEY),
        b"hello-from-a",
    );
    let handle_b = tls_backend::spawn_accept_one(
        "BACKEND-B",
        addr_b,
        tls_backend::server_config_single_cert(BACKEND_B_CERT, BACKEND_B_KEY),
        b"hello-from-b",
    );
    thread::sleep(Duration::from_millis(50));

    let connect_and_roundtrip = |sni: &str, request: &[u8]| -> (Vec<u8>, Option<Vec<u8>>) {
        let config = insecure_client_config(&[]);
        let server_name = ServerName::try_from(sni.to_owned()).unwrap();
        let conn = ClientConnection::new(Arc::new(config), server_name).expect("client conn");
        let tcp = TcpStream::connect_timeout(&front_address, Duration::from_secs(5))
            .expect("connect to sozu");
        tcp.set_read_timeout(Some(Duration::from_secs(3))).ok();
        tcp.set_write_timeout(Some(Duration::from_secs(3))).ok();
        let mut tls = StreamOwned::new(conn, tcp);
        tls.write_all(request).expect("write request over TLS");
        tls.flush().ok();
        let mut buf = [0u8; 256];
        let n = tls.read(&mut buf).unwrap_or(0);
        let peer_cert = tls
            .conn
            .peer_certificates()
            .and_then(|certs| certs.first())
            .map(|cert| cert.as_ref().to_vec());
        (buf[..n].to_vec(), peer_cert)
    };

    let (response_a, cert_a) = connect_and_roundtrip("a.example.com", b"ping-a");
    let (response_b, cert_b) = connect_and_roundtrip("b.example.com", b"ping-b");

    let capture_a = handle_a.join().expect("backend A thread panicked");
    let capture_b = handle_b.join().expect("backend B thread panicked");

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    let routing_ok = response_a == b"hello-from-a" && response_b == b"hello-from-b";
    let backend_saw_right_bytes = capture_a.as_ref().is_some_and(|c| c.received == b"ping-a")
        && capture_b.as_ref().is_some_and(|c| c.received == b"ping-b");
    let sni_seen_ok = capture_a.as_ref().and_then(|c| c.sni_seen.clone())
        == Some("a.example.com".to_owned())
        && capture_b.as_ref().and_then(|c| c.sni_seen.clone()) == Some("b.example.com".to_owned());
    let distinct_certs = cert_a.is_some() && cert_a != cert_b;

    println!(
        "routing_ok={routing_ok} backend_saw_right_bytes={backend_saw_right_bytes} sni_seen_ok={sni_seen_ok} distinct_certs={distinct_certs}"
    );

    if stopped && routing_ok && backend_saw_right_bytes && sni_seen_ok && distinct_certs {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_tcp_sni_routes_to_correct_backend_by_cert() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "TCP SNI: two SNI routes on one listener reach two DISTINCT backend certs",
            try_tcp_sni_routes_to_correct_backend_by_cert,
        ),
        State::Success,
    );
}

/// mTLS positive space: the backend requires a client certificate
/// (Kubernetes-API-server style); the in-test client presents one signed
/// by the backend's trusted CA. Asserts the handshake succeeds AND the
/// backend captured the client's exact certificate -- proving the
/// client's identity reached the backend THROUGH Sōzu, which a
/// terminating proxy would break (the backend would see Sōzu's identity,
/// not the original client's).
fn try_tcp_sni_mtls_client_cert_reaches_backend() -> State {
    let front_address = create_local_address();
    let addr_mtls = create_local_address();

    let mut worker = setup_sni_worker(
        "TCP-SNI-MTLS-OK",
        front_address,
        false,
        DEFAULT_SNI_PREREAD_TIMEOUT,
        DEFAULT_SNI_PREREAD_MAX_BYTES,
        &[SniRoute {
            sni: Some("mtls.example.com"),
            alpn: &[],
            cluster_id: "cluster_mtls",
            proxy_protocol: None,
            backend_address: addr_mtls,
            max_connections_per_ip: None,
        }],
    );

    let handle = tls_backend::spawn_accept_one(
        "BACKEND-MTLS",
        addr_mtls,
        tls_backend::server_config_requiring_client_cert(
            MTLS_BACKEND_CERT,
            MTLS_BACKEND_KEY,
            CA_CERT,
        ),
        b"hello-mtls",
    );
    thread::sleep(Duration::from_millis(50));

    let config = insecure_client_config_with_identity(&[], MTLS_CLIENT_CERT, MTLS_CLIENT_KEY);
    let server_name = ServerName::try_from("mtls.example.com".to_owned()).unwrap();
    let conn = ClientConnection::new(Arc::new(config), server_name).expect("client conn");
    let tcp = TcpStream::connect_timeout(&front_address, Duration::from_secs(5))
        .expect("connect to sozu");
    tcp.set_read_timeout(Some(Duration::from_secs(3))).ok();
    tcp.set_write_timeout(Some(Duration::from_secs(3))).ok();
    let mut tls = StreamOwned::new(conn, tcp);
    let write_ok = tls.write_all(b"ping-mtls").is_ok();
    tls.flush().ok();
    let mut buf = [0u8; 256];
    let response = match tls.read(&mut buf) {
        Ok(n) => buf[..n].to_vec(),
        Err(_) => Vec::new(),
    };

    let capture = handle.join().expect("backend thread panicked");

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    let expected_client_der = CertificateDer::from_pem_slice(MTLS_CLIENT_CERT)
        .expect("parse expected client cert")
        .as_ref()
        .to_vec();

    let handshake_ok = write_ok && response == b"hello-mtls";
    let client_cert_reached_backend = capture
        .as_ref()
        .and_then(|c| c.client_cert_der.clone())
        .as_ref()
        == Some(&expected_client_der);
    let request_reached_backend = capture.as_ref().is_some_and(|c| c.received == b"ping-mtls");

    println!(
        "handshake_ok={handshake_ok} client_cert_reached_backend={client_cert_reached_backend} request_reached_backend={request_reached_backend}"
    );

    if stopped && handshake_ok && client_cert_reached_backend && request_reached_backend {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_tcp_sni_mtls_client_cert_reaches_backend() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "TCP SNI mTLS: client cert presented through Sōzu reaches and is verified by the backend",
            try_tcp_sni_mtls_client_cert_reaches_backend,
        ),
        State::Success,
    );
}

/// mTLS negative space: a client with NO certificate against the SAME
/// backend, which requires one. The rejection must come from the
/// BACKEND, not Sōzu -- structurally guaranteed here since Sōzu never
/// participates in the TLS handshake on this path at all (it only reads
/// far enough to find the SNI/ALPN extensions, then forwards raw bytes);
/// the ONLY TLS participant that can reject a client certificate is the
/// backend's own `WebPkiClientVerifier`.
fn try_tcp_sni_mtls_rejects_client_without_cert() -> State {
    let front_address = create_local_address();
    let addr_mtls = create_local_address();

    let mut worker = setup_sni_worker(
        "TCP-SNI-MTLS-REJECT",
        front_address,
        false,
        DEFAULT_SNI_PREREAD_TIMEOUT,
        DEFAULT_SNI_PREREAD_MAX_BYTES,
        &[SniRoute {
            sni: Some("mtls.example.com"),
            alpn: &[],
            cluster_id: "cluster_mtls",
            proxy_protocol: None,
            backend_address: addr_mtls,
            max_connections_per_ip: None,
        }],
    );

    let handle = tls_backend::spawn_accept_one(
        "BACKEND-MTLS-REJECT",
        addr_mtls,
        tls_backend::server_config_requiring_client_cert(
            MTLS_BACKEND_CERT,
            MTLS_BACKEND_KEY,
            CA_CERT,
        ),
        b"hello-mtls",
    );
    thread::sleep(Duration::from_millis(50));

    // No client identity at all -- `with_no_client_auth()`.
    let config = insecure_client_config(&[]);
    let server_name = ServerName::try_from("mtls.example.com".to_owned()).unwrap();
    let conn = ClientConnection::new(Arc::new(config), server_name).expect("client conn");
    let tcp = TcpStream::connect_timeout(&front_address, Duration::from_secs(5))
        .expect("connect to sozu");
    tcp.set_read_timeout(Some(Duration::from_secs(3))).ok();
    tcp.set_write_timeout(Some(Duration::from_secs(3))).ok();
    let mut tls = StreamOwned::new(conn, tcp);
    // Either the write or the read may surface the handshake failure,
    // depending on exactly which flight the fatal alert arrives on.
    let write_result = tls.write_all(b"ping-no-cert");
    tls.flush().ok();
    let mut buf = [0u8; 256];
    let read_result = tls.read(&mut buf);
    let client_side_rejected = write_result.is_err() || read_result.is_err();

    let capture = handle.join().expect("backend thread panicked");

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    // The backend's own handshake never completed (its first `read`
    // returns `Err` on a client-cert-verification failure), so
    // `spawn_accept_one` yields `None` -- this function's positive
    // signal that the BACKEND rejected the client.
    let backend_rejected = capture.is_none();

    println!(
        "client_side_rejected={client_side_rejected} backend_rejected={backend_rejected} write_result={write_result:?}"
    );

    if stopped && client_side_rejected && backend_rejected {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_tcp_sni_mtls_rejects_client_without_cert() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "TCP SNI mTLS: a client without a certificate is rejected BY THE BACKEND",
            try_tcp_sni_mtls_rejects_client_without_cert,
        ),
        State::Success,
    );
}

/// Byte-for-byte replay (plain path): capture the EXACT ClientHello bytes
/// the client sends and assert the backend received an IDENTICAL leading
/// byte sequence -- the passthrough contract in its most literal form.
/// No TLS handshake needed on either side: Sōzu forwards raw bytes
/// regardless of whether anyone ever completes the handshake they
/// belong to.
fn try_tcp_sni_byte_for_byte_replay() -> State {
    let front_address = create_local_address();
    let backend_address = create_local_address();

    let mut worker = setup_sni_worker(
        "TCP-SNI-REPLAY",
        front_address,
        false,
        DEFAULT_SNI_PREREAD_TIMEOUT,
        DEFAULT_SNI_PREREAD_MAX_BYTES,
        &[SniRoute {
            sni: Some("replay.example.com"),
            alpn: &[],
            cluster_id: "cluster_replay",
            proxy_protocol: None,
            backend_address,
            max_connections_per_ip: None,
        }],
    );

    let handle = spawn_raw_capture_backend(backend_address, b"replay-ack");
    thread::sleep(Duration::from_millis(50));

    let hello = build_client_hello("replay.example.com", &[b"h2"]);
    let mut stream = raw_connect(front_address);
    stream.write_all(&hello).expect("write ClientHello");

    let received = handle.join().expect("backend thread panicked");
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    let byte_identical = received == hello;
    println!(
        "sent {} bytes, backend received {} bytes, byte_identical={byte_identical}",
        hello.len(),
        received.len()
    );

    if stopped && byte_identical {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_tcp_sni_byte_for_byte_replay() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "TCP SNI: the backend receives the client's ClientHello byte-for-byte",
            try_tcp_sni_byte_for_byte_replay,
        ),
        State::Success,
    );
}

// =========================================================================
// Preread reject matrix + positive ALPN routing
// =========================================================================

fn try_tcp_sni_reject_non_tls_bytes() -> State {
    let front_address = create_local_address();
    let mut worker = setup_single_sni_route_worker(
        "TCP-SNI-REJECT-NONTLS",
        front_address,
        DEFAULT_SNI_PREREAD_TIMEOUT,
        DEFAULT_SNI_PREREAD_MAX_BYTES,
    );

    let mut stream = raw_connect(front_address);
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: not-tls\r\n\r\n")
        .expect("write plain non-TLS bytes");
    let closed = expect_closed_within(&mut stream, Duration::from_secs(2));

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    if stopped && closed {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_tcp_sni_reject_non_tls_bytes() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "TCP SNI reject: plain non-TLS bytes close the connection",
            try_tcp_sni_reject_non_tls_bytes,
        ),
        State::Success,
    );
}

fn try_tcp_sni_reject_no_sni_extension() -> State {
    let front_address = create_local_address();
    let mut worker = setup_single_sni_route_worker(
        "TCP-SNI-REJECT-NOSNI",
        front_address,
        DEFAULT_SNI_PREREAD_TIMEOUT,
        DEFAULT_SNI_PREREAD_MAX_BYTES,
    );

    let hello = build_client_hello_no_sni(&[]);
    let mut stream = raw_connect(front_address);
    stream
        .write_all(&hello)
        .expect("write valid ClientHello with no SNI extension");
    let closed = expect_closed_within(&mut stream, Duration::from_secs(2));

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    if stopped && closed {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_tcp_sni_reject_no_sni_extension() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "TCP SNI reject: a valid ClientHello with no SNI extension closes the connection",
            try_tcp_sni_reject_no_sni_extension,
        ),
        State::Success,
    );
}

fn try_tcp_sni_reject_unmatched_sni() -> State {
    let front_address = create_local_address();
    let mut worker = setup_single_sni_route_worker(
        "TCP-SNI-REJECT-UNMATCHED-SNI",
        front_address,
        DEFAULT_SNI_PREREAD_TIMEOUT,
        DEFAULT_SNI_PREREAD_MAX_BYTES,
    );

    // The only configured route is "known.example.com".
    let hello = build_client_hello("unknown.example.net", &[]);
    let mut stream = raw_connect(front_address);
    stream
        .write_all(&hello)
        .expect("write ClientHello with unmatched SNI");
    let closed = expect_closed_within(&mut stream, Duration::from_secs(2));

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    if stopped && closed {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_tcp_sni_reject_unmatched_sni() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "TCP SNI reject: an SNI absent from the route table closes the connection",
            try_tcp_sni_reject_unmatched_sni,
        ),
        State::Success,
    );
}

fn try_tcp_sni_reject_unmatched_alpn() -> State {
    let front_address = create_local_address();
    let backend_address = create_local_address();
    // A OneOf-only route (no catch-all): the SNI matches but the client's
    // ALPN offer does not.
    let mut worker = setup_sni_worker(
        "TCP-SNI-REJECT-UNMATCHED-ALPN",
        front_address,
        false,
        DEFAULT_SNI_PREREAD_TIMEOUT,
        DEFAULT_SNI_PREREAD_MAX_BYTES,
        &[SniRoute {
            sni: Some("h2-only.example.com"),
            alpn: &["h2"],
            cluster_id: "cluster_h2_only",
            proxy_protocol: None,
            backend_address,
            max_connections_per_ip: None,
        }],
    );

    let hello = build_client_hello("h2-only.example.com", &[b"http/1.1"]);
    let mut stream = raw_connect(front_address);
    stream
        .write_all(&hello)
        .expect("write ClientHello with unmatched ALPN");
    let closed = expect_closed_within(&mut stream, Duration::from_secs(2));

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    if stopped && closed {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_tcp_sni_reject_unmatched_alpn() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "TCP SNI reject: SNI matches but ALPN offer matches no route entry",
            try_tcp_sni_reject_unmatched_alpn,
        ),
        State::Success,
    );
}

/// Partial ClientHello, then silence: the preread deadline must fire and
/// close the connection within its configured budget (1s here) plus a
/// margin -- asserted with a bounded deadline read, never a sleep. The
/// elapsed time is also checked against a lower bound so this test can't
/// pass via some OTHER, unrelated fast-reject path.
fn try_tcp_sni_reject_preread_timeout() -> State {
    let front_address = create_local_address();
    let preread_timeout_secs = 1;
    let mut worker = setup_single_sni_route_worker(
        "TCP-SNI-REJECT-TIMEOUT",
        front_address,
        preread_timeout_secs,
        DEFAULT_SNI_PREREAD_MAX_BYTES,
    );

    let hello = build_client_hello("known.example.com", &[]);
    let partial_len = hello.len().min(10);
    let mut stream = raw_connect(front_address);
    stream
        .write_all(&hello[..partial_len])
        .expect("write partial ClientHello");

    let start = Instant::now();
    let closed = expect_closed_within(&mut stream, Duration::from_secs(4));
    let elapsed = start.elapsed();

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    let timely = elapsed >= Duration::from_millis(800) && elapsed <= Duration::from_secs(4);
    println!("preread timeout: closed={closed} elapsed={elapsed:?} timely={timely}");

    if stopped && closed && timely {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_tcp_sni_reject_preread_timeout() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "TCP SNI reject: a stalled partial ClientHello closes within timeout + margin",
            try_tcp_sni_reject_preread_timeout,
        ),
        State::Success,
    );
}

/// A normal ClientHello sent against a listener configured with a tiny
/// `sni_preread_max_bytes` (16) must be rejected as oversized -- any real
/// ClientHello vastly exceeds 16 bytes, so this deterministically
/// exercises `RejectReason::TooLarge` regardless of exact extension
/// sizes. Because the preread `socket_read` is
/// hard-capped at `effective_max_bytes`
/// (`lib/src/protocol/tcp_preread/shell.rs`), the hello reaches the core
/// INCOMPLETE with `buf.len() == max_bytes` -- the exact `TooLarge`
/// condition (`need_more_or_too_large`) -- and the reject fires FAST (no
/// dependence on the preread timeout). Asserted specifically via the
/// `tcp.sni_preread.rejected.too_large` counter, not just the client-side
/// close, so a future regression that closed the connection for some
/// OTHER reason would not silently pass this test.
fn try_tcp_sni_reject_oversized_preread() -> State {
    let front_address = create_local_address();
    let tiny_max_bytes = 16;
    let mut worker = setup_single_sni_route_worker(
        "TCP-SNI-REJECT-OVERSIZED",
        front_address,
        DEFAULT_SNI_PREREAD_TIMEOUT,
        tiny_max_bytes,
    );

    let hello = build_client_hello("known.example.com", &[]);
    assert!(
        hello.len() > tiny_max_bytes as usize,
        "sanity: a real ClientHello ({} bytes) must exceed the tiny test cap ({tiny_max_bytes})",
        hello.len()
    );
    let mut stream = raw_connect(front_address);
    stream
        .write_all(&hello)
        .expect("write oversized ClientHello");
    let closed = expect_closed_within(&mut stream, Duration::from_secs(2));

    let too_large_count = query_global_count(&mut worker, "tcp.sni_preread.rejected.too_large");
    let routed_count = query_global_count(&mut worker, "tcp.sni_preread.routed");
    let rejected_too_large = too_large_count == Some(1) && routed_count.unwrap_or(0) == 0;
    println!(
        "oversized preread: closed={closed} tcp.sni_preread.rejected.too_large={too_large_count:?}, tcp.sni_preread.routed={routed_count:?}"
    );

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    if stopped && closed && rejected_too_large {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_tcp_sni_reject_oversized_preread() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "TCP SNI reject: a ClientHello exceeding sni_preread_max_bytes closes the connection",
            try_tcp_sni_reject_oversized_preread,
        ),
        State::Success,
    );
}

/// Positive ALPN routing: same SNI, two clusters split by ALPN (`h2` vs a
/// catch-all). A client offering `h2` (even second in its list) lands on
/// the `h2` cluster; a client offering only `http/1.1` falls through to
/// the catch-all -- proven by WHICH physical backend socket received the
/// bytes, the strongest available signal (not just a response tag).
fn try_tcp_sni_alpn_routes_by_client_preference() -> State {
    let front_address = create_local_address();
    let addr_h2 = create_local_address();
    let addr_catchall = create_local_address();

    let mut worker = setup_sni_worker(
        "TCP-SNI-ALPN-ROUTE",
        front_address,
        false,
        DEFAULT_SNI_PREREAD_TIMEOUT,
        DEFAULT_SNI_PREREAD_MAX_BYTES,
        &[
            SniRoute {
                sni: Some("multi.example.com"),
                alpn: &["h2"],
                cluster_id: "cluster_h2",
                proxy_protocol: None,
                backend_address: addr_h2,
                max_connections_per_ip: None,
            },
            SniRoute {
                sni: Some("multi.example.com"),
                alpn: &[],
                cluster_id: "cluster_catchall",
                proxy_protocol: None,
                backend_address: addr_catchall,
                max_connections_per_ip: None,
            },
        ],
    );

    let handle_h2 = spawn_raw_capture_backend(addr_h2, b"h2-ack");
    let handle_catchall = spawn_raw_capture_backend(addr_catchall, b"catchall-ack");
    thread::sleep(Duration::from_millis(50));

    // Client prefers h2 (offered first, even though a plain-http/1.1
    // client would otherwise be common) -> must land on cluster_h2.
    let hello_h2_pref = build_client_hello("multi.example.com", &[b"h2", b"http/1.1"]);
    let mut stream_h2 = raw_connect(front_address);
    stream_h2
        .write_all(&hello_h2_pref)
        .expect("write h2-preferring ClientHello");
    let response_h2 = raw_read_all(&mut stream_h2);

    // Client offers ONLY http/1.1 -> no OneOf entry matches -> Any
    // catch-all.
    let hello_h1_only = build_client_hello("multi.example.com", &[b"http/1.1"]);
    let mut stream_catchall = raw_connect(front_address);
    stream_catchall
        .write_all(&hello_h1_only)
        .expect("write http/1.1-only ClientHello");
    let response_catchall = raw_read_all(&mut stream_catchall);

    let capture_h2 = handle_h2.join().expect("h2 backend thread panicked");
    let capture_catchall = handle_catchall
        .join()
        .expect("catchall backend thread panicked");

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    let h2_ok = capture_h2 == hello_h2_pref && response_h2 == b"h2-ack";
    let catchall_ok = capture_catchall == hello_h1_only && response_catchall == b"catchall-ack";
    println!("h2_ok={h2_ok} catchall_ok={catchall_ok}");

    if stopped && h2_ok && catchall_ok {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_tcp_sni_alpn_routes_by_client_preference() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "TCP SNI: ALPN routing follows client preference order, falling back to the Any catch-all",
            try_tcp_sni_alpn_routes_by_client_preference,
        ),
        State::Success,
    );
}

// =========================================================================
// PROXY-protocol composition with the SNI preread
// =========================================================================

/// `expect_proxy` listener + `ExpectHeader` cluster: the client sends a
/// PPv2 header THEN the ClientHello; the backend must receive ONLY the
/// ClientHello (PPv2 header dropped -- Sōzu terminates it locally,
/// "Expect" semantics).
fn try_tcp_sni_expect_proxy_drops_header_before_backend() -> State {
    let front_address = create_local_address();
    let backend_address = create_local_address();
    let mut worker = setup_sni_worker(
        "TCP-SNI-PP-EXPECT",
        front_address,
        true,
        DEFAULT_SNI_PREREAD_TIMEOUT,
        DEFAULT_SNI_PREREAD_MAX_BYTES,
        &[SniRoute {
            sni: Some("expect.example.com"),
            alpn: &[],
            cluster_id: "cluster_expect",
            proxy_protocol: Some(ProxyProtocolConfig::ExpectHeader),
            backend_address,
            max_connections_per_ip: None,
        }],
    );

    let handle = spawn_raw_capture_backend(backend_address, b"expect-ack");
    thread::sleep(Duration::from_millis(50));

    let hello = build_client_hello("expect.example.com", &[]);
    let pp_header = pp_v2_header(54321, front_address.port());
    let mut stream = raw_connect(front_address);
    stream.write_all(&pp_header).expect("write PPv2 header");
    stream.write_all(&hello).expect("write ClientHello");

    let received = handle.join().expect("backend thread panicked");
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    let clean_stream = received == hello;
    println!(
        "expect: received {} bytes, expected clean ClientHello {} bytes, clean_stream={clean_stream}",
        received.len(),
        hello.len()
    );

    if stopped && clean_stream {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_tcp_sni_expect_proxy_drops_header_before_backend() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "TCP SNI + PROXY expect: inbound PPv2 header is terminated locally, backend sees a clean ClientHello",
            try_tcp_sni_expect_proxy_drops_header_before_backend,
        ),
        State::Success,
    );
}

/// A plain client (no inbound PPv2) routed by SNI to a `SendHeader`
/// cluster: the backend must receive a SYNTHESIZED PPv2 header THEN the
/// untouched ClientHello, in that wire order.
fn try_tcp_sni_send_proxy_synthesizes_header_after_route() -> State {
    let front_address = create_local_address();
    let backend_address = create_local_address();
    let mut worker = setup_sni_worker(
        "TCP-SNI-PP-SEND",
        front_address,
        false,
        DEFAULT_SNI_PREREAD_TIMEOUT,
        DEFAULT_SNI_PREREAD_MAX_BYTES,
        &[SniRoute {
            sni: Some("send.example.com"),
            alpn: &[],
            cluster_id: "cluster_send",
            proxy_protocol: Some(ProxyProtocolConfig::SendHeader),
            backend_address,
            max_connections_per_ip: None,
        }],
    );

    let handle = spawn_raw_capture_backend(backend_address, b"send-ack");
    thread::sleep(Duration::from_millis(50));

    let hello = build_client_hello("send.example.com", &[]);
    let mut stream = raw_connect(front_address);
    stream
        .write_all(&hello)
        .expect("write ClientHello (no inbound PPv2)");

    let received = handle.join().expect("backend thread panicked");
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    let starts_with_synthesized_ppv2 = received.starts_with(&PPV2_MAGIC_AND_VERSION);
    let ends_with_untouched_hello = received.ends_with(&hello[..]);
    let strictly_longer = received.len() > hello.len();
    println!(
        "send: starts_with_ppv2={starts_with_synthesized_ppv2} ends_with_hello={ends_with_untouched_hello} strictly_longer={strictly_longer}"
    );

    if stopped && starts_with_synthesized_ppv2 && ends_with_untouched_hello && strictly_longer {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_tcp_sni_send_proxy_synthesizes_header_after_route() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "TCP SNI + PROXY send: backend receives a synthesized PPv2 header THEN the untouched ClientHello",
            try_tcp_sni_send_proxy_synthesizes_header_after_route,
        ),
        State::Success,
    );
}

/// `expect_proxy` listener + `RelayHeader` cluster: the backend must
/// receive the client's VERBATIM PPv2 header THEN the ClientHello,
/// byte-for-byte in wire order (no re-synthesis, unlike `SendHeader`).
fn try_tcp_sni_relay_proxy_forwards_header_verbatim() -> State {
    let front_address = create_local_address();
    let backend_address = create_local_address();
    let mut worker = setup_sni_worker(
        "TCP-SNI-PP-RELAY",
        front_address,
        true,
        DEFAULT_SNI_PREREAD_TIMEOUT,
        DEFAULT_SNI_PREREAD_MAX_BYTES,
        &[SniRoute {
            sni: Some("relay.example.com"),
            alpn: &[],
            cluster_id: "cluster_relay",
            proxy_protocol: Some(ProxyProtocolConfig::RelayHeader),
            backend_address,
            max_connections_per_ip: None,
        }],
    );

    let handle = spawn_raw_capture_backend(backend_address, b"relay-ack");
    thread::sleep(Duration::from_millis(50));

    let hello = build_client_hello("relay.example.com", &[]);
    let pp_header = pp_v2_header(11111, front_address.port());
    let mut expected = pp_header.clone();
    expected.extend_from_slice(&hello);

    let mut stream = raw_connect(front_address);
    stream.write_all(&pp_header).expect("write PPv2 header");
    stream.write_all(&hello).expect("write ClientHello");

    let received = handle.join().expect("backend thread panicked");
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    let verbatim = received == expected;
    println!(
        "relay: received {} bytes, expected verbatim {} bytes, verbatim={verbatim}",
        received.len(),
        expected.len()
    );

    if stopped && verbatim {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_tcp_sni_relay_proxy_forwards_header_verbatim() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "TCP SNI + PROXY relay: backend receives the client's PPv2 header verbatim, then the ClientHello",
            try_tcp_sni_relay_proxy_forwards_header_verbatim,
        ),
        State::Success,
    );
}

/// Intended behavior, documented at `lib/src/tcp.rs`'s
/// `upgrade_sni_preread`: a `None`-proxy_protocol
/// cluster behind an `expect_proxy = true` SNI listener must ALSO deliver
/// a clean stream to the backend -- the inbound PPv2 header is absorbed,
/// not leaked, even though the routed cluster itself asked for no PROXY
/// protocol at all (a backend that expects no PROXY header must never
/// receive a stray one).
fn try_tcp_sni_none_proxy_protocol_absorbs_inbound_header() -> State {
    let front_address = create_local_address();
    let backend_address = create_local_address();
    let mut worker = setup_sni_worker(
        "TCP-SNI-PP-NONE",
        front_address,
        true,
        DEFAULT_SNI_PREREAD_TIMEOUT,
        DEFAULT_SNI_PREREAD_MAX_BYTES,
        &[SniRoute {
            sni: Some("none.example.com"),
            alpn: &[],
            cluster_id: "cluster_none",
            proxy_protocol: None,
            backend_address,
            max_connections_per_ip: None,
        }],
    );

    let handle = spawn_raw_capture_backend(backend_address, b"none-ack");
    thread::sleep(Duration::from_millis(50));

    let hello = build_client_hello("none.example.com", &[]);
    let pp_header = pp_v2_header(22222, front_address.port());
    let mut stream = raw_connect(front_address);
    stream.write_all(&pp_header).expect("write PPv2 header");
    stream.write_all(&hello).expect("write ClientHello");

    let received = handle.join().expect("backend thread panicked");
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    let no_ppv2_leaked = received == hello;
    println!(
        "none: received {} bytes, expected clean ClientHello {} bytes, no_ppv2_leaked={no_ppv2_leaked}",
        received.len(),
        hello.len()
    );

    if stopped && no_ppv2_leaked {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_tcp_sni_none_proxy_protocol_absorbs_inbound_header() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "TCP SNI + PROXY none: a None-proxy_protocol cluster behind expect_proxy still absorbs the inbound PPv2 header",
            try_tcp_sni_none_proxy_protocol_absorbs_inbound_header,
        ),
        State::Success,
    );
}

// =========================================================================
// Large payload through an SNI-routed session (splice-agnostic)
// =========================================================================

/// A 1 MiB payload sent AFTER the routing decision has demonstrably been
/// made (synchronized on the backend actually accepting the TCP
/// connection -- a real milestone, not a blind sleep) must reach the
/// backend byte-identical. Splice-agnostic by construction (no
/// `#[cfg]`): the assertion is on the final byte content, not on which
/// code path moved it, so this test passes whether or not the build
/// enables `sozu-lib/splice` (CI's full-feature cells run it with
/// `--features opentelemetry,splice,simd`). The non-SNI splice path
/// itself is already covered by the pre-existing `test_tcp_proxy_large_payload`
/// in `tcp_tests.rs` -- not duplicated here.
///
/// NOTE: this test deliberately gives the ClientHello a clear run at the
/// preread core before any payload bytes hit the wire. The harder
/// variant that sends both back-to-back with NO gap --
/// `try_tcp_sni_large_payload_coalesced_with_hello_delivered_intact`
/// below -- exercises the coalesced-read path (the reproducer for the
/// sozu#1279 close-before-flush truncation) and is
/// kept as a separate test so the plain "after a clear route" contract
/// and the coalesced-delivery guard each fail individually and legibly.
fn try_tcp_sni_large_payload_after_route() -> State {
    let front_address = create_local_address();
    let backend_address = create_local_address();
    let mut worker = setup_sni_worker(
        "TCP-SNI-LARGE-PAYLOAD",
        front_address,
        false,
        DEFAULT_SNI_PREREAD_TIMEOUT,
        DEFAULT_SNI_PREREAD_MAX_BYTES,
        &[SniRoute {
            sni: Some("large.example.com"),
            alpn: &[],
            cluster_id: "cluster_large",
            proxy_protocol: None,
            backend_address,
            max_connections_per_ip: None,
        }],
    );

    let hello = build_client_hello("large.example.com", &[]);
    let payload_size: usize = 1024 * 1024;
    let payload: Vec<u8> = (0..payload_size).map(|i| (i % 256) as u8).collect();
    let mut expected = hello.clone();
    expected.extend_from_slice(&payload);
    let expected_total = expected.len();

    let (accepted_tx, accepted_rx) = std::sync::mpsc::channel::<()>();
    let backend_handle = thread::spawn(move || {
        let listener = bind_std_listener(backend_address, "sni large payload backend");
        let (mut stream, _) = listener.accept().expect("backend accept failed");
        let _ = accepted_tx.send(());
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");
        let mut received = Vec::with_capacity(expected_total);
        let mut buf = [0u8; 8192];
        while received.len() < expected_total {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => received.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => continue,
                Err(e) if e.kind() == ErrorKind::TimedOut => break,
                Err(_) => break,
            }
        }
        received
    });

    let mut stream = raw_connect(front_address);
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("set write timeout");
    stream.write_all(&hello).expect("write ClientHello");
    // Real synchronization point, not a sleep: the backend only accepts
    // once Sōzu has read+parsed the ClientHello, decided the route, and
    // successfully connected out -- proving the routing decision is fully
    // resolved before a single payload byte is sent.
    let backend_accepted = accepted_rx.recv_timeout(Duration::from_secs(5)).is_ok();
    stream.write_all(&payload).expect("write large payload");

    let received = backend_handle.join().expect("backend thread panicked");
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    let byte_identical = received == expected;
    println!(
        "large payload: backend_accepted={backend_accepted} sent {} bytes, backend received {} bytes, byte_identical={byte_identical}",
        expected_total,
        received.len()
    );

    if stopped && backend_accepted && byte_identical {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_tcp_sni_large_payload_after_route() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "TCP SNI: a 1 MiB payload after the ClientHello reaches the backend byte-identical",
            try_tcp_sni_large_payload_after_route,
        ),
        State::Success,
    );
}

/// Permanent regression guard for the sozu-proxy/sozu#1279
/// close-before-flush truncation, on the coalesced-payload path. Sends
/// the SAME ClientHello immediately
/// followed by the SAME 1 MiB payload as
/// `try_tcp_sni_large_payload_after_route` above, but with NO gap and NO
/// synchronization between the two writes -- exactly what any real fast
/// TLS client does (a ClientHello immediately followed by more records /
/// early application data in the same TCP send, delivered to Sōzu inside
/// ONE read).
///
/// When first written, this test FAILED: it surfaced two `lib` defects on
/// the coalesced-read path, both since fixed on this branch (PR #1290):
///
///  - A spurious `RejectReason::TooLarge`: the preread `socket_read` was
///    not hard-capped at `effective_max_bytes`, so a small valid
///    ClientHello immediately followed by payload over-filled the
///    accumulator and the WHOLE combined length tripped the cap, rejecting
///    the connection before the backend was ever dialed. (The default
///    9-byte gap between `sni_preread_max_bytes` = 16384 and `buffer_size`
///    = 16393 made this trivial to hit -- not a contrived edge case.)
///  - Silent tail truncation (the close-before-flush drop): bytes already
///    read from the frontend and queued for the backend were dropped when
///    the frontend closed before they flushed.
///
/// Both are FIXED in `lib`: the preread read is now hard-capped at
/// `effective_max_bytes` (`lib/src/protocol/tcp_preread/shell.rs`, so an
/// over-cap hello reaches the core INCOMPLETE and only a genuinely
/// oversized hello -- not one padded by trailing payload -- rejects), and
/// `Pipe` now drains queued front->back bytes before teardown on every
/// frontend-close signal: the EOF (`SocketResult::Closed`) arm of
/// `readable`, the `frontend_hup` (EPOLLRDHUP) in-flight branch, and
/// `splice_readable`'s EOF arm for bytes already spliced into the kernel
/// pipe (`lib/src/protocol/pipe.rs`).
///
/// The assertion is now the permanent guard: the connection must ROUTE
/// (`tcp.sni_preread.routed` incremented, `tcp.sni_preread.rejected.too_large`
/// untouched) AND the full ClientHello + 1 MiB payload must reach the
/// backend BYTE-IDENTICAL. A re-regression of either defect fails it -- a
/// truncation as a short/mismatched backend read, a spurious reject as the
/// backend never being contacted plus the too_large counter firing. The
/// backend read and the client write are both deadline-bounded so a
/// re-regressed STALL fails fast rather than hanging the suite.
fn try_tcp_sni_large_payload_coalesced_with_hello_delivered_intact() -> State {
    let front_address = create_local_address();
    let backend_address = create_local_address();
    let mut worker = setup_sni_worker(
        "TCP-SNI-LARGE-COALESCED",
        front_address,
        false,
        DEFAULT_SNI_PREREAD_TIMEOUT,
        DEFAULT_SNI_PREREAD_MAX_BYTES,
        &[SniRoute {
            sni: Some("large-coalesced.example.com"),
            alpn: &[],
            cluster_id: "cluster_large_coalesced",
            proxy_protocol: None,
            backend_address,
            max_connections_per_ip: None,
        }],
    );

    let hello = build_client_hello("large-coalesced.example.com", &[]);
    let payload_size: usize = 1024 * 1024;
    let payload: Vec<u8> = (0..payload_size).map(|i| (i % 256) as u8).collect();
    let mut expected = hello.clone();
    expected.extend_from_slice(&payload);
    let expected_total = expected.len();

    let backend_handle = thread::spawn(move || {
        let listener = bind_std_listener(backend_address, "sni coalesced payload backend");
        // Non-blocking + polled accept with its OWN deadline: if a
        // re-regressed reject ever kept Sōzu from dialing the backend, a
        // plain blocking `accept()` would hang this thread (and the test)
        // forever. Bounding accept itself keeps the guard fail-fast.
        listener
            .set_nonblocking(true)
            .expect("set backend listener nonblocking");
        let accept_deadline = Instant::now() + Duration::from_secs(5);
        let mut accepted = None;
        while Instant::now() < accept_deadline {
            match listener.accept() {
                Ok(pair) => {
                    accepted = Some(pair);
                    break;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(_) => break,
            }
        }
        let Some((mut stream, _)) = accepted else {
            println!("sni coalesced payload backend: never accepted a connection within 5s");
            return Vec::new();
        };
        stream.set_nonblocking(false).ok();
        stream.set_read_timeout(Some(Duration::from_secs(3))).ok();
        let mut received = Vec::with_capacity(expected_total);
        let mut buf = [0u8; 8192];
        let read_deadline = Instant::now() + Duration::from_secs(8);
        while received.len() < expected_total && Instant::now() < read_deadline {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => received.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                    continue;
                }
                Err(_) => break,
            }
        }
        received
    });

    // The client write runs on a background thread reporting through a
    // channel: a re-regressed stall would block `write_all` past its own
    // socket timeout, so the MAIN thread must not block on it directly --
    // `recv_timeout` keeps the guard bounded rather than hanging the suite.
    let (write_tx, write_rx) = std::sync::mpsc::channel::<bool>();
    thread::spawn(move || {
        let outcome = (|| -> std::io::Result<()> {
            let stream = TcpStream::connect(front_address)?;
            stream.set_write_timeout(Some(Duration::from_secs(3)))?;
            let mut stream = stream;
            stream.write_all(&hello)?;
            stream.write_all(&payload)?;
            Ok(())
        })();
        let _ = write_tx.send(outcome.is_ok());
    });
    let write_completed = write_rx
        .recv_timeout(Duration::from_secs(8))
        .unwrap_or(false);

    let received = backend_handle.join().unwrap_or_else(|_| Vec::new());

    let byte_identical = received == expected;
    let backend_contacted = !received.is_empty();

    // Confirm the connection ROUTED rather than being rejected -- the
    // command channel is independent of the data-path session, so these
    // queries are unaffected by whatever the session is doing.
    let too_large_count = query_global_count(&mut worker, "tcp.sni_preread.rejected.too_large");
    let routed_count = query_global_count(&mut worker, "tcp.sni_preread.routed");
    let routed_ok = routed_count == Some(1) && too_large_count.unwrap_or(0) == 0;

    println!(
        "coalesced large payload delivered intact: write_completed={write_completed} backend_contacted={backend_contacted} backend received {} of {} expected bytes, byte_identical={byte_identical}, tcp.sni_preread.rejected.too_large={too_large_count:?}, tcp.sni_preread.routed={routed_count:?}, routed_ok={routed_ok}",
        received.len(),
        expected_total
    );

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    if stopped && write_completed && byte_identical && routed_ok {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_tcp_sni_large_payload_coalesced_with_hello_delivered_intact() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "TCP SNI regression guard (sozu#1279 close-before-flush truncation): a payload coalesced with the ClientHello (no gap) routes and reaches the backend byte-identical",
            try_tcp_sni_large_payload_coalesced_with_hello_delivered_intact,
        ),
        State::Success,
    );
}

// =========================================================================
// Per-(cluster, source-IP) limiter: rejection, decrement, and the
// differential that proves both are attributable to the limiter itself
// =========================================================================

/// Distinct ack payloads for the two backend connection slots the
/// per-IP limiter differential below ever lets reach the backend (see
/// [`spawn_held_multi_capture_backend`]): slot 0 is always the FIRST
/// client connection; slot 1 is whichever client connection is the
/// SECOND one that actually reaches the backend -- the raised-limit
/// cell's second connection, or the limit=1 cell's post-release third
/// connection.
const PER_IP_LIMITER_RESPONSES: [&[u8]; 2] = [b"backend-slot-a-ack", b"backend-slot-b-ack"];

/// Shared harness for the per-(cluster, source-IP) limiter differential
/// (see the two `#[test]`s below): identical worker/listener/backend setup
/// and connection choreography, varying only `max_connections_per_ip`.
/// Both cells route the first connection through
/// [`spawn_held_multi_capture_backend`], which holds the accepted backend
/// stream open behind an explicit release gate instead of dropping it as
/// soon as it has responded -- so "the first connection is still open"
/// when the second connection is attempted is an enforced invariant, not
/// an assumption. (sozu-proxy/sozu#1290: an earlier version of this test
/// used a true one-shot capture backend that had already accepted,
/// responded, and returned -- dropping both the stream and its listener
/// -- by the time the second connection was attempted, so its observed
/// close proved nothing about the limiter: a completely no-op limiter
/// would have produced the identical observation, because the second
/// connection's own backend dial would fail against the now-unreachable
/// address.)
///
/// - `limit == 1`: drives N preread rejects from the source IP first
///   (must never leak into the admission count), then the first
///   legitimate connection (must succeed), then a SECOND concurrent
///   connection from the SAME IP (must be limit-rejected BEFORE ever
///   reaching the backend -- proven by the backend's ready-channel never
///   reporting a second accepted connection while the first is held).
///   Releasing the first connection and retrying then MUST admit a THIRD
///   connection from the same IP, proving the limiter decremented its
///   count on close rather than only ever refusing. The real
///   `tcp.sni_preread.active` gauge is also asserted: it increments while
///   a connection is parked mid-preread, and returns to its pre-test
///   baseline only once every session opened above (the mid-preread
///   probe, the reject storm, the held-then-released first connection,
///   the rejected second connection, and the released third connection)
///   has actually closed.
/// - any other `limit` (the differential control; the paired test below
///   passes `2`): the SAME second concurrent connection that the
///   `limit == 1` cell rejects, sent while the first is genuinely still
///   held open, MUST instead reach the backend and complete end-to-end.
///   Without this cell, a broken (always-reject, or permanently-stuck)
///   limiter could make the `limit == 1` cell's rejection assertion pass
///   for the wrong reason; this cell proves the identical harness, with
///   only the cap raised, lets the second connection through -- so the
///   rejection above is attributable to the limiter's cap, not to some
///   artifact of holding a backend connection open. The reject-storm and
///   gauge-conservation checks are specific to proving the limited
///   cluster doesn't leak/underflow and are skipped here; they do not
///   depend on which cap value is configured.
fn try_tcp_sni_per_ip_limiter(limit: u64) -> State {
    let front_address = create_local_address();
    let backend_address = create_local_address();
    let mut worker = setup_sni_worker(
        "TCP-SNI-LIMIT-DIFF",
        front_address,
        false,
        DEFAULT_SNI_PREREAD_TIMEOUT,
        DEFAULT_SNI_PREREAD_MAX_BYTES,
        &[SniRoute {
            sni: Some("limited.example.com"),
            alpn: &[],
            cluster_id: "cluster_limited",
            proxy_protocol: None,
            backend_address,
            max_connections_per_ip: Some(limit),
        }],
    );

    let active = sozu_lib::metrics::names::tcp::sni_preread::ACTIVE;

    // Gauge/reject-storm setup is only meaningful against the limited
    // (=1) cluster -- see the doc comment above.
    let mut baseline_active = 0u64;
    let mut observed_active = 0u64;
    if limit == 1 {
        // Baseline: no preread session has run yet, so the gauge is absent
        // from the `QueryMetrics` response (empty `proxy` map) -- a
        // legitimate "0" state (mirrors `metrics_lifecycle_tests.rs`'s
        // `cluster_row_present` treating absence as a signal), normalized
        // through `unwrap_or(0)`.
        baseline_active = query_global_gauge(&mut worker, active).unwrap_or(0);

        // (gauge-increment live-check) Mid-preread observation: an accepted
        // SNI-listener connection increments `tcp.sni_preread.active` on
        // entry (`TcpSession::new_sni_preread`). Park one mid-preread with a
        // PARTIAL ClientHello (deterministically `NeedMore`, so it never
        // routes and never touches the per-IP admission count) and poll
        // until the gauge reflects it -- proving the increment fires and is
        // observable, which the pre-fix underflow-clamped gauge never
        // could.
        let full_hello = build_client_hello("limited.example.com", &[]);
        let partial_len = full_hello.len().min(8);
        let mut probe = raw_connect(front_address);
        probe
            .write_all(&full_hello[..partial_len])
            .expect("write partial ClientHello probe");
        let probe_deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < probe_deadline {
            if let Some(value) = query_global_gauge(&mut worker, active)
                && value >= 1
            {
                observed_active = value;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        drop(probe);

        const REJECT_ATTEMPTS: usize = 5;
        for _ in 0..REJECT_ATTEMPTS {
            let mut stream = raw_connect(front_address);
            let _ = stream.write_all(b"not a tls client hello, just junk bytes");
            let _ = expect_closed_within(&mut stream, Duration::from_secs(2));
        }
    }

    let (ready_rx, release_txs) =
        spawn_held_multi_capture_backend(backend_address, &PER_IP_LIMITER_RESPONSES, 2);
    thread::sleep(Duration::from_millis(50));

    // First connection: must succeed and reach the backend -- the
    // synchronization point for everything that follows. Waiting on
    // `ready_rx` (rather than a sleep) proves the response was actually
    // flushed end-to-end before the test moves on, and the backend holds
    // this stream open (no FIN) until `release_txs[0]` fires below.
    let hello_first = build_client_hello("limited.example.com", &[]);
    let mut first = raw_connect(front_address);
    first
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    first
        .write_all(&hello_first)
        .expect("write first ClientHello");
    let first_ready = ready_rx.recv_timeout(Duration::from_secs(3));
    let first_response = raw_read_all(&mut first);
    let first_ok = matches!(
        &first_ready,
        Ok((index, captured)) if *index == 0
            && captured.as_slice() == hello_first.as_slice()
            && first_response.as_slice() == PER_IP_LIMITER_RESPONSES[0]
    );

    // Second CONCURRENT connection from the SAME source IP, sent while the
    // first is still held open at a LIVE backend connection -- not
    // dropped, not closed: `spawn_held_multi_capture_backend` blocks it on
    // `release_txs[0]`, which nothing has signaled yet.
    let hello_second = build_client_hello("limited.example.com", &[]);
    let mut second = raw_connect(front_address);
    second
        .write_all(&hello_second)
        .expect("write second ClientHello");

    let differential_ok = if limit == 1 {
        // Rejection cell: the second connection must be limit-closed
        // BEFORE ever reaching the backend -- `cluster_ip_at_limit`
        // returns `Err(TooManyConnectionsPerIp)` ahead of any backend dial
        // (`lib/src/tcp.rs`'s `connect()`), so the backend's `ready_rx`
        // must stay silent for it.
        let second_limited = expect_closed_within(&mut second, Duration::from_secs(2));
        let second_never_reached_backend = ready_rx.try_recv().is_err();
        drop(second);

        // Release the first connection and retry a THIRD connection (same
        // source IP) until it succeeds -- proving the limiter DECREMENTED
        // on close rather than only ever rejecting. The close is
        // asynchronous (backend EOF -> `Pipe::backend_hup` ->
        // `SessionResult::Close` -> `TcpSession::close`'s
        // `untrack_all_cluster_ip`, all on the worker's own event loop), so
        // retry on a deadline instead of guessing a fixed sleep.
        release_txs[0]
            .send(())
            .expect("release the first backend connection");
        drop(first);

        let third_deadline = Instant::now() + Duration::from_secs(3);
        let mut third_ok = false;
        let mut third_response = Vec::new();
        while Instant::now() < third_deadline && !third_ok {
            let hello_third = build_client_hello("limited.example.com", &[]);
            let mut third = raw_connect(front_address);
            third
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let _ = third.write_all(&hello_third);
            if let Ok((index, captured)) = ready_rx.recv_timeout(Duration::from_millis(700))
                && index == 1
                && captured == hello_third
            {
                third_response = raw_read_all(&mut third);
                third_ok = true;
            } else {
                thread::sleep(Duration::from_millis(50));
            }
        }
        let third_ok = third_ok && third_response.as_slice() == PER_IP_LIMITER_RESPONSES[1];
        let _ = release_txs[1].send(());

        // Conservation: once every preread session above has reached
        // terminal close (mid-preread probe, reject storm, first, second,
        // third), the gauge must return to exactly the baseline. Poll-until
        // so the async close of the last sessions does not race the read; a
        // genuine leak never reaches baseline and fails here.
        let mut final_active = query_global_gauge(&mut worker, active).unwrap_or(0);
        let settle_deadline = Instant::now() + Duration::from_secs(3);
        while final_active != baseline_active && Instant::now() < settle_deadline {
            thread::sleep(Duration::from_millis(50));
            final_active = query_global_gauge(&mut worker, active).unwrap_or(0);
        }
        let gauge_incremented_mid_preread = observed_active >= 1;
        let gauge_back_to_baseline = final_active == baseline_active;

        println!(
            "limit=1: first_ok={first_ok} second_limited={second_limited} second_never_reached_backend={second_never_reached_backend} third_ok={third_ok} observed_active={observed_active} baseline_active={baseline_active} final_active={final_active} gauge_incremented_mid_preread={gauge_incremented_mid_preread} gauge_back_to_baseline={gauge_back_to_baseline}"
        );

        second_limited
            && second_never_reached_backend
            && third_ok
            && gauge_incremented_mid_preread
            && gauge_back_to_baseline
    } else {
        // Differential control: the SAME harness, but the raised cap must
        // let the second connection through concurrently, while the first
        // is still held open and unreleased.
        let second_ready = ready_rx.recv_timeout(Duration::from_secs(3));
        let second_response = raw_read_all(&mut second);
        let second_ok = matches!(
            &second_ready,
            Ok((index, captured)) if *index == 1
                && captured.as_slice() == hello_second.as_slice()
                && second_response.as_slice() == PER_IP_LIMITER_RESPONSES[1]
        );

        let _ = release_txs[0].send(());
        let _ = release_txs[1].send(());
        drop(first);
        drop(second);

        println!("limit={limit}: first_ok={first_ok} second_ok={second_ok}");

        second_ok
    };

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    if stopped && first_ok && differential_ok {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_tcp_sni_per_ip_limiter_rejects_second_then_admits_after_release() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "TCP SNI per-(cluster, IP) limiter: a concurrent second connection is rejected while the first is genuinely held open end-to-end (not a torn-down one-shot backend), and releasing the first admits a third connection -- proving the limiter both enforces and decrements",
            || try_tcp_sni_per_ip_limiter(1),
        ),
        State::Success,
    );
}

#[test]
fn test_tcp_sni_per_ip_limiter_admits_concurrent_connection_when_raised() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "TCP SNI per-(cluster, IP) limiter differential control: with the cap raised to 2, the SAME second concurrent connection that the limit=1 test rejects must instead reach the backend and complete end-to-end -- proving the rejection above is attributable to the limiter, not an artifact of holding a backend connection open",
            || try_tcp_sni_per_ip_limiter(2),
        ),
        State::Success,
    );
}
