//! End-to-end tests for the client affinity key of `HRW` on HTTP, HTTPS and
//! TCP clusters (sozu-proxy/sozu#524).
//!
//! Before #524 only the UDP datapath handed the consistent-hashing policies a
//! key; HTTP, HTTPS and TCP selected with none, so a cluster configured with
//! `HRW` or `MAGLEV` silently fell back to round-robin. These tests hold the
//! documented behaviour instead: one client key lands on one backend, request
//! after request, and different keys land where the policy puts them.
//!
//! # Predicted, not sampled
//!
//! Backend ports are allocated per run, and `HRW` hashes the backend address,
//! so which backend a key lands on changes between runs. A test asserting
//! "at least two backends were hit" would be a statistical fraction over that
//! varying input. Each test instead PREDICTS the backend of every key with
//! the library's own `Rendezvous` over the backends it registered, picks keys
//! whose predictions differ, and asserts every request reaches exactly its
//! predicted backend. Under the round-robin fallback this change removes,
//! [`REQUESTS_PER_KEY`] requests of one key visit that many different
//! backends, so the assertion fails on the first key.
//!
//! # Distinct client keys
//!
//! HTTP and HTTPS clients leave from distinct loopback addresses
//! (`127.0.0.N`, all local on Linux) or send distinct header/cookie values
//! from one address. TCP clients announce distinct sources in a PROXY v2
//! header, which is also the evidence that the key is the post-PROXY-protocol
//! source and not the socket peer.

use std::{
    cell::RefCell,
    io::{ErrorKind, Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, AddCertificate, CertificateAndKey, Cluster, ListenerType,
        LoadBalancingAlgorithms, LoadBalancingParams, ProxyProtocolConfig, SocketAddress,
        request::RequestType,
    },
};
use sozu_lib::{
    backends::Backend,
    load_balancing::{
        Candidates, LoadBalancingAlgorithm, Rendezvous, affinity_key_from_ip,
        affinity_key_from_value,
    },
};

use crate::{
    mock::{
        aggregator::SimpleAggregator,
        async_backend::BackendHandle as AsyncBackend,
        https_client::{
            build_h2_client, build_h2_client_from, build_https_client_from,
            resolve_prepared_request, resolve_prepared_requests_in_sequence,
        },
    },
    port_registry::bind_std_listener,
    sozu::worker::Worker,
    tests::{State, provide_port, repeat_until_error_or, tests::create_local_address},
};

/// Backends per cluster. Four, so the round-robin fallback visits four
/// different backends over [`REQUESTS_PER_KEY`] requests of one key.
const BACKENDS: usize = 4;
/// Requests sent per client key; each opens its own frontend connection and
/// so reaches backend selection.
const REQUESTS_PER_KEY: usize = 4;
const CLUSTER: &str = "cluster_0";
const AFFINITY_HEADER: &str = "X-Tenant";
const AFFINITY_COOKIE: &str = "tenant";

/// The backend `HRW` picks for `key` among `backends`, computed with the
/// library's own policy over the addresses and weights the worker holds.
fn predicted_backend(backends: &[SocketAddr], key: u64) -> usize {
    let list: Vec<Rc<RefCell<Backend>>> = backends
        .iter()
        .enumerate()
        .map(|(index, address)| {
            Rc::new(RefCell::new(Backend::new(
                &format!("{CLUSTER}-{index}"),
                *address,
                None,
                // `Worker::default_backend` registers exactly these parameters.
                Some(LoadBalancingParams::default()),
                None,
            )))
        })
        .collect();
    let indices: Vec<usize> = (0..list.len()).collect();
    let chosen = Rendezvous::new()
        .next_available_backend(Some(key), Candidates::new(&list, &indices, Instant::now()))
        .expect("HRW picks a backend out of a non-empty set");
    let address = chosen.borrow().address;
    backends
        .iter()
        .position(|candidate| *candidate == address)
        .expect("the predicted backend is one of the registered ones")
}

/// The first two candidates, in order, whose predicted backends differ: two
/// client keys `HRW` must send to two different backends.
fn two_keys_on_two_backends<T: Copy>(
    backends: &[SocketAddr],
    candidates: impl IntoIterator<Item = T>,
    key_of: impl Fn(T) -> u64,
) -> [(T, usize); 2] {
    let mut first: Option<(T, usize)> = None;
    for candidate in candidates {
        let backend = predicted_backend(backends, key_of(candidate));
        match first {
            None => first = Some((candidate, backend)),
            Some((_, first_backend)) if first_backend != backend => {
                return [first.expect("set above"), (candidate, backend)];
            }
            Some(_) => {}
        }
    }
    panic!("no two candidate keys map to different backends among {backends:?}");
}

