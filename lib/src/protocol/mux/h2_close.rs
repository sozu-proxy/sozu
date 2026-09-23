//! Close decisions taken under TLS backpressure, for [`super::h2::ConnectionH2`].
//!
//! Three sites in `h2.rs` decide whether a connection may close or must keep
//! draining: the `H2State::GoAway` arm of `writable`, the
//! `(H2State::Error, Position::Server)` arm beside it, and
//! `force_disconnect`'s server arm. All three ask the same question — does
//! rustls still hold encrypted records that have not reached the kernel? —
//! and all three get it wrong the same way if they get it wrong at all:
//! **the peer sees a truncated response.** That is data loss, not style,
//! which is why the decision is separated from the I/O that feeds it.
//!
//! A fourth site, `ConnectionH2::finalize_write`, ends every write pass with
//! the same three-step shape without deciding a close at all; see
//! [`finalize_action`] and the section below for why it is a sibling enum
//! rather than four more [`CloseAction`] variants.
//!
//! **This module owns**: the mapping from `(has the peer gone, does rustls
//! still hold records, have we flushed yet)` to a close action, and the
//! mapping from `(does rustls still hold records, has this pass written
//! already, what does the next tick still owe)` to a write-pass finalization
//! action. Nothing else.
//!
//! **It deliberately does NOT own, and why**:
//!
//! - **The flush.** A core cannot attempt I/O. [`CloseAction::Flush`] is an
//!   instruction to the caller, which performs `socket_write(&[])` and then
//!   asks again with [`TlsFlushPhase::AfterFlush`].
//! - **`Readiness`.** [`CloseAction::ReArmAndContinue`] says to re-arm;
//!   the caller owns the bits.
//! - **Reading `socket_wants_write()`.** That is a live-socket query and the
//!   caller makes it. This module receives the answer as a `bool` — a one-bit
//!   projection, the same shape `H2Scheduler::begin_pass` takes its readiness
//!   predicate in.
//!
//! # Why the two queries are a phase and not a loop
//!
//! The pre-image fused two different questions into one nested `if`:
//!
//! ```ignore
//! if self.socket.socket_wants_write() {      // (1) does rustls hold records?
//!     self.socket.socket_write(&[]);         //     try to push them
//!     if self.socket.socket_wants_write() {  // (2) did the kernel take them?
//! ```
//!
//! Query (2) is not a repeat of query (1). It is the only way the code learns
//! whether the flush succeeded — **`socket_write(&[])`'s returned `size` and
//! `SocketResult` are both discarded at that site**, so "did the kernel accept
//! the flush" is inferred purely from the handler's internal state changing
//! between the two calls. A caller that collapses the two into one query, or
//! that reuses the first answer for the second, silently reintroduces the
//! truncation. [`TlsFlushPhase`] exists to make the two questions two values
//! that cannot be confused for each other.
//!
//! # The flush the GoAway arm does not perform
//!
//! `ConnectionH2::writable` already attempts an unconditional flush in its
//! preamble, before the state match runs. So by the time the GoAway arm asks
//! its first question, one flush has been attempted this pass already, and
//! the arm's own flush is the SECOND attempt. Both are deliberate: the
//! preamble pushes bytes for every state, and the GoAway arm re-checks
//! because a close is about to be decided on the answer.
//!
//! # Tick count
//!
//! Returning [`CloseAction::Flush`] must not cost an extra event-loop tick.
//! The caller flushes and re-asks **within the same `writable()` call**; it
//! does not return `Continue` and wait to be called again. A version that
//! deferred would still be correct — the data would flush on the next
//! WRITABLE — but it would double the latency of every close under
//! backpressure, and under a peer that never re-arms it would strand the
//! connection. `a_flush_that_succeeds_closes_within_one_writable_call` pins
//! that a single call reaches the disconnect.

