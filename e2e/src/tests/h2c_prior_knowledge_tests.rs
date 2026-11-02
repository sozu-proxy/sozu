//! End-to-end coverage for cleartext HTTP/2 by prior knowledge on an HTTP
//! listener (`h2c_prior_knowledge`, sozu-proxy/sozu#1911).
//!
//! A client that knows the server speaks HTTP/2 opens the TCP connection with
//! the 24-byte preface (RFC 9113 §3.3). With the listener knob on, the session
//! peeks at the first bytes (`H2cPrefaceSniff`, `lib/src/protocol/h2c.rs`)
//! and builds an H2 frontend for the preface, an H1 frontend for anything
//! else. With the knob off — the default — nothing changes: the preface is a
//! malformed HTTP/1.x request and is answered 400.
//!
//! ## Test list
//! 1. [`test_h2c_prior_knowledge_to_h1_backend`] — knob on, an h2c client is
//!    served by an HTTP/1.1 backend.
//! 2. [`test_h2c_prior_knowledge_to_h2_backend`] — knob on, an h2c client is
//!    served by an HTTP/2 backend, which sees `X-Forwarded-Proto: http` and
//!    `Forwarded: proto=http`.
//! 3. [`test_h2c_prior_knowledge_keeps_http11`] — knob on, a plain HTTP/1.1
//!    request is still served as HTTP/1.1.
//! 4. [`test_h2c_prior_knowledge_split_preface`] — knob on, a preface split
//!    across two writes with a pause between them still gets the server
//!    SETTINGS: the sniff waits on a strict prefix instead of deciding.
//! 5. [`test_h2c_prior_knowledge_behind_proxy_protocol`] — knob on with
//!    `expect_proxy`: the preface behind a PROXY v2 header is served as h2c.
//! 6. [`test_h2c_preface_rejected_without_prior_knowledge`] — knob off (the
//!    default), the preface is answered `400 Bad Request`, as before the knob
//!    existed.

use std::{
    io::{ErrorKind, Read, Write},
    net::{SocketAddr, TcpStream},
    thread,
    time::{Duration, Instant},
};

use http_body_util::{BodyExt, Empty};
use hyper::{Request, StatusCode, body::Bytes};
use hyper_util::rt::{TokioExecutor, TokioIo};
use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, Cluster, ListenerType, RequestHttpFrontend, request::RequestType,
    },
};

use crate::{
    mock::{
        aggregator::SimpleAggregator, async_backend::BackendHandle as AsyncBackend,
        h2_backend::H2Backend,
    },
    sozu::worker::Worker,
    tests::{State, repeat_until_error_or},
};

use super::{
    h2_utils::{H2_FRAME_SETTINGS, H2Frame, parse_h2_frames, read_all_available},
    tests::create_local_address,
};

/// The HTTP/2 client connection preface (RFC 9113 §3.4).
const PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// Budget for one request/response exchange.
const EXCHANGE_BUDGET: Duration = Duration::from_secs(5);

/// The backend a test cluster routes to.
enum Backend {
    H1(AsyncBackend<SimpleAggregator>),
    H2(H2Backend),
}

impl Backend {
    fn stop(self) {
        match self {
            Backend::H1(mut backend) => {
                backend.stop_and_get_aggregator();
            }
            Backend::H2(mut backend) => backend.stop(),
        }
    }
}

/// An HTTP listener on a fresh port, one `localhost` frontend, and a cluster
/// whose single backend speaks HTTP/2 when `h2_backend`, HTTP/1.1 otherwise.
///
/// `h2c_prior_knowledge = None` leaves the key unset, which is how every
/// listener was built before the knob existed.
fn setup(
    name: &str,
    h2c_prior_knowledge: Option<bool>,
    expect_proxy: bool,
    h2_backend: bool,
) -> (Worker, Backend, SocketAddr) {
    let front_address = create_local_address();
    let (config, listeners, state) = Worker::empty_http_config(front_address);
    let mut worker = Worker::start_new_worker_owned(name, config, listeners, state);

    let mut builder = ListenerBuilder::new_http(front_address.into());
    if let Some(enabled) = h2c_prior_knowledge {
        builder.with_h2c_prior_knowledge(enabled);
    }
    let listener = builder
        .with_expect_proxy(expect_proxy)
        .to_http(None)
        .expect("valid HTTP listener");
    assert_eq!(listener.h2c_prior_knowledge, h2c_prior_knowledge);

    worker.send_proxy_request_type(RequestType::AddHttpListener(listener));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        interface: None,
        address: front_address.into(),
        proxy: ListenerType::Http.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(Cluster {
        http2: Some(h2_backend),
        ..Worker::default_cluster("cluster_0")
    }));
    worker.send_proxy_request_type(RequestType::AddHttpFrontend(RequestHttpFrontend {
        hostname: String::from("localhost"),
        ..Worker::default_http_frontend("cluster_0", front_address)
    }));

    let back_address = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));
    let backend = if h2_backend {
        Backend::H2(H2Backend::start(
            format!("{name}-H2-BACKEND"),
            back_address,
            "pong-h2",
        ))
    } else {
        Backend::H1(AsyncBackend::spawn_detached_backend(
            format!("{name}-H1-BACKEND"),
            back_address,
            SimpleAggregator::default(),
            AsyncBackend::http_handler("pong-h1"),
        ))
    };

    worker.read_to_last();
    (worker, backend, front_address)
}

