//! The sans-io [`UdpManager`]: admission, flow table, LB request, timers.
//!
//! `UdpManager` owns the flow table (`HashMap<FlowKey, FlowId>` over a
//! `slab::Slab<UdpFlow>`), admission ("allocate nothing for unknown / over-cap
//! / invalid datagrams"), the flow-table cap and shedding, pluggable flow-key
//! extraction ([`FlowKeyExtractor`]), backend selection for new flows (from
//! the [`BackendSource`] view the embedder supplies), and the
//! timer scheduling: a **single armed manager-wide deadline** plus per-flow
//! **generation tokens** so a stale expiry can never close a refreshed flow.
//!
//! Pure: every entry point that depends on time takes `now: Instant`; the hash
//! seed is injected at construction. The shell drives the manager with
//! [`ManagerInput`] and drains [`Output`] via [`UdpManager::poll_output`].

use std::{
    collections::{HashMap, VecDeque},
    hash::{Hash, Hasher},
    net::SocketAddr,
    time::Instant,
};

use slab::Slab;

use crate::protocol::udp::{
    BackendSource, ClusterConfig, ConfigEvent, DropReason, FlowId, FlowKey, ManagerInput,
    MetricEvent, Output, Transmit,
    flow::{CloseReason, FlowPhase, UdpFlow},
    proxy_protocol::prepend_dgram_header,
};

/// Extracts a [`FlowKey`] from an admitted client datagram. The default
/// [`SourceTupleExtractor`] keys on the real client source address (2-tuple
/// source-IP or 4-tuple source-IP+port per cluster config). The trait is the
/// only seam for alternative keying (e.g. a QUIC-CID extractor, a non-goal);
/// the 4-tuple impl is the only one in scope.
pub trait FlowKeyExtractor {
    /// Compute the flow key for a datagram from `src`. Returns `None` to reject
    /// the datagram (the manager then emits `Drop(Invalid)` and allocates
    /// nothing). `cfg` is the listener's active cluster config.
    fn flow_key(&self, src: SocketAddr, payload: &[u8], cfg: &ClusterConfig) -> Option<FlowKey>;
}

/// The in-scope flow-key extractor: keys on the real client source address,
/// honouring the cluster's `affinity_with_port` knob (4-tuple vs 2-tuple).
#[derive(Clone, Copy, Debug, Default)]
pub struct SourceTupleExtractor;

impl FlowKeyExtractor for SourceTupleExtractor {
    fn flow_key(&self, src: SocketAddr, payload: &[u8], cfg: &ClusterConfig) -> Option<FlowKey> {
        // "Silence is a virtue": an empty datagram is not a valid flow trigger.
        if payload.is_empty() {
            return None;
        }
        Some(FlowKey::from_src(src, cfg.affinity_with_port))
    }
}

/// The pure UDP flow manager. Generic over the flow-key extractor so the seam
/// stays type-checked; the shell instantiates it with [`SourceTupleExtractor`].
pub struct UdpManager<E: FlowKeyExtractor = SourceTupleExtractor> {
    /// `FlowKey -> FlowId` lookup for tracked-flow reuse (two-tier selection).
    table: HashMap<FlowKey, FlowId>,
    /// Slab of admitted flows; the slot index is the [`FlowId`].
    flows: Slab<UdpFlow>,
    /// Flow-table cap. New flows beyond this are shed (drop + metric).
    max_flows: usize,
    /// Maximum accepted rx datagram size; larger datagrams are dropped as
    /// truncated.
    max_rx_datagram_size: usize,
    /// Active cluster routing + per-cluster knobs for *new* flows.
    cluster: ClusterConfig,
    /// Per-worker hash seed, injected once at construction; persisted across
    /// reconfig for stable affinity.
    hash_seed: u64,
    /// Pluggable flow-key extractor.
    extractor: E,
    /// Draining: admit no new flows; let existing ones reach teardown.
    draining: bool,

    /// FIFO of outputs the shell drains via [`poll_output`].
    outputs: VecDeque<Output>,
    /// The single armed manager-wide deadline currently reflected to the shell
    /// via the last `ArmTimer`. `None` means no timer is armed — which
    /// [`handle_timeout`](Self::handle_timeout) sets on entry, because the
    /// expiry that called it consumed the shell's wheel entry.
    armed_deadline: Option<Instant>,

    /// High-water mark of every `max_flows` cap ever set (construction +
    /// `SetMaxFlows`). The live cap can be shrunk below `flows.len()` by a
    /// `SetMaxFlows`, so `flows.len() <= max_flows` is NOT an invariant; but a
    /// flow can only ever have been admitted under *some* cap that was in force
    /// at admission, so `flows.len() <= max_flows_high_water` always holds.
    /// Debug-only — only read by [`check_invariants`](Self::check_invariants).
    #[cfg(debug_assertions)]
    max_flows_high_water: usize,
}

impl UdpManager<SourceTupleExtractor> {
    /// Construct a manager with the default 4-tuple/2-tuple source extractor.
    pub fn new(
        cluster: ClusterConfig,
        max_flows: usize,
        max_rx_datagram_size: usize,
        hash_seed: u64,
    ) -> Self {
        Self::with_extractor(
            cluster,
            max_flows,
            max_rx_datagram_size,
            hash_seed,
            SourceTupleExtractor,
        )
    }
}

impl<E: FlowKeyExtractor> UdpManager<E> {
    /// Construct a manager with a custom flow-key extractor.
    pub fn with_extractor(
        cluster: ClusterConfig,
        max_flows: usize,
        max_rx_datagram_size: usize,
        hash_seed: u64,
        extractor: E,
    ) -> Self {
        UdpManager {
            table: HashMap::new(),
            flows: Slab::new(),
            max_flows,
            max_rx_datagram_size,
            cluster,
            hash_seed,
            extractor,
            draining: false,
            outputs: VecDeque::new(),
            armed_deadline: None,
            #[cfg(debug_assertions)]
            max_flows_high_water: max_flows,
        }
    }

    // ---- introspection (used by log macros / tests / the shell) ------------

    /// Number of currently admitted flows. Mirrors `udp.active_flows`.
    pub fn flow_count(&self) -> usize {
        self.flows.len()
    }

    /// The configured flow-table cap.
    pub fn max_flows(&self) -> usize {
        self.max_flows
    }

    /// Whether the listener is draining.
    pub fn is_draining(&self) -> bool {
        self.draining
    }

    /// Whether the active cluster config keys flows on the 4-tuple (source
    /// IP + port) rather than source IP only. The shell needs this to mirror
    /// the manager's flow keying for its `SendToBackend` socket resolution.
    pub fn affinity_with_port(&self) -> bool {
        self.cluster.affinity_with_port
    }

    /// Borrow a flow by id (for the shell's access log on close).
    pub fn flow(&self, flow: FlowId) -> Option<&UdpFlow> {
        self.flows.get(flow)
    }

    // ---- input -------------------------------------------------------------

    /// Feed one input into the manager. Pure: `now` is injected.
    pub fn handle_input(&mut self, input: ManagerInput<'_>, now: Instant) {
        match input {
            ManagerInput::ClientDatagram {
                src,
                payload,
                backends,
            } => self.on_client_datagram(src, payload, backends, now),
            ManagerInput::BackendDatagram { flow, payload } => {
                self.on_backend_datagram(flow, payload, now)
            }
            ManagerInput::Config(event) => self.on_config(event, now),
        }
        // Post-condition: the public method ran to completion under the caller's
        // lock, so every structural invariant must hold again.
        self.debug_assert_invariants();
    }

    /// Tear down a single flow on demand, emitting the same outputs a normal
    /// idle close produces — `Output::Metric(MetricEvent::FlowEvicted)` then
    /// `Output::CloseFlow(flow)` — so the shell draining `poll_output()`
    /// decrements `udp.active_flows` and frees the upstream socket exactly once.
    ///
    /// The shell calls this when it cannot establish a flow it just admitted:
    /// the upstream `connect()` failed (EMFILE / connection refused) or no
    /// backend resolved. Without it the flow would sit `AwaitingBackend` /
    /// `Established` pinning a `max_flows` slot for the full idle timeout.
    ///
    /// Idempotent: a missing flow or one already `Closing` is a no-op — no
    /// double-evict, no gauge underflow. Works in either `AwaitingBackend` or
    /// `Established`. `now` is accepted for signature symmetry with the other
    /// time-driven entry points (the teardown itself is time-independent).
    pub fn abort_flow(&mut self, flow: FlowId, _now: Instant, reason: CloseReason) {
        self.close_flow(flow, reason);
        self.debug_assert_invariants();
    }

