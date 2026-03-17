//! Benchmarks comparing `format!()` vs `Vec<u8>` + `write!()` + `itoa`
//! for building HTTP header values in the `on_headers` hot path.
//!
//! Context: <https://github.com/sozu-proxy/sozu/pull/1200>

use std::io::Write as _;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};

// ---------------------------------------------------------------------------
// Helpers (optimized path from PR #1200)
// ---------------------------------------------------------------------------

fn write_forwarded_for_by(buf: &mut Vec<u8>, peer_ip: IpAddr, peer_port: u16, public_ip: IpAddr) {
    buf.extend_from_slice(b";for=\"");
    let _ = write!(buf, "{peer_ip}");
    buf.push(b':');
    let mut port_buf = itoa::Buffer::new();
    buf.extend_from_slice(port_buf.format(peer_port).as_bytes());
    buf.extend_from_slice(b"\";by=");
    match public_ip {
        IpAddr::V4(_) => {
            let _ = write!(buf, "{public_ip}");
        }
        IpAddr::V6(_) => {
            buf.push(b'"');
            let _ = write!(buf, "{public_ip}");
            buf.push(b'"');
        }
    }
}

fn write_forwarded_suffix(
    buf: &mut Vec<u8>,
    proto: &str,
    peer_ip: IpAddr,
    peer_port: u16,
    public_ip: IpAddr,
) {
    buf.extend_from_slice(b", proto=");
    buf.extend_from_slice(proto.as_bytes());
    write_forwarded_for_by(buf, peer_ip, peer_port, public_ip);
}

// ---------------------------------------------------------------------------
// Test fixtures
// ---------------------------------------------------------------------------

struct Fixture {
    name: &'static str,
    peer_ip: IpAddr,
    peer_port: u16,
    public_ip: IpAddr,
    public_port: u16,
    existing_x_forwarded_for: &'static str,
    existing_forwarded: &'static str,
    proto: &'static str,
    sticky_name: &'static str,
    sticky_session: &'static str,
}

fn fixtures() -> Vec<Fixture> {
    vec![
        Fixture {
            name: "ipv4",
            peer_ip: IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)),
            peer_port: 54321,
            public_ip: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            public_port: 8443,
            existing_x_forwarded_for: "203.0.113.50",
            existing_forwarded: "proto=https;for=\"203.0.113.50:12345\";by=10.0.0.1",
            proto: "https",
            sticky_name: "SERVERID",
            sticky_session: "srv-backend-01-abc123",
        },
        Fixture {
            name: "ipv6",
            peer_ip: IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)),
            peer_port: 54321,
            public_ip: IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1)),
            public_port: 443,
            existing_x_forwarded_for: "2001:db8::99",
            existing_forwarded: "proto=https;for=\"2001:db8::99:12345\";by=\"fd00::1\"",
            proto: "https",
            sticky_name: "SERVERID",
            sticky_session: "srv-backend-02-def456",
        },
    ]
}

// ---------------------------------------------------------------------------
// Benchmark: X-Forwarded-For header value
// ---------------------------------------------------------------------------

