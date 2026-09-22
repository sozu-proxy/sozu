//! Proxy-emitted RST_STREAM transmit queue for [`super::h2::ConnectionH2`].
//!
//! Groups the queued-RST state behind a narrow, closed API — the shape the
//! `hpack_state` / `h2_flow_control` / `h2_stream_table` / `h2_drain` /
//! `h2_flood_detector` / `h2_header_reassembly` / `h2_scheduler` steps
//! established. Every field here is private to this module; `ConnectionH2`
//! reaches them only through the accessor methods declared below.
//!
//! **This module owns**: the pending `(StreamId, H2Error)` queue, the
//! never-decaying `total_rst_streams_queued` lifetime counter behind the
//! CVE-2025-8671 MadeYouReset cap, that cap's value
//! ([`MAX_PENDING_RST_STREAMS`]) and the test for whether it has been
//! reached, the per-insert bound that refuses to grow the queue past that cap
//! (sozu-proxy/sozu#1413), and the serialization of queued frames into a
//! caller-supplied buffer.
//!
//! **It deliberately does NOT own, and why**:
//!
//! - **The dedupe set.** At most one queued RST per wire stream id, tracked
//!   by `rst_sent`, which lives on [`super::h2_stream_table::H2StreamTable`]
//!   because the wire map's own removal path asserts it clean. Passing it in
//!   per call keeps one owner rather than two views that can disagree.
//! - **`Readiness`.** Queueing a frame must arm `Ready::WRITABLE` (LIFECYCLE
//!   invariant 15) or the frame sits until an unrelated event re-triggers
//!   `writable()`. The connection owns readiness, so it is a parameter.
//! - **The decision to drain.** Three conditions gate the drain — nothing
//!   queued, a partially-written zero buffer (`expect_write`), or a header
//!   block mid-reassembly — and all three read `ConnectionH2` state this
//!   module has no business holding. The caller decides *whether*; this
//!   module only decides *what bytes*.
//! - **Metrics, logs and the flood detector.** Accounting happens once, at
//!   queue time, in `ConnectionH2::account_emitted_rst`, because a lifetime-cap
//!   trip converts to a connection-wide GOAWAY that only the connection can
//!   return. [`H2ControlTx::enqueue_rst`] therefore reports an
//!   [`EnqueueRstOutcome`] and leaves the caller to account exactly the
//!   freshly-queued case; draining emits no metric at all, or every frame
//!   would be counted twice.
//!   Same boundary `h2_flow_control` and `h2_stream_table` draw — this module
//!   stays log- and metrics-free.
//! - **The buffer.** [`H2ControlTx::drain_rst_streams_into`] writes into a
//!   caller-supplied `&mut [u8]`, the shape
//!   [`super::h2_flow_control::H2FlowControl::drain_window_updates_into`]
//!   already uses and that `serializer::gen_*` established before either.
//!   The caller passes `zero.storage.space()` and `fill()`s the returned
//!   byte count, so this module never holds a second buffer whose lifetime
//!   someone has to remember to coordinate — the hazard LIFECYCLE invariant
//!   24 exists to name.
//!
//! **Partial drains are normal, not an error.** A pass serializes as many
//! whole frames as fit and removes exactly those from the queue; the rest
//! stay queued for the next `writable()`. `enqueue_rst` already armed
//! `Ready::WRITABLE`, so a short buffer delays a frame, never drops it.

use std::collections::HashSet;

use crate::{
    Readiness,
    protocol::mux::{
        StreamId,
        parser::{self, H2Error},
        serializer,
    },
};

/// Hard cap on the lifetime count of queued RST_STREAM frames before the
/// connection escalates to `GOAWAY(ENHANCE_YOUR_CALM)` (CVE-2025-8671,
/// MadeYouReset). Checked against the never-decaying lifetime counter rather
/// than the pending queue length, because `writable()` drains the queue
/// between `readable()` calls, so the pending count alone may never reach the
/// cap even under sustained abuse.
pub(super) const MAX_PENDING_RST_STREAMS: usize = 200;