    /// Tear down EVERY live flow, emitting — for each — the same outputs a
    /// normal idle close produces (`Output::Metric(MetricEvent::FlowEvicted)`
    /// then `Output::CloseFlow(flow)`), so the shell draining `poll_output()`
    /// decrements `udp.active_flows` and frees each upstream socket exactly once
    /// per flow.
    ///
    /// The shell calls this on listener remove / deactivate / soft-stop before
    /// dropping the manager, so the active-flows gauge does not leak. Draining
    /// the flow table + slab to zero; a flow already `Closing` is skipped
    /// (idempotent, no double-evict, no underflow). After this returns,
    /// `flow_count() == 0` and no timer is armed.
    pub fn close_all(&mut self, now: Instant) {
        // Snapshot the live ids first: `close_flow` mutates the slab, so we must
        // not iterate it while closing. `Closing` slots are already gone from
        // the slab (`close_flow` removes them), so every id here is live.
        let live: Vec<FlowId> = self.flows.iter().map(|(id, _)| id).collect();
        for flow_id in live {
            self.abort_flow(flow_id, now, CloseReason::Drain);
        }
        // Post: the table and slab are drained to zero and no timer is armed.
        #[cfg(debug_assertions)]
        {
            debug_assert_eq!(self.flow_count(), 0, "close_all must drain every flow");
            debug_assert!(
                self.table.is_empty(),
                "close_all must clear every table entry"
            );
            debug_assert!(
                self.armed_deadline.is_none(),
                "close_all must leave no armed timer"
            );
        }
        self.debug_assert_invariants();
    }

    fn on_client_datagram(
        &mut self,
        src: SocketAddr,
        payload: &[u8],
        backends: &mut dyn BackendSource,
        now: Instant,
    ) {
        // Over-size check first — never allocate for a truncated datagram.
        if payload.len() > self.max_rx_datagram_size {
            self.drop_datagram(DropReason::Truncated);
            return;
        }
        // No backend cluster configured → nothing to route to.
        if self.cluster.cluster.is_empty() {
            self.drop_datagram(DropReason::NoBackend);
            return;
        }
        // Extract the flow key; rejection allocates nothing.
        let key = match self.extractor.flow_key(src, payload, &self.cluster) {
            Some(key) => key,
            None => {
                self.drop_datagram(DropReason::Invalid);
                return;
            }
        };

        // Tracked flow → reuse its backend (two-tier selection).
        if let Some(&flow_id) = self.table.get(&key) {
            self.forward_on_existing_flow(flow_id, payload, now);
            return;
        }

        // New flow. Draining or at cap → shed, allocate nothing.
        // Drop / shed paths ("silence is a virtue"): a reject must allocate
        // nothing, so `flows.len()` is unchanged across it. Snapshot the count
        // to pair-assert that on every early return below. Not debug-gated: the
        // `debug_assert_eq!`s that read it compile in release too (execution only
        // is gated); the read is dead there and the optimizer drops the binding.
        let flows_before_admit = self.flows.len();
        if self.draining {
            self.drop_datagram(DropReason::Shed);
            debug_assert_eq!(
                self.flows.len(),
                flows_before_admit,
                "drain shed must allocate no flow"
            );
            return;
        }
        if self.flows.len() >= self.max_flows {
            self.outputs
                .push_back(Output::Metric(MetricEvent::FlowShed));
            self.drop_datagram(DropReason::Shed);
            debug_assert_eq!(
                self.flows.len(),
                flows_before_admit,
                "cap shed must allocate no flow"
            );
            return;
        }

        // Admit. Pre-conditions (positive + negative space): we are on the admit
        // path, so there is room under the live cap AND the key is NOT already
        // tracked (a tracked key would have been served above without a new
        // slot — a double-insert would orphan the previous flow and leak a slab
        // slot).
        debug_assert!(
            self.flows.len() < self.max_flows,
            "admit path entered while at/over the cap"
        );
        debug_assert!(
            !self.table.contains_key(&key),
            "admit path entered for a key already in the table (would orphan a flow)"
        );

        // Select the backend HERE, in the core, from the view the embedder
        // handed in with this datagram (#1340, Question 6). The affinity hash
        // is computed from a provisional flow so HRW / Maglev keep a client
        // pinned; it reads only `client` and `config`, neither of which the
        // backend can change.
        let key_hash = self.affinity_hash(&UdpFlow::new(
            src,
            self.cluster.clone(),
            String::new(),
            src,
            now,
        ));
        let Some((backend_id, backend_addr)) =
            backends.select(&self.cluster.cluster, Some(key_hash))
        else {
            // The cluster has nothing that can serve. Drop without allocating
            // a slab slot: there is no flow to park and nothing to abort.
            self.drop_datagram(DropReason::NoBackend);
            debug_assert_eq!(
                self.flows.len(),
                flows_before_admit,
                "a failed selection must allocate no flow"
            );
            return;
        };

        // Admit: one slab slot. The flow is `Established` from birth — its
        // backend was chosen a line ago — so there is no window in which it
        // exists without one, and no datagram to buffer against that window.
        let flow = UdpFlow::new(src, self.cluster.clone(), backend_id, backend_addr, now);
        let flow_id = self.flows.insert(flow);
        self.table.insert(key, flow_id);

        // Post (admission): exactly one slot was added and the key now maps to
        // it. Pairs the pre-conditions above (grew by exactly 1, not 0 or 2).
        debug_assert_eq!(
            self.flows.len(),
            flows_before_admit + 1,
            "admission must add exactly one flow"
        );
        debug_assert_eq!(
            self.table.get(&key),
            Some(&flow_id),
            "admission must map the key to the new flow"
        );

        self.outputs
            .push_back(Output::Metric(MetricEvent::FlowCreated));
        // Ask the shell to open the connected upstream socket and register
        // `upstream_token -> flow` for NAT return demux. This is the returned
        // request the embedder fulfils — the shape `Output::SelectBackend` used
        // to have, except the decision it carries is now the core's.
        self.outputs.push_back(Output::OpenUpstream {
            flow: flow_id,
            backend: backend_addr,
        });

        // Forward the admission datagram immediately. It used to be buffered
        // until the shell resolved a backend; there is no such window now, so
        // it is a forward like any other and is counted as one.
        self.forward_admission_datagram(flow_id, payload, backend_addr, now);
    }

    /// Send the datagram that caused a flow's admission, counting it as the
    /// forward it now is.
    ///
    /// Before selection moved into the core this datagram sat in a one-slot
    /// newest-wins buffer until `BackendResolved` arrived, and a second
    /// datagram in that window replaced it. The window is gone, so the buffer
    /// is too: every datagram of an opening burst is forwarded.
    fn forward_admission_datagram(
        &mut self,
        flow_id: FlowId,
        payload: &[u8],
        backend: SocketAddr,
        now: Instant,
    ) {
        let Some(flow) = self.flows.get_mut(flow_id) else {
            debug_assert!(false, "the flow was inserted one statement ago");
            return;
        };
        let mut out = payload.to_vec();
        flow.on_client_datagram(now);
        if flow.take_proxy_protocol() {
            prepend_dgram_header(&mut out, flow.client, backend);
        }
        let teardown = flow.teardown_reason();
        self.outputs
            .push_back(Output::Metric(MetricEvent::DatagramIn(payload.len())));
        self.outputs.push_back(Output::SendToBackend(Transmit {
            dst: backend,
            segment_size: None,
            payload: out,
        }));
        if let Some(reason) = teardown {
            self.close_flow(flow_id, reason);
            return;
        }
        // `on_client_datagram` already refreshed the deadline + generation.
        self.reschedule();
    }

    fn forward_on_existing_flow(&mut self, flow_id: FlowId, payload: &[u8], now: Instant) {
        let Some(flow) = self.flows.get_mut(flow_id) else {
            // Table/slab desync should be impossible, but never panic on it.
            self.drop_datagram(DropReason::UnknownFlow);
            return;
        };

        match flow.phase {
            FlowPhase::Established => {
                flow.on_client_datagram(now);
                let backend = flow
                    .backend_addr
                    .expect("Established flow always has a backend address");
                let mut out = payload.to_vec();
                if flow.take_proxy_protocol() {
                    prepend_dgram_header(&mut out, flow.client, backend);
                }
                let teardown = flow.teardown_reason();
                self.outputs
                    .push_back(Output::Metric(MetricEvent::DatagramIn(payload.len())));
                self.outputs.push_back(Output::SendToBackend(Transmit {
                    dst: backend,
                    segment_size: None,
                    payload: out,
                }));
                if let Some(reason) = teardown {
                    self.close_flow(flow_id, reason);
                } else {
                    self.reschedule();
                }
            }
            FlowPhase::Closing => {
                // Racing datagram against a teardown already decided: drop it.
                self.drop_datagram(DropReason::Shed);
            }
        }
    }

