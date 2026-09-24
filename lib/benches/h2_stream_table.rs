//! Benchmarks for the H2 wire stream-table container choice (issue #1338).
//!
//! Context: `sozu_lib::protocol::mux::h2_stream_table::H2StreamTable::streams`
//! is the wire `StreamId -> GlobalStreamId` map. Issue #1338 asked whether it
//! should become a `BTreeMap` — the container four sibling maps already use on
//! determinism grounds (`H2FlowControl::pending_window_updates` and
//! `H2StreamTable`'s `stream_last_activity_at`, `stream_fc_stalled_since` and
//! `stream_fc_stalled_progress`) — or stay a `HashMap` with a seeded hasher.
//! It IS one now: this bench is the measurement that closed that question, and
//! it is kept so the crossover below can be re-measured rather than re-argued.
//!
//! Unlike those four, `streams` is read on the **per-frame** path, so the
//! determinism argument alone did not settle it. This bench measures the
//! access pattern that path actually produces. It decides nothing itself: it
//! reports the cost of each container under that mix.
//!
//! ## Containers compared
//!
//! 1. `hashmap_random_state` — `HashMap` with the std default `RandomState`.
//!    What `H2StreamTable::new` built before issue #1338 was closed.
//! 2. `btreemap` — what `H2StreamTable::new` builds today. Total order, no
//!    seed.
//! 3. `hashmap_fixed_seed` — `HashMap` with
//!    `BuildHasherDefault<DefaultHasher>`: the same SipHash-1-3 as the std
//!    default but with fixed keys, so iteration order is reproducible across
//!    restarts of one binary. This is the in-tree shape —
//!    `sozu_lib::protocol::udp::manager::UdpManager::affinity_hash` already
//!    builds a `DefaultHasher` directly — and needs no new crate. Note that
//!    `DefaultHasher`'s algorithm is explicitly unspecified across std
//!    versions, so its order is reproducible for one build and not a language
//!    guarantee the way `BTreeMap`'s is; that difference is not something a
//!    benchmark can show.
//!
//! ## Operation mix (read off the production call sites, not invented)
//!
//! Point lookups dominate, and they are the only operations whose complexity
//! class moves (`O(1)` -> `O(log n)`):
//!
//! - **One lookup per inbound frame.** `ConnectionH2::handle_data_frame`,
//!   `handle_headers_frame`, `handle_rst_stream_frame`,
//!   `handle_continuation_header_state` and `handle_header_state` each perform
//!   exactly one wire-map lookup; `handle_window_update_frame` performs up to
//!   two.
//! - **One lookup per stream per write pass, plus one full key walk.**
//!   `ConnectionH2::begin_scheduler_pass` hands `streams().keys().copied()` to
//!   `H2Scheduler::begin_pass`, which `extend`s an order buffer and sorts it by
//!   `(urgency, stream_id)`; `ConnectionH2::poll_write_target`'s prepare phase
//!   then looks the `GlobalStreamId` back up once per stream in that order.
//!   `begin_pass`'s readiness closure adds a second lookup per stream, but only
//!   for streams flagged incremental, so an all-incremental connection doubles
//!   this count.
//! - **One insert and one remove per stream lifetime**
//!   (`H2StreamTable::register` / `H2StreamTable::remove`) — amortized to
//!   nothing against the frame count.
//! - `len()`/`is_empty()` are `O(1)` in both containers, and the remaining
//!   iteration sites (`compute_stream_byte_totals`,
//!   `update_initial_window_size`, `ConnectionH2::close`,
//!   `handle_goaway_frame`, `any_stream_has_pending_back`, the `Debug` impl)
//!   already walk all `n` in both containers.
//!
//! So the four groups below are: `get` hit, `get` miss, steady-state churn,
//! and the composite write pass.
//!
//! ## Probe order
//!
//! `get_hit` and `get_miss` probe a **seeded shuffle** of their id set, not the
//! ascending sequence. Ascending probes walk a `BTreeMap` leaf at a time and
//! keep it L1-resident, which flatters the tree and tells us nothing about a
//! connection whose frames interleave across streams. The seed is fixed so the
//! permutation is identical on every run and between containers.
//!
//! `get_miss` additionally spreads its probes *through* the live key range
//! rather than above it: its table holds every other client-initiated id and it
//! probes the retired ids interleaved between them. Probing only ids above the
//! whole range would send every miss down the same rightmost `BTreeMap` leaf.
//!
//! The write pass deliberately does **not** shuffle: production looks streams
//! up in `order`'s sorted sequence, so that sequence is the faithful one.
//!
//! ## Why these `n`
//!
//! - `n = 8` — the expected steady-state occupancy, which `H2StreamTable::new`
//!   used to state as `HashMap::with_capacity(8)`. `BTreeMap` takes no
//!   capacity hint, so that statement now lives here instead of there.
//! - `n = 100` — `DEFAULT_MAX_CONCURRENT_STREAMS`, the advertised default cap.
//! - `n = 1000` — an operator-raised cap, an order of magnitude over default.
//! - `n = 10000` — `MAX_SAFE_CONCURRENT_STREAMS`, the value
//!   `H2ConnectionConfig`'s constructor clamps any larger request down to. The
//!   worst case the proxy can reach, not a realistic one.

