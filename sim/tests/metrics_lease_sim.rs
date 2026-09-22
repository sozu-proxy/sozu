// Gated on `--cfg tokio_unstable`: moonpool-sim seeds tokio's runtime RNG via the
// unstable `RngSeed` API. Without the flag this whole test crate compiles to an
// empty (0-test) binary and pulls no moonpool/tokio deps (see Cargo.toml), so a
// plain `cargo test --workspace` stays free of tokio_unstable. Run the real sweep
// with `RUSTFLAGS="--cfg tokio_unstable" cargo test -p sozu-sim --test metrics_lease_sim`.
#![cfg(tokio_unstable)]
//! Deterministic simulation of the metrics cardinality-lease core
//! (`sozu_lib::metrics::Aggregator`'s `lease_apply` / `lease_clear` /
//! `lease_tick` machinery, plus the `remove_cluster` / `add_cluster` /
//! `remove_backend` tombstone), driven by the [moonpool-sim] engine on the
//! same pattern as [`udp_simulation.rs`] and [`tcp_preread_sim.rs`] (see
//! `doc/testing.md`'s "Recipe: adding a simulator over another sans-io
//! core").
//!
//! Unlike [`UdpManager`] (a long-lived flow table) or `SniPrereadCore` (a
//! near-stateless per-connection machine), the lease core is a long-lived
//! `Aggregator` stepped through many actions, same shape as the UDP sim:
//! one moonpool iteration == one seed == one `Aggregator` driven through
//! `steps` weighted-random actions. `Aggregator::leases` is PRIVATE (unlike
//! the UDP sim's tests, which live inside `lib/src/metrics/mod.rs` and can
//! reach in), so this harness can only observe `lease_count()`,
//! `detail_effective()`, `lease_apply`/`lease_clear`/`lease_tick`'s own
//! return values, and `query()`'s `WorkerMetrics` — exactly the real
//! caller's surface. A parallel shadow [`Model`] mirrors the same
//! insert/renew/expire/clear/tombstone discipline and is cross-checked
//! against that surface after every step.
//!
//! # Why `lease_apply` needed a clock parameter first
//!
//! Before this change `lease_apply` read `Instant::now()` directly instead
//! of taking `now: Instant` the way `lease_tick` already did. That is what
//! made this simulator possible: a virtual-clock-driven workload can only
//! control lease expiry if `now` is threaded through EVERY entry point that
//! computes one, not just the janitor.
//!
//! # Four traps this project has already paid for (do not reintroduce them)
//!
//! 1. **Virtual time makes a leaked host-clock read vacuously
//!    deterministic.** A leaked `Instant::now()` does not produce noisy
//!    samples under a virtual clock — it saturates to a near-constant real
//!    offset every iteration, so `assert_eq!(run_a, run_b)` alone is green
//!    before AND after a fix. What actually catches it is the ordinary,
//!    UNCONDITIONAL per-step [`Model::check`] cross-check —
//!    `agg.lease_count()` against the shadow's own count,
//!    `agg.detail_effective()` against the shadow's recomputed value — run
//!    after every single action regardless of which one just fired.
//!    Verified directly: reverting `lease_apply` to `Instant::now() + ttl`
//!    while keeping its `now: Instant` parameter (so every caller still
//!    compiles) fails the seed sweep at seed `0x0`, step 17, with
//!    `lease_count 0 != shadow 1` — `Model::check`, not a named
//!    determinism or boundary assertion. Two assertions that read as if
//!    they should catch this do NOT, and are not credited for it: the
//!    `Action::BoundaryExpiry` arm's `tick_now >= apply_now + ttl` compares
//!    two harness-local virtual-clock values computed from `ctx.time()`
//!    alone and never reads `agg`, so it cannot observe `lease_apply`'s
//!    output no matter what that output is; and
//!    [`metrics_lease_simulation_is_deterministic`]'s FINAL zero-leases /
//!    effective-equals-configured pairing is a liveness/termination
//!    post-condition — under this defect it is never even reached (the run
//!    panics at step 17, in `Model::check`, long before the end), and its
//!    sleep (`LEASE_TTL_MAX + 60s`) swamps any bounded real-clock-anchored
//!    expiry regardless of correctness, so it would very likely hold by
//!    construction even if it were reached.
//! 2. **A pinned seed that does not actually replay.** Every
//!    `set_debug_seeds` call in this file is paired with `set_iterations(1)`
//!    — without it `SimulationBuilder`'s default `UntilCoverageStable` keeps
//!    drawing FRESH seeds after the requested one.
//! 3. **A simulator that never inspects its own output.** [`Model::check`]
//!    does not stop at internal counters: it calls `Aggregator::query` (the
//!    same surface a real `sozu top`/CLI client uses) and asserts, from the
//!    returned `WorkerMetrics`, that every tombstoned cluster id's row is
//!    ABSENT and every live cluster/backend's recorded `Count` metric
//!    equals the shadow's expected cumulative sum exactly.
//! 4. **Chaos that is on by default and rarely fires is an untested path
//!    wearing a green suite as camouflage.** The FINAL mass-reap inside
//!    [`MetricsLeaseSimWorkload::run`] is NOT gated on the weighted draw or
//!    on buggify — it runs unconditionally at the end of EVERY seed, and
//!    bounded termination (the table reaching exactly zero) is asserted as
//!    a hard post-condition every time, not inferred from typical green
//!    runs.
//!
//! # #1408, resolved: renewals draw freely, both directions
//!
//! `lease_apply` used to carry a `debug_assert!(self.effective >=
//! previous_effective, "lease_apply must not lower the effective detail
//! level")` that assumed a renewal could only ever raise `effective`. That
//! was wrong: a lease exists to let a client temporarily ELEVATE
//! cardinality on its own behalf, so a client renewing at a LOWER level
//! than its own previous one is withdrawing part of its own request, and
//! `effective` recomputing downward in response is correct, not a defect.
//! The assertion was deleted and the comment corrected in
//! `lib/src/metrics/mod.rs` (#1408); `recompute_effective`'s
//! `max(configured, max over live leases)` already computed the right
//! answer throughout.
//!
//! Every level drawn for [`Action::LeaseApply`] — fresh insert or renewal —
//! is now an unconstrained fresh draw (see `random_level` at the
//! `Action::LeaseApply` call site). Nothing reroutes a lowering draw to the
//! client's existing level anymore: lowering renewals are ordinary input,
//! not an excluded corner. Both the 256-seed default sweep and a deeper
//! 2048-seed sweep (`SOZU_METRICS_LEASE_SIM_SEEDS=2048`) pass cleanly with
//! lowering renewals exercised throughout, alongside raising ones, with no
//! new failure surfaced. The shadow [`Model::apply`] needed no change: it
//! already recomputed `expected_effective()` as a pure `max(configured,
//! every currently-stored lease's level)` on every call, with no
//! elevate-only assumption baked in.
//!
//! # What this harness does NOT cover
//!
//! Most notably: `Server::notify`'s `SetMetricDetail` dispatch arm
//! (`lib/src/server.rs`) — the actual production code Part 1 touches,
//! including ULID/PID peer-binding parsing, the TTL-bound dispatch gate,
//! `WorkerResponse` encoding, and `push_metric_detail_transition` — has no
//! test anywhere in this repository, before or after this change. No e2e
//! test references `SetMetricDetail`, and this simulator, like the
//! existing 14 unit tests in `lib/src/metrics/mod.rs`, calls
//! `Aggregator::lease_apply` directly and bypasses `notify` entirely. That
//! gap is pre-existing and the `now`-threading in Part 1 is a low-risk
//! mechanical change, so it was not a reason to hold this change — but it
//! is real, and it is the one gap that touches the code this change edits.
//!
//! # Swarm configurations (Groce et al., "Swarm Testing", ISSTA 2012)
//!
//! Same contract as the other two simulators (`doc/testing.md` §5): each
//! seed draws a [`SwarmConfig`] from the seeded RNG before the first
//! action. `LeaseApply` (the sole lease creator), `RemoveCluster` (the sole
//! tombstone armer) and `EmitClusterMetric` (the sole resurrection prober)
//! are MANDATORY — omitting any of them makes an entire assertion class
//! vacuously green. `LeaseClear`, `LeaseTick`, and `AddCluster` are
//! SUPPRESSORS: each shrinks the lease table or clears a tombstone,
//! repairing the very full-table / still-tombstoned state a capacity or
//! resurrection bug needs. The rest are OPTIONAL. Seeds divisible by four
//! keep the inclusive all-features configuration; `SOZU_SIM_SWARM=0`
//! (shared by all three simulators) disables the draw entirely.
//!
//! # Replay / sweep ergonomics
//!
//! - `SOZU_METRICS_LEASE_SIM_SEED=<u64|0xhex>` — replay that ONE seed
//!   verbosely.
//! - `SOZU_METRICS_LEASE_SIM_SEEDS=<n>` — sweep `n` iterations (default
//!   256).
//! - `SOZU_METRICS_LEASE_SIM_STEPS=<n>` — steps per seed (default 1200).
//! - `SOZU_SIM_SWARM=0|1` — draw per-seed swarm configurations (default
//!   `1`).
//!
//! [moonpool-sim]: https://crates.io/crates/moonpool-sim

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use moonpool_sim::{
    RandomProvider, SimContext, SimulationBuilder, SimulationReport, SimulationResult,
    TimeProvider, Workload, buggify_with_prob, current_sim_seed,
};
use sozu_command_lib::{
    config::MetricDetailLevel,
    proto::command::{QueryMetricsOptions, filtered_metrics, response_content::ContentType},
};
use sozu_lib::metrics::{
    Aggregator, LEASE_CLIENT_ID_MAX_BYTES, LEASE_TABLE_CAP, LEASE_TTL_MAX, LeaseApplyOutcome,
    LeaseClearOutcome, MetricValue, PeerBinding, Subscriber,
};

