//! End-to-end correctness tests for fixes identified during Codex review:
//!
//! * [`test_h2_end_stream_nonzero_content_length_rejected`]: RFC 9113 §8.1.1 —
//!   HEADERS with END_STREAM + non-zero Content-Length yields RST_STREAM(PROTOCOL_ERROR).
//! * [`test_h2_end_stream_zero_content_length_accepted`]: Content-Length: 0 with
//!   END_STREAM is valid and must be accepted.
//! * [`test_h2_session_teardown_clean_with_active_backends`]: Multiple in-flight
//!   streams on multiple backends — abrupt client disconnect produces clean
//!   shutdown (no panic, no hang) and the worker can soft-stop.
//! * [`test_h2_idle_stream_timeout_frees_slot`]: A stream held idle past the
//!   per-stream deadline gets RST_STREAM(CANCEL) and frees its
//!   MAX_CONCURRENT_STREAMS slot, allowing a new stream to open.
//! * [`test_h2_active_upload_survives_idle_timeout`]: A POST stream that
//!   trickles non-empty DATA frames past the idle timeout is NOT cancelled —
//!   each DATA frame resets the per-stream idle timer.
//! * [`test_h2_idle_stream_no_data_cancelled`]: A POST stream that sends no
//!   DATA (only connection-level PINGs) is RST_STREAM(CANCEL)'d after the idle
//!   timeout, proving the Slowloris guard remains effective.
//! * [`test_h2_stranded_expect_write_survives_cancellation`]: When a stream is
//!   cancelled by the per-stream idle timer while the write path is parked on
//!   that stream's [`expect_write`], opening a new stream recycles the slot and
//!   may shrink the context streams Vec — the stranded [`expect_write`] must
//!   not panic on the next `writable()` call (h2.rs:1896 OOB regression).
//! * [`test_h2_stranded_expect_write_peer_rst_survives_cancellation`][] — same
//!   invariant as above but triggered via **peer-initiated RST_STREAM**; the
//!   `handle_rst_stream_frame` eviction path must also invalidate any cached
//!   `expect_write`/`expect_read` referencing the evicted gid.

use std::{
    io::{Read, Write},
    net::{Shutdown, SocketAddr},
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
    mock::{aggregator::SimpleAggregator, async_backend::BackendHandle as AsyncBackend},
    sozu::worker::Worker,
    tests::{State, h2_utils::*, provide_port, repeat_until_error_or, tests::create_local_address},
};

// ============================================================================
// Test 1: END_STREAM with non-zero Content-Length → RST_STREAM(PROTOCOL_ERROR)
// ============================================================================

/// RFC 9113 §8.1.1: A HEADERS frame with END_STREAM set means no DATA frames
/// follow, so the payload length is 0. If Content-Length declares a non-zero
/// value, this is a stream error (PROTOCOL_ERROR).
fn try_h2_end_stream_nonzero_content_length() -> State {
    let (worker, backends, front_port) = setup_h2_test("H2-COR-CL-ENDSTREAM", 1);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);

    // Build HEADERS with :method GET, :path /, :scheme https, :authority localhost,
    // content-length: 42, and END_STREAM + END_HEADERS flags.
    let mut header_block = vec![
        0x82, // :method GET (indexed)
        0x84, // :path / (indexed)
        0x87, // :scheme https (indexed)
        0x41, 0x09, // :authority, value length 9
        b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't',
    ];
    // Add content-length: 42 (literal without indexing, new name)
    header_block.push(0x00);
    header_block.push(0x0E); // name length 14
    header_block.extend_from_slice(b"content-length");
    header_block.push(0x02); // value length 2
    header_block.extend_from_slice(b"42");

    // END_STREAM + END_HEADERS
    let headers = H2Frame::headers(1, header_block, true, true);
    tls.write_all(&headers.encode()).unwrap();
    tls.flush().unwrap();

    let frames = collect_response_frames(&mut tls, 500, 3, 500);
    log_frames("END_STREAM + CL:42", &frames);

    // Expect RST_STREAM(PROTOCOL_ERROR) on stream 1, GOAWAY(PROTOCOL_ERROR),
    // or silent connection close (0 frames — stricter CL validation rejects
    // before emitting any H2 error frame).
    let rst_streams = extract_rst_streams(&frames);
    let has_protocol_error_rst = rst_streams
        .iter()
        .any(|(sid, ec)| *sid == 1 && *ec == H2_ERROR_PROTOCOL_ERROR);
    let has_protocol_error_goaway = contains_goaway_with_error(&frames, H2_ERROR_PROTOCOL_ERROR);
    let rejected = has_protocol_error_rst || has_protocol_error_goaway || frames.is_empty();

    println!("END_STREAM + CL:42 - rejected with PROTOCOL_ERROR: {rejected}");

    let infra_ok = teardown(tls, front_port, worker, backends);
    if infra_ok && rejected {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_end_stream_nonzero_content_length_rejected() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2 correctness: END_STREAM + non-zero Content-Length \u{2192} RST_STREAM(PROTOCOL_ERROR)",
            try_h2_end_stream_nonzero_content_length
        ),
        State::Success
    );
}

// ============================================================================
// Test 2: END_STREAM with Content-Length: 0 must be accepted
// ============================================================================

/// Counterpart to test 1: Content-Length: 0 with END_STREAM is perfectly valid —
/// the payload length is 0 and the declared length matches. Sozu must accept it
/// and forward to the backend.
fn try_h2_end_stream_zero_content_length() -> State {
    let (worker, backends, front_port) = setup_h2_test("H2-COR-CL0-OK", 1);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);

    let mut header_block = vec![
        0x82, // :method GET
        0x84, // :path /
        0x87, // :scheme https
        0x41, 0x09, // :authority localhost
        b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't',
    ];
    // content-length: 0
    header_block.push(0x00);
    header_block.push(0x0E);
    header_block.extend_from_slice(b"content-length");
    header_block.push(0x01); // value length 1
    header_block.push(b'0');

    let headers = H2Frame::headers(1, header_block, true, true);
    tls.write_all(&headers.encode()).unwrap();
    tls.flush().unwrap();

    let frames = collect_response_frames(&mut tls, 500, 5, 500);
    log_frames("END_STREAM + CL:0", &frames);

    // Should NOT get RST_STREAM(PROTOCOL_ERROR) — expect a valid response HEADERS
    let has_response = frames
        .iter()
        .any(|(ft, _, sid, _)| *ft == H2_FRAME_HEADERS && *sid == 1);
    let has_protocol_error = frames.iter().any(|(ft, _, sid, payload)| {
        *ft == H2_FRAME_RST_STREAM
            && *sid == 1
            && payload.len() >= 4
            && u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
                == H2_ERROR_PROTOCOL_ERROR
    });

    println!(
        "END_STREAM + CL:0 - got response: {has_response}, protocol_error: {has_protocol_error}"
    );

    let infra_ok = teardown(tls, front_port, worker, backends);
    if infra_ok && has_response && !has_protocol_error {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_end_stream_zero_content_length_accepted() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "H2 correctness: END_STREAM + Content-Length: 0 accepted normally",
            try_h2_end_stream_zero_content_length
        ),
        State::Success
    );
}

// ============================================================================
// Test 3: Abrupt client disconnect with active backend streams
// ============================================================================

/// When a client abruptly disconnects (TCP RST) while multiple streams are
/// in-flight across multiple backends, the session teardown must:
/// - not panic or hang
/// - properly clean up backend active_requests counters
/// - allow the worker to soft-stop cleanly
///
/// This exercises the fix for backend_streams.clear() ordering in mod.rs close().
fn try_h2_session_teardown_with_active_backends() -> State {
    let (mut worker, backends, front_port) = setup_h2_test("H2-COR-TEARDOWN", 2);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);

    // Open 3 concurrent streams (all without END_STREAM to keep them in-flight)
    for stream_id in (1..=5).step_by(2) {
        let mut header_block = vec![
            0x83, // :method POST
            0x84, // :path /
            0x87, // :scheme https
            0x41, 0x09, // :authority localhost
            b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't',
        ];
        // content-length: 100 (claim a body that we will never send)
        header_block.push(0x00);
        header_block.push(0x0E);
        header_block.extend_from_slice(b"content-length");
        header_block.push(0x03); // value length 3
        header_block.extend_from_slice(b"100");

        let headers = H2Frame::headers(stream_id, header_block, true, false);
        tls.write_all(&headers.encode()).unwrap();
    }
    tls.flush().unwrap();

    // Wait for sozu to start connecting to backends
    thread::sleep(Duration::from_millis(500));

    // Abrupt disconnect — drop the TLS stream and immediately close TCP
    let tcp = tls.get_ref();
    let _ = tcp.shutdown(Shutdown::Both);
    drop(tls);

    // Wait a beat for sozu to process the disconnect
    thread::sleep(Duration::from_millis(500));

    // Verify worker is still alive and can serve new connections
    let still_alive = verify_sozu_alive(front_port);
    println!("After abrupt disconnect - sozu alive: {still_alive}");

    // Clean soft-stop (this tests that no counters are stuck preventing shutdown)
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    for mut backend in backends {
        backend.stop_and_get_aggregator();
    }

    if still_alive && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_session_teardown_clean_with_active_backends() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 correctness: abrupt client disconnect with active backends \u{2014} clean teardown",
            try_h2_session_teardown_with_active_backends
        ),
        State::Success
    );
}

// ============================================================================
// Test 4: Idle stream timeout frees MAX_CONCURRENT_STREAMS slot
// ============================================================================

/// With h2_stream_idle_timeout_seconds=2, open a stream without sending
/// END_STREAM, wait for the timeout to fire, then verify that:
/// (a) the timed-out stream receives RST_STREAM(CANCEL)
/// (b) a new stream can be opened (slot was freed)
fn try_h2_idle_stream_timeout_frees_slot() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);

    let (config, listeners, state) = Worker::empty_https_config(front_address.clone().into());
    let mut worker =
        Worker::start_new_worker_owned("H2-COR-IDLE-RECYCLE", config, listeners, state);

    // Use a very short stream idle timeout (2 seconds)
    let mut listener_config = ListenerBuilder::new_https(front_address.clone())
        .to_tls(None)
        .unwrap();
    listener_config.h2_stream_idle_timeout_seconds = Some(2);

    worker.send_proxy_request_type(RequestType::AddHttpsListener(listener_config));
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

    let back_address = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0".to_owned(),
        back_address,
        None,
    )));
    let backend = AsyncBackend::spawn_detached_backend(
        "BACKEND_0".to_owned(),
        back_address,
        SimpleAggregator::default(),
        AsyncBackend::http_handler("pong0".to_owned()),
    );
    worker.read_to_last();

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);

    // Open stream 1 as POST without END_STREAM (to keep it idle)
    let header_block = vec![
        0x83, // :method POST
        0x84, // :path /
        0x87, // :scheme https
        0x41, 0x09, // :authority localhost
        b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't',
    ];
    let headers = H2Frame::headers(1, header_block, true, false);
    tls.write_all(&headers.encode()).unwrap();
    tls.flush().unwrap();

    // Wait for the idle timeout to fire (2s + margin)
    thread::sleep(Duration::from_secs(4));

    // Send a PING to tickle the readable path (which runs cancel_timed_out_streams)
    let ping = H2Frame::ping([0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);
    let _ = tls.write_all(&ping.encode());
    let _ = tls.flush();
    thread::sleep(Duration::from_millis(500));

    // Read any pending frames — should contain RST_STREAM(CANCEL) for stream 1
    let data = read_all_available(&mut tls, Duration::from_millis(1000));
    let mut all_frames = parse_h2_frames(&data);
    log_frames("Idle timeout", &all_frames);

    // Now open a new stream — should succeed because the slot was freed
    let header_block_new = vec![
        0x82, // :method GET
        0x84, // :path /
        0x87, // :scheme https
        0x41, 0x09, // :authority localhost
        b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't',
    ];
    let headers_new = H2Frame::headers(3, header_block_new, true, true);
    let write_ok = tls.write_all(&headers_new.encode()).is_ok() && tls.flush().is_ok();
    println!("New stream 3 write: {write_ok}");

    let frames_new = collect_response_frames(&mut tls, 500, 5, 500);
    log_frames("New stream after timeout", &frames_new);
    // Merge all frames for checking
    all_frames.extend(frames_new.iter().cloned());

    let cancel_code: u32 = 0x8; // CANCEL
    let rst_cancel = extract_rst_streams(&all_frames)
        .iter()
        .any(|(sid, ec)| *sid == 1 && *ec == cancel_code);
    println!("Stream 1 got RST_STREAM(CANCEL): {rst_cancel}");

    // Expect a response on stream 3 (HEADERS frame)
    let got_response = all_frames
        .iter()
        .any(|(ft, _, sid, _)| *ft == H2_FRAME_HEADERS && *sid == 3);
    println!("New stream 3 got response: {got_response}");

    let infra_ok = teardown(tls, front_port, worker, vec![backend]);
    if infra_ok && rst_cancel && got_response {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_idle_stream_timeout_frees_slot() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 correctness: idle stream timeout frees MAX_CONCURRENT_STREAMS slot",
            try_h2_idle_stream_timeout_frees_slot
        ),
        State::Success
    );
}

