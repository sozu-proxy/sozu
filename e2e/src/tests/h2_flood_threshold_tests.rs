//! End-to-end tests for the negative space of the H2 flood detector: traffic
//! that ordinary clients produce must not trip it.
//!
//! The positive space — a real flood is answered with
//! `GOAWAY(ENHANCE_YOUR_CALM)` — lives in `h2_tests.rs`
//! (`test_h2_rapid_reset_triggers_goaway`, the PING/SETTINGS/empty-DATA and
//! CONTINUATION floods) and `h2_clock_tests.rs`. These tests pin the other
//! side, which those cannot: a detector that trips on legitimate traffic
//! passes every one of them.
//!
//! ## Test list
//! 1. [`test_h2_pre_response_cancels_keep_the_connection`] — sixty requests
//!    the client cancels before any response arrives (a single-page
//!    application navigating away, a browser dropping prefetches) leave the
//!    connection serving. The pre-response reset cap is relative to the
//!    streams the connection opened, with a floor of
//!    `h2_max_rst_stream_abusive_lifetime`; sixty is far below it.
//! 2. [`test_h2_per_frame_connection_window_updates_do_not_trip`] — a client
//!    that returns connection-level credit with one `WINDOW_UPDATE(0)` per
//!    DATA frame it receives (RFC 9113 §6.9 allows any cadence) downloads a
//!    16 MiB body to the end. A stream-0 `WINDOW_UPDATE` that answers DATA the
//!    proxy sent is credited, not counted toward
//!    `h2_max_window_update_stream0_per_window` — which the test sets to the
//!    former default of 100 so a per-frame client would cross it.
//! 3. [`test_h2_cancels_during_a_backend_outage_keep_the_connection`] — with
//!    the backend down, a client whose requests Sōzu answers 503 and which
//!    cancels one request in three before any answer keeps its connection
//!    past the pre-response floor: a stream routed to a cluster and answered
//!    by Sōzu counts as answered.

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

use super::h2_utils::{
    CHROME146_CONN_WINDOW_UPDATE_DELTA, CHROME146_INITIAL_WINDOW_SIZE, H2_ERROR_ENHANCE_YOUR_CALM,
    H2_FLAG_END_STREAM, H2_FRAME_DATA, H2_FRAME_GOAWAY, H2_FRAME_HEADERS, H2_FRAME_RST_STREAM,
    H2Frame, advance_one_frame, decode_status, h2_handshake, h2_handshake_with_initial_window,
    raw_h2_connection, setup_h2_listener_only, setup_h2_test, stream_status_matches, teardown,
};
use crate::{
    mock::{aggregator::SimpleAggregator, async_backend::BackendHandle as AsyncBackend},
    sozu::worker::Worker,
    tests::{
        State, provide_port, repeat_until_error_or,
        tests::{create_local_address, create_unbound_local_address},
    },
};
use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, AddCertificate, CertificateAndKey, Cluster, ListenerType,
        RequestHttpFrontend, SocketAddress, request::RequestType,
    },
};

type TlsStream = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;

/// `GET /` with all four pseudo-headers (RFC 9113 §8.3.1), `:authority`
/// `localhost` — the minimal block every raw H2 test in this crate sends.
fn get_root_header_block() -> Vec<u8> {
    vec![
        0x82, // :method GET (static index 2)
        0x84, // :path / (static index 4)
        0x86, // :scheme https (static index 6)
        0x41, 0x09, // :authority, literal value of length 9
        b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't',
    ]
}

/// Read frames into `carry` until `done` holds for the frames seen so far or
/// `budget` elapses, and return them all.
fn pump_frames<F>(
    tls: &mut TlsStream,
    carry: &mut Vec<u8>,
    budget: Duration,
    done: F,
) -> Vec<(u8, u8, u32, Vec<u8>)>
where
    F: Fn(&[(u8, u8, u32, Vec<u8>)]) -> bool,
{
    tls.sock
        .set_read_timeout(Some(Duration::from_millis(100)))
        .ok();
    let deadline = Instant::now() + budget;
    let mut frames = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    while Instant::now() < deadline && !done(&frames) {
        match tls.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => carry.extend_from_slice(&buf[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
        }
        while let Some(frame) = advance_one_frame(carry) {
            frames.push(frame);
        }
    }
    frames
}

fn goaway_with_calm(frames: &[(u8, u8, u32, Vec<u8>)]) -> bool {
    frames.iter().any(|(ft, _, _, payload)| {
        *ft == H2_FRAME_GOAWAY
            && payload.len() >= 8
            && u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]])
                == H2_ERROR_ENHANCE_YOUR_CALM
    })
}

// ── 1. pre-response cancels ─────────────────────────────────────────────