    fn on_backend_datagram(&mut self, flow_id: FlowId, payload: &[u8], now: Instant) {
        if payload.len() > self.max_rx_datagram_size {
            self.drop_datagram(DropReason::Truncated);
            return;
        }
        let Some(flow) = self.flows.get_mut(flow_id) else {
            self.drop_datagram(DropReason::UnknownFlow);
            return;
        };
        if flow.phase != FlowPhase::Established {
            self.drop_datagram(DropReason::UnknownFlow);
            return;
        }
        flow.on_backend_datagram(now);
        let client = flow.client;
        let teardown = flow.teardown_reason();
        self.outputs
            .push_back(Output::Metric(MetricEvent::DatagramOut(payload.len())));
        self.outputs.push_back(Output::SendToClient(Transmit {
            dst: client,
            segment_size: None,
            payload: payload.to_vec(),
        }));
        if let Some(reason) = teardown {
            self.close_flow(flow_id, reason);
        } else {
            self.reschedule();
        }
    }

    fn on_config(&mut self, event: ConfigEvent, _now: Instant) {
        match event {
            ConfigEvent::SetCluster(cfg) => self.cluster = cfg,
            ConfigEvent::SetMaxFlows(n) => {
                self.max_flows = n;
                // Track the high-water mark: a `SetMaxFlows` may shrink the live
                // cap below `flows.len()` (documented), so the only durable cap
                // bound is the largest cap ever in force.
                #[cfg(debug_assertions)]
                {
                    self.max_flows_high_water = self.max_flows_high_water.max(n);
                }
            }
            ConfigEvent::SetMaxRxDatagramSize(n) => self.max_rx_datagram_size = n,
            ConfigEvent::Drain => self.draining = true,
        }
    }

    // ---- timers ------------------------------------------------------------

    /// Fire all flows whose idle deadline has elapsed at `now`. A flow is only
    /// closed if its generation token still matches the scheduled deadline —
    /// generation mismatch means the flow saw traffic and was rescheduled, so
    /// the stale expiry is ignored (defeats the busy-loop / stale-close bug).
    ///
    /// Called ONLY from a wheel expiry: the shell's single timer entry has just
    /// been delivered and consumed. `now` is therefore the wheel's tick date,
    /// not the deadline — `crate::timer` rounds a delay to the nearest tick, so
    /// with a 100 ms tick the entry arrives up to 50 ms EARLY on the tick grid
    /// (up to 99 ms off it; see `duration_to_tick`) and no flow need be due at
    /// all. Either way the shell now holds nothing, so
    /// `armed_deadline` is cleared on entry and `reschedule`
    /// re-emits `ArmTimer` even when the minimum deadline has not moved.
    /// Without that, an early expiry is a LOST WAKEUP: nothing is closed,
    /// nothing is re-armed, and the flow is never reaped until some other
    /// flow's deadline happens to change the minimum. This is the
    /// consume-then-reschedule rule, and its cost is one extra wheel wakeup per
    /// early-fired expiry.
    pub fn handle_timeout(&mut self, now: Instant) {
        // The wheel entry that brought us here is gone. Record that BEFORE any
        // `reschedule` — including the ones `close_flow` runs below — so a
        // recomputed deadline equal to the old one is still emitted as a fresh
        // `ArmTimer` instead of being memoized away.
        self.armed_deadline = None;

        // Collect due flow ids first to avoid borrowing the slab while mutating.
        let due: Vec<FlowId> = self
            .flows
            .iter()
            .filter(|(_, flow)| flow.idle_deadline <= now)
            .map(|(id, _)| id)
            .collect();
        for flow_id in due {
            // Re-check under current state (a flow may have been closed by an
            // earlier iteration's teardown, though here ids are disjoint).
            if let Some(flow) = self.flows.get(flow_id)
                && flow.idle_deadline <= now
                && flow.phase != FlowPhase::Closing
            {
                self.close_flow(flow_id, CloseReason::Idle);
            }
        }
        self.reschedule();

        // Strict-advance guard: after firing every flow due at `now`, the next
        // armed deadline (if any) MUST be strictly greater than `now`. A
        // deadline `<= now` would make the shell immediately re-fire and spin —
        // the canonical sans-io busy-loop bug. This is the real reason the
        // generation tokens + `reschedule` exist.
        #[cfg(debug_assertions)]
        if let Some(next) = self.armed_deadline {
            debug_assert!(
                next > now,
                "UdpManager::poll_timeout must strictly advance past a firing: \
                 armed {next:?} <= fired_at {now:?} (busy-loop)"
            );
        }

        // Post: every flow due at `now` was reaped — no live flow may retain a
        // deadline `<= now`. Pair with the strict-advance guard above: that one
        // proves the next *armed* deadline advanced, this one proves no *flow*
        // was left behind due (a leak the armed-deadline check alone misses,
        // since min() over an empty set is None regardless of stragglers).
        #[cfg(debug_assertions)]
        for (id, flow) in self.flows.iter() {
            debug_assert!(
                flow.idle_deadline > now,
                "FlowId {id} still due after handle_timeout: deadline {:?} <= now {now:?}",
                flow.idle_deadline,
            );
        }
        self.debug_assert_invariants();
    }

    /// The next manager-wide deadline, or `None` if no flow is armed. After a
    /// [`Self::handle_timeout`] at deadline `d`, the value returned here is guaranteed
    /// `> d` (or `None`) — the strict-advance invariant `handle_timeout` asserts
    /// in debug builds, which is what stops the shell busy-looping.
    pub fn poll_timeout(&self) -> Option<Instant> {
        self.armed_deadline
    }

    /// Drain the next queued output, or `None` when the queue is empty.
    pub fn poll_output(&mut self) -> Option<Output> {
        self.outputs.pop_front()
    }

    // ---- internals ---------------------------------------------------------

    /// Recompute the earliest flow deadline and emit `ArmTimer` only when it
    /// differs from `armed_deadline`, so the shell re-arms its wheel exactly
    /// once per real change.
    ///
    /// The memoization is against what the SHELL currently holds, not against
    /// the previous minimum: [`handle_timeout`](Self::handle_timeout) clears
    /// `armed_deadline` on entry because the expiry consumed that wheel entry,
    /// so an unchanged minimum is re-emitted there rather than swallowed. A
    /// memoization against the minimum alone would be a lost wakeup on every
    /// expiry the wheel delivered early.
    fn reschedule(&mut self) {
        let next = self
            .flows
            .iter()
            .filter(|(_, f)| f.phase != FlowPhase::Closing)
            .map(|(_, f)| f.idle_deadline)
            .min();
        if next != self.armed_deadline {
            self.armed_deadline = next;
            if let Some(deadline) = next {
                self.outputs.push_back(Output::ArmTimer(deadline));
            }
        }
    }

    /// Tear down a flow: remove it from the table + slab, emit `CloseFlow` and
    /// the eviction metric. Idempotent — a missing flow is a no-op (never an
    /// underflow). Re-arms the manager timer.
    fn close_flow(&mut self, flow_id: FlowId, _reason: CloseReason) {
        let Some(flow) = self.flows.get_mut(flow_id) else {
            return;
        };
        if flow.phase == FlowPhase::Closing {
            return;
        }
        flow.set_phase(FlowPhase::Closing);
        let key = FlowKey::from_src(flow.client, self.cluster.affinity_with_port);
        // Remove the table entry only if it still points at this flow; a
        // recreated flow under the same key must not be unmapped.
        if self.table.get(&key) == Some(&flow_id) {
            self.table.remove(&key);
        } else {
            // The flow was keyed when admitted; recompute via its own config to
            // be robust to a mid-flow affinity change.
            let own_key = FlowKey::from_src(flow.client, flow.config.affinity_with_port);
            if self.table.get(&own_key) == Some(&flow_id) {
                self.table.remove(&own_key);
            }
        }
        self.flows.remove(flow_id);
        self.outputs
            .push_back(Output::Metric(MetricEvent::FlowEvicted));
        self.outputs.push_back(Output::CloseFlow(flow_id));
        self.reschedule();

        // Post: the slot is gone from the slab AND no table key still maps to it
        // (a stale table key would dangle — caught by check_invariants (1), but
        // assert it here too so the local failure points at close_flow). Pair:
        // positive = removed from slab; negative = no key still references it.
        #[cfg(debug_assertions)]
        {
            debug_assert!(
                !self.flows.contains(flow_id),
                "close_flow left FlowId {flow_id} in the slab"
            );
            debug_assert!(
                self.table.values().all(|&id| id != flow_id),
                "close_flow left a table entry mapping to the removed FlowId {flow_id}"
            );
        }
    }