use std::{
    collections::{BTreeMap, HashMap, hash_map::DefaultHasher},
    hash::BuildHasherDefault,
    hint::black_box,
};

use criterion::{
    BenchmarkGroup, BenchmarkId, Criterion, criterion_group, criterion_main, measurement::WallTime,
};
use rand::{SeedableRng, rngs::StdRng, seq::SliceRandom};

/// Mirrors `sozu_lib::protocol::mux::StreamId` — the wire stream identifier.
type StreamId = u32;

/// Mirrors `sozu_lib::protocol::mux::GlobalStreamId` — the slot index into
/// `Context::streams`.
type GlobalStreamId = usize;

/// Mirrors `sozu_lib::protocol::mux::h2::DEFAULT_MAX_CONCURRENT_STREAMS`, the
/// RFC 9113 §6.5.2 setting sozu advertises unless the operator overrides it.
const DEFAULT_MAX_CONCURRENT_STREAMS: usize = 100;

/// Mirrors the `MAX_SAFE_CONCURRENT_STREAMS` ceiling
/// `H2ConnectionConfig::new` clamps `h2_max_concurrent_streams` to.
const MAX_SAFE_CONCURRENT_STREAMS: usize = 10_000;

/// The expected steady-state stream count for the wire map.
/// `H2StreamTable::new` used to state it as `HashMap::with_capacity(8)`;
/// `BTreeMap` takes no capacity hint, so this constant carries it now.
const TYPICAL_CONCURRENT_STREAMS: usize = 8;

/// Number of distinct RFC 9218 §4.1 urgency buckets. Mirrors
/// `sozu_lib::protocol::mux::h2_scheduler::URGENCY_LEVELS`.
const URGENCY_LEVELS: u32 = 8;

/// Fixed seed for the probe-order shuffle, so every run and every container
/// sees the identical permutation.
const PROBE_SHUFFLE_SEED: u64 = 0x1338;

/// The stream counts every group is measured at.
const STREAM_COUNTS: [usize; 4] = [
    TYPICAL_CONCURRENT_STREAMS,
    DEFAULT_MAX_CONCURRENT_STREAMS,
    1_000,
    MAX_SAFE_CONCURRENT_STREAMS,
];

/// A `HashMap` whose hasher has fixed keys instead of a per-map random seed.
/// The seeded-hasher alternative issue #1338 weighs against `BTreeMap`.
type FixedSeedMap = HashMap<StreamId, GlobalStreamId, BuildHasherDefault<DefaultHasher>>;

/// The closed set of wire-map operations the production call sites perform.
/// Implemented by each candidate container so every group measures all three
/// through one code path.
trait StreamMap {
    /// Criterion function name for this container.
    const LABEL: &'static str;

    /// Build an empty table the way `H2StreamTable::new` does.
    fn new_table() -> Self;