/// Synthetic metric name this harness emits: purely a simulation probe, not
/// a real sōzu metric (mirrors the arbitrary string literals
/// `local_drain.rs`'s own unit tests use alongside `names::` constants).
const SIM_METRIC: &str = "sim.session_activity";

// --------------------------------------------------------------------------
// Pools: small, bounded id spaces so client/cluster/backend ids collide and
// get reused realistically (renewals, repeated removals, repeated emission).
// --------------------------------------------------------------------------

const CLIENT_POOL: u32 = 6;
const CLUSTER_POOL: u32 = 4;
const BACKEND_POOL: u32 = 3;
const BINDING_POOL: u32 = 3;

fn pooled_client_id(ctx: &SimContext) -> String {
    format!("client-{}", ctx.random().random_range(0..CLIENT_POOL))
}

fn pooled_cluster_id(ctx: &SimContext) -> String {
    format!("cluster-{}", ctx.random().random_range(0..CLUSTER_POOL))
}

fn pooled_backend_id(ctx: &SimContext) -> String {
    format!("backend-{}", ctx.random().random_range(0..BACKEND_POOL))
}

/// `0` => the unknown/default binding (pre-`SO_PEERCRED`, non-Linux peers);
/// `1..=BINDING_POOL` => one of a small set of distinct KNOWN bindings, so
/// applies/renewals/clears collide across the authorisation boundary
/// (cb530aa5) often enough to exercise both the authorised and
/// `Unauthorized` paths from the random grammar alone.
fn pooled_binding(ctx: &SimContext) -> PeerBinding {
    let n = ctx.random().random_range(0..BINDING_POOL + 1);
    if n == 0 {
        PeerBinding::default()
    } else {
        PeerBinding {
            pid: Some(1000 + n as i32),
            session_ulid: Some(0xA5A5_0000_0000_0000_0000_0000_0000_0000u128 | n as u128),
        }
    }
}

fn random_level(ctx: &SimContext) -> MetricDetailLevel {
    match ctx.random().random_range(0..4u8) {
        0 => MetricDetailLevel::Process,
        1 => MetricDetailLevel::Frontend,
        2 => MetricDetailLevel::Cluster,
        _ => MetricDetailLevel::Backend,
    }
}

/// `client_id` for `Action::LeaseApply` specifically: mostly the pooled
/// small ids, but a low probability draws one OVER
/// [`LEASE_CLIENT_ID_MAX_BYTES`] — required coverage per this file's module
/// doc (`ClientIdTooLong`). Such an id is never accepted, so it never
/// collides with a pooled id's renewal/authorisation state.
fn client_id_for_apply(ctx: &SimContext) -> String {
    if ctx.random().random_bool(0.03) {
        let over = LEASE_CLIENT_ID_MAX_BYTES + 1 + ctx.random().random_range(0..32) as usize;
        return "x".repeat(over);
    }
    pooled_client_id(ctx)
}

/// A TTL drawn to hit all three interesting regions: comfortably inside the
/// bound, EXACTLY at [`LEASE_TTL_MAX`] (accepted boundary), and just over it
/// (rejected boundary — required coverage per this file's module doc).
fn random_ttl(ctx: &SimContext) -> Duration {
    match ctx.random().random_range(0..10u8) {
        0..6 => Duration::from_secs(ctx.random().random_range(1..LEASE_TTL_MAX.as_secs())),
        6..8 => LEASE_TTL_MAX,
        _ => LEASE_TTL_MAX + Duration::from_secs(ctx.random().random_range(1..120)),
    }
}

/// Mirrors `filter_labels_for_detail` (`lib/src/metrics/mod.rs`), which
/// `Aggregator::receive_metric` runs BEFORE either drain ever sees a
/// metric: at `Process`/`Frontend` both labels are dropped (the metric
/// becomes proxy-level, out of scope for this harness's cluster/backend
/// tracking); at `Cluster` the backend label is ALSO dropped, so a
/// backend-shaped emission lands as a CLUSTER-level one; only at `Backend`
/// do both labels survive. Missing this step is what made the first draft
/// of this harness assert `cluster "cluster-N" expected Count(1), got None`
/// under a randomly-lowered `configured` floor.
fn filtered_labels<'a>(
    effective: MetricDetailLevel,
    cluster_id: &'a str,
    backend_id: Option<&'a str>,
) -> (Option<&'a str>, Option<&'a str>) {
    match effective {
        MetricDetailLevel::Process | MetricDetailLevel::Frontend => (None, None),
        MetricDetailLevel::Cluster => (Some(cluster_id), None),
        MetricDetailLevel::Backend => (Some(cluster_id), backend_id),
    }
}

// --------------------------------------------------------------------------
// Shadow model. Mirrors `Aggregator`'s private lease table and both drains'
// tombstone discipline closely enough to predict every OBSERVABLE outcome
// (`LeaseApplyOutcome`, `LeaseClearOutcome`, `lease_tick`'s `Option`,
// `detail_effective()`, `lease_count()`, and `query()`'s `WorkerMetrics`).
// --------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct ShadowLease {
    level: MetricDetailLevel,
    expires_at: Instant,
    binding: PeerBinding,
}

#[derive(Debug, Default, Clone)]
struct CoverageTally {
    applied: u64,
    client_id_too_long: u64,
    table_full: u64,
    ttl_out_of_range: u64,
    apply_unauthorized: u64,
    cleared: u64,
    clear_not_found: u64,
    clear_unauthorized: u64,
    tick_expired_some: u64,
    tick_exact_boundary_expired: u64,
    late_emission_dropped: u64,
    late_emission_recorded: u64,
    backend_removed: u64,
}

