//! A large `Content-Length` transfer through sozu, download or upload, moves
//! every byte (sozu-proxy/sozu#1906).
//!
//! The report: a multi-gigabyte download through an HTTPS listener with
//! `alpn_protocols = ["h2", "http/1.1"]`, to an HTTP/1.1 client, ended early
//! at a random offset ("Premature end of Content-Length delimited message
//! body"). The cause was the iteration budget of `Mux::ready_inner`
//! (`lib/src/protocol/mux/mod.rs`): when both peers kept up for
//! `MAX_LOOP_ITERATIONS` iterations of one `ready()` call (about 160 MiB with
//! 16 KiB buffers), the session was closed although every iteration had
//! forwarded bytes. It did not depend on the ALPN list, on TLS, or on the
//! direction. Such a pass now yields to the event loop
//! (`http.mux.loop_budget_yield`), which resumes it after its next poll. These
//! transfers run as fast as loopback allows, which is what reaches the budget;
//! on the unfixed code about one run in ten was cut.
//! `a_transfer_progressing_at_the_loop_limit_yields_and_resumes_without_a_new_event`
//! in `lib/src/protocol/mux/mod.rs` reproduces it deterministically.
//!
//! The other scenarios exercise backpressure: a client that reads in bursts
//! with pauses parks the backend on a full stream buffer, with or without a
//! backend that closes right after its last byte.
//!
//! Each scenario asserts the provable invariant: every body byte arrives, and
//! the loop budget is never exhausted (`http.infinite_loop.error` stays 0).

use std::{
    io::{Read, Write},
    net::{Shutdown, SocketAddr, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
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
    mock::https_client::Verifier,
    port_registry::bind_std_listener,
    sozu::worker::Worker,
    tests::{
        State, h2_tests::query_proxy_count, h2_utils::connect_with_small_receive_buffer,
        provide_port, repeat_until_error_or, tests::create_local_address,
    },
};

/// The listener the client downloads through.
#[derive(Clone, Copy, Debug)]
enum Listener {
    /// Plaintext HTTP listener.
    Plain,
    /// HTTPS listener with the default `alpn_protocols` (`h2`, `http/1.1`).
    TlsH2Alpn,
    /// HTTPS listener with `alpn_protocols = ["http/1.1"]` only.
    TlsH1Alpn,
}

#[derive(Clone, Copy, Debug)]
struct Scenario {
    listener: Listener,
    body_size: usize,
    /// The backend closes its connection right after the last body byte
    /// (`Connection: close`).
    backend_close: bool,
    /// The client pauses this long after every `.0` bytes it reads.
    client_pause: Option<(usize, Duration)>,
    /// The backend pauses this long after every `.0` body bytes it writes.
    backend_pause: Option<(usize, Duration)>,
    /// The listener's `front_timeout` and `back_timeout`, in seconds.
    front_timeout: Option<u32>,
    back_timeout: Option<u32>,
}

impl Scenario {
    fn new(listener: Listener, body_size: usize) -> Self {
        Self {
            listener,
            body_size,
            backend_close: false,
            client_pause: None,
            backend_pause: None,
            front_timeout: None,
            back_timeout: None,
        }
    }
}

fn start_worker(name: &str, scenario: &Scenario, back_address: SocketAddr) -> (Worker, SocketAddr) {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let front: SocketAddr = front_address.clone().into();
    let mut worker = match scenario.listener {
        Listener::Plain => {
            let (config, listeners, state) = Worker::empty_http_config(front);
            let mut worker = Worker::start_new_worker_owned(name, config, listeners, state);
            worker.send_proxy_request_type(RequestType::AddHttpListener(
                ListenerBuilder::new_http(front_address.clone())
                    .with_front_timeout(scenario.front_timeout)
                    .with_back_timeout(scenario.back_timeout)
                    .to_http(None)
                    .unwrap(),
            ));
            worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
                interface: None,
                address: front_address.clone(),
                proxy: ListenerType::Http.into(),
                from_scm: false,
            }));
            worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
                "cluster_0",
            )));
            worker.send_proxy_request_type(RequestType::AddHttpFrontend(
                Worker::default_http_frontend("cluster_0", front),
            ));
            worker
        }
        Listener::TlsH2Alpn | Listener::TlsH1Alpn => {
            let alpn = match scenario.listener {
                Listener::TlsH1Alpn => Some(vec!["http/1.1".to_owned()]),
                _ => Some(vec!["h2".to_owned(), "http/1.1".to_owned()]),
            };
            let (config, listeners, state) = Worker::empty_https_config(front);
            let mut worker = Worker::start_new_worker_owned(name, config, listeners, state);
            worker.send_proxy_request_type(RequestType::AddHttpsListener(
                ListenerBuilder::new_https(front_address.clone())
                    .with_alpn_protocols(alpn)
                    .with_front_timeout(scenario.front_timeout)
                    .with_back_timeout(scenario.back_timeout)
                    .to_tls(None)
                    .unwrap(),
            ));
            worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
                interface: None,
                address: front_address.clone(),
                proxy: ListenerType::Https.into(),
                from_scm: false,
            }));
            worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
                "cluster_0",
            )));
            worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
                hostname: "localhost".to_owned(),
                ..Worker::default_http_frontend("cluster_0", front)
            }));
            worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
                address: front_address,
                certificate: CertificateAndKey {
                    certificate: String::from(include_str!(
                        "../../../lib/assets/local-certificate.pem"
                    )),
                    key: String::from(include_str!("../../../lib/assets/local-key.pem")),
                    certificate_chain: vec![],
                    versions: vec![],
                    names: vec![],
                },
                expired_at: None,
            }));
            worker
        }
    };
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));
    worker.read_to_last();
    (worker, front)
}

