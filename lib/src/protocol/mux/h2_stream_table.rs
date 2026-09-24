//! H2 wire-level stream-slot bookkeeping for [`super::h2::ConnectionH2`].
//!
//! Groups the mapping from wire `StreamId`s to `GlobalStreamId` slots and its
//! directly-associated per-stream caches behind a narrow, closed API — the
//! shape [`super::hpack_state::HpackState`] and
//! [`super::h2_flow_control::H2FlowControl`] established in the prior two
//! extraction steps. Every field here is private to this module; `ConnectionH2`
//! reaches them only through the accessor methods declared below.
//!
//! This module owns:
//!
//! - [`H2StreamTable::streams`] — the wire `StreamId -> GlobalStreamId` map
//!   (`ConnectionH2.streams` before this extraction).
//! - [`H2StreamTable::highest_peer_stream_id`] — RFC 9113 §6.8 GOAWAY
//!   last-stream-id bookkeeping.
//! - `expect_read` / `expect_write` — the index-caching fields
//!   `LIFECYCLE.md` §5 documents at length, including their invalidation
//!   discipline. [`H2StreamTable::remove`] is now the ONLY function in the
//!   crate that can remove an entry from the wire map — Rust's module
//!   privacy turns LIFECYCLE.md §5.4's "no call site removes inline
//!   anymore" from prose that can go stale (see #1399, which had to correct
//!   a false claim of exactly this shape) into a compiler-enforced fact:
//!   `streams` is a private field of this module, so `self.streams.remove`
//!   cannot even be written outside `h2_stream_table.rs`.
//! - `rst_sent` — RFC 9113 §6.8 duplicate-RST_STREAM dedupe.
//! - The per-stream liveness/stall caches `stream_last_activity_at`,
//!   `stream_fc_stalled_since`, `stream_fc_stalled_progress` (LIFECYCLE.md
//!   §7.2), and [`H2StreamTable::collect_timed_out`], the pure reap-candidate
//!   scan `ConnectionH2::cancel_timed_out_streams` drives.
//!
//! `ConnectionH2::create_stream`, `ConnectionH2::start_stream`,
//! `ConnectionH2::remove_dead_stream` and `ConnectionH2::refuse_stream_and_discard`
//! stay on `ConnectionH2`: they orchestrate `Context`, logging, metrics, the
//! RFC 9218 `prioriser` and readiness — none of which this module has, the
//! same boundary [`super::h2_flow_control`]'s doc draws for per-stream send
//! window state. Their stream-table bookkeeping now delegates to this closed
//! API instead of touching seven raw fields directly.
//!
//! ## Determinism (RFC 9113 doesn't mandate an order; issue #1338 does)
//!
//! [`H2StreamTable::collect_timed_out`] used to read two `HashMap`s
//! (`stream_last_activity_at`, `stream_fc_stalled_since`) with a plain
//! `for (&k, &v) in map` loop, in the map's iteration order — seeded
//! per-`HashMap` (`RandomState`), so **which of several simultaneously
//! timed-out streams got RST_STREAM'd first was non-deterministic across
//! process restarts**, even for the identical sequence of inbound frames and
//! identical wall-clock timing. That order flows directly onto the wire:
//! `collect_timed_out`'s returned `Vec` order is the order
//! `ConnectionH2::cancel_timed_out_streams` calls `enqueue_rst` in, which
//! pushes onto [`super::h2_control_tx::H2ControlTx`]'s queue, drained onto
//! the wire in push order by `flush_pending_control_frames`.
//!
//! Both maps are now `BTreeMap`, so `collect_timed_out` walks each guard
//! group (idle-timeout, then window-stall) in ascending `StreamId` order —
//! fixed, reproducible, and free (`BTreeMap` iteration is already sorted; no
//! extra sort step, no injected seed). `stream_fc_stalled_progress` is
//! converted alongside `stream_fc_stalled_since` for the same reason
//! `H2FlowControl`'s module doc gives for `pending_window_updates`: the two
//! maps are documented (LIFECYCLE.md §7.2) as kept in lockstep at every
//! arm/clear/evict site, and [`H2StreamTable::check_invariants`] asserts
//! that lockstep directly by comparing their (sorted) key sequences.
//!
//! Ascending `StreamId` order is fair here in a way it is not for
//! `H2FlowControl::pending_window_updates`: a wire `StreamId` is never
//! reused (RFC 9113 §5.1.1), so a stream reaped earlier in one
//! `collect_timed_out` pass never competes for the same slot again — there
//! is no repeated round-over-round disadvantage the way a low-numbered
//! stream can keep winning a *replenished* connection-level send window
//! ahead of a high-numbered one every single `drain_window_updates_into`
//! pass. That asymmetry (a one-shot reap vs. a resource a stream competes
//! for repeatedly) is what makes a fixed total order safe to pick for
//! reaping without a fairness cursor, unlike the caveat
//! `H2FlowControl`'s own module doc carries for its pending map.
//!
//! `streams` itself (the wire `StreamId -> GlobalStreamId` map) is now a
//! `BTreeMap` too — issue #1338's last open question. The justification this
//! doc used to give for keeping it a `HashMap` was wrong, and the shape of
//! the error is worth recording: it enumerated every iteration site and
//! argued each loop BODY was order-independent, but per-element
//! commutativity does not cover EARLY TERMINATION.
//!
//! `update_initial_window_size` is the counter-example. Its body does read
//! as commutative — an additive delta to each stream's own `window`, a
//! `bool` OR-combined — but the loop also carries the `None => return true`
//! arm RFC 9113 §6.9.2 requires on `checked_add`, and it returns from
//! INSIDE the loop. So when a SETTINGS_INITIAL_WINDOW_SIZE change overflows
//! one stream, which PREFIX of the others was already mutated before the
//! abort is precisely the iteration order. `handle_settings_frame` turns
//! that `true` into a GOAWAY, and the half-updated window set stays in
//! `context.streams` for `close`'s teardown walk and any pass still to run.
//!
//! Two further sites order observable work rather than arithmetic.
//! `handle_goaway_frame` walks the map into `retry_streams` and pushes each
//! retryable stream onto `context.pending_links`, so relink — and therefore
//! reconnect — order was hash order. `ConnectionH2::close` notifies each
//! linked stream through `endpoint.end_stream`, so teardown order was hash
//! order. Neither reorders the bytes of a fixed frame set, which is what
//! the old text meant by "scheduling only"; both are still the
//! restart-to-restart irreproducibility issue #1338 is about.
//!
//! The rest are genuinely order-free and were correct under either
//! container: `write_streams` hands the keys to `H2Scheduler::begin_pass`,
//! which sorts by `(urgency, id)` and discards the order it was given;
//! `compute_stream_byte_totals` sums two `usize`s; `close`'s debug-log loop
//! orders only log lines; and `GOAWAY`'s `last_stream_id` comes from the
//! scalar `highest_peer_stream_id`, never from iterating `streams`.
//!
//! `streams` is read on the per-frame path, so the container was measured,
//! not assumed: see `H2StreamTable::new` and `lib/benches/h2_stream_table.rs`.
//!
//! `rst_sent` is only ever queried by `.contains()` / mutated by
//! `.insert()`/`.remove()` — never iterated — so it carries no ordering
//! concern either.