impl CoverageTally {
    fn merge(&mut self, other: &CoverageTally) {
        self.applied += other.applied;
        self.client_id_too_long += other.client_id_too_long;
        self.table_full += other.table_full;
        self.ttl_out_of_range += other.ttl_out_of_range;
        self.apply_unauthorized += other.apply_unauthorized;
        self.cleared += other.cleared;
        self.clear_not_found += other.clear_not_found;
        self.clear_unauthorized += other.clear_unauthorized;
        self.tick_expired_some += other.tick_expired_some;
        self.tick_exact_boundary_expired += other.tick_exact_boundary_expired;
        self.late_emission_dropped += other.late_emission_dropped;
        self.late_emission_recorded += other.late_emission_recorded;
        self.backend_removed += other.backend_removed;
    }

    /// Asserted on the MERGED campaign tally, never per seed (a single
    /// swarm-subset seed legitimately cannot reach every class) — same
    /// placement rule as `tcp_preread_sim.rs`'s `CoverageTally`.
    fn assert_full_coverage(&self) {
        let zero: Vec<&str> = [
            ("applied", self.applied),
            ("client_id_too_long", self.client_id_too_long),
            ("table_full", self.table_full),
            ("ttl_out_of_range", self.ttl_out_of_range),
            ("apply_unauthorized", self.apply_unauthorized),
            ("cleared", self.cleared),
            ("clear_not_found", self.clear_not_found),
            ("clear_unauthorized", self.clear_unauthorized),
            ("tick_expired_some", self.tick_expired_some),
            (
                "tick_exact_boundary_expired",
                self.tick_exact_boundary_expired,
            ),
            ("late_emission_dropped", self.late_emission_dropped),
            ("late_emission_recorded", self.late_emission_recorded),
            ("backend_removed", self.backend_removed),
        ]
        .into_iter()
        .filter(|(_, v)| *v == 0)
        .map(|(name, _)| name)
        .collect();
        assert!(
            zero.is_empty(),
            "coverage gate failed -- zero occurrences of: {zero:?}\nfull tally: {self:?}",
        );
    }
}

struct Model {
    configured: MetricDetailLevel,
    leases: HashMap<String, ShadowLease>,
    removed_clusters: HashSet<String>,
    cluster_counts: HashMap<String, i64>,
    backend_counts: HashMap<(String, String), i64>,
    coverage: CoverageTally,
}

impl Model {
    fn new(configured: MetricDetailLevel) -> Self {
        Model {
            configured,
            leases: HashMap::new(),
            removed_clusters: HashSet::new(),
            cluster_counts: HashMap::new(),
            backend_counts: HashMap::new(),
            coverage: CoverageTally::default(),
        }
    }

    /// Mirrors `Aggregator::recompute_effective`: `max(configured, every
    /// CURRENTLY STORED lease's level)` — including leases whose real-world
    /// expiry has already passed but have not yet been reaped by a
    /// `lease_tick` call. `effective` is CACHED and only recomputed on
    /// apply/clear/tick, never at read-time, so the shadow mirrors that
    /// exact non-lazy discipline rather than a live "still valid at `now`"
    /// filter.
    fn expected_effective(&self) -> MetricDetailLevel {
        self.leases
            .values()
            .map(|l| l.level)
            .fold(self.configured, MetricDetailLevel::max)
    }

    /// Mirrors `Aggregator::lease_apply` exactly, in the SAME precondition
    /// order, so both sides reject the same request for the same reason.
    fn apply(
        &mut self,
        client_id: &str,
        level: MetricDetailLevel,
        ttl: Duration,
        binding: PeerBinding,
        now: Instant,
    ) -> LeaseApplyOutcome {
        if client_id.len() > LEASE_CLIENT_ID_MAX_BYTES {
            self.coverage.client_id_too_long += 1;
            return LeaseApplyOutcome::ClientIdTooLong;
        }
        if ttl > LEASE_TTL_MAX {
            self.coverage.ttl_out_of_range += 1;
            return LeaseApplyOutcome::TtlOutOfRange;
        }
        let is_renewal = self.leases.contains_key(client_id);
        if !is_renewal && self.leases.len() >= LEASE_TABLE_CAP {
            self.coverage.table_full += 1;
            return LeaseApplyOutcome::TableFull;
        }
        if is_renewal {
            let existing = &self.leases[client_id];
            if existing.binding.is_known() && !existing.binding.matches(&binding) {
                self.coverage.apply_unauthorized += 1;
                return LeaseApplyOutcome::Unauthorized;
            }
        }
        let previous_effective = self.expected_effective();
        self.leases.insert(
            client_id.to_owned(),
            ShadowLease {
                level,
                expires_at: now + ttl,
                binding,
            },
        );
        let new_effective = self.expected_effective();
        self.coverage.applied += 1;
        LeaseApplyOutcome::Applied {
            previous_effective,
            new_effective,
        }
    }

    /// Mirrors `Aggregator::lease_clear`.
    fn clear(&mut self, client_id: &str, presented: PeerBinding) -> LeaseClearOutcome {
        let Some(entry) = self.leases.get(client_id) else {
            self.coverage.clear_not_found += 1;
            return LeaseClearOutcome::NotFound;
        };
        if entry.binding.is_known() && !entry.binding.matches(&presented) {
            self.coverage.clear_unauthorized += 1;
            return LeaseClearOutcome::Unauthorized;
        }
        let previous = self.expected_effective();
        self.leases.remove(client_id);
        self.coverage.cleared += 1;
        LeaseClearOutcome::Cleared {
            previous_effective: previous,
        }
    }

    /// Mirrors `Aggregator::lease_tick`: `retain(expires_at > now)`, i.e. a
    /// lease whose `expires_at == now` EXACTLY is evicted, not kept.
    fn tick(&mut self, now: Instant) -> Option<MetricDetailLevel> {
        let before_len = self.leases.len();
        let previous_effective = self.expected_effective();
        self.leases.retain(|_, e| e.expires_at > now);
        if self.leases.len() == before_len {
            return None;
        }
        self.coverage.tick_expired_some += 1;
        let new_effective = self.expected_effective();
        if previous_effective != new_effective {
            Some(previous_effective)
        } else {
            None
        }
    }

    /// Mirrors `LocalDrain::remove_cluster`: drop the row AND arm the
    /// tombstone.
    fn remove_cluster(&mut self, cluster_id: &str) {
        self.removed_clusters.insert(cluster_id.to_owned());
        self.cluster_counts.remove(cluster_id);
        self.backend_counts.retain(|(c, _), _| c != cluster_id);
    }

    /// Mirrors `LocalDrain::add_cluster`: clear the tombstone only.
    fn add_cluster(&mut self, cluster_id: &str) {
        self.removed_clusters.remove(cluster_id);
    }

    /// Mirrors `LocalDrain::remove_backend`: drop ONE backend's counters.
    /// Deliberately NOT a tombstone — a later emission for the same
    /// backend legitimately resurrects it (only `remove_cluster` arms a
    /// tombstone).
    fn remove_backend(&mut self, cluster_id: &str, backend_id: &str) {
        self.backend_counts
            .remove(&(cluster_id.to_owned(), backend_id.to_owned()));
        self.coverage.backend_removed += 1;
    }

    /// Mirrors the FULL `Aggregator::receive_metric` -> `LocalDrain` cluster
    /// path: `Aggregator::receive_metric` filters `(cluster_id,
    /// backend_id)` through `filter_labels_for_detail(effective, ...)`
    /// BEFORE either drain ever sees the metric (see [`filtered_labels`]),
    /// THEN the tombstone guard applies. Returns `true` iff the emission
    /// was recorded as a cluster-level metric.
    fn emit_cluster_metric(&mut self, cluster_id: &str, effective: MetricDetailLevel) -> bool {
        let Some(cluster_id) = filtered_labels(effective, cluster_id, None).0 else {
            // Filtered to proxy-level by the cardinality knob -- out of
            // scope for this harness's cluster/backend tracking.
            return false;
        };
        if self.removed_clusters.contains(cluster_id) {
            self.coverage.late_emission_dropped += 1;
            return false;
        }
        *self
            .cluster_counts
            .entry(cluster_id.to_owned())
            .or_insert(0) += 1;
        self.coverage.late_emission_recorded += 1;
        true
    }

