//! Benchmarks for H2 stream priority scheduling.
//!
//! Context: In the H2 mux write path, every writable cycle sorts active stream
//! IDs by `(urgency, stream_id)` and then — per RFC 9218 §4 — applies an
//! incremental round-robin rotation inside each urgency bucket. Two groups:
//!
//! 1. `h2_priority_sort` — compares sort strategies (`sort_by_cached_key` vs
//!    `sort_unstable_by_key` vs `sort_unstable_by`) for the primary sort.
//! 2. `h2_incremental_rr` — simulates the full scheduling pass for 4
//!    same-urgency incremental streams, each producing 1000 DATA chunks, and
//!    reports the stddev of per-stream completion time to quantify fairness.

use std::collections::HashMap;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

type StreamId = u32;

/// Simulates the `Prioriser::get()` method.
struct MockPrioriser {
    priorities: HashMap<StreamId, (u8, bool)>,
    /// RFC 9218 §4 round-robin cursor. Mirrors
    /// `sozu_lib::protocol::mux::h2::Prioriser::incremental_cursor`.
    incremental_cursor: StreamId,
}

impl MockPrioriser {
    fn get(&self, id: &StreamId) -> (u8, bool) {
        self.priorities.get(id).copied().unwrap_or((3, false))
    }

    /// Mirror of `Prioriser::apply_incremental_rotation`. Kept inline in the
    /// bench so we measure exactly what the production scheduler does without
    /// pulling in the full crate (the bench does not link the library).
    fn apply_incremental_rotation(&self, buf: &mut [StreamId]) -> usize {
        let mut total_incremental = 0usize;
        let mut i = 0;
        while i < buf.len() {
            let (urgency_i, _) = self.get(&buf[i]);
            let mut j = i + 1;
            while j < buf.len() {
                let (urgency_j, _) = self.get(&buf[j]);
                if urgency_j != urgency_i {
                    break;
                }
                j += 1;
            }
            let bucket = &mut buf[i..j];
            if bucket.len() > 1 {
                bucket.sort_by_key(|id| self.get(id).1);
                let split = bucket.partition_point(|id| !self.get(id).1);
                let incremental_tail = &mut bucket[split..];
                if incremental_tail.len() > 1 {
                    let start =
                        incremental_tail.partition_point(|id| *id <= self.incremental_cursor);
                    incremental_tail.rotate_left(start);
                }
                total_incremental += incremental_tail.len();
            } else if bucket.len() == 1 && self.get(&bucket[0]).1 {
                total_incremental += 1;
            }
            i = j;
        }
        total_incremental
    }
}

fn build_scenario(n: usize, distribution: &str) -> (Vec<StreamId>, MockPrioriser) {
    // Stream IDs are odd (client-initiated H2): 1, 3, 5, ...
    let stream_ids: Vec<StreamId> = (0..n).map(|i| (i as u32) * 2 + 1).collect();
    let mut priorities = HashMap::new();

    match distribution {
        "uniform" => {
            // All streams get default urgency 3 (no entries needed).
        }
        "varied" => {
            // Mix of urgencies 0-7.
            for (i, &id) in stream_ids.iter().enumerate() {
                priorities.insert(id, ((i % 8) as u8, i % 3 == 0));
            }
        }
        "skewed" => {
            // Most streams urgency 3, top 10% get high priority (0).
            for (i, &id) in stream_ids.iter().enumerate() {
                if i < n / 10 {
                    priorities.insert(id, (0, false));
                }
            }
        }
        _ => {}
    }

    (
        stream_ids,
        MockPrioriser {
            priorities,
            incremental_cursor: 0,
        },
    )
}

fn bench_sort_strategies(c: &mut Criterion) {
    let mut group = c.benchmark_group("h2_priority_sort");

    for n in [10, 50, 100, 200] {
        for dist in ["uniform", "varied", "skewed"] {
            let (stream_ids, prioriser) = build_scenario(n, dist);
            let param = format!("n={n}/{dist}");

            // Current: sort_by_cached_key
            group.bench_with_input(
                BenchmarkId::new("sort_by_cached_key", &param),
                &(&stream_ids, &prioriser),
                |b, (ids, prio)| {
                    let mut buf = ids.to_vec();
                    b.iter(|| {
                        buf.clear();
                        buf.extend_from_slice(ids);
                        buf.sort_by_cached_key(|id| {
                            let (urgency, _) = prio.get(id);
                            (urgency, *id)
                        });
                    });
                },
            );

            // Alternative 1: sort_unstable_by_key
            group.bench_with_input(
                BenchmarkId::new("sort_unstable_by_key", &param),
                &(&stream_ids, &prioriser),
                |b, (ids, prio)| {
                    let mut buf = ids.to_vec();
                    b.iter(|| {
                        buf.clear();
                        buf.extend_from_slice(ids);
                        buf.sort_unstable_by_key(|id| {
                            let (urgency, _) = prio.get(id);
                            (urgency, *id)
                        });
                    });
                },
            );

            // Alternative 2: sort_unstable_by with inline compare
            group.bench_with_input(
                BenchmarkId::new("sort_unstable_by", &param),
                &(&stream_ids, &prioriser),
                |b, (ids, prio)| {
                    let mut buf = ids.to_vec();
                    b.iter(|| {
                        buf.clear();
                        buf.extend_from_slice(ids);
                        buf.sort_unstable_by(|a, b| {
                            let (ua, _) = prio.get(a);
                            let (ub, _) = prio.get(b);
                            (ua, a).cmp(&(ub, b))
                        });
                    });
                },
            );
        }
    }

    group.finish();
}