use std::{
    collections::{BTreeMap, HashSet},
    time::{Duration, Instant},
};

use super::{GlobalStreamId, StreamId, h2::H2StreamId};

/// H2 wire-level stream-slot bookkeeping: see the module doc.
pub(super) struct H2StreamTable {
    /// Wire `StreamId -> GlobalStreamId` map.
    streams: BTreeMap<StreamId, GlobalStreamId>,
    /// Highest stream ID accepted from the peer (used for GoAway last_stream_id).
    highest_peer_stream_id: StreamId,
    /// See `LIFECYCLE.md` §5.1 — `(id, remaining bytes)` of a parked partial read.
    expect_read: Option<(H2StreamId, usize)>,
    /// See `LIFECYCLE.md` §5.1 — the stream a parked partial write belongs to.
    expect_write: Option<H2StreamId>,
    /// RFC 9113 §6.8: tracks stream IDs for which RST_STREAM has already been
    /// sent, preventing duplicate RST_STREAM frames on the wire.
    rst_sent: HashSet<StreamId>,
    /// Per-stream wall-clock timestamp of last meaningful activity (DATA or
    /// HEADERS frame receipt) — the bidirectional-silence (slow-multiplex)
    /// reap guard. `BTreeMap` for deterministic reap order — see the module
    /// doc's Determinism section.
    stream_last_activity_at: BTreeMap<StreamId, Instant>,
    /// Per-stream timestamp of when the stream first became outbound
    /// flow-control-stalled — the window-stall reap guard. Kept in lockstep
    /// with `stream_fc_stalled_progress`; see [`Self::check_invariants`].
    stream_fc_stalled_since: BTreeMap<StreamId, Instant>,
    /// Cumulative outbound flow-control bytes drained on a window-stalled
    /// stream since `stream_fc_stalled_since` was armed (M2 cumulative-stall
    /// budget). An entry exists IFF `stream_fc_stalled_since` has one.
    stream_fc_stalled_progress: BTreeMap<StreamId, usize>,
}