/// Outcome of [`H2ControlTx::enqueue_rst`]. Mirrors
/// [`super::h2_flow_control::QueueWindowUpdateOutcome`]: this module stays log-
/// and metrics-free and `ConnectionH2::enqueue_rst` maps each variant to its
/// accounting.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum EnqueueRstOutcome {
    /// Freshly queued. The caller must account it: tx counter, per-error
    /// breakdown, and the CVE-2025-8671 MadeYouReset emitted-lifetime cap.
    Queued,
    /// The wire id was already in `rst_sent` — a benign re-entrant
    /// idempotency, NOT a new wire emission. Nothing was queued and nothing
    /// is accounted.
    Deduped,
    /// The pending queue was already at its per-insert cap: not queued, not
    /// accounted.
    ///
    /// Nothing that would have reached the wire is lost here, and — as long as
    /// the connection has not already entered `H2State::GoAway`/`H2State::Error`
    /// — the drop is not silent to the peer. [`H2ControlTx::check_invariants`]
    /// holds `total_rst_streams_queued >= pending_rst_streams.len()`, so a full
    /// queue implies `total_rst_streams_queued >= MAX_PENDING_RST_STREAMS`,
    /// which is the counter half — [`H2ControlTx::lifetime_cap_reached`] — of
    /// the condition `ConnectionH2::flush_pending_control_frames` tests
    /// *before* its drain loop, returning `goaway(EnhanceYourCalm)` instead of
    /// serialising anything. The 200 enqueues that filled the queue each armed
    /// `Ready::WRITABLE`, so that escalation runs on the next writable tick on
    /// both `cancel_timed_out_streams` call paths, including `Mux::timeout`
    /// against a silent peer. The peer is told to back off with
    /// `GOAWAY(ENHANCE_YOUR_CALM)` (RFC 9113 §6.8, the documented answer to
    /// RST_STREAM abuse since CVE-2023-44487) and the connection is torn down,
    /// rather than being left believing a stream is still live.
    ///
    /// The other half of that condition is a state gate —
    /// `!matches!(self.state, H2State::GoAway | H2State::Error)` — and the RST
    /// drain underneath it has none, so the implication holds only until the
    /// first GOAWAY. `ConnectionH2::goaway` sets `H2State::GoAway` without
    /// calling [`H2ControlTx::clear_pending`], so a `Mux::timeout` reap landing
    /// in that window is refused here and raises no second escalation while the
    /// drain still serialises what is queued. The window is bounded and benign:
    /// the peer already holds the GOAWAY that made the connection terminal, and
    /// `writable()`'s `H2State::GoAway` arm force-disconnects on the same pass
    /// once the TLS buffer is flushed.
    Dropped,
}

/// Queue of proxy-emitted RST_STREAM frames awaiting serialization.
pub(super) struct H2ControlTx {
    /// Frames queued but not yet written to the wire, in queue order.
    pending_rst_streams: Vec<(StreamId, H2Error)>,
    /// Lifetime count of frames that ever entered `pending_rst_streams`.
    /// Never decremented: the MadeYouReset cap relies on it not
    /// under-counting across drains.
    total_rst_streams_queued: usize,
    /// Per-insert bound on `pending_rst_streams`. Always
    /// [`MAX_PENDING_RST_STREAMS`] in production; a field rather than the
    /// constant so the bound's own tests can reach it in a handful of inserts
    /// instead of two hundred.
    max_pending: usize,
}

impl H2ControlTx {
    pub(super) fn new() -> Self {
        Self::with_cap(MAX_PENDING_RST_STREAMS)
    }

    /// [`Self::new`] with an explicit per-insert bound.
    ///
    /// Production always uses [`MAX_PENDING_RST_STREAMS`]; this exists so the
    /// bound's own tests reach capacity in a handful of inserts and so the
    /// quickcheck property below reaches it on almost every generated run.
    fn with_cap(max_pending: usize) -> Self {
        Self {
            pending_rst_streams: Vec::new(),
            total_rst_streams_queued: 0,
            max_pending,
        }
    }