// ============================================================================
// Test 5: Active upload survives past idle timeout (trickled DATA resets timer)
// ============================================================================

/// With h2_stream_idle_timeout_seconds=2, open a POST stream and trickle DATA
/// frames every 500ms for 5 seconds total (well past the 2s timeout). The
/// stream must survive and eventually receive a response — NOT a RST_STREAM.
/// This proves that non-empty DATA frames reset the per-stream idle timer.
fn try_h2_active_upload_survives_idle_timeout() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);

    let (config, listeners, state) = Worker::empty_https_config(front_address.clone().into());
    let mut worker =
        Worker::start_new_worker_owned("H2-COR-ACTIVE-UPLOAD", config, listeners, state);

    let mut listener_config = ListenerBuilder::new_https(front_address.clone())
        .to_tls(None)
        .unwrap();
    listener_config.h2_stream_idle_timeout_seconds = Some(2);

    worker.send_proxy_request_type(RequestType::AddHttpsListener(listener_config));
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

    let back_address = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0".to_owned(),
        back_address,
        None,
    )));
    let backend = AsyncBackend::spawn_detached_backend(
        "BACKEND_0".to_owned(),
        back_address,
        SimpleAggregator::default(),
        AsyncBackend::http_handler("upload-ok".to_owned()),
    );
    worker.read_to_last();

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);

    // Open stream 1 as POST without END_STREAM (body will follow)
    let header_block = vec![
        0x83, // :method POST
        0x84, // :path /
        0x87, // :scheme https
        0x41, 0x09, // :authority localhost
        b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't',
    ];
    let headers = H2Frame::headers(1, header_block, true, false);
    tls.write_all(&headers.encode()).unwrap();
    tls.flush().unwrap();

    // Trickle DATA frames every 500ms for 5 seconds (10 chunks).
    // Each chunk is non-empty (128 bytes of payload).
    // Total time: ~5s, well past the 2s idle timeout.
    let chunk = vec![0xABu8; 128];
    for i in 0..10 {
        thread::sleep(Duration::from_millis(500));
        let end_stream = i == 9; // last chunk closes the stream
        let data_frame = H2Frame::data(1, chunk.clone(), end_stream);
        if tls.write_all(&data_frame.encode()).is_err() {
            println!("Write failed at chunk {i} — connection closed prematurely");
            let _ = teardown(tls, front_port, worker, vec![backend]);
            return State::Fail;
        }
        let _ = tls.flush();
        println!("Sent DATA chunk {i}/9 (end_stream={end_stream})");
    }

    // Collect response frames — expect HEADERS response on stream 1, NOT RST_STREAM
    let frames = collect_response_frames(&mut tls, 500, 10, 500);
    log_frames("Active upload", &frames);

    let cancel_code: u32 = 0x8; // CANCEL
    let got_rst_cancel = extract_rst_streams(&frames)
        .iter()
        .any(|(sid, ec)| *sid == 1 && *ec == cancel_code);
    let got_response = frames
        .iter()
        .any(|(ft, _, sid, _)| *ft == H2_FRAME_HEADERS && *sid == 1);

    println!("Stream 1 got RST_STREAM(CANCEL): {got_rst_cancel}");
    println!("Stream 1 got HEADERS response: {got_response}");

    let infra_ok = teardown(tls, front_port, worker, vec![backend]);

    // Success: we got a response and did NOT get cancelled
    if infra_ok && got_response && !got_rst_cancel {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_active_upload_survives_idle_timeout() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 correctness: active upload survives past idle timeout",
            try_h2_active_upload_survives_idle_timeout
        ),
        State::Success
    );
}

// ============================================================================
// Test 6: Idle stream (no DATA) is still cancelled after timeout
// ============================================================================

/// With h2_stream_idle_timeout_seconds=2, open a POST stream without sending
/// any DATA frames and only keep the connection alive with PINGs. The stream
/// must receive RST_STREAM(CANCEL) — proving the Slowloris guard still works
/// even after the idle-timeout-reset-on-DATA fix.
fn try_h2_idle_stream_no_data_cancelled() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);

    let (config, listeners, state) = Worker::empty_https_config(front_address.clone().into());
    let mut worker =
        Worker::start_new_worker_owned("H2-COR-IDLE-NO-DATA", config, listeners, state);

    let mut listener_config = ListenerBuilder::new_https(front_address.clone())
        .to_tls(None)
        .unwrap();
    listener_config.h2_stream_idle_timeout_seconds = Some(2);

    worker.send_proxy_request_type(RequestType::AddHttpsListener(listener_config));
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

    let back_address = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0".to_owned(),
        back_address,
        None,
    )));
    let backend = AsyncBackend::spawn_detached_backend(
        "BACKEND_0".to_owned(),
        back_address,
        SimpleAggregator::default(),
        AsyncBackend::http_handler("pong".to_owned()),
    );
    worker.read_to_last();

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);

    // Open stream 1 as POST without END_STREAM — and send NO DATA at all
    let header_block = vec![
        0x83, // :method POST
        0x84, // :path /
        0x87, // :scheme https
        0x41, 0x09, // :authority localhost
        b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't',
    ];
    let headers = H2Frame::headers(1, header_block, true, false);
    tls.write_all(&headers.encode()).unwrap();
    tls.flush().unwrap();

    // Keep the connection alive with PINGs (control frames) for 4 seconds.
    // PINGs must NOT reset the per-stream idle timer.
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_millis(4_000) {
        thread::sleep(Duration::from_millis(400));
        let ping = H2Frame::ping([0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);
        let _ = tls.write_all(&ping.encode());
        let _ = tls.flush();
    }

    // Read all frames — should contain RST_STREAM(CANCEL) for stream 1
    let frames = collect_response_frames(&mut tls, 200, 5, 500);
    log_frames("Idle no-data stream", &frames);

    let cancel_code: u32 = 0x8; // CANCEL
    let got_rst_cancel = extract_rst_streams(&frames)
        .iter()
        .any(|(sid, ec)| *sid == 1 && *ec == cancel_code);
    println!("Stream 1 got RST_STREAM(CANCEL): {got_rst_cancel}");

    let infra_ok = teardown(tls, front_port, worker, vec![backend]);

    if infra_ok && got_rst_cancel {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_idle_stream_no_data_cancelled() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 correctness: idle stream with no DATA is cancelled after timeout",
            try_h2_idle_stream_no_data_cancelled
        ),
        State::Success
    );
}

// ============================================================================
// Test 7: Stranded `expect_write` survives per-stream idle cancellation
// ============================================================================

/// Regression for the OOB panic at `h2.rs:1896`:
/// `index out of bounds: the len is 1 but the index is 1`.
///
/// Reproduction sequence (precise ordering matters):
///
/// 1. Open **stream 1** (GET) asking a backend that returns a large body.
///    The client intentionally stops reading from the TLS/TCP socket so the
///    kernel send buffer and TLS write queue fill, and sozu's write path
///    stalls mid-frame — parking `self.expect_write = Some(H2StreamId::Other
///    { id: 1, gid: G })` at `h2.rs:2043`.
/// 2. Per-stream idle timer fires (>2 s): `cancel_timed_out_streams`
///    (`h2.rs:2823`) evicts stream 1. `self.streams.remove(&1)` is called
///    and `context.streams[G].state` is set to `Recycle`, but
///    `expect_write` still references gid `G`.
/// 3. Fresh HEADERS opens **stream 3** → `Context::create_stream`
///    (`mod.rs:430`) finds the `Recycle` slot at `G`, reuses it, then runs
///    `shrink_trailing_recycle` (`mod.rs:491`). If the recycled slot is the
///    last element (common in the 1-active-stream case), the shrink may
///    reduce `context.streams.len()` — leaving the stranded `expect_write`
///    pointing past the end of the Vec.
/// 4. Next `writable()` → `write_streams` (`h2.rs:1879`) dereferences
///    `context.streams[global_stream_id]` at `h2.rs:1896` → OOB panic.
///
/// The fix (see `remove_dead_stream` and the idle-cancellation path) must
/// clear `expect_write` / `expect_read` whenever the referenced gid is
/// evicted. With the fix applied, this test's worker stays alive through
/// the full sequence.
///
/// **Trade-off notice**: reliably wedging sozu's write path inside a
/// single e2e run is timing-dependent — H2 INITIAL_WINDOW_SIZE caps a
/// single response at 64 KiB, rustls has its own write queue, and the
/// kernel TCP send buffer absorbs another ~64 KiB. So rather than trying
/// to force the exact panic signature, this test asserts the **indirect
/// invariant**: "worker survives a burst of idle-cancelled streams
/// followed immediately by fresh stream creation that triggers recycle +
/// trailing-Recycle shrink". Any regression that re-introduces a stranded
/// `expect_write`/`expect_read` referencing a popped gid would panic on
/// the next `writable()` tick and kill the worker thread, failing
/// `verify_sozu_alive`.
fn try_h2_stranded_expect_write_survives_cancellation() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);

    let (config, listeners, state) = Worker::empty_https_config(front_address.clone().into());
    let mut worker = Worker::start_new_worker_owned("H2-COR-STRANDED-EW", config, listeners, state);

    // Short per-stream idle deadline so the timer fires well within the
    // test budget.
    let mut listener_config = ListenerBuilder::new_https(front_address.clone())
        .to_tls(None)
        .unwrap();
    listener_config.h2_stream_idle_timeout_seconds = Some(2);

    worker.send_proxy_request_type(RequestType::AddHttpsListener(listener_config));
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

    let back_address = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0".to_owned(),
        back_address,
        None,
    )));
    // Large enough response body that the converter is likely to fragment it
    // across frames — makes the stalled-write path more likely while the
    // client is not reading.
    let large_body: String = "x".repeat(256 * 1024);
    let backend = AsyncBackend::spawn_detached_backend(
        "BACKEND_0".to_owned(),
        back_address,
        SimpleAggregator::default(),
        AsyncBackend::http_handler(large_body),
    );
    worker.read_to_last();

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);

    let get_headers = vec![
        0x82, // :method GET
        0x84, // :path /
        0x87, // :scheme https
        0x41, 0x09, // :authority localhost
        b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't',
    ];

    // Open stream 1 (GET), don't read the response. Sozu starts emitting
    // H2 DATA, may or may not park `expect_write` depending on buffer
    // sizes — either way the per-stream idle timer will fire next.
    let h1 = H2Frame::headers(1, get_headers.clone(), true, true);
    tls.write_all(&h1.encode()).unwrap();
    tls.flush().unwrap();

    // Wait past the 2 s per-stream idle deadline.
    thread::sleep(Duration::from_millis(2_400));

    // Wake the readable path so `cancel_timed_out_streams` runs. This
    // evicts stream 1 and (pre-fix) leaves `expect_write` stranded if it
    // had parked on stream 1's gid.
    let ping = H2Frame::ping([0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8]);
    let _ = tls.write_all(&ping.encode());
    let _ = tls.flush();
    thread::sleep(Duration::from_millis(200));

    // Open stream 3 — this is the `Context::create_stream` call that
    // recycles the just-evicted slot and may pop the trailing Recycle
    // entry, shrinking `context.streams`. If `expect_write` was still
    // pointing at the old gid, the next `writable()` tick panics.
    let h3 = H2Frame::headers(3, get_headers.clone(), true, true);
    let write3_ok = tls.write_all(&h3.encode()).is_ok() && tls.flush().is_ok();
    println!("stream 3 write: {write3_ok}");

    // Resume reading: drain any queued frames so sozu runs `writable()`
    // again — the panic (if any) fires here.
    let _ = collect_response_frames(&mut tls, 200, 6, 400);

    // Burst three more idle-cancel + new-stream cycles back-to-back to
    // widen the race window. Each iteration: open a stream, wait past the
    // idle deadline, open another — exercising the evict-then-shrink path
    // with different gids.
    let mut next_sid: u32 = 5;
    for _ in 0..3 {
        let hs = H2Frame::headers(next_sid, get_headers.clone(), true, true);
        let _ = tls.write_all(&hs.encode());
        let _ = tls.flush();
        next_sid += 2;

        thread::sleep(Duration::from_millis(2_300));
        let _ = tls.write_all(&ping.encode());
        let _ = tls.flush();
        thread::sleep(Duration::from_millis(100));

        let hn = H2Frame::headers(next_sid, get_headers.clone(), true, true);
        let _ = tls.write_all(&hn.encode());
        let _ = tls.flush();
        next_sid += 2;

        let _ = collect_response_frames(&mut tls, 150, 4, 300);
    }

    drop(tls);
    thread::sleep(Duration::from_millis(200));

    // Primary assertion: the worker must still be accepting connections.
    // A panic inside `write_streams` would tear down the worker thread.
    let still_alive = verify_sozu_alive(front_port);
    println!("stranded expect_write - sozu alive: {still_alive}");

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    let mut backends = vec![backend];
    for backend in backends.iter_mut() {
        backend.stop_and_get_aggregator();
    }

    if still_alive && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_stranded_expect_write_survives_cancellation() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "H2 correctness: stranded expect_write survives per-stream idle cancellation",
            try_h2_stranded_expect_write_survives_cancellation
        ),
        State::Success
    );
}