    /// Mirrors the FULL `Aggregator::receive_metric` -> `LocalDrain`
    /// backend path. At [`MetricDetailLevel::Cluster`] the backend label is
    /// filtered away, so the emission is recorded as a CLUSTER-level
    /// metric instead — matching `LocalDrain::receive_metric`'s
    /// `(Some(cluster), None)` dispatch arm, NOT a backend one.
    fn emit_backend_metric(
        &mut self,
        cluster_id: &str,
        backend_id: &str,
        effective: MetricDetailLevel,
    ) -> bool {
        let (filtered_cluster, filtered_backend) =
            filtered_labels(effective, cluster_id, Some(backend_id));
        let Some(cluster_id) = filtered_cluster else {
            return false;
        };
        if self.removed_clusters.contains(cluster_id) {
            self.coverage.late_emission_dropped += 1;
            return false;
        }
        match filtered_backend {
            Some(backend_id) => {
                *self
                    .backend_counts
                    .entry((cluster_id.to_owned(), backend_id.to_owned()))
                    .or_insert(0) += 1;
            }
            None => {
                *self
                    .cluster_counts
                    .entry(cluster_id.to_owned())
                    .or_insert(0) += 1;
            }
        }
        self.coverage.late_emission_recorded += 1;
        true
    }

    /// Harness-level invariants, checked after every step: the real
    /// `Aggregator`'s own `debug_assert` invariants fire for free inside
    /// each call; this adds the cross-step model checks AND inspects the
    /// actual `query()` output (trap 3 in this file's module doc).
    fn check(&mut self, agg: &mut Aggregator, step: usize, ctx_label: &str) {
        let seed = current_sim_seed();

        assert!(
            agg.lease_count() as usize <= LEASE_TABLE_CAP,
            "seed={seed:#x} step={step} [{ctx_label}]: lease_count {} exceeds LEASE_TABLE_CAP {}",
            agg.lease_count(),
            LEASE_TABLE_CAP,
        );
        assert_eq!(
            agg.lease_count() as usize,
            self.leases.len(),
            "seed={seed:#x} step={step} [{ctx_label}]: lease_count {} != shadow {}",
            agg.lease_count(),
            self.leases.len(),
        );
        assert_eq!(
            agg.detail_effective(),
            self.expected_effective(),
            "seed={seed:#x} step={step} [{ctx_label}]: detail_effective {:?} != expected {:?}",
            agg.detail_effective(),
            self.expected_effective(),
        );
        assert_eq!(
            agg.detail_configured(),
            self.configured,
            "seed={seed:#x} step={step} [{ctx_label}]: detail_configured {:?} != shadow {:?}",
            agg.detail_configured(),
            self.configured,
        );
        assert!(
            agg.detail_effective() >= agg.detail_configured(),
            "seed={seed:#x} step={step} [{ctx_label}]: effective must dominate configured",
        );

        let content = agg
            .query(&QueryMetricsOptions {
                list: false,
                cluster_ids: Vec::new(),
                backend_ids: Vec::new(),
                metric_names: Vec::new(),
                no_clusters: false,
                workers: false,
            })
            .expect("query with empty selectors must not fail");
        // `response_content::ContentType` has no `Debug` impl (only the
        // outer `ResponseContent` does), so the mismatch arm names the
        // expectation instead of formatting `other`.
        let Some(ContentType::WorkerMetrics(wm)) = content.content_type else {
            panic!("seed={seed:#x} step={step} [{ctx_label}]: expected WorkerMetrics from query()",);
        };

        // The 0238c3a4 resurrection guard: every tombstoned cluster id's
        // row must be ABSENT from the actual query() output.
        for cluster_id in &self.removed_clusters {
            assert!(
                !wm.clusters.contains_key(cluster_id),
                "seed={seed:#x} step={step} [{ctx_label}]: tombstoned cluster {cluster_id:?} \
                 resurrected in query() output",
            );
        }
        // Every live cluster's recorded cumulative Count must match EXACTLY.
        for (cluster_id, expected_count) in &self.cluster_counts {
            let observed = wm
                .clusters
                .get(cluster_id)
                .and_then(|cm| cm.cluster.get(SIM_METRIC))
                .and_then(|m| m.inner.as_ref());
            match observed {
                Some(filtered_metrics::Inner::Count(v)) => assert_eq!(
                    v, expected_count,
                    "seed={seed:#x} step={step} [{ctx_label}]: cluster {cluster_id:?} observed \
                     count {v} != expected {expected_count}",
                ),
                other => panic!(
                    "seed={seed:#x} step={step} [{ctx_label}]: cluster {cluster_id:?} expected \
                     Count({expected_count}), got {other:?}",
                ),
            }
        }
        for ((cluster_id, backend_id), expected_count) in &self.backend_counts {
            let observed = wm
                .clusters
                .get(cluster_id)
                .and_then(|cm| cm.backends.iter().find(|b| &b.backend_id == backend_id))
                .and_then(|b| b.metrics.get(SIM_METRIC))
                .and_then(|m| m.inner.as_ref());
            match observed {
                Some(filtered_metrics::Inner::Count(v)) => assert_eq!(
                    v, expected_count,
                    "seed={seed:#x} step={step} [{ctx_label}]: backend {cluster_id:?}/\
                     {backend_id:?} observed count {v} != expected {expected_count}",
                ),
                other => panic!(
                    "seed={seed:#x} step={step} [{ctx_label}]: backend {cluster_id:?}/\
                     {backend_id:?} expected Count({expected_count}), got {other:?}",
                ),
            }
        }
    }
}

// --------------------------------------------------------------------------
// Workload grammar.
// --------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    LeaseApply,
    RemoveCluster,
    EmitClusterMetric,
    LeaseClear,
    LeaseTick,
    AddCluster,
    EmitBackendMetric,
    RemoveBackend,
    SetDetail,
    TableCapacityStress,
    BoundaryExpiry,
    RenewalAuthGate,
}

/// Weighted action grammar (weights sum to 100). Swarm classification (see
/// this file's module doc and `doc/testing.md`): indices 0-2 (`LeaseApply`,
/// `RemoveCluster`, `EmitClusterMetric`) are MANDATORY; indices 3-5
/// (`LeaseClear`, `LeaseTick`, `AddCluster`) are SUPPRESSORS; the rest are
/// OPTIONAL.
const ACTION_TABLE: [(Action, u32); 12] = [
    (Action::LeaseApply, 24),
    (Action::RemoveCluster, 10),
    (Action::EmitClusterMetric, 16),
    (Action::LeaseClear, 10),
    (Action::LeaseTick, 10),
    (Action::AddCluster, 6),
    (Action::EmitBackendMetric, 8),
    (Action::RemoveBackend, 4),
    (Action::SetDetail, 4),
    (Action::TableCapacityStress, 3),
    (Action::BoundaryExpiry, 3),
    (Action::RenewalAuthGate, 2),
];

const MANDATORY_COUNT: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SwarmConfig {
    enabled: [bool; 12],
}

fn swarm_enabled() -> bool {
    !matches!(std::env::var("SOZU_SIM_SWARM"), Ok(v) if v.trim() == "0")
}

fn inclusive_seed(seed: u64) -> bool {
    seed.is_multiple_of(4)
}

fn campaign_seeds(count: usize) -> Vec<u64> {
    (0..u64::try_from(count).expect("campaign seed count fits in u64")).collect()
}

impl SwarmConfig {
    fn full() -> Self {
        SwarmConfig {
            enabled: [true; 12],
        }
    }

