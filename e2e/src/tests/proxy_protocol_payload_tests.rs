//! End-to-end coverage for the bytes that follow an inbound PROXY header
//! (sozu-proxy/sozu#1841).
//!
//! `ExpectProxyProtocol::readable` used to read the header in fixed stages of
//! 28, 52 and 232 bytes. A header shorter than the stage it landed in pulled
//! the start of the payload into the header buffer, and that buffer was
//! dropped on upgrade: a 16-byte `LOCAL` header lost 12 payload bytes, a
//! 36-byte `PROXY` header with TLVs lost 16. The expect stage now reads
//! exactly `16 + len` bytes, so every byte behind the header stays in the
//! socket for the next stage.
//!
//! Every header shape is sent twice: coalesced with the payload in ONE
//! `write`, which is what exposed the loss, and split into two writes with a
//! pause between them, which never did.
//!
//! ## Test list
//! 1. [`test_ppv2_tcp_expect_keeps_the_payload_behind_the_header`] — TCP
//!    cluster with `ExpectHeader`: the backend receives the payload byte-exact
//!    for `LOCAL`, IPv4, IPv4 + TLVs, IPv6 and IPv6 + TLVs; a v1 text header,
//!    an oversized header and a malformed one close without dialing.
//! 2. [`test_ppv2_http_expect_keeps_the_request_behind_the_header`] — HTTP
//!    listener with `expect_proxy`: the request reaches the backend intact.
//! 3. [`test_ppv2_https_expect_keeps_the_client_hello_behind_the_header`] —
//!    HTTPS listener with `expect_proxy`: the TLS handshake completes and the
//!    request is answered, including behind a header of exactly 232 bytes
//!    (the full buffer), which must not leave the handshake without READABLE
//!    interest.

use std::{
    io::{ErrorKind, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, AddCertificate, CertificateAndKey, Cluster, ListenerType,
        ProxyProtocolConfig, RequestHttpFrontend, request::RequestType,
    },
};

use crate::{
    mock::https_client::Verifier,
    port_registry::bind_std_listener,
    sozu::worker::Worker,
    tests::{State, repeat_until_error_or},
};

use super::tests::create_local_address;

const SIGNATURE_V2: [u8; 12] = [
    0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
];

/// One `PP2_TYPE_NOOP` TLV (type `0x04`, length 5): the receiver must skip it,
/// and it stands in for the TLVs HAProxy's `send-proxy-v2-ssl` appends.
const NOOP_TLV: [u8; 8] = [0x04, 0x00, 0x05, 0, 0, 0, 0, 0];

/// A v2 header: signature, ver/cmd, family, big-endian length, then `block`.
fn v2(command: u8, family: u8, block: &[u8]) -> Vec<u8> {
    let mut header = SIGNATURE_V2.to_vec();
    header.push(command);
    header.push(family);
    header.extend_from_slice(&(block.len() as u16).to_be_bytes());
    header.extend_from_slice(block);
    header
}

/// The 16-byte `LOCAL` header HAProxy sends for its own health checks.
fn v2_local() -> Vec<u8> {
    v2(0x20, 0x00, &[])
}

/// `PROXY` over `AF_INET`, `127.0.0.1:54321 -> 127.0.0.1:dst_port`, then `tlvs`.
fn v2_ipv4(dst_port: u16, tlvs: &[u8]) -> Vec<u8> {
    let mut block = vec![127, 0, 0, 1, 127, 0, 0, 1];
    block.extend_from_slice(&54321u16.to_be_bytes());
    block.extend_from_slice(&dst_port.to_be_bytes());
    block.extend_from_slice(tlvs);
    v2(0x21, 0x11, &block)
}

/// `PROXY` over `AF_INET6`, `[::1]:54321 -> [::1]:dst_port`, then `tlvs`.
fn v2_ipv6(dst_port: u16, tlvs: &[u8]) -> Vec<u8> {
    let mut loopback = [0u8; 16];
    loopback[15] = 1;
    let mut block = loopback.to_vec();
    block.extend_from_slice(&loopback);
    block.extend_from_slice(&54321u16.to_be_bytes());
    block.extend_from_slice(&dst_port.to_be_bytes());
    block.extend_from_slice(tlvs);
    v2(0x21, 0x21, &block)
}

