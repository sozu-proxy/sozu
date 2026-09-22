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
//! **This module owns**: the mapping from `(has the peer gone, does rustls
//! still hold records, have we flushed yet)` to an action. Nothing else.
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
}
