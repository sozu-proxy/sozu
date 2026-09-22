//! RFC 9218 stream priority and the per-write-pass scheduling decision for
//! [`super::h2::ConnectionH2`].
//!
//! This module answers one question — **which stream writes next, and may it
//! interleave** — as a decision over state it owns, behind the same narrow,
//! closed API [`super::hpack_state::HpackState`],
//! [`super::h2_flow_control::H2FlowControl`],
//! [`super::h2_stream_table::H2StreamTable`], [`super::h2_drain::H2DrainState`]
//! and [`super::h2_header_reassembly::HeaderBlockAccumulator`] already use.
//! Every field below is private to this module; `ConnectionH2` reaches them
//! only through the methods declared here.
//!
//! It owns:
//!
//! - [`Prioriser`] — the per-stream `(urgency, incremental)` map (RFC 9218
//!   §4), its `MAX_PRIORITIES` flood cap, the idle-stream look-ahead filter
//!   for standalone PRIORITY / PRIORITY_UPDATE frames, the same-urgency
//!   incremental rotation, and the round-robin cursor. Moved here verbatim
//!   from `h2.rs`.
//! - The reusable order buffer the pass is sorted into. It used to live on
//!   `HpackState` as `priorities_buf`, which is where the first extraction
//!   step found it; it is a scheduling buffer and has nothing to do with
//!   HPACK.
//! - [`ReadyIncrementalCensus`] — one write pass's same-urgency ready-peer
//!   counts, the RFC 9218 §4 round-robin leader, and the running
//!   `incremental_count` `write_streams` traces.
//!
//! ## What it deliberately does NOT own, and why
//!
//! **Readiness.** Whether a stream has something to send this pass is
//! `kawa.is_main_phase() || (terminated && !completed) || (error &&
//! !rst_sent)` over `Context.streams[gid].{front,back}` plus
//! `H2StreamTable::rst_sent` — live stream storage this module has no
//! business holding. [`H2Scheduler::begin_pass`] therefore takes it as
//! **data**: a `FnMut(StreamId) -> bool` projection the caller computes.
//! That predicate is the whole of this module's dependency on the rest of
//! the connection — there is no `&mut Context`, no `Kawa`, no socket and no
//! clock anywhere in this file. The predicate is invoked for *incremental*
//! streams only, never for the rest, so the projection costs exactly what
//! the same test cost inline.
//!
//! **The yield itself.** `H2BlockConverter`'s DATA arm decides whether to
//! return to the scheduler after one frame: `incremental_mode &&
//! incremental_peer_count > 1 && !next_closes_stream` (LIFECYCLE.md
//! invariants 15 and 19). This module supplies the two scheduling inputs of
//! that expression, `is_incremental` and
//! [`ReadyIncrementalCensus::incremental_peer_count`], and leaves the
//! expression itself where it is.
//!
//! The `next_closes_stream` term genuinely cannot move — it reads
//! `kawa.blocks.front()`, which only the converter can see. The first two
//! conjuncts could: collapsing them into one scheduler-computed
//! `may_interleave` would leave `converter.rs`'s DATA arm reading
//! `may_interleave && !next_closes_stream`, look-ahead untouched, and would
//! move the `> 1` threshold into this module beside the count that feeds it.
//! The reason that is NOT done here is narrower and worth writing down
//! rather than inventing a principle for: this changeset's claim is that the
//! write path gained no copy, and the evidence for it is that `converter.rs`
//! is byte-identical to its parent commit. Editing its struct, its two
//! constructors and its unit tests forfeits that evidence for a tidier
//! boundary. It belongs in a changeset whose own diff can carry the
//! before/after, not in this one.
//!
//! ## Fairness, and exactly how far it reaches (LIFECYCLE.md invariant 26)
//!
//! Two halves, neither sufficient alone:
//!
//! 1. [`Prioriser::apply_incremental_rotation`] rotates each urgency
//!    bucket's incremental tail to start at the first stream id strictly
//!    greater than [`Prioriser::incremental_cursor`], wrapping.
//! 2. [`H2Scheduler::end_pass`] commits that cursor to the first incremental
//!    stream which actually consumed send window during the pass.
//!
//! A rotation that never commits re-reads the same cursor and hands the same
//! stream the lead forever; a cursor committed without the rotation is read
//! by nobody. Together they make leadership advance exactly one position per
//! pass — **in the one urgency bucket that supplied the pass's leader**.
//! There, K ready incremental peers each lead once every K passes and none
//! waits longer than K-1.
//!
//! **It reaches no further than that bucket, and this is a real limitation,
//! not a rounding error.** [`Prioriser::incremental_cursor`] is a single
//! connection-global stream id. `apply_incremental_rotation` applies it to
//! EVERY bucket's incremental tail, but `end_pass` can only ever commit an
//! id drawn from the lowest-numbered urgency bucket that had a ready
//! incremental stream fire — the pass order is ascending by urgency, so the
//! first stream to consume window comes from there. Every other bucket is
//! rotated by a cursor from a foreign id range, and when that cursor sits
//! entirely below (or entirely above) the bucket's own ids,
//! `partition_point` returns a constant and the bucket never rotates at all.
//! Measured, u=0 incremental `{1, 3}` beside u=3 incremental `{5, 7}`, all
//! ready and all consuming: u=0 alternates `1, 3, 1, 3, …` while u=3 is
//! `5, 7` in every single pass. Stream 7 never leads its bucket. That is
//! positional starvation, and it becomes byte starvation the moment a pass
//! is cut short — a `FlushOutcome::Stalled` at stream 5 means 7 writes
//! nothing, pass after pass.
//!
//! Pinned, as observed behaviour rather than as an aspiration, by
//! [`tests::the_round_robin_cursor_is_connection_global_so_only_the_leading_bucket_rotates`].
//! Making it per-bucket means `incremental_cursor: [StreamId; URGENCY_LEVELS]`,
//! which changes wire ordering on multi-bucket connections and therefore
//! needs its own changeset and its own e2e evidence; this one is a pure
//! move and deliberately does not attempt it. `fairness_property` generates
//! its distractors in strictly lower-priority buckets precisely so that its
//! main bucket is always the leader, which is the scope the invariant claims.
//!
//! `begin_pass`'s census is bucket-scoped for an unrelated reason: a
//! connection-global count makes a solo incremental stream yield to a peer
//! in an urgency bucket it can never interleave with, which is the
//! invariant-15 strand.
//!
//! ## Cost
//!
//! The pass census is a fixed `[usize; 8]` — RFC 9218 §4.1 urgency is
//! `[0, 7]` and [`Prioriser::push_priority`] clamps to it — rather than the
//! `HashMap<u8, usize>` `write_streams` used to build per pass.
//!
//! State the saving exactly, because it is conditional. `HashMap::new()`
//! allocates nothing; the FIRST `entry().or_insert()` allocates, and a fifth
//! distinct bucket allocates again on the grow. So this is one heap
//! allocation fewer on every write pass that carries at least one ready
//! incremental stream, two when five or more urgency buckets are populated,
//! and **nothing at all on a pass with no ready incremental stream** — which
//! is the common case, since RFC 9218's default is `i=0` and a connection
//! whose peers never send a `priority` header has no incremental stream to
//! count. The pass still allocates once inside `sort_by_cached_key` for two
//! or more streams, at base and here alike; that is untouched. What is
//! unconditional is an array index in place of a hash lookup at each of the
//! four sites that read or decrement the census. The
//! order buffer is moved in and out of [`H2Scheduler`] exactly as
//! `priorities_buf` was moved in and out of `HpackState`, for the same
//! borrow reason: the per-stream loop re-borrows `self.hpack`'s encoder for
//! every eligible stream, so no borrow of a connection field may span it.

use std::collections::HashMap;

use sozu_command::logging::ansi_palette;

use super::{GlobalStreamId, StreamId, parser};

/// RFC 9218 Extensible Priorities for HTTP stream scheduling.
///
/// Stores per-stream urgency (0-7, lower = more important) and incremental
/// flag. Used by `writable()` to sort streams: lower urgency first, then
/// stream ID for stability among same-urgency non-incremental streams.
///
/// Within a same-urgency bucket the scheduler (see
/// [`ConnectionH2::write_streams`]) drains non-incremental streams
/// sequentially, then applies RFC 9218 §4 round-robin to the incremental
/// streams starting from [`Self::incremental_cursor`], so multiple concurrent
/// downloads at the same urgency interleave their DATA frames fairly.
///
/// Streams without an explicit `priority` header get the RFC 9218 defaults:
/// urgency 3, incremental false.
#[derive(Default)]
pub struct Prioriser {
    /// Per-stream priority: stream_id -> (urgency 0-7, incremental flag)
    priorities: HashMap<StreamId, (u8, bool)>,
    /// RFC 9218 §4 round-robin cursor: stream ID that fired first in the
    /// last write pass over the incremental tail of the lowest-urgency
    /// bucket that contained at least one incremental stream. The next pass
    /// starts from the stream immediately after this ID (wrapping around),
    /// so a single slow-draining stream cannot hog the connection.
    ///
    /// `0` is the "no cursor yet" sentinel and means "start from the
    /// smallest ID in the bucket" — H2 stream IDs are always > 0.
    incremental_cursor: StreamId,
}

