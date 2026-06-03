//! The sans-io [`UdpManager`]: admission, flow table, LB request, timers.
//!
//! `UdpManager` owns the flow table (`HashMap<FlowKey, FlowId>` over a
//! `slab::Slab<UdpFlow>`), admission ("allocate nothing for unknown / over-cap
//! / invalid datagrams"), the flow-table cap and shedding, pluggable flow-key
//! extraction ([`FlowKeyExtractor`]), the LB-selection *request* for new flows
//! (it emits [`Output::SelectBackend`]; the shell does the actual LB), and the
//! timer scheduling: a **single armed manager-wide deadline** plus per-flow
//! **generation tokens** so a stale expiry can never close a refreshed flow.
//!
//! Pure: every entry point that depends on time takes `now: Instant`; the hash
//! seed is injected at construction. The shell drives the manager with
//! [`ManagerInput`] and drains [`Output`] via [`poll_output`].

use std::{
    collections::{HashMap, VecDeque},
    hash::{Hash, Hasher},
    net::SocketAddr,
    time::Instant,
};

use slab::Slab;

use crate::protocol::udp::{
    ClusterConfig, ConfigEvent, DropReason, FlowId, FlowKey, ManagerInput, MetricEvent, Output,
    Transmit,
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
    /// via the last `ArmTimer`. `None` means no timer is armed.
    armed_deadline: Option<Instant>,
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
            ManagerInput::ClientDatagram { src, payload } => {
                self.on_client_datagram(src, payload, now)
            }
            ManagerInput::BackendDatagram { flow, payload } => {
                self.on_backend_datagram(flow, payload, now)
            }
            ManagerInput::Config(event) => self.on_config(event, now),
            ManagerInput::BackendResolved {
                flow,
                backend,
                addr,
            } => self.on_backend_resolved(flow, backend, addr, now),
        }
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
    }

    fn on_client_datagram(&mut self, src: SocketAddr, payload: &[u8], now: Instant) {
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
        if self.draining {
            self.drop_datagram(DropReason::Shed);
            return;
        }
        if self.flows.len() >= self.max_flows {
            self.outputs
                .push_back(Output::Metric(MetricEvent::FlowShed));
            self.drop_datagram(DropReason::Shed);
            return;
        }

        // Admit: one slab slot + one copy of the payload (the design's single
        // admission copy). The flow is parked AwaitingBackend with the datagram
        // buffered until the shell resolves a backend. `requests_seen` stays 0:
        // the datagram is only buffered here, and is counted as a forward when
        // it is actually flushed in `on_backend_resolved` — so `requests`
        // measures real forwards, not buffered-and-maybe-discarded datagrams.
        let mut flow = UdpFlow::new(src, self.cluster.clone(), now);
        flow.pending_payload = Some(payload.to_vec());
        let key_hash = self.affinity_hash(&flow);
        let flow_id = self.flows.insert(flow);
        self.table.insert(key, flow_id);

        self.outputs
            .push_back(Output::Metric(MetricEvent::FlowCreated));
        self.outputs.push_back(Output::SelectBackend {
            flow: flow_id,
            cluster: self.cluster.cluster.clone(),
            key: key_hash,
        });
        // Arm the idle timer for the freshly-admitted flow.
        self.reschedule();
    }

    fn forward_on_existing_flow(&mut self, flow_id: FlowId, payload: &[u8], now: Instant) {
        let Some(flow) = self.flows.get_mut(flow_id) else {
            // Table/slab desync should be impossible, but never panic on it.
            self.drop_datagram(DropReason::UnknownFlow);
            return;
        };

        match flow.phase {
            FlowPhase::AwaitingBackend => {
                // Backend not yet resolved: buffer (newest-wins, one slot) and
                // refresh idle ONLY. Do not count this toward `requests` — the
                // previously-buffered datagram is now discarded and was never
                // forwarded. The single surviving buffered datagram is counted
                // when it is actually flushed in `on_backend_resolved`.
                flow.pending_payload = Some(payload.to_vec());
                flow.touch(flow.config.front_timeout, now);
                self.reschedule();
            }
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

    fn on_backend_resolved(
        &mut self,
        flow_id: FlowId,
        backend: String,
        addr: SocketAddr,
        now: Instant,
    ) {
        let Some(flow) = self.flows.get_mut(flow_id) else {
            // Flow was reaped (idle/drain) before the shell resolved a backend.
            self.drop_datagram(DropReason::UnknownFlow);
            return;
        };
        if flow.phase != FlowPhase::AwaitingBackend {
            // Duplicate / late resolution; ignore without allocating.
            return;
        }
        flow.backend_id = Some(backend);
        flow.backend_addr = Some(addr);
        flow.phase = FlowPhase::Established;

        // Open the connected upstream socket (the shell registers
        // upstream_token -> flow for NAT return).
        self.outputs.push_back(Output::OpenUpstream {
            flow: flow_id,
            backend: addr,
        });

        // Flush the buffered first datagram, if any. This is the real forward
        // site for the admission datagram, so count it toward `requests` here
        // (refreshing the front idle deadline at the same time) — not at
        // admission, where it was only buffered.
        let pending = flow.pending_payload.take();
        if let Some(mut payload) = pending {
            let payload_len = payload.len();
            flow.on_client_datagram(now);
            if flow.take_proxy_protocol() {
                prepend_dgram_header(&mut payload, flow.client, addr);
            }
            let teardown = flow.teardown_reason();
            self.outputs
                .push_back(Output::Metric(MetricEvent::DatagramIn(payload_len)));
            self.outputs.push_back(Output::SendToBackend(Transmit {
                dst: addr,
                segment_size: None,
                payload,
            }));
            if let Some(reason) = teardown {
                self.close_flow(flow_id, reason);
                return;
            }
            // `on_client_datagram` already refreshed the deadline + generation.
            self.reschedule();
            return;
        }
        // No buffered datagram: refresh idle and re-arm (phase changed; the
        // deadline was set at `new()`).
        let _gen = self
            .flows
            .get_mut(flow_id)
            .map(|f| f.touch(self.cluster.front_timeout, now));
        self.reschedule();
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
            ConfigEvent::SetMaxFlows(n) => self.max_flows = n,
            ConfigEvent::SetMaxRxDatagramSize(n) => self.max_rx_datagram_size = n,
            ConfigEvent::Drain => self.draining = true,
        }
    }

    // ---- timers ------------------------------------------------------------

    /// Fire all flows whose idle deadline has elapsed at `now`. A flow is only
    /// closed if its generation token still matches the scheduled deadline —
    /// generation mismatch means the flow saw traffic and was rescheduled, so
    /// the stale expiry is ignored (defeats the busy-loop / stale-close bug).
    pub fn handle_timeout(&mut self, now: Instant) {
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
            if let Some(flow) = self.flows.get(flow_id) {
                if flow.idle_deadline <= now && flow.phase != FlowPhase::Closing {
                    self.close_flow(flow_id, CloseReason::Idle);
                }
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
    }

    /// The next manager-wide deadline, or `None` if no flow is armed. After a
    /// [`handle_timeout`] at deadline `d`, the value returned here is guaranteed
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
    /// changes, so the shell re-arms its wheel exactly once per real change.
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
        flow.phase = FlowPhase::Closing;
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
    }

    /// Emit a drop with its by-reason metric. Allocates nothing per the
    /// "silence is a virtue" posture.
    fn drop_datagram(&mut self, reason: DropReason) {
        self.outputs
            .push_back(Output::Metric(MetricEvent::DatagramDropped(reason)));
        self.outputs.push_back(Output::Drop(reason));
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
}

#[cfg(test)]
mod tests {
    use std::{
        net::{IpAddr, Ipv4Addr},
        time::Duration,
    };

    use super::*;

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

    fn backend() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 5300)
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
                .any(|o| matches!(o, Output::SelectBackend { .. }))
        );
    }

    #[test]
    fn empty_datagram_is_invalid() {
        let mut mgr = UdpManager::new(cluster("dns"), 16, 65535, 1);
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src: client(1, 1000),
                payload: b"",
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

    #[test]
    fn new_flow_requests_backend_then_forwards_buffered_datagram() {
        let mut mgr = UdpManager::new(cluster("dns"), 16, 65535, 7);
        let now = Instant::now();
        let src = client(1, 1000);
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src,
                payload: b"query",
            },
            now,
        );
        assert_eq!(mgr.flow_count(), 1);
        let outs = drain(&mut mgr);
        let select = outs
            .iter()
            .find_map(|o| match o {
                Output::SelectBackend { flow, cluster, .. } => Some((*flow, cluster.clone())),
                _ => None,
            })
            .expect("SelectBackend emitted");
        assert_eq!(select.1, "dns");
        // No SendToBackend yet — backend not resolved.
        assert!(!outs.iter().any(|o| matches!(o, Output::SendToBackend(_))));

        mgr.handle_input(
            ManagerInput::BackendResolved {
                flow: select.0,
                backend: "b1".to_owned(),
                addr: backend(),
            },
            now,
        );
        let outs = drain(&mut mgr);
        assert!(
            outs.iter()
                .any(|o| matches!(o, Output::OpenUpstream { .. }))
        );
        let sent = outs
            .iter()
            .find_map(|o| match o {
                Output::SendToBackend(t) => Some(t.clone()),
                _ => None,
            })
            .expect("buffered datagram flushed on resolve");
        assert_eq!(sent.dst, backend());
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
            },
            now,
        );
        let select_flow = drain(&mut mgr)
            .into_iter()
            .find_map(|o| match o {
                Output::SelectBackend { flow, .. } => Some(flow),
                _ => None,
            })
            .unwrap();
        mgr.handle_input(
            ManagerInput::BackendResolved {
                flow: select_flow,
                backend: "b1".to_owned(),
                addr: backend(),
            },
            now,
        );
        drain(&mut mgr);

        // Second datagram from same source: no new SelectBackend, direct send.
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src,
                payload: b"q2",
            },
            now,
        );
        assert_eq!(mgr.flow_count(), 1);
        let outs = drain(&mut mgr);
        assert!(
            !outs
                .iter()
                .any(|o| matches!(o, Output::SelectBackend { .. }))
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
        mgr.handle_input(ManagerInput::ClientDatagram { src, payload: b"q" }, now);
        let flow = drain(&mut mgr)
            .into_iter()
            .find_map(|o| match o {
                Output::SelectBackend { flow, .. } => Some(flow),
                _ => None,
            })
            .unwrap();
        mgr.handle_input(
            ManagerInput::BackendResolved {
                flow,
                backend: "b1".to_owned(),
                addr: backend(),
            },
            now,
        );
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
        mgr.handle_input(ManagerInput::ClientDatagram { src, payload: b"1" }, now);
        let flow = drain(&mut mgr)
            .into_iter()
            .find_map(|o| match o {
                Output::SelectBackend { flow, .. } => Some(flow),
                _ => None,
            })
            .unwrap();
        mgr.handle_input(
            ManagerInput::BackendResolved {
                flow,
                backend: "b1".to_owned(),
                addr: backend(),
            },
            now,
        );
        drain(&mut mgr);
        // requests_seen is now 1 (the admission datagram). Second hits the cap.
        mgr.handle_input(ManagerInput::ClientDatagram { src, payload: b"2" }, now);
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

    #[test]
    fn idle_race_resolved_by_generation_token() {
        // A datagram refreshes the deadline; the stale expiry must NOT close.
        let mut cfg = cluster("dns");
        cfg.front_timeout = Duration::from_secs(10);
        let mut mgr = UdpManager::new(cfg, 16, 65535, 7);
        let now = Instant::now();
        let src = client(1, 1000);
        mgr.handle_input(ManagerInput::ClientDatagram { src, payload: b"q" }, now);
        let flow = drain(&mut mgr)
            .into_iter()
            .find_map(|o| match o {
                Output::SelectBackend { flow, .. } => Some(flow),
                _ => None,
            })
            .unwrap();
        mgr.handle_input(
            ManagerInput::BackendResolved {
                flow,
                backend: "b1".to_owned(),
                addr: backend(),
            },
            now,
        );
        drain(&mut mgr);
        let gen0 = mgr.flow(flow).unwrap().timer_gen;
        // Datagram at t=5 refreshes deadline to t=15 and bumps generation.
        let t5 = now + Duration::from_secs(5);
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src,
                payload: b"q2",
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
        mgr.handle_input(ManagerInput::ClientDatagram { src, payload: b"q" }, now);
        drain(&mut mgr);
        mgr.handle_input(ManagerInput::Config(ConfigEvent::Drain), now);
        // New flow shed.
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src: client(2, 1000),
                payload: b"q",
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
        mgr.handle_input(ManagerInput::ClientDatagram { src, payload: b"q" }, now);
        let flow = drain(&mut mgr)
            .into_iter()
            .find_map(|o| match o {
                Output::SelectBackend { flow, .. } => Some(flow),
                _ => None,
            })
            .unwrap();
        mgr.handle_input(
            ManagerInput::BackendResolved {
                flow,
                backend: "b1".to_owned(),
                addr: backend(),
            },
            now,
        );
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
            },
            now,
        );
        let flow = drain(&mut mgr)
            .into_iter()
            .find_map(|o| match o {
                Output::SelectBackend { flow, .. } => Some(flow),
                _ => None,
            })
            .unwrap();
        mgr.handle_input(
            ManagerInput::BackendResolved {
                flow,
                backend: "b1".to_owned(),
                addr: backend(),
            },
            now,
        );
        // First flushed datagram carries the PPv2 prefix.
        let first = drain(&mut mgr)
            .into_iter()
            .find_map(|o| match o {
                Output::SendToBackend(t) => Some(t.payload),
                _ => None,
            })
            .unwrap();
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
    fn establish(mgr: &mut UdpManager, src: SocketAddr, now: Instant) -> FlowId {
        mgr.handle_input(ManagerInput::ClientDatagram { src, payload: b"q" }, now);
        let flow = drain(mgr)
            .into_iter()
            .find_map(|o| match o {
                Output::SelectBackend { flow, .. } => Some(flow),
                _ => None,
            })
            .unwrap();
        mgr.handle_input(
            ManagerInput::BackendResolved {
                flow,
                backend: "b1".to_owned(),
                addr: backend(),
            },
            now,
        );
        drain(mgr);
        flow
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
                },
                now,
            );
            let flow = drain(&mut mgr)
                .into_iter()
                .find_map(|o| match o {
                    Output::SelectBackend { flow, .. } => Some(flow),
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

    #[test]
    fn abort_flow_closes_awaiting_backend_flow() {
        // Simulates udp_connect failing / no backend resolving: the flow never
        // leaves AwaitingBackend and must still free its slot immediately.
        let mut mgr = UdpManager::new(cluster("dns"), 16, 65535, 7);
        let now = Instant::now();
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src: client(1, 1000),
                payload: b"q",
            },
            now,
        );
        let flow = drain(&mut mgr)
            .into_iter()
            .find_map(|o| match o {
                Output::SelectBackend { flow, .. } => Some(flow),
                _ => None,
            })
            .unwrap();
        assert_eq!(mgr.flow_count(), 1);
        assert_eq!(mgr.flow(flow).unwrap().phase, FlowPhase::AwaitingBackend);

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
        assert_eq!(mgr.flow_count(), 0, "AwaitingBackend slot freed by abort");
        assert!(mgr.poll_timeout().is_none());
        // The freed key is reusable: a new datagram from the same source admits
        // a fresh flow rather than colliding with the aborted one.
        mgr.handle_input(
            ManagerInput::ClientDatagram {
                src: client(1, 1000),
                payload: b"q2",
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

    #[test]
    fn requests_counts_forwards_not_buffered_during_await() {
        // requests = 2. A burst of client datagrams arrives while the flow is
        // still AwaitingBackend; all but the newest are discarded (newest-wins)
        // and never forwarded — so they must NOT count toward `requests`. After
        // resolve, the single buffered datagram is forwarded (count = 1). Only a
        // SECOND real forward (post-establish) should reach the cap.
        let mut cfg = cluster("syslog");
        cfg.requests = 2;
        let mut mgr = UdpManager::new(cfg, 16, 65535, 7);
        let now = Instant::now();
        let src = client(1, 1000);

        // Admission datagram → AwaitingBackend, buffered (not counted yet).
        mgr.handle_input(ManagerInput::ClientDatagram { src, payload: b"1" }, now);
        let flow = drain(&mut mgr)
            .into_iter()
            .find_map(|o| match o {
                Output::SelectBackend { flow, .. } => Some(flow),
                _ => None,
            })
            .unwrap();
        // Burst during await: 3 more datagrams, all overwriting the one slot.
        for p in [b"2".as_slice(), b"3", b"4"] {
            mgr.handle_input(ManagerInput::ClientDatagram { src, payload: p }, now);
        }
        drain(&mut mgr);
        // Still alive, still awaiting — the burst did NOT trip the cap.
        assert_eq!(mgr.flow_count(), 1, "await burst must not close the flow");
        assert_eq!(
            mgr.flow(flow).unwrap().requests_seen,
            0,
            "buffered-only datagrams must not count toward requests"
        );

        // Resolve: the single surviving buffered datagram is forwarded (count 1).
        mgr.handle_input(
            ManagerInput::BackendResolved {
                flow,
                backend: "b1".to_owned(),
                addr: backend(),
            },
            now,
        );
        let outs = drain(&mut mgr);
        assert!(
            outs.iter()
                .any(|o| matches!(o, Output::SendToBackend(t) if t.payload == b"4")),
            "newest buffered datagram is the one forwarded"
        );
        assert!(
            !outs.iter().any(|o| matches!(o, Output::CloseFlow(_))),
            "one forward must not reach requests=2"
        );
        assert_eq!(mgr.flow_count(), 1);
        assert_eq!(mgr.flow(flow).unwrap().requests_seen, 1);

        // A real second forward (Established) reaches the cap and closes.
        mgr.handle_input(ManagerInput::ClientDatagram { src, payload: b"5" }, now);
        let outs = drain(&mut mgr);
        assert!(
            outs.iter()
                .any(|o| matches!(o, Output::SendToBackend(t) if t.payload == b"5"))
        );
        assert!(
            outs.iter().any(|o| matches!(o, Output::CloseFlow(_))),
            "second real forward reaches requests=2"
        );
        assert_eq!(mgr.flow_count(), 0);
    }

    // ---- quickcheck property tests (zero sockets, injected Instants) -------

    use quickcheck::{Arbitrary, Gen, quickcheck};

    /// One step of an abstract client-side workload: a datagram from one of a
    /// bounded set of sources, or a clock advance that fires the reaper.
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
                        mgr.handle_input(ManagerInput::ClientDatagram { src, payload: b"q" }, now);
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
            mgr.handle_input(ManagerInput::ClientDatagram { src, payload: b"q" }, now);
            while mgr.poll_output().is_some() {}

            // Refresh inside the window: bumps the generation, pushes the
            // deadline forward.
            let refreshed_at = now + Duration::from_secs(offset);
            mgr.handle_input(
                ManagerInput::ClientDatagram {
                    src,
                    payload: b"q2",
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
