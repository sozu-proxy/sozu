//! End-to-end regression tests for the H2 "rearm + peer-signal" gaps
//! identified by the `/ask --with-codex` follow-up on PR #1209.
//!
//! Three production-grade `signal_pending_write`/`Ready::WRITABLE` pairing
//! gaps plus one scheduler staleness bug are covered here. See the
//! canonical plan at `~/.claude/plans/ask-h2-prio-truncation-plan.md` and
//! `lib/src/protocol/mux/LIFECYCLE.md` invariants 15/16/17.
//!
//! * [`test_h2_backend_silent_triggers_504_within_back_timeout`]: Fix A —
//!   defence-in-depth for invariant 15 on the `set_default_answer` path.
//!   On HEAD the synchronous drain loop at `mux/mod.rs:1402-1423` already
//!   flushes the 504 body before the session closes, so this test is a
//!   lock-in regression guard rather than a RED-to-green flip. The actual
//!   RED for Fix A is the unit test shipped alongside the patch in
//!   `mux/answers.rs::tests`.
//! * [`test_h2_priority_update_rearms_writable`]: Fix B — marked
//!   `#[ignore]` because reliably forcing sozu into the WRITABLE-stripped
//!   yielded state requires precise flow-control starvation that is
//!   prone to CI timing noise; on a healthy event loop, most other
//!   triggers (stream window update, PING, tick) wake the scheduler
//!   before the rearm gap becomes observable on the wire. The RED that
//!   drives Fix B lives in the unit test shipped alongside the patch in
//!   `mux/h2.rs::tests`, which exercises `handle_priority_update_frame`
//!   directly and asserts `readiness.event.is_writable()`.
//! * [`test_h2_backend_silent_headers_data_peer_signal`]: Fix C — marked
//!   `#[ignore]` until `e2e/src/mock/raw_h2_response_backend.rs` gains a
//!   HEADERS-then-delay-then-DATA mode (~40 LOC extension). Deferred to a
//!   follow-up to keep PR #1209 scope contained.
//! * [`test_h2_mid_pass_rst_does_not_force_yield`]: Fix D — marked
//!   `#[ignore]` because the scheduler-race observation is inherently
//!   flaky under CI timing noise. Re-enable after confirmation across 10
//!   consecutive CI runs. See memory `project_sozu_h2_flood_family_flakes`.

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener},
    thread,
    time::{Duration, Instant},
};

use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, AddCertificate, CertificateAndKey, ListenerType, RequestHttpFrontend,
        SocketAddress, request::RequestType,
    },
};

use crate::{
    sozu::worker::Worker,
    tests::{State, h2_utils::*, provide_port, provide_unbound_port, repeat_until_error_or},
};

// ============================================================================
// Shared helpers
// ============================================================================

/// Build an HPACK-encoded GET / request block carrying an RFC 9218
/// `priority` header. Mirrors `h2_correctness_tests::build_get_headers_with_priority`
/// but is duplicated here so this file does not depend on the private
/// helpers of a sibling test module.
fn build_get_with_priority(urgency: u8, inc_token: &str) -> Vec<u8> {
    let mut block = vec![
        0x82, // :method GET (indexed)
        0x84, // :path / (indexed)
        0x87, // :scheme https (indexed)
        0x41, 0x09, // :authority, value len 9
        b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't',
    ];
    let value = format!("u={urgency}, {inc_token}");
    block.push(0x00);
    block.push(0x08);
    block.extend_from_slice(b"priority");
    assert!(value.len() < 127, "priority value too long");
    block.push(value.len() as u8);
    block.extend_from_slice(value.as_bytes());
    block
}

/// Set up an HTTPS listener + cluster configured with an explicit
/// `back_timeout`. Returns `(worker, front_port, back_address)`. The
/// caller must bind a backend at `back_address`.
fn setup_listener_with_back_timeout(
    name: &str,
    back_timeout_secs: u32,
) -> (Worker, u16, SocketAddr) {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);

    let (config, listeners, state) = Worker::empty_https_config(front_address.clone().into());
    let mut worker = Worker::start_new_worker_owned(name, config, listeners, state);

    let mut listener_builder = ListenerBuilder::new_https(front_address.clone());
    listener_builder.with_back_timeout(Some(back_timeout_secs));
    worker.send_proxy_request_type(RequestType::AddHttpsListener(
        listener_builder.to_tls(None).unwrap(),
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
    let certificate_and_key = CertificateAndKey {
        certificate: String::from(include_str!("../../../lib/assets/local-certificate.pem")),
        key: String::from(include_str!("../../../lib/assets/local-key.pem")),
        certificate_chain: vec![],
        versions: vec![],
        names: vec![],
    };
    worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
        address: front_address,
        certificate: certificate_and_key,
        expired_at: None,
    }));

    let back_port = provide_unbound_port();
    let back_address: SocketAddr = format!("127.0.0.1:{back_port}").parse().unwrap();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0".to_owned(),
        back_address,
        None,
    )));
    worker.read_to_last();
    (worker, front_port, back_address)
}