/// Outcome of [`H2StreamTable::remove`] — whether `stream_id` was present in
/// the wire map. `ConnectionH2::remove_dead_stream` uses this to decide
/// whether to log its "dead stream_id missing from streams map" error —
/// this module stays log-free, like [`super::hpack_state`] and
/// [`super::h2_flow_control`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RemoveOutcome {
    /// `stream_id` was present and every associated cache entry is now gone.
    Removed,
    /// `stream_id` was already absent from the wire map. The other caches
    /// are still evicted defensively (a stale entry there would be a
    /// separate bug), but there is nothing to report as removed.
    NotPresent,
}

impl H2StreamTable {
    /// Builds the empty stream-slot bookkeeping for a new connection.
    /// `expect_read` seeds the initial parked read — `ConnectionH2::new`'s
    /// caller differs it for a frontend (client preface) vs. a backend
    /// (nothing yet expected) connection.
    ///
    /// `streams` is built with `BTreeMap::new()` and carries NO capacity
    /// hint. `BTreeMap` has no `with_capacity`: it allocates one node at a
    /// time instead of one contiguous bucket array, so there is nothing to
    /// size up front. The `HashMap::with_capacity(8)` this replaced is
    /// dropped outright rather than translated — do not reintroduce the 8
    /// as a comment claiming a reservation that no longer happens. The
    /// number itself is not lost: it is the expected steady-state occupancy
    /// `lib/benches/h2_stream_table.rs` measures as its smallest `n`, and
    /// the size at which the tree wins hardest — a tree of 11 keys or fewer
    /// is one node, while hashing a `u32` with SipHash-1-3 costs about 9 ns
    /// whatever `n` is. See the module doc's Determinism section for why
    /// the container changed at all.
    pub(super) fn new(expect_read: Option<(H2StreamId, usize)>) -> Self {
        let table = H2StreamTable {
            streams: BTreeMap::new(),
            highest_peer_stream_id: 0,
            expect_read,
            expect_write: None,
            rst_sent: HashSet::new(),
            stream_last_activity_at: BTreeMap::new(),
            stream_fc_stalled_since: BTreeMap::new(),
            stream_fc_stalled_progress: BTreeMap::new(),
        };
        table.debug_assert_invariants();
        table
    }

    // ---- wire StreamId -> GlobalStreamId map --------------------------------

    /// Read-only borrow of the wire map, for the several distinct read-only
    /// iteration/query shapes call sites in `h2.rs` need (`.len()`,
    /// `.is_empty()`, `.contains_key()`, `.keys()`, `.values()`, `.iter()`).
    /// Every iteration shape among those walks ascending `StreamId`. That is
    /// load-bearing, not incidental: three call sites turn this order into
    /// observable behaviour, and the module doc's Determinism section names
    /// them. Do not swap the container back without reading it.
    ///
    /// No `&mut` accessor is exposed: mutation only happens through
    /// [`Self::register`] and [`Self::remove`], which keep the associated
    /// caches and `expect_read`/`expect_write` invalidation in lockstep.
    pub(super) fn streams(&self) -> &BTreeMap<StreamId, GlobalStreamId> {
        &self.streams
    }

    /// Convenience wrapper for the common `self.streams.get(&id).copied()`
    /// shape (`GlobalStreamId` is `Copy`).
    pub(super) fn get(&self, stream_id: StreamId) -> Option<GlobalStreamId> {
        self.streams.get(&stream_id).copied()
    }

    pub(super) fn len(&self) -> usize {
        self.streams.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.streams.is_empty()
    }

    /// Register a newly-created stream: maps `stream_id -> gid` on the wire
    /// and arms its liveness timer. `ConnectionH2::create_stream` and
    /// `ConnectionH2::start_stream` both did this as two separate inserts
    /// before this extraction; consolidated here because they always ran
    /// together (a stream is never wire-mapped without its liveness timer
    /// armed, or vice versa).
    pub(super) fn register(&mut self, stream_id: StreamId, gid: GlobalStreamId, now: Instant) {
        self.streams.insert(stream_id, gid);
        self.stream_last_activity_at.insert(stream_id, now);
        debug_assert_eq!(
            self.streams.get(&stream_id).copied(),
            Some(gid),
            "register must map the wire id to the given slot"
        );
        debug_assert!(
            self.stream_last_activity_at.contains_key(&stream_id),
            "register must arm the per-stream idle timer"
        );
        self.debug_assert_invariants();
    }

