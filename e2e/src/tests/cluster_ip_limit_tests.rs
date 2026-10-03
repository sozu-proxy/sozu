//! End-to-end coverage for the per-(cluster, source-IP) connection limit.
//!
//! Layered exercises:
//! - HTTP/1.1: a global `max_connections_per_ip = 1` rejects the second
//!   concurrent connection from the same source IP with `429 Too Many
//!   Requests`. Confirms the answer-engine path (template registration,
//!   `Answer429` variant; the `connections.rejected_per_cluster_ip` metric
//!   is not asserted) and the SessionManager-side accounting (`untrack_all_cluster_ip`
//!   on close releases the slot for a follow-up request).
//! - HTTP/1.1 + per-cluster override: an unlimited cluster (`Some(0)`)
//!   coexists with a capped cluster (`Some(1)`). Two concurrent
//!   connections to the unlimited cluster MUST both succeed; the same
//!   pattern against the capped cluster MUST 429.
//! - HTTP/1.1 + `Retry-After: 0` semantics: when the resolved retry is
//!   `0` the response MUST omit the header (rendering `Retry-After: 0`
//!   would invite an immediate retry that defeats the limit).
//! - TCP: a TCP listener with `max_connections_per_ip = 1` accepts the
//!   first connection but closes the second one gracefully (FIN, no
//!   RST) without dialing the backend.
//! - HTTP/2: a failed dial answers its stream and keeps the connection,
//!   which holds one slot: a retry on it is admitted, a second
//!   connection from the same IP gets 429.
//!
//! Tests run in serial within this module to keep the per-IP slot
//! accounting deterministic per-test and to avoid port-allocator
//! contention with the heavier mux tests.

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    thread,
    time::{Duration, Instant},
};

use sozu_command_lib::{
    config::{FileConfig, ListenerBuilder},
    proto::command::{
        ActivateListener, AddCertificate, CertificateAndKey, Cluster, ListenerType, Request,
        RequestTcpFrontend, ServerConfig, SocketAddress, request::RequestType,
    },
};

use crate::{
    http_utils::http_ok_response,
    mock::{client::Client, sync_backend::Backend as SyncBackend},
    port_registry::{attach_reserved_http_listener, attach_reserved_tcp_listener},
    sozu::worker::Worker,
    tests::{State, provide_port, repeat_until_error_or, setup_sync_test},
};

use super::h2_utils::{
    H2_FLAG_END_STREAM, H2Frame, contains_goaway, h2_handshake, log_frames, parse_h2_frames,
    raw_h2_connection, read_all_available, stream_status_matches,
};

use super::tests::{create_local_address, create_unbound_local_address};

// ── Test fixtures ──────────────────────────────────────────────────────────

/// Build a [`ServerConfig`] like `Worker::empty_config` but with a global
/// `max_connections_per_ip` set to `limit`. `retry_after` is also overridden
/// so the rendered 429 response carries the expected header (or omits it
/// when set to `0`).
fn config_with_limit(limit: u64, retry_after: u32) -> ServerConfig {
    let mut server = Worker::into_config(FileConfig::default());
    server.max_connections_per_ip = Some(limit);
    server.retry_after = Some(retry_after);
    server
}

/// Open a parallel sync HTTP client. `Connection: keep-alive` is critical
/// for the per-(cluster, IP) limit test: each test client must hold its
/// frontend connection open while the next one tries to admit, otherwise
/// SessionManager sees the slot freed before we can race.
fn keep_alive_client(name: &'static str, front: SocketAddr, host: &str, path: &str) -> Client {
    Client::new(
        name,
        front,
        format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: keep-alive\r\n\r\n"),
    )
}

// ── Test 1: H1 global limit emits 429 with Retry-After ─────────────────────