/// A loopback source address `127.0.0.octet`.
fn loopback(octet: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(127, 0, 0, octet))
}

/// An `HRW` cluster keyed as `cluster` says, on its HTTP or HTTPS listener
/// at `front`, with [`BACKENDS`] backends answering `pong{index}`.
fn setup_http_worker(
    name: &str,
    https: bool,
    cluster: Cluster,
) -> (
    Worker,
    Vec<AsyncBackend<SimpleAggregator>>,
    Vec<SocketAddr>,
    u16,
) {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let (config, listeners, state) = if https {
        Worker::empty_https_config(front_address.clone().into())
    } else {
        Worker::empty_http_config(front_address.clone().into())
    };
    let mut worker = Worker::start_new_worker_owned(name, config, listeners, state);

    if https {
        worker.send_proxy_request_type(RequestType::AddHttpsListener(
            ListenerBuilder::new_https(front_address.clone())
                .to_tls(None)
                .expect("test https listener config must build"),
        ));
    } else {
        worker.send_proxy_request_type(RequestType::AddHttpListener(
            ListenerBuilder::new_http(front_address.clone())
                .to_http(None)
                .expect("test http listener config must build"),
        ));
    }
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        interface: None,
        address: front_address.clone(),
        proxy: if https {
            ListenerType::Https.into()
        } else {
            ListenerType::Http.into()
        },
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(cluster));
    let frontend = Worker::default_http_frontend(CLUSTER, front_address.clone().into());
    if https {
        worker.send_proxy_request_type(RequestType::AddHttpsFrontend(frontend));
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
    } else {
        worker.send_proxy_request_type(RequestType::AddHttpFrontend(frontend));
    }

    let mut addresses = Vec::with_capacity(BACKENDS);
    let mut backends = Vec::with_capacity(BACKENDS);
    for index in 0..BACKENDS {
        let address = create_local_address();
        worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
            CLUSTER,
            format!("{CLUSTER}-{index}"),
            address,
            None,
        )));
        backends.push(AsyncBackend::spawn_detached_backend(
            format!("BACKEND_{index}"),
            address,
            SimpleAggregator::default(),
            AsyncBackend::http_handler(format!("pong{index}")),
        ));
        addresses.push(address);
    }
    worker.read_to_last();
    (worker, backends, addresses, front_port)
}

/// An `HRW` cluster, keyed on the source IP unless `header`/`cookie` say
/// otherwise.
fn hrw_cluster(header: Option<&str>, cookie: Option<&str>) -> Cluster {
    Cluster {
        load_balancing: LoadBalancingAlgorithms::Hrw as i32,
        affinity_header: header.map(ToOwned::to_owned),
        affinity_cookie: cookie.map(ToOwned::to_owned),
        ..Worker::default_cluster(CLUSTER)
    }
}

/// Send [`REQUESTS_PER_KEY`] requests from `source`, each built by `request`,
/// and check every one reached backend `expected`. `label` names the key in
/// the failure output.
/// How a test client reaches the frontend.
#[derive(Clone, Copy)]
enum Transport {
    /// HTTP/1.1 over plaintext.
    Http,
    /// HTTP/1.1 over TLS.
    Https,
    /// HTTP/2 over TLS, negotiated by ALPN.
    H2,
}

fn every_request_reaches(
    transport: Transport,
    front_port: u16,
    source: IpAddr,
    extra: Option<(&str, &str)>,
    expected: usize,
    label: &str,
) -> bool {
    let scheme = match transport {
        Transport::Http => "http",
        Transport::Https | Transport::H2 => "https",
    };
    let mut reached = Vec::with_capacity(REQUESTS_PER_KEY);
    for _ in 0..REQUESTS_PER_KEY {
        // A client per request, so each request opens its own frontend
        // connection — its own session — and reaches backend selection
        // rather than a backend connection an earlier request left behind.
        let client = match transport {
            Transport::H2 => build_h2_client_from(source),
            Transport::Http | Transport::Https => build_https_client_from(source),
        };
        let mut builder = hyper::Request::builder()
            .method("GET")
            .uri(format!("{scheme}://localhost:{front_port}/affinity"));
        if let Some((name, value)) = extra {
            builder = builder.header(name, value);
        }
        let request = builder
            .body(String::new())
            .expect("the test request must build");
        match resolve_prepared_request(&client, request) {
            Some((status, body)) if status.is_success() => reached.push(body),
            other => {
                println!("{label}: request failed: {other:?}");
                return false;
            }
        }
    }
    let want = format!("pong{expected}");
    let all_on_expected = reached.iter().all(|body| *body == want);
    if !all_on_expected {
        println!("{label}: predicted {want} for every request, reached {reached:?}");
    }
    all_on_expected
}

