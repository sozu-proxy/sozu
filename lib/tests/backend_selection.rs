//! Backend selection: allocation budget and per-policy pick sequences.
//!
//! Two properties of `BackendList::next_available_backend_with_key`
//! (`lib/src/backends.rs`), measured through the public API only so the same
//! file runs unchanged against any internal shape of the candidate walk:
//!
//! 1. **No heap allocation per selection in steady state**, under every
//!    policy and on each of the three candidate tiers (primary, backup,
//!    fail-open). This binary installs a counting global allocator; the
//!    counter is thread-local, so the harness's other test threads cannot
//!    pollute a measurement.
//! 2. **Pick sequences are pinned.** Each policy selects from a list whose
//!    unavailable backends are interleaved with the available ones, so an
//!    index computed over the wrong list (the full one instead of the
//!    filtered one) picks a different backend. The expected sequences were
//!    recorded from the `Vec`-collecting implementation this file was written
//!    against; any change to them is a change of load-balancing behavior.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    net::SocketAddr,
};

use sozu_command_lib::proto::command::{LoadBalancingAlgorithms, LoadBalancingParams, LoadMetric};
use sozu_lib::{
    backends::{Backend, BackendList, BackendStatus},
    load_balancing::{PowerOfTwo, Random},
    retry::RetryPolicy,
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

/// Which tier of the candidate walk a fixture exercises.
#[derive(Clone, Copy, Debug)]
enum Tier {
    /// Primary backends, with backup and unavailable ones interleaved.
    Primary,
    /// Every primary is unavailable, so selection retries with `backup`.
    Backup,
    /// Nothing passes `can_open`; fail-open routes over `Normal` backends
    /// whose retry policy is `OKAY`.
    FailOpen,
}

fn address(index: usize) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9000 + index as u16))
}

/// Whether backend `index` of the [`fixture`] for `tier` is a candidate of
/// that tier. Mirrors the statuses [`fixture`] assigns.
fn is_candidate(tier: Tier, index: usize) -> bool {
    match tier {
        Tier::Primary => ![2, 4, 5, 7].contains(&index),
        Tier::Backup => [1, 5, 7, 9].contains(&index),
        Tier::FailOpen => ![1, 6].contains(&index),
    }
}

/// Ten backends with distinct weights. The unavailable ones sit between
/// available ones, never only at the tail, so a policy that indexed the full
/// list instead of the candidate list would pick differently.
fn fixture(tier: Tier) -> BackendList {
    let mut list = BackendList::new();
    for index in 0..10 {
        let backup = match tier {
            // Backends 2 and 7 are backups: not primary candidates.
            Tier::Primary | Tier::FailOpen => index == 2 || index == 7,
            // Only the odd backends are backups; every primary is closed.
            Tier::Backup => index % 2 == 1,
        };
        let mut backend = Backend::new(
            &format!("backend-{index}"),
            address(index),
            None,
            Some(LoadBalancingParams {
                weight: 10 + 7 * index as i32,
            }),
            Some(backup),
        );
        // Every candidate carries a load of at least 1 and every excluded
        // backend a load of 0, so a least-loaded pick that leaked outside the
        // candidate set would land on an excluded backend.
        if is_candidate(tier, index) {
            backend.active_connections = 1 + ((index + 3) * 3) % 5;
            backend.active_requests = 1 + ((index + 2) * 7) % 4;
        }
        match tier {
            Tier::Primary => {
                // Backend 4 is closing, backend 5 is in retry back-off.
                if index == 4 {
                    backend.status = BackendStatus::Closing;
                }
                if index == 5 {
                    backend.retry_policy().fail();
                }
            }
            Tier::Backup => {
                if !backup {
                    backend.status = BackendStatus::Closing;
                }
                // One backup is closing too, interleaved with open ones.
                if index == 3 {
                    backend.status = BackendStatus::Closing;
                }
            }
            Tier::FailOpen => {
                // Everyone is health-check unhealthy, so nothing can open and
                // the fail-open tier applies. Backend 1 is closing and
                // backend 6 in back-off: both are excluded from fail-open.
                backend.health.record_failure(1);
                if index == 1 {
                    backend.status = BackendStatus::Closing;
                }
                if index == 6 {
                    backend.retry_policy().fail();
                }
            }
        }
        list.add_backend(backend);
    }
    list
}

