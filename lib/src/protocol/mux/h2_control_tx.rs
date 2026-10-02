//! Proxy-emitted RST_STREAM transmit queue for [`super::h2::ConnectionH2`].
//!
//! Groups the queued-RST state behind a narrow, closed API — the shape the
//! `hpack_state` / `h2_flow_control` / `h2_stream_table` / `h2_drain` /
//! `h2_flood_detector` / `h2_header_reassembly` / `h2_scheduler` steps
//! established. Every field here is private to this module; `ConnectionH2`
//! reaches them only through the accessor methods declared below.
//!
//! **This module owns**: the pending `(StreamId, H2Error)` queue, its bound
//! ([`pending_rst_bound`], at least [`MIN_PENDING_RST_STREAMS`]) and the
//! per-insert refusal that keeps the queue within it (sozu-proxy/sozu#1413),
//! the overflow flag that records such a refusal, the
//! `total_rst_streams_queued` lifetime count the session log reports, and the
//! serialization of queued frames into a caller-supplied buffer.
//!
//! The bound limits what is *pending* — queued and not yet serialized — not
//! how many resets a connection may emit over its lifetime: the queue drains
//! on every `writable()`, so only a burst inside one pass can fill it. The
//! peer-provoked resets that make up CVE-2025-8671 MadeYouReset are capped by
//! the flood detector (`H2FloodDetector::record_rst_emitted`,
//! `lib/src/protocol/mux/h2_flood_detector.rs`), which never sees the resets
//! Sōzu decides on its own. Go bounds its queued control frames the same way
//! (`maxQueuedControlFrames`, `http2/server.go`), as do Envoy
//! (`max_outbound_control_frames`) and nghttp2 (`NGHTTP2_DEFAULT_MAX_OBQ_FLOOD_ITEM`).
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
//! - **The decision to drain.** The caller drains whenever frames are
//!   queued, into the room it sizes from [`H2ControlTx::pending_len`] at the
//!   end of its ordered output queue (#1604); before that queue existed, a
//!   partially-written zero buffer or a header block mid-reassembly also
//!   gated it. The caller decides *whether*; this module only decides *what
//!   bytes*.
//! - **Metrics, logs and the flood detector.** Accounting happens once, at
//!   queue time, in `ConnectionH2::account_emitted_rst`, because a lifetime-cap
//!   trip converts to a connection-wide GOAWAY that only the connection can
//!   return. [`H2ControlTx::enqueue_rst`] therefore reports an
//!   [`EnqueueRstOutcome`] and leaves the caller to account exactly the
//!   freshly-queued case; draining emits no metric at all, or every frame
//!   would be counted twice. Whether a reset counts against the peer at all
//!   is the caller's call too (`RstOrigin` in `lib/src/protocol/mux/h2.rs`).
//!   Same boundary `h2_flow_control` and `h2_stream_table` draw — this module
//!   stays log- and metrics-free.
//! - **The buffer.** [`H2ControlTx::drain_rst_streams_into`] writes into a
//!   caller-supplied `&mut [u8]`, the shape
//!   [`super::h2_flow_control::H2FlowControl::drain_window_updates_into`]
//!   already uses and that `serializer::gen_*` established before either.
//!   The caller passes room it reserved at the end of its ordered output
//!   queue (`h2_output::H2Output::push_frames`) and keeps the returned byte
//!   count, so this module never holds a second buffer whose lifetime
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

/// Floor of the bound on RST_STREAM frames queued and not yet serialized.
///
/// Above the outbound control-frame queue bounds of Envoy
/// (`max_outbound_control_frames`, 1000) and nghttp2
/// (`NGHTTP2_DEFAULT_MAX_OBQ_FLOOD_ITEM`, 1000), below Go's
/// (`maxQueuedControlFrames`, 10 000). A connection whose queue overflows
/// within one pass escalates to `GOAWAY(ENHANCE_YOUR_CALM)` — see
/// [`EnqueueRstOutcome::Dropped`].
pub(super) const MIN_PENDING_RST_STREAMS: usize = 4000;

