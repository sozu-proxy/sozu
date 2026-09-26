//! A `file://` log target batches records in a `MultiLineWriter` and writes
//! them out only once its buffer fills, which is what keeps it far below one
//! `write(2)` per record. The records still held in that buffer must reach the
//! file when the logger is flushed on a worker's way out; a worker that exits
//! with them still buffered is killed by the main process before its
//! thread-local logger drops, and they are lost.
//!
//! To SEE THIS RED, empty the body of `log::Log::flush` for `CompatLogger`
//! (the pre-fix `fn flush(&self) {}`): the file keeps nothing after the flush.

use std::{fs, time::Duration};

use rusty_ulid::Ulid;
use sozu_command_lib::{
    info, info_access,
    logging::{COMPAT_LOGGER, EndpointRecord, LogContext, Logger},
};

/// Few and short enough to stay far below the 4096-byte `MultiLineWriter`
/// capacity, so none of them is written before the flush.
const RECORDS: usize = 8;

fn lines_containing(path: &std::path::Path, marker: &str) -> usize {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains(marker))
        .count()
}

#[test]
fn flushing_the_logger_writes_out_every_buffered_file_record() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let main_path = dir.path().join("main.log");
    let access_path = dir.path().join("access.log");

    Logger::init(
        "FLUSH".to_owned(),
        "info",
        &format!("file://{}", main_path.display()),
        false,
        Some(&format!("file://{}", access_path.display())),
        None,
        None,
    )
    .expect("a file:// logger initialises");

    for i in 0..RECORDS {
        info!("main-record-{}", i);
        let path = format!("/access-record-{i}");
        info_access!(
            on_failure: { panic!("the access record could not be buffered") },
            message: None,
            context: LogContext {
                session_id: Ulid::generate(),
                request_id: None,
                cluster_id: None,
                backend_id: None,
            },
            session_address: None,
            backend_address: None,
            protocol: "HTTP",
            endpoint: EndpointRecord::Http {
                method: Some("GET"),
                authority: Some("localhost"),
                path: Some(&path),
                status: Some(200),
                reason: None,
            },
            tags: None,
            client_rtt: None,
            server_rtt: None,
            user_agent: None,
            x_request_id: None,
            tls_version: None,
            tls_cipher: None,
            tls_sni: None,
            tls_alpn: None,
            xff_chain: None,
            service_time: Duration::ZERO,
            response_time: None,
            request_time: Duration::ZERO,
            start_time_ns: None,
            bytes_in: 0,
            bytes_out: 0,
            otel: None,
        );
    }

    // Precondition: the records are buffered, not written. Without it the
    // assertions below would pass whether or not the flush does anything.
    assert_eq!(lines_containing(&main_path, "main-record-"), 0);
    assert_eq!(lines_containing(&access_path, "access-record-"), 0);

    log::Log::flush(&COMPAT_LOGGER);

    assert_eq!(lines_containing(&main_path, "main-record-"), RECORDS);
    assert_eq!(lines_containing(&access_path, "access-record-"), RECORDS);
}