    /// Queue one RST_STREAM for `wire_stream_id`.
    ///
    /// The returned [`EnqueueRstOutcome`] lets `ConnectionH2::enqueue_rst`
    /// account the RST only on the freshly-queued path, so neither a duplicate
    /// call nor an at-capacity refusal inflates the per-error counter or trips
    /// the MadeYouReset cap for a frame that never reaches the wire.
    ///
    /// Invariants enforced here:
    /// - **Dedupe** via `rst_sent`: at most one queued RST per wire stream
    ///   id. `HashSet::insert` returns `false` when the id is already
    ///   present; the short-circuit on that branch keeps the queue, the
    ///   lifetime counter and the wire counts consistent.
    /// - **MadeYouReset queued cap**: each freshly queued RST bumps the
    ///   lifetime counter that [`Self::lifetime_cap_reached`] polices.
    /// - **Per-insert queue bound** (`max_pending`): the queue itself refuses
    ///   to grow past the cap, so the bound holds however many RSTs ONE caller
    ///   queues between two `ConnectionH2::flush_pending_control_frames`
    ///   passes. `cancel_timed_out_streams` is the largest such caller: it
    ///   walks the whole timed-out set in a single sweep, and a reap of more
    ///   than [`MAX_PENDING_RST_STREAMS`] streams — which needs an
    ///   operator-raised `max_concurrent_streams`, because that is what bounds
    ///   the live set the reaper walks — used to push `pending_rst_streams`
    ///   past the bound [`Self::check_invariants`] asserts, panicking on the
    ///   next inbound frame in a debug build or growing unbounded in release
    ///   (sozu-proxy/sozu#1413). `max_concurrent_streams` bounds one sweep, not
    ///   the queue: the queue holds what every caller queued since the last
    ///   successful drain, and the DATA-on-closed-stream enqueue is a second
    ///   caller `ConnectionH2::check_invariants` never inspects — see
    ///   `ConnectionH2::enqueue_rst`.
    /// - **LIFECYCLE invariant 15** (edge-triggered epoll): pair
    ///   `Ready::WRITABLE` interest with the event bit so `writable()` is
    ///   scheduled on the next tick.
    pub(super) fn enqueue_rst(
        &mut self,
        rst_sent: &mut HashSet<StreamId>,
        readiness: &mut Readiness,
        wire_stream_id: StreamId,
        error: H2Error,
    ) -> EnqueueRstOutcome {
        let pending_before = self.pending_rst_streams.len();
        let total_before = self.total_rst_streams_queued;
        // Queue bound, tested BEFORE `rst_sent` is touched. Recording an id
        // whose RST was never queued would make a later, legitimate
        // `enqueue_rst` for that same stream dedupe against a frame that does
        // not exist.
        if pending_before >= self.max_pending {
            return EnqueueRstOutcome::Dropped;
        }
        if !rst_sent.insert(wire_stream_id) {
            // Dedupe short-circuit: the id was already queued/flushed. We must
            // NOT touch any of the wire-count state, otherwise duplicate calls
            // inflate the MadeYouReset (CVE-2025-8671) lifetime cap with frames
            // that never reach the wire.
            debug_assert!(
                rst_sent.contains(&wire_stream_id),
                "dedupe path requires the id to already be present in rst_sent"
            );
            debug_assert_eq!(
                self.pending_rst_streams.len(),
                pending_before,
                "dedupe path must not enqueue a new pending RST"
            );
            debug_assert_eq!(
                self.total_rst_streams_queued, total_before,
                "dedupe path must not bump the queued-RST lifetime counter"
            );
            self.debug_assert_invariants();
            return EnqueueRstOutcome::Deduped;
        }
        self.pending_rst_streams.push((wire_stream_id, error));
        self.total_rst_streams_queued += 1;
        readiness.arm_writable();
        // Post-condition: a freshly-queued RST advances both the pending Vec
        // and the lifetime counter by exactly one, and the id is now tracked
        // for dedupe.
        debug_assert!(
            rst_sent.contains(&wire_stream_id),
            "freshly-queued RST must be recorded in rst_sent for future dedupe"
        );
        debug_assert_eq!(
            self.pending_rst_streams.len(),
            pending_before + 1,
            "a freshly-queued RST must push exactly one pending entry"
        );
        debug_assert_eq!(
            self.total_rst_streams_queued,
            total_before + 1,
            "a freshly-queued RST must bump the queued-RST lifetime counter by one"
        );
        debug_assert_eq!(
            self.pending_rst_streams.last().map(|(id, _)| *id),
            Some(wire_stream_id),
            "the just-pushed entry must be the requested wire stream id"
        );
        self.debug_assert_invariants();
        EnqueueRstOutcome::Queued
    }