    /// `H2StreamTable::register`'s wire-map half.
    fn insert(&mut self, id: StreamId, gid: GlobalStreamId);

    /// The per-frame lookup. Returns by value: production reads
    /// `Option<GlobalStreamId>`, a `Copy` index, not a reference.
    fn get(&self, id: StreamId) -> Option<GlobalStreamId>;

    /// `H2StreamTable::remove`'s wire-map half.
    fn remove(&mut self, id: StreamId);

    /// The `streams().contains_key(...)` presence check.
    fn contains_key(&self, id: StreamId) -> bool;

    /// `begin_scheduler_pass`' `streams().keys().copied()` walk, written as an
    /// `extend` into a reused buffer because that is what
    /// `H2Scheduler::begin_pass` does with the iterator.
    fn extend_keys(&self, out: &mut Vec<StreamId>);
}

impl StreamMap for HashMap<StreamId, GlobalStreamId> {
    const LABEL: &'static str = "hashmap_random_state";

    fn new_table() -> Self {
        HashMap::with_capacity(TYPICAL_CONCURRENT_STREAMS)
    }

    fn insert(&mut self, id: StreamId, gid: GlobalStreamId) {
        HashMap::insert(self, id, gid);
    }

    fn get(&self, id: StreamId) -> Option<GlobalStreamId> {
        HashMap::get(self, &id).copied()
    }

    fn remove(&mut self, id: StreamId) {
        HashMap::remove(self, &id);
    }

    fn contains_key(&self, id: StreamId) -> bool {
        HashMap::contains_key(self, &id)
    }

    fn extend_keys(&self, out: &mut Vec<StreamId>) {
        out.extend(self.keys().copied());
    }
}

impl StreamMap for FixedSeedMap {
    const LABEL: &'static str = "hashmap_fixed_seed";

    fn new_table() -> Self {
        FixedSeedMap::with_capacity_and_hasher(
            TYPICAL_CONCURRENT_STREAMS,
            BuildHasherDefault::default(),
        )
    }

    fn insert(&mut self, id: StreamId, gid: GlobalStreamId) {
        HashMap::insert(self, id, gid);
    }

    fn get(&self, id: StreamId) -> Option<GlobalStreamId> {
        HashMap::get(self, &id).copied()
    }

    fn remove(&mut self, id: StreamId) {
        HashMap::remove(self, &id);
    }

    fn contains_key(&self, id: StreamId) -> bool {
        HashMap::contains_key(self, &id)
    }

    fn extend_keys(&self, out: &mut Vec<StreamId>) {
        out.extend(self.keys().copied());
    }
}

impl StreamMap for BTreeMap<StreamId, GlobalStreamId> {
    const LABEL: &'static str = "btreemap";

    fn new_table() -> Self {
        BTreeMap::new()
    }

    fn insert(&mut self, id: StreamId, gid: GlobalStreamId) {
        BTreeMap::insert(self, id, gid);
    }

    fn get(&self, id: StreamId) -> Option<GlobalStreamId> {
        BTreeMap::get(self, &id).copied()
    }

    fn remove(&mut self, id: StreamId) {
        BTreeMap::remove(self, &id);
    }

    fn contains_key(&self, id: StreamId) -> bool {
        BTreeMap::contains_key(self, &id)
    }

    fn extend_keys(&self, out: &mut Vec<StreamId>) {
        out.extend(self.keys().copied());
    }
}

/// The `i`-th client-initiated wire stream id. RFC 9113 §5.1.1:
/// client-initiated ids are odd, and an id is never reused.
fn stream_id_at(index: usize) -> StreamId {
    (index as StreamId) * 2 + 1
}

/// A table holding `n` live streams with the ids a connection would have
/// accumulated from a cold start.
fn build_table<M: StreamMap>(n: usize) -> M {
    let mut table = M::new_table();
    for i in 0..n {
        table.insert(stream_id_at(i), i);
    }
    table
}