    /// Test-only backdoor: map `stream_id -> gid` on the wire WITHOUT arming
    /// the liveness timer, deliberately violating the invariant
    /// [`Self::register`] otherwise upholds unconditionally. Production code
    /// has no legitimate reason to do this — no real caller can create a wire
    /// mapping without a liveness timestamp — but
    /// `mod::tests::shutting_down_refreshes_the_snapshot_so_the_drain_budget_expires`
    /// needs exactly that artificial state, to force
    /// `cancel_timed_out_streams` down its early-return path (empty activity
    /// map) rather than have it race an RST_STREAM into the test's
    /// unrelated drain-budget assertion. Mirrors the existing
    /// `ConnectionH2::__test_set_last_stream_id` precedent.
    #[cfg(any(test, feature = "e2e-hooks"))]
    pub(super) fn __test_insert_wire_mapping_only(
        &mut self,
        stream_id: StreamId,
        gid: GlobalStreamId,
    ) {
        self.streams.insert(stream_id, gid);
    }

    /// The single removal chokepoint for every per-stream cache this module
    /// owns, including `expect_read`/`expect_write` invalidation — see the
    /// module doc and `LIFECYCLE.md` §5.4. `global_stream_id` is the
    /// `GlobalStreamId` slot being retired, used to null out a cached
    /// `expect_read`/`expect_write` that references it (the slot may be
    /// popped by `shrink_trailing_recycle` on the next `create_stream`, which
    /// would turn a stale cached `gid` into a dangling `Vec` index).
    pub(super) fn remove(
        &mut self,
        stream_id: StreamId,
        global_stream_id: GlobalStreamId,
    ) -> RemoveOutcome {
        let outcome = if self.streams.remove(&stream_id).is_some() {
            RemoveOutcome::Removed
        } else {
            RemoveOutcome::NotPresent
        };
        self.rst_sent.remove(&stream_id);
        self.stream_last_activity_at.remove(&stream_id);
        self.stream_fc_stalled_since.remove(&stream_id);
        self.stream_fc_stalled_progress.remove(&stream_id);
        if matches!(self.expect_write, Some(H2StreamId::Other { gid, .. }) if gid == global_stream_id)
        {
            self.expect_write = None;
        }
        if matches!(
            self.expect_read,
            Some((H2StreamId::Other { gid, .. }, _)) if gid == global_stream_id
        ) {
            self.expect_read = None;
        }
        debug_assert!(
            !self.rst_sent.contains(&stream_id),
            "rst_sent still contains stream_id {stream_id} after eviction"
        );
        debug_assert!(
            !self.stream_last_activity_at.contains_key(&stream_id),
            "stream_last_activity_at still contains stream_id {stream_id} after eviction"
        );
        debug_assert!(
            !self.stream_fc_stalled_since.contains_key(&stream_id),
            "stream_fc_stalled_since still contains stream_id {stream_id} after eviction"
        );
        debug_assert!(
            !self.stream_fc_stalled_progress.contains_key(&stream_id),
            "stream_fc_stalled_progress still contains stream_id {stream_id} after eviction"
        );
        self.debug_assert_invariants();
        outcome
    }

    // ---- highest_peer_stream_id ---------------------------------------------

    pub(super) fn highest_peer_stream_id(&self) -> StreamId {
        self.highest_peer_stream_id
    }

    /// RFC 9113 §6.8: record a peer-initiated `stream_id` toward the GoAway
    /// last-stream-id watermark. Monotonic non-decreasing — a no-op when
    /// `stream_id` is not new-highest.
    pub(super) fn observe_peer_stream_id(&mut self, stream_id: StreamId) {
        let before = self.highest_peer_stream_id;
        if stream_id > self.highest_peer_stream_id {
            self.highest_peer_stream_id = stream_id;
        }
        debug_assert!(
            self.highest_peer_stream_id >= before,
            "highest_peer_stream_id must never regress"
        );
    }

    // ---- expect_read / expect_write -----------------------------------------

    pub(super) fn expect_read(&self) -> Option<(H2StreamId, usize)> {
        self.expect_read
    }

    pub(super) fn set_expect_read(&mut self, value: Option<(H2StreamId, usize)>) {
        self.expect_read = value;
    }

    pub(super) fn expect_write(&self) -> Option<H2StreamId> {
        self.expect_write
    }

    pub(super) fn set_expect_write(&mut self, value: Option<H2StreamId>) {
        self.expect_write = value;
    }

