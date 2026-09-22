//! End-to-end coverage for the `peer=` slot of the socket layer's `SOCKET`
//! log envelope, on a PROXY-protocol v2 TLS frontend.
//!
//! `log_socket_context!` (`lib/src/socket.rs`) renders the prefix every
//! `SOCKET` error line carries. Its only production caller is
//! `impl SocketHandler for FrontRustls` — the thirteen expansions between
//! `socket.rs`'s `FrontRustls::socket_read` and `socket_write_vectored` are all
//! of them, and the other two handlers (`TcpStream`, `SessionTcpStream`) route
//! their error logs through the free function `log_socket_module_prefix`
//! instead. So `SOCKET` on a TLS frontend is exactly this macro.
//!
//! The macro used to render `peer` from a live `getpeername(2)` on the raw
//! socket while its module-level twin already preferred the address the
//! handler cached. On a PROXY-protocol frontend the two disagree: `MUX-H2`
//! named the advertised client and `SOCKET` named the load balancer, for one
//! and the same connection. That was an artefact of `FrontRustls` having
//! carried no cached address until `SocketHandler::peer_addr` and
//! `FrontRustls::configured_peer` landed, not a deliberate split between
//! layers, and `doc/observability.md` recorded it as a follow-up needing its
//! own test. This file is that test.
//!
//! The unit test `socket::tests::log_socket_context_renders_the_cached_peer_
//! not_a_live_lookup` pins the same slot, but expands the macro against a
//! `SessionTcpStream` because `lib/src/socket.rs` builds no TLS. This test is
//! worth its runtime because it drives the REAL path the operator sees:
//! PROXY-v2 parse → `upgrade_expect` → TLS handshake → `upgrade_handshake` →
//! `FrontRustls::configured_peer` → `SocketHandler::peer_addr` → the rendered
//! `SOCKET` line, on a connection whose kernel peer is genuinely a different
//! address.
//!
//! ## Why `error`, and not a raised level
//!
//! The opposite of `h2_log_context_tests`, and for the opposite reason.
//! Measured: EVERY expansion of `log_socket_context!` in `socket.rs` sits
//! inside an `error!`, and every one of them needs an abnormal condition — a
//! `MAX_LOOP_ITERATIONS` overrun, an unhandled `read_tls`/`write` error kind,
//! or a `process_new_packets` failure. A healthy TLS session emits no `SOCKET`
//! line at any level, so raising the level cannot help and the test must
//! provoke an error path instead. The capture therefore stays at plain
//! `"error"` — the level every other worker in the suite already runs at — and
//! the test corrupts the TLS stream on purpose.
//!
//! ## Why corrupting the record is the right provocation
//!
//! Keeping the connection ESTABLISHED is load-bearing for WHICH HALF is
//! proven, not for redness. Be precise about this. A provocation that killed
//! the connection first would still redden the test, because the pre-fix
//! rendering would then be `peer=None` and the first assertion
//! (`advertised_lines != peer_slot_lines.len()`) fires on `None` just as it
//! fires on the raw address. What only an ESTABLISHED connection buys is the
//! SECOND assertion, `raw_peer_lines != 0`: it can only discriminate while
//! `getpeername(2)` succeeds and returns the load balancer. So this test
//! proves the PROXY mis-attribution — a healthy live lookup naming the wrong
//! host — while the `ENOTCONN` half is pinned by
//! `socket::tests::log_socket_context_renders_the_cached_peer_when_the_live_lookup_fails`.
//! The `state=Some("ESTABLISHED")` predicate below turns that premise into a
//! guard rather than a comment.
//!
//! The corrupt record lands after `h2_handshake` has read the server's
//! SETTINGS, so the session has demonstrably left `HttpsStateMachine::
//! Handshake` and a `FrontRustls` exists. Sending it earlier should race the
//! client's own `Finished` into the same read burst and fail inside
//! `protocol/rustls.rs`, which logs `RUSTLS` rather than `SOCKET` — reasoned
//! from the state machine, not measured, and nothing here asserts it.

use std::{
    io::Write,
    net::{SocketAddr, TcpStream},
    sync::Arc,
    thread,
    time::Duration,
};

use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, AddCertificate, CertificateAndKey, ListenerType, RequestHttpFrontend,
        SocketAddress, request::RequestType,
    },
};

use crate::{
    mock::https_client::Verifier,
    sozu::{log_capture::WorkerLogCapture, worker::Worker},
    tests::{State, provide_port, repeat_until_error_or},
};