fn teardown(mut worker: Worker, backend: Backend) -> bool {
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    backend.stop();
    stopped
}

/// One GET over HTTP/2 by prior knowledge on `stream`, after `prelude` (a
/// PROXY header, or nothing). The hyper HTTP/2 connection writes the preface
/// itself: no Upgrade, no ALPN.
fn h2c_get(
    front_address: SocketAddr,
    prelude: &[u8],
    path: &str,
) -> Result<(StatusCode, String), String> {
    let runtime = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    runtime.block_on(async {
        let exchange = async {
            let mut stream = tokio::net::TcpStream::connect(front_address)
                .await
                .map_err(|e| format!("connect: {e}"))?;
            if !prelude.is_empty() {
                tokio::io::AsyncWriteExt::write_all(&mut stream, prelude)
                    .await
                    .map_err(|e| format!("prelude: {e}"))?;
            }
            let (mut sender, connection) =
                hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(stream))
                    .await
                    .map_err(|e| format!("h2c handshake: {e}"))?;
            tokio::spawn(connection);

            let request = Request::builder()
                .uri(format!("http://localhost:{}{path}", front_address.port()))
                .body(Empty::<Bytes>::new())
                .map_err(|e| format!("request: {e}"))?;
            let response = sender
                .send_request(request)
                .await
                .map_err(|e| format!("send_request: {e}"))?;
            let status = response.status();
            let body = response
                .into_body()
                .collect()
                .await
                .map_err(|e| format!("body: {e}"))?
                .to_bytes();
            Ok((status, String::from_utf8_lossy(&body).into_owned()))
        };
        tokio::time::timeout(EXCHANGE_BUDGET, exchange)
            .await
            .map_err(|_| format!("h2c exchange timed out after {EXCHANGE_BUDGET:?}"))?
    })
}

/// Read until the peer closes, `done` holds on what arrived, or `deadline`
/// elapses, across as many reads as the response takes.
fn read_until(stream: &mut TcpStream, deadline: Duration, done: impl Fn(&[u8]) -> bool) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .expect("set read timeout");
    let started = Instant::now();
    let mut received = Vec::new();
    let mut buf = [0u8; 4096];
    while started.elapsed() < deadline {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => received.extend_from_slice(&buf[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => break,
        }
        if done(&received) {
            break;
        }
    }
    received
}

fn try_h2c_prior_knowledge(name: &str, h2_backend: bool) -> State {
    let (worker, backend, front_address) = setup(name, Some(true), false, h2_backend);

    let result = h2c_get(front_address, &[], "/api");
    println!("{name}: h2c response {result:?}");
    let expected_body = if h2_backend { "pong-h2" } else { "pong-h1" };
    let served = matches!(&result, Ok((StatusCode::OK, body)) if body == expected_body);

    // The cleartext H2 frontend keeps the listener's scheme: the backend is
    // told the client spoke `http`, never `https`.
    let scheme_ok = match &backend {
        Backend::H2(h2) => {
            let recorded = h2.recorded_requests();
            println!("{name}: backend saw {recorded:?}");
            recorded.len() == 1
                && recorded[0]
                    .headers
                    .iter()
                    .any(|(name, value)| name == "x-forwarded-proto" && value.as_slice() == b"http")
                && recorded[0].headers.iter().any(|(name, value)| {
                    name == "forwarded"
                        && String::from_utf8_lossy(value)
                            .split(';')
                            .any(|pair| pair.trim() == "proto=http")
                })
        }
        Backend::H1(_) => true,
    };

    let stopped = teardown(worker, backend);
    if served && scheme_ok && stopped {
        State::Success
    } else {
        println!("{name}: served={served} scheme_ok={scheme_ok} stopped={stopped}");
        State::Fail
    }
}

#[test]
fn test_h2c_prior_knowledge_to_h1_backend() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "h2c prior knowledge: an h2c client is served by an HTTP/1.1 backend",
            || try_h2c_prior_knowledge("H2C-TO-H1", false),
        ),
        State::Success
    );
}

#[test]
fn test_h2c_prior_knowledge_to_h2_backend() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "h2c prior knowledge: an h2c client is served by an HTTP/2 backend over http",
            || try_h2c_prior_knowledge("H2C-TO-H2", true),
        ),
        State::Success
    );
}