/// Every policy under test, with the seeded constructions for the two
/// stateful random policies so the recorded sequences are reproducible.
#[derive(Clone, Copy, Debug)]
enum Policy {
    RoundRobin,
    Random,
    LeastLoaded(LoadMetric),
    PowerOfTwo(LoadMetric),
    Hrw,
    Maglev,
}

const POLICIES: [Policy; 10] = [
    Policy::RoundRobin,
    Policy::Random,
    Policy::LeastLoaded(LoadMetric::Connections),
    Policy::LeastLoaded(LoadMetric::Requests),
    Policy::LeastLoaded(LoadMetric::ConnectionTime),
    Policy::PowerOfTwo(LoadMetric::Connections),
    Policy::PowerOfTwo(LoadMetric::Requests),
    Policy::PowerOfTwo(LoadMetric::ConnectionTime),
    Policy::Hrw,
    Policy::Maglev,
];

const SEED: u64 = 0x5eed_1549;

fn apply(list: &mut BackendList, policy: Policy) {
    match policy {
        Policy::RoundRobin => {
            list.set_load_balancing_policy(LoadBalancingAlgorithms::RoundRobin, None)
        }
        Policy::Random => list.load_balancing = Box::new(Random::with_seed(SEED)),
        Policy::LeastLoaded(metric) => {
            list.set_load_balancing_policy(LoadBalancingAlgorithms::LeastLoaded, Some(metric))
        }
        Policy::PowerOfTwo(metric) => {
            list.load_balancing = Box::new(PowerOfTwo::with_seed(SEED, metric))
        }
        Policy::Hrw => list.set_load_balancing_policy(LoadBalancingAlgorithms::Hrw, None),
        Policy::Maglev => list.set_load_balancing_policy(LoadBalancingAlgorithms::Maglev, None),
    }
}