    /// Serialize as many queued RST_STREAM frames as fit into `buf`, in queue
    /// order, and remove exactly those from the queue.
    ///
    /// Returns `(bytes_written, frames_written)`. A frame is written whole or
    /// not at all — a frame that would straddle the end of `buf` stops the
    /// pass and stays queued, so the caller can `fill()` the returned byte
    /// count without inspecting frame boundaries. Same `(usize, usize)`
    /// contract as
    /// [`super::h2_flow_control::H2FlowControl::drain_window_updates_into`].
    ///
    /// Emission order is queue order, which is arrival order. Unlike the
    /// WINDOW_UPDATE drain there is no coalescing map to impose a total order
    /// on: a queued RST is already keyed to one stream id and deduped at
    /// queue time, so arrival order is a total order over distinct ids and is
    /// already deterministic.
    pub(super) fn drain_rst_streams_into(&mut self, buf: &mut [u8]) -> (usize, usize) {
        let pending_before = self.pending_rst_streams.len();
        let mut offset = 0;
        let mut written_count = 0;
        for &(stream_id, ref error) in &self.pending_rst_streams {
            let frame_size = parser::FRAME_HEADER_SIZE + parser::RST_STREAM_PAYLOAD_SIZE as usize;
            if offset + frame_size > buf.len() {
                break;
            }
            match serializer::gen_rst_stream(&mut buf[offset..], stream_id, error.to_owned()) {
                Ok((_, _)) => {
                    offset += frame_size;
                    written_count += 1;
                }
                Err(_) => break,
            }
        }
        self.pending_rst_streams.drain(..written_count);
        debug_assert_eq!(
            self.pending_rst_streams.len(),
            pending_before - written_count,
            "the drain must remove exactly the frames it serialized"
        );
        debug_assert!(
            offset <= buf.len(),
            "the drain must never report more bytes than the buffer holds"
        );
        // Pair (negative space): reporting frames without bytes, or bytes
        // without frames, would desynchronise the caller's `fill()` from the
        // queue it just shortened.
        debug_assert_eq!(
            offset == 0,
            written_count == 0,
            "bytes written and frames written must be zero together"
        );
        self.debug_assert_invariants();
        (offset, written_count)
    }

    /// True while frames are queued but not yet serialized.
    pub(super) fn has_pending(&self) -> bool {
        !self.pending_rst_streams.is_empty()
    }

    /// The queued frames, in emission order.
    ///
    /// Inspection only, and deliberately `#[cfg(test)]`: production code needs
    /// [`Self::has_pending`] and nothing finer, and a queue this type can hand
    /// out by reference is a queue someone can be tempted to push onto without
    /// the dedupe set or the lifetime counter. `h2.rs`'s own tests use it to
    /// assert on what a refusal path queued.
    #[cfg(test)]
    pub(super) fn pending(&self) -> &[(StreamId, H2Error)] {
        &self.pending_rst_streams
    }

    /// Lifetime count of frames that ever entered the queue. Never decreases.
    pub(super) fn lifetime_queued(&self) -> usize {
        self.total_rst_streams_queued
    }

    /// True once the CVE-2025-8671 MadeYouReset lifetime cap has been reached
    /// and the connection must escalate to `GOAWAY(ENHANCE_YOUR_CALM)`.
    pub(super) fn lifetime_cap_reached(&self) -> bool {
        self.total_rst_streams_queued >= MAX_PENDING_RST_STREAMS
    }

    /// Drop every queued frame without serializing it, leaving the lifetime
    /// counter untouched.
    ///
    /// The counter deliberately survives: it is the MadeYouReset evidence,
    /// and a peer that provoked 200 RSTs has done so whether or not the
    /// connection got to write them.
    pub(super) fn clear_pending(&mut self) {
        let total_before = self.total_rst_streams_queued;
        self.pending_rst_streams.clear();
        debug_assert_eq!(
            self.total_rst_streams_queued, total_before,
            "clearing the queue must not rewind the lifetime counter"
        );
        self.debug_assert_invariants();
    }

    /// Full invariant sweep, run as a post-condition of every mutating method.
    ///
    /// 1. The never-decaying lifetime counter is always `>=` the currently
    ///    pending queue length. The MadeYouReset cap relies on the lifetime
    ///    counter never under-counting.
    /// 2. The pending queue stays within its hard cap + 1 — the escalation
    ///    tripwire fires *at* the cap, so one entry may sit above it for the
    ///    single call between queueing and the caller's check. The per-insert
    ///    bound in [`Self::enqueue_rst`] is what holds this; the assertion is
    ///    the tripwire that fires when it is removed.
    #[cfg(debug_assertions)]
    fn check_invariants(&self) {
        debug_assert!(
            self.total_rst_streams_queued >= self.pending_rst_streams.len(),
            "queued-RST lifetime counter ({}) must be >= currently-pending queue ({})",
            self.total_rst_streams_queued,
            self.pending_rst_streams.len()
        );
        debug_assert!(
            self.pending_rst_streams.len() <= self.max_pending + 1,
            "pending RST queue must stay within its hard cap (escalates at the cap)"
        );
    }

    fn debug_assert_invariants(&self) {
        #[cfg(debug_assertions)]
        self.check_invariants();
    }
}

#[cfg(test)]
mod tests {
    use quickcheck::{Arbitrary, Gen, quickcheck};

    use super::*;