/// How many requests the client cancels before the response. Above the
/// former fixed lifetime cap of 50 pre-response resets, far below the
/// `h2_max_rst_stream_abusive_lifetime` floor of 1000 and below every
/// per-window rate.
const PRE_RESPONSE_CANCELS: u32 = 60;

fn try_h2_pre_response_cancels_keep_the_connection() -> State {
    let (worker, backends, front_port) = setup_h2_test("H2-PRE-RESPONSE-CANCELS", 1);
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);

    // HEADERS immediately followed by RST_STREAM(CANCEL) on the same stream,
    // in the same write: the reset reaches Sōzu before any backend answer,
    // which is what classifies it as pre-response. Spread over six writes so
    // no per-window counter is anywhere near its threshold — the test is
    // about the connection-lifetime accounting, not about a burst.
    let mut carry = Vec::new();
    let mut seen = Vec::new();
    for batch in 0..6 {
        let mut bytes = Vec::new();
        for i in 0..PRE_RESPONSE_CANCELS / 6 {
            let stream_id = 1 + 2 * (batch * (PRE_RESPONSE_CANCELS / 6) + i);
            bytes.extend_from_slice(
                &H2Frame::headers(stream_id, get_root_header_block(), true, true).encode(),
            );
            bytes.extend_from_slice(&H2Frame::rst_stream(stream_id, 0x8).encode());
        }
        if tls.write_all(&bytes).and_then(|_| tls.flush()).is_err() {
            println!("pre-response cancels: write failed at batch {batch}");
            break;
        }
        seen.extend(pump_frames(
            &mut tls,
            &mut carry,
            Duration::from_millis(200),
            |_| false,
        ));
    }

    // The connection must still serve a request after the cancels.
    let probe_id = 1 + 2 * PRE_RESPONSE_CANCELS;
    let probe = H2Frame::headers(probe_id, get_root_header_block(), true, true).encode();
    let probe_written = tls.write_all(&probe).and_then(|_| tls.flush()).is_ok();
    seen.extend(pump_frames(
        &mut tls,
        &mut carry,
        Duration::from_secs(5),
        |frames| {
            stream_status_matches(frames, probe_id, 200)
                || frames.iter().any(|(ft, _, _, _)| *ft == H2_FRAME_GOAWAY)
        },
    ));

    let calm = goaway_with_calm(&seen);
    let served = stream_status_matches(&seen, probe_id, 200);
    println!(
        "pre-response cancels: probe_written={probe_written} served={served} \
         goaway(ENHANCE_YOUR_CALM)={calm}"
    );
    let stopped = teardown(tls, front_port, worker, backends);
    if probe_written && served && !calm && stopped {
        State::Success
    } else {
        State::Fail
    }
}

/// Sixty requests cancelled before their response do not make Sōzu send
/// `GOAWAY(ENHANCE_YOUR_CALM)`, and the connection keeps serving.
///
/// TO SEE THIS RED: in `H2FloodDetector::record_rst_lifetime`
/// (`lib/src/protocol/mux/h2_flood_detector.rs`), drop the
/// streams-opened ratio from the pre-response test, and set
/// `DEFAULT_MAX_RST_STREAM_ABUSIVE_LIFETIME` back to 50: the 51st
/// cancel trips the cap and the probe never gets its 200.
#[test]
fn test_h2_pre_response_cancels_keep_the_connection() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "H2: sixty pre-response client cancels keep the connection serving",
            try_h2_pre_response_cancels_keep_the_connection,
        ),
        State::Success
    );
}

// ── 2. per-frame connection WINDOW_UPDATE ───────────────────────────────

/// Large enough that a client returning credit per DATA frame sends about a
/// thousand stream-0 WINDOW_UPDATEs — ten times the threshold the test
/// configures — whatever the host's throughput.
const DOWNLOAD_BODY: usize = 16 * 1024 * 1024;

/// An H1 backend answering any request with a `DOWNLOAD_BODY`-byte body.
/// It writes in blocking mode: the mock's default non-blocking socket would
/// stop at the first `WouldBlock`, a few MB in, and truncate the response.
fn large_body_backend(worker: &mut Worker) -> AsyncBackend<SimpleAggregator> {
    let back_address = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
        None,
    )));
    let body = "x".repeat(DOWNLOAD_BODY);
    let handler: Box<dyn Fn(&TcpStream, &str, SimpleAggregator) -> SimpleAggregator + Send + Sync> =
        Box::new(move |mut stream, backend_name, mut aggregator| {
            let mut buf = [0u8; 4096];
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => return aggregator,
                Ok(n) => println!("{backend_name} received {n}"),
            }
            aggregator.requests_received += 1;
            if stream.set_nonblocking(false).is_err() {
                return aggregator;
            }
            let header = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
            let sent = stream
                .write_all(header.as_bytes())
                .and_then(|_| stream.write_all(body.as_bytes()))
                .is_ok();
            stream.set_nonblocking(true).ok();
            if sent {
                aggregator.responses_sent += 1;
            }
            aggregator
        });
    AsyncBackend::spawn_detached_backend(
        "LARGE_BODY".to_owned(),
        back_address,
        SimpleAggregator::default(),
        handler,
    )
}