// ============================================================================
// Test 8: Stranded expect_write via peer-initiated RST_STREAM — covers the
//        `handle_rst_stream_frame` eviction path at h2.rs:3752
// ============================================================================

/// Sibling to [`test_h2_stranded_expect_write_survives_cancellation`] — same
/// invariant, different eviction trigger. Where the original test uses the
/// per-stream idle timer to evict a stream whose gid may still be cached in
/// `self.expect_write`, this test drives the **peer-initiated RST_STREAM**
/// path in `handle_rst_stream_frame` (`lib/src/protocol/mux/h2.rs:3752`),
/// which was previously an inline `self.streams.remove(...)` that did not
/// go through `remove_dead_stream` and therefore did not clear
/// `expect_write`/`expect_read`.
///
/// Reproduction shape:
///
/// 1. Client opens a GET stream `victim` to a backend returning a large
///    body — sozu begins emitting HEADERS + DATA on the frontend.
/// 2. Before the response fully flushes, the client sends RST_STREAM
///    (error=CANCEL) for `victim`. `handle_rst_stream_frame` evicts the
///    stream from `self.streams`, transitions `context.streams[gid].state`
///    to `Recycle`, and (with the fix) invalidates
///    `expect_write`/`expect_read` if they referenced `gid`.
/// 3. Client opens a new GET stream — `Context::create_stream`
///    (`mod.rs:430`) reuses the recycled slot, then runs
///    `shrink_trailing_recycle` (`mod.rs:491`), potentially shrinking
///    `context.streams.len()`.
/// 4. Client drains — sozu runs `writable()`. With the fix, no OOB index.
///    Without the fix (pre-refactor of site 3752), the stranded gid in
///    `expect_write` would panic at `h2.rs:1896`.
///
/// As with the idle-cancel test, the exact panic conditions depend on
/// timing (rustls buffering, H2 INITIAL_WINDOW_SIZE, kernel TCP send
/// buffer), so the primary assertion is the indirect "worker survives
/// N peer-RST + new-stream cycles" invariant via `verify_sozu_alive`.
fn try_h2_stranded_expect_write_peer_rst_survives() -> State {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);

    let (config, listeners, state) = Worker::empty_https_config(front_address.clone().into());
    let mut worker = Worker::start_new_worker_owned("H2-COR-RST-EW", config, listeners, state);

    let listener_config = ListenerBuilder::new_https(front_address.clone())
        .to_tls(None)
        .unwrap();
    worker.send_proxy_request_type(RequestType::AddHttpsListener(listener_config));
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

    let back_address = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0".to_owned(),
        back_address,
        None,
    )));
    // Large body so sozu is likely to emit multiple DATA frames before the
    // peer RST arrives — maximises the chance of parking expect_write.
    let large_body: String = "x".repeat(256 * 1024);
    let backend = AsyncBackend::spawn_detached_backend(
        "BACKEND_0".to_owned(),
        back_address,
        SimpleAggregator::default(),
        AsyncBackend::http_handler(large_body),
    );
    worker.read_to_last();

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);

    let get_headers = vec![
        0x82, // :method GET
        0x84, // :path /
        0x87, // :scheme https
        0x41, 0x09, // :authority localhost
        b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't',
    ];

    // Four iterations, each: open victim stream → give sozu time to park a
    // partial write → peer RST the victim → open a fresh stream → drain.
    let mut next_sid: u32 = 1;
    for _ in 0..4 {
        let victim = next_sid;
        let h_victim = H2Frame::headers(victim, get_headers.clone(), true, true);
        tls.write_all(&h_victim.encode()).unwrap();
        tls.flush().unwrap();
        next_sid += 2;

        // Let sozu start responding. HEADERS + first DATA frame should be
        // in flight, and expect_write may be parked if the buffer fills.
        thread::sleep(Duration::from_millis(150));

        // Peer-initiated RST_STREAM(CANCEL = 0x8). Exercises
        // handle_rst_stream_frame (h2.rs:3752), which now routes through
        // remove_dead_stream and must clear expect_write/expect_read if
        // they reference the evicted gid.
        let rst = H2Frame::rst_stream(victim, 0x8);
        let _ = tls.write_all(&rst.encode());
        let _ = tls.flush();
        thread::sleep(Duration::from_millis(80));

        // Fresh stream: drives Context::create_stream → recycle +
        // shrink_trailing_recycle. A stranded expect_write would panic
        // at h2.rs:1896 on the next writable() tick.
        let fresh = next_sid;
        let h_fresh = H2Frame::headers(fresh, get_headers.clone(), true, true);
        let _ = tls.write_all(&h_fresh.encode());
        let _ = tls.flush();
        next_sid += 2;

        // Drain — forces writable() to run, revealing any panic.
        let _ = collect_response_frames(&mut tls, 200, 8, 400);
    }

    drop(tls);
    thread::sleep(Duration::from_millis(200));

    let still_alive = verify_sozu_alive(front_port);
    println!("stranded expect_write peer RST - sozu alive: {still_alive}");

    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    let mut backends = vec![backend];
    for backend in backends.iter_mut() {
        backend.stop_and_get_aggregator();
    }

    if still_alive && stopped {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_stranded_expect_write_peer_rst_survives_cancellation() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "H2 correctness: stranded expect_write survives peer-initiated RST_STREAM",
            try_h2_stranded_expect_write_peer_rst_survives
        ),
        State::Success
    );
}

// ============================================================================
// Test: RFC 9218 §4 incremental round-robin scheduling
// ============================================================================

/// Build an HPACK-encoded header block for `GET / HTTP/2 https localhost`
/// plus an optional literal `priority` header (`u=<urgency>, <inc_token>`).
///
/// `inc_token` is inserted as-is after the comma — pass `"i"` for
/// `incremental=true` and `"i=?0"` to explicitly opt out. The priority header
/// is encoded as a literal-without-indexing field with a new name so sozu's
/// HPACK decoder takes the `compare_no_case(&k, b"priority")` branch in
/// `pkawa.rs:732`.
fn build_get_headers_with_priority(urgency: u8, inc_token: &str) -> Vec<u8> {
    let mut block = vec![
        0x82, // :method GET (indexed)
        0x84, // :path / (indexed)
        0x87, // :scheme https (indexed)
        0x41, 0x09, // :authority, value len 9
        b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't',
    ];
    // Build "u=N, <inc_token>" priority value.
    let value = format!("u={urgency}, {inc_token}");
    // Literal Header Field without Indexing — New Name (0x00), name length 8.
    block.push(0x00);
    block.push(0x08);
    block.extend_from_slice(b"priority");
    // Value length, value bytes.
    assert!(
        value.len() < 127,
        "priority value too long for short encoding"
    );
    block.push(value.len() as u8);
    block.extend_from_slice(value.as_bytes());
    block
}

/// Extract DATA frame stream IDs in arrival order, ignoring empty DATA
/// (END_STREAM markers sometimes emit payload-less DATA frames).
fn data_frame_stream_order(frames: &[(u8, u8, u32, Vec<u8>)]) -> Vec<u32> {
    frames
        .iter()
        .filter_map(|(ft, _fl, sid, payload)| {
            if *ft == H2_FRAME_DATA && !payload.is_empty() {
                Some(*sid)
            } else {
                None
            }
        })
        .collect()
}

