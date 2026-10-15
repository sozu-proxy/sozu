// Gated on `--cfg tokio_unstable`, like every simulator in this crate: without
// the flag the file compiles to an empty test binary and pulls no moonpool deps.
// Run the sweep with `RUSTFLAGS="--cfg tokio_unstable" cargo test -p sozu-sim`.
#![cfg(tokio_unstable)]
//! Deterministic simulation of HTTP backend selection, driven by moonpool-sim
//! ([#1684](https://github.com/sozu-proxy/sozu/issues/1684)).
//!
//! The mux core reaches the worker's backends through one seam:
//! `Router::backend_from_request` borrows a `router::BackendSelector` for one
//! call and gets back an opaque `BackendId`, reserved but not dialled. This
//! harness owns that selector. It wraps a real `BackendMap` built with
//! `BackendMap::with_seed`, mints ids with `BackendId::new` from its own slot
//! table, and plays the embedder's half of the contract: it decides whether
//! each reserved dial connects, releases a failed dial's reservation with
//! `Backend::release_failed_dial`, records a connect's latency and success,
//! and closes connections it opened. Reservation and resolution are separate
//! steps, so the harness can remove, re-add and upsert backends while a dial or
//! connection still holds the old `Rc`. Every load-balancing policy runs its
//! real code, including `LoadMetric::ConnectionTime` over `PeakEWMA`.
//!
//! # What makes it deterministic
//!
//! Nothing here reads the wall clock after one base `Instant` is captured per
//! run, and nothing draws ambient entropy:
//!
//! - **Time.** The harness keeps its own virtual offset, advanced only by
//!   seeded draws, and hands `base + offset` to every selection, backoff and
//!   connection-time call. The core compares instants only relative to each
//!   other, so the base cannot leak into an outcome.
//! - **Randomness.** The map's generator (policy seeds and backoff jitter) is
//!   seeded from moonpool's seeded RNG, which also draws every workload step.
//!
//! The trace records what a selection decided — the cluster, the backend, the
//! `BackendId` slot in minting order, the sticky cookie answered — and every
//! reservation, release, close, add, remove and in-place upsert. It holds no
//! instant and no duration, so two runs of one seed must produce the same
//! bytes. `backend_selection_simulation_is_deterministic` checks exactly that,
//! and also checks absolute values, so a trace that is equal only because it
//! is empty cannot pass.
//!
//! # Why `run` never awaits
//!
//! Same constraint as `h2_simulation.rs`: `moonpool_sim::Workload` is
//! `#[async_trait]` without `?Send`, and `BackendMap` holds `Rc<RefCell<_>>`
//! backends. No `Rc` may live across a yield point, so the workload is one
//! synchronous loop and the clock is the harness's own, never
//! `ctx.time().sleep`.
//!
//! # Invariant
//!
//! After every step, each backend's `active_connections` equals the shadow
//! count this harness keeps: `+1` per reservation, `-1` per failed-dial release
//! and per close. That is LIFECYCLE §9 invariant 14's "selection reserves,
//! failure releases" rule, observed from outside the core. Removing a backend
//! with a pending reservation or live connection moves that exact allocation
//! from `Closing` to `Closed` only after its count drains to zero; a replacement
//! with the same `(backend_id, address)` is a distinct allocation and receives
//! every later selection. An upsert of a still-live identity preserves its
//! allocation and count. Closing every connection at the end must bring every
//! backend incarnation back to zero.
//!
//! Every run proves those two pending/active scenarios first, with exactly two
//! adds, removes, upserts and retired drains. The seeded phase then weights
//! reservation, delayed resolution, close, add/re-add, in-place upsert, remove
//! and virtual-time advance as separate actions. Its counters make membership
//! coverage non-vacuous in the determinism campaign; a replay prints the exact
//! action trace.
//!
//! # Replay / sweep
//!
//! - `SOZU_BACKEND_SELECTION_SIM_SEED=<u64|0xhex>` — replay ONE seed and print
//!   its trace to stderr.
//! - `SOZU_BACKEND_SELECTION_SIM_SEEDS=<n>` — sweep `n` seeds (default 64).
//! - `SOZU_BACKEND_SELECTION_SIM_STEPS=<n>` — steps per seed (default 400).