/// Every `SOCKET` line carries this tag and no other prefix in the proxy uses
/// it. `log_socket_context!` and `log_socket_module_prefix` both render it, so
/// a match alone does not say which; the TLS frontend under test has only the
/// macro.
const SOCKET_TAG: &str = "SOCKET";

/// The `state=` slot a healthy connection renders, straight from
/// `getsockopt(TCP_INFO)`. Asserted because "the connection is still up when
/// the line is written" is the premise that makes this test's negative
/// assertion meaningful — see the module note.
const ESTABLISHED_STATE: &str = r#"state=Some("ESTABLISHED")"#;

/// Source port announced in the PROXY-v2 header. Same choice, and the same
/// reasoning, as `h2_log_context_tests`: below Linux's default
/// `net.ipv4.ip_local_port_range` floor (32768) and below the e2e port
/// registry's `PORT_SEARCH_START` (20000), so it can collide neither with the
/// test client's real ephemeral source port nor with a listener this suite
/// allocates. The test asserts the two differ anyway.
const PROXY_ADVERTISED_CLIENT_PORT: u16 = 9974;

/// A 28-byte PROXY-protocol v2 `PROXY` header for an IPv4/STREAM connection
/// from `127.0.0.1:source_port` to `127.0.0.1:destination_port`.
fn proxy_protocol_v2_header(source_port: u16, destination_port: u16) -> Vec<u8> {
    let mut header = vec![
        // 12-byte signature
        0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
        // version 2, command PROXY
        0x21, // AF_INET, STREAM
        0x11, // address block length: 12
        0x00, 0x0C, // source IP
        127, 0, 0, 1, // destination IP
        127, 0, 0, 1,
    ];
    header.extend_from_slice(&source_port.to_be_bytes());
    header.extend_from_slice(&destination_port.to_be_bytes());
    header
}

/// A structurally valid but undecryptable TLS 1.3 record: content type
/// `application_data` (0x17), legacy version 0x0303, a 48-byte body of filler.
///
/// The framing is deliberate. rustls accepts the record header, buffers the
/// body and fails in `process_new_packets` with a decrypt error — which is
/// `FrontRustls::socket_read`'s `could not process read TLS packets` arm, one
/// of the macro's thirteen expansions — measured: that is the arm the captured
/// line comes from, and it names `could not process read TLS packets:
/// DecryptError`. A malformed HEADER should instead be rejected by `read_tls`
/// itself, whose `_ =>` arm is also a `log_socket_context!` site; that is read
/// off the match arms rather than measured, and nothing here asserts it. The
/// decrypt path is the one this test actually drives.
fn undecryptable_tls_record() -> Vec<u8> {
    let body = [0xABu8; 48];
    let mut record = vec![0x17, 0x03, 0x03];
    record.extend_from_slice(&(body.len() as u16).to_be_bytes());
    record.extend_from_slice(&body);
    record
}

// =========================================================================
// A PROXY-v2 TLS frontend renders the ADVERTISED client in a SOCKET `peer=`
//
// One PROXY-v2 TLS+h2 connection, established to the point where the mux
// holds a `FrontRustls`, then deliberately corrupted so the socket layer
// logs. The test asserts on the `peer=` slot of the `SOCKET` lines the worker
// wrote: the advertised `127.0.0.1:9974` must be there, and the kernel's view
// of the connection — the test client's ephemeral source port — must not.
//
// Both halves matter. The positive one alone would stay green against a macro
// that rendered a constant; the negative one alone would stay green against a
// macro that rendered `None`.
// =========================================================================