    fn readiness() -> Readiness {
        Readiness::new()
    }

    const RST_FRAME_SIZE: usize =
        parser::FRAME_HEADER_SIZE + parser::RST_STREAM_PAYLOAD_SIZE as usize;

    // ── enqueue_rst: queue / dedupe / counter / arm invariants ───────────
    //
    // These four moved here verbatim in behaviour from `h2.rs`, where they
    // drove the `enqueue_rst_into` free function this type replaced. That
    // function existed so the invariants could be tested without building a
    // full `ConnectionH2<Front>` fixture; owning them on `H2ControlTx` gets
    // the same thing from the type instead of from a parameter list. Their
    // assertions now read the `EnqueueRstOutcome` the method returns in place
    // of the `bool` the free function did.

    #[test]
    fn test_enqueue_rst_into_populates_queue_and_dedupe() {
        let mut tx = H2ControlTx::new();
        let mut sent: HashSet<StreamId> = HashSet::new();
        let mut readiness = Readiness::new();

        let first = tx.enqueue_rst(&mut sent, &mut readiness, 5, H2Error::ProtocolError);
        assert_eq!(
            first,
            EnqueueRstOutcome::Queued,
            "first call must report a fresh queue"
        );
        // Second call for the same stream must be a no-op AND report
        // `Deduped` so accounting in `ConnectionH2::enqueue_rst` skips this case.
        let second = tx.enqueue_rst(&mut sent, &mut readiness, 5, H2Error::InternalError);
        assert_eq!(
            second,
            EnqueueRstOutcome::Deduped,
            "second call for same stream must report Deduped"
        );

        assert_eq!(
            tx.pending().len(),
            1,
            "dedupe must collapse to a single entry"
        );
        assert_eq!(
            tx.pending()[0],
            (5, H2Error::ProtocolError),
            "the first error wins — second push is ignored"
        );
        assert_eq!(
            tx.lifetime_queued(),
            1,
            "queued-cap counter must bump exactly once"
        );
        assert!(sent.contains(&5), "rst_sent must record the id");
    }

    #[test]
    fn test_enqueue_rst_into_bumps_total_for_distinct_ids() {
        let mut tx = H2ControlTx::new();
        let mut sent: HashSet<StreamId> = HashSet::new();
        let mut readiness = Readiness::new();

        for sid in [1u32, 3, 5, 7] {
            tx.enqueue_rst(&mut sent, &mut readiness, sid, H2Error::ProtocolError);
        }

        assert_eq!(tx.pending().len(), 4);
        assert_eq!(tx.lifetime_queued(), 4);
        assert_eq!(sent.len(), 4);
    }

    #[test]
    fn test_enqueue_rst_into_arms_writable_in_invariant_15_form() {
        let mut tx = H2ControlTx::new();
        let mut sent: HashSet<StreamId> = HashSet::new();
        let mut readiness = Readiness::new();

        // Precondition: no WRITABLE bits set.
        assert!(!readiness.interest.is_writable());
        assert!(!readiness.event.is_writable());

        tx.enqueue_rst(&mut sent, &mut readiness, 9, H2Error::FlowControlError);

        // Postcondition: invariant-15 — both `interest` and `event` WRITABLE
        // are raised so the next tick runs `writable()` under edge-triggered
        // epoll.
        assert!(
            readiness.interest.is_writable(),
            "arm_writable must raise the interest bit"
        );
        assert!(
            readiness.event.is_writable(),
            "arm_writable must raise the event bit (edge-triggered epoll)"
        );
    }

    #[test]
    fn test_enqueue_rst_into_dedupe_does_not_rearm_writable() {
        // Dedupe is a pure short-circuit: if the stream id is already in
        // `rst_sent`, we do not touch the readiness. This matters because
        // a re-entrant reset_stream call during a cascading error path
        // would otherwise re-raise WRITABLE unnecessarily — harmless but
        // noisy in metrics.
        let mut tx = H2ControlTx::new();
        let mut sent: HashSet<StreamId> = HashSet::new();
        sent.insert(11);
        let mut readiness = Readiness::new();

        tx.enqueue_rst(&mut sent, &mut readiness, 11, H2Error::ProtocolError);

        assert!(
            tx.pending().is_empty(),
            "already-sent ids must not queue a second frame"
        );
        assert_eq!(tx.lifetime_queued(), 0);
        assert!(!readiness.interest.is_writable());
        assert!(!readiness.event.is_writable());
    }