fn stop(mut worker: Worker, backends: Vec<AsyncBackend<SimpleAggregator>>) -> bool {
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    for mut backend in backends {
        backend.stop_and_get_aggregator();
    }
    stopped
}

/// HTTP, source-IP key: each of two client addresses lands on its predicted
/// backend on every request, and the two predictions differ.
///
/// TO SEE THIS RED: pass `None` for the key in `RegistrySelector::select`
/// (`lib/src/protocol/mux/mod.rs`); `HRW` then round-robins and the first
/// address's four requests reach four backends.
fn try_http_source_ip_pins_each_client() -> State {
    let (worker, backends, addresses, front_port) =
        setup_http_worker("AFFINITY-HTTP-IP", false, hrw_cluster(None, None));
    let pair = two_keys_on_two_backends(&addresses, 2..=254u8, |octet| {
        affinity_key_from_ip(loopback(octet))
    });
    let pinned = pair.iter().all(|&(octet, backend)| {
        every_request_reaches(
            Transport::Http,
            front_port,
            loopback(octet),
            None,
            backend,
            &format!("source 127.0.0.{octet}"),
        )
    });
    let stopped = stop(worker, backends);
    if pinned && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_http_source_ip_pins_each_client() {
    assert_eq!(
        repeat_until_error_or(
            1,
            "HTTP HRW: each source IP stays on its predicted backend",
            try_http_source_ip_pins_each_client,
        ),
        State::Success
    );
}

/// HTTP, header key: two clients behind ONE source address split by the
/// value of the configured header, and a request without the header falls
/// back to the source-IP key.
///
/// TO SEE THIS RED: derive the key from the source IP alone in
/// `affinity_key` (`lib/src/protocol/mux/router.rs`); both header values then
/// land on the source address's backend.
fn try_http_header_splits_clients_behind_one_address() -> State {
    let (worker, backends, addresses, front_port) = setup_http_worker(
        "AFFINITY-HTTP-HEADER",
        false,
        hrw_cluster(Some(AFFINITY_HEADER), None),
    );
    let source = loopback(1);
    let values: Vec<String> = (0..256).map(|n| format!("tenant-{n}")).collect();
    let pair = two_keys_on_two_backends(&addresses, values.iter(), |value| {
        affinity_key_from_value(value.as_bytes())
    });
    let split = pair.iter().all(|&(value, backend)| {
        every_request_reaches(
            Transport::Http,
            front_port,
            source,
            Some((AFFINITY_HEADER, value)),
            backend,
            &format!("{AFFINITY_HEADER}: {value}"),
        )
    });
    let fallback = every_request_reaches(
        Transport::Http,
        front_port,
        source,
        None,
        predicted_backend(&addresses, affinity_key_from_ip(source)),
        "no header, source 127.0.0.1",
    );
    let stopped = stop(worker, backends);
    if split && fallback && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_http_header_splits_clients_behind_one_address() {
    assert_eq!(
        repeat_until_error_or(
            1,
            "HTTP HRW: a header value keys the client, the source IP is the fallback",
            try_http_header_splits_clients_behind_one_address,
        ),
        State::Success
    );
}

/// HTTPS, source-IP key and cookie key: over TLS, each client address lands
/// on its predicted backend, and two clients behind one address split by the
/// value of the configured cookie.
fn try_https_source_ip_and_cookie_pin_each_client() -> State {
    let (worker, backends, addresses, front_port) = setup_http_worker(
        "AFFINITY-HTTPS",
        true,
        hrw_cluster(None, Some(AFFINITY_COOKIE)),
    );
    // No cookie: the source address keys the request.
    let by_address = two_keys_on_two_backends(&addresses, 2..=254u8, |octet| {
        affinity_key_from_ip(loopback(octet))
    });
    let pinned = by_address.iter().all(|&(octet, backend)| {
        every_request_reaches(
            Transport::Https,
            front_port,
            loopback(octet),
            None,
            backend,
            &format!("source 127.0.0.{octet}"),
        )
    });
    let values: Vec<String> = (0..256).map(|n| format!("tenant-{n}")).collect();
    let by_cookie = two_keys_on_two_backends(&addresses, values.iter(), |value| {
        affinity_key_from_value(value.as_bytes())
    });
    let split = by_cookie.iter().all(|&(value, backend)| {
        let cookie = format!("theme=dark; {AFFINITY_COOKIE}={value}");
        every_request_reaches(
            Transport::Https,
            front_port,
            loopback(1),
            Some(("Cookie", &cookie)),
            backend,
            &format!("cookie {AFFINITY_COOKIE}={value}"),
        )
    });
    let stopped = stop(worker, backends);
    if pinned && split && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_https_source_ip_and_cookie_pin_each_client() {
    assert_eq!(
        repeat_until_error_or(
            1,
            "HTTPS HRW: source IP and cookie keys stay on their predicted backends",
            try_https_source_ip_and_cookie_pin_each_client,
        ),
        State::Success
    );
}

/// A TCP backend that answers every connection with `backend-{index}` and
/// closes it, until stopped.
struct TcpBackend {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl TcpBackend {
    fn start(index: usize, address: SocketAddr) -> Self {
        let listener = bind_std_listener(address, "affinity tcp backend");
        listener
            .set_nonblocking(true)
            .expect("the backend listener must go non-blocking");
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_nonblocking(false)
                            .expect("an accepted stream must go blocking");
                        stream
                            .set_read_timeout(Some(Duration::from_secs(3)))
                            .expect("set read timeout");
                        let mut buf = [0u8; 64];
                        // The proxied request; its content does not matter.
                        let _ = stream.read(&mut buf);
                        let _ = stream.write_all(format!("backend-{index}").as_bytes());
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("affinity tcp backend accept failed: {error}"),
                }
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for TcpBackend {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A PROXY v2 header announcing an IPv4 TCP connection from `source`.
fn proxy_v2_from(source: Ipv4Addr, destination_port: u16) -> Vec<u8> {
    let mut header = vec![
        0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A, // magic
        0x21, // version 2, command PROXY
        0x11, // AF_INET, STREAM
        0x00, 0x0C, // address length: 12
    ];
    header.extend_from_slice(&source.octets());
    header.extend_from_slice(&[127, 0, 0, 1]);
    header.extend_from_slice(&40_000u16.to_be_bytes());
    header.extend_from_slice(&destination_port.to_be_bytes());
    header
}

/// One TCP session announcing `source`, and the backend that answered it.
fn tcp_session_from(front: SocketAddr, source: Ipv4Addr) -> Option<String> {
    let mut stream = TcpStream::connect(front).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("set read timeout");
    stream
        .write_all(&proxy_v2_from(source, front.port()))
        .ok()?;
    stream.write_all(b"ping").ok()?;
    let mut answer = Vec::new();
    let mut buf = [0u8; 64];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => answer.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    String::from_utf8(answer)
        .ok()
        .filter(|answer| !answer.is_empty())
}

/// TCP, source-IP key after the PROXY protocol: each of two announced
/// sources lands on its predicted backend on every session. Every session
/// comes from the same socket peer, 127.0.0.1, so only the PROXY-v2 source
/// can tell them apart.
///
/// TO SEE THIS RED: pass `None` for the key to `backend_from_cluster_id` in
/// `TcpSession::connect_to_backend` (`lib/src/tcp.rs`); `HRW` then
/// round-robins and the first source's four sessions reach four backends.
fn try_tcp_proxy_protocol_source_pins_each_client() -> State {
    let front = create_local_address();
    let (config, listeners, state) = Worker::empty_tcp_config(front);
    let mut worker = Worker::start_new_worker_owned("AFFINITY-TCP", config, listeners, state);
    worker.send_proxy_request_type(RequestType::AddTcpListener(
        ListenerBuilder::new_tcp(front.into())
            .with_expect_proxy(true)
            .to_tcp(None)
            .expect("test tcp listener config must build"),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        interface: None,
        address: front.into(),
        proxy: ListenerType::Tcp.into(),
        from_scm: false,
    }));
    // `ExpectHeader` on the cluster is what makes a TCP session parse the
    // PROXY header instead of passing it through to the backend.
    worker.send_proxy_request_type(RequestType::AddCluster(Cluster {
        proxy_protocol: Some(ProxyProtocolConfig::ExpectHeader as i32),
        ..hrw_cluster(None, None)
    }));
    worker.send_proxy_request_type(RequestType::AddTcpFrontend(Worker::default_tcp_frontend(
        CLUSTER, front,
    )));
    let mut addresses = Vec::with_capacity(BACKENDS);
    let mut backends = Vec::with_capacity(BACKENDS);
    for index in 0..BACKENDS {
        let address = create_local_address();
        worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
            CLUSTER,
            format!("{CLUSTER}-{index}"),
            address,
            None,
        )));
        backends.push(TcpBackend::start(index, address));
        addresses.push(address);
    }
    worker.read_to_last();

    let source = |octet: u8| Ipv4Addr::new(10, 0, 0, octet);
    let pair = two_keys_on_two_backends(&addresses, 1..=254u8, |octet| {
        affinity_key_from_ip(IpAddr::V4(source(octet)))
    });
    let pinned = pair.iter().all(|&(octet, backend)| {
        let want = format!("backend-{backend}");
        let reached: Vec<Option<String>> = (0..REQUESTS_PER_KEY)
            .map(|_| tcp_session_from(front, source(octet)))
            .collect();
        let ok = reached
            .iter()
            .all(|answer| answer.as_deref() == Some(&*want));
        if !ok {
            println!(
                "source 10.0.0.{octet}: predicted {want} for every session, reached {reached:?}"
            );
        }
        ok
    });

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    drop(backends);
    if pinned && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_tcp_proxy_protocol_source_pins_each_client() {
    assert_eq!(
        repeat_until_error_or(
            1,
            "TCP HRW: each PROXY-v2 source stays on its predicted backend",
            try_tcp_proxy_protocol_source_pins_each_client,
        ),
        State::Success
    );
}

/// HTTP/2, header and cookie keys: two clients behind one address split by
/// the configured header over H2, and likewise by the configured cookie, whose
/// crumbs `pkawa` stores in the same cookie jar H1 parsing fills. The
/// frontend is HTTPS with `h2` negotiated by ALPN; every request is its own
/// H2 connection, hence its own session and its own backend dial.
fn try_h2_header_and_cookie_split_clients_behind_one_address() -> State {
    let mut ok = true;
    for (header, cookie) in [(Some(AFFINITY_HEADER), None), (None, Some(AFFINITY_COOKIE))] {
        let (worker, backends, addresses, front_port) =
            setup_http_worker("AFFINITY-H2", true, hrw_cluster(header, cookie));
        let values: Vec<String> = (0..256).map(|n| format!("tenant-{n}")).collect();
        let pair = two_keys_on_two_backends(&addresses, values.iter(), |value| {
            affinity_key_from_value(value.as_bytes())
        });
        let split = pair.iter().all(|&(value, backend)| {
            let (name, sent) = match header {
                Some(header) => (header, value.to_owned()),
                None => ("Cookie", format!("theme=dark; {AFFINITY_COOKIE}={value}")),
            };
            every_request_reaches(
                Transport::H2,
                front_port,
                loopback(1),
                Some((name, &sent)),
                backend,
                &format!("h2 {name}: {sent}"),
            )
        });
        let stopped = stop(worker, backends);
        ok &= split && stopped;
    }
    if ok { State::Success } else { State::Fail }
}

#[test]
fn test_h2_header_and_cookie_split_clients_behind_one_address() {
    assert_eq!(
        repeat_until_error_or(
            1,
            "H2 HRW: header and cookie values key clients behind one address",
            try_h2_header_and_cookie_split_clients_behind_one_address,
        ),
        State::Success
    );
}

/// Read one HTTP/1.1 response framed by `Content-Length` off `stream`, and
/// return its body.
fn read_one_response(stream: &mut TcpStream) -> Option<String> {
    let mut received = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        if let Some(end) = received.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&received[..end]).to_ascii_lowercase();
            let length: usize = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse().ok())?;
            let body_start = end + 4;
            if received.len() >= body_start + length {
                return String::from_utf8(received[body_start..body_start + length].to_vec()).ok();
            }
        }
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return None,
            Ok(n) => received.extend_from_slice(&buf[..n]),
        }
    }
}