/// The header shapes whose payload must survive, with their wire length so a
/// failure names the exact case.
fn surviving_headers(dst_port: u16, include_local: bool) -> Vec<(&'static str, Vec<u8>)> {
    let mut headers = Vec::new();
    if include_local {
        headers.push(("v2 LOCAL (16 bytes)", v2_local()));
    }
    headers.extend([
        ("v2 PROXY IPv4 (28 bytes)", v2_ipv4(dst_port, &[])),
        (
            "v2 PROXY IPv4 + TLV (36 bytes)",
            v2_ipv4(dst_port, &NOOP_TLV),
        ),
        ("v2 PROXY IPv6 (52 bytes)", v2_ipv6(dst_port, &[])),
        (
            "v2 PROXY IPv6 + TLV (60 bytes)",
            v2_ipv6(dst_port, &NOOP_TLV),
        ),
        // 12 address bytes + 204 TLV padding: exactly the 232-byte maximum.
        (
            "v2 PROXY IPv4 + TLV (232 bytes)",
            v2_ipv4(dst_port, &[0u8; 204]),
        ),
    ]);
    headers
}

/// Headers the expect stage must refuse without dialing a backend.
fn refused_headers(dst_port: u16) -> Vec<(&'static str, Vec<u8>)> {
    vec![
        (
            "v1 text header",
            format!("PROXY TCP4 127.0.0.1 127.0.0.1 54321 {dst_port}\r\n").into_bytes(),
        ),
        // One byte over the 232-byte maximum.
        ("v2 header of 233 bytes", v2_ipv4(dst_port, &[0u8; 205])),
        // `AF_INET` needs 12 address bytes; the length field promises 4.
        (
            "v2 IPv4 with a 4-byte block",
            v2(0x21, 0x11, &[127, 0, 0, 1]),
        ),
    ]
}

/// Deterministic payload longer than the 232-byte header buffer, so a loss
/// anywhere up to that bound shows.
fn payload() -> Vec<u8> {
    (0..300u32).map(|i| b'A' + (i % 26) as u8).collect()
}

/// Write `header` then `payload` either in one `write` or in two separated
/// by a pause long enough for the expect stage to read the header on its own.
fn send(front_address: SocketAddr, header: &[u8], payload: &[u8], coalesced: bool) -> TcpStream {
    let mut stream = TcpStream::connect(front_address).expect("could not connect to sozu");
    stream.set_nodelay(true).expect("set TCP_NODELAY");
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("set read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .expect("set write timeout");
    if coalesced {
        let mut bytes = header.to_vec();
        bytes.extend_from_slice(payload);
        stream.write_all(&bytes).expect("write header and payload");
    } else {
        stream.write_all(header).expect("write header");
        // An exposure window, not a synchronisation point: it makes the split
        // case actually split on the wire. Nothing observable is awaited.
        thread::sleep(Duration::from_millis(50));
        stream.write_all(payload).expect("write payload");
    }
    stream
}

/// Accept one backend connection, or `None` once `deadline` elapses.
fn accept_within(listener: &TcpListener, deadline: Duration) -> Option<TcpStream> {
    listener
        .set_nonblocking(true)
        .expect("set the backend listener nonblocking");
    let started = Instant::now();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_nonblocking(false)
                    .expect("set the backend stream blocking");
                stream
                    .set_read_timeout(Some(Duration::from_millis(100)))
                    .expect("set read timeout");
                return Some(stream);
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                if started.elapsed() >= deadline {
                    return None;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("backend accept failed: {e}"),
        }
    }
}