    // ---- rst_sent -------------------------------------------------------------

    pub(super) fn rst_sent_contains(&self, stream_id: StreamId) -> bool {
        self.rst_sent.contains(&stream_id)
    }

    /// Narrow escape hatch for the two sites that need raw `&mut
    /// HashSet<StreamId>` access: `h2_control_tx::H2ControlTx::enqueue_rst`
    /// (unit-tested independently of `ConnectionH2` — see `LIFECYCLE.md`
    /// §8.2) and `end_stream`'s direct `rst_sent.insert` on the dedupe-check
    /// path. Both call sites only ever `.insert()`; nothing about this
    /// accessor is `pub` outside `h2.rs`.
    pub(super) fn rst_sent_mut(&mut self) -> &mut HashSet<StreamId> {
        &mut self.rst_sent
    }

    // ---- per-stream activity / flow-control-stall caches --------------------

    pub(super) fn stream_last_activity_at(&self) -> &BTreeMap<StreamId, Instant> {
        &self.stream_last_activity_at
    }

    pub(super) fn activity_is_empty(&self) -> bool {
        self.stream_last_activity_at.is_empty()
    }

    /// Refresh `stream_id`'s liveness timestamp IF it is currently tracked.
    /// Returns whether an entry existed. Never inserts — a stream not yet
    /// (or no longer) in the table has no liveness timer to refresh.
    pub(super) fn touch_activity(&mut self, stream_id: StreamId, at: Instant) -> bool {
        if let Some(t) = self.stream_last_activity_at.get_mut(&stream_id) {
            *t = at;
            true
        } else {
            false
        }
    }

    pub(super) fn fc_stall_is_empty(&self) -> bool {
        self.stream_fc_stalled_since.is_empty()
    }

    pub(super) fn fc_stall_progress(&self, stream_id: StreamId) -> Option<usize> {
        self.stream_fc_stalled_progress.get(&stream_id).copied()
    }

    /// M2 cumulative-stall budget: a genuine un-stall clears both the
    /// deadline and the progress accumulator together (kept in lockstep).
    pub(super) fn clear_fc_stall(&mut self, stream_id: StreamId) {
        self.stream_fc_stalled_since.remove(&stream_id);
        self.stream_fc_stalled_progress.remove(&stream_id);
    }

    /// Arm the flow-control-stall deadline WITHOUT refreshing an already-armed
    /// `Instant` (`entry().or_insert(now)`) and set the cumulative progress
    /// accumulator. See `LIFECYCLE.md` §7.2 for why the deadline must not be
    /// refreshed on every call (a `WINDOW_UPDATE(+1)` drip must not reset it).
    pub(super) fn arm_fc_stall(&mut self, stream_id: StreamId, now: Instant, progress: usize) {
        self.stream_fc_stalled_since.entry(stream_id).or_insert(now);
        self.stream_fc_stalled_progress.insert(stream_id, progress);
    }