//! # The fourth site: finalizing a write pass is not a close
//!
//! `ConnectionH2::finalize_write` runs the same triple —
//! `socket_wants_write()`, a flush, `socket_wants_write()` again — and it is
//! the last of them left inline in `h2.rs`, so its decision belongs here too.
//! It gets [`FinalizeAction`], a SIBLING of [`CloseAction`], and the
//! separation is the point rather than a filing preference.
//!
//! Every [`CloseAction`] variant answers one question: may this connection
//! close, or must it keep draining. `finalize_write`'s non-flush branch
//! answers a different one — which `Readiness` bits does the next tick need —
//! which is LIFECYCLE §9 invariant 16's readiness policy and decides nothing
//! about closing. Widening [`CloseAction`] with `RetainPendingBack` or
//! `Quiesce` would force a named-impossible arm into every exhaustive `match`
//! `ConnectionH2::writable` already writes over it, for variants that can
//! never reach those arms.
//!
//! [`TlsFlushPhase`] is shared rather than duplicated, because the two-query
//! distinction is the identical one: the first query asks whether rustls holds
//! records, the second whether the flush between them landed.
//!
//! ## The conditional middle flush
//!
//! `finalize_write`'s flush carries a third input the GOAWAY arm has no
//! analogue for. Its middle step is CONDITIONAL:
//!
//! ```ignore
//! if self.socket.socket_wants_write() {
//!     if !socket_write {                 // <- the third input
//!         self.socket.socket_write(&[]);
//!     }
//!     self.ensure_tls_flushed();
//! }
//! ```
//!
//! A pass that already pushed bytes through `socket_write_vectored` has
//! attempted this pass's flush as a side effect of the write, so the
//! empty-buffer flush would be a second syscall for nothing. That `if` does
//! NOT move to the caller: it becomes the `socket_write` input and surfaces as
//! two distinct pre-flush answers, [`FinalizeAction::Flush`] and
//! [`FinalizeAction::SkipFlush`], which differ only in the step the caller
//! performs and both lead to the same post-flush query. A caller that kept the
//! `if` would still be taking the decision this module exists to own, and the
//! decision would be untestable without a socket.
//!
//! ## What this site does NOT consume
//!
//! `socket_write(&[])`'s `(size, status)` return. `finalize_write` discards it
//! — the post-flush `socket_wants_write()` query is how it learns whether the
//! flush landed — so no `SocketResult` reaches [`finalize_action`]. That
//! matters because `super::update_readiness` treats `size > 0` with a
//! `WouldBlock` status as NOT stalled (it clears the WRITABLE event bit and
//! returns `false`, so the flush issues another write), and any
//! decision function that took a `SocketResult` and treated
//! `status != Continue` as a terminator would silently drop that second
//! attempt. `ConnectionH2::flush_zero_buffer` is the site that does consume a
//! status; it is a different symbol and stays inline.

/// Which of the two `socket_wants_write()` questions the caller is answering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TlsFlushPhase {
    /// Before this site has attempted a flush of its own.
    BeforeFlush,
    /// After `socket_write(&[])`, re-querying whether records survived.
    AfterFlush,
}

/// What the caller should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CloseAction {
    /// Close the session now.
    CloseSession,
    /// Attempt `socket_write(&[])`, then ask again with
    /// [`TlsFlushPhase::AfterFlush`]. Only ever returned for
    /// [`TlsFlushPhase::BeforeFlush`].
    Flush,
    /// Records are still buffered: re-arm WRITABLE and keep the session.
    ReArmAndContinue,
    /// Nothing is buffered: proceed to the connection's disconnect path.
    Disconnect,
}

/// The `H2State::GoAway` arm of `ConnectionH2::writable`.
///
/// `peer_gone` is `peer_gone_after_final_goaway()`: the peer hung up after we
/// sent our final GOAWAY, so there is nobody left to deliver to and buffered
/// records are moot.
pub(super) fn goaway_close_action(
    phase: TlsFlushPhase,
    peer_gone: bool,
    tls_wants_write: bool,
) -> CloseAction {
    match phase {
        TlsFlushPhase::BeforeFlush => {
            if peer_gone {
                CloseAction::CloseSession
            } else if tls_wants_write {
                CloseAction::Flush
            } else {
                CloseAction::Disconnect
            }
        }
        // `peer_gone` is deliberately not re-read here. It was answered before
        // the flush, and the flush cannot resurrect a departed peer; re-reading
        // it would invite a caller to pass a stale value for one question and a
        // fresh one for the other.
        TlsFlushPhase::AfterFlush => {
            if tls_wants_write {
                CloseAction::ReArmAndContinue
            } else {
                CloseAction::Disconnect
            }
        }
    }
}

/// The `(H2State::Error, Position::Server)` arm of `ConnectionH2::writable`.
///
/// No `Flush` variant is reachable: `writable`'s preamble already attempted
/// one this pass, and this arm's answer is read after it. Unlike the GoAway
/// arm, an error connection has no graceful disconnect to fall through to —
/// it closes.
pub(super) fn error_close_action(tls_wants_write: bool) -> CloseAction {
    if tls_wants_write {
        CloseAction::ReArmAndContinue
    } else {
        CloseAction::CloseSession
    }
}

