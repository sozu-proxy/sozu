//! Connection-level HTTP/2 flow control (RFC 9113 §6.9) for
//! [`super::h2::ConnectionH2`].
//!
//! Groups the connection-level (stream 0) send window, the receive-side
//! byte accounting that decides when to grant more window back to the peer,
//! and the queue of not-yet-flushed outbound `WINDOW_UPDATE` frames behind a
//! narrow, closed API — the shape [`super::hpack_state::HpackState`]
//! established in the prior extraction step. Every field here is private to
//! this module; `ConnectionH2` reaches them only through the accessor
//! methods declared below.
//!
//! This module owns exactly what RFC 9113 §6.9 calls the connection-level
//! flow-control state:
//!
//! - [`H2FlowControl::window`] — our send credit toward the peer, replenished
//!   by [`H2FlowControl::apply_window_update`] (a `WINDOW_UPDATE(stream=0)`
//!   we receive) and consumed by [`H2FlowControl::consume_send_window`] (DATA
//!   bytes we write).
//! - [`H2FlowControl::received_bytes_since_update`] — bytes we've received
//!   and not yet credited back; [`H2FlowControl::account_received_bytes`]
//!   accumulates it and signals when it crosses the configured threshold.
//! - [`H2FlowControl::pending_window_updates`] — outbound `WINDOW_UPDATE`
//!   frames queued for both the connection (`stream_id == 0`) and individual
//!   streams (grant-back credit consumed by an inbound DATA frame), coalesced
//!   by [`H2FlowControl::queue_window_update`] and flushed by
//!   [`H2FlowControl::drain_window_updates_into`].
//!
//! Per-stream *send*-window state (`Stream.window` in `stream.rs`) and the
//! per-stream arm of `handle_window_update_frame` (which resolves a stream
//! slot via `ConnectionH2::streams` and can RST the stream) stay on
//! `ConnectionH2` / `Stream` — they need the stream table and endpoint this
//! module deliberately does not have.
//!
//! ## The advertised connection-level receive window is not enforced
//!
//! A peer derives its connection-level send allowance from RFC 9113 §6.9.2's
//! fixed 65535-octet initial value — no SETTINGS parameter can change the
//! connection-level window, only `WINDOW_UPDATE` on stream 0 can — plus every
//! stream-0 `WINDOW_UPDATE` Sōzu sends it: the one-shot enlargement to
//! `H2ConnectionConfig::initial_connection_window`, serialised straight into
//! `zero` beside the server SETTINGS by `ConnectionH2::readable`'s
//! `(H2State::ClientSettings, Position::Server)` arm on a frontend connection
//! (so the whole server preface leaves in one write) and queued by
//! `ConnectionH2::handle_settings_frame` on a backend one, and then the
//! periodic grants back. On this side that
//! configured number governs exactly two things: how large that one-shot
//! enlargement is, and how often credit is returned —
//! `ConnectionH2::handle_data_frame` passes `initial_connection_window / 2`
//! as the threshold to [`H2FlowControl::account_received_bytes`]. It is an
//! invitation to send. It is not a ceiling anything checks.
//!
//! Nothing here bounds inbound DATA. [`H2FlowControl::window`] is the SEND
//! window — peer-granted credit for our own writes — and
//! [`H2FlowControl::account_received_bytes`] only accumulates. No state in
//! this module is decremented by an inbound DATA frame, so none can go
//! negative, and no connection-level `FLOW_CONTROL_ERROR` is raised: the only
//! `H2Error::FlowControlError` this connection emits at all comes from
//! `ConnectionH2::handle_window_update_frame`, when an increment would grow a
//! SEND window past 2^31-1 — GOAWAY for the connection window, RST_STREAM for
//! a stream's. Every other flow-control-shaped rejection there is a
//! `ProtocolError` (a zero increment; a `SETTINGS_INITIAL_WINDOW_SIZE` above
//! 2^31-1, which `ConnectionH2::update_initial_window_size` reports to
//! `ConnectionH2::handle_settings_frame`). Measured on
//! sozu-proxy/sozu#1488: against 98303 octets advertised — the 65535 default
//! plus one 32768 grant — **106496 octets of DATA were accepted, with no
//! GOAWAY and no `FLOW_CONTROL_ERROR`**.
//!
//! The real boundary is memory, and it is the buffer pool. Every buffer a
//! stream needs comes from the caller-supplied `BufferSource`
//! (`buffer_source.rs`), which may refuse, and a refusal degrades one stream
//! with `RST_STREAM(REFUSED_STREAM)` and never the connection. That module's
//! doc is the contract and states the rule in full; read it there rather than
//! trusting a restatement here. The pool bounds memory, which is the thing
//! flow control exists to protect, and it does so without per-connection
//! credit bookkeeping.
//!
//! The consequence, unsoftened: a peer that trusts the advertisement has no
//! way to discover the real limit — nothing on the wire reports the pool's
//! remaining capacity — and RFC 9113 §6.9.1 makes enforcement a MUST, so a
//! conformance suite will flag this as a §6.9.1 violation. sozu-proxy/sozu#1488
//! offered two defensible resolutions and the decision was to document the
//! gap, not to close it. Closing it means tracking outstanding inbound credit
//! and emitting `FLOW_CONTROL_ERROR`, which changes observable behaviour and
//! belongs to its own changeset; until then, do not describe this window as
//! enforced anywhere, and do not assert the connection window is never
//! overcommitted in a test — see `doc/testing.md` for why the H2 simulator
//! deliberately carries no such property.
//!
//! ## Determinism (RFC 9113 doesn't mandate an order; issue #1338 does)
//!
//! `pending_window_updates` used to be a `HashMap`, and
//! [`H2FlowControl::drain_window_updates_into`] drained it in the map's
//! iteration order — which for `HashMap` is seeded per-process
//! (`RandomState`), so which stream's `WINDOW_UPDATE` reaches the wire first
//! was **non-deterministic across process restarts** even for the exact same
//! sequence of inbound frames. It is now a `BTreeMap`, so drain order is the
//! total order on `u32` stream ids — fixed, reproducible, and free (no sort
//! step, no injected seed to thread through construction and tests).
//!
//! RFC 9113 does not mandate a `WINDOW_UPDATE` emission order, but ascending
//! stream-id order is also the protocol-friendlier choice here, not just a
//! deterministic one: the connection-level entry is always keyed `0`, which
//! sorts before every stream id, so the connection-wide grant — the one
//! update that unblocks *every* stream at once — is always flushed first in
//! a pass. The residual risk the task calls out (a stream starving because
//! its id sorts late) is real in the abstract but not live here in practice:
//! `max_pending_window_updates` caps the map at `1 + 4 *
//! max_concurrent_streams` entries (see `ConnectionH2::new`), each frame is
//! 13 bytes, and the buffer-pool floor (`buffer_size >= 16393`, enforced
//! independently for H2) comfortably holds the whole capped map in one
//! `drain_window_updates_into` pass under the default configuration — so the
//! `Err(_) => break` partial-drain path this method still handles defensively
//! is not expected to trigger. If a future change ever decouples the pending
//! cap from the write-buffer size, revisit with a fairness cursor (the same
//! shape `Prioriser::advance_incremental_cursor` already uses for per-stream
//! writes) rather than assume ascending order stays starvation-free.
//!
//! ## Complexity — UNMEASURED beyond the default configuration
//!
//! [`H2FlowControl::queue_window_update`] is on the per-DATA-frame hot path
//! (called whenever an inbound DATA frame does not carry `END_STREAM`), and
//! its `BTreeMap` insert/coalesce is `O(log n)` against the prior `HashMap`'s
//! amortized `O(1)`, where `n` is the number of distinct stream ids with a
//! pending `WINDOW_UPDATE`. At the default `max_concurrent_streams` (100,
//! `n <= 401`) this is not expected to be measurable — a handful of
//! comparisons per call — but that expectation has **not been benchmarked**,
//! neither before nor after this change. `ConnectionH2::new` clamps
//! `max_concurrent_streams` to `MAX_SAFE_CONCURRENT_STREAMS = 10_000`, so an
//! operator raising that listener setting reaches `n` up to `1 + 4 * 10_000 =
//! 40_001` by the same formula — the point at which `O(log n)` (~15-16
//! comparisons) vs `O(1)` stops being self-evidently negligible and becomes
//! an empirical question. Closing this requires a before/after run of this
//! repository's `Bombardier bench` CI job (or an equivalent local
//! `bombardier` run) against a listener configured at or near
//! `MAX_SAFE_CONCURRENT_STREAMS`, comparing queue/drain-path latency or
//! throughput at the two commits; no such run has been produced for this
//! change.