fn try_h1_global_limit_emits_429() -> State {
    let front_address = create_local_address();
    let config = config_with_limit(1, 30);
    let mut listeners = sozu_command_lib::scm_socket::Listeners::default();
    attach_reserved_http_listener(&mut listeners, front_address);
    let state = sozu_command_lib::state::ConfigState::new();

    let (mut worker, mut backends) = setup_sync_test(
        "PER-IP-H1-LIMIT",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );
    let mut backend = backends.pop().unwrap();
    backend.connect();

    // First client: occupies the single slot.
    let mut client_a = keep_alive_client("clientA", front_address, "localhost", "/a");
    client_a.connect();
    client_a.send();
    backend.accept(0);
    backend.receive(0);
    backend.send(0);
    let resp_a = match client_a.receive() {
        Some(r) => r,
        None => return State::Fail,
    };
    if !resp_a.contains("200") {
        return State::Fail;
    }

    // Second client (same source IP, same cluster): MUST 429 without
    // hitting the backend. The keep-alive on `client_a` keeps the slot
    // taken in SessionManager.
    let mut client_b = keep_alive_client("clientB", front_address, "localhost", "/b");
    client_b.connect();
    client_b.send();
    let resp_b = match client_b.receive() {
        Some(r) => r,
        None => return State::Fail,
    };
    if !resp_b.contains("429") {
        return State::Fail;
    }
    if !resp_b.contains("Retry-After: 30") {
        return State::Fail;
    }

    // After client_a goes away, the slot must free and client_c admit.
    client_a.disconnect();
    thread::sleep(Duration::from_millis(150));

    let mut client_c = keep_alive_client("clientC", front_address, "localhost", "/c");
    client_c.connect();
    client_c.send();
    backend.accept(1);
    backend.receive(1);
    backend.send(1);
    let resp_c = match client_c.receive() {
        Some(r) => r,
        None => return State::Fail,
    };
    if !resp_c.contains("200") {
        return State::Fail;
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_h1_global_limit_emits_429() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H1 per-(cluster, IP) limit returns 429 with Retry-After header",
            try_h1_global_limit_emits_429,
        ),
        State::Success,
    );
}

// ── Test 2: H1 Retry-After: 0 omits the header ─────────────────────────────

fn try_h1_retry_after_zero_elides_header() -> State {
    let front_address = create_local_address();
    let config = config_with_limit(1, 0);
    let mut listeners = sozu_command_lib::scm_socket::Listeners::default();
    attach_reserved_http_listener(&mut listeners, front_address);
    let state = sozu_command_lib::state::ConfigState::new();

    let (mut worker, mut backends) = setup_sync_test(
        "PER-IP-RETRY-ZERO",
        config,
        listeners,
        state,
        front_address,
        1,
        false,
    );
    let mut backend = backends.pop().unwrap();
    backend.connect();

    let mut client_a = keep_alive_client("clientA", front_address, "localhost", "/a");
    client_a.connect();
    client_a.send();
    backend.accept(0);
    backend.receive(0);
    backend.send(0);
    if !matches!(client_a.receive(), Some(ref r) if r.contains("200")) {
        return State::Fail;
    }

    let mut client_b = keep_alive_client("clientB", front_address, "localhost", "/b");
    client_b.connect();
    client_b.send();
    let resp_b = match client_b.receive() {
        Some(r) => r,
        None => return State::Fail,
    };
    if !resp_b.contains("429") {
        return State::Fail;
    }
    // `Retry-After: 0` would invite an immediate retry; the template
    // engine MUST elide the header line entirely when the resolved
    // retry value is 0 / absent.
    if resp_b.to_ascii_lowercase().contains("retry-after") {
        return State::Fail;
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_h1_retry_after_zero_elides_header() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H1 per-(cluster, IP) 429: Retry-After: 0 elides the header",
            try_h1_retry_after_zero_elides_header,
        ),
        State::Success,
    );
}

// ── Test 3: per-cluster override coexistence ───────────────────────────────