fn bench_x_forwarded_for(c: &mut Criterion) {
    let mut group = c.benchmark_group("x_forwarded_for");

    for f in &fixtures() {
        // --- format!() baseline ---
        group.bench_with_input(BenchmarkId::new("format", f.name), f, |b, f| {
            b.iter(|| {
                let value = f.existing_x_forwarded_for;
                let peer_ip = black_box(f.peer_ip);
                let result = format!("{value}, {peer_ip}");
                black_box(result.into_bytes().into_boxed_slice());
            });
        });

        // --- Vec + write!() optimized ---
        group.bench_with_input(BenchmarkId::new("vec_write", f.name), f, |b, f| {
            b.iter(|| {
                let mut buf = Vec::with_capacity(128);
                buf.extend_from_slice(black_box(f.existing_x_forwarded_for.as_bytes()));
                let _ = write!(buf, ", {}", black_box(f.peer_ip));
                black_box(buf.into_boxed_slice());
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: Forwarded header value
// ---------------------------------------------------------------------------

fn bench_forwarded(c: &mut Criterion) {
    let mut group = c.benchmark_group("forwarded");

    for f in &fixtures() {
        // --- format!() baseline ---
        group.bench_with_input(
            BenchmarkId::new("format", f.name),
            f,
            |b, f| {
                b.iter(|| {
                    let value = f.existing_forwarded;
                    let proto = black_box(f.proto);
                    let peer_ip = black_box(f.peer_ip);
                    let peer_port = black_box(f.peer_port);
                    let public_ip = black_box(f.public_ip);
                    let result = match public_ip {
                        IpAddr::V4(_) => {
                            format!(
                                "{value}, proto={proto};for=\"{peer_ip}:{peer_port}\";by={public_ip}"
                            )
                        }
                        IpAddr::V6(_) => {
                            format!(
                                "{value}, proto={proto};for=\"{peer_ip}:{peer_port}\";by=\"{public_ip}\""
                            )
                        }
                    };
                    black_box(result.into_bytes().into_boxed_slice());
                });
            },
        );

        // --- Vec + write!() optimized ---
        group.bench_with_input(BenchmarkId::new("vec_write", f.name), f, |b, f| {
            b.iter(|| {
                let mut buf = Vec::with_capacity(128);
                buf.extend_from_slice(black_box(f.existing_forwarded.as_bytes()));
                write_forwarded_suffix(
                    &mut buf,
                    black_box(f.proto),
                    black_box(f.peer_ip),
                    black_box(f.peer_port),
                    black_box(f.public_ip),
                );
                black_box(buf.into_boxed_slice());
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: X-Forwarded-Port header value
// ---------------------------------------------------------------------------

fn bench_x_forwarded_port(c: &mut Criterion) {
    let mut group = c.benchmark_group("x_forwarded_port");

    for f in &fixtures() {
        // --- to_string() baseline ---
        group.bench_with_input(BenchmarkId::new("to_string", f.name), f, |b, f| {
            b.iter(|| {
                let port = black_box(f.public_port);
                let result = port.to_string();
                black_box(result.into_bytes().into_boxed_slice());
            });
        });

        // --- itoa::Buffer optimized ---
        group.bench_with_input(BenchmarkId::new("itoa", f.name), f, |b, f| {
            b.iter(|| {
                let mut port_buf = itoa::Buffer::new();
                let port_str = port_buf.format(black_box(f.public_port));
                // from_slice copies into a new Box<[u8]>, matching Store::from_slice behavior
                black_box(port_str.as_bytes().to_vec().into_boxed_slice());
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: Set-Cookie header value
// ---------------------------------------------------------------------------

fn bench_set_cookie(c: &mut Criterion) {
    let mut group = c.benchmark_group("set_cookie");

    for f in &fixtures() {
        // --- format!() baseline ---
        group.bench_with_input(BenchmarkId::new("format", f.name), f, |b, f| {
            b.iter(|| {
                let sticky_name = black_box(f.sticky_name);
                let sticky_session = black_box(f.sticky_session);
                let result = format!("{sticky_name}={sticky_session}; Path=/");
                black_box(result.into_bytes().into_boxed_slice());
            });
        });

        // --- Vec + extend_from_slice optimized ---
        group.bench_with_input(BenchmarkId::new("vec_extend", f.name), f, |b, f| {
            b.iter(|| {
                let sticky_name = black_box(f.sticky_name);
                let sticky_session = black_box(f.sticky_session);
                let mut buf = Vec::with_capacity(sticky_name.len() + 1 + sticky_session.len() + 8);
                buf.extend_from_slice(sticky_name.as_bytes());
                buf.push(b'=');
                buf.extend_from_slice(sticky_session.as_bytes());
                buf.extend_from_slice(b"; Path=/");
                black_box(buf.into_boxed_slice());
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: Full pipeline (all headers combined, simulating one request)
// ---------------------------------------------------------------------------

fn bench_full_pipeline(c: &mut Criterion) {
    let mut group = c.benchmark_group("full_pipeline");

    for f in &fixtures() {
        // --- format!() baseline (simulates old on_request_headers) ---
        group.bench_with_input(
            BenchmarkId::new("format", f.name),
            f,
            |b, f| {
                b.iter(|| {
                    let peer_ip = black_box(f.peer_ip);
                    let peer_port = black_box(f.peer_port);
                    let public_ip = black_box(f.public_ip);
                    let public_port = black_box(f.public_port);
                    let proto = black_box(f.proto);
                    let existing_xff = black_box(f.existing_x_forwarded_for);
                    let existing_fwd = black_box(f.existing_forwarded);
                    let sticky_name = black_box(f.sticky_name);
                    let sticky_session = black_box(f.sticky_session);

                    // X-Forwarded-For
                    let xff = format!("{existing_xff}, {peer_ip}");
                    let xff_store = xff.into_bytes().into_boxed_slice();

                    // Forwarded
                    let fwd = match public_ip {
                        IpAddr::V4(_) => format!(
                            "{existing_fwd}, proto={proto};for=\"{peer_ip}:{peer_port}\";by={public_ip}"
                        ),
                        IpAddr::V6(_) => format!(
                            "{existing_fwd}, proto={proto};for=\"{peer_ip}:{peer_port}\";by=\"{public_ip}\""
                        ),
                    };
                    let fwd_store = fwd.into_bytes().into_boxed_slice();

                    // X-Forwarded-Port
                    let port_str = public_port.to_string();
                    let port_store = port_str.into_bytes().into_boxed_slice();

                    // Set-Cookie
                    let cookie = format!("{sticky_name}={sticky_session}; Path=/");
                    let cookie_store = cookie.into_bytes().into_boxed_slice();

                    black_box((xff_store, fwd_store, port_store, cookie_store));
                });
            },
        );

        // --- Vec + write!() + itoa optimized (simulates new on_request_headers) ---
        group.bench_with_input(BenchmarkId::new("vec_write", f.name), f, |b, f| {
            b.iter(|| {
                let peer_ip = black_box(f.peer_ip);
                let peer_port = black_box(f.peer_port);
                let public_ip = black_box(f.public_ip);
                let public_port = black_box(f.public_port);
                let proto = black_box(f.proto);
                let existing_xff = black_box(f.existing_x_forwarded_for);
                let existing_fwd = black_box(f.existing_forwarded);
                let sticky_name = black_box(f.sticky_name);
                let sticky_session = black_box(f.sticky_session);

                // Shared buffer, reused via take()
                let mut hdr_buf = Vec::with_capacity(128);

                // X-Forwarded-For
                hdr_buf.extend_from_slice(existing_xff.as_bytes());
                let _ = write!(hdr_buf, ", {peer_ip}");
                let xff_store = std::mem::take(&mut hdr_buf).into_boxed_slice();

                // Forwarded
                hdr_buf.extend_from_slice(existing_fwd.as_bytes());
                write_forwarded_suffix(&mut hdr_buf, proto, peer_ip, peer_port, public_ip);
                let fwd_store = std::mem::take(&mut hdr_buf).into_boxed_slice();

                // X-Forwarded-Port
                let mut port_buf = itoa::Buffer::new();
                let port_str = port_buf.format(public_port);
                let port_store = port_str.as_bytes().to_vec().into_boxed_slice();

                // Set-Cookie
                let mut cookie_buf =
                    Vec::with_capacity(sticky_name.len() + 1 + sticky_session.len() + 8);
                cookie_buf.extend_from_slice(sticky_name.as_bytes());
                cookie_buf.push(b'=');
                cookie_buf.extend_from_slice(sticky_session.as_bytes());
                cookie_buf.extend_from_slice(b"; Path=/");
                let cookie_store = cookie_buf.into_boxed_slice();

                black_box((xff_store, fwd_store, port_store, cookie_store));
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_x_forwarded_for,
    bench_forwarded,
    bench_x_forwarded_port,
    bench_set_cookie,
    bench_full_pipeline,
);
criterion_main!(benches);