use std::collections::BTreeMap;

use super::serializer;

/// Connection-level flow control state (RFC 9113 §6.9): our send window
/// toward the peer, receive-side byte accounting, and the queue of
/// not-yet-flushed outbound `WINDOW_UPDATE` frames (connection- and
/// stream-level alike — see the module doc).
pub(super) struct H2FlowControl {
    /// Connection-level send window (can go negative per RFC 9113 §6.9.2).
    window: i32,
    /// Bytes received since last connection-level WINDOW_UPDATE.
    received_bytes_since_update: u32,
    /// Queued stream_id -> accumulated increment for WINDOW_UPDATE frames
    /// (O(log n) coalescing). `BTreeMap` rather than `HashMap` so drain
    /// order is deterministic — see the module doc.
    pending_window_updates: BTreeMap<u32, u32>,
}

/// Outcome of [`H2FlowControl::queue_window_update`]. `ConnectionH2` maps
/// each variant to the exact log line / metric the old inline implementation
/// emitted — this module stays log- and metrics-free, like
/// [`super::hpack_state::HpackState`] and `protocol::udp::manager::UdpManager`.
pub(super) enum QueueWindowUpdateOutcome {
    /// Coalesced into an existing queued entry for this stream_id.
    Coalesced { old: u32, new: u32 },
    /// Queued as a fresh entry for this stream_id.
    Inserted { increment: u32 },
    /// The pending map was already at `max_pending` distinct stream ids —
    /// dropped rather than queued.
    Dropped,
    /// `increment` was zero: a legal no-op (RFC 9113 §6.9.1 only forbids
    /// *receiving* a zero-increment WINDOW_UPDATE; sozu simply never queues
    /// one to send). Nothing was queued.
    Noop,
}