use std::{
    cell::RefCell,
    collections::HashMap,
    fmt::Write as _,
    net::SocketAddr,
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use moonpool_sim::{
    RandomProvider, SimContext, SimulationBuilder, SimulationReport, SimulationResult, Workload,
    current_sim_seed,
};
use rusty_ulid::Ulid;
use sozu_command_lib::proto::command::{LoadBalancingAlgorithms, LoadMetric};
use sozu_lib::{
    Protocol,
    backends::{Backend, BackendError, BackendMap, BackendStatus},
    protocol::{
        kawa_h1::editor::HttpContext,
        mux::{
            BackendId,
            router::{Affinity, BackendSelector, Router, SelectedBackend},
        },
    },
    retry::{RetryAction, RetryPolicy},
};

const CLUSTERS: [&str; 2] = ["cluster-a", "cluster-b"];
const BACKENDS_PER_CLUSTER: u16 = 4;

/// The policies a cluster draws from, with the metric each reads.
const POLICIES: [(LoadBalancingAlgorithms, LoadMetric); 7] = [
    (LoadBalancingAlgorithms::RoundRobin, LoadMetric::Connections),
    (LoadBalancingAlgorithms::Random, LoadMetric::Connections),
    (
        LoadBalancingAlgorithms::LeastLoaded,
        LoadMetric::Connections,
    ),
    (
        LoadBalancingAlgorithms::LeastLoaded,
        LoadMetric::ConnectionTime,
    ),
    (LoadBalancingAlgorithms::PowerOfTwo, LoadMetric::Connections),
    (
        LoadBalancingAlgorithms::PowerOfTwo,
        LoadMetric::ConnectionTime,
    ),
    (LoadBalancingAlgorithms::Hrw, LoadMetric::Connections),
];

/// The harness's selector: the worker's real backend map, and a slot table
/// of its own standing in for the session's `BackendRegistry`.
struct SimSelector<'a> {
    map: &'a mut BackendMap,
    slots: &'a mut Vec<Rc<RefCell<Backend>>>,
    retired: &'a [Rc<RefCell<Backend>>],
    selected_slot: &'a mut Option<usize>,
    now: Instant,
}

impl BackendSelector for SimSelector<'_> {
    fn select(
        &mut self,
        cluster_id: &str,
        affinity: Affinity<'_>,
        key: Option<u64>,
    ) -> Result<SelectedBackend, BackendError> {
        let handle = match affinity {
            Affinity::Sticky(Some(cookie)) => self
                .map
                .reserve_sticky_backend(cluster_id, cookie, key, self.now)?,
            Affinity::Sticky(None) | Affinity::Unpinned => {
                self.map.reserve_backend(cluster_id, key, self.now)?
            }
        };
        let (backend_id, address, sticky_id) = {
            let backend = handle.borrow();
            (
                backend.backend_id.clone(),
                backend.address,
                backend.sticky_id.clone(),
            )
        };
        let sticky_session = match affinity {
            Affinity::Sticky(_) => Some(sticky_id.unwrap_or_else(|| backend_id.clone())),
            Affinity::Unpinned => None,
        };
        // Slots in minting order, one per distinct backend, as the mux's
        // registry numbers them.
        let slot = match self
            .slots
            .iter()
            .position(|known| Rc::ptr_eq(known, &handle))
        {
            Some(slot) => slot,
            None => {
                self.slots.push(handle);
                self.slots.len() - 1
            }
        };
        assert!(
            self.retired
                .iter()
                .all(|retired| !Rc::ptr_eq(retired, &self.slots[slot])),
            "seed={:#x}: a retired backend allocation was selected again",
            current_sim_seed()
        );
        *self.selected_slot = Some(slot);
        Ok(SelectedBackend {
            backend: BackendId::new(slot, Rc::from(backend_id.as_str()), address),
            sticky_session,
        })
    }
}

/// What one run observed, for the determinism guard.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RunRecord {
    trace: Vec<u8>,
    selections: usize,
    failed_dials: usize,
    closes: usize,
    sticky_answers: usize,
    backend_adds: usize,
    backend_removes: usize,
    backend_upserts: usize,
    retired_drains: usize,
    random_reservations: usize,
    random_resolutions: usize,
    random_backend_adds: usize,
    random_backend_readds: usize,
    random_backend_removes: usize,
    random_backend_upserts: usize,
    /// Clusters whose policy draws from the map's generator (`Random`,
    /// `PowerOfTwo`), so the guard can require that the map's seed was
    /// exercised and not only the workload's.
    seeded_policies: usize,
}

type RecordSink = Arc<Mutex<Vec<RunRecord>>>;

struct BackendSelectionWorkload {
    steps: usize,
    verbose: bool,
    sink: Option<RecordSink>,
}

#[derive(Clone, Debug)]
struct BackendIdentity {
    cluster_id: Arc<str>,
    backend_id: String,
    address: SocketAddr,
    sticky_id: Option<String>,
}

#[derive(Clone, Copy, Debug)]
enum DialOutcome {
    Failed,
    Connected(Duration),
}