    // ── enqueue_rst: per-insert queue bound (sozu-proxy/sozu#1413) ───────
    //
    // The bound has to hold at the INSERT, not at the drain: one
    // `cancel_timed_out_streams` sweep queues an RST per timed-out stream with
    // no `ConnectionH2::flush_pending_control_frames` pass in between, so a
    // drain-side check bounds only what is written and never what is held.

    /// At capacity the queue takes nothing, counts nothing, records nothing in
    /// `rst_sent`, and does not re-arm WRITABLE — the same no-side-effect
    /// shape as the dedupe short-circuit above.
    ///
    /// `rst_sent` is the load-bearing one: marking an id as reset without
    /// queuing its frame would make a later, legitimate `enqueue_rst` for that
    /// stream dedupe against a frame that was never queued.
    ///
    /// To SEE THIS RED: in [`H2ControlTx::enqueue_rst`], move the
    /// `pending_before >= self.max_pending` guard down so it runs *after* the
    /// `if !rst_sent.insert(wire_stream_id) { … }` block instead of before it,
    /// then run `cargo test -p sozu-lib --locked
    /// test_enqueue_rst_into_refuses_at_capacity_without_side_effects`. The
    /// `rst_sent` assertion fails with `an at-capacity refusal must not record
    /// the id as reset`.
    #[test]
    fn test_enqueue_rst_into_refuses_at_capacity_without_side_effects() {
        const MAX: usize = 4;
        let mut tx = H2ControlTx::with_cap(MAX);
        let mut sent: HashSet<StreamId> = HashSet::new();
        let mut readiness = Readiness::new();

        for sid in [1u32, 3, 5, 7] {
            assert_eq!(
                tx.enqueue_rst(&mut sent, &mut readiness, sid, H2Error::Cancel),
                EnqueueRstOutcome::Queued,
                "every insert below the cap must be queued"
            );
        }
        assert_eq!(
            tx.pending().len(),
            MAX,
            "the queue must fill to exactly the cap"
        );

        let mut at_capacity = Readiness::new();
        assert_eq!(
            tx.enqueue_rst(&mut sent, &mut at_capacity, 9, H2Error::Cancel),
            EnqueueRstOutcome::Dropped,
            "an insert at the cap must be refused"
        );

        assert_eq!(
            tx.pending().len(),
            MAX,
            "a refused insert must leave the queue at the cap"
        );
        assert_eq!(
            tx.lifetime_queued(),
            MAX,
            "a refused insert must not bump the queued-RST lifetime counter"
        );
        assert!(
            !sent.contains(&9),
            "an at-capacity refusal must not record the id as reset"
        );
        assert!(
            !at_capacity.interest.is_writable() && !at_capacity.event.is_writable(),
            "a refused insert must not arm WRITABLE for a frame it did not queue"
        );
    }

    // ── quickcheck: the bound survives any reap/enqueue/drain interleaving ─

    /// One step of an abstract RST workload against the queue.
    #[derive(Clone, Debug)]
    enum RstStep {
        /// A single proxy-emitted reset (`reset_stream`, DATA-on-closed,
        /// `refuse_stream_and_discard`) on a small, deliberately colliding id
        /// space so the dedupe path is exercised too.
        Enqueue(u8),
        /// One `cancel_timed_out_streams` sweep: up to 96 never-before-seen
        /// wire ids queued back-to-back with no drain in between.
        Reap(u8),
        /// One `ConnectionH2::flush_pending_control_frames` drain, through the
        /// real serializer, into a buffer with room for `n % 8` frames.
        Drain(u8),
    }

    impl Arbitrary for RstStep {
        fn arbitrary(g: &mut Gen) -> Self {
            match u8::arbitrary(g) % 3 {
                0 => RstStep::Enqueue(u8::arbitrary(g)),
                1 => RstStep::Reap(u8::arbitrary(g)),
                _ => RstStep::Drain(u8::arbitrary(g)),
            }
        }
    }