    fn draw(ctx: &SimContext, seed: u64) -> Self {
        if inclusive_seed(seed) {
            return SwarmConfig::full();
        }
        let mut enabled = [false; 12];
        for slot in enabled.iter_mut().take(MANDATORY_COUNT) {
            *slot = true;
        }
        for slot in enabled.iter_mut().skip(MANDATORY_COUNT) {
            *slot = ctx.random().random_bool(0.5);
        }
        if !enabled.iter().skip(MANDATORY_COUNT).any(|e| *e) {
            enabled[MANDATORY_COUNT] = true;
        } else if enabled.iter().all(|e| *e) {
            enabled[11] = false;
        }
        let cfg = SwarmConfig { enabled };
        debug_assert!(
            enabled.iter().take(MANDATORY_COUNT).all(|e| *e),
            "MANDATORY features must stay enabled"
        );
        debug_assert!(
            cfg.total_weight() > ACTION_TABLE[0].1 + ACTION_TABLE[1].1 + ACTION_TABLE[2].1,
            "a drawn subset keeps at least one non-mandatory feature"
        );
        debug_assert!(
            !cfg.enabled.iter().all(|e| *e),
            "a non-reserved seed must use a proper subset"
        );
        cfg
    }

    fn is_enabled(&self, action: Action) -> bool {
        ACTION_TABLE
            .iter()
            .zip(&self.enabled)
            .find(|((a, _), _)| *a == action)
            .is_some_and(|(_, e)| *e)
    }

    fn total_weight(&self) -> u32 {
        ACTION_TABLE
            .iter()
            .zip(&self.enabled)
            .filter(|(_, e)| **e)
            .map(|((_, w), _)| *w)
            .sum()
    }

    fn log_line(&self, seed: u64, swarm: bool) -> String {
        let mode = if !swarm {
            "off"
        } else if self.enabled.iter().all(|e| *e) {
            "full"
        } else {
            "subset"
        };
        let features: Vec<String> = ACTION_TABLE
            .iter()
            .zip(&self.enabled)
            .filter(|(_, e)| **e)
            .map(|((a, w), _)| format!("{a:?}:{w}"))
            .collect();
        format!(
            "swarm-config sim=metrics_lease seed={seed:#x} mode={mode} features=[{}] total_weight={}",
            features.join(","),
            self.total_weight(),
        )
    }
}

fn pick_action(ctx: &SimContext, cfg: &SwarmConfig) -> Action {
    let roll = ctx.random().random_range(0..cfg.total_weight());
    let mut remaining = roll;
    for ((action, weight), enabled) in ACTION_TABLE.iter().zip(&cfg.enabled) {
        if !enabled {
            continue;
        }
        if remaining < *weight {
            return *action;
        }
        remaining -= weight;
    }
    unreachable!("roll {roll} below the renormalized total always lands on an enabled action")
}

fn apply_tag(o: &LeaseApplyOutcome) -> u64 {
    match o {
        LeaseApplyOutcome::Applied {
            previous_effective,
            new_effective,
        } => 0xA1 ^ ((*previous_effective as u64) << 8) ^ ((*new_effective as u64) << 16),
        LeaseApplyOutcome::ClientIdTooLong => 0xA2,
        LeaseApplyOutcome::TableFull => 0xA3,
        LeaseApplyOutcome::TtlOutOfRange => 0xA4,
        LeaseApplyOutcome::Unauthorized => 0xA5,
    }
}

fn clear_tag(o: &LeaseClearOutcome) -> u64 {
    match o {
        LeaseClearOutcome::Cleared { previous_effective } => {
            0xB1 ^ ((*previous_effective as u64) << 8)
        }
        LeaseClearOutcome::NotFound => 0xB2,
        LeaseClearOutcome::Unauthorized => 0xB3,
    }
}

fn tick_tag(o: &Option<MetricDetailLevel>) -> u64 {
    match o {
        Some(level) => 0xC1 ^ ((*level as u64) << 8),
        None => 0xC2,
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0001_0000_01b3);
    }
    hash
}

// --------------------------------------------------------------------------
// The workload: one moonpool iteration == one deterministic seed run.
// --------------------------------------------------------------------------

/// `(fold, final_lease_count, final_effective, configured_floor)` pushed
/// per run, for the determinism guard AND its paired absolute-value check
/// (trap 1 in this file's module doc).
type FingerprintSink = Arc<Mutex<Vec<(u64, u32, MetricDetailLevel, MetricDetailLevel)>>>;
type ConfigSink = Arc<Mutex<Vec<SwarmConfig>>>;
type CoverageSink = Arc<Mutex<CoverageTally>>;

struct MetricsLeaseSimWorkload {
    steps: usize,
    verbose: bool,
    sink: Option<FingerprintSink>,
    swarm: bool,
    config_sink: Option<ConfigSink>,
    coverage: Option<CoverageSink>,
}

#[async_trait]
impl Workload for MetricsLeaseSimWorkload {
    fn name(&self) -> &'static str {
        "metrics_lease_simulation"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let seed = current_sim_seed();
        // Virtual clock base: captured once; moonpool's simulated Duration
        // is added to it so `Aggregator::lease_apply`/`lease_tick` see
        // monotonically advancing `Instant`s. Same pattern as the UDP sim.
        let base = Instant::now();
        let now = |ctx: &SimContext| base + ctx.time().now();

        let swarm_cfg = if self.swarm {
            SwarmConfig::draw(ctx, seed)
        } else {
            SwarmConfig::full()
        };
        eprintln!("{}", swarm_cfg.log_line(seed, self.swarm));
        if let Some(sink) = &self.config_sink {
            sink.lock().unwrap().push(swarm_cfg);
        }

        let configured = random_level(ctx);
        let mut agg = Aggregator::new("sim".to_owned());
        agg.set_up_detail(configured);
        let mut model = Model::new(configured);
        let mut fresh_counter: u64 = 0;
        let mut fp: u64 = 0x9E37_79B9_7F4A_7C15;

        model.check(&mut agg, 0, "init");

