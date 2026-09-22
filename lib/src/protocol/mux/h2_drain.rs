//! H2 GOAWAY/drain STATE for [`super::h2::ConnectionH2`] — what the RFC 9113
//! §6.8 double-GOAWAY drain state machine is, when it transitions, and what
//! it wants to emit next. Groups [`H2DrainState`] (`draining`,
//! `peer_last_stream_id`, `started_at`, `graceful_shutdown_deadline`,
//! `initial_goaway_pending`) behind the same closed-API shape
//! [`super::hpack_state::HpackState`], [`super::h2_flow_control::H2FlowControl`],
//! [`super::h2_stream_table::H2StreamTable`] and
//! [`super::h2_flood_detector::H2FloodDetector`] established in the four
//! prior extraction steps. Every field here is private to this module;
//! `ConnectionH2` reaches them only through the accessor/decision methods
//! declared below.
//!
//! `H2DrainState`'s fields were already socket-free before this extraction —
//! they are plain `bool`/`Option<Instant>`/`Option<Duration>`/`Option<StreamId>`
//! scalars. What was NOT socket-free is the six `ConnectionH2` functions that
//! read and write them: `graceful_goaway`, `goaway`, `send_initial_goaway`,
//! `handle_goaway_frame`, `peer_gone_after_final_goaway`, and the GOAWAY stage
//! of `flush_pending_control_frames` (all `h2.rs`). This module extracts only
//! the DECISION those functions make — draining or not, defer-or-send,
//! budget-elapsed-or-not — as pure `debug_assert!`-guarded transitions over
//! [`H2DrainState`]'s own fields. Every I/O consequence of a decision
//! (serializing a GOAWAY frame into `self.zero`, logging, metrics,
//! `self.readiness`, `self.stream_table`, `self.state`) stays on
//! `ConnectionH2`, which owns the socket, `Context` and every other piece a
//! decision needs but this module deliberately does not have:
//!
//! - [`H2DrainState::begin_graceful_drain`] replaces the state-mutating half
//!   of `ConnectionH2::graceful_goaway`. It returns a
//!   [`GracefulDrainDecision`] telling the caller whether to send the FINAL
//!   GOAWAY (`ConnectionH2::goaway`), defer the initial one
//!   (`self.readiness.arm_writable()` plus a `debug!` log — reassembly is in
//!   progress and `self.zero` is off-limits, see below), or send the initial
//!   one now (`ConnectionH2::send_initial_goaway`).
//! - [`H2DrainState::take_deferred_initial_goaway`] replaces the
//!   `initial_goaway_pending` check-and-clear at the top of
//!   `ConnectionH2::flush_pending_control_frames`'s GOAWAY stage. The caller
//!   computes `ready_to_flush` (`expect_write().is_none() &&
//!   !header_block_reassembly_in_progress()`) — this module has neither
//!   `H2StreamTable` nor `H2State` to compute it itself.
//! - [`H2DrainState::enter_final_goaway`] replaces the two-field mutation at
//!   the top of `ConnectionH2::goaway`.
//! - [`H2DrainState::observe_peer_goaway`] replaces the two-field mutation in
//!   `ConnectionH2::handle_goaway_frame`.
//! - [`H2DrainState::deadline_elapsed`] replaces
//!   `ConnectionH2::graceful_shutdown_deadline_elapsed`'s body outright — it
//!   was already pure modulo the `now` it read from `self.now`, which is now
//!   an explicit parameter instead (the same shape
//!   [`super::h2_flood_detector::H2FloodDetector::check_flood`] uses: the
//!   module never samples a clock itself, LIFECYCLE.md invariant 20).
//!
//! `ConnectionH2::peer_gone_after_final_goaway` and `ConnectionH2::force_disconnect`
//! stay on `ConnectionH2` untouched: the former reads `self.drain.draining()`
//! (one line) but is otherwise entirely about `self.readiness.event`,
//! `self.stream_table` and `self.zero.storage` — socket/session state this
//! module does not have reason to hold. The latter does not reference
//! `self.drain` at all.
//!
//! ## The `zero.storage` dual-role hazard (LIFECYCLE.md invariant 24)
//!
//! `self.zero.storage` is simultaneously the read-side HEADERS+CONTINUATION
//! reassembly accumulator and the write-side scratch buffer every
//! control-frame flush (WINDOW_UPDATE, RST_STREAM, GOAWAY) clears and reuses.
//! Three bugs shipped from clobbering the former while reassembly was still
//! in progress — sozu-proxy/sozu#1396, #1397 and #1401; see LIFECYCLE.md §9
//! invariant 24 for the full account and provenance. This module cannot
//! reproduce that bug class itself (it has no `self.zero` to clobber), but it
//! exists BECAUSE of the fix for the third one: [`H2DrainState::initial_goaway_pending`]
//! is the flag #1401 added so a `graceful_goaway` landing mid-reassembly
//! defers its advisory GOAWAY instead of clobbering `self.zero.storage` out
//! from under an in-flight header block — and so the deferred send is not
//! silently lost once reassembly completes. Every decision method below that
//! touches `initial_goaway_pending` takes the reassembly/readiness state it
//! needs as a caller-computed `bool` parameter rather than reaching for
//! `self.zero`/`self.state` itself, precisely so this module stays unable to
//! reintroduce that clobber.
//!
//! ## Determinism / clock — nothing to fix here
//!
//! No field is a map, set or `Vec`, so there is no iteration-order hazard of
//! the kind [`super::h2_flow_control`] and [`super::h2_stream_table`]'s module
//! docs describe. No method samples a clock: [`H2DrainState::deadline_elapsed`]
//! and [`H2DrainState::begin_graceful_drain`] both take `now: Instant` from
//! the caller, exactly as `ConnectionH2::graceful_shutdown_deadline_elapsed`
//! and `ConnectionH2::graceful_goaway` always have.