/// The state one run drives. Built and dropped inside `run`, never held across
/// a yield point (see the module docs).
struct Harness {
    map: BackendMap,
    slots: Vec<Rc<RefCell<Backend>>>,
    router: Router,
    /// Expected `active_connections` per slot.
    shadow: HashMap<usize, usize>,
    /// Slots of the connections this harness dialled successfully and has
    /// not closed yet, in dial order.
    open: Vec<usize>,
    /// Slots reserved by selection whose dial has not resolved yet.
    pending: Vec<usize>,
    /// Allocations removed from the live map while a pending dial or open
    /// connection still owns them.
    retired: Vec<Rc<RefCell<Backend>>>,
    /// Exact identities removed by the seeded workload and available for a
    /// later re-add as a distinct allocation.
    removed_identities: Vec<BackendIdentity>,
    next_dynamic_backend: u16,
    base: Instant,
    offset: Duration,
    record: RunRecord,
}

impl Harness {
    fn new(ctx: &SimContext) -> Self {
        let base = Instant::now();
        let mut map = BackendMap::with_seed(ctx.random().random_range(0..u64::MAX));
        let mut record = RunRecord::default();
        for (cluster_index, cluster) in CLUSTERS.iter().enumerate() {
            let (policy, metric) = POLICIES[ctx.random().random_range(0..POLICIES.len())];
            map.set_load_balancing_policy_for_cluster(cluster, policy, Some(metric));
            if matches!(
                policy,
                LoadBalancingAlgorithms::Random | LoadBalancingAlgorithms::PowerOfTwo
            ) {
                record.seeded_policies += 1;
            }
            let _ = writeln!(
                TraceLine(&mut record.trace),
                "cluster {cluster} policy={policy:?} metric={metric:?}"
            );
            for index in 0..BACKENDS_PER_CLUSTER {
                let port = 20_000 + 100 * cluster_index as u16 + index;
                // Half the backends carry a sticky id of their own; the others
                // answer with their backend id.
                let sticky_id = (index % 2 == 0).then(|| format!("{cluster}-sticky-{index}"));
                map.add_backend(
                    cluster,
                    Backend::new_at(
                        &format!("{cluster}-{index}"),
                        SocketAddr::from(([10, 0, cluster_index as u8, index as u8], port)),
                        sticky_id,
                        None,
                        None,
                        base,
                    ),
                );
            }
        }
        Harness {
            map,
            slots: Vec::new(),
            router: Router::new(Duration::from_secs(10), Duration::from_secs(10)),
            shadow: HashMap::new(),
            open: Vec::new(),
            pending: Vec::new(),
            retired: Vec::new(),
            removed_identities: Vec::new(),
            next_dynamic_backend: 0,
            base,
            offset: Duration::ZERO,
            record,
        }
    }

    fn now(&self) -> Instant {
        self.base + self.offset
    }

    fn trace(&mut self, line: std::fmt::Arguments<'_>) {
        let _ = TraceLine(&mut self.record.trace).write_fmt(line);
        self.record.trace.push(b'\n');
    }

    /// Reserve one request through the real router and leave its dial pending.
    fn reserve_request(
        &mut self,
        cluster: &str,
        sticks: bool,
        cookie: Option<String>,
    ) -> Option<usize> {
        let mut context = HttpContext::new(
            Ulid::from(0u128),
            Ulid::from(0u128),
            Protocol::HTTP,
            SocketAddr::from(([127, 0, 0, 1], 80)),
            None,
            "SOZUBALANCEID".to_owned(),
            "Sozu-Id".to_owned(),
            false,
            false,
        );
        context.sticky_session_found = cookie.clone();

        let now = self.now();
        let mut selected_slot = None;
        let selected = {
            let mut selector = SimSelector {
                map: &mut self.map,
                slots: &mut self.slots,
                retired: &self.retired,
                selected_slot: &mut selected_slot,
                now,
            };
            self.router
                .backend_from_request(cluster, sticks, &mut context, &mut selector)
        };
        let backend = match selected {
            Ok(backend) => backend,
            Err(_) => {
                self.trace(format_args!(
                    "select {cluster} stick={sticks} cookie={cookie:?} -> none"
                ));
                return None;
            }
        };
        let slot = selected_slot.expect("the selector records the slot it minted");
        self.pending.push(slot);
        *self.shadow.entry(slot).or_default() += 1;
        self.record.selections += 1;
        if context.sticky_session.is_some() {
            self.record.sticky_answers += 1;
        }
        assert_eq!(
            context.backend_id.as_deref(),
            Some(&*backend.backend_id),
            "seed={:#x}: the request must be stamped with the selected backend",
            current_sim_seed()
        );
        assert_eq!(
            context.sticky_session.is_some(),
            sticks,
            "seed={:#x}: a sticky answer exactly when the frontend sticks",
            current_sim_seed()
        );
        self.trace(format_args!(
            "select {cluster} stick={sticks} cookie={cookie:?} -> slot={slot} backend={} sticky={:?}",
            backend.backend_id, context.sticky_session
        ));
        Some(slot)
    }

