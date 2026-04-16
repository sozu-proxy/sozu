//! Benchmarks comparing sort strategies for H2 stream priority scheduling.
//!
//! Context: In the H2 mux write path, every writable cycle sorts active stream
//! IDs by `(urgency, stream_id)`.  The current implementation uses
//! `sort_by_cached_key`; this benchmark tests whether `sort_unstable_by_key`
//! or `sort_unstable_by` would be faster.

use std::collections::HashMap;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

type StreamId = u32;

/// Simulates the `Prioriser::get()` method.
struct MockPrioriser {
    priorities: HashMap<StreamId, (u8, bool)>,
}

impl MockPrioriser {
    fn get(&self, id: &StreamId) -> (u8, bool) {
        self.priorities.get(id).copied().unwrap_or((3, false))
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

    (stream_ids, MockPrioriser { priorities })
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

criterion_group!(benches, bench_sort_strategies);
criterion_main!(benches);