    /// Property: for ANY interleaving of single resets, mass reaps and partial
    /// drains, the pending queue stays within `max_pending`, the never-decaying
    /// lifetime counter never under-counts it, and every queued id is recorded
    /// exactly once — the four facts `ConnectionH2::check_invariants`
    /// invariant 3 and the dedupe invariant assert on the live connection.
    ///
    /// `MAX` is 16 rather than the production [`MAX_PENDING_RST_STREAMS`] so a
    /// generated `Reap` reaches the cap on almost every run; reachability
    /// itself is pinned deterministically by
    /// [`test_enqueue_rst_into_refuses_at_capacity_without_side_effects`] and,
    /// on a live connection, by
    /// `mass_reap_keeps_the_pending_rst_queue_within_its_hard_cap` in `h2.rs`.
    ///
    /// To SEE THIS RED: delete the `pending_before >= self.max_pending` guard
    /// in [`H2ControlTx::enqueue_rst`], then run `cargo test -p sozu-lib
    /// --locked prop_pending_rst_queue_stays_within_its_bound`. The first
    /// `Reap` longer than `MAX` pushes the queue past the bound and
    /// [`Self::check_invariants`] — the post-condition every mutating method
    /// runs — panics before the property body reaches its own
    /// `pending().len() > MAX` return, so quickcheck reports it as a runtime
    /// error rather than a `false`: `[quickcheck] TEST FAILED (runtime error).
    /// Arguments: ([Reap(183)])`, `Error: "pending RST queue must stay within
    /// its hard cap (escalates at the cap)"` on the run that produced this
    /// comment.
    #[test]
    fn prop_pending_rst_queue_stays_within_its_bound() {
        fn prop(steps: Vec<RstStep>) -> bool {
            const MAX: usize = 16;
            let mut tx = H2ControlTx::with_cap(MAX);
            let mut sent: HashSet<StreamId> = HashSet::new();
            let mut readiness = Readiness::new();
            // Disjoint from the `Enqueue` id space (odd, 1..=127) so a reap
            // always queues ids the workload has not used before.
            let mut next_reaped: StreamId = 1001;

            for step in steps {
                match step {
                    RstStep::Enqueue(id) => {
                        tx.enqueue_rst(
                            &mut sent,
                            &mut readiness,
                            2 * (id as StreamId % 64) + 1,
                            H2Error::ProtocolError,
                        );
                    }
                    RstStep::Reap(n) => {
                        for _ in 0..=(n % 96) {
                            tx.enqueue_rst(&mut sent, &mut readiness, next_reaped, H2Error::Cancel);
                            next_reaped += 2;
                        }
                    }
                    RstStep::Drain(n) => {
                        let mut buf = vec![0u8; RST_FRAME_SIZE * (n as usize % 8)];
                        tx.drain_rst_streams_into(&mut buf);
                    }
                }

                if tx.pending().len() > MAX {
                    return false;
                }
                if tx.lifetime_queued() < tx.pending().len() {
                    return false;
                }
                if !tx.pending().iter().all(|(id, _)| sent.contains(id)) {
                    return false;
                }
                let unique: HashSet<StreamId> = tx.pending().iter().map(|(id, _)| *id).collect();
                if unique.len() != tx.pending().len() {
                    return false;
                }
            }
            true
        }
        quickcheck(prop as fn(Vec<RstStep>) -> bool);
    }

    // ── drain: whole frames only, queue order, exact removal ────────────

    #[test]
    fn the_drain_serializes_every_queued_frame_when_the_buffer_is_large_enough() {
        let mut tx = H2ControlTx::new();
        let mut rst_sent = HashSet::new();
        let mut readiness = readiness();
        for id in [1, 3, 5] {
            tx.enqueue_rst(&mut rst_sent, &mut readiness, id, H2Error::Cancel);
        }
        let mut buf = vec![0u8; RST_FRAME_SIZE * 8];

        let (bytes, frames) = tx.drain_rst_streams_into(&mut buf);

        assert_eq!(frames, 3);
        assert_eq!(bytes, RST_FRAME_SIZE * 3);
        assert!(!tx.has_pending(), "a complete drain empties the queue");
        assert_eq!(
            tx.lifetime_queued(),
            3,
            "draining must not rewind the lifetime counter"
        );
    }

    #[test]
    fn a_short_buffer_writes_whole_frames_only_and_leaves_the_rest_queued() {
        let mut tx = H2ControlTx::new();
        let mut rst_sent = HashSet::new();
        let mut readiness = readiness();
        for id in [1, 3, 5, 7] {
            tx.enqueue_rst(&mut rst_sent, &mut readiness, id, H2Error::Cancel);
        }
        // Room for two whole frames and one byte of a third.
        let mut buf = vec![0u8; RST_FRAME_SIZE * 2 + 1];

        let (bytes, frames) = tx.drain_rst_streams_into(&mut buf);

        assert_eq!(frames, 2, "a frame is written whole or not at all");
        assert_eq!(
            bytes,
            RST_FRAME_SIZE * 2,
            "the reported byte count must not include a partial frame"
        );
        assert_eq!(tx.pending().len(), 2, "the unwritten frames stay queued");
    }