    /// Resolve one pending dial. A connected dial keeps the reservation until
    /// the matching connection closes; a failed dial releases it immediately.
    fn resolve_pending(&mut self, slot: usize, outcome: DialOutcome) {
        let pending_index = self
            .pending
            .iter()
            .position(|pending| *pending == slot)
            .expect("the dial being resolved is pending");
        self.pending.swap_remove(pending_index);
        let now = self.now();

        match outcome {
            DialOutcome::Failed => {
                let handle = Rc::clone(&self.slots[slot]);
                handle.borrow_mut().release_failed_dial(now, self.map.rng());
                *self.shadow.get_mut(&slot).expect("reserved above") -= 1;
                self.record.failed_dials += 1;
                self.trace(format_args!("dial slot={slot} -> failed, released"));
            }
            DialOutcome::Connected(latency) => {
                {
                    let mut connected = self.slots[slot].borrow_mut();
                    connected.set_connection_time(latency, now);
                    connected.retry_policy.succeed(now);
                    connected.failures = 0;
                }
                self.open.push(slot);
                self.trace(format_args!(
                    "dial slot={slot} -> connected in {}ms",
                    latency.as_millis()
                ));
            }
        }
    }

    /// One request of a random cluster selects a backend, and its dial
    /// connects or fails.
    fn select_and_dial(&mut self, ctx: &SimContext) {
        let Some(slot) = self.reserve_random_request(ctx) else {
            return;
        };
        let outcome = if ctx.random().random_bool(0.2) {
            DialOutcome::Failed
        } else {
            DialOutcome::Connected(Duration::from_millis(ctx.random().random_range(1..200)))
        };
        self.resolve_pending(slot, outcome);
    }

    fn reserve_random_request(&mut self, ctx: &SimContext) -> Option<usize> {
        let cluster = CLUSTERS[ctx.random().random_range(0..CLUSTERS.len())];
        let sticks = ctx.random().random_bool(0.4);
        let cookie = match ctx.random().random_range(0..3u8) {
            0 => None,
            1 => Some(format!(
                "{cluster}-sticky-{}",
                2 * ctx.random().random_range(0..BACKENDS_PER_CLUSTER / 2)
            )),
            _ => Some("no-such-backend".to_owned()),
        };
        self.reserve_request(cluster, sticks, cookie)
    }

    fn live_identity_for_slot(&self, slot: usize) -> BackendIdentity {
        let handle = &self.slots[slot];
        for (cluster_id, list) in &self.map.backends {
            if let Some(live) = list.backends().iter().find(|live| Rc::ptr_eq(live, handle)) {
                let backend = live.borrow();
                return BackendIdentity {
                    cluster_id: cluster_id.clone(),
                    backend_id: backend.backend_id.clone(),
                    address: backend.address,
                    sticky_id: backend.sticky_id.clone(),
                };
            }
        }
        panic!("slot {slot} is not in the live backend map");
    }

    fn live_backend(&self, identity: &BackendIdentity) -> Option<Rc<RefCell<Backend>>> {
        self.map
            .backends
            .get(identity.cluster_id.as_ref())
            .and_then(|list| list.find_backend_by_identity(&identity.backend_id, &identity.address))
            .cloned()
    }

    /// Update one live backend in place and prove its allocation and count did
    /// not change. Only the exact `(backend_id, address)` identity is used.
    fn upsert_slot(&mut self, slot: usize, sticky_id: String) -> BackendIdentity {
        let identity = self.live_identity_for_slot(slot);
        let before = Rc::clone(&self.slots[slot]);
        let (connections, status) = {
            let backend = before.borrow();
            (backend.active_connections, backend.status.clone())
        };
        self.map.add_backend(
            &identity.cluster_id,
            Backend::new_at(
                &identity.backend_id,
                identity.address,
                Some(sticky_id.clone()),
                None,
                None,
                self.now(),
            ),
        );
        let updated_identity = BackendIdentity {
            sticky_id: Some(sticky_id.clone()),
            ..identity
        };
        let after = self
            .live_backend(&updated_identity)
            .expect("an upsert keeps the backend live");
        assert!(
            Rc::ptr_eq(&before, &after),
            "seed={:#x}: upsert must keep the live backend allocation",
            current_sim_seed()
        );
        let after = after.borrow();
        assert_eq!(after.active_connections, connections);
        assert_eq!(after.status, status);
        assert_eq!(after.sticky_id.as_deref(), Some(sticky_id.as_str()));
        drop(after);
        self.record.backend_upserts += 1;
        self.trace(format_args!(
            "upsert {} {} -> slot={slot} count={connections}",
            updated_identity.cluster_id, updated_identity.backend_id
        ));
        updated_identity
    }