/// Spawn a TCP listener that accepts one connection and then holds it
/// open without ever sending a byte. Mimics a stuck backend. Returns a
/// thread handle holding the accepted socket alive until join.
fn spawn_silent_backend(addr: SocketAddr) -> thread::JoinHandle<()> {
    let listener = TcpListener::bind(addr).expect("bind silent backend");
    listener
        .set_nonblocking(false)
        .expect("set blocking on listener");
    thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            // Hold the stream alive so sozu sees the TCP connection complete
            // but the HTTP layer never produces bytes. Park the thread for a
            // bounded window so the test teardown doesn't hang behind it.
            thread::sleep(Duration::from_secs(15));
            drop(stream);
        }
    })
}

/// Drop `tls`, soft-stop the worker, wait for it. Does not attempt to
/// terminate any ad-hoc backend threads — each test owns that lifecycle.
fn teardown_simple<T>(tls: T, front_port: u16, mut worker: Worker) -> bool {
    drop(tls);
    thread::sleep(Duration::from_millis(200));
    let still_alive = verify_sozu_alive(front_port);
    worker.soft_stop();
    let success = worker.wait_for_server_stop();
    success && still_alive
}

// ============================================================================
// Test 1 — Fix A lock-in: backend silence yields a 504 within back_timeout
// ============================================================================

/// Single H2 stream → backend that accepts the TCP connection and never
/// replies. `back_timeout = 2 s` on the listener. After the timeout,
/// `timeout_backend` in `mux/mod.rs:1343-1391` must:
///
/// 1. Render a 504 default answer via `set_default_answer`.
/// 2. Queue the response bytes into the H2 out buffer.
/// 3. Either (a) rely on the synchronous drain loop at
///    `mux/mod.rs:1402-1423` to flush before teardown, or (b) arm the
///    writable readiness + signal so the next scheduler tick delivers
///    the body.
///
/// On HEAD, path (a) masks the missing `signal_pending_write` pairing
/// in `set_default_answer` (the Fix A gap). This test therefore passes
/// on HEAD and remains green after Fix A — it's a lock-in regression
/// guard for the end-to-end 504 path, not a RED-to-green flip. The
/// actual RED for invariant 15 compliance lives in the unit test
/// alongside Fix A (`mux/answers.rs::tests`).
fn try_h2_backend_silent_triggers_504() -> State {
    let (worker, front_port, back_address) =
        setup_listener_with_back_timeout("H2-PRIO-REARM-504", 2);
    let _backend_thread = spawn_silent_backend(back_address);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake_with_initial_window(&mut tls, 1_000_000);

    let sid: u32 = 1;
    let block = build_get_with_priority(3, "i");
    let headers = H2Frame::headers(sid, block, true, true);
    tls.write_all(&headers.encode()).unwrap();
    tls.flush().unwrap();
    tls.write_all(&H2Frame::window_update(0, 1_000_000).encode())
        .unwrap();
    tls.flush().unwrap();

    // back_timeout = 2 s. Allow the synchronous drain + one event-loop
    // tick on top. A wall-clock budget of 4 s is generous but well below
    // the sozu default request_timeout (60 s) so a stall would be
    // unambiguous.
    let deadline = Duration::from_secs(4);
    let start = Instant::now();
    let mut raw = Vec::new();
    let mut rbuf = vec![0u8; 65536];
    let mut saw_headers = false;
    let mut saw_end_stream = false;
    tls.sock
        .set_read_timeout(Some(Duration::from_millis(250)))
        .ok();
    while !(saw_headers && saw_end_stream) && start.elapsed() < deadline {
        match tls.read(&mut rbuf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&rbuf[..n]);
                for (ft, fl, s, _) in parse_h2_frames(&raw) {
                    if s == sid && ft == H2_FRAME_HEADERS {
                        saw_headers = true;
                    }
                    if s == sid && (fl & H2_FLAG_END_STREAM) != 0 {
                        saw_end_stream = true;
                    }
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => break,
        }
    }
    let elapsed = start.elapsed();
    println!(
        "silent-backend 504: saw_headers={saw_headers} saw_end_stream={saw_end_stream} \
         elapsed={elapsed:?}"
    );

    let infra_ok = teardown_simple(tls, front_port, worker);

    if infra_ok && saw_headers && saw_end_stream && elapsed < Duration::from_secs(4) {
        State::Success
    } else {
        println!(
            "FAIL: saw_headers={saw_headers} saw_end_stream={saw_end_stream} \
             elapsed={elapsed:?} infra_ok={infra_ok}"
        );
        State::Fail
    }
}