/// A backend answering one request with a `Content-Length` body of
/// `scenario.body_size` bytes, written in 64 KiB chunks.
fn spawn_backend(
    address: SocketAddr,
    scenario: Scenario,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<usize> {
    let listener = bind_std_listener(address, "large response backend");
    thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(_) if Instant::now() < deadline && !stop.load(Ordering::Relaxed) => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return 0,
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
        let mut request = Vec::new();
        let mut buf = [0u8; 4096];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => return 0,
                Ok(n) => request.extend_from_slice(&buf[..n]),
            }
        }
        let connection = if scenario.backend_close {
            "close"
        } else {
            "keep-alive"
        };
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: {connection}\r\n\r\n",
            scenario.body_size
        );
        if stream.write_all(head.as_bytes()).is_err() {
            return 0;
        }
        let chunk: Vec<u8> = (0..64 * 1024).map(|i| (i % 251) as u8).collect();
        let mut written = 0;
        let mut since_pause = 0usize;
        while written < scenario.body_size {
            let len = chunk.len().min(scenario.body_size - written);
            if stream.write_all(&chunk[..len]).is_err() {
                return written;
            }
            written += len;
            since_pause += len;
            if let Some((every, pause)) = scenario.backend_pause
                && since_pause >= every
                && written < scenario.body_size
            {
                since_pause = 0;
                thread::sleep(pause);
            }
        }
        if scenario.backend_close {
            let _ = stream.shutdown(Shutdown::Both);
        } else {
            // Keep the connection open until the test ends, like a
            // keep-alive backend waiting for the next request.
            stream
                .set_read_timeout(Some(Duration::from_millis(100)))
                .ok();
            while !stop.load(Ordering::Relaxed) {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    _ => {}
                }
            }
        }
        written
    })
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

/// How the client's download ended.
#[derive(Debug)]
struct Outcome {
    status_ok: bool,
    body_received: usize,
    ended: &'static str,
    elapsed: Duration,
    spins: i64,
    /// `http.mux.loop_budget_yield`: passes that yielded while forwarding.
    yields: i64,
}