fn try_h1_per_cluster_override() -> State {
    use sozu_command_lib::proto::command::PathRule;

    let front_address = create_local_address();
    let config = config_with_limit(0, 0); // global default = unlimited
    let mut listeners = sozu_command_lib::scm_socket::Listeners::default();
    attach_reserved_http_listener(&mut listeners, front_address);
    let state = sozu_command_lib::state::ConfigState::new();

    let mut worker = Worker::start_new_worker_owned("PER-IP-OVERRIDE", config, listeners, state);

    worker.send_proxy_request(Request {
        request_type: Some(RequestType::AddHttpListener(
            ListenerBuilder::new_http(front_address.into())
                .to_http(None)
                .unwrap(),
        )),
    });
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::ActivateListener(ActivateListener {
            interface: None,
            address: front_address.into(),
            proxy: ListenerType::Http.into(),
            from_scm: false,
        })),
    });

    // Cluster A: explicit override "unlimited" — the LIMIT cluster's cap
    // must NOT bleed into A.
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::AddCluster(Cluster {
            cluster_id: "cluster_unlimited".to_owned(),
            max_connections_per_ip: Some(0),
            ..Default::default()
        })),
    });
    let mut frontend_a = Worker::default_http_frontend("cluster_unlimited", front_address);
    frontend_a.hostname = "unlimited.local".to_owned();
    frontend_a.path = PathRule::prefix(String::from("/"));
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::AddHttpFrontend(frontend_a)),
    });

    // Cluster B: capped at 1 — must reject the second concurrent
    // connection from this IP.
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::AddCluster(Cluster {
            cluster_id: "cluster_capped".to_owned(),
            max_connections_per_ip: Some(1),
            retry_after: Some(7),
            ..Default::default()
        })),
    });
    let mut frontend_b = Worker::default_http_frontend("cluster_capped", front_address);
    frontend_b.hostname = "capped.local".to_owned();
    frontend_b.path = PathRule::prefix(String::from("/"));
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::AddHttpFrontend(frontend_b)),
    });

    let back_unlimited = create_local_address();
    let back_capped = create_local_address();
    worker.send_proxy_request(
        RequestType::AddBackend(Worker::default_backend(
            "cluster_unlimited",
            "cluster_unlimited-0".to_owned(),
            back_unlimited,
            None,
        ))
        .into(),
    );
    worker.send_proxy_request(
        RequestType::AddBackend(Worker::default_backend(
            "cluster_capped",
            "cluster_capped-0".to_owned(),
            back_capped,
            None,
        ))
        .into(),
    );
    worker.read_to_last();

    let mut backend_unlimited =
        SyncBackend::new("BACK-U", back_unlimited, http_ok_response("u".to_owned()));
    let mut backend_capped =
        SyncBackend::new("BACK-C", back_capped, http_ok_response("c".to_owned()));
    backend_unlimited.connect();
    backend_capped.connect();

    // Two concurrent connections to the unlimited cluster: both must
    // pass through to the backend.
    let mut u1 = keep_alive_client("u1", front_address, "unlimited.local", "/1");
    u1.connect();
    u1.send();
    backend_unlimited.accept(0);
    backend_unlimited.receive(0);
    backend_unlimited.send(0);
    if !matches!(u1.receive(), Some(ref r) if r.contains("200")) {
        return State::Fail;
    }

    let mut u2 = keep_alive_client("u2", front_address, "unlimited.local", "/2");
    u2.connect();
    u2.send();
    backend_unlimited.accept(1);
    backend_unlimited.receive(1);
    backend_unlimited.send(1);
    if !matches!(u2.receive(), Some(ref r) if r.contains("200")) {
        return State::Fail;
    }

    // Same client load against the capped cluster: first admits,
    // second 429s with the cluster-specific retry value.
    let mut c1 = keep_alive_client("c1", front_address, "capped.local", "/1");
    c1.connect();
    c1.send();
    backend_capped.accept(0);
    backend_capped.receive(0);
    backend_capped.send(0);
    if !matches!(c1.receive(), Some(ref r) if r.contains("200")) {
        return State::Fail;
    }

    let mut c2 = keep_alive_client("c2", front_address, "capped.local", "/2");
    c2.connect();
    c2.send();
    let resp_c2 = match c2.receive() {
        Some(r) => r,
        None => return State::Fail,
    };
    if !resp_c2.contains("429") {
        return State::Fail;
    }
    if !resp_c2.contains("Retry-After: 7") {
        return State::Fail;
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_h1_per_cluster_override() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H1 per-cluster max_connections_per_ip override coexists with unlimited cluster",
            try_h1_per_cluster_override,
        ),
        State::Success,
    );
}