use std::time::{Duration, Instant};

use super::StreamId;

/// RFC 9113 §6.8 double-GOAWAY drain bookkeeping: see the module doc.
pub(super) struct H2DrainState {
    /// True once we've sent (or deferred) a GOAWAY and are draining — either
    /// proxy-initiated ([`H2DrainState::begin_graceful_drain`]) or
    /// peer-initiated ([`H2DrainState::observe_peer_goaway`]). Monotonic:
    /// nothing in this module ever clears it back to `false`.
    draining: bool,
    /// Last stream ID from the peer's own GOAWAY (for retry decisions in
    /// `ConnectionH2::handle_goaway_frame`).
    peer_last_stream_id: Option<StreamId>,
    /// Wall-clock snapshot captured the first time this connection entered
    /// `draining` during a PROXY-initiated soft-stop. Used together with
    /// [`H2DrainState::graceful_shutdown_deadline`] by
    /// [`H2DrainState::deadline_elapsed`] to decide when to force-close.
    /// Remains `None` until [`H2DrainState::begin_graceful_drain`] runs —
    /// [`H2DrainState::observe_peer_goaway`] (a peer-initiated drain) never
    /// arms it; the forced-close budget is a proxy soft-stop concept only.
    started_at: Option<Instant>,
    /// Wall-clock budget granted to in-flight streams after the initial
    /// `GOAWAY(NO_ERROR)`. `None` means "wait indefinitely" (knob value `0`).
    /// Default when unset upstream: 5 s (see `L7ListenerHandler`).
    graceful_shutdown_deadline: Option<Duration>,
    /// True when [`H2DrainState::begin_graceful_drain`] decided to drain but
    /// had to defer serializing the advisory GOAWAY because reassembly was in
    /// progress — see the module doc's dual-role-hazard section.
    /// [`H2DrainState::take_deferred_initial_goaway`] clears it once the
    /// caller reports reassembly has completed. [`H2DrainState::enter_final_goaway`]
    /// also clears it: a final GOAWAY supersedes any still-deferred advisory
    /// one.
    initial_goaway_pending: bool,
}