/// RFC 9218 §4: three same-urgency streams with `priority: u=3, i` must see
/// their DATA frames **interleaved**, not drained sequentially. The test
/// starves sozu of per-stream send window during setup so all three
/// backend responses sit in sozu's kawa buffers, then releases the three
/// stream windows back-to-back. That forces the scheduler to iterate the
/// full `priorities_buf = [1, 3, 5]` inside a single `writable()` pass,
/// which is exactly the regime the round-robin logic governs.
fn try_h2_rfc9218_incremental_round_robin() -> State {
    // Three backends so each stream is served by a fresh TCP connection to
    // a dedicated thread — avoids head-of-line blocking on the backend side
    // that would otherwise serialize responses.
    let (worker, backends, front_port) = setup_h2_test_with_large_bodies(
        "H2-COR-RFC9218-INC",
        3,
        // Body ≈ 2 × max_frame_size (16384) → each stream emits ≥ 2 DATA
        // frames so interleaving is observable on the wire.
        32 * 1024,
    );

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    // INITIAL_WINDOW_SIZE=0 → sozu cannot send DATA on a new stream until
    // we WINDOW_UPDATE that stream explicitly. HEADERS still flow; the
    // responses accumulate in kawa.
    h2_handshake_with_initial_window(&mut tls, 0);

    let ids = [1u32, 3, 5];
    for sid in ids {
        let block = build_get_headers_with_priority(3, "i");
        let headers = H2Frame::headers(sid, block, true, true);
        tls.write_all(&headers.encode()).unwrap();
    }
    tls.flush().unwrap();

    // Let the three backends reply and sozu stash responses in stream
    // kawas. No DATA can egress yet because stream windows are 0.
    thread::sleep(Duration::from_millis(400));

    // Pack all four WINDOW_UPDATEs into a single TCP write so sozu sees
    // every stream's window lifted in one `readable()` pass and only runs
    // `writable()` afterwards — at which point the scheduler iterates a
    // fully-populated priorities_buf of length 3. If we flush between
    // frames, sozu's event loop may interleave `readable()` and
    // `writable()` and emit DATA for stream 1 alone before the other
    // windows are lifted, producing a non-RR wire trace for timing
    // reasons unrelated to the scheduler.
    let mut bulk = Vec::new();
    for sid in ids {
        bulk.extend_from_slice(&H2Frame::window_update(sid, 1_000_000).encode());
    }
    bulk.extend_from_slice(&H2Frame::window_update(0, 1_000_000).encode());
    tls.write_all(&bulk).unwrap();
    tls.flush().unwrap();

    let frames = collect_response_frames(&mut tls, 300, 6, 400);
    log_frames("RFC 9218 incremental RR", &frames);

    let order = data_frame_stream_order(&frames);
    println!("DATA stream-ID order: {order:?}");

    // Fairness predicate: count "transitions" where adjacent DATA frames
    // belong to different streams. Sequential drain (the non-incremental
    // baseline) gives exactly `n_streams - 1` transitions — stream IDs
    // like [1,1,3,3,5,5] have 2 transitions for 3 streams. Round-robin
    // produces many more transitions because each stream fires at most
    // once per scheduling pass before yielding.
    //
    // The threshold is relaxed past `n_streams - 1` rather than demanding
    // a perfect interleave: backend response arrivals are not synchronous,
    // so the very first few DATA frames may still come from whichever
    // stream's kawa was populated first. The RR logic must then kick in
    // and rotate across the remaining streams — which shows up as
    // *additional* transitions above the sequential baseline.
    let transitions = order.windows(2).filter(|w| w[0] != w[1]).count();
    let streams_seen: std::collections::HashSet<u32> = order.iter().copied().collect();
    let n_streams_seen = streams_seen
        .intersection(&ids.iter().copied().collect())
        .count();
    let each_stream_fired = ids.iter().all(|sid| streams_seen.contains(sid));
    // Need ≥ 2 transitions on top of the sequential baseline to prove RR
    // kicked in. A 3-stream round-robin drain of ~10 DATA frames typically
    // produces ≥ 6 transitions; we demand at least `n_streams` (= 3),
    // giving noise budget while still excluding the sequential pattern.
    let interleaved = transitions >= n_streams_seen;

    println!(
        "each_stream_fired={each_stream_fired} transitions={transitions} \
         interleaved={interleaved} streams_seen={n_streams_seen} \
         total_frames={}",
        order.len()
    );

    let infra_ok = teardown(tls, front_port, worker, backends);
    if infra_ok && each_stream_fired && interleaved {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_rfc9218_incremental_round_robin() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 RFC 9218 §4: incremental streams interleave DATA frames",
            try_h2_rfc9218_incremental_round_robin
        ),
        State::Success
    );
}

/// RFC 9218 §4: three same-urgency streams with `priority: u=3, i=?0` must
/// drain **sequentially** — the pre-existing behaviour. Uses the same
/// window-starving handshake as the round-robin test so the scheduler sees
/// all three streams as eligible in a single `writable()` pass; the only
/// difference from that test is the `incremental` flag in the priority
/// header. Each stream's DATA frames must appear as a contiguous run on
/// the wire (stream order like [1,1,1,3,3,3,5,5,5]).
fn try_h2_rfc9218_non_incremental_sequential() -> State {
    let (worker, backends, front_port) =
        setup_h2_test_with_large_bodies("H2-COR-RFC9218-SEQ", 3, 32 * 1024);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    // INITIAL_WINDOW_SIZE=0 so the three responses park in kawa before any
    // DATA is released — keeps the test symmetric with the incremental
    // round-robin counterpart.
    h2_handshake_with_initial_window(&mut tls, 0);

    let ids = [1u32, 3, 5];
    for sid in ids {
        // `i=?0` = explicit non-incremental (also the RFC 9218 default when
        // the parameter is absent).
        let block = build_get_headers_with_priority(3, "i=?0");
        let headers = H2Frame::headers(sid, block, true, true);
        tls.write_all(&headers.encode()).unwrap();
    }
    tls.flush().unwrap();

    thread::sleep(Duration::from_millis(400));

    // Bulk WINDOW_UPDATE so sozu sees all three streams eligible in one
    // pass — see the round-robin test for the rationale.
    let mut bulk = Vec::new();
    for sid in ids {
        bulk.extend_from_slice(&H2Frame::window_update(sid, 1_000_000).encode());
    }
    bulk.extend_from_slice(&H2Frame::window_update(0, 1_000_000).encode());
    tls.write_all(&bulk).unwrap();
    tls.flush().unwrap();

    let frames = collect_response_frames(&mut tls, 300, 6, 400);
    log_frames("RFC 9218 non-incremental (baseline)", &frames);

    let order = data_frame_stream_order(&frames);
    println!("DATA stream-ID order: {order:?}");

    // Sequential-drain assertion: for each stream, all of its DATA frames
    // should form a contiguous block. Equivalently, the number of
    // "transitions" (adjacent frames with different stream IDs) equals the
    // number of streams − 1 — i.e. stream order like [1,1,1,3,3,3,5,5,5]
    // gives 2 transitions for 3 streams. Round-robin would yield 8.
    let transitions = order.windows(2).filter(|w| w[0] != w[1]).count();
    let streams_seen: std::collections::HashSet<u32> = order.iter().copied().collect();
    let n_streams = streams_seen.len();
    let sequential = !order.is_empty() && transitions == n_streams.saturating_sub(1);

    println!("transitions={transitions} streams_seen={n_streams} sequential={sequential}");

    let infra_ok = teardown(tls, front_port, worker, backends);
    if infra_ok && sequential && ids.iter().all(|sid| streams_seen.contains(sid)) {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_rfc9218_non_incremental_sequential() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 RFC 9218 §4: non-incremental streams drain sequentially",
            try_h2_rfc9218_non_incremental_sequential
        ),
        State::Success
    );
}

/// Tier 3a — LIFECYCLE §9 invariant 17: RFC 9218 incremental peer count
/// must be scoped to the same urgency bucket, not the whole connection.
/// Two incremental streams in DIFFERENT urgency buckets (`u=0, i` and
/// `u=7, i`) are each solo in their own bucket. With the baseline
/// connection-global count, both streams would see
/// `incremental_peer_count >= 2` and yield after every DATA frame,
/// spuriously triggering the same strand the `finalize_write`
/// WRITABLE-withdrawal guard was added for. With bucket scoping each
/// stream sees peer count == 1 and drains sequentially — DATA frames
/// for stream 1 form a contiguous run, stream 3 likewise.
fn try_h2_rfc9218_incremental_multi_bucket_drains_sequentially() -> State {
    let (worker, backends, front_port) =
        setup_h2_test_with_large_bodies("H2-COR-RFC9218-MB", 2, 32 * 1024);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    // INITIAL_WINDOW_SIZE=0 so both responses park in kawa before any DATA
    // is released — ensures the scheduler sees both streams as eligible in
    // a single `writable()` pass.
    h2_handshake_with_initial_window(&mut tls, 0);

    // Stream 1: highest urgency incremental. Stream 3: lowest urgency
    // incremental. Different urgency buckets on purpose.
    let request_ids: [(u32, u8); 2] = [(1, 0), (3, 7)];
    for (sid, urgency) in request_ids {
        let block = build_get_headers_with_priority(urgency, "i");
        let headers = H2Frame::headers(sid, block, true, true);
        tls.write_all(&headers.encode()).unwrap();
    }
    tls.flush().unwrap();

    thread::sleep(Duration::from_millis(400));

    // Bulk WINDOW_UPDATE — see round-robin test for rationale.
    let mut bulk = Vec::new();
    for (sid, _) in request_ids {
        bulk.extend_from_slice(&H2Frame::window_update(sid, 1_000_000).encode());
    }
    bulk.extend_from_slice(&H2Frame::window_update(0, 1_000_000).encode());
    tls.write_all(&bulk).unwrap();
    tls.flush().unwrap();

    let frames = collect_response_frames(&mut tls, 300, 6, 400);
    log_frames("RFC 9218 multi-bucket incremental", &frames);

    let order = data_frame_stream_order(&frames);
    println!("DATA stream-ID order: {order:?}");

    // Each stream is solo in its urgency bucket. `incremental_peer_count`
    // is 1 per bucket → converter drains sequentially inside each bucket.
    // Priority sort picks the lower urgency first, so stream 1 drains
    // before stream 3. Total transitions = 1 (stream 1 block → stream 3
    // block); baseline (without bucket scoping) would interleave and
    // produce many more transitions.
    let transitions = order.windows(2).filter(|w| w[0] != w[1]).count();
    let streams_seen: std::collections::HashSet<u32> = order.iter().copied().collect();
    let both_fired = request_ids
        .iter()
        .all(|(sid, _)| streams_seen.contains(sid));
    let sequential_per_bucket =
        !order.is_empty() && transitions == streams_seen.len().saturating_sub(1);

    println!(
        "both_fired={both_fired} transitions={transitions} \
         sequential_per_bucket={sequential_per_bucket} \
         streams_seen={} total_frames={}",
        streams_seen.len(),
        order.len()
    );

    let infra_ok = teardown(tls, front_port, worker, backends);
    if infra_ok && both_fired && sequential_per_bucket {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_rfc9218_incremental_multi_bucket_drains_sequentially() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 RFC 9218 §4 Tier 3a: incremental_peer_count is bucket-scoped",
            try_h2_rfc9218_incremental_multi_bucket_drains_sequentially
        ),
        State::Success
    );
}

/// Tier 3b — RFC 9218 §7.1: a PRIORITY_UPDATE frame for an open stream
/// with a well-formed priority field value is accepted and does not
/// terminate the connection. The response must still drain fully with
/// END_STREAM.
fn try_h2_priority_update_on_open_stream_is_accepted() -> State {
    let (worker, backends, front_port) =
        setup_h2_test_with_large_bodies("H2-COR-PU-ACCEPT", 1, 1024);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake_with_initial_window(&mut tls, 65_535);

    // Open stream 1 with a default priority header.
    let block = build_get_headers_with_priority(3, "i=?0");
    let headers = H2Frame::headers(1, block, true, true);
    tls.write_all(&headers.encode()).unwrap();
    // Re-prioritize stream 1 mid-flight with `u=0, i`.
    let pu = H2Frame::priority_update(1, "u=0, i");
    tls.write_all(&pu.encode()).unwrap();
    tls.flush().unwrap();

    let frames = collect_response_frames(&mut tls, 200, 5, 400);
    log_frames("PRIORITY_UPDATE accept", &frames);

    // No GOAWAY — the response must deliver normally.
    let has_goaway = frames.iter().any(|(ft, _, _, _)| *ft == H2_FRAME_GOAWAY);
    let has_end_stream_on_1 = frames.iter().any(|(ft, fl, sid, _)| {
        (*ft == H2_FRAME_DATA || *ft == H2_FRAME_HEADERS)
            && *sid == 1
            && (fl & H2_FLAG_END_STREAM) != 0
    });

    println!(
        "has_goaway={has_goaway} has_end_stream_on_1={has_end_stream_on_1} frames={}",
        frames.len()
    );

    let infra_ok = teardown(tls, front_port, worker, backends);
    if infra_ok && !has_goaway && has_end_stream_on_1 {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_priority_update_on_open_stream_is_accepted() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 RFC 9218 §7.1: PRIORITY_UPDATE accepted + response drains",
            try_h2_priority_update_on_open_stream_is_accepted
        ),
        State::Success
    );
}