/// Outcome of [`H2FlowControl::apply_window_update`].
pub(super) enum ApplyWindowUpdateOutcome {
    /// The window grew by the increment. `should_arm_writable` is true when
    /// the window transitioned from stalled (`<= 0`) to open (`> 0`) — the
    /// caller must re-arm WRITABLE so streams parked on the connection
    /// window get a chance to resume.
    Applied {
        new_window: i32,
        should_arm_writable: bool,
    },
    /// `checked_add` overflowed `i32` — RFC 9113 §6.9.1 makes this a
    /// FLOW_CONTROL_ERROR; the caller must GOAWAY.
    Overflow,
}

impl H2FlowControl {
    /// Builds the connection-level flow-control state for a new connection.
    /// `initial_window` is `DEFAULT_INITIAL_WINDOW_SIZE` (RFC 9113 §6.5.2
    /// default) at construction time — `ConnectionH2::new` never starts a
    /// connection pre-enlarged; the enlargement to
    /// `H2ConnectionConfig::initial_connection_window` happens later, via a
    /// stream-0 WINDOW_UPDATE: serialised with the server SETTINGS on a
    /// frontend connection, queued once the server's SETTINGS arrive on a
    /// backend one.
    pub(super) fn new(initial_window: i32) -> Self {
        let flow_control = H2FlowControl {
            window: initial_window,
            received_bytes_since_update: 0,
            pending_window_updates: BTreeMap::new(),
        };
        flow_control.debug_assert_invariants();
        flow_control
    }

    // ---- send-side window (peer-granted credit for OUR sends) --------------

    /// Current connection-level send window.
    pub(super) fn window(&self) -> i32 {
        self.window
    }

    /// RFC 9113 §6.9.2: consume `consumed` octets of connection-level send
    /// credit after writing that many DATA-frame payload bytes to the peer.
    /// The window may go negative (a SETTINGS-driven shrink can already have
    /// put it there); this only ever moves it further down.
    pub(super) fn consume_send_window(&mut self, consumed: i32) {
        debug_assert!(
            consumed >= 0,
            "send-window consumption must not be negative"
        );
        let before = self.window;
        self.window = self.window.saturating_sub(consumed);
        debug_assert!(
            self.window <= before,
            "consuming send credit must never grow the window"
        );
        self.debug_assert_invariants();
    }