/// To SEE THIS RED: in `lib/src/socket.rs`, revert the `peer` slot of
/// `log_socket_context!` from
/// `peer = crate::socket::SocketHandler::peer_addr($self),` to the pre-fix
/// `peer = $self.socket_ref().peer_addr().ok(),`. Measured on one iteration:
/// 1 `SOCKET` line, 0 naming the PROXY-advertised client, 1 naming the
/// connection's raw TCP source.
fn try_tls_socket_log_peer_is_the_advertised_client() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);

    let capture = WorkerLogCapture::new("tls-socket-peer");
    let (config, listeners, state) = Worker::empty_https_config(front_address.clone().into());
    // Plain `error`: see the module note. Every `log_socket_context!`
    // expansion is an `error!`, so nothing needs raising — the line only
    // needs provoking.
    let mut worker = Worker::start_new_worker_owned_with_logging(
        "TLS-SOCK-PEER",
        config,
        listeners,
        state,
        &capture.target(),
        "error",
    );

    let mut listener_builder = ListenerBuilder::new_https(front_address.clone());
    listener_builder.with_expect_proxy(true);
    worker.send_proxy_request_type(RequestType::AddHttpsListener(
        listener_builder
            .to_tls(None)
            .expect("could not build the https listener config"),
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
    worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
        address: front_address.clone(),
        certificate: CertificateAndKey {
            certificate: String::from(include_str!("../../../lib/assets/local-certificate.pem")),
            key: String::from(include_str!("../../../lib/assets/local-key.pem")),
            certificate_chain: vec![],
            versions: vec![],
            names: vec![],
        },
        expired_at: None,
    }));
    worker.read_to_last();

    // No backend and no request: the connection never needs to be routed. The
    // H2 handshake below is only there to prove the session reached the mux,
    // which is what guarantees a `FrontRustls` exists to log through.
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}")
        .parse()
        .expect("front address must parse");
    let tcp = TcpStream::connect(front_addr).expect("could not connect to the frontend");
    tcp.set_nodelay(true).expect("set TCP_NODELAY");
    // Short read timeout, same reason as `h2_tests::try_h2_with_proxy_protocol_v2`:
    // rustls's `complete_io` blocks in `read_tls` after a write, and the
    // default would let the server's SETTINGS-ACK watchdog expire before the
    // client's ACK lands.
    tcp.set_read_timeout(Some(Duration::from_millis(500)))
        .expect("set read timeout");
    tcp.set_write_timeout(Some(Duration::from_secs(5)))
        .expect("set write timeout");

    // The kernel's view of this connection: what the pre-fix macro rendered.
    let raw_peer = tcp
        .local_addr()
        .expect("the test client socket must have a local address");
    if raw_peer.port() == PROXY_ADVERTISED_CLIENT_PORT {
        println!(
            "TLS SOCKET peer - the ephemeral source port collided with the advertised \
             {PROXY_ADVERTISED_CLIENT_PORT}; the two renderings would be \
             indistinguishable"
        );
        return State::Fail;
    }

    let mut tcp = tcp;
    tcp.write_all(&proxy_protocol_v2_header(
        PROXY_ADVERTISED_CLIENT_PORT,
        front_port,
    ))
    .expect("could not send the PROXY v2 header");
    tcp.flush().expect("could not flush the PROXY v2 header");

    let mut tls_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Verifier))
        .with_no_client_auth();
    tls_config.alpn_protocols = vec![b"h2".to_vec()];
    let server_name =
        rustls::pki_types::ServerName::try_from("localhost").expect("server name must parse");
    let connection = rustls::ClientConnection::new(Arc::new(tls_config), server_name.to_owned())
        .expect("could not create the rustls client connection");
    let mut tls = rustls::StreamOwned::new(connection, tcp);

    // Reads the server's SETTINGS, so on return the session has left the TLS
    // handshake state and the mux holds a `FrontRustls`.
    super::h2_utils::h2_handshake(&mut tls);

    // Straight onto the TCP socket, bypassing rustls: the server sees a record
    // it cannot decrypt while the connection underneath stays perfectly
    // healthy.
    tls.sock
        .write_all(&undecryptable_tls_record())
        .expect("could not send the corrupt TLS record");
    tls.sock
        .flush()
        .expect("could not flush the corrupt record");

    // Let the worker read the record and write its line while the client
    // socket is still open — the whole point being that `getpeername(2)`
    // would succeed here.
    thread::sleep(Duration::from_millis(300));

    drop(tls);
    thread::sleep(Duration::from_millis(200));

    worker.soft_stop();
    // Joins the worker thread, which is what drops its thread-local `LOGGER`
    // and flushes the `MultiLineWriter` behind the `file://` backend. Read the
    // capture only after this returns.
    let stopped = worker.wait_for_server_stop();

    let socket_lines = capture.lines_containing(SOCKET_TAG);
    let advertised = format!("peer=Some(127.0.0.1:{PROXY_ADVERTISED_CLIENT_PORT})");
    let raw = format!("peer=Some({raw_peer})");
    let peer_slot_lines: Vec<&String> = socket_lines
        .iter()
        .filter(|l| l.contains("peer="))
        .collect();
    let advertised_lines = peer_slot_lines
        .iter()
        .filter(|l| l.contains(&advertised))
        .count();
    let raw_peer_lines = peer_slot_lines.iter().filter(|l| l.contains(&raw)).count();
    // The design's premise, asserted rather than assumed. This test proves the
    // half of the defect where the live lookup SUCCEEDS and is wrong, which it
    // can only do while the connection is up: on a reset socket
    // `getpeername(2)` answers `ENOTCONN` and the pre-fix macro would render
    // `peer=None`, a different half with its own unit test. If the corrupt
    // record ever started arriving after a reset, the `raw_peer_lines` check
    // below would quietly stop discriminating and this test would keep
    // passing for the wrong reason.
    let established_lines = peer_slot_lines
        .iter()
        .filter(|l| l.contains(ESTABLISHED_STATE))
        .count();

    println!(
        "TLS SOCKET peer - stopped={stopped}, {SOCKET_TAG} lines={}, with a peer= slot={}, \
         advertised({advertised})={advertised_lines}, raw({raw})={raw_peer_lines}, \
         established={established_lines}",
        socket_lines.len(),
        peer_slot_lines.len()
    );

    if peer_slot_lines.is_empty() {
        println!(
            "TLS SOCKET peer - captured no {SOCKET_TAG} line carrying a peer= slot. \
             Every `log_socket_context!` expansion is an `error!`, so the level is \
             not the problem: the corrupt record did not reach \
             `FrontRustls::socket_read`, or the session failed earlier, inside \
             `protocol/rustls.rs`, which logs RUSTLS instead."
        );
        for line in capture.contents().lines().take(20) {
            println!("TLS SOCKET peer - captured: {line}");
        }
        return State::Fail;
    }

    // EVERY rendered slot, not merely one: the advertised address is cached on
    // the handler, so a line that renders anything else — the raw peer, or
    // `None` once the client has gone away — is the defect this guards.
    if advertised_lines != peer_slot_lines.len() {
        println!(
            "TLS SOCKET peer - {advertised_lines} of {} {SOCKET_TAG} peer= slots name the \
             PROXY-advertised client. Offending lines:",
            peer_slot_lines.len()
        );
        for line in peer_slot_lines
            .iter()
            .filter(|l| !l.contains(&advertised))
            .take(5)
        {
            println!("TLS SOCKET peer - {line}");
        }
        return State::Fail;
    }

    if established_lines != peer_slot_lines.len() {
        println!(
            "TLS SOCKET peer - {established_lines} of {} peer= slots were written on an \
             ESTABLISHED connection. The negative assertion below only discriminates while \
             `getpeername(2)` still succeeds, so a line logged on a dead socket makes this \
             test pass for the wrong reason. Offending lines:",
            peer_slot_lines.len()
        );
        for line in peer_slot_lines
            .iter()
            .filter(|l| !l.contains(ESTABLISHED_STATE))
            .take(5)
        {
            println!("TLS SOCKET peer - {line}");
        }
        return State::Fail;
    }

    if raw_peer_lines != 0 {
        println!(
            "TLS SOCKET peer - {raw_peer_lines} {SOCKET_TAG} line(s) name the \
             connection's raw TCP peer {raw_peer} instead of the \
             PROXY-advertised client. Offending lines:"
        );
        for line in peer_slot_lines.iter().filter(|l| l.contains(&raw)).take(5) {
            println!("TLS SOCKET peer - {line}");
        }
        return State::Fail;
    }

    if stopped {
        State::Success
    } else {
        println!("TLS SOCKET peer - stopped={stopped}");
        State::Fail
    }
}

/// What `n = 3` costs and buys is on `repeat_until_error_or`'s doc comment in
/// `e2e/src/tests/mod.rs`; this note keeps only what is local to this test.
/// The capture depends on two fixed sleeps (300 ms for the worker to read the
/// corrupt record and write its line, 200 ms after the client goes away), so a
/// sufficiently loaded box could capture zero peer= slots and fail on the
/// `peer_slot_lines.is_empty()` arm, whose message says exactly that. Measured
/// 12/12 green across four runs here; if it ever flakes, lengthen the first
/// sleep rather than weakening an assertion.
#[test]
fn test_tls_socket_log_peer_is_the_advertised_client() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "TLS: the SOCKET peer= slot names the PROXY-v2 advertised client",
            try_tls_socket_log_peer_is_the_advertised_client
        ),
        State::Success
    );
}