/// The pending-queue bound for a connection advertising
/// `max_concurrent_streams`: [`MIN_PENDING_RST_STREAMS`], raised to four
/// resets per concurrent stream so the largest single caller — one idle
/// reaper sweep, at most one `CANCEL` per open stream — can never fill it on
/// its own however far an operator raises `h2_max_concurrent_streams`. That
/// knob is how the bound is configured.
pub(super) fn pending_rst_bound(max_concurrent_streams: u32) -> usize {
    MIN_PENDING_RST_STREAMS.max((max_concurrent_streams as usize).saturating_mul(4))
}

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
    /// accounted, and [`H2ControlTx::overflowed`] now reports `true`.
    ///
    /// Nothing that would have reached the wire is lost here, and — as long as
    /// the connection has not already entered `H2State::GoAway`/`H2State::Error`
    /// — the drop is not silent to the peer. [`H2ControlTx::overflowed`] is
    /// the condition `ConnectionH2::flush_pending_control_frames` tests
    /// *before* its drain loop, returning `goaway(EnhanceYourCalm)` instead of
    /// serialising anything. The enqueues that filled the queue each armed
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
    /// Never decremented. Reported by the session log line; it bounds
    /// nothing.
    total_rst_streams_queued: usize,
    /// Per-insert bound on `pending_rst_streams`:
    /// [`pending_rst_bound`] of the connection's `max_concurrent_streams` in
    /// production; a field so the bound's own tests can reach it in a handful
    /// of inserts.
    max_pending: usize,
    /// Set once an insert was refused because the queue was at
    /// `max_pending` ([`EnqueueRstOutcome::Dropped`]). Never cleared: the
    /// connection escalates to GOAWAY on the next flush.
    overflowed: bool,
}

impl H2ControlTx {
    /// A queue bounded by [`pending_rst_bound`] of the connection's
    /// advertised `max_concurrent_streams`.
    pub(super) fn new(max_concurrent_streams: u32) -> Self {
        Self::with_cap(pending_rst_bound(max_concurrent_streams))
    }