/// Read from `stream` until `done` holds for what was read, the peer closes,
/// or `deadline` elapses.
fn read_until(stream: &mut TcpStream, deadline: Duration, done: impl Fn(&[u8]) -> bool) -> Vec<u8> {
    let mut received = Vec::new();
    let mut buf = [0u8; 4096];
    let started = Instant::now();
    while !done(&received) && started.elapsed() < deadline {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => received.extend_from_slice(&buf[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => break,
        }
    }
    received
}

/// Assert that a refused header closes the client and never reaches the
/// backend.
fn refused_without_dial(
    front_address: SocketAddr,
    backend: &TcpListener,
    name: &str,
    header: &[u8],
    request: &[u8],
) -> bool {
    let mut client = send(front_address, header, request, true);
    let dialed = accept_within(backend, Duration::from_millis(300)).is_some();
    let answer = read_until(&mut client, Duration::from_secs(1), |_| false);
    let ok = !dialed && answer.is_empty();
    println!(
        "{name}: dialed={dialed} answer={} bytes -> {}",
        answer.len(),
        if ok { "ok" } else { "FAIL" }
    );
    ok
}

// =========================================================================
// Test 1: TCP cluster with `ExpectHeader`
// =========================================================================

/// To SEE THIS RED: restore the staged read in
/// `ExpectProxyProtocol::readable` (`lib/src/protocol/proxy_protocol/expect.rs`),
/// i.e. read up to 28, then 52, then 232 bytes instead of `16 + len`. The
/// coalesced `LOCAL` case then delivers `MNOP...` (12 bytes short), the
/// coalesced IPv4 + TLV case `QRST...` (16 bytes short), and the IPv6 + TLV
/// case 172 bytes short.
fn try_ppv2_tcp_expect_keeps_the_payload_behind_the_header() -> State {
    let front_address = create_local_address();
    let back_address = create_local_address();
    let (config, listeners, state) = Worker::empty_tcp_config(front_address);
    let mut worker = Worker::start_new_worker_owned("PPV2-PAYLOAD-TCP", config, listeners, state);

    worker.send_proxy_request_type(RequestType::AddTcpListener(
        ListenerBuilder::new_tcp(front_address.into())
            .to_tcp(None)
            .expect("could not build the tcp listener config"),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        interface: None,
        address: front_address.into(),
        proxy: ListenerType::Tcp.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(Cluster {
        proxy_protocol: Some(ProxyProtocolConfig::ExpectHeader as i32),
        ..Worker::default_cluster("cluster_0")
    }));
    worker.send_proxy_request_type(RequestType::AddTcpFrontend(Worker::default_tcp_frontend(
        "cluster_0",
        front_address,
    )));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));
    worker.read_to_last();

    let backend = bind_std_listener(back_address, "ppv2 payload tcp backend");
    let payload = payload();
    let mut all_ok = true;

    for coalesced in [true, false] {
        let mode = if coalesced { "coalesced" } else { "split" };
        for (name, header) in surviving_headers(front_address.port(), true) {
            let client = send(front_address, &header, &payload, coalesced);
            let received = match accept_within(&backend, Duration::from_secs(2)) {
                Some(mut stream) => read_until(&mut stream, Duration::from_secs(2), |r| {
                    r.len() >= payload.len()
                }),
                None => Vec::new(),
            };
            let ok = received == payload;
            println!(
                "TCP {name} {mode}: backend received {} of {} bytes, starting {:?} -> {}",
                received.len(),
                payload.len(),
                String::from_utf8_lossy(&received[..received.len().min(8)]),
                if ok { "ok" } else { "FAIL" }
            );
            all_ok &= ok;
            drop(client);
        }
    }

    for (name, header) in refused_headers(front_address.port()) {
        all_ok &= refused_without_dial(
            front_address,
            &backend,
            &format!("TCP {name}"),
            &header,
            &payload,
        );
    }

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    if all_ok && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_ppv2_tcp_expect_keeps_the_payload_behind_the_header() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "PROXY expect (TCP): the payload behind the header reaches the backend byte-exact",
            try_ppv2_tcp_expect_keeps_the_payload_behind_the_header,
        ),
        State::Success,
    );
}

// =========================================================================
// Shared HTTP plumbing
// =========================================================================

const BODY: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

fn http_request() -> Vec<u8> {
    format!(
        "POST /api HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{BODY}",
        BODY.len()
    )
    .into_bytes()
}