fn download(name: &str, scenario: Scenario) -> Outcome {
    let back_address = create_local_address();
    let stop = Arc::new(AtomicBool::new(false));
    let backend = spawn_backend(back_address, scenario, stop.clone());
    let (mut worker, front) = start_worker(name, &scenario, back_address);

    // A small receive buffer makes the client's pauses backpressure sozu at
    // once instead of after the kernel has absorbed megabytes.
    let tcp = if scenario.client_pause.is_some() {
        connect_with_small_receive_buffer(front)
    } else {
        TcpStream::connect_timeout(&front, Duration::from_secs(5)).unwrap()
    };
    tcp.set_read_timeout(Some(Duration::from_secs(20))).ok();
    tcp.set_write_timeout(Some(Duration::from_secs(5))).ok();
    let mut stream: Box<dyn ReadWrite> = match scenario.listener {
        Listener::Plain => Box::new(tcp),
        Listener::TlsH2Alpn | Listener::TlsH1Alpn => {
            let mut tls_config = ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(Verifier))
                .with_no_client_auth();
            tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];
            let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
            let conn = rustls::ClientConnection::new(Arc::new(tls_config), server_name.to_owned())
                .unwrap();
            Box::new(rustls::StreamOwned::new(conn, tcp))
        }
    };

    let start = Instant::now();
    let sent = stream
        .write_all(b"GET /large HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .is_ok()
        && stream.flush().is_ok();
    let mut head = Vec::new();
    let mut body_received = 0usize;
    let mut since_pause = 0usize;
    let mut header_done = false;
    let mut buf = vec![0u8; 64 * 1024];
    let deadline = start + Duration::from_secs(120);
    let ended = loop {
        if !sent {
            break "request not sent";
        }
        if header_done && body_received >= scenario.body_size {
            break "complete";
        }
        if Instant::now() > deadline {
            break "deadline";
        }
        match stream.read(&mut buf) {
            Ok(0) => break "eof",
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break "eof (no close_notify)";
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break "client read timeout",
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => break "client read timeout",
            Err(_) => break "error",
            Ok(n) => {
                let mut data = &buf[..n];
                if !header_done {
                    head.extend_from_slice(data);
                    match head.windows(4).position(|w| w == b"\r\n\r\n") {
                        Some(at) => {
                            header_done = true;
                            body_received = head.len() - (at + 4);
                            data = &[];
                        }
                        None => continue,
                    }
                } else {
                    body_received += data.len();
                }
                since_pause += data.len();
                if let Some((every, pause)) = scenario.client_pause
                    && since_pause >= every
                {
                    since_pause = 0;
                    thread::sleep(pause);
                }
            }
        }
    };
    let elapsed = start.elapsed();
    let status_ok = head.starts_with(b"HTTP/1.1 200");
    let spins = query_proxy_count(
        &mut worker,
        sozu_lib::metrics::names::http::INFINITE_LOOP_ERROR,
    );
    let yields = query_proxy_count(
        &mut worker,
        sozu_lib::metrics::names::http::MUX_LOOP_BUDGET_YIELD,
    );
    drop(stream);
    stop.store(true, Ordering::Relaxed);
    worker.soft_stop();
    let _ = worker.wait_for_server_stop();
    let written = backend.join().unwrap_or(0);
    let outcome = Outcome {
        status_ok,
        body_received,
        ended,
        elapsed,
        spins,
        yields,
    };
    println!(
        "{name}: {scenario:?} -> backend wrote {written}, {outcome:?}, body {}/{}",
        outcome.body_received, scenario.body_size
    );
    outcome
}

fn check(name: &'static str, scenario: Scenario) -> State {
    let outcome = download(name, scenario);
    if outcome.status_ok && outcome.body_received == scenario.body_size && outcome.spins == 0 {
        State::Success
    } else {
        State::Fail
    }
}

const MIB: usize = 1024 * 1024;

fn run(name: &'static str, scenario: Scenario) {
    assert_eq!(
        repeat_until_error_or(3, name, || check(name, scenario)),
        State::Success,
        "{name}"
    );
}

// ---- Fast backend, eager client: throughput only ----

#[test]
fn test_large_download_eager_client_tls_h2_alpn() {
    run(
        "LARGE-EAGER-TLS-H2",
        Scenario::new(Listener::TlsH2Alpn, 256 * MIB),
    );
}

#[test]
fn test_large_download_eager_client_tls_h1_alpn() {
    run(
        "LARGE-EAGER-TLS-H1",
        Scenario::new(Listener::TlsH1Alpn, 256 * MIB),
    );
}

#[test]
fn test_large_download_eager_client_plain() {
    run(
        "LARGE-EAGER-PLAIN",
        Scenario::new(Listener::Plain, 256 * MIB),
    );
}

// ---- Fast backend, bursty client: backpressure parks the backend ----

fn bursty_client(listener: Listener) -> Scenario {
    Scenario {
        client_pause: Some((4 * MIB, Duration::from_millis(300))),
        ..Scenario::new(listener, 64 * MIB)
    }
}

#[test]
fn test_large_download_bursty_client_tls_h2_alpn() {
    run("LARGE-BURSTY-TLS-H2", bursty_client(Listener::TlsH2Alpn));
}

#[test]
fn test_large_download_bursty_client_plain() {
    run("LARGE-BURSTY-PLAIN", bursty_client(Listener::Plain));
}

/// The backend sends the whole body and closes while the client is still
/// reading slowly: sozu holds the tail in its buffers (#1820's shape).
#[test]
fn test_large_download_bursty_client_backend_closes_tls_h2_alpn() {
    run(
        "LARGE-BURSTY-CLOSE-TLS-H2",
        Scenario {
            backend_close: true,
            ..bursty_client(Listener::TlsH2Alpn)
        },
    );
}

#[test]
fn test_large_download_bursty_client_backend_closes_plain() {
    run(
        "LARGE-BURSTY-CLOSE-PLAIN",
        Scenario {
            backend_close: true,
            ..bursty_client(Listener::Plain)
        },
    );
}

// ---- Pauses longer than the other side's timeout (sozu-proxy/sozu#1906) ----
//
// `back_timeout` and `front_timeout` bound the inactivity of one socket each.
// A client that pauses longer than `back_timeout` leaves Sōzu holding bytes
// for it, so Sōzu stops reading the backend itself: the backend is not
// inactive. A backend that pauses longer than `front_timeout` leaves a client
// that has taken every byte waiting: the client is not inactive. Both used to
// cut the response mid-body (`backend_response_timeout`, then a silent close).