    /// Remove the live allocation of `identity`, keeping the session-owned
    /// `Rc` so its drain can be observed independently of a replacement.
    fn remove_slot(&mut self, slot: usize, identity: &BackendIdentity) -> Rc<RefCell<Backend>> {
        let handle = self
            .live_backend(identity)
            .expect("the backend being removed is live");
        assert!(Rc::ptr_eq(&handle, &self.slots[slot]));
        let connections = handle.borrow().active_connections;
        assert!(
            connections > 0,
            "seed={:#x}: removal coverage must hold a reservation or connection",
            current_sim_seed()
        );
        assert!(self.map.remove_backend(
            &identity.cluster_id,
            &identity.backend_id,
            &identity.address
        ));
        {
            let removed = handle.borrow();
            assert_eq!(removed.status, BackendStatus::Closing);
            assert_eq!(removed.active_connections, connections);
        }
        self.retired.push(Rc::clone(&handle));
        self.record.backend_removes += 1;
        self.trace(format_args!(
            "remove {} {} -> slot={slot} closing count={connections}",
            identity.cluster_id, identity.backend_id
        ));
        handle
    }

    fn readd_backend(&mut self, identity: &BackendIdentity) -> Rc<RefCell<Backend>> {
        self.map.add_backend(
            &identity.cluster_id,
            Backend::new_at(
                &identity.backend_id,
                identity.address,
                identity.sticky_id.clone(),
                None,
                None,
                self.now(),
            ),
        );
        let replacement = self
            .live_backend(identity)
            .expect("the re-added backend is live");
        assert!(
            self.retired
                .iter()
                .all(|retired| !Rc::ptr_eq(retired, &replacement)),
            "seed={:#x}: re-add must create a new backend allocation",
            current_sim_seed()
        );
        {
            let replacement = replacement.borrow();
            assert_eq!(replacement.status, BackendStatus::Normal);
            assert_eq!(replacement.active_connections, 0);
        }
        self.record.backend_adds += 1;
        self.trace(format_args!(
            "add {} {} -> allocation",
            identity.cluster_id, identity.backend_id
        ));
        replacement
    }

    fn assert_retired_drained(&mut self, handle: &Rc<RefCell<Backend>>, slot: usize) {
        {
            let backend = handle.borrow();
            assert_eq!(backend.status, BackendStatus::Closed);
            assert_eq!(backend.active_connections, 0);
        }
        self.record.retired_drains += 1;
        self.trace(format_args!("retired slot={slot} -> closed count=0"));
    }

    /// Deterministic membership coverage around one real backend identity.
    /// It holds a pending reservation across the first remove/re-add, then a
    /// connected session across the second, while every replacement selection
    /// still travels through `Router::backend_from_request`.
    fn exercise_membership_mutations(&mut self) {
        let pending_slot = self
            .reserve_request("cluster-a", true, Some("cluster-a-sticky-0".to_owned()))
            .expect("the initial sticky backend is selectable");
        let pending_identity =
            self.upsert_slot(pending_slot, "cluster-a-sticky-0-pending-upsert".to_owned());
        let retired_pending = self.remove_slot(pending_slot, &pending_identity);
        let first_replacement = self.readd_backend(&pending_identity);
        assert!(!Rc::ptr_eq(&retired_pending, &first_replacement));
        let replacement_slot = self
            .reserve_request(
                &pending_identity.cluster_id,
                true,
                pending_identity.sticky_id.clone(),
            )
            .expect("the replacement backend is selectable");
        assert!(!Rc::ptr_eq(&retired_pending, &self.slots[replacement_slot]));

        self.resolve_pending(pending_slot, DialOutcome::Failed);
        self.assert_retired_drained(&retired_pending, pending_slot);
        self.resolve_pending(
            replacement_slot,
            DialOutcome::Connected(Duration::from_millis(7)),
        );

        let active_identity = self.upsert_slot(
            replacement_slot,
            "cluster-a-sticky-0-active-upsert".to_owned(),
        );
        let retired_active = self.remove_slot(replacement_slot, &active_identity);
        let second_replacement = self.readd_backend(&active_identity);
        assert!(!Rc::ptr_eq(&retired_active, &second_replacement));
        let second_replacement_slot = self
            .reserve_request(
                &active_identity.cluster_id,
                true,
                active_identity.sticky_id.clone(),
            )
            .expect("the second replacement backend is selectable");
        assert!(!Rc::ptr_eq(
            &retired_active,
            &self.slots[second_replacement_slot]
        ));
        self.resolve_pending(second_replacement_slot, DialOutcome::Failed);
        self.close_slot(replacement_slot);
        self.assert_retired_drained(&retired_active, replacement_slot);
    }