    /// RFC 9113 §6.9: apply a peer-sent, already-validated-non-zero
    /// WINDOW_UPDATE increment to the connection-level send window. The
    /// caller (`ConnectionH2::handle_window_update_frame`) is responsible
    /// for the zero-increment case, which is a distinct protocol error
    /// (§6.9.1) handled before this is ever called.
    pub(super) fn apply_window_update(&mut self, increment: i32) -> ApplyWindowUpdateOutcome {
        debug_assert!(
            increment > 0,
            "WINDOW_UPDATE increment must be strictly positive at this point"
        );
        let window_before = self.window;
        match self.window.checked_add(increment) {
            Some(new_window) => {
                let should_arm_writable = self.window <= 0 && new_window > 0;
                self.window = new_window;
                // Flow-control replenish invariant (RFC 9113 §6.9): the
                // connection send window grows by exactly `increment` and
                // stays within i32 (checked_add already rejected overflow
                // above). The window may legally be negative going in (a
                // SETTINGS change can shrink it below zero) but a
                // WINDOW_UPDATE only ever increases it.
                debug_assert_eq!(
                    self.window,
                    window_before + increment,
                    "connection window must increase by exactly the increment"
                );
                debug_assert!(
                    self.window > window_before,
                    "a positive WINDOW_UPDATE must strictly grow the connection window"
                );
                self.debug_assert_invariants();
                ApplyWindowUpdateOutcome::Applied {
                    new_window,
                    should_arm_writable,
                }
            }
            None => {
                // RFC 9113 §6.9.1: a flow-control window MUST NOT be
                // increased to a value larger than 2^31-1. `checked_add`
                // above is what enforces that — it rejects exactly when the
                // true (unbounded) sum exceeds `i32::MAX`, which numerically
                // *is* 2^31-1. Widen to i64 (a range `i32` cannot itself
                // overflow at these magnitudes) to prove that check fired for
                // the right reason, rather than asserting `window <=
                // i32::MAX` on the i32 field itself — which Rust's type
                // already guarantees unconditionally and clippy's
                // `absurd_extreme_comparisons` correctly rejects as a
                // tautology.
                debug_assert!(
                    i64::from(window_before) + i64::from(increment) > i64::from(i32::MAX),
                    "checked_add rejected a WINDOW_UPDATE that would not have overflowed i32"
                );
                ApplyWindowUpdateOutcome::Overflow
            }
        }
    }

    // ---- receive-side accounting (bytes WE received, owed back to peer) ----

    /// RFC 9113 §6.9: accumulate `wire_payload_len` bytes of connection-level
    /// receive credit consumed by an inbound DATA frame (including padding —
    /// callers pass the full wire length, not just the application payload).
    /// Once the running total reaches `threshold`, resets the counter and
    /// returns the accumulated amount as the increment the caller must queue
    /// back to the peer via `queue_window_update(0, increment)`.
    pub(super) fn account_received_bytes(
        &mut self,
        wire_payload_len: u32,
        threshold: u32,
    ) -> Option<u32> {
        self.received_bytes_since_update = self
            .received_bytes_since_update
            .saturating_add(wire_payload_len);
        let increment = if self.received_bytes_since_update >= threshold {
            let increment = self.received_bytes_since_update;
            self.received_bytes_since_update = 0;
            Some(increment)
        } else {
            None
        };
        self.debug_assert_invariants();
        increment
    }

    // ---- outbound WINDOW_UPDATE queue ---------------------------------------

    pub(super) fn pending_window_updates_len(&self) -> usize {
        self.pending_window_updates.len()
    }

    pub(super) fn pending_window_updates_is_empty(&self) -> bool {
        self.pending_window_updates.is_empty()
    }

    /// Drops every queued (un-flushed) WINDOW_UPDATE. Used when the frontend
    /// has hung up while draining — see
    /// `ConnectionH2::flush_pending_control_frames`.
    pub(super) fn clear_pending_window_updates(&mut self) {
        self.pending_window_updates.clear();
        self.debug_assert_invariants();
    }