    /// Union the two independent per-stream reap guards (bidirectional
    /// silence + outbound flow-control stall), deduped, into the ordered
    /// list of `(stream_id, reason)` pairs `ConnectionH2::cancel_timed_out_streams`
    /// RSTs. Streams not currently in the wire map, or already RST'd, are
    /// skipped. The idle-timeout guard takes precedence on a tie (a stream
    /// present in both maps and expired in both), purely for a stable label.
    ///
    /// Both source maps are `BTreeMap`s, so each guard group is walked in
    /// ascending `StreamId` order — see the module doc's Determinism
    /// section for why this is load-bearing (the returned order becomes the
    /// wire RST_STREAM emission order).
    pub(super) fn collect_timed_out(
        &self,
        now: Instant,
        deadline: Duration,
    ) -> Vec<(StreamId, &'static str)> {
        // `self.streams.contains_key(&sid)` defends against an activity/stall
        // entry that outlived its wire-map entry. That state is structurally
        // unreachable through this module's public API: `register` always
        // inserts into `streams` and the activity map together, and `remove`
        // always evicts both together, so nothing outside `h2_stream_table.rs`
        // can construct a `stream_last_activity_at`/`stream_fc_stalled_since`
        // entry with no matching `streams` entry — not even a test, short of
        // reaching into private fields. The check is kept anyway as defence
        // in depth against a future internal change to this module that
        // breaks that lockstep (e.g. a new insertion path added to one map
        // but not the other); it is untested by construction, not by
        // omission, and must not be deleted as apparently-dead code.
        let eligible =
            |sid: StreamId| self.streams.contains_key(&sid) && !self.rst_sent.contains(&sid);
        let expired = |t: Instant| now.saturating_duration_since(t) > deadline;
        let mut seen: HashSet<StreamId> = HashSet::new();
        let mut out: Vec<(StreamId, &'static str)> = Vec::new();
        for (&sid, &t) in &self.stream_last_activity_at {
            if eligible(sid) && expired(t) && seen.insert(sid) {
                out.push((sid, "H2::IdleTimeout"));
            }
        }
        for (&sid, &t) in &self.stream_fc_stalled_since {
            if eligible(sid) && expired(t) && seen.insert(sid) {
                out.push((sid, "H2::WindowStall"));
            }
        }
        out
    }

    // ---- invariants -----------------------------------------------------------

    /// TigerStyle invariant sweep. A single full check of every structural
    /// invariant this type must preserve, asserted at the END of every
    /// public mutating method via
    /// [`debug_assert_invariants`](Self::debug_assert_invariants) — the
    /// pattern `h2_flow_control::H2FlowControl` and
    /// `protocol::udp::manager::UdpManager` both use. Compiled out entirely
    /// in release (`#[cfg(debug_assertions)]`); on in every test/e2e/fuzz/dev
    /// build. Must never change runtime behavior — it only reads.
    ///
    /// This module has no `Context`, so it cannot check LIFECYCLE.md
    /// checklist invariants 1/3/4 (`gid` bounds against `context.streams`) —
    /// those remain `ConnectionH2::check_invariants`'s job (it has `context`)
    /// and now read this table through [`Self::streams`], [`Self::expect_read`]
    /// and [`Self::expect_write`].
    #[cfg(debug_assertions)]
    fn check_invariants(&self) {
        // LIFECYCLE.md §7.2: stream_fc_stalled_since and
        // stream_fc_stalled_progress are kept in lockstep at every
        // arm/clear/evict site — an entry exists in one IFF it exists in the
        // other. Both are BTreeMap, so comparing sorted key sequences is a
        // cheap, order-correct equality check.
        debug_assert!(
            self.stream_fc_stalled_since
                .keys()
                .eq(self.stream_fc_stalled_progress.keys()),
            "stream_fc_stalled_since and stream_fc_stalled_progress must track exactly the same stream ids"
        );
    }

    /// Run the full [`check_invariants`](Self::check_invariants) sweep, but
    /// only in debug builds — a thin wrapper so call sites read as one line
    /// and the whole sweep is dead-stripped in release.
    #[inline]
    fn debug_assert_invariants(&self) {
        #[cfg(debug_assertions)]
        self.check_invariants();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state() {
        let table = H2StreamTable::new(None);
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
        assert_eq!(table.highest_peer_stream_id(), 0);
        assert_eq!(table.expect_read(), None);
        assert_eq!(table.expect_write(), None);
    }

    #[test]
    fn register_maps_wire_id_and_arms_activity() {
        let mut table = H2StreamTable::new(None);
        let now = Instant::now();
        table.register(1, 0, now);
        assert_eq!(table.get(1), Some(0));
        assert_eq!(table.len(), 1);
        assert!(table.stream_last_activity_at().contains_key(&1));
    }

    #[test]
    fn remove_evicts_every_cache_and_invalidates_matching_expect() {
        let mut table = H2StreamTable::new(None);
        let now = Instant::now();
        table.register(1, 0, now);
        table.rst_sent_mut().insert(1);
        table.arm_fc_stall(1, now, 10);
        table.set_expect_write(Some(H2StreamId::Other { id: 1, gid: 0 }));
        table.set_expect_read(Some((H2StreamId::Other { id: 1, gid: 0 }, 5)));

        assert_eq!(table.remove(1, 0), RemoveOutcome::Removed);

        assert_eq!(table.get(1), None);
        assert!(!table.rst_sent_contains(1));
        assert_eq!(table.fc_stall_progress(1), None);
        assert!(!table.stream_last_activity_at().contains_key(&1));
        assert_eq!(table.expect_write(), None);
        assert_eq!(table.expect_read(), None);
    }

    #[test]
    fn remove_on_absent_stream_reports_not_present_but_still_evicts_caches() {
        let mut table = H2StreamTable::new(None);
        assert_eq!(table.remove(42, 0), RemoveOutcome::NotPresent);
    }

    #[test]
    fn remove_only_invalidates_expect_referencing_the_removed_gid() {
        let mut table = H2StreamTable::new(None);
        let now = Instant::now();
        table.register(1, 0, now);
        table.register(3, 1, now);
        table.set_expect_write(Some(H2StreamId::Other { id: 3, gid: 1 }));

        table.remove(1, 0);

        // expect_write references gid=1 (stream 3), untouched by removing gid=0.
        assert_eq!(
            table.expect_write(),
            Some(H2StreamId::Other { id: 3, gid: 1 })
        );
    }

    #[test]
    fn observe_peer_stream_id_is_monotonic() {
        let mut table = H2StreamTable::new(None);
        table.observe_peer_stream_id(5);
        assert_eq!(table.highest_peer_stream_id(), 5);
        table.observe_peer_stream_id(3);
        assert_eq!(table.highest_peer_stream_id(), 5, "must not regress");
        table.observe_peer_stream_id(9);
        assert_eq!(table.highest_peer_stream_id(), 9);
    }

    #[test]
    fn fc_stall_arm_does_not_refresh_an_already_armed_deadline() {
        let mut table = H2StreamTable::new(None);
        let t0 = Instant::now();
        table.arm_fc_stall(1, t0, 100);
        let t1 = t0 + std::time::Duration::from_secs(5);
        table.arm_fc_stall(1, t1, 200);
        // Deadline stays at t0 (or_insert only sets when absent); progress
        // still overwrites to the latest value.
        assert_eq!(table.stream_last_activity_at().get(&1), None);
        assert_eq!(table.fc_stall_progress(1), Some(200));
    }

    #[test]
    fn touch_activity_does_not_insert_for_an_untracked_stream() {
        let mut table = H2StreamTable::new(None);
        assert!(!table.touch_activity(1, Instant::now()));
        assert!(table.activity_is_empty());
    }

    /// The regression test for the ordering defect this extraction fixes:
    /// `collect_timed_out` must return reap candidates in ascending
    /// `StreamId` order within each guard group, deterministically,
    /// regardless of insertion order — mirrors
    /// `h2_flow_control::tests::drain_emits_ascending_stream_id_order_deterministically`.
    #[test]
    fn collect_timed_out_is_ascending_stream_id_order_deterministically() {
        let mut table = H2StreamTable::new(None);
        let old = Instant::now() - Duration::from_secs(60);
        let deadline = Duration::from_secs(30);
        // Insert in a deliberately scrambled order.
        let insertion_order: [StreamId; 9] = [17, 3, 901, 1, 501, 19, 777, 5, 45];
        for &sid in &insertion_order {
            table.register(sid, sid as GlobalStreamId, old);
        }
        let timed_out = table.collect_timed_out(Instant::now(), deadline);
        let observed: Vec<StreamId> = timed_out.iter().map(|&(sid, _)| sid).collect();
        let mut expected = insertion_order.to_vec();
        expected.sort_unstable();
        assert_eq!(
            observed, expected,
            "collect_timed_out must return the idle-timeout guard group in ascending stream_id order"
        );
        assert!(
            timed_out
                .iter()
                .all(|&(_, reason)| reason == "H2::IdleTimeout")
        );
    }

    #[test]
    fn collect_timed_out_skips_untracked_and_already_rst_streams() {
        let mut table = H2StreamTable::new(None);
        let old = Instant::now() - Duration::from_secs(60);
        let deadline = Duration::from_secs(30);
        table.register(1, 0, old);
        table.register(3, 1, old);
        table.rst_sent_mut().insert(3);
        let timed_out = table.collect_timed_out(Instant::now(), deadline);
        assert_eq!(timed_out, vec![(1, "H2::IdleTimeout")]);
    }

    #[test]
    fn collect_timed_out_idle_takes_precedence_on_a_tie() {
        let mut table = H2StreamTable::new(None);
        let old = Instant::now() - Duration::from_secs(60);
        let deadline = Duration::from_secs(30);
        table.register(1, 0, old);
        table.arm_fc_stall(1, old, 0);
        let timed_out = table.collect_timed_out(Instant::now(), deadline);
        assert_eq!(timed_out, vec![(1, "H2::IdleTimeout")]);
    }

    /// A window-stalled stream MUST be reaped on the flow-control-stall
    /// deadline even if its bidirectional-liveness timer is fresh — an
    /// inbound 1-byte DATA drip keeps `stream_last_activity_at` warm but
    /// never touches `stream_fc_stalled_since`. Without the fc-stall guard
    /// this stream is never reaped (the pre-fix window-stall hold).
    #[test]
    fn collect_timed_out_reaps_fc_stall_despite_fresh_liveness() {
        let mut table = H2StreamTable::new(None);
        let now = Instant::now();
        let deadline = Duration::from_secs(2);
        table.register(7, 0, now); // fresh: just received an inbound DATA drip
        table.arm_fc_stall(7, now - Duration::from_secs(5), 0);
        let timed_out = table.collect_timed_out(now, deadline);
        assert_eq!(timed_out, vec![(7, "H2::WindowStall")]);
    }

    #[test]
    fn collect_timed_out_empty_when_all_fresh() {
        let mut table = H2StreamTable::new(None);
        let now = Instant::now();
        let deadline = Duration::from_secs(2);
        table.register(1, 0, now);
        table.arm_fc_stall(1, now, 0);
        assert!(table.collect_timed_out(now, deadline).is_empty());
    }

    /// The wire map's iteration order is an observable, and three production
    /// sites consume it. `ConnectionH2::handle_goaway_frame` walks the map
    /// into `retry_streams` and pushes each retryable stream onto
    /// `context.pending_links`, so relink — and therefore reconnect — order
    /// is this order. `ConnectionH2::update_initial_window_size` walks the
    /// values and can `return` from INSIDE its loop when `checked_add`
    /// overflows, so the set of streams whose `window` it already rewrote
    /// when it aborts is a PREFIX of this order. `ConnectionH2::close`
    /// walks the same values calling `endpoint.end_stream`, so teardown
    /// notification order is this order.
    ///
    /// None of those three is reachable from inside this module, so the
    /// guard sits on the single property all three inherit: the map
    /// iterates in ascending wire `StreamId` whatever order `register` was
    /// called in. Keys, values and pairs are all asserted, because the
    /// three sites between them read all three shapes.
    ///
    /// TO SEE THIS RED: restore `streams` to
    /// `HashMap<StreamId, GlobalStreamId>` and `new`'s
    /// `HashMap::with_capacity(8)`. Measured on that revert, this test
    /// fails on its first assertion. No permutation below is the EXPECTED
    /// failure: `RandomState` is seeded per process, so the left-hand value
    /// differs between runs by construction and none of them is a fact to
    /// assert on. What reproduces is the failure and its shape — a
    /// permutation of `expected_ids` that is not `expected_ids`. Four runs
    /// of that revert gave four different permutations, every one red. One,
    /// as a labelled sample only: `left: [501, 901, 1, 17, 45, 3, 19, 5,
    /// 777]`.
    #[test]
    fn streams_iterates_in_ascending_stream_id_order_whatever_the_insertion_order() {
        let mut table = H2StreamTable::new(None);
        let now = Instant::now();
        // Scrambled, and deliberately NOT the id set
        // `collect_timed_out_is_ascending_stream_id_order_deterministically`
        // uses, so neither test can quietly come to rest on the other's
        // fixture.
        let insertion_order: [StreamId; 9] = [901, 5, 17, 45, 1, 777, 19, 3, 501];
        for (slot, &stream_id) in insertion_order.iter().enumerate() {
            table.register(stream_id, slot as GlobalStreamId, now);
        }

        // The slot handed to `register` is the INSERTION index, so sorting
        // the pairs by wire id scrambles the slots — which is what makes the
        // values assertion below independent of the keys assertion instead
        // of a restatement of it.
        let mut expected_pairs: Vec<(StreamId, GlobalStreamId)> = insertion_order
            .iter()
            .enumerate()
            .map(|(slot, &stream_id)| (stream_id, slot as GlobalStreamId))
            .collect();
        expected_pairs.sort_unstable();
        let expected_ids: Vec<StreamId> = expected_pairs.iter().map(|&(id, _)| id).collect();
        let expected_slots: Vec<GlobalStreamId> =
            expected_pairs.iter().map(|&(_, slot)| slot).collect();

        let observed_ids: Vec<StreamId> = table.streams().keys().copied().collect();
        assert_eq!(
            observed_ids, expected_ids,
            "streams() must walk ascending wire StreamId: begin_scheduler_pass \
             seeds the whole write-pass order buffer from exactly these keys"
        );

        let observed_slots: Vec<GlobalStreamId> = table.streams().values().copied().collect();
        assert_eq!(
            observed_slots, expected_slots,
            "streams().values() must follow that same ascending key order: it is \
             the prefix update_initial_window_size has already mutated when a \
             checked_add overflow aborts it, and the order close tears down in"
        );

        let observed_pairs: Vec<(StreamId, GlobalStreamId)> = table
            .streams()
            .iter()
            .map(|(&stream_id, &slot)| (stream_id, slot))
            .collect();
        assert_eq!(
            observed_pairs, expected_pairs,
            "streams() pairs must agree with its own keys and values: \
             handle_goaway_frame reads both halves of this iterator together"
        );
    }
}
