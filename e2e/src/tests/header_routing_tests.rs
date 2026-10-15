//! End-to-end coverage for header predicates on HTTP frontends
//! (sozu-proxy/sozu#1253).
//!
//! A canary by header: two clusters share one hostname and path, the canary
//! frontend carrying an `exact` header predicate. Over HTTP/1.1 and HTTP/2
//! (both over TLS, so the H2 stream reaches the same `route_from_request` as
//! the H1 request — `lib/src/protocol/mux/router.rs`), a request carrying the
//! header must reach the canary backend and every other request the stable
//! one. Removing the canary frontend — which must name the same predicate —
//! must leave the stable frontend serving every request.
//!
//! Unit coverage of the selection rules lives beside `select_tree_rule`
//! (`lib/src/router/mod.rs`); this test proves the request's headers really
//! reach the router on both protocols.
//!
//! ## Test list
//! 1. [`test_header_predicate_canary_routes_h1_and_h2`]
//! 2. [`test_header_predicate_silent_close_h1_and_h2`]

use std::{
    io::{ErrorKind, Read, Write},
    sync::Arc,
    time::{Duration, Instant},
};

use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, AddCertificate, CertificateAndKey, HeaderMatchKind, HeaderPredicate,
        ListenerType, RedirectPolicy, RequestHttpFrontend, ResponseStatus, SocketAddress,
        request::RequestType,
    },
};

use crate::{
    mock::{
        aggregator::SimpleAggregator,
        async_backend::BackendHandle as AsyncBackend,
        https_client::{
            HttpsClient, build_h2_client, build_https_client, resolve_prepared_request,
        },
    },
    sozu::worker::Worker,
    tests::{
        State,
        h2_utils::{
            H2_FRAME_HEADERS, H2Frame, collect_response_frames, contains_goaway,
            contains_rst_stream_with_error, h2_handshake, log_frames, raw_h2_connection,
            setup_h2_test, stream_status_matches,
        },
        provide_port, repeat_until_error_or,
        tests::create_local_address,
    },
};

const STABLE: &str = "stable";
const CANARY: &str = "canary";
const CANARY_HEADER: &str = "X-Canary";
const CANARY_VALUE: &str = "always";

fn canary_frontend(front_address: SocketAddress) -> RequestHttpFrontend {
    RequestHttpFrontend {
        header_predicates: vec![HeaderPredicate {
            name: CANARY_HEADER.to_owned(),
            kind: HeaderMatchKind::Exact as i32,
            value: Some(CANARY_VALUE.to_owned()),
        }],
        ..Worker::default_http_frontend(CANARY, front_address.into())
    }
}

/// The body of the backend that answered one GET, on a fresh client so
/// every request opens its own frontend connection and is routed on its own.
fn reached(
    build_client: fn() -> HttpsClient,
    front_port: u16,
    header: Option<(&str, &str)>,
) -> Option<String> {
    let client = build_client();
    let mut builder = hyper::Request::builder()
        .method("GET")
        .uri(format!("https://localhost:{front_port}/api"));
    if let Some((name, value)) = header {
        builder = builder.header(name, value);
    }
    let request = builder
        .body(String::new())
        .expect("the test request must build");
    match resolve_prepared_request(&client, request) {
        Some((status, body)) if status.is_success() => Some(body),
        other => {
            println!("HEADER-ROUTING: request failed: {other:?}");
            None
        }
    }
}

/// Every protocol and header case of one phase, `expect_canary` naming the
/// cases a header-carrying request must reach the canary in.
fn check_phase(front_port: u16, expect_canary: bool, phase: &str) -> bool {
    let canary_body = if expect_canary { CANARY } else { STABLE };
    let mut ok = true;
    for (protocol, build_client) in [
        ("h1", build_https_client as fn() -> HttpsClient),
        ("h2", build_h2_client as fn() -> HttpsClient),
    ] {
        for (header, expected) in [
            (Some((CANARY_HEADER, CANARY_VALUE)), canary_body),
            (Some(("x-canary", CANARY_VALUE)), canary_body),
            (Some((CANARY_HEADER, "never")), STABLE),
            (None, STABLE),
        ] {
            let body = reached(build_client, front_port, header);
            if body.as_deref() != Some(expected) {
                println!(
                    "HEADER-ROUTING {phase} {protocol} header={header:?}: expected {expected}, reached {body:?}"
                );
                ok = false;
            }
        }
    }
    ok
}

