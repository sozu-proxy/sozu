//! End-to-end coverage for the `peer=` slot of the H2 mux's `MUX-H2` log
//! envelope, on a PROXY-protocol v2 frontend.
//!
//! `log_context!` / `log_context_stream!` (`lib/src/protocol/mux/h2.rs`) render
//! `peer=` from `SocketHandler::peer_addr()` — the address the handler cached
//! at accept/handshake time — rather than from a live `getpeername(2)` on the
//! raw socket. Two consequences, and this file pins the second:
//!
//! 1. the address survives the peer's RST, so the error lines an operator
//!    actually reads still name a host instead of `None`;
//! 2. on a frontend with `expect_proxy = true`, `upgrade_expect` adopts the
//!    PROXY-advertised source into `HttpsSession::peer_address`
//!    (`lib/src/https.rs`) and `upgrade_handshake` seeds `FrontRustls` from
//!    that same field, so `MUX-H2` names the advertised CLIENT and not the
//!    load balancer whose TCP connection carries it. Before the fix the two
//!    disagreed: for one request id the `HTTPS` line named the client and the
//!    `MUX-H2` line named the load balancer.
//!
//! The unit tests (`protocol::mux::h2::tests::log_context_renders_the_cached_
//! peer_not_a_live_lookup` and
//! `https::tests::front_rustls_peer_snapshot_is_the_session_peer_not_the_
//! accepted_socket`) fake the adoption by assigning `peer_address` directly.
//! This test is worth its runtime because it drives a REAL PROXY-v2 header
//! through the real accept path: expect-proxy parse → `upgrade_expect` →
//! TLS handshake → `upgrade_handshake` → `FrontRustls::configured_peer` →
//! `SocketHandler::peer_addr` → the rendered line. A seeding substitution
//! anywhere along that chain shows up here.
//!
//! ## Why this is reachable at all
//!
//! It was not, until [`Worker::start_new_worker_owned_with_logging`] and
//! [`WorkerLogCapture`] landed: the worker's log target was the hardcoded
//! string `"stdout"`, whose backend bypasses the capture libtest reads. See
//! `e2e/COVERAGE.md > Out of e2e reach by construction` for the three
//! mechanisms and what each one cost.
//!
//! ## Why `trace`, and not the default `error`
//!
//! Measured: a HEALTHY PROXY-v2 H2 session emits no `MUX-H2` line at level
//! `error` at all. Every `log_context!` expansion in `h2.rs` that fires on a
//! clean request/response is a `trace!`; the `debug!`, `warn!` and `error!`
//! ones all need an abnormal condition (a frame on a closed stream, a
//! SETTINGS-ACK timeout, an unparseable frame). The alternative was to provoke
//! an error path, which would have made the test's subject "what Sōzu logs
//! when something is wrong" instead of "what Sōzu logs about a healthy PROXY
//! frontend" — and the PROXY mis-attribution is present on every line, healthy
//! or not. Raising the level is also what the new spawn parameter is for, and
//! costs one worker's log file in a temporary directory.
//!
//! The spec scopes `trace` to `sozu_lib::protocol::mux` and leaves the rest of
//! the worker at `error`, so the capture stays in the tens of kilobytes.
//!
//! Note that `trace!` is compiled in under `any(debug_assertions,
//! feature = "logs-trace")` (`command/src/logging/logs.rs`). Every CI cell
//! runs `cargo test` in the dev profile, so `debug_assertions` holds; a
//! `--release` e2e run without `logs-trace` would capture nothing, and the
//! failure message below says so.

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

use super::h2_utils::{H2Frame, collect_response_frames, contains_headers_response, log_frames};
use crate::{
    mock::{
        aggregator::SimpleAggregator, async_backend::BackendHandle as AsyncBackend,
        https_client::Verifier,
    },
    sozu::{log_capture::WorkerLogCapture, worker::Worker},
    tests::{State, provide_port, repeat_until_error_or, tests::create_local_address},
};