/// Connection reuse keeps priority over the key (#524): the key is read only
/// when Sōzu dials a NEW backend connection. Two header values that `HRW`
/// sends to two different backends, sent one after the other on ONE H1
/// keep-alive client connection, both reach the backend the first one chose,
/// because the second request reuses that session's keep-alive backend
/// connection. This is the behaviour an upstream proxy or CDN multiplexing
/// tenants over warm connections gets: tenants follow the connection.
///
/// TO SEE THIS RED: in `Router::decide_after_gate`
/// (`lib/src/protocol/mux/router.rs`) never attach to a reusable connection
/// (`reuse_token.filter(|_| false)`); the second request then dials, and
/// reaches its own predicted backend. The H2 test below turns red the same
/// way.
fn try_h1_keep_alive_connection_keeps_its_backend_whatever_the_key() -> State {
    let (worker, backends, addresses, front_port) = setup_http_worker(
        "AFFINITY-H1-REUSE",
        false,
        hrw_cluster(Some(AFFINITY_HEADER), None),
    );
    let values: Vec<String> = (0..256).map(|n| format!("tenant-{n}")).collect();
    let [(first, first_backend), (second, second_backend)] =
        two_keys_on_two_backends(&addresses, values.iter(), |value| {
            affinity_key_from_value(value.as_bytes())
        });
    let mut stream =
        TcpStream::connect(("127.0.0.1", front_port)).expect("connect to the frontend");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    let mut reached = Vec::new();
    for value in [first, second] {
        let request = format!(
            "GET /affinity HTTP/1.1\r\nHost: localhost\r\n{AFFINITY_HEADER}: {value}\r\n\r\n"
        );
        stream
            .write_all(request.as_bytes())
            .expect("write a request");
        reached.push(read_one_response(&mut stream));
    }
    drop(stream);
    let want = Some(format!("pong{first_backend}"));
    let followed = reached.iter().all(|body| *body == want);
    if !followed {
        println!(
            "one keep-alive connection: predicted {want:?} for both {first} (backend {first_backend}) \
             and {second} (backend {second_backend}), reached {reached:?}"
        );
    }
    let stopped = stop(worker, backends);
    if followed && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h1_keep_alive_connection_keeps_its_backend_whatever_the_key() {
    assert_eq!(
        repeat_until_error_or(
            1,
            "H1 keep-alive: a reused backend connection wins over the key",
            try_h1_keep_alive_connection_keeps_its_backend_whatever_the_key,
        ),
        State::Success
    );
}

/// The same property over H2: two header values sent as two streams of ONE
/// H2 client connection share its session's backend connection, so both
/// reach the backend the first stream chose.
fn try_h2_connection_keeps_its_backend_whatever_the_key() -> State {
    let (worker, backends, addresses, front_port) = setup_http_worker(
        "AFFINITY-H2-REUSE",
        true,
        hrw_cluster(Some(AFFINITY_HEADER), None),
    );
    let values: Vec<String> = (0..256).map(|n| format!("tenant-{n}")).collect();
    let [(first, first_backend), (second, second_backend)] =
        two_keys_on_two_backends(&addresses, values.iter(), |value| {
            affinity_key_from_value(value.as_bytes())
        });
    let requests = [first, second]
        .into_iter()
        .map(|value| {
            hyper::Request::builder()
                .method("GET")
                .uri(format!("https://localhost:{front_port}/affinity"))
                .header(AFFINITY_HEADER, value.as_str())
                .body(String::new())
                .expect("the test request must build")
        })
        .collect();
    let reached: Vec<Option<String>> =
        resolve_prepared_requests_in_sequence(&build_h2_client(), requests)
            .into_iter()
            .map(|answer| answer.map(|(_, body)| body))
            .collect();
    let want = Some(format!("pong{first_backend}"));
    let followed = reached.iter().all(|body| *body == want);
    if !followed {
        println!(
            "one H2 connection: predicted {want:?} for both {first} (backend {first_backend}) \
             and {second} (backend {second_backend}), reached {reached:?}"
        );
    }
    let stopped = stop(worker, backends);
    if followed && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_connection_keeps_its_backend_whatever_the_key() {
    assert_eq!(
        repeat_until_error_or(
            1,
            "H2: a reused backend connection wins over the key",
            try_h2_connection_keeps_its_backend_whatever_the_key,
        ),
        State::Success
    );
}