/// The stream-0 WINDOW_UPDATE threshold this test configures: the former
/// default. The credit DATA frames earn must keep a per-frame client clear
/// of it however low an operator sets it; at the current default (2000) a
/// slow host would not reach the threshold even without the credit, and the
/// test would prove nothing.
const TIGHT_WINDOW_UPDATE_STREAM0_PER_WINDOW: u32 = 100;

fn try_h2_per_frame_connection_window_updates_do_not_trip() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);
    let (config, listeners, state) = Worker::empty_https_config(front_address.clone().into());
    let mut worker = Worker::start_new_worker_owned("H2-PER-FRAME-WU0", config, listeners, state);
    let mut listener = ListenerBuilder::new_https(front_address.clone())
        .to_tls(None)
        .unwrap();
    listener.h2_max_window_update_stream0_per_window = Some(TIGHT_WINDOW_UPDATE_STREAM0_PER_WINDOW);
    worker.send_proxy_request_type(RequestType::AddHttpsListener(listener));
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
    let backends = vec![large_body_backend(&mut worker)];
    worker.read_to_last();
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    // Browser-sized windows (Chromium: 6 MiB per stream, 15 MiB for the
    // session), so the download is not paced by flow control on a loaded
    // host; the client still returns every frame's credit as it reads it.
    h2_handshake_with_initial_window(&mut tls, CHROME146_INITIAL_WINDOW_SIZE);

    let stream_id = 1;
    let mut request = H2Frame::window_update(0, CHROME146_CONN_WINDOW_UPDATE_DELTA).encode();
    request.extend_from_slice(
        &H2Frame::headers(stream_id, get_root_header_block(), true, true).encode(),
    );
    if tls.write_all(&request).and_then(|_| tls.flush()).is_err() {
        let _ = teardown(tls, front_port, worker, backends);
        return State::Fail;
    }

    tls.sock
        .set_read_timeout(Some(Duration::from_millis(100)))
        .ok();
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut carry = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut body = 0usize;
    let mut window_updates_sent = 0usize;
    let mut end_stream = false;
    let mut calm = false;
    let mut status_ok = false;
    let mut exit = String::from("deadline");
    let started = Instant::now();
    'read: while Instant::now() < deadline && !end_stream {
        match tls.read(&mut buf) {
            Ok(0) => {
                exit = "eof".to_owned();
                break;
            }
            Ok(n) => carry.extend_from_slice(&buf[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(e) => {
                exit = format!("read error {e:?}");
                break;
            }
        }
        while let Some((ft, flags, sid, payload)) = advance_one_frame(&mut carry) {
            if ft == H2_FRAME_HEADERS && sid == stream_id {
                status_ok |= stream_status_matches(&[(ft, flags, sid, payload.clone())], sid, 200);
            }
            if ft == H2_FRAME_GOAWAY {
                calm |= goaway_with_calm(&[(ft, flags, sid, payload)]);
                exit = "goaway".to_owned();
                break 'read;
            }
            if ft == H2_FRAME_RST_STREAM && sid == stream_id {
                exit = format!("RST_STREAM {payload:?}");
                break 'read;
            }
            if ft == H2_FRAME_DATA && sid == stream_id {
                body += payload.len();
                if flags & H2_FLAG_END_STREAM != 0 {
                    end_stream = true;
                    break;
                }
                if payload.is_empty() {
                    continue;
                }
                // Return exactly the credit this frame consumed, on the
                // connection and on the stream.
                let len = payload.len() as u32;
                let mut credit = H2Frame::window_update(0, len).encode();
                credit.extend_from_slice(&H2Frame::window_update(stream_id, len).encode());
                if let Err(e) = tls.write_all(&credit).and_then(|_| tls.flush()) {
                    exit = format!("write error {e:?}");
                    break 'read;
                }
                window_updates_sent += 1;
            }
        }
    }

    println!(
        "per-frame WU0: body={body}/{DOWNLOAD_BODY} end_stream={end_stream} status_ok={status_ok} \
         stream0_window_updates_sent={window_updates_sent} goaway(ENHANCE_YOUR_CALM)={calm} \
         exit={exit} elapsed={:?}",
        started.elapsed()
    );
    let stopped = teardown(tls, front_port, worker, backends);
    if status_ok && end_stream && body == DOWNLOAD_BODY && !calm && stopped {
        State::Success
    } else {
        State::Fail
    }
}

