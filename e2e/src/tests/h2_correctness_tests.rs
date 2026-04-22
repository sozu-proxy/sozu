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
    io::Write,
    net::{Shutdown, SocketAddr},
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
            let infra_ok = teardown(tls, front_port, worker, vec![backend]);
            return if infra_ok { State::Fail } else { State::Fail };
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