    #[test]
    fn a_buffer_too_small_for_one_frame_writes_nothing_and_drops_nothing() {
        let mut tx = H2ControlTx::new();
        let mut rst_sent = HashSet::new();
        let mut readiness = readiness();
        tx.enqueue_rst(&mut rst_sent, &mut readiness, 1, H2Error::Cancel);
        let mut buf = vec![0u8; RST_FRAME_SIZE - 1];

        let (bytes, frames) = tx.drain_rst_streams_into(&mut buf);

        assert_eq!((bytes, frames), (0, 0));
        assert_eq!(tx.pending().len(), 1, "nothing fit, so nothing was dropped");
    }

    #[test]
    fn an_exactly_sized_buffer_writes_the_frame() {
        let mut tx = H2ControlTx::new();
        let mut rst_sent = HashSet::new();
        let mut readiness = readiness();
        tx.enqueue_rst(&mut rst_sent, &mut readiness, 1, H2Error::Cancel);
        let mut buf = vec![0u8; RST_FRAME_SIZE];

        let (bytes, frames) = tx.drain_rst_streams_into(&mut buf);

        assert_eq!((bytes, frames), (RST_FRAME_SIZE, 1));
        assert!(!tx.has_pending());
    }

    #[test]
    fn draining_an_empty_queue_is_a_no_op() {
        let mut tx = H2ControlTx::new();
        let mut buf = vec![0u8; RST_FRAME_SIZE * 4];

        assert_eq!(tx.drain_rst_streams_into(&mut buf), (0, 0));
    }

    // ── the MadeYouReset lifetime cap ───────────────────────────────────

    #[test]
    fn the_lifetime_cap_is_reached_at_the_cap_not_above_it() {
        let mut tx = H2ControlTx::new();
        let mut rst_sent = HashSet::new();
        let mut readiness = readiness();

        for id in 0..MAX_PENDING_RST_STREAMS as u32 - 1 {
            tx.enqueue_rst(&mut rst_sent, &mut readiness, id * 2 + 1, H2Error::Cancel);
        }
        assert!(
            !tx.lifetime_cap_reached(),
            "one below the cap must not trip it"
        );

        tx.enqueue_rst(&mut rst_sent, &mut readiness, 100_001, H2Error::Cancel);
        assert!(tx.lifetime_cap_reached(), "the cap trips AT the cap");
    }

    /// Draining does not rewind the cap: the counter is the MadeYouReset
    /// evidence, and a peer that provoked the cap has done so whether or not
    /// the frames were written.
    ///
    /// TO SEE THIS RED: in [`H2ControlTx::drain_rst_streams_into`], add
    /// `self.total_rst_streams_queued -= written_count;` after the
    /// `self.pending_rst_streams.drain(..written_count);` line. The final
    /// assertion then fails with
    /// `the cap must stay tripped after the queue drains`, and
    /// [`the_drain_serializes_every_queued_frame_when_the_buffer_is_large_enough`]
    /// fails alongside it on `draining must not rewind the lifetime counter` —
    /// two tests, because the counter is the CVE-2025-8671 evidence and
    /// rewinding it is exactly how a peer would evade the cap: drain, re-queue,
    /// repeat.
    #[test]
    fn draining_does_not_rewind_the_lifetime_cap() {
        let mut tx = H2ControlTx::new();
        let mut rst_sent = HashSet::new();
        let mut readiness = readiness();
        for id in 0..MAX_PENDING_RST_STREAMS as u32 {
            tx.enqueue_rst(&mut rst_sent, &mut readiness, id * 2 + 1, H2Error::Cancel);
        }
        assert!(tx.lifetime_cap_reached());

        let mut buf = vec![0u8; RST_FRAME_SIZE * (MAX_PENDING_RST_STREAMS + 1)];
        let (_, frames) = tx.drain_rst_streams_into(&mut buf);

        assert_eq!(frames, MAX_PENDING_RST_STREAMS);
        assert!(!tx.has_pending());
        assert!(
            tx.lifetime_cap_reached(),
            "the cap must stay tripped after the queue drains"
        );
    }

    #[test]
    fn clearing_the_queue_does_not_rewind_the_lifetime_counter() {
        let mut tx = H2ControlTx::new();
        let mut rst_sent = HashSet::new();
        let mut readiness = readiness();
        for id in [1, 3, 5] {
            tx.enqueue_rst(&mut rst_sent, &mut readiness, id, H2Error::Cancel);
        }

        tx.clear_pending();

        assert!(!tx.has_pending());
        assert_eq!(
            tx.lifetime_queued(),
            3,
            "the MadeYouReset evidence survives a queue clear"
        );
    }
}
