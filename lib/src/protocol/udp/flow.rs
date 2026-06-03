//! Per-flow UDP state machine (sans-io).
//!
//! A [`UdpFlow`] is the per-admitted-flow half of the two-level split. It owns
//! the three-knob teardown counters (`responses` / `requests` / idle), the idle
//! and lifetime deadlines, PPv2-first-datagram bookkeeping, the real (pre-NAT)
//! client address, the chosen backend, and the forward/return decisions. It
//! carries a `timer_gen` generation token so a stale wheel expiry cannot close
//! a flow that has since seen traffic.
//!
//! No socket, no clock, no rand: every time-dependent method takes `now:
//! Instant`. The manager owns the slab; this type is the slot payload.

use std::{net::SocketAddr, time::Instant};

use crate::protocol::udp::ClusterConfig;

/// Why a flow reached teardown. Surfaces in the access log on close.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    /// Idle timeout elapsed (no datagram in either direction).
    Idle,
    /// The configured `responses` count was reached (e.g. DNS = 1 reply).
    ResponsesReached,
    /// The configured `requests` count was reached.
    RequestsReached,
    /// The listener is draining.
    Drain,
    /// The flow was aborted by the shell before it could serve traffic — the
    /// upstream `connect()` failed (EMFILE / refused) or no backend resolved.
    /// Distinct from `Idle` so the access log shows the flow never established,
    /// rather than implying it timed out.
    Aborted,
}

/// The lifecycle of a flow w.r.t. its backend. A flow is admitted in
/// [`AwaitingBackend`](FlowPhase::AwaitingBackend), transitions to
/// [`Established`](FlowPhase::Established) once the shell resolves and opens an
/// upstream, and is reaped on teardown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlowPhase {
    /// `SelectBackend` emitted; awaiting `BackendResolved`. Client datagrams
    /// received in this window are buffered (one slot) so the first datagram is
    /// not lost between admission and upstream open.
    AwaitingBackend,
    /// Backend resolved and upstream opened; datagrams flow both ways.
    Established,
    /// Marked for teardown; the manager will emit `CloseFlow` and free the slot.
    Closing,
}

/// Per-admitted-flow state. Slab slot payload owned by the manager.
#[derive(Clone, Debug)]
pub struct UdpFlow {
    /// Real (pre-NAT) client source address — the symmetric NAT return target
    /// and the PPv2 source address.
    pub client: SocketAddr,
    /// Resolved backend id, set on `BackendResolved`.
    pub backend_id: Option<String>,
    /// Resolved backend address, set on `BackendResolved`.
    pub backend_addr: Option<SocketAddr>,
    /// Lifecycle phase.
    pub phase: FlowPhase,
    /// Captured per-cluster knobs (responses/requests/timeouts/PPv2). Captured
    /// at admission so a mid-flow reconfig does not change a live flow's
    /// teardown contract (stable affinity).
    pub config: ClusterConfig,

    /// Client datagrams forwarded so far (counts toward `requests`).
    pub requests_seen: u32,
    /// Backend replies returned so far (counts toward `responses`).
    pub responses_seen: u32,

    /// Absolute idle deadline; reset on every datagram in either direction.
    pub idle_deadline: Instant,
    /// Generation token. Incremented every time the idle deadline is pushed
    /// back. A wheel expiry only closes the flow when its captured generation
    /// still matches — defeating the stale-close busy-loop bug.
    pub timer_gen: u64,

    /// True until the first upstream datagram is sent; gates PPv2 prefixing
    /// when `proxy_protocol_every_datagram` is false (first-datagram-only).
    pub first_upstream_pending: bool,

    /// One-slot buffer for a client datagram that arrived while
    /// [`AwaitingBackend`](FlowPhase::AwaitingBackend). Flushed on
    /// `BackendResolved`. A second datagram in the window replaces it (newest
    /// wins) rather than allocating an unbounded queue.
    pub pending_payload: Option<Vec<u8>>,
}

impl UdpFlow {
    /// Create a flow for `client`, awaiting a backend, with its idle deadline
    /// armed `front_timeout` from `now`.
    pub fn new(client: SocketAddr, config: ClusterConfig, now: Instant) -> Self {
        let idle_deadline = now + config.front_timeout;
        UdpFlow {
            client,
            backend_id: None,
            backend_addr: None,
            phase: FlowPhase::AwaitingBackend,
            first_upstream_pending: config.send_proxy_protocol,
            config,
            requests_seen: 0,
            responses_seen: 0,
            idle_deadline,
            timer_gen: 0,
            pending_payload: None,
        }
    }

    /// Push the idle deadline back to `now + timeout` and bump the generation
    /// token so any in-flight wheel expiry for the old deadline is invalidated.
    /// Returns the *new* generation so the manager can re-arm the wheel.
    pub fn touch(&mut self, timeout: std::time::Duration, now: Instant) -> u64 {
        self.idle_deadline = now + timeout;
        self.timer_gen = self.timer_gen.wrapping_add(1);
        self.timer_gen
    }

    /// Record that one client datagram was actually *forwarded* upstream: bump
    /// the `requests` counter and refresh the front idle deadline. Returns the
    /// new generation token. Call this only at a real forward site — a datagram
    /// merely buffered while [`AwaitingBackend`](FlowPhase::AwaitingBackend) and
    /// later overwritten (newest-wins) must NOT count, or a burst during await
    /// could trip the `requests` cap having delivered fewer than `requests`
    /// datagrams. Use [`touch`](Self::touch) for the buffer-only idle refresh.
    pub fn on_client_datagram(&mut self, now: Instant) -> u64 {
        self.requests_seen = self.requests_seen.saturating_add(1);
        self.touch(self.config.front_timeout, now)
    }

    /// Record that one backend reply was returned; refresh the back idle
    /// deadline. Returns the new generation token.
    pub fn on_backend_datagram(&mut self, now: Instant) -> u64 {
        self.responses_seen = self.responses_seen.saturating_add(1);
        self.touch(self.config.back_timeout, now)
    }

    /// Whether the `requests` knob has been exhausted (`0` = unlimited).
    pub fn requests_exhausted(&self) -> bool {
        self.config.requests != 0 && self.requests_seen >= self.config.requests
    }

    /// Whether the `responses` knob has been exhausted (`0` = unlimited). A DNS
    /// flow with `responses = 1` closes after its single reply.
    pub fn responses_exhausted(&self) -> bool {
        self.config.responses != 0 && self.responses_seen >= self.config.responses
    }

    /// The teardown reason if any knob is exhausted, else `None`. Idle is
    /// handled separately by the manager via the timer wheel.
    pub fn teardown_reason(&self) -> Option<CloseReason> {
        if self.responses_exhausted() {
            Some(CloseReason::ResponsesReached)
        } else if self.requests_exhausted() {
            Some(CloseReason::RequestsReached)
        } else {
            None
        }
    }

    /// Whether this upstream datagram should carry a PPv2 DGRAM prefix, given
    /// the cluster's first-datagram-only vs every-datagram policy. Marks the
    /// first-datagram bookkeeping as consumed.
    pub fn take_proxy_protocol(&mut self) -> bool {
        if !self.config.send_proxy_protocol {
            return false;
        }
        if self.config.proxy_protocol_every_datagram {
            return true;
        }
        // First-datagram-only: prefix exactly once.
        if self.first_upstream_pending {
            self.first_upstream_pending = false;
            true
        } else {
            false
        }
    }
}