    /// Emit a drop with its by-reason metric. Allocates nothing per the
    /// "silence is a virtue" posture.
    fn drop_datagram(&mut self, reason: DropReason) {
        // "Silence is a virtue": a drop allocates no flow and frees none — the
        // slab is untouched across the reject. Snapshot + pair-assert so a future
        // edit that accidentally mutates the slab on a drop path is caught loudly.
        // Not debug-gated: the `debug_assert_eq!` reads it in release too (only
        // execution is gated); dead there, dropped by the optimizer.
        let flows_before_drop = self.flows.len();
        self.outputs
            .push_back(Output::Metric(MetricEvent::DatagramDropped(reason)));
        self.outputs.push_back(Output::Drop(reason));
        debug_assert_eq!(
            self.flows.len(),
            flows_before_drop,
            "a drop must allocate nothing and free nothing (flows.len() unchanged)"
        );
    }

    /// Affinity hash for a flow: `hash(seed, affinity_key)`. The shell feeds
    /// this into HRW (`max hash`) / Maglev (`key % M`) / RR (ignores it).
    fn affinity_hash(&self, flow: &UdpFlow) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.hash_seed.hash(&mut hasher);
        if flow.config.affinity_with_port {
            flow.client.hash(&mut hasher);
        } else {
            flow.client.ip().hash(&mut hasher);
        }
        hasher.finish()
    }

    /// TigerStyle invariant sweep (TigerBeetle / FoundationDB style). A single
    /// full check of every structural invariant the manager must preserve,
    /// asserted at the END of every public mutating method via
    /// [`debug_assert_invariants`](Self::debug_assert_invariants). Compiled out
    /// entirely in release (`#[cfg(debug_assertions)]`); on in every test / e2e /
    /// fuzz / dev build. It must NEVER change runtime behavior — it only reads.
    ///
    /// The right place for the sweep is a *post-condition*: these public methods
    /// run to completion under the caller's lock, so the table/slab/timer state
    /// is fully reconciled by the time the method returns.
    #[cfg(debug_assertions)]
    fn check_invariants(&self) {
        use std::collections::HashSet;

        // (3) flow_count() is exactly the slab population.
        debug_assert_eq!(
            self.flow_count(),
            self.flows.len(),
            "flow_count() must equal flows.len()"
        );

        // (1) Table -> slab consistency + (2) table injectivity: every FlowId in
        // the table points at a live slab slot, and no two keys share a FlowId.
        let mut seen_ids: HashSet<FlowId> = HashSet::with_capacity(self.table.len());
        for (key, &id) in self.table.iter() {
            debug_assert!(
                self.flows.contains(id),
                "table key {key:?} maps to FlowId {id} absent from the slab (dangling key)"
            );
            debug_assert!(
                seen_ids.insert(id),
                "table injectivity violated: FlowId {id} is the target of two distinct FlowKeys"
            );
        }
        // Pair (negative space): a live flow that is reachable from the table is
        // mapped exactly once — there is never a live flow with two table keys.
        debug_assert!(
            seen_ids.len() <= self.flows.len(),
            "more distinct table targets than live flows"
        );

        // Per-flow invariants over the slab.
        let mut min_live_deadline: Option<Instant> = None;
        let mut live_count = 0usize;
        for (id, flow) in self.flows.iter() {
            // (4) No slab flow is Closing. close_flow sets Closing then removes
            // the slot in the same call, so a Closing flow must never persist.
            // Pair: positive space — every live flow is Awaiting or Established.
            debug_assert_ne!(
                flow.phase,
                FlowPhase::Closing,
                "FlowId {id} persists in the slab while Closing (close_flow must remove it)"
            );
            debug_assert_eq!(
                flow.phase,
                FlowPhase::Established,
                "FlowId {id} has an unexpected live phase"
            );

            // (5) A live flow always carries the backend it was admitted on.
            // The negative half of this pair — a phase that carried no address
            // — went with `AwaitingBackend`: selection now happens before the
            // flow exists, so there is no such state to assert about.
            debug_assert!(
                flow.backend_addr.is_some(),
                "live FlowId {id} has no backend address"
            );

            // (7) Counters within caps OR a teardown is due. A flow whose cap is
            // exhausted must report a teardown reason; an exhausted flow that
            // reports None would be an immortal flow (the cap silently lost).
            if flow.requests_exhausted() || flow.responses_exhausted() {
                debug_assert!(
                    flow.teardown_reason().is_some(),
                    "FlowId {id} exhausted a cap (req {}/{}, resp {}/{}) but reports no teardown",
                    flow.requests_seen,
                    flow.config.requests,
                    flow.responses_seen,
                    flow.config.responses,
                );
            } else {
                // Pair (negative): a flow within both caps must NOT report a
                // cap-driven teardown (idle teardown is handled by the timer,
                // not teardown_reason()).
                debug_assert!(
                    flow.teardown_reason().is_none(),
                    "FlowId {id} reports a teardown while within both caps"
                );
            }

            live_count += 1;
            min_live_deadline = Some(match min_live_deadline {
                Some(d) => d.min(flow.idle_deadline),
                None => flow.idle_deadline,
            });
        }

        // The high-water cap bounds the live population at all times (the live
        // cap itself can be shrunk below flows.len() by SetMaxFlows, so we assert
        // against the largest cap ever in force, not max_flows).
        debug_assert!(
            self.flows.len() <= self.max_flows_high_water,
            "live flows {} exceed the high-water cap {}",
            self.flows.len(),
            self.max_flows_high_water,
        );

        // (6) Timer coherence: armed_deadline is Some IFF at least one (non-
        // Closing) live flow exists, and when Some it equals the minimum
        // idle_deadline over live flows. Closing flows are never in the slab
        // (invariant 4), so "live" == "slab" here.
        debug_assert_eq!(
            self.armed_deadline.is_some(),
            live_count > 0,
            "timer coherence: armed_deadline.is_some() ({}) must match having live flows ({})",
            self.armed_deadline.is_some(),
            live_count > 0,
        );
        if let Some(armed) = self.armed_deadline {
            debug_assert_eq!(
                Some(armed),
                min_live_deadline,
                "timer coherence: armed deadline {armed:?} must equal the minimum live idle deadline {min_live_deadline:?}"
            );
        }
    }

    /// Run the full [`check_invariants`](Self::check_invariants) sweep, but only
    /// in debug builds. A thin wrapper so call sites read as one line and so the
    /// whole sweep — and any read it performs — is dead-stripped in release.
    #[inline]
    fn debug_assert_invariants(&self) {
        #[cfg(debug_assertions)]
        self.check_invariants();
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::{IpAddr, Ipv4Addr},
        time::Duration,
    };

    use super::*;

    /// The backend set these tests select from.
    ///
    /// Selection moved into the core (#1340, Question 6), so a test that
    /// admits a flow has to supply one. Round-robin over whatever it holds;
    /// `none()` is the cluster-has-nothing case that makes admission fail,
    /// which is a live path because
    /// `BackendMap::backend_from_cluster_id_with_key` has three
    /// `NoBackendForCluster` returns.
    struct TestBackends {
        backends: Vec<(String, SocketAddr)>,
        next: usize,
    }

    impl TestBackends {
        /// One backend, which is what every test that is not about selection
        /// wants.
        fn one() -> Self {
            Self {
                backends: vec![("b1".to_owned(), backend_addr(1))],
                next: 0,
            }
        }

        /// A cluster with nothing that can serve.
        fn none() -> Self {
            Self {
                backends: Vec::new(),
                next: 0,
            }
        }
    }

    impl BackendSource for TestBackends {
        fn select(&mut self, _cluster: &str, _key: Option<u64>) -> Option<(String, SocketAddr)> {
            if self.backends.is_empty() {
                return None;
            }
            let picked = self.backends[self.next % self.backends.len()].clone();
            self.next += 1;
            Some(picked)
        }
    }

    fn backend_addr(n: u8) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, n)), 5353)
    }

    fn cluster(name: &str) -> ClusterConfig {
        ClusterConfig {
            cluster: name.to_owned(),
            front_timeout: Duration::from_secs(30),
            back_timeout: Duration::from_secs(30),
            ..Default::default()
        }
    }

    fn client(n: u8, port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, n)), port)
    }

    /// Drain all outputs into a Vec for assertions.
    fn drain(mgr: &mut UdpManager) -> Vec<Output> {
        let mut out = Vec::new();
        while let Some(o) = mgr.poll_output() {
            out.push(o);
        }
        out
    }

    #[test]
    fn unknown_datagram_allocates_nothing_when_no_cluster() {
        let mut mgr = UdpManager::new(ClusterConfig::default(), 16, 65535, 0xABCD);
        let now = Instant::now();
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src: client(1, 1000),
                payload: b"hi",
                backends: &mut TestBackends::one(),
            },
            now,
        );
        assert_eq!(mgr.flow_count(), 0);
        let outs = drain(&mut mgr);
        assert!(
            outs.iter()
                .any(|o| matches!(o, Output::Drop(DropReason::NoBackend)))
        );
        assert!(
            !outs
                .iter()
                .any(|o| matches!(o, Output::OpenUpstream { .. }))
        );
    }

    #[test]
    fn empty_datagram_is_invalid() {
        let mut mgr = UdpManager::new(cluster("dns"), 16, 65535, 1);
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src: client(1, 1000),
                payload: b"",
                backends: &mut TestBackends::one(),
            },
            Instant::now(),
        );
        assert_eq!(mgr.flow_count(), 0);
        let outs = drain(&mut mgr);
        assert!(
            outs.iter()
                .any(|o| matches!(o, Output::Drop(DropReason::Invalid)))
        );
    }

    #[test]
    fn truncated_datagram_dropped_before_admission() {
        let mut mgr = UdpManager::new(cluster("dns"), 16, 4, 1);
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src: client(1, 1000),
                payload: b"toolong",
                backends: &mut TestBackends::one(),
            },
            Instant::now(),
        );
        assert_eq!(mgr.flow_count(), 0);
        let outs = drain(&mut mgr);
        assert!(
            outs.iter()
                .any(|o| matches!(o, Output::Drop(DropReason::Truncated)))
        );
    }

    /// A new flow opens an upstream and forwards its admission datagram, in
    /// one call.
    ///
    /// Retargeted from `new_flow_requests_backend_then_forwards_buffered_datagram`.
    /// That name described a two-step: the core asked the shell to choose a
    /// backend, buffered the datagram, and flushed it when the shell replied.
    /// Selection is the core's now, so there is no window and nothing to
    /// buffer — the datagram is forwarded on the way in.
    ///
    /// Its "no SendToBackend yet, backend not resolved" assertion is the one
    /// casualty: there is no *yet*. Its partner — that the datagram does reach
    /// the backend, intact and at the right address — survives and is below.
    #[test]
    fn a_new_flow_opens_an_upstream_and_forwards_its_admission_datagram() {
        let mut mgr = UdpManager::new(cluster("dns"), 16, 65535, 7);
        let now = Instant::now();
        let src = client(1, 1000);
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src,
                payload: b"query",
                backends: &mut TestBackends::one(),
            },
            now,
        );
        assert_eq!(mgr.flow_count(), 1);
        let outs = drain(&mut mgr);
        let opened = outs
            .iter()
            .find_map(|o| match o {
                Output::OpenUpstream { flow, backend } => Some((*flow, *backend)),
                _ => None,
            })
            .expect("admission must ask the shell to open an upstream");
        assert_eq!(
            opened.1,
            backend_addr(1),
            "the upstream must be opened to the backend the core selected"
        );
        let sent = outs
            .iter()
            .find_map(|o| match o {
                Output::SendToBackend(t) => Some(t.clone()),
                _ => None,
            })
            .expect("the admission datagram must be forwarded in the same call");
        assert_eq!(sent.dst, backend_addr(1));
        assert_eq!(sent.payload, b"query");
    }

    #[test]
    fn tracked_flow_reuses_backend() {
        let mut mgr = UdpManager::new(cluster("dns"), 16, 65535, 7);
        let now = Instant::now();
        let src = client(1, 1000);
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src,
                payload: b"q1",
                backends: &mut TestBackends::one(),
            },
            now,
        );
        let _select_flow = drain(&mut mgr)
            .into_iter()
            .find_map(|o| match o {
                Output::OpenUpstream { flow, .. } => Some(flow),
                _ => None,
            })
            .unwrap();
        drain(&mut mgr);

        // Second datagram from same source: no new SelectBackend, direct send.
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src,
                payload: b"q2",
                backends: &mut TestBackends::one(),
            },
            now,
        );
        assert_eq!(mgr.flow_count(), 1);
        let outs = drain(&mut mgr);
        assert!(
            !outs
                .iter()
                .any(|o| matches!(o, Output::OpenUpstream { .. }))
        );
        assert!(
            outs.iter()
                .any(|o| matches!(o, Output::SendToBackend(t) if t.payload == b"q2"))
        );
    }

    #[test]
    fn responses_one_closes_flow_after_single_reply() {
        let mut cfg = cluster("dns");
        cfg.responses = 1;
        let mut mgr = UdpManager::new(cfg, 16, 65535, 7);
        let now = Instant::now();
        let src = client(1, 1000);
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src,
                payload: b"q",
                backends: &mut TestBackends::one(),
            },
            now,
        );
        let flow = drain(&mut mgr)
            .into_iter()
            .find_map(|o| match o {
                Output::OpenUpstream { flow, .. } => Some(flow),
                _ => None,
            })
            .unwrap();
        drain(&mut mgr);

        // Single backend reply closes the flow.
        mgr.handle_input(
            ManagerInput::BackendDatagram {
                flow,
                payload: b"answer",
            },
            now,
        );
        let outs = drain(&mut mgr);
        assert!(
            outs.iter()
                .any(|o| matches!(o, Output::SendToClient(t) if t.payload == b"answer"))
        );
        assert!(
            outs.iter()
                .any(|o| matches!(o, Output::CloseFlow(f) if *f == flow))
        );
        assert_eq!(mgr.flow_count(), 0);
    }

    #[test]
    fn requests_cap_closes_flow() {
        let mut cfg = cluster("syslog");
        cfg.requests = 2;
        let mut mgr = UdpManager::new(cfg, 16, 65535, 7);
        let now = Instant::now();
        let src = client(1, 1000);
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src,
                payload: b"1",
                backends: &mut TestBackends::one(),
            },
            now,
        );
        let _flow = drain(&mut mgr)
            .into_iter()
            .find_map(|o| match o {
                Output::OpenUpstream { flow, .. } => Some(flow),
                _ => None,
            })
            .unwrap();
        drain(&mut mgr);
        // requests_seen is now 1 (the admission datagram). Second hits the cap.
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src,
                payload: b"2",
                backends: &mut TestBackends::one(),
            },
            now,
        );
        let outs = drain(&mut mgr);
        assert!(outs.iter().any(|o| matches!(o, Output::CloseFlow(_))));
        assert_eq!(mgr.flow_count(), 0);
    }

    #[test]
    fn flow_table_full_sheds_new_flow() {
        let mut mgr = UdpManager::new(cluster("dns"), 1, 65535, 7);
        let now = Instant::now();
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src: client(1, 1000),
                payload: b"a",
                backends: &mut TestBackends::one(),
            },
            now,
        );
        assert_eq!(mgr.flow_count(), 1);
        drain(&mut mgr);
        // Second distinct source over cap → shed.
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src: client(2, 1000),
                payload: b"b",
                backends: &mut TestBackends::one(),
            },
            now,
        );
        assert_eq!(mgr.flow_count(), 1);
        let outs = drain(&mut mgr);
        assert!(
            outs.iter()
                .any(|o| matches!(o, Output::Metric(MetricEvent::FlowShed)))
        );
        assert!(
            outs.iter()
                .any(|o| matches!(o, Output::Drop(DropReason::Shed)))
        );
    }

    #[test]
    fn idle_timeout_closes_flow() {
        let mut cfg = cluster("dns");
        cfg.front_timeout = Duration::from_secs(10);
        let mut mgr = UdpManager::new(cfg, 16, 65535, 7);
        let now = Instant::now();
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src: client(1, 1000),
                payload: b"q",
                backends: &mut TestBackends::one(),
            },
            now,
        );
        drain(&mut mgr);
        let deadline = mgr.poll_timeout().expect("timer armed");
        assert!(deadline >= now + Duration::from_secs(10));
        // Fire after the deadline.
        mgr.handle_timeout(now + Duration::from_secs(11));
        let outs = drain(&mut mgr);
        assert!(outs.iter().any(|o| matches!(o, Output::CloseFlow(_))));
        assert_eq!(mgr.flow_count(), 0);
        assert!(mgr.poll_timeout().is_none());
    }

    /// An expiry that finds NOTHING due must still re-arm.
    ///
    /// The shell's wheel (`crate::timer`) rounds a delay to the nearest tick, so
    /// with a 100 ms tick it delivers an entry up to 50 ms EARLY on the grid
    /// (`test_timeout_fires_up_to_half_a_tick_early`, `lib/src/timer.rs`) and
    /// up to 99 ms off it (see `duration_to_tick`). The
    /// shell then calls `handle_timeout` at a `now` that has not reached any
    /// flow's deadline: no flow is due, nothing closes, the minimum deadline is
    /// unchanged — yet the wheel entry has been CONSUMED. If `reschedule` keeps
    /// memoizing on "the deadline changed" it emits no `ArmTimer`, the shell
    /// never re-arms, and the flow is never reaped. `handle_timeout` is by
    /// contract only ever called from a wheel expiry, so on entry the shell has
    /// no armed timer and `armed_deadline` must say so.
    ///
    /// To SEE THIS RED: remove `self.armed_deadline = None;` from the top of
    /// [`UdpManager::handle_timeout`]. `reschedule` then finds `next ==
    /// self.armed_deadline`, emits nothing, and the ArmTimer assertion fails
    /// with `left: []  right: [Instant { .. }]`.
    #[test]
    fn early_expiry_that_finds_nothing_due_still_rearms() {
        let mut cfg = cluster("dns");
        cfg.front_timeout = Duration::from_secs(10);
        let mut mgr = UdpManager::new(cfg, 16, 65535, 7);
        let now = Instant::now();
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src: client(1, 1000),
                payload: b"q",
                backends: &mut TestBackends::one(),
            },
            now,
        );
        drain(&mut mgr);
        let deadline = mgr.poll_timeout().expect("timer armed on admission");

        // The wheel fires before the deadline. One second early here rather than
        // the wheel's real earliness — up to 50 ms on the tick grid, up to 99 ms
        // off it (see `duration_to_tick`) — so the test does not encode the
        // tick size.
        let early = deadline - Duration::from_secs(1);
        mgr.handle_timeout(early);
        let outs = drain(&mut mgr);

        assert_eq!(
            mgr.flow_count(),
            1,
            "nothing was due at the early expiry: the flow must survive"
        );
        assert!(
            !outs.iter().any(|o| matches!(
                o,
                Output::CloseFlow(_) | Output::Metric(MetricEvent::FlowEvicted)
            )),
            "nothing was due: no flow may be closed, got {outs:?}"
        );

        let armed: Vec<Instant> = outs
            .iter()
            .filter_map(|o| match o {
                Output::ArmTimer(d) => Some(*d),
                _ => None,
            })
            .collect();
        assert_eq!(
            armed,
            vec![deadline],
            "an expiry that found nothing due must re-arm the consumed wheel \
             entry with the (unchanged) deadline"
        );
        assert_eq!(
            mgr.poll_timeout(),
            Some(deadline),
            "the flow's deadline itself must not move"
        );
    }

    /// The re-arm is not a one-shot: a second early expiry must re-arm again.
    /// The shell holds exactly one wheel entry for the whole listener, so every
    /// consumed entry owes exactly one `ArmTimer` for as long as a flow is live.
    ///
    /// To SEE THIS RED: the same mutation as
    /// `early_expiry_that_finds_nothing_due_still_rearms` — remove
    /// `self.armed_deadline = None;` from the top of
    /// [`UdpManager::handle_timeout`]. Both expiries then emit nothing and this
    /// fails with `left: 0  right: 1` on the first `assert_eq!`.
    #[test]
    fn repeated_early_expiries_each_rearm() {
        let mut cfg = cluster("dns");
        cfg.front_timeout = Duration::from_secs(10);
        let mut mgr = UdpManager::new(cfg, 16, 65535, 7);
        let now = Instant::now();
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src: client(1, 1000),
                payload: b"q",
                backends: &mut TestBackends::one(),
            },
            now,
        );
        drain(&mut mgr);
        let deadline = mgr.poll_timeout().expect("timer armed on admission");

        let count_arms = |outs: &[Output]| {
            outs.iter()
                .filter(|o| matches!(o, Output::ArmTimer(_)))
                .count()
        };

        mgr.handle_timeout(deadline - Duration::from_secs(2));
        let first = drain(&mut mgr);
        assert_eq!(count_arms(&first), 1, "first early expiry must re-arm");

        mgr.handle_timeout(deadline - Duration::from_secs(1));
        let second = drain(&mut mgr);
        assert_eq!(
            count_arms(&second),
            1,
            "second early expiry must re-arm too"
        );

        // The flow survived both and is still reaped at its real deadline.
        assert_eq!(mgr.flow_count(), 1);
        mgr.handle_timeout(deadline);
        let outs = drain(&mut mgr);
        assert!(outs.iter().any(|o| matches!(o, Output::CloseFlow(_))));
        assert_eq!(mgr.flow_count(), 0);
        assert_eq!(
            count_arms(&outs),
            0,
            "the last flow is gone: nothing left to arm"
        );
        assert!(mgr.poll_timeout().is_none());
    }

    #[test]
    fn idle_race_resolved_by_generation_token() {
        // A datagram refreshes the deadline; the stale expiry must NOT close.
        let mut cfg = cluster("dns");
        cfg.front_timeout = Duration::from_secs(10);
        let mut mgr = UdpManager::new(cfg, 16, 65535, 7);
        let now = Instant::now();
        let src = client(1, 1000);
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src,
                payload: b"q",
                backends: &mut TestBackends::one(),
            },
            now,
        );
        let flow = drain(&mut mgr)
            .into_iter()
            .find_map(|o| match o {
                Output::OpenUpstream { flow, .. } => Some(flow),
                _ => None,
            })
            .unwrap();
        drain(&mut mgr);
        let gen0 = mgr.flow(flow).unwrap().timer_gen;
        // Datagram at t=5 refreshes deadline to t=15 and bumps generation.
        let t5 = now + Duration::from_secs(5);
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src,
                payload: b"q2",
                backends: &mut TestBackends::one(),
            },
            t5,
        );
        drain(&mut mgr);
        let gen1 = mgr.flow(flow).unwrap().timer_gen;
        assert_ne!(gen0, gen1, "generation token must bump on touch");
        // Stale expiry at the original t=10 deadline must NOT close (deadline is
        // now t=15).
        mgr.handle_timeout(now + Duration::from_secs(10));
        assert_eq!(mgr.flow_count(), 1, "refreshed flow survives stale expiry");
        // The real deadline at t=15 closes it.
        mgr.handle_timeout(now + Duration::from_secs(16));
        assert_eq!(mgr.flow_count(), 0);
    }

    #[test]
    fn drain_sheds_new_flows_but_keeps_existing() {
        let mut mgr = UdpManager::new(cluster("dns"), 16, 65535, 7);
        let now = Instant::now();
        let src = client(1, 1000);
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src,
                payload: b"q",
                backends: &mut TestBackends::one(),
            },
            now,
        );
        drain(&mut mgr);
        mgr.handle_input(ManagerInput::Config(ConfigEvent::Drain), now);
        // New flow shed.
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src: client(2, 1000),
                payload: b"q",
                backends: &mut TestBackends::one(),
            },
            now,
        );
        assert_eq!(mgr.flow_count(), 1, "existing flow kept, new flow shed");
        let outs = drain(&mut mgr);
        assert!(
            outs.iter()
                .any(|o| matches!(o, Output::Drop(DropReason::Shed)))
        );
    }

    #[test]
    fn reconfig_midflow_preserves_existing_flow_contract() {
        let mut cfg = cluster("dns");
        cfg.responses = 0;
        let mut mgr = UdpManager::new(cfg, 16, 65535, 7);
        let now = Instant::now();
        let src = client(1, 1000);
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src,
                payload: b"q",
                backends: &mut TestBackends::one(),
            },
            now,
        );
        let flow = drain(&mut mgr)
            .into_iter()
            .find_map(|o| match o {
                Output::OpenUpstream { flow, .. } => Some(flow),
                _ => None,
            })
            .unwrap();
        drain(&mut mgr);
        // Reconfigure to responses=1; the live flow keeps responses=0 (captured).
        let mut newcfg = cluster("dns");
        newcfg.responses = 1;
        mgr.handle_input(ManagerInput::Config(ConfigEvent::SetCluster(newcfg)), now);
        // A reply does NOT close the existing flow (it captured responses=0).
        mgr.handle_input(
            ManagerInput::BackendDatagram {
                flow,
                payload: b"reply",
            },
            now,
        );
        assert_eq!(mgr.flow_count(), 1);
    }

    #[test]
    fn reaper_drains_active_flows_to_zero() {
        let mut cfg = cluster("dns");
        cfg.front_timeout = Duration::from_secs(5);
        let mut mgr = UdpManager::new(cfg, 64, 65535, 7);
        let now = Instant::now();
        for n in 1..=10u8 {
            mgr.handle_input(
                ManagerInput::ClientDatagram {
                    src: client(n, 1000),
                    payload: b"q",
                    backends: &mut TestBackends::one(),
                },
                now,
            );
        }
        assert_eq!(mgr.flow_count(), 10);
        drain(&mut mgr);
        // Reaper after all idle deadlines pass.
        mgr.handle_timeout(now + Duration::from_secs(6));
        assert_eq!(mgr.flow_count(), 0, "reaper drains every flow, no leak");
        let outs = drain(&mut mgr);
        let closes = outs
            .iter()
            .filter(|o| matches!(o, Output::CloseFlow(_)))
            .count();
        assert_eq!(closes, 10);
        assert!(mgr.poll_timeout().is_none(), "no armed timer after drain");
    }

    #[test]
    fn proxy_protocol_first_datagram_only() {
        let mut cfg = cluster("dns");
        cfg.send_proxy_protocol = true;
        cfg.proxy_protocol_every_datagram = false;
        let mut mgr = UdpManager::new(cfg, 16, 65535, 7);
        let now = Instant::now();
        let src = client(1, 1000);
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src,
                payload: b"q1",
                backends: &mut TestBackends::one(),
            },
            now,
        );
        // One drain: the upstream request and the admission datagram are
        // emitted by the same call now, not across a resolve round-trip.
        let outs = drain(&mut mgr);
        let _flow = outs
            .iter()
            .find_map(|o| match o {
                Output::OpenUpstream { flow, .. } => Some(*flow),
                _ => None,
            })
            .expect("admission must ask the shell to open an upstream");
        // First datagram to the backend carries the PPv2 prefix.
        let first = outs
            .into_iter()
            .find_map(|o| match o {
                Output::SendToBackend(t) => Some(t.payload),
                _ => None,
            })
            .expect("the admission datagram must be forwarded");
        assert!(first.len() > 2, "PPv2 header prepended to first datagram");
        assert_eq!(&first[..4], &[0x0D, 0x0A, 0x0D, 0x0A]);
        assert_eq!(first[12], 0x21);
        assert_eq!(first[13], 0x12);
        assert_eq!(&first[first.len() - 2..], b"q1");

        // Second datagram: NO prefix.
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src,
                payload: b"q2",
                backends: &mut TestBackends::one(),
            },
            now,
        );
        let second = drain(&mut mgr)
            .into_iter()
            .find_map(|o| match o {
                Output::SendToBackend(t) => Some(t.payload),
                _ => None,
            })
            .unwrap();
        assert_eq!(second, b"q2", "no PPv2 prefix on subsequent datagrams");
    }

    /// Helper: admit a flow from `src`, resolve it to `backend()`, and return
    /// its FlowId. Drains the manager between steps. Leaves the flow
    /// `Established`.
    /// Admit a flow and return its id.
    ///
    /// One call now. It used to be two — admit, then reply to
    /// `Output::SelectBackend` with `ManagerInput::BackendResolved` — because
    /// the shell chose the backend. The core chooses it, so admission
    /// establishes in the same call and the flow id comes from the
    /// `OpenUpstream` the manager asks for.
    fn establish(mgr: &mut UdpManager, src: SocketAddr, now: Instant) -> FlowId {
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src,
                payload: b"q",
                backends: &mut TestBackends::one(),
            },
            now,
        );
        drain(mgr)
            .into_iter()
            .find_map(|o| match o {
                Output::OpenUpstream { flow, .. } => Some(flow),
                _ => None,
            })
            .expect("admission must ask the shell to open an upstream")
    }

    #[test]
    fn close_all_evicts_every_flow_exactly_once() {
        let mut cfg = cluster("dns");
        cfg.front_timeout = Duration::from_secs(30);
        let mut mgr = UdpManager::new(cfg, 64, 65535, 7);
        let now = Instant::now();
        // N flows: a mix of Established and AwaitingBackend.
        const N: usize = 6;
        let mut flow_ids = Vec::new();
        for n in 1..=4u8 {
            flow_ids.push(establish(&mut mgr, client(n, 1000), now));
        }
        // Two flows left AwaitingBackend (admitted, not resolved).
        for n in 5..=6u8 {
            mgr.handle_input(
                ManagerInput::ClientDatagram {
                    src: client(n, 1000),
                    payload: b"q",
                    backends: &mut TestBackends::one(),
                },
                now,
            );
            let flow = drain(&mut mgr)
                .into_iter()
                .find_map(|o| match o {
                    Output::OpenUpstream { flow, .. } => Some(flow),
                    _ => None,
                })
                .unwrap();
            flow_ids.push(flow);
        }
        assert_eq!(mgr.flow_count(), N);

        mgr.close_all(now);
        let outs = drain(&mut mgr);
        let evicted = outs
            .iter()
            .filter(|o| matches!(o, Output::Metric(MetricEvent::FlowEvicted)))
            .count();
        let closed = outs
            .iter()
            .filter(|o| matches!(o, Output::CloseFlow(_)))
            .count();
        assert_eq!(evicted, N, "one FlowEvicted per live flow");
        assert_eq!(closed, N, "one CloseFlow per live flow");
        assert_eq!(mgr.flow_count(), 0, "flow table + slab drained to zero");
        assert!(
            mgr.poll_timeout().is_none(),
            "no armed timer after close_all"
        );
    }

    #[test]
    fn close_all_is_idempotent_and_empty_safe() {
        let mut mgr = UdpManager::new(cluster("dns"), 16, 65535, 7);
        let now = Instant::now();
        // Empty manager: close_all is a no-op, no outputs.
        mgr.close_all(now);
        assert!(drain(&mut mgr).is_empty());
        assert_eq!(mgr.flow_count(), 0);

        // Now one flow; close it twice. The second pass emits nothing (no
        // double-evict / underflow).
        establish(&mut mgr, client(1, 1000), now);
        assert_eq!(mgr.flow_count(), 1);
        mgr.close_all(now);
        let first = drain(&mut mgr);
        assert_eq!(
            first
                .iter()
                .filter(|o| matches!(o, Output::CloseFlow(_)))
                .count(),
            1
        );
        mgr.close_all(now);
        assert!(drain(&mut mgr).is_empty(), "second close_all emits nothing");
        assert_eq!(mgr.flow_count(), 0);
    }

    #[test]
    fn abort_flow_closes_established_flow() {
        let mut mgr = UdpManager::new(cluster("dns"), 16, 65535, 7);
        let now = Instant::now();
        let flow = establish(&mut mgr, client(1, 1000), now);
        assert_eq!(mgr.flow_count(), 1);

        mgr.abort_flow(flow, now, CloseReason::Aborted);
        let outs = drain(&mut mgr);
        assert!(
            outs.iter()
                .any(|o| matches!(o, Output::Metric(MetricEvent::FlowEvicted)))
        );
        assert!(
            outs.iter()
                .any(|o| matches!(o, Output::CloseFlow(f) if *f == flow))
        );
        assert_eq!(mgr.flow_count(), 0);

        // Idempotent: aborting again emits nothing, no underflow.
        mgr.abort_flow(flow, now, CloseReason::Aborted);
        assert!(drain(&mut mgr).is_empty());
        assert_eq!(mgr.flow_count(), 0);
    }

    /// A flow that cannot get a backend must free its slot immediately.
    ///
    /// Retargeted from `abort_flow_closes_awaiting_backend_flow`. Its subject
    /// was the `AwaitingBackend` phase, which selection-in-core removed — but
    /// the *property* it pinned survives, because selection can still fail:
    /// `BackendMap::backend_from_cluster_id_with_key` has three
    /// `NoBackendForCluster` returns. The failure just happens inside the
    /// admitting call now instead of across two.
    ///
    /// What is asserted is unchanged in substance: nothing is left pinning a
    /// `max_flows` slot, and no timer is armed for a flow that never ran.
    #[test]
    fn a_flow_that_cannot_get_a_backend_frees_its_slot_immediately() {
        let mut mgr = UdpManager::new(cluster("dns"), 16, 65535, 7);
        let now = Instant::now();
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src: client(1, 1000),
                payload: b"q",
                // The cluster has nothing that can serve.
                backends: &mut TestBackends::none(),
            },
            now,
        );

        let outs = drain(&mut mgr);
        assert!(
            outs.iter()
                .any(|o| matches!(o, Output::Drop(DropReason::NoBackend))),
            "a failed selection must surface as a NoBackend drop: {outs:?}"
        );
        assert!(
            !outs
                .iter()
                .any(|o| matches!(o, Output::OpenUpstream { .. })),
            "no upstream may be opened for a flow that got no backend"
        );
        assert_eq!(
            mgr.flow_count(),
            0,
            "a failed selection must leave no flow pinning a max_flows slot"
        );
        assert!(
            mgr.poll_timeout().is_none(),
            "no timer may be armed for a flow that never ran"
        );

        // The key is reusable: a later datagram from the same source admits a
        // fresh flow rather than colliding with the refused one.
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src: client(1, 1000),
                payload: b"q2",
                backends: &mut TestBackends::one(),
            },
            now,
        );
        assert_eq!(mgr.flow_count(), 1);
    }

    #[test]
    fn abort_flow_unknown_id_is_noop() {
        let mut mgr = UdpManager::new(cluster("dns"), 16, 65535, 7);
        let now = Instant::now();
        mgr.abort_flow(999, now, CloseReason::Aborted);
        assert!(drain(&mut mgr).is_empty());
        assert_eq!(mgr.flow_count(), 0);
    }

    /// Every datagram of an opening burst reaches the backend, and each is
    /// counted exactly once.
    ///
    /// Retargeted from the test that pinned the await-window buffering.
    /// Its subject was the await window: datagrams arriving while the flow was
    /// `AwaitingBackend` went into a one-slot newest-wins buffer, all but the
    /// last were discarded, and only the survivor counted toward `requests`.
    /// Selection happens inside the admitting call now, so there is no window,
    /// no buffer, and nothing discarded — which is the **behaviour change**
    /// this changeset carries into the UDP datapath.
    ///
    /// Two properties, both live: the burst is delivered in full, and
    /// `requests` counts real forwards exactly once each so the cap still
    /// closes the flow on the right datagram.
    // ---- quickcheck property tests (zero sockets, injected Instants) -------
    use quickcheck::{Arbitrary, Gen, quickcheck};
    #[derive(Clone, Debug)]
    enum Step {
        /// Client datagram from source `id % 8`.
        Client(u8),
        /// Advance the injected clock by `secs` seconds and fire the reaper.
        Tick(u8),
    }

    impl Arbitrary for Step {
        fn arbitrary(g: &mut Gen) -> Self {
            if bool::arbitrary(g) {
                Step::Client(u8::arbitrary(g))
            } else {
                Step::Tick(u8::arbitrary(g) % 40)
            }
        }
    }

    /// Property: across any interleaving of datagrams and clock ticks, the flow
    /// count never exceeds `max_flows`, and a final long tick reaps every flow
    /// back to zero — no leak, no gauge underflow (`CloseFlow` count ==
    /// `FlowCreated` count). The busy-loop strict-advance invariant is asserted
    /// inside `handle_timeout` itself (debug builds), so every `Tick` exercises
    /// it for free.
    #[test]
    fn every_datagram_of_an_opening_burst_is_forwarded_and_counted_once() {
        // Part one: an unlimited flow delivers the whole burst. Before this
        // change only `b"3"` would have reached the backend.
        let mut mgr = UdpManager::new(cluster("syslog"), 16, 65535, 7);
        let now = Instant::now();
        let src = client(1, 1000);
        for p in [b"1".as_slice(), b"2", b"3"] {
            mgr.handle_input(
                ManagerInput::ClientDatagram {
                    src,
                    payload: p,
                    backends: &mut TestBackends::one(),
                },
                now,
            );
        }
        let sent: Vec<Vec<u8>> = drain(&mut mgr)
            .into_iter()
            .filter_map(|o| match o {
                Output::SendToBackend(t) => Some(t.payload),
                _ => None,
            })
            .collect();
        assert_eq!(
            sent,
            vec![b"1".to_vec(), b"2".to_vec(), b"3".to_vec()],
            "every datagram of the opening burst must reach the backend, in order"
        );

        // Part two: `requests` counts each real forward once, so a cap of two
        // closes the flow on the second datagram and not before.
        let mut cfg = cluster("syslog");
        cfg.requests = 2;
        let mut mgr = UdpManager::new(cfg, 16, 65535, 7);
        let src = client(2, 1000);
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src,
                payload: b"a",
                backends: &mut TestBackends::one(),
            },
            now,
        );
        let flow = drain(&mut mgr)
            .into_iter()
            .find_map(|o| match o {
                Output::OpenUpstream { flow, .. } => Some(flow),
                _ => None,
            })
            .expect("admission must ask the shell to open an upstream");
        assert_eq!(
            mgr.flow(flow).unwrap().requests_seen,
            1,
            "the admission datagram is a forward and is counted exactly once"
        );
        assert_eq!(mgr.flow_count(), 1, "one forward must not reach requests=2");

        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src,
                payload: b"b",
                backends: &mut TestBackends::one(),
            },
            now,
        );
        let outs = drain(&mut mgr);
        assert!(
            outs.iter()
                .any(|o| matches!(o, Output::SendToBackend(t) if t.payload == b"b")),
            "the second datagram must still be forwarded before the cap closes the flow"
        );
        assert!(
            outs.iter().any(|o| matches!(o, Output::CloseFlow(_))),
            "the second real forward reaches requests=2 and closes the flow"
        );
        assert_eq!(mgr.flow_count(), 0);
    }

    #[test]
    fn prop_flow_invariants() {
        fn prop(steps: Vec<Step>) -> bool {
            const MAX_FLOWS: usize = 4;
            let mut cfg = cluster("dns");
            cfg.front_timeout = Duration::from_secs(10);
            let mut mgr = UdpManager::new(cfg, MAX_FLOWS, 65535, 0x5EED);
            let base = Instant::now();
            let mut now = base;
            let mut created = 0usize;
            let mut closed = 0usize;

            for step in steps {
                match step {
                    Step::Client(id) => {
                        let src = client(id % 8, 9000 + (id % 8) as u16);
                        mgr.handle_input(
                            ManagerInput::ClientDatagram {
                                src,
                                payload: b"q",
                                backends: &mut TestBackends::one(),
                            },
                            now,
                        );
                    }
                    Step::Tick(secs) => {
                        now += Duration::from_secs(secs as u64);
                        mgr.handle_timeout(now);
                    }
                }
                // Drain outputs, tallying create/close.
                while let Some(out) = mgr.poll_output() {
                    match out {
                        Output::Metric(MetricEvent::FlowCreated) => created += 1,
                        Output::CloseFlow(_) => closed += 1,
                        _ => {}
                    }
                }
                // The cap is never exceeded.
                if mgr.flow_count() > MAX_FLOWS {
                    return false;
                }
            }

            // Final long tick must reap everything: drains to zero, no timer.
            now += Duration::from_secs(60);
            mgr.handle_timeout(now);
            while let Some(out) = mgr.poll_output() {
                if let Output::CloseFlow(_) = out {
                    closed += 1;
                }
            }
            mgr.flow_count() == 0 && mgr.poll_timeout().is_none() && created == closed
        }
        quickcheck(prop as fn(Vec<Step>) -> bool);
    }

    /// Property: an idle-timeout race is always resolved by generation tokens —
    /// for any refresh strictly inside the timeout window, a stale expiry at the
    /// original deadline never closes a refreshed flow, and the refreshed
    /// deadline eventually does.
    #[test]
    fn prop_generation_token_defeats_stale_close() {
        fn prop(refresh_offset: u8) -> bool {
            let timeout = 20u64;
            // Refresh at 1..=timeout-1 seconds (strictly inside the window).
            let offset = 1 + (refresh_offset as u64 % (timeout - 1));
            let mut cfg = cluster("dns");
            cfg.front_timeout = Duration::from_secs(timeout);
            let mut mgr = UdpManager::new(cfg, 8, 65535, 1);
            let now = Instant::now();
            let src = client(1, 1000);
            mgr.handle_input(
                ManagerInput::ClientDatagram {
                    src,
                    payload: b"q",
                    backends: &mut TestBackends::one(),
                },
                now,
            );
            while mgr.poll_output().is_some() {}

            // Refresh inside the window: bumps the generation, pushes the
            // deadline forward.
            let refreshed_at = now + Duration::from_secs(offset);
            mgr.handle_input(
                ManagerInput::ClientDatagram {
                    src,
                    payload: b"q2",
                    backends: &mut TestBackends::one(),
                },
                refreshed_at,
            );
            while mgr.poll_output().is_some() {}

            // Stale expiry at the ORIGINAL deadline must not close the flow.
            mgr.handle_timeout(now + Duration::from_secs(timeout));
            while mgr.poll_output().is_some() {}
            if mgr.flow_count() != 1 {
                return false;
            }
            // The refreshed deadline eventually closes it.
            mgr.handle_timeout(refreshed_at + Duration::from_secs(timeout + 1));
            while mgr.poll_output().is_some() {}
            mgr.flow_count() == 0
        }
        quickcheck(prop as fn(u8) -> bool);
    }
}