/// Accept the request sozu forwards, check it carries the request line and the
/// body unaltered, and answer it. `false` when no request arrived or it was
/// damaged.
fn serve_one_request(backend: &TcpListener) -> bool {
    let Some(mut stream) = accept_within(backend, Duration::from_secs(2)) else {
        println!("  backend: no connection");
        return false;
    };
    let received = read_until(&mut stream, Duration::from_secs(2), |r| {
        r.ends_with(BODY.as_bytes())
    });
    let intact =
        received.starts_with(b"POST /api HTTP/1.1\r\n") && received.ends_with(BODY.as_bytes());
    if !intact {
        println!(
            "  backend: damaged request {:?}",
            String::from_utf8_lossy(&received)
        );
    }
    let _ =
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\npong");
    intact
}

fn answered_200(answer: &[u8]) -> bool {
    answer.starts_with(b"HTTP/1.1 200") && answer.ends_with(b"pong")
}

// =========================================================================
// Test 2: HTTP listener with `expect_proxy`
//
// A `LOCAL` header is left out: it carries no address pair, so
// `HttpSession::upgrade_expect` closes it by design
// (`proxy_protocol_local_tests.rs`).
// =========================================================================

/// To SEE THIS RED: restore the staged read in `ExpectProxyProtocol::readable`
/// as described on the TCP test. The coalesced IPv4 + TLV request reaches the
/// HTTP parser as `PI HTTP/1.1...`, which answers 400 and never dials.
fn try_ppv2_http_expect_keeps_the_request_behind_the_header() -> State {
    let front_address = create_local_address();
    let back_address = create_local_address();
    let (config, listeners, state) = Worker::empty_http_config(front_address);
    let mut worker = Worker::start_new_worker_owned("PPV2-PAYLOAD-HTTP", config, listeners, state);

    let mut builder = ListenerBuilder::new_http(front_address.into());
    builder.with_expect_proxy(true);
    worker.send_proxy_request_type(RequestType::AddHttpListener(
        builder
            .to_http(None)
            .expect("could not build the http listener config"),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        interface: None,
        address: front_address.into(),
        proxy: ListenerType::Http.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "cluster_0",
    )));
    worker.send_proxy_request_type(RequestType::AddHttpFrontend(Worker::default_http_frontend(
        "cluster_0",
        front_address,
    )));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));
    worker.read_to_last();

    let backend = bind_std_listener(back_address, "ppv2 payload http backend");
    let request = http_request();
    let mut all_ok = true;

    for coalesced in [true, false] {
        let mode = if coalesced { "coalesced" } else { "split" };
        for (name, header) in surviving_headers(front_address.port(), false) {
            let mut client = send(front_address, &header, &request, coalesced);
            let intact = serve_one_request(&backend);
            let answer = read_until(&mut client, Duration::from_secs(2), answered_200);
            let ok = intact && answered_200(&answer);
            println!(
                "HTTP {name} {mode}: request intact={intact} answer={:?} -> {}",
                String::from_utf8_lossy(&answer[..answer.len().min(16)]),
                if ok { "ok" } else { "FAIL" }
            );
            all_ok &= ok;
        }
    }

    for (name, header) in refused_headers(front_address.port()) {
        all_ok &= refused_without_dial(
            front_address,
            &backend,
            &format!("HTTP {name}"),
            &header,
            &request,
        );
    }

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    if all_ok && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_ppv2_http_expect_keeps_the_request_behind_the_header() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "PROXY expect (HTTP): the request behind the header reaches the backend intact",
            try_ppv2_http_expect_keeps_the_request_behind_the_header,
        ),
        State::Success,
    );
}

// =========================================================================
// Test 3: HTTPS listener with `expect_proxy`
// =========================================================================