/// To SEE THIS RED: in `lib/src/protocol/mux/router.rs`, pass
/// `&crate::router::NoHeaders` instead of `&FrontRequestHeaders(front)` to
/// `frontend_from_request` in `route_from_request`; every header-carrying
/// request of the first phase then reaches the stable backend.
fn try_header_predicate_canary_routes_h1_and_h2() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let (config, listeners, state) = Worker::empty_https_config(front_address.clone().into());
    let mut worker = Worker::start_new_worker_owned("HEADER-ROUTING", config, listeners, state);

    worker.send_proxy_request_type(RequestType::AddHttpsListener(
        ListenerBuilder::new_https(front_address.clone())
            .to_tls(None)
            .expect("test https listener config must build"),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        interface: None,
        address: front_address.clone(),
        proxy: ListenerType::Https.into(),
        from_scm: false,
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

    let mut backends = Vec::new();
    for cluster in [STABLE, CANARY] {
        let address = create_local_address();
        worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(cluster)));
        worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
            cluster,
            format!("{cluster}-0"),
            address,
            None,
        )));
        backends.push(AsyncBackend::spawn_detached_backend(
            format!("BACKEND_{cluster}"),
            address,
            SimpleAggregator::default(),
            AsyncBackend::http_handler(cluster),
        ));
    }
    // The canary is declared FIRST: precedence must not depend on order.
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(canary_frontend(
        front_address.clone(),
    )));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(
        Worker::default_http_frontend(STABLE, front_address.clone().into()),
    ));
    worker.read_to_last();

    let routed_with_canary = check_phase(front_port, true, "with-canary");

    worker.send_proxy_request_type(RequestType::RemoveHttpsFrontend(canary_frontend(
        front_address,
    )));
    let removed = worker
        .read_proxy_response()
        .map(|response| response.status == ResponseStatus::Ok as i32)
        .unwrap_or(false);
    let routed_without_canary = check_phase(front_port, false, "without-canary");

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    for mut backend in backends {
        backend.stop_and_get_aggregator();
    }

    println!(
        "HEADER-ROUTING: routed_with_canary={routed_with_canary} removed={removed} routed_without_canary={routed_without_canary} stopped={stopped}"
    );
    if routed_with_canary && removed && routed_without_canary && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_header_predicate_canary_routes_h1_and_h2() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "router: a header-predicate canary serves exactly the requests carrying its header, over H1 and H2",
            try_header_predicate_canary_routes_h1_and_h2,
        ),
        State::Success,
    );
}

const DROP_HEADER: &str = "x-drop";
const DROP_VALUE: &str = "yes";

/// Budget for sozu to close a silently closed H1 connection; the close
/// follows the routing decision on the same event-loop pass.
const SILENT_CLOSE_DEADLINE_MS: u64 = 2000;

/// RFC 9113 §7 CANCEL.
const H2_ERROR_CANCEL: u32 = 0x8;

/// HPACK block for `GET https://localhost/` plus one optional literal
/// header (RFC 7541 §6.2.2, new name, neither side Huffman-coded).
fn h2_get_headers(header: Option<(&str, &str)>) -> Vec<u8> {
    let mut block = vec![0x82, 0x87, 0x84, 0x41, 0x09];
    block.extend_from_slice(b"localhost");
    if let Some((name, value)) = header {
        assert!(
            name.len() < 0x7f && value.len() < 0x7f,
            "single-octet lengths"
        );
        block.extend_from_slice(&[0x00, name.len() as u8]);
        block.extend_from_slice(name.as_bytes());
        block.push(value.len() as u8);
        block.extend_from_slice(value.as_bytes());
    }
    block
}