/// Outcome of [`H2DrainState::begin_graceful_drain`] — tells
/// `ConnectionH2::graceful_goaway` which I/O to perform next. This module
/// stays I/O-free: it decides, the caller (which owns the socket,
/// `self.zero`, logging and `self.readiness`) acts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GracefulDrainDecision {
    /// The connection was already draining — the caller must send the FINAL
    /// GOAWAY (`ConnectionH2::goaway(H2Error::NoError)`) with the real
    /// `last_stream_id`.
    AlreadyDraining,
    /// First drain, but reassembly is in progress. `initial_goaway_pending`
    /// is now set; the caller must NOT touch `self.zero` and must
    /// `self.readiness.arm_writable()` so the deferred send is retried once
    /// `ConnectionH2::flush_pending_control_frames` observes reassembly has
    /// completed.
    DeferInitial,
    /// First drain, no reassembly in progress — the caller must serialize
    /// and send the initial advisory GOAWAY now
    /// (`ConnectionH2::send_initial_goaway`).
    SendInitial,
}

impl H2DrainState {
    /// Builds the undrained state for a new connection.
    /// `graceful_shutdown_deadline` is the listener's
    /// `h2_graceful_shutdown_deadline_seconds` knob, already resolved to
    /// `None` (wait indefinitely) or `Some(duration)` by the caller.
    pub(super) fn new(graceful_shutdown_deadline: Option<Duration>) -> Self {
        let state = H2DrainState {
            draining: false,
            peer_last_stream_id: None,
            started_at: None,
            graceful_shutdown_deadline,
            initial_goaway_pending: false,
        };
        state.debug_assert_invariants();
        state
    }

    // ---- accessors -----------------------------------------------------

    pub(super) fn draining(&self) -> bool {
        self.draining
    }

    /// Read by `ConnectionH2::handle_goaway_frame`'s retry loop, which
    /// compares each wire stream id against the value
    /// [`H2DrainState::observe_peer_goaway`] just recorded here rather than
    /// re-reading the frame it was built from.
    pub(super) fn peer_last_stream_id(&self) -> Option<StreamId> {
        self.peer_last_stream_id
    }

    // ---- decisions -------------------------------------------------------

    /// Returns `true` once the forced-close budget armed by
    /// [`Self::begin_graceful_drain`] has elapsed. `now` is the caller's
    /// clock snapshot (`ConnectionH2::now`, or `Mux::shutting_down`'s own
    /// sample) — this module never reads a clock itself.
    ///
    /// Returns `false` when: drain has not started yet (`started_at` is
    /// `None`), the knob is `0`/`None` (indefinite wait explicitly opted
    /// in), or the elapsed time is still within budget.
    pub(super) fn deadline_elapsed(&self, now: Instant) -> bool {
        match (self.started_at, self.graceful_shutdown_deadline) {
            (Some(started_at), Some(deadline)) => {
                now.saturating_duration_since(started_at) >= deadline
            }
            _ => false,
        }
    }

    /// RFC 9113 §6.8: record a peer-initiated GOAWAY. Marks the connection
    /// draining WITHOUT arming the forced-close budget: that budget is a
    /// proxy soft-stop concept only [`Self::begin_graceful_drain`] arms — see
    /// LIFECYCLE.md §8.3.
    pub(super) fn observe_peer_goaway(&mut self, last_stream_id: StreamId) {
        self.draining = true;
        self.peer_last_stream_id = Some(last_stream_id);
        debug_assert!(self.draining, "observing a peer GOAWAY must mark draining");
        debug_assert_eq!(
            self.peer_last_stream_id,
            Some(last_stream_id),
            "observing a peer GOAWAY must record its last_stream_id"
        );
        self.debug_assert_invariants();
    }