    fn live_active_slots(&self) -> Vec<usize> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(slot, handle)| {
                let is_live = self
                    .map
                    .backends
                    .values()
                    .any(|list| list.backends().iter().any(|live| Rc::ptr_eq(live, handle)));
                (is_live && handle.borrow().active_connections > 0).then_some(slot)
            })
            .collect()
    }

    fn resolve_random_pending(&mut self, ctx: &SimContext) {
        if self.pending.is_empty() {
            if self.reserve_random_request(ctx).is_some() {
                self.record.random_reservations += 1;
            }
            return;
        }
        let slot = self.pending[ctx.random().random_range(0..self.pending.len())];
        let outcome = if ctx.random().random_bool(0.25) {
            DialOutcome::Failed
        } else {
            DialOutcome::Connected(Duration::from_millis(ctx.random().random_range(1..200)))
        };
        self.resolve_pending(slot, outcome);
        self.record.random_resolutions += 1;
    }

    fn upsert_random_live_backend(&mut self, ctx: &SimContext, step: usize) {
        let candidates = self.live_active_slots();
        if candidates.is_empty() {
            if self.reserve_random_request(ctx).is_some() {
                self.record.random_reservations += 1;
            }
            return;
        }
        let slot = candidates[ctx.random().random_range(0..candidates.len())];
        self.upsert_slot(slot, format!("random-upsert-{}-{step}", current_sim_seed()));
        self.record.random_backend_upserts += 1;
    }

    fn remove_random_live_backend(&mut self, ctx: &SimContext) {
        let candidates = self.live_active_slots();
        if candidates.is_empty() {
            if self.reserve_random_request(ctx).is_some() {
                self.record.random_reservations += 1;
            }
            return;
        }
        let slot = candidates[ctx.random().random_range(0..candidates.len())];
        let identity = self.live_identity_for_slot(slot);
        self.remove_slot(slot, &identity);
        self.removed_identities.push(identity);
        self.record.random_backend_removes += 1;
    }

    fn add_random_backend(&mut self, ctx: &SimContext) {
        let readd = !self.removed_identities.is_empty() && ctx.random().random_bool(0.75);
        let identity = if readd {
            let index = ctx.random().random_range(0..self.removed_identities.len());
            self.removed_identities.swap_remove(index)
        } else {
            let ordinal = self.next_dynamic_backend;
            self.next_dynamic_backend = self
                .next_dynamic_backend
                .checked_add(1)
                .expect("the bounded simulation cannot exhaust backend ordinals");
            let cluster_id: Arc<str> =
                Arc::from(CLUSTERS[ctx.random().random_range(0..CLUSTERS.len())]);
            BackendIdentity {
                cluster_id,
                backend_id: format!("dynamic-{ordinal}"),
                address: SocketAddr::from(([10, 1, (ordinal >> 8) as u8, ordinal as u8], 30_000)),
                sticky_id: Some(format!("dynamic-sticky-{ordinal}")),
            }
        };
        self.readd_backend(&identity);
        self.record.random_backend_adds += 1;
        self.record.random_backend_readds += readd as usize;
    }

    fn seeded_step(&mut self, ctx: &SimContext, step: usize) {
        match ctx.random().random_range(0..16u8) {
            0..6 => {
                if self.reserve_random_request(ctx).is_some() {
                    self.record.random_reservations += 1;
                }
            }
            6..8 => self.resolve_random_pending(ctx),
            8..10 => self.close_one(ctx),
            10 => self.upsert_random_live_backend(ctx, step),
            11 => self.remove_random_live_backend(ctx),
            12 => self.add_random_backend(ctx),
            13 => self.select_and_dial(ctx),
            _ => {
                // Advance the harness's clock: sometimes within a backoff
                // window, sometimes past the longest one.
                let millis = if ctx.random().random_bool(0.2) {
                    ctx.random().random_range(1_000..70_000)
                } else {
                    ctx.random().random_range(1..500)
                };
                self.offset += Duration::from_millis(millis);
                self.trace(format_args!("advance {millis}ms"));
            }
        }
    }

    fn close_slot(&mut self, slot: usize) {
        let open_index = self
            .open
            .iter()
            .position(|open| *open == slot)
            .expect("the connection being closed is open");
        self.open.swap_remove(open_index);
        self.slots[slot].borrow_mut().dec_connections();
        *self
            .shadow
            .get_mut(&slot)
            .expect("an open connection was reserved") -= 1;
        self.record.closes += 1;
        self.trace(format_args!("close slot={slot}"));
    }

    /// Close one of the open connections, releasing its count.
    fn close_one(&mut self, ctx: &SimContext) {
        if self.open.is_empty() {
            return;
        }
        let slot = self.open[ctx.random().random_range(0..self.open.len())];
        self.close_slot(slot);
    }

    /// Every backend holds exactly the connections the shadow says it does.
    fn check(&self, step: usize) {
        for (slot, handle) in self.slots.iter().enumerate() {
            let expected = self.shadow.get(&slot).copied().unwrap_or(0);
            let actual = handle.borrow().active_connections;
            assert_eq!(
                actual,
                expected,
                "seed={:#x} step={step}: slot {slot} ({}) holds {actual} connections, the \
                 reservations, releases and closes this harness made add up to {expected}",
                current_sim_seed(),
                handle.borrow().backend_id
            );
        }
    }
}