/// To SEE THIS RED: in `lib/src/protocol/mux/router.rs`, pass
/// `&crate::router::NoHeaders` instead of `&FrontRequestHeaders(front)` to
/// `frontend_from_request` in `route_from_request`; the predicated
/// silent-close frontend is then never selected, so the matching H1 request
/// is answered by the backend and the matching H2 stream is not reset.
fn try_header_predicate_silent_close_h1_and_h2() -> State {
    let (mut worker, mut backends, front_port) = setup_h2_test("HEADER-SILENT", 1);
    let front_address: std::net::SocketAddr = format!("127.0.0.1:{front_port}")
        .parse()
        .expect("front address must parse");
    // One configuration for every case below: the default `localhost`
    // frontend forwards, the predicated one silently refuses.
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        header_predicates: vec![HeaderPredicate {
            name: DROP_HEADER.to_owned(),
            kind: HeaderMatchKind::Exact as i32,
            value: Some(DROP_VALUE.to_owned()),
        }],
        redirect: Some(RedirectPolicy::SilentClose as i32),
        ..Worker::default_http_frontend("cluster_0", front_address)
    }));
    worker.read_to_last();

    // H1 over TLS, matching header: the connection ends with zero bytes.
    let mut tls_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(crate::mock::https_client::Verifier))
        .with_no_client_auth();
    tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let server_name = rustls::pki_types::ServerName::try_from("localhost")
        .expect("SNI must parse")
        .to_owned();
    let connection = rustls::ClientConnection::new(Arc::new(tls_config), server_name)
        .expect("client TLS state must build");
    let tcp = std::net::TcpStream::connect(front_address).expect("connect to the H1 frontend");
    tcp.set_read_timeout(Some(Duration::from_millis(100)))
        .expect("set read timeout");
    let mut tls = rustls::StreamOwned::new(connection, tcp);
    tls.write_all(
        format!(
            "GET / HTTP/1.1\r\nHost: localhost\r\n{DROP_HEADER}: {DROP_VALUE}\r\nConnection: keep-alive\r\n\r\n"
        )
        .as_bytes(),
    )
        .expect("send the matching H1 request");
    tls.flush().expect("flush the matching H1 request");
    let deadline = Instant::now() + Duration::from_millis(SILENT_CLOSE_DEADLINE_MS);
    let mut h1_received = Vec::new();
    let mut h1_closed = false;
    while Instant::now() < deadline {
        let mut buf = [0u8; 1024];
        match tls.read(&mut buf) {
            Ok(0) => {
                h1_closed = true;
                break;
            }
            Ok(n) => h1_received.extend_from_slice(&buf[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            // close_notify is optional; a reset or an EOF without it ends
            // the connection too. Nothing may have been written first.
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::UnexpectedEof
                        | ErrorKind::ConnectionReset
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::BrokenPipe
                ) =>
            {
                h1_closed = true;
                break;
            }
            Err(e) => {
                println!("HEADER-SILENT: unexpected H1 read error: {e}");
                break;
            }
        }
    }
    drop(tls);

    // H1, header present with another value: the normal backend answers.
    let h1_fallthrough = reached(build_https_client, front_port, Some((DROP_HEADER, "no")));

    // H2 on one connection: stream 1 matches and is reset; streams 3 and 5
    // do not match and are answered, so the connection survives.
    let mut h2 = raw_h2_connection(front_address);
    h2_handshake(&mut h2);
    for (stream_id, header) in [
        (1, Some((DROP_HEADER, DROP_VALUE))),
        (3, Some((DROP_HEADER, "no"))),
        (5, None),
    ] {
        h2.write_all(&H2Frame::headers(stream_id, h2_get_headers(header), true, true).encode())
            .expect("send the H2 request");
    }
    h2.flush().expect("flush the H2 requests");
    let frames = collect_response_frames(&mut h2, 200, 8, 250);
    log_frames("header silent close H2", &frames);
    let h2_reset_with_cancel = contains_rst_stream_with_error(&frames, 1, H2_ERROR_CANCEL);
    let h2_matching_answered = frames
        .iter()
        .any(|(ft, _, sid, _)| *ft == H2_FRAME_HEADERS && *sid == 1);
    let h2_others_answered =
        stream_status_matches(&frames, 3, 200) && stream_status_matches(&frames, 5, 200);
    let h2_goaway = contains_goaway(&frames);
    drop(h2);

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    // Only the three non-matching requests reach the backend.
    let requests_received = backends
        .iter_mut()
        .filter_map(|backend| backend.stop_and_get_aggregator())
        .map(|aggregator| aggregator.requests_received)
        .sum::<usize>();

    println!(
        "HEADER-SILENT: h1_closed={h1_closed} h1_received={h1_received:?} h1_fallthrough={h1_fallthrough:?} \
         h2_reset_with_cancel={h2_reset_with_cancel} h2_matching_answered={h2_matching_answered} \
         h2_others_answered={h2_others_answered} h2_goaway={h2_goaway} \
         requests_received={requests_received} stopped={stopped}"
    );
    if h1_closed
        && h1_received.is_empty()
        && h1_fallthrough.as_deref() == Some("pong0")
        && h2_reset_with_cancel
        && !h2_matching_answered
        && h2_others_answered
        && !h2_goaway
        && requests_received == 3
        && stopped
    {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_header_predicate_silent_close_h1_and_h2() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "router: a SILENT_CLOSE header-predicate frontend closes the matching H1 connection and resets the matching H2 stream only",
            try_header_predicate_silent_close_h1_and_h2,
        ),
        State::Success,
    );
}