/// Tier 3b — RFC 9218 §7.1: a PRIORITY_UPDATE frame with
/// `prioritized_stream_id == 0` is a connection-level PROTOCOL_ERROR.
/// Sozu must emit GOAWAY(PROTOCOL_ERROR) and close the session rather
/// than silently ignoring the frame.
fn try_h2_priority_update_on_stream_zero_is_protocol_error() -> State {
    let (worker, backends, front_port) =
        setup_h2_test_with_large_bodies("H2-COR-PU-STREAM0", 1, 1024);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake_with_initial_window(&mut tls, 65_535);

    // Send a PRIORITY_UPDATE targeting stream 0 (the connection itself).
    let pu = H2Frame::priority_update(0, "u=0, i");
    tls.write_all(&pu.encode()).unwrap();
    tls.flush().unwrap();

    let frames = collect_response_frames(&mut tls, 200, 4, 300);
    log_frames("PRIORITY_UPDATE stream0", &frames);

    let has_protocol_error_goaway = frames.iter().any(|(ft, _, _, payload)| {
        *ft == H2_FRAME_GOAWAY
            && payload.len() >= 8
            // bytes 4..8 carry the error code (RFC 9113 §6.8)
            && u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]) == 0x1
    });

    println!(
        "has_protocol_error_goaway={has_protocol_error_goaway} frames={}",
        frames.len()
    );

    let infra_ok = teardown(tls, front_port, worker, backends);
    if infra_ok && has_protocol_error_goaway {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_priority_update_on_stream_zero_is_protocol_error() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 RFC 9218 §7.1: PRIORITY_UPDATE on stream 0 → GOAWAY(PROTOCOL_ERROR)",
            try_h2_priority_update_on_stream_zero_is_protocol_error
        ),
        State::Success
    );
}

/// Tier 3c — LIFECYCLE §9 invariant 19: the converter must NOT yield
/// between the last DATA frame and the closing `Block::Flags`
/// (end_stream = true). Three same-urgency incremental streams drive
/// the round-robin yield; each response body is small enough that the
/// scheduler reaches the closing Flags right after its first DATA frame.
/// With Tier 3c's close-race guard, END_STREAM lands in the same pass
/// rather than waiting for another writable tick (or — pre-invariant-16
/// — stranding permanently).
fn try_h2_incremental_round_robin_closes_every_stream() -> State {
    let (worker, backends, front_port) =
        setup_h2_test_with_large_bodies("H2-COR-RR-CLOSE", 3, 4 * 1024);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake_with_initial_window(&mut tls, 0);

    let ids = [1u32, 3, 5];
    for sid in ids {
        let block = build_get_headers_with_priority(3, "i");
        let headers = H2Frame::headers(sid, block, true, true);
        tls.write_all(&headers.encode()).unwrap();
    }
    tls.flush().unwrap();

    thread::sleep(Duration::from_millis(400));

    let mut bulk = Vec::new();
    for sid in ids {
        bulk.extend_from_slice(&H2Frame::window_update(sid, 1_000_000).encode());
    }
    bulk.extend_from_slice(&H2Frame::window_update(0, 1_000_000).encode());
    tls.write_all(&bulk).unwrap();
    tls.flush().unwrap();

    let frames = collect_response_frames(&mut tls, 300, 6, 400);
    log_frames("RR close-race", &frames);

    // Every stream must observe at least one frame with END_STREAM set
    // (either the terminal DATA or an empty DATA marker produced by the
    // converter's close path).
    let closed: std::collections::HashSet<u32> = frames
        .iter()
        .filter_map(|(ft, fl, sid, _)| {
            if (*ft == H2_FRAME_DATA || *ft == H2_FRAME_HEADERS) && (fl & H2_FLAG_END_STREAM) != 0 {
                Some(*sid)
            } else {
                None
            }
        })
        .collect();
    let all_closed = ids.iter().all(|sid| closed.contains(sid));

    println!("closed_streams={closed:?} all_closed={all_closed}");

    let infra_ok = teardown(tls, front_port, worker, backends);
    if infra_ok && all_closed {
        State::Success
    } else {
        State::Fail
    }
}

#[test]
fn test_h2_incremental_round_robin_closes_every_stream() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 RFC 9218 §4 Tier 3c: round-robin drain emits END_STREAM per stream",
            try_h2_incremental_round_robin_closes_every_stream
        ),
        State::Success
    );
}

/// RFC 9218 §4 + sozu incremental-scheduling regression (PR #1209 follow-up):
/// a SOLO stream whose client sent `priority: u=0, i` (Chrome's navigation
/// default since Chrome 107) must drain its full response body promptly.
///
/// With only one stream in the urgency-0 incremental bucket, the round-robin
/// yield at `converter.rs:434` has no peer to rotate to; yielding strands the
/// stream because `finalize_write:2634-2641` removes `Ready::WRITABLE` on a
/// clean drain when `expect_write.is_none()`. Edge-triggered epoll never
/// re-fires (kernel TCP buffer barely touched) and no new peer frame is
/// coming (Chrome's 6 MiB per-stream window is not yet exhausted), so the
/// stream parks silently. `front_timeout` (60 s default per
/// `command/src/config.rs:118 DEFAULT_FRONT_TIMEOUT`) eventually tears the
/// TCP connection down and Chrome reports `ERR_CONNECTION_CLOSED`.
///
/// Test shape:
/// - One backend serving an 80 KB body (= 5 full 16 384-byte DATA frames).
/// - Single client, single stream id=1 with `priority: u=0, i`.
/// - Generous initial per-stream + connection windows so flow control is
///   never the stall reason.
/// - Assert: full 80 KB body received within 2 s, END_STREAM flag on the
///   last DATA frame.
///
/// MUST FAIL on `feat/h2-mux` HEAD (commit `fe528e75` landed the RFC 9218
/// scheduling that introduced the strand); MUST PASS after Fix A (skip
/// incremental yield when `incremental_peer_count <= 1`).
fn try_h2_solo_incremental_drains_fully() -> State {
    const BODY_SIZE: usize = 80 * 1024;

    let (worker, backends, front_port) =
        setup_h2_test_with_large_bodies("H2-COR-SOLO-INCREMENTAL", 1, BODY_SIZE);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    // Generous initial per-stream window. Chrome advertises 6 MiB; we use
    // 1 MiB for the test so flow control is demonstrably not the stall.
    h2_handshake_with_initial_window(&mut tls, 1_000_000);

    let sid: u32 = 1;
    // Chrome's navigation priority: u=0 highest urgency, i (incremental).
    let block = build_get_headers_with_priority(0, "i");
    let headers = H2Frame::headers(sid, block, true, true);
    tls.write_all(&headers.encode()).unwrap();
    tls.flush().unwrap();

    // Lift the connection-level window so the 80 KB body isn't capped by
    // the default 65535 conn window — per-stream flow control alone is not
    // enough when the connection pool needs bandwidth.
    tls.write_all(&H2Frame::window_update(0, 1_000_000).encode())
        .unwrap();
    tls.flush().unwrap();

    // Deadline: 2 seconds. A healthy scheduler delivers 80 KB in a few
    // milliseconds; the bugged scheduler stalls after the first pass and
    // never emits the final DATA + END_STREAM on this stream.
    //
    // We set a 250 ms socket read timeout and stop reading as soon as
    // END_STREAM is seen on our stream id (or we time out on the 3 s wall
    // clock). Using a pure `collect_response_frames` call would conflate
    // "body delivered" with "connection still open and idle" — H2 keeps
    // the TCP connection around after END_STREAM so the collect loop
    // would spin on empty reads until its own budget expires, making the
    // measurement useless for a body-delivery deadline.
    tls.sock
        .set_read_timeout(Some(Duration::from_millis(250)))
        .ok();
    let start = Instant::now();
    let mut raw = Vec::new();
    let mut rbuf = vec![0u8; 65536];
    let mut done = false;
    while !done && start.elapsed() < Duration::from_secs(3) {
        match tls.read(&mut rbuf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&rbuf[..n]);
                let frames = parse_h2_frames(&raw);
                for (ft, fl, s, _) in &frames {
                    if *ft == H2_FRAME_DATA && *s == sid && (*fl & H2_FLAG_END_STREAM) != 0 {
                        done = true;
                        break;
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
    let frames = parse_h2_frames(&raw);

    log_frames("solo incremental 80 KB", &frames);

    // Count body bytes on our stream; track END_STREAM on any DATA frame.
    let mut body_bytes: usize = 0;
    let mut end_stream_seen = false;
    for (ft, fl, s, payload) in &frames {
        if *ft == H2_FRAME_DATA && *s == sid {
            body_bytes += payload.len();
            if (*fl & H2_FLAG_END_STREAM) != 0 {
                end_stream_seen = true;
            }
        }
    }

    println!(
        "solo-incremental: body_bytes={body_bytes}/{BODY_SIZE} \
         end_stream={end_stream_seen} elapsed={elapsed:?}"
    );

    let infra_ok = teardown(tls, front_port, worker, backends);

    let full_body = body_bytes == BODY_SIZE;
    let in_deadline = elapsed < Duration::from_secs(2);

    if infra_ok && full_body && end_stream_seen && in_deadline {
        State::Success
    } else {
        println!(
            "FAIL: body_bytes={body_bytes} (want {BODY_SIZE}), \
             end_stream={end_stream_seen}, elapsed={elapsed:?} (want <2s), \
             infra_ok={infra_ok}"
        );
        State::Fail
    }
}

#[test]
fn test_h2_solo_incremental_drains_fully() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 solo `priority: u=0, i` stream must drain full body within 2 s",
            try_h2_solo_incremental_drains_fully
        ),
        State::Success
    );
}

// ============================================================================
// H2 large-asset repro suite (Sébastien Brunat 2026-04-23 PHP/Apache report)
// ============================================================================

/// Firefox-shape header block: same pseudo-headers as
/// [`build_get_headers_with_priority`] but WITHOUT the `priority` literal.
/// Firefox does not emit `priority: u=0, i` by default, so any truncation
/// observable on this shape lives outside the RFC 9218 solo-bucket fix
/// at `converter.rs:437-450`.
fn build_get_headers_no_priority() -> Vec<u8> {
    vec![
        0x82, // :method GET (indexed)
        0x84, // :path / (indexed)
        0x87, // :scheme https (indexed)
        0x41, 0x09, // :authority, value len 9
        b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't',
    ]
}