    /// [`Self::new`] with an explicit per-insert bound.
    ///
    /// Production always uses [`pending_rst_bound`]; this exists so the
    /// bound's own tests reach capacity in a handful of inserts and so the
    /// quickcheck property below reaches it on almost every generated run.
    pub(super) fn with_cap(max_pending: usize) -> Self {
        Self {
            pending_rst_streams: Vec::new(),
            total_rst_streams_queued: 0,
            max_pending,
            overflowed: false,
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
    ///   lifetime counter and the wire counts consistent. The caller passes
    ///   `None` for a stream that was never registered (a refused stream, DATA
    ///   on a closed one): only `H2StreamTable`'s eviction removes an id from
    ///   `rst_sent`, so recording such an id would keep it there for the
    ///   connection's lifetime, one entry per refused stream. Refused ids are
    ///   fresh by construction, so they need no dedupe.
    /// - **Lifetime count**: each freshly queued RST bumps
    ///   `total_rst_streams_queued`, for the session log.
    /// - **Per-insert queue bound** (`max_pending`): the queue itself refuses
    ///   to grow past the bound, so it holds however many RSTs ONE caller
    ///   queues between two `ConnectionH2::flush_pending_control_frames`
    ///   passes, and a refusal sets [`Self::overflowed`].
    ///   `cancel_timed_out_streams` is the largest such caller: it walks the
    ///   whole timed-out set in a single sweep, and a reap larger than the
    ///   bound used to push `pending_rst_streams` past the bound
    ///   [`Self::check_invariants`] asserts, panicking on the next inbound
    ///   frame in a debug build or growing unbounded in release
    ///   (sozu-proxy/sozu#1413). [`pending_rst_bound`] keeps one sweep below
    ///   the bound, but the queue holds what every caller queued since the
    ///   last successful drain, and the DATA-on-closed-stream and refusal
    ///   enqueues are callers `ConnectionH2::check_invariants` never
    ///   inspects — see `ConnectionH2::enqueue_rst`.
    /// - **LIFECYCLE invariant 15** (edge-triggered epoll): pair
    ///   `Ready::WRITABLE` interest with the event bit so `writable()` is
    ///   scheduled on the next tick.
    pub(super) fn enqueue_rst(
        &mut self,
        rst_sent: Option<&mut HashSet<StreamId>>,
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
            self.overflowed = true;
            debug_assert_eq!(
                self.pending_rst_streams.len(),
                pending_before,
                "an at-capacity refusal must not enqueue"
            );
            self.debug_assert_invariants();
            return EnqueueRstOutcome::Dropped;
        }
        if let Some(rst_sent) = rst_sent
            && !rst_sent.insert(wire_stream_id)
        {
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
        // and the lifetime counter by exactly one; a tracked id was recorded
        // for dedupe by the `insert` above.
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

    /// How many frames are queued, so the caller can size the room it hands
    /// [`Self::drain_rst_streams_into`] for all of them.
    pub(super) fn pending_len(&self) -> usize {
        self.pending_rst_streams.len()
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

    /// The per-insert bound on the pending queue.
    pub(super) fn max_pending(&self) -> usize {
        self.max_pending
    }

    /// True once an RST could not be queued because the pending queue was at
    /// its bound; the connection must then escalate to
    /// `GOAWAY(ENHANCE_YOUR_CALM)` rather than leave the peer believing the
    /// dropped stream is live.
    ///
    /// This is a bound on what is pending, not a lifetime cap: a connection
    /// that emits any number of resets, drained as they come, never sets it.
    pub(super) fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// Drop every queued frame without serializing it, leaving the lifetime
    /// counter and the overflow flag untouched.
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
    ///    pending queue length.
    /// 2. The pending queue stays within its hard cap + 1. The per-insert
    ///    bound in [`Self::enqueue_rst`] is what holds this; the assertion is
    ///    the tripwire that fires when it is removed.
    /// 3. An overflow is only ever recorded against a queue that reached its
    ///    bound — the flag cannot be set while `max_pending` was never hit,
    ///    which `total_rst_streams_queued >= max_pending` witnesses.
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
        debug_assert!(
            !self.overflowed || self.total_rst_streams_queued >= self.max_pending,
            "an overflow requires the queue to have reached its bound"
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
        let mut tx = H2ControlTx::new(100);
        let mut sent: HashSet<StreamId> = HashSet::new();
        let mut readiness = Readiness::new();

        let first = tx.enqueue_rst(Some(&mut sent), &mut readiness, 5, H2Error::ProtocolError);
        assert_eq!(
            first,
            EnqueueRstOutcome::Queued,
            "first call must report a fresh queue"
        );
        // Second call for the same stream must be a no-op AND report
        // `Deduped` so accounting in `ConnectionH2::enqueue_rst` skips this case.
        let second = tx.enqueue_rst(Some(&mut sent), &mut readiness, 5, H2Error::InternalError);
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
        let mut tx = H2ControlTx::new(100);
        let mut sent: HashSet<StreamId> = HashSet::new();
        let mut readiness = Readiness::new();

        for sid in [1u32, 3, 5, 7] {
            tx.enqueue_rst(Some(&mut sent), &mut readiness, sid, H2Error::ProtocolError);
        }

        assert_eq!(tx.pending().len(), 4);
        assert_eq!(tx.lifetime_queued(), 4);
        assert_eq!(sent.len(), 4);
    }

    #[test]
    fn test_enqueue_rst_into_arms_writable_in_invariant_15_form() {
        let mut tx = H2ControlTx::new(100);
        let mut sent: HashSet<StreamId> = HashSet::new();
        let mut readiness = Readiness::new();

        // Precondition: no WRITABLE bits set.
        assert!(!readiness.interest.is_writable());
        assert!(!readiness.event.is_writable());

        tx.enqueue_rst(
            Some(&mut sent),
            &mut readiness,
            9,
            H2Error::FlowControlError,
        );

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
        let mut tx = H2ControlTx::new(100);
        let mut sent: HashSet<StreamId> = HashSet::new();
        sent.insert(11);
        let mut readiness = Readiness::new();

        tx.enqueue_rst(Some(&mut sent), &mut readiness, 11, H2Error::ProtocolError);

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
                tx.enqueue_rst(Some(&mut sent), &mut readiness, sid, H2Error::Cancel),
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
            tx.enqueue_rst(Some(&mut sent), &mut at_capacity, 9, H2Error::Cancel),
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
    /// `MAX` is 16 rather than the production [`MIN_PENDING_RST_STREAMS`] so a
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
                            Some(&mut sent),
                            &mut readiness,
                            2 * (id as StreamId % 64) + 1,
                            H2Error::ProtocolError,
                        );
                    }
                    RstStep::Reap(n) => {
                        for _ in 0..=(n % 96) {
                            tx.enqueue_rst(
                                Some(&mut sent),
                                &mut readiness,
                                next_reaped,
                                H2Error::Cancel,
                            );
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
        let mut tx = H2ControlTx::new(100);
        let mut rst_sent = HashSet::new();
        let mut readiness = readiness();
        for id in [1, 3, 5] {
            tx.enqueue_rst(Some(&mut rst_sent), &mut readiness, id, H2Error::Cancel);
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
        let mut tx = H2ControlTx::new(100);
        let mut rst_sent = HashSet::new();
        let mut readiness = readiness();
        for id in [1, 3, 5, 7] {
            tx.enqueue_rst(Some(&mut rst_sent), &mut readiness, id, H2Error::Cancel);
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
        let mut tx = H2ControlTx::new(100);
        let mut rst_sent = HashSet::new();
        let mut readiness = readiness();
        tx.enqueue_rst(Some(&mut rst_sent), &mut readiness, 1, H2Error::Cancel);
        let mut buf = vec![0u8; RST_FRAME_SIZE - 1];

        let (bytes, frames) = tx.drain_rst_streams_into(&mut buf);

        assert_eq!((bytes, frames), (0, 0));
        assert_eq!(tx.pending().len(), 1, "nothing fit, so nothing was dropped");
    }

    #[test]
    fn an_exactly_sized_buffer_writes_the_frame() {
        let mut tx = H2ControlTx::new(100);
        let mut rst_sent = HashSet::new();
        let mut readiness = readiness();
        tx.enqueue_rst(Some(&mut rst_sent), &mut readiness, 1, H2Error::Cancel);
        let mut buf = vec![0u8; RST_FRAME_SIZE];

        let (bytes, frames) = tx.drain_rst_streams_into(&mut buf);

        assert_eq!((bytes, frames), (RST_FRAME_SIZE, 1));
        assert!(!tx.has_pending());
    }

    #[test]
    fn draining_an_empty_queue_is_a_no_op() {
        let mut tx = H2ControlTx::new(100);
        let mut buf = vec![0u8; RST_FRAME_SIZE * 4];

        assert_eq!(tx.drain_rst_streams_into(&mut buf), (0, 0));
    }

    // ── the queue bound is a bound on what is pending ───────────────────

    /// The bound limits the pending queue, not the connection's lifetime:
    /// any number of resets, drained as they come, never overflow it.
    ///
    /// TO SEE THIS RED: make [`H2ControlTx::overflowed`] return
    /// `self.total_rst_streams_queued >= self.max_pending`, the lifetime
    /// reading the bound had before. The second fill then reports an
    /// overflow that never happened.
    #[test]
    fn draining_keeps_any_number_of_resets_within_the_bound() {
        let mut tx = H2ControlTx::new(100);
        let bound = tx.max_pending();
        let mut rst_sent = HashSet::new();
        let mut readiness = readiness();
        let mut buf = vec![0u8; RST_FRAME_SIZE * bound];
        let mut next: StreamId = 1;
        for _ in 0..5 {
            for _ in 0..bound {
                assert_eq!(
                    tx.enqueue_rst(Some(&mut rst_sent), &mut readiness, next, H2Error::Cancel),
                    EnqueueRstOutcome::Queued
                );
                next += 2;
            }
            let (_, frames) = tx.drain_rst_streams_into(&mut buf);
            assert_eq!(frames, bound);
            assert!(
                !tx.overflowed(),
                "a queue drained before it overflows must never report an overflow"
            );
        }
        assert_eq!(tx.lifetime_queued(), 5 * bound);
    }

    /// The overflow is recorded by the refused insert itself, and only by it,
    /// and survives the drain that follows.
    #[test]
    fn overflow_is_set_by_a_refused_insert_and_survives_the_drain() {
        const MAX: usize = 4;
        let mut tx = H2ControlTx::with_cap(MAX);
        let mut sent: HashSet<StreamId> = HashSet::new();
        let mut readiness = readiness();

        for sid in [1u32, 3, 5, 7] {
            tx.enqueue_rst(Some(&mut sent), &mut readiness, sid, H2Error::Cancel);
        }
        assert!(
            !tx.overflowed(),
            "a queue exactly at its bound has not overflowed"
        );

        assert_eq!(
            tx.enqueue_rst(Some(&mut sent), &mut readiness, 9, H2Error::Cancel),
            EnqueueRstOutcome::Dropped
        );
        assert!(tx.overflowed(), "the refused insert records the overflow");

        let mut buf = vec![0u8; RST_FRAME_SIZE * MAX];
        tx.drain_rst_streams_into(&mut buf);
        assert!(!tx.has_pending());
        assert!(
            tx.overflowed(),
            "the overflow survives the drain: the dropped RST never reached the wire"
        );
    }

    /// The bound follows the advertised concurrency: one reaper sweep, at
    /// most one CANCEL per open stream, never fills it on its own.
    #[test]
    fn the_bound_follows_max_concurrent_streams() {
        assert_eq!(pending_rst_bound(1), MIN_PENDING_RST_STREAMS);
        assert_eq!(pending_rst_bound(100), MIN_PENDING_RST_STREAMS);
        assert_eq!(pending_rst_bound(1000), MIN_PENDING_RST_STREAMS);
        assert_eq!(pending_rst_bound(10_000), 40_000);
        assert_eq!(H2ControlTx::new(10_000).max_pending(), 40_000);
        for mcs in [1u32, 100, 250, 251, 1000, 10_000] {
            assert!(pending_rst_bound(mcs) >= mcs as usize);
        }
    }

    #[test]
    fn clearing_the_queue_does_not_rewind_the_lifetime_counter() {
        let mut tx = H2ControlTx::new(100);
        let mut rst_sent = HashSet::new();
        let mut readiness = readiness();
        for id in [1, 3, 5] {
            tx.enqueue_rst(Some(&mut rst_sent), &mut readiness, id, H2Error::Cancel);
        }

        tx.clear_pending();

        assert!(!tx.has_pending());
        assert_eq!(
            tx.lifetime_queued(),
            3,
            "the lifetime count survives a queue clear"
        );
    }
}