// ── Test 4: TCP graceful close on limit ────────────────────────────────────

fn try_tcp_graceful_close_on_limit() -> State {
    let front_address = create_local_address();
    let back_address = create_local_address();

    let config = config_with_limit(1, 0);
    let mut listeners = sozu_command_lib::scm_socket::Listeners::default();
    attach_reserved_tcp_listener(&mut listeners, front_address);
    let state = sozu_command_lib::state::ConfigState::new();

    let mut worker = Worker::start_new_worker_owned("PER-IP-TCP", config, listeners, state);
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::AddTcpListener(
            ListenerBuilder::new_tcp(front_address.into())
                .to_tcp(None)
                .unwrap(),
        )),
    });
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::ActivateListener(ActivateListener {
            interface: None,
            address: front_address.into(),
            proxy: ListenerType::Tcp.into(),
            from_scm: false,
        })),
    });
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::AddCluster(Cluster {
            cluster_id: "tcp_capped".to_owned(),
            max_connections_per_ip: Some(1),
            ..Default::default()
        })),
    });
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::AddTcpFrontend(RequestTcpFrontend {
            cluster_id: "tcp_capped".to_owned(),
            address: front_address.into(),
            ..Default::default()
        })),
    });
    worker.send_proxy_request(
        RequestType::AddBackend(Worker::default_backend(
            "tcp_capped",
            "tcp_capped-0".to_owned(),
            back_address,
            None,
        ))
        .into(),
    );
    worker.read_to_last();

    let mut backend = SyncBackend::new("TCP-BACK", back_address, "PONG\n".to_owned());
    backend.connect();

    // First TCP connection holds the slot.
    let mut tcp_a = TcpStream::connect(front_address).expect("connect 1");
    tcp_a
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    tcp_a.write_all(b"hello-a").unwrap();
    backend.accept(0);
    backend.receive(0);
    backend.send(0);
    let mut buf_a = [0u8; 16];
    let n_a = tcp_a.read(&mut buf_a).unwrap_or(0);
    if n_a == 0 || !buf_a[..n_a].starts_with(b"PONG") {
        return State::Fail;
    }

    // Second TCP connection from the same source IP: sozu must close
    // gracefully before the backend can see anything. The read returns
    // 0 bytes (EOF) without any payload.
    let mut tcp_b = TcpStream::connect(front_address).expect("connect 2");
    tcp_b
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    // Give sozu a tick to enforce + close.
    thread::sleep(Duration::from_millis(50));
    let mut buf_b = [0u8; 16];
    let n_b = tcp_b.read(&mut buf_b).unwrap_or(0);
    if n_b != 0 {
        // We expected EOF; receiving payload means enforcement leaked.
        return State::Fail;
    }

    worker.soft_stop();
    worker.wait_for_server_stop();
    State::Success
}

#[test]
fn test_tcp_graceful_close_on_limit() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "TCP per-(cluster, IP) limit closes second connection gracefully",
            try_tcp_graceful_close_on_limit,
        ),
        State::Success,
    );
}

// ── Test 5: H2 failed dial keeps the connection and one slot ───────────────

/// HPACK block for `GET /` on `localhost`.
fn h2_get_localhost() -> Vec<u8> {
    let mut block = vec![0x82, 0x84, 0x87, 0x41, 9];
    block.extend_from_slice(b"localhost");
    block
}