/// Trace the H2 mux only. See the module note on why `error` cannot work.
const MUX_TRACE_SPEC: &str = "error,sozu_lib::protocol::mux=trace";

/// Source port announced in the PROXY-v2 header. Deliberately below Linux's
/// default `net.ipv4.ip_local_port_range` floor (32768) and below the e2e port
/// registry's own `PORT_SEARCH_START` (20000), so it can collide with neither
/// the test client's real ephemeral source port nor any listener this suite
/// allocates. The test asserts the two differ anyway.
const PROXY_ADVERTISED_CLIENT_PORT: u16 = 9973;

/// The protocol tag every `ConnectionH2` log line carries, and the only one
/// `log_context!` / `log_context_stream!` in `h2.rs` emit.
const MUX_H2_TAG: &str = "MUX-H2";

/// A 28-byte PROXY-protocol v2 `PROXY` header for an IPv4/STREAM connection
/// from `127.0.0.1:source_port` to `127.0.0.1:destination_port`. Same shape as
/// the one in `h2_tests::try_h2_with_proxy_protocol_v2`.
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

// =========================================================================
// A PROXY-v2 TLS+h2 frontend renders the ADVERTISED client in `peer=`
//
// One healthy request over a real PROXY-v2 header. The worker logs to its own
// file at `trace` for the mux, and the test asserts on the `peer=` slot of the
// `MUX-H2` lines it wrote: the advertised `127.0.0.1:9973` must be there, and
// the kernel's view of the connection — the test client's ephemeral source
// port — must not.
//
// Both halves matter. The positive one alone would stay green against a macro
// that rendered a constant; the negative one alone would stay green against a
// macro that rendered `None`.
// =========================================================================