    /// Decide what a proxy-initiated `graceful_goaway` call must do next.
    /// `now` arms the forced-close budget on the very first call;
    /// `reassembly_in_progress` is the caller's
    /// `ConnectionH2::header_block_reassembly_in_progress()` — this module
    /// has no `H2State` to read it itself, and reading it here rather than
    /// letting the caller pass it in is exactly the shape that would let a
    /// future edit reintroduce the dual-role-hazard clobber the module doc
    /// describes.
    ///
    /// Idempotent on `draining`: once already draining, every field but the
    /// return value is left untouched — the forced-close budget must arm
    /// exactly once, from the FIRST call (LIFECYCLE.md §8.3), never
    /// re-armed or extended by a later one.
    pub(super) fn begin_graceful_drain(
        &mut self,
        now: Instant,
        reassembly_in_progress: bool,
    ) -> GracefulDrainDecision {
        if self.draining {
            return GracefulDrainDecision::AlreadyDraining;
        }
        // Pre-condition: this is the FIRST transition to draining, so the
        // forced-close budget must still be unarmed — arming it twice would
        // let a second drain silently re-extend it.
        debug_assert!(
            self.started_at.is_none(),
            "begin_graceful_drain must arm started_at exactly once, on the first call"
        );
        self.draining = true;
        self.started_at = Some(now);
        let decision = if reassembly_in_progress {
            self.initial_goaway_pending = true;
            GracefulDrainDecision::DeferInitial
        } else {
            GracefulDrainDecision::SendInitial
        };
        debug_assert!(self.draining, "begin_graceful_drain must mark draining");
        debug_assert_eq!(
            self.started_at,
            Some(now),
            "begin_graceful_drain must arm the forced-close budget from its `now` parameter"
        );
        debug_assert_eq!(
            self.initial_goaway_pending,
            matches!(decision, GracefulDrainDecision::DeferInitial),
            "initial_goaway_pending must be set iff the initial GOAWAY was deferred"
        );
        self.debug_assert_invariants();
        decision
    }

    /// Returns `true` (and clears the flag) exactly when a deferred initial
    /// GOAWAY should be sent now. `ready_to_flush` is the caller's own
    /// `expect_write().is_none() && !header_block_reassembly_in_progress()`
    /// check — this module has neither `self.zero` nor `H2State` to compute
    /// it. A caller that gets `false` back must not treat that as "nothing
    /// pending": the flag survives untouched for a later pass to retry,
    /// exactly as `H2FlowControl::drain_window_updates_into`'s partial-drain
    /// path leaves the remainder queued rather than dropping it.
    pub(super) fn take_deferred_initial_goaway(&mut self, ready_to_flush: bool) -> bool {
        if self.initial_goaway_pending && ready_to_flush {
            self.initial_goaway_pending = false;
            self.debug_assert_invariants();
            true
        } else {
            false
        }
    }

    /// `ConnectionH2::goaway` (the FINAL GOAWAY) supersedes any deferred
    /// advisory GOAWAY: it carries the real `last_stream_id` and its caller
    /// drops `expect_read`, so no further reassembly will ever complete to
    /// send the stale advisory. Also marks draining — idempotent, since the
    /// final GOAWAY can fire without a prior graceful drain (a flood
    /// violation, a SETTINGS-ACK timeout, a serialization failure).
    pub(super) fn enter_final_goaway(&mut self) {
        self.draining = true;
        self.initial_goaway_pending = false;
        debug_assert!(self.draining, "enter_final_goaway must mark draining");
        debug_assert!(
            !self.initial_goaway_pending,
            "enter_final_goaway must clear any deferred initial GOAWAY"
        );
        self.debug_assert_invariants();
    }

    // ---- test-only backdoors ---------------------------------------------
    //
    // Production code always reaches `draining`/`started_at` through
    // `begin_graceful_drain`/`observe_peer_goaway`, which is why those two
    // fields have no general-purpose accessor: only tests need to observe or
    // force them directly, mirroring the precedent
    // `H2StreamTable::__test_insert_wire_mapping_only` sets for this same
    // extraction family.

    /// Test-only accessor for the forced-close budget's arm time.
    #[cfg(any(test, feature = "e2e-hooks"))]
    pub(super) fn __test_started_at(&self) -> Option<Instant> {
        self.started_at
    }

    /// Test-only accessor for the deferred-initial-GOAWAY flag.
    #[cfg(any(test, feature = "e2e-hooks"))]
    pub(super) fn __test_initial_goaway_pending(&self) -> bool {
        self.initial_goaway_pending
    }