/// A fixed, reproducible permutation of `ids` — the interleaved probe order a
/// multiplexed connection produces, rather than the ascending walk that keeps
/// one `BTreeMap` leaf hot.
fn shuffled(mut ids: Vec<StreamId>) -> Vec<StreamId> {
    let mut rng = StdRng::seed_from_u64(PROBE_SHUFFLE_SEED);
    ids.shuffle(&mut rng);
    ids
}

/// RFC 9218 §4.1 urgency, all streams at the default. This is the
/// non-incremental fast path `H2Scheduler::begin_pass` documents, and it hands
/// a `BTreeMap`-sourced order buffer an already-ascending sort key.
fn urgency_uniform(_id: StreamId) -> u8 {
    3
}

/// RFC 9218 §4.1 urgency spread across all eight buckets, mirroring the
/// `varied` distribution in the `h2_scheduling` bench. Under this shape the
/// sort has real work to do whichever container produced the keys.
fn urgency_varied(id: StreamId) -> u8 {
    ((id / 2) % URGENCY_LEVELS) as u8
}

// ─── Groups 1 and 2: the per-frame lookup ───────────────────────────────

/// One `get` on a live stream — the cost `handle_data_frame` and
/// `handle_headers_frame` pay for every frame they parse.
fn run_get_hit<M: StreamMap>(group: &mut BenchmarkGroup<'_, WallTime>, n: usize) {
    let table = build_table::<M>(n);
    let live = shuffled((0..n).map(stream_id_at).collect());
    group.bench_with_input(BenchmarkId::new(M::LABEL, format!("n={n}")), &n, |b, _| {
        let mut cursor = 0usize;
        let mut acc = 0usize;
        b.iter(|| {
            let id = live[cursor];
            cursor += 1;
            if cursor == live.len() {
                cursor = 0;
            }
            acc = acc.wrapping_add(table.get(id).unwrap_or(0));
            black_box(acc)
        });
    });
}

/// One `contains_key` that misses — the shape of a frame arriving for a stream
/// that has already been retired, as `handle_rst_stream_frame` and a late DATA
/// frame both produce.
fn run_get_miss<M: StreamMap>(group: &mut BenchmarkGroup<'_, WallTime>, n: usize) {
    // Live streams occupy every other client-initiated id, and the probed ids
    // are the already-retired ones interleaved between them. Probing ids above
    // the whole live range instead would send every miss down the same
    // rightmost `BTreeMap` leaf and keep it cache-hot, which measures the tree
    // at its most flattering rather than at the spread a real connection has.
    let mut table = M::new_table();
    for i in 0..n {
        table.insert(stream_id_at(2 * i), i);
    }
    let absent = shuffled((0..n).map(|i| stream_id_at(2 * i + 1)).collect());
    group.bench_with_input(BenchmarkId::new(M::LABEL, format!("n={n}")), &n, |b, _| {
        let mut cursor = 0usize;
        let mut hits = 0usize;
        b.iter(|| {
            let id = absent[cursor];
            cursor += 1;
            if cursor == absent.len() {
                cursor = 0;
            }
            hits += usize::from(table.contains_key(id));
            black_box(hits)
        });
    });
}

// ─── Group 3: steady-state churn ────────────────────────────────────────

/// One stream completing and one opening, plus the lookup its next frame
/// costs. Occupancy stays at `n`, and ids advance monotonically because RFC
/// 9113 §5.1.1 forbids reuse — so this measures insert and remove against a
/// growing key range rather than against a recycled one.
///
/// The id cursors use `wrapping_add` purely so a long criterion run cannot
/// panic on `u32` overflow; at two ids per iteration the wrap point is ~2.1
/// billion iterations away, far beyond any sample this bench takes.
fn run_churn<M: StreamMap>(group: &mut BenchmarkGroup<'_, WallTime>, n: usize) {
    group.bench_with_input(BenchmarkId::new(M::LABEL, format!("n={n}")), &n, |b, _| {
        let mut table = build_table::<M>(n);
        let mut oldest = stream_id_at(0);
        let mut next = stream_id_at(n);
        let mut gid = n;
        let mut acc = 0usize;
        b.iter(|| {
            table.remove(oldest);
            oldest = oldest.wrapping_add(2);
            table.insert(next, gid);
            acc = acc.wrapping_add(table.get(next).unwrap_or(0));
            next = next.wrapping_add(2);
            gid = gid.wrapping_add(1);
            black_box(acc)
        });
    });
}