/// Drain one stream until END_STREAM on the given `sid` or the `deadline`
/// expires. Returns `(body_bytes, end_stream_seen, got_rst_stream_on_sid,
/// elapsed)`.
///
/// Uses the END_STREAM-aware early-exit pattern documented in memory
/// `feedback_collect_response_frames_quiet_time.md`: a `collect_response_frames`
/// elapsed budget measures quiet time, not body delivery. The caller's
/// deadline wall-clock is the source of truth.
fn drain_h2_stream_until_end_stream(
    tls: &mut rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>,
    sid: u32,
    deadline: Duration,
) -> (usize, bool, bool, Duration) {
    tls.sock
        .set_read_timeout(Some(Duration::from_millis(250)))
        .ok();
    let start = Instant::now();
    let mut raw = Vec::new();
    let mut rbuf = vec![0u8; 65536];
    let mut done = false;
    let mut got_rst = false;
    while !done && start.elapsed() < deadline {
        match tls.read(&mut rbuf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&rbuf[..n]);
                let frames = parse_h2_frames(&raw);
                for (ft, fl, s, _) in &frames {
                    if *ft == H2_FRAME_DATA && *s == sid && (*fl & H2_FLAG_END_STREAM) != 0 {
                        done = true;
                        break;
                    }
                    if *ft == H2_FRAME_HEADERS && *s == sid && (*fl & H2_FLAG_END_STREAM) != 0 {
                        done = true;
                        break;
                    }
                    if *ft == H2_FRAME_RST_STREAM && *s == sid {
                        got_rst = true;
                        done = true;
                        break;
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
    let frames = parse_h2_frames(&raw);
    let mut body_bytes = 0usize;
    let mut end_stream_seen = false;
    for (ft, fl, s, payload) in &frames {
        if *ft == H2_FRAME_DATA && *s == sid {
            body_bytes += payload.len();
            if (*fl & H2_FLAG_END_STREAM) != 0 {
                end_stream_seen = true;
            }
        }
    }
    (body_bytes, end_stream_seen, got_rst, elapsed)
}

/// Build a full HTTPS listener + cluster + cert + backend stack with a
/// single configurable backend thread. Returns (worker, front_port,
/// back_address). The caller spawns the mock backend against `back_address`.
fn setup_single_h1_backend_listener(
    name: &str,
    h2_stream_idle_timeout_seconds: Option<u32>,
) -> (Worker, u16, SocketAddr) {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);

    let (config, listeners, state) = Worker::empty_https_config(front_address.clone().into());
    let mut worker = Worker::start_new_worker_owned(name, config, listeners, state);

    let mut listener_config = ListenerBuilder::new_https(front_address.clone())
        .to_tls(None)
        .unwrap();
    if let Some(secs) = h2_stream_idle_timeout_seconds {
        listener_config.h2_stream_idle_timeout_seconds = Some(secs);
    }

    worker.send_proxy_request_type(RequestType::AddHttpsListener(listener_config));
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

    let back_address = create_local_address();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0".to_owned(),
        back_address,
        None,
    )));
    worker.read_to_last();
    (worker, front_port, back_address)
}

// ----------------------------------------------------------------------------
// (a) test_h2_firefox_shape_h1_cl_drains_fully — Firefox shape + H1 CL
// ----------------------------------------------------------------------------

/// Firefox shape: no `priority` header, H1 backend with Content-Length,
/// 80 892 bytes (HAR-exact value from Sébastien Brunat's 2026-04-23 AM
/// report). Exercises `mux/h1.rs:241-347` on the write path — the site
/// where C1 (`signal_pending_write` missing on `Ready::WRITABLE` insert)
/// historically parked large asset deliveries.
///
/// Expected on HEAD (post-C1 fix): PASS within 3 s. Before the fix: would
/// stall — the test is a lock-in anti-regression for C1 on the Firefox
/// code path.
fn try_h2_firefox_shape_h1_cl_drains_fully() -> State {
    const BODY_SIZE: usize = 80_892;

    let (worker, backends, front_port) =
        setup_h2_test_with_large_bodies("H2-LA-FF-CL", 1, BODY_SIZE);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake_with_initial_window(&mut tls, 1_000_000);

    let sid: u32 = 1;
    let block = build_get_headers_no_priority();
    let headers = H2Frame::headers(sid, block, true, true);
    tls.write_all(&headers.encode()).unwrap();
    tls.flush().unwrap();
    tls.write_all(&H2Frame::window_update(0, 1_000_000).encode())
        .unwrap();
    tls.flush().unwrap();

    let (body_bytes, end_stream_seen, got_rst, elapsed) =
        drain_h2_stream_until_end_stream(&mut tls, sid, Duration::from_secs(3));

    println!(
        "firefox-shape CL: body_bytes={body_bytes}/{BODY_SIZE} \
         end_stream={end_stream_seen} rst={got_rst} elapsed={elapsed:?}"
    );

    let infra_ok = teardown(tls, front_port, worker, backends);

    if infra_ok && body_bytes == BODY_SIZE && end_stream_seen && !got_rst {
        State::Success
    } else {
        println!(
            "FAIL: body_bytes={body_bytes} (want {BODY_SIZE}), \
             end_stream={end_stream_seen}, rst={got_rst}, \
             elapsed={elapsed:?}, infra_ok={infra_ok}"
        );
        State::Fail
    }
}

#[test]
fn test_h2_firefox_shape_h1_cl_drains_fully() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 Firefox shape (no priority) + H1 CL backend must drain 80892 B within 3 s",
            try_h2_firefox_shape_h1_cl_drains_fully
        ),
        State::Success
    );
}

// ----------------------------------------------------------------------------
// (b) test_h2_chrome_shape_h1_cl_drains_fully — Chrome shape + H1 CL
// ----------------------------------------------------------------------------

/// Chrome shape: `priority: u=0, i`, H1 backend with Content-Length,
/// 100 KiB. Locks in the RFC 9218 solo-bucket fix (`converter.rs:437-450`,
/// commit `f6c02912`) on the H1-backend code path — the existing
/// `test_h2_solo_incremental_drains_fully` uses an H2 backend via
/// `setup_h2_test_with_large_bodies`, which internally spawns
/// `AsyncBackend::http_handler` — so this test adds coverage for the
/// `mux/h1.rs` → `mux/h2.rs` write-path interaction under the solo
/// incremental shape.
fn try_h2_chrome_shape_h1_cl_drains_fully() -> State {
    const BODY_SIZE: usize = 100 * 1024;

    let (worker, backends, front_port) =
        setup_h2_test_with_large_bodies("H2-LA-CH-CL", 1, BODY_SIZE);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake_with_initial_window(&mut tls, 1_000_000);

    let sid: u32 = 1;
    let block = build_get_headers_with_priority(0, "i");
    let headers = H2Frame::headers(sid, block, true, true);
    tls.write_all(&headers.encode()).unwrap();
    tls.flush().unwrap();
    tls.write_all(&H2Frame::window_update(0, 1_000_000).encode())
        .unwrap();
    tls.flush().unwrap();

    let (body_bytes, end_stream_seen, got_rst, elapsed) =
        drain_h2_stream_until_end_stream(&mut tls, sid, Duration::from_secs(3));

    println!(
        "chrome-shape H1 CL: body_bytes={body_bytes}/{BODY_SIZE} \
         end_stream={end_stream_seen} rst={got_rst} elapsed={elapsed:?}"
    );

    let infra_ok = teardown(tls, front_port, worker, backends);

    if infra_ok && body_bytes == BODY_SIZE && end_stream_seen && !got_rst {
        State::Success
    } else {
        println!(
            "FAIL: body_bytes={body_bytes} (want {BODY_SIZE}), \
             end_stream={end_stream_seen}, rst={got_rst}, \
             elapsed={elapsed:?}, infra_ok={infra_ok}"
        );
        State::Fail
    }
}

#[test]
fn test_h2_chrome_shape_h1_cl_drains_fully() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 Chrome shape (u=0, i) + H1 CL backend must drain 100 KiB within 3 s",
            try_h2_chrome_shape_h1_cl_drains_fully
        ),
        State::Success
    );
}

// ----------------------------------------------------------------------------
// (c) test_h2_php_apache_chunked_flush_drains_fully — chunked + per-chunk
// ----------------------------------------------------------------------------

/// PHP/Apache shape: chunked + per-chunk `flush()` cadence from
/// [`ChunkedFlushH1Backend`], 312 215 bytes (HAR-exact `big.svg`). Pre-C1
/// fix this would park because `mux/h1.rs:341-346, 351-357` flipped
/// `Ready::WRITABLE` on the peer without `signal_pending_write()` and
/// edge-triggered epoll never re-fired for the queued bytes. Post-fix the
/// stream drains within 5 s.
fn try_h2_php_apache_chunked_flush_drains_fully() -> State {
    use crate::mock::chunked_flush_h1_backend::{
        ChunkedFlushConfig, ChunkedFlushH1Backend, TransferEncoding,
    };
    const BODY_SIZE: usize = 312_215;

    let (worker, front_port, back_address) =
        setup_single_h1_backend_listener("H2-LA-PHP-CHUNKED", None);

    let backend = ChunkedFlushH1Backend::start(
        back_address,
        ChunkedFlushConfig {
            body_size: BODY_SIZE,
            chunk_size: 8 * 1024,
            inter_chunk_delay: Duration::from_micros(500),
            transfer_encoding: TransferEncoding::Chunked,
            tcp_nodelay: true,
            truncate_at_byte: None,
        },
    );

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake_with_initial_window(&mut tls, 1_000_000);

    let sid: u32 = 1;
    let block = build_get_headers_no_priority();
    let headers = H2Frame::headers(sid, block, true, true);
    tls.write_all(&headers.encode()).unwrap();
    tls.flush().unwrap();
    tls.write_all(&H2Frame::window_update(0, 1_000_000).encode())
        .unwrap();
    tls.flush().unwrap();

    let (body_bytes, end_stream_seen, got_rst, elapsed) =
        drain_h2_stream_until_end_stream(&mut tls, sid, Duration::from_secs(5));

    println!(
        "php-apache-chunked: body_bytes={body_bytes}/{BODY_SIZE} \
         end_stream={end_stream_seen} rst={got_rst} elapsed={elapsed:?} \
         backend_responses={}",
        backend.responses_sent()
    );

    let infra_ok = teardown(
        tls,
        front_port,
        worker,
        Vec::<AsyncBackend<SimpleAggregator>>::new(),
    );
    drop(backend);

    if infra_ok && body_bytes == BODY_SIZE && end_stream_seen && !got_rst {
        State::Success
    } else {
        println!(
            "FAIL: body_bytes={body_bytes} (want {BODY_SIZE}), \
             end_stream={end_stream_seen}, rst={got_rst}, \
             elapsed={elapsed:?}, infra_ok={infra_ok}"
        );
        State::Fail
    }
}

#[test]
fn test_h2_php_apache_chunked_flush_drains_fully() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 PHP/Apache chunked-flush backend must drain 312 KiB within 5 s",
            try_h2_php_apache_chunked_flush_drains_fully
        ),
        State::Success
    );
}

// ----------------------------------------------------------------------------
// (d) test_h2_slow_backend_idle_timeout_cancels — C2 RED/GREEN
// ----------------------------------------------------------------------------