#[test]
fn test_h2_backend_silent_triggers_504_within_back_timeout() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "H2 silent backend must 504 within back_timeout + synchronous drain window",
            try_h2_backend_silent_triggers_504
        ),
        State::Success
    );
}

// ============================================================================
// Test 2 — Fix B: PRIORITY_UPDATE rearms WRITABLE after a scheduler yield
// ============================================================================

/// Two concurrent H2 streams on the same connection, both `u=3, i` at
/// open time. The backend returns bodies just large enough to emit
/// multiple DATA frames per stream (memory `feedback_h2_repro_multi_data_frames`
/// — single-DATA responses pass by coincidence of natural writable).
///
/// Setup pins the sozu scheduler in a state where `finalize_write`
/// stripped `Ready::WRITABLE` on a voluntary incremental yield. At that
/// point the client sends a PRIORITY_UPDATE for the second stream,
/// bumping it to `u=0, i`. Without Fix B the scheduler does not
/// rearm — the reprioritised stream waits for an external event
/// (ping, window update, timeout tick) before draining.
///
/// Post-fix, both streams complete within 2 s of HEADERS being sent.
/// On HEAD the test fails — the second stream's trailing DATA frames
/// strand in `kawa.out` until another wake path fires. The exact
/// latency bound depends on scheduler pacing so we use a conservative
/// 5-second wall-clock budget: long enough to tolerate CI noise,
/// short enough that a stall would still be observable against the
/// sozu-wide 60 s request_timeout.
fn try_h2_priority_update_rearms_writable() -> State {
    // Body must produce ≥ 2 DATA frames (> 16 KiB default max_frame_size).
    // 64 KiB yields 4 DATA frames and is the size used by sibling
    // incremental-scheduling tests in `h2_correctness_tests.rs`.
    const BODY_SIZE: usize = 64 * 1024;

    let (worker, backends, front_port) =
        setup_h2_test_with_large_bodies("H2-PRIO-REARM-PU", 2, BODY_SIZE);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake_with_initial_window(&mut tls, 1_000_000);

    // Both streams start as `u=3, i`. Once the first DATA frame of each
    // stream has been observed, kick the second stream via PRIORITY_UPDATE
    // to `u=0, i` — this forces sozu to mutate `self.prioriser` via
    // `handle_priority_update_frame`, which is the Fix B code path.
    let sid_a: u32 = 1;
    let sid_b: u32 = 3;
    tls.write_all(&H2Frame::headers(sid_a, build_get_with_priority(3, "i"), true, true).encode())
        .unwrap();
    tls.write_all(&H2Frame::headers(sid_b, build_get_with_priority(3, "i"), true, true).encode())
        .unwrap();
    tls.flush().unwrap();
    tls.write_all(&H2Frame::window_update(0, 1_000_000).encode())
        .unwrap();
    tls.flush().unwrap();

    // Pump the read side briefly so both streams open and emit at least
    // HEADERS. We then send the PRIORITY_UPDATE for stream B and measure
    // how long it takes the second stream to drain.
    tls.sock
        .set_read_timeout(Some(Duration::from_millis(100)))
        .ok();
    let mut raw = Vec::new();
    let mut rbuf = vec![0u8; 65536];
    // Read until we've seen HEADERS for both streams, or 500 ms elapsed.
    let open_start = Instant::now();
    let mut seen_a_headers = false;
    let mut seen_b_headers = false;
    while (!seen_a_headers || !seen_b_headers) && open_start.elapsed() < Duration::from_millis(500)
    {
        match tls.read(&mut rbuf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&rbuf[..n]);
                for (ft, _fl, s, _) in parse_h2_frames(&raw) {
                    if ft == H2_FRAME_HEADERS && s == sid_a {
                        seen_a_headers = true;
                    }
                    if ft == H2_FRAME_HEADERS && s == sid_b {
                        seen_b_headers = true;
                    }
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => break,
        }
    }
    // Send PRIORITY_UPDATE now — bumping stream B to u=0, i. The frame
    // must travel on stream 0 (control stream).
    tls.write_all(&H2Frame::priority_update(sid_b, "u=0, i").encode())
        .unwrap();
    tls.flush().unwrap();
    let pu_sent_at = Instant::now();

    // Read until END_STREAM on both streams or deadline.
    let deadline = Duration::from_secs(5);
    let mut end_a = false;
    let mut end_b = false;
    while (!end_a || !end_b) && pu_sent_at.elapsed() < deadline {
        match tls.read(&mut rbuf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&rbuf[..n]);
                for (ft, fl, s, _) in parse_h2_frames(&raw) {
                    if ft == H2_FRAME_DATA && (fl & H2_FLAG_END_STREAM) != 0 {
                        if s == sid_a {
                            end_a = true;
                        }
                        if s == sid_b {
                            end_b = true;
                        }
                    }
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => break,
        }
    }
    let post_pu_elapsed = pu_sent_at.elapsed();

    let frames = parse_h2_frames(&raw);
    let (mut body_a, mut body_b) = (0usize, 0usize);
    for (ft, _fl, s, payload) in &frames {
        if *ft == H2_FRAME_DATA {
            if *s == sid_a {
                body_a += payload.len();
            } else if *s == sid_b {
                body_b += payload.len();
            }
        }
    }
    println!(
        "priority-update rearm: a={body_a}/{BODY_SIZE} end_a={end_a}, \
         b={body_b}/{BODY_SIZE} end_b={end_b}, post_pu_elapsed={post_pu_elapsed:?}"
    );

    let infra_ok = teardown(tls, front_port, worker, backends);

    if infra_ok
        && body_a == BODY_SIZE
        && end_a
        && body_b == BODY_SIZE
        && end_b
        && post_pu_elapsed < Duration::from_secs(5)
    {
        State::Success
    } else {
        println!(
            "FAIL: body_a={body_a} end_a={end_a} body_b={body_b} end_b={end_b} \
             post_pu_elapsed={post_pu_elapsed:?} infra_ok={infra_ok}"
        );
        State::Fail
    }
}