/// `ConnectionH2::force_disconnect`'s `Position::Server` arm.
///
/// Returning `CloseSession` here triggers `shutdown(Write)`, which sends FIN —
/// and any TLS records still in rustls's buffer are lost, which the client
/// reads as "TLS decode error / unexpected eof". So a connection with records
/// pending keeps WRITABLE interest instead and lets the writable path flush.
pub(super) fn force_disconnect_action(peer_gone: bool, tls_wants_write: bool) -> CloseAction {
    if peer_gone {
        CloseAction::CloseSession
    } else if tls_wants_write {
        CloseAction::ReArmAndContinue
    } else {
        CloseAction::CloseSession
    }
}

/// What the caller should do at the end of an H2 write pass.
///
/// A sibling of [`CloseAction`] rather than four more of its variants — see
/// this module's "fourth site" section. The first six answers are returned
/// only for [`TlsFlushPhase::BeforeFlush`], the last two only for
/// [`TlsFlushPhase::AfterFlush`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FinalizeAction {
    /// rustls still holds records and this pass has not written to the socket:
    /// attempt `socket_write(&[])`, then ask again with
    /// [`TlsFlushPhase::AfterFlush`].
    Flush,
    /// rustls still holds records, but this pass already wrote through
    /// `socket_write_vectored`, which attempted the flush as a side effect: go
    /// straight to the [`TlsFlushPhase::AfterFlush`] query without spending a
    /// second empty-buffer write on it.
    SkipFlush,
    /// A partial write parked the pass (`expect_write` is set). The parked
    /// write owns the next tick, so the readiness policy below is not
    /// consulted at all and no bit moves.
    Parked,
    /// LIFECYCLE §9 invariant 16: the pass made forward progress and left
    /// queued response bytes on at least one open stream, so `Ready::WRITABLE`
    /// must SURVIVE. The caller changes no bit — retaining is the absence of
    /// the withdrawal, not an action of its own — but the answer is named
    /// because it is a different reason for doing nothing than [`Self::Parked`].
    RetainPendingBack,
    /// A queued RST_STREAM or WINDOW_UPDATE is still unserialized: re-arm
    /// `Ready::WRITABLE` (interest AND event) so the next tick drains it.
    ArmControlQueue,
    /// Nothing is owed on any stream and nothing is queued: withdraw
    /// `Ready::WRITABLE` interest and wait for an external wake-up
    /// (`WINDOW_UPDATE`, backend readable, a new request).
    Quiesce,
    /// Records survived the flush: re-arm the edge-triggered WRITABLE event.
    ReArm,
    /// The kernel took everything this site had to give. Nothing left to do —
    /// and deliberately NOT a fall-through into the readiness policy: once
    /// rustls was found holding records, this pass has already decided not to
    /// touch the readiness bits, and a successful flush does not reopen that.
    /// The next tick reaches the policy through
    /// [`TlsFlushPhase::BeforeFlush`] with nothing pending.
    Settled,
}