fn h2_stream_ended(frames: &[(u8, u8, u32, Vec<u8>)], stream_id: u32) -> bool {
    frames
        .iter()
        .any(|(_, flags, sid, _)| *sid == stream_id && flags & H2_FLAG_END_STREAM != 0)
}

/// Read frames until `stream_id` ends, a GOAWAY arrives, or 10 s pass: a
/// refused dial may be retried before it is answered.
fn h2_read_stream(tls: &mut impl Read, stream_id: u32) -> Vec<(u8, u8, u32, Vec<u8>)> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut bytes = Vec::new();
    loop {
        bytes.extend(read_all_available(tls, Duration::from_millis(100)));
        let frames = parse_h2_frames(&bytes);
        if h2_stream_ended(&frames, stream_id)
            || contains_goaway(&frames)
            || Instant::now() >= deadline
        {
            return frames;
        }
    }
}

/// On an H2 frontend, a failed dial is answered on its stream only
/// (sozu-proxy/sozu#1740): the connection stays open and keeps the one
/// slot it claimed for the cluster. A retry on the same connection is
/// admitted again — the slot is per connection, not per stream — while a
/// second connection from the same IP is refused with 429.
fn try_h2_failed_dial_keeps_connection_and_one_slot() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let (_, listeners, state) = Worker::empty_https_config(front_address.clone().into());
    let config = config_with_limit(1, 0);
    let mut worker = Worker::start_new_worker_owned("PER-IP-H2-DIAL", config, listeners, state);

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
    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "cluster_0",
    )));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(
        Worker::default_http_frontend("cluster_0", front_address.clone().into()),
    ));
    // Nothing listens there: every dial is refused.
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        create_unbound_local_address(),
        None,
    )));
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
    worker.read_to_last();

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut first = raw_h2_connection(front_addr);
    h2_handshake(&mut first);
    first
        .sock
        .set_read_timeout(Some(Duration::from_millis(100)))
        .expect("set read timeout");
    first
        .write_all(&H2Frame::headers(1, h2_get_localhost(), true, true).encode())
        .unwrap();
    first.flush().unwrap();
    let mut frames = h2_read_stream(&mut first, 1);

    // Retry on the same connection once the first answer is out.
    first
        .write_all(&H2Frame::headers(3, h2_get_localhost(), true, true).encode())
        .unwrap();
    first.flush().unwrap();
    frames.extend(h2_read_stream(&mut first, 3));

    // A second connection from the same IP while the first one is open.
    let mut second = raw_h2_connection(front_addr);
    h2_handshake(&mut second);
    second
        .sock
        .set_read_timeout(Some(Duration::from_millis(100)))
        .expect("set read timeout");
    second
        .write_all(&H2Frame::headers(1, h2_get_localhost(), true, true).encode())
        .unwrap();
    second.flush().unwrap();
    let second_frames = h2_read_stream(&mut second, 1);

    let first_answered = stream_status_matches(&frames, 1, 503) && h2_stream_ended(&frames, 1);
    let retry_admitted = stream_status_matches(&frames, 3, 503) && h2_stream_ended(&frames, 3);
    let no_goaway = !contains_goaway(&frames);
    let second_refused = stream_status_matches(&second_frames, 1, 429);

    drop(first);
    drop(second);
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    if stopped && first_answered && retry_admitted && no_goaway && second_refused {
        State::Success
    } else {
        println!(
            "H2 failed dial - stopped={stopped}, first_answered={first_answered}, \
             retry_admitted={retry_admitted}, no_goaway={no_goaway}, \
             second_refused={second_refused}"
        );
        log_frames("H2 failed dial, first connection", &frames);
        log_frames("H2 failed dial, second connection", &second_frames);
        State::Fail
    }
}

#[test]
fn test_h2_failed_dial_keeps_connection_and_one_slot() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2: a failed dial keeps the connection and holds one per-IP slot",
            try_h2_failed_dial_keeps_connection_and_one_slot
        ),
        State::Success,
    );
}