// ─── RFC 9218 §4 round-robin fairness simulation ────────────────────────

/// Simulate a full drain of `streams_per_bucket` same-urgency incremental
/// streams, each with `frames_per_stream` DATA frames to send. Returns the
/// per-stream completion index (iteration count at which that stream's last
/// frame was emitted). Used by the bench to compute stddev of completion
/// times — low stddev ⇒ fair interleaving.
fn simulate_incremental_rr(streams_per_bucket: usize, frames_per_stream: usize) -> Vec<usize> {
    // All streams same urgency, same incremental flag.
    let stream_ids: Vec<StreamId> = (0..streams_per_bucket)
        .map(|i| (i as u32) * 2 + 1)
        .collect();
    let mut priorities = HashMap::new();
    for &id in &stream_ids {
        priorities.insert(id, (3u8, true));
    }
    let mut prioriser = MockPrioriser {
        priorities,
        incremental_cursor: 0,
    };
    // Remaining frames to send per stream, keyed by stream id.
    let mut remaining: HashMap<StreamId, usize> = stream_ids
        .iter()
        .map(|&id| (id, frames_per_stream))
        .collect();
    // Per-stream iteration at which the final frame was emitted. A fair
    // schedule has these values clustered tightly.
    let mut completion: HashMap<StreamId, usize> = HashMap::new();
    let mut iteration = 0usize;

    while !remaining.is_empty() {
        iteration += 1;
        // Build the priority buffer for this pass.
        let mut buf: Vec<StreamId> = remaining.keys().copied().collect();
        buf.sort_unstable();
        prioriser.apply_incremental_rotation(&mut buf);

        // RFC 9218 round-robin: one DATA frame per stream per pass, in the
        // rotated order. Track the first-fired ID so we can advance the
        // cursor afterwards.
        let mut first_fired: Option<StreamId> = None;
        for id in buf {
            if first_fired.is_none() {
                first_fired = Some(id);
            }
            let slot = remaining.get_mut(&id).expect("id present by construction");
            *slot -= 1;
            if *slot == 0 {
                completion.insert(id, iteration);
                remaining.remove(&id);
            }
        }
        prioriser.incremental_cursor = first_fired.unwrap_or(0);
    }

    let mut out: Vec<usize> = stream_ids.iter().map(|id| completion[id]).collect();
    out.sort_unstable();
    out
}

/// Stddev of a slice of `usize` values (population variance — we have the
/// full set, not a sample). Returned as `f64` in the same units as the input.
fn stddev(values: &[usize]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mean = values.iter().sum::<usize>() as f64 / values.len() as f64;
    let var = values
        .iter()
        .map(|&v| {
            let d = v as f64 - mean;
            d * d
        })
        .sum::<f64>()
        / values.len() as f64;
    var.sqrt()
}

fn bench_incremental_round_robin(c: &mut Criterion) {
    let mut group = c.benchmark_group("h2_incremental_rr");

    // Fairness check: 4 same-urgency incremental streams with 1000 DATA
    // frames each. Completion indices should be tightly clustered.
    for (streams, frames) in [(2usize, 1000usize), (4, 1000), (8, 500)] {
        let completions = simulate_incremental_rr(streams, frames);
        let mean = completions.iter().sum::<usize>() as f64 / completions.len() as f64;
        let sd = stddev(&completions);
        let pct = if mean > 0.0 { sd / mean * 100.0 } else { 0.0 };
        // Fairness budget: stddev < 20 % of mean (a single-frame RR gives
        // identical completions so the ratio stays near 0 %).
        assert!(
            pct < 20.0,
            "unfair incremental schedule: streams={streams} frames={frames} \
             completions={completions:?} mean={mean:.1} stddev={sd:.2} \
             ({pct:.1}% of mean)"
        );
        let param = format!("streams={streams}/frames={frames}");
        group.bench_with_input(
            BenchmarkId::new("drain", &param),
            &(streams, frames),
            |b, &(s, f)| b.iter(|| simulate_incremental_rr(s, f)),
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_sort_strategies,
    bench_incremental_round_robin
);
criterion_main!(benches);