/// `ConnectionH2::finalize_write`, the end of every H2 write pass.
///
/// The inputs after `phase` are consulted in exactly the order the pre-image's
/// nested `if`/`else if` consulted them, and the order is load-bearing rather
/// than cosmetic. TLS backpressure suppresses the entire readiness policy: a
/// pass whose records are still in rustls leaves every bit alone and lets the
/// flush decide. A parked `expect_write` then suppresses the rest of it.
///
/// - `tls_wants_write` — `socket_wants_write()`, the live-socket query the
///   caller makes. This module receives the one-bit projection.
/// - `socket_write` — did this pass already push bytes through
///   `socket_write_vectored`? See "the conditional middle flush".
/// - `expect_write_parked` — `stream_table.expect_write().is_some()`.
/// - `made_progress` — `bytes_written_this_pass > 0`.
/// - `any_pending_back` — the LIFECYCLE §9 invariant 16 probe.
/// - `control_pending` — a queued RST_STREAM or WINDOW_UPDATE is unserialized.
///
/// `any_pending_back` is a closure and not a `bool`, the one place this module
/// departs from its own one-bit-projection convention, and the departure buys
/// a measurable thing. The probe behind it (`any_stream_has_pending_back`)
/// walks every open stream of the connection, and the pre-image reached it
/// only under `!tls_wants_write && expect_write.is_none() && bytes_written > 0`.
/// A `bool` parameter would make the caller run that walk on every write pass
/// instead — including every TLS-backpressured one and every zero-progress one
/// — which is a cost regression wearing "no behaviour change" as a disguise.
/// `FnOnce` keeps the short-circuit inside the decision where it is testable;
/// `the_invariant_16_probe_is_not_walked_while_rustls_holds_records` and
/// `the_invariant_16_probe_is_not_walked_without_progress` pin that it stays
/// uncalled.
///
/// [`TlsFlushPhase::AfterFlush`] reads `tls_wants_write` and nothing else,
/// exactly as [`goaway_close_action`] stops reading `peer_gone` after its
/// flush. The caller passes degenerate values for the rest; this module
/// answers from the one input the flush could have changed.
pub(super) fn finalize_action<P>(
    phase: TlsFlushPhase,
    tls_wants_write: bool,
    socket_write: bool,
    expect_write_parked: bool,
    made_progress: bool,
    any_pending_back: P,
    control_pending: bool,
) -> FinalizeAction
where
    P: FnOnce() -> bool,
{
    match phase {
        TlsFlushPhase::BeforeFlush => {
            if tls_wants_write {
                if socket_write {
                    FinalizeAction::SkipFlush
                } else {
                    FinalizeAction::Flush
                }
            } else if expect_write_parked {
                FinalizeAction::Parked
            } else if made_progress && any_pending_back() {
                FinalizeAction::RetainPendingBack
            } else if control_pending {
                FinalizeAction::ArmControlQueue
            } else {
                FinalizeAction::Quiesce
            }
        }
        TlsFlushPhase::AfterFlush => {
            if tls_wants_write {
                FinalizeAction::ReArm
            } else {
                FinalizeAction::Settled
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The three functions are total over their inputs, so the tables below are
    // exhaustive rather than illustrative. Each row is one reachable state of
    // the close decision; there is no input these functions do not answer.

    #[test]
    fn goaway_before_flush_is_exhaustive() {
        use TlsFlushPhase::BeforeFlush;
        // A departed peer wins over buffered records: there is nobody to
        // deliver them to.
        assert_eq!(
            goaway_close_action(BeforeFlush, true, true),
            CloseAction::CloseSession
        );
        assert_eq!(
            goaway_close_action(BeforeFlush, true, false),
            CloseAction::CloseSession
        );
        // Live peer, records pending: try to push them before deciding.
        assert_eq!(
            goaway_close_action(BeforeFlush, false, true),
            CloseAction::Flush
        );
        // Live peer, nothing pending: nothing to lose by disconnecting.
        assert_eq!(
            goaway_close_action(BeforeFlush, false, false),
            CloseAction::Disconnect
        );
    }

    #[test]
    fn goaway_after_flush_is_exhaustive_and_ignores_peer_gone() {
        use TlsFlushPhase::AfterFlush;
        for peer_gone in [true, false] {
            assert_eq!(
                goaway_close_action(AfterFlush, peer_gone, true),
                CloseAction::ReArmAndContinue,
                "records survived the flush: keep the session whatever the \
                 pre-flush peer answer was"
            );
            assert_eq!(
                goaway_close_action(AfterFlush, peer_gone, false),
                CloseAction::Disconnect,
                "the kernel took everything: it is safe to disconnect"
            );
        }
    }

    /// The two phases answer DIFFERENTLY for the same `tls_wants_write`, which
    /// is the whole reason the phase is a parameter.
    ///
    /// TO SEE THIS RED: in [`goaway_close_action`], make the `AfterFlush` arm
    /// delegate to the `BeforeFlush` arm. The first assertion fails with
    /// `a live peer with records pending must FLUSH before the flush and
    /// RE-ARM after it` — a caller that treats the second answer like the
    /// first flushes forever and never re-arms. Measured: four tests in this
    /// module fail together, and in `h2.rs`
    /// `a_flush_that_does_not_drain_keeps_the_connection_open` panics at the
    /// `unreachable!` with `AfterFlush yielded Flush`.
    #[test]
    fn the_two_phases_are_not_interchangeable() {
        assert_ne!(
            goaway_close_action(TlsFlushPhase::BeforeFlush, false, true),
            goaway_close_action(TlsFlushPhase::AfterFlush, false, true),
            "a live peer with records pending must FLUSH before the flush and \
             RE-ARM after it"
        );
        assert_eq!(
            goaway_close_action(TlsFlushPhase::BeforeFlush, false, true),
            CloseAction::Flush
        );
        assert_eq!(
            goaway_close_action(TlsFlushPhase::AfterFlush, false, true),
            CloseAction::ReArmAndContinue
        );
    }

    /// `Flush` is only ever an instruction to the pre-flush caller. A site
    /// that received it post-flush would flush again, ask again, and loop.
    #[test]
    fn flush_is_never_returned_after_a_flush() {
        for peer_gone in [true, false] {
            for tls_wants_write in [true, false] {
                assert_ne!(
                    goaway_close_action(TlsFlushPhase::AfterFlush, peer_gone, tls_wants_write),
                    CloseAction::Flush,
                    "AfterFlush must never ask for another flush"
                );
            }
        }
    }

    #[test]
    fn error_arm_is_exhaustive() {
        assert_eq!(error_close_action(true), CloseAction::ReArmAndContinue);
        assert_eq!(error_close_action(false), CloseAction::CloseSession);
    }

    /// The error arm closes where the GoAway arm disconnects: an error
    /// connection has no graceful path left to fall through to.
    #[test]
    fn the_error_arm_closes_where_goaway_disconnects() {
        assert_eq!(error_close_action(false), CloseAction::CloseSession);
        assert_eq!(
            goaway_close_action(TlsFlushPhase::AfterFlush, false, false),
            CloseAction::Disconnect
        );
    }

    #[test]
    fn force_disconnect_arm_is_exhaustive() {
        assert_eq!(
            force_disconnect_action(true, true),
            CloseAction::CloseSession,
            "a departed peer closes even with records pending"
        );
        assert_eq!(
            force_disconnect_action(true, false),
            CloseAction::CloseSession
        );
        assert_eq!(
            force_disconnect_action(false, true),
            CloseAction::ReArmAndContinue,
            "records pending: keep WRITABLE rather than sending FIN and \
             truncating them"
        );
        assert_eq!(
            force_disconnect_action(false, false),
            CloseAction::CloseSession
        );
    }

    /// The truncation guard, stated as one assertion: no site may close while
    /// a live peer still has records buffered for it.
    ///
    /// TO SEE THIS RED: in [`force_disconnect_action`], drop the
    /// `tls_wants_write` arm so a live peer always yields `CloseSession`.
    /// Measured: two tests fail, this one with `closing on a live peer with
    /// records pending is the truncation bug: got CloseSession`, plus
    /// `force_disconnect_arm_is_exhaustive`.
    #[test]
    fn no_site_closes_on_a_live_peer_with_records_pending() {
        let live_peer_with_records = [
            goaway_close_action(TlsFlushPhase::AfterFlush, false, true),
            error_close_action(true),
            force_disconnect_action(false, true),
        ];
        for action in live_peer_with_records {
            assert!(
                matches!(action, CloseAction::ReArmAndContinue),
                "closing on a live peer with records pending is the truncation \
                 bug: got {action:?}"
            );
        }
        // The pre-flush GoAway answer is the one exception, and it is not a
        // close: it is an instruction to try harder first.
        assert_eq!(
            goaway_close_action(TlsFlushPhase::BeforeFlush, false, true),
            CloseAction::Flush
        );
    }

    // ── finalize_action: the end of a write pass ─────────────────────────
    //
    // `finalize_action` is total over its inputs too, so the sweeps below are
    // exhaustive rather than illustrative: every loop enumerates the FULL
    // cross product of the inputs it does not fix, and asserts a property that
    // must hold across all of it.

    /// One `BeforeFlush` / `AfterFlush` row, in declaration order:
    /// `[tls_wants_write, socket_write, expect_write_parked, made_progress,
    /// any_pending_back, control_pending]`.
    ///
    /// The probe is a plain `bool` here and the helper wraps it; the two tests
    /// that care whether the probe is CALLED bypass this helper and build
    /// their own closure, and the tests that pin the shape of one answer call
    /// `finalize_action` directly with named bindings — so an argument-order
    /// slip in this helper cannot make the whole table agree with itself.
    fn act(phase: TlsFlushPhase, inputs: [bool; 6]) -> FinalizeAction {
        let [
            tls_wants_write,
            socket_write,
            expect_write_parked,
            made_progress,
            any_pending_back,
            control_pending,
        ] = inputs;
        finalize_action(
            phase,
            tls_wants_write,
            socket_write,
            expect_write_parked,
            made_progress,
            || any_pending_back,
            control_pending,
        )
    }

    /// The conditional middle flush, as two answers.
    ///
    /// This is the input the GOAWAY arm has no analogue for: a pass that
    /// already wrote through `socket_write_vectored` has attempted this pass's
    /// flush already, and must not spend a second empty-buffer write on it.
    ///
    /// TO SEE THIS RED: in [`finalize_action`], make the `tls_wants_write` arm
    /// of `BeforeFlush` return `FinalizeAction::Flush` unconditionally
    /// (dropping the `socket_write` test — the shape a caller that kept the
    /// `if` for itself would leave behind). The second assertion fails with
    /// `a pass that already wrote must not spend a second empty-buffer flush`.
    #[test]
    fn the_pre_flush_answer_depends_on_whether_the_pass_already_wrote() {
        // Named bindings rather than the `act` helper: this test pins the
        // meaning of two specific positions, so it must not read them through
        // a helper that could have them swapped.
        let tls_wants_write = true;
        let expect_write_parked = false;
        let made_progress = false;
        let control_pending = false;

        assert_eq!(
            finalize_action(
                TlsFlushPhase::BeforeFlush,
                tls_wants_write,
                false, // socket_write: nothing went out this pass
                expect_write_parked,
                made_progress,
                || false,
                control_pending,
            ),
            FinalizeAction::Flush,
            "records pending and no write this pass: the empty-buffer flush is \
             this site's only attempt"
        );
        assert_eq!(
            finalize_action(
                TlsFlushPhase::BeforeFlush,
                tls_wants_write,
                true, // socket_write: the pass pushed bytes already
                expect_write_parked,
                made_progress,
                || false,
                control_pending,
            ),
            FinalizeAction::SkipFlush,
            "a pass that already wrote must not spend a second empty-buffer \
             flush"
        );
    }

    /// Records in rustls suppress the ENTIRE readiness policy, whatever the
    /// four policy inputs say. Sweeps all sixteen of them against both
    /// `socket_write` answers.
    #[test]
    fn tls_backpressure_dominates_every_readiness_input() {
        for socket_write in [false, true] {
            let expected = if socket_write {
                FinalizeAction::SkipFlush
            } else {
                FinalizeAction::Flush
            };
            for expect_write_parked in [false, true] {
                for made_progress in [false, true] {
                    for any_pending_back in [false, true] {
                        for control_pending in [false, true] {
                            let inputs = [
                                true,
                                socket_write,
                                expect_write_parked,
                                made_progress,
                                any_pending_back,
                                control_pending,
                            ];
                            assert_eq!(
                                act(TlsFlushPhase::BeforeFlush, inputs),
                                expected,
                                "pending TLS records must answer from the flush \
                                 alone, not from the readiness policy: {inputs:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// A parked `expect_write` suppresses the rest of the readiness policy.
    /// The parked write owns the next tick; nothing here may move a bit.
    #[test]
    fn a_parked_expect_write_suppresses_the_readiness_policy() {
        for made_progress in [false, true] {
            for any_pending_back in [false, true] {
                for control_pending in [false, true] {
                    let inputs = [
                        false,
                        false,
                        true,
                        made_progress,
                        any_pending_back,
                        control_pending,
                    ];
                    assert_eq!(
                        act(TlsFlushPhase::BeforeFlush, inputs),
                        FinalizeAction::Parked,
                        "a parked expect_write owns the next tick: {inputs:?}"
                    );
                }
            }
        }
    }

    /// LIFECYCLE §9 invariant 16, as an explicit table rather than a
    /// re-derivation: `Ready::WRITABLE` survives a clean pass only when the
    /// pass made forward progress AND a stream still has queued response
    /// bytes. Either half missing falls through to the control queue, and then
    /// to the withdrawal.
    ///
    /// The progress half is the load-bearing one: a zero-progress pass (every
    /// stream flow-control-starved) must relinquish `Ready::WRITABLE` so the
    /// session dispatcher does not busy-spin against it.
    ///
    /// TO SEE THIS RED: in [`finalize_action`], drop the `made_progress &&`
    /// conjunct so the invariant-16 arm reads `any_pending_back()`. The fifth
    /// row fails with `no progress: a starved pass must relinquish WRITABLE
    /// rather than busy-spin, got RetainPendingBack`.
    #[test]
    fn invariant_16_retains_writable_only_with_progress_and_pending_back() {
        // [made_progress, any_pending_back, control_pending] -> answer
        let rows: [([bool; 3], FinalizeAction, &str); 8] = [
            (
                [true, true, false],
                FinalizeAction::RetainPendingBack,
                "progress and queued bytes: WRITABLE must survive",
            ),
            (
                [true, true, true],
                FinalizeAction::RetainPendingBack,
                "invariant 16 outranks the control queue: both want WRITABLE \
                 kept and only one of them needs to re-arm the event",
            ),
            (
                [true, false, true],
                FinalizeAction::ArmControlQueue,
                "progress but nothing queued on a stream: the deferred control \
                 frame is what still needs the next tick",
            ),
            (
                [true, false, false],
                FinalizeAction::Quiesce,
                "progress, nothing left anywhere: withdraw WRITABLE",
            ),
            (
                [false, true, false],
                FinalizeAction::Quiesce,
                "no progress: a starved pass must relinquish WRITABLE rather \
                 than busy-spin",
            ),
            (
                [false, true, true],
                FinalizeAction::ArmControlQueue,
                "no progress, but a queued control frame still needs a tick",
            ),
            (
                [false, false, true],
                FinalizeAction::ArmControlQueue,
                "nothing on the streams, a frame in the control queue",
            ),
            (
                [false, false, false],
                FinalizeAction::Quiesce,
                "nothing owed anywhere: withdraw WRITABLE",
            ),
        ];
        for ([made_progress, any_pending_back, control_pending], expected, why) in rows {
            let inputs = [
                false,
                false,
                false,
                made_progress,
                any_pending_back,
                control_pending,
            ];
            assert_eq!(
                act(TlsFlushPhase::BeforeFlush, inputs),
                expected,
                "{why}, got {:?}",
                act(TlsFlushPhase::BeforeFlush, inputs)
            );
        }
    }

    /// The invariant-16 probe walks every open stream of the connection, so
    /// the decision must not reach it while rustls holds records — the
    /// pre-image never did, and a `bool` parameter would have made the caller
    /// pay for it on every backpressured pass.
    ///
    /// TO SEE THIS RED: in [`finalize_action`], hoist the probe to the top of
    /// the `BeforeFlush` arm (`let any_pending_back = any_pending_back();`)
    /// and test the local below. This test fails with `the invariant 16 probe
    /// must not be walked while rustls still holds records`.
    #[test]
    fn the_invariant_16_probe_is_not_walked_while_rustls_holds_records() {
        let walked = std::cell::Cell::new(false);
        let action = finalize_action(
            TlsFlushPhase::BeforeFlush,
            true,  // tls_wants_write
            false, // socket_write
            false, // expect_write_parked
            true,  // made_progress: the probe's other conjunct is satisfied
            || {
                walked.set(true);
                true
            },
            false,
        );
        assert_eq!(
            action,
            FinalizeAction::Flush,
            "premise: this is the TLS arm"
        );
        assert!(
            !walked.get(),
            "the invariant 16 probe must not be walked while rustls still \
             holds records"
        );
    }

    /// The same contract for the other short-circuit: a zero-progress pass
    /// cannot retain `Ready::WRITABLE` whatever the probe would answer, so the
    /// walk is wasted work.
    ///
    /// TO SEE THIS RED: in [`finalize_action`], swap the invariant-16 arm's
    /// conjuncts to `any_pending_back() && made_progress`. The answer is
    /// unchanged — `&&` is commutative in value — but the probe now runs, and
    /// this test fails with `a zero-progress pass cannot retain WRITABLE, so
    /// the probe must not be walked`. That is the mutation a `bool` parameter
    /// would bake in permanently.
    #[test]
    fn the_invariant_16_probe_is_not_walked_without_progress() {
        let walked = std::cell::Cell::new(false);
        let action = finalize_action(
            TlsFlushPhase::BeforeFlush,
            false, // tls_wants_write
            false, // socket_write
            false, // expect_write_parked
            false, // made_progress: zero-progress pass
            || {
                walked.set(true);
                true
            },
            false,
        );
        assert_eq!(
            action,
            FinalizeAction::Quiesce,
            "premise: a zero-progress pass with nothing queued quiesces"
        );
        assert!(
            !walked.get(),
            "a zero-progress pass cannot retain WRITABLE, so the probe must \
             not be walked"
        );
    }

    /// `AfterFlush` answers from the records and nothing else — the readiness
    /// policy is not reopened by a flush that landed. Sweeps the full cross
    /// product of the five inputs it must ignore.
    #[test]
    fn after_flush_answers_from_the_records_and_nothing_else() {
        for tls_wants_write in [false, true] {
            let expected = if tls_wants_write {
                FinalizeAction::ReArm
            } else {
                FinalizeAction::Settled
            };
            for socket_write in [false, true] {
                for expect_write_parked in [false, true] {
                    for made_progress in [false, true] {
                        for any_pending_back in [false, true] {
                            for control_pending in [false, true] {
                                let inputs = [
                                    tls_wants_write,
                                    socket_write,
                                    expect_write_parked,
                                    made_progress,
                                    any_pending_back,
                                    control_pending,
                                ];
                                assert_eq!(
                                    act(TlsFlushPhase::AfterFlush, inputs),
                                    expected,
                                    "the post-flush query reads the records \
                                     only: {inputs:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// A site handed `Flush` or `SkipFlush` after a flush would flush again,
    /// ask again, and loop — the same property [`CloseAction::Flush`] has.
    ///
    /// TO SEE THIS RED: in [`finalize_action`], make the `AfterFlush` arm
    /// delegate to the `BeforeFlush` arm. This test fails on its own assertion
    /// with `AfterFlush must never ask for another flush: [true, false, false,
    /// true, true, true] gave Flush`. Measured: four tests fail together — this
    /// one, `after_flush_answers_from_the_records_and_nothing_else`,
    /// `the_two_finalize_phases_are_not_interchangeable`, and in `h2.rs`
    /// `a_finalized_write_pass_flushes_once_and_re_arms_while_records_survive`,
    /// which panics at the production `unreachable!` with `AfterFlush yielded
    /// Flush`. The recipe names THIS test's own message rather than that
    /// panic, which is someone else's assertion.
    #[test]
    fn the_flush_answers_are_never_returned_after_a_flush() {
        for tls_wants_write in [false, true] {
            for socket_write in [false, true] {
                for expect_write_parked in [false, true] {
                    let inputs = [
                        tls_wants_write,
                        socket_write,
                        expect_write_parked,
                        true,
                        true,
                        true,
                    ];
                    let action = act(TlsFlushPhase::AfterFlush, inputs);
                    assert!(
                        !matches!(action, FinalizeAction::Flush | FinalizeAction::SkipFlush),
                        "AfterFlush must never ask for another flush: \
                         {inputs:?} gave {action:?}"
                    );
                }
            }
        }
    }

    /// The mirror: the two post-flush answers are meaningless before a flush,
    /// so `BeforeFlush` must never return one. The production caller matches
    /// them as a named-impossible arm; this is what makes that arm true.
    #[test]
    fn the_post_flush_answers_are_never_returned_before_a_flush() {
        for tls_wants_write in [false, true] {
            for socket_write in [false, true] {
                for expect_write_parked in [false, true] {
                    for made_progress in [false, true] {
                        for any_pending_back in [false, true] {
                            for control_pending in [false, true] {
                                let inputs = [
                                    tls_wants_write,
                                    socket_write,
                                    expect_write_parked,
                                    made_progress,
                                    any_pending_back,
                                    control_pending,
                                ];
                                let action = act(TlsFlushPhase::BeforeFlush, inputs);
                                assert!(
                                    !matches!(
                                        action,
                                        FinalizeAction::ReArm | FinalizeAction::Settled
                                    ),
                                    "BeforeFlush must never return a post-flush \
                                     answer: {inputs:?} gave {action:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// The two phases answer DIFFERENTLY for the same records answer, which is
    /// why the phase is a parameter here as it is for the close decision.
    #[test]
    fn the_two_finalize_phases_are_not_interchangeable() {
        let inputs = [true, false, false, false, false, false];
        assert_ne!(
            act(TlsFlushPhase::BeforeFlush, inputs),
            act(TlsFlushPhase::AfterFlush, inputs),
            "pending records must FLUSH before the flush and RE-ARM after it"
        );
        assert_eq!(
            act(TlsFlushPhase::BeforeFlush, inputs),
            FinalizeAction::Flush
        );
        assert_eq!(
            act(TlsFlushPhase::AfterFlush, inputs),
            FinalizeAction::ReArm
        );
    }
}