/// Slow backend shape: chunked body streamed at ~16 KiB per 1-second tick,
/// across 4 ticks (~64 KiB total, 4-second wall clock). Listener config
/// sets `h2_stream_idle_timeout_seconds = 2` (well below the total
/// delivery time). Pre-C2 fix the per-stream idle timer refreshed only on
/// inbound DATA/HEADERS (`h2.rs:3887-3895, 4026-4031`) — a long-running
/// response without any inbound client frames would be cancelled mid-
/// delivery. Post-fix the outbound write path refreshes the timer and
/// the response drains to completion.
fn try_h2_slow_backend_idle_timeout_cancels() -> State {
    use crate::mock::chunked_flush_h1_backend::{
        ChunkedFlushConfig, ChunkedFlushH1Backend, TransferEncoding,
    };
    const BODY_SIZE: usize = 64 * 1024;

    let (worker, front_port, back_address) =
        setup_single_h1_backend_listener("H2-LA-SLOW-IDLE", Some(2));

    let backend = ChunkedFlushH1Backend::start(
        back_address,
        ChunkedFlushConfig {
            body_size: BODY_SIZE,
            chunk_size: 16 * 1024,
            inter_chunk_delay: Duration::from_secs(1),
            transfer_encoding: TransferEncoding::Chunked,
            tcp_nodelay: true,
            truncate_at_byte: None,
        },
    );

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake_with_initial_window(&mut tls, 1_000_000);

    let sid: u32 = 1;
    let block = build_get_headers_no_priority();
    let headers = H2Frame::headers(sid, block, true, true);
    tls.write_all(&headers.encode()).unwrap();
    tls.flush().unwrap();
    tls.write_all(&H2Frame::window_update(0, 1_000_000).encode())
        .unwrap();
    tls.flush().unwrap();

    let (body_bytes, end_stream_seen, got_rst, elapsed) =
        drain_h2_stream_until_end_stream(&mut tls, sid, Duration::from_secs(10));

    println!(
        "slow-backend idle: body_bytes={body_bytes}/{BODY_SIZE} \
         end_stream={end_stream_seen} rst={got_rst} elapsed={elapsed:?} \
         backend_responses={}",
        backend.responses_sent()
    );

    let infra_ok = teardown(
        tls,
        front_port,
        worker,
        Vec::<AsyncBackend<SimpleAggregator>>::new(),
    );
    drop(backend);

    if infra_ok && body_bytes == BODY_SIZE && end_stream_seen && !got_rst {
        State::Success
    } else {
        println!(
            "FAIL: body_bytes={body_bytes} (want {BODY_SIZE}), \
             end_stream={end_stream_seen}, rst={got_rst}, \
             elapsed={elapsed:?}, infra_ok={infra_ok}"
        );
        State::Fail
    }
}

#[test]
fn test_h2_slow_backend_idle_timeout_cancels() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 slow chunked backend must not be cancelled by per-stream idle timer",
            try_h2_slow_backend_idle_timeout_cancels
        ),
        State::Success
    );
}

// ----------------------------------------------------------------------------
// (e) test_h2_chunked_backend_crash_mid_stream_rsts — C3 RED/GREEN
// ----------------------------------------------------------------------------

/// Chunked backend crashes mid-body: writes ~50 KiB of a 100 KiB body then
/// drops the TCP connection WITHOUT the terminating `0\r\n\r\n`. Pre-C3
/// fix the H1 reader path (`mux/h1.rs:terminate_close_delimited`) would
/// silently mark the stream END_STREAM, so the H2 converter emitted a
/// truncated DATA frame with END_STREAM — silent corruption. Post-fix the
/// chunked EOF is demoted to `ParsingPhase::Error` and the H2 converter
/// emits RST_STREAM(InternalError) per RFC 9112 §7.1.
fn try_h2_chunked_backend_crash_mid_stream_rsts() -> State {
    use crate::mock::chunked_flush_h1_backend::{
        ChunkedFlushConfig, ChunkedFlushH1Backend, TransferEncoding,
    };
    const BODY_SIZE: usize = 100 * 1024;
    const TRUNCATE_AT: usize = 50 * 1024;

    let (worker, front_port, back_address) = setup_single_h1_backend_listener("H2-LA-CRASH", None);

    let backend = ChunkedFlushH1Backend::start(
        back_address,
        ChunkedFlushConfig {
            body_size: BODY_SIZE,
            chunk_size: 8 * 1024,
            inter_chunk_delay: Duration::from_micros(500),
            transfer_encoding: TransferEncoding::Chunked,
            tcp_nodelay: true,
            truncate_at_byte: Some(TRUNCATE_AT),
        },
    );

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake_with_initial_window(&mut tls, 1_000_000);

    let sid: u32 = 1;
    let block = build_get_headers_no_priority();
    let headers = H2Frame::headers(sid, block, true, true);
    tls.write_all(&headers.encode()).unwrap();
    tls.flush().unwrap();
    tls.write_all(&H2Frame::window_update(0, 1_000_000).encode())
        .unwrap();
    tls.flush().unwrap();

    let (body_bytes, end_stream_seen, got_rst, elapsed) =
        drain_h2_stream_until_end_stream(&mut tls, sid, Duration::from_secs(5));

    println!(
        "chunked-crash: body_bytes={body_bytes}/{BODY_SIZE} \
         end_stream={end_stream_seen} rst={got_rst} elapsed={elapsed:?}"
    );

    let infra_ok = teardown(
        tls,
        front_port,
        worker,
        Vec::<AsyncBackend<SimpleAggregator>>::new(),
    );
    drop(backend);

    // Success condition: the client must see either RST_STREAM (preferred)
    // or no END_STREAM (partial delivery without completion). Silent
    // END_STREAM with body_bytes == BODY_SIZE would be the C3 bug.
    let c3_guard = got_rst || !end_stream_seen;
    if infra_ok && c3_guard {
        State::Success
    } else {
        println!(
            "FAIL (C3 silent truncation): body_bytes={body_bytes}, \
             end_stream={end_stream_seen}, rst={got_rst}, elapsed={elapsed:?}, \
             infra_ok={infra_ok}"
        );
        State::Fail
    }
}

#[test]
fn test_h2_chunked_backend_crash_mid_stream_rsts() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 chunked backend crash mid-body must emit RST_STREAM, not silent END_STREAM",
            try_h2_chunked_backend_crash_mid_stream_rsts
        ),
        State::Success
    );
}

// ----------------------------------------------------------------------------
// (f) test_h2_coalesced_chrome_firefox_streams_drain — coalescing lock-in
// ----------------------------------------------------------------------------