/// RFC 9218 §4 default urgency value.
const DEFAULT_URGENCY: u8 = 3;

/// Maximum entries in the priority map to prevent flooding via PRIORITY frames.
const MAX_PRIORITIES: usize = 4096;

/// Small look-ahead window (in stream IDs) for PRIORITY frames that arrive
/// slightly before the peer opens the corresponding stream. RFC 9218 allows
/// PRIORITY to be sent for an idle stream that the peer intends to open
/// soon. Past this budget we assume the ID will never be used and drop the
/// entry, preventing flooding with far-future stream IDs.
const PRIORITY_IDLE_LOOKAHEAD: u32 = 64;

impl Prioriser {
    /// Record or update the priority for a stream that we know exists or are
    /// currently processing (used from pkawa's header-handling path where the
    /// owning stream's HEADERS frame is being decoded).
    ///
    /// Returns `true` if the priority is invalid (self-dependency for RFC 7540),
    /// signalling the caller should reset the stream with a protocol error.
    pub fn push_priority(&mut self, stream_id: StreamId, priority: parser::PriorityPart) -> bool {
        trace!(
            "{} PRIORITY REQUEST FOR {}: {:?}",
            log_module_context!(),
            stream_id,
            priority
        );
        // Pre-condition: the priority map never grows past MAX_PRIORITIES.
        // The cap is the only thing standing between a PRIORITY flood and
        // unbounded memory; assert it holds on entry (each insert path below
        // either updates an existing key or is gated by this check).
        debug_assert!(
            self.priorities.len() <= MAX_PRIORITIES,
            "priority map must never exceed MAX_PRIORITIES entries"
        );
        // Cap the priority map to prevent flooding via PRIORITY frames
        if !self.priorities.contains_key(&stream_id) && self.priorities.len() >= MAX_PRIORITIES {
            return false;
        }
        match priority {
            parser::PriorityPart::Rfc7540 {
                stream_dependency,
                weight: _,
            } => {
                // RFC 9113 §5.3.1: a stream cannot depend on itself; signal
                // the caller to RST_STREAM with PROTOCOL_ERROR. Otherwise the
                // RFC 7540 priority tree is deprecated and silently ignored.
                stream_dependency.stream_id == stream_id
            }
            parser::PriorityPart::Rfc9218 {
                urgency,
                incremental,
            } => {
                // RFC 9218 §7.1: a malformed or out-of-range priority field
                // MUST be "treated as absent", NOT as a stream error. Clamping
                // an urgency > 7 to 7 is the policy-correct interpretation:
                // the field is still present (so defaulting would lose
                // information) but its value is normalised to the RFC's
                // allowed range [0..=7]. Intentionally not PROTOCOL_ERROR.
                self.priorities
                    .insert(stream_id, (urgency.min(7), incremental));
                // Post-conditions: the entry now exists with a clamped urgency
                // in [0, 7] (the writable scheduler buckets by urgency and would
                // mis-order on a value above 7), and the map stays within its
                // memory cap.
                debug_assert!(
                    self.priorities
                        .get(&stream_id)
                        .is_some_and(|(u, _)| *u <= 7),
                    "stored RFC 9218 urgency must be clamped to [0, 7]"
                );
                debug_assert!(
                    self.priorities.len() <= MAX_PRIORITIES,
                    "priority map must stay within MAX_PRIORITIES after insert"
                );
                false
            }
        }
    }

    /// Record or update the priority for a stream ID that arrived via a
    /// standalone PRIORITY frame.
    ///
    /// Pass 3 Medium #4: without this guard, a peer could send PRIORITY for
    /// arbitrary stream IDs (e.g. 2^31 ever-increasing IDs) and pin up to
    /// `MAX_PRIORITIES` entries of memory. Accept only:
    /// - an ID that corresponds to a currently-open stream (`open_streams`);
    /// - an idle ID slightly ahead of `last_stream_id` (within
    ///   [`PRIORITY_IDLE_LOOKAHEAD`]), matching RFC 9218's "set priority for
    ///   a stream about to be opened" pattern.
    ///
    /// IDs in the past that we do not currently track (already closed) and
    /// IDs too far in the future are silently dropped. The `MAX_PRIORITIES`
    /// ceiling is preserved as a defensive backstop if both filters are ever
    /// circumvented.
    ///
    /// Returns the same value semantics as [`Self::push_priority`].
    pub fn push_priority_guarded(
        &mut self,
        stream_id: StreamId,
        priority: parser::PriorityPart,
        last_stream_id: StreamId,
        open_streams: &HashMap<StreamId, GlobalStreamId>,
    ) -> bool {
        if !self.is_acceptable(stream_id, last_stream_id, open_streams) {
            trace!(
                "{} PRIORITY dropped for unknown/far stream {} (last_stream_id={})",
                log_module_context!(),
                stream_id,
                last_stream_id
            );
            return false;
        }
        self.push_priority(stream_id, priority)
    }

    fn is_acceptable(
        &self,
        stream_id: StreamId,
        last_stream_id: StreamId,
        open_streams: &HashMap<StreamId, GlobalStreamId>,
    ) -> bool {
        if open_streams.contains_key(&stream_id) {
            return true;
        }
        // Idle stream ahead of the current counter: accept a small look-ahead.
        // Past IDs that are NOT in `open_streams` are closed — drop them.
        let upper = last_stream_id.saturating_add(PRIORITY_IDLE_LOOKAHEAD);
        stream_id > last_stream_id && stream_id <= upper
    }

    /// Remove a stream's priority entry (called when the stream is recycled).
    pub fn remove(&mut self, stream_id: &StreamId) {
        let had = self.priorities.contains_key(stream_id);
        let before = self.priorities.len();
        self.priorities.remove(stream_id);
        // Post-conditions: the entry is truly gone, and the map shrinks by
        // exactly one iff it was present. A leak here re-introduces the
        // PRIORITY-flood memory exposure the cap defends against.
        debug_assert!(
            !self.priorities.contains_key(stream_id),
            "remove must evict the priority entry"
        );
        debug_assert_eq!(
            self.priorities.len(),
            before - had as usize,
            "priority map length drops by exactly one iff the id was present"
        );
    }

    /// Look up the priority for a stream, returning RFC 9218 defaults if absent.
    #[inline]
    pub fn get(&self, stream_id: &StreamId) -> (u8, bool) {
        self.priorities
            .get(stream_id)
            .copied()
            .unwrap_or((DEFAULT_URGENCY, false))
    }