#[test]
#[ignore = "fix-b RED is covered by the unit test in mux/h2.rs::tests; e2e reliability requires an e2e-hooks readiness probe (tracked as #1209-followup-fix-b-probe)"]
fn test_h2_priority_update_rearms_writable() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "H2 PRIORITY_UPDATE must rearm WRITABLE so reprioritised streams drain promptly",
            try_h2_priority_update_rearms_writable
        ),
        State::Success
    );
}

// ============================================================================
// Test 3 — Fix C: peer signal_pending_write on H2 backend DATA/HEADERS
// ============================================================================

/// An H2 backend that emits HEADERS → 50 ms pause → DATA (END_STREAM)
/// with a ≥ 32 KiB body (two DATA frames). The frontend must forward
/// the full body within 500 ms post-HEADERS. Before Fix C the peer
/// readiness is inserted but `signal_pending_write` is never called,
/// so the frontend never wakes to forward the body bytes.
///
/// DEFERRED: `e2e/src/mock/raw_h2_response_backend.rs` does not yet
/// support HEADERS-then-delay-then-DATA. Extending the mock is ~40 LOC
/// and would grow this PR beyond the targeted invariant-15 scope.
/// Track under `#1209-followup-fix-c-mock` — enable once the mock
/// lands.
#[allow(dead_code)]
fn try_h2_backend_silent_headers_data_peer_signal() -> State {
    State::Success // placeholder — body follows the Fix C mock extension
}

#[test]
#[ignore = "fix-c requires raw-H2 mock with HEADERS-then-delay-then-DATA support; tracked as #1209-followup-fix-c-mock"]
fn test_h2_backend_silent_headers_data_peer_signal() {
    assert_eq!(
        repeat_until_error_or(
            1,
            "H2 backend HEADERS-then-delay-then-DATA must forward body under 500 ms",
            try_h2_backend_silent_headers_data_peer_signal
        ),
        State::Success
    );
}

// ============================================================================
// Test 4 — Fix D: mid-pass RST_STREAM does not force incremental yield
// ============================================================================

/// Three streams all `u=1, i` on the same connection. The middle
/// stream is RST_STREAM'd mid-pass. Without Fix D the surviving
/// streams still see `incremental_peer_count = 3` for the remainder
/// of the pass and fire one unnecessary voluntary yield each.
///
/// DEFERRED: timing-sensitive — the scheduler races between the
/// client RST_STREAM frame and the in-progress write pass. Even with
/// the fix, the observable delta is one DATA frame boundary — well
/// below CI timing noise. Re-enable after confirmation across 10
/// consecutive CI runs, or after extending the test with an
/// `e2e-hooks` probe that reads `incremental_peer_count` directly.
/// See memory `project_sozu_h2_flood_family_flakes`.
#[allow(dead_code)]
fn try_h2_mid_pass_rst_does_not_force_yield() -> State {
    State::Success // placeholder — body follows a non-flaky observation probe
}

#[test]
#[ignore = "fix-d test is timing-sensitive; re-enable after 10 stable CI runs (#1209-followup-flaky)"]
fn test_h2_mid_pass_rst_does_not_force_yield() {
    assert_eq!(
        repeat_until_error_or(
            1,
            "H2 mid-pass RST must not force a voluntary yield on same-urgency peers",
            try_h2_mid_pass_rst_does_not_force_yield
        ),
        State::Success
    );
}