    /// Queue a WINDOW_UPDATE, coalescing with any existing entry for the same
    /// stream_id. RFC 9113 §6.9.1: window size increment MUST be
    /// 1..2^31-1 (0x7FFFFFFF) — `increment == 0` is a legal no-op to *send*
    /// (only *receiving* one is a protocol error) and is never queued, which
    /// is what keeps `pending_window_updates` free of dead zero entries (see
    /// `check_invariants`).
    ///
    /// `max_pending` is `ConnectionH2::max_pending_window_updates` — kept as
    /// a parameter rather than a field here because it derives from
    /// connection configuration (`max_concurrent_streams`) that this module
    /// has no reason to duplicate.
    pub(super) fn queue_window_update(
        &mut self,
        stream_id: u32,
        increment: u32,
        max_pending: usize,
    ) -> QueueWindowUpdateOutcome {
        if increment == 0 {
            return QueueWindowUpdateOutcome::Noop;
        }
        let max_increment = i32::MAX as u32;
        let len_before = self.pending_window_updates.len();
        let outcome = if let Some(existing) = self.pending_window_updates.get_mut(&stream_id) {
            let old = *existing;
            *existing = existing.saturating_add(increment).min(max_increment);
            // Coalescing invariant: the accumulated increment never decreases
            // and never exceeds i32::MAX (RFC 9113 §6.9 caps a WINDOW_UPDATE
            // increment at 2^31-1; emitting a larger value would be a
            // protocol error on the wire).
            debug_assert!(
                *existing >= old,
                "coalesced WINDOW_UPDATE increment must be monotonic non-decreasing"
            );
            debug_assert!(
                *existing <= max_increment,
                "coalesced WINDOW_UPDATE increment must stay within i32::MAX"
            );
            QueueWindowUpdateOutcome::Coalesced {
                old,
                new: *existing,
            }
        } else if self.pending_window_updates.len() < max_pending {
            let capped = increment.min(max_increment);
            self.pending_window_updates.insert(stream_id, capped);
            QueueWindowUpdateOutcome::Inserted { increment: capped }
        } else {
            QueueWindowUpdateOutcome::Dropped
        };
        debug_assert!(
            self.pending_window_updates.len() >= len_before,
            "queue_window_update must never shrink the pending map"
        );
        self.debug_assert_invariants();
        outcome
    }

    /// Serialize as many queued WINDOW_UPDATE frames as fit into `buf`, in
    /// ascending stream-id order (the `BTreeMap`'s natural iteration order —
    /// see the module doc for why that is both deterministic and
    /// protocol-friendly here). Mirrors the house `gen_*(buf, ...)` shape
    /// (`serializer::gen_window_update`): the caller passes a writable
    /// region of its own ring buffer (`kawa.storage.space()`) and this
    /// method never allocates an output buffer and never returns owned
    /// bytes.
    ///
    /// Drained entries are removed from the pending map; an entry that does
    /// not fit in `buf` (`GenError` from `serializer::gen_window_update`)
    /// stops the pass and stays queued for a later call — nothing is lost,
    /// only delayed. Returns `(bytes_written, frames_written)`; metrics are
    /// the caller's responsibility (this type stays metrics-free).
    pub(super) fn drain_window_updates_into(&mut self, buf: &mut [u8]) -> (usize, usize) {
        let len_before = self.pending_window_updates.len();
        let mut offset = 0usize;
        let mut frames_written = 0usize;
        let mut drained_ids: Vec<u32> = Vec::new();
        for (&stream_id, &increment) in self.pending_window_updates.iter() {
            if increment == 0 {
                // Defense-in-depth: queue_window_update() never inserts a
                // zero increment and coalescing is monotonic non-decreasing,
                // so this is unreachable in practice — but never emit a
                // WINDOW_UPDATE(0) on the wire, which RFC 9113 §6.9.1 makes
                // a protocol error for the peer to receive.
                debug_assert!(
                    false,
                    "pending_window_updates must never hold a zero-increment entry"
                );
                drained_ids.push(stream_id);
                continue;
            }
            match serializer::gen_window_update(&mut buf[offset..], stream_id, increment) {
                Ok((_, size)) => {
                    offset += size;
                    frames_written += 1;
                    drained_ids.push(stream_id);
                }
                Err(_) => {
                    // Buffer full — stop here, remaining entries stay queued.
                    break;
                }
            }
        }
        for id in &drained_ids {
            self.pending_window_updates.remove(id);
        }
        debug_assert!(
            self.pending_window_updates.len() <= len_before,
            "drain must only shrink the pending map"
        );
        debug_assert_eq!(
            drained_ids.len(),
            frames_written,
            "every drained id must have produced a written frame (zero entries are unreachable)"
        );
        self.debug_assert_invariants();
        (offset, frames_written)
    }