        for step in 1..=self.steps {
            let action = pick_action(ctx, &swarm_cfg);
            if self.verbose && step % 200 == 0 {
                eprintln!(
                    "seed={seed:#x} step={step} action={action:?} leases={} effective={:?}",
                    agg.lease_count(),
                    agg.detail_effective(),
                );
            }

            match action {
                Action::LeaseApply => {
                    let client_id = client_id_for_apply(ctx);
                    // Every apply -- fresh insert or renewal, raising or
                    // lowering -- draws an unconstrained fresh level. #1408
                    // established that a renewal is ordinary input in
                    // either direction; nothing here reroutes a lowering
                    // draw to the client's existing level anymore.
                    let level = random_level(ctx);
                    let ttl = random_ttl(ctx);
                    let binding = pooled_binding(ctx);
                    let t = now(ctx);
                    let expected = model.apply(&client_id, level, ttl, binding, t);
                    let actual = agg.lease_apply(client_id.clone(), level, ttl, binding, t);
                    assert_eq!(
                        actual, expected,
                        "seed={seed:#x} step={step}: lease_apply({client_id:?}) outcome mismatch",
                    );
                    fp = fp.rotate_left(7) ^ apply_tag(&actual);
                }
                Action::LeaseClear => {
                    let client_id = pooled_client_id(ctx);
                    let binding = pooled_binding(ctx);
                    let expected = model.clear(&client_id, binding);
                    let actual = agg.lease_clear(&client_id, binding);
                    assert_eq!(
                        actual, expected,
                        "seed={seed:#x} step={step}: lease_clear({client_id:?}) outcome mismatch",
                    );
                    fp = fp.rotate_left(7) ^ clear_tag(&actual);
                }
                Action::LeaseTick => {
                    let delta = if ctx.random().random_bool(0.15) {
                        Duration::from_secs(
                            ctx.random().random_range(1..LEASE_TTL_MAX.as_secs() + 60),
                        )
                    } else {
                        Duration::from_millis(ctx.random().random_range(1..5_000))
                    };
                    let _ = ctx.time().sleep(delta).await;
                    let t = now(ctx);
                    let expected = model.tick(t);
                    let actual = agg.lease_tick(t);
                    assert_eq!(
                        actual, expected,
                        "seed={seed:#x} step={step}: lease_tick outcome mismatch",
                    );
                    fp = fp.rotate_left(7) ^ tick_tag(&actual);
                }
                Action::RemoveCluster => {
                    let cluster_id = pooled_cluster_id(ctx);
                    model.remove_cluster(&cluster_id);
                    agg.remove_cluster(&cluster_id);
                    fp = fp.rotate_left(7) ^ fnv1a(cluster_id.as_bytes()).rotate_left(11);
                }
                Action::AddCluster => {
                    let cluster_id = pooled_cluster_id(ctx);
                    model.add_cluster(&cluster_id);
                    agg.add_cluster(&cluster_id);
                }
                Action::RemoveBackend => {
                    let cluster_id = pooled_cluster_id(ctx);
                    let backend_id = pooled_backend_id(ctx);
                    model.remove_backend(&cluster_id, &backend_id);
                    agg.remove_backend(&cluster_id, &backend_id);
                }
                Action::EmitClusterMetric => {
                    let cluster_id = pooled_cluster_id(ctx);
                    // `effective` gates what `Aggregator::receive_metric`
                    // itself is about to do (see `filtered_labels`); read
                    // it BEFORE the call, matching what the real code reads
                    // internally at the same instant.
                    let effective = agg.detail_effective();
                    let recorded = model.emit_cluster_metric(&cluster_id, effective);
                    agg.receive_metric(SIM_METRIC, Some(&cluster_id), None, MetricValue::Count(1));
                    fp = fp.rotate_left(7) ^ (recorded as u64);
                }
                Action::EmitBackendMetric => {
                    let cluster_id = pooled_cluster_id(ctx);
                    let backend_id = pooled_backend_id(ctx);
                    let effective = agg.detail_effective();
                    let recorded = model.emit_backend_metric(&cluster_id, &backend_id, effective);
                    agg.receive_metric(
                        SIM_METRIC,
                        Some(&cluster_id),
                        Some(&backend_id),
                        MetricValue::Count(1),
                    );
                    fp = fp.rotate_left(7) ^ (recorded as u64).rotate_left(3);
                }
                Action::SetDetail => {
                    let level = random_level(ctx);
                    model.configured = level;
                    agg.set_up_detail(level);
                }
                Action::TableCapacityStress => {
                    // Directed: apply with FRESH, never-reused ids so the
                    // small client pool cannot mask table-capacity
                    // exhaustion behind renewals -- guarantees `TableFull`
                    // coverage rather than hoping a bounded pool overflows
                    // LEASE_TABLE_CAP by chance.
                    let t = now(ctx);
                    for _ in 0..ctx.random().random_range(1..8u32) {
                        fresh_counter += 1;
                        let client_id = format!("stress-{fresh_counter}");
                        let level = random_level(ctx);
                        let ttl = Duration::from_secs(
                            ctx.random().random_range(1..LEASE_TTL_MAX.as_secs()),
                        );
                        let binding = PeerBinding::default();
                        let expected = model.apply(&client_id, level, ttl, binding, t);
                        let actual = agg.lease_apply(client_id.clone(), level, ttl, binding, t);
                        assert_eq!(
                            actual, expected,
                            "seed={seed:#x} step={step}: capacity-stress apply({client_id:?}) mismatch",
                        );
                        fp = fp.rotate_left(7) ^ apply_tag(&actual);
                    }
                }
                Action::BoundaryExpiry => {
                    // Directed replay of the exact-boundary CONTRACT of
                    // `lease_tick`'s retain condition (`expires_at > now`,
                    // strict -- equality must NOT survive): apply a fresh
                    // lease, advance the VIRTUAL clock by AT LEAST `ttl`,
                    // and confirm eviction. This is a SCENARIO that drives
                    // the boundary, not a detector of a leaked host clock
                    // in `lease_apply` -- see this file's module doc, trap
                    // 1, for what actually catches that (the unconditional
                    // per-step `Model::check`, verified at seed 0x0 step
                    // 17) and why the two checks below do not.
                    fresh_counter += 1;
                    let client_id = format!("boundary-{fresh_counter}");
                    let ttl = Duration::from_millis(ctx.random().random_range(100..60_000));
                    let apply_now = now(ctx);
                    let level = MetricDetailLevel::Backend;
                    let expected_apply =
                        model.apply(&client_id, level, ttl, PeerBinding::default(), apply_now);
                    let actual_apply = agg.lease_apply(
                        client_id.clone(),
                        level,
                        ttl,
                        PeerBinding::default(),
                        apply_now,
                    );
                    assert_eq!(
                        actual_apply, expected_apply,
                        "seed={seed:#x} step={step}: boundary apply mismatch",
                    );
                    let _ = ctx.time().sleep(ttl).await;
                    let tick_now = now(ctx);
                    // Self-check on the harness's OWN virtual-clock
                    // arithmetic, NOT a check on `lease_apply` or `agg`:
                    // `apply_now` and `tick_now` are both computed from
                    // `ctx.time()` alone (see the `now` closure above),
                    // with no read of `agg` anywhere in this comparison —
                    // it cannot observe `lease_apply`'s output no matter
                    // what that output is, and is not credited for
                    // catching trap 1 (see this file's module doc). Its
                    // only job is to confirm the scenario actually drove
                    // the boundary it claims to (moonpool's `sleep(ttl)`
                    // guarantees resuming AT OR AFTER `ttl`, not exact
                    // nanosecond equality, hence `>=` rather than
                    // `assert_eq!`) before the real assertions below run.
                    assert!(
                        tick_now >= apply_now + ttl,
                        "seed={seed:#x} step={step}: virtual clock did not advance by at least \
                         the scripted ttl (tick_now={tick_now:?} apply_now+ttl={:?})",
                        apply_now + ttl,
                    );
                    let expected_tick = model.tick(tick_now);
                    let actual_tick = agg.lease_tick(tick_now);
                    assert_eq!(
                        actual_tick, expected_tick,
                        "seed={seed:#x} step={step}: boundary tick outcome mismatch",
                    );
                    assert!(
                        !model.leases.contains_key(&client_id),
                        "seed={seed:#x} step={step}: shadow lease {client_id:?} must be gone once \
                         `now` has reached its expiry instant",
                    );
                    // Probe the REAL aggregator through its public
                    // behavioural contract (lease_count() alone cannot
                    // distinguish "never existed" from "expired"): a clear
                    // of an already-expired id must report NotFound. This
                    // DOES read `agg` (unlike the virtual-clock self-check
                    // above) and is this scenario's actual assertion of
                    // the boundary contract itself — that equality at
                    // `expires_at` does not survive. It is a real,
                    // deliberate consequence of a clean fix; it is not
                    // credited as the trap-1 defence (see this file's
                    // module doc): under the reintroduced clock leak the
                    // run panics earlier, in the unconditional per-step
                    // `Model::check`, before this scenario-specific probe
                    // would even get a chance to run or not run.
                    let probe = agg.lease_clear(&client_id, PeerBinding::default());
                    assert_eq!(
                        probe,
                        LeaseClearOutcome::NotFound,
                        "seed={seed:#x} step={step}: a lease that has reached its expiry instant \
                         must already be gone from the real aggregator",
                    );
                    model.coverage.tick_exact_boundary_expired += 1;
                    fp = fp.rotate_left(7) ^ 0xD1;
                }
                Action::RenewalAuthGate => {
                    // Directed replay of cb530aa5: victim applies with a
                    // KNOWN binding, an attacker renews the SAME
                    // client_id with a DIFFERENT known binding ->
                    // Unauthorized, and the victim's original binding
                    // must still authorise their own clear afterwards --
                    // proof the refused renewal did not corrupt state.
                    fresh_counter += 1;
                    let client_id = format!("renewal-{fresh_counter}");
                    let victim = PeerBinding {
                        pid: Some(42_000 + fresh_counter as i32),
                        session_ulid: Some(0x5EED_0000_0000_0000 | fresh_counter as u128),
                    };
                    let attacker = PeerBinding {
                        pid: Some(43_000 + fresh_counter as i32),
                        session_ulid: Some(0xBAD0_0000_0000_0000 | fresh_counter as u128),
                    };
                    let t = now(ctx);
                    let level = random_level(ctx);
                    let ttl =
                        Duration::from_secs(ctx.random().random_range(1..LEASE_TTL_MAX.as_secs()));
                    let first_expected = model.apply(&client_id, level, ttl, victim, t);
                    let first_actual = agg.lease_apply(client_id.clone(), level, ttl, victim, t);
                    assert_eq!(
                        first_actual, first_expected,
                        "seed={seed:#x} step={step}: victim's initial apply mismatch",
                    );
                    // The table can legitimately be full at this point (a
                    // fresh fresh-counter-derived `client_id` is a fresh
                    // insert, not a renewal — `TableCapacityStress` can have
                    // filled it). That is not what this directed scenario
                    // tests, so only proceed with the renewal-authorisation
                    // assertions once the victim's own apply actually
                    // landed; otherwise the fp fold still records the
                    // TableFull outcome via `apply_tag` and the step moves
                    // on.
                    if let LeaseApplyOutcome::Applied { .. } = first_actual {
                        let renew_expected = model.apply(&client_id, level, ttl, attacker, t);
                        let renew_actual =
                            agg.lease_apply(client_id.clone(), level, ttl, attacker, t);
                        assert_eq!(
                            renew_actual, renew_expected,
                            "seed={seed:#x} step={step}: renewal outcome mismatch",
                        );
                        assert_eq!(
                            renew_actual,
                            LeaseApplyOutcome::Unauthorized,
                            "seed={seed:#x} step={step}: foreign-binding renewal must be refused",
                        );
                        let clear_expected = model.clear(&client_id, victim);
                        let clear_actual = agg.lease_clear(&client_id, victim);
                        assert_eq!(
                            clear_actual, clear_expected,
                            "seed={seed:#x} step={step}: clear outcome mismatch",
                        );
                        assert!(
                            matches!(clear_actual, LeaseClearOutcome::Cleared { .. }),
                            "seed={seed:#x} step={step}: victim's original binding must still \
                             clear cleanly after the foreign-binding renewal was refused",
                        );
                    }
                    fp = fp.rotate_left(7) ^ apply_tag(&first_actual).rotate_left(17);
                }
            }

            // Buggify: low-probability extra adversarial event, gated by
            // its sibling grammar feature so a swarm subset omits the
            // fault along with the feature (never redrawn when disabled,
            // keeping the draw sequence a pure function of (seed, config)).
            if buggify_with_prob!(0.02) {
                let arm = ctx.random().random_range(0..3u8);
                let arm_feature = match arm {
                    0 => Action::RemoveCluster,
                    1 => Action::TableCapacityStress,
                    _ => Action::LeaseTick,
                };
                if swarm_cfg.is_enabled(arm_feature) {
                    match arm {
                        0 => {
                            // Stress the 0238c3a4 interleaving hard: several
                            // late emissions immediately after one removal.
                            let cluster_id = pooled_cluster_id(ctx);
                            model.remove_cluster(&cluster_id);
                            agg.remove_cluster(&cluster_id);
                            for _ in 0..ctx.random().random_range(2..6u8) {
                                let backend_id = pooled_backend_id(ctx);
                                let effective = agg.detail_effective();
                                let recorded =
                                    model.emit_backend_metric(&cluster_id, &backend_id, effective);
                                agg.receive_metric(
                                    SIM_METRIC,
                                    Some(&cluster_id),
                                    Some(&backend_id),
                                    MetricValue::Count(1),
                                );
                                assert!(
                                    !recorded,
                                    "seed={seed:#x} step={step}: buggify late-emission burst \
                                     recorded a tombstoned backend",
                                );
                            }
                        }
                        1 => {
                            let t = now(ctx);
                            for _ in 0..ctx.random().random_range(4..16u32) {
                                fresh_counter += 1;
                                let client_id = format!("stress-{fresh_counter}");
                                let level = random_level(ctx);
                                let _ = model.apply(
                                    &client_id,
                                    level,
                                    LEASE_TTL_MAX,
                                    PeerBinding::default(),
                                    t,
                                );
                                let _ = agg.lease_apply(
                                    client_id,
                                    level,
                                    LEASE_TTL_MAX,
                                    PeerBinding::default(),
                                    t,
                                );
                            }
                        }
                        _ => {
                            // Giant jump past LEASE_TTL_MAX so it mass-expires.
                            let _ = ctx
                                .time()
                                .sleep(
                                    LEASE_TTL_MAX
                                        + Duration::from_secs(ctx.random().random_range(1..60)),
                                )
                                .await;
                            let t = now(ctx);
                            let _ = model.tick(t);
                            let _ = agg.lease_tick(t);
                        }
                    }
                }
            }

            model.check(&mut agg, step, "step");
        }

