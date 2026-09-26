//! Benchmarks for the local metrics drain's per-request emission path
//! (issue #1548).
//!
//! `SessionMetrics::register_end_of_session` ends every request with this
//! sequence of `Subscriber::receive_metric` calls on the worker's
//! `Aggregator`: two cluster-labelled and two proxy-wide times, the six
//! `record_backend_metrics!` emissions (bytes in/out, response, connection and
//! header time, request count) and one `access_logs.count` carrying both
//! labels. This bench replays that sequence, against a drain already holding
//! `n` backends of one cluster, rotating over them so the average lookup
//! lands mid-way through the container.
//!
//! It also counts heap allocations per replayed session with a counting
//! global allocator, because the metric path's contract is zero allocation in
//! steady state and a wall-clock number alone cannot show a violation.
//!
//! Two cardinality levels are measured: `cluster` (the configured default,
//! which strips `backend_id` before the drain so the per-backend container is
//! never touched) and `backend` (both labels kept).

use std::{
    alloc::{GlobalAlloc, Layout, System},
    hint::black_box,
    sync::atomic::{AtomicUsize, Ordering},
};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use sozu_command_lib::config::MetricDetailLevel;
use sozu_lib::metrics::{Aggregator, MetricValue, Subscriber, names};

struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

const CLUSTER: &str = "app_fc55a197-4b8f-4cac-88a3-c172ad4fea25";
const BACKEND_COUNTS: [usize; 5] = [1, 10, 100, 1000, 10000];

fn backend_ids(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("{CLUSTER}-{i}")).collect()
}

/// One `register_end_of_session`, as the `time!`, `record_backend_metrics!`
/// and `incr!` macros hand it to the aggregator.
fn end_of_session(agg: &mut Aggregator, backend_id: &str) {
    let c = Some(CLUSTER);
    let b = Some(backend_id);
    agg.receive_metric(
        names::event_loop::REQUEST_TIME,
        c,
        None,
        MetricValue::Time(12),
    );
    agg.receive_metric(
        names::event_loop::SERVICE_TIME,
        c,
        None,
        MetricValue::Time(3),
    );
    agg.receive_metric(
        names::event_loop::REQUEST_TIME,
        None,
        None,
        MetricValue::Time(12),
    );
    agg.receive_metric(
        names::event_loop::SERVICE_TIME,
        None,
        None,
        MetricValue::Time(3),
    );
    agg.receive_metric(names::backend::BYTES_IN, c, b, MetricValue::Count(512));
    agg.receive_metric(names::backend::BYTES_OUT, c, b, MetricValue::Count(4096));
    agg.receive_metric(names::backend::RESPONSE_TIME, c, b, MetricValue::Time(9));
    agg.receive_metric(names::backend::CONNECTION_TIME, c, b, MetricValue::Time(1));
    agg.receive_metric(names::backend::HEADER_TIME, c, b, MetricValue::Time(7));
    agg.receive_metric(names::backend::REQUESTS, c, b, MetricValue::Count(1));
    agg.receive_metric(names::access_logs::COUNT, c, b, MetricValue::Count(1));
}

fn primed(detail: MetricDetailLevel, ids: &[String]) -> Aggregator {
    let mut agg = Aggregator::new(String::from("sozu"));
    agg.set_up_detail(detail);
    for id in ids {
        end_of_session(&mut agg, id);
    }
    agg
}

fn report_allocations() {
    for (label, detail) in [
        ("cluster", MetricDetailLevel::Cluster),
        ("backend", MetricDetailLevel::Backend),
    ] {
        for n in BACKEND_COUNTS {
            let ids = backend_ids(n);
            let mut agg = primed(detail, &ids);
            let sessions = 10_000;
            let before = ALLOCATIONS.load(Ordering::Relaxed);
            for i in 0..sessions {
                end_of_session(&mut agg, &ids[i % n]);
            }
            let after = ALLOCATIONS.load(Ordering::Relaxed);
            println!(
                "allocations_per_session detail={label} n={n}: {:.3}",
                (after - before) as f64 / sessions as f64
            );
        }
    }
}

fn bench_end_of_session(c: &mut Criterion) {
    report_allocations();
    for (label, detail) in [
        ("cluster", MetricDetailLevel::Cluster),
        ("backend", MetricDetailLevel::Backend),
    ] {
        let mut group = c.benchmark_group(format!("local_drain_end_of_session/{label}"));
        for n in BACKEND_COUNTS {
            let ids = backend_ids(n);
            let mut agg = primed(detail, &ids);
            let mut next = 0usize;
            group.bench_with_input(BenchmarkId::from_parameter(n), &n, |bencher, &n| {
                bencher.iter(|| {
                    end_of_session(black_box(&mut agg), black_box(&ids[next]));
                    next = (next + 1) % n;
                })
            });
        }
        group.finish();
    }
}

criterion_group!(benches, bench_end_of_session);
criterion_main!(benches);