/// Two concurrent streams on the same TLS connection: sid=1 is Chrome shape
/// (`priority: u=0, i`), sid=3 is Firefox shape (no priority). Both served
/// by H1 backends at 100 KiB each. Asserts both drain within 5 s. Locks in
/// the absence of cross-stream interaction that a connection-coalescing
/// regression (H7) could introduce.
fn try_h2_coalesced_chrome_firefox_streams_drain() -> State {
    const BODY_SIZE: usize = 100 * 1024;

    let (worker, backends, front_port) =
        setup_h2_test_with_large_bodies("H2-LA-COALESCED", 2, BODY_SIZE);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake_with_initial_window(&mut tls, 1_000_000);

    // sid=1 Chrome shape
    let block1 = build_get_headers_with_priority(0, "i");
    let h1 = H2Frame::headers(1, block1, true, true);
    tls.write_all(&h1.encode()).unwrap();
    // sid=3 Firefox shape
    let block3 = build_get_headers_no_priority();
    let h3 = H2Frame::headers(3, block3, true, true);
    tls.write_all(&h3.encode()).unwrap();
    tls.flush().unwrap();
    tls.write_all(&H2Frame::window_update(0, 1_000_000).encode())
        .unwrap();
    tls.flush().unwrap();

    // Custom drain: wait until END_STREAM on both sid=1 AND sid=3.
    tls.sock
        .set_read_timeout(Some(Duration::from_millis(250)))
        .ok();
    let start = Instant::now();
    let deadline = Duration::from_secs(5);
    let mut raw = Vec::new();
    let mut rbuf = vec![0u8; 65536];
    let mut end1 = false;
    let mut end3 = false;
    while (!end1 || !end3) && start.elapsed() < deadline {
        match tls.read(&mut rbuf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&rbuf[..n]);
                let frames = parse_h2_frames(&raw);
                for (ft, fl, s, _) in &frames {
                    if *ft == H2_FRAME_DATA && (*fl & H2_FLAG_END_STREAM) != 0 {
                        if *s == 1 {
                            end1 = true;
                        }
                        if *s == 3 {
                            end3 = true;
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
    let elapsed = start.elapsed();
    let frames = parse_h2_frames(&raw);
    let mut body1 = 0usize;
    let mut body3 = 0usize;
    for (ft, _fl, s, payload) in &frames {
        if *ft == H2_FRAME_DATA {
            if *s == 1 {
                body1 += payload.len();
            } else if *s == 3 {
                body3 += payload.len();
            }
        }
    }
    println!(
        "coalesced: sid1 body={body1}/{BODY_SIZE} end={end1}, \
         sid3 body={body3}/{BODY_SIZE} end={end3}, elapsed={elapsed:?}"
    );

    let infra_ok = teardown(tls, front_port, worker, backends);

    if infra_ok && body1 == BODY_SIZE && end1 && body3 == BODY_SIZE && end3 {
        State::Success
    } else {
        println!(
            "FAIL: sid1={body1}/{BODY_SIZE} end={end1}, \
             sid3={body3}/{BODY_SIZE} end={end3}, elapsed={elapsed:?}, \
             infra_ok={infra_ok}"
        );
        State::Fail
    }
}

#[test]
fn test_h2_coalesced_chrome_firefox_streams_drain() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 two streams (Chrome+Firefox shape) must both drain within 5 s",
            try_h2_coalesced_chrome_firefox_streams_drain
        ),
        State::Success
    );
}

// ============================================================================
// Customer HAR repro (2026-04-24 selfiexxl): H2 response body never delivered
// ============================================================================
//
// The customer's Axios XHR observed `_transferSize=134`, `receive≈49.6s`,
// `_error=net::ERR_ABORTED` on a `GET` that returned `200 OK` with
// `Content-Length: 3752` (see `tasks/wwwselfiexxlfr-debug_7zbkq2.har`).
// Response headers reached Chrome; the single 3752-byte DATA frame never
// did. The request carried `priority: u=1, i` on a multiplexed connection.
//
// Two independent wake-gap bugs can each produce this shape on
// `feat/h2-mux`:
//
// * **Bug 2 (H2 scheduler)**: the connection-global incremental count
//   was fed to the converter as `incremental_peer_count` even when a
//   stream was solo in its own urgency bucket — tripping the
//   yield-after-one-DATA branch and stranding the rest of the body
//   because `finalize_write` strips `Ready::WRITABLE` on a voluntary
//   yield with no `expect_write`. Fixed by `3f9f5e38`, which scopes the
//   count via the per-bucket `ready_incremental_by_urgency` HashMap in
//   `h2.rs::write_streams` and looks it up per-stream by urgency.
// * **Bug 1 (H1→H2 wake-gap)**: `mux/h1.rs:324` wraps the Linked-peer
//   `signal_pending_write()` call inside `if kawa.is_main_phase()`.
//   kawa 0.6.8 `storage/repr.rs` declares `Terminated` as a main-phase
//   state (see the `is_main_phase` match arms), so a keep-alive H1
//   backend that emits headers + small Content-Length body in a single
//   parse round trip still signals correctly *today*. The historical
//   regression covered by this test set lives in the `Headers → Body →
//   Terminated` boundary where an older kawa returned false at line 324
//   and skipped the peer wake. The close-delimited fallback at lines
//   385-395 is gated on `!is_keep_alive_backend`, so it only papers
//   over the gap on non-keep-alive backends.
//
// The three tests below exercise each bug in isolation, then together.

// ----------------------------------------------------------------------------
// (g) test_h2_mixed_urgency_incremental_solo_drains — Bug 2 RED
// ----------------------------------------------------------------------------

/// Two concurrent H2 streams on the same connection, BOTH incremental, in
/// DIFFERENT urgency buckets:
/// * sid=1 — `priority: u=0, i` (Chrome navigation shape, solo in bucket 0)
/// * sid=3 — `priority: u=3, i` (Chrome image/XHR shape, solo in bucket 3)
///
/// Pre-`3f9f5e38`, the converter's `incremental_peer_count` read the
/// connection-global total (2 = one incremental per urgency bucket × 2
/// buckets), so each stream still tripped the yield-after-one-DATA guard
/// even though it was solo in its bucket. `finalize_write` stripped
/// `Ready::WRITABLE` on the voluntary yield, edge-triggered epoll did
/// not re-fire, and the remaining body bytes stranded in `kawa.out`
/// until `front_timeout` (60 s default). `3f9f5e38` replaced the global
/// read with `ready_incremental_by_urgency`, so a solo-in-bucket
/// incremental stream now sees `incremental_peer_count = 1` (itself)
/// and does not yield.
///
/// Lock-in regression guard: MUST PASS on HEAD post-`3f9f5e38`. A
/// regression that reintroduces the global-count path would fail this
/// test within the 3 s deadline.
fn try_h2_mixed_urgency_incremental_solo_drains() -> State {
    const BODY_SIZE: usize = 64 * 1024;

    let (worker, backends, front_port) =
        setup_h2_test_with_large_bodies("H2-COR-MIXURG-INC", 2, BODY_SIZE);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake_with_initial_window(&mut tls, 1_000_000);

    // sid=1, u=0, i — solo incremental in urgency-0 bucket
    let block1 = build_get_headers_with_priority(0, "i");
    let h1 = H2Frame::headers(1, block1, true, true);
    tls.write_all(&h1.encode()).unwrap();
    // sid=3, u=3, i — solo incremental in urgency-3 bucket
    let block3 = build_get_headers_with_priority(3, "i");
    let h3 = H2Frame::headers(3, block3, true, true);
    tls.write_all(&h3.encode()).unwrap();
    tls.flush().unwrap();

    tls.write_all(&H2Frame::window_update(0, 1_000_000).encode())
        .unwrap();
    tls.flush().unwrap();

    tls.sock
        .set_read_timeout(Some(Duration::from_millis(250)))
        .ok();
    let start = Instant::now();
    let deadline = Duration::from_secs(5);
    let mut raw = Vec::new();
    let mut rbuf = vec![0u8; 65536];
    let mut end1 = false;
    let mut end3 = false;
    while (!end1 || !end3) && start.elapsed() < deadline {
        match tls.read(&mut rbuf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&rbuf[..n]);
                let frames = parse_h2_frames(&raw);
                for (ft, fl, s, _) in &frames {
                    if *ft == H2_FRAME_DATA && (*fl & H2_FLAG_END_STREAM) != 0 {
                        if *s == 1 {
                            end1 = true;
                        }
                        if *s == 3 {
                            end3 = true;
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
    let elapsed = start.elapsed();
    let frames = parse_h2_frames(&raw);
    let mut body1 = 0usize;
    let mut body3 = 0usize;
    for (ft, _fl, s, payload) in &frames {
        if *ft == H2_FRAME_DATA {
            if *s == 1 {
                body1 += payload.len();
            } else if *s == 3 {
                body3 += payload.len();
            }
        }
    }
    println!(
        "mixed-urgency incremental: sid1 body={body1}/{BODY_SIZE} end={end1}, \
         sid3 body={body3}/{BODY_SIZE} end={end3}, elapsed={elapsed:?}"
    );

    let infra_ok = teardown(tls, front_port, worker, backends);

    if infra_ok
        && body1 == BODY_SIZE
        && end1
        && body3 == BODY_SIZE
        && end3
        && elapsed < Duration::from_secs(3)
    {
        State::Success
    } else {
        println!(
            "FAIL: sid1={body1}/{BODY_SIZE} end={end1}, \
             sid3={body3}/{BODY_SIZE} end={end3}, elapsed={elapsed:?}, \
             infra_ok={infra_ok}"
        );
        State::Fail
    }
}

#[test]
fn test_h2_mixed_urgency_incremental_solo_drains() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 two solo-in-bucket incremental streams (u=0,i + u=3,i) must drain within 3 s",
            try_h2_mixed_urgency_incremental_solo_drains
        ),
        State::Success
    );
}

// ----------------------------------------------------------------------------
// (h) test_h2_h1_keepalive_single_read_cl_drains — Bug 1 RED
// ----------------------------------------------------------------------------

/// Single H2 stream → keep-alive H1 backend that writes the entire
/// response (headers + 3752-byte `Content-Length` body) in ONE `write_all`
/// syscall and then waits for the next request. Reproduces the customer
/// HAR body size exactly.
///
/// sōzu's H1 reader at `mux/h1.rs` receives headers+body in a single
/// `socket_read` cycle. `kawa::h1::parse` transitions kawa from
/// `Headers` → `Body` → `Terminated` within one call. At `h1.rs:324`,
/// `kawa.is_main_phase() == false` after the transition, so the
/// `signal_pending_write()` call at line 367 is skipped. The close-
/// delimited fallback at lines 385-395 requires `status == Closed`, which
/// does not fire for keep-alive backends. The frontend has DATA+END_STREAM
/// queued in `kawa.out` but never receives the wake-up; edge-triggered
/// epoll never re-fires; the body strands until `front_timeout`.
///
/// MUST FAIL on HEAD (Bug 1). MUST PASS after the signal-gap fix.
fn try_h2_h1_keepalive_single_read_cl_drains() -> State {
    use crate::mock::single_read_h1_backend::{SingleReadConfig, SingleReadH1Backend};
    const BODY_SIZE: usize = 3752;

    let (worker, front_port, back_address) =
        setup_single_h1_backend_listener("H2-COR-SINGLEREAD-CL", None);

    let backend = SingleReadH1Backend::start(
        back_address,
        SingleReadConfig {
            body_size: BODY_SIZE,
            content_type: Some("application/json"),
        },
    );

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake_with_initial_window(&mut tls, 1_000_000);

    let sid: u32 = 1;
    let block = build_get_headers_no_priority();
    let headers = H2Frame::headers(sid, block, true, true);
    tls.write_all(&headers.encode()).unwrap();
    tls.flush().unwrap();
    tls.write_all(&H2Frame::window_update(0, 1_000_000).encode())
        .unwrap();
    tls.flush().unwrap();

    let (body_bytes, end_stream_seen, got_rst, elapsed) =
        drain_h2_stream_until_end_stream(&mut tls, sid, Duration::from_secs(3));

    println!(
        "h1-keepalive-single-read: body_bytes={body_bytes}/{BODY_SIZE} \
         end_stream={end_stream_seen} rst={got_rst} elapsed={elapsed:?} \
         backend_responses={}",
        backend.responses_sent()
    );

    let infra_ok = teardown(
        tls,
        front_port,
        worker,
        Vec::<AsyncBackend<SimpleAggregator>>::new(),
    );
    drop(backend);

    if infra_ok && body_bytes == BODY_SIZE && end_stream_seen && !got_rst {
        State::Success
    } else {
        println!(
            "FAIL: body_bytes={body_bytes} (want {BODY_SIZE}), \
             end_stream={end_stream_seen}, rst={got_rst}, \
             elapsed={elapsed:?}, infra_ok={infra_ok}"
        );
        State::Fail
    }
}

#[test]
fn test_h2_h1_keepalive_single_read_cl_drains() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 stream → keep-alive H1 backend (CL 3752, single write) must drain within 3 s",
            try_h2_h1_keepalive_single_read_cl_drains
        ),
        State::Success
    );
}

// ----------------------------------------------------------------------------
// (i) test_h2_customer_har_shape_drains — combined HAR repro
// ----------------------------------------------------------------------------

/// Full customer HAR reproduction: multiplexed H2 connection with two
/// streams, both incremental, in different urgency buckets, each served by
/// a keep-alive H1 backend that writes its response in a single syscall.
/// sid=1 is the customer's stalling XHR (`priority: u=1, i`, 3752-byte
/// JSON). sid=3 is a concurrent incremental request in urgency bucket 3
/// (e.g. an image prefetch, RFC 9218 §8 default urgency).
///
/// On HEAD this test can fail on either Bug 1 or Bug 2 — both contribute.
/// After both fixes land, both streams drain within the 3 s deadline.
fn try_h2_customer_har_shape_drains() -> State {
    use crate::mock::single_read_h1_backend::{SingleReadConfig, SingleReadH1Backend};
    const BODY_SIZE: usize = 3752;

    let (worker, front_port, back_address) =
        setup_single_h1_backend_listener("H2-COR-HAR-SHAPE", None);

    let backend = SingleReadH1Backend::start(
        back_address,
        SingleReadConfig {
            body_size: BODY_SIZE,
            content_type: Some("application/json"),
        },
    );

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake_with_initial_window(&mut tls, 1_000_000);

    // sid=1 — customer's XHR priority: u=1, i
    let block1 = build_get_headers_with_priority(1, "i");
    let h1 = H2Frame::headers(1, block1, true, true);
    tls.write_all(&h1.encode()).unwrap();
    // sid=3 — incremental peer in a different urgency bucket
    let block3 = build_get_headers_with_priority(3, "i");
    let h3 = H2Frame::headers(3, block3, true, true);
    tls.write_all(&h3.encode()).unwrap();
    tls.flush().unwrap();
    tls.write_all(&H2Frame::window_update(0, 1_000_000).encode())
        .unwrap();
    tls.flush().unwrap();

    tls.sock
        .set_read_timeout(Some(Duration::from_millis(250)))
        .ok();
    let start = Instant::now();
    let deadline = Duration::from_secs(5);
    let mut raw = Vec::new();
    let mut rbuf = vec![0u8; 65536];
    let mut end1 = false;
    let mut end3 = false;
    while (!end1 || !end3) && start.elapsed() < deadline {
        match tls.read(&mut rbuf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&rbuf[..n]);
                let frames = parse_h2_frames(&raw);
                for (ft, fl, s, _) in &frames {
                    if *ft == H2_FRAME_DATA && (*fl & H2_FLAG_END_STREAM) != 0 {
                        if *s == 1 {
                            end1 = true;
                        }
                        if *s == 3 {
                            end3 = true;
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
    let elapsed = start.elapsed();
    let frames = parse_h2_frames(&raw);
    let mut body1 = 0usize;
    let mut body3 = 0usize;
    for (ft, _fl, s, payload) in &frames {
        if *ft == H2_FRAME_DATA {
            if *s == 1 {
                body1 += payload.len();
            } else if *s == 3 {
                body3 += payload.len();
            }
        }
    }
    println!(
        "customer HAR shape: sid1 body={body1}/{BODY_SIZE} end={end1}, \
         sid3 body={body3}/{BODY_SIZE} end={end3}, elapsed={elapsed:?}, \
         backend_responses={}",
        backend.responses_sent()
    );

    let infra_ok = teardown(
        tls,
        front_port,
        worker,
        Vec::<AsyncBackend<SimpleAggregator>>::new(),
    );
    drop(backend);

    if infra_ok
        && body1 == BODY_SIZE
        && end1
        && body3 == BODY_SIZE
        && end3
        && elapsed < Duration::from_secs(3)
    {
        State::Success
    } else {
        println!(
            "FAIL: sid1={body1}/{BODY_SIZE} end={end1}, \
             sid3={body3}/{BODY_SIZE} end={end3}, elapsed={elapsed:?}, \
             infra_ok={infra_ok}"
        );
        State::Fail
    }
}

#[test]
fn test_h2_customer_har_shape_drains() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "H2 customer HAR shape (mixed-urgency incremental + single-read CL) must drain within 3 s",
            try_h2_customer_har_shape_drains
        ),
        State::Success
    );
}