        // FINAL invariant, unconditional (never gated on the weighted
        // draw or on buggify — trap 4 in this file's module doc): a clock
        // jump well past LEASE_TTL_MAX must reap the table to EXACTLY
        // zero, on both the real aggregator and the shadow.
        let _ = ctx
            .time()
            .sleep(LEASE_TTL_MAX + Duration::from_secs(60))
            .await;
        let final_now = now(ctx);
        let _ = agg.lease_tick(final_now);
        let _ = model.tick(final_now);
        model.check(&mut agg, self.steps + 1, "final-reap");
        assert_eq!(
            agg.lease_count(),
            0,
            "seed={seed:#x}: FINAL reap left {} live leases",
            agg.lease_count(),
        );
        assert!(
            model.leases.is_empty(),
            "seed={seed:#x}: FINAL reap left shadow leases: {:?}",
            model.leases.keys().collect::<Vec<_>>(),
        );
        let final_effective = agg.detail_effective();
        assert_eq!(
            final_effective, model.configured,
            "seed={seed:#x}: FINAL effective {final_effective:?} != configured floor \
             {:?} once every lease is gone",
            model.configured,
        );

        if self.verbose {
            eprintln!(
                "seed={seed:#x} DONE steps={} coverage={:?}",
                self.steps, model.coverage,
            );
        }
        if let Some(sink) = &self.sink {
            sink.lock()
                .unwrap()
                .push((fp, agg.lease_count(), final_effective, model.configured));
        }
        if let Some(sink) = &self.coverage {
            sink.lock().unwrap().merge(&model.coverage);
        }

        Ok(())
    }
}

// --------------------------------------------------------------------------
// Env-knob parsing (decimal or 0x-hex) + run-time budget.
// --------------------------------------------------------------------------

fn parse_u64(s: &str) -> Option<u64> {
    let t = s.trim();
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()
    } else {
        t.parse::<u64>().ok()
    }
}

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok().and_then(|s| parse_u64(&s))
}

fn env_usize(key: &str) -> Option<usize> {
    env_u64(key).map(|v| v as usize)
}

/// This workload's largest single-step logical-time advance is
/// `LEASE_TTL_MAX + 60s` (~360s, the `LeaseTick`/buggify mass-expire arms
/// and the final reap), so budget generously and proportionally to the
/// step count. Logical time only — moonpool runs it in milliseconds.
fn run_budget(steps: usize) -> Duration {
    Duration::from_secs((steps as u64).saturating_mul(500).saturating_add(7_200))
}