fn try_h2c_prior_knowledge_keeps_http11() -> State {
    let (worker, backend, front_address) = setup("H2C-KEEPS-H1", Some(true), false, false);

    let mut client = TcpStream::connect(front_address).expect("connect to sozu");
    client
        .write_all(b"GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("send HTTP/1.1 request");
    // `Connection: close`: the response ends at end of stream.
    let response = read_until(&mut client, EXCHANGE_BUDGET, |_| false);
    let response = String::from_utf8_lossy(&response);
    println!("H2C-KEEPS-H1: response {response:?}");
    let served = response.starts_with("HTTP/1.1 200") && response.ends_with("pong-h1");

    let stopped = teardown(worker, backend);
    if served && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2c_prior_knowledge_keeps_http11() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "h2c prior knowledge: a plain HTTP/1.1 request is still served as HTTP/1.1",
            try_h2c_prior_knowledge_keeps_http11,
        ),
        State::Success
    );
}

fn try_h2c_prior_knowledge_split_preface() -> State {
    let (worker, backend, front_address) = setup("H2C-SPLIT", Some(true), false, false);

    let mut client = TcpStream::connect(front_address).expect("connect to sozu");
    client
        .set_write_timeout(Some(Duration::from_secs(2)))
        .expect("set write timeout");
    // A strict prefix that could still be HTTP/1.x or HTTP/2: the sniff must
    // wait for more, not decide.
    client.write_all(&PREFACE[..10]).expect("send first part");
    client.flush().expect("flush first part");
    thread::sleep(Duration::from_millis(200));
    let mut rest = PREFACE[10..].to_vec();
    rest.extend_from_slice(&H2Frame::settings(&[]).encode());
    client
        .write_all(&rest)
        .expect("send the rest of the preface");

    let data = read_all_available(&mut client, Duration::from_secs(2));
    let frames = parse_h2_frames(&data);
    println!("H2C-SPLIT: frames {frames:?}");
    let got_settings = frames.first().is_some_and(|(frame_type, _, stream_id, _)| {
        *frame_type == H2_FRAME_SETTINGS && *stream_id == 0
    });
    drop(client);

    let stopped = teardown(worker, backend);
    if got_settings && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2c_prior_knowledge_split_preface() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "h2c prior knowledge: a preface split across two writes is served as HTTP/2",
            try_h2c_prior_knowledge_split_preface,
        ),
        State::Success
    );
}

/// `PROXY` v2 over `AF_INET`, `127.0.0.1:54321 -> 127.0.0.1:dst_port`.
fn proxy_v2_ipv4(dst_port: u16) -> Vec<u8> {
    let mut header = vec![
        0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A, // signature
        0x21, // v2, PROXY
        0x11, // AF_INET, STREAM
        0x00, 0x0C, // 12 address bytes
        127, 0, 0, 1, 127, 0, 0, 1,
    ];
    header.extend_from_slice(&54321u16.to_be_bytes());
    header.extend_from_slice(&dst_port.to_be_bytes());
    header
}

fn try_h2c_prior_knowledge_behind_proxy_protocol() -> State {
    let (worker, backend, front_address) = setup("H2C-PROXY", Some(true), true, false);

    let result = h2c_get(front_address, &proxy_v2_ipv4(front_address.port()), "/api");
    println!("H2C-PROXY: h2c response {result:?}");
    let served = matches!(&result, Ok((StatusCode::OK, body)) if body == "pong-h1");

    let stopped = teardown(worker, backend);
    if served && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2c_prior_knowledge_behind_proxy_protocol() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "h2c prior knowledge: the preface behind a PROXY v2 header is served as h2c",
            try_h2c_prior_knowledge_behind_proxy_protocol,
        ),
        State::Success
    );
}

fn try_h2c_preface_rejected_without_prior_knowledge() -> State {
    // The key left unset: the listener every configuration built before the
    // knob existed.
    let (worker, backend, front_address) = setup("H2C-OFF", None, false, false);

    let mut client = TcpStream::connect(front_address).expect("connect to sozu");
    let mut wire = PREFACE.to_vec();
    wire.extend_from_slice(&H2Frame::settings(&[]).encode());
    client.write_all(&wire).expect("send the preface");
    // The status line is the assertion; stop at the end of the response
    // head rather than wait for the session to linger out.
    let response = read_until(&mut client, EXCHANGE_BUDGET, |received| {
        received.windows(4).any(|w| w == b"\r\n\r\n")
    });
    let response = String::from_utf8_lossy(&response);
    println!("H2C-OFF: response {response:?}");
    let rejected = response.starts_with("HTTP/1.1 400");

    let stopped = teardown(worker, backend);
    if rejected && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2c_preface_rejected_without_prior_knowledge() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "h2c prior knowledge off: the preface is still answered 400",
            try_h2c_preface_rejected_without_prior_knowledge,
        ),
        State::Success
    );
}