/// `fmt::Write` over the trace buffer.
struct TraceLine<'a>(&'a mut Vec<u8>);

impl std::fmt::Write for TraceLine<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

#[async_trait]
impl Workload for BackendSelectionWorkload {
    fn name(&self) -> &'static str {
        "backend_selection_simulation"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let mut harness = Harness::new(ctx);
        harness.exercise_membership_mutations();
        assert_eq!(harness.record.backend_adds, 2);
        assert_eq!(harness.record.backend_removes, 2);
        assert_eq!(harness.record.backend_upserts, 2);
        assert_eq!(harness.record.retired_drains, 2);
        harness.check(0);
        for step in 0..self.steps {
            harness.seeded_step(ctx, step);
            harness.check(step);
        }

        // Drain every unresolved reservation, then every open connection, so
        // every live and retired incarnation returns to zero.
        while let Some(slot) = harness.pending.last().copied() {
            harness.resolve_pending(slot, DialOutcome::Failed);
        }
        while let Some(slot) = harness.open.pop() {
            harness.slots[slot].borrow_mut().dec_connections();
            *harness.shadow.get_mut(&slot).expect("reserved") -= 1;
            harness.record.closes += 1;
        }
        harness.check(self.steps);

        // Absolute check on the clock itself (a leaked wall-clock read would
        // make two runs agree vacuously): a backend's first failure opens a
        // one-second backoff window, which must close exactly one second
        // later on the HARNESS's clock, although almost no real time passes.
        if let Some(probe) = harness.slots.first().map(Rc::clone) {
            let at = harness.now();
            {
                let mut backend = probe.borrow_mut();
                backend.retry_policy.succeed(at);
                backend.retry_policy.fail(at, harness.map.rng());
            }
            assert_eq!(
                probe.borrow().retry_policy.can_try(harness.now()),
                Some(RetryAction::WAIT),
                "seed={:#x}: a fresh failure must open a backoff window",
                current_sim_seed()
            );
            harness.offset += Duration::from_secs(1);
            assert_eq!(
                probe.borrow().retry_policy.can_try(harness.now()),
                Some(RetryAction::OKAY),
                "seed={:#x}: the window must close one second later on the harness's clock",
                current_sim_seed()
            );
        }

        for (slot, handle) in harness.slots.iter().enumerate() {
            assert_eq!(
                handle.borrow().active_connections,
                0,
                "seed={:#x}: slot {slot} still holds connections after every one closed",
                current_sim_seed()
            );
        }
        for retired in &harness.retired {
            let retired = retired.borrow();
            assert_eq!(retired.status, BackendStatus::Closed);
            assert_eq!(retired.active_connections, 0);
        }
        assert_eq!(
            harness.retired.len(),
            2 + harness.record.random_backend_removes,
            "every seeded removal must preserve its retired allocation for the drain oracle"
        );

        if self.verbose {
            eprintln!("{}", String::from_utf8_lossy(&harness.record.trace));
        }
        if let Some(sink) = &self.sink {
            sink.lock()
                .expect("the record sink is not poisoned")
                .push(harness.record.clone());
        }
        Ok(())
    }
}

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

fn assert_no_failures(report: &SimulationReport) {
    if report.failed_runs != 0 {
        let errs: Vec<String> = report
            .individual_metrics
            .iter()
            .filter_map(|r| r.as_ref().err().map(|e| format!("{e:?}")))
            .collect();
        panic!(
            "failed_runs={} seeds_failing={:?}\nerrors:\n{}",
            report.failed_runs,
            report.seeds_failing,
            errs.join("\n")
        );
    }
}

/// The workload runs no logical time through moonpool (see the module docs),
/// so a small budget is ample.
const RUN_BUDGET: Duration = Duration::from_secs(3_600);

