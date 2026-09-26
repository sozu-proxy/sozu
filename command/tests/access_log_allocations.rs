//! An ASCII access-log line costs no heap allocation in steady state.
//!
//! Every field of a `RequestRecord` is a borrow and the logger reuses one
//! `LoggerBuffer` for every record, so rendering a line has no reason to
//! allocate. This binary installs a counting global allocator whose counter is
//! thread-local — the logger is thread-local too, so the harness's other test
//! threads cannot pollute a measurement — emits a few warm-up lines to let the
//! reused buffers reach their size, then requires the next lines to allocate
//! nothing at all.
//!
//! The record exercises every rendered field that has an allocating shape to
//! avoid: both socket addresses, both ULIDs of the context, a frontend tag map
//! followed by a user agent that needs escaping, an HTTP status, and an
//! OpenTelemetry context. Two targets are measured, because they render
//! through different paths: `file://` formats straight into its
//! `MultiLineWriter`, `udp://` formats into the reused `LoggerBuffer` first.
//!
//! To SEE THIS RED, render the socket addresses with the public
//! `AsString::as_string_or` again in `InnerLogger::log_access`: each line then
//! allocates four times (a `String` per address, then its growth).

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    collections::BTreeMap,
    fs,
    net::{SocketAddr, UdpSocket},
    time::Duration,
};

use rusty_ulid::Ulid;
use sozu_command_lib::{
    info_access,
    logging::{COMPAT_LOGGER, CachedTags, EndpointRecord, LogContext, Logger, OpenTelemetry},
};

struct CountingAllocator;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

fn record_allocation() {
    // `try_with`: the slot may already be torn down while a thread exits.
    let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
}

// SAFETY: every method forwards to `System` unchanged; the only addition is a
// thread-local counter increment, which neither allocates nor unwinds.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_allocation();
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_allocation();
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record_allocation();
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

fn allocations() -> usize {
    ALLOCATIONS.with(Cell::get)
}

/// Lines emitted before measuring, so the reused buffers reach their size.
const WARM_UP: usize = 16;
/// Lines measured; the budget is zero for all of them together.
const MEASURED: usize = 64;

/// A user agent holding every byte the renderer replaces (space, `[`, `]`)
/// between spans it must copy verbatim.
const USER_AGENT: &str = "Mozilla/5.0 [X11; Linux x86_64] curl/8.18.0";
const ESCAPED_USER_AGENT: &str = "Mozilla/5.0_{X11;_Linux_x86_64}_curl/8.18.0";

/// Everything a line borrows, built before the measured window: generating a
/// ULID or building the tag map allocates, and must not be counted.
struct Fixture {
    session_id: Ulid,
    request_id: Ulid,
    session_address: SocketAddr,
    backend_address: SocketAddr,
    tags: CachedTags,
    otel: OpenTelemetry,
}

impl Fixture {
    fn new() -> Self {
        let mut tags = BTreeMap::new();
        tags.insert("owner".to_owned(), "team-a".to_owned());
        tags.insert("env".to_owned(), "prod".to_owned());
        Self {
            session_id: Ulid::from(0x0192_3A4B_5C6D_7E8F_9012_3456_7890_ABCD_u128),
            request_id: Ulid::from(0x0192_3A4B_5C6D_7E8F_9012_3456_7890_ABCE_u128),
            session_address: "127.0.0.1:49312".parse().expect("session address"),
            backend_address: "[::1]:8080".parse().expect("backend address"),
            tags: CachedTags::new(tags),
            otel: OpenTelemetry {
                trace_id: *b"c49db9fb0f3cfc3320168d35aa769c45",
                span_id: *b"d4f75b9c1e32aa03",
                parent_span_id: None,
            },
        }
    }

    /// The part of the line after the prompt (timestamp, pid, level, tag),
    /// which is the part this fixture controls.
    fn expected_body(&self) -> String {
        format!(
            "[{} {} cluster-1 backend-1] 127.0.0.1:49312 [::1]:8080 \
             1000μs/2000μs/3000μs/-/- 91 253 [env=prod, owner=team-a, user-agent={ESCAPED_USER_AGENT}] \
             Some(c49db9fb0f3cfc3320168d35aa769c45 d4f75b9c1e32aa03 -) HTTP \
             example.com GET /index.html 200 | H1::Complete",
            self.session_id, self.request_id,
        )
    }