/// To SEE THIS RED: in `lib/src/protocol/mux/h2.rs`, revert the `peer` slot of
/// BOTH `log_context!` (`:84`) and `log_context_stream!` (`:118`) from
/// `peer = $self.socket.peer_addr(),` to the pre-fix
/// `peer = $self.socket.socket_ref().peer_addr().ok(),`. Measured on one
/// iteration: 27 `peer=` slots, 0 naming the PROXY-advertised client, 26
/// naming the connection's raw TCP source — and the 27th rendering
/// `peer=None`, because by then the client has gone and `getpeername(2)`
/// answers `ENOTCONN`. That last line is the other half of the same fix,
/// reproduced end to end.
fn try_h2_proxy_protocol_peer_is_the_advertised_client() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);

    let capture = WorkerLogCapture::new("h2-proxy-peer");
    let (config, listeners, state) = Worker::empty_https_config(front_address.clone().into());
    let mut worker = Worker::start_new_worker_owned_with_logging(
        "H2-PP-PEER",
        config,
        listeners,
        state,
        &capture.target(),
        MUX_TRACE_SPEC,
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

    let back_address = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));
    worker.read_to_last();

    let backend = AsyncBackend::spawn_detached_backend(
        "PP-PEER-BACKEND",
        back_address,
        SimpleAggregator::default(),
        AsyncBackend::http_handler("pp-peer-pong"),
    );

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}")
        .parse()
        .expect("front address must parse");
    let tcp = TcpStream::connect(front_addr).expect("could not connect to the frontend");
    tcp.set_nodelay(true).expect("set TCP_NODELAY");
    // Short read timeout, for the same reason as in
    // `h2_tests::try_h2_with_proxy_protocol_v2`: rustls's `complete_io` blocks
    // in `read_tls` after a write, and the default would let the server's
    // SETTINGS-ACK watchdog expire before the client's ACK lands.
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
            "H2 PP peer - the ephemeral source port collided with the advertised \
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

    super::h2_utils::h2_handshake(&mut tls);

    // GET / over stream 1: `:method GET` (0x82), `:path /` (0x84),
    // `:scheme https` (0x87), then a literal `:authority: localhost`.
    let header_block = vec![
        0x82, 0x84, 0x87, 0x41, 0x09, b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't',
    ];
    tls.write_all(&H2Frame::headers(1, header_block, true, true).encode())
        .expect("could not send HEADERS");
    tls.flush().expect("could not flush HEADERS");

    let frames = collect_response_frames(&mut tls, 500, 5, 500);
    log_frames("H2 PP peer", &frames);
    let got_response = contains_headers_response(&frames);

    drop(tls);
    thread::sleep(Duration::from_millis(200));

    worker.soft_stop();
    // Joins the worker thread, which is what drops its thread-local `LOGGER`
    // and flushes the `MultiLineWriter` behind the `file://` backend. Read the
    // capture only after this returns.
    let stopped = worker.wait_for_server_stop();

    let mut backends = vec![backend];
    for b in backends.iter_mut() {
        b.stop_and_get_aggregator();
    }

    let mux_lines = capture.lines_containing(MUX_H2_TAG);
    let advertised = format!("peer=Some(127.0.0.1:{PROXY_ADVERTISED_CLIENT_PORT})");
    let raw = format!("peer=Some({raw_peer})");
    // `log_context_lite!` (`h2.rs:139`) renders the bare `MUX-H2` tag with no
    // session block, so not every tagged line carries a `peer=` slot. Only the
    // ones that do are under test.
    let peer_slot_lines: Vec<&String> = mux_lines.iter().filter(|l| l.contains("peer=")).collect();
    let advertised_lines = peer_slot_lines
        .iter()
        .filter(|l| l.contains(&advertised))
        .count();
    let raw_peer_lines = peer_slot_lines.iter().filter(|l| l.contains(&raw)).count();

    println!(
        "H2 PP peer - stopped={stopped}, got_response={got_response}, \
         {MUX_H2_TAG} lines={}, with a peer= slot={}, advertised({advertised})={advertised_lines}, \
         raw({raw})={raw_peer_lines}",
        mux_lines.len(),
        peer_slot_lines.len()
    );

    if mux_lines.is_empty() {
        println!(
            "H2 PP peer - captured no {MUX_H2_TAG} line at all. Either the \
             worker never reached the H2 mux, or `trace!` was compiled out — \
             it is gated on `any(debug_assertions, feature = \"logs-trace\")`, \
             so a `--release` run without `logs-trace` captures nothing."
        );
        for line in capture.contents().lines().take(20) {
            println!("H2 PP peer - captured: {line}");
        }
        return State::Fail;
    }

    if peer_slot_lines.is_empty() {
        println!(
            "H2 PP peer - {} {MUX_H2_TAG} line(s), none carrying a peer= slot",
            mux_lines.len()
        );
        return State::Fail;
    }

    // EVERY rendered slot, not merely one: the advertised address is cached on
    // the handler, so a line that renders anything else — the raw peer, or
    // `None` once the client has gone away — is the defect this guards.
    if advertised_lines != peer_slot_lines.len() {
        println!(
            "H2 PP peer - {advertised_lines} of {} {MUX_H2_TAG} peer= slots name the \
             PROXY-advertised client. Offending lines:",
            peer_slot_lines.len()
        );
        for line in peer_slot_lines
            .iter()
            .filter(|l| !l.contains(&advertised))
            .take(5)
        {
            println!("H2 PP peer - {line}");
        }
        return State::Fail;
    }

    if raw_peer_lines != 0 {
        println!(
            "H2 PP peer - {raw_peer_lines} {MUX_H2_TAG} line(s) name the \
             connection's raw TCP peer {raw_peer} instead of the \
             PROXY-advertised client. Offending lines:"
        );
        for line in peer_slot_lines.iter().filter(|l| l.contains(&raw)).take(5) {
            println!("H2 PP peer - {line}");
        }
        return State::Fail;
    }

    if stopped && got_response {
        State::Success
    } else {
        println!("H2 PP peer - stopped={stopped}, got_response={got_response}");
        State::Fail
    }
}

#[test]
fn test_h2_proxy_protocol_peer_is_the_advertised_client() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2: the MUX-H2 peer= slot names the PROXY-v2 advertised client",
            try_h2_proxy_protocol_peer_is_the_advertised_client
        ),
        State::Success
    );
}