/// A client that sends one stream-0 `WINDOW_UPDATE` per DATA frame it
/// receives downloads a 16 MiB body without a `GOAWAY(ENHANCE_YOUR_CALM)`.
///
/// TO SEE THIS RED: in `H2FloodDetector::record_window_update_stream0`
/// (`lib/src/protocol/mux/h2_flood_detector.rs`), count every stream-0
/// `WINDOW_UPDATE` into the per-window counter instead of spending the
/// credit DATA frames earn. About a thousand updates then land in a few
/// seconds and the 101st within one window trips the configured threshold.
#[test]
fn test_h2_per_frame_connection_window_updates_do_not_trip() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "H2: per-DATA-frame stream-0 WINDOW_UPDATEs do not trip the flood detector",
            try_h2_per_frame_connection_window_updates_do_not_trip,
        ),
        State::Success
    );
}

// ── 3. cancels during a backend outage ──────────────────────────────────

/// Rounds of two answered requests and one cancelled before its answer:
/// past the `h2_max_rst_stream_abusive_lifetime` floor (1000) with a third
/// of the streams cancelled.
const OUTAGE_ROUNDS: u32 = 1100;

fn try_h2_cancels_during_a_backend_outage_keep_the_connection() -> State {
    let (mut worker, front_port, _) = setup_h2_listener_only("H2-OUTAGE-CANCELS");
    // The default 503 template carries `Connection: close`, which drains the
    // whole H2 connection after the first answer; a template without it keeps
    // the connection open for the rounds below.
    worker.send_proxy_request_type(RequestType::AddCluster(Cluster {
        answer_503: Some(String::from(
            "HTTP/1.1 503 Service Unavailable\r\nCache-Control: no-cache\r\nContent-Length: 0\r\n\r\n",
        )),
        ..Worker::default_cluster("cluster_0")
    }));
    // A backend nothing listens on: every routed request is answered 503.
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        create_unbound_local_address(),
        None,
    )));
    worker.read_to_last();
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);

    let mut carry = Vec::new();
    let mut calm = false;
    let mut answered = 0u32;
    let mut next_id: u32 = 1;
    for _ in 0..OUTAGE_ROUNDS {
        let (first, second, cancelled) = (next_id, next_id + 2, next_id + 4);
        next_id += 6;
        let mut wire = H2Frame::headers(first, get_root_header_block(), true, true).encode();
        wire.extend_from_slice(
            &H2Frame::headers(second, get_root_header_block(), true, true).encode(),
        );
        wire.extend_from_slice(
            &H2Frame::headers(cancelled, get_root_header_block(), true, true).encode(),
        );
        wire.extend_from_slice(&H2Frame::rst_stream(cancelled, 0x8).encode());
        if tls.write_all(&wire).and_then(|_| tls.flush()).is_err() {
            break;
        }
        let frames = pump_frames(&mut tls, &mut carry, Duration::from_secs(5), |frames| {
            (stream_status_matches(frames, first, 503)
                && stream_status_matches(frames, second, 503))
                || frames.iter().any(|(ft, _, _, _)| *ft == H2_FRAME_GOAWAY)
        });
        if goaway_with_calm(&frames) {
            calm = true;
            break;
        }
        if !(stream_status_matches(&frames, first, 503)
            && stream_status_matches(&frames, second, 503))
        {
            let statuses: Vec<(u32, Option<u16>)> = frames
                .iter()
                .filter(|(ft, _, _, _)| *ft == H2_FRAME_HEADERS)
                .map(|(_, _, sid, payload)| (*sid, decode_status(payload)))
                .collect();
            println!(
                "outage cancels: round starting at stream {first} got no 503 pair; \
                 {} frames, statuses {statuses:?}",
                frames.len()
            );
            break;
        }
        answered += 2;
    }

    println!(
        "outage cancels: answered={answered} cancelled={} goaway(ENHANCE_YOUR_CALM)={calm}",
        answered / 2
    );
    let stopped = teardown(tls, front_port, worker, Vec::new());
    if !calm && answered == 2 * OUTAGE_ROUNDS && stopped {
        State::Success
    } else {
        State::Fail
    }
}

/// During a backend outage, 503 answers count as answered streams, so
/// cancelling a third of the requests before their answer does not trip
/// the pre-response reset cap.
#[test]
fn test_h2_cancels_during_a_backend_outage_keep_the_connection() {
    assert_eq!(
        repeat_until_error_or(
            1,
            "H2: client cancels during a backend outage keep the connection",
            try_h2_cancels_during_a_backend_outage_keep_the_connection,
        ),
        State::Success
    );
}