/// Run one TLS session whose ClientHello follows `header`, either in the same
/// `write` or after a pause, then send the request and return the answer.
fn https_exchange(front_address: SocketAddr, header: &[u8], coalesced: bool) -> Vec<u8> {
    let mut tls_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Verifier))
        .with_no_client_auth();
    tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let server_name =
        rustls::pki_types::ServerName::try_from("localhost").expect("server name must parse");
    let mut connection = rustls::ClientConnection::new(Arc::new(tls_config), server_name)
        .expect("could not create the rustls client connection");

    // The ClientHello, extracted so it can share a `write` with the header.
    let mut client_hello = Vec::new();
    while connection.wants_write() {
        connection
            .write_tls(&mut client_hello)
            .expect("could not serialize the ClientHello");
    }

    let tcp = send(front_address, header, &client_hello, coalesced);
    let mut tls = rustls::StreamOwned::new(connection, tcp);
    if tls
        .write_all(&http_request())
        .and_then(|()| tls.flush())
        .is_err()
    {
        return Vec::new();
    }
    let mut answer = Vec::new();
    let mut buf = [0u8; 4096];
    let started = Instant::now();
    while !answered_200(&answer) && started.elapsed() < Duration::from_secs(2) {
        match tls.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => answer.extend_from_slice(&buf[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => break,
        }
    }
    answer
}

/// To SEE THIS RED: restore the staged read in `ExpectProxyProtocol::readable`
/// as described on the TCP test. The coalesced IPv4 + TLV case then feeds
/// rustls a ClientHello missing its first 16 bytes; the handshake fails and the
/// request is never answered.
///
/// To SEE THE 232-BYTE CASE RED: in `ExpectProxyProtocol::readable`, restore
/// the `if self.index == self.frontend_buffer.len()` branch that removes
/// READABLE from the frontend interest once the 232-byte buffer is full.
/// `HttpsSession::upgrade_expect` (`lib/src/https.rs`) copies that interest
/// into the TLS handshake, which then never reads the ClientHello: both the
/// coalesced and the split 232-byte cases answer nothing, while every shorter
/// header still completes.
fn try_ppv2_https_expect_keeps_the_client_hello_behind_the_header() -> State {
    let front_address = create_local_address();
    let back_address = create_local_address();
    let (config, listeners, state) = Worker::empty_https_config(front_address);
    let mut worker = Worker::start_new_worker_owned("PPV2-PAYLOAD-HTTPS", config, listeners, state);

    let mut builder = ListenerBuilder::new_https(front_address.into());
    builder.with_expect_proxy(true);
    worker.send_proxy_request_type(RequestType::AddHttpsListener(
        builder
            .to_tls(None)
            .expect("could not build the https listener config"),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        interface: None,
        address: front_address.into(),
        proxy: ListenerType::Https.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "cluster_0",
    )));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: String::from("localhost"),
        ..Worker::default_http_frontend("cluster_0", front_address)
    }));
    worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
        address: front_address.into(),
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
        "cluster_0-0",
        back_address,
        None,
    )));
    worker.read_to_last();

    let backend = bind_std_listener(back_address, "ppv2 payload https backend");
    let mut all_ok = true;

    for coalesced in [true, false] {
        let mode = if coalesced { "coalesced" } else { "split" };
        for (name, header) in surviving_headers(front_address.port(), false) {
            // The backend is served from its own thread: the TLS client blocks
            // on the answer while sozu waits on the backend.
            let backend = backend.try_clone().expect("clone the backend listener");
            let served = thread::spawn(move || serve_one_request(&backend));
            let answer = https_exchange(front_address, &header, coalesced);
            let intact = served.join().expect("backend thread panicked");
            let ok = intact && answered_200(&answer);
            println!(
                "HTTPS {name} {mode}: request intact={intact} answer={:?} -> {}",
                String::from_utf8_lossy(&answer[..answer.len().min(16)]),
                if ok { "ok" } else { "FAIL" }
            );
            all_ok &= ok;
        }
    }

    for (name, header) in refused_headers(front_address.port()) {
        all_ok &= refused_without_dial(
            front_address,
            &backend,
            &format!("HTTPS {name}"),
            &header,
            b"\x16\x03\x01",
        );
    }

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    if all_ok && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_ppv2_https_expect_keeps_the_client_hello_behind_the_header() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "PROXY expect (HTTPS): the ClientHello behind the header completes the handshake",
            try_ppv2_https_expect_keeps_the_client_hello_behind_the_header,
        ),
        State::Success,
    );
}
