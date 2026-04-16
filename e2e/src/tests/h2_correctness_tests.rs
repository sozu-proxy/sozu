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

    // Expect RST_STREAM(PROTOCOL_ERROR) on stream 1 or GOAWAY(PROTOCOL_ERROR)
    let rst_streams = extract_rst_streams(&frames);
    let has_protocol_error_rst = rst_streams
        .iter()
        .any(|(sid, ec)| *sid == 1 && *ec == H2_ERROR_PROTOCOL_ERROR);
    let has_protocol_error_goaway = contains_goaway_with_error(&frames, H2_ERROR_PROTOCOL_ERROR);
    let rejected = has_protocol_error_rst || has_protocol_error_goaway;

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