    fn emit(&self) {
        info_access!(
            on_failure: { panic!("the access record could not be written") },
            message: Some("H1::Complete"),
            context: LogContext {
                session_id: self.session_id,
                request_id: Some(self.request_id),
                cluster_id: Some("cluster-1"),
                backend_id: Some("backend-1"),
            },
            session_address: Some(self.session_address),
            backend_address: Some(self.backend_address),
            protocol: "HTTP",
            endpoint: EndpointRecord::Http {
                method: Some("GET"),
                authority: Some("example.com"),
                path: Some("/index.html"),
                status: Some(200),
                reason: Some("OK"),
            },
            tags: Some(&self.tags),
            client_rtt: None,
            server_rtt: None,
            user_agent: Some(USER_AGENT),
            x_request_id: None,
            tls_version: None,
            tls_cipher: None,
            tls_sni: None,
            tls_alpn: None,
            xff_chain: None,
            service_time: Duration::from_millis(2),
            response_time: Some(Duration::from_millis(3)),
            request_time: Duration::from_millis(1),
            start_time_ns: None,
            bytes_in: 91,
            bytes_out: 253,
            otel: Some(&self.otel),
        );
    }
}

/// Emit the warm-up lines, then return how many allocations the measured
/// lines performed.
fn allocations_for_measured_lines(fixture: &Fixture) -> usize {
    for _ in 0..WARM_UP {
        fixture.emit();
    }
    let before = allocations();
    for _ in 0..MEASURED {
        fixture.emit();
    }
    allocations() - before
}

fn init_logger(access_logs_target: &str) {
    Logger::init(
        "ALLOC".to_owned(),
        "info",
        "stdout",
        false,
        Some(access_logs_target),
        None,
        None,
    )
    .expect("the logger initialises");
}

#[test]
fn a_file_access_log_line_does_not_allocate() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let access_path = dir.path().join("access.log");
    init_logger(&format!("file://{}", access_path.display()));
    let fixture = Fixture::new();

    let allocated = allocations_for_measured_lines(&fixture);

    log::Log::flush(&COMPAT_LOGGER);
    let content = fs::read_to_string(&access_path).expect("access log written");
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), WARM_UP + MEASURED, "one line per record");
    let expected = fixture.expected_body();
    for line in lines {
        let (_prompt, body) = line.split_once('\t').expect("a prompt, then a tab");
        assert_eq!(body, expected);
    }

    assert_eq!(
        allocated,
        0,
        "{MEASURED} file:// access-log lines allocated {allocated} times \
         ({:.2} per line), expected none",
        allocated as f64 / MEASURED as f64
    );
}

#[test]
fn a_udp_access_log_line_does_not_allocate() {
    let receiver = UdpSocket::bind("127.0.0.1:0").expect("bind the receiving socket");
    receiver
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("receive timeout");
    let port = receiver.local_addr().expect("receiver address").port();
    init_logger(&format!("udp://127.0.0.1:{port}"));
    let fixture = Fixture::new();

    let allocated = allocations_for_measured_lines(&fixture);

    let expected = format!("{}\n", fixture.expected_body());
    let mut datagram = [0u8; 2048];
    for _ in 0..WARM_UP + MEASURED {
        let len = receiver
            .recv(&mut datagram)
            .expect("one datagram per record");
        let line = std::str::from_utf8(&datagram[..len]).expect("UTF-8 line");
        let (_prompt, body) = line.split_once('\t').expect("a prompt, then a tab");
        assert_eq!(body, expected);
    }

    assert_eq!(
        allocated,
        0,
        "{MEASURED} udp:// access-log lines allocated {allocated} times \
         ({:.2} per line), expected none",
        allocated as f64 / MEASURED as f64
    );
}