// ─── Group 4: the composite write pass ──────────────────────────────────

/// The whole of one writable cycle's container interaction: walk every key
/// into the scheduler's order buffer the way `begin_scheduler_pass` does, sort
/// it by `(urgency, stream_id)` as `H2Scheduler::begin_pass` does, then look
/// the `GlobalStreamId` back up for every stream in that order, as
/// `poll_write_target`'s prepare phase does.
///
/// Measured under both urgency distributions, because they stress the sort
/// differently: under `uniform` a `BTreeMap` hands the sort an already-ascending
/// key sequence, which Rust's adaptive stable sort finishes in `O(n)`; under
/// `varied` neither container's key order matches the sort order.
fn run_write_pass<M: StreamMap>(
    group: &mut BenchmarkGroup<'_, WallTime>,
    n: usize,
    distribution: &str,
    urgency: fn(StreamId) -> u8,
) {
    let table = build_table::<M>(n);
    group.bench_with_input(
        BenchmarkId::new(M::LABEL, format!("n={n}/{distribution}")),
        &n,
        |b, _| {
            let mut order: Vec<StreamId> = Vec::with_capacity(n);
            let mut acc = 0usize;
            b.iter(|| {
                order.clear();
                table.extend_keys(&mut order);
                order.sort_by_cached_key(|id| (urgency(*id), *id));
                for &id in order.iter() {
                    acc = acc.wrapping_add(table.get(id).unwrap_or(0));
                }
                black_box(acc)
            });
        },
    );
}

// ─── Wiring ─────────────────────────────────────────────────────────────

fn bench_get_hit(c: &mut Criterion) {
    let mut group = c.benchmark_group("h2_stream_table_get_hit");
    for n in STREAM_COUNTS {
        run_get_hit::<HashMap<StreamId, GlobalStreamId>>(&mut group, n);
        run_get_hit::<FixedSeedMap>(&mut group, n);
        run_get_hit::<BTreeMap<StreamId, GlobalStreamId>>(&mut group, n);
    }
    group.finish();
}

fn bench_get_miss(c: &mut Criterion) {
    let mut group = c.benchmark_group("h2_stream_table_get_miss");
    for n in STREAM_COUNTS {
        run_get_miss::<HashMap<StreamId, GlobalStreamId>>(&mut group, n);
        run_get_miss::<FixedSeedMap>(&mut group, n);
        run_get_miss::<BTreeMap<StreamId, GlobalStreamId>>(&mut group, n);
    }
    group.finish();
}

fn bench_churn(c: &mut Criterion) {
    let mut group = c.benchmark_group("h2_stream_table_churn");
    for n in STREAM_COUNTS {
        run_churn::<HashMap<StreamId, GlobalStreamId>>(&mut group, n);
        run_churn::<FixedSeedMap>(&mut group, n);
        run_churn::<BTreeMap<StreamId, GlobalStreamId>>(&mut group, n);
    }
    group.finish();
}

fn bench_write_pass(c: &mut Criterion) {
    let mut group = c.benchmark_group("h2_stream_table_write_pass");
    for n in STREAM_COUNTS {
        for (distribution, urgency) in [
            ("uniform", urgency_uniform as fn(StreamId) -> u8),
            ("varied", urgency_varied as fn(StreamId) -> u8),
        ] {
            run_write_pass::<HashMap<StreamId, GlobalStreamId>>(
                &mut group,
                n,
                distribution,
                urgency,
            );
            run_write_pass::<FixedSeedMap>(&mut group, n, distribution, urgency);
            run_write_pass::<BTreeMap<StreamId, GlobalStreamId>>(
                &mut group,
                n,
                distribution,
                urgency,
            );
        }
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_get_hit,
    bench_get_miss,
    bench_churn,
    bench_write_pass
);
criterion_main!(benches);