/// The affinity key for pick `step`: `None` on even steps, so HRW and Maglev
/// exercise both their keyed lookup and their round-robin fallback.
fn key(step: usize) -> Option<u64> {
    (step % 2 == 1).then(|| (step as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

/// Index (`0..10`, see [`address`]) of every backend picked over `steps`
/// selections.
fn picks(tier: Tier, policy: Policy, steps: usize) -> Vec<usize> {
    let mut list = fixture(tier);
    apply(&mut list, policy);
    (0..steps)
        .map(|step| {
            let picked = list
                .next_available_backend_with_key(key(step))
                .expect("every fixture tier has candidates");
            let port = picked.borrow().address.port();
            usize::from(port - 9000)
        })
        .collect()
}

#[test]
fn backend_selection_allocates_nothing_in_steady_state() {
    const WARMUP: usize = 16;
    const MEASURED: usize = 256;
    let mut failures = Vec::new();
    for tier in [Tier::Primary, Tier::Backup, Tier::FailOpen] {
        for policy in POLICIES {
            let mut list = fixture(tier);
            apply(&mut list, policy);
            // Warm-up: the fail-open latch, the metrics aggregator's first
            // insert for a key, and any lazily built policy state settle
            // here, off the measured window.
            for step in 0..WARMUP {
                assert!(list.next_available_backend_with_key(key(step)).is_some());
            }
            let before = allocations();
            for step in 0..MEASURED {
                let picked = list.next_available_backend_with_key(key(step));
                assert!(picked.is_some(), "{tier:?}/{policy:?}: no backend selected");
            }
            let allocated = allocations() - before;
            if allocated != 0 {
                failures.push(format!(
                    "{tier:?}/{policy:?}: {allocated} allocations over {MEASURED} selections"
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "backend selection must not allocate in steady state:\n{}",
        failures.join("\n")
    );
}

/// Recorded pick sequences over 12 selections.
///
/// `LoadMetric::ConnectionTime` is left out on purpose: `PeakEWMA::get`
/// (`lib/src/lib.rs`) decays each backend's estimate by the wall-clock time
/// since its last read, so its ties resolve on timing, not on the candidate
/// walk. It stays covered by the allocation test above.
const PINNED: &[(Tier, Policy, [usize; 12])] = &[
    (
        Tier::Primary,
        Policy::RoundRobin,
        [0, 1, 3, 6, 8, 9, 0, 1, 3, 6, 8, 9],
    ),
    (
        Tier::Primary,
        Policy::Random,
        [6, 9, 6, 9, 8, 8, 8, 9, 0, 6, 8, 6],
    ),
    (
        Tier::Primary,
        Policy::LeastLoaded(LoadMetric::Connections),
        [9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9],
    ),
    (
        Tier::Primary,
        Policy::LeastLoaded(LoadMetric::Requests),
        [6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6],
    ),
    (
        Tier::Primary,
        Policy::PowerOfTwo(LoadMetric::Connections),
        [9, 9, 6, 9, 3, 1, 3, 9, 9, 9, 1, 6],
    ),
    (
        Tier::Primary,
        Policy::PowerOfTwo(LoadMetric::Requests),
        [9, 6, 6, 0, 6, 8, 0, 1, 6, 6, 8, 6],
    ),
    (
        Tier::Primary,
        Policy::Hrw,
        [0, 8, 1, 9, 3, 9, 6, 9, 8, 6, 9, 1],
    ),
    (
        Tier::Primary,
        Policy::Maglev,
        [0, 9, 1, 3, 3, 0, 6, 6, 8, 6, 9, 9],
    ),
    (
        Tier::Backup,
        Policy::RoundRobin,
        [1, 5, 7, 9, 1, 5, 7, 9, 1, 5, 7, 9],
    ),
    (
        Tier::Backup,
        Policy::Random,
        [5, 9, 7, 9, 7, 7, 7, 9, 1, 5, 9, 5],
    ),
    (
        Tier::Backup,
        Policy::LeastLoaded(LoadMetric::Connections),
        [7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7],
    ),
    (
        Tier::Backup,
        Policy::LeastLoaded(LoadMetric::Requests),
        [1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1],
    ),
    (
        Tier::Backup,
        Policy::PowerOfTwo(LoadMetric::Connections),
        [9, 9, 7, 7, 1, 7, 9, 9, 1, 9, 9, 7],
    ),
    (
        Tier::Backup,
        Policy::PowerOfTwo(LoadMetric::Requests),
        [9, 5, 9, 1, 9, 5, 5, 5, 5, 9, 9, 1],
    ),
    (
        Tier::Backup,
        Policy::Hrw,
        [1, 7, 5, 9, 7, 5, 9, 9, 1, 5, 5, 7],
    ),
    (
        Tier::Backup,
        Policy::Maglev,
        [1, 9, 5, 9, 7, 7, 9, 7, 1, 5, 5, 9],
    ),
    (
        Tier::FailOpen,
        Policy::RoundRobin,
        [0, 2, 3, 4, 5, 7, 8, 9, 0, 2, 3, 4],
    ),
    (
        Tier::FailOpen,
        Policy::Random,
        [4, 9, 5, 9, 7, 7, 7, 9, 2, 4, 8, 4],
    ),
    (
        Tier::FailOpen,
        Policy::LeastLoaded(LoadMetric::Connections),
        [2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2],
    ),
    (
        Tier::FailOpen,
        Policy::LeastLoaded(LoadMetric::Requests),
        [2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2],
    ),
    (
        Tier::FailOpen,
        Policy::PowerOfTwo(LoadMetric::Connections),
        [9, 4, 9, 3, 2, 4, 3, 9, 9, 7, 3, 7],
    ),
    (
        Tier::FailOpen,
        Policy::PowerOfTwo(LoadMetric::Requests),
        [9, 9, 5, 5, 2, 2, 4, 9, 5, 5, 3, 9],
    ),
    (
        Tier::FailOpen,
        Policy::Hrw,
        [0, 7, 2, 9, 3, 5, 4, 9, 5, 3, 7, 7],
    ),
    (
        Tier::FailOpen,
        Policy::Maglev,
        [0, 9, 2, 3, 3, 2, 4, 3, 5, 8, 7, 9],
    ),
];

#[test]
fn backend_selection_pick_sequences_are_pinned() {
    let mut mismatches = Vec::new();
    for &(tier, policy, expected) in PINNED {
        let got = picks(tier, policy, 12);
        // Negative space: nothing outside the tier's candidate set is picked.
        for &index in &got {
            assert!(
                is_candidate(tier, index),
                "{tier:?}/{policy:?}: picked backend {index}, which is not a candidate"
            );
        }
        if got != expected {
            mismatches.push(format!(
                "{tier:?}/{policy:?}: got {got:?}, expected {expected:?}"
            ));
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}