    // ---- invariants ----------------------------------------------------------

    /// TigerStyle invariant sweep. A single full check of every structural
    /// invariant this type must preserve, asserted at the END of every
    /// public mutating method via
    /// [`debug_assert_invariants`](Self::debug_assert_invariants) — exactly
    /// the pattern `protocol::udp::manager::UdpManager` uses. Compiled out
    /// entirely in release (`#[cfg(debug_assertions)]`); on in every
    /// test/e2e/fuzz/dev build. Must never change runtime behavior — it only
    /// reads.
    #[cfg(debug_assertions)]
    fn check_invariants(&self) {
        // RFC 9113 §6.9.1: a flow-control window MUST NOT exceed 2^31-1.
        // `window: i32` makes that bound structurally true for every value
        // the field can ever hold (`i32::MAX` *is* 2^31-1) — a runtime
        // `self.window <= i32::MAX` here would be comparing the type against
        // its own maximum, which clippy's `absurd_extreme_comparisons`
        // correctly flags as vacuous. The real proof obligation is that
        // nothing lets a sum silently wrap past that bound instead of being
        // rejected; [`Self::apply_window_update`]'s `Overflow` arm asserts
        // that directly, in i64 (a range i32 cannot itself overflow at these
        // magnitudes), at the one call site that grows the window.

        // A queued WINDOW_UPDATE increment is never zero — queue_window_update
        // treats increment == 0 as a no-op rather than inserting a dead entry.
        debug_assert!(
            self.pending_window_updates
                .values()
                .all(|&increment| increment != 0),
            "pending_window_updates must never hold a zero-increment entry"
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
        let fc = H2FlowControl::new(65_535);
        assert_eq!(fc.window(), 65_535);
        assert_eq!(fc.pending_window_updates_len(), 0);
        assert!(fc.pending_window_updates_is_empty());
    }

    #[test]
    fn window_update_coalescing() {
        let mut fc = H2FlowControl::new(65_535);

        match fc.queue_window_update(1, 1000, 10) {
            QueueWindowUpdateOutcome::Inserted { increment } => assert_eq!(increment, 1000),
            _ => panic!("expected Inserted"),
        }

        match fc.queue_window_update(1, 500, 10) {
            QueueWindowUpdateOutcome::Coalesced { old, new } => {
                assert_eq!(old, 1000);
                assert_eq!(new, 1500);
            }
            _ => panic!("expected Coalesced"),
        }

        fc.queue_window_update(3, 2000, 10);
        assert_eq!(fc.pending_window_updates_len(), 2);
    }

    #[test]
    fn window_update_saturation() {
        let mut fc = H2FlowControl::new(65_535);
        let max_increment = i32::MAX as u32;
        fc.queue_window_update(1, max_increment - 100, 10);
        match fc.queue_window_update(1, 200, 10) {
            QueueWindowUpdateOutcome::Coalesced { new, .. } => assert_eq!(new, max_increment),
            _ => panic!("expected Coalesced"),
        }
    }

    #[test]
    fn zero_increment_is_a_noop_and_never_queued() {
        let mut fc = H2FlowControl::new(65_535);
        match fc.queue_window_update(1, 0, 10) {
            QueueWindowUpdateOutcome::Noop => {}
            _ => panic!("expected Noop"),
        }
        assert!(fc.pending_window_updates_is_empty());
    }

    #[test]
    fn queue_window_update_drops_beyond_cap() {
        let mut fc = H2FlowControl::new(65_535);
        for i in 0..4u32 {
            fc.queue_window_update(i, 1000, 4);
        }
        assert_eq!(fc.pending_window_updates_len(), 4);
        match fc.queue_window_update(4, 1000, 4) {
            QueueWindowUpdateOutcome::Dropped => {}
            _ => panic!("expected Dropped"),
        }
        assert_eq!(fc.pending_window_updates_len(), 4);
    }

    #[test]
    fn connection_window_can_go_negative_via_consume() {
        // RFC 9113 §6.9.2: connection-level window can go negative.
        let mut fc = H2FlowControl::new(100);
        fc.consume_send_window(200);
        assert_eq!(fc.window(), -100);
    }

    #[test]
    fn consume_send_window_saturates_at_i32_min_without_wrapping() {
        // Boundary the adjacent `connection_window_can_go_negative_via_consume`
        // test does not reach: `consume_send_window` uses `saturating_sub`,
        // not `-`, specifically so a pathological (but legal — RFC 9113
        // §6.9.2 permits an arbitrarily negative window) sequence of
        // DATA-frame writes cannot wrap the sign (silent corruption) or
        // panic (a debug-build integer-underflow abort turning a protocol
        // edge case into a crash). Drive it past what plain subtraction
        // could represent and confirm it clamps at `i32::MIN` instead.
        let mut fc = H2FlowControl::new(i32::MIN + 10);
        fc.consume_send_window(i32::MAX);
        assert_eq!(fc.window(), i32::MIN);
    }

    #[test]
    fn apply_window_update_grows_window_and_signals_arm_on_unstall() {
        let mut fc = H2FlowControl::new(0);
        match fc.apply_window_update(50) {
            ApplyWindowUpdateOutcome::Applied {
                new_window,
                should_arm_writable,
            } => {
                assert_eq!(new_window, 50);
                assert!(should_arm_writable, "0 -> positive must arm writable");
            }
            ApplyWindowUpdateOutcome::Overflow => panic!("unexpected overflow"),
        }
    }

    #[test]
    fn apply_window_update_overflow_is_reported() {
        let mut fc = H2FlowControl::new(i32::MAX - 1);
        match fc.apply_window_update(10) {
            ApplyWindowUpdateOutcome::Overflow => {}
            ApplyWindowUpdateOutcome::Applied { .. } => panic!("expected Overflow"),
        }
    }

    #[test]
    fn account_received_bytes_accumulates_then_resets_at_threshold() {
        let mut fc = H2FlowControl::new(65_535);
        assert_eq!(fc.account_received_bytes(100, 250), None);
        assert_eq!(fc.account_received_bytes(100, 250), None);
        assert_eq!(fc.account_received_bytes(100, 250), Some(300));
        // Counter reset after crossing the threshold.
        assert_eq!(fc.account_received_bytes(1, 250), None);
    }

    /// The regression test for the ordering defect: drain must emit queued
    /// WINDOW_UPDATE frames in ascending stream_id order, deterministically,
    /// regardless of insertion order. Parses the serialized frame stream
    /// back out (9-byte header + 4-byte payload per RFC 9113 §6.9) to check
    /// the *wire* order, not just the map's internal order.
    #[test]
    fn drain_emits_ascending_stream_id_order_deterministically() {
        let mut fc = H2FlowControl::new(65_535);
        // Insert in a deliberately scrambled order, including the
        // connection-level (0) entry in the middle of the sequence.
        let insertion_order: [u32; 21] = [
            17, 3, 900, 1, 0, 501, 2, 19, 4, 777, 13, 6, 8, 333, 21, 5, 9, 111, 7, 45, 23,
        ];
        for &stream_id in &insertion_order {
            fc.queue_window_update(stream_id, 1000, insertion_order.len());
        }
        assert_eq!(fc.pending_window_updates_len(), insertion_order.len());

        let mut buf = [0u8; 21 * 13];
        let (bytes_written, frames_written) = fc.drain_window_updates_into(&mut buf);
        assert_eq!(frames_written, insertion_order.len());
        assert!(fc.pending_window_updates_is_empty());

        // Parse each 13-byte WINDOW_UPDATE frame's stream_id back out
        // (bytes 5..9 of the frame, big-endian, reserved bit masked) and
        // check the observed wire order is strictly ascending.
        let mut observed_order = Vec::new();
        let mut offset = 0;
        while offset < bytes_written {
            let stream_id = u32::from_be_bytes([
                buf[offset + 5],
                buf[offset + 6],
                buf[offset + 7],
                buf[offset + 8],
            ]) & 0x7FFF_FFFF;
            observed_order.push(stream_id);
            offset += 13;
        }

        let mut expected_order = insertion_order.to_vec();
        expected_order.sort_unstable();
        assert_eq!(
            observed_order, expected_order,
            "WINDOW_UPDATE drain order must be the deterministic ascending stream_id order"
        );
        // The connection-level entry (stream_id 0) must be first: it sorts
        // before every stream id and unblocks every stream at once.
        assert_eq!(observed_order[0], 0);
    }
}