    /// Reorder a pre-sorted slice of writable stream IDs so that inside each
    /// urgency bucket, incremental streams appear after non-incremental ones,
    /// and the incremental tail is rotated by [`Self::incremental_cursor`]
    /// (RFC 9218 §4).
    ///
    /// The input `buf` must already be sorted by `(urgency, stream_id)`:
    /// this routine only partitions and rotates inside same-urgency
    /// contiguous runs, it does not re-sort.
    ///
    /// Returns the total number of incremental streams seen, so callers that
    /// need to update the cursor at the end of the write pass can early-exit
    /// when the count is zero.
    pub fn apply_incremental_rotation(&self, buf: &mut [StreamId]) -> usize {
        // Pre-condition: callers must hand a slice already sorted by urgency so
        // same-urgency runs are contiguous (this routine only partitions/rotates
        // within a run, it does not re-sort across urgencies). A non-monotonic
        // urgency sequence would split one logical bucket into several and
        // mis-schedule the round-robin. `windows(2)` over a slice of size N is
        // dead code in release.
        #[cfg(debug_assertions)]
        debug_assert!(
            buf.windows(2)
                .all(|w| self.get(&w[0]).0 <= self.get(&w[1]).0),
            "apply_incremental_rotation requires input pre-sorted by urgency"
        );
        let len_before = buf.len();
        #[cfg(debug_assertions)]
        let expected_incremental = buf.iter().filter(|id| self.get(id).1).count();
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
            // `buf[i..j]` is a contiguous run of same-urgency stream IDs.
            let bucket = &mut buf[i..j];
            if bucket.len() > 1 {
                // Stable partition: non-incremental first, incremental last,
                // each subrange staying in ascending stream-id order.
                bucket.sort_by_key(|id| self.get(id).1);
                let split = bucket.partition_point(|id| !self.get(id).1);
                let incremental_tail = &mut bucket[split..];
                if incremental_tail.len() > 1 {
                    // Rotate so the pass starts right after the stream that
                    // fired first previously. `partition_point` returns the
                    // first index whose stream ID > cursor (so cursor itself
                    // is still drained, but after the streams ahead of it).
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
        // Post-conditions: the routine is a permutation — it reorders in place
        // and never drops a stream id (len unchanged), and the returned count is
        // exactly the number of incremental streams present (the cursor-advance
        // callers rely on this being the true incremental-tail size).
        debug_assert_eq!(
            buf.len(),
            len_before,
            "rotation must preserve the slice (no streams dropped or added)"
        );
        #[cfg(debug_assertions)]
        debug_assert_eq!(
            total_incremental, expected_incremental,
            "reported incremental count must equal the incremental streams in buf"
        );
        total_incremental
    }

    /// Advance the RFC 9218 §4 round-robin cursor after a write pass.
    ///
    /// `first_incremental_fired` is the stream ID that headed the incremental
    /// tail we just drained; the next pass will start at the next stream
    /// after that ID. Callers may pass `None` when no incremental streams
    /// were eligible, leaving the cursor where it was.
    pub fn advance_incremental_cursor(&mut self, first_incremental_fired: Option<StreamId>) {
        if let Some(id) = first_incremental_fired {
            self.incremental_cursor = id;
        }
    }
}

/// RFC 9218 §4.1 highest urgency value. [`Prioriser::push_priority`] clamps
/// every advertised urgency into `[0, MAX_URGENCY]` and
/// [`Prioriser::get`] defaults to [`DEFAULT_URGENCY`], so no urgency outside
/// that range can reach the scheduler — which is what lets the per-pass
/// census be a fixed-size array rather than a map.
const MAX_URGENCY: u8 = 7;

/// Number of distinct RFC 9218 urgency buckets, `MAX_URGENCY + 1`.
const URGENCY_LEVELS: usize = MAX_URGENCY as usize + 1;

/// Index of `urgency`'s bucket in a [`ReadyIncrementalCensus`]. The clamp is
/// defence in depth behind [`Prioriser::push_priority`]'s own: an urgency
/// above [`MAX_URGENCY`] cannot be produced by any path in this module, and
/// folding one into the last bucket is still preferable to a panic on the
/// write hot path.
#[inline]
fn bucket(urgency: u8) -> usize {
    debug_assert!(
        urgency <= MAX_URGENCY,
        "urgency must be clamped to [0, MAX_URGENCY] before it reaches the census"
    );
    usize::from(urgency.min(MAX_URGENCY))
}

/// One write pass's live scheduling census: how many *ready* incremental
/// streams share each urgency bucket, which incremental stream led the pass,
/// and how many incremental streams the rotation saw in total.
///
/// Built by [`H2Scheduler::begin_pass`], kept live across the per-stream loop
/// by [`Self::note_ineligible`] / [`Self::note_fired`], and retired by
/// [`H2Scheduler::end_pass`]. It holds no reference to anything: every method
/// here is a decision over the counts it owns.
pub(super) struct ReadyIncrementalCensus {
    /// Ready incremental streams per urgency bucket — LIFECYCLE.md invariant
    /// 17's "bucket-scoped, ready-only, kept live mid-pass". A bucket with no
    /// ready incremental stream reads `0`, which is what the `HashMap` this
    /// replaced expressed as an absent key.
    ready: [usize; URGENCY_LEVELS],
    /// Every incremental stream the rotation placed this pass, ready or not.
    /// Reported by `write_streams`'s `PRIORITIES` trace line and by nothing
    /// else; deliberately NOT `ready.iter().sum()`.
    incremental_count: usize,
    /// RFC 9218 §4 round-robin leader: the first incremental stream that
    /// actually consumed send window this pass. [`H2Scheduler::end_pass`]
    /// commits it as the next pass's cursor; `None` leaves the cursor alone.
    first_incremental_fired: Option<StreamId>,
}

impl ReadyIncrementalCensus {
    /// Ready incremental peers sharing `urgency` this pass — the value
    /// `H2BlockConverter::incremental_peer_count` is set from.
    ///
    /// `<= 1` means there is nobody in this bucket to interleave with, and
    /// the converter must then NOT yield after a DATA frame: a clean yield
    /// sets no `expect_write`, `finalize_write` strips `Ready::WRITABLE`, and
    /// edge-triggered epoll never re-fires for a sozu-owned buffer
    /// (LIFECYCLE.md invariants 15 and 16).
    pub(super) fn incremental_peer_count(&self, urgency: u8) -> usize {
        self.ready[bucket(urgency)]
    }

    /// LIFECYCLE.md invariant 17: a stream that becomes ineligible part-way
    /// through a pass leaves its bucket immediately, so the streams after it
    /// read the live count rather than the entry snapshot. Missing one costs
    /// a voluntary yield per same-urgency peer trailing the transition.
    ///
    /// A non-incremental stream was never counted, so it never decrements;
    /// a bucket already at zero saturates instead of wrapping.
    pub(super) fn note_ineligible(&mut self, urgency: u8, is_incremental: bool) {
        if !is_incremental {
            return;
        }
        let slot = &mut self.ready[bucket(urgency)];
        *slot = slot.saturating_sub(1);
    }

    /// RFC 9218 §4: record the pass's round-robin leader — the FIRST
    /// incremental stream that moved bytes.
    ///
    /// `consumed <= 0` is not a lead. A stream that reached the converter and
    /// emitted nothing (its send window was exhausted) must not advance the
    /// cursor past the peers that are still waiting for their turn, or a
    /// permanently window-blocked stream would hand the lead onward every
    /// pass while never using it.
    pub(super) fn note_fired(&mut self, stream_id: StreamId, is_incremental: bool, consumed: i32) {
        if is_incremental && consumed > 0 && self.first_incremental_fired.is_none() {
            self.first_incremental_fired = Some(stream_id);
        }
    }

    /// Ready incremental streams across every bucket — the sample
    /// `ConnectionH2::gauge_connection_state` publishes as the
    /// `h2.streams.ready_incremental.by_urgency` aggregate.
    pub(super) fn ready_total(&self) -> usize {
        self.ready.iter().sum()
    }

    /// Every incremental stream this pass placed, ready or not. Trace only.
    pub(super) fn incremental_count(&self) -> usize {
        self.incremental_count
    }

    /// The per-urgency counts, for `write_streams`'s `PRIORITIES` trace line.
    pub(super) fn ready_buckets(&self) -> &[usize; URGENCY_LEVELS] {
        &self.ready
    }
}

/// RFC 9218 priority state plus the reusable buffer one write pass is
/// ordered into. See the module doc for the boundary and the fairness
/// argument.
#[derive(Default)]
pub(super) struct H2Scheduler {
    prioriser: Prioriser,
    /// Reusable buffer the pass order is built in. Moved out by
    /// [`Self::begin_pass`] and back by [`Self::end_pass`] rather than
    /// borrowed in place: the per-stream loop in
    /// `ConnectionH2::write_streams` re-borrows the HPACK encoder for every
    /// eligible stream's `kawa.prepare`, so no borrow of a connection field
    /// may span it. This is the same move `HpackState::take_priorities_buf`
    /// performed before the buffer was recognised as scheduling state.
    order: Vec<StreamId>,
}

impl H2Scheduler {
    /// RFC 9218 `(urgency, incremental)` for `stream_id`, defaulting to
    /// `(DEFAULT_URGENCY, false)` for a stream that advertised nothing.
    #[inline]
    pub(super) fn priority(&self, stream_id: &StreamId) -> (u8, bool) {
        self.prioriser.get(stream_id)
    }

    /// Record a priority for a stream whose own HEADERS block is being
    /// decoded. Returns `true` on an RFC 7540 self-dependency, which the
    /// caller answers with RST_STREAM(PROTOCOL_ERROR).
    pub(super) fn push_priority(
        &mut self,
        stream_id: StreamId,
        priority: parser::PriorityPart,
    ) -> bool {
        self.prioriser.push_priority(stream_id, priority)
    }

    /// Record a priority that arrived on a standalone PRIORITY or
    /// PRIORITY_UPDATE frame, behind the open-stream / idle-look-ahead
    /// filter that bounds a PRIORITY flood (LIFECYCLE.md invariant 18).
    pub(super) fn push_priority_guarded(
        &mut self,
        stream_id: StreamId,
        priority: parser::PriorityPart,
        last_stream_id: StreamId,
        open_streams: &HashMap<StreamId, GlobalStreamId>,
    ) -> bool {
        self.prioriser
            .push_priority_guarded(stream_id, priority, last_stream_id, open_streams)
    }

    /// Evict a retired stream's priority entry. Called from every stream
    /// removal site; leaking one re-opens the PRIORITY-flood memory exposure
    /// `MAX_PRIORITIES` exists to bound.
    pub(super) fn remove_stream(&mut self, stream_id: &StreamId) {
        self.prioriser.remove(stream_id);
    }

    /// The raw [`Prioriser`], for `pkawa::handle_header`, which records a
    /// `priority` request header while decoding the block and has no other
    /// scheduler business.
    pub(super) fn prioriser_mut(&mut self) -> &mut Prioriser {
        &mut self.prioriser
    }

    /// Order one write pass and take its ready-incremental census.
    ///
    /// The order is RFC 9218 §4: ascending urgency, then ascending stream id
    /// for stability, then — inside each urgency bucket — non-incremental
    /// streams before incremental ones, with the incremental tail rotated to
    /// start after [`Prioriser::incremental_cursor`].
    ///
    /// `ready` is the caller's projection of the one fact this module does
    /// not own: whether a stream has something to send this pass. It is
    /// called **only for incremental streams**, because only they can be
    /// counted as an interleaving peer — the same streams the inline census
    /// this replaced evaluated, in the same order, so the projection costs
    /// exactly what it cost before.
    ///
    /// Pair every call with [`Self::end_pass`]: until then the scheduler
    /// holds an empty order buffer.
    pub(super) fn begin_pass<I, F>(
        &mut self,
        stream_ids: I,
        mut ready: F,
    ) -> (Vec<StreamId>, ReadyIncrementalCensus)
    where
        I: IntoIterator<Item = StreamId>,
        F: FnMut(StreamId) -> bool,
    {
        let mut order = std::mem::take(&mut self.order);
        order.clear();
        order.extend(stream_ids);
        // RFC 9218 §4 primary sort: ascending urgency, then stream ID for
        // stability. The incremental flag is handled by
        // `apply_incremental_rotation` below so it does not perturb the
        // non-incremental fast path.
        order.sort_by_cached_key(|id| {
            let (urgency, _) = self.prioriser.get(id);
            (urgency, *id)
        });
        // RFC 9218 §4: inside each urgency bucket, move incremental streams
        // to the tail and rotate them by the per-connection round-robin
        // cursor so no single slow-draining stream can starve its
        // same-urgency incremental peers.
        let incremental_count = self.prioriser.apply_incremental_rotation(&mut order);

        // The connection-global `incremental_count` is too coarse for the
        // converter's yield decision: a solo `u=0, i` stream with an
        // unrelated `u=7, i` peer in a different bucket would see a peer
        // count above 1 and voluntarily yield, stranding bytes the
        // invariant-15/16 guards exist to prevent. Scope the count to
        // same-urgency streams that are actually ready to emit this pass.
        let mut census = ReadyIncrementalCensus {
            ready: [0; URGENCY_LEVELS],
            incremental_count,
            first_incremental_fired: None,
        };
        for &stream_id in order.iter() {
            let (urgency, is_incremental) = self.prioriser.get(&stream_id);
            if !is_incremental {
                continue;
            }
            if ready(stream_id) {
                census.ready[bucket(urgency)] += 1;
            }
        }
        (order, census)
    }

    /// End a write pass: commit the RFC 9218 §4 round-robin cursor to the
    /// stream that led it, and take the order buffer back.
    ///
    /// This is the second half of LIFECYCLE.md invariant 26. Committing the
    /// cursor is what makes the next pass's rotation start one position
    /// further on; without it `begin_pass` re-reads the same cursor and the
    /// same stream leads every pass forever, which is starvation of every
    /// other incremental peer in the bucket.
    ///
    /// `write_streams` calls this at exactly the point it used to call
    /// `HpackState::put_priorities_buf` and
    /// `Prioriser::advance_incremental_cursor` — after the deferred
    /// RST accounting, so a MadeYouReset cap trip that returns a GOAWAY
    /// early still skips both, exactly as before.
    pub(super) fn end_pass(&mut self, order: Vec<StreamId>, census: ReadyIncrementalCensus) {
        self.order = order;
        self.prioriser
            .advance_incremental_cursor(census.first_incremental_fired);
    }

    /// Quiet-time reclaim of the order buffer once it holds 4x `retain_size`,
    /// mirroring `HpackState::reclaim_idle_buffers` for the scratch buffers
    /// that stayed there. Called from `cancel_timed_out_streams`.
    pub(super) fn reclaim_idle_buffer(&mut self, retain_size: usize) {
        if self.order.capacity() > retain_size * 4 {
            self.order.shrink_to(retain_size);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Prioriser ────────────────────────────────────────────────────────

    #[test]
    fn test_prioriser_defaults_for_unknown_stream() {
        let p = Prioriser::default();
        // Unknown stream -> RFC 9218 defaults: urgency 3, incremental false
        assert_eq!(p.get(&1), (3, false));
        assert_eq!(p.get(&999), (3, false));
    }

    #[test]
    fn test_prioriser_push_rfc9218_and_get() {
        let mut p = Prioriser::default();

        let invalid = p.push_priority(
            1,
            parser::PriorityPart::Rfc9218 {
                urgency: 0,
                incremental: true,
            },
        );
        assert!(!invalid);
        assert_eq!(p.get(&1), (0, true));

        let invalid = p.push_priority(
            3,
            parser::PriorityPart::Rfc9218 {
                urgency: 7,
                incremental: false,
            },
        );
        assert!(!invalid);
        assert_eq!(p.get(&3), (7, false));
    }

    #[test]
    fn test_prioriser_urgency_clamped_to_7() {
        let mut p = Prioriser::default();

        p.push_priority(
            1,
            parser::PriorityPart::Rfc9218 {
                urgency: 255,
                incremental: false,
            },
        );
        assert_eq!(p.get(&1), (7, false));
    }

    #[test]
    fn test_prioriser_update_priority() {
        let mut p = Prioriser::default();

        p.push_priority(
            1,
            parser::PriorityPart::Rfc9218 {
                urgency: 3,
                incremental: false,
            },
        );
        assert_eq!(p.get(&1), (3, false));

        // Update same stream
        p.push_priority(
            1,
            parser::PriorityPart::Rfc9218 {
                urgency: 1,
                incremental: true,
            },
        );
        assert_eq!(p.get(&1), (1, true));
    }

    #[test]
    fn test_prioriser_remove() {
        let mut p = Prioriser::default();

        p.push_priority(
            1,
            parser::PriorityPart::Rfc9218 {
                urgency: 0,
                incremental: true,
            },
        );
        assert_eq!(p.get(&1), (0, true));

        p.remove(&1);
        // After removal, falls back to defaults
        assert_eq!(p.get(&1), (3, false));
    }

    #[test]
    fn test_prioriser_rfc7540_self_dependency() {
        let mut p = Prioriser::default();

        // Self-dependency should return true (invalid)
        let invalid = p.push_priority(
            5,
            parser::PriorityPart::Rfc7540 {
                stream_dependency: parser::StreamDependency {
                    exclusive: false,
                    stream_id: 5, // same as stream_id
                },
                weight: 16,
            },
        );
        assert!(invalid);
    }

    #[test]
    fn test_prioriser_rfc7540_valid_dependency() {
        let mut p = Prioriser::default();

        // Non-self dependency is valid (but ignored for scheduling)
        let invalid = p.push_priority(
            5,
            parser::PriorityPart::Rfc7540 {
                stream_dependency: parser::StreamDependency {
                    exclusive: false,
                    stream_id: 3, // different stream
                },
                weight: 16,
            },
        );
        assert!(!invalid);
        // Still returns defaults since RFC 7540 priority is ignored
        assert_eq!(p.get(&5), (3, false));
    }

    #[test]
    fn test_prioriser_max_entries_cap() {
        let mut p = Prioriser::default();

        // Fill up to MAX_PRIORITIES
        for i in 0..MAX_PRIORITIES as u32 {
            let stream_id = i * 2 + 1; // odd stream IDs
            p.push_priority(
                stream_id,
                parser::PriorityPart::Rfc9218 {
                    urgency: (i % 8) as u8,
                    incremental: false,
                },
            );
        }

        // Next insert for a new stream should be silently rejected
        let next_id = (MAX_PRIORITIES as u32) * 2 + 1;
        let invalid = p.push_priority(
            next_id,
            parser::PriorityPart::Rfc9218 {
                urgency: 0,
                incremental: true,
            },
        );
        assert!(!invalid); // not a protocol error, just silently dropped
        assert_eq!(p.get(&next_id), (3, false)); // defaults, not stored
    }

    #[test]
    fn test_prioriser_update_existing_at_cap() {
        let mut p = Prioriser::default();

        // Fill to cap
        for i in 0..MAX_PRIORITIES as u32 {
            p.push_priority(
                i * 2 + 1,
                parser::PriorityPart::Rfc9218 {
                    urgency: 3,
                    incremental: false,
                },
            );
        }

        // Updating an existing entry should still work even at cap
        p.push_priority(
            1,
            parser::PriorityPart::Rfc9218 {
                urgency: 0,
                incremental: true,
            },
        );
        assert_eq!(p.get(&1), (0, true));
    }

    #[test]
    fn test_prioriser_guarded_accepts_open_stream() {
        let mut p = Prioriser::default();
        let mut open: HashMap<StreamId, GlobalStreamId> = HashMap::new();
        open.insert(3, 0);
        let invalid = p.push_priority_guarded(
            3,
            parser::PriorityPart::Rfc9218 {
                urgency: 1,
                incremental: false,
            },
            7,
            &open,
        );
        assert!(!invalid);
        assert_eq!(p.get(&3), (1, false));
    }

    #[test]
    fn test_prioriser_guarded_accepts_idle_lookahead() {
        let mut p = Prioriser::default();
        let open: HashMap<StreamId, GlobalStreamId> = HashMap::new();
        // Just ahead of last_stream_id, within PRIORITY_IDLE_LOOKAHEAD.
        let invalid = p.push_priority_guarded(
            105,
            parser::PriorityPart::Rfc9218 {
                urgency: 2,
                incremental: true,
            },
            99,
            &open,
        );
        assert!(!invalid);
        assert_eq!(p.get(&105), (2, true));
    }

    #[test]
    fn test_prioriser_guarded_drops_far_future_stream() {
        let mut p = Prioriser::default();
        let open: HashMap<StreamId, GlobalStreamId> = HashMap::new();
        // Beyond the 64-slot lookahead window.
        let invalid = p.push_priority_guarded(
            1_000_001,
            parser::PriorityPart::Rfc9218 {
                urgency: 0,
                incremental: false,
            },
            3,
            &open,
        );
        assert!(!invalid); // not a protocol error, just dropped
        // Default priority returned — no entry stored.
        assert_eq!(p.get(&1_000_001), (DEFAULT_URGENCY, false));
    }

    #[test]
    fn test_prioriser_guarded_drops_closed_past_stream() {
        let mut p = Prioriser::default();
        let open: HashMap<StreamId, GlobalStreamId> = HashMap::new();
        // Past the counter and not open = already closed. Drop.
        let invalid = p.push_priority_guarded(
            3,
            parser::PriorityPart::Rfc9218 {
                urgency: 5,
                incremental: false,
            },
            99,
            &open,
        );
        assert!(!invalid);
        assert_eq!(p.get(&3), (DEFAULT_URGENCY, false));
    }

    #[test]
    fn test_prioriser_guarded_cannot_flood_with_far_ids() {
        // Previously an attacker could pack MAX_PRIORITIES entries by picking
        // far-future stream IDs. The guard rejects them before the cap helps.
        let mut p = Prioriser::default();
        let open: HashMap<StreamId, GlobalStreamId> = HashMap::new();
        for delta in 10_000..(10_000 + MAX_PRIORITIES as u32) {
            p.push_priority_guarded(
                delta,
                parser::PriorityPart::Rfc9218 {
                    urgency: 0,
                    incremental: false,
                },
                0,
                &open,
            );
        }
        assert_eq!(p.priorities.len(), 0);
    }

    // ── RFC 9218 §4 round-robin rotation ───────────────────────────────

    /// Helper: mark `stream_id` as (urgency, incremental) in the map.
    fn set_prio(p: &mut Prioriser, stream_id: StreamId, urgency: u8, incremental: bool) {
        p.push_priority(
            stream_id,
            parser::PriorityPart::Rfc9218 {
                urgency,
                incremental,
            },
        );
    }

    #[test]
    fn test_apply_incremental_rotation_all_non_incremental_is_noop() {
        // Non-incremental streams keep the existing (urgency, stream_id) sort.
        let mut p = Prioriser::default();
        set_prio(&mut p, 1, 3, false);
        set_prio(&mut p, 3, 3, false);
        set_prio(&mut p, 5, 3, false);

        let mut buf = vec![1u32, 3, 5];
        let count = p.apply_incremental_rotation(&mut buf);
        assert_eq!(count, 0);
        assert_eq!(buf, vec![1, 3, 5]);
    }

    #[test]
    fn test_apply_incremental_rotation_moves_incremental_to_tail() {
        // Within a same-urgency bucket non-incremental must come before
        // incremental, each subrange staying ascending.
        let mut p = Prioriser::default();
        set_prio(&mut p, 1, 3, true);
        set_prio(&mut p, 3, 3, false);
        set_prio(&mut p, 5, 3, true);
        set_prio(&mut p, 7, 3, false);

        let mut buf = vec![1u32, 3, 5, 7];
        let count = p.apply_incremental_rotation(&mut buf);
        assert_eq!(count, 2);
        // Non-incremental first (3, 7), then incremental (1, 5) — ascending
        // within each subrange before the cursor rotation.
        assert_eq!(buf, vec![3, 7, 1, 5]);
    }

    #[test]
    fn test_apply_incremental_rotation_respects_urgency_buckets() {
        // Different urgency buckets must not be mixed.
        let mut p = Prioriser::default();
        set_prio(&mut p, 1, 0, true); // urgent incremental
        set_prio(&mut p, 3, 3, false); // default non-incremental
        set_prio(&mut p, 5, 3, true); // default incremental
        set_prio(&mut p, 7, 5, false); // low-priority non-incremental

        // Input is pre-sorted by (urgency, id) as the scheduler does.
        let mut buf = vec![1u32, 3, 5, 7];
        let count = p.apply_incremental_rotation(&mut buf);
        assert_eq!(count, 2);
        // Bucket 0: [1] (alone, stays). Bucket 3: [3] non-inc, [5] inc.
        // Bucket 5: [7] alone. Cross-bucket order is preserved.
        assert_eq!(buf, vec![1, 3, 5, 7]);
    }

    #[test]
    fn test_apply_incremental_rotation_rotates_by_cursor() {
        // Three same-urgency incremental streams: cursor advancement shifts
        // the bucket so the next pass starts after the previously fired ID.
        let mut p = Prioriser::default();
        set_prio(&mut p, 1, 3, true);
        set_prio(&mut p, 3, 3, true);
        set_prio(&mut p, 5, 3, true);

        let base = vec![1u32, 3, 5];

        // Pass 1: cursor is 0 (initial), so order stays 1, 3, 5.
        let mut buf = base.clone();
        assert_eq!(p.apply_incremental_rotation(&mut buf), 3);
        assert_eq!(buf, vec![1, 3, 5]);
        p.advance_incremental_cursor(Some(1));

        // Pass 2: cursor is 1, rotate so 3 comes first.
        let mut buf = base.clone();
        assert_eq!(p.apply_incremental_rotation(&mut buf), 3);
        assert_eq!(buf, vec![3, 5, 1]);
        p.advance_incremental_cursor(Some(3));

        // Pass 3: cursor is 3, rotate so 5 comes first.
        let mut buf = base.clone();
        assert_eq!(p.apply_incremental_rotation(&mut buf), 3);
        assert_eq!(buf, vec![5, 1, 3]);
        p.advance_incremental_cursor(Some(5));

        // Pass 4: cursor is 5 (largest in bucket), wrap to 1.
        let mut buf = base;
        assert_eq!(p.apply_incremental_rotation(&mut buf), 3);
        assert_eq!(buf, vec![1, 3, 5]);
    }

    #[test]
    fn test_apply_incremental_rotation_cursor_unknown_id() {
        // Cursor points at an ID no longer active (stream completed). Rotation
        // should still start from the smallest ID greater than the cursor.
        let mut p = Prioriser::default();
        set_prio(&mut p, 3, 3, true);
        set_prio(&mut p, 5, 3, true);
        set_prio(&mut p, 7, 3, true);
        p.advance_incremental_cursor(Some(4)); // 4 is not in the bucket

        let mut buf = vec![3u32, 5, 7];
        assert_eq!(p.apply_incremental_rotation(&mut buf), 3);
        assert_eq!(buf, vec![5, 7, 3]);
    }

    #[test]
    fn test_apply_incremental_rotation_single_stream_buckets() {
        // Single-stream buckets are a degenerate fast path: no reordering.
        let mut p = Prioriser::default();
        set_prio(&mut p, 1, 1, true);
        set_prio(&mut p, 3, 2, false);
        set_prio(&mut p, 5, 3, true);

        let mut buf = vec![1u32, 3, 5];
        let count = p.apply_incremental_rotation(&mut buf);
        assert_eq!(count, 2);
        assert_eq!(buf, vec![1, 3, 5]);
    }

    #[test]
    fn test_advance_incremental_cursor_none_is_noop() {
        // If no incremental stream fires (only non-incremental served), the
        // cursor must stay put so fairness is preserved for the next pass.
        let mut p = Prioriser::default();
        p.advance_incremental_cursor(Some(5));
        p.advance_incremental_cursor(None);
        assert_eq!(p.incremental_cursor, 5);
    }

    #[test]
    fn test_apply_incremental_rotation_mixed_bucket_with_cursor() {
        // Same-urgency bucket with a mix: non-inc served first in ascending
        // order, then the incremental tail rotated by cursor.
        let mut p = Prioriser::default();
        set_prio(&mut p, 1, 3, true);
        set_prio(&mut p, 3, 3, false);
        set_prio(&mut p, 5, 3, true);
        set_prio(&mut p, 7, 3, false);
        set_prio(&mut p, 9, 3, true);
        p.advance_incremental_cursor(Some(5));

        let mut buf = vec![1u32, 3, 5, 7, 9];
        let count = p.apply_incremental_rotation(&mut buf);
        assert_eq!(count, 3);
        // Non-inc (3, 7) first, then incremental rotated: cursor 5 means
        // next-after-5 = 9, then 1, then 5 (wrap).
        assert_eq!(buf, vec![3, 7, 9, 1, 5]);
    }

    // The three scalar `ready_incremental_bucket_decrement_*` tests that used
    // to live in `h2.rs` arrived with this extraction and were rewritten
    // below to drive `ReadyIncrementalCensus`'s own methods. In `h2.rs` they
    // re-implemented the `saturating_sub` and the bucket lookup inline in the
    // test body, over a `HashMap` the test built itself — a copy of the
    // scheduler's arithmetic, which could never catch the scheduler getting
    // that arithmetic wrong.

    // ── Pass ordering and the RFC 9218 §4 round-robin ───────────────────

    /// A scheduler whose priority map is exactly `entries`, each
    /// `(stream_id, urgency, incremental)`.
    fn scheduler_with(entries: &[(StreamId, u8, bool)]) -> H2Scheduler {
        let mut scheduler = H2Scheduler::default();
        for &(stream_id, urgency, incremental) in entries {
            scheduler.push_priority(
                stream_id,
                parser::PriorityPart::Rfc9218 {
                    urgency,
                    incremental,
                },
            );
        }
        scheduler
    }

    /// Drive `passes` write passes over `ids` with every stream ready and
    /// every incremental stream consuming window, and return the RFC 9218 §4
    /// leader each pass committed — exactly the sequence
    /// `ConnectionH2::write_streams` produces when nothing stalls.
    fn drive_leaders(
        scheduler: &mut H2Scheduler,
        ids: &[StreamId],
        passes: usize,
    ) -> Vec<Option<StreamId>> {
        let mut leaders = Vec::with_capacity(passes);
        for _ in 0..passes {
            let (order, mut census) = scheduler.begin_pass(ids.iter().copied(), |_| true);
            for &stream_id in &order {
                let (_, is_incremental) = scheduler.priority(&stream_id);
                census.note_fired(stream_id, is_incremental, 1);
            }
            leaders.push(census.first_incremental_fired);
            scheduler.end_pass(order, census);
        }
        leaders
    }

    #[test]
    fn pass_order_is_ascending_urgency_then_id_with_incrementals_at_the_bucket_tail() {
        let mut scheduler = scheduler_with(&[
            (1, 3, true),
            (3, 0, false),
            (5, 3, false),
            (7, 0, true),
            (9, 3, false),
        ]);
        let (order, _) = scheduler.begin_pass([9u32, 1, 7, 5, 3], |_| true);
        assert_eq!(
            order,
            vec![3, 7, 5, 9, 1],
            "urgency 0 before urgency 3; inside each bucket non-incremental \
             first in ascending id order, then the incremental tail"
        );
    }

    /// LIFECYCLE.md invariant 26, first half. `apply_incremental_rotation`
    /// starts the incremental tail at the first id strictly greater than the
    /// cursor, and `end_pass` commits the cursor to the stream that led. One
    /// position per pass is what bounds any stream's wait at K-1 passes.
    ///
    /// TO SEE THIS RED: in [`Prioriser::apply_incremental_rotation`], change
    /// `partition_point(|id| *id <= self.incremental_cursor)` to `*id <`.
    /// The tail then never rotates past the cursor it just committed and
    /// stream 1 leads forever:
    /// `assertion `left == right` failed: leadership must advance exactly one`
    /// `position per pass and wrap` /
    /// `left: [Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1)]`.
    /// The same mutation reds seven tests in all — this one,
    /// `no_incremental_peer_is_starved_over_a_full_cycle`,
    /// `a_pass_with_no_incremental_progress_leaves_the_cursor_alone`,
    /// `the_round_robin_cursor_is_connection_global_so_only_the_leading_bucket_rotates`,
    /// `test_apply_incremental_rotation_rotates_by_cursor`,
    /// `test_apply_incremental_rotation_mixed_bucket_with_cursor` and
    /// `fairness_property::qc_incremental_leadership_visits_every_peer_once_per_cycle`
    /// — because it breaks the rotation primitive the moved unit tests
    /// already pinned as well as the composition this step adds.
    #[test]
    fn incremental_leadership_rotates_one_position_per_pass() {
        let mut scheduler =
            scheduler_with(&[(1, 3, true), (3, 3, true), (5, 3, true), (7, 3, true)]);
        let leaders = drive_leaders(&mut scheduler, &[1, 3, 5, 7], 8);
        assert_eq!(
            leaders,
            vec![
                Some(1),
                Some(3),
                Some(5),
                Some(7),
                Some(1),
                Some(3),
                Some(5),
                Some(7)
            ],
            "leadership must advance exactly one position per pass and wrap"
        );
    }

    /// The same invariant stated as the starvation bound the refactor exists
    /// to protect: over K passes every one of K same-urgency incremental
    /// peers leads once, so none waits longer than K-1 passes.
    ///
    /// Deliberately K=5 rather than 2: a rotation bug that swaps a pair
    /// still looks fair on two streams and starves the fifth.
    ///
    /// TO SEE THIS RED: delete the `self.prioriser.advance_incremental_cursor(
    /// census.first_incremental_fired);` statement from
    /// [`H2Scheduler::end_pass`]. The cursor stays at its `0` sentinel, every
    /// pass re-reads the same order, and this fails with
    /// `stream 1 must lead 2 of 10 passes, led 10: [Some(1), Some(1), ...]` /
    /// `left: 10` / `right: 2`.
    #[test]
    fn no_incremental_peer_is_starved_over_a_full_cycle() {
        let ids = [1u32, 3, 5, 7, 9];
        let mut scheduler = scheduler_with(&ids.map(|id| (id, 3, true)));
        let leaders = drive_leaders(&mut scheduler, &ids, 10);
        for id in ids {
            let led = leaders.iter().filter(|l| **l == Some(id)).count();
            assert_eq!(
                led, 2,
                "stream {id} must lead 2 of 10 passes, led {led}: {leaders:?}"
            );
        }
    }

    /// A stream that reaches the converter and emits nothing — its send
    /// window is exhausted — has not taken its turn, so it must not consume
    /// the lead and push the cursor past the peers still waiting.
    ///
    /// TO SEE THIS RED: drop the `consumed > 0` term from
    /// [`ReadyIncrementalCensus::note_fired`]. Stream 1 then "leads" without
    /// sending and this fails with
    /// `the lead belongs to the first incremental stream that moved bytes` /
    /// `left: Some(1)` / `right: Some(3)`.
    #[test]
    fn a_window_blocked_stream_does_not_take_the_lead() {
        let mut scheduler = scheduler_with(&[(1, 3, true), (3, 3, true), (5, 3, true)]);
        let (order, mut census) = scheduler.begin_pass([1u32, 3, 5], |_| true);
        assert_eq!(order, vec![1, 3, 5]);
        // Stream 1 is window-blocked; 3 and 5 send.
        census.note_fired(1, true, 0);
        census.note_fired(3, true, 1);
        census.note_fired(5, true, 1);
        assert_eq!(
            census.first_incremental_fired,
            Some(3),
            "the lead belongs to the first incremental stream that moved bytes"
        );
    }

    /// `None` leaves the cursor untouched: a pass in which no incremental
    /// stream sent anything must not silently rotate the bucket.
    #[test]
    fn a_pass_with_no_incremental_progress_leaves_the_cursor_alone() {
        let mut scheduler = scheduler_with(&[(1, 3, true), (3, 3, true)]);
        let leaders = drive_leaders(&mut scheduler, &[1, 3], 1);
        assert_eq!(leaders, vec![Some(1)]);
        // A second pass where nothing fires.
        let (order, census) = scheduler.begin_pass([1u32, 3], |_| true);
        assert_eq!(order, vec![3, 1], "pass two starts after the pass-one lead");
        assert_eq!(census.first_incremental_fired, None);
        scheduler.end_pass(order, census);
        assert_eq!(
            scheduler.prioriser.incremental_cursor, 1,
            "a pass with no incremental progress must not move the cursor"
        );
    }

    /// The RFC 9218 §4 round-robin cursor is ONE connection-global stream id,
    /// so only the urgency bucket that supplies the pass's leader rotates.
    /// Every other bucket is rotated by a cursor drawn from a foreign id
    /// range and can be permanently static.
    ///
    /// **This test pins observed behaviour, not a desired property.** It is
    /// the counter-example that scopes LIFECYCLE.md invariant 26 to the
    /// leading bucket: u=0 alternates `1, 3, 1, 3, …` as invariant 26
    /// promises, while u=3 is `5, 7` in every pass and stream 7 never leads
    /// it. `partition_point(|id| *id <= cursor)` over `[5, 7]` with a cursor
    /// of 1 or 3 is 0 in every pass, so `rotate_left(0)` is a no-op forever.
    ///
    /// It is positional starvation here because every stream still gets its
    /// DATA frame; it becomes byte starvation as soon as a pass is cut short,
    /// since a `FlushOutcome::Stalled` at stream 5 leaves 7 unwritten, pass
    /// after pass.
    ///
    /// This is NOT a regression: `Prioriser` is byte-identical to its parent
    /// commit and the field's own doc has always scoped the cursor to "the
    /// lowest-urgency bucket that contained at least one incremental
    /// stream". Fixing it means `incremental_cursor: [StreamId;
    /// URGENCY_LEVELS]`, which changes wire ordering on multi-bucket
    /// connections and needs its own changeset and its own e2e evidence.
    ///
    /// DO NOT "fix" this test by changing its expectation. If per-bucket
    /// rotation lands, delete it and move its scenario into
    /// `fairness_property`, whose `RotationPlan` deliberately keeps every
    /// distractor in a strictly lower-priority bucket so its main bucket is
    /// always the leader — the exact scope invariant 26 claims.
    ///
    /// TO SEE THIS RED: this test asserts the LEADING bucket too, so it reds
    /// under the same two mutations invariant 26's other tests do — changing
    /// `partition_point(|id| *id <= self.incremental_cursor)` to `*id <` in
    /// [`Prioriser::apply_incremental_rotation`], or deleting the
    /// `advance_incremental_cursor` statement from
    /// [`H2Scheduler::end_pass`]. Either freezes the leading bucket and the
    /// assertion fails with
    /// `left: [[1, 3, 5, 7], [1, 3, 5, 7], [1, 3, 5, 7], [1, 3, 5, 7], [1, 3, 5, 7], [1, 3, 5, 7]]`.
    ///
    /// Its DISTINCTIVE half — the frozen TRAILING bucket — has no one-line
    /// red, and that is the point: it asserts what the code already does.
    /// Note that both mutations above leave columns 2-3 reading `5, 7`
    /// throughout, exactly as the green run does; nothing short of the
    /// design change moves them. The change that
    /// invalidates it is a design change — `incremental_cursor:
    /// [StreamId; URGENCY_LEVELS]`, a per-bucket leader in
    /// [`ReadyIncrementalCensus`] (`note_fired` would need the urgency it is
    /// not given today), and an [`H2Scheduler::end_pass`] that commits one
    /// per bucket. `end_pass` cannot do it with what it holds now, because
    /// `first_incremental_fired` is a single id. After that change the
    /// trailing bucket alternates `5, 7 / 7, 5 / …` like the leading one and
    /// the expectation below is simply wrong. Delete the test then; do not
    /// edit its expectation to match.
    #[test]
    fn the_round_robin_cursor_is_connection_global_so_only_the_leading_bucket_rotates() {
        let mut scheduler =
            scheduler_with(&[(1, 0, true), (3, 0, true), (5, 3, true), (7, 3, true)]);
        let mut orders = Vec::new();
        for _ in 0..6 {
            let (order, mut census) = scheduler.begin_pass([1u32, 3, 5, 7], |_| true);
            for &stream_id in &order {
                let (_, is_incremental) = scheduler.priority(&stream_id);
                census.note_fired(stream_id, is_incremental, 1);
            }
            orders.push(order.clone());
            scheduler.end_pass(order, census);
        }
        assert_eq!(
            orders,
            vec![
                vec![1, 3, 5, 7],
                vec![3, 1, 5, 7],
                vec![1, 3, 5, 7],
                vec![3, 1, 5, 7],
                vec![1, 3, 5, 7],
                vec![3, 1, 5, 7],
            ],
            "the leading bucket (u=0) alternates; the trailing bucket (u=3) \
             is frozen at `5, 7` because the single connection-global cursor \
             only ever holds an id from the leading bucket"
        );
    }

    // ── The same-urgency ready-incremental census (invariant 17) ─────────

    /// The census counts incremental streams that are *ready*, per urgency
    /// bucket. A connection-global count would make the solo `u=0, i` stream
    /// below see a peer and yield to a stream it can never interleave with,
    /// which is the invariant-15 strand.
    ///
    /// TO SEE THIS RED: in [`H2Scheduler::begin_pass`], replace
    /// `census.ready[bucket(urgency)] += 1;` with an increment of every
    /// bucket — the connection-global count this scoping replaced. The first
    /// assertion fails with
    /// `the solo u=0 incremental stream has no peer to interleave with` /
    /// `left: 3` / `right: 1`.
    /// `fairness_property::qc_incremental_leadership_visits_every_peer_once_per_cycle`
    /// reds alongside it with `pass 0: urgency <U> must hold exactly <K>
    /// ready incremental peers, holds <K + distractors>`. `quickcheck` seeds
    /// `Gen` from OS entropy, so `<U>`, `<K>` and the run that trips first
    /// differ every time — the shape is stable, the numbers are not.
    #[test]
    fn the_ready_census_is_scoped_to_one_urgency_bucket() {
        let mut scheduler =
            scheduler_with(&[(1, 0, true), (3, 7, true), (5, 7, true), (7, 3, false)]);
        let (_, census) = scheduler.begin_pass([1u32, 3, 5, 7], |_| true);
        assert_eq!(
            census.incremental_peer_count(0),
            1,
            "the solo u=0 incremental stream has no peer to interleave with"
        );
        assert_eq!(census.incremental_peer_count(7), 2);
        assert_eq!(
            census.incremental_peer_count(3),
            0,
            "a non-incremental stream is never a peer"
        );
        assert_eq!(census.ready_total(), 3);
        assert_eq!(
            census.incremental_count(),
            3,
            "the traced incremental count covers every bucket"
        );
    }

    /// A stream that is not ready to emit this pass is not a peer to
    /// interleave with, so it must not inflate the count that decides
    /// whether its bucket-mates yield.
    #[test]
    fn an_unready_incremental_stream_is_not_counted_as_a_peer() {
        let mut scheduler = scheduler_with(&[(1, 3, true), (3, 3, true), (5, 3, true)]);
        let (_, census) = scheduler.begin_pass([1u32, 3, 5], |id| id != 3);
        assert_eq!(census.incremental_peer_count(3), 2);
        assert_eq!(
            census.incremental_count(),
            3,
            "the rotation still placed all three; only the READY census drops one"
        );
    }

    /// The readiness projection is the caller's only cost in this module and
    /// it is paid for incremental streams alone — the same streams the
    /// inline census evaluated before the extraction. A projection that ran
    /// for every open stream would put a `Context.streams` index and three
    /// parsing-phase tests on every stream of every write pass.
    ///
    /// TO SEE THIS RED: hoist the `ready(stream_id)` call in
    /// [`H2Scheduler::begin_pass`] above the `if !is_incremental { continue; }`
    /// guard. The assertion fails with
    /// `only the incremental stream's readiness may be projected` /
    /// `left: [3, 5, 1]` / `right: [1]` — the pass order, every stream of it.
    #[test]
    fn the_readiness_projection_runs_for_incremental_streams_only() {
        let mut scheduler = scheduler_with(&[(1, 3, true), (3, 3, false), (5, 3, false)]);
        let mut consulted = Vec::new();
        let (_, _census) = scheduler.begin_pass([1u32, 3, 5], |id| {
            consulted.push(id);
            true
        });
        assert_eq!(
            consulted,
            vec![1],
            "only the incremental stream's readiness may be projected"
        );
    }

    /// LIFECYCLE.md invariant 17's mid-pass half. These three replace the
    /// scalar tests that used to re-implement the `saturating_sub` in the
    /// test body: they now drive the production method.
    ///
    /// TO SEE THIS RED: replace the `bucket(urgency)` index in
    /// [`ReadyIncrementalCensus::note_ineligible`] with a constant `0`. This
    /// fails with `urgency-1 drops to 2` / `left: 3` / `right: 2`: the
    /// decrement lands in a bucket the departing stream never belonged to,
    /// so its own bucket-mates keep yielding to a peer that has left.
    #[test]
    fn note_ineligible_reduces_only_its_own_bucket() {
        let mut census = ReadyIncrementalCensus {
            ready: [0, 3, 0, 2, 0, 0, 0, 0],
            incremental_count: 5,
            first_incremental_fired: None,
        };
        census.note_ineligible(1, true);
        assert_eq!(census.incremental_peer_count(1), 2, "urgency-1 drops to 2");
        assert_eq!(census.incremental_peer_count(3), 2, "urgency-3 untouched");
        assert_eq!(census.ready_total(), 4);
    }

    #[test]
    fn note_ineligible_saturates_at_zero() {
        let mut census = ReadyIncrementalCensus {
            ready: [0; URGENCY_LEVELS],
            incremental_count: 0,
            first_incremental_fired: None,
        };
        census.note_ineligible(0, true);
        assert_eq!(
            census.incremental_peer_count(0),
            0,
            "an empty bucket must saturate rather than wrap"
        );
    }

    #[test]
    fn note_ineligible_is_a_no_op_for_a_non_incremental_stream() {
        let mut census = ReadyIncrementalCensus {
            ready: [0, 3, 0, 0, 0, 0, 0, 0],
            incremental_count: 3,
            first_incremental_fired: None,
        };
        census.note_ineligible(1, false);
        assert_eq!(
            census.incremental_peer_count(1),
            3,
            "a non-incremental stream was never counted, so it cannot leave"
        );
    }

    // ── The order buffer ────────────────────────────────────────────────

    /// `end_pass` takes the buffer back so the next pass reuses its
    /// allocation, which is the whole reason it is moved rather than
    /// created per pass.
    #[test]
    fn end_pass_returns_the_order_buffer_for_reuse() {
        let mut scheduler = scheduler_with(&[(1, 3, false), (3, 3, false)]);
        let (order, census) = scheduler.begin_pass([1u32, 3], |_| true);
        assert!(
            scheduler.order.is_empty(),
            "the scheduler holds an empty buffer while a pass is live"
        );
        scheduler.end_pass(order, census);
        assert_eq!(scheduler.order, vec![1, 3]);
        assert!(scheduler.order.capacity() >= 2);
    }

    #[test]
    fn reclaim_idle_buffer_shrinks_an_oversized_order_buffer() {
        let mut scheduler = H2Scheduler {
            order: Vec::with_capacity(4096),
            ..H2Scheduler::default()
        };
        scheduler.reclaim_idle_buffer(16);
        assert!(scheduler.order.capacity() <= 4096, "capacity must not grow");
        assert!(
            scheduler.order.capacity() < 4096,
            "a buffer past 4x the retain size must shrink"
        );
        let before = scheduler.order.capacity();
        scheduler.reclaim_idle_buffer(16);
        assert_eq!(
            scheduler.order.capacity(),
            before,
            "reclaim is idempotent once the buffer is within budget"
        );
    }

    // ── Property coverage: RFC 9218 §4 leadership fairness ──────────────
    //
    // The deterministic tests above fix two shapes: four same-urgency
    // incremental peers over eight passes, and five over ten. This property
    // generalises the peer count (2..=8), the stream ids and their gaps, the
    // urgency bucket they share, the number of cycles, and — the part no
    // deterministic case covers — a set of DISTRACTORS that must not perturb
    // the rotation: non-incremental streams in the same bucket, and
    // incremental streams in a strictly lower-priority bucket.
    //
    // It is a sibling of `h2.rs`'s `write_pass_property` and
    // `reassembly_property` rather than a case inside either. Those two
    // drive the HPACK encoder across a write pass and the CONTINUATION
    // reassembly accumulator across a read pass; this one drives the
    // scheduler's cursor across many passes and its oracle — the cyclic
    // successor in the plan's own ascending id list — is computed from the
    // generated plan and never from anything the scheduler returned. One
    // `quickcheck` verdict over three unrelated state machines would say
    // nothing about any of them.
    //
    // A panic inside `drive()` (an out-of-range urgency reaching `bucket`,
    // a rotation that drops a stream) fails the run directly.
    mod fairness_property {
        use quickcheck::{Arbitrary, Gen, TestResult, quickcheck};

        use super::*;

        /// One generated rotation scenario.
        #[derive(Debug, Clone)]
        struct RotationPlan {
            /// 2..=8 incremental streams sharing `urgency`, ascending.
            incremental: Vec<StreamId>,
            /// The urgency bucket they share.
            urgency: u8,
            /// Streams that must never lead: non-incremental peers in the
            /// same bucket, and incremental streams in a numerically higher
            /// (lower-priority) bucket that therefore sort after them.
            distractors: Vec<(StreamId, u8, bool)>,
            /// Full cycles to drive, 1..=4.
            cycles: u8,
        }

        impl Arbitrary for RotationPlan {
            fn arbitrary(g: &mut Gen) -> Self {
                let peers = *g.choose(&[2u8, 3, 4, 5, 6, 7, 8]).expect("non-empty slice");
                let urgency = *g
                    .choose(&[0u8, 1, 2, 3, 4, 5, 6, 7])
                    .expect("non-empty slice");
                // Client-initiated ids are odd; the gaps are arbitrary so the
                // rotation cannot rely on them being consecutive.
                let mut next: StreamId = 1;
                let mut incremental = Vec::with_capacity(usize::from(peers));
                for _ in 0..peers {
                    incremental.push(next);
                    next += 2 * (1 + u32::from(u8::arbitrary(g) % 4));
                }
                let mut distractors = Vec::new();
                for _ in 0..(u8::arbitrary(g) % 4) {
                    let id = next;
                    next += 2;
                    if urgency < MAX_URGENCY && bool::arbitrary(g) {
                        let spread = MAX_URGENCY - urgency;
                        let lower_priority = urgency + 1 + (u8::arbitrary(g) % spread);
                        distractors.push((id, lower_priority, true));
                    } else {
                        distractors.push((id, urgency, false));
                    }
                }
                RotationPlan {
                    incremental,
                    urgency,
                    distractors,
                    cycles: 1 + u8::arbitrary(g) % 4,
                }
            }
        }

        fn drive(plan: RotationPlan) -> TestResult {
            let peers = plan.incremental.len();
            let mut entries: Vec<(StreamId, u8, bool)> = plan
                .incremental
                .iter()
                .map(|&id| (id, plan.urgency, true))
                .collect();
            entries.extend(plan.distractors.iter().copied());
            let all_ids: Vec<StreamId> = entries.iter().map(|&(id, _, _)| id).collect();

            let mut scheduler = scheduler_with(&entries);
            let passes = peers * usize::from(plan.cycles);
            let mut leaders = Vec::with_capacity(passes);
            for pass in 0..passes {
                let (order, mut census) = scheduler.begin_pass(all_ids.iter().copied(), |_| true);
                if order.len() != all_ids.len() {
                    return TestResult::error(format!(
                        "pass {pass}: the rotation is a permutation, expected \
                         {} ids, got {order:?}",
                        all_ids.len()
                    ));
                }
                // Bucket-scoped: the lower-priority incremental distractors
                // sit in their own buckets and must not inflate this one.
                if census.incremental_peer_count(plan.urgency) != peers {
                    return TestResult::error(format!(
                        "pass {pass}: urgency {} must hold exactly {peers} ready \
                         incremental peers, holds {}",
                        plan.urgency,
                        census.incremental_peer_count(plan.urgency)
                    ));
                }
                for &stream_id in &order {
                    let (_, is_incremental) = scheduler.priority(&stream_id);
                    census.note_fired(stream_id, is_incremental, 1);
                }
                leaders.push(census.first_incremental_fired);
                scheduler.end_pass(order, census);
            }

            // Oracle: built from the plan's own ascending id list, never
            // from anything the scheduler produced. Pass n is led by the
            // n-th stream, cycling.
            for (pass, leader) in leaders.iter().enumerate() {
                let expected = Some(plan.incremental[pass % peers]);
                if *leader != expected {
                    return TestResult::error(format!(
                        "pass {pass}: expected leader {expected:?}, got {leader:?} \
                         (peers {:?}, leaders {leaders:?})",
                        plan.incremental
                    ));
                }
            }
            // The starvation bound, stated directly: every peer leads the
            // same number of times over a whole number of cycles.
            for &id in &plan.incremental {
                let led = leaders.iter().filter(|l| **l == Some(id)).count();
                if led != usize::from(plan.cycles) {
                    return TestResult::error(format!(
                        "stream {id} led {led} of {passes} passes, expected {} \
                         — a starved peer",
                        plan.cycles
                    ));
                }
            }
            TestResult::passed()
        }

        quickcheck! {
            fn qc_incremental_leadership_visits_every_peer_once_per_cycle(plan: RotationPlan) -> TestResult {
                drive(plan)
            }
        }
    }
}