fn assert_no_failures(report: &SimulationReport) {
    if report.failed_runs != 0 {
        let errs: Vec<String> = report
            .individual_metrics
            .iter()
            .filter_map(|r| r.as_ref().err().map(|e| format!("{e:?}")))
            .collect();
        panic!(
            "failed_runs={} seeds_failing={:?}\nassertion_violations={:?}\ncoverage_violations={:?}\nerrors:\n{}",
            report.failed_runs,
            report.seeds_failing,
            report.assertion_violations,
            report.coverage_violations,
            errs.join("\n---\n"),
        );
    }
}

// --------------------------------------------------------------------------
// Tests.
// --------------------------------------------------------------------------

/// Deterministic seed sweep. On a hard invariant violation the panic
/// carries the failing seed + step; moonpool also lists it in
/// `report.seeds_failing`. The merged coverage tally is gated at the end,
/// never per seed (a swarm-subset seed legitimately cannot reach every
/// class).
#[test]
fn metrics_lease_simulation_seed_sweep() {
    let steps = env_usize("SOZU_METRICS_LEASE_SIM_STEPS").unwrap_or(1_200);
    let swarm = swarm_enabled();

    if let Some(seed) = env_u64("SOZU_METRICS_LEASE_SIM_SEED") {
        eprintln!(
            "== metrics-lease sim single-seed replay: seed={seed:#x} steps={steps} swarm={swarm} =="
        );
        let report = SimulationBuilder::new()
            .workload(MetricsLeaseSimWorkload {
                steps,
                verbose: true,
                sink: None,
                swarm,
                config_sink: None,
                coverage: None,
            })
            .set_debug_seeds(vec![seed])
            // Without an explicit iteration count, `SimulationBuilder`
            // defaults to `UntilCoverageStable` (up to 1000 iterations) and
            // would keep drawing FRESH random seeds after this one — fixing
            // it to 1 is what actually makes this "replay that ONE seed"
            // (see this file's module doc, trap 2).
            .set_iterations(1)
            .run_time_budget(run_budget(steps))
            .run();
        assert_no_failures(&report);
        return;
    }

    let seeds = env_usize("SOZU_METRICS_LEASE_SIM_SEEDS").unwrap_or(256);
    let coverage: CoverageSink = Arc::new(Mutex::new(CoverageTally::default()));
    let report = SimulationBuilder::new()
        .workload(MetricsLeaseSimWorkload {
            steps,
            verbose: false,
            sink: None,
            swarm,
            config_sink: None,
            coverage: Some(coverage.clone()),
        })
        .set_debug_seeds(campaign_seeds(seeds))
        .set_iterations(seeds)
        .run_time_budget(run_budget(steps))
        .run();
    assert_no_failures(&report);
    coverage.lock().unwrap().assert_full_coverage();
}

/// Fast smoke test pinning one representative seed. Swarm is off so the
/// pinned trajectory stays byte-identical across changes that don't touch
/// the grammar itself.
#[test]
fn metrics_lease_simulation_replays_known_seed() {
    let steps = 2_000;
    let report = SimulationBuilder::new()
        .workload(MetricsLeaseSimWorkload {
            steps,
            verbose: false,
            sink: None,
            swarm: false,
            config_sink: None,
            coverage: None,
        })
        .set_debug_seeds(vec![0x5EED_1EA5])
        .set_iterations(1)
        .run_time_budget(run_budget(steps))
        .run();
    assert_no_failures(&report);
}

/// Determinism guard, paired with an ABSOLUTE-value assertion (trap 1 in
/// this file's module doc): two runs of the same seed must agree AND the
/// scripted FINAL reap must leave EXACTLY zero live leases with
/// `detail_effective()` EXACTLY equal to the configured floor — concrete
/// values, not just cross-run equality (which a leaked host-clock read
/// would satisfy vacuously).
#[test]
fn metrics_lease_simulation_is_deterministic() {
    fn run(seed: u64) -> (u64, u32, MetricDetailLevel, MetricDetailLevel) {
        let steps = 1_200;
        let sink: FingerprintSink = Arc::new(Mutex::new(Vec::new()));
        let report = SimulationBuilder::new()
            .workload(MetricsLeaseSimWorkload {
                steps,
                verbose: false,
                sink: Some(sink.clone()),
                // Swarm stays ON here: the guard then also covers the
                // config draw itself (a nondeterministic draw would fork
                // the trace).
                swarm: true,
                config_sink: None,
                coverage: None,
            })
            .set_debug_seeds(vec![seed])
            .set_iterations(1)
            .run_time_budget(run_budget(steps))
            .run();
        assert_no_failures(&report);
        let v = sink.lock().unwrap();
        *v.first().expect("workload recorded a fingerprint")
    }

    let a = run(0x00AB_CDEF);
    let b = run(0x00AB_CDEF);
    assert_eq!(
        a, b,
        "same seed must yield an identical trace (determinism)"
    );
    assert_eq!(
        a.1, 0,
        "FINAL reap must leave EXACTLY zero live leases -- an absolute invariant, checked on \
         top of (not instead of) the cross-run equality above",
    );
    assert_eq!(
        a.2, a.3,
        "post-reap detail_effective() must equal EXACTLY the configured floor once every lease \
         is gone",
    );
}

/// Swarm-config stability: the configuration drawn for a seed is a pure
/// function of that seed.
#[test]
fn metrics_lease_swarm_config_is_stable_across_draws() {
    fn draw(seed: u64) -> SwarmConfig {
        let steps = 8;
        let sink: ConfigSink = Arc::new(Mutex::new(Vec::new()));
        let report = SimulationBuilder::new()
            .workload(MetricsLeaseSimWorkload {
                steps,
                verbose: false,
                sink: None,
                swarm: true,
                config_sink: Some(sink.clone()),
                coverage: None,
            })
            .set_debug_seeds(vec![seed])
            .set_iterations(1)
            .run_time_budget(run_budget(steps))
            .run();
        assert_no_failures(&report);
        let v = sink.lock().unwrap();
        *v.first().expect("workload recorded a swarm config")
    }

    for seed in [0x0Au64, 0x0B, 0x0C, 0x5EED_5EED, 0xDEAD_BEEF] {
        let a = draw(seed);
        let b = draw(seed);
        assert_eq!(
            a, b,
            "seed={seed:#x}: swarm config must be identical across two draws"
        );
        assert!(
            a.is_enabled(Action::LeaseApply)
                && a.is_enabled(Action::RemoveCluster)
                && a.is_enabled(Action::EmitClusterMetric),
            "seed={seed:#x}: the MANDATORY features must always be enabled"
        );
        assert_eq!(
            a.enabled.iter().all(|e| *e),
            inclusive_seed(seed),
            "seed={seed:#x}: only reserved seeds may use the full grammar"
        );
        assert_eq!(a.log_line(seed, true), b.log_line(seed, true));
    }
}

#[test]
fn metrics_lease_campaign_reserves_one_inclusive_seed_per_four() {
    for count in [1usize, 3, 4, 5, 256] {
        let seeds = campaign_seeds(count);
        let inclusive = seeds.iter().filter(|seed| inclusive_seed(**seed)).count();
        assert_eq!(inclusive, count.div_ceil(4), "count={count}");
        assert_eq!(seeds.len() - inclusive, count - count.div_ceil(4));
    }
}

#[test]
fn env_parse_accepts_hex_and_decimal() {
    assert_eq!(parse_u64("42"), Some(42));
    assert_eq!(parse_u64(" 256 "), Some(256));
    assert_eq!(parse_u64("0xdeadbeef"), Some(0xdead_beef));
    assert_eq!(parse_u64("0XFF"), Some(0xFF));
    assert_eq!(parse_u64("notanumber"), None);
}