/// A client that pauses 4 s, twice the 2 s `back_timeout`, within a 10 s
/// `front_timeout`.
#[test]
fn test_large_download_client_pause_longer_than_back_timeout_plain() {
    run(
        "LARGE-CLIENT-PAUSE-PLAIN",
        Scenario {
            client_pause: Some((4 * MIB, Duration::from_secs(4))),
            front_timeout: Some(10),
            back_timeout: Some(2),
            ..Scenario::new(Listener::Plain, 8 * MIB)
        },
    );
}

/// A backend that pauses 4 s, twice the 2 s `front_timeout`, within a 10 s
/// `back_timeout`.
#[test]
fn test_large_download_backend_pause_longer_than_front_timeout_plain() {
    run(
        "LARGE-BACKEND-PAUSE-PLAIN",
        Scenario {
            backend_pause: Some((4 * MIB, Duration::from_secs(4))),
            front_timeout: Some(2),
            back_timeout: Some(10),
            ..Scenario::new(Listener::Plain, 8 * MIB)
        },
    );
}

// ---- Upload: the bytes flow from the client to the backend ----

/// The client uploads a large `Content-Length` body as fast as it can. No
/// byte goes to the client until the backend answers.
fn try_large_upload(name: &'static str, listener: Listener) -> State {
    let body_size = 256 * MIB;
    let back_address = create_local_address();
    let backend_listener = bind_std_listener(back_address, "large upload backend");
    let backend = thread::spawn(move || {
        let (mut stream, _) = backend_listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
        let mut buf = vec![0u8; 64 * 1024];
        let mut head = Vec::new();
        let mut body = 0usize;
        loop {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => return body,
                Ok(n) => {
                    if body == 0 && !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        head.extend_from_slice(&buf[..n]);
                        if let Some(at) = head.windows(4).position(|w| w == b"\r\n\r\n") {
                            body = head.len() - (at + 4);
                        }
                    } else {
                        body += n;
                    }
                    if body >= body_size {
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        );
                        return body;
                    }
                }
            }
        }
    });
    let scenario = Scenario::new(listener, body_size);
    let (mut worker, front) = start_worker(name, &scenario, back_address);
    let tcp = TcpStream::connect_timeout(&front, Duration::from_secs(5)).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(20))).ok();
    tcp.set_write_timeout(Some(Duration::from_secs(20))).ok();
    let mut stream: Box<dyn ReadWrite> = match listener {
        Listener::Plain => Box::new(tcp),
        Listener::TlsH2Alpn | Listener::TlsH1Alpn => {
            let mut tls_config = ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(Verifier))
                .with_no_client_auth();
            tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];
            let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
            let conn = rustls::ClientConnection::new(Arc::new(tls_config), server_name.to_owned())
                .unwrap();
            Box::new(rustls::StreamOwned::new(conn, tcp))
        }
    };
    let head =
        format!("POST /upload HTTP/1.1\r\nHost: localhost\r\nContent-Length: {body_size}\r\n\r\n");
    let chunk: Vec<u8> = (0..64 * 1024).map(|i| (i % 251) as u8).collect();
    let mut sent = 0usize;
    let mut ok = stream.write_all(head.as_bytes()).is_ok();
    while ok && sent < body_size {
        ok = stream.write_all(&chunk).is_ok();
        if ok {
            sent += chunk.len();
        }
    }
    let _ = stream.flush();
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    let spins = query_proxy_count(
        &mut worker,
        sozu_lib::metrics::names::http::INFINITE_LOOP_ERROR,
    );
    let yields = query_proxy_count(
        &mut worker,
        sozu_lib::metrics::names::http::MUX_LOOP_BUDGET_YIELD,
    );
    drop(stream);
    worker.soft_stop();
    let _ = worker.wait_for_server_stop();
    let received = backend.join().unwrap_or(0);
    println!(
        "{name}: client sent {sent}/{body_size}, backend received {received}, spins {spins}, yields {yields}, response {:?}",
        String::from_utf8_lossy(&response[..response.len().min(40)])
    );
    if sent == body_size
        && received == body_size
        && spins == 0
        && response.starts_with(b"HTTP/1.1 200")
    {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_large_upload_plain() {
    assert_eq!(
        repeat_until_error_or(3, "LARGE-UPLOAD-PLAIN", || try_large_upload(
            "LARGE-UPLOAD-PLAIN",
            Listener::Plain
        )),
        State::Success
    );
}

#[test]
fn test_large_upload_tls_h2_alpn() {
    assert_eq!(
        repeat_until_error_or(3, "LARGE-UPLOAD-TLS-H2", || try_large_upload(
            "LARGE-UPLOAD-TLS-H2",
            Listener::TlsH2Alpn
        )),
        State::Success
    );
}