/// Seed sweep: at least 64 seeds, every invariant checked after every step.
#[test]
fn backend_selection_simulation_seed_sweep() {
    let steps = env_usize("SOZU_BACKEND_SELECTION_SIM_STEPS").unwrap_or(400);
    if let Some(seed) = env_u64("SOZU_BACKEND_SELECTION_SIM_SEED") {
        eprintln!("== backend-selection sim single-seed replay: seed={seed:#x} steps={steps} ==");
        let report = SimulationBuilder::new()
            .workload(BackendSelectionWorkload {
                steps,
                verbose: true,
                sink: None,
            })
            .set_debug_seeds(vec![seed])
            // Without an explicit count the builder keeps drawing fresh seeds
            // after this one; one iteration is what makes this a replay.
            .set_iterations(1)
            .run_time_budget(RUN_BUDGET)
            .run();
        assert_no_failures(&report);
        return;
    }
    let seeds = env_usize("SOZU_BACKEND_SELECTION_SIM_SEEDS")
        .unwrap_or(64)
        .max(64);
    let report = SimulationBuilder::new()
        .workload(BackendSelectionWorkload {
            steps,
            verbose: false,
            sink: None,
        })
        .set_debug_seeds((0..seeds as u64).collect())
        .set_iterations(seeds)
        .run_time_budget(RUN_BUDGET)
        .run();
    assert_no_failures(&report);
    assert_eq!(
        report.successful_runs, seeds,
        "every one of the {seeds} seeds must run to completion"
    );
}

/// Determinism guard: one seed, run twice, must produce byte-identical
/// traces. Paired with absolute checks, so two empty or degenerate traces
/// cannot pass: the run must have selected, failed dials, closed connections
/// and answered sticky cookies, and every count returned to zero (asserted
/// inside the workload).
#[test]
fn backend_selection_simulation_is_deterministic() {
    fn run(seed: u64) -> RunRecord {
        let sink: RecordSink = Arc::new(Mutex::new(Vec::new()));
        let report = SimulationBuilder::new()
            .workload(BackendSelectionWorkload {
                steps: 600,
                verbose: false,
                sink: Some(sink.clone()),
            })
            .set_debug_seeds(vec![seed])
            .set_iterations(1)
            .run_time_budget(RUN_BUDGET)
            .run();
        assert_no_failures(&report);
        let records = sink.lock().expect("the record sink is not poisoned");
        records
            .first()
            .cloned()
            .expect("the workload recorded its run")
    }

    // Several seeds, each run twice: between them they draw every kind of
    // policy, including the ones that consume the map's own generator.
    let mut seeded_policies = 0;
    let mut runs = Vec::new();
    for seed in 0x1684..0x1684 + 8 {
        let first = run(seed);
        let second = run(seed);
        assert_eq!(
            first.trace,
            second.trace,
            "seed={seed:#x}: one seed must yield one byte-identical trace:\n--- first ---\n{}\n--- \
             second ---\n{}",
            String::from_utf8_lossy(&first.trace),
            String::from_utf8_lossy(&second.trace)
        );
        assert_eq!(first, second, "seed={seed:#x}: and identical counts");
        assert!(
            first.selections >= 100,
            "seed={seed:#x}: the trace must record real selections, got {}",
            first.selections
        );
        assert!(
            first.failed_dials > 0,
            "seed={seed:#x}: some dials must fail and release"
        );
        assert!(
            first.closes > 0,
            "seed={seed:#x}: some connections must close"
        );
        assert!(
            first.sticky_answers > 0,
            "seed={seed:#x}: some selections must answer a sticky cookie"
        );
        assert_eq!(first.backend_adds, 2 + first.random_backend_adds);
        assert_eq!(first.backend_removes, 2 + first.random_backend_removes);
        assert_eq!(first.backend_upserts, 2 + first.random_backend_upserts);
        assert_eq!(
            first.retired_drains, 2,
            "seed={seed:#x}: both retired backend incarnations must drain to zero"
        );
        assert!(
            first.random_reservations > 0,
            "seed={seed:#x}: the seeded workload must leave dials pending"
        );
        assert!(
            first.random_resolutions > 0,
            "seed={seed:#x}: the seeded workload must resolve pending dials"
        );
        assert!(
            first.random_backend_adds > 0,
            "seed={seed:#x}: the seeded workload must add backend incarnations"
        );
        assert!(
            first.random_backend_readds > 0,
            "seed={seed:#x}: the seeded workload must re-add a removed identity"
        );
        assert!(
            first.random_backend_removes > 0,
            "seed={seed:#x}: the seeded workload must remove live backend incarnations"
        );
        assert!(
            first.random_backend_upserts > 0,
            "seed={seed:#x}: the seeded workload must upsert live backend incarnations"
        );
        seeded_policies += first.seeded_policies;
        runs.push(first.trace);
    }
    assert!(
        seeded_policies > 0,
        "no seed drew a Random or PowerOfTwo cluster, so the map's generator went untested"
    );
    runs.dedup();
    assert_eq!(
        runs.len(),
        8,
        "the trace must come from the seed: different seeds must differ"
    );
}