    /// Test-only backdoor: force `draining = true` without arming the
    /// forced-close budget or touching `initial_goaway_pending`. Used by
    /// fixtures that need a draining connection to exercise an unrelated
    /// code path (e.g. stream refusal while draining) and do not care about
    /// the forced-close budget at all.
    #[cfg(any(test, feature = "e2e-hooks"))]
    pub(super) fn __test_set_draining(&mut self) {
        self.draining = true;
    }

    /// Test-only backdoor: force `draining = true` AND arm the forced-close
    /// budget at `started_at`, bypassing `begin_graceful_drain`'s normal
    /// decision entirely. Exists for exactly one caller —
    /// `shutting_down_refreshes_the_snapshot_so_the_drain_budget_expires`
    /// (`mod.rs`) — which must simulate "already draining, budget armed at a
    /// specific instant" WITHOUT going through `graceful_goaway`: that test
    /// exists specifically to prove `Mux::shutting_down` refreshes its own
    /// clock snapshot on a path that never calls `graceful_goaway`, so
    /// arming the budget through `graceful_goaway`'s own `now` parameter
    /// would defeat the one property under test.
    #[cfg(any(test, feature = "e2e-hooks"))]
    pub(super) fn __test_arm_draining(&mut self, started_at: Instant) {
        self.draining = true;
        self.started_at = Some(started_at);
    }

    // ---- invariants ----------------------------------------------------------

    /// TigerStyle invariant sweep. A single full check of every structural
    /// invariant this type must preserve, asserted at the END of every
    /// public mutating method via
    /// [`debug_assert_invariants`](Self::debug_assert_invariants) — the
    /// pattern every sibling extraction in this family uses. Compiled out
    /// entirely in release (`#[cfg(debug_assertions)]`); on in every
    /// test/e2e/fuzz/dev build. Must never change runtime behavior — it only
    /// reads.
    #[cfg(debug_assertions)]
    fn check_invariants(&self) {
        // A deferred initial GOAWAY can only exist once the connection has
        // started draining: `begin_graceful_drain` is the only site that
        // ever sets `initial_goaway_pending`, and it always sets `draining`
        // in the same call, which is never cleared afterward.
        debug_assert!(
            !self.initial_goaway_pending || self.draining,
            "initial_goaway_pending must imply draining"
        );
        // The forced-close budget is armed only from `begin_graceful_drain`
        // (or the test backdoor mirroring it), which always sets `draining`
        // in the same call, and `draining` is monotonic — never cleared once
        // set (LIFECYCLE.md §8.3: only the proxy's own soft-stop arms the
        // budget, and it arms exactly once).
        debug_assert!(
            self.started_at.is_none() || self.draining,
            "an armed forced-close budget must imply draining"
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
        let drain = H2DrainState::new(Some(Duration::from_secs(5)));
        assert!(!drain.draining());
        assert_eq!(drain.peer_last_stream_id(), None);
        assert_eq!(drain.__test_started_at(), None);
        assert!(!drain.__test_initial_goaway_pending());
    }

    #[test]
    fn begin_graceful_drain_sends_initial_when_no_reassembly() {
        let mut drain = H2DrainState::new(None);
        let now = Instant::now();
        let decision = drain.begin_graceful_drain(now, false);
        assert_eq!(decision, GracefulDrainDecision::SendInitial);
        assert!(drain.draining());
        assert_eq!(drain.__test_started_at(), Some(now));
        assert!(!drain.__test_initial_goaway_pending());
    }

    #[test]
    fn begin_graceful_drain_defers_initial_during_reassembly() {
        let mut drain = H2DrainState::new(None);
        let now = Instant::now();
        let decision = drain.begin_graceful_drain(now, true);
        assert_eq!(decision, GracefulDrainDecision::DeferInitial);
        assert!(drain.draining());
        assert_eq!(drain.__test_started_at(), Some(now));
        assert!(
            drain.__test_initial_goaway_pending(),
            "reassembly in progress must defer the initial GOAWAY"
        );
    }

    /// The regression shape for #1401: a SECOND `begin_graceful_drain` call
    /// must not re-arm the forced-close budget, even when it lands well
    /// after the first.
    #[test]
    fn begin_graceful_drain_is_idempotent_and_does_not_rearm_the_budget() {
        let mut drain = H2DrainState::new(None);
        let first = Instant::now();
        assert_eq!(
            drain.begin_graceful_drain(first, false),
            GracefulDrainDecision::SendInitial
        );

        let later = first + Duration::from_secs(100);
        assert_eq!(
            drain.begin_graceful_drain(later, false),
            GracefulDrainDecision::AlreadyDraining
        );
        assert_eq!(
            drain.__test_started_at(),
            Some(first),
            "a second begin_graceful_drain must not move started_at"
        );
    }

    #[test]
    fn deadline_elapsed_false_when_unarmed() {
        let drain = H2DrainState::new(Some(Duration::from_secs(5)));
        assert!(!drain.deadline_elapsed(Instant::now()));
    }

    #[test]
    fn deadline_elapsed_respects_configured_budget() {
        let mut drain = H2DrainState::new(Some(Duration::from_secs(5)));
        let armed_at = Instant::now();
        drain.begin_graceful_drain(armed_at, false);

        assert!(
            !drain.deadline_elapsed(armed_at + Duration::from_secs(5) - Duration::from_millis(1))
        );
        assert!(drain.deadline_elapsed(armed_at + Duration::from_secs(5)));
    }

    #[test]
    fn deadline_elapsed_never_fires_when_deadline_is_none() {
        let mut drain = H2DrainState::new(None);
        let armed_at = Instant::now();
        drain.begin_graceful_drain(armed_at, false);
        assert!(!drain.deadline_elapsed(armed_at + Duration::from_secs(3600)));
    }

    #[test]
    fn observe_peer_goaway_marks_draining_without_arming_budget() {
        let mut drain = H2DrainState::new(Some(Duration::from_secs(5)));
        drain.observe_peer_goaway(41);
        assert!(drain.draining());
        assert_eq!(drain.peer_last_stream_id(), Some(41));
        assert_eq!(
            drain.__test_started_at(),
            None,
            "a peer-initiated GOAWAY must not arm the proxy's forced-close budget"
        );
    }

    #[test]
    fn take_deferred_initial_goaway_only_fires_when_ready() {
        let mut drain = H2DrainState::new(None);
        drain.begin_graceful_drain(Instant::now(), true);
        assert!(drain.__test_initial_goaway_pending());

        assert!(
            !drain.take_deferred_initial_goaway(false),
            "not ready to flush must leave the flag pending"
        );
        assert!(drain.__test_initial_goaway_pending());

        assert!(drain.take_deferred_initial_goaway(true));
        assert!(!drain.__test_initial_goaway_pending());
        // Idempotent: nothing left to take.
        assert!(!drain.take_deferred_initial_goaway(true));
    }

    #[test]
    fn enter_final_goaway_supersedes_a_deferred_initial_goaway() {
        let mut drain = H2DrainState::new(None);
        drain.begin_graceful_drain(Instant::now(), true);
        assert!(drain.__test_initial_goaway_pending());

        drain.enter_final_goaway();
        assert!(drain.draining());
        assert!(!drain.__test_initial_goaway_pending());
    }

    #[test]
    fn enter_final_goaway_marks_draining_even_without_a_prior_graceful_drain() {
        let mut drain = H2DrainState::new(None);
        drain.enter_final_goaway();
        assert!(drain.draining());
    }

    #[test]
    fn test_backdoors_match_the_shapes_their_callers_need() {
        let mut drain = H2DrainState::new(None);
        drain.__test_set_draining();
        assert!(drain.draining());
        assert_eq!(drain.__test_started_at(), None);

        let mut drain = H2DrainState::new(None);
        let at = Instant::now();
        drain.__test_arm_draining(at);
        assert!(drain.draining());
        assert_eq!(drain.__test_started_at(), Some(at));
    }
}
